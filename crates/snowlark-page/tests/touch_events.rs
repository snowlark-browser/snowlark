// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Touch Events: the `TouchEvent`, `Touch` and `TouchList` interfaces are always exposed; the legacy
//! touch event APIs (`ontouch*`, `createEvent("TouchEvent")`) only with a touch device, which
//! `navigator.maxTouchPoints` and the `pointer`/`hover` media features report alike.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_style::InputDevices;

fn page(net: &NetThread, input: InputDevices) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        render: Some(RenderSetup::new().unwrap()),
        input,
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body>x",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// What a page sees of touch: the classic `'ontouchstart' in window` test, the handlers on every
/// target of GlobalEventHandlers, `createEvent("TouchEvent")`, the interfaces, `maxTouchPoints`
/// and the media features.
const PROBE: &str = "
    const legacy = ['ontouchstart', 'ontouchmove', 'ontouchend', 'ontouchcancel'];
    const targets = [window, document, HTMLElement.prototype, SVGElement.prototype, Document.prototype];
    let created;
    try { created = document.createEvent('TouchEvent') instanceof TouchEvent; }
    catch (e) { created = e.name; }
    [
      'ontouchstart' in window,
      targets.map(t => legacy.map(h => h in t).join()).join(),
      typeof TouchEvent, typeof Touch, typeof TouchList,
      created,
      navigator.maxTouchPoints,
      ['(pointer: fine)', '(pointer: coarse)', '(hover: hover)', '(any-pointer: coarse)']
        .map(q => matchMedia(q).matches).join(),
    ].join('|')";

#[test]
fn without_a_touch_device_the_legacy_apis_are_hidden_and_touch_event_is_exposed() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, InputDevices::MOUSE);
    let r = eval(&mut p, PROBE);
    let none = vec!["false"; 20].join(",");
    assert_eq!(
        r,
        format!(
            "false|{none}|function|function|function|NotSupportedError|0|true,false,true,false"
        )
    );
}

#[test]
fn with_a_touch_device_the_legacy_apis_are_exposed() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let touch = InputDevices {
        mouse: false,
        max_touch_points: 10,
    };
    let mut p = page(&net, touch);
    let r = eval(&mut p, PROBE);
    let all = vec!["true"; 20].join(",");
    assert_eq!(
        r,
        format!("true|{all}|function|function|function|true|10|false,true,false,true")
    );
}

#[test]
fn touch_event_carries_its_init() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, InputDevices::MOUSE);
    let r = eval(
        &mut p,
        "const t1 = new Touch({identifier: 7, target: document.body, clientX: 3, pageY: 4,
                               force: 0.5, touchType: 'stylus'});
         const t2 = new Touch({identifier: 8, target: document});
         const e = new TouchEvent('touchstart', {touches: [t1, t2], targetTouches: [t1],
                                  altKey: true, modifierCapsLock: true, bubbles: true});
         let required;
         try { new Touch({identifier: 1}); } catch (x) { required = x.name; }
         [t1.identifier, t1.target === document.body, t1.clientX, t1.pageY, t1.force, t1.touchType,
          t2.touchType, e.touches.length, e.touches[0] === t1, e.touches.item(1) === t2,
          e.touches === e.touches, e.targetTouches.length, e.changedTouches.length,
          e.altKey, e.ctrlKey, e.getModifierState('CapsLock'), e.bubbles,
          e instanceof UIEvent, required].join()",
    );
    assert_eq!(
        r,
        "7,true,3,4,0.5,stylus,direct,2,true,true,true,1,0,true,false,true,true,true,TypeError"
    );
}
