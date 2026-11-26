// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Transforms 2 §11 `backface-visibility: hidden`: an element whose 3D transformation matrix
//! turns its back face toward the viewer (a negative row 3, column 3 component) is not rendered,
//! nor is it the target of a hit test; the point goes to what is beneath it. In a 3D rendering
//! context (`transform-style: preserve-3d`) the accumulated matrix decides: a child turned back by
//! its parent's turn shows.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

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

/// Boxes 100 px wide side by side over a paragraph, each with a transform and the
/// `backface-visibility` its id says.
const PAGE: &str = "<!doctype html><style>body{margin:0} p{margin:0;height:200px}\
    div{position:absolute;top:0;width:100px;height:100px;backface-visibility:hidden}</style>\
    <p id=under>under</p>\
    <div id=flipped style='left:0;transform:rotateY(180deg)'><span>back</span></div>\
    <div id=shown style='left:100px;transform:rotateY(180deg);backface-visibility:visible'></div>\
    <div id=twice style='left:200px;transform:rotateY(180deg) rotateY(180deg)'></div>\
    <div id=rotate style='left:300px;rotate:x 180deg'></div>\
    <div id=scale style='left:400px;scale:1 1 -1'></div>\
    <div id=edge style='left:500px;transform:rotateY(60deg)'></div>\
    <div id=flat style='left:600px;transform:rotate(180deg)'></div>\
    <div id=outer3d style='left:700px;transform-style:preserve-3d;transform:rotateY(180deg);backface-visibility:visible'>\
    <div id=inner3d style='left:0;transform:rotateY(180deg)'></div></div>";

#[test]
fn an_element_showing_its_hidden_back_face_is_not_hit() {
    let mut page = page_with(PAGE);
    assert_eq!(
        eval(
            &mut page,
            "[50, 150, 250, 350, 450, 550, 650, 750].map(x => document.elementFromPoint(x, 50).id).join()"
        ),
        "under,shown,twice,under,under,edge,flat,inner3d"
    );
}
