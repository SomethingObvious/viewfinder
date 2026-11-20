//! Autofocus that the computer drives, since the camera's own live view AF hunts and can stall
//! the stream. It reads how sharp each frame is and steps the lens: big steps until it passes
//! the sharpest place, smaller ones back, then it returns to the sharpest place it saw.

use crate::face::Face;

/// The part of a face's box that's measured, as shares of its width and height: the eyes and
/// nose, without the hair or what's behind.
const REGION: [f32; 4] = [0.2, 0.25, 0.6, 0.45];
/// Every second pixel of every second row, which keeps the cost near 0.1 ms a frame.
const STEP: usize = 2;
/// Frames pulled after a move that may still show the lens before it or on its way. Live view
/// lags the body by one or two frames and the lens takes a moment to settle.
const SKIP: u64 = 3;
/// Readings averaged at each place, since noise moves a single frame's reading by a few percent.
const AVERAGE: u32 = 2;
/// How much sharper a reading has to be to count as better, and how much less sharp to count as
/// worse. The gap between them is what keeps noise from turning the search around.
const RISE: f32 = 1.06;
const DROP: f32 = 0.92;
/// Moves in a row that change nothing before the search turns around, as at the end of the
/// lens's travel or with nothing to focus on. After that turn it goes back past where it
/// started and this many more.
const STILL: u32 = 5;
/// If the sharpest and least sharp readings differ by less than this, there was nothing to focus on.
const FLAT: f32 = 1.15;
/// The most the search turns around before it settles for the sharpest place it saw.
const TURNS: u32 = 6;
/// Below this mean luma the frame is too dark to measure, like a capped lens.
const DARK: u32 = 8;
/// Frames a search waits for a face before giving up, about a second and a half at 28 fps,
/// since the face finder only looks twice a second.
const FACE_WAIT: u64 = 42;

/// `DriveLens` step sizes the button's search uses, biggest first, and its limit on moves,
/// counting the ones back to the sharpest place.
pub(crate) const SEARCH: (&[i8], u32) = (&[3, 2, 1], 48);
/// Always Focus only nudges, so a refocus never sweeps far from where the picture was sharp.
pub(crate) const NUDGE: (&[i8], u32) = (&[1], 12);

/// The mean square difference between neighbouring pixels over `REGION` of `face`, divided by
/// the square of their mean so it doesn't change with exposure. `pixels` is RGB or YCbCr.
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
    /// The face had too little detail to focus on, so the lens went back to where it started.
    Flat,
    /// There was no face, or it went away, so the lens went back to the sharpest place seen.
    NoFace,
}

/// One search for the sharpest place, fed a reading a frame. It never makes more moves than
/// its limit, including the ones back.
#[derive(Debug)]
pub(crate) struct Search {
    sizes: &'static [i8],
    size: usize,
    /// 1 is farther, -1 nearer.
    dir: i8,
    limit: u32,
    made: u32,
    /// Every move so far, so any earlier place can be reached by undoing the later ones.
    moves: Vec<i8>,
    /// Readings from frames numbered at or below this don't count yet.
    wait: u64,
    sum: f32,
    count: u32,
    /// The sharpest reading and how many moves in it came.
    best: Option<(f32, usize)>,
    /// The sharpest reading on this leg, and whether the leg has got sharper since it began.
    leg: f32,
    rose: bool,
    /// Moves on this leg, those in a row that changed nothing, and how many of those turn it.
    leg_moves: u32,
    still: u32,
    allow: u32,
    /// Whether the last turn was for a leg where nothing changed.
    turned_still: bool,
    turns: u32,
    range: (f32, f32),
    /// Moves still to make back to where the search will end, and what it will report then.
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

    /// Ignores frames numbered up to `pulled`, which were pulled before the search began.
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

    /// Takes the sharpness of frame `frame`, with `pulled` frames pulled so far, and says how
    /// far to drive the lens now.
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

    /// The next move on this leg, unless it would leave too few moves to get back to the
    /// sharpest place.
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

    /// Ends at the sharpest place seen, or where it started if nothing had any detail.
    fn settle(&mut self, pulled: u64) -> Option<i8> {
        if self.range.1 < self.range.0 * FLAT {
            return self.finish(0, Report::Flat, pulled);
        }
        let at = self.best.map_or(0, |b| b.1);
        self.finish(at, Report::Focused, pulled)
    }

    /// Undoes every move after the first `at`, then reports `end`.
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

/// How far sharpness has to fall, as a share of what it was after focusing, before Always
/// Focus looks again. Turning the head moves it less than this, and leaning in more.
const LOST: f32 = 0.25;
/// How long it has to stay fallen, in seconds.
const LOST_FOR: f64 = 2.0;
/// How long after any focus change to wait before the reference is taken, and how long the
/// reading is averaged for it.
const QUIET: f64 = 1.0;
const TAU: f64 = 0.5;

/// Always Focus: watches sharpness after a focus and says when it has stayed well below that for
/// a while, like steady exposure does for light.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    reference: Option<f32>,
    level: Option<f32>,
    last: f64,
    quiet_until: f64,
    lost_since: Option<f64>,
}

impl Watch {
    /// Starts over after the focus changed at time `t`, taking a new reference once it's quiet.
    pub(crate) fn rebase(&mut self, t: f64) {
        *self = Self {
            quiet_until: t + QUIET,
            ..Self::default()
        };
    }

    /// Takes one frame's sharpness at time `t` and says whether to refocus.
    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add would change rounding and is a software call without FMA"
    )]
    pub(crate) fn update(&mut self, t: f64, sharpness: Option<f32>) -> bool {
        let Some(sharpness) = sharpness.filter(|_| t >= self.quiet_until) else {
            return false;
        };
        let level = match self.level {
            Some(l) => l + (sharpness - l) * (1.0 - (-(t - self.last).max(0.0) / TAU).exp()) as f32,
            None => sharpness,
        };
        (self.level, self.last) = (Some(level), t);
        let Some(reference) = self.reference else {
            if t >= self.quiet_until + TAU {
                self.reference = Some(level);
            }
            return false;
        };
        if level >= reference * (1.0 - LOST) {
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

    /// A lens stepped by `DriveLens`: sizes 1, 2 and 3 move it 1, 5 and 20 units between end
    /// stops at 0 and 600, losing `backlash` units whenever it turns around. Live view shows
    /// where it was `lag` frames ago, and readings carry `noise` as a share.
    struct Lens {
        at: f64,
        peak: f64,
        /// How sharp the picture is in focus, over what defocus leaves.
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

        /// Uniform noise from xorshift, so runs repeat exactly.
        fn noise(&mut self) -> f64 {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            ((self.seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) * self.noise
        }

        /// One frame: what live view shows, as defocus blur falls off away from the peak.
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

    /// Runs a search over `lens` at one reading a frame, the step going out before the next
    /// pull. The pipeline's reading is `behind` frames older than the newest pull.
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

    /// From nearer and farther than the peak, at the end stops, and right beside it, with
    /// noise, live view one or two frames behind, slack in the gears and a pipeline running a
    /// frame behind the pulls.
    #[test]
    fn converges_from_either_side() {
        let places = [
            (300.0, 190.0),
            (300.0, 420.0),
            (300.0, 303.0),
            (60.0, 200.0),
            (590.0, 470.0),
        ];
        let mut worst = (0.0f64, 0u32, 0u64);
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
                    worst = (
                        worst.0.max((r.at - peak).abs()),
                        worst.1.max(r.moves),
                        worst.2.max(r.frames),
                    );
                }
            }
        }
        println!(
            "worst: {:.1} units off, {} moves, {} frames",
            worst.0, worst.1, worst.2
        );
    }

    #[test]
    fn stays_inside_the_move_limit() {
        // A peak at the far end with the lens at the near one is the longest search there is.
        for limit in [10, 20, 48] {
            let mut lens = Lens::new(0.0, 590.0);
            let r = run(&mut lens, (SEARCH.0, limit));
            assert!(r.moves <= limit, "{} moves for a limit of {limit}", r.moves);
            assert_ne!(r.report, Report::Searching);
        }
    }

    #[test]
    fn stops_once_focused() {
        let mut lens = Lens::new(300.0, 250.0);
        let mut search = Search::new(SEARCH);
        let mut frame = 0;
        while !search.done() {
            frame += 1;
            if let Some(step) = search.update(frame, Some(lens.frame()), frame) {
                lens.drive(step);
            }
        }
        for _ in 0..200 {
            frame += 1;
            assert_eq!(search.update(frame, Some(lens.frame()), frame), None);
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
        // Out, back past the start and as far again, then home by the shortest way.
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
        // Anything up to the pull count at the move plus SKIP may still show the old place.
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

    /// A face that turns up a moment after the button is still focused on.
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

    /// Losing the face partway stops the search at the sharpest place it had seen.
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

    /// Steady sharpness with someone shifting about changes nothing, and focus really going
    /// soft for a few seconds asks for a nudge.
    #[test]
    fn always_focus_waits_for_a_real_loss() {
        let mut watch = Watch::default();
        watch.rebase(0.0);
        let sway = |t: f64| 3.0 * (1.0 + 0.15 * (t * 2.0).sin()) as f32;
        let mut asked = None;
        for i in 0..(40 * 28) {
            let t = f64::from(i) / 28.0;
            // A quick dip as someone turns away, then focus slipping at 20 s.
            let s = if (10.0..11.0).contains(&t) {
                1.0
            } else if t >= 20.0 {
                1.2
            } else {
                sway(t)
            };
            if watch.update(t, Some(s)) && asked.is_none() {
                asked = Some(t);
            }
        }
        let t = asked.expect("asked for a refocus");
        assert!((20.0 + LOST_FOR..20.0 + LOST_FOR + 1.0).contains(&t), "{t}");
    }

    #[test]
    fn always_focus_leaves_the_lens_without_a_face() {
        let mut watch = Watch::default();
        watch.rebase(0.0);
        for i in 0..(40 * 28) {
            let t = f64::from(i) / 28.0;
            let s = (t < 5.0).then_some(3.0);
            assert!(!watch.update(t, s), "{t}");
        }
    }

    /// A face filling most of a 1056x704 frame.
    const FACE: Option<Face> = Some(Face {
        x: 200,
        y: 100,
        w: 600,
        h: 500,
    });

    /// A 1056x704 checkerboard of 7 pixel squares, so its edges fall on odd and even
    /// columns alike, blurred `blur` times and scaled by `gain`.
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

    /// The chart only in the top left corner, flat grey everywhere else.
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

    /// The fastest of 30 batches of 10 runs, in ms a run, so other tests running alongside
    /// don't count.
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
        println!("sharpness: {ms:.3} ms a frame");
        // Debug builds run many times slower, so the budget only holds in release.
        if !cfg!(debug_assertions) {
            assert!(ms < 0.3, "{ms:.3} ms a frame");
        }
    }
}
