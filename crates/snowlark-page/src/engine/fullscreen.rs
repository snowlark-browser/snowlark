// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Fullscreen API Standard: `requestFullscreen()`, `exitFullscreen()`, `fullscreenElement`,
//! `fullscreenEnabled`, the fullscreen flag (`:fullscreen`) and the top layer, each document's list
//! of pending fullscreen events (fired by "run the fullscreen steps" in "update the rendering"),
//! the removing steps and the unloading document cleanup steps, and the "fullscreen" feature of
//! Permissions Policy (the default allowlist `'self'`, the container's `allow` and `allowfullscreen`
//! attributes).
//!
//! The steps the standard runs "in parallel" wait for the top-level viewport: the engine asks the
//! embedder to resize it to the fullscreen dimensions or back ([`Engine::take_fullscreen_asks`])
//! and goes on when it answers ([`Engine::fullscreen_viewport_changed`]), after the viewport's new
//! size (so the `resize` event fires in the same rendering update as `fullscreenchange`, before
//! it). The browser decides; when the user ends fullscreen there (Esc, another tab) it says the
//! viewport is no longer fullscreen and the engine fully exits fullscreen. Without an embedder that
//! answers (the headless modes) the engine answers itself, in a task.
//!
//! Not implemented: popovers (the "hide all popovers" steps), the `Permissions-Policy` header,
//! screen orientation locks.

use rustc_hash::FxHashSet;
use snowlark_bindings::rt::{self, Cx, Handle, JsPromise};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, ns, NodeId};
use snowlark_style::ElementState;

use super::events::EventExt;
use super::policy::Feature;
use super::{node_handle, Engine};
use crate::event_loop::TaskSource;

/// A `requestFullscreen()` whose remaining steps wait for the viewport.
struct Enter {
    id: u64,
    element: NodeId,
    pending_doc: NodeId,
    promise: Option<JsPromise>,
    /// The steps before "in parallel" found an error: no resize is asked for.
    error: bool,
}

/// An "exit fullscreen" whose remaining steps wait for the viewport (or a task).
struct Exit {
    id: u64,
    doc: NodeId,
    promise: Option<JsPromise>,
    resize: bool,
}

/// Fullscreen state the DOM does not keep.
#[derive(Default)]
pub struct FullscreenState {
    /// Elements whose fullscreen flag is set.
    flagged: FxHashSet<NodeId>,
    /// Elements whose iframe fullscreen flag is set.
    iframe_flagged: FxHashSet<NodeId>,
    /// The documents' lists of pending fullscreen events, in the order they were appended.
    pending: Vec<(NodeId, &'static str, NodeId)>,
    entering: Vec<Enter>,
    exiting: Vec<Exit>,
    next_id: u64,
    /// The top-level viewport has the fullscreen dimensions (the embedder said so).
    viewport: bool,
    /// What the engine last asked the embedder for and has no answer to yet.
    asked: Option<bool>,
}

/// The embedder's side: whether it resizes the viewport (the window), and what the engine asked.
#[derive(Default)]
pub struct FullscreenAsks {
    pub by_embedder: bool,
    pub asks: Vec<bool>,
}

impl Engine {
    // ----- state -----

    pub(crate) fn has_fullscreen_flag(&self, el: NodeId) -> bool {
        self.fullscreen.borrow().flagged.contains(&el)
    }

    /// A document's fullscreen element: the topmost element of its top layer whose fullscreen flag
    /// is set.
    pub(crate) fn fullscreen_element_of(&self, doc: NodeId) -> Option<NodeId> {
        let fs = self.fullscreen.borrow();
        if fs.flagged.is_empty() {
            return None;
        }
        self.top_layer(doc)
            .into_iter()
            .rev()
            .find(|e| fs.flagged.contains(e))
    }

    fn fullscreen_id(&self) -> u64 {
        let mut fs = self.fullscreen.borrow_mut();
        fs.next_id += 1;
        fs.next_id
    }

    fn set_fullscreen_flag(&self, el: NodeId, on: bool) {
        {
            let mut fs = self.fullscreen.borrow_mut();
            if on {
                fs.flagged.insert(el);
            } else {
                fs.flagged.remove(&el);
                fs.iframe_flagged.remove(&el);
            }
        }
        if self.dom().is_alive(el) {
            // (`:modal` matches an element whose fullscreen flag is set, too)
            self.set_element_state(el, ElementState::FULLSCREEN | ElementState::MODAL, on);
        }
    }

    /// "Fullscreen an element".
    fn fullscreen_an_element(&self, el: NodeId) {
        self.set_fullscreen_flag(el, true);
        self.remove_from_top_layer(el);
        self.add_to_top_layer(el);
        self.fullscreen_element_changed(self.dom().node_document(el));
    }

    /// "Unfullscreen an element".
    fn unfullscreen_element(&self, el: NodeId) {
        self.set_fullscreen_flag(el, false);
        self.remove_from_top_layer(el);
        if self.dom().is_alive(el) {
            self.fullscreen_element_changed(self.dom().node_document(el));
        }
    }

    /// The renderer of `doc` takes its fullscreen element (the viewport's scrollbar follows).
    fn fullscreen_element_changed(&self, doc: NodeId) {
        let fe = self.fullscreen_element_of(doc);
        self.with_doc_renderer(doc, |r, st| {
            r.set_fullscreen_element(fe);
            st.dirty = true;
        });
    }

    /// "Unfullscreen a document": every element of its top layer with the fullscreen flag.
    fn unfullscreen_document(&self, doc: NodeId) {
        let flagged: Vec<NodeId> = {
            let fs = self.fullscreen.borrow();
            self.top_layer(doc)
                .into_iter()
                .filter(|e| fs.flagged.contains(e))
                .collect()
        };
        for e in flagged {
            self.unfullscreen_element(e);
        }
    }

    /// A simple fullscreen document: exactly one element of its top layer has the fullscreen flag.
    fn is_simple_fullscreen_document(&self, doc: NodeId) -> bool {
        let fs = self.fullscreen.borrow();
        self.top_layer(doc)
            .iter()
            .filter(|e| fs.flagged.contains(e))
            .count()
            == 1
    }

    fn push_fullscreen_event(&self, doc: NodeId, type_: &'static str, el: NodeId) {
        self.fullscreen.borrow_mut().pending.push((doc, type_, el));
    }

    // ----- navigables -----

    /// The container of `doc`'s node navigable (an `iframe`, `frame`, `object` or `embed`).
    pub(crate) fn container_of_document(&self, doc: NodeId) -> Option<NodeId> {
        let id = self.context_of_document(doc)?;
        self.frames.borrow().list[id].container
    }

    /// "Fully active": the active document of a navigable that is top-level or whose container
    /// document is fully active.
    pub(crate) fn is_fully_active(&self, mut doc: NodeId) -> bool {
        loop {
            let Some(id) = self.context_of_document(doc) else {
                return false;
            };
            let (discarded, container) = {
                let f = self.frames.borrow();
                (f.list[id].discarded, f.list[id].container)
            };
            if discarded {
                return false;
            }
            match container {
                None => return true,
                Some(c) => {
                    if !self.dom().is_alive(c) {
                        return false;
                    }
                    doc = self.dom().node_document(c);
                }
            }
        }
    }

    /// The active document of `doc`'s top-level traversable.
    pub(crate) fn top_level_document_of(&self, mut doc: NodeId) -> NodeId {
        while let Some(c) = self.container_of_document(doc) {
            if !self.dom().is_alive(c) {
                break;
            }
            doc = self.dom().node_document(c);
        }
        doc
    }

    /// The active documents of `doc`'s descendant navigables, in tree order.
    pub(crate) fn descendant_documents(&self, doc: NodeId) -> Vec<NodeId> {
        let Some(id) = self.context_of_document(doc) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(at) = stack.pop() {
            let f = self.frames.borrow();
            let mut children: Vec<(NodeId, usize)> = f
                .list
                .iter()
                .enumerate()
                .filter(|(_, b)| b.parent == Some(at) && !b.discarded)
                .map(|(c, b)| (b.document, c))
                .collect();
            drop(f);
            // (tree order of their containers)
            let dom = self.dom();
            children.sort_by(|a, b| {
                let ca = self.frames.borrow().list[a.1].container;
                let cb = self.frames.borrow().list[b.1].container;
                match (ca, cb) {
                    (Some(x), Some(y)) => dom.tree_order(x, y),
                    _ => std::cmp::Ordering::Equal,
                }
            });
            drop(dom);
            for &(d, c) in children.iter().rev() {
                stack.push(c);
                let _ = d;
            }
            if at != id {
                out.push(self.frames.borrow().list[at].document);
            }
        }
        out
    }

    /// "Allowed to use" the "fullscreen" feature.
    fn fullscreen_allowed_to_use(&self, doc: NodeId) -> bool {
        self.allowed_to_use(doc, Feature::Fullscreen)
    }

    /// "Fullscreen element ready check".
    fn fullscreen_ready_check(&self, el: NodeId) -> bool {
        let dom = self.dom();
        if !dom.is_alive(el) || !dom.is_connected(el) {
            return false;
        }
        let doc = dom.node_document(el);
        drop(dom);
        // (a showing popover is in the top layer already)
        !self.popover_showing(el) && self.fullscreen_allowed_to_use(doc)
    }

    // ----- the embedder -----

    /// The embedder resizes the viewport for fullscreen (the window); without it the engine answers
    /// its own requests.
    pub fn set_fullscreen_by_embedder(&self, on: bool) {
        self.embedder.borrow_mut().fullscreen.by_embedder = on;
    }

    /// What the engine asked the embedder for since the last call: `true` to resize the top-level
    /// viewport to the fullscreen dimensions, `false` back to the normal ones. The embedder answers
    /// with [`Engine::fullscreen_viewport_changed`], denials included.
    pub fn take_fullscreen_asks(&self) -> Vec<bool> {
        std::mem::take(&mut self.embedder.borrow_mut().fullscreen.asks)
    }

    fn ask_fullscreen_viewport(&self, on: bool) {
        {
            let mut fs = self.fullscreen.borrow_mut();
            if fs.asked == Some(on) {
                return;
            }
            fs.asked = Some(on);
        }
        if self.embedder.borrow().fullscreen.by_embedder {
            self.embedder.borrow_mut().fullscreen.asks.push(on);
        } else {
            self.queue_task(TaskSource::UserInteraction, move |e, cx| {
                e.fullscreen_viewport_changed(cx, on);
            });
        }
    }

    /// The embedder's answer ([`Engine::fullscreen_viewport_changed`]) from outside a task: it is
    /// taken up by a task of its own, after the viewport's new size.
    pub fn fullscreen_viewport_answer(&self, on: bool) {
        self.queue_task(TaskSource::UserInteraction, move |e, cx| {
            e.fullscreen_viewport_changed(cx, on);
        });
    }

    /// The embedder's answer, or its own decision: the top-level viewport has the fullscreen
    /// dimensions now (`true`) or the normal ones. Requests waiting for it go on; when the viewport
    /// left fullscreen without being asked to (the user ended it), the top-level document fully
    /// exits fullscreen.
    pub fn fullscreen_viewport_changed(&self, cx: &mut Cx, on: bool) {
        let (enters, exits) = {
            let mut fs = self.fullscreen.borrow_mut();
            fs.viewport = on;
            if fs.asked == Some(on) || !on {
                fs.asked = None;
            }
            let enters: Vec<u64> = fs
                .entering
                .iter()
                .filter(|r| !r.error)
                .map(|r| r.id)
                .collect();
            let exits: Vec<u64> = if on {
                Vec::new()
            } else {
                fs.exiting.iter().map(|r| r.id).collect()
            };
            (enters, exits)
        };
        for id in enters {
            self.finish_enter(cx, id, on);
        }
        for id in exits {
            self.finish_exit(cx, id);
        }
        let Some(top) = self.document() else {
            return;
        };
        if on {
            // (a grant nothing took up any more: the element left meanwhile)
            if self.fullscreen_element_of(top).is_none()
                && self.fullscreen.borrow().exiting.is_empty()
            {
                self.ask_fullscreen_viewport(false);
            }
        } else if self.fullscreen_element_of(top).is_some() {
            // "If the end user instructs the user agent to end a fullscreen session initiated via
            // requestFullscreen()": fully exit fullscreen the top-level document.
            self.fully_exit_fullscreen(cx, top);
        }
    }

    // ----- requestFullscreen() -----

    pub(crate) fn request_fullscreen(&self, cx: &mut Cx, el: NodeId) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let pending_doc = self.dom().node_document(el);
        // 3.
        if !self.is_fully_active(pending_doc) {
            p.reject(
                cx,
                rt::Error::Type("The document is not fully active".into()),
            );
            return Ok(p);
        }
        // 5.
        let kind_ok = {
            let dom = self.dom();
            let ns_ = dom.namespace(el).clone();
            let name = dom.local_name(el).clone();
            (ns_ == ns!(html) && name != local_name!("dialog"))
                || (ns_ == ns!(svg) && name == local_name!("svg"))
                || (ns_ == ns!(mathml) && name == local_name!("math"))
        };
        let error = !kind_ok
            || !self.fullscreen_ready_check(el)
            || !self.has_transient_activation(pending_doc);
        // 6.
        if !error {
            self.consume_user_activation(pending_doc);
        }
        let id = self.fullscreen_id();
        self.fullscreen.borrow_mut().entering.push(Enter {
            id,
            element: el,
            pending_doc,
            promise: Some(p.clone()),
            error,
        });
        // 8-9. "In parallel": resize the top-level viewport, unless it has the fullscreen
        // dimensions already.
        let viewport = self.fullscreen.borrow().viewport;
        if error || viewport {
            self.queue_task(TaskSource::UserInteraction, move |e, cx| {
                let on = e.fullscreen.borrow().viewport;
                e.finish_enter(cx, id, on);
            });
        } else {
            self.ask_fullscreen_viewport(true);
        }
        Ok(p)
    }

    /// Steps 10-15 of `requestFullscreen()`, once the viewport was resized (`resized`) or not.
    fn finish_enter(&self, cx: &mut Cx, id: u64, resized: bool) {
        let req = {
            let mut fs = self.fullscreen.borrow_mut();
            let Some(i) = fs.entering.iter().position(|r| r.id == id) else {
                return;
            };
            fs.entering.remove(i)
        };
        let el = req.element;
        let doc = req.pending_doc;
        let alive = self.dom().is_alive(el);
        // 10-11.
        if req.error
            || !resized
            || !alive
            || self.dom().node_document(el) != doc
            || !self.fullscreen_ready_check(el)
        {
            if alive {
                self.push_fullscreen_event(doc, "fullscreenerror", el);
            }
            if let Some(p) = req.promise {
                p.reject(cx, rt::Error::Type("Fullscreen request denied".into()));
            }
            return;
        }
        // 12-13. The element, then each container up to the top-level traversable.
        let mut elements = vec![el];
        loop {
            let last = *elements.last().unwrap();
            let doc_of_last = self.dom().node_document(last);
            match self.container_of_document(doc_of_last) {
                Some(c) if self.dom().is_alive(c) => elements.push(c),
                _ => break,
            }
        }
        // 14.
        for &e in &elements {
            let d = self.dom().node_document(e);
            if self.fullscreen_element_of(d) == Some(e) {
                continue;
            }
            if e == el && self.dom().is_html_element_named(e, &local_name!("iframe")) {
                self.fullscreen.borrow_mut().iframe_flagged.insert(e);
            }
            // (open popovers the element is not in are hidden)
            self.hide_popovers_for_top_layer(cx, e);
            self.fullscreen_an_element(e);
            self.push_fullscreen_event(d, "fullscreenchange", e);
        }
        // 15.
        if let Some(p) = req.promise {
            p.resolve(cx, ());
        }
    }

    // ----- exitFullscreen() -----

    /// "Collect documents to unfullscreen".
    fn collect_documents_to_unfullscreen(&self, doc: NodeId) -> Vec<NodeId> {
        let mut docs = vec![doc];
        loop {
            let last = *docs.last().unwrap();
            if !self.is_simple_fullscreen_document(last) {
                break;
            }
            let Some(container) = self.container_of_document(last) else {
                break;
            };
            if !self.dom().is_alive(container)
                || self.fullscreen.borrow().iframe_flagged.contains(&container)
            {
                break;
            }
            docs.push(self.dom().node_document(container));
        }
        docs
    }

    /// "Exit fullscreen" a document, settling `promise` when done.
    pub(crate) fn exit_fullscreen(&self, mut doc: NodeId, promise: Option<JsPromise>, cx: &mut Cx) {
        // 2.
        if !self.is_fully_active(doc) || self.fullscreen_element_of(doc).is_none() {
            if let Some(p) = promise {
                p.reject(cx, rt::Error::Type("Not in fullscreen".into()));
            }
            return;
        }
        // 3-6.
        let mut resize = false;
        let docs = self.collect_documents_to_unfullscreen(doc);
        let top = self.top_level_document_of(doc);
        if docs.contains(&top) && self.is_simple_fullscreen_document(top) {
            doc = top;
            resize = true;
        }
        // 7.
        if let Some(fe) = self.fullscreen_element_of(doc) {
            if !self.dom().is_alive(fe) || !self.dom().is_connected(fe) {
                self.push_fullscreen_event(doc, "fullscreenchange", fe);
                self.unfullscreen_element(fe);
            }
        }
        // 8-10. "In parallel": resize the viewport back to its normal dimensions.
        let id = self.fullscreen_id();
        self.fullscreen.borrow_mut().exiting.push(Exit {
            id,
            doc,
            promise,
            resize,
        });
        if resize && self.fullscreen.borrow().viewport {
            self.ask_fullscreen_viewport(false);
        } else {
            self.queue_task(TaskSource::UserInteraction, move |e, cx| {
                e.finish_exit(cx, id)
            });
        }
    }

    /// Steps 11-16 of "exit fullscreen".
    fn finish_exit(&self, cx: &mut Cx, id: u64) {
        let req = {
            let mut fs = self.fullscreen.borrow_mut();
            let Some(i) = fs.exiting.iter().position(|r| r.id == id) else {
                return;
            };
            fs.exiting.remove(i)
        };
        let doc = req.doc;
        // 11.
        if !self.dom().is_alive(doc) || self.fullscreen_element_of(doc).is_none() {
            if let Some(p) = req.promise {
                p.resolve(cx, ());
            }
            return;
        }
        // 12-13.
        let exit_docs = self.collect_documents_to_unfullscreen(doc);
        let descendants: Vec<NodeId> = self
            .descendant_documents(doc)
            .into_iter()
            .filter(|&d| self.fullscreen_element_of(d).is_some())
            .collect();
        // 14.
        for d in exit_docs {
            let Some(fe) = self.fullscreen_element_of(d) else {
                continue;
            };
            self.push_fullscreen_event(d, "fullscreenchange", fe);
            if req.resize {
                self.unfullscreen_document(d);
            } else {
                self.unfullscreen_element(fe);
            }
        }
        // 15.
        for d in descendants {
            if let Some(fe) = self.fullscreen_element_of(d) {
                self.push_fullscreen_event(d, "fullscreenchange", fe);
                self.unfullscreen_document(d);
            }
        }
        // 16.
        if let Some(p) = req.promise {
            p.resolve(cx, ());
        }
    }

    /// "Fully exit fullscreen" a document.
    pub(crate) fn fully_exit_fullscreen(&self, cx: &mut Cx, doc: NodeId) {
        let Some(fe) = self.fullscreen_element_of(doc) else {
            return;
        };
        let others: Vec<NodeId> = {
            let fs = self.fullscreen.borrow();
            self.top_layer(doc)
                .into_iter()
                .filter(|e| *e != fe && fs.flagged.contains(e))
                .collect()
        };
        for e in others {
            self.unfullscreen_element(e);
        }
        self.exit_fullscreen(doc, None, cx);
    }

    /// The fullscreen removing steps, for every element that left its document: one that was its
    /// document's fullscreen element exits fullscreen; others are unfullscreened.
    pub(crate) fn fullscreen_removed(&self, cx: &mut Cx) {
        let gone: Vec<NodeId> = {
            let fs = self.fullscreen.borrow();
            let dom = self.dom();
            fs.flagged
                .iter()
                .copied()
                .filter(|&e| !dom.is_alive(e) || !dom.is_connected(e))
                .collect()
        };
        if gone.is_empty() {
            return;
        }
        // (shadow-including tree order is not observable here: each is handled on its own)
        for node in gone {
            if !self.dom().is_alive(node) {
                self.set_fullscreen_flag(node, false);
                continue;
            }
            let doc = self.dom().node_document(node);
            if self.fullscreen_element_of(doc) == Some(node) {
                self.exit_fullscreen(doc, None, cx);
            } else {
                self.unfullscreen_element(node);
            }
            if self.top_layer(doc).contains(&node) {
                self.remove_from_top_layer(node);
            }
        }
    }

    /// The unloading document cleanup steps: fully exit fullscreen.
    pub(crate) fn fullscreen_unloaded(&self, cx: &mut Cx, doc: NodeId) {
        if self.fullscreen.borrow().flagged.is_empty() {
            return;
        }
        self.fully_exit_fullscreen(cx, doc);
    }

    /// A document is gone: its fullscreen state goes with it.
    pub(crate) fn fullscreen_forget(&self, docs: &[NodeId]) {
        let mut fs = self.fullscreen.borrow_mut();
        fs.pending.retain(|(d, _, _)| !docs.contains(d));
    }

    // ----- the rendering -----

    pub(crate) fn fullscreen_events_pending(&self) -> bool {
        !self.fullscreen.borrow().pending.is_empty()
    }

    /// "Run the fullscreen steps" for `doc` (HTML "update the rendering"): its pending fullscreen
    /// events, in order, at the element when it is still connected and in `doc`, else at `doc`;
    /// they bubble and are composed.
    pub(crate) fn run_fullscreen_steps(&self, cx: &mut Cx, doc: NodeId) {
        let events: Vec<(&'static str, NodeId)> = {
            let mut fs = self.fullscreen.borrow_mut();
            if fs.pending.is_empty() {
                return;
            }
            let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut fs.pending)
                .into_iter()
                .partition(|(d, _, _)| *d == doc);
            fs.pending = rest;
            mine.into_iter().map(|(_, t, e)| (t, e)).collect()
        };
        for (type_, el) in events {
            let target = {
                let dom = self.dom();
                if dom.is_alive(el) && dom.is_connected(el) && dom.node_document(el) == doc {
                    el
                } else {
                    doc
                }
            };
            let e = self.new_event(InterfaceId::Event, type_, true, false, true, EventExt::None);
            self.ev_mut(e, |ev| ev.is_trusted = true);
            self.dispatch(cx, e, node_handle(target), false);
        }
    }

    // ----- user activation -----

    /// HTML §6.4.3 "consume user activation": the last activation timestamp of every window of the
    /// top-level traversable's navigables that has one becomes negative infinity (sticky activation
    /// stays, transient activation ends).
    pub(crate) fn consume_user_activation(&self, doc: NodeId) {
        let top = self.top_level_document_of(doc);
        let mut docs = vec![top];
        docs.extend(self.descendant_documents(top));
        let mut e = self.embedder.borrow_mut();
        for d in docs {
            if let Some(t) = e.activations.get_mut(&d) {
                if *t != f64::INFINITY {
                    *t = f64::NEG_INFINITY;
                }
            }
        }
    }

    /// The handle of `doc`'s fullscreen element retargeted against `root` (a document or a shadow
    /// root), for `fullscreenElement`.
    pub(crate) fn fullscreen_element_for(&self, root: NodeId) -> Option<Handle> {
        let dom = self.dom();
        if !dom.is_document(root) {
            let host = dom.host(root)?;
            if !dom.is_connected(host) {
                return None;
            }
        }
        let doc = dom.node_document(root);
        drop(dom);
        let fe = self.fullscreen_element_of(doc)?;
        let dom = self.dom();
        let t = self.retarget(&dom, fe, root);
        (dom.root(t) == root).then(|| node_handle(t))
    }

    /// `fullscreenEnabled`.
    pub(crate) fn fullscreen_enabled_for(&self, doc: NodeId) -> bool {
        self.fullscreen_allowed_to_use(doc)
    }
}
