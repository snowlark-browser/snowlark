// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS UI 4 §6.2 `accent-color`: the colour of checked checkboxes and radio buttons, range and
//! progress fills; and the other CSS UI 4 and CSS Values 5 properties that were refused before
//! (`field-sizing`, `interpolate-size`) parse and compute.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::{Point, Size};

fn rendered(net: &NetThread, html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (400, 100),
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

/// How many pixels of each 100 px column are close to `(r, g, b)`.
fn near_per_column(page: &mut Page, (r, g, b): (u8, u8, u8)) -> Vec<usize> {
    page.engine()
        .with_renderer(|ren, _| {
            let pm = ren
                .rasterize(Size::new(400.0, 100.0), Point::ZERO, 1.0)
                .unwrap();
            (0..4)
                .map(|c| {
                    let mut n = 0;
                    for x in c * 100..(c + 1) * 100 {
                        for y in 0..100 {
                            let p = pm.pixel(x, y);
                            let d = (p.r as i32 - r as i32).abs()
                                + (p.g as i32 - g as i32).abs()
                                + (p.b as i32 - b as i32).abs();
                            if d < 30 {
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
fn accent_color_paints_checked_controls() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; background: white }\
         input { position: absolute; top: 10px; width: 60px; height: 60px; margin: 0 }\
         #plain { left: 10px } #red { left: 110px; accent-color: rgb(255, 0, 0) }\
         #yellow { left: 210px; accent-color: rgb(255, 255, 0) }\
         div { position: absolute; left: 310px; accent-color: rgb(0, 160, 0) }</style>\
         <input type=checkbox checked id=plain><input type=checkbox checked id=red>\
         <input type=checkbox checked id=yellow>\
         <div><input type=radio checked id=green style='left: 10px'></div>",
    );
    assert_eq!(
        eval(
            &mut page,
            "[CSS.supports('accent-color', 'red'), getComputedStyle(plain).accentColor, \
              getComputedStyle(red).accentColor, getComputedStyle(green).accentColor].join('|')"
        ),
        "true|auto|rgb(255, 0, 0)|rgb(0, 160, 0)"
    );
    let blue = near_per_column(&mut page, (0x00, 0x75, 0xff));
    let red = near_per_column(&mut page, (255, 0, 0));
    let green = near_per_column(&mut page, (0, 160, 0));
    let dark = near_per_column(&mut page, (0x3b, 0x3b, 0x3b));
    let white = near_per_column(&mut page, (255, 255, 255));
    // The default accent, then the page's: red, yellow, green (inherited by the radio button).
    assert!(blue[0] > 1000 && red[0] == 0, "{blue:?} {red:?}");
    assert!(red[1] > 1000 && blue[1] == 0, "{blue:?} {red:?}");
    assert!(green[3] > 500 && blue[3] == 0, "{green:?} {blue:?}");
    // A light accent gets a dark checkmark; a dark one keeps the white one.
    assert!(dark[2] > 20 && dark[1] == 0, "{dark:?}");
    assert!(white[1] > 100 * 100 - 60 * 60, "{white:?}");
}

#[test]
fn field_sizing_and_interpolate_size_parse_and_compute() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>textarea { field-sizing: content }\
         :root { interpolate-size: allow-keywords }</style><textarea id=t></textarea><p id=p></p>",
    );
    assert_eq!(
        eval(
            &mut page,
            "[CSS.supports('field-sizing', 'content'), CSS.supports('field-sizing', 'fixed'), \
              CSS.supports('field-sizing', 'auto'), getComputedStyle(t).fieldSizing, \
              getComputedStyle(p).fieldSizing, \
              CSS.supports('interpolate-size', 'allow-keywords'), \
              CSS.supports('interpolate-size', 'auto'), getComputedStyle(p).interpolateSize, \
              getComputedStyle(document.head).interpolateSize, \
              document.body.style.interpolateSize === ''].join('|')"
        ),
        "true|true|false|content|fixed|true|false|allow-keywords|allow-keywords|true"
    );
    // (CSS Text 4 §5.2 `word-break: auto-phrase` computes as itself)
    assert_eq!(
        eval(
            &mut page,
            "p.style.wordBreak = 'auto-phrase'; \
             [CSS.supports('word-break', 'auto-phrase'), getComputedStyle(p).wordBreak].join('|')"
        ),
        "true|auto-phrase"
    );
}

#[test]
fn field_sizing_content_sizes_controls_from_their_value() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>input, textarea { font: 20px/30px sans-serif; padding: 0;\
         border: 0 } .c { field-sizing: content } textarea { width: 100px }</style>\
         <input class=c id=one value=W><input class=c id=ten value=WWWWWWWWWW>\
         <input id=f1 value=W><input id=f10 value=WWWWWWWWWW>\
         <textarea class=c id=short>one</textarea>\
         <textarea class=c id=long>one two three four five six seven</textarea>\
         <textarea class=c id=lines rows=9>a\nb\n</textarea><textarea id=fixed rows=4>a</textarea>",
    );
    let sizes = eval(
        &mut page,
        "[one, ten, f1, f10, short, long, lines, fixed].map(e => e.offsetWidth + 'x' + \
         e.clientHeight).join(' ')",
    );
    let sizes: Vec<(f32, f32)> = sizes
        .split(' ')
        .map(|s| {
            let (w, h) = s.split_once('x').unwrap();
            (w.parse().unwrap(), h.parse().unwrap())
        })
        .collect();
    let [one, ten, f1, f10, short, long, lines, fixed] = sizes[..] else {
        panic!("{sizes:?}")
    };
    // A field grows with its value; a fixed one keeps its `size`'s width.
    assert!(ten.0 > one.0 + 100.0 && one.0 < 40.0, "{sizes:?}");
    assert_eq!(f1, f10, "{sizes:?}");
    // A textarea is as tall as its text wrapped to its width, whatever its `rows`; a final
    // newline starts a line.
    assert_eq!(short.1, 30.0, "{sizes:?}");
    assert!(long.1 >= 90.0 && long.1 % 30.0 == 0.0, "{sizes:?}");
    assert_eq!(lines.1, 90.0, "{sizes:?}");
    assert_eq!(fixed.1, 120.0, "{sizes:?}");
}
