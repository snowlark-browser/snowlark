// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Intersection Observer (https://w3c.github.io/IntersectionObserver/): `IntersectionObserver`,
//! `IntersectionObserverEntry`, the "update intersection observations" steps that HTML's "update the
//! rendering" runs for each document, and the intersection observer task that notifies observers.
//!
//! Geometry comes from the renderers' last layout (`view.rs`): a target's bounding box, the content
//! clips on its containing block chain, and, for a target in a nested document, that document's
//! viewport and its container's content box in the parent document.
//!
//! Not implemented yet: Intersection Observer 2's visibility algorithm (`trackVisibility`), so
//! `isVisible` is always false.

use snowlark_bindings::generated::{
    DoubleOrDoubleSequence, ElementOrDocument, IntersectionObserverCallback,
    IntersectionObserverEntryImpl, IntersectionObserverEntryInit, IntersectionObserverImpl,
    IntersectionObserverInit,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle};
use snowlark_dom::NodeId;

use super::geometry::Rect;
use super::{node_handle, Engine, Object};
use crate::event_loop::TaskSource;

/// One side of a root or scroll margin: pixels, or a percentage of the box's width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Margin {
    pub value: f64,
    pub percent: bool,
}

impl Margin {
    fn resolve(self, width: f64) -> f64 {
        if self.percent {
            width * self.value / 100.0
        } else {
            self.value
        }
    }
}

/// Intersection Observer "parse a margin": one to four absolute lengths or percentages.
pub fn parse_margin(s: &str) -> Option<[Margin; 4]> {
    let mut values = Vec::new();
    for token in s.split_ascii_whitespace() {
        if values.len() == 4 {
            return None;
        }
        values.push(parse_length_or_percentage(token)?);
    }
    let px0 = Margin {
        value: 0.0,
        percent: false,
    };
    Some(match values[..] {
        [] => [px0; 4],
        [a] => [a; 4],
        [a, b] => [a, b, a, b],
        [a, b, c] => [a, b, c, b],
        [a, b, c, d] => [a, b, c, d],
        _ => return None,
    })
}

/// A CSS `<length>` in an absolute unit (as px) or a `<percentage>` token.
fn parse_length_or_percentage(token: &str) -> Option<Margin> {
    let numeric = token
        .char_indices()
        .take_while(|&(i, c)| {
            c.is_ascii_digit()
                || c == '.'
                || ((c == '+' || c == '-') && (i == 0 || token[..i].ends_with(['e', 'E'])))
                || ((c == 'e' || c == 'E')
                    && token[i + 1..]
                        .starts_with(|n: char| n.is_ascii_digit() || n == '+' || n == '-'))
        })
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    let (number, unit) = token.split_at(numeric);
    if !number.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    if unit == "%" {
        return Some(Margin {
            value,
            percent: true,
        });
    }
    let px = match unit.to_ascii_lowercase().as_str() {
        "px" => 1.0,
        "in" => 96.0,
        "cm" => 96.0 / 2.54,
        "mm" => 96.0 / 25.4,
        "q" => 96.0 / 101.6,
        "pt" => 96.0 / 72.0,
        "pc" => 16.0,
        _ => return None,
    };
    Some(Margin {
        value: value * px,
        percent: false,
    })
}

fn serialize_margin(m: &[Margin; 4]) -> String {
    m.iter()
        .map(|m| format!("{}{}", m.value, if m.percent { "%" } else { "px" }))
        .collect::<Vec<_>>()
        .join(" ")
}

/// An IntersectionObserverRegistration (one per observed target).
pub struct Registration {
    pub target: NodeId,
    pub previous_threshold_index: i64,
    pub previous_is_intersecting: bool,
    pub previous_is_visible: bool,
    pub last_update_time: Option<f64>,
}

/// An `IntersectionObserver`.
pub struct ObserverData {
    pub callback: super::held::Held<v8::Object>,
    /// The root option; `None` for an implicit root observer.
    pub root: Option<NodeId>,
    pub root_margin: [Margin; 4],
    pub scroll_margin: [Margin; 4],
    pub thresholds: Vec<f64>,
    pub delay: i32,
    pub track_visibility: bool,
    /// The [[ObservationTargets]] with their registrations, in `observe()` order.
    pub registrations: Vec<Registration>,
    /// [[QueuedEntries]].
    pub queued: Vec<Handle>,
}

/// An `IntersectionObserverEntry`.
pub struct EntryData {
    pub time: f64,
    pub root_bounds: Option<Handle>,
    pub bounding_client_rect: Handle,
    pub intersection_rect: Handle,
    pub is_intersecting: bool,
    pub is_visible: bool,
    pub intersection_ratio: f64,
    pub target: NodeId,
}

/// The event loop's intersection and resize observers.
#[derive(Default)]
pub struct IntersectionState {
    /// Every IntersectionObserver, in creation order.
    pub observers: Vec<Handle>,
    /// Every ResizeObserver, in creation order (each belongs to its document's [[resizeObservers]]).
    pub resize_observers: Vec<Handle>,
    /// Documents whose IntersectionObserverTaskQueued flag is set.
    pub task_queued: rustc_hash::FxHashSet<NodeId>,
}

/// The edge-inclusive intersection of two rectangles (`None` when they neither overlap nor touch).
fn intersect(a: &Rect, b: &Rect) -> Option<Rect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right >= left && bottom >= top).then(|| Rect::read_only(left, top, right - left, bottom - top))
}

/// A rectangle grown by a margin (top, right, bottom, left). The specification resolves every
/// percentage against the width; web-platform-tests (root-margin.html) and engines resolve the top
/// and bottom ones against the height, which is what this does.
fn expand(r: &Rect, m: &[Margin; 4]) -> Rect {
    let (w, h) = (r.width, r.height);
    let (top, right, bottom, left) = (
        m[0].resolve(h),
        m[1].resolve(w),
        m[2].resolve(h),
        m[3].resolve(w),
    );
    Rect::read_only(
        r.x - left,
        r.y - top,
        (r.width + left + right).max(0.0),
        (r.height + top + bottom).max(0.0),
    )
}

impl Engine {
    fn io<R>(&self, h: Handle, f: impl FnOnce(&mut ObserverData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::IntersectionObserver(o) => f(o),
            _ => panic!("not an IntersectionObserver"),
        }
    }

    fn io_entry<R>(&self, h: Handle, f: impl FnOnce(&EntryData) -> R) -> R {
        match &*self.obj(h) {
            Object::IntersectionObserverEntry(e) => f(e),
            _ => panic!("not an IntersectionObserverEntry"),
        }
    }

    /// The document an observer's notifications belong to: its root's node document, or the
    /// top-level document for an implicit root.
    fn io_document(&self, root: Option<NodeId>) -> Option<NodeId> {
        match root {
            Some(r) => {
                let dom = self.dom();
                if !dom.is_alive(r) {
                    return None;
                }
                Some(if dom.is_document(r) {
                    r
                } else {
                    dom.node_document(r)
                })
            }
            None => self.document(),
        }
    }

    /// Whether some observed target has not been observed by a rendering update yet (a document
    /// with pending initial targets must not skip its rendering update).
    pub(crate) fn intersection_pending(&self) -> bool {
        let state = self.intersection.borrow();
        if state.observers.is_empty() {
            return false;
        }
        let objects = self.objects.borrow();
        state.observers.iter().any(|h| {
            matches!(&objects[h.get() as usize - 1], Object::IntersectionObserver(o)
                if o.registrations.iter().any(|r| r.last_update_time.is_none()))
        })
    }

    /// The root intersection rectangle of an observer (before the root margin), in the root
    /// document's viewport coordinates.
    fn root_intersection_rect(&self, root: Option<NodeId>) -> Rect {
        let zero = Rect::read_only(0.0, 0.0, 0.0, 0.0);
        let viewport = |doc: NodeId| {
            self.view_viewport_size(doc)
                .map(|(w, h)| Rect::read_only(0.0, 0.0, w, h))
                .unwrap_or(zero)
        };
        match root {
            None => self.document().map(viewport).unwrap_or(zero),
            Some(r) if self.dom().is_document(r) => viewport(r),
            Some(r) => match self.view_clip_box(r) {
                Some((padding, true)) => padding,
                Some(_) => {
                    let b = self.view_bounding_rect(r);
                    Rect::read_only(b.x, b.y, b.width, b.height)
                }
                None => zero,
            },
        }
    }

    /// Intersection Observer "compute the intersection" of `target` with an observer's root:
    /// `None` when the target is clipped away or does not reach the root rectangle. The result is in
    /// the target document's viewport coordinates. `target_rect` is the target's bounding client
    /// rect (step 1, "the result of getting the bounding box for target").
    fn compute_intersection(
        &self,
        target: NodeId,
        target_rect: &Rect,
        root: Option<NodeId>,
        root_rect: &Rect,
        scroll_margin: &[Margin; 4],
    ) -> Option<Rect> {
        let mut rect = Rect::read_only(
            target_rect.x,
            target_rect.y,
            target_rect.width,
            target_rect.height,
        );
        let root_element = root.filter(|&r| !self.dom().is_document(r));
        let root_document = self.io_document(root)?;
        let (mut el, mut doc) = (target, self.dom().node_document(target));
        let (mut dx, mut dy) = (0.0, 0.0);
        loop {
            let stop = root_element.filter(|_| doc == root_document);
            let (clips, _) = self.view_containing_clips(el, stop)?;
            for c in clips {
                let clip = if c.scroll_container {
                    expand(&c.rect, scroll_margin)
                } else {
                    c.rect
                };
                rect = intersect(&rect, &clip)?;
            }
            if doc == root_document {
                break;
            }
            // Leave a nested document: clip to its viewport and map into its container's.
            let (w, h) = self.view_viewport_size(doc)?;
            rect = intersect(&rect, &Rect::read_only(0.0, 0.0, w, h))?;
            let container = self.rendered_container(doc)?;
            let cb = self.view_content_box(container)?;
            rect.x += cb.x;
            rect.y += cb.y;
            dx += cb.x;
            dy += cb.y;
            el = container;
            doc = self.dom().node_document(container);
        }
        let mut rect = intersect(&rect, root_rect)?;
        rect.x -= dx;
        rect.y -= dy;
        Some(rect)
    }

    /// Whether `target` can be observed against `root` (steps 5-6 of "update intersection
    /// observations"): it is in the root's document (or, for the implicit root, in a document
    /// rendered under the top-level one) and, for an element root, on a containing block chain that
    /// passes through it.
    fn io_target_applies(&self, target: NodeId, root: Option<NodeId>) -> bool {
        let (connected, mut doc) = {
            let dom = self.dom();
            if !dom.is_alive(target) {
                return false;
            }
            (dom.is_connected(target), dom.node_document(target))
        };
        if !connected {
            return false;
        }
        match root {
            None => {
                let Some(top) = self.document() else {
                    return false;
                };
                loop {
                    if doc == top {
                        return true;
                    }
                    match self.rendered_container(doc) {
                        Some(c) => doc = self.dom().node_document(c),
                        None => return false,
                    }
                }
            }
            Some(r) if self.dom().is_document(r) => doc == r,
            Some(r) => {
                doc == self.dom().node_document(r)
                    && self
                        .view_containing_clips(target, Some(r))
                        .is_some_and(|(_, reached)| reached)
            }
        }
    }

    /// Intersection Observer §3.2.8 "run the update intersection observations steps", for every
    /// observer of the event loop at once.
    pub(crate) fn update_intersection_observations(&self, cx: &mut Cx) {
        let observers = self.intersection.borrow().observers.clone();
        if observers.is_empty() {
            return;
        }
        let time = self.now();
        for io in observers {
            let (root, root_margin, scroll_margin, thresholds, delay, targets) = self.io(io, |o| {
                (
                    o.root,
                    o.root_margin,
                    o.scroll_margin,
                    o.thresholds.clone(),
                    o.delay,
                    o.registrations.iter().map(|r| r.target).collect::<Vec<_>>(),
                )
            });
            if targets.is_empty() {
                continue;
            }
            // 2.1 rootBounds: the root intersection rectangle with the root margin applied.
            let root_bounds = expand(&self.root_intersection_rect(root), &root_margin);
            for target in targets {
                // 2.2.2-3 The delay.
                let skip = self.io(io, |o| {
                    let Some(reg) = o.registrations.iter_mut().find(|r| r.target == target) else {
                        return true;
                    };
                    if reg
                        .last_update_time
                        .is_some_and(|last| time - last < delay as f64)
                    {
                        return true;
                    }
                    reg.last_update_time = Some(time);
                    false
                });
                if skip {
                    continue;
                }
                // 4-10.
                let zero = Rect::read_only(0.0, 0.0, 0.0, 0.0);
                // A target that is not rendered or not under the root is reported with empty
                // rectangles (rootBounds included).
                // (the target's boxes are found once per target and update: each read maps them to
                // the viewport through the layout tree)
                let rects = if self.io_target_applies(target, root) {
                    self.view_client_rects(target)
                } else {
                    Vec::new()
                };
                let (target_rect, intersection, entry_root_bounds) = if !rects.is_empty() {
                    let b = super::view::bounding_rect(rects);
                    let target_rect = Rect::read_only(b.x, b.y, b.width, b.height);
                    let intersection = self.compute_intersection(
                        target,
                        &target_rect,
                        root,
                        &root_bounds,
                        &scroll_margin,
                    );
                    (target_rect, intersection, root_bounds)
                } else {
                    (zero, None, zero)
                };
                // 11. A target that reaches the root rectangle intersects, even with zero area.
                let is_intersecting = intersection.is_some();
                let intersection_rect = intersection.unwrap_or(zero);
                // 12-13.
                let target_area = target_rect.width * target_rect.height;
                let ratio = if target_area > 0.0 {
                    (intersection_rect.width * intersection_rect.height / target_area).min(1.0)
                } else if is_intersecting {
                    1.0
                } else {
                    0.0
                };
                let threshold_index = thresholds
                    .iter()
                    .position(|&t| t > ratio)
                    .unwrap_or(thresholds.len()) as i64;
                // 14. isVisible (Intersection Observer 2's visibility algorithm is not implemented).
                let is_visible = false;
                // 15-17.
                let changed = self.io(io, |o| {
                    let Some(reg) = o.registrations.iter_mut().find(|r| r.target == target) else {
                        return false;
                    };
                    let changed = reg.previous_threshold_index != threshold_index
                        || reg.previous_is_intersecting != is_intersecting
                        || reg.previous_is_visible != is_visible;
                    reg.previous_threshold_index = threshold_index;
                    reg.previous_is_intersecting = is_intersecting;
                    reg.previous_is_visible = is_visible;
                    changed
                });
                if changed {
                    let entry = EntryData {
                        time,
                        root_bounds: Some(self.alloc(Object::DomRect(entry_root_bounds))),
                        bounding_client_rect: self.alloc(Object::DomRect(target_rect)),
                        intersection_rect: self.alloc(Object::DomRect(intersection_rect)),
                        is_intersecting,
                        is_visible,
                        intersection_ratio: ratio,
                        target,
                    };
                    self.queue_intersection_entry(io, root, entry);
                }
            }
        }
        let _ = cx;
    }

    /// "Queue an IntersectionObserverEntry".
    fn queue_intersection_entry(&self, io: Handle, root: Option<NodeId>, entry: EntryData) {
        let h = self.alloc(Object::IntersectionObserverEntry(Box::new(entry)));
        self.io(io, |o| o.queued.push(h));
        if let Some(doc) = self.io_document(root) {
            self.queue_intersection_observer_task(doc);
        }
    }

    /// "Queue an intersection observer task" for a document.
    fn queue_intersection_observer_task(&self, doc: NodeId) {
        if !self.intersection.borrow_mut().task_queued.insert(doc) {
            return;
        }
        self.queue_task(TaskSource::IntersectionObserver, move |e, cx| {
            e.notify_intersection_observers(cx, doc);
        });
    }

    /// "Notify intersection observers" of a document.
    fn notify_intersection_observers(&self, cx: &mut Cx, doc: NodeId) {
        let observers = {
            let mut state = self.intersection.borrow_mut();
            state.task_queued.remove(&doc);
            state.observers.clone()
        };
        for io in observers {
            let root = self.io(io, |o| o.root);
            if self.io_document(root) != Some(doc) {
                continue;
            }
            let (queue, callback) =
                self.io(io, |o| (std::mem::take(&mut o.queued), o.callback.clone()));
            if queue.is_empty() {
                continue;
            }
            let Some(callback) = callback.get(cx.scope()).map(IntersectionObserverCallback) else {
                continue;
            };
            // Invoke the callback in its realm, reporting what it throws.
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
                if callback.invoke(&mut cx2, Some(&io), queue, io).is_err() {
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
    }

    /// Freed nodes leave the observers' target lists.
    pub(crate) fn intersection_nodes_freed(&self, freed: &snowlark_dom::NodeSet) {
        let observers = self.intersection.borrow().observers.clone();
        let mut objects = self.objects.borrow_mut();
        for h in observers {
            if let Some(Object::IntersectionObserver(o)) = objects.get_mut(h.get() as usize - 1) {
                o.registrations.retain(|r| !freed.contains(&r.target));
            }
        }
    }
}

impl IntersectionObserverImpl for Engine {
    fn root(&self, this: Handle) -> Option<ElementOrDocument> {
        let root = self.io(this, |o| o.root)?;
        Some(if self.dom().is_document(root) {
            ElementOrDocument::Document(node_handle(root))
        } else {
            ElementOrDocument::Element(node_handle(root))
        })
    }
    fn root_margin(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.io(this, |o| serialize_margin(&o.root_margin)))
    }
    fn scroll_margin(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.io(this, |o| serialize_margin(&o.scroll_margin)))
    }
    fn thresholds(&self, this: Handle) -> Vec<f64> {
        self.io(this, |o| o.thresholds.clone())
    }
    fn delay(&self, this: Handle) -> i32 {
        self.io(this, |o| o.delay)
    }
    fn track_visibility(&self, this: Handle) -> bool {
        self.io(this, |o| o.track_visibility)
    }

    /// `observe(target)`.
    fn observe(&self, _cx: &mut Cx, this: Handle, target: Handle) -> rt::Result<()> {
        let target = self.n(target);
        self.io(this, |o| {
            if o.registrations.iter().any(|r| r.target == target) {
                return;
            }
            o.registrations.push(Registration {
                target,
                previous_threshold_index: -1,
                previous_is_intersecting: false,
                previous_is_visible: false,
                last_update_time: None,
            });
        });
        Ok(())
    }

    /// `unobserve(target)`.
    fn unobserve(&self, _cx: &mut Cx, this: Handle, target: Handle) -> rt::Result<()> {
        let target = self.n(target);
        self.io(this, |o| o.registrations.retain(|r| r.target != target));
        Ok(())
    }

    /// `disconnect()`.
    fn disconnect(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.io(this, |o| o.registrations.clear());
        Ok(())
    }

    /// `takeRecords()`.
    fn take_records(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        Ok(self.io(this, |o| std::mem::take(&mut o.queued)))
    }

    /// "Initialize a new IntersectionObserver".
    fn constructor(
        &self,
        _cx: &mut Cx,
        callback: IntersectionObserverCallback,
        options: IntersectionObserverInit,
    ) -> rt::Result<rt::Handle> {
        let root = options.root.map(|r| match r {
            ElementOrDocument::Element(h) | ElementOrDocument::Document(h) => self.n(h),
        });
        let syntax = |what: &str| {
            Error::Dom(
                ExceptionName::SyntaxError,
                format!("{what} must be one to four lengths in px or percentages").into(),
            )
        };
        let root_margin = parse_margin(&options.root_margin).ok_or_else(|| syntax("rootMargin"))?;
        let scroll_margin =
            parse_margin(&options.scroll_margin).ok_or_else(|| syntax("scrollMargin"))?;
        let mut thresholds = match options.threshold {
            DoubleOrDoubleSequence::Double(t) => vec![t],
            DoubleOrDoubleSequence::DoubleSequence(l) => l,
        };
        if thresholds.iter().any(|&t| !(0.0..=1.0).contains(&t)) {
            return Err(Error::Range(
                "Threshold values must be between 0 and 1".into(),
            ));
        }
        thresholds.sort_by(f64::total_cmp);
        if thresholds.is_empty() {
            thresholds.push(0.0);
        }
        let mut delay = options.delay;
        if options.track_visibility && delay < 100 {
            delay = 100;
        }
        let h = self.alloc(Object::IntersectionObserver(Box::new(ObserverData {
            callback: super::held::Held::object(callback.0),
            root,
            root_margin,
            scroll_margin,
            thresholds,
            delay,
            track_visibility: options.track_visibility,
            registrations: Vec::new(),
            queued: Vec::new(),
        })));
        self.intersection.borrow_mut().observers.push(h);
        Ok(h)
    }
}

impl IntersectionObserverEntryImpl for Engine {
    fn time(&self, this: Handle) -> f64 {
        self.io_entry(this, |e| e.time)
    }
    fn root_bounds(&self, this: Handle) -> Option<Handle> {
        self.io_entry(this, |e| e.root_bounds)
    }
    fn bounding_client_rect(&self, this: Handle) -> Handle {
        self.io_entry(this, |e| e.bounding_client_rect)
    }
    fn intersection_rect(&self, this: Handle) -> Handle {
        self.io_entry(this, |e| e.intersection_rect)
    }
    fn is_intersecting(&self, this: Handle) -> bool {
        self.io_entry(this, |e| e.is_intersecting)
    }
    fn is_visible(&self, this: Handle) -> bool {
        self.io_entry(this, |e| e.is_visible)
    }
    fn intersection_ratio(&self, this: Handle) -> f64 {
        self.io_entry(this, |e| e.intersection_ratio)
    }
    fn target(&self, this: Handle) -> Handle {
        node_handle(self.io_entry(this, |e| e.target))
    }
    /// `new IntersectionObserverEntry(init)`: an entry holding the given values.
    fn constructor(&self, _cx: &mut Cx, init: IntersectionObserverEntryInit) -> rt::Result<Handle> {
        let rect = |r: &snowlark_bindings::generated::DOMRectInit| {
            self.alloc(Object::DomRect(Rect::read_only(
                r.x, r.y, r.width, r.height,
            )))
        };
        let entry = EntryData {
            time: init.time,
            root_bounds: init.root_bounds.as_ref().map(rect),
            bounding_client_rect: rect(&init.bounding_client_rect),
            intersection_rect: rect(&init.intersection_rect),
            is_intersecting: init.is_intersecting,
            is_visible: init.is_visible,
            intersection_ratio: init.intersection_ratio,
            target: self.n(init.target),
        };
        Ok(self.alloc(Object::IntersectionObserverEntry(Box::new(entry))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(v: f64) -> Margin {
        Margin {
            value: v,
            percent: false,
        }
    }

    #[test]
    fn margins_parse_and_serialize() {
        let m = parse_margin("10% 20px").unwrap();
        assert_eq!(serialize_margin(&m), "10% 20px 10% 20px");
        assert_eq!(
            serialize_margin(&parse_margin(" ").unwrap()),
            "0px 0px 0px 0px"
        );
        assert_eq!(parse_margin("1in").unwrap()[0], px(96.0));
        assert_eq!(parse_margin("-1.5e1px").unwrap()[0], px(-15.0));
        for bad in [
            "1",
            "2em",
            "auto",
            "calc(1px + 2px)",
            "1px !important",
            "1px 1px 1px 1px 1px",
            "px",
            "1e",
        ] {
            assert!(parse_margin(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn intersection_is_edge_inclusive() {
        let a = Rect::read_only(0.0, 0.0, 10.0, 10.0);
        let b = Rect::read_only(10.0, 0.0, 5.0, 5.0);
        let i = intersect(&a, &b).unwrap();
        assert_eq!((i.x, i.width, i.height), (10.0, 0.0, 5.0));
        assert!(intersect(&a, &Rect::read_only(11.0, 0.0, 1.0, 1.0)).is_none());
        let e = expand(&a, &parse_margin("10%").unwrap());
        assert_eq!((e.x, e.y, e.width, e.height), (-1.0, -1.0, 12.0, 12.0));
        let tall = expand(
            &Rect::read_only(0.0, 0.0, 10.0, 20.0),
            &parse_margin("10%").unwrap(),
        );
        assert_eq!((tall.y, tall.height, tall.width), (-2.0, 24.0, 12.0));
    }
}
