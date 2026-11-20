//! The Adjust sliders, applied to the RGB frame in one pass with white balance, and Auto,
//! which picks values for them from one frame.
//!
//! The tone sliders make one curve folded into each channel's lookup table. HDR and Clarity
//! work on a blurred luma made from a 16th of the pixels, so tones even out while detail stays.

use std::fmt::Write as _;

use crate::face::Face;
use crate::store;

/// All at zero leaves the picture as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Adjust {
    /// Stops brighter or darker.
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub hdr: f32,
    pub clarity: f32,
    pub fade: f32,
    pub saturation: f32,
    pub vibrance: f32,
    /// Below zero darkens the corners, above lightens them.
    pub vignette: f32,
}

/// Each slider's name, hover text and range, in the order `Adjust::sliders` gives them.
pub(crate) const SLIDERS: [(&str, &str, f32, f32); 12] = [
    (
        "Exposure",
        "Brightens or darkens the whole picture",
        -2.0,
        2.0,
    ),
    (
        "Contrast",
        "Pushes darks and lights apart, or draws them together",
        -1.0,
        1.0,
    ),
    (
        "Highlights",
        "Brightens or tames the bright areas",
        -1.0,
        1.0,
    ),
    ("Shadows", "Lifts or deepens the dark areas", -1.0, 1.0),
    (
        "Whites",
        "Sets how bright the brightest parts get",
        -1.0,
        1.0,
    ),
    ("Blacks", "Sets how dark the darkest parts get", -1.0, 1.0),
    (
        "HDR",
        "Evens out bright and dark areas and keeps the detail in both",
        0.0,
        1.0,
    ),
    ("Clarity", "Adds punch to texture, or softens it", -1.0, 1.0),
    ("Fade", "Lifts the blacks for a soft, matte look", 0.0, 1.0),
    (
        "Saturation",
        "Makes every colour stronger or weaker",
        -1.0,
        1.0,
    ),
    (
        "Vibrance",
        "Strengthens dull colours more than strong ones and goes easy on skin",
        -1.0,
        1.0,
    ),
    ("Vignette", "Darkens or lightens the corners", -1.0, 1.0),
];

impl Adjust {
    pub(crate) const fn sliders(&mut self) -> [&mut f32; 12] {
        [
            &mut self.exposure,
            &mut self.contrast,
            &mut self.highlights,
            &mut self.shadows,
            &mut self.whites,
            &mut self.blacks,
            &mut self.hdr,
            &mut self.clarity,
            &mut self.fade,
            &mut self.saturation,
            &mut self.vibrance,
            &mut self.vignette,
        ]
    }

    pub(crate) fn is_neutral(&self) -> bool {
        *self == Self::default()
    }

    /// The tone curve on a 0 to 1 scale, before HDR and Clarity.
    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add would change rounding and is a software call without FMA"
    )]
    fn tone(&self, x: f32) -> f32 {
        let mut y = exposed(x, self.exposure);
        y += self.contrast * (y * y * 2.0f32.mul_add(-y, 3.0) - y);
        y += 0.25
            * (self.shadows * deep(y)
                + self.highlights * bright(y)
                + self.blacks * (1.0 - y).powi(3)
                + self.whites * y.powi(3));
        (0.1 * self.fade).mul_add(1.0 - y, y)
    }

    /// The tone curve for each 8-bit code, never falling and between 0 and 1.
    pub(crate) fn curve(&self) -> [f32; 256] {
        let mut top = 0.0f32;
        std::array::from_fn(|c| {
            top = top.max(self.tone(c as f32 / 255.0).clamp(0.0, 1.0));
            top
        })
    }
}

/// Exposure in stops, with a shoulder above so a brighter picture rolls into white instead of
/// clipping at a hard edge.
fn exposed(x: f32, stops: f32) -> f32 {
    let gain = (stops / 2.2).exp2();
    let knee = 0.5 / gain;
    if gain <= 1.0 || x <= knee {
        return x * gain;
    }
    // A cubic from the knee, where it carries on the straight line, to white.
    let span = 1.0 - knee;
    let t = (x - knee) / span;
    // Hermite's weights on the end point and on the slopes at each end.
    let end = t * t * 2.0f32.mul_add(-t, 3.0);
    let slopes = (span / gain).mul_add(t * t * (t - 1.0), span * gain * t * (1.0 - t).powi(2));
    0.5f32.mul_add(end, 0.5) + slopes
}

/// Weights that are 1 at their peak and 0 at black and white. `deep` peaks at a quarter and
/// `bright` at three quarters, and each fades out well before the far end.
fn deep(x: f32) -> f32 {
    256.0 / 27.0 * x * (1.0 - x).powi(3)
}

fn bright(x: f32) -> f32 {
    256.0 / 27.0 * x.powi(3) * (1.0 - x)
}

/// The blurred luma is averaged over blocks this many pixels square.
const BLOCK: usize = 16;
/// Every this many pixels across and down is sampled for the blurred luma.
const SAMPLE: usize = 4;

/// What one frame's pass works from. Only the tone tables are looked up per pixel. The rest
/// are gains worked out in the loop, in 8.8 fixed point, so the compiler can run it on several
/// pixels at once.
struct Tables {
    tone: [[u8; 256]; 3],
    /// What HDR adds to a pixel, by the blurred luma around it.
    hdr: [i32; 256],
    /// Clarity's gain on a pixel's difference from the blurred luma, scaled so that times the
    /// pixel's luma and its distance from white, shifted down 16, it peaks at mid grey.
    clarity: i32,
    saturation: i32,
    /// Vibrance at its strongest, on the dullest colours, and on skin.
    vibrance: i32,
    skin: i32,
    /// The change in level at the corners.
    vignette: i32,
}

impl Tables {
    fn new(adjust: &Adjust, balance: &[[u8; 256]; 3]) -> Self {
        let curve = adjust.curve();
        let fixed = |v: f32| (v * 256.0).round() as i32;
        let clarity = adjust.clarity * if adjust.clarity > 0.0 { 0.9 } else { 0.5 };
        Self {
            tone: balance.map(|table| {
                table.map(|c| {
                    let y = curve.get(usize::from(c)).copied().unwrap_or_default();
                    (y * 255.0).round() as u8
                })
            }),
            hdr: std::array::from_fn(|c| {
                let x = c as f32 / 255.0;
                (255.0 * adjust.hdr * 0.4f32.mul_add(deep(x), -0.3 * bright(x))).round() as i32
            }),
            clarity: fixed(clarity * 4.0 * 65536.0 / 65025.0),
            saturation: fixed(1.0 + adjust.saturation),
            vibrance: fixed(adjust.vibrance),
            skin: fixed(adjust.vibrance * 0.3),
            vignette: fixed(adjust.vignette * 0.5),
        }
    }
}

/// Buffers kept between frames.
#[derive(Default)]
pub(crate) struct Pass {
    sums: Vec<i32>,
    counts: Vec<i32>,
    /// The blurred luma, a value per block, and a spare for blurring.
    map: Vec<i32>,
    spare: Vec<i32>,
    /// For each pixel across and down, the map columns or rows either side and the second
    /// one's weight.
    across: Vec<(usize, usize, i32)>,
    down: Vec<(usize, usize, i32)>,
    /// The blurred luma and what HDR adds, lerped across to every pixel of each map row and
    /// scaled by 256, so each pixel row only lerps between two of them.
    wide_base: Vec<i32>,
    wide_lift: Vec<i32>,
    /// The current row's toned channels, one plane each.
    red: Vec<i32>,
    green: Vec<i32>,
    blue: Vec<i32>,
    /// Squared distance from the middle across and down, each 0 to 511.
    vx: Vec<i32>,
    vy: Vec<i32>,
}

impl Pass {
    /// Applies `balance` then `adjust` to a frame of RGB `pixels`, `width` wide. `window` is
    /// the left, top, width and height of the part apps see, which the vignette centres on.
    #[expect(unsafe_code, reason = "calling the AVX2 build of the loop")]
    pub(crate) fn apply(
        &mut self,
        pixels: &mut [u8],
        width: usize,
        window: [usize; 4],
        balance: &[[u8; 256]; 3],
        adjust: &Adjust,
    ) {
        let height = pixels.len().checked_div(width * 3).unwrap_or_default();
        if height == 0 {
            return;
        }
        let t = Tables::new(adjust, balance);
        for v in [&mut self.red, &mut self.green, &mut self.blue] {
            v.clear();
            v.resize(width, 0);
        }
        if adjust.hdr != 0.0 || adjust.clarity != 0.0 {
            self.measure(pixels, width, height, &t);
        } else {
            self.down.clear();
            for v in [&mut self.wide_base, &mut self.wide_lift] {
                v.clear();
                v.resize(width, 0);
            }
        }
        let [left, top, w, h] = window;
        let reach = |at: usize, from: usize, across: usize| {
            let n = (at as f32 + 0.5 - from as f32) / (across as f32 / 2.0) - 1.0;
            (n * n * 511.0).min(511.0) as i32
        };
        self.vx.clear();
        self.vx.extend((0..width).map(|x| reach(x, left, w)));
        self.vy.clear();
        self.vy.extend((0..height).map(|y| reach(y, top, h)));
        // The build targets every x86-64, whose SSE2 has no 32-bit multiply or min and max
        // across lanes, so the loop only runs several pixels at once with AVX2.
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU has AVX2, checked just above.
            unsafe { self.rows_avx2(pixels, width, &t) }
        } else {
            self.rows(pixels, width, &t);
        }
    }

    #[target_feature(enable = "avx2")]
    fn rows_avx2(&mut self, pixels: &mut [u8], width: usize, t: &Tables) {
        self.rows(pixels, width, t);
    }

    /// Each row is toned into the planes, taken through HDR, Clarity, Saturation, Vibrance
    /// and Vignette there with nothing looked up, then put back.
    #[expect(
        clippy::inline_always,
        reason = "inlined into the AVX2 build, which only then compiles it for AVX2"
    )]
    #[inline(always)]
    fn rows(&mut self, pixels: &mut [u8], width: usize, t: &Tables) {
        for (y, row) in pixels.chunks_exact_mut(width * 3).enumerate() {
            let row = row.as_chunks_mut::<3>().0;
            let planes = self
                .red
                .iter_mut()
                .zip(self.green.iter_mut())
                .zip(self.blue.iter_mut());
            for (px, ((r, g), b)) in row.iter().zip(planes) {
                [*r, *g, *b] = toned(&t.tone, *px);
            }
            let (above, below, wy) = self.down.get(y).copied().unwrap_or_default();
            let vy = self.vy.get(y).copied().unwrap_or_default();
            let bases =
                line(&self.wide_base, above, width)
                    .iter()
                    .zip(line(&self.wide_base, below, width));
            let lifts =
                line(&self.wide_lift, above, width)
                    .iter()
                    .zip(line(&self.wide_lift, below, width));
            let planes = self
                .red
                .iter_mut()
                .zip(self.green.iter_mut())
                .zip(self.blue.iter_mut())
                .zip(&self.vx);
            for ((((r, g), b), &vx), ((&b0, &b1), (&l0, &l1))) in planes.zip(bases.zip(lifts)) {
                let rgb = [*r, *g, *b];
                let y = luma(rgb);
                let base = (b0 * (256 - wy) + b1 * wy) >> 16;
                let lift = (l0 * (256 - wy) + l1 * wy) >> 16;
                let clarity = (t.clarity * y * (255 - y)) >> 16;
                let level = (y + lift + (((y - base) * clarity) >> 8)) << 8;
                let [red, green, blue] = rgb;
                let dull = 255 - (red.max(green).max(blue) - red.min(green).min(blue));
                let vibrance = if skin(rgb) { t.skin } else { t.vibrance };
                let gain = ((t.saturation * (256 + ((vibrance * dull * dull) >> 16))) >> 8).max(0);
                let corner = vx + vy;
                let shade = 256 + ((t.vignette * ((corner * corner) >> 10)) >> 10);
                let out = |c: i32| ((((c - y) * gain + level) * shade) >> 16).clamp(0, 255);
                (*r, *g, *b) = (out(red), out(green), out(blue));
            }
            let planes = self.red.iter().zip(&self.green).zip(&self.blue);
            for (px, ((&r, &g), &b)) in row.iter_mut().zip(planes) {
                *px = [r as u8, g as u8, b as u8];
            }
        }
    }

    /// Averages the toned luma over each block, blurs the blocks, and lerps them across.
    fn measure(&mut self, pixels: &[u8], width: usize, height: usize, t: &Tables) {
        let (mw, mh) = (width.div_ceil(BLOCK), height.div_ceil(BLOCK));
        for v in [&mut self.sums, &mut self.counts] {
            v.clear();
            v.resize(mw * mh, 0);
        }
        for (y, row) in pixels.chunks_exact(width * 3).enumerate().step_by(SAMPLE) {
            let line = y.div_euclid(BLOCK) * mw;
            for (x, px) in row.as_chunks::<3>().0.iter().enumerate().step_by(SAMPLE) {
                let i = line + x.div_euclid(BLOCK);
                if let (Some(s), Some(n)) = (self.sums.get_mut(i), self.counts.get_mut(i)) {
                    *s += luma(toned(&t.tone, *px));
                    *n += 1;
                }
            }
        }
        self.map.clear();
        self.map.extend(
            self.sums
                .iter()
                .zip(&self.counts)
                .map(|(&s, &n)| s.checked_div(n).unwrap_or_default()),
        );
        // Two passes of a 3 by 3 box come close to a Gaussian.
        for _ in 0..2 {
            box_blur(&self.map, &mut self.spare, mw, mh);
            std::mem::swap(&mut self.map, &mut self.spare);
        }
        let to_map = |x: usize, blocks: usize| {
            let at = ((x as f32 + 0.5) / BLOCK as f32 - 0.5).clamp(0.0, (blocks - 1) as f32);
            let first = at.floor() as usize;
            (
                first,
                (first + 1).min(blocks - 1),
                ((at - first as f32) * 256.0) as i32,
            )
        };
        self.across.clear();
        self.across.extend((0..width).map(|x| to_map(x, mw)));
        self.down.clear();
        self.down.extend((0..height).map(|y| to_map(y, mh)));
        self.wide_base.clear();
        self.wide_lift.clear();
        for i in 0..mh {
            let cell = |j: usize| self.map.get(i * mw + j).copied().unwrap_or_default();
            for &(j0, j1, wx) in &self.across {
                let (a, b) = (cell(j0), cell(j1));
                self.wide_base.push(a * (256 - wx) + b * wx);
                self.wide_lift
                    .push(at(&t.hdr, a as u8) * (256 - wx) + at(&t.hdr, b as u8) * wx);
            }
        }
    }
}

/// Row `i` of a grid `width` wide, or nothing past its end.
fn line(grid: &[i32], i: usize, width: usize) -> &[i32] {
    grid.get(i * width..(i + 1) * width).unwrap_or_default()
}

/// A 3 by 3 box blur of a `w` by `h` grid, the edges repeated outward.
fn box_blur(src: &[i32], dst: &mut Vec<i32>, w: usize, h: usize) {
    let at = |x: usize, y: usize| {
        src.get(y.min(h - 1) * w + x.min(w - 1))
            .copied()
            .unwrap_or_default()
    };
    dst.clear();
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0;
            for dy in 0..3 {
                for dx in 0..3 {
                    sum += at((x + dx).saturating_sub(1), (y + dy).saturating_sub(1));
                }
            }
            dst.push((sum * 57) >> 9);
        }
    }
}

fn at<T: Copy + Default>(table: &[T; 256], i: u8) -> T {
    table.get(usize::from(i)).copied().unwrap_or_default()
}

fn toned(tone: &[[u8; 256]; 3], [r, g, b]: [u8; 3]) -> [i32; 3] {
    let [tr, tg, tb] = tone;
    [at(tr, r), at(tg, g), at(tb, b)].map(i32::from)
}

/// Rec. 709 luma, as `exposure::brightness` weighs it.
const fn luma([r, g, b]: [i32; 3]) -> i32 {
    (54 * r + 183 * g + 19 * b) >> 8
}

/// Orange between red and yellow, where every skin tone falls. Written without short
/// circuits so it stays a mask the compiler can run on several pixels at once.
#[expect(
    clippy::needless_bitwise_bool,
    reason = "each test is one comparison, so none is worth a branch"
)]
const fn skin([r, g, b]: [i32; 3]) -> bool {
    (r > g) & (g > b) & (6 * (g - b) > r - b) & (4 * (g - b) < 3 * (r - b))
}

struct Seen {
    /// Luma of the part apps see, with the face counted as at least 30% of it, or with no
    /// face the middle counted twice.
    luma: [f32; 256],
    face: Option<[f32; 256]>,
    /// How strong the colour is on average, 0 to 1, leaving out the darkest and brightest.
    colour: f32,
}

/// Where a face's skin lands best, as a share of white. Darker skin sits nearer the bottom.
const SKIN: (f32, f32) = (0.42, 0.62);

/// Picks tone and colour sliders for one frame, as an editor's Auto Tone does. Vignette,
/// Clarity and Fade are a matter of taste and are left at zero.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding and is a software call without FMA"
)]
pub(crate) fn auto(
    pixels: &[u8],
    width: usize,
    window: [usize; 4],
    balance: &[[u8; 256]; 3],
    face: Option<Face>,
) -> Adjust {
    let seen = Seen::read(pixels, width, window, balance, face);
    let mut a = Adjust {
        exposure: seen.exposure(),
        ..Adjust::default()
    };
    let lit = through(&seen.luma, &a.curve());
    let (floor, ceiling) = (percentile(&lit, 0.005), percentile(&lit, 0.995));
    let (crushed, clipped) = (share(&lit, 0.0, 0.012), share(&lit, 0.98, 1.0));
    a.whites = if clipped > 0.005 {
        -(clipped * 15.0).min(0.6)
    } else {
        ((0.95 - ceiling) / (0.25 * ceiling.max(0.3).powi(3))).clamp(0.0, 0.8)
    };
    a.blacks = if crushed > 0.01 {
        (crushed * 10.0).min(0.5)
    } else {
        -((floor - 0.03) * 4.0).clamp(0.0, 0.6)
    };
    a.highlights = -(share(&lit, 0.8, 1.0) * 2.0).min(0.8);
    a.shadows = (share(&lit, 0.0, 0.2) * 2.0).min(0.8);
    let set = through(&seen.luma, &a.curve());
    // The middle 80% rather than 50%, so a frame that's mostly one wall still has a spread.
    let spread = percentile(&set, 0.9) - percentile(&set, 0.1);
    a.contrast = ((0.6 - spread) * 2.0).clamp(-0.4, 0.8);
    let toned = through(&seen.luma, &a.curve());
    let backlit = seen
        .face
        .as_ref()
        .is_some_and(|f| percentile(&lit, 0.9) - percentile(&through(f, &a.curve()), 0.5) > 0.4);
    a.hdr = (4.0 * share(&toned, 0.0, 0.15).min(share(&toned, 0.85, 1.0))
        + if backlit { 0.3 } else { 0.0 })
    .min(0.8);
    if let Some(f) = &seen.face {
        let skin = percentile(&through(f, &a.curve()), 0.5);
        a.shadows = (a.shadows + (SKIN.0 - skin).max(0.0) * 4.0).min(1.0);
        a.highlights = (a.highlights - (skin - SKIN.1).max(0.0) * 4.0).max(-1.0);
    }
    // Contrast strengthens colour on its own.
    let colour = seen.colour * 0.3f32.mul_add(a.contrast, 1.0);
    a.vibrance = ((0.22 - colour) * 3.0).clamp(0.0, 0.6);
    a.saturation = if colour > 0.32 {
        -((colour - 0.32) * 2.0).min(0.4)
    } else {
        (0.12 - colour).max(0.0)
    };
    for v in a.sliders() {
        // Adding zero turns a rounded -0 into 0.
        *v = (*v * 100.0).round() / 100.0 + 0.0;
    }
    a
}

impl Seen {
    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add would change rounding and is a software call without FMA"
    )]
    fn read(
        pixels: &[u8],
        width: usize,
        [left, top, w, h]: [usize; 4],
        balance: &[[u8; 256]; 3],
        face: Option<Face>,
    ) -> Self {
        // The middle of the face's box, which leaves out hair and what's behind.
        let skin = face.map(|f| {
            let (dx, dy) = (f.w.div_euclid(5), f.h.div_euclid(5));
            (f.x + dx..f.x + f.w - dx, f.y + dy..f.y + f.h - dy)
        });
        // Without a face, whoever is on camera is most likely in the middle half.
        let (qw, qh) = (w.div_euclid(4), h.div_euclid(4));
        let middle = (left + qw..left + w - qw, top + qh..top + h - qh);
        let (mut scene, mut faced) = ([0.0f32; 256], [0.0f32; 256]);
        let (mut colour, mut counted) = (0.0f32, 0.0f32);
        for (y, row) in pixels
            .chunks_exact(width * 3)
            .enumerate()
            .skip(top)
            .take(h)
            .step_by(2)
        {
            for (x, &px) in row
                .as_chunks::<3>()
                .0
                .iter()
                .enumerate()
                .skip(left)
                .take(w)
                .step_by(2)
            {
                let rgb = toned(balance, px);
                let level = luma(rgb).clamp(0, 255) as usize;
                let on_face = skin
                    .as_ref()
                    .is_some_and(|(xs, ys)| xs.contains(&x) && ys.contains(&y));
                let weight = if skin.is_none() && middle.0.contains(&x) && middle.1.contains(&y) {
                    2.0
                } else {
                    1.0
                };
                if let Some(bin) = if on_face { &mut faced } else { &mut scene }.get_mut(level) {
                    *bin += weight;
                }
                if (26..=230).contains(&level) {
                    let [red, green, blue] = rgb;
                    let strength = red.max(green).max(blue) - red.min(green).min(blue);
                    colour += strength as f32 / 255.0;
                    counted += 1.0;
                }
            }
        }
        let (n_scene, n_face) = (scene.iter().sum::<f32>(), faced.iter().sum::<f32>());
        let weight = (0.3 / 0.7 * n_scene / n_face.max(1.0)).max(1.0);
        Self {
            luma: std::array::from_fn(|i| {
                scene.get(i).copied().unwrap_or_default()
                    + faced.get(i).copied().unwrap_or_default() * weight
            }),
            face: (n_face > 0.0).then_some(faced),
            colour: colour / counted.max(1.0),
        }
    }

    /// Stops toward bringing the frame's log average to steady exposure's target, held to
    /// where the face's skin stays in `SKIN`. The room only counts for 70%, since a white wall
    /// should stay a little bright and steady exposure has already metered it once.
    fn exposure(&self) -> f32 {
        let total = self.luma.iter().sum::<f32>().max(1.0);
        let average: f32 = self
            .luma
            .iter()
            .zip(crate::exposure::LINEAR.iter())
            .map(|(n, l)| n * l)
            .sum::<f32>()
            / total;
        let mut stops = 0.7 * (crate::exposure::TARGET - average);
        if let Some(f) = &self.face {
            let skin = percentile(f, 0.5).max(0.02);
            let to = |level: f32| 2.2 * (level / skin).log2();
            stops = stops.clamp(to(SKIN.0), to(SKIN.1));
        }
        stops.clamp(-2.0, 2.0)
    }
}

fn through(hist: &[f32; 256], curve: &[f32; 256]) -> [f32; 256] {
    let mut out = [0.0; 256];
    for (&n, &y) in hist.iter().zip(curve) {
        if let Some(bin) = out.get_mut((y * 255.0).round() as usize) {
            *bin += n;
        }
    }
    out
}

/// The level, 0 to 1, that `q` of the histogram lies below.
fn percentile(hist: &[f32; 256], q: f32) -> f32 {
    let want = hist.iter().sum::<f32>() * q;
    let mut below = 0.0;
    for (i, &n) in hist.iter().enumerate() {
        below += n;
        if below > want {
            return i as f32 / 255.0;
        }
    }
    1.0
}

/// The share of the histogram from level `from` to `to`, 0 to 1 and both included.
fn share(hist: &[f32; 256], from: f32, to: f32) -> f32 {
    let total = hist.iter().sum::<f32>().max(1.0);
    let range = (from * 255.0).round() as usize..=(to * 255.0).round() as usize;
    hist.iter()
        .enumerate()
        .filter(|(i, _)| range.contains(i))
        .map(|(_, n)| n)
        .sum::<f32>()
        / total
}

pub(crate) fn load() -> Adjust {
    let mut adjust = Adjust::default();
    let text = store::read("adjust.txt").unwrap_or_default();
    for (key, value) in store::numbers(&text) {
        for (&(name, _, lo, hi), slot) in SLIDERS.iter().zip(adjust.sliders()) {
            if key.eq_ignore_ascii_case(name) {
                *slot = value.clamp(lo, hi);
            }
        }
    }
    adjust
}

pub(crate) fn save(mut adjust: Adjust) {
    let mut text = String::new();
    for (&(name, ..), v) in SLIDERS.iter().zip(adjust.sliders()) {
        writeln!(text, "{}={v}", name.to_ascii_lowercase()).ok();
    }
    store::write("adjust.txt", &text);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    const W: usize = 1056;
    const H: usize = 704;
    const WHOLE: [usize; 4] = [0, 0, W, H];
    /// The pixel in the middle of the frame.
    const MIDDLE: usize = H.div_euclid(2) * W + W.div_euclid(2);

    fn plain() -> [[u8; 256]; 3] {
        [std::array::from_fn(|c| c as u8); 3]
    }

    /// Soft gradients, fine texture and a range of colours, like a room.
    fn room() -> Vec<u8> {
        (0..W * H)
            .flat_map(|i| {
                let (x, y) = (i.rem_euclid(W), i.div_euclid(W));
                let grain = ((x * 7 + y * 13).rem_euclid(17) * 3) as u8;
                [
                    (x * 200).div_euclid(W) as u8 + grain,
                    (y * 200).div_euclid(H) as u8 + grain,
                    ((x + y) * 120).div_euclid(W + H) as u8 + 40,
                ]
            })
            .collect()
    }

    fn adjusted(adjust: Adjust, frame: &[u8]) -> Vec<u8> {
        let mut out = frame.to_vec();
        Pass::default().apply(&mut out, W, WHOLE, &plain(), &adjust);
        out
    }

    /// A frame of one colour put through `adjust`, read back in the middle.
    fn one(adjust: Adjust, rgb: [u8; 3]) -> [u8; 3] {
        let mut frame = rgb.repeat(64 * 64);
        Pass::default().apply(&mut frame, 64, [0, 0, 64, 64], &plain(), &adjust);
        pixel(&frame, 32 * 64 + 32)
    }

    fn pixel(frame: &[u8], i: usize) -> [u8; 3] {
        frame.as_chunks::<3>().0.get(i).copied().unwrap()
    }

    fn grey(adjust: Adjust, level: u8) -> u8 {
        one(adjust, [level; 3])[1]
    }

    fn chroma([r, g, b]: [u8; 3]) -> u8 {
        r.max(g).max(b) - r.min(g).min(b)
    }

    fn with(set: fn(&mut Adjust, f32), v: f32) -> Adjust {
        let mut a = Adjust::default();
        set(&mut a, v);
        a
    }

    #[test]
    fn neutral_sliders_change_nothing() {
        let frame = room();
        assert_eq!(adjusted(Adjust::default(), &frame), frame);
        let curve = Adjust::default().curve();
        for (c, y) in curve.iter().enumerate() {
            assert!((y * 255.0 - c as f32).abs() < 0.01, "{c} goes to {y}");
        }
    }

    #[test]
    fn tone_sliders_move_the_right_way() {
        let up = |set: fn(&mut Adjust, f32), level: u8| {
            (
                grey(with(set, 0.6), level),
                level,
                grey(with(set, -0.6), level),
            )
        };
        let rising = |(more, at, less): (u8, u8, u8), name: &str| {
            assert!(more > at && at > less, "{name}: {more} {at} {less}");
        };
        rising(up(|a, v| a.exposure = v, 100), "exposure at mid");
        rising(up(|a, v| a.shadows = v, 70), "shadows in the darks");
        rising(up(|a, v| a.highlights = v, 190), "highlights in the lights");
        rising(up(|a, v| a.whites = v, 220), "whites near white");
        rising(up(|a, v| a.blacks = v, 30), "blacks near black");
        rising(up(|a, v| a.contrast = v, 200), "contrast in the lights");
        let (more, at, less) = up(|a, v| a.contrast = v, 50);
        assert!(
            more < at && at < less,
            "contrast in the darks: {more} {at} {less}"
        );
        assert!(
            grey(with(|a, v| a.shadows = v, 1.0), 220) < 223,
            "shadows leave lights"
        );
        assert!(
            grey(with(|a, v| a.fade = v, 1.0), 0) > 15,
            "fade lifts black"
        );
        assert!(
            grey(with(|a, v| a.exposure = v, 2.0), 255) == 255,
            "white stays white"
        );
        let hdr = with(|a, v| a.hdr = v, 1.0);
        assert!(grey(hdr, 40) > 50 && grey(hdr, 220) < 210, "HDR evens out");
    }

    #[test]
    fn colour_sliders_move_the_right_way() {
        let colour = |a: Adjust, rgb| chroma(one(a, rgb));
        let (dull, strong, skin) = ([120, 100, 140], [200, 40, 30], [200, 150, 120]);
        assert!(colour(with(|a, v| a.saturation = v, 0.5), strong) > chroma(strong));
        assert_eq!(colour(with(|a, v| a.saturation = v, -1.0), dull), 0);
        let vibrance = with(|a, v| a.vibrance = v, 1.0);
        let rise = |rgb: [u8; 3]| f32::from(colour(vibrance, rgb)) / f32::from(chroma(rgb));
        assert!(rise(dull) > rise(strong), "vibrance favours dull colours");
        assert!(rise(dull) > rise(skin) * 1.3, "vibrance goes easy on skin");

        let corner = |a: Adjust| {
            let out = adjusted(a, &vec![128; W * H * 3]);
            (pixel(&out, 0)[0], pixel(&out, MIDDLE)[0])
        };
        let (edge, middle) = corner(with(|a, v| a.vignette = v, -1.0));
        assert!(edge < 90 && middle > 125, "darker corners: {edge} {middle}");
        let (edge, _) = corner(with(|a, v| a.vignette = v, 1.0));
        assert!(edge > 160, "lighter corners: {edge}");
    }

    #[test]
    fn clarity_widens_and_narrows_fine_contrast() {
        let frame: Vec<u8> = (0..W * H)
            .flat_map(|i| [if i.rem_euclid(8) < 4 { 110 } else { 146 }; 3])
            .collect();
        let swing = |v: f32| {
            let out = adjusted(with(|a, v| a.clarity = v, v), &frame);
            let px: Vec<u8> = (MIDDLE..MIDDLE + 8).map(|i| pixel(&out, i)[0]).collect();
            px.iter().max().unwrap() - px.iter().min().unwrap()
        };
        let (more, at, less) = (swing(1.0), swing(0.0), swing(-1.0));
        assert!(more > at + 20 && less + 10 < at, "{more} {at} {less}");
    }

    fn picked(frame: &[u8], face: Option<Face>) -> Adjust {
        auto(frame, W, WHOLE, &plain(), face)
    }

    #[test]
    fn auto_brightens_a_dark_frame() {
        let dark: Vec<u8> = room().iter().map(|&c| c.div_euclid(5)).collect();
        let a = picked(&dark, None);
        assert!(a.exposure > 0.8, "{a:?}");
    }

    #[test]
    fn auto_adds_contrast_to_a_flat_frame() {
        let flat: Vec<u8> = room().iter().map(|&c| 105 + c.div_euclid(6)).collect();
        let a = picked(&flat, None);
        let balanced = picked(&room(), None);
        assert!(
            a.contrast > 0.2 && a.contrast > balanced.contrast,
            "{a:?} {balanced:?}"
        );
    }

    #[test]
    fn auto_brings_up_a_face_against_a_window() {
        let face = Face {
            x: 400,
            y: 200,
            w: 240,
            h: 300,
        };
        let frame: Vec<u8> = (0..W * H)
            .flat_map(|i| {
                let (x, y) = (i.rem_euclid(W), i.div_euclid(W));
                let inside = (face.x..face.x + face.w).contains(&x)
                    && (face.y..face.y + face.h).contains(&y);
                if inside {
                    [70, 52, 42]
                } else {
                    [235, 235, 238]
                }
            })
            .collect();
        let without = picked(&frame, None);
        let with = picked(&frame, Some(face));
        assert!(
            with.exposure > without.exposure + 0.5,
            "{without:?} {with:?}"
        );
        let out = adjusted(with, &frame);
        let skin = luma(pixel(&out, MIDDLE).map(i32::from));
        assert!(skin > 100, "the face lands at {skin}: {with:?}");
    }

    /// The whole pass at the camera's size has to fit well inside a frame.
    #[test]
    fn every_slider_fits_the_budget() {
        let frame = room();
        let all = Adjust {
            exposure: 0.4,
            contrast: 0.3,
            highlights: -0.4,
            shadows: 0.3,
            whites: 0.2,
            blacks: -0.2,
            hdr: 0.5,
            clarity: 0.4,
            fade: 0.2,
            saturation: 0.1,
            vibrance: 0.3,
            vignette: -0.3,
        };
        let cast = crate::white::Cast {
            warm: 0.2,
            tint: 0.05,
        };
        let balance = crate::white::balance_lut(cast);
        let mut pass = Pass::default();
        let mut out = frame.clone();
        // The fastest pass, since other tests running alongside slow the average.
        let mut time = |adjust: Option<&Adjust>| {
            (0..40)
                .map(|_| {
                    let started = Instant::now();
                    out.copy_from_slice(&frame);
                    if let Some(a) = adjust {
                        pass.apply(&mut out, W, WHOLE, &balance, a);
                    }
                    started.elapsed().as_secs_f64() * 1000.0
                })
                .fold(f64::INFINITY, f64::min)
        };
        time(Some(&all));
        let copy = time(None);
        let ms = time(Some(&all)) - copy;
        let single = time(Some(&with(|a, v| a.exposure = v, 0.4))) - copy;
        println!("adjust pass at {W}x{H}: every slider {ms:.2} ms, exposure alone {single:.2} ms");
        if !cfg!(debug_assertions) {
            assert!(ms < 2.0, "{ms:.2} ms per frame");
        }
    }
}
