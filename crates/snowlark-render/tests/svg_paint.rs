// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inline SVG recorded as display list paint operations (`snowlark_render::svg_paint`): what is
//! recorded paints the pixels the SVG says, and what is not falls back to the image.

use std::sync::Arc;
use std::time::Duration;

use snowlark_net::{NetConfig, NetThread, Url};
use snowlark_paint::{Color, Item, Pixmap, Point, Size, TextEngine};
use snowlark_render::Renderer;
use snowlark_style::DeviceConfig;

const VIEWPORT: Size = Size {
    width: 120.0,
    height: 100.0,
};

fn render(net: &NetThread, body: &str) -> Renderer {
    let html = format!(
        "<!DOCTYPE html><style>body {{ margin: 0 }} svg {{ display: block }}</style>{body}"
    );
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            scripting: false,
            ..Default::default()
        },
    );
    let url = Url::parse("about:blank").unwrap();
    r.load_html(html.as_bytes(), &url, None);
    r.restyle();
    assert!(r.render_until_settled(Duration::from_secs(20)).complete);
    r.layout(VIEWPORT).unwrap();
    r
}

fn svg_of(r: &Renderer) -> snowlark_dom::NodeId {
    let dom = r.dom().unwrap();
    dom.descendants(r.document().unwrap())
        .find(|&n| dom.element(n).is_some_and(|e| &*e.name().local == "svg"))
        .unwrap()
}

fn pixels(r: &mut Renderer) -> Pixmap {
    r.rasterize(VIEWPORT, Point::ZERO, 1.0).unwrap()
}

fn close(a: Color, b: Color) -> bool {
    let d = |x: u8, y: u8| (x as i32 - y as i32).abs() <= 2;
    d(a.r, b.r) && d(a.g, b.g) && d(a.b, b.b) && d(a.a, b.a)
}

#[track_caller]
fn assert_pixel(px: &Pixmap, x: u32, y: u32, want: Color) {
    let got = px.pixel(x, y);
    assert!(close(got, want), "({x}, {y}) is {got:?}, want {want:?}");
}

const RED: Color = Color::rgb(255, 0, 0);
const GREEN: Color = Color::rgb(0, 128, 0);
const BLUE: Color = Color::rgb(0, 0, 255);
const WHITE: Color = Color::rgb(255, 255, 255);

#[test]
fn shapes_paint_as_path_items() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100 viewBox='0 0 60 50'>
           <title>a chart</title><style>.s { stroke: blue }</style>
           <rect x=0 y=0 width=10 height=10 fill=red />
           <g transform='translate(20 0)'><circle cx=5 cy=5 r=5 fill=green /></g>
           <path d='M40 0 h10 v10 h-10 z' fill=none class=s stroke-width=2 />
           <line x1=0 y1=30 x2=60 y2=30 stroke=red stroke-width=4 stroke-dasharray='5 5' />
           <rect x=0 y=40 width=10 height=10 fill=green opacity=0 />
           <rect x=20 y=40 width=10 height=10 fill=green visibility=hidden />
         </svg>",
    );
    let list = r.paint(VIEWPORT, Point::ZERO);
    assert!(
        !list.items.iter().any(|i| matches!(i, Item::Image { .. })),
        "{:?}",
        r.inline_svg_unsupported(svg_of(&r))
    );
    let fills = list
        .items
        .iter()
        .filter(|i| matches!(i, Item::FillPath { .. }))
        .count();
    let strokes = list
        .items
        .iter()
        .filter(|i| matches!(i, Item::StrokePath { .. }))
        .count();
    assert_eq!((fills, strokes), (2, 2));
    assert_eq!(r.stats().svgs_recorded, 1);
    // painted again: the recording is reused
    r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.stats().svgs_recorded, 1);
    // (the viewBox doubles everything)
    let px = pixels(&mut r);
    assert_pixel(&px, 10, 10, RED);
    assert_pixel(&px, 50, 10, GREEN);
    assert_pixel(&px, 30, 10, WHITE);
    // the stroke along the square's edge, its inside empty
    assert_pixel(&px, 80, 10, BLUE);
    assert_pixel(&px, 90, 10, WHITE);
    // dashes 10 on, 10 off at this scale
    assert_pixel(&px, 5, 60, RED);
    assert_pixel(&px, 15, 60, WHITE);
    // opacity 0 and hidden paint nothing
    assert_pixel(&px, 10, 90, WHITE);
    assert_pixel(&px, 50, 90, WHITE);
}

#[test]
fn group_opacity_composites_once_and_clip_paths_clip() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100>
           <defs><clipPath id=c><rect x=0 y=0 width=20 height=100 /></clipPath></defs>
           <g opacity=0.5>
             <rect x=0 y=0 width=30 height=20 fill=blue />
             <rect x=10 y=0 width=30 height=20 fill=blue />
           </g>
           <rect x=0 y=40 width=60 height=20 fill=red clip-path='url(#c)' />
           <rect x=0 y=70 width=60 height=20 fill=blue stroke=red opacity=0.5 />
         </svg>",
    );
    let px = pixels(&mut r);
    assert_eq!(r.inline_svg_unsupported(svg_of(&r)), None);
    assert_eq!(r.stats().svgs_recorded, 1);
    // (the overlap is no darker: the group is composited as one)
    let half = Color::rgb(128, 128, 255);
    assert_pixel(&px, 5, 10, half);
    assert_pixel(&px, 15, 10, half);
    assert_pixel(&px, 35, 10, half);
    assert_pixel(&px, 10, 50, RED);
    assert_pixel(&px, 30, 50, WHITE);
    assert_pixel(&px, 30, 80, half);
}

/// CSS Masking 1 §6: a clip path of any shapes, each under its own transform, through the
/// `clipPath`'s transform (in the user space) and `objectBoundingBox` units; several shapes clip as
/// their union.
#[test]
fn clip_paths_of_any_shapes_clip() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100>
           <defs>
             <clipPath id=t><rect width=10 height=20 transform='translate(10 0)' /></clipPath>
             <clipPath id=c><circle cx=50 cy=10 r=10 /></clipPath>
             <clipPath id=u><rect x=0 y=40 width=10 height=20 /><g><rect x=10 y=40 width=60 height=20 /></g>
               <rect x=30 y=40 width=10 height=20 /></clipPath>
             <clipPath id=b clipPathUnits=objectBoundingBox transform='translate(20 0)'>
               <rect width=0.25 height=1 /></clipPath>
           </defs>
           <rect x=0 y=0 width=40 height=20 fill=red clip-path='url(#t)' />
           <rect x=40 y=0 width=20 height=20 fill=blue clip-path='url(#c)' />
           <rect x=0 y=40 width=60 height=20 fill=green clip-path='url(#u)' />
           <rect x=60 y=70 width=40 height=20 fill=blue clip-path='url(#b)' />
         </svg>",
    );
    let list = r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.inline_svg_unsupported(svg_of(&r)), None);
    assert!(list
        .items
        .iter()
        .any(|i| matches!(i, Item::PushClipPath { .. })));
    assert!(list
        .items
        .iter()
        .any(|i| matches!(i, Item::PushMask { .. })));
    let px = pixels(&mut r);
    // the translated rectangle: x 10 to 20
    assert_pixel(&px, 5, 10, WHITE);
    assert_pixel(&px, 15, 10, RED);
    assert_pixel(&px, 25, 10, WHITE);
    // the circle: its center in, the square's corner out
    assert_pixel(&px, 50, 10, BLUE);
    assert_pixel(&px, 41, 1, WHITE);
    // the union of two rectangles (a `g` in a clip path draws nothing)
    assert_pixel(&px, 5, 50, GREEN);
    assert_pixel(&px, 20, 50, WHITE);
    assert_pixel(&px, 35, 50, GREEN);
    assert_pixel(&px, 50, 50, WHITE);
    // the box's units: 60 + [0, 0.25] × 40, moved 20 in the user space
    assert_pixel(&px, 75, 80, WHITE);
    assert_pixel(&px, 85, 80, BLUE);
    assert_pixel(&px, 95, 80, WHITE);
}

#[track_caller]
fn assert_like(px: &Pixmap, x: u32, y: u32, what: &str, like: impl Fn(Color) -> bool) {
    let got = px.pixel(x, y);
    assert!(like(got), "({x}, {y}) is {got:?}, want {what}");
}

/// SVG 2 §14.2: linear and radial gradients paint fills and strokes, in the box's units or the
/// user space, with stops and attributes from the gradient `href` names, and reflected.
#[test]
fn gradients_paint_fills_and_strokes() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100>
           <linearGradient id=h><stop offset=0 stop-color=red /><stop offset=1 stop-color=blue /></linearGradient>
           <linearGradient id=v href='#h' x2=0 y2=1 />
           <linearGradient id=u gradientUnits=userSpaceOnUse x1=0 x2=10 spreadMethod=reflect href='#h' />
           <radialGradient id=r><stop offset=0 stop-color=white /><stop offset=1 stop-color=green /></radialGradient>
           <rect x=0 y=0 width=40 height=20 fill='url(#h)' />
           <rect x=50 y=0 width=20 height=40 fill='url(#v)' />
           <rect x=0 y=30 width=40 height=10 fill='url(#u)' />
           <rect x=80 y=0 width=40 height=40 fill='url(#r)' />
           <rect x=10 y=60 width=40 height=20 fill=none stroke='url(#h)' stroke-width=10 />
         </svg>",
    );
    let px = pixels(&mut r);
    assert_eq!(r.inline_svg_unsupported(svg_of(&r)), None);
    let red = |c: Color| c.r > 200 && c.b < 60;
    let blue = |c: Color| c.b > 200 && c.r < 60;
    // across the box, red to blue
    assert_like(&px, 1, 10, "red", red);
    assert_like(&px, 20, 10, "purple", |c| {
        (100..156).contains(&c.r) && (100..156).contains(&c.b)
    });
    assert_like(&px, 38, 10, "blue", blue);
    // down the box, with the stops of #h
    assert_like(&px, 60, 1, "red", red);
    assert_like(&px, 60, 38, "blue", blue);
    // 10 user units, then mirrored
    assert_like(&px, 9, 35, "blue", blue);
    assert_like(&px, 19, 35, "red", red);
    assert_like(&px, 29, 35, "blue", blue);
    // white at the center, green beyond the circle
    assert_like(&px, 100, 20, "white", |c| {
        c.r > 230 && c.g > 230 && c.b > 230
    });
    assert_like(&px, 81, 1, "green", |c| close(c, GREEN));
    // the stroke over the path's box: red at its left side, blue at its right, nothing inside
    assert_like(&px, 8, 70, "red", red);
    assert_like(&px, 52, 70, "blue", blue);
    assert_pixel(&px, 30, 70, WHITE);
}

#[test]
fn text_is_shaped_and_anchored() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100 font-size=20 font-family=Arial>
           <text x=110 y=40 text-anchor=end fill=blue>MM</text>
           <text x=10 y=80>M<tspan fill=red dy=-10>M</tspan></text>
         </svg>",
    );
    let list = r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.inline_svg_unsupported(svg_of(&r)), None);
    let runs: Vec<_> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Text(t) => Some(*t),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 3);
    // the end-anchored run ends at x = 110 (in the list, under the svg's transform at 0, 0)
    let end = runs[0].origin.x + list.run_width(&runs[0]);
    assert!((end - 110.0).abs() < 0.01, "{end}");
    assert_eq!(runs[0].color, BLUE);
    // the tspan follows the first glyph, 10 px higher, in its own fill
    let after = runs[1].origin.x + list.run_width(&runs[1]);
    assert!((runs[2].origin.x - after).abs() < 0.01);
    assert_eq!((runs[1].origin.y, runs[2].origin.y), (80.0, 70.0));
    assert_eq!(runs[2].color, RED);
    assert_eq!(runs[1].color, Color::BLACK);
}

#[test]
fn what_is_not_recorded_is_drawn_as_the_image() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100>
           <pattern id=g width=20 height=20 patternUnits=userSpaceOnUse><rect width=20 height=20 fill=red /></pattern>
           <rect x=0 y=0 width=20 height=20 fill='url(#g)' />
         </svg>",
    );
    let list = r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.inline_svg_unsupported(svg_of(&r)), Some("paint server"));
    assert_eq!(r.stats().svgs_not_recorded, 1);
    assert!(list.items.iter().any(|i| matches!(i, Item::Image { .. })));
    let px = pixels(&mut r);
    assert_pixel(&px, 10, 10, RED);
    assert_pixel(&px, 30, 10, WHITE);
}

#[test]
fn effect_attributes_the_style_engine_drops_fall_back() {
    // (`filter: url()` does not parse in the style engine; the image path reads the attribute)
    let net = NetThread::start(NetConfig::default()).unwrap();
    let r = render(
        &net,
        "<svg width=120 height=100>
           <filter id=f><feFlood flood-color=green /></filter>
           <rect width=20 height=20 fill=red filter='url(#f)' />
         </svg>",
    );
    let mut r = r;
    r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(
        r.inline_svg_unsupported(svg_of(&r)),
        Some("effect attribute")
    );
}

#[test]
fn a_zero_view_box_disables_rendering() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100 viewBox='0 0 0 100'><rect width=100 height=100 fill=red /></svg>",
    );
    let list = r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.inline_svg_unsupported(svg_of(&r)), None);
    assert!(!list
        .items
        .iter()
        .any(|i| matches!(i, Item::FillPath { .. } | Item::Image { .. })));
    let px = pixels(&mut r);
    assert_pixel(&px, 50, 50, WHITE);
}

#[test]
fn equal_svgs_share_one_recording() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let icon = "<svg width=30 height=30><rect width=30 height=30 fill=currentColor /></svg>";
    let mut r = render(
        &net,
        &format!("<div style='color: red'>{icon}{icon}</div><div style='color: blue'>{icon}</div>"),
    );
    let list = r.paint(VIEWPORT, Point::ZERO);
    assert!(!list.items.iter().any(|i| matches!(i, Item::Image { .. })));
    // the two red icons are one recording; the blue one's markup carries its own color
    assert_eq!(r.stats().svgs_recorded, 2);
    let px = pixels(&mut r);
    assert_pixel(&px, 15, 15, RED);
    assert_pixel(&px, 15, 45, RED);
    assert_pixel(&px, 15, 75, BLUE);
}

#[test]
fn lengths_read_only_the_font_metrics_their_units_need() {
    use snowlark_layout::svg::LengthUnit;
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100 font-size=20 font-family=Arial>
           <text x=10 y=50 dy=0.5em>MM</text>
         </svg>",
    );
    // (the lengths resolve as they did: dy 0.5em of a 20px font is 10)
    let list = r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.inline_svg_unsupported(svg_of(&r)), None);
    let run = list
        .items
        .iter()
        .find_map(|i| match i {
            Item::Text(t) => Some(*t),
            _ => None,
        })
        .unwrap();
    assert_eq!((run.origin.x, run.origin.y), (10.0, 60.0));
    let text = {
        let dom = r.dom().unwrap();
        dom.descendants(r.document().unwrap())
            .find(|&n| dom.element(n).is_some_and(|e| &*e.name().local == "text"))
            .unwrap()
    };
    let q = r.svg_query().unwrap();
    let full = q.length_context(text);
    assert!(full.ch.is_some() && full.ic.is_some() && full.viewport.is_some());
    // user units and `em` read the font size only: no metrics, no `0` or `水` shaped
    let em = q.length_context_for(text, [&LengthUnit::Number, &LengthUnit::Em]);
    assert_eq!(em.font_size, Some(20.0));
    assert_eq!(
        (em.x_height, em.cap_height, em.ch, em.ic, em.viewport),
        (None, None, None, None, None)
    );
    let ch = q.length_context_for(text, [&LengthUnit::Other("ch".into())]);
    assert_eq!((ch.ch, ch.ic), (full.ch, None));
    let ex = q.length_context_for(text, [&LengthUnit::Ex]);
    assert_eq!((ex.x_height, ex.ch), (full.x_height, None));
    let pct = q.length_context_for(text, [&LengthUnit::Percent]);
    assert_eq!((pct.viewport, pct.font_size), (full.viewport, None));
}

/// Applies `f` to the DOM as a script would, then styles and lays the page out again.
fn change(r: &mut Renderer, f: impl FnOnce(&mut snowlark_dom::Dom)) {
    let mut dom = r.dom_mut().unwrap();
    dom.set_reactions_enabled(true);
    f(&mut dom);
    let reactions = dom.take_reactions();
    drop(dom);
    r.dom_changed(&reactions);
    r.restyle();
    r.layout(VIEWPORT).unwrap();
}

fn by_id(r: &Renderer, id: &str) -> snowlark_dom::NodeId {
    let dom = r.dom().unwrap();
    dom.descendants(r.document().unwrap())
        .find(|&n| dom.element(n).is_some() && dom.get_attribute(n, "id") == Some(id))
        .unwrap()
}

#[test]
fn a_recorded_svg_is_never_written_out() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100 viewBox='0 0 60 50'><rect id=r width=10 height=10 fill=red /></svg>",
    );
    r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.stats().svgs_serialized, 0);
    assert_eq!(r.stats().svgs_recorded, 1);
    assert_pixel(&pixels(&mut r), 10, 10, RED);
    // a change inside it is recorded again, still without markup
    let rect = by_id(&r, "r");
    change(&mut r, |dom| {
        dom.set_attribute(rect, "fill", "blue").unwrap()
    });
    r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.stats().svgs_serialized, 0);
    assert_eq!(r.stats().svgs_recorded, 2);
    assert_pixel(&pixels(&mut r), 10, 10, BLUE);
    // the natural size comes from the root's attributes
    let svg = svg_of(&r);
    change(&mut r, |dom| dom.set_attribute(svg, "width", "60").unwrap());
    // (60 x 100 shows the 60 x 50 viewBox at scale 1, centred: the rect at 0..10, 25..35)
    let px = pixels(&mut r);
    assert_pixel(&px, 5, 30, BLUE);
    assert_pixel(&px, 15, 30, WHITE);
    assert_pixel(&px, 70, 30, WHITE);
}

#[test]
fn an_unchanged_svg_is_not_recorded_again() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=50><rect width=10 height=10 fill=red /></svg>
         <svg width=120 height=50><g id=sprite><rect id=r width=10 height=10 fill=red /></g></svg>",
    );
    r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.stats().svgs_recorded, 2);
    // (a change inside an element with an id may change what other svgs refer to: every svg is
    // looked at again, and only the changed one is recorded)
    let rect = by_id(&r, "r");
    change(&mut r, |dom| {
        dom.set_attribute(rect, "fill", "green").unwrap()
    });
    r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.stats().svgs_recorded, 3);
    assert_eq!(r.stats().svgs_serialized, 0);
    let px = pixels(&mut r);
    assert_pixel(&px, 5, 5, RED);
    assert_pixel(&px, 5, 55, GREEN);
}

#[test]
fn an_svg_that_fetches_is_written_out() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100><image href='x.png' width=10 height=10 /></svg>",
    );
    assert_eq!(r.stats().svgs_serialized, 1);
    r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.inline_svg_unsupported(svg_of(&r)), Some("image"));
}

#[test]
fn an_svg_its_fingerprint_takes_and_paint_cannot_record_gets_its_markup() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = render(
        &net,
        "<svg width=120 height=100><rect width=20 height=20 fill=red style='vector-effect: non-scaling-stroke' /></svg>",
    );
    assert_eq!(r.stats().svgs_serialized, 0);
    let list = r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.inline_svg_unsupported(svg_of(&r)), Some("vector-effect"));
    assert_eq!(r.stats().svgs_serialized, 1);
    assert!(list.items.iter().any(|i| matches!(i, Item::Image { .. })));
    let px = pixels(&mut r);
    assert_pixel(&px, 10, 10, RED);
    assert_pixel(&px, 30, 10, WHITE);
}

#[test]
fn icons_with_equal_styles_share_one_recording() {
    // (the two divs match different rules, so their icons' styles are equal, not shared)
    let net = NetThread::start(NetConfig::default()).unwrap();
    let icon = "<svg width=30 height=30><rect width=30 height=30 fill=currentColor /></svg>";
    let mut r = render(
        &net,
        &format!(
            "<style>.a {{ color: red }} .b {{ color: rgb(255, 0, 0) }}</style>
             <div class=a>{icon}</div><div class=b>{icon}</div>"
        ),
    );
    r.paint(VIEWPORT, Point::ZERO);
    assert_eq!(r.stats().svgs_recorded, 1);
    assert_eq!(r.stats().svgs_serialized, 0);
    let px = pixels(&mut r);
    assert_pixel(&px, 15, 15, RED);
    assert_pixel(&px, 15, 45, RED);
}
