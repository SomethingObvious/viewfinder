//! Turns a raw NV12 or YUY2 frame into packed RGB24, the form the pipeline's GPU path and
//! pixel repair take.

use crate::{Error, Pixels, Result};

/// Limited-range YCbCr to full-range RGB in 1/1024ths, as `[V to R, U to G, V to G, U to B]`.
/// Luma scales by 255/219 for both.
const BT601: [i32; 4] = [1634, 401, 833, 2066];
const BT709: [i32; 4] = [1836, 218, 546, 2163];
const LUMA: i32 = 1192;

fn rgb(matrix: [i32; 4], luma: u8, cb: u8, cr: u8) -> [u8; 3] {
    let base = (i32::from(luma) - 16) * LUMA + 512;
    let blue = i32::from(cb) - 128;
    let red = i32::from(cr) - 128;
    let px = |sum: i32| (sum >> 10).clamp(0, 255) as u8;
    let [rv, gu, gv, bu] = matrix;
    [
        px(base + rv * red),
        px(base - gu * blue - gv * red),
        px(base + bu * blue),
    ]
}

/// Converts a raw frame of `width` by `height` into `out` as RGB24, replacing its contents.
///
/// # Errors
///
/// Fails when the size is odd where the layout needs it even, or `src` is too short.
pub fn to_rgb(
    pixels: Pixels,
    width: u32,
    height: u32,
    bt709: bool,
    src: &[u8],
    out: &mut Vec<u8>,
) -> Result<()> {
    let (w, h) = (width as usize, height as usize);
    let m = if bt709 { BT709 } else { BT601 };
    let short = || Error::Failed(format!("a {width}x{height} {pixels:?} frame came in short"));
    if w == 0 || h == 0 || !w.is_multiple_of(2) || (pixels == Pixels::Nv12 && !h.is_multiple_of(2))
    {
        return Err(short());
    }
    out.clear();
    out.resize(w * h * 3, 0);
    let rows = out.chunks_exact_mut(w * 3);
    match pixels {
        Pixels::Nv12 => {
            let luma = src.get(..w * h).ok_or_else(short)?;
            let chroma = src
                .get(w * h..w * h + w * h.div_euclid(2))
                .ok_or_else(short)?;
            for (row, (ys, o)) in luma.chunks_exact(w).zip(rows).enumerate() {
                let uv = chroma
                    .chunks_exact(w)
                    .nth(row.div_euclid(2))
                    .unwrap_or_default();
                let pairs = ys.as_chunks::<2>().0.iter().zip(uv.as_chunks::<2>().0);
                for ((&[y0, y1], &[u, v]), o) in pairs.zip(o.as_chunks_mut::<6>().0) {
                    let ([r0, g0, b0], [r1, g1, b1]) = (rgb(m, y0, u, v), rgb(m, y1, u, v));
                    *o = [r0, g0, b0, r1, g1, b1];
                }
            }
        }
        Pixels::Yuy2 => {
            let src = src.get(..w * h * 2).ok_or_else(short)?;
            for (ys, o) in src.chunks_exact(w * 2).zip(rows) {
                for (&[y0, u, y1, v], o) in ys.as_chunks::<4>().0.iter().zip(o.as_chunks_mut().0) {
                    let ([r0, g0, b0], [r1, g1, b1]) = (rgb(m, y0, u, v), rgb(m, y1, u, v));
                    *o = [r0, g0, b0, r1, g1, b1];
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &[u8], b: &[u8]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.abs_diff(*y) <= 1)
    }

    #[test]
    fn levels_and_primaries() {
        assert_eq!(rgb(BT601, 16, 128, 128), [0, 0, 0]);
        assert_eq!(rgb(BT601, 235, 128, 128), [255, 255, 255]);
        assert_eq!(rgb(BT709, 126, 128, 128), [128, 128, 128]);
        // Full red, green and blue in each standard's limited-range coding.
        assert!(close(&rgb(BT601, 81, 90, 240), &[255, 0, 0]));
        assert!(close(&rgb(BT601, 145, 54, 34), &[0, 255, 0]));
        assert!(close(&rgb(BT601, 41, 240, 110), &[0, 0, 255]));
        assert!(close(&rgb(BT709, 63, 102, 240), &[255, 0, 0]));
        assert!(close(&rgb(BT709, 173, 42, 26), &[0, 255, 0]));
        assert!(close(&rgb(BT709, 32, 240, 118), &[0, 0, 255]));
    }

    #[test]
    fn nv12_shares_chroma_across_two_rows_and_two_columns() {
        // A 4x2 frame: the left pair is red, the right pair is blue.
        let mut src = vec![81, 81, 41, 41, 81, 81, 41, 41];
        src.extend([90, 240, 240, 110]);
        let mut out = Vec::new();
        to_rgb(Pixels::Nv12, 4, 2, false, &src, &mut out).unwrap();
        let red_blue = [255, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255];
        assert!(close(&out, &[red_blue, red_blue].concat()), "{out:?}");
    }

    #[test]
    fn yuy2_pairs_share_chroma() {
        let src = [81, 90, 81, 240, 41, 240, 41, 110];
        let mut out = Vec::new();
        to_rgb(Pixels::Yuy2, 4, 1, false, &src, &mut out).unwrap();
        assert!(close(&out, &[255, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255]));
    }

    #[test]
    fn rejects_odd_sizes_and_short_frames() {
        let mut out = Vec::new();
        assert!(to_rgb(Pixels::Yuy2, 3, 1, false, &[0; 6], &mut out).is_err());
        assert!(to_rgb(Pixels::Nv12, 2, 3, false, &[0; 9], &mut out).is_err());
        assert!(to_rgb(Pixels::Nv12, 2, 2, false, &[0; 5], &mut out).is_err());
        assert!(to_rgb(Pixels::Yuy2, 2, 2, false, &[0; 7], &mut out).is_err());
        assert!(to_rgb(Pixels::Yuy2, 0, 2, false, &[], &mut out).is_err());
        let black = [16, 16, 16, 16, 128, 128];
        assert!(to_rgb(Pixels::Nv12, 2, 2, true, &black, &mut out).is_ok());
        assert_eq!(out, [0; 12]);
    }
}
