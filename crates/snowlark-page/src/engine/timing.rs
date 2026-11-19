// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The timing entries a document's loading, painting and input make, from the engine's real events:
//!
//! - Resource Timing: a [`ResourceObserver`] on the page's network client sees every fetch it starts
//!   and the events the page takes from it (with the network layer's [`FetchTiming`]); finished
//!   fetches become `PerformanceResourceTiming` entries of their client's global ("mark resource
//!   timing", with the timing allow check and the resource timing buffer).
//! - Navigation Timing Level 2: a document made from a response gets a `PerformanceNavigationTiming`
//!   entry from the response's timing, its body's end and its document load timing info; Level 1's
//!   `performance.timing` and `performance.navigation` read the same.
//! - Server Timing: the `Server-Timing` response header's metrics.
//! - Paint Timing: `first-paint` and `first-contentful-paint` at the rendering update that first paints
//!   anything / contentful content.
//! - Event Timing: trusted input events dispatched at nodes give `event` entries (with the duration up
//!   to the next rendering update) and the first one a `first-input` entry.
//!
//! The engine has one clock per event loop: the top-level document's time origin is its navigation's
//! start ([`Engine::rebase_time_origin`]); a frame's entries are relative to that same origin.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use snowlark_bindings::generated::{
    NavigationTimingType, PaintTimingMixinImpl, PerformanceEventTimingImpl,
    PerformanceNavigationImpl, PerformanceNavigationTimingImpl, PerformancePaintTimingImpl,
    PerformanceResourceTimingImpl, PerformanceServerTimingImpl, PerformanceTimingImpl,
    RenderBlockingStatusType,
};
use snowlark_bindings::rt::{Cx, DomStr, Handle};
use snowlark_dom::{local_name, ns, NodeId};
use snowlark_net::observe::FetchObserver;
use snowlark_net::{Client, Destination, FetchEvent, FetchTiming, Request, ResponseHead, Url};

use super::timeline::{EntryData, EntryExtra, EntryType};
use super::{handle_node, node_handle, Engine, Object};
use crate::event_loop::TaskSource;

/// What a `PerformanceResourceTiming` (or the resource part of a `PerformanceNavigationTiming`)
/// reports, in milliseconds since the time origin (0 for what did not happen or is not exposed).
#[derive(Clone, Debug, Default)]
pub struct ResourceTiming {
    pub initiator_type: String,
    pub next_hop_protocol: String,
    pub redirect_start: f64,
    pub redirect_end: f64,
    pub fetch_start: f64,
    pub domain_lookup_start: f64,
    pub domain_lookup_end: f64,
    pub connect_start: f64,
    pub connect_end: f64,
    pub secure_connection_start: f64,
    pub request_start: f64,
    pub response_start: f64,
    pub response_end: f64,
    pub transfer_size: u64,
    pub encoded_body_size: u64,
    pub decoded_body_size: u64,
    pub response_status: u16,
    pub content_type: String,
    pub content_encoding: String,
    pub render_blocking: bool,
    /// (name, duration, description) of each `Server-Timing` metric.
    pub server_timing: Vec<(String, f64, String)>,
    /// Navigation Timing: the redirects of a same-origin redirect chain (0 otherwise).
    pub redirect_count: u16,
}

/// What a `PerformanceEventTiming` reports besides its PerformanceEntry attributes.
#[derive(Clone, Debug, Default)]
pub struct EventTiming {
    pub processing_start: f64,
    pub processing_end: f64,
    pub cancelable: bool,
    pub target: Option<NodeId>,
    pub interaction_id: u64,
}

/// A response head as Resource Timing needs it.
#[derive(Clone)]
struct HeadInfo {
    status: u16,
    url_list: Vec<Url>,
    timing: FetchTiming,
    protocol: String,
    content_type: String,
    content_encoding: String,
    content_length: Option<u64>,
    timing_allow_origin: Option<String>,
    server_timing: Option<String>,
    cors_same_origin: bool,
    /// `Content-Disposition: attachment`: a download, which a navigation does not report.
    download: bool,
}

impl HeadInfo {
    fn of(h: &ResponseHead) -> HeadInfo {
        let essence = snowlark_net::mime::extract_mime_type(&h.headers)
            .map(|m| m.essence())
            .unwrap_or_default();
        HeadInfo {
            status: h.status,
            url_list: h.url_list.clone(),
            timing: h.timing,
            protocol: h.connection.protocol.clone(),
            content_type: essence,
            content_encoding: h
                .headers
                .get("content-encoding")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase(),
            content_length: h
                .headers
                .get("content-length")
                .and_then(|v| v.trim().parse().ok()),
            timing_allow_origin: h.headers.get("timing-allow-origin"),
            server_timing: h.headers.get("server-timing"),
            download: h.headers.get("content-disposition").is_some_and(|v| {
                v.trim_start()
                    .to_ascii_lowercase()
                    .starts_with("attachment")
            }) || essence_is_download(&h.headers),
            cors_same_origin: !matches!(
                h.response_type,
                snowlark_net::ResponseType::Opaque | snowlark_net::ResponseType::OpaqueRedirect
            ),
        }
    }
}

/// Whether a navigation response's type is one this engine does not render (it would be a
/// download): `application/octet-stream`.
fn essence_is_download(headers: &snowlark_net::HeaderList) -> bool {
    snowlark_net::mime::extract_mime_type(headers)
        .is_some_and(|m| m.essence() == "application/octet-stream")
}

/// A fetch the [`ResourceObserver`] follows.
struct FetchRecord {
    url: Url,
    initiator_type: String,
    render_blocking: bool,
    /// A frame's navigation request.
    navigation: bool,
    client: Option<Arc<Client>>,
    start: Instant,
    head: Option<HeadInfo>,
    decoded: u64,
    end: Option<Instant>,
}

#[derive(Default)]
struct ObserverState {
    live: rustc_hash::FxHashMap<u64, FetchRecord>,
    done: Vec<FetchRecord>,
}

/// Resource Timing's view of the page's network client: every fetch started through it and the
/// events the page takes from it, passed on to the client's other observer (the developer tools).
pub struct ResourceObserver {
    inner: Option<Arc<dyn FetchObserver>>,
    next: AtomicU64,
    state: Mutex<ObserverState>,
}

impl ResourceObserver {
    pub fn new(inner: Option<Arc<dyn FetchObserver>>) -> ResourceObserver {
        ResourceObserver {
            inner,
            next: AtomicU64::new(0),
            state: Mutex::new(ObserverState::default()),
        }
    }

    fn take_done(&self) -> Vec<FetchRecord> {
        std::mem::take(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()).done)
    }
}

/// HTML's initiator type for a request that did not say: from its destination.
fn initiator_for(request: &Request) -> String {
    if !request.initiator_type.is_empty() {
        return request.initiator_type.clone();
    }
    match request.destination {
        Destination::Script => "script",
        Destination::Style => "link",
        Destination::Image => "img",
        Destination::Font => "css",
        Destination::Iframe => "iframe",
        Destination::Frame => "frame",
        Destination::Video => "video",
        Destination::Audio => "audio",
        Destination::Track => "track",
        Destination::Embed => "embed",
        Destination::Object => "object",
        _ => "other",
    }
    .to_string()
}

impl FetchObserver for ResourceObserver {
    fn started(&self, request: &Request) -> u64 {
        let id = match &self.inner {
            Some(o) => o.started(request),
            None => self.next.fetch_add(1, Ordering::Relaxed) + 1,
        };
        let url = request.url_list[0].clone();
        // Resource Timing reports HTTP(S) fetches; a top-level navigation is Navigation Timing's.
        if !matches!(url.scheme(), "http" | "https") || request.destination == Destination::Document
        {
            return id;
        }
        let record = FetchRecord {
            url,
            initiator_type: initiator_for(request),
            render_blocking: request.render_blocking,
            navigation: request.is_navigation_request(),
            client: request.client.clone(),
            start: Instant::now(),
            head: None,
            decoded: 0,
            end: None,
        };
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live
            .insert(id, record);
        id
    }

    fn event(&self, id: u64, event: &FetchEvent) {
        if let Some(o) = &self.inner {
            o.event(id, event);
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let finished = match event {
            FetchEvent::Head(h) => {
                if let Some(r) = st.live.get_mut(&id) {
                    r.head = Some(HeadInfo::of(h));
                }
                false
            }
            FetchEvent::Body(b) => {
                if let Some(r) = st.live.get_mut(&id) {
                    r.decoded += b.len() as u64;
                }
                false
            }
            FetchEvent::Done | FetchEvent::Error(_) => true,
            FetchEvent::Violation(_) => false,
        };
        if finished {
            if let Some(mut r) = st.live.remove(&id) {
                r.end = Some(Instant::now());
                st.done.push(r);
            }
        }
    }

    fn canceled(&self, id: u64) {
        if let Some(o) = &self.inner {
            o.canceled(id);
        }
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live
            .remove(&id);
    }
}

/// Server Timing §3 "parse the Server-Timing header field": `name;dur=1.2;desc="text"`, comma
/// separated.
pub(crate) fn parse_server_timing(value: &str) -> Vec<(String, f64, String)> {
    let mut out = Vec::new();
    for metric in value.split(',') {
        let mut parts = metric.split(';');
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic() && b != b'"') {
            continue;
        }
        let (mut dur, mut desc) = (None::<f64>, None::<String>);
        for p in parts {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(v);
            match k.trim().to_ascii_lowercase().as_str() {
                "dur" if dur.is_none() => dur = Some(v.parse().unwrap_or(0.0)),
                "desc" if desc.is_none() => desc = Some(v.to_string()),
                _ => {}
            }
        }
        out.push((
            name.to_string(),
            dur.unwrap_or(0.0),
            desc.unwrap_or_default(),
        ));
    }
    out
}

/// The initiator types of fetches elements make for the document (not `fetch()`, XHR, beacons).
const ELEMENT_INITIATORS: &[&str] = &[
    "link", "css", "img", "image", "script", "video", "audio", "track",
];

/// Event Timing's event types (§2.1 "the events whose timing is reported").
const EVENT_TIMING_TYPES: &[&str] = &[
    "auxclick",
    "click",
    "contextmenu",
    "dblclick",
    "mousedown",
    "mouseenter",
    "mouseleave",
    "mouseout",
    "mouseover",
    "mouseup",
    "pointerover",
    "pointerenter",
    "pointerdown",
    "pointerup",
    "pointercancel",
    "pointerout",
    "pointerleave",
    "gotpointercapture",
    "lostpointercapture",
    "touchstart",
    "touchend",
    "touchcancel",
    "keydown",
    "keypress",
    "keyup",
    "beforeinput",
    "input",
    "compositionstart",
    "compositionupdate",
    "compositionend",
    "dragstart",
    "dragend",
    "dragenter",
    "dragleave",
    "dragover",
    "drop",
];

/// The engine's timing bookkeeping.
#[derive(Default)]
pub struct TimingState {
    /// The page's resource observer (absent without a network client).
    pub observer: Option<Arc<ResourceObserver>>,
    /// Each document's navigation timing entry.
    pub navigations: rustc_hash::FxHashMap<NodeId, Handle>,
    /// Documents whose first paint / first contentful paint was reported.
    pub first_paint: rustc_hash::FxHashSet<NodeId>,
    pub first_contentful_paint: rustc_hash::FxHashSet<NodeId>,
    /// Event Timing entries waiting for the next rendering update (their duration ends there).
    /// (They are made into objects then: until then nothing in the store refers to them.)
    pub pending_events: Vec<EntryData>,
    /// The (window, URL) of the element resources reported (see `mark_resource_timing`).
    pub element_resources: HashSet<(Handle, String)>,
    /// The (document, URL) of the `link` preloads whose resource is not reported twice yet: the
    /// preload and the fetch that takes its response report it once (`true`: once already).
    pub link_preloads: rustc_hash::FxHashMap<(NodeId, String), bool>,
    /// Windows whose first input was reported.
    pub first_input: rustc_hash::FxHashSet<Handle>,
    /// The interaction id of the key press or pointer interaction in progress, by window.
    pub interaction: rustc_hash::FxHashMap<Handle, u64>,
}

impl Engine {
    /// The network client the page uses, wrapped so Resource Timing sees its fetches.
    pub(crate) fn observe_resource_timing(
        &self,
        net: snowlark_net::NetClient,
    ) -> snowlark_net::NetClient {
        let observer = Arc::new(ResourceObserver::new(net.observer()));
        self.timing.borrow_mut().observer = Some(observer.clone());
        net.observed(observer)
    }

    /// An instant on the high resolution time line of the event loop's time origin, coarsened as
    /// `performance.now()` is ([`crate::event_loop::coarsen_time`]).
    pub(crate) fn time_at(&self, at: Instant) -> f64 {
        let origin = self.event_loop.borrow().origin;
        let ms = if at >= origin {
            (at - origin).as_secs_f64() * 1000.0
        } else {
            -((origin - at).as_secs_f64() * 1000.0)
        };
        crate::event_loop::coarsen_time(ms)
    }

    /// Moves the event loop's time origin to `at` (a later instant): the top-level document made
    /// from a navigation's response gets the navigation's start as its time origin (HR Time §4,
    /// Navigation Timing §4). Timers and the last frame time keep their instants.
    pub(crate) fn rebase_time_origin(&self, at: Instant) {
        let mut el = self.event_loop.borrow_mut();
        if at <= el.origin {
            return;
        }
        let delta = at - el.origin;
        let delta_ms = delta.as_secs_f64() * 1000.0;
        let delta_us = delta.as_micros() as u64;
        el.origin = at;
        el.origin_unix_ms += delta_ms;
        let schedule = std::mem::take(&mut el.schedule);
        el.schedule = schedule
            .into_iter()
            .map(|((us, seq), id)| ((us.saturating_sub(delta_us), seq), id))
            .collect();
        el.last_frame -= delta_ms;
    }

    // ----- Resource Timing ---------------------------------------------------------------------

    /// Turns the fetches that finished since the last call into resource timing entries of their
    /// clients' globals.
    pub(crate) fn drain_resource_timings(&self) {
        let Some(observer) = self.timing.borrow().observer.clone() else {
            return;
        };
        for record in observer.take_done() {
            self.mark_resource_timing(record);
        }
    }

    /// The document whose request client `client` is.
    fn document_of_client(&self, client: &Arc<Client>) -> Option<NodeId> {
        let policy = self.policy.borrow();
        policy
            .docs
            .iter()
            .find_map(|(doc, p)| {
                p.client
                    .as_ref()
                    .is_some_and(|(c, _)| Arc::ptr_eq(c, client))
                    .then_some(*doc)
            })
            .or_else(|| {
                // (a client the document had when the fetch started)
                policy
                    .retired_clients
                    .iter()
                    .find(|(c, _)| std::ptr::eq(c.as_ptr(), Arc::as_ptr(client)))
                    .map(|(_, doc)| *doc)
            })
    }

    /// Resource Timing §4.6 "mark resource timing" for a finished fetch.
    fn mark_resource_timing(&self, r: FetchRecord) {
        if r.navigation && r.head.as_ref().is_some_and(|h| h.download) {
            return;
        }
        let Some(doc) = r.client.as_ref().and_then(|c| self.document_of_client(c)) else {
            return;
        };
        if !self.dom().is_alive(doc) {
            return;
        }
        let Some(window) = self.window_of_document(doc) else {
            return;
        };
        // A `link` preload and the first fetch of its URL (which took the preloaded response) are
        // one resource, reported once, as the preload's (`link`), when the first of the two ends.
        let mut r = r;
        let preload = (doc, r.url.to_string());
        let state = self.timing.borrow().link_preloads.get(&preload).copied();
        match state {
            Some(false) => {
                self.timing.borrow_mut().link_preloads.insert(preload, true);
                r.initiator_type = "link".into();
            }
            Some(true) => {
                self.timing.borrow_mut().link_preloads.remove(&preload);
                return;
            }
            None => {}
        }
        // A document's elements share one resource per URL (HTML's list of available images,
        // the stylesheet and script reuse of engines' memory caches): an element that fetches a
        // URL the document already loaded for an element reports no second entry.
        if ELEMENT_INITIATORS.contains(&r.initiator_type.as_str())
            && !self
                .timing
                .borrow_mut()
                .element_resources
                .insert((window, r.url.to_string()))
        {
            return;
        }
        let origin = r
            .client
            .as_ref()
            .map(|c| c.origin.clone())
            .expect("a client");
        let start = self.time_at(r.start);
        let end = r.end.map_or(start, |e| self.time_at(e));
        let timing = match &r.head {
            Some(h) => self.resource_timing_of(h, &origin, start, end, r.decoded),
            None => ResourceTiming {
                fetch_start: start,
                response_end: end,
                ..Default::default()
            },
        };
        let timing = ResourceTiming {
            initiator_type: r.initiator_type.clone(),
            render_blocking: r.render_blocking,
            ..timing
        };
        let entry = self.alloc(Object::PerformanceEntry(Box::new(EntryData {
            entry_type: EntryType::Resource,
            name: r.url.to_string(),
            start_time: start,
            duration: end - start,
            id: 0,
            navigation_id: 0,
            detail: None,
            global: window,
            extra: EntryExtra::Resource(Box::new(timing)),
        })));
        self.queue_performance_entry(entry);
        let perf = self.performance_of(window);
        self.add_resource_timing_entry(perf, entry);
    }

    /// The resource timing of a response (Resource Timing §4.6, with Fetch's "timing allow check"
    /// and "create an opaque timing info" for responses that fail it).
    fn resource_timing_of(
        &self,
        h: &HeadInfo,
        origin: &snowlark_net::Origin,
        start: f64,
        end: f64,
        decoded: u64,
    ) -> ResourceTiming {
        let same_origin = h.url_list.iter().all(|u| &u.origin() == origin);
        let serialized = origin.ascii_serialization();
        let allowed = same_origin
            || h.timing_allow_origin.as_deref().is_some_and(|v| {
                v.split(',')
                    .map(str::trim)
                    .any(|t| t == "*" || t == serialized)
            });
        let mut t = ResourceTiming {
            fetch_start: start,
            response_end: end,
            response_status: if h.cors_same_origin { h.status } else { 0 },
            content_type: if h.cors_same_origin {
                h.content_type.clone()
            } else {
                String::new()
            },
            ..Default::default()
        };
        if !allowed {
            return t;
        }
        let at = |i: Option<Instant>| i.map(|i| self.time_at(i));
        let ft = &h.timing;
        // Redirects: their start and end only for a chain the timing allow check passes as a whole
        // (here: all same-origin).
        if ft.redirect_end.is_some() && same_origin {
            t.redirect_start = start;
            t.redirect_end = at(ft.redirect_end).unwrap_or(start);
            t.fetch_start = t.redirect_end;
            t.redirect_count = (h.url_list.len().saturating_sub(1)) as u16;
        } else if let Some(re) = at(ft.redirect_end) {
            t.fetch_start = re;
        }
        let fetch_start = t.fetch_start;
        // A reused connection (or none): its connection timing is the fetch start.
        t.domain_lookup_start = at(ft.domain_lookup_start).unwrap_or(fetch_start);
        t.domain_lookup_end = at(ft.domain_lookup_end).unwrap_or(fetch_start);
        t.connect_start = at(ft.connect_start).unwrap_or(fetch_start);
        t.connect_end = at(ft.connect_end).unwrap_or(fetch_start);
        let https = h.url_list.last().is_some_and(|u| u.scheme() == "https");
        t.secure_connection_start = if https {
            at(ft.secure_connection_start).unwrap_or(fetch_start)
        } else {
            0.0
        };
        t.request_start = at(ft.request_start).unwrap_or(t.connect_end);
        t.response_start = at(ft.response_start).unwrap_or(t.request_start);
        t.next_hop_protocol = h.protocol.clone();
        t.decoded_body_size = decoded;
        t.encoded_body_size = if h.content_encoding.is_empty() || h.content_encoding == "identity" {
            decoded
        } else {
            h.content_length.unwrap_or(decoded)
        };
        // (no HTTP cache: every response came over the network, with its header bytes)
        t.transfer_size = if h.protocol.is_empty() {
            0
        } else {
            t.encoded_body_size + 300
        };
        t.content_encoding = h.content_encoding.clone();
        t.server_timing = h
            .server_timing
            .as_deref()
            .map(parse_server_timing)
            .unwrap_or_default();
        t
    }

    /// Resource Timing §4.4 "add a PerformanceResourceTiming entry".
    fn add_resource_timing_entry(&self, perf: Handle, entry: Handle) {
        let queue = self.perf(perf, |p| {
            let buffer = &mut p.tuples[EntryType::Resource as usize].buffer;
            if buffer.len() < p.resource_buffer_size && !p.resource_full_pending {
                buffer.push(entry);
                return false;
            }
            p.resource_secondary.push(entry);
            !std::mem::replace(&mut p.resource_full_pending, true)
        });
        if queue {
            self.queue_task(TaskSource::PerformanceTimeline, move |e, cx| {
                e.fire_resource_buffer_full(cx, perf);
            });
        }
    }

    /// Resource Timing §4.5 "fire a buffer full event".
    fn fire_resource_buffer_full(&self, cx: &mut Cx, perf: Handle) {
        loop {
            let (pending, full) = self.perf(perf, |p| {
                (
                    p.resource_secondary.len(),
                    p.tuples[EntryType::Resource as usize].buffer.len() >= p.resource_buffer_size,
                )
            });
            if pending == 0 {
                break;
            }
            if full {
                self.fire_event(cx, perf, "resourcetimingbufferfull", false, false);
            }
            let left = self.perf(perf, |p| {
                let size = p.resource_buffer_size;
                let secondary = std::mem::take(&mut p.resource_secondary);
                let mut rest = Vec::new();
                for h in secondary {
                    let buffer = &mut p.tuples[EntryType::Resource as usize].buffer;
                    if buffer.len() < size && rest.is_empty() {
                        buffer.push(h);
                    } else {
                        rest.push(h);
                    }
                }
                let left = rest.len();
                p.resource_secondary = rest;
                left
            });
            if left == pending {
                // Nothing made room: the entries are dropped.
                self.perf(perf, |p| {
                    p.tuples[EntryType::Resource as usize].dropped +=
                        p.resource_secondary.len() as u64;
                    p.resource_secondary.clear();
                });
                break;
            }
        }
        self.perf(perf, |p| p.resource_full_pending = false);
    }

    /// HTML "render-blocking": the element is potentially render-blocking (its `blocking` attribute
    /// has `render`, or `implicit`) and its document still allows adding render-blocking elements
    /// (an HTML document without a body yet).
    pub(crate) fn render_blocking_element(&self, el: NodeId, implicit: bool) -> bool {
        let dom = self.dom();
        let doc = dom.node_document(el);
        let explicit = dom
            .attr(el, &ns!(), &local_name!("blocking"))
            .is_some_and(|v| {
                v.split_ascii_whitespace()
                    .any(|t| t.eq_ignore_ascii_case("render"))
            });
        // (`blocking=render` on an element of the head counts whenever it is inserted, as in
        // Chromium, whose Resource Timing tests expect it; implicitly, only before the body)
        let in_head = dom
            .head(doc)
            .is_some_and(|head| dom.ancestors(el).any(|a| a == head));
        (explicit && in_head) || (implicit && dom.body(doc).is_none())
    }

    // ----- Navigation Timing -------------------------------------------------------------------

    /// Navigation Timing §4 "create the navigation timing entry" for a document made from a
    /// response: its fetch's timing, added to the window's buffer (it is queued for observers when
    /// the load event ends).
    pub(crate) fn create_navigation_timing_entry(&self, doc: NodeId, head: &ResponseHead) {
        let Some(window) = self.window_of_document(doc) else {
            return;
        };
        let origin = self.net_origin_of_document(doc);
        let info = HeadInfo::of(head);
        let start = head.timing.start.map_or(0.0, |s| self.time_at(s)).max(0.0);
        let mut timing = self.resource_timing_of(&info, &origin, start, 0.0, 0);
        timing.initiator_type = "navigation".into();
        // The body is still to come: its sizes and responseEnd are set as it arrives.
        timing.decoded_body_size = 0;
        timing.encoded_body_size = 0;
        timing.transfer_size = 0;
        let name = self.dom().document_url(doc).to_string();
        let entry = self.alloc(Object::PerformanceEntry(Box::new(EntryData {
            entry_type: EntryType::Navigation,
            name,
            start_time: 0.0,
            duration: 0.0,
            id: 0,
            navigation_id: 0,
            detail: None,
            global: window,
            extra: EntryExtra::Navigation(Box::new(timing), doc),
        })));
        let perf = self.performance_of(window);
        self.perf(perf, |p| {
            let t = &mut p.tuples[EntryType::Navigation as usize];
            t.buffer.clear();
            t.buffer.push(entry);
        });
        self.timing.borrow_mut().navigations.insert(doc, entry);
    }

    /// Bytes of a document's response body arrived; `last` at its end (responseEnd).
    pub(crate) fn navigation_body(&self, doc: NodeId, bytes: usize, last: bool) {
        let Some(entry) = self.timing.borrow().navigations.get(&doc).copied() else {
            return;
        };
        let now = self.now();
        if let Object::PerformanceEntry(e) = &mut *self.obj_mut(entry) {
            if let EntryExtra::Navigation(t, _) = &mut e.extra {
                if t.response_end > 0.0 {
                    return;
                }
                t.decoded_body_size += bytes as u64;
                if last {
                    t.response_end = now.max(t.response_start);
                    t.encoded_body_size =
                        if t.content_encoding.is_empty() || t.content_encoding == "identity" {
                            t.decoded_body_size
                        } else {
                            t.encoded_body_size.max(t.decoded_body_size)
                        };
                    if !t.next_hop_protocol.is_empty() {
                        t.transfer_size = t.encoded_body_size + 300;
                    }
                }
            }
        }
    }

    /// The load event of `doc` ended: Navigation Timing's "queue the navigation timing entry".
    pub(crate) fn queue_navigation_timing_entry(&self, doc: NodeId) {
        let Some(entry) = self.timing.borrow().navigations.get(&doc).copied() else {
            return;
        };
        self.queue_performance_entry_as(entry, false);
    }

    /// An entry's duration; a navigation entry's is its document's loadEventEnd (0 until then).
    pub(crate) fn entry_duration(&self, entry: Handle) -> f64 {
        let (duration, doc) = self.entry(entry, |e| match &e.extra {
            EntryExtra::Navigation(_, doc) => (e.duration, Some(*doc)),
            _ => (e.duration, None),
        });
        match doc {
            Some(doc) => self.doc_state(doc, |s| s.load_timing.load_event_end),
            None => duration,
        }
    }

    fn resource<R>(&self, h: Handle, f: impl FnOnce(&ResourceTiming) -> R) -> R {
        match &*self.obj(h) {
            Object::PerformanceEntry(e) => match &e.extra {
                EntryExtra::Resource(t) | EntryExtra::Navigation(t, _) => f(t),
                _ => f(&ResourceTiming::default()),
            },
            _ => f(&ResourceTiming::default()),
        }
    }

    fn navigation_document(&self, h: Handle) -> Option<NodeId> {
        self.entry(h, |e| match &e.extra {
            EntryExtra::Navigation(_, doc) => Some(*doc),
            _ => None,
        })
    }

    fn load_timing_of(&self, entry: Handle) -> super::document::LoadTiming {
        match self.navigation_document(entry) {
            Some(doc) if self.dom().is_alive(doc) => self.doc_state(doc, |s| s.load_timing),
            _ => Default::default(),
        }
    }

    /// The navigation timing entry of a window's document (for `performance.timing`).
    fn window_navigation_entry(&self, global: Handle) -> Option<Handle> {
        let doc = self.document_of_window(global)?;
        self.timing.borrow().navigations.get(&doc).copied()
    }

    /// A `PerformanceTiming` attribute: the time in whole milliseconds since the epoch, 0 when
    /// it did not happen.
    fn legacy_time(&self, this: Handle, f: impl FnOnce(&Engine, Handle) -> f64) -> u64 {
        let global = match &*self.obj(this) {
            Object::PerformanceTiming(g) => *g,
            _ => return 0,
        };
        let Some(entry) = self.window_navigation_entry(global) else {
            return 0;
        };
        let relative = f(self, entry);
        if relative <= 0.0 {
            return 0;
        }
        let origin = self.event_loop.borrow().origin_unix_ms;
        (origin + relative).floor() as u64
    }

    // ----- Paint Timing ------------------------------------------------------------------------

    /// Paint Timing §4 "mark paint timing" after a rendering update of `doc`: `first-paint` when it
    /// first paints anything but the canvas background, `first-contentful-paint` when it first paints
    /// text, an image or a video frame.
    pub(crate) fn mark_paint_timing(&self, doc: NodeId) {
        if self.timing.borrow().first_contentful_paint.contains(&doc) {
            return;
        }
        let Some(window) = self.window_of_document(doc) else {
            return;
        };
        let list = self
            .with_doc_renderer(doc, |r, st| {
                if r.render_blocked() || r.fragments().is_none() {
                    return None;
                }
                Some(r.paint_probe(st.viewport, st.scroll))
            })
            .flatten();
        let Some(list) = list else {
            return;
        };
        use snowlark_paint::display_list::Item;
        let mut painted = false;
        let mut contentful = false;
        // (content under a zero opacity group is not painted)
        let mut opacity: Vec<bool> = Vec::new();
        for item in &list.items {
            match item {
                Item::PushOpacity { opacity: o, .. } => {
                    opacity.push(*o <= 0.0);
                    continue;
                }
                Item::PopOpacity => {
                    opacity.pop();
                    continue;
                }
                _ => {}
            }
            if opacity.iter().any(|z| *z) {
                continue;
            }
            match item {
                Item::Text(t) if t.glyphs.len > 0 && t.color.a > 0 => contentful = true,
                Item::Image { opacity: o, .. } if *o > 0.0 => contentful = true,
                Item::Video { .. } => contentful = true,
                Item::FillRect { color, .. } => {
                    if color.a > 0 && !(color.r == 255 && color.g == 255 && color.b == 255) {
                        painted = true;
                    }
                }
                i if i.is_drawing() => painted = true,
                _ => {}
            }
            if contentful {
                break;
            }
        }
        painted |= contentful;
        let now = self.now();
        let mut st = self.timing.borrow_mut();
        let report_fp = painted && st.first_paint.insert(doc);
        let report_fcp = contentful && st.first_contentful_paint.insert(doc);
        drop(st);
        for (report, name) in [
            (report_fp, "first-paint"),
            (report_fcp, "first-contentful-paint"),
        ] {
            if report {
                let entry = self.alloc(Object::PerformanceEntry(Box::new(EntryData {
                    entry_type: EntryType::Paint,
                    name: name.into(),
                    start_time: now,
                    duration: 0.0,
                    id: 0,
                    navigation_id: 0,
                    detail: None,
                    global: window,
                    extra: EntryExtra::None,
                })));
                self.queue_performance_entry(entry);
            }
        }
    }

    // ----- Event Timing ------------------------------------------------------------------------

    /// Whether Event Timing times this event's dispatch at `target`: a trusted event of a timed
    /// type at a node of a document with a window.
    pub(crate) fn event_timing_wanted(&self, event: Handle, target: Handle) -> bool {
        let Some(node) = handle_node(target) else {
            return false;
        };
        let timed = self.ev(event, |e| {
            e.is_trusted && EVENT_TIMING_TYPES.contains(&e.type_.as_str())
        });
        timed && {
            let doc = self.dom().node_document(node);
            self.window_of_document(doc).is_some()
        }
    }

    /// Event Timing "finalize event timing": the entry of a dispatched event, waiting for the next
    /// rendering update for its duration.
    pub(crate) fn event_timing_dispatched(
        &self,
        event: Handle,
        target: Handle,
        processing_start: f64,
    ) {
        let processing_end = self.now();
        let Some(node) = handle_node(target) else {
            return;
        };
        let doc = self.dom().node_document(node);
        let Some(window) = self.window_of_document(doc) else {
            return;
        };
        let (type_, start, cancelable) =
            self.ev(event, |e| (e.type_.clone(), e.time_stamp, e.cancelable));
        let interaction_id = self.interaction_id(window, &type_);
        let entry = EntryData {
            entry_type: EntryType::Event,
            name: type_,
            start_time: start,
            duration: 0.0,
            id: 0,
            navigation_id: 0,
            detail: None,
            global: window,
            extra: EntryExtra::Event(Box::new(EventTiming {
                processing_start,
                processing_end,
                cancelable,
                target: Some(node),
                interaction_id,
            })),
        };
        self.timing.borrow_mut().pending_events.push(entry);
    }

    /// Event Timing §3 "compute interactionId" (simplified to key presses and pointer taps): a key
    /// press's keydown and a tap's pointerup start an interaction, which the events after them share.
    fn interaction_id(&self, window: Handle, type_: &str) -> u64 {
        let new = matches!(type_, "keydown" | "pointerup")
            || (type_ == "click" && !self.timing.borrow().interaction.contains_key(&window));
        let shared = matches!(type_, "keypress" | "keyup" | "click");
        if new {
            let perf = self.performance_of(window);
            let id = self.perf(perf, |p| {
                p.interaction_count += 1;
                // (ids start at a random value and grow by small steps, as the spec allows)
                if p.last_interaction_id == 0 {
                    p.last_interaction_id = 100 + p.last_id % 1000;
                }
                p.last_interaction_id += 7;
                p.last_interaction_id
            });
            self.timing.borrow_mut().interaction.insert(window, id);
            id
        } else if shared {
            let id = self
                .timing
                .borrow()
                .interaction
                .get(&window)
                .copied()
                .unwrap_or(0);
            if type_ == "click" || type_ == "keyup" {
                self.timing.borrow_mut().interaction.remove(&window);
            }
            id
        } else {
            0
        }
    }

    /// Event Timing entries wait for a rendering update (which they make due).
    pub(crate) fn event_timing_pending(&self) -> bool {
        !self.timing.borrow().pending_events.is_empty()
    }

    /// Event Timing "dispatch pending Event Timing entries" at a rendering update: each entry's
    /// duration runs to now (in 8 ms steps); they are queued, and a window's first input event also
    /// as a `first-input` entry.
    pub(crate) fn dispatch_pending_event_timings(&self) {
        let pending = std::mem::take(&mut self.timing.borrow_mut().pending_events);
        if pending.is_empty() {
            return;
        }
        let now = self.now();
        for mut data in pending {
            data.duration = ((now - data.start_time) / 8.0).round().max(0.0) * 8.0;
            let first = matches!(
                data.name.as_str(),
                "keydown" | "mousedown" | "pointerdown" | "click"
            ) && self.timing.borrow_mut().first_input.insert(data.global);
            let first_input = first.then(|| EntryData {
                entry_type: EntryType::FirstInput,
                name: data.name.clone(),
                start_time: data.start_time,
                duration: data.duration,
                id: 0,
                navigation_id: 0,
                detail: None,
                global: data.global,
                extra: match &data.extra {
                    EntryExtra::Event(t) => EntryExtra::Event(t.clone()),
                    _ => EntryExtra::None,
                },
            });
            let entry = self.alloc(Object::PerformanceEntry(Box::new(data)));
            self.queue_performance_entry(entry);
            if let Some(fi) = first_input {
                let fi = self.alloc(Object::PerformanceEntry(Box::new(fi)));
                self.queue_performance_entry(fi);
            }
        }
    }

    fn event_timing<R>(&self, h: Handle, f: impl FnOnce(&EventTiming) -> R) -> R {
        match &*self.obj(h) {
            Object::PerformanceEntry(e) => match &e.extra {
                EntryExtra::Event(t) => f(t),
                _ => f(&EventTiming::default()),
            },
            _ => f(&EventTiming::default()),
        }
    }
}

impl PerformanceResourceTimingImpl for Engine {
    fn initiator_type(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.resource(this, |t| t.initiator_type.clone()))
    }
    fn delivery_type(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn next_hop_protocol(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.resource(this, |t| t.next_hop_protocol.clone()))
    }
    fn worker_start(&self, _this: Handle) -> f64 {
        0.0
    }
    fn redirect_start(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.redirect_start)
    }
    fn redirect_end(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.redirect_end)
    }
    fn fetch_start(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.fetch_start)
    }
    fn domain_lookup_start(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.domain_lookup_start)
    }
    fn domain_lookup_end(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.domain_lookup_end)
    }
    fn connect_start(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.connect_start)
    }
    fn connect_end(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.connect_end)
    }
    fn secure_connection_start(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.secure_connection_start)
    }
    fn request_start(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.request_start)
    }
    fn final_response_headers_start(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.response_start)
    }
    fn first_interim_response_start(&self, _this: Handle) -> f64 {
        0.0
    }
    fn response_start(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.response_start)
    }
    fn response_end(&self, this: Handle) -> f64 {
        self.resource(this, |t| t.response_end)
    }
    fn worker_router_evaluation_start(&self, _this: Handle) -> f64 {
        0.0
    }
    fn worker_cache_lookup_start(&self, _this: Handle) -> f64 {
        0.0
    }
    fn worker_matched_router_source(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn worker_final_router_source(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn transfer_size(&self, this: Handle) -> u64 {
        self.resource(this, |t| t.transfer_size)
    }
    fn encoded_body_size(&self, this: Handle) -> u64 {
        self.resource(this, |t| t.encoded_body_size)
    }
    fn decoded_body_size(&self, this: Handle) -> u64 {
        self.resource(this, |t| t.decoded_body_size)
    }
    fn response_status(&self, this: Handle) -> u16 {
        self.resource(this, |t| t.response_status)
    }
    fn render_blocking_status(&self, this: Handle) -> RenderBlockingStatusType {
        if self.resource(this, |t| t.render_blocking) {
            RenderBlockingStatusType::Blocking
        } else {
            RenderBlockingStatusType::NonBlocking
        }
    }
    fn content_type(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.resource(this, |t| t.content_type.clone()))
    }
    fn content_encoding(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.resource(this, |t| t.content_encoding.clone()))
    }
    fn server_timing(&self, this: Handle) -> Vec<Handle> {
        let metrics = self.resource(this, |t| t.server_timing.clone());
        metrics
            .into_iter()
            .map(|m| self.alloc(Object::PerformanceServerTiming(Box::new(m))))
            .collect()
    }
}

impl PerformanceServerTimingImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        match &*self.obj(this) {
            Object::PerformanceServerTiming(m) => DomStr::Owned(m.0.clone()),
            _ => DomStr::Borrowed(""),
        }
    }
    fn duration(&self, this: Handle) -> f64 {
        match &*self.obj(this) {
            Object::PerformanceServerTiming(m) => m.1,
            _ => 0.0,
        }
    }
    fn description(&self, this: Handle) -> DomStr<'_> {
        match &*self.obj(this) {
            Object::PerformanceServerTiming(m) => DomStr::Owned(m.2.clone()),
            _ => DomStr::Borrowed(""),
        }
    }
}

impl PerformanceNavigationTimingImpl for Engine {
    fn unload_event_start(&self, _this: Handle) -> f64 {
        0.0
    }
    fn unload_event_end(&self, _this: Handle) -> f64 {
        0.0
    }
    fn dom_interactive(&self, this: Handle) -> f64 {
        self.load_timing_of(this).dom_interactive
    }
    fn dom_content_loaded_event_start(&self, this: Handle) -> f64 {
        self.load_timing_of(this).dom_content_loaded_event_start
    }
    fn dom_content_loaded_event_end(&self, this: Handle) -> f64 {
        self.load_timing_of(this).dom_content_loaded_event_end
    }
    fn dom_complete(&self, this: Handle) -> f64 {
        self.load_timing_of(this).dom_complete
    }
    fn load_event_start(&self, this: Handle) -> f64 {
        self.load_timing_of(this).load_event_start
    }
    fn load_event_end(&self, this: Handle) -> f64 {
        self.load_timing_of(this).load_event_end
    }
    fn type_(&self, this: Handle) -> NavigationTimingType {
        match self.navigation_document(this) {
            Some(doc) if self.doc_state(doc, |s| s.load_timing.reload) => {
                NavigationTimingType::Reload
            }
            _ => NavigationTimingType::Navigate,
        }
    }
    fn redirect_count(&self, this: Handle) -> u16 {
        self.resource(this, |t| t.redirect_count)
    }
    fn critical_ch_restart(&self, _this: Handle) -> f64 {
        0.0
    }
    fn activation_start(&self, _this: Handle) -> f64 {
        0.0
    }
}

impl PerformancePaintTimingImpl for Engine {}

impl PaintTimingMixinImpl for Engine {
    fn paint_time(&self, this: Handle) -> f64 {
        self.entry(this, |e| e.start_time)
    }
    fn presentation_time(&self, _this: Handle) -> Option<f64> {
        None
    }
}

impl PerformanceEventTimingImpl for Engine {
    fn processing_start(&self, this: Handle) -> f64 {
        self.event_timing(this, |t| t.processing_start)
    }
    fn processing_end(&self, this: Handle) -> f64 {
        self.event_timing(this, |t| t.processing_end)
    }
    fn cancelable(&self, this: Handle) -> bool {
        self.event_timing(this, |t| t.cancelable)
    }
    /// Event Timing: the target while it is connected (and not in a shadow tree).
    fn target(&self, this: Handle) -> Option<Handle> {
        let node = self.event_timing(this, |t| t.target)?;
        let dom = self.dom();
        (dom.is_alive(node) && dom.is_connected(node) && !dom.is_shadow_root(dom.root(node)))
            .then(|| node_handle(node))
    }
    fn target_selector(&self, this: Handle) -> DomStr<'_> {
        let Some(node) = self.event_timing(this, |t| t.target) else {
            return DomStr::Borrowed("");
        };
        let dom = self.dom();
        if !dom.is_alive(node) {
            return DomStr::Borrowed("");
        }
        // The element's tag name, with its id or classes.
        if dom.element(node).is_none() {
            return DomStr::Borrowed("");
        }
        let mut s = dom.local_name(node).to_string();
        if let Some(id) = dom.attr(node, &ns!(), &local_name!("id")) {
            if !id.is_empty() {
                s.push('#');
                s.push_str(id);
                return DomStr::Owned(s);
            }
        }
        if let Some(class) = dom.attr(node, &ns!(), &local_name!("class")) {
            for c in class.split_ascii_whitespace() {
                s.push('.');
                s.push_str(c);
            }
        }
        DomStr::Owned(s)
    }
    fn interaction_id(&self, this: Handle) -> u64 {
        self.event_timing(this, |t| t.interaction_id)
    }
}

impl PerformanceTimingImpl for Engine {
    fn navigation_start(&self, _this: Handle) -> u64 {
        self.event_loop.borrow().origin_unix_ms.floor() as u64
    }
    fn unload_event_start(&self, _this: Handle) -> u64 {
        0
    }
    fn unload_event_end(&self, _this: Handle) -> u64 {
        0
    }
    fn redirect_start(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.resource(n, |t| t.redirect_start))
    }
    fn redirect_end(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.resource(n, |t| t.redirect_end))
    }
    fn fetch_start(&self, this: Handle) -> u64 {
        // (a fetch that started at the time origin is still a time: not 0)
        self.legacy_time(this, |e, n| e.resource(n, |t| t.fetch_start).max(0.001))
    }
    fn domain_lookup_start(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| {
            e.resource(n, |t| t.domain_lookup_start).max(0.001)
        })
    }
    fn domain_lookup_end(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| {
            e.resource(n, |t| t.domain_lookup_end).max(0.001)
        })
    }
    fn connect_start(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.resource(n, |t| t.connect_start).max(0.001))
    }
    fn connect_end(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.resource(n, |t| t.connect_end).max(0.001))
    }
    fn secure_connection_start(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.resource(n, |t| t.secure_connection_start))
    }
    fn request_start(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.resource(n, |t| t.request_start).max(0.001))
    }
    fn response_start(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.resource(n, |t| t.response_start).max(0.001))
    }
    fn response_end(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.resource(n, |t| t.response_end))
    }
    fn dom_loading(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.load_timing_of(n).dom_loading)
    }
    fn dom_interactive(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.load_timing_of(n).dom_interactive)
    }
    fn dom_content_loaded_event_start(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| {
            e.load_timing_of(n).dom_content_loaded_event_start
        })
    }
    fn dom_content_loaded_event_end(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| {
            e.load_timing_of(n).dom_content_loaded_event_end
        })
    }
    fn dom_complete(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.load_timing_of(n).dom_complete)
    }
    fn load_event_start(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.load_timing_of(n).load_event_start)
    }
    fn load_event_end(&self, this: Handle) -> u64 {
        self.legacy_time(this, |e, n| e.load_timing_of(n).load_event_end)
    }
}

impl PerformanceNavigationImpl for Engine {
    /// Navigation Timing L1 `type`: TYPE_NAVIGATE (0) or TYPE_RELOAD (1).
    fn type_(&self, this: Handle) -> u16 {
        let global = match &*self.obj(this) {
            Object::PerformanceNavigation(g) => *g,
            _ => return 0,
        };
        match self.window_navigation_entry(global) {
            Some(entry) => match PerformanceNavigationTimingImpl::type_(self, entry) {
                NavigationTimingType::Navigate => 0,
                NavigationTimingType::Reload => 1,
                NavigationTimingType::BackForward => 2,
            },
            None => 0,
        }
    }
    fn redirect_count(&self, this: Handle) -> u16 {
        let global = match &*self.obj(this) {
            Object::PerformanceNavigation(g) => *g,
            _ => return 0,
        };
        self.window_navigation_entry(global)
            .map_or(0, |e| self.resource(e, |t| t.redirect_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_timing_headers_parse() {
        assert_eq!(
            parse_server_timing(r#"db;dur=53.2;desc="Database", cache;desc=hit, total;dur=12"#),
            vec![
                ("db".to_string(), 53.2, "Database".to_string()),
                ("cache".to_string(), 0.0, "hit".to_string()),
                ("total".to_string(), 12.0, String::new()),
            ]
        );
        assert!(parse_server_timing("").is_empty());
    }
}
