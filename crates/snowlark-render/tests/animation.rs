// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Render-blocking stylesheets (HTML "render-blocked", the `stylesheet` link type) and the document
//! timeline (CSS Transitions 1, CSS Animations 1) through the renderer, with stylesheets served late by
//! an in-process HTTP server.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread, Url};
use snowlark_paint::{Size, TextEngine};
use snowlark_render::{Renderer, TimelineClock};
use snowlark_style::DeviceConfig;

/// What the server answers for a path: a delay before answering, a content type and a body.
type Route = fn(&str) -> (Duration, &'static str, Vec<u8>);

/// An HTTP/1.1 server on 127.0.0.1 answering each request on its own thread, after the route's delay.
struct Server {
    port: u16,
}

impl Server {
    fn start(route: Route) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                std::thread::spawn(move || serve(stream, route));
            }
        });
        Server { port }
    }

    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}{path}", self.port)).unwrap()
    }
}

fn serve(mut stream: TcpStream, route: Route) {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head);
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let (delay, content_type, body) = route(&path);
    std::thread::sleep(delay);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&body);
}

const LATE: Duration = Duration::from_millis(300);

fn css(delay: Duration, text: &str) -> (Duration, &'static str, Vec<u8>) {
    (delay, "text/css", text.as_bytes().to_vec())
}

fn renderer(net: &NetThread) -> Renderer {
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (400.0, 300.0),
            scripting: false,
            ..Default::default()
        },
    );
    r.set_timeline_clock(TimelineClock::Manual(Duration::ZERO));
    r.set_render_blocking_timeout(Duration::from_secs(30));
    r
}

const VIEWPORT: Size = Size {
    width: 400.0,
    height: 300.0,
};

fn by_id(r: &Renderer, id: &str) -> snowlark_dom::NodeId {
    let dom = r.dom().unwrap();
    dom.descendants(r.document().unwrap())
        .find(|&n| dom.is_element(n) && dom.get_attribute(n, "id") == Some(id))
        .unwrap()
}

fn value(r: &Renderer, id: &str, property: &str) -> String {
    r.resolved_property(by_id(r, id), property).unwrap()
}

/// Pumps until no fetch is in flight, without rendering.
fn wait_for_fetches(r: &mut Renderer) {
    let start = Instant::now();
    while r.pending() > 0 {
        assert!(start.elapsed() < Duration::from_secs(20), "fetches stalled");
        r.wait(Duration::from_millis(20));
        r.pump();
    }
}

fn at(r: &mut Renderer, ms: u64) {
    r.set_timeline_clock(TimelineClock::Manual(Duration::from_millis(ms)));
    assert!(r.update_rendering(VIEWPORT).is_some());
}

fn routes(path: &str) -> (Duration, &'static str, Vec<u8>) {
    match path {
        "/head.css" => css(
            LATE,
            "#b { background-color: rgb(248, 249, 250); color: rgb(64, 66, 68);
                  transition: background-color 1s, color 1s }",
        ),
        "/body.css" => css(LATE, "#b { background-color: rgb(200, 0, 0) }"),
        "/slow.css" => css(Duration::from_secs(2), "#b { color: rgb(1, 2, 3) }"),
        "/import.css" => css(Duration::ZERO, "@import url(/imported.css);"),
        "/imported.css" => css(LATE, "#b { color: rgb(9, 9, 9) }"),
        p if p.starts_with("/w/load.php") => css(LATE, include_str!("fixtures/search-button.css")),
        _ => (Duration::ZERO, "text/plain", Vec::new()),
    }
}

#[test]
fn a_late_head_sheet_blocks_rendering_and_starts_no_transition() {
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let html = "<!DOCTYPE html><html><head><link rel=stylesheet href=/head.css></head>
                <body><div id=b>x</div></body></html>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    // HTML "update the rendering" skips the render-blocked document: no style, no frame.
    assert!(r.render_blocked());
    assert!(r.update_rendering(VIEWPORT).is_none());
    assert_eq!(r.timings().restyles, 0);

    wait_for_fetches(&mut r);
    assert!(!r.render_blocked());
    // The first rendering update styles the element for the first time, with the sheet: no
    // before-change style, so no transition (CSS Transitions 1 §3).
    at(&mut r, 400);
    assert_eq!(value(&r, "b", "background-color"), "rgb(248, 249, 250)");
    assert_eq!(value(&r, "b", "color"), "rgb(64, 66, 68)");
    assert!(!r.animating());
}

#[test]
fn a_sheet_arriving_after_the_first_rendering_starts_transitions() {
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    // A `<link>` in the body is not render-blocking: the document renders before its sheet arrives.
    let html = "<!DOCTYPE html><style>
                  #b { background-color: rgb(0, 0, 0); transition: background-color 1s linear }
                </style><body><div id=b>x</div><link rel=stylesheet href=/body.css></body>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    assert!(!r.render_blocked());
    at(&mut r, 0);
    assert_eq!(value(&r, "b", "background-color"), "rgb(0, 0, 0)");

    wait_for_fetches(&mut r);
    // The sheet's arrival is applied at the next rendering update, a style change event at 1 s.
    at(&mut r, 1000);
    assert_eq!(value(&r, "b", "background-color"), "rgb(0, 0, 0)");
    assert!(r.animating());
    at(&mut r, 1500);
    assert_eq!(value(&r, "b", "background-color"), "rgb(100, 0, 0)");
    at(&mut r, 2000);
    assert_eq!(value(&r, "b", "background-color"), "rgb(200, 0, 0)");
    assert!(!r.animating());
}

#[test]
fn the_render_blocking_timeout_bounds_the_wait() {
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    r.set_render_blocking_timeout(Duration::from_millis(100));
    let html = "<!DOCTYPE html><head><link rel=stylesheet href=/slow.css></head><div id=b>x</div>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    assert!(r.render_blocked());
    std::thread::sleep(Duration::from_millis(150));
    r.pump();
    assert_eq!(r.pending(), 1);
    assert!(!r.render_blocked());
    at(&mut r, 0);
    assert_eq!(value(&r, "b", "color"), "rgb(0, 0, 0)");
}

#[test]
fn imports_of_head_sheets_block_rendering() {
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    // The `<style>`'s `@import` is a critical subresource: rendering waits for it too.
    let html =
        "<!DOCTYPE html><head><style>@import url(/import.css);</style></head><div id=b>x</div>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    r.restyle();
    assert!(r.render_blocked());
    let start = Instant::now();
    while r.render_blocked() {
        assert!(start.elapsed() < Duration::from_secs(20), "never unblocked");
        r.wait(Duration::from_millis(20));
        r.pump();
        // A forced (script) restyle reveals the nested import and does not render.
        r.restyle();
    }
    at(&mut r, 0);
    assert_eq!(value(&r, "b", "color"), "rgb(9, 9, 9)");
}

#[test]
fn a_class_change_after_the_first_rendering_transitions() {
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let html = "<!DOCTYPE html><style>
                  #b { width: 100px; height: 10px; transition: width 400ms linear 100ms }
                  #b.wide { width: 300px }
                </style><div id=b></div>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    at(&mut r, 0);
    assert_eq!(value(&r, "b", "width"), "100px");

    let b = by_id(&r, "b");
    let mut dom = r.dom_mut().unwrap();
    dom.set_reactions_enabled(true);
    dom.set_attribute(b, "class", "wide").unwrap();
    let reactions = dom.take_reactions();
    drop(dom);
    r.dom_changed(&reactions);
    // The style change event of the frame at 1 s starts the transition: 100 ms delay, then 400 ms.
    at(&mut r, 1000);
    assert_eq!(value(&r, "b", "width"), "100px");
    assert!(r.animating());
    at(&mut r, 1050);
    assert_eq!(value(&r, "b", "width"), "100px");
    at(&mut r, 1200);
    assert_eq!(value(&r, "b", "width"), "150px");
    // Layout sees the animated value.
    assert_eq!(r.geometry(b).unwrap().border_box.width, 150.0);
    at(&mut r, 1400);
    assert_eq!(value(&r, "b", "width"), "250px");
    at(&mut r, 1500);
    assert_eq!(value(&r, "b", "width"), "300px");
    // (1.0 + 0.1 + 0.4 s is a hair past 1.5 in floating point: the transition ends at the next frame.)
    at(&mut r, 1520);
    assert_eq!(value(&r, "b", "width"), "300px");
    assert!(!r.animating());
    assert_eq!(r.geometry(b).unwrap().border_box.width, 300.0);
}

#[test]
fn a_keyframes_animation_sampled_at_three_times() {
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let html = "<!DOCTYPE html><style>
                  @keyframes fade { from { opacity: 0 } 50% { opacity: 1 } to { opacity: 0 } }
                  #b { animation: fade 2s linear 2 }
                </style><div id=b>x</div>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    // The animation starts with the first rendering update, at 0.
    at(&mut r, 0);
    assert!(r.animating());
    at(&mut r, 250);
    assert_eq!(value(&r, "b", "opacity"), "0.25");
    at(&mut r, 1500);
    assert_eq!(value(&r, "b", "opacity"), "0.5");
    // The second iteration.
    at(&mut r, 2750);
    assert_eq!(value(&r, "b", "opacity"), "0.75");
    // After both iterations, no fill: the underlying value.
    at(&mut r, 4500);
    assert_eq!(value(&r, "b", "opacity"), "1");
    assert!(!r.animating());
}

#[test]
fn the_search_button_computes_to_its_stylesheet_colours() {
    // en.wikipedia.org's search button (see fixtures/README.md): its sheet arrives late, its colours
    // transition. The first rendering waits for the sheet, so the button shows the sheet's colours,
    // not a transition from the UA's.
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    // The real clock, as `--render` runs.
    r.set_timeline_clock(TimelineClock::Monotonic);
    let html = include_str!("fixtures/search-button.html");
    r.load_html(html.as_bytes(), &server.url("/wiki/Snowlark"), None);
    let settled = r.render_until_settled(Duration::from_secs(20));
    assert!(settled.complete, "{settled:?}");
    assert!(r.update_rendering(VIEWPORT).is_some());
    assert_eq!(value(&r, "go", "background-color"), "rgb(248, 249, 250)");
    assert_eq!(value(&r, "go", "color"), "rgb(64, 66, 68)");
    assert_eq!(value(&r, "go", "border-top-color"), "rgb(114, 119, 125)");
    assert!(!r.animating());
}

/// The pieces a compositor gets for the page at `ms`: kinds, states and lists.
fn pieces(r: &mut Renderer, ms: u64) -> Vec<snowlark_paint::SplitLayer> {
    at(r, ms);
    let list = r.paint_with_embeds(VIEWPORT, snowlark_paint::Point::ZERO, 1.0, None, true);
    list.split_layers().layers
}

/// `docs/compositor-animations.md` S1: an element with a transform animation in effect (Blink's
/// `kActiveTransformAnimation`) or `will-change: transform` is painted as a composited layer, whose
/// list stays the same while the animation turns it: only its state changes, so the compositor
/// repaints nothing. A plain transform is no reason; a finished animation that does not fill is
/// none either.
#[test]
fn a_running_transform_animation_paints_its_box_as_a_composited_layer() {
    use snowlark_paint::LayerKind;
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let html = "<!DOCTYPE html><style>
                  @keyframes spin { to { transform: rotate(1turn) } }
                  #s { position: absolute; left: 100px; top: 100px; width: 64px; height: 64px;
                       animation: spin 1s linear 3 }
                  #s > div { height: 52px; border: 6px solid #ddd; border-radius: 50%;
                             border-bottom-color: transparent }
                  #w { position: absolute; left: 10px; top: 10px; width: 20px; height: 20px;
                       background: blue; will-change: transform }
                  #p { position: absolute; left: 40px; top: 10px; width: 20px; height: 20px;
                       background: red; transform: rotate(10deg) }
                </style><div id=s><div></div></div><div id=w></div><div id=p></div>
                <p style='margin-top: 200px'>after</p>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    let composited = |p: &[snowlark_paint::SplitLayer]| {
        p.iter()
            .filter(|l| matches!(l.kind, Some(LayerKind::Composited(_))))
            .count()
    };
    let first = pieces(&mut r, 0);
    let later = pieces(&mut r, 300);
    // The spinner and the `will-change` box; not the box with a plain transform.
    assert_eq!(composited(&first), 2, "{first:#?}");
    assert_eq!(first.len(), later.len());
    for (a, b) in first.iter().zip(&later) {
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.list, b.list, "a piece's list changed with the animation");
    }
    let turned = |p: &[snowlark_paint::SplitLayer]| {
        p.iter()
            .find(|l| {
                matches!(l.kind, Some(LayerKind::Composited(_)))
                    && l.list
                        .items
                        .iter()
                        .any(|i| matches!(i, snowlark_paint::Item::Border(_)))
            })
            .unwrap()
            .state
            .clone()
    };
    assert_ne!(turned(&first), turned(&later));
    // Past its three iterations the animation is not in effect: the spinner is painted in place.
    let done = pieces(&mut r, 3500);
    assert_eq!(composited(&done), 1);
    // Without layers (`--render`, `--wpt`), nothing is marked.
    let list = r.paint_with_embeds(VIEWPORT, snowlark_paint::Point::ZERO, 1.0, None, false);
    assert!(!list
        .items
        .iter()
        .any(|i| matches!(i, snowlark_paint::Item::PushLayer { .. })));
}

/// The state steps a piece's box's animations write: each [`snowlark_paint::StateOp::Animated`]
/// marker's key with the transform and opacity steps before it.
fn own_steps(state: &snowlark_paint::LayerState) -> Vec<(u64, [f32; 6], f32)> {
    use snowlark_paint::StateOp;
    let ops = &state.0;
    (2..ops.len())
        .filter_map(|i| match (&ops[i - 2], &ops[i - 1], &ops[i]) {
            (StateOp::Transform(t), StateOp::Opacity(o), StateOp::Animated(k)) => {
                Some((*k, *t, *o))
            }
            _ => None,
        })
        .collect()
}

/// `docs/compositor-animations.md` S2: the compositor gets the transform and opacity animations of
/// composited boxes with the frame, and what it samples from them at a time is what the page paints
/// at that time (the box's own state steps), through iterations, delays, directions, keyframe
/// easings, percentages, `transform-origin`, the individual transform properties and a
/// transition. Animations it cannot interpolate function by function, that animate anything else
/// as well, or that run reversed iterations with a non-linear easing, stay with the page.
#[test]
fn the_compositor_samples_what_the_page_paints() {
    use snowlark_paint::layer_animation::{Property, Sample};
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let html = "<!DOCTYPE html><style>
                  div { position: absolute; width: 40px; height: 30px; background: teal }
                  @keyframes spin { to { transform: rotate(1turn) } }
                  #a { left: 10px; top: 10px; animation: spin 1s linear infinite;
                       translate: 5px 3px; scale: 1.5 }
                  @keyframes slide {
                    0% { transform: translateX(0) rotate(0) }
                    40% { transform: translateX(50%) rotate(45deg);
                          animation-timing-function: steps(3, jump-both) }
                    100% { transform: translateX(120%) rotate(90deg) } }
                  #b { left: 60px; top: 10px; transform-origin: 10px 20px;
                       animation: slide 700ms cubic-bezier(.3, .1, .6, 1) 200ms 3 both }
                  @keyframes fade { from { opacity: 0 } 50% { opacity: .8 } to { opacity: .3 } }
                  #c { left: 110px; top: 10px; animation: fade 900ms linear infinite reverse }
                  @keyframes skewy { from { transform: skewX(0) scale(1) }
                                     to { transform: skewX(20deg) scale(2, .5) } }
                  #d { left: 160px; top: 10px; animation: skewy 1.3s ease-in-out infinite;
                       transform-box: content-box; padding: 4px; transform-origin: 30% 70% }
                  @keyframes bob { to { transform: translate(10%, -20%) } }
                  #e { left: 260px; top: 10px; animation: bob 600ms linear infinite alternate-reverse }
                  #f { left: 310px; top: 10px; animation: bob 600ms ease-in infinite alternate }
                  #t { left: 210px; top: 10px; opacity: 1; transition: opacity 800ms ease 100ms }
                  #t.off { opacity: .2 }
                  @keyframes mixed { from { transform: scale(1) } to { transform: rotate(10deg) } }
                  #m { left: 10px; top: 60px; animation: mixed 1s infinite }
                  @keyframes grow { to { transform: scale(2); width: 80px } }
                  #g { left: 60px; top: 60px; animation: grow 1s infinite }
                </style><div id=a></div><div id=b></div><div id=c></div><div id=d></div>
                <div id=e></div><div id=f></div><div id=t></div><div id=m></div><div id=g></div>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    at(&mut r, 0);
    let t = by_id(&r, "t");
    let mut dom = r.dom_mut().unwrap();
    dom.set_reactions_enabled(true);
    dom.set_attribute(t, "class", "off").unwrap();
    let reactions = dom.take_reactions();
    drop(dom);
    r.dom_changed(&reactions);
    let key = |id: &str| by_id(&r, id).to_bits();
    let (a, b, c, d, e, f, tr) = (
        key("a"),
        key("b"),
        key("c"),
        key("d"),
        key("e"),
        key("f"),
        key("t"),
    );
    let mut checked = 0;
    for ms in [
        16, 50, 130, 199, 230, 333, 450, 610, 777, 890, 1001, 1234, 1500, 2000, 2400, 2750,
    ] {
        at(&mut r, ms);
        let list = r.paint_with_embeds(VIEWPORT, snowlark_paint::Point::ZERO, 1.0, None, true);
        let mut keys: Vec<u64> = list.animations.iter().map(|x| x.element).collect();
        keys.sort_unstable();
        keys.dedup();
        // (the transition starts at the frame after the change, 16 ms, and runs 100 + 800 ms)
        // (#b runs 200 + 3 × 700 ms, then fills forwards: sampled at its end)
        // (#f eases alternate iterations with `ease-in`)
        let mut want = vec![a, b, c, d, e, f];
        // (also in its delay, at opacity 1: a current animation acts as `will-change: opacity`,
        // Web Animations 1 §5.7, so the box is a stacking context and a layer)
        if ms <= 900 {
            want.push(tr);
        }
        want.sort_unstable();
        assert_eq!(keys, want, "at {ms} ms");
        let split = list.split_layers();
        let now = ms as f64 / 1000.0;
        let mut seen = Vec::new();
        for piece in &split.layers {
            for (k, transform, opacity) in own_steps(&piece.state) {
                for anim in list.animations.iter().filter(|x| x.element == k) {
                    seen.push(k);
                    match (anim.sample(now), anim.property()) {
                        (Some(Sample::Transform(m)), Property::Transform) => {
                            let got = [m.m11, m.m12, m.m21, m.m22, m.dx, m.dy];
                            for (g, p) in got.iter().zip(transform) {
                                assert!(
                                    (g - p).abs() < 2e-3,
                                    "{ms} ms, #{k}: sampled {got:?}, painted {transform:?}"
                                );
                            }
                        }
                        (Some(Sample::Opacity(o)), Property::Opacity) => {
                            assert!(
                                (o - opacity).abs() < 1e-4,
                                "{ms} ms, #{k}: sampled {o}, painted {opacity}"
                            );
                        }
                        (s, p) => panic!("{ms} ms, #{k}: {p:?} sampled as {s:?}"),
                    }
                    checked += 1;
                }
            }
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen, want, "at {ms} ms: a box's own steps are not marked");
    }
    assert!(checked > 60, "{checked}");
}

/// S2, second half: once a frame painted for a compositor carries an animation, the page asks no
/// rendering update for it, only one at its next change (its end, its next iteration while a
/// listener for `animationiteration` exists, its start after a delay); an animation the compositor
/// does not run still asks for every frame.
#[test]
fn compositor_animations_ask_only_for_their_changes() {
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let html = "<!DOCTYPE html><style>
                  div { position: absolute; width: 40px; height: 30px; background: teal }
                  @keyframes spin { to { transform: rotate(1turn) } }
                  @keyframes fade { from { opacity: 0 } to { opacity: .5 } }
                  @keyframes tint { to { background-color: red } }
                  #a { left: 10px; animation: spin 1s linear infinite }
                  #b { left: 60px; animation: fade 400ms linear 3 }
                  #c { left: 110px; animation: spin 1s linear 300ms infinite; will-change: transform }
                </style><div id=a></div><div id=b></div><div id=c></div>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    at(&mut r, 0);
    at(&mut r, 100);
    // (before a frame for a compositor, the page services every animation)
    assert!(r.animating());
    let list = r.paint_with_embeds(VIEWPORT, snowlark_paint::Point::ZERO, 1.0, None, true);
    assert_eq!(list.animations.len(), 3);
    assert!(!r.animating(), "nothing left for the page to tick");
    // (#c starts at 300 ms; #b ends at 1200 ms; #a never)
    let next = r.compositor_next_change(false).unwrap();
    assert!((next - 0.3).abs() < 1e-6, "{next}");
    at(&mut r, 450);
    let _ = r.paint_with_embeds(VIEWPORT, snowlark_paint::Point::ZERO, 1.0, None, true);
    let next = r.compositor_next_change(false).unwrap();
    assert!((next - 1.2).abs() < 1e-6, "{next}");
    // (with iteration events: #b's next iteration at 800 ms)
    let next = r.compositor_next_change(true).unwrap();
    assert!((next - 0.8).abs() < 1e-6, "{next}");
    // (a list painted without layers goes to no compositor: what it runs stays)
    let _ = r.paint_with_embeds(VIEWPORT, snowlark_paint::Point::ZERO, 1.0, None, false);
    assert!(!r.animating());
    // An animation the compositor cannot run keeps the page ticking.
    let a = by_id(&r, "a");
    let mut dom = r.dom_mut().unwrap();
    dom.set_reactions_enabled(true);
    dom.set_attribute(a, "style", "animation: tint 1s infinite")
        .unwrap();
    let reactions = dom.take_reactions();
    drop(dom);
    r.dom_changed(&reactions);
    at(&mut r, 500);
    let _ = r.paint_with_embeds(VIEWPORT, snowlark_paint::Point::ZERO, 1.0, None, true);
    assert!(r.animating());
}

/// Web Animations 1 §5.3.4: in a reversed iteration the interval is chosen by the directed progress,
/// and the easing of its first keyframe (by offset) applies to the distance from it.
/// With `steps(2, jump-end)` 100 ms into a reversed 1 s iteration (directed progress 0.9) the value
/// is half way; easing the reversed distance with the other keyframe's function gave the start.
#[test]
fn reversed_iterations_ease_from_the_intervals_first_keyframe() {
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let html = "<!DOCTYPE html><style>
                  @keyframes f { from { opacity: 0 } to { opacity: 1 } }
                  @keyframes g { from { opacity: 0; animation-timing-function: steps(2, jump-end) }
                                 50% { opacity: .5; animation-timing-function: ease-in }
                                 to { opacity: 1 } }
                  #r { animation: f 1s steps(2, jump-end) reverse }
                  #a { animation: f 1s steps(2, jump-end) 2 alternate }
                  #k { animation: g 1s reverse }
                </style><div id=r>r</div><div id=a>a</div><div id=k>k</div>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    at(&mut r, 0);
    at(&mut r, 100);
    assert_eq!(value(&r, "r", "opacity"), "0.5");
    // (forwards, the first iteration: progress 0.1, the first step)
    assert_eq!(value(&r, "a", "opacity"), "0");
    // (directed progress 0.9: the interval 50%..100%, eased with 50%'s `ease-in`)
    let k: f64 = value(&r, "k", "opacity").parse().unwrap();
    let eased = {
        // cubic-bezier(.42, 0, 1, 1) at x = 0.8
        let (x1, y1, x2, y2) = (0.42f64, 0.0f64, 1.0f64, 1.0f64);
        let bez = |t: f64, a: f64, b: f64| {
            3.0 * (1.0 - t).powi(2) * t * a + 3.0 * (1.0 - t) * t * t * b + t.powi(3)
        };
        let (mut lo, mut hi) = (0.0, 1.0);
        for _ in 0..60 {
            let m = (lo + hi) / 2.0;
            if bez(m, x1, x2) < 0.8 {
                lo = m
            } else {
                hi = m
            }
        }
        bez(lo, y1, y2)
    };
    assert!(
        (k - (0.5 + 0.5 * eased)).abs() < 0.01,
        "{k}, want {}",
        0.5 + 0.5 * eased
    );
    // The second, reversed iteration of #a: 100 ms into it, directed progress 0.9.
    at(&mut r, 1100);
    assert_eq!(value(&r, "a", "opacity"), "0.5");
}

/// One script animation as the page would hand it to the style engine: its target, keyframes,
/// timing, start time (timeline ms), playback rate and composite operation.
struct ScriptEffect {
    target: &'static str,
    id: u64,
    keyframes: Arc<Vec<snowlark_style::web_animations::Keyframe>>,
    timing: snowlark_style::web_animations::Timing,
    start: f64,
    rate: f64,
    composite: snowlark_style::web_animations::CompositeOperation,
}

/// A keyframe: its offset, its (property, value)s and its easing.
type RawFrame<'a> = (Option<f64>, &'a [(&'a str, &'a str)], &'a str);

/// Keyframes from [`RawFrame`]s in order.
fn script_keyframes(
    base: &Url,
    frames: &[RawFrame<'_>],
) -> Arc<Vec<snowlark_style::web_animations::Keyframe>> {
    let raws = frames
        .iter()
        .map(
            |(offset, values, easing)| snowlark_style::web_animations::RawKeyframe {
                offset: *offset,
                easing: easing.to_string(),
                composite: None,
                values: values
                    .iter()
                    .map(|(p, v)| (p.to_string(), v.to_string()))
                    .collect(),
            },
        )
        .collect();
    Arc::new(snowlark_style::web_animations::process_sequence(raws, base).unwrap())
}

/// `docs/compositor-animations.md` S2, script animations: the effects `element.animate()` makes go
/// to the compositor under the rules CSS animations go through (Blink's
/// `CompositorAnimations::CheckCanStartEffectOnCompositor`), and what the compositor samples from
/// them at a time is what the page paints then, through the Web Animations timing CSS animations
/// do not use (playback rate, a negative one, delay, iteration start, the effect's easing),
/// keyframe easings, percentages and a neutral keyframe. An effect that adds, that animates
/// anything else as well, or that shares its property with another effect (CSS or script) stays
/// with the page; one that shares only the element does not.
#[test]
fn the_compositor_samples_script_animations_as_the_page_paints_them() {
    use snowlark_paint::layer_animation::{Property, Sample};
    use snowlark_style::web_animations::{
        CompositeOperation, CompositorTiming, Easing, EffectSample, FillMode, PlaybackDirection,
        Timing,
    };
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let html = "<!DOCTYPE html><style>
                  div { position: absolute; top: 10px; width: 40px; height: 30px; background: teal }
                  @keyframes fade { to { opacity: .5 } }
                  @keyframes spin { to { transform: rotate(1turn) } }
                </style>
                <div id=a style='left: 10px'></div><div id=b style='left: 60px'></div>
                <div id=c style='left: 110px'></div><div id=d style='left: 160px; opacity: .9'></div>
                <div id=e style='left: 210px'></div><div id=f style='left: 260px'></div>
                <div id=h style='left: 310px'></div>
                <div id=i style='left: 10px; top: 60px; animation: fade 1s infinite'></div>
                <div id=j style='left: 60px; top: 60px; animation: spin 1s linear infinite'></div>";
    let url = server.url("/page.html");
    r.load_html(html.as_bytes(), &url, None);
    at(&mut r, 0);
    let timing = |duration: f64, iterations: f64, easing: &str| Timing {
        duration: Some(duration),
        iterations,
        easing: Easing::parse(easing).unwrap(),
        ..Timing::default()
    };
    let replace = CompositeOperation::Replace;
    let mut id = 0;
    let mut effect = |target, keyframes, timing, start, rate, composite| {
        id += 1;
        ScriptEffect {
            target,
            id,
            keyframes,
            timing,
            start,
            rate,
            composite,
        }
    };
    let effects = vec![
        effect(
            "a",
            script_keyframes(
                &url,
                &[
                    (None, &[("transform", "rotate(0) translateX(0)")], "linear"),
                    (
                        None,
                        &[("transform", "rotate(1turn) translateX(20px)")],
                        "linear",
                    ),
                ],
            ),
            timing(1000.0, f64::INFINITY, "linear"),
            0.0,
            1.0,
            replace,
        ),
        effect(
            "b",
            script_keyframes(
                &url,
                &[
                    (None, &[("opacity", "0")], "ease-out"),
                    (None, &[("opacity", ".8")], "linear"),
                    (None, &[("opacity", ".3")], "linear"),
                ],
            ),
            Timing {
                delay: 200.0,
                fill: FillMode::Both,
                iteration_start: 0.25,
                direction: PlaybackDirection::Alternate,
                ..timing(700.0, 3.0, "cubic-bezier(.3, .1, .6, 1)")
            },
            50.0,
            1.5,
            replace,
        ),
        effect(
            "c",
            script_keyframes(
                &url,
                &[
                    (
                        None,
                        &[("transform", "translate(10%, 0)")],
                        "steps(3, jump-both)",
                    ),
                    (None, &[("transform", "translate(50%, 20px)")], "linear"),
                ],
            ),
            Timing {
                fill: FillMode::Both,
                ..timing(800.0, 2.0, "ease-in-out")
            },
            2000.0,
            -1.0,
            replace,
        ),
        // (one keyframe: the neutral one at 0 is the value without animations, 0.9)
        effect(
            "d",
            script_keyframes(&url, &[(Some(1.0), &[("opacity", ".2")], "linear")]),
            timing(1000.0, f64::INFINITY, "steps(4)"),
            0.0,
            1.0,
            replace,
        ),
        effect(
            "e",
            script_keyframes(
                &url,
                &[
                    (None, &[("transform", "rotate(0)")], "linear"),
                    (None, &[("transform", "rotate(90deg)")], "linear"),
                ],
            ),
            timing(1000.0, f64::INFINITY, "linear"),
            0.0,
            1.0,
            CompositeOperation::Add,
        ),
        effect(
            "f",
            script_keyframes(
                &url,
                &[
                    (None, &[("opacity", "0"), ("color", "red")], "linear"),
                    (None, &[("opacity", "1"), ("color", "blue")], "linear"),
                ],
            ),
            timing(1000.0, f64::INFINITY, "linear"),
            0.0,
            1.0,
            replace,
        ),
        effect(
            "h",
            script_keyframes(
                &url,
                &[
                    (None, &[("transform", "rotate(0)")], "linear"),
                    (None, &[("transform", "rotate(90deg)")], "linear"),
                ],
            ),
            timing(1000.0, f64::INFINITY, "linear"),
            0.0,
            1.0,
            replace,
        ),
        effect(
            "h",
            script_keyframes(
                &url,
                &[
                    (None, &[("transform", "scale(1)")], "linear"),
                    (None, &[("transform", "scale(2)")], "linear"),
                ],
            ),
            timing(600.0, f64::INFINITY, "linear"),
            0.0,
            1.0,
            replace,
        ),
        // (with #i's CSS opacity animation: neither runs there)
        effect(
            "i",
            script_keyframes(
                &url,
                &[
                    (None, &[("opacity", ".2")], "linear"),
                    (None, &[("opacity", ".6")], "linear"),
                ],
            ),
            timing(900.0, f64::INFINITY, "linear"),
            0.0,
            1.0,
            replace,
        ),
        // (beside #j's CSS transform animation: both run there)
        effect(
            "j",
            script_keyframes(
                &url,
                &[
                    (None, &[("opacity", ".2")], "linear"),
                    (None, &[("opacity", ".6")], "linear"),
                ],
            ),
            timing(900.0, f64::INFINITY, "linear"),
            0.0,
            1.0,
            replace,
        ),
    ];
    let targets: Vec<snowlark_dom::NodeId> = effects.iter().map(|e| by_id(&r, e.target)).collect();
    // What the page's timing model gives at `ms`.
    let samples = |ms: f64| -> Vec<EffectSample> {
        effects
            .iter()
            .zip(&targets)
            .filter_map(|(e, &target)| {
                let local = (ms - e.start) * e.rate;
                let t = e.timing.compute(Some(local), e.rate < 0.0);
                Some(EffectSample {
                    target,
                    id: e.id,
                    keyframes: e.keyframes.clone(),
                    composite: e.composite,
                    progress: Some(t.progress?),
                    iteration: t.current_iteration.unwrap_or(0.0),
                    repeat: 1,
                    compositor: Some(CompositorTiming {
                        start: e.start / 1000.0,
                        playback_rate: e.rate,
                        timing: e.timing.clone(),
                    }),
                })
            })
            .collect()
    };
    let key = |id: &str| by_id(&r, id).to_bits();
    let mut want: Vec<u64> = ["a", "b", "c", "d", "j"].iter().map(|i| key(i)).collect();
    want.sort_unstable();
    let j = key("j");
    let mut checked = 0;
    for ms in [16, 120, 260, 333, 480, 777, 1001, 1450, 1999, 2300] {
        r.set_script_effects(samples(ms as f64));
        at(&mut r, ms);
        let list = r.paint_with_embeds(VIEWPORT, snowlark_paint::Point::ZERO, 1.0, None, true);
        let mut keys: Vec<u64> = list.animations.iter().map(|x| x.element).collect();
        keys.sort_unstable();
        // (#j: its CSS transform animation and the script opacity one)
        assert_eq!(keys.iter().filter(|&&k| k == j).count(), 2, "at {ms} ms");
        keys.dedup();
        assert_eq!(keys, want, "at {ms} ms");
        let split = list.split_layers();
        let now = ms as f64 / 1000.0;
        let mut seen = Vec::new();
        for piece in &split.layers {
            for (k, transform, opacity) in own_steps(&piece.state) {
                for anim in list.animations.iter().filter(|x| x.element == k) {
                    seen.push(k);
                    match (anim.sample(now), anim.property()) {
                        (Some(Sample::Transform(m)), Property::Transform) => {
                            let got = [m.m11, m.m12, m.m21, m.m22, m.dx, m.dy];
                            for (g, p) in got.iter().zip(transform) {
                                assert!(
                                    (g - p).abs() < 2e-3,
                                    "{ms} ms, #{k}: sampled {got:?}, painted {transform:?}"
                                );
                            }
                        }
                        (Some(Sample::Opacity(o)), Property::Opacity) => {
                            assert!(
                                (o - opacity).abs() < 1e-4,
                                "{ms} ms, #{k}: sampled {o}, painted {opacity}"
                            );
                        }
                        (s, p) => panic!("{ms} ms, #{k}: {p:?} sampled as {s:?}"),
                    }
                    checked += 1;
                }
            }
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen, want, "at {ms} ms: a box's own steps are not marked");
    }
    assert!(checked >= 60, "{checked}");
}

/// Filter Effects 1 §14 on the compositor: a box whose `filter` animation (or transition) has
/// color functions only is painted with its filter group around its layer, a state step the
/// compositor writes; what it samples at a time is what the page paints at that time, `none` and
/// the shorter list's missing functions at their initial values. A blur (it moves pixels), lists
/// that do not interpolate function by function and a box with a backdrop filter stay with the
/// page.
#[test]
fn the_compositor_samples_filter_animations_as_the_page_paints_them() {
    use snowlark_paint::layer_animation::{Property, Sample};
    use snowlark_paint::StateOp;
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = renderer(&net);
    let html = "<!DOCTYPE html><style>
                  div { position: absolute; top: 10px; width: 40px; height: 30px; background: teal }
                  @keyframes gray { to { filter: grayscale(1) hue-rotate(90deg) } }
                  #a { left: 10px; animation: gray 1s linear infinite }
                  @keyframes dim { from { filter: brightness(.5) contrast(2) }
                                   to { filter: brightness(1.5) } }
                  #b { left: 60px; animation: dim 700ms ease-in-out 100ms infinite alternate }
                  @keyframes soft { to { filter: blur(4px) } }
                  #c { left: 110px; animation: soft 1s infinite }
                  @keyframes swap { from { filter: grayscale(1) } to { filter: sepia(1) } }
                  #d { left: 160px; animation: swap 1s infinite }
                  #e { left: 210px; animation: gray 1s infinite; backdrop-filter: invert(1) }
                  #f { left: 260px; filter: invert(0) opacity(1); transition: filter 800ms }
                  #f.on { filter: invert(1) opacity(.5) }
                </style><div id=a></div><div id=b></div><div id=c></div><div id=d></div>
                <div id=e></div><div id=f></div>";
    r.load_html(html.as_bytes(), &server.url("/page.html"), None);
    at(&mut r, 0);
    let f = by_id(&r, "f");
    let mut dom = r.dom_mut().unwrap();
    dom.set_reactions_enabled(true);
    dom.set_attribute(f, "class", "on").unwrap();
    let reactions = dom.take_reactions();
    drop(dom);
    r.dom_changed(&reactions);
    let key = |id: &str| by_id(&r, id).to_bits();
    let mut want = vec![key("a"), key("b"), key("f")];
    want.sort_unstable();
    let mut checked = 0;
    // (the transition runs from the frame after the change, 16 ms, for 800 ms)
    for ms in [16, 90, 150, 333, 500, 640, 777] {
        at(&mut r, ms);
        let list = r.paint_with_embeds(VIEWPORT, snowlark_paint::Point::ZERO, 1.0, None, true);
        let mut keys: Vec<u64> = list
            .animations
            .iter()
            .filter(|x| x.property() == Property::Filter)
            .map(|x| x.element)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, want, "at {ms} ms");
        let now = ms as f64 / 1000.0;
        let mut seen = Vec::new();
        for piece in list.split_layers().layers {
            let ops = &piece.state.0;
            for i in 3..ops.len() {
                let (
                    StateOp::Transform(_),
                    StateOp::Opacity(_),
                    StateOp::Filter(painted),
                    StateOp::Animated(k),
                ) = (&ops[i - 3], &ops[i - 2], &ops[i - 1], &ops[i])
                else {
                    continue;
                };
                for anim in list.animations.iter().filter(|x| x.element == *k) {
                    seen.push(*k);
                    // (in its delay, with no backwards fill, it has no effect: the box shows its
                    // own filter, `none` here)
                    let Some(Sample::Filter(sampled)) = anim.sample(now) else {
                        assert!(painted.as_slice().is_empty(), "{ms} ms, #{k}: {painted:?}");
                        continue;
                    };
                    let (s, p) = (sampled.ops(), painted.ops());
                    assert_eq!(s.len(), p.len(), "{ms} ms: sampled {s:?}, painted {p:?}");
                    for (x, y) in sampled.as_slice().iter().zip(painted.as_slice()) {
                        let close = match (x, y) {
                            (a, b) if std::mem::discriminant(a) != std::mem::discriminant(b) => {
                                false
                            }
                            _ => {
                                let amount = |f: &snowlark_paint::ColorFilter| match *f {
                                    snowlark_paint::ColorFilter::Brightness(v)
                                    | snowlark_paint::ColorFilter::Contrast(v)
                                    | snowlark_paint::ColorFilter::Grayscale(v)
                                    | snowlark_paint::ColorFilter::HueRotate(v)
                                    | snowlark_paint::ColorFilter::Invert(v)
                                    | snowlark_paint::ColorFilter::Opacity(v)
                                    | snowlark_paint::ColorFilter::Saturate(v)
                                    | snowlark_paint::ColorFilter::Sepia(v) => v,
                                };
                                (amount(x) - amount(y)).abs() < 2e-3
                            }
                        };
                        assert!(close, "{ms} ms, #{k}: sampled {s:?}, painted {p:?}");
                    }
                    checked += 1;
                }
            }
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen, want, "at {ms} ms: a box's filter step is not marked");
    }
    assert!(checked >= 18, "{checked}");
}
