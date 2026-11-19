// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rendering opportunities from begin frames (the window's compositor sends one at each vsync while
//! the page asks): a page that uses them updates its rendering only at a begin frame, with the
//! frame's time, keeps one begin frame in flight, asks for them only while it wants frames (and one
//! frame more), and takes the display's interval for its idle deadlines. Without them (headless)
//! the 60 Hz timer is unchanged.

use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{BeginFrame, Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

fn page(begin_frames: bool) -> Page {
    page_of("<!doctype html><div id=box>a</div>", begin_frames)
}

fn page_of(html: &str, begin_frames: bool) -> Page {
    static NET: std::sync::OnceLock<NetThread> = std::sync::OnceLock::new();
    let net = NET.get_or_init(|| NetThread::start(NetConfig::default()).unwrap());
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    // (a secure context: `getCoalescedEvents()` is [SecureContext])
    page.load_html(html, &url::Url::parse("https://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    page.update_the_rendering();
    if begin_frames {
        page.engine().use_begin_frames();
    }
    page
}

/// A begin frame for the vsync `ago` before now, at `interval`.
fn vsync(ago: Duration, interval: Duration) -> BeginFrame {
    let frame_time = Instant::now() - ago;
    BeginFrame {
        frame_time,
        deadline: frame_time + interval,
        interval,
        missed: false,
    }
}

/// Runs event loop turns for `ms`.
fn spin(page: &mut Page, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        if !page.turn() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

const REQUEST: &str =
    "window.calls = []; requestAnimationFrame(t => calls.push([t, performance.now()]));";

#[test]
fn animation_frames_wait_for_a_begin_frame_and_get_its_time() {
    let mut page = page(true);
    eval(&mut page, REQUEST);
    assert!(page.engine().needs_begin_frames(), "asks for begin frames");
    // (no frame from the 60 Hz timer, and no wake-up for one)
    spin(&mut page, 60);
    assert_eq!(
        eval(&mut page, "calls.length"),
        "0",
        "no frame without a begin frame"
    );
    assert!(
        page.engine()
            .next_wakeup()
            .is_none_or(|t| t > Instant::now() + Duration::from_secs(1)),
        "the loop waits for the begin frame, not a timer"
    );
    let interval = Duration::from_micros(5556);
    page.engine()
        .begin_frame(vsync(Duration::from_millis(4), interval));
    spin(&mut page, 5);
    assert_eq!(
        eval(&mut page, "calls.length"),
        "1",
        "the begin frame ran it"
    );
    // The callback's time is the vsync's, about 4 ms before it ran.
    let lag: f64 = eval(&mut page, "calls[0][1] - calls[0][0]")
        .parse()
        .unwrap();
    assert!(
        (3.5..40.0).contains(&lag),
        "the frame's time, not now: lag {lag}"
    );
    assert!(
        (page.engine().frame_interval() - 5.556).abs() < 0.01,
        "the display's interval"
    );
}

#[test]
fn a_late_missed_begin_frame_and_one_already_used_are_dropped() {
    let mut page = page(true);
    eval(&mut page, REQUEST);
    let interval = Duration::from_micros(5556);
    // A missed begin frame whose deadline has passed.
    let mut late = vsync(Duration::from_millis(20), interval);
    late.missed = true;
    page.engine().begin_frame(late);
    spin(&mut page, 5);
    assert_eq!(eval(&mut page, "calls.length"), "0", "too late");
    // A missed one within its interval is used.
    let mut missed = vsync(Duration::from_millis(1), interval);
    missed.missed = true;
    page.engine().begin_frame(missed);
    spin(&mut page, 5);
    assert_eq!(
        eval(&mut page, "calls.length"),
        "1",
        "a missed frame in time"
    );
    // The same frame again (not as a missed one) does not start another update.
    eval(&mut page, REQUEST);
    let mut again = missed;
    again.missed = false;
    page.engine().begin_frame(again);
    spin(&mut page, 5);
    assert_eq!(eval(&mut page, "calls.length"), "0", "used already");
}

#[test]
fn one_begin_frame_is_in_flight_and_the_update_uses_the_first() {
    let mut page = page(true);
    eval(&mut page, REQUEST);
    let interval = Duration::from_micros(5556);
    // (both frames after the document's time origin)
    std::thread::sleep(Duration::from_millis(40));
    let first = vsync(Duration::from_millis(30), interval);
    let later = vsync(Duration::from_millis(1), interval);
    // (both came while the loop was busy: a `BeginMainFrame` carries its impl frame's args)
    page.engine().begin_frame(first);
    page.engine().begin_frame(later);
    spin(&mut page, 5);
    assert_eq!(eval(&mut page, "calls.length"), "1");
    let lag: f64 = eval(&mut page, "calls[0][1] - calls[0][0]")
        .parse()
        .unwrap();
    assert!(lag >= 29.0, "the first frame's time: lag {lag}");
}

#[test]
fn the_page_asks_for_one_begin_frame_more_after_an_update() {
    let mut page = page(true);
    assert!(!page.engine().needs_begin_frames(), "nothing to render");
    eval(&mut page, REQUEST);
    assert!(page.engine().needs_begin_frames());
    let interval = Duration::from_micros(5556);
    page.engine()
        .begin_frame(vsync(Duration::from_millis(1), interval));
    spin(&mut page, 5);
    assert_eq!(eval(&mut page, "calls.length"), "1");
    // (cc's `ProactiveBeginFrameWanted`: one frame more after a commit)
    assert!(page.engine().needs_begin_frames(), "one frame more");
    std::thread::sleep(Duration::from_millis(6));
    page.engine().begin_frame(vsync(Duration::ZERO, interval));
    spin(&mut page, 5);
    assert!(
        !page.engine().needs_begin_frames(),
        "a frame with nothing to do ends it"
    );
    // A begin frame the page did not want serves a need that arises before its deadline.
    let idle = vsync(Duration::ZERO, Duration::from_millis(50));
    page.engine().begin_frame(idle);
    eval(&mut page, REQUEST);
    spin(&mut page, 5);
    assert_eq!(eval(&mut page, "calls.length"), "1", "within the frame");
}

#[test]
fn idle_deadlines_end_at_the_next_vsync() {
    let mut page = page(true);
    let interval = Duration::from_micros(5556);
    eval(&mut page, REQUEST);
    page.engine().begin_frame(vsync(Duration::ZERO, interval));
    spin(&mut page, 2);
    assert_eq!(eval(&mut page, "calls.length"), "1");
    // With a frame pending, an idle period ends by the next frame: the display's interval after
    // the last, not 60 Hz's.
    eval(
        &mut page,
        "window.left = -1; requestAnimationFrame(() => {}); \
         requestIdleCallback(d => { left = d.timeRemaining(); });",
    );
    spin(&mut page, 30);
    let left: f64 = eval(&mut page, "left").parse().unwrap();
    assert!(
        (0.0..=5.6).contains(&left),
        "at most one 180 Hz interval: {left}"
    );
}

#[test]
fn without_begin_frames_the_timer_runs_frames_and_begin_frames_are_ignored() {
    let mut page = page(false);
    eval(&mut page, REQUEST);
    assert!(!page.engine().needs_begin_frames());
    page.engine()
        .begin_frame(vsync(Duration::from_millis(10), Duration::from_millis(5)));
    spin(&mut page, 40);
    assert_eq!(
        eval(&mut page, "calls.length"),
        "1",
        "the 60 Hz timer's frame"
    );
    let lag: f64 = eval(&mut page, "calls[0][1] - calls[0][0]")
        .parse()
        .unwrap();
    assert!(lag < 2.0, "now's time: lag {lag}");
    assert!((page.engine().frame_interval() - 1000.0 / 60.0).abs() < 1e-9);
}

/// While the page gets begin frames, pointer moves wait for its rendering update: the moves before
/// a frame are one `pointermove`, at the last position, dispatched first in the frame (before its
/// animation frame callbacks), and its outcome is kept for the embedder. A press dispatches a held
/// move before itself. Without begin frames each move is dispatched at once.
#[test]
fn pointer_moves_are_dispatched_once_a_frame() {
    let none = Modifiers::default();
    let mut page = page_of(BOX, true);
    eval(
        &mut page,
        "window.log = []; \
         addEventListener('pointermove', e => log.push('move ' + e.clientX)); \
         addEventListener('pointerdown', e => log.push('down ' + e.clientX)); \
         requestAnimationFrame(() => log.push('raf'));",
    );
    for x in [10.0, 20.0, 30.0] {
        assert!(
            page.queue_pointer_move(Point::new(x, 10.0), none).is_none(),
            "held"
        );
    }
    spin(&mut page, 20);
    assert_eq!(eval(&mut page, "log.join()"), "", "nothing before a frame");
    assert!(
        page.engine().needs_begin_frames(),
        "a held move asks for a frame"
    );
    let interval = Duration::from_micros(5556);
    page.engine()
        .begin_frame(vsync(Duration::from_millis(1), interval));
    spin(&mut page, 5);
    assert_eq!(eval(&mut page, "log.join()"), "move 30,raf");
    assert!(page.take_held_outcome().is_some(), "the outcome waits");
    assert!(page.take_held_outcome().is_none());
    // A press after a held move: the move, then the press.
    assert!(page
        .queue_pointer_move(Point::new(40.0, 10.0), none)
        .is_none());
    page.pointer_pressed(Point::new(45.0, 10.0), 0, none, 1);
    assert_eq!(eval(&mut page, "log.join()"), "move 30,raf,move 40,down 45");
    assert!(
        page.take_held_outcome().is_none(),
        "the press's outcome supersedes the move's"
    );
    // Headless: at once.
    let mut page = page_of(BOX, false);
    eval(
        &mut page,
        "window.log = []; addEventListener('pointermove', e => log.push('move ' + e.clientX));",
    );
    assert!(page
        .queue_pointer_move(Point::new(12.0, 10.0), none)
        .is_some());
    assert_eq!(eval(&mut page, "log.join()"), "move 12");
}

/// The moves a frame-aligned `pointermove` stands for are its coalesced events (Pointer Events 3),
/// in time order, each trusted, not bubbling or cancelable, with the move's target and pointer and
/// no coalesced events of their own; a move dispatched alone has itself as its one coalesced event.
#[test]
fn a_frame_aligned_move_has_the_moves_it_stands_for_as_its_coalesced_events() {
    let none = Modifiers::default();
    let check = "window.log = []; addEventListener('pointermove', e => { \
         const l = e.getCoalescedEvents(); \
         const ok = l.every((c, i) => c.isTrusted && !c.bubbles && !c.cancelable && \
           c.target === e.target && c.pointerId === e.pointerId && c.pointerType === e.pointerType && \
           c.isPrimary === e.isPrimary && c.getCoalescedEvents().length === 0 && \
           c.timeStamp <= e.timeStamp && (i === 0 || c.timeStamp >= l[i - 1].timeStamp)); \
         log.push(e.clientX + ':' + l.map(c => c.clientX).join('/') + (ok ? '' : ' bad')); });";
    let mut page = page_of(BOX, true);
    eval(&mut page, check);
    for x in [10.0, 20.0, 30.0] {
        assert!(page.queue_pointer_move(Point::new(x, 10.0), none).is_none());
    }
    let interval = Duration::from_micros(5556);
    page.engine()
        .begin_frame(vsync(Duration::from_millis(1), interval));
    spin(&mut page, 5);
    assert_eq!(eval(&mut page, "log.join()"), "30:10/20/30");
    // Headless: each move alone.
    let mut page = page_of(BOX, false);
    eval(&mut page, check);
    page.queue_pointer_move(Point::new(12.0, 10.0), none);
    assert_eq!(eval(&mut page, "log.join()"), "12:12");
}

/// A box under the points the pointer tests use.
const BOX: &str = "<!doctype html><div id=box style='height: 100px'>a</div>";
