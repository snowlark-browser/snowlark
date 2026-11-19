// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.8.14 image maps: pointer input over an `img` element with a `usemap` targets the `area`
//! element whose shape is under the pointer, so its hyperlink follows a real click.

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

/// A 200x100 image at 0,0 with a rectangle (left half's top), a circle (center 150,50, radius 20)
/// and a polygon (a triangle at the bottom left), and nothing over the rest.
const MAPPED: &str = "<!doctype html><style>body{margin:0}img{display:block;width:200px;height:100px}</style>\
    <img id=img usemap='#m' src=\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='200' height='100'/%3E\">\
    <map name=m>\
    <area id=rect coords='0,0,99,40' href='#rect'>\
    <area id=circle shape=circle coords='150,50,20' href='#circle'>\
    <area id=poly shape=poly coords='0,100, 50,60, 100,100' href='#poly'>\
    </map><script>var log = [];\
    for (const type of ['pointerdown','mousedown','mouseup','click'])\
      document.addEventListener(type, e => log.push(type + '@' + e.target.id), true);</script>";

fn click(page: &mut Page, x: f32, y: f32) {
    let none = Modifiers::default();
    page.pointer_moved(Point::new(x, y), none);
    page.pointer_pressed(Point::new(x, y), 0, none, 1);
    page.pointer_released(Point::new(x, y), 0, none, 1);
}

#[test]
fn hit_testing_an_image_map_finds_the_area_under_the_point() {
    let mut page = page_with(MAPPED);
    let at = |page: &mut Page, x: u32, y: u32| {
        eval(
            page,
            &format!("document.elementFromPoint({x}, {y})?.id ?? 'none'"),
        )
    };
    assert_eq!(at(&mut page, 10, 10), "rect");
    assert_eq!(at(&mut page, 150, 50), "circle");
    assert_eq!(at(&mut page, 10, 95), "poly");
    // between the shapes: the image itself
    assert_eq!(at(&mut page, 120, 10), "img");
    assert_eq!(at(&mut page, 90, 55), "img");
}

#[test]
fn a_click_on_an_area_targets_it_and_follows_its_hyperlink() {
    let mut page = page_with(MAPPED);
    click(&mut page, 150.0, 50.0);
    assert_eq!(
        eval(&mut page, "log.join()"),
        "pointerdown@circle,mousedown@circle,mouseup@circle,click@circle"
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "location.hash"), "#circle");
}
