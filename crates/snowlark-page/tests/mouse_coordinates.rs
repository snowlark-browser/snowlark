// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSSOM View's MouseEvent coordinates: `double` attributes and init members; Pointer Events 3 keeps
//! a pointer event's fractional coordinates (not for `click`, `auxclick`, `contextmenu`), other
//! mouse events give them rounded down; `pageX` adds the window's scroll position and `offsetX` is
//! relative to the target's padding edge while the event is dispatched.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn page_with(html: &str) -> Page {
    static NET: std::sync::OnceLock<NetThread> = std::sync::OnceLock::new();
    let net = NET.get_or_init(|| NetThread::start(NetConfig::default()).unwrap());
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

#[test]
fn constructed_events_keep_or_floor_their_coordinates_by_type() {
    let mut page = page_with("<!doctype html>");
    let js = "const d = {screenX: 0.25, screenY: -0.5, clientX: 1.75, clientY: -1.5}; [\
        Object.values((e => ({a: e.screenX, b: e.screenY, c: e.clientX, d: e.clientY, p: e.pageX, o: e.offsetX}))(new PointerEvent('pointerdown', d))).join(' '),\
        Object.values((e => ({a: e.screenX, b: e.screenY, c: e.clientX, d: e.clientY, p: e.pageX, o: e.offsetX}))(new PointerEvent('click', d))).join(' '),\
        Object.values((e => ({a: e.screenX, b: e.screenY, c: e.clientX, d: e.clientY, p: e.pageX, o: e.offsetX}))(new MouseEvent('mousedown', d))).join(' '),\
        ].join(' | ')";
    assert_eq!(
        eval(&mut page, js),
        "0.25 -0.5 1.75 -1.5 1.75 1.75 | 0 -1 1 -2 1 1 | 0 -1 1 -2 1 1"
    );
}

#[test]
fn a_real_click_has_page_coordinates_with_the_scroll_and_offsets_in_the_target() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0;height:3000px}#t{position:absolute;left:100px;top:250px;\
         width:50px;height:50px;border:2px solid;padding:3px}</style><div id=t></div><script>var got;\
         var ev; document.addEventListener('click', e => { ev = e; got = [e.clientX, e.clientY, e.pageX, e.pageY, e.offsetX, e.offsetY].join() });\
         </script><svg style='position:absolute;left:300px;top:250px' width=100 height=100>\
         <rect x=20 y=20 width=50 height=50 /></svg>",
    );
    eval(&mut page, "scrollTo(0, 200)");
    page.run_until_settled(Duration::from_secs(2));
    let none = Modifiers::default();
    // (the box's border edge is at 100,50 in the viewport)
    let at = Point::new(120.0, 70.0);
    page.pointer_moved(at, none);
    page.pointer_pressed(at, 0, none, 1);
    page.pointer_released(at, 0, none, 1);
    assert_eq!(eval(&mut page, "got"), "120,70,120,270,18,18");
    // after the dispatch the target keeps its offsets; layerX/Y follow pageX/Y
    assert_eq!(
        eval(
            &mut page,
            "[ev.offsetX, ev.offsetY, ev.layerX, ev.layerY].join()"
        ),
        "18,18,120,270"
    );
    // inside an svg element: relative to the outermost svg element
    let at = Point::new(330.0, 80.0);
    page.pointer_moved(at, none);
    page.pointer_pressed(at, 0, none, 1);
    page.pointer_released(at, 0, none, 1);
    assert_eq!(eval(&mut page, "got"), "330,80,330,280,30,30");
}

#[test]
fn pointer_event_tilts_and_angles_are_derived_from_each_other() {
    let mut page = page_with("<!doctype html>");
    let js = "[(e => [e.azimuthAngle, e.altitudeAngle].join())(new PointerEvent('pointermove', {tiltY: 90})),\
        (e => [e.tiltX, e.tiltY].join())(new PointerEvent('pointermove', {azimuthAngle: 0, altitudeAngle: 0})),\
        (e => [e.tiltX, e.tiltY, e.azimuthAngle].join())(new PointerEvent('pointermove', {tiltX: 10, azimuthAngle: 1}))].join(' | ')";
    assert_eq!(
        eval(&mut page, js),
        format!("{},0 | 90,0 | 10,0,1", std::f64::consts::FRAC_PI_2)
    );
}
