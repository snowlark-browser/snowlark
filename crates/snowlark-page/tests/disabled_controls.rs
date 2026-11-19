// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.10.18.5: a disabled form control prevents the click events of user interaction from being
//! dispatched on it. A trusted mousedown, mouseup, click or dblclick stops below the first disabled
//! form control on its path; pointer events and untrusted events are dispatched as usual.

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

/// A disabled 100x50 button at 0,0 in `parent`, with a 20x20 `inner` span in it; the
/// parent logs what reaches it, the span what reaches it.
const DISABLED: &str =
    "<!doctype html><style>body{margin:0}button{display:block;margin:0;padding:0;\
    border:0;width:100px;height:50px}span{display:block;width:20px;height:20px}</style>\
    <div id=parent><button id=b disabled><span id=inner></span></button></div><script>var log = [];\
    for (const type of ['pointerdown','mousedown','pointerup','mouseup','click'])\
      parent.addEventListener(type, e => log.push(type + '@' + e.target.id));\
    inner.addEventListener('mousedown', () => log.push('inner:mousedown'));\
    inner.addEventListener('click', () => log.push('inner:click'));</script>";

fn press(page: &mut Page, x: f32, y: f32) -> String {
    let none = Modifiers::default();
    eval(page, "log = []");
    page.pointer_moved(Point::new(x, y), none);
    page.pointer_pressed(Point::new(x, y), 0, none, 1);
    page.pointer_released(Point::new(x, y), 0, none, 1);
    eval(page, "log.join()")
}

#[test]
fn a_disabled_control_and_its_ancestors_get_no_mouse_button_events() {
    let mut page = page_with(DISABLED);
    assert_eq!(press(&mut page, 80.0, 40.0), "pointerdown@b,pointerup@b");
}

#[test]
fn an_element_inside_a_disabled_control_still_gets_them_but_they_stop_there() {
    let mut page = page_with(DISABLED);
    // (the button centers its content: the span's own center)
    let r = eval(
        &mut page,
        "(r => (r.left + r.width / 2) + ',' + (r.top + r.height / 2))(inner.getBoundingClientRect())",
    );
    let (x, y) = r.split_once(',').unwrap();
    assert_eq!(
        eval(
            &mut page,
            &format!("document.elementFromPoint({x}, {y}).id")
        ),
        "inner"
    );
    assert_eq!(
        press(&mut page, x.parse().unwrap(), y.parse().unwrap()),
        "pointerdown@inner,inner:mousedown,pointerup@inner,inner:click"
    );
}

#[test]
fn an_enabled_control_and_untrusted_events_are_dispatched_as_usual() {
    let mut page = page_with(DISABLED);
    eval(&mut page, "b.disabled = false");
    assert_eq!(
        press(&mut page, 80.0, 40.0),
        "pointerdown@b,mousedown@b,pointerup@b,mouseup@b,click@b"
    );
    eval(&mut page, "b.disabled = true; log = []");
    eval(
        &mut page,
        "b.dispatchEvent(new MouseEvent('click', {bubbles: true}))",
    );
    assert_eq!(eval(&mut page, "log.join()"), "click@b");
}

#[test]
fn a_disabled_fieldset_disables_its_controls_but_not_itself() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}fieldset{margin:0;padding:0;border:0}\
         span,button{display:block;margin:0;padding:0;border:0;width:100px;height:40px}</style>\
         <div id=parent><fieldset id=f disabled><span id=s>s</span><button id=c>c</button></fieldset></div>\
         <script>var log = []; parent.addEventListener('click', e => log.push('click@' + e.target.id));</script>",
    );
    // on the span: the fieldset and its parent see the click
    assert_eq!(press(&mut page, 50.0, 20.0), "click@s");
    // on the button the fieldset disables: nothing
    assert_eq!(press(&mut page, 50.0, 60.0), "");
}
