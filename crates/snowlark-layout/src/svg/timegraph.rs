// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SMIL timing model of SVG Animations' time containers (SMIL 3.0 Timing and Synchronization
//! §5.4 "Timing model details" and §5.4.5 "Evaluation of begin and end time lists"): instance time
//! lists, the current interval of each timed element and its life cycle (getFirstInterval,
//! getNextInterval, restart), time dependents through syncbase and repeat values, and the
//! active duration (§5.4.4). The engine feeds it elements and instance times, moves it forward or
//! seeks it, and reads which events to dispatch and where each element is.

use std::collections::HashSet;

use rustc_hash::FxHashMap;
use snowlark_dom::NodeId;

use super::smil::{clock_value, offset_value, Sample};

/// One value of a `begin` or `end` list (SMIL 3 §5.3.2 "begin-value" and SVG Animations §3.6.7).
#[derive(Clone, Debug, PartialEq)]
pub enum TimeSpec {
    Offset(f64),
    Indefinite,
    /// `id.begin` or `id.end`, plus an offset.
    Sync {
        base: String,
        end: bool,
        offset: f64,
    },
    /// `id.repeat(n)`, plus an offset.
    Repeat {
        base: String,
        iteration: u32,
        offset: f64,
    },
    /// `id.event` or `event` (the element's target), plus an offset.
    Event {
        base: Option<String>,
        name: String,
        offset: f64,
    },
    /// `accessKey(c)`, plus an offset.
    AccessKey {
        key: char,
        offset: f64,
    },
}

/// The position of the sign that starts an offset in `s` (not escaped, after the token's start).
fn offset_start(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            // (the rest must be a clock value, else the sign is part of the name)
            b'+' | b'-' if i > 0 && offset_value(s[i..].replace(' ', "").as_str()).is_some() => {
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// A name with its `\` escapes removed.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl TimeSpec {
    /// Parses one begin or end value; `None` when it is in error.
    pub fn parse(s: &str) -> Option<TimeSpec> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        if s == "indefinite" {
            return Some(TimeSpec::Indefinite);
        }
        if let Some(v) = offset_value(&s.replace(' ', "")) {
            return Some(TimeSpec::Offset(v));
        }
        if s.starts_with("wallclock(") {
            return None;
        }
        let (name, offset) = match offset_start(s) {
            Some(i) => (
                s[..i].trim(),
                offset_value(&s[i..].replace(' ', "")).unwrap_or(0.0),
            ),
            None => (s, 0.0),
        };
        if let Some(rest) = name.strip_prefix("accessKey(") {
            let key = rest.strip_suffix(')')?.chars().next()?;
            return Some(TimeSpec::AccessKey { key, offset });
        }
        // The last unescaped dot separates the base's id from the token.
        let bytes = name.as_bytes();
        let mut dot = None;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 1,
                b'.' => dot = Some(i),
                _ => {}
            }
            i += 1;
        }
        let (base, token) = match dot {
            Some(d) => (Some(unescape(&name[..d])), name[d + 1..].trim()),
            None => (None, name),
        };
        if token.is_empty() || !token.as_bytes()[0].is_ascii_alphabetic() {
            return None;
        }
        Some(match (token, base) {
            ("begin", Some(base)) => TimeSpec::Sync {
                base,
                end: false,
                offset,
            },
            ("end", Some(base)) => TimeSpec::Sync {
                base,
                end: true,
                offset,
            },
            (t, Some(base)) if t.starts_with("repeat(") => {
                let n = t.strip_prefix("repeat(")?.strip_suffix(')')?;
                TimeSpec::Repeat {
                    base,
                    iteration: n.trim().parse().ok()?,
                    offset,
                }
            }
            (t, base) => TimeSpec::Event {
                base,
                name: unescape(t),
                offset,
            },
        })
    }

    /// Parses a `begin` or `end` attribute: its values that are not in error, in order.
    pub fn parse_list(s: &str) -> Vec<TimeSpec> {
        s.split(';').filter_map(TimeSpec::parse).collect()
    }
}

/// The `restart` attribute.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Restart {
    #[default]
    Always,
    WhenNotActive,
    Never,
}

/// A timed element's timing attributes (SVG Animations §3.6).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ElementTiming {
    pub begin: Vec<TimeSpec>,
    pub end: Vec<TimeSpec>,
    /// Whether there is an `end` attribute.
    pub has_end: bool,
    /// The simple duration (`None`: indefinite).
    pub dur: Option<f64>,
    /// `repeatCount` (infinite for `indefinite`).
    pub repeat_count: Option<f64>,
    pub repeat_dur: Option<f64>,
    pub min: f64,
    pub max: Option<f64>,
    pub restart: Restart,
    pub freeze: bool,
}

impl ElementTiming {
    /// Reads the timing attributes through `attr`.
    pub fn parse(attr: impl Fn(&str) -> Option<String>) -> ElementTiming {
        let begin_attr = attr("begin");
        let mut begin = begin_attr
            .as_deref()
            .map(TimeSpec::parse_list)
            .unwrap_or_default();
        if begin.is_empty() {
            // (absent, or wholly in error: the default, 0)
            begin.push(TimeSpec::Offset(0.0));
        }
        let end_attr = attr("end");
        let end = end_attr
            .as_deref()
            .map(TimeSpec::parse_list)
            .unwrap_or_default();
        let dur = attr("dur")
            .and_then(|d| clock_value(&d))
            .filter(|d| *d > 0.0);
        let count_or_dur = |name: &str, parse: fn(&str) -> Option<f64>| {
            attr(name).and_then(|v| {
                let v = v.trim();
                if v == "indefinite" {
                    Some(f64::INFINITY)
                } else {
                    parse(v)
                }
            })
        };
        let repeat_count = count_or_dur("repeatCount", |v| {
            v.parse::<f64>().ok().filter(|c| *c > 0.0 && c.is_finite())
        });
        let repeat_dur = count_or_dur("repeatDur", |v| clock_value(v).filter(|d| *d > 0.0));
        let min = attr("min")
            .and_then(|v| clock_value(&v))
            .filter(|m| *m >= 0.0)
            .unwrap_or(0.0);
        let max = attr("max")
            .and_then(|v| clock_value(&v))
            .filter(|m| *m > 0.0);
        let (min, max) = match max {
            // (§5.3.3: a min above max ignores both)
            Some(m) if m < min => (0.0, None),
            m => (min, m),
        };
        let restart = match attr("restart").as_deref().map(str::trim) {
            Some("whenNotActive") => Restart::WhenNotActive,
            Some("never") => Restart::Never,
            _ => Restart::Always,
        };
        ElementTiming {
            has_end: end_attr.is_some(),
            begin,
            end,
            dur,
            repeat_count,
            repeat_dur,
            min,
            max,
            restart,
            freeze: attr("fill").as_deref().map(str::trim) == Some("freeze"),
        }
    }

    /// Whether the `end` list has conditions that may yet generate instances.
    fn end_has_events(&self) -> bool {
        self.end.iter().any(|s| {
            matches!(
                s,
                TimeSpec::Event { .. } | TimeSpec::Repeat { .. } | TimeSpec::AccessKey { .. }
            )
        })
    }

    /// The repeat duration without an `end` constraint (SMIL 3 §5.4.4 "Intermediate active
    /// duration", without the `end` part).
    fn repeating_duration(&self) -> f64 {
        let d = self.dur.unwrap_or(f64::INFINITY);
        match (self.repeat_count, self.repeat_dur) {
            (None, None) => d,
            (Some(rc), None) => rc * d,
            (None, Some(rd)) => rd,
            (Some(rc), Some(rd)) => (rc * d).min(rd),
        }
    }

    /// calcActiveEnd (§5.4.4): the active end of an interval from `begin`, with the `end` instance
    /// `end` (infinite: none), `min` and `max` applied.
    fn active_end(&self, begin: f64, end: f64) -> f64 {
        let mut ad = (end - begin).min(self.repeating_duration());
        // (with an `end` and neither `dur` nor repeats, the end alone decides)
        if end.is_finite()
            && self.dur.is_none()
            && self.repeat_count.is_none()
            && self.repeat_dur.is_none()
        {
            ad = end - begin;
        }
        ad = ad.max(self.min);
        if let Some(m) = self.max {
            ad = ad.min(m);
        }
        begin + ad
    }
}

/// Where an instance time came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// An offset value.
    Static,
    /// `beginElement()` and friends, an event or a repeat notice: cleared when the element resets.
    Dynamic,
    /// A syncbase's interval: (the base, its interval's serial, the base's end).
    Sync(NodeId, u64, bool),
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Instance {
    time: f64,
    origin: Origin,
    /// The offset from the syncbase's time.
    offset: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Interval {
    begin: f64,
    end: f64,
    serial: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    /// The current interval (if any) has not begun.
    #[default]
    Waiting,
    Active,
    /// The current interval ended; no later one exists yet.
    Post,
}

/// A time dependent's condition on a base.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Dependent {
    element: NodeId,
    /// The condition is in the dependent's `begin` list (else `end`).
    in_begin: bool,
    /// On the base's end (else begin).
    base_end: bool,
    offset: f64,
}

#[derive(Clone, Debug, Default)]
struct Element {
    timing: ElementTiming,
    begin: Vec<Instance>,
    end: Vec<Instance>,
    state: State,
    /// The current interval.
    interval: Option<Interval>,
    /// The last interval that ended (what `fill="freeze"` shows until the next one begins).
    last: Option<Interval>,
    /// The repeat iteration last reported.
    iteration: u32,
    dependents: Vec<Dependent>,
    /// `id.repeat(n)` conditions on this element.
    repeat_dependents: Vec<(u32, Dependent)>,
}

fn insert_sorted(list: &mut Vec<Instance>, i: Instance) {
    let at = list.partition_point(|x| x.time <= i.time);
    list.insert(at, i);
}

/// What happened to an element, for the DOM events (SVG Animations §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeEventKind {
    Begin,
    End,
    Repeat(u32),
}

/// The timed elements of one time container.
#[derive(Debug, Default)]
pub struct Timegraph {
    elements: FxHashMap<NodeId, Element>,
    /// The elements in document order.
    order: Vec<NodeId>,
    time: f64,
    serial: u64,
    /// The events to dispatch, in order.
    pub events: Vec<(NodeId, TimeEventKind)>,
}

impl Timegraph {
    /// The container's current time, seconds.
    pub fn time(&self) -> f64 {
        self.time
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.elements.contains_key(&id)
    }

    /// Adds an element or gives it new timing attributes, keeping the instance times that are not
    /// its offsets, and re-evaluates its interval. `order` is its place in document order.
    pub fn set_element(&mut self, id: NodeId, timing: ElementTiming) {
        if !self.elements.contains_key(&id) {
            self.order.push(id);
        }
        let e = self.elements.entry(id).or_default();
        e.timing = timing;
        e.begin.retain(|i| i.origin != Origin::Static);
        e.end.retain(|i| i.origin != Origin::Static);
        let (begin, end) = (e.timing.begin.clone(), e.timing.end.clone());
        for s in begin {
            if let TimeSpec::Offset(t) = s {
                insert_sorted(
                    &mut e.begin,
                    Instance {
                        time: t,
                        origin: Origin::Static,
                        offset: 0.0,
                    },
                );
            }
        }
        for s in end {
            if let TimeSpec::Offset(t) = s {
                insert_sorted(
                    &mut e.end,
                    Instance {
                        time: t,
                        origin: Origin::Static,
                        offset: 0.0,
                    },
                );
            }
        }
        // (a waiting element is reconsidered from scratch; an active one keeps its begin)
        if e.state == State::Waiting {
            e.interval = None;
        }
        self.reevaluate(id, &mut HashSet::new());
    }

    /// [`Timegraph::set_element`] when the element is new or its timing differs.
    pub fn set_element_if_changed(&mut self, id: NodeId, timing: &ElementTiming) {
        if self.elements.get(&id).is_none_or(|e| e.timing != *timing) {
            self.set_element(id, timing.clone());
        }
    }

    /// Removes an element: its instances in dependents go.
    pub fn remove_element(&mut self, id: NodeId) {
        self.elements.remove(&id);
        self.order.retain(|&e| e != id);
        let dependents: Vec<NodeId> = self.elements.keys().copied().collect();
        for d in dependents {
            let changed = {
                let e = self.elements.get_mut(&d).expect("an element");
                let before = e.begin.len() + e.end.len();
                let keep = |i: &Instance| !matches!(i.origin, Origin::Sync(b, ..) if b == id);
                e.begin.retain(keep);
                e.end.retain(keep);
                e.dependents.retain(|x| x.element != id);
                e.repeat_dependents.retain(|x| x.1.element != id);
                before != e.begin.len() + e.end.len()
            };
            if changed {
                self.reevaluate(d, &mut HashSet::new());
            }
        }
    }

    /// Puts the elements in document order (the order of simultaneous events and of the sandwich).
    pub fn set_order(&mut self, order: Vec<NodeId>) {
        self.order = order
            .into_iter()
            .filter(|e| self.elements.contains_key(e))
            .collect();
    }

    /// Makes `dependent`'s condition (its `begin` list when `in_begin`) on `base`'s begin or end
    /// known: the base's current and past intervals are not replayed, its current one is.
    pub fn add_sync(
        &mut self,
        base: NodeId,
        dependent: NodeId,
        in_begin: bool,
        base_end: bool,
        offset: f64,
    ) {
        let d = Dependent {
            element: dependent,
            in_begin,
            base_end,
            offset,
        };
        let Some(b) = self.elements.get_mut(&base) else {
            return;
        };
        if b.dependents.contains(&d) {
            return;
        }
        b.dependents.push(d);
        let current = b.interval.or(b.last);
        if let Some(i) = current {
            self.notify(base, i, d, &mut HashSet::new());
        }
    }

    /// Makes `dependent`'s condition on `base`'s repeat `iteration` known.
    pub fn add_repeat_sync(
        &mut self,
        base: NodeId,
        dependent: NodeId,
        in_begin: bool,
        iteration: u32,
        offset: f64,
    ) {
        if let Some(b) = self.elements.get_mut(&base) {
            let d = Dependent {
                element: dependent,
                in_begin,
                base_end: false,
                offset,
            };
            if !b.repeat_dependents.contains(&(iteration, d)) {
                b.repeat_dependents.push((iteration, d));
            }
        }
    }

    /// Forgets the dependencies on and of the elements (before they are worked out again).
    pub fn clear_dependencies(&mut self) {
        for e in self.elements.values_mut() {
            e.dependents.clear();
            e.repeat_dependents.clear();
            e.begin.retain(|i| !matches!(i.origin, Origin::Sync(..)));
            e.end.retain(|i| !matches!(i.origin, Origin::Sync(..)));
        }
    }

    /// Adds an instance time (an event, `beginElement()`...): `t` in container time.
    pub fn add_instance(&mut self, id: NodeId, begin: bool, t: f64) {
        let Some(e) = self.elements.get_mut(&id) else {
            return;
        };
        let i = Instance {
            time: t,
            origin: Origin::Dynamic,
            offset: 0.0,
        };
        insert_sorted(if begin { &mut e.begin } else { &mut e.end }, i);
        self.instance_added(id, begin, t, &mut HashSet::new());
    }

    /// An instance was added at `t` (§5.4.5 "Interaction with restart semantics").
    fn instance_added(&mut self, id: NodeId, begin: bool, t: f64, seen: &mut HashSet<NodeId>) {
        let Some(e) = self.elements.get_mut(&id) else {
            return;
        };
        if e.state == State::Active && begin {
            // Playing: only restart="always" ends the interval at the new time.
            if e.timing.restart == Restart::Always {
                if let Some(mut i) = e.interval {
                    if t > i.begin && t < i.end {
                        i.end = t;
                        e.interval = Some(i);
                        self.interval_end_changed(id, i, seen);
                    }
                }
            }
            return;
        }
        self.reevaluate(id, seen);
    }

    /// Whether a begin at `begin` is allowed by `restart` given the last interval.
    fn first_begin(e: &Element, after: f64, strict: bool) -> Option<f64> {
        e.begin
            .iter()
            .map(|i| i.time)
            .find(|t| if strict { *t > after } else { *t >= after })
    }

    /// getFirstInterval/getNextInterval (§5.4.5) from the instance lists, for an interval beginning
    /// at or after `after`.
    fn next_interval(e: &Element, mut after: f64, first: bool) -> Option<(f64, f64)> {
        let mut strict = false;
        loop {
            let begin = Self::first_begin(e, after, strict)?;
            let end = if e.timing.has_end {
                let mut found = e.end.iter().map(|i| i.time).find(|t| *t >= begin);
                // (a zero-length interval at the end of the last one is not repeated)
                if !first && found == Some(after) && begin == after {
                    found = e.end.iter().map(|i| i.time).find(|t| *t > begin);
                }
                match found {
                    Some(t) => e.timing.active_end(begin, t),
                    None if e.timing.end_has_events() || e.end.is_empty() => {
                        e.timing.active_end(begin, f64::INFINITY)
                    }
                    None => return None,
                }
            } else {
                e.timing.active_end(begin, f64::INFINITY)
            };
            if !first || end > 0.0 || (begin == 0.0 && end == 0.0) {
                return Some((begin, end));
            }
            // An interval wholly before the container's begin is filtered out.
            after = end;
            strict = end <= begin;
        }
    }

    /// Re-evaluates an element's current interval after its instance lists changed.
    fn reevaluate(&mut self, id: NodeId, seen: &mut HashSet<NodeId>) {
        if !seen.insert(id) {
            return;
        }
        let Some(e) = self.elements.get(&id) else {
            return;
        };
        match e.state {
            State::Active => {
                // Only the end can change while playing.
                let Some(mut i) = e.interval else {
                    return;
                };
                let end = if e.timing.has_end {
                    e.end
                        .iter()
                        .map(|x| x.time)
                        .find(|t| *t >= i.begin)
                        .unwrap_or(f64::INFINITY)
                } else {
                    f64::INFINITY
                };
                let new_end = e.timing.active_end(i.begin, end);
                if new_end != i.end {
                    i.end = new_end;
                    self.elements.get_mut(&id).expect("an element").interval = Some(i);
                    self.interval_end_changed(id, i, seen);
                }
            }
            State::Waiting | State::Post => {
                // After a last interval, restart decides whether another may come.
                let after = match e.last {
                    Some(_) if e.timing.restart == Restart::Never => return,
                    Some(l) => l.end,
                    None => f64::NEG_INFINITY,
                };
                let found = Self::next_interval(e, after, e.last.is_none());
                let old = e.interval;
                match (found, old) {
                    (Some((b, en)), Some(o)) if o.begin == b && o.end == en => {}
                    (Some((b, en)), Some(o)) if o.begin == b => {
                        let i = Interval { end: en, ..o };
                        let e = self.elements.get_mut(&id).expect("an element");
                        e.interval = Some(i);
                        self.interval_end_changed(id, i, seen);
                    }
                    (Some((b, en)), _) => {
                        self.serial += 1;
                        let i = Interval {
                            begin: b,
                            end: en,
                            serial: self.serial,
                        };
                        let e = self.elements.get_mut(&id).expect("an element");
                        e.interval = Some(i);
                        e.state = State::Waiting;
                        if let Some(o) = old {
                            self.remove_dependent_instances(id, o, seen);
                        }
                        self.new_interval(id, i, seen);
                    }
                    (None, Some(o)) => {
                        // Pruned: its dependents' instances go.
                        let e = self.elements.get_mut(&id).expect("an element");
                        e.interval = None;
                        self.remove_dependent_instances(id, o, seen);
                    }
                    (None, None) => {}
                }
            }
        }
        seen.remove(&id);
    }

    /// The new interval notice to the time dependents.
    fn new_interval(&mut self, id: NodeId, i: Interval, seen: &mut HashSet<NodeId>) {
        let deps = match self.elements.get(&id) {
            Some(e) => e.dependents.clone(),
            None => return,
        };
        for d in deps {
            self.notify(id, i, d, seen);
        }
    }

    /// Gives a dependent its instance for interval `i` of `base`.
    fn notify(&mut self, base: NodeId, i: Interval, d: Dependent, seen: &mut HashSet<NodeId>) {
        let t = if d.base_end { i.end } else { i.begin };
        if !t.is_finite() {
            return;
        }
        let time = t + d.offset;
        let Some(e) = self.elements.get_mut(&d.element) else {
            return;
        };
        let origin = Origin::Sync(base, i.serial, d.base_end);
        let list = if d.in_begin { &mut e.begin } else { &mut e.end };
        if list
            .iter()
            .any(|x| x.origin == origin && x.offset == d.offset)
        {
            return;
        }
        insert_sorted(
            list,
            Instance {
                time,
                origin,
                offset: d.offset,
            },
        );
        self.instance_added(d.element, d.in_begin, time, seen);
    }

    /// The end of the current interval changed: the instances on it follow.
    fn interval_end_changed(&mut self, id: NodeId, i: Interval, seen: &mut HashSet<NodeId>) {
        let deps = match self.elements.get(&id) {
            Some(e) => e.dependents.clone(),
            None => return,
        };
        for d in deps.into_iter().filter(|d| d.base_end) {
            let origin = Origin::Sync(id, i.serial, true);
            let Some(e) = self.elements.get_mut(&d.element) else {
                continue;
            };
            let list = if d.in_begin { &mut e.begin } else { &mut e.end };
            let had = list
                .iter()
                .position(|x| x.origin == origin && x.offset == d.offset);
            if let Some(p) = had {
                list.remove(p);
            }
            if i.end.is_finite() {
                insert_sorted(
                    list,
                    Instance {
                        time: i.end + d.offset,
                        origin,
                        offset: d.offset,
                    },
                );
            }
            self.reevaluate(d.element, seen);
        }
    }

    /// An interval was pruned: the instances made from it go.
    fn remove_dependent_instances(&mut self, id: NodeId, i: Interval, seen: &mut HashSet<NodeId>) {
        let deps = match self.elements.get(&id) {
            Some(e) => e.dependents.clone(),
            None => return,
        };
        for d in deps {
            let Some(e) = self.elements.get_mut(&d.element) else {
                continue;
            };
            let gone = |x: &Instance| matches!(x.origin, Origin::Sync(b, s, _) if b == id && s == i.serial);
            let list = if d.in_begin { &mut e.begin } else { &mut e.end };
            let before = list.len();
            list.retain(|x| !gone(x));
            if before != list.len() {
                self.reevaluate(d.element, seen);
            }
        }
    }

    /// The next time something happens to an element.
    fn next_change(e: &Element) -> Option<f64> {
        let i = e.interval?;
        match e.state {
            State::Waiting => Some(i.begin),
            State::Active => {
                let mut t = i.end;
                if let Some(d) = e.timing.dur {
                    let next = i.begin + (e.iteration as f64 + 1.0) * d;
                    if next < t {
                        t = next;
                    }
                }
                t.is_finite().then_some(t)
            }
            State::Post => None,
        }
    }

    /// Moves the time forward to `to`, in order of what happens (`silent`: no events, as a seek).
    fn run(&mut self, to: f64, silent: bool) {
        let mut guard = 0u32;
        loop {
            guard += 1;
            // (cycles that produce an interval each turn stop here)
            if guard > 100_000 {
                break;
            }
            let mut next: Option<(f64, NodeId)> = None;
            for &id in &self.order {
                if let Some(t) = self.elements.get(&id).and_then(Self::next_change) {
                    if t <= to && next.is_none_or(|n| t < n.0) {
                        next = Some((t, id));
                    }
                }
            }
            let Some((t, id)) = next else {
                break;
            };
            self.time = self.time.max(t);
            self.step(id, t, silent);
        }
        self.time = to;
    }

    /// One change of an element at `t`.
    fn step(&mut self, id: NodeId, t: f64, silent: bool) {
        let e = self.elements.get_mut(&id).expect("an element");
        let i = e.interval.expect("a current interval");
        match e.state {
            State::Waiting => {
                e.state = State::Active;
                e.iteration = 0;
                // A begin instance already in the interval ends it there when `restart` allows.
                let next = e.begin.iter().map(|x| x.time).find(|x| *x > i.begin);
                let cut = next.filter(|n| *n < i.end && e.timing.restart == Restart::Always);
                let i = match cut {
                    Some(end) => Interval { end, ..i },
                    None => i,
                };
                e.interval = Some(i);
                // §5.4.3 "Resetting element state": past dynamic times go, but the one this
                // interval began from.
                e.begin
                    .retain(|x| x.origin != Origin::Dynamic || x.time == i.begin || x.time > t);
                e.end.retain(|x| x.origin != Origin::Dynamic || x.time >= t);
                if !silent {
                    self.events.push((id, TimeEventKind::Begin));
                }
                if cut.is_some() {
                    self.interval_end_changed(id, i, &mut HashSet::new());
                }
            }
            State::Active if t >= i.end => {
                e.state = State::Post;
                e.last = Some(i);
                e.interval = None;
                if !silent {
                    self.events.push((id, TimeEventKind::End));
                }
                // The next interval, if restart allows.
                self.reevaluate(id, &mut HashSet::new());
            }
            State::Active => {
                // A repeat boundary.
                e.iteration += 1;
                let n = e.iteration;
                let deps: Vec<Dependent> = e
                    .repeat_dependents
                    .iter()
                    .filter(|d| d.0 == n)
                    .map(|d| d.1)
                    .collect();
                if !silent {
                    self.events.push((id, TimeEventKind::Repeat(n)));
                }
                for d in deps {
                    let time = t + d.offset;
                    if let Some(de) = self.elements.get_mut(&d.element) {
                        let list = if d.in_begin {
                            &mut de.begin
                        } else {
                            &mut de.end
                        };
                        insert_sorted(
                            list,
                            Instance {
                                time,
                                origin: Origin::Dynamic,
                                offset: 0.0,
                            },
                        );
                    }
                    self.instance_added(d.element, d.in_begin, time, &mut HashSet::new());
                }
            }
            State::Post => {}
        }
    }

    /// The time moves on to `to` (an animation frame).
    pub fn advance(&mut self, to: f64) {
        // Several repeats within one step are one repeatEvent, of the last iteration.
        let from = self.events.len();
        self.run(to.max(self.time), false);
        let mut last: FxHashMap<NodeId, usize> = FxHashMap::default();
        for (i, (id, k)) in self.events.iter().enumerate().skip(from) {
            if matches!(k, TimeEventKind::Repeat(_)) {
                last.insert(*id, i);
            }
        }
        let mut at = 0usize;
        self.events.retain(|(id, k)| {
            let here = at;
            at += 1;
            here < from || !matches!(k, TimeEventKind::Repeat(_)) || last.get(id) == Some(&here)
        });
    }

    /// `setCurrentTime()`: the timegraph is played again silently to `to`; then an element that
    /// was active and no longer is in that interval gets `endEvent`, and one that is active in
    /// another interval now gets `beginEvent` (SVG Animations §5.1). `running`: the timeline plays
    /// on, and the iterations the seek skipped are one `repeatEvent` of the next sample; a paused
    /// one samples nothing more, so a seek alone dispatches no `repeatEvent`.
    pub fn seek(&mut self, to: f64, running: bool) {
        let before: FxHashMap<NodeId, (f64, u32)> = self
            .elements
            .iter()
            .filter(|(_, e)| e.state == State::Active)
            .filter_map(|(id, e)| Some((*id, (e.interval?.begin, e.iteration))))
            .collect();
        for e in self.elements.values_mut() {
            e.state = State::Waiting;
            e.interval = None;
            e.last = None;
            e.iteration = 0;
            e.begin.retain(|i| !matches!(i.origin, Origin::Sync(..)));
            e.end.retain(|i| !matches!(i.origin, Origin::Sync(..)));
        }
        self.time = f64::NEG_INFINITY;
        let order = self.order.clone();
        for &id in &order {
            self.reevaluate(id, &mut HashSet::new());
        }
        self.time = 0.0;
        self.run(to.max(0.0), true);
        for &id in &order {
            let Some(e) = self.elements.get(&id) else {
                continue;
            };
            let now = (e.state == State::Active)
                .then(|| e.interval.map(|i| i.begin))
                .flatten();
            let iteration = e.iteration;
            let was = before.get(&id).copied();
            let same = was.map(|w| w.0) == now;
            if !same {
                if was.is_some() {
                    self.events.push((id, TimeEventKind::End));
                }
                if now.is_some() {
                    self.events.push((id, TimeEventKind::Begin));
                }
            }
            let reached = if same { was.map_or(0, |w| w.1) } else { 0 };
            if running && now.is_some() && iteration > reached {
                self.events.push((id, TimeEventKind::Repeat(iteration)));
            }
        }
    }

    /// Where an element is at the current time; `None` when it has no effect (SMIL 3 §5.4.5, fill).
    pub fn sample(&self, id: NodeId) -> Option<Sample> {
        let e = self.elements.get(&id)?;
        let t = self.time;
        let d = e.timing.dur.unwrap_or(f64::INFINITY);
        if e.state == State::Active {
            let i = e.interval?;
            let elapsed = (t - i.begin).max(0.0);
            if !d.is_finite() {
                return Some(Sample {
                    begin: i.begin,
                    progress: 0.0,
                    iteration: 0,
                });
            }
            let iteration = (elapsed / d).floor();
            return Some(Sample {
                begin: i.begin,
                progress: (elapsed - iteration * d) / d,
                iteration: iteration as u32,
            });
        }
        if !e.timing.freeze {
            return None;
        }
        let i = e.last?;
        // Frozen at the end of the active duration (§5.4.4, fill: an end on an iteration boundary
        // shows the end of the last iteration).
        if !d.is_finite() || !i.end.is_finite() {
            return Some(Sample {
                begin: i.begin,
                progress: 0.0,
                iteration: 0,
            });
        }
        let iterations = (i.end - i.begin) / d;
        let whole = iterations.floor();
        let (progress, iteration) = if iterations > 0.0 && (iterations - whole).abs() < 1e-9 {
            (1.0, whole - 1.0)
        } else {
            (iterations - whole, whole)
        };
        Some(Sample {
            begin: i.begin,
            progress,
            iteration: iteration.max(0.0) as u32,
        })
    }

    /// The begin of the element's current interval, or of the one to come (`getStartTime()`).
    pub fn current_begin(&self, id: NodeId) -> Option<f64> {
        self.elements.get(&id)?.interval.map(|i| i.begin)
    }

    /// The next time, after the current one, that an element changes.
    pub fn next_time(&self) -> Option<f64> {
        self.elements
            .values()
            .filter_map(Self::next_change)
            .fold(None, |a: Option<f64>, t| Some(a.map_or(t, |a| a.min(t))))
    }

    /// Whether any element is playing (each animation frame may change what shows).
    pub fn playing(&self) -> bool {
        self.elements.values().any(|e| e.state == State::Active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> NodeId {
        NodeId::from_bits((1 << 32) | n).unwrap()
    }

    fn timing(begin: &str, dur: &str, extra: &[(&str, &str)]) -> ElementTiming {
        ElementTiming::parse(|a| match a {
            "begin" => Some(begin.to_owned()),
            "dur" => Some(dur.to_owned()),
            _ => extra.iter().find(|e| e.0 == a).map(|e| e.1.to_owned()),
        })
    }

    #[test]
    fn parses_time_specs() {
        assert_eq!(TimeSpec::parse("2s"), Some(TimeSpec::Offset(2.0)));
        assert_eq!(TimeSpec::parse("-1s"), Some(TimeSpec::Offset(-1.0)));
        assert_eq!(TimeSpec::parse("indefinite"), Some(TimeSpec::Indefinite));
        assert_eq!(
            TimeSpec::parse("foo-bar.end+1s"),
            Some(TimeSpec::Sync {
                base: "foo-bar".into(),
                end: true,
                offset: 1.0
            })
        );
        assert_eq!(
            TimeSpec::parse("a.repeat(3) - 0.5s"),
            Some(TimeSpec::Repeat {
                base: "a".into(),
                iteration: 3,
                offset: -0.5
            })
        );
        assert_eq!(
            TimeSpec::parse("click+0.5s"),
            Some(TimeSpec::Event {
                base: None,
                name: "click".into(),
                offset: 0.5
            })
        );
        assert_eq!(
            TimeSpec::parse("x.y\\.z.begin"),
            Some(TimeSpec::Sync {
                base: "x.y.z".into(),
                end: false,
                offset: 0.0
            })
        );
    }

    #[test]
    fn intervals_and_events() {
        let mut g = Timegraph::default();
        let a = id(1);
        g.set_element(
            a,
            timing("1s; 5s", "1s", &[("repeatCount", "2"), ("fill", "freeze")]),
        );
        g.advance(0.5);
        assert!(g.sample(a).is_none());
        g.advance(1.5);
        assert_eq!(g.sample(a).unwrap().progress, 0.5);
        g.advance(3.5);
        assert_eq!(g.sample(a).unwrap().progress, 1.0);
        g.advance(6.0);
        assert_eq!(g.sample(a).unwrap().begin, 5.0);
        use TimeEventKind::*;
        assert_eq!(
            g.events.iter().map(|e| e.1).collect::<Vec<_>>(),
            vec![Begin, Repeat(1), End, Begin, Repeat(1)]
        );
    }

    #[test]
    fn syncbase_and_restart() {
        let mut g = Timegraph::default();
        let (a, b) = (id(1), id(2));
        g.set_element(a, timing("1s", "2s", &[]));
        g.set_element(b, timing("indefinite", "1s", &[]));
        g.add_sync(a, b, true, true, 0.5);
        g.advance(3.6);
        assert_eq!(g.sample(b).unwrap().begin, 3.5);
        // restart="always": a new begin ends the interval there.
        g.add_instance(b, true, 3.7);
        g.advance(3.8);
        assert_eq!(g.sample(b).unwrap().begin, 3.7);
    }

    #[test]
    fn seeking_dispatches_only_state_changes() {
        let mut g = Timegraph::default();
        let a = id(1);
        g.set_element(a, timing("5s", "1s", &[("repeatCount", "2")]));
        g.seek(5.5, false);
        assert_eq!(g.events, vec![(a, TimeEventKind::Begin)]);
        g.events.clear();
        g.seek(6.5, false);
        assert!(g.events.is_empty());
        g.seek(10.0, false);
        assert_eq!(g.events, vec![(a, TimeEventKind::End)]);
    }
}
