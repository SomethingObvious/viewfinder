//! Live view to limited range BT.709 NV12 or YUY2. If NVIDIA's VideoSuperRes ever fails on
//! the GPU, the CPU path takes over for good.

use std::path::PathBuf;
use std::time::Instant;

use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use shared::{Format, NV12, YUY2};
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::adjust::Adjust;

const FIXED: i32 = 1 << 16;
const HALF: i32 = FIXED >> 1;

#[derive(Clone, Copy, Debug, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent switches, each saved on its own line"
)]
pub struct Look {
    /// Where the 16:9 crop sits in the 3:2 frame, 0 is the top edge.
    pub framing: f32,
    pub sharpen: f32,
    /// VideoSuperRes `QualityLevel`, 0 turns the GPU path off.
    pub vsr_quality: u32,
    pub vsr_strength: f32,
    /// 8 to 11, or 0 for none.
    pub denoise: u32,
    pub fix_pixels: bool,
    pub fit: bool,
    pub smooth: bool,
    pub mirror: bool,
    pub upside_down: bool,
    /// Percent, 100 to 200.
    pub zoom: u32,
    /// Where a zoomed crop sits across the frame, 0 is the left edge.
    pub pan: f32,
    pub auto_frame: bool,
}

impl Default for Look {
    fn default() -> Self {
        // Mode 3 takes out blocking without smearing text, where the streaming modes grey out
        // fine detail. Denoise 10 takes 12 to 17 dB of grain off flat areas and still gains
        // detail, for about 4 ms. Ultra costs the same and loses detail.
        Self {
            framing: 0.5,
            sharpen: 0.55,
            vsr_quality: 3,
            vsr_strength: 1.0,
            denoise: 10,
            fix_pixels: true,
            fit: true,
            smooth: true,
            mirror: false,
            upside_down: false,
            zoom: 100,
            pan: 0.5,
            auto_frame: false,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Source {
    #[default]
    Ycc,
    Rgb {
        offset: usize,
        stride: usize,
    },
}

/// Building a model takes about a second, so the last two sizes are kept.
#[derive(Default)]
struct Gpu {
    dir: Option<PathBuf>,
    models: Vec<Model>,
    settings: Option<vsr::Settings>,
    failed: Option<String>,
}

struct Model {
    input: (u32, u32),
    output: (u32, u32),
    settings: vsr::Settings,
    sr: vsr::SuperRes,
}

#[derive(Default)]
pub(crate) struct Pipeline {
    decoded: Vec<u8>,
    /// Frames decode into this and swap with `decoded` only once they've worked, so a broken
    /// one can't leave `decoded` resized under the last frame's geometry.
    incoming: Vec<u8>,
    /// The decoded frame before `finish`, while Compare needs it.
    untouched: Vec<u8>,
    luma: Vec<u8>,
    chroma: Vec<u8>,
    blur: Vec<u8>,
    scratch_luma: Vec<u8>,
    scratch_chroma: Vec<u8>,
    width: u32,
    height: u32,
    resizer: Resizer,
    source: Source,
    /// Whether `luma` and `chroma` hold this frame. An RGB decode only fills them to scale down.
    planes_fresh: bool,
    gpu: Gpu,
    repair: crate::pixels::Repair,
    staging: Vec<u8>,
    padded: Vec<u8>,
    held: std::collections::VecDeque<(u64, Vec<u8>)>,
    spare: Vec<Vec<u8>>,
    decoded_size: (usize, usize),
    rgb: bool,
    turn: (bool, bool),
    adjusting: crate::adjust::Pass,
    builds: u32,
    /// Milliseconds per stage since the caller last took them, indexed by `DECODE` and the rest.
    pub(crate) times: [f32; 4],
}

impl Pipeline {
    pub(crate) fn with_gpu(vfx: Option<PathBuf>) -> Self {
        let mut p = Self::default();
        p.gpu.dir = vfx;
        p
    }

    pub(crate) const fn stuck_pixels(&self) -> usize {
        self.repair.count()
    }

    pub(crate) fn avoid_eyes(&mut self, face: Option<crate::face::Face>) {
        self.repair.avoid_eyes(face);
    }

    pub(crate) fn gpu_problem(&self) -> Option<&str> {
        self.gpu.failed.as_deref()
    }

    #[cfg(test)]
    pub(crate) const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub(crate) fn decode(&mut self, jpeg: &[u8], look: Look) -> Result<(), String> {
        let gpu = self.gpu_path(look);
        let space = if gpu {
            ColorSpace::RGB
        } else {
            ColorSpace::YCbCr
        };
        let options = DecoderOptions::default().jpeg_set_out_colorspace(space);
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(jpeg), options);
        decoder
            .decode_headers()
            .map_err(|e| format!("bad live view JPEG: {e:?}"))?;
        let (w, h) = decoder.dimensions().ok_or("live view JPEG has no size")?;
        check_size(w, h)?;
        let len = decoder
            .output_buffer_size()
            .ok_or("live view JPEG has no size")?;
        if len != w * h * 3 {
            return Err(format!("live view JPEG is {w}x{h} but not 3-channel"));
        }
        let started = Instant::now();
        self.incoming.resize(len, 0);
        decoder
            .decode_into(&mut self.incoming)
            .map_err(|e| format!("bad live view JPEG: {e:?}"))?;
        std::mem::swap(&mut self.decoded, &mut self.incoming);
        self.times[DECODE] += ms_since(started);
        self.place(w, h, !gpu, look);
        Ok(())
    }

    pub(crate) fn decode_frame(
        &mut self,
        frame: &[u8],
        format: capture::Format,
        look: Look,
    ) -> Result<(), String> {
        let capture::Format::Raw {
            pixels,
            width,
            height,
            bt709,
        } = format
        else {
            return self.decode(frame, look);
        };
        check_size(width as usize, height as usize)?;
        let started = Instant::now();
        capture::convert::to_rgb(pixels, width, height, bt709, frame, &mut self.incoming)
            .map_err(|e| e.to_string())?;
        std::mem::swap(&mut self.decoded, &mut self.incoming);
        self.times[DECODE] += ms_since(started);
        self.place(width as usize, height as usize, false, look);
        Ok(())
    }

    fn place(&mut self, w: usize, h: usize, ycc: bool, look: Look) {
        let gpu = self.gpu_path(look);
        self.decoded_size = (w, h);
        self.rgb = !ycc;
        if look.fix_pixels {
            let started = Instant::now();
            self.repair.apply(&mut self.decoded, w, h, ycc);
            self.times[REPAIR] += ms_since(started);
        }

        let (cw, ch) = crop_size((w, h), look);
        // Pan only shows when zoomed, so at 1x a wide picture that loses its sides stays centred.
        let pan = if look.zoom > 100 { look.pan } else { 0.5 };
        let left = ((w - cw) as f32 * pan.clamp(0.0, 1.0)).floor() as usize;
        let top = ((h - ch) as f32 * look.framing.clamp(0.0, 1.0)).round() as usize;
        self.turn = (look.mirror, look.upside_down);
        self.width = cw as u32;
        self.height = ch as u32;
        self.planes_fresh = false;
        if gpu {
            self.source = Source::Rgb {
                offset: (top * w + left) * 3,
                stride: w * 3,
            };
            let strength = look.vsr_strength.clamp(0.0, 1.0);
            let denoise = if (8..=11).contains(&look.denoise) {
                look.denoise
            } else {
                0
            };
            self.gpu.settings = Some(vsr::Settings {
                quality: look.vsr_quality,
                strength,
                denoise,
            });
            return;
        }
        self.source = Source::Ycc;
        let started = Instant::now();
        let (matrix, centre) = if ycc {
            (bt601_full_to_bt709_limited(), 128)
        } else {
            (rgb_to_bt709_limited(), 0)
        };
        self.fill_planes((top * w + left) * 3, w * 3, matrix, centre);
        if look.sharpen > 0.0 {
            sharpen(&mut self.luma, &mut self.blur, cw, ch, look.sharpen);
        }
        self.times[OUTPUT] += ms_since(started);
    }

    /// The last decoded frame uncropped, its width, and whether it's RGB rather than YCbCr.
    pub(crate) fn frame(&self) -> (&[u8], usize, bool) {
        (&self.decoded, self.decoded_size.0, self.rgb)
    }

    const fn gpu_path(&self, look: Look) -> bool {
        look.vsr_quality > 0 && self.gpu.dir.is_some() && self.gpu.failed.is_none()
    }

    pub(crate) const fn can_smooth(&self, look: Look) -> bool {
        look.smooth && self.gpu_path(look)
    }

    /// Takes `cast` out and applies Adjust in one pass. `keep` holds the original for Compare.
    // RGB only, so the CPU path without an NVIDIA card keeps the camera's colour.
    pub(crate) fn finish(&mut self, cast: Option<crate::white::Cast>, adjust: &Adjust, keep: bool) {
        self.untouched.clear();
        if !self.rgb || (cast.is_none() && adjust.is_neutral()) {
            return;
        }
        let started = Instant::now();
        if keep {
            self.untouched.extend_from_slice(&self.decoded);
        }
        let lut = balance_lut(cast);
        if adjust.is_neutral() {
            let at = |table: &[u8; 256], c: u8| table.get(usize::from(c)).copied().unwrap_or(c);
            let [r, g, b] = &lut;
            for px in self.decoded.as_chunks_mut::<3>().0 {
                *px = [at(r, px[0]), at(g, px[1]), at(b, px[2])];
            }
        } else {
            let window = self.window();
            self.adjusting
                .apply(&mut self.decoded, self.decoded_size.0, window, &lut, adjust);
        }
        self.times[REPAIR] += ms_since(started);
    }

    pub(crate) fn auto(
        &self,
        cast: Option<crate::white::Cast>,
        face: Option<crate::face::Face>,
    ) -> Option<Adjust> {
        let lut = balance_lut(cast);
        self.rgb.then(|| {
            crate::adjust::auto(
                &self.decoded,
                self.decoded_size.0,
                self.window(),
                &lut,
                face,
            )
        })
    }

    /// The left, top, width and height of the part of `decoded` that apps see.
    const fn window(&self) -> [usize; 4] {
        match self.source {
            Source::Rgb { offset, stride } if stride > 0 => [
                offset.rem_euclid(stride).div_euclid(3),
                offset.div_euclid(stride),
                self.width as usize,
                self.height as usize,
            ],
            _ => [0, 0, self.decoded_size.0, self.decoded_size.1],
        }
    }

    pub(crate) fn hold(&mut self, seq: u64) {
        if self
            .held
            .back()
            .is_some_and(|(_, f)| f.len() != self.decoded.len())
        {
            self.forget();
        }
        let mut frame = self.spare.pop().unwrap_or_default();
        frame.clone_from(&self.decoded);
        self.held.push_back((seq, frame));
        while self.held.len() > 8
            && let Some((_, f)) = self.held.pop_front()
        {
            self.spare.push(f);
        }
    }

    pub(crate) fn forget(&mut self) {
        self.spare.extend(self.held.drain(..).map(|(_, f)| f));
    }

    /// Makes the current frame the held one nearest `at` frames along, so 4.25 shows frame 4.
    /// Frames before `at` are dropped.
    pub(crate) fn show_at(&mut self, at: f64) {
        let base = at.floor() as u64;
        while self.held.len() > 1
            && self.held.front().is_some_and(|(seq, _)| *seq < base)
            && let Some((_, f)) = self.held.pop_front()
        {
            self.spare.push(f);
        }
        let Some(&(first, _)) = self.held.front() else {
            return;
        };
        let next = self.held.iter().position(|(seq, _)| *seq == base + 1);
        let pick = match next {
            Some(i) if base == first && at - base as f64 >= 0.5 => i,
            _ if base < first => 0,
            _ => self
                .held
                .iter()
                .rposition(|(seq, _)| *seq <= base)
                .unwrap_or(0),
        };
        if let Some((_, frame)) = self.held.get(pick) {
            self.decoded.clone_from(frame);
        }
        self.planes_fresh = false;
    }

    /// Builds and runs once the GPU passes `out` will use, since a fresh pass takes another 0.1
    /// to 0.2 s its first time. All of it takes about a second.
    pub(crate) fn prepare(&mut self, out: Format, look: Look) {
        if self.width == 0
            || self.height == 0
            || !self.gpu_path(look)
            || self.gpu.settings.is_none()
        {
            return;
        }
        // A failure here is recorded by render, as it would be on a real frame.
        let mut scratch = vec![0; out.bytes()];
        self.render(out, &mut scratch).ok();
    }

    pub(crate) const fn builds(&self) -> u32 {
        self.builds
    }

    pub(crate) const fn on_gpu(&self) -> bool {
        !self.gpu.models.is_empty()
    }

    pub(crate) fn release(&mut self) {
        self.gpu.models.clear();
        self.forget();
        self.spare = Vec::new();
        for buf in [
            &mut self.staging,
            &mut self.padded,
            &mut self.scratch_luma,
            &mut self.scratch_chroma,
            &mut self.blur,
        ] {
            *buf = Vec::new();
        }
    }

    fn fill_planes(&mut self, offset: usize, stride: usize, m: [[i32; 3]; 3], centre: i32) {
        let (cw, ch) = (self.width as usize, self.height as usize);
        self.luma.resize(cw * ch, 0);
        self.chroma.resize(cw * ch * 2, 0);
        self.planes_fresh = true;
        if cw == 0 || stride == 0 {
            return;
        }
        let rows = self
            .decoded
            .get(offset..)
            .unwrap_or_default()
            .chunks(stride)
            .zip(self.luma.chunks_exact_mut(cw))
            .zip(self.chroma.chunks_exact_mut(cw * 2))
            .take(ch);
        for ((src, luma), chroma) in rows {
            let pixels = src.as_chunks::<3>().0.iter().take(cw);
            for ((px, l), c) in pixels.zip(luma).zip(chroma.as_chunks_mut::<2>().0) {
                let (y, cb, cr) = (
                    i32::from(px[0]),
                    i32::from(px[1]) - centre,
                    i32::from(px[2]) - centre,
                );
                // checked_div truncates negative chroma toward zero like `/` does.
                let mix = |r: [i32; 3], offset: i32, lo: i32, hi: i32| {
                    ((r[0] * y + r[1] * cb + r[2] * cr + HALF)
                        .checked_div(FIXED)
                        .unwrap_or_default()
                        + offset)
                        .clamp(lo, hi) as u8
                };
                *l = mix(m[0], 16, 16, 235);
                *c = [mix(m[1], 128, 16, 240), mix(m[2], 128, 16, 240)];
            }
        }
    }

    pub(crate) fn render(&mut self, out: Format, dst: &mut [u8]) -> Result<(), String> {
        if self.width == 0 || self.height == 0 {
            return Err("nothing decoded yet".into());
        }
        let inner = self.fit(out);
        let result = if inner == out {
            self.render_exact(out, dst)
        } else {
            let mut picture = std::mem::take(&mut self.padded);
            picture.resize(inner.bytes(), 0);
            let result = self.render_exact(inner, &mut picture).and_then(|()| {
                pillarbox(inner, &picture, out, frame_of(dst, out)?);
                Ok(())
            });
            self.padded = picture;
            result
        };
        let (mirror, upside_down) = self.turn;
        if result.is_ok() && (mirror || upside_down) {
            turn(out, frame_of(dst, out)?, mirror, upside_down);
        }
        result
    }

    fn fit(&self, out: Format) -> Format {
        let width = (u64::from(out.height) * u64::from(self.width))
            .div_euclid(u64::from(self.height)) as u32;
        Format {
            width: (width & !1).min(out.width),
            ..out
        }
    }

    fn render_exact(&mut self, out: Format, dst: &mut [u8]) -> Result<(), String> {
        // VideoSuperRes only scales up, and scaling down hides noise anyway.
        if let Source::Rgb { offset, stride } = self.source {
            // After a GPU failure the rest of the frame, the preview included, goes to the CPU
            // instead of building another model for a second.
            if out.height >= self.height && self.gpu.failed.is_none() {
                let result = self.render_gpu(out, dst, offset, stride);
                if let Err(e) = &result {
                    self.gpu.failed = Some(e.clone());
                    self.gpu.models.clear();
                }
                return result;
            }
            if !self.planes_fresh {
                self.fill_planes(offset, stride, rgb_to_bt709_limited(), 0);
            }
        }
        let (w, h) = (out.width, out.height);
        let luma_bytes = (w * h) as usize;
        match out.fourcc {
            NV12 => {
                let (y, uv) = frame_of(dst, out)?.split_at_mut(luma_bytes);
                self.scale(Plane::Luma, w, h, y)?;
                self.scale(Plane::Chroma, w.div_euclid(2), h.div_euclid(2), uv)?;
            }
            YUY2 => {
                let mut y = std::mem::take(&mut self.scratch_luma);
                let mut uv = std::mem::take(&mut self.scratch_chroma);
                y.resize(luma_bytes, 0);
                uv.resize(luma_bytes, 0);
                let scaled = self
                    .scale(Plane::Luma, w, h, &mut y)
                    .and_then(|()| self.scale(Plane::Chroma, w.div_euclid(2), h, &mut uv))
                    .and_then(|()| frame_of(dst, out));
                let result = scaled.map(|dst| {
                    for ((d, y), uv) in dst
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(y.as_chunks::<2>().0)
                        .zip(uv.as_chunks::<2>().0)
                    {
                        *d = [y[0], uv[0], y[1], uv[1]];
                    }
                });
                self.scratch_luma = y;
                self.scratch_chroma = uv;
                result?;
            }
            other => return Err(format!("can't render fourcc {other:#x}")),
        }
        Ok(())
    }

    fn model(&mut self, output: (u32, u32)) -> Result<usize, String> {
        let dir = self.gpu.dir.as_deref().ok_or("no NVIDIA libraries")?;
        let settings = self.gpu.settings.ok_or("no super resolution settings")?;
        let input = (self.width, self.height);
        // Turning denoise on or off rewires the GPU passes, so it needs its own model.
        let denoised = settings.denoise > 0;
        let i = if let Some(i) = self.gpu.models.iter().position(|m| {
            m.input == input && m.output == output && (m.settings.denoise > 0) == denoised
        }) {
            i
        } else {
            let sr = vsr::SuperRes::new(dir, input, output, settings)?;
            if self.gpu.models.len() == 2 {
                self.gpu.models.remove(0);
            }
            self.gpu.models.push(Model {
                input,
                output,
                settings,
                sr,
            });
            self.builds += 1;
            self.gpu.models.len() - 1
        };
        let m = self.gpu.models.get_mut(i).ok_or(NO_MODEL)?;
        if m.settings != settings {
            m.sr.set(settings)?;
            m.settings = settings;
        }
        Ok(i)
    }

    fn render_gpu(
        &mut self,
        out: Format,
        dst: &mut [u8],
        offset: usize,
        stride: usize,
    ) -> Result<(), String> {
        let i = self.model((out.width, out.height))?;
        let model = &mut self.gpu.models.get_mut(i).ok_or(NO_MODEL)?.sr;
        let window = self
            .decoded
            .get(offset..)
            .ok_or("the crop is outside the frame")?;
        // The call blocks until the GPU is done, so timing around it needs no extra sync.
        let mut gpu = 0.0;
        let mut run = |nv12: &mut [u8]| {
            let started = Instant::now();
            let result = model.run(window, stride, nv12);
            gpu += ms_since(started);
            result
        };
        let result = match out.fourcc {
            NV12 => frame_of(dst, out).and_then(&mut run),
            YUY2 => {
                let nv12 = Format {
                    fourcc: NV12,
                    ..out
                };
                self.staging.resize(nv12.bytes(), 0);
                run(&mut self.staging).and_then(|()| {
                    nv12_to_yuy2(
                        &self.staging,
                        out.width as usize,
                        out.height as usize,
                        frame_of(dst, out)?,
                    );
                    Ok(())
                })
            }
            other => Err(format!("can't render fourcc {other:#x}")),
        };
        self.times[GPU] += gpu;
        result
    }

    pub(crate) fn preview(&self, rgba: &mut Vec<u8>) -> (usize, usize) {
        let (w, h) = (self.width as usize, self.height as usize);
        let (pw, ph) = (w.div_euclid(2), h.div_euclid(2));
        rgba.clear();
        rgba.reserve(pw * ph * 4);
        if pw == 0 || ph == 0 {
            return (pw, ph);
        }
        if let Source::Rgb { offset, stride } = self.source {
            let original = if self.untouched.is_empty() {
                &self.decoded
            } else {
                &self.untouched
            };
            let window = original.get(offset..).unwrap_or_default();
            for row in window.chunks(stride * 2).take(ph) {
                for px in row.as_chunks::<6>().0.iter().take(pw) {
                    rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
            return (pw, ph);
        }
        let rows = self.luma.chunks(w * 2).zip(self.chroma.chunks(w * 4));
        for (luma, chroma) in rows.take(ph) {
            let pixels = luma
                .as_chunks::<2>()
                .0
                .iter()
                .zip(chroma.as_chunks::<4>().0);
            for (y, c) in pixels.take(pw) {
                rgba.extend_from_slice(&bt709_limited_to_rgba(y[0], c[0], c[1]));
            }
        }
        (pw, ph)
    }

    fn scale(&mut self, plane: Plane, w: u32, h: u32, dst: &mut [u8]) -> Result<(), String> {
        let (src, kind, filter) = match plane {
            Plane::Luma => (&mut self.luma, PixelType::U8, FilterType::Lanczos3),
            Plane::Chroma => (&mut self.chroma, PixelType::U8x2, FilterType::CatmullRom),
        };
        let src =
            Image::from_slice_u8(self.width, self.height, src, kind).map_err(|e| e.to_string())?;
        let mut out = Image::from_slice_u8(w, h, dst, kind).map_err(|e| e.to_string())?;
        let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(filter));
        self.resizer
            .resize(&src, &mut out, &options)
            .map_err(|e| e.to_string())
    }
}

pub(crate) const DECODE: usize = 0;
pub(crate) const REPAIR: usize = 1;
pub(crate) const GPU: usize = 2;
pub(crate) const OUTPUT: usize = 3;

fn balance_lut(cast: Option<crate::white::Cast>) -> [[u8; 256]; 3] {
    cast.map_or_else(
        || [std::array::from_fn(|c| c as u8); 3],
        crate::white::balance_lut,
    )
}

fn ms_since(started: Instant) -> f32 {
    started.elapsed().as_secs_f32() * 1000.0
}

#[derive(Clone, Copy)]
enum Plane {
    Luma,
    Chroma,
}

const NO_MODEL: &str = "the super resolution model went missing";

fn frame_of(dst: &mut [u8], f: Format) -> Result<&mut [u8], String> {
    let len = dst.len();
    dst.get_mut(..f.bytes())
        .ok_or_else(|| format!("{len} bytes is too small for a {f:?} frame"))
}

fn pillarbox(inner: Format, picture: &[u8], out: Format, dst: &mut [u8]) {
    let (iw, ow, h) = (
        inner.width as usize,
        out.width as usize,
        out.height as usize,
    );
    // Even, so NV12 chroma pairs and YUY2 macropixels stay aligned.
    let left = (ow - iw).div_euclid(2) & !1;
    if out.fourcc == YUY2 {
        for (src, row) in picture
            .chunks_exact(iw * 2)
            .zip(dst.chunks_exact_mut(ow * 2))
            .take(h)
        {
            let (l, rest) = row.split_at_mut(left * 2);
            let (mid, r) = rest.split_at_mut(iw * 2);
            for side in [l, r] {
                for px in side.as_chunks_mut::<2>().0 {
                    *px = [16, 128];
                }
            }
            mid.copy_from_slice(src);
        }
        return;
    }
    let (src_y, src_uv) = picture.split_at(iw * h);
    let (dst_y, dst_uv) = dst.split_at_mut(ow * h);
    for (plane_src, plane_dst, rows, fill) in [
        (src_y, dst_y, h, 16u8),
        (src_uv, dst_uv, h.div_euclid(2), 128u8),
    ] {
        for (src, row) in plane_src
            .chunks_exact(iw)
            .zip(plane_dst.chunks_exact_mut(ow))
            .take(rows)
        {
            let (l, rest) = row.split_at_mut(left);
            let (mid, r) = rest.split_at_mut(iw);
            l.fill(fill);
            mid.copy_from_slice(src);
            r.fill(fill);
        }
    }
}

/// RGBA of exactly what apps get. Halving frames wider than 1280 still covers a 4K screen. A
/// row at a time takes a 1080p frame in 0.8 ms, against 6.3 ms a byte at a time.
pub(crate) fn preview_output(f: Format, frame: &[u8], rgba: &mut Vec<u8>) -> (usize, usize) {
    let (w, h) = (f.width as usize, f.height as usize);
    let step = if w > 1280 { 2 } else { 1 };
    let (pw, ph) = (w.div_euclid(step), h.div_euclid(step));
    rgba.clear();
    rgba.resize(pw * ph * 4, 0);
    if pw == 0 {
        return (pw, ph);
    }
    let rows = rgba.chunks_exact_mut(pw * 4).enumerate();
    if f.fourcc == YUY2 {
        for (py, out) in rows {
            let row = py * step;
            let src = frame
                .get(row * w * 2..(row + 1) * w * 2)
                .unwrap_or_default();
            let pairs = src.as_chunks::<4>().0;
            let out = out.as_chunks_mut::<4>().0;
            if step == 2 {
                for (o, p) in out.iter_mut().zip(pairs) {
                    *o = bt709_limited_to_rgba(p[0], p[1], p[3]);
                }
            } else {
                for (o, p) in out.as_chunks_mut::<2>().0.iter_mut().zip(pairs) {
                    *o = [
                        bt709_limited_to_rgba(p[0], p[1], p[3]),
                        bt709_limited_to_rgba(p[2], p[1], p[3]),
                    ];
                }
            }
        }
        return (pw, ph);
    }
    let (luma, chroma) = frame.split_at(frame.len().min(w * h));
    for (py, out) in rows {
        let row = py * step;
        let half = row.div_euclid(2);
        let y = luma.get(row * w..(row + 1) * w).unwrap_or_default();
        let uv = chroma.get(half * w..(half + 1) * w).unwrap_or_default();
        let (y, uv) = (y.as_chunks::<2>().0, uv.as_chunks::<2>().0);
        let out = out.as_chunks_mut::<4>().0;
        if step == 2 {
            for ((o, y), c) in out.iter_mut().zip(y).zip(uv) {
                *o = bt709_limited_to_rgba(y[0], c[0], c[1]);
            }
        } else {
            for ((o, y), c) in out.as_chunks_mut::<2>().0.iter_mut().zip(y).zip(uv) {
                *o = [
                    bt709_limited_to_rgba(y[0], c[0], c[1]),
                    bt709_limited_to_rgba(y[1], c[0], c[1]),
                ];
            }
        }
    }
    (pw, ph)
}

fn bt709_limited_to_rgba(y: u8, cb: u8, cr: u8) -> [u8; 4] {
    let y = (i32::from(y) - 16) * 298;
    let (cb, cr) = (i32::from(cb) - 128, i32::from(cr) - 128);
    let c = |v: i32| ((v + 128) >> 8).clamp(0, 255) as u8;
    [
        c(y + 459 * cr),
        c(y - 55 * cb - 136 * cr),
        c(y + 541 * cb),
        255,
    ]
}

fn nv12_to_yuy2(nv12: &[u8], w: usize, h: usize, dst: &mut [u8]) {
    let (luma, chroma) = nv12.split_at(w * h);
    let chroma_rows = chroma.chunks_exact(w).flat_map(|uv| [uv, uv]);
    let rows = dst
        .chunks_exact_mut(w * 2)
        .zip(luma.chunks_exact(w))
        .zip(chroma_rows)
        .take(h);
    for ((out, y), uv) in rows {
        let pairs = y.as_chunks::<2>().0.iter().zip(uv.as_chunks::<2>().0);
        for (d, (y, uv)) in out.as_chunks_mut::<4>().0.iter_mut().zip(pairs) {
            *d = [y[0], uv[0], y[1], uv[1]];
        }
    }
}

/// Live view is 1056 by 704 and webcams stop at 4K. A forged huge header costs hundreds of MB.
const SIDES: std::ops::RangeInclusive<usize> = 16..=4096;

fn check_size(w: usize, h: usize) -> Result<(), String> {
    if SIDES.contains(&w) && SIDES.contains(&h) {
        Ok(())
    } else {
        Err(format!(
            "the camera sent a {w}x{h} frame, which can't be right"
        ))
    }
}

/// Every format apps can pick is 16:9, so a picture wider than that loses its sides even in
/// Whole Picture, since the alternative is squashing it.
pub(crate) fn crop_size((w, h): (usize, usize), look: Look) -> (usize, usize) {
    let whole = look.fit && w * 9 <= h * 16;
    zoomed(if whole { (w, h) } else { crop_16_9(w, h) }, look.zoom)
}

fn zoomed((w, h): (usize, usize), zoom: u32) -> (usize, usize) {
    if zoom <= 100 {
        return (w, h);
    }
    let zoom = zoom.min(200) as usize;
    (
        (w * 100).div_euclid(zoom) & !1,
        (h * 100).div_euclid(zoom) & !1,
    )
}

fn turn(f: Format, frame: &mut [u8], mirror: bool, upside_down: bool) {
    let (w, h) = (f.width as usize, f.height as usize);
    if f.fourcc == YUY2 {
        if mirror {
            for row in frame.chunks_exact_mut(w * 2) {
                let pairs = row.as_chunks_mut::<4>().0;
                pairs.reverse();
                for p in pairs {
                    p.swap(0, 2);
                }
            }
        }
        if upside_down {
            swap_rows(frame, w * 2, h);
        }
        return;
    }
    let (luma, chroma) = frame.split_at_mut((w * h).min(frame.len()));
    if mirror {
        for row in luma.chunks_exact_mut(w) {
            row.reverse();
        }
        for row in chroma.chunks_exact_mut(w) {
            row.as_chunks_mut::<2>().0.reverse();
        }
    }
    if upside_down {
        swap_rows(luma, w, h);
        swap_rows(chroma, w, h.div_euclid(2));
    }
}

fn swap_rows(plane: &mut [u8], len: usize, rows: usize) {
    let (top, bottom) = plane.split_at_mut((len * rows.div_euclid(2)).min(plane.len()));
    let bottom = bottom.get_mut(..len * rows.div_ceil(2)).unwrap_or_default();
    for (a, b) in top.chunks_exact_mut(len).zip(bottom.rchunks_exact_mut(len)) {
        a.swap_with_slice(b);
    }
}

fn crop_16_9(w: usize, h: usize) -> (usize, usize) {
    let even = |v: usize| v & !1;
    if w * 9 <= h * 16 {
        (even(w), even((w * 9).div_euclid(16)))
    } else {
        (even((h * 16).div_euclid(9)), even(h))
    }
}

/// Rows map JFIF `[Y, Cb-128, Cr-128]` to limited-range BT.709 `[Y-16, Cb-128, Cr-128]`.
fn bt601_full_to_bt709_limited() -> [[i32; 3]; 3] {
    to_bt709_limited([
        [1.0, 0.0, 1.402],
        [1.0, -0.344_136, -0.714_136],
        [1.0, 1.772, 0.0],
    ])
}

fn rgb_to_bt709_limited() -> [[i32; 3]; 3] {
    to_bt709_limited([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]])
}

fn to_bt709_limited(to_rgb: [[f64; 3]; 3]) -> [[i32; 3]; 3] {
    let (kr, kb) = (0.2126, 0.0722);
    let kg = 1.0 - kr - kb;
    let ys = 219.0 / 255.0;
    let cs = 224.0 / 255.0;
    let to_709 = [
        [kr * ys, kg * ys, kb * ys],
        [
            -kr / (2.0 * (1.0 - kb)) * cs,
            -kg / (2.0 * (1.0 - kb)) * cs,
            0.5 * cs,
        ],
        [
            0.5 * cs,
            -kg / (2.0 * (1.0 - kr)) * cs,
            -kb / (2.0 * (1.0 - kr)) * cs,
        ],
    ];
    let [[rr, rg, rb], [gr, gg, gb], [br, bg, bb]] = to_rgb;
    let columns = [[rr, gr, br], [rg, gg, bg], [rb, gb, bb]];
    let mut m = [[0; 3]; 3];
    for (row, from) in m.iter_mut().zip(&to_709) {
        for (v, column) in row.iter_mut().zip(&columns) {
            let f: f64 = from.iter().zip(column).map(|(a, b)| a * b).sum();
            *v = (f * f64::from(FIXED)).round() as i32;
        }
    }
    m
}

/// Detail this small is left alone, so sharpening doesn't lift grain that calls spend bits on.
/// With ISO 3200 grain the default costs 8% more bits, where 0.35 without it costs 31%.
const CORE: i32 = 8;

fn sharpen(luma: &mut [u8], blur: &mut Vec<u8>, w: usize, h: usize, amount: f32) {
    if w < 3 || h < 3 {
        return;
    }
    blur.clear();
    blur.extend_from_slice(luma);
    let k = (amount * 256.0) as i32;
    let rows = luma
        .chunks_exact_mut(w)
        .skip(1)
        .zip(blur.windows(3 * w).step_by(w))
        .take(h - 2);
    for (out, around) in rows {
        let (up, rest) = around.split_at(w);
        let (mid, down) = rest.split_at(w);
        let windows = up.windows(3).zip(mid.windows(3)).zip(down.windows(3));
        for (o, ((u, m), d)) in out.iter_mut().skip(1).zip(windows) {
            let (&[u0, u1, u2], &[m0, m1, m2], &[d0, d1, d2]) = (u, m, d) else {
                continue;
            };
            let sum: i32 = [u0, u1, u2, m0, m1, m2, d0, d1, d2]
                .into_iter()
                .map(i32::from)
                .sum();
            let centre = i32::from(m1);
            let detail = centre - sum.div_euclid(9);
            let detail = detail - detail.clamp(-CORE, CORE);
            let boost = (detail * k).checked_div(256).unwrap_or_default();
            *o = (centre + boost).clamp(16, 235) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn jpeg_of(w: u16, h: u16, rgb: [u8; 3]) -> Vec<u8> {
        let pixels: Vec<u8> = (0..w as usize * h as usize).flat_map(|_| rgb).collect();
        let mut out = Vec::new();
        let encoder = jpeg_encoder::Encoder::new(&mut out, 95);
        encoder
            .encode(&pixels, w, h, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        out
    }

    fn rendered(rgb: [u8; 3], out: Format) -> Vec<u8> {
        let mut p = Pipeline::default();
        let look = Look {
            sharpen: 0.0,
            ..Look::default()
        };
        p.decode(&jpeg_of(1056, 704, rgb), look).unwrap();
        assert_eq!(p.size(), (1056, 704));
        let mut dst = vec![0; out.bytes()];
        p.render(out, &mut dst).unwrap();
        dst
    }

    fn at(buf: &[u8], i: usize) -> u8 {
        buf.get(i).copied().unwrap()
    }

    fn near(got: u8, want: u8) {
        assert!(
            (i32::from(got) - i32::from(want)).abs() <= 2,
            "got {got}, want {want}"
        );
    }

    #[test]
    fn sharpening_lifts_edges_and_leaves_grain() {
        let (w, h) = (8, 3);
        let mut grain: Vec<u8> = (0..w * h)
            .map(|i| if i % 2 == 0 { 120 } else { 126 })
            .collect();
        let before = grain.clone();
        sharpen(&mut grain, &mut Vec::new(), w, h, 1.0);
        assert_eq!(grain, before);
        let mut edge: Vec<u8> = (0..w * h)
            .map(|i| if i % w < 4 { 60 } else { 180 })
            .collect();
        sharpen(&mut edge, &mut Vec::new(), w, h, 1.0);
        assert!(at(&edge, w + 3) < 60 && at(&edge, w + 4) > 180);
    }

    #[test]
    fn crops_three_by_two_to_sixteen_by_nine() {
        assert_eq!(crop_16_9(1056, 704), (1056, 594));
        assert_eq!(crop_16_9(1920, 1080), (1920, 1080));
        assert_eq!(crop_16_9(800, 300), (532, 300));
    }

    #[test]
    fn zoom_crops_the_middle_before_scaling() {
        assert_eq!(zoomed((1056, 704), 100), (1056, 704));
        assert_eq!(zoomed((1056, 704), 200), (528, 352));
        assert_eq!(zoomed((1056, 594), 150), (704, 396));
        let mut p = Pipeline::default();
        let look = Look {
            zoom: 200,
            ..Look::default()
        };
        p.decode(&jpeg_of(1056, 704, [255, 0, 0]), look).unwrap();
        assert_eq!(p.size(), (528, 352));
    }

    #[test]
    fn mirror_and_upside_down_move_pixels_where_they_belong() {
        let f = Format {
            width: 4,
            height: 2,
            fourcc: NV12,
        };
        let mut frame: Vec<u8> = (0..f.bytes()).map(|i| i as u8).collect();
        let before = frame.clone();
        turn(f, &mut frame, true, true);
        let luma: Vec<u8> = before.iter().take(8).rev().copied().collect();
        assert_eq!(frame.get(..8).unwrap(), luma);
        assert_eq!(
            frame.get(8..).unwrap(),
            [
                at(&before, 10),
                at(&before, 11),
                at(&before, 8),
                at(&before, 9)
            ]
        );
        let f = Format { fourcc: YUY2, ..f };
        let mut frame = vec![10, 128, 20, 130, 30, 132, 40, 134];
        turn(f, &mut frame, true, false);
        assert_eq!(frame, [40, 132, 30, 134, 20, 128, 10, 130]);
    }

    #[test]
    fn broken_frame_sizes_are_refused() {
        let mut p = Pipeline::default();
        let look = Look {
            zoom: 150,
            ..Look::default()
        };
        for (width, height) in [(64, 2), (8192, 8192)] {
            let format = capture::Format::Raw {
                pixels: capture::Pixels::Yuy2,
                width,
                height,
                bt709: false,
            };
            assert!(
                p.decode_frame(&[], format, look).is_err(),
                "{width}x{height}"
            );
        }
        let out = Format {
            width: 1280,
            height: 720,
            fourcc: NV12,
        };
        assert!(p.render(out, &mut vec![0; out.bytes()]).is_err());
    }

    #[test]
    fn wide_pictures_crop_instead_of_squashing() {
        let look = Look {
            fit: true,
            zoom: 100,
            ..Look::default()
        };
        assert_eq!(crop_size((2560, 1080), look), (1920, 1080));
        assert_eq!(crop_size((1056, 704), look), (1056, 704));
    }

    #[test]
    fn a_short_frame_keeps_the_last_good_one() {
        let mut p = Pipeline::default();
        let look = Look::default();
        let raw = |width, height| capture::Format::Raw {
            pixels: capture::Pixels::Yuy2,
            width,
            height,
            bt709: false,
        };
        p.decode_frame(&[81, 90, 81, 240].repeat(8 * 16), raw(16, 16), look)
            .unwrap();
        let good = p.frame().0.to_vec();
        assert!(p.decode_frame(&[0; 64], raw(32, 32), look).is_err());
        let (pixels, width, _) = p.frame();
        assert_eq!((pixels, width), (good.as_slice(), 16));
    }

    #[test]
    fn raw_webcam_frames_land_on_the_same_colours() {
        // BT.601 limited red as a webcam sends it in YUY2, which apps get as BT.709 red.
        let mut p = Pipeline::default();
        let look = Look {
            sharpen: 0.0,
            ..Look::default()
        };
        let yuy2: Vec<u8> = [81, 90, 81, 240].repeat(640 * 720);
        let format = capture::Format::Raw {
            pixels: capture::Pixels::Yuy2,
            width: 1280,
            height: 720,
            bt709: false,
        };
        p.decode_frame(&yuy2, format, look).unwrap();
        assert_eq!(p.size(), (1280, 720));
        let out = Format {
            width: 1920,
            height: 1080,
            fourcc: NV12,
        };
        let mut nv12 = vec![0; out.bytes()];
        p.render(out, &mut nv12).unwrap();
        let uv = 1920 * 1080 + 1920 * 270 + 960;
        near(at(&nv12, 1920 * 540 + 960), 63);
        near(at(&nv12, uv), 102);
        near(at(&nv12, uv + 1), 240);
        // Read as RGB, red is dark. Read as YCbCr by mistake, it would be full brightness.
        let (pixels, width, rgb) = p.frame();
        assert!(rgb);
        assert!(
            crate::exposure::brightness(pixels, width, rgb)
                < crate::exposure::brightness(pixels, width, false)
        );
    }

    /// BT.709 limited red is Y 63, Cb 102, Cr 240.
    #[test]
    fn yuy2_interleaves_luma_and_chroma() {
        let out = Format {
            width: 640,
            height: 360,
            fourcc: YUY2,
        };
        let yuy2 = rendered([255, 0, 0], out);
        let px = (180 * 640 + 320) * 2;
        near(at(&yuy2, px), 63);
        near(at(&yuy2, px + 1), 102);
        near(at(&yuy2, px + 2), 63);
        near(at(&yuy2, px + 3), 240);
    }

    #[test]
    fn gpu_path_matches_colours() {
        let Some(dir) = std::env::var_os("VIEWFINDER_VFX").map(PathBuf::from) else {
            return;
        };
        let out = Format {
            width: 1920,
            height: 1080,
            fourcc: NV12,
        };
        let mut p = Pipeline::with_gpu(Some(dir));
        let mut dst = vec![0; out.bytes()];
        let bicubic = Look {
            vsr_quality: 0,
            ..Look::default()
        };
        p.decode(
            &jpeg_of(1056, 704, [255, 0, 0]),
            Look {
                vsr_quality: 3,
                vsr_strength: 0.0,
                ..bicubic
            },
        )
        .unwrap();
        p.render(out, &mut dst).unwrap();
        let uv = 1920 * 1080 + 1920 * 270 + 960;
        near(at(&dst, 1920 * 540 + 960), 63);
        near(at(&dst, uv), 102);
        near(at(&dst, uv + 1), 240);

        let pixels = test_pattern();
        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, 85)
            .encode(&pixels, 1056, 704, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        p.decode(&jpeg, Look::default()).unwrap();
        p.render(out, &mut dst).unwrap();
        assert!(p.gpu_problem().is_none());

        for (w, h) in [(1280, 720), (640, 360)] {
            let f = Format {
                width: w,
                height: h,
                fourcc: NV12,
            };
            let mut dst = vec![0; f.bytes()];
            p.decode(&jpeg, Look::default()).unwrap();
            p.render(f, &mut dst).unwrap();
            assert!(p.gpu_problem().is_none(), "{w}x{h}: {:?}", p.gpu_problem());
            assert_eq!(
                at(&dst, (h as usize).div_euclid(2) * w as usize),
                16,
                "{w}x{h} has no bar"
            );
        }

        let look = Look {
            fit: false,
            zoom: 200,
            pan: 0.0,
            mirror: true,
            upside_down: true,
            ..Look::default()
        };
        p.decode(&jpeg, look).unwrap();
        p.render(out, &mut dst).unwrap();
        assert!(p.gpu_problem().is_none(), "zoomed: {:?}", p.gpu_problem());
    }

    fn preview_by_pixel(f: Format, frame: &[u8]) -> Vec<u8> {
        let (w, h) = (f.width as usize, f.height as usize);
        let step = if w > 1280 { 2 } else { 1 };
        let at = |i: usize| frame.get(i).copied().unwrap_or_default();
        let mut rgba = Vec::new();
        for py in 0..h.div_euclid(step) {
            let row = py * step;
            for px in 0..w.div_euclid(step) {
                let col = (px * step) & !1;
                let (y, cb, cr) = if f.fourcc == YUY2 {
                    let i = (row * w + col) * 2;
                    (at(i + ((px * step) & 1) * 2), at(i + 1), at(i + 3))
                } else {
                    let uv = w * h + row.div_euclid(2) * w + col;
                    (at(row * w + px * step), at(uv), at(uv + 1))
                };
                rgba.extend_from_slice(&bt709_limited_to_rgba(y, cb, cr));
            }
        }
        rgba
    }

    #[test]
    fn preview_matches_pixel_by_pixel_on_random_frames() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut rgba = Vec::new();
        for (width, height) in [(1920, 1080), (1280, 720), (1622, 1082), (640, 360), (2, 2)] {
            for fourcc in [NV12, YUY2] {
                let f = Format {
                    width,
                    height,
                    fourcc,
                };
                for _ in 0..4 {
                    let frame: Vec<u8> = (0..f.bytes())
                        .map(|_| {
                            state ^= state << 13;
                            state ^= state >> 7;
                            state ^= state << 17;
                            (state >> 32) as u8
                        })
                        .collect();
                    let size = preview_output(f, &frame, &mut rgba);
                    let step = if width > 1280 { 2 } else { 1 };
                    assert_eq!(
                        size,
                        (
                            width.div_euclid(step) as usize,
                            height.div_euclid(step) as usize
                        )
                    );
                    assert!(
                        rgba == preview_by_pixel(f, &frame),
                        "{width}x{height} {fourcc:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn fit_keeps_the_whole_frame_between_black_bars() {
        for fourcc in [NV12, YUY2] {
            let out = Format {
                width: 1920,
                height: 1080,
                fourcc,
            };
            let frame = rendered([255, 0, 0], out);
            // 1056x704 at 1080 tall is 1620 wide, so the bars are 150 pixels each.
            let luma = |x: usize| {
                if fourcc == YUY2 {
                    at(&frame, (540 * 1920 + x) * 2)
                } else {
                    at(&frame, 540 * 1920 + x)
                }
            };
            for x in [0, 149, 1771, 1919] {
                assert_eq!(luma(x), 16, "{fourcc:#x} pixel {x} isn't bar black");
            }
            for x in [152, 960, 1767] {
                near(luma(x), 63);
            }
            if fourcc == NV12 {
                let uv = 1920 * 1080 + 270 * 1920;
                assert_eq!([at(&frame, uv), at(&frame, uv + 1)], [128, 128]);
                near(at(&frame, uv + 960 + 1), 240);
            }
        }
    }

    fn test_pattern() -> Vec<u8> {
        (0..1056 * 704)
            .flat_map(|i: usize| {
                [
                    i.rem_euclid(251) as u8,
                    i.rem_euclid(97) as u8,
                    i.div_euclid(1056).rem_euclid(256) as u8,
                ]
            })
            .collect()
    }

    #[test]
    fn full_frame_fits_well_inside_the_frame_budget() {
        let pixels = test_pattern();
        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, 85)
            .encode(&pixels, 1056, 704, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        let out = Format {
            width: 1920,
            height: 1080,
            fourcc: NV12,
        };
        let mut dst = vec![0; out.bytes()];
        let mut p = Pipeline::default();
        p.decode(&jpeg, Look::default()).unwrap();
        p.render(out, &mut dst).unwrap();
        let start = Instant::now();
        for _ in 0..20 {
            p.decode(&jpeg, Look::default()).unwrap();
            p.render(out, &mut dst).unwrap();
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / 20.0;
        println!("decode + 1080p NV12 render: {ms:.1} ms");
        if !cfg!(debug_assertions) {
            assert!(ms < 20.0, "{ms:.1} ms per frame");
        }
    }
}
