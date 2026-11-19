// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Positioned layout (css-position-3): relative offsets of table parts and of blocks inside split
//! inline boxes, containing blocks (inline boxes, filters and containment), static positions, and
//! absolutely positioned boxes sized and aligned in their inset-modified containing block. Ahem
//! 10px, as in `tests.rs`.

use crate::style::*;
use crate::tests::*;

fn px(v: f32) -> LengthPercentageAuto {
    LengthPercentageAuto::px(v)
}

#[test]
fn relatively_positioned_table_parts_move_with_their_cells() {
    let mut t = T::new();
    let r0 = t.root();
    let table = t.doc.element(r0, "table", |s| {
        s.display = Display::TABLE;
        s.position = Position::Relative;
        s.border_spacing = (0.0, 0.0);
    });
    let tbody = t.doc.element(table, "tbody", |s| {
        s.display = Display::TABLE_ROW_GROUP;
        s.position = Position::Relative;
        s.inset[3] = px(5.0);
    });
    let tr = t.doc.element(tbody, "tr", |s| {
        s.display = Display::TABLE_ROW;
        s.position = Position::Relative;
        s.inset[0] = px(7.0);
    });
    let td = t.doc.element(tr, "td", |s| {
        s.display = Display::TABLE_CELL;
        s.position = Position::Relative;
        s.inset[0] = px(1.0);
    });
    let content = t.doc.block(td, |s| {
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(20.0);
    });
    // An absolutely positioned box whose containing block is the row group.
    let abs = t.doc.block(td, |s| {
        s.position = Position::Absolute;
        s.inset[3] = px(30.0);
        s.inset[0] = px(0.0);
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(10.0);
    });
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, table), r(0.0, 0.0, 50.0, 20.0));
    assert_eq!(rect(&f, content), r(5.0, 8.0, 50.0, 20.0));
    // the row group's padding box (moved 5px right) is its containing block... but the cell is
    // nearer: positioned too, it is the containing block (moved 5 + 7 + 1).
    assert_eq!(rect(&f, abs), r(35.0, 8.0, 10.0, 10.0));
}

#[test]
fn absolutely_positioned_box_in_a_relative_row_group() {
    let mut t = T::new();
    let r0 = t.root();
    let table = t.doc.element(r0, "table", |s| {
        s.display = Display::TABLE;
        s.border_spacing = (0.0, 0.0);
    });
    let tbody = t.doc.element(table, "tbody", |s| {
        s.display = Display::TABLE_ROW_GROUP;
        s.position = Position::Relative;
        s.inset[3] = px(50.0);
    });
    let tr = t
        .doc
        .element(tbody, "tr", |s| s.display = Display::TABLE_ROW);
    let td = t.doc.element(tr, "td", |s| s.display = Display::TABLE_CELL);
    t.doc.block(td, |s| {
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
    });
    let abs = t.doc.block(td, |s| {
        s.position = Position::Absolute;
        s.inset[0] = px(0.0);
        s.inset[3] = px(50.0);
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
    });
    let f = t.run(300.0, 300.0);
    // left: 50px from the row group, itself moved 50px.
    assert_eq!(rect(&f, abs), r(100.0, 0.0, 50.0, 50.0));
}

#[test]
fn a_block_inside_a_relative_inline_moves_with_it() {
    // CSS 2 §9.2.1.1: the inline's relative offset also moves the block it was split around, and
    // percentages resolve against the inline's containing block.
    let mut t = T::new();
    let r0 = t.root();
    let cb = t.doc.block(r0, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
    });
    let span = t.doc.span(cb, |s| {
        s.position = Position::Relative;
        s.inset[0] = LengthPercentageAuto::percent(100.0);
        s.inset[3] = LengthPercentageAuto::percent(100.0);
    });
    let block = t.doc.block(span, |s| {
        s.position = Position::Relative;
        s.inset[0] = px(-100.0);
        s.inset[3] = px(-100.0);
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
    });
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, block), r(0.0, 0.0, 100.0, 100.0));
}

#[test]
fn inline_containing_block_spans_first_to_last_fragment() {
    // css-position-3 §2.1: the start edges of the first fragment, the end edges of the last.
    let mut t = T::new();
    let r0 = t.root();
    let cb = t.doc.block(r0, |s| s.width = SizeValue::px(80.0));
    t.doc.text(cb, "XX ");
    let span = t.doc.span(cb, |s| s.position = Position::Relative);
    t.doc.text(span, "XXXX XXXXXXX XXXXX");
    let abs = t.doc.block(span, |s| {
        s.position = Position::Absolute;
        s.inset = [px(0.0); 4];
    });
    let f = t.run(300.0, 300.0);
    // first fragment from x 30 on line 1; last ends at 50 on line 3
    assert_eq!(rect(&f, abs), r(30.0, 0.0, 20.0, 30.0));
}

/// A filtered inline box is the containing block of its absolutely and fixed positioned
/// descendants (Filter Effects 1 §5 applies to inline boxes, unlike a transform), with the same
/// edges as a relatively positioned one.
#[test]
fn a_filtered_inline_is_a_containing_block() {
    let mut t = T::new();
    let r0 = t.root();
    let cb = t.doc.block(r0, |s| s.width = SizeValue::px(80.0));
    t.doc.text(cb, "XX ");
    let span = t.doc.span(cb, |s| {
        s.filters = vec![snowlark_paint::FilterOp::Brightness(1.0)];
    });
    t.doc.text(span, "XXXX XXXXXXX XXXXX");
    let abs = t.doc.block(span, |s| {
        s.position = Position::Absolute;
        s.inset = [px(0.0); 4];
    });
    let fixed = t.doc.block(span, |s| {
        s.position = Position::Fixed;
        s.inset = [
            px(0.0),
            LengthPercentageAuto::Auto,
            LengthPercentageAuto::Auto,
            px(0.0),
        ];
        s.width = SizeValue::px(5.0);
        s.height = SizeValue::px(5.0);
    });
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, abs), r(30.0, 0.0, 20.0, 30.0));
    assert_eq!(rect(&f, fixed), r(30.0, 0.0, 5.0, 5.0));
}

#[test]
fn inline_containing_block_across_a_split() {
    // The containing block of an inline split around a block starts at its first fragment, in
    // the anonymous block before the split.
    let mut t = T::new();
    let r0 = t.root();
    let cb = t.doc.block(r0, |s| s.width = SizeValue::px(100.0));
    t.doc.text(cb, "B ");
    let span = t.doc.span(cb, |s| s.position = Position::Relative);
    t.doc.block(span, |_| {});
    t.doc.text(span, "AA");
    let abs = t.doc.block(span, |s| {
        s.position = Position::Absolute;
        s.inset[0] = px(0.0);
        s.width = SizeValue::px(5.0);
        s.height = SizeValue::px(5.0);
    });
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, abs).y, 0.0);
}

#[test]
fn filters_and_containment_contain_fixed_boxes() {
    let mut t = T::new();
    let r0 = t.root();
    t.doc.block(r0, |s| s.height = SizeValue::px(30.0));
    let filtered = t.doc.block(r0, |s| {
        s.fixed_cb = true;
        s.margin[3] = px(20.0);
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
    });
    let fixed = t.doc.block(filtered, |s| {
        s.position = Position::Fixed;
        s.inset[0] = px(0.0);
        s.inset[3] = px(0.0);
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(10.0);
    });
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, fixed), r(20.0, 30.0, 10.0, 10.0));
}

#[test]
fn inline_level_abspos_among_blocks_is_placed_as_on_a_line() {
    // Beside a float, aligned by text-align (css-position-3 §3.5.3, as if on a line of its own).
    let mut t = T::new();
    let r0 = t.root();
    let cb = t.doc.block(r0, |s| {
        s.position = Position::Relative;
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
        s.text_align = TextAlign::Center;
    });
    t.doc.block(cb, |s| s.height = SizeValue::px(50.0));
    t.doc.block(cb, |s| {
        s.float = Float::Left;
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
    });
    let abs = t.doc.element(cb, "div", |s| {
        s.display = Display::BLOCK;
        s.original_display = Some(Display::INLINE);
        s.position = Position::Absolute;
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(10.0);
    });
    let block = t.doc.block(cb, |s| {
        s.position = Position::Absolute;
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(10.0);
    });
    let f = t.run(300.0, 300.0);
    // the line beside the float is 50..100, centered at 75
    assert_eq!(rect(&f, abs), r(75.0, 50.0, 10.0, 10.0));
    // a block-level one ignores the float
    assert_eq!(rect(&f, block), r(0.0, 50.0, 10.0, 10.0));
}

#[test]
fn abspos_with_negative_inset_modified_containing_block() {
    // WPT position-absolute-with-negative-sized-imcb case 1 and 3.
    let mut t = T::new();
    let r0 = t.root();
    let cb = t.doc.block(r0, |s| {
        s.position = Position::Relative;
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(20.0);
    });
    let auto_margins = t.doc.block(cb, |s| {
        s.position = Position::Absolute;
        s.inset = [px(12.0); 4];
        s.margin = [LengthPercentageAuto::Auto; 4];
        s.width = SizeValue::px(18.0);
        s.height = SizeValue::px(18.0);
    });
    let stretched = t.doc.block(cb, |s| {
        s.position = Position::Absolute;
        s.inset = [px(18.0); 4];
        s.margin = [px(-10.0); 4];
    });
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, auto_margins), r(12.0, 3.0, 18.0, 18.0));
    assert_eq!(rect(&f, stretched), r(8.0, 8.0, 20.0, 20.0));
}

#[test]
fn abspos_table_with_both_insets_is_fit_content() {
    let mut t = T::new();
    let r0 = t.root();
    let cb = t.doc.block(r0, |s| {
        s.position = Position::Relative;
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(100.0);
    });
    let table = t.doc.element(cb, "div", |s| {
        s.display = Display::TABLE;
        s.position = Position::Absolute;
        s.inset[1] = px(0.0);
        s.inset[3] = px(0.0);
        s.border_spacing = (0.0, 0.0);
    });
    let cell = t
        .doc
        .element(table, "div", |s| s.display = Display::TABLE_CELL);
    t.doc.block(cell, |s| {
        s.width = SizeValue::px(30.0);
        s.height = SizeValue::px(10.0);
    });
    let f = t.run(300.0, 300.0);
    // normal alignment is start: fit-content width at the left
    assert_eq!(rect(&f, table), r(0.0, 0.0, 30.0, 10.0));
}

#[test]
fn geometry_sees_sticky_boxes_at_their_offset_position() {
    // css-position-3 §3.4.1: a `top: 0` sticky box below 50px of content, the viewport scrolled
    // by 80: it is moved 30px down to stay in view, within its 300px tall parent.
    let mut t = T::new();
    let r0 = t.root();
    let parent = t.doc.block(r0, |s| s.height = SizeValue::px(300.0));
    t.doc.block(parent, |s| s.height = SizeValue::px(50.0));
    let sticky = t.doc.block(parent, |s| {
        s.position = Position::Sticky;
        s.inset[0] = px(0.0);
        s.height = SizeValue::px(10.0);
    });
    let inner = t.doc.block(sticky, |s| s.height = SizeValue::px(5.0));
    let after = t.doc.block(parent, |s| s.height = SizeValue::px(10.0));
    t.run(100.0, 100.0);
    let none = std::collections::HashMap::new();
    let view = snowlark_paint::Size::new(100.0, 100.0);
    let at = |n, y: f32| {
        t.layout
            .sticky_offset(n, view, snowlark_paint::Point::new(0.0, y), &none)
    };
    assert_eq!(at(sticky, 80.0), snowlark_paint::Point::new(0.0, 30.0));
    // what it contains moves with it; what follows it does not
    assert_eq!(at(inner, 80.0), snowlark_paint::Point::new(0.0, 30.0));
    assert_eq!(at(after, 80.0), snowlark_paint::Point::ZERO);
    // not scrolled past it: no shift; scrolled past its parent: held at the parent's bottom
    assert_eq!(at(sticky, 20.0), snowlark_paint::Point::ZERO);
    assert_eq!(at(sticky, 400.0), snowlark_paint::Point::new(0.0, 240.0));
}

#[test]
fn client_rects_apply_transforms() {
    let mut t = T::new();
    let r0 = t.root();
    let b = t.doc.block(r0, |s| {
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(10.0);
        s.margin[3] = px(20.0);
        s.transform = Some(snowlark_paint::Matrix::scale(2.0, 2.0));
    });
    t.run(100.0, 100.0);
    // scaled about its center (15, 5)
    assert_eq!(t.layout.client_rects(b), vec![r(15.0, -5.0, 20.0, 20.0)]);
}

#[test]
fn sticky_inline_boxes_shift() {
    let mut t = T::new();
    let r0 = t.root();
    let parent = t.doc.block(r0, |s| s.height = SizeValue::px(300.0));
    t.doc.block(parent, |s| s.height = SizeValue::px(50.0));
    let sticky = t.doc.span(parent, |s| {
        s.position = Position::Sticky;
        s.inset[0] = px(0.0);
    });
    t.doc.text(sticky, "XXX");
    let f = t.run(100.0, 100.0);
    let none = std::collections::HashMap::new();
    let view = snowlark_paint::Size::new(100.0, 100.0);
    let d = t
        .layout
        .sticky_offset(sticky, view, snowlark_paint::Point::new(0.0, 80.0), &none);
    assert_eq!(d, snowlark_paint::Point::new(0.0, 30.0), "{}", f.dump());
}
