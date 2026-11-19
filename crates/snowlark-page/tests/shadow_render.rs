// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Boxes come from the flat tree (CSS Scoping 1 §3.1): a shadow host renders its shadow tree, a
//! slot its assigned nodes (else its fallback), unslotted light children nothing; also when the
//! shadow root is attached after the host was rendered (Reddit's Lit components).

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn page(net: &NetThread, html: &str) -> Page {
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

#[test]
fn a_late_shadow_root_replaces_the_light_children() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body style='margin:0'>\
         <x-head id=h style='display:block'><div id=skeleton style='height:50px'></div>\
         <span id=count slot=count>2.3M</span></x-head>",
    );
    // Before: the light children render.
    assert_eq!(
        eval(&mut p, "document.getElementById('skeleton').getClientRects().length + '|' + (h.offsetHeight > 50)"),
        "1|true"
    );
    eval(
        &mut p,
        r#"
        const r = h.attachShadow({ mode: 'open' });
        r.innerHTML = '<div id=box style="height:30px">Title <slot name=count></slot><slot name=none>fallback</slot></div>';
        'ok'
        "#,
    );
    p.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(
            &mut p,
            "[document.getElementById('skeleton').getClientRects().length, h.offsetHeight, \
              document.getElementById('count').getClientRects().length > 0, \
              h.shadowRoot.getElementById('box').offsetHeight].join('|')"
        ),
        "0|30|true|30"
    );
}

#[test]
fn an_empty_shadow_root_hides_the_light_children() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body style='margin:0'><div id=h><p id=c>light</p></div>",
    );
    eval(&mut p, "h.attachShadow({ mode: 'open' }); 'ok'");
    p.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut p, "c.getClientRects().length + '|' + h.offsetHeight"),
        "0|0"
    );
}

/// A constructed sheet's rules apply where it is adopted, and changing them restyles only there:
/// `replaceSync` on a sheet adopted nowhere restyles nothing (Lit makes its components' sheets so,
/// then adopts them in each component's shadow root); on one a shadow root adopts, that shadow tree;
/// on one the document adopts, the document.
#[test]
fn constructed_sheets_restyle_where_they_are_adopted() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut html = String::from("<!doctype html><body>");
    for _ in 0..200 {
        html.push_str("<p>x</p>");
    }
    html.push_str("<x-a id=h style='display:block'></x-a>");
    let mut p = page(&net, &html);
    eval(
        &mut p,
        "window.r = h.attachShadow({ mode: 'open' }); r.innerHTML = '<span id=s>s</span>';\
         window.sheet = new CSSStyleSheet(); r.adoptedStyleSheets = [sheet]; 'ok'",
    );
    p.update_the_rendering();
    let restyled = |p: &mut Page| p.engine().with_renderer(|r, _| r.stats().restyled).unwrap();
    let before = restyled(&mut p);
    eval(
        &mut p,
        "new CSSStyleSheet().replaceSync('p { color: red }'); 'ok'",
    );
    p.update_the_rendering();
    assert_eq!(
        restyled(&mut p),
        before,
        "a sheet adopted nowhere restyles nothing"
    );
    eval(
        &mut p,
        "sheet.replaceSync('#s { color: rgb(0, 128, 0) }'); 'ok'",
    );
    p.update_the_rendering();
    let shadow = restyled(&mut p) - before;
    assert!(
        shadow < 20,
        "only the shadow tree restyles: {shadow} elements"
    );
    assert_eq!(
        eval(&mut p, "getComputedStyle(r.getElementById('s')).color"),
        "rgb(0, 128, 0)"
    );
    eval(
        &mut p,
        "const d = new CSSStyleSheet(); document.adoptedStyleSheets = [d];\
         d.replaceSync('p { color: rgb(0, 0, 255) }'); 'ok'",
    );
    assert_eq!(
        eval(
            &mut p,
            "getComputedStyle(document.querySelector('p')).color"
        ),
        "rgb(0, 0, 255)"
    );
}
