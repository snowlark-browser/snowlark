// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS `filter` on boxes (Filter Effects 1 §10): the functions reach the raster from the computed
//! style, a blur shows past the border box, a drop shadow at its offset.

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

fn close(a: Color, b: Color, tolerance: i32) -> bool {
    (a.r as i32 - b.r as i32).abs() <= tolerance
        && (a.g as i32 - b.g as i32).abs() <= tolerance
        && (a.b as i32 - b.b as i32).abs() <= tolerance
}

#[test]
fn filter_functions_show_in_the_raster() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; background: white }\
         div { position: absolute; top: 40px; width: 40px; height: 40px; background: red }\
         #a { left: 40px; filter: blur(4px) }\
         #b { left: 140px; filter: grayscale(100%) }\
         #c { left: 240px; filter: drop-shadow(20px 0 0 blue) }\
         #d { left: 340px; filter: invert(1) opacity(50%) }</style>\
         <div id=a></div><div id=b></div><div id=c></div><div id=d></div>",
    );
    assert_eq!(
        eval(
            &mut page,
            "[getComputedStyle(a).filter, getComputedStyle(c).filter].join('|')"
        ),
        "blur(4px)|drop-shadow(rgb(0, 0, 255) 20px 0px 0px)"
    );
    let px = pixels(
        &mut page,
        &[
            (60, 60),
            (34, 60),
            (20, 60),
            (160, 60),
            (260, 60),
            (290, 60),
            (360, 60),
        ],
    );
    // the blurred box: red inside, a tint 6 px outside, white further out
    assert!(
        close(px[0], Color::rgb(255, 0, 0), 2),
        "blur center {:?}",
        px[0]
    );
    assert!(
        px[1].r == 255 && px[1].g > 150 && px[1].g < 250,
        "blur outside {:?}",
        px[1]
    );
    assert_eq!(px[2], Color::WHITE, "far from the blur");
    // grayscale(100%) of red is its luminance
    assert!(
        close(px[3], Color::rgb(54, 54, 54), 2),
        "grayscale {:?}",
        px[3]
    );
    // the drop shadow: the box on top, blue 20 px to its right
    assert_eq!(px[4], Color::rgb(255, 0, 0), "shadowed box");
    assert!(close(px[5], Color::rgb(0, 0, 255), 2), "shadow {:?}", px[5]);
    // invert then opacity: cyan at half over white
    assert!(
        close(px[6], Color::rgb(128, 255, 255), 2),
        "invert opacity {:?}",
        px[6]
    );
}

/// A filtered box is a stacking context and the containing block of its absolutely positioned
/// descendants (Filter Effects 1 §5).
#[test]
fn a_filter_makes_a_stacking_context_and_a_containing_block() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 }\
         #f { position: relative; left: 50px; top: 50px; width: 100px; height: 100px;\
              filter: grayscale(1) }\
         #abs { position: absolute; left: 0; top: 0; width: 10px; height: 10px }\
         #under { position: absolute; left: 60px; top: 60px; width: 10px; height: 10px;\
                  z-index: -1 }</style>\
         <div id=f><div id=abs></div><div id=under></div></div>",
    );
    // `abs` is placed against the filtered box, not the viewport; a z-index: -1 child stays
    // inside the filtered box's stacking context, over the body
    assert_eq!(
        eval(
            &mut page,
            "const r = abs.getBoundingClientRect();\
             [r.left, r.top, document.elementFromPoint(115, 115).id].join()"
        ),
        "50,50,under"
    );
}

/// The window rasters in 512 px tiles and culls each item to the tile, in frame pixels: a
/// filtered inline `<svg>` icon in a round button (the player controls of a video site) in the
/// second tile must survive that as it does headless.
#[test]
fn a_filtered_svg_icon_survives_tile_culling() {
    use snowlark_paint::{Driver, Gpu, Sources, TileRasterizer, Viewport};
    let net = NetThread::start(NetConfig::default()).unwrap();
    let page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; background: #888 }\
         .b { border: 0; background: rgba(0,0,0,.6); display: inline-block; width: 56px;\
              height: 56px; padding: 0; overflow: hidden; position: relative; border-radius: 50% }\
         .b svg { filter: drop-shadow(0 0 1px rgba(0,0,0,.8)) } .f { fill: #fff }\
         .c { position: absolute; left: 600px; top: 20px; height: 56px }</style>\
         <div class=c><button class=b><svg height='100%' viewBox='0 0 36 36' width='100%'>\
         <path class=f d='M 12,26 18.5,22 18.5,14 12,10 z M 18.5,22 25,18 25,14 18.5,10 z'/>\
         </svg></button></div>",
    );
    let (list, text, images) = page
        .engine()
        .with_renderer(|r, _| {
            (
                r.paint(Size::new(1100.0, 200.0), Point::ZERO),
                r.text().clone(),
                r.images().clone(),
            )
        })
        .unwrap();
    let gpu = Gpu::with_driver(Driver::Warp).unwrap();
    let mut tiles = TileRasterizer::new(gpu).unwrap();
    let view = Viewport {
        width: 1100,
        height: 200,
        scale: 1.0,
        background: Color::rgb(136, 136, 136),
    };
    let sources = Sources {
        text: &*text,
        images: &*images,
    };
    let frame = tiles.raster(&list, sources, &view).unwrap();
    let pm = tiles.read_back(&frame).unwrap();
    // (the icon's left triangle is around (624, 48) of the viewport, in the second tile)
    assert_eq!(frame.tiles.len(), 3);
    let icon = pm.pixel(624, 48);
    let ring = pm.pixel(604, 48);
    assert!(icon.r > 200 && icon.g > 200, "icon {icon:?}");
    assert!(ring.r < 90, "button {ring:?}");
}

/// `filter: url(#id)` (Filter Effects 1 §5-§9): the `<filter>`'s primitives apply over its
/// region (the border box with 10% around it by default), a url that names no `<filter>` makes
/// the whole list ignored, and a change to the `<filter>` shows.
#[test]
fn a_url_applies_the_svg_filter_it_names() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; background: white }\
         div { position: absolute; top: 40px; width: 50px; height: 50px; background: red }\
         #a { left: 40px; filter: url(#flood) }\
         #b { left: 140px; filter: url(#missing) invert(1) }\
         #c { left: 240px; filter: url(#shift) }</style>\
         <svg width=0 height=0><filter id=flood><feFlood flood-color=green /></filter>\
         <filter id=shift x=0 y=0 width=2 height=1><feOffset dx=25 /></filter></svg>\
         <div id=a></div><div id=b></div><div id=c></div>",
    );
    assert_eq!(
        eval(&mut page, "getComputedStyle(a).filter"),
        "url(\"http://example.test/#flood\")"
    );
    let px = pixels(
        &mut page,
        &[
            (65, 65),
            (37, 65),
            (30, 65),
            (165, 65),
            (250, 65),
            (280, 65),
            (310, 65),
        ],
    );
    let green = Color::rgb(0, 128, 0);
    let (red, white) = (Color::rgb(255, 0, 0), Color::rgb(255, 255, 255));
    // the flood over the box and 5 px (10%) around it, not further
    assert!(close(px[0], green, 2), "{:?}", px[0]);
    assert!(close(px[1], green, 2), "{:?}", px[1]);
    assert!(close(px[2], white, 0), "{:?}", px[2]);
    // a missing filter: the whole list is ignored, the box as it is
    assert!(close(px[3], red, 0), "{:?}", px[3]);
    // the offset: 25 px to the right, within a region twice the box's width
    assert!(close(px[4], white, 0), "{:?}", px[4]);
    assert!(close(px[5], red, 0), "{:?}", px[5]);
    assert!(close(px[6], red, 0), "{:?}", px[6]);
    // a change to the filter's primitive shows
    eval(
        &mut page,
        "document.querySelector('feFlood').setAttribute('flood-color', 'blue'); 1",
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    let px = pixels(&mut page, &[(65, 65)]);
    assert!(close(px[0], Color::rgb(0, 0, 255), 2), "{:?}", px[0]);
}

/// Filter Effects 1 §14 and CSS Color 4 §12: a shadow color extrapolated far past the end saturates
/// (each channel clamped), at a progress where `1 - p` and `p` do not sum to 1 in single precision.
#[test]
fn a_shadow_color_extrapolated_far_past_the_end_saturates() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><div id=d></div>");
    let out = eval(
        &mut page,
        "const b = (8 * 2147483648 - 1) / 6;\
         const a = d.animate([\
           { filter: 'drop-shadow(20px 10px blue)', color: 'blue' },\
           { filter: 'drop-shadow(20px 10px green)', color: 'green' }],\
           { duration: 100000, easing: `cubic-bezier(0, ${b}, 1, ${b})` });\
         a.pause(); a.currentTime = 50000;\
         getComputedStyle(d).filter + '|' + getComputedStyle(d).color",
    );
    assert_eq!(
        out,
        "drop-shadow(rgb(0, 255, 0) 20px 10px 0px)|rgb(0, 255, 0)"
    );
}

/// `feImage` (Filter Effects 1 §9.15) in a filter on a box: an element of the document drawn
/// from the subregion's corner (with the style it has where it is, in a `defs`), and an image
/// fetched by URL, stretched over the subregion with `preserveAspectRatio="none"` and centred
/// and fitted by default; the rest of the subregion is transparent.
#[test]
fn an_image_primitive_draws_an_element_or_an_image() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let image = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='10' \
                 height='20'%3E%3Crect width='10' height='20' fill='blue'/%3E%3C/svg%3E";
    let mut page = rendered(
        &net,
        &format!(
            "<!doctype html><style>body {{ margin: 0; background: white }}\
             div {{ position: absolute; top: 40px; width: 50px; height: 50px; background: red }}\
             #a {{ left: 40px; filter: url(#element) }}\
             #b {{ left: 140px; filter: url(#stretched) }}\
             #c {{ left: 240px; filter: url(#fitted) }}</style>\
             <svg width=0 height=0>\
             <filter id=element x=0 y=0 width=1 height=1><feImage href='#dot'/></filter>\
             <filter id=stretched x=0 y=0 width=1 height=1>\
             <feImage href=\"{image}\" preserveAspectRatio=none /></filter>\
             <filter id=fitted x=0 y=0 width=1 height=1><feImage href=\"{image}\"/></filter>\
             <defs><g fill=green><rect id=dot x=10 y=5 width=20 height=20 /></g></defs></svg>\
             <div id=a></div><div id=b></div><div id=c></div>"
        ),
    );
    let px = pixels(
        &mut page,
        &[
            (60, 55),
            (45, 85),
            (145, 45),
            (185, 85),
            (265, 65),
            (245, 65),
            (285, 65),
        ],
    );
    let (green, blue) = (Color::rgb(0, 128, 0), Color::rgb(0, 0, 255));
    let white = Color::rgb(255, 255, 255);
    // the rect at (10, 5) from the box's corner, green from its group; nothing else
    assert!(close(px[0], green, 2), "{:?}", px[0]);
    assert!(close(px[1], white, 0), "{:?}", px[1]);
    // the image over the whole box
    assert!(close(px[2], blue, 2), "{:?}", px[2]);
    assert!(close(px[3], blue, 2), "{:?}", px[3]);
    // fitted: 25 px wide in the middle of the box, nothing at its sides
    assert!(close(px[4], blue, 2), "{:?}", px[4]);
    assert!(close(px[5], white, 0), "{:?}", px[5]);
    assert!(close(px[6], white, 0), "{:?}", px[6]);
}

/// Filter and primitive lengths are CSS lengths (SVG 2 §4.6.2): math functions and units, as the
/// element computes them. In bounding box units a percentage and a number are both fractions of
/// the box (`calc(25% + 0.25px)` is half of it); in user space `em` is the element's font size and
/// a percentage the box's.
#[test]
fn filter_lengths_take_math_and_units() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; background: white }\
         div { position: absolute; left: 100px; width: 200px; height: 50px; background: red }\
         #a { top: 20px; filter: url(#half) } #b { top: 110px; filter: url(#user) }</style>\
         <svg width=0 height=0>\
         <filter id=half x=0 y=0 width='calc(25% + 0.25px)' height=1>\
         <feFlood flood-color=green /></filter>\
         <filter id=user x=0 y=0 width=1 height=1 style='font-size: 20px'>\
         <feFlood flood-color=green x='calc(1em + 5px)' width='max(20% + 10px, 1px)' /></filter>\
         </svg><div id=a></div><div id=b></div>",
    );
    let px = pixels(
        &mut page,
        &[(175, 45), (250, 45), (115, 135), (150, 135), (185, 135)],
    );
    let green = Color::rgb(0, 128, 0);
    let white = Color::rgb(255, 255, 255);
    // the region over the left half of the box
    assert!(close(px[0], green, 2), "{:?}", px[0]);
    assert!(close(px[1], white, 0), "{:?}", px[1]);
    // the flood from 25 px (1em + 5px) to 75 px (20% of 200 + 10px) from the box's corner
    assert!(close(px[2], white, 0), "{:?}", px[2]);
    assert!(close(px[3], green, 2), "{:?}", px[3]);
    assert!(close(px[4], white, 0), "{:?}", px[4]);
}

#[test]
fn text_shadows_paint_below_the_text_and_its_decorations() {
    // CSS Text Decoration 3 §4: each `text-shadow` paints the text and its decorations in the
    // shadow's color at its offset, blurred by half its blur radius as a standard deviation. The
    // shadows here are 100 px below the text, where nothing else paints: #a sharp and red, its
    // underline under no-break spaces (no glyph ink, so a red row there is the underline's shadow);
    // #b blurred and blue.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; background: white }\
         div { position: absolute; top: 0; font: 40px monospace; line-height: 40px; color: black }\
         #a { left: 0; text-shadow: 0 100px rgb(255, 0, 0) }\
         #u { left: 100px; text-decoration: underline; text-shadow: 0 100px rgb(255, 0, 0) }\
         #b { left: 250px; text-shadow: 0 100px 10px rgb(0, 0, 255) }</style>\
         <div id=a>MM</div><div id=u>&nbsp;&nbsp;&nbsp;</div><div id=b>MM</div>",
    );
    let region = |x0: u32, x1: u32| -> Vec<(u32, u32)> {
        (100..160)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .collect()
    };
    let red = |c: &Color| c.r > 200 && c.g < 60 && c.b < 60;
    let a = pixels(&mut page, &region(0, 90));
    assert!(a.iter().filter(|c| red(c)).count() > 100, "no shadow of #a");
    // the underline's shadow: a red line across most of the 72 px of three spaces
    let u = pixels(&mut page, &region(100, 200));
    let widest = u
        .chunks(100)
        .map(|row| row.iter().filter(|c| red(c)).count())
        .max()
        .unwrap();
    assert!(widest >= 60, "underline shadow rows reach {widest} px");
    // the blurred shadow: light blue around the glyphs, and no fully blue pixel at its center
    let b = pixels(&mut page, &region(240, 340));
    let light = b
        .iter()
        .filter(|c| c.b > 200 && c.r > 40 && c.r < 215 && c.r == c.g)
        .count();
    assert!(light > 200, "{light} light blue pixels");
    assert!(
        b.iter().all(|c| c.r > 20),
        "a blurred shadow is never fully blue"
    );
}
