//! Hot and dead pixels. Each pixel is compared with the second brightest and second darkest of
//! its eight neighbours, so a stuck pixel that JPEG smeared into a pair still counts and a one
//! pixel line does not. A score per pixel rises and falls, so the map follows a warming sensor.
//!
//! Live view scales the sensor down about five times, so a stuck photosite can also land as a
//! blob up to 3 by 3, where every pixel has several bright neighbours. A blob is found by its
//! brightest pixel standing out from the 16 around its 3 by 3, again against the second
//! brightest, since a one pixel line crosses that ring twice.

use crate::face::Face;

/// On the sum of three channels.
const MARGIN: u16 = 60;
/// Stuck pixels don't come and go between frames, and `ON` and `OFF` count these scans.
const SCAN_EVERY: u32 = 3;
/// `ON` is about 3 seconds of standing out.
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
    cells: Vec<Cell>,
    /// Left, top, right and bottom of the eyes, where a catchlight looks just like a blob.
    eyes: Option<[usize; 4]>,
}

impl Repair {
    /// `luma_only` is for YCbCr, where only the first channel is brightness.
    pub(crate) fn apply(&mut self, img: &mut [u8], width: usize, height: usize, luma_only: bool) {
        if (width, height) != (self.width, self.height) {
            *self = Self {
                width,
                height,
                eyes: self.eyes,
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
            if fill_blob(img, width, i, luma_only) {
                continue;
            }
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

    pub(crate) const fn count(&self) -> usize {
        self.list.len()
    }

    pub(crate) fn avoid_eyes(&mut self, face: Option<Face>) {
        self.eyes = face.map(|f| {
            let at = |start: usize, n: usize, share: f32| start + (n as f32 * share) as usize;
            [
                at(f.x, f.w, 0.1),
                at(f.y, f.h, 0.2),
                at(f.x, f.w, 0.9),
                at(f.y, f.h, 0.6),
            ]
        });
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
        let mut out = std::mem::take(&mut self.cells);
        out.resize(inner, Cell::Plain);
        for y in 1..h - 1 {
            let row = |r: usize, dx: usize| {
                let start = r * w + dx;
                level.get(start..start + inner).unwrap_or_default()
            };
            // Counting neighbours needs no sort and runs on SIMD, which took the scan from 14 ms to
            // under 1. Zipping the rows avoids bounds checks.
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
                *flag = Cell::of(centre, [n0, n1, n2, n3, n4, n5, n6, n7]);
            }
            let eyes = self
                .eyes
                .filter(|e| (e[1]..e[3]).contains(&y))
                .map_or(0..0, |e| e[0]..e[2]);
            find_blobs(&mut out, level, (w, h), y, &eyes);
            let at = y * w + 1..y * w + 1 + inner;
            let (Some(score), Some(stuck)) =
                (self.score.get_mut(at.clone()), self.stuck.get_mut(at))
            else {
                continue;
            };
            for ((s, st), &cell) in score.iter_mut().zip(stuck.iter_mut()).zip(&out) {
                *s = if cell == Cell::Single {
                    s.saturating_add(3)
                } else {
                    s.saturating_sub(1)
                };
                let now = if *st { *s > OFF } else { *s >= ON };
                changed |= now != *st;
                *st = now;
            }
        }
        self.cells = out;
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

/// The 16 pixels around the 3 by 3 centred on `i`.
const fn ring(i: usize, w: usize) -> [usize; 16] {
    let (above, below) = (i - 2 * w, i + 2 * w);
    [
        above - 2,
        above - 1,
        above,
        above + 1,
        above + 2,
        below - 2,
        below - 1,
        below,
        below + 1,
        below + 2,
        i - w - 2,
        i - 2,
        i + w - 2,
        i - w + 2,
        i + 2,
        i + w + 2,
    ]
}

/// How a pixel compares with its eight neighbours. `Single` ends up meaning it stands out, alone,
/// as a pair, or as the middle of a blob.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cell {
    Plain,
    Single,
    Brightest,
    Darkest,
}

impl Cell {
    fn of(centre: u16, sides: [u16; 8]) -> Self {
        let (mut not_darker, mut not_brighter, mut above, mut below) = (0u8, 0u8, 0u8, 0u8);
        for side in sides {
            not_darker += u8::from(side + MARGIN > centre);
            not_brighter += u8::from(side < centre + MARGIN);
            above += u8::from(side > centre);
            below += u8::from(side < centre);
        }
        if not_darker <= 1 || not_brighter <= 1 {
            Self::Single
        } else if above == 0 {
            Self::Brightest
        } else if below == 0 {
            Self::Darkest
        } else {
            Self::Plain
        }
    }
}

/// Turns the brightest and darkest pixels of row `y` into `Single` where they're the middle of a
/// blob, and into `Plain` otherwise.
fn find_blobs(
    out: &mut [Cell],
    level: &[u16],
    (w, h): (usize, usize),
    y: usize,
    eyes: &std::ops::Range<usize>,
) {
    for (x, cell) in (1..).zip(out.iter_mut()) {
        let bright = match cell {
            Cell::Brightest => true,
            Cell::Darkest => false,
            _ => continue,
        };
        let blob = (2..h - 2).contains(&y)
            && (2..w - 2).contains(&x)
            && !eyes.contains(&x)
            && stands_out(level, w, y * w + x, bright);
        *cell = if blob { Cell::Single } else { Cell::Plain };
    }
}

/// Whether the brightest or darkest pixel of a 3 by 3 stands out from all but one of the ring
/// around it.
fn stands_out(level: &[u16], w: usize, i: usize, bright: bool) -> bool {
    let at = |j: usize| level.get(j).copied().unwrap_or_default();
    let centre = at(i);
    ring(i, w)
        .into_iter()
        .map(at)
        .filter(|&v| {
            if bright {
                v + MARGIN > centre
            } else {
                v < centre + MARGIN
            }
        })
        .nth(1)
        .is_none()
}

/// Fills the blob around a stuck `i` from the ring around it. False when fewer than two of its
/// neighbours belong with it, which the neighbour average handles better.
fn fill_blob(img: &mut [u8], w: usize, i: usize, luma_only: bool) -> bool {
    if i < 2 * w + 2 || i % w < 2 || i % w + 2 >= w {
        return false;
    }
    let level = |img: &[u8], j: usize| -> i32 {
        img.get(j * 3..j * 3 + 3).map_or(0, |p| {
            if luma_only {
                p.first().map_or(0, |&v| i32::from(v) * 3)
            } else {
                p.iter().map(|&v| i32::from(v)).sum()
            }
        })
    };
    let centre = level(img, i);
    let around = ring(i, w);
    let mut ring_levels = around.map(|j| level(img, j));
    ring_levels.sort_unstable();
    let middle = centre.midpoint(ring_levels.get(7).copied().unwrap_or(centre));
    let with_centre = |v: i32| (v > middle) == (centre > middle);
    let members: Vec<usize> = neighbours(i, w)
        .into_iter()
        .filter(|&j| with_centre(level(img, j)))
        .collect();
    if members.len() < 2 {
        return false;
    }
    let mut sum = [0u32; 3];
    let mut n = 0;
    for j in around {
        if !with_centre(level(img, j))
            && let Some(px) = img.get(j * 3..j * 3 + 3)
        {
            for (s, &v) in sum.iter_mut().zip(px) {
                *s += u32::from(v);
            }
            n += 1;
        }
    }
    if n == 0 {
        return false;
    }
    let fill = sum.map(|s| s.div_euclid(n) as u8);
    for j in members.into_iter().chain([i]) {
        if let Some(px) = img.get_mut(j * 3..j * 3 + 3) {
            px.copy_from_slice(&fill);
        }
    }
    true
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

    /// The shape of a stuck photosite on the T3 after live view scales it down and JPEG smears it.
    const BLOB: [(usize, usize, u8); 6] = [
        (30, 20, 165),
        (31, 20, 175),
        (32, 20, 155),
        (30, 21, 158),
        (31, 21, 168),
        (32, 21, 150),
    ];

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
            for (x, y, v) in BLOB {
                put(&mut img, x, y, [v; 3]);
            }
            repair.apply(&mut img, W, H, false);
        }
        assert_eq!(repair.count(), 5);
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
        for (x, y, _) in BLOB {
            let (p, outside) = (first(&img, x, y), first(&img, 28, y));
            assert!(
                p.abs_diff(outside) < 15,
                "blob {x},{y} is {p}, beside it {outside}"
            );
        }
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
