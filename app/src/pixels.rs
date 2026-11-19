//! Finds hot and dead pixels by how they behave over time and fills them from their neighbours.
//!
//! A stuck pixel stays brighter (or darker) than its surroundings whatever the scene does,
//! while real detail moves or belongs to a line or edge. Each pixel is compared with the
//! second-brightest and second-darkest of its eight neighbours, so a stuck pixel that JPEG
//! smeared into a pair still counts and a one-pixel line does not. A score per pixel rises
//! while it stands out and falls while it doesn't, so the map follows the sensor as it warms
//! up and grows more hot pixels during a call.

/// Brightness gap to the second-nearest neighbour, on the sum of three channels.
const MARGIN: u16 = 60;
/// Stuck pixels don't come and go between frames, and `ON` and `OFF` are counted in these scans.
const SCAN_EVERY: u32 = 3;
/// A pixel is filled once its score reaches `ON`, about 3 seconds of standing out, and
/// left alone again once it falls to `OFF`.
const ON: u8 = 90;
const OFF: u8 = 30;

#[derive(Default)]
pub(crate) struct Repair {
    width: usize,
    height: usize,
    frames: u32,
    score: Vec<u8>,
    stuck: Vec<bool>,
    list: Vec<usize>,
    brightness: Vec<u16>,
}

impl Repair {
    /// Repairs `img`, 3 interleaved bytes a pixel, in place. `luma_only` is for YCbCr, where
    /// only the first channel says how bright a pixel is.
    pub(crate) fn apply(&mut self, img: &mut [u8], width: usize, height: usize, luma_only: bool) {
        if (width, height) != (self.width, self.height) {
            *self = Self {
                width,
                height,
                ..Self::default()
            };
            self.score = vec![0; width * height];
            self.stuck = vec![false; width * height];
        }
        if width < 3 || height < 3 {
            return;
        }
        self.frames += 1;
        if self.frames.is_multiple_of(SCAN_EVERY) {
            self.scan(img, luma_only);
        }
        for &i in &self.list {
            let mut sum = [0u32; 3];
            let mut n = 0;
            for j in neighbours(i, width) {
                if self.stuck.get(j) == Some(&false)
                    && let Some(px) = img.get(j * 3..j * 3 + 3)
                {
                    for (s, &v) in sum.iter_mut().zip(px) {
                        *s += u32::from(v);
                    }
                    n += 1;
                }
            }
            if let Some(px) = img.get_mut(i * 3..i * 3 + 3) {
                for (p, s) in px.iter_mut().zip(sum) {
                    if let Some(v) = s.checked_div(n) {
                        *p = v as u8;
                    }
                }
            }
        }
    }

    /// How many pixels are being filled right now.
    pub(crate) const fn count(&self) -> usize {
        self.list.len()
    }

    fn scan(&mut self, img: &[u8], luma_only: bool) {
        let (w, h) = (self.width, self.height);
        self.brightness.clear();
        self.brightness
            .extend(img.as_chunks::<3>().0.iter().map(|p| {
                if luma_only {
                    u16::from(p[0]) * 3
                } else {
                    u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2])
                }
            }));
        let level = &self.brightness;
        let mut changed = false;
        let inner = w - 2;
        let mut out = vec![false; inner];
        for y in 1..h - 1 {
            let row = |r: usize, dx: usize| {
                let start = r * w + dx;
                level.get(start..start + inner).unwrap_or_default()
            };
            // Standing out from the second-brightest neighbour means at most one neighbour comes
            // within the margin, which counts without sorting and runs on SIMD. The scan went
            // from 14 ms to under 1. Zipping every row keeps the loop free of bounds checks.
            let cells = row(y, 1)
                .iter()
                .zip(row(y - 1, 0))
                .zip(row(y - 1, 1))
                .zip(row(y - 1, 2))
                .zip(row(y, 0))
                .zip(row(y, 2))
                .zip(row(y + 1, 0))
                .zip(row(y + 1, 1))
                .zip(row(y + 1, 2));
            for (flag, ((((((((&centre, &n0), &n1), &n2), &n3), &n4), &n5), &n6), &n7)) in
                out.iter_mut().zip(cells)
            {
                let (mut not_darker, mut not_brighter) = (0u8, 0u8);
                for side in [n0, n1, n2, n3, n4, n5, n6, n7] {
                    not_darker += u8::from(side + MARGIN > centre);
                    not_brighter += u8::from(side < centre + MARGIN);
                }
                *flag = not_darker <= 1 || not_brighter <= 1;
            }
            let at = y * w + 1..y * w + 1 + inner;
            let (Some(score), Some(stuck)) =
                (self.score.get_mut(at.clone()), self.stuck.get_mut(at))
            else {
                continue;
            };
            for ((s, st), &o) in score.iter_mut().zip(stuck.iter_mut()).zip(&out) {
                *s = if o {
                    s.saturating_add(3)
                } else {
                    s.saturating_sub(1)
                };
                let now = if *st { *s > OFF } else { *s >= ON };
                changed |= now != *st;
                *st = now;
            }
        }
        if changed {
            self.list.clear();
            self.list.extend(
                self.stuck
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| **s)
                    .map(|(i, _)| i),
            );
        }
    }
}

const fn neighbours(i: usize, w: usize) -> [usize; 8] {
    [
        i - w - 1,
        i - w,
        i - w + 1,
        i - 1,
        i + 1,
        i + w - 1,
        i + w,
        i + w + 1,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 64;
    const H: usize = 48;

    fn put(img: &mut [u8], x: usize, y: usize, px: [u8; 3]) {
        let i = (y * W + x) * 3;
        img.get_mut(i..i + 3).unwrap().copy_from_slice(&px);
    }

    fn first(img: &[u8], x: usize, y: usize) -> u8 {
        img.get((y * W + x) * 3).copied().unwrap()
    }

    /// A gradient with a one-pixel bright line down column 40, plus flickering grain so the
    /// scene is never perfectly still.
    fn scene(t: usize) -> Vec<u8> {
        let mut img = Vec::with_capacity(W * H * 3);
        for y in 0..H {
            for x in 0..W {
                let grain = ((x * 7 + y * 13 + t * 31) % 9) as u8;
                let v = if x == 40 {
                    230
                } else {
                    60 + (x + y) as u8 + grain
                };
                img.extend([v, v, v]);
            }
        }
        img
    }

    #[test]
    fn fills_hot_and_dead_pixels_and_leaves_lines_alone() {
        let mut repair = Repair::default();
        let mut img = Vec::new();
        for t in 0..200 {
            img = scene(t);
            put(&mut img, 10, 10, [250, 40, 40]);
            for x in [20, 21] {
                put(&mut img, x, 30, [255, 255, 255]);
            }
            put(&mut img, 50, 15, [0, 0, 0]);
            repair.apply(&mut img, W, H, false);
        }
        assert_eq!(repair.count(), 4);
        let near = |x: usize, y: usize| {
            let p = first(&img, x, y);
            let around = first(&img, x - 1, y);
            assert!(
                p.abs_diff(around) < 15,
                "pixel {x},{y} is {p}, its neighbour {around}"
            );
        };
        near(10, 10);
        near(20, 30);
        near(21, 30);
        near(50, 15);
        assert_eq!(first(&img, 40, 24), 230, "the one-pixel line was filled");
    }

    #[test]
    fn needs_a_few_seconds_before_filling_anything() {
        let mut repair = Repair::default();
        for t in 0..60 {
            let mut img = scene(t);
            *img.get_mut((10 * W + 10) * 3).unwrap() = 255;
            repair.apply(&mut img, W, H, false);
        }
        assert_eq!(repair.count(), 0);
    }
}
