// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS UI 4 §5.1 `resize`: its computed value, and the resizer in the bottom right corner of a
//! scroll container or textarea, whose drag sets the element's `width` and `height` in its style
//! attribute.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn page(net: &NetThread, body: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        &format!(
            "<!doctype html><style>
               body {{ margin: 0 }}
               .box {{ position: absolute; left: 0; top: 0; width: 100px; height: 100px;
                       overflow: scroll }}
             </style>{body}"
        ),
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    page.update_the_rendering();
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    page.eval(js, "t").unwrap()
}

/// A primary press at `from`, a move to `to` and the release there.
fn drag(page: &mut Page, from: (f32, f32), to: (f32, f32)) -> String {
    let none = Modifiers::default();
    page.pointer_moved(Point::new(from.0, from.1), none);
    page.pointer_pressed(Point::new(from.0, from.1), 0, none, 1);
    let cursor = page.pointer_moved(Point::new(to.0, to.1), none).cursor;
    page.pointer_released(Point::new(to.0, to.1), 0, none, 1);
    page.update_the_rendering();
    cursor
}

#[test]
fn resize_computes_as_specified_and_textareas_resize_both_ways() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<div id=a style='resize: vertical'></div><div id=b style='resize: inline'></div>
         <div id=c style='resize: block; writing-mode: vertical-lr'></div><div id=d></div>
         <textarea id=t></textarea>",
    );
    assert_eq!(
        eval(
            &mut p,
            "['a', 'b', 'c', 'd', 't'].map(i => getComputedStyle(document.getElementById(i)).resize).join()"
        ),
        "vertical,horizontal,horizontal,none,both"
    );
}

#[test]
fn dragging_the_resizer_sets_width_and_height_in_px() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<div id=a class=box style='resize: both'></div>");
    // (the bottom right corner's last pixel, then 100 px further)
    let cursor = drag(&mut p, (99.0, 99.0), (199.0, 149.0));
    assert_eq!(cursor, "nwse-resize");
    assert_eq!(
        eval(
            &mut p,
            "const a = document.getElementById('a'); a.getAttribute('style')"
        ),
        "resize: both; width: 200px; height: 150px;"
    );
    assert_eq!(
        eval(&mut p, "a.offsetWidth + ',' + a.offsetHeight"),
        "200,150"
    );
}

#[test]
fn a_resizer_changes_only_the_sizes_resize_names() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<div id=a class=box style='resize: vertical'></div>");
    let cursor = drag(&mut p, (95.0, 95.0), (140.0, 60.0));
    assert_eq!(cursor, "ns-resize");
    assert_eq!(
        eval(&mut p, "document.getElementById('a').getAttribute('style')"),
        "resize: vertical; height: 65px;"
    );
}

#[test]
fn no_resizer_without_resize_or_away_from_the_corner() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<div id=a class=box></div><div id=b class=box style='resize: both; top: 200px'></div>",
    );
    drag(&mut p, (99.0, 99.0), (199.0, 199.0));
    // (the middle of the resizable box)
    drag(&mut p, (50.0, 250.0), (150.0, 350.0));
    assert_eq!(
        eval(
            &mut p,
            "const [a, b] = ['a', 'b'].map(i => document.getElementById(i));
             [a, b].map(e => e.getAttribute('style')).join('|') + ',' + a.offsetWidth + ',' + b.offsetWidth"
        ),
        // (a null attribute joins as nothing)
        "|resize: both; top: 200px,100,100"
    );
}

#[test]
fn a_textarea_resizes_from_its_corner() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<textarea id=t style='position: absolute; left: 0; top: 0; width: 100px; height: 50px;
         padding: 2px; border: 1px solid'></textarea>",
    );
    // (the border box is 106 by 56: the corner's last pixel inside the padding box)
    drag(&mut p, (104.0, 54.0), (124.0, 84.0));
    assert_eq!(
        eval(
            &mut p,
            "const t = document.getElementById('t'); t.style.width + ' ' + t.style.height"
        ),
        "120px 80px"
    );
}
