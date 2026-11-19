// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.10.5 "show the picker, if applicable": the activation behavior of an input whose type has
//! a picker consumes the transient user activation (when the element is mutable), connected or not.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn page_with(html: &str) -> Page {
    static NET: std::sync::OnceLock<NetThread> = std::sync::OnceLock::new();
    let net = NET.get_or_init(|| NetThread::start(NetConfig::default()).unwrap());
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

/// A button at 0,0 whose real click runs `window.onpress` and records the activation after it.
const PAGE: &str = "<!doctype html><style>body{margin:0}button{width:100px;height:40px}</style>\
    <button id=b>b</button><script>var after = null;\
    b.addEventListener('click', () => { window.onpress(); after = navigator.userActivation.isActive; });\
    </script>";

fn click_with(page: &mut Page, js: &str) -> String {
    eval(page, &format!("window.onpress = () => {{ {js} }}"));
    let none = Modifiers::default();
    let at = Point::new(20.0, 20.0);
    page.pointer_moved(at, none);
    page.pointer_pressed(at, 0, none, 1);
    page.pointer_released(at, 0, none, 1);
    eval(page, "String(after)")
}

#[test]
fn clicking_a_picker_input_consumes_the_user_activation() {
    for ty in [
        "color",
        "date",
        "datetime-local",
        "file",
        "month",
        "time",
        "week",
    ] {
        let mut page = page_with(PAGE);
        let js = format!("const i = document.createElement('input'); i.type = '{ty}'; i.click();");
        assert_eq!(click_with(&mut page, &js), "false", "{ty}");
    }
}

/// HTML §4.10.5 and §4.10.7 `showPicker()`: a mutable element first (disabled, or read-only where
/// `readonly` applies, throws and keeps the activation), then transient activation, which it
/// consumes; a select must be being rendered.
#[test]
fn show_picker_checks_mutability_activation_and_rendering() {
    let mut page = page_with(PAGE);
    let name = "(f => { try { f(); return 'ok'; } catch (e) { return e.name; } })";
    eval(&mut page, &format!("window.name_of = {name}"));
    assert_eq!(
        eval(
            &mut page,
            "name_of(() => document.createElement('input').showPicker())"
        ),
        "NotAllowedError"
    );
    let js = "const i = document.createElement('input'); i.disabled = true; \
        window.r = name_of(() => i.showPicker());";
    click_with(&mut page, js);
    assert_eq!(eval(&mut page, "r"), "InvalidStateError");
    let mut page = page_with(PAGE);
    eval(&mut page, &format!("window.name_of = {name}"));
    let js = "const i = document.createElement('input'); i.readOnly = true; \
        window.r = name_of(() => i.showPicker());";
    assert_eq!(click_with(&mut page, js), "true");
    assert_eq!(eval(&mut page, "r"), "InvalidStateError");
    // (`readonly` does not apply to a checkbox: the picker step runs and consumes the activation)
    let js = "const i = document.createElement('input'); i.type = 'checkbox'; i.readOnly = true; \
        window.r = name_of(() => i.showPicker());";
    assert_eq!(click_with(&mut page, js), "false");
    assert_eq!(eval(&mut page, "r"), "ok");
    let mut page = page_with(PAGE);
    eval(&mut page, &format!("window.name_of = {name}"));
    let js = "const s = document.createElement('select'); s.style.display = 'none'; \
        document.body.append(s); window.r = name_of(() => s.showPicker());";
    assert_eq!(click_with(&mut page, js), "true");
    assert_eq!(eval(&mut page, "r"), "NotSupportedError");
    let js = "const s = document.createElement('select'); document.body.append(s); \
        window.r = name_of(() => s.showPicker());";
    assert_eq!(click_with(&mut page, js), "false");
    assert_eq!(eval(&mut page, "r"), "ok");
}

#[test]
fn other_inputs_and_disabled_pickers_keep_it() {
    let mut page = page_with(PAGE);
    let js = "const i = document.createElement('input'); i.type = 'text'; i.click();";
    assert_eq!(click_with(&mut page, js), "true");
    let mut page = page_with(PAGE);
    let js = "const i = document.createElement('input'); i.type = 'color'; i.disabled = true; i.click();";
    assert_eq!(click_with(&mut page, js), "true");
}
