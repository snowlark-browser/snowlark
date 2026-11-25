// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! URLPattern through the page: the `urlpattern` library's parser and matcher with V8's regular
//! expressions behind it (the ECMAScript syntax: look-ahead, back-references, named groups).

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page() -> Page {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html>",
        &url::Url::parse("https://example.test/").unwrap(),
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

#[test]
fn components_groups_and_inputs() {
    let mut p = page();
    let r = eval(
        &mut p,
        "const u = new URLPattern({ pathname: '/books/:id/:rest*' });\
         const m = u.exec('https://x.test/books/12/a/b?q=1', undefined);\
         JSON.stringify([u.pathname, u.protocol, u.hasRegExpGroups, m.pathname.groups, m.inputs,\
           m.search.input, Object.keys(m.hostname.groups)])",
    );
    assert_eq!(
        r,
        r#"["/books/:id/:rest*","*",false,{"id":"12","rest":"a/b"},["https://x.test/books/12/a/b?q=1"],"q=1",["0"]]"#
    );
}

#[test]
fn unnamed_groups_keep_their_order_and_missing_ones_are_undefined() {
    let mut p = page();
    let r = eval(
        &mut p,
        "const u = new URLPattern({ pathname: '/(a)/:x?/(b)?' });\
         const g = u.exec({ pathname: '/a/y' }).pathname.groups;\
         JSON.stringify([Object.keys(g), g[0], g.x, g[1], '1' in g])",
    );
    assert_eq!(r, r#"[["0","1","x"],"a","y",null,true]"#);
}

#[test]
fn custom_groups_run_as_ecmascript_regular_expressions() {
    let mut p = page();
    // A look-ahead and a look-behind, which the `regex` crate has no syntax for.
    let r = eval(
        &mut p,
        "const u = new URLPattern({ pathname: '/:a((?=x)\\\\w+)' });\
         const v = new URLPattern({ pathname: '/(\\\\w(?<!a)\\\\w)' });\
         [u.hasRegExpGroups, u.test({ pathname: '/xyz' }), u.test({ pathname: '/abc' }),\
          v.test({ pathname: '/ba' }), v.test({ pathname: '/ab' })].join()",
    );
    assert_eq!(r, "true,true,false,true,false");
}

#[test]
fn captures_of_non_ascii_text_are_sliced_at_their_characters() {
    let mut p = page();
    let r = eval(
        &mut p,
        "const u = new URLPattern({ pathname: '/(.*)-(.*)' });\
         const g = u.exec({ pathname: '/\u{e9}\u{1F600}-x\u{4e2d}' }).pathname.groups;\
         JSON.stringify(g)",
    );
    assert_eq!(r, r#"{"0":"%C3%A9%F0%9F%98%80","1":"x%E4%B8%AD"}"#);
}

#[test]
fn ignore_case_and_base_url() {
    let mut p = page();
    let r = eval(
        &mut p,
        "const a = new URLPattern({ pathname: '/ABC/(d+)' }, { ignoreCase: true });\
         const b = new URLPattern('/abc/:x', 'https://example.test/base');\
         [a.test({ pathname: '/abc/DD' }), new URLPattern({ pathname: '/ABC' }).test({ pathname: '/abc' }),\
          b.hostname, b.test('https://example.test/abc/1'), b.test('/abc/1', 'https://example.test'),\
          b.test('https://other.test/abc/1')].join()",
    );
    assert_eq!(r, "true,false,example.test,true,true,false");
}

#[test]
fn bad_patterns_and_inputs_throw_type_errors() {
    let mut p = page();
    let r = eval(
        &mut p,
        "const t = f => { try { f(); return 'no throw' } catch (e) { return e.constructor.name } };\
         [t(() => new URLPattern('(\\\\')), t(() => new URLPattern({ pathname: '/(' })),\
          t(() => new URLPattern({ pathname: '/(?<!a)+' })), t(() => new URLPattern('/relative')),\
          t(() => new URLPattern({ pathname: '/a' }, 'https://example.test/')),\
          t(() => new URLPattern({ pathname: '/:a/:a' })),\
          new URLPattern({ pathname: '/a' }).test({ pathname: '/a' }, undefined),\
          t(() => new URLPattern({ pathname: '/a' }).test({ pathname: '/a' }, 'https://example.test/'))\
         ].join()",
    );
    assert_eq!(
        r,
        "TypeError,TypeError,TypeError,TypeError,TypeError,TypeError,true,TypeError"
    );
}
