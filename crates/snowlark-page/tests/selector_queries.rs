// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The DOM's selector queries (`matches`, `closest`, `querySelector(All)`, Selectors §17, DOM
//! §4.2.6): each selector list is parsed once per page, and the quick check on each selector's
//! rightmost id, class or type never rules out an element the full match accepts.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page(html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        viewport: (800, 600),
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

/// Selectors 4 §compat: a `::-webkit-` pseudo-element is valid, and user action pseudo-classes may
/// follow it; another pseudo-element may not.
#[test]
fn webkit_pseudo_elements_take_user_action_pseudo_classes() {
    let mut p = page("<!doctype html><span>a</span>");
    assert_eq!(
        eval(
            &mut p,
            "const ok = s => { try { document.querySelectorAll(s); return 1 } catch (e) { return 0 } }; \
             [ok('span::-webkit-x'), ok('span::-webkit-x:hover'), ok('span::-webkit-x:active'), \
              ok('span::-webkit-x::before'), ok('span::-webkitx')].join()"
        ),
        "1,1,1,0,0"
    );
}

#[test]
fn a_selector_list_is_parsed_once_per_page() {
    let mut p =
        page("<!doctype html><ul class=list><li class=item><span class=label>a</span></ul>");
    let before = p.engine().selector_lists_parsed();
    assert_eq!(
        eval(
            &mut p,
            "const s = document.querySelector('span'); let n = 0; \
             for (let i = 0; i < 100; i++) { \
               if (s.matches('.list .item > .label')) n++; \
               if (s.closest('.list')) n++; \
               if (document.querySelector('ul li .label')) n++; \
               n += document.querySelectorAll('ul li .label').length; \
             } n"
        ),
        "400"
    );
    let parsed = p.engine().selector_lists_parsed() - before;
    assert_eq!(
        parsed, 4,
        "{parsed} lists parsed for 4 distinct selector strings"
    );
    // An invalid list is not kept: it throws every time.
    assert_eq!(
        eval(
            &mut p,
            "let t = 0; for (let i = 0; i < 3; i++) { \
               try { document.querySelector('a[') } catch (e) { if (e.name == 'SyntaxError') t++ } \
             } t"
        ),
        "3"
    );
}

#[test]
fn the_quick_check_keeps_every_match() {
    // Quirks mode: class and id selectors match ASCII case-insensitively.
    let mut p = page(
        "<div id=Main class='A  b'><p class=x>p</p></div>\
         <svg><foreignObject id=fo></foreignObject></svg>",
    );
    for (js, want) in [
        ("document.querySelectorAll('.a').length", "1"),
        ("document.querySelectorAll('.B').length", "1"),
        ("document.querySelectorAll('#main').length", "1"),
        (
            "document.querySelector('p').closest('#MAIN') !== null",
            "true",
        ),
        ("document.querySelector('p').matches('.A .X')", "true"),
        // (a type selector matches an SVG element's mixed-case name as written: only HTML elements
        // compare lowercased)
        ("document.querySelectorAll('foreignObject').length", "1"),
        ("document.querySelectorAll('foreignobject').length", "0"),
        // (a list with a selector that has no key: every element is matched)
        (
            "document.querySelectorAll('.x, :not(div)').length > 1",
            "true",
        ),
        ("document.querySelectorAll('.nothing').length", "0"),
    ] {
        assert_eq!(eval(&mut p, js), want, "{js}");
    }
    // No-quirks mode: case-sensitive, and the quick check does not loosen the match.
    let mut p = page("<!doctype html><div id=Main class=A><p class=x>p</p></div>");
    for (js, want) in [
        ("document.querySelectorAll('.a').length", "0"),
        ("document.querySelectorAll('.A').length", "1"),
        ("document.querySelectorAll('#main').length", "0"),
        (
            "document.querySelector('p').closest('#Main') !== null",
            "true",
        ),
        ("document.querySelector('p').matches('.a .x')", "false"),
    ] {
        assert_eq!(eval(&mut p, js), want, "{js}");
    }
}
