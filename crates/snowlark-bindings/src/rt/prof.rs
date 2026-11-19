// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Binding call profiling (`SNOWLARK_PROFILE=1`): calls and time per interface member.
//!
//! Every generated callback (attribute getters and setters, operations, constructors, indexed and
//! named getters) starts with [`enter`], which is one load of a flag read once from the environment
//! when profiling is off. When it is on, the embedder installs a [`Profiler`] in the isolate
//! ([`install`]); each call is then counted with its inclusive time and its self time (minus the
//! nested binding calls it made, such as the listeners a `dispatchEvent` ran into).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use super::HandleHash;

static ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var("SNOWLARK_PROFILE").is_ok_and(|v| v == "1"));

/// Whether profiling was asked for (`SNOWLARK_PROFILE=1`), read once per process.
#[inline(always)]
pub fn enabled() -> bool {
    *ENABLED
}

/// What one member's calls added up to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MemberStat {
    pub calls: u64,
    /// From entry to return, including nested binding calls.
    pub total: Duration,
    /// [`MemberStat::total`] minus the nested binding calls' totals.
    pub self_time: Duration,
}

struct Frame {
    name: &'static str,
    start: Instant,
    children: Duration,
}

/// The counters of one isolate.
#[derive(Default)]
pub struct Profiler {
    /// By the address of the member's name (a string literal of the generated code).
    members: RefCell<HashMap<u64, (&'static str, MemberStat), HandleHash>>,
    stack: RefCell<Vec<Frame>>,
}

/// Counts one call from [`enter`] to its drop.
pub struct Guard(Rc<Profiler>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.exit();
    }
}

/// Starts counting a call of `name` (`"Node.appendChild"`, `"get Node.firstChild"`), when profiling
/// is on and a [`Profiler`] is installed in the isolate.
#[inline(always)]
pub fn enter(scope: &v8::PinScope<'_, '_>, name: &'static str) -> Option<Guard> {
    if !enabled() {
        return None;
    }
    enter_slow(scope, name)
}

#[cold]
#[inline(never)]
fn enter_slow(scope: &v8::PinScope<'_, '_>, name: &'static str) -> Option<Guard> {
    let p = scope.get_slot::<Rc<Profiler>>()?.clone();
    p.push(name);
    Some(Guard(p))
}

/// Installs `profiler` in `isolate`: generated callbacks count into it from now on (when
/// [`enabled`]).
pub fn install(isolate: &mut v8::Isolate, profiler: Rc<Profiler>) {
    isolate.set_slot(profiler);
}

impl Profiler {
    pub fn new() -> Profiler {
        Profiler::default()
    }

    /// Counts the start of a call (what [`enter`] does).
    pub fn push(&self, name: &'static str) {
        self.stack.borrow_mut().push(Frame {
            name,
            start: Instant::now(),
            children: Duration::ZERO,
        });
    }

    /// Counts the end of the innermost call.
    pub fn exit(&self) {
        let mut stack = self.stack.borrow_mut();
        let Some(f) = stack.pop() else {
            return;
        };
        let total = f.start.elapsed();
        if let Some(parent) = stack.last_mut() {
            parent.children += total;
        }
        drop(stack);
        let mut members = self.members.borrow_mut();
        let e = members
            .entry(f.name.as_ptr() as u64)
            .or_insert((f.name, MemberStat::default()));
        e.1.calls += 1;
        e.1.total += total;
        e.1.self_time += total.saturating_sub(f.children);
    }

    /// The innermost member being called, if any (what forced a layout, say).
    pub fn current(&self) -> Option<&'static str> {
        self.stack.borrow().last().map(|f| f.name)
    }

    /// The counters by member name (members whose names are equal strings merged), most self time
    /// first.
    pub fn stats(&self) -> Vec<(&'static str, MemberStat)> {
        let mut by_name: HashMap<&'static str, MemberStat> = HashMap::new();
        for (name, s) in self.members.borrow().values() {
            let e = by_name.entry(name).or_default();
            e.calls += s.calls;
            e.total += s.total;
            e.self_time += s.self_time;
        }
        let mut v: Vec<_> = by_name.into_iter().collect();
        v.sort_by(|a, b| b.1.self_time.cmp(&a.1.self_time).then(a.0.cmp(b.0)));
        v
    }

    /// The `top` members by self time, as a table.
    pub fn report(&self, top: usize) -> String {
        let stats = self.stats();
        let calls: u64 = stats.iter().map(|s| s.1.calls).sum();
        let time: Duration = stats.iter().map(|s| s.1.self_time).sum();
        let mut o = String::new();
        let _ = writeln!(
            o,
            "[profile] bindings: {} members, {calls} calls, {:.1} ms self",
            stats.len(),
            ms(time)
        );
        let _ = writeln!(
            o,
            "[profile] {:>10} {:>10} {:>10} {:>9}  member",
            "calls", "self ms", "total ms", "self us"
        );
        for (name, s) in stats.iter().take(top) {
            let _ = writeln!(
                o,
                "[profile] {:>10} {:>10.1} {:>10.1} {:>9.2}  {name}",
                s.calls,
                ms(s.self_time),
                ms(s.total),
                ms(s.self_time) * 1000.0 / s.calls.max(1) as f64
            );
        }
        o
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_calls_with_self_and_total_time() {
        let p = Profiler::new();
        p.push("outer");
        std::thread::sleep(Duration::from_millis(2));
        p.push("inner");
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(p.current(), Some("inner"));
        p.exit();
        p.push("inner");
        p.exit();
        assert_eq!(p.current(), Some("outer"));
        p.exit();
        assert_eq!(p.current(), None);
        let stats = p.stats();
        let get = |n: &str| stats.iter().find(|s| s.0 == n).unwrap().1;
        let (outer, inner) = (get("outer"), get("inner"));
        assert_eq!((outer.calls, inner.calls), (1, 2));
        assert!(outer.total >= inner.total);
        assert_eq!(inner.total, inner.self_time);
        assert!(outer.self_time + inner.total <= outer.total + Duration::from_micros(50));
        assert!(p.report(10).contains("inner"));
    }

    #[test]
    fn an_unbalanced_exit_is_ignored() {
        let p = Profiler::new();
        p.exit();
        assert!(p.stats().is_empty());
    }
}
