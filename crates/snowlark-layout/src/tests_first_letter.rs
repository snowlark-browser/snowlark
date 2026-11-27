// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `::first-letter` (CSS Pseudo 4 §3.2): which text it takes, its box inline or floated, and the
//! text node's offsets on both sides of the split; `::first-line` (§3.1): the first formatted line
//! laid out in its styles, the lines after it in their own. On Ahem: every glyph is a square of the
//! font size.

use crate::style::*;
use crate::tests::*;

fn big_letter(s: &mut crate::testing::TestStyle) {
    s.font_size = 40.0;
}

#[test]
fn the_first_letter_takes_its_own_style() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| s.font_size = 20.0);
    let text = t.doc.text(p, "Hello world");
    t.doc.pseudo(p, PseudoElement::FirstLetter, big_letter);
    let f = t.run(800.0, 400.0);
    let parts = texts(&f, text);
    assert_eq!(parts.len(), 2, "{}", t.dump());
    // The letter is 40px; the rest of the line follows it at 20px, on the same baseline.
    assert_eq!(
        (parts[0].x, parts[0].width, parts[0].height),
        (0.0, 40.0, 40.0)
    );
    assert_eq!((parts[1].x, parts[1].width), (40.0, 200.0));
    assert_eq!(parts[0].y + 32.0, parts[1].y + 16.0);
    assert!(t.dump().contains("::FirstLetter"));
}

#[test]
fn offsets_past_the_letter_are_the_text_nodes() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| s.font_size = 20.0);
    let text = t.doc.text(p, "Hello");
    t.doc.pseudo(p, PseudoElement::FirstLetter, big_letter);
    t.run(800.0, 400.0);
    // The DOM range 1..2 is the "e" right after the 40px letter.
    let rects = t.layout.text_range_rects(text, 1, 2);
    assert_eq!(rects.len(), 1);
    assert_eq!((rects[0].x, rects[0].width), (40.0, 20.0));
    let rects = t.layout.text_range_rects(text, 0, 1);
    assert_eq!((rects[0].x, rects[0].width), (0.0, 40.0));
}

#[test]
fn a_floated_letter_is_a_float_beside_the_lines() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        s.font_size = 20.0;
        s.width = SizeValue::px(200.0);
    });
    let text = t.doc.text(p, "Hello world");
    t.doc.pseudo(p, PseudoElement::FirstLetter, |s| {
        s.font_size = 40.0;
        s.float = Float::Left;
        s.display = Display::BLOCK;
    });
    let f = t.run(800.0, 400.0);
    let parts = texts(&f, text);
    assert_eq!(parts[0], r(0.0, 0.0, 40.0, 40.0), "{}", t.dump());
    // "ello " fits in the 160px beside the float on the first line, "world" wraps beside it too.
    assert_eq!((parts[1].x, parts[1].y), (40.0, 0.0));
    assert_eq!((parts[2].x, parts[2].y), (40.0, 20.0));
}

#[test]
fn punctuation_and_white_space_before_the_letter_go_with_it() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| s.font_size = 20.0);
    let text = t.doc.text(p, "  \u{201C}Hi");
    t.doc.pseudo(p, PseudoElement::FirstLetter, big_letter);
    let f = t.run(800.0, 400.0);
    let parts = texts(&f, text);
    // The leading spaces collapse away; the quote and the H make the 80px letter.
    assert_eq!((parts[0].x, parts[0].width), (0.0, 80.0), "{}", t.dump());
    assert_eq!((parts[1].x, parts[1].width), (80.0, 20.0));
}

#[test]
fn the_letter_is_found_in_the_first_block_child_and_inline_boxes() {
    let mut t = T::new();
    let r0 = t.root();
    let div = t.doc.block(r0, |s| s.font_size = 20.0);
    let p = t.doc.block(div, |_| {});
    let span = t.doc.span(p, |_| {});
    let text = t.doc.text(span, "Hello");
    t.doc.pseudo(div, PseudoElement::FirstLetter, big_letter);
    let f = t.run(800.0, 400.0);
    let parts = texts(&f, text);
    assert_eq!((parts[0].width, parts[1].x), (40.0, 40.0), "{}", t.dump());
}

#[test]
fn no_letter_after_an_atomic_inline_or_in_a_flex_child() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| s.font_size = 20.0);
    t.doc.element(p, "span", |s| {
        s.display = Display::INLINE_BLOCK;
        s.width = SizeValue::px(10.0);
    });
    t.doc.text(p, "Hello");
    t.doc.pseudo(p, PseudoElement::FirstLetter, big_letter);
    let div = t.doc.block(r0, |s| s.font_size = 20.0);
    let flex = t.doc.block(div, |s| s.display = Display::FLEX);
    t.doc.text(flex, "Hello");
    t.doc.pseudo(div, PseudoElement::FirstLetter, big_letter);
    t.run(800.0, 400.0);
    assert!(!t.dump().contains("::FirstLetter"), "{}", t.dump());
}

#[test]
fn a_text_change_splits_the_new_text() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| s.font_size = 20.0);
    let text = t.doc.text(p, "Hello");
    t.doc.pseudo(p, PseudoElement::FirstLetter, big_letter);
    t.run(800.0, 400.0);
    t.doc.dom.set_data(text, "Jello there");
    // A split text box is not updated in place: its container is built again.
    assert!(!t.layout.text_changed(&t.doc.dom, text));
    t.layout.children_changed(&t.doc.dom, p, &t.doc.styles);
    let f = t.relayout();
    let parts = texts(&f, text);
    assert_eq!(
        (parts[0].width, parts[1].x, parts[1].width),
        (40.0, 40.0, 200.0)
    );
}

#[test]
fn the_first_line_is_laid_out_in_its_style() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        s.font_size = 20.0;
        s.width = SizeValue::px(200.0);
    });
    let text = t.doc.text(p, "aaaa bbbb cccc dddd");
    t.doc.pseudo(p, PseudoElement::FirstLine, big_letter);
    let f = t.run(800.0, 400.0);
    let parts = texts(&f, text);
    // "aaaa" at 40px fills the first line; the second goes on at 20px from "bbbb".
    assert_eq!(parts[0], r(0.0, 0.0, 160.0, 40.0), "{}", t.dump());
    assert_eq!((parts[1].x, parts[1].y, parts[1].height), (0.0, 40.0, 20.0));
    let rects = t.layout.text_range_rects(text, 5, 6);
    assert_eq!(rects[0], r(0.0, 40.0, 20.0, 20.0));
}

#[test]
fn inline_boxes_on_the_first_line_inherit_from_it() {
    let mut t = T::new();
    let r0 = t.root();
    let div = t.doc.block(r0, |s| {
        s.font_size = 20.0;
        s.width = SizeValue::px(400.0);
    });
    // The line belongs to the first block child's lines.
    let p = t.doc.block(div, |_| {});
    t.doc.text(p, "aa ");
    let span = t.doc.span(p, |_| {});
    let inner = t.doc.text(span, "bb");
    t.doc.pseudo(div, PseudoElement::FirstLine, big_letter);
    let f = t.run(800.0, 400.0);
    assert_eq!(
        texts(&f, inner)[0],
        r(120.0, 0.0, 80.0, 40.0),
        "{}",
        t.dump()
    );
}

#[test]
fn a_restyle_inside_the_first_line_keeps_its_style() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| s.font_size = 20.0);
    let span = t.doc.span(p, |_| {});
    let inner = t.doc.text(span, "bb");
    t.doc.pseudo(p, PseudoElement::FirstLine, big_letter);
    t.run(800.0, 400.0);
    t.doc
        .restyle(span, |s| s.color = snowlark_paint::Color::rgb(255, 0, 0));
    t.layout.style_changed(&t.doc.dom, span, &t.doc.styles);
    let f = t.relayout();
    assert_eq!(texts(&f, inner)[0].height, 40.0, "{}", t.dump());
}

#[test]
fn a_shrink_to_fit_width_measures_the_first_line_in_its_style() {
    let mut t = T::new();
    let r0 = t.root();
    let fl = t.doc.block(r0, |s| {
        s.font_size = 20.0;
        s.float = Float::Left;
    });
    t.doc.text(fl, "aaaa bbbb");
    t.doc.pseudo(fl, PseudoElement::FirstLine, big_letter);
    let f = t.run(800.0, 400.0);
    // Its max-content width is its one line at 40px.
    assert_eq!(rect(&f, fl).width, 360.0, "{}", t.dump());
}
