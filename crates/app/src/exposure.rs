//! Steady exposure. With the mode dial on M the app sets ISO and shutter itself and only
//! meters again once the light has really changed, like a lamp being switched on. A face moving
//! nearer or farther moves the frame's level by well under a stop, so it changes nothing.

use std::sync::LazyLock;

/// What the frame's level is steered to, as the log average of linear luminance. Mid grey in
/// sRGB is about -2.5.
pub(crate) const TARGET: f32 = -2.5;
/// How far the light has to move, in stops, before the exposure is metered again.
const THRESHOLD: f32 = 1.0;
/// How long it has to stay moved, in seconds, so someone walking past a window doesn't count.
const HOLD: f64 = 1.5;
/// How long after a change the camera's frames are trusted to show it.
const SETTLE: f64 = 0.6;
/// How long the level is averaged before metering decides.
const MEASURE: f64 = 0.3;
/// Seconds.
const TAU: f64 = 0.25;
/// Close enough to the target to stop adjusting. ISO comes in whole stops on the T3, so the
/// finest steps it can make with the flicker-free shutters are about a third of a stop.
const TOLERANCE: f32 = 0.4;
/// The most one adjustment moves, so a covered lens doesn't throw the settings to the limit.
const MAX_STEP: f32 = 3.0;
const MAX_TRIES: u32 = 4;
/// If the picture moved by less than this share of an adjustment, the camera isn't showing
/// the exposure in live view (or the lens is capped), so metering gives up and holds.
const RESPONSE: f32 = 0.3;
/// Below this the frame is treated as black, so a capped lens reads as very dark, not as minus infinity.
const FLOOR: f32 = -12.0;

const ISO_100: u32 = 0x48;
/// ISO 3200. Above it the T3's live view turns to mush, and 6400 is an expansion setting.
const ISO_MAX: u32 = 0x70;
const SHUTTER_30: u32 = 0x60;
/// Multiples of 1/120 s, so lights on 60 Hz mains don't roll bands through the picture.
/// 1/125 is the camera's nearest to 1/120.
const FLICKER_FREE: [u32; 4] = [0x60, 0x63, 0x68, 0x70];

/// Log2 of linear luminance for each 8-bit code, assuming a 2.2 gamma.
pub(crate) static LINEAR: LazyLock<[f32; 256]> =
    LazyLock::new(|| std::array::from_fn(|c| (c as f32 / 255.0).powf(2.2).log2().max(FLOOR)));

/// The log average of linear luminance over a decoded frame, sampling every 8th pixel of every
/// 8th row. `pixels` is three bytes a pixel, either RGB or YCbCr with luma first.
pub(crate) fn brightness(pixels: &[u8], width: usize, rgb: bool) -> f32 {
    if width == 0 {
        return FLOOR;
    }
    let (mut sum, mut n) = (0.0f64, 0u32);
    for row in pixels.chunks_exact(width * 3).step_by(8) {
        for px in row.as_chunks::<3>().0.iter().step_by(8) {
            let y = if rgb {
                (u32::from(px[0]) * 54 + u32::from(px[1]) * 183 + u32::from(px[2]) * 19) >> 8
            } else {
                u32::from(px[0])
            };
            sum += f64::from(LINEAR.get(y as usize).copied().unwrap_or(FLOOR));
            n += 1;
        }
    }
    if n == 0 {
        FLOOR
    } else {
        (sum / f64::from(n)) as f32
    }
}

/// Canon's exposure codes step by 8 a stop, with 3 and 5 meaning thirds.
fn stops(code_from_base: i32) -> f32 {
    let frac = match code_from_base.rem_euclid(8) {
        3 => 1.0 / 3.0,
        5 => 2.0 / 3.0,
        f => f as f32 / 8.0,
    };
    code_from_base.div_euclid(8) as f32 + frac
}

/// How much light ISO and shutter let through, in stops above ISO 100 at 1/30. None for Auto ISO.
fn exposure(iso: u32, shutter: u32) -> Option<f32> {
    (iso >= ISO_100).then(|| {
        stops(iso.cast_signed() - ISO_100.cast_signed())
            - stops(shutter.cast_signed() - SHUTTER_30.cast_signed())
    })
}

pub(crate) struct Settings<'a> {
    pub iso: u32,
    pub shutter: u32,
    pub isos: &'a [u32],
    pub shutters: &'a [u32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Change {
    pub iso: u32,
    pub shutter: u32,
}

/// The allowed pair closest to `want` stops, preferring low ISO, then flicker-free shutters.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding and is a software call without FMA"
)]
fn pick(want: f32, s: &Settings<'_>) -> Option<Change> {
    let mut best: Option<(f32, Change)> = None;
    for &iso in s.isos.iter().filter(|&&i| (ISO_100..=ISO_MAX).contains(&i)) {
        for &shutter in s
            .shutters
            .iter()
            .filter(|&&t| (SHUTTER_30..=0xA0).contains(&t))
        {
            let Some(e) = exposure(iso, shutter) else {
                continue;
            };
            // A flickering shutter is only worth it in light too bright for 1/125 at ISO 100,
            // which is daylight, and daylight doesn't flicker.
            let flicker = if FLICKER_FREE.contains(&shutter) {
                0.0
            } else {
                0.75
            };
            let cost = (e - want).abs()
                + 0.04 * stops(iso.cast_signed() - ISO_100.cast_signed())
                + flicker;
            if best.is_none_or(|(c, _)| cost < c) {
                best = Some((cost, Change { iso, shutter }));
            }
        }
    }
    best.map(|(_, c)| c)
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

/// Decides when to change ISO and shutter from a stream of frame levels. Time is in seconds
/// from any fixed start, so recorded or synthetic traces can be replayed through it.
#[derive(Debug)]
pub(crate) struct Steady {
    phase: Phase,
    level: Option<f32>,
    last: f64,
    /// When the current measurement's first reading came in.
    first: f64,
    /// The level before the last change and how many stops it was meant to move.
    applied: Option<(f32, f32)>,
}

impl Default for Steady {
    fn default() -> Self {
        Self {
            phase: Phase::Measure {
                from: 0.0,
                tries: 0,
            },
            level: None,
            last: 0.0,
            first: 0.0,
            applied: None,
        }
    }
}

impl Steady {
    /// Meters again from scratch, as after connecting or changing the target.
    pub(crate) fn remeter(&mut self, t: f64) {
        *self = Self {
            phase: Phase::Measure {
                from: t + SETTLE,
                tries: 0,
            },
            ..Self::default()
        };
    }

    /// Takes one frame's level at time `t` and says what to change.
    pub(crate) fn update(
        &mut self,
        t: f64,
        level: f32,
        cam: &Settings<'_>,
        target: f32,
    ) -> Option<Change> {
        match self.phase {
            Phase::Measure { from, tries } => {
                if t < from {
                    self.level = None;
                    return None;
                }
                if self.level.is_none() {
                    self.first = t;
                }
                let level = self.smooth(t, level);
                if t - self.first < MEASURE {
                    return None;
                }
                self.decide(t, level, cam, target, tries)
            }
            Phase::Hold { reference, away } => {
                let level = self.smooth(t, level);
                if (level - reference).abs() <= THRESHOLD {
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
                self.decide(t, level, cam, target, 0)
            }
        }
    }

    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add would change rounding and is a software call without FMA"
    )]
    fn smooth(&mut self, t: f64, reading: f32) -> f32 {
        let level = match self.level {
            Some(l) => l + (reading - l) * (1.0 - (-(t - self.last).max(0.0) / TAU).exp()) as f32,
            None => reading,
        };
        self.level = Some(level);
        self.last = t;
        level
    }

    const fn hold(&mut self, level: f32) -> Option<Change> {
        self.phase = Phase::Hold {
            reference: level,
            away: None,
        };
        None
    }

    fn decide(
        &mut self,
        t: f64,
        level: f32,
        cam: &Settings<'_>,
        target: f32,
        tries: u32,
    ) -> Option<Change> {
        if let Some((before, meant)) = self.applied.take()
            && meant.abs() >= 1.0
            && (level - before) / meant < RESPONSE
        {
            return self.hold(level);
        }
        let error = target - level;
        if error.abs() <= TOLERANCE || tries >= MAX_TRIES {
            return self.hold(level);
        }
        // Auto ISO gives no number to start from, so the first step assumes ISO 400.
        let now = exposure(cam.iso, cam.shutter);
        let base = now.unwrap_or(2.0);
        let Some(change) = pick(base + error.clamp(-MAX_STEP, MAX_STEP), cam) else {
            return self.hold(level);
        };
        if change.iso == cam.iso && change.shutter == cam.shutter {
            return self.hold(level);
        }
        let new = exposure(change.iso, change.shutter).unwrap_or(base);
        self.applied = now.map(|now| (level, new - now));
        self.phase = Phase::Measure {
            from: t + SETTLE,
            tries: tries + 1,
        };
        self.level = None;
        Some(change)
    }
}

/// What the app holds on the camera: steady exposure and the brightness it aims for in stops
/// from `TARGET`, steady white balance, and how it focuses.
#[derive(Clone, Copy, Debug, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent switches in the window, not states of one machine"
)]
pub(crate) struct Prefs {
    pub steady: bool,
    pub brightness: f32,
    /// Off by default, as it's untested on the T3, which has no Kelvin setting in its menus.
    pub white: bool,
    /// Autofocus runs the camera's own live view AF rather than the computer's.
    pub camera_af: bool,
    /// Off by default, since it moves the lens on its own.
    pub hold_focus: bool,
    /// Whether the app ever focuses by itself. The nudge buttons and Remember Focus work
    /// either way.
    pub autofocus: bool,
    /// Puts the lens back where it was last focused each time the camera comes on, since
    /// the half-press that wakes the T3 also refocuses it.
    pub remember_focus: bool,
    /// How much warmer than neutral white balance leaves the picture, in the natural log of
    /// red over blue.
    pub warmth: f32,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            steady: true,
            brightness: 0.0,
            white: false,
            camera_af: false,
            hold_focus: false,
            autofocus: true,
            remember_focus: true,
            warmth: 0.0,
        }
    }
}

/// The most the Warmth slider moves white balance either way, about 30 mireds.
pub(crate) const WARMTH: f32 = 0.3;

pub(crate) fn load() -> Prefs {
    let mut prefs = Prefs::default();
    let text = crate::store::read("exposure.txt").unwrap_or_default();
    for (key, value) in crate::store::numbers(&text) {
        match key {
            "steady" => prefs.steady = value != 0.0,
            "white" => prefs.white = value != 0.0,
            "camera_af" => prefs.camera_af = value != 0.0,
            "hold_focus" => prefs.hold_focus = value != 0.0,
            "autofocus" => prefs.autofocus = value != 0.0,
            "remember_focus" => prefs.remember_focus = value != 0.0,
            "brightness" => prefs.brightness = value.clamp(-2.0, 2.0),
            "warmth" => prefs.warmth = value.clamp(-WARMTH, WARMTH),
            _ => {}
        }
    }
    prefs
}

pub(crate) fn save(prefs: Prefs) {
    crate::store::write(
        "exposure.txt",
        &format!(
            "steady={}\nbrightness={}\nwhite={}\ncamera_af={}\nhold_focus={}\nautofocus={}\nremember_focus={}\nwarmth={}\n",
            u8::from(prefs.steady),
            prefs.brightness,
            u8::from(prefs.white),
            u8::from(prefs.camera_af),
            u8::from(prefs.hold_focus),
            u8::from(prefs.autofocus),
            u8::from(prefs.remember_focus),
            prefs.warmth
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISOS: [u32; 8] = [0x00, 0x48, 0x50, 0x58, 0x60, 0x68, 0x70, 0x78];
    const SHUTTERS: [u32; 12] = [
        0x58, 0x5b, 0x5d, 0x60, 0x63, 0x65, 0x68, 0x6b, 0x6d, 0x70, 0x78, 0x80,
    ];
    /// How long the camera takes to show a new setting in live view.
    const LAG: f64 = 0.15;

    struct Run {
        changes: Vec<(f64, Change)>,
        /// Frame level minus target at the end.
        error: f32,
        /// Exposure in stops at the end.
        exposure: f32,
    }

    /// Plays `scene` (stops of light at ISO 100, 1/30) through a simulated camera at 28 fps.
    fn run(secs: f64, start: Change, scene: impl Fn(f64) -> f32, responds: bool) -> Run {
        let mut steady = Steady::default();
        steady.remeter(0.0);
        let mut set = start;
        let mut shown = start;
        let mut pending: Option<(f64, Change)> = None;
        let mut changes = Vec::new();
        let mut level = 0.0;
        for i in 0..(secs * 28.0) as u32 {
            let t = f64::from(i) / 28.0;
            if let Some((at, c)) = pending
                && t >= at
            {
                shown = c;
                pending = None;
            }
            let e = if responds {
                exposure(shown.iso, shown.shutter).unwrap_or(2.0)
            } else {
                0.0
            };
            level = (scene(t) + e).clamp(FLOOR, 0.0);
            let cam = Settings {
                iso: set.iso,
                shutter: set.shutter,
                isos: &ISOS,
                shutters: &SHUTTERS,
            };
            if let Some(c) = steady.update(t, level, &cam, TARGET) {
                changes.push((t, c));
                set = c;
                pending = Some((t + LAG, c));
            }
        }
        Run {
            changes,
            error: level - TARGET,
            exposure: exposure(set.iso, set.shutter).unwrap(),
        }
    }

    const START: Change = Change {
        iso: 0x48,
        shutter: 0x68,
    };

    /// A dim room, about 3 stops under at ISO 100 and 1/30.
    const ROOM: f32 = -5.5;

    /// Someone leaning in for ten seconds at a time and sitting back, swaying as they talk.
    /// Up close their face hides more of a brighter wall, which takes the whole frame down 0.8 stops.
    #[expect(
        clippy::suboptimal_flops,
        reason = "the test scene reads as the formula it models"
    )]
    fn leaning(t: f64) -> f32 {
        let near =
            |from: f64| ((t - from).clamp(0.0, 1.0) - (t - from - 10.0).clamp(0.0, 1.0)) as f32;
        let sway = 0.25 * (t * std::f64::consts::TAU / 3.0).sin() + 0.1 * (t * 97.0).sin();
        ROOM - 0.8 * (near(10.0) + near(35.0)) + sway as f32
    }

    #[test]
    fn settles_near_the_target_from_any_start() {
        let r = run(8.0, START, |_| ROOM, true);
        assert!(r.error.abs() <= TOLERANCE, "{}", r.error);
        assert!(r.changes.len() <= 3, "{:?}", r.changes);
        assert!(
            r.changes
                .iter()
                .all(|(_, c)| FLICKER_FREE.contains(&c.shutter) && c.iso != 0)
        );
        let r = run(
            8.0,
            Change {
                iso: 0x00,
                shutter: 0x60,
            },
            |_| ROOM,
            true,
        );
        assert!(r.error.abs() <= TOLERANCE, "{}", r.error);
    }

    #[test]
    fn moving_nearer_and_farther_changes_nothing() {
        let r = run(60.0, START, leaning, true);
        let after = r.changes.iter().filter(|(t, _)| *t > 6.0).count();
        assert_eq!(after, 0, "{:?}", r.changes);
    }

    #[test]
    fn a_lamp_switching_on_is_followed() {
        let lamp = |t: f64| leaning(t) + if t >= 20.0 { 2.0 } else { 0.0 };
        let before = run(19.9, START, lamp, true).exposure;
        let r = run(30.0, START, lamp, true);
        let late: Vec<_> = r.changes.iter().filter(|(t, _)| *t >= 20.0).collect();
        assert!(!late.is_empty(), "no change after the lamp");
        assert!(late.first().unwrap().0 < 20.0 + HOLD + 0.6, "{late:?}");
        assert!(
            (r.exposure - (before - 2.0)).abs() <= 0.5,
            "{} then {}",
            before,
            r.exposure
        );
        assert!(r.error.abs() <= 1.0, "{}", r.error);
    }

    #[test]
    fn a_passing_flash_changes_nothing() {
        let flash = |t: f64| ROOM + if (20.0..21.0).contains(&t) { 2.5 } else { 0.0 };
        let r = run(30.0, START, flash, true);
        assert!(r.changes.iter().all(|(t, _)| *t < 6.0), "{:?}", r.changes);
    }

    #[test]
    fn a_capped_lens_moves_the_settings_once_at_most() {
        let r = run(30.0, START, |_| -20.0, false);
        assert!(r.changes.len() <= 1, "{:?}", r.changes);
    }

    #[test]
    fn exposure_reads_canon_codes() {
        assert_eq!(exposure(0x48, 0x60), Some(0.0));
        assert_eq!(exposure(0x58, 0x68), Some(1.0));
        assert!((exposure(0x50, 0x63).unwrap() - (1.0 - 1.0 / 3.0)).abs() < 1e-6);
        assert_eq!(exposure(0x00, 0x60), None);
    }

    #[test]
    fn brightness_of_mid_grey() {
        let grey = vec![118u8; 64 * 16 * 3];
        let b = brightness(&grey, 64, true);
        assert!((b - -2.44).abs() < 0.05, "{b}");
        assert!((brightness(&grey, 64, false) - b).abs() < 0.05);
        let black = brightness(&vec![0u8; 64 * 16 * 3], 64, true);
        assert_eq!(black.to_bits(), FLOOR.to_bits(), "{black}");
    }
}
