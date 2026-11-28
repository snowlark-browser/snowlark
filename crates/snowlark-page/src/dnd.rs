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
//! In a window (system drags, [`Engine::set_system_drags`]) a drag the page starts leaves the
//! page for the platform's drag loop, which carries it to other programs and back: after a
//! `dragstart` nothing canceled the page hands the embedder its drag data store ([`DragOut`]),
//! the window's drop target drives the iterations while the pointer is over the page (the
//! session keeps the page's own store, so a drop back on the page reads what `dragstart` set),
//! and the loop's end fires `dragend` with the operation the target carried out
//! ([`Engine::system_drag_ended`]).
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
    /// The page's drag, carried on by the platform's drag loop (system drags).
    pub system: bool,
}

/// A drag the page started that the embedder's drag loop carries on (system drags): the drag
/// data store's text items as `dragstart` left them, and what the platform's formats offer beyond
/// them (a link's title, an image's file).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DragOut {
    /// The type and data of each text item, in order.
    pub items: Vec<(String, String)>,
    /// `effectAllowed` after `dragstart`.
    pub allowed: String,
    /// The link's or image's URL, as the drag began with it.
    pub url: Option<String>,
    /// Its title: a link's text, an image's `alt`.
    pub title: String,
    /// The link or image as markup (`text/html` for other programs, while the store's
    /// `text/uri-list` is still `url`).
    pub markup: Option<String>,
    /// A dragged image's file and picture.
    pub image: Option<DraggedImage>,
}

/// A dragged image: a file of its pixels and the picture that follows the pointer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraggedImage {
    /// The file's name without its extension (from the URL's last path segment; may be empty).
    pub stem: String,
    /// The image's pixels as a PNG file (the first frame of an animation).
    pub png: Vec<u8>,
    /// The picture: its size and premultiplied BGRA pixels, rows `width * 4` bytes apart.
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// The drag picture of an image is at most this many pixels a side.
const PICTURE_MAX: f32 = 200.0;

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
    /// ancestor (flat tree) whose `draggable` is true, or whose `-webkit-user-drag` is `element`
    /// (one whose `-webkit-user-drag` is `none` is skipped). A press in a text control, a select or
    /// editable content is for selecting there, never a drag.
    pub(crate) fn drag_candidate(&self, target: NodeId) -> Option<(NodeId, DragKind)> {
        let mut n = Some(target);
        while let Some(id) = n {
            if self.dom().is_element(id) {
                let select = self.dom().is_html_element_named(id, &local_name!("select"));
                if self.is_text_control(id) || select || self.is_editable(id) {
                    return None;
                }
                // `-webkit-user-drag` (Compat): `element` makes the box a drag source, `none`
                // keeps it from being one (its ancestors may still be), `auto` leaves it to
                // `draggable`.
                let user_drag = self
                    .with_style(id, |r| r.resolved_property(id, "-webkit-user-drag"))
                    .flatten();
                let source = match user_drag.as_deref() {
                    Some("element") => true,
                    Some("none") => false,
                    _ => is_draggable(&self.dom(), id),
                };
                if source {
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
            system: false,
        };
        self.pointer.borrow_mut().drag = Some(Box::new(session));
        // (at the press, as the engines fire it)
        let r = self.fire_dnd(cx, "dragstart", pending.source, None, pending.client, mods);
        if !r.go {
            self.pointer.borrow_mut().drag = None;
            return false;
        }
        self.suppress_pointer_stream(cx, pending.source, p, mods);
        if self.embedder.borrow().system_drags {
            // The platform's drag loop takes it from here: its drop target over the page runs
            // the iterations.
            let out = self.drag_out(pending.source);
            if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
                s.system = true;
            }
            self.embedder.borrow_mut().drag_out = Some(Box::new(out));
            return true;
        }
        self.drag_iteration(cx, p, mods, false);
        true
    }

    /// Drags the page starts are carried on by the embedder's drag loop ([`Engine::take_drag_out`],
    /// [`Engine::system_drag_ended`]); off, they stay in the page (headless runs).
    pub fn set_system_drags(&self, on: bool) {
        self.embedder.borrow_mut().system_drags = on;
    }

    /// A drag the page started since the embedder last asked (system drags).
    pub fn take_drag_out(&self) -> Option<DragOut> {
        self.embedder.borrow_mut().drag_out.take().map(|b| *b)
    }

    /// Whether the drag in progress is the page's own, carried on by the platform's drag loop.
    fn system_dragging(&self) -> bool {
        self.pointer
            .borrow()
            .drag
            .as_ref()
            .is_some_and(|s| s.system)
    }

    /// A system drag whose end the embedder never told (it took no drag loop) ends as one
    /// nothing took.
    pub(crate) fn end_stale_system_drag(&self, cx: &mut Cx) {
        if self.system_dragging() {
            self.system_drag_ended(cx, DragOperation::None);
        }
    }

    /// What the platform's formats get of the drag of `source` that `dragstart` just set up.
    fn drag_out(&self, source: NodeId) -> DragOut {
        let (items, allowed) = match self.pointer.borrow().drag.as_ref() {
            Some(s) => (
                s.items
                    .iter()
                    .filter_map(|i| match &i.data {
                        ItemData::Text(t) => Some((i.type_.clone(), t.clone())),
                        _ => None,
                    })
                    .collect(),
                s.allowed.clone(),
            ),
            None => (Vec::new(), String::new()),
        };
        let url = self
            .initial_drag_items(source)
            .into_iter()
            .find(|i| i.type_ == "text/uri-list")
            .and_then(|i| match i.data {
                ItemData::Text(t) => Some(t),
                _ => None,
            });
        let (image, title) = {
            let dom = self.dom();
            if dom.is_html_element_named(source, &local_name!("img")) {
                (
                    true,
                    dom.get_attribute(source, "alt")
                        .unwrap_or_default()
                        .to_owned(),
                )
            } else {
                let text = dom.descendant_text_content(source);
                (false, text.split_whitespace().collect::<Vec<_>>().join(" "))
            }
        };
        let markup = url.as_deref().map(|u| {
            if image {
                format!(
                    "<img src=\"{}\" alt=\"{}\">",
                    escape_markup(u),
                    escape_markup(&title)
                )
            } else {
                format!(
                    "<a href=\"{}\">{}</a>",
                    escape_markup(u),
                    escape_markup(&title)
                )
            }
        });
        let image = if image {
            self.dragged_image(source, url.as_deref())
        } else {
            None
        };
        DragOut {
            items,
            allowed,
            url,
            title,
            markup,
            image,
        }
    }

    /// An image's file (its decoded pixels as a PNG) and its drag picture: the image scaled to
    /// at most [`PICTURE_MAX`] pixels a side, at three quarters opacity. None while it has no
    /// pixels.
    fn dragged_image(&self, el: NodeId, url: Option<&str>) -> Option<DraggedImage> {
        use snowlark_render::canvas::skia as tiny_skia;
        let image = self
            .with_node_renderer(el, |r, _| r.image_pixels(el))
            .flatten()?;
        let pm = snowlark_render::canvas::pixmap_from_frame(image.default_frame())?;
        let png = pm.encode_png().ok()?;
        let (w, h) = (pm.width() as f32, pm.height() as f32);
        let s = (PICTURE_MAX / w.max(h)).min(1.0);
        let (pw, ph) = (
            ((w * s).round() as u32).max(1),
            ((h * s).round() as u32).max(1),
        );
        let mut picture = tiny_skia::Pixmap::new(pw, ph)?;
        let paint = tiny_skia::PixmapPaint {
            opacity: 0.75,
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        };
        let scale = tiny_skia::Transform::from_scale(pw as f32 / w, ph as f32 / h);
        picture.draw_pixmap(0, 0, pm.as_ref(), &paint, scale, None);
        let mut bgra = picture.take();
        for px in bgra.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        Some(DraggedImage {
            stem: url.map(file_stem).unwrap_or_default(),
            png,
            width: pw,
            height: ph,
            bgra,
        })
    }

    /// The platform's drag loop of the page's drag ended: `operation` is what its target carried
    /// out (none: canceled, or nothing took it). `dragend` at the source; the drag is over.
    pub fn system_drag_ended(&self, cx: &mut Cx, operation: DragOperation) {
        let (source, target) = match self.pointer.borrow().drag.as_ref() {
            Some(s) if s.system => (s.source, s.target.filter(|&t| self.dom().is_alive(t))),
            _ => return,
        };
        // (a drag the embedder has not taken yet is over too)
        self.embedder.borrow_mut().drag_out = None;
        let p = self.pointer.borrow().at;
        // (the drop target's leave did not come: the loop ended over the page)
        if let Some(t) = target {
            let tp = self.client_point(t, p);
            self.fire_dnd(cx, "dragleave", t, None, tp, Modifiers::default());
        }
        if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
            s.operation = operation;
            s.buttons = 0;
        }
        if let Some(src) = source.filter(|&s| self.dom().is_alive(s)) {
            let sp = self.client_point(src, p);
            self.fire_dnd(cx, "dragend", src, None, sp, Modifiers::default());
        }
        {
            let mut ps = self.pointer.borrow_mut();
            ps.drag = None;
            // (the loop took the button's release)
            ps.buttons = 0;
        }
        self.microtask_checkpoint(cx);
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
        // (a system drag's moves are the drag loop's)
        if self.system_dragging() {
            return;
        }
        self.drag_iteration(cx, p, mods, false);
    }

    /// The button went up during a drag: the last iteration (a drop, or a failed drag).
    pub(crate) fn drag_released(&self, cx: &mut Cx, p: Point, mods: Modifiers) {
        if self.system_dragging() {
            return;
        }
        if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
            s.buttons = 0;
        }
        self.drag_iteration(cx, p, mods, true);
    }

    /// The user canceled the drag (Escape): it ends without a drop.
    pub(crate) fn drag_canceled(&self, cx: &mut Cx, mods: Modifiers) {
        // (the drag loop sees Escape itself)
        if self.system_dragging() {
            return;
        }
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
        let (source, system) = match self.pointer.borrow().drag.as_ref() {
            Some(s) => (s.source, s.system),
            None => return,
        };
        let mut canceled = false;
        if let Some(src) = source {
            let r = self.fire_dnd(cx, "drag", src, None, self.client_point(src, p), mods);
            if !r.go {
                if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
                    s.operation = DragOperation::None;
                }
                // (the drag loop goes on to its end: nothing takes this iteration's drop)
                if system {
                    return;
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
                system: false,
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
    /// target, and the page has no drag any more. The page's own system drag goes on (it may
    /// come back): it only has no target.
    pub fn external_drag_leave(&self, cx: &mut Cx) {
        let (target, system) = match self.pointer.borrow().drag.as_ref() {
            Some(s) if s.source.is_none() || s.system => {
                (s.target.filter(|&t| self.dom().is_alive(t)), s.system)
            }
            _ => return,
        };
        if let Some(t) = target {
            let p = self.pointer.borrow().at;
            let tp = self.client_point(t, p);
            self.fire_dnd(cx, "dragleave", t, None, tp, Modifiers::default());
        }
        if system {
            if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
                s.immediate = None;
                s.target = None;
                s.operation = DragOperation::None;
                s.first = true;
            }
        } else {
            self.pointer.borrow_mut().drag = None;
        }
        self.microtask_checkpoint(cx);
    }

    /// A drag from outside the page was dropped at `p`, with `data` (the files' contents now)
    /// replacing what it entered with: the last iteration. Returns the drag operation the drop
    /// ended with (none: nothing took it). The page's own system drag keeps its store and goes
    /// on to the end of the drag loop ([`Engine::system_drag_ended`]).
    pub fn external_drag_drop(
        &self,
        cx: &mut Cx,
        data: Option<&ExternalDrag>,
        p: Point,
        mods: Modifiers,
    ) -> DragOperation {
        let (external, system) = match self.pointer.borrow().drag.as_ref() {
            Some(s) => (s.source.is_none(), s.system),
            None => (false, false),
        };
        if !external && !system {
            return DragOperation::None;
        }
        let data = data.filter(|_| !system);
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
        if system {
            if let Some(s) = self.pointer.borrow_mut().drag.as_mut() {
                s.immediate = None;
                s.target = None;
                s.operation = result.get();
            }
        } else {
            self.pointer.borrow_mut().drag = None;
        }
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

/// Text as markup text or a quoted attribute value.
fn escape_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// A file name for what `url` names: its last path segment, percent-decoded, without its
/// extension; empty for a URL without one (`data:`, `blob:`, a path ending in `/`).
fn file_stem(url: &str) -> String {
    let Ok(u) = url::Url::parse(url) else {
        return String::new();
    };
    let Some(last) = u.path_segments().and_then(|mut s| s.next_back()) else {
        return String::new();
    };
    let bytes = last.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(h), Some(l)) => {
                decoded.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, _, _) => {
                decoded.push(b);
                i += 1;
            }
        }
    }
    let name = String::from_utf8_lossy(&decoded);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_owned(),
        _ => name.into_owned(),
    }
}

impl Page {
    /// The platform's drag loop of the page's own drag ended with `operation` (system drags):
    /// `dragend`.
    pub fn system_drag_ended(&mut self, operation: DragOperation) {
        self.with(|e, cx| e.system_drag_ended(cx, operation))
    }

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
    fn a_dragged_image_file_is_named_after_its_url() {
        assert_eq!(
            file_stem("https://a.example/img/cat%20photo.jpg?x=1"),
            "cat photo"
        );
        assert_eq!(file_stem("https://a.example/logo"), "logo");
        assert_eq!(file_stem("https://a.example/.hidden"), ".hidden");
        assert_eq!(file_stem("https://a.example/dir/"), "");
        assert_eq!(file_stem("data:image/png;base64,AAAA"), "");
        assert_eq!(
            escape_markup("a \"b\" <c> & d"),
            "a &quot;b&quot; &lt;c&gt; &amp; d"
        );
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
