// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every font of this machine, resolved by a raster side's own [`FontTable`] from what a renderer
//! says of it, draws the pixels the [`TextEngine`] that shaped it draws: by its file, and as bytes.
//!
//! Run by hand: `--test it font_table:: '--' --ignored --nocapture`. It is ignored because touching
//! every font of the machine changes what comes next: the next process to draw text in a system font
//! comes out a few pixels apart from itself, which `word_shaping` fails on.
//!
//! Each drawing is on a device of its own. A face from the system's collection and another object
//! of the same font come out a few pixels apart now and then, by themselves (the same text in the
//! same face does too: `raster_pixels` in `snowlark-tab`), so [`same_font`] allows for that: a wrong
//! face, weight or slant changes thousands of bytes of these drawings.

use std::sync::Arc;

use snowlark_paint::*;

const SAMPLE: &str = "Sphinx of black quartz 0123 \u{E9}\u{F1}";

fn view() -> Viewport {
    Viewport {
        width: 420,
        height: 40,
        scale: 1.0,
        background: Color::WHITE,
    }
}

fn draw(tiles: &mut TileRasterizer, list: &DisplayList, text: &dyn FontResolver) -> Pixmap {
    let images = ImageStore::new();
    let sources = Sources {
        text,
        images: &images,
    };
    let frame = tiles.raster(list, sources, &view()).unwrap();
    let pixmap = tiles.read_back(&frame).unwrap();
    tiles.recycle(frame);
    pixmap
}

fn draw_alone(list: &DisplayList, text: &dyn FontResolver) -> Pixmap {
    let gpu = Gpu::with_driver(Driver::Warp).unwrap();
    draw(&mut TileRasterizer::new(gpu).unwrap(), list, text)
}

/// Whether two drawings differ no more than the same font does from itself: the most seen was 297
/// bytes of a frame, by at most 63.
fn same_font(a: &Pixmap, b: &Pixmap) -> bool {
    let differ = a.data.iter().zip(&b.data).filter(|(a, b)| a != b);
    let (mut bytes, mut most) = (0, 0);
    for (a, b) in differ {
        bytes += 1;
        most = most.max(a.abs_diff(*b));
    }
    bytes <= 400 && most <= 64
}

/// The sample in `family`.
fn sample(text: &TextEngine, family: &str, weight: u16, style: FontStyle) -> DisplayList {
    let families = [family];
    let list = text
        .font_list(&FontQuery {
            families: &families,
            weight,
            style,
            stretch: FontStretch::NORMAL,
        })
        .unwrap();
    let options = ShapeOptions {
        size: 19.0,
        ..Default::default()
    };
    let mut shaped = ShapedText::default();
    text.shape(SAMPLE, list, &options, &mut shaped).unwrap();
    let mut b = DisplayListBuilder::new();
    shaped.paint(
        0..shaped.clusters.len(),
        Point::new(4.0, 28.0),
        Color::rgb(20, 24, 32),
        &mut b,
    );
    b.build()
}

/// Draws `family` with a text engine of its own (the instances of a variable font share an id in
/// one). Returns how many fonts it took.
fn compare(family: &str, weight: u16, style: FontStyle) -> usize {
    let text = Arc::new(TextEngine::new().unwrap());
    let display = sample(&text, family, weight, style);
    let mut fonts: Vec<FontId> = display
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Text(run) => Some(run.font),
            _ => None,
        })
        .collect();
    fonts.sort();
    fonts.dedup();
    let expected = draw_alone(&display, &text);

    let mut by_file = FontTable::new().unwrap();
    let mut as_bytes = FontTable::new().unwrap();
    for font in &fonts {
        let source = text.font_source(*font).unwrap();
        by_file.add(*font, &source).unwrap();
        let FontFile::System { path, .. } = &source.file else {
            panic!("{family}: not a file of the system");
        };
        let path = String::from_utf16_lossy(path);
        let source = FontSource {
            file: FontFile::Data(std::fs::read(&path).unwrap()),
            ..source
        };
        as_bytes.add(*font, &source).unwrap();
    }
    let what = format!("{family} {weight} {style:?}");
    let got = draw_alone(&display, &by_file);
    assert!(same_font(&got, &expected), "{what}: by its file");
    let got = draw_alone(&display, &as_bytes);
    assert!(same_font(&got, &expected), "{what}: as bytes");
    fonts.len()
}

fn families() -> Vec<String> {
    let database = TextEngine::new().unwrap().font_database();
    let mut families: Vec<String> = database
        .faces()
        .filter_map(|f| f.families.first().map(|(name, _)| name.clone()))
        .collect();
    families.sort();
    families.dedup();
    assert!(families.len() > 20, "{} families", families.len());
    families
}

#[test]
#[ignore]
fn every_family_draws_the_same_through_a_font_table() {
    let families = families();
    let mut compared = 0;
    for family in &families {
        compared += compare(family, 400, FontStyle::Normal);
        compared += compare(family, 700, FontStyle::Normal);
        compared += compare(family, 400, FontStyle::Italic);
    }
    eprintln!("[fonts] {} families, {compared} fonts", families.len());
}
