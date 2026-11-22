// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A trusted `wheel` event is also delivered to `mousewheel` listeners: at each object in its path
//! with no `wheel` listener, its `mousewheel` listeners are invoked with the event's type set to
//! `mousewheel`, as DOM §2.9 "invoke" step 7 does for the prefixed animation and transition names.
//! Pages that pick the legacy name by browser (mapping libraries among them) depend on it.

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

const PAGE: &str = "<!doctype html><style>body{margin:0}div{position:absolute;left:0;top:0;\
    width:400px;height:300px}</style><div id=outer><div id=inner></div></div><script>var log = [];\
    inner.addEventListener('mousewheel', e => { log.push('inner:' + e.type + ':' + e.wheelDeltaY + ':' + e.cancelable); e.preventDefault(); });\
    outer.addEventListener('wheel', e => log.push('outer:' + e.type + ':' + e.defaultPrevented));\
    outer.addEventListener('mousewheel', e => log.push('outer-legacy:' + e.type));\
    document.addEventListener('mousewheel', e => log.push('document:' + e.type + ':' + e.defaultPrevented));\
    addEventListener('wheel', e => log.push('window:' + e.type));\
    </script>";

#[test]
fn a_trusted_wheel_reaches_mousewheel_listeners_where_no_wheel_listener_is() {
    let mut page = page_with(PAGE);
    page.wheel(
        Point::new(100.0, 100.0),
        Point::new(0.0, 100.0),
        Modifiers::default(),
    );
    // (inner and document listen for mousewheel only, outer and window for wheel: outer's
    // mousewheel listener is not invoked)
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "inner:mousewheel:-120:true outer:wheel:true document:mousewheel:true window:wheel"
    );
}

#[test]
fn a_dispatched_wheel_event_reaches_no_mousewheel_listener() {
    let mut page = page_with(PAGE);
    assert_eq!(
        eval(
            &mut page,
            "inner.dispatchEvent(new WheelEvent('wheel', {deltaY: 100, bubbles: true, cancelable: true})); log.join(' ')"
        ),
        "outer:wheel:false window:wheel"
    );
}
