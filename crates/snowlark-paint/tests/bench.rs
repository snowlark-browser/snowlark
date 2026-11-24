// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What it costs a list to cross as bytes: `--test it bench:: '--' --ignored --nocapture`.

use std::time::{Duration, Instant};

use snowlark_paint::*;

/// A list shaped like a long page's: mostly short text runs and boxes in nested clips and
/// transforms, with images, gradients, borders and shadows among them. About `blocks * 20` items.
fn page_list(blocks: usize) -> DisplayList {
    let glyphs: Vec<u16> = (40..64).collect();
    let advances = vec![7.5f32; glyphs.len()];
    let offsets = vec![GlyphOffset::default(); glyphs.len()];
    let stops = [
        GradientStop {
            offset: 0.0,
            color: Color::rgb(10, 20, 30),
        },
        GradientStop {
            offset: 1.0,
            color: Color::rgb(200, 210, 220),
        },
    ];
    let mut b = DisplayListBuilder::new();
    for i in 0..blocks {
        let y = i as f32 * 96.0;
        let r = Rect::new(8.0, y, 760.0, 90.0);
        b.push_clip_rect(r);
        b.push_transform(Matrix::translation(0.0, 0.5));
        b.fill_rect(r, Color::rgb(250, 250, 250));
        b.border(Border {
            rect: r,
            radii: Radii::uniform(4.0),
            widths: Sides::new(1.0, 1.0, 1.0, 1.0),
            colors: [Color::BLACK; 4],
            styles: [BorderStyle::Solid; 4],
        });
        for line in 0..10 {
            let at = Point::new(16.0, y + 14.0 + line as f32 * 7.0);
            let (font, ink) = (FontId(line % 3), Color::rgb(20, 20, 20));
            b.text(&glyphs, &advances, &offsets, at, font, 14.0, ink, 0);
        }
        match i % 4 {
            0 => {
                let dest = Rect::new(600.0, y + 4.0, 80.0, 80.0);
                b.image(ImageId(i as u32), dest, None, 1.0, ImageRendering::Smooth);
            }
            1 => {
                let (from, to) = (Point::new(8.0, y), Point::new(768.0, y));
                b.linear_gradient(r, Radii::ZERO, from, to, &stops, Extend::Clamp);
            }
            2 => b.box_shadow(BoxShadow {
                rect: r,
                radii: Radii::ZERO,
                offset: Point::new(0.0, 2.0),
                blur: 6.0,
                spread: 0.0,
                color: Color::rgba(0, 0, 0, 80),
                inset: false,
            }),
            _ => b.fill_rounded_rect(r, Radii::uniform(8.0), Color::rgb(240, 240, 200)),
        }
        b.pop_transform();
        b.pop_clip();
    }
    b.build()
}

/// The fastest of many runs of `f`, in ms.
fn best(mut f: impl FnMut()) -> f64 {
    let mut least = Duration::MAX;
    for _ in 0..30 {
        let t = Instant::now();
        f();
        least = least.min(t.elapsed());
    }
    least.as_secs_f64() * 1000.0
}

#[test]
#[ignore]
fn list_encode_and_decode() {
    for blocks in [500, 5000] {
        let list = page_list(blocks);
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        let size = bytes.len();
        let encode = best(|| {
            bytes.clear();
            list.encode(&mut bytes);
        });
        let mut out = DisplayList::new();
        let decode = best(|| out.decode_from(&bytes).unwrap());
        assert_eq!(out, list);
        let fresh = best(|| drop(DisplayList::decode(&bytes).unwrap()));
        let validate = best(|| list.validate().unwrap());
        let mut hashes = Vec::new();
        let hash = best(|| list.content_hashes(&|_| 1, &mut hashes));
        println!(
            "{} items, {} glyphs, {} KB: encode {encode:.3} ms, decode {decode:.3} ms \
             (into a new list {fresh:.3} ms; of it validate {validate:.3} ms), content_hashes {hash:.3} ms",
            list.len(),
            list.glyph_ids.len(),
            size / 1024,
        );
    }
}

/// A frame of the 500-block page rastered in tiles on the GPU: plain, with a color filter around
/// everything, and with a large blur around everything (Filter Effects 1 §10), each the best of
/// 30 frames with the tiles rastered again every time.
#[test]
#[ignore]
fn filter_raster() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let gpu = Gpu::new()
        .or_else(|_| Gpu::with_driver(Driver::Warp))
        .unwrap();
    let mut tiles = TileRasterizer::new(gpu).unwrap();
    let view = Viewport {
        width: 1300,
        height: 1000,
        scale: 1.0,
        background: Color::WHITE,
    };
    let plain = page_list(500);
    let wrapped = |ops: &[FilterOp]| {
        let mut b = DisplayListBuilder::new();
        b.push_filter(ops, Rect::new(-100.0, -100.0, 1500.0, 1200.0));
        b.append(&plain);
        b.pop_filter();
        b.build()
    };
    let gray = wrapped(&[FilterOp::Grayscale(1.0)]);
    let blurred = wrapped(&[FilterOp::Blur(20.0)]);
    for (what, list) in [
        ("plain", &plain),
        ("grayscale", &gray),
        ("blur 20px", &blurred),
    ] {
        let mut frame = 0u8;
        let ms = best(|| {
            // (a list that differs each frame: its tiles are not reused)
            frame = frame.wrapping_add(1);
            let mut fresh = list.clone();
            fresh.items.push(Item::FillRect {
                rect: Rect::new(0.0, 0.0, 1.0, 1.0),
                color: Color::rgba(0, 0, 0, frame),
            });
            tiles
                .raster(
                    &fresh,
                    Sources {
                        text: &text,
                        images: &images,
                    },
                    &view,
                )
                .unwrap();
        });
        println!("{what}: {ms:.2} ms a frame (1300x1000, 6 tiles)");
    }
}

/// The copy an image costs on its way to the raster side.
#[test]
#[ignore]
fn image_copy() {
    let pixels = vec![7u8; 4000 * 3000 * 4];
    let copy = best(|| drop(std::hint::black_box(pixels.clone())));
    println!("{} MB copied in {copy:.2} ms", pixels.len() >> 20);
}

/// Decoding speed of each image in the folder `SNOWLARK_DECODE_BENCH` names, through the decoders
/// pages use ([`image::decode`]) and through WIC ([`image::decode_wic`]), and the largest channel
/// difference between the two. Without the variable it prints how to make such a folder.
#[test]
#[ignore]
fn image_decode() {
    let Some(dir) = std::env::var_os("SNOWLARK_DECODE_BENCH") else {
        println!("set SNOWLARK_DECODE_BENCH to a folder of PNG, JPEG, GIF and WebP files");
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        let ours = image::decode(&bytes).unwrap();
        let wic = image::decode_wic(&bytes).unwrap();
        let (a, b) = (&ours.frames[0], &wic.frames[0]);
        let diff = if (a.width, a.height) == (b.width, b.height) {
            a.pixels
                .iter()
                .zip(&b.pixels)
                .map(|(x, y)| x.abs_diff(*y))
                .max()
                .unwrap_or(0)
                .to_string()
        } else {
            format!("size {}x{} vs {}x{}", a.width, a.height, b.width, b.height)
        };
        let t_ours = best(|| drop(std::hint::black_box(image::decode(&bytes).unwrap())));
        let t_wic = best(|| drop(std::hint::black_box(image::decode_wic(&bytes).unwrap())));
        let name = path.file_name().unwrap().to_string_lossy();
        println!(
            "{name:28} {}x{}: {t_ours:7.2} ms, WIC {t_wic:7.2} ms ({:.2}x); max difference {diff}",
            a.width,
            a.height,
            t_ours / t_wic
        );
    }
}
