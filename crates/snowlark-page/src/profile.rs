// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Page profiling (`SNOWLARK_PROFILE=1`): V8's sampling CPU profile and the binding call counters,
//! printed when the page closes, and a renderer statistics line per rendering update with the
//! forced style and layout updates script caused since the previous one, by the API that forced
//! them.
//!
//! Everything here is off (and the engine holds no [`PageProfile`]) unless the variable is set. All
//! output goes to stderr as `[profile]` and `[render]` lines.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::Write;
use std::rc::Rc;
use std::time::{Duration, Instant};

use snowlark_bindings::rt::prof::{self, Profiler};
use snowlark_dom::NodeId;
use snowlark_render::{RenderStats, Timings};

/// Whether `SNOWLARK_PROFILE=1` asked for profiling.
pub use snowlark_bindings::rt::prof::enabled;

/// Forced style or layout updates by one API.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Forced {
    /// Reads that brought style or layout up to date.
    pub calls: u64,
    /// Of those, the ones that had something to do (elements restyled or boxes laid out).
    pub dirty: u64,
    pub time: Duration,
}

/// Whether `SNOWLARK_PROFILE_NATIVE=only` asked for the native sampler alone (with the
/// `performance.mark` tags, so scored windows can be told apart), nothing else of profiling on.
pub fn native_only() -> bool {
    !enabled() && std::env::var("SNOWLARK_PROFILE_NATIVE").as_deref() == Ok("only")
}

/// The profiling state of one page.
pub struct PageProfile {
    pub bindings: Rc<Profiler>,
    /// The `performance.mark` calls, for the native sampler to tag its samples with.
    pub marks: snowlark_js::native_profile::SampleMarks,
    /// Forced updates since the last rendering update, and over the page's lifetime; by
    /// `(API, "style" | "layout")`.
    forced: RefCell<HashMap<(&'static str, &'static str), Forced>>,
    forced_all: RefCell<HashMap<(&'static str, &'static str), Forced>>,
    /// Each renderer's statistics at the last rendering update.
    last: RefCell<rustc_hash::FxHashMap<NodeId, (RenderStats, Timings)>>,
    updates: Cell<u64>,
    started: Instant,
}

impl PageProfile {
    /// A profile when `SNOWLARK_PROFILE=1`.
    pub fn from_env() -> Option<PageProfile> {
        prof::enabled().then(PageProfile::new)
    }

    pub fn new() -> PageProfile {
        PageProfile {
            bindings: Rc::new(Profiler::new()),
            marks: Default::default(),
            forced: RefCell::default(),
            forced_all: RefCell::default(),
            last: RefCell::default(),
            updates: Cell::new(0),
            started: Instant::now(),
        }
    }

    /// Counts a style (`kind` "style") or layout ("layout") update a script read forced, with the
    /// renderer's statistics before and after it.
    pub fn forced(
        &self,
        kind: &'static str,
        time: Duration,
        before: &RenderStats,
        after: &RenderStats,
    ) {
        let api = self.bindings.current().unwrap_or("(engine)");
        let dirty = after.restyled != before.restyled || after.laid_out != before.laid_out;
        for map in [&self.forced, &self.forced_all] {
            let mut m = map.borrow_mut();
            let e = m.entry((api, kind)).or_default();
            e.calls += 1;
            e.dirty += u64::from(dirty);
            e.time += time;
        }
    }

    /// The `[render]` lines of one rendering update: per renderer (`label`, current statistics)
    /// what changed since the last update, then the forced updates since then. Empty when nothing
    /// happened.
    pub fn rendering_update(&self, renderers: &[(NodeId, String, RenderStats, Timings)]) -> String {
        let n = self.updates.get() + 1;
        self.updates.set(n);
        let at = self.started.elapsed().as_secs_f64() * 1000.0;
        let mut o = String::new();
        let mut last = self.last.borrow_mut();
        for (doc, label, s, t) in renderers {
            let (ps, pt) = last.get(doc).copied().unwrap_or_default();
            last.insert(*doc, (*s, *t));
            if s.restyled == ps.restyled && s.laid_out == ps.laid_out && t.layouts == pt.layouts {
                continue;
            }
            let u = &s.update;
            let pu = &ps.update;
            let _ = writeln!(
                o,
                "[render] #{n} {at:.1}ms {label}: restyled {} (unchanged {} repaint {} relayout {} reconstruct {}), builds full {} incremental {}, boxes built {} rebuilt containers {} restyled in place {}, laid out {} reused {} again {} (flex/grid built {} runs {}), shaped {} cached {} (words shaped {} cached {}); style {:.2}ms x{} build {:.2}ms layout {:.2}ms x{} sheets {}; traversed {} matched {} shared {}",
                s.restyled - ps.restyled,
                s.unchanged - ps.unchanged,
                s.repaint - ps.repaint,
                s.relayout - ps.relayout,
                s.reconstruct - ps.reconstruct,
                s.full_builds - ps.full_builds,
                s.incremental_updates - ps.incremental_updates,
                u.boxes_built - pu.boxes_built,
                u.containers_rebuilt - pu.containers_rebuilt,
                u.restyled_in_place - pu.restyled_in_place,
                s.laid_out - ps.laid_out,
                s.reused - ps.reused,
                s.relaid - ps.relaid,
                s.containers_built - ps.containers_built,
                s.algorithm_runs - ps.algorithm_runs,
                s.shaped - ps.shaped,
                s.shape_cache_hits - ps.shape_cache_hits,
                s.words_shaped - ps.words_shaped,
                s.words_cached - ps.words_cached,
                t.style_ms - pt.style_ms,
                t.restyles - pt.restyles,
                t.build_ms - pt.build_ms,
                t.layout_ms - pt.layout_ms,
                t.layouts - pt.layouts,
                s.sheet_changes.saturating_sub(ps.sheet_changes),
                s.traversed - ps.traversed,
                s.matched - ps.matched,
                s.shared - ps.shared,
            );
        }
        drop(last);
        let forced = std::mem::take(&mut *self.forced.borrow_mut());
        if !forced.is_empty() {
            let _ = writeln!(o, "[render] #{n} forced: {}", format_forced(&forced, 8));
        }
        o
    }

    /// The report printed when the page closes: forced updates over the page's lifetime and the
    /// binding counters.
    pub fn report(&self, top: usize) -> String {
        let mut o = String::new();
        let all = self.forced_all.borrow();
        let _ = writeln!(
            o,
            "[profile] rendering updates: {}; forced style/layout by API: {}",
            self.updates.get(),
            if all.is_empty() {
                "none".to_string()
            } else {
                format_forced(&all, top)
            }
        );
        o.push_str(&self.bindings.report(top));
        o
    }
}

impl Default for PageProfile {
    fn default() -> Self {
        Self::new()
    }
}

/// `api kind xN (D dirty, T ms)` for the `top` entries by time.
fn format_forced(m: &HashMap<(&'static str, &'static str), Forced>, top: usize) -> String {
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|a, b| b.1.time.cmp(&a.1.time).then(a.0.cmp(b.0)));
    v.iter()
        .take(top)
        .map(|((api, kind), f)| {
            format!(
                "{api} {kind} x{} ({} dirty, {:.2} ms)",
                f.calls,
                f.dirty,
                f.time.as_secs_f64() * 1000.0
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_updates_are_attributed_to_the_current_binding() {
        let p = PageProfile::new();
        let clean = RenderStats::default();
        let dirty = RenderStats {
            laid_out: 3,
            ..Default::default()
        };
        p.bindings.push("get HTMLElement.offsetWidth");
        p.forced("layout", Duration::from_millis(2), &clean, &dirty);
        p.forced("layout", Duration::from_millis(1), &dirty, &dirty);
        p.bindings.exit();
        p.forced("style", Duration::ZERO, &clean, &clean);
        let line = p.rendering_update(&[]);
        assert!(
            line.contains("get HTMLElement.offsetWidth layout x2 (1 dirty, 3.00 ms)"),
            "{line}"
        );
        assert!(line.contains("(engine) style x1 (0 dirty"), "{line}");
        // The per-update counts start again; the lifetime counts stay.
        assert_eq!(p.rendering_update(&[]), "");
        assert!(p.report(10).contains("offsetWidth layout x2"));
    }

    #[test]
    fn a_rendering_update_reports_what_changed_since_the_last() {
        let p = PageProfile::new();
        let doc = NodeId::from_bits((1 << 32) | 1).unwrap();
        let mut s = RenderStats {
            restyled: 5,
            laid_out: 7,
            ..Default::default()
        };
        let t = Timings {
            layouts: 1,
            ..Default::default()
        };
        let first = p.rendering_update(&[(doc, "top".into(), s, t)]);
        assert!(first.contains("top: restyled 5"), "{first}");
        assert_eq!(p.rendering_update(&[(doc, "top".into(), s, t)]), "");
        s.restyled = 6;
        s.traversed = 9;
        s.matched = 2;
        let third = p.rendering_update(&[(doc, "top".into(), s, t)]);
        assert!(third.starts_with("[render] #3"), "{third}");
        assert!(third.contains("restyled 1 "), "{third}");
        // (what the restyles visited, of it what ran selector matching and what was shared)
        assert!(
            third.contains("; traversed 9 matched 2 shared 0"),
            "{third}"
        );
    }
}
