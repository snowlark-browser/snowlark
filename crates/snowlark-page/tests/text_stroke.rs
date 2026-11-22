// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compat §5.5-5.7 `-webkit-text-stroke`: the glyphs' outlines stroked over their fill, the
//! outlined text of `-webkit-text-fill-color: transparent` included.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::{Point, Size};

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

/// The dark pixels in each 100 px column of the viewport's first 200 rows.
fn dark_per_column(page: &mut Page) -> Vec<usize> {
    page.engine()
        .with_renderer(|r, _| {
            let pm = r
                .rasterize(Size::new(400.0, 200.0), Point::ZERO, 1.0)
                .unwrap();
            (0..4)
                .map(|c| {
                    let mut n = 0;
                    for x in c * 100..(c + 1) * 100 {
                        for y in 0..200 {
                            let p = pm.pixel(x, y);
                            if (p.r as u32 + p.g as u32 + p.b as u32) < 3 * 128 {
                                n += 1;
                            }
                        }
                    }
                    n
                })
                .collect()
        })
        .unwrap()
}

#[test]
fn text_stroke_outlines_the_glyphs() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; background: white }\
         div { position: absolute; top: 0; font: 120px sans-serif; line-height: 1 }\
         #fill { left: 0; color: black }\
         #outline { left: 100px; -webkit-text-fill-color: transparent; -webkit-text-stroke: 3px black }\
         #none { left: 200px; -webkit-text-fill-color: transparent }\
         #thin { left: 300px; -webkit-text-fill-color: transparent;\
                 -webkit-text-stroke-width: thin; -webkit-text-stroke-color: black }</style>\
         <div id=fill>O</div><div id=outline>O</div><div id=none>O</div><div id=thin>O</div>",
    );
    assert_eq!(
        eval(
            &mut page,
            "[CSS.supports('-webkit-text-stroke', '1px red'), \
              getComputedStyle(outline).webkitTextStrokeWidth, \
              getComputedStyle(outline).webkitTextStrokeColor, \
              getComputedStyle(thin).webkitTextStrokeWidth, \
              outline.style.cssText].join('|')"
        ),
        "true|3px|rgb(0, 0, 0)|1px|"
    );
    let dark = dark_per_column(&mut page);
    // The filled glyph; its outline only (some, but fewer than the fill's); nothing without a
    // stroke; a thin outline, less than the 3px one.
    assert!(dark[0] > 0, "{dark:?}");
    assert!(dark[1] > 0 && dark[1] < dark[0], "{dark:?}");
    assert_eq!(dark[2], 0, "{dark:?}");
    assert!(dark[3] > 0 && dark[3] < dark[1], "{dark:?}");
}
