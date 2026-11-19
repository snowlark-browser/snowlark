// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pointer Events 3 and UI Events boundary events when the document changes under the pointer: the
//! element under the pointer removed (no `out` or `leave` for it, an `over` at the parent that
//! remained, no `enter` again for what it was inside), and content moving under a still pointer
//! (the boundary events at the next rendering update, without a move event).

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

/// `parent` (100x100 at 0,0) holds `child` (50x50 at 0,0); every boundary, button and move event
/// is logged as `type@id`.
const NESTED: &str = "<!doctype html><style>body{margin:0}div{position:absolute;left:0;top:0}\
    #parent{width:100px;height:100px}#child{width:50px;height:50px}</style>\
    <div id=parent><div id=child></div></div><script>var log = [];\
    for (const type of ['pointerover','pointerout','pointerenter','pointerleave','pointerdown',\
      'pointerup','pointermove','mouseover','mouseout','mouseenter','mouseleave','mousemove'])\
      document.addEventListener(type, e => { if (e.target.id) log.push(type + '@' + e.target.id) }, true);\
    </script>";

#[test]
fn removing_the_element_under_the_pointer_gives_an_over_at_the_parent_only() {
    let mut page = page_with(NESTED);
    let none = Modifiers::default();
    let at = Point::new(10.0, 10.0);
    page.pointer_moved(at, none);
    eval(
        &mut page,
        "log = []; child.addEventListener('pointerdown', () => { child.remove(); log.push('(removed)') })",
    );
    page.pointer_pressed(at, 0, none, 1);
    page.pointer_released(at, 0, none, 1);
    let log = eval(&mut page, "log.filter(l => !l.startsWith('mouse')).join()");
    assert_eq!(
        log,
        "pointerdown@child,(removed),pointerover@parent,pointerup@parent"
    );
    let log = eval(&mut page, "log.filter(l => l.startsWith('mouse')).join()");
    assert_eq!(log, "mouseover@parent");
}

#[test]
fn content_moving_under_a_still_pointer_dispatches_boundary_events_at_the_rendering_update() {
    let mut page = page_with(NESTED);
    let none = Modifiers::default();
    // over the parent, beside the child
    page.pointer_moved(Point::new(75.0, 75.0), none);
    eval(
        &mut page,
        "log = []; child.style.left = '50px'; child.style.top = '50px'",
    );
    // (nothing until the rendering updates)
    assert_eq!(eval(&mut page, "log.join()"), "");
    page.update_the_rendering();
    assert_eq!(
        eval(&mut page, "log.join()"),
        "pointerout@parent,mouseout@parent,pointerover@child,pointerenter@child,mouseover@child,mouseenter@child"
    );
    // a rendering update that moved nothing dispatches nothing
    eval(&mut page, "log = []");
    page.update_the_rendering();
    assert_eq!(eval(&mut page, "log.join()"), "");
}

#[test]
fn a_still_pointer_over_a_removed_element_gets_an_over_at_the_parent_at_the_rendering_update() {
    let mut page = page_with(NESTED);
    page.pointer_moved(Point::new(10.0, 10.0), Modifiers::default());
    eval(&mut page, "log = []; child.remove()");
    page.update_the_rendering();
    assert_eq!(
        eval(&mut page, "log.join()"),
        "pointerover@parent,mouseover@parent"
    );
}
