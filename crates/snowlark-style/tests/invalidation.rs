// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Incremental restyles: element state, attribute and tree changes restyle what their selectors depend on
//! and nothing else.

mod common;

use common::Page;
use snowlark_style::ElementState;

/// A document with `n` sibling sections of three paragraphs each, plus a hover rule on one section.
fn sections(n: usize) -> String {
    let mut html = String::from(
        "<!DOCTYPE html><style>
           section:hover p { color: rgb(1, 2, 3); }
           .on { width: 7px; }
           [data-x=y] > p { height: 3px; }
           li:nth-child(2) { margin-top: 9px; }
           li:last-child { margin-left: 4px; }
           div:has(> .flag) { padding-top: 5px; }
           #e:empty { padding-left: 6px; }
         </style><body>",
    );
    for i in 0..n {
        html.push_str(&format!(
            "<section id=s{i}><p>a</p><p>b</p><p>c</p></section>"
        ));
    }
    html.push_str(
        "<ul id=list><li id=l1>1</li><li id=l2>2</li></ul><div id=d><span id=sp></span></div>",
    );
    html.push_str("<div id=e></div></body>");
    html
}

/// Appends a `div` with `n` `p` children to the body and restyles.
fn insert_paragraphs(p: &mut Page, n: usize) -> snowlark_style::RestyleStats {
    let doc = p.doc;
    let body = p.q("body");
    let div = p.dom.create_element(doc, "div", None).unwrap();
    for _ in 0..n {
        let para = p.dom.create_element(doc, "p", None).unwrap();
        p.dom.append_child(div, para).unwrap();
    }
    p.dom.append_child(body, div).unwrap();
    p.restyle()
}

#[test]
fn a_restyle_of_a_hundred_new_elements_runs_on_the_pool() {
    // A level of a hundred new elements is shared with the pool; a small insertion stays on the page
    // thread.
    let mut p = Page::new(&sections(2));
    let stats = insert_paragraphs(&mut p, 100);
    assert!(stats.restyled >= 100, "{stats:?}");
    assert!(stats.parallel, "{stats:?}");
    let stats = insert_paragraphs(&mut p, 10);
    assert!(stats.restyled >= 10, "{stats:?}");
    assert!(!stats.parallel, "{stats:?}");
}

#[test]
fn a_restyle_that_selectors_spread_over_many_elements_runs_on_the_pool() {
    // One class change restyles every paragraph through a descendant selector: what goes to the pool
    // is decided by the elements the traversal finds dirty, not by the size of the change.
    let mut html =
        String::from("<!DOCTYPE html><style>.wide p { margin-bottom: 2px; }</style><body>");
    for i in 0..200 {
        html.push_str(&format!("<section id=s{i}><p>a</p><p>b</p></section>"));
    }
    let mut p = Page::new(&html);
    let body = p.q("body");
    p.dom.set_attribute(body, "class", "wide").unwrap();
    let stats = p.restyle();
    assert_eq!(p.get("#s150 p", "margin-bottom"), "2px");
    assert!(stats.restyled >= 400, "{stats:?}");
    assert!(stats.parallel, "{stats:?}");

    let s3 = p.q("#s3");
    p.dom.set_attribute(s3, "class", "wide").unwrap();
    let stats = p.restyle();
    assert!(stats.restyled <= 3, "{stats:?}");
    assert!(!stats.parallel, "{stats:?}");
}

#[test]
fn hover_restyles_only_the_hovered_subtree() {
    let mut p = Page::new(&sections(50));
    let s7 = p.q("#s7");
    p.engine
        .set_state(&p.dom, &mut p.data, s7, ElementState::HOVER, true);
    let stats = p.restyle();
    assert_eq!(p.get("#s7 p", "color"), "rgb(1, 2, 3)");
    assert_eq!(p.get("#s8 p", "color"), "rgb(0, 0, 0)");
    // The section and its three paragraphs; the rest of the 150+ elements are untouched.
    assert!(stats.restyled <= 4, "{stats:?}");
    assert!(stats.traversed <= 8, "{stats:?}");
    assert!(!stats.parallel);
    assert!(p.data.state(s7).contains(ElementState::HOVER));

    p.engine
        .set_state(&p.dom, &mut p.data, s7, ElementState::HOVER, false);
    let stats = p.restyle();
    assert_eq!(p.get("#s7 p", "color"), "rgb(0, 0, 0)");
    assert!(stats.restyled <= 4, "{stats:?}");
}

#[test]
fn unrelated_state_changes_restyle_nothing() {
    let mut p = Page::new(&sections(10));
    // No rule depends on :focus of a paragraph: the invalidator finds nothing to do.
    let para = p.q("#s3 p");
    p.engine
        .set_state(&p.dom, &mut p.data, para, ElementState::FOCUS, true);
    let stats = p.restyle();
    assert_eq!(stats.restyled, 0, "{stats:?}");
}

#[test]
fn class_and_attribute_changes() {
    let mut p = Page::new(&sections(20));
    let s5 = p.q("#s5");
    p.dom.set_attribute(s5, "class", "on").unwrap();
    let stats = p.restyle();
    assert_eq!(p.get("#s5", "width"), "7px");
    // Only the section matches selectors again; its children recascade (servo's style difference does
    // not yet tell reset-only changes apart) and nothing else is visited.
    assert_eq!(stats.matched, 1, "{stats:?}");
    assert!(stats.restyled <= 4, "{stats:?}");

    p.dom.set_attribute(s5, "data-x", "y").unwrap();
    let stats = p.restyle();
    assert_eq!(p.get("#s5 p", "height"), "3px");
    assert!(stats.matched <= 4, "{stats:?}");

    p.dom.remove_attribute(s5, "class");
    p.restyle();
    assert_eq!(p.get("#s5", "width"), "auto");

    // The style attribute.
    p.dom
        .set_attribute(s5, "style", "margin-left: 2px")
        .unwrap();
    let stats = p.restyle();
    assert_eq!(p.get("#s5", "margin-left"), "2px");
    assert!(stats.matched <= 1, "{stats:?}");
}

#[test]
fn insertions_and_removals_restyle_dependent_siblings() {
    let mut p = Page::new(&sections(3));
    assert_eq!(p.get("#l2", "margin-top"), "9px");
    assert_eq!(p.get("#l2", "margin-left"), "4px");
    let list = p.q("#list");
    let l1 = p.q("#l1");
    let doc = p.doc;
    let new = p.dom.create_element(doc, "li", None).unwrap();
    p.dom.set_attribute(new, "id", "l0").unwrap();
    p.dom.insert_before(list, new, Some(l1)).unwrap();
    p.restyle();
    // #l1 is now the second child; #l2 is still the last.
    assert_eq!(p.get("#l1", "margin-top"), "9px");
    assert_eq!(p.get("#l2", "margin-top"), "0px");
    assert_eq!(p.get("#l0", "margin-top"), "0px");
    let last = p.dom.create_element(doc, "li", None).unwrap();
    p.dom.append_child(list, last).unwrap();
    p.restyle();
    assert_eq!(p.get("#l2", "margin-left"), "0px");
    p.dom.remove_child(list, new).unwrap();
    p.restyle();
    assert_eq!(p.get("#l2", "margin-top"), "9px");
}

#[test]
fn has_and_empty_invalidation() {
    let mut p = Page::new(&sections(3));
    assert_eq!(p.get("#d", "padding-top"), "0px");
    let sp = p.q("#sp");
    p.dom.set_attribute(sp, "class", "flag").unwrap();
    p.restyle();
    assert_eq!(p.get("#d", "padding-top"), "5px");
    p.dom.remove_child(p.q("#d"), sp).unwrap();
    p.restyle();
    assert_eq!(p.get("#d", "padding-top"), "0px");

    assert_eq!(p.get("#e", "padding-left"), "6px");
    let e = p.q("#e");
    let doc = p.doc;
    let text = p.dom.create_text_node(doc, "x");
    p.dom.append_child(e, text).unwrap();
    p.restyle();
    assert_eq!(p.get("#e", "padding-left"), "0px");
}

#[test]
fn display_none_toggles() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>.hide { display: none; } p { color: rgb(1, 1, 1); }</style>
         <div id=d><p id=p>x</p></div>",
    );
    let d = p.q("#d");
    p.dom.set_attribute(d, "class", "hide").unwrap();
    p.restyle();
    assert!(p.data.styles(p.q("#p")).is_none());
    p.dom.remove_attribute(d, "class");
    p.restyle();
    assert!(p.data.styles(p.q("#p")).is_some());
    assert_eq!(p.get("#p", "color"), "rgb(1, 1, 1)");
}

#[test]
fn parallel_and_sequential_traversals_agree() {
    use snowlark_style::{DeviceConfig, Parallelism, StyleData, StyleEngine};
    let html = sections(400);
    let style = |parallelism| {
        let mut dom = snowlark_dom::Dom::new();
        let doc = dom.parse_html_str(&html, snowlark_dom::ParseOptions::default());
        let mut data = StyleData::new();
        let mut engine = StyleEngine::new(&dom, doc, DeviceConfig::default());
        engine.set_parallelism(parallelism);
        engine.update_all_style_blocks(&dom, &data);
        let stats = engine.restyle(&dom, &mut data);
        assert_eq!(stats.parallel, parallelism == Parallelism::Parallel);
        let values: Vec<String> =
            dom.descendants(doc)
                .filter(|&n| dom.is_element(n))
                .map(|n| match data.styles(n) {
                    Some(v) => snowlark_style::computed::computed_property_by_name(v, "margin-top")
                        .unwrap(),
                    None => "-".into(),
                })
                .collect();
        (stats.restyled, values)
    };
    assert_eq!(style(Parallelism::Parallel), style(Parallelism::Sequential));
}

#[test]
fn a_sheet_added_with_a_state_change_pending_restyles() {
    // Stylesheet invalidation reads the classes and id of every element snapshot, also of one that
    // only records a state change.
    let mut p = Page::new(&sections(3));
    let s1 = p.q("#s1");
    p.engine
        .set_state(&p.dom, &mut p.data, s1, ElementState::HOVER, true);
    let base = url::Url::parse("https://example.com/").unwrap();
    let sheet = p.engine.parse_sheet(
        &p.data,
        "#s1 p { width: 2px; } .on { height: 1px; }",
        &base,
        "",
    );
    let owner = p.q("section");
    p.engine.add_sheet(&p.dom, &p.data, owner, sheet);
    p.restyle();
    assert_eq!(p.get("#s1 p", "width"), "2px");
    assert_eq!(p.get("#s1 p", "color"), "rgb(1, 2, 3)");
}

/// Subtrees inserted between two restyles have their descendants prepared at the restyle, if they
/// are still in the document then: replaced ones are skipped, the ones that stay are styled, and
/// page-owned state set on a new element before the restyle is kept.
#[test]
fn subtrees_inserted_between_restyles_are_prepared_at_the_restyle() {
    let mut p = Page::new(
        "<style>li { color: red } .a span { color: blue } li:hover { color: green }</style><ul></ul>",
    );
    let ul = p.q("ul");
    let process = |p: &mut Page| {
        let r = p.dom.take_reactions();
        p.engine.process_reactions(&p.dom, &mut p.data, &r);
    };
    p.dom
        .set_inner_html(ul, "<li><span>gone</span></li>")
        .unwrap();
    process(&mut p);
    p.dom
        .set_inner_html(ul, "<li class=a><span>b</span></li><li><b>c</b></li>")
        .unwrap();
    process(&mut p);
    let first = p.dom.first_child(ul).unwrap();
    let second = p.dom.next_sibling(first).unwrap();
    let b = p.dom.first_child(second).unwrap();
    // A descendant of a new element gets its state before the restyle prepared it.
    p.engine
        .set_state(&p.dom, &mut p.data, b, ElementState::HOVER, true);
    p.engine
        .set_state(&p.dom, &mut p.data, second, ElementState::HOVER, true);
    p.restyle();
    let span = p.dom.first_child(first).unwrap();
    assert_eq!(p.value_of(first, "color"), "rgb(255, 0, 0)");
    assert_eq!(p.value_of(span, "color"), "rgb(0, 0, 255)");
    assert_eq!(p.value_of(second, "color"), "rgb(0, 128, 0)");
    assert_eq!(p.value_of(b, "color"), "rgb(0, 128, 0)");
    // Removed again before any restyle, then inserted elsewhere: styled where it lands.
    p.dom
        .set_inner_html(ul, "<li class=a><span>x</span></li>")
        .unwrap();
    process(&mut p);
    let li = p.dom.first_child(ul).unwrap();
    p.dom.remove_node(li);
    process(&mut p);
    let body = p.q("body");
    p.dom.append_child(body, li).unwrap();
    p.restyle();
    let span = p.dom.first_child(li).unwrap();
    assert_eq!(p.value_of(span, "color"), "rgb(0, 0, 255)");
}

/// A removal's `:has()` invalidation can meet children that the same operation inserted (a
/// parent's children replaced at once); they are prepared before the batch's other reactions are
/// handled. (flingup.com crashed here: a button's icon replaced through a fragment.)
#[test]
fn removal_invalidation_sees_children_inserted_by_the_same_operation() {
    let mut p = Page::new(
        "<style>button:has(i) { color: red } button:has(svg) { color: blue }</style>\
         <div><button><i></i>x</button></div>",
    );
    let button = p.q("button");
    assert_eq!(p.value_of(button, "color"), "rgb(255, 0, 0)");
    // One operation: the old children go, a fragment's children (built detached) come.
    let doc = p.doc;
    let fragment = p.dom.create_document_fragment(doc);
    let holder = p.dom.create_element(doc, "span", None).unwrap();
    p.dom
        .set_inner_html(holder, "<svg><path d='M0 0'/></svg><b>y</b>")
        .unwrap();
    while let Some(c) = p.dom.first_child(holder) {
        p.dom.append_child(fragment, c).unwrap();
    }
    let _ = p.dom.take_reactions();
    p.dom
        .replace_children(button, vec![snowlark_dom::NodeOrString::Node(fragment)])
        .unwrap();
    p.restyle();
    assert_eq!(p.value_of(button, "color"), "rgb(0, 0, 255)");
    let svg = p.dom.first_child(button).unwrap();
    assert!(p.engine.computed_style(&p.dom, &p.data, svg).is_some());
}

/// A DOM change that never reached the engine (a bug elsewhere) does not abort the restyle in
/// release builds: the restyle prepares the elements it would meet unprepared (and logs it).
#[cfg(not(debug_assertions))]
#[test]
fn unreported_insertions_are_prepared_before_the_traversal() {
    let mut p = Page::new("<style>b { color: blue }</style><div></div>");
    let div = p.q("div");
    p.dom.set_inner_html(div, "<p><b>x</b></p>").unwrap();
    // The reactions are lost.
    let _ = p.dom.take_reactions();
    p.engine.invalidate_subtree(&p.dom, &mut p.data, div);
    p.restyle();
    let b = p.dom.first_child(p.dom.first_child(div).unwrap()).unwrap();
    assert_eq!(p.value_of(b, "color"), "rgb(0, 0, 255)");
}

/// Selectors 4 §4.4 `:has()`: removing the element that made a non-subject anchor match restyles
/// the subject that depends on it (`div:has(> .test) > #subject`).
#[test]
fn has_in_parent_position_invalidates_on_removal() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>div { color: rgb(1, 1, 1) } div:has(> .test) > #subject { color: rgb(0, 128, 0) }</style>\
         <main><div id=p><div id=subject></div></div></main>",
    );
    let parent = p.q("#p");
    let subject = p.q("#subject");
    let doc = p.doc;
    assert_eq!(p.value_of(subject, "color"), "rgb(1, 1, 1)");
    let t = p.dom.create_element(doc, "div", None).unwrap();
    p.dom.set_attribute(t, "class", "test").unwrap();
    p.dom.insert_before(parent, t, Some(subject)).unwrap();
    p.restyle();
    assert_eq!(p.value_of(subject, "color"), "rgb(0, 128, 0)");
    p.dom.remove_child(parent, t).unwrap();
    p.restyle();
    assert_eq!(p.value_of(subject, "color"), "rgb(1, 1, 1)");
}

/// Selectors 4 §4.4: a class change on an ancestor of a `:has()` anchor, outside every search
/// path, invalidates the anchor when the argument holds a complex selector reaching it
/// (`.red:has(#d:is(.scope .b))`).
#[test]
fn has_with_complex_is_invalidates_from_above_the_anchor() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>div { color: rgb(1, 1, 1) } .red:has(#d:is(.scope .b)) { color: rgb(255, 0, 0) }</style>\
         <div id=parent><div id=anchor class=red><div id=d class=b></div></div></div>",
    );
    let anchor = p.q("#anchor");
    assert_eq!(p.value_of(anchor, "color"), "rgb(1, 1, 1)");
    let parent = p.q("#parent");
    p.dom.set_attribute(parent, "class", "scope").unwrap();
    p.restyle();
    assert_eq!(p.value_of(anchor, "color"), "rgb(255, 0, 0)");
}

/// Selectors 4 §4.4 with §14.3-4: an element without the argument's class inserted before or
/// removed from among the children moves the others' indices, which `:nth-child()` and
/// `:first-child` in a `:has()` argument read (`#c:has(.a:nth-child(2)) ~ #subject`).
#[test]
fn has_with_nth_child_invalidates_when_siblings_move() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>p { color: rgb(1, 1, 1) } #c:has(.a:nth-child(2)) ~ #s1 { color: rgb(0, 128, 0) }\
         #c:has(.a:first-child) ~ #s2 { color: rgb(0, 0, 255) }</style>\
         <div id=c><div id=x class=a></div></div><p id=s1></p><p id=s2></p>",
    );
    let (c, x, s1, s2, doc) = (p.q("#c"), p.q("#x"), p.q("#s1"), p.q("#s2"), p.doc);
    assert_eq!(p.value_of(s1, "color"), "rgb(1, 1, 1)");
    assert_eq!(p.value_of(s2, "color"), "rgb(0, 0, 255)");
    let plain = p.dom.create_element(doc, "div", None).unwrap();
    p.dom.insert_before(c, plain, Some(x)).unwrap();
    p.restyle();
    assert_eq!(
        p.value_of(s1, "color"),
        "rgb(0, 128, 0)",
        "after the insertion"
    );
    assert_eq!(
        p.value_of(s2, "color"),
        "rgb(1, 1, 1)",
        "after the insertion"
    );
    p.dom.remove_child(c, plain).unwrap();
    p.restyle();
    assert_eq!(p.value_of(s1, "color"), "rgb(1, 1, 1)", "after the removal");
    assert_eq!(
        p.value_of(s2, "color"),
        "rgb(0, 0, 255)",
        "after the removal"
    );
}

/// Selectors 4 §4.4: a subtree inserted after a sibling on a `:has(~ div .test)` search path, with
/// no match in it, is on that path: a class added in it later invalidates the anchor.
#[test]
fn has_with_sibling_descendant_search_reaches_an_inserted_subtree() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>p { color: rgb(1, 1, 1) } div:has(~ div .test) ~ #s { color: rgb(0, 128, 0) }</style>\
         <main id=m><div id=a></div><p id=s></p><div id=b></div></main>",
    );
    let (m, s, b, doc) = (p.q("#m"), p.q("#s"), p.q("#b"), p.doc);
    assert_eq!(p.value_of(s, "color"), "rgb(1, 1, 1)");
    let outer = p.dom.create_element(doc, "div", None).unwrap();
    let inner = p.dom.create_element(doc, "div", None).unwrap();
    p.dom.append_child(outer, inner).unwrap();
    p.dom
        .insert_before(m, outer, p.dom.next_sibling(b))
        .unwrap();
    p.restyle();
    assert_eq!(p.value_of(s, "color"), "rgb(1, 1, 1)", "inserted");
    p.dom.set_attribute(inner, "class", "test").unwrap();
    p.restyle();
    assert_eq!(p.value_of(s, "color"), "rgb(0, 128, 0)", "class added");
}

/// Selectors 4 §4.4: an element inserted between an anchor and the sibling its `:has(+ #b)` matched
/// unmatches it; a text child inserted into an element makes `:has(:empty)` stop matching.
#[test]
fn has_with_next_sibling_and_empty_invalidates_on_insertion() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>div, p { color: rgb(1, 1, 1) } #a:has(+ #b) { color: rgb(0, 128, 0) }\
         #e:has(> :empty) ~ #s { color: rgb(0, 0, 255) }</style>\
         <main id=m><div id=a></div><div id=b></div></main><div id=e><i id=i></i></div><p id=s></p>",
    );
    let (m, a, b, i, s, doc) = (p.q("#m"), p.q("#a"), p.q("#b"), p.q("#i"), p.q("#s"), p.doc);
    assert_eq!(p.value_of(a, "color"), "rgb(0, 128, 0)");
    assert_eq!(p.value_of(s, "color"), "rgb(0, 0, 255)");
    let x = p.dom.create_element(doc, "div", None).unwrap();
    p.dom.insert_before(m, x, Some(b)).unwrap();
    let t = p.dom.create_text_node(doc, "x");
    p.dom.append_child(i, t).unwrap();
    p.restyle();
    assert_eq!(p.value_of(a, "color"), "rgb(1, 1, 1)", "separated from #b");
    assert_eq!(p.value_of(s, "color"), "rgb(1, 1, 1)", "#i not empty");
}

/// Selectors 4 §4.4: removing an element restyles `:has()` anchors whose search reached it through
/// siblings (`div:has(~ div .test) > #subject`).
#[test]
fn has_with_sibling_search_invalidates_on_removal() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>div { color: rgb(1, 1, 1) } div:has(~ div .test) > #subject { color: rgb(0, 128, 0) }</style>\
         <main><div id=a><div id=subject></div></div><div id=next><div id=t class=test></div></div></main>",
    );
    let subject = p.q("#subject");
    assert_eq!(p.value_of(subject, "color"), "rgb(0, 128, 0)");
    let next = p.q("#next");
    let t = p.q("#t");
    p.dom.remove_child(next, t).unwrap();
    p.restyle();
    assert_eq!(p.value_of(subject, "color"), "rgb(1, 1, 1)");
}

#[test]
fn an_attribute_change_parses_only_what_that_attribute_decides() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let mut p = Page::new(
        "<!DOCTYPE html><style>.on { width: 7px; }</style><div id=d style='height: 3px'></div>",
    );
    let parsed = Arc::new(AtomicUsize::new(0));
    let count = parsed.clone();
    p.engine
        .set_style_attribute_check(Some(Arc::new(move |_, _| {
            count.fetch_add(1, Ordering::Relaxed);
            true
        })));
    let d = p.q("#d");
    for (name, value) in [("data-x", "1"), ("class", "on"), ("title", "t")] {
        p.dom.set_attribute(d, name, value).unwrap();
        p.restyle();
    }
    assert_eq!(parsed.load(Ordering::Relaxed), 0, "`style` did not change");
    assert_eq!(p.get("#d", "width"), "7px");
    assert_eq!(p.get("#d", "height"), "3px");

    p.dom.set_attribute(d, "style", "height: 4px").unwrap();
    p.restyle();
    assert_eq!(parsed.load(Ordering::Relaxed), 1);
    assert_eq!(p.get("#d", "height"), "4px");
}

#[test]
fn presentation_attributes_stay_right_across_other_attribute_changes() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>.wide { height: 9px; }</style>
         <img id=i width=10><svg><rect id=r opacity=0.5 /></svg>",
    );
    let (i, r) = (p.q("#i"), p.q("#r"));
    assert_eq!(p.get("#i", "width"), "10px");
    assert_eq!(p.get("#r", "opacity"), "0.5");

    // (attributes that are no hint leave the hints as they were)
    p.dom.set_attribute(i, "class", "wide").unwrap();
    p.dom.set_attribute(i, "data-x", "1").unwrap();
    p.dom.set_attribute(r, "class", "wide").unwrap();
    p.dom.set_attribute(r, "data-x", "1").unwrap();
    p.restyle();
    assert_eq!(p.get("#i", "width"), "10px");
    assert_eq!(p.get("#i", "height"), "9px");
    assert_eq!(p.get("#r", "opacity"), "0.5");

    p.dom.set_attribute(i, "width", "20").unwrap();
    p.dom.set_attribute(r, "opacity", "0.25").unwrap();
    p.restyle();
    assert_eq!(p.get("#i", "width"), "20px");
    assert_eq!(p.get("#r", "opacity"), "0.25");

    p.dom.remove_attribute(r, "opacity");
    p.restyle();
    assert_eq!(p.get("#r", "opacity"), "1");
}
