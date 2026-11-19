// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The HTML Editing APIs' `execCommand`: `delete`, `forwardDelete`, `insertParagraph`,
//! `insertLineBreak`, `insertText` and `defaultParagraphSeparator`, with cases from WPT's
//! `editing/data/*.js`.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

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

/// Puts `html` in the editing host, collapses the selection where `caret` (a JS expression over
/// `d`, the host, giving `[node, offset]`) says, runs `commands` (`[name, value]` pairs) and
/// returns the host's markup.
fn run(page: &mut Page, html: &str, caret: &str, commands: &str) -> String {
    eval(
        page,
        &format!(
            r#"(() => {{
            const d = document.getElementById("host");
            d.innerHTML = {html:?};
            const [n, o] = {caret};
            getSelection().collapse(n, o);
            for (const [c, v] of {commands}) document.execCommand(c, false, v);
            return d.innerHTML;
            }})()"#
        ),
    )
}

#[test]
fn delete_merges_blocks_and_removes_line_breaks() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<div id=host contenteditable></div>");
    // ["<p>foo</p>[]bar", [["delete",""]], "<p>foo{}bar</p>"]
    assert_eq!(
        run(
            &mut page,
            "<p>foo</p>bar",
            "[d.childNodes[1], 0]",
            r#"[["delete", ""]]"#
        ),
        "<p>foobar</p>"
    );
    // ["foo<hr>[]bar", [["delete",""]], "foo{}bar"]
    assert_eq!(
        run(
            &mut page,
            "foo<hr>bar",
            "[d.lastChild, 0]",
            r#"[["delete", ""]]"#
        ),
        "foobar"
    );
    // forwardDelete at a paragraph's end merges the next one.
    assert_eq!(
        run(
            &mut page,
            "<p>foo</p><p>bar</p>",
            "[d.firstChild.firstChild, 3]",
            r#"[["forwarddelete", ""]]"#
        ),
        "<p>foobar</p>"
    );
}

#[test]
fn insert_paragraph_splits_the_container() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<div id=host contenteditable></div>");
    // ["<p>foo[]bar</p>", [["defaultparagraphseparator","div"],["insertparagraph",""]], ...]
    assert_eq!(
        run(
            &mut page,
            "<p>foobar</p>",
            "[d.firstChild.firstChild, 3]",
            r#"[["defaultparagraphseparator", "div"], ["insertparagraph", ""]]"#
        ),
        "<p>foo</p><p>bar</p>"
    );
    // ["<ol><li>foo[]bar</ol>", [["insertparagraph",""]], "<ol><li>foo</li><li>{}bar</li></ol>"]
    assert_eq!(
        run(
            &mut page,
            "<ol><li>foobar</li></ol>",
            "[d.firstChild.firstChild.firstChild, 3]",
            r#"[["insertparagraph", ""]]"#
        ),
        "<ol><li>foo</li><li>bar</li></ol>"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.execCommand('defaultParagraphSeparator', false, 'P') + ',' + \
             document.queryCommandValue('defaultparagraphseparator')"
        ),
        "true,p"
    );
}

#[test]
fn insert_line_break_and_text() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<div id=host contenteditable></div>");
    // ["<p>foo[]</p>", [["insertlinebreak",""]], "<p>foo<br>{}<br></p>"]
    assert_eq!(
        run(
            &mut page,
            "<p>foo</p>",
            "[d.firstChild.firstChild, 3]",
            r#"[["insertlinebreak", ""]]"#
        ),
        "<p>foo<br><br></p>"
    );
    // ["foo[] ", [["inserttext"," "]], "foo&nbsp;"]
    assert_eq!(
        run(
            &mut page,
            "foo ",
            "[d.firstChild, 3]",
            r#"[["inserttext", " "]]"#
        ),
        "foo&nbsp;"
    );
}
