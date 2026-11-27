// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The engine's per-change work, checked on the side table itself (which the integration tests do not
//! see): what an insertion prepares now and what it leaves to the next restyle.

use snowlark_dom::{Dom, NodeId, ParseOptions, ShadowRootInit, ShadowRootMode};

use super::*;
use crate::{computed, DeviceConfig};

struct Page {
    dom: Dom,
    data: StyleData,
    engine: StyleEngine,
    doc: NodeId,
}

impl Page {
    fn new(html: &str) -> Page {
        let mut dom = Dom::new();
        let doc = dom.parse_html_str(html, ParseOptions::default());
        let data = StyleData::new();
        let mut engine = StyleEngine::new(&dom, doc, DeviceConfig::default());
        engine.update_all_style_blocks(&dom, &data);
        let mut page = Page {
            dom,
            data,
            engine,
            doc,
        };
        page.restyle();
        page.dom.set_reactions_enabled(true);
        page
    }

    /// Hands the DOM's reactions to the engine, as the page layer does after each operation.
    fn process(&mut self) {
        let reactions = self.dom.take_reactions();
        self.engine
            .process_reactions(&self.dom, &mut self.data, &reactions);
    }

    fn restyle(&mut self) {
        self.process();
        self.engine.restyle(&self.dom, &mut self.data);
    }

    fn q_in(&self, root: NodeId, selector: &str) -> NodeId {
        let list = self.engine.parse_selector(selector).expect("selector");
        self.engine
            .query_selector_all(&self.dom, &self.data, root, &list)
            .first()
            .copied()
            .unwrap_or_else(|| panic!("no element matches {selector}"))
    }

    fn q(&self, selector: &str) -> NodeId {
        self.q_in(self.doc, selector)
    }

    fn value_of(&self, element: NodeId, property: &str) -> String {
        let values = self
            .engine
            .computed_style(&self.dom, &self.data, element)
            .expect("element has a computed style");
        computed::computed_property_by_name(&values, property).expect("known property")
    }
}

/// An inserted element's descendants are prepared at the next restyle, also once the event loop has
/// made a shadow tree somewhere (Speedometer's suites share one, and a list rebuilt with `innerHTML`
/// several times between two frames then prepared and reset every element each time).
#[test]
fn an_inserted_subtree_waits_for_the_restyle_while_shadow_trees_exist() {
    let mut p =
        Page::new("<style>li span { color: rgb(0, 0, 255) }</style><div id=h></div><ul></ul>");
    let host = p.q("#h");
    p.dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    p.engine.shadow_root_attached(&p.dom, &mut p.data, host);
    p.restyle();
    let ul = p.q("ul");
    p.dom.set_inner_html(ul, "<li><span>a</span></li>").unwrap();
    p.process();
    let li = p.dom.first_child(ul).unwrap();
    let span = p.dom.first_child(li).unwrap();
    assert!(
        p.data.slot(li).is_none(),
        "the new inserted element waits for the restyle with its subtree"
    );
    assert!(
        p.data.slot(span).is_none(),
        "its descendants wait for the restyle"
    );
    // Replaced before any restyle: the removal does not walk the subtree that was never prepared,
    // and the restyle prepares what stayed.
    p.dom.set_inner_html(ul, "<li><span>b</span></li>").unwrap();
    p.process();
    assert!(p.data.slot(li).is_none() && p.data.slot(span).is_none());
    p.restyle();
    let li = p.dom.first_child(ul).unwrap();
    let span = p.dom.first_child(li).unwrap();
    assert_eq!(p.value_of(span, "color"), "rgb(0, 0, 255)");
    assert!(p.data.slot(li).is_some());
}

/// An element prepared before (here moved within the document) is prepared at its insertion, its
/// descendants later; a host's new child too, whose slot holds the slot it is assigned to.
#[test]
fn a_moved_element_and_a_hosts_child_are_prepared_at_their_insertion() {
    let mut p = Page::new(
        "<style>li span { color: rgb(0, 0, 255) }</style>\
         <ul id=a><li><span>x</span></li></ul><ul id=b></ul><div id=h></div>",
    );
    let li = p.dom.first_child(p.q("#a")).unwrap();
    let span = p.dom.first_child(li).unwrap();
    p.dom.append_child(p.q("#b"), li).unwrap();
    p.process();
    assert!(p.data.slot(li).unwrap().has(bits::UNPREPARED_DESCENDANTS));
    let host = p.q("#h");
    let shadow = p
        .dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    p.engine.shadow_root_attached(&p.dom, &mut p.data, host);
    p.dom.set_inner_html(shadow, "<slot></slot>").unwrap();
    p.restyle();
    p.dom.set_inner_html(host, "<p><span>y</span></p>").unwrap();
    p.process();
    let child = p.dom.first_child(host).unwrap();
    let slot = p.dom.first_child(shadow).unwrap();
    assert_eq!(p.data.slot(child).unwrap().assigned_slot, Some(slot));
    p.restyle();
    assert_eq!(p.value_of(span, "color"), "rgb(0, 0, 255)");
    assert!(p.data.styles(p.dom.first_child(child).unwrap()).is_some());
}

/// Page-owned state set on an element whose preparation waits whole (a checked box just inserted)
/// prepares it alone to keep the state; its descendants still wait, and the restyle keeps the state.
#[test]
fn state_set_on_a_waiting_root_is_kept_and_its_descendants_still_wait() {
    let mut p = Page::new("<style>li:checked span { color: rgb(0, 0, 255) }</style><ul></ul>");
    let ul = p.q("ul");
    p.dom.set_inner_html(ul, "<li><span>a</span></li>").unwrap();
    p.process();
    let li = p.dom.first_child(ul).unwrap();
    let span = p.dom.first_child(li).unwrap();
    assert!(p.data.slot(li).is_none());
    assert!(p
        .engine
        .set_state(&p.dom, &mut p.data, li, ElementState::CHECKED, true));
    assert!(p.data.slot(li).unwrap().has(bits::UNPREPARED_DESCENDANTS));
    assert!(p.data.slot(span).is_none());
    p.restyle();
    assert_eq!(p.value_of(span, "color"), "rgb(0, 0, 255)");
}

/// A subtree inserted into a shadow tree waits too, and the slots in it are assigned when it is
/// prepared: the host's children are slotted into them (the assigned nodes layout reads), inherit
/// through them and match `::slotted()` (DOM §4.2.2.4 "assign slottables for a tree", CSS Scoping 1 §3.2).
#[test]
fn slots_inserted_into_a_shadow_tree_are_assigned_when_their_subtree_is_prepared() {
    let mut p = Page::new("<div id=h><span id=a slot=one>a</span></div>");
    let host = p.q("#h");
    let shadow = p
        .dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    p.engine.shadow_root_attached(&p.dom, &mut p.data, host);
    p.dom
        .set_inner_html(
            shadow,
            "<style>slot { color: rgb(5, 5, 5) } ::slotted(span) { width: 3px }</style><p></p>",
        )
        .unwrap();
    let style = p.dom.first_child(shadow).unwrap();
    p.restyle();
    p.engine.update_style_block(&p.dom, &p.data, style);
    p.restyle();
    let a = p.q("#a");
    assert!(
        p.data.styles(a).is_none(),
        "no slot yet: #a is out of the flat tree"
    );
    let wrapper = p.dom.create_element(p.doc, "div", None).unwrap();
    p.dom
        .set_inner_html(wrapper, "<section><slot name=one></slot></section>")
        .unwrap();
    p.dom.append_child(shadow, wrapper).unwrap();
    p.process();
    let section = p.dom.first_child(wrapper).unwrap();
    assert!(
        p.data.slot(wrapper).is_none() && p.data.slot(section).is_none(),
        "the new inserted element waits for the restyle with its descendants"
    );
    p.restyle();
    // (what layout builds the slot's boxes from)
    let slot = p.dom.first_child(section).unwrap();
    assert_eq!(p.data.slotted_nodes(slot), Some(&[a][..]));
    assert_eq!(p.value_of(a, "color"), "rgb(5, 5, 5)");
    assert_eq!(p.value_of(a, "width"), "3px");
}

/// An attribute change marks the presentational hints and the next restyle parses them, once however
/// many of the element's attributes changed (Blink: `SetPresentationAttributeStyleIsDirty`, built at
/// the style update); hints that appear, change or go all show then (HTML §15, SVG 2 §6.6).
#[test]
fn presentational_hints_are_parsed_at_the_restyle() {
    let mut p =
        Page::new("<svg><rect width=10 height=10 fill=red></rect></svg><div align=left>x</div>");
    let rect = p.q("rect");
    let hints = |p: &Page, id| p.data.slot(id).unwrap().attrs.hints.clone();
    let before = hints(&p, rect).expect("the rect has hints");
    p.dom.set_attribute(rect, "fill", "blue").unwrap();
    p.dom.set_attribute(rect, "stroke", "green").unwrap();
    p.process();
    let now = hints(&p, rect).expect("the rect has hints");
    assert!(Arc::ptr_eq(&before, &now), "parsed before the restyle");
    p.restyle();
    assert_eq!(p.value_of(rect, "fill"), "rgb(0, 0, 255)");
    assert_eq!(p.value_of(rect, "stroke"), "rgb(0, 128, 0)");
    // Gone: the property falls back to its initial value.
    p.dom.remove_attribute(rect, "fill");
    p.dom.remove_attribute(rect, "stroke");
    p.restyle();
    assert_eq!(p.value_of(rect, "fill"), "rgb(0, 0, 0)");
    assert_eq!(p.value_of(rect, "stroke"), "none");
    let div = p.q("div");
    assert_eq!(p.value_of(div, "text-align"), "-webkit-left");
    p.dom.set_attribute(div, "align", "right").unwrap();
    p.restyle();
    assert_eq!(p.value_of(div, "text-align"), "-webkit-right");
}

/// HTML §15.3.8: an `li` whose `value` parses by the rules for parsing integers sets its `list-item`
/// counter (`counter-set: list-item <value>`); one that does not parse sets nothing.
#[test]
fn li_value_sets_the_list_item_counter() {
    let mut p = Page::new(
        "<ol><li id=a value=10></li><li id=b value=10e10></li><li id=c value=-3x></li>\
         <li id=d value=xyz></li><li id=e></li></ol>",
    );
    p.restyle();
    for (id, expected) in [
        ("#a", "list-item 10"),
        ("#b", "list-item 10"),
        ("#c", "list-item -3"),
        ("#d", "none"),
        ("#e", "none"),
    ] {
        let li = p.q(id);
        assert_eq!(p.value_of(li, "counter-set"), expected, "{id}");
    }
}

/// SVG 2 §6.6: each presentation attribute is parsed alone, as a value of its property: one that
/// leaves a function open does not take the attributes after it in, and an invalid one is ignored
/// alone. Elements with the same presentation attributes share one block.
#[test]
fn svg_presentation_attributes_are_parsed_one_by_one() {
    let mut p = Page::new(
        "<svg><rect id=a fill='url(#g' stroke=green stroke-width=2></rect>\
         <rect id=b fill='url(#g' stroke=green stroke-width=2></rect>\
         <circle id=c r=5 fill=bogus stroke=blue></circle></svg>",
    );
    let (a, b, c) = (p.q("#a"), p.q("#b"), p.q("#c"));
    assert_eq!(p.value_of(a, "stroke"), "rgb(0, 128, 0)");
    assert_eq!(p.value_of(a, "stroke-width"), "2px");
    assert_eq!(p.value_of(c, "fill"), "rgb(0, 0, 0)");
    assert_eq!(p.value_of(c, "stroke"), "rgb(0, 0, 255)");
    assert_eq!(p.value_of(c, "r"), "5px");
    let hints = |p: &Page, id| p.data.slot(id).unwrap().attrs.hints.clone().unwrap();
    assert!(Arc::ptr_eq(&hints(&p, a), &hints(&p, b)));
    // A value parsed for one element is the same for the next one given it.
    p.dom.set_attribute(c, "stroke", "green").unwrap();
    p.restyle();
    assert_eq!(p.value_of(c, "stroke"), "rgb(0, 128, 0)");
}

/// SVG 2 §9.4.1: a `path`'s `d` attribute is the `d` property's value, path data in the attribute's
/// grammar (§9.3.9), where newlines are white space (as in a pretty-printed file); data with an
/// error gives no value (the attribute's path keeps what precedes it); `d` on other elements is
/// no presentation attribute.
#[test]
fn the_d_attribute_is_parsed_as_path_data() {
    let mut p = Page::new(
        "<svg><path id=a d='M 10 20\n L 30 40'></path><path id=b d='M 0 0 L 5 5 X'></path>\
         <rect id=c d='M 1 1'></rect></svg>",
    );
    let (a, b, c) = (p.q("#a"), p.q("#b"), p.q("#c"));
    assert_eq!(p.value_of(a, "d"), "path(\"M 10 20 L 30 40\")");
    assert_eq!(p.value_of(b, "d"), "none");
    assert_eq!(p.value_of(c, "d"), "none");
    p.dom.set_attribute(b, "d", "m 1 2 h 3").unwrap();
    p.restyle();
    assert_eq!(p.value_of(b, "d"), "path(\"m 1 2 h 3\")");
}

/// CSS Shapes 1 §3.1 and CSSOM's shortest serialization: `rect()` computes to the `inset()` it is,
/// and a side computed by arithmetic (`100% - 80%`) that serializes as another side does is dropped
/// as a side written so would be.
#[test]
fn computed_insets_serialize_their_shortest_form() {
    let p = Page::new(
        "<style>#a { clip-path: rect(50% 80% 70% 20%) } #b { shape-outside: rect(10% 90% 90% 10%) }
         #c { clip-path: inset(10% 20% 30% 40%) }</style><div id=a></div><div id=b></div><div id=c></div>",
    );
    assert_eq!(p.value_of(p.q("#a"), "clip-path"), "inset(50% 20% 30%)");
    assert_eq!(p.value_of(p.q("#b"), "shape-outside"), "inset(10%)");
    assert_eq!(p.value_of(p.q("#c"), "clip-path"), "inset(10% 20% 30% 40%)");
}

/// CSS Shapes 2: `polygon()`'s `round` interpolates with none as zero, and a zero result is none, as
/// a zero radius parses.
#[test]
fn an_interpolated_zero_polygon_radius_is_none() {
    use style::values::animated::{Animate, Procedure};
    use style::values::computed::LengthPercentage;
    use style::values::generics::basic_shape::{FillRule, Polygon, PolygonCoord};
    let px = |v: f32| LengthPercentage::new_length(style::values::computed::Length::new(v));
    let with = |round: Option<f32>| Polygon {
        fill: FillRule::Nonzero,
        round: round.map(px),
        coordinates: vec![
            PolygonCoord(px(0.0), px(0.0)),
            PolygonCoord(px(9.0), px(0.0)),
        ]
        .into(),
    };
    let (a, b) = (with(Some(0.0)), with(Some(20.0)));
    let at = |p: f64| {
        a.animate(&b, Procedure::Interpolate { progress: p })
            .unwrap()
    };
    assert_eq!(at(0.0).round, None);
    assert_eq!(at(0.5).round, Some(px(10.0)));
}

/// HTML §3.2.6.2: an element's `lang` (or `xml:lang`) is the content language of its text and its
/// descendants' (stylo's internal `-x-lang`, which layout shapes and breaks lines with); a change
/// shows at the next restyle, and `lang=""` makes the language unknown.
#[test]
fn the_lang_attribute_sets_the_content_language() {
    let mut p = Page::new("<div lang=ja><p><span>x</span></p></div><p id=none>y</p>");
    let lang = |p: &Page, id| {
        p.engine
            .computed_style(&p.dom, &p.data, id)
            .unwrap()
            .get_font()
            ._x_lang
            .0
            .to_string()
    };
    let (div, span, none) = (p.q("div"), p.q("span"), p.q("#none"));
    assert_eq!(lang(&p, div), "ja");
    assert_eq!(lang(&p, span), "ja");
    assert_eq!(lang(&p, none), "");
    p.dom.set_attribute(span, "lang", "zh-Hant").unwrap();
    p.dom.set_attribute(div, "lang", "").unwrap();
    p.restyle();
    assert_eq!(lang(&p, span), "zh-Hant");
    assert_eq!(lang(&p, div), "");
    assert_eq!(lang(&p, p.q("p")), "");
}

/// Elements whose presentational hints have the same text share one parsed block (one parse, one
/// rule node, and style sharing between them), an element given the same hints joins it, and the
/// shared block gives each of them its values.
#[test]
fn elements_with_the_same_hints_share_one_block() {
    let mut p = Page::new(
        "<svg><rect id=a fill=red stroke-width=2></rect><rect id=b fill=red stroke-width=2></rect>\
         <rect id=c fill=blue stroke-width=2></rect></svg>",
    );
    let (a, b, c) = (p.q("#a"), p.q("#b"), p.q("#c"));
    let hints = |p: &Page, id| p.data.slot(id).unwrap().attrs.hints.clone().unwrap();
    assert!(
        Arc::ptr_eq(&hints(&p, a), &hints(&p, b)),
        "same text, one block"
    );
    assert!(
        !Arc::ptr_eq(&hints(&p, a), &hints(&p, c)),
        "another value, its own"
    );
    p.dom.set_attribute(c, "fill", "red").unwrap();
    p.restyle();
    assert!(
        Arc::ptr_eq(&hints(&p, a), &hints(&p, c)),
        "changed to the same text"
    );
    assert_eq!(p.value_of(c, "fill"), "rgb(255, 0, 0)");
    p.dom.set_attribute(b, "fill", "lime").unwrap();
    p.restyle();
    assert_eq!(p.value_of(a, "fill"), "rgb(255, 0, 0)");
    assert_eq!(p.value_of(b, "fill"), "rgb(0, 255, 0)");
    assert_eq!(p.value_of(c, "stroke-width"), "2px");
}

/// Selectors 4 §14: a removal from a parent whose children match by position (`:nth-child()`, `~`)
/// restyles the children left, marked once on the parent rather than child by child for each
/// removal (a list emptied one child at a time would walk the children once per child); their
/// styles follow their new positions.
#[test]
fn a_removal_restyles_positional_siblings_through_the_parent() {
    let mut html = String::from(
        "<style>li { color: blue } li:nth-child(2n) { color: red } .x ~ li { width: 5px }</style><ul>",
    );
    for i in 0..8 {
        html.push_str(if i == 0 {
            "<li class=x></li>"
        } else {
            "<li></li>"
        });
    }
    let mut p = Page::new(&(html + "</ul>"));
    let ul = p.q("ul");
    let items: Vec<_> = p.dom.element_children(ul).collect();
    let hint = |p: &Page, id| p.data.slot(id).unwrap().data.borrow().hint;
    p.dom.remove_child(ul, items[0]).unwrap();
    p.dom.remove_child(ul, items[1]).unwrap();
    p.process();
    assert!(hint(&p, ul).contains(RestyleHint::RESTYLE_DESCENDANTS));
    for &li in &items[2..] {
        assert!(
            hint(&p, li).is_empty(),
            "marked on the parent, not each child"
        );
    }
    p.restyle();
    assert_eq!(p.value_of(items[2], "color"), "rgb(0, 0, 255)");
    assert_eq!(p.value_of(items[3], "color"), "rgb(255, 0, 0)");
    assert_eq!(p.value_of(items[7], "color"), "rgb(255, 0, 0)");
    assert_eq!(p.value_of(items[7], "width"), "auto");
}

/// SVG hint blocks are kept with their key text only for attributes a second element has: the
/// shapes of a chart, each with values of its own, leave none behind; the first element's block is
/// the one the second gets, and every later one. Values written differently are kept apart, and
/// each still gives its value.
#[test]
fn svg_hint_blocks_are_kept_from_the_second_element_with_the_same_attributes() {
    let mut p = Page::new(
        "<svg><rect id=a y=1 height=9></rect><rect id=b y=2 height=8></rect>\
         <rect id=c y=3 height=7></rect></svg>",
    );
    let kept = |p: &Page| p.engine.hint_blocks.borrow().svg_len;
    assert_eq!(kept(&p), 0, "attributes of their own: no key kept");
    assert_eq!(p.value_of(p.q("#b"), "height"), "8px");
    let svg = p.q("svg");
    for (id, y) in [("d", "2"), ("e", "2"), ("f", "2px")] {
        let rect = p
            .dom
            .create_element_ns(p.doc, Some("http://www.w3.org/2000/svg"), "rect", None)
            .unwrap();
        p.dom.set_attribute(rect, "id", id).unwrap();
        p.dom.set_attribute(rect, "y", y).unwrap();
        p.dom.set_attribute(rect, "height", "8").unwrap();
        p.dom.append_child(svg, rect).unwrap();
    }
    p.restyle();
    assert_eq!(kept(&p), 1, "b's attributes, seen twice");
    let hints = |p: &Page, id| p.data.slot(id).unwrap().attrs.hints.clone().unwrap();
    let (b, d, e, f) = (p.q("#b"), p.q("#d"), p.q("#e"), p.q("#f"));
    assert!(
        Arc::ptr_eq(&hints(&p, b), &hints(&p, d)),
        "the first one's block"
    );
    assert!(Arc::ptr_eq(&hints(&p, b), &hints(&p, e)), "kept");
    assert!(
        !Arc::ptr_eq(&hints(&p, b), &hints(&p, f)),
        "written differently"
    );
    assert_eq!(p.value_of(f, "y"), "2px");
    assert_eq!(p.value_of(e, "y"), "2px");
}

/// An element whose hint block no other element holds is marked so (the style sharing cache is
/// not searched for it); the mark goes when a later element is given the block, and the two
/// share their style again in a restyle where the later one comes first in the tree.
#[test]
fn svg_hint_blocks_held_by_one_element_are_marked_until_shared() {
    let mut p = Page::new(
        "<style>svg.k rect { stroke: red }</style>\
         <svg><rect id=a y=1></rect><rect id=b y=2></rect></svg>",
    );
    let unshared = |p: &Page, id| p.data.slot(id).unwrap().has(bits::HINTS_UNSHARED);
    let (a, b, svg) = (p.q("#a"), p.q("#b"), p.q("svg"));
    assert!(
        unshared(&p, a) && unshared(&p, b),
        "attributes of their own"
    );
    // (c, with b's attributes, goes before b: b built the block, c is styled first)
    let c = p
        .dom
        .create_element_ns(p.doc, Some("http://www.w3.org/2000/svg"), "rect", None)
        .unwrap();
    p.dom.set_attribute(c, "y", "2").unwrap();
    p.dom.insert_before(svg, c, Some(b)).unwrap();
    p.restyle();
    assert!(!unshared(&p, c), "the first one's block");
    assert!(!unshared(&p, b), "its block is c's now");
    assert!(unshared(&p, a));
    p.dom.set_attribute(svg, "class", "k").unwrap();
    p.restyle();
    let style = |p: &Page, id| p.data.slot(id).unwrap().primary.clone().unwrap();
    assert!(
        Arc::ptr_eq(&style(&p, b), &style(&p, c)),
        "b shares c's style"
    );
    assert_eq!(p.value_of(b, "stroke"), "rgb(255, 0, 0)");
    assert_eq!(p.value_of(a, "y"), "1px");
}

/// The key of an SVG hint block is compared with an element's attributes in place: each property
/// and value, in order, and the language.
#[test]
fn svg_hint_keys_compare_with_the_attributes() {
    let list = [("fill", "red"), ("clip-path", "url(#a:b)")];
    let key = svg_key_text(&list, None);
    assert!(svg_key_is(&key, &list, None));
    assert!(!svg_key_is(&key, &list[..1], None));
    assert!(!svg_key_is(
        &key,
        &[("fill", "red"), ("clip-path", "url(#a)")],
        None
    ));
    assert!(!svg_key_is(
        &key,
        &[("clip-path", "url(#a:b)"), ("fill", "red")],
        None
    ));
    assert!(!svg_key_is(&key, &list, Some("en")));
    let with_lang = svg_key_text(&list, Some("en"));
    assert!(svg_key_is(&with_lang, &list, Some("en")));
    assert!(!svg_key_is(&with_lang, &list, Some("e")));
    assert!(!svg_key_is(&with_lang, &list, None));
    assert_eq!(svg_key_hash(&list, None), svg_key_hash(&list, None));
}

/// A style view builds element handles for the part of the side table it touches: a query or a
/// lazily resolved pseudo-element of one element costs a chunk, not one handle for every element the
/// event loop ever had (the table keeps a slot for each arena index an element held).
#[test]
fn a_view_builds_handles_only_for_what_it_touches() {
    let mut html = String::from("<div id=first></div>");
    for _ in 0..(4 * crate::handle::CELL_CHUNK) {
        html.push_str("<p></p>");
    }
    let p = Page::new(&html);
    let first = p.q("#first");
    assert!(p.data.slots.len() > 4 * crate::handle::CELL_CHUNK);
    let view = TreeView::new(&p.dom, &p.data, &p.engine.shadows, p.doc, false);
    let element = view.element(first);
    assert_eq!(element.node_id(), first);
    assert_eq!(view.built_chunks(), 1);
}

/// A shadow tree's content replaced by a subtree with a slot while the host's children are replaced
/// too, between two restyles (shadow-dom/MouseEvent-prototype-offsetX-offsetY.html, where stylo's
/// bloom filter found no common ancestor): the restyle styles the new children in the new slot.
#[test]
fn slotted_children_replaced_with_the_slot_between_restyles() {
    let mut p = Page::new("<div id=h></div>");
    let host = p.q("#h");
    let css = "<style>#c { padding: 10px } #t { margin-left: 5px }</style>";
    p.dom
        .set_inner_html(host, &format!("{css}<div id=c><span id=t>x</span></div>"))
        .unwrap();
    p.restyle();
    let shadow = p
        .dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Closed))
        .unwrap();
    p.engine.shadow_root_attached(&p.dom, &mut p.data, host);
    p.dom
        .set_inner_html(shadow, &format!("{css}<div id=c><span id=t>x</span></div>"))
        .unwrap();
    p.restyle();
    p.dom
        .set_inner_html(shadow, &format!("{css}<div id=c><slot></slot></div>"))
        .unwrap();
    p.process();
    p.dom
        .set_inner_html(host, &format!("{css}<div id=t>x</div>"))
        .unwrap();
    p.restyle();
    let t = p.q("#t");
    assert!(
        p.data.styles(t).is_some(),
        "the new child is styled in its slot"
    );
}

/// In a reaction batch, an ancestor walk stops where an earlier walk of the batch marked: the
/// children of one parent changed at once (a list replaced through `innerHTML`) mark its
/// ancestors once. A slot mutated since (here an ancestor's styles reset, which clears its bit)
/// makes the next walk go to the root again.
#[test]
fn ancestor_marks_in_a_batch_stop_where_the_batch_marked_until_a_slot_changes() {
    let mut p = Page::new("<div id=a><ul><li>x</li><li>y</li></ul></div>");
    let a = p.q("#a");
    let ul = p.q("ul");
    let first = p.q("li");
    let second = p.dom.next_element_sibling(first).unwrap();
    let dirty = |p: &Page, id: NodeId| p.data.slot(id).unwrap().has(bits::DIRTY_DESCENDANTS);
    assert!(!dirty(&p, a) && !dirty(&p, ul));

    p.data.begin_batch();
    mark_ancestors_dirty(&p.dom, &p.data, first);
    assert!(dirty(&p, ul) && dirty(&p, a));
    assert_eq!(p.data.marked_ancestors(), Some(ul));
    // (cleared behind the table's back: a walk that stops at the cached element leaves it so)
    p.data.slot(a).unwrap().clear(bits::DIRTY_DESCENDANTS);
    mark_ancestors_dirty(&p.dom, &p.data, second);
    assert!(
        !dirty(&p, a),
        "the walk stopped at the element the batch marked"
    );
    mark_ancestors_dirty(&p.dom, &p.data, ul);
    assert!(
        !dirty(&p, a),
        "the marked element's own ancestors are marked"
    );

    // A mutated slot forgets the mark.
    p.data.slot_mut(a).unwrap().reset_data();
    assert_eq!(p.data.marked_ancestors(), None);
    mark_ancestors_dirty(&p.dom, &p.data, second);
    assert!(dirty(&p, a));
    p.data.end_batch();

    // Outside a batch, every walk goes to the root.
    mark_ancestors_dirty(&p.dom, &p.data, first);
    assert_eq!(p.data.marked_ancestors(), None);
    p.data.slot(a).unwrap().clear(bits::DIRTY_DESCENDANTS);
    mark_ancestors_dirty(&p.dom, &p.data, second);
    assert!(dirty(&p, a));
}

/// A slot moved out of its shadow tree into the document has no assigned nodes (DOM §4.2.2: only a
/// slot in a shadow tree is assigned slottables), so the restyle walks its own children: the host's
/// child it had is out of the flat tree. Before, the moved slot kept its list and the restyle met the
/// host's child under it while its parent chain went through the host, and stylo's bloom filter panicked
/// ("should have found a common ancestor").
#[test]
fn a_slot_moved_out_of_its_shadow_tree_has_no_assigned_nodes() {
    let mut p = Page::new("<div id=h><span id=a>a</span></div><div id=out></div>");
    let host = p.q("#h");
    let shadow = p
        .dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    p.engine.shadow_root_attached(&p.dom, &mut p.data, host);
    p.dom
        .set_inner_html(shadow, "<slot><b id=fallback>f</b></slot>")
        .unwrap();
    p.restyle();
    let a = p.q("#a");
    let slot = p.dom.first_child(shadow).unwrap();
    assert_eq!(p.data.slotted_nodes(slot), Some(&[a][..]));
    assert!(p.data.styles(a).is_some());

    let out = p.q("#out");
    p.dom.append_child(out, slot).unwrap();
    p.restyle();
    assert_eq!(p.data.slotted_nodes(slot), None);
    assert!(p.data.styles(a).is_none(), "#a is out of the flat tree");
    let fallback = p.dom.first_child(slot).unwrap();
    assert!(
        p.data.styles(fallback).is_some(),
        "the slot's own child is styled"
    );
}

/// The same with the slot inside a subtree that leaves the shadow tree and is inserted into the
/// document between two restyles.
#[test]
fn a_subtree_with_a_slot_moved_out_of_its_shadow_tree() {
    let mut p = Page::new("<div id=h><span id=a>a</span></div><div id=out></div>");
    let host = p.q("#h");
    let shadow = p
        .dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    p.engine.shadow_root_attached(&p.dom, &mut p.data, host);
    p.dom
        .set_inner_html(shadow, "<section><p><slot></slot></p></section>")
        .unwrap();
    p.restyle();
    let a = p.q("#a");
    let section = p.dom.first_child(shadow).unwrap();
    let removed = p.dom.remove_child(shadow, section).unwrap();
    p.process();
    let out = p.q("#out");
    p.dom.append_child(out, removed).unwrap();
    p.restyle();
    let slot = p
        .dom
        .first_child(p.dom.first_child(section).unwrap())
        .unwrap();
    assert_eq!(p.data.slotted_nodes(slot), None);
    assert!(p.data.styles(a).is_none());
    assert!(p.data.styles(slot).is_some());
}

/// A host moved in one operation (its removal and insertion in one reaction batch) where `:has()`
/// makes the insertion prepare the subtree at once (reddit's feed put back on Back): the removal,
/// handled after the batch's insertions were prepared, must not clear the slot assignment the
/// insertion computed. Before, the host's child kept no slot while the slot still listed it, and
/// stylo's bloom filter panicked ("should have found a common ancestor").
#[test]
fn a_host_moved_where_has_prepares_its_subtree_keeps_its_slot_assignment() {
    let mut p = Page::new(
        "<style>#out:has(i) { color: rgb(1, 2, 3) }</style>\
         <div id=h><span id=a>a</span></div><div id=out></div>",
    );
    let host = p.q("#h");
    let shadow = p
        .dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    p.engine.shadow_root_attached(&p.dom, &mut p.data, host);
    p.dom
        .set_inner_html(shadow, "<p><slot></slot></p>")
        .unwrap();
    p.restyle();
    let a = p.q("#a");
    let slot = p
        .dom
        .first_child(p.dom.first_child(shadow).unwrap())
        .unwrap();
    assert_eq!(p.data.slot(a).unwrap().assigned_slot, Some(slot));

    let out = p.q("#out");
    p.dom.append_child(out, host).unwrap();
    p.process();
    assert!(
        !p.data.slot(host).unwrap().has(bits::UNPREPARED_DESCENDANTS),
        "`:has()` on the new parent: the subtree is prepared now"
    );
    assert_eq!(p.data.slotted_nodes(slot), Some(&[a][..]));
    assert_eq!(p.data.slot(a).unwrap().assigned_slot, Some(slot));
    p.restyle();
    assert!(p.data.styles(a).is_some());
}

/// The restyle never meets a slotted element at the wrong depth: a slot whose list holds an element
/// assigned elsewhere (a stale list, here made by hand) leaves it out of the traversal instead of
/// panicking in stylo's bloom filter. (Debug builds assert instead, to find what made the list stale.)
#[cfg(not(debug_assertions))]
#[test]
fn a_stale_slot_list_does_not_panic_the_restyle() {
    let mut p = Page::new("<div id=h><span id=a>a</span></div>");
    let host = p.q("#h");
    let shadow = p
        .dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    p.engine.shadow_root_attached(&p.dom, &mut p.data, host);
    p.dom
        .set_inner_html(shadow, "<p><slot></slot></p>")
        .unwrap();
    p.restyle();
    let a = p.q("#a");
    let slot = p
        .dom
        .first_child(p.dom.first_child(shadow).unwrap())
        .unwrap();
    assert_eq!(p.data.slotted_nodes(slot), Some(&[a][..]));
    p.data.slot_mut(a).unwrap().assigned_slot = None;
    p.engine.invalidate_subtree(&p.dom, &mut p.data, host);
    p.restyle();
    assert!(p.data.styles(slot).is_some());
}
