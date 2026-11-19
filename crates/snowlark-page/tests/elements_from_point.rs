// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSSOM View §5 `elementsFromPoint()`: the elements of every box under the point, topmost first
//! (an overlapped sibling too, not only the topmost element's ancestors), with no hit for table
//! rows and row groups, and the root element last.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

#[test]
fn every_box_under_the_point_topmost_first() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    p.load_html(
        "<!doctype html><body style='margin:0;height:600px'>\
         <div id=under style='width:100px;height:100px'></div>\
         <div id=over style='position:absolute;left:50px;top:50px;width:100px;height:100px'>\
         </div><table id=t style='position:absolute;top:200px;border-spacing:0'>\
         <tr><td id=c style='width:50px;height:50px'></td></tr></table>",
        &url::Url::parse("https://example.test/").unwrap(),
    );
    assert!(p.run_until_settled(Duration::from_secs(5)));
    let r = p
        .eval(
            "const ids = (x, y) => document.elementsFromPoint(x, y)\
             .map(e => e.id || e.localName).join(','); [ids(75, 75), ids(10, 220)].join('|')",
            "test.js",
        )
        .unwrap();
    assert_eq!(r, "over,under,body,html|c,t,body,html");
}
