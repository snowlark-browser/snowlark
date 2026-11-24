// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Animations 1 (https://drafts.csswg.org/web-animations-1/): document timelines, `Animation`,
//! `KeyframeEffect`, the animation events and "update animations and send events" (§4.4), which HTML's
//! "update the rendering" runs before the animation frame callbacks.
//!
//! An animation's playback state machine is [`model::Model`]; the timing calculations, keyframe
//! processing and the effect values in the cascade are `snowlark_style::web_animations`. This module
//! keeps the objects, carries out the promise and event work the procedures ask for, and hands the
//! style engine of each rendered document the effects in effect ([`Engine::sync_animation_styles`]).

pub mod model;

pub(crate) mod api;
mod css;
mod keyframes_js;

use std::sync::Arc;

use rustc_hash::FxHashMap;
use snowlark_bindings::rt::{self, Cx, Error, ExceptionName, Handle, JsPromise};
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;
use snowlark_style::web_animations::{
    CompositeOperation, CompositorTiming, ComputedTiming, EffectSample, Keyframe, Phase, Timing,
};

use super::events::EventExt;
use super::{exception, Engine, Object};
use model::{Env, Model, Note, PlayState, Throw};

/// A promise that is made only when script asks for it (`ready`, `finished`): until then only its
/// state is kept, so animations whose promises nobody reads cost no V8 objects.
#[derive(Default)]
pub struct LazyPromise {
    pub promise: Option<JsPromise>,
    pub state: PromiseState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromiseState {
    #[default]
    Pending,
    Resolved,
    Rejected,
}

impl LazyPromise {
    fn resolved() -> LazyPromise {
        LazyPromise {
            promise: None,
            state: PromiseState::Resolved,
        }
    }
}

/// A `DocumentTimeline`.
pub struct TimelineData {
    pub document: NodeId,
    /// The origin time (ms from the time origin).
    pub origin_time: f64,
}

/// `AnimationReplaceState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceState {
    Active,
    Removed,
    Persisted,
}

/// What made a CSS animation or transition object (CSS Animations 2 §4.1, CSS Transitions 2 §5):
/// its owning element.
#[derive(Clone, Debug, PartialEq)]
pub enum CssOwner {
    Animation {
        element: NodeId,
        pseudo: Option<String>,
        name: String,
    },
    Transition {
        element: NodeId,
        pseudo: Option<String>,
        property: String,
    },
}

/// An `Animation`.
pub struct AnimationData {
    /// For a `CSSAnimation` or `CSSTransition`: its owner.
    pub css: Option<CssOwner>,
    /// The CSS animation or transition it shows, while stylo runs it.
    pub css_key: Option<snowlark_style::web_animations::CssKey>,
    /// Script drove it: its model leads and stylo's animation follows ([`Engine::css_follow_model`]).
    pub css_controlled: bool,
    /// A `CSSAnimation`'s or `CSSTransition`'s own effect, which shows stylo's keyframes and timing.
    /// While script has set another effect, stylo's animation applies nothing and the page samples
    /// the effect it set ([`Engine::css_replaced_effect_samples`]).
    pub css_effect: Option<Handle>,
    pub id: String,
    pub timeline: Option<Handle>,
    pub effect: Option<Handle>,
    pub model: Model,
    pub ready: LazyPromise,
    pub finished: LazyPromise,
    pub replace_state: ReplaceState,
    /// The position in the global animation list (§4.5).
    pub sequence: u64,
    /// A microtask to run the finish notification steps is queued.
    pub finish_queued: bool,
    /// What a compositor runs of it, as of the last frame painted (`None`: nothing).
    pub compositor: Option<CompositorState>,
}

/// What a compositor got of a script animation with a frame (Blink's `Animation::CompositorState`,
/// `animation.h`, made when `Animation::PreCommit` starts it there): its effect, the effect's target,
/// keyframes and composite operation, and the timing the compositor samples it with. When the
/// animation no longer gives the same, a change reached it since (Blink's
/// `Animation::SetCompositorPending` compares the start time and effective playback rate, and
/// `Animation::EffectInvalidated` reports an effect change: `docs/compositor-animations.md`, S3).
#[derive(Clone, Debug)]
pub struct CompositorState {
    effect: Handle,
    target: Option<NodeId>,
    pseudo: Option<String>,
    /// Compared by identity (`setKeyframes()` replaces them).
    keyframes: Arc<Vec<Keyframe>>,
    composite: CompositeOperation,
    timing: Option<CompositorTiming>,
}

impl CompositorState {
    fn same_as(&self, other: &CompositorState) -> bool {
        self.effect == other.effect
            && self.target == other.target
            && self.pseudo == other.pseudo
            && Arc::ptr_eq(&self.keyframes, &other.keyframes)
            && self.composite == other.composite
            && self.timing == other.timing
    }
}

/// A `KeyframeEffect`.
pub struct EffectData {
    pub timing: Timing,
    pub target: Option<NodeId>,
    /// The target pseudo-selector (`::before`...), `None` for the element itself.
    pub pseudo: Option<String>,
    pub composite: CompositeOperation,
    pub keyframes: Arc<Vec<Keyframe>>,
    /// The animation this effect is the associated effect of.
    pub animation: Option<Handle>,
}

/// An animation event in a document's pending animation event queue (§4.5.18).
pub struct QueuedEvent {
    /// The scheduled event time (origin-relative, ms); `None` sorts first.
    pub scheduled: Option<f64>,
    /// The composite order of the animation that queued it.
    pub order: u64,
    pub event: Handle,
    pub target: Handle,
    /// For CSS events: the pseudo-element's place after its originating element (CSS Animations 2
    /// §4.2: the element, then `::marker`, `::before`, `::after`).
    pub pseudo_rank: u8,
}

/// The engine's animation state.
#[derive(Default)]
pub struct AnimationState {
    /// The global animation list: every script animation, in creation order.
    pub list: Vec<Handle>,
    pub next_sequence: u64,
    /// The default document timeline of each document that asked for it.
    pub timelines: FxHashMap<NodeId, Handle>,
    /// Each document's "now" (ms since the time origin) as of its last "update animations and send
    /// events", or of the first read before one.
    pub doc_times: FxHashMap<NodeId, f64>,
    /// The task in which each of `doc_times` was last set (see [`AnimationState::task`]).
    pub doc_time_tasks: FxHashMap<NodeId, u64>,
    /// Counts the tasks the event loop has run (Blink's `AnimationClock::NotifyTaskStart`): a
    /// document's time moves at most once per task outside rendering updates.
    pub task: u64,
    /// A listener for `animationiteration` was added somewhere on the page (Blink's
    /// `Document::kAnimationIterationListener`): animations a compositor runs then need a
    /// rendering update at each iteration, for its event.
    pub iteration_listeners: bool,
    /// The model changed since the style engines were last told.
    pub dirty: bool,
    /// The pending animation event queue (of every document: they are dispatched together).
    pub events: Vec<QueuedEvent>,
    /// The `CSSAnimation`/`CSSTransition` objects made so far, by the animation they show.
    pub css: FxHashMap<snowlark_style::web_animations::CssKey, Handle>,
    /// Events (and their targets) queued as tasks, kept until dispatched.
    pub held: Vec<Handle>,
    /// Sample buffers kept between frames.
    spare: Vec<Vec<EffectSample>>,
}

fn throw(t: Throw) -> Error {
    match t {
        Throw::Type => Error::Type("The current time cannot be unset".into()),
        Throw::InvalidState => exception(
            ExceptionName::InvalidStateError,
            "The animation cannot be played, paused, finished or reversed in this state",
        ),
    }
}

impl Engine {
    // ----- objects -----------------------------------------------------------------------------------------

    pub(crate) fn anim<R>(&self, h: Handle, f: impl FnOnce(&AnimationData) -> R) -> R {
        match &*self.obj(h) {
            Object::Animation(a) => f(a),
            _ => panic!("not an Animation"),
        }
    }

    pub(crate) fn anim_mut<R>(&self, h: Handle, f: impl FnOnce(&mut AnimationData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::Animation(a) => f(a),
            _ => panic!("not an Animation"),
        }
    }

    pub(crate) fn effect<R>(&self, h: Handle, f: impl FnOnce(&EffectData) -> R) -> R {
        match &*self.obj(h) {
            Object::KeyframeEffect(e) => f(e),
            _ => panic!("not a KeyframeEffect"),
        }
    }

    pub(crate) fn effect_mut<R>(&self, h: Handle, f: impl FnOnce(&mut EffectData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::KeyframeEffect(e) => f(e),
            _ => panic!("not a KeyframeEffect"),
        }
    }

    // ----- time --------------------------------------------------------------------------------------------

    /// The time (ms since the time origin) a rendering update of `doc` would read now: the page's
    /// clock as the document's renderer sees it (held by `--render --animation-time`).
    fn clock_for(&self, doc: NodeId) -> f64 {
        let now = self.now();
        if self.is_rendered_document(doc) {
            if let Some(t) = self.with_doc_renderer(doc, |r, _| r.clock_at(now / 1000.0) * 1000.0) {
                return t;
            }
        }
        now
    }

    /// A document timeline's "now" for `doc`: `None` while `doc` is not an active document (§4.3.1).
    pub(crate) fn document_time(&self, doc: NodeId) -> Option<f64> {
        if !self.dom().is_alive(doc) || self.context_of_document(doc).is_none() {
            return None;
        }
        let held = self.animations.borrow().doc_times.get(&doc).copied();
        if let Some(t) = held {
            return Some(self.advance_document_time(doc, t));
        }
        // Before its first rendering update the timeline reads its renderer's (where CSS animations
        // started so far have their start times), or the clock for a document that is not rendered,
        // and holds it (it does not change within a task).
        let rendered = if self.is_rendered_document(doc) {
            self.with_doc_renderer(doc, |r, _| r.timeline_time().as_secs_f64() * 1000.0)
        } else {
            None
        };
        let t = rendered.unwrap_or_else(|| self.clock_for(doc));
        let mut st = self.animations.borrow_mut();
        let task = st.task;
        st.doc_times.insert(doc, t);
        st.doc_time_tasks.insert(doc, task);
        Some(t)
    }

    /// A document's time `held` as a task after the one that set it reads it: moved to the last
    /// frame time the clock has passed, as if the rendering updates that did not run (nothing asked
    /// for them: a compositor runs the animations) had run (Blink's `AnimationClock::CurrentTime`,
    /// with its `kApproximateFrameTime`). Within one task it does not change, so a long script
    /// sees one time; a rendering update sets it from the frame's time.
    fn advance_document_time(&self, doc: NodeId, held: f64) -> f64 {
        {
            let st = self.animations.borrow();
            if st.doc_time_tasks.get(&doc) == Some(&st.task) {
                return held;
            }
        }
        let now = self.clock_for(doc);
        let t = if held < now {
            now - (now - held) % crate::event_loop::FRAME_MS
        } else {
            held
        };
        let mut st = self.animations.borrow_mut();
        // (script animations' values at the new time: Blink's
        // `DocumentAnimations::UpdateAnimationTimingIfNeeded` before a style read)
        if t != held && !st.list.is_empty() {
            st.dirty = true;
        }
        let task = st.task;
        st.doc_time_tasks.insert(doc, task);
        st.doc_times.insert(doc, t);
        t
    }

    /// The current time of a timeline (§4.3.1), ms.
    pub(crate) fn timeline_time(&self, timeline: Handle) -> Option<f64> {
        let (doc, origin) = match &*self.obj(timeline) {
            Object::DocumentTimeline(t) => (t.document, t.origin_time),
            _ => return None,
        };
        self.document_time(doc).map(|t| t - origin)
    }

    /// The default document timeline of `doc`.
    pub(crate) fn default_timeline(&self, doc: NodeId) -> Handle {
        if let Some(h) = self.animations.borrow().timelines.get(&doc) {
            return *h;
        }
        let h = self.alloc(Object::DocumentTimeline(Box::new(TimelineData {
            document: doc,
            origin_time: 0.0,
        })));
        self.animations.borrow_mut().timelines.insert(doc, h);
        h
    }

    /// The associated effect end of an animation (§4.5.12).
    fn effect_end(&self, effect: Option<Handle>) -> f64 {
        effect.map_or(0.0, |e| self.effect(e, |e| e.timing.end_time()))
    }

    /// What the model of `anim` needs to know about its surroundings now.
    pub(crate) fn env(&self, anim: Handle) -> Env {
        let (timeline, effect) = self.anim(anim, |a| (a.timeline, a.effect));
        Env {
            has_timeline: timeline.is_some(),
            timeline_time: timeline.and_then(|t| self.timeline_time(t)),
            effect_end: self.effect_end(effect),
        }
    }

    pub(crate) fn play_state(&self, anim: Handle) -> PlayState {
        let env = self.env(anim);
        self.anim(anim, |a| a.model.play_state(&env))
    }

    pub(crate) fn anim_current_time(&self, anim: Handle) -> Option<f64> {
        let env = self.env(anim);
        self.anim(anim, |a| a.model.current_time(&env))
    }

    /// The §4.7 calculations for an effect at its local time (its animation's current time).
    pub(crate) fn computed_timing(&self, effect: Handle, inclusive: bool) -> ComputedTiming {
        let animation = self.effect(effect, |e| e.animation);
        let (local, backwards) = match animation {
            Some(a) => (
                self.anim_current_time(a),
                self.anim(a, |a| a.model.playback_rate < 0.0),
            ),
            None => (None, false),
        };
        self.effect(effect, |e| {
            e.timing.compute_with(local, backwards, inclusive)
        })
    }

    // ----- procedures ----------------------------------------------------------------------------------

    /// Runs a model procedure script asked for on `anim` and carries out the notes it leaves; a CSS
    /// animation's object then leads its stylo animation.
    pub(crate) fn with_model<R>(
        &self,
        cx: &mut Cx,
        anim: Handle,
        f: impl FnOnce(&mut Model, &Env, &mut Vec<Note>) -> Result<R, Throw>,
    ) -> rt::Result<R> {
        let r = self.run_model(cx, anim, f);
        if self.anim(anim, |a| a.css_key.is_some()) {
            self.css_follow_model(anim);
        }
        r
    }

    /// Runs a model procedure on `anim` and carries out the notes it leaves.
    pub(crate) fn run_model<R>(
        &self,
        cx: &mut Cx,
        anim: Handle,
        f: impl FnOnce(&mut Model, &Env, &mut Vec<Note>) -> Result<R, Throw>,
    ) -> rt::Result<R> {
        let env = self.env(anim);
        let mut notes = Vec::new();
        let r = self.anim_mut(anim, |a| f(&mut a.model, &env, &mut notes));
        self.apply_notes(cx, anim, &notes);
        self.animations.borrow_mut().dirty = true;
        r.map_err(throw)
    }

    fn apply_notes(&self, cx: &mut Cx, anim: Handle, notes: &[Note]) {
        for note in notes {
            match *note {
                Note::NewReady => self.anim_mut(anim, |a| a.ready = LazyPromise::default()),
                Note::ResolveReady => self.settle(cx, anim, true, PromiseState::Resolved),
                Note::RejectReady => {
                    self.settle(cx, anim, true, PromiseState::Rejected);
                    self.anim_mut(anim, |a| a.ready = LazyPromise::resolved());
                }
                Note::QueueFinishNotification => {
                    if !self.anim(anim, |a| a.finish_queued) {
                        self.anim_mut(anim, |a| a.finish_queued = true);
                        queue_finish_microtask(cx, anim);
                    }
                }
                Note::FinishNotificationNow => {
                    self.anim_mut(anim, |a| a.finish_queued = false);
                    self.finish_notification_steps(cx, anim);
                }
                Note::NewFinished => self.anim_mut(anim, |a| a.finished = LazyPromise::default()),
                Note::RejectFinished => {
                    self.settle(cx, anim, false, PromiseState::Rejected);
                    self.anim_mut(anim, |a| a.finished = LazyPromise::default());
                }
                Note::CancelEvent(timeline_time) => {
                    let scheduled = timeline_time.and_then(|t| self.origin_relative(anim, t));
                    self.queue_playback_event(cx, anim, "cancel", None, timeline_time, scheduled);
                }
            }
        }
    }

    /// Resolves (with the animation) or rejects (with an AbortError, handled) the current ready or
    /// finished promise, if it is pending.
    fn settle(&self, cx: &mut Cx, anim: Handle, ready: bool, to: PromiseState) {
        let promise = self.anim_mut(anim, |a| {
            let p = if ready { &mut a.ready } else { &mut a.finished };
            if p.state != PromiseState::Pending {
                return None;
            }
            p.state = to;
            Some(p.promise.clone())
        });
        let Some(Some(p)) = promise else {
            return;
        };
        match to {
            PromiseState::Resolved => p.resolve(cx, anim),
            _ => {
                p.reject(
                    cx,
                    exception(ExceptionName::AbortError, "The animation was aborted"),
                );
                p.mark_handled(cx);
            }
        }
    }

    /// The promise object of a lazy promise (made now when nobody asked before).
    pub(crate) fn lazy_promise(
        &self,
        cx: &mut Cx,
        anim: Handle,
        ready: bool,
    ) -> rt::Result<JsPromise> {
        let existing = self.anim(anim, |a| {
            let p = if ready { &a.ready } else { &a.finished };
            (p.promise.clone(), p.state)
        });
        if let (Some(p), _) = existing {
            return Ok(p);
        }
        let p = JsPromise::new(cx).ok_or(Error::Pending)?;
        match existing.1 {
            PromiseState::Pending => {}
            PromiseState::Resolved => p.resolve(cx, anim),
            PromiseState::Rejected => {
                p.reject(
                    cx,
                    exception(ExceptionName::AbortError, "The animation was aborted"),
                );
                p.mark_handled(cx);
            }
        }
        self.anim_mut(anim, |a| {
            let slot = if ready { &mut a.ready } else { &mut a.finished };
            slot.promise = Some(p.clone());
        });
        Ok(p)
    }

    /// The finish notification steps (§4.5.12).
    pub(crate) fn finish_notification_steps(&self, cx: &mut Cx, anim: Handle) {
        let env = self.env(anim);
        if !self.anim_mut(anim, |a| a.model.finish_notification(&env)) {
            return;
        }
        self.settle(cx, anim, false, PromiseState::Resolved);
        let current = self.anim(anim, |a| a.model.current_time(&env));
        let scheduled = self.origin_relative_from_animation_time(anim, env.effect_end);
        self.queue_playback_event(cx, anim, "finish", current, env.timeline_time, scheduled);
    }

    /// §4.5.18.1: an animation time (relative to the start time) as an origin-relative time.
    fn origin_relative_from_animation_time(&self, anim: Handle, time: f64) -> Option<f64> {
        if time.is_infinite() {
            return None;
        }
        let (rate, start) = self.anim(anim, |a| (a.model.playback_rate, a.model.start_time));
        if rate == 0.0 {
            return None;
        }
        let timeline_time = time / rate + start?;
        self.origin_relative(anim, timeline_time)
    }

    /// A time of `anim`'s timeline as an origin-relative time.
    fn origin_relative(&self, anim: Handle, timeline_time: f64) -> Option<f64> {
        let timeline = self.anim(anim, |a| a.timeline)?;
        self.timeline_time(timeline)?;
        match &*self.obj(timeline) {
            Object::DocumentTimeline(t) => Some(timeline_time + t.origin_time),
            _ => None,
        }
    }

    /// Queues an `AnimationPlaybackEvent` at `anim` in its document for timing's pending animation
    /// event queue, or as a task when it has none.
    fn queue_playback_event(
        &self,
        _cx: &mut Cx,
        anim: Handle,
        type_: &str,
        current_time: Option<f64>,
        timeline_time: Option<f64>,
        scheduled: Option<f64>,
    ) {
        let event = self.trusted_event(
            InterfaceId::AnimationPlaybackEvent,
            type_,
            false,
            false,
            EventExt::AnimationPlayback {
                current_time,
                timeline_time,
            },
        );
        let (timeline, order) = self.anim(anim, |a| (a.timeline, a.sequence));
        let has_document =
            timeline.is_some_and(|t| matches!(&*self.obj(t), Object::DocumentTimeline(_)));
        if has_document {
            self.animations.borrow_mut().events.push(QueuedEvent {
                scheduled,
                // (after CSS transitions' and CSS animations' events, §5.4.1)
                order: order + 2,
                event,
                target: anim,
                pseudo_rank: 0,
            });
        } else {
            // (the task keeps the event and its target until it runs)
            self.animations.borrow_mut().held.extend([event, anim]);
            self.queue_task(
                crate::event_loop::TaskSource::DomManipulation,
                move |e, cx| {
                    e.dispatch(cx, event, anim, false);
                    let mut st = e.animations.borrow_mut();
                    for h in [event, anim] {
                        if let Some(i) = st.held.iter().position(|&x| x == h) {
                            st.held.swap_remove(i);
                        }
                    }
                },
            );
        }
    }

    /// Queues a CSS animation or transition event at its element.
    fn queue_css_event(&self, e: snowlark_style::web_animations::CssAnimationEvent) {
        let element = e.key.element;
        if !self.dom().is_alive(element) {
            return;
        }
        let interface = if e.kind.is_transition() {
            InterfaceId::TransitionEvent
        } else {
            InterfaceId::AnimationEvent
        };
        let animation = self.css_event_object(&e.key);
        let mut elapsed = e.elapsed;
        if matches!(
            e.kind,
            snowlark_style::web_animations::CssEventKind::AnimationCancel
                | snowlark_style::web_animations::CssEventKind::TransitionCancel
        ) {
            // A canceled animation script drives was where its object says.
            if let Some(a) = animation.filter(|&a| self.anim(a, |a| a.css_controlled)) {
                if let (Some(current), Some(effect)) =
                    (self.anim_current_time(a), self.anim(a, |a| a.effect))
                {
                    let timing = self.effect(effect, |e| e.timing.clone());
                    elapsed =
                        (current - timing.delay).clamp(0.0, timing.active_duration()) / 1000.0;
                }
            }
        }
        let fields = api::CssEventFields {
            name: e.key.name.clone(),
            elapsed_time: elapsed,
            pseudo_element: e.key.pseudo_text(),
            animation,
        };
        let event = self.trusted_event(
            interface,
            e.kind.type_name(),
            true,
            false,
            EventExt::CssAnimation(Box::new(fields)),
        );
        self.animations.borrow_mut().events.push(QueuedEvent {
            scheduled: Some(e.scheduled * 1000.0),
            order: if e.kind.is_transition() { 0 } else { 1 },
            event,
            target: super::node_handle(element),
            pseudo_rank: match e.key.pseudo_text().as_str() {
                "" => 0,
                "::marker" => 1,
                "::before" => 2,
                "::after" => 3,
                _ => 4,
            },
        });
    }

    /// Makes a new animation (§6.4 `Animation()`): in the global animation list.
    pub(crate) fn new_animation(
        &self,
        cx: &mut Cx,
        effect: Option<Handle>,
        timeline: Option<Handle>,
    ) -> rt::Result<Handle> {
        let sequence = {
            let mut st = self.animations.borrow_mut();
            st.next_sequence += 1;
            st.next_sequence
        };
        let h = self.alloc(Object::Animation(Box::new(AnimationData {
            css: None,
            css_key: None,
            css_controlled: false,
            css_effect: None,
            id: String::new(),
            timeline: None,
            effect: None,
            model: Model::default(),
            ready: LazyPromise::resolved(),
            finished: LazyPromise::default(),
            replace_state: ReplaceState::Active,
            sequence,
            finish_queued: false,
            compositor: None,
        })));
        self.animations.borrow_mut().list.push(h);
        self.set_animation_timeline(cx, h, timeline)?;
        self.set_animation_effect(cx, h, effect)?;
        Ok(h)
    }

    /// §4.5.2 "set the timeline of an animation".
    pub(crate) fn set_animation_timeline(
        &self,
        cx: &mut Cx,
        anim: Handle,
        timeline: Option<Handle>,
    ) -> rt::Result<()> {
        if self.anim(anim, |a| a.timeline) == timeline {
            return Ok(());
        }
        self.anim_mut(anim, |a| {
            a.timeline = timeline;
            if a.model.start_time.is_some() {
                a.model.hold_time = None;
            }
        });
        self.with_model(cx, anim, |m, env, notes| {
            m.update_finished_state(env, false, false, notes);
            Ok(())
        })
    }

    /// §4.5.3 "set the associated effect of an animation".
    pub(crate) fn set_animation_effect(
        &self,
        cx: &mut Cx,
        anim: Handle,
        effect: Option<Handle>,
    ) -> rt::Result<()> {
        let old = self.anim(anim, |a| a.effect);
        if old == effect {
            return Ok(());
        }
        if let Some(e) = effect {
            if let Some(previous) = self.effect(e, |e| e.animation) {
                if previous != anim {
                    self.set_animation_effect(cx, previous, None)?;
                }
            }
        }
        if let Some(o) = old {
            self.effect_mut(o, |e| e.animation = None);
        }
        if let Some(e) = effect {
            self.effect_mut(e, |e| e.animation = Some(anim));
        }
        self.anim_mut(anim, |a| a.effect = effect);
        self.css_effect_changed(anim);
        self.animations.borrow_mut().dirty = true;
        self.with_model(cx, anim, |m, env, notes| {
            m.update_finished_state(env, false, false, notes);
            Ok(())
        })
    }

    /// An effect's timing changed (§6.5.4 last step): its animation's finished state is updated.
    pub(crate) fn effect_timing_changed(&self, cx: &mut Cx, effect: Handle) {
        self.animations.borrow_mut().dirty = true;
        if let Some(a) = self.effect(effect, |e| e.animation) {
            let _ = self.with_model(cx, a, |m, env, notes| {
                m.update_finished_state(env, false, false, notes);
                Ok(())
            });
        }
    }

    // ----- relevance and composite order ---------------------------------------------------------------

    /// §4.6.7: the animation is relevant (its effect is current or in effect, and it is not removed).
    pub(crate) fn is_relevant(&self, anim: Handle) -> bool {
        let (effect, removed) = self.anim(anim, |a| {
            (a.effect, a.replace_state == ReplaceState::Removed)
        });
        let Some(effect) = effect else {
            return false;
        };
        if removed {
            return false;
        }
        let env = self.env(anim);
        let timing = self.effect(effect, |e| e.timing.clone());
        self.anim(anim, |a| relevant(&a.model, &env, &timing))
    }

    /// The live animations in composite order (the global animation list).
    pub(crate) fn animations_in_order(&self) -> Vec<Handle> {
        self.animations.borrow().list.clone()
    }

    // ----- update animations and send events (§4.4) ------------------------------------------------------

    /// Whether animations want a rendering update now (`now`: ms on the page clock).
    pub(crate) fn animations_need_frame(&self) -> bool {
        self.animations_frame_at()
            .is_some_and(|at| at <= self.now())
    }

    /// When animations next want a rendering update, ms on the page clock (`NEG_INFINITY`: at the next
    /// rendering opportunity): one has a pending task or there are queued events, or one runs an
    /// effect that could show. A running animation whose effect cannot show (its target is
    /// disconnected, in a document that is not rendered, or not being rendered) has nothing for an
    /// update to change on screen (HTML "update the rendering" lets the user agent skip a document
    /// whose update would have no visible effect); it asks only for the update at which it finishes,
    /// whose finish event and promise script can observe. Its times are those of the last update
    /// until then, as they would be between two updates.
    pub(crate) fn animations_frame_at(&self) -> Option<f64> {
        if self.use_trees_dirty() {
            return Some(f64::NEG_INFINITY);
        }
        let smil = self.smil_frame_at();
        if smil == Some(f64::NEG_INFINITY) {
            return smil;
        }
        // CSS animations a compositor runs ask for the update at their next change only
        // (`docs/compositor-animations.md`, S2; Blink's `DocumentTimeline::ScheduleNextService`).
        // (only the top-level document's frames carry compositor animations; its timeline counts
        // from the page's time origin)
        let iteration_events = self.animations.borrow().iteration_listeners;
        let compositor = self
            .with_renderer(|r, _| r.compositor_next_change(iteration_events))
            .flatten()
            .map(|s| s * 1000.0);
        let first = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        };
        let smil = first(smil, compositor);
        let list = {
            let st = self.animations.borrow();
            if !st.events.is_empty() {
                return Some(f64::NEG_INFINITY);
            }
            if st.list.is_empty() && st.css.is_empty() {
                return smil;
            }
            st.list
                .iter()
                .chain(st.css.values())
                .copied()
                .collect::<Vec<_>>()
        };
        let mut at: Option<f64> = smil;
        for a in list {
            // (changed since a compositor got it: the next update's frame carries the change, S3)
            if self.anim(a, |a| a.model.pending()) || self.compositor_pending(a) {
                return Some(f64::NEG_INFINITY);
            }
            let env = self.env(a);
            let (state, current, rate, effect, resolved) = self.anim(a, |a| {
                (
                    a.model.play_state(&env),
                    a.model.current_time(&env),
                    a.model.playback_rate,
                    a.effect,
                    a.model.finished_resolved,
                )
            });
            // (finished at a time script saw between rendering updates: the update that runs its
            // finished state, its promise and events, is due)
            if state == PlayState::Finished && !resolved {
                return Some(f64::NEG_INFINITY);
            }
            if state != PlayState::Running {
                continue;
            }
            // (a `CSSAnimation`/`CSSTransition` whose animation a compositor runs: its changes are
            // asked for above; a script animation a compositor runs asks for the update at its
            // next change)
            let script_on = effect.filter(|&e| self.script_on_compositor(a, e));
            if let Some(due) = script_on.and_then(|e| self.compositor_effect_change_at(a, e)) {
                at = Some(at.map_or(due, |t: f64| t.min(due)));
            }
            if effect.is_none_or(|e| self.effect_could_show(e))
                && !self.css_on_compositor(a)
                && script_on.is_none()
            {
                return Some(f64::NEG_INFINITY);
            }
            // (the time to the associated effect end, or to 0 backwards, at the playback rate)
            let Some(current) = current else { continue };
            let remaining = if rate > 0.0 {
                (env.effect_end - current) / rate
            } else if rate < 0.0 {
                current / -rate
            } else {
                continue;
            };
            if remaining.is_finite() {
                let due = self.event_loop.borrow().last_frame.max(0.0) + remaining.max(0.0);
                at = Some(at.map_or(due, |t: f64| t.min(due)));
            }
        }
        at
    }

    /// Whether animations have work left that ends (unlike a running animation, which would never
    /// let the loop settle): queued events, a pending play or pause task, or a finished state whose
    /// promise and events have not been run yet.
    pub(crate) fn animation_work_pending(&self) -> bool {
        let list = {
            let st = self.animations.borrow();
            if !st.events.is_empty() {
                return true;
            }
            st.list
                .iter()
                .chain(st.css.values())
                .copied()
                .collect::<Vec<_>>()
        };
        list.into_iter().any(|a| {
            if !matches!(&*self.obj(a), Object::Animation(_)) {
                return false;
            }
            let env = self.env(a);
            self.anim(a, |a| {
                a.model.pending()
                    || (!a.model.finished_resolved
                        && a.model.play_state(&env) == PlayState::Finished)
            })
        })
    }

    /// Whether `anim` is a `CSSAnimation` or `CSSTransition` whose animation a compositor runs.
    fn css_on_compositor(&self, anim: Handle) -> bool {
        let Some(key) = self.anim(anim, |a| {
            (!a.css_controlled).then(|| a.css_key.clone()).flatten()
        }) else {
            return false;
        };
        self.with_renderer(|r, _| r.css_on_compositor(&key))
            .unwrap_or(false)
    }

    /// Whether `anim`, a script animation, has its effect `effect` run by a compositor (Blink's
    /// `Animation::HasActiveAnimationsOnCompositor`).
    fn script_on_compositor(&self, anim: Handle, effect: Handle) -> bool {
        if self.anim(anim, |a| a.css_key.is_some() || a.css.is_some()) {
            return false;
        }
        self.with_renderer(|r, _| r.script_effect_on_compositor(effect.get()))
            .unwrap_or(false)
    }

    /// When (page clock, ms) the effect of `anim`, which a compositor runs, next changes in a way
    /// the page must service: Blink's `Animation::TimeToEffectChange` with
    /// `KeyframeEffect::CalculateTimeToEffectChange` (`keyframe_effect.cc`: "Need service to apply
    /// fill / fire events"; script animations have no iteration events). `None`: never.
    fn compositor_effect_change_at(&self, anim: Handle, effect: Handle) -> Option<f64> {
        let env = self.env(anim);
        let (local, rate, timeline) = self.anim(anim, |a| {
            (
                a.model.current_time(&env),
                a.model.playback_rate,
                a.timeline,
            )
        });
        let local = local?;
        let origin = match &*self.obj(timeline?) {
            Object::DocumentTimeline(t) => t.origin_time,
            _ => return None,
        };
        let now = env.timeline_time? + origin;
        let timing = self.effect(effect, |e| e.timing.clone());
        let forwards = rate > 0.0;
        let start = timing.delay;
        let end_minus_end_delay = start + timing.active_duration();
        let end = end_minus_end_delay + timing.end_delay;
        let after = end_minus_end_delay.min(end);
        let change = match timing.phase(Some(local), !forwards, false) {
            Phase::Idle => return None,
            Phase::Before if forwards => (start - local).max(0.0),
            Phase::Before if start > local => local.max(0.0),
            Phase::Before => return None,
            Phase::Active if forwards => after - local,
            Phase::Active => (local - start).max(0.0),
            Phase::After if forwards && end > local => end - local,
            Phase::After if forwards => return None,
            Phase::After => (local - after).max(0.0),
        };
        let wait = change / rate.abs();
        wait.is_finite().then_some(now + wait)
    }

    /// What a compositor needs to run `effect` (targeting `target`) itself, when its animation could
    /// run there (`docs/compositor-animations.md`, S2; Blink's
    /// `Animation::CheckCanStartAnimationOnCompositorInternal`, `animation.cc`, and
    /// `CompositorAnimations::ConvertTimingForCompositor`, `compositor_animations.cc`): it is
    /// running with no pending task or playback rate, its start time resolved, on a document
    /// timeline of the target's document, at a playback rate that is not 0; its effect's
    /// iteration duration is finite and above 0, its iteration count above 0, its delay finite, its
    /// end delay 0, and its active duration finite when it plays backwards.
    fn compositor_timing(&self, effect: Handle, target: NodeId) -> Option<CompositorTiming> {
        let anim = self.effect(effect, |e| e.animation)?;
        if self.play_state(anim) != PlayState::Running {
            return None;
        }
        let (timeline, start, rate, pending) = self.anim(anim, |a| {
            (
                a.timeline,
                a.model.start_time,
                a.model.playback_rate,
                a.model.pending() || a.model.pending_playback_rate.is_some(),
            )
        });
        if pending || self.anim(anim, |a| a.css.is_some()) {
            return None;
        }
        let (doc, origin) = match &*self.obj(timeline?) {
            Object::DocumentTimeline(t) => (t.document, t.origin_time),
            _ => return None,
        };
        if doc != self.dom().node_document(target) {
            return None;
        }
        let start = start?;
        let timing = self.effect(effect, |e| e.timing.clone());
        let duration = timing.iteration_duration();
        let ok = start.is_finite()
            && rate.is_finite()
            && rate != 0.0
            && duration.is_finite()
            && duration > 0.0
            && timing.iterations > 0.0
            && timing.delay.is_finite()
            && timing.end_delay == 0.0
            && (rate > 0.0 || timing.active_duration().is_finite());
        ok.then(|| CompositorTiming {
            start: (start + origin) / 1000.0,
            playback_rate: rate,
            timing,
        })
    }

    /// What a compositor would get of `anim` with a frame painted now ([`CompositorState`]): `None`
    /// without an effect.
    fn compositor_state_now(&self, anim: Handle) -> Option<CompositorState> {
        let effect = self.anim(anim, |a| a.effect)?;
        let (target, pseudo, keyframes, composite) = self.effect(effect, |e| {
            (e.target, e.pseudo.clone(), e.keyframes.clone(), e.composite)
        });
        let target = target.filter(|&t| self.dom().is_alive(t));
        let timing = target.and_then(|t| self.compositor_timing(effect, t));
        Some(CompositorState {
            effect,
            target,
            pseudo,
            keyframes,
            composite,
            timing,
        })
    }

    /// Whether `anim` changed since a compositor got it with the last frame painted: a seek, a new
    /// start time or playback rate, a pause, cancel or finish, a new effect, timeline, target,
    /// timing or keyframes (`docs/compositor-animations.md`, S3). Blink's
    /// `Animation::SetCompositorPending` (`animation.cc`) compares the animation with its
    /// `CompositorState` and then adds it to the document's pending animations, whose
    /// `PendingAnimations::Add` schedules a frame (`pending_animations.cc`); there
    /// `Animation::PreCommit` cancels it on the compositor and starts it again as it now is. Ours
    /// sends the whole set with every frame, so the frame of that rendering update is the restart.
    fn compositor_pending(&self, anim: Handle) -> bool {
        if self.anim(anim, |a| a.compositor.is_none()) {
            return false;
        }
        let now = self.compositor_state_now(anim);
        self.anim(anim, |a| match (&a.compositor, &now) {
            (Some(old), Some(now)) => !old.same_as(now),
            (Some(_), None) => true,
            (None, _) => false,
        })
    }

    /// After the top-level document painted a frame for a compositor: what the compositor runs of
    /// each script animation from now on (Blink's `Animation::PreCommit`, which makes the
    /// `CompositorState` when it starts the animation there and drops it when it cannot).
    pub(crate) fn note_compositor_frame(&self) {
        let list = self.animations_in_order();
        for a in list {
            let on = self
                .anim(a, |a| a.effect)
                .is_some_and(|e| self.script_on_compositor(a, e));
            let state = if on {
                self.compositor_state_now(a)
            } else {
                None
            };
            self.anim_mut(a, |a| a.compositor = state);
        }
    }

    /// A rendering update hands every change to the animations a compositor runs to the frame
    /// painted from it ([`Engine::note_compositor_frame`]): until then they ask for no other update
    /// (an embedder that paints no frame, as for a hidden page, leaves the compositor as it was).
    pub(crate) fn compositor_changes_handed_over(&self) {
        let list = self.animations_in_order();
        for a in list {
            if self.anim(a, |a| a.compositor.is_none()) {
                continue;
            }
            let state = self.compositor_state_now(a);
            self.anim_mut(a, |a| a.compositor = state);
        }
    }

    /// Whether an effect's target could show what it animates: it has no target (kept updating), or
    /// its target is connected, in the rendered document, and being rendered (or not styled yet, or
    /// the effect animates `display`).
    fn effect_could_show(&self, effect: Handle) -> bool {
        let Some(target) = self.effect(effect, |e| e.target) else {
            return true;
        };
        let chain: Vec<NodeId> = {
            let dom = self.dom();
            if !dom.is_alive(target) || !dom.is_connected(target) {
                return false;
            }
            let doc = dom.node_document(target);
            if !self.is_rendered_document(doc) {
                // (a page without rendering at all keeps its animations' updates)
                return self.rendered_top().is_none();
            }
            std::iter::once(target)
                .chain(dom.ancestors(target))
                .filter(|&n| dom.is_element(n))
                .collect()
        };
        let rendered = self
            .with_node_renderer(target, |r, _| {
                r.needs_restyle()
                    || chain.iter().all(|&n| {
                        r.element_style(n).is_some_and(|s| {
                            !matches!(
                                snowlark_layout::Style::display(&s),
                                snowlark_layout::style::Display::None
                            )
                        })
                    })
            })
            .unwrap_or(true);
        rendered
            || self.effect(effect, |e| {
                e.keyframes
                    .iter()
                    .any(|k| k.longhand_names().iter().any(|p| p == "display"))
            })
    }

    /// HTML "update the rendering" step "update animations and send events" for every document with
    /// timelines, at the frame's time `now` (ms, the page clock): the timelines move, pending tasks
    /// run, finished states update, replaced animations are removed, then (after a microtask
    /// checkpoint) the queued animation events are dispatched in order.
    pub(crate) fn update_animations_and_send_events(&self, cx: &mut Cx, frame_now: f64) {
        // Update the current time of all timelines.
        let mut docs: Vec<NodeId> = {
            let st = self.animations.borrow();
            st.doc_times
                .keys()
                .chain(st.timelines.keys())
                .copied()
                .collect()
        };
        let rendered = self.rendered_documents();
        for &d in &rendered {
            if !docs.contains(&d) {
                docs.push(d);
            }
        }
        for doc in docs {
            let t = if self.is_rendered_document(doc) {
                self.with_doc_renderer(doc, |r, _| r.clock_at(frame_now / 1000.0) * 1000.0)
                    .unwrap_or(frame_now)
            } else {
                frame_now
            };
            let mut st = self.animations.borrow_mut();
            let e = st.doc_times.entry(doc).or_insert(t);
            if t > *e {
                *e = t;
            }
            // (within the rendering update, and the rest of its task, the time holds)
            let task = st.task;
            st.doc_time_tasks.insert(doc, task);
        }
        // CSS animations' and transitions' events (CSS Animations 2 §4.1, CSS Transitions 2 §5.2).
        for doc in rendered {
            let Some(now) = self.animations.borrow().doc_times.get(&doc).copied() else {
                continue;
            };
            let events = self
                .with_doc_renderer(doc, |r, _| r.css_animation_events(now / 1000.0))
                .unwrap_or_default();
            for e in events {
                self.queue_css_event(e);
            }
        }
        // SVG 2 §5.6: the shadow trees of `use` elements follow what they clone.
        self.svg_use_sync(cx);
        // SVG Animations' time containers and events (SVG Animations §5.2).
        self.smil_update(cx);
        let mut list = self.animations_in_order();
        list.extend(self.animations.borrow().css.values().copied());
        if list.is_empty() && self.animations.borrow().events.is_empty() {
            return;
        }
        for &a in &list {
            if !matches!(&*self.obj(a), Object::Animation(_)) {
                continue;
            }
            let env = self.env(a);
            let mut notes = Vec::new();
            self.anim_mut(a, |a| {
                if a.model.pending() {
                    if let Some(t) = env.timeline_time {
                        a.model.run_pending_tasks(&env, t, &mut notes);
                    }
                }
                a.model
                    .update_finished_state(&env, false, false, &mut notes);
            });
            self.apply_notes(cx, a, &notes);
            if self.anim(a, |a| a.css_controlled && a.css_key.is_some()) {
                self.css_follow_model(a);
            }
        }
        self.remove_replaced_animations(cx);
        self.animations.borrow_mut().dirty = true;
        self.microtask_checkpoint(cx);
        // Dispatch the pending animation events, sorted by scheduled time then composite order.
        let mut events = std::mem::take(&mut self.animations.borrow_mut().events);
        // CSS events at the same time go in their elements' tree order.
        let positions = self.tree_positions(events.iter().filter_map(|e| {
            (e.order < 2)
                .then(|| super::handle_node(e.target))
                .flatten()
        }));
        let position = |e: &QueuedEvent| {
            super::handle_node(e.target)
                .and_then(|n| positions.get(&n).copied())
                .unwrap_or(usize::MAX)
        };
        events.sort_by(|a, b| {
            let time = match (a.scheduled, b.scheduled) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(x), Some(y)) => x.total_cmp(&y),
            };
            time.then(a.order.cmp(&b.order))
                .then(position(a).cmp(&position(b)))
                .then(a.pseudo_rank.cmp(&b.pseudo_rank))
        });
        for e in events {
            self.dispatch(cx, e.event, e.target, false);
        }
    }

    /// The shadow-including tree order positions of `nodes` within their documents.
    fn tree_positions(&self, nodes: impl Iterator<Item = NodeId>) -> FxHashMap<NodeId, usize> {
        let mut positions: FxHashMap<NodeId, usize> = nodes.map(|n| (n, usize::MAX)).collect();
        if positions.len() < 2 {
            return positions;
        }
        let dom = self.dom();
        let mut docs: Vec<NodeId> = positions
            .keys()
            .filter(|&&n| dom.is_alive(n))
            .map(|&n| dom.node_document(n))
            .collect();
        docs.sort_by_key(|d| d.to_bits());
        docs.dedup();
        for doc in docs {
            let mut i = 0;
            let mut n = Some(doc);
            while let Some(id) = n {
                if let Some(p) = positions.get_mut(&id) {
                    *p = i;
                }
                i += 1;
                n = dom.next_in_shadow_including_tree_order(id, doc);
            }
        }
        positions
    }

    /// §5.5.2 "remove replaced animations".
    fn remove_replaced_animations(&self, cx: &mut Cx) {
        let list = self.animations_in_order();
        // Replaceable animations in composite order, with their targets and properties.
        let mut replaceable: Vec<(Handle, NodeId, Option<String>, Vec<String>)> = Vec::new();
        for &a in &list {
            let (effect, state, timeline) =
                self.anim(a, |a| (a.effect, a.replace_state, a.timeline));
            if state == ReplaceState::Removed || timeline.is_none() {
                continue;
            }
            let Some(effect) = effect else { continue };
            if self.play_state(a) != PlayState::Finished {
                continue;
            }
            let (target, pseudo, props) = self.effect(effect, |e| {
                (e.target, e.pseudo.clone(), longhands(&e.keyframes))
            });
            let Some(target) = target else { continue };
            if !self.computed_timing(effect, false).in_effect() {
                continue;
            }
            replaceable.push((a, target, pseudo, props));
        }
        for i in 0..replaceable.len() {
            let (a, target, ref pseudo, ref props) = replaceable[i];
            if self.anim(a, |a| a.replace_state) != ReplaceState::Active {
                continue;
            }
            let covered = props.iter().all(|p| {
                replaceable[i + 1..]
                    .iter()
                    .any(|(_, t, ps, pr)| *t == target && ps == pseudo && pr.contains(p))
            });
            if !covered {
                continue;
            }
            self.anim_mut(a, |a| a.replace_state = ReplaceState::Removed);
            let env = self.env(a);
            let current = self.anim(a, |a| a.model.current_time(&env));
            let scheduled = env.timeline_time.and_then(|t| self.origin_relative(a, t));
            self.queue_playback_event(cx, a, "remove", current, env.timeline_time, scheduled);
        }
    }

    // ----- effects in the cascade ------------------------------------------------------------------------

    /// The sample of an effect current or in effect with an element target, for its document's
    /// style engine (a current one not in effect has no progress: Web Animations 1 §5.7).
    pub(crate) fn effect_sample(&self, effect: Handle, inclusive: bool) -> Option<EffectSample> {
        let t = self.computed_timing(effect, inclusive);
        let progress = t.progress;
        if progress.is_none()
            && !self
                .effect(effect, |e| e.animation)
                .is_some_and(|a| self.is_relevant(a))
        {
            return None;
        }
        let mut sample = self.effect(effect, |e| {
            if e.pseudo.is_some() {
                return None;
            }
            Some(EffectSample {
                target: e.target?,
                id: effect.get(),
                keyframes: e.keyframes.clone(),
                composite: e.composite,
                progress,
                iteration: t.current_iteration.unwrap_or(0.0),
                repeat: 1,
                compositor: None,
            })
        })?;
        sample.compositor = self.compositor_timing(effect, sample.target);
        Some(sample)
    }

    /// Hands each rendered document's style engine the script animations in effect in it, when the
    /// model changed (after API calls and at each frame).
    pub(crate) fn sync_animation_styles(&self) {
        if !std::mem::take(&mut self.animations.borrow_mut().dirty) {
            return;
        }
        let list = self.animations_in_order();
        let mut per_doc: FxHashMap<NodeId, Vec<EffectSample>> = FxHashMap::default();
        // (SMIL animations go first: they sit below script animations in the composite order)
        self.smil_effect_samples(&mut per_doc);
        // (then the effects script set on CSS animations: CSS animations sort before script
        // animations, CSS Animations 2 §4.2)
        for (doc, sample) in self.css_replaced_effect_samples() {
            per_doc
                .entry(doc)
                .or_insert_with(|| self.animations.borrow_mut().spare.pop().unwrap_or_default())
                .push(sample);
        }
        for a in list {
            let (effect, removed) =
                self.anim(a, |a| (a.effect, a.replace_state == ReplaceState::Removed));
            if removed {
                continue;
            }
            let Some(effect) = effect else { continue };
            let Some(sample) = self.effect_sample(effect, false) else {
                continue;
            };
            let doc = {
                let dom = self.dom();
                if !dom.is_alive(sample.target) {
                    continue;
                }
                dom.node_document(sample.target)
            };
            per_doc
                .entry(doc)
                .or_insert_with(|| self.animations.borrow_mut().spare.pop().unwrap_or_default())
                .push(sample);
        }
        let mut docs = self.rendered_documents();
        docs.retain(|d| !per_doc.contains_key(d));
        for doc in docs {
            per_doc.insert(
                doc,
                self.animations.borrow_mut().spare.pop().unwrap_or_default(),
            );
        }
        for (doc, samples) in per_doc {
            if let Some(mut old) = self.with_doc_renderer(doc, |r, st| {
                // (no script effects before or after: nothing on screen changes)
                let idle = samples.is_empty() && !r.has_script_effects();
                let old = r.set_script_effects(samples);
                if !idle {
                    st.dirty = true;
                }
                old
            }) {
                old.clear();
                self.animations.borrow_mut().spare.push(old);
            }
        }
    }

    /// The effects script set on `CSSAnimation` objects in place of their own (Web Animations 1
    /// §4.5.3), with their documents, in the order the objects were made. Stylo's animation applies
    /// nothing for them ([`Engine::css_effect_changed`]).
    fn css_replaced_effect_samples(&self) -> Vec<(NodeId, EffectSample)> {
        let mut objects: Vec<Handle> = {
            let st = self.animations.borrow();
            st.css.values().copied().collect()
        };
        objects.sort_by_key(|h| h.get());
        let mut out = Vec::new();
        for a in objects {
            let (effect, own) = self.anim(a, |a| (a.effect, a.css_effect));
            let Some(effect) = effect.filter(|&e| Some(e) != own) else {
                continue;
            };
            let Some(mut sample) = self.effect_sample(effect, false) else {
                continue;
            };
            // (stylo's animation stays the one a compositor may run, and it applies nothing)
            sample.compositor = None;
            let dom = self.dom();
            if !dom.is_alive(sample.target) {
                continue;
            }
            out.push((dom.node_document(sample.target), sample));
        }
        out
    }

    /// Script set the effect of a `CSSAnimation`: stylo's animation applies its keyframes only while
    /// the object's effect is its own.
    pub(crate) fn css_effect_changed(&self, h: Handle) {
        let Some(key) = self.anim(h, |a| a.css_key.clone()) else {
            return;
        };
        let replaced = self.anim(h, |a| a.effect != a.css_effect);
        let doc = {
            let dom = self.dom();
            if !dom.is_alive(key.element) {
                return;
            }
            dom.node_document(key.element)
        };
        self.with_doc_renderer(doc, |r, st| {
            r.css_effect_replaced(&key, replaced);
            st.dirty = true;
        });
        self.animations.borrow_mut().dirty = true;
    }

    /// Freed objects leave the animation lists.
    pub(crate) fn animation_freed(&self, h: Handle) {
        let mut st = self.animations.borrow_mut();
        st.list.retain(|&a| a != h);
        st.timelines.retain(|_, &mut t| t != h);
        st.css.retain(|_, &mut a| a != h);
    }

    /// The animations (and queued events) the collector must keep: animations with a pending task or
    /// notification, and relevant ones whose target is connected (or that have none). `None` when some
    /// state is borrowed (the pass waits).
    pub(crate) fn animation_pins(&self) -> Option<Vec<Handle>> {
        let st = self.animations.try_borrow().ok()?;
        let objects = self.objects.try_borrow().ok()?;
        let dom = self.dom.try_borrow().ok()?;
        let mut pins: Vec<Handle> = st.events.iter().flat_map(|e| [e.event, e.target]).collect();
        pins.extend(&st.held);
        for &a in &st.list {
            let Some(Object::Animation(d)) = objects.get(a.get() as usize - 1) else {
                continue;
            };
            if d.model.pending() || d.finish_queued {
                pins.push(a);
                continue;
            }
            if d.replace_state == ReplaceState::Removed {
                continue;
            }
            let Some(Object::KeyframeEffect(e)) =
                d.effect.and_then(|e| objects.get(e.get() as usize - 1))
            else {
                continue;
            };
            // (the documents' times as of the last update: nothing is read anew here)
            let timeline_time = d
                .timeline
                .and_then(|t| match objects.get(t.get() as usize - 1) {
                    Some(Object::DocumentTimeline(td)) => st
                        .doc_times
                        .get(&td.document)
                        .map(|now| now - td.origin_time),
                    _ => None,
                });
            let env = Env {
                has_timeline: d.timeline.is_some(),
                timeline_time,
                effect_end: e.timing.end_time(),
            };
            let connected = e
                .target
                .is_none_or(|n| dom.is_alive(n) && dom.is_connected(n));
            if connected && relevant(&d.model, &env, &e.timing) {
                pins.push(a);
            }
        }
        Some(pins)
    }

    // ----- the pseudo-element argument -----------------------------------------------------------------

    /// "Pseudo-element parsing" (§6.6 `pseudoElement`): the legacy single-colon forms become two-colon
    /// ones; a selector that is not a supported pseudo-element is a SyntaxError.
    pub(crate) fn parse_pseudo_element(value: Option<&str>) -> rt::Result<Option<String>> {
        let Some(v) = value else {
            return Ok(None);
        };
        let v = match v {
            ":before" | ":after" | ":first-letter" | ":first-line" => format!(":{v}"),
            _ => v.to_owned(),
        };
        const SUPPORTED: &[&str] = &[
            "::before",
            "::after",
            "::marker",
            "::first-letter",
            "::first-line",
            "::placeholder",
            "::selection",
            "::backdrop",
            "::file-selector-button",
        ];
        if SUPPORTED.contains(&v.as_str()) {
            Ok(Some(v))
        } else {
            Err(exception(
                ExceptionName::SyntaxError,
                "The pseudo-element selector is not valid",
            ))
        }
    }
}

/// The longhand (and custom) properties a set of keyframes animates.
pub(crate) fn longhands(keyframes: &[Keyframe]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in keyframes {
        for p in k.longhand_names() {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// §4.6.6-§4.6.7: an animation's effect (of timing `timing`) is current or in effect.
fn relevant(model: &Model, env: &Env, timing: &Timing) -> bool {
    use snowlark_style::web_animations::Phase;
    let rate = model.playback_rate;
    let t = timing.compute(model.current_time(env), rate < 0.0);
    if t.in_effect() {
        return true;
    }
    (t.phase == Phase::Active && model.play_state(env) != PlayState::Finished)
        || (t.phase == Phase::Before && rate > 0.0)
        || (t.phase == Phase::After && rate < 0.0)
}

fn finish_microtask(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(bits) = args.data().number_value(scope) else {
        return;
    };
    let anim = Handle::new(bits as u64);
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    let mut cx = Cx::new(scope);
    let queued = matches!(&*engine.obj(anim), Object::Animation(a) if a.finish_queued);
    if queued {
        engine.anim_mut(anim, |a| a.finish_queued = false);
        engine.finish_notification_steps(&mut cx, anim);
    }
}

/// Queues the microtask that runs the finish notification steps of `anim`.
fn queue_finish_microtask(cx: &mut Cx, anim: Handle) {
    let scope = cx.scope();
    let data = v8::Number::new(scope, anim.get() as f64);
    let Some(f) = v8::Function::builder(rt::guarded(finish_microtask))
        .data(data.into())
        .build(scope)
    else {
        return;
    };
    scope.enqueue_microtask(f);
}
