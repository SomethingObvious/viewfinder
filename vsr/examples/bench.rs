//! Times every useful quality level on a synthetic JPEG-blocked live view frame and saves
//! bicubic-versus-mode PNGs. Usage: `bench [dll dir] [png dir]`, or set `VSR_DIR`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vsr::{Settings, SuperRes};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LoadLibraryExW,
};
use windows::core::{HSTRING, s, w};

const IN: (u32, u32) = (1056, 594);
const MODES: [u32; 8] = [0, 1, 2, 3, 4, 9, 21, 23];
const OUTPUTS: [(u32, u32); 2] = [(1920, 1080), (1280, 720)];

fn main() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let dir = args
        .next()
        .or_else(|| std::env::var_os("VSR_DIR"))
        .map(PathBuf::from)
        .ok_or("pass the DLL folder or set VSR_DIR")?;
    let out_dir = args
        .next()
        .map_or_else(|| std::env::temp_dir().join("vsr-out"), PathBuf::from);
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    let rgb = jpeg_round_trip(&synthetic(), 60)?;
    save_png(&out_dir.join("input.png"), IN.0, &rgb)?;
    let stride = IN.0 as usize * 3;
    let baseline = gpu_free(&dir).ok_or("cudaMemGetInfo failed")?;

    println!("mode  output     new ms  first ms  steady ms  frames  GPU MB");
    for out in OUTPUTS {
        let mut bicubic = Vec::new();
        for mode in MODES {
            // Denoise and deblur refuse to change the resolution, so they run at the input size.
            let size = if (8..=15).contains(&mode) { IN } else { out };
            let mut nv12 = vec![0; (size.0 * size.1 * 3).div_euclid(2) as usize];

            let start = Instant::now();
            let mut e = SuperRes::new(
                &dir,
                IN,
                size,
                Settings {
                    quality: mode,
                    strength: 1.0,
                    denoise: 0,
                },
            )?;
            let new_ms = ms(start.elapsed());
            let start = Instant::now();
            e.run(&rgb, stride, &mut nv12)?;
            let first_ms = ms(start.elapsed());

            let start = Instant::now();
            let mut frames = 0;
            while frames < 60 && start.elapsed() < Duration::from_secs(3) {
                e.run(&rgb, stride, &mut nv12)?;
                frames += 1;
            }
            let steady_ms = ms(start.elapsed()) / f64::from(frames);
            let used = gpu_free(&dir).map(|free| baseline.saturating_sub(free) as f64 / 1e6);
            println!(
                "{mode:>4}  {:>4}x{:<4}  {new_ms:>6.0}  {first_ms:>8.1}  {steady_ms:>9.2}  {frames:>6}  {}",
                size.0,
                size.1,
                used.map_or_else(|| "?".into(), |mb| format!("{mb:.0}"))
            );

            let shown = nv12_to_rgb(&nv12, size.0, size.1)?;
            if mode == 0 {
                bicubic = shown;
            } else if size == out && out.0 == 1920 {
                let name = format!("q{mode:02}_{}x{}", size.0, size.1);
                save_png(
                    &out_dir.join(format!("{name}.png")),
                    size.0 * 2,
                    &side_by_side(&bicubic, &shown, size.0, (0, 0, size.0, size.1))?,
                )?;
                for (tag, x) in [("a", 0), ("b", 960)] {
                    let crop = side_by_side(&bicubic, &shown, size.0, (x, 0, 960, 540))?;
                    save_png(&out_dir.join(format!("{name}_{tag}.png")), 1920, &crop)?;
                }
            } else if size == IN && out.0 == 1920 {
                let reference = nv12_to_rgb(&rgb_to_nv12_reference(&rgb), IN.0, IN.1)?;
                save_png(
                    &out_dir.join(format!("q{mode:02}_{}x{}.png", IN.0, IN.1)),
                    IN.0 * 2,
                    &side_by_side(&reference, &shown, IN.0, (0, 0, IN.0, IN.1))?,
                )?;
            }
        }
    }
    println!(
        "SDK version {}, PNGs in {}",
        sdk_version(),
        out_dir.display()
    );
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Gradients, a zone plate, digits, fine texture and saturated edges, the things JPEG and upscalers get wrong.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add changes rounding, and the frame must stay the same"
)]
fn synthetic() -> Vec<u8> {
    let (w, h) = (IN.0.cast_signed(), IN.1.cast_signed());
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let mut p = [
                (60.0 + 150.0 * fx) as u8,
                (90.0 + 100.0 * fy) as u8,
                (200.0 - 120.0 * fx * fy) as u8,
            ];
            if y < 297 {
                match x.div_euclid(264) {
                    0 => p = digits(x, y),
                    1 => {
                        let (dx, dy) = ((x - 396) as f32, (y - 148) as f32);
                        let v = (128.0 + 110.0 * ((dx * dx + dy * dy) * 0.012).cos()) as u8;
                        p = [v, v, v];
                    }
                    2 => p = texture(x, y),
                    _ => p = edges(x - 792, y),
                }
            } else {
                let (dx, dy) = ((x - 264) as f32 / 160.0, (y - 446) as f32 / 120.0);
                let d = dx * dx + dy * dy;
                if d < 1.0 {
                    let shade = 1.0 - 0.35 * d;
                    p = [
                        (225.0 * shade) as u8,
                        (172.0 * shade) as u8,
                        (148.0 * shade) as u8,
                    ];
                } else if x > 600 && (x % 6 == 0 || y % 7 == 0) {
                    p = [30, 30, 30];
                }
            }
            rgb.extend_from_slice(&p);
        }
    }
    rgb
}

fn digits(x: i32, y: i32) -> [u8; 3] {
    const FONT: [[u8; 7]; 10] = [
        [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
    ];
    // Bands of digits at scales 1, 2 and 3, dark on light like a menu or a page held up to the camera.
    let (scale, top) = match y {
        0..60 => (1, 0),
        60..160 => (2, 60),
        _ => (3, 160),
    };
    let (cx, cy) = (x.div_euclid(scale), (y - top).div_euclid(scale));
    let (col, row) = (cx.rem_euclid(6), cy.rem_euclid(9));
    let glyph = (cx.div_euclid(6) + cy.div_euclid(9) * 7) as usize % 10;
    let ink = col < 5
        && row < 7
        && FONT
            .get(glyph)
            .and_then(|bits| bits.get(row as usize))
            .is_some_and(|bits| bits >> (4 - col) & 1 == 1);
    if ink { [25, 25, 35] } else { [235, 232, 220] }
}

const fn texture(x: i32, y: i32) -> [u8; 3] {
    // Hashed noise with a weave, standing in for hair or fabric that smoothing would erase.
    let n = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77);
    let n = (n ^ (n >> 15)).wrapping_mul(0x2C1B_3C6D) >> 24;
    let weave = if (x + y).rem_euclid(4) < 2 { 30 } else { 0 };
    let v = (70 + n.div_euclid(3).cast_signed() + weave) as u8;
    [v + 40, v + 20, v]
}

fn edges(x: i32, y: i32) -> [u8; 3] {
    let (dx, dy) = (x - 80, y - 90);
    if dx * dx + dy * dy < 60 * 60 {
        [230, 20, 20]
    } else if (150..240).contains(&x) && (40..140).contains(&y) {
        [20, 200, 40]
    } else if y > 180 && (x.div_euclid(12) + y.div_euclid(12)) % 2 == 0 {
        [250, 220, 0]
    } else if y > 180 {
        [120, 0, 160]
    } else {
        [20, 40, 200]
    }
}

fn jpeg_round_trip(rgb: &[u8], quality: u8) -> Result<Vec<u8>, String> {
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, quality)
        .encode(rgb, IN.0 as u16, IN.1 as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| format!("encode: {e}"))?;
    let mut decoder =
        zune_jpeg::JpegDecoder::new(zune_jpeg::zune_core::bytestream::ZCursor::new(&jpeg));
    decoder.decode().map_err(|e| format!("decode: {e}"))
}

/// Same-size limited-range BT.709 NV12 made on the CPU, so the denoise picture has an untouched twin.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add changes rounding, and the reference must stay the same"
)]
fn rgb_to_nv12_reference(rgb: &[u8]) -> Vec<u8> {
    let (width, height) = (IN.0 as usize, IN.1 as usize);
    let mut nv12 = vec![0; (width * height * 3).div_euclid(2)];
    let (luma, chroma) = nv12.split_at_mut(width * height);
    for ((idx, px), y) in rgb.as_chunks::<3>().0.iter().enumerate().zip(luma) {
        let [r, g, b] = px.map(f32::from);
        *y = (16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0).round() as u8;
        let (row, col) = (idx.div_euclid(width), idx.rem_euclid(width));
        if row % 2 == 0 && col % 2 == 0 {
            let at = row.div_euclid(2) * width + col;
            if let Some([cb, cr]) = chroma.get_mut(at..at + 2) {
                *cb = (128.0 + (-0.1146 * r - 0.3854 * g + 0.5 * b) * 224.0 / 255.0).round() as u8;
                *cr = (128.0 + (0.5 * r - 0.4542 * g - 0.0458 * b) * 224.0 / 255.0).round() as u8;
            }
        }
    }
    nv12
}

#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add changes rounding, and the picture must stay the same"
)]
fn nv12_to_rgb(nv12: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let (w, h) = (w as usize, h as usize);
    let sample = |at: usize| {
        nv12.get(at)
            .map(|&v| f32::from(v))
            .ok_or("short NV12 frame")
    };
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let l = 1.164 * (sample(y * w + x)? - 16.0);
            let c = w * h + y.div_euclid(2) * w + x.div_euclid(2) * 2;
            let (cb, cr) = (sample(c)? - 128.0, sample(c + 1)? - 128.0);
            let px = [l + 1.793 * cr, l - 0.213 * cb - 0.533 * cr, l + 2.112 * cb];
            rgb.extend(px.map(|v| v.round().clamp(0.0, 255.0) as u8));
        }
    }
    Ok(rgb)
}

/// Two RGB images cropped to `(x, y, w, h)` and put next to each other.
fn side_by_side(
    left: &[u8],
    right: &[u8],
    width: u32,
    (x, y, w, h): (u32, u32, u32, u32),
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for r in y..y + h {
        let start = ((r * width + x) * 3) as usize;
        for img in [left, right] {
            let span = img
                .get(start..start + w as usize * 3)
                .ok_or("crop is outside the image")?;
            out.extend_from_slice(span);
        }
    }
    Ok(out)
}

fn save_png(path: &Path, width: u32, rgb: &[u8]) -> Result<(), String> {
    let height = (rgb.len() as u32).div_euclid(width).div_euclid(3);
    let file = std::io::BufWriter::new(std::fs::File::create(path).map_err(|e| e.to_string())?);
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(rgb))
        .map_err(|e| e.to_string())
}

/// Free device memory. The first call also creates the CUDA context, so it is not counted.
#[expect(unsafe_code, reason = "CUDA is reached through a raw function pointer")]
fn gpu_free(dir: &Path) -> Option<usize> {
    let path = HSTRING::from(dir.join("cudart64_12.dll").as_os_str());
    // SAFETY: cudaMemGetInfo(size_t *free, size_t *total) from cuda_runtime_api.h.
    unsafe {
        let cudart = LoadLibraryExW(
            &path,
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
        .ok()?;
        let f: unsafe extern "C" fn(*mut usize, *mut usize) -> i32 =
            std::mem::transmute(GetProcAddress(cudart, s!("cudaMemGetInfo"))?);
        let (mut free, mut total) = (0, 0);
        (f(&raw mut free, &raw mut total) == 0).then_some(free)
    }
}

/// `NvVFX_GetVersion` packs major, minor and build into the top three bytes.
#[expect(
    unsafe_code,
    reason = "the SDK is reached through a raw function pointer"
)]
fn sdk_version() -> String {
    // SAFETY: NvVFX_GetVersion(unsigned int *) from nvVideoEffects.h.
    unsafe {
        let Ok(vfx) = GetModuleHandleW(w!("NVVideoEffects.dll")) else {
            return "?".into();
        };
        let Some(f) = GetProcAddress(vfx, s!("NvVFX_GetVersion")) else {
            return "?".into();
        };
        let f: unsafe extern "C" fn(*mut u32) -> i32 = std::mem::transmute(f);
        let mut v = 0;
        f(&raw mut v);
        format!("{}.{}.{}", v >> 24, (v >> 16) & 0xFF, (v >> 8) & 0xFF)
    }
}
