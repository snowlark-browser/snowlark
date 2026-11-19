// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Animations 1 §4.6 and §4.7: an animation effect's timing properties and the calculations that
//! turn its local time into an iteration progress (phases, active time, overall progress, simple
//! iteration progress, current iteration, directed and transformed progress). Times are in
//! milliseconds, as the programming interface gives them.

use super::easing::Easing;

/// `FillMode` (§6.5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillMode {
    None,
    Forwards,
    Backwards,
    Both,
    Auto,
}

/// `PlaybackDirection` (§6.5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackDirection {
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

/// §4.6.6: the phase of an animation effect; `Idle` when its local time is unresolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Before,
    Active,
    After,
    Idle,
}

/// The specified timing properties of an animation effect (the `EffectTiming` members).
#[derive(Clone, Debug, PartialEq)]
pub struct Timing {
    /// The start delay.
    pub delay: f64,
    pub end_delay: f64,
    pub fill: FillMode,
    pub iteration_start: f64,
    /// The iteration count (may be +∞).
    pub iterations: f64,
    /// The iteration duration; `None` is `auto` (zero in this level).
    pub duration: Option<f64>,
    pub direction: PlaybackDirection,
    pub easing: Easing,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            delay: 0.0,
            end_delay: 0.0,
            fill: FillMode::Auto,
            iteration_start: 0.0,
            iterations: 1.0,
            duration: None,
            direction: PlaybackDirection::Normal,
            easing: Easing::linear(),
        }
    }
}

impl Timing {
    /// The iteration duration used in calculations (`auto` is zero).
    pub fn iteration_duration(&self) -> f64 {
        self.duration.unwrap_or(0.0)
    }

    /// §4.6.5.2: iteration duration × iteration count, zero when either is zero.
    pub fn active_duration(&self) -> f64 {
        let d = self.iteration_duration();
        if d == 0.0 || self.iterations == 0.0 {
            0.0
        } else {
            d * self.iterations
        }
    }

    /// §4.6.5.3: max(start delay + active duration + end delay, 0).
    pub fn end_time(&self) -> f64 {
        (self.delay + self.active_duration() + self.end_delay).max(0.0)
    }

    /// The fill mode used in calculations: `auto` is `none` for keyframe effects (§6.5.1).
    pub fn computed_fill(&self) -> FillMode {
        match self.fill {
            FillMode::Auto => FillMode::None,
            f => f,
        }
    }

    /// §4.6.6: the phase at `local_time`. `backwards` is the animation direction (the associated
    /// animation's playback rate is negative); `inclusive` is the endpoint-inclusive active interval
    /// flag (commitStyles).
    pub fn phase(&self, local_time: Option<f64>, backwards: bool, inclusive: bool) -> Phase {
        let Some(t) = local_time else {
            return Phase::Idle;
        };
        let end = self.end_time();
        let before_active = self.delay.min(end).max(0.0);
        let active_after = (self.delay + self.active_duration()).min(end).max(0.0);
        if t < before_active || (backwards && !inclusive && t == before_active) {
            Phase::Before
        } else if t > active_after || (!backwards && !inclusive && t == active_after) {
            Phase::After
        } else {
            Phase::Active
        }
    }

    /// §4.7: everything the timing model derives from the local time.
    pub fn compute(&self, local_time: Option<f64>, backwards: bool) -> ComputedTiming {
        self.compute_with(local_time, backwards, false)
    }

    /// [`Timing::compute`] with the endpoint-inclusive active interval flag.
    pub fn compute_with(
        &self,
        local_time: Option<f64>,
        backwards: bool,
        inclusive: bool,
    ) -> ComputedTiming {
        let phase = self.phase(local_time, backwards, inclusive);
        let active_duration = self.active_duration();
        let fill = self.computed_fill();
        // §4.7.2 Calculating the active time.
        let active_time = match (phase, local_time) {
            (Phase::Before, Some(t)) => matches!(fill, FillMode::Backwards | FillMode::Both)
                .then(|| (t - self.delay).max(0.0)),
            (Phase::Active, Some(t)) => Some(t - self.delay),
            (Phase::After, Some(t)) => matches!(fill, FillMode::Forwards | FillMode::Both)
                .then(|| (t - self.delay).min(active_duration).max(0.0)),
            _ => None,
        };
        // §4.7.3 Calculating the overall progress.
        let duration = self.iteration_duration();
        let overall = active_time.map(|at| {
            let p = if duration == 0.0 {
                if phase == Phase::Before {
                    0.0
                } else {
                    self.iterations
                }
            } else {
                at / duration
            };
            p + self.iteration_start
        });
        // §4.7.4 Calculating the simple iteration progress.
        let simple = overall.map(|o| {
            let mut s = if o.is_infinite() {
                self.iteration_start % 1.0
            } else {
                o % 1.0
            };
            if s == 0.0
                && matches!(phase, Phase::Active | Phase::After)
                && active_time == Some(active_duration)
                && self.iterations != 0.0
            {
                s = 1.0;
            }
            s
        });
        // §4.7.5 Calculating the current iteration index.
        let current_iteration = match (active_time, overall, simple) {
            (Some(_), Some(o), Some(s)) => {
                Some(if phase == Phase::After && self.iterations.is_infinite() {
                    f64::INFINITY
                } else if s == 1.0 {
                    o.floor() - 1.0
                } else {
                    o.floor()
                })
            }
            _ => None,
        };
        // §4.7.6 Calculating the directed progress.
        let forwards = match self.direction {
            PlaybackDirection::Normal => true,
            PlaybackDirection::Reverse => false,
            PlaybackDirection::Alternate | PlaybackDirection::AlternateReverse => {
                let mut d = current_iteration.unwrap_or(0.0);
                if self.direction == PlaybackDirection::AlternateReverse {
                    d += 1.0;
                }
                d.is_infinite() || d % 2.0 == 0.0
            }
        };
        let directed = simple.map(|s| if forwards { s } else { 1.0 - s });
        // §4.7.7 Calculating the transformed progress.
        let before_flag =
            (phase == Phase::Before && forwards) || (phase == Phase::After && !forwards);
        let progress = directed.map(|d| self.easing.apply(d, before_flag));
        ComputedTiming {
            phase,
            local_time,
            active_time,
            overall_progress: overall,
            simple_progress: simple,
            current_iteration,
            directed_progress: directed,
            progress,
            before_flag,
        }
    }
}

/// The results of the §4.7 calculations at one local time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ComputedTiming {
    pub phase: Phase,
    pub local_time: Option<f64>,
    pub active_time: Option<f64>,
    pub overall_progress: Option<f64>,
    pub simple_progress: Option<f64>,
    pub current_iteration: Option<f64>,
    pub directed_progress: Option<f64>,
    /// The transformed progress: the iteration progress (§4.7.8).
    pub progress: Option<f64>,
    /// The before flag the easing function was evaluated with.
    pub before_flag: bool,
}

impl ComputedTiming {
    /// §4.6.6 "in effect": the active time is resolved.
    pub fn in_effect(&self) -> bool {
        self.active_time.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing(duration: f64) -> Timing {
        Timing {
            duration: Some(duration),
            ..Timing::default()
        }
    }

    #[test]
    fn active_duration_and_end_time() {
        let mut t = timing(1000.0);
        t.iterations = 2.5;
        assert_eq!(t.active_duration(), 2500.0);
        t.delay = 100.0;
        t.end_delay = -3000.0;
        assert_eq!(t.end_time(), 0.0);
        // Infinity × 0 is zero, not NaN.
        let mut t = timing(0.0);
        t.iterations = f64::INFINITY;
        assert_eq!(t.active_duration(), 0.0);
        assert_eq!(Timing::default().active_duration(), 0.0);
    }

    #[test]
    fn phases_at_boundaries() {
        let mut t = timing(1000.0);
        t.delay = 500.0;
        assert_eq!(t.phase(None, false, false), Phase::Idle);
        assert_eq!(t.phase(Some(499.0), false, false), Phase::Before);
        // Forwards, the start of the active interval is in it; backwards it is not.
        assert_eq!(t.phase(Some(500.0), false, false), Phase::Active);
        assert_eq!(t.phase(Some(500.0), true, false), Phase::Before);
        // Forwards, the end is in the after phase; backwards it is in the active phase.
        assert_eq!(t.phase(Some(1500.0), false, false), Phase::After);
        assert_eq!(t.phase(Some(1500.0), true, false), Phase::Active);
        // The endpoint-inclusive flag keeps both ends active.
        assert_eq!(t.phase(Some(1500.0), false, true), Phase::Active);
        // A negative end delay cuts the active interval short.
        t.end_delay = -600.0;
        assert_eq!(t.phase(Some(1000.0), false, false), Phase::After);
    }

    #[test]
    fn active_time_follows_fill() {
        let mut t = timing(1000.0);
        t.delay = 500.0;
        assert_eq!(t.compute(Some(100.0), false).active_time, None);
        t.fill = FillMode::Backwards;
        assert_eq!(t.compute(Some(100.0), false).active_time, Some(0.0));
        assert_eq!(t.compute(Some(2000.0), false).active_time, None);
        t.fill = FillMode::Both;
        assert_eq!(t.compute(Some(2000.0), false).active_time, Some(1000.0));
        assert_eq!(t.compute(Some(700.0), false).active_time, Some(200.0));
        // `auto` is `none`.
        t.fill = FillMode::Auto;
        assert_eq!(t.compute(Some(2000.0), false).active_time, None);
    }

    #[test]
    fn iteration_progress_and_index() {
        let mut t = timing(1000.0);
        t.iterations = 3.0;
        t.fill = FillMode::Forwards;
        let c = t.compute(Some(1250.0), false);
        assert_eq!(c.progress, Some(0.25));
        assert_eq!(c.current_iteration, Some(1.0));
        // At the end of the last iteration the fill holds its end, not the start of the next.
        let c = t.compute(Some(3000.0), false);
        assert_eq!(c.phase, Phase::After);
        assert_eq!(c.progress, Some(1.0));
        assert_eq!(c.current_iteration, Some(2.0));
        // An iteration boundary inside the active interval starts the next iteration.
        let c = t.compute(Some(2000.0), false);
        assert_eq!(c.progress, Some(0.0));
        assert_eq!(c.current_iteration, Some(2.0));
        // Iteration start shifts progress and index without changing the active duration.
        t.iteration_start = 0.5;
        let c = t.compute(Some(0.0), false);
        assert_eq!(c.progress, Some(0.5));
        assert_eq!(c.current_iteration, Some(0.0));
        let c = t.compute(Some(3000.0), false);
        assert_eq!(c.progress, Some(0.5));
        assert_eq!(c.current_iteration, Some(3.0));
    }

    #[test]
    fn zero_duration() {
        let mut t = timing(0.0);
        t.fill = FillMode::Both;
        t.iterations = 2.0;
        let c = t.compute(Some(-1.0), false);
        assert_eq!(c.phase, Phase::Before);
        assert_eq!(c.progress, Some(0.0));
        assert_eq!(c.current_iteration, Some(0.0));
        let c = t.compute(Some(0.0), false);
        assert_eq!(c.phase, Phase::After);
        assert_eq!(c.progress, Some(1.0));
        assert_eq!(c.current_iteration, Some(1.0));
        t.iterations = f64::INFINITY;
        let c = t.compute(Some(0.0), false);
        assert_eq!(c.current_iteration, Some(f64::INFINITY));
        assert_eq!(c.progress, Some(1.0));
        // No iterations at all: the fill is the start of the first.
        t.iterations = 0.0;
        let c = t.compute(Some(0.0), false);
        assert_eq!(c.progress, Some(0.0));
        assert_eq!(c.current_iteration, Some(0.0));
    }

    #[test]
    fn directions() {
        let mut t = timing(1000.0);
        t.iterations = 4.0;
        t.direction = PlaybackDirection::Reverse;
        assert_eq!(t.compute(Some(250.0), false).progress, Some(0.75));
        t.direction = PlaybackDirection::Alternate;
        assert_eq!(t.compute(Some(250.0), false).progress, Some(0.25));
        assert_eq!(t.compute(Some(1250.0), false).progress, Some(0.75));
        t.direction = PlaybackDirection::AlternateReverse;
        assert_eq!(t.compute(Some(250.0), false).progress, Some(0.75));
        assert_eq!(t.compute(Some(1250.0), false).progress, Some(0.25));
        // Infinite iterations of a zero-length effect play forwards.
        let mut z = timing(0.0);
        z.iterations = f64::INFINITY;
        z.fill = FillMode::Forwards;
        z.direction = PlaybackDirection::Alternate;
        assert_eq!(z.compute(Some(10.0), false).progress, Some(1.0));
    }

    #[test]
    fn before_flag_reaches_step_easing() {
        let mut t = timing(1000.0);
        t.delay = 1000.0;
        t.fill = FillMode::Both;
        t.easing = Easing::parse("steps(2, start)").unwrap();
        // Before phase, going forwards: the before flag drops the jump at 0.
        let c = t.compute(Some(0.0), false);
        assert!(c.before_flag);
        assert_eq!(c.progress, Some(0.0));
        // Active at 0: jump-start already jumped.
        assert_eq!(t.compute(Some(1000.0), false).progress, Some(0.5));
    }
}
