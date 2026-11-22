// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Scroll Snap 1: `scroll-snap-type`, `scroll-snap-align` and `scroll-snap-stop`, and the
//! scrolls of a snap container coming to rest on its snap positions (§6.2).

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn rendered(net: &NetThread, html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(html, &url::Url::parse("http://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// A 300 px wide carousel of five 300 px slides (`id=c`, `scroll-snap-type: x <strictness>`).
fn carousel(strictness: &str) -> String {
    format!(
        "<!doctype html><body style='margin:0'>
         <div id=c style='display:flex;width:300px;height:100px;overflow-x:auto;
                          scroll-snap-type:x {strictness}'>
           {}
         </div>",
        (0..5)
            .map(|i| format!(
                "<div id=s{i} style='flex:none;width:300px;height:50px;scroll-snap-align:start'>\
                 </div>"
            ))
            .collect::<String>()
    )
}

#[test]
fn scroll_snap_properties_parse_and_compute() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, &carousel("mandatory"));
    let r = eval(
        &mut page,
        "const c = getComputedStyle(document.getElementById('c'));
         const s = document.getElementById('s1');
         s.style.scrollSnapAlign = 'center end';
         s.style.scrollSnapStop = 'always';
         const t = getComputedStyle(s);
         [c.scrollSnapType, getComputedStyle(document.body).scrollSnapType, t.scrollSnapAlign,
          t.scrollSnapStop, CSS.supports('scroll-snap-type', 'y proximity'),
          CSS.supports('scroll-snap-type', 'mandatory')].join('|')",
    );
    assert_eq!(r, "x mandatory|none|center end|always|true|false");
}

#[test]
fn script_scrolls_of_a_snap_container_rest_on_snap_positions() {
    // §6.2: an end position snaps to the nearest snap position; a scroll by a distance to one in
    // its direction. A proximity container snaps only near one.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, &carousel("mandatory"));
    let r = eval(
        &mut page,
        "const c = document.getElementById('c'), out = [];
         c.scrollTo(140, 0); out.push(c.scrollLeft);
         c.scrollTo({left: 160}); out.push(c.scrollLeft);
         c.scrollBy(10, 0); out.push(c.scrollLeft);
         c.scrollBy({left: -10}); out.push(c.scrollLeft);
         c.scrollLeft = 1000; out.push(c.scrollLeft);
         out.join()",
    );
    assert_eq!(r, "0,300,600,300,900");
    let mut page = rendered(&net, &carousel("proximity"));
    let r = eval(
        &mut page,
        "const c = document.getElementById('c'), out = [];
         c.scrollTo(150, 0); out.push(c.scrollLeft);
         c.scrollTo(220, 0); out.push(c.scrollLeft);
         out.join()",
    );
    assert_eq!(r, "150,300");
}

#[test]
fn wheel_notches_and_keys_take_a_snapping_viewport_to_the_next_snap_position() {
    // A mouse wheel notch (a direction and a distance) and an arrow key (a direction) take the
    // viewport to the next snap position in their direction, smoothly; a notch while it is still
    // going counts from where it is going.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><html style='scroll-snap-type:y mandatory'><body style='margin:0'>
         <section style='height:600px;scroll-snap-align:start'></section>
         <section style='height:600px;scroll-snap-align:start'></section>
         <section style='height:600px;scroll-snap-align:start'></section>
         <section style='height:600px;scroll-snap-align:start'></section>",
    );
    let moved = page.with(|e, cx| e.snap_scroll_by(cx, None, Point::new(0.0, 100.0), false));
    assert!(moved);
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "scrollY"), "600");
    page.with(|e, cx| {
        e.snap_scroll_by(cx, None, Point::new(0.0, 100.0), false);
        e.snap_scroll_by(cx, None, Point::new(0.0, 100.0), false);
    });
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "scrollY"), "1800");
    page.with(|e, cx| e.snap_scroll_by(cx, None, Point::new(0.0, -40.0), true));
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "scrollY"), "1200");
}

#[test]
fn wheel_notches_during_a_smooth_scroll_carry_it_on() {
    // A notch while the last one is still moving the box aims that same scroll further, at the
    // speed it has: no frame stands still or goes back, and it ends at the notches' sum.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><div style='height:5000px'></div>",
    );
    page.with(|e, cx| e.snap_scroll_by(cx, None, Point::new(0.0, 100.0), false));
    let t0 = 1.0e6;
    let mut ys = Vec::new();
    for i in 0..40 {
        page.with(|e, cx| {
            if i == 6 || i == 9 {
                e.snap_scroll_by(cx, None, Point::new(0.0, 100.0), false);
            }
            e.step_smooth_scrolls(cx, t0 + i as f64 * 1000.0 / 60.0);
        });
        ys.push(eval(&mut page, "scrollY").parse::<f64>().unwrap());
    }
    for w in ys.windows(2) {
        assert!(w[1] >= w[0], "{ys:?}");
    }
    for i in 1..14 {
        assert!(ys[i] > ys[i - 1], "frame {i}: {ys:?}");
    }
    // (the frame after each notch moves about as far as the one before it, not from rest)
    for i in [6, 9] {
        assert!(
            ys[i] - ys[i - 1] >= (ys[i - 1] - ys[i - 2]) * 0.8,
            "frame {i}: {ys:?}"
        );
    }
    assert_eq!(*ys.last().unwrap(), 300.0);
}
