// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The renderer end to end: a page from disk with a linked stylesheet (which `@import`s another), a
//! `data:` image and an `@font-face` web font (Ahem, from a `data:` URL) loaded asynchronously,
//! rendered to pixels; and (ignored, needs the network) a real page.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use snowlark_net::{NetConfig, NetThread, Url};
use snowlark_paint::{Color, Point, Size, TextEngine};
use snowlark_render::Renderer;
use snowlark_style::DeviceConfig;

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn fixture_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("snowlark-render-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn renderer(net: &NetThread, width: f32, height: f32) -> Renderer {
    Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (width, height),
            scripting: false,
            ..Default::default()
        },
    )
}

fn write(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).unwrap();
}

fn by_id(r: &Renderer, id: &str) -> snowlark_dom::NodeId {
    let dom = r.dom().unwrap();
    dom.descendants(r.document().unwrap())
        .find(|&n| dom.is_element(n) && dom.get_attribute(n, "id") == Some(id))
        .unwrap()
}

#[test]
fn page_with_stylesheet_image_and_web_font() {
    let dir = fixture_dir("page");
    let ahem = include_bytes!("../../snowlark-layout/tests/fixtures/Ahem.ttf");
    // A 20x10 red BMP.
    let rgba: Vec<u8> = (0..20 * 10).flat_map(|_| [255, 0, 0, 255]).collect();
    let bmp = snowlark_paint::image::bmp_from_rgba(20, 10, &rgba);
    write(
        &dir,
        "style.css",
        b"@import url(more.css);
          body { margin: 0 }
          #box { width: 100px; height: 50px; background: rgb(0, 128, 0) }",
    );
    write(
        &dir,
        "more.css",
        b"#imported { height: 30px; background: rgb(0 0 255) }",
    );
    let html = format!(
        "<!DOCTYPE html><html><head><link rel=stylesheet href=style.css>
         <style>@font-face {{ font-family: TestAhem;
             src: url(data:font/ttf;base64,{}) format('truetype') }}
           .ahem {{ font: 40px/1 TestAhem, serif; color: rgb(255, 0, 255); margin: 0 }}</style>
         </head><body><div id=box></div><div id=imported></div>
         <img id=img src='data:image/bmp;base64,{}' style='display:block'>
         <p class=ahem>X</p></body></html>",
        base64(ahem),
        base64(&bmp)
    );
    write(&dir, "page.html", html.as_bytes());
    let url = Url::from_file_path(dir.join("page.html")).unwrap();

    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net, 400.0, 300.0);
    r.load_html(html.as_bytes(), &url, None);
    // The sheet is in flight; nothing waits for it.
    assert!(r.pending() > 0);
    r.restyle();
    let settled = r.render_until_settled(Duration::from_secs(20));
    assert!(settled.complete, "{settled:?}");
    let counts = r.resource_counts();
    assert_eq!((counts.sheets, counts.imports), (1, 1), "{counts:?}");
    assert_eq!(
        (counts.images, counts.fonts, counts.failed),
        (1, 1, 0),
        "{counts:?}"
    );

    let viewport = Size::new(400.0, 300.0);
    r.layout(viewport).unwrap();
    let img = r.geometry(by_id(&r, "img")).unwrap().border_box;
    assert_eq!(
        (img.x, img.y, img.width, img.height),
        (0.0, 80.0, 20.0, 10.0)
    );
    // The imported sheet applied; getComputedStyle's height comes from layout.
    assert_eq!(
        r.geometry(by_id(&r, "imported")).unwrap().border_box.height,
        30.0
    );
    assert_eq!(
        r.resolved_property(by_id(&r, "box"), "width").as_deref(),
        Some("100px")
    );

    let pixmap = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    assert_eq!(pixmap.pixel(50, 25), Color::rgb(0, 128, 0));
    assert_eq!(pixmap.pixel(50, 65), Color::rgb(0, 0, 255));
    assert_eq!(pixmap.pixel(10, 85), Color::rgb(255, 0, 0));
    // Ahem's X is a full em square: a 40x40 magenta block under the image.
    for (x, y) in [(1, 91), (20, 110), (38, 128)] {
        assert_eq!(pixmap.pixel(x, y), Color::rgb(255, 0, 255), "at {x},{y}");
    }
    assert_eq!(pixmap.pixel(45, 110), Color::WHITE);
    let png = dir.join("out.png");
    r.render_png(&png, viewport, viewport, Point::ZERO).unwrap();
    assert!(std::fs::metadata(&png).unwrap().len() > 100);
    // Scrolled by 50 px: the image moves up.
    let scrolled = r.rasterize(viewport, Point::new(0.0, 50.0), 1.0).unwrap();
    assert_eq!(scrolled.pixel(10, 35), Color::rgb(255, 0, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stylesheet_decodes_with_its_charset_rule() {
    let dir = fixture_dir("charset");
    write(
        &dir,
        "a.css",
        b"@charset \"iso-8859-1\";\n#t::before { content: '\xe9' }",
    );
    let html = "<!DOCTYPE html><meta charset=utf-8><link rel=stylesheet href=a.css><p id=t>x</p>";
    write(&dir, "page.html", html.as_bytes());
    let url = Url::from_file_path(dir.join("page.html")).unwrap();
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net, 400.0, 300.0);
    r.load_html(html.as_bytes(), &url, None);
    r.render_until_settled(Duration::from_secs(20));
    let t = by_id(&r, "t");
    let (engine, data) = r.style().unwrap();
    let before = engine
        .pseudo_style(
            &r.dom().unwrap(),
            data,
            t,
            &snowlark_style::PseudoElement::Before,
        )
        .unwrap();
    assert_eq!(
        snowlark_style::computed::computed_property_by_name(&before, "content").unwrap(),
        "\"\u{e9}\""
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn svg_images_size_and_rasterize_at_the_drawn_size() {
    let dir = fixture_dir("svg");
    // 20x10 with no viewBox: a red left half and a blue right half.
    write(
        &dir,
        "fixed.svg",
        b"<svg xmlns='http://www.w3.org/2000/svg' width='20' height='10'>
            <rect width='10' height='10' fill='#f00'/><rect x='10' width='10' height='10' fill='#00f'/>
          </svg>",
    );
    // A viewBox only (ratio 2:1): green.
    write(
        &dir,
        "ratio.svg",
        b"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 2 1'><rect width='2' height='1' fill='#0f0'/></svg>",
    );
    // Not SVG at all: a broken image.
    write(&dir, "broken.svg", b"<html>not svg</html>");
    let html = "<!DOCTYPE html><style>body { margin: 0 } img { display: block }
          #bg { width: 40px; height: 40px; background: url(fixed.svg) no-repeat; background-size: 40px }
          #wrap { width: 100px }
          li { list-style: url(ratio.svg) inside; font-size: 20px; line-height: 1 }
        </style>
        <img id=a src=fixed.svg>
        <img id=b src=fixed.svg width=60>
        <div id=wrap><img id=c src=ratio.svg></div>
        <img id=d src=broken.svg>
        <div id=bg></div>
        <ul style='margin: 0; padding: 0'><li id=li></li></ul>";
    write(&dir, "page.html", html.as_bytes());
    let url = Url::from_file_path(dir.join("page.html")).unwrap();
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net, 400.0, 300.0);
    r.load_html(html.as_bytes(), &url, None);
    r.restyle();
    assert!(r.render_until_settled(Duration::from_secs(20)).complete);
    assert_eq!(r.resource_counts().failed, 1);
    let viewport = Size::new(400.0, 300.0);
    r.layout(viewport).unwrap();
    let rect = |id| {
        let b = r.geometry(by_id(&r, id)).unwrap().border_box;
        (b.x, b.y, b.width, b.height)
    };
    // The natural size; a width with the natural ratio; a ratio only fills the containing block.
    assert_eq!(rect("a"), (0.0, 0.0, 20.0, 10.0));
    assert_eq!(rect("b"), (0.0, 10.0, 60.0, 30.0));
    assert_eq!(rect("c"), (0.0, 40.0, 100.0, 50.0));
    assert_eq!(rect("d"), (0.0, 90.0, 0.0, 0.0));
    assert_eq!(rect("bg"), (0.0, 90.0, 40.0, 40.0));
    let pixmap = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    let red = Color::rgb(255, 0, 0);
    let blue = Color::rgb(0, 0, 255);
    let green = Color::rgb(0, 255, 0);
    assert_eq!(pixmap.pixel(2, 5), red);
    assert_eq!(pixmap.pixel(17, 5), blue);
    // Scaled to 60x30: the vector is rasterized at that size, so the halves split at x = 30.
    assert_eq!(pixmap.pixel(28, 25), red);
    assert_eq!(pixmap.pixel(31, 25), blue);
    assert_eq!(pixmap.pixel(50, 60), green);
    assert_eq!(pixmap.pixel(99, 89), green);
    // The background is 40x20 (background-size: 40px, height from the ratio).
    assert_eq!(pixmap.pixel(18, 100), red);
    assert_eq!(pixmap.pixel(22, 100), blue);
    assert_eq!(pixmap.pixel(20, 115), Color::WHITE);
    // The list marker: ratio only, contained in a 1em (20px) square: 20x10.
    // (Its baseline-aligned position may be fractional: count the pixels that are mostly green.)
    let greenish: Vec<(u32, u32)> = (130..160)
        .flat_map(|y| (0..40).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let c = pixmap.pixel(x, y);
            c.g > 128 && c.r < 128 && c.b < 128
        })
        .collect();
    let xs = greenish.iter().map(|p| p.0);
    let ys = greenish.iter().map(|p| p.1);
    let width = xs.clone().max().unwrap() - xs.min().unwrap() + 1;
    let height = ys.clone().max().unwrap() - ys.min().unwrap() + 1;
    assert_eq!((width, height), (20, 10), "a 20x10 marker image");
    // One raster per distinct size: fixed.svg at 20x10, 60x30 and 40x20, ratio.svg at 100x50 and 20x10.
    assert_eq!(r.svg_stats().rasterizations, 5);
    // At 2x, the same layout rasterizes at twice the pixels: crisp edges at the halves.
    let hi = r.rasterize(viewport, Point::ZERO, 2.0).unwrap();
    assert_eq!(hi.pixel(59, 50), red);
    assert_eq!(hi.pixel(60, 50), blue);
    // (fixed.svg at 40x20 device pixels is the background's raster from the 1x pass.)
    assert_eq!(r.svg_stats().rasterizations, 9);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inline_svg_current_color_resolves_with_each_elements_color() {
    // CSS Color 4 §4.4: `currentcolor` inherits as the keyword and resolves with the element's own
    // `color`, so a `g` that only changes `color` changes what an inherited `fill: currentColor`
    // paints, although its `fill` is the same computed value as its parent's.
    let dir = fixture_dir("isvg-current");
    let html = "<!DOCTYPE html><style>body { margin: 0 } svg { display: block }</style>
        <svg width=40 height=20 style='color: rgb(255, 0, 0)' fill=currentColor>
          <rect width='20' height='20'/>
          <g style='color: rgb(0, 0, 255)'><rect x='20' width='20' height='20'/></g></svg>";
    write(&dir, "page.html", html.as_bytes());
    let url = Url::from_file_path(dir.join("page.html")).unwrap();
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net, 100.0, 100.0);
    r.load_html(html.as_bytes(), &url, None);
    r.restyle();
    assert!(r.render_until_settled(Duration::from_secs(20)).complete);
    let viewport = Size::new(100.0, 100.0);
    r.layout(viewport).unwrap();
    let pixmap = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    assert_eq!(pixmap.pixel(10, 10), Color::rgb(255, 0, 0));
    assert_eq!(pixmap.pixel(30, 10), Color::rgb(0, 0, 255));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inline_svg_images_are_fetched_by_the_document() {
    // SVG 2 §5.7: an inline `svg`'s `<image>` (by `href`, or `xlink:href`) resolves against the
    // document's base URL and is fetched by the document (an SVG *image* loads nothing); the svg,
    // sized by CSS alone, scales its viewBox to that size (§8.7), clip paths applying.
    let dir = fixture_dir("isvg");
    let green = [0u8, 255, 0, 255].repeat(4 * 4);
    snowlark_paint::image::encode_png(4, 4, &green, &dir.join("green.png")).unwrap();
    let html = "<!DOCTYPE html><style>body { margin: 0 } svg { display: block; width: 40px; height: 40px }
        </style>
        <svg id=a viewBox='0 0 121 122'><defs><clipPath id=c><rect width='121' height='61'/></clipPath></defs>
          <image href='green.png' width='100%' height='100%' clip-path='url(#c)'/></svg>
        <svg id=b viewBox='0 0 10 10' xmlns:xlink='http://www.w3.org/1999/xlink'>
          <image xlink:href='green.png' width='10' height='10'/></svg>";
    write(&dir, "page.html", html.as_bytes());
    let url = Url::from_file_path(dir.join("page.html")).unwrap();
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net, 200.0, 200.0);
    r.load_html(html.as_bytes(), &url, None);
    r.restyle();
    assert!(r.render_until_settled(Duration::from_secs(20)).complete);
    let viewport = Size::new(200.0, 200.0);
    r.layout(viewport).unwrap();
    let pixmap = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    let g = Color::rgb(0, 255, 0);
    // The top half of the first (the clip path's), none of its bottom half; all of the second.
    assert_eq!(pixmap.pixel(20, 5), g);
    assert_eq!(pixmap.pixel(20, 35), Color::WHITE);
    assert_eq!(pixmap.pixel(20, 60), g);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pill_buttons_clip_to_their_curve_and_fitting_bordered_scrollers_have_no_bar() {
    // A pill (border-radius above half its height, CSS Backgrounds 3 §5.5) whose `overflow:
    // hidden` clips a positioned highlight to its curve (§5.3), as YouTube's consent buttons do;
    // and a bordered `overflow: auto` box whose content fits (no scroll bar: CSS Overflow 3 §2.2).
    let html = "<!DOCTYPE html><style>body { margin: 0 }
        #pill { position: relative; width: 100px; height: 40px; border-radius: 999px;
                overflow: hidden; background: #000 }
        #wash { position: absolute; left: 0; top: -20px; width: 100%; height: 100%;
                background: #f00; border-radius: 20px }
        #box { margin-top: 10px; width: 100px; height: 50px; overflow: auto;
               border: 1px solid #00f }
        #box div { height: 50px }
      </style><div id=pill><div id=wash></div></div><div id=box><div></div></div>";
    let dir = fixture_dir("pill");
    write(&dir, "page.html", html.as_bytes());
    let url = Url::from_file_path(dir.join("page.html")).unwrap();
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net, 200.0, 200.0);
    r.load_html(html.as_bytes(), &url, None);
    r.restyle();
    assert!(r.render_until_settled(Duration::from_secs(20)).complete);
    let viewport = Size::new(200.0, 200.0);
    r.layout(viewport).unwrap();
    let pixmap = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    // Outside the curve at the top corners: nothing (the wash would be there unclipped).
    assert_eq!(pixmap.pixel(1, 1), Color::WHITE);
    assert_eq!(pixmap.pixel(98, 1), Color::WHITE);
    // Inside: the wash at the top, the pill below it.
    assert_eq!(pixmap.pixel(50, 5), Color::rgb(255, 0, 0));
    assert_eq!(pixmap.pixel(50, 30), Color::BLACK);
    // The box (50..102 in y): no thumb along its right edge.
    for y in 52..100 {
        for x in 90..100 {
            assert_eq!(pixmap.pixel(x, y), Color::WHITE, "at {x},{y}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// CSS Masking 1 §5: `clip-path` clips everything the element paints, its box shadow included,
/// to the shape in its reference box. svt.se widens a block's background with a 1280 px shadow
/// spread and cuts it back to the block's height with `inset(0 -1280px)`: unclipped, the shadow
/// covered the header above it.
#[test]
fn clip_paths_clip_what_the_element_paints() {
    let html = "<!DOCTYPE html><style>body { margin: 0 }
        #inset { width: 100px; height: 100px; background: #c00; clip-path: inset(25px) }
        #circle { position: absolute; left: 120px; top: 0; width: 100px; height: 100px;
                  background: #00c; padding: 10px; clip-path: circle(40px at 50% 50%) content-box }
        #header { position: relative; height: 60px; background: #fff }
        #wide { margin: 0 50px; height: 60px; background: #036;
                box-shadow: #036 0 0 0 200px; clip-path: inset(0 -200px) }
      </style><div id=inset></div><div id=circle></div>
      <div id=header></div><div style='position: relative'><div id=wide></div></div>";
    let dir = fixture_dir("clip-path");
    write(&dir, "page.html", html.as_bytes());
    let url = Url::from_file_path(dir.join("page.html")).unwrap();
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net, 300.0, 300.0);
    r.load_html(html.as_bytes(), &url, None);
    r.restyle();
    assert!(r.render_until_settled(Duration::from_secs(20)).complete);
    let viewport = Size::new(300.0, 300.0);
    r.layout(viewport).unwrap();
    let pixmap = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    let red = Color::rgb(0xcc, 0, 0);
    // inset(25px): only 25..75 shows.
    assert_eq!(pixmap.pixel(10, 10), Color::WHITE);
    assert_eq!(pixmap.pixel(30, 30), red);
    assert_eq!(pixmap.pixel(80, 50), Color::WHITE);
    // A circle of 40 px around the middle of the content box (130..230, 10..110): (180, 60).
    assert_eq!(pixmap.pixel(180, 60), Color::rgb(0, 0, 0xcc));
    assert_eq!(pixmap.pixel(180, 30), Color::rgb(0, 0, 0xcc));
    assert_eq!(
        pixmap.pixel(140, 20),
        Color::WHITE,
        "outside the circle, in the box"
    );
    assert_eq!(pixmap.pixel(125, 5), Color::WHITE);
    // The header (100..160) stays white; the shadow shows beside the block, within its height
    // (160..220), across the whole width.
    let navy = Color::rgb(0, 0x33, 0x66);
    assert_eq!(pixmap.pixel(150, 130), Color::WHITE, "the header");
    assert_eq!(pixmap.pixel(10, 130), Color::WHITE, "the header");
    assert_eq!(pixmap.pixel(150, 190), navy);
    assert_eq!(pixmap.pixel(10, 190), navy, "the shadow beside the block");
    assert_eq!(pixmap.pixel(150, 240), Color::WHITE, "below the block");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn form_controls_render_with_the_native_look() {
    let dir = fixture_dir("controls");
    let html = "<!DOCTYPE html><style>
          body { margin: 0 } .row { position: absolute; left: 0 }
        </style>
        <div class=row style='top: 0'><input id=text value=Hello></div>
        <div class=row style='top: 30px'><input id=on type=checkbox checked><input id=off type=checkbox></div>
        <div class=row style='top: 60px'><input id=styled value=x
          style='border: 3px solid rgb(200, 0, 0); background: rgb(255, 255, 200); width: 100px'></div>
        <div class=row style='top: 100px'><progress id=prog value=0.5></progress></div>
        <div class=row style='top: 130px'><select id=sel><option>One<option selected>Second</select></div>
        <div class=row style='top: 160px'><input id=dis disabled value=No></div>
        <div class=row style='top: 190px'><input id=none type=checkbox checked style='appearance: none'></div>
        <div class=row style='top: 220px; width: 300px'><fieldset id=fs><legend id=lg>Legend</legend>x</fieldset></div>
        <div class=row style='top: 300px'><textarea id=ta rows=3 cols=10>a</textarea><button id=btn>Go</button></div>
        <div class=row style='top: 360px'><input id=bg value=y style='background: white'><input id=bd
          value=z style='border-color: rgb(200, 0, 0)'></div>";
    write(&dir, "page.html", html.as_bytes());
    let url = Url::from_file_path(dir.join("page.html")).unwrap();
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net, 400.0, 400.0);
    r.load_html(html.as_bytes(), &url, None);
    r.render_until_settled(Duration::from_secs(10));
    let viewport = Size::new(400.0, 400.0);
    r.layout(viewport).unwrap();
    let rect = |r: &Renderer, id| r.geometry(by_id(r, id)).unwrap().border_box;
    // A text field: `size` 20 of 13.33px Arial's "0", 2px borders and 1px/2px padding around a
    // normal line: about 21px tall like Chrome's (whose line height rounds Arial's 15.33px to 15).
    let text = rect(&r, "text");
    assert!((21.0..21.5).contains(&text.height), "{text:?}");
    assert!((150.0..175.0).contains(&text.width), "{text:?}");
    let on = rect(&r, "on");
    assert_eq!((on.x, on.width, on.height), (4.0, 13.0, 13.0));
    // (its own 3px border and the UA sheet's 2px inline padding)
    assert_eq!(rect(&r, "styled").width, 100.0 + 2.0 * 3.0 + 2.0 * 2.0);
    // progress: 10em x 1em (it keeps the inherited 16px font).
    let prog = rect(&r, "prog");
    assert_eq!((prog.width, prog.height), (160.0, 16.0));
    assert_eq!(rect(&r, "none").width, 0.0);
    let ta = rect(&r, "ta");
    let btn = rect(&r, "btn");
    assert_eq!(btn.height, text.height);
    // The textarea's baseline is its bottom: the button's text sits near its bottom.
    assert!(btn.bottom() > ta.y + ta.height / 2.0, "{ta:?} {btn:?}");

    let (off, styled, sel, dis) = (
        rect(&r, "off"),
        rect(&r, "styled"),
        rect(&r, "sel"),
        rect(&r, "dis"),
    );
    let (none, fs, lg) = (rect(&r, "none"), rect(&r, "fs"), rect(&r, "lg"));
    let px = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    let at = |x: f32, y: f32| px.pixel(x as u32, y as u32);
    let grey = Color::rgb(0x76, 0x76, 0x76);
    let accent = Color::rgb(0x00, 0x75, 0xff);
    // The field: its own 1px grey frame, white inside, dark text.
    assert_eq!(at(text.x, text.y + 10.0), grey);
    assert_eq!(at(text.x + 1.0, text.y + 10.0), Color::WHITE);
    assert_eq!(at(text.x + 2.0, text.y + 10.0), Color::WHITE);
    // A page that sets only the background or a border color drops the native frame (CSS UI 4
    // §6.2.1): the UA sheet's 2px inset border shows, the same size as the frame and its spacing.
    let (bg, bd) = (rect(&r, "bg"), rect(&r, "bd"));
    assert_eq!((bg.width, bg.height), (text.width, text.height));
    assert_ne!(at(bg.x + 1.0, bg.y + 10.0), Color::WHITE);
    assert_eq!(at(bg.x + 2.0, bg.y + 10.0), Color::WHITE);
    let inner = at(bd.x + 1.0, bd.y + 10.0);
    assert!(inner.r > inner.g + 40 && inner != Color::WHITE, "{inner:?}");
    let dark = (0..40)
        .flat_map(|dx| (0..21).map(move |dy| (dx, dy)))
        .filter(|&(dx, dy)| at(text.x + 4.0 + dx as f32, text.y + dy as f32).r < 100)
        .count();
    assert!(dark > 20, "the value's glyphs");
    // Checked: accent with a white check; unchecked: grey border, white inside.
    assert_eq!(at(on.x + 2.0, on.y + 2.0), accent);
    assert_eq!(at(off.x + 6.0, off.y + 6.0), Color::WHITE);
    assert_eq!(at(off.x + 0.0, off.y + 6.0), grey);
    // Author styles replace the native look.
    assert_eq!(at(styled.x + 1.0, styled.y + 10.0), Color::rgb(200, 0, 0));
    assert_eq!(
        at(styled.x + 5.0, styled.y + 10.0),
        Color::rgb(255, 255, 200)
    );
    // progress at 50%: accent fill, then the light track.
    assert_eq!(at(prog.x + 40.0, prog.y + 8.0), accent);
    assert_eq!(
        at(prog.x + 120.0, prog.y + 8.0),
        Color::rgb(0xef, 0xef, 0xef)
    );
    // The select's drop-down arrow: dark pixels in the last 20px of its content box.
    let arrow = (0..20)
        .flat_map(|dx| (4..16).map(move |dy| (dx, dy)))
        .filter(|&(dx, dy)| at(sel.right() - 21.0 + dx as f32, sel.y + dy as f32).r < 128)
        .count();
    assert!(arrow > 4, "{arrow} arrow pixels");
    // Disabled: a lighter border.
    assert!(at(dis.x, dis.y + 10.0).r > 0x76 + 40);
    // appearance: none draws no check.
    assert_eq!(at(none.x + 1.0, none.y + 1.0), Color::WHITE);
    // The fieldset's top border runs through the legend's middle and stops at the legend.
    let mid = (lg.y + lg.height / 2.0).floor();
    assert_ne!(
        at(fs.x + 5.0, mid),
        Color::WHITE,
        "border left of the legend"
    );
    assert_eq!(at(lg.x + lg.width / 2.0, fs.y), Color::WHITE);
    let _ = std::fs::remove_dir_all(&dir);
}

/// HTML 15.5.14-15, CSS UI 4 §6.2: with `appearance: none`, a `progress` or `meter` element is
/// drawn by its parts, which pages style through `::-webkit-progress-value` and the others: the
/// bar fills the element, the value part takes the position's share of it, and it follows the
/// `value` attribute.
#[test]
fn progress_and_meter_without_native_appearance_draw_their_parts() {
    let dir = fixture_dir("progress-parts");
    let html = "<!DOCTYPE html><style>
          body { margin: 0 }
          progress, meter { appearance: none; display: block; width: 200px; height: 20px;
                            border: none; background: blue }
          #p::-webkit-progress-value { background: rgb(255, 0, 0) }
        </style>
        <progress id=p value=50 max=100>fallback text</progress>
        <progress id=i></progress>
        <meter id=m value=0.25></meter>
        <progress id=n value=0.5 style='appearance: auto'></progress>";
    write(&dir, "page.html", html.as_bytes());
    let url = Url::from_file_path(dir.join("page.html")).unwrap();
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net, 300.0, 120.0);
    r.load_html(html.as_bytes(), &url, None);
    r.render_until_settled(Duration::from_secs(10));
    let viewport = Size::new(300.0, 120.0);
    r.layout(viewport).unwrap();
    let px = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    let red = Color::rgb(255, 0, 0);
    let gray = Color::rgb(128, 128, 128);
    // 50 of 100: the value part fills the first half of the bar, the bar (gray) the rest.
    assert_eq!(px.pixel(50, 10), red);
    assert_eq!(px.pixel(98, 10), red);
    assert_eq!(px.pixel(102, 10), gray);
    assert_eq!(px.pixel(190, 10), gray);
    // Indeterminate: no value part, the bar only.
    assert_eq!(px.pixel(10, 30), gray);
    // The meter: the bar is the middle half of its height, the optimum value (green) a quarter
    // of its width; above the bar the element's own background.
    assert_eq!(px.pixel(100, 42), Color::rgb(0, 0, 255));
    let value = px.pixel(25, 50);
    assert!(value.g > value.r && value.g > value.b, "{value:?}");
    let bar = px.pixel(150, 50);
    assert!(bar.r == bar.g && bar.g == bar.b && bar.r > 0xc0, "{bar:?}");
    // `appearance: auto` keeps the native look: the accent fill.
    assert_eq!(px.pixel(50, 70), Color::rgb(0x00, 0x75, 0xff));

    // Script sets the value: the value part follows.
    let p = by_id(&r, "p");
    let mut dom = r.dom_mut().unwrap();
    dom.set_reactions_enabled(true);
    dom.set_attribute(p, "value", "25").unwrap();
    let reactions = dom.take_reactions();
    drop(dom);
    r.dom_changed(&reactions);
    r.render_until_settled(Duration::from_secs(10));
    r.layout(viewport).unwrap();
    let px = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    assert_eq!(px.pixel(48, 10), red);
    assert_eq!(px.pixel(52, 10), gray);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Needs the network.
#[test]
#[ignore]
fn renders_example_com() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let url = Url::parse("https://example.com/").unwrap();
    let response = net
        .client()
        .fetch(snowlark_net::Request::navigation(url.clone()))
        .read_all()
        .unwrap();
    let mut r = renderer(&net, 1280.0, 800.0);
    r.load_html(response.body_bytes(), &url, None);
    r.restyle();
    let settled = r.render_until_settled(Duration::from_secs(30));
    assert!(settled.complete);
    r.layout(Size::new(1280.0, 800.0)).unwrap();
    let pixmap = r
        .rasterize(Size::new(1280.0, 800.0), Point::ZERO, 1.0)
        .unwrap();
    // example.com's light background, and dark text in the centred column.
    assert_eq!(pixmap.pixel(5, 5), Color::rgb(0xee, 0xee, 0xee));
    let dark = (200..400)
        .flat_map(|y| (400..880).map(move |x| (x, y)))
        .filter(|&(x, y)| pixmap.pixel(x, y).r < 100)
        .count();
    assert!(dark > 200, "{dark} dark pixels");
}

/// `SNOWLARK_HTML=<file>`: prints every element's border box (a debugging aid).
#[test]
#[ignore]
fn dump_geometry() {
    let path = std::env::var("SNOWLARK_HTML").expect("SNOWLARK_HTML");
    let html = std::fs::read(&path).unwrap();
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net, 1280.0, 800.0);
    let url = Url::from_file_path(std::fs::canonicalize(&path).unwrap()).unwrap();
    r.load_html(&html, &url, None);
    r.render_until_settled(Duration::from_secs(10));
    r.layout(Size::new(1280.0, 800.0));
    let dom = r.dom().unwrap();
    for n in dom.descendants(r.document().unwrap()) {
        let (Some(e), Some(g)) = (dom.element(n), r.geometry(n)) else {
            continue;
        };
        let b = g.border_box;
        println!(
            "{:<10} {:>8.2} {:>8.2} {:>8.2} {:>8.2}  {}",
            &*e.name().local,
            b.x,
            b.y,
            b.width,
            b.height,
            dom.get_attribute(n, "class").unwrap_or("")
        );
    }
}

/// A classic vertical scroll bar takes its width from the initial containing block when the
/// document overflows the viewport vertically (or the viewport's `overflow-y` is `scroll`), not when
/// it fits or the viewport's overflow is hidden; media queries and `vw` keep the whole viewport.
#[test]
fn a_classic_scroll_bar_narrows_the_layout_viewport_when_shown() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let url = Url::parse("http://example.test/").unwrap();
    let case = |html: &str| {
        let mut r = renderer(&net, 800.0, 600.0);
        r.set_scrollbar_width(17.0);
        r.load_html(html.as_bytes(), &url, None);
        r.layout(Size::new(800.0, 600.0)).unwrap();
        let width = |id: &str| {
            let n = by_id(&r, id);
            r.geometry(n).unwrap().border_box.width
        };
        (
            r.vertical_scrollbar(),
            r.layout_viewport(Size::new(800.0, 600.0)).width,
            width("block"),
            width("vw"),
            width("mq"),
        )
    };
    let page = |root_style: &str, height: u32| {
        format!(
            "<!doctype html><html style='{root_style}'><style>#mq {{ width: 20px }} @media (min-width: 800px) {{ #mq {{ width: 10px }} }}</style>\
             <body style='margin:0'><div id=block style='height:{height}px'></div>\
             <div id=vw style='width:100vw;height:1px'></div><div id=mq style='height:1px'></div>"
        )
    };
    // Taller than the viewport: the bar shows, layout gets 783 px; 100vw and the media query see 800.
    assert_eq!(case(&page("", 2000)), (true, 783.0, 783.0, 800.0, 10.0));
    // Short: no bar.
    assert_eq!(case(&page("", 100)), (false, 800.0, 800.0, 800.0, 10.0));
    // `overflow: hidden` on the root: never; `overflow-y: scroll`: always.
    assert!(!case(&page("overflow:hidden", 2000)).0);
    // `scrollbar-width: none` on the root hides the bar (CSS Scrollbars 1).
    assert_eq!(
        case(&page("scrollbar-width:none", 2000)),
        (false, 800.0, 800.0, 800.0, 10.0)
    );
    assert_eq!(
        case(&page("overflow-y:scroll", 100)),
        (true, 783.0, 783.0, 800.0, 10.0)
    );
    // Overlay scroll bars (no width): the whole viewport.
    let mut r = renderer(&net, 800.0, 600.0);
    r.load_html(page("", 2000).as_bytes(), &url, None);
    r.layout(Size::new(800.0, 600.0)).unwrap();
    assert!(!r.vertical_scrollbar());
}

/// A scroll container's `::-webkit-scrollbar` parts style its scroll bars when it sets neither
/// `scrollbar-width` nor `scrollbar-color`: `display: none` hides them, the part's `width` is the
/// bar's, the thumb and track are drawn in their backgrounds, and a state pseudo-class after a part
/// does not apply. The viewport's bar takes the body's (else the root's) parts.
#[test]
fn webkit_scrollbar_parts_style_scroll_bars() {
    use snowlark_paint::display_list::Item;
    let net = NetThread::start(NetConfig::default()).unwrap();
    let url = Url::parse("http://example.test/").unwrap();
    let mut r = renderer(&net, 400.0, 600.0);
    r.load_html(
        b"<!doctype html><style>\
          .a, .b, .c { overflow: auto; width: 200px; height: 100px }\
          .a::-webkit-scrollbar { display: none }\
          .b::-webkit-scrollbar { width: 10px }\
          .b::-webkit-scrollbar-thumb { background: rgb(255, 0, 0); border-radius: 5px }\
          .b::-webkit-scrollbar-thumb:hover { background: rgb(0, 255, 0) }\
          .b::-webkit-scrollbar-track { background: rgb(0, 0, 255) }\
          .c { scrollbar-color: rgb(0, 128, 0) rgb(0, 0, 128) }\
          .c::-webkit-scrollbar { display: none }\
          </style><body style='margin:0'>\
          <div class=a><div style='height:300px'></div></div>\
          <div class=b><div style='height:300px'></div></div>\
          <div class=c><div style='height:300px'></div></div>",
        &url,
        None,
    );
    let viewport = Size::new(400.0, 600.0);
    r.layout(viewport).unwrap();
    let list = r.paint(viewport, Point::ZERO);
    let rounded: Vec<_> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRoundedRect { rect, color, .. } => Some((*rect, *color)),
            _ => None,
        })
        .collect();
    let filled = |c: Color| {
        list.items
            .iter()
            .any(|i| matches!(i, Item::FillRect { rect, color } if *color == c && rect.x == 190.0 && rect.width == 10.0 && rect.y == 100.0))
    };
    // `.a`: no bar (the automatic thumb is not drawn).
    assert!(!rounded.iter().any(|(r, _)| r.y < 100.0), "{rounded:?}");
    // `.b`: a 10 px red thumb at the edge over a blue track; `:hover`'s green never applies.
    let red: Vec<_> = rounded
        .iter()
        .filter(|(_, c)| *c == Color::rgb(255, 0, 0))
        .collect();
    assert_eq!(red.len(), 1, "{rounded:?}");
    assert_eq!((red[0].0.x, red[0].0.width), (190.0, 10.0));
    assert!(filled(Color::rgb(0, 0, 255)));
    assert!(!rounded.iter().any(|(_, c)| *c == Color::rgb(0, 255, 0)));
    // `.c`: `scrollbar-color` wins over the parts.
    assert!(rounded.iter().any(|(_, c)| *c == Color::rgb(0, 128, 0)));

    // The viewport's bar: the body's parts, else the root's.
    let case = |css: &str| {
        let mut r = renderer(&net, 400.0, 300.0);
        r.set_scrollbar_width(17.0);
        let html = format!(
            "<!doctype html><style>{css}</style><body style='margin:0'><div style='height:2000px'></div>"
        );
        r.load_html(html.as_bytes(), &url, None);
        r.layout(Size::new(400.0, 300.0)).unwrap();
        let bar = r.viewport_scrollbar();
        (
            r.layout_viewport(Size::new(400.0, 300.0)).width,
            bar.colors.map(|c| (c.thumb, c.track)),
        )
    };
    // (a hidden bar: no thumb, no track)
    let clear = Some((Color::TRANSPARENT, Color::TRANSPARENT));
    assert_eq!(
        case("body::-webkit-scrollbar { display: none }"),
        (400.0, clear)
    );
    assert_eq!(case("::-webkit-scrollbar { width: 0 }"), (400.0, clear));
    assert_eq!(
        case("::-webkit-scrollbar { width: 8px } ::-webkit-scrollbar-thumb { background: red }"),
        (392.0, Some((Color::rgb(255, 0, 0), Color::TRANSPARENT)))
    );
    // A root `scrollbar-width` or `scrollbar-color` wins over them.
    assert_eq!(
        case("html { scrollbar-width: thin } ::-webkit-scrollbar { display: none }"),
        (389.0, None)
    );
    // No rule for the parts: the automatic bar.
    assert_eq!(case(""), (383.0, None));
}

/// CSS Color Adjust 1 §2.4 and CSS Scrollbars 1: the root's used color scheme colors the canvas
/// surface and the viewport's scroll bar, the root's `scrollbar-color` and `scrollbar-width` (not
/// the body's) style that bar, and the canvas surface shows in the headless raster as in the window.
#[test]
fn the_root_styles_the_canvas_and_the_viewport_scroll_bar() {
    use snowlark_layout::style::{ColorScheme, ScrollbarColors};
    let net = NetThread::start(NetConfig::default()).unwrap();
    let url = Url::parse("http://example.test/").unwrap();
    let viewport = Size::new(400.0, 300.0);
    let case = |html: &str| {
        let mut r = renderer(&net, 400.0, 300.0);
        r.set_scrollbar_width(17.0);
        r.load_html(html.as_bytes(), &url, None);
        r.layout(viewport).unwrap();
        r
    };
    let tall = "<div style='height:2000px'></div>";
    let r = case(&format!("<!doctype html>{tall}"));
    let bar = r.viewport_scrollbar();
    assert_eq!(
        (bar.width, bar.color_scheme, bar.colors),
        (17.0, ColorScheme::Light, None)
    );
    assert_eq!(r.canvas_color(), Color::WHITE);

    let mut r = case(&format!(
        "<!doctype html><style>:root {{ color-scheme: dark }}</style>{tall}"
    ));
    assert_eq!(r.viewport_scrollbar().color_scheme, ColorScheme::Dark);
    assert_eq!(r.canvas_color(), Color::rgb(18, 18, 18));
    let pixmap = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    assert_eq!(pixmap.pixel(200, 150), Color::rgb(18, 18, 18));

    // `<meta name=color-scheme>` makes the page dark too; an author background still paints.
    let mut r = case(&format!(
        "<!doctype html><meta name=color-scheme content=dark><body style='background:rgb(0,0,255)'>{tall}"
    ));
    assert_eq!(r.canvas_color(), Color::rgb(18, 18, 18));
    let pixmap = r.rasterize(viewport, Point::ZERO, 1.0).unwrap();
    assert_eq!(pixmap.pixel(200, 150), Color::rgb(0, 0, 255));

    // What shows where the page is not drawn: the canvas background's color (the root's, or the
    // body's when the root has none) over the canvas surface.
    let page = |html: &str| case(&format!("<!doctype html>{html}{tall}")).page_background();
    assert_eq!(page(""), Color::WHITE);
    assert_eq!(
        page("<style>:root { color-scheme: dark }</style>"),
        Color::rgb(18, 18, 18)
    );
    assert_eq!(
        page("<body style='background:rgb(5,11,26)'>"),
        Color::rgb(5, 11, 26)
    );
    assert_eq!(
        page("<style>html { background: rgb(0,128,0) } body { background: red }</style>"),
        Color::rgb(0, 128, 0)
    );
    // (an image alone on the root keeps the body's color off the canvas)
    assert_eq!(
        page("<style>html { background: linear-gradient(red, red) } body { background: blue }</style>"),
        Color::WHITE
    );
    assert_eq!(
        page("<body style='background:rgba(0,0,255,0.2)'>"),
        Color::rgb(204, 204, 255)
    );

    let r = case(&format!(
        "<!doctype html><html style='scrollbar-color: red blue; scrollbar-width: thin'>\
         <body style='scrollbar-width: none'>{tall}"
    ));
    let bar = r.viewport_scrollbar();
    assert_eq!(bar.width, 11.0);
    assert_eq!(r.layout_viewport(viewport).width, 389.0);
    assert_eq!(
        bar.colors,
        Some(ScrollbarColors {
            thumb: Color::rgb(255, 0, 0),
            track: Color::rgb(0, 0, 255)
        })
    );
    // The body's `scrollbar-color` is not the viewport's.
    let r = case(&format!(
        "<!doctype html><body style='scrollbar-color: red blue'>{tall}"
    ));
    assert_eq!(r.viewport_scrollbar().colors, None);
}

/// One renderer laid out at one width, then at others (a window resized by a few pixels, at 150%):
/// `vw` follows the viewport in CSS px, `rem` the root's font size, and lengths ignore the device
/// pixel ratio (bug 21: `rem` boxes and text grew 1.6 times after a resize).
#[test]
fn layout_after_resizes_keeps_root_relative_lengths() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let url = Url::parse("http://example.test/").unwrap();
    let mut r = renderer(&net, 1000.0, 600.0);
    r.set_device_pixel_ratio(1.5);
    r.load_html(
        b"<!doctype html><style>html { font-size: 10px } body { margin: 0 }\
          #dlg { max-height: 90vh; font-size: 1.6rem }\
          #icon { width: 2.4rem; height: 2.4rem } #vw { width: 2vw; height: 1px }\
          #mq { width: 20px; height: 1px } @media (min-width: 900px) { #mq { width: 30px } }\
          </style><div id=dlg><div id=icon></div></div><div id=vw></div><div id=mq></div>",
        &url,
        None,
    );
    for (w, mq) in [
        (1000.0, 30.0),
        (1003.0, 30.0),
        (997.0, 30.0),
        (640.0, 20.0),
        (1000.0, 30.0),
    ] {
        r.layout(Size::new(w, 600.0)).unwrap();
        let width = |id: &str| r.geometry(by_id(&r, id)).unwrap().border_box.width;
        assert_eq!(width("icon"), 24.0, "at {w}");
        assert!((width("vw") - w / 50.0).abs() < 0.02, "at {w}");
        assert_eq!(width("mq"), mq, "at {w}");
    }
}

/// `background-clip: text` (CSS Backgrounds 4 §2.1): the layer and the color under it are painted
/// in a mask group whose mask is the box's text (its descendants' too) as opaque glyphs, and
/// `-webkit-text-fill-color` (Compat §5.4) fills the glyphs themselves.
#[test]
fn background_clip_text_masks_the_background_with_the_text() {
    use snowlark_paint::display_list::Item;
    let net = NetThread::start(NetConfig::default()).unwrap();
    let url = Url::parse("http://example.test/").unwrap();
    let mut r = renderer(&net, 400.0, 600.0);
    r.load_html(
        b"<!doctype html><body style='margin:0; font-size:40px'>\
          <div style='background: linear-gradient(rgb(255, 0, 0), rgb(0, 0, 255));\
            -webkit-background-clip: text; -webkit-text-fill-color: transparent;\
            color: rgb(0, 128, 0)'>Hi <span>there</span></div>\
          <div style='background-color: rgb(255, 0, 255); background-clip: text'>X</div>\
          <div style='color: rgb(0, 128, 0); -webkit-text-fill-color: rgb(0, 0, 128)'>fill</div>\
          <div style='color: rgb(0, 128, 0)'>plain</div>",
        &url,
        None,
    );
    let viewport = Size::new(400.0, 600.0);
    r.layout(viewport).unwrap();
    let list = r.paint(viewport, Point::ZERO);
    let items = &list.items;
    let masks: Vec<usize> = (0..items.len())
        .filter(|&i| matches!(items[i], Item::PushMask { .. }))
        .collect();
    assert_eq!(masks.len(), 2, "{items:?}");
    let group = |at: usize| {
        let content = (at..items.len())
            .find(|&i| matches!(items[i], Item::MaskContent))
            .unwrap();
        let end = (content..items.len())
            .find(|&i| matches!(items[i], Item::PopMask))
            .unwrap();
        (&items[at + 1..content], &items[content + 1..end], end)
    };
    let texts = |part: &[Item]| -> Vec<Color> {
        part.iter()
            .filter_map(|i| match i {
                Item::Text(t) => Some(t.color),
                _ => None,
            })
            .collect()
    };
    // The first box: both of its text runs (its span's too) are the mask, in opaque black; the
    // gradient is what the mask shows.
    let (mask, shown, end) = group(masks[0]);
    assert_eq!(texts(mask), [Color::BLACK, Color::BLACK], "{mask:?}");
    assert!(
        matches!(
            items[masks[0]],
            Item::PushMask {
                luminance: false,
                ..
            }
        ),
        "{:?}",
        items[masks[0]]
    );
    assert!(
        shown
            .iter()
            .any(|i| matches!(i, Item::LinearGradient { .. })),
        "{shown:?}"
    );
    assert!(texts(shown).is_empty());
    // The gradient is painted nowhere else.
    let gradients = items
        .iter()
        .filter(|i| matches!(i, Item::LinearGradient { .. }))
        .count();
    assert_eq!(gradients, 1);
    // Its glyphs are then filled with the transparent fill colour (nothing drawn), not `color`.
    let green = Color::rgb(0, 128, 0);
    let after = texts(&items[end..masks[1]]);
    assert!(after.iter().all(|c| c.is_transparent()), "{after:?}");
    // The second: the background color alone, clipped to the text.
    let magenta = Color::rgb(255, 0, 255);
    let (mask, shown, _) = group(masks[1]);
    assert_eq!(texts(mask), [Color::BLACK]);
    let fills = |part: &[Item]| {
        part.iter().any(|i| {
            matches!(i, Item::FillRect { color, .. } | Item::FillRoundedRect { color, .. }
                if *color == magenta)
        })
    };
    assert!(fills(shown), "{shown:?}");
    assert!(!fills(&items[..masks[1]]));
    // Boxes without it: no mask; glyphs in the fill colour where it is set, else in `color`.
    let rest = texts(&items[masks[1]..]);
    assert_eq!(
        rest[rest.len() - 2..],
        [Color::rgb(0, 0, 128), green],
        "{rest:?}"
    );
}
