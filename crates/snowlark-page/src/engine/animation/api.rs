// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Web Animations programming interface (§6): `AnimationTimeline`, `DocumentTimeline`,
//! `Animation`, `AnimationEffect`, `KeyframeEffect`, `Animatable`, `AnimationPlaybackEvent`, and the
//! CSS Animations/Transitions event interfaces.

use std::sync::Arc;

use snowlark_bindings::generated::{
    self as g, AnimatableImpl, AnimationEffectImpl, AnimationEventImpl, AnimationImpl,
    AnimationPlayState, AnimationPlaybackEventImpl, AnimationReplaceState, AnimationTimelineImpl,
    CSSAnimationImpl, CSSTransitionImpl, DocumentTimelineImpl, KeyframeEffectImpl,
    TransitionEventImpl, UnrestrictedDoubleOrString,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle, JsObject, JsPromise};
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;
use snowlark_style::web_animations::{
    CompositeOperation, Easing, FillMode, PlaybackDirection, Timing,
};

use super::super::events::EventExt;
use super::super::{exception, handle_node, node_handle, Engine, Object};
use super::model::PlayState;
use super::{EffectData, ReplaceState};

/// The members of an `OptionalEffectTiming` (or of an `EffectTiming` with all members present).
#[derive(Default)]
struct TimingInput {
    delay: Option<f64>,
    end_delay: Option<f64>,
    fill: Option<g::FillMode>,
    iteration_start: Option<f64>,
    iterations: Option<f64>,
    duration: Option<UnrestrictedDoubleOrString>,
    direction: Option<g::PlaybackDirection>,
    easing: Option<String>,
}

fn type_error(message: &'static str) -> Error {
    Error::Type(message.into())
}

/// §6.5.4 "update the timing properties of an animation effect": checks, then assigns.
fn update_timing(timing: &mut Timing, input: TimingInput) -> rt::Result<()> {
    if input.iteration_start.is_some_and(|s| s < 0.0) {
        return Err(type_error("iterationStart must be non-negative"));
    }
    if input.iterations.is_some_and(|i| i < 0.0 || i.is_nan()) {
        return Err(type_error("iterations must be non-negative"));
    }
    let duration = match input.duration {
        None => None,
        Some(UnrestrictedDoubleOrString::UnrestrictedDouble(d)) => {
            if d < 0.0 || d.is_nan() {
                return Err(type_error("duration must be non-negative or auto"));
            }
            Some(Some(d))
        }
        Some(UnrestrictedDoubleOrString::String(s)) => {
            if s != "auto" {
                return Err(type_error("duration must be non-negative or auto"));
            }
            Some(None)
        }
    };
    let easing =
        match &input.easing {
            Some(e) => Some(Easing::parse(e).ok_or_else(|| {
                Error::Type(format!("'{e}' is not a valid value for easing").into())
            })?),
            None => None,
        };
    if let Some(d) = input.delay {
        timing.delay = d;
    }
    if let Some(d) = input.end_delay {
        timing.end_delay = d;
    }
    if let Some(f) = input.fill {
        timing.fill = match f {
            g::FillMode::None => FillMode::None,
            g::FillMode::Forwards => FillMode::Forwards,
            g::FillMode::Backwards => FillMode::Backwards,
            g::FillMode::Both => FillMode::Both,
            g::FillMode::Auto => FillMode::Auto,
        };
    }
    if let Some(s) = input.iteration_start {
        timing.iteration_start = s;
    }
    if let Some(i) = input.iterations {
        timing.iterations = i;
    }
    if let Some(d) = duration {
        timing.duration = d;
    }
    if let Some(d) = input.direction {
        timing.direction = match d {
            g::PlaybackDirection::Normal => PlaybackDirection::Normal,
            g::PlaybackDirection::Reverse => PlaybackDirection::Reverse,
            g::PlaybackDirection::Alternate => PlaybackDirection::Alternate,
            g::PlaybackDirection::AlternateReverse => PlaybackDirection::AlternateReverse,
        };
    }
    if let Some(e) = easing {
        timing.easing = e;
    }
    Ok(())
}

fn g_fill(f: FillMode) -> g::FillMode {
    match f {
        FillMode::None => g::FillMode::None,
        FillMode::Forwards => g::FillMode::Forwards,
        FillMode::Backwards => g::FillMode::Backwards,
        FillMode::Both => g::FillMode::Both,
        FillMode::Auto => g::FillMode::Auto,
    }
}

fn g_direction(d: PlaybackDirection) -> g::PlaybackDirection {
    match d {
        PlaybackDirection::Normal => g::PlaybackDirection::Normal,
        PlaybackDirection::Reverse => g::PlaybackDirection::Reverse,
        PlaybackDirection::Alternate => g::PlaybackDirection::Alternate,
        PlaybackDirection::AlternateReverse => g::PlaybackDirection::AlternateReverse,
    }
}

fn composite_from(c: g::CompositeOperation) -> CompositeOperation {
    match c {
        g::CompositeOperation::Replace => CompositeOperation::Replace,
        g::CompositeOperation::Add => CompositeOperation::Add,
        g::CompositeOperation::Accumulate => CompositeOperation::Accumulate,
    }
}

/// The timing members of `KeyframeEffectOptions`/`KeyframeAnimationOptions` (all present).
macro_rules! timing_input {
    ($o:expr) => {
        TimingInput {
            delay: Some($o.delay.unwrap_or(0.0)),
            end_delay: Some($o.end_delay.unwrap_or(0.0)),
            fill: Some($o.fill),
            iteration_start: Some($o.iteration_start),
            iterations: Some($o.iterations),
            duration: Some($o.duration),
            direction: Some($o.direction),
            easing: Some($o.easing),
        }
    };
}

/// The options argument of the `KeyframeEffect` constructor and `animate()`, reduced.
struct EffectOptions {
    timing: TimingInput,
    composite: CompositeOperation,
    pseudo: Option<String>,
}

impl Engine {
    /// The `KeyframeEffect(target, keyframes, options)` steps (§6.6).
    fn new_keyframe_effect(
        &self,
        cx: &mut Cx,
        target: Option<NodeId>,
        keyframes: Option<&JsObject>,
        options: EffectOptions,
    ) -> rt::Result<Handle> {
        let pseudo = Engine::parse_pseudo_element(options.pseudo.as_deref())?;
        let mut timing = Timing::default();
        update_timing(&mut timing, options.timing)?;
        let base = match target {
            Some(t) => self.base_url_of(t),
            None => {
                let doc = self.current_document(cx)?;
                self.base_url_of(doc)
            }
        };
        let keyframes = self.process_keyframes(cx, keyframes, &base)?;
        Ok(self.alloc(Object::KeyframeEffect(Box::new(EffectData {
            timing,
            target,
            pseudo,
            composite: options.composite,
            keyframes: Arc::new(keyframes),
            animation: None,
        }))))
    }

    /// The relevant animations of the targets `matches` accepts, in composite order (CSS transitions,
    /// CSS animations, then script animations), after a style change event for `node`'s document.
    fn relevant_animations(
        &self,
        cx: &mut Cx,
        node: NodeId,
        matches: impl Fn(NodeId, Option<&str>) -> bool,
    ) -> Vec<Handle> {
        // "Calling this method triggers a style change event."
        self.with_style(node, |_| ());
        let doc = self.dom().node_document(node);
        let mut all = self.css_animation_objects(cx, doc);
        all.extend(self.animations_in_order());
        all.into_iter()
            .filter(|&a| {
                let Some(effect) = self.anim(a, |a| a.effect) else {
                    return false;
                };
                let target = self.effect(effect, |e| e.target.map(|t| (t, e.pseudo.clone())));
                let Some((t, pseudo)) = target else {
                    return false;
                };
                self.dom().is_alive(t) && matches(t, pseudo.as_deref()) && self.is_relevant(a)
            })
            .collect()
    }

    /// `Document.getAnimations()` and `ShadowRoot.getAnimations()`: the relevant animations whose
    /// target is a descendant of `root` (shadow-including for a document... in the root's tree).
    pub(crate) fn document_or_shadow_animations(&self, cx: &mut Cx, root: NodeId) -> Vec<Handle> {
        self.relevant_animations(cx, root, |t, _| {
            let dom = self.dom();
            dom.is_connected(t) && dom.ancestors(t).any(|a| a == root)
        })
    }

    /// `document.timeline`.
    pub(crate) fn document_timeline(&self, doc: NodeId) -> Handle {
        self.default_timeline(doc)
    }
}

impl AnimationTimelineImpl for Engine {
    fn current_time(&self, this: Handle) -> Option<f64> {
        self.timeline_time(this)
    }
    fn duration(&self, _this: Handle) -> Option<f64> {
        None
    }
}

impl DocumentTimelineImpl for Engine {
    fn constructor(&self, cx: &mut Cx, options: g::DocumentTimelineOptions) -> rt::Result<Handle> {
        let document = self.current_document(cx)?;
        Ok(
            self.alloc(Object::DocumentTimeline(Box::new(super::TimelineData {
                document,
                origin_time: options.origin_time,
            }))),
        )
    }
}

impl AnimationImpl for Engine {
    fn id(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.anim(this, |a| a.id.clone()))
    }
    fn set_id(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.anim_mut(this, |a| a.id = value.to_owned());
        Ok(())
    }
    fn effect(&self, this: Handle) -> Option<Handle> {
        self.anim(this, |a| a.effect)
    }
    fn set_effect(&self, cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
        self.set_animation_effect(cx, this, value)
    }
    fn timeline(&self, this: Handle) -> Option<Handle> {
        self.anim(this, |a| a.timeline)
    }
    fn set_timeline(&self, cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
        self.set_animation_timeline(cx, this, value)
    }
    fn playback_rate(&self, this: Handle) -> f64 {
        self.anim(this, |a| a.model.playback_rate)
    }
    fn set_playback_rate(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.with_model(cx, this, |m, env, notes| {
            m.set_playback_rate(env, value, notes);
            Ok(())
        })
    }
    fn play_state(&self, this: Handle) -> AnimationPlayState {
        match Engine::play_state(self, this) {
            PlayState::Idle => AnimationPlayState::Idle,
            PlayState::Running => AnimationPlayState::Running,
            PlayState::Paused => AnimationPlayState::Paused,
            PlayState::Finished => AnimationPlayState::Finished,
        }
    }
    fn replace_state(&self, this: Handle) -> AnimationReplaceState {
        match self.anim(this, |a| a.replace_state) {
            ReplaceState::Active => AnimationReplaceState::Active,
            ReplaceState::Removed => AnimationReplaceState::Removed,
            ReplaceState::Persisted => AnimationReplaceState::Persisted,
        }
    }
    fn pending(&self, this: Handle) -> bool {
        self.anim(this, |a| a.model.pending())
    }
    fn ready(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.lazy_promise(cx, this, true)
    }
    fn finished(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.lazy_promise(cx, this, false)
    }
    fn start_time(&self, this: Handle) -> Option<f64> {
        self.anim(this, |a| a.model.start_time)
    }
    fn set_start_time(&self, cx: &mut Cx, this: Handle, value: Option<f64>) -> rt::Result<()> {
        self.with_model(cx, this, |m, env, notes| {
            m.set_start_time(env, value, notes);
            Ok(())
        })
    }
    fn current_time(&self, this: Handle) -> Option<f64> {
        self.anim_current_time(this)
    }
    fn set_current_time(&self, cx: &mut Cx, this: Handle, value: Option<f64>) -> rt::Result<()> {
        self.with_model(cx, this, |m, env, notes| {
            m.set_current_time(env, value, notes)
        })
    }
    /// Web Animations 2 `overallProgress`: the current time over the associated effect end, in [0, 1].
    fn overall_progress(&self, this: Handle) -> Option<f64> {
        let effect = self.anim(this, |a| a.effect)?;
        let current = self.anim_current_time(this)?;
        let end = self.effect(effect, |e| e.timing.end_time());
        Some(if end == 0.0 {
            if current < 0.0 {
                0.0
            } else {
                1.0
            }
        } else if end.is_infinite() {
            0.0
        } else {
            (current / end).clamp(0.0, 1.0)
        })
    }
    fn cancel(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.with_model(cx, this, |m, env, notes| {
            m.cancel(env, notes);
            Ok(())
        })
    }
    fn finish(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.with_model(cx, this, |m, env, notes| m.finish(env, notes))
    }
    fn play(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.with_model(cx, this, |m, env, notes| m.play(env, true, notes))
    }
    fn pause(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.with_model(cx, this, |m, env, notes| m.pause(env, notes))
    }
    fn update_playback_rate(
        &self,
        cx: &mut Cx,
        this: Handle,
        playback_rate: f64,
    ) -> rt::Result<()> {
        self.with_model(cx, this, |m, env, notes| {
            m.update_playback_rate(env, playback_rate, notes)
        })
    }
    fn reverse(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.with_model(cx, this, |m, env, notes| m.reverse(env, notes))
    }
    fn persist(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.anim_mut(this, |a| a.replace_state = ReplaceState::Persisted);
        self.animations.borrow_mut().dirty = true;
        Ok(())
    }
    fn commit_styles(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.commit_animation_styles(cx, this)
    }
    fn constructor(
        &self,
        cx: &mut Cx,
        effect: Option<Handle>,
        timeline: Option<Handle>,
    ) -> rt::Result<Handle> {
        // (a missing timeline and a null one look the same here: both take the default timeline)
        let timeline = match timeline {
            Some(t) => t,
            None => {
                let doc = self.current_document(cx)?;
                self.default_timeline(doc)
            }
        };
        self.new_animation(cx, effect, Some(timeline))
    }
}

impl AnimationEffectImpl for Engine {
    fn get_timing(&self, _cx: &mut Cx, this: Handle) -> rt::Result<g::EffectTiming> {
        Ok(self.effect(this, |e| {
            let t = &e.timing;
            g::EffectTiming {
                delay: Some(t.delay),
                direction: g_direction(t.direction),
                duration: match t.duration {
                    Some(d) => UnrestrictedDoubleOrString::UnrestrictedDouble(d),
                    None => UnrestrictedDoubleOrString::String("auto".into()),
                },
                easing: t.easing.css().to_owned(),
                end_delay: Some(t.end_delay),
                fill: g_fill(t.fill),
                iteration_start: t.iteration_start,
                iterations: t.iterations,
            }
        }))
    }
    fn get_computed_timing(
        &self,
        _cx: &mut Cx,
        this: Handle,
    ) -> rt::Result<g::ComputedEffectTiming> {
        let c = self.computed_timing(this, false);
        Ok(self.effect(this, |e| {
            let t = &e.timing;
            g::ComputedEffectTiming {
                delay: Some(t.delay),
                direction: g_direction(t.direction),
                duration: UnrestrictedDoubleOrString::UnrestrictedDouble(t.iteration_duration()),
                easing: t.easing.css().to_owned(),
                end_delay: Some(t.end_delay),
                fill: g_fill(t.computed_fill()),
                iteration_start: t.iteration_start,
                iterations: t.iterations,
                active_duration: Some(t.active_duration()),
                current_iteration: c.current_iteration,
                end_time: Some(t.end_time()),
                local_time: c.local_time,
                progress: c.progress,
                start_time: Some(0.0),
            }
        }))
    }
    fn update_timing(
        &self,
        cx: &mut Cx,
        this: Handle,
        timing: g::OptionalEffectTiming,
    ) -> rt::Result<()> {
        let input = TimingInput {
            delay: timing.delay,
            end_delay: timing.end_delay,
            fill: timing.fill,
            iteration_start: timing.iteration_start,
            iterations: timing.iterations,
            duration: timing.duration,
            direction: timing.direction,
            easing: timing.easing,
        };
        let mut t = self.effect(this, |e| e.timing.clone());
        update_timing(&mut t, input)?;
        self.effect_mut(this, |e| e.timing = t);
        self.effect_timing_changed(cx, this);
        Ok(())
    }
}

impl KeyframeEffectImpl for Engine {
    fn target(&self, this: Handle) -> Option<Handle> {
        self.effect(this, |e| e.target).map(node_handle)
    }
    fn set_target(&self, cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
        let target = value.and_then(handle_node);
        self.effect_mut(this, |e| e.target = target);
        self.effect_timing_changed(cx, this);
        Ok(())
    }
    fn pseudo_element(&self, this: Handle) -> Option<DomStr<'_>> {
        self.effect(this, |e| e.pseudo.clone()).map(DomStr::Owned)
    }
    fn set_pseudo_element(&self, cx: &mut Cx, this: Handle, value: Option<&str>) -> rt::Result<()> {
        let pseudo = Engine::parse_pseudo_element(value)?;
        self.effect_mut(this, |e| e.pseudo = pseudo);
        self.effect_timing_changed(cx, this);
        Ok(())
    }
    fn composite(&self, this: Handle) -> g::CompositeOperation {
        match self.effect(this, |e| e.composite) {
            CompositeOperation::Replace => g::CompositeOperation::Replace,
            CompositeOperation::Add => g::CompositeOperation::Add,
            CompositeOperation::Accumulate => g::CompositeOperation::Accumulate,
        }
    }
    fn set_composite(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: g::CompositeOperation,
    ) -> rt::Result<()> {
        self.effect_mut(this, |e| e.composite = composite_from(value));
        self.effect_timing_changed(cx, this);
        Ok(())
    }
    fn get_keyframes(&self, cx: &mut Cx, this: Handle) -> rt::Result<Vec<JsObject>> {
        let (mut keyframes, animation, target) =
            self.effect(this, |e| (e.keyframes.clone(), e.animation, e.target));
        // A CSSAnimation's effect shows its @keyframes rule's computed keyframes until script sets
        // others (CSS Animations 2 §4.3).
        let css_name = animation.and_then(|a| {
            self.anim(a, |a| match &a.css {
                Some(super::CssOwner::Animation { name, .. }) if a.css_key.is_some() => {
                    Some(name.clone())
                }
                _ => None,
            })
        });
        if let (Some(name), Some(target), true) = (css_name, target, keyframes.is_empty()) {
            let raws = self
                .with_style(target, |r| r.css_animation_keyframes(target, &name))
                .unwrap_or_default();
            let base = self.base_url_of(target);
            if let Ok(k) = snowlark_style::web_animations::keyframes::process_sequence(raws, &base)
            {
                keyframes = Arc::new(k);
            }
        }
        Ok(self.keyframe_objects(cx, &keyframes))
    }
    fn set_keyframes(
        &self,
        cx: &mut Cx,
        this: Handle,
        keyframes: Option<JsObject>,
    ) -> rt::Result<()> {
        let base = match self.effect(this, |e| e.target) {
            Some(t) => self.base_url_of(t),
            None => {
                let doc = self.current_document(cx)?;
                self.base_url_of(doc)
            }
        };
        let frames = self.process_keyframes(cx, keyframes.as_ref(), &base)?;
        self.effect_mut(this, |e| e.keyframes = Arc::new(frames));
        self.effect_timing_changed(cx, this);
        Ok(())
    }
    fn constructor_target_keyframes_options(
        &self,
        cx: &mut Cx,
        target: Option<Handle>,
        keyframes: Option<JsObject>,
        options: g::UnrestrictedDoubleOrKeyframeEffectOptions,
    ) -> rt::Result<Handle> {
        let options = match options {
            g::UnrestrictedDoubleOrKeyframeEffectOptions::UnrestrictedDouble(d) => EffectOptions {
                timing: TimingInput {
                    duration: Some(UnrestrictedDoubleOrString::UnrestrictedDouble(d)),
                    ..Default::default()
                },
                composite: CompositeOperation::Replace,
                pseudo: None,
            },
            g::UnrestrictedDoubleOrKeyframeEffectOptions::KeyframeEffectOptions(o) => {
                EffectOptions {
                    composite: composite_from(o.composite),
                    pseudo: o.pseudo_element.clone(),
                    timing: timing_input!(o),
                }
            }
        };
        self.new_keyframe_effect(
            cx,
            target.and_then(handle_node),
            keyframes.as_ref(),
            options,
        )
    }
    fn constructor_source(&self, _cx: &mut Cx, source: Handle) -> rt::Result<Handle> {
        let copy = self.effect(source, |e| EffectData {
            timing: e.timing.clone(),
            target: e.target,
            pseudo: e.pseudo.clone(),
            composite: e.composite,
            keyframes: e.keyframes.clone(),
            animation: None,
        });
        Ok(self.alloc(Object::KeyframeEffect(Box::new(copy))))
    }
}

/// Scroll-driven Animations 1 `<timeline-range-name>`.
const RANGE_NAMES: &[&str] = &[
    "cover",
    "contain",
    "entry",
    "exit",
    "entry-crossing",
    "exit-crossing",
    "scroll",
];

/// Checks a `rangeStart`/`rangeEnd` option (Web Animations 2, Scroll-driven Animations 1 §4.2):
/// `normal`, or an optional `<timeline-range-name>` and a `<length-percentage>` that does not depend
/// on fonts; a dictionary's `rangeName` must be a range name. Ranges only matter for scroll-driven
/// timelines, which are not implemented: a valid range is accepted and has no effect.
fn check_range(range: &g::TimelineRangeOffsetOrString) -> rt::Result<()> {
    let invalid = || type_error("The animation range is not valid");
    match range {
        g::TimelineRangeOffsetOrString::TimelineRangeOffset(o) => match &o.range_name {
            Some(n) if !RANGE_NAMES.contains(&n.as_str()) => Err(invalid()),
            _ => Ok(()),
        },
        g::TimelineRangeOffsetOrString::String(s) => {
            let s = s.trim();
            if s == "normal" {
                return Ok(());
            }
            let (name, offset) = match s.split_once(char::is_whitespace) {
                Some((n, rest)) => (Some(n), rest.trim()),
                None if s.starts_with(|c: char| c.is_ascii_alphabetic()) => (Some(s), ""),
                None => (None, s),
            };
            if name.is_some_and(|n| !RANGE_NAMES.contains(&n)) {
                return Err(invalid());
            }
            if offset.is_empty() {
                return Ok(());
            }
            let font_relative = offset
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '.')
                .any(|tok| {
                    let unit = tok.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
                    tok.starts_with(|c: char| c.is_ascii_digit() || c == '.')
                        && matches!(
                            unit,
                            "em" | "rem"
                                | "ex"
                                | "rex"
                                | "cap"
                                | "rcap"
                                | "ch"
                                | "rch"
                                | "ic"
                                | "ric"
                                | "lh"
                                | "rlh"
                        )
                });
            if font_relative || !snowlark_style::supports_declaration("margin-left", offset) {
                return Err(invalid());
            }
            Ok(())
        }
    }
}

impl AnimatableImpl for Engine {
    /// `animate(keyframes, options)` (§6.8).
    fn animate(
        &self,
        cx: &mut Cx,
        this: Handle,
        keyframes: Option<JsObject>,
        options: g::UnrestrictedDoubleOrKeyframeAnimationOptions,
    ) -> rt::Result<Handle> {
        let target = self.n(this);
        let (options, id, timeline) = match options {
            g::UnrestrictedDoubleOrKeyframeAnimationOptions::UnrestrictedDouble(d) => (
                EffectOptions {
                    timing: TimingInput {
                        duration: Some(UnrestrictedDoubleOrString::UnrestrictedDouble(d)),
                        ..Default::default()
                    },
                    composite: CompositeOperation::Replace,
                    pseudo: None,
                },
                String::new(),
                None,
            ),
            g::UnrestrictedDoubleOrKeyframeAnimationOptions::KeyframeAnimationOptions(o) => {
                check_range(&o.range_start)?;
                check_range(&o.range_end)?;
                (
                    EffectOptions {
                        composite: composite_from(o.composite),
                        pseudo: o.pseudo_element.clone(),
                        timing: timing_input!(o),
                    },
                    o.id.clone(),
                    o.timeline,
                )
            }
        };
        let effect = self.new_keyframe_effect(cx, Some(target), keyframes.as_ref(), options)?;
        // (a missing timeline member and a null one look the same here: both take the default one)
        let timeline = match timeline {
            Some(t) => t,
            None => {
                let doc = self.dom().node_document(target);
                self.default_timeline(doc)
            }
        };
        let anim = self.new_animation(cx, Some(effect), Some(timeline))?;
        self.anim_mut(anim, |a| a.id = id);
        self.with_model(cx, anim, |m, env, notes| m.play(env, true, notes))?;
        Ok(anim)
    }

    /// `getAnimations(options)`.
    fn get_animations(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: g::GetAnimationsOptions,
    ) -> rt::Result<Vec<Handle>> {
        let el = self.n(this);
        let pseudo = Engine::parse_pseudo_element(options.pseudo_element.as_deref())?;
        let subtree = options.subtree;
        Ok(self.relevant_animations(cx, el, |t, p| match &pseudo {
            Some(want) => t == el && p == Some(want.as_str()),
            None if subtree => t == el || self.dom().ancestors(t).any(|a| a == el),
            None => t == el && p.is_none(),
        }))
    }
}

impl AnimationPlaybackEventImpl for Engine {
    fn current_time(&self, this: Handle) -> Option<f64> {
        self.ev(this, |e| match e.ext {
            EventExt::AnimationPlayback { current_time, .. } => current_time,
            _ => None,
        })
    }
    fn timeline_time(&self, this: Handle) -> Option<f64> {
        self.ev(this, |e| match e.ext {
            EventExt::AnimationPlayback { timeline_time, .. } => timeline_time,
            _ => None,
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::AnimationPlaybackEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::AnimationPlaybackEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::AnimationPlayback {
                current_time: init.current_time,
                timeline_time: init.timeline_time,
            },
        ))
    }
}

/// The fields of an `AnimationEvent` or a `TransitionEvent`.
#[derive(Clone, Debug, Default)]
pub struct CssEventFields {
    /// `animationName` or `propertyName`.
    pub name: String,
    pub elapsed_time: f64,
    pub pseudo_element: String,
    pub animation: Option<Handle>,
}

impl Engine {
    fn css_event<R>(&self, h: Handle, f: impl FnOnce(&CssEventFields) -> R) -> R {
        self.ev(h, |e| match &e.ext {
            EventExt::CssAnimation(c) => f(c),
            _ => f(&CssEventFields::default()),
        })
    }
}

impl AnimationEventImpl for Engine {
    fn animation_name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.css_event(this, |c| c.name.clone()))
    }
    fn elapsed_time(&self, this: Handle) -> f64 {
        self.css_event(this, |c| c.elapsed_time)
    }
    fn pseudo_element(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.css_event(this, |c| c.pseudo_element.clone()))
    }
    fn animation(&self, this: Handle) -> Option<Handle> {
        self.css_event(this, |c| c.animation)
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::AnimationEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::AnimationEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::CssAnimation(Box::new(CssEventFields {
                name: init.animation_name,
                elapsed_time: init.elapsed_time,
                pseudo_element: init.pseudo_element,
                animation: init.animation,
            })),
        ))
    }
}

impl TransitionEventImpl for Engine {
    fn property_name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.css_event(this, |c| c.name.clone()))
    }
    fn elapsed_time(&self, this: Handle) -> f64 {
        self.css_event(this, |c| c.elapsed_time)
    }
    fn pseudo_element(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.css_event(this, |c| c.pseudo_element.clone()))
    }
    fn animation(&self, this: Handle) -> Option<Handle> {
        self.css_event(this, |c| c.animation)
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::TransitionEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::TransitionEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::CssAnimation(Box::new(CssEventFields {
                name: init.property_name,
                elapsed_time: init.elapsed_time,
                pseudo_element: init.pseudo_element,
                animation: init.animation,
            })),
        ))
    }
}

impl CSSAnimationImpl for Engine {
    fn animation_name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.anim(this, |a| match &a.css {
            Some(super::CssOwner::Animation { name, .. }) => name.clone(),
            _ => String::new(),
        }))
    }
}

impl CSSTransitionImpl for Engine {
    fn transition_property(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.anim(this, |a| match &a.css {
            Some(super::CssOwner::Transition { property, .. }) => property.clone(),
            _ => String::new(),
        }))
    }
}

impl Engine {
    /// §6.4 "commit computed styles".
    pub(crate) fn commit_animation_styles(&self, cx: &mut Cx, anim: Handle) -> rt::Result<()> {
        let Some(effect) = self.anim(anim, |a| a.effect) else {
            return Ok(());
        };
        let (target, pseudo, keyframes) = self.effect(effect, |e| {
            (e.target, e.pseudo.clone(), e.keyframes.clone())
        });
        let Some(target) = target else {
            return Ok(());
        };
        // (an element of a namespace without a style attribute, or a pseudo-element)
        let styleable = {
            let dom = self.dom();
            dom.is_element(target)
                && [
                    snowlark_dom::ns!(html),
                    snowlark_dom::ns!(svg),
                    snowlark_dom::ns!(mathml),
                ]
                .contains(dom.namespace(target))
        };
        if pseudo.is_some() || !styleable {
            return Err(exception(
                ExceptionName::NoModificationAllowedError,
                "The target cannot have a style attribute",
            ));
        }
        self.sync_animation_styles();
        let rendered = self
            .with_style(target, |r| r.is_being_rendered(target))
            .unwrap_or(false);
        if !rendered {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The target is not being rendered",
            ));
        }
        let properties = super::longhands(&keyframes);
        if properties.is_empty() {
            return Ok(());
        }
        // The partial effect stack: the effects of the target up to this animation's, this one's
        // with the endpoint-inclusive active interval.
        let sequence = self.anim(anim, |a| a.sequence);
        let doc = self.dom().node_document(target);
        let mut samples = Vec::new();
        for a in self.animations_in_order() {
            let (seq, eff, removed) = self.anim(a, |a| {
                (
                    a.sequence,
                    a.effect,
                    a.replace_state == ReplaceState::Removed,
                )
            });
            let Some(eff) = eff else { continue };
            if seq > sequence || (removed && a != anim) {
                continue;
            }
            let sample = self.effect_sample(eff, a == anim);
            if let Some(s) = sample.filter(|s| self.dom().node_document(s.target) == doc) {
                samples.push(s);
            }
        }
        let values: Vec<(String, String)> = self
            .with_doc_renderer(doc, |r, _| {
                let old = r.set_script_effects(samples);
                r.restyle();
                let values = properties
                    .iter()
                    .filter_map(|p| {
                        let v = r.computed_property(target, p)?;
                        Some((r.physical_property(target, p), v))
                    })
                    .collect();
                r.set_script_effects(old);
                values
            })
            .unwrap_or_default();
        self.animations.borrow_mut().dirty = true;
        self.set_inline_properties(cx, target, &values);
        Ok(())
    }
}
