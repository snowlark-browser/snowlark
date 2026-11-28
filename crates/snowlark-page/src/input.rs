// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! User input into the page: pointer input as UI Events and Pointer Events (UI Events §3, Pointer
//! Events §4: `pointerover`/`mouseover`, `pointerenter`/`mouseenter`, the `out`/`leave` pairs,
//! `pointermove`/`mousemove`, `pointerdown`/`mousedown`, `pointerup`/`mouseup`, `click`, `auxclick`,
//! `dblclick`, `contextmenu`, `wheel`), with the default actions a canceled event suppresses: focusing
//! on press (HTML §6.6.3 "focusing steps"), the `:hover` and `:active` states, a link's middle-click
//! opening a new tab. Activation behavior (links, buttons, labels, checkboxes) runs from the `click`
//! dispatch itself. Keyboard input and editing are in [`crate::keyboard`].

use snowlark_bindings::rt::{Cx, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, LocalName as LocalNameRef, NodeId};
use snowlark_paint::Point;
use snowlark_style::ElementState;

use crate::engine::events::{EventExt, UiFields};
use crate::engine::{node_handle, Engine};
use crate::Page;

/// Modifier keys held during input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

/// What pointer input found and did, for the embedder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PointerOutcome {
    /// The CSS `cursor` keyword over the hit element (`auto` resolved: `pointer` over links, `text`
    /// over text and text fields, `default` elsewhere).
    pub cursor: String,
    /// The hyperlink under the pointer (for the status line).
    pub link: Option<url::Url>,
    /// The advisory information under the pointer (a tooltip).
    pub title: Option<String>,
    /// For a press: the page canceled its `mousedown` (UI Events: the default action, here the
    /// embedder's autoscroll, does not happen).
    pub press_canceled: bool,
    /// For a press: it is on a link or in a form control or an editable element, where a middle
    /// press means something else than autoscroll.
    pub autoscroll_blocked: bool,
}

/// The pointer's state between events.
#[derive(Default)]
pub struct PointerState {
    /// The element under the pointer, and its flat-tree ancestors in `:hover`.
    pub over: Option<NodeId>,
    pub hovered: Vec<NodeId>,
    /// `over` stands in for the element under the pointer that left the document: the parent
    /// element that remained (the boundary events after a node removal of Pointer Events 3 and UI
    /// Events: no `out` for the removed one, no `enter` again for what it was inside, an `over` at
    /// the next boundary update even when that is the same element).
    pub over_removed: bool,
    /// The pointer is over the page, at `at` (the top-level viewport's CSS px).
    pub inside: bool,
    pub at: Point,
    /// Where the last `pointermove` was on the screen (`screenX`, `screenY`), for the next one's
    /// `movementX` and `movementY`; none since the pointer came over the page.
    pub move_screen: Option<(f64, f64)>,
    /// The layout and scroll the hover state was last computed against (re-hit-tested at the next
    /// rendering update when they change).
    pub hover_layout: Option<(u64, Point)>,
    /// Pointer Events 3 "pointer capture": the mouse's pointer capture target override and its pending one
    /// (`setPointerCapture` sets the pending one; it takes effect at the next pointer event).
    pub capture: Option<NodeId>,
    pub pending_capture: Option<NodeId>,
    /// The document the last press was in: only its elements can capture the pointer.
    pub capture_doc: Option<NodeId>,
    /// The element and button of the press in progress, and the `:active` chain.
    pub pressed: Option<(NodeId, u16)>,
    pub active: Vec<NodeId>,
    /// The buttons held (UI Events `buttons` bits).
    pub buttons: u16,
    pub last: Point,
    /// Pointer Events 3 §11.2 "PREVENT MOUSE EVENT flag" of the mouse: its primary `pointerdown`
    /// was canceled, so no compatibility `mousedown`, `mousemove` or `mouseup` is fired until the
    /// `pointerup` (the boundary mouse events and `click` still are).
    pub prevent_mouse: bool,
    /// The last `keydown` was canceled: its character is not typed.
    pub suppress_char: bool,
    /// A press-and-drag selecting in a text control.
    pub text_drag: Option<crate::caret::TextDrag>,
    /// A press-and-drag selecting in a document.
    pub doc_drag: Option<crate::editing::DocDrag>,
    /// A press on a draggable element: a drag starts if the pointer moves far enough before the
    /// release (HTML §6.11.5).
    pub may_drag: Option<crate::dnd::PendingDrag>,
    /// A press on a resizer being dragged (CSS UI 4 §5.1).
    pub resizing: Option<crate::resizer::Resizing>,
    /// The drag-and-drop operation in progress, which suppresses the pointer events.
    pub drag: Option<Box<crate::dnd::DragSession>>,
    /// A move held for the start of the next rendering update ([`Engine::queue_pointer_move`]).
    pub held: Option<HeldMove>,
    /// What the held move found when it was dispatched, for the embedder to take
    /// ([`Engine::take_held_outcome`]).
    pub held_outcome: Option<PointerOutcome>,
}

/// A pointer move waiting for the next rendering update, with the moves merged into it.
pub struct HeldMove {
    pub at: Point,
    pub mods: Modifiers,
    /// When the first of them came (ms since the time origin).
    pub since: f64,
    /// The moves it stands for, oldest first: where each went and when (ms since the time
    /// origin), the `pointermove`'s coalesced events.
    pub moves: Vec<(Point, f64)>,
}

/// How long a held move waits for a rendering update before it is dispatched anyway (ms): a
/// fallback for a frame that never comes.
pub(crate) const HELD_MOVE_FALLBACK_MS: f64 = 5000.0;

/// UI Events `button` for a button index: 0 primary, 1 auxiliary, 2 secondary, 3 back, 4 forward.
fn button_bit(button: u16) -> u16 {
    match button {
        0 => 1,
        1 => 4,
        2 => 2,
        3 => 8,
        4 => 16,
        _ => 0,
    }
}

/// The parent in the flat tree (a shadow root's host for its children), for hover chains.
impl Engine {
    /// A node's parent in the flat tree (CSS Scoping 1 §2.2): its assigned slot, else its parent
    /// element, or the host of the shadow root it is a child of.
    pub(crate) fn flat_parent(&self, n: NodeId) -> Option<NodeId> {
        if !self.dom().is_alive(n) {
            return None;
        }
        if let Some(slot) = self.assigned_slot_of(n) {
            return Some(slot);
        }
        let dom = self.dom();
        let p = dom.parent(n)?;
        if dom.is_shadow_root(p) {
            return dom.host(p);
        }
        dom.is_element(p).then_some(p)
    }

    /// The flat-tree inclusive ancestors of `n`.
    pub(crate) fn chain(&self, n: Option<NodeId>) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut cur = n;
        while let Some(id) = cur {
            out.push(id);
            cur = self.flat_parent(id);
        }
        out
    }

    /// The flat-tree inclusive ancestors of `n`, continuing from a frame document's root element to
    /// the frame element that shows it (`:hover` and `:active` chains, the click target).
    fn chain_across(&self, n: Option<NodeId>) -> Vec<NodeId> {
        let mut out = self.chain(n.filter(|&o| self.dom().is_alive(o)));
        while let Some(&last) = out.last() {
            let doc = self.dom().node_document(last);
            match self.rendered_container(doc) {
                Some(c) if self.dom().is_alive(c) && !out.contains(&c) => {
                    out.extend(self.chain(Some(c)))
                }
                _ => break,
            }
        }
        out
    }

    /// Sets page-owned element state (`:hover`, `:active`, `:focus`...) in the style engine.
    pub(crate) fn set_element_state(&self, el: NodeId, state: ElementState, on: bool) {
        // (what the developer tools force stays on, whatever the pointer and focus do)
        let on = on || self.devtools_forced(el, state);
        self.with_node_renderer(el, |r, st| {
            if r.set_element_state(el, state, on) {
                st.dirty = true;
            }
        });
    }

    /// [`Engine::set_element_state`] for several bits in one call: those of `mask` take the values
    /// `bits` has for them.
    pub(crate) fn set_element_states(&self, el: NodeId, mask: ElementState, bits: ElementState) {
        let bits = (bits | self.forced_mask(el)) & mask;
        self.with_node_renderer(el, |r, st| {
            if r.set_element_states(el, mask, bits) {
                st.dirty = true;
            }
        });
    }

    /// Hands `state` bits back to what the DOM derives (a link's `:visited`).
    pub(crate) fn reset_element_state(&self, el: NodeId, state: ElementState) {
        self.with_node_renderer(el, |r, st| {
            if r.reset_element_state(el, state) {
                st.dirty = true;
            }
        });
    }

    /// A trusted mouse or pointer event with the pointer's position and buttons.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pointer_event(
        &self,
        interface: InterfaceId,
        type_: &str,
        target: NodeId,
        p: Point,
        button: i16,
        buttons: u16,
        mods: Modifiers,
        detail: i32,
        related: Option<NodeId>,
        extra: impl FnOnce(&mut UiFields),
    ) -> Handle {
        let (bubbles, cancelable) = match type_ {
            "mouseenter" | "mouseleave" | "pointerenter" | "pointerleave" => (false, false),
            "pointermove" | "pointerover" | "pointerout" | "mousemove" | "mouseover"
            | "mouseout" | "pointerdown" | "pointerup" | "mousedown" | "mouseup" | "click"
            | "auxclick" | "dblclick" | "contextmenu" | "wheel" => (true, true),
            _ => (true, false),
        };
        let pointer = interface == InterfaceId::PointerEvent;
        let doc = self.dom().node_document(target);
        // A related target in another document (a frame's) is not exposed.
        let related = related.filter(|&r| {
            let dom = self.dom();
            dom.is_alive(r) && dom.node_document(r) == doc
        });
        // UI Events §5.3.1: screen coordinates are the point's place on the screen, not in the
        // viewport (the window says where the viewport is); the legacy `which` is `button` + 1 for
        // the events a button causes, 0 for the others (§7.1.1, as the engines answer it).
        let (screen_x, screen_y) = self
            .screen_of_document(doc)
            .to_screen(p.x as f64, p.y as f64);
        let which = match type_ {
            "pointerdown" | "pointerup" | "mousedown" | "mouseup" | "click" | "auxclick"
            | "dblclick" | "contextmenu" => (button + 1).max(0) as u32,
            _ => 0,
        };
        let mut u = UiFields {
            view: self.window_of_document(doc),
            detail,
            ctrl: mods.ctrl,
            shift: mods.shift,
            alt: mods.alt,
            meta: mods.meta,
            screen_x: screen_x as f64,
            screen_y: screen_y as f64,
            client_x: p.x as f64,
            client_y: p.y as f64,
            which,
            button,
            buttons,
            related_target: related.map(node_handle),
            pointer_id: if pointer { 1 } else { 0 },
            pointer_type: if pointer {
                "mouse".into()
            } else {
                String::new()
            },
            is_primary: pointer,
            width: if pointer { 1.0 } else { 0.0 },
            height: if pointer { 1.0 } else { 0.0 },
            pressure: if pointer && buttons != 0 { 0.5 } else { 0.0 },
            ..Default::default()
        };
        extra(&mut u);
        let h = self.new_event(
            interface,
            type_,
            bubbles,
            cancelable,
            true,
            EventExt::Ui(Box::new(u)),
        );
        self.ev_mut(h, |e| {
            e.is_trusted = true;
            e.related_target = related.map(node_handle);
        });
        h
    }

    /// Fires a pointer or mouse event; returns whether it was not canceled.
    #[allow(clippy::too_many_arguments)]
    fn fire_pointer(
        &self,
        cx: &mut Cx,
        interface: InterfaceId,
        type_: &str,
        target: NodeId,
        p: Point,
        button: i16,
        buttons: u16,
        mods: Modifiers,
        detail: i32,
        related: Option<NodeId>,
    ) -> bool {
        if !self.dom().is_alive(target) {
            return true;
        }
        // (HTML "run light dismiss activities": a trusted pointerdown or pointerup, before its
        // dispatch; popovers, then dialogs)
        if matches!(type_, "pointerdown" | "pointerup") {
            let down = type_ == "pointerdown";
            self.popover_light_dismiss(cx, target, down);
            if !self.dom().is_alive(target) {
                return true;
            }
            self.dialog_light_dismiss(cx, target, (p.x as f64, p.y as f64), down);
            if !self.dom().is_alive(target) {
                return true;
            }
        }
        let e = self.pointer_event(
            interface,
            type_,
            target,
            p,
            button,
            buttons,
            mods,
            detail,
            related,
            |_| {},
        );
        self.dispatch(cx, e, node_handle(target), false)
    }

    /// Fires a trusted `pointermove` at `target` with its coalesced events (Pointer Events 3
    /// §"Coalesced events"): one per move it stands for (`moves`, where each went and when), or a
    /// copy of itself for a move alone. Each is trusted, does not bubble and cannot be canceled,
    /// has the move's target and no coalesced events of its own, and they are in time order.
    /// Pointer Lock 2 §5: `movementX` and `movementY` are how far `screenX` and `screenY` moved
    /// since the last move (0 for the first since the pointer came over the page), each coalesced
    /// event's since the one before it. Returns whether it was not canceled, and its movement.
    fn fire_pointer_move(
        &self,
        cx: &mut Cx,
        target: NodeId,
        p: Point,
        buttons: u16,
        mods: Modifiers,
        moves: &[(Point, f64)],
    ) -> (bool, (f64, f64)) {
        if !self.dom().is_alive(target) {
            return (true, (0.0, 0.0));
        }
        let doc = self.dom().node_document(target);
        let screen = self.screen_of_document(doc);
        let at_screen = |q: Point| {
            let (x, y) = screen.to_screen(q.x as f64, q.y as f64);
            (x as f64, y as f64)
        };
        let from = self.pointer.borrow().move_screen;
        let to = at_screen(p);
        let movement = from.map_or((0.0, 0.0), |(x, y)| (to.0 - x, to.1 - y));
        self.pointer.borrow_mut().move_screen = Some(to);
        let pe = InterfaceId::PointerEvent;
        let e = self.pointer_event(
            pe,
            "pointermove",
            target,
            p,
            -1,
            buttons,
            mods,
            0,
            None,
            |u| (u.movement_x, u.movement_y) = movement,
        );
        let own = [(p, self.ev(e, |e| e.time_stamp))];
        let moves = if moves.is_empty() { &own[..] } else { moves };
        let mut last = from;
        let coalesced = moves
            .iter()
            .map(|&(at, time)| {
                let here = at_screen(at);
                let moved = last.map_or((0.0, 0.0), |(x, y)| (here.0 - x, here.1 - y));
                last = Some(here);
                let c = self.pointer_event(
                    pe,
                    "pointermove",
                    target,
                    at,
                    -1,
                    buttons,
                    mods,
                    0,
                    None,
                    |u| (u.movement_x, u.movement_y) = moved,
                );
                self.ev_mut(c, |c| {
                    c.bubbles = false;
                    c.cancelable = false;
                    c.time_stamp = time;
                    c.target = Some(node_handle(target));
                });
                c
            })
            .collect();
        self.ev_mut(e, |e| {
            if let EventExt::Ui(u) = &mut e.ext {
                u.coalesced = coalesced;
            }
        });
        (self.dispatch(cx, e, node_handle(target), false), movement)
    }

    /// The `cursor` and hyperlink at an element (`text`: at text in it).
    fn pointer_outcome(&self, el: Option<NodeId>, text: bool) -> PointerOutcome {
        // (listeners ran since the hit test: what it found may be gone)
        let Some(el) = el.filter(|&e| self.dom().is_alive(e)) else {
            return PointerOutcome {
                cursor: "default".into(),
                ..Default::default()
            };
        };
        let link = self.hyperlink_at(el);
        let cursor = self
            .with_style(el, |r| r.resolved_property(el, "cursor"))
            .flatten()
            .unwrap_or_else(|| "auto".into());
        let cursor = if cursor == "auto" {
            if link.is_some() {
                "pointer".into()
            } else if self.auto_cursor_is_text(el, text) {
                "text".into()
            } else {
                "default".into()
            }
        } else {
            cursor
        };
        PointerOutcome {
            cursor,
            link,
            title: self.advisory_information(el),
            ..Default::default()
        }
    }

    /// Whether a middle press at `t` is not for autoscroll: `t` is in a link, a form control or an
    /// editable element.
    fn blocks_autoscroll(&self, t: NodeId) -> bool {
        let dom = self.dom();
        self.chain(Some(t)).into_iter().any(|n| {
            dom.is_element(n)
                && (dom
                    .get_attribute(n, "contenteditable")
                    .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
                    || ["input", "textarea", "select", "button", "option"]
                        .iter()
                        .any(|name| dom.is_html_element_named(n, &LocalNameRef::from(*name))))
        })
    }

    /// HTML §3.2.6.1: an element's advisory information is its `title` attribute, else its nearest
    /// flat-tree ancestor's (an empty `title` means none), for HTML elements.
    pub(crate) fn advisory_information(&self, el: NodeId) -> Option<String> {
        let mut n = Some(el);
        while let Some(id) = n {
            {
                let dom = self.dom();
                if dom.is_element(id) && dom.namespace(id) == &snowlark_dom::ns!(html) {
                    if let Some(t) = dom.get_attribute(id, "title") {
                        return (!t.trim().is_empty()).then(|| t.to_owned());
                    }
                }
            }
            n = self.flat_parent(id);
        }
        None
    }

    /// The URL of the hyperlink `el` is in (HTML §4.6.4): the nearest `a` or `area` with an `href`.
    pub(crate) fn hyperlink_at(&self, el: NodeId) -> Option<url::Url> {
        let mut n = Some(el);
        while let Some(id) = n {
            let href = {
                let dom = self.dom();
                (dom.is_html_element_named(id, &local_name!("a"))
                    || dom.is_html_element_named(id, &local_name!("area")))
                .then(|| dom.get_attribute(id, "href").map(|h| h.trim().to_owned()))
                .flatten()
            };
            if let Some(href) = href {
                return self.base_url_of(id).join(&href).ok();
            }
            n = self.flat_parent(id);
        }
        None
    }

    /// CSS UI 4 §5.1.1: `auto` is `text` "over selectable text or editable elements". A selection
    /// being dragged keeps it, wherever the pointer goes.
    fn auto_cursor_is_text(&self, el: NodeId, text: bool) -> bool {
        self.pointer.borrow().doc_drag.is_some()
            || self.is_text_control(el)
            || self.is_editable(el)
            || (text && !self.in_form_control(el) && !self.user_select_none(el))
    }

    /// Whether `el` is a form control, whose text a press does not select.
    pub(crate) fn is_form_control(&self, el: NodeId) -> bool {
        let dom = self.dom();
        ["select", "button", "input", "textarea", "option"]
            .iter()
            .any(|n| dom.is_html_element_named(el, &LocalNameRef::from(*n)))
    }

    fn in_form_control(&self, el: NodeId) -> bool {
        let mut n = Some(el);
        while let Some(id) = n {
            if self.dom().is_element(id) && self.is_form_control(id) {
                return true;
            }
            n = self.flat_parent(id);
        }
        false
    }

    /// CSS UI 4 §6.1: the used value of `user-select` at `el` is `none`. `auto` is the parent's.
    /// An inert subtree's text is not selectable either (HTML §6.3).
    pub(crate) fn user_select_none(&self, el: NodeId) -> bool {
        if self.is_inert(el) {
            return true;
        }
        let mut n = Some(el);
        while let Some(id) = n {
            if self.dom().is_element(id) {
                let value = self
                    .with_style(id, |r| r.resolved_property(id, "user-select"))
                    .flatten();
                match value.as_deref() {
                    Some("none") => return true,
                    Some("auto") | None => {}
                    Some(_) => return false,
                }
            }
            n = self.flat_parent(id);
        }
        false
    }

    /// Whether `el` is a text field or a textarea (the text cursor, editing).
    pub(crate) fn is_text_control(&self, el: NodeId) -> bool {
        let dom = self.dom();
        if dom.is_html_element_named(el, &local_name!("textarea")) {
            return true;
        }
        if !dom.is_html_element_named(el, &local_name!("input")) {
            return false;
        }
        drop(dom);
        matches!(
            self.input_type(el).as_deref(),
            Some("text" | "search" | "url" | "tel" | "email" | "password" | "number") | None
        )
    }

    /// Selectors 4 §9.4: an element that takes keyboard input once focused (a text or date and time
    /// field, a textarea, an editing host), whose focus shows a ring however it came.
    pub(crate) fn takes_keyboard_input(&self, el: NodeId) -> bool {
        if self.is_text_control(el) {
            return true;
        }
        if matches!(
            self.input_type(el).as_deref(),
            Some("date" | "datetime-local" | "month" | "time" | "week")
        ) {
            return true;
        }
        self.editing_host(el) == Some(el)
    }

    /// Keyboard input at `target` while it is focused without a ring gives it one (Selectors 4 §9.4).
    pub(crate) fn show_focus_ring_for_keyboard(&self, target: NodeId) {
        let doc = self.dom().node_document(target);
        let Some(f) = self.focused_element(doc).filter(|&f| f == target) else {
            return;
        };
        if self.focus_ring.get() != Some(f) {
            self.set_element_state(f, ElementState::FOCUSRING, true);
            self.focus_ring.set(Some(f));
        }
    }

    /// The pointer moved to `p`, aligned to rendering opportunities: while they come from begin
    /// frames, the move is held and dispatched at the start of the next rendering update, merged
    /// with the moves that come before it, so a page sees one move, one hit test and one style
    /// update per frame however fast the mouse reports (UI Events leaves the rate of `mousemove` to
    /// the user agent; Pointer Events 3 calls the merged moves coalesced events). Any other input
    /// dispatches a held move first, so the order of events is kept. Without begin frames (the
    /// headless modes), or while the page has no rendering opportunities (hidden, render-blocked),
    /// the move is dispatched now and its outcome returned; a held move returns None, and its
    /// outcome is taken after the update ([`Engine::take_held_outcome`]).
    pub fn queue_pointer_move(
        &self,
        cx: &mut Cx,
        p: Point,
        mods: Modifiers,
    ) -> Option<PointerOutcome> {
        if !self.moves_aligned() {
            self.dispatch_held_move(cx);
            return Some(self.pointer_moved(cx, p, mods));
        }
        let now = self.now();
        let mut ps = self.pointer.borrow_mut();
        match &mut ps.held {
            Some(h) => {
                h.at = p;
                h.mods = mods;
                h.moves.push((p, now));
            }
            None => {
                ps.held = Some(HeldMove {
                    at: p,
                    mods,
                    since: now,
                    moves: vec![(p, now)],
                })
            }
        }
        None
    }

    /// Dispatches the held move, if there is one; its outcome waits for
    /// [`Engine::take_held_outcome`]. Returns whether there was one.
    pub fn dispatch_held_move(&self, cx: &mut Cx) -> bool {
        let Some(h) = self.pointer.borrow_mut().held.take() else {
            return false;
        };
        let o = self.pointer_moved_coalesced(cx, h.at, h.mods, &h.moves);
        self.pointer.borrow_mut().held_outcome = Some(o);
        true
    }

    /// Dispatches the held move before other pointer input, whose own outcome supersedes the
    /// move's.
    pub(crate) fn flush_held_move(&self, cx: &mut Cx) {
        if self.dispatch_held_move(cx) {
            self.pointer.borrow_mut().held_outcome = None;
        }
    }

    /// What the last held move found when it was dispatched (the cursor and link under it).
    pub fn take_held_outcome(&self) -> Option<PointerOutcome> {
        self.pointer.borrow_mut().held_outcome.take()
    }

    /// When a held move is dispatched without a rendering update (ms since the time origin).
    pub(crate) fn held_move_deadline(&self) -> Option<f64> {
        self.pointer
            .borrow()
            .held
            .as_ref()
            .map(|h| h.since + HELD_MOVE_FALLBACK_MS)
    }

    /// The pointer moved to `p` (viewport CSS px): over/out/enter/leave on a change of element,
    /// then `pointermove` and `mousemove`.
    pub fn pointer_moved(&self, cx: &mut Cx, p: Point, mods: Modifiers) -> PointerOutcome {
        self.pointer_moved_coalesced(cx, p, mods, &[])
    }

    /// [`Engine::pointer_moved`] for a move that stands for `moves` (where each went and when,
    /// oldest first; none: the move alone), its `pointermove`'s coalesced events.
    fn pointer_moved_coalesced(
        &self,
        cx: &mut Cx,
        p: Point,
        mods: Modifiers,
        moves: &[(Point, f64)],
    ) -> PointerOutcome {
        // (during a drag, the pointer events are suppressed: a move is an iteration of the
        // drag-and-drop processing model)
        if self.dragging() {
            self.drag_moved(cx, p, mods);
            return self.drag_outcome();
        }
        let may_drag = self.pointer.borrow().may_drag.is_some();
        let resizing = self.resizer_moved(cx, p);
        if self.pointer.borrow().buttons & 1 != 0 && !may_drag && !resizing {
            self.text_control_dragged(p);
            self.doc_pointer_dragged(p);
        }
        let top = p;
        let hit = self.hit_element(p);
        let p = hit.map_or(p, |h| h.point);
        let target = self.effective_target(cx, hit.map(|h| h.element), p, mods);
        let (old, buttons, removed) = {
            let ps = self.pointer.borrow();
            (ps.over, ps.buttons, ps.over_removed)
        };
        {
            let mut ps = self.pointer.borrow_mut();
            ps.last = p;
            ps.inside = true;
            ps.at = top;
        }
        if old != target || removed {
            self.pointer_transition(cx, old, target, p, mods, buttons);
        }
        if let Some(t) = target {
            // (the moves' points in the hit frame's viewport, as the move's own)
            let (dx, dy) = (p.x - top.x, p.y - top.y);
            let moves: Vec<(Point, f64)> = moves
                .iter()
                .map(|&(at, time)| (Point::new(at.x + dx, at.y + dy), time))
                .collect();
            let (_, movement) = self.fire_pointer_move(cx, t, p, buttons, mods, &moves);
            // Pointer Events 3 §11.2: the compatibility `mousemove` follows unless the PREVENT
            // MOUSE EVENT flag is set (canceling the `pointermove` itself does not suppress it).
            let prevented = {
                let mut ps = self.pointer.borrow_mut();
                // (a move with no button held: the press ended outside the page)
                if ps.buttons == 0 {
                    ps.prevent_mouse = false;
                }
                ps.prevent_mouse
            };
            if !prevented && self.dom().is_alive(t) {
                // (the move's compatibility mouse event moved as far as its pointermove)
                let e = self.pointer_event(
                    InterfaceId::MouseEvent,
                    "mousemove",
                    t,
                    p,
                    0,
                    buttons,
                    mods,
                    0,
                    None,
                    |u| (u.movement_x, u.movement_y) = movement,
                );
                self.dispatch(cx, e, node_handle(t), false);
            }
        }
        // A press on a draggable element that moved far enough starts a drag, after the move's
        // own events.
        if let Some(pending) = self.drag_threshold_passed(top) {
            if self.start_drag(cx, pending, top, mods) {
                self.microtask_checkpoint(cx);
                return self.drag_outcome();
            }
        }
        self.microtask_checkpoint(cx);
        let mut outcome = self.pointer_outcome(target, hit.is_some_and(|h| h.text));
        if let Some(c) = self.resizer_cursor(hit.map(|h| h.element), p) {
            outcome.cursor = c.into();
        }
        outcome
    }

    /// What pointer input finds during a drag: the default cursor (the platform's drag feedback
    /// is the embedder's), no link or tooltip.
    fn drag_outcome(&self) -> PointerOutcome {
        PointerOutcome {
            cursor: "default".into(),
            ..Default::default()
        }
    }

    /// The pointer left `old` for `new`: the out/leave and over/enter events and the `:hover`
    /// state.
    fn pointer_transition(
        &self,
        cx: &mut Cx,
        old: Option<NodeId>,
        new: Option<NodeId>,
        p: Point,
        mods: Modifiers,
        buttons: u16,
    ) {
        let (old_chain, new_chain) = (self.chain_across(old), self.chain_across(new));
        // (`old` is the parent that remained of a removed element under the pointer: it was never
        // "over" itself, so it gets no `out`; what it is inside was entered already)
        let removed = std::mem::take(&mut self.pointer.borrow_mut().over_removed);
        let pe = InterfaceId::PointerEvent;
        let me = InterfaceId::MouseEvent;
        // Pointer Events 3 §11.3: the pointer events first, then their compatibility mouse events
        // (`pointerout`, `pointerleave`s, `mouseout`, `mouseleave`s; `pointerover`, `pointerenter`s,
        // `mouseover`, `mouseenter`s).
        if let Some(o) = old_chain.first().copied() {
            let left: Vec<NodeId> = old_chain
                .iter()
                .copied()
                .filter(|n| !new_chain.contains(n))
                .collect();
            if !removed {
                self.fire_pointer(cx, pe, "pointerout", o, p, -1, buttons, mods, 0, new);
            }
            for &l in &left {
                self.fire_pointer(cx, pe, "pointerleave", l, p, -1, buttons, mods, 0, new);
            }
            if !removed {
                self.fire_pointer(cx, me, "mouseout", o, p, 0, buttons, mods, 0, new);
            }
            for &l in &left {
                self.fire_pointer(cx, me, "mouseleave", l, p, 0, buttons, mods, 0, new);
            }
        }
        if let Some(n) = new_chain.first().copied() {
            let entered: Vec<NodeId> = new_chain
                .iter()
                .rev()
                .copied()
                .filter(|n| !old_chain.contains(n))
                .collect();
            self.fire_pointer(cx, pe, "pointerover", n, p, -1, buttons, mods, 0, old);
            for &e in &entered {
                self.fire_pointer(cx, pe, "pointerenter", e, p, -1, buttons, mods, 0, old);
            }
            self.fire_pointer(cx, me, "mouseover", n, p, 0, buttons, mods, 0, old);
            for &e in &entered {
                self.fire_pointer(cx, me, "mouseenter", e, p, 0, buttons, mods, 0, old);
            }
        }
        // `:hover` follows the element under the pointer and its ancestors.
        for &o in old_chain.iter().filter(|n| !new_chain.contains(n)) {
            self.set_element_state(o, ElementState::HOVER, false);
        }
        for &n in new_chain.iter().filter(|n| !old_chain.contains(n)) {
            self.set_element_state(n, ElementState::HOVER, true);
        }
        let mut ps = self.pointer.borrow_mut();
        ps.over = new;
        ps.hovered = new_chain;
    }

    /// Pointer Events 3 "setting pointer capture": only while a button is down (the mouse is
    /// pointer 1, always active).
    pub(crate) fn set_pointer_capture_target(&self, el: NodeId) {
        let doc = self.dom().node_document(el);
        let mut ps = self.pointer.borrow_mut();
        // (a pointer active in one frame's document is not captured by another frame's element)
        if ps.buttons != 0 && ps.capture_doc == Some(doc) {
            ps.pending_capture = Some(el);
        }
    }

    /// Pointer Events 3 "releasing pointer capture" for `el`.
    pub(crate) fn release_pointer_capture_target(&self, el: NodeId) {
        let mut ps = self.pointer.borrow_mut();
        if ps.pending_capture == Some(el) {
            ps.pending_capture = None;
        }
    }

    /// `hasPointerCapture`: `el` is the pending pointer capture target override.
    pub(crate) fn has_pointer_capture_target(&self, el: NodeId) -> bool {
        self.pointer.borrow().pending_capture == Some(el)
    }

    /// Pointer Events 3 "process pending pointer capture", before a pointer event: a changed
    /// override fires `lostpointercapture` at the old one (at its document when it left it), then
    /// moves the element under the pointer to the new one and fires `gotpointercapture` at it.
    fn process_pending_capture(&self, cx: &mut Cx, p: Point, mods: Modifiers) {
        let (current, pending, buttons) = {
            let ps = self.pointer.borrow();
            (ps.capture, ps.pending_capture, ps.buttons)
        };
        if current == pending {
            return;
        }
        let pe = InterfaceId::PointerEvent;
        if let Some(c) = current {
            self.pointer.borrow_mut().capture = None;
            if self.dom().is_alive(c) {
                let at = if self.dom().is_connected(c) {
                    c
                } else {
                    self.dom().node_document(c)
                };
                self.fire_pointer(
                    cx,
                    pe,
                    "lostpointercapture",
                    at,
                    p,
                    -1,
                    buttons,
                    mods,
                    0,
                    None,
                );
            }
        }
        let Some(n) = pending.filter(|&n| self.dom().is_alive(n) && self.dom().is_connected(n))
        else {
            return;
        };
        let (old, removed) = {
            let ps = self.pointer.borrow();
            (ps.over, ps.over_removed)
        };
        if old != Some(n) || removed {
            self.pointer_transition(cx, old, Some(n), p, mods, buttons);
        }
        self.fire_pointer(
            cx,
            pe,
            "gotpointercapture",
            n,
            p,
            -1,
            buttons,
            mods,
            0,
            None,
        );
        if self.dom().is_connected(n) {
            self.pointer.borrow_mut().capture = Some(n);
        } else {
            let doc = self.dom().node_document(n);
            self.fire_pointer(
                cx,
                pe,
                "lostpointercapture",
                doc,
                p,
                -1,
                buttons,
                mods,
                0,
                None,
            );
        }
    }

    /// The target of a pointer event at the element `hit`: the pointer capture target override,
    /// after processing the pending one, else `hit`.
    fn effective_target(
        &self,
        cx: &mut Cx,
        hit: Option<NodeId>,
        p: Point,
        mods: Modifiers,
    ) -> Option<NodeId> {
        self.process_pending_capture(cx, p, mods);
        self.pointer.borrow().capture.or(hit)
    }

    /// A button press or release updates the element under the pointer first (its boundary events
    /// come before `pointerdown`/`pointerup`).
    fn boundary_before_button(
        &self,
        cx: &mut Cx,
        target: Option<NodeId>,
        top: Point,
        p: Point,
        mods: Modifiers,
    ) {
        let (old, buttons, removed) = {
            let mut ps = self.pointer.borrow_mut();
            ps.inside = true;
            ps.at = top;
            (ps.over, ps.buttons, ps.over_removed)
        };
        if old != target || removed {
            self.pointer_transition(cx, old, target, p, mods, buttons);
        }
    }

    /// A node left its parent (`old_parent`): when the element under the pointer went with it, the
    /// parent element that remained stands in for it until the next boundary update, and the
    /// removed elements leave `:hover`.
    pub(crate) fn pointer_node_removed(&self, node: NodeId, old_parent: NodeId) {
        // (a pending pointer capture target that left goes: the next pointer event processes it)
        let pending = self.pointer.borrow().pending_capture;
        if let Some(pc) = pending {
            let dom = self.dom();
            let mut cur = Some(pc);
            while let Some(c) = cur {
                if c == node {
                    drop(dom);
                    self.pointer.borrow_mut().pending_capture = None;
                    break;
                }
                cur = match dom.parent(c) {
                    Some(p) if dom.is_shadow_root(p) => dom.host(p),
                    p => p,
                };
            }
        }
        let (over, hovered) = {
            let ps = self.pointer.borrow();
            (ps.over, ps.hovered.clone())
        };
        let Some(over) = over else {
            return;
        };
        let inside = |n: NodeId| {
            let dom = self.dom();
            let mut cur = Some(n);
            while let Some(c) = cur {
                if c == node {
                    return true;
                }
                cur = match dom.parent(c) {
                    Some(p) if dom.is_shadow_root(p) => dom.host(p),
                    p => p,
                };
            }
            false
        };
        if !inside(over) {
            return;
        }
        let remaining = {
            let dom = self.dom();
            if dom.is_element(old_parent) {
                Some(old_parent)
            } else if dom.is_shadow_root(old_parent) {
                dom.host(old_parent)
            } else {
                None
            }
        };
        let gone: Vec<NodeId> = hovered.iter().copied().filter(|&n| inside(n)).collect();
        for &n in &gone {
            self.set_element_state(n, ElementState::HOVER, false);
        }
        let mut ps = self.pointer.borrow_mut();
        ps.over = remaining;
        ps.over_removed = true;
        ps.hovered.retain(|n| !gone.contains(n));
        // (the next rendering update looks again at what is under the pointer)
        ps.hover_layout = None;
    }

    /// The layout and scroll position of the top-level document, which decide what is under a
    /// still pointer.
    fn hover_layout(&self) -> Option<(u64, Point)> {
        let st = self.render.borrow();
        let s = st.renderer.as_ref()?.stats();
        Some((
            s.laid_out + s.full_builds + s.incremental_updates,
            st.scroll,
        ))
    }

    /// At a rendering update: when the layout or scroll changed (or the element under the pointer
    /// left the document) since the hover state was computed, what is under a still pointer is hit
    /// tested again and the boundary events of a change are dispatched, without a move event.
    pub(crate) fn recompute_hover(&self, cx: &mut Cx) {
        let (inside, at, removed, seen, buttons, old) = {
            let ps = self.pointer.borrow();
            (
                ps.inside,
                ps.at,
                ps.over_removed,
                ps.hover_layout,
                ps.buttons,
                ps.over,
            )
        };
        if !inside {
            return;
        }
        let now = self.hover_layout();
        if !removed && seen.is_some() && seen == now {
            return;
        }
        let hit = self.hit_element(at);
        let target = hit.map(|h| h.element);
        let p = hit.map_or(at, |h| h.point);
        self.pointer.borrow_mut().hover_layout = self.hover_layout();
        if old != target || removed {
            self.pointer_transition(cx, old, target, p, Modifiers::default(), buttons);
            self.microtask_checkpoint(cx);
        }
    }

    /// The pointer left the page.
    pub fn pointer_left(&self, cx: &mut Cx) {
        let (old, buttons, last) = {
            let mut ps = self.pointer.borrow_mut();
            ps.inside = false;
            ps.move_screen = None;
            (ps.over, ps.buttons, ps.last)
        };
        if old.is_some() {
            self.pointer_transition(cx, old, None, last, Modifiers::default(), buttons);
        }
        self.microtask_checkpoint(cx);
    }

    /// A button went down (`button` 0 primary, 1 middle, 2 secondary, 3 back, 4 forward; `clicks` 2 for
    /// the second press of a double-click).
    pub fn pointer_pressed(
        &self,
        cx: &mut Cx,
        p: Point,
        button: u16,
        mods: Modifiers,
        clicks: u32,
    ) -> PointerOutcome {
        // (a press comes after the drag loop of a system drag: one whose end never came is over)
        self.end_stale_system_drag(cx);
        let top_point = p;
        let hit = self.hit_element(p);
        let target = hit.map(|h| h.element);
        let content = hit.and_then(|h| h.content);
        let p = hit.map_or(p, |h| h.point);
        let target = self.effective_target(cx, target, p, mods);
        self.boundary_before_button(cx, target, top_point, p, mods);
        let pressed_doc = hit.map(|h| self.dom().node_document(h.element));
        let buttons = {
            let mut ps = self.pointer.borrow_mut();
            ps.capture_doc = pressed_doc;
            ps.buttons |= button_bit(button);
            ps.last = p;
            ps.buttons
        };
        self.pointer.borrow_mut().may_drag = None;
        let Some(t) = target else {
            return self.pointer_outcome(None, false);
        };
        // HTML §6.4.3: a pointer press is an activation-triggering input event.
        self.note_user_activation(t);
        let go = self.fire_pointer(
            cx,
            InterfaceId::PointerEvent,
            "pointerdown",
            t,
            p,
            button as i16,
            buttons,
            mods,
            0,
            None,
        );
        // Pointer Events 3 §11.2: a canceled `pointerdown` sets the PREVENT MOUSE EVENT flag, and
        // while it is set no compatibility `mousedown` is fired. The press's default actions
        // (focus, starting a selection or a drag) are those of an uncanceled press: canceling
        // either event suppresses them.
        let prevented = {
            let mut ps = self.pointer.borrow_mut();
            if !go {
                ps.prevent_mouse = true;
            }
            ps.prevent_mouse
        };
        let mut default = go;
        if !prevented {
            default = self.fire_pointer(
                cx,
                InterfaceId::MouseEvent,
                "mousedown",
                t,
                p,
                button as i16,
                buttons,
                mods,
                clicks as i32,
                None,
            );
        }
        // Default actions of an uncanceled primary press: focus, `:active`. (A listener may have
        // taken the target out of the document: there is nothing left to act on.)
        let here = {
            let dom = self.dom();
            dom.is_alive(t) && dom.is_connected(t)
        };
        if default && button == 0 && here {
            self.focus_by_click(cx, t);
            let active = self.chain_across(Some(t));
            for &a in &active {
                self.set_element_state(a, ElementState::ACTIVE, true);
            }
            self.pointer.borrow_mut().active = active;
            // A press on a resizer starts a resize, and nothing else (CSS UI 4 §5.1).
            let resizing = self.resizer_pressed(t, p, top_point);
            // A focused text control: the caret goes where the press is; elsewhere the document
            // selection does.
            let doc = self.dom().node_document(t);
            let control = self.is_form_control(t);
            if resizing {
                self.pointer.borrow_mut().doc_drag = None;
            } else if self.is_text_control(t) && self.focused_element(doc) == Some(t) {
                self.text_control_pressed(t, top_point, clicks, mods);
            } else if !control {
                self.doc_pointer_pressed(cx, t, p, clicks, mods);
            }
            // HTML §6.11.5: a press on a draggable element may begin a drag (and selects
            // nothing while it may). What is under the press now counts, after the `mousedown`
            // listeners moved things, as the engines hit test the press point again.
            let candidate = self
                .hit_element(top_point)
                .and_then(|h| self.drag_candidate(h.element))
                .or_else(|| {
                    self.dom()
                        .is_alive(t)
                        .then(|| self.drag_candidate(t))
                        .flatten()
                })
                .filter(|_| !resizing);
            {
                if let Some((source, kind)) = candidate {
                    let mut ps = self.pointer.borrow_mut();
                    ps.doc_drag = None;
                    ps.may_drag = Some(crate::dnd::PendingDrag {
                        source,
                        kind,
                        at: top_point,
                        client: p,
                    });
                }
            }
            // A select's list: the drop-down opens; a list box selects the option pressed.
            let select = self.dom().is_html_element_named(t, &local_name!("select"));
            if select && !resizing {
                if crate::popup::is_drop_down(&self.dom(), t) {
                    self.open_select_popup(t);
                } else if let Some(c) = content {
                    self.list_box_press(cx, t, c.y, mods);
                }
            }
        }
        self.pointer.borrow_mut().pressed = Some((t, button));
        self.microtask_checkpoint(cx);
        let mut outcome = self.pointer_outcome(Some(t), hit.is_some_and(|h| h.text));
        outcome.press_canceled = !default;
        outcome.autoscroll_blocked = outcome.link.is_some() || self.blocks_autoscroll(t);
        outcome
    }

    /// HTML §6.6.3: a press focuses the nearest focusable inclusive ancestor, or unfocuses.
    fn focus_by_click(&self, cx: &mut Cx, t: NodeId) {
        self.composition_interrupted(cx);
        // (a shadow host that delegates focus stops the walk: its focusable area takes the focus,
        // unless it already has it)
        let target = self
            .chain(Some(t))
            .into_iter()
            .find(|&n| self.is_focusable(n) || crate::engine::html::delegates_focus(&self.dom(), n))
            .and_then(|n| {
                if self.is_focusable(n) {
                    Some(n)
                } else {
                    self.focusable_area_for(n)
                }
            });
        let doc = self.dom().node_document(t);
        self.focus_from_keyboard.set(false);
        // (HTML §6.6.5: a click sets the sequential focus navigation starting point; a focus it
        // causes sets it again, to the element focused)
        self.doc_state(doc, |s| s.nav_start = Some(t));
        match target {
            Some(f) => self.focus_element(cx, f),
            None => {
                if let Some(f) = self.focused_element(doc) {
                    self.blur_element(cx, f);
                }
            }
        }
        self.focus_frame_chain(cx, doc);
    }

    /// The navigable containers part of HTML §6.6.4's focus update steps for a focus moving into
    /// `doc`: each frame from `doc` up becomes its parent's focused area (the focus chain runs
    /// through the navigable containers), what the parents had focused loses the focus, and so
    /// do the focused elements of the other frames in the same top-level document (another
    /// top-level document, a popup's, keeps its focused area).
    pub(crate) fn focus_frame_chain(&self, cx: &mut Cx, doc: NodeId) {
        let mut chain_docs = vec![doc];
        let mut doc = doc;
        while let Some(c) = self.rendered_container(doc) {
            let parent = self.dom().node_document(c);
            if let Some(old) = self.focused_element(parent).filter(|&o| o != c) {
                self.blur_element(cx, old);
            }
            self.doc_state(parent, |s| s.focused = Some(c));
            chain_docs.push(parent);
            doc = parent;
        }
        let top = doc;
        let top_of = |mut d: NodeId| {
            while let Some(c) = self.rendered_container(d) {
                d = self.dom().node_document(c);
            }
            d
        };
        let others: Vec<NodeId> = self
            .frame_renders
            .borrow()
            .keys()
            .copied()
            .filter(|d| !chain_docs.contains(d))
            .collect();
        let others: Vec<NodeId> = others.into_iter().filter(|&d| top_of(d) == top).collect();
        for d in others {
            if let Some(f) = self.focused_element(d) {
                if !self.rendered_frames_of(d).iter().any(|(c, _)| *c == f) {
                    self.blur_element(cx, f);
                }
            }
        }
    }

    /// A button went up: `pointerup`, `mouseup`, then `click` (or `auxclick`, `contextmenu`) at the
    /// nearest common ancestor of the press and release targets, `dblclick` on a double-click.
    pub fn pointer_released(
        &self,
        cx: &mut Cx,
        p: Point,
        button: u16,
        mods: Modifiers,
        clicks: u32,
    ) -> PointerOutcome {
        let top_point = p;
        let hit = self.hit_element(p);
        let target = hit.map(|h| h.element);
        let p = hit.map_or(p, |h| h.point);
        if self.dragging() {
            self.pointer.borrow_mut().buttons &= !button_bit(button);
            if button == 0 {
                self.drag_released(cx, top_point, mods);
            }
            return self.drag_outcome();
        }
        self.pointer.borrow_mut().may_drag = None;
        let target = self.effective_target(cx, target, p, mods);
        self.boundary_before_button(cx, target, top_point, p, mods);
        let buttons = {
            let mut ps = self.pointer.borrow_mut();
            ps.buttons &= !button_bit(button);
            ps.last = p;
            ps.buttons
        };
        let pressed = self.pointer.borrow_mut().pressed.take();
        if button == 0 {
            self.text_drag_ended();
            self.doc_drag_ended();
            self.resizer_released();
        }
        for a in std::mem::take(&mut self.pointer.borrow_mut().active) {
            self.set_element_state(a, ElementState::ACTIVE, false);
        }
        let Some(t) = target else {
            return self.pointer_outcome(None, false);
        };
        self.fire_pointer(
            cx,
            InterfaceId::PointerEvent,
            "pointerup",
            t,
            p,
            button as i16,
            buttons,
            mods,
            0,
            None,
        );
        // Pointer Events 3 §11.2: the compatibility `mouseup` follows unless the PREVENT MOUSE
        // EVENT flag is set (canceling the `pointerup` does not suppress it); the `pointerup` of the
        // last button clears the flag.
        let prevented = {
            let mut ps = self.pointer.borrow_mut();
            let prevented = ps.prevent_mouse;
            if buttons == 0 {
                ps.prevent_mouse = false;
            }
            prevented
        };
        if !prevented {
            self.fire_pointer(
                cx,
                InterfaceId::MouseEvent,
                "mouseup",
                t,
                p,
                button as i16,
                buttons,
                mods,
                clicks as i32,
                None,
            );
        }
        // Pointer Events 3 "implicit release of pointer capture" after `pointerup` (and its
        // `mouseup`): `lostpointercapture` before the click, which goes to the capture target.
        let captured = self
            .pointer
            .borrow()
            .capture
            .filter(|&c| self.dom().is_alive(c));
        self.pointer.borrow_mut().pending_capture = None;
        self.process_pending_capture(cx, p, mods);
        // The click target: the nearest common inclusive ancestor of the press and the release.
        let click_target = pressed.filter(|(_, b)| *b == button).and_then(|(down, _)| {
            if captured.is_some() {
                return captured;
            }
            if !self.dom().is_alive(down) {
                return None;
            }
            let down_chain = self.chain_across(Some(down));
            self.chain_across(Some(t))
                .into_iter()
                .find(|n| down_chain.contains(n))
        });
        if let Some(c) = click_target {
            let detail = clicks.max(1) as i32;
            match button {
                0 => {
                    let clicked = self.fire_pointer(
                        cx,
                        InterfaceId::PointerEvent,
                        "click",
                        c,
                        p,
                        0,
                        buttons,
                        mods,
                        detail,
                        None,
                    );
                    // (an image document zooms its image on any click on it)
                    if self.dom().is_alive(c) {
                        self.image_document_clicked(cx, c, p.x as f64, p.y as f64);
                    }
                    // A media element's user agent controls act on a click nothing canceled.
                    if clicked && c == t && self.dom().is_alive(c) {
                        if let Some(at) = hit.and_then(|h| h.content) {
                            self.media_controls_click(cx, c, at);
                        }
                    }
                    if clicks == 2 {
                        self.fire_pointer(
                            cx,
                            InterfaceId::MouseEvent,
                            "dblclick",
                            c,
                            p,
                            0,
                            buttons,
                            mods,
                            2,
                            None,
                        );
                    }
                }
                1 => {
                    let go = self.fire_pointer(
                        cx,
                        InterfaceId::PointerEvent,
                        "auxclick",
                        c,
                        p,
                        1,
                        buttons,
                        mods,
                        detail,
                        None,
                    );
                    // A middle click on a link opens it in a new background tab.
                    if go {
                        if let Some(url) = self.hyperlink_at(c) {
                            self.open_requests.borrow_mut().push((url, true));
                        }
                    }
                }
                2 => {
                    self.fire_pointer(
                        cx,
                        InterfaceId::PointerEvent,
                        "auxclick",
                        c,
                        p,
                        2,
                        buttons,
                        mods,
                        detail,
                        None,
                    );
                    let shown = self.fire_pointer(
                        cx,
                        InterfaceId::PointerEvent,
                        "contextmenu",
                        c,
                        p,
                        2,
                        buttons,
                        mods,
                        0,
                        None,
                    );
                    // (not canceled: the embedder shows its context menu for `c`)
                    if shown {
                        self.embedder.borrow_mut().context_menu = Some(c);
                    }
                }
                _ => {}
            }
        }
        // (released: the element under the pointer is what is there, not the capture target)
        if captured.is_some() {
            let under = hit.map(|h| h.element);
            let (old, removed) = {
                let ps = self.pointer.borrow();
                (ps.over, ps.over_removed)
            };
            if old != under || removed {
                self.pointer_transition(cx, old, under, p, mods, buttons);
            }
        }
        self.microtask_checkpoint(cx);
        self.pointer_outcome(Some(t), hit.is_some_and(|h| h.text))
    }

    /// A wheel turn at `p` (`delta` in CSS px): a `wheel` event at the element under the pointer.
    /// The embedder scrolls without waiting for it (the event is not cancelable in effect, as a
    /// passive listener's is not).
    pub fn wheel(&self, cx: &mut Cx, p: Point, delta: Point, mods: Modifiers) {
        let Some(hit) = self.hit_element(p) else {
            return;
        };
        let (t, p) = (hit.element, hit.point);
        let e = self.pointer_event(
            InterfaceId::WheelEvent,
            "wheel",
            t,
            p,
            0,
            self.pointer.borrow().buttons,
            mods,
            0,
            None,
            |u| {
                u.delta_x = delta.x as f64;
                u.delta_y = delta.y as f64;
                u.delta_mode = 0;
                // Chromium's native wheel events: 120 per wheel notch (100 px here), positive away
                // from the user, as `wheel_ticks * 120` truncated.
                u.wheel_delta = (
                    (-delta.x as f64 * 1.2) as i32,
                    (-delta.y as f64 * 1.2) as i32,
                );
            },
        );
        self.dispatch(cx, e, node_handle(t), false);
        self.microtask_checkpoint(cx);
    }

    /// HTML §4.16.3 `:focus`: `el` is its document's focused element, or a shadow host whose shadow
    /// root is the root of an element that has the focus.
    pub(crate) fn has_focus(&self, dom: &snowlark_dom::Dom, el: NodeId) -> bool {
        let Some(mut f) = self.focused_element(dom.node_document(el)) else {
            return false;
        };
        loop {
            if f == el {
                return true;
            }
            if !dom.is_alive(f) || !dom.is_shadow_root(dom.root(f)) {
                return false;
            }
            f = dom.host(dom.root(f)).expect("a shadow root has a host");
        }
    }

    /// The shadow hosts of the shadow trees `el` is in, innermost first.
    fn focus_hosts(&self, el: NodeId) -> Vec<NodeId> {
        let dom = self.dom();
        let mut out = Vec::new();
        let mut n = el;
        while dom.is_alive(n) && dom.is_shadow_root(dom.root(n)) {
            n = dom.host(dom.root(n)).expect("a shadow root has a host");
            out.push(n);
        }
        out
    }

    /// The style engine's focus states after focus moved from `old` to `new` (HTML §6.6.4): `:focus`
    /// on the element, `:focus-visible` when it came from the keyboard or is a text field,
    /// `:focus-within` on its flat-tree ancestors.
    pub(crate) fn focus_states_changed(&self, old: Option<NodeId>, new: Option<NodeId>) {
        let old = old.filter(|&o| self.dom().is_alive(o));
        let (old_chain, new_chain) = (self.chain(old), self.chain(new));
        if let Some(o) = old_chain.first() {
            self.set_element_state(*o, ElementState::FOCUS | ElementState::FOCUSRING, false);
            for h in self.focus_hosts(*o) {
                self.set_element_state(h, ElementState::FOCUS, false);
            }
        }
        // (HTML §4.16.3 `:focus`: a shadow host has the focus when its shadow root is the root of
        // an element that has it)
        if let Some(&n) = new_chain.first() {
            for h in self.focus_hosts(n) {
                self.set_element_state(h, ElementState::FOCUS, true);
            }
        }
        for &o in old_chain.iter().filter(|n| !new_chain.contains(n)) {
            self.set_element_state(o, ElementState::FOCUS_WITHIN, false);
        }
        self.focus_ring.set(None);
        if let Some(&n) = new_chain.first() {
            self.set_element_state(n, ElementState::FOCUS, true);
            let visible = self
                .focus_visible_option
                .get()
                .unwrap_or_else(|| self.focus_from_keyboard.get() || self.takes_keyboard_input(n));
            if visible {
                self.set_element_state(n, ElementState::FOCUSRING, true);
                self.focus_ring.set(Some(n));
            }
        }
        for &n in &new_chain {
            self.set_element_state(n, ElementState::FOCUS_WITHIN, true);
        }
    }
}

/// Where a button press is: `MouseEvent.button` values.
pub mod buttons {
    pub const PRIMARY: u16 = 0;
    pub const AUXILIARY: u16 = 1;
    pub const SECONDARY: u16 = 2;
    pub const BACK: u16 = 3;
    pub const FORWARD: u16 = 4;
}

impl Page {
    /// The pointer moved over the page (viewport CSS px), dispatched now.
    pub fn pointer_moved(&mut self, p: Point, mods: Modifiers) -> PointerOutcome {
        self.with(|e, cx| {
            e.flush_held_move(cx);
            e.pointer_moved(cx, p, mods)
        })
    }

    /// The pointer moved over the page, aligned to rendering updates
    /// ([`Engine::queue_pointer_move`]): None while the move is held.
    pub fn queue_pointer_move(&mut self, p: Point, mods: Modifiers) -> Option<PointerOutcome> {
        self.with(|e, cx| e.queue_pointer_move(cx, p, mods))
    }

    /// What the last held move found when it was dispatched ([`Engine::take_held_outcome`]).
    pub fn take_held_outcome(&mut self) -> Option<PointerOutcome> {
        self.engine().take_held_outcome()
    }

    pub fn pointer_left(&mut self) {
        self.with(|e, cx| {
            e.flush_held_move(cx);
            e.pointer_left(cx)
        })
    }

    pub fn pointer_pressed(
        &mut self,
        p: Point,
        button: u16,
        mods: Modifiers,
        clicks: u32,
    ) -> PointerOutcome {
        self.with(|e, cx| {
            e.flush_held_move(cx);
            e.pointer_pressed(cx, p, button, mods, clicks)
        })
    }

    pub fn pointer_released(
        &mut self,
        p: Point,
        button: u16,
        mods: Modifiers,
        clicks: u32,
    ) -> PointerOutcome {
        self.with(|e, cx| {
            e.flush_held_move(cx);
            e.pointer_released(cx, p, button, mods, clicks)
        })
    }

    pub fn wheel(&mut self, p: Point, delta: Point, mods: Modifiers) {
        self.with(|e, cx| {
            e.flush_held_move(cx);
            e.wheel(cx, p, delta, mods)
        })
    }
}
