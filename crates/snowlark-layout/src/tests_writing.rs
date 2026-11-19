// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Writing Modes 4: block layout in vertical writing modes and across writing-mode boundaries
//! (the Ahem setup of [`crate::tests`]: 10px glyphs, 10px lines).

use snowlark_paint::{Rect, Size};

use crate::style::*;
use crate::tests::*;

fn px(v: f32) -> SizeValue {
    SizeValue::px(v)
}

/// A document whose root has writing mode `wm`.
fn doc(wm: WritingMode) -> T {
    let mut t = T::new();
    let root = t.root();
    t.doc.restyle(root, |s| s.writing_mode = wm);
    t
}

#[test]
fn vertical_rl_blocks_stack_right_to_left() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let a = t.doc.block(root, |s| s.width = px(100.0));
    let b = t.doc.block(root, |s| s.width = px(50.0));
    let f = t.run(800.0, 600.0);
    // The root's block size is its content's (150); the ICB's block-start is its right edge.
    assert_eq!(rect(&f, root), Rect::new(650.0, 0.0, 150.0, 600.0));
    assert_eq!(rect(&f, a), Rect::new(700.0, 0.0, 100.0, 600.0));
    assert_eq!(rect(&f, b), Rect::new(650.0, 0.0, 50.0, 600.0));
}

#[test]
fn vertical_lr_blocks_stack_left_to_right() {
    let mut t = doc(WritingMode::VerticalLr);
    let root = t.root();
    let a = t.doc.block(root, |s| s.width = px(100.0));
    let b = t.doc.block(root, |s| {
        s.width = px(50.0);
        s.height = px(200.0);
        // line-left is the top: `margin-top` is the inline-start margin.
        s.margin[0] = LengthPercentageAuto::px(10.0);
    });
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, a), Rect::new(0.0, 0.0, 100.0, 600.0));
    assert_eq!(rect(&f, b), Rect::new(100.0, 10.0, 50.0, 200.0));
}

#[test]
fn vertical_margins_collapse_in_the_block_axis() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    // Block-axis margins are left and right: adjoining ones collapse.
    let a = t.doc.block(root, |s| {
        s.width = px(20.0);
        s.margin[3] = LengthPercentageAuto::px(30.0);
    });
    let b = t.doc.block(root, |s| {
        s.width = px(20.0);
        s.margin[1] = LengthPercentageAuto::px(10.0);
    });
    let f = t.run(800.0, 600.0);
    let ra = rect(&f, a);
    let rb = rect(&f, b);
    assert_eq!(ra.x - (rb.x + rb.width), 30.0);
}

#[test]
fn auto_inline_margins_center_vertically() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let a = t.doc.block(root, |s| {
        s.width = px(20.0);
        s.height = px(200.0);
        s.margin[0] = LengthPercentageAuto::Auto;
        s.margin[2] = LengthPercentageAuto::Auto;
    });
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, a).y, 200.0);
}

#[test]
fn orthogonal_child_in_horizontal_flow() {
    let mut t = T::new();
    let root = t.root();
    let v = t.doc.block(root, |s| {
        s.writing_mode = WritingMode::VerticalRl;
        s.width = px(100.0);
        s.height = px(200.0);
    });
    let g = t.doc.block(v, |s| s.width = px(30.0));
    let after = t.doc.block(root, |s| s.height = px(10.0));
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, v), Rect::new(0.0, 0.0, 100.0, 200.0));
    // The grandchild starts at the vertical box's block-start: its right edge.
    assert_eq!(rect(&f, g), Rect::new(70.0, 0.0, 30.0, 200.0));
    assert_eq!(rect(&f, after).y, 200.0);
}

#[test]
fn orthogonal_child_auto_block_size_fits_its_content() {
    let mut t = T::new();
    let root = t.root();
    let v = t.doc.block(root, |s| {
        s.writing_mode = WritingMode::VerticalLr;
        s.height = px(100.0);
    });
    t.doc.block(v, |s| s.width = px(30.0));
    t.doc.block(v, |s| s.width = px(20.0));
    let f = t.run(800.0, 600.0);
    // §7.3: its block size (physical width) fits its content.
    assert_eq!(rect(&f, v), Rect::new(0.0, 0.0, 50.0, 100.0));
}

#[test]
fn orthogonal_child_auto_inline_size_uses_the_icb_fallback() {
    let mut t = T::new();
    let root = t.root();
    let v = t.doc.block(root, |s| {
        s.writing_mode = WritingMode::VerticalRl;
        s.width = px(40.0);
    });
    let f = t.run(800.0, 600.0);
    // No definite block size in the parent: shrink-to-fit against the ICB's height; with no
    // content that is its min-content size.
    let r = rect(&f, v);
    assert_eq!((r.width, r.height), (40.0, 0.0));
}

#[test]
fn parallel_flows_reverse_the_block_axis() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let lr = t.doc.block(root, |s| {
        s.writing_mode = WritingMode::VerticalLr;
        s.width = px(100.0);
    });
    let a = t.doc.block(lr, |s| s.width = px(30.0));
    let f = t.run(800.0, 600.0);
    let r = rect(&f, lr);
    assert_eq!(r, Rect::new(700.0, 0.0, 100.0, 600.0));
    // vertical-lr content starts at the left.
    assert_eq!(rect(&f, a), Rect::new(700.0, 0.0, 30.0, 600.0));
}

#[test]
fn horizontal_layout_reads_physical_values() {
    // The view is the identity in horizontal-tb.
    let mut t = T::new();
    let root = t.root();
    let a = t.doc.block(root, |s| {
        s.width = px(100.0);
        s.margin[3] = LengthPercentageAuto::px(5.0);
    });
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, a), Rect::new(5.0, 0.0, 100.0, 0.0));
}

/// The rects (physical) of the text fragments under `f`, in tree order.
fn text_rects(f: &F) -> Vec<Rect> {
    let mut out = Vec::new();
    f.walk(&mut |fr, at| {
        if fr.as_text().is_some() {
            out.push(Rect::new(at.x, at.y, fr.size.width, fr.size.height));
        }
    });
    out
}

#[test]
fn sideways_lr_inline_runs_upward_from_the_bottom() {
    let mut t = doc(WritingMode::SidewaysLr);
    let root = t.root();
    let a = t.doc.block(root, |s| s.width = px(100.0));
    let b = t.doc.block(root, |s| {
        s.width = px(50.0);
        s.height = px(200.0);
        // line-left is the bottom: `margin-bottom` is the inline-start margin.
        s.margin[2] = LengthPercentageAuto::px(10.0);
    });
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, a), Rect::new(0.0, 0.0, 100.0, 600.0));
    assert_eq!(
        rect(&f, b),
        Rect::new(100.0, 600.0 - 10.0 - 200.0, 50.0, 200.0)
    );
}

#[test]
fn sideways_lr_text_starts_at_the_bottom() {
    let mut t = doc(WritingMode::SidewaysLr);
    let root = t.root();
    let a = t.doc.block(root, |s| s.height = px(100.0));
    t.doc.text(a, "XXX");
    let f = t.run(800.0, 600.0);
    let r = text_rects(&f);
    assert_eq!(r, vec![Rect::new(0.0, 570.0, 10.0, 30.0)]);
    let (_, fr) = f.find(a).unwrap();
    let mut flip = None;
    fr.walk(&mut |x, _| {
        if let Some(t) = x.as_text() {
            flip = Some(t.flip);
        }
    });
    // A quarter turn counterclockwise, not a mirror image.
    assert_eq!(flip, Some(Flip::SIDEWAYS_LR));
}

#[test]
fn vertical_lines_stack_in_the_block_direction() {
    for (wm, first, second) in [
        (WritingMode::VerticalRl, 90.0, 80.0),
        (WritingMode::VerticalLr, 0.0, 10.0),
    ] {
        let mut t = doc(wm);
        let root = t.root();
        let a = t.doc.block(root, |s| {
            s.width = px(100.0);
            s.height = px(30.0);
        });
        t.doc.text(a, "XXX XXX");
        let f = t.run(800.0, 600.0);
        let ra = rect(&f, a);
        let r = text_rects(&f);
        assert_eq!(r.len(), 2, "{wm:?}");
        assert_eq!(r[0].x - ra.x, first, "{wm:?}");
        assert_eq!(r[1].x - ra.x, second, "{wm:?}");
        assert_eq!((r[0].width, r[0].height), (10.0, 30.0), "{wm:?}");
    }
}

#[test]
fn vertical_text_never_mirrors() {
    for wm in [
        WritingMode::VerticalRl,
        WritingMode::VerticalLr,
        WritingMode::SidewaysRl,
        WritingMode::SidewaysLr,
    ] {
        let mut t = doc(wm);
        let root = t.root();
        let a = t.doc.block(root, |s| s.height = px(100.0));
        t.doc.text(a, "X");
        let f = t.run(800.0, 600.0);
        f.walk(&mut |x, _| {
            if let Some(t) = x.as_text() {
                assert!(!t.flip.mirrors(), "{wm:?}");
                assert!(t.flip.swaps(), "{wm:?}");
            }
        });
    }
}

#[test]
fn central_baseline_centers_fonts_of_different_sizes() {
    // §4.2: the em boxes of a 10px and a 30px font share their middle in vertical-rl.
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let a = t.doc.block(root, |s| s.height = px(200.0));
    t.doc.text(a, "X");
    let big = t.doc.span(a, |s| {
        s.font_size = 30.0;
        s.line_height = LineHeight::Number(1.0);
    });
    t.doc.text(big, "X");
    let f = t.run(800.0, 600.0);
    let r = text_rects(&f);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].x + r[0].width / 2.0, r[1].x + r[1].width / 2.0);
    // The alphabetic baseline (sideways text) puts the 8px and 24px ascents on one line instead.
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    t.doc
        .restyle(root, |s| s.text_orientation = TextOrientation::Sideways);
    let a = t.doc.block(root, |s| s.height = px(200.0));
    t.doc.text(a, "X");
    let big = t.doc.span(a, |s| {
        s.font_size = 30.0;
        s.line_height = LineHeight::Number(1.0);
    });
    t.doc.text(big, "X");
    let f = t.run(800.0, 600.0);
    let r = text_rects(&f);
    assert_ne!(r[0].x + r[0].width / 2.0, r[1].x + r[1].width / 2.0);
}

#[test]
fn vertical_lr_lines_put_line_over_on_the_right() {
    // `vertical-align: top` is line-over: the right side in vertical-lr too (§6.3).
    let mut t = doc(WritingMode::VerticalLr);
    let root = t.root();
    let a = t.doc.block(root, |s| {
        s.height = px(200.0);
        s.line_height = LineHeight::Number(3.0);
    });
    let s1 = t.doc.span(a, |s| {
        s.vertical_align = VerticalAlign::Top;
        s.line_height = LineHeight::Number(1.0);
    });
    t.doc.text(s1, "X");
    let f = t.run(800.0, 600.0);
    let r = text_rects(&f);
    let line = rect(&f, a);
    assert_eq!(line.width, 30.0);
    assert_eq!(r[0].x + r[0].width, line.x + line.width);
}

#[test]
fn inline_box_block_padding_keeps_its_physical_side_in_vertical_lr() {
    let mut t = doc(WritingMode::VerticalLr);
    let root = t.root();
    let a = t.doc.block(root, |s| s.height = px(200.0));
    let sp = t.doc.span(a, |s| {
        // padding-left is the block-start padding here.
        s.padding[3] = LengthPercentage::px(7.0);
    });
    t.doc.text(sp, "X");
    let f = t.run(800.0, 600.0);
    let b = rect(&f, sp);
    let tx = text_rects(&f)[0];
    assert_eq!(tx.x - b.x, 7.0);
    assert_eq!(b.right(), tx.right());
}

#[test]
fn orthogonal_flex_item_is_sized_in_the_containers_frame() {
    let mut t = T::new();
    let root = t.root();
    let c = t.doc.block(root, |s| {
        s.display = Display::FLEX;
        s.width = px(300.0);
        s.height = px(100.0);
    });
    let item = t.doc.block(c, |s| {
        s.writing_mode = WritingMode::VerticalRl;
        s.width = px(40.0);
    });
    let inner = t.doc.block(item, |s| s.width = px(10.0));
    let after = t.doc.block(c, |s| s.width = px(20.0));
    let f = t.run(800.0, 600.0);
    // Stretched in the cross axis (physical height); its fixed width is its block size.
    assert_eq!(rect(&f, item), Rect::new(0.0, 0.0, 40.0, 100.0));
    assert_eq!(rect(&f, inner), Rect::new(30.0, 0.0, 10.0, 100.0));
    assert_eq!(rect(&f, after).x, 40.0);
}

#[test]
fn vertical_flex_container_main_axis_is_vertical() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let c = t.doc.block(root, |s| {
        s.display = Display::FLEX;
        s.width = px(100.0);
        s.height = px(300.0);
    });
    let a = t.doc.block(c, |s| s.height = px(50.0));
    let b = t.doc.block(c, |s| s.flex_grow = 1.0);
    let f = t.run(800.0, 600.0);
    let rc = rect(&f, c);
    assert_eq!(rect(&f, a), Rect::new(rc.x, 0.0, 100.0, 50.0));
    assert_eq!(rect(&f, b), Rect::new(rc.x, 50.0, 100.0, 250.0));
}

#[test]
fn orthogonal_grid_item_fills_its_area() {
    let mut t = T::new();
    let root = t.root();
    let c = t.doc.block(root, |s| {
        s.display = Display::GRID;
        s.width = px(200.0);
        s.height = px(100.0);
    });
    let item = t.doc.block(c, |s| s.writing_mode = WritingMode::VerticalLr);
    let f = t.run(800.0, 600.0);
    let r = rect(&f, item);
    assert_eq!((r.width, r.height), (200.0, 100.0));
}

#[test]
fn abspos_insets_are_physical_in_vertical_rl() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let cb = t.doc.block(root, |s| {
        s.position = Position::Relative;
        s.width = px(200.0);
        s.height = px(100.0);
    });
    let a = t.doc.block(cb, |s| {
        s.position = Position::Absolute;
        s.inset[0] = LengthPercentageAuto::px(10.0); // top
        s.inset[3] = LengthPercentageAuto::px(20.0); // left
        s.width = px(30.0);
        s.height = px(40.0);
    });
    let f = t.run(800.0, 600.0);
    let rc = rect(&f, cb);
    assert_eq!(rect(&f, a), Rect::new(rc.x + 20.0, 10.0, 30.0, 40.0));
}

#[test]
fn abspos_static_position_is_block_start_in_vertical_rl() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let cb = t.doc.block(root, |s| {
        s.position = Position::Relative;
        s.width = px(200.0);
        s.height = px(100.0);
    });
    let a = t.doc.block(cb, |s| {
        s.position = Position::Absolute;
        s.width = px(30.0);
        s.height = px(40.0);
    });
    let f = t.run(800.0, 600.0);
    let rc = rect(&f, cb);
    // The right edge of the containing block is its block-start edge.
    assert_eq!(rect(&f, a), Rect::new(rc.x + 170.0, 0.0, 30.0, 40.0));
}

#[test]
fn abspos_of_another_writing_mode_is_placed_in_its_containing_blocks_frame() {
    let mut t = T::new();
    let root = t.root();
    let cb = t.doc.block(root, |s| {
        s.position = Position::Relative;
        s.width = px(200.0);
        s.height = px(100.0);
    });
    let a = t.doc.block(cb, |s| {
        s.position = Position::Absolute;
        s.writing_mode = WritingMode::VerticalRl;
        s.inset[1] = LengthPercentageAuto::px(5.0); // right
        s.inset[2] = LengthPercentageAuto::px(6.0); // bottom
        s.width = px(30.0);
        s.height = px(40.0);
    });
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, a), Rect::new(165.0, 54.0, 30.0, 40.0));
}

#[test]
fn orthogonal_auto_inline_size_shrinks_to_fit_a_definite_block_size() {
    // §7.3.2: fit-content, not fill, even when the containing block's block size is definite.
    let mut t = T::new();
    let root = t.root();
    let cb = t.doc.block(root, |s| s.height = px(300.0));
    let v = t.doc.block(cb, |s| {
        s.writing_mode = WritingMode::VerticalRl;
        s.width = px(50.0);
    });
    t.doc.text(v, "XXX");
    let f = t.run(800.0, 600.0);
    let r = rect(&f, v);
    assert_eq!((r.width, r.height), (50.0, 30.0));
}

#[test]
fn orthogonal_fallback_takes_the_scrollport() {
    // §7.3.1: the nearest scrollport's fixed size bounds the available inline space.
    let mut t = T::new();
    let root = t.root();
    let sc = t.doc.block(root, |s| {
        s.overflow_y = Overflow::Auto;
        s.overflow_x = Overflow::Auto;
        s.height = px(40.0);
    });
    let inner = t.doc.block(sc, |_| {});
    let v = t
        .doc
        .block(inner, |s| s.writing_mode = WritingMode::VerticalRl);
    t.doc.text(v, "XXX XXX XXX");
    let f = t.run(800.0, 600.0);
    let r = rect(&f, v);
    // Lines break at 40px; the auto inline size fills it (max-content is wider).
    assert_eq!((r.width, r.height), (30.0, 40.0));
}

#[test]
fn body_writing_mode_is_the_principal_writing_mode() {
    let mut t = T::new();
    let root = t.root();
    let body = t.doc.element(root, "body", |s| {
        s.display = Display::BLOCK;
        s.writing_mode = WritingMode::VerticalRl;
        s.width = px(100.0);
    });
    let f = t.run(800.0, 600.0);
    // The root is used vertical-rl: it sits at the right, the body not orthogonal to it.
    assert_eq!(rect(&f, body), Rect::new(700.0, 0.0, 100.0, 600.0));
    assert_eq!(
        t.layout.principal_writing_mode().map(|p| p.0),
        Some(WritingMode::VerticalRl)
    );
}

#[test]
fn a_viewport_change_reaches_orthogonal_flows() {
    let mut t = T::new();
    let root = t.root();
    let a = t.doc.block(root, |s| s.width = px(10.0));
    let v = t.doc.block(a, |s| s.writing_mode = WritingMode::VerticalRl);
    t.doc.text(v, "XXX XXX");
    let f = t.run(800.0, 40.0);
    assert_eq!(rect(&f, v).width, 20.0);
    t.viewport = Size::new(800.0, 100.0);
    let f = t.relayout();
    assert_eq!(rect(&f, v).width, 10.0);
}

#[test]
fn replaced_natural_size_stays_physical_in_a_vertical_frame() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let img = t.doc.block(root, |_| {});
    t.doc.replace(
        img,
        ReplacedContent::image(snowlark_paint::ImageId(1), 40.0, 20.0),
    );
    let f = t.run(800.0, 600.0);
    let r = rect(&f, img);
    assert_eq!((r.width, r.height), (40.0, 20.0));
}

#[test]
fn an_atomic_inline_without_a_baseline_centers_on_the_central_baseline() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let a = t.doc.block(root, |s| s.height = px(200.0));
    t.doc.text(a, "X");
    let b = t.doc.span(a, |s| {
        s.display = Display::INLINE_BLOCK;
        s.width = px(40.0);
        s.height = px(10.0);
        s.overflow_x = Overflow::Hidden;
        s.overflow_y = Overflow::Hidden;
    });
    let f = t.run(800.0, 600.0);
    let tx = text_rects(&f)[0];
    let rb = rect(&f, b);
    assert_eq!(tx.x + tx.width / 2.0, rb.x + rb.width / 2.0);
}

#[test]
fn justify_self_aligns_a_block_level_box() {
    let mut t = T::new();
    let root = t.root();
    let cb = t.doc.block(root, |s| s.width = px(100.0));
    let a = t.doc.block(cb, |s| {
        s.justify_self = Align::End;
        s.width = px(30.0);
        s.height = px(10.0);
    });
    let b = t.doc.block(cb, |s| {
        s.justify_self = Align::Center;
        s.display = Display::BLOCK;
    });
    t.doc.text(b, "XX");
    let f = t.run(800.0, 600.0);
    assert_eq!(rect(&f, a).x, 70.0);
    // An aligned auto width fits its content.
    assert_eq!((rect(&f, b).x, rect(&f, b).width), (40.0, 20.0));
}

#[test]
fn bidi_override_on_an_inline_box_reverses_its_text() {
    let mut t = T::new();
    let root = t.root();
    let p = t.doc.block(root, |_| {});
    t.doc.text(p, "a");
    let s = t.doc.span(p, |s| {
        s.direction = Direction::Rtl;
        s.unicode_bidi = UnicodeBidi::BidiOverride;
    });
    t.doc.text(s, "bc");
    let f = t.run(800.0, 600.0);
    let mut level = None;
    f.walk(&mut |x, _| {
        if let Some(t) = x.as_text() {
            if t.clusters.len() == 2 {
                level = Some(t.shaped.runs[t.run as usize].bidi_level);
            }
        }
    });
    assert_eq!(level, Some(1));
}

#[test]
fn text_combine_upright_is_one_em_on_the_line() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let a = t.doc.block(root, |s| s.height = px(200.0));
    t.doc.text(a, "X");
    let s = t.doc.span(a, |s| s.text_combine_upright = true);
    t.doc.text(s, "123");
    t.doc.text(a, "X");
    let f = t.run(800.0, 600.0);
    let r = text_rects(&f);
    assert_eq!(r.len(), 3);
    // (three digits in one 10px square of the line)
    assert_eq!(r[1].height, 10.0);
    assert_eq!(r[2].y, 20.0);
}

#[test]
fn table_rows_are_read_in_the_tables_frame() {
    let mut t = T::new();
    let root = t.root();
    let table = t.doc.element(root, "table", |s| s.display = Display::TABLE);
    let row = t.doc.element(table, "tr", |s| {
        s.display = Display::TABLE_ROW;
        s.writing_mode = WritingMode::VerticalRl;
    });
    let cell = t.doc.element(row, "td", |s| {
        s.display = Display::TABLE_CELL;
        s.writing_mode = WritingMode::HorizontalTb;
        s.width = px(20.0);
        s.height = px(30.0);
    });
    let f = t.run(800.0, 600.0);
    let r = rect(&f, cell);
    assert_eq!((r.width, r.height), (20.0, 30.0));
}

#[test]
fn orthogonal_abspos_width_is_its_block_size_in_the_height_it_has() {
    let mut t = T::new();
    let root = t.root();
    let cb = t.doc.block(root, |s| {
        s.position = Position::Relative;
        s.width = px(200.0);
        s.height = px(100.0);
    });
    let a = t.doc.block(cb, |s| {
        s.position = Position::Absolute;
        s.writing_mode = WritingMode::VerticalRl;
        s.inset[0] = LengthPercentageAuto::px(35.0); // top
        s.inset[2] = LengthPercentageAuto::px(35.0); // bottom
    });
    t.doc.text(a, "XXX XXX");
    let f = t.run(800.0, 600.0);
    let r = rect(&f, a);
    // (two lines of 10px in the 30px between the insets)
    assert_eq!((r.width, r.height), (20.0, 30.0));
}

#[test]
fn a_cells_padding_adds_to_its_block_size_in_a_vertical_table() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let table = t.doc.element(root, "table", |s| s.display = Display::TABLE);
    let row = t
        .doc
        .element(table, "tr", |s| s.display = Display::TABLE_ROW);
    let cell = t.doc.element(row, "td", |s| {
        s.display = Display::TABLE_CELL;
        s.width = px(100.0);
        s.height = px(50.0);
        s.padding = [LengthPercentage::px(1.0); 4];
    });
    let f = t.run(800.0, 600.0);
    let r = rect(&f, cell);
    assert_eq!((r.width, r.height), (102.0, 52.0));
}

#[test]
fn last_baselines_of_a_flex_line_line_up_at_its_end() {
    let mut t = T::new();
    let root = t.root();
    let c = t.doc.block(root, |s| {
        s.display = Display::FLEX;
        s.align_items = Align::LastBaseline;
        s.width = px(300.0);
        s.height = px(100.0);
    });
    let a = t.doc.block(c, |_| {});
    t.doc.text(a, "X");
    let b = t.doc.block(c, |s| s.width = px(30.0));
    t.doc.text(b, "XXX XXX");
    let f = t.run(800.0, 600.0);
    assert_eq!((rect(&f, a).y, rect(&f, b).y), (90.0, 80.0));
}

#[test]
fn self_start_of_an_abspos_box_is_in_its_own_writing_mode() {
    let mut t = doc(WritingMode::VerticalRl);
    let root = t.root();
    let c = t.doc.block(root, |s| {
        s.display = Display::FLEX;
        s.position = Position::Relative;
        s.width = px(100.0);
        s.height = px(50.0);
    });
    let a = t.doc.block(c, |s| {
        s.position = Position::Absolute;
        s.writing_mode = WritingMode::HorizontalTb;
        s.align_self = Align::SelfStart;
        s.width = px(20.0);
        s.height = px(10.0);
    });
    let b = t.doc.block(c, |s| {
        s.position = Position::Absolute;
        s.writing_mode = WritingMode::HorizontalTb;
        s.align_self = Align::SelfEnd;
        s.width = px(20.0);
        s.height = px(10.0);
    });
    let f = t.run(800.0, 600.0);
    let cx = rect(&f, c).x;
    // The container's cross axis runs right to left, the boxes' own left to right.
    assert_eq!(rect(&f, a).x - cx, 0.0);
    assert_eq!(rect(&f, b).x - cx, 80.0);
}
