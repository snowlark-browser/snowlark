// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pattern fills (SVG 2 §14.3): a tile's drawing repeated in both directions through its
//! transform, clipped to the tile, the same headless and in GPU tiles, at any display scale.

use snowlark_paint::*;

const W: u32 = 200;
const H: u32 = 120;
const RED: Color = Color::rgb(255, 0, 0);
const BLUE: Color = Color::rgb(0, 0, 255);

fn close(a: Color, b: Color, tolerance: i32) -> bool {
    let d = |x: u8, y: u8| (x as i32 - y as i32).abs() <= tolerance;
    d(a.r, b.r) && d(a.g, b.g) && d(a.b, b.b) && d(a.a, b.a)
}

/// A 10 by 10 tile: red in its top left quarter, blue in its bottom right one, and a blue
/// square drawn past its right edge, which the tile clips.
fn tile() -> DisplayList {
    let mut t = DisplayListBuilder::new();
    t.fill_rect(Rect::new(0.0, 0.0, 5.0, 5.0), RED);
    t.fill_rect(Rect::new(5.0, 5.0, 5.0, 5.0), BLUE);
    t.fill_rect(Rect::new(8.0, 0.0, 30.0, 2.0), BLUE);
    t.build()
}

fn render(list: &DisplayList, scale: f32) -> [Pixmap; 2] {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut headless = HeadlessRenderer::new().unwrap();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let view = Viewport {
        width: (W as f32 * scale) as u32,
        height: (H as f32 * scale) as u32,
        scale,
        background: Color::WHITE,
    };
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let a = headless.render(list, sources, &view).unwrap();
    let frame = tiles.raster(list, sources, &view).unwrap();
    [a, tiles.read_back(&frame).unwrap()]
}

/// The color at CSS px (`x`, `y`) of each rendering.
fn check(list: &DisplayList, scale: f32, probes: &[(f32, f32, Color)]) {
    for (what, pm) in ["headless", "tiles"].into_iter().zip(render(list, scale)) {
        for &(x, y, expected) in probes {
            let (px, py) = ((x * scale) as u32, (y * scale) as u32);
            let got = pm.pixel(px, py);
            assert!(
                close(got, expected, 8),
                "{what} at {scale}: ({x}, {y}) is {got:?}, expected {expected:?}"
            );
        }
    }
}

#[test]
fn a_tile_repeats_both_ways_clipped_to_itself() {
    let mut b = DisplayListBuilder::new();
    b.fill_pattern(
        Rect::new(0.0, 0.0, 100.0, 100.0),
        Size::new(10.0, 10.0),
        Matrix::IDENTITY,
        tile(),
    );
    let list = b.build();
    for scale in [1.0, 1.5, 2.0] {
        check(
            &list,
            scale,
            &[
                (2.5, 2.5, RED),
                (7.5, 7.5, BLUE),
                (7.5, 3.0, Color::WHITE),
                (52.5, 32.5, RED),
                (97.5, 97.5, BLUE),
                (92.5, 87.5, Color::WHITE),
                // (the square past the tile's edge shows only inside it)
                (9.0, 1.0, BLUE),
                (12.0, 1.0, RED),
                (15.5, 1.0, Color::WHITE),
                // (nothing outside the filled area)
                (150.0, 50.0, Color::WHITE),
            ],
        );
    }
}

#[test]
fn the_transform_moves_and_scales_the_tiles() {
    let mut b = DisplayListBuilder::new();
    // (tiles twice as large, starting at (3, 4))
    b.fill_pattern(
        Rect::new(0.0, 0.0, 120.0, 100.0),
        Size::new(10.0, 10.0),
        Matrix::scale(2.0, 2.0).then(&Matrix::translation(3.0, 4.0)),
        tile(),
    );
    let list = b.build();
    for scale in [1.0, 1.5] {
        check(
            &list,
            scale,
            &[
                (3.0 + 5.0, 4.0 + 5.0, RED),
                (3.0 + 15.0, 4.0 + 15.0, BLUE),
                (3.0 + 15.0, 4.0 + 5.0, Color::WHITE),
                // (the tile before the first: the pattern covers the whole area)
                (1.0, 2.0, BLUE),
                (3.0 + 60.0 + 5.0, 4.0 + 40.0 + 5.0, RED),
            ],
        );
    }
}

/// A pattern under a transform of the list (a rotated shape's fill) turns with it.
#[test]
fn a_rotated_list_turns_its_pattern() {
    let mut b = DisplayListBuilder::new();
    b.push_transform(
        Matrix::rotation(std::f32::consts::FRAC_PI_2).then(&Matrix::translation(100.0, 0.0)),
    );
    b.fill_pattern(
        Rect::new(0.0, 0.0, 100.0, 100.0),
        Size::new(10.0, 10.0),
        Matrix::IDENTITY,
        tile(),
    );
    b.pop_transform();
    let list = b.build();
    // (local (x, y) shows at (100 - y, x))
    check(
        &list,
        1.0,
        &[
            (100.0 - 2.5, 2.5, RED),
            (100.0 - 7.5, 7.5, BLUE),
            (100.0 - 2.5, 7.5, Color::WHITE),
        ],
    );
}
