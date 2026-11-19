// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Panics that page content could trigger, each pinned by a test that runs the content.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn run(html: &str, script: &str) -> String {
    let mut page = Page::new(PageOptions::default());
    page.load_html(html, &url::Url::parse("http://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)));
    page.eval(script, "t.js").unwrap()
}

/// DOM Parsing "insert adjacent" steps: when the parent is a Document, the context is a new body
/// element; the DOM borrow taken to find the parent must not be held while that element is created.
#[test]
fn before_html_on_the_document_element_uses_a_body_context() {
    let r = run(
        "<!doctype html><p id=a>x</p>",
        r#"
        // (a Comment directly under a Document: the context is a new body element, as the spec says)
        const doc = document.implementation.createHTMLDocument();
        const c = doc.createComment('c');
        doc.insertBefore(c, doc.firstChild);
        let name = 'none';
        try { c.beforeHTML('<b>y</b>'); } catch (e) { name = e.name; }
        let name2 = 'none';
        try { document.documentElement.beforeHTML('<b>y</b>'); } catch (e) { name2 = e.name; }
        [name, name2, document.childNodes.length].join('|')
        "#,
    );
    // (an element cannot be inserted beside the document element: the insertion itself throws)
    assert_eq!(r, "HierarchyRequestError|HierarchyRequestError|2");
}

/// A wrapper's handle is a node id (with a generation) or an object index; code that asks the object
/// table about a handle must first brand-check for the interface it wants. `eval(node)` reaches the
/// code generation hook with a DOM node, whose handle once indexed the object table (index 2^32+n).
#[test]
fn eval_of_a_node_or_other_object_returns_it() {
    let r = run(
        "<!doctype html><div id=a></div>",
        r#"
        const d = document.getElementById('a');
        [eval(d) === d, eval(window) === window, eval(document) === document,
         eval(new Blob(['x'])) instanceof Blob, eval(trustedTypes.emptyScript) === undefined,
         trustedTypes.isScript(d), trustedTypes.isHTML(window)].join('|')
        "#,
    );
    assert_eq!(r, "true|true|true|true|true|false|false");
}

/// A grapheme cluster too large to shape leaves a text item with no clusters (the shaper's error
/// path stores an empty `ShapedText`); laying that line out indexed `clusters[0]` of nothing.
#[test]
fn layout_of_an_unshapable_text_item_does_not_panic() {
    let r = run(
        "<!doctype html><div id=target></div>",
        r#"
        target.textContent = 'e' + '́'.repeat(35000) + 'X';
        typeof target.offsetWidth
        "#,
    );
    assert_eq!(r, "number");
}
