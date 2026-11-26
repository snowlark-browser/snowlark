// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tile rasterizer's tests: tiles put together show what one picture of the list shows, and
//! a tile whose content did not change is copied, not painted.

use super::*;
use crate::display_list::DisplayListBuilder;
use crate::filter::FilterOp;
use crate::geom::{Color, Radii};
use crate::image::ImageStore;
use crate::raster::{HeadlessRenderer, ImageResolver};
use crate::text::TextEngine;

const WHITE: Color = Color::rgb(255, 255, 255);
const RED: Color = Color::rgb(255, 0, 0);
const BLUE: Color = Color::rgb(0, 0, 255);
const GREEN: Color = Color::rgb(0, 160, 0);

fn view(width: u32, height: u32, scale: f32) -> Viewport {
    Viewport {
        width,
        height,
        scale,
        background: WHITE,
    }
}

/// Shapes across tile edges: rects, a rounded rect, a stroked one, a group with opacity.
fn scene(shift: f32) -> DisplayList {
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(500.0 + shift, 20.0, 40.0, 1000.0), RED);
    b.fill_rounded_rect(
        Rect::new(300.0, 480.0, 400.0, 90.0),
        Radii::uniform(30.0),
        BLUE,
    );
    b.stroke_rect(Rect::new(10.3, 10.7, 1000.0, 520.5), 3.0, GREEN);
    b.push_opacity(0.5, Rect::new(700.0, 300.0, 300.0, 400.0));
    b.fill_rect(Rect::new(700.0, 300.0, 300.0, 400.0), RED);
    b.pop_opacity();
    b.build()
}

fn headless(list: &DisplayList, v: &Viewport) -> Pixmap {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::default();
    HeadlessRenderer::new()
        .unwrap()
        .render(
            list,
            Sources {
                text: &text,
                images: &images,
            },
            v,
        )
        .unwrap()
}

fn tiles(r: &mut TileRasterizer, list: &DisplayList, v: &Viewport) -> TiledFrame {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::default();
    r.raster(
        list,
        Sources {
            text: &text,
            images: &images,
        },
        v,
    )
    .unwrap()
}

/// The pixmaps are equal within 1 per channel but for a few pixels within 16; else the first pixels
/// that differ (x, y, ours, the picture's). (Skia's anti-aliasing of a shape that a tile's edge
/// clips rounds a little apart from the same shape unclipped: one unit along a curve, a few units
/// in the rows of an edge beside the tile's edge.)
fn assert_same(a: &Pixmap, b: &Pixmap) {
    let far = a
        .data
        .iter()
        .zip(&b.data)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0);
    assert!(far <= 16, "a channel differs by {far}");
    assert_eq!((a.width, a.height), (b.width, b.height));
    let differ: Vec<_> = a
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.data.as_chunks::<4>().0)
        .enumerate()
        .filter(|(_, (p, q))| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 1))
        .map(|(i, (p, q))| {
            (
                i as u32 % a.width,
                i as u32 / a.width,
                p.to_vec(),
                q.to_vec(),
            )
        })
        .collect();
    assert!(
        differ.len() <= 4,
        "{} pixels differ: {:?}",
        differ.len(),
        &differ[..differ.len().min(10)]
    );
}

fn rasterizer() -> TileRasterizer {
    TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap()
}

/// A raster side's table: it says what an image shows but keeps no pixels (they came by upload).
struct NoPixels;

impl ImageResolver for NoPixels {
    fn shown(&self, _id: ImageId) -> Option<ShownImage> {
        Some(ShownImage {
            frame: 0,
            version: 7,
            width: 2,
            height: 2,
            image: None,
        })
    }

    fn version(&self, _id: ImageId) -> u64 {
        7
    }
}

#[test]
fn uploaded_pixels_draw_where_the_table_has_none() {
    let mut b = DisplayListBuilder::new();
    b.image(
        ImageId(3),
        Rect::new(10.0, 10.0, 40.0, 40.0),
        None,
        1.0,
        crate::ImageRendering::Pixelated,
    );
    let list = b.build();
    let text = TextEngine::new().unwrap();
    let mut r = rasterizer();
    let shown = NoPixels.shown(ImageId(3)).unwrap();
    // Opaque blue, premultiplied BGRA.
    let blue = [255u8, 0, 0, 255].repeat(4);
    r.upload(&text, ImageId(3), &shown, &blue).unwrap();
    let sources = Sources {
        text: &text,
        images: &NoPixels,
    };
    let frame = r.raster(&list, sources, &view(64, 64, 1.0)).unwrap();
    let p = read_back(&frame);
    let at = |x: u32, y: u32| p.data[((y * p.width + x) * 4) as usize..][..4].to_vec();
    assert_eq!(at(30, 30), [255, 0, 0, 255], "the uploaded pixels");
    assert_eq!(at(5, 5), [255, 255, 255, 255], "the background");
    // Pixels of another version are not drawn for this one.
    let older = ShownImage {
        version: 6,
        ..shown
    };
    let mut r = rasterizer();
    r.upload(&text, ImageId(3), &older, &blue).unwrap();
    let p = read_back(&r.raster(&list, sources, &view(64, 64, 1.0)).unwrap());
    assert_eq!(
        p.data[((30 * p.width + 30) * 4) as usize..][..4],
        [255, 255, 255, 255]
    );
}

#[test]
fn only_the_software_device_can_be_made() {
    assert!(Gpu::with_driver(Driver::Hardware).is_err());
    let gpu = Gpu::with_driver(Driver::Warp).unwrap();
    assert_eq!(gpu.driver(), Driver::Warp);
    // (the default falls back to it, as on Windows without a usable card)
    assert_eq!(Gpu::new().unwrap().driver(), Driver::Warp);
}

#[test]
fn tiles_show_what_one_picture_shows() {
    for scale in [1.0, 1.5] {
        let v = view(1100, 700, scale);
        let list = scene(0.0);
        let frame = tiles(&mut rasterizer(), &list, &v);
        assert_eq!(frame.tiles.len(), 3 * 2);
        let a = read_back(&frame);
        let b = headless(&list, &v);
        assert_eq!((a.width, a.height), (b.width, b.height));
        assert_same(&a, &b);
    }
}

#[test]
fn a_small_frame_has_one_small_tile() {
    let v = view(100, 40, 1.0);
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(10.0, 10.0, 20.0, 20.0), RED);
    let frame = tiles(&mut rasterizer(), &b.build(), &v);
    assert_eq!(frame.tiles.len(), 1);
    assert_eq!(frame.tiles[0].size(), 128);
    let p = read_back(&frame);
    assert_eq!(p.pixel(15, 15), RED);
    assert_eq!(p.pixel(50, 15), WHITE);
}

#[test]
fn unchanged_tiles_are_copied() {
    let v = view(1100, 700, 1.0);
    let mut r = rasterizer();
    let first = tiles(&mut r, &scene(0.0), &v);
    assert_eq!(
        r.stats(),
        RasterStats {
            painted: 6,
            reused: 0
        }
    );
    // The same list again: every tile is copied from the frame before.
    let second = tiles(&mut r, &scene(0.0), &v);
    assert_eq!(
        r.stats(),
        RasterStats {
            painted: 0,
            reused: 6
        }
    );
    r.recycle(first);
    // The red bar moves within the first column of tiles: only the two tiles it is in change
    // (it spans both rows), and the frame shows what a fresh raster does.
    let moved = scene(-200.0);
    let third = tiles(&mut r, &moved, &v);
    assert_eq!(
        r.stats(),
        RasterStats {
            painted: 4,
            reused: 2
        }
    );
    assert_same(&read_back(&third), &headless(&moved, &v));
    r.recycle(second);
    r.recycle(third);
}

#[test]
fn a_sparse_region_leaves_out_empty_tiles() {
    let v = Viewport {
        background: Color::TRANSPARENT,
        ..view(1100, 700, 1.0)
    };
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(600.0, 600.0, 20.0, 20.0), RED);
    let text = TextEngine::new().unwrap();
    let images = ImageStore::default();
    let frame = rasterizer()
        .raster_region(
            &b.build(),
            Sources {
                text: &text,
                images: &images,
            },
            &v,
            (0, 0),
            true,
        )
        .unwrap();
    assert_eq!(frame.tiles.len(), 1);
    assert_eq!((frame.tiles[0].x, frame.tiles[0].y), (512, 512));
}

#[test]
fn a_backdrop_filter_reads_across_tile_edges() {
    let v = view(1100, 700, 1.0);
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(480.0, 0.0, 64.0, 700.0), RED);
    b.backdrop_filter(
        &[FilterOp::Blur(8.0)],
        Rect::new(400.0, 400.0, 300.0, 200.0),
        Radii::default(),
        0,
    );
    let list = b.build();
    let a = read_back(&tiles(&mut rasterizer(), &list, &v));
    assert_same(&a, &headless(&list, &v));
}
