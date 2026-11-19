// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Filter functions as a Direct2D effect graph: a [`FilterOp`] list applied to a device-pixel
//! image (a filter group's content recorded in a command list), the output of each effect the
//! input of the next.
//!
//! The color functions are one ColorMatrix effect each, applied to the un-premultiplied colors
//! with the output clamped and premultiplied again, as Filter Effects 1 §10 defines them through `feColorMatrix` and
//! `feComponentTransfer` (each primitive clamps its result). `blur()` is a GaussianBlur with the
//! standard deviation scaled to device pixels and transparent black outside the input (the
//! filter's input is transparent beyond the content). `drop-shadow()` is a Shadow effect (the
//! input's alpha blurred, in the color) moved by the offset through an affine transform, with the
//! input composited over it.

use windows::core::Result;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

use crate::filter::{FilterOp, MAX_SIGMA};
use crate::geom::{Color, Matrix, Rect};
use crate::ImageId;

/// The bitmaps of the images an SVG filter's `feImage`s draw, on the device of the context the
/// filter is applied with (`None`: the image is not there, and the primitive shows nothing).
pub(crate) type Images<'a> = &'a mut dyn FnMut(ImageId) -> Option<ID2D1Bitmap1>;

/// Applies `ops` to `input` under `transform` (the list's coordinates to the image's device
/// pixels: it scales the lengths and turns the offsets); the result image.
///
/// # Safety
/// `ctx` is a live device context of the device `input` belongs to, between BeginDraw and EndDraw
/// on this thread.
pub(crate) unsafe fn apply(
    ctx: &ID2D1DeviceContext,
    ops: &[FilterOp],
    input: &ID2D1Image,
    transform: &Matrix,
    images: Images<'_>,
) -> Result<ID2D1Image> {
    // (lengths scale by the transform's area scale; an anisotropic transform blurs by their mean)
    let scale = (transform.m11 * transform.m22 - transform.m12 * transform.m21)
        .abs()
        .sqrt();
    let mut current: ID2D1Image = input.clone();
    for op in ops {
        current = match *op {
            FilterOp::Blur(sigma) => {
                let sigma = (sigma * scale).min(MAX_SIGMA);
                if sigma <= 0.0 {
                    continue;
                }
                blur(ctx, &current, sigma)?
            }
            FilterOp::DropShadow {
                offset,
                sigma,
                color,
            } => {
                if color.a == 0 {
                    continue;
                }
                let sigma = (sigma * scale).min(MAX_SIGMA);
                let shadow = ctx.CreateEffect(&CLSID_D2D1Shadow)?;
                shadow.SetInput(0, &current, true);
                shadow.SetValue(
                    D2D1_SHADOW_PROP_BLUR_STANDARD_DEVIATION.0 as u32,
                    D2D1_PROPERTY_TYPE_FLOAT,
                    &sigma.to_le_bytes(),
                )?;
                shadow.SetValue(
                    D2D1_SHADOW_PROP_COLOR.0 as u32,
                    D2D1_PROPERTY_TYPE_VECTOR4,
                    &vector4(color),
                )?;
                // (the offset turns with the transform; the shadow stays where the content's
                // pixels are, so it is drawn with no translation of its own)
                let dx = offset.x * transform.m11 + offset.y * transform.m21;
                let dy = offset.x * transform.m12 + offset.y * transform.m22;
                let moved = ctx.CreateEffect(&CLSID_D2D12DAffineTransform)?;
                moved.SetInput(0, &shadow.GetOutput()?, true);
                let m = Matrix::translation(dx, dy);
                let mut bytes = Vec::with_capacity(24);
                for v in [m.m11, m.m12, m.m21, m.m22, m.dx, m.dy] {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                moved.SetValue(
                    D2D1_2DAFFINETRANSFORM_PROP_TRANSFORM_MATRIX.0 as u32,
                    D2D1_PROPERTY_TYPE_MATRIX_3X2,
                    &bytes,
                )?;
                let over = ctx.CreateEffect(&CLSID_D2D1Composite)?;
                over.SetInputCount(2)?;
                over.SetInput(0, &moved.GetOutput()?, true);
                over.SetInput(1, &current, true);
                over.SetValue(
                    D2D1_COMPOSITE_PROP_MODE.0 as u32,
                    D2D1_PROPERTY_TYPE_ENUM,
                    &(D2D1_COMPOSITE_MODE_SOURCE_OVER.0 as u32).to_le_bytes(),
                )?;
                over.GetOutput()?
            }
            FilterOp::Svg(ref f) => {
                crate::svg_effects::apply(ctx, f, &current, transform, &mut *images)?
            }
            _ => {
                let Some(m) = op.color_matrix() else {
                    continue;
                };
                let effect = ctx.CreateEffect(&CLSID_D2D1ColorMatrix)?;
                effect.SetInput(0, &current, true);
                // (Direct2D's 5x4 matrix is by input channel: each row the coefficients one
                // input channel, then the offset, gives to the four outputs)
                let mut bytes = Vec::with_capacity(80);
                for i in 0..5 {
                    for row in &m {
                        bytes.extend_from_slice(&row[i].to_le_bytes());
                    }
                }
                effect.SetValue(
                    D2D1_COLORMATRIX_PROP_COLOR_MATRIX.0 as u32,
                    D2D1_PROPERTY_TYPE_MATRIX_5X4,
                    &bytes,
                )?;
                // (the premultiplied mode un-premultiplies the input, applies the matrix to the
                // straight colors, and premultiplies the output: the "straight" mode takes and
                // gives straight alpha, and a transparent pixel with an offset would then draw as
                // an opaque color when composited as premultiplied)
                effect.SetValue(
                    D2D1_COLORMATRIX_PROP_ALPHA_MODE.0 as u32,
                    D2D1_PROPERTY_TYPE_ENUM,
                    &(D2D1_COLORMATRIX_ALPHA_MODE_PREMULTIPLIED.0 as u32).to_le_bytes(),
                )?;
                effect.SetValue(
                    D2D1_COLORMATRIX_PROP_CLAMP_OUTPUT.0 as u32,
                    D2D1_PROPERTY_TYPE_BOOL,
                    &1u32.to_le_bytes(),
                )?;
                effect.GetOutput()?
            }
        };
    }
    Ok(current)
}

/// A backdrop filter's result (Filter Effects 2 §3): `input` (the backdrop, device pixels) over
/// `base` (the canvas color for a backdrop read from the page's root, else transparent), cut to
/// `crop` (device pixels, whole), with the pixels beyond its edges mirrored from inside it, put
/// through `ops` as [`apply`] does, and cut to `crop` again.
///
/// # Safety
/// As [`apply`].
pub(crate) unsafe fn backdrop(
    ctx: &ID2D1DeviceContext,
    ops: &[FilterOp],
    input: &ID2D1Image,
    base: Color,
    crop: &Rect,
    transform: &Matrix,
    images: Images<'_>,
) -> Result<ID2D1Image> {
    // (the input's own bounds are only where something was drawn: over a flood cut to the
    // rectangle, it covers the whole rectangle, which is what the edges are mirrored from)
    let flood = ctx.CreateEffect(&CLSID_D2D1Flood)?;
    flood.SetValue(
        D2D1_FLOOD_PROP_COLOR.0 as u32,
        D2D1_PROPERTY_TYPE_VECTOR4,
        &vector4(base),
    )?;
    let over = ctx.CreateEffect(&CLSID_D2D1Composite)?;
    over.SetInputCount(2)?;
    over.SetInput(0, &cropped(ctx, &flood.GetOutput()?, crop)?, true);
    over.SetInput(1, input, true);
    over.SetValue(
        D2D1_COMPOSITE_PROP_MODE.0 as u32,
        D2D1_PROPERTY_TYPE_ENUM,
        &(D2D1_COMPOSITE_MODE_SOURCE_OVER.0 as u32).to_le_bytes(),
    )?;
    let cut = cropped(ctx, &over.GetOutput()?, crop)?;
    let border = ctx.CreateEffect(&CLSID_D2D1Border)?;
    border.SetInput(0, &cut, true);
    for prop in [D2D1_BORDER_PROP_EDGE_MODE_X, D2D1_BORDER_PROP_EDGE_MODE_Y] {
        border.SetValue(
            prop.0 as u32,
            D2D1_PROPERTY_TYPE_ENUM,
            &(D2D1_BORDER_EDGE_MODE_MIRROR.0 as u32).to_le_bytes(),
        )?;
    }
    let filtered = apply(ctx, ops, &border.GetOutput()?, transform, images)?;
    cropped(ctx, &filtered, crop)
}

/// `input` cut to `rect` (device pixels).
// SAFETY contract: as `apply`.
pub(crate) unsafe fn cropped(
    ctx: &ID2D1DeviceContext,
    input: &ID2D1Image,
    rect: &Rect,
) -> Result<ID2D1Image> {
    let crop = ctx.CreateEffect(&CLSID_D2D1Crop)?;
    crop.SetInput(0, input, true);
    let mut bytes = [0u8; 16];
    for (i, v) in [rect.x, rect.y, rect.x + rect.width, rect.y + rect.height]
        .into_iter()
        .enumerate()
    {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    crop.SetValue(
        D2D1_CROP_PROP_RECT.0 as u32,
        D2D1_PROPERTY_TYPE_VECTOR4,
        &bytes,
    )?;
    crop.GetOutput()
}

/// A Gaussian blur of `input` by `sigma` device pixels, transparent beyond it.
// SAFETY contract: as `apply`.
pub(crate) unsafe fn blur(
    ctx: &ID2D1DeviceContext,
    input: &ID2D1Image,
    sigma: f32,
) -> Result<ID2D1Image> {
    let effect = ctx.CreateEffect(&CLSID_D2D1GaussianBlur)?;
    effect.SetInput(0, input, true);
    effect.SetValue(
        D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION.0 as u32,
        D2D1_PROPERTY_TYPE_FLOAT,
        &sigma.to_le_bytes(),
    )?;
    effect.SetValue(
        D2D1_GAUSSIANBLUR_PROP_BORDER_MODE.0 as u32,
        D2D1_PROPERTY_TYPE_ENUM,
        &(D2D1_BORDER_MODE_SOFT.0 as u32).to_le_bytes(),
    )?;
    effect.GetOutput()
}

/// A color as the bytes of a `D2D1_VECTOR_4F` (straight alpha, 0..=1 channels).
fn vector4(c: Color) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, v) in [c.r, c.g, c.b, c.a].into_iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&(v as f32 / 255.0).to_le_bytes());
    }
    out
}
