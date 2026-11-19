// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The HTML Editing APIs' block formatting commands: `insertOrderedList` and
//! `insertUnorderedList` (toggle lists, the selection's list state), `indent`, `outdent` and
//! `formatBlock`, with cases from WPT's `editing/data/*.js`.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn rendered(net: &NetThread) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<div id=host contenteditable></div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

/// Puts `html` in the editing host, selects from `start` to `end` (JS expressions over `d`, the
/// host, each giving `[node, offset]`), runs `commands` (`[name, value]` pairs) and returns
/// whether each ran, the host's markup, and `query` evaluated after the commands, joined by `|`.
fn run(page: &mut Page, html: &str, start: &str, end: &str, commands: &str, query: &str) -> String {
    let js = format!(
        r#"(() => {{
        const d = document.getElementById("host");
        d.innerHTML = {html:?};
        const r = document.createRange();
        r.setStart(...{start});
        r.setEnd(...{end});
        getSelection().removeAllRanges();
        getSelection().addRange(r);
        const ran = {commands}.map(([c, v]) => document.execCommand(c, false, v));
        return ran.join(",") + "|" + d.innerHTML + "|" + ({query});
        }})()"#
    );
    match page.eval(&js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

#[test]
fn insert_ordered_list_wraps_paragraphs_in_items() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net);
    // ["<p>[foo<p>bar]<p>baz", [["insertorderedlist",""]], "<ol><li>[foo</li><li>bar]</li></ol><p>baz</p>"]
    assert_eq!(
        run(
            &mut page,
            "<p>foo</p><p>bar</p><p>baz</p>",
            "[d.firstChild.firstChild, 0]",
            "[d.childNodes[1].firstChild, 3]",
            r#"[["insertorderedlist", ""]]"#,
            "[document.queryCommandState('insertorderedlist'), \
             document.queryCommandState('insertunorderedlist')]"
        ),
        "true|<ol><li>foo</li><li>bar</li></ol><p>baz</p>|true,false"
    );
    // The other list type converts the list.
    assert_eq!(
        run(
            &mut page,
            "<ol><li>foo</li><li>bar</li></ol>",
            "[d.firstChild.firstChild.firstChild, 0]",
            "[d.firstChild.lastChild.firstChild, 3]",
            r#"[["insertunorderedlist", ""]]"#,
            "document.queryCommandState('insertunorderedlist')"
        ),
        "true|<ul><li>foo</li><li>bar</li></ul>|true"
    );
    // Inline content becomes one item; a mixed selection is indeterminate.
    assert_eq!(
        run(
            &mut page,
            "foo<ul><li>bar</li></ul>",
            "[d.firstChild, 0]",
            "[d.firstChild, 3]",
            r#"[["insertorderedlist", ""]]"#,
            "document.queryCommandIndeterm('insertorderedlist')"
        ),
        "true|<ol><li>foo</li></ol><ul><li>bar</li></ul>|false"
    );
}

#[test]
fn indent_and_outdent_nest_and_unnest() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net);
    assert_eq!(
        run(
            &mut page,
            "<p>foo</p>",
            "[d.firstChild.firstChild, 1]",
            "[d.firstChild.firstChild, 1]",
            r#"[["indent", ""]]"#,
            "''"
        ),
        "true|<blockquote><p>foo</p></blockquote>|"
    );
    // ["<blockquote><p>foo[bar]</p></blockquote><p>baz</p><p>extra", [["outdent",""]], "<p>foo[bar]</p><p>baz</p><p>extra</p>"]
    assert_eq!(
        run(
            &mut page,
            "<blockquote><p>foobar</p></blockquote><p>baz</p>",
            "[d.firstChild.firstChild.firstChild, 3]",
            "[d.firstChild.firstChild.firstChild, 6]",
            r#"[["outdent", ""]]"#,
            "''"
        ),
        "true|<p>foobar</p><p>baz</p>|"
    );
    // An item indents into a nested list of its own type.
    assert_eq!(
        run(
            &mut page,
            "<ol><li>foo</li><li>bar</li></ol>",
            "[d.firstChild.lastChild.firstChild, 1]",
            "[d.firstChild.lastChild.firstChild, 1]",
            r#"[["indent", ""], ["outdent", ""]]"#,
            "''"
        ),
        "true,true|<ol><li>foo</li><li>bar</li></ol>|"
    );
}

#[test]
fn insert_horizontal_rule_and_image_insert_at_the_selection() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net);
    let caret = "(() => { const r = getSelection().getRangeAt(0); \
                 return r.startContainer.nodeName + ',' + r.startOffset; })()";
    // ["foo[]bar", [["inserthorizontalrule",""]], "foo<hr>{}bar"]
    assert_eq!(
        run(
            &mut page,
            "foobar",
            "[d.firstChild, 3]",
            "[d.firstChild, 3]",
            r#"[["inserthorizontalrule", ""]]"#,
            caret
        ),
        "true|foo<hr>bar|DIV,2"
    );
    // A rule in a paragraph splits it ("fix disallowed ancestors").
    assert_eq!(
        run(
            &mut page,
            "<p>foobarbaz</p>",
            "[d.firstChild.firstChild, 3]",
            "[d.firstChild.firstChild, 3]",
            r#"[["inserthorizontalrule", ""]]"#,
            caret
        ),
        "true|<p>foo</p><hr><p>barbaz</p>|DIV,2"
    );
    // ["foo[bar]baz", [["insertimage","/img/lion.svg"]], "foo<img src=\"/img/lion.svg\">{}baz"]
    assert_eq!(
        run(
            &mut page,
            "foobarbaz",
            "[d.firstChild, 3]",
            "[d.firstChild, 6]",
            r#"[["insertimage", "/img/lion.svg"], ["insertimage", ""]]"#,
            caret
        ),
        "true,false|foo<img src=\"/img/lion.svg\">baz|DIV,2"
    );
}

#[test]
fn format_block_sets_the_block_type() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net);
    // ["<p>[foo<p>bar]<p>baz", [["formatblock","<blockquote>"]], "<blockquote>[foo<br>bar]</blockquote><p>baz</p>"]
    assert_eq!(
        run(
            &mut page,
            "<p>foo</p><p>bar</p><p>baz</p>",
            "[d.firstChild.firstChild, 0]",
            "[d.childNodes[1].firstChild, 3]",
            r#"[["formatblock", "<h1>"]]"#,
            "document.queryCommandValue('formatblock') + ',' + \
             document.queryCommandIndeterm('formatblock')"
        ),
        "true|<h1>foo<br>bar</h1><p>baz</p>|h1,false"
    );
    assert_eq!(
        run(
            &mut page,
            "<p>foo</p>",
            "[d.firstChild.firstChild, 1]",
            "[d.firstChild.firstChild, 1]",
            r#"[["formatblock", "pre"], ["formatblock", "blockquote"]]"#,
            "document.queryCommandValue('formatblock')"
        ),
        "true,false|<pre>foo</pre>|pre"
    );
}
