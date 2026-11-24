// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Element reflection of `FrozenArray<Element>?` attributes (HTML §2.6.1, "get the attr-associated
//! elements"): the ARIA `…Elements` properties read the elements named by the content attribute until
//! elements are set explicitly, and a changed content attribute drops the explicit ones.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn run(js: &str) -> String {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><div id=a></div><div id=b></div><p id=p></p>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

#[test]
fn the_content_attribute_names_the_elements_until_they_are_set() {
    let got = run("p.setAttribute('aria-labelledby', ' a  missing b a ');\
         const read = p.ariaLabelledByElements.map(e => e.id).join();\
         const none = document.createElement('i').ariaLabelledByElements;\
         p.setAttribute('aria-controls', '');\
         [read, String(none), p.ariaControlsElements.length].join('|')");
    assert_eq!(got, "a,b,a|null|0");
}

#[test]
fn setting_the_property_overrides_the_attribute_and_changing_the_attribute_clears_it() {
    let got = run("p.setAttribute('aria-owns', 'a');\
         p.ariaOwnsElements = [b];\
         const set = [p.ariaOwnsElements[0].id, p.getAttribute('aria-owns')].join();\
         p.setAttribute('aria-owns', 'a');\
         const after = p.ariaOwnsElements.map(e => e.id).join();\
         p.ariaOwnsElements = null;\
         [set, after, p.hasAttribute('aria-owns'), p.ariaOwnsElements].join('|')");
    assert_eq!(got, "b,|a|false|");
}

#[test]
fn an_explicitly_set_element_outside_the_scope_is_not_returned() {
    let got = run(
        "const host = document.createElement('div'); document.body.append(host);\
         const root = host.attachShadow({mode: 'open'});\
         const inner = document.createElement('span'); root.append(inner);\
         a.ariaDetailsElements = [inner, b];\
         a.ariaDetailsElements.map(e => e.id || 'inner').join()",
    );
    assert_eq!(got, "b");
}
