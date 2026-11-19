// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --speedometer [--iterations N] [--suites A,B] [--url URL] [--out DIR] [--snapshot]`: runs
//! Speedometer's own runner headless (rendering, no window), and writes into `DIR` (default
//! `speedometer-report`):
//!
//! - `results.json`: the run's results, as the page's "Copy JSON" gives them;
//! - `report.txt`: the score of each iteration, each suite's totals by iteration with how much they
//!   grew from the first to the last, and the subtests that grew most; then, at each suite boundary,
//!   the engine's counts (V8 heap, nodes, documents, platform objects, removed frames not freed yet);
//!   then, after the run and forced collections, the removed frames still alive (each is a leak) with
//!   the engine's `SNOWLARK_GC_STATS` lines on what keeps them;
//! - `leaks.heapsnapshot` when frames stayed (or always, with `--snapshot`), for
//!   `python tools/heap-retainers/retainers.py <file> --holders`.
//!
//! The runner is observed through what the page already exposes (`benchmarkClient`'s progress and
//! results, its `#info-label`), read between event loop turns: nothing is added to the page. Headless
//! times are not the window's (no raster, no compositor), but growth across iterations and leaks are
//! the same. `--url` runs another copy of Speedometer 3 (a local server's); `--suites` takes the
//! runner's own `suites` parameter (names as in its results, comma-separated).
//! `SNOWLARK_SPEEDOMETER_TIMEOUT_S` bounds the run (default 3600); a run in which no step finishes for
//! two minutes stops, and its report says what the page was waiting for (its frames' documents and
//! the fetches in flight).
//!
//! `--cycles` also counts the CPU cycles charged to this (the page's) thread and to the whole process
//! in each event loop turn, by the suite and iteration the runner was in at the turn's end, and writes
//! them to `cycles.json` and the report. The checks between turns are not charged; they make the
//! run's wall times a little longer. `--pin-pcores` first restricts the process to the performance
//! cores of a hybrid processor; `--cpu N` runs the page's thread on logical processor N alone, at a
//! raised priority, and the process's other threads off N's core (`tools/speedometer/quick_ab.py`
//! gives two builds the two hyperthreads of one core). See `docs/perf-counters.md`.
//!
//! `SNOWLARK_SPEEDOMETER_MARKS=1` prints, after every event loop turn, the runner's performance marks
//! made in it (`[turn]` lines, stderr): with `SNOWLARK_PROFILE`'s `[render]` lines,
//! `tools/vs-chrome/restyled_steps.py` counts the restyled elements of each step's scored window.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use snowlark_ipc::win::cpu::{self, Cycles};
use snowlark_page::{ConsoleOutput, Page, PageOptions};

const USAGE: &str = "usage: snowlark --speedometer [--iterations N] [--suites A,B] [--url URL] [--out DIR] [--snapshot] [--cycles] [--pin-pcores] [--cpu N]";

/// The runner's iteration (the iterations it has finished) and suite (its label), `|` between.
const WHERE_JS: &str = "(() => { const c = globalThis.benchmarkClient; if (!c) return ''; \
     const l = document.getElementById('info-label'); \
     return ((c._measuredValuesList && c._measuredValuesList.length) || 0) + '|' + (l ? l.textContent : ''); })()";

/// The cycles of each event loop turn, by the iteration and suite the runner was in at its end.
#[derive(Default)]
struct SuiteCycles {
    /// (iteration, suite, cycles), in the order first seen.
    entries: Vec<(u32, String, Cycles)>,
    /// When the last turn charged ended (the cycles from there on are the next turn's).
    mark: Cycles,
}

impl SuiteCycles {
    /// Charges the cycles since the mark, the turn just run, to where the runner is now.
    fn charge(&mut self, page: &mut Page) {
        let turn = Cycles::now().since(self.mark);
        if let Some((iteration, suite)) = page
            .eval(WHERE_JS, "<speedometer-harness>")
            .ok()
            .as_deref()
            .and_then(parse_where)
        {
            self.add(iteration, suite, turn);
        }
        self.mark = Cycles::now();
    }

    /// Leaves the cycles since the mark uncharged (the harness's own work).
    fn skip(&mut self) {
        self.mark = Cycles::now();
    }

    fn add(&mut self, iteration: u32, suite: &str, cycles: Cycles) {
        match self
            .entries
            .iter_mut()
            .find(|(i, s, _)| *i == iteration && s == suite)
        {
            Some((_, _, c)) => c.add(cycles),
            None => self.entries.push((iteration, suite.to_owned(), cycles)),
        }
    }

    fn suites(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for (_, s, _) in &self.entries {
            if !out.contains(&s.as_str()) {
                out.push(s);
            }
        }
        out
    }

    fn iterations(&self) -> u32 {
        self.entries.iter().map(|(i, _, _)| i + 1).max().unwrap_or(0)
    }

    fn get(&self, iteration: u32, suite: &str) -> Cycles {
        self.entries
            .iter()
            .find(|(i, s, _)| *i == iteration && s == suite)
            .map_or(Cycles::default(), |e| e.2)
    }

    /// `{"pinned": mask or null, "iterations": [{suite: {"thread": n, "process": n}}]}`.
    fn json(&self, pinned: Option<u64>) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "{{\"pinned\":{},\"iterations\":[",
            pinned.map_or("null".to_owned(), |m| m.to_string())
        );
        for i in 0..self.iterations() {
            out.push_str(if i == 0 { "{" } else { ",{" });
            let mut first = true;
            for (_, s, c) in self.entries.iter().filter(|e| e.0 == i) {
                let _ = write!(
                    out,
                    "{}\"{}\":{{\"thread\":{},\"process\":{}}}",
                    if first { "" } else { "," },
                    json_escape(s),
                    c.thread,
                    c.process
                );
                first = false;
            }
            out.push('}');
        }
        out.push_str("]}\n");
        out
    }

    /// The report's table: each suite's page thread cycles by iteration (G), then the process's.
    fn table(&self) -> String {
        let mut out = String::new();
        let n = self.iterations();
        for (title, pick) in [
            ("page thread", (|c: Cycles| c.thread) as fn(Cycles) -> u64),
            ("process", |c: Cycles| c.process),
        ] {
            let _ = writeln!(out, "cycles by iteration, {title} (G):");
            let mut totals = vec![0u64; n as usize];
            for suite in self.suites() {
                let _ = write!(out, "{suite:<45}");
                for i in 0..n {
                    let c = pick(self.get(i, suite));
                    totals[i as usize] += c;
                    let _ = write!(out, "{:8.3}", c as f64 / 1e9);
                }
                out.push('\n');
            }
            let _ = write!(out, "{:<45}", "total");
            for t in totals {
                let _ = write!(out, "{:8.3}", t as f64 / 1e9);
            }
            out.push_str("\n\n");
        }
        out
    }
}

/// `SNOWLARK_SPEEDOMETER_MARKS=1`: after every event loop turn, a `[turn]` line on stderr with the
/// runner's performance marks made in that turn, so the `[render]` lines of `SNOWLARK_PROFILE` (printed
/// during the turn) can be split into scored windows (`<Suite>.<Step>-start` to `-async-end`) and the rest.
struct MarkLog {
    seen: usize,
}

impl MarkLog {
    fn turn(&mut self, page: &mut Page) {
        let js = format!(
            "(() => {{ const m = performance.getEntriesByType('mark'); const o = [m.length]; \
             for (let i = {}; i < m.length; i++) o.push(m[i].name); return o.join(' '); }})()",
            self.seen
        );
        let Ok(text) = page.eval(&js, "<speedometer-harness>") else {
            return;
        };
        let (count, names) = text.split_once(' ').unwrap_or((&text, ""));
        self.seen = count.parse().unwrap_or(self.seen);
        eprintln!("[turn] {names}");
    }
}

/// `WHERE_JS`'s answer as (iteration, suite); `None` before the runner has a suite.
fn parse_where(text: &str) -> Option<(u32, &str)> {
    let (iteration, suite) = text.split_once('|')?;
    if suite.is_empty() {
        return None;
    }
    Some((iteration.parse().ok()?, suite))
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// What the page says about the run's progress.
struct Progress {
    finished: u64,
    done: bool,
    label: String,
}

fn progress(page: &mut Page) -> Option<Progress> {
    let text = page
        .eval(
            "(() => { const c = globalThis.benchmarkClient; if (!c) return ''; \
             const l = document.getElementById('info-label'); \
             return [c._finishedTestCount ?? 0, c._hasResults ? 1 : 0, l ? l.textContent : ''].join('\\u0001'); })()",
            "<speedometer-harness>",
        )
        .ok()?;
    let mut parts = text.split('\u{1}');
    let finished = parts.next()?.parse().ok()?;
    let done = parts.next()? == "1";
    let label = parts.next().unwrap_or("").to_owned();
    Some(Progress {
        finished,
        done,
        label,
    })
}

/// One line of engine counts.
fn counts(page: &mut Page) -> String {
    let heap = page.heap_used() >> 20;
    let (nodes, objects) = page.engine().live_counts();
    let documents = page.engine().dom().document_count();
    let slots = page.engine().dom().node_slots();
    let scatter = page.engine().dom().tree_scatter();
    let frames = page.frames_alive();
    let alive: usize = frames.values().sum();
    let mut by_url = String::new();
    for (url, n) in &frames {
        // (the last path segments name the suite)
        let short: String = url
            .rsplit('/')
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("/");
        let _ = write!(by_url, " {short}={n}");
    }
    format!(
        "heap {heap} MiB, {nodes} nodes ({slots} slots, scatter {scatter:.0}), {documents} documents, {objects} objects, removed frames alive {alive}:{by_url}"
    )
}

/// The results' summary, computed by the page from `benchmarkClient.metrics`.
const SUMMARY_JS: &str = r#"(() => {
  const m = benchmarkClient.metrics;
  const out = [];
  const fmt = (v, d) => v.toFixed(d).padStart(8);
  const values = (k) => (m[k] && m[k].values) || [];
  out.push("score by iteration:" + values("Score").map((v) => fmt(v, 2)).join(""));
  out.push("");
  out.push("suite totals by iteration (ms), last/first:");
  const suites = Object.keys(m).filter((k) => !k.includes("/") && !/^(Iteration-|Geomean|Score)/.test(k));
  for (const s of suites) {
    const v = values(s);
    const ratio = v.length ? v[v.length - 1] / v[0] : 1;
    out.push(s.padEnd(45) + v.map((x) => fmt(x, 1)).join("") + "   x" + ratio.toFixed(2) + (ratio >= 1.3 ? "  GROWS" : ""));
  }
  out.push("");
  out.push("subtests that grew most (ms, first -> last):");
  const sub = Object.keys(m).filter((k) => k.split("/").length === 3).map((k) => {
    const v = values(k);
    return { k, first: v[0], last: v[v.length - 1], max: Math.max(...v) };
  }).filter((x) => x.last - x.first >= 5 && x.last / x.first >= 1.3);
  sub.sort((a, b) => (b.last - b.first) - (a.last - a.first));
  for (const x of sub.slice(0, 25))
    out.push("  " + x.k.padEnd(60) + fmt(x.first, 1) + " ->" + fmt(x.last, 1) + "  (max" + fmt(x.max, 1) + ")");
  if (!sub.length) out.push("  none");
  return out.join("\n");
})()"#;

/// A run in which no step finished for this long has stalled.
const STALLED: Duration = Duration::from_secs(120);

/// For a run that did not finish:each frame's document as the runner page sees it.
const STALL_JS: &str = r#"(() => {
  const out = [];
  for (const f of document.querySelectorAll("iframe")) {
    let d = null;
    try { d = f.contentDocument; } catch (e) {}
    out.push("frame " + f.src + ": " + (d ? d.URL + " " + d.readyState + ", " + d.getElementsByTagName("*").length + " elements" : "no document"));
  }
  return out.length ? out.join("\n") : "no frames";
})()"#;

pub fn run(args: &[String]) -> i32 {
    let mut iterations = 5u32;
    let mut suites: Option<String> = None;
    let mut base = "https://browserbench.org/Speedometer3.1/".to_owned();
    let mut out_dir = PathBuf::from("speedometer-report");
    let mut always_snapshot = false;
    let mut count_cycles = false;
    let mut pin = false;
    let mut cpu: Option<u8> = None;
    let mut i = 0;
    while i < args.len() {
        let value = |i: usize| args.get(i + 1).cloned();
        match args[i].as_str() {
            "--speedometer" => {}
            "--iterations" => match value(i).and_then(|v| v.parse().ok()) {
                Some(n) => {
                    iterations = n;
                    i += 1;
                }
                None => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            "--suites" => {
                suites = value(i);
                i += 1;
            }
            "--url" => {
                if let Some(u) = value(i) {
                    base = u;
                }
                i += 1;
            }
            "--out" => {
                if let Some(d) = value(i) {
                    out_dir = d.into();
                }
                i += 1;
            }
            "--snapshot" => always_snapshot = true,
            "--cycles" => count_cycles = true,
            "--pin-pcores" => pin = true,
            "--cpu" => match value(i).and_then(|v| v.parse().ok()) {
                Some(n) => {
                    cpu = Some(n);
                    i += 1;
                }
                None => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            other => {
                eprintln!("unknown argument {other}\n{USAGE}");
                return 2;
            }
        }
        i += 1;
    }
    let mut url = match url::Url::parse(&base) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("bad --url {base}: {e}");
            return 2;
        }
    };
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("iterationCount", &iterations.to_string());
        if let Some(s) = &suites {
            q.append_pair("suites", s);
        }
        q.append_pair("startAutomatically", "");
    }
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {e}", out_dir.display());
        return 1;
    }
    let timeout = Duration::from_secs(
        std::env::var("SNOWLARK_SPEEDOMETER_TIMEOUT_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600),
    );

    let pinned = if pin {
        let mask = cpu::pin_to_performance_cores();
        match mask {
            Some(m) => eprintln!("[speedometer] on the performance cores {m:#x}"),
            None => eprintln!("[speedometer] --pin-pcores: no performance cores to restrict to"),
        }
        mask
    } else {
        None
    };
    // (this thread is the page's)
    if let Some(n) = cpu {
        if let Err(e) = cpu::dedicate_core(n) {
            eprintln!("[speedometer] --cpu {n}: {e}");
            return 2;
        }
    }
    let net = match snowlark_tab::script_check::start_net(snowlark_net::NetConfig::from_env()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("network thread: {e}");
            return 1;
        }
    };
    let render = match snowlark_page::RenderSetup::new() {
        Ok(mut r) => {
            r.net = Some(net.client());
            r
        }
        Err(e) => {
            eprintln!("render: {e}");
            return 1;
        }
    };
    let mut page = Page::new(PageOptions {
        console: if std::env::var("SNOWLARK_CONSOLE").is_ok_and(|v| v == "1") {
            ConsoleOutput::Stderr
        } else {
            ConsoleOutput::Quiet
        },
        net: Some(net.client()),
        render: Some(render),
        viewport: (1280, 900),
        ..Default::default()
    });
    // (canvas frames as the window takes them)
    page.engine().set_canvas_frames_async(true);
    eprintln!("[speedometer] {url}");
    page.navigate(url.clone());

    let start = Instant::now();
    let mut report = String::new();
    let _ = writeln!(report, "Speedometer headless run: {url}\n");
    let mut boundaries = String::new();
    let mut label = String::new();
    let mut finished = 0;
    let mut completed = false;
    let mut started = false;
    // (when the last step finished or the runner moved to another suite)
    let mut moved = Instant::now();
    let mut cycles = count_cycles.then(|| SuiteCycles {
        mark: Cycles::now(),
        ..Default::default()
    });
    let mut marks =
        std::env::var_os("SNOWLARK_SPEEDOMETER_MARKS").map(|_| MarkLog { seen: 0 });
    while start.elapsed() < timeout {
        page.run_until(Instant::now() + Duration::from_millis(100), |p| {
            if let Some(c) = cycles.as_mut() {
                c.charge(p);
            }
            if let Some(m) = marks.as_mut() {
                m.turn(p);
            }
            false
        });
        // Poll only once a turn runs nothing. A step's async time runs from the end of its sync
        // callback to a zero-delay timer that is due at once, so the loop is never idle inside it;
        // between steps it waits for the next frame. (Polled at any turn, the counts below landed
        // inside scored windows: 1.3% of a profile's scored samples, up to 3.9% of a suite's.)
        let settle = Instant::now() + Duration::from_secs(1);
        while Instant::now() < settle {
            let ran = page.turn();
            if let Some(c) = cycles.as_mut() {
                c.charge(&mut page);
            }
            if let Some(m) = marks.as_mut() {
                m.turn(&mut page);
            }
            if !ran {
                break;
            }
        }
        let polled = progress(&mut page);
        let Some(p) = polled else {
            if let Some(c) = cycles.as_mut() {
                c.skip();
            }
            continue;
        };
        if p.label != label {
            if !label.is_empty() {
                let line = format!(
                    "{:7.1}s  after {label:<45} ({} steps)  {}",
                    start.elapsed().as_secs_f64(),
                    p.finished,
                    counts(&mut page)
                );
                eprintln!("[speedometer] {line}");
                boundaries.push_str(&line);
                boundaries.push('\n');
            }
            label = p.label.clone();
            moved = Instant::now();
        }
        if p.finished != finished {
            moved = Instant::now();
        }
        finished = p.finished;
        started |= p.finished > 0 || !p.label.is_empty();
        if let Some(c) = cycles.as_mut() {
            c.skip();
        }
        if p.done {
            completed = true;
            break;
        }
        // (the runner starts at once; one that has not after two minutes is not going to: a
        // `--suites` name it does not know leaves it with nothing to run)
        if !started && start.elapsed() > Duration::from_secs(120) {
            let _ = writeln!(
                report,
                "the runner did not start within 120 s: check --suites against the suite names in its results\n"
            );
            break;
        }
        // (a step takes seconds at most, a suite's load over the network a few: a run with no
        // step done for this long is waiting for something that will not come)
        if started && moved.elapsed() > STALLED {
            let _ = writeln!(
                report,
                "no step finished for {} s: stopped",
                STALLED.as_secs()
            );
            break;
        }
    }
    if !completed {
        let _ = writeln!(
            report,
            "the run did not finish: stopped after {:.0} s (limit {} s), {finished} steps done, at {label:?}",
            start.elapsed().as_secs_f64(),
            timeout.as_secs()
        );
        // What the page was waiting for: its frames' documents, and the fetches in flight.
        let frames = page
            .eval(STALL_JS, "<speedometer-harness>")
            .unwrap_or_else(|e| format!("frames: {e}"));
        let _ = writeln!(report, "{frames}");
        for line in page.engine().loading_report() {
            let _ = writeln!(report, "{line}");
        }
        // (and what the network's connections are doing: a request waiting for a connection
        // slot that a stuck fetch holds shows here)
        for line in net.client().pool_report() {
            let _ = writeln!(report, "{line}");
        }
        report.push('\n');
        eprint!("[speedometer] stalled:\n{report}");
    } else {
        match page.eval(
            "benchmarkClient._formattedJSONResult({ modern: true })",
            "<speedometer-harness>",
        ) {
            Ok(json) => {
                let path = out_dir.join("results.json");
                if let Err(e) = std::fs::write(&path, json) {
                    eprintln!("cannot write {}: {e}", path.display());
                }
            }
            Err(e) => eprintln!("results: {e}"),
        }
        match page.eval(SUMMARY_JS, "<speedometer-harness>") {
            Ok(s) => {
                report.push_str(&s);
                report.push_str("\n\n");
            }
            Err(e) => {
                let _ = writeln!(report, "summary: {e}\n");
            }
        }
    }
    if let Some(c) = &cycles {
        report.push_str(&c.table());
        let path = out_dir.join("cycles.json");
        if let Err(e) = std::fs::write(&path, c.json(pinned)) {
            eprintln!("cannot write {}: {e}", path.display());
        }
    }
    let _ = writeln!(report, "engine counts at each suite boundary:");
    report.push_str(&boundaries);

    // Every frame is removed now: what is still alive after full collections stayed.
    for _ in 0..8 {
        page.collect_garbage();
        page.run_until(Instant::now() + Duration::from_millis(250), |_| false);
    }
    let _ = writeln!(report, "\nafter the run and forced collections: {}", counts(&mut page));
    for line in page.engine().live_census() {
        let _ = writeln!(report, "{line}");
    }
    let frames = page.frames_alive();
    if frames.is_empty() {
        let _ = writeln!(report, "no removed frame stayed");
    } else {
        let _ = writeln!(report, "removed frames that stayed (leaks):");
        for (url, n) in &frames {
            let _ = writeln!(report, "  {n} x {url}");
        }
        let _ = writeln!(report, "what keeps them (engine side; heap snapshot for V8's):");
        for line in page.gc_report_text().unwrap_or_default() {
            let _ = writeln!(report, "  {line}");
        }
    }
    if always_snapshot || !frames.is_empty() {
        let path = out_dir.join("leaks.heapsnapshot");
        match page.write_heap_snapshot(&path) {
            Ok(()) => {
                let _ = writeln!(
                    report,
                    "heap snapshot: {} (python tools/heap-retainers/retainers.py <it> --holders)",
                    path.display()
                );
            }
            Err(e) => {
                let _ = writeln!(report, "heap snapshot: {e}");
            }
        }
    }
    let path = out_dir.join("report.txt");
    if let Err(e) = std::fs::write(&path, &report) {
        eprintln!("cannot write {}: {e}", path.display());
    }
    print!("{report}");
    if completed {
        0
    } else {
        1
    }
}
