// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Blurred box shadows (CSS Backgrounds 3 §7.1) rastered headless and on GPU tiles at display
//! scales other than 1: a shadow is the shape blurred by a Gaussian of half the blur radius, so its
//! edge fades as the Gaussian's integral does, whatever the scale.

use snowlark_paint::*;

const W: u32 = 200;
const H: u32 = 140;
const BG: Color = Color::rgb(200, 200, 200);
/// The card: a shadow 3 px down with a 12 px blur (a deviation of 6 px), as the tab hover card's.
const CARD: Rect = Rect::new(40.0, 40.0, 120.0, 60.0);
const SIGMA: f32 = 6.0;
const ALPHA: u8 = 130;

fn page() -> DisplayList {
    let mut b = DisplayListBuilder::new();
    b.box_shadow(BoxShadow {
        rect: CARD,
        radii: Radii::uniform(8.0),
        offset: Point::new(0.0, 3.0),
        blur: 12.0,
        spread: 0.0,
        color: Color::rgba(0, 0, 0, ALPHA),
        inset: false,
    });
    b.fill_rounded_rect(CARD, Radii::uniform(8.0), Color::rgb(255, 255, 255));
    b.build()
}

/// The standard normal distribution's tail beyond `z` (Abramowitz and Stegun 7.1.26).
fn tail(z: f32) -> f32 {
    let x = z.abs() / std::f32::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let erf = 1.0
        - ((((1.061_405_4 * t - 1.453_152) * t + 1.421_413_8) * t - 0.284_496_74) * t
            + 0.254_829_6)
            * t
            * (-x * x).exp();
    let erf = if z >= 0.0 { erf } else { -erf };
    0.5 * (1.0 - erf)
}

/// The gray right of the card at CSS x `x` (mid-height, where only the right edge matters).
fn expected(x: f32) -> f32 {
    let coverage = tail((x - CARD.right()) / SIGMA);
    BG.r as f32 * (1.0 - ALPHA as f32 / 255.0 * coverage)
}

#[test]
fn a_blurred_shadow_fades_at_every_scale() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let mut headless = HeadlessRenderer::new().unwrap();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let mut wrong = Vec::new();
    for scale in [1.0f32, 1.25, 1.5, 2.0] {
        let view = Viewport {
            width: (W as f32 * scale) as u32,
            height: (H as f32 * scale) as u32,
            scale,
            background: BG,
        };
        let list = page();
        let a = headless.render(&list, sources, &view).unwrap();
        let frame = tiles.raster(&list, sources, &view).unwrap();
        let b = tiles.read_back(&frame).unwrap();
        for (what, pm) in [("headless", &a), ("tiles", &b)] {
            let y = (70.0 * scale) as u32;
            for x in [161.0f32, 163.0, 166.0, 170.0, 176.0, 182.0] {
                let px = (x * scale) as u32;
                // (the pixel's center in CSS px)
                let want = expected((px as f32 + 0.5) / scale);
                let got = pm.pixel(px, y).r as f32;
                if (got - want).abs() > 8.0 {
                    wrong.push(format!(
                        "{what} at {scale}x: CSS x {x} is {got}, expected {want:.0}"
                    ));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The shadow of a box far taller than the page shown (a long column, a page-high wrapper) is
/// blurred where it is shown, at full resolution: its edge fades as a small box's does. (Blurred
/// whole, it needs an intermediate past the largest a device makes, so it was made smaller and the
/// edge came out blocky.)
#[test]
fn a_shadow_taller_than_the_page_fades_as_a_small_one_does() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let tall = Rect::new(CARD.x, -50_000.0, CARD.width, 100_000.0);
    let mut b = DisplayListBuilder::new();
    b.box_shadow(BoxShadow {
        rect: tall,
        radii: Radii::ZERO,
        offset: Point::new(0.0, 3.0),
        blur: 12.0,
        spread: 0.0,
        color: Color::rgba(0, 0, 0, ALPHA),
        inset: false,
    });
    b.fill_rect(tall, Color::rgb(255, 255, 255));
    let list = b.build();
    let mut headless = HeadlessRenderer::new().unwrap();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let mut wrong = Vec::new();
    for scale in [1.0f32, 1.5] {
        let view = Viewport {
            width: (W as f32 * scale) as u32,
            height: (H as f32 * scale) as u32,
            scale,
            background: BG,
        };
        let a = headless.render(&list, sources, &view).unwrap();
        let frame = tiles.raster(&list, sources, &view).unwrap();
        let t = tiles.read_back(&frame).unwrap();
        for (what, pm) in [("headless", &a), ("tiles", &t)] {
            for y in [5.0f32, 70.0, 135.0] {
                for x in [161.0f32, 163.0, 166.0, 170.0, 176.0, 182.0] {
                    let px = (x * scale) as u32;
                    let want = expected((px as f32 + 0.5) / scale);
                    let got = pm.pixel(px, (y * scale) as u32).r as f32;
                    if (got - want).abs() > 8.0 {
                        wrong.push(format!(
                            "{what} at {scale}x: CSS ({x}, {y}) is {got}, expected {want:.0}"
                        ));
                    }
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// An inset shadow is cast by the edges of a hole: inside the box, at distance d from an edge it
/// covers as much as the Gaussian's tail beyond d.
#[test]
fn an_inset_shadow_is_as_dark_at_the_edge_as_its_blur_says() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let mut b = DisplayListBuilder::new();
    b.fill_rect(CARD, Color::rgb(255, 255, 255));
    b.box_shadow(BoxShadow {
        rect: CARD,
        radii: Radii::ZERO,
        offset: Point::new(0.0, 0.0),
        blur: 16.0,
        spread: 0.0,
        color: Color::rgba(0, 0, 0, 153),
        inset: true,
    });
    let list = b.build();
    let mut headless = HeadlessRenderer::new().unwrap();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let mut wrong = Vec::new();
    for scale in [1.0f32, 1.5] {
        let view = Viewport {
            width: (W as f32 * scale) as u32,
            height: (H as f32 * scale) as u32,
            scale,
            background: BG,
        };
        let a = headless.render(&list, sources, &view).unwrap();
        let frame = tiles.raster(&list, sources, &view).unwrap();
        let t = tiles.read_back(&frame).unwrap();
        for (what, pm) in [("headless", &a), ("tiles", &t)] {
            let y = (70.0 * scale) as u32;
            for x in [40.5f32, 42.0, 45.0, 50.0, 60.0] {
                let px = (x * scale) as u32;
                let d = (px as f32 + 0.5) / scale - CARD.x;
                let want = 255.0 * (1.0 - 153.0 / 255.0 * tail(d / 8.0));
                let got = pm.pixel(px, y).r as f32;
                if (got - want).abs() > 8.0 {
                    wrong.push(format!(
                        "{what} at {scale}x: CSS x {x} is {got}, expected {want:.0}"
                    ));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
