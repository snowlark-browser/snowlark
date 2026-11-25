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

/// As [`run`], with the selection from `start` to `end` (JS expressions giving `[node, offset]`).
fn run_range(page: &mut Page, html: &str, start: &str, end: &str, commands: &str) -> String {
    eval(
        page,
        &format!(
            r#"(() => {{
            const d = document.getElementById("host");
            d.innerHTML = {html:?};
            const [sn, so] = {start};
            const [en, eo] = {end};
            getSelection().setBaseAndExtent(sn, so, en, eo);
            for (const [c, v] of {commands}) document.execCommand(c, false, v);
            return d.innerHTML;
            }})()"#
        ),
    )
}

/// Deleting the empty line before a child block leaves the block's first line in it; deleting
/// nothing visible but the boundary still merges; and a line moving next to a page-declared
/// `white-space` keeps its own (WPT `editing/other/delete-without-unwrapping-first-line-of-
/// child-block`, `join-different-white-space-style-left-paragraph-and-right-line`).
#[test]
fn deleting_the_line_before_a_child_block_keeps_the_block_whole() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<div id=host contenteditable></div>");
    assert_eq!(
        run_range(
            &mut page,
            "abc<br><br><div id=\"child\">def<br>ghi</div>",
            "[d, 2]",
            "[d.lastChild.firstChild, 0]",
            r#"[["delete", ""], ["inserttext", "X"]]"#
        ),
        "abc<br><div id=\"child\">Xdef<br>ghi</div>"
    );
    assert_eq!(
        run_range(
            &mut page,
            "foo<br><p>bar</p>",
            "[d, 2]",
            "[d.lastChild.firstChild, 0]",
            r#"[["delete", ""]]"#
        ),
        "foobar"
    );
    assert_eq!(
        run(
            &mut page,
            "<div style=\"white-space:pre-line\">abc</div>def",
            "[d.lastChild, 0]",
            r#"[["delete", ""]]"#
        ),
        "<div style=\"white-space:pre-line\">abc<span style=\"white-space: normal;\">def</span></div>"
    );
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

/// Joining two blocks moves the first line of the second (up to a `br`, a line feed its
/// `white-space` keeps, or a block) and keeps the line's `white-space` when the page's style gave
/// it one the destination does not have (WPT `editing/other/join-different-white-space-style-*`,
/// `join-pre-and-other-block`).
#[test]
fn joining_blocks_moves_one_line_and_keeps_its_white_space() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<style>.preWrap { white-space: pre-wrap }</style><div id=host contenteditable></div>",
    );
    assert_eq!(
        run(
            &mut page,
            "<p>foo</p><p>bar<br>baz</p>",
            "[d.lastChild.firstChild, 0]",
            r#"[["delete", ""]]"#
        ),
        "<p>foobar</p><p>baz</p>"
    );
    assert_eq!(
        run(
            &mut page,
            "<div style=\"white-space:normal\">abc</div><div style=\"white-space:pre\">def\nghi</div>",
            "[d.firstChild.firstChild, 3]",
            r#"[["forwarddelete", ""]]"#
        ),
        "<div style=\"white-space:normal\">abc<span style=\"white-space: pre;\">def</span></div>\
         <div style=\"white-space:pre\">ghi</div>"
    );
    // (a `<pre>`'s own white-space comes from the user agent's style sheet: not kept)
    assert_eq!(
        run(
            &mut page,
            "<div>abc</div><pre>def\nghi</pre>",
            "[d.firstChild.firstChild, 3]",
            r#"[["forwarddelete", ""]]"#
        ),
        "<div>abcdef</div><pre>ghi</pre>"
    );
    // (an inline element the line ends in is split; one moved element takes the style itself)
    assert_eq!(
        run(
            &mut page,
            "<div>abc</div><span class=\"preWrap\">def\nghi</span>",
            "[d.lastChild.firstChild, 0]",
            r#"[["delete", ""]]"#
        ),
        "<div>abc<span class=\"preWrap\" style=\"white-space: pre-wrap;\">def</span></div>\
         <span class=\"preWrap\">ghi</span>"
    );
    // (list items still merge whole)
    assert_eq!(
        run(
            &mut page,
            "<ol><li>foo</li><li>bar<br>baz</li></ol>",
            "[d.firstChild.lastChild.firstChild, 0]",
            r#"[["delete", ""]]"#
        ),
        "<ol><li>foo<br>bar<br>baz</li></ol>"
    );
}

/// `insertLineBreak` inserts a line feed where the white space keeps line feeds (but `br`s in a
/// `pre`, as browsers do), and runs of white space are written as browsers write them: a run of
/// two or more starts with a no-break space, and a split paragraph's two halves are rebalanced
/// (WPT `editing/other/insertlinebreak-with-white-space-style`, `editing/whitespaces/chrome-compat`).
#[test]
fn line_feeds_where_kept_and_white_space_runs_as_browsers_write_them() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<div id=host contenteditable></div>");
    assert_eq!(
        run(
            &mut page,
            "<div style=\"white-space:pre-wrap\">abc</div>",
            "[d.firstChild.firstChild, 1]",
            r#"[["insertlinebreak", ""]]"#
        ),
        "<div style=\"white-space:pre-wrap\">a\nbc</div>"
    );
    assert_eq!(
        run(
            &mut page,
            "<div style=\"white-space:pre-wrap\">abc</div>",
            "[d.firstChild.firstChild, 3]",
            r#"[["insertlinebreak", ""]]"#
        ),
        "<div style=\"white-space:pre-wrap\">abc\n\n</div>"
    );
    assert_eq!(
        run(
            &mut page,
            "<pre>foobar</pre>",
            "[d.firstChild.firstChild, 3]",
            r#"[["insertlinebreak", ""]]"#
        ),
        "<pre>foo<br>bar</pre>"
    );
    assert_eq!(
        run(
            &mut page,
            "a\u{a0}\u{a0}\u{a0}",
            "[d.firstChild, 4]",
            r#"[["delete", ""]]"#
        ),
        "a&nbsp;&nbsp;"
    );
    assert_eq!(
        run(
            &mut page,
            "<div>a\u{a0}\u{a0}\u{a0} b</div>",
            "[d.firstChild.firstChild, 3]",
            r#"[["insertparagraph", ""]]"#
        ),
        "<div>a&nbsp;&nbsp;</div><div>&nbsp; b</div>"
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
