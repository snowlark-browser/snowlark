// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `:focus-visible` (Selectors 4 §9.4): the heuristics that decide whether a focus shows a ring,
//! for style and for the DOM's own selector matching.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::keyboard::KeyInput;
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

/// Script focus shows a ring until a pointer focuses something; then a script focus does not,
/// until keyboard input, which also gives the focused element one. A date field shows a ring
/// whatever focused it.
#[test]
fn script_focus_follows_the_last_pointer_or_keyboard_input() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>* { outline: none } :focus-visible { outline: 2px solid green }\
         button, input { position: absolute; left: 0; width: 100px; height: 40px }</style>\
         <button id=a style='top: 0'>a</button><button id=b style='top: 50px'>b</button>\
         <input id=d type=date style='top: 100px'>",
    );
    let ring = |page: &mut Page, id: &str| {
        eval(
            page,
            &format!("{id}.matches(':focus-visible') + ',' + getComputedStyle({id}).outlineStyle"),
        )
    };
    eval(&mut page, "a.focus()");
    assert_eq!(ring(&mut page, "a"), "true,solid", "initial script focus");
    let m = Modifiers::default();
    page.pointer_pressed(Point::new(50.0, 70.0), 0, m, 1);
    page.pointer_released(Point::new(50.0, 70.0), 0, m, 1);
    assert_eq!(eval(&mut page, "document.activeElement.id"), "b");
    assert_eq!(ring(&mut page, "b"), "false,none", "focused by a click");
    eval(&mut page, "a.focus()");
    assert_eq!(
        ring(&mut page, "a"),
        "false,none",
        "script focus after a click"
    );
    eval(&mut page, "d.focus()");
    assert_eq!(ring(&mut page, "d"), "true,solid", "a date field");
    eval(&mut page, "a.focus()");
    let k = KeyInput {
        vk: 0x41,
        scan: 0,
        text: Some('a'),
        mods: m,
        repeat: false,
        paste: None,
        paste_html: None,
    };
    page.key_down(&k);
    page.key_up(&k);
    assert_eq!(ring(&mut page, "a"), "true,solid", "keyboard input");
    eval(&mut page, "b.focus()");
    assert_eq!(
        ring(&mut page, "b"),
        "true,solid",
        "script focus after keyboard input"
    );
}
