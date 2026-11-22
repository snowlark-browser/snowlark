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
