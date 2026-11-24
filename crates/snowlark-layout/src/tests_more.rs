// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! More layout tests: WPT transcriptions, positioning against inline and flex containing blocks,
//! incremental rebuilds, painting effects and hit testing through them.

use snowlark_dom::NodeId;
use snowlark_paint::{DisplayListBuilder, Item, Point};

use crate::style::*;
use crate::tests::*;
use crate::*;

#[test]
fn colspan_widths_wpt() {
    // css-tables/colspan-001.html: spanning cells' widths distributed over the columns they span.
    let mut t = T::new();
    let r0 = t.root();
    let tb = table(&mut t, r0, |_| {});
    let square = |t: &mut T, tr: NodeId, span: Option<&str>, w: f32, h: f32| {
        let td = cell(t, tr, |_| {});
        if let Some(s) = span {
            t.doc.dom.set_attribute(td, "colspan", s).unwrap();
        }
        t.doc.block(td, |s| {
            s.width = SizeValue::px(w);
            s.height = SizeValue::px(h);
        });
        td
    };
    let tr = row(&mut t, tb);
    let blue = square(&mut t, tr, Some("2"), 50.0, 75.0);
    let green = square(&mut t, tr, Some("2"), 50.0, 75.0);
    let tr = row(&mut t, tb);
    let black = square(&mut t, tr, None, 50.0, 50.0);
    let lime = square(&mut t, tr, Some("2"), 50.0, 50.0);
    let pink = square(&mut t, tr, None, 50.0, 50.0);
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, blue), r(0.0, 0.0, 75.0, 75.0));
    assert_eq!(rect(&f, green), r(75.0, 0.0, 75.0, 75.0));
    assert_eq!(rect(&f, black), r(0.0, 75.0, 50.0, 50.0));
    assert_eq!(rect(&f, lime), r(50.0, 75.0, 50.0, 50.0));
    assert_eq!(rect(&f, pink), r(100.0, 75.0, 50.0, 50.0));
}

#[test]
fn abspos_in_flex_container_uses_the_flex_static_position() {
    // css-flexbox-1 §4.1: as if it were the sole flex item.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.position = Position::Relative;
        s.justify_content = Align::Center;
        s.align_items = Align::Center;
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(100.0);
    });
    let abs = t.doc.block(flex, |s| {
        s.position = Position::Absolute;
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(20.0);
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, abs), r(90.0, 40.0, 20.0, 20.0));
}

#[test]
fn abspos_in_a_relative_inline_box() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    t.doc.text(p, "aaaa");
    let span = t.doc.span(p, |s| s.position = Position::Relative);
    t.doc.text(span, "bb");
    let abs = t.doc.block(span, |s| {
        s.position = Position::Absolute;
        s.inset[0] = LengthPercentageAuto::px(0.0);
        s.inset[3] = LengthPercentageAuto::px(0.0);
        s.width = SizeValue::px(5.0);
        s.height = SizeValue::px(5.0);
    });
    let f = t.run(400.0, 400.0);
    // The containing block is the span's padding box: x 40, y 0.
    assert_eq!(rect(&f, abs), r(40.0, 0.0, 5.0, 5.0));
}

#[test]
fn style_change_rebuilds_from_the_parent() {
    let mut t = T::new();
    let r0 = t.root();
    let a = t.doc.block(r0, |s| s.height = SizeValue::px(10.0));
    let b = t.doc.block(r0, |s| s.height = SizeValue::px(10.0));
    let f = t.run(100.0, 100.0);
    assert_eq!(rect(&f, b).y, 10.0);
    t.doc.restyle(a, |s| s.display = Display::None);
    t.layout.style_changed(&t.doc.dom, a, &t.doc.styles);
    let f = t.relayout();
    assert!(f.find(a).is_none());
    assert_eq!(rect(&f, b).y, 0.0);
    // And back, as an inline box: an anonymous block wraps it.
    t.doc.restyle(a, |s| {
        s.display = Display::INLINE;
        s.height = SizeValue::Auto;
    });
    t.doc.text(a, "x");
    t.layout.style_changed(&t.doc.dom, a, &t.doc.styles);
    let f = t.relayout();
    assert_eq!(rect(&f, b).y, 10.0);
}

#[test]
fn rebuilt_list_item_keeps_its_ordinal() {
    let mut t = T::new();
    let r0 = t.root();
    let ol = t.doc.element(r0, "ol", |s| s.display = Display::BLOCK);
    let mut items = Vec::new();
    for _ in 0..3 {
        let li = t.doc.element(ol, "li", |s| {
            s.display = Display::LIST_ITEM;
            s.list_style_type = ListStyleType::Decimal;
        });
        t.doc.text(li, "x");
        items.push(li);
    }
    t.run(100.0, 100.0);
    t.doc.text(items[2], "y");
    t.layout
        .children_changed(&t.doc.dom, items[2], &t.doc.styles);
    t.relayout();
    assert!(t.dump().contains("Text anon \"3. \""), "{}", t.dump());
}

/// A `filter` box paints as a group of its functions, and a blur reaches past its border box: the
/// box's paint bounds, and the group's, hold three standard deviations around it (Filter Effects 1
/// §10).
#[test]
fn a_filter_is_a_group_whose_bounds_hold_the_blur() {
    use snowlark_paint::{FilterOp, Rect};
    let mut t = T::new();
    let r0 = t.root();
    t.doc.block(r0, |s| {
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(20.0);
        s.background_color = snowlark_paint::Color::BLACK;
        s.filters = vec![FilterOp::Blur(2.0), FilterOp::Sepia(1.0)];
    });
    let f = t.run(200.0, 200.0);
    // (the root's paint bounds take in the child's, 6 px around its 20 px box)
    assert_eq!(f.paint_bounds, Rect::new(-6.0, -6.0, 206.0, 206.0));
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let list = b.build();
    let group = list
        .items
        .iter()
        .find_map(|i| match i {
            Item::PushFilter { filters, bounds } => Some((list.filter(*filters), *bounds)),
            _ => None,
        })
        .expect("a filter group");
    assert_eq!(group.0, &[FilterOp::Blur(2.0), FilterOp::Sepia(1.0)]);
    assert_eq!(group.1, Rect::new(-6.0, -6.0, 32.0, 32.0));
    assert!(list.items.iter().any(|i| matches!(i, Item::PopFilter)));
}

/// Filter Effects 2 §3: the backdrop filter is drawn under the element's content, inside its
/// opacity group (which applies to it, and which it counts as its own), over the border box with
/// its corners; the element's own filter is applied to the backdrop after the backdrop's.
#[test]
fn a_backdrop_filter_is_drawn_inside_the_elements_opacity_under_its_content() {
    use snowlark_paint::{FilterOp, Radii, Rect};
    let mut t = T::new();
    let r0 = t.root();
    t.doc.block(r0, |s| {
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(30.0);
        s.margin = [LengthPercentageAuto::px(5.0); 4];
        s.background_color = snowlark_paint::Color::BLACK;
        s.opacity = 0.5;
        s.backdrop_filters = vec![FilterOp::Blur(3.0)];
        s.filters = vec![FilterOp::Invert(1.0)];
        s.border_radius = [(LengthPercentage::px(4.0), LengthPercentage::px(4.0)); 4];
    });
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let list = b.build();
    let at = |want: fn(&Item) -> bool| list.items.iter().position(want).unwrap();
    let opacity = at(|i| matches!(i, Item::PushOpacity { .. }));
    let backdrop = at(|i| matches!(i, Item::BackdropFilter { .. }));
    let group = at(|i| matches!(i, Item::PushFilter { .. }));
    assert!(opacity < backdrop && backdrop < group);
    let Item::BackdropFilter {
        filters,
        rect,
        radii,
        own,
    } = list.items[backdrop]
    else {
        unreachable!()
    };
    assert_eq!(
        list.filter(filters),
        &[FilterOp::Blur(3.0), FilterOp::Invert(1.0)]
    );
    assert_eq!(rect, Rect::new(5.0, 5.0, 20.0, 30.0));
    assert_eq!(radii, Radii::uniform(4.0));
    assert_eq!(own, 1);
}

/// The root element has no backdrop (Filter Effects 2 §3: nothing is painted before it within a
/// backdrop root): its backdrop filter draws nothing.
#[test]
fn the_root_elements_backdrop_filter_draws_nothing() {
    use snowlark_paint::FilterOp;
    let mut t = T::new();
    let r0 = t.root();
    t.doc
        .restyle(r0, |s| s.backdrop_filters = vec![FilterOp::Invert(1.0)]);
    let child = t.doc.block(r0, |s| {
        s.height = SizeValue::px(20.0);
        s.backdrop_filters = vec![FilterOp::Invert(1.0)];
    });
    let _ = child;
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let list = b.build();
    let rects: Vec<_> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::BackdropFilter { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect();
    assert_eq!(rects, [snowlark_paint::Rect::new(0.0, 0.0, 200.0, 20.0)]);
}

#[test]
fn transforms_and_opacity_in_the_display_list_and_hit_testing() {
    let mut t = T::new();
    let r0 = t.root();
    let moved = t.doc.block(r0, |s| {
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(20.0);
        s.background_color = snowlark_paint::Color::BLACK;
        s.transform = Some(snowlark_paint::Matrix::translation(100.0, 0.0));
        s.opacity = 0.5;
    });
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let list = b.build();
    assert!(list
        .items
        .iter()
        .any(|i| matches!(i, Item::PushTransform { .. })));
    assert!(list
        .items
        .iter()
        .any(|i| matches!(i, Item::PushOpacity { opacity, .. } if *opacity == 0.5)));
    // The box paints at x 100..120 after its transform.
    assert_eq!(
        hit_test(&f, Point::new(110.0, 10.0), t.viewport).map(|h| h.node),
        Some(moved)
    );
    assert_ne!(
        hit_test(&f, Point::new(10.0, 10.0), t.viewport).map(|h| h.node),
        Some(moved)
    );
}

#[test]
fn overflow_clips_and_records_scrollable_overflow() {
    let mut t = T::new();
    let r0 = t.root();
    let scroller = t.doc.block(r0, |s| {
        s.height = SizeValue::px(50.0);
        s.overflow_x = Overflow::Auto;
        s.overflow_y = Overflow::Auto;
    });
    t.doc.block(scroller, |s| s.height = SizeValue::px(300.0));
    let after = t.doc.block(r0, |s| s.height = SizeValue::px(10.0));
    let f = t.run(200.0, 200.0);
    let (_, sf) = f.find(scroller).unwrap();
    assert_eq!(sf.overflow, r(0.0, 0.0, 200.0, 300.0));
    // The scroll container's own box is what its parent sees.
    assert_eq!(rect(&f, after).y, 50.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let list = b.build();
    assert!(list
        .items
        .iter()
        .any(|i| matches!(i, Item::PushClipRect { rect } if rect.height == 50.0)));
    // Content clipped away isn't hit.
    let hit = hit_test(&f, Point::new(10.0, 55.0), t.viewport).unwrap();
    assert_eq!(hit.node, after);
}

#[test]
fn sticky_box_sticks_to_its_inset_within_its_container() {
    let mut t = T::new();
    let r0 = t.root();
    let container = t.doc.block(r0, |s| {
        s.height = SizeValue::px(100.0);
        s.margin[0] = LengthPercentageAuto::px(-20.0);
    });
    let sticky = t.doc.block(container, |s| {
        s.position = Position::Sticky;
        s.inset[0] = LengthPercentageAuto::px(5.0);
        s.height = SizeValue::px(10.0);
        s.background_color = snowlark_paint::Color::BLACK;
    });
    let f = t.run(200.0, 200.0);
    // Laid out at -20 (in flow), painted and hit at 5.
    assert_eq!(rect(&f, sticky).y, -20.0);
    let hit = hit_test(&f, Point::new(10.0, 8.0), t.viewport).unwrap();
    assert_eq!(hit.node, sticky);
}

#[test]
fn lines_move_below_floats_when_nothing_fits_beside_them() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| s.width = SizeValue::px(60.0));
    t.doc.block(p, |s| {
        s.float = Float::Left;
        s.width = SizeValue::px(40.0);
        s.height = SizeValue::px(15.0);
    });
    let text = t.doc.text(p, "aaaa");
    let f = t.run(200.0, 200.0);
    // 40px of text doesn't fit in the 20px beside the float: the line moves below it.
    assert_eq!(texts(&f, text)[0], r(0.0, 15.0, 40.0, 10.0));
}

#[test]
fn intrinsic_widths_drive_shrink_to_fit() {
    let mut t = T::new();
    let r0 = t.root();
    let fl = t.doc.block(r0, |s| s.float = Float::Left);
    t.doc.text(fl, "aaa bbbbb");
    let inner = t.doc.block(fl, |s| s.width = SizeValue::px(120.0));
    let mc = t.doc.block(r0, |s| {
        s.width = SizeValue::MinContent;
        s.clear = Clear::Both;
    });
    t.doc.text(mc, "aaa bbbbb");
    let fit = t.doc.block(r0, |s| s.width = SizeValue::FitContent(None));
    t.doc.text(fit, "aa bb");
    let f = t.run(100.0, 200.0);
    // The float's min-content is the 120px block's: it overflows the 100px available.
    assert_eq!(rect(&f, fl).width, 120.0);
    assert_eq!(rect(&f, inner).width, 120.0);
    assert_eq!(rect(&f, mc).width, 50.0);
    assert_eq!(rect(&f, fit).width, 50.0);
}

#[test]
fn legacy_alignment_aligns_over_constrained_block_descendants() {
    // HTML §15.3.3: inside `center` (`text-align: -webkit-center`), a block whose inline margins are
    // both non-auto and over-constrained is centered; an auto margin still wins.
    let mut t = T::new();
    let r0 = t.root();
    let outer = t.doc.block(r0, |s| {
        s.width = SizeValue::px(400.0);
        s.legacy_align = Some(LegacyAlign::Center);
    });
    let centered = t.doc.block(outer, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(10.0);
    });
    let auto_left = t.doc.block(outer, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(10.0);
        s.margin[3] = LengthPercentageAuto::Auto;
    });
    let right = t.doc.block(r0, |s| {
        s.width = SizeValue::px(400.0);
        s.legacy_align = Some(LegacyAlign::Right);
    });
    let pushed = t.doc.block(right, |s| {
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(10.0);
    });
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, centered).x, 150.0);
    assert_eq!(rect(&f, auto_left).x, 300.0);
    assert_eq!(rect(&f, pushed).x, 300.0);
}

#[test]
fn percentage_max_width_does_not_shrink_a_replaced_max_content_contribution() {
    // css-sizing-3 §5.2.1c/§5.2.2: `max-width: calc(100% - 8px)` makes an image's min-content
    // contribution zero, but its max-content contribution stays its size, so a float around it
    // shrinks to the image's width (Wikipedia's thumbnails); then the max-width applies.
    let mut t = T::new();
    let r0 = t.root();
    let float = t.doc.block(r0, |s| s.float = Float::Left);
    let img = t.doc.element(float, "img", |s| {
        s.display = Display::BLOCK;
        s.width = SizeValue::px(190.0);
        s.max_width = MaxSizeValue::LengthPercentage(LengthPercentage::linear(-8.0, Some(1.0)));
    });
    t.doc.replace(
        img,
        ReplacedContent::image(snowlark_paint::ImageId(1), 250.0, 227.0),
    );
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, float).width, 190.0);
    assert_eq!(rect(&f, img).width, 182.0);
}

#[test]
fn default_sizing_with_partial_natural_dimensions() {
    // CSS Images 3 §5.2 with a 300x150 default object size.
    let d = (300.0, 150.0);
    let ratio_only = NaturalSize {
        width: None,
        height: None,
        ratio: Some(2.0),
    };
    assert_eq!(ratio_only.concrete((None, None), d), (300.0, 150.0));
    assert_eq!(
        ratio_only.concrete((None, None), (100.0, 100.0)),
        (100.0, 50.0)
    );
    assert_eq!(ratio_only.concrete((Some(40.0), None), d), (40.0, 20.0));
    assert_eq!(ratio_only.concrete((None, Some(40.0)), d), (80.0, 40.0));
    let width_only = NaturalSize {
        width: Some(50.0),
        height: None,
        ratio: None,
    };
    assert_eq!(width_only.concrete((None, None), d), (50.0, 150.0));
    assert_eq!(width_only.concrete((None, Some(10.0)), d), (50.0, 10.0));
    let width_and_ratio = NaturalSize {
        ratio: Some(5.0),
        ..width_only
    };
    assert_eq!(width_and_ratio.concrete((None, None), d), (50.0, 10.0));
    assert_eq!(NaturalSize::default().concrete((None, None), d), d);
    assert_eq!(
        NaturalSize::fixed(30.0, 10.0).concrete((None, Some(20.0)), d),
        (60.0, 20.0)
    );
}

#[test]
fn background_size_of_images_without_natural_size() {
    use crate::paint::background_size;
    use snowlark_paint::{Rect, Size};
    let area = Rect::new(0.0, 0.0, 200.0, 100.0);
    let ratio_only = NaturalSize {
        width: None,
        height: None,
        ratio: Some(1.0),
    };
    // auto: contained in the positioning area; cover and contain use the ratio.
    assert_eq!(
        background_size(BackgroundSize::Auto, ratio_only, area),
        Size::new(100.0, 100.0)
    );
    assert_eq!(
        background_size(BackgroundSize::Cover, ratio_only, area),
        Size::new(200.0, 200.0)
    );
    assert_eq!(
        background_size(BackgroundSize::Contain, ratio_only, area),
        Size::new(100.0, 100.0)
    );
    // `background-size: 10px` (auto height) follows the ratio.
    assert_eq!(
        background_size(
            BackgroundSize::Explicit(Some(LengthPercentage::px(10.0)), None),
            ratio_only,
            area
        ),
        Size::new(10.0, 10.0)
    );
    // No ratio and no size (a gradient): the area.
    assert_eq!(
        background_size(BackgroundSize::Contain, NaturalSize::default(), area),
        Size::new(200.0, 100.0)
    );
    // A bitmap keeps its size.
    assert_eq!(
        background_size(BackgroundSize::Auto, NaturalSize::fixed(8.0, 6.0), area),
        Size::new(8.0, 6.0)
    );
}

fn control(kind: ControlKind, native: bool) -> ReplacedContent {
    ReplacedContent {
        control: Some(std::sync::Arc::new(Control {
            editing: None,
            kind,
            native,
            disabled: false,
            readonly: false,
            placeholder_color: Color::rgb(0x75, 0x75, 0x75),
        })),
        ..ReplacedContent::default()
    }
}

#[test]
fn form_control_natural_sizes() {
    // Ahem 10px: every character 10px wide, lines 10px tall.
    let mut t = T::new();
    let r0 = t.root();
    let add = |t: &mut T, kind: ControlKind, native: bool, f: &dyn Fn(&mut TestStyle)| {
        let el = t.doc.element(r0, "input", |s| {
            s.display = Display::BLOCK;
            f(s);
        });
        t.doc.replace(el, control(kind, native));
        el
    };
    let text = |size| ControlKind::TextField {
        text: "hi".into(),
        placeholder: false,
        size,
    };
    let field = add(&mut t, text(20), true, &|_| {});
    let small = add(&mut t, text(5), true, &|s| {
        s.padding_px(2.0);
        s.border(1.0, Color::BLACK);
    });
    let sized = add(&mut t, text(20), true, &|s| s.width = SizeValue::px(80.0));
    let area = add(
        &mut t,
        ControlKind::TextArea {
            text: "a\nb".into(),
            placeholder: false,
            rows: 3,
            cols: 4,
        },
        true,
        &|_| {},
    );
    let check = add(
        &mut t,
        ControlKind::Checkbox {
            checked: true,
            indeterminate: false,
        },
        true,
        &|_| {},
    );
    let bare = add(&mut t, ControlKind::Radio { checked: true }, false, &|_| {});
    let option = |label: &str, selected| SelectOption {
        label: label.into(),
        selected,
        disabled: false,
        group: false,
        indented: false,
    };
    let options: std::sync::Arc<[SelectOption]> = vec![
        option("ab", false),
        option("abcd", true),
        option("x", false),
    ]
    .into();
    let drop = add(
        &mut t,
        ControlKind::Select {
            options: options.clone(),
            list_rows: None,
        },
        true,
        &|_| {},
    );
    let list = add(
        &mut t,
        ControlKind::Select {
            options,
            list_rows: Some(2),
        },
        true,
        &|_| {},
    );
    let button = add(
        &mut t,
        ControlKind::Button {
            label: "abc".into(),
        },
        true,
        &|_| {},
    );
    let f = t.run(800.0, 600.0);
    let size = |n| {
        let r = rect(&f, n);
        (r.width, r.height)
    };
    // `size` characters; padding and border outside the content box; an author width wins.
    assert_eq!(size(field), (200.0, 10.0));
    assert_eq!(size(small), (56.0, 16.0));
    assert_eq!(size(sized), (80.0, 10.0));
    // `cols` characters plus a scroll bar, `rows` lines.
    assert_eq!(size(area), (57.0, 30.0));
    assert_eq!(size(check), (13.0, 13.0));
    assert_eq!(size(bare), (0.0, 0.0));
    // The widest option plus the arrow; a list box of 2 rows of 3 options gets a scroll bar.
    assert_eq!(size(drop), (64.0, 10.0));
    assert_eq!(size(list), (65.0, 20.0));
    assert_eq!(size(button), (30.0, 10.0));
}

/// A form control's font (for its natural size and its text) comes from the tree's font cache, as
/// text's does: resolved once for its style, and not again when the control is laid out again.
#[test]
fn form_control_fonts_are_resolved_once() {
    use std::sync::atomic::Ordering::Relaxed;
    let mut t = T::new();
    let r0 = t.root();
    let el = t.doc.element(r0, "input", |s| s.display = Display::BLOCK);
    t.doc.replace(
        el,
        control(
            ControlKind::TextField {
                text: "hi".into(),
                placeholder: false,
                size: 20,
            },
            true,
        ),
    );
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, el).width, 200.0);
    assert_eq!(t.layout.tree.fonts.resolved.load(Relaxed), 1);
    t.run(400.0, 600.0);
    assert_eq!(t.layout.tree.fonts.resolved.load(Relaxed), 1);
}

#[test]
fn form_controls_paint_and_align() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    t.doc.text(p, "ab");
    let field = t.doc.element(p, "input", |s| {
        s.display = Display::INLINE_BLOCK;
        s.padding_px(5.0);
    });
    t.doc.replace(
        field,
        control(
            ControlKind::TextField {
                text: "xyz".into(),
                placeholder: false,
                size: 4,
            },
            true,
        ),
    );
    let check = t.doc.element(r0, "input", |s| s.display = Display::BLOCK);
    t.doc.replace(
        check,
        control(
            ControlKind::Checkbox {
                checked: true,
                indeterminate: false,
            },
            true,
        ),
    );
    let f = t.run(400.0, 300.0);
    // The field's text baseline lines up with the text before it: its top is 5px (padding) above
    // the text's top.
    let r = rect(&f, field);
    assert_eq!((r.x, r.width, r.height), (20.0, 50.0, 20.0));
    let mut b = DisplayListBuilder::new();
    crate::paint_with(
        &f,
        &t.text,
        &PaintOptions::viewport(snowlark_paint::Size::new(400.0, 300.0)),
        &mut b,
    );
    let list = b.build();
    // The field's value is drawn as one text run at the content box's left, on the baseline 5px +
    // 8px (Ahem's ascent) below its top.
    let runs: Vec<Point> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Text(run) => Some(run.origin),
            _ => None,
        })
        .collect();
    assert!(
        runs.contains(&Point::new(r.x + 5.0, r.y + 13.0)),
        "{runs:?} {r:?}"
    );
    // The checked checkbox: an accent rounded square and a white check mark.
    let c = rect(&f, check);
    assert!(list.items.iter().any(|i| matches!(i,
        Item::FillRoundedRect { rect, color, .. }
            if *rect == c && *color == crate::controls::ACCENT)));
    assert!(list
        .items
        .iter()
        .any(|i| matches!(i, Item::FillRect { color, .. } if *color == Color::WHITE)));
}

#[test]
fn fieldset_legend_sits_in_the_border() {
    let mut t = T::new();
    let r0 = t.root();
    let fs = t.doc.element(r0, "fieldset", |s| {
        s.display = Display::BLOCK;
        s.border(2.0, Color::BLACK);
        s.padding_px(4.0);
    });
    let legend = t.doc.element(fs, "legend", |s| s.display = Display::BLOCK);
    t.doc.text(legend, "ab");
    let body = t.doc.block(fs, |_| {});
    t.doc.text(body, "c");
    let f = t.run(400.0, 300.0);
    // The legend (10px tall, shrink-to-fit) is centered on the 2px top border, which grows to the
    // legend's height: the content starts 10px + 4px padding down.
    let l = rect(&f, legend);
    assert_eq!((l.x, l.y, l.width, l.height), (6.0, 0.0, 20.0, 10.0));
    let c = rect(&f, body);
    assert_eq!((c.x, c.y), (6.0, 14.0));
    assert_eq!(rect(&f, fs).height, 10.0 + 4.0 + 10.0 + 4.0 + 2.0);
    // The top border is painted at the legend's middle, not behind it.
    let mut b = DisplayListBuilder::new();
    crate::paint_with(
        &f,
        &t.text,
        &PaintOptions::viewport(snowlark_paint::Size::new(400.0, 300.0)),
        &mut b,
    );
    let list = b.build();
    let borders: Vec<Rect> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Border(b) => Some(b.rect),
            _ => None,
        })
        .collect();
    assert_eq!(borders.len(), 3, "{borders:?}");
    assert!(borders.iter().all(|r| r.y == 4.0 && r.height == 26.0));
    let clips: Vec<Rect> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::PushClipRect { rect } => Some(*rect),
            _ => None,
        })
        .collect();
    assert!(clips.iter().any(|c| c.right() == 6.0), "{clips:?}");
    assert!(clips.iter().any(|c| c.x == 26.0), "{clips:?}");
}

#[test]
fn size_containment_sizes_as_if_empty() {
    // CSS Contain 2 §3.1/§3.2 (and `container-type`, CSS Contain 3 §2): the intrinsic sizes of a
    // size-contained box are those of the box with no content.
    let inline_size = SizeContainment {
        width: true,
        ..Default::default()
    };
    let mut t = T::new();
    let r0 = t.root();
    let grid = |t: &mut T, parent: NodeId| {
        t.doc.block(parent, |s| {
            s.display = Display::GRID;
            s.width = SizeValue::px(600.0);
            s.grid_template_columns = TrackList {
                items: vec![
                    TrackListItem::Track(TrackSize::Breadth(TrackBreadth::LengthPercentage(
                        LengthPercentage::px(236.0),
                    ))),
                    // `1fr` is `minmax(auto, 1fr)`: the column grows to its items' minimums.
                    TrackListItem::Track(TrackSize::Breadth(TrackBreadth::Fr(1.0))),
                ],
                line_names: Vec::new(),
            };
        })
    };
    let wide = |t: &mut T, parent: NodeId| {
        t.doc.block(parent, |s| {
            s.width = SizeValue::px(900.0);
            s.height = SizeValue::px(10.0);
        });
    };
    let mut items = Vec::new();
    for (contain, display) in [
        (SizeContainment::default(), Display::BLOCK),
        (inline_size, Display::BLOCK),
        (inline_size, Display::FLEX),
    ] {
        let g = grid(&mut t, r0);
        t.doc.block(g, |_| {});
        let item = t.doc.block(g, |s| {
            s.display = display;
            s.size_containment = contain;
        });
        wide(&mut t, item);
        items.push(item);
    }
    let shrink = t.doc.block(r0, |s| {
        s.display = Display::INLINE_BLOCK;
        s.size_containment = inline_size;
    });
    wide(&mut t, shrink);
    let size = SizeContainment {
        width: true,
        height: true,
        ..Default::default()
    };
    let both = t.doc.block(r0, |s| s.size_containment = size);
    wide(&mut t, both);
    // `contain-intrinsic-size` (CSS Sizing 4 §5.1) stands in for the content.
    let explicit = t.doc.block(r0, |s| {
        s.display = Display::INLINE_BLOCK;
        s.size_containment = SizeContainment {
            intrinsic_width: Some(40.0),
            intrinsic_height: Some(30.0),
            ..size
        };
    });
    wide(&mut t, explicit);
    // A size-contained grid keeps its tracks (contain-size-grid-003).
    let tracks = t.doc.block(r0, |s| {
        s.display = Display::INLINE_GRID;
        s.size_containment = size;
        s.grid_template_columns = TrackList {
            items: vec![TrackListItem::Track(TrackSize::Breadth(
                TrackBreadth::LengthPercentage(LengthPercentage::px(100.0)),
            ))],
            line_names: Vec::new(),
        };
        s.grid_template_rows = TrackList {
            items: vec![TrackListItem::Track(TrackSize::Breadth(
                TrackBreadth::LengthPercentage(LengthPercentage::px(50.0)),
            ))],
            line_names: Vec::new(),
        };
    });
    wide(&mut t, tracks);
    let img = t.doc.element(r0, "img", |s| {
        s.display = Display::BLOCK;
        s.size_containment = inline_size;
    });
    t.doc.replace(
        img,
        ReplacedContent::image(snowlark_paint::ImageId(1), 200.0, 100.0),
    );
    let f = t.run(1000.0, 1000.0);
    assert_eq!(rect(&f, items[0]).width, 900.0);
    assert_eq!(rect(&f, items[1]).width, 364.0);
    assert_eq!(rect(&f, items[2]).width, 364.0);
    assert_eq!(rect(&f, shrink).width, 0.0);
    // Block-size containment: the auto height is that of an empty box.
    assert_eq!(rect(&f, both).height, 0.0);
    let re = rect(&f, explicit);
    assert_eq!((re.width, re.height), (40.0, 30.0));
    let rt = rect(&f, tracks);
    assert_eq!((rt.width, rt.height), (100.0, 50.0));
    // A replaced box: natural width zero and no natural aspect ratio.
    let ri = rect(&f, img);
    assert_eq!((ri.width, ri.height), (0.0, 100.0));
}

#[test]
fn inline_level_widget_and_container_baselines() {
    // A 20 px image puts the line's baseline 20 px down; each box below is placed against it.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    let img = t.doc.element(p, "img", |_| {});
    t.doc.replace(
        img,
        ReplacedContent::image(snowlark_paint::ImageId(1), 20.0, 20.0),
    );
    // A native checkbox: its border-box bottom on the baseline, the bottom margin below it.
    let check = t.doc.element(p, "input", |s| {
        s.display = Display::INLINE_BLOCK;
        s.margin_px(3.0);
    });
    t.doc.replace(
        check,
        control(
            ControlKind::Checkbox {
                checked: false,
                indeterminate: false,
            },
            true,
        ),
    );
    // The same without the native look is an inline-block without line boxes: its bottom margin
    // edge on the baseline (CSS 2 §10.8.1).
    let bare = t.doc.element(p, "input", |s| {
        s.display = Display::INLINE_BLOCK;
        s.margin_px(3.0);
        s.width = SizeValue::px(13.0);
        s.height = SizeValue::px(13.0);
    });
    t.doc.replace(
        bare,
        control(
            ControlKind::Checkbox {
                checked: false,
                indeterminate: false,
            },
            false,
        ),
    );
    // An inline flex container whose only item has no baseline: the baseline is synthesized from
    // the item's border box (css-flexbox-1 §8.5), 5 px down; the same for a grid.
    let mut containers = Vec::new();
    for display in [Display::INLINE_FLEX, Display::INLINE_GRID] {
        let c = t.doc.block(p, |s| {
            s.display = display;
            s.height = SizeValue::px(12.0);
        });
        t.doc.block(c, |s| {
            s.width = SizeValue::px(4.0);
            s.height = SizeValue::px(5.0);
        });
        containers.push(c);
    }
    // A wrapping flex container: an item taking part in baseline alignment on its second line does
    // not give the container its baseline; the first line's first item does.
    let wrap = t.doc.block(p, |s| {
        s.display = Display::INLINE_FLEX;
        s.flex_wrap = FlexWrap::Wrap;
        s.width = SizeValue::px(10.0);
    });
    for (h, align) in [(5.0, Align::Auto), (7.0, Align::Baseline)] {
        t.doc.block(wrap, |s| {
            s.width = SizeValue::px(10.0);
            s.height = SizeValue::px(h);
            s.align_self = align;
        });
    }
    let f = t.run(400.0, 200.0);
    assert_eq!(rect(&f, wrap).y, 20.0 - 5.0);
    assert_eq!(rect(&f, img).y, 0.0);
    assert_eq!(rect(&f, check).y, 20.0 - 13.0);
    assert_eq!(rect(&f, bare).y, 20.0 - 3.0 - 13.0);
    for c in containers {
        assert_eq!(rect(&f, c).y, 20.0 - 5.0);
    }
}

#[test]
fn body_overflow_propagated_to_the_viewport_does_not_make_it_a_formatting_context_root() {
    // css-grid/layout-algorithm/grid-as-flex-item-should-not-shrink-to-fit-001.html has
    // `body { overflow: hidden }`: that overflow is the viewport's (CSS Overflow 3 §3.3) and the
    // body's own used overflow is `visible`, so its first child's top margin collapses with its own.
    let mut t = T::new();
    let r0 = t.root();
    let body = t.doc.element(r0, "body", |s| {
        s.display = Display::BLOCK;
        s.overflow_x = Overflow::Hidden;
        s.overflow_y = Overflow::Hidden;
        s.margin = [LengthPercentageAuto::LengthPercentage(LengthPercentage::px(8.0)); 4];
    });
    let p = t.doc.block(body, |s| {
        s.margin[0] = LengthPercentageAuto::LengthPercentage(LengthPercentage::px(16.0));
        s.height = SizeValue::px(10.0);
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, p).y, 16.0);
    // With a non-visible root overflow, the body's overflow is its own again.
    t.doc.restyle(r0, |s| s.overflow_y = Overflow::Hidden);
    let changes = TreeChanges {
        restyled: vec![ElementChange::restyled(r0, BoxDamage::Relayout, false)],
        ..Default::default()
    };
    t.layout
        .update(&t.doc.dom, t.doc.document, &t.doc.styles, &changes);
    let f = t.relayout();
    assert_eq!(rect(&f, p).y, 8.0 + 16.0);
}

/// HTML §15.5.3 button layout: a `button` behaves as `flow-root` for a block-level or internal
/// `display` and as `inline-block` for an inline one, and its `auto` inline size is the fit-content
/// size (html/rendering/widgets/button-layout `shrink-wrap`, `display-other`).
#[test]
fn buttons_shrink_to_fit_as_flow_root_or_inline_block() {
    let mut t = T::new();
    let r0 = t.root();
    let make = |t: &mut T, parent: NodeId, tag: &str, display: Display| {
        let b = t.doc.element(parent, tag, |s| {
            s.display = display;
            s.padding_px(5.0);
        });
        t.doc.block(b, |s| {
            s.width = SizeValue::px(30.0);
            s.height = SizeValue::px(10.0);
        });
        b
    };
    let block = make(&mut t, r0, "button", Display::BLOCK);
    let list_item = make(&mut t, r0, "button", Display::LIST_ITEM);
    let row = make(&mut t, r0, "button", Display::TABLE_ROW);
    let table = make(&mut t, r0, "button", Display::TABLE);
    let line = t.doc.block(r0, |_| {});
    let inline = make(&mut t, line, "button", Display::INLINE);
    let div = make(&mut t, r0, "div", Display::BLOCK);
    let f = t.run(800.0, 600.0);
    let size = |n| {
        let r = rect(&f, n);
        (r.width, r.height)
    };
    for (name, b) in [
        ("block", block),
        ("list-item", list_item),
        ("table-row", row),
        ("table", table),
        ("inline", inline),
    ] {
        assert_eq!(size(b), (40.0, 20.0), "{name}");
    }
    // The inline one is atomic: its padding makes the line at least as tall as its margin box
    // (the strut's descent goes under it, its baseline being its bottom margin edge).
    assert!(rect(&f, line).height >= 20.0, "{}", rect(&f, line).height);
    // Any other element fills the line.
    assert_eq!(size(div), (800.0, 20.0));
}

/// HTML §15.5.3 button layout: a button's content is centered in the block axis when it fits,
/// with `align-content` at its computed `normal` (html/rendering/non-replaced-elements/
/// form-controls `resets.html` expects `normal`); an author's value applies, and content taller
/// than the button stays at its start. Other block containers keep `normal` as `start`.
#[test]
fn a_buttons_content_is_centered_in_the_block_axis() {
    let mut t = T::new();
    let r0 = t.root();
    let make = |t: &mut T, tag: &str, height: f32, align: Align| {
        let b = t.doc.element(r0, tag, |s| {
            s.display = Display::BLOCK;
            s.height = SizeValue::px(50.0);
            s.align_content = align;
        });
        let c = t.doc.block(b, |s| {
            s.width = SizeValue::px(30.0);
            s.height = SizeValue::px(height);
        });
        (b, c)
    };
    let centered = make(&mut t, "button", 10.0, Align::Normal);
    let start = make(&mut t, "button", 10.0, Align::Start);
    let tall = make(&mut t, "button", 80.0, Align::Normal);
    let div = make(&mut t, "div", 10.0, Align::Normal);
    let f = t.run(800.0, 600.0);
    let top = |(b, c): (NodeId, NodeId)| rect(&f, c).y - rect(&f, b).y;
    assert_eq!(top(centered), 20.0);
    assert_eq!(top(start), 0.0);
    assert_eq!(top(tall), 0.0);
    assert_eq!(top(div), 0.0);
}

#[test]
fn a_percentage_width_resolves_against_zero_in_a_min_content_contribution() {
    // css-sizing-3 §5.2.1c and §5.2.2: in the min-content contribution of a replaced box (a text
    // field among them) a percentage `width` resolves against zero, so `100%` contributes its
    // border and padding (then `min-width`), and `calc(50% + 50px)` 50px (the spec's example). An
    // image's percentage contributes nothing, as engines do. Checked in a grid's min-content track
    // and in a `width: min-content` block.
    let field = || {
        control(
            ControlKind::TextField {
                text: "".into(),
                placeholder: false,
                size: 20,
            },
            true,
        )
    };
    let calc = SizeValue::LengthPercentage(LengthPercentage::linear(50.0, Some(0.5)));
    let case = |grid: bool, content: ReplacedContent, f: &dyn Fn(&mut TestStyle)| {
        let mut t = T::new();
        let r0 = t.root();
        let container = t.doc.block(r0, |s| {
            if grid {
                s.display = Display::GRID;
                s.width = SizeValue::px(500.0);
                s.grid_template_columns = crate::tests_grid::tracks(vec![TrackListItem::Track(
                    TrackSize::Breadth(TrackBreadth::MinContent),
                )]);
            } else {
                s.width = SizeValue::MinContent;
            }
        });
        let item = t.doc.element(container, "input", |s| {
            s.display = Display::BLOCK;
            f(s);
        });
        t.doc.replace(item, content);
        let f = t.run(800.0, 600.0);
        rect(&f, item).width
    };
    let full_width = |s: &mut TestStyle| {
        s.width = SizeValue::percent(100.0);
        s.min_width = SizeValue::px(2.0);
    };
    let calc_width = |s: &mut TestStyle| s.width = calc;
    let image = || ReplacedContent::image(snowlark_paint::ImageId(1), 100.0, 50.0);
    let button = || {
        control(
            ControlKind::Button {
                label: "Send".into(),
            },
            true,
        )
    };
    for grid in [true, false] {
        assert_eq!(case(grid, field(), &full_width), 2.0, "grid {grid}");
        // a 50px track or container, of which the field takes 50% + 50px
        assert_eq!(case(grid, field(), &calc_width), 75.0, "grid {grid}");
        // a 0px one: the image is 50px
        assert_eq!(case(grid, image(), &calc_width), 50.0, "grid {grid}");
        // A button-like input is not compressible: it contributes its natural width.
        let natural = case(grid, button(), &|_| {});
        assert!(natural > 2.0);
        assert_eq!(case(grid, button(), &full_width), natural, "grid {grid}");
    }
    // A flex item's content size suggestion (css-flexbox-1 §4.5) keeps an image's natural width:
    // `width: 100%; flex: 0 0 10px` in a 100px container stays 100px wide (WPT
    // css-flexbox/flex-aspect-ratio-img-row-017.html).
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.width = SizeValue::px(100.0);
    });
    let img = t.doc.element(flex, "img", |s| {
        s.width = SizeValue::percent(100.0);
        s.flex_shrink = 0.0;
        s.flex_basis = FlexBasis::LengthPercentage(LengthPercentage::px(10.0));
    });
    t.doc.replace(img, image());
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, img).width, 100.0);
}
