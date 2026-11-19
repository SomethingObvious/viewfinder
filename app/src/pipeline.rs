//! Live view JPEG to NV12 or YUY2 at whatever size the camera DLL asks for.
//!
//! The JPEG is decoded straight to YCbCr and never goes through RGB. One
//! fixed-point matrix turns JFIF's full-range BT.601 into the limited-range
//! BT.709 that apps assume for HD video, then the luma and chroma planes are
//! resized separately, which is what NV12 wants anyway.
//!
//! With super resolution on, the JPEG is decoded to RGB instead and NVIDIA's
//! VideoSuperRes upscales the cropped window on the GPU, removing JPEG
//! blocking on the way. If that ever fails the CPU path takes over for good.

use std::path::PathBuf;
use std::time::Instant;

use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use shared::{Format, NV12, YUY2};
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

const FIXED: i32 = 1 << 16;
const HALF: i32 = FIXED >> 1;

/// User-facing picture settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    /// Where the 16:9 crop sits in the 3:2 frame, 0 is the top edge.
    pub framing: f32,
    /// Unsharp mask strength on luma for the CPU path, 0 turns it off.
    pub sharpen: f32,
    /// VideoSuperRes `QualityLevel`, 0 turns the GPU path off.
    pub vsr_quality: u32,
    /// VideoSuperRes `Strength`, 0 to 1.
    pub vsr_strength: f32,
    /// Denoise level on the GPU before upscaling, 8 to 11, or 0 for none.
    pub denoise: u32,
    /// Whether hot and dead pixels get filled from their neighbours.
    pub fix_pixels: bool,
    /// Show the whole 3:2 frame with black bars at the sides, rather than cropping to 16:9.
    pub fit: bool,
    /// Pace apps at an even 30 fps, drawing the frames between camera frames with NVIDIA's
    /// frame generation. Needs the GPU path.
    pub smooth: bool,
}

impl Default for Look {
    fn default() -> Self {
        // Mode 3 removed blocking without smearing text in the synthetic test.
        // The streaming modes greyed out fine detail.
        // Denoise 10 took grain off flat areas by 12 to 17 dB and still gained on detail, for
        // about 4 ms. Ultra cost the same and lost detail.
        Self {
            framing: 0.5,
            sharpen: 0.35,
            vsr_quality: 3,
            vsr_strength: 1.0,
            denoise: 10,
            fix_pixels: true,
            fit: true,
            smooth: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Ycc,
    /// Byte offset and row stride of the cropped window in `decoded`.
    Rgb {
        offset: usize,
        stride: usize,
    },
}

/// `VideoFrameGeneration`'s medium mode. On a sliding test scene at the camera's size it took
/// 4 to 6 ms and scored 34 to 45 dB against the true middle frame, where a plain blend scored
/// 19 to 28. High mode scored about the same below 16 pixels of motion and took 30 ms.
const FRAMEGEN_MODE: u32 = 1;

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

pub(crate) struct Pipeline {
    decoded: Vec<u8>,
    luma: Vec<u8>,
    chroma: Vec<u8>,
    blur: Vec<u8>,
    scratch_luma: Vec<u8>,
    scratch_chroma: Vec<u8>,
    width: u32,
    height: u32,
    matrix: [[i32; 3]; 3],
    resizer: Resizer,
    source: Source,
    /// Whether `luma` and `chroma` hold the current frame, which an RGB decode only fills
    /// when a frame has to be scaled down.
    planes_fresh: bool,
    gpu: Gpu,
    repair: crate::pixels::Repair,
    staging: Vec<u8>,
    padded: Vec<u8>,
    /// Recent decoded frames by number, oldest first, for drawing moments between them.
    held: std::collections::VecDeque<(u64, Vec<u8>)>,
    spare: Vec<Vec<u8>>,
    decoded_size: (usize, usize),
    framegen: Option<vsr::FrameGen>,
    framegen_failed: Option<String>,
    /// Frame numbers now uploaded to `framegen` as its earlier and later frame.
    pair: Option<(u64, u64)>,
    /// Whether the current frame is the one `framegen` drew, which stays on the GPU and
    /// leaves `decoded` stale until something on the CPU needs it.
    generated: bool,
    /// How many GPU passes have been built, so the caller can tell when one was.
    builds: u32,
    /// Milliseconds spent per stage since the caller last took them: decode, pixel repair,
    /// the GPU and the conversion for output, in that order.
    pub(crate) times: [f32; 4],
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            decoded: Vec::new(),
            luma: Vec::new(),
            chroma: Vec::new(),
            blur: Vec::new(),
            scratch_luma: Vec::new(),
            scratch_chroma: Vec::new(),
            width: 0,
            height: 0,
            matrix: bt601_full_to_bt709_limited(),
            resizer: Resizer::new(),
            source: Source::Ycc,
            planes_fresh: false,
            gpu: Gpu::default(),
            repair: crate::pixels::Repair::default(),
            staging: Vec::new(),
            padded: Vec::new(),
            held: std::collections::VecDeque::new(),
            spare: Vec::new(),
            decoded_size: (0, 0),
            framegen: None,
            framegen_failed: None,
            pair: None,
            generated: false,
            builds: 0,
            times: [0.0; 4],
        }
    }
}

impl Pipeline {
    /// A pipeline that can upscale on the GPU with NVIDIA's DLLs from `vfx`.
    pub(crate) fn with_gpu(vfx: Option<PathBuf>) -> Self {
        let mut p = Self::default();
        p.gpu.dir = vfx;
        p
    }

    /// How many hot or dead pixels are being filled.
    pub(crate) const fn stuck_pixels(&self) -> usize {
        self.repair.count()
    }

    /// Why the GPU path was given up on, if it was.
    pub(crate) fn gpu_problem(&self) -> Option<&str> {
        self.gpu.failed.as_deref()
    }

    /// Size of the cropped 16:9 picture from the last decode.
    #[cfg(test)]
    pub(crate) const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Decodes one live view JPEG into cropped, converted planes.
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
        let len = decoder
            .output_buffer_size()
            .ok_or("live view JPEG has no size")?;
        if len != w * h * 3 {
            return Err(format!("live view JPEG is {w}x{h} but not 3-channel"));
        }
        let started = Instant::now();
        self.decoded.resize(len, 0);
        self.generated = false;
        decoder
            .decode_into(&mut self.decoded)
            .map_err(|e| format!("bad live view JPEG: {e:?}"))?;
        self.decoded_size = (w, h);
        self.times[DECODE] += ms_since(started);
        if look.fix_pixels {
            let started = Instant::now();
            self.repair.apply(&mut self.decoded, w, h, !gpu);
            self.times[REPAIR] += ms_since(started);
        }

        let (cw, ch) = if look.fit { (w, h) } else { crop_16_9(w, h) };
        let left = (w - cw).div_euclid(2);
        let top = ((h - ch) as f32 * look.framing.clamp(0.0, 1.0)).round() as usize;
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
            return Ok(());
        }
        self.source = Source::Ycc;
        let started = Instant::now();
        self.fill_planes((top * w + left) * 3, w * 3, self.matrix, 128);
        if look.sharpen > 0.0 {
            sharpen(&mut self.luma, &mut self.blur, cw, ch, look.sharpen);
        }
        self.times[OUTPUT] += ms_since(started);
        Ok(())
    }

    /// The last decoded frame's level for steady exposure, see `exposure::brightness`.
    pub(crate) fn brightness(&self) -> f32 {
        crate::exposure::brightness(
            &self.decoded,
            self.decoded_size.0,
            matches!(self.source, Source::Rgb { .. }),
        )
    }

    const fn gpu_path(&self, look: Look) -> bool {
        look.vsr_quality > 0 && self.gpu.dir.is_some() && self.gpu.failed.is_none()
    }

    /// Whether frames can be paced and filled in, which needs RGB frames from the GPU path.
    pub(crate) const fn can_smooth(&self, look: Look) -> bool {
        look.smooth && self.gpu_path(look)
    }

    /// Why frame generation was turned off, if it was.
    pub(crate) fn smooth_problem(&self) -> Option<&str> {
        self.framegen_failed.as_deref()
    }

    /// Keeps the frame just decoded as number `seq`.
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
        self.pair = None;
    }

    /// Makes the current frame the moment `at` frames along the held ones, so 4.25 is a
    /// quarter of the way from frame 4 to frame 5. Frames before `at` are dropped.
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
        let t = at - base as f64;
        let next = self.held.iter().position(|(seq, _)| *seq == base + 1);
        let between = base == first && (0.04..=0.96).contains(&t);
        if let Some(i) = next
            && between
            && self.framegen_failed.is_none()
        {
            let started = Instant::now();
            let generated = self.generate(i, t as f32);
            self.times[GPU] += ms_since(started);
            match generated {
                Ok(()) => {
                    self.planes_fresh = false;
                    self.generated = true;
                    return;
                }
                Err(e) => self.framegen_failed = Some(e),
            }
        }
        let pick = match next {
            Some(i) if base == first && t >= 0.5 => i,
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
        self.generated = false;
    }

    /// Draws the moment `t` between the first held frame and held frame `next`, on the GPU.
    fn generate(&mut self, next: usize, t: f32) -> Result<(), String> {
        let stride = self.decoded_size.0 * 3;
        self.build_framegen()?;
        let generator = self.framegen.as_mut().ok_or(NO_FRAMEGEN)?;
        let (Some((first, earlier)), Some((second, later))) =
            (self.held.front(), self.held.get(next))
        else {
            return Err("no held frames to draw between".into());
        };
        let pair = Some((*first, *second));
        if self.pair != pair {
            self.pair = None;
            generator.upload(0, earlier, stride)?;
            generator.upload(1, later, stride)?;
            self.pair = pair;
        }
        generator.draw(t)
    }

    fn build_framegen(&mut self) -> Result<(), String> {
        if self.framegen.is_none() {
            let (w, h) = self.decoded_size;
            let dir = self
                .gpu
                .dir
                .as_deref()
                .ok_or("NVIDIA's libraries aren't installed")?;
            self.framegen = Some(vsr::FrameGen::new(
                dir,
                (w as u32, h as u32),
                FRAMEGEN_MODE,
            )?);
            self.pair = None;
            self.builds += 1;
        }
        Ok(())
    }

    /// Builds the GPU passes a call at `out` will use and runs each once on the last frame, since
    /// a fresh pass takes another 0.1 to 0.2 s the first time. All of it takes about a second.
    /// Does nothing before the first frame, when the camera's size isn't known.
    pub(crate) fn prepare(&mut self, out: Format, look: Look) {
        if self.width == 0 || !self.gpu_path(look) || self.gpu.settings.is_none() {
            return;
        }
        self.bring_back().ok();
        if self.can_smooth(look) && self.framegen_failed.is_none() {
            let w = self.decoded_size.0;
            let warmed = self.build_framegen().and_then(|()| {
                let g = self.framegen.as_mut().ok_or(NO_FRAMEGEN)?;
                g.upload(0, &self.decoded, w * 3)?;
                g.upload(1, &self.decoded, w * 3)?;
                g.draw(0.5)
            });
            self.pair = None;
            if let Err(e) = warmed {
                self.framegen_failed = Some(e);
            }
        }
        // A failure here is recorded by render, as it would be on a real frame.
        let mut scratch = vec![0; out.bytes()];
        self.render(out, &mut scratch).ok();
    }

    /// How many GPU passes have been built so far.
    pub(crate) const fn builds(&self) -> u32 {
        self.builds
    }

    /// Whether any GPU passes are built, holding video memory.
    pub(crate) const fn on_gpu(&self) -> bool {
        !self.gpu.models.is_empty() || self.framegen.is_some()
    }

    /// Gives back the GPU passes and the frames kept for pacing. `prepare` builds them again.
    pub(crate) fn release(&mut self) {
        self.bring_back().ok();
        self.gpu.models.clear();
        self.framegen = None;
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

    /// Brings a generated frame into `decoded`, for the work done on the CPU.
    fn bring_back(&mut self) -> Result<(), String> {
        if !self.generated {
            return Ok(());
        }
        let (w, h) = self.decoded_size;
        self.decoded.resize(w * h * 3, 0);
        let started = Instant::now();
        let downloaded = self
            .framegen
            .as_mut()
            .ok_or("no generated frame")?
            .download(&mut self.decoded, w * 3);
        self.times[GPU] += ms_since(started);
        downloaded?;
        self.generated = false;
        Ok(())
    }

    /// Converts the cropped window of `decoded` into `luma` and full-size interleaved
    /// `chroma`. `centre` is 128 when the second and third channels are signed chroma.
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
                // Chroma can be negative, and checked_div truncates toward zero like `/` does.
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

    /// Scales the last decoded picture into `dst` in format `out`.
    /// Renders the decoded picture into `dst`, scaled to the height of `out` and centred
    /// between black bars when it's narrower than `out`.
    pub(crate) fn render(&mut self, out: Format, dst: &mut [u8]) -> Result<(), String> {
        if self.width == 0 {
            return Err("nothing decoded yet".into());
        }
        let inner = self.fit(out);
        if inner == out {
            return self.render_exact(out, dst);
        }
        let mut picture = std::mem::take(&mut self.padded);
        picture.resize(inner.bytes(), 0);
        let result = self.render_exact(inner, &mut picture).and_then(|()| {
            pillarbox(inner, &picture, out, frame_of(dst, out)?);
            Ok(())
        });
        self.padded = picture;
        result
    }

    /// The largest even-width format of the picture's shape that `out` holds at full height.
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
        if let Source::Rgb { offset, stride } = self.source
            && out.height < self.height
        {
            if !self.planes_fresh {
                self.bring_back()?;
                self.fill_planes(offset, stride, rgb_to_bt709_limited(), 0);
            }
        } else if let Source::Rgb { offset, stride } = self.source {
            let result = self.render_gpu(out, dst, offset, stride);
            if let Err(e) = &result {
                self.gpu.failed = Some(e.clone());
                self.gpu.models.clear();
            }
            return result;
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

    /// Which of `gpu.models` takes the current picture size to `output`, built if need be.
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
        let (pixel, row) = (offset.div_euclid(3), stride.div_euclid(3));
        let (left, top) = (pixel.rem_euclid(row) as u32, pixel.div_euclid(row) as u32);
        // The call blocks until the GPU is done, so timing around it needs no extra sync.
        let mut gpu = 0.0;
        let mut run = |nv12: &mut [u8]| {
            let started = Instant::now();
            let result = match self.framegen.as_ref().filter(|_| self.generated) {
                Some(g) => model.run_from(g, left, top, nv12),
                None => model.run(window, stride, nv12),
            };
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

    /// Whether the current frame is a decoded one, so `preview` can read it without
    /// bringing a generated frame back from the GPU.
    pub(crate) const fn on_cpu(&self) -> bool {
        !self.generated
    }

    /// RGBA at half the cropped size, for the preview window.
    pub(crate) fn preview(&mut self, rgba: &mut Vec<u8>) -> (usize, usize) {
        if self.bring_back().is_err() {
            self.framegen_failed = Some("the generated frame couldn't be read back".into());
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let (pw, ph) = (w.div_euclid(2), h.div_euclid(2));
        rgba.clear();
        rgba.reserve(pw * ph * 4);
        if pw == 0 || ph == 0 {
            return (pw, ph);
        }
        if let Source::Rgb { offset, stride } = self.source {
            let window = self.decoded.get(offset..).unwrap_or_default();
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

/// Indexes into `Pipeline::times`.
pub(crate) const DECODE: usize = 0;
pub(crate) const REPAIR: usize = 1;
pub(crate) const GPU: usize = 2;
pub(crate) const OUTPUT: usize = 3;

fn ms_since(started: Instant) -> f32 {
    started.elapsed().as_secs_f32() * 1000.0
}

#[derive(Clone, Copy)]
enum Plane {
    Luma,
    Chroma,
}

/// What `framegen` is missing when it should have just been built.
const NO_FRAMEGEN: &str = "frame generation isn't built";
/// What `gpu.models` is missing when `model` should have just returned it.
const NO_MODEL: &str = "the super resolution model went missing";

/// The first `f.bytes()` of `dst`, or an error when it's too small for a frame.
fn frame_of(dst: &mut [u8], f: Format) -> Result<&mut [u8], String> {
    let len = dst.len();
    dst.get_mut(..f.bytes())
        .ok_or_else(|| format!("{len} bytes is too small for a {f:?} frame"))
}

/// Copies `picture` into the middle of `dst` and paints the sides video-range black.
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

/// RGBA from a rendered NV12 or YUY2 frame, so the preview shows exactly what apps get.
/// Frames wider than 1280 are halved, which still covers the preview on a 4K screen.
pub(crate) fn preview_output(f: Format, frame: &[u8], rgba: &mut Vec<u8>) -> (usize, usize) {
    let (w, h) = (f.width as usize, f.height as usize);
    let step = if w > 1280 { 2 } else { 1 };
    let (pw, ph) = (w.div_euclid(step), h.div_euclid(step));
    let at = |i: usize| frame.get(i).copied().unwrap_or_default();
    rgba.clear();
    rgba.reserve(pw * ph * 4);
    for py in 0..ph {
        let row = py * step;
        for px in 0..pw {
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
    (pw, ph)
}

/// Fixed-point BT.709 video range to full-range RGB, 8 fractional bits.
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

/// Repeats each NV12 chroma row for the two luma rows it covers.
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

/// The largest centred-width 16:9 region, with even sides for chroma.
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

/// `to_rgb` takes the decoded pixel to full-range RGB.
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

/// Unsharp mask against a 3x3 box blur. Edges are left as they are.
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
            // Detail can be negative, and checked_div truncates toward zero like `/` does.
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
    fn crops_three_by_two_to_sixteen_by_nine() {
        assert_eq!(crop_16_9(1056, 704), (1056, 594));
        assert_eq!(crop_16_9(1920, 1080), (1920, 1080));
        assert_eq!(crop_16_9(800, 300), (532, 300));
    }

    #[test]
    fn mid_grey_lands_on_video_range_grey() {
        let out = Format {
            width: 1280,
            height: 720,
            fourcc: NV12,
        };
        let nv12 = rendered([128, 128, 128], out);
        near(at(&nv12, 1280 * 360 + 640), 126);
        near(at(&nv12, 1280 * 720 + 1280 * 180 + 640), 128);
    }

    #[test]
    fn pure_red_matches_bt709_limited() {
        // BT.709 limited red is Y 63, Cb 102, Cr 240.
        let out = Format {
            width: 1920,
            height: 1080,
            fourcc: NV12,
        };
        let nv12 = rendered([255, 0, 0], out);
        let uv = 1920 * 1080 + 1920 * 270 + 960;
        near(at(&nv12, 1920 * 540 + 960), 63);
        near(at(&nv12, uv), 102);
        near(at(&nv12, uv + 1), 240);
    }

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

    /// Runs only with `VIEWFINDER_VFX` pointing at NVIDIA's DLLs.
    #[test]
    fn gpu_path_matches_colours_and_timing() {
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
        // Quality 0 takes the CPU path, so force bicubic through the GPU to check colours.
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
        let start = Instant::now();
        for _ in 0..30 {
            p.decode(&jpeg, Look::default()).unwrap();
            p.render(out, &mut dst).unwrap();
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / 30.0;
        println!("decode + 1080p super resolution: {ms:.1} ms");
        assert!(p.gpu_problem().is_none());

        // 640x360 scales the picture down, which the GPU path has to handle too.
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

    /// A 1056x704 RGB picture with detail everywhere, so the JPEG isn't trivially small.
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
