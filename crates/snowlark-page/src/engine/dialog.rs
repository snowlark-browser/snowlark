// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.11.4 the `dialog` element and the top layer (CSS Position 4 §4.1, HTML "top layer"
//! operations): `show()`, `showModal()`, `close()`, `requestClose()`, the dialog focusing steps,
//! `beforetoggle`/`toggle` events, `:modal`, inertness (HTML §6.3: the `inert` attribute, and a
//! document "blocked by a modal dialog" outside the dialog; inert nodes are not focusable), and Esc
//! as a close request for the topmost modal dialog.
//!
//! The top layer of each document is kept here and handed to its renderer, which builds the top
//! layer's boxes (each over its `::backdrop`) as the root's last children and paints them above
//! everything else.
//!
//! Each document's open dialogs list, each open dialog's close watcher (`close_watcher.rs`: Esc
//! and `requestClose()`), the `closedby` attribute and its computed state, and light dismiss of the
//! topmost open dialog on a trusted `pointerdown` and `pointerup` outside it.
//!
//! Not implemented: pending top layer removals (`overlay` transitions). Hit testing and selection
//! see inertness through the style engine (`update_blocked_marks` and the UA sheet's inert rules).

use snowlark_bindings::generated::HTMLDialogElementImpl;
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, ns, NodeId};
use snowlark_style::ElementState;

use super::close_watcher::CloseWatcher;
use super::events::EventExt;
use super::{exception, node_handle, Engine};
use crate::event_loop::TaskSource;

/// Dialog and top layer state the DOM does not keep.
#[derive(Default)]
pub struct DialogState {
    /// Each document's top layer, bottom first.
    pub top_layers: rustc_hash::FxHashMap<NodeId, Vec<NodeId>>,
    /// Dialogs whose "is modal" flag is set.
    pub modal: Vec<NodeId>,
    /// Each dialog's "previously focused element".
    pub previously_focused: rustc_hash::FxHashMap<NodeId, NodeId>,
    /// Each dialog's "dialog toggle task tracker": the pending toggle task's generation and the old
    /// state it fires with.
    pub toggle_tracker: rustc_hash::FxHashMap<NodeId, (u64, &'static str)>,
    pub toggle_generation: u64,
    /// Each document's "open dialogs list", oldest first.
    pub open_dialogs: rustc_hash::FxHashMap<NodeId, Vec<NodeId>>,
    /// Each document's "dialog pointerdown target".
    pub pointerdown_target: rustc_hash::FxHashMap<NodeId, Option<NodeId>>,
    /// Dialogs whose "enable close watcher for request close" is true.
    pub enable_for_request_close: rustc_hash::FxHashSet<NodeId>,
    /// Each dialog's "request close return value" and "request close source element".
    pub request_close: rustc_hash::FxHashMap<NodeId, (Option<String>, Option<NodeId>)>,
    /// Each blocked document's root element and blocking dialog, as marked for the style engine
    /// (`update_blocked_marks`).
    pub blocked_marks: rustc_hash::FxHashMap<NodeId, (Option<NodeId>, Option<NodeId>)>,
}

impl Engine {
    fn is_open(&self, el: NodeId) -> bool {
        self.dom().attr(el, &ns!(), &local_name!("open")).is_some()
    }

    fn is_modal(&self, el: NodeId) -> bool {
        self.dialogs.borrow().modal.contains(&el)
    }

    /// HTML "retrieve a dialog's computed closed-by state": the `closedby` attribute's state; in
    /// the Auto state (missing or invalid), Close Request for a modal dialog and None otherwise.
    fn computed_closed_by(&self, el: NodeId) -> ClosedBy {
        let v = self
            .attr_str(el, "closedby")
            .map(|v| v.to_ascii_lowercase());
        match v.as_deref() {
            Some("any") => ClosedBy::Any,
            Some("closerequest") => ClosedBy::CloseRequest,
            Some("none") => ClosedBy::None,
            _ if self.is_modal(el) => ClosedBy::CloseRequest,
            _ => ClosedBy::None,
        }
    }

    /// The dialog close watcher's "get enabled state".
    pub(crate) fn dialog_close_watcher_enabled(&self, el: NodeId) -> bool {
        self.dialogs.borrow().enable_for_request_close.contains(&el)
            || self.computed_closed_by(el) != ClosedBy::None
    }

    /// The dialog close watcher's cancel action: a `cancel` event, cancelable when the close can
    /// be prevented. Returns whether it was not canceled.
    pub(crate) fn dialog_cancel_action(
        &self,
        cx: &mut Cx,
        el: NodeId,
        can_prevent_close: bool,
    ) -> bool {
        self.fire_event(cx, node_handle(el), "cancel", false, can_prevent_close)
    }

    /// The dialog close watcher's close action: close the dialog with its request close return
    /// value and source element.
    pub(crate) fn dialog_close_action(&self, cx: &mut Cx, el: NodeId) {
        let (value, source) = self
            .dialogs
            .borrow()
            .request_close
            .get(&el)
            .cloned()
            .unwrap_or_default();
        self.close_the_dialog(cx, el, value.as_deref(), source);
    }

    /// HTML "dialog setup steps": the open dialog joins its document's open dialogs list and gets
    /// a close watcher ("set the dialog close watcher").
    fn dialog_setup_steps(&self, el: NodeId) {
        let doc = self.dom().node_document(el);
        {
            let mut d = self.dialogs.borrow_mut();
            let list = d.open_dialogs.entry(doc).or_default();
            if list.contains(&el) {
                return;
            }
            list.push(el);
        }
        self.establish_close_watcher(doc, CloseWatcher::Dialog(el));
    }

    /// HTML "dialog cleanup steps": out of the open dialogs list, its close watcher destroyed.
    fn dialog_cleanup_steps(&self, el: NodeId) {
        {
            let mut d = self.dialogs.borrow_mut();
            for list in d.open_dialogs.values_mut() {
                list.retain(|&e| e != el);
            }
            d.open_dialogs.retain(|_, l| !l.is_empty());
        }
        self.destroy_close_watcher(CloseWatcher::Dialog(el));
    }

    /// The dialog attribute change steps for `open` (`added`: it was added, else removed).
    pub(crate) fn dialog_open_changed(&self, el: NodeId, added: bool) {
        if !added {
            self.dialog_cleanup_steps(el);
            self.dialog_open_removed(el);
            return;
        }
        let doc = self.dom().node_document(el);
        if self.is_fully_active(doc) && self.dom().is_connected(el) {
            self.dialog_setup_steps(el);
        }
    }

    /// The dialog insertion steps: an open dialog connected to a fully active document is set up.
    pub(crate) fn dialog_inserted(&self, el: NodeId) {
        let doc = self.dom().node_document(el);
        if self.is_fully_active(doc) && self.is_open(el) && self.dom().is_connected(el) {
            self.dialog_setup_steps(el);
        }
    }

    /// The dialog removing steps for the open dialogs list: open dialogs that are no longer
    /// connected, or now in another document, are cleaned up.
    pub(crate) fn open_dialogs_prune(&self) {
        let gone: Vec<NodeId> = {
            let d = self.dialogs.borrow();
            let dom = self.dom();
            d.open_dialogs
                .iter()
                .flat_map(|(&doc, l)| l.iter().map(move |&e| (doc, e)))
                .filter(|&(doc, e)| {
                    !dom.is_alive(e) || !dom.is_connected(e) || dom.node_document(e) != doc
                })
                .map(|(_, e)| e)
                .collect()
        };
        for el in gone {
            self.dialog_cleanup_steps(el);
        }
    }

    /// HTML "nearest clicked dialog" for a pointer event at `target`, `point` in its client
    /// coordinates: a press on a modal dialog's backdrop (outside its box) is in no dialog.
    fn nearest_clicked_dialog(&self, target: NodeId, point: (f64, f64)) -> Option<NodeId> {
        let is_open_dialog = |e: &Engine, n: NodeId| {
            e.dom().is_html_element_named(n, &local_name!("dialog")) && e.is_open(n)
        };
        if is_open_dialog(self, target) && self.is_modal(target) {
            let r = self.view_bounding_rect(target);
            let (x, y) = point;
            let inside = x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
            if !inside {
                return None;
            }
        }
        let mut cur = Some(target);
        while let Some(n) = cur {
            if is_open_dialog(self, n) {
                return Some(n);
            }
            cur = self.flat_parent(n);
        }
        None
    }

    /// HTML "light dismiss open dialogs", for a trusted `pointerdown` or `pointerup` at `target`
    /// (`point` in its client coordinates).
    pub(crate) fn dialog_light_dismiss(
        &self,
        cx: &mut Cx,
        target: NodeId,
        point: (f64, f64),
        pointerdown: bool,
    ) {
        let doc = self.dom().node_document(target);
        let topmost = {
            let d = self.dialogs.borrow();
            match d.open_dialogs.get(&doc).and_then(|l| l.last()) {
                Some(&t) => t,
                None => return,
            }
        };
        let ancestor = self.nearest_clicked_dialog(target, point);
        if pointerdown {
            self.dialogs
                .borrow_mut()
                .pointerdown_target
                .insert(doc, ancestor);
            return;
        }
        let down = self
            .dialogs
            .borrow_mut()
            .pointerdown_target
            .remove(&doc)
            .flatten();
        if ancestor != down || ancestor == Some(topmost) {
            return;
        }
        if self.computed_closed_by(topmost) != ClosedBy::Any {
            return;
        }
        self.request_close_watcher(cx, CloseWatcher::Dialog(topmost), false);
    }

    /// A document's top layer.
    pub(crate) fn top_layer(&self, doc: NodeId) -> Vec<NodeId> {
        self.dialogs
            .borrow()
            .top_layers
            .get(&doc)
            .cloned()
            .unwrap_or_default()
    }

    /// The renderer of `doc` takes its top layer.
    fn top_layer_changed(&self, doc: NodeId) {
        let list = self.top_layer(doc);
        if let Some(any) = list
            .first()
            .copied()
            .or_else(|| self.dom().document_element(doc))
        {
            self.with_node_renderer(any, |r, st| {
                r.set_top_layer(list);
                st.dirty = true;
            });
        }
    }

    /// "Add an element to the top layer": appended last (moved there if it was in it).
    pub(crate) fn add_to_top_layer(&self, el: NodeId) {
        let doc = self.dom().node_document(el);
        {
            let mut d = self.dialogs.borrow_mut();
            let list = d.top_layers.entry(doc).or_default();
            list.retain(|&e| e != el);
            list.push(el);
        }
        self.top_layer_changed(doc);
        self.update_blocked_marks(doc);
    }

    /// "Remove an element from the top layer immediately" (of the document it was added in, which
    /// is no longer its node document when it moved to another one).
    pub(crate) fn remove_from_top_layer(&self, el: NodeId) {
        let doc = self.dom().node_document(el);
        let removed = {
            let mut d = self.dialogs.borrow_mut();
            let doc = if d.top_layers.get(&doc).is_some_and(|l| l.contains(&el)) {
                Some(doc)
            } else {
                d.top_layers
                    .iter()
                    .find(|(_, l)| l.contains(&el))
                    .map(|(&k, _)| k)
            };
            doc.inspect(|k| {
                if let Some(list) = d.top_layers.get_mut(k) {
                    list.retain(|&e| e != el);
                }
            })
        };
        if let Some(doc) = removed {
            self.top_layer_changed(doc);
            self.update_blocked_marks(doc);
        }
    }

    fn set_modal(&self, el: NodeId, on: bool) {
        {
            let mut d = self.dialogs.borrow_mut();
            d.modal.retain(|&e| e != el);
            if on {
                d.modal.push(el);
            }
        }
        self.set_element_state(el, ElementState::MODAL, on);
        self.update_blocked_marks(self.dom().node_document(el));
    }

    /// The modal dialog `doc` is blocked by: the topmost modal dialog of its top layer.
    pub(crate) fn blocking_dialog(&self, doc: NodeId) -> Option<NodeId> {
        let d = self.dialogs.borrow();
        d.top_layers
            .get(&doc)?
            .iter()
            .rev()
            .copied()
            .find(|e| d.modal.contains(e))
    }

    /// HTML §6.3 inert nodes: an HTML element with the `inert` attribute and its flat tree
    /// descendants, and, in a document blocked by a modal dialog, every node but that dialog's flat
    /// tree descendants (which an `inert` attribute in the dialog still makes inert). The style
    /// engine computes the same from the UA sheet's `-moz-inert` rules for hit testing and selection.
    pub(crate) fn is_inert(&self, node: NodeId) -> bool {
        let blocking = self.blocking_dialog(self.dom().node_document(node));
        let mut n = Some(node);
        while let Some(id) = n {
            {
                let dom = self.dom();
                if dom.is_element(id)
                    && *dom.namespace(id) == ns!(html)
                    && dom.attr(id, &ns!(), &local_name!("inert")).is_some()
                {
                    return true;
                }
            }
            if Some(id) == blocking {
                return false;
            }
            n = self.flat_parent(id);
        }
        blocking.is_some()
    }

    /// Marks what makes a blocked document inert for the style engine (the UA sheet's inert
    /// rules): the root element of `doc` while a modal dialog blocks it (`:-moz-inert`), and that
    /// dialog (`:-moz-topmost-modal`), which escapes the root's inertness.
    fn update_blocked_marks(&self, doc: NodeId) {
        let blocking = self.blocking_dialog(doc);
        let root = blocking.and_then(|_| self.dom().document_element(doc));
        let old = {
            let mut d = self.dialogs.borrow_mut();
            let old = if root.is_none() && blocking.is_none() {
                d.blocked_marks.remove(&doc)
            } else {
                d.blocked_marks.insert(doc, (root, blocking))
            };
            old.unwrap_or_default()
        };
        if old == (root, blocking) {
            return;
        }
        let alive = |n: NodeId| self.dom().is_alive(n);
        if let Some(r) = old.0.filter(|&r| alive(r)) {
            self.set_element_state(r, ElementState::INERT, false);
        }
        if let Some(d) = old.1.filter(|&d| alive(d)) {
            self.set_element_state(d, ElementState::TOPMOST_MODAL, false);
        }
        if let Some(r) = root {
            self.set_element_state(r, ElementState::INERT, true);
        }
        if let Some(d) = blocking {
            self.set_element_state(d, ElementState::TOPMOST_MODAL, true);
        }
    }

    /// A node left the document: top layer elements that are no longer connected leave the top
    /// layer and are no longer modal (the dialog "removing steps").
    pub(crate) fn top_layer_prune(&self) {
        let gone: Vec<NodeId> = {
            let d = self.dialogs.borrow();
            let dom = self.dom();
            d.top_layers
                .values()
                .flatten()
                .copied()
                .filter(|&e| !dom.is_alive(e) || !dom.is_connected(e))
                .collect()
        };
        for el in gone {
            if self.dom().is_alive(el) {
                self.remove_from_top_layer(el);
                self.set_modal(el, false);
            } else {
                let mut d = self.dialogs.borrow_mut();
                for list in d.top_layers.values_mut() {
                    list.retain(|&e| e != el);
                }
                d.modal.retain(|&e| e != el);
            }
        }
    }

    /// The `open` attribute of a modal dialog was removed (not by `close()`): it leaves the top
    /// layer and stops being modal.
    pub(crate) fn dialog_open_removed(&self, el: NodeId) {
        if self.is_modal(el) {
            self.remove_from_top_layer(el);
            self.set_modal(el, false);
        }
    }

    /// Fires `beforetoggle` (a ToggleEvent) at a dialog; returns whether it was not canceled.
    fn fire_beforetoggle(
        &self,
        cx: &mut Cx,
        el: NodeId,
        old: &str,
        new: &str,
        cancelable: bool,
        source: Option<NodeId>,
    ) -> bool {
        let e = self.trusted_event(
            InterfaceId::ToggleEvent,
            "beforetoggle",
            false,
            cancelable,
            EventExt::Toggle {
                old_state: old.to_owned(),
                new_state: new.to_owned(),
                source: source.map(node_handle),
            },
        );
        self.dispatch(cx, e, node_handle(el), false)
    }

    /// "Queue a dialog toggle event task" (and a `details` element's "details notification task
    /// steps", which are the same): a later toggle task replaces a pending one and keeps its old state.
    pub(crate) fn queue_dialog_toggle(
        &self,
        el: NodeId,
        old: &'static str,
        new: &'static str,
        source: Option<NodeId>,
    ) {
        let (generation, old) = {
            let mut d = self.dialogs.borrow_mut();
            let old = d.toggle_tracker.get(&el).map_or(old, |t| t.1);
            d.toggle_generation += 1;
            let g = d.toggle_generation;
            d.toggle_tracker.insert(el, (g, old));
            (g, old)
        };
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let current = e.dialogs.borrow().toggle_tracker.get(&el).map(|t| t.0);
            if current != Some(generation) {
                return;
            }
            e.dialogs.borrow_mut().toggle_tracker.remove(&el);
            if !e.dom().is_alive(el) {
                return;
            }
            let source = source.filter(|&s| e.dom().is_alive(s));
            let ev = e.trusted_event(
                InterfaceId::ToggleEvent,
                "toggle",
                false,
                false,
                EventExt::Toggle {
                    old_state: old.to_owned(),
                    new_state: new.to_owned(),
                    source: source.map(node_handle),
                },
            );
            e.dispatch(cx, ev, node_handle(el), false);
        });
    }

    /// HTML "dialog focusing steps".
    pub(crate) fn dialog_focusing_steps(&self, cx: &mut Cx, dialog: NodeId) {
        let autofocus =
            |e: &Engine, n: NodeId| e.dom().attr(n, &ns!(), &local_name!("autofocus")).is_some();
        let control = if autofocus(self, dialog) {
            Some(dialog)
        } else {
            // The focus delegate: the first focusable descendant with `autofocus`, else the first
            // sequentially focusable one (a dialog's own focusability does not count).
            let descendants: Vec<NodeId> = {
                let dom = self.dom();
                dom.descendants(dialog)
                    .filter(|&n| dom.is_element(n))
                    .collect()
            };
            descendants
                .iter()
                .copied()
                .find(|&n| autofocus(self, n) && self.is_focusable(n))
                .or_else(|| {
                    descendants.iter().copied().find(|&n| {
                        self.is_focusable(n)
                            && self
                                .dom()
                                .get_attribute(n, "tabindex")
                                .and_then(rt::parse_integer)
                                .is_none_or(|t| t >= 0)
                    })
                })
                .or(Some(dialog))
        };
        if let Some(c) = control {
            self.focus_element(cx, c);
            self.autofocus_preempted(c);
        }
    }

    /// HTML "close the dialog".
    pub(crate) fn close_the_dialog(
        &self,
        cx: &mut Cx,
        el: NodeId,
        result: Option<&str>,
        source: Option<NodeId>,
    ) {
        if !self.is_open(el) {
            return;
        }
        self.fire_beforetoggle(cx, el, "open", "closed", false, source);
        if !self.is_open(el) {
            return;
        }
        self.queue_dialog_toggle(el, "open", "closed", source);
        let was_modal = self.is_modal(el);
        self.remove_attr(cx, el, "open");
        if was_modal {
            self.remove_from_top_layer(el);
        }
        self.set_modal(el, false);
        if let Some(r) = result {
            self.html
                .borrow_mut()
                .dialog_return
                .insert(el, r.to_owned());
        }
        self.dialogs.borrow_mut().request_close.remove(&el);
        let previous = self.dialogs.borrow_mut().previously_focused.remove(&el);
        if let Some(prev) = previous {
            let doc = self.dom().node_document(el);
            let focus_inside = self
                .focused_element(doc)
                .is_some_and(|f| self.dom().is_shadow_including_inclusive_ancestor_of(el, f));
            if (focus_inside || was_modal) && self.dom().is_alive(prev) {
                self.focus_element(cx, prev);
            }
        }
        self.queue_task(TaskSource::UserInteraction, move |e, cx| {
            if e.dom().is_alive(el) {
                e.fire_event(cx, node_handle(el), "close", false, false);
            }
        });
    }

    /// HTML "request to close the dialog".
    pub(crate) fn request_close_dialog(
        &self,
        cx: &mut Cx,
        el: NodeId,
        return_value: Option<&str>,
        source: Option<NodeId>,
    ) {
        if !self.is_open(el) {
            return;
        }
        let doc = self.dom().node_document(el);
        if !self.dom().is_connected(el) || !self.is_fully_active(doc) {
            return;
        }
        {
            let mut d = self.dialogs.borrow_mut();
            d.enable_for_request_close.insert(el);
            d.request_close
                .insert(el, (return_value.map(str::to_owned), source));
        }
        self.request_close_watcher(cx, CloseWatcher::Dialog(el), false);
        // (a request a `cancel` listener prevented leaves no value or source for a later close
        // request, such as light dismiss, to close with)
        let mut d = self.dialogs.borrow_mut();
        d.enable_for_request_close.remove(&el);
        d.request_close.remove(&el);
    }

    /// HTML "show a modal dialog".
    pub(crate) fn show_modal_dialog(
        &self,
        cx: &mut Cx,
        el: NodeId,
        source: Option<NodeId>,
    ) -> rt::Result<()> {
        if self.is_open(el) && self.is_modal(el) {
            return Ok(());
        }
        if self.is_open(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The dialog is already open as a non-modal dialog",
            ));
        }
        // (a dialog of a document with no browsing context, or not fully active, cannot be modal)
        let doc = self.dom().node_document(el);
        if !self.is_fully_active(doc) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The dialog's document is not fully active",
            ));
        }
        if !self.dom().is_connected(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The dialog is not connected",
            ));
        }
        if self.html.borrow().popovers.contains(&el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The dialog is an open popover",
            ));
        }
        if !self.fire_beforetoggle(cx, el, "closed", "open", true, source) {
            return Ok(());
        }
        if self.is_open(el)
            || !self.dom().is_connected(el)
            || self.html.borrow().popovers.contains(&el)
        {
            return Ok(());
        }
        self.queue_dialog_toggle(el, "closed", "open", source);
        self.set_attr(cx, el, "open", "");
        self.set_modal(el, true);
        let doc = self.dom().node_document(el);
        if !self.top_layer(doc).contains(&el) {
            self.add_to_top_layer(el);
        }
        if let Some(f) = self.focused_element(doc) {
            self.dialogs.borrow_mut().previously_focused.insert(el, f);
        }
        // (open popovers the dialog is not in are hidden)
        self.hide_popovers_for_top_layer(cx, el);
        // (the rest of the document is inert now: its focus goes)
        if let Some(f) = self.focused_element(doc) {
            if self.is_inert(f) {
                self.blur_element(cx, f);
            }
        }
        self.dialog_focusing_steps(cx, el);
        Ok(())
    }
}

/// A `closedby` state as computed for a dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClosedBy {
    Any,
    CloseRequest,
    None,
}

impl HTMLDialogElementImpl for Engine {
    fn return_value(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.html
                .borrow()
                .dialog_return
                .get(&self.n(this))
                .cloned()
                .unwrap_or_default(),
        )
    }

    fn set_return_value(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.html
            .borrow_mut()
            .dialog_return
            .insert(self.n(this), value.to_owned());
        Ok(())
    }

    /// `closedBy`: the keyword of the computed closed-by state (the Auto state has none).
    fn closed_by(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(match self.computed_closed_by(self.n(this)) {
            ClosedBy::Any => "any",
            ClosedBy::CloseRequest => "closerequest",
            ClosedBy::None => "none",
        })
    }

    /// HTML §4.11.4 `show()`.
    fn show(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let el = self.n(this);
        if self.is_open(el) && !self.is_modal(el) {
            return Ok(());
        }
        if self.is_open(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The dialog is already open as a modal dialog",
            ));
        }
        if !self.fire_beforetoggle(cx, el, "closed", "open", true, None) {
            return Ok(());
        }
        if self.is_open(el) {
            return Ok(());
        }
        self.queue_dialog_toggle(el, "closed", "open", None);
        self.set_attr(cx, el, "open", "");
        let doc = self.dom().node_document(el);
        if let Some(f) = self.focused_element(doc) {
            self.dialogs.borrow_mut().previously_focused.insert(el, f);
        }
        // (open popovers the dialog is not in are hidden)
        self.hide_popovers_for_top_layer(cx, el);
        self.dialog_focusing_steps(cx, el);
        Ok(())
    }

    /// HTML §4.11.4 `showModal()` ("show a modal dialog" with no source).
    fn show_modal(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.show_modal_dialog(cx, self.n(this), None)
    }

    /// HTML §4.11.4 `close(result)`.
    fn close(&self, cx: &mut Cx, this: Handle, return_value: Option<&str>) -> rt::Result<()> {
        self.close_the_dialog(cx, self.n(this), return_value, None);
        Ok(())
    }

    /// HTML §4.11.4 `requestClose(result)` ("request to close the dialog" with no source).
    fn request_close(
        &self,
        cx: &mut Cx,
        this: Handle,
        return_value: Option<&str>,
    ) -> rt::Result<()> {
        self.request_close_dialog(cx, self.n(this), return_value, None);
        Ok(())
    }
}
