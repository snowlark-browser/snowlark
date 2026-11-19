// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Selector matching through the TElement/selectors::Element implementation.

mod common;

use common::Page;

const RED: &str = "rgb(255, 0, 0)";
const BLACK: &str = "rgb(0, 0, 0)";

#[test]
fn structural_pseudo_classes() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           li:nth-child(2n+1) { color: red; }
           li:nth-last-child(1) { width: 1px; }
           li:first-child { height: 2px; }
           li:nth-of-type(2) { margin-top: 3px; }
           li:nth-child(2 of .k) { margin-left: 4px; }
           p:only-child { color: red; }
           div:empty { width: 5px; }
           :root { margin-top: 6px; }
         </style>
         <ul><li id=a>1</li><li id=b class=k>2</li><li id=c class=k>3</li><li id=d>4</li></ul>
         <div id=e></div><div id=f><p id=g>x</p></div><div id=h><!-- comment --></div>",
    );
    assert_eq!(p.get("#a", "color"), RED);
    assert_eq!(p.get("#b", "color"), BLACK);
    assert_eq!(p.get("#c", "color"), RED);
    assert_eq!(p.get("#d", "width"), "1px");
    assert_eq!(p.get("#c", "width"), "auto");
    assert_eq!(p.get("#a", "height"), "2px");
    assert_eq!(p.get("#b", "margin-top"), "3px");
    assert_eq!(p.get("#c", "margin-left"), "4px");
    assert_eq!(p.get("#b", "margin-left"), "0px");
    assert_eq!(p.get("#g", "color"), RED);
    assert_eq!(p.get("#e", "width"), "5px");
    assert_eq!(p.get("#h", "width"), "5px");
    assert_eq!(p.get("#f", "width"), "auto");
    assert_eq!(p.get("html", "margin-top"), "6px");
}

#[test]
fn logical_combinations() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           div:not(.x) { width: 1px; }
           :is(#a, #b) span { color: red; }
           :where(#c) span { color: red; }
           span.override { color: rgb(0, 0, 255); }
           div:has(> em) { height: 7px; }
           div:has(+ section) { margin-top: 8px; }
           section:has(.deep) { margin-left: 9px; }
         </style>
         <div id=a class=x><span id=s1>a</span></div>
         <div id=c><span id=s2 class=override>c</span></div>
         <div id=b><span id=s3 class=override>b</span></div>
         <div id=h><em>e</em></div><section><p><i class=deep>d</i></p></section>",
    );
    assert_eq!(p.get("#a", "width"), "auto");
    assert_eq!(p.get("#c", "width"), "1px");
    assert_eq!(p.get("#s1", "color"), RED);
    // :where() has zero specificity, so the class rule wins; :is() takes its argument's.
    assert_eq!(p.get("#s2", "color"), "rgb(0, 0, 255)");
    assert_eq!(p.get("#s3", "color"), RED);
    assert_eq!(p.get("#h", "height"), "7px");
    assert_eq!(p.get("#a", "height"), "auto");
    assert_eq!(p.get("#h", "margin-top"), "8px");
    assert_eq!(p.get("section", "margin-left"), "9px");
}

#[test]
fn attribute_selectors() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           [data-x] { width: 1px; }
           [data-y='a b'] { height: 2px; }
           [data-z~=two] { margin-top: 3px; }
           [lang|=en] { margin-left: 4px; }
           [href^='https:'] { margin-right: 5px; }
           [href$='.pdf'] { padding-top: 6px; }
           [href*=example] { padding-left: 7px; }
           [data-c='ABC' i] { padding-right: 8px; }
           input[type=checkbox] { padding-bottom: 9px; }
         </style>
         <p id=a data-x></p><p id=b data-y='a b'></p><p id=c data-z='one two'></p>
         <p id=d lang=en-US></p><a id=e href='https://example.com/x.pdf'></a>
         <p id=f data-c='abc'></p><input id=g type=CHECKBOX>",
    );
    assert_eq!(p.get("#a", "width"), "1px");
    assert_eq!(p.get("#b", "height"), "2px");
    assert_eq!(p.get("#c", "margin-top"), "3px");
    assert_eq!(p.get("#d", "margin-left"), "4px");
    assert_eq!(p.get("#e", "margin-right"), "5px");
    assert_eq!(p.get("#e", "padding-top"), "6px");
    assert_eq!(p.get("#e", "padding-left"), "7px");
    assert_eq!(p.get("#f", "padding-right"), "8px");
    // `type` is matched ASCII case-insensitively in HTML documents (HTML §4.16.2).
    assert_eq!(p.get("#g", "padding-bottom"), "9px");
}

#[test]
fn combinators_and_ids_and_lang() {
    let p = Page::new(
        "<!DOCTYPE html><html lang=fr><style>
           #x > .y { width: 1px; }
           #x .z { height: 2px; }
           .y + .z { margin-top: 3px; }
           .y ~ .w { margin-left: 4px; }
           :lang(fr) .w { padding-top: 5px; }
           :lang(de) { padding-left: 6px; }
         </style>
         <div id=x><p class=y></p><p class=z></p><p class=q></p><p class=w></p></div>",
    );
    assert_eq!(p.get(".y", "width"), "1px");
    assert_eq!(p.get(".z", "height"), "2px");
    assert_eq!(p.get(".z", "margin-top"), "3px");
    assert_eq!(p.get(".w", "margin-left"), "4px");
    assert_eq!(p.get(".w", "padding-top"), "5px");
    assert_eq!(p.get(".w", "padding-left"), "0px");
}

#[test]
fn dir_follows_the_elements_directionality() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           :dir(rtl) { width: 1px; }
           :dir(ltr) { height: 2px; }
           :dir(sideways) { padding-top: 3px; }
         </style>
         <div dir=rtl><p id=a></p><p id=b dir=ltr></p></div>
         <p id=c dir=auto><b>\u{5d0}</b>a</p><p id=d dir=auto>1 a \u{5d0}</p><bdi id=e>\u{5d0}</bdi>
         <p id=f dir=auto><bdi>\u{5d0}</bdi>a</p><p id=g dir=bogus></p>",
    );
    for (id, rtl) in [
        ("#a", true),
        ("#b", false),
        ("#c", true),
        ("#d", false),
        ("#e", true),
        ("#f", false),
        ("#g", false),
    ] {
        assert_eq!(p.get(id, "width") == "1px", rtl, "{id}");
        assert_eq!(p.get(id, "height") == "2px", !rtl, "{id}");
        assert_eq!(p.get(id, "padding-top"), "0px", "{id}");
    }
}

#[test]
fn form_state_pseudo_classes() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           :disabled { width: 1px; }
           :enabled { width: 2px; }
           :checked { height: 3px; }
           :required { margin-top: 4px; }
           :optional { margin-top: 5px; }
           :read-only { margin-left: 6px; }
           :read-write { margin-left: 7px; }
           :placeholder-shown { padding-top: 8px; }
           details:open { padding-left: 9px; }
           :link { padding-right: 10px; }
         </style>
         <input id=a disabled><input id=b required placeholder=x><input id=c type=checkbox checked>
         <fieldset><optgroup disabled><option id=d>o</option></optgroup></fieldset>
         <textarea id=e readonly></textarea><details open id=f></details><a id=g href=x></a><a id=h></a>",
    );
    assert_eq!(p.get("#a", "width"), "1px");
    assert_eq!(p.get("#b", "width"), "2px");
    assert_eq!(p.get("#c", "height"), "3px");
    assert_eq!(p.get("#b", "margin-top"), "4px");
    assert_eq!(p.get("#a", "margin-top"), "5px");
    assert_eq!(p.get("#a", "margin-left"), "6px");
    assert_eq!(p.get("#b", "margin-left"), "7px");
    assert_eq!(p.get("#e", "margin-left"), "6px");
    assert_eq!(p.get("#b", "padding-top"), "8px");
    assert_eq!(p.get("#f", "padding-left"), "9px");
    assert_eq!(p.get("#g", "padding-right"), "10px");
    assert_eq!(p.get("#h", "padding-right"), "0px");
}

#[test]
fn query_selector_all_matches_in_tree_order() {
    let p = Page::new("<!DOCTYPE html><p class=a></p><div><p class=a></p></div><p class=b></p>");
    let list = p.engine.parse_selector("p.a, div").unwrap();
    let found = p.engine.query_selector_all(&p.dom, &p.data, p.doc, &list);
    let names: Vec<String> = found
        .iter()
        .map(|&n| p.dom.local_name(n).to_string())
        .collect();
    assert_eq!(names, ["p", "div", "p"]);
    assert!(p.engine.parse_selector("p[").is_err());
}
