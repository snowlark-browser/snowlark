// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Performance Timeline Level 2 and User Timing Level 3: `PerformanceEntry`, `PerformanceMark`,
//! `PerformanceMeasure`, the `Performance` buffer methods, `PerformanceObserver` and
//! `PerformanceObserverEntryList`, plus HR Time's `Performance` (`now`, `timeOrigin`). The entry
//! types of Resource, Navigation, Paint and Event Timing are made in [`super::timing`]; this module
//! keeps them in the buffer map and delivers them to observers.
//!
//! Every global (a Window or a worker's global scope) has one `Performance` object, which holds that
//! global's performance entry buffer map (one tuple per supported entry type), its list of registered
//! performance observers and its "performance observer task queued flag" (Performance Timeline §3).

use snowlark_bindings::generated::{
    PerformanceEntryImpl, PerformanceImpl, PerformanceMarkImpl, PerformanceMarkOptions,
    PerformanceMeasureImpl, PerformanceMeasureOptions, PerformanceObserverCallback,
    PerformanceObserverCallbackOptions, PerformanceObserverEntryListImpl, PerformanceObserverImpl,
    PerformanceObserverInit, StringOrDouble, StringOrPerformanceMeasureOptions,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle, JsValue};

use super::element::rejected;
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// The entry types of the timing entry type registry this engine supports, in alphabetical order (the
/// frozen array of supported entry types).
pub(crate) const SUPPORTED_ENTRY_TYPES: &[&str] = &[
    "event",
    "first-input",
    "mark",
    "measure",
    "navigation",
    "paint",
    "resource",
    "visibility-state",
];

/// Event Timing's default duration threshold, and the lowest an observer may ask for.
pub(crate) const DEFAULT_DURATION_THRESHOLD: f64 = 104.0;
const MIN_DURATION_THRESHOLD: f64 = 16.0;

/// User Timing §4.1 / Navigation Timing L1: the read-only attributes of `PerformanceTiming`, which
/// `mark()` may not use as names on a Window and which `measure()` converts to timestamps.
const TIMING_ATTRIBUTES: &[&str] = &[
    "navigationStart",
    "unloadEventStart",
    "unloadEventEnd",
    "redirectStart",
    "redirectEnd",
    "fetchStart",
    "domainLookupStart",
    "domainLookupEnd",
    "connectStart",
    "connectEnd",
    "secureConnectionStart",
    "requestStart",
    "responseStart",
    "responseEnd",
    "domLoading",
    "domInteractive",
    "domContentLoadedEventStart",
    "domContentLoadedEventEnd",
    "domComplete",
    "loadEventStart",
    "loadEventEnd",
];

/// A supported entry type; its index is its tuple's in the buffer map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryType {
    Mark = 0,
    Measure = 1,
    Resource = 2,
    Navigation = 3,
    Paint = 4,
    Event = 5,
    FirstInput = 6,
    VisibilityState = 7,
}

/// The number of entry types (tuples in the buffer map).
const ENTRY_TYPES: usize = 8;

impl EntryType {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryType::Mark => "mark",
            EntryType::Measure => "measure",
            EntryType::Resource => "resource",
            EntryType::Navigation => "navigation",
            EntryType::Paint => "paint",
            EntryType::Event => "event",
            EntryType::FirstInput => "first-input",
            EntryType::VisibilityState => "visibility-state",
        }
    }

    fn parse(s: &str) -> Option<EntryType> {
        match s {
            "mark" => Some(EntryType::Mark),
            "measure" => Some(EntryType::Measure),
            "resource" => Some(EntryType::Resource),
            "navigation" => Some(EntryType::Navigation),
            "paint" => Some(EntryType::Paint),
            "event" => Some(EntryType::Event),
            "first-input" => Some(EntryType::FirstInput),
            "visibility-state" => Some(EntryType::VisibilityState),
            _ => None,
        }
    }

    /// The registry's maxBufferSize (`None` is infinite; the resource buffer's size is the
    /// Performance object's, see [`PerformanceData::resource_buffer_size`]).
    fn max_buffer_size(self) -> Option<usize> {
        match self {
            EntryType::Mark | EntryType::Measure | EntryType::Resource => None,
            EntryType::Navigation | EntryType::FirstInput => Some(1),
            EntryType::Paint => Some(2),
            EntryType::Event => Some(150),
            EntryType::VisibilityState => Some(50),
        }
    }

    /// The registry's availableFromTimeline.
    fn available_from_timeline(self) -> bool {
        self != EntryType::Event
    }
}

/// What an entry of a timing specification other than User Timing carries.
pub enum EntryExtra {
    None,
    /// A `PerformanceResourceTiming`.
    Resource(Box<super::timing::ResourceTiming>),
    /// A `PerformanceNavigationTiming`: its resource timing and its document.
    Navigation(Box<super::timing::ResourceTiming>, snowlark_dom::NodeId),
    /// A `PerformanceEventTiming` (an `event` or `first-input` entry).
    Event(Box<super::timing::EventTiming>),
}

/// A `PerformanceEntry` (a `PerformanceMark` or a `PerformanceMeasure`).
pub struct EntryData {
    pub entry_type: EntryType,
    pub name: String,
    pub start_time: f64,
    pub duration: f64,
    /// Set when the entry is queued; 0 until then.
    pub id: u64,
    pub navigation_id: u64,
    /// `None` is null.
    pub detail: Option<JsValue>,
    /// The entry's relevant global object.
    pub global: Handle,
    pub extra: EntryExtra,
}

/// A performance entry tuple (Performance Timeline §3): the buffer and its dropped entries count. Marks
/// and measures have an infinite maxBufferSize and are available from the timeline.
#[derive(Default)]
pub struct Tuple {
    pub buffer: Vec<Handle>,
    pub dropped: u64,
}

/// `PerformanceObserverInit`, as a registered performance observer's options list keeps it.
#[derive(Clone, Debug, Default)]
pub struct ObserveOptions {
    pub entry_types: Option<Vec<String>>,
    pub type_: Option<String>,
    pub buffered: bool,
    /// Event Timing's durationThreshold (for `event` entries).
    pub duration_threshold: f64,
}

impl ObserveOptions {
    fn wants(&self, t: EntryType) -> bool {
        self.entry_types
            .as_ref()
            .is_some_and(|l| l.iter().any(|x| x == t.as_str()))
            || self.type_.as_deref() == Some(t.as_str())
    }

    /// Whether the options want this entry (Event Timing: an `event` entry at least as long as the
    /// duration threshold).
    fn wants_entry(&self, t: EntryType, duration: f64) -> bool {
        self.wants(t) && (t != EntryType::Event || duration >= self.duration_threshold)
    }

    /// The supported types this options item names.
    fn types(&self) -> impl Iterator<Item = EntryType> + '_ {
        self.type_
            .iter()
            .chain(self.entry_types.iter().flatten())
            .filter_map(|s| EntryType::parse(s))
    }
}

/// A registered performance observer.
pub struct Registered {
    pub observer: Handle,
    pub options: Vec<ObserveOptions>,
}

/// A global's performance timeline state, held by its `Performance` object.
pub struct PerformanceData {
    pub global: Handle,
    pub tuples: [Tuple; ENTRY_TYPES],
    /// Resource Timing §4.3: the resource timing buffer size limit, the buffer-full event pending
    /// flag and the secondary buffer.
    pub resource_buffer_size: usize,
    pub resource_full_pending: bool,
    pub resource_secondary: Vec<Handle>,
    /// Event Timing: the interaction count and the last interaction id given out.
    pub interaction_count: u64,
    pub last_interaction_id: u64,
    pub observers: Vec<Registered>,
    pub task_queued: bool,
    /// The last performance entry id (starts at a random integer in 100..=10000).
    pub last_id: u64,
    /// The generator of the id increments.
    rng: u64,
}

impl PerformanceData {
    pub fn new(global: Handle) -> PerformanceData {
        let mut seed = [0u8; 8];
        getrandom::fill(&mut seed).expect("the operating system's random number generator failed");
        let rng = u64::from_le_bytes(seed) | 1;
        PerformanceData {
            global,
            tuples: Default::default(),
            resource_buffer_size: 250,
            resource_full_pending: false,
            resource_secondary: Vec::new(),
            interaction_count: 0,
            last_interaction_id: 0,
            observers: Vec::new(),
            task_queued: false,
            last_id: 100 + rng % 9901,
            rng,
        }
    }

    /// Performance Timeline "generate a Performance Entry id": the last id plus a small random step,
    /// so that ids are not a count of entries.
    fn generate_id(&mut self) -> u64 {
        // xorshift64
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        self.last_id += 1 + x % 4;
        self.last_id
    }
}

/// The observer type of a `PerformanceObserver`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObserverType {
    Undefined,
    Single,
    Multiple,
}

/// A `PerformanceObserver`.
pub struct ObserverData {
    pub callback: super::held::Held<v8::Object>,
    pub buffer: Vec<Handle>,
    pub kind: ObserverType,
    pub requires_dropped_entries: bool,
    pub global: Handle,
}

/// Performance Timeline "filter buffer by name and type", appending to `out`.
fn filter_into(
    objects: &[Object],
    buffer: &[Handle],
    name: Option<&str>,
    type_: Option<&str>,
    out: &mut Vec<Handle>,
) {
    for &h in buffer {
        let Object::PerformanceEntry(e) = &objects[h.get() as usize - 1] else {
            continue;
        };
        if type_.is_some_and(|t| t != e.entry_type.as_str()) {
            continue;
        }
        if name.is_some_and(|n| n != e.name) {
            continue;
        }
        out.push(h);
    }
}

/// Sorts entries chronologically by startTime (stable: equal times keep buffer order).
fn sort_by_start_time(objects: &[Object], list: &mut [Handle]) {
    let start = |h: &Handle| match &objects[h.get() as usize - 1] {
        Object::PerformanceEntry(e) => e.start_time,
        _ => 0.0,
    };
    list.sort_by(|a, b| start(a).total_cmp(&start(b)));
}

impl Engine {
    /// The global object of the current realm (a Window or a worker's global scope).
    pub(crate) fn current_global(&self, cx: &mut Cx) -> Option<Handle> {
        rt::realm_global(cx.scope().get_current_context())
    }

    pub(crate) fn is_window(&self, global: Handle) -> bool {
        matches!(&*self.obj(global), Object::Window)
    }

    /// A global's `Performance` object, made on first use. A Window's timeline starts with the entry
    /// of HTML's "set the initial visibility state": its document's visibility state, at time 0.
    pub(crate) fn performance_of(&self, global: Handle) -> Handle {
        let mut made = false;
        let perf = self.window_singleton(global, "performance", || {
            made = true;
            Object::Performance(Box::new(PerformanceData::new(global)))
        });
        if made && self.is_window(global) {
            if let Some(doc) = self.associated_document(global) {
                self.queue_visibility_state_entry(global, self.document_visible(doc), 0.0);
            }
        }
        perf
    }

    /// HTML "queue a new VisibilityStateEntry" for a Window's document.
    pub(crate) fn queue_visibility_state_entry(
        &self,
        window: Handle,
        visible: bool,
        timestamp: f64,
    ) {
        let entry = self.alloc(Object::PerformanceEntry(Box::new(EntryData {
            entry_type: EntryType::VisibilityState,
            name: if visible { "visible" } else { "hidden" }.into(),
            start_time: timestamp,
            duration: 0.0,
            id: 0,
            navigation_id: 0,
            detail: None,
            global: window,
            extra: EntryExtra::None,
        })));
        self.queue_performance_entry(entry);
    }

    pub(crate) fn perf<R>(&self, h: Handle, f: impl FnOnce(&mut PerformanceData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::Performance(p) => f(p),
            _ => panic!("not a Performance"),
        }
    }

    pub(crate) fn entry<R>(&self, h: Handle, f: impl FnOnce(&EntryData) -> R) -> R {
        match &*self.obj(h) {
            Object::PerformanceEntry(e) => f(e),
            _ => panic!("not a PerformanceEntry"),
        }
    }

    fn perf_observer<R>(&self, h: Handle, f: impl FnOnce(&mut ObserverData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::PerformanceObserver(o) => f(o),
            _ => panic!("not a PerformanceObserver"),
        }
    }

    /// Performance Timeline "filter buffer map by name and type" over a global's buffer map.
    fn filter_buffer_map(
        &self,
        perf: Handle,
        name: Option<&str>,
        type_: Option<&str>,
    ) -> Vec<Handle> {
        let objects = self.objects.borrow();
        let Object::Performance(p) = &objects[perf.get() as usize - 1] else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match type_ {
            Some(t) => {
                if let Some(t) = EntryType::parse(t).filter(|t| t.available_from_timeline()) {
                    filter_into(
                        &objects,
                        &p.tuples[t as usize].buffer,
                        name,
                        type_,
                        &mut out,
                    );
                }
            }
            None => {
                for (i, tuple) in p.tuples.iter().enumerate() {
                    if i == EntryType::Event as usize {
                        continue;
                    }
                    filter_into(&objects, &tuple.buffer, name, None, &mut out);
                }
            }
        }
        sort_by_start_time(&objects, &mut out);
        out
    }

    /// Performance Timeline "queue a PerformanceEntry", including adding it to its global's buffer
    /// when its type's "should add entry" steps say so (resource entries are added by Resource
    /// Timing's own steps; an `event` entry shorter than 104 ms is not added).
    pub(crate) fn queue_performance_entry(&self, entry: Handle) {
        self.queue_performance_entry_as(entry, true)
    }

    /// "Queue a PerformanceEntry"; `add` false for an entry already in the buffer (the navigation
    /// entry, added when its document was made).
    pub(crate) fn queue_performance_entry_as(&self, entry: Handle, add: bool) {
        let (global, entry_type, duration) =
            self.entry(entry, |e| (e.global, e.entry_type, e.duration));
        let perf = self.performance_of(global);
        // 1. The id. 4. The navigation id: this engine keeps no navigation entries, so a document's
        // most recent navigation is unset and the entry keeps the initial value.
        let id = self.perf(perf, |p| p.generate_id());
        if let Object::PerformanceEntry(e) = &mut *self.obj_mut(entry) {
            if e.id == 0 {
                e.id = id;
            }
        }
        // 5-6. Interested observers get the entry.
        let interested: Vec<Handle> = self.perf(perf, |p| {
            p.observers
                .iter()
                .filter(|r| {
                    r.options
                        .iter()
                        .any(|o| o.wants_entry(entry_type, duration))
                })
                .map(|r| r.observer)
                .collect()
        });
        for o in interested {
            self.perf_observer(o, |o| o.buffer.push(entry));
        }
        // 7-9.
        let should_add = add
            && match entry_type {
                EntryType::Resource => false,
                EntryType::Event => duration >= DEFAULT_DURATION_THRESHOLD,
                _ => true,
            };
        if should_add {
            self.perf(perf, |p| {
                let t = &mut p.tuples[entry_type as usize];
                match entry_type.max_buffer_size() {
                    Some(max) if t.buffer.len() >= max => t.dropped += 1,
                    _ => t.buffer.push(entry),
                }
            });
        }
        // 10.
        self.queue_performance_observer_task(perf);
    }

    /// Performance Timeline "queue the PerformanceObserver task".
    fn queue_performance_observer_task(&self, perf: Handle) {
        if self.perf(perf, |p| std::mem::replace(&mut p.task_queued, true)) {
            return;
        }
        self.queue_task(TaskSource::PerformanceTimeline, move |e, cx| {
            e.run_performance_observer_task(cx, perf);
        });
    }

    fn run_performance_observer_task(&self, cx: &mut Cx, perf: Handle) {
        // 1-2. (A removed frame's Performance may be freed before its task runs.)
        let (registered, global) = match &mut *self.obj_mut(perf) {
            Object::Performance(p) => {
                p.task_queued = false;
                let list: Vec<(Handle, Vec<ObserveOptions>)> = p
                    .observers
                    .iter()
                    .map(|r| (r.observer, r.options.clone()))
                    .collect();
                (list, p.global)
            }
            _ => return,
        };
        let realm = self
            .context_of_window(global)
            .and_then(|id| self.realm_of_context(id));
        // 3. For each registered observer.
        for (po, options) in registered {
            let (entries, requires_dropped, callback) = match &mut *self.obj_mut(po) {
                Object::PerformanceObserver(o) => (
                    std::mem::take(&mut o.buffer),
                    std::mem::replace(&mut o.requires_dropped_entries, false),
                    o.callback.clone(),
                ),
                _ => continue,
            };
            // "If entries is empty, return": this observer has nothing to deliver; the others still
            // get theirs.
            if entries.is_empty() {
                if requires_dropped {
                    self.perf_observer(po, |o| o.requires_dropped_entries = true);
                }
                continue;
            }
            let Some(callback) = callback.get(cx.scope()).map(PerformanceObserverCallback) else {
                continue;
            };
            let list = self.alloc(Object::PerformanceObserverEntryList(entries, global));
            let dropped = requires_dropped.then(|| {
                self.perf(perf, |p| {
                    options
                        .iter()
                        .flat_map(|o| o.types())
                        .map(|t| p.tuples[t as usize].dropped)
                        .sum::<u64>()
                })
            });
            let options = PerformanceObserverCallbackOptions {
                dropped_entries_count: dropped,
            };
            self.in_realm(cx, realm.clone(), |cx| {
                let scope = cx.scope();
                v8::tc_scope!(let tc, scope);
                let mut cx2 = Cx::new(tc);
                let r = callback.invoke(&mut cx2, Some(&po), list, po, Some(options));
                if r.is_err() {
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

    /// User Timing "convert a name to a timestamp" for a `PerformanceTiming` attribute name.
    fn convert_name_to_timestamp(&self, global: Handle, name: &str) -> rt::Result<f64> {
        // 1.
        if !self.is_window(global) {
            return Err(Error::Type(
                format!("{name} is a PerformanceTiming attribute, which workers do not have")
                    .into(),
            ));
        }
        // 2.
        if name == "navigationStart" {
            return Ok(0.0);
        }
        // 3-5. The attribute's value minus navigationStart; a value of 0 (not reached yet, or not
        // recorded) throws.
        let relative = self
            .document_of_window(global)
            .and_then(|doc| self.doc_state(doc, |s| s.load_timing.get(name)))
            .unwrap_or(0.0);
        if relative <= 0.0 {
            return Err(Error::Dom(
                ExceptionName::InvalidAccessError,
                format!("performance.timing.{name} is 0").into(),
            ));
        }
        // PerformanceTiming holds whole milliseconds since the epoch.
        let origin = self.event_loop.borrow().origin_unix_ms;
        Ok((origin + relative).floor() - origin.floor())
    }

    /// User Timing "convert a mark to a timestamp".
    fn convert_mark_to_timestamp(
        &self,
        perf: Handle,
        global: Handle,
        mark: &StringOrDouble,
    ) -> rt::Result<f64> {
        match mark {
            StringOrDouble::String(name) => {
                if TIMING_ATTRIBUTES.contains(&name.as_str()) {
                    return self.convert_name_to_timestamp(global, name);
                }
                // The most recent mark of that name in the buffer.
                let objects = self.objects.borrow();
                let Object::Performance(p) = &objects[perf.get() as usize - 1] else {
                    return Err(Error::Type("not a Performance".into()));
                };
                p.tuples[EntryType::Mark as usize]
                    .buffer
                    .iter()
                    .rev()
                    .find_map(|h| match &objects[h.get() as usize - 1] {
                        Object::PerformanceEntry(e) if e.name == *name => Some(e.start_time),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        Error::Dom(
                            ExceptionName::SyntaxError,
                            format!("The mark '{name}' does not exist").into(),
                        )
                    })
            }
            StringOrDouble::Double(t) => {
                if *t < 0.0 {
                    return Err(Error::Type(format!("'{t}' is a negative timestamp").into()));
                }
                Ok(*t)
            }
        }
    }

    /// The `PerformanceMark` constructor steps (User Timing §4.2), for a mark of `global`.
    fn create_mark(
        &self,
        cx: &mut Cx,
        global: Handle,
        name: &str,
        options: PerformanceMarkOptions,
    ) -> rt::Result<Handle> {
        // 1.
        if self.is_window(global) && TIMING_ATTRIBUTES.contains(&name) {
            return Err(Error::Dom(
                ExceptionName::SyntaxError,
                format!("'{name}' is part of the PerformanceTiming interface").into(),
            ));
        }
        // 3.
        let start_time = match options.start_time {
            Some(t) if t < 0.0 => {
                return Err(Error::Type(format!("'{t}' is a negative startTime").into()))
            }
            Some(t) => t,
            None => self.now(),
        };
        // 5-6.
        let detail = self.clone_detail(cx, options.detail)?;
        Ok(self.alloc(Object::PerformanceEntry(Box::new(EntryData {
            entry_type: EntryType::Mark,
            name: name.to_owned(),
            start_time,
            duration: 0.0,
            id: 0,
            navigation_id: 0,
            detail,
            global,
            extra: EntryExtra::None,
        }))))
    }

    /// A `detail`: null stays null; anything else is structured-cloned into the current realm.
    fn clone_detail(&self, cx: &mut Cx, detail: Option<JsValue>) -> rt::Result<Option<JsValue>> {
        let Some(d) = detail else {
            return Ok(None);
        };
        {
            let scope = cx.scope();
            if v8::Local::new(scope, d.global()).is_null() {
                return Ok(None);
            }
        }
        self.structured_clone_value(cx, &d).map(Some)
    }

    /// The Performance object of `this`'s relevant global.
    pub(crate) fn this_global(&self, perf: Handle) -> Handle {
        self.perf(perf, |p| p.global)
    }
}

impl PerformanceImpl for Engine {
    fn time_origin(&self, _this: Handle) -> f64 {
        self.event_loop.borrow().origin_unix_ms
    }
    fn interaction_count(&self, this: Handle) -> u64 {
        self.perf(this, |p| p.interaction_count)
    }
    fn now(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<f64> {
        Ok(Engine::now(self))
    }
    fn measure_user_agent_specific_memory(
        &self,
        cx: &mut Cx,
        _this: Handle,
    ) -> rt::Result<snowlark_bindings::rt::JsPromise> {
        rejected(
            cx,
            ExceptionName::SecurityError,
            "Not cross-origin isolated",
        )
    }
    /// Resource Timing §4.2 `clearResourceTimings()`.
    fn clear_resource_timings(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.drain_resource_timings();
        self.perf(this, |p| {
            p.tuples[EntryType::Resource as usize].buffer.clear();
        });
        Ok(())
    }
    /// Resource Timing §4.2 `setResourceTimingBufferSize()`.
    fn set_resource_timing_buffer_size(
        &self,
        _cx: &mut Cx,
        this: Handle,
        max: u32,
    ) -> rt::Result<()> {
        self.perf(this, |p| p.resource_buffer_size = max as usize);
        Ok(())
    }
    fn timing(&self, this: Handle) -> Handle {
        let global = self.this_global(this);
        self.window_singleton(global, "performance.timing", || {
            Object::PerformanceTiming(global)
        })
    }
    fn navigation(&self, this: Handle) -> Handle {
        let global = self.this_global(this);
        self.window_singleton(global, "performance.navigation", || {
            Object::PerformanceNavigation(global)
        })
    }

    fn get_entries(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        self.drain_resource_timings();
        Ok(self.filter_buffer_map(this, None, None))
    }
    fn get_entries_by_type(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_: &str,
    ) -> rt::Result<Vec<Handle>> {
        self.drain_resource_timings();
        Ok(self.filter_buffer_map(this, None, Some(type_)))
    }
    fn get_entries_by_name(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        type_: Option<&str>,
    ) -> rt::Result<Vec<Handle>> {
        self.drain_resource_timings();
        Ok(self.filter_buffer_map(this, Some(name), type_))
    }

    /// User Timing §3.1.1 `mark()`.
    fn mark(
        &self,
        cx: &mut Cx,
        this: Handle,
        mark_name: &str,
        mark_options: PerformanceMarkOptions,
    ) -> rt::Result<Handle> {
        let global = self.this_global(this);
        let entry = self.create_mark(cx, global, mark_name, mark_options)?;
        self.queue_performance_entry(entry);
        if let Some(m) = &self.sample_marks {
            m.mark(mark_name);
        }
        Ok(entry)
    }

    /// User Timing §3.1.2 `clearMarks()`.
    fn clear_marks(&self, _cx: &mut Cx, this: Handle, mark_name: Option<&str>) -> rt::Result<()> {
        self.clear_entries(this, EntryType::Mark, mark_name);
        Ok(())
    }

    /// User Timing §3.1.3 `measure()`.
    fn measure(
        &self,
        cx: &mut Cx,
        this: Handle,
        measure_name: &str,
        start_or_measure_options: StringOrPerformanceMeasureOptions,
        end_mark: Option<&str>,
    ) -> rt::Result<Handle> {
        let global = self.this_global(this);
        let options = match &start_or_measure_options {
            StringOrPerformanceMeasureOptions::PerformanceMeasureOptions(o) => Some(o),
            StringOrPerformanceMeasureOptions::String(_) => None,
        };
        // 1. A measure options object with members.
        if let Some(o) = options {
            if o.start.is_some() || o.end.is_some() || o.duration.is_some() || o.detail.is_some() {
                if end_mark.is_some() {
                    return Err(Error::Type(
                        "measure() cannot take both measure options and an end mark".into(),
                    ));
                }
                if o.start.is_none() && o.end.is_none() {
                    return Err(Error::Type("measure options need a start or an end".into()));
                }
                if o.start.is_some() && o.duration.is_some() && o.end.is_some() {
                    return Err(Error::Type(
                        "measure options cannot have start, duration and end together".into(),
                    ));
                }
            }
        }
        let convert = |m: &StringOrDouble| self.convert_mark_to_timestamp(this, global, m);
        let duration_of = |o: &PerformanceMeasureOptions| {
            convert(&StringOrDouble::Double(o.duration.unwrap_or(0.0)))
        };
        // 2. The end time.
        let end_time = if let Some(end) = end_mark {
            convert(&StringOrDouble::String(end.to_owned()))?
        } else if let Some(end) = options.and_then(|o| o.end.as_ref()) {
            convert(end)?
        } else if let Some(o) = options.filter(|o| o.start.is_some() && o.duration.is_some()) {
            let start = convert(o.start.as_ref().expect("checked"))?;
            let duration = duration_of(o)?;
            start + duration
        } else {
            self.now()
        };
        // 3. The start time.
        let start_time = if let Some(start) = options.and_then(|o| o.start.as_ref()) {
            convert(start)?
        } else if let Some(o) = options.filter(|o| o.duration.is_some() && o.end.is_some()) {
            let duration = duration_of(o)?;
            let end = convert(o.end.as_ref().expect("checked"))?;
            end - duration
        } else if let StringOrPerformanceMeasureOptions::String(s) = &start_or_measure_options {
            convert(&StringOrDouble::String(s.clone()))?
        } else {
            0.0
        };
        // 4-9.
        let detail = match start_or_measure_options {
            StringOrPerformanceMeasureOptions::PerformanceMeasureOptions(o) => {
                self.clone_detail(cx, o.detail)?
            }
            StringOrPerformanceMeasureOptions::String(_) => None,
        };
        let entry = self.alloc(Object::PerformanceEntry(Box::new(EntryData {
            entry_type: EntryType::Measure,
            name: measure_name.to_owned(),
            start_time,
            duration: end_time - start_time,
            id: 0,
            navigation_id: 0,
            detail,
            global,
            extra: EntryExtra::None,
        })));
        // 10-11.
        self.queue_performance_entry(entry);
        Ok(entry)
    }

    /// User Timing §3.1.4 `clearMeasures()`.
    fn clear_measures(
        &self,
        _cx: &mut Cx,
        this: Handle,
        measure_name: Option<&str>,
    ) -> rt::Result<()> {
        self.clear_entries(this, EntryType::Measure, measure_name);
        Ok(())
    }
}

impl Engine {
    /// `clearMarks()` / `clearMeasures()`: removes the entries of a type (of a name, when given) from
    /// the buffer.
    fn clear_entries(&self, perf: Handle, t: EntryType, name: Option<&str>) {
        let mut objects = self.objects.borrow_mut();
        let mut buffer = match &mut objects[perf.get() as usize - 1] {
            Object::Performance(p) => std::mem::take(&mut p.tuples[t as usize].buffer),
            _ => return,
        };
        match name {
            None => buffer.clear(),
            Some(n) => buffer.retain(|h| match &objects[h.get() as usize - 1] {
                Object::PerformanceEntry(e) => e.name != n,
                _ => false,
            }),
        }
        if let Object::Performance(p) = &mut objects[perf.get() as usize - 1] {
            p.tuples[t as usize].buffer = buffer;
        }
    }
}

/// HTML §6.2.1: a `visibility-state` entry's attributes are its visibility state, its timestamp
/// and a zero duration.
impl snowlark_bindings::generated::VisibilityStateEntryImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        PerformanceEntryImpl::name(self, this)
    }
    fn entry_type(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(EntryType::VisibilityState.as_str())
    }
    fn start_time(&self, this: Handle) -> f64 {
        self.entry(this, |e| e.start_time)
    }
    fn duration(&self, _this: Handle) -> u32 {
        0
    }
}

impl PerformanceEntryImpl for Engine {
    fn id(&self, this: Handle) -> u64 {
        self.entry(this, |e| e.id)
    }
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Guard(std::cell::Ref::map(self.objects.borrow(), |o| {
            match &o[this.get() as usize - 1] {
                Object::PerformanceEntry(e) => e.name.as_str(),
                _ => "",
            }
        }))
    }
    fn entry_type(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(self.entry(this, |e| e.entry_type.as_str()))
    }
    fn start_time(&self, this: Handle) -> f64 {
        self.entry(this, |e| e.start_time)
    }
    fn duration(&self, this: Handle) -> f64 {
        self.entry_duration(this)
    }
    fn navigation_id(&self, this: Handle) -> u64 {
        self.entry(this, |e| e.navigation_id)
    }
}

impl Engine {
    fn entry_detail(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        let detail = self.entry(this, |e| e.detail.clone());
        Ok(match detail {
            Some(d) => d,
            None => {
                let scope = cx.scope();
                JsValue::new(scope, v8::null(scope).into())
            }
        })
    }
}

impl PerformanceMarkImpl for Engine {
    fn detail(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        self.entry_detail(cx, this)
    }
    fn constructor(
        &self,
        cx: &mut Cx,
        mark_name: &str,
        mark_options: PerformanceMarkOptions,
    ) -> rt::Result<Handle> {
        let global = self
            .current_global(cx)
            .ok_or_else(|| exception(ExceptionName::InvalidStateError, "No global object"))?;
        self.create_mark(cx, global, mark_name, mark_options)
    }
}

impl PerformanceMeasureImpl for Engine {
    fn detail(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        self.entry_detail(cx, this)
    }
}

impl PerformanceObserverImpl for Engine {
    fn static_supported_entry_types(&self) -> Vec<String> {
        SUPPORTED_ENTRY_TYPES
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// Performance Timeline §5.3 `observe()`.
    fn observe(
        &self,
        _cx: &mut Cx,
        this: Handle,
        options: PerformanceObserverInit,
    ) -> rt::Result<()> {
        // 1.
        let global = self.perf_observer(this, |o| o.global);
        let perf = self.performance_of(global);
        // 2-3.
        if options.entry_types.is_none() && options.type_.is_none() {
            return Err(Error::Type("observe() needs entryTypes or type".into()));
        }
        if options.entry_types.is_some()
            && (options.type_.is_some()
                || options.buffered.is_some()
                || options.duration_threshold.is_some())
        {
            return Err(Error::Type("observe() takes entryTypes alone".into()));
        }
        // 4-5.
        let kind = self.perf_observer(this, |o| o.kind);
        let kind = match kind {
            ObserverType::Undefined if options.entry_types.is_some() => ObserverType::Multiple,
            ObserverType::Undefined => ObserverType::Single,
            ObserverType::Single if options.entry_types.is_some() => {
                return Err(exception(
                    ExceptionName::InvalidModificationError,
                    "This observer observes a single type",
                ))
            }
            ObserverType::Multiple if options.type_.is_some() => {
                return Err(exception(
                    ExceptionName::InvalidModificationError,
                    "This observer observes a list of entry types",
                ))
            }
            k => k,
        };
        self.perf_observer(this, |o| {
            o.kind = kind;
            o.requires_dropped_entries = true;
        });
        if kind == ObserverType::Multiple {
            // 6.
            let requested = options.entry_types.unwrap_or_default();
            let types: Vec<String> = requested
                .iter()
                .filter(|t| SUPPORTED_ENTRY_TYPES.contains(&t.as_str()))
                .cloned()
                .collect();
            for t in requested.iter().filter(|t| !types.contains(t)) {
                self.console_print(
                    "warn",
                    &format!("PerformanceObserver: the entry type '{t}' is not supported; ignored"),
                );
            }
            if types.is_empty() {
                self.console_print(
                    "warn",
                    "PerformanceObserver: observe() with no supported entry types does nothing",
                );
                return Ok(());
            }
            let item = ObserveOptions {
                entry_types: Some(types),
                type_: None,
                buffered: false,
                duration_threshold: DEFAULT_DURATION_THRESHOLD,
            };
            self.perf(perf, |p| {
                match p.observers.iter_mut().find(|r| r.observer == this) {
                    Some(r) => r.options = vec![item],
                    None => p.observers.push(Registered {
                        observer: this,
                        options: vec![item],
                    }),
                }
            });
            return Ok(());
        }
        // 7.
        let type_ = options.type_.unwrap_or_default();
        let Some(entry_type) = EntryType::parse(&type_) else {
            self.console_print(
                "warn",
                &format!("PerformanceObserver: the entry type '{type_}' is not supported; ignored"),
            );
            return Ok(());
        };
        let buffered = options.buffered.unwrap_or(false);
        let duration_threshold = options
            .duration_threshold
            .map_or(DEFAULT_DURATION_THRESHOLD, |d| {
                d.max(MIN_DURATION_THRESHOLD)
            });
        let item = ObserveOptions {
            entry_types: None,
            type_: Some(type_),
            buffered,
            duration_threshold,
        };
        self.perf(perf, |p| {
            match p.observers.iter_mut().find(|r| r.observer == this) {
                Some(r) => match r.options.iter_mut().find(|o| o.type_ == item.type_) {
                    Some(o) => *o = item,
                    None => r.options.push(item),
                },
                None => p.observers.push(Registered {
                    observer: this,
                    options: vec![item],
                }),
            }
        });
        if buffered {
            self.drain_resource_timings();
            let mut past = self.perf(perf, |p| p.tuples[entry_type as usize].buffer.clone());
            if entry_type == EntryType::Event {
                past.retain(|&h| self.entry(h, |e| e.duration) >= duration_threshold);
            }
            self.perf_observer(this, |o| o.buffer.extend(past));
            self.queue_performance_observer_task(perf);
        }
        Ok(())
    }

    /// Performance Timeline §5.4 `disconnect()`.
    fn disconnect(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let global = self.perf_observer(this, |o| {
            o.buffer.clear();
            o.global
        });
        let perf = self.performance_of(global);
        self.perf(perf, |p| p.observers.retain(|r| r.observer != this));
        Ok(())
    }

    /// Performance Timeline §5.5 `takeRecords()`.
    fn take_records(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        Ok(self.perf_observer(this, |o| std::mem::take(&mut o.buffer)))
    }

    fn constructor(
        &self,
        cx: &mut Cx,
        callback: PerformanceObserverCallback,
    ) -> rt::Result<Handle> {
        let global = self
            .current_global(cx)
            .ok_or_else(|| exception(ExceptionName::InvalidStateError, "No global object"))?;
        Ok(
            self.alloc(Object::PerformanceObserver(Box::new(ObserverData {
                callback: super::held::Held::object(callback.0),
                buffer: Vec::new(),
                kind: ObserverType::Undefined,
                requires_dropped_entries: false,
                global,
            }))),
        )
    }
}

impl Engine {
    fn entry_list_filter(
        &self,
        this: Handle,
        name: Option<&str>,
        type_: Option<&str>,
    ) -> Vec<Handle> {
        let objects = self.objects.borrow();
        let Object::PerformanceObserverEntryList(list, _) = &objects[this.get() as usize - 1]
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        filter_into(&objects, list, name, type_, &mut out);
        sort_by_start_time(&objects, &mut out);
        out
    }
}

impl PerformanceObserverEntryListImpl for Engine {
    fn get_entries(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        Ok(self.entry_list_filter(this, None, None))
    }
    fn get_entries_by_type(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_: &str,
    ) -> rt::Result<Vec<Handle>> {
        Ok(self.entry_list_filter(this, None, Some(type_)))
    }
    fn get_entries_by_name(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        type_: Option<&str>,
    ) -> rt::Result<Vec<Handle>> {
        Ok(self.entry_list_filter(this, Some(name), type_))
    }
}
