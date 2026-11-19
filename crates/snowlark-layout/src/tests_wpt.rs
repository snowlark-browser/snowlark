// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WPT `check-layout-th` expectations transcribed into trees (the file each comes from is named).

use crate::style::*;
use crate::testing::TestStyle;
use crate::tests::*;

#[test]
fn flexbox_column_reverse_gap() {
    // css/css-flexbox/column-reverse-gap.html
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::ColumnReverse;
        s.row_gap = Some(LengthPercentage::px(5.0));
        s.column_gap = Some(LengthPercentage::px(5.0));
        s.position = Position::Relative;
    });
    let items: Vec<_> = (0..3)
        .map(|_| t.doc.block(flex, |s| s.height = SizeValue::px(10.0)))
        .collect();
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, flex).height, 40.0);
    assert_eq!(rect(&f, items[0]).y, 30.0);
    assert_eq!(rect(&f, items[1]).y, 15.0);
    assert_eq!(rect(&f, items[2]).y, 0.0);
}

#[test]
fn flexbox_items_dont_collapse_margins() {
    // css/css-flexbox/flexitem-no-margin-collapsing.html
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.position = Position::Relative;
    });
    let item = t.doc.block(flex, |_| {});
    let p = t.doc.element(item, "p", |s| {
        s.display = Display::BLOCK;
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
        s.margin_px(10.0);
    });
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, item), r(0.0, 0.0, 120.0, 120.0));
    assert_eq!(rect(&f, p), r(10.0, 10.0, 100.0, 100.0));
}

#[test]
fn flexbox_justify_space_between_with_min_height() {
    // css/css-flexbox/justify-content_space-between-002.html
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
        s.justify_content = Align::SpaceBetween;
        s.min_height = SizeValue::px(500.0);
    });
    let a = t.doc.block(flex, |_| {});
    t.doc.text(a, "First item");
    let b = t.doc.block(flex, |_| {});
    t.doc.text(b, "Second item");
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, flex).height, 500.0);
    assert_eq!(rect(&f, b).y, 490.0);
}

#[test]
fn flexbox_box_sizing_min_max() {
    // css/css-flexbox/box-sizing-min-max-sizes-001.html
    let mut t = T::new();
    let r0 = t.root();
    let item = |t: &mut T, parent, f: &dyn Fn(&mut TestStyle)| {
        t.doc.block(parent, |s| {
            s.box_sizing = BoxSizing::BorderBox;
            s.border(5.0, snowlark_paint::Color::BLACK);
            s.padding_px(5.0);
            f(s);
        })
    };
    let row = t.doc.block(r0, |s| s.display = Display::FLEX);
    let a = item(&mut t, row, &|s| {
        s.max_width = MaxSizeValue::px(30.0);
        s.width = SizeValue::px(40.0);
    });
    let b = item(&mut t, row, &|s| {
        s.min_width = SizeValue::px(30.0);
        s.width = SizeValue::px(10.0);
    });
    let col = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
    });
    let c = item(&mut t, col, &|s| {
        s.max_height = MaxSizeValue::px(30.0);
        s.height = SizeValue::px(40.0);
    });
    let d = item(&mut t, col, &|s| {
        s.min_height = SizeValue::px(30.0);
        s.height = SizeValue::px(10.0);
    });
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, a).width, 30.0);
    assert_eq!(rect(&f, b).width, 30.0);
    assert_eq!(rect(&f, c).height, 30.0);
    assert_eq!(rect(&f, d).height, 30.0);
}

#[test]
fn grid_item_fixed_max_width() {
    // css/css-grid/grid-items/grid-item-fixed-max-width-001.html
    let mut t = T::new();
    let r0 = t.root();
    let grid = t.doc.block(r0, |s| {
        s.display = Display::GRID;
        s.grid_template_columns = TrackList {
            items: vec![TrackListItem::Track(TrackSize::Breadth(
                TrackBreadth::LengthPercentage(LengthPercentage::px(100.0)),
            ))],
            line_names: Vec::new(),
        };
    });
    let item = t.doc.block(grid, |s| {
        s.max_width = MaxSizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
    });
    let f = t.run(300.0, 300.0);
    assert_eq!(rect(&f, item).width, 50.0);
}

#[test]
fn grid_alignment_implies_size_change() {
    // css/css-grid/alignment/grid-alignment-implies-size-change-001.html, including the dynamic
    // change from `align-items: stretch` to `start`.
    let mut t = T::new();
    let r0 = t.root();
    let grid = t.doc.block(r0, |s| {
        s.display = Display::INLINE_GRID;
        s.position = Position::Relative;
        s.grid_template_columns = TrackList {
            items: vec![TrackListItem::Track(TrackSize::Breadth(
                TrackBreadth::LengthPercentage(LengthPercentage::px(100.0)),
            ))],
            line_names: Vec::new(),
        };
        s.grid_template_rows = TrackList {
            items: vec![TrackListItem::Track(TrackSize::Breadth(
                TrackBreadth::LengthPercentage(LengthPercentage::px(200.0)),
            ))],
            line_names: Vec::new(),
        };
        s.font_size = 20.0;
        s.align_items = Align::Stretch;
    });
    let item = t.doc.block(grid, |_| {});
    for (i, line) in ["XX X", "X XXX", "X", "XX XXX"].iter().enumerate() {
        if i > 0 {
            t.doc.element(item, "br", |_| {});
        }
        t.doc.text(item, line);
    }
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, item), r(0.0, 0.0, 100.0, 200.0));
    t.doc.restyle(grid, |s| s.align_items = Align::Start);
    t.layout.style_changed(&t.doc.dom, grid, &t.doc.styles);
    let f = t.relayout();
    // "XX XXX" (120px) wraps in 100px: five 20px lines.
    assert_eq!(rect(&f, item), r(0.0, 0.0, 100.0, 100.0));
}
