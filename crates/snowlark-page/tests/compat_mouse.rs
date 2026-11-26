// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pointer Events 3 §11.2, compatibility mapping with mouse events: a canceled primary
//! `pointerdown` sets the PREVENT MOUSE EVENT flag, so no `mousedown`, `mousemove` or `mouseup` is
//! fired until the `pointerup` (the `click` still is), and the press's default actions (focus,
//! starting a text selection) do not happen. Canceling a `pointermove` or `pointerup` suppresses
//! nothing.

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

/// Two lines of text 20 px tall over the full width and a focused button below them; pointer and
/// mouse button events are logged by type. `window.cancel` names the events a listener on
/// `window` cancels.
const PAGE: &str = "<!doctype html><style>body{margin:0;font:20px/20px monospace}p{margin:0}</style>\
    <p id=a>First line of text here</p><p id=b>Second line of text here</p><button id=f>f</button>\
    <script>var log = [], cancel = [];\
    for (const type of ['pointerdown','pointermove','pointerup','mousedown','mousemove','mouseup','click'])\
      addEventListener(type, e => { log.push(type); if (cancel.includes(type)) e.preventDefault(); });\
    f.focus();</script>";

/// Presses at the start of the first line, moves to the end of the second and releases.
fn drag_across(page: &mut Page) {
    let none = Modifiers::default();
    page.pointer_moved(Point::new(2.0, 10.0), none);
    eval(page, "log = []");
    page.pointer_pressed(Point::new(2.0, 10.0), 0, none, 1);
    page.pointer_moved(Point::new(100.0, 30.0), none);
    page.pointer_moved(Point::new(200.0, 30.0), none);
    page.pointer_released(Point::new(200.0, 30.0), 0, none, 1);
}

#[test]
fn a_canceled_pointerdown_suppresses_the_compatibility_mouse_events_until_pointerup() {
    let mut page = page_with(PAGE);
    eval(&mut page, "cancel = ['pointerdown']");
    drag_across(&mut page);
    assert_eq!(
        eval(&mut page, "log.join()"),
        "pointerdown,pointermove,pointermove,pointerup,click"
    );
    // The flag ends with the `pointerup`: the next press has its mouse events again.
    eval(&mut page, "cancel = []");
    drag_across(&mut page);
    assert_eq!(
        eval(&mut page, "log.join()"),
        "pointerdown,mousedown,pointermove,mousemove,pointermove,mousemove,pointerup,mouseup,click"
    );
}

#[test]
fn a_canceled_pointerdown_starts_no_selection_and_moves_no_focus() {
    let mut page = page_with(PAGE);
    eval(&mut page, "cancel = ['pointerdown']");
    drag_across(&mut page);
    assert_eq!(
        eval(
            &mut page,
            "[JSON.stringify(getSelection().toString()), document.activeElement.id].join()"
        ),
        "\"\",f"
    );
    // Uncanceled, the same drag selects the text and the press takes the focus from the button.
    eval(&mut page, "cancel = []");
    drag_across(&mut page);
    assert_eq!(
        eval(
            &mut page,
            "[getSelection().toString().length > 20, document.activeElement.id].join()"
        ),
        "true,"
    );
}

#[test]
fn canceling_pointermove_or_pointerup_suppresses_no_mouse_event() {
    let mut page = page_with(PAGE);
    eval(&mut page, "cancel = ['pointermove', 'pointerup']");
    drag_across(&mut page);
    assert_eq!(
        eval(&mut page, "log.join()"),
        "pointerdown,mousedown,pointermove,mousemove,pointermove,mousemove,pointerup,mouseup,click"
    );
}
