// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §6.3 inert subtrees as user interaction sees them: inert boxes are not hit (an inert
//! element is absent for targeting: what is under it is hit), their text is not selected, the
//! blocking modal dialog escapes an inert document but not its own `inert` attribute, inertness
//! follows the flat tree (slotted content of an inert slot), and none of it shows in computed
//! style. Also `focus()`'s options (HTML §6.6.6): `preventScroll` and `focusVisible`.

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

#[test]
fn inert_elements_are_not_hit_and_their_text_is_not_selected() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body style='margin:0'>\
         <div id=under style='position:absolute;left:0;top:0;width:200px;height:100px'></div>\
         <div id=over inert style='position:absolute;left:0;top:0;width:200px;height:100px'>\
         <span id=child style='pointer-events:auto'>inert text</span>\
         <svg width=100 height=50><rect id=r width=100 height=50 /></svg></div>\
         <p id=sel style='position:absolute;top:200px'>selectable</p>",
    );
    let r = eval(
        &mut p,
        "const s = getSelection(); s.selectAllChildren(document.body); \
         [document.elementFromPoint(5, 5).id, document.elementFromPoint(20, 40).id, \
          getComputedStyle(over).pointerEvents, getComputedStyle(child).userSelect, \
          s.toString().trim()].join('|')",
    );
    assert_eq!(r, "under|under|auto|auto|selectable");
    // No longer inert: hit again, selected again.
    let r = eval(
        &mut p,
        "over.inert = false; getSelection().selectAllChildren(over); \
         [document.elementFromPoint(5, 5).id, getSelection().toString().trim()].join('|')",
    );
    assert_eq!(r, "over|inert text");
}

#[test]
fn the_blocking_dialog_escapes_an_inert_document_but_not_its_own_inert_attribute() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body style='margin:0'>\
         <div id=wrap inert><div id=page style='height:300px'></div>\
         <dialog id=d style='padding:0;border:0;width:200px;height:100px'>\
         <div id=a style='height:50px'></div><div id=b inert style='height:50px'></div>\
         </dialog></div>",
    );
    // Not modal: everything under `wrap` is inert, the dialog too.
    let r = eval(
        &mut p,
        "d.show(); const at = (x, y) => { const e = document.elementFromPoint(x, y); \
         return e.id || e.localName; }; let q = d.getBoundingClientRect(); \
         [at(10, 10), at(q.left + 10, q.top + 10)].join('|')",
    );
    assert_eq!(r, "body|html");
    // Modal: the dialog escapes `wrap`'s inertness, its `inert` child does not; the rest of the
    // document is inert (the backdrop's hit is the dialog).
    let r = eval(
        &mut p,
        "d.close(); d.showModal(); const r = d.getBoundingClientRect(); \
         [at(r.left + 10, r.top + 10), at(r.left + 10, r.top + 60), at(1, 1)].join('|')",
    );
    assert_eq!(r, "a|d|d");
    let r = eval(&mut p, "d.close(); wrap.inert = false; at(1, 1)");
    assert_eq!(r, "page");
}

#[test]
fn an_inert_slot_makes_its_slotted_content_inert() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body><div id=host><button id=slotted>b</button></div>\
         <script>host.attachShadow({mode: 'open'}).innerHTML = '<slot inert></slot>';</script>",
    );
    let r = eval(
        &mut p,
        "slotted.focus(); const before = document.activeElement === slotted; \
         host.shadowRoot.querySelector('slot').inert = false; slotted.focus(); \
         [before, document.activeElement === slotted].join('|')",
    );
    assert_eq!(r, "false|true");
}

#[test]
fn focus_options_scroll_into_view_and_indicate_focus() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body style='margin:0'><div style='height:2000px'></div>\
         <button id=far>far</button><button id=near style='position:fixed;top:0'>near</button>",
    );
    let r = eval(
        &mut p,
        "far.focus({preventScroll: true}); const kept = scrollY; near.focus(); \
         far.focus(); const moved = scrollY; \
         [kept, moved > 1000, far.matches(':focus-visible')].join('|')",
    );
    assert_eq!(r, "0|true|true");
    let r = eval(
        &mut p,
        "near.focus({focusVisible: false}); const a = near.matches(':focus-visible'); \
         far.focus({focusVisible: true}); [a, far.matches(':focus-visible')].join('|')",
    );
    assert_eq!(r, "false|true");
}

/// `focus()` on the element that already has the focus changes nothing: the focusing steps
/// return at once (HTML §6.6.4 step 3), and browsers do not scroll it into view again either,
/// also through a shadow host that delegates the focus to it.
#[test]
fn focus_on_the_focused_element_does_not_scroll() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body style='margin:0'><div style='height:2000px'></div>\
         <button id=far>far</button><div id=host></div><div style='height:2000px'></div>",
    );
    let r = eval(
        &mut p,
        "far.focus(); const moved = scrollY > 1000; scrollTo(0, 0); far.focus(); \
         const again = scrollY; \
         host.attachShadow({mode: 'open', delegatesFocus: true}).innerHTML = '<input>'; \
         host.focus(); const inner = scrollY > 1000; scrollTo(0, 0); host.focus(); \
         [moved, again, inner, scrollY, document.activeElement === host].join('|')",
    );
    assert_eq!(r, "true|0|true|0|true");
}
