// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hit testing replaced elements (CSS UI 4 §5.2 `pointer-events`, CSS 2 §11.2 `visibility`): a
//! canvas, image or video with `pointer-events: none` or hidden is not the target of pointer
//! events, and the point goes to what is beneath it; an outermost `svg` element's graphics
//! elements take hits by their own `pointer-events` (SVG 2 §16.5). A press and drag over such an
//! overlay selects the text beneath, not the overlay's own.

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

/// A paragraph under a full-page overlay of `pointer-events: none` (inherited by what is inside
/// it) holding a canvas, then an image, a video and a canvas with their own `pointer-events` or
/// `visibility`, side by side over the paragraph; the overlay's last child is text (as a hidden
/// keyboard help would be), after the paragraph in tree order.
const OVERLAY: &str = "<!doctype html><style>body{margin:0;font:20px/20px monospace}\
    p{margin:0;height:200px} .o{position:absolute;inset:0;pointer-events:none}\
    .o>*{position:absolute;top:0;width:100px;height:100px}\
    .k{position:absolute;width:1px;height:1px;overflow:hidden;clip-path:inset(50%);white-space:nowrap}</style>\
    <p id=under>Text under the overlay, long enough to fill the first line of the page here</p>\
    <div class=o><canvas id=c style=left:0></canvas>\
    <img id=i style='left:100px;pointer-events:auto;visibility:hidden' src='data:image/gif;base64,R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw=='>\
    <video id=v style=left:200px></video>\
    <canvas id=h style='left:300px;pointer-events:auto;visibility:hidden'></canvas>\
    <canvas id=a style='left:400px;pointer-events:auto'></canvas>\
    <div class=k><p>Keyboard help text</p><button>button</button></div></div>";

#[test]
fn a_replaced_element_without_pointer_events_or_hidden_is_not_hit() {
    let mut page = page_with(OVERLAY);
    assert_eq!(
        eval(
            &mut page,
            "[50, 150, 250, 350, 450].map(x => document.elementFromPoint(x, 10).id).join()"
        ),
        "under,under,under,under,a"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.elementsFromPoint(50, 10).map(e => e.id || e.localName).join()"
        ),
        "under,body,html"
    );
}

#[test]
fn an_svg_element_sets_pointer_events_back_inside_an_svg_root_without_them() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}</style><p id=under style='margin:0;height:200px'>x</p>\
         <svg id=s style='position:absolute;left:0;top:0;pointer-events:none' width=200 height=100>\
         <rect id=r width=100 height=100 style=pointer-events:auto /></svg>",
    );
    assert_eq!(eval(&mut page, "document.elementFromPoint(50, 50).id"), "r");
}

#[test]
fn a_drag_over_an_overlay_without_pointer_events_selects_the_text_beneath() {
    let mut page = page_with(OVERLAY);
    let none = Modifiers::default();
    page.pointer_moved(Point::new(20.0, 10.0), none);
    page.pointer_pressed(Point::new(20.0, 10.0), 0, none, 1);
    page.pointer_moved(Point::new(81.0, 10.0), none);
    page.pointer_released(Point::new(81.0, 10.0), 0, none, 1);
    // (both ends in the paragraph: not the overlay's text, which follows it in tree order)
    assert_eq!(
        eval(
            &mut page,
            "const s = getSelection();\
             [s.anchorNode.parentNode.id, s.focusNode.parentNode.id, s.toString().length > 2].join()"
        ),
        "under,under,true"
    );
}
