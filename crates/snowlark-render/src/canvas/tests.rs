// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The 2D context on both backends: pixel checks on each, and the GPU backend against the CPU one
//! within a tolerance (antialiasing differs at edges).

use super::*;

const SKIA: Backend = Backend::Skia;
const GPU: Backend = Backend::Gpu(Driver::Hardware);

fn backends() -> [Backend; 2] {
    [SKIA, GPU]
}

fn pixel(c: &mut Canvas2D, x: i32, y: i32) -> [u8; 4] {
    let d = c.get_image_data(x, y, 1, 1);
    [d[0], d[1], d[2], d[3]]
}

fn fonts() -> Fonts {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    snowlark_paint::font_files::map_all(&mut db);
    Fonts {
        text: Arc::new(TextEngine::new().unwrap()),
        env: Default::default(),
        cache: Default::default(),
        cpu: Some(Arc::new(db)),
    }
}

fn color(name: &str) -> Style {
    Style::Color(parse_color(name).unwrap(), name.into())
}

/// Pixels whose premultiplied channels differ by more than `tolerance` (the colour of an almost
/// transparent pixel is noise).
fn differing(a: &[u8], b: &[u8], tolerance: u8) -> usize {
    let pm = |p: &[u8]| {
        let k = p[3] as u32;
        [
            (p[0] as u32 * k / 255) as u8,
            (p[1] as u32 * k / 255) as u8,
            (p[2] as u32 * k / 255) as u8,
            p[3],
        ]
    };
    a.chunks(4)
        .zip(b.chunks(4))
        .filter(|(p, q)| {
            pm(p)
                .iter()
                .zip(pm(q).iter())
                .any(|(x, y)| x.abs_diff(*y) > tolerance)
        })
        .count()
}

/// Draws `scene` on both backends and returns their RGBA pixels.
fn both(w: u32, h: u32, scene: impl Fn(&mut Canvas2D)) -> (Vec<u8>, Vec<u8>) {
    let mut cpu = Canvas2D::with_backend(w, h, SKIA);
    let mut gpu = Canvas2D::with_backend(w, h, GPU);
    assert_eq!(gpu.backend(), GPU, "the GPU canvas could not be made");
    scene(&mut cpu);
    scene(&mut gpu);
    if let Ok(dir) = std::env::var("SNOWLARK_CANVAS_TEST_PNG") {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for (name, c) in [("cpu", &mut cpu), ("gpu", &mut gpu)] {
            if let Some(pm) = c.snapshot() {
                let _ = pm.save_png(format!("{dir}/canvas-{n}-{name}.png"));
            }
        }
    }
    (
        cpu.get_image_data(0, 0, w, h),
        gpu.get_image_data(0, 0, w, h),
    )
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "needs the GPU canvas: Windows only until Linux stage 5"
)]
fn text_through_a_layer_lands_where_direct_text_does() {
    let fonts = fonts();
    for backend in backends() {
        let draw = |alpha: f32| {
            let mut c = Canvas2D::with_backend(120, 40, backend);
            c.state.font = "24px sans-serif".into();
            c.state.transform = Transform::from_translate(7.0, 3.0);
            c.state.global_alpha = alpha;
            c.draw_text(&fonts, "MWg", 10.0, 28.0, false);
            c.get_image_data(0, 0, 120, 40)
        };
        let (direct, layered) = (draw(1.0), draw(0.5));
        let mut inked = 0;
        for (d, l) in direct.chunks(4).zip(layered.chunks(4)) {
            assert!(
                (d[3] as i32 / 2 - l[3] as i32).abs() <= 2,
                "{backend:?} {d:?} {l:?}"
            );
            inked += usize::from(d[3] > 0);
        }
        assert!(inked > 50, "{backend:?}");
    }
}

/// A GPU canvas drawn into another through a copy on the device lands as its read-back pixels do,
/// scaled, clipped to the source and with negative sizes; drawn into itself, it draws the bitmap as
/// it was before the call.
#[test]
#[cfg_attr(
    not(windows),
    ignore = "needs the GPU canvas: Windows only until Linux stage 5"
)]
fn a_canvas_drawn_on_the_device_lands_as_its_pixels_do() {
    let mut src = Canvas2D::with_backend(16, 12, GPU);
    src.state.fill = color("red");
    src.fill_rect(0.0, 0.0, 16.0, 12.0);
    src.state.fill = Style::Color(parse_color("#0000ff80").unwrap(), String::new());
    src.fill_rect(4.0, 3.0, 8.0, 6.0);
    type Rect = (f32, f32, f32, f32);
    let cases: [(Rect, Rect); 3] = [
        ((0.0, 0.0, 16.0, 12.0), (2.0, 3.0, 16.0, 12.0)),
        ((2.0, 2.0, 30.0, 8.0), (1.0, 1.0, 20.0, 10.0)),
        ((16.0, 12.0, -10.0, -8.0), (30.0, 25.0, -12.0, -9.0)),
    ];
    for (s, d) in cases {
        let mut by_pixels = Canvas2D::with_backend(40, 30, GPU);
        let pm = src.snapshot().unwrap();
        by_pixels.draw_image(&pm, s, d);
        let mut on_device = Canvas2D::with_backend(40, 30, GPU);
        let snapshot = src.gpu_snapshot().unwrap();
        assert!(on_device.draw_canvas_image(&snapshot, s, d));
        assert_eq!(
            by_pixels.get_image_data(0, 0, 40, 30),
            on_device.get_image_data(0, 0, 40, 30),
            "{s:?} -> {d:?}"
        );
    }
    let before = src.get_image_data(0, 0, 16, 12);
    let snapshot = src.gpu_snapshot().unwrap();
    src.state.fill = color("green");
    src.fill_rect(0.0, 0.0, 16.0, 12.0);
    assert!(src.draw_canvas_image(&snapshot, (0.0, 0.0, 8.0, 12.0), (0.0, 0.0, 8.0, 12.0)));
    let after = src.get_image_data(0, 0, 16, 12);
    assert_eq!(&after[..4], &before[..4]);
    assert_eq!(&after[60..64], &[0, 128, 0, 255]);
    // A CPU canvas cannot draw it: the caller draws the pixels.
    let mut cpu = Canvas2D::with_backend(4, 4, SKIA);
    assert!(!cpu.draw_canvas_image(&snapshot, (0.0, 0.0, 4.0, 4.0), (0.0, 0.0, 4.0, 4.0)));
}

#[test]
fn rects_paths_and_state() {
    for backend in backends() {
        let mut c = Canvas2D::with_backend(20, 20, backend);
        c.state.fill = color("red");
        c.fill_rect(0.0, 0.0, 10.0, 10.0);
        assert_eq!(pixel(&mut c, 5, 5), [255, 0, 0, 255], "{backend:?}");
        assert_eq!(pixel(&mut c, 15, 15), [0, 0, 0, 0]);
        c.save();
        c.state.transform = c.state.transform.pre_translate(10.0, 10.0);
        c.state.fill = Style::Color(parse_color("#00ff0080").unwrap(), String::new());
        c.fill_rect(0.0, 0.0, 5.0, 5.0);
        c.restore();
        let p = pixel(&mut c, 12, 12);
        assert_eq!((p[1], p[3]), (255, 128), "{backend:?}");
        c.clear_rect(0.0, 0.0, 5.0, 5.0);
        assert_eq!(pixel(&mut c, 2, 2), [0, 0, 0, 0]);
        // A path: a triangle filled.
        let t = c.state.transform;
        c.path.move_to(&t, 0.0, 19.0);
        c.path.line_to(&t, 19.0, 19.0);
        c.path.line_to(&t, 19.0, 0.0);
        c.path.close();
        c.state.fill = Style::Color(tiny_skia::Color::from_rgba8(0, 0, 255, 255), String::new());
        c.fill(None, false);
        assert_eq!(pixel(&mut c, 18, 17), [0, 0, 255, 255], "{backend:?}");
        assert!(c.is_point_in_path(None, 18.0, 17.0, false));
        assert!(!c.is_point_in_path(None, 1.0, 1.0, false));
        // Pixels round-trip.
        c.put_image_data(&[1, 2, 3, 255], 1, 1, 0, 0, (0, 0, 1, 1));
        assert_eq!(pixel(&mut c, 0, 0), [1, 2, 3, 255], "{backend:?}");
    }
    assert_eq!(
        serialize_color(parse_color("rgb(1, 2, 3)").unwrap()),
        "#010203"
    );
    assert_eq!(
        serialize_color(parse_color("rgba(1, 2, 3, 0.5)").unwrap()),
        "rgba(1, 2, 3, 0.502)"
    );
    assert_eq!(font_size("bold 12pt Arial"), 16.0);
}

#[test]
fn arcs_and_frames() {
    for backend in backends() {
        let mut c = Canvas2D::with_backend(40, 40, backend);
        let t = c.state.transform;
        c.path.ellipse(
            &t,
            20.0,
            20.0,
            10.0,
            10.0,
            0.0,
            0.0,
            std::f32::consts::TAU,
            false,
        );
        assert!(matches!(c.path.shape(), PathShape::Circle { .. }));
        c.fill(None, false);
        assert_eq!(pixel(&mut c, 20, 20), [0, 0, 0, 255], "{backend:?}");
        assert_eq!(pixel(&mut c, 2, 2), [0, 0, 0, 0]);
        let f = c.frame().unwrap();
        assert_eq!((f.width, f.height), (40, 40));
        let back = pixmap_from_frame(&f).unwrap();
        assert_eq!(back.width(), 40);
        assert_eq!(back.pixel(20, 20).unwrap().alpha(), 255);
    }
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "needs the GPU canvas: Windows only until Linux stage 5"
)]
fn circles_as_sprites_cover_what_the_circles_do() {
    // Chart.js's points: small filled circles and rings at fractional positions, drawn as sprites.
    // Each pixel's alpha is about the circle's exact area in it (supersampled here): edge pixels are
    // within a quarter of full scale (Direct2D's own antialiasing of the same circle as a path is off
    // by up to about 45 levels), and the mean error is small.
    let mut c = Canvas2D::with_backend(140, 60, GPU);
    let mut circles = Vec::new();
    for i in 0..24 {
        let x = 8.0 + (i % 12) as f32 * 11.0 + i as f32 * 0.137;
        let y = 12.0 + (i / 12) as f32 * 25.0 + i as f32 * 0.071;
        let r = 2.5 + (i % 4) as f32;
        let ring = i % 2 == 1;
        circles.push((x, y, r, ring));
        c.path = CanvasPath::new();
        let t = c.state.transform;
        c.path
            .ellipse(&t, x, y, r, r, 0.0, 0.0, std::f32::consts::TAU, false);
        c.path.close();
        if ring {
            c.state.stroke = color("black");
            c.stroke(None);
        } else {
            c.state.fill = color("black");
            c.fill(None, false);
        }
    }
    let data = c.get_image_data(0, 0, 140, 60);
    let mut worst = 0i32;
    let mut total = 0i64;
    for py in 0..60 {
        for px in 0..140 {
            let mut covered = 0;
            const N: i32 = 16;
            for sy in 0..N {
                for sx in 0..N {
                    let (x, y) = (
                        px as f32 + (sx as f32 + 0.5) / N as f32,
                        py as f32 + (sy as f32 + 0.5) / N as f32,
                    );
                    let inside = circles.iter().any(|&(cx, cy, r, ring)| {
                        let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                        if ring {
                            (r - 0.5..=r + 0.5).contains(&d)
                        } else {
                            d <= r
                        }
                    });
                    covered += i32::from(inside);
                }
            }
            let expected = covered * 255 / (N * N);
            let got = data[((py * 140 + px) * 4 + 3) as usize] as i32;
            worst = worst.max((expected - got).abs());
            total += (expected - got).abs() as i64;
        }
    }
    assert!(worst <= 64, "worst {worst}");
    assert!(total / (140 * 60) <= 2, "mean {}", total / (140 * 60));
}

#[test]
fn paths_gradients_patterns_and_strokes_match_the_cpu_rasterization() {
    let pattern = {
        let mut pm = Pixmap::new(4, 4).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(0, 128, 0, 255));
        for x in 0..2 {
            for y in 0..2 {
                pm.pixels_mut()[y * 4 + x] =
                    tiny_skia::ColorU8::from_rgba(200, 0, 0, 255).premultiply();
            }
        }
        Arc::new(Pattern {
            image: Arc::new(pm),
            repeat_x: true,
            repeat_y: true,
        })
    };
    let (cpu, gpu) = both(160, 120, |c| {
        // A linear gradient rectangle under a rotation.
        c.save();
        c.state.transform = Transform::from_translate(40.0, 30.0).pre_rotate(20.0);
        let mut g = Gradient {
            kind: GradientKind::Linear {
                x0: -30.0,
                y0: 0.0,
                x1: 30.0,
                y1: 0.0,
            },
            stops: Vec::new(),
        };
        g.stops.push((0.0, parse_color("blue").unwrap()));
        g.stops.push((1.0, parse_color("yellow").unwrap()));
        c.state.fill = Style::Gradient(Arc::new(g));
        c.fill_rect(-30.0, -20.0, 60.0, 40.0);
        c.restore();
        // A pattern in a star, even-odd.
        c.state.fill = Style::Pattern(pattern.clone());
        let t = c.state.transform;
        c.path = CanvasPath::new();
        c.path.move_to(&t, 110.0, 5.0);
        c.path.line_to(&t, 135.0, 55.0);
        c.path.line_to(&t, 85.0, 25.0);
        c.path.line_to(&t, 140.0, 25.0);
        c.path.line_to(&t, 90.0, 55.0);
        c.path.close();
        c.fill(None, true);
        // Thick strokes with joins and caps, scaled, and a dashed curve.
        c.save();
        c.state.transform = Transform::from_scale(1.5, 1.5);
        c.state.line_width = 6.0;
        c.state.line_join = LineJoin::Round;
        c.state.line_cap = LineCap::Square;
        c.state.stroke = color("purple");
        let t = c.state.transform;
        c.path = CanvasPath::new();
        c.path.move_to(&t, 7.0, 53.0);
        c.path.line_to(&t, 20.0, 47.0);
        c.path.line_to(&t, 30.0, 67.0);
        c.stroke(None);
        c.restore();
        c.state.line_width = 3.0;
        c.state.dash = vec![8.0, 4.0];
        c.state.stroke = color("black");
        let t = c.state.transform;
        c.path = CanvasPath::new();
        c.path.move_to(&t, 100.0, 110.0);
        c.path.cubic_to(&t, 110.0, 60.0, 140.0, 120.0, 155.0, 70.0);
        c.stroke(None);
    });
    let inked = cpu.chunks(4).filter(|p| p[3] > 0).count();
    assert!(inked > 4000, "{inked}");
    assert!(
        differing(&cpu, &gpu, 48) <= inked / 25,
        "{} of {inked}",
        differing(&cpu, &gpu, 48)
    );
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "needs the GPU canvas: Windows only until Linux stage 5"
)]
fn strokes_transform_with_the_current_matrix() {
    // HTML: a stroke is drawn in user space; under scale(2, 1) a 6-unit line is 6 px tall when
    // horizontal and 12 px wide when vertical.
    let mut c = Canvas2D::with_backend(60, 60, GPU);
    c.state.transform = Transform::from_scale(2.0, 1.0);
    c.state.line_width = 6.0;
    c.state.stroke = color("black");
    let t = c.state.transform;
    c.path.move_to(&t, 2.0, 10.0);
    c.path.line_to(&t, 12.0, 10.0);
    c.path.move_to(&t, 20.0, 20.0);
    c.path.line_to(&t, 20.0, 50.0);
    c.stroke(None);
    let column: Vec<u8> = (0..30).map(|y| pixel(&mut c, 10, y)[3]).collect();
    assert_eq!(column.iter().filter(|a| **a > 128).count(), 6, "{column:?}");
    let row: Vec<u8> = (25..55).map(|x| pixel(&mut c, x, 35)[3]).collect();
    assert_eq!(row.iter().filter(|a| **a > 128).count(), 12, "{row:?}");
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "needs the GPU canvas: Windows only until Linux stage 5"
)]
fn compositing_follows_the_porter_duff_and_blend_equations() {
    // A blue square, then within a circular clip a red square with the operator. Expected values
    // per HTML (the operator applies to the whole clip region, inside the shape or not) and
    // Compositing and Blending 1, in premultiplied colour.
    let blue = [0.0, 0.0, 0.75, 0.75];
    let red = [0.6, 0.0, 0.0, 0.6];
    let none = [0.0; 4];
    // (point, destination, source, inside the clip)
    let points = [
        ((30, 30), blue, red, true),
        ((15, 15), blue, none, true),
        ((42, 35), none, red, true),
        ((6, 6), blue, none, false),
        ((3, 57), none, none, false),
    ];
    for op in [
        "source-over",
        "source-in",
        "source-out",
        "source-atop",
        "destination-over",
        "destination-in",
        "destination-out",
        "destination-atop",
        "lighter",
        "copy",
        "xor",
        "multiply",
        "screen",
        "darken",
        "lighten",
        "difference",
    ] {
        let mut c = Canvas2D::with_backend(60, 60, GPU);
        c.state.fill = color("rgba(0, 0, 255, 0.75)");
        c.fill_rect(5.0, 5.0, 35.0, 35.0);
        let t = c.state.transform;
        c.path = CanvasPath::new();
        c.path.ellipse(
            &t,
            30.0,
            30.0,
            22.0,
            22.0,
            0.0,
            0.0,
            std::f32::consts::TAU,
            false,
        );
        c.clip(None, false);
        c.state.composite = blend_mode(op).unwrap();
        c.state.fill = color("rgba(255, 0, 0, 0.6)");
        c.fill_rect(20.0, 20.0, 35.0, 35.0);
        for ((x, y), d, s, inside) in points {
            let expected = if inside { composite(op, d, s) } else { d };
            let got = c.get_image_data(x, y, 1, 1);
            let a = got[3] as f32 / 255.0;
            let got = [
                got[0] as f32 / 255.0 * a,
                got[1] as f32 / 255.0 * a,
                got[2] as f32 / 255.0 * a,
                a,
            ];
            assert!(
                got.iter()
                    .zip(expected.iter())
                    .all(|(g, e)| (g - e).abs() <= 3.0 / 255.0),
                "{op} at ({x}, {y}): expected {expected:?}, got {got:?}"
            );
        }
    }
}

/// `op` of premultiplied source `s` over destination `d`.
fn composite(op: &str, d: [f32; 4], s: [f32; 4]) -> [f32; 4] {
    let (ad, as_) = (d[3], s[3]);
    let pd = |fa: f32, fb: f32| -> [f32; 4] {
        let mut o = [0.0; 4];
        for i in 0..4 {
            o[i] = (s[i] * fa + d[i] * fb).min(1.0);
        }
        o
    };
    let blend = |b: fn(f32, f32) -> f32| -> [f32; 4] {
        let mut o = [0.0; 4];
        for i in 0..3 {
            let cs = if as_ > 0.0 { s[i] / as_ } else { 0.0 };
            let cb = if ad > 0.0 { d[i] / ad } else { 0.0 };
            o[i] = s[i] * (1.0 - ad) + d[i] * (1.0 - as_) + as_ * ad * b(cb, cs);
        }
        o[3] = as_ + ad * (1.0 - as_);
        o
    };
    match op {
        "source-over" => pd(1.0, 1.0 - as_),
        "source-in" => pd(ad, 0.0),
        "source-out" => pd(1.0 - ad, 0.0),
        "source-atop" => pd(ad, 1.0 - as_),
        "destination-over" => pd(1.0 - ad, 1.0),
        "destination-in" => pd(0.0, as_),
        "destination-out" => pd(0.0, 1.0 - as_),
        "destination-atop" => pd(1.0 - ad, as_),
        "lighter" => pd(1.0, 1.0),
        "copy" => pd(1.0, 0.0),
        "xor" => pd(1.0 - ad, 1.0 - as_),
        "multiply" => blend(|b, s| b * s),
        "screen" => blend(|b, s| b + s - b * s),
        "darken" => blend(f32::min),
        "lighten" => blend(f32::max),
        "difference" => blend(|b, s| (b - s).abs()),
        _ => unreachable!(),
    }
}

/// `drawImage` into an axis-aligned destination at fractional coordinates covers whole pixels,
/// those whose centres it covers, so images drawn edge to edge leave no blended seam. Rotated, or
/// less than a pixel across, its edges are antialiased.
#[test]
fn axis_aligned_images_cover_whole_pixels() {
    for backend in backends() {
        let mut src = Canvas2D::with_backend(10, 10, backend);
        src.state.fill = color("red");
        src.fill_rect(0.0, 0.0, 10.0, 10.0);
        let image = src.snapshot().unwrap();
        let mut c = Canvas2D::with_backend(60, 40, backend);
        // Two cells edge to edge at x = 10.3 and 20.3.
        c.draw_image(&image, (0.0, 0.0, 10.0, 10.0), (10.3, 0.0, 10.0, 10.0));
        c.draw_image(&image, (0.0, 0.0, 10.0, 10.0), (20.3, 0.0, 10.0, 10.0));
        for x in [10, 19, 20, 29] {
            assert_eq!(pixel(&mut c, x, 5), [255, 0, 0, 255], "{backend:?} x {x}");
        }
        assert_eq!(pixel(&mut c, 30, 5)[3], 0, "{backend:?}");
        assert_eq!(pixel(&mut c, 9, 5)[3], 0, "{backend:?}");
        // Under a pixel wide: antialiased, so it does not drop out.
        c.draw_image(&image, (0.0, 0.0, 10.0, 10.0), (40.25, 0.0, 0.5, 10.0));
        let a = pixel(&mut c, 40, 5)[3];
        assert!(a > 64 && a < 192, "{backend:?} {a}");
        // Rotated a little: the edges blend.
        c.state.transform = Transform::from_rotate_at(10.0, 15.0, 30.0);
        c.draw_image(&image, (0.0, 0.0, 10.0, 10.0), (10.0, 25.0, 10.0, 10.0));
        let data = c.get_image_data(0, 20, 30, 20);
        let partial = data.chunks(4).filter(|p| p[3] > 0 && p[3] < 255).count();
        assert!(partial > 4, "{backend:?} {partial}");
    }
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "needs the GPU canvas: Windows only until Linux stage 5"
)]
fn images_text_and_shadows_draw_on_the_gpu() {
    let fonts = fonts();
    let mut c = Canvas2D::with_backend(200, 100, GPU);
    // drawImage of another canvas, scaled, without smoothing.
    let mut src = Canvas2D::with_backend(4, 4, GPU);
    src.state.fill = color("lime");
    src.fill_rect(0.0, 0.0, 2.0, 4.0);
    let image = src.snapshot().unwrap();
    c.state.image_smoothing = false;
    c.draw_image(&image, (0.0, 0.0, 4.0, 4.0), (10.0, 10.0, 40.0, 40.0));
    assert_eq!(pixel(&mut c, 15, 30), [0, 255, 0, 255]);
    assert_eq!(pixel(&mut c, 45, 30), [0, 0, 0, 0]);
    // A source rectangle beyond the image is clipped to it, the destination in proportion.
    c.draw_image(&image, (0.0, 0.0, 8.0, 4.0), (60.0, 10.0, 40.0, 20.0));
    assert_eq!(pixel(&mut c, 65, 15), [0, 255, 0, 255]);
    assert_eq!(pixel(&mut c, 95, 15), [0, 0, 0, 0]);
    // Text, centred, with a shadow below it.
    c.state.font = "bold 20px Arial, sans-serif".into();
    c.state.text_align = "center".into();
    c.state.text_baseline = "middle".into();
    c.state.fill = color("black");
    c.state.shadow_color = "rgba(255, 0, 0, 1)".into();
    c.state.shadow_offset = (0.0, 20.0);
    c.draw_text(&fonts, "Snowlark", 150.0, 40.0, false);
    let (w, ascent, descent) = c.measure_text(&fonts, "Snowlark");
    assert!(w > 50.0 && w < 90.0, "{w}");
    assert!(ascent > 15.0 && descent > 3.0, "{ascent} {descent}");
    let data = c.get_image_data(100, 25, 100, 70);
    let black = data
        .chunks(4)
        .filter(|p| p[3] > 200 && p[0] < 60 && p[1] < 60)
        .count();
    let red = data
        .chunks(4)
        .filter(|p| p[3] > 200 && p[0] > 200 && p[1] < 60)
        .count();
    assert!(black > 100 && red > 100, "{black} {red}");
    // The text's ink is centred on x = 150.
    let ink_x: Vec<usize> = data
        .chunks(4)
        .enumerate()
        .filter(|(_, p)| p[3] > 200 && p[0] < 60)
        .map(|(i, _)| i % 100)
        .collect();
    let mid = (ink_x.iter().min().unwrap() + ink_x.iter().max().unwrap()) / 2;
    assert!((45..=55).contains(&mid), "{mid}");
}

#[test]
fn colours_parse_fast_as_svgtypes_parses_them() {
    for text in [
        "#ff6384",
        "#0f8",
        "rgb(54, 162, 235)",
        "rgba(255, 99, 132, 0.5)",
        "rgba(255,99,132,.25)",
        "rgba(1, 2, 3, 0.502)",
        "rgba(0, 0, 0, 0.1)",
        "rgb(300, 0, 0)",
        "rgba(10, 20, 30, 1)",
        "rgba(10.5, 20.49, 30, 0.333)",
    ] {
        let fast = fast_color(text).expect(text);
        let c: svgtypes::Color = text.parse().unwrap();
        assert_eq!(
            fast.to_color_u8(),
            tiny_skia::ColorU8::from_rgba(c.red, c.green, c.blue, c.alpha),
            "{text}"
        );
    }
    // Other forms go through svgtypes.
    for text in ["red", "rgb(10%, 20%, 30%)", "hsl(120, 50%, 50%)"] {
        assert!(fast_color(text).is_none(), "{text}");
        assert!(parse_color(text).is_some(), "{text}");
    }
    assert!(fast_color("rgb(1 2 3 / 0.5)").is_none());
    assert!(parse_color("rgb(1, 2)").is_none());
}

/// Canvas text goes through the document's shaping cache: a text measured or drawn before (by any
/// canvas of the document's origin) is a lookup, with the same result as shaping it again.
#[test]
#[cfg_attr(
    not(windows),
    ignore = "needs the GPU canvas: Windows only until Linux stage 5"
)]
fn canvas_text_shaped_before_is_found_in_the_shared_cache() {
    let fonts = fonts();
    // (a character the first font lacks: shaping it needs font fallback)
    let text = "\u{2708} Lisbon (LIS) \u{2192} Oslo (OSL): 2771 km";
    let font = "12px 'Helvetica Neue', 'Helvetica', 'Arial', sans-serif";
    let mut a = Canvas2D::with_backend(100, 50, GPU);
    a.state.font = font.into();
    let first = a.measure_text(&fonts, text);
    let hits = fonts.cache.lock().unwrap().runs.hits;
    a.state.fill = color("black");
    a.draw_text(&fonts, text, 0.0, 20.0, false);
    let mut b = Canvas2D::with_backend(100, 50, GPU);
    b.state.font = font.into();
    let again = b.measure_text(&fonts, text);
    assert_eq!(fonts.cache.lock().unwrap().runs.hits, hits + 2);
    assert_eq!(first, again);
    // The same as shaping it with nothing cached.
    let fresh = Fonts {
        cache: Default::default(),
        ..fonts.clone()
    };
    let mut c = Canvas2D::with_backend(100, 50, GPU);
    c.state.font = font.into();
    assert_eq!(c.measure_text(&fresh, text), first);
    assert!(first.0 > 100.0, "{first:?}");
}

#[test]
fn fonts_parse_as_the_shorthand() {
    let f = parse_font("italic small-caps bold 12pt/1.5 \"Helvetica Neue\", Arial, sans-serif");
    assert_eq!(
        f.families,
        [
            FamilyName::Named("Helvetica Neue".into()),
            FamilyName::Named("Arial".into()),
            FamilyName::Generic("sans-serif".into())
        ]
    );
    // A quoted keyword is a family name; an unquoted one is the generic family.
    let g = parse_font("12px \"serif\", Times New  Roman, SERIF");
    assert_eq!(
        g.families,
        [
            FamilyName::Named("serif".into()),
            FamilyName::Named("Times New Roman".into()),
            FamilyName::Generic("serif".into())
        ]
    );
    assert_eq!((f.weight, f.style, f.size), (700, FontStyle::Italic, 16.0));
    let f = parse_font("600 2em serif");
    assert_eq!((f.weight, f.size), (600, 32.0));
    let f = parse_font("nonsense");
    assert_eq!(
        (&f.families[0], f.size),
        (&FamilyName::Generic("sans-serif".into()), 10.0)
    );
}
