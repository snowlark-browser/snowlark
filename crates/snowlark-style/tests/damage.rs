// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Style change classification: each restyled element is recorded with what its new style requires of
//! layout (repaint, relayout, reconstruct; its contents; counters).

mod common;

use common::Page;
use snowlark_dom::NodeId;
use snowlark_style::{ElementRestyle, StyleChange};

const CSS: &str = "<!DOCTYPE html><style>
    #t.color { color: rgb(200, 0, 0) }
    #t.bg { background-color: rgb(0, 0, 200) }
    #t.border-color { border: 1px solid red } #t.border-color.b2 { border-color: blue }
    #t.vis { visibility: hidden }
    #t.outline { outline: 2px solid red }
    #t.box-shadow { box-shadow: 2px 2px 4px red }
    #t.cursor { cursor: pointer }
    #t.radius { border-radius: 4px }
    #t.z { position: relative } #t.z.z2 { z-index: 3 }
    #t.opacity { opacity: 0.5 }
    #t.tf { transform: rotate(10deg) } #t.tf.tf2 { transform: rotate(20deg) }
    #t.width { width: 120px }
    #t.margin { margin-left: 5px }
    #t.font { font-size: 30px }
    #t.lh { line-height: 3 }
    #t.display { display: inline }
    #t.position { position: absolute }
    #t.float { float: left }
    #t.overflow { overflow: hidden } #t.overflow.scroll { overflow: scroll }
    #t.before::before { content: 'x' }
    #t.before.before-color::before { color: red }
    #t.list { display: list-item; list-style-type: square }
    #t.list.l2 { list-style-type: disc }
    #t.deco { text-decoration: underline }
    #t.ws { white-space: pre }
    #t.counter { counter-increment: c }
    #t.anim { transition: color 1s }
    #t.same-a { color: rgb(1, 2, 3) }
    #t.same-b { color: rgb(1, 2, 3) }
    #t.gray { filter: grayscale(0.2) } #t.gray.gray2 { filter: grayscale(1) }
    #t.blur { filter: blur(5px) } #t.blur.blur2 { filter: blur(8px) }
    #t.blur.blur-gray { filter: blur(5px) grayscale(1) }
    #t.shadow { filter: drop-shadow(2px 2px 1px red) }
    #t.shadow.shadow2 { filter: drop-shadow(2px 2px 1px blue) }
  </style><body><div id=t><span id=child>text</span></div><p id=other>x</p></body>";

/// Sets `class` on `#t` (from `from`), restyles, and returns `#t`'s record (if it was restyled).
fn change(from: &str, to: &str) -> Option<ElementRestyle> {
    let mut p = Page::new(CSS);
    let t = p.q("#t");
    if !from.is_empty() {
        p.dom.set_attribute(t, "class", from).unwrap();
        p.restyle();
    }
    p.data.take_restyles();
    p.dom.set_attribute(t, "class", to).unwrap();
    p.restyle();
    record(&mut p, t)
}

fn record(p: &mut Page, el: NodeId) -> Option<ElementRestyle> {
    let log = p.data.take_restyles();
    assert!(!log.overflowed);
    log.elements.into_iter().find(|r| r.element == el)
}

fn own(from: &str, to: &str) -> StyleChange {
    change(from, to).expect("restyled").change
}

/// What painting reads from the style alone: the fragments stand.
#[test]
fn paint_only_properties_are_paint() {
    assert_eq!(own("", "color"), StyleChange::Paint);
    assert_eq!(own("", "bg"), StyleChange::Paint);
    assert_eq!(own("border-color", "border-color b2"), StyleChange::Paint);
    assert_eq!(own("", "vis"), StyleChange::Paint);
    assert_eq!(own("", "outline"), StyleChange::Paint);
    assert_eq!(own("", "box-shadow"), StyleChange::Paint);
    assert_eq!(own("", "cursor"), StyleChange::Paint);
    // (with the strongest of the struct's changes)
    assert_eq!(own("", "color radius"), StyleChange::Repaint);
}

/// Painting that layout records in the fragments: where the box paints, how it stacks.
#[test]
fn stacking_and_clipping_properties_are_repaint() {
    assert_eq!(own("", "radius"), StyleChange::Repaint);
    assert_eq!(own("z", "z z2"), StyleChange::Repaint);
    assert_eq!(own("", "opacity"), StyleChange::Repaint);
    // Between two transforms; gaining one makes the box the containing block of its positioned
    // descendants, which the boxes below find again.
    assert_eq!(own("tf", "tf tf2"), StyleChange::Repaint);
    assert_eq!(own("", "tf"), StyleChange::Reconstruct);
}

/// A `filter` change repaints, unless it changes how far the box paints past itself (a blur or a
/// drop shadow added, removed or resized: Filter Effects 1 §10), which the fragments' paint bounds
/// record, so layout runs again.
#[test]
fn filter_changes_relayout_only_when_their_outsets_change() {
    assert_eq!(own("gray", "gray gray2"), StyleChange::Repaint);
    assert_eq!(own("blur", "blur blur-gray"), StyleChange::Repaint);
    assert_eq!(own("shadow", "shadow shadow2"), StyleChange::Repaint);
    assert_eq!(own("blur", "blur blur2"), StyleChange::Relayout);
    assert_eq!(own("gray", "shadow"), StyleChange::Relayout);
    // (gaining a filter makes the box the containing block of its positioned descendants, as
    // gaining a transform does)
    assert_eq!(own("", "blur"), StyleChange::Reconstruct);
}

#[test]
fn geometry_properties_are_relayout() {
    assert_eq!(own("", "width"), StyleChange::Relayout);
    assert_eq!(own("", "margin"), StyleChange::Relayout);
    assert_eq!(own("", "font"), StyleChange::Relayout);
    assert_eq!(own("", "lh"), StyleChange::Relayout);
    // Between two non-visible values: the box stays a scroll container's.
    assert_eq!(own("overflow", "overflow scroll"), StyleChange::Relayout);
}

#[test]
fn box_generating_properties_are_reconstruct() {
    assert_eq!(own("", "display"), StyleChange::Reconstruct);
    assert_eq!(own("", "position"), StyleChange::Reconstruct);
    assert_eq!(own("", "float"), StyleChange::Reconstruct);
    assert_eq!(own("", "overflow"), StyleChange::Reconstruct);
}

#[test]
fn generated_contents_are_classified_separately() {
    // A ::before appearing, and its own style changing, regenerate the element's contents only.
    let r = change("", "before").unwrap();
    assert_eq!(r.contents, StyleChange::Reconstruct);
    let r = change("before", "before before-color").unwrap();
    assert_eq!(r.contents, StyleChange::Reconstruct);
    // A marker's type; white space its descendants take.
    let r = change("list", "list l2").unwrap();
    assert_eq!(r.contents, StyleChange::Reconstruct);
    assert!(r.change < StyleChange::Reconstruct);
    let r = change("", "ws").unwrap();
    assert_eq!(r.contents, StyleChange::Reconstruct);
    // Counters reach other elements' generated content.
    let r = change("", "counter").unwrap();
    assert!(r.counters);
    assert!(!change("", "color").unwrap().counters);
}

/// The text decorations its descendants' boxes take from it (and their colour, which resolves
/// `currentcolor` while it is decorated) are painting only: the boxes take them in place.
#[test]
fn decorations_its_contents_take_are_paint() {
    let r = change("", "deco").unwrap();
    assert_eq!(
        (r.change, r.contents),
        (StyleChange::Unchanged, StyleChange::Paint)
    );
    let r = change("deco", "deco color").unwrap();
    assert_eq!(
        (r.change, r.contents),
        (StyleChange::Paint, StyleChange::Paint)
    );
    // (undecorated, a colour change is the element's own only)
    let r = change("", "color").unwrap();
    assert_eq!(r.contents, StyleChange::Unchanged);
    // Regenerating wins over taking decorations in place.
    let r = change("", "deco ws").unwrap();
    assert_eq!(r.contents, StyleChange::Reconstruct);
}

#[test]
fn equal_values_are_unchanged() {
    // A transition declaration alone changes no value layout reads; equal colours from two rules.
    let r = change("", "anim").expect("restyled");
    assert_eq!(r.strongest(), StyleChange::Unchanged);
    let r = change("same-a", "same-b").expect("restyled");
    assert_eq!(r.strongest(), StyleChange::Unchanged);
}

#[test]
fn inherited_changes_record_the_descendants_and_nothing_else() {
    let mut p = Page::new(CSS);
    let t = p.q("#t");
    let child = p.q("#child");
    let other = p.q("#other");
    p.data.take_restyles();
    p.dom.set_attribute(t, "class", "font").unwrap();
    p.restyle();
    let log = p.data.take_restyles();
    let of = |n| log.elements.iter().find(|r| r.element == n).copied();
    assert_eq!(of(t).unwrap().change, StyleChange::Relayout);
    // The span inherits the font size.
    assert_eq!(of(child).unwrap().change, StyleChange::Relayout);
    assert!(of(other).is_none());
    // Drained: nothing is recorded twice.
    assert!(p.data.take_restyles().elements.is_empty());
}

#[test]
fn first_styles_are_reconstruct() {
    let mut p = Page::new(CSS);
    let doc = p.doc;
    let body = p.q("body");
    let el = p.dom.create_element(doc, "div", None).unwrap();
    p.dom.append_child(body, el).unwrap();
    p.data.take_restyles();
    p.restyle();
    assert_eq!(record(&mut p, el).unwrap().change, StyleChange::Reconstruct);
}
