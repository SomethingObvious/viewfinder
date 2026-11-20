//! Scores each denoise level on a grainy JPEG frame against the same upscaler fed the clean one.
//! Usage: `noise [dll dir] [sigma]`.

use std::time::Instant;

use vsr::{Settings, SuperRes};

const IN: (u32, u32) = (1056, 594);
const OUT: (u32, u32) = (1920, 1080);

fn main() -> Result<(), String> {
    let dir = std::env::args_os()
        .nth(1)
        .or_else(|| std::env::var_os("VSR_DIR"))
        .ok_or("pass the DLL folder")?;
    let sigma: f32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10.0);
    let clean = scene();
    let noisy = jpeg(&grain(&clean, sigma), 92)?;
    let clean = jpeg(&clean, 92)?;
    let stride = IN.0 as usize * 3;

    let run = |input: &[u8], denoise: u32| -> Result<(Vec<u8>, f64), String> {
        let mut sr = SuperRes::new(
            dir.as_ref(),
            IN,
            OUT,
            Settings {
                quality: 3,
                strength: 1.0,
                denoise,
            },
        )?;
        let mut nv12 = vec![0; (OUT.0 * OUT.1 * 3).div_euclid(2) as usize];
        sr.run(input, stride, &mut nv12)?;
        let start = Instant::now();
        for _ in 0..30 {
            sr.run(input, stride, &mut nv12)?;
        }
        Ok((nv12, start.elapsed().as_secs_f64() * 1e3 / 30.0))
    };

    let (reference, _) = run(&clean, 0)?;
    let luma = (OUT.0 * OUT.1) as usize;
    println!(
        "grain sigma {sigma}, scored on luma against the clean frame through the same upscaler"
    );
    for denoise in [0, 8, 9, 10, 11] {
        let (out, ms) = run(&noisy, denoise)?;
        let (flat, detail) = split_psnr(
            reference.get(..luma).ok_or("short reference frame")?,
            out.get(..luma).ok_or("short frame")?,
        );
        let name = match denoise {
            0 => "off",
            8 => "low",
            9 => "medium",
            10 => "high",
            _ => "ultra",
        };
        println!(
            "denoise {name:<6} {ms:5.1} ms   PSNR flat areas {flat:5.1} dB, detailed areas {detail:5.1} dB"
        );
    }
    Ok(())
}

/// Gradients like a wall, stripes like fabric and hard marks like text.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add changes rounding, and the scene must stay the same"
)]
fn scene() -> Vec<u8> {
    let (w, h) = (IN.0 as usize, IN.1 as usize);
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let mut p = [60.0 + 120.0 * fx, 70.0 + 80.0 * fy, 90.0 + 40.0 * (fx + fy)];
            if x > w.div_euclid(2) && y > h.div_euclid(2) {
                let s = ((x as f32 * 0.9).sin() * (y as f32 * 0.7).sin()) * 30.0;
                p = [p[0] + s, p[1] + s, p[2] + s];
            }
            if x < w.div_euclid(2)
                && y > h.div_euclid(2)
                && (x.div_euclid(6) % 3 == 0)
                && (y.div_euclid(9) % 2 == 0)
            {
                p = [20.0, 20.0, 25.0];
            }
            rgb.extend(p.map(|v| v.clamp(0.0, 255.0) as u8));
        }
    }
    rgb
}

/// Gaussian grain, stronger in the darks like a real sensor's.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add changes rounding, and the grain must stay the same"
)]
fn grain(rgb: &[u8], sigma: f32) -> Vec<u8> {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut uniform = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f32 / (1u64 << 53) as f32
    };
    rgb.iter()
        .map(|&v| {
            let g = (-2.0 * uniform().max(1e-9).ln()).sqrt()
                * (std::f32::consts::TAU * uniform()).cos();
            let scale = 1.3 - f32::from(v) / 255.0 * 0.6;
            (f32::from(v) + g * sigma * scale).clamp(0.0, 255.0) as u8
        })
        .collect()
}

fn jpeg(rgb: &[u8], quality: u8) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    jpeg_encoder::Encoder::new(&mut bytes, quality)
        .encode(rgb, IN.0 as u16, IN.1 as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| e.to_string())?;
    zune_jpeg::JpegDecoder::new(zune_jpeg::zune_core::bytestream::ZCursor::new(&bytes))
        .decode()
        .map_err(|e| e.to_string())
}

/// The top half is gradients only.
fn split_psnr(a: &[u8], b: &[u8]) -> (f64, f64) {
    let half = a.len().div_euclid(2);
    let psnr = |a: &[u8], b: &[u8]| {
        let mse = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
            .sum::<f64>()
            / a.len() as f64;
        10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
    };
    let ((a_top, a_bottom), (b_top, b_bottom)) = (a.split_at(half), b.split_at(half));
    (psnr(a_top, b_top), psnr(a_bottom, b_bottom))
}
