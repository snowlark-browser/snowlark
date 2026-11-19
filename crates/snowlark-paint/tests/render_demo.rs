// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Renders the demo display list headless and on GPU tiles (WARP, so it runs without a graphics card)
//! and checks pixels whose colors the demo fixes.

use snowlark_paint::*;

fn close(a: Color, b: Color, tolerance: i32) -> bool {
    (a.r as i32 - b.r as i32).abs() <= tolerance
        && (a.g as i32 - b.g as i32).abs() <= tolerance
        && (a.b as i32 - b.b as i32).abs() <= tolerance
        && (a.a as i32 - b.a as i32).abs() <= tolerance
}

fn view() -> Viewport {
    Viewport {
        width: demo::WIDTH,
        height: demo::HEIGHT,
        scale: 1.0,
        background: demo::BACKGROUND,
    }
}

fn check(pixmap: &Pixmap, what: &str) {
    assert_eq!((pixmap.width, pixmap.height), (demo::WIDTH, demo::HEIGHT));
    for &(x, y, expected) in demo::PROBES {
        let got = pixmap.pixel(x as u32, y as u32);
        assert!(
            close(got, expected, 2),
            "{what}: ({x}, {y}) is {got:?}, expected {expected:?}"
        );
    }
    // The header gradient runs from #1f3b73 on the left to #6a2c91 on the right.
    let left = pixmap.pixel(2, 50);
    let right = pixmap.pixel(1297, 50);
    assert!(
        close(left, Color::hex(0x1f3b73), 3),
        "{what}: header left {left:?}"
    );
    assert!(
        close(right, Color::hex(0x6a2c91), 3),
        "{what}: header right {right:?}"
    );
    // The card's shadow darkens the background below it, softly.
    let shadow = pixmap.pixel(340, 552);
    assert!(
        shadow.r < demo::BACKGROUND.r && shadow.r > 150,
        "{what}: shadow {shadow:?}"
    );
    // Emoji are drawn in color: the line has saturated yellow pixels.
    let yellow = (690..1100)
        .flat_map(|x| (480..560).map(move |y| (x, y)))
        .filter(|&(x, y)| {
            let c = pixmap.pixel(x, y);
            c.r > 200 && c.g > 150 && c.b < 90
        })
        .count();
    assert!(yellow > 50, "{what}: only {yellow} yellow emoji pixels");
    // Text is drawn: the Segoe UI sample line has dark ink.
    let ink = (690..1000)
        .filter(|&x| pixmap.pixel(x, 190).r < 100)
        .count();
    assert!(ink > 10, "{what}: no text ink ({ink})");
    // The pixel-art face is scaled with nearest neighbor: a solid yellow at its center.
    let face = pixmap.pixel(1060 + 84, 660 + 30);
    assert!(
        close(face, Color::rgb(255, 200, 40), 2),
        "{what}: face {face:?}"
    );
}

#[test]
fn demo_renders_headless() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let list = demo::build(&text, &images).unwrap();
    let mut renderer = HeadlessRenderer::new().unwrap();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let pixmap = renderer.render(&list, sources, &view()).unwrap();
    check(&pixmap, "headless");
    // ClearType on the opaque target: the sample line has color fringes
    assert!(
        chromatic(&pixmap, 688..860, 176..196) > 100,
        "no ClearType fringes"
    );
    // the target is reused for the same size: the second frame is the same
    let again = renderer.render(&list, sources, &view()).unwrap();
    assert_eq!(again.data, pixmap.data);
    // grayscale text (the reftest screenshots): the same ink, without fringes
    renderer.set_grayscale_text(true);
    let gray = renderer.render(&list, sources, &view()).unwrap();
    assert!(
        chromatic(&gray, 688..860, 176..196) == 0,
        "fringes in grayscale text"
    );
    let ink = (690..1000).filter(|&x| gray.pixel(x, 190).r < 100).count();
    assert!(ink > 10, "no grayscale text ink ({ink})");
}

#[test]
fn demo_renders_in_gpu_tiles() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let list = demo::build(&text, &images).unwrap();
    let gpu = Gpu::with_driver(Driver::Warp).unwrap();
    let mut tiles = TileRasterizer::new(gpu).unwrap();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let frame = tiles.raster(&list, sources, &view()).unwrap();
    // 1300x1000 in 512 px tiles: 3 across, 2 down
    assert_eq!(frame.tiles.len(), 6);
    let pixmap = tiles.read_back(&frame).unwrap();
    check(&pixmap, "tiles");
    // seams: the background either side of a tile edge is the same
    assert_eq!(pixmap.pixel(1290, 511), pixmap.pixel(1290, 512));
}

#[test]
fn transparent_background_renders_premultiplied() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut b = DisplayListBuilder::new();
    b.fill_rect(
        Rect::new(10.0, 10.0, 20.0, 20.0),
        Color::rgba(255, 0, 0, 128),
    );
    let list = b.build();
    let mut renderer = HeadlessRenderer::new().unwrap();
    let pixmap = renderer
        .render(
            &list,
            Sources {
                text: &text,
                images: &images,
            },
            &Viewport {
                width: 40,
                height: 40,
                scale: 2.0,
                background: Color::TRANSPARENT,
            },
        )
        .unwrap();
    assert_eq!(pixmap.pixel(0, 0), Color::TRANSPARENT);
    // scale 2: the rect covers device pixels 20..60, cut to the 40 px target
    assert!(close(pixmap.pixel(30, 30), Color::rgba(255, 0, 0, 128), 2));
    assert_eq!(pixmap.pixel(19, 30), Color::TRANSPARENT);
}

#[test]
fn malformed_lists_are_refused() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut list = DisplayList::new();
    list.items.push(Item::PopClip);
    let mut renderer = HeadlessRenderer::new().unwrap();
    let r = renderer.render(
        &list,
        Sources {
            text: &text,
            images: &images,
        },
        &view(),
    );
    assert!(r.is_err());
}

/// Pixels whose channels differ a lot (ClearType fringes on black-on-beige text).
fn chromatic(p: &Pixmap, xs: std::ops::Range<u32>, ys: std::ops::Range<u32>) -> usize {
    xs.flat_map(|x| ys.clone().map(move |y| (x, y)))
        .filter(|&(x, y)| {
            let c = p.pixel(x, y);
            let max = c.r.max(c.g).max(c.b) as i32;
            let min = c.r.min(c.g).min(c.b) as i32;
            max - min > 40
        })
        .count()
}

#[test]
fn text_on_transparent_targets_is_grayscale() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let fonts = text
        .font_list(&FontQuery {
            families: &["Segoe UI"],
            weight: 400,
            style: FontStyle::Normal,
            stretch: FontStretch::NORMAL,
        })
        .unwrap();
    let mut shaped = ShapedText::default();
    let options = ShapeOptions {
        size: 16.0,
        ..Default::default()
    };
    text.shape("Grayscale text here", fonts, &options, &mut shaped)
        .unwrap();
    let mut b = DisplayListBuilder::new();
    shaped.paint(
        0..shaped.clusters.len(),
        Point::new(4.0, 20.0),
        Color::BLACK,
        &mut b,
    );
    let list = b.build();
    let mut renderer = HeadlessRenderer::new().unwrap();
    let pixmap = renderer
        .render(
            &list,
            Sources {
                text: &text,
                images: &images,
            },
            &Viewport {
                width: 200,
                height: 30,
                scale: 1.0,
                background: Color::TRANSPARENT,
            },
        )
        .unwrap();
    let inked = (0..200)
        .flat_map(|x| (0..30).map(move |y| (x, y)))
        .filter(|&(x, y)| pixmap.pixel(x, y).a > 0)
        .count();
    assert!(inked > 100);
    assert_eq!(chromatic(&pixmap, 0..200, 0..30), 0);
}

/// A mask group (CSS `mask-image`): the content shows where the mask's alpha is, nowhere outside
/// the bounds; with `luminance` the mask's luminance counts. Headless and on GPU tiles.
#[test]
fn masks_cover_content_by_alpha_or_luminance() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    // A 2x1 image, opaque on the left, transparent on the right, drawn smoothly over 0..20: the
    // left half shows (a 20 px image would not be sampled exactly).
    let half = images.add(DecodedImage {
        frames: vec![Frame {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
            delay_ms: 0,
        }],
        orientation: 1,
        animation: None,
    });
    let mut b = DisplayListBuilder::new();
    // An image mask bounded to 10..50 whose image covers 30..50: only 30..50 shows.
    b.push_mask(Rect::new(10.0, 0.0, 40.0, 20.0), false);
    b.image(
        half,
        Rect::new(30.0, 0.0, 20.0, 20.0),
        None,
        1.0,
        ImageRendering::Smooth,
    );
    b.mask_content();
    b.fill_rect(Rect::new(0.0, 0.0, 60.0, 20.0), Color::hex(0xff0000));
    b.pop_mask();
    // A luminance mask over 10..50 at y 20..40: black on the left hides, white on the right shows.
    b.push_mask(Rect::new(10.0, 20.0, 40.0, 20.0), true);
    b.fill_rect(Rect::new(10.0, 20.0, 20.0, 20.0), Color::BLACK);
    b.fill_rect(Rect::new(30.0, 20.0, 20.0, 20.0), Color::WHITE);
    b.mask_content();
    b.fill_rect(Rect::new(0.0, 20.0, 60.0, 20.0), Color::hex(0x0000ff));
    b.pop_mask();
    let list = b.build();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let view = Viewport {
        width: 60,
        height: 40,
        scale: 1.0,
        background: Color::WHITE,
    };
    let check = |p: &Pixmap, what: &str| {
        let at = |x, y| p.pixel(x, y);
        assert!(
            close(at(40, 10), Color::hex(0xff0000), 2),
            "{what}: alpha in"
        );
        assert!(close(at(20, 10), Color::WHITE, 2), "{what}: alpha out");
        assert!(
            close(at(5, 10), Color::WHITE, 2),
            "{what}: before the bounds"
        );
        assert!(
            close(at(55, 10), Color::WHITE, 2),
            "{what}: after the bounds"
        );
        assert!(
            close(at(40, 30), Color::hex(0x0000ff), 2),
            "{what}: luminance in"
        );
        assert!(close(at(20, 30), Color::WHITE, 2), "{what}: luminance out");
        assert!(
            close(at(5, 30), Color::WHITE, 2),
            "{what}: luminance bounds"
        );
    };
    let mut renderer = HeadlessRenderer::new().unwrap();
    check(&renderer.render(&list, sources, &view).unwrap(), "headless");
    let gpu = Gpu::with_driver(Driver::Warp).unwrap();
    let mut tiles = TileRasterizer::new(gpu).unwrap();
    let frame = tiles.raster(&list, sources, &view).unwrap();
    check(&tiles.read_back(&frame).unwrap(), "tiles");
}

/// CSS Masking 1 §5: a clip by path shows what it clips only inside the path's fill, by its fill
/// rule, under a transform too; its pop ends it.
#[test]
fn clip_paths_show_only_the_fill_of_their_path() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let p = Point::new;
    let mut b = DisplayListBuilder::new();
    // A triangle (0,0) (60,0) (0,60): its lower right half clips out.
    let triangle = b
        .add_path(&[
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(60.0, 0.0)),
            PathEl::LineTo(p(0.0, 60.0)),
            PathEl::Close,
        ])
        .unwrap();
    b.push_clip_path(triangle, false);
    b.fill_rect(Rect::new(0.0, 0.0, 60.0, 60.0), Color::hex(0xff0000));
    b.pop_clip();
    // After the pop, nothing is clipped.
    b.fill_rect(Rect::new(70.0, 0.0, 20.0, 20.0), Color::hex(0x00ff00));
    // A square with a square hole, even-odd, moved by a transform to x 100..160.
    b.push_transform(Matrix::translation(100.0, 0.0));
    let ring = b
        .add_path(&[
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(60.0, 0.0)),
            PathEl::LineTo(p(60.0, 60.0)),
            PathEl::LineTo(p(0.0, 60.0)),
            PathEl::Close,
            PathEl::MoveTo(p(20.0, 20.0)),
            PathEl::LineTo(p(40.0, 20.0)),
            PathEl::LineTo(p(40.0, 40.0)),
            PathEl::LineTo(p(20.0, 40.0)),
            PathEl::Close,
        ])
        .unwrap();
    b.push_clip_path(ring, true);
    b.fill_rect(Rect::new(-50.0, -50.0, 200.0, 200.0), Color::hex(0x0000ff));
    b.pop_clip();
    b.pop_transform();
    let list = b.build();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let view = Viewport {
        width: 180,
        height: 80,
        scale: 1.0,
        background: Color::WHITE,
    };
    let check = |px: &Pixmap, what: &str| {
        let at = |x, y| px.pixel(x, y);
        assert!(
            close(at(10, 10), Color::hex(0xff0000), 2),
            "{what}: in the triangle"
        );
        assert!(
            close(at(50, 50), Color::WHITE, 2),
            "{what}: out of the triangle"
        );
        assert!(close(at(5, 70), Color::WHITE, 2), "{what}: below it");
        assert!(
            close(at(80, 10), Color::hex(0x00ff00), 2),
            "{what}: after the pop"
        );
        assert!(
            close(at(105, 30), Color::hex(0x0000ff), 2),
            "{what}: in the ring"
        );
        assert!(close(at(130, 30), Color::WHITE, 2), "{what}: in its hole");
        assert!(close(at(170, 30), Color::WHITE, 2), "{what}: right of it");
        assert!(close(at(130, 70), Color::WHITE, 2), "{what}: below it");
    };
    let mut renderer = HeadlessRenderer::new().unwrap();
    check(&renderer.render(&list, sources, &view).unwrap(), "headless");
    let gpu = Gpu::with_driver(Driver::Warp).unwrap();
    let mut tiles = TileRasterizer::new(gpu).unwrap();
    let frame = tiles.raster(&list, sources, &view).unwrap();
    check(&tiles.read_back(&frame).unwrap(), "tiles");
}

/// Damage: a frame of the same geometry repaints only the tiles whose items changed and copies the
/// others from the previous frame; the pixels are those of a full repaint.
#[test]
fn unchanged_tiles_are_copied_from_the_previous_frame() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let gpu = Gpu::with_driver(Driver::Warp).unwrap();
    let mut tiles = TileRasterizer::new(gpu.clone()).unwrap();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let view = Viewport {
        width: 1024,
        height: 1024,
        scale: 1.0,
        background: Color::WHITE,
    };
    let list = |highlight: Color| {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(10.0, 10.0, 100.0, 100.0), Color::rgb(0, 128, 0));
        // (only in the bottom-right tile)
        b.fill_rect(Rect::new(600.0, 600.0, 50.0, 50.0), highlight);
        b.build()
    };
    let red = Color::rgb(255, 0, 0);
    let blue = Color::rgb(0, 0, 255);
    let first = tiles.raster(&list(red), sources, &view).unwrap();
    assert_eq!(tiles.stats().painted, 4);
    let second = tiles.raster(&list(blue), sources, &view).unwrap();
    assert_eq!((tiles.stats().painted, tiles.stats().reused), (1, 3));
    let pixels = tiles.read_back(&second).unwrap();
    assert!(close(pixels.pixel(50, 50), Color::rgb(0, 128, 0), 1));
    assert!(close(pixels.pixel(620, 620), blue, 1));
    // The same list again: nothing painted.
    tiles.recycle(first);
    let third = tiles.raster(&list(blue), sources, &view).unwrap();
    assert_eq!((tiles.stats().painted, tiles.stats().reused), (0, 4));
    assert_eq!(tiles.read_back(&third).unwrap().data, pixels.data);
    // Another geometry (scale): everything is painted.
    let scaled = Viewport { scale: 2.0, ..view };
    tiles.raster(&list(blue), sources, &scaled).unwrap();
    assert_eq!(tiles.stats().painted, 4);
    // Recycling the frame the next raster would copy from forgets it.
    let mut fresh = TileRasterizer::new(gpu).unwrap();
    let f1 = fresh.raster(&list(red), sources, &view).unwrap();
    fresh.recycle(f1);
    fresh.raster(&list(red), sources, &view).unwrap();
    assert_eq!(fresh.stats().reused, 0);
}

/// Renders `list` into a `w`×`h` white target at `scale`.
fn render_at(list: &DisplayList, w: u32, h: u32, scale: f32) -> Pixmap {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let mut renderer = HeadlessRenderer::new().unwrap();
    renderer
        .render(
            list,
            Sources {
                text: &text,
                images: &images,
            },
            &Viewport {
                width: w,
                height: h,
                scale,
                background: Color::WHITE,
            },
        )
        .unwrap()
}

#[test]
fn box_edges_snap_to_device_pixels() {
    // A fill and a border at fractional positions: each edge on the nearest device pixel, with
    // no antialiased (blended) row or column anywhere.
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(2.3, 2.6, 10.4, 5.2), Color::rgb(0, 0, 255));
    b.border(Border {
        rect: Rect::new(20.5, 3.25, 12.0, 8.4),
        radii: Radii::ZERO,
        widths: Sides::all(1.0),
        colors: [Color::BLACK; 4],
        styles: [BorderStyle::Solid; 4],
    });
    let list = b.build();
    let p = render_at(&list, 40, 16, 1.0);
    let blended = (0..16)
        .flat_map(|y| (0..40).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let c = p.pixel(x, y);
            c != Color::WHITE && c != Color::BLACK && c != Color::rgb(0, 0, 255)
        })
        .count();
    assert_eq!(blended, 0);
    // the fill: x 2.3..12.7 -> 2..13, y 2.6..7.8 -> 3..8
    assert_eq!(p.pixel(2, 3), Color::rgb(0, 0, 255));
    assert_eq!(p.pixel(12, 7), Color::rgb(0, 0, 255));
    assert_eq!(p.pixel(13, 5), Color::WHITE);
    assert_eq!(p.pixel(5, 2), Color::WHITE);
    assert_eq!(p.pixel(5, 8), Color::WHITE);
    // the border: x 20.5..32.5 -> 21..33 (half up), y 3.25..11.65 -> 3..12
    assert_eq!(p.pixel(21, 6), Color::BLACK);
    assert_eq!(p.pixel(20, 6), Color::WHITE);
    assert_eq!(p.pixel(32, 6), Color::BLACK);
    assert_eq!(p.pixel(25, 3), Color::BLACK);
    assert_eq!(p.pixel(25, 11), Color::BLACK);
    assert_eq!(p.pixel(25, 10), Color::WHITE);

    // At a device scale of 1.5 the edges are on device pixels too: 2.3 * 1.5 = 3.45 -> 3.
    let p = render_at(&list, 60, 24, 1.5);
    assert_eq!(p.pixel(3, 6), Color::rgb(0, 0, 255));
    assert_eq!(p.pixel(2, 6), Color::WHITE);
}

#[test]
fn no_snapping_under_a_fractional_translation() {
    // Inside a transform other than a whole-pixel translation, edges are not moved: the half
    // pixel shows as a blended column.
    let mut b = DisplayListBuilder::new();
    b.push_transform(Matrix::translation(0.5, 0.0));
    b.fill_rect(Rect::new(2.0, 2.0, 10.0, 5.0), Color::BLACK);
    b.pop_transform();
    let p = render_at(&b.build(), 20, 10, 1.0);
    let c = p.pixel(2, 4);
    assert!(c.r > 64 && c.r < 192, "{c:?}");
    // a whole-pixel translation keeps snapping
    let mut b = DisplayListBuilder::new();
    b.push_transform(Matrix::translation(1.0, 0.0));
    b.fill_rect(Rect::new(2.4, 2.0, 10.0, 5.0), Color::BLACK);
    b.pop_transform();
    let p = render_at(&b.build(), 20, 10, 1.0);
    assert_eq!(p.pixel(3, 4), Color::BLACK);
    // (2.4..12.4 moved by 1 is 3.4..13.4: pixels 3 to 12)
    assert_eq!(p.pixel(12, 4), Color::BLACK);
    assert_eq!(p.pixel(13, 4), Color::WHITE);
}

#[test]
fn hard_color_stops_are_crisp() {
    // linear-gradient(to right, red 50%, blue 50%) and a repeating 45° one: every pixel is one
    // of the two colors (Chrome evaluates the gradient at pixel centers).
    let (red, blue) = (Color::rgb(255, 0, 0), Color::rgb(0, 0, 255));
    let stops = [
        GradientStop {
            offset: 0.0,
            color: red,
        },
        GradientStop {
            offset: 0.5,
            color: red,
        },
        GradientStop {
            offset: 0.5,
            color: blue,
        },
        GradientStop {
            offset: 1.0,
            color: blue,
        },
    ];
    for (start, end, extend) in [
        (Point::new(0.0, 0.0), Point::new(101.0, 0.0), Extend::Clamp),
        (Point::new(0.0, 0.0), Point::new(7.0, 7.0), Extend::Repeat),
    ] {
        let mut b = DisplayListBuilder::new();
        b.linear_gradient(
            Rect::new(0.0, 0.0, 101.0, 40.0),
            Radii::ZERO,
            start,
            end,
            &stops,
            extend,
        );
        let p = render_at(&b.build(), 101, 40, 1.0);
        let other = (0..40)
            .flat_map(|y| (0..101).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let c = p.pixel(x, y);
                c != red && c != blue
            })
            .count();
        assert_eq!(other, 0, "{extend:?}");
        if extend == Extend::Clamp {
            // the stop at 50.5: pixel 50 (center 50.5) is blue, as in Skia
            assert_eq!(p.pixel(49, 10), red);
            assert_eq!(p.pixel(51, 10), blue);
        }
    }
}

#[test]
fn radial_hard_color_stops_are_crisp() {
    // radial-gradient(circle 20px, red 50%, blue 50%), and a repeating one: no blended ring.
    let (red, blue) = (Color::rgb(255, 0, 0), Color::rgb(0, 0, 255));
    let stop = |offset: f32, color: Color| GradientStop { offset, color };
    let stops = [
        stop(0.0, red),
        stop(0.5, red),
        stop(0.5, blue),
        stop(1.0, blue),
    ];
    for (radius, extend) in [(20.0, Extend::Clamp), (6.0, Extend::Repeat)] {
        let mut b = DisplayListBuilder::new();
        b.radial_gradient(
            Rect::new(0.0, 0.0, 41.0, 41.0),
            Radii::ZERO,
            Point::new(20.5, 20.5),
            Size::new(radius, radius),
            &stops,
            extend,
        );
        let p = render_at(&b.build(), 41, 41, 1.0);
        let other = (0..41)
            .flat_map(|y| (0..41).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let c = p.pixel(x, y);
                c != red && c != blue
            })
            .count();
        assert_eq!(other, 0, "{extend:?}");
        assert_eq!(p.pixel(20, 20), red);
    }
}

#[test]
fn scaled_images_keep_solid_edges() {
    // A 2x2 green image drawn at 20x20 (a scaled draw): its edge pixels are fully green, as in
    // Chrome (the image is sampled clamped to its bounds), not faded over the last half pixel.
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let green = [0u8, 128, 0, 255];
    let id = images.add(DecodedImage::still(Frame {
        width: 2,
        height: 2,
        pixels: green.repeat(4),
        delay_ms: 0,
    }));
    let mut b = DisplayListBuilder::new();
    b.image(
        id,
        Rect::new(5.0, 5.0, 20.0, 20.0),
        None,
        1.0,
        ImageRendering::Smooth,
    );
    let mut renderer = HeadlessRenderer::new().unwrap();
    let p = renderer
        .render(
            &b.build(),
            Sources {
                text: &text,
                images: &images,
            },
            &Viewport {
                width: 30,
                height: 30,
                scale: 1.0,
                background: Color::WHITE,
            },
        )
        .unwrap();
    for (x, y) in [(5, 5), (24, 24), (5, 24), (15, 5)] {
        assert_eq!(p.pixel(x, y), Color::rgb(0, 128, 0), "({x}, {y})");
    }
    assert_eq!(p.pixel(4, 15), Color::WHITE);
    assert_eq!(p.pixel(25, 15), Color::WHITE);
}

/// Text inside a layer (a rounded `overflow` clip, an opacity group) on an opaque target is drawn:
/// ClearType into a layer that starts transparent draws nothing, so layers use grayscale text.
#[test]
fn text_inside_layers_is_drawn() {
    let text = TextEngine::new().unwrap();
    let images = ImageStore::new();
    let fonts = text
        .font_list(&FontQuery {
            families: &["Segoe UI"],
            weight: 400,
            style: FontStyle::Normal,
            stretch: FontStretch::NORMAL,
        })
        .unwrap();
    let mut shaped = ShapedText::default();
    let options = ShapeOptions {
        size: 16.0,
        ..Default::default()
    };
    text.shape("Text in a layer", fonts, &options, &mut shaped)
        .unwrap();
    let paint = |b: &mut DisplayListBuilder, y: f32| {
        shaped.paint(
            0..shaped.clusters.len(),
            Point::new(4.0, y),
            Color::BLACK,
            b,
        );
    };
    let mut b = DisplayListBuilder::new();
    b.push_clip_rounded_rect(Rect::new(0.0, 0.0, 200.0, 30.0), Radii::uniform(4.0));
    paint(&mut b, 20.0);
    b.pop_clip();
    b.push_opacity(0.99, Rect::new(0.0, 30.0, 200.0, 30.0));
    paint(&mut b, 50.0);
    b.pop_opacity();
    // Outside any layer the text is ClearType again.
    paint(&mut b, 80.0);
    let list = b.build();
    let mut renderer = HeadlessRenderer::new().unwrap();
    let pixmap = renderer
        .render(
            &list,
            Sources {
                text: &text,
                images: &images,
            },
            &Viewport {
                width: 200,
                height: 90,
                scale: 1.0,
                background: Color::WHITE,
            },
        )
        .unwrap();
    let ink = |ys: std::ops::Range<u32>| {
        (0..200u32)
            .flat_map(|x| ys.clone().map(move |y| (x, y)))
            .filter(|&(x, y)| pixmap.pixel(x, y).r < 128)
            .count()
    };
    assert!(ink(0..30) > 50, "rounded clip");
    assert!(ink(30..60) > 50, "opacity group");
    assert!(ink(60..90) > 50, "outside layers");
}

#[test]
fn paths_fill_and_stroke_with_their_rules_and_dashes() {
    let p = |x, y| Point::new(x, y);
    let mut b = DisplayListBuilder::new();
    // two nested squares, the inner one a hole by the even-odd rule
    let squares = b
        .add_path(&[
            PathEl::MoveTo(p(10.0, 10.0)),
            PathEl::LineTo(p(50.0, 10.0)),
            PathEl::LineTo(p(50.0, 50.0)),
            PathEl::LineTo(p(10.0, 50.0)),
            PathEl::Close,
            PathEl::MoveTo(p(20.0, 20.0)),
            PathEl::LineTo(p(40.0, 20.0)),
            PathEl::LineTo(p(40.0, 40.0)),
            PathEl::LineTo(p(20.0, 40.0)),
            PathEl::Close,
        ])
        .unwrap();
    b.fill_path(squares, Color::hex(0xff0000), true, true);
    // under a transform: the same squares by the nonzero rule, filled whole
    b.push_transform(Matrix::translation(60.0, 0.0));
    b.fill_path(squares, Color::hex(0x0000ff), false, true);
    b.pop_transform();
    // a 4 px line dashed 10 on, 10 off, along y = 70
    let line = b
        .add_path(&[PathEl::MoveTo(p(0.0, 70.0)), PathEl::LineTo(p(100.0, 70.0))])
        .unwrap();
    let stroke = Stroke {
        width: 4.0,
        ..Stroke::default()
    };
    b.stroke_path(line, Color::hex(0x00ff00), &stroke, &[10.0, 10.0], true);
    let list = b.build();
    let px = render_at(&list, 120, 80, 1.0);
    let red = Color::hex(0xff0000);
    let blue = Color::hex(0x0000ff);
    let green = Color::hex(0x00ff00);
    assert!(close(px.pixel(15, 15), red, 2), "{:?}", px.pixel(15, 15));
    assert!(
        close(px.pixel(30, 30), Color::WHITE, 2),
        "hole {:?}",
        px.pixel(30, 30)
    );
    assert!(
        close(px.pixel(90, 30), blue, 2),
        "nonzero {:?}",
        px.pixel(90, 30)
    );
    assert!(
        close(px.pixel(5, 70), green, 2),
        "dash {:?}",
        px.pixel(5, 70)
    );
    assert!(
        close(px.pixel(15, 70), Color::WHITE, 2),
        "gap {:?}",
        px.pixel(15, 70)
    );
    assert!(
        close(px.pixel(25, 70), green, 2),
        "dash {:?}",
        px.pixel(25, 70)
    );
    // the stroke is 4 px wide: 2 each side of the line
    assert!(
        close(px.pixel(5, 66), Color::WHITE, 2),
        "{:?}",
        px.pixel(5, 66)
    );
    // and encodes and decodes to the same pixels
    let mut bytes = Vec::new();
    list.encode(&mut bytes);
    let again = render_at(&DisplayList::decode(&bytes).unwrap(), 120, 80, 1.0);
    assert_eq!(again.data, px.data);
}
