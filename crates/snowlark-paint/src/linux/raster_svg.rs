// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An SVG `<filter>` (Filter Effects 1 §9) as a graph of Skia image filters: each primitive one
//! filter reading the earlier ones' results, cropped to its subregion, the whole cropped to the
//! filter region. Primitives that compute on colors do so in linear light unless
//! `color-interpolation-filters: sRGB` (§10.6): their inputs are converted to it and their
//! result back.

use skia_safe as sk;

use super::Painter;
use crate::svg_filter::{Channel, CompositeOp, EdgeMode, Input, Kind, Light, SvgFilter};
use crate::Rect;

fn crop(r: &Rect) -> sk::image_filters::CropRect {
    sk::image_filters::CropRect::from(super::rect(r))
}

/// The filter's graph applied to `source` (`None`: the content), or `None` when it cannot be made.
pub(super) fn filter(
    svg: &SvgFilter,
    _bounds: &Rect,
    source: Option<sk::ImageFilter>,
    painter: &mut Painter,
) -> Option<sk::ImageFilter> {
    let region = svg.region;
    // (nothing of the input shows outside the filter region)
    let source = sk::image_filters::crop(super::rect(&region), sk::TileMode::Decal, source)?;
    let alpha_only = || {
        sk::image_filters::color_filter(
            super::filters::color_matrix(&[
                [0.0, 0.0, 0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 1.0, 0.0],
            ]),
            source.clone(),
            None,
        )
    };
    let mut results: Vec<(sk::ImageFilter, Rect)> = Vec::with_capacity(svg.primitives.len());
    for prim in &svg.primitives {
        let sub = prim.subregion;
        let input = |n: usize| -> Option<sk::ImageFilter> {
            match prim.inputs.get(n).copied().unwrap_or(Input::SourceGraphic) {
                Input::SourceGraphic => Some(source.clone()),
                Input::SourceAlpha => alpha_only(),
                Input::Transparent => Some(sk::image_filters::empty()),
                Input::Result(i) => results.get(i as usize).map(|r| r.0.clone()),
            }
        };
        let linear = prim.linear_rgb
            && !matches!(
                prim.kind,
                Kind::Flood(_) | Kind::Image { .. } | Kind::Offset { .. } | Kind::Tile
            );
        let operand = |n: usize| -> Option<sk::ImageFilter> {
            let f = input(n)?;
            if linear {
                sk::image_filters::color_filter(sk::color_filters::srgb_to_linear_gamma(), f, None)
            } else {
                Some(f)
            }
        };
        let c = crop(&sub);
        let result = match &prim.kind {
            Kind::Flood(color) => {
                sk::image_filters::shader(sk::shaders::color(super::color(*color)), c)
            }
            Kind::ColorMatrix(m) => {
                sk::image_filters::color_filter(super::filters::color_matrix(m), operand(0)?, c)
            }
            Kind::ComponentTransfer(f) => {
                let table = |n: usize| -> [u8; 256] {
                    std::array::from_fn(|i| {
                        (f[n].eval(i as f32 / 255.0).clamp(0.0, 1.0) * 255.0).round() as u8
                    })
                };
                let (r, g, b, a) = (table(0), table(1), table(2), table(3));
                let cf = sk::color_filters::table_argb(&a, &r, &g, &b)?;
                sk::image_filters::color_filter(cf, operand(0)?, c)
            }
            Kind::GaussianBlur { x, y } => sk::image_filters::blur(
                (x.max(0.0), y.max(0.0)),
                sk::TileMode::Decal,
                operand(0)?,
                c,
            ),
            Kind::Offset { dx, dy } => sk::image_filters::offset((*dx, *dy), operand(0)?, c),
            Kind::Merge => {
                let n = prim.inputs.len();
                let inputs: Vec<Option<sk::ImageFilter>> = (0..n).map(operand).collect();
                sk::image_filters::merge(inputs, c)
            }
            Kind::Blend(mode) => {
                sk::image_filters::blend(blend_mode(*mode), operand(1)?, operand(0)?, c)
            }
            Kind::Composite { op, k } => match op {
                CompositeOp::Arithmetic => sk::image_filters::arithmetic(
                    k[0],
                    k[1],
                    k[2],
                    k[3],
                    true,
                    operand(1)?,
                    operand(0)?,
                    c,
                ),
                op => sk::image_filters::blend(
                    match op {
                        CompositeOp::In => sk::BlendMode::SrcIn,
                        CompositeOp::Out => sk::BlendMode::SrcOut,
                        CompositeOp::Atop => sk::BlendMode::SrcATop,
                        CompositeOp::Xor => sk::BlendMode::Xor,
                        CompositeOp::Lighter => sk::BlendMode::Plus,
                        _ => sk::BlendMode::SrcOver,
                    },
                    operand(1)?,
                    operand(0)?,
                    c,
                ),
            },
            Kind::DropShadow {
                dx,
                dy,
                x,
                y,
                color,
            } => sk::image_filters::drop_shadow(
                (*dx, *dy),
                (x.max(0.0), y.max(0.0)),
                sk::Color4f::from(super::color(*color)),
                None,
                operand(0)?,
                c,
            ),
            Kind::Morphology { dilate, rx, ry } => {
                if *rx < 0.0 || *ry < 0.0 {
                    // (a negative radius disables the effect: transparent black, §9.20)
                    Some(sk::image_filters::empty())
                } else if *dilate {
                    sk::image_filters::dilate((*rx, *ry), operand(0)?, c)
                } else {
                    sk::image_filters::erode((*rx, *ry), operand(0)?, c)
                }
            }
            Kind::Turbulence {
                base_x,
                base_y,
                octaves,
                seed,
                stitch,
                fractal_noise,
            } => {
                let tile = stitch.then(|| sk::ISize::new(sub.width as i32, sub.height as i32));
                let shader = if *fractal_noise {
                    sk::Shader::fractal_perlin_noise(
                        (*base_x, *base_y),
                        *octaves as usize,
                        *seed,
                        tile,
                    )
                } else {
                    sk::Shader::turbulence_perlin_noise(
                        (*base_x, *base_y),
                        *octaves as usize,
                        *seed,
                        tile,
                    )
                }?;
                // (the noise is generated where the subregion starts)
                let shader = shader.with_local_matrix(&sk::Matrix::translate((sub.x, sub.y)));
                sk::image_filters::shader(shader, c)
            }
            Kind::Tile => {
                let from = match prim.inputs.first() {
                    Some(Input::Result(i)) => results.get(*i as usize).map(|r| r.1),
                    _ => Some(region),
                }?;
                sk::image_filters::tile(super::rect(&from), super::rect(&sub), operand(0)?)
            }
            Kind::ConvolveMatrix {
                order_x,
                order_y,
                kernel,
                divisor,
                bias,
                target_x,
                target_y,
                edge,
                preserve_alpha,
            } => sk::image_filters::matrix_convolution(
                sk::ISize::new(*order_x as i32, *order_y as i32),
                kernel,
                if *divisor != 0.0 { 1.0 / divisor } else { 1.0 },
                *bias,
                sk::IPoint::new(*target_x as i32, *target_y as i32),
                match edge {
                    EdgeMode::None => sk::TileMode::Decal,
                    EdgeMode::Duplicate => sk::TileMode::Clamp,
                    EdgeMode::Wrap => sk::TileMode::Repeat,
                    EdgeMode::Mirror => sk::TileMode::Mirror,
                },
                !preserve_alpha,
                operand(0)?,
                c,
            ),
            Kind::DisplacementMap { scale, x, y } => sk::image_filters::displacement_map(
                (channel(*x), channel(*y)),
                *scale,
                operand(1)?,
                operand(0)?,
                c,
            ),
            Kind::DiffuseLighting {
                surface_scale,
                constant,
                color,
                light,
            } => {
                let lc = super::color(*color);
                let input = operand(0)?;
                match *light {
                    Light::Distant { azimuth, elevation } => {
                        sk::image_filters::distant_lit_diffuse(
                            direction(azimuth, elevation),
                            lc,
                            *surface_scale,
                            *constant,
                            input,
                            c,
                        )
                    }
                    Light::Point { x, y, z } => sk::image_filters::point_lit_diffuse(
                        sk::Point3::new(x, y, z),
                        lc,
                        *surface_scale,
                        *constant,
                        input,
                        c,
                    ),
                    Light::Spot {
                        x,
                        y,
                        z,
                        at_x,
                        at_y,
                        at_z,
                        exponent,
                        cone,
                    } => sk::image_filters::spot_lit_diffuse(
                        sk::Point3::new(x, y, z),
                        sk::Point3::new(at_x, at_y, at_z),
                        exponent,
                        cone.unwrap_or(180.0),
                        lc,
                        *surface_scale,
                        *constant,
                        input,
                        c,
                    ),
                }
            }
            Kind::SpecularLighting {
                surface_scale,
                constant,
                exponent: shine,
                color,
                light,
            } => {
                let lc = super::color(*color);
                let input = operand(0)?;
                match *light {
                    Light::Distant { azimuth, elevation } => {
                        sk::image_filters::distant_lit_specular(
                            direction(azimuth, elevation),
                            lc,
                            *surface_scale,
                            *constant,
                            *shine,
                            input,
                            c,
                        )
                    }
                    Light::Point { x, y, z } => sk::image_filters::point_lit_specular(
                        sk::Point3::new(x, y, z),
                        lc,
                        *surface_scale,
                        *constant,
                        *shine,
                        input,
                        c,
                    ),
                    Light::Spot {
                        x,
                        y,
                        z,
                        at_x,
                        at_y,
                        at_z,
                        exponent,
                        cone,
                    } => sk::image_filters::spot_lit_specular(
                        sk::Point3::new(x, y, z),
                        sk::Point3::new(at_x, at_y, at_z),
                        exponent,
                        cone.unwrap_or(180.0),
                        lc,
                        *surface_scale,
                        *constant,
                        *shine,
                        input,
                        c,
                    ),
                }
            }
            Kind::Image { image, dest, src } => {
                let images = painter.images();
                match painter.caches().image(images, *image) {
                    Some(img) => {
                        let src = if src.is_empty() {
                            sk::Rect::from_wh(img.width() as f32, img.height() as f32)
                        } else {
                            super::rect(src)
                        };
                        sk::image_filters::image(
                            img,
                            Some(&src),
                            Some(&super::rect(dest)),
                            sk::SamplingOptions::new(sk::FilterMode::Linear, sk::MipmapMode::None),
                        )
                    }
                    None => Some(sk::image_filters::empty()),
                }
            }
        }?;
        let result = if linear {
            sk::image_filters::color_filter(
                sk::color_filters::linear_to_srgb_gamma(),
                result,
                None,
            )?
        } else {
            result
        };
        let result = sk::image_filters::crop(super::rect(&sub), sk::TileMode::Decal, result)?;
        results.push((result, sub));
    }
    match results.pop() {
        Some((last, _)) => sk::image_filters::crop(super::rect(&region), sk::TileMode::Decal, last),
        // (a filter without primitives: transparent black, §6)
        None => Some(sk::image_filters::empty()),
    }
}

/// `feBlend`'s mode by the index of its keyword (`normal` 0, Compositing 1 §10's order).
fn blend_mode(mode: u8) -> sk::BlendMode {
    use sk::BlendMode::*;
    [
        SrcOver, Multiply, Screen, Overlay, Darken, Lighten, ColorDodge, ColorBurn, HardLight,
        SoftLight, Difference, Exclusion, Hue, Saturation, Color, Luminosity,
    ]
    .get(mode as usize)
    .copied()
    .unwrap_or(SrcOver)
}

fn channel(c: Channel) -> sk::ColorChannel {
    match c {
        Channel::R => sk::ColorChannel::R,
        Channel::G => sk::ColorChannel::G,
        Channel::B => sk::ColorChannel::B,
        Channel::A => sk::ColorChannel::A,
    }
}

/// A distant light's direction (§9.20 `feDistantLight`): degrees.
fn direction(azimuth: f32, elevation: f32) -> sk::Point3 {
    let (a, e) = (azimuth.to_radians(), elevation.to_radians());
    sk::Point3::new(a.cos() * e.cos(), a.sin() * e.cos(), e.sin())
}
