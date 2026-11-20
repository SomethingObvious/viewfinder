//! Auto Frame keeps the face centred and a third of the way down, and drifts back to the user's
//! framing a moment after the face is lost.

use crate::face::Face;

/// In percent, since the whole frame leaves no room to move.
pub(crate) const MIN_ZOOM: u32 = 125;
/// As a share of the crop.
const DEAD_ZONE: f32 = 0.1;
/// Per second. At 5 a move is 96% done after a second.
const PACE: f32 = 5.0;
/// Seconds, on top of the second the finder already keeps a face.
const HOLD: f32 = 1.5;

/// Pan and framing from 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Steer {
    across: Axis,
    down: Axis,
    lost: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Axis {
    at: f32,
    goal: f32,
    speed: f32,
}

impl Steer {
    pub(crate) const fn new(own: (f32, f32)) -> Self {
        Self {
            across: Axis::new(own.0),
            down: Axis::new(own.1),
            lost: 0.0,
        }
    }

    /// `own` is the user's pan and framing.
    pub(crate) fn next(
        mut self,
        face: Option<Face>,
        frame: (usize, usize),
        crop: (usize, usize),
        own: (f32, f32),
        dt: f32,
    ) -> Self {
        if let Some(f) = face {
            self.lost = 0.0;
            let middle = |at: usize, size: usize| (size as f32).mul_add(0.5, at as f32);
            self.across.follow(middle(f.x, f.w), frame.0, crop.0, 0.5);
            self.down
                .follow(middle(f.y, f.h), frame.1, crop.1, 1.0 / 3.0);
        } else {
            self.lost += dt;
            if self.lost >= HOLD {
                self.across.goal = own.0;
                self.down.goal = own.1;
            }
        }
        self.across.ease(dt);
        self.down.ease(dt);
        self
    }

    pub(crate) const fn at(&self) -> (f32, f32) {
        (self.across.at, self.down.at)
    }
}

impl Axis {
    const fn new(own: f32) -> Self {
        Self {
            at: own,
            goal: own,
            speed: 0.0,
        }
    }

    fn follow(&mut self, middle: f32, size: usize, crop: usize, place: f32) {
        let room = size.saturating_sub(crop) as f32;
        if room < 1.0 {
            return;
        }
        let crop = crop as f32;
        let want = (crop.mul_add(-place, middle) / room).clamp(0.0, 1.0);
        if (want - self.goal).abs() * room > DEAD_ZONE * crop {
            self.goal = want;
        }
    }

    /// A critically damped spring, so it never overshoots.
    fn ease(&mut self, dt: f32) {
        let change = self.at - self.goal;
        let pull = PACE.mul_add(change, self.speed) * dt;
        let decay = (-PACE * dt).exp();
        self.at = (change + pull).mul_add(decay, self.goal);
        self.speed = PACE.mul_add(-pull, self.speed) * decay;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: (usize, usize) = (1000, 600);
    const CROP: (usize, usize) = (800, 480);
    const OWN: (f32, f32) = (0.5, 0.5);
    const TICK: f32 = 1.0 / 30.0;

    const fn face(x: usize, y: usize) -> Face {
        Face {
            x: x - 50,
            y: y - 50,
            w: 100,
            h: 100,
        }
    }

    fn run(mut s: Steer, face: Option<Face>, seconds: f32) -> Steer {
        for _ in 0..(seconds / TICK).round() as usize {
            s = s.next(face, FRAME, CROP, OWN, TICK);
        }
        s
    }

    /// At OWN the crop's middle across is 500 and its third down is 60 + 160.
    #[test]
    fn a_face_inside_the_dead_zone_moves_nothing() {
        let s = run(Steer::new(OWN), Some(face(560, 250)), 3.0);
        assert_eq!(s.at(), OWN);
    }

    #[test]
    fn a_face_past_the_dead_zone_is_eased_to() {
        // Far right clamps the pan to 1. Down it's 20 pixels off, inside the dead zone.
        let far = Some(face(950, 240));
        let first = Steer::new(OWN).next(far, FRAME, CROP, OWN, TICK);
        assert!(
            first.at().0 - OWN.0 < 0.01,
            "starts gently: {:?}",
            first.at()
        );
        assert!(
            (first.at().1 - OWN.1).abs() < f32::EPSILON,
            "20 pixels down is inside the dead zone"
        );
        let mut s = first;
        let mut last = s.at().0;
        for _ in 0..30 {
            s = s.next(far, FRAME, CROP, OWN, TICK);
            assert!(s.at().0 >= last && s.at().0 <= 1.0, "steady, no overshoot");
            last = s.at().0;
        }
        assert!(
            s.at().0 > 0.9,
            "most of the way after a second: {:?}",
            s.at()
        );
        let s = run(s, far, 2.0);
        assert!((s.at().0 - 1.0).abs() < 0.001, "{:?}", s.at());
    }

    #[test]
    fn a_lost_face_holds_then_drifts_back() {
        let s = run(Steer::new(OWN), Some(face(950, 220)), 3.0);
        let held = run(s, None, HOLD - 0.2);
        assert!((held.at().0 - 1.0).abs() < 0.001, "holds: {:?}", held.at());
        let back = run(held, None, 3.0);
        assert!((back.at().0 - OWN.0).abs() < 0.001, "back: {:?}", back.at());
        let found = run(back, Some(face(950, 220)), 3.0);
        assert!(
            (found.at().0 - 1.0).abs() < 0.001,
            "found again: {:?}",
            found.at()
        );
    }

    #[test]
    fn no_room_means_no_move() {
        let mut s = Steer::new(OWN);
        for _ in 0..90 {
            s = s.next(Some(face(950, 100)), FRAME, FRAME, OWN, TICK);
        }
        assert_eq!(s.at(), OWN);
    }
}
