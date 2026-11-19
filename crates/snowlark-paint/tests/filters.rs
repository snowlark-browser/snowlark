// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Filter groups (Filter Effects 1 §10) rastered headless and on GPU tiles: each function's effect
//! on known pixels, a blur that reaches past its content, a drop shadow where its offset puts it,
//! and a list applied in order.

use snowlark_paint::*;

const W: u32 = 120;
const H: u32 = 120;
/// The page's background: gray, so that a transparent part of a group that came out white (as a
/// color matrix with an offset on straight alpha does) shows.
const BG: Color = Color::rgb(200, 200, 200);

fn close(a: Color, b: Color, tolerance: i32) -> bool {
    (a.r as i32 - b.r as i32).abs() <= tolerance
        && (a.g as i32 - b.g as i32).abs() <= tolerance
        && (a.b as i32 - b.b as i32).abs() <= tolerance
        && (a.a as i32 - b.a as i32).abs() <= tolerance
}

/// A red 40 px square at (40, 40) on gray, filtered by `ops` in a group whose bounds reach 10 px
/// around it (transparent there), with a blue square after the group.
fn page(ops: &[FilterOp]) -> DisplayList {
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, W as f32, H as f32), BG);
    let content = Rect::new(40.0, 40.0, 40.0, 40.0);
    b.push_filter(ops, filter::map_rect(ops, content.inflate(10.0, 10.0)));
    b.fill_rect(content, Color::rgb(255, 0, 0));
    b.pop_filter();
    b.fill_rect(Rect::new(100.0, 100.0, 20.0, 20.0), Color::rgb(0, 0, 255));
    b.build()
}

struct Rig {
    text: TextEngine,
    images: ImageStore,
    headless: HeadlessRenderer,
    tiles: TileRasterizer,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            text: TextEngine::new().unwrap(),
            images: ImageStore::new(),
            headless: HeadlessRenderer::new().unwrap(),
            tiles: TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap(),
        }
    }

    fn view(&self, scale: f32) -> Viewport {
        Viewport {
            width: (W as f32 * scale) as u32,
            height: (H as f32 * scale) as u32,
            scale,
            background: BG,
        }
    }

    /// The page rendered headless and in tiles; both are checked.
    fn render(&mut self, ops: &[FilterOp], scale: f32) -> [Pixmap; 2] {
        let list = page(ops);
        let sources = Sources {
            text: &self.text,
            images: &self.images,
        };
        let a = self
            .headless
            .render(&list, sources, &self.view(scale))
            .unwrap();
        let frame = self
            .tiles
            .raster(&list, sources, &self.view(scale))
            .unwrap();
        let b = self.tiles.read_back(&frame).unwrap();
        [a, b]
    }
}

fn check(rig: &mut Rig, ops: &[FilterOp], probes: &[(u32, u32, Color, i32)]) {
    for (what, pm) in ["headless", "tiles"].into_iter().zip(rig.render(ops, 1.0)) {
        for &(x, y, expected, tolerance) in probes {
            let got = pm.pixel(x, y);
            assert!(
                close(got, expected, tolerance),
                "{what} {ops:?}: ({x}, {y}) is {got:?}, expected {expected:?}"
            );
        }
    }
}

#[test]
fn color_functions_change_the_content_and_nothing_else() {
    let mut rig = Rig::new();
    let red = Color::rgb(255, 0, 0);
    // (the probe inside the square, the white beside it, the blue square after the group)
    // (the probe inside the square, the group's transparent part beside it and the background
    // beyond the group, the blue square after the group)
    let probes = |inside: Color| {
        [
            (60, 60, inside, 3),
            (35, 60, BG, 0),
            (20, 60, BG, 0),
            (110, 110, Color::rgb(0, 0, 255), 0),
        ]
    };
    check(
        &mut rig,
        &[FilterOp::Grayscale(1.0)],
        &probes(Color::rgb(54, 54, 54)),
    );
    check(&mut rig, &[FilterOp::Grayscale(0.0)], &probes(red));
    check(
        &mut rig,
        &[FilterOp::Sepia(1.0)],
        &probes(Color::rgb(100, 89, 69)),
    );
    check(
        &mut rig,
        &[FilterOp::Invert(1.0)],
        &probes(Color::rgb(0, 255, 255)),
    );
    check(
        &mut rig,
        &[FilterOp::Brightness(0.5)],
        &probes(Color::rgb(128, 0, 0)),
    );
    check(
        &mut rig,
        &[FilterOp::Contrast(0.0)],
        &probes(Color::rgb(128, 128, 128)),
    );
    check(
        &mut rig,
        &[FilterOp::Saturate(0.0)],
        &probes(Color::rgb(54, 54, 54)),
    );
    check(
        &mut rig,
        &[FilterOp::HueRotate(180.0)],
        &probes(Color::rgb(0, 109, 109)),
    );
    // opacity(0.5): red at half over the gray
    check(
        &mut rig,
        &[FilterOp::Opacity(0.5)],
        &probes(Color::rgb(228, 100, 100)),
    );
    // in order: inverted then half bright is dark cyan; half bright then inverted is light cyan
    check(
        &mut rig,
        &[FilterOp::Invert(1.0), FilterOp::Brightness(0.5)],
        &probes(Color::rgb(0, 128, 128)),
    );
    check(
        &mut rig,
        &[FilterOp::Brightness(0.5), FilterOp::Invert(1.0)],
        &probes(Color::rgb(128, 255, 255)),
    );
}

#[test]
fn blur_reaches_past_the_content_and_fades_it_at_the_edge() {
    let mut rig = Rig::new();
    for (what, pm) in ["headless", "tiles"]
        .into_iter()
        .zip(rig.render(&[FilterOp::Blur(4.0)], 1.0))
    {
        // the center is still red, the edge half red, and 6 px outside a tint remains
        assert!(
            close(pm.pixel(60, 60), Color::rgb(255, 0, 0), 2),
            "{what} center"
        );
        let edge = pm.pixel(40, 60);
        assert!(
            edge.r > 215 && (70..130).contains(&(edge.g as i32)),
            "{what} edge {edge:?}"
        );
        let outside = pm.pixel(34, 60);
        assert!(
            outside.r >= 200 && outside.g < 198 && outside.g > 150,
            "{what} outside {outside:?}"
        );
        // far outside it is white, and the blue square after the group is untouched
        assert_eq!(pm.pixel(20, 60), BG, "{what} far");
        assert_eq!(pm.pixel(110, 110), Color::rgb(0, 0, 255), "{what} after");
    }
    // At scale 2 the blur covers twice the device pixels: the same CSS pixel looks the same.
    let [two, _] = rig.render(&[FilterOp::Blur(4.0)], 2.0);
    let [one, _] = rig.render(&[FilterOp::Blur(4.0)], 1.0);
    assert!(
        close(two.pixel(68, 120), one.pixel(34, 60), 6),
        "scale 2 {:?} against {:?}",
        two.pixel(68, 120),
        one.pixel(34, 60)
    );
}

#[test]
fn drop_shadow_sits_at_its_offset_under_the_content() {
    let mut rig = Rig::new();
    let shadow = FilterOp::DropShadow {
        offset: Point::new(20.0, 0.0),
        sigma: 0.0,
        color: Color::rgba(0, 0, 255, 128),
    };
    check(
        &mut rig,
        &[shadow],
        &[
            // the content is on top
            (60, 60, Color::rgb(255, 0, 0), 0),
            // the shadow shows to its right, half blue over the gray
            (90, 60, Color::rgb(100, 100, 228), 2),
            // and nowhere else
            (20, 60, BG, 0),
            (60, 30, BG, 0),
        ],
    );
    // A blurred shadow spreads past its offset box; the content is unchanged.
    let soft = FilterOp::DropShadow {
        offset: Point::new(0.0, 20.0),
        sigma: 4.0,
        color: Color::BLACK,
    };
    for (what, pm) in ["headless", "tiles"]
        .into_iter()
        .zip(rig.render(&[soft], 1.0))
    {
        assert_eq!(pm.pixel(60, 60), Color::rgb(255, 0, 0), "{what} content");
        // (the shadow's box is 60..100 below the content: dark well inside it, a fringe past it)
        let below = pm.pixel(60, 92);
        assert!(below.r < 60 && below.g < 60, "{what} below {below:?}");
        let fringe = pm.pixel(60, 103);
        assert!(fringe.r < 195 && fringe.r > 60, "{what} fringe {fringe:?}");
        assert_eq!(pm.pixel(60, 118), BG, "{what} past the fringe");
    }
}

/// A tile's cull area is in frame device pixels: a group in the second tile (its painter's
/// transform carries the tile's offset) is drawn there too.
#[test]
fn a_group_in_a_later_tile_is_drawn() {
    let rig = Rig::new();
    let mut tiles = rig.tiles;
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, 1100.0, 100.0), BG);
    let content = Rect::new(600.0, 30.0, 40.0, 40.0);
    b.push_filter(&[FilterOp::Invert(1.0)], content);
    b.fill_rect(content, Color::rgb(255, 0, 0));
    b.pop_filter();
    let list = b.build();
    let view = Viewport {
        width: 1100,
        height: 100,
        scale: 1.0,
        background: BG,
    };
    let sources = Sources {
        text: &rig.text,
        images: &rig.images,
    };
    let frame = tiles.raster(&list, sources, &view).unwrap();
    assert_eq!(frame.tiles.len(), 3);
    let pm = tiles.read_back(&frame).unwrap();
    assert_eq!(pm.pixel(620, 50), Color::rgb(0, 255, 255));
    assert_eq!(pm.pixel(590, 50), BG);
}

#[test]
fn an_empty_list_and_a_group_outside_the_tile_cost_nothing_visible() {
    let mut rig = Rig::new();
    check(
        &mut rig,
        &[],
        &[(60, 60, Color::rgb(255, 0, 0), 0), (20, 60, BG, 0)],
    );
}
