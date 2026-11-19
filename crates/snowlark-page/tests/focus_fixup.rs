// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML "update the rendering": a document whose focused element stopped being a focusable area
//! (disabled, not rendered, invisible, inert, no longer focusable) focuses its viewport at the next
//! rendering update, firing `blur`; nothing changes before it.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

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

const PAGE: &str = "<!doctype html><button id=b>b</button><div id=d tabindex=0>d</div>\
    <div id=wrap><button id=w>w</button></div><script>var blurred = 0;\
    for (const el of [b, d, w]) el.addEventListener('blur', () => blurred++);</script>";

/// Focuses `id`, runs `change`, and returns the active element's id (or the tag) right after it and
/// after a rendering update, with the number of `blur` events.
fn fixup(id: &str, change: &str) -> (String, String) {
    let mut page = page_with(PAGE);
    eval(&mut page, &format!("{id}.focus()"));
    let before = eval(
        &mut page,
        &format!("{change}; document.activeElement.id || document.activeElement.localName"),
    );
    page.update_the_rendering();
    let after = eval(
        &mut page,
        "(document.activeElement.id || document.activeElement.localName) + ',' + blurred",
    );
    (before, after)
}

#[test]
fn a_focused_element_that_stops_being_focusable_loses_focus_at_the_rendering_update() {
    for (id, change) in [
        ("b", "b.disabled = true"),
        ("b", "b.hidden = true"),
        ("b", "b.style.visibility = 'hidden'"),
        ("d", "d.removeAttribute('tabindex')"),
        ("w", "wrap.inert = true"),
        ("w", "wrap.style.display = 'none'"),
    ] {
        let (before, after) = fixup(id, change);
        assert_eq!(before, id, "{change}: unchanged until the rendering update");
        assert_eq!(after, "body,1", "{change}");
    }
}

#[test]
fn a_focused_element_that_stays_focusable_keeps_focus() {
    let (before, after) = fixup("b", "b.textContent = 'changed'");
    assert_eq!((before.as_str(), after.as_str()), ("b", "b,0"));
}
