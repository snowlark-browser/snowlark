// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Overflow 3: the scrollable overflow rectangle (§2.2: end padding around the in-flow content,
//! transforms, zero-area boxes, single-axis clipping), the scroll origin and the unreachable
//! scrollable overflow region (§2.3), and `overflow-clip-margin` (§3.2).

use snowlark_dom::NodeId;
use snowlark_paint::{DisplayListBuilder, Item, Matrix};

use crate::style::*;
use crate::tests::*;
use crate::*;

fn scroller(t: &mut T, parent: NodeId, f: impl FnOnce(&mut TestStyle)) -> NodeId {
    t.doc.block(parent, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
        s.overflow_x = Overflow::Scroll;
        s.overflow_y = Overflow::Scroll;
        f(s);
    })
}

fn geometry(t: &T, n: NodeId) -> BoxGeometry {
    t.layout.geometry(n).expect("geometry")
}

/// scrollWidth and scrollHeight: the scrollable overflow rectangle's size.
fn scroll_size(t: &T, n: NodeId) -> (f32, f32) {
    let g = geometry(t, n);
    (g.scrollable_overflow.width, g.scrollable_overflow.height)
}

#[test]
fn end_padding_follows_the_in_flow_content() {
    // css-overflow/scrollable-overflow-padding.html: 10px 5px padding; a child 200px tall with a
    // 50px bottom margin, moved up by relative positioning (which does not move its in-flow
    // bounds), gives 10 + 200 + 50 + 10.
    let mut t = T::new();
    let r0 = t.root();
    let a = scroller(&mut t, r0, |s| {
        s.padding = [
            LengthPercentage::px(10.0),
            LengthPercentage::px(5.0),
            LengthPercentage::px(10.0),
            LengthPercentage::px(5.0),
        ];
    });
    t.doc.block(a, |s| {
        s.width = SizeValue::px(0.0);
        s.height = SizeValue::px(200.0);
        s.margin[2] = LengthPercentageAuto::px(50.0);
        s.position = Position::Relative;
        s.inset[0] = LengthPercentageAuto::px(-1000.0);
    });
    // ...and a child 200px wide with a 50px right margin: 5 + 200 + 50 + 5.
    let b = scroller(&mut t, r0, |s| {
        s.padding = [
            LengthPercentage::px(10.0),
            LengthPercentage::px(5.0),
            LengthPercentage::px(10.0),
            LengthPercentage::px(5.0),
        ];
    });
    t.doc.block(b, |s| {
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(0.0);
        s.margin[1] = LengthPercentageAuto::px(50.0);
    });
    // Margins collapsing through the last child count as they collapse: 100 and -50 give 50.
    let c = scroller(&mut t, r0, |s| {
        s.padding = [
            LengthPercentage::px(10.0),
            LengthPercentage::px(5.0),
            LengthPercentage::px(10.0),
            LengthPercentage::px(5.0),
        ];
    });
    let mid = t.doc.block(c, |s| {
        s.width = SizeValue::px(0.0);
        s.margin[2] = LengthPercentageAuto::px(100.0);
    });
    t.doc.block(mid, |s| {
        s.height = SizeValue::px(200.0);
        s.margin[2] = LengthPercentageAuto::px(-50.0);
    });
    t.run(800.0, 600.0);
    assert_eq!(scroll_size(&t, a).1, 270.0);
    assert_eq!(scroll_size(&t, b).0, 260.0);
    assert_eq!(scroll_size(&t, c).1, 270.0);
}

#[test]
fn end_padding_only_for_scroll_containers() {
    // A box that is not a scroll container measures its descendants' boxes only.
    let mut t = T::new();
    let r0 = t.root();
    let a = t.doc.block(r0, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
        s.padding_px(10.0);
    });
    t.doc.block(a, |s| s.height = SizeValue::px(150.0));
    t.run(800.0, 600.0);
    // (the padding box is 120 tall; the child reaches 10 + 150)
    assert_eq!(scroll_size(&t, a), (120.0, 160.0));
}

#[test]
fn line_boxes_count_atomic_inline_margins() {
    // An inline-block's end margin is part of its line's content.
    let mut t = T::new();
    let r0 = t.root();
    let a = scroller(&mut t, r0, |s| s.padding_px(5.0));
    t.doc.element(a, "span", |s| {
        s.display = Display::INLINE_BLOCK;
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(10.0);
        s.margin[1] = LengthPercentageAuto::px(50.0);
    });
    t.run(800.0, 600.0);
    assert_eq!(scroll_size(&t, a).0, 260.0);
}

#[test]
fn rtl_scroll_origin_is_on_the_right() {
    // css-overflow §2.3: in rtl the overflow to the left is reachable and that to the right is not;
    // scroll positions run from -(overflow) to 0.
    let mut t = T::new();
    let r0 = t.root();
    let a = scroller(&mut t, r0, |s| s.direction = Direction::Rtl);
    t.doc.block(a, |s| {
        s.width = SizeValue::px(300.0);
        s.height = SizeValue::px(10.0);
        s.margin[1] = LengthPercentageAuto::px(-100.0);
    });
    t.run(800.0, 600.0);
    let g = geometry(&t, a);
    assert_eq!(g.scroll_origin_end, (true, false));
    // (the child spans -100..200 of the padding box: 100 to the left is reachable, the right
    // overflow is not)
    assert_eq!(scroll_size(&t, a).0, 200.0);
    let (min, max) = g.scroll_range();
    assert_eq!((min.x, max.x), (-100.0, 0.0));
}

#[test]
fn reversed_flex_containers_flip_the_scroll_origin() {
    let mut t = T::new();
    let r0 = t.root();
    let mut make = |dir: FlexDirection, wrap: FlexWrap| {
        scroller(&mut t, r0, |s| {
            s.display = Display::FLEX;
            s.flex_direction = dir;
            s.flex_wrap = wrap;
        })
    };
    let row_reverse = make(FlexDirection::RowReverse, FlexWrap::Nowrap);
    let column_reverse = make(FlexDirection::ColumnReverse, FlexWrap::Nowrap);
    let wrap_reverse = make(FlexDirection::Row, FlexWrap::WrapReverse);
    let row = make(FlexDirection::Row, FlexWrap::Nowrap);
    t.run(800.0, 600.0);
    assert_eq!(geometry(&t, row_reverse).scroll_origin_end, (true, false));
    assert_eq!(
        geometry(&t, column_reverse).scroll_origin_end,
        (false, true)
    );
    assert_eq!(geometry(&t, wrap_reverse).scroll_origin_end, (false, true));
    assert_eq!(geometry(&t, row).scroll_origin_end, (false, false));
}

#[test]
fn flex_item_margins_reach_the_scrollable_overflow() {
    // css-overflow/scrollable-overflow-with-flex-item-margin-inline-end.html: the item shrinks to
    // 0, its 950px end margin still counts.
    let mut t = T::new();
    let r0 = t.root();
    let a = scroller(&mut t, r0, |s| s.display = Display::FLEX);
    t.doc.block(a, |s| {
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
        s.margin[1] = LengthPercentageAuto::px(950.0);
    });
    t.run(800.0, 600.0);
    assert_eq!(scroll_size(&t, a).0, 950.0);
}

#[test]
fn transformed_boxes_contribute_their_transformed_bounds() {
    // css-overflow/scrollable-overflow-transform-001.html.
    let mut t = T::new();
    let r0 = t.root();
    let a = scroller(&mut t, r0, |s| s.overflow_x = Overflow::Auto);
    t.doc.block(a, |s| {
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
        s.transform = Some(Matrix::translation(200.0, 0.0));
    });
    t.run(800.0, 600.0);
    assert_eq!(scroll_size(&t, a).0, 250.0);
}

#[test]
fn zero_width_boxes_still_extend_the_height() {
    // css-overflow/scrollable-overflow-zero-one-axis.html.
    let mut t = T::new();
    let r0 = t.root();
    let a = t.doc.block(r0, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(0.0);
        s.overflow_x = Overflow::Scroll;
        s.overflow_y = Overflow::Scroll;
    });
    t.doc.block(a, |s| {
        s.width = SizeValue::px(0.0);
        s.height = SizeValue::px(100.0);
    });
    t.run(800.0, 600.0);
    assert_eq!(scroll_size(&t, a).1, 100.0);
}

#[test]
fn single_axis_clip_lets_the_other_axis_overflow() {
    // `overflow-x: clip; overflow-y: visible`: the tall child still reaches the parent's
    // scrollable overflow, the wide one does not.
    let mut t = T::new();
    let r0 = t.root();
    let outer = scroller(&mut t, r0, |_| {});
    let clip = t.doc.block(outer, |s| {
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
        s.overflow_x = Overflow::Clip;
    });
    t.doc.block(clip, |s| {
        s.width = SizeValue::px(400.0);
        s.height = SizeValue::px(300.0);
    });
    t.run(800.0, 600.0);
    assert_eq!(scroll_size(&t, outer), (100.0, 300.0));
}

#[test]
fn overflow_clip_margin_moves_the_clip_edge() {
    let mut t = T::new();
    let r0 = t.root();
    let outer = scroller(&mut t, r0, |_| {});
    let clip = t.doc.block(outer, |s| {
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
        s.overflow_x = Overflow::Clip;
        s.overflow_y = Overflow::Clip;
        s.overflow_clip_margin = OverflowClipMargin {
            reference: BoxArea::PaddingBox,
            margin: 20.0,
        };
    });
    t.doc.block(clip, |s| {
        s.width = SizeValue::px(400.0);
        s.height = SizeValue::px(400.0);
    });
    let f = t.run(800.0, 600.0);
    // (the clip edge is 20px outside the 50px padding box: 70 wide; the scroller is 100)
    assert_eq!(scroll_size(&t, outer), (100.0, 100.0));
    let (_, cf) = f.find(clip).unwrap();
    assert_eq!(cf.overflow_contribution(), r(0.0, 0.0, 70.0, 70.0));
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let list = b.build();
    assert!(list
        .items
        .iter()
        .any(|i| matches!(i, Item::PushClipRect { rect } if rect.width == 90.0)));
}

fn clamp(lines: Option<u32>, auto: bool) -> Option<LineClamp> {
    Some(LineClamp {
        lines,
        auto,
        ellipsis: Some("\u{2026}".to_string()),
    })
}

/// A block of five preserved lines of Ahem text, 10px each.
fn five_lines(t: &mut T, parent: NodeId, f: impl FnOnce(&mut TestStyle)) -> NodeId {
    let b = t.doc.block(parent, |s| {
        s.white_space_collapse = WhiteSpaceCollapse::Preserve;
        f(s);
    });
    t.doc.text(b, "L1\nL2\nL3\nL4\nL5");
    b
}

#[test]
fn line_clamp_shows_max_lines_with_an_ellipsis() {
    // css-overflow-4/line-clamp-001.html: four of five lines, the fourth ending in an ellipsis.
    let mut t = T::new();
    let r0 = t.root();
    let c = five_lines(&mut t, r0, |s| s.line_clamp = clamp(Some(4), false));
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, c).height, 40.0);
    assert_eq!(lines(&f, c).len(), 4);
    // The last line holds "L4" and the ellipsis: three Ahem glyphs.
    let (_, cf) = f.find(c).unwrap();
    let last = &cf.children.last().unwrap().fragment;
    assert_eq!(last.inflow.unwrap().width, 30.0);
}

#[test]
fn line_clamp_does_nothing_when_the_lines_fit() {
    let mut t = T::new();
    let r0 = t.root();
    let c = five_lines(&mut t, r0, |s| s.line_clamp = clamp(Some(5), false));
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, c).height, 50.0);
    let (_, cf) = f.find(c).unwrap();
    let last = &cf.children.last().unwrap().fragment;
    assert_eq!(last.inflow.unwrap().width, 20.0);
}

#[test]
fn line_clamp_counts_lines_through_nested_blocks() {
    // The clamp point is after the container's third line, in its second child; the third child
    // is not laid out.
    let mut t = T::new();
    let r0 = t.root();
    let c = t.doc.block(r0, |s| s.line_clamp = clamp(Some(3), false));
    let a = t.doc.block(c, |_| {});
    t.doc.text(a, "A1");
    let b = five_lines(&mut t, c, |_| {});
    let after = t.doc.block(c, |_| {});
    t.doc.text(after, "Z");
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, c).height, 30.0);
    assert_eq!(lines(&f, b).len(), 2);
    assert!(f.find(after).is_none());
}

#[test]
fn line_clamp_auto_clamps_at_the_max_height() {
    // css-overflow-4/line-clamp-auto-001.html: `max-height` of 2.5 lines shows two.
    let mut t = T::new();
    let r0 = t.root();
    let c = five_lines(&mut t, r0, |s| {
        s.line_clamp = clamp(None, true);
        s.max_height = MaxSizeValue::px(25.0);
    });
    let f = t.run(800.0, 600.0);
    assert_eq!(lines(&f, c).len(), 2);
    assert_eq!(rect(&f, c).height, 20.0);
}

#[test]
fn independent_formatting_contexts_do_not_count_toward_the_clamp() {
    let mut t = T::new();
    let r0 = t.root();
    let c = t.doc.block(r0, |s| s.line_clamp = clamp(Some(2), false));
    let ib = t.doc.element(c, "span", |s| {
        s.display = Display::INLINE_BLOCK;
        s.white_space_collapse = WhiteSpaceCollapse::Preserve;
    });
    t.doc.text(ib, "I1\nI2\nI3");
    t.doc.text(c, " ");
    let f = t.run(800.0, 600.0);
    // (one line holding the three-line inline-block: nothing to clamp)
    assert_eq!(lines(&f, c).len(), 1);
    assert_eq!(rect(&f, ib).height, 30.0);
}

#[test]
fn line_clamp_auto_can_clamp_between_blocks() {
    // css-overflow-4/line-clamp-auto-034.html: the last possible clamp point that fits is after a
    // block (an independent formatting context, whose own lines do not count): no line gets an
    // ellipsis, and what follows is not laid out.
    let mut t = T::new();
    let r0 = t.root();
    let c = t.doc.block(r0, |s| {
        s.line_clamp = clamp(None, true);
        s.max_height = MaxSizeValue::px(25.0);
    });
    let a = t.doc.block(c, |_| {});
    t.doc.text(a, "A1");
    let b = t.doc.block(c, |s| s.display = Display::FLOW_ROOT);
    t.doc.text(b, "B1");
    let z = t.doc.block(c, |s| s.display = Display::FLOW_ROOT);
    t.doc.text(z, "Z1");
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, c).height, 20.0);
    assert!(f.find(z).is_none());
    // (no ellipsis: "A1" is two glyphs wide)
    let (_, af) = f.find(a).unwrap();
    assert_eq!(af.children[0].fragment.inflow.unwrap().width, 20.0);
}

#[test]
fn hit_testing_follows_rounded_overflow_clips() {
    // css-overflow/hit-test-stacking-context-parent-border-radius.html: a point inside the
    // parent's box but outside its padding box curve misses the clipped child.
    let mut t = T::new();
    let r0 = t.root();
    let parent = t.doc.block(r0, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
        s.overflow_x = Overflow::Hidden;
        s.overflow_y = Overflow::Hidden;
        s.border_radius = [(
            LengthPercentage::percent(50.0),
            LengthPercentage::percent(50.0),
        ); 4];
    });
    let child = t.doc.block(parent, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
        s.background_color = snowlark_paint::Color::BLACK;
    });
    let f = t.run(800.0, 600.0);
    let at =
        |x: f32, y: f32| hit_test(&f, snowlark_paint::Point::new(x, y), t.viewport).map(|h| h.node);
    assert_ne!(at(27.0, 95.0), Some(child));
    assert_eq!(at(32.0, 90.0), Some(child));
}

/// A block with an overflow clip, its size given.
fn clip_box(t: &mut T, parent: NodeId, width: f32, height: f32) -> NodeId {
    t.doc.block(parent, |s| {
        s.width = SizeValue::px(width);
        s.height = SizeValue::px(height);
        s.overflow_x = Overflow::Hidden;
        s.overflow_y = Overflow::Hidden;
    })
}

#[test]
fn hit_testing_tests_a_clip_in_the_coordinates_it_was_set_in() {
    // CSSOM View `elementFromPoint` with CSS Overflow 3 and CSS Transforms 1: a clip applies in
    // the coordinate space of the box that sets it, whatever transforms lie between it and what
    // it clips (a sidebar sliding panels with `translateX` inside `overflow: hidden`).
    let translate = |x: f32| Some(Matrix::translation(x, 0.0));
    let mut t = T::new();
    let r0 = t.root();

    // A transform inside a clip: a 400 px track moved left by 200 px shows its second half.
    let clip = clip_box(&mut t, r0, 200.0, 100.0);
    let track = t.doc.block(clip, |s| {
        s.width = SizeValue::px(400.0);
        s.height = SizeValue::px(100.0);
        s.transform = translate(-200.0);
    });
    let second = t.doc.block(track, |s| {
        s.margin[3] = LengthPercentageAuto::px(200.0);
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(100.0);
    });
    let text = t.doc.text(second, "hi");

    // A clip inside a transform, with a transform inside it: the clip box is moved to x 300, its
    // 200 px child moved back 50 px, so the child shows at 300..400 only.
    let outer = t.doc.block(r0, |s| {
        s.height = SizeValue::px(100.0);
        s.transform = translate(300.0);
    });
    let clip2 = clip_box(&mut t, outer, 100.0, 100.0);
    let moved = t.doc.block(clip2, |s| {
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(100.0);
        s.transform = translate(-50.0);
    });

    // Nested clips across a transform: a 120 px clip holds a box moved right by 50 px whose own
    // 100 px clip holds a wide child, which shows at 50..120 only.
    let clip3 = clip_box(&mut t, r0, 120.0, 100.0);
    let shifted = t.doc.block(clip3, |s| {
        s.height = SizeValue::px(100.0);
        s.transform = translate(50.0);
    });
    let inner = clip_box(&mut t, shifted, 100.0, 100.0);
    let wide = t.doc.block(inner, |s| {
        s.width = SizeValue::px(300.0);
        s.height = SizeValue::px(100.0);
    });

    let f = t.run(800.0, 600.0);
    let at =
        |x: f32, y: f32| hit_test(&f, snowlark_paint::Point::new(x, y), t.viewport).map(|h| h.node);
    // (all compared at once, so a failure shows every case)
    let hits = [
        at(60.0, 50.0) == Some(second),
        at(190.0, 50.0) == Some(second),
        at(390.0, 150.0) == Some(moved),
        at(280.0, 150.0) != Some(moved),
        at(100.0, 250.0) == Some(wide),
        at(130.0, 250.0) != Some(wide),
        at(40.0, 250.0) != Some(wide),
        // the caret too: the only text, in the moved track, is inside the clip
        caret_position(
            &f,
            snowlark_paint::Point::new(5.0, 5.0),
            &PaintOptions::viewport(t.viewport),
            &|_| true,
        )
        .map(|c| c.0)
            == Some(text),
    ];
    assert_eq!(hits, [true; 8]);
}

#[test]
fn paint_containment_clips_at_the_overflow_clip_edge() {
    // css-overflow/overflow-clip-margin-004.html: `contain: paint` clips like `overflow: clip`,
    // `overflow-clip-margin` included, and makes an independent formatting context.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
        s.paint_containment = true;
        s.overflow_clip_margin = OverflowClipMargin {
            reference: BoxArea::PaddingBox,
            margin: 10.0,
        };
    });
    t.doc.block(p, |s| {
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(200.0);
    });
    let f = t.run(800.0, 600.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let list = b.build();
    assert!(list
        .items
        .iter()
        .any(|i| matches!(i, Item::PushClipRect { rect } if rect.width == 120.0)));
    assert!(t
        .layout
        .boxes()
        .b(t.layout.boxes().box_of(p).unwrap())
        .is_independent());
}

#[test]
fn line_clamp_auto_with_nothing_fitting_shows_nothing() {
    // css-overflow-4/line-clamp-auto-037.html: a max-height smaller than the first line leaves
    // the box empty, its content height 0.
    let mut t = T::new();
    let r0 = t.root();
    let c = five_lines(&mut t, r0, |s| {
        s.line_clamp = clamp(None, true);
        s.max_height = MaxSizeValue::px(5.0);
    });
    let f = t.run(800.0, 600.0);
    assert_eq!(lines(&f, c).len(), 0);
    assert_eq!(rect(&f, c).height, 0.0);
}

#[test]
fn line_clamp_auto_counts_the_block_end_padding_around_a_line() {
    // css-overflow-4/line-clamp-auto-020.html: a line in a padded descendant fits only with that
    // descendant's block-end padding.
    let mut t = T::new();
    let r0 = t.root();
    let c = t.doc.block(r0, |s| {
        s.line_clamp = clamp(None, true);
        s.max_height = MaxSizeValue::px(25.0);
    });
    let inner = five_lines(&mut t, c, |s| s.padding[2] = LengthPercentage::px(10.0));
    let f = t.run(800.0, 600.0);
    // (the second line ends at 20, 30 with the padding: only the first fits)
    assert_eq!(lines(&f, inner).len(), 1);
    assert_eq!(rect(&f, c).height, 20.0);
}

#[test]
fn a_clamped_container_clips_its_floats_at_the_clamp_point() {
    // css-overflow-4/line-clamp-with-floats-005.html and line-clamp-011.html: a float hanging
    // below the clamp point is clipped at the clamped content's edge (not at a fixed height),
    // and nothing else is.
    let mut t = T::new();
    let r0 = t.root();
    let c = t.doc.block(r0, |s| {
        s.line_clamp = clamp(Some(2), false);
        s.white_space_collapse = WhiteSpaceCollapse::Preserve;
        s.height = SizeValue::px(5.0);
    });
    t.doc.text(
        c, "L1
L2",
    );
    t.doc.block(c, |s| {
        s.float = Float::Left;
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(100.0);
    });
    t.doc.text(
        c, "
L3",
    );
    let f = t.run(800.0, 600.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let list = b.build();
    let clips: Vec<f32> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::PushClipRect { rect } => Some(rect.bottom()),
            _ => None,
        })
        .collect();
    assert_eq!(clips, vec![20.0]);
}

/// Custom scroll bars (`::-webkit-scrollbar`) `w` px thick, with a red thumb and a blue track.
fn custom_bars(w: f32) -> Option<CustomScrollbar> {
    Some(CustomScrollbar {
        vertical: Some(w),
        horizontal: Some(w),
        bars: [CustomBar {
            thumb: Some(CustomScrollbarThumb {
                color: snowlark_paint::Color::rgb(255, 0, 0),
                radius: (0.0, 0.0),
                inset: 0.0,
                min_length: None,
            }),
            track: Some(snowlark_paint::Color::rgb(0, 0, 255)),
        }; 2],
        corner: None,
    })
}

#[test]
fn custom_scroll_bars_take_space_from_the_content_box() {
    // CSS Overflow 3 §3: a scroll bar sits between the border and the padding and its space comes
    // out of the content box; custom bars are not overlay bars, so they take it. 100px square
    // boxes, 8px bars.
    let mut t = T::new();
    let r0 = t.root();
    // `scroll` on both axes: both bars, overflow or not; `width` stays the border box's.
    let a = scroller(&mut t, r0, |s| s.custom_scrollbar = custom_bars(8.0));
    let a1 = t.doc.block(a, |s| s.height = SizeValue::px(20.0));
    // `auto`, overflowing in the block axis: a vertical bar only.
    let b = scroller(&mut t, r0, |s| {
        s.overflow_x = Overflow::Auto;
        s.overflow_y = Overflow::Auto;
        s.custom_scrollbar = custom_bars(8.0);
    });
    let b1 = t.doc.block(b, |s| s.height = SizeValue::px(300.0));
    // `auto`, not overflowing: no bar.
    let c = scroller(&mut t, r0, |s| {
        s.overflow_x = Overflow::Auto;
        s.overflow_y = Overflow::Auto;
        s.custom_scrollbar = custom_bars(8.0);
    });
    let c1 = t.doc.block(c, |s| s.height = SizeValue::px(50.0));
    // Right to left: the vertical bar is at the left.
    let d = scroller(&mut t, r0, |s| {
        s.direction = Direction::Rtl;
        s.overflow_x = Overflow::Hidden;
        s.custom_scrollbar = custom_bars(8.0);
    });
    let d1 = t.doc.block(d, |s| s.height = SizeValue::px(20.0));
    // `scrollbar-width` wins over the parts: overlay bars, no space.
    let e = scroller(&mut t, r0, |s| {
        s.scrollbar_width = ScrollbarWidth::Thin;
        s.custom_scrollbar = custom_bars(8.0);
    });
    let e1 = t.doc.block(e, |s| s.height = SizeValue::px(20.0));
    t.run(800.0, 600.0);

    let ga = geometry(&t, a);
    assert_eq!(ga.border_box.width, 100.0);
    assert_eq!((ga.padding_box.width, ga.padding_box.height), (92.0, 92.0));
    assert_eq!(geometry(&t, a1).border_box.width, 92.0);
    assert_eq!(scroll_size(&t, a), (92.0, 92.0));

    let gb = geometry(&t, b);
    assert_eq!((gb.padding_box.width, gb.padding_box.height), (92.0, 100.0));
    assert_eq!(geometry(&t, b1).border_box.width, 92.0);
    assert_eq!(scroll_size(&t, b), (92.0, 300.0));

    assert_eq!(geometry(&t, c).padding_box.width, 100.0);
    assert_eq!(geometry(&t, c1).border_box.width, 100.0);

    let gd = geometry(&t, d);
    assert_eq!(gd.padding_box.x - gd.border_box.x, 8.0);
    assert_eq!(geometry(&t, d1).border_box.x - gd.border_box.x, 8.0);
    assert_eq!(geometry(&t, d1).border_box.width, 92.0);

    assert_eq!(geometry(&t, e1).border_box.width, 100.0);
}

#[test]
fn custom_scroll_bars_add_to_shrink_to_fit_and_flex_content_boxes() {
    // A float sized by its content grows by its vertical bar (its intrinsic sizes include it); a
    // flex container's items stretch in what the bars leave.
    let mut t = T::new();
    let r0 = t.root();
    let a = t.doc.block(r0, |s| {
        s.float = Float::Left;
        s.height = SizeValue::px(50.0);
        s.overflow_y = Overflow::Scroll;
        s.overflow_x = Overflow::Hidden;
        s.custom_scrollbar = custom_bars(10.0);
    });
    let a1 = t.doc.block(a, |s| {
        s.width = SizeValue::px(60.0);
        s.height = SizeValue::px(10.0);
    });
    let f = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.clear = Clear::Both;
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(100.0);
        s.overflow_x = Overflow::Scroll;
        s.overflow_y = Overflow::Scroll;
        s.custom_scrollbar = custom_bars(10.0);
    });
    let f1 = t.doc.block(f, |s| s.flex_grow = 1.0);
    t.run(800.0, 600.0);
    assert_eq!(geometry(&t, a).border_box.width, 70.0);
    assert_eq!(geometry(&t, a1).border_box.width, 60.0);
    let gf = geometry(&t, f);
    assert_eq!((gf.padding_box.width, gf.padding_box.height), (90.0, 90.0));
    let g1 = geometry(&t, f1);
    assert_eq!((g1.border_box.width, g1.border_box.height), (90.0, 90.0));
}

#[test]
fn custom_scroll_bars_are_drawn_in_their_space() {
    // The track fills the space a bar takes (with nothing to scroll too, under `overflow:
    // scroll`); the thumb is drawn only where there is something to scroll, no shorter than the
    // thumb part's `min-height` (40px; the proportional length is 25px).
    use snowlark_paint::Color;
    let mut t = T::new();
    let r0 = t.root();
    let a = scroller(&mut t, r0, |s| {
        s.overflow_x = Overflow::Hidden;
        s.custom_scrollbar = custom_bars(8.0);
        if let Some(CustomScrollbar { bars, .. }) = &mut s.custom_scrollbar {
            if let Some(thumb) = &mut bars[0].thumb {
                thumb.min_length = Some(40.0);
            }
        }
    });
    t.doc.block(a, |s| s.height = SizeValue::px(400.0));
    let f = t.run(800.0, 600.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let list = b.build();
    let track = list.items.iter().find_map(|i| match i {
        Item::FillRect { rect, color } if *color == Color::rgb(0, 0, 255) => Some(*rect),
        _ => None,
    });
    assert_eq!(
        track,
        Some(snowlark_paint::Rect::new(92.0, 0.0, 8.0, 100.0))
    );
    let thumb = list.items.iter().find_map(|i| match i {
        Item::FillRoundedRect { rect, color, .. } | Item::FillRect { rect, color }
            if *color == Color::rgb(255, 0, 0) =>
        {
            Some(*rect)
        }
        _ => None,
    });
    assert_eq!(thumb, Some(snowlark_paint::Rect::new(92.0, 0.0, 8.0, 40.0)));
}
