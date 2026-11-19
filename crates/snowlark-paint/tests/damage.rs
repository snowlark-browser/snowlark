// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a raster paints again: only the tiles whose content changed. A box that moves by a
//! transform, or a clip that moves with it, changes the tiles it left and entered, not the others
//! (a seek bar's thumb dragged across a page repaints the tiles around the thumb).

use snowlark_paint::*;

const SIZE: u32 = 1024;

/// A page of four 512 px tiles: a static box in each, and a thumb in the top-left tile, moved by a
/// transform to `x` and clipped by a rounded clip that moves with it.
fn page(x: f32) -> DisplayList {
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, SIZE as f32, SIZE as f32), Color::WHITE);
    for (tx, ty) in [(0.0, 0.0), (512.0, 0.0), (0.0, 512.0), (512.0, 512.0)] {
        b.fill_rect(
            Rect::new(tx + 300.0, ty + 300.0, 100.0, 100.0),
            Color::rgb(0, 0, 200),
        );
    }
    b.push_transform(Matrix::translation(x, 0.0));
    b.push_clip_rounded_rect(Rect::new(0.0, 100.0, 40.0, 40.0), Radii::uniform(20.0));
    b.fill_rect(Rect::new(0.0, 100.0, 40.0, 40.0), Color::rgb(220, 0, 0));
    b.pop_clip();
    b.pop_transform();
    b.build()
}

#[test]
fn a_box_moved_by_a_transform_repaints_its_tiles_only() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let view = Viewport {
        width: SIZE,
        height: SIZE,
        scale: 1.0,
        background: Color::WHITE,
    };
    let first = tiles.raster(&page(10.0), sources, &view).unwrap();
    assert_eq!(tiles.stats().painted, 4);
    // The thumb moves within the top-left tile: that tile only.
    let second = tiles.raster(&page(200.0), sources, &view).unwrap();
    assert_eq!(
        tiles.stats(),
        RasterStats {
            painted: 1,
            reused: 3
        }
    );
    let px = tiles.read_back(&second).unwrap();
    assert_eq!(
        px.pixel(220, 120),
        Color::rgb(220, 0, 0),
        "the thumb where it went"
    );
    assert_eq!(px.pixel(30, 120), Color::WHITE, "nothing where it was");
    assert_eq!(
        px.pixel(512 + 350, 350),
        Color::rgb(0, 0, 200),
        "a reused tile"
    );
    tiles.recycle(first);
    // Into the top-right tile: the tile it left and the tile it entered.
    let third = tiles.raster(&page(600.0), sources, &view).unwrap();
    assert_eq!(
        tiles.stats(),
        RasterStats {
            painted: 2,
            reused: 2
        }
    );
    tiles.recycle(second);
    tiles.recycle(third);
}
