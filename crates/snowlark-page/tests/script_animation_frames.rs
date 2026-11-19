// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Script animations and rendering opportunities: a running animation whose effect cannot show (its
//! target is not being rendered, or not connected) asks for no rendering updates but the one at which
//! it finishes (HTML "update the rendering": updates with no visible effect may be skipped); one whose
//! target is rendered asks for every frame.

use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Runs the event loop for `ms` (`run_until` returns early when no task, timer or callback is left,
/// and running animations are not counted as work: time has to pass for them here).
fn run(page: &mut Page, ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        page.run_until(deadline, |_| false);
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(2)));
    }
}

/// How long until the event loop wants to wake (`None`: never by itself).
fn wake_in(page: &Page) -> Option<Duration> {
    page.engine()
        .next_wakeup()
        .map(|t| t.saturating_duration_since(Instant::now()))
}

#[test]
fn script_animations_that_cannot_show_ask_only_for_the_update_at_their_finish() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><div id=shown>a</div><div style='display: none'><span id=hidden>b</span>\
         </div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    page.update_the_rendering();

    // An endless animation in a `display: none` subtree and a long one of a disconnected element:
    // updates while their play tasks are pending, then none (the long one's finish is far away).
    eval(
        &mut page,
        "var hidden = document.getElementById('hidden').animate(\
             {opacity: [0, 1]}, {duration: 1000, iterations: Infinity});\
         var loose = document.createElement('div').animate({opacity: [0, 1]}, 1000000);",
    );
    assert!(wake_in(&page).is_some_and(|w| w <= Duration::from_millis(20)));
    run(&mut page, 100);
    assert_eq!(
        eval(&mut page, "hidden.playState + ' ' + loose.playState"),
        "running running"
    );
    let wait = wake_in(&page);
    assert!(
        wait.is_none_or(|w| w > Duration::from_secs(100)),
        "{wait:?}"
    );

    // A short one of a disconnected element still finishes: the loop wakes for its end.
    eval(
        &mut page,
        "var done = false;\
         var short = document.createElement('div').animate({opacity: [0, 1]}, 150);\
         short.onfinish = () => done = true;",
    );
    run(&mut page, 50);
    let wait = wake_in(&page).expect("wakes for the finish");
    assert!(wait <= Duration::from_millis(150), "{wait:?}");
    run(&mut page, 400);
    assert_eq!(
        eval(&mut page, "done + ' ' + short.playState"),
        "true finished"
    );

    // Once the hidden element is rendered its animation asks for every frame again.
    eval(
        &mut page,
        "document.getElementById('hidden').parentNode.style.display = 'block'",
    );
    run(&mut page, 100);
    assert!(wake_in(&page).is_some_and(|w| w <= Duration::from_millis(20)));
}
