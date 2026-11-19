// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The HTML Editing APIs' inline formatting commands (`bold`, `fontSize`, `foreColor`,
//! `createLink`, `removeFormat`...), their states, values and overrides, and the justify
//! commands, with cases from WPT's `editing/data/*.js`.

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

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Puts `html` in the editing host, selects from `start` to `end` (JS expressions over `d`, the
/// host, each giving `[node, offset]`), runs `commands` (`[name, value]` pairs) and returns the
/// host's markup, then `|` and `query` (a JS expression evaluated after the commands).
fn run(page: &mut Page, html: &str, start: &str, end: &str, commands: &str, query: &str) -> String {
    eval(
        page,
        &format!(
            r#"(() => {{
            const d = document.getElementById("host");
            d.innerHTML = {html:?};
            const r = document.createRange();
            r.setStart(...{start});
            r.setEnd(...{end});
            getSelection().removeAllRanges();
            getSelection().addRange(r);
            // (the queries editing/run's tests make before the commands)
            for (const [c] of {commands}) {{
                document.queryCommandIndeterm(c);
                document.queryCommandState(c);
                document.queryCommandValue(c);
            }}
            for (const [c, v] of {commands}) document.execCommand(c, false, v);
            return d.innerHTML + "|" + ({query});
            }})()"#
        ),
    )
}

#[test]
fn bold_wraps_and_unwraps_the_selection() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net);
    // ["foo[bar]baz", [["stylewithcss","false"],["bold",""]], "foo<b>[bar]</b>baz"]
    assert_eq!(
        run(
            &mut page,
            "foobarbaz",
            "[d.firstChild, 3]",
            "[d.firstChild, 6]",
            r#"[["stylewithcss", "false"], ["bold", ""]]"#,
            "document.queryCommandState('bold')"
        ),
        "foo<b>bar</b>baz|true"
    );
    // ["foo[<span style=\"font-weight: bold\">bar</span>]baz", [["stylewithcss","false"],
    // ["bold",""]], "foo[bar]baz", ... "bold":[false,true,"",false,false,""]]
    assert_eq!(
        run(
            &mut page,
            "foo<span style=\"font-weight: bold\">bar</span>baz",
            "[d.firstChild, 3]",
            "[d.lastChild, 0]",
            r#"[["stylewithcss", "false"], ["bold", ""]]"#,
            "document.queryCommandState('bold')"
        ),
        "foobarbaz|false"
    );
    // ["abc<i>[def]</i>ghi", [["stylewithcss","false"],["bold",""]], "abc<i><b>[def]</b></i>ghi"]:
    // an element the selection is inside keeps its place
    assert_eq!(
        run(
            &mut page,
            "abc<i>def</i>ghi",
            "[d.childNodes[1].firstChild, 0]",
            "[d.childNodes[1].firstChild, 3]",
            r#"[["stylewithcss", "false"], ["bold", ""]]"#,
            "''"
        ),
        "abc<i><b>def</b></i>ghi|"
    );
    // With the CSS styling flag, a span: ["foo[bar]baz", [["stylewithcss","true"],["bold",""]],
    // "foo<span style=\"font-weight:bold\">[bar]</span>baz"]
    assert_eq!(
        run(
            &mut page,
            "foobarbaz",
            "[d.firstChild, 3]",
            "[d.firstChild, 6]",
            r#"[["stylewithcss", "true"], ["bold", ""]]"#,
            "''"
        ),
        "foo<span style=\"font-weight: bold;\">bar</span>baz|"
    );
}

#[test]
fn font_size_and_colors_use_font_elements() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net);
    // ["foo[bar]baz", [["stylewithcss","false"],["fontsize","4"]],
    // "foo<font size=\"4\">[bar]</font>baz", ... "fontsize":[false,false,"3",false,false,"4"]]
    assert_eq!(
        run(
            &mut page,
            "foobarbaz",
            "[d.firstChild, 3]",
            "[d.firstChild, 6]",
            r#"[["stylewithcss", "false"], ["fontsize", "4"]]"#,
            "document.queryCommandValue('fontsize') + document.queryCommandIndeterm('fontsize')"
        ),
        "foo<font size=\"4\">bar</font>baz|4false"
    );
    // ["foo[bar]baz", [["stylewithcss","false"],["forecolor","#0000FF"]],
    // "foo<font color=\"#0000ff\">[bar]</font>baz"]
    assert_eq!(
        run(
            &mut page,
            "foobarbaz",
            "[d.firstChild, 3]",
            "[d.firstChild, 6]",
            r##"[["stylewithcss", "false"], ["forecolor", "#0000FF"]]"##,
            "document.queryCommandValue('forecolor')"
        ),
        "foo<font color=\"#0000ff\">bar</font>baz|rgb(0, 0, 255)"
    );
}

#[test]
fn links_are_created_and_removed() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net);
    // ["foo[bar]baz", [["createlink","http://www.google.com/"]],
    // "foo<a href=\"http://www.google.com/\">[bar]</a>baz"]
    assert_eq!(
        run(
            &mut page,
            "foobarbaz",
            "[d.firstChild, 3]",
            "[d.firstChild, 6]",
            r#"[["createlink", "http://www.google.com/"]]"#,
            "''"
        ),
        "foo<a href=\"http://www.google.com/\">bar</a>baz|"
    );
    // ["<a href=http://www.google.com/>foo[bar]baz</a>", [["unlink",""]], "foo[bar]baz"]
    assert_eq!(
        run(
            &mut page,
            "<a href=http://www.google.com/>foobarbaz</a>",
            "[d.firstChild.firstChild, 3]",
            "[d.firstChild.firstChild, 6]",
            r#"[["unlink", ""]]"#,
            "''"
        ),
        "foobarbaz|"
    );
}

#[test]
fn remove_format_strips_inline_formatting() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net);
    // ["foo<b>b[a]r</b>baz", [["removeformat",""]], "foo<b>b</b>[a]<b>r</b>baz"]
    assert_eq!(
        run(
            &mut page,
            "foo<b>bar</b>baz",
            "[d.childNodes[1].firstChild, 1]",
            "[d.childNodes[1].firstChild, 2]",
            r#"[["removeformat", ""]]"#,
            "''"
        ),
        "foo<b>b</b>a<b>r</b>baz|"
    );
}

#[test]
fn justify_wraps_the_block_in_an_aligned_div() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net);
    // ["foo[]bar<p>extra", [["defaultparagraphseparator","div"],["justifycenter",""]],
    // "<div style=\"text-align:center\">foo[]bar</div><p>extra</p>", ... "justifycenter":
    // [false,false,"left",false,true,"center"]]
    assert_eq!(
        run(
            &mut page,
            "foobar<p>extra</p>",
            "[d.firstChild, 3]",
            "[d.firstChild, 3]",
            r#"[["defaultparagraphseparator", "div"], ["justifycenter", ""]]"#,
            "document.queryCommandState('justifycenter') + document.queryCommandValue('justifycenter')"
        ),
        "<div style=\"text-align: center\">foobar</div><p>extra</p>|truecenter"
    );
    // ["<center><p>[foo]<p>bar</center><p>extra", [["justifycenter",""]],
    // "<center><p>[foo]</p><p>bar</p></center><p>extra</p>"]: already centred
    assert_eq!(
        run(
            &mut page,
            "<center><p>foo</p><p>bar</p></center><p>extra</p>",
            "[d.firstChild.firstChild.firstChild, 0]",
            "[d.firstChild.firstChild.firstChild, 3]",
            r#"[["justifycenter", ""]]"#,
            "''"
        ),
        "<center><p>foo</p><p>bar</p></center><p>extra</p>|"
    );
}

#[test]
fn a_collapsed_bold_formats_the_next_text_typed() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net);
    // ["foo[]bar", [["bold",""],["inserttext","a"]], "foo<b>a[]</b>bar"]: the state override
    assert_eq!(
        run(
            &mut page,
            "foobar",
            "[d.firstChild, 3]",
            "[d.firstChild, 3]",
            r#"[["stylewithcss", "false"], ["bold", ""], ["inserttext", "a"]]"#,
            "''"
        ),
        "foo<b>a</b>bar|"
    );
}
