// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An SVG filter ([`SvgFilter`]) as a Direct2D effect graph (Filter Effects 1 §9).
//!
//! The filter's input (the group's content, in device pixels) is cut to the filter region; each
//! primitive's result is cut to its subregion and kept by index for later primitives. A result is
//! kept in the color space its primitive operates in (`color-interpolation-filters`) and turned
//! into another primitive's space where that one reads it, and into sRGB at the end, with a table
//! transfer of the sRGB transfer function on the un-premultiplied colors.

use windows::core::{Interface, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

use crate::effects::{blur, cropped, Images};
use crate::filter::MAX_SIGMA;
use crate::geom::{Color, Matrix, Point, Rect};
use crate::svg_filter::*;
use crate::ImageId;

/// The sampled points of the sRGB transfer functions.
const CURVE_POINTS: usize = 256;

/// Applies `filter` to `input` (device pixels) under `t` (the list's coordinates to device
/// pixels); the result image, in sRGB.
///
/// # Safety
/// As [`crate::effects::apply`].
pub(crate) unsafe fn apply(
    ctx: &ID2D1DeviceContext,
    filter: &SvgFilter,
    input: &ID2D1Image,
    t: &Matrix,
    images: Images<'_>,
) -> Result<ID2D1Image> {
    let region = t.apply_rect(&filter.region);
    if filter.is_identity() || region.is_empty() {
        return Ok(input.clone());
    }
    let g = Graph {
        ctx,
        t,
        source: cropped(ctx, input, &region)?,
        images: std::cell::RefCell::new(images),
    };
    // (each result with whether it is in linear light, and its subregion in device pixels)
    let mut results: Vec<(ID2D1Image, bool, Rect)> = Vec::with_capacity(filter.primitives.len());
    for p in &filter.primitives {
        let sub = t.apply_rect(&p.subregion).intersect(&region);
        let input = |i: usize| -> Result<(ID2D1Image, Rect)> {
            let (img, linear, rect) = match p.inputs.get(i).copied() {
                Some(Input::SourceGraphic) | None => (g.source.clone(), false, region),
                Some(Input::SourceAlpha) => {
                    (g.alpha(&g.source, [0.0, 0.0, 0.0, 1.0])?, false, region)
                }
                Some(Input::Transparent) => (transparent(ctx)?, false, region),
                Some(Input::Result(r)) => results[r as usize].clone(),
            };
            Ok((g.to_space(img, linear, p.linear_rgb)?, rect))
        };
        let out = match g.primitive(p, &input, sub) {
            Ok(out) => out,
            // (a primitive whose parameters Direct2D refuses shows nothing, as the reference
            // implementation's errors do: transparent black)
            Err(_) => transparent(ctx)?,
        };
        // (an image is drawn as it is, in sRGB, whatever the primitive's space)
        let linear = p.linear_rgb && !matches!(p.kind, Kind::Image { .. });
        results.push((cropped(ctx, &out, &sub)?, linear, sub));
    }
    let (last, linear, _) = results.pop().expect("a primitive");
    g.to_space(last, linear, false)
}

struct Graph<'a> {
    ctx: &'a ID2D1DeviceContext,
    /// The list's coordinates to device pixels.
    t: &'a Matrix,
    /// The filter's input cut to the region.
    source: ID2D1Image,
    /// The `feImage`s' bitmaps.
    images: std::cell::RefCell<Images<'a>>,
}

impl Graph<'_> {
    /// A length along each axis in device pixels.
    fn scale(&self) -> (f32, f32) {
        let t = self.t;
        (t.m11.hypot(t.m12), t.m21.hypot(t.m22))
    }

    /// A vector of the list's coordinates in device pixels.
    fn vector(&self, dx: f32, dy: f32) -> (f32, f32) {
        let t = self.t;
        (dx * t.m11 + dy * t.m21, dx * t.m12 + dy * t.m22)
    }

    /// A point of the list's coordinates in device pixels.
    fn point(&self, x: f32, y: f32) -> Point {
        self.t.apply(Point::new(x, y))
    }

    unsafe fn effect(
        &self,
        clsid: &windows::core::GUID,
        inputs: &[&ID2D1Image],
    ) -> Result<ID2D1Effect> {
        // (every effect used here has as many inputs as it is given by default)
        let e = self.ctx.CreateEffect(clsid)?;
        for (i, img) in inputs.iter().enumerate() {
            e.SetInput(i as u32, *img, true);
        }
        Ok(e)
    }

    /// `input`'s alpha in `color` (straight components; the alpha multiplied by its alpha).
    unsafe fn alpha(&self, input: &ID2D1Image, color: [f32; 4]) -> Result<ID2D1Image> {
        let m = [
            [0.0, 0.0, 0.0, 0.0, color[0]],
            [0.0, 0.0, 0.0, 0.0, color[1]],
            [0.0, 0.0, 0.0, 0.0, color[2]],
            [0.0, 0.0, 0.0, color[3], 0.0],
        ];
        self.color_matrix(input, &m)
    }

    unsafe fn color_matrix(&self, input: &ID2D1Image, m: &[[f32; 5]; 4]) -> Result<ID2D1Image> {
        let e = self.effect(&CLSID_D2D1ColorMatrix, &[input])?;
        // (Direct2D's 5x4 matrix is by input channel: each row the coefficients one input channel,
        // then the offset, gives to the four outputs)
        let mut bytes = Vec::with_capacity(80);
        for i in 0..5 {
            for row in m {
                bytes.extend_from_slice(&row[i].to_le_bytes());
            }
        }
        set(
            &e,
            D2D1_COLORMATRIX_PROP_COLOR_MATRIX.0,
            D2D1_PROPERTY_TYPE_MATRIX_5X4,
            &bytes,
        )?;
        set_u32(
            &e,
            D2D1_COLORMATRIX_PROP_ALPHA_MODE.0,
            D2D1_PROPERTY_TYPE_ENUM,
            D2D1_COLORMATRIX_ALPHA_MODE_PREMULTIPLIED.0 as u32,
        )?;
        set_u32(
            &e,
            D2D1_COLORMATRIX_PROP_CLAMP_OUTPUT.0,
            D2D1_PROPERTY_TYPE_BOOL,
            1,
        )?;
        e.GetOutput()
    }

    /// `input` through a table transfer on each channel (`None`: unchanged), on un-premultiplied
    /// colors, clamped.
    unsafe fn tables(&self, input: &ID2D1Image, tables: [Option<&[f32]>; 4]) -> Result<ID2D1Image> {
        let e = self.effect(&CLSID_D2D1TableTransfer, &[input])?;
        let props = [
            (
                D2D1_TABLETRANSFER_PROP_RED_TABLE,
                D2D1_TABLETRANSFER_PROP_RED_DISABLE,
            ),
            (
                D2D1_TABLETRANSFER_PROP_GREEN_TABLE,
                D2D1_TABLETRANSFER_PROP_GREEN_DISABLE,
            ),
            (
                D2D1_TABLETRANSFER_PROP_BLUE_TABLE,
                D2D1_TABLETRANSFER_PROP_BLUE_DISABLE,
            ),
            (
                D2D1_TABLETRANSFER_PROP_ALPHA_TABLE,
                D2D1_TABLETRANSFER_PROP_ALPHA_DISABLE,
            ),
        ];
        for ((table, disable), values) in props.into_iter().zip(tables) {
            match values {
                Some(v) => {
                    let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
                    set(&e, table.0, D2D1_PROPERTY_TYPE_BLOB, &bytes)?;
                    set_u32(&e, disable.0, D2D1_PROPERTY_TYPE_BOOL, 0)?;
                }
                None => set_u32(&e, disable.0, D2D1_PROPERTY_TYPE_BOOL, 1)?,
            }
        }
        set_u32(
            &e,
            D2D1_TABLETRANSFER_PROP_CLAMP_OUTPUT.0,
            D2D1_PROPERTY_TYPE_BOOL,
            1,
        )?;
        e.GetOutput()
    }

    /// `feComponentTransfer`: each kind of function as Direct2D's transfer effect of that kind
    /// (they follow the same definitions), on the channels that use it, the others left alone.
    unsafe fn transfer(&self, input: &ID2D1Image, funcs: &[Transfer; 4]) -> Result<ID2D1Image> {
        let mut out = input.clone();
        for kind in [
            TransferKind::Table,
            TransferKind::Discrete,
            TransferKind::Linear,
            TransferKind::Gamma,
        ] {
            // (a table or discrete function without values is the identity)
            let used = |f: &Transfer| {
                f.kind == kind
                    && !(matches!(kind, TransferKind::Table | TransferKind::Discrete)
                        && f.table.is_empty())
            };
            if !funcs.iter().any(used) {
                continue;
            }
            let (clsid, stride) = match kind {
                TransferKind::Table => (&CLSID_D2D1TableTransfer, 2),
                TransferKind::Discrete => (&CLSID_D2D1DiscreteTransfer, 2),
                TransferKind::Linear => (&CLSID_D2D1LinearTransfer, 3),
                _ => (&CLSID_D2D1GammaTransfer, 4),
            };
            let e = self.effect(clsid, &[&out])?;
            for (c, f) in funcs.iter().enumerate() {
                let base = (c * stride) as i32;
                let disable = base + stride as i32 - 1;
                if !used(f) {
                    set_u32(&e, disable, D2D1_PROPERTY_TYPE_BOOL, 1)?;
                    continue;
                }
                match kind {
                    TransferKind::Table | TransferKind::Discrete => {
                        let bytes: Vec<u8> = f.table.iter().flat_map(|v| v.to_le_bytes()).collect();
                        set(&e, base, D2D1_PROPERTY_TYPE_BLOB, &bytes)?;
                    }
                    TransferKind::Linear => {
                        set_f32(&e, base, f.intercept)?;
                        set_f32(&e, base + 1, f.slope)?;
                    }
                    _ => {
                        set_f32(&e, base, f.amplitude)?;
                        set_f32(&e, base + 1, f.exponent)?;
                        set_f32(&e, base + 2, f.offset)?;
                    }
                }
                set_u32(&e, disable, D2D1_PROPERTY_TYPE_BOOL, 0)?;
            }
            set_u32(&e, 4 * stride as i32, D2D1_PROPERTY_TYPE_BOOL, 1)?;
            out = e.GetOutput()?;
        }
        Ok(out)
    }

    /// `img` (in linear light when `from`) in the space `to` names.
    unsafe fn to_space(&self, img: ID2D1Image, from: bool, to: bool) -> Result<ID2D1Image> {
        if from == to {
            return Ok(img);
        }
        let curve: Vec<f32> = (0..CURVE_POINTS)
            .map(|i| {
                let c = i as f32 / (CURVE_POINTS - 1) as f32;
                if to {
                    srgb_to_linear(c)
                } else {
                    linear_to_srgb(c)
                }
            })
            .collect();
        self.tables(&img, [Some(&curve), Some(&curve), Some(&curve), None])
    }

    /// A Gaussian blur by standard deviations in the list's units, transparent beyond the input.
    unsafe fn blur(&self, input: &ID2D1Image, x: f32, y: f32) -> Result<ID2D1Image> {
        let (sx, sy) = self.scale();
        let (x, y) = ((x * sx).min(MAX_SIGMA), (y * sy).min(MAX_SIGMA));
        if (x - y).abs() < 0.01 {
            return if x > 0.0 {
                blur(self.ctx, input, x)
            } else {
                Ok(input.clone())
            };
        }
        let mut out = input.clone();
        for (sigma, angle) in [(x, 0.0f32), (y, 90.0)] {
            if sigma <= 0.0 {
                continue;
            }
            let e = self.effect(&CLSID_D2D1DirectionalBlur, &[&out])?;
            set_f32(&e, D2D1_DIRECTIONALBLUR_PROP_STANDARD_DEVIATION.0, sigma)?;
            set_f32(&e, D2D1_DIRECTIONALBLUR_PROP_ANGLE.0, angle)?;
            set_u32(
                &e,
                D2D1_DIRECTIONALBLUR_PROP_BORDER_MODE.0,
                D2D1_PROPERTY_TYPE_ENUM,
                D2D1_BORDER_MODE_SOFT.0 as u32,
            )?;
            out = e.GetOutput()?;
        }
        Ok(out)
    }

    /// `input` moved by a vector of the list's coordinates.
    unsafe fn offset(&self, input: &ID2D1Image, dx: f32, dy: f32) -> Result<ID2D1Image> {
        let (dx, dy) = self.vector(dx, dy);
        let e = self.effect(&CLSID_D2D12DAffineTransform, &[input])?;
        let m = Matrix::translation(dx, dy);
        let bytes: Vec<u8> = [m.m11, m.m12, m.m21, m.m22, m.dx, m.dy]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        set(
            &e,
            D2D1_2DAFFINETRANSFORM_PROP_TRANSFORM_MATRIX.0,
            D2D1_PROPERTY_TYPE_MATRIX_3X2,
            &bytes,
        )?;
        e.GetOutput()
    }

    /// `top` composited onto `bottom` by a Composite effect mode.
    unsafe fn composite(
        &self,
        bottom: &ID2D1Image,
        top: &ID2D1Image,
        mode: D2D1_COMPOSITE_MODE,
    ) -> Result<ID2D1Image> {
        let e = self.effect(&CLSID_D2D1Composite, &[bottom, top])?;
        set_u32(
            &e,
            D2D1_COMPOSITE_PROP_MODE.0,
            D2D1_PROPERTY_TYPE_ENUM,
            mode.0 as u32,
        )?;
        e.GetOutput()
    }

    /// A flood of `color` (straight alpha, in the primitive's space) over the subregion.
    unsafe fn flood(&self, color: Color, linear: bool, sub: &Rect) -> Result<ID2D1Image> {
        let [r, g, b, a] = components(color, linear);
        let e = self.effect(&CLSID_D2D1Flood, &[])?;
        // (the Flood effect's color is premultiplied)
        set_vec(&e, D2D1_FLOOD_PROP_COLOR.0, &[r * a, g * a, b * a, a])?;
        cropped(self.ctx, &e.GetOutput()?, sub)
    }

    /// `feImage`: the part `src` of the image (all of it when empty) drawn into `dest` (the
    /// list's coordinates), with hard edges (nothing outside `dest`); transparent black when
    /// the image is not there or is empty.
    unsafe fn image(&self, image: ImageId, dest: &Rect, src: &Rect) -> Result<ID2D1Image> {
        let Some(bitmap) = (self.images.borrow_mut())(image) else {
            return transparent(self.ctx);
        };
        let size = bitmap.GetPixelSize();
        let src = if src.is_empty() {
            Rect::new(0.0, 0.0, size.width as f32, size.height as f32)
        } else {
            *src
        };
        if src.is_empty() || dest.is_empty() {
            return transparent(self.ctx);
        }
        // (image pixels to the list's coordinates, then to device pixels)
        let m = Matrix::translation(-src.x, -src.y)
            .then(&Matrix::scale(
                dest.width / src.width,
                dest.height / src.height,
            ))
            .then(&Matrix::translation(dest.x, dest.y))
            .then(self.t);
        let e = self.effect(&CLSID_D2D12DAffineTransform, &[&bitmap.cast()?])?;
        let bytes: Vec<u8> = [m.m11, m.m12, m.m21, m.m22, m.dx, m.dy]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        set(
            &e,
            D2D1_2DAFFINETRANSFORM_PROP_TRANSFORM_MATRIX.0,
            D2D1_PROPERTY_TYPE_MATRIX_3X2,
            &bytes,
        )?;
        set_u32(
            &e,
            D2D1_2DAFFINETRANSFORM_PROP_INTERPOLATION_MODE.0,
            D2D1_PROPERTY_TYPE_ENUM,
            D2D1_2DAFFINETRANSFORM_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC.0 as u32,
        )?;
        set_u32(
            &e,
            D2D1_2DAFFINETRANSFORM_PROP_BORDER_MODE.0,
            D2D1_PROPERTY_TYPE_ENUM,
            D2D1_BORDER_MODE_HARD.0 as u32,
        )?;
        cropped(self.ctx, &e.GetOutput()?, &self.t.apply_rect(dest))
    }

    unsafe fn primitive(
        &self,
        p: &Primitive,
        input: &dyn Fn(usize) -> Result<(ID2D1Image, Rect)>,
        sub: Rect,
    ) -> Result<ID2D1Image> {
        let ctx = self.ctx;
        let (sx, sy) = self.scale();
        let one = || input(0).map(|(img, _)| img);
        Ok(match &p.kind {
            Kind::Flood(color) => self.flood(*color, p.linear_rgb, &sub)?,
            Kind::Image { image, dest, src } => self.image(*image, dest, src)?,
            Kind::ColorMatrix(m) => self.color_matrix(&one()?, m)?,
            Kind::ComponentTransfer(funcs) => self.transfer(&one()?, funcs)?,
            Kind::GaussianBlur { x, y } => {
                // (a negative or zero deviation in one axis blurs only along the other; both zero
                // or less disable the primitive, Filter Effects 1 §9.10)
                self.blur(&one()?, *x, *y)?
            }
            Kind::Offset { dx, dy } => self.offset(&one()?, *dx, *dy)?,
            Kind::Merge => {
                let mut out = transparent(ctx)?;
                for i in 0..p.inputs.len() {
                    let (img, _) = input(i)?;
                    out = self.composite(&out, &img, D2D1_COMPOSITE_MODE_SOURCE_OVER)?;
                }
                out
            }
            Kind::Blend(mode) => {
                let (top, _) = input(0)?;
                let (bottom, _) = input(1)?;
                match blend_mode(*mode) {
                    None => self.composite(&bottom, &top, D2D1_COMPOSITE_MODE_SOURCE_OVER)?,
                    Some(m) => {
                        let e = self.effect(&CLSID_D2D1Blend, &[&bottom, &top])?;
                        set_u32(
                            &e,
                            D2D1_BLEND_PROP_MODE.0,
                            D2D1_PROPERTY_TYPE_ENUM,
                            m.0 as u32,
                        )?;
                        e.GetOutput()?
                    }
                }
            }
            Kind::Composite { op, k } => {
                let (top, _) = input(0)?;
                let (bottom, _) = input(1)?;
                let mode = match op {
                    CompositeOp::Over => D2D1_COMPOSITE_MODE_SOURCE_OVER,
                    CompositeOp::In => D2D1_COMPOSITE_MODE_SOURCE_IN,
                    CompositeOp::Out => D2D1_COMPOSITE_MODE_SOURCE_OUT,
                    CompositeOp::Atop => D2D1_COMPOSITE_MODE_SOURCE_ATOP,
                    CompositeOp::Xor => D2D1_COMPOSITE_MODE_XOR,
                    CompositeOp::Lighter => D2D1_COMPOSITE_MODE_PLUS,
                    CompositeOp::Arithmetic => {
                        let e = self.effect(&CLSID_D2D1ArithmeticComposite, &[&bottom, &top])?;
                        let bytes: Vec<u8> = k.iter().flat_map(|v| v.to_le_bytes()).collect();
                        set(
                            &e,
                            D2D1_ARITHMETICCOMPOSITE_PROP_COEFFICIENTS.0,
                            D2D1_PROPERTY_TYPE_VECTOR4,
                            &bytes,
                        )?;
                        set_u32(
                            &e,
                            D2D1_ARITHMETICCOMPOSITE_PROP_CLAMP_OUTPUT.0,
                            D2D1_PROPERTY_TYPE_BOOL,
                            1,
                        )?;
                        // (k4 adds color where both inputs are transparent: within the
                        // subregion only)
                        return cropped(ctx, &e.GetOutput()?, &sub);
                    }
                };
                self.composite(&bottom, &top, mode)?
            }
            Kind::DropShadow {
                dx,
                dy,
                x,
                y,
                color,
            } => {
                let content = one()?;
                let color = components(*color, p.linear_rgb);
                let shadow = self.alpha(&content, color)?;
                let shadow = self.blur(&shadow, *x, *y)?;
                let shadow = self.offset(&shadow, *dx, *dy)?;
                self.composite(&shadow, &content, D2D1_COMPOSITE_MODE_SOURCE_OVER)?
            }
            Kind::Morphology { dilate, rx, ry } => {
                let content = one()?;
                let (w, h) = ((rx * sx).round(), (ry * sy).round());
                // (a zero radius disables the primitive: its result is its input, §9.13)
                if w <= 0.0 && h <= 0.0 {
                    return Ok(content);
                }
                let e = self.effect(&CLSID_D2D1Morphology, &[&content])?;
                let mode = if *dilate {
                    D2D1_MORPHOLOGY_MODE_DILATE
                } else {
                    D2D1_MORPHOLOGY_MODE_ERODE
                };
                set_u32(
                    &e,
                    D2D1_MORPHOLOGY_PROP_MODE.0,
                    D2D1_PROPERTY_TYPE_ENUM,
                    mode.0 as u32,
                )?;
                // (the kernel is the radius each side of the pixel, at most Direct2D's 100)
                let size = |r: f32| (2.0 * r.max(0.0) + 1.0).min(100.0) as u32;
                set_u32(
                    &e,
                    D2D1_MORPHOLOGY_PROP_WIDTH.0,
                    D2D1_PROPERTY_TYPE_UINT32,
                    size(w),
                )?;
                set_u32(
                    &e,
                    D2D1_MORPHOLOGY_PROP_HEIGHT.0,
                    D2D1_PROPERTY_TYPE_UINT32,
                    size(h),
                )?;
                e.GetOutput()?
            }
            Kind::Tile => {
                let (content, rect) = input(0)?;
                let e = self.effect(&CLSID_D2D1Tile, &[&content])?;
                let bytes: Vec<u8> = [rect.x, rect.y, rect.right(), rect.bottom()]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect();
                set(
                    &e,
                    D2D1_TILE_PROP_RECT.0,
                    D2D1_PROPERTY_TYPE_VECTOR4,
                    &bytes,
                )?;
                // (the tiles are laid from the input's subregion's corner)
                e.GetOutput()?
            }
            Kind::Turbulence {
                base_x,
                base_y,
                octaves,
                seed,
                stitch,
                fractal_noise,
            } => {
                // (at frequency 0 the noise is 0 everywhere: turbulence transparent black,
                // fractal noise every channel at one half)
                if *base_x <= 0.0 && *base_y <= 0.0 {
                    let v = if *fractal_noise { 0.5 } else { 0.0 };
                    let e = self.effect(&CLSID_D2D1Flood, &[])?;
                    // (premultiplied: one half of every channel, alpha one half)
                    set_vec(&e, D2D1_FLOOD_PROP_COLOR.0, &[v * v, v * v, v * v, v])?;
                    return cropped(ctx, &e.GetOutput()?, &sub);
                }
                let e = self.effect(&CLSID_D2D1Turbulence, &[])?;
                // (the noise is a function of the user space position: Direct2D's is of the
                // output pixel plus the offset, so the frequency scales and the offset moves the
                // device origin to the user space origin)
                let origin = self.point(0.0, 0.0);
                set_vec(&e, D2D1_TURBULENCE_PROP_OFFSET.0, &[-origin.x, -origin.y])?;
                set_vec(
                    &e,
                    D2D1_TURBULENCE_PROP_SIZE.0,
                    &[sub.right(), sub.bottom()],
                )?;
                set_vec(
                    &e,
                    D2D1_TURBULENCE_PROP_BASE_FREQUENCY.0,
                    &[(base_x / sx).max(1e-6), (base_y / sy).max(1e-6)],
                )?;
                set_u32(
                    &e,
                    D2D1_TURBULENCE_PROP_NUM_OCTAVES.0,
                    D2D1_PROPERTY_TYPE_UINT32,
                    *octaves,
                )?;
                set_u32(
                    &e,
                    D2D1_TURBULENCE_PROP_SEED.0,
                    D2D1_PROPERTY_TYPE_INT32,
                    seed.round() as i32 as u32,
                )?;
                let noise = if *fractal_noise {
                    D2D1_TURBULENCE_NOISE_FRACTAL_SUM
                } else {
                    D2D1_TURBULENCE_NOISE_TURBULENCE
                };
                set_u32(
                    &e,
                    D2D1_TURBULENCE_PROP_NOISE.0,
                    D2D1_PROPERTY_TYPE_ENUM,
                    noise.0 as u32,
                )?;
                set_u32(
                    &e,
                    D2D1_TURBULENCE_PROP_STITCHABLE.0,
                    D2D1_PROPERTY_TYPE_BOOL,
                    *stitch as u32,
                )?;
                cropped(ctx, &e.GetOutput()?, &sub)?
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
            } => {
                let content = one()?;
                let e = self.effect(&CLSID_D2D1ConvolveMatrix, &[&content])?;
                set_u32(
                    &e,
                    D2D1_CONVOLVEMATRIX_PROP_KERNEL_SIZE_X.0,
                    D2D1_PROPERTY_TYPE_UINT32,
                    *order_x,
                )?;
                set_u32(
                    &e,
                    D2D1_CONVOLVEMATRIX_PROP_KERNEL_SIZE_Y.0,
                    D2D1_PROPERTY_TYPE_UINT32,
                    *order_y,
                )?;
                let bytes: Vec<u8> = kernel.iter().flat_map(|v| v.to_le_bytes()).collect();
                set(
                    &e,
                    D2D1_CONVOLVEMATRIX_PROP_KERNEL_MATRIX.0,
                    D2D1_PROPERTY_TYPE_BLOB,
                    &bytes,
                )?;
                set_f32(&e, D2D1_CONVOLVEMATRIX_PROP_DIVISOR.0, *divisor)?;
                set_f32(&e, D2D1_CONVOLVEMATRIX_PROP_BIAS.0, *bias)?;
                // (Direct2D's offset is the target pixel's from the kernel's center)
                let centre = |order: u32, target: u32| target as f32 - ((order - 1) / 2) as f32;
                set_vec(
                    &e,
                    D2D1_CONVOLVEMATRIX_PROP_KERNEL_OFFSET.0,
                    &[centre(*order_x, *target_x), centre(*order_y, *target_y)],
                )?;
                set_u32(
                    &e,
                    D2D1_CONVOLVEMATRIX_PROP_PRESERVE_ALPHA.0,
                    D2D1_PROPERTY_TYPE_BOOL,
                    *preserve_alpha as u32,
                )?;
                let border = match edge {
                    EdgeMode::Duplicate => D2D1_BORDER_MODE_HARD,
                    _ => D2D1_BORDER_MODE_SOFT,
                };
                set_u32(
                    &e,
                    D2D1_CONVOLVEMATRIX_PROP_BORDER_MODE.0,
                    D2D1_PROPERTY_TYPE_ENUM,
                    border.0 as u32,
                )?;
                set_u32(
                    &e,
                    D2D1_CONVOLVEMATRIX_PROP_CLAMP_OUTPUT.0,
                    D2D1_PROPERTY_TYPE_BOOL,
                    1,
                )?;
                e.GetOutput()?
            }
            Kind::DisplacementMap { scale, x, y } => {
                let (content, _) = input(0)?;
                let (map, _) = input(1)?;
                let e = self.effect(&CLSID_D2D1DisplacementMap, &[&content, &map])?;
                set_f32(
                    &e,
                    D2D1_DISPLACEMENTMAP_PROP_SCALE.0,
                    scale * (sx * sy).sqrt(),
                )?;
                let channel = |c: &Channel| match c {
                    Channel::R => D2D1_CHANNEL_SELECTOR_R,
                    Channel::G => D2D1_CHANNEL_SELECTOR_G,
                    Channel::B => D2D1_CHANNEL_SELECTOR_B,
                    Channel::A => D2D1_CHANNEL_SELECTOR_A,
                };
                set_u32(
                    &e,
                    D2D1_DISPLACEMENTMAP_PROP_X_CHANNEL_SELECT.0,
                    D2D1_PROPERTY_TYPE_ENUM,
                    channel(x).0 as u32,
                )?;
                set_u32(
                    &e,
                    D2D1_DISPLACEMENTMAP_PROP_Y_CHANNEL_SELECT.0,
                    D2D1_PROPERTY_TYPE_ENUM,
                    channel(y).0 as u32,
                )?;
                e.GetOutput()?
            }
            Kind::DiffuseLighting {
                surface_scale,
                constant,
                color,
                light,
            } => {
                let content = one()?;
                let color = components(*color, p.linear_rgb);
                self.lighting(
                    &content,
                    light,
                    *surface_scale,
                    *constant,
                    None,
                    color,
                    &sub,
                )?
            }
            Kind::SpecularLighting {
                surface_scale,
                constant,
                exponent,
                color,
                light,
            } => {
                let content = one()?;
                let color = components(*color, p.linear_rgb);
                self.lighting(
                    &content,
                    light,
                    *surface_scale,
                    *constant,
                    Some(*exponent),
                    color,
                    &sub,
                )?
            }
        })
    }

    /// A lighting primitive: diffuse, or specular with an exponent.
    #[allow(clippy::too_many_arguments)]
    unsafe fn lighting(
        &self,
        input: &ID2D1Image,
        light: &Light,
        surface_scale: f32,
        constant: f32,
        specular: Option<f32>,
        color: [f32; 4],
        sub: &Rect,
    ) -> Result<ID2D1Image> {
        // (positions in device pixels; a height scales as the lengths do)
        let z_scale = {
            let (sx, sy) = self.scale();
            (sx * sy).sqrt()
        };
        let rgb = [color[0], color[1], color[2]];
        let specular_exponent = specular.map(|e| e.clamp(1.0, 128.0));
        let (clsid, props): (&windows::core::GUID, LightProps) = match (*light, specular_exponent) {
            (Light::Distant { azimuth, elevation }, None) => (
                &CLSID_D2D1DistantDiffuse,
                LightProps::Distant(
                    azimuth,
                    elevation,
                    D2D1_DISTANTDIFFUSE_PROP_AZIMUTH.0,
                    D2D1_DISTANTDIFFUSE_PROP_ELEVATION.0,
                ),
            ),
            (Light::Distant { azimuth, elevation }, Some(_)) => (
                &CLSID_D2D1DistantSpecular,
                LightProps::Distant(
                    azimuth,
                    elevation,
                    D2D1_DISTANTSPECULAR_PROP_AZIMUTH.0,
                    D2D1_DISTANTSPECULAR_PROP_ELEVATION.0,
                ),
            ),
            (Light::Point { x, y, z }, s) => {
                let at = self.point(x, y);
                let clsid = if s.is_some() {
                    &CLSID_D2D1PointSpecular
                } else {
                    &CLSID_D2D1PointDiffuse
                };
                let prop = if s.is_some() {
                    D2D1_POINTSPECULAR_PROP_LIGHT_POSITION.0
                } else {
                    D2D1_POINTDIFFUSE_PROP_LIGHT_POSITION.0
                };
                (clsid, LightProps::Point([at.x, at.y, z * z_scale], prop))
            }
            (
                Light::Spot {
                    x,
                    y,
                    z,
                    at_x,
                    at_y,
                    at_z,
                    exponent,
                    cone,
                },
                s,
            ) => {
                let from = self.point(x, y);
                let to = self.point(at_x, at_y);
                let clsid = if s.is_some() {
                    &CLSID_D2D1SpotSpecular
                } else {
                    &CLSID_D2D1SpotDiffuse
                };
                (
                    clsid,
                    LightProps::Spot {
                        from: [from.x, from.y, z * z_scale],
                        to: [to.x, to.y, at_z * z_scale],
                        exponent,
                        cone: cone.map_or(90.0, |c| c.abs().min(90.0)),
                        specular: s.is_some(),
                    },
                )
            }
        };
        let e = self.effect(clsid, &[input])?;
        match props {
            LightProps::Distant(azimuth, elevation, pa, pe) => {
                set_f32(&e, pa, azimuth)?;
                set_f32(&e, pe, elevation)?;
            }
            LightProps::Point(at, prop) => set_vec(&e, prop, &at)?,
            LightProps::Spot {
                from,
                to,
                exponent,
                cone,
                specular,
            } => {
                let (p_from, p_to, p_focus, p_cone) = if specular {
                    (
                        D2D1_SPOTSPECULAR_PROP_LIGHT_POSITION.0,
                        D2D1_SPOTSPECULAR_PROP_POINTS_AT.0,
                        D2D1_SPOTSPECULAR_PROP_FOCUS.0,
                        D2D1_SPOTSPECULAR_PROP_LIMITING_CONE_ANGLE.0,
                    )
                } else {
                    (
                        D2D1_SPOTDIFFUSE_PROP_LIGHT_POSITION.0,
                        D2D1_SPOTDIFFUSE_PROP_POINTS_AT.0,
                        D2D1_SPOTDIFFUSE_PROP_FOCUS.0,
                        D2D1_SPOTDIFFUSE_PROP_LIMITING_CONE_ANGLE.0,
                    )
                };
                set_vec(&e, p_from, &from)?;
                set_vec(&e, p_to, &to)?;
                set_f32(&e, p_focus, exponent.clamp(0.0, 200.0))?;
                set_f32(&e, p_cone, cone)?;
            }
        }
        // (after the light's own properties: the diffuse constant, the surface scale and the color;
        // or the specular exponent, the specular constant, the surface scale and the color)
        let base = match light {
            Light::Distant { .. } => 2,
            Light::Point { .. } => 1,
            Light::Spot { .. } => 4,
        };
        let rest = match specular_exponent {
            Some(exponent) => {
                set_f32(&e, base, exponent)?;
                base + 1
            }
            None => base,
        };
        set_f32(&e, rest, constant)?;
        set_f32(&e, rest + 1, surface_scale)?;
        set_vec(&e, rest + 2, &rgb)?;
        // (lighting reads the whole plane: its result is the subregion's)
        cropped(self.ctx, &e.GetOutput()?, sub)
    }
}

enum LightProps {
    Distant(f32, f32, i32, i32),
    Point([f32; 3], i32),
    Spot {
        from: [f32; 3],
        to: [f32; 3],
        exponent: f32,
        cone: f32,
        specular: bool,
    },
}

/// The `feBlend` mode (a `mix-blend-mode` keyword's index) as Direct2D's; `None` for `normal`.
fn blend_mode(mode: u8) -> Option<D2D1_BLEND_MODE> {
    Some(match mode {
        1 => D2D1_BLEND_MODE_MULTIPLY,
        2 => D2D1_BLEND_MODE_SCREEN,
        3 => D2D1_BLEND_MODE_OVERLAY,
        4 => D2D1_BLEND_MODE_DARKEN,
        5 => D2D1_BLEND_MODE_LIGHTEN,
        6 => D2D1_BLEND_MODE_COLOR_DODGE,
        7 => D2D1_BLEND_MODE_COLOR_BURN,
        8 => D2D1_BLEND_MODE_HARD_LIGHT,
        9 => D2D1_BLEND_MODE_SOFT_LIGHT,
        10 => D2D1_BLEND_MODE_DIFFERENCE,
        11 => D2D1_BLEND_MODE_EXCLUSION,
        12 => D2D1_BLEND_MODE_HUE,
        13 => D2D1_BLEND_MODE_SATURATION,
        14 => D2D1_BLEND_MODE_COLOR,
        15 => D2D1_BLEND_MODE_LUMINOSITY,
        _ => return None,
    })
}

/// A transparent image (an empty command list's, which has no pixels).
unsafe fn transparent(ctx: &ID2D1DeviceContext) -> Result<ID2D1Image> {
    let e = ctx.CreateEffect(&CLSID_D2D1Flood)?;
    set(
        &e,
        D2D1_FLOOD_PROP_COLOR.0,
        D2D1_PROPERTY_TYPE_VECTOR4,
        &[0u8; 16],
    )?;
    // (cut to nothing, so it bounds nothing)
    cropped(ctx, &e.GetOutput()?, &Rect::ZERO)
}

unsafe fn set(e: &ID2D1Effect, index: i32, kind: D2D1_PROPERTY_TYPE, bytes: &[u8]) -> Result<()> {
    e.SetValue(index as u32, kind, bytes)
}

unsafe fn set_u32(e: &ID2D1Effect, index: i32, kind: D2D1_PROPERTY_TYPE, v: u32) -> Result<()> {
    set(e, index, kind, &v.to_le_bytes())
}

unsafe fn set_f32(e: &ID2D1Effect, index: i32, v: f32) -> Result<()> {
    set(e, index, D2D1_PROPERTY_TYPE_FLOAT, &v.to_le_bytes())
}

/// A vector of 2, 3 or 4 floats.
unsafe fn set_vec(e: &ID2D1Effect, index: i32, v: &[f32]) -> Result<()> {
    let kind = match v.len() {
        2 => D2D1_PROPERTY_TYPE_VECTOR2,
        3 => D2D1_PROPERTY_TYPE_VECTOR3,
        _ => D2D1_PROPERTY_TYPE_VECTOR4,
    };
    let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
    set(e, index, kind, &bytes)
}

/// The sRGB transfer function's inverse (CSS Color 4 §10.2), 0..=1.
pub(crate) fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The sRGB transfer function, 0..=1.
pub(crate) fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// A color's straight components, 0..=1, in linear light when `linear`.
fn components(c: Color, linear: bool) -> [f32; 4] {
    let f = |v: u8| {
        let v = v as f32 / 255.0;
        if linear {
            srgb_to_linear(v)
        } else {
            v
        }
    };
    [f(c.r), f(c.g), f(c.b), c.a as f32 / 255.0]
}
