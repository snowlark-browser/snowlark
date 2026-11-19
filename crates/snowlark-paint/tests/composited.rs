// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Composited layers (`docs/compositor-animations.md`, S1): a list split into pieces with their
//! states (`DisplayList::split_layers`), each piece rastered in its own coordinates and drawn through
//! its state as the compositor draws it (`TileRasterizer::compose`), looks as the list rastered whole
//! does; and a frame that only moves, turns or fades a composited box paints no tile. On GPU tiles
//! (WARP, so it runs without a graphics card).

use snowlark_paint::*;

const W: u32 = 400;
const H: u32 = 300;

/// A page with a composited box: a background, a border, a rounded box and a stroke inside the
/// box's `transform`, `opacity` and clip, and content painted after it.
fn page(transform: Matrix, opacity: f32, clip: Option<Rect>) -> DisplayList {
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, 400.0, 300.0), Color::hex(0xf0f0f0));
    b.fill_rect(Rect::new(20.0, 20.0, 120.0, 40.0), Color::hex(0x336699));
    if let Some(c) = clip {
        b.push_clip_rect(c);
    }
    b.push_transform(transform);
    if opacity < 1.0 {
        b.push_opacity(opacity, Rect::new(100.0, 80.0, 160.0, 160.0));
    }
    b.push_layer(LayerKind::Composited(42));
    b.fill_rounded_rect(
        Rect::new(110.0, 90.0, 140.0, 140.0),
        Radii::uniform(30.0),
        Color::hex(0xcc3333),
    );
    b.border(Border {
        rect: Rect::new(130.0, 110.0, 100.0, 100.0),
        widths: Sides {
            top: 6.0,
            right: 6.0,
            bottom: 6.0,
            left: 6.0,
        },
        colors: [Color::hex(0xdddddd); 4],
        styles: [BorderStyle::Solid; 4],
        radii: Radii::uniform(50.0),
    });
    b.fill_rect(Rect::new(176.0, 100.0, 8.0, 40.0), Color::BLACK);
    b.pop_layer();
    if opacity < 1.0 {
        b.pop_opacity();
    }
    b.pop_transform();
    if clip.is_some() {
        b.pop_clip();
    }
    b.fill_rect(Rect::new(300.0, 200.0, 60.0, 60.0), Color::hex(0x228822));
    b.build()
}

struct Rig {
    text: TextEngine,
    images: ImageStore,
    tiles: TileRasterizer,
    /// Another rasterizer for the lists rastered whole (its full frames would replace the split
    /// frames' kept tiles).
    whole: TileRasterizer,
    /// The frames shown: as the compositor does, they go back once the next frame is rastered.
    shown: Vec<TiledFrame>,
}

impl Rig {
    fn new() -> Rig {
        let gpu = Gpu::with_driver(Driver::Warp).unwrap();
        Rig {
            text: TextEngine::new().unwrap(),
            images: ImageStore::new(),
            tiles: TileRasterizer::new(gpu.clone()).unwrap(),
            whole: TileRasterizer::new(gpu).unwrap(),
            shown: Vec::new(),
        }
    }

    fn view(scale: f32) -> Viewport {
        Viewport {
            width: (W as f32 * scale) as u32,
            height: (H as f32 * scale) as u32,
            scale,
            background: Color::WHITE,
        }
    }

    /// The list rastered whole, read back.
    fn whole(&mut self, list: &DisplayList, scale: f32) -> Pixmap {
        let sources = Sources {
            text: &self.text,
            images: &self.images,
        };
        let frame = self.whole.raster(list, sources, &Rig::view(scale)).unwrap();
        let p = self.whole.read_back(&frame).unwrap();
        self.whole.recycle(frame);
        p
    }

    /// The list split, its pieces rastered as the raster thread does (the first piece as the full
    /// frame, the others sparse over what they draw), and composed as the compositor draws them.
    /// Returns the picture and how many tiles were painted and reused.
    fn split(&mut self, list: &DisplayList, scale: f32) -> (Pixmap, RasterStats, usize) {
        let sources = Sources {
            text: &self.text,
            images: &self.images,
        };
        let view = Rig::view(scale);
        let split = list.split_layers();
        let mut pieces = split.layers.iter();
        let first = pieces.next().unwrap();
        assert!(first.kind.is_none() && first.state.0.is_empty());
        let base = self
            .tiles
            .raster_region(&first.list, sources, &view, (0, 0), false)
            .unwrap();
        let mut frames = Vec::new();
        for (n, piece) in pieces.enumerate() {
            // (the region the raster thread gives a piece)
            let drawn = piece.list.drawn_bounds(&Matrix::scale(scale, scale));
            let (x, y) = (drawn.x.floor(), drawn.y.floor());
            let w = (drawn.right().ceil() - x).max(1.0) as u32;
            let h = (drawn.bottom().ceil() - y).max(1.0) as u32;
            let at = (x as i32, y as i32);
            let pv = Viewport {
                width: w,
                height: h,
                scale,
                background: Color::TRANSPARENT,
            };
            let frame = self
                .tiles
                .raster_piece(&piece.list, sources, &pv, at, n as u64 + 1)
                .unwrap();
            frames.push((frame, at, piece.state.clone()));
        }
        let stats = self.tiles.stats();
        let refs: Vec<_> = frames.iter().map(|(f, at, s)| (f, *at, s)).collect();
        let picture = self
            .tiles
            .compose(&base, &refs, scale, Color::WHITE)
            .unwrap();
        // (the frames before these go back now that these are rastered, as the compositor recycles
        // the frame it no longer shows when the next one arrives)
        let count = split.layers.len();
        for f in self.shown.drain(..) {
            self.tiles.recycle(f);
        }
        self.shown.push(base);
        self.shown.extend(frames.into_iter().map(|(f, _, _)| f));
        (picture, stats, count)
    }
}

/// The largest difference of a channel, and how many pixels differ by more than `slack`.
fn diff(a: &Pixmap, b: &Pixmap, slack: u8) -> (u8, usize) {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let mut max = 0u8;
    let mut over = 0;
    for (x, y) in a.data.chunks(4).zip(b.data.chunks(4)) {
        let d = x.iter().zip(y).map(|(p, q)| p.abs_diff(*q)).max().unwrap();
        max = max.max(d);
        if d > slack {
            over += 1;
        }
    }
    (max, over)
}

/// A composited box moved by whole device pixels, at 1x and 1.5x, under a clip: the same pixels
/// as the list rastered whole.
#[test]
fn a_composited_layer_at_a_whole_pixel_offset_looks_as_painted() {
    let mut rig = Rig::new();
    for scale in [1.0, 1.5] {
        for clip in [None, Some(Rect::new(0.0, 100.0, 400.0, 100.0))] {
            let list = page(Matrix::translation(20.0, -10.0), 1.0, clip);
            let whole = rig.whole(&list, scale);
            let (split, _, pieces) = rig.split(&list, scale);
            // (what follows the box does not overlap it: it joins the page's piece)
            assert_eq!(pieces, 2, "the page, the box");
            assert_eq!(
                diff(&whole, &split, 0),
                (0, 0),
                "scale {scale}, clip {clip:?}"
            );
        }
    }
}

/// A blurred composited box whose opacity a compositor runs (a filter applies before opacity,
/// Filter Effects 1 §5): its layer holds the whole blur, which reaches past the box, so the split
/// list looks as the list rastered whole, with no edge where the box ends. On gray, probing the
/// blur's fringe outside the box, where only the group's spread draws.
#[test]
fn a_blurred_composited_box_keeps_its_blur_past_its_edges() {
    let mut rig = Rig::new();
    for scale in [1.0, 1.5] {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 400.0, 300.0), Color::rgb(200, 200, 200));
        let content = Rect::new(140.0, 90.0, 120.0, 120.0);
        // (the group's bounds hold the blur's outset, 3 sigma)
        let bounds = content.inflate(72.0, 72.0);
        b.push_opacity(0.9, bounds);
        b.push_layer(LayerKind::Composited(42));
        b.push_filter(&[FilterOp::Blur(24.0)], bounds);
        b.fill_rect(content, Color::hex(0x2040c0));
        b.pop_filter();
        b.pop_layer();
        b.pop_opacity();
        let list = b.build();
        let whole = rig.whole(&list, scale);
        let (split, _, pieces) = rig.split(&list, scale);
        assert_eq!(pieces, 2, "the page, the box");
        // Outside the box, within the blur's reach: the fringe is drawn, as in the whole raster.
        let px = |x: f32, y: f32| ((x * scale) as u32, (y * scale) as u32);
        for (x, y) in [
            (130.0, 150.0),
            (270.0, 150.0),
            (200.0, 80.0),
            (200.0, 220.0),
        ] {
            let (x, y) = px(x, y);
            assert_ne!(
                whole.pixel(x, y),
                Color::rgb(200, 200, 200),
                "the blur reaches ({x}, {y}) at scale {scale}"
            );
        }
        // (but for rounding of the blend through the layer's opacity; cut at the box's edge, the
        // difference is 71)
        let (max, _) = diff(&whole, &split, 0);
        assert!(max <= 4, "scale {scale}: largest difference {max}");
    }
}

/// Faded: the same pixels but for rounding of the blend.
#[test]
fn a_faded_composited_layer_looks_as_painted() {
    let mut rig = Rig::new();
    let list = page(Matrix::translation(-10.0, 0.0), 0.5, None);
    let whole = rig.whole(&list, 1.0);
    let (split, _, _) = rig.split(&list, 1.0);
    let (max, _) = diff(&whole, &split, 0);
    assert!(max <= 2, "largest difference {max}");
}

/// Turned: drawn from its own raster through the rotation (linear filtering, as Chromium's
/// compositor draws a turned layer), so edges differ by antialiasing; nothing else.
#[test]
fn a_turned_composited_layer_looks_as_painted_up_to_its_edges() {
    let mut rig = Rig::new();
    let turn = Matrix::rotation_about(0.6, Point::new(180.0, 160.0));
    let clip = Some(Rect::new(0.0, 0.0, 400.0, 200.0));
    let list = page(turn, 1.0, clip);
    let whole = rig.whole(&list, 1.0);
    let (split, _, _) = rig.split(&list, 1.0);
    let (_, over) = diff(&whole, &split, 48);
    // (the box and its border cover about 20,000 pixels; their edges about 1,300)
    assert!(over < 400, "{over} pixels differ by more than 48");
    // The inside of the box and the page around it are exact.
    for (x, y) in [(180, 150), (60, 40), (330, 230), (10, 290)] {
        assert_eq!(whole.pixel(x, y), split.pixel(x, y), "({x}, {y})");
    }
}

/// An animation frame that only changes the composited box's transform and opacity changes no
/// piece's list: every tile is copied from the frame before, none painted.
#[test]
fn a_frame_that_only_turns_or_fades_a_composited_box_paints_no_tile() {
    let mut rig = Rig::new();
    let at = |a: f32| Matrix::rotation_about(a, Point::new(180.0, 160.0));
    let first = page(at(0.1), 0.9, None);
    let (_, stats, _) = rig.split(&first, 1.0);
    assert!(stats.painted > 0);
    for (angle, opacity) in [(0.4, 0.9), (1.3, 0.5), (2.0, 0.75)] {
        let list = page(at(angle), opacity, None);
        let (picture, stats, _) = rig.split(&list, 1.0);
        assert_eq!(stats.painted, 0, "angle {angle}: {stats:?}");
        assert!(stats.reused > 0);
        // (and what shows is the new frame: the box's inside, turned, over the page)
        let whole = rig.whole(&list, 1.0);
        assert_eq!(whole.pixel(180, 160), picture.pixel(180, 160));
    }
    // Without a composited layer, the same frames repaint every tile the box's transform group
    // reaches (each group's push counts in every tile).
    let mut plain = DisplayListBuilder::new();
    plain.fill_rect(Rect::new(0.0, 0.0, 400.0, 300.0), Color::WHITE);
    plain.push_transform(at(0.4));
    plain.fill_rect(Rect::new(150.0, 150.0, 40.0, 40.0), Color::BLACK);
    plain.pop_transform();
    let sources = Sources {
        text: &rig.text,
        images: &rig.images,
    };
    let f = rig
        .whole
        .raster(&plain.build(), sources, &Rig::view(1.0))
        .unwrap();
    let mut again = DisplayListBuilder::new();
    again.fill_rect(Rect::new(0.0, 0.0, 400.0, 300.0), Color::WHITE);
    again.push_transform(at(0.5));
    again.fill_rect(Rect::new(150.0, 150.0, 40.0, 40.0), Color::BLACK);
    again.pop_transform();
    rig.whole
        .raster(&again.build(), sources, &Rig::view(1.0))
        .unwrap();
    assert_eq!(rig.whole.stats().reused, 0);
    rig.whole.recycle(f);
}

/// A composited box with its own filter of color functions around its layer (Filter Effects 1
/// §10: each pixel on its own, so it commutes with the box's clips and mask): split off with the
/// filter as a state step, drawn through it by the compositor, it looks as the list rastered
/// whole, at 1x and 1.5x, moved and faded, but for rounding; turned, but for its edges.
#[test]
fn a_composited_box_with_a_color_filter_looks_as_painted() {
    let mut rig = Rig::new();
    let filters = [
        FilterOp::Grayscale(0.6),
        FilterOp::HueRotate(70.0),
        FilterOp::Invert(0.2),
        FilterOp::Contrast(1.4),
        FilterOp::Opacity(0.8),
    ];
    let list = |m: Matrix, opacity: f32| {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 400.0, 300.0), Color::rgb(200, 200, 200));
        let bounds = Rect::new(100.0, 80.0, 160.0, 160.0);
        b.push_transform(m);
        b.push_opacity(opacity, bounds);
        b.push_filter(&filters, bounds);
        b.push_layer(LayerKind::Composited(42));
        b.fill_rounded_rect(
            Rect::new(110.0, 90.0, 140.0, 140.0),
            Radii::uniform(30.0),
            Color::hex(0xcc3333),
        );
        b.fill_rect(Rect::new(150.0, 120.0, 60.0, 30.0), Color::hex(0x2266dd));
        b.pop_layer();
        b.pop_filter();
        b.pop_opacity();
        b.pop_transform();
        b.build()
    };
    for scale in [1.0, 1.5] {
        for (m, opacity) in [
            (Matrix::translation(20.0, -10.0), 1.0),
            (Matrix::translation(-10.0, 6.0), 0.6),
        ] {
            let l = list(m, opacity);
            let state = &l.split_layers().layers[1].state;
            assert!(
                state.0.iter().any(|op| matches!(op, StateOp::Filter(_))),
                "{state:?}"
            );
            let whole = rig.whole(&l, scale);
            let (split, _, pieces) = rig.split(&l, scale);
            assert_eq!(pieces, 2, "the page, the box");
            let (max, _) = diff(&whole, &split, 0);
            assert!(
                max <= 3,
                "scale {scale}, opacity {opacity}: largest difference {max}"
            );
        }
    }
    let l = list(Matrix::rotation_about(0.6, Point::new(180.0, 160.0)), 1.0);
    let whole = rig.whole(&l, 1.0);
    let (split, _, _) = rig.split(&l, 1.0);
    let (_, over) = diff(&whole, &split, 48);
    assert!(over < 400, "{over} pixels differ by more than 48");
    for (x, y) in [(180, 150), (60, 40), (330, 230)] {
        let (a, b) = (whole.pixel(x, y), split.pixel(x, y));
        let d = [a.r.abs_diff(b.r), a.g.abs_diff(b.g), a.b.abs_diff(b.b)];
        assert!(d.iter().all(|&d| d <= 3), "({x}, {y}): {a:?} {b:?}");
    }
}
