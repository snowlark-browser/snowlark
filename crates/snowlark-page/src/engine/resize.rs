// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resize Observer (https://drafts.csswg.org/resize-observer-1/): `ResizeObserver`,
//! `ResizeObserverEntry`, `ResizeObserverSize`, and the gather/broadcast loop that HTML's "update the
//! rendering" runs for each document after style and layout.

use snowlark_bindings::generated::{
    ResizeObserverBoxOptions, ResizeObserverCallback, ResizeObserverEntryImpl, ResizeObserverImpl,
    ResizeObserverOptions, ResizeObserverSizeImpl,
};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;

use super::events::EventExt;
use super::geometry::Rect;
use super::view::BoxSizes;
use super::{exception, node_handle, Engine, Object};

/// A ResizeObservation.
pub struct Observation {
    pub target: NodeId,
    pub observed_box: ResizeObserverBoxOptions,
    /// lastReportedSizes[0] as (inline, block); (-1, -1) before the first report.
    pub last_reported: (f64, f64),
}

/// A `ResizeObserver`.
pub struct ObserverData {
    pub callback: super::held::Held<v8::Object>,
    /// The document whose [[resizeObservers]] it is in.
    pub document: Option<NodeId>,
    pub observations: Vec<Observation>,
    /// [[activeTargets]] and [[skippedTargets]], as indices into `observations`.
    pub active: Vec<usize>,
    pub skipped: Vec<usize>,
}

/// A `ResizeObserverEntry`.
pub struct EntryData {
    pub target: NodeId,
    pub content_rect: Handle,
    pub border_box_size: Handle,
    pub content_box_size: Handle,
    pub device_pixel_content_box_size: Handle,
}

/// Resize Observer "calculate box size" as (inline, block): 0×0 for an element without a box.
fn box_size(sizes: Option<&BoxSizes>, observed: ResizeObserverBoxOptions) -> (f64, f64) {
    let Some(s) = sizes else {
        return (0.0, 0.0);
    };
    let (w, h) = match observed {
        ResizeObserverBoxOptions::BorderBox => s.border,
        ResizeObserverBoxOptions::ContentBox => s.content,
        ResizeObserverBoxOptions::DevicePixelContentBox => (
            (s.content.0 * s.device_pixel_ratio).round(),
            (s.content.1 * s.device_pixel_ratio).round(),
        ),
    };
    if s.vertical {
        (h, w)
    } else {
        (w, h)
    }
}

impl Engine {
    fn ro<R>(&self, h: Handle, f: impl FnOnce(&mut ObserverData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::ResizeObserver(o) => f(o),
            _ => panic!("not a ResizeObserver"),
        }
    }

    fn ro_entry<R>(&self, h: Handle, f: impl FnOnce(&EntryData) -> R) -> R {
        match &*self.obj(h) {
            Object::ResizeObserverEntry(e) => f(e),
            _ => panic!("not a ResizeObserverEntry"),
        }
    }

    /// The resize observers of a document, in creation order.
    fn resize_observers_of(&self, doc: NodeId) -> Vec<Handle> {
        let list = self.intersection.borrow().resize_observers.clone();
        list.into_iter()
            .filter(|&h| self.ro(h, |o| o.document) == Some(doc))
            .collect()
    }

    /// Whether some observation has never been reported (its first size always is).
    pub(crate) fn resize_pending(&self) -> bool {
        let state = self.intersection.borrow();
        if state.resize_observers.is_empty() {
            return false;
        }
        let objects = self.objects.borrow();
        // (only rendered documents run the observation loop)
        state.resize_observers.iter().any(|h| {
            matches!(&objects[h.get() as usize - 1], Object::ResizeObserver(o)
                if o.document.is_some_and(|d| self.is_rendered_document(d))
                    && o.observations.iter().any(|ob| ob.last_reported.0 < 0.0))
        })
    }

    /// Resize Observer "calculate depth for node": the number of nodes on the flat tree path from
    /// `node` to the root.
    fn flat_tree_depth(&self, node: NodeId) -> usize {
        let dom = self.dom();
        let mut depth = 0;
        let mut n = Some(node);
        while let Some(id) = n {
            depth += 1;
            n = match dom.parent(id) {
                Some(p) if dom.is_shadow_root(p) => dom.host(p),
                p => p,
            };
        }
        depth
    }

    /// "Gather active resize observations at depth".
    fn gather_active_resize_observations(&self, doc: NodeId, depth: usize) {
        for ro in self.resize_observers_of(doc) {
            let observations: Vec<(NodeId, ResizeObserverBoxOptions, (f64, f64))> =
                self.ro(ro, |o| {
                    o.active.clear();
                    o.skipped.clear();
                    o.observations
                        .iter()
                        .map(|ob| (ob.target, ob.observed_box, ob.last_reported))
                        .collect()
                });
            let mut active = Vec::new();
            let mut skipped = Vec::new();
            for (i, (target, observed, last)) in observations.into_iter().enumerate() {
                let alive = self.dom().is_alive(target);
                let sizes = if alive {
                    self.view_box_sizes(target)
                } else {
                    None
                };
                // isActive(): the size differs from the last reported one.
                if box_size(sizes.as_ref(), observed) == last {
                    continue;
                }
                if !alive || self.flat_tree_depth(target) > depth {
                    active.push(i);
                } else {
                    skipped.push(i);
                }
            }
            self.ro(ro, |o| {
                o.active = active;
                o.skipped = skipped;
            });
        }
    }

    fn has_active_resize_observations(&self, doc: NodeId) -> bool {
        self.resize_observers_of(doc)
            .into_iter()
            .any(|ro| self.ro(ro, |o| !o.active.is_empty()))
    }

    fn has_skipped_resize_observations(&self, doc: NodeId) -> bool {
        self.resize_observers_of(doc)
            .into_iter()
            .any(|ro| self.ro(ro, |o| !o.skipped.is_empty()))
    }

    fn new_resize_size(&self, (inline, block): (f64, f64)) -> Handle {
        self.alloc(Object::ResizeObserverSize { inline, block })
    }

    /// "Broadcast active resize observations": returns the shallowest depth of a reported target.
    fn broadcast_active_resize_observations(&self, cx: &mut Cx, doc: NodeId) -> usize {
        let mut shallowest = usize::MAX;
        for ro in self.resize_observers_of(doc) {
            let active: Vec<(usize, NodeId, ResizeObserverBoxOptions)> = self.ro(ro, |o| {
                o.active
                    .iter()
                    .map(|&i| (i, o.observations[i].target, o.observations[i].observed_box))
                    .collect()
            });
            if active.is_empty() {
                continue;
            }
            let mut entries = Vec::new();
            for (i, target, observed) in active {
                let sizes = self.view_box_sizes(target);
                let border = box_size(sizes.as_ref(), ResizeObserverBoxOptions::BorderBox);
                let content = box_size(sizes.as_ref(), ResizeObserverBoxOptions::ContentBox);
                let device = box_size(
                    sizes.as_ref(),
                    ResizeObserverBoxOptions::DevicePixelContentBox,
                );
                let (x, y, w, h) = match &sizes {
                    Some(s) => (s.padding_left, s.padding_top, s.content.0, s.content.1),
                    None => (0.0, 0.0, 0.0, 0.0),
                };
                let entry = EntryData {
                    target,
                    content_rect: self.alloc(Object::DomRect(Rect::read_only(x, y, w, h))),
                    border_box_size: self.new_resize_size(border),
                    content_box_size: self.new_resize_size(content),
                    device_pixel_content_box_size: self.new_resize_size(device),
                };
                entries.push(self.alloc(Object::ResizeObserverEntry(Box::new(entry))));
                let reported = match observed {
                    ResizeObserverBoxOptions::BorderBox => border,
                    ResizeObserverBoxOptions::ContentBox => content,
                    ResizeObserverBoxOptions::DevicePixelContentBox => device,
                };
                self.ro(ro, |o| {
                    if let Some(ob) = o.observations.get_mut(i) {
                        ob.last_reported = reported;
                    }
                });
                if self.dom().is_alive(target) {
                    shallowest = shallowest.min(self.flat_tree_depth(target));
                }
            }
            let callback = self.ro(ro, |o| {
                o.active.clear();
                o.callback.clone()
            });
            let Some(callback) = callback.get(cx.scope()).map(ResizeObserverCallback) else {
                continue;
            };
            let realm = {
                let scope = cx.scope();
                let f = v8::Local::new(scope, callback.0.global());
                f.get_creation_context(scope)
                    .map(|c| v8::Global::new(scope, c))
            };
            self.in_realm(cx, realm, |cx| {
                let scope = cx.scope();
                v8::tc_scope!(let tc, scope);
                let mut cx2 = Cx::new(tc);
                if callback.invoke(&mut cx2, Some(&ro), entries, ro).is_err() {
                    if let Some(e) = tc.exception() {
                        let e = v8::Global::new(tc, e);
                        let m = tc.message().map(|m| v8::Global::new(tc, m));
                        tc.reset();
                        let mut cx3 = Cx::new(tc);
                        self.report_exception(&mut cx3, e, m);
                    }
                }
            });
        }
        shallowest
    }

    /// "Deliver resize loop error notification": an ErrorEvent at the document's window.
    fn deliver_resize_loop_error(&self, cx: &mut Cx, doc: NodeId) {
        let Some(window) = self.window_of_document(doc) else {
            return;
        };
        let message = "ResizeObserver loop completed with undelivered notifications.";
        let event = self.trusted_event(
            InterfaceId::ErrorEvent,
            "error",
            false,
            true,
            EventExt::Error {
                message: message.to_owned(),
                filename: String::new(),
                lineno: 0,
                colno: 0,
                error: None,
            },
        );
        if self.dispatch(cx, event, window, false) {
            self.console_error(message);
        }
    }

    /// HTML "update the rendering": the resize observation loop for each rendered document, after
    /// its style and layout are up to date.
    pub(crate) fn update_resize_observations(&self, cx: &mut Cx) {
        if self.intersection.borrow().resize_observers.is_empty() {
            return;
        }
        for doc in self.rendered_documents() {
            if self.resize_observers_of(doc).is_empty() {
                continue;
            }
            let mut depth = 0;
            loop {
                self.gather_active_resize_observations(doc, depth);
                if !self.has_active_resize_observations(doc) {
                    break;
                }
                depth = self.broadcast_active_resize_observations(cx, doc);
                self.microtask_checkpoint(cx);
            }
            if self.has_skipped_resize_observations(doc) {
                self.deliver_resize_loop_error(cx, doc);
            }
        }
    }

    /// Freed nodes leave the observers' observation lists.
    pub(crate) fn resize_nodes_freed(&self, freed: &snowlark_dom::NodeSet) {
        let observers = self.intersection.borrow().resize_observers.clone();
        let mut objects = self.objects.borrow_mut();
        for h in observers {
            if let Some(Object::ResizeObserver(o)) = objects.get_mut(h.get() as usize - 1) {
                o.observations.retain(|ob| !freed.contains(&ob.target));
                o.active.clear();
                o.skipped.clear();
            }
        }
    }
}

impl ResizeObserverImpl for Engine {
    /// `observe(target, options)`.
    fn observe(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: Handle,
        options: ResizeObserverOptions,
    ) -> rt::Result<()> {
        let target = self.n(target);
        self.ro(this, |o| {
            if let Some(i) = o.observations.iter().position(|ob| ob.target == target) {
                o.observations.remove(i);
                o.active.clear();
                o.skipped.clear();
            }
            o.observations.push(Observation {
                target,
                observed_box: options.box_,
                last_reported: (-1.0, -1.0),
            });
        });
        Ok(())
    }
    /// `unobserve(target)`.
    fn unobserve(&self, _cx: &mut Cx, this: Handle, target: Handle) -> rt::Result<()> {
        let target = self.n(target);
        self.ro(this, |o| {
            if let Some(i) = o.observations.iter().position(|ob| ob.target == target) {
                o.observations.remove(i);
                o.active.clear();
                o.skipped.clear();
            }
        });
        Ok(())
    }
    /// `disconnect()`.
    fn disconnect(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.ro(this, |o| {
            o.observations.clear();
            o.active.clear();
        });
        Ok(())
    }
    /// `new ResizeObserver(callback)`: in the current global's document's [[resizeObservers]].
    fn constructor(&self, cx: &mut Cx, callback: ResizeObserverCallback) -> rt::Result<Handle> {
        let window = self
            .current_window(cx)
            .ok_or_else(|| exception(ExceptionName::InvalidStateError, "No window"))?;
        let document = self.associated_document(window);
        let h = self.alloc(Object::ResizeObserver(Box::new(ObserverData {
            callback: super::held::Held::object(callback.0),
            document,
            observations: Vec::new(),
            active: Vec::new(),
            skipped: Vec::new(),
        })));
        self.intersection.borrow_mut().resize_observers.push(h);
        Ok(h)
    }
}

impl ResizeObserverEntryImpl for Engine {
    fn target(&self, this: Handle) -> Handle {
        node_handle(self.ro_entry(this, |e| e.target))
    }
    fn content_rect(&self, this: Handle) -> Handle {
        self.ro_entry(this, |e| e.content_rect)
    }
    fn border_box_size(&self, this: Handle) -> Vec<Handle> {
        vec![self.ro_entry(this, |e| e.border_box_size)]
    }
    fn content_box_size(&self, this: Handle) -> Vec<Handle> {
        vec![self.ro_entry(this, |e| e.content_box_size)]
    }
    fn device_pixel_content_box_size(&self, this: Handle) -> Vec<Handle> {
        vec![self.ro_entry(this, |e| e.device_pixel_content_box_size)]
    }
}

impl ResizeObserverSizeImpl for Engine {
    fn inline_size(&self, this: Handle) -> f64 {
        match &*self.obj(this) {
            Object::ResizeObserverSize { inline, .. } => *inline,
            _ => 0.0,
        }
    }
    fn block_size(&self, this: Handle) -> f64 {
        match &*self.obj(this) {
            Object::ResizeObserverSize { block, .. } => *block,
            _ => 0.0,
        }
    }
}
