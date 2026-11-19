// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SVG filter references (Filter Effects 1 §9) rastered headless and on GPU tiles: the region and
//! subregions clip, results flow by index, the color spaces convert un-premultiplied colors, and a
//! filter without primitives changes nothing.

use std::sync::Arc;

use snowlark_paint::svg_filter::*;
use snowlark_paint::*;

const W: u32 = 120;
const H: u32 = 120;
/// Gray, so that a transparent part that came out white or black shows.
const BG: Color = Color::rgb(200, 200, 200);
const RED: Color = Color::rgb(255, 0, 0);
/// The filter region: 10 px around the content.
const REGION: Rect = Rect::new(30.0, 30.0, 60.0, 60.0);

fn close(a: Color, b: Color, tolerance: i32) -> bool {
    (a.r as i32 - b.r as i32).abs() <= tolerance
        && (a.g as i32 - b.g as i32).abs() <= tolerance
        && (a.b as i32 - b.b as i32).abs() <= tolerance
        && (a.a as i32 - b.a as i32).abs() <= tolerance
}

fn prim(kind: Kind, inputs: Vec<Input>, linear_rgb: bool) -> Primitive {
    Primitive {
        kind,
        inputs,
        subregion: REGION,
        linear_rgb,
    }
}

/// A 40 px square of `content` at (40, 40) on gray, filtered by `primitives` over [`REGION`].
fn page(primitives: Vec<Primitive>, content: Color) -> DisplayList {
    let ops = [FilterOp::Svg(Arc::new(SvgFilter {
        region: REGION,
        primitives,
    }))];
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, W as f32, H as f32), BG);
    b.push_filter(&ops, REGION);
    b.fill_rect(Rect::new(40.0, 40.0, 40.0, 40.0), content);
    b.pop_filter();
    b.build()
}

fn render(list: &DisplayList) -> [Pixmap; 2] {
    render_with(list, &ImageStore::new())
}

fn render_with(list: &DisplayList, images: &ImageStore) -> [Pixmap; 2] {
    let text = TextEngine::new().unwrap();
    let sources = Sources {
        text: &text,
        images,
    };
    let view = Viewport {
        width: W,
        height: H,
        scale: 1.0,
        background: BG,
    };
    let a = HeadlessRenderer::new()
        .unwrap()
        .render(list, sources, &view)
        .unwrap();
    let mut tiles = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    let frame = tiles.raster(list, sources, &view).unwrap();
    let b = tiles.read_back(&frame).unwrap();
    [a, b]
}

fn check(primitives: Vec<Primitive>, content: Color, probes: &[(u32, u32, Color, i32)]) {
    let list = page(primitives, content);
    assert!(list.validate().is_ok());
    for (what, pm) in ["headless", "tiles"].into_iter().zip(render(&list)) {
        for &(x, y, expected, tolerance) in probes {
            let got = pm.pixel(x, y);
            assert!(
                close(got, expected, tolerance),
                "{what}: ({x}, {y}) is {got:?}, expected {expected:?}"
            );
        }
    }
}

/// A flood fills its subregion, cut to the region, whatever the content; in linear light its
/// color comes out as written.
#[test]
fn a_flood_fills_its_subregion() {
    let green = Color::rgb(0, 128, 0);
    let mut flood = prim(Kind::Flood(green), vec![], true);
    flood.subregion = Rect::new(20.0, 50.0, 200.0, 20.0);
    check(
        vec![flood],
        RED,
        &[
            (35, 55, green, 2),
            (85, 65, green, 2),
            // (outside the subregion, in the content)
            (60, 45, BG, 0),
            // (outside the region)
            (25, 55, BG, 0),
            (95, 55, BG, 0),
        ],
    );
    // a flood's opacity: half-transparent blue over gray
    check(
        vec![prim(
            Kind::Flood(Color::rgba(0, 0, 255, 128)),
            vec![],
            false,
        )],
        RED,
        &[(35, 35, Color::rgb(100, 100, 228), 2)],
    );
}

/// An offset moves the content, a later primitive reads the earlier one's result by index, and
/// a merge puts its inputs over one another.
#[test]
fn results_flow_by_index() {
    let blue = Color::rgb(0, 0, 255);
    check(
        vec![
            prim(Kind::Flood(blue), vec![], false),
            prim(
                Kind::Offset { dx: 10.0, dy: 0.0 },
                vec![Input::SourceGraphic],
                false,
            ),
            prim(Kind::Merge, vec![Input::Result(0), Input::Result(1)], false),
        ],
        RED,
        &[
            (45, 60, blue, 0),
            (85, 60, RED, 0),
            (60, 35, blue, 0),
            (95, 60, BG, 0),
        ],
    );
}

/// A color matrix in linear light works on linear values of the un-premultiplied colors: half
/// of full red in linear light is sRGB 188, in sRGB 128; a half-transparent content keeps its
/// colors through an identity in linear light.
#[test]
fn color_spaces_convert_unpremultiplied_colors() {
    let mut half = [[0.0; 5]; 4];
    half[0][0] = 0.5;
    half[3][3] = 1.0;
    check(
        vec![prim(
            Kind::ColorMatrix(half),
            vec![Input::SourceGraphic],
            true,
        )],
        RED,
        &[(60, 60, Color::rgb(188, 0, 0), 3), (35, 35, BG, 0)],
    );
    check(
        vec![prim(
            Kind::ColorMatrix(half),
            vec![Input::SourceGraphic],
            false,
        )],
        RED,
        &[(60, 60, Color::rgb(128, 0, 0), 2)],
    );
    let mut identity = [[0.0; 5]; 4];
    for (i, row) in identity.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    // (half-transparent red over gray: 228, 100, 100)
    check(
        vec![prim(
            Kind::ColorMatrix(identity),
            vec![Input::SourceGraphic],
            true,
        )],
        Color::rgba(255, 0, 0, 128),
        &[(60, 60, Color::rgb(228, 100, 100), 3), (35, 35, BG, 0)],
    );
}

/// `in` composites the flood where the content is; `SourceAlpha` is black.
#[test]
fn composite_and_source_alpha() {
    let green = Color::rgb(0, 128, 0);
    check(
        vec![
            prim(Kind::Flood(green), vec![], false),
            prim(
                Kind::Composite {
                    op: CompositeOp::In,
                    k: [0.0; 4],
                },
                vec![Input::Result(0), Input::SourceGraphic],
                false,
            ),
        ],
        RED,
        &[(60, 60, green, 1), (35, 35, BG, 0)],
    );
    check(
        vec![prim(
            Kind::Offset { dx: 0.0, dy: 0.0 },
            vec![Input::SourceAlpha],
            false,
        )],
        RED,
        &[(60, 60, Color::BLACK, 0), (35, 35, BG, 0)],
    );
}

/// Each kind of transfer function on its channel: discrete steps, a table, a line, a gamma.
#[test]
fn component_transfer_applies_each_kind() {
    let f = |kind, table: &[f32]| Transfer {
        kind,
        table: table.to_vec(),
        ..Transfer::default()
    };
    let funcs = [
        f(TransferKind::Discrete, &[0.2, 0.8]),
        f(TransferKind::Table, &[1.0, 0.0]),
        Transfer {
            kind: TransferKind::Linear,
            slope: 0.0,
            intercept: 0.5,
            ..Transfer::default()
        },
        Transfer {
            kind: TransferKind::Gamma,
            amplitude: 1.0,
            exponent: 1.0,
            offset: 0.0,
            ..Transfer::default()
        },
    ];
    check(
        vec![prim(
            Kind::ComponentTransfer(Box::new(funcs)),
            vec![Input::SourceGraphic],
            false,
        )],
        RED,
        &[(60, 60, Color::rgb(204, 255, 128), 2), (35, 35, BG, 0)],
    );
}

/// A filter without primitives changes nothing.
#[test]
fn an_empty_filter_changes_nothing() {
    check(vec![], RED, &[(60, 60, RED, 0), (35, 35, BG, 0)]);
}

/// The display list carries a filter of every kind through its wire format.
#[test]
fn a_filter_of_every_kind_round_trips() {
    let mut b = DisplayListBuilder::new();
    let filter = SvgFilter {
        region: REGION,
        primitives: vec![
            prim(
                Kind::ConvolveMatrix {
                    order_x: 2,
                    order_y: 1,
                    kernel: vec![1.0, -1.0],
                    divisor: 1.0,
                    bias: 0.5,
                    target_x: 1,
                    target_y: 0,
                    edge: EdgeMode::None,
                    preserve_alpha: false,
                },
                vec![Input::SourceGraphic],
                true,
            ),
            prim(
                Kind::SpecularLighting {
                    surface_scale: 1.0,
                    constant: 1.0,
                    exponent: 4.0,
                    color: Color::rgb(1, 2, 3),
                    light: Light::Point {
                        x: 1.0,
                        y: 2.0,
                        z: 3.0,
                    },
                },
                vec![Input::Result(0)],
                false,
            ),
        ],
    };
    b.push_filter(&[FilterOp::Svg(Arc::new(filter))], REGION);
    b.pop_filter();
    let list = b.build();
    let mut bytes = Vec::new();
    list.encode(&mut bytes);
    let back = DisplayList::decode(&bytes).unwrap();
    assert_eq!(back.filters, list.filters);
    // (an input that names a later result is refused)
    let mut bad = list.clone();
    if let FilterOp::Svg(f) = &mut bad.filters[0] {
        Arc::make_mut(f).primitives[0].inputs = vec![Input::Result(1)];
    }
    assert!(bad.validate().is_err());
}

/// A 20 by 20 image, dark red (128, 0, 0) on the left half and blue on the right (premultiplied
/// BGRA rows).
fn halves(images: &ImageStore) -> ImageId {
    let mut pixels = Vec::with_capacity(20 * 20 * 4);
    for _ in 0..20 {
        for x in 0..20 {
            pixels.extend_from_slice(if x < 10 {
                &[0, 0, 128, 255]
            } else {
                &[255, 0, 0, 255]
            });
        }
    }
    images.add(DecodedImage::still(Frame {
        width: 20,
        height: 20,
        pixels,
        delay_ms: 0,
    }))
}

/// `feImage` (§9.15): the image (or the part of it named) drawn into its destination, scaled,
/// in sRGB whatever the primitive's space, and transparent black elsewhere in the subregion;
/// an image that is not there draws nothing.
#[test]
fn an_image_primitive_draws_its_image_into_its_destination() {
    let blue = Color::rgb(0, 0, 255);
    let dark_red = Color::rgb(128, 0, 0);
    let images = ImageStore::new();
    let id = halves(&images);
    let probe = |src: Rect, expected: &[(u32, u32, Color)]| {
        let list = page(
            vec![prim(
                Kind::Image {
                    image: id,
                    dest: Rect::new(40.0, 40.0, 40.0, 40.0),
                    src,
                },
                vec![],
                true,
            )],
            Color::rgb(0, 128, 0),
        );
        assert!(list.validate().is_ok());
        for (what, pm) in ["headless", "tiles"]
            .into_iter()
            .zip(render_with(&list, &images))
        {
            for &(x, y, color) in expected {
                let got = pm.pixel(x, y);
                assert!(
                    close(got, color, 2),
                    "{what}: ({x}, {y}) is {got:?}, expected {color:?}"
                );
            }
        }
    };
    // (the whole image: its colors as they are, though the primitive says linearRGB, the content
    // replaced; outside the destination, within the region, transparent)
    probe(
        Rect::ZERO,
        &[
            (45, 60, dark_red),
            (75, 60, blue),
            (35, 60, BG),
            (60, 85, BG),
        ],
    );
    // (its right half, stretched over the destination)
    probe(
        Rect::new(10.0, 0.0, 10.0, 20.0),
        &[(45, 60, blue), (75, 60, blue)],
    );
    // An image the store does not have: transparent black over the subregion.
    let list = page(
        vec![prim(
            Kind::Image {
                image: ImageId(999),
                dest: Rect::new(40.0, 40.0, 40.0, 40.0),
                src: Rect::ZERO,
            },
            vec![],
            false,
        )],
        RED,
    );
    for pm in render_with(&list, &images) {
        assert!(close(pm.pixel(60, 60), BG, 0));
    }
}
