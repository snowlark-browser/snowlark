// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The document timeline's side of CSS Transitions 1 and CSS Animations 1 (Web Animations 1 §4.4
//! "update animations and send events"): stylo starts, replaces and cancels transitions and animations
//! while it restyles, and computes their values at the time it is given; advancing their states as the
//! timeline moves (pending to running, the next iteration, finished) is the timeline owner's job, as it
//! is Servo's script thread's for Servo.

use style::animation::{Animation, AnimationState, ElementAnimationSet, KeyframesIterationState};
use style::properties::longhands::animation_direction::computed_value::single_value::T as AnimationDirection;
use style::properties::longhands::animation_fill_mode::computed_value::single_value::T as AnimationFillMode;
use style::properties::{LonghandId, PropertyDeclarationId};

/// Advances the states of one element's (or pseudo-element's) animations and transitions from timeline
/// time `prev` (the last update) to `now` (seconds). Returns whether its animated values may differ
/// from those of its last style: a state changed, or something was in its active (or delay) phase
/// since `prev`.
pub(crate) fn update_set(set: &mut ElementAnimationSet, prev: f64, now: f64) -> bool {
    set.clear_canceled_animations();
    let mut changed = now > prev && needs_ticks(set, prev);
    for a in set.animations.iter_mut() {
        // CSS Animations 1 §3: the animation starts once its delay has passed.
        if a.state == AnimationState::Pending && a.started_at <= now {
            a.state = AnimationState::Running;
            changed = true;
        }
        changed |= iterate(a, now);
        if a.state == AnimationState::Running && a.has_ended(now) && !fills_forwards(a) {
            // Finished animations without a forwards fill stop applying (stylo drops them at the next
            // restyle). One that fills forwards stays as it is: stylo's value computation applies its
            // fill, and dropping it would lose that.
            a.state = AnimationState::Finished;
            changed = true;
        }
    }
    for t in set.transitions.iter_mut() {
        // CSS Transitions 1 §3: a transition runs from its start time (the style change event plus
        // its delay) to its end time; once completed it no longer applies.
        if t.state == AnimationState::Pending && t.start_time <= now {
            t.state = AnimationState::Running;
            changed = true;
        }
        if t.state == AnimationState::Running && t.has_ended(now) {
            t.state = AnimationState::Finished;
            changed = true;
        }
    }
    changed
}

/// Whether the values of `set` change as the timeline moves on from `now`: an animation or transition
/// is pending (in its delay) or running and not yet past its end.
/// The names of `set`'s animations that need ticks at `now` (into `names`), and how many of its
/// transitions do: what [`needs_ticks`] answers for (profiling).
pub(crate) fn ticking(set: &ElementAnimationSet, now: f64, names: &mut Vec<String>) -> usize {
    let live = |s: &AnimationState| matches!(s, AnimationState::Pending | AnimationState::Running);
    for a in &set.animations {
        if live(&a.state) && !a.has_ended(now) {
            names.push(a.name.to_string());
        }
    }
    set.transitions
        .iter()
        .filter(|t| live(&t.state) && !t.has_ended(now))
        .count()
}

/// Whether a property is one a compositor animates: `transform`, `rotate`, `scale`, `translate`,
/// `opacity` or `filter` (Blink's `UpdateAnimationFlagsForEffect`, `css_animations.cc`).
pub(crate) fn composited_property(id: style::properties::PropertyDeclarationId) -> bool {
    matches!(
        id,
        PropertyDeclarationId::Longhand(
            LonghandId::Transform
                | LonghandId::Rotate
                | LonghandId::Scale
                | LonghandId::Translate
                | LonghandId::Opacity
                | LonghandId::Filter
        )
    )
}

/// [`crate::StyleEngine::animation_flags`]: an element's current animations of `opacity`.
pub const ANIMATES_OPACITY: u8 = 1;
/// [`crate::StyleEngine::animation_flags`]: an element's current animations of `transform`,
/// `translate`, `rotate` or `scale`.
pub const ANIMATES_TRANSFORM: u8 = 2;
/// [`crate::StyleEngine::animation_flags`]: an element's current animations of `filter`.
pub const ANIMATES_FILTER: u8 = 4;

/// The flag of a property's current animations (0 for the properties that have none).
pub(crate) fn animation_flag(id: PropertyDeclarationId) -> u8 {
    match id {
        PropertyDeclarationId::Longhand(LonghandId::Opacity) => ANIMATES_OPACITY,
        PropertyDeclarationId::Longhand(
            LonghandId::Transform | LonghandId::Translate | LonghandId::Rotate | LonghandId::Scale,
        ) => ANIMATES_TRANSFORM,
        PropertyDeclarationId::Longhand(LonghandId::Filter) => ANIMATES_FILTER,
        _ => 0,
    }
}

/// The flags of what `set`'s CSS animations and transitions that are current or in effect
/// animate (Blink's `UpdateAnimationFlagsForAnimation`, `css_animations.cc`: `IsCurrent() ||
/// IsInEffect()`).
pub(crate) fn current_flags(set: &ElementAnimationSet) -> u8 {
    let mut flags = 0;
    for a in &set.animations {
        let current = match a.state {
            AnimationState::Canceled => false,
            AnimationState::Finished => fills_forwards(a),
            _ => true,
        };
        if current {
            for p in a.properties_changed().iter() {
                flags |= animation_flag(p);
            }
        }
    }
    for t in &set.transitions {
        if matches!(t.state, AnimationState::Pending | AnimationState::Running) {
            flags |= animation_flag(t.property_animation.property_id());
        }
    }
    flags
}

/// Whether `set` has a CSS animation or transition of a property a compositor animates that is
/// current or in effect (Blink's `UpdateAnimationFlagsForAnimation`: `IsCurrent() ||
/// IsInEffect()`): pending in its delay, running, paused, or finished and filling forwards.
pub(crate) fn has_composited_animation(set: &ElementAnimationSet) -> bool {
    let ids = [
        LonghandId::Transform,
        LonghandId::Rotate,
        LonghandId::Scale,
        LonghandId::Translate,
        LonghandId::Opacity,
        LonghandId::Filter,
    ];
    set.animations.iter().any(|a| {
        let current = match a.state {
            AnimationState::Canceled => false,
            AnimationState::Finished => fills_forwards(a),
            _ => true,
        };
        current
            && ids.iter().any(|&id| {
                a.properties_changed()
                    .contains(PropertyDeclarationId::Longhand(id))
            })
    }) || set.transitions.iter().any(|t| {
        matches!(t.state, AnimationState::Pending | AnimationState::Running)
            && composited_property(t.property_animation.property_id())
    })
}

pub(crate) fn needs_ticks(set: &ElementAnimationSet, now: f64) -> bool {
    needs_page_ticks(set, &[], now)
}

/// [`needs_ticks`] leaving out what runs on a compositor: `on` is what the element's compositor
/// animations animate ([`crate::StyleEngine::set_on_compositor`]).
pub(crate) fn needs_page_ticks(set: &ElementAnimationSet, on: &[LonghandId], now: f64) -> bool {
    set.animations.iter().any(|a| {
        matches!(a.state, AnimationState::Pending | AnimationState::Running)
            && !a.has_ended(now)
            && !animation_on_compositor(set, a, on)
    }) || set.transitions.iter().any(|t| {
        matches!(t.state, AnimationState::Pending | AnimationState::Running)
            && !t.has_ended(now)
            && !transition_on_compositor(set, t, on)
    })
}

/// How many effects of `set` that are current or in effect animate `id` (CSS animations and
/// transitions).
pub(crate) fn effects_on(set: &ElementAnimationSet, id: LonghandId) -> usize {
    let live = |s: &AnimationState| matches!(s, AnimationState::Pending | AnimationState::Running);
    let current = |a: &Animation| match a.state {
        AnimationState::Canceled => false,
        AnimationState::Finished => fills_forwards(a),
        _ => true,
    };
    set.animations
        .iter()
        .filter(|a| {
            current(a)
                && a.properties_changed()
                    .contains(PropertyDeclarationId::Longhand(id))
        })
        .count()
        + set
            .transitions
            .iter()
            .filter(|t| {
                live(&t.state)
                    && t.property_animation.property_id() == PropertyDeclarationId::Longhand(id)
            })
            .count()
}

/// Whether the compositor runs what `id` shows on this element: the element's compositor
/// animations animate it (`on`), and the effect that does is still the only one on it (as
/// [`compositor_candidates`] chose it: for `transform`, nothing animates `translate`, `rotate` or
/// `scale` either).
fn property_on_compositor(set: &ElementAnimationSet, on: &[LonghandId], id: LonghandId) -> bool {
    on.contains(&id)
        && effects_on(set, id) == 1
        && (id != LonghandId::Transform
            || [LonghandId::Translate, LonghandId::Rotate, LonghandId::Scale]
                .into_iter()
                .all(|i| effects_on(set, i) == 0))
}

/// Whether a running (or delayed) CSS animation runs on a compositor: every property it animates
/// does (Blink's `KeyframeEffect::HasActiveAnimationsOnCompositor`).
pub(crate) fn animation_on_compositor(
    set: &ElementAnimationSet,
    a: &Animation,
    on: &[LonghandId],
) -> bool {
    !on.is_empty()
        && matches!(a.state, AnimationState::Pending | AnimationState::Running)
        && a.properties_changed().iter().all(|p| match p {
            PropertyDeclarationId::Longhand(id) => property_on_compositor(set, on, id),
            _ => false,
        })
}

/// [`animation_on_compositor`] for a transition.
pub(crate) fn transition_on_compositor(
    set: &ElementAnimationSet,
    t: &style::animation::Transition,
    on: &[LonghandId],
) -> bool {
    !on.is_empty()
        && matches!(t.state, AnimationState::Pending | AnimationState::Running)
        && match t.property_animation.property_id() {
            PropertyDeclarationId::Longhand(id) => property_on_compositor(set, on, id),
            _ => false,
        }
}

/// The earliest timeline time (s) at which an animation or transition of `set` that runs on a
/// compositor changes in a way the page must see: it starts (its delay ends), it ends (its fill
/// applies, its end event fires), or, when `iteration_events`, its next iteration begins. Blink's
/// `KeyframeEffect::CalculateTimeToEffectChange` ("Need service to apply fill / fire events"),
/// which `DocumentTimeline::ScheduleNextService` waits for instead of servicing every frame.
pub(crate) fn compositor_next_change(
    set: &ElementAnimationSet,
    on: &[LonghandId],
    iteration_events: bool,
) -> Option<f64> {
    let mut at: Option<f64> = None;
    let mut due = |t: f64| {
        if t.is_finite() {
            at = Some(at.map_or(t, |a| a.min(t)));
        }
    };
    for a in &set.animations {
        if !animation_on_compositor(set, a, on) {
            continue;
        }
        match a.state {
            AnimationState::Pending => due(a.started_at),
            _ => {
                let (current, max) = match a.iteration_state {
                    KeyframesIterationState::Finite(c, m) => (c, m),
                    KeyframesIterationState::Infinite(c) => (c, f64::INFINITY),
                };
                // (stylo moves `started_at` to the start of each iteration it reaches)
                let end = a.started_at + (max - current) * a.duration;
                due(end);
                let next = a.started_at + a.duration;
                if iteration_events && next < end {
                    due(next);
                }
            }
        }
    }
    for t in &set.transitions {
        if !transition_on_compositor(set, t, on) {
            continue;
        }
        match t.state {
            AnimationState::Pending => due(t.start_time),
            _ => due(t.start_time + t.property_animation.duration),
        }
    }
    at
}

fn fills_forwards(a: &Animation) -> bool {
    matches!(
        a.fill_mode,
        AnimationFillMode::Forwards | AnimationFillMode::Both
    )
}

/// CSS Animations 1 §4.6 `animation-iteration-count`, §4.5 `animation-direction`: moves a running
/// animation to the iteration that contains `now` (all at once, however long since the last update),
/// stopping at the last one; alternating directions flip with each iteration. Stylo's own
/// `iterate_if_necessary` steps one iteration per call.
fn iterate(a: &mut Animation, now: f64) -> bool {
    if a.state != AnimationState::Running || a.duration < 0.0 {
        return false;
    }
    let elapsed = now - a.started_at;
    // (an infinite count of zero-length iterations counts as one: stylo's `remaining_iterations`)
    let remaining = a.remaining_iterations();
    let n = if a.duration == 0.0 {
        // Zero-length iterations (CSS Animations 1 §3.4) are all over once the delay is: the last
        // one is current, its direction as the alternation leaves it.
        if elapsed < 0.0 {
            return false;
        }
        remaining.ceil() - 1.0
    } else {
        if elapsed <= a.duration {
            return false;
        }
        // The current iteration is over once `now` is past its end: skip the whole iterations that
        // fit before `now`, but not past the start of the last one.
        ((elapsed / a.duration).ceil() - 1.0).min(remaining.ceil() - 1.0)
    };
    if n < 1.0 {
        return false;
    }
    match a.iteration_state {
        KeyframesIterationState::Finite(ref mut current, max) => *current = (*current + n).min(max),
        KeyframesIterationState::Infinite(ref mut current) => *current += n,
    }
    a.started_at += a.duration * n;
    if matches!(
        a.direction,
        AnimationDirection::Alternate | AnimationDirection::AlternateReverse
    ) && n % 2.0 == 1.0
    {
        a.current_direction = match a.current_direction {
            AnimationDirection::Normal => AnimationDirection::Reverse,
            _ => AnimationDirection::Normal,
        };
    }
    true
}

/// An animation, transition or script animation's effect a compositor may run
/// ([`crate::StyleEngine::compositor_candidates`]).
pub enum CompositorCandidate<'a> {
    /// A CSS animation and the property it animates (`transform` or `opacity`).
    Animation(&'a Animation, style::properties::LonghandId),
    Transition(
        &'a style::animation::Transition,
        style::properties::LonghandId,
    ),
    /// One property of a script animation's keyframe effect.
    Script(crate::web_animations::ScriptCandidate<'a>),
}

/// The CSS animations and transitions of `set` a compositor may run
/// ([`crate::StyleEngine::compositor_candidates`]); `alone(id)` tells whether the effect that
/// animates `id` is the only one there, `important(id)` whether the element has an important
/// declaration of `id` (an animation of such a property stays with the page; a transition, above
/// important declarations in the cascade, does not).
pub(crate) fn compositor_candidates(
    set: &ElementAnimationSet,
    alone: &dyn Fn(LonghandId) -> bool,
    important: &dyn Fn(LonghandId) -> bool,
    f: &mut dyn FnMut(CompositorCandidate<'_>),
) {
    let live = |s: &AnimationState| matches!(s, AnimationState::Pending | AnimationState::Running);
    let changes = |a: &Animation, id: LonghandId| {
        a.properties_changed()
            .contains(PropertyDeclarationId::Longhand(id))
    };
    for id in [
        LonghandId::Transform,
        LonghandId::Opacity,
        LonghandId::Filter,
    ] {
        if !alone(id) {
            continue;
        }
        for a in &set.animations {
            let only_ours = a.properties_changed().iter().all(|p| {
                matches!(
                    p,
                    PropertyDeclarationId::Longhand(
                        LonghandId::Transform | LonghandId::Opacity | LonghandId::Filter
                    )
                )
            });
            let affects_important = || {
                a.properties_changed()
                    .iter()
                    .any(|p| matches!(p, PropertyDeclarationId::Longhand(l) if important(l)))
            };
            if live(&a.state)
                && changes(a, id)
                && only_ours
                && a.duration.is_finite()
                && a.duration > 0.0
                && a.started_at.is_finite()
                && !affects_important()
            {
                f(CompositorCandidate::Animation(a, id));
            }
        }
        for t in &set.transitions {
            let d = t.property_animation.duration;
            if live(&t.state)
                && t.property_animation.property_id() == PropertyDeclarationId::Longhand(id)
                && d.is_finite()
                && d > 0.0
                && t.start_time.is_finite()
            {
                f(CompositorCandidate::Transition(t, id));
            }
        }
    }
}
