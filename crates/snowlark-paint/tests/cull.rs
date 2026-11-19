// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a tile draws: the items that can paint in it, and the groups around them only where
//! their content does. A clip, transform or opacity group whose content misses a tile is skipped
//! there whole, its push and pop too (a rounded clip is a Direct2D layer: pushing one for content
//! another tile shows cost as much as drawing it), and a group of opacity 0 is not drawn at all.

use snowlark_paint::*;

const RED: Color = Color::rgb(220, 0, 0);

fn sources_for<'a>(text: &'a TextEngine, images: &'a ImageStore) -> Sources<'a> {
    Sources { text, images }
}

fn view(width: u32, height: u32) -> Viewport {
    Viewport {
        width,
        height,
        scale: 1.0,
        background: Color::WHITE,
    }
}

#[test]
fn a_group_whose_content_misses_a_tile_is_not_drawn_in_it() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    // Two tiles side by side: the page's background in both, a rounded clip in a transform in the
    // right one only.
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, 1024.0, 512.0), Color::WHITE);
    b.push_transform(Matrix::translation(600.0, 0.0));
    b.push_clip_rounded_rect(Rect::new(0.0, 100.0, 100.0, 100.0), Radii::uniform(20.0));
    b.fill_rect(Rect::new(0.0, 100.0, 100.0, 100.0), RED);
    b.pop_clip();
    b.pop_transform();
    let list = b.build();
    let frame = tiles
        .raster(&list, sources_for(&text, &images), &view(1024, 512))
        .unwrap();
    // The left tile: the background. The right one: the background and the two groups with their
    // fill (pushes and pops count as items drawn).
    assert_eq!(tiles.work().items, 1 + 6);
    let px = tiles.read_back(&frame).unwrap();
    assert_eq!(px.pixel(650, 150), RED, "the clipped box");
    assert_eq!(px.pixel(300, 150), Color::WHITE, "the other tile");
    tiles.recycle(frame);
}

#[test]
fn a_group_is_drawn_in_every_tile_its_content_or_its_filter_reaches() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, 1024.0, 512.0), Color::WHITE);
    // A box across the tiles' edge, in a clip in a transform.
    b.push_transform(Matrix::translation(400.0, 0.0));
    b.push_clip_rect(Rect::new(0.0, 0.0, 300.0, 100.0));
    b.fill_rect(Rect::new(0.0, 0.0, 300.0, 100.0), RED);
    b.pop_clip();
    b.pop_transform();
    // A box in the right tile, blurred into the left one: its group's content is in one tile, its
    // filter's result in both.
    b.push_opacity(1.0, Rect::new(0.0, 0.0, 1024.0, 512.0));
    b.push_filter(
        &[FilterOp::Blur(8.0)],
        Rect::new(460.0, 260.0, 160.0, 160.0),
    );
    b.fill_rect(Rect::new(520.0, 300.0, 60.0, 80.0), Color::rgb(0, 0, 200));
    b.pop_filter();
    b.pop_opacity();
    let list = b.build();
    let frame = tiles
        .raster(&list, sources_for(&text, &images), &view(1024, 512))
        .unwrap();
    let px = tiles.read_back(&frame).unwrap();
    assert_eq!(px.pixel(450, 50), RED, "the box in the left tile");
    assert_eq!(px.pixel(650, 50), RED, "the box in the right tile");
    let blurred = px.pixel(510, 340);
    assert!(
        blurred.b > 0 && blurred.r < 255,
        "the blur reaches into the left tile: {blurred:?}"
    );
    tiles.recycle(frame);
}

#[test]
fn a_group_of_opacity_zero_draws_nothing() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, 512.0, 512.0), Color::WHITE);
    b.push_opacity(0.0, Rect::new(100.0, 100.0, 100.0, 100.0));
    b.push_clip_rounded_rect(Rect::new(100.0, 100.0, 100.0, 100.0), Radii::uniform(20.0));
    b.fill_rect(Rect::new(100.0, 100.0, 100.0, 100.0), RED);
    b.pop_clip();
    b.pop_opacity();
    let list = b.build();
    let frame = tiles
        .raster(&list, sources_for(&text, &images), &view(512, 512))
        .unwrap();
    assert_eq!(tiles.work().items, 1, "the background only");
    let px = tiles.read_back(&frame).unwrap();
    assert_eq!(px.pixel(150, 150), Color::WHITE);
    tiles.recycle(frame);
}
