// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Masking 1 §7.7 `mask-composite`: each mask layer is composited onto the layers below it by
//! its operator, and `-webkit-mask-composite` takes the legacy operators too.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::{Color, Point, Size};

fn rendered(net: &NetThread, html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (400, 200),
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

fn pixels(page: &mut Page, points: &[(u32, u32)]) -> Vec<Color> {
    page.engine()
        .with_renderer(|r, _| {
            let pm = r
                .rasterize(Size::new(400.0, 200.0), Point::ZERO, 1.0)
                .unwrap();
            points.iter().map(|&(x, y)| pm.pixel(x, y)).collect()
        })
        .unwrap()
}

fn close(a: Color, b: Color) -> bool {
    (a.r as i32 - b.r as i32).abs() <= 2
        && (a.g as i32 - b.g as i32).abs() <= 2
        && (a.b as i32 - b.b as i32).abs() <= 2
}

#[test]
fn the_legacy_operators_are_values_of_the_prefixed_property_only() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><div id=a style='-webkit-mask-composite: xor, source-over'></div>\
         <div id=b style='mask-composite: xor'></div>",
    );
    assert_eq!(
        eval(
            &mut page,
            "[CSS.supports('-webkit-mask-composite', 'xor'), \
              CSS.supports('-webkit-mask-composite', 'destination-out'), \
              CSS.supports('-webkit-mask-composite', 'exclude'), \
              CSS.supports('mask-composite', 'xor'), \
              CSS.supports('mask', 'none xor'), \
              CSS.supports('mask', 'none exclude')].join()"
        ),
        "true,true,true,false,false,true"
    );
    assert_eq!(
        eval(
            &mut page,
            "[a.style.webkitMaskComposite, getComputedStyle(a).maskComposite, \
              b.style.maskComposite].join('|')"
        ),
        "xor, source-over|xor, source-over|"
    );
}

/// The gradient border: two opaque layers, the top one clipped to the content box. With `exclude`
/// (or `xor`) only the padding ring shows; with `intersect` only the content box; `subtract` of
/// the content box from the whole box is the ring again. Without the operators the layers add up
/// and the whole box shows.
#[test]
fn mask_layers_composite_by_their_operator() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; background: white }\
         div { position: absolute; top: 40px; width: 24px; height: 24px; padding: 8px;\
               background: red }\
         #a { left: 40px; mask: linear-gradient(#000 0 0) content-box, linear-gradient(#000 0 0);\
              mask-composite: exclude }\
         #b { left: 120px; -webkit-mask: linear-gradient(#000 0 0) content-box,\
              linear-gradient(#000 0 0); -webkit-mask-composite: xor }\
         #c { left: 200px; mask: linear-gradient(#000 0 0) content-box, linear-gradient(#000 0 0);\
              mask-composite: intersect }\
         #d { left: 280px; mask: linear-gradient(#000 0 0), linear-gradient(#000 0 0) content-box;\
              mask-composite: subtract }</style>\
         <div id=a></div><div id=b></div><div id=c></div><div id=d></div>",
    );
    let red = Color::rgb(255, 0, 0);
    let white = Color::rgb(255, 255, 255);
    // (each box: its center, inside the content box, then a point of the padding ring)
    let px = pixels(
        &mut page,
        &[
            (60, 60),
            (44, 60),
            (140, 60),
            (124, 60),
            (220, 60),
            (204, 60),
            (300, 60),
            (284, 60),
        ],
    );
    let want = [white, red, white, red, red, white, white, red];
    for (i, (got, want)) in px.iter().zip(want).enumerate() {
        assert!(close(*got, want), "point {i}: {got:?}, want {want:?}");
    }
}
