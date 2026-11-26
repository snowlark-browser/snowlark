// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The curve a smooth scroll follows ([`ScrollCurve`]): an ease-in-out cubic Bézier from where the
//! scroll starts to where it goes, over a duration that depends on what asked for the scroll
//! ([`ScrollKind`]). A scroll asked for while one is under way retargets it: the new segment
//! starts where the box is, at the speed it has, so more wheel notches carry the movement on
//! without stopping it first.
//!
//! CSSOM View §4 leaves a smooth scroll's timing to the user agent; these are the durations and
//! curves the major engines' compositors use for each input, in 60 Hz frames.

use serde::{Deserialize, Serialize};

use crate::Point;

/// What asked for a smooth scroll, which sets its duration and curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollKind {
    /// Mouse wheel notches: the further a segment goes, the shorter it is (12 frames up to
    /// 120 px, 6 from 480 px), so a slow turn moves gently and a fast one keeps up.
    Wheel,
    /// Arrow, page, space and Home/End keys: 9 frames.
    Keyboard,
    /// `scroll-behavior: smooth` and the scroll methods' `behavior: "smooth"`: the square root of
    /// the distance in frames, at most 1.5 s, on a curve that starts fast and settles slowly.
    Programmatic,
}

/// Frames are counted at this rate.
const FRAMES_PER_SECOND: f64 = 60.0;
/// A keyboard scroll's length, in frames.
const CONSTANT_FRAMES: f64 = 9.0;
/// A wheel segment's length falls from `WHEEL_MAX_FRAMES` at `WHEEL_RAMP_START` px to
/// `WHEEL_MIN_FRAMES` at `WHEEL_RAMP_END` px.
const WHEEL_RAMP_START: f64 = 120.0;
const WHEEL_RAMP_END: f64 = 480.0;
const WHEEL_MIN_FRAMES: f64 = 6.0;
const WHEEL_MAX_FRAMES: f64 = 12.0;
/// The longest a programmatic smooth scroll takes, in seconds.
const PROGRAMMATIC_MAX_SECONDS: f64 = 1.5;
/// Distances (px) and durations (s) under this count as none.
const EPSILON: f64 = 0.01;

/// `ease-in-out`.
const EASE_IN_OUT: [f64; 4] = [0.42, 0.0, 0.58, 1.0];
/// The programmatic scroll's curve: a quick start and a long settle.
const PROGRAMMATIC: [f64; 4] = [0.4, 0.0, 0.0, 1.0];

/// How a segment's duration follows from its distance.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Rule {
    Constant,
    InverseDelta,
    DeltaBased,
}

impl ScrollKind {
    fn control_points(self) -> [f64; 4] {
        match self {
            ScrollKind::Programmatic => PROGRAMMATIC,
            ScrollKind::Wheel | ScrollKind::Keyboard => EASE_IN_OUT,
        }
    }

    fn rule(self) -> Rule {
        match self {
            ScrollKind::Wheel => Rule::InverseDelta,
            ScrollKind::Keyboard => Rule::Constant,
            ScrollKind::Programmatic => Rule::DeltaBased,
        }
    }
}

/// The longer of a delta's two components, with its sign (a segment's duration and speed go by
/// it).
fn max_dimension(dx: f64, dy: f64) -> f64 {
    if dx.abs() > dy.abs() {
        dx
    } else {
        dy
    }
}

/// A CSS cubic Bézier timing function (`cubic-bezier(x1, y1, x2, y2)`, x1 and x2 in 0..=1).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Bezier {
    ax: f64,
    bx: f64,
    cx: f64,
    ay: f64,
    by: f64,
    cy: f64,
}

impl Bezier {
    fn new([x1, y1, x2, y2]: [f64; 4]) -> Self {
        let cx = 3.0 * x1;
        let bx = 3.0 * (x2 - x1) - cx;
        let cy = 3.0 * y1;
        let by = 3.0 * (y2 - y1) - cy;
        Bezier {
            ax: 1.0 - cx - bx,
            bx,
            cx,
            ay: 1.0 - cy - by,
            by,
            cy,
        }
    }

    fn sample_x(&self, t: f64) -> f64 {
        ((self.ax * t + self.bx) * t + self.cx) * t
    }

    fn sample_y(&self, t: f64) -> f64 {
        ((self.ay * t + self.by) * t + self.cy) * t
    }

    fn derivative_x(&self, t: f64) -> f64 {
        (3.0 * self.ax * t + 2.0 * self.bx) * t + self.cx
    }

    fn derivative_y(&self, t: f64) -> f64 {
        (3.0 * self.ay * t + 2.0 * self.by) * t + self.cy
    }

    /// The curve parameter where x is `x` (0..=1): Newton's method, bisection when it stalls.
    fn solve_t(&self, x: f64) -> f64 {
        const EPS: f64 = 1e-7;
        let mut t = x;
        for _ in 0..8 {
            let err = self.sample_x(t) - x;
            if err.abs() < EPS {
                return t;
            }
            let d = self.derivative_x(t);
            if d.abs() < EPS {
                break;
            }
            t -= err / d;
        }
        let (mut lo, mut hi) = (0.0, 1.0);
        t = x;
        while lo < hi {
            let v = self.sample_x(t);
            if (v - x).abs() < EPS {
                return t;
            }
            if x > v {
                lo = t;
            } else {
                hi = t;
            }
            let next = (lo + hi) * 0.5;
            if next == t {
                break;
            }
            t = next;
        }
        t
    }

    /// The output at input progress `x` (0..=1).
    fn value(&self, x: f64) -> f64 {
        self.sample_y(self.solve_t(x))
    }

    /// dy/dx at input progress `x` (clamped to 0..=1).
    fn slope(&self, x: f64) -> f64 {
        let t = self.solve_t(x.clamp(0.0, 1.0));
        let (dx, dy) = (self.derivative_x(t), self.derivative_y(t));
        if dx == 0.0 && dy == 0.0 {
            return 0.0;
        }
        let s = dy / dx;
        if s.is_finite() {
            s
        } else if s > 0.0 {
            f64::MAX
        } else {
            f64::MIN
        }
    }
}

/// A smooth scroll's path over time (seconds from its start) between two scroll positions. Its
/// latest segment runs from `last_retarget` to `total`, from `initial` to `target`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollCurve {
    kind: ScrollKind,
    rule: Rule,
    points: [f64; 4],
    timing: Bezier,
    initial: (f64, f64),
    target: (f64, f64),
    total: f64,
    last_retarget: f64,
}

impl ScrollCurve {
    /// A smooth scroll of `kind` from `from` to `to`, starting at time 0.
    pub fn new(kind: ScrollKind, from: Point, to: Point) -> Self {
        Self::with(kind, kind.rule(), kind.control_points(), from, to)
    }

    fn with(kind: ScrollKind, rule: Rule, points: [f64; 4], from: Point, to: Point) -> Self {
        let mut c = ScrollCurve {
            kind,
            rule,
            points,
            timing: Bezier::new(points),
            initial: (from.x as f64, from.y as f64),
            target: (to.x as f64, to.y as f64),
            total: 0.0,
            last_retarget: 0.0,
        };
        c.total = c.segment_duration(c.target.0 - c.initial.0, c.target.1 - c.initial.1);
        c
    }

    pub fn kind(&self) -> ScrollKind {
        self.kind
    }

    /// Where it ends.
    pub fn target(&self) -> Point {
        Point::new(self.target.0 as f32, self.target.1 as f32)
    }

    /// When it ends, in seconds from its start.
    pub fn duration(&self) -> f64 {
        self.total
    }

    /// Whether it has ended at time `t`.
    pub fn is_done(&self, t: f64) -> bool {
        t >= self.total
    }

    /// The scroll position at time `t`.
    pub fn value(&self, t: f64) -> Point {
        let (x, y) = self.value64(t);
        Point::new(x as f32, y as f32)
    }

    fn value64(&self, t: f64) -> (f64, f64) {
        let duration = self.total - self.last_retarget;
        let t = t - self.last_retarget;
        if duration <= 0.0 || t >= duration {
            return self.target;
        }
        if t <= 0.0 {
            return self.initial;
        }
        let p = self.timing.value(t / duration);
        (
            self.initial.0 + (self.target.0 - self.initial.0) * p,
            self.initial.1 + (self.target.1 - self.initial.1) * p,
        )
    }

    /// How long a segment covering (`dx`, `dy`) takes.
    fn segment_duration(&self, dx: f64, dy: f64) -> f64 {
        let d = max_dimension(dx, dy).abs();
        let frames = match self.rule {
            Rule::Constant => CONSTANT_FRAMES,
            Rule::InverseDelta => {
                let slope =
                    (WHEEL_MIN_FRAMES - WHEEL_MAX_FRAMES) / (WHEEL_RAMP_END - WHEEL_RAMP_START);
                let offset = WHEEL_MAX_FRAMES - WHEEL_RAMP_START * slope;
                (offset + d * slope).clamp(WHEEL_MIN_FRAMES, WHEEL_MAX_FRAMES)
            }
            Rule::DeltaBased => d.sqrt().min(PROGRAMMATIC_MAX_SECONDS * FRAMES_PER_SECOND),
        };
        frames / FRAMES_PER_SECOND
    }

    /// The speed (px/s, along the longer axis of the segment) at time `t`.
    fn velocity(&self, t: f64) -> f64 {
        let duration = self.total - self.last_retarget;
        let slope = self.timing.slope((t - self.last_retarget) / duration);
        let d = max_dimension(
            self.target.0 - self.initial.0,
            self.target.1 - self.initial.1,
        );
        slope * (d / duration)
    }

    /// Aims the scroll at `to` from time `t` on: a new segment from the position at `t`, as fast
    /// there as the old one was, over the duration its own distance gives (shorter when the speed
    /// would otherwise overshoot a near target).
    pub fn update_target(&mut self, t: f64, to: Point) {
        let new_target = (to.x as f64, to.y as f64);
        let t = t.max(self.last_retarget);
        if max_dimension(self.target.0 - new_target.0, self.target.1 - new_target.1).abs() < EPSILON
        {
            self.target = new_target;
            return;
        }
        let at = self.value64(t);
        let (dx, dy) = (new_target.0 - at.0, new_target.1 - at.1);
        let delta = max_dimension(dx, dy);
        if delta.abs() < EPSILON {
            // (already there: it ends now)
            self.last_retarget = t;
            self.total = t;
            self.target = new_target;
            return;
        }
        if self.total - self.last_retarget <= 0.0 {
            // (the last segment took no time: there is no speed to keep)
            self.total = t + self.segment_duration(dx, dy);
            self.target = new_target;
            return;
        }
        let velocity = self.velocity(t);
        // (how long the present speed takes to get there, with room for the ease out: a fast
        // scroll turned to a near target ends sooner instead of overshooting and coming back)
        let bound = if velocity.abs() < EPSILON {
            f64::INFINITY
        } else {
            match delta / velocity * 2.5 {
                b if b < 0.0 => f64::INFINITY,
                b => b,
            }
        };
        let duration = self.segment_duration(dx, dy).min(bound);
        if duration < EPSILON {
            self.target = new_target;
            self.total = t;
            return;
        }
        let slope = (velocity * (duration / delta)).clamp(-1000.0, 1000.0);
        let [x1, _, x2, y2] = self.points;
        self.timing = Bezier::new([x1, x1 * slope, x2, y2]);
        self.initial = at;
        self.target = new_target;
        self.total = t + duration;
        self.last_retarget = t;
    }

    /// Moves the whole curve by `delta` (the content moved under the box: scroll anchoring), its
    /// shape and timing kept.
    pub fn adjust(&mut self, delta: Point) {
        let (dx, dy) = (delta.x as f64, delta.y as f64);
        self.initial = (self.initial.0 + dx, self.initial.1 + dy);
        self.target = (self.target.0 + dx, self.target.1 + dy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() <= eps, "{a} is not {b} (±{eps})");
    }

    fn y(c: &ScrollCurve, t: f64) -> f64 {
        c.value64(t).1
    }

    #[test]
    fn programmatic_curve_values() {
        let c = ScrollCurve::new(
            ScrollKind::Programmatic,
            Point::new(2.0, 40.0),
            Point::new(10.0, 20.0),
        );
        let d = c.duration();
        near(d, 20f64.sqrt() / 60.0, 1e-9);
        assert_eq!(c.value(-1.0), Point::new(2.0, 40.0));
        assert_eq!(c.value(0.0), Point::new(2.0, 40.0));
        let half = c.value64(d * 0.5);
        near(half.0, 8.8921, 0.00025);
        near(half.1, 22.7697, 0.00025);
        // (the reference values were taken at whole microseconds: 18633 of 74535)
        let quarter = c.value64(d * 0.25);
        near(quarter.0, 5.2583, 0.0005);
        near(quarter.1, 31.8541, 0.0012);
        let quarter = c.value64(d * (18633.0 / 74535.0));
        near(quarter.0, 5.2583, 0.0002);
        near(quarter.1, 31.8541, 0.0002);
        assert_eq!(c.value(d), Point::new(10.0, 20.0));
        assert_eq!(c.value(d + 1.0), Point::new(10.0, 20.0));
    }

    #[test]
    fn programmatic_duration_is_capped() {
        let c = ScrollCurve::new(
            ScrollKind::Programmatic,
            Point::new(2.0, 40.0),
            Point::new(200000.0, 400000.0),
        );
        assert_eq!(c.duration(), 1.5);
    }

    #[test]
    fn retarget_keeps_the_speed() {
        // (a constant-duration curve on the programmatic shape, retargeted twice)
        let mut c = ScrollCurve::with(
            ScrollKind::Programmatic,
            Rule::Constant,
            PROGRAMMATIC,
            Point::ZERO,
            Point::new(0.0, 3600.0),
        );
        let d = 9.0 / 60.0;
        near(c.duration(), d, 0.0002);
        near(y(&c, d / 2.0), 3101.4792, 0.0002);
        near(y(&c, d), 3600.0, 0.0002);

        c.update_target(d / 2.0, Point::new(0.0, 9900.0));
        near(c.duration(), d * 1.5, 0.0002);
        near(y(&c, d / 2.0), 3101.4792, 0.0002);
        near(y(&c, d), 9097.8, 0.1);
        near(y(&c, d * 1.5), 9900.0, 0.0002);

        // (a nearer target at high speed shortens it)
        c.update_target(d, Point::new(0.0, 7200.0));
        near(c.duration(), d * 2.0, 0.0002);
        near(y(&c, d), 9097.8, 0.1);
        near(y(&c, d * 2.0), 7200.0, 0.0002);
    }

    #[test]
    fn wheel_duration_falls_with_distance() {
        let at = |px: f32| {
            ScrollCurve::new(ScrollKind::Wheel, Point::ZERO, Point::new(0.0, px)).duration()
        };
        // (one notch: 12 frames)
        near(at(100.0), 12.0 / 60.0, 1e-9);
        near(at(-120.0), 12.0 / 60.0, 1e-9);
        near(at(300.0), 9.0 / 60.0, 1e-9);
        near(at(480.0), 6.0 / 60.0, 1e-9);
        near(at(5000.0), 6.0 / 60.0, 1e-9);
        near(
            ScrollCurve::new(ScrollKind::Keyboard, Point::ZERO, Point::new(0.0, 40.0)).duration(),
            9.0 / 60.0,
            1e-9,
        );
    }

    #[test]
    fn one_wheel_notch_eases_in_and_out() {
        let c = ScrollCurve::new(ScrollKind::Wheel, Point::ZERO, Point::new(0.0, 100.0));
        let d = c.duration();
        // (symmetric: half way at half time; slow at both ends)
        near(y(&c, d / 2.0), 50.0, 1e-6);
        let first = y(&c, 1.0 / 60.0);
        let second = y(&c, 2.0 / 60.0) - first;
        assert!(first > 0.0 && first < 3.0, "{first}");
        assert!(second > first, "{first} then {second}");
        let last = 100.0 - y(&c, d - 1.0 / 60.0);
        assert!(last > 0.0 && last < 3.0, "{last}");
    }

    #[test]
    fn a_notch_during_a_wheel_scroll_carries_it_on() {
        let mut c = ScrollCurve::new(ScrollKind::Wheel, Point::ZERO, Point::new(0.0, 100.0));
        let t = 0.1;
        let v = c.velocity(t);
        let at = y(&c, t);
        c.update_target(t, Point::new(0.0, 200.0));
        // (no jump and no change of speed where the new segment starts)
        near(y(&c, t), at, 1e-9);
        near(c.velocity(t), v, v * 1e-6);
        // (it keeps moving forward each frame to the new target)
        let mut last = at;
        let mut s = t;
        while !c.is_done(s) {
            s += 1.0 / 60.0;
            let p = y(&c, s);
            assert!(p >= last, "{p} after {last} at {s}");
            last = p;
        }
        near(last, 200.0, 1e-9);
        // (it ends one segment's time after the notch: 150 px left, 11.5 frames)
        near(at, 50.0, 1e-6);
        near(c.duration(), t + 11.5 / 60.0, 1e-9);
    }

    #[test]
    fn adjust_moves_the_whole_curve() {
        let mut c = ScrollCurve::new(ScrollKind::Wheel, Point::ZERO, Point::new(0.0, 100.0));
        let before = y(&c, 0.05);
        c.adjust(Point::new(0.0, 30.0));
        near(y(&c, 0.05), before + 30.0, 1e-9);
        assert_eq!(c.target(), Point::new(0.0, 130.0));
    }
}
