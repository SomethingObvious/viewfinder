//! Autofocus that the computer drives, since the camera's own live view AF hunts and can stall
//! the stream.

use crate::face::Face;

/// The eyes and nose of a face's box, as shares of its width and height.
const REGION: [f32; 4] = [0.2, 0.25, 0.6, 0.45];
/// Every second pixel of every second row keeps the cost near 0.1 ms a frame.
const STEP: usize = 2;
/// Live view lags the body by one or two frames and the lens takes a moment to settle.
const SKIP: u64 = 3;
/// Noise moves a single frame's reading by a few percent.
const AVERAGE: u32 = 2;
/// The gap between better and worse keeps noise from turning the search around.
const RISE: f32 = 1.06;
const DROP: f32 = 0.92;
/// Moves in a row that change nothing before the search turns, as at the end of the lens's travel.
const STILL: u32 = 5;
const FLAT: f32 = 1.15;
const TURNS: u32 = 6;
const DARK: u32 = 8;
/// About 1.5 s at 28 fps, since the face finder only looks twice a second.
const FACE_WAIT: u64 = 42;

/// `DriveLens` step sizes, biggest first, and the limit on moves including the ones back.
pub(crate) const SEARCH: (&[i8], u32) = (&[3, 2, 1], 48);
/// Always Focus only nudges, so a refocus never sweeps far from where the picture was sharp.
pub(crate) const NUDGE: (&[i8], u32) = (&[1], 12);

/// Mean square neighbour difference over the square of the mean, so exposure doesn't change it.
pub(crate) fn sharpness(pixels: &[u8], width: usize, rgb: bool, face: Option<Face>) -> Option<f32> {
    let face = face?;
    if width == 0 {
        return Some(0.0);
    }
    let height = pixels.len().div_euclid(width * 3);
    let share = |at: usize, n: usize, s: f32| at + (n as f32 * s) as usize;
    let [left, top, w, h] = REGION;
    let (x0, x1) = (
        share(face.x, face.w, left),
        share(face.x, face.w, left + w).min(width - 1),
    );
    let (y0, y1) = (
        share(face.y, face.h, top),
        share(face.y, face.h, top + h).min(height.saturating_sub(1)),
    );
    // Green carries most of the detail in RGB and costs a third of working out luma.
    let channel = usize::from(rgb);
    let luma = |px: &[u8; 3]| u32::from(px.get(channel).copied().unwrap_or_default());
    let (mut energy, mut sum, mut n) = (0u64, 0u64, 0u64);
    let rows = pixels.chunks_exact(width * 3);
    for (row, below) in rows
        .clone()
        .zip(rows.skip(1))
        .take(y1)
        .skip(y0)
        .step_by(STEP)
    {
        let (Some(row), Some(below)) = (
            row.as_chunks::<3>().0.get(x0..=x1),
            below.as_chunks::<3>().0.get(x0..x1),
        ) else {
            continue;
        };
        for (pair, under) in row.windows(2).zip(below).step_by(STEP) {
            let (Some(here), Some(right)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            let y = luma(here);
            let dx = u64::from(luma(right).abs_diff(y));
            let dy = u64::from(luma(under).abs_diff(y));
            energy += dx * dx + dy * dy;
            sum += u64::from(y);
            n += 1;
        }
    }
    if n == 0 || sum < u64::from(DARK) * n {
        return Some(0.0);
    }
    let (n, mean) = (n as f64, sum as f64 / n as f64);
    Some((energy as f64 / (n * mean * mean)) as f32)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Report {
    #[default]
    Idle,
    Searching,
    Focused,
    Flat,
    NoFace,
}

#[derive(Debug)]
pub(crate) struct Search {
    sizes: &'static [i8],
    size: usize,
    /// 1 is farther, -1 nearer.
    dir: i8,
    limit: u32,
    made: u32,
    moves: Vec<i8>,
    wait: u64,
    sum: f32,
    count: u32,
    best: Option<(f32, usize)>,
    leg: f32,
    rose: bool,
    leg_moves: u32,
    still: u32,
    allow: u32,
    turned_still: bool,
    turns: u32,
    range: (f32, f32),
    back: Vec<i8>,
    end: Option<Report>,
    report: Report,
}

impl Search {
    pub(crate) const fn new((sizes, limit): (&'static [i8], u32)) -> Self {
        Self {
            sizes,
            size: 0,
            dir: 1,
            limit,
            made: 0,
            moves: Vec::new(),
            wait: 0,
            sum: 0.0,
            count: 0,
            best: None,
            leg: 0.0,
            rose: false,
            leg_moves: 0,
            still: 0,
            allow: STILL,
            turned_still: false,
            turns: 0,
            range: (f32::INFINITY, 0.0),
            back: Vec::new(),
            end: None,
            report: Report::Searching,
        }
    }

    pub(crate) const fn after(mut self, pulled: u64) -> Self {
        self.wait = pulled;
        self
    }

    pub(crate) const fn report(&self) -> Report {
        self.report
    }

    pub(crate) const fn done(&self) -> bool {
        !matches!(self.report, Report::Searching)
    }

    pub(crate) fn update(&mut self, frame: u64, sharpness: Option<f32>, pulled: u64) -> Option<i8> {
        if self.done() || frame <= self.wait {
            return None;
        }
        if !self.back.is_empty() || self.end.is_some() {
            return self.go_back(pulled);
        }
        let Some(sharpness) = sharpness else {
            // No move is made before the first reading, so `wait` is still where it began.
            if self.best.is_none() && frame <= self.wait + FACE_WAIT {
                return None;
            }
            let at = self.best.map_or(0, |b| b.1);
            return self.finish(at, Report::NoFace, pulled);
        };
        self.sum += sharpness;
        self.count += 1;
        if self.count < AVERAGE {
            return None;
        }
        let reading = self.sum / self.count as f32;
        (self.sum, self.count) = (0.0, 0);
        self.range = (self.range.0.min(reading), self.range.1.max(reading));
        let Some(best) = self.best else {
            if reading <= 0.0 {
                return self.finish(0, Report::Flat, pulled);
            }
            self.best = Some((reading, 0));
            self.leg = reading;
            return self.step(pulled);
        };
        if reading > best.0 {
            self.best = Some((reading, self.moves.len()));
        }
        if reading > self.leg * RISE {
            (self.leg, self.rose, self.still, self.turned_still) = (reading, true, 0, false);
        } else if reading < self.leg * DROP {
            self.still = 0;
            // Past the sharpest place at this size, so come back with a smaller step. A leg
            // that only got worse went the wrong way, so it turns at the same size.
            if self.rose {
                if self.size + 1 >= self.sizes.len() {
                    return self.settle(pulled);
                }
                self.size += 1;
            }
            self.turn(reading, false);
        } else {
            self.still += 1;
            if self.still >= self.allow {
                if self.turned_still {
                    return self.settle(pulled);
                }
                self.turn(reading, true);
            }
        }
        if self.turns > TURNS {
            return self.settle(pulled);
        }
        self.step(pulled)
    }

    const fn turn(&mut self, reading: f32, still: bool) {
        self.dir = -self.dir;
        self.turns += 1;
        self.allow = if still { self.leg_moves + STILL } else { STILL };
        (self.leg, self.rose, self.still, self.turned_still) = (reading, false, 0, still);
        self.leg_moves = 0;
    }

    fn step(&mut self, pulled: u64) -> Option<i8> {
        let size = self.sizes.get(self.size).copied().unwrap_or(1);
        let best = self.best.map_or(0, |b| b.1);
        let back = (self.moves.len() + 1 - best) as u32;
        if self.made + 1 + back > self.limit {
            return self.settle(pulled);
        }
        let step = size * self.dir;
        self.moves.push(step);
        self.leg_moves += 1;
        self.moved(pulled);
        Some(step)
    }

    fn settle(&mut self, pulled: u64) -> Option<i8> {
        if self.range.1 < self.range.0 * FLAT {
            return self.finish(0, Report::Flat, pulled);
        }
        let at = self.best.map_or(0, |b| b.1);
        self.finish(at, Report::Focused, pulled)
    }

    fn finish(&mut self, at: usize, end: Report, pulled: u64) -> Option<i8> {
        // Steps of one size add up in any order, so only what's left over of each is undone,
        // the biggest first since they're taken from the end.
        let since = self.moves.get(at..).unwrap_or_default();
        self.back.clear();
        for &size in self.sizes.iter().rev() {
            let net: i32 = since
                .iter()
                .filter(|s| s.abs() == size)
                .map(|&s| i32::from(s.signum()))
                .sum();
            let undo = if net > 0 { -size } else { size };
            self.back
                .extend(std::iter::repeat_n(undo, net.unsigned_abs() as usize));
        }
        self.end = Some(end);
        self.go_back(pulled)
    }

    fn go_back(&mut self, pulled: u64) -> Option<i8> {
        let Some(step) = self.back.pop() else {
            self.report = self.end.take().unwrap_or(Report::Focused);
            return None;
        };
        self.moved(pulled);
        Some(step)
    }

    const fn moved(&mut self, pulled: u64) {
        self.made += 1;
        self.wait = pulled + SKIP;
    }
}

/// A share of the sharpness after focusing. Turning the head moves it less than this, and
/// leaning in more.
const LOST: f32 = 0.22;
/// A face this much wider or narrower than after focusing has moved nearer or farther. Sharpness
/// alone misses a step back, since a smaller face packs its detail into fewer pixels and reads
/// sharper, which cancels the blur.
const MOVED: f32 = 0.15;
/// Both scale how long a change takes to start a refocus, so they move together.
const LOST_FOR: f64 = 1.4;
const QUIET: f64 = 1.0;
const TAU: f64 = 0.35;

#[derive(Debug, Default)]
pub(crate) struct Watch {
    /// Sharpness and face width.
    reference: Option<[f32; 2]>,
    level: Option<[f32; 2]>,
    last: f64,
    quiet_until: f64,
    lost_since: Option<f64>,
}

impl Watch {
    pub(crate) fn rebase(&mut self, t: f64) {
        *self = Self {
            quiet_until: t + QUIET,
            ..Self::default()
        };
    }

    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add would change rounding and is a software call without FMA"
    )]
    pub(crate) fn update(&mut self, t: f64, reading: Option<(f32, f32)>) -> bool {
        let Some(reading) = reading.filter(|_| t >= self.quiet_until) else {
            return false;
        };
        let now: [f32; 2] = reading.into();
        let level = self.level.map_or(now, |l| {
            let k = (1.0 - (-(t - self.last).max(0.0) / TAU).exp()) as f32;
            [l[0] + (now[0] - l[0]) * k, l[1] + (now[1] - l[1]) * k]
        });
        (self.level, self.last) = (Some(level), t);
        let Some([sharp, size]) = self.reference else {
            if t >= self.quiet_until + TAU {
                self.reference = Some(level);
            }
            return false;
        };
        let blurred = level[0] < sharp * (1.0 - LOST);
        let moved = (level[1] / size - 1.0).abs() > MOVED;
        if !blurred && !moved {
            self.lost_since = None;
            return false;
        }
        let since = *self.lost_since.get_or_insert(t);
        t - since >= LOST_FOR
    }
}

#[cfg(test)]
#[expect(
    clippy::suboptimal_flops,
    reason = "the simulated scenes read as the formulas they model"
)]
pub(crate) mod tests {
    use super::*;

    /// Sizes 1, 2 and 3 move it 1, 5 and 20 units between end stops at 0 and 600, losing
    /// `backlash` units whenever it turns. Live view shows where it was `lag` frames ago.
    struct Lens {
        at: f64,
        peak: f64,
        detail: f64,
        backlash: f64,
        last_dir: i8,
        lag: usize,
        noise: f64,
        history: Vec<f64>,
        seed: u64,
    }

    impl Lens {
        fn new(at: f64, peak: f64) -> Self {
            Self {
                at,
                peak,
                detail: 4.0,
                backlash: 2.0,
                last_dir: 0,
                lag: 2,
                noise: 0.03,
                history: vec![at; 8],
                seed: 0x9E37_79B9_7F4A_7C15,
            }
        }

        fn drive(&mut self, step: i8) {
            let units = match step.unsigned_abs() {
                1 => 1.0,
                2 => 5.0,
                _ => 20.0,
            };
            let dir = step.signum();
            let lost = if self.last_dir != 0 && dir != self.last_dir {
                self.backlash.min(units)
            } else {
                0.0
            };
            self.last_dir = dir;
            self.at = (self.at + f64::from(dir) * (units - lost)).clamp(0.0, 600.0);
        }

        fn noise(&mut self) -> f64 {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            ((self.seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) * self.noise
        }

        fn frame(&mut self) -> f32 {
            self.history.push(self.at);
            let shown = self
                .history
                .get(self.history.len() - 1 - self.lag)
                .copied()
                .unwrap();
            let off = (shown - self.peak) / 8.0;
            let sharp = 1.0 + self.detail / (1.0 + off * off);
            (sharp * (1.0 + self.noise())) as f32
        }
    }

    struct Run {
        report: Report,
        moves: u32,
        turns: u32,
        at: f64,
        frames: u64,
    }

    fn run_behind(lens: &mut Lens, kind: (&'static [i8], u32), behind: u64) -> Run {
        let mut search = Search::new(kind);
        let (mut moves, mut turns, mut last) = (0, 0, 0i8);
        let mut frame = 0;
        let mut queue = std::collections::VecDeque::new();
        while !search.done() && frame < 2000 {
            frame += 1;
            queue.push_back((frame, lens.frame()));
            if queue.len() as u64 <= behind {
                continue;
            }
            let Some((shown, s)) = queue.pop_front() else {
                continue;
            };
            if let Some(step) = search.update(shown, Some(s), frame) {
                lens.drive(step);
                moves += 1;
                if last != 0 && step.signum() != last.signum() {
                    turns += 1;
                }
                last = step;
            }
        }
        Run {
            report: search.report(),
            moves,
            turns,
            at: lens.at,
            frames: frame,
        }
    }

    fn run(lens: &mut Lens, kind: (&'static [i8], u32)) -> Run {
        run_behind(lens, kind, 0)
    }

    #[test]
    fn converges_from_either_side() {
        let places = [
            (300.0, 190.0),
            (300.0, 420.0),
            (300.0, 303.0),
            (60.0, 200.0),
            (590.0, 470.0),
        ];
        for (start, peak) in places {
            for seed in 1..=12u64 {
                for (lag, backlash, behind) in [(1, 0.0, 0), (2, 2.0, 0), (2, 3.0, 1), (1, 1.0, 1)]
                {
                    let mut lens = Lens::new(start, peak);
                    (lens.lag, lens.backlash) = (lag, backlash);
                    lens.seed = lens.seed.wrapping_mul(seed);
                    let r = run_behind(&mut lens, SEARCH, behind);
                    let case = format!(
                        "{start} to {peak}, seed {seed}, lag {lag}, backlash {backlash}, behind {behind}"
                    );
                    assert_eq!(r.report, Report::Focused, "{case}");
                    assert!((r.at - peak).abs() <= 4.0, "{case}: ended at {}", r.at);
                    assert!(r.moves <= SEARCH.1, "{case}: {} moves", r.moves);
                    assert!(r.turns <= 6, "{case}: turned {} times", r.turns);
                }
            }
        }
    }

    #[test]
    fn stays_inside_the_move_limit() {
        for limit in [10, 20, 48] {
            let mut lens = Lens::new(0.0, 590.0);
            let r = run(&mut lens, (SEARCH.0, limit));
            assert!(r.moves <= limit, "{} moves for a limit of {limit}", r.moves);
            assert_ne!(r.report, Report::Searching);
        }
    }

    #[test]
    fn a_flat_scene_gives_up_where_it_started() {
        let mut lens = Lens::new(300.0, 300.0);
        lens.detail = 0.0;
        lens.backlash = 0.0;
        let r = run(&mut lens, SEARCH);
        assert_eq!(r.report, Report::Flat);
        assert!((r.at - 300.0).abs() < 1e-9, "ended at {}", r.at);
        assert!(r.moves <= 4 * STILL + 2, "{} moves", r.moves);
    }

    #[test]
    fn a_black_frame_never_moves_the_lens() {
        let mut search = Search::new(SEARCH);
        for frame in 1..50 {
            assert_eq!(search.update(frame, Some(0.0), frame), None);
        }
        assert_eq!(search.report(), Report::Flat);
    }

    #[test]
    fn waits_out_the_frames_behind_a_move() {
        let mut search = Search::new(SEARCH);
        assert_eq!(search.update(1, Some(2.0), 1), None);
        let first = search.update(2, Some(2.0), 2);
        assert!(first.is_some());
        for frame in 3..=2 + SKIP {
            assert_eq!(search.update(frame, Some(0.1), frame), None);
        }
        assert_eq!(search.update(3 + SKIP, Some(2.0), 3 + SKIP), None);
        assert!(search.update(4 + SKIP, Some(2.0), 4 + SKIP).is_some());
    }

    #[test]
    fn no_face_never_moves_the_lens() {
        let mut search = Search::new(SEARCH).after(10);
        for frame in 1..200 {
            assert_eq!(search.update(frame, None, frame), None);
        }
        assert_eq!(search.report(), Report::NoFace);
    }

    #[test]
    fn waits_a_moment_for_a_face() {
        let mut lens = Lens::new(300.0, 250.0);
        let mut search = Search::new(SEARCH);
        let mut frame = 0;
        while !search.done() {
            frame += 1;
            let s = (frame > FACE_WAIT - 5).then(|| lens.frame());
            if let Some(step) = search.update(frame, s, frame) {
                lens.drive(step);
            }
        }
        assert_eq!(search.report(), Report::Focused);
    }

    #[test]
    fn losing_the_face_goes_back_to_the_sharpest_place() {
        let mut lens = Lens::new(300.0, 250.0);
        lens.backlash = 0.0;
        let mut search = Search::new(SEARCH);
        let (mut frame, mut best) = (0, (0.0, 300.0));
        while !search.done() {
            frame += 1;
            let s = (frame <= 40).then(|| lens.frame());
            if let Some(s) = s.filter(|&s| s > best.0) {
                best = (s, lens.history.iter().rev().nth(lens.lag).copied().unwrap());
            }
            if let Some(step) = search.update(frame, s, frame) {
                lens.drive(step);
            }
        }
        assert_eq!(search.report(), Report::NoFace);
        assert!(
            (lens.at - best.1).abs() <= 5.0,
            "ended at {}, sharpest at {}",
            lens.at,
            best.1
        );
    }

    #[test]
    fn nudging_finds_a_small_drift_with_small_steps() {
        let mut lens = Lens::new(300.0, 304.0);
        let r = run(&mut lens, NUDGE);
        assert_eq!(r.report, Report::Focused);
        assert!((r.at - 304.0).abs() <= 2.0, "ended at {}", r.at);
        assert!(r.moves <= NUDGE.1);
        assert!(r.frames < 200);
    }

    #[test]
    fn always_focus_waits_for_a_real_loss() {
        let mut watch = Watch::default();
        watch.rebase(0.0);
        let sway = |t: f64| 3.0 * (1.0 + 0.15 * (t * 2.0).sin()) as f32;
        let mut asked = None;
        for i in 0..(40 * 28) {
            let t = f64::from(i) / 28.0;
            // A hand passing the lens, which should be waited out.
            let s = if (10.0..10.7).contains(&t) {
                1.0
            } else if t >= 20.0 {
                1.2
            } else {
                sway(t)
            };
            if watch.update(t, Some((s, 300.0))) && asked.is_none() {
                asked = Some(t);
            }
        }
        let t = asked.expect("asked for a refocus");
        assert!((20.0 + LOST_FOR..20.0 + LOST_FOR + 1.0).contains(&t), "{t}");
    }

    #[test]
    fn always_focus_sees_a_step_back() {
        let mut watch = Watch::default();
        watch.rebase(0.0);
        let mut asked = None;
        for i in 0..(10 * 28) {
            let t = f64::from(i) / 28.0;
            let (s, width) = if t >= 5.0 { (3.3, 220.0) } else { (3.0, 300.0) };
            if watch.update(t, Some((s, width))) && asked.is_none() {
                asked = Some(t);
            }
        }
        let t = asked.expect("asked for a refocus");
        assert!((5.0 + LOST_FOR..5.0 + LOST_FOR + 1.0).contains(&t), "{t}");
    }

    const FACE: Option<Face> = Some(Face {
        x: 200,
        y: 100,
        w: 600,
        h: 500,
    });

    fn chart(blur: usize, gain: f32) -> Vec<u8> {
        let (w, h): (usize, usize) = (1056, 704);
        let sharp: Vec<f32> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i.div_euclid(w));
                if (x.div_euclid(7) + y.div_euclid(7)) % 2 == 0 {
                    60.0
                } else {
                    160.0
                }
            })
            .collect();
        let mut out = sharp;
        for _ in 0..blur {
            let prev = out.clone();
            for (i, o) in out.iter_mut().enumerate() {
                let at = |d: isize| prev.get(i.wrapping_add_signed(d)).copied().unwrap_or(110.0);
                *o = (at(-1) + at(1) + at(-w.cast_signed()) + at(w.cast_signed()) + at(0)) / 5.0;
            }
        }
        out.iter()
            .flat_map(|&v| {
                let c = (v * gain).round().clamp(0.0, 255.0) as u8;
                [c, c, c]
            })
            .collect()
    }

    #[test]
    fn sharpness_falls_with_blur_but_not_with_exposure() {
        let read = |pixels: &[u8], width, rgb| sharpness(pixels, width, rgb, FACE).unwrap();
        let [sharp, soft, softer] = [0, 1, 4].map(|b| read(&chart(b, 1.0), 1056, true));
        assert!(
            sharp > soft * 1.5 && soft > softer * 1.5,
            "{sharp} {soft} {softer}"
        );
        let dim = read(&chart(1, 0.5), 1056, true);
        assert!((dim / soft - 1.0).abs() < 0.05, "{dim} {soft}");
        assert!(read(&chart(0, 1.0), 1056, false) > 0.0);
        assert!(read(&vec![0u8; 1056 * 704 * 3], 1056, true).abs() < f32::EPSILON);
        assert!(read(&[], 0, true).abs() < f32::EPSILON);
    }

    #[test]
    fn sharpness_reads_the_face_not_the_middle() {
        let mut frame = chart(0, 1.0);
        for (i, px) in frame.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            if i % 1056 >= 300 || i.div_euclid(1056) >= 300 {
                px.fill(110);
            }
        }
        let face = |x, y| {
            Some(Face {
                x,
                y,
                w: 250,
                h: 250,
            })
        };
        assert!(sharpness(&frame, 1056, true, face(20, 20)).unwrap() > 0.1);
        let middle = sharpness(&frame, 1056, true, face(400, 220)).unwrap();
        assert!(middle.abs() < f32::EPSILON, "{middle}");
        assert_eq!(sharpness(&frame, 1056, true, None), None);
    }

    pub(crate) fn best_ms(mut f: impl FnMut()) -> f64 {
        (0..30)
            .map(|_| {
                let started = std::time::Instant::now();
                for _ in 0..10 {
                    f();
                }
                started.elapsed().as_secs_f64() * 100.0
            })
            .fold(f64::INFINITY, f64::min)
    }

    #[test]
    fn sharpness_is_cheap() {
        let frame = chart(1, 1.0);
        let ms = best_ms(|| {
            assert!(sharpness(std::hint::black_box(&frame), 1056, true, FACE) > Some(0.0));
        });
        if !cfg!(debug_assertions) {
            assert!(ms < 0.3, "{ms:.3} ms a frame");
        }
    }
}
