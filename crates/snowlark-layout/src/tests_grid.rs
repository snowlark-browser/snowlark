// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Grid layout tests (css-grid-2): the taffy integration and the inputs Snowlark gives it, mostly
//! transcribed from WPT css-grid reftests.

use crate::style::*;
use crate::tests::*;

pub(crate) fn px_track(v: f32) -> TrackListItem {
    TrackListItem::Track(TrackSize::Breadth(TrackBreadth::LengthPercentage(
        LengthPercentage::px(v),
    )))
}

pub(crate) fn tracks(items: Vec<TrackListItem>) -> TrackList {
    TrackList {
        items,
        line_names: Vec::new(),
    }
}

#[test]
fn abspos_grid_child_has_its_grid_area_as_containing_block() {
    // css-grid/abspos/positioned-grid-items-002.html: the containing block of an absolutely
    // positioned child of a grid container is the grid area its placement gives (css-grid-2 §9.1);
    // `left: 0` is from that area's start, an `auto` line is the padding edge.
    let mut t = T::new();
    let r0 = t.root();
    let grid = t.doc.block(r0, |s| {
        s.display = Display::GRID;
        s.position = Position::Relative;
        s.width = SizeValue::px(550.0);
        s.height = SizeValue::px(400.0);
        s.padding = [
            LengthPercentage::px(20.0),
            LengthPercentage::px(15.0),
            LengthPercentage::px(10.0),
            LengthPercentage::px(5.0),
        ];
        s.border_width = [9.0, 3.0, 12.0, 6.0];
        s.border_style = [BorderStyle::Solid; 4];
        s.grid_template_columns = tracks(vec![px_track(200.0), px_track(300.0)]);
        s.grid_template_rows = tracks(vec![px_track(150.0), px_track(100.0)]);
    });
    let item = |t: &mut T, col: i16, row: i16| {
        t.doc.block(grid, |s| {
            s.position = Position::Absolute;
            s.inset[3] = LengthPercentageAuto::LengthPercentage(LengthPercentage::px(0.0));
            s.inset[0] = LengthPercentageAuto::LengthPercentage(LengthPercentage::px(0.0));
            s.width = SizeValue::px(10.0);
            s.height = SizeValue::px(10.0);
            s.grid_column_start = GridLine::Line(col);
            s.grid_column_end = GridLine::Line(col + 1);
            s.grid_row_start = GridLine::Line(row);
            s.grid_row_end = GridLine::Line(row + 1);
        })
    };
    let a = item(&mut t, 1, 1);
    let b = item(&mut t, 2, 2);
    let auto = t.doc.block(grid, |s| {
        s.position = Position::Absolute;
        s.inset[3] = LengthPercentageAuto::LengthPercentage(LengthPercentage::px(0.0));
        s.inset[0] = LengthPercentageAuto::LengthPercentage(LengthPercentage::px(0.0));
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(10.0);
        s.grid_column_start = GridLine::Line(2);
    });
    let f = t.run(800.0, 600.0);
    let g = rect(&f, grid);
    // The first track starts at the content edge: border 6 + padding 5 across, 9 + 20 down.
    assert_eq!(rect(&f, a), r(g.x + 11.0, g.y + 29.0, 10.0, 10.0));
    assert_eq!(rect(&f, b), r(g.x + 211.0, g.y + 179.0, 10.0, 10.0));
    // Column line 2 to the end padding edge; rows auto/auto: the padding box.
    assert_eq!(rect(&f, auto), r(g.x + 211.0, g.y + 9.0, 10.0, 10.0));
}

#[test]
fn grid_items_are_auto_placed_in_order_modified_document_order() {
    // css-grid/grid-items/grid-order-property-auto-placement-001.html (css-grid-2 §4.2, §8.5).
    let mut t = T::new();
    let r0 = t.root();
    let grid = t.doc.block(r0, |s| {
        s.display = Display::GRID;
        s.grid_template_columns = tracks(vec![px_track(50.0), px_track(50.0)]);
    });
    let item = |t: &mut T, order: i32| {
        t.doc.block(grid, |s| {
            s.order = order;
            s.height = SizeValue::px(50.0);
        })
    };
    let magenta = item(&mut t, 10);
    let lime = item(&mut t, 5);
    let yellow = item(&mut t, 1);
    let blue = item(&mut t, 0);
    let f = t.run(400.0, 400.0);
    assert_eq!((rect(&f, blue).x, rect(&f, blue).y), (0.0, 0.0));
    assert_eq!((rect(&f, yellow).x, rect(&f, yellow).y), (50.0, 0.0));
    assert_eq!((rect(&f, lime).x, rect(&f, lime).y), (0.0, 50.0));
    assert_eq!((rect(&f, magenta).x, rect(&f, magenta).y), (50.0, 50.0));
}

#[test]
fn replaced_grid_item_transfers_its_own_size_through_its_ratio() {
    // css-grid/grid-items/grid-automatic-minimum-intrinsic-aspect-ratio-001.html: a 100x50 image with
    // `height: 25px` in a min-content grid contributes 50px (CSS 2 §10.3.2), with `width: 50px` it
    // is 25px tall.
    let mut t = T::new();
    let r0 = t.root();
    let grid_with = |t: &mut T, f: fn(&mut crate::testing::TestStyle)| {
        let grid = t.doc.block(r0, |s| {
            s.display = Display::GRID;
            s.width = SizeValue::MinContent;
        });
        let img = t.doc.element(grid, "img", f);
        t.doc.replace(
            img,
            ReplacedContent::image(snowlark_paint::ImageId(1), 100.0, 50.0),
        );
        (grid, img)
    };
    let (g1, i1) = grid_with(&mut t, |s| s.height = SizeValue::px(25.0));
    let (g2, i2) = grid_with(&mut t, |s| s.width = SizeValue::px(50.0));
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, g1).width, 50.0);
    assert_eq!((rect(&f, i1).width, rect(&f, i1).height), (50.0, 25.0));
    assert_eq!(rect(&f, g2).width, 50.0);
    assert_eq!((rect(&f, i2).width, rect(&f, i2).height), (50.0, 25.0));
}

#[test]
fn grid_tracks_and_end_padding_are_scrollable_overflow() {
    // css-grid/grid-model/grid-overflow-padding-001.html: an empty grid whose track and end padding
    // overflow its 100px border box scrolls to them (CSS Overflow 3 §2.2).
    let mut t = T::new();
    let r0 = t.root();
    let grid = t.doc.block(r0, |s| {
        s.display = Display::GRID;
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
        s.box_sizing = BoxSizing::BorderBox;
        s.padding = [
            LengthPercentage::ZERO,
            LengthPercentage::px(50.0),
            LengthPercentage::px(50.0),
            LengthPercentage::ZERO,
        ];
        s.overflow_x = Overflow::Auto;
        s.overflow_y = Overflow::Auto;
        s.grid_template_columns = tracks(vec![px_track(100.0)]);
        s.grid_template_rows = tracks(vec![px_track(100.0)]);
    });
    let f = t.run(400.0, 400.0);
    let (_, gf) = f.find(grid).unwrap();
    assert_eq!(gf.scrollable_overflow(), r(0.0, 0.0, 150.0, 150.0));
}

#[test]
fn rtl_grid_aligns_abspos_children_by_physical_left_and_right() {
    // css-grid/abspos/grid-abspos-staticpos-justify-self-rtl-001.html: in a right-to-left grid,
    // `start` is the right edge of the area, `left`/`right` stay physical (css-align-3 §4.1).
    let mut t = T::new();
    let r0 = t.root();
    let place = |t: &mut T, j: Align| {
        let grid = t.doc.block(r0, |s| {
            s.display = Display::GRID;
            s.direction = Direction::Rtl;
            s.position = Position::Relative;
            s.width = SizeValue::px(30.0);
            s.height = SizeValue::px(20.0);
            s.grid_template_columns = tracks(vec![px_track(2.0), px_track(20.0), px_track(2.0)]);
        });
        let item = t.doc.block(grid, |s| {
            s.direction = Direction::Rtl;
            s.position = Position::Absolute;
            s.justify_self = j;
            s.grid_column_start = GridLine::Line(2);
            s.grid_column_end = GridLine::Line(3);
            s.width = SizeValue::px(6.0);
            s.height = SizeValue::px(8.0);
        });
        (grid, item)
    };
    let cases = [
        (Align::Start, 22.0),
        (Align::End, 8.0),
        (Align::Left, 8.0),
        (Align::Right, 22.0),
        (Align::Normal, 22.0),
    ];
    let placed: Vec<_> = cases.iter().map(|(j, _)| place(&mut t, *j)).collect();
    let f = t.run(400.0, 400.0);
    for ((grid, item), (j, x)) in placed.iter().zip(cases) {
        // The area (column line 2 to 3, from the right): x 8..28 of the 30px grid.
        assert_eq!(
            rect(&f, *item).x - rect(&f, *grid).x,
            x,
            "justify-self {j:?}"
        );
    }
}

#[test]
fn abspos_child_of_an_inline_grid_reaches_its_containing_block() {
    // css-grid/abspos/grid-positioned-item-dynamic-change-003.html: an absolutely positioned child
    // of an inline-level grid whose containing block is further up is laid out (it was dropped with
    // the atomic inline's other out-of-flow descendants).
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    let grid = t.doc.element(p, "span", |s| {
        s.display = Display::INLINE_GRID;
        s.grid_template_columns = tracks(vec![px_track(100.0)]);
        s.grid_template_rows = tracks(vec![px_track(100.0)]);
    });
    let item = t.doc.block(grid, |s| {
        s.position = Position::Absolute;
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
    });
    let f = t.run(400.0, 400.0);
    let g = rect(&f, grid);
    assert_eq!(rect(&f, item), r(g.x, g.y, 100.0, 100.0));
}

#[test]
fn percentage_height_in_a_grid_item_resolves_against_its_area_for_its_width() {
    // css-grid/grid-items/grid-item-inline-contribution-001.html: the item's `height: 100%` is the
    // 50px row, so a 2:1 replaced child with `height: 100%` contributes 100px to the column.
    let mut t = T::new();
    let r0 = t.root();
    let grid = t.doc.block(r0, |s| {
        s.display = Display::INLINE_GRID;
        s.height = SizeValue::px(100.0);
        s.grid_template_rows = tracks(vec![px_track(50.0)]);
    });
    let item = t.doc.block(grid, |s| s.height = SizeValue::percent(100.0));
    let canvas = t.doc.element(item, "canvas", |s| {
        s.display = Display::INLINE_BLOCK;
        s.height = SizeValue::percent(100.0);
    });
    t.doc.replace(
        canvas,
        ReplacedContent::image(snowlark_paint::ImageId(1), 20.0, 10.0),
    );
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, grid).width, 100.0);
    assert_eq!(rect(&f, canvas).width, 100.0);
}

#[test]
fn flexible_rows_of_an_auto_height_grid_are_sized_again_in_its_height() {
    // css-grid/layout-algorithm/flex-sizing-rows-indefinite-height.html: `minmax(0, .5fr)` with a
    // 200px item: the indefinite free space makes the grid 100px tall (an fr of 200px, css-grid-2
    // §11.7.1), and against that definite height the row is 50px, as the 100px-wide grid's column.
    let mut t = T::new();
    let r0 = t.root();
    let half_fr = || {
        tracks(vec![TrackListItem::Track(TrackSize::MinMax(
            TrackBreadth::LengthPercentage(LengthPercentage::ZERO),
            TrackBreadth::Fr(0.5),
        ))])
    };
    let grid = t.doc.block(r0, |s| {
        s.display = Display::INLINE_GRID;
        s.position = Position::Relative;
        s.grid_template_columns = half_fr();
        s.grid_template_rows = half_fr();
    });
    t.doc.block(grid, |s| {
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(200.0);
    });
    let abspos = t.doc.block(grid, |s| {
        s.position = Position::Absolute;
        s.grid_row_start = GridLine::Line(1);
        s.grid_row_end = GridLine::Line(2);
        s.grid_column_start = GridLine::Line(1);
        s.grid_column_end = GridLine::Line(2);
        s.width = SizeValue::percent(100.0);
        s.height = SizeValue::percent(100.0);
    });
    let f = t.run(400.0, 400.0);
    let g = rect(&f, grid);
    assert_eq!((g.width, g.height), (100.0, 100.0));
    let a = rect(&f, abspos);
    assert_eq!((a.width, a.height), (50.0, 50.0));
}

#[test]
fn white_space_only_text_in_a_grid_is_not_rendered_even_when_preserved() {
    // css-grid/grid-items/whitespace-in-grid-item-001.html: `white-space: pre` does not make a run of
    // spaces, tabs and newlines a grid item (css-grid-2 §6).
    let mut t = T::new();
    let r0 = t.root();
    let grid = t.doc.block(r0, |s| {
        s.display = Display::GRID;
        s.white_space_collapse = WhiteSpaceCollapse::Preserve;
    });
    t.doc.block(grid, |s| s.width = SizeValue::px(30.0));
    t.doc.text(grid, " \t\n  ");
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, grid).height, 0.0);
}

#[test]
fn clearing_floats_do_not_add_up_in_a_max_content_contribution() {
    // css-grid/grid-model/grid-floats-no-intrude-002.html: four `float: left; clear: both` boxes of
    // 30px stack, so the item's max-content contribution (and a max-content column) is 30px.
    let mut t = T::new();
    let r0 = t.root();
    let grid = t.doc.block(r0, |s| {
        s.display = Display::GRID;
        s.width = SizeValue::MaxContent;
    });
    let item = t.doc.block(grid, |_| {});
    for _ in 0..4 {
        t.doc.block(item, |s| {
            s.float = Float::Left;
            s.clear = Clear::Both;
            s.width = SizeValue::px(30.0);
            s.height = SizeValue::px(10.0);
        });
    }
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, grid).width, 30.0);
}

#[test]
fn grid_item_not_stretched_in_the_block_axis_is_as_tall_as_its_content_at_its_width() {
    // An item that `align-self` does not stretch (css-align-3 §6.1, css-grid-2 §6.2) takes its
    // content's height at its used width (CSS 2 §10.6.3), whether that width is its own `width` or
    // the stretched one. The content has inline-size containment (CSS Contain 2 §3.2), so its
    // max-content width is zero: two 150px floats are side by side in the item's 400px and would
    // stack at the content's shrink-to-fit width.
    for width in [
        SizeValue::percent(100.0),
        SizeValue::px(400.0),
        SizeValue::Auto,
    ] {
        let mut t = T::new();
        let r0 = t.root();
        let grid = t.doc.block(r0, |s| {
            s.display = Display::GRID;
            s.width = SizeValue::px(400.0);
            s.height = SizeValue::px(100.0);
            s.align_items = Align::Center;
        });
        let item = t.doc.block(grid, |s| s.width = width);
        let contained = t.doc.block(item, |s| {
            s.display = Display::FLOW_ROOT;
            s.size_containment = SizeContainment {
                width: true,
                ..Default::default()
            };
        });
        for _ in 0..2 {
            t.doc.block(contained, |s| {
                s.float = Float::Left;
                s.width = SizeValue::px(150.0);
                s.height = SizeValue::px(10.0);
            });
        }
        let f = t.run(800.0, 600.0);
        let (g, i) = (rect(&f, grid), rect(&f, item));
        assert_eq!((i.width, i.height), (400.0, 10.0), "width: {width:?}");
        assert_eq!(i.y - g.y, 45.0, "width: {width:?}");
    }
}

#[test]
fn aspect_ratio_height_of_a_grid_does_not_go_below_its_content() {
    // css-grid/alignment/grid-content-distribution-029.html: 50px wide with `aspect-ratio: 2` is 25px
    // tall, but `min-height: auto` makes the 100px item's height the minimum (css-sizing-4 §5.1).
    let mut t = T::new();
    let r0 = t.root();
    let grid = t.doc.block(r0, |s| {
        s.display = Display::GRID;
        s.width = SizeValue::px(100.0);
        s.max_width = MaxSizeValue::LengthPercentage(LengthPercentage::px(50.0));
        s.align_content = Align::Center;
        s.aspect_ratio = AspectRatio {
            auto: false,
            ratio: Some(2.0),
        };
    });
    let item = t.doc.block(grid, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, grid).height, 100.0);
    assert_eq!(rect(&f, item).y, rect(&f, grid).y);
}
