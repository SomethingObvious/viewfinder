//! Steady white balance. The camera's auto lets a red shirt or a blue screen pull everything
//! else, so the light is worked out from the pixels that look grey and held until it changes.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crate::face::Face;

/// The `WhiteBalance` value for a colour temperature in Kelvin.
const KELVIN: u32 = 9;
/// Canon's presets and their colour temperatures, leaving out Flash and the custom ones.
const PRESETS: [(u32, f32); 5] = [
    (1, 5200.0),
    (2, 6000.0),
    (3, 3200.0),
    (4, 4000.0),
    (8, 7000.0),
];
/// The T3's colour temperature range, in 100 K steps.
const KELVIN_RANGE: (u32, u32) = (2500, 10000);
/// Daylight, where measuring starts when Auto or a custom setting says nothing about the light.
const START: f32 = 5200.0;

/// Mireds the light has to move before it's measured again. A desk lamp against a window is
/// over 100, and someone moving around the frame is under 30.
const THRESHOLD: f32 = 60.0;
/// Seconds it has to stay moved, so someone walking past doesn't count.
const HOLD: f64 = 10.0;
/// Seconds after a change before the camera's frames show it.
const SETTLE: f64 = 0.8;
const MEASURE: f64 = 0.5;
const TAU: f64 = 0.5;
/// 100 K steps are 11 mireds apart at 3000 K and 3 at 5500.
const TOLERANCE: f32 = 8.0;
const MAX_TRIES: u32 = 3;
/// Greys that moved less than this share of what a change should do mean live view isn't
/// showing white balance.
const RESPONSE: f32 = 0.3;
/// Mireds below which a change is too small to test that live view responds.
const TESTABLE: f32 = 20.0;

const STRIDE: usize = 8;
/// Clipping bleaches the colour toward white.
const CLIPPED: u8 = 245;
/// Below this, noise and the tone curve's toe swamp the colour.
const DARK: u8 = 32;
/// Natural log distance from the grey middle, narrowing pass by pass. Skin is about 1 warm of
/// grey, and a saturated shirt 2 or more.
const GATES: [f32; 2] = [0.35, 0.15];
const MIN_GREY: f32 = 0.05;
/// A frame of one colour has nothing grey but agrees with itself, and this turns it away. A
/// 2,500 K lamp through a 10,000 K setting is 3.2 warm and 0.47 green.
const PLAUSIBLE: (f32, f32) = (3.3, 0.6);

static LN: LazyLock<[f32; 256]> =
    LazyLock::new(|| std::array::from_fn(|c| 2.2 * (c.max(1) as f32 / 255.0).ln()));

/// The colour of the grey pixels, in natural log units of linear light.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Cast {
    /// ln(R / B).
    pub warm: f32,
    /// ln G less the mean of ln R and ln B.
    pub tint: f32,
}

impl Cast {
    pub(crate) fn toward(self, to: Self, k: f32) -> Self {
        Self {
            warm: (to.warm - self.warm).mul_add(k, self.warm),
            tint: (to.tint - self.tint).mul_add(k, self.tint),
        }
    }
}

/// Kept so the next start opens on it instead of flashing.
pub(crate) fn load_shown() -> Option<Cast> {
    let text = crate::store::read("white.txt")?;
    let mut v = text
        .split_whitespace()
        .map(|n| n.parse::<f32>().ok().filter(|v| v.is_finite()));
    Some(Cast {
        warm: v.next()??,
        tint: v.next()??,
    })
}

pub(crate) fn save_shown(cast: Cast) {
    crate::store::write("white.txt", &format!("{} {}\n", cast.warm, cast.tint));
}

#[derive(Debug, Default)]
pub(crate) struct Greys {
    samples: Vec<[f32; 2]>,
    kept: Vec<[f32; 2]>,
    axis: Vec<f32>,
}

impl Greys {
    /// Plain grey world would read a red shirt as a red light. Narrowing on the pixels nearest
    /// the middle colour drops shirts, skin and screens and keeps walls, paper and grey clothes.
    pub(crate) fn cast(&mut self, pixels: &[u8], width: usize, rgb: bool) -> Option<Cast> {
        self.samples.clear();
        if width == 0 {
            return None;
        }
        for row in pixels.chunks_exact(width * 3).step_by(STRIDE) {
            self.sample(row, STRIDE, rgb);
        }
        let centre = self.narrow()?;
        let enough =
            self.kept.len() as f32 >= self.samples.len() as f32 * MIN_GREY && self.kept.len() >= 20;
        let plausible = centre[0].abs() <= PLAUSIBLE.0 && centre[1].abs() <= PLAUSIBLE.1;
        (enough && plausible).then_some(Cast {
            warm: centre[0],
            tint: centre[1],
        })
    }

    /// From the cheeks and nose, where there's least hair, eye and lip.
    pub(crate) fn skin(
        &mut self,
        pixels: &[u8],
        width: usize,
        rgb: bool,
        face: Face,
    ) -> Option<Cast> {
        self.samples.clear();
        // Clipped to the frame, so a face partly off the right edge still gives its cheeks.
        let (x0, x1) = (
            face.x + face.w.div_euclid(4),
            (face.x + (face.w * 3).div_euclid(4)).min(width),
        );
        let (y0, y1) = (
            face.y + (face.h * 2).div_euclid(5),
            face.y + (face.h * 3).div_euclid(4),
        );
        let step = face.w.div_euclid(40).max(1);
        if width == 0 {
            return None;
        }
        for row in pixels
            .chunks_exact(width * 3)
            .skip(y0)
            .take(y1 - y0)
            .step_by(step)
        {
            if let Some(cheeks) = row.get(x0 * 3..x1 * 3) {
                self.sample(cheeks, step, rgb);
            }
        }
        let centre = self.narrow()?;
        (self.kept.len() >= 20).then_some(Cast {
            warm: centre[0],
            tint: centre[1],
        })
    }

    fn sample(&mut self, run: &[u8], step: usize, rgb: bool) {
        for px in run.as_chunks::<3>().0.iter().step_by(step) {
            let [r, g, b] = if rgb { *px } else { ycc_to_rgb(*px) };
            let (hi, lo) = (r.max(g).max(b), r.min(g).min(b));
            if !(DARK..CLIPPED).contains(&hi) || lo == 0 {
                continue;
            }
            let ln = |c: u8| LN.get(usize::from(c)).copied().unwrap_or_default();
            let (r, g, b) = (ln(r), ln(g), ln(b));
            self.samples.push([r - b, g - f32::midpoint(r, b)]);
        }
    }

    fn narrow(&mut self) -> Option<[f32; 2]> {
        let mut centre = [
            median(&mut self.axis, self.samples.iter().map(|s| s[0]))?,
            median(&mut self.axis, self.samples.iter().map(|s| s[1]))?,
        ];
        for gate in GATES {
            self.kept.clear();
            self.kept.extend(self.samples.iter().filter(|s| {
                let (dw, dt) = (s[0] - centre[0], s[1] - centre[1]);
                dw.hypot(dt) <= gate
            }));
            centre = [
                median(&mut self.axis, self.kept.iter().map(|s| s[0]))?,
                median(&mut self.axis, self.kept.iter().map(|s| s[1]))?,
            ];
        }
        Some(centre)
    }
}

/// Skin under a neutral light. The `ColorChecker`'s skin patches land at 0.88 and 1.09 warm and
/// -0.13 and -0.16 tint, and warm sits under them since a face put right on them looks too
/// orange live.
const SKIN: Cast = Cast {
    warm: 0.7,
    tint: -0.13,
};
const SKIN_LOCK: Duration = Duration::from_secs(2);
const FACE_WAIT: Duration = Duration::from_secs(3);
/// How far a face reading can move the correction from last time's, so a lamp or false face
/// read in a bad moment can't turn the whole session blue.
const ANCHOR: f32 = 0.15;
/// About half a preset apart.
const NEW_LIGHT: f32 = 0.25;

/// The cast a grey beside the face would have, or None when it's further off than any lamp.
pub(crate) fn light_on(skin: Cast) -> Option<Cast> {
    let light = Cast {
        warm: skin.warm - SKIN.warm,
        tint: skin.tint - SKIN.tint,
    };
    (light.warm.abs() <= PLAUSIBLE.0 && light.tint.abs() <= PLAUSIBLE.1).then_some(light)
}

/// Measures the skin once and keeps that, so moving around or leaving the frame changes nothing.
#[derive(Debug, Default)]
pub(crate) struct Skin {
    anchor: Option<Cast>,
    started: Option<Instant>,
    first: Option<Instant>,
    sum: [f32; 2],
    count: u16,
    locked: Option<Cast>,
}

impl Skin {
    pub(crate) fn new(anchor: Option<Cast>) -> Self {
        Self {
            anchor,
            ..Self::default()
        }
    }

    /// Only a real change of light lets go of last time's correction.
    pub(crate) fn reset(&mut self, was: Option<Cast>, now: Option<Cast>) {
        let moved = was.zip(now).is_some_and(|(a, b)| {
            (a.warm - b.warm).abs() > NEW_LIGHT || (a.tint - b.tint).abs() > NEW_LIGHT
        });
        *self = Self {
            anchor: self.anchor.filter(|_| !moved),
            started: Some(Instant::now()),
            ..Self::default()
        };
    }

    /// Still waiting on a face, so whatever is on screen should stay.
    pub(crate) fn deciding(&self) -> bool {
        self.locked.is_none()
            && (self.first.is_some()
                || self.anchor.is_some()
                || self.started.is_some_and(|t| t.elapsed() < FACE_WAIT))
    }

    pub(crate) fn correction(&mut self, reading: Option<Cast>) -> Option<Cast> {
        if self.locked.is_none()
            && let Some(r) = reading
        {
            let first = *self.first.get_or_insert_with(Instant::now);
            self.sum = [self.sum[0] + r.warm, self.sum[1] + r.tint];
            self.count = self.count.saturating_add(1);
            if first.elapsed() >= SKIN_LOCK {
                let n = f32::from(self.count);
                let near = |v: f32, a: f32| v.clamp(a - ANCHOR, a + ANCHOR);
                self.locked = light_on(Cast {
                    warm: self.sum[0] / n,
                    tint: self.sum[1] / n,
                })
                .map(|c| {
                    self.anchor.map_or(c, |a| Cast {
                        warm: near(c.warm, a.warm),
                        tint: near(c.tint, a.tint),
                    })
                });
                if self.locked.is_none() {
                    self.reset(None, None);
                }
            }
        }
        self.locked
    }
}

fn median(axis: &mut Vec<f32>, values: impl Iterator<Item = f32>) -> Option<f32> {
    axis.clear();
    axis.extend(values);
    let mid = axis.len().div_euclid(2);
    if axis.is_empty() {
        return None;
    }
    let (_, m, _) = axis.select_nth_unstable_by(mid, f32::total_cmp);
    Some(*m)
}

fn ycc_to_rgb([y, cb, cr]: [u8; 3]) -> [u8; 3] {
    let (y, cb, cr) = (i32::from(y) << 16, i32::from(cb) - 128, i32::from(cr) - 128);
    let c = |v: i32| ((v + (1 << 15)) >> 16).clamp(0, 255) as u8;
    [
        c(y + 91_881 * cr),
        c(y - 22_554 * cb - 46_802 * cr),
        c(y + 116_130 * cb),
    ]
}

/// With a 2.2 gamma, a gain in linear light is a fixed multiple of the code.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding and is a software call without FMA"
)]
pub(crate) fn balance_lut(cast: Cast) -> [[u8; 256]; 3] {
    let (warm, tint) = (cast.warm.clamp(-1.0, 1.0), cast.tint.clamp(-0.3, 0.3));
    // Lifts all three by the Rec. 709 luma the red, green and blue gains take away.
    let lift = 0.0702 * warm + 0.7152 * tint;
    [lift - warm / 2.0, lift - tint, lift + warm / 2.0].map(|gain| {
        let m = (gain / 2.2).exp();
        std::array::from_fn(|c| (c as f32 * m).round().min(255.0) as u8)
    })
}

pub(crate) fn mired(kelvin: f32) -> f32 {
    1e6 / kelvin
}

const LOCUS_FROM: f32 = 80.0;
const LOCUS_LEN: usize = 381;

/// A black body's ln(R / B) in linear sRGB, one mired a step from `LOCUS_FROM`.
static LOCUS: LazyLock<Vec<f32>> = LazyLock::new(|| {
    (0..LOCUS_LEN)
        .map(|i| {
            let [r, _, b] = black_body(1e6 / (LOCUS_FROM + i as f32));
            (r / b).ln()
        })
        .collect()
});

/// A black body's colour in linear sRGB, from Kim et al.'s cubic fit to the Planckian locus.
#[expect(
    clippy::suboptimal_flops,
    reason = "the fit reads as the published polynomials"
)]
pub(crate) fn black_body(kelvin: f32) -> [f32; 3] {
    let t = f64::from(kelvin.clamp(1667.0, 25000.0));
    let (t2, t3) = (t * t, t * t * t);
    let x = if t <= 4000.0 {
        -0.266_123_9e9 / t3 - 0.234_358_9e6 / t2 + 0.877_695_6e3 / t + 0.179_910
    } else {
        -3.025_846_9e9 / t3 + 2.107_037_9e6 / t2 + 0.222_634_7e3 / t + 0.240_390
    };
    let (x2, x3) = (x * x, x * x * x);
    let y = if t <= 2222.0 {
        -1.106_381_4 * x3 - 1.348_110_20 * x2 + 2.185_558_32 * x - 0.202_196_83
    } else if t <= 4000.0 {
        -0.954_947_6 * x3 - 1.374_185_93 * x2 + 2.091_370_15 * x - 0.167_488_67
    } else {
        3.081_758_0 * x3 - 5.873_386_70 * x2 + 3.751_129_97 * x - 0.370_014_83
    };
    let (cx, cz) = (x / y, (1.0 - x - y) / y);
    [
        3.2406 * cx - 1.5372 - 0.4986 * cz,
        -0.9689 * cx + 1.8758 + 0.0415 * cz,
        0.0557 * cx - 0.2040 + 1.0570 * cz,
    ]
    .map(|c| c.max(1e-4) as f32)
}

#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding and is a software call without FMA"
)]
fn locus(mired: f32) -> f32 {
    let at = (mired - LOCUS_FROM).clamp(0.0, (LOCUS_LEN - 1) as f32);
    let i = at.floor() as usize;
    let (a, b) = (
        LOCUS.get(i).copied().unwrap_or_default(),
        LOCUS
            .get(i + 1)
            .or_else(|| LOCUS.last())
            .copied()
            .unwrap_or_default(),
    );
    a + (b - a) * at.fract()
}

fn locus_tint(mired: f32) -> f32 {
    let [r, g, b] = black_body(1e6 / mired);
    g.ln() - f32::midpoint(r.ln(), b.ln())
}

/// The inverse of `locus`.
fn mired_of(warm: f32) -> f32 {
    let i = LOCUS.partition_point(|&w| w < warm).clamp(1, LOCUS_LEN - 1);
    let (a, b) = (
        LOCUS.get(i - 1).copied().unwrap_or_default(),
        LOCUS.get(i).copied().unwrap_or_default(),
    );
    let along = if b > a {
        ((warm - a) / (b - a)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    LOCUS_FROM + (i - 1) as f32 + along
}

pub(crate) struct Settings<'a> {
    pub wb: u32,
    pub kelvin: Option<u32>,
    pub wbs: &'a [u32],
    pub kelvins: &'a [u32],
}

impl Settings<'_> {
    fn mired(&self) -> Option<f32> {
        if self.wb == KELVIN {
            return self.kelvin.filter(|&k| k > 0).map(|k| mired(k as f32));
        }
        PRESETS.iter().find(|p| p.0 == self.wb).map(|p| mired(p.1))
    }

    fn has_kelvin(&self) -> bool {
        self.kelvin.is_some() && (self.wbs.is_empty() || self.wbs.contains(&KELVIN))
    }
}

pub(crate) fn held(wb: Option<u32>, kelvin: Option<u32>) -> Option<u32> {
    if wb == Some(KELVIN) {
        return kelvin;
    }
    PRESETS.iter().find(|p| Some(p.0) == wb).map(|p| p.1 as u32)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Change {
    pub wb: u32,
    pub kelvin: Option<u32>,
}

impl Change {
    fn mired(self) -> f32 {
        Settings {
            wb: self.wb,
            kelvin: self.kelvin,
            wbs: &[],
            kelvins: &[],
        }
        .mired()
        .unwrap_or_else(|| mired(START))
    }
}

fn pick(want: f32, s: &Settings<'_>) -> Option<Change> {
    let nearest = |options: &mut dyn Iterator<Item = (u32, f32)>| {
        options.min_by(|a, b| {
            (mired(a.1) - want)
                .abs()
                .total_cmp(&(mired(b.1) - want).abs())
        })
    };
    if s.has_kelvin() {
        let kelvin = if s.kelvins.is_empty() {
            let k = (1e6 / want / 100.0).round() as u32 * 100;
            k.clamp(KELVIN_RANGE.0, KELVIN_RANGE.1)
        } else {
            nearest(&mut s.kelvins.iter().filter(|&&k| k > 0).map(|&k| (k, k as f32)))?.0
        };
        return Some(Change {
            wb: KELVIN,
            kelvin: Some(kelvin),
        });
    }
    let wb = nearest(&mut PRESETS.iter().copied().filter(|p| s.wbs.contains(&p.0)))?.0;
    Some(Change { wb, kelvin: None })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// Readings before `from` are ignored, then averaged for `MEASURE` before deciding.
    Measure {
        from: f64,
        tries: u32,
    },
    Hold {
        reference: f32,
        away: Option<f64>,
    },
}

impl Default for Phase {
    fn default() -> Self {
        Self::Measure {
            from: 0.0,
            tries: 0,
        }
    }
}

/// Time is in seconds from any fixed start, so recorded or synthetic traces replay through it.
#[derive(Debug, Default)]
pub(crate) struct Steady {
    phase: Phase,
    cast: Option<Cast>,
    last: f64,
    first: f64,
    /// How warm the greys were before the last change, and how much warmer it should make them.
    applied: Option<(f32, f32)>,
    /// The light and the setting it was seen through, in mireds.
    light: Option<f32>,
    set: Option<f32>,
    unseen: bool,
    residual: Option<Cast>,
}

impl Steady {
    pub(crate) fn remeter(&mut self, t: f64) {
        *self = Self {
            phase: Phase::Measure {
                from: t + SETTLE,
                tries: 0,
            },
            ..Self::default()
        };
    }

    /// Kelvin.
    pub(crate) fn light(&self) -> Option<f32> {
        self.light.map(|m| 1e6 / m)
    }

    /// What the held setting leaves on the greys. The nearest preset can be 80 mireds off under
    /// a dim lamp, so the pipeline takes the rest out. It only changes when the setting settles.
    pub(crate) const fn residual(&self) -> Option<Cast> {
        self.residual
    }

    pub(crate) const fn unseen(&self) -> bool {
        self.unseen
    }

    /// Green beyond what the black body line explains, which Kelvin can't fix. Fluorescent
    /// tubes and cheap LEDs read positive.
    pub(crate) fn tint(&self) -> Option<f32> {
        let (cast, light, set) = (self.cast?, self.light?, self.set?);
        Some(cast.tint - (locus_tint(light) - locus_tint(set)))
    }

    pub(crate) fn update(
        &mut self,
        t: f64,
        cast: Option<Cast>,
        cam: &Settings<'_>,
    ) -> Option<Change> {
        let Some(set) = cam.mired() else {
            // Auto and custom settings don't say what they correct for, so start from a known one.
            return match self.phase {
                Phase::Measure { from, tries } if t >= from && tries < MAX_TRIES => {
                    self.change(t, None, pick(mired(START), cam), tries)
                }
                _ => None,
            };
        };
        let cast = cast?;
        match self.phase {
            Phase::Measure { from, tries } => {
                if t < from {
                    self.cast = None;
                    return None;
                }
                if self.cast.is_none() {
                    self.first = t;
                }
                let warm = self.smooth(t, cast, set);
                if t - self.first < MEASURE {
                    return None;
                }
                self.decide(t, warm, set, cam, tries)
            }
            Phase::Hold { reference, away } => {
                let warm = self.smooth(t, cast, set);
                if self.unseen {
                    return None;
                }
                let light = self.light.unwrap_or(set);
                if (light - reference).abs() <= THRESHOLD {
                    self.phase = Phase::Hold {
                        reference,
                        away: None,
                    };
                    return None;
                }
                let since = away.unwrap_or(t);
                self.phase = Phase::Hold {
                    reference,
                    away: Some(since),
                };
                if t - since < HOLD {
                    return None;
                }
                self.applied = None;
                self.decide(t, warm, set, cam, 0)
            }
        }
    }

    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add would change rounding and is a software call without FMA"
    )]
    fn smooth(&mut self, t: f64, reading: Cast, set: f32) -> f32 {
        let cast = match self.cast {
            Some(c) => {
                let k = (1.0 - (-(t - self.last).max(0.0) / TAU).exp()) as f32;
                Cast {
                    warm: c.warm + (reading.warm - c.warm) * k,
                    tint: c.tint + (reading.tint - c.tint) * k,
                }
            }
            None => reading,
        };
        (self.cast, self.last) = (Some(cast), t);
        // The camera divided by its own idea of the light, so the greys show what's left over.
        self.light = Some(mired_of(cast.warm + locus(set)));
        self.set = Some(set);
        cast.warm
    }

    fn hold(&mut self) -> Option<Change> {
        self.residual = self.cast;
        self.phase = Phase::Hold {
            reference: self.light.unwrap_or_default(),
            away: None,
        };
        None
    }

    fn decide(
        &mut self,
        t: f64,
        warm: f32,
        set: f32,
        cam: &Settings<'_>,
        tries: u32,
    ) -> Option<Change> {
        if let Some((before, meant)) = self.applied.take()
            && (before - warm) / meant < RESPONSE
        {
            self.unseen = true;
            return self.hold();
        }
        let light = self.light.unwrap_or(set);
        // The setting already held stays unless the light is clearly somewhere else.
        let close = if tries == 0 { THRESHOLD } else { TOLERANCE };
        if (light - set).abs() <= close || tries >= MAX_TRIES {
            return self.hold();
        }
        self.change(t, Some((warm, set)), pick(light, cam), tries)
    }

    fn change(
        &mut self,
        t: f64,
        from: Option<(f32, f32)>,
        change: Option<Change>,
        tries: u32,
    ) -> Option<Change> {
        let Some(change) = change else {
            return self.hold();
        };
        let to = change.mired();
        if from.is_some_and(|(_, set)| (to - set).abs() < 0.5) {
            return self.hold();
        }
        self.applied = from
            .filter(|(_, set)| (to - set).abs() >= TESTABLE)
            .map(|(warm, set)| (warm, locus(to) - locus(set)));
        self.phase = Phase::Measure {
            from: t + SETTLE,
            tries: tries + 1,
        };
        self.cast = None;
        self.residual = None;
        Some(change)
    }
}

#[cfg(test)]
#[expect(
    clippy::suboptimal_flops,
    reason = "the simulated scenes read as the formulas they model"
)]
mod tests {
    use super::*;

    const WBS: [u32; 8] = [0, 1, 2, 3, 4, 5, 8, KELVIN];

    #[test]
    fn balance_takes_a_lamps_leftover_off_grey() {
        let mut greys = Greys::default();
        // Grey under a 2,500 K lamp through the 3,200 K Tungsten preset.
        let frame = frame(2500.0, 3200.0, 0.0, true, 1);
        let cast = greys.cast(&frame, 528, true).unwrap();
        assert!(cast.warm > 0.2, "{cast:?}");
        let lut = balance_lut(cast);
        let fixed: Vec<u8> = frame
            .iter()
            .zip([0, 1, 2].into_iter().cycle())
            .map(|(px, ch)| {
                lut.get(ch)
                    .and_then(|t| t.get(usize::from(*px)))
                    .copied()
                    .unwrap()
            })
            .collect();
        let after = greys.cast(&fixed, 528, true).unwrap();
        // Rounding to 8 bits leaves about 8 mireds, inside `TOLERANCE`.
        assert!(
            after.warm.abs() < cast.warm / 4.0 && after.tint.abs() < 0.05,
            "{after:?}"
        );
    }

    #[test]
    fn a_bad_face_reading_stays_near_last_time() {
        let last = Cast {
            warm: -0.7,
            tint: -0.2,
        };
        let mut skin = Skin {
            first: Instant::now().checked_sub(SKIN_LOCK),
            ..Skin::new(Some(last))
        };
        let lamp = Cast {
            warm: super::SKIN.warm + 0.6,
            tint: super::SKIN.tint + 0.3,
        };
        let cast = skin.correction(Some(lamp)).unwrap();
        assert!((cast.warm - (last.warm + ANCHOR)).abs() < 1e-6, "{cast:?}");
        assert!((cast.tint - (last.tint + ANCHOR)).abs() < 1e-6, "{cast:?}");

        skin.reset(
            Some(last),
            Some(Cast {
                warm: -0.68,
                tint: -0.2,
            }),
        );
        assert!(
            skin.anchor.is_some(),
            "a small shift keeps last time's correction"
        );
        skin.reset(
            Some(last),
            Some(Cast {
                warm: 0.0,
                tint: -0.2,
            }),
        );
        assert!(skin.anchor.is_none(), "new light lets it go");
    }

    #[test]
    fn a_face_lit_by_a_screen_is_put_back_on_skin() {
        // The ColorChecker's light skin, 0.4 too blue, on a grey wall.
        let (w, h) = (200, 120);
        let face = Face {
            x: 60,
            y: 20,
            w: 80,
            h: 90,
        };
        let blue_skin = [175u8, 150, 150];
        let mut frame = vec![110u8; w * h * 3];
        for y in face.y..face.y + face.h {
            for x in face.x..face.x + face.w {
                frame
                    .get_mut((y * w + x) * 3..(y * w + x) * 3 + 3)
                    .unwrap()
                    .copy_from_slice(&blue_skin);
            }
        }
        let reading = Greys::default().skin(&frame, w, true, face).unwrap();
        let mut skin = Skin {
            first: Instant::now().checked_sub(SKIN_LOCK),
            ..Skin::default()
        };
        let cast = skin.correction(Some(reading)).unwrap();
        let lut = balance_lut(cast);
        let fixed = blue_skin
            .iter()
            .zip(&lut)
            .map(|(c, t)| *t.get(usize::from(*c)).unwrap());
        let fixed: Vec<u8> = fixed.collect();
        let after = Greys::default()
            .skin(&fixed.repeat(w * h), w, true, face)
            .unwrap();
        assert!(
            (after.warm - super::SKIN.warm).abs() < 0.05
                && (after.tint - super::SKIN.tint).abs() < 0.05,
            "{reading:?} became {after:?}"
        );
    }

    const PRESETS_ONLY: [u32; 7] = [0, 1, 2, 3, 4, 5, 8];

    const WALL: [f32; 3] = [0.35, 0.35, 0.35];
    const DOOR: [f32; 3] = [0.7, 0.7, 0.68];
    const SHELF: [f32; 3] = [0.12, 0.12, 0.13];
    const BOOK: [f32; 3] = [0.05, 0.1, 0.4];
    const PLANT: [f32; 3] = [0.08, 0.25, 0.06];
    const SKIN: [f32; 3] = [0.55, 0.32, 0.22];
    const SHIRT: [f32; 3] = [0.6, 0.04, 0.05];

    /// The room lit at `light` K through a camera set to `set` K, with a red shirt filling the
    /// lower `shirt` share.
    #[expect(
        clippy::many_single_char_names,
        reason = "pixel coordinates and channels"
    )]
    fn frame(light: f32, set: f32, shirt: f32, rgb: bool, seed: u32) -> Vec<u8> {
        let (w, h) = (528usize, 352usize);
        let (lamp, camera) = (black_body(light), black_body(set));
        let gain = [0, 1, 2].map(|i| {
            lamp.get(i).copied().unwrap() / camera.get(i).copied().unwrap() * camera[1] / lamp[1]
        });
        let mut out = Vec::with_capacity(w * h * 3);
        for i in 0..w * h {
            let (x, y) = (i % w, i.div_euclid(w));
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let surface = if fy > 1.0 - shirt && (0.2..0.8).contains(&fx) {
                SHIRT
            } else if (0.35..0.65).contains(&fx) && (0.15..0.6).contains(&fy) {
                SKIN
            } else if fx < 0.15 {
                DOOR
            } else if fx > 0.85 && fy > 0.5 {
                PLANT
            } else if fx > 0.8 && fy < 0.3 {
                BOOK
            } else if fy > 0.8 {
                SHELF
            } else {
                WALL
            };
            let speck = ((i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed) >> 28) as f32
                / 16.0
                - 0.5;
            let px = [0, 1, 2].map(|ch| {
                let v = surface.get(ch).copied().unwrap() * gain.get(ch).copied().unwrap() * 0.55;
                (v.max(0.0).powf(1.0 / 2.2) * 255.0 + speck * 3.0)
                    .round()
                    .clamp(0.0, 255.0) as u8
            });
            if rgb {
                out.extend_from_slice(&px);
            } else {
                out.extend_from_slice(&rgb_to_ycc(px));
            }
        }
        out
    }

    fn rgb_to_ycc([r, g, b]: [u8; 3]) -> [u8; 3] {
        let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
        let y = 0.299 * r + 0.587 * g + 0.114 * b;
        [y, 128.0 + (b - y) / 1.772, 128.0 + (r - y) / 1.402]
            .map(|v| v.round().clamp(0.0, 255.0) as u8)
    }

    fn seen(light: f32, set: f32, shirt: f32, rgb: bool) -> f32 {
        let cast = Greys::default()
            .cast(&frame(light, set, shirt, rgb, 1), 528, rgb)
            .expect("greys");
        1e6 / mired_of(cast.warm + locus(mired(set)))
    }

    #[test]
    fn reads_the_light_from_the_greys() {
        for (light, set) in [
            (3000.0, 5200.0),
            (5200.0, 5200.0),
            (4200.0, 3200.0),
            (6500.0, 4000.0),
        ] {
            for rgb in [true, false] {
                let got = seen(light, set, 0.0, rgb);
                assert!(
                    (mired(got) - mired(light)).abs() < 8.0,
                    "{light} K through {set} K read as {got} K, rgb {rgb}"
                );
            }
        }
    }

    #[test]
    fn a_red_shirt_is_not_the_light() {
        for shirt in [0.0, 0.3, 0.45] {
            let got = seen(3400.0, 3400.0, shirt, true);
            assert!(
                (mired(got) - mired(3400.0)).abs() < 8.0,
                "shirt {shirt} read as {got} K"
            );
        }
    }

    #[test]
    fn nothing_grey_says_nothing() {
        let red = [180u8, 30, 40].repeat(200 * 100);
        assert_eq!(Greys::default().cast(&red, 200, true), None);
        let black = vec![0u8; 200 * 100 * 3];
        assert_eq!(Greys::default().cast(&black, 200, true), None);
        assert_eq!(Greys::default().cast(&[], 0, true), None);
    }

    #[test]
    fn the_locus_runs_warm_to_cool() {
        assert!(locus(mired(2700.0)) > locus(mired(4000.0)));
        assert!(locus(mired(4000.0)) > locus(mired(6500.0)));
        for k in [2500.0f32, 3200.0, 5200.0, 9000.0] {
            let m = mired(k);
            assert!((mired_of(locus(m)) - m).abs() < 0.05, "{k}");
        }
        let [r, g, b] = black_body(6504.0);
        assert!((r / g - 1.0).abs() < 0.08 && (b / g - 1.0).abs() < 0.08);
    }

    #[test]
    fn picks_kelvin_or_the_nearest_preset() {
        let s = |wbs: &'static [u32], kelvins: &'static [u32]| Settings {
            wb: 0,
            kelvin: Some(5200),
            wbs,
            kelvins,
        };
        assert_eq!(
            pick(mired(3333.0), &s(&WBS, &[])),
            Some(Change {
                wb: KELVIN,
                kelvin: Some(3300)
            })
        );
        assert_eq!(
            pick(mired(1500.0), &s(&WBS, &[])).and_then(|c| c.kelvin),
            Some(2500)
        );
        assert_eq!(
            pick(mired(3333.0), &s(&WBS, &[2500, 3000, 3500])).and_then(|c| c.kelvin),
            Some(3500)
        );
        assert_eq!(
            pick(mired(3300.0), &s(&PRESETS_ONLY, &[])).map(|c| c.wb),
            Some(3)
        );
        assert_eq!(
            pick(mired(4500.0), &s(&PRESETS_ONLY, &[])).map(|c| c.wb),
            Some(4)
        );
        assert_eq!(pick(mired(4500.0), &s(&[0], &[])), None);
    }

    struct Run {
        changes: Vec<(f64, Change)>,
        set: f32,
        unseen: bool,
    }

    /// A simulated camera at 28 fps. One that `responds` shows its setting in live view.
    fn run(
        secs: f64,
        wbs: &[u32],
        light: impl Fn(f64) -> f32,
        shirt: impl Fn(f64) -> f32,
        responds: bool,
    ) -> Run {
        let mut steady = Steady::default();
        steady.remeter(0.0);
        let mut greys = Greys::default();
        let (mut wb, mut kelvin) = (0u32, Some(5200u32));
        let mut changes = Vec::new();
        let mut cast = None;
        for i in 0..(secs * 28.0) as u32 {
            let t = f64::from(i) / 28.0;
            let cam = Settings {
                wb,
                kelvin,
                wbs,
                kelvins: &[],
            };
            if i % 4 == 0 {
                // DIGIC 4's auto white balance only takes tungsten two thirds of the way.
                let shown = match cam.mired() {
                    Some(m) if responds => 1e6 / m,
                    _ if responds => 1e6 / (mired(light(t)) * 0.67 + mired(5200.0) * 0.33),
                    _ => 5200.0,
                };
                cast = greys.cast(&frame(light(t), shown, shirt(t), true, i), 528, true);
            }
            if let Some(c) = steady.update(t, cast, &cam) {
                changes.push((t, c));
                (wb, kelvin) = (c.wb, c.kelvin.or(kelvin));
            }
        }
        let set = Settings {
            wb,
            kelvin,
            wbs,
            kelvins: &[],
        }
        .mired()
        .map_or(0.0, |m| 1e6 / m);
        Run {
            changes,
            set,
            unseen: steady.unseen(),
        }
    }

    #[test]
    fn settles_on_the_light_from_auto() {
        for light in [2900.0, 4300.0, 6200.0] {
            let r = run(10.0, &WBS, |_| light, |_| 0.0, true);
            assert!(
                (mired(r.set) - mired(light)).abs() <= TOLERANCE + 6.0,
                "{light} K settled at {} K after {:?}",
                r.set,
                r.changes
            );
            assert!(r.changes.len() <= 3, "{:?}", r.changes);
            assert!(r.changes.iter().all(|(t, _)| *t < 5.0), "{:?}", r.changes);
        }
    }

    #[test]
    fn a_red_shirt_coming_in_changes_nothing() {
        let shirt = |t: f64| if (20.0..40.0).contains(&t) { 0.45 } else { 0.0 };
        let r = run(50.0, &WBS, |_| 3200.0, shirt, true);
        let late: Vec<_> = r.changes.iter().filter(|(t, _)| *t > 8.0).collect();
        assert!(late.is_empty(), "{late:?}");
    }

    #[test]
    fn a_lamp_change_is_followed() {
        let light = |t: f64| if t >= 20.0 { 5000.0 } else { 3000.0 };
        let r = run(35.0, &WBS, light, |_| 0.0, true);
        let late: Vec<_> = r.changes.iter().filter(|(t, _)| *t >= 20.0).collect();
        assert!(!late.is_empty(), "no change after the lamp");
        assert!(late.first().unwrap().0 < 20.0 + HOLD + 1.0, "{late:?}");
        assert!(
            (mired(r.set) - mired(5000.0)).abs() <= TOLERANCE + 6.0,
            "ended at {} K",
            r.set
        );
    }

    #[test]
    fn a_body_without_kelvin_holds_the_nearest_preset() {
        let r = run(10.0, &PRESETS_ONLY, |_| 3100.0, |_| 0.0, true);
        assert_eq!(r.changes.last().map(|c| c.1.wb), Some(3), "{:?}", r.changes);
        assert!(r.changes.len() <= 3, "{:?}", r.changes);
    }

    #[test]
    fn live_view_that_ignores_white_balance_is_left_alone() {
        let r = run(30.0, &WBS, |_| 3000.0, |_| 0.0, false);
        assert!(r.changes.len() <= 2, "{:?}", r.changes);
        assert!(r.unseen);
        let r = run(30.0, &WBS, |_| 3000.0, |_| 0.0, true);
        assert!(!r.unseen);
    }
}
