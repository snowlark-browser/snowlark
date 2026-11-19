// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Animated images through the page's event loop: it wakes for the next frame of an animated image
//! painted in view (HTML §4.8.4.3), not for one scrolled out of view or on a hidden page.

use std::path::Path;
use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn page(net: &NetThread) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    // (the fixture loops forever: red 100 ms, lime 200 ms, blue 300 ms)
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../snowlark-render/tests/fixtures/");
    let url = url::Url::from_file_path(dir.join("animated.html")).unwrap();
    page.load_html(
        "<!doctype html><body style='margin: 0'><img src=anim-rgb.gif>\
         <div style='height: 5000px'></div>",
        &url,
    );
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    assert!(page.load_complete());
    page
}

/// How long until the event loop wants to wake (`None`: never by itself).
fn wake_in(page: &Page) -> Option<Duration> {
    page.engine()
        .next_wakeup()
        .map(|t| t.saturating_duration_since(Instant::now()))
}

#[test]
fn the_loop_wakes_for_frames_in_view_only() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    page.update_the_rendering();
    // Not painted yet: no animation runs.
    assert_eq!(wake_in(&page), None);
    page.engine().paint(Point::ZERO, 1.0).unwrap();
    let wait = wake_in(&page).expect("wakes for the second frame");
    assert!(wait <= Duration::from_millis(100), "{wait:?}");
    // A hidden page (a background tab) does not.
    page.engine().set_shown(false);
    assert_eq!(wake_in(&page), None);
    page.engine().set_shown(true);
    assert!(wake_in(&page).is_some());
    // Nor does one scrolled away from its animated image.
    page.engine().viewport_scrolled(Point::new(0.0, 2000.0));
    page.update_the_rendering();
    page.engine().paint(Point::new(0.0, 2000.0), 1.0).unwrap();
    assert_eq!(wake_in(&page), None);
    page.engine().viewport_scrolled(Point::ZERO);
    page.update_the_rendering();
    page.engine().paint(Point::ZERO, 1.0).unwrap();
    assert!(wake_in(&page).is_some());
    // The frames come: the loop wakes for each while it plays.
    let start = Instant::now();
    let mut seen = Vec::new();
    while start.elapsed() < Duration::from_millis(700) {
        page.run_for(Duration::from_millis(5));
        if page.engine().take_needs_paint() {
            page.engine().paint(Point::ZERO, 1.0);
        }
        let next = page.engine().next_wakeup();
        if let Some(t) = next.filter(|t| !seen.contains(t)) {
            seen.push(t);
        }
        let nap = next.map_or(Duration::from_millis(5), |t| {
            t.saturating_duration_since(Instant::now())
        });
        std::thread::sleep(nap.min(Duration::from_millis(20)));
    }
    // (frames at 100, 300 and 600 ms)
    assert!(seen.len() >= 3, "{seen:?}");
}
