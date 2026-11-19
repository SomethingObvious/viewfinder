//! Times `VideoFrameGeneration` at the camera's size and scores its middle frame against the true
//! one on a scene that slides sideways, next to a plain blend. Usage: `framegen <dll dir>`.

use std::path::PathBuf;
use std::time::Instant;

use vsr::FrameGen;

const W: usize = 1056;
const H: usize = 704;

/// Discs and stripes on a gradient, `dx` pixels to the right.
#[expect(
    clippy::suboptimal_flops,
    clippy::imprecise_flops,
    reason = "mul_add and hypot change rounding, and the scene must stay the same"
)]
fn scene(dx: f32) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(W * H * 3);
    for y in 0..H {
        for x in 0..W {
            let (fx, fy) = (x as f32 - dx, y as f32);
            let mut c = [40.0 + fy * 0.2, 60.0 + fx * 0.1, 90.0];
            for (cx, cy, r, col) in [
                (300.0, 300.0, 120.0, [220.0, 180.0, 140.0]),
                (700.0, 380.0, 90.0, [60.0, 160.0, 220.0]),
            ] {
                let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                let a = (r - d + 0.5).clamp(0.0, 1.0);
                for (ch, target) in c.iter_mut().zip(col) {
                    *ch += (target - *ch) * a;
                }
            }
            if (fx / 19.0).floor() as i32 % 3 == 0 && (500.0..600.0).contains(&fy) {
                c = [230.0, 230.0, 230.0];
            }
            rgb.extend(c.map(|v| v.clamp(0.0, 255.0) as u8));
        }
    }
    rgb
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    10.0 * (255.0f64.powi(2) / mse.max(1e-9)).log10()
}

fn main() -> Result<(), String> {
    let dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("pass the DLL folder")?;
    for shift in [6.0f32, 16.0, 30.0] {
        let (a, b, truth) = (scene(0.0), scene(shift), scene(shift / 2.0));
        let blend: Vec<u8> = a
            .iter()
            .zip(&b)
            .map(|(&x, &y)| u16::midpoint(u16::from(x), u16::from(y)) as u8)
            .collect();
        for mode in [1, 2] {
            let started = Instant::now();
            let mut g = FrameGen::new(&dir, (W as u32, H as u32), mode)?;
            let setup = started.elapsed();
            let mut out = vec![0u8; W * H * 3];
            g.upload(0, &a, W * 3)?;
            g.upload(1, &b, W * 3)?;
            g.run(0.5, &mut out, W * 3)?;
            let started = Instant::now();
            let n = 30;
            for i in 0..n {
                g.upload(i % 2, if i % 2 == 0 { &a } else { &b }, W * 3)?;
                g.run(0.5, &mut out, W * 3)?;
            }
            let each = started.elapsed().as_secs_f64() * 1000.0 / f64::from(n as u32);
            g.upload(0, &a, W * 3)?;
            g.upload(1, &b, W * 3)?;
            g.run(0.5, &mut out, W * 3)?;
            println!(
                "shift {shift:>2} px  mode {mode}  setup {:>5.0} ms  upload+run {each:>5.1} ms  PSNR {:.1} dB (blend {:.1})",
                setup.as_secs_f64() * 1000.0,
                psnr(&out, &truth),
                psnr(&blend, &truth)
            );
        }
    }
    Ok(())
}
