// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Backdrop filters (Filter Effects 2 §3) rastered headless and on GPU tiles: what is under the
//! border box filtered and clipped to its rounded corners, read from the backdrop root only, the
//! element's own groups applying to it, mirrored edges, and a backdrop in a later tile.

use snowlark_paint::*;

const W: u32 = 120;
const H: u32 = 120;
const RED: Color = Color::rgb(255, 0, 0);
const BLUE: Color = Color::rgb(0, 0, 255);
const CYAN: Color = Color::rgb(0, 255, 255);
const YELLOW: Color = Color::rgb(255, 255, 0);

fn close(a: Color, b: Color, tolerance: i32) -> bool {
    (a.r as i32 - b.r as i32).abs() <= tolerance
        && (a.g as i32 - b.g as i32).abs() <= tolerance
        && (a.b as i32 - b.b as i32).abs() <= tolerance
        && (a.a as i32 - b.a as i32).abs() <= tolerance
}

/// Red on the left half, blue on the right.
fn halves(b: &mut DisplayListBuilder) {
    b.fill_rect(Rect::new(0.0, 0.0, 60.0, H as f32), RED);
    b.fill_rect(Rect::new(60.0, 0.0, 60.0, H as f32), BLUE);
}

/// `list` rendered headless and in tiles.
fn render(list: &DisplayList) -> [Pixmap; 2] {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut headless = HeadlessRenderer::new().unwrap();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let view = Viewport {
        width: W,
        height: H,
        scale: 1.0,
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

fn check(list: &DisplayList, probes: &[(u32, u32, Color, i32)]) {
    for (what, pm) in ["headless", "tiles"].into_iter().zip(render(list)) {
        for &(x, y, expected, tolerance) in probes {
            let got = pm.pixel(x, y);
            assert!(
                close(got, expected, tolerance),
                "{what}: ({x}, {y}) is {got:?}, expected {expected:?}"
            );
        }
    }
}

#[test]
fn the_backdrop_under_the_border_box_is_filtered_within_its_corners() {
    let mut b = DisplayListBuilder::new();
    halves(&mut b);
    b.backdrop_filter(
        &[FilterOp::Invert(1.0)],
        Rect::new(20.0, 20.0, 80.0, 80.0),
        Radii::uniform(20.0),
        0,
    );
    // (the element's own background, half transparent, over the filtered backdrop)
    b.fill_rect(
        Rect::new(50.0, 50.0, 20.0, 20.0),
        Color::rgba(255, 255, 255, 128),
    );
    let list = b.build();
    check(
        &list,
        &[
            (40, 60, CYAN, 0),
            (80, 60, YELLOW, 0),
            // outside the box, and in its rounded corner, the page is as painted
            (10, 60, RED, 0),
            (110, 60, BLUE, 0),
            (21, 21, RED, 0),
            (98, 98, BLUE, 0),
            // the element paints over its backdrop
            (55, 60, Color::rgb(128, 255, 255), 2),
        ],
    );
}

#[test]
fn the_backdrop_is_read_from_the_backdrop_root() {
    // In an opacity group, which is a backdrop root (Filter Effects 2 §3.1), what was painted
    // before the group is not in the backdrop: the filter sees transparent pixels.
    let mut b = DisplayListBuilder::new();
    halves(&mut b);
    b.push_opacity(1.0, Rect::new(0.0, 0.0, W as f32, H as f32));
    b.backdrop_filter(
        &[FilterOp::Invert(1.0)],
        Rect::new(20.0, 20.0, 80.0, 80.0),
        Radii::ZERO,
        0,
    );
    b.pop_opacity();
    let list = b.build();
    check(&list, &[(40, 60, RED, 0), (80, 60, BLUE, 0)]);

    // The element's own opacity group is not its backdrop root, and applies to the backdrop.
    let mut b = DisplayListBuilder::new();
    halves(&mut b);
    b.push_opacity(0.5, Rect::new(20.0, 20.0, 80.0, 80.0));
    b.backdrop_filter(
        &[FilterOp::Invert(1.0)],
        Rect::new(20.0, 20.0, 80.0, 80.0),
        Radii::ZERO,
        1,
    );
    b.pop_opacity();
    let list = b.build();
    check(
        &list,
        &[
            (40, 60, Color::rgb(128, 128, 128), 2),
            (80, 60, Color::rgb(128, 128, 128), 2),
            (10, 60, RED, 0),
        ],
    );
}

#[test]
fn edges_are_mirrored_from_inside_the_border_box() {
    // A blurred, inverted backdrop over the red half only: with mirrored edges no blue beyond the
    // box's right edge bleeds in, and the edges don't fade (Filter Effects 2 §3: the backdrop's
    // edge mode is mirror), so it is cyan to its edges.
    let mut b = DisplayListBuilder::new();
    halves(&mut b);
    b.backdrop_filter(
        &[FilterOp::Blur(6.0), FilterOp::Invert(1.0)],
        Rect::new(10.0, 10.0, 50.0, 100.0),
        Radii::ZERO,
        0,
    );
    let list = b.build();
    check(
        &list,
        &[(59, 60, CYAN, 2), (11, 60, CYAN, 2), (61, 60, BLUE, 0)],
    );
}

#[test]
fn a_backdrop_in_a_later_tile_reads_what_is_under_it_there() {
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, 1100.0, 100.0), RED);
    b.fill_rect(Rect::new(620.0, 0.0, 480.0, 100.0), BLUE);
    b.backdrop_filter(
        &[FilterOp::Invert(1.0), FilterOp::Blur(2.0)],
        Rect::new(500.0, 20.0, 200.0, 60.0),
        Radii::ZERO,
        0,
    );
    let list = b.build();
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let view = Viewport {
        width: 1100,
        height: 100,
        scale: 1.0,
        background: Color::WHITE,
    };
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let frame = tiles.raster(&list, sources, &view).unwrap();
    assert!(frame.tiles.len() > 1);
    let pm = tiles.read_back(&frame).unwrap();
    // (on both sides of the tile edge at 512, and under the blue part)
    for x in [505, 510, 515, 600] {
        assert!(
            close(pm.pixel(x, 50), CYAN, 2),
            "{x}: {:?}",
            pm.pixel(x, 50)
        );
    }
    assert!(
        close(pm.pixel(690, 50), YELLOW, 2),
        "{:?}",
        pm.pixel(690, 50)
    );
    assert_eq!(pm.pixel(495, 50), RED);
}

#[test]
fn a_backdrop_read_from_the_root_holds_the_canvas_and_only_what_is_under_the_box() {
    // A blue square inside the box, the white canvas around it: the canvas is inverted too, and the
    // edges are mirrored from the box's rectangle, not from the square's.
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(50.0, 50.0, 20.0, 20.0), BLUE);
    b.backdrop_filter(
        &[FilterOp::Invert(1.0)],
        Rect::new(20.0, 20.0, 80.0, 80.0),
        Radii::ZERO,
        0,
    );
    let list = b.build();
    check(
        &list,
        &[
            (60, 60, YELLOW, 0),
            (30, 30, Color::BLACK, 0),
            (90, 60, Color::BLACK, 0),
            (10, 10, Color::WHITE, 0),
        ],
    );
}

/// `n` sibling backdrop filters that invert, stacked over the same box: each one's backdrop holds
/// what the ones before it drew (Filter Effects 2 §3: the backdrop is everything painted since the
/// backdrop root), so the box shows the page inverted `n` times.
fn stacked_inverts(n: usize, rect: Rect) -> DisplayList {
    let mut b = DisplayListBuilder::new();
    halves(&mut b);
    for _ in 0..n {
        b.backdrop_filter(&[FilterOp::Invert(1.0)], rect, Radii::ZERO, 0);
    }
    b.build()
}

#[test]
fn stacked_backdrops_each_read_what_the_ones_before_drew() {
    for n in 1..=8 {
        let list = stacked_inverts(n, Rect::new(20.0, 20.0, 80.0, 80.0));
        let (left, right) = if n % 2 == 0 {
            (RED, BLUE)
        } else {
            (CYAN, YELLOW)
        };
        for (what, pm) in ["headless", "tiles"].into_iter().zip(render(&list)) {
            for (x, expected) in [(40, left), (80, right), (10, RED)] {
                let got = pm.pixel(x, 60);
                assert!(
                    close(got, expected, 0),
                    "{n} stacked, {what}: ({x}, 60) is {got:?}, expected {expected:?}"
                );
            }
        }
    }
}

/// Stacked backdrop filters, as a header that blurs more towards its edge stacks them: each is
/// rendered once a frame, not once more inside each later one's backdrop, nor once per tile it is
/// in (at a display scale, where a tile's pixels are offset from the frame's).
#[test]
fn stacked_backdrops_are_rendered_once_a_frame() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let view = Viewport {
        width: 1024,
        height: 300,
        scale: 1.5,
        background: Color::WHITE,
    };
    // (the box spans the tile edge at device pixel 512)
    let rect = Rect::new(300.0, 20.0, 100.0, 140.0);
    let stack = |ops: &dyn Fn(usize) -> FilterOp, n: usize| {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 360.0, 200.0), RED);
        b.fill_rect(Rect::new(360.0, 0.0, 400.0, 200.0), BLUE);
        for i in 0..n {
            b.backdrop_filter(&[ops(i)], rect, Radii::ZERO, 0);
        }
        b.build()
    };
    let list = stack(&|i| FilterOp::Blur(1.0 + i as f32), 12);
    let frame = tiles.raster(&list, sources, &view).unwrap();
    assert!(frame.tiles.len() > 1);
    assert_eq!(tiles.backdrops_made(), 12);
    tiles.recycle(frame);

    // What the tiles show of one result agrees on both sides of their edge: five inverts.
    let list = stack(&|_| FilterOp::Invert(1.0), 5);
    let made = tiles.backdrops_made();
    let frame = tiles.raster(&list, sources, &view).unwrap();
    assert_eq!(tiles.backdrops_made() - made, 5);
    let pm = tiles.read_back(&frame).unwrap();
    // (red up to device pixel 540, blue beyond)
    for (x, expected) in [(460, CYAN), (505, CYAN), (520, CYAN), (590, YELLOW)] {
        assert_eq!(pm.pixel(x, 100), expected, "{x}");
    }
    assert_eq!(pm.pixel(440, 100), RED);
}

/// An opacity group at 1 draws no layer of its own (it is a backdrop root and nothing more), so a
/// `plus-lighter` sum (a cross-fade's) isolates itself: two half-transparent reds add up to red
/// before they are composited over the blue page, once.
#[test]
fn a_plus_lighter_sum_is_composited_over_the_page_once() {
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, W as f32, H as f32), BLUE);
    b.push_opacity(1.0, Rect::new(0.0, 0.0, W as f32, H as f32));
    b.push_plus();
    b.fill_rect(
        Rect::new(20.0, 20.0, 40.0, 40.0),
        Color::rgba(255, 0, 0, 128),
    );
    b.fill_rect(
        Rect::new(20.0, 20.0, 40.0, 40.0),
        Color::rgba(255, 0, 0, 127),
    );
    b.pop_plus();
    b.pop_opacity();
    let list = b.build();
    check(&list, &[(40, 40, RED, 1), (80, 80, BLUE, 0)]);
}

/// Damage: a backdrop filter shows what is painted under it, also where that is in another tile.
/// A change under its border box, in the tile next to it, repaints the tile the backdrop is in.
#[test]
fn a_change_under_a_backdrop_in_another_tile_repaints_it() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let view = Viewport {
        width: 1024,
        height: 512,
        scale: 1.0,
        background: Color::WHITE,
    };
    // (the backdrop's box spans both tiles; the square that changes is in the right one, and its
    // blur reaches the left one)
    let list = |square: Color| {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(520.0, 100.0, 40.0, 40.0), square);
        b.backdrop_filter(
            &[FilterOp::Blur(10.0)],
            Rect::new(400.0, 50.0, 300.0, 150.0),
            Radii::ZERO,
            0,
        );
        b.build()
    };
    let first = tiles.raster(&list(RED), sources, &view).unwrap();
    assert_eq!(tiles.stats().painted, 2);
    let second = tiles.raster(&list(BLUE), sources, &view).unwrap();
    assert_eq!((tiles.stats().painted, tiles.stats().reused), (2, 0));
    let pm = tiles.read_back(&second).unwrap();
    let edge = pm.pixel(508, 120);
    assert!(edge.b > edge.r, "{edge:?}");
    tiles.recycle(first);
}
