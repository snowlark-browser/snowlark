// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML 7.10.4 the Location `hash` setter and 7.4.2.3.3 fragment navigation: the same fragment
//! again navigates nowhere (no scroll, no history entry), an empty one is a fragment navigation to
//! the top.

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

#[test]
fn setting_the_same_hash_again_navigates_nowhere() {
    let mut page = page_with(
        "<!doctype html><body style='margin:0'><div style='height:3000px'></div>\
         <p id='te&lt;st'>target</p><div style='height:3000px'></div>\
         <script>window.changes = 0; onhashchange = () => changes++;</script>",
    );
    eval(&mut page, "location.hash = 'te<st'; ''");
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_ne!(eval(&mut page, "String(scrollY)"), "0");
    let entries = eval(&mut page, "String(history.length)");
    // the same fragment, written three ways: nothing happens
    eval(
        &mut page,
        "scroll(0, 0); location.hash = 'te<st'; location.hash = '#te<st'; location.hash = 'te%3Cst'; ''",
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "String(scrollY)"), "0");
    assert_eq!(eval(&mut page, "String(history.length)"), entries);
    assert_eq!(eval(&mut page, "String(changes)"), "1");
    // an empty fragment: `#` stays on the URL, and the page goes to the top
    eval(&mut page, "location.hash = 'te<st'; ''");
    assert!(page.run_until_settled(Duration::from_secs(5)));
    eval(&mut page, "location.hash = ''; ''");
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "location.href"), "http://example.test/#");
    assert_eq!(eval(&mut page, "String(scrollY)"), "0");
}

/// HTML "shared history push/replace state steps" step 4: an empty-string URL keeps the
/// document's URL, fragment included, and is not resolved against a different base URL.
#[test]
fn push_and_replace_state_with_an_empty_url_keep_the_url() {
    let mut page = page_with("<!doctype html><base href='/the-base'><p>x</p>");
    let r = eval(
        &mut page,
        "location.hash = 'keep'; history.pushState(1, '', ''); const a = location.href; \
         history.replaceState(2, '', ''); [a, location.href, history.state].join()",
    );
    assert_eq!(r, "http://example.test/#keep,http://example.test/#keep,2");
}
