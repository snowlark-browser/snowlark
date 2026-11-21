// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS animations and transitions seen through Web Animations: their events (CSS Animations 2 §4.1
//! and CSS Transitions 2 §5.2: `animationstart`, `animationiteration`, `animationend`,
//! `animationcancel`, `transitionrun`, `transitionstart`, `transitionend`, `transitioncancel`) from the
//! phases of stylo's animations at each "update animations and send events", and the timing the page's
//! `CSSAnimation`/`CSSTransition` objects show.
//!
//! Stylo keeps an animation's start as `started_at` (moved by each iteration) and its iteration index;
//! a transition's as `start_time` (its delay included). The Web Animations start time (local time 0)
//! is derived from them: `started_at - delay - iteration × duration`.

use rustc_hash::FxHashMap;
use style::animation::{Animation, AnimationState, KeyframesIterationState, Transition};
use style::properties::longhands::animation_direction::computed_value::single_value::T as AnimationDirection;
use style::properties::longhands::animation_fill_mode::computed_value::single_value::T as AnimationFillMode;
use style::selector_parser::PseudoElement;
use style_traits::ToCss;

use super::timing::{FillMode, Phase, PlaybackDirection, Timing};
use snowlark_dom::NodeId;

/// The kinds of CSS animation and transition events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssEventKind {
    AnimationStart,
    AnimationIteration,
    AnimationEnd,
    AnimationCancel,
    TransitionRun,
    TransitionStart,
    TransitionEnd,
    TransitionCancel,
}

impl CssEventKind {
    /// The event type.
    pub fn type_name(self) -> &'static str {
        match self {
            CssEventKind::AnimationStart => "animationstart",
            CssEventKind::AnimationIteration => "animationiteration",
            CssEventKind::AnimationEnd => "animationend",
            CssEventKind::AnimationCancel => "animationcancel",
            CssEventKind::TransitionRun => "transitionrun",
            CssEventKind::TransitionStart => "transitionstart",
            CssEventKind::TransitionEnd => "transitionend",
            CssEventKind::TransitionCancel => "transitioncancel",
        }
    }

    pub fn is_transition(self) -> bool {
        matches!(
            self,
            CssEventKind::TransitionRun
                | CssEventKind::TransitionStart
                | CssEventKind::TransitionEnd
                | CssEventKind::TransitionCancel
        )
    }
}

/// Identifies one CSS animation (by element, pseudo-element, name and occurrence of the name) or
/// one CSS transition (by element, pseudo-element, property and start time).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CssKey {
    pub element: NodeId,
    pub pseudo: Option<PseudoElement>,
    pub transition: bool,
    /// The animation name or the transitioned property.
    pub name: String,
    /// The occurrence of the name among the element's animations, or the bits of the transition's
    /// start time.
    pub index: u64,
}

impl CssKey {
    /// The pseudo-element selector (`::before`) or the empty string.
    pub fn pseudo_text(&self) -> String {
        self.pseudo
            .as_ref()
            .map(|p| {
                let mut s = String::new();
                let _ = cssparser::ToCss::to_css(p, &mut s);
                s
            })
            .unwrap_or_default()
    }
}

/// An event to dispatch at the element (or its pseudo-element's originating element).
#[derive(Clone, Debug)]
pub struct CssAnimationEvent {
    pub key: CssKey,
    pub kind: CssEventKind,
    /// `elapsedTime` (s).
    pub elapsed: f64,
    /// The scheduled event time (s on the document timeline).
    pub scheduled: f64,
}

/// What the page needs to show a CSS animation or transition as an `Animation`: its timing (ms) and
/// its start time on the document timeline (ms, `None` while it waits), or the hold time when paused.
#[derive(Clone, Debug)]
pub struct CssTiming {
    pub key: CssKey,
    pub timing: Timing,
    pub start_time: Option<f64>,
    pub hold_time: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tracked {
    /// Its place in the last scan, which goes through an element's animations in list order.
    position: u32,
    phase: Phase,
    iteration: f64,
    /// Paused (its phase does not change by itself).
    paused: bool,
    /// The active time (s) as of the scan, clamped to the active interval: a cancel's elapsedTime.
    active_time: f64,
}

/// A time stylo keeps (seconds, from single-precision computed values) to the microsecond: `0.1s`
/// reads 0.1, not 0.10000000149.
fn seconds(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

fn iteration_of(a: &Animation) -> (f64, f64) {
    match a.iteration_state {
        KeyframesIterationState::Finite(cur, max) => (cur, max),
        KeyframesIterationState::Infinite(cur) => (cur, f64::INFINITY),
    }
}

/// An animation's Web Animations timing (ms) and start time (s).
pub(crate) fn animation_timing(a: &Animation) -> (Timing, f64) {
    let (cur, max) = iteration_of(a);
    let timing = Timing {
        delay: seconds(a.delay) * 1000.0,
        end_delay: 0.0,
        fill: match a.fill_mode {
            AnimationFillMode::None => FillMode::None,
            AnimationFillMode::Forwards => FillMode::Forwards,
            AnimationFillMode::Backwards => FillMode::Backwards,
            AnimationFillMode::Both => FillMode::Both,
        },
        iteration_start: 0.0,
        iterations: max,
        duration: Some(seconds(a.duration) * 1000.0),
        direction: match a.direction {
            AnimationDirection::Normal => PlaybackDirection::Normal,
            AnimationDirection::Reverse => PlaybackDirection::Reverse,
            AnimationDirection::Alternate => PlaybackDirection::Alternate,
            AnimationDirection::AlternateReverse => PlaybackDirection::AlternateReverse,
        },
        easing: super::Easing::linear(),
    };
    let start = a.started_at - a.delay - cur * a.duration;
    (timing, start)
}

/// The local time (ms) of an animation at `now` (s): paused ones hold theirs.
pub(crate) fn animation_local(a: &Animation, start: f64, now: f64) -> f64 {
    match a.state {
        AnimationState::Paused(p) => {
            let (cur, _) = iteration_of(a);
            (a.delay + (cur + p) * a.duration) * 1000.0
        }
        _ => (now - start) * 1000.0,
    }
}

pub(crate) fn transition_timing(t: &Transition) -> (Timing, f64) {
    let timing = Timing {
        delay: seconds(t.delay) * 1000.0,
        duration: Some(seconds(t.property_animation.duration) * 1000.0),
        ..Timing::default()
    };
    (timing, t.start_time - t.delay)
}

/// Times this close (ms) are the same time where an event's phase is decided: a running
/// animation's local time is `now - start`, and that difference rounds, so a seek to an iteration
/// boundary or `finish()` could read back a hair before it and its event wait for the next
/// frame that moves the time, or never come for a paused or finished animation. One microsecond,
/// as precise as the clocks are.
const TIME_TOLERANCE: f64 = 0.001;

/// `local` (ms), or the iteration boundary or end of `timing` within [`TIME_TOLERANCE`] of it.
fn snapped_to_boundary(timing: &Timing, local: f64) -> f64 {
    let Some(d) = timing.duration.filter(|d| *d > 0.0) else {
        return local;
    };
    let end = timing.delay + timing.iterations * d;
    if end.is_finite() && (local - end).abs() <= TIME_TOLERANCE {
        return end;
    }
    let k = ((local - timing.delay) / d).round();
    if k < 0.0 || k > timing.iterations {
        return local;
    }
    let boundary = timing.delay + k * d;
    if (local - boundary).abs() <= TIME_TOLERANCE {
        boundary
    } else {
        local
    }
}

/// The phase and current iteration of `timing` at `local` (ms).
fn phase_at(timing: &Timing, local: f64) -> (Phase, f64) {
    let both = Timing {
        fill: FillMode::Both,
        ..timing.clone()
    };
    let c = both.compute(Some(snapped_to_boundary(timing, local)), false);
    (c.phase, c.current_iteration.unwrap_or(0.0))
}

/// The keys and timings of the CSS animations and transitions of one element's set.
pub(crate) fn entries<'a>(
    element: NodeId,
    pseudo: &'a Option<PseudoElement>,
    animations: &'a [Animation],
    transitions: &'a [Transition],
    now: f64,
) -> impl Iterator<Item = (CssKey, Timing, f64, f64, bool)> + 'a {
    let mut seen: FxHashMap<&'a str, u64> = FxHashMap::default();
    let a = animations.iter().filter_map(move |a| {
        let name: &str = &a.name;
        let n = seen.entry(name).or_insert(0);
        let index = *n;
        *n += 1;
        if matches!(a.state, AnimationState::Canceled) {
            return None;
        }
        let (timing, start) = animation_timing(a);
        let local = animation_local(a, start, now);
        let paused = matches!(a.state, AnimationState::Paused(_));
        Some((
            CssKey {
                element,
                pseudo: *pseudo,
                transition: false,
                name: name.to_owned(),
                index,
            },
            timing,
            start,
            local,
            paused,
        ))
    });
    let t = transitions.iter().filter_map(move |t| {
        if matches!(t.state, AnimationState::Canceled) {
            return None;
        }
        let (timing, start) = transition_timing(t);
        let mut name = String::new();
        let _ = t
            .property_animation
            .property_id()
            .to_css(&mut style_traits::CssWriter::new(&mut name));
        Some((
            CssKey {
                element,
                pseudo: *pseudo,
                transition: true,
                name,
                index: t.created_at.to_bits(),
            },
            timing,
            start,
            (now - start) * 1000.0,
            false,
        ))
    });
    a.chain(t)
}

/// The timings of one element's set (see [`CssTiming`]).
pub(crate) fn timings(
    element: NodeId,
    pseudo: &Option<PseudoElement>,
    animations: &[Animation],
    transitions: &[Transition],
    now: f64,
    out: &mut Vec<CssTiming>,
) {
    for (key, timing, start, local, paused) in
        entries(element, pseudo, animations, transitions, now)
    {
        out.push(CssTiming {
            key,
            timing,
            start_time: (!paused).then_some(start * 1000.0),
            hold_time: paused.then_some(local),
        });
    }
}

/// Where a CSS animation put at local time `t` (s) is, from `t` alone: its current iteration
/// (within its iteration count `max`) and how far into that iteration it is (s; negative in its
/// delay). Not by way of the timeline time: `(now - t) + delay + cur * d` rounds, so a seek to the
/// start of an iteration could read back as the end of the one before (its paused progress a
/// little under 0), and its `animationiteration` never come.
fn seek_position(t: f64, delay: f64, d: f64, max: f64) -> (f64, f64) {
    let active = t - delay;
    let mut cur = if active <= 0.0 || d <= 0.0 {
        0.0
    } else {
        (active / d).floor()
    };
    if max.is_finite() && max > 0.0 {
        cur = cur.min(max - 1.0).max(0.0);
    }
    (cur, active - cur * d)
}

/// Puts a CSS animation at local time `local` (ms), paused or playing, at timeline time `now` (s):
/// script controls it through its `CSSAnimation` object.
pub(crate) fn control_animation(a: &mut Animation, now: f64, local: f64, paused: bool) {
    let t = local / 1000.0;
    let d = a.duration;
    let (_, max) = iteration_of(a);
    let (cur, into) = seek_position(t, a.delay, d, max);
    a.started_at = now - into;
    a.iteration_state = match a.iteration_state {
        KeyframesIterationState::Finite(_, m) => KeyframesIterationState::Finite(cur, m),
        KeyframesIterationState::Infinite(_) => KeyframesIterationState::Infinite(cur),
    };
    let flip = matches!(
        a.direction,
        AnimationDirection::Alternate | AnimationDirection::AlternateReverse
    ) && cur % 2.0 == 1.0;
    let base = match a.direction {
        AnimationDirection::Reverse | AnimationDirection::AlternateReverse => {
            AnimationDirection::Reverse
        }
        _ => AnimationDirection::Normal,
    };
    a.current_direction = match (base, flip) {
        (AnimationDirection::Normal, true) => AnimationDirection::Reverse,
        (AnimationDirection::Reverse, true) => AnimationDirection::Normal,
        (b, _) => b,
    };
    a.state = if paused {
        AnimationState::Paused(if d > 0.0 { into / d } else { 0.0 })
    } else if a.started_at > now {
        AnimationState::Pending
    } else {
        AnimationState::Running
    };
}

/// Puts a CSS transition at local time `local` (ms) at timeline time `now` (s).
pub(crate) fn control_transition(t: &mut Transition, now: f64, local: f64) {
    t.start_time = now - local / 1000.0 + t.delay;
    t.state = if t.start_time > now {
        AnimationState::Pending
    } else {
        AnimationState::Running
    };
}

/// The event tracker: each CSS animation's and transition's phase as of the last scan.
#[derive(Default)]
pub struct CssEventTracker {
    tracked: FxHashMap<CssKey, Tracked>,
    next: FxHashMap<CssKey, Tracked>,
}

impl CssEventTracker {
    /// One entry seen at this scan: the events its phase change since the last scan gives.
    pub(crate) fn see(
        &mut self,
        key: CssKey,
        timing: &Timing,
        start: f64,
        local: f64,
        paused: bool,
        out: &mut Vec<CssAnimationEvent>,
    ) {
        let (phase, iteration) = phase_at(timing, local);
        let previous = self.tracked.get(&key).copied();
        let active = timing.active_duration();
        let begin = (-timing.delay).min(active).max(0.0) / 1000.0;
        let end = active / 1000.0;
        let at = |t: f64| start + t / 1000.0;
        let mut push = |kind: CssEventKind, elapsed: f64, scheduled: f64| {
            out.push(CssAnimationEvent {
                key: key.clone(),
                kind,
                elapsed,
                scheduled,
            });
        };
        let (start_kind, iter_kind, end_kind) = if key.transition {
            (
                CssEventKind::TransitionStart,
                None,
                CssEventKind::TransitionEnd,
            )
        } else {
            (
                CssEventKind::AnimationStart,
                Some(CssEventKind::AnimationIteration),
                CssEventKind::AnimationEnd,
            )
        };
        let begin_at = at(timing.delay.max(0.0));
        let end_at = at(timing.delay + active);
        match previous {
            None => {
                if key.transition {
                    push(CssEventKind::TransitionRun, begin, start);
                }
                match phase {
                    Phase::Active => push(start_kind, begin, begin_at),
                    Phase::After => {
                        push(start_kind, begin, begin_at);
                        push(end_kind, end, end_at);
                    }
                    _ => {}
                }
            }
            Some(p) => match (p.phase, phase) {
                (Phase::Before | Phase::Idle, Phase::Active) => push(start_kind, begin, begin_at),
                (Phase::Before | Phase::Idle, Phase::After) => {
                    push(start_kind, begin, begin_at);
                    push(end_kind, end, end_at);
                }
                (Phase::Active, Phase::Active) => {
                    if let Some(kind) = iter_kind {
                        if iteration != p.iteration {
                            let d = timing.iteration_duration();
                            push(
                                kind,
                                iteration * d / 1000.0,
                                at(timing.delay + iteration * d),
                            );
                        }
                    }
                }
                (Phase::Active, Phase::After) => push(end_kind, end, end_at),
                // Playing backwards (CSS Animations 2 §4.1): the interval's ends swap.
                (Phase::Active, Phase::Before) => push(end_kind, begin, begin_at),
                (Phase::After, Phase::Active) => push(start_kind, end, end_at),
                (Phase::After, Phase::Before) => {
                    push(start_kind, end, end_at);
                    push(end_kind, begin, begin_at);
                }
                _ => {}
            },
        }
        let position = self.next.len() as u32;
        self.next.insert(
            key,
            Tracked {
                position,
                phase,
                iteration,
                paused,
                active_time: (local - timing.delay).clamp(0.0, active) / 1000.0,
            },
        );
    }

    /// The active time of an entry that is about to be canceled (its element stops displaying, or
    /// script cancels it), for its cancel event's elapsedTime.
    pub(crate) fn note_active(&mut self, key: &CssKey, timing: &Timing, local: f64) {
        if let Some(t) = self.tracked.get_mut(key) {
            t.active_time = (local - timing.delay).clamp(0.0, timing.active_duration()) / 1000.0;
        }
    }

    /// Whether an entry seen at the last scan that `keep` accepts has an event to come (it has not
    /// ended).
    pub(crate) fn unfinished_where(&self, keep: impl Fn(&CssKey) -> bool) -> bool {
        self.tracked
            .iter()
            .any(|(k, t)| !t.paused && !matches!(t.phase, Phase::After) && keep(k))
    }

    /// Ends a scan: entries that were not seen are gone (removed by a style change, or their element
    /// left): those that had not ended are canceled.
    pub(crate) fn finish(&mut self, now: f64, out: &mut Vec<CssAnimationEvent>) {
        // Cancel events go in composite order (CSS Animations 2 §4.2): an earlier position in the
        // animation list comes first, whatever order the keys hash in.
        let mut gone: Vec<(CssKey, Tracked)> = self
            .tracked
            .drain()
            .filter(|(key, t)| !self.next.contains_key(key) && !matches!(t.phase, Phase::After))
            .collect();
        gone.sort_by_key(|(_, t)| t.position);
        for (key, t) in gone {
            let kind = if key.transition {
                CssEventKind::TransitionCancel
            } else {
                CssEventKind::AnimationCancel
            };
            out.push(CssAnimationEvent {
                key,
                kind,
                elapsed: t.active_time,
                scheduled: now,
            });
        }
        std::mem::swap(&mut self.tracked, &mut self.next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(transition: bool) -> CssKey {
        CssKey {
            element: NodeId::from_bits(1 << 32 | 1).unwrap(),
            pseudo: None,
            transition,
            name: "a".into(),
            index: 0,
        }
    }

    fn timing(delay: f64, duration: f64, iterations: f64) -> Timing {
        Timing {
            delay,
            duration: Some(duration),
            iterations,
            ..Timing::default()
        }
    }

    fn kinds(out: &[CssAnimationEvent]) -> Vec<CssEventKind> {
        out.iter().map(|e| e.kind).collect()
    }

    #[test]
    fn cancel_events_follow_the_animation_list_whatever_the_keys_hash_to() {
        let mut t = CssEventTracker::default();
        let mut out = Vec::new();
        let tm = timing(0.0, 1000.0, 1.0);
        let names: Vec<String> = (0..32).map(|i| format!("anim{i}")).collect();
        let keyed = |name: &String| CssKey {
            name: name.clone(),
            ..key(false)
        };
        for name in &names {
            t.see(keyed(name), &tm, 0.0, 100.0, false, &mut out);
        }
        t.finish(0.1, &mut out);
        out.clear();
        // All but the last are removed by a style change.
        t.see(keyed(&names[31]), &tm, 0.0, 200.0, false, &mut out);
        t.finish(0.2, &mut out);
        let canceled: Vec<&str> = out
            .iter()
            .filter(|e| e.kind == CssEventKind::AnimationCancel)
            .map(|e| e.key.name.as_str())
            .collect();
        let expected: Vec<&str> = names[..31].iter().map(String::as_str).collect();
        assert_eq!(canceled, expected);
    }

    /// A seek to the start of an iteration is at that iteration, exactly: a paused animation's
    /// local time, `delay + (cur + progress) * duration`, reads it back whatever the timeline time
    /// (by way of the timeline time, 0.0263 s read 99999.99999999999 ms for a seek to 100 s).
    #[test]
    fn a_seek_to_an_iteration_start_reads_back_exactly() {
        for (t, delay, d, max, cur) in [
            (100.0, 0.0, 100.0, 2.0, 1.0),
            (3.0, 1.0, 0.5, f64::INFINITY, 4.0),
            (250.0, 50.0, 100.0, 3.0, 2.0),
        ] {
            let (c, into) = seek_position(t, delay, d, max);
            assert_eq!((c, into), (cur, 0.0), "{t} {delay} {d}");
            assert_eq!(delay + (c + into / d) * d, t);
        }
        // In its delay: the first iteration, before its start.
        assert_eq!(seek_position(0.5, 1.0, 2.0, 1.0), (0.0, -0.5));
        // Past its last iteration: in the last one.
        assert_eq!(seek_position(9.0, 0.0, 2.0, 2.0), (1.0, 7.0));
    }

    /// A local time a rounding error short of an iteration boundary or the end is at it (a running
    /// animation's is `now - start`): its iteration and end events come.
    #[test]
    fn events_come_at_a_local_time_a_rounding_error_short() {
        let mut t = CssEventTracker::default();
        let mut out = Vec::new();
        let tm = timing(0.0, 100_000.0, 2.0);
        t.see(key(false), &tm, 0.0, 50_000.0, false, &mut out);
        t.finish(0.05, &mut out);
        out.clear();
        t.see(key(false), &tm, 0.0, 99_999.999_999_999_99, false, &mut out);
        t.finish(0.1, &mut out);
        assert_eq!(kinds(&out), vec![CssEventKind::AnimationIteration]);
        out.clear();
        t.see(
            key(false),
            &tm,
            0.0,
            199_999.999_999_999_97,
            false,
            &mut out,
        );
        t.finish(0.2, &mut out);
        assert_eq!(kinds(&out), vec![CssEventKind::AnimationEnd]);
        // A time clearly before the boundary is not at it.
        assert_eq!(snapped_to_boundary(&tm, 99_999.9), 99_999.9);
    }

    #[test]
    fn animation_events_follow_phases() {
        let mut t = CssEventTracker::default();
        let mut out = Vec::new();
        let tm = timing(100.0, 1000.0, 2.0);
        // In its delay: nothing yet.
        t.see(key(false), &tm, 0.0, 50.0, false, &mut out);
        t.finish(0.05, &mut out);
        assert!(out.is_empty());
        t.see(key(false), &tm, 0.0, 200.0, false, &mut out);
        t.finish(0.2, &mut out);
        assert_eq!(kinds(&out), vec![CssEventKind::AnimationStart]);
        out.clear();
        t.see(key(false), &tm, 0.0, 1200.0, false, &mut out);
        t.finish(1.2, &mut out);
        assert_eq!(kinds(&out), vec![CssEventKind::AnimationIteration]);
        assert_eq!(out[0].elapsed, 1.0);
        out.clear();
        t.see(key(false), &tm, 0.0, 2500.0, false, &mut out);
        t.finish(2.5, &mut out);
        assert_eq!(kinds(&out), vec![CssEventKind::AnimationEnd]);
        assert_eq!(out[0].elapsed, 2.0);
        out.clear();
        // Gone after it ended: no cancel.
        t.finish(3.0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn backwards_and_cancel_times() {
        let mut t = CssEventTracker::default();
        let mut out = Vec::new();
        let tm = timing(0.0, 1000.0, 1.0);
        t.see(key(false), &tm, 0.0, 500.0, false, &mut out);
        t.finish(0.5, &mut out);
        out.clear();
        // Seeked back before the start: the end event says the interval's start.
        t.see(key(false), &tm, 0.0, -10.0, false, &mut out);
        t.finish(0.5, &mut out);
        assert_eq!(kinds(&out), vec![CssEventKind::AnimationEnd]);
        assert_eq!(out[0].elapsed, 0.0);
        out.clear();
        // Back in the active phase, then canceled at 300 ms.
        t.see(key(false), &tm, 0.0, 200.0, false, &mut out);
        t.finish(0.6, &mut out);
        out.clear();
        t.note_active(&key(false), &tm, 300.0);
        t.finish(0.7, &mut out);
        assert_eq!(kinds(&out), vec![CssEventKind::AnimationCancel]);
        assert_eq!(out[0].elapsed, 0.3);
    }

    #[test]
    fn transitions_run_then_cancel() {
        let mut t = CssEventTracker::default();
        let mut out = Vec::new();
        let tm = timing(500.0, 1000.0, 1.0);
        t.see(key(true), &tm, 0.0, 0.0, false, &mut out);
        t.finish(0.0, &mut out);
        assert_eq!(kinds(&out), vec![CssEventKind::TransitionRun]);
        out.clear();
        t.see(key(true), &tm, 0.0, 600.0, false, &mut out);
        t.finish(0.6, &mut out);
        assert_eq!(kinds(&out), vec![CssEventKind::TransitionStart]);
        out.clear();
        t.finish(0.7, &mut out);
        assert_eq!(kinds(&out), vec![CssEventKind::TransitionCancel]);
        // A transition whose whole run fits between two scans: run, start and end at once.
        let mut t = CssEventTracker::default();
        out.clear();
        t.see(
            key(true),
            &timing(0.0, 10.0, 1.0),
            0.0,
            20.0,
            false,
            &mut out,
        );
        assert_eq!(
            kinds(&out),
            vec![
                CssEventKind::TransitionRun,
                CssEventKind::TransitionStart,
                CssEventKind::TransitionEnd
            ]
        );
        // A negative delay starts it part-way: elapsedTime says how far.
        out.clear();
        let mut t = CssEventTracker::default();
        t.see(
            key(true),
            &timing(-300.0, 1000.0, 1.0),
            0.0,
            10.0,
            false,
            &mut out,
        );
        assert_eq!(out[1].elapsed, 0.3);
    }
}
