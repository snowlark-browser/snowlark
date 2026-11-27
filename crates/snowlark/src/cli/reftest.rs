// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --reftest <list> <outdir> [<results.jsonl>] [--font <file>]...`: takes the screenshots of
//! web-platform-tests reftests (tests and references alike) one after another in this process, as
//! wptrunner does, writing a PNG per page into `outdir` and a JSON line per page to the results file
//! (default `<outdir>/results.jsonl`, appended to). `tools/wpt/run.py` drives it and
//! compares the screenshots.
//!
//! - Each list line is `<url> <png name> [timeout=<ms>] [size=<W>x<H>]`: the page, the file its
//!   screenshot goes to (in `outdir`), the time it may take (default 10 s, WPT's reftest timeout)
//!   and the viewport (default 800×600 CSS px, WPT's). Device pixel ratio 1.
//! - `[start] <url>` goes to stderr before each page, so the driver can blame a crash on it.
//! - Each result is `{"url":…,"png":…,"status":"OK"|"TIMEOUT"|"ERROR","wait":…,"ms":…,
//!   "message":…,"error":…}`; `wait` says whether the root element had `reftest-wait`
//!   (`"removed"`, or `"never removed"` when the page timed out waiting for it), `null` when it
//!   never had it; `error` is the page's first uncaught exception, if any.
//!
//! The screenshot is taken as wptrunner's `test-wait.js` takes it: after the `load` event (and the
//! document's subresources: images, fonts, sheets), `document.fonts.ready`, and two animation
//! frames; while the root element's class list has `reftest-wait` the runner fires `TestRendered`
//! at it and waits for the class to go (two more frames after each change of the root's
//! attributes). A page that times out is still captured (to look at), with status TIMEOUT.
//!
//! Text is antialiased in grayscale (no ClearType fringes). `--font <file>` registers a font as if
//! installed on the system (WPT expects Ahem installed: `run.py` passes the WPT checkout's `fonts/Ahem.ttf`).
//! Scroll bars are overlay scroll bars (taking no room) unless `SNOWLARK_WPT_SCROLLBAR` gives the
//! classic one's width, as in `--wpt`. Host resolver rules come from `SNOWLARK_HOST_RESOLVER_RULES`.
//! A watchdog stops a page whose script never yields and ends the process (exit 3) if that does
//! not help; the driver restarts after the page that crashed or hung the process.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snowlark_page::{ConsoleOutput, Page, PageOptions, RenderSetup, Terminator};
use snowlark_paint::{HeadlessRenderer, Sources, Viewport};

const TESTDRIVER_VENDOR: &str = include_str!("wpt/testdriver-vendor.js");

/// WPT's reftest timeout (wptrunner's `RefTest.default_timeout`).
const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// DOM nodes a page may create before the runner gives up on it.
const NODE_LIMIT: usize = 20_000_000;

/// The V8 heap a page may use.
const HEAP_LIMIT: usize = 2 << 30;

/// The global the wait script sets: "waiting" while `reftest-wait` holds the screenshot, then
/// "complete" (no `reftest-wait`) or "complete-wait" (it was removed).
const STATE_GLOBAL: &str = "__snowlarkReftest";

/// wptrunner's `test-wait.js` for screenshots, reporting through [`STATE_GLOBAL`].
const WAIT_SCRIPT: &str = r#"(() => {
  const root = document.documentElement;
  let observer = null;
  function waitLoad() {
    // (fonts.ready of a set that is not loading is resolved already: going straight on keeps the
    // wait free of promise reactions, which this engine runs only at a task's microtask checkpoint)
    if (document.fonts && document.fonts.status === "loading") {
      document.fonts.ready.then(waitPaints, waitPaints);
    } else {
      waitPaints();
    }
  }
  function waitPaints() {
    requestAnimationFrame(() => requestAnimationFrame(screenshotIfReady));
  }
  function screenshotIfReady() {
    if (root && root.classList && root.classList.contains("reftest-wait")) {
      if (observer === null) {
        window.__snowlarkReftest = "waiting";
        observer = new MutationObserver(waitPaints);
        observer.observe(root, {attributes: true});
        root.dispatchEvent(new Event("TestRendered", {bubbles: true}));
      }
      return;
    }
    if (observer !== null) observer.disconnect();
    window.__snowlarkReftest = observer !== null ? "complete-wait" : "complete";
  }
  if (document.readyState !== "complete") {
    addEventListener("load", waitLoad, {once: true});
  } else {
    waitLoad();
  }
})();"#;

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

/// One page of the list.
struct Entry {
    url: String,
    png: String,
    timeout: Duration,
    size: (u32, u32),
}

fn parse_line(line: &str) -> Result<Entry, String> {
    let mut parts = line.split_whitespace();
    let (Some(url), Some(png)) = (parts.next(), parts.next()) else {
        return Err(format!("bad list line: {line}"));
    };
    let mut entry = Entry {
        url: url.to_owned(),
        png: png.to_owned(),
        timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        size: (800, 600),
    };
    for part in parts {
        if let Some(ms) = part.strip_prefix("timeout=") {
            let ms: u64 = ms.parse().map_err(|_| format!("bad timeout: {part}"))?;
            entry.timeout = Duration::from_millis(ms);
        } else if let Some(size) = part.strip_prefix("size=") {
            let (w, h) = size
                .split_once(['x', 'X'])
                .ok_or_else(|| format!("bad size: {part}"))?;
            let w: u32 = w.parse().map_err(|_| format!("bad size: {part}"))?;
            let h: u32 = h.parse().map_err(|_| format!("bad size: {part}"))?;
            if w == 0 || h == 0 || w > 4096 || h > 4096 {
                return Err(format!("bad size: {part}"));
            }
            entry.size = (w, h);
        } else {
            return Err(format!("unknown list option: {part}"));
        }
    }
    Ok(entry)
}

struct Outcome {
    status: &'static str,
    wait: Option<&'static str>,
    message: String,
}

fn result_line(entry: &Entry, ms: u128, outcome: &Outcome, error: Option<&str>) -> String {
    format!(
        "{{\"url\":{},\"png\":{},\"status\":\"{}\",\"wait\":{},\"ms\":{ms},\"message\":{},\"error\":{}}}",
        json_string(&entry.url),
        json_string(&entry.png),
        outcome.status,
        outcome
            .wait
            .map_or_else(|| "null".to_owned(), json_string),
        json_string(&outcome.message),
        error.map_or_else(|| "null".to_owned(), json_string)
    )
}

/// The page's first uncaught exception or unhandled rejection, as the console showed it.
fn first_error(page: &Page) -> Option<String> {
    page.console_messages()
        .into_iter()
        .find(|(level, text)| *level == "error" && text.trim_start().starts_with("Uncaught"))
        .map(|(_, text)| text.trim().chars().take(300).collect())
}

type Results = Arc<Mutex<std::fs::File>>;

fn append(results: &Results, line: &str) {
    let mut f = results.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(f, "{line}");
    let _ = f.flush();
}

/// What the watchdog watches: the current page (its result line if the process must end), when it
/// started and how long it may take.
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
        let Some((line, terminator)) = cur else {
            continue;
        };
        let now = epoch.elapsed().as_millis() as u64;
        let elapsed = now.saturating_sub(watch.started_ms.load(Ordering::Relaxed));
        let limit = watch.limit_ms.load(Ordering::Relaxed);
        if elapsed > limit + 2_000 && !watch.terminated.swap(true, Ordering::Relaxed) {
            // The page's script never returned to the event loop: stop it; the runner then reports
            // the page.
            terminator.terminate();
        }
        if elapsed > limit + 20_000 {
            append(&results, &line);
            std::process::exit(3);
        }
    }
}

fn harness_resource(u: &url::Url) -> Option<(Vec<u8>, String)> {
    (u.path() == "/resources/testdriver-vendor.js").then(|| {
        (
            TESTDRIVER_VENDOR.as_bytes().to_vec(),
            "text/javascript".into(),
        )
    })
}

pub fn run(args: &[String]) -> i32 {
    const USAGE: &str =
        "usage: snowlark --reftest <list.txt> <outdir> [<results.jsonl>] [--font <file>]...";
    let mut positional = Vec::new();
    let mut fonts = Vec::new();
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--font" {
            let Some(f) = args.get(i + 1) else {
                eprintln!("{USAGE}");
                return 2;
            };
            fonts.push(PathBuf::from(f));
            i += 2;
        } else {
            positional.push(args[i].clone());
            i += 1;
        }
    }
    let (list, outdir, results_path) = match positional.as_slice() {
        [l, o] => (l, PathBuf::from(o), PathBuf::from(o).join("results.jsonl")),
        [l, o, r] => (l, PathBuf::from(o), PathBuf::from(r)),
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let Ok(text) = std::fs::read_to_string(list) else {
        eprintln!("cannot read {list}");
        return 2;
    };
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match parse_line(line) {
            Ok(e) => entries.push(e),
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        }
    }
    if let Err(e) = std::fs::create_dir_all(&outdir) {
        eprintln!("cannot create {}: {e}", outdir.display());
        return 2;
    }
    let results: Results = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&results_path)
    {
        Ok(f) => Arc::new(Mutex::new(f)),
        Err(e) => {
            eprintln!("cannot open {}: {e}", results_path.display());
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
    let blocker = snowlark_blocker::cli_filter();
    let client = snowlark_blocker::filtered_client(&net.client(), blocker.as_ref());
    let log = std::env::var("SNOWLARK_WPT_LOG").is_ok_and(|v| v == "1");
    let mut render = match RenderSetup::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot start the text engine: {e}");
            return 2;
        }
    };
    render.scrollbar_width = std::env::var("SNOWLARK_WPT_SCROLLBAR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    for font in &fonts {
        let added = std::fs::read(font)
            .map_err(|e| e.to_string())
            .and_then(|data| render.text.add_local_font(&data).map_err(|e| e.to_string()));
        if let Err(e) = added {
            eprintln!("cannot add the font {}: {e}", font.display());
            return 2;
        }
    }
    let mut raster = match HeadlessRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot start the rasterizer: {e}");
            return 2;
        }
    };
    // Grayscale text, as browsers' test modes draw it: ClearType's color fringes would make text
    // differ from the boxes references draw in its place (Ahem's squares).
    raster.set_grayscale_text(true);

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

    for entry in &entries {
        eprintln!("[start] {}", entry.url);
        let t0 = Instant::now();
        let (outcome, error) = match url::Url::parse(&entry.url) {
            Err(e) => (
                Outcome {
                    status: "ERROR",
                    wait: None,
                    message: format!("runner: bad URL: {e}"),
                },
                None,
            ),
            Ok(url) => {
                let mut page = Page::new(PageOptions {
                    net: Some(client.clone()),
                    content_filter: snowlark_blocker::content_filter(blocker.as_ref()),
                    console: if log {
                        ConsoleOutput::Stderr
                    } else {
                        ConsoleOutput::Quiet
                    },
                    viewport: (entry.size.0 as i32, entry.size.1 as i32),
                    resource_override: Some(Box::new(harness_resource)),
                    heap_limit: Some(HEAP_LIMIT),
                    render: Some(render.clone()),
                    webgl: snowlark_shell::gpu_process::headless_webgl(),
                    broadcast: None,
                    web_storage: None,
                    idb: None,
                    ..Default::default()
                });
                page.engine()
                    .set_media_config(snowlark_page::media::MediaConfig {
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
                    .store(entry.timeout.as_millis() as u64, Ordering::Relaxed);
                let hung = Outcome {
                    status: "ERROR",
                    wait: None,
                    message: "runner: the page hung and could not be stopped".into(),
                };
                *watch.current.lock().unwrap_or_else(|e| e.into_inner()) = Some((
                    result_line(entry, entry.timeout.as_millis(), &hung, None),
                    page.terminator(),
                ));
                let outcome = take_screenshot(&mut page, url, entry, &outdir, &mut raster, &watch);
                *watch.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
                let error = first_error(&page);
                (outcome, error)
            }
        };
        let fatal = outcome.message.starts_with("runner: over")
            || outcome.message.starts_with("CRASH: ");
        append(
            &results,
            &result_line(entry, t0.elapsed().as_millis(), &outcome, error.as_deref()),
        );
        if fatal {
            std::process::exit(3);
        }
    }
    0
}

/// Loads the page, waits as wptrunner does and writes the screenshot.
fn take_screenshot(
    page: &mut Page,
    url: url::Url,
    entry: &Entry,
    outdir: &Path,
    raster: &mut HeadlessRenderer,
    watch: &Watch,
) -> Outcome {
    let t0 = Instant::now();
    page.navigate(url);
    let mut injected = false;
    let mut over = false;
    let mut state: Option<String> = None;
    // (testdriver's clicks, keys and actions enter through the page's input)
    let mut input = snowlark_tab::webdriver_actions::Player::new();
    let done = snowlark_tab::webdriver_actions::run_until(page, t0 + entry.timeout, &mut input, |p| {
        if watch.terminated.load(Ordering::Relaxed) || snowlark_js::guard::failed() {
            return true;
        }
        if p.engine().dom().node_count() > NODE_LIMIT || p.heap_limit_reached() {
            over = true;
            return true;
        }
        if !injected {
            if !p.load_complete() {
                return false;
            }
            // Images, fonts and sheets still arriving (this engine's load event does not wait for
            // the CSS images, which browsers' does): the screenshot waits for them too.
            if p.engine()
                .with_renderer(|r, _| r.pending() > r.script_font_fetches())
                .unwrap_or(false)
            {
                return false;
            }
            injected = true;
            if let Err(e) = p.eval(WAIT_SCRIPT, "snowlark-reftest-wait") {
                state = Some(format!("error: {e}"));
                return true;
            }
        }
        state = p.global_string(STATE_GLOBAL);
        state.as_deref().is_some_and(|s| s.starts_with("complete"))
    });
    // A panic in a V8 callback: the failure belongs to the thread, and while it is set no engine
    // callback runs, so every later page here would come out blank. The page is the crash, and the
    // runner goes on with the rest in a new process.
    if let Some(message) = snowlark_js::guard::take() {
        return Outcome {
            status: "ERROR",
            wait: None,
            message: format!("CRASH: {message}"),
        };
    }
    if over {
        return Outcome {
            status: "ERROR",
            wait: None,
            message: "runner: over the DOM node or heap limit".into(),
        };
    }
    let terminated = watch.terminated.load(Ordering::Relaxed);
    let (status, wait, message) = match state.as_deref() {
        Some("complete") if done && !terminated => ("OK", None, String::new()),
        Some("complete-wait") if done && !terminated => ("OK", Some("removed"), String::new()),
        Some(s) if s.starts_with("error: ") => ("ERROR", None, format!("runner: {s}")),
        _ if terminated => (
            "ERROR",
            None,
            "runner: the page's script did not yield (terminated)".to_owned(),
        ),
        Some("waiting") => (
            "TIMEOUT",
            Some("never removed"),
            "reftest-wait was not removed before the timeout".to_owned(),
        ),
        _ if !injected => (
            "TIMEOUT",
            None,
            "the load event did not fire before the timeout".to_owned(),
        ),
        _ => (
            "TIMEOUT",
            None,
            "no screenshot before the timeout".to_owned(),
        ),
    };
    // (the loop also ends early when the page has nothing left to do: nothing can change then)
    let message = if !done && !terminated && t0.elapsed() < entry.timeout {
        format!("{message} (the page went idle)")
    } else {
        message
    };
    if terminated {
        return Outcome {
            status,
            wait,
            message,
        };
    }
    let mut message = message;
    if let Err(e) = capture(page, entry, &outdir.join(&entry.png), raster) {
        if status == "OK" {
            return Outcome {
                status: "ERROR",
                wait,
                message: format!("runner: screenshot: {e}"),
            };
        }
        message.push_str(&format!(" (screenshot: {e})"));
    }
    Outcome {
        status,
        wait,
        message,
    }
}

/// The viewport as the user would see it now: a rendering update, then the top-level document's
/// display list (with its frames) at its scroll position, rasterized at 1 device pixel per CSS px.
fn capture(
    page: &mut Page,
    entry: &Entry,
    png: &Path,
    raster: &mut HeadlessRenderer,
) -> Result<(), String> {
    page.update_the_rendering();
    let engine = page.engine();
    let (scroll, text, images) = engine
        .with_renderer(|r, st| (st.scroll, r.text().clone(), r.images().clone()))
        .ok_or("no document")?;
    let (w, h) = entry.size;
    // (a render-blocked document shows nothing yet: an empty canvas)
    let list = engine.paint(scroll, 1.0).unwrap_or_default();
    let view = Viewport {
        width: w,
        height: h,
        scale: 1.0,
        background: engine.canvas_color(),
    };
    let pixmap = raster
        .render(
            &list,
            Sources {
                text: &text,
                images: &images,
            },
            &view,
        )
        .map_err(|e| e.to_string())?;
    pixmap
        .write_png(png)
        .map_err(|e| format!("{}: {e}", png.display()))
}
