// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pointer Events 3 pointer capture: `setPointerCapture` while a button is down takes effect at the
//! next pointer event (`gotpointercapture`), pointer and mouse events then go to the capture
//! target wherever the pointer is, and the release after `pointerup` fires `lostpointercapture`
//! before the click, which goes to the capture target.

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

/// `a` (100x50 at 0,0) and `b` (100x50 at 0,50); pointer, capture, mouse button and click events
/// are logged as `type@id`.
const PAGE: &str = "<!doctype html><style>body{margin:0}div{width:100px;height:50px}</style>\
    <div id=a></div><div id=b></div><script>var log = [];\
    for (const type of ['pointerdown','pointermove','pointerup','pointerover','gotpointercapture',\
      'lostpointercapture','mousemove','mouseup','click'])\
      document.addEventListener(type, e => { if (e.target.id) log.push(type + '@' + e.target.id) }, true);\
    a.addEventListener('pointerdown', e => a.setPointerCapture(e.pointerId));</script>";

#[test]
fn a_captured_pointer_sends_its_events_to_the_capture_target() {
    let mut page = page_with(PAGE);
    let none = Modifiers::default();
    page.pointer_moved(Point::new(50.0, 25.0), none);
    page.pointer_pressed(Point::new(50.0, 25.0), 0, none, 1);
    assert_eq!(eval(&mut page, "String(a.hasPointerCapture(1))"), "true");
    eval(&mut page, "log = []");
    page.pointer_moved(Point::new(50.0, 75.0), none);
    assert_eq!(
        eval(&mut page, "log.join()"),
        "gotpointercapture@a,pointermove@a,mousemove@a"
    );
    eval(&mut page, "log = []");
    page.pointer_released(Point::new(50.0, 75.0), 0, none, 1);
    assert_eq!(
        eval(&mut page, "log.join()"),
        "pointerup@a,mouseup@a,lostpointercapture@a,click@a,pointerover@b"
    );
    assert_eq!(eval(&mut page, "String(a.hasPointerCapture(1))"), "false");
}

#[test]
fn an_element_of_another_frame_does_not_capture_the_pointer() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}iframe{border:0;width:200px;height:200px;display:block}</style>\
         <iframe srcdoc=\"<style>body{margin:0}</style><div style='width:100px;height:100px'></div>\"></iframe>\
         <div id=o style='width:100px;height:50px'></div>",
    );
    eval(
        &mut page,
        "window.r = 'none'; frames[0].document.addEventListener('pointerdown', () => \
         { o.setPointerCapture(1); window.r = String(o.hasPointerCapture(1)); })",
    );
    let none = Modifiers::default();
    page.pointer_moved(Point::new(50.0, 50.0), none);
    page.pointer_pressed(Point::new(50.0, 50.0), 0, none, 1);
    assert_eq!(eval(&mut page, "window.r"), "false");
    page.pointer_released(Point::new(50.0, 50.0), 0, none, 1);
}

#[test]
fn capture_needs_a_button_down() {
    let mut page = page_with(PAGE);
    page.pointer_moved(Point::new(50.0, 25.0), Modifiers::default());
    eval(&mut page, "a.setPointerCapture(1)");
    assert_eq!(eval(&mut page, "String(a.hasPointerCapture(1))"), "false");
}
