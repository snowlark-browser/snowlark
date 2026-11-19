// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Many boxes with the same gradient (CSS Images 3 §3.1, a repeated background's tiles, a list's
//! rows): the rasterizer makes the gradient's stops once and draws every box from them, in every
//! tile, and each box still shows its own gradient.

use snowlark_paint::*;

const W: u32 = 600;
const H: u32 = 600;
const RED: Color = Color::rgb(255, 0, 0);
const BLUE: Color = Color::rgb(0, 0, 255);
const GREEN: Color = Color::rgb(0, 160, 0);

fn stops(a: Color, b: Color) -> [GradientStop; 2] {
    [
        GradientStop {
            offset: 0.0,
            color: a,
        },
        GradientStop {
            offset: 1.0,
            color: b,
        },
    ]
}

/// 400 boxes of 20x20 (red to blue, left to right) over 600x600, so over four tiles' worth of
/// 512 px, and one box of another gradient (green to a lighter green) at the end.
fn page() -> DisplayList {
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, W as f32, H as f32), Color::WHITE);
    for row in 0..20 {
        for col in 0..20 {
            let (x, y) = (col as f32 * 30.0, row as f32 * 30.0);
            b.linear_gradient(
                Rect::new(x, y, 20.0, 20.0),
                Radii::default(),
                Point::new(x, y),
                Point::new(x + 20.0, y),
                &stops(RED, BLUE),
                Extend::Clamp,
            );
        }
    }
    b.linear_gradient(
        Rect::new(585.0, 585.0, 10.0, 10.0),
        Radii::default(),
        Point::new(585.0, 585.0),
        Point::new(595.0, 585.0),
        &stops(GREEN, Color::rgb(0, 170, 0)),
        Extend::Clamp,
    );
    b.build()
}

#[test]
fn boxes_of_one_gradient_share_its_stops() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let view = Viewport {
        width: W,
        height: H,
        scale: 1.0,
        background: Color::WHITE,
    };
    let list = page();
    let frame = tiles.raster(&list, sources, &view).unwrap();
    let px = tiles.read_back(&frame).unwrap();
    // (two gradients: the 400 boxes' and the last box's)
    let made = tiles.gradient_stops_made();
    assert!(made <= 2, "{made} stop collections for 2 gradients");
    // Each box still shows its own gradient: red at its left, blue at its right, in every tile.
    let at = |x: u32, y: u32| px.pixel(x, y);
    for (x, y) in [(0u32, 0u32), (540, 0), (0, 540), (540, 540), (270, 270)] {
        let left = at(x + 1, y + 10);
        let right = at(x + 18, y + 10);
        assert!(
            left.r > 200 && left.b < 60,
            "left of the box at {x},{y}: {left:?}"
        );
        assert!(
            right.b > 200 && right.r < 60,
            "right of the box at {x},{y}: {right:?}"
        );
    }
    let last = at(590, 590);
    assert!(
        last.g > 120 && last.r < 40 && last.b < 40,
        "the other gradient: {last:?}"
    );
    tiles.recycle(frame);
    // A second raster of the same list makes none.
    let frame = tiles.raster(&list, sources, &view).unwrap();
    assert_eq!(tiles.gradient_stops_made(), made);
    tiles.recycle(frame);
}
