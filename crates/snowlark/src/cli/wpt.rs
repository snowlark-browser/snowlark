// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --wpt <list> <results.jsonl> [--font <file>]...`: runs web-platform-tests (testharness.js tests) one after
//! another in this process, appending one JSON line per test file to the results file, in the protocol
//! of the runner (`tools/wpt/run.py` drives it):
//!
//! - each list line is a test URL on a running wptserve, optionally followed by ` long` (the harness's
//!   long timeout);
//! - each `--font <file>` is installed as a local font (the driver passes WPT's `fonts/Ahem.ttf`, which
//!   tests name by `font-family: Ahem` and which WPT expects to be installed, as `--reftest` does);
//! - `[start] <url>` goes to stderr before each test, so the driver can blame a crash on it;
//! - each result is `{"url":…,"ms":…,"result":{"status":…,"message":…,"tests":[{"name","status",
//!   "message"}]}}`, harness status 1 (ERROR) with a `runner: …` message when the runner gave up.
//!
//! The runner serves its own `/resources/testharnessreport.js` (collecting the results) and
//! `/resources/testdriver-vendor.js` (a small testdriver backend). Host resolver rules come from
//! `SNOWLARK_HOST_RESOLVER_RULES` (snowlark-net's `NetConfig::from_env`). A watchdog stops a page whose
//! script never yields, and ends the process (exit 3) if even that does not help; a page over the DOM
//! node or V8 heap limit fails its test and ends the process, so the driver restarts it.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snowlark_page::{ConsoleOutput, Page, PageOptions, RenderSetup, Terminator};

#[path = "wpt/remote.rs"]
mod remote;

const TESTHARNESSREPORT: &str = include_str!("wpt/testharnessreport.js");
const TESTDRIVER_VENDOR: &str = include_str!("wpt/testdriver-vendor.js");

/// DOM nodes a test may create before the runner gives up on it.
const NODE_LIMIT: usize = 20_000_000;

/// The V8 heap a test page may use (the C# runner caps its process at 4 GB).
const HEAP_LIMIT: usize = 2 << 30;

fn json_string(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn error_result(message: &str) -> String {
    format!(
        "{{\"status\":1,\"message\":{},\"tests\":[]}}",
        json_string(message)
    )
}

fn result_line(url: &str, ms: u128, result: &str) -> String {
    format!(
        "{{\"url\":{},\"ms\":{ms},\"result\":{result}}}",
        json_string(url)
    )
}

type Results = Arc<Mutex<std::fs::File>>;

fn append(results: &Results, line: &str) {
    let mut f = results.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(f, "{line}");
    let _ = f.flush();
}

/// What the watchdog watches: the current test, when it started and how long it may take.
struct Watch {
    current: Mutex<Option<(String, Terminator)>>,
    started_ms: AtomicU64,
    limit_ms: AtomicU64,
    terminated: AtomicBool,
}

fn watchdog(watch: Arc<Watch>, results: Results, epoch: Instant) {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let cur = watch
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some((url, terminator)) = cur else {
            continue;
        };
        let now = epoch.elapsed().as_millis() as u64;
        let elapsed = now.saturating_sub(watch.started_ms.load(Ordering::Relaxed));
        let limit = watch.limit_ms.load(Ordering::Relaxed);
        if elapsed > limit + 2_000 && !watch.terminated.swap(true, Ordering::Relaxed) {
            // The page's script never returned to the event loop: stop it; the runner then reports
            // the test.
            terminator.terminate();
        }
        if elapsed > limit + 20_000 {
            append(
                &results,
                &result_line(
                    &url,
                    elapsed as u128,
                    &error_result("runner: the page hung and could not be stopped"),
                ),
            );
            std::process::exit(3);
        }
    }
}

fn harness_resource(u: &url::Url) -> Option<(Vec<u8>, String)> {
    let body = match u.path() {
        "/resources/testharnessreport.js" => TESTHARNESSREPORT,
        "/resources/testdriver-vendor.js" => TESTDRIVER_VENDOR,
        _ => return None,
    };
    Some((body.as_bytes().to_vec(), "text/javascript".into()))
}

/// The files named by `--font <file>` pairs among the arguments after the two positional ones.
fn font_args(rest: &[String]) -> Vec<String> {
    rest.windows(2)
        .filter(|w| w[0] == "--font")
        .map(|w| w[1].clone())
        .collect()
}

pub fn run(args: &[String]) -> i32 {
    let (Some(list), Some(out)) = (args.get(2), args.get(3)) else {
        eprintln!("usage: snowlark --wpt <list.txt> <results.jsonl> [--font <file>]...");
        return 2;
    };
    let fonts = font_args(args.get(4..).unwrap_or_default());
    let Ok(text) = std::fs::read_to_string(list) else {
        eprintln!("cannot read {list}");
        return 2;
    };
    let results: Results = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
    {
        Ok(f) => Arc::new(Mutex::new(f)),
        Err(e) => {
            eprintln!("cannot open {out}: {e}");
            return 2;
        }
    };
    let net = match snowlark_tab::script_check::start_net(snowlark_net::NetConfig::from_env()) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("cannot start the network: {e:?}");
            return 2;
        }
    };
    // Content blocking: off unless SNOWLARK_BLOCK=1 (and then never on the test server's hosts).
    let blocker = snowlark_blocker::cli_filter();
    let client = snowlark_blocker::filtered_client(&net.client(), blocker.as_ref());
    let log = std::env::var("SNOWLARK_WPT_LOG").is_ok_and(|v| v == "1");
    let remote = match remote::Remote::from_env(&client) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    // Every test page renders (style, layout: innerText, getComputedStyle, geometry), with one text
    // engine for the process.
    let mut render = match RenderSetup::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot start the text engine: {e}");
            return 2;
        }
    };
    // Overlay scroll bars (taking no room) unless `SNOWLARK_WPT_SCROLLBAR` gives the width of the
    // window's classic one (17).
    render.scrollbar_width = std::env::var("SNOWLARK_WPT_SCROLLBAR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    for font in &fonts {
        let added = std::fs::read(font)
            .map_err(|e| e.to_string())
            .and_then(|data| render.text.add_local_font(&data).map_err(|e| e.to_string()));
        if let Err(e) = added {
            eprintln!("cannot add the font {font}: {e}");
            return 2;
        }
    }

    let epoch = Instant::now();
    let watch = Arc::new(Watch {
        current: Mutex::new(None),
        started_ms: AtomicU64::new(0),
        limit_ms: AtomicU64::new(0),
        terminated: AtomicBool::new(false),
    });
    {
        let watch = watch.clone();
        let results = results.clone();
        std::thread::spawn(move || watchdog(watch, results, epoch));
    }

    for raw in text.lines() {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let (url_text, long) = match raw.strip_suffix(" long") {
            Some(u) => (u.trim_end(), true),
            None => (raw, false),
        };
        eprintln!("[start] {url_text}");
        let limit = Duration::from_secs(if long { 65 } else { 15 });
        let t0 = Instant::now();
        let result = match url::Url::parse(url_text) {
            Err(e) => error_result(&format!("runner: bad URL: {e}")),
            Ok(url) => {
                let session = match &remote {
                    Some(r) => match r.start(&url) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            eprintln!("cannot connect the page's services: {e}");
                            return 2;
                        }
                    },
                    None => None,
                };
                let mut page = Page::new(PageOptions {
                    net: Some(match &session {
                        Some(s) => s.net(&client),
                        None => client.clone(),
                    }),
                    broadcast: session.as_ref().map(|s| {
                        snowlark_page::BroadcastHub::with_backend(s.services.broadcast.clone())
                    }),
                    web_storage: session
                        .as_ref()
                        .map(|s| s.services.storage.clone() as Arc<_>),
                    idb: session
                        .as_ref()
                        .map(|s| snowlark_page::IdbBackend::new(s.services.idb.clone())),
                    content_filter: snowlark_blocker::content_filter(blocker.as_ref()),
                    console: if log {
                        ConsoleOutput::Stderr
                    } else {
                        ConsoleOutput::Quiet
                    },
                    resource_override: Some(Box::new(harness_resource)),
                    heap_limit: Some(HEAP_LIMIT),
                    render: Some(render.clone()),
                    ..Default::default()
                });
                // (test runs play media without a user, and silently)
                page.engine().set_media_config(snowlark_page::media::MediaConfig {
                    autoplay: snowlark_page::media::AutoplayPolicy::Allowed,
                    silent: true,
                    ..Default::default()
                });
                watch.terminated.store(false, Ordering::Relaxed);
                watch
                    .started_ms
                    .store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                watch
                    .limit_ms
                    .store(limit.as_millis() as u64, Ordering::Relaxed);
                *watch.current.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((url_text.to_owned(), page.terminator()));
                page.navigate(url);
                let mut done: Option<String> = None;
                let mut over = false;
                // (testdriver's clicks, keys and actions enter through the page's input)
                let mut input = snowlark_tab::webdriver_actions::Player::new();
                snowlark_tab::webdriver_actions::run_until(&mut page, t0 + limit, &mut input, |p| {
                    if watch.terminated.load(Ordering::Relaxed) || snowlark_js::guard::failed() {
                        return true;
                    }
                    if p.engine().dom().node_count() > NODE_LIMIT || p.heap_limit_reached() {
                        over = true;
                        return true;
                    }
                    match p.global_string("__snowlarkWpt") {
                        Some(r) if r.starts_with('{') => {
                            done = Some(r);
                            true
                        }
                        _ => false,
                    }
                });
                *watch.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
                if let (Some(s), Some(r)) = (session, &remote) {
                    drop(page);
                    s.finish(r, url_text);
                }
                // A panic in a V8 callback: the failure belongs to the thread, and while it is set
                // no engine callback runs, so every later test here would report no result. The
                // window crashes the tab; here the test is the crash, and the runner goes on with
                // the rest in a new process.
                if let Some(message) = snowlark_js::guard::take() {
                    append(
                        &results,
                        &result_line(
                            url_text,
                            t0.elapsed().as_millis(),
                            &error_result(&format!("CRASH: {message}")),
                        ),
                    );
                    std::process::exit(3);
                }
                if over {
                    append(
                        &results,
                        &result_line(
                            url_text,
                            t0.elapsed().as_millis(),
                            &error_result("runner: over the DOM node or heap limit"),
                        ),
                    );
                    std::process::exit(3);
                }
                match done {
                    Some(r) => r,
                    None if watch.terminated.load(Ordering::Relaxed) => {
                        error_result("runner: the page's script did not yield (terminated)")
                    }
                    None => error_result("runner: no result before the timeout"),
                }
            }
        };
        append(
            &results,
            &result_line(url_text, t0.elapsed().as_millis(), &result),
        );
    }
    0
}
