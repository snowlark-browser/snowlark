// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Text Decoration 4 §2.6 `text-decoration-skip-ink`: with `auto` (the initial value) an
//! underline stops where the glyphs' outlines cross it; with `none` it runs through them.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::{Point, Size};

fn rendered(net: &NetThread, html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (400, 400),
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

/// The red pixels in each 100 px band of rows of the viewport.
fn red_per_row_band(page: &mut Page) -> Vec<usize> {
    page.engine()
        .with_renderer(|r, _| {
            let pm = r
                .rasterize(Size::new(400.0, 400.0), Point::ZERO, 1.0)
                .unwrap();
            (0..4)
                .map(|band| {
                    let mut n = 0;
                    for y in band * 100..(band + 1) * 100 {
                        for x in 0..400 {
                            let p = pm.pixel(x, y);
                            if p.r > 200 && p.g < 80 && p.b < 80 {
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
fn underlines_skip_the_ink_of_descenders() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    // (the glyphs themselves are transparent: only the underlines are red)
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; background: white }\
         div { position: absolute; left: 0; font: 60px sans-serif; line-height: 100px;\
               color: transparent; text-decoration: underline red 4px }\
         #a { top: 0 } #b { top: 100px; text-decoration-skip-ink: none }\
         #c { top: 200px } #d { top: 300px; text-decoration-skip-ink: none }</style>\
         <div id=a>gypqgypq</div><div id=b>gypqgypq</div>\
         <div id=c>xxxxxxxx</div><div id=d>xxxxxxxx</div>",
    );
    assert_eq!(
        eval(
            &mut page,
            "[CSS.supports('text-decoration-skip-ink', 'auto'), \
              CSS.supports('text-decoration-skip-ink', 'all'), \
              getComputedStyle(a).textDecorationSkipInk, \
              getComputedStyle(b).textDecorationSkipInk].join('|')"
        ),
        "true|true|auto|none"
    );
    let red = red_per_row_band(&mut page);
    // Descenders cut the `auto` underline into pieces: less of it than of the `none` one. Glyphs
    // that stay above it leave it whole.
    assert!(red[1] > 0 && red[0] < red[1] * 9 / 10, "{red:?}");
    assert!(red[3] > 0 && red[2] == red[3], "{red:?}");
}
