// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Masking 1 §5: `clip-path` in hit testing. What a clipping path clips out takes no pointer
//! events, for the element and everything it paints.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn rendered(net: &NetThread, html: &str) -> Page {
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

/// The id of the element at each point, joined.
fn at(page: &mut Page, points: &[(i32, i32)]) -> String {
    let list = points
        .iter()
        .map(|(x, y)| format!("[{x},{y}]"))
        .collect::<Vec<_>>()
        .join(",");
    eval(
        page,
        &format!(
            "[{list}].map(([x, y]) => {{ const e = document.elementFromPoint(x, y);
               return e.id || e.localName; }}).join()"
        ),
    )
}

/// A box over the whole viewport, clipped to a strip, as tooltips keep the pointer on their way to
/// them: what is under it outside the strip gets the input, in shadow trees too.
#[test]
fn a_box_clipped_to_a_strip_leaves_what_is_under_it_to_the_pointer() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } #b { display: block; width: 100px; height: 40px }\
         #area { position: fixed; inset: 0; z-index: 5;\
           clip-path: polygon(0px 48px, 100px 48px, 120px 56px, 0px 56px) }</style>\
         <button id=b>Log In</button><div id=host></div><div id=area></div>\
         <script>\
         host.attachShadow({ mode: 'open' }).innerHTML =\
           `<input id=f style='display:block;width:300px;height:30px;margin:60px 0 0;padding:0;border:0'>`;\
         window.seen = [];\
         b.addEventListener('click', () => seen.push('button'));\
         area.addEventListener('click', () => seen.push('area'));\
         </script>",
    );
    assert_eq!(
        at(&mut page, &[(50, 20), (50, 52), (50, 110), (700, 300)]),
        "b,area,host,html"
    );
    let none = Modifiers::default();
    let click = |p: &mut Page, x: f32, y: f32| {
        let at = Point::new(x, y);
        p.pointer_moved(at, none);
        p.pointer_pressed(at, 0, none, 1);
        p.pointer_released(at, 0, none, 1);
    };
    click(&mut page, 50.0, 20.0);
    click(&mut page, 50.0, 52.0);
    assert_eq!(eval(&mut page, "seen.join()"), "button,area");
    click(&mut page, 50.0, 110.0);
    assert_eq!(
        eval(
            &mut page,
            "host.shadowRoot.activeElement && host.shadowRoot.activeElement.id"
        ),
        "f"
    );
}

/// CSS Shapes 1 §3: each basic shape against its reference box, percentages included.
#[test]
fn basic_shapes_clip_hit_testing() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } div { width: 100px; height: 100px }\
         .abs { position: absolute; top: 0 }\
         #diamond { left: 0; clip-path: polygon(50% 0, 100% 50%, 50% 100%, 0 50%) }\
         #circle { left: 100px; clip-path: circle(50px) }\
         #ellipse { left: 200px; clip-path: ellipse(50px 20px at 50% 50%) }\
         #inset { left: 300px; clip-path: inset(10px 20px 30% 40%) }\
         #round { left: 400px; clip-path: inset(0 round 50px) }\
         #content { left: 500px; width: 60px; height: 60px; padding: 20px;\
           clip-path: inset(0) content-box }\
         #hole { left: 600px; clip-path: polygon(evenodd, 0 0, 100% 0, 100% 100%, 0 100%, 0 0,\
           25% 25%, 75% 25%, 75% 75%, 25% 75%, 25% 25%) }</style>\
         <div class=abs id=diamond></div><div class=abs id=circle></div>\
         <div class=abs id=ellipse></div><div class=abs id=inset></div>\
         <div class=abs id=round></div><div class=abs id=content></div>\
         <div class=abs id=hole></div>",
    );
    assert_eq!(
        at(&mut page, &[(50, 50), (5, 5), (95, 95)]),
        "diamond,html,html"
    );
    assert_eq!(at(&mut page, &[(150, 50), (105, 5)]), "circle,html");
    assert_eq!(
        at(&mut page, &[(250, 50), (250, 25), (205, 50)]),
        "ellipse,html,ellipse"
    );
    // Inset: 40 px from the left (40%), 20 from the right, 10 from the top, 30 from the bottom.
    assert_eq!(
        at(&mut page, &[(345, 15), (335, 15), (385, 15), (345, 75)]),
        "inset,html,html,html"
    );
    assert_eq!(at(&mut page, &[(450, 50), (403, 3)]), "round,html");
    assert_eq!(at(&mut page, &[(550, 50), (510, 50)]), "content,html");
    assert_eq!(at(&mut page, &[(610, 50), (650, 50)]), "hole,html");
}

/// The clip applies to all the element paints, positioned descendants too, and follows transforms.
#[test]
fn a_clip_covers_descendants_and_follows_transforms() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 }\
         #c { position: absolute; left: 0; top: 0; width: 100px; height: 100px;\
           clip-path: inset(0 50px 0 0) }\
         #kid { position: absolute; left: 0; top: 0; width: 200px; height: 50px }\
         #t { position: absolute; left: 300px; top: 0; width: 100px; height: 100px;\
           transform: translateX(100px); clip-path: inset(0 50px 0 0) }</style>\
         <div id=c><div id=kid></div></div><div id=t></div>",
    );
    assert_eq!(
        at(&mut page, &[(25, 25), (75, 25), (150, 25)]),
        "kid,html,html"
    );
    assert_eq!(
        at(&mut page, &[(425, 50), (475, 50), (325, 50)]),
        "t,html,html"
    );
}

/// CSS Shapes 1 §3.1: `path()` in px from the reference box, its fill rule, and a curve; `shape()`
/// with percentages (CSS Shapes 2).
#[test]
fn paths_and_shapes_clip_hit_testing() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } div { position: absolute; top: 0;\
           width: 100px; height: 100px }\
         #p { left: 0; clip-path: path('M 0 0 L 100 0 L 0 100 Z') }\
         #eo { left: 100px; clip-path: path(evenodd, 'M 0 0 H 100 V 100 H 0 Z M 25 25 H 75 V 75 H 25 Z') }\
         #arc { left: 200px; clip-path: path('M 0 50 A 50 50 0 0 1 100 50 Z') }\
         #sh { left: 300px; clip-path: shape(from 0% 0%, hline to 100%, vline by 50%, hline to 0%, close) }\
         #curve { left: 400px; clip-path: path('M 0 100 Q 50 -100 100 100 Z') }</style>\
         <div id=p></div><div id=eo></div><div id=arc></div><div id=sh></div><div id=curve></div>",
    );
    assert_eq!(at(&mut page, &[(10, 10), (90, 90)]), "p,html");
    assert_eq!(at(&mut page, &[(110, 50), (150, 50)]), "eo,html");
    // (the upper half disc)
    assert_eq!(
        at(&mut page, &[(250, 10), (250, 60), (205, 5)]),
        "arc,html,html"
    );
    assert_eq!(at(&mut page, &[(350, 25), (350, 75)]), "sh,html");
    // (a parabola: at x = 50 it reaches y = 0, at x = 10 only y = 64)
    assert_eq!(
        at(&mut page, &[(450, 10), (410, 50), (410, 80)]),
        "curve,html,curve"
    );
}

/// CSS Masking 1 §5.1: `margin-box` is the reference box with its margins.
#[test]
fn margin_box_is_a_reference_box() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 }\
         #m { position: absolute; left: 20px; top: 20px; width: 100px; height: 100px;\
           margin: 10px 30px; clip-path: inset(0 0 0 25%) margin-box }</style><div id=m></div>",
    );
    // The margin box runs from 20 to 180: a quarter of it is 40, so the clip starts at 60 (from
    // the border box, 50 to 150, it would start at 75).
    assert_eq!(at(&mut page, &[(65, 50), (55, 50)]), "m,html");
}

/// CSS Masking 1 §6: a `<clipPath>` by `url()`, in user space and in bounding box units, in an `svg`
/// of no size; a reference to something else or to nothing does not clip.
#[test]
fn clip_path_elements_clip_hit_testing() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } div { position: absolute; top: 0;\
           width: 100px; height: 100px }\
         #u { left: 0; clip-path: url(#user) } #b { left: 100px; clip-path: url(#bbox) }\
         #n { left: 200px; clip-path: url(#missing) } #w { left: 300px; clip-path: url(#other) }\
         #e { left: 400px; clip-path: url(#empty) } #t { left: 500px; clip-path: url(#moved) }\
         </style><div id=u></div><div id=b></div><div id=n></div><div id=w></div>\
         <div id=e></div><div id=t></div>\
         <svg width=0 height=0 style='position:absolute'><defs>\
         <clipPath id=user><rect x=10 y=10 width=30 height=30 /></clipPath>\
         <clipPath id=bbox clipPathUnits=objectBoundingBox><circle cx=0.5 cy=0.5 r=0.25 /></clipPath>\
         <rect id=other width=1 height=1 /><clipPath id=empty></clipPath>\
         <clipPath id=moved><rect transform='translate(50 50)' width=20 height=20 /></clipPath>\
         </defs></svg>",
    );
    assert_eq!(at(&mut page, &[(20, 20), (60, 60)]), "u,html");
    assert_eq!(at(&mut page, &[(150, 50), (105, 50)]), "b,html");
    assert_eq!(at(&mut page, &[(250, 50), (350, 50)]), "n,w");
    assert_eq!(at(&mut page, &[(450, 50)]), "html");
    assert_eq!(at(&mut page, &[(560, 60), (510, 10)]), "t,html");
}

/// `transform` on what a `<clipPath>` holds and on the element itself, as an attribute and as a
/// style; percentages are of the nearest `svg`'s viewport (its `viewBox` here), as the reference
/// browser has them, not of the element using the clip.
#[test]
fn clip_path_elements_take_transforms_and_viewport_percentages() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } div { position: absolute; top: 0;\
           width: 100px; height: 100px }\
         #a { left: 0; clip-path: url(#attr) } #s { left: 100px; clip-path: url(#css) }\
         #p { left: 200px; width: 500px; clip-path: url(#percent) }</style>\
         <div id=a></div><div id=s></div><div id=p></div>\
         <svg width=0 height=0 viewBox='0 0 800 600' style='position:absolute'>\
         <clipPath id=attr><rect width=10 height=10 transform='rotate(45 5 5) scale(4)' /></clipPath>\
         <clipPath id=css style='transform: scale(50, 50)'><rect width=1 height=1 /></clipPath>\
         <clipPath id=percent><rect x='50%' width='25%' height=100 /></clipPath>\
         </svg>",
    );
    // (a square of 40 turned about (5, 5): (10, 25) is in it, its unturned corner (35, 5) is not)
    assert_eq!(at(&mut page, &[(10, 25), (35, 5)]), "a,html");
    assert_eq!(at(&mut page, &[(140, 40), (160, 60)]), "s,html");
    // 50% of 800 from the box's left: x from 400 to 600 past 200, so 600 to 800 is clipped too
    assert_eq!(at(&mut page, &[(650, 50), (300, 50)]), "p,html");
}

/// A `<clipPath>` that changes, and one that appears, reach what uses it, though nothing about that
/// element's style changed.
#[test]
fn a_clip_path_element_that_changes_clips_again() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } div { position: absolute; top: 0;\
           width: 100px; height: 100px }\
         #a { left: 0; clip-path: url(#c) } #b { left: 100px; clip-path: url(#later) }</style>\
         <div id=a></div><div id=b></div>\
         <svg width=0 height=0 id=s style='position:absolute'>\
         <clipPath id=c clipPathUnits=objectBoundingBox>\
         <circle id=r cx=0.5 cy=0.5 r=0.25 /></clipPath></svg>",
    );
    assert_eq!(at(&mut page, &[(50, 50), (5, 5), (150, 50)]), "a,html,b");
    eval(&mut page, "r.setAttribute('r', '1'); 'ok'");
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(at(&mut page, &[(5, 5)]), "a");
    eval(
        &mut page,
        "const n = 'http://www.w3.org/2000/svg';
         const c = document.createElementNS(n, 'clipPath'); c.id = 'later';
         const q = document.createElementNS(n, 'rect');
         q.setAttribute('width', '10'); q.setAttribute('height', '10');
         c.append(q); s.append(c); 'ok'",
    );
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(at(&mut page, &[(105, 5), (150, 50)]), "b,html");
}

/// Painting takes the same clip: the list carries a path clip around everything the element
/// paints, and an `inset()` with `round` is a path with curves (its corners arcs, its edges where
/// they are), not a snapped rectangle clip.
#[test]
fn the_page_paints_what_the_clip_leaves() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 }\
         #c { width: 100px; height: 100px; background: green;\
           clip-path: polygon(0 0, 50% 0, 0 50%) }\
         #r { width: 100px; height: 100px; background: blue;\
           clip-path: inset(10px round 20px) }</style><div id=c></div><div id=r></div>",
    );
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let opened = list
        .items
        .iter()
        .position(|i| matches!(i, snowlark_paint::Item::PushClipPath { .. }));
    let opened = opened.expect("a clip path in the list");
    let box_at = list
        .items
        .iter()
        .position(|i| matches!(i, snowlark_paint::Item::FillRect { color, .. } if color.g > 100 && color.r == 0));
    assert!(box_at.expect("the green box") > opened);
    let rounded = list.items.iter().find_map(|i| match i {
        snowlark_paint::Item::PushClipPath { path, bounds, .. } => {
            let curves = list
                .path(*path)
                .0
                .iter()
                .filter(|&&v| v == snowlark_paint::PathVerb::Cubic as u8)
                .count();
            (curves == 4).then_some(*bounds)
        }
        _ => None,
    });
    let bounds = rounded.expect("a rounded rectangle clip, as a path of four curves");
    assert_eq!(bounds, snowlark_paint::Rect::new(10.0, 110.0, 80.0, 80.0));
    assert!(!list
        .items
        .iter()
        .any(|i| matches!(i, snowlark_paint::Item::PushClipRoundedRect { .. })));
    assert_eq!(list.validate(), Ok(()));
}

/// An inline box over several lines: its first piece is the reference box for all of them (WPT's
/// `clip-path-inline-*`; CSS Masking 1 does not say), so a later line outside it is clipped away.
#[test]
fn an_inline_box_clips_to_its_first_piece() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0; font: 20px/20px monospace }\
         div { width: 200px } span { clip-path: inset(0) }</style>\
         <div><span id=s>first line of text that goes on to a second one</span></div>",
    );
    assert_eq!(at(&mut page, &[(5, 10), (5, 30)]), "s,div");
}

/// `zoom` scales `path()`, whose numbers are not lengths, and user space of a `<clipPath>`.
#[test]
fn zoom_scales_paths_and_user_space() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>body { margin: 0 } div { position: absolute; top: 0; zoom: 2;\
           width: 50px; height: 50px }\
         #p { left: 0; clip-path: path('M 0 0 H 25 V 25 H 0 Z') }\
         #u { left: 100px; clip-path: url(#c) }</style><div id=p></div><div id=u></div>\
         <svg width=0 height=0><clipPath id=c><rect width=25 height=25 /></clipPath></svg>",
    );
    assert_eq!(at(&mut page, &[(40, 40), (60, 60)]), "p,html");
    assert_eq!(at(&mut page, &[(240, 40), (260, 60)]), "u,html");
}

/// A `clip-path` on the root element clips the canvas: a background that reaches the whole page has
/// the same clip as everything else.
#[test]
fn the_root_clip_clips_the_canvas() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>html { background: green; clip-path: circle(50%) }\
         body { margin: 0; height: 100px }</style>",
    );
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let clip = list
        .items
        .iter()
        .position(|i| matches!(i, snowlark_paint::Item::PushClipPath { .. }))
        .expect("a clip");
    let background = list
        .items
        .iter()
        .position(|i| matches!(i, snowlark_paint::Item::FillRect { .. }))
        .expect("the canvas");
    assert!(clip < background);
}
