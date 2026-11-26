// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Transform and opacity animations a compositor runs on its own (`docs/compositor-animations.md`,
//! stage S2): what the page hands it with a frame ([`LayerAnimation`]) and how it samples one at a
//! display frame's time, so a busy page thread no longer holds the animation back.
//!
//! Chromium's counterpart is `cc::KeyframeModel` with `gfx::KeyframedTransformAnimationCurve` and
//! `KeyframedFloatAnimationCurve`, ticked by `cc::AnimationHost::TickAnimations`. The page keeps its
//! own timeline for everything script observes; the compositor only draws.
//!
//! Timing is Web Animations 1 §4.6 to §4.7 (the animation's start time and playback rate give the
//! local time, then phases, active time, iterations, directions and the effect's easing; end delay
//! 0, which Blink's `CompositorAnimations::ConvertTimingForCompositor` requires too), the keyframe
//! interval and its easing §5.3.4 ("the effect value of a keyframe effect"), easing functions CSS
//! Easing 2 (ported from stylo's `ComputedTimingFunction::calculate_output`, which the page uses,
//! with its epsilon and before flag), and transform lists interpolate function by function (CSS
//! Transforms 2 §16; the page sends only lists that do).

use serde::{Deserialize, Serialize};

use crate::{ColorFilters, LayerState, Matrix, StateOp, Ticks};

/// A 2D transform function with its lengths resolved to px and angles in radians: the primitives of
/// CSS Transforms 2 §16 that interpolate number by number.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransformFn {
    Translate(f32, f32),
    Scale(f32, f32),
    Rotate(f32),
    /// `skew()`; `skewX()` and `skewY()` are primitives of their own (they interpolate only with
    /// themselves, as in stylo).
    Skew(f32, f32),
    SkewX(f32),
    SkewY(f32),
}

impl TransformFn {
    /// The identity function of the same primitive (what `none` stands for against it, §16).
    pub fn identity(&self) -> TransformFn {
        match self {
            TransformFn::Translate(..) => TransformFn::Translate(0.0, 0.0),
            TransformFn::Scale(..) => TransformFn::Scale(1.0, 1.0),
            TransformFn::Rotate(_) => TransformFn::Rotate(0.0),
            TransformFn::Skew(..) => TransformFn::Skew(0.0, 0.0),
            TransformFn::SkewX(_) => TransformFn::SkewX(0.0),
            TransformFn::SkewY(_) => TransformFn::SkewY(0.0),
        }
    }

    /// Whether `self` and `other` are the same primitive (they interpolate number by number).
    pub fn same_kind(&self, other: &TransformFn) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    pub fn matrix(&self) -> Matrix {
        match *self {
            TransformFn::Translate(x, y) => Matrix::translation(x, y),
            TransformFn::Scale(x, y) => Matrix::scale(x, y),
            TransformFn::Rotate(a) => Matrix::rotation(a),
            TransformFn::Skew(x, y) => Matrix::skew(x, y),
            TransformFn::SkewX(a) => Matrix::skew(a, 0.0),
            TransformFn::SkewY(a) => Matrix::skew(0.0, a),
        }
    }

    fn numbers(&self) -> [f32; 2] {
        match *self {
            TransformFn::Translate(x, y) | TransformFn::Scale(x, y) | TransformFn::Skew(x, y) => {
                [x, y]
            }
            TransformFn::Rotate(a) | TransformFn::SkewX(a) | TransformFn::SkewY(a) => [a, 0.0],
        }
    }

    fn with_numbers(&self, [a, b]: [f32; 2]) -> TransformFn {
        match self {
            TransformFn::Translate(..) => TransformFn::Translate(a, b),
            TransformFn::Scale(..) => TransformFn::Scale(a, b),
            TransformFn::Rotate(_) => TransformFn::Rotate(a),
            TransformFn::Skew(..) => TransformFn::Skew(a, b),
            TransformFn::SkewX(_) => TransformFn::SkewX(a),
            TransformFn::SkewY(_) => TransformFn::SkewY(a),
        }
    }
}

/// The matrix of a transform list (CSS Transforms 1: the functions multiplied left to right, so
/// a point goes through the last one first), as a row-vector [`Matrix`].
pub fn list_matrix(list: &[TransformFn]) -> Matrix {
    list.iter()
        .rev()
        .fold(Matrix::IDENTITY, |m, f| m.then(&f.matrix()))
}

/// Whether two keyframes' transform lists interpolate function by function: equal lengths and
/// primitives, or one of them `none` (empty).
pub fn lists_interpolate(a: &[TransformFn], b: &[TransformFn]) -> bool {
    a.is_empty()
        || b.is_empty()
        || (a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same_kind(y)))
}

/// `from` and `to` at `p` (§16: `none` against a list is the list's identity functions).
fn blend_lists(from: &[TransformFn], to: &[TransformFn], p: f64, out: &mut Vec<TransformFn>) {
    out.clear();
    let n = from.len().max(to.len());
    for i in 0..n {
        let a = from.get(i).copied().unwrap_or_else(|| to[i].identity());
        let b = to.get(i).copied().unwrap_or_else(|| from[i].identity());
        let (x, y) = (a.numbers(), b.numbers());
        let lerp = |u: f32, v: f32| (u as f64 + (v as f64 - u as f64) * p) as f32;
        out.push(a.with_numbers([lerp(x[0], y[0]), lerp(x[1], y[1])]));
    }
}

/// `steps()`'s step position (CSS Easing 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepPosition {
    JumpStart,
    JumpEnd,
    JumpNone,
    JumpBoth,
}

/// An easing function (CSS Easing 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Easing {
    Linear,
    /// `cubic-bezier(x1, y1, x2, y2)` (and the keywords that name one).
    CubicBezier([f32; 4]),
    Steps(u32, StepPosition),
    /// `linear(...)`: its points as (input, output), in order.
    Piecewise(Vec<[f32; 2]>),
}

impl Easing {
    /// The output for input `progress`, as stylo's `ComputedTimingFunction::calculate_output`
    /// computes it with its before flag unset (`epsilon`: the cubic Bézier solver's precision).
    pub fn output(&self, progress: f64, epsilon: f64) -> f64 {
        self.output_with(progress, false, epsilon)
    }

    /// [`Easing::output`] with the before flag (Web Animations 1 §4.7.7: it moves `steps()` back
    /// a step at a step boundary).
    pub fn output_with(&self, progress: f64, before_flag: bool, epsilon: f64) -> f64 {
        let out = match self {
            Easing::Linear => progress,
            Easing::CubicBezier([x1, y1, x2, y2]) => {
                bezier_output(progress, epsilon, *x1, *y1, *x2, *y2)
            }
            Easing::Steps(steps, pos) => step_output(*steps, *pos, progress, before_flag),
            Easing::Piecewise(points) => piecewise_at(points, progress as f32) as f64,
        };
        out.clamp(f64::MIN, f64::MAX)
    }

    fn is_valid(&self) -> bool {
        match self {
            Easing::Linear => true,
            Easing::CubicBezier(v) => {
                v.iter().all(|n| n.is_finite())
                    && (0.0..=1.0).contains(&v[0])
                    && (0.0..=1.0).contains(&v[2])
            }
            Easing::Steps(n, pos) => *n > (*pos == StepPosition::JumpNone) as u32,
            Easing::Piecewise(p) => {
                p.len() <= MAX_EASING_POINTS
                    && p.iter().flatten().all(|n| n.is_finite())
                    && p.windows(2).all(|w| w[0][0] <= w[1][0])
            }
        }
    }
}

/// stylo's `ComputedTimingFunction::calculate_step_output` (its "approximately equal" is euclid's
/// `ApproxEq`, within 1e-6).
fn step_output(steps: u32, pos: StepPosition, progress: f64, before_flag: bool) -> f64 {
    let steps = steps.min(i32::MAX as u32) as i32;
    let mut current = (progress * steps as f64).floor() as i32;
    if matches!(pos, StepPosition::JumpStart | StepPosition::JumpBoth) {
        current = current.checked_add(1).unwrap_or(current);
    }
    if before_flag && ((progress * steps as f64).rem_euclid(1.0) - 0.0).abs() <= 1e-6 {
        current = current.checked_sub(1).unwrap_or(current);
    }
    if progress >= 0.0 && current < 0 {
        current = 0;
    }
    let jumps = match pos {
        StepPosition::JumpBoth => steps.checked_add(1).unwrap_or(steps),
        StepPosition::JumpNone => steps.checked_sub(1).unwrap_or(steps),
        _ => steps,
    };
    if progress <= 1.0 && current > jumps {
        current = jumps;
    }
    current as f64 / jumps as f64
}

/// stylo's `Bezier::calculate_bezier_output`.
fn bezier_output(progress: f64, epsilon: f64, x1: f32, y1: f32, x2: f32, y2: f32) -> f64 {
    if x1 == y1 && x2 == y2 {
        return progress;
    }
    if progress == 0.0 {
        return 0.0;
    }
    if progress == 1.0 {
        return 1.0;
    }
    let (x1, y1, x2, y2) = (x1 as f64, y1 as f64, x2 as f64, y2 as f64);
    if progress < 0.0 {
        if x1 > 0.0 {
            return progress * y1 / x1;
        }
        if y1 == 0.0 && x2 > 0.0 {
            return progress * y2 / x2;
        }
        return 0.0;
    }
    if progress > 1.0 {
        if x2 < 1.0 {
            return 1.0 + (progress - 1.0) * (y2 - 1.0) / (x2 - 1.0);
        }
        if y2 == 1.0 && x1 < 1.0 {
            return 1.0 + (progress - 1.0) * (y1 - 1.0) / (x1 - 1.0);
        }
        return 1.0;
    }
    let cx = 3.0 * x1;
    let bx = 3.0 * (x2 - x1) - cx;
    let ax = 1.0 - cx - bx;
    let cy = 3.0 * y1;
    let by = 3.0 * (y2 - y1) - cy;
    let ay = 1.0 - cy - by;
    let sample_x = |t: f64| ((ax * t + bx) * t + cx) * t;
    let sample_y = |t: f64| ((ay * t + by) * t + cy) * t;
    let derivative_x = |t: f64| (3.0 * ax * t + 2.0 * bx) * t + cx;
    let near = |a: f64, b: f64, e: f64| (a - b).abs() < e;
    let x = progress;
    let solve = || {
        // Newton's method first, then bisection (stylo's `solve_curve_x`).
        let mut t = x;
        for _ in 0..8 {
            let x2 = sample_x(t);
            if near(x2, x, epsilon) {
                return t;
            }
            let dx = derivative_x(t);
            if near(dx, 0.0, 1e-6) {
                break;
            }
            t -= (x2 - x) / dx;
        }
        let (mut lo, mut hi, mut t) = (0.0, 1.0, x);
        if t < lo {
            return lo;
        }
        if t > hi {
            return hi;
        }
        while lo < hi {
            let x2 = sample_x(t);
            if near(x2, x, epsilon) {
                return t;
            }
            if x > x2 {
                lo = t
            } else {
                hi = t
            }
            t = (hi - lo) / 2.0 + lo
        }
        t
    };
    sample_y(solve())
}

/// stylo's `PiecewiseLinearFunction::at`.
fn piecewise_at(points: &[[f32; 2]], x: f32) -> f32 {
    if !x.is_finite() {
        return if x > 0.0 { 1.0 } else { 0.0 };
    }
    match points.len() {
        0 => return x,
        1 => return points[0][1],
        _ => {}
    }
    let near = |a: f32, b: f32| (a - b).abs() <= f32::EPSILON * 16.0;
    let interpolate = |x: f32, prev: [f32; 2], next: [f32; 2], asymptote: [f32; 2]| {
        if near(x, next[0]) {
            return next[1];
        }
        if near(x, prev[0]) {
            return prev[1];
        }
        if near(prev[0], next[0]) {
            return next[1];
        }
        let slope = (next[1] - prev[1]) / (next[0] - prev[0]);
        slope * (x - asymptote[0]) + asymptote[1]
    };
    if x < points[0][0] {
        return interpolate(x, points[0], points[1], points[0]);
    }
    let last = points[points.len() - 1];
    if x >= last[0] {
        let second = points[points.len() - 2];
        return interpolate(x, second, last, last);
    }
    for i in (0..points.len() - 1).rev() {
        let (a, b) = (points[i], points[i + 1]);
        if x < a[0] {
            continue;
        }
        return interpolate(x, a, b, a);
    }
    x
}

/// `animation-direction` (Web Animations 1, playback direction).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

/// What an animation animates: each keyframe's value, in keyframe order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AnimatedValues {
    Opacity(Vec<f32>),
    /// Transform lists (empty: `none`). The box's whole transform at a list `X` is
    /// `pre · X · post` (row vectors: `pre`, then `X`, then `post`): `pre` moves the box's transform
    /// origin to (0, 0) in the list's coordinates, `post` applies its other transform properties and
    /// moves it back, as the page builds the box's transform group.
    Transform {
        lists: Vec<Vec<TransformFn>>,
        pre: [f32; 6],
        post: [f32; 6],
    },
    /// Filter lists of color functions (empty: `none`), each two adjacent ones interpolating
    /// function by function (Filter Effects 1 §14).
    Filter(Vec<ColorFilters>),
}

/// Which of a box's own state steps an animation writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    Transform,
    Opacity,
    Filter,
}

/// A keyframe effect on one property of a composited box ([`crate::LayerKind::Composited`]) that the
/// compositor samples at each display frame: Chromium's `cc::KeyframeModel`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerAnimation {
    /// The composited box's key ([`crate::LayerKind::Composited`]).
    pub element: u64,
    /// When the page's timeline reads 0 (its times below are seconds from here).
    pub origin: Ticks,
    /// The animation's start time (timeline s): its local time at timeline time `t` is
    /// `(t - start) × playback_rate` (Web Animations 1 §4.5.4). A CSS animation's is the start of
    /// its active interval, its delay folded in.
    pub start: f64,
    /// The playback rate: finite and not 0 (Blink composites no animation at rate 0).
    pub playback_rate: f64,
    /// The start delay (s of local time).
    pub delay: f64,
    /// The iteration duration (s), finite and above 0.
    pub duration: f64,
    /// The iteration count (0 or more, possibly infinite).
    pub iterations: f64,
    /// The iteration start (0 or more, finite).
    pub iteration_start: f64,
    pub direction: Direction,
    /// The effect's easing (Web Animations' `easing` timing property): from the directed progress
    /// to the iteration progress. `linear` for CSS animations and transitions.
    pub easing: Easing,
    /// The precision of the cubic Bézier easings: `None` for stylo's CSS one (1 / (200 × the
    /// keyframe interval's duration in s)), else this (script animations: 1e-6).
    pub epsilon: Option<f64>,
    /// Whether it fills backwards (before its start) and forwards (after its end).
    pub fill_backwards: bool,
    pub fill_forwards: bool,
    /// The keyframes' offsets (0 to 1, in order, the first 0 and the last 1) and the easing from
    /// each to the next.
    pub offsets: Vec<f32>,
    pub easings: Vec<Easing>,
    pub values: AnimatedValues,
}

/// The most keyframes an animation may have, and points a `linear()` easing.
pub const MAX_KEYFRAMES: usize = 256;
pub const MAX_EASING_POINTS: usize = 256;
/// The most functions in a keyframe's transform list.
pub const MAX_LIST: usize = 32;
/// The most animations a frame may carry.
pub const MAX_ANIMATIONS: usize = 1024;

/// How many bytes a frame's animations may take on the wire, at most: well inside the one pipe
/// frame a raster job's head must fit in (`snowlark_ipc::frame::MAX_BODY`).
pub const MAX_ANIMATION_BYTES: usize = 256 * 1024;

/// Keeps the animations that fit [`MAX_ANIMATION_BYTES`] and [`MAX_ANIMATIONS`], in order; the
/// boxes of those left out are drawn as the page painted them.
pub fn fit_budget(animations: &mut Vec<LayerAnimation>) {
    let mut used = 0;
    let mut kept = 0;
    animations.retain(|a| {
        used += a.wire_bytes();
        kept += 1;
        used <= MAX_ANIMATION_BYTES && kept <= MAX_ANIMATIONS
    });
}

/// What an animation gives at a moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sample {
    Opacity(f32),
    /// The box's whole transform (the [`StateOp::Transform`] the page would paint).
    Transform(Matrix),
    Filter(ColorFilters),
}

impl LayerAnimation {
    pub fn property(&self) -> Property {
        match self.values {
            AnimatedValues::Opacity(_) => Property::Opacity,
            AnimatedValues::Transform { .. } => Property::Transform,
            AnimatedValues::Filter(_) => Property::Filter,
        }
    }

    /// Whether a compositor can sample it: every number finite and in range, keyframes well
    /// ordered, lists within the caps and interpolating function by function.
    pub fn is_valid(&self) -> bool {
        let n = self.offsets.len();
        let values_ok = match &self.values {
            AnimatedValues::Opacity(v) => v.len() == n && v.iter().all(|o| o.is_finite()),
            AnimatedValues::Transform { lists, pre, post } => {
                lists.len() == n
                    && pre.iter().chain(post).all(|v| v.is_finite())
                    && lists.iter().all(|l| {
                        l.len() <= MAX_LIST
                            && l.iter().all(|f| f.numbers().iter().all(|v| v.is_finite()))
                    })
                    && lists.windows(2).all(|w| lists_interpolate(&w[0], &w[1]))
            }
            AnimatedValues::Filter(lists) => {
                lists.len() == n
                    && lists.iter().all(ColorFilters::is_valid)
                    && lists
                        .windows(2)
                        .all(|w| ColorFilters::interpolate(&w[0], &w[1]))
            }
        };
        (2..=MAX_KEYFRAMES).contains(&n)
            && self.easings.len() == n
            && values_ok
            && self.offsets[0] == 0.0
            && self.offsets[n - 1] == 1.0
            && self.offsets.windows(2).all(|w| w[0] <= w[1])
            && self.easings.iter().all(Easing::is_valid)
            && self.easing.is_valid()
            && self.epsilon.is_none_or(|e| e.is_finite() && e > 0.0)
            && self.start.is_finite()
            && self.playback_rate.is_finite()
            && self.playback_rate != 0.0
            && self.delay.is_finite()
            && self.duration.is_finite()
            && self.duration > 0.0
            && self.iterations >= 0.0
            && self.iteration_start.is_finite()
            && self.iteration_start >= 0.0
            // (backwards, the whole active interval is played: Blink's `Animation::
            // CheckCanStartAnimationOnCompositorInternal` takes no infinite one)
            && (self.playback_rate > 0.0 || self.iterations.is_finite())
    }

    /// An upper bound of its size on the wire (bytes).
    pub fn wire_bytes(&self) -> usize {
        let easings: usize = self
            .easings
            .iter()
            .map(|e| match e {
                Easing::Piecewise(p) => 16 + 8 * p.len(),
                _ => 24,
            })
            .sum();
        let values = match &self.values {
            AnimatedValues::Opacity(v) => 4 * v.len(),
            AnimatedValues::Transform { lists, .. } => {
                64 + lists.iter().map(|l| 8 + 12 * l.len()).sum::<usize>()
            }
            AnimatedValues::Filter(lists) => lists.len() * (2 + 8 * ColorFilters::MAX),
        };
        let effect_easing = match &self.easing {
            Easing::Piecewise(p) => 16 + 8 * p.len(),
            _ => 24,
        };
        136 + 4 * self.offsets.len() + easings + effect_easing + values
    }

    /// §4.6.5.2 the active duration (s of local time).
    fn active_duration(&self) -> f64 {
        if self.iterations == 0.0 {
            0.0
        } else {
            self.duration * self.iterations
        }
    }

    /// The local time (s) at timeline time `t` (§4.5.4).
    pub fn local_time(&self, t: f64) -> f64 {
        (t - self.start) * self.playback_rate
    }

    /// Whether its value still changes after timeline time `t` (s): its local time has not yet
    /// left the active interval in the direction it plays. A compositor keeps drawing frames
    /// while one does.
    pub fn running_at(&self, t: f64) -> bool {
        let local = self.local_time(t);
        if self.playback_rate > 0.0 {
            local < self.delay + self.active_duration()
        } else {
            local > self.delay
        }
    }

    /// The timeline time (s) at `now`.
    pub fn time_at(&self, now: Ticks) -> f64 {
        now.since(self.origin).as_secs_f64()
    }

    /// Its iteration progress at timeline time `t` (s), Web Animations 1 §4.6.6 to §4.7.7 (as the
    /// page's `Timing::compute_with` computes it, with the animation direction backwards for a
    /// negative playback rate and end delay 0): `None` where it has no effect (in its before phase
    /// without a backwards fill, in its after phase without a forwards one).
    pub fn iteration_progress(&self, t: f64) -> Option<f64> {
        #[derive(PartialEq)]
        enum Phase {
            Before,
            Active,
            After,
        }
        let local = self.local_time(t);
        let backwards = self.playback_rate < 0.0;
        let active_duration = self.active_duration();
        // §4.6.6 (the end time is the delay plus the active duration)
        let end = (self.delay + active_duration).max(0.0);
        let before_active = self.delay.min(end).max(0.0);
        let active_after = (self.delay + active_duration).min(end).max(0.0);
        let phase = if local < before_active || (backwards && local == before_active) {
            Phase::Before
        } else if local > active_after || (!backwards && local == active_after) {
            Phase::After
        } else {
            Phase::Active
        };
        // §4.7.2 the active time
        let active_time = match phase {
            Phase::Before => (local - self.delay).max(0.0),
            Phase::Active => local - self.delay,
            Phase::After => (local - self.delay).min(active_duration).max(0.0),
        };
        match phase {
            Phase::Before if !self.fill_backwards => return None,
            Phase::After if !self.fill_forwards => return None,
            _ => {}
        }
        // §4.7.3 the overall progress (the duration is above 0)
        let overall = active_time / self.duration + self.iteration_start;
        // §4.7.4 the simple iteration progress
        let mut simple = if overall.is_infinite() {
            self.iteration_start % 1.0
        } else {
            overall % 1.0
        };
        if simple == 0.0
            && phase != Phase::Before
            && active_time == active_duration
            && self.iterations != 0.0
        {
            simple = 1.0;
        }
        // §4.7.5 the current iteration
        let iteration = if phase == Phase::After && self.iterations.is_infinite() {
            f64::INFINITY
        } else if simple == 1.0 {
            overall.floor() - 1.0
        } else {
            overall.floor()
        };
        // §4.7.6 the directed progress
        let forwards = match self.direction {
            Direction::Normal => true,
            Direction::Reverse => false,
            Direction::Alternate | Direction::AlternateReverse => {
                let d = iteration + (self.direction == Direction::AlternateReverse) as u8 as f64;
                d.is_infinite() || d % 2.0 == 0.0
            }
        };
        let directed = if forwards { simple } else { 1.0 - simple };
        // §4.7.7 the transformed progress
        let before_flag =
            (phase == Phase::Before && forwards) || (phase == Phase::After && !forwards);
        Some(self.easing.output_with(directed, before_flag, 1e-6))
    }

    /// What it gives at timeline time `t` (s), if it has an effect then.
    pub fn sample(&self, t: f64) -> Option<Sample> {
        let p = self.iteration_progress(t)?;
        let n = self.offsets.len();
        let at = |o: f32| self.offsets.iter().filter(|&&x| x == o).count();
        // §5.3.4: outside [0, 1) with more than one keyframe at that end, its first (below 0) or
        // last (at 1 or above) keyframe alone; else the interval whose start is the last keyframe
        // at or before `p` (and before 1).
        if (p < 0.0 && at(0.0) > 1) || (p >= 1.0 && at(1.0) > 1) {
            let k = if p < 0.0 { 0 } else { n - 1 };
            return Some(self.value_between(k, k, 0.0));
        }
        let mut i = 0;
        while i + 2 < n && self.offsets[i + 1] as f64 <= p {
            i += 1;
        }
        let (a, b) = (self.offsets[i] as f64, self.offsets[i + 1] as f64);
        let (from, to, local) = if b > a {
            (i, i + 1, (p - a) / (b - a))
        } else {
            (i + 1, i + 1, 0.0)
        };
        // (stylo's epsilon for CSS: 1 / (200 × the interval's duration in seconds))
        let epsilon = self
            .epsilon
            .unwrap_or_else(|| 1.0 / (200.0 * ((b - a) * self.duration).max(f64::MIN_POSITIVE)));
        let eased = self.easings[from].output(local, epsilon);
        Some(self.value_between(from, to, eased))
    }

    /// The value from keyframe `from` to keyframe `to` at `eased`.
    fn value_between(&self, from: usize, to: usize, eased: f64) -> Sample {
        match &self.values {
            AnimatedValues::Opacity(v) => {
                let (x, y) = (v[from] as f64, v[to] as f64);
                Sample::Opacity((x + (y - x) * eased).clamp(0.0, 1.0) as f32)
            }
            AnimatedValues::Transform { lists, pre, post } => {
                let mut list = Vec::new();
                blend_lists(&lists[from], &lists[to], eased, &mut list);
                Sample::Transform(
                    crate::matrix_of(pre)
                        .then(&list_matrix(&list))
                        .then(&crate::matrix_of(post)),
                )
            }
            AnimatedValues::Filter(lists) => {
                Sample::Filter(ColorFilters::blend(&lists[from], &lists[to], eased))
            }
        }
    }
}

/// The state a piece is drawn through at `now`: its own steps written by its box's animations
/// (`None`: `state` as the page painted it). Each [`StateOp::Animated`] marks the box whose
/// transform and opacity steps are the two before it.
pub fn sampled_state(
    state: &LayerState,
    animations: &[LayerAnimation],
    now: Ticks,
) -> Option<LayerState> {
    if animations.is_empty() || !state.0.iter().any(|op| matches!(op, StateOp::Animated(_))) {
        return None;
    }
    let mut out = state.clone();
    let mut changed = false;
    for i in 0..out.0.len() {
        let StateOp::Animated(key) = out.0[i] else {
            continue;
        };
        if i < 2 {
            continue;
        }
        for a in animations.iter().filter(|a| a.element == key) {
            let Some(sample) = a.sample(a.time_at(now)) else {
                continue;
            };
            let (own, _) = out.0.split_at_mut(i);
            // (the box's own filter step, when it has one, is the last before the mark)
            let (own, filter) = match own {
                [rest @ .., StateOp::Filter(f)] => (rest, Some(f)),
                _ => (own, None),
            };
            let [.., t, o] = own else { continue };
            match (sample, t, o) {
                (Sample::Transform(m), StateOp::Transform(t), StateOp::Opacity(_)) => {
                    if m.is_finite() {
                        *t = [m.m11, m.m12, m.m21, m.m22, m.dx, m.dy];
                        changed = true;
                    }
                }
                (Sample::Opacity(v), StateOp::Transform(_), StateOp::Opacity(o)) => {
                    *o = v;
                    changed = true;
                }
                (Sample::Filter(v), StateOp::Transform(_), StateOp::Opacity(_)) => {
                    if let Some(f) = filter {
                        *f = v;
                        changed = true;
                    }
                }
                _ => {}
            }
        }
    }
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spin(direction: Direction, iterations: f64) -> LayerAnimation {
        LayerAnimation {
            element: 7,
            origin: Ticks::now(),
            start: 1.0,
            playback_rate: 1.0,
            delay: 0.0,
            duration: 2.0,
            iterations,
            iteration_start: 0.0,
            direction,
            easing: Easing::Linear,
            epsilon: None,
            fill_backwards: false,
            fill_forwards: false,
            offsets: vec![0.0, 1.0],
            easings: vec![Easing::Linear, Easing::Linear],
            values: AnimatedValues::Opacity(vec![0.0, 1.0]),
        }
    }

    fn opacity(a: &LayerAnimation, t: f64) -> Option<f32> {
        match a.sample(t)? {
            Sample::Opacity(o) => Some(o),
            _ => None,
        }
    }

    #[test]
    fn timing_follows_the_phases_iterations_and_directions() {
        let a = spin(Direction::Normal, 2.0);
        assert!(a.is_valid());
        assert_eq!(opacity(&a, 0.5), None);
        assert_eq!(opacity(&a, 1.0), Some(0.0));
        assert_eq!(opacity(&a, 2.0), Some(0.5));
        assert_eq!(opacity(&a, 4.0), Some(0.5));
        assert_eq!(opacity(&a, 5.0), None);
        let mut f = a.clone();
        f.fill_backwards = true;
        f.fill_forwards = true;
        assert_eq!(opacity(&f, 0.0), Some(0.0));
        // (the end of the last iteration, not the start of a next one)
        assert_eq!(opacity(&f, 9.0), Some(1.0));
        let r = spin(Direction::Reverse, f64::INFINITY);
        assert_eq!(opacity(&r, 1.5), Some(0.75));
        assert_eq!(opacity(&r, 101.5), Some(0.75));
        assert!(r.running_at(1e9));
        let alt = spin(Direction::Alternate, 3.0);
        assert_eq!(opacity(&alt, 2.0), Some(0.5));
        assert_eq!(opacity(&alt, 3.5), Some(0.75));
        assert_eq!(opacity(&alt, 5.5), Some(0.25));
        let alt_r = spin(Direction::AlternateReverse, 3.0);
        assert_eq!(opacity(&alt_r, 1.5), Some(0.75));
        assert_eq!(opacity(&alt_r, 3.5), Some(0.25));
    }

    /// Web Animations 1 §4.5.4 to §4.7.7 beyond what CSS animations use: the playback rate, a
    /// delay in local time, the iteration start, the effect's easing with its before flag, a
    /// negative rate, and §5.3.4's keyframe at an end that has several.
    #[test]
    fn script_animation_timing_follows_web_animations() {
        let mut a = spin(Direction::Normal, 1.0);
        a.playback_rate = 2.0;
        a.delay = 0.5;
        assert!(a.is_valid());
        // (local time (t - 1) × 2; active from 0.5 to 2.5)
        assert_eq!(opacity(&a, 1.2), None);
        assert_eq!(opacity(&a, 1.5), Some(0.25));
        assert_eq!(opacity(&a, 2.25), None);
        assert!(a.running_at(2.2) && !a.running_at(2.3));

        let mut s = spin(Direction::Normal, 1.0);
        s.iteration_start = 0.5;
        assert_eq!(opacity(&s, 1.0), Some(0.5));
        assert_eq!(opacity(&s, 2.0), Some(0.0));

        let mut e = spin(Direction::Normal, 1.0);
        e.easing = Easing::Steps(2, StepPosition::JumpEnd);
        assert_eq!(opacity(&e, 1.5), Some(0.0));
        assert_eq!(opacity(&e, 2.0), Some(0.5));
        // (in the before phase the before flag takes `jump-start` back a step)
        e.easing = Easing::Steps(1, StepPosition::JumpStart);
        e.delay = 1.0;
        e.fill_backwards = true;
        assert_eq!(opacity(&e, 1.0), Some(0.0));
        assert_eq!(opacity(&e, 2.5), Some(1.0));

        let mut r = spin(Direction::Normal, 1.0);
        r.playback_rate = -1.0;
        r.start = 4.0;
        assert!(r.is_valid());
        // (local time 4 - t: after, active, then before at 0)
        assert_eq!(opacity(&r, 1.0), None);
        assert_eq!(opacity(&r, 2.5), Some(0.75));
        assert!(r.running_at(3.9) && !r.running_at(4.0));
        assert_eq!(opacity(&r, 4.0), None);

        let mut d = spin(Direction::Normal, 1.0);
        d.offsets = vec![0.0, 0.0, 1.0];
        d.easings = vec![Easing::Linear; 3];
        d.values = AnimatedValues::Opacity(vec![0.2, 0.0, 1.0]);
        d.easing = Easing::CubicBezier([0.5, -0.5, 0.5, 1.0]);
        // (eased below 0, with two keyframes at 0: the first one's value)
        assert_eq!(opacity(&d, 1.05), Some(0.2));
    }

    #[test]
    fn keyframe_intervals_take_their_own_easing() {
        let mut a = spin(Direction::Normal, 1.0);
        a.offsets = vec![0.0, 0.5, 1.0];
        a.easings = vec![
            Easing::Steps(2, StepPosition::JumpEnd),
            Easing::Linear,
            Easing::Linear,
        ];
        a.values = AnimatedValues::Opacity(vec![0.0, 1.0, 0.0]);
        // first half: steps(2) from 0 to 1
        assert_eq!(opacity(&a, 1.2), Some(0.0));
        assert_eq!(opacity(&a, 1.6), Some(0.5));
        // second half: linear from 1 to 0
        assert_eq!(opacity(&a, 2.5), Some(0.5));
        assert!((opacity(&a, 2.999).unwrap() - 0.001).abs() < 1e-4);
    }

    #[test]
    fn easing_matches_the_curves() {
        // `ease` at 0.5 and `linear(0, 0.25 75%, 1)`.
        let ease = Easing::CubicBezier([0.25, 0.1, 0.25, 1.0]);
        assert!((ease.output(0.5, 1e-6) - 0.8024033877399112).abs() < 1e-5);
        let lin = Easing::Piecewise(vec![[0.0, 0.0], [0.75, 0.25], [1.0, 1.0]]);
        assert!((lin.output(0.375, 1e-6) - 0.125).abs() < 1e-6);
        assert!((lin.output(0.875, 1e-6) - 0.625).abs() < 1e-6);
        assert_eq!(
            Easing::Steps(4, StepPosition::JumpStart).output(0.0, 1e-6),
            0.25
        );
        assert_eq!(
            Easing::Steps(4, StepPosition::JumpBoth).output(0.5, 1e-6),
            0.6
        );
        assert_eq!(
            Easing::Steps(5, StepPosition::JumpNone).output(1.0, 1e-6),
            1.0
        );
    }

    #[test]
    fn transform_lists_blend_function_by_function() {
        let mut a = spin(Direction::Normal, 1.0);
        a.values = AnimatedValues::Transform {
            lists: vec![
                vec![],
                vec![
                    TransformFn::Translate(20.0, 0.0),
                    TransformFn::Rotate(std::f32::consts::PI),
                ],
            ],
            pre: [1.0, 0.0, 0.0, 1.0, -5.0, -5.0],
            post: [1.0, 0.0, 0.0, 1.0, 5.0, 5.0],
        };
        assert!(a.is_valid());
        let Some(Sample::Transform(m)) = a.sample(2.0) else {
            panic!()
        };
        // halfway: translate(10px) rotate(90deg) about (5, 5)
        let want = Matrix::translation(-5.0, -5.0)
            .then(&Matrix::rotation(std::f32::consts::FRAC_PI_2))
            .then(&Matrix::translation(10.0, 0.0))
            .then(&Matrix::translation(5.0, 5.0));
        for (x, y) in [
            (m.m11, want.m11),
            (m.m12, want.m12),
            (m.dx, want.dx),
            (m.dy, want.dy),
        ] {
            assert!((x - y).abs() < 1e-4, "{m:?} {want:?}");
        }
        // lists that would need a matrix decomposition are not taken
        if let AnimatedValues::Transform { lists, .. } = &mut a.values {
            lists[0] = vec![TransformFn::Scale(2.0, 2.0)];
        }
        assert!(!a.is_valid());
    }

    #[test]
    fn invalid_animations_are_refused() {
        let ok = spin(Direction::Normal, 1.0);
        let bad: [fn(&mut LayerAnimation); 12] = [
            |a| a.duration = 0.0,
            |a| a.playback_rate = 0.0,
            |a| a.delay = f64::INFINITY,
            |a| a.iteration_start = -1.0,
            |a| a.epsilon = Some(0.0),
            |a| a.easing = Easing::Steps(0, StepPosition::JumpStart),
            |a| a.start = f64::NAN,
            |a| a.iterations = -1.0,
            |a| a.offsets = vec![0.0, 0.5],
            |a| a.offsets = vec![0.0],
            |a| a.easings = vec![Easing::Linear],
            |a| a.easings[0] = Easing::CubicBezier([2.0, 0.0, 0.0, 1.0]),
        ];
        for (i, change) in bad.iter().enumerate() {
            let mut a = ok.clone();
            change(&mut a);
            assert!(!a.is_valid(), "{i}");
        }
    }

    #[test]
    fn a_marked_state_has_its_own_steps_written() {
        let now = Ticks::now();
        let mut a = spin(Direction::Normal, f64::INFINITY);
        a.origin = now.before(std::time::Duration::from_millis(2000));
        let state = LayerState(vec![
            StateOp::Opacity(0.5),
            StateOp::Transform([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
            StateOp::Opacity(1.0),
            StateOp::Animated(7),
            StateOp::Clip(crate::Rect::new(0.0, 0.0, 4.0, 4.0)),
        ]);
        let s = sampled_state(&state, &[a.clone()], now).unwrap();
        // (2 s on the timeline: halfway through the first iteration)
        let StateOp::Opacity(o) = s.0[2] else {
            panic!()
        };
        assert!((o - 0.5).abs() < 0.01, "{o}");
        assert_eq!(s.0[0], StateOp::Opacity(0.5));
        assert_eq!(s.0[4], state.0[4]);
        // another box's animation leaves it as painted
        a.element = 8;
        assert_eq!(sampled_state(&state, &[a], now), None);
    }

    /// A box's own filter step, after its transform and opacity: its filter animation writes it
    /// (`none` to `grayscale(1) invert(.5)`, halfway: the missing functions from their initial
    /// values), and its opacity animation writes the opacity step past it.
    #[test]
    fn a_marked_filter_step_is_written() {
        use crate::{ColorFilters, FilterOp};
        let now = Ticks::now();
        let mut fade = spin(Direction::Normal, f64::INFINITY);
        fade.origin = now.before(std::time::Duration::from_millis(2000));
        let mut gray = fade.clone();
        gray.values = AnimatedValues::Filter(vec![
            ColorFilters::default(),
            ColorFilters::of(&[FilterOp::Grayscale(1.0), FilterOp::Invert(0.5)]).unwrap(),
        ]);
        assert!(gray.is_valid());
        let state = LayerState(vec![
            StateOp::Transform([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
            StateOp::Opacity(1.0),
            StateOp::Filter(ColorFilters::default()),
            StateOp::Animated(7),
        ]);
        let s = sampled_state(&state, &[gray, fade], now).unwrap();
        let StateOp::Filter(f) = s.0[2] else { panic!() };
        assert_eq!(
            f.ops(),
            vec![FilterOp::Grayscale(0.5), FilterOp::Invert(0.25)]
        );
        let StateOp::Opacity(o) = s.0[1] else {
            panic!()
        };
        assert!((o - 0.5).abs() < 0.01, "{o}");
    }
}
