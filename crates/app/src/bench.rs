//! Times each pipeline stage on synthetic JPEGs, without the camera.

use std::time::{Duration, Instant};

use shared::{Format, NV12};

use super::*;

const W: usize = 1056;
const H: usize = 704;

/// A panning scene of edges, fine texture and soft shapes, with fresh grain on every frame like
/// a warm sensor at high ISO.
#[expect(
    clippy::suboptimal_flops,
    clippy::imprecise_flops,
    reason = "mul_add and hypot change rounding, and the scene must stay the same"
)]
#[expect(
    clippy::modulo_arithmetic,
    reason = "the pan goes negative, and the gradient keeps the sign of a truncating remainder"
)]
fn scene(t: usize) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(W * H * 3);
    let pan = (t as f32 * 2.7).sin() * 40.0 + t as f32 * 1.5;
    for y in 0..H {
        for x in 0..W {
            let fx = x as f32 + pan;
            let fy = y as f32 + (t as f32 * 0.21).sin() * 12.0;
            let mut colour = [70.0 + fy * 0.15, 80.0 + fx * 0.05 % 60.0, 110.0 - fy * 0.05];
            let hash =
                (fx as u32).wrapping_mul(0x9E37_79B1) ^ (fy as u32).wrapping_mul(0x85EB_CA77);
            let weave = ((hash ^ (hash >> 15)).wrapping_mul(0x2C1B_3C6D) >> 26) as f32;
            if (300.0..700.0).contains(&fx.rem_euclid(1400.0)) && (200.0..520.0).contains(&fy) {
                colour = [140.0 + weave, 110.0 + weave, 90.0 + weave];
            }
            for (cx, cy, radius, disc) in [
                (500.0, 300.0, 110.0, [215.0, 170.0, 140.0]),
                (850.0, 420.0, 70.0, [40.0, 150.0, 210.0]),
            ] {
                let dist = ((x as f32 - cx - (t as f32 * 0.13).sin() * 60.0).powi(2)
                    + (fy - cy).powi(2))
                .sqrt();
                let cover = (radius - dist + 0.5).clamp(0.0, 1.0);
                for (ch, target) in colour.iter_mut().zip(disc) {
                    *ch += (target - *ch) * cover;
                }
            }
            if (fx / 11.0).floor() as i32 % 2 == 0 && (600.0..660.0).contains(&fy) {
                colour = [235.0, 235.0, 230.0];
            }
            let grain = (x as u32 * 7919 + y as u32 * 104_729 + t as u32 * 15_485_863)
                .wrapping_mul(0x9E37_79B1)
                >> 27;
            rgb.extend(colour.map(|v| (v + grain as f32 - 16.0).clamp(0.0, 255.0) as u8));
        }
    }
    rgb
}

fn jpegs(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|t| {
            let mut jpeg = Vec::new();
            jpeg_encoder::Encoder::new(&mut jpeg, 85)
                .encode(&scene(t), W as u16, H as u16, jpeg_encoder::ColorType::Rgb)
                .unwrap();
            jpeg
        })
        .collect()
}

/// Paces like a call, about 0.93 camera frames per output frame, and prints the mean time of
/// each stage once warm.
#[test]
#[ignore = "needs NVIDIA's DLLs in VIEWFINDER_VFX and a quiet GPU"]
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add changes rounding, and the frame times must stay the same"
)]
fn stages() {
    let vfx = std::env::var_os("VIEWFINDER_VFX").map(std::path::PathBuf::from);
    let call = Format {
        width: 1920,
        height: 1080,
        fourcc: NV12,
    };
    let frames = jpegs(56);
    let look = Look::default();
    let mut pipeline = Pipeline::with_gpu(vfx);
    let mut slot = vec![0u8; call.bytes()];
    let (mut decode, mut show, mut render) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let (mut seq, mut decoded, n) = (0usize, 0u32, 50u32);
    for i in 0..n + 10 {
        if i == 10 {
            (decode, show, render, decoded) = (Duration::ZERO, Duration::ZERO, Duration::ZERO, 0);
        }
        let at = 3.0 + f64::from(i) * 0.93;
        while seq <= (at + 2.2).floor() as usize {
            let t = Instant::now();
            pipeline
                .decode(frames.get(seq % frames.len()).unwrap(), look)
                .unwrap();
            decode += t.elapsed();
            pipeline.hold(seq as u64);
            seq += 1;
            decoded += 1;
        }
        let t = Instant::now();
        pipeline.show_at(at);
        show += t.elapsed();
        let t = Instant::now();
        pipeline.render(call, &mut slot).unwrap();
        render += t.elapsed();
    }
    let ms = |d: Duration, n: u32| d.as_secs_f64() * 1e3 / f64::from(n);
    println!(
        "decode {:.2} ms, show_at {:.2} ms, render {:.2} ms",
        ms(decode, decoded),
        ms(show, n),
        ms(render, n),
    );
    assert!(pipeline.gpu_problem().is_none());
}
