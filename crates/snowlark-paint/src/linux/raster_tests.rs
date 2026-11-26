// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Skia painter's tests: what each kind of item puts in the pixels.

use super::*;
use crate::display_list::{DisplayListBuilder, GlyphOffset};
use crate::image::{DecodedImage, Frame, ImageStore};
use crate::text::{FontQuery, FontStretch, FontStyle, ShapeOptions, ShapedText};

const WHITE: Color = Color::rgb(255, 255, 255);
const RED: Color = Color::rgb(255, 0, 0);
const BLUE: Color = Color::rgb(0, 0, 255);

fn render_with(
    list: &DisplayList,
    text: &TextEngine,
    images: &ImageStore,
    w: u32,
    h: u32,
    scale: f32,
) -> Pixmap {
    let mut r = HeadlessRenderer::new().unwrap();
    r.render(
        list,
        Sources { text, images },
        &Viewport {
            width: w,
            height: h,
            scale,
            background: WHITE,
        },
    )
    .unwrap()
}

fn render(list: &DisplayList, w: u32, h: u32) -> Pixmap {
    render_with(
        list,
        &TextEngine::new().unwrap(),
        &ImageStore::default(),
        w,
        h,
        1.0,
    )
}

#[test]
fn rects_snap_to_device_pixels() {
    let mut b = DisplayListBuilder::new();
    // at scale 2, 10.3 → 20.6 device px rounds to 21, 19.7 → 39.4 to 39
    b.fill_rect(Rect::new(10.3, 5.0, 9.4, 5.0), RED);
    let p = render_with(
        &b.build(),
        &TextEngine::new().unwrap(),
        &ImageStore::default(),
        60,
        40,
        2.0,
    );
    assert_eq!(p.pixel(20, 12), WHITE);
    assert_eq!(p.pixel(21, 12), RED);
    assert_eq!(p.pixel(38, 12), RED);
    assert_eq!(p.pixel(39, 12), WHITE);
}

#[test]
fn a_border_paints_its_ring_by_side() {
    let mut b = DisplayListBuilder::new();
    b.border(Border {
        rect: Rect::new(10.0, 10.0, 40.0, 30.0),
        radii: Radii::default(),
        widths: Sides::all(4.0),
        colors: [RED, BLUE, RED, BLUE],
        styles: [BorderStyle::Solid; 4],
    });
    let p = render(&b.build(), 60, 50);
    assert_eq!(p.pixel(30, 11), RED);
    assert_eq!(p.pixel(48, 25), BLUE);
    assert_eq!(p.pixel(30, 38), RED);
    assert_eq!(p.pixel(11, 25), BLUE);
    assert_eq!(p.pixel(30, 25), WHITE);
}

#[test]
fn an_opacity_group_composites_once() {
    let mut b = DisplayListBuilder::new();
    let r = Rect::new(0.0, 0.0, 20.0, 20.0);
    b.push_opacity(0.5, r);
    b.fill_rect(r, RED);
    b.fill_rect(r, RED);
    b.pop_opacity();
    let p = render(&b.build(), 20, 20);
    let c = p.pixel(10, 10);
    assert_eq!((c.r, c.a), (255, 255));
    assert!((126..=129).contains(&c.g), "{c:?}");
}

#[test]
fn a_linear_gradient_runs_from_stop_to_stop() {
    let mut b = DisplayListBuilder::new();
    b.linear_gradient(
        Rect::new(0.0, 0.0, 100.0, 10.0),
        Radii::default(),
        Point::new(0.0, 0.0),
        Point::new(100.0, 0.0),
        &[
            GradientStop {
                offset: 0.0,
                color: RED,
            },
            GradientStop {
                offset: 1.0,
                color: BLUE,
            },
        ],
        Extend::Clamp,
    );
    let p = render(&b.build(), 100, 10);
    assert!(p.pixel(1, 5).r > 240 && p.pixel(1, 5).b < 15);
    assert!(p.pixel(98, 5).b > 240 && p.pixel(98, 5).r < 15);
    let mid = p.pixel(50, 5);
    assert!(
        (110..=145).contains(&mid.r) && (110..=145).contains(&mid.b),
        "{mid:?}"
    );
}

#[test]
fn a_blur_spreads_the_content() {
    let mut b = DisplayListBuilder::new();
    let r = Rect::new(20.0, 20.0, 20.0, 20.0);
    b.push_filter(&[FilterOp::Blur(4.0)], r.inflate(12.0, 12.0));
    b.fill_rect(r, RED);
    b.pop_filter();
    let p = render(&b.build(), 60, 60);
    // outside the rect, red bleeds in; at its middle (2.5 deviations in) it is nearly all red
    let edge = p.pixel(17, 30);
    assert!(edge.g < 250 && edge.r == 255, "{edge:?}");
    let mid = p.pixel(30, 30);
    assert!(mid.r == 255 && mid.g < 10, "{mid:?}");
    assert_eq!(p.pixel(2, 2), WHITE);
}

#[test]
fn a_mask_keeps_its_coverage() {
    let mut b = DisplayListBuilder::new();
    b.push_mask(Rect::new(0.0, 0.0, 40.0, 20.0), false);
    b.fill_rect(Rect::new(0.0, 0.0, 20.0, 20.0), BLUE);
    b.mask_content();
    b.fill_rect(Rect::new(0.0, 0.0, 40.0, 20.0), RED);
    b.pop_mask();
    let p = render(&b.build(), 40, 20);
    assert_eq!(p.pixel(10, 10), RED);
    assert_eq!(p.pixel(30, 10), WHITE);
}

#[test]
fn an_image_fills_its_destination() {
    let images = ImageStore::default();
    // 2x1: red, blue (premultiplied BGRA)
    let id = images.add(DecodedImage::still(Frame {
        width: 2,
        height: 1,
        pixels: vec![0, 0, 255, 255, 255, 0, 0, 255],
        delay_ms: 0,
    }));
    let mut b = DisplayListBuilder::new();
    b.image(
        id,
        Rect::new(0.0, 0.0, 40.0, 20.0),
        None,
        1.0,
        ImageRendering::Pixelated,
    );
    let p = render_with(
        &b.build(),
        &TextEngine::new().unwrap(),
        &images,
        40,
        20,
        1.0,
    );
    assert_eq!(p.pixel(5, 10), RED);
    assert_eq!(p.pixel(35, 10), BLUE);
}

fn shaped(e: &TextEngine, text: &str) -> ShapedText {
    let l = e
        .font_list(&FontQuery {
            families: &["DejaVu Sans"],
            weight: 400,
            style: FontStyle::Normal,
            stretch: FontStretch::NORMAL,
        })
        .unwrap();
    let mut out = ShapedText::default();
    e.shape(
        text,
        l,
        &ShapeOptions {
            size: 20.0,
            ..Default::default()
        },
        &mut out,
    )
    .unwrap();
    out
}

/// The columns of `p` with a dark pixel.
fn inked(p: &Pixmap) -> Vec<u32> {
    (0..p.width)
        .filter(|&x| (0..p.height).any(|y| p.pixel(x, y).r < 128))
        .collect()
}

#[test]
fn text_is_drawn_where_it_was_shaped() {
    let e = TextEngine::new().unwrap();
    let s = shaped(&e, "Hello");
    let mut b = DisplayListBuilder::new();
    let width = s.paint(
        0..s.clusters.len(),
        Point::new(10.0, 30.0),
        Color::BLACK,
        &mut b,
    );
    let p = render_with(&b.build(), &e, &ImageStore::default(), 120, 40, 1.0);
    let ink = inked(&p);
    assert!(!ink.is_empty());
    assert!(
        *ink.first().unwrap() >= 10 && *ink.first().unwrap() <= 13,
        "{ink:?}"
    );
    assert!(
        *ink.last().unwrap() as f32 <= 10.0 + width + 1.0,
        "{ink:?} {width}"
    );
    // nothing above the ascent or below the descent
    assert!((0..p.width).all(|x| p.pixel(x, 2).r > 200 && p.pixel(x, 38).r > 200));
}

#[test]
fn right_to_left_text_stays_in_its_run() {
    let e = TextEngine::new().unwrap();
    let s = shaped(&e, "مرحبا");
    assert!(s.runs.iter().all(|r| r.bidi_level == 1));
    let mut b = DisplayListBuilder::new();
    let width = s.paint(
        0..s.clusters.len(),
        Point::new(20.0, 30.0),
        Color::BLACK,
        &mut b,
    );
    let p = render_with(&b.build(), &e, &ImageStore::default(), 140, 40, 1.0);
    let ink = inked(&p);
    assert!(!ink.is_empty());
    assert!(*ink.first().unwrap() >= 19, "{ink:?}");
    assert!(
        *ink.last().unwrap() as f32 <= 20.0 + width + 1.0,
        "{ink:?} {width}"
    );
}

#[test]
fn glyph_offsets_move_glyphs_up() {
    let e = TextEngine::new().unwrap();
    let s = shaped(&e, "x");
    let run = |offset: f32| {
        let mut b = DisplayListBuilder::new();
        b.text(
            &s.glyphs,
            &s.advances,
            &[GlyphOffset {
                advance_offset: 0.0,
                ascender_offset: offset,
            }],
            Point::new(5.0, 30.0),
            s.runs[0].font,
            20.0,
            Color::BLACK,
            0,
        );
        let p = render_with(&b.build(), &e, &ImageStore::default(), 30, 40, 1.0);
        (0..p.height).find(|&y| (0..p.width).any(|x| p.pixel(x, y).r < 128))
    };
    assert_eq!(run(0.0).unwrap() - run(5.0).unwrap(), 5);
}
