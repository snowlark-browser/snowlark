// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Scroll Anchoring 1 for the viewport: content above what is visible changing size moves the
//! scroll position with it, except where `overflow-anchor: none` excludes the anchor or a
//! suppression trigger fires; the embedder hears of the adjustment as a relative move.

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

const PAGE: &str = "<!doctype html><style>body { margin: 0; height: 4000px } div { height: 100px }\
                    </style><div id=above></div><div id=anchor>text</div>";

/// Growing a block above the visible one moves the scroll position by as much: the visible block
/// stays where it was, in a script's reads and at the rendering update.
#[test]
fn content_growing_above_the_viewport_keeps_the_visible_content_in_place() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, PAGE);
    let r = eval(
        &mut page,
        "scrollTo(0, 150);
         const before = anchor.getBoundingClientRect().top;
         above.style.height = '300px';
         [before, scrollY, anchor.getBoundingClientRect().top].join()",
    );
    assert_eq!(r, "-50,350,-50");
    // The rendering update does the same for a change no script read.
    eval(&mut page, "above.style.height = '350px'");
    page.update_the_rendering();
    assert_eq!(
        eval(
            &mut page,
            "[scrollY, anchor.getBoundingClientRect().top].join()"
        ),
        "400,-50"
    );
    // The embedder hears of the script's scroll as a place, and of the adjustment after it as
    // part of that place.
    assert_eq!(
        page.engine().take_scroll_request(),
        Some(Point::new(0.0, 400.0))
    );
    assert_eq!(page.engine().take_scroll_adjustment(), None);
    // An adjustment alone is a relative move.
    eval(&mut page, "above.style.height = '360px'");
    page.update_the_rendering();
    assert_eq!(page.engine().take_scroll_request(), None);
    assert_eq!(
        page.engine().take_scroll_adjustment(),
        Some(Point::new(0.0, 10.0))
    );
}

/// `overflow-anchor: none` on the anchor or an ancestor excludes it; at scroll position 0 there
/// is no anchor; a change of the anchor's own position suppresses the adjustment.
#[test]
fn exclusions_and_suppression_triggers() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, PAGE);
    let r = eval(
        &mut page,
        "const out = [];
         document.body.style.overflowAnchor = 'none';
         scrollTo(0, 150);
         above.style.height = '200px';
         out.push(scrollY);
         document.body.style.overflowAnchor = '';
         above.style.height = '';
         scrollTo(0, 0);
         above.style.height = '200px';
         out.push(scrollY);
         above.style.height = '';
         scrollTo(0, 150);
         anchor.style.position = 'relative';
         anchor.style.top = '5px';
         above.style.height = '200px';
         out.push(scrollY);
         out.join()",
    );
    assert_eq!(r, "150,0,150");
}

const SCROLLERS: &str = "<!doctype html><style>body { margin: 0; height: 4000px }\
     #scroller { overflow: scroll; width: 400px; height: 300px }\
     #nested { overflow: scroll; height: 200px }\
     .block { height: 100px }</style>\
     <div id=outerAbove class=block></div><div id=outerAnchor class=block></div>\
     <div id=scroller><div id=above></div><div id=anchor class=block>text</div>\
     <div id=nested><div id=nestedAbove></div><div id=nestedAnchor class=block></div>\
     <div style='height: 600px'></div></div><div style='height: 1000px'></div></div>";

/// A scroll container keeps its visible content in place as the viewport does, each scroller by
/// its own anchor; one with `overflow-anchor: none` does not; a scroll container inside it is
/// walked into with its own scroll position.
#[test]
fn scroll_containers_keep_their_visible_content_in_place() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, SCROLLERS);
    let r = eval(
        &mut page,
        "const out = [];
         scroller.scrollTop = 50;
         scrollTo(0, 150);
         above.style.height = '200px';
         outerAbove.style.height = '150px';
         out.push(scroller.scrollTop, scrollY);
         above.style.height = '';
         outerAbove.style.height = '';
         scroller.style.overflowAnchor = 'none';
         scroller.scrollTop = 50;
         above.style.height = '200px';
         out.push(scroller.scrollTop);
         above.style.height = '';
         scroller.style.overflowAnchor = '';
         scrollTo(0, 0);
         nested.scrollTop = 50;
         scroller.scrollTop = 220;
         nestedAbove.style.height = '30px';
         out.push(nested.scrollTop, scroller.scrollTop);
         out.join()",
    );
    // (the nested scroller's own anchor moves it by 30; the outer one's anchor is inside the
    // nested one, which kept it in place)
    assert_eq!(r, "250,200,50,80,220");
}

/// An element becoming absolutely positioned suppresses the adjustments of the scrollers it is
/// in (the viewport too), not of a scroller it is outside of.
#[test]
fn a_position_change_suppresses_only_the_scrollers_around_it() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, SCROLLERS);
    let r = eval(
        &mut page,
        "scroller.scrollTop = 50;
         scrollTo(0, 150);
         outerAbove.style.position = 'absolute';
         above.style.height = '200px';
         [scroller.scrollTop, scrollY].join()",
    );
    assert_eq!(r, "250,150");
    let mut page = rendered(&net, SCROLLERS);
    let r = eval(
        &mut page,
        "scroller.scrollTop = 50;
         scrollTo(0, 150);
         above.style.position = 'absolute';
         above.style.height = '200px';
         outerAbove.style.height = '150px';
         [scroller.scrollTop, scrollY].join()",
    );
    assert_eq!(r, "50,150");
}

const FOCUS: &str = "<!doctype html><style>body { margin: 0; height: 4000px } .s { height: 100px }\
     #growing { height: 100px } #focused, button { display: block; height: 10px }</style>\
     <div class=s></div><div class=s></div><div id=growing></div><div class=s></div>\
     <div id=focused contenteditable></div><button id=button></button>";

/// The focused element is the anchor when it is editable and visible (a priority candidate):
/// content growing between the first visible block and it keeps it in place; a focused element
/// that is not editable is not one.
#[test]
fn a_focused_editable_element_is_anchored_first() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, FOCUS);
    let r = eval(
        &mut page,
        "scrollTo(0, 150);
         focused.focus();
         const before = focused.getBoundingClientRect().top;
         growing.style.height = '400px';
         [before, focused.getBoundingClientRect().top, scrollY].join()",
    );
    assert_eq!(r, "250,250,450");
    let mut page = rendered(&net, FOCUS);
    let r = eval(
        &mut page,
        "scrollTo(0, 150);
         button.focus();
         growing.style.height = '400px';
         [document.activeElement.id, scrollY].join()",
    );
    assert_eq!(r, "button,150");
}

/// `overflow-anchor: none` set in the same task as the change counts: on the scroller it selects
/// no anchor, on the anchor it excludes it.
#[test]
fn overflow_anchor_none_set_with_the_change_counts() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, SCROLLERS);
    let r = eval(
        &mut page,
        "const out = [];
         scroller.scrollTop = 50;
         above.style.height = '100px';
         out.push(scroller.scrollTop);
         scroller.style.overflowAnchor = 'none';
         above.style.height = '200px';
         out.push(scroller.scrollTop);
         scroller.style.overflowAnchor = '';
         scrollTo(0, 150);
         outerAnchor.style.overflowAnchor = 'none';
         outerAnchor.style.height = '150px';
         out.push(scrollY);
         out.join()",
    );
    // (the excluded block growing moves the next candidate, the scroll container below it)
    assert_eq!(r, "150,150,200");
}

/// A sticky box's `offsetTop` depends on its scroll container's position: the adjustment of a
/// change above the visible content comes first.
#[test]
fn a_sticky_offset_read_sees_the_adjustment() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } #scroller { position: relative; overflow: scroll;\
         width: 200px; height: 200px } #sticky { position: sticky; top: 50px; height: 100px }\
         #spacer { height: 2000px }</style>\
         <div id=scroller><div id=sticky></div><div id=spacer></div></div>",
    );
    let r = eval(
        &mut page,
        "scroller.scrollTop = 100;
         const div = document.createElement('div');
         div.style.height = '65px';
         scroller.insertBefore(div, sticky);
         [sticky.offsetTop, scroller.scrollTop].join()",
    );
    assert_eq!(r, "215,165");
}
