//! Runs only with `VSR_DIR` pointing at the folder of NVIDIA's DLLs.

use std::path::PathBuf;

use vsr::{Settings, SuperRes};

const IN: (u32, u32) = (1056, 594);
const OUT: (u32, u32) = (1920, 1080);

fn dir() -> Option<PathBuf> {
    let dir = std::env::var_os("VSR_DIR").map(PathBuf::from);
    if dir.is_none() {
        println!("VSR_DIR is not set, skipping");
    }
    dir
}

fn render(e: &mut SuperRes, rgb: &[u8]) -> Result<Vec<u8>, String> {
    let mut nv12 = vec![0; (OUT.0 * OUT.1 * 3).div_euclid(2) as usize];
    e.run(rgb, IN.0 as usize * 3, &mut nv12)?;
    Ok(nv12)
}

fn flat(rgb: [u8; 3]) -> Vec<u8> {
    (0..IN.0 * IN.1).flat_map(|_| rgb).collect()
}

fn centre(nv12: &[u8]) -> Option<[u8; 3]> {
    let (w, h) = (OUT.0 as usize, OUT.1 as usize);
    let uv = w * h + h.div_euclid(4) * w + w.div_euclid(2);
    let y = *nv12.get(h.div_euclid(2) * w + w.div_euclid(2))?;
    Some([y, *nv12.get(uv)?, *nv12.get(uv + 1)?])
}

fn near(got: [u8; 3], want: [u8; 3]) {
    let ok = got.iter().zip(want).all(|(&g, w)| g.abs_diff(w) <= 3);
    assert!(ok, "got YCbCr {got:?}, want {want:?}");
}

#[test]
fn flat_colours_land_on_bt709_limited() {
    let Some(dir) = dir() else { return };
    let mut e = SuperRes::new(
        &dir,
        IN,
        OUT,
        Settings {
            quality: 0,
            strength: 1.0,
            denoise: 0,
        },
    )
    .unwrap();
    near(
        centre(&render(&mut e, &flat([128, 128, 128])).unwrap()).unwrap(),
        [126, 128, 128],
    );
    near(
        centre(&render(&mut e, &flat([255, 0, 0])).unwrap()).unwrap(),
        [63, 102, 240],
    );
}

#[test]
fn ai_path_differs_from_bicubic() {
    let Some(dir) = dir() else { return };
    let pattern: Vec<u8> = (0..IN.1)
        .flat_map(|y| {
            (0..IN.0).map(move |x| {
                if (x.div_euclid(2) + y.div_euclid(3)) % 2 == 0 {
                    40u8
                } else {
                    220
                }
            })
        })
        .flat_map(|v| [v, v, v])
        .collect();
    let mut e = SuperRes::new(
        &dir,
        IN,
        OUT,
        Settings {
            quality: 0,
            strength: 1.0,
            denoise: 0,
        },
    )
    .unwrap();
    let bicubic = render(&mut e, &pattern).unwrap();
    e.set(Settings {
        quality: 3,
        strength: 1.0,
        denoise: 0,
    })
    .unwrap();
    let upscaled = render(&mut e, &pattern).unwrap();
    let changed = bicubic
        .iter()
        .zip(&upscaled)
        .filter(|(a, b)| a.abs_diff(**b) > 2)
        .count();
    assert!(
        changed > bicubic.len().div_euclid(100),
        "only {changed} samples moved"
    );
}
