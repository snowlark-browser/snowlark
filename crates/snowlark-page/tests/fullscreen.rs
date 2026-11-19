// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Fullscreen API: `requestFullscreen()` with its activation and policy checks, the events in the
//! rendering update, `:fullscreen` and the top layer, exits (the API, removal, the embedder), and
//! the requests the window answers.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn page(html: &str) -> Page {
    // (the thread lives as long as the test: it is leaked, as the page needs it until the end)
    let net = Box::leak(Box::new(NetThread::start(NetConfig::default()).unwrap()));
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(html, &url::Url::parse("https://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

fn settle(page: &mut Page) {
    page.run_until_settled(Duration::from_secs(5));
}

/// A user activation (a key press).
fn activate(page: &mut Page) {
    page.key_down(&snowlark_page::keyboard::KeyInput {
        vk: 0x41,
        scan: 0x1E,
        text: Some('a'),
        mods: snowlark_page::input::Modifiers::default(),
        repeat: false,
        paste: None,
        paste_html: None,
    });
    settle(page);
}

const DOC: &str = "<!doctype html><body style='margin:0'>\
    <div style='width:100px;height:50px;margin:20px;transform:translateX(5px)'>\
    <div id=v style='width:60px;height:30px;border:3px solid red'></div></div>\
    <script>window.log = [];\
    document.addEventListener('fullscreenchange', e => log.push('change@' + (e.target.id || e.target.nodeName) + ':' + e.composed + e.bubbles));\
    document.addEventListener('fullscreenerror', e => log.push('error@' + (e.target.id || e.target.nodeName)));\
    addEventListener('resize', () => log.push('resize'));</script>";

#[test]
fn a_request_without_activation_is_an_error() {
    let mut p = page(DOC);
    eval(
        &mut p,
        "v.requestFullscreen().then(() => log.push('resolved'), e => log.push('rejected:' + e.name)); 'ok'",
    );
    settle(&mut p);
    assert_eq!(
        eval(
            &mut p,
            "[log.join(), document.fullscreenElement, document.fullscreen].join('|')"
        ),
        "rejected:TypeError,error@v||false"
    );
}

#[test]
fn a_request_with_activation_makes_the_element_fullscreen() {
    let mut p = page(DOC);
    activate(&mut p);
    let r = eval(
        &mut p,
        "v.requestFullscreen().then(() => log.push('resolved:' + (document.fullscreenElement === v)));\
         [document.fullscreenElement, navigator.userActivation.isActive].join('|')",
    );
    // (nothing changes before the steps that run in parallel; the activation is consumed)
    assert_eq!(r, "|false");
    settle(&mut p);
    let r = eval(
        &mut p,
        "var r = v.getBoundingClientRect();\
         [log.join(), document.fullscreenElement === v, document.fullscreen, v.matches(':fullscreen'),\
          v.matches(':modal'), document.querySelector(':fullscreen') === v,\
          r.x, r.y, r.width, r.height, getComputedStyle(v).position,\
          getComputedStyle(v).objectFit].join('|')",
    );
    // The promise resolves before the rendering update fires the event; the element covers the
    // viewport from the top layer (its transformed ancestor notwithstanding).
    assert_eq!(
        r,
        "resolved:true,change@v:truetrue|true|true|true|true|true|0|0|800|600|fixed|contain"
    );
}

#[test]
fn exit_fullscreen_and_removal_end_it() {
    let mut p = page(DOC);
    activate(&mut p);
    eval(&mut p, "v.requestFullscreen(); 'ok'");
    settle(&mut p);
    let r = eval(
        &mut p,
        "log.length = 0; document.exitFullscreen().then(() => log.push('exited'));\
         String(document.fullscreenElement === v)",
    );
    assert_eq!(r, "true");
    settle(&mut p);
    assert_eq!(
        eval(
            &mut p,
            "[log.join(), document.fullscreenElement, v.matches(':fullscreen')].join('|')"
        ),
        "exited,change@v:truetrue||false"
    );
    // Not fullscreen: exitFullscreen() rejects.
    eval(
        &mut p,
        "document.exitFullscreen().catch(e => log.push(e.name)); 'ok'",
    );
    settle(&mut p);
    assert_eq!(
        eval(&mut p, "log.join()"),
        "exited,change@v:truetrue,TypeError"
    );
    // The fullscreen element leaves the document: fullscreen ends, the event goes to the document.
    activate(&mut p);
    eval(&mut p, "v.requestFullscreen(); 'ok'");
    settle(&mut p);
    eval(&mut p, "log.length = 0; v.remove(); 'ok'");
    settle(&mut p);
    assert_eq!(
        eval(&mut p, "[log.join(), document.fullscreenElement].join('|')"),
        "change@#document:truetrue|"
    );
}

#[test]
fn the_fullscreen_feature_follows_the_container_policy() {
    let mut p = page(
        "<!doctype html><iframe id=a srcdoc='x'></iframe>\
         <iframe id=b allow=\"fullscreen 'none'\" srcdoc='x'></iframe>\
         <iframe id=c allow='fullscreen *' srcdoc='x'></iframe>",
    );
    let r = eval(
        &mut p,
        "[document.fullscreenEnabled, a.contentDocument.fullscreenEnabled,\
          b.contentDocument.fullscreenEnabled, c.contentDocument.fullscreenEnabled,\
          document.implementation.createHTMLDocument('').fullscreenEnabled].join()",
    );
    assert_eq!(r, "true,true,false,true,false");
    // The policy is the document's from when it was made: a later attribute does not change it.
    let r = eval(
        &mut p,
        "a.setAttribute('allow', \"fullscreen 'none'\"); String(a.contentDocument.fullscreenEnabled)",
    );
    assert_eq!(r, "true");
}

#[test]
fn an_element_in_an_iframe_makes_its_iframe_fullscreen_too() {
    let mut p = page(
        "<!doctype html><body style='margin:0'><iframe id=f style='width:100px;height:80px'\
         srcdoc='<div id=inner>x</div>'></iframe><script>window.log = [];\
         document.addEventListener('fullscreenchange', e => log.push('outer@' + e.target.id));</script>",
    );
    activate(&mut p);
    eval(
        &mut p,
        "var d = f.contentDocument;\
         d.addEventListener('fullscreenchange', e => log.push('inner@' + e.target.id));\
         d.getElementById('inner').requestFullscreen(); 'ok'",
    );
    settle(&mut p);
    let r = eval(
        &mut p,
        "var d2 = f.contentDocument; var r2 = f.getBoundingClientRect();\
         [log.join(), document.fullscreenElement === f, d2.fullscreenElement.id, f.matches(':fullscreen'),\
          r2.width, r2.height].join('|')",
    );
    // (the parent's event before the inner document's: documents in tree order)
    assert_eq!(r, "outer@f,inner@inner|true|inner|true|800|600");
    // Leaving from the inner document leaves both (the iframe was a simple fullscreen document).
    eval(
        &mut p,
        "log.length = 0; f.contentDocument.exitFullscreen(); 'ok'",
    );
    settle(&mut p);
    assert_eq!(
        eval(&mut p, "[log.join(), document.fullscreenElement, f.contentDocument.fullscreenElement].join('|')"),
        "outer@f,inner@inner||"
    );
}

#[test]
fn the_window_answers_the_request_and_ends_it() {
    let mut p = page(DOC);
    p.engine().set_fullscreen_by_embedder(true);
    activate(&mut p);
    eval(
        &mut p,
        "v.requestFullscreen().then(() => log.push('resolved')); 'ok'",
    );
    settle(&mut p);
    // The page asked; nothing happens until the window answers.
    assert_eq!(p.engine().take_fullscreen_asks(), vec![true]);
    assert_eq!(
        eval(&mut p, "[log.join(), document.fullscreenElement].join('|')"),
        "|"
    );
    p.engine().fullscreen_viewport_answer(true);
    settle(&mut p);
    assert_eq!(
        eval(
            &mut p,
            "[log.join(), document.fullscreenElement === v].join('|')"
        ),
        "resolved,change@v:truetrue|true"
    );
    // The user ends it in the window (Esc): the page fully exits fullscreen.
    eval(&mut p, "log.length = 0; 'ok'");
    p.engine().fullscreen_viewport_answer(false);
    settle(&mut p);
    assert_eq!(
        eval(&mut p, "[log.join(), document.fullscreenElement].join('|')"),
        "change@v:truetrue|"
    );
    assert!(p.engine().take_fullscreen_asks().is_empty());
    // A denied request: an error, and the page asks nothing more.
    activate(&mut p);
    eval(
        &mut p,
        "log.length = 0; v.requestFullscreen().catch(e => log.push(e.name)); 'ok'",
    );
    settle(&mut p);
    assert_eq!(p.engine().take_fullscreen_asks(), vec![true]);
    p.engine().fullscreen_viewport_answer(false);
    settle(&mut p);
    assert_eq!(eval(&mut p, "log.join()"), "TypeError,error@v");
    // The page's own exit asks the window, and finishes when it answers.
    activate(&mut p);
    eval(&mut p, "log.length = 0; v.requestFullscreen(); 'ok'");
    settle(&mut p);
    assert_eq!(p.engine().take_fullscreen_asks(), vec![true]);
    p.engine().fullscreen_viewport_answer(true);
    settle(&mut p);
    eval(
        &mut p,
        "log.length = 0; document.exitFullscreen().then(() => log.push('exited')); 'ok'",
    );
    settle(&mut p);
    assert_eq!(p.engine().take_fullscreen_asks(), vec![false]);
    assert_eq!(
        eval(&mut p, "String(document.fullscreenElement === v)"),
        "true"
    );
    p.engine().fullscreen_viewport_answer(false);
    settle(&mut p);
    assert_eq!(
        eval(&mut p, "[log.join(), document.fullscreenElement].join('|')"),
        "exited,change@v:truetrue|"
    );
}

#[test]
fn the_root_element_fullscreen_keeps_its_layout() {
    // `document.documentElement.requestFullscreen()`: the root is the root box already (it is not
    // built a second time from the top layer), and the user agent rules for `:not(:root):fullscreen`
    // leave it and its content as they are.
    let mut p = page(
        "<!doctype html><body style='margin:0'><div id=v style='width:480px;aspect-ratio:4/3'></div>",
    );
    activate(&mut p);
    eval(&mut p, "document.documentElement.requestFullscreen(); 'ok'");
    settle(&mut p);
    let r = eval(
        &mut p,
        "var r = v.getBoundingClientRect();\
         [document.fullscreenElement === document.documentElement,\
          document.documentElement.matches(':fullscreen'), r.width, r.height,\
          document.documentElement.getBoundingClientRect().width].join('|')",
    );
    assert_eq!(r, "true|true|480|360|800");
}

#[test]
fn rects_in_the_handler_match_the_settled_layout() {
    // The root element fullscreen with the viewport resized by the window: what the handler of
    // `fullscreenchange` measures is the settled layout (widths of the new viewport, heights as
    // they were: nothing is laid out twice).
    let mut p = page(
        "<!doctype html><body style='margin:20px'><div><button>a</button><button>b</button></div>         <video id=v style='width:480px;display:block'></video>         <div id=w style='width:480px;aspect-ratio:4/3'></div><div id=log style='white-space:pre'>x
y
z</div>",
    );
    p.engine().set_fullscreen_by_embedder(true);
    let probe = "[v, w, log, document.body, document.documentElement].map(e => { const r = e.getBoundingClientRect(); return e.id + ':' + r.y + ',' + r.width + 'x' + r.height; }).join(' ')";
    let before = eval(&mut p, probe);
    activate(&mut p);
    eval(
        &mut p,
        &format!("window.seen = []; document.addEventListener('fullscreenchange', () => seen.push({probe})); document.documentElement.requestFullscreen(); 'ok'"),
    );
    settle(&mut p);
    assert_eq!(p.engine().take_fullscreen_asks(), vec![true]);
    p.engine()
        .set_viewport(snowlark_paint::Size::new(640.0, 480.0));
    p.engine().fullscreen_viewport_answer(true);
    settle(&mut p);
    let after = eval(&mut p, probe);
    let in_handler = eval(&mut p, "seen.join(' // ')");
    assert_eq!(
        before,
        "v:41,480x150 w:191,480x360 log:551,760x54 :20,760x585 :0,800x625"
    );
    assert_eq!(
        after,
        "v:41,480x150 w:191,480x360 log:551,600x54 :20,600x585 :0,640x625"
    );
    assert_eq!(after, in_handler);
}

#[test]
fn the_viewport_has_no_scrollbar_under_a_fullscreen_element() {
    // The viewport's overflow is hidden while an element of its document is fullscreen (the
    // window's scrollbar showed beside the element otherwise), so the element covers the whole
    // viewport; the scrollbar comes back on exit, and the root element itself fullscreen keeps it.
    let mut p = page(
        "<!doctype html><body style='margin:0;height:3000px'><div id=v style='width:60px;height:30px'></div>",
    );
    p.engine().set_scrollbar_width(15.0);
    settle(&mut p);
    let probe = "[v.getBoundingClientRect().width, document.documentElement.clientWidth].join()";
    assert_eq!(eval(&mut p, probe), "60,785");
    activate(&mut p);
    eval(&mut p, "v.requestFullscreen(); 'ok'");
    settle(&mut p);
    assert_eq!(eval(&mut p, probe), "800,800");
    eval(&mut p, "document.exitFullscreen(); 'ok'");
    settle(&mut p);
    assert_eq!(eval(&mut p, probe), "60,785");
    activate(&mut p);
    eval(&mut p, "document.documentElement.requestFullscreen(); 'ok'");
    settle(&mut p);
    assert_eq!(eval(&mut p, probe), "60,785");
}
