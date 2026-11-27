// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Events (DOM §2): `Event` and the UI and HTML event interfaces, `EventTarget` with its listener lists,
//! the dispatch algorithm with the composed path and shadow retargeting, event handlers (HTML §8.1.8),
//! `AbortController`/`AbortSignal`, and reporting exceptions (HTML §8.1.4.7).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use snowlark_bindings::generated::{
    self as g, AbortControllerImpl, AbortSignalImpl, AddEventListenerOptionsOrBoolean,
    BeforeUnloadEventImpl, ClipboardEventImpl, CompositionEventImpl, CustomEventImpl,
    DragEventImpl, ErrorEventImpl, EventImpl, EventListener, EventListenerOptionsOrBoolean,
    EventTargetImpl, FocusEventImpl, HashChangeEventImpl, InputDeviceCapabilitiesImpl,
    InputEventImpl, KeyboardEventImpl, MouseEventImpl, PageTransitionEventImpl, PointerEventImpl,
    PopStateEventImpl, ProgressEventImpl, PromiseRejectionEventImpl, SubmitEventImpl,
    TextEventImpl, ToggleEventImpl, UIEventImpl, WheelEventImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsObject, JsValue, ToJs};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, Dom, NodeData, NodeId, ShadowRootMode};

use super::pointer_tilt::{
    altitude_from_tilt, altitude_in_range, azimuth_from_tilt, azimuth_in_range, tilt_in_range,
    tilt_x_from_spherical, tilt_y_from_spherical,
};

use super::{exception, handle_node, node_handle, Engine, Object};

/// One registered event listener (DOM §2.7 "event listener").
pub struct Listener {
    pub type_: String,
    /// The callback (none for an event handler's listener).
    pub callback: RefCell<Option<Callback>>,
    /// The callback's callback context, when it is not its own realm ([`rt::callback_context`]).
    pub callback_context: Option<Handle>,
    pub capture: bool,
    pub passive: bool,
    pub once: bool,
    pub removed: Cell<bool>,
    /// The listener of an event handler (HTML §8.1.8.1): runs the event handler processing algorithm for
    /// this event type instead of a callback.
    pub handler: Option<&'static str>,
}

/// The vectors of a dispatch, kept for the next one (empty between dispatches).
#[derive(Default)]
pub struct DispatchScratch {
    path: Vec<PathEntry>,
    /// Each struct's invocation target (invoke step 1).
    targets: Vec<Option<Handle>>,
    /// Each struct's: 1 when its item had listeners of the event's type as the dispatch began, 2
    /// when it has a shadow-adjusted target.
    flags: Vec<u8>,
}

/// An event target's event listener list (DOM §2.7), kept by type: one group per type, its listeners in
/// the order they were added. Inner invoke only ever walks the listeners of one type, so the order
/// between types is never observed by dispatch.
/// A type and its listeners, in the order they were added.
type ListenerGroup = (Box<str>, Vec<Rc<Listener>>);

#[derive(Clone, Default)]
pub struct ListenerList {
    /// One group per type: kept when its last listener is removed, as a list keeps its capacity, so
    /// that adding one again allocates nothing.
    groups: smallvec::SmallVec<[ListenerGroup; 2]>,
}

impl ListenerList {
    /// The listeners whose type is `type_`, in the order they were added.
    pub fn of_type(&self, type_: &str) -> &[Rc<Listener>] {
        self.groups
            .iter()
            .find(|g| &*g.0 == type_)
            .map_or(&[], |g| g.1.as_slice())
    }

    pub fn push(&mut self, l: Rc<Listener>) {
        match self.groups.iter_mut().find(|g| *g.0 == *l.type_) {
            Some(g) => g.1.push(l),
            None => self.groups.push((l.type_.as_str().into(), vec![l])),
        }
    }

    /// Removes `l` (the same listener).
    pub fn remove(&mut self, l: &Rc<Listener>) {
        if let Some(g) = self.groups.iter_mut().find(|g| *g.0 == *l.type_) {
            if let Some(j) = g.1.iter().position(|x| Rc::ptr_eq(x, l)) {
                g.1.remove(j);
            }
        }
    }

    /// Removes the first listener of `type_` that `is` accepts, flagged removed.
    pub fn remove_first(&mut self, type_: &str, mut is: impl FnMut(&Listener) -> bool) {
        if let Some(g) = self.groups.iter_mut().find(|g| &*g.0 == type_) {
            if let Some(j) = g.1.iter().position(|l| is(l)) {
                g.1.remove(j).removed.set(true);
            }
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&Rc<Listener>) -> bool) {
        for g in &mut self.groups {
            g.1.retain(&mut keep);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|g| g.1.is_empty())
    }

    /// Every listener, by type.
    pub fn iter(&self) -> ListenerIter<'_> {
        fn of_group(g: &ListenerGroup) -> std::slice::Iter<'_, Rc<Listener>> {
            g.1.iter()
        }
        let f: for<'g> fn(&'g ListenerGroup) -> std::slice::Iter<'g, Rc<Listener>> = of_group;
        self.groups.iter().flat_map(f)
    }
}

/// [`ListenerList::iter`]'s iterator.
pub type ListenerIter<'a> = std::iter::FlatMap<
    std::slice::Iter<'a, ListenerGroup>,
    std::slice::Iter<'a, Rc<Listener>>,
    for<'g> fn(&'g ListenerGroup) -> std::slice::Iter<'g, Rc<Listener>>,
>;

impl<'a> IntoIterator for &'a ListenerList {
    type Item = &'a Rc<Listener>;
    type IntoIter = ListenerIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// A listener's or event handler's callback: strong until its target's wrapper is weak, then kept
/// by that wrapper (engine/held.rs), so that a callback whose closure reaches its target (or
/// anything of the target's group or realm) does not keep them alive.
pub type Callback = super::held::Held<v8::Object>;

/// An event handler's value (HTML §8.1.8.1): a callback, or an uncompiled body from a content attribute.
#[derive(Clone)]
pub enum Handler {
    /// With its callback context, when that is not the callback's own realm.
    Compiled(Callback, Option<Handle>),
    Uncompiled {
        body: String,
        /// The element whose content attribute it came from (for the scope chain and the URL).
        element: Option<NodeId>,
        line: u32,
    },
}

/// The fields of the UI event interfaces (UI Events, Pointer Events), flattened: each interface reads
/// the ones its init dictionary sets.
#[derive(Default, Clone)]
pub struct UiFields {
    pub view: Option<Handle>,
    pub detail: i32,
    pub which: u32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
    /// The other modifier states (AltGraph, CapsLock...), by key name.
    pub modifiers: Vec<&'static str>,
    pub screen_x: f64,
    pub screen_y: f64,
    pub client_x: f64,
    pub client_y: f64,
    pub movement_x: f64,
    pub movement_y: f64,
    pub button: i16,
    pub buttons: u16,
    pub related_target: Option<Handle>,
    pub delta_x: f64,
    pub delta_y: f64,
    pub delta_z: f64,
    pub delta_mode: u32,
    pub momentum: bool,
    /// The legacy `wheelDeltaX`/`wheelDeltaY` (Chromium's `wheel_delta_`).
    pub wheel_delta: (i32, i32),
    pub pointer_id: i32,
    pub width: f64,
    pub height: f64,
    pub pressure: f32,
    pub tangential_pressure: f32,
    pub tilt_x: i32,
    pub tilt_y: i32,
    pub twist: i32,
    pub altitude_angle: f64,
    pub azimuth_angle: f64,
    pub pointer_type: String,
    pub is_primary: bool,
    pub persistent_device_id: i32,
    /// Pointer Events 3: the coalesced events list (`getCoalescedEvents()`).
    pub coalesced: Vec<Handle>,
    /// Pointer Events 3: the predicted events list (`getPredictedEvents()`).
    pub predicted: Vec<Handle>,
    pub key: String,
    pub code: String,
    pub location: u32,
    pub repeat: bool,
    pub is_composing: bool,
    pub char_code: u32,
    pub key_code: u32,
    pub data: Option<String>,
    pub input_type: String,
    /// Input Events: the StaticRanges a `beforeinput` will change (`getTargetRanges()`).
    pub target_ranges: Vec<Handle>,
    /// Input Events: the DataTransfer of an insertion from a paste or drop.
    pub data_transfer: Option<Handle>,
    /// Touch Events: a TouchEvent's `touches`, `targetTouches` and `changedTouches` (TouchLists).
    pub touch_lists: Option<[Handle; 3]>,
}

/// What an event carries beyond `Event`.
///
/// The JS values an event carries (a `detail`, `data`, `error`, `state`, promise or reason) are
/// [`Held`](super::held::Held): strong while the event has no weak wrapper, then kept by its wrapper
/// ([`Engine::gc_trace_event_values`]). A strong handle to a value of a realm would keep the realm,
/// and through it the event's own wrapper, alive for good (a removed frame's events kept the frame).
pub enum EventExt {
    None,
    Custom(Option<super::held::Held<v8::Value>>),
    /// An ExtendableEvent's extend lifetime promises (Service Workers §4.4).
    Extendable(std::rc::Rc<crate::service_worker::Extensions>),
    Ui(Box<UiFields>),
    Error {
        message: String,
        filename: String,
        lineno: u32,
        colno: u32,
        error: Option<super::held::Held<v8::Value>>,
    },
    Rejection {
        promise: super::held::Held<v8::Object>,
        reason: Option<super::held::Held<v8::Value>>,
    },
    HashChange {
        old_url: String,
        new_url: String,
    },
    PageTransition(bool),
    /// `WebGLContextEvent.statusMessage`.
    WebGlContext(String),
    PopState(Option<super::held::Held<v8::Value>>),
    Progress {
        length_computable: bool,
        loaded: f64,
        total: f64,
    },
    Submit(Option<Handle>),
    /// Clipboard API `ClipboardEvent`: its `clipboardData`.
    Clipboard(Option<Handle>),
    /// HTML §4.10.21.4 `FormDataEvent`: the FormData of the entry list being constructed.
    FormData(Handle),
    /// HTML §4.10.6 `CommandEvent`: the command and the element that invoked it.
    Command {
        command: String,
        source: Option<Handle>,
    },
    Toggle {
        old_state: String,
        new_state: String,
        source: Option<Handle>,
    },
    BeforeUnload(String),
    Message {
        data: Option<super::held::Held<v8::Value>>,
        origin: String,
        last_event_id: String,
        source: Option<Handle>,
        /// The `ports` (MessagePort handles).
        ports: Vec<Handle>,
    },
    MediaQuery {
        media: String,
        matches: bool,
    },
    FontFaces(Vec<Handle>),
    Storage(super::storage::StorageFields),
    /// `TrackEvent.track` (HTML §4.8.11.11.?).
    Track(Option<Handle>),
    /// EME `MediaKeyMessageEvent`: its type and `message` ArrayBuffer.
    MediaKeyMessage {
        message_type: snowlark_bindings::generated::MediaKeyMessageType,
        message: Option<super::held::Held<v8::Object>>,
    },
    /// EME `MediaEncryptedEvent`: `initDataType` and the `initData` ArrayBuffer.
    Encrypted {
        init_data_type: String,
        init_data: Option<super::held::Held<v8::Object>>,
    },
    /// A `SecurityPolicyViolationEvent`'s fields.
    Csp(Box<snowlark_net::report::ViolationReport>),
    /// An `AnimationPlaybackEvent`'s fields.
    AnimationPlayback {
        current_time: Option<f64>,
        timeline_time: Option<f64>,
    },
    /// An `AnimationEvent`'s or `TransitionEvent`'s fields.
    CssAnimation(Box<super::animation::api::CssEventFields>),
    /// A `TaskPriorityChangeEvent`'s `previousPriority`.
    TaskPriorityChange(snowlark_bindings::generated::TaskPriority),
    /// A `CloseEvent`'s fields (WebSockets §4).
    /// An OfflineAudioCompletionEvent's `renderedBuffer`.
    AudioCompletion(Handle),
    Close {
        was_clean: bool,
        code: u16,
        reason: String,
    },
    /// An `IDBVersionChangeEvent`'s versions (IndexedDB §4.2).
    VersionChange {
        old: u64,
        new: Option<u64>,
    },
}

impl EventExt {
    /// From now on `owner`'s wrapper (the event's own) keeps the JS values the event carries
    /// ([`Held::trace`](super::held::Held::trace)).
    pub(crate) fn trace(&mut self, scope: &mut v8::PinScope<'_, '_>, owner: Handle) {
        match self {
            EventExt::Custom(Some(v))
            | EventExt::PopState(Some(v))
            | EventExt::Error { error: Some(v), .. }
            | EventExt::Message { data: Some(v), .. } => v.trace(scope, owner),
            EventExt::MediaKeyMessage {
                message: Some(o), ..
            }
            | EventExt::Encrypted {
                init_data: Some(o), ..
            } => o.trace(scope, owner),
            EventExt::Rejection { promise, reason } => {
                promise.trace(scope, owner);
                if let Some(r) = reason {
                    r.trace(scope, owner);
                }
            }
            _ => {}
        }
    }
}

/// The roots of the nodes an event's path walks up through. A node's parent in its tree has the
/// same root, so each tree's root is walked to once per dispatch, not once per path entry.
#[derive(Default)]
struct PathRoots {
    last: Option<(NodeId, NodeId)>,
}

impl PathRoots {
    fn of(&mut self, dom: &Dom, n: NodeId) -> NodeId {
        let root = match self.last {
            Some((prev, root)) if prev == n || dom.parent(prev) == Some(n) => root,
            _ => dom.root(n),
        };
        self.last = Some((n, root));
        root
    }
}

/// One struct of an event's path (DOM §2.9 "append to an event path").
#[derive(Clone, Copy, Debug)]
pub struct PathEntry {
    pub item: Handle,
    pub item_in_shadow_tree: bool,
    pub shadow_adjusted_target: Option<Handle>,
    pub related_target: Option<Handle>,
    pub root_of_closed_tree: bool,
    pub slot_in_closed_tree: bool,
}

/// An event (DOM §2.2).
pub struct EventData {
    pub interface: InterfaceId,
    pub type_: String,
    pub target: Option<Handle>,
    pub related_target: Option<Handle>,
    pub current_target: Option<Handle>,
    pub phase: u16,
    pub bubbles: bool,
    pub cancelable: bool,
    pub composed: bool,
    pub is_trusted: bool,
    pub stop_propagation: bool,
    pub stop_immediate: bool,
    pub canceled: bool,
    pub in_passive_listener: bool,
    pub initialized: bool,
    pub dispatching: bool,
    pub time_stamp: f64,
    pub path: Vec<PathEntry>,
    pub ext: EventExt,
}

impl EventData {
    pub fn interface(&self) -> InterfaceId {
        self.interface
    }
}

/// AbortSignal state (DOM §3.2).
#[derive(Default)]
pub struct AbortSignalData {
    pub aborted: bool,
    /// (kept by the signal's wrapper once it is weak: [`EventExt`]'s values are, for the same reason)
    pub reason: Option<super::held::Held<v8::Value>>,
    /// Signals that follow this one (`AbortSignal.any`).
    pub dependents: Vec<Handle>,
    pub dependent: bool,
    pub sources: Vec<Handle>,
    /// Abort algorithms: listeners to remove from targets when aborted.
    pub remove_listeners: Vec<(Handle, std::rc::Weak<Listener>)>,
    /// A timer for `AbortSignal.timeout`.
    pub timeout: Option<u32>,
    /// Abort algorithms: script fetches to abort.
    pub fetches: Vec<u32>,
    /// Abort algorithms: scheduler task handles (`scheduler.postTask`, `yield`).
    pub scheduled: Vec<u64>,
    /// A TaskSignal's slots (Prioritized Task Scheduling).
    pub task: Option<Box<super::scheduler::TaskSignalData>>,
}

const NONE: u16 = 0;
const CAPTURING: u16 = 1;
const AT_TARGET: u16 = 2;
const BUBBLING: u16 = 3;

/// DOM §2.9 "invoke" steps 1-2 for the path's struct `s`: the event's target is `target` (the
/// shadow-adjusted target of the nearest struct at or before `s` that has one), its relatedTarget the
/// struct's.
fn set_invocation_targets(e: &mut EventData, s: &PathEntry, target: Option<Handle>) {
    let related = s.related_target;
    e.target = target;
    e.related_target = related;
    // (the relatedTarget a MouseEvent or FocusEvent shows)
    if let EventExt::Ui(u) = &mut e.ext {
        if related.is_some() || u.related_target.is_some() {
            u.related_target = related;
        }
    }
}

/// DOM §2.9 "invoke" step 7: the legacy prefixed name trusted events of `type_` are also delivered to.
/// `wheel` has `mousewheel`, the name of the event before UI Events' `wheel`: engines that fired it
/// deliver a trusted `wheel` to `mousewheel` listeners the same way, and pages that choose the old
/// name by browser listen for it alone.
fn legacy_type(type_: &str) -> Option<&'static str> {
    match type_ {
        "animationend" => Some("webkitAnimationEnd"),
        "animationiteration" => Some("webkitAnimationIteration"),
        "animationstart" => Some("webkitAnimationStart"),
        "transitionend" => Some("webkitTransitionEnd"),
        "wheel" => Some("mousewheel"),
        _ => None,
    }
}

/// "Uncaught …" for a console line (V8's message text often says it already).
fn uncaught(text: &str) -> String {
    if text.starts_with("Uncaught") {
        text.to_owned()
    } else {
        format!("Uncaught {text}")
    }
}

impl Engine {
    // ----- event objects ---------------------------------------------------------------------------------

    pub(crate) fn ev<R>(&self, h: Handle, f: impl FnOnce(&EventData) -> R) -> R {
        match &*self.obj(h) {
            Object::Event(e) => f(e),
            _ => panic!("not an event"),
        }
    }

    pub(crate) fn ev_mut<R>(&self, h: Handle, f: impl FnOnce(&mut EventData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::Event(e) => f(e),
            _ => panic!("not an event"),
        }
    }

    pub(super) fn ui<R>(&self, h: Handle, f: impl FnOnce(&UiFields) -> R) -> R
    where
        R: Default,
    {
        self.ev(h, |e| match &e.ext {
            EventExt::Ui(u) => f(u),
            _ => R::default(),
        })
    }

    fn ui_str(&self, h: Handle, f: impl FnOnce(&UiFields) -> &str) -> DomStr<'_> {
        DomStr::Guard(std::cell::Ref::map(self.objects.borrow(), |o| {
            match &o[h.get() as usize - 1] {
                Object::Event(e) => match &e.ext {
                    EventExt::Ui(u) => f(u),
                    _ => "",
                },
                _ => "",
            }
        }))
    }

    /// A new event object (DOM §2.5 "create an event" and the constructor's inner steps).
    pub(crate) fn new_event(
        &self,
        interface: InterfaceId,
        type_: &str,
        bubbles: bool,
        cancelable: bool,
        composed: bool,
        ext: EventExt,
    ) -> Handle {
        self.alloc(Object::Event(Box::new(EventData {
            interface,
            type_: type_.to_owned(),
            target: None,
            related_target: None,
            current_target: None,
            phase: NONE,
            bubbles,
            cancelable,
            composed,
            is_trusted: false,
            stop_propagation: false,
            stop_immediate: false,
            canceled: false,
            in_passive_listener: false,
            initialized: true,
            dispatching: false,
            time_stamp: self.now(),
            path: Vec::new(),
            ext,
        })))
    }

    /// `document.createEvent`: an event of the interface with its initialized flag unset.
    pub(crate) fn create_uninitialized_event(&self, interface: InterfaceId) -> Handle {
        let ext = match interface {
            InterfaceId::CustomEvent => EventExt::Custom(None),
            InterfaceId::Event => EventExt::None,
            InterfaceId::HashChangeEvent => EventExt::HashChange {
                old_url: String::new(),
                new_url: String::new(),
            },
            InterfaceId::BeforeUnloadEvent => EventExt::BeforeUnload(String::new()),
            _ => EventExt::Ui(Box::default()),
        };
        let h = self.new_event(interface, "", false, false, false, ext);
        self.ev_mut(h, |e| e.initialized = false);
        h
    }

    /// A trusted event the engine fires (DOM §2.10 "fire an event").
    pub(crate) fn trusted_event(
        &self,
        interface: InterfaceId,
        type_: &str,
        bubbles: bool,
        cancelable: bool,
        ext: EventExt,
    ) -> Handle {
        let h = self.new_event(interface, type_, bubbles, cancelable, false, ext);
        self.ev_mut(h, |e| e.is_trusted = true);
        h
    }

    /// "Fire an event named `type_` at `target`" with the `Event` interface.
    pub(crate) fn fire_event(
        &self,
        cx: &mut Cx,
        target: Handle,
        type_: &str,
        bubbles: bool,
        cancelable: bool,
    ) -> bool {
        let e = self.trusted_event(
            InterfaceId::Event,
            type_,
            bubbles,
            cancelable,
            EventExt::None,
        );
        self.dispatch(cx, e, target, false)
    }

    /// The inner event creation steps' init for the common members.
    fn init_event(&self, h: Handle, type_: &str, bubbles: bool, cancelable: bool) {
        self.ev_mut(h, |e| {
            e.initialized = true;
            e.stop_propagation = false;
            e.stop_immediate = false;
            e.canceled = false;
            e.is_trusted = false;
            e.target = None;
            e.type_ = type_.to_owned();
            e.bubbles = bubbles;
            e.cancelable = cancelable;
        });
    }

    /// "Set the canceled flag" (DOM §2.2).
    fn set_canceled(&self, h: Handle) {
        self.ev_mut(h, |e| {
            if e.cancelable && !e.in_passive_listener {
                e.canceled = true;
            }
        });
    }

    // ----- the parent and retargeting ------------------------------------------------------------------

    /// DOM §4.8 "retarget A against B".
    pub(crate) fn retarget(&self, dom: &Dom, mut a: NodeId, b: NodeId) -> NodeId {
        loop {
            let root = dom.root(a);
            if !dom.is_shadow_root(root) || dom.is_shadow_including_inclusive_ancestor_of(root, b) {
                return a;
            }
            a = dom.host(root).expect("a shadow root has a host");
        }
    }

    fn retarget_handle(&self, a: Option<Handle>, b: Handle) -> Option<Handle> {
        let a = a?;
        let Some(mut an) = handle_node(a) else {
            return Some(a);
        };
        let dom = self.dom();
        let Some(bn) = handle_node(b) else {
            // (B is not a node, as a Window: A leaves every shadow tree)
            while dom.is_shadow_root(dom.root(an)) {
                an = dom.host(dom.root(an)).expect("a shadow root has a host");
            }
            return Some(node_handle(an));
        };
        Some(node_handle(self.retarget(&dom, an, bn)))
    }

    /// "Get the parent" of an event target (DOM §2.7, §4.4, §4.8, HTML §7.2), with `known_slot` the
    /// target's assigned slot when the caller found it already.
    fn get_the_parent(
        &self,
        target: Handle,
        event: Handle,
        path_first: Option<Handle>,
        known_slot: Option<Option<NodeId>>,
    ) -> Option<Handle> {
        let Some(node) = handle_node(target) else {
            // IndexedDB's requests and transactions have parents; Window and other targets
            // have none.
            return self.idb_parent(target);
        };
        let dom = self.dom();
        match dom.data(node) {
            NodeData::Document(_) => {
                let is_load = self.ev(event, |e| e.type_ == "load");
                if is_load || !self.is_active_document(node) {
                    None
                } else {
                    self.window_of_document(node)
                }
            }
            NodeData::ShadowRoot(s) => {
                let composed = self.ev(event, |e| e.composed);
                let first = path_first.and_then(handle_node);
                if !composed && first.is_some_and(|f| dom.root(f) == node) {
                    None
                } else {
                    Some(node_handle(s.host()))
                }
            }
            _ => {
                let parent = dom.parent(node);
                drop(dom);
                let slot = match known_slot {
                    Some(s) => s,
                    None => self.assigned_slot_of(node),
                };
                slot.or(parent).map(node_handle)
            }
        }
    }

    fn root_is_closed_shadow(&self, dom: &Dom, n: NodeId) -> bool {
        let root = dom.root(n);
        dom.shadow_root_data(root)
            .is_some_and(|s| s.mode() == ShadowRootMode::Closed)
    }

    // ----- dispatch -------------------------------------------------------------------------------------

    /// DOM §2.9 "dispatch" `event` to `target`. Returns false when the event was canceled.
    pub(crate) fn dispatch(
        &self,
        cx: &mut Cx,
        event: Handle,
        target: Handle,
        legacy_target_override: bool,
    ) -> bool {
        self.dispatch_flag(cx, event, target, legacy_target_override)
            .0
    }

    /// "Dispatch" with its legacyOutputDidListenersThrowFlag: (not canceled, a listener threw).
    pub(crate) fn dispatch_flag(
        &self,
        cx: &mut Cx,
        event: Handle,
        target: Handle,
        legacy_target_override: bool,
    ) -> (bool, bool) {
        // Nothing can listen at a freed node, and whatever kept its id across a collection (a task,
        // a saved target) must not take the tab down with it.
        if handle_node(target).is_some_and(|n| !self.dom().is_alive(n)) {
            return (true, false);
        }
        let outer = self.listener_threw.replace(false);
        // Event Timing: a trusted input event's processing times around its dispatch.
        let not_canceled = if self.event_timing_wanted(event, target) {
            let processing_start = self.now();
            let r = self.dispatch_steps(cx, event, target, legacy_target_override);
            self.event_timing_dispatched(event, target, processing_start);
            r
        } else {
            self.dispatch_steps(cx, event, target, legacy_target_override)
        };
        let threw = self.listener_threw.replace(outer);
        if self.smil_has_conditions() {
            if let Some(n) = handle_node(target) {
                let (type_, bubbles) = self.ev(event, |e| (e.type_.clone(), e.bubbles));
                self.smil_event(cx, &type_, n, bubbles);
            }
        }
        (not_canceled, threw)
    }

    /// DOM §2.9 "dispatch".
    fn dispatch_steps(
        &self,
        cx: &mut Cx,
        event: Handle,
        target: Handle,
        legacy_target_override: bool,
    ) -> bool {
        // 1.
        self.ev_mut(event, |e| e.dispatching = true);
        // 2.
        let target_override = if legacy_target_override {
            self.document_of_window(target)
                .map(node_handle)
                .unwrap_or(target)
        } else {
            target
        };
        // 3-4.
        let mut activation_target: Option<Handle> = None;
        let event_related = self.ev(event, |e| e.related_target);
        let mut related = self.retarget_handle(event_related, target);
        let mut clear_targets = false;
        // 6.
        if Some(target) != related || Some(target) == event_related {
            // (event's type cannot change while it is dispatched: `initEvent` does nothing then)
            let type_ = self.ev(event, |e| e.type_.clone());
            // (the vectors of an earlier dispatch, reused: paths are as deep as the tree)
            let mut scratch = self.dispatch_scratch.borrow_mut().pop().unwrap_or_default();
            let mut path = std::mem::take(&mut scratch.path);
            let mut roots = PathRoots::default();
            let mut root_of = |h: Handle| handle_node(h).map(|n| roots.of(&self.dom(), n));
            let in_shadow =
                |root: Option<NodeId>| root.is_some_and(|r| self.dom().is_shadow_root(r));
            let closed_root = |h: Handle| {
                handle_node(h).is_some_and(|n| {
                    self.dom()
                        .shadow_root_data(n)
                        .is_some_and(|s| s.mode() == ShadowRootMode::Closed)
                })
            };
            let target_root = root_of(target);
            path.push(PathEntry {
                item: target,
                item_in_shadow_tree: in_shadow(target_root),
                shadow_adjusted_target: Some(target_override),
                related_target: related,
                root_of_closed_tree: closed_root(target),
                slot_in_closed_tree: false,
            });
            let is_activation =
                type_ == "click" && self.ev(event, |e| super::html::is_mouse_event(e.interface));
            if is_activation && self.has_activation_behavior(target) {
                activation_target = Some(target);
            }
            // (an item's assigned slot, found once: it decides both whether the item is a
            // slottable and, as its parent, where the path goes next)
            let target_slot = handle_node(target).map(|n| self.assigned_slot_of(n));
            let mut slottable = handle_node(target).filter(|_| target_slot.flatten().is_some());
            let mut slot_in_closed_tree = false;
            let mut parent = self.get_the_parent(target, event, Some(target), target_slot);
            // (the root of the tree of the last shadow-adjusted target on the path)
            let mut current_root = target_root;
            let bubbles = self.ev(event, |e| e.bubbles);
            while let Some(p) = parent {
                let p_root = root_of(p);
                let p_node = handle_node(p);
                if slottable.is_some() {
                    slottable = None;
                    if let Some(pn) = p_node {
                        if self.root_is_closed_shadow(&self.dom(), pn) {
                            slot_in_closed_tree = true;
                        }
                    }
                }
                let p_slot = p_node.map(|pn| self.assigned_slot_of(pn));
                if p_slot.flatten().is_some() {
                    slottable = p_node;
                }
                if event_related.is_some() {
                    related = self.retarget_handle(event_related, p);
                }
                // ("the root of target" for the current target: a root is an inclusive ancestor of
                // every node in its tree)
                let target_root_contains = match (current_root, handle_node(p)) {
                    (Some(r), Some(pn)) => {
                        p_root == Some(r)
                            || self.dom().is_shadow_including_inclusive_ancestor_of(r, pn)
                    }
                    _ => false,
                };
                // (a node is never a Window)
                let parent_is_window = p_node.is_none() && self.context_of_window(p).is_some();
                // (a parent of a target that is not a node, as IndexedDB's requests and
                // transactions have, does not retarget the event: browsers and IndexedDB's tests
                // keep the request as the target in its transaction's and connection's listeners)
                let object_chain = p_node.is_none() && handle_node(target).is_none();
                if parent_is_window || target_root_contains || object_chain {
                    if is_activation
                        && bubbles
                        && activation_target.is_none()
                        && self.has_activation_behavior(p)
                    {
                        activation_target = Some(p);
                    }
                    path.push(PathEntry {
                        item: p,
                        item_in_shadow_tree: in_shadow(p_root),
                        shadow_adjusted_target: None,
                        related_target: related,
                        root_of_closed_tree: closed_root(p),
                        slot_in_closed_tree,
                    });
                } else if Some(p) == related {
                    parent = None;
                } else {
                    current_root = p_root;
                    if is_activation
                        && activation_target.is_none()
                        && self.has_activation_behavior(p)
                    {
                        activation_target = Some(p);
                    }
                    path.push(PathEntry {
                        item: p,
                        item_in_shadow_tree: in_shadow(p_root),
                        shadow_adjusted_target: Some(p),
                        related_target: related,
                        root_of_closed_tree: closed_root(p),
                        slot_in_closed_tree,
                    });
                }
                if parent.is_some() {
                    parent = self.get_the_parent(p, event, Some(path[0].item), p_slot);
                }
                slot_in_closed_tree = false;
            }
            // HTML §4.10.18.5: a disabled form control prevents the click events of user
            // interaction from being dispatched on it. A trusted mouse button event (mousedown,
            // mouseup, click, dblclick) stops below the first disabled form control on its path:
            // the control and what contains it do not see it, what is inside it still does.
            let button_event = self.ev(event, |e| {
                e.is_trusted
                    && super::html::is_mouse_event(e.interface)
                    && ["mousedown", "mouseup", "click", "dblclick"]
                        .iter()
                        .any(|t| e.type_ == *t)
            });
            if button_event {
                // (a disabled fieldset is not itself a disabled form control: it disables the
                // controls in it)
                let cut = path.iter().position(|s| {
                    handle_node(s.item).is_some_and(|n| {
                        let dom = self.dom();
                        super::html::is_disabled(&dom, n)
                            && !dom.is_html_element_named(n, &local_name!("fieldset"))
                    })
                });
                if let Some(i) = cut {
                    path.truncate(i);
                    if activation_target.is_some_and(|t| !path.iter().any(|s| s.item == t)) {
                        activation_target = None;
                    }
                }
            }
            // 6.9-6.10.
            let clear = path
                .iter()
                .rev()
                .find(|s| s.shadow_adjusted_target.is_some())
                .copied();
            if let Some(c) = clear {
                // (the shadow-adjusted target of a struct that has one is its invocation target, or
                // for the first struct the target or its override, a document: whether the root of
                // either is a shadow root is the struct's `item_in_shadow_tree`)
                let in_shadow_root = |h: Option<Handle>| {
                    h.and_then(handle_node)
                        .is_some_and(|n| self.dom().is_in_shadow_tree(n))
                };
                clear_targets = c.item_in_shadow_tree || in_shadow_root(c.related_target);
            }
            // Invoke step 1 for every struct, once: the shadow-adjusted target of the nearest
            // struct at or before it that has one.
            let mut targets = std::mem::take(&mut scratch.targets);
            let mut nearest = None;
            targets.extend(path.iter().map(|s| {
                if s.shadow_adjusted_target.is_some() {
                    nearest = s.shadow_adjusted_target;
                }
                nearest
            }));
            // An item with no listeners of the event's type (or of its legacy prefixed name) when
            // the dispatch began has none to invoke until one is added: invoking it would only set
            // the event's target, relatedTarget, phase and currentTarget for no listener to see.
            // (Only the last invocation's target and relatedTarget outlast the dispatch; they are
            // set below.)
            let added = self.listeners_added.get();
            let legacy = legacy_type(&type_);
            let mut flags = std::mem::take(&mut scratch.flags);
            {
                let map = self.listeners.borrow();
                flags.extend(path.iter().map(|s| {
                    let listened = map.get(&s.item).is_some_and(|l| {
                        !l.of_type(&type_).is_empty()
                            || legacy.is_some_and(|t| !l.of_type(t).is_empty())
                    });
                    let at_target = s.shadow_adjusted_target.is_some();
                    u8::from(listened) | (u8::from(at_target) << 1)
                }));
            }
            let len = path.len();
            // (the path is the event's for the dispatch, as composedPath() reads it)
            self.ev_mut(event, |e| e.path = std::mem::take(&mut path));
            let may_listen = |i: usize| flags[i] & 1 != 0 || self.listeners_added.get() != added;
            let at_target = |i: usize| flags[i] & 2 != 0;
            let entry = |i: usize| self.ev(event, |e| e.path[i]);
            // 6.11.
            let pre_activation = activation_target.map(|t| self.legacy_pre_activation(cx, t));
            // 6.12: capturing.
            for i in (0..len).rev() {
                if !may_listen(i) {
                    continue;
                }
                let phase = if at_target(i) { AT_TARGET } else { CAPTURING };
                self.invoke(cx, &entry(i), targets[i], event, &type_, phase, CAPTURING);
            }
            // 6.13: bubbling.
            let mut last = None;
            for (i, &target) in targets.iter().enumerate() {
                let phase = if at_target(i) {
                    AT_TARGET
                } else if bubbles {
                    BUBBLING
                } else {
                    continue;
                };
                last = Some(i);
                if !may_listen(i) {
                    continue;
                }
                self.invoke(cx, &entry(i), target, event, &type_, phase, BUBBLING);
            }
            // (what the last invocation sets first, whether or not it was passed over)
            if let Some(i) = last {
                self.ev_mut(event, |e| {
                    let s = e.path[i];
                    set_invocation_targets(e, &s, targets[i])
                });
            }
            // 7-11.
            let mut path = self.ev_mut(event, |e| {
                e.phase = NONE;
                e.current_target = None;
                let path = std::mem::take(&mut e.path);
                e.dispatching = false;
                e.stop_propagation = false;
                e.stop_immediate = false;
                if clear_targets {
                    e.target = None;
                    e.related_target = None;
                    // (the relatedTarget a MouseEvent or FocusEvent shows)
                    if let EventExt::Ui(u) = &mut e.ext {
                        u.related_target = None;
                    }
                }
                path
            });
            path.clear();
            targets.clear();
            flags.clear();
            scratch.path = path;
            scratch.targets = targets;
            scratch.flags = flags;
            {
                let mut pool = self.dispatch_scratch.borrow_mut();
                if pool.len() < 4 {
                    pool.push(scratch);
                }
            }
            // 12: activation behavior.
            if let Some(t) = activation_target {
                let canceled = self.ev(event, |e| e.canceled);
                if !canceled {
                    self.activation_behavior(cx, t, event);
                } else if let Some(state) = pre_activation.flatten() {
                    self.legacy_canceled_activation(cx, t, state);
                }
            }
        } else {
            self.ev_mut(event, |e| {
                e.phase = NONE;
                e.current_target = None;
                e.dispatching = false;
                e.stop_propagation = false;
                e.stop_immediate = false;
            });
        }
        !self.ev(event, |e| e.canceled)
    }

    /// DOM §2.9 "invoke" for the path's struct `s`, for an event of type `type_` (its type when the
    /// dispatch began, which cannot change while it is dispatched), with the event's phase set to
    /// `event_phase` first and `target` step 1's (the shadow-adjusted target of the nearest struct at or
    /// before `s` that has one).
    #[allow(clippy::too_many_arguments)]
    fn invoke(
        &self,
        cx: &mut Cx,
        s: &PathEntry,
        target: Option<Handle>,
        event: Handle,
        type_: &str,
        event_phase: u16,
        phase: u16,
    ) {
        let current = s.item;
        let stop = self.ev_mut(event, |e| {
            e.phase = event_phase;
            set_invocation_targets(e, s, target);
            // 4 (after 3).
            if !e.stop_propagation {
                e.current_target = Some(current);
            }
            e.stop_propagation
        });
        // 3.
        if stop {
            return;
        }
        // 5: a clone of the listener list. Only the listeners of the event's type are kept: inner
        // invoke passes over the others, and a listener's type never changes.
        let listeners = self.listeners_of_type(current, type_);
        // 6.
        let found = !listeners.is_empty()
            && self.inner_invoke(cx, event, current, &listeners, phase, s.item_in_shadow_tree);
        // 7: legacy-pre-activation prefixed names for trusted events nobody listened to.
        if found {
            return;
        }
        let Some(legacy) = legacy_type(type_) else {
            return;
        };
        if !self.ev(event, |e| e.is_trusted) {
            return;
        }
        // (no listener ran since step 5: the clone's listeners of the legacy type are the list's)
        let listeners = self.listeners_of_type(current, legacy);
        if listeners.is_empty() {
            return;
        }
        let original = self.ev_mut(event, |e| {
            std::mem::replace(&mut e.type_, legacy.to_owned())
        });
        self.inner_invoke(cx, event, current, &listeners, phase, s.item_in_shadow_tree);
        self.ev_mut(event, |e| e.type_ = original);
    }

    /// The listeners of `target` whose type is `type_`, in order (a clone: what the callbacks add
    /// or remove does not change it; a removed listener is flagged).
    fn listeners_of_type(
        &self,
        target: Handle,
        type_: &str,
    ) -> smallvec::SmallVec<[Rc<Listener>; 4]> {
        match self.listeners.borrow().get(&target) {
            Some(list) => list.of_type(type_).iter().cloned().collect(),
            None => smallvec::SmallVec::new(),
        }
    }

    /// DOM §2.9 "inner invoke" step 8.2: the global of a listener's callback's realm. An event handler
    /// is compiled lazily, in the realm of its target.
    fn listener_global(&self, cx: &mut Cx, target: Handle, l: &Listener) -> Option<Handle> {
        if l.handler.is_none() {
            let scope = cx.scope();
            let cb = l.callback.borrow().as_ref().and_then(|c| c.local(scope))?;
            return rt::creation_realm(scope, cb);
        }
        match handle_node(target) {
            Some(n) => {
                let doc = {
                    let dom = self.dom();
                    if dom.is_document(n) {
                        n
                    } else {
                        dom.node_document(n)
                    }
                };
                self.window_of_document(doc)
            }
            None if self.is_window(target) => Some(target),
            None => rt::realm_global(cx.scope().get_current_context()),
        }
    }

    /// DOM "inner invoke": whether listeners of `target` may run, which is no when its global is a
    /// Window whose document is not fully active. A node's global follows its node document, so an
    /// adopted node goes with the document it is in now. Asked per listener: one can remove the frame
    /// or adopt the target.
    fn target_may_run(&self, cx: &mut Cx, target: Handle) -> bool {
        if let Some(n) = handle_node(target) {
            let doc = {
                let dom = self.dom();
                if dom.is_document(n) {
                    n
                } else {
                    dom.node_document(n)
                }
            };
            return !self.document_destroyed(doc);
        }
        if self.is_window(target) {
            return self.can_run_script(target);
        }
        rt::wrapper_realm(cx.scope(), target).is_none_or(|g| self.can_run_script(g))
    }

    /// DOM §2.9 "inner invoke", with `listeners` the listeners of the event's type.
    fn inner_invoke(
        &self,
        cx: &mut Cx,
        event: Handle,
        current: Handle,
        listeners: &[Rc<Listener>],
        phase: u16,
        in_shadow_tree: bool,
    ) -> bool {
        let mut found = false;
        for l in listeners {
            if l.removed.get() {
                continue;
            }
            found = true;
            if (phase == CAPTURING && !l.capture) || (phase == BUBBLING && l.capture) {
                continue;
            }
            if l.once {
                self.remove_listener_object(current, l);
            }
            if !self.target_may_run(cx, current) {
                continue;
            }
            let global = self.listener_global(cx, current, l);
            // Web IDL "call a user object's operation": HTML "check if we can run script".
            // A listener of a realm whose document is not fully active (its browsing context was
            // discarded, or a navigation replaced its Window) does not run.
            if !global.is_none_or(|g| self.can_run_script(g)) {
                continue;
            }
            // window.event (HTML §7.2.2.1).
            let previous = self.current_event.get();
            let previous_global = self.current_event_global.get();
            if !in_shadow_tree {
                self.current_event.set(Some(event));
                self.current_event_global.set(global);
            }
            if l.passive {
                self.ev_mut(event, |e| e.in_passive_listener = true);
            }
            // Web IDL "call a user object's operation": the listener runs as script, and "clean up
            // after running script" performs a microtask checkpoint when the stack empties (between
            // the listeners of an event the user agent dispatches).
            self.enter_script();
            match l.handler {
                Some(name) => self.run_event_handler(cx, current, name, event),
                None => {
                    let cb = l.callback.borrow().as_ref().and_then(|c| c.get(cx.scope()));
                    if let Some(cb) = cb {
                        let cb = EventListener(cb);
                        self.call_listener(cx, &cb, l.callback_context, current, event);
                    }
                }
            }
            self.leave_script(cx);
            self.ev_mut(event, |e| e.in_passive_listener = false);
            self.current_event.set(previous);
            self.current_event_global.set(previous_global);
            if self.ev(event, |e| e.stop_immediate) {
                break;
            }
        }
        found
    }

    /// Calls a listener's callback with `this` = the current target, in the callback's realm: what
    /// it throws is reported for that realm's global object (DOM §2.9 "inner invoke").
    fn call_listener(
        &self,
        cx: &mut Cx,
        cb: &EventListener,
        callback_context: Option<Handle>,
        current: Handle,
        event: Handle,
    ) {
        rt::in_callback_realm(cx, &cb.0, callback_context, |cx| {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let mut cx2 = Cx::new(tc);
            let r = cb.handle_event_in_realm(&mut cx2, Some(&current), event);
            if r.is_err() {
                self.listener_threw.set(true);
                if let Some(exception) = tc.exception() {
                    let exception = v8::Global::new(tc, exception);
                    let message = tc.message().map(|m| v8::Global::new(tc, m));
                    tc.reset();
                    let mut cx3 = Cx::new(tc);
                    self.report_exception(&mut cx3, exception, message);
                }
            }
        });
    }

    // ----- listeners ------------------------------------------------------------------------------------

    /// DOM §2.7.3 "default passive value".
    fn default_passive(&self, type_: &str, target: Handle) -> bool {
        if !matches!(type_, "touchstart" | "touchmove" | "wheel" | "mousewheel") {
            return false;
        }
        if self.context_of_window(target).is_some() {
            return true;
        }
        let Some(n) = handle_node(target) else {
            return false;
        };
        let dom = self.dom();
        let doc = dom.node_document(n);
        n == doc || dom.document_element(doc) == Some(n) || dom.body(doc) == Some(n)
    }

    fn remove_listener_object(&self, target: Handle, l: &Rc<Listener>) {
        l.removed.set(true);
        if let Some(list) = self.listeners.borrow_mut().get_mut(&target) {
            list.remove(l);
        }
    }

    /// DOM §2.7.3 "add an event listener".
    pub(crate) fn add_listener(
        &self,
        cx: &mut Cx,
        target: Handle,
        type_: &str,
        callback: Option<EventListener>,
        options: ListenerOptions,
    ) {
        let ListenerOptions {
            capture,
            passive,
            once,
            signal,
        } = options;
        if let Some(s) = signal {
            if self.signal_aborted(s) {
                return;
            }
        }
        let Some(callback) = callback else {
            return;
        };
        let passive = passive.unwrap_or_else(|| self.default_passive(type_, target));
        let exists = self.listeners.borrow().get(&target).is_some_and(|list| {
            list.of_type(type_).iter().any(|l| {
                l.handler.is_none()
                    && l.capture == capture
                    && l.callback
                        .borrow()
                        .as_ref()
                        .is_some_and(|c| c.is(cx.scope(), &callback.0))
            })
        });
        if exists {
            return;
        }
        let l = Rc::new(Listener {
            type_: type_.to_owned(),
            callback_context: rt::callback_context(cx.scope(), &callback.0),
            callback: RefCell::new(Some(Callback::object(callback.0))),
            capture,
            passive,
            once,
            removed: Cell::new(false),
            handler: None,
        });
        if let Some(s) = signal {
            if let Object::AbortSignal(sig) = &mut *self.obj_mut(s) {
                sig.remove_listeners.push((target, Rc::downgrade(&l)));
            }
        }
        self.listeners
            .borrow_mut()
            .entry(target)
            .or_default()
            .push(l);
        self.listeners_added.set(self.listeners_added.get() + 1);
        self.note_listener_type(type_);
        self.gc_trace_if_weak(cx, target);
    }

    /// Notes the types of listeners the page has that change how animations are serviced (Blink's
    /// `Document::AddListenerTypeIfNeeded`): it is never cleared.
    fn note_listener_type(&self, type_: &str) {
        if matches!(
            type_,
            "animationiteration" | "webkitAnimationIteration" | "webkitanimationiteration"
        ) {
            self.animations.borrow_mut().iteration_listeners = true;
        }
    }

    // ----- event handlers (HTML §8.1.8) -----------------------------------------------------------------

    /// The event type an event handler of `this` handles (an SVG animation element's `onend` handles
    /// `endEvent`).
    fn svg_handler_event(&self, this: Handle, event: &'static str) -> &'static str {
        handle_node(this)
            .and_then(|n| super::svg::animation_event_type(&self.dom(), n, event))
            .unwrap_or_else(|| super::html::handler_event_type(event))
    }

    /// HTML "determining the target of an event handler": `body` and `frameset` forward the
    /// Window-reflecting handlers to their node document's window, and have none (`None`) when
    /// that document is not an active document.
    pub(crate) fn handler_target(&self, this: Handle, event: &str) -> Option<Handle> {
        if let Some(n) = handle_node(this) {
            let dom = self.dom();
            let forwards = dom.is_html_element_named(n, &snowlark_dom::local_name!("body"))
                || dom.is_html_element_named(n, &snowlark_dom::local_name!("frameset"));
            if forwards && super::html::is_window_reflecting_handler(event) {
                let doc = dom.node_document(n);
                drop(dom);
                if !self.is_active_document(doc) {
                    return None;
                }
                return self.window_of_document(doc);
            }
        }
        Some(this)
    }

    /// Sets an event handler's value and registers its listener on first activation.
    pub(crate) fn set_handler(&self, target: Handle, event: &'static str, value: Option<Handler>) {
        match value {
            None => {
                self.handlers.borrow_mut().remove(&(target, event));
                // Deactivate: remove the listener.
                if let Some(list) = self.listeners.borrow_mut().get_mut(&target) {
                    list.retain(|l| {
                        let keep = l.handler != Some(event);
                        if !keep {
                            l.removed.set(true);
                        }
                        keep
                    });
                }
            }
            Some(h) => {
                self.note_listener_type(event);
                self.handlers.borrow_mut().insert((target, event), h);
                // (an active handler whose value became null, as a body that did not compile
                // leaves it, keeps its listener)
                let active = self.listeners.borrow().get(&target).is_some_and(|list| {
                    list.iter()
                        .any(|l| l.handler == Some(event) && !l.removed.get())
                });
                if !active {
                    let l = Rc::new(Listener {
                        type_: event.to_owned(),
                        callback: RefCell::new(None),
                        callback_context: None,
                        capture: false,
                        passive: false,
                        once: false,
                        removed: Cell::new(false),
                        handler: Some(event),
                    });
                    self.listeners
                        .borrow_mut()
                        .entry(target)
                        .or_default()
                        .push(l);
                    self.listeners_added.set(self.listeners_added.get() + 1);
                }
            }
        }
    }

    /// HTML §8.1.8.1 "get the current value of the event handler": compiles an uncompiled handler.
    fn handler_value(&self, cx: &mut Cx, target: Handle, event: &'static str) -> Option<JsObject> {
        let h = self.handlers.borrow().get(&(target, event)).cloned()?;
        match h {
            Handler::Compiled(f, _) => f.get(cx.scope()),
            Handler::Uncompiled {
                body,
                element,
                line,
            } => {
                // Step 1.1: null while scripting is disabled for the element's document (one
                // without a browsing context: a template's contents, `DOMParser`'s, `new
                // Document()`); the handler stays uncompiled.
                if let Some(el) = element {
                    self.context_of_document(self.dom().node_document(el))?;
                }
                let f = self.compile_event_handler(cx, target, event, &body, element, line);
                match f {
                    Some(f) => {
                        self.handlers.borrow_mut().insert(
                            (target, event),
                            Handler::Compiled(Callback::object(f.clone()), None),
                        );
                        self.gc_trace_if_weak(cx, target);
                        Some(f)
                    }
                    None => {
                        // A body that does not compile makes the value null; the handler stays
                        // active, so its listener keeps its place in the list.
                        self.handlers.borrow_mut().remove(&(target, event));
                        None
                    }
                }
            }
        }
    }

    /// HTML §8.1.8.1 "the event handler processing algorithm".
    fn run_event_handler(
        &self,
        cx: &mut Cx,
        target: Handle,
        event_name: &'static str,
        event: Handle,
    ) {
        let Some(callback) = self.handler_value(cx, target, event_name) else {
            return;
        };
        let callback_context = match self.handlers.borrow().get(&(target, event_name)) {
            Some(Handler::Compiled(_, context)) => *context,
            _ => None,
        };
        rt::in_callback_realm(cx, &callback, callback_context, |cx| {
            self.call_event_handler(cx, target, event_name, event, &callback)
        });
    }

    /// The event handler processing algorithm, in the callback's realm: the call and what its value
    /// does to the event.
    fn call_event_handler(
        &self,
        cx: &mut Cx,
        target: Handle,
        event_name: &'static str,
        event: Handle,
        callback: &JsObject,
    ) {
        let is_error_special = event_name == "error"
            && self.context_of_window(target).is_some()
            && self.ev(event, |e| e.interface == InterfaceId::ErrorEvent);
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let f = v8::Local::new(tc, callback.global());
        let Ok(f) = v8::Local::<v8::Function>::try_from(f) else {
            return;
        };
        let this = rt::wrap(tc, target).map(Into::into);
        let this: v8::Local<v8::Value> = this.unwrap_or_else(|| v8::undefined(tc).into());
        let args: Vec<v8::Local<v8::Value>> = if is_error_special {
            let (message, filename, lineno, colno, error) = self.ev(event, |e| match &e.ext {
                EventExt::Error {
                    message,
                    filename,
                    lineno,
                    colno,
                    error,
                } => (
                    message.clone(),
                    filename.clone(),
                    *lineno,
                    *colno,
                    error.as_ref().and_then(|v| v.get(tc)),
                ),
                _ => Default::default(),
            });
            vec![
                message.to_js(tc).unwrap(),
                filename.to_js(tc).unwrap(),
                lineno.to_js(tc).unwrap(),
                colno.to_js(tc).unwrap(),
                match error {
                    Some(e) => e.to_js(tc).unwrap(),
                    None => v8::undefined(tc).into(),
                },
            ]
        } else {
            match rt::wrap(tc, event) {
                Some(w) => vec![w.into()],
                None => return,
            }
        };
        let ret = f.call(tc, this, &args);
        match ret {
            None => {
                self.listener_threw.set(true);
                if let Some(exception) = tc.exception() {
                    let exception = v8::Global::new(tc, exception);
                    let message = tc.message().map(|m| v8::Global::new(tc, m));
                    tc.reset();
                    let mut cx2 = Cx::new(tc);
                    self.report_exception(&mut cx2, exception, message);
                }
            }
            Some(ret) => {
                // Step 5: the return value cancels the event.
                let is_before_unload =
                    self.ev(event, |e| e.interface == InterfaceId::BeforeUnloadEvent);
                if is_error_special {
                    if ret.is_true() {
                        self.set_canceled(event);
                    }
                } else if is_before_unload {
                    if !ret.is_null_or_undefined() {
                        self.set_canceled(event);
                    }
                } else if ret.is_false() {
                    self.set_canceled(event);
                }
            }
        }
    }

    // ----- reporting exceptions (HTML §8.1.4.7) ---------------------------------------------------------

    /// "Report an exception": an ErrorEvent at the window; printed when not canceled.
    pub(crate) fn report_exception(
        &self,
        cx: &mut Cx,
        exception: v8::Global<v8::Value>,
        message: Option<v8::Global<v8::Message>>,
    ) {
        self.report_exception_from(cx, exception, message, true);
    }

    /// [`Engine::report_exception`] for a module script's error or an import map's, which are
    /// never muted (a module script's "muted errors" is false). (Their JSON parse errors come from
    /// a script V8 makes for the JSON text, which is not "shared cross-origin" when no script
    /// was running.)
    pub(crate) fn report_module_exception(
        &self,
        cx: &mut Cx,
        exception: v8::Global<v8::Value>,
        message: Option<v8::Global<v8::Message>>,
    ) {
        self.report_exception_from(cx, exception, message, false);
    }

    fn report_exception_from(
        &self,
        cx: &mut Cx,
        exception: v8::Global<v8::Value>,
        message: Option<v8::Global<v8::Message>>,
        may_mute: bool,
    ) {
        let (text, filename, lineno, colno, muted, stack) = {
            let scope = cx.scope();
            let ex = v8::Local::new(scope, &exception);
            // (the console shows an error's call frames after its message, as V8 captured them
            // when it was made: its `stack` property, which the page may redefine, is not read)
            let stack = if ex.is_native_error() {
                super::window::captured_stack(scope, ex)
            } else {
                String::new()
            };
            let msg = match &message {
                Some(m) => Some(v8::Local::new(scope, m)),
                None => Some(v8::Exception::create_message(scope, ex)),
            };
            match msg {
                Some(m) => {
                    let mut text = m.get(scope).to_rust_string_lossy(scope);
                    // V8 words its message for its own error objects only; a DOMException is
                    // reported as the engines do, "Uncaught <name>: <message>" (HTML "report the
                    // exception" leaves the message's text to the implementation).
                    if let Some(h) = rt::unwrap_value(
                        ex,
                        snowlark_bindings::generated::ids::DOM_EXCEPTION,
                        snowlark_bindings::generated::ids::DOM_EXCEPTION_END,
                    ) {
                        if let Object::DomException { name, message } = &*self.obj(h) {
                            text = if message.is_empty() {
                                format!("Uncaught {name}")
                            } else {
                                format!("Uncaught {name}: {message}")
                            };
                        }
                    }
                    // (code `eval` or `Function` made has no name: undefined, not "undefined")
                    let filename = m
                        .get_script_resource_name(scope)
                        .filter(|n| n.is_string())
                        .map(|n| n.to_rust_string_lossy(scope))
                        .unwrap_or_default();
                    let line = m.get_line_number(scope).unwrap_or(0) as u32;
                    // (V8 has no column for some messages, a terminated script's among them: -1;
                    // the event's colno is then 0, unknown)
                    let col = u32::try_from(m.get_start_column()).map_or(0, |c| c + 1);
                    // The script the exception came from has muted errors: its origin is not
                    // "shared cross-origin" (every other script's is, and so is the script V8
                    // gives an exception thrown with no script running).
                    let muted = may_mute && !m.is_shared_cross_origin();
                    (text, filename, line, col, muted, stack)
                }
                None => (
                    ex.to_rust_string_lossy(scope),
                    String::new(),
                    0,
                    0,
                    false,
                    stack,
                ),
            }
        };
        let console_text = uncaught(&text) + &stack;
        // HTML "report an exception" step 5: a muted script's error is "Script error.", with no
        // URL, position or value (the console, which is not the page's, keeps all of it).
        let (event_text, event_filename, event_lineno, event_colno, error) = if muted {
            let scope = cx.scope();
            let null: v8::Local<v8::Value> = v8::null(scope).into();
            let null = JsValue::from_global(v8::Global::new(scope, null));
            ("Script error.".to_owned(), String::new(), 0, 0, Some(null))
        } else {
            (
                text.clone(),
                filename.clone(),
                lineno,
                colno,
                Some(JsValue::from_global(exception.clone())),
            )
        };
        // (a realm a navigation replaced fires nothing: it has no active document, and the error must
        // not reach any other window)
        let Some(window) = self
            .current_window(cx)
            .filter(|&w| self.context_of_window(w).is_some())
        else {
            self.console_error(&console_text);
            return;
        };
        // A nested report while the window's error event is dispatching prints instead (the "in error
        // reporting mode" flag).
        if self.in_error_reporting.get() {
            self.console_error(&console_text);
            return;
        }
        // A script without a name (code `eval` or `Function` made, an exception thrown with no
        // script running) is located at the document (HTML leaves the location to the
        // implementation).
        let event_filename = match self.associated_document(window) {
            Some(doc) if event_filename.is_empty() && !muted => {
                self.dom().document_url(doc).to_owned()
            }
            _ => event_filename,
        };
        self.in_error_reporting.set(true);
        let event = self.trusted_event(
            InterfaceId::ErrorEvent,
            "error",
            false,
            true,
            EventExt::Error {
                message: event_text.clone(),
                filename: event_filename.clone(),
                lineno: event_lineno,
                colno: event_colno,
                error: error.map(super::held::Held::value),
            },
        );
        let not_handled = self.dispatch(cx, event, window, false);
        self.in_error_reporting.set(false);
        // In a worker, an error its global did not handle goes on to the Worker object.
        if not_handled
            && self.worker_report_error(&event_text, &event_filename, event_lineno, event_colno)
        {
            return;
        }
        if not_handled {
            let source = (!filename.is_empty()).then(|| crate::devtools::SourceLocation {
                url: filename.clone(),
                line: lineno,
                column: colno,
            });
            if let Some(doc) = self.associated_document(window) {
                self.devtools_note_realm(doc);
            }
            self.console_print_at("error", &console_text, source);
        }
    }

    // ----- abort signals (DOM §3.2) ---------------------------------------------------------------------

    pub(crate) fn signal_aborted(&self, s: Handle) -> bool {
        match &*self.obj(s) {
            Object::AbortSignal(sig) => sig.aborted,
            _ => false,
        }
    }

    /// A signal's abort reason, once aborted.
    pub(crate) fn signal_reason(&self, cx: &mut Cx, s: Handle) -> Option<JsValue> {
        let scope = cx.scope();
        match &*self.obj(s) {
            Object::AbortSignal(sig) => sig.reason.as_ref().and_then(|r| r.get(scope)),
            _ => None,
        }
    }

    /// A new AbortSignal (an AbortController's, or a writable stream controller's).
    pub(crate) fn new_abort_signal(&self) -> Handle {
        self.new_signal()
    }

    fn new_signal(&self) -> Handle {
        self.alloc(Object::AbortSignal(AbortSignalData::default()))
    }

    /// "signal abort" (DOM §3.2).
    pub(crate) fn signal_abort(&self, cx: &mut Cx, signal: Handle, reason: Option<JsValue>) {
        if self.signal_aborted(signal) {
            return;
        }
        let reason = match reason {
            Some(r) if !r.is_undefined(cx) => r,
            _ => self.dom_exception_value(cx, "AbortError", "signal is aborted without reason"),
        };
        let dependents = {
            let mut o = self.obj_mut(signal);
            let Object::AbortSignal(sig) = &mut *o else {
                return;
            };
            sig.aborted = true;
            sig.reason = Some(super::held::Held::value(reason.clone()));
            std::mem::take(&mut sig.dependents)
        };
        // Dependent signals abort with the same reason first (they collect in "dependentSignalsToAbort").
        let mut to_abort = Vec::new();
        for d in dependents {
            if !self.signal_aborted(d) {
                if let Object::AbortSignal(sig) = &mut *self.obj_mut(d) {
                    sig.aborted = true;
                    sig.reason = Some(super::held::Held::value(reason.clone()));
                }
                to_abort.push(d);
            }
        }
        self.run_abort_steps(cx, signal);
        for d in to_abort {
            self.run_abort_steps(cx, d);
        }
    }

    /// "run the abort steps": the abort algorithms, then an `abort` event.
    fn run_abort_steps(&self, cx: &mut Cx, signal: Handle) {
        let (removals, fetches, scheduled, reason) = match &mut *self.obj_mut(signal) {
            Object::AbortSignal(sig) => (
                std::mem::take(&mut sig.remove_listeners),
                std::mem::take(&mut sig.fetches),
                std::mem::take(&mut sig.scheduled),
                sig.reason.clone(),
            ),
            _ => (Vec::new(), Vec::new(), Vec::new(), None),
        };
        let reason = reason.and_then(|r| r.get(cx.scope()));
        self.abort_scheduler_tasks(cx, scheduled, reason.clone());
        for id in fetches {
            self.abort_api_fetch(cx, id, reason.clone());
        }
        self.run_stream_abort_algorithms(cx, signal);
        for (target, l) in removals {
            if let Some(l) = l.upgrade() {
                self.remove_listener_object(target, &l);
            }
        }
        let e = self.trusted_event(InterfaceId::Event, "abort", false, false, EventExt::None);
        self.dispatch(cx, e, signal, false);
    }

    /// A DOMException object as a JS value.
    pub(crate) fn dom_exception_value(&self, cx: &mut Cx, name: &str, message: &str) -> JsValue {
        let h = self.alloc(Object::DomException {
            name: name.to_owned(),
            message: message.to_owned(),
        });
        let scope = cx.scope();
        match rt::wrap(scope, h) {
            Some(w) => JsValue::new(scope, w.into()),
            None => rt::undefined_value(scope),
        }
    }
}

/// The flattened options of `addEventListener`.
pub(crate) struct ListenerOptions {
    pub capture: bool,
    pub passive: Option<bool>,
    pub once: bool,
    pub signal: Option<Handle>,
}

// ----- EventTarget ---------------------------------------------------------------------------------------

impl EventTargetImpl for Engine {
    fn add_event_listener(
        &self,
        cx: &mut Cx,
        this: Handle,
        type_: &str,
        callback: Option<EventListener>,
        options: AddEventListenerOptionsOrBoolean,
    ) -> rt::Result<()> {
        let (capture, passive, once, signal) = match options {
            AddEventListenerOptionsOrBoolean::Boolean(b) => (b, None, false, None),
            AddEventListenerOptionsOrBoolean::AddEventListenerOptions(o) => {
                (o.capture, o.passive, o.once, o.signal)
            }
        };
        let options = ListenerOptions {
            capture,
            passive,
            once,
            signal,
        };
        self.add_listener(cx, this, type_, callback, options);
        Ok(())
    }

    fn remove_event_listener(
        &self,
        cx: &mut Cx,
        this: Handle,
        type_: &str,
        callback: Option<EventListener>,
        options: EventListenerOptionsOrBoolean,
    ) -> rt::Result<()> {
        let capture = match options {
            EventListenerOptionsOrBoolean::Boolean(b) => b,
            EventListenerOptionsOrBoolean::EventListenerOptions(o) => o.capture,
        };
        let Some(callback) = callback else {
            return Ok(());
        };
        // (found and removed in one pass: "remove an event listener" sets its removed flag first)
        if let Some(list) = self.listeners.borrow_mut().get_mut(&this) {
            list.remove_first(type_, |l| {
                l.handler.is_none()
                    && l.capture == capture
                    && l.callback
                        .borrow()
                        .as_ref()
                        .is_some_and(|c| c.is(cx.scope(), &callback.0))
            });
        }
        Ok(())
    }

    fn dispatch_event(&self, cx: &mut Cx, this: Handle, event: Handle) -> rt::Result<bool> {
        let (dispatching, initialized) = self.ev(event, |e| (e.dispatching, e.initialized));
        if dispatching || !initialized {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The event is already being dispatched or was not initialized",
            ));
        }
        self.ev_mut(event, |e| e.is_trusted = false);
        Ok(self.dispatch(cx, event, this, false))
    }

    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        Ok(self.alloc(Object::EventTarget))
    }
}

// ----- rt::EventHandlers --------------------------------------------------------------------------------

impl rt::EventHandlers for Engine {
    fn get_event_handler(
        &self,
        cx: &mut Cx,
        this: Handle,
        event: &'static str,
    ) -> rt::Result<Option<JsObject>> {
        let event = self.svg_handler_event(this, event);
        let Some(target) = self.handler_target(this, event) else {
            return Ok(None);
        };
        Ok(self.handler_value(cx, target, event))
    }

    fn set_event_handler(
        &self,
        cx: &mut Cx,
        this: Handle,
        event: &'static str,
        value: Option<JsObject>,
    ) -> rt::Result<()> {
        let event = self.svg_handler_event(this, event);
        let Some(target) = self.handler_target(this, event) else {
            return Ok(());
        };
        let value = value.map(|f| {
            let context = rt::callback_context(cx.scope(), &f);
            Handler::Compiled(Callback::object(f), context)
        });
        self.set_handler(target, event, value);
        self.gc_trace_if_weak(cx, target);
        // HTML §9.4.4: setting a MessagePort's onmessage enables its port message queue.
        if event == "message" {
            self.port_onmessage_set(target);
        }
        Ok(())
    }
}

// ----- Event --------------------------------------------------------------------------------------------

impl EventImpl for Engine {
    fn type_(&self, this: Handle) -> DomStr<'_> {
        DomStr::Guard(std::cell::Ref::map(self.objects.borrow(), |o| {
            match &o[this.get() as usize - 1] {
                Object::Event(e) => e.type_.as_str(),
                _ => "",
            }
        }))
    }
    fn target(&self, this: Handle) -> Option<Handle> {
        self.ev(this, |e| e.target)
    }
    fn src_element(&self, this: Handle) -> Option<Handle> {
        self.ev(this, |e| e.target)
    }
    fn current_target(&self, this: Handle) -> Option<Handle> {
        self.ev(this, |e| e.current_target)
    }
    fn event_phase(&self, this: Handle) -> u16 {
        self.ev(this, |e| e.phase)
    }
    fn cancel_bubble(&self, this: Handle) -> bool {
        self.ev(this, |e| e.stop_propagation)
    }
    fn set_cancel_bubble(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        if value {
            self.ev_mut(this, |e| e.stop_propagation = true);
        }
        Ok(())
    }
    fn bubbles(&self, this: Handle) -> bool {
        self.ev(this, |e| e.bubbles)
    }
    fn cancelable(&self, this: Handle) -> bool {
        self.ev(this, |e| e.cancelable)
    }
    fn return_value(&self, this: Handle) -> bool {
        !self.ev(this, |e| e.canceled)
    }
    fn set_return_value(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        if !value {
            self.set_canceled(this);
        }
        Ok(())
    }
    fn default_prevented(&self, this: Handle) -> bool {
        self.ev(this, |e| e.canceled)
    }
    fn composed(&self, this: Handle) -> bool {
        self.ev(this, |e| e.composed)
    }
    fn is_trusted(&self, this: Handle) -> bool {
        self.ev(this, |e| e.is_trusted)
    }
    fn time_stamp(&self, this: Handle) -> f64 {
        self.ev(this, |e| e.time_stamp)
    }

    /// DOM §2.2 `composedPath()`.
    fn composed_path(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        let (path, current) = self.ev(this, |e| (e.path.clone(), e.current_target));
        let Some(current) = current else {
            return Ok(Vec::new());
        };
        if path.is_empty() {
            return Ok(Vec::new());
        }
        // 5-6.
        let mut composed = vec![current];
        let mut current_index = 0;
        let mut current_hidden = 0i32;
        let mut index = path.len() as i32 - 1;
        while index >= 0 {
            let s = &path[index as usize];
            if s.root_of_closed_tree {
                current_hidden += 1;
            }
            if s.item == current {
                current_index = index as usize;
                break;
            }
            if s.slot_in_closed_tree {
                current_hidden -= 1;
            }
            index -= 1;
        }
        // 8-10: before the current target.
        let mut current_hidden_level = current_hidden;
        let mut max_hidden = current_hidden;
        let mut index = current_index as i32 - 1;
        while index >= 0 {
            let s = &path[index as usize];
            if s.root_of_closed_tree {
                current_hidden_level += 1;
            }
            if current_hidden_level <= max_hidden {
                composed.insert(0, s.item);
            }
            if s.slot_in_closed_tree {
                current_hidden_level -= 1;
                if current_hidden_level < max_hidden {
                    max_hidden = current_hidden_level;
                }
            }
            index -= 1;
        }
        // 11-13: after it.
        let mut current_hidden_level = current_hidden;
        let mut max_hidden = current_hidden;
        for s in &path[current_index + 1..] {
            if s.slot_in_closed_tree {
                current_hidden_level += 1;
            }
            if current_hidden_level <= max_hidden {
                composed.push(s.item);
            }
            if s.root_of_closed_tree {
                current_hidden_level -= 1;
                if current_hidden_level < max_hidden {
                    max_hidden = current_hidden_level;
                }
            }
        }
        Ok(composed)
    }

    fn stop_propagation(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.ev_mut(this, |e| e.stop_propagation = true);
        Ok(())
    }
    fn stop_immediate_propagation(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.ev_mut(this, |e| {
            e.stop_propagation = true;
            e.stop_immediate = true;
        });
        Ok(())
    }
    fn prevent_default(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.set_canceled(this);
        Ok(())
    }
    fn init_event(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_: &str,
        bubbles: bool,
        cancelable: bool,
    ) -> rt::Result<()> {
        if !self.ev(this, |e| e.dispatching) {
            Engine::init_event(self, this, type_, bubbles, cancelable);
        }
        Ok(())
    }
    fn constructor(&self, _cx: &mut Cx, type_: &str, init: g::EventInit) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::Event,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::None,
        ))
    }
}

impl CustomEventImpl for Engine {
    fn detail(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        let scope = cx.scope();
        let d = self.ev(this, |e| match &e.ext {
            EventExt::Custom(d) => d.as_ref().and_then(|v| v.get(scope)),
            _ => None,
        });
        Ok(d.unwrap_or_else(|| rt::null_value(cx.scope())))
    }
    fn init_custom_event(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_: &str,
        bubbles: bool,
        cancelable: bool,
        detail: JsValue,
    ) -> rt::Result<()> {
        if self.ev(this, |e| e.dispatching) {
            return Ok(());
        }
        Engine::init_event(self, this, type_, bubbles, cancelable);
        self.ev_mut(this, |e| {
            e.ext = EventExt::Custom(Some(super::held::Held::value(detail)))
        });
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::CustomEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::CustomEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Custom(init.detail.map(super::held::Held::value)),
        ))
    }
}

macro_rules! ui_init_common {
    ($init:expr) => {{
        let i = &$init;
        UiFields {
            view: i.view,
            detail: i.detail,
            which: i.which,
            ..Default::default()
        }
    }};
}

macro_rules! modifiers_init {
    ($u:expr, $i:expr) => {{
        let i = &$i;
        $u.ctrl = i.ctrl_key;
        $u.shift = i.shift_key;
        $u.alt = i.alt_key;
        $u.meta = i.meta_key;
        let mut m = Vec::new();
        if i.modifier_alt_graph {
            m.push("AltGraph");
        }
        if i.modifier_caps_lock {
            m.push("CapsLock");
        }
        if i.modifier_fn {
            m.push("Fn");
        }
        if i.modifier_fn_lock {
            m.push("FnLock");
        }
        if i.modifier_hyper {
            m.push("Hyper");
        }
        if i.modifier_num_lock {
            m.push("NumLock");
        }
        if i.modifier_scroll_lock {
            m.push("ScrollLock");
        }
        if i.modifier_super {
            m.push("Super");
        }
        if i.modifier_symbol {
            m.push("Symbol");
        }
        if i.modifier_symbol_lock {
            m.push("SymbolLock");
        }
        $u.modifiers = m;
    }};
}
pub(super) use modifiers_init;

macro_rules! mouse_init {
    ($u:expr, $i:expr) => {{
        let i = &$i;
        modifiers_init!($u, *i);
        $u.button = i.button;
        $u.buttons = i.buttons;
        $u.client_x = i.client_x;
        $u.client_y = i.client_y;
        $u.screen_x = i.screen_x;
        $u.screen_y = i.screen_y;
        $u.movement_x = i.movement_x;
        $u.movement_y = i.movement_y;
        $u.related_target = i.related_target;
    }};
}

impl Engine {
    pub(super) fn ui_event(
        &self,
        interface: InterfaceId,
        type_: &str,
        bubbles: bool,
        cancelable: bool,
        composed: bool,
        u: UiFields,
    ) -> Handle {
        let related = u.related_target;
        let h = self.new_event(
            interface,
            type_,
            bubbles,
            cancelable,
            composed,
            EventExt::Ui(Box::new(u)),
        );
        self.ev_mut(h, |e| e.related_target = related);
        h
    }

    pub(super) fn modifier_state(&self, this: Handle, key: &str) -> bool {
        self.ui(this, |u| match key {
            "Control" => u.ctrl,
            "Shift" => u.shift,
            "Alt" => u.alt,
            "Meta" => u.meta,
            k => u.modifiers.contains(&k),
        })
    }

    /// A trusted MouseEvent (or PointerEvent) the engine fires, e.g. for `click()`.
    pub(crate) fn trusted_mouse_event(
        &self,
        interface: InterfaceId,
        type_: &str,
        detail: i32,
        view: Option<Handle>,
    ) -> Handle {
        let u = UiFields {
            view,
            detail,
            pointer_type: if interface == InterfaceId::PointerEvent {
                "mouse".into()
            } else {
                String::new()
            },
            pointer_id: if interface == InterfaceId::PointerEvent {
                -1
            } else {
                0
            },
            ..Default::default()
        };
        let h = self.ui_event(interface, type_, true, true, true, u);
        self.ev_mut(h, |e| e.is_trusted = true);
        h
    }
}

impl UIEventImpl for Engine {
    fn view(&self, this: Handle) -> Option<Handle> {
        self.ui(this, |u| u.view)
    }
    fn detail(&self, this: Handle) -> i32 {
        self.ui(this, |u| u.detail)
    }
    fn which(&self, this: Handle) -> u32 {
        self.ui(this, |u| {
            if u.key_code != 0 || u.char_code != 0 {
                u.which.max(u.key_code.max(u.char_code))
            } else {
                u.which
            }
        })
    }
    fn source_capabilities(&self, _this: Handle) -> Option<Handle> {
        None
    }
    fn init_ui_event(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_arg: &str,
        bubbles_arg: bool,
        cancelable_arg: bool,
        view_arg: Option<Handle>,
        detail_arg: i32,
    ) -> rt::Result<()> {
        if self.ev(this, |e| e.dispatching) {
            return Ok(());
        }
        Engine::init_event(self, this, type_arg, bubbles_arg, cancelable_arg);
        self.ev_mut(this, |e| {
            if let EventExt::Ui(u) = &mut e.ext {
                u.view = view_arg;
                u.detail = detail_arg;
            }
        });
        Ok(())
    }
    fn constructor(&self, _cx: &mut Cx, type_: &str, init: g::UIEventInit) -> rt::Result<Handle> {
        let u = ui_init_common!(init);
        Ok(self.ui_event(
            InterfaceId::UIEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            u,
        ))
    }
}

impl FocusEventImpl for Engine {
    fn related_target(&self, this: Handle) -> Option<Handle> {
        self.ui(this, |u| u.related_target)
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::FocusEventInit,
    ) -> rt::Result<Handle> {
        let mut u = ui_init_common!(init);
        u.related_target = init.related_target;
        Ok(self.ui_event(
            InterfaceId::FocusEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            u,
        ))
    }
}

impl Engine {
    /// A mouse event coordinate as its attribute returns it: Pointer Events 3 keeps a pointer
    /// event's fractional coordinates, except for `click`, `auxclick` and `contextmenu`; other mouse
    /// events give whole CSS pixels, rounded down (the reference engine's choice where CSSOM View
    /// allows either).
    fn mouse_coordinate(&self, this: Handle, v: f64) -> f64 {
        let fractional = self.ev(this, |e| {
            e.interface == InterfaceId::PointerEvent
                && !["click", "auxclick", "contextmenu"]
                    .iter()
                    .any(|t| e.type_ == *t)
        });
        if fractional {
            v
        } else {
            v.floor()
        }
    }

    /// CSSOM View `pageX`/`pageY`: the client coordinates plus the scroll position of the event's
    /// window (the initial containing block's origin).
    fn mouse_page(&self, this: Handle) -> (f64, f64) {
        let (cx_, cy_) = self.ui(this, |u| (u.client_x, u.client_y));
        let (sx, sy) = self
            .ui(this, |u| u.view)
            .map_or((0.0, 0.0), |w| self.window_scroll(w));
        (
            self.mouse_coordinate(this, cx_ + sx),
            self.mouse_coordinate(this, cy_ + sy),
        )
    }

    /// CSSOM View `offsetX`/`offsetY`: with an element target, the position relative to the
    /// target's padding edge (the outermost `svg` element's for an element inside one), which the
    /// target keeps after the dispatch; otherwise `pageX`/`pageY`.
    fn mouse_offset(&self, this: Handle) -> (f64, f64) {
        let el = self
            .ev(this, |e| e.target)
            .and_then(super::handle_node)
            .filter(|&n| self.dom().is_element(n))
            .map(|n| self.outermost_svg_of(n).unwrap_or(n));
        let Some(el) = el else {
            return self.mouse_page(this);
        };
        let r = self.view_bounding_rect(el);
        let (top, left, _, _) = self.view_client(el);
        let (cx_, cy_) = self.ui(this, |u| (u.client_x, u.client_y));
        (
            self.mouse_coordinate(this, cx_ - r.x - left as f64),
            self.mouse_coordinate(this, cy_ - r.y - top as f64),
        )
    }
}

impl Engine {
    /// The outermost `svg` element an SVG element other than it is inside.
    fn outermost_svg_of(&self, n: NodeId) -> Option<NodeId> {
        let dom = self.dom();
        let is_svg = |e: NodeId| {
            dom.element(e)
                .is_some_and(|d| d.name().ns == snowlark_dom::ns!(svg))
        };
        if !is_svg(n) {
            return None;
        }
        let mut top = None;
        let mut cur = Some(n);
        while let Some(c) = cur.filter(|&c| is_svg(c)) {
            if dom
                .element(c)
                .is_some_and(|d| d.name().local == local_name!("svg"))
            {
                top = Some(c);
            }
            cur = dom.parent(c);
        }
        top.filter(|&t| t != n)
    }
}

impl MouseEventImpl for Engine {
    fn screen_x(&self, this: Handle) -> f64 {
        self.mouse_coordinate(this, self.ui(this, |u| u.screen_x))
    }
    fn screen_y(&self, this: Handle) -> f64 {
        self.mouse_coordinate(this, self.ui(this, |u| u.screen_y))
    }
    fn client_x(&self, this: Handle) -> f64 {
        self.mouse_coordinate(this, self.ui(this, |u| u.client_x))
    }
    fn client_y(&self, this: Handle) -> f64 {
        self.mouse_coordinate(this, self.ui(this, |u| u.client_y))
    }
    fn layer_x(&self, this: Handle) -> i32 {
        self.mouse_page(this).0.floor() as i32
    }
    fn layer_y(&self, this: Handle) -> i32 {
        self.mouse_page(this).1.floor() as i32
    }
    fn ctrl_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.ctrl)
    }
    fn shift_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.shift)
    }
    fn alt_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.alt)
    }
    fn meta_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.meta)
    }
    fn button(&self, this: Handle) -> i16 {
        self.ui(this, |u| u.button)
    }
    fn buttons(&self, this: Handle) -> u16 {
        self.ui(this, |u| u.buttons)
    }
    fn related_target(&self, this: Handle) -> Option<Handle> {
        self.ui(this, |u| u.related_target)
    }
    fn page_x(&self, this: Handle) -> f64 {
        self.mouse_page(this).0
    }
    fn page_y(&self, this: Handle) -> f64 {
        self.mouse_page(this).1
    }
    fn x(&self, this: Handle) -> f64 {
        MouseEventImpl::client_x(self, this)
    }
    fn y(&self, this: Handle) -> f64 {
        MouseEventImpl::client_y(self, this)
    }
    fn offset_x(&self, this: Handle) -> f64 {
        self.mouse_offset(this).0
    }
    fn offset_y(&self, this: Handle) -> f64 {
        self.mouse_offset(this).1
    }
    fn movement_x(&self, this: Handle) -> f64 {
        self.ui(this, |u| u.movement_x)
    }
    fn movement_y(&self, this: Handle) -> f64 {
        self.ui(this, |u| u.movement_y)
    }
    fn get_modifier_state(&self, _cx: &mut Cx, this: Handle, key_arg: &str) -> rt::Result<bool> {
        Ok(self.modifier_state(this, key_arg))
    }
    #[allow(clippy::too_many_arguments)]
    fn init_mouse_event(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_arg: &str,
        bubbles_arg: bool,
        cancelable_arg: bool,
        view_arg: Option<Handle>,
        detail_arg: i32,
        screen_x_arg: i32,
        screen_y_arg: i32,
        client_x_arg: i32,
        client_y_arg: i32,
        ctrl_key_arg: bool,
        alt_key_arg: bool,
        shift_key_arg: bool,
        meta_key_arg: bool,
        button_arg: i16,
        related_target_arg: Option<Handle>,
    ) -> rt::Result<()> {
        if self.ev(this, |e| e.dispatching) {
            return Ok(());
        }
        Engine::init_event(self, this, type_arg, bubbles_arg, cancelable_arg);
        self.ev_mut(this, |e| {
            e.related_target = related_target_arg;
            if let EventExt::Ui(u) = &mut e.ext {
                u.view = view_arg;
                u.detail = detail_arg;
                u.screen_x = screen_x_arg as f64;
                u.screen_y = screen_y_arg as f64;
                u.client_x = client_x_arg as f64;
                u.client_y = client_y_arg as f64;
                u.ctrl = ctrl_key_arg;
                u.alt = alt_key_arg;
                u.shift = shift_key_arg;
                u.meta = meta_key_arg;
                u.button = button_arg;
                u.related_target = related_target_arg;
            }
        });
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::MouseEventInit,
    ) -> rt::Result<Handle> {
        let mut u = ui_init_common!(init);
        mouse_init!(u, init);
        Ok(self.ui_event(
            InterfaceId::MouseEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            u,
        ))
    }
}

impl WheelEventImpl for Engine {
    fn delta_x(&self, this: Handle) -> f64 {
        self.ui(this, |u| u.delta_x)
    }
    fn delta_y(&self, this: Handle) -> f64 {
        self.ui(this, |u| u.delta_y)
    }
    fn delta_z(&self, this: Handle) -> f64 {
        self.ui(this, |u| u.delta_z)
    }
    fn delta_mode(&self, this: Handle) -> u32 {
        self.ui(this, |u| u.delta_mode)
    }
    fn momentum(&self, this: Handle) -> bool {
        self.ui(this, |u| u.momentum)
    }
    /// Chromium's legacy `wheelDeltaX` (data/legacy.idl).
    fn wheel_delta_x(&self, this: Handle) -> i32 {
        self.ui(this, |u| u.wheel_delta.0)
    }
    fn wheel_delta_y(&self, this: Handle) -> i32 {
        self.ui(this, |u| u.wheel_delta.1)
    }
    /// Chromium `WheelEvent::wheelDelta()`: `wheelDeltaY`, or `wheelDeltaX` when that is 0.
    fn wheel_delta(&self, this: Handle) -> i32 {
        self.ui(this, |u| {
            if u.wheel_delta.1 != 0 {
                u.wheel_delta.1
            } else {
                u.wheel_delta.0
            }
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::WheelEventInit,
    ) -> rt::Result<Handle> {
        let mut u = ui_init_common!(init);
        mouse_init!(u, init);
        u.delta_x = init.delta_x;
        u.delta_y = init.delta_y;
        u.delta_z = init.delta_z;
        u.delta_mode = init.delta_mode;
        u.momentum = init.momentum;
        // Chromium's WheelEvent(type, init): the given wheel deltas, else the negated deltas
        // (truncated to integers).
        u.wheel_delta = (
            if init.wheel_delta_x != 0 {
                init.wheel_delta_x
            } else {
                -init.delta_x as i32
            },
            if init.wheel_delta_y != 0 {
                init.wheel_delta_y
            } else {
                -init.delta_y as i32
            },
        );
        Ok(self.ui_event(
            InterfaceId::WheelEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            u,
        ))
    }
}

impl PointerEventImpl for Engine {
    fn pointer_id(&self, this: Handle) -> i32 {
        self.ui(this, |u| u.pointer_id)
    }
    fn width(&self, this: Handle) -> f64 {
        self.ui(this, |u| u.width)
    }
    fn height(&self, this: Handle) -> f64 {
        self.ui(this, |u| u.height)
    }
    fn pressure(&self, this: Handle) -> f32 {
        self.ui(this, |u| u.pressure)
    }
    fn tangential_pressure(&self, this: Handle) -> f32 {
        self.ui(this, |u| u.tangential_pressure)
    }
    fn tilt_x(&self, this: Handle) -> i32 {
        self.ui(this, |u| u.tilt_x)
    }
    fn tilt_y(&self, this: Handle) -> i32 {
        self.ui(this, |u| u.tilt_y)
    }
    fn twist(&self, this: Handle) -> i32 {
        self.ui(this, |u| u.twist)
    }
    fn altitude_angle(&self, this: Handle) -> f64 {
        self.ui(this, |u| u.altitude_angle)
    }
    fn azimuth_angle(&self, this: Handle) -> f64 {
        self.ui(this, |u| u.azimuth_angle)
    }
    fn pointer_type(&self, this: Handle) -> DomStr<'_> {
        self.ui_str(this, |u| u.pointer_type.as_str())
    }
    fn is_primary(&self, this: Handle) -> bool {
        self.ui(this, |u| u.is_primary)
    }
    fn persistent_device_id(&self, this: Handle) -> i32 {
        self.ui(this, |u| u.persistent_device_id)
    }
    /// Pointer Events 3 `getCoalescedEvents()`. The events of a trusted event take its target
    /// once it has one (after a retargeting out of a shadow tree, the retargeted one); a
    /// redispatch, which makes the event untrusted, leaves them as they were.
    fn get_coalesced_events(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        let list = self.ui(this, |u| u.coalesced.clone());
        let (trusted, target) = self.ev(this, |e| (e.is_trusted, e.target));
        if let (true, Some(t)) = (trusted, target) {
            for &c in &list {
                self.ev_mut(c, |c| c.target = Some(t));
            }
        }
        Ok(list)
    }
    fn get_predicted_events(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        Ok(self.ui(this, |u| u.predicted.clone()))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::PointerEventInit,
    ) -> rt::Result<Handle> {
        let mut u = ui_init_common!(init);
        mouse_init!(u, init);
        u.pointer_id = init.pointer_id;
        u.width = init.width;
        u.height = init.height;
        u.pressure = init.pressure;
        u.tangential_pressure = init.tangential_pressure;
        u.tilt_x = init.tilt_x.unwrap_or(0);
        u.tilt_y = init.tilt_y.unwrap_or(0);
        u.twist = init.twist;
        u.altitude_angle = init.altitude_angle.unwrap_or(std::f64::consts::FRAC_PI_2);
        u.azimuth_angle = init.azimuth_angle.unwrap_or(0.0);
        // Pointer Events 3: the angles from the tilts when only the tilts are given, and the other
        // way round.
        let tilts = init.tilt_x.is_some() || init.tilt_y.is_some();
        let angles = init.altitude_angle.is_some() || init.azimuth_angle.is_some();
        if tilts && !angles {
            let (tx, ty) = (tilt_in_range(u.tilt_x), tilt_in_range(u.tilt_y));
            u.azimuth_angle = azimuth_from_tilt(tx as f64, ty as f64);
            u.altitude_angle = altitude_from_tilt(tx as f64, ty as f64);
        } else if angles && !tilts {
            let az = azimuth_in_range(u.azimuth_angle);
            let alt = altitude_in_range(u.altitude_angle);
            u.tilt_x = tilt_x_from_spherical(az, alt);
            u.tilt_y = tilt_y_from_spherical(az, alt);
        }
        u.pointer_type = init.pointer_type;
        u.is_primary = init.is_primary;
        u.persistent_device_id = init.persistent_device_id;
        // (the init dictionary's lists, as given)
        u.coalesced = init.coalesced_events;
        u.predicted = init.predicted_events;
        Ok(self.ui_event(
            InterfaceId::PointerEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            u,
        ))
    }
}

impl KeyboardEventImpl for Engine {
    fn key(&self, this: Handle) -> DomStr<'_> {
        self.ui_str(this, |u| u.key.as_str())
    }
    fn code(&self, this: Handle) -> DomStr<'_> {
        self.ui_str(this, |u| u.code.as_str())
    }
    fn location(&self, this: Handle) -> u32 {
        self.ui(this, |u| u.location)
    }
    fn ctrl_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.ctrl)
    }
    fn shift_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.shift)
    }
    fn alt_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.alt)
    }
    fn meta_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.meta)
    }
    fn repeat(&self, this: Handle) -> bool {
        self.ui(this, |u| u.repeat)
    }
    fn is_composing(&self, this: Handle) -> bool {
        self.ui(this, |u| u.is_composing)
    }
    fn char_code(&self, this: Handle) -> u32 {
        self.ui(this, |u| u.char_code)
    }
    fn key_code(&self, this: Handle) -> u32 {
        self.ui(this, |u| u.key_code)
    }
    fn get_modifier_state(&self, _cx: &mut Cx, this: Handle, key_arg: &str) -> rt::Result<bool> {
        Ok(self.modifier_state(this, key_arg))
    }
    #[allow(clippy::too_many_arguments)]
    fn init_keyboard_event(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_arg: &str,
        bubbles_arg: bool,
        cancelable_arg: bool,
        view_arg: Option<Handle>,
        key_arg: &str,
        location_arg: u32,
        ctrl_key: bool,
        alt_key: bool,
        shift_key: bool,
        meta_key: bool,
    ) -> rt::Result<()> {
        if self.ev(this, |e| e.dispatching) {
            return Ok(());
        }
        Engine::init_event(self, this, type_arg, bubbles_arg, cancelable_arg);
        self.ev_mut(this, |e| {
            if let EventExt::Ui(u) = &mut e.ext {
                u.view = view_arg;
                u.key = key_arg.to_owned();
                u.location = location_arg;
                u.ctrl = ctrl_key;
                u.alt = alt_key;
                u.shift = shift_key;
                u.meta = meta_key;
            }
        });
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::KeyboardEventInit,
    ) -> rt::Result<Handle> {
        let mut u = ui_init_common!(init);
        modifiers_init!(u, init);
        u.key = init.key;
        u.code = init.code;
        u.location = init.location;
        u.repeat = init.repeat;
        u.is_composing = init.is_composing;
        u.char_code = init.char_code;
        u.key_code = init.key_code;
        Ok(self.ui_event(
            InterfaceId::KeyboardEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            u,
        ))
    }
}

impl snowlark_bindings::generated::MessageEventImpl for Engine {
    fn data(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        let scope = cx.scope();
        let d = self.ev(this, |e| match &e.ext {
            EventExt::Message { data, .. } => data.as_ref().and_then(|v| v.get(scope)),
            _ => None,
        });
        Ok(d.unwrap_or_else(|| rt::null_value(cx.scope())))
    }
    fn origin(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::Message { origin, .. } => origin.clone(),
            _ => String::new(),
        }))
    }
    fn last_event_id(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::Message { last_event_id, .. } => last_event_id.clone(),
            _ => String::new(),
        }))
    }
    fn source(&self, this: Handle) -> Option<g::WindowOrMessagePortOrServiceWorker> {
        let source = self.ev(this, |e| match &e.ext {
            EventExt::Message { source, .. } => *source,
            _ => None,
        })?;
        Some(self.message_event_source(source))
    }
    fn ports(&self, this: Handle) -> Vec<Handle> {
        self.ev(this, |e| match &e.ext {
            EventExt::Message { ports, .. } => ports.clone(),
            _ => Vec::new(),
        })
    }
    /// HTML §9.3.1 `initMessageEvent()`: initializes the event like `initEvent()`, and its message
    /// attributes.
    #[allow(clippy::too_many_arguments)]
    fn init_message_event(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_: &str,
        bubbles: bool,
        cancelable: bool,
        data: JsValue,
        origin: &str,
        last_event_id: &str,
        source: Option<g::WindowOrMessagePortOrServiceWorker>,
        ports: Vec<Handle>,
    ) -> rt::Result<()> {
        if self.ev(this, |e| e.dispatching) {
            return Ok(());
        }
        Engine::init_event(self, this, type_, bubbles, cancelable);
        self.ev_mut(this, |e| {
            e.ext = EventExt::Message {
                data: Some(super::held::Held::value(data)),
                origin: origin.to_owned(),
                last_event_id: last_event_id.to_owned(),
                source: source.map(source_handle),
                ports,
            }
        });
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::MessageEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::MessageEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Message {
                data: init.data.map(super::held::Held::value),
                origin: init.origin,
                last_event_id: init.last_event_id,
                source: init.source.map(source_handle),
                ports: init.ports,
            },
        ))
    }
}

/// The handle behind a `MessageEventSource`.
fn source_handle(s: g::WindowOrMessagePortOrServiceWorker) -> Handle {
    match s {
        g::WindowOrMessagePortOrServiceWorker::Window(h)
        | g::WindowOrMessagePortOrServiceWorker::MessagePort(h)
        | g::WindowOrMessagePortOrServiceWorker::ServiceWorker(h) => h,
    }
}

impl Engine {
    /// A message event's source as the union the bindings return.
    fn message_event_source(&self, h: Handle) -> g::WindowOrMessagePortOrServiceWorker {
        if super::handle_node(h).is_none() && matches!(&*self.obj(h), Object::ServiceWorker(_)) {
            g::WindowOrMessagePortOrServiceWorker::ServiceWorker(h)
        } else if super::handle_node(h).is_none() && matches!(&*self.obj(h), Object::MessagePort(_))
        {
            g::WindowOrMessagePortOrServiceWorker::MessagePort(h)
        } else {
            g::WindowOrMessagePortOrServiceWorker::Window(h)
        }
    }

    /// HTML §8.4.1 "erase all event listeners and handlers" of a target.
    pub(crate) fn erase_listeners(&self, target: Handle) {
        if let Some(list) = self.listeners.borrow_mut().remove(&target) {
            for l in &list {
                l.removed.set(true);
            }
        }
        self.handlers.borrow_mut().retain(|(t, _), _| *t != target);
    }
}

impl InputEventImpl for Engine {
    /// Input Events: the data transfer of an insertion from a paste or drop.
    fn data_transfer(&self, this: Handle) -> Option<Handle> {
        self.ui(this, |u| u.data_transfer)
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::InputEventInit,
    ) -> rt::Result<Handle> {
        let mut u = ui_init_common!(init);
        u.data = init.data;
        u.is_composing = init.is_composing;
        u.input_type = init.input_type;
        u.data_transfer = init.data_transfer;
        u.target_ranges = init.target_ranges;
        Ok(self.ui_event(
            InterfaceId::InputEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            u,
        ))
    }
    fn data(&self, this: Handle) -> Option<DomStr<'_>> {
        self.ui(this, |u| u.data.clone()).map(DomStr::Owned)
    }
    fn is_composing(&self, this: Handle) -> bool {
        self.ui(this, |u| u.is_composing)
    }
    fn input_type(&self, this: Handle) -> DomStr<'_> {
        self.ui_str(this, |u| u.input_type.as_str())
    }
    /// Input Events: the StaticRanges the edit will change (the same ones for the event's life).
    fn get_target_ranges(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        Ok(self.ui(this, |u| u.target_ranges.clone()))
    }
}

impl CompositionEventImpl for Engine {
    fn data(&self, this: Handle) -> DomStr<'_> {
        self.ui_str(this, |u| u.data.as_deref().unwrap_or(""))
    }
    #[allow(clippy::too_many_arguments)]
    fn init_composition_event(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_arg: &str,
        bubbles_arg: bool,
        cancelable_arg: bool,
        view_arg: Option<Handle>,
        data_arg: &str,
    ) -> rt::Result<()> {
        if self.ev(this, |e| e.dispatching) {
            return Ok(());
        }
        Engine::init_event(self, this, type_arg, bubbles_arg, cancelable_arg);
        self.ev_mut(this, |e| {
            if let EventExt::Ui(u) = &mut e.ext {
                u.view = view_arg;
                u.data = Some(data_arg.to_owned());
            }
        });
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::CompositionEventInit,
    ) -> rt::Result<Handle> {
        let mut u = ui_init_common!(init);
        u.data = Some(init.data);
        Ok(self.ui_event(
            InterfaceId::CompositionEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            u,
        ))
    }
}

impl TextEventImpl for Engine {
    fn data(&self, this: Handle) -> DomStr<'_> {
        self.ui_str(this, |u| u.data.as_deref().unwrap_or(""))
    }
    #[allow(clippy::too_many_arguments)]
    fn init_text_event(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_: &str,
        bubbles: bool,
        cancelable: bool,
        view: Option<Handle>,
        data: &str,
    ) -> rt::Result<()> {
        if self.ev(this, |e| e.dispatching) {
            return Ok(());
        }
        Engine::init_event(self, this, type_, bubbles, cancelable);
        self.ev_mut(this, |e| {
            if let EventExt::Ui(u) = &mut e.ext {
                u.view = view;
                u.data = Some(data.to_owned());
            }
        });
        Ok(())
    }
}

impl InputDeviceCapabilitiesImpl for Engine {
    fn fires_touch_events(&self, this: Handle) -> bool {
        matches!(&*self.obj(this), Object::InputDeviceCapabilities(true))
    }
    fn pointer_movement_scrolls(&self, _this: Handle) -> bool {
        false
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        init: g::InputDeviceCapabilitiesInit,
    ) -> rt::Result<Handle> {
        Ok(self.alloc(Object::InputDeviceCapabilities(init.fires_touch_events)))
    }
}

impl ErrorEventImpl for Engine {
    fn message(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::Error { message, .. } => message.clone(),
            _ => String::new(),
        }))
    }
    fn filename(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::Error { filename, .. } => filename.clone(),
            _ => String::new(),
        }))
    }
    fn lineno(&self, this: Handle) -> u32 {
        self.ev(this, |e| match &e.ext {
            EventExt::Error { lineno, .. } => *lineno,
            _ => 0,
        })
    }
    fn colno(&self, this: Handle) -> u32 {
        self.ev(this, |e| match &e.ext {
            EventExt::Error { colno, .. } => *colno,
            _ => 0,
        })
    }
    fn error(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        let scope = cx.scope();
        let e = self.ev(this, |e| match &e.ext {
            EventExt::Error { error, .. } => error.as_ref().and_then(|v| v.get(scope)),
            _ => None,
        });
        Ok(e.unwrap_or_else(|| rt::undefined_value(cx.scope())))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::ErrorEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::ErrorEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Error {
                message: init.message,
                filename: init.filename,
                lineno: init.lineno,
                colno: init.colno,
                error: init.error.map(super::held::Held::value),
            },
        ))
    }
}

impl PromiseRejectionEventImpl for Engine {
    fn promise(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsObject> {
        let scope = cx.scope();
        self.ev(this, |e| match &e.ext {
            EventExt::Rejection { promise, .. } => promise
                .get(scope)
                .ok_or_else(|| rt::Error::Type("no promise".into())),
            _ => Err(rt::Error::Type("no promise".into())),
        })
    }
    fn reason(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        let scope = cx.scope();
        let r = self.ev(this, |e| match &e.ext {
            EventExt::Rejection { reason, .. } => reason.as_ref().and_then(|v| v.get(scope)),
            _ => None,
        });
        Ok(r.unwrap_or_else(|| rt::undefined_value(cx.scope())))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::PromiseRejectionEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::PromiseRejectionEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Rejection {
                promise: super::held::Held::object(init.promise),
                reason: init.reason.map(super::held::Held::value),
            },
        ))
    }
}

impl HashChangeEventImpl for Engine {
    fn old_url(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::HashChange { old_url, .. } => old_url.clone(),
            _ => String::new(),
        }))
    }
    fn new_url(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::HashChange { new_url, .. } => new_url.clone(),
            _ => String::new(),
        }))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::HashChangeEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::HashChangeEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::HashChange {
                old_url: init.old_url,
                new_url: init.new_url,
            },
        ))
    }
}

impl PageTransitionEventImpl for Engine {
    fn persisted(&self, this: Handle) -> bool {
        self.ev(this, |e| matches!(e.ext, EventExt::PageTransition(true)))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::PageTransitionEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::PageTransitionEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::PageTransition(init.persisted),
        ))
    }
}

impl PopStateEventImpl for Engine {
    fn state(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        let scope = cx.scope();
        let s = self.ev(this, |e| match &e.ext {
            EventExt::PopState(s) => s.as_ref().and_then(|v| v.get(scope)),
            _ => None,
        });
        Ok(s.unwrap_or_else(|| rt::null_value(cx.scope())))
    }
    fn has_ua_visual_transition(&self, _this: Handle) -> bool {
        false
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::PopStateEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::PopStateEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::PopState(init.state.map(super::held::Held::value)),
        ))
    }
}

impl BeforeUnloadEventImpl for Engine {
    fn return_value(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::BeforeUnload(s) => s.clone(),
            _ => String::new(),
        }))
    }
    fn set_return_value(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.ev_mut(this, |e| e.ext = EventExt::BeforeUnload(value.to_owned()));
        Ok(())
    }
}

impl ProgressEventImpl for Engine {
    fn length_computable(&self, this: Handle) -> bool {
        self.ev(this, |e| match &e.ext {
            EventExt::Progress {
                length_computable, ..
            } => *length_computable,
            _ => false,
        })
    }
    fn loaded(&self, this: Handle) -> f64 {
        self.ev(this, |e| match &e.ext {
            EventExt::Progress { loaded, .. } => *loaded,
            _ => 0.0,
        })
    }
    fn total(&self, this: Handle) -> f64 {
        self.ev(this, |e| match &e.ext {
            EventExt::Progress { total, .. } => *total,
            _ => 0.0,
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::ProgressEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::ProgressEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Progress {
                length_computable: init.length_computable,
                loaded: init.loaded,
                total: init.total,
            },
        ))
    }
}

impl ClipboardEventImpl for Engine {
    fn clipboard_data(&self, this: Handle) -> Option<Handle> {
        self.ev(this, |e| match &e.ext {
            EventExt::Clipboard(d) => *d,
            _ => None,
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::ClipboardEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::ClipboardEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Clipboard(init.clipboard_data),
        ))
    }
}

/// HTML §6.11.4 `DragEvent`: a MouseEvent with the DataTransfer it was initialized with.
impl DragEventImpl for Engine {
    fn data_transfer(&self, this: Handle) -> Option<Handle> {
        self.ui(this, |u| u.data_transfer)
    }
    fn constructor(&self, _cx: &mut Cx, type_: &str, init: g::DragEventInit) -> rt::Result<Handle> {
        let mut u = ui_init_common!(init);
        mouse_init!(u, init);
        u.data_transfer = init.data_transfer;
        Ok(self.ui_event(
            InterfaceId::DragEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            u,
        ))
    }
}

impl SubmitEventImpl for Engine {
    fn submitter(&self, this: Handle) -> Option<Handle> {
        self.ev(this, |e| match &e.ext {
            EventExt::Submit(s) => *s,
            _ => None,
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::SubmitEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::SubmitEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Submit(init.submitter),
        ))
    }
}

impl g::CommandEventImpl for Engine {
    fn source(&self, this: Handle) -> Option<Handle> {
        self.ev(this, |e| match &e.ext {
            EventExt::Command { source, .. } => *source,
            _ => None,
        })
    }
    fn command(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::Command { command, .. } => command.clone(),
            _ => String::new(),
        }))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::CommandEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::CommandEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Command {
                command: init.command,
                source: init.source,
            },
        ))
    }
}

impl g::FormDataEventImpl for Engine {
    fn form_data(&self, this: Handle) -> Handle {
        self.ev(this, |e| match &e.ext {
            EventExt::FormData(f) => *f,
            _ => unreachable!("a FormDataEvent carries its FormData"),
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::FormDataEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::FormDataEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::FormData(init.form_data),
        ))
    }
}

impl ToggleEventImpl for Engine {
    fn old_state(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::Toggle { old_state, .. } => old_state.clone(),
            _ => String::new(),
        }))
    }
    fn new_state(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::Toggle { new_state, .. } => new_state.clone(),
            _ => String::new(),
        }))
    }
    fn source(&self, this: Handle) -> Option<Handle> {
        self.ev(this, |e| match &e.ext {
            EventExt::Toggle { source, .. } => *source,
            _ => None,
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::ToggleEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::ToggleEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Toggle {
                old_state: init.old_state,
                new_state: init.new_state,
                source: init.source,
            },
        ))
    }
}

// ----- AbortController and AbortSignal ------------------------------------------------------------------

impl AbortControllerImpl for Engine {
    fn signal(&self, this: Handle) -> Handle {
        match &*self.obj(this) {
            Object::AbortController { signal } => *signal,
            _ => panic!("not an AbortController"),
        }
    }
    fn abort(&self, cx: &mut Cx, this: Handle, reason: Option<JsValue>) -> rt::Result<()> {
        let signal = AbortControllerImpl::signal(self, this);
        self.signal_abort(cx, signal, reason);
        Ok(())
    }
    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        let signal = self.new_signal();
        Ok(self.alloc(Object::AbortController { signal }))
    }
}

impl AbortSignalImpl for Engine {
    fn aborted(&self, this: Handle) -> bool {
        self.signal_aborted(this)
    }
    fn reason(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        let scope = cx.scope();
        let r = match &*self.obj(this) {
            Object::AbortSignal(s) => s.reason.as_ref().and_then(|r| r.get(scope)),
            _ => None,
        };
        Ok(r.unwrap_or_else(|| rt::undefined_value(cx.scope())))
    }
    fn static_abort(&self, cx: &mut Cx, reason: Option<JsValue>) -> rt::Result<Handle> {
        let s = self.new_signal();
        let reason = match reason {
            Some(r) if !r.is_undefined(cx) => r,
            _ => self.dom_exception_value(cx, "AbortError", "signal is aborted without reason"),
        };
        if let Object::AbortSignal(sig) = &mut *self.obj_mut(s) {
            sig.aborted = true;
            sig.reason = Some(super::held::Held::value(reason));
        }
        Ok(s)
    }
    fn static_timeout(&self, _cx: &mut Cx, milliseconds: u64) -> rt::Result<Handle> {
        let s = self.new_signal();
        self.queue_signal_timeout(s, milliseconds);
        Ok(s)
    }
    /// DOM §3.2 "create a dependent abort signal".
    fn static_any(&self, _cx: &mut Cx, signals: Vec<Handle>) -> rt::Result<Handle> {
        let result = self.new_signal();
        for &s in &signals {
            if self.signal_aborted(s) {
                let reason = match &*self.obj(s) {
                    Object::AbortSignal(sig) => sig.reason.clone(),
                    _ => None,
                };
                if let Object::AbortSignal(sig) = &mut *self.obj_mut(result) {
                    sig.aborted = true;
                    sig.reason = reason;
                }
                return Ok(result);
            }
        }
        let mut sources = Vec::new();
        for &s in &signals {
            let (dependent, its_sources) = match &*self.obj(s) {
                Object::AbortSignal(sig) => (sig.dependent, sig.sources.clone()),
                _ => (false, Vec::new()),
            };
            if !dependent {
                if !sources.contains(&s) {
                    sources.push(s);
                }
            } else {
                for src in its_sources {
                    if !self.signal_aborted(src) && !sources.contains(&src) {
                        sources.push(src);
                    }
                }
            }
        }
        for &src in &sources {
            if let Object::AbortSignal(sig) = &mut *self.obj_mut(src) {
                sig.dependents.push(result);
            }
        }
        if let Object::AbortSignal(sig) = &mut *self.obj_mut(result) {
            sig.dependent = true;
            sig.sources = sources;
        }
        Ok(result)
    }
    fn throw_if_aborted(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let scope = cx.scope();
        let r = match &*self.obj(this) {
            Object::AbortSignal(s) if s.aborted => s.reason.as_ref().and_then(|r| r.get(scope)),
            _ => return Ok(()),
        };
        if let Some(r) = r {
            let scope = cx.scope();
            let v = v8::Local::new(scope, r.global());
            scope.throw_exception(v);
        }
        Err(rt::Error::Pending)
    }
}
