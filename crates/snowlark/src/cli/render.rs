// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --render <url-or-file> <png> [width] [--height H] [--scroll Y] [--dpr R] [--animation-time MS]
//! [--trace] [--no-scripts]`: fetches a page
//! (http(s) through snowlark-net, or a local file), loads its stylesheets, images and web fonts, lays it
//! out at `width` CSS px (default 1280) in an 800 px tall viewport, and writes a PNG. The image is as
//! tall as the document (at least the viewport, at most 4000 px, like the C# engine), or `H` px with
//! `--height`; `--scroll Y` scrolls the viewport first; `--dpr R` renders at R device pixels per CSS
//! px (default 1: the PNG is R times the CSS size).
//!
//! By default the page runs as in the browser: a `snowlark_page::Page` loads it with its scripts, and
//! the capture is taken once the document's `load` event has fired and nothing is left to do (no task,
//! timer, frame or fetch), or `SNOWLARK_SETTLE_MS` (default 2 s) after the load event, at most
//! `SNOWLARK_RENDER_TIMEOUT_MS` (default 30 s) after the start, as the C# engine settled pages.
//!
//! With `--no-scripts` no scripts run (scripting is off, so `<noscript>` content shows) and the
//! renderer loads the document alone. Subresources are waited for until none is in flight, at most
//! `SNOWLARK_RENDER_TIMEOUT_MS`; `SNOWLARK_SETTLE_MS` keeps pumping at least that long. While waiting, the page renders as
//! a browser's would: not before its render-blocking stylesheets arrive (or the render-blocking timeout
//! passes), then with the document timeline following the clock, so CSS transitions and animations
//! run. The capture is a screenshot: the state at the timeline time of the capture, which is the
//! time since the document was parsed, or `--animation-time MS` (without scripts the timeline holds
//! there once it gets there during the wait, and moves forward to it at the capture if the wait was
//! shorter; with scripts it stays at 0 during the wait, so animations started by the load, CSS or
//! script, are captured MS after they started, whatever the load took).
//! `--trace` (or `SNOWLARK_TRACE=1`) logs every request. `SNOWLARK_PROFILE_DIR=<dir>` keeps cookies (and HSTS) in `<dir>` across
//! runs, as the window's profile does (a returning visitor; without it every run starts with none), and
//! `SNOWLARK_UA_PRESET=<id>` reports the user agent preset Settings would choose (`chrome`, `firefox`, ...).
//! `SNOWLARK_COLOR_SCHEME=dark` renders as for a user who prefers a dark color scheme (`prefers-color-scheme`),
//! as the window does under a dark theme. `SNOWLARK_EVAL=<js>` prints an expression's
//! value in the settled page; `SNOWLARK_EVAL_LOADED=<js>` runs once 3 s after the load event. `SNOWLARK_CONSOLE=1` prints the page's console to stderr, and
//! `SNOWLARK_CONSOLE=tagged` one `[console.<level>] <text> (<source>)` line per message (uncaught
//! exceptions with where they were thrown), for tools that parse it. Prints `[timing]` lines per
//! phase to stderr, and a `[stats]` line: elements restyled by what their change required, box tree
//! builds and incremental updates, boxes laid out and reused, text runs shaped and taken from the
//! cache.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use snowlark_net::{FetchEvent, NetConfig, Request, Url};
use snowlark_paint::{Point, Size, TextEngine};
use snowlark_render::{Renderer, TimelineClock};
use snowlark_style::DeviceConfig;

const USAGE: &str = "usage: snowlark --render <url-or-file> <png> [width] [--height H] [--scroll Y] [--dpr R] [--animation-time MS] [--trace] [--no-scripts]";

/// A URL from the command line: absolute, a local path, or a host that gets https://.
fn parse_target(text: &str) -> Option<Url> {
    if let Ok(url) = Url::parse(text) {
        // (a Windows path like C:\x parses as a URL with scheme "c")
        if url.scheme().len() > 1 {
            return Some(url);
        }
    }
    let path = std::path::Path::new(text);
    if path.exists() {
        return std::fs::canonicalize(path)
            .ok()
            .and_then(|p| Url::from_file_path(p).ok());
    }
    Url::parse(&format!("https://{text}")).ok()
}

/// `SNOWLARK_PROFILE_DIR=<dir>`: cookies (and HSTS) persist in `<dir>`, as in the window's profile, so a
/// run is a returning visitor; without it they are in memory and every run starts with none.
fn profile_dir() -> Option<PathBuf> {
    std::env::var_os("SNOWLARK_PROFILE_DIR").map(PathBuf::from)
}

/// `SNOWLARK_UA_PRESET=<id>`: the user agent preset Settings would choose (`chrome`, `firefox`, ...).
fn preset_user_agent() -> Option<snowlark_net::ua::UserAgent> {
    snowlark_net::ua::UserAgent::preset(&std::env::var("SNOWLARK_UA_PRESET").ok()?)
}

/// `SNOWLARK_COLOR_SCHEME=dark`: the user prefers a dark color scheme.
fn prefers_dark() -> bool {
    std::env::var("SNOWLARK_COLOR_SCHEME").is_ok_and(|v| v.eq_ignore_ascii_case("dark"))
}

fn env_ms(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

pub fn run(args: &[String]) -> i32 {
    let mut positional = Vec::new();
    let mut height = None;
    let mut scroll = 0.0f32;
    let mut dpr = 1.0f32;
    let mut animation_time = None;
    let mut trace = std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1");
    let mut scripts = true;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--height" => {
                height = args.get(i + 1).and_then(|v| v.parse::<f32>().ok());
                if height.is_none() {
                    eprintln!("{USAGE}");
                    return 2;
                }
                i += 2;
            }
            "--scroll" => {
                match args.get(i + 1).and_then(|v| v.parse::<f32>().ok()) {
                    Some(v) => scroll = v,
                    None => {
                        eprintln!("{USAGE}");
                        return 2;
                    }
                }
                i += 2;
            }
            "--dpr" => {
                match args.get(i + 1).and_then(|v| v.parse::<f32>().ok()) {
                    Some(v) if v > 0.0 && v <= 8.0 => dpr = v,
                    _ => {
                        eprintln!("{USAGE}");
                        return 2;
                    }
                }
                i += 2;
            }
            "--animation-time" => {
                match args.get(i + 1).and_then(|v| v.parse::<f64>().ok()) {
                    Some(v) if v.is_finite() && v >= 0.0 => {
                        animation_time = Some(Duration::from_secs_f64(v / 1000.0))
                    }
                    _ => {
                        eprintln!("{USAGE}");
                        return 2;
                    }
                }
                i += 2;
            }
            "--trace" => {
                trace = true;
                i += 1;
            }
            "--no-scripts" => {
                scripts = false;
                i += 1;
            }
            a => {
                positional.push(a.to_owned());
                i += 1;
            }
        }
    }
    let (target, png, width) = match positional.as_slice() {
        [t, p] => (t, p, 1280.0),
        [t, p, w] => match w.parse::<f32>() {
            Ok(w) if w >= 1.0 => (t, p, w),
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        },
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let Some(url) = parse_target(target) else {
        eprintln!("not a URL: {target}");
        return 2;
    };
    let options = Options {
        width,
        height,
        scroll,
        dpr,
        animation_time,
        trace,
    };
    let result = if scripts {
        render_scripted(&url, &PathBuf::from(png), &options)
    } else {
        render(&url, &PathBuf::from(png), &options)
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("render: {e}");
            1
        }
    }
}

struct Options {
    width: f32,
    height: Option<f32>,
    scroll: f32,
    dpr: f32,
    /// The document timeline's time at the capture.
    animation_time: Option<Duration>,
    trace: bool,
}

fn render(url: &Url, png: &std::path::Path, options: &Options) -> Result<(), String> {
    let Options {
        width,
        height,
        scroll,
        dpr,
        animation_time,
        trace,
    } = *options;
    let started = Instant::now();
    let timing = |phase: &str, ms: f64| eprintln!("[timing] {phase}: {ms:.1} ms");
    let mut config = NetConfig::from_env();
    config.trace |= trace;
    config.profile_dir = profile_dir();
    let thread = snowlark_tab::script_check::start_net(config).map_err(|e| format!("network thread: {e}"))?;
    if let Some(ua) = preset_user_agent() {
        thread.client().set_user_agent(ua);
    }
    // Content blocking: off unless SNOWLARK_BLOCK=1.
    let blocker = snowlark_blocker::cli_filter();
    let net = snowlark_blocker::filtered_client(&thread.client(), blocker.as_ref());
    let text = Arc::new(TextEngine::new().map_err(|e| e.to_string())?);

    // The document.
    let t = Instant::now();
    let mut stream = net.fetch(Request::navigation(url.clone()));
    let mut head = None;
    let mut body = Vec::new();
    while let Some(event) = stream.next_blocking() {
        match event {
            FetchEvent::Head(h) => head = Some(h),
            FetchEvent::Body(b) => body.extend_from_slice(&b),
            FetchEvent::Done => break,
            FetchEvent::Error(e) => return Err(format!("{url}: {e}")),
            FetchEvent::Violation(_) => {}
        }
    }
    let head = head.ok_or("no response")?;
    if !head.ok() {
        eprintln!("[render] {url}: HTTP {}", head.status);
    }
    let final_url = head.url().cloned().unwrap_or_else(|| url.clone());
    let charset = snowlark_net::mime::extract_mime_type(&head.headers)
        .and_then(|m| m.parameter("charset").map(str::to_owned));
    timing("fetch", ms(t));

    let viewport = Size::new(width, 800.0);
    let device = DeviceConfig {
        viewport: (viewport.width, viewport.height),
        scripting: false,
        device_pixel_ratio: dpr,
        color_scheme: if prefers_dark() {
            snowlark_style::ColorScheme::Dark
        } else {
            snowlark_style::ColorScheme::Light
        },
        ..DeviceConfig::default()
    };
    let mut renderer = Renderer::new(net, text, device);
    renderer.set_trace(trace);
    if let Some(t) = animation_time {
        renderer.set_timeline_clock(TimelineClock::MonotonicUntil(t));
    }

    let t = Instant::now();
    renderer.load_html(&body, &final_url, charset.as_deref());
    timing("parse", renderer.timings().parse_ms);
    let after_parse = renderer.timings();

    // Subresources (each arrival can reveal more: imports, images, fonts), rendering as they arrive.
    let t_wait = Instant::now();
    let timeout = Duration::from_millis(env_ms("SNOWLARK_RENDER_TIMEOUT_MS").unwrap_or(30_000));
    let mut settled = renderer.render_until_settled(timeout);
    if let Some(settle) = env_ms("SNOWLARK_SETTLE_MS") {
        let until = t_wait + Duration::from_millis(settle);
        while Instant::now() < until {
            renderer.wait(until.saturating_duration_since(Instant::now()));
            settled = renderer.render_until_settled(Duration::ZERO);
        }
    }
    let wait_ms = ms(t_wait);
    let counts = renderer.resource_counts();
    let tm = renderer.timings();
    eprintln!(
        "[timing] subresources: {wait_ms:.1} ms waiting, {} requests ({} sheets, {} imports, {} images, {} fonts, {} failed){}; {:.1} ms applying them, {:.1} ms in {} restyles",
        settled.requests,
        counts.sheets,
        counts.imports,
        counts.images,
        counts.fonts,
        counts.failed,
        if settled.complete {
            String::new()
        } else {
            format!(", {} still pending at the timeout", settled.pending)
        },
        tm.resources_ms,
        tm.style_ms - after_parse.style_ms,
        tm.restyles,
    );
    let _ = t;

    let before = renderer.timings();
    // The capture: a rendering update at the capture's timeline time.
    if let Some(t) = animation_time {
        renderer.set_timeline_clock(TimelineClock::Manual(t));
    }
    if renderer.update_rendering(viewport).is_none() {
        // Still render-blocked (a document without a body, within the render-blocking timeout): a
        // browser would show nothing yet; draw what there is.
        eprintln!("[render] still render-blocked at the capture");
        renderer.layout(viewport).ok_or("no document")?;
    }
    eprintln!(
        "[render] timeline at {:.0} ms{}",
        renderer.timeline_time().as_secs_f64() * 1000.0,
        if renderer.animating() {
            ", transitions or animations running"
        } else {
            ""
        }
    );
    let tm = renderer.timings();
    let stats = renderer.layout_stats();
    eprintln!(
        "[timing] layout: {:.1} ms (box tree {:.1} ms, layout {:.1} ms; {} boxes laid out, {} shaped)",
        (tm.build_ms - before.build_ms) + (tm.layout_ms - before.layout_ms),
        tm.build_ms - before.build_ms,
        tm.layout_ms - before.layout_ms,
        stats.laid_out,
        stats.shaped,
    );
    let st = renderer.stats();
    eprintln!(
        "[stats] restyle: {} elements restyled ({} repaint, {} relayout, {} reconstruct, {} unchanged); box tree: {} full builds, {} incremental updates, {} boxes built, {} restyled in place; layout: {} boxes laid out, {} reused; text: {} runs shaped, {} from the shaping cache",
        st.restyled,
        st.repaint,
        st.relayout,
        st.reconstruct,
        st.unchanged,
        st.full_builds,
        st.incremental_updates,
        st.update.boxes_built,
        st.update.boxes_restyled,
        st.laid_out,
        st.reused,
        st.shaped,
        st.shape_cache_hits,
    );
    let doc_height = renderer.document_height();
    let out_height = height.unwrap_or_else(|| doc_height.clamp(viewport.height, 4000.0)).max(1.0);
    let size = Size::new(width, out_height);
    let pixmap = renderer.rasterize(size, Point::new(0.0, scroll), dpr)?;
    let tm2 = renderer.timings();
    let svg = renderer.svg_stats();
    timing("paint (display list)", tm2.paint_ms - tm.paint_ms);
    if svg.rasterizations + svg.hits > 0 {
        eprintln!(
            "[timing] svg images: {} drawn, {} rasterized, {} from the raster cache",
            svg.rasterizations + svg.hits,
            svg.rasterizations,
            svg.hits
        );
    }
    let st = renderer.stats();
    if st.svgs_recorded + st.svgs_not_recorded > 0 {
        eprintln!(
            "[render] inline svgs: {} recorded as paint operations, {} drawn as images {:?}",
            st.svgs_recorded,
            st.svgs_not_recorded,
            renderer.inline_svg_fallbacks()
        );
    }
    let t = Instant::now();
    pixmap
        .write_png(png)
        .map_err(|e| format!("{}: {e}", png.display()))?;
    timing(
        "raster + png",
        (tm2.raster_ms - tm.raster_ms) + ms(t),
    );
    timing("total", ms(started));
    eprintln!(
        "[render] {} -> {} ({}x{} px, document {:.0} px tall)",
        final_url,
        png.display(),
        pixmap.width,
        pixmap.height,
        doc_height
    );
    Ok(())
}

/// The page with its scripts: loaded through `snowlark_page::Page`, settled, captured.
fn render_scripted(url: &Url, png: &std::path::Path, options: &Options) -> Result<(), String> {
    let Options {
        width,
        height,
        scroll,
        dpr,
        animation_time,
        trace,
    } = *options;
    let started = Instant::now();
    let mut config = NetConfig::from_env();
    config.trace |= trace;
    config.profile_dir = profile_dir();
    let thread = snowlark_tab::script_check::start_net(config).map_err(|e| format!("network thread: {e}"))?;
    if let Some(ua) = preset_user_agent() {
        thread.client().set_user_agent(ua);
    }
    // Content blocking: off unless SNOWLARK_BLOCK=1.
    let blocker = snowlark_blocker::cli_filter();
    let net = snowlark_blocker::filtered_client(&thread.client(), blocker.as_ref());
    let mut setup = snowlark_page::RenderSetup::new()?;
    setup.device_pixel_ratio = dpr;
    setup.trace = trace;
    setup.dark = prefers_dark();
    let viewport = Size::new(width, 800.0);
    // `SNOWLARK_CONSOLE=1`: console lines as they are printed; `SNOWLARK_CONSOLE=tagged`: one line per
    // message with its level and source position (see `print_console_entries`).
    let console_mode = std::env::var("SNOWLARK_CONSOLE").unwrap_or_default();
    let tagged = console_mode == "tagged";
    let console = if console_mode == "1" {
        snowlark_page::ConsoleOutput::Stderr
    } else {
        snowlark_page::ConsoleOutput::Quiet
    };
    let mut page = snowlark_page::Page::new(snowlark_page::PageOptions {
        net: Some(net),
        console,
        viewport: (width as i32, viewport.height as i32),
        render: Some(setup),
        content_filter: snowlark_blocker::content_filter(blocker.as_ref()),
        ..Default::default()
    });
    // Posters and video frames: decoded on the GPU, copied to memory for the capture; silent.
    page.engine().set_media_config(snowlark_page::media::MediaConfig {
        videos: snowlark_paint::Gpu::new()
            .ok()
            .map(|g| snowlark_paint::video::VideoStore::new(Some(g))),
        copy_frames: true,
        silent: true,
        ..Default::default()
    });
    // The document timeline stays at 0 while the page loads and settles, so what starts then
    // (script animations start at a rendering update, CSS animations at a style change) starts at 0
    // whatever the load took, and the capture shows it `--animation-time` later.
    if animation_time.is_some() {
        page.engine()
            .set_timeline_clock(TimelineClock::Manual(Duration::ZERO));
    }
    if tagged {
        page.engine().devtools_console(true);
    }
    page.navigate(url.clone());
    let timeout = Duration::from_millis(env_ms("SNOWLARK_RENDER_TIMEOUT_MS").unwrap_or(30_000));
    let settle = Duration::from_millis(env_ms("SNOWLARK_SETTLE_MS").unwrap_or(2_000));
    let mut loaded_at: Option<Instant> = None;
    // `SNOWLARK_EVAL_LOADED=<js>`: run once, 3 s after the load event, while the page goes on running
    // (a debugging aid: press a button or start a player, and watch what follows until the capture).
    let mut eval_loaded = std::env::var("SNOWLARK_EVAL_LOADED").ok();
    // A watchdog: a task that never yields (a script that loops, or one so slow that one task
    // outlasts the timeout) is terminated 2 s after the timeout, so the capture still happens, as
    // a browser's screenshot would show what the page painted.
    // Script stops at once when terminated; a task still running a second after that is in native
    // code (a binding, the network, layout), which termination cannot stop. Its stack is sampled
    // from then on, so the report can say where the page thread was.
    let stuck_sampler = snowlark_js::native_profile::NativeSampler::start_after(
        (started + timeout + Duration::from_secs(3)).saturating_duration_since(Instant::now()),
        Duration::from_millis(100),
        Default::default(),
    );
    let watch_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watch_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog = {
        let terminator = page.terminator();
        let done = watch_done.clone();
        let fired = watch_fired.clone();
        let deadline = started + timeout + Duration::from_secs(2);
        std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            while !done.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now >= deadline {
                    fired.store(true, Ordering::Relaxed);
                    terminator.terminate();
                    return;
                }
                std::thread::park_timeout((deadline - now).min(Duration::from_millis(100)));
            }
        })
    };
    let settled = loop {
    let settled = page.run_until(started + timeout, |p| {
        if tagged {
            print_console_entries(p.engine());
        }
        if loaded_at.is_none() && p.load_complete() {
            loaded_at = Some(Instant::now());
        }
        if loaded_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(3)) {
            if let Some(js) = eval_loaded.take() {
                if let Err(e) = p.eval(&js, "snowlark-eval-loaded") {
                    eprintln!("[eval] threw {e}");
                }
            }
        }
        match loaded_at {
            // An idle page is settled, unless `SNOWLARK_EVAL_LOADED` has yet to run in it.
            Some(t) => {
                let e = p.engine();
                (eval_loaded.is_none() && !e.has_pending_work() && !e.has_ready_task())
                    || t.elapsed() >= settle
            }
            None => false,
        }
    });
    // An idle page ends `run_until` before `SNOWLARK_EVAL_LOADED` is due: wait for it and go on.
    match loaded_at {
        Some(t) if eval_loaded.is_some() && Instant::now() < started + timeout => {
            let due = t + Duration::from_secs(3);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        _ => break settled,
    }
    };
    watch_done.store(true, std::sync::atomic::Ordering::Relaxed);
    watchdog.thread().unpark();
    let _ = watchdog.join();
    let stuck = stuck_sampler.map(|s| s.stop().0).unwrap_or_default();
    if watch_fired.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("[render] a task ran past the timeout: its script was terminated");
        if !stuck.is_empty() {
            eprint!("{}", stuck_report(&stuck));
        }
        page.terminator().resume();
    }
    eprintln!(
        "[render] {} after {:.0} ms{}",
        if loaded_at.is_some() {
            "loaded"
        } else {
            "not loaded"
        },
        ms(started),
        if settled { "" } else { " (timeout)" }
    );
    if tagged {
        print_console_entries(page.engine());
    }
    eprintln!("{}", page.engine().load_summary());
    if let Some(b) = &blocker {
        eprintln!("[blocker] {} requests blocked", b.blocked_count());
    }
    // `SNOWLARK_EVAL=<js>`: an expression evaluated in the settled page, printed (a debugging aid).
    if let Ok(js) = std::env::var("SNOWLARK_EVAL") {
        match page.eval(&js, "snowlark-eval") {
            Ok(v) => println!("{v}"),
            Err(e) => eprintln!("[eval] threw {e}"),
        }
    }
    // The capture: a rendering update at the capture's timeline time.
    if let Some(t) = animation_time {
        page.engine().set_timeline_clock(TimelineClock::Manual(t));
    }
    page.update_the_rendering();
    if tagged {
        print_console_entries(page.engine());
    }
    let pixmap = page
        .engine()
        .with_renderer(|r, _| -> Result<_, String> {
            if r.fragments().is_none() {
                // Still render-blocked: draw what there is.
                eprintln!("[render] still render-blocked at the capture");
                r.layout(viewport).ok_or("no document")?;
            }
            let doc_height = r.document_height();
            let out_height = height
                .unwrap_or_else(|| doc_height.clamp(viewport.height, 4000.0))
                .max(1.0);
            let pixmap = r.rasterize(Size::new(width, out_height), Point::new(0.0, scroll), dpr)?;
            let st = r.stats();
            if st.svgs_recorded + st.svgs_not_recorded > 0 {
                eprintln!(
                    "[render] inline svgs: {} recorded as paint operations, {} drawn as images {:?}",
                    st.svgs_recorded,
                    st.svgs_not_recorded,
                    r.inline_svg_fallbacks()
                );
            }
            Ok((pixmap, doc_height))
        })
        .ok_or("no document")??;
    let (pixmap, doc_height) = pixmap;
    pixmap
        .write_png(png)
        .map_err(|e| format!("{}: {e}", png.display()))?;
    let final_url = page
        .engine()
        .document()
        .map(|d| page.engine().dom().document_url(d).to_owned())
        .unwrap_or_default();
    eprintln!(
        "[render] {} -> {} ({}x{} px, document {:.0} px tall) in {:.0} ms",
        final_url,
        png.display(),
        pixmap.width,
        pixmap.height,
        doc_height,
        ms(started)
    );
    Ok(())
}

/// `SNOWLARK_CONSOLE=tagged`: the console messages since the last call, one stderr line each
/// ([`snowlark_page::devtools::ConsoleEntry::tagged_line`]): the level as the console API was called
/// (uncaught exceptions and unhandled rejections are errors), the text, and the position the
/// developer tools would show (the console call's caller, or where the uncaught error was thrown).
/// Tools such as `tools/census` parse these lines.
fn print_console_entries(engine: &snowlark_page::Engine) {
    for entry in engine.take_console_entries() {
        eprintln!("{}", entry.tagged_line());
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Where a task that outlived its termination was: the page thread's most sampled stack, innermost
/// frame first (`[render] stuck:` lines, a sample each 100 ms).
fn stuck_report(samples: &[Vec<u64>]) -> String {
    use std::fmt::Write;
    let summary = snowlark_js::native_profile::summarize(samples, &Default::default());
    let mut o = String::new();
    let _ = writeln!(
        o,
        "[render] stuck: the task went on {} ms in native code after its script was terminated",
        samples.len() * 100
    );
    if let Some((stack, n)) = summary.folded.iter().max_by_key(|(s, n)| (**n, s.len())) {
        let _ = writeln!(o, "[render] stuck: {n} of {} samples in", samples.len());
        for frame in stack.rsplit(';').take(24) {
            let _ = writeln!(o, "[render] stuck:   {frame}");
        }
    }
    o
}
