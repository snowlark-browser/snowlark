// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §6.11.5 the drag-and-drop processing model. A press on a draggable element (an element
//! whose `draggable` is true: `draggable=true`, images, links) that moves past the drag threshold
//! starts a drag: `dragstart` at the source node with the drag data store in read/write mode, then
//! the pointer event stream is suppressed (`pointercancel`, `pointerout`, `pointerleave`) and each
//! pointer move is an iteration of the processing model: `drag` at the source, `dragenter` at a new
//! immediate user selection and `dragleave` at the element it replaces, `dragover` at the current
//! target, whose result sets the current drag operation. The release ends it: `drop` (or
//! `dragleave` when nothing accepted the drag), then `dragend` at the source. Escape cancels it.
//!
//! A drag can also come from outside the page (the window's drop target: files or text the user
//! drags from another program): the same iterations without a source node, driven by
//! [`Engine::external_drag_over`], [`Engine::external_drag_leave`] and
//! [`Engine::external_drag_drop`], which answer the current drag operation for the platform's
//! feedback.
//!
//! Each DND event gets a new DataTransfer associated with the drag data store for its dispatch
//! (§6.11.4 "fire a DND event"): the store's items are given to it in the mode of the event and,
//! after a `dragstart`, taken back; then the DataTransfer is disabled.
//!
//! Two places follow what the engines do where the processing model reads otherwise (the drag
//! and drop tests expect it): the current target element is the immediate user selection whether
//! or not its `dragenter` was canceled (the model falls back to the body, so an element entered
//! would never get its `dragleave`), and `dragenter` has the previous target as its related
//! target. A `drop` nothing canceled keeps the current drag operation for `dragend`.

use serde::{Deserialize, Serialize};
use snowlark_bindings::rt::Cx;
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, NodeId};
use snowlark_paint::Point;
use snowlark_style::ElementState;

use crate::engine::datatransfer::{DataTransferData, Item, ItemData, StoreMode};
use crate::engine::{node_handle, Engine, Object};
use crate::input::Modifiers;
use crate::Page;

/// How far the pointer moves from the press before a drag starts (CSS px, either axis).
const DRAG_THRESHOLD: f32 = 4.0;

/// A drag operation (§6.11.5 "current drag operation").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DragOperation {
    #[default]
    None,
    Copy,
    Link,
    Move,
}

impl DragOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            DragOperation::None => "none",
            DragOperation::Copy => "copy",
            DragOperation::Link => "link",
            DragOperation::Move => "move",
        }
    }

    /// The drag operation a canceled `dragover` (or `drop`) leaves, from the DataTransfer's
    /// `effectAllowed` and `dropEffect` after the dispatch (§6.11.5, the table).
    fn from_effects(allowed: &str, drop_effect: &str) -> DragOperation {
        let has = |list: &[&str]| list.contains(&allowed);
        match drop_effect {
            "copy" if has(&["uninitialized", "copy", "copyLink", "copyMove", "all"]) => {
                DragOperation::Copy
            }
            "link" if has(&["uninitialized", "link", "copyLink", "linkMove", "all"]) => {
                DragOperation::Link
            }
            "move" if has(&["uninitialized", "move", "copyMove", "linkMove", "all"]) => {
                DragOperation::Move
            }
            _ => DragOperation::None,
        }
    }
}

/// What is being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragKind {
    /// An element whose `draggable` is true (not a link).
    Element,
    /// An `a` element with an `href`.
    Link,
    /// Something from outside the page (another program: files, text).
    External,
}

/// A press that may become a drag once the pointer moves far enough.
#[derive(Clone, Copy, Debug)]
pub struct PendingDrag {
    pub source: NodeId,
    pub kind: DragKind,
    /// Where the press was, in the top-level viewport and in the source's document's viewport.
    pub at: Point,
    pub client: Point,
}

/// A drag-and-drop operation in progress (§6.11.5).
pub struct DragSession {
    /// The source node; None for a drag from outside the page.
    pub source: Option<NodeId>,
    pub kind: DragKind,
    /// The drag data store's item list and allowed effects state.
    pub items: Vec<Item>,
    pub allowed: String,
    /// The immediate user selection and the current target element.
    pub immediate: Option<NodeId>,
    pub target: Option<NodeId>,
    pub operation: DragOperation,
    /// No iteration ran yet.
    pub first: bool,
    /// The buttons held (UI Events `buttons`).
    pub buttons: u16,
}

/// What a DND event's dispatch left: not canceled, and its DataTransfer's `dropEffect`.
struct DndResult {
    go: bool,
    drop_effect: String,
}

/// A file dragged in from outside the page: its name, MIME type and contents (empty until the
/// drop: the page cannot read them before).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedFile {
    pub name: String,
    pub type_: String,
    pub bytes: Vec<u8>,
    /// Last modified (ms since the epoch).
    pub modified: i64,
}

/// What a drag from outside the page carries (the platform's data, as MIME types).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalDrag {
    pub files: Vec<DroppedFile>,
    pub text: Option<String>,
    pub uri_list: Option<String>,
    pub html: Option<String>,
    /// The operations the source allows, as an `effectAllowed` value.
    pub allowed: String,
}

/// §6.11.7: whether an element's `draggable` IDL attribute is true: its `draggable` attribute's
/// state, and in the auto state, images and links (`a` with an `href`).
pub(crate) fn is_draggable(dom: &snowlark_dom::Dom, el: NodeId) -> bool {
    if !dom.is_element(el) {
        return false;
    }
    match dom.get_attribute(el, "draggable") {
        Some(v) if v.eq_ignore_ascii_case("true") => return true,
        Some(v) if v.eq_ignore_ascii_case("false") => return false,
        _ => {}
    }
    dom.is_html_element_named(el, &local_name!("img"))
        || (dom.is_html_element_named(el, &local_name!("a"))
            && dom.get_attribute(el, "href").is_some())
}

impl Engine {
    /// §6.11.5 "determine what is being dragged" for a press on `target`: the first inclusive
    /// ancestor (flat tree) whose `draggable` is true. A press in a text control, a select or
    /// editable content is for selecting there, never a drag.
    pub(crate) fn drag_candidate(&self, target: NodeId) -> Option<(NodeId, DragKind)> {
        let mut n = Some(target);
        while let Some(id) = n {
            if self.dom().is_element(id) {
                let select = self.dom().is_html_element_named(id, &local_name!("select"));
                if self.is_text_control(id) || select || self.is_editable(id) {
                    return None;
                }
                if is_draggable(&self.dom(), id) {
                    let link = {
                        let dom = self.dom();
                        dom.is_html_element_named(id, &local_name!("a"))
                            && dom.get_attribute(id, "href").is_some()
                    };
                    let kind = if link {
                        DragKind::Link
                    } else {
                        DragKind::Element
                    };
                    return Some((id, kind));
                }
            }
            n = self.flat_parent(id);
        }
        None
    }

    /// Whether a drag is in progress.
    pub fn dragging(&self) -> bool {
        self.pointer.borrow().drag.is_some()
    }

    /// Whether a move to `p` (top-level viewport) is far enough from a pending press to start a
    /// drag.
    pub(crate) fn drag_threshold_passed(&self, p: Point) -> Option<PendingDrag> {
        let ps = self.pointer.borrow();
        let pending = ps.may_drag?;
        ((p.x - pending.at.x).abs() >= DRAG_THRESHOLD
            || (p.y - pending.at.y).abs() >= DRAG_THRESHOLD)
            .then_some(pending)
    }

    /// The press's drag data (§6.11.5 "determine the list of dragged nodes" and the items it
    /// adds): a link's URL as `text/uri-list` (and `text/plain`), an image's URL as
    /// `text/uri-list`; nothing for other elements.
    fn initial_drag_items(&self, source: NodeId) -> Vec<Item> {
        let url = {
            let dom = self.dom();
            let attr = if dom.is_html_element_named(source, &local_name!("a")) {
                "href"
            } else if dom.is_html_element_named(source, &local_name!("img")) {
                "src"
            } else {
                return Vec::new();
            };
            match dom.get_attribute(source, attr) {
                Some(v) => v.trim().to_owned(),
                None => return Vec::new(),
            }
        };
        let Some(url) = self.encoding_parse_url(source, &url) else {
            return Vec::new();
        };
        let url = url.to_string();
        let mut items = vec![Item {
            data: ItemData::Text(url.clone()),
            type_: "text/uri-list".into(),
            object: None,
        }];
        if self.dom().is_html_element_named(source, &local_name!("a")) {
            items.push(Item {
                data: ItemData::Text(url),
                type_: "text/plain".into(),
                object: None,
            });
        }
        items
    }

    /// Starts the drag of a pending press, the pointer now at `p` (top-level viewport). Returns
    /// whether it started (`dragstart` was not canceled and the source is still there).
    pub(crate) fn start_drag(
        &self,
        cx: &mut Cx,
        pending: PendingDrag,
        p: Point,
        mods: Modifiers,
    ) -> bool {
        self.pointer.borrow_mut().may_drag = None;
        let alive = {
            let dom = self.dom();
            dom.is_alive(pending.source) && dom.is_connected(pending.source)
        };
        if !alive {
            return false;
        }
        let buttons = self.pointer.borrow().buttons;
        let session = DragSession {
            source: Some(pending.source),
            kind: pending.kind,
            items: self.initial_drag_items(pending.source),
            allowed: "uninitialized".into(),
            immediate: None,
            target: None,
            operation: DragOperation::None,
            first: true,
            buttons,
        };
        self.pointer.borrow_mut().drag = Some(Box::new(session));
        // (at the press, as the engines fire it)
        let r = self.fire_dnd(cx, "dragstart", pending.source, None, pending.client, mods);
        if !r.go {
            self.pointer.borrow_mut().drag = None;
            return false;
        }
        self.suppress_pointer_stream(cx, pending.source, p, mods);
        self.drag_iteration(cx, p, mods, false);
        true
    }

    /// §6.11.5 after `dragstart`, Pointer Events 3 "suppressing a pointer event stream":
    /// `pointercancel` at the source, then `pointerout` and `pointerleave` where the pointer is;
    /// the press is over (no `click`), the `:hover` and `:active` states are cleared, and the
    /// pointer capture and the selecting drags end.
    fn suppress_pointer_stream(&self, cx: &mut Cx, source: NodeId, top: Point, mods: Modifiers) {
        let (over, hovered, active, capture) = {
            let mut ps = self.pointer.borrow_mut();
            ps.pressed = None;
            ps.text_drag = None;
            ps.doc_drag = None;
            let capture = ps.capture.take();
            ps.pending_capture = None;
            (
                ps.over.take(),
                std::mem::take(&mut ps.hovered),
                std::mem::take(&mut ps.active),
                capture,
            )
        };
        let buttons = self.pointer.borrow().buttons;
        let p = self.client_point(source, top);
        let pe = InterfaceId::PointerEvent;
        self.fire_suppression(cx, pe, "pointercancel", source, p, buttons, mods);
        if let Some(c) = capture.filter(|&c| self.dom().is_alive(c)) {
            self.fire_suppression(cx, pe, "lostpointercapture", c, p, buttons, mods);
        }
        if let Some(o) = over.filter(|&o| self.dom().is_alive(o)) {
            let p = self.client_point(o, top);
            self.fire_suppression(cx, pe, "pointerout", o, p, buttons, mods);
            for &l in &hovered {
                if self.dom().is_alive(l) {
                    self.fire_suppression(cx, pe, "pointerleave", l, p, buttons, mods);
                }
            }
        }
        for h in hovered {
            self.set_element_state(h, ElementState::HOVER, false);
        }
        for a in active {
            self.set_element_state(a, ElementState::ACTIVE, false);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn fire_suppression(
        &self,
        cx: &mut Cx,
        interface: InterfaceId,
        type_: &str,
        target: NodeId,
        p: Point,
        buttons: u16,
        mods: Modifiers,
    ) {
        let e = self.pointer_event(
            interface,
            type_,
            target,
            p,
            -1,
            buttons,
            mods,
            0,
            None,
            |_| {},
        );
        // (`pointercancel` and `lostpointercapture` bubble and cannot be canceled; the
        // boundary events keep their own)
        if matches!(type_, "pointercancel" | "lostpointercapture") {
            self.ev_mut(e, |e| {
                e.bubbles = true;
                e.cancelable = false;
            });
        }
        self.dispatch(cx, e, node_handle(target), false);
    }

    /// A point of the top-level viewport in the viewport of `node`'s document (the frame it is
    /// in), as the hit test finds it there; the point itself when the hit is in another document.
    fn client_point(&self, node: NodeId, top: Point) -> Point {
        let doc = self.dom().node_document(node);
        match self.hit_element(top) {
            Some(h) if self.dom().node_document(h.element) == doc => h.point,
            _ => top,
        }
    }

    /// §6.11.4 "fire a DND event" `type_` at `target` with the drag data store of the drag in
    /// progress.
    fn fire_dnd(
        &self,
        cx: &mut Cx,
        type_: &str,
        target: NodeId,
        related: Option<NodeId>,
        p: Point,
        mods: Modifiers,
    ) -> DndResult {
        let Some((items, allowed, operation, kind, buttons)) =
            self.pointer.borrow().drag.as_ref().map(|s| {
                (
                    s.items.clone(),
                    s.allowed.clone(),
                    s.operation,
                    s.kind,
                    s.buttons,
                )
            })
        else {
            return DndResult {
                go: true,
                drop_effect: "none".into(),
            };
        };
        let mode = match type_ {
            "dragstart" => StoreMode::ReadWrite,
            "drop" => StoreMode::ReadOnly,
            _ => StoreMode::Protected,
        };
        let drop_effect = match type_ {
            "dragstart" | "drag" | "dragleave" => "none",
            "drop" | "dragend" => operation.as_str(),
            _ => initial_drop_effect(&allowed, kind, mods),
        };
        let dt = self.alloc(Object::DataTransfer(Box::new(DataTransferData {
            items: items
                .into_iter()
                .map(|i| Item { object: None, ..i })
                .collect(),
            drop_effect: drop_effect.into(),
            effect_allowed: allowed,
            mode,
            ..Default::default()
        })));
        let e = self.pointer_event(
            InterfaceId::DragEvent,
            type_,
            target,
            p,
            0,
            buttons,
            mods,
            0,
            related,
            |u| u.data_transfer = Some(dt),
        );
        self.ev_mut(e, |e| {
            e.bubbles = true;
            e.cancelable = !matches!(type_, "dragleave" | "dragend");
        });
        let go = self.dispatch(cx, e, node_handle(target), false);
        // After the dispatch: the allowed effects state from `effectAllowed`, the items a
        // `dragstart` set, and the association broken.
        let (allowed, drop_effect, items) = match &mut *self.obj_mut(dt) {
            Object::DataTransfer(d) => {
                d.mode = StoreMode::Disabled;
                let items = (type_ == "dragstart").then(|| {
                    d.items
                        .iter()
                        .map(|i| Item {
                            object: None,
                            ..i.clone()
                        })
                        .collect::<Vec<_>>()
                });
                (d.effect_allowed.clone(), d.drop_effect.clone(), items)
            }
            _ => unreachable!(),
        };
        if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
            s.allowed = allowed;
            if let Some(items) = items {
                s.items = items;
            }
        }
        DndResult { go, drop_effect }
    }

    /// The pointer moved to `p` during a drag: an iteration of the processing model.
    pub(crate) fn drag_moved(&self, cx: &mut Cx, p: Point, mods: Modifiers) {
        self.drag_iteration(cx, p, mods, false);
    }

    /// The button went up during a drag: the last iteration (a drop, or a failed drag).
    pub(crate) fn drag_released(&self, cx: &mut Cx, p: Point, mods: Modifiers) {
        if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
            s.buttons = 0;
        }
        self.drag_iteration(cx, p, mods, true);
    }

    /// The user canceled the drag (Escape): it ends without a drop.
    pub(crate) fn drag_canceled(&self, cx: &mut Cx, mods: Modifiers) {
        if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
            s.operation = DragOperation::None;
        }
        let p = self.pointer.borrow().at;
        self.end_drag(cx, p, mods, true);
    }

    /// §6.11.5, one iteration of the drag-and-drop operation at `p` (top-level viewport): `drag`
    /// at the source, then the target and the drag operation updated; `ended` (the user released
    /// the pointer) makes it the last one.
    fn drag_iteration(&self, cx: &mut Cx, p: Point, mods: Modifiers, ended: bool) {
        self.pointer.borrow_mut().at = p;
        let source = match self.pointer.borrow().drag.as_ref() {
            Some(s) => s.source,
            None => return,
        };
        let mut canceled = false;
        if let Some(src) = source {
            let r = self.fire_dnd(cx, "drag", src, None, self.client_point(src, p), mods);
            if !r.go {
                if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
                    s.operation = DragOperation::None;
                }
                canceled = true;
            }
        }
        if ended || canceled {
            self.end_drag(cx, p, mods, canceled);
            return;
        }
        self.update_drag_target(cx, p, mods);
    }

    /// §6.11.5 step 3 of an iteration: a new immediate user selection gets `dragenter` and
    /// becomes the current target element (the one it replaces gets `dragleave`); the current
    /// target gets `dragover`, which sets the current drag operation.
    fn update_drag_target(&self, cx: &mut Cx, p: Point, mods: Modifiers) {
        let hit = self.hit_element(p);
        let immediate = hit.map(|h| h.element);
        let (old_immediate, old_target, first) = match self.pointer.borrow().drag.as_ref() {
            Some(s) => (s.immediate, s.target, s.first),
            None => return,
        };
        let old_target = old_target.filter(|&t| self.dom().is_alive(t));
        if first || immediate != old_immediate || (old_target.is_none() && immediate.is_some()) {
            let mut target = None;
            if let Some(i) = immediate {
                let ip = hit.map_or(p, |h| h.point);
                self.fire_dnd(cx, "dragenter", i, old_target, ip, mods);
                target = Some(i).filter(|&i| self.dom().is_alive(i));
            }
            if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
                s.immediate = immediate;
                s.target = target;
                s.first = false;
            }
            if let Some(old) = old_target.filter(|&o| Some(o) != target) {
                let op = self.client_point(old, p);
                self.fire_dnd(cx, "dragleave", old, target, op, mods);
            }
        }
        let target = match self.pointer.borrow().drag.as_ref() {
            Some(s) => s.target.filter(|&t| self.dom().is_alive(t)),
            None => return,
        };
        let operation = match target {
            Some(t) => {
                let tp = self.client_point(t, p);
                let r = self.fire_dnd(cx, "dragover", t, None, tp, mods);
                if r.go {
                    // (the default action: a text control or editable element takes text)
                    if self.accepts_dropped_text(t) {
                        self.text_drop_operation()
                    } else {
                        DragOperation::None
                    }
                } else {
                    let allowed = self
                        .pointer
                        .borrow()
                        .drag
                        .as_ref()
                        .map(|s| s.allowed.clone())
                        .unwrap_or_default();
                    DragOperation::from_effects(&allowed, &r.drop_effect)
                }
            }
            None => DragOperation::None,
        };
        if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
            s.operation = operation;
        }
    }

    /// Whether a drop on `el` inserts the drag's text (§6.11.5: a text control or an editable
    /// element, and a `text/plain` item). Not yet: dropped text is not inserted, so nothing takes
    /// a drop that the page does not handle.
    fn accepts_dropped_text(&self, _el: NodeId) -> bool {
        false
    }

    /// The drag operation a text drop is: a move within the page, a copy from outside.
    fn text_drop_operation(&self) -> DragOperation {
        match self.pointer.borrow().drag.as_ref().map(|s| s.kind) {
            Some(DragKind::External) | None => DragOperation::Copy,
            _ => DragOperation::Move,
        }
    }

    /// §6.11.5, the last iteration: `drop` at the current target when it accepted the drag
    /// (else `dragleave`), then `dragend` at the source; the drag is over.
    fn end_drag(&self, cx: &mut Cx, p: Point, mods: Modifiers, canceled: bool) {
        let (target, operation, source) = match self.pointer.borrow().drag.as_ref() {
            Some(s) => (
                s.target.filter(|&t| self.dom().is_alive(t)),
                s.operation,
                s.source,
            ),
            None => return,
        };
        match target {
            Some(t) if !canceled && operation != DragOperation::None => {
                let tp = self.client_point(t, p);
                let r = self.fire_dnd(cx, "drop", t, None, tp, mods);
                if !r.go {
                    let allowed = self
                        .pointer
                        .borrow()
                        .drag
                        .as_ref()
                        .map(|s| s.allowed.clone())
                        .unwrap_or_default();
                    let op = DragOperation::from_effects(&allowed, &r.drop_effect);
                    if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
                        s.operation = op;
                    }
                }
            }
            other => {
                if let Some(t) = other {
                    let tp = self.client_point(t, p);
                    self.fire_dnd(cx, "dragleave", t, None, tp, mods);
                }
                if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
                    s.operation = DragOperation::None;
                }
            }
        }
        if let Some(src) = source {
            let sp = self.client_point(src, p);
            self.fire_dnd(cx, "dragend", src, None, sp, mods);
        }
        self.pointer.borrow_mut().drag = None;
        self.microtask_checkpoint(cx);
    }

    // ----- drags from outside the page ------------------------------------------------------

    /// A drag from outside the page entered it (or moved over it) at `p`: an iteration, the
    /// drag data store made from `data` on entering. Returns the current drag operation.
    pub fn external_drag_over(
        &self,
        cx: &mut Cx,
        data: Option<&ExternalDrag>,
        p: Point,
        mods: Modifiers,
    ) -> DragOperation {
        if self.pointer.borrow().drag.is_none() {
            let Some(data) = data else {
                return DragOperation::None;
            };
            let items = self.external_items(data);
            let allowed = if data.allowed.is_empty() {
                "all".to_owned()
            } else {
                data.allowed.clone()
            };
            self.pointer.borrow_mut().drag = Some(Box::new(DragSession {
                source: None,
                kind: DragKind::External,
                items,
                allowed,
                immediate: None,
                target: None,
                operation: DragOperation::None,
                first: true,
                buttons: 1,
            }));
        }
        self.drag_iteration(cx, p, mods, false);
        self.microtask_checkpoint(cx);
        self.pointer
            .borrow()
            .drag
            .as_ref()
            .map_or(DragOperation::None, |s| s.operation)
    }

    /// A drag from outside the page left it (or was canceled there): `dragleave` at the current
    /// target, and the page has no drag any more.
    pub fn external_drag_leave(&self, cx: &mut Cx) {
        let target = match self.pointer.borrow().drag.as_ref() {
            Some(s) if s.source.is_none() => s.target.filter(|&t| self.dom().is_alive(t)),
            _ => return,
        };
        if let Some(t) = target {
            let p = self.pointer.borrow().at;
            let tp = self.client_point(t, p);
            self.fire_dnd(cx, "dragleave", t, None, tp, Modifiers::default());
        }
        self.pointer.borrow_mut().drag = None;
        self.microtask_checkpoint(cx);
    }

    /// A drag from outside the page was dropped at `p`, with `data` (the files' contents now)
    /// replacing what it entered with: the last iteration. Returns the drag operation the drop
    /// ended with (none: nothing took it).
    pub fn external_drag_drop(
        &self,
        cx: &mut Cx,
        data: Option<&ExternalDrag>,
        p: Point,
        mods: Modifiers,
    ) -> DragOperation {
        let external = self
            .pointer
            .borrow()
            .drag
            .as_ref()
            .is_some_and(|s| s.source.is_none());
        if !external {
            return DragOperation::None;
        }
        // (the result the drop leaves, read before the session ends)
        let (target, operation) = match self.pointer.borrow().drag.as_ref() {
            Some(s) => (s.target, s.operation),
            None => (None, DragOperation::None),
        };
        if target.is_none() || operation == DragOperation::None {
            self.external_drag_leave(cx);
            return DragOperation::None;
        }
        let result = std::cell::Cell::new(operation);
        if let Some(data) = data {
            let items = self.external_items(data);
            if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
                s.items = items;
            }
        }
        self.pointer.borrow_mut().at = p;
        // `drop` at the target; its result is the session's operation after it.
        if let Some(t) = target.filter(|&t| self.dom().is_alive(t)) {
            let tp = self.client_point(t, p);
            let r = self.fire_dnd(cx, "drop", t, None, tp, mods);
            if !r.go {
                let allowed = self
                    .pointer
                    .borrow()
                    .drag
                    .as_ref()
                    .map(|s| s.allowed.clone())
                    .unwrap_or_default();
                result.set(DragOperation::from_effects(&allowed, &r.drop_effect));
            }
        } else {
            result.set(DragOperation::None);
        }
        self.pointer.borrow_mut().drag = None;
        self.microtask_checkpoint(cx);
        result.get()
    }

    /// The drag data store items of a drag from outside: one File item per file (its MIME type
    /// or `application/octet-stream`), then the text formats.
    fn external_items(&self, data: &ExternalDrag) -> Vec<Item> {
        let mut items = Vec::new();
        for (format, value) in [
            ("text/plain", &data.text),
            ("text/uri-list", &data.uri_list),
            ("text/html", &data.html),
        ] {
            if let Some(v) = value {
                items.push(Item {
                    data: ItemData::Text(v.clone()),
                    type_: format.into(),
                    object: None,
                });
            }
        }
        for f in &data.files {
            let type_ = if f.type_.is_empty() {
                "application/octet-stream".to_owned()
            } else {
                f.type_.to_ascii_lowercase()
            };
            let file = self.new_file(f.bytes.clone(), &f.type_, &f.name, f.modified);
            items.push(Item {
                data: ItemData::File(file),
                type_,
                object: None,
            });
        }
        items
    }
}

/// §6.11.4: the `dropEffect` of a `dragenter` or `dragover` from `effectAllowed` and what is
/// dragged: the table's first value, or the alternative the platform's modifier keys ask for
/// (Windows: Ctrl copies, Shift moves, Ctrl+Shift or Alt links).
fn initial_drop_effect(allowed: &str, kind: DragKind, mods: Modifiers) -> &'static str {
    let options: &[&'static str] = match allowed {
        "none" => &["none"],
        "copy" => &["copy"],
        "copyLink" => &["copy", "link"],
        "copyMove" => &["copy", "move"],
        "all" => &["copy", "link", "move"],
        "link" => &["link"],
        "linkMove" => &["link", "move"],
        "move" => &["move"],
        _ if kind == DragKind::Link => &["link", "copy", "move"],
        _ => &["copy", "link", "move"],
    };
    let wanted = if (mods.ctrl && mods.shift) || mods.alt {
        Some("link")
    } else if mods.ctrl {
        Some("copy")
    } else if mods.shift {
        Some("move")
    } else {
        None
    };
    match wanted {
        Some(w) if options.contains(&w) => w,
        _ => options[0],
    }
}

impl Page {
    /// A drag from outside the page (the window's drop target) entered it with `data`, or moved
    /// over it (`data` None) to `p` (viewport CSS px). Returns the current drag operation.
    pub fn drag_over(
        &mut self,
        data: Option<&ExternalDrag>,
        p: Point,
        mods: Modifiers,
    ) -> DragOperation {
        self.with(|e, cx| {
            e.flush_held_move(cx);
            e.external_drag_over(cx, data, p, mods)
        })
    }

    /// The drag from outside left the page.
    pub fn drag_leave(&mut self) {
        self.with(|e, cx| e.external_drag_leave(cx))
    }

    /// The drag from outside was dropped at `p`, with its data (`None`: as it entered). Returns
    /// the operation it ended with.
    pub fn drop_dragged(
        &mut self,
        data: Option<&ExternalDrag>,
        p: Point,
        mods: Modifiers,
    ) -> DragOperation {
        self.with(|e, cx| e.external_drag_drop(cx, data, p, mods))
    }

    /// Whether a drag is in progress (one that started in the page, or one from outside).
    pub fn dragging(&self) -> bool {
        self.engine().dragging()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_canceled_dragover_sets_the_operation_from_the_effects() {
        let op = DragOperation::from_effects;
        assert_eq!(op("uninitialized", "copy"), DragOperation::Copy);
        assert_eq!(op("copyMove", "move"), DragOperation::Move);
        assert_eq!(op("copyMove", "link"), DragOperation::None);
        assert_eq!(op("none", "copy"), DragOperation::None);
        assert_eq!(op("all", "link"), DragOperation::Link);
        assert_eq!(op("link", "none"), DragOperation::None);
    }

    #[test]
    fn the_initial_drop_effect_follows_effect_allowed() {
        let m = Modifiers::default();
        assert_eq!(initial_drop_effect("none", DragKind::Element, m), "none");
        assert_eq!(
            initial_drop_effect("copyMove", DragKind::Element, m),
            "copy"
        );
        assert_eq!(
            initial_drop_effect("linkMove", DragKind::Element, m),
            "link"
        );
        assert_eq!(
            initial_drop_effect("uninitialized", DragKind::Link, m),
            "link"
        );
        assert_eq!(
            initial_drop_effect("uninitialized", DragKind::Element, m),
            "copy"
        );
        let shift = Modifiers { shift: true, ..m };
        assert_eq!(
            initial_drop_effect("copyMove", DragKind::Element, shift),
            "move"
        );
        assert_eq!(
            initial_drop_effect("copy", DragKind::Element, shift),
            "copy"
        );
    }
}
