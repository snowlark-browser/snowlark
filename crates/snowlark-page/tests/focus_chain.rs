// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §6.6 focus beyond one element: the focus chain through navigable containers (a frame's
//! element focused by script makes the frame its parent's focused area, and focus in the parent
//! takes it from the frame), the sequential focus navigation starting point (Tab moves on from a
//! focused element that was disabled), and the `tabIndex` getter's defaults.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::keyboard::KeyInput;
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

fn tab(page: &mut Page) {
    page.key_down(&KeyInput {
        vk: 0x09,
        scan: 0,
        text: None,
        mods: Modifiers::default(),
        repeat: false,
        paste: None,
        paste_html: None,
    });
    page.run_until_settled(Duration::from_secs(5));
}

#[test]
fn script_focus_in_a_frame_focuses_its_container() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body><input id=outer>\
         <iframe id=f srcdoc='<input id=inner>'></iframe>",
    );
    let r = eval(
        &mut p,
        "const d = f.contentDocument; outer.focus(); d.getElementById('inner').focus(); \
         const a = [document.activeElement.id, d.activeElement.id]; outer.focus(); \
         a.concat([document.activeElement.id, d.activeElement.localName]).join('|')",
    );
    assert_eq!(r, "f|inner|outer|body");
}

#[test]
fn tab_moves_on_from_a_focused_element_that_was_disabled() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body><button id=a>a</button><button id=b>b</button>\
         <button id=c>c</button>",
    );
    eval(&mut p, "b.focus(); b.disabled = true; 'ok'");
    p.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut p, "document.activeElement.localName"), "body");
    tab(&mut p);
    assert_eq!(eval(&mut p, "document.activeElement.id"), "c");
}

#[test]
fn tab_index_defaults() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body><a id=a>a</a><object id=o></object>\
         <details><summary id=s1>1</summary><summary id=s2>2</summary></details>\
         <svg><a id=sa></a></svg><div id=d></div>",
    );
    let r = eval(
        &mut p,
        "[a, o, s1, s2, document.getElementById('sa'), d].map(e => e.tabIndex).join('|')",
    );
    assert_eq!(r, "0|0|0|-1|0|-1");
}
