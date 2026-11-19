// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Flex layout tests (css-flexbox-1): the taffy integration and the item sizing inputs Snowlark computes,
//! mostly transcribed from WPT css-flexbox reftests.

use crate::style::*;
use crate::tests::*;
use crate::*;

/// The border and padding widths of `node`'s box fragment.
fn edges(f: &F, node: snowlark_dom::NodeId) -> (snowlark_paint::Sides, snowlark_paint::Sides) {
    let (_, fr) = f.find(node).expect("fragment");
    match &fr.kind {
        FragmentKind::Box(b) => (b.border, b.padding),
        _ => panic!("not a box fragment"),
    }
}

#[test]
fn flex_container_in_flow_keeps_its_border_and_padding() {
    // css-flexbox/flexbox_flex-N-0.html: a block-level flex container's own border (CSS 2 §8.5) is
    // painted; its fragment carries the border and padding widths.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.width = SizeValue::px(100.0);
        s.border_width = [1.0, 2.0, 3.0, 4.0];
        s.border_style = [BorderStyle::Solid; 4];
        s.padding = [LengthPercentage::px(5.0); 4];
    });
    let item = t.doc.block(flex, |s| s.height = SizeValue::px(10.0));
    let f = t.run(400.0, 400.0);
    let (border, padding) = edges(&f, flex);
    assert_eq!(border, snowlark_paint::Sides::new(1.0, 2.0, 3.0, 4.0));
    assert_eq!(padding, snowlark_paint::Sides::new(5.0, 5.0, 5.0, 5.0));
    assert_eq!(rect(&f, flex), r(0.0, 0.0, 116.0, 24.0));
    assert_eq!(rect(&f, item), r(9.0, 6.0, 0.0, 10.0));
}

#[test]
fn flex_items_are_laid_out_in_order_modified_document_order() {
    // css-flexbox/order/order-with-row-reverse.html: ordinal groups ascending, document order within a
    // group (css-flexbox-1 §5.4), then the main axis direction.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.width = SizeValue::px(300.0);
    });
    let item = |t: &mut T, order: i32| {
        t.doc.block(flex, |s| {
            s.order = order;
            s.width = SizeValue::px(50.0);
            s.height = SizeValue::px(10.0);
        })
    };
    let a = item(&mut t, 1);
    let b = item(&mut t, 0);
    let c = item(&mut t, -1);
    let d = item(&mut t, 0);
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, c).x, 0.0);
    assert_eq!(rect(&f, b).x, 50.0);
    assert_eq!(rect(&f, d).x, 100.0);
    assert_eq!(rect(&f, a).x, 150.0);
    // A child's new `order` (a relayout change) reorders the items, and so does row-reverse.
    t.doc.restyle(a, |s| s.order = -2);
    t.doc
        .restyle(flex, |s| s.flex_direction = FlexDirection::RowReverse);
    let changes = TreeChanges {
        restyled: vec![
            ElementChange::restyled(a, BoxDamage::Relayout, false),
            ElementChange::restyled(flex, BoxDamage::Relayout, false),
        ],
        ..Default::default()
    };
    t.layout
        .update(&t.doc.dom, t.doc.document, &t.doc.styles, &changes);
    let f = t.relayout();
    assert_eq!(rect(&f, a).x, 250.0);
    assert_eq!(rect(&f, c).x, 200.0);
    assert_eq!(rect(&f, d).x, 100.0);
}

#[test]
fn item_content_size_ignores_its_own_height() {
    // css-flexbox-1 §4.5: the content size suggestion is the min-content size, not the item's
    // `height`; with `flex: 0 0 100px` a 500px-tall item is 100px tall (Chrome agrees).
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
        s.width = SizeValue::px(100.0);
    });
    let item = t.doc.block(flex, |s| {
        s.height = SizeValue::px(500.0);
        s.flex_grow = 0.0;
        s.flex_shrink = 0.0;
        s.flex_basis = FlexBasis::LengthPercentage(LengthPercentage::px(100.0));
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, item), r(0.0, 0.0, 100.0, 100.0));
    assert_eq!(rect(&f, flex).height, 100.0);
}

#[test]
fn table_item_takes_the_flexed_sizes() {
    // css-flexbox/table-as-item-specified-width.html and table-as-item-flex-cross-size.html: the
    // table's width is its preferred size, not a minimum (css-flexbox-1 §4), and a flexed or
    // stretched table wrapper's captions count as the table box's border and padding.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| s.display = Display::FLEX);
    let tw = table(&mut t, flex, |s| {
        s.width = SizeValue::px(500.0);
        s.height = SizeValue::px(100.0);
        s.flex_grow = 0.0;
        s.flex_shrink = 0.0;
        s.flex_basis = FlexBasis::LengthPercentage(LengthPercentage::px(100.0));
    });
    let column = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
    });
    let grown = table(&mut t, column, |s| s.flex_grow = 1.0);
    let caption = t.doc.block(grown, |s| {
        s.display = Display::TABLE_CAPTION;
        s.height = SizeValue::px(10.0);
    });
    let tr = row(&mut t, grown);
    let td = cell(&mut t, tr, |_| {});
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, tw), r(0.0, 0.0, 100.0, 100.0));
    assert_eq!(rect(&f, grown), r(0.0, 100.0, 100.0, 100.0));
    assert_eq!(rect(&f, caption), r(0.0, 100.0, 100.0, 10.0));
    assert_eq!(rect(&f, td).height, 90.0);
}

#[test]
fn replaced_item_minimum_is_its_cross_size_through_the_ratio() {
    // css-flexbox/flexbox-min-width-auto-002a.html: `min-width: auto` of an image with a definite
    // height is the smaller of its specified width and the height converted through its ratio
    // (css-flexbox-1 §4.5), so a 100px-wide image 30px tall shrinks to 30px in a 0px container.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.width = SizeValue::px(0.0);
    });
    let img = t.doc.element(flex, "img", |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(30.0);
        s.border_width = [2.0; 4];
        s.border_style = [BorderStyle::Solid; 4];
    });
    t.doc.replace(
        img,
        ReplacedContent::image(snowlark_paint::ImageId(1), 16.0, 16.0),
    );
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, img), r(0.0, 0.0, 34.0, 34.0));
}

#[test]
fn intrinsic_main_size_flexes_items_by_the_chosen_flex_fraction() {
    // css-flexbox/intrinsic-size/row-002.html, row-004.html, row-007.html (css-flexbox-1 §9.9.1.1):
    // a shrink-to-fit flex container is as wide as its items' flex base sizes flexed by the line's
    // chosen flex fraction, clamped by their min/max main sizes.
    let mut t = T::new();
    let r0 = t.root();
    let container = |t: &mut T, parent| {
        t.doc.block(parent, |s| {
            s.display = Display::FLEX;
            s.float = Float::Left;
            s.height = SizeValue::px(10.0);
        })
    };
    // row-002: an item that can't shrink keeps its flex base size in the max-content size.
    let a = container(&mut t, r0);
    t.doc.block(a, |s| {
        s.flex_grow = 1.0;
        s.flex_shrink = 0.0;
        s.flex_basis = FlexBasis::LengthPercentage(LengthPercentage::px(100.0));
    });
    // row-004: in a 0px-wide block, an item that can't grow takes its flex base size, not its
    // 200px content.
    let narrow = t.doc.block(r0, |s| {
        s.width = SizeValue::px(0.0);
        s.clear = Clear::Both;
    });
    let b = container(&mut t, narrow);
    let item = t.doc.block(b, |s| {
        s.flex_grow = 0.0;
        s.flex_shrink = 1.0;
        s.min_width = SizeValue::px(0.0);
        s.flex_basis = FlexBasis::LengthPercentage(LengthPercentage::px(100.0));
    });
    t.doc.block(item, |s| s.width = SizeValue::px(200.0));
    // row-007: max-width wins over an inflexible flex basis.
    let c = container(&mut t, r0);
    t.doc.block(c, |s| {
        s.flex_grow = 0.0;
        s.flex_shrink = 0.0;
        s.flex_basis = FlexBasis::LengthPercentage(LengthPercentage::px(200.0));
        s.box_sizing = BoxSizing::BorderBox;
        s.border_width = [10.0; 4];
        s.border_style = [BorderStyle::Solid; 4];
        s.max_width = MaxSizeValue::LengthPercentage(LengthPercentage::px(100.0));
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, a).width, 100.0);
    assert_eq!(rect(&f, b).width, 100.0);
    assert_eq!(rect(&f, c).width, 100.0);
}

#[test]
fn column_container_block_size_is_its_items_hypothetical_sizes() {
    // css-flexbox/flex-minimum-height-flex-items-031.html: a column container is as tall as its
    // items' hypothetical main sizes, also as the min-content height of a `flex-basis: 0` item.
    let mut t = T::new();
    let r0 = t.root();
    let outer = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
        s.float = Float::Left;
    });
    let inner = t.doc.block(outer, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
        s.flex_basis = FlexBasis::LengthPercentage(LengthPercentage::px(0.0));
    });
    let mut items = Vec::new();
    for _ in 0..2 {
        let item = t.doc.block(inner, |s| {
            s.flex_grow = 1.0;
            s.flex_shrink = 1.0;
            s.flex_basis = FlexBasis::LengthPercentage(LengthPercentage::px(44.0));
        });
        t.doc.text(item, "x");
        items.push(item);
    }
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, items[0]).height, 44.0);
    assert_eq!(rect(&f, items[1]).y, 44.0);
    assert_eq!(rect(&f, inner).height, 88.0);
    assert_eq!(rect(&f, outer).height, 88.0);
}

#[test]
fn container_baseline_comes_from_baseline_aligned_items() {
    // css-flexbox-1 §8.5: an item of the first line that participates in baseline alignment gives the
    // container its baseline, before the startmost item (css-flexbox/flexbox-align-self-horiz-001).
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.height = SizeValue::px(50.0);
    });
    let a = t.doc.block(flex, |s| s.align_self = Align::FlexEnd);
    t.doc.text(a, "a");
    let b = t.doc.block(flex, |s| s.align_self = Align::Baseline);
    t.doc.text(b, "b");
    let f = t.run(400.0, 400.0);
    let (_, fr) = f.find(flex).unwrap();
    assert_eq!(fr.baseline, Some(8.0));
}

#[test]
fn safe_alignment_keeps_overflowing_content_at_the_start() {
    // css-flexbox/flexbox-safe-overflow-position-002.html: `align-content: safe flex-start` in a
    // wrap-reverse container whose line overflows aligns it to the start (css-align-3 §4.4).
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.width = SizeValue::px(90.0);
        s.height = SizeValue::px(90.0);
        s.flex_wrap = FlexWrap::WrapReverse;
        s.align_content = Align::FlexStart;
        s.justify_content = Align::FlexStart;
        s.align_safety.align_content = OverflowPosition::Safe;
        s.align_safety.justify_content = OverflowPosition::Safe;
    });
    let item = t.doc.block(flex, |s| {
        s.flex_grow = 0.0;
        s.flex_shrink = 0.0;
        s.flex_basis = FlexBasis::LengthPercentage(LengthPercentage::px(100.0));
        s.height = SizeValue::px(100.0);
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, item), r(0.0, 0.0, 100.0, 100.0));
}

#[test]
fn justify_content_left_and_right_in_a_column_are_start() {
    // css-flexbox/flexbox-justify-content-vert-001a.html: the main axis of a column container is not
    // the inline axis, so `left` and `right` behave as `start` (css-align-3 §6.1).
    let mut t = T::new();
    let r0 = t.root();
    let mut items = Vec::new();
    for a in [Align::Left, Align::Right] {
        let flex = t.doc.block(r0, |s| {
            s.display = Display::FLEX;
            s.flex_direction = FlexDirection::Column;
            s.height = SizeValue::px(100.0);
            s.justify_content = a;
        });
        items.push(t.doc.block(flex, |s| s.height = SizeValue::px(10.0)));
    }
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, items[0]).y, 0.0);
    assert_eq!(rect(&f, items[1]).y, 100.0);
}

#[test]
fn overflowing_stretched_lines_fall_back_to_flex_start() {
    // css-flexbox/align-content-007.htm: `align-content: stretch` falls back to (unsafe) flex-start,
    // which is the bottom of a wrap-reverse container (css-align-3 §3.3).
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(200.0);
        s.flex_wrap = FlexWrap::WrapReverse;
        s.align_content = Align::Stretch;
    });
    let item = t.doc.block(flex, |s| {
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(400.0);
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, item).y, -200.0);
}

#[test]
fn collapsed_items_take_no_main_space_and_keep_their_strut() {
    // css-flexbox/flexbox_visibility-collapse.html (css-flexbox-1 §4.4, §9.4 step 10): a collapsed
    // item is not rendered and takes no main space, but its line keeps the item's cross size.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| s.display = Display::FLEX);
    let a = t.doc.block(flex, |s| {
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(10.0);
        s.align_self = Align::FlexStart;
    });
    let b = t.doc.block(flex, |s| {
        s.width = SizeValue::px(30.0);
        s.height = SizeValue::px(40.0);
        s.visibility = Visibility::Collapse;
    });
    let c = t.doc.block(flex, |s| s.width = SizeValue::px(20.0));
    let f = t.run(400.0, 400.0);
    assert!(f.find(b).is_none());
    assert_eq!(rect(&f, a), r(0.0, 0.0, 20.0, 10.0));
    assert_eq!(rect(&f, c), r(20.0, 0.0, 20.0, 40.0));
    assert_eq!(rect(&f, flex).height, 40.0);
}

#[test]
fn abspos_static_position_aligns_physically_in_rtl() {
    // css-flexbox/abspos/position-absolute-013.html: in an rtl row, `justify-content: flex-end` puts
    // the static position of an absolutely positioned child at the left (css-flexbox-1 §4.1).
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.position = Position::Relative;
        s.direction = Direction::Rtl;
        s.justify_content = Align::FlexEnd;
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
    });
    let abs = t.doc.block(flex, |s| {
        s.position = Position::Absolute;
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, abs), r(0.0, 0.0, 50.0, 50.0));
}

#[test]
fn percentage_height_image_sizes_a_stretched_item() {
    // css-flexbox/flex-basis-013.html, aspect-ratio-intrinsic-size-001.html: in a single-line
    // container with a definite height, a stretched item's height is definite (css-flexbox-1 §9.8),
    // so its image's `height: 100%` resolves while the item's width is measured.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.height = SizeValue::px(50.0);
    });
    let item = t.doc.block(flex, |s| s.min_width = SizeValue::px(0.0));
    let img = t.doc.element(item, "img", |s| {
        s.display = Display::BLOCK;
        s.height = SizeValue::percent(100.0);
    });
    t.doc.replace(
        img,
        ReplacedContent::image(snowlark_paint::ImageId(1), 5.0, 5.0),
    );
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, item), r(0.0, 0.0, 50.0, 50.0));
    assert_eq!(rect(&f, img), r(0.0, 0.0, 50.0, 50.0));
}

#[test]
fn container_intrinsic_width_uses_its_definite_height() {
    // css-flexbox/aspect-ratio-intrinsic-size-001.html: a shrink-to-fit container 100px tall holds a
    // stretched item whose image is `height: 100%`: the image, the item and the container are all
    // 100px wide.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.float = Float::Left;
        s.height = SizeValue::px(100.0);
    });
    let item = t.doc.block(flex, |_| {});
    let img = t.doc.element(item, "img", |s| {
        s.display = Display::BLOCK;
        s.height = SizeValue::percent(100.0);
    });
    t.doc.replace(
        img,
        ReplacedContent::image(snowlark_paint::ImageId(1), 10.0, 10.0),
    );
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, flex).width, 100.0);
    assert_eq!(rect(&f, item), r(0.0, 0.0, 100.0, 100.0));
}

#[test]
fn intrinsic_min_and_max_width_keywords_of_items() {
    // css-flexbox/flex-item-max-width-min-content-002.html: `max-width: min-content` caps a stretched
    // item at its min-content width (css-sizing-3 §3.2).
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
        s.width = SizeValue::px(200.0);
    });
    let item = t
        .doc
        .block(flex, |s| s.max_width = MaxSizeValue::MinContent);
    t.doc.text(item, "aa bbb c");
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, item).width, 30.0);
}

#[test]
fn table_item_height_counts_its_captions() {
    // css-flexbox/table-as-item-stretch-cross-size-3.html: a table item's `height` is its table box's,
    // its captions count as the box's border and padding (css-flexbox-1 §4): 90px + a 10px caption.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
        s.width = SizeValue::px(100.0);
    });
    let tw = table(&mut t, flex, |s| s.height = SizeValue::px(90.0));
    t.doc.block(tw, |s| {
        s.display = Display::TABLE_CAPTION;
        s.height = SizeValue::px(10.0);
    });
    let tr = row(&mut t, tw);
    let td = cell(&mut t, tr, |_| {});
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, tw).height, 100.0);
    assert_eq!(rect(&f, td).height, 90.0);
}

#[test]
fn column_wrap_intrinsic_width_is_not_measured_in_lines() {
    // css-flexbox/intrinsic-size/col-wrap-012.html: the max-content width of a multi-line column
    // container with a definite height takes its items' max-content widths.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
        s.flex_wrap = FlexWrap::Wrap;
        s.float = Float::Left;
        s.height = SizeValue::px(100.0);
    });
    let item = t.doc.block(flex, |s| {
        s.width = SizeValue::percent(100.0);
        s.flex_shrink = 0.0;
        s.min_height = SizeValue::px(0.0);
    });
    for _ in 0..2 {
        t.doc.block(item, |s| {
            s.float = Float::Left;
            s.width = SizeValue::px(50.0);
            s.height = SizeValue::px(50.0);
        });
    }
    t.doc.block(flex, |s| {
        s.width = SizeValue::px(90.0);
        s.height = SizeValue::px(50.0);
        s.flex_shrink = 0.0;
        s.min_height = SizeValue::px(0.0);
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, flex).width, 100.0);
}

#[test]
fn safe_static_position_falls_back_only_when_overflowing_the_containing_block() {
    // css-flexbox/abspos/flex-abspos-align-self-safe-outer-cb-003.tentative.html: `align-self: safe
    // center` of an absolutely positioned child wider than its flex container but not than its
    // containing block stays centered (css-align-3 §4.4: its alignment container is the containing
    // block).
    let mut t = T::new();
    let r0 = t.root();
    let cb = t.doc.block(r0, |s| {
        s.position = Position::Relative;
        s.width = SizeValue::px(200.0);
    });
    let flex = t.doc.block(cb, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
        s.margin[3] = LengthPercentageAuto::px(100.0);
    });
    let abs = t.doc.block(flex, |s| {
        s.position = Position::Absolute;
        s.width = SizeValue::px(60.0);
        s.height = SizeValue::px(40.0);
        s.align_self = Align::Center;
        s.align_safety.align_self = OverflowPosition::Safe;
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, abs).x, 95.0);
}
