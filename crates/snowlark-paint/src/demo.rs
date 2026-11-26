// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A demo display list exercising every item kind: nested boxes with borders and shadows, text in
//! several families, sizes and weights (CJK, Arabic, emoji through fallback), a rotated opacity
//! layer, gradients and decoded images. Used by `snowlark --render-demo` and the integration test.

use windows::core::Result;

use crate::display_list::*;
use crate::geom::{Color, Matrix, Point, Radii, Rect, Sides, Size};
use crate::image::{bmp_from_rgba, decode_wic, ImageStore};
use crate::text::{
    Direction, FontQuery, FontStretch, FontStyle, ShapeOptions, ShapedText, TextEngine,
};

/// The size the demo is laid out for, in CSS px.
pub const WIDTH: u32 = 1300;
pub const HEIGHT: u32 = 1000;
pub const BACKGROUND: Color = Color::hex(0xf3efe6);

/// Spots of the demo with known colors, for tests: (x, y, color) in CSS px.
pub const PROBES: &[(f32, f32, Color)] = &[
    // the page background
    (1290.0, 130.0, BACKGROUND),
    // box A: fill, top border (red), left border (blue)
    (318.0, 308.0, Color::hex(0xfff7e0)),
    (200.0, 172.0, Color::hex(0xd0021b)),
    (72.0, 300.0, Color::hex(0x1f6fd0)),
    // the card's white fill between the boxes
    (345.0, 335.0, Color::WHITE),
];

struct Writer<'a> {
    text: &'a TextEngine,
    shaped: ShapedText,
}

impl Writer<'_> {
    #[allow(clippy::too_many_arguments)]
    fn line(
        &mut self,
        out: &mut DisplayListBuilder,
        s: &str,
        families: &[&str],
        weight: u16,
        style: FontStyle,
        size: f32,
        at: Point,
        color: Color,
        direction: Direction,
    ) -> Result<f32> {
        let list = self.text.font_list(&FontQuery {
            families,
            weight,
            style,
            stretch: FontStretch::NORMAL,
        })?;
        self.text.shape(
            s,
            list,
            &ShapeOptions {
                size,
                direction,
                ..Default::default()
            },
            &mut self.shaped,
        )?;
        let n = self.shaped.clusters.len();
        // right-to-left paragraphs are right-aligned at `at.x`
        let x = if direction == Direction::Rtl {
            at.x - self.shaped.width()
        } else {
            at.x
        };
        Ok(self.shaped.paint(0..n, Point::new(x, at.y), color, out))
    }
}

fn border(
    rect: Rect,
    radii: Radii,
    w: Sides,
    colors: [Color; 4],
    styles: [BorderStyle; 4],
) -> Border {
    Border {
        rect,
        radii,
        widths: w,
        colors,
        styles,
    }
}

/// A smooth color disc with soft alpha at its edge.
fn disc(size: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    let c = size as f32 / 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - c;
            let dy = y as f32 + 0.5 - c;
            let r = (dx * dx + dy * dy).sqrt() / c;
            let angle = dy.atan2(dx);
            let ch = |phase: f32| ((angle + phase).cos() * 0.5 + 0.5) * (1.0 - r * 0.4);
            let a = ((1.0 - r) * 12.0).clamp(0.0, 1.0);
            rgba.extend_from_slice(&[
                (ch(0.0) * 255.0) as u8,
                (ch(2.094) * 255.0) as u8,
                (ch(4.189) * 255.0) as u8,
                (a * 255.0) as u8,
            ]);
        }
    }
    rgba
}

/// A 12×12 pixel-art face, for `image-rendering: pixelated`.
fn face() -> Vec<u8> {
    const ART: [&str; 12] = [
        "....YYYY....",
        "..YYYYYYYY..",
        ".YYYYYYYYYY.",
        ".YYKYYYYKYY.",
        "YYYKYYYYKYYY",
        "YYYYYYYYYYYY",
        "YYYYYYYYYYYY",
        "YYKYYYYYYKYY",
        ".YYKKKKKKYY.",
        ".YYYYYYYYYY.",
        "..YYYYYYYY..",
        "....YYYY....",
    ];
    let mut rgba = Vec::new();
    for row in ART {
        for ch in row.bytes() {
            rgba.extend_from_slice(match ch {
                b'Y' => &[255, 200, 40, 255],
                b'K' => &[40, 30, 20, 255],
                _ => &[0, 0, 0, 0],
            });
        }
    }
    rgba
}

/// Builds the demo. Images are decoded (from BMP bytes made here) and added to `images`.
pub fn build(text: &TextEngine, images: &ImageStore) -> Result<DisplayList> {
    let mut b = DisplayListBuilder::new();
    let mut w = Writer {
        text,
        shaped: ShapedText::default(),
    };
    let ui = &["Segoe UI"];
    let normal = FontStyle::Normal;
    let ltr = Direction::Ltr;

    // Header: a gradient band with the title.
    b.linear_gradient(
        Rect::new(0.0, 0.0, WIDTH as f32, 110.0),
        Radii::ZERO,
        Point::new(0.0, 0.0),
        Point::new(WIDTH as f32, 0.0),
        &[
            GradientStop {
                offset: 0.0,
                color: Color::hex(0x1f3b73),
            },
            GradientStop {
                offset: 1.0,
                color: Color::hex(0x6a2c91),
            },
        ],
        Extend::Clamp,
    );
    w.line(
        &mut b,
        "Snowlark paint",
        &["Segoe UI"],
        600,
        normal,
        44.0,
        Point::new(40.0, 66.0),
        Color::WHITE,
        ltr,
    )?;
    w.line(
        &mut b,
        "Direct2D + DirectWrite + WIC, from one display list",
        ui,
        400,
        normal,
        18.0,
        Point::new(42.0, 96.0),
        Color::rgba(255, 255, 255, 210),
        ltr,
    )?;

    // A card with a soft shadow holding four boxes.
    let card = Rect::new(40.0, 140.0, 600.0, 400.0);
    b.box_shadow(BoxShadow {
        rect: card,
        radii: Radii::uniform(18.0),
        offset: Point::new(0.0, 10.0),
        blur: 28.0,
        spread: 0.0,
        color: Color::rgba(40, 30, 10, 90),
        inset: false,
    });
    b.fill_rounded_rect(card, Radii::uniform(18.0), Color::WHITE);
    b.border(border(
        card,
        Radii::uniform(18.0),
        Sides::all(2.0),
        [Color::hex(0xc9c2b4); 4],
        [BorderStyle::Solid; 4],
    ));

    // A: four colors, square.
    let a = Rect::new(70.0, 170.0, 260.0, 150.0);
    b.fill_rect(a, Color::hex(0xfff7e0));
    b.border(border(
        a,
        Radii::ZERO,
        Sides::all(6.0),
        [
            Color::hex(0xd0021b),
            Color::hex(0x2e9e44),
            Color::hex(0xf5a623),
            Color::hex(0x1f6fd0),
        ],
        [BorderStyle::Solid; 4],
    ));
    w.line(
        &mut b,
        "solid, four colors",
        ui,
        400,
        normal,
        16.0,
        Point::new(90.0, 205.0),
        Color::hex(0x333333),
        ltr,
    )?;

    // B: dashed, rounded, with an inset shadow.
    let bx = Rect::new(360.0, 170.0, 250.0, 150.0);
    b.fill_rounded_rect(bx, Radii::uniform(14.0), Color::hex(0xeef4ff));
    // (an inset shadow takes the padding box)
    b.box_shadow(BoxShadow {
        rect: bx.deflate(&Sides::all(4.0)),
        radii: Radii::uniform(14.0).inner(&Sides::all(4.0)),
        offset: Point::new(4.0, 4.0),
        blur: 12.0,
        spread: 0.0,
        color: Color::rgba(20, 40, 120, 110),
        inset: true,
    });
    b.border(border(
        bx,
        Radii::uniform(14.0),
        Sides::all(4.0),
        [Color::hex(0x3366cc); 4],
        [BorderStyle::Dashed; 4],
    ));
    w.line(
        &mut b,
        "dashed, inset shadow",
        ui,
        400,
        normal,
        16.0,
        Point::new(380.0, 205.0),
        Color::hex(0x223366),
        ltr,
    )?;

    // C: double, elliptical corners.
    let c = Rect::new(70.0, 350.0, 260.0, 160.0);
    let c_radii = Radii {
        top_left: Size::new(40.0, 24.0),
        top_right: Size::new(6.0, 6.0),
        bottom_right: Size::new(40.0, 24.0),
        bottom_left: Size::new(6.0, 6.0),
    };
    b.fill_rounded_rect(c, c_radii, Color::hex(0xf0fff4));
    b.border(border(
        c,
        c_radii,
        Sides::all(9.0),
        [Color::hex(0x2d6a4f); 4],
        [BorderStyle::Double; 4],
    ));
    w.line(
        &mut b,
        "double, elliptical radii",
        ui,
        400,
        normal,
        16.0,
        Point::new(95.0, 390.0),
        Color::hex(0x1b4332),
        ltr,
    )?;

    // D: a different style per side.
    let d = Rect::new(360.0, 350.0, 250.0, 160.0);
    b.fill_rect(d, Color::hex(0xfdf0f0));
    b.border(border(
        d,
        Radii::ZERO,
        Sides::new(10.0, 10.0, 10.0, 10.0),
        [Color::hex(0xb07070); 4],
        [
            BorderStyle::Groove,
            BorderStyle::Ridge,
            BorderStyle::Dotted,
            BorderStyle::Outset,
        ],
    ));
    w.line(
        &mut b,
        "groove, ridge, dotted, outset",
        ui,
        400,
        normal,
        15.0,
        Point::new(380.0, 390.0),
        Color::hex(0x6b2d2d),
        ltr,
    )?;

    // Text samples.
    let ink = Color::hex(0x1d1d1f);
    let x = 690.0;
    let mut y = 170.0;
    let samples: &[(&str, &[&str], u16, FontStyle, f32)] = &[
        (
            "The quick brown fox jumps over the lazy dog: 0123456789",
            &["Segoe UI"],
            400,
            normal,
            17.0,
        ),
        (
            "Georgia italic: office ffi fl, kerning AV To Wa",
            &["Georgia"],
            400,
            FontStyle::Italic,
            23.0,
        ),
        (
            "fn main() { println!(\"monospace\"); }",
            &["monospace"],
            400,
            normal,
            16.0,
        ),
        (
            "Times New Roman Bold 26px",
            &["Times New Roman"],
            700,
            normal,
            26.0,
        ),
        ("Segoe UI Light 300, 30px", &["Segoe UI"], 300, normal, 30.0),
        ("Arial Black 900", &["Arial"], 900, normal, 22.0),
    ];
    for (s, families, weight, style, size) in samples {
        y += size * 1.45;
        w.line(
            &mut b,
            s,
            families,
            *weight,
            *style,
            *size,
            Point::new(x, y),
            ink,
            ltr,
        )?;
    }
    y += 44.0;
    w.line(
        &mut b,
        "日本語のテキスト・中文文本・한국어",
        ui,
        400,
        normal,
        26.0,
        Point::new(x, y),
        ink,
        ltr,
    )?;
    y += 46.0;
    w.line(
        &mut b,
        "مرحبا بالعالم: نص عربي من اليمين",
        ui,
        400,
        normal,
        28.0,
        Point::new(1250.0, y),
        ink,
        Direction::Rtl,
    )?;
    y += 54.0;
    w.line(
        &mut b,
        "Emoji 😀 🎉 👍🏽 🇯🇵 👨‍👩‍👧 ❤️",
        ui,
        400,
        normal,
        34.0,
        Point::new(x, y),
        ink,
        ltr,
    )?;

    // A rotated layer at 75% opacity: a radial gradient card with text and the disc image.
    let disc_id = images.add(decode_wic(&bmp_from_rgba(128, 128, &disc(128)))?);
    let layer = Rect::new(90.0, 600.0, 470.0, 300.0);
    let center = Point::new(layer.x + layer.width / 2.0, layer.y + layer.height / 2.0);
    b.push_transform(Matrix::rotation_about(-8f32.to_radians(), center));
    b.push_opacity(0.75, layer.inflate(4.0, 4.0));
    b.radial_gradient(
        layer,
        Radii::uniform(24.0),
        Point::new(layer.x + 120.0, layer.y + 90.0),
        Size::new(420.0, 300.0),
        &[
            GradientStop {
                offset: 0.0,
                color: Color::hex(0xff8a5c),
            },
            GradientStop {
                offset: 0.6,
                color: Color::hex(0xd6336c),
            },
            GradientStop {
                offset: 1.0,
                color: Color::hex(0x5f3dc4),
            },
        ],
        Extend::Clamp,
    );
    b.stroke_rounded_rect(
        layer.inflate(-6.0, -6.0),
        Radii::uniform(19.0),
        2.0,
        Color::rgba(255, 255, 255, 160),
    );
    w.line(
        &mut b,
        "Rotated −8°, 75% opacity",
        ui,
        600,
        normal,
        28.0,
        Point::new(layer.x + 28.0, layer.y + 56.0),
        Color::WHITE,
        ltr,
    )?;
    b.push_clip_rounded_rect(
        Rect::new(layer.x + 28.0, layer.y + 90.0, 180.0, 180.0),
        Radii::uniform(90.0),
    );
    b.image(
        disc_id,
        Rect::new(layer.x + 28.0, layer.y + 90.0, 180.0, 180.0),
        None,
        1.0,
        ImageRendering::Smooth,
    );
    b.pop_clip();
    w.line(
        &mut b,
        "clipped image, round",
        ui,
        400,
        normal,
        18.0,
        Point::new(layer.x + 230.0, layer.y + 190.0),
        Color::WHITE,
        ltr,
    )?;
    b.pop_opacity();
    b.pop_transform();

    // Decoded images: the disc at its size and larger, and pixel art scaled up crisply.
    let face_id = images.add(decode_wic(&bmp_from_rgba(12, 12, &face()))?);
    b.image(
        disc_id,
        Rect::new(690.0, 680.0, 128.0, 128.0),
        None,
        1.0,
        ImageRendering::Smooth,
    );
    b.image(
        disc_id,
        Rect::new(840.0, 650.0, 190.0, 190.0),
        None,
        1.0,
        ImageRendering::Smooth,
    );
    b.image(
        face_id,
        Rect::new(1060.0, 660.0, 168.0, 168.0),
        None,
        1.0,
        ImageRendering::Pixelated,
    );
    w.line(
        &mut b,
        "WIC-decoded: 1×, scaled, pixelated",
        ui,
        400,
        normal,
        15.0,
        Point::new(690.0, 870.0),
        Color::hex(0x555555),
        ltr,
    )?;

    // A repeating stripe gradient along the bottom.
    b.linear_gradient(
        Rect::new(690.0, 900.0, 560.0, 50.0),
        Radii::uniform(10.0),
        Point::new(690.0, 900.0),
        Point::new(714.0, 924.0),
        &[
            GradientStop {
                offset: 0.0,
                color: Color::hex(0x2b2d42),
            },
            GradientStop {
                offset: 0.5,
                color: Color::hex(0x2b2d42),
            },
            GradientStop {
                offset: 0.5,
                color: Color::hex(0xedf2f4),
            },
            GradientStop {
                offset: 1.0,
                color: Color::hex(0xedf2f4),
            },
        ],
        Extend::Repeat,
    );
    Ok(b.build())
}

/// About 1,000 words of English prose, for shaping measurements.
pub fn thousand_words() -> String {
    const SENTENCE: &str = "Browsers shape text by splitting it into runs of one font, script and \
        direction, mapping characters to glyphs and placing them with kerning and ligatures, \
        while layout asks where lines may break and how wide each cluster is. ";
    let words = SENTENCE.split_whitespace().count();
    SENTENCE.repeat(1000_usize.div_ceil(words))
}
