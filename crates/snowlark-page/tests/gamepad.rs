// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Gamepad through the page: `navigator.getGamepads()` with no gamepad connected, its Permissions
//! Policy check, and the interfaces; and the "battery" feature through a container's `allow`.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page(html: &str) -> Page {
    let mut page = Page::new(PageOptions::default());
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

/// Waits for `window.out` to be set by a frame's message.
fn wait_out(page: &mut Page) -> String {
    page.run_until(std::time::Instant::now() + Duration::from_secs(10), |p| {
        p.global_string("out").is_some()
    });
    eval(page, "String(window.out)")
}

#[test]
fn get_gamepads_answers_an_empty_list() {
    let mut p = page("<!doctype html>");
    let r = eval(
        &mut p,
        "const a = navigator.getGamepads(), b = navigator.getGamepads();\
         [Array.isArray(a), a.length, a !== b].join()",
    );
    assert_eq!(r, "true,0,true");
    let r = eval(
        &mut p,
        "[typeof Gamepad, typeof GamepadButton, typeof GamepadHapticActuator, typeof GamepadEvent,\
          'ongamepadconnected' in window, 'ongamepaddisconnected' in window,\
          new GamepadEvent('gamepadconnected').gamepad,\
          (() => { try { Navigator.prototype.getGamepads.call({}); return 'no throw' }\
                   catch (e) { return e.name } })()].join()",
    );
    assert_eq!(
        r,
        "function,function,function,function,true,true,,TypeError"
    );
}

#[test]
fn get_gamepads_follows_the_permissions_policy() {
    let mut p = page(
        "<!doctype html><iframe id=a srcdoc='x'></iframe>\
         <iframe id=b allow=\"gamepad 'none'\" srcdoc='x'></iframe>",
    );
    let r = eval(
        &mut p,
        "const n = b.contentWindow.navigator;\
         [a.contentWindow.navigator.getGamepads().length,\
          (() => { try { n.getGamepads(); return 'no throw' } catch (e) { return e.name } })()]\
         .join()",
    );
    assert_eq!(r, "0,SecurityError");
    // A navigator whose document is no longer fully active has no gamepads.
    let r = eval(
        &mut p,
        "const m = a.contentWindow.navigator; a.remove(); String(m.getGamepads().length)",
    );
    assert_eq!(r, "0");
}

#[test]
fn a_cross_origin_frame_may_use_gamepads_by_default() {
    // The default allowlist of "gamepad" is `*`: an opaque-origin frame may use it.
    let mut p = page(
        "<!doctype html><script>onmessage = e => window.out = e.data</script>\
         <iframe sandbox=allow-scripts srcdoc=\"<script>parent.postMessage(\
         'gamepads ' + navigator.getGamepads().length, '*')</script>\"></iframe>",
    );
    assert_eq!(wait_out(&mut p), "gamepads 0");
}

#[test]
fn the_battery_feature_follows_the_container_allow_attribute() {
    // "battery" defaults to `'self'`: a cross-origin frame gets it only through `allow`.
    let frame = |allow: &str| {
        format!(
            "<!doctype html><script>onmessage = e => window.out = e.data</script>\
             <iframe {allow} sandbox=allow-scripts srcdoc=\"<script>navigator.getBattery().then(\
             b => parent.postMessage('level ' + b.level, '*'), e => parent.postMessage(e.name, '*'))\
             </script>\"></iframe>"
        )
    };
    let mut p = page(&frame(""));
    assert_eq!(wait_out(&mut p), "NotAllowedError");
    let mut p = page(&frame("allow='battery *'"));
    assert_eq!(wait_out(&mut p), "level 1");
}
