// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §7.4.3 "page load processing model for media": an image opened on its own is a document
//! whose body's one child is an `img`. A top-level one is shown centred on a dark background,
//! shrunk to fit the viewport when it is larger, a click toggling between fitted and natural size;
//! its title is the file's name and the image's natural size.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// An opaque PNG of `width` by `height`.
fn png(width: u32, height: u32) -> Vec<u8> {
    let file = std::env::temp_dir().join(format!(
        "snowlark-image-document-{width}x{height}-{}.png",
        std::process::id()
    ));
    let pixels = vec![200u8; (width * height * 4) as usize];
    snowlark_paint::image::encode_png(width, height, &pixels, &file).unwrap();
    let bytes = std::fs::read(&file).unwrap();
    let _ = std::fs::remove_file(&file);
    bytes
}

/// A rendered 800x600 page that navigated to `name`, an image of `width` by `height`.
fn image_page(net: &NetThread, name: &str, width: u32, height: u32) -> Page {
    let port = crate::webgl::serve_png(png(width, height));
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("http://127.0.0.1:{port}/{name}")).unwrap());
    let start = std::time::Instant::now();
    while !page.load_complete() {
        assert!(start.elapsed() < Duration::from_secs(10), "the page loads");
        page.run_for(Duration::from_millis(10));
    }
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

/// What the image document shows: its title, the body's children, the image's size attributes,
/// offsets, cursor and backdrop, the body's background and the viewport's scroll position.
const LOOK: &str = "(() => {
    const i = document.body.firstChild, s = getComputedStyle(i);
    return [document.title, document.body.childNodes.length + i.nodeName,
            i.getAttribute('width') + 'x' + i.getAttribute('height'),
            i.offsetLeft + ',' + i.offsetTop, s.cursor, s.backgroundColor,
            getComputedStyle(document.body).backgroundColor, scrollX + ',' + scrollY].join('|');
})()";

fn click(page: &mut Page, x: f32, y: f32) {
    let none = Modifiers::default();
    page.pointer_moved(Point::new(x, y), none);
    page.pointer_pressed(Point::new(x, y), 0, none, 1);
    page.pointer_released(Point::new(x, y), 0, none, 1);
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
}

#[test]
fn a_large_image_is_fitted_centred_and_zoomed_by_a_click() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = image_page(&net, "big%20one.png", 1600, 900);
    // (half size fits: 800x450, centred vertically)
    assert_eq!(
        eval(&mut page, LOOK),
        "big one.png (1600\u{d7}900)|1IMG|800x450|0,75|zoom-in|rgb(230, 230, 230)|\
         rgb(14, 14, 14)|0,0"
    );
    // A click shows its natural size, the point clicked in the middle of the viewport: (400,
    // 300) is (800, 450) in the image.
    click(&mut page, 400.0, 300.0);
    assert_eq!(
        eval(&mut page, LOOK),
        "big one.png (1600\u{d7}900)|1IMG|1600x900|0,0|zoom-out|rgb(230, 230, 230)|\
         rgb(14, 14, 14)|400,150"
    );
    // and another fits it again
    click(&mut page, 400.0, 300.0);
    let look = eval(&mut page, LOOK);
    assert!(look.contains("|800x450|0,75|zoom-in|"), "{look}");
}

#[test]
fn a_small_image_is_centred_at_its_natural_size() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = image_page(&net, "small.png", 10, 20);
    assert_eq!(
        eval(&mut page, LOOK),
        "small.png (10\u{d7}20)|1IMG|nullxnull|395,290|auto|rgb(230, 230, 230)|\
         rgb(14, 14, 14)|0,0"
    );
    // a click changes nothing
    click(&mut page, 400.0, 300.0);
    let look = eval(&mut page, LOOK);
    assert!(look.contains("|nullxnull|395,290|auto|"), "{look}");
}
