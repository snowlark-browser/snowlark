// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Audio API 1.1 §1.2 AudioContext: the constructor's options, "allowed to start" without and
//! with a user activation, `resume()`, `suspend()` and `close()` with their promises and
//! `statechange` events, the clock, and `getOutputTimestamp()`.

use std::time::{Duration, Instant};

use snowlark_page::media::{AutoplayPolicy, MediaConfig};
use snowlark_page::{Page, PageOptions};

fn page(autoplay: AutoplayPolicy) -> Page {
    let mut page = Page::new(PageOptions::default());
    page.engine().set_media_config(MediaConfig {
        autoplay,
        silent: true,
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Runs the page until `js` is true, for at most `secs` seconds.
fn wait_for(page: &mut Page, js: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        page.turn();
        if eval(page, js) == "true" {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// Runs the page for `ms` milliseconds.
fn run_for(page: &mut Page, ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        page.turn();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A user activation (a key press).
fn activate(page: &mut Page) {
    page.key_down(&snowlark_page::keyboard::KeyInput {
        vk: 0x41,
        scan: 0x1E,
        text: Some('a'),
        mods: snowlark_page::input::Modifiers::default(),
        repeat: false,
        paste: None,
        paste_html: None,
    });
    page.run_until_settled(Duration::from_secs(5));
}

/// Logs a context's `statechange` events and its promises' outcomes into `log`.
const LOG: &str = r#"
    window.log = [];
    window.track = (ctx) => { ctx.onstatechange = () => log.push('statechange:' + ctx.state); return ctx; };
    window.note = (name, p) => p.then(() => log.push(name + ':ok:' + ctx.state), e => log.push(name + ':' + e.name));
"#;

#[test]
fn the_constructor_checks_its_options() {
    let mut page = page(AutoplayPolicy::Allowed);
    let r = eval(
        &mut page,
        r#"
        const name = (f) => { try { f(); return 'ok'; } catch (e) { return e.name; } };
        const lat = (hint) => new AudioContext({ latencyHint: hint }).baseLatency;
        const d = new AudioContext();
        const r = new AudioContext({ sampleRate: 22050 });
        [
          d instanceof BaseAudioContext, d.sampleRate, r.sampleRate, r.state,
          d.destination.channelCount, d.destination.maxChannelCount,
          name(() => new AudioContext({ sampleRate: 1 })),
          name(() => new AudioContext({ sampleRate: 1e7 })),
          name(() => new AudioContext({ latencyHint: 'fastest' })),
          name(() => new AudioContext({ latencyHint: NaN })),
          lat('interactive') === d.baseLatency,
          lat('interactive') <= lat('balanced') && lat('balanced') <= lat('playback'),
          lat(-1) === lat('interactive'), lat(100) === lat('playback'),
          d.baseLatency > 0, d.outputLatency >= 0,
          'sinkId' in d, 'setSinkId' in d,
        ].join(' ')
        "#,
    );
    assert_eq!(
        r,
        "true 48000 22050 suspended 2 2 NotSupportedError NotSupportedError TypeError \
         TypeError true true true true true true false false"
    );
}

#[test]
fn an_allowed_context_runs_suspends_and_closes_in_order() {
    let mut page = page(AutoplayPolicy::Allowed);
    eval(
        &mut page,
        &format!("{LOG} window.ctx = track(new AudioContext()); log.push('initial:' + ctx.state);"),
    );
    assert!(wait_for(&mut page, "log.length == 2", 5), "starts");
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "initial:suspended statechange:running"
    );
    // The clock runs while running. (The rendering starts in parallel with the constructor, so
    // the zeros of a context that has rendered nothing yet are checked on one that cannot start.)
    assert!(
        wait_for(&mut page, "ctx.currentTime > 0.1", 5),
        "the clock runs"
    );
    let r = eval(
        &mut page,
        "const ts = ctx.getOutputTimestamp();
         [ts.contextTime < ctx.currentTime, ts.performanceTime > 0,
          ts.performanceTime <= performance.now() + 1].join(' ')",
    );
    assert_eq!(r, "true true true");
    // suspend(): the promise settles before the event; the clock stops.
    eval(&mut page, "note('suspend', ctx.suspend())");
    assert!(wait_for(&mut page, "log.length == 4", 5), "suspends");
    eval(&mut page, "window.held = ctx.currentTime");
    run_for(&mut page, 150);
    assert_eq!(eval(&mut page, "ctx.currentTime == held"), "true");
    eval(&mut page, "note('resume', ctx.resume())");
    assert!(wait_for(&mut page, "log.length == 6", 5), "resumes");
    assert!(
        wait_for(&mut page, "ctx.currentTime > held", 5),
        "the clock goes on"
    );
    eval(&mut page, "note('close', ctx.close())");
    assert!(wait_for(&mut page, "log.length == 8", 5), "closes");
    assert_eq!(
        eval(&mut page, "log.slice(2).join(' ')"),
        "suspend:ok:suspended statechange:suspended resume:ok:running statechange:running \
         close:ok:closed statechange:closed"
    );
    // Closed: the methods reject, nodes can still be made.
    eval(
        &mut page,
        "note('close2', ctx.close()); note('resume2', ctx.resume()); note('suspend2', ctx.suspend());
         ctx.createGain(); ctx.createOscillator();",
    );
    assert!(wait_for(&mut page, "log.length == 11", 5));
    assert_eq!(
        eval(&mut page, "log.slice(8).join(' ')"),
        "close2:InvalidStateError resume2:InvalidStateError suspend2:InvalidStateError"
    );
}

#[test]
fn without_activation_it_stays_suspended_and_resume_waits_for_it() {
    let mut page = page(AutoplayPolicy::UserActivation);
    eval(
        &mut page,
        &format!(
            "{LOG} window.ctx = track(new AudioContext()); note('resume1', ctx.resume());
             window.policy = [navigator.getAutoplayPolicy(ctx)];"
        ),
    );
    run_for(&mut page, 200);
    assert_eq!(
        eval(
            &mut page,
            "const t = ctx.getOutputTimestamp();
             [ctx.state, ctx.currentTime, log.length, policy, t.contextTime, t.performanceTime]
               .join(' ')"
        ),
        // Nothing rendered yet: the timestamp is zeros.
        "suspended 0 0 disallowed 0 0"
    );
    // A user activation lets it start; the next resume() starts it, and the waiting promise
    // resolves first.
    activate(&mut page);
    assert_eq!(
        eval(&mut page, "navigator.getAutoplayPolicy(ctx)"),
        "allowed"
    );
    eval(&mut page, "note('resume2', ctx.resume())");
    assert!(wait_for(&mut page, "log.length == 3", 5), "starts");
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "resume1:ok:running resume2:ok:running statechange:running"
    );
    assert!(
        wait_for(&mut page, "ctx.currentTime > 0", 5),
        "the clock runs"
    );
}

#[test]
fn a_source_started_after_activation_starts_the_context() {
    let mut page = page(AutoplayPolicy::UserActivation);
    eval(
        &mut page,
        &format!(
            "{LOG} window.ctx = track(new AudioContext()); note('resume', ctx.resume());
             ctx.createConstantSource().start();"
        ),
    );
    run_for(&mut page, 100);
    assert_eq!(eval(&mut page, "ctx.state + log.length"), "suspended0");
    activate(&mut page);
    eval(&mut page, "ctx.createOscillator().start()");
    assert!(wait_for(&mut page, "log.length == 2", 5), "starts");
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "resume:ok:running statechange:running"
    );
}

#[test]
fn close_rejects_the_resume_promises_still_waiting() {
    let mut page = page(AutoplayPolicy::UserActivation);
    eval(
        &mut page,
        &format!(
            "{LOG} window.ctx = track(new AudioContext());
             note('resume', ctx.resume()); note('close', ctx.close());"
        ),
    );
    assert!(wait_for(&mut page, "log.length == 3", 5), "closes");
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "resume:InvalidStateError close:ok:closed statechange:closed"
    );
}
