// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rasterizing display lists on Linux: a painter over a Skia canvas, and [`HeadlessRenderer`],
//! which paints into memory on Skia's CPU backend (the headless modes and reftests).
//!
//! Every group of the list is a Skia save or layer: a clip or transform a `save`, an opacity group
//! a layer with that alpha, a filter group a layer whose paint carries the filters as an image
//! filter, a mask group a layer the mask is composited onto with destination-in. Box edges are
//! moved to device pixels as on Windows ([`Snap`]); text is drawn with the faces the text engine
//! shaped it with ([`FontResolver::font_data`]).

use std::collections::HashMap;
use std::sync::Arc;

use skia_safe as sk;
use skia_safe::gradient::{shaders as gradients, Colors, Gradient, Interpolation};

use crate::display_list::{
    Border, BorderStyle, BoxShadow, DisplayList, Extend, GradientStop, Item, LineCap, LineJoin,
    MaskComposite, PathRange, PathVerb, Pattern, Stroke, TextRun,
};
use crate::filter::FilterOp;
use crate::image::{Pixmap, ShownImage};
use crate::text::{FontData, TextEngine};
use crate::{Color, FontId, ImageId, ImageRendering, Matrix, Point, Radii, Rect, Sides, Size};
use snowlark_graphics::geom::Snap;
pub use snowlark_graphics::raster::{ImageResolver, Viewport};
use snowlark_graphics::{Error, Result};

#[path = "raster_filters.rs"]
pub(crate) mod filters;
#[path = "raster_svg.rs"]
mod svg;
#[cfg(test)]
#[path = "raster_tests.rs"]
mod tests;

/// Where a list's `FontId`s resolve: the text engine that shaped its glyphs.
pub trait FontResolver {
    /// The face's bytes and how it is drawn (variations, synthesized bold or oblique).
    fn font_data(&self, font: FontId) -> Option<FontData>;
}

impl FontResolver for TextEngine {
    fn font_data(&self, font: FontId) -> Option<FontData> {
        TextEngine::font_data(self, font)
    }
}

impl<T: FontResolver + ?Sized> FontResolver for Arc<T> {
    fn font_data(&self, font: FontId) -> Option<FontData> {
        (**self).font_data(font)
    }
}

/// What a display list refers to: the fonts that shaped its glyphs and the images it shows.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    pub text: &'a dyn FontResolver,
    pub images: &'a dyn ImageResolver,
}

/// A typeface's key: the face's file (the bytes' address), index and variations (axis, value bits).
type FaceKey = (usize, u32, Vec<([u8; 4], u32)>);

/// The Skia objects a painter keeps between lists: typefaces by face and images by the pixels
/// they were made from.
#[derive(Default)]
pub(crate) struct Caches {
    typefaces: HashMap<FaceKey, Option<sk::Typeface>>,
    /// Images by id, frame and the pixels' version.
    images: HashMap<(ImageId, usize, u64), sk::Image>,
    /// Pixels that came ahead of the lists that show them (a tab's raster thread: its table keeps
    /// no pixels), by id and frame, with their version: the newest of each, kept until replaced.
    uploads: HashMap<(ImageId, usize), (u64, sk::Image)>,
}

/// Skia's image of `width` x `height` premultiplied BGRA pixels, rows packed.
fn bgra_image(width: u32, height: u32, pixels: &[u8]) -> Option<sk::Image> {
    if width == 0 || height == 0 || pixels.len() as u64 != width as u64 * height as u64 * 4 {
        return None;
    }
    let info = sk::ImageInfo::new(
        (width as i32, height as i32),
        sk::ColorType::BGRA8888,
        sk::AlphaType::Premul,
        None,
    );
    sk::images::raster_from_data(&info, sk::Data::new_copy(pixels), width as usize * 4)
}

/// Images kept between lists before the cache starts over.
const MAX_CACHED_IMAGES: usize = 512;

impl Caches {
    /// Keeps pixels that came ahead of the lists that show them ([`crate::gpu::TileRasterizer::upload`]).
    pub(crate) fn upload(&mut self, id: ImageId, shown: &ShownImage, pixels: &[u8]) -> Result<()> {
        let image = bgra_image(shown.width, shown.height, pixels).ok_or(Error::Invalid)?;
        self.uploads
            .insert((id, shown.frame), (shown.version, image));
        Ok(())
    }

    fn typeface(&mut self, data: &FontData) -> Option<sk::Typeface> {
        let key = (
            Arc::as_ptr(&data.bytes) as *const u8 as usize,
            data.index,
            data.variations
                .iter()
                .map(|(tag, v)| (*tag, v.to_bits()))
                .collect::<Vec<_>>(),
        );
        self.typefaces
            .entry(key)
            .or_insert_with(|| {
                let bytes = sk::Data::new_copy(&data.bytes);
                let face = sk::FontMgr::new().new_from_data(bytes, data.index)?;
                if data.variations.is_empty() {
                    return Some(face);
                }
                let coordinates: Vec<sk::font_arguments::variation_position::Coordinate> = data
                    .variations
                    .iter()
                    .map(
                        |(tag, value)| sk::font_arguments::variation_position::Coordinate {
                            axis: sk::FourByteTag::from_chars(
                                tag[0] as char,
                                tag[1] as char,
                                tag[2] as char,
                                tag[3] as char,
                            ),
                            value: *value,
                        },
                    )
                    .collect();
                let args = sk::FontArguments::new().set_variation_design_position(
                    sk::font_arguments::VariationPosition {
                        coordinates: &coordinates,
                    },
                );
                Some(face.clone_with_arguments(&args).unwrap_or(face))
            })
            .clone()
    }

    fn image(&mut self, images: &dyn ImageResolver, id: ImageId) -> Option<sk::Image> {
        let shown = images.shown(id)?;
        let key = (id, shown.frame, shown.version);
        if let Some(image) = self.images.get(&key) {
            return Some(image.clone());
        }
        let Some(decoded) = shown.image else {
            // (no pixels at hand: those uploaded for this version, if they came)
            return self
                .uploads
                .get(&(id, shown.frame))
                .filter(|(version, _)| *version == shown.version)
                .map(|(_, image)| image.clone());
        };
        let frame = decoded.frames.get(shown.frame)?;
        let image = bgra_image(frame.width, frame.height, &frame.pixels)?;
        if self.images.len() >= MAX_CACHED_IMAGES {
            self.images.clear();
        }
        self.images.insert(key, image.clone());
        Some(image)
    }
}

/// A color as Skia's.
fn color(c: Color) -> sk::Color {
    sk::Color::from_argb(c.a, c.r, c.g, c.b)
}

fn color4f(c: Color) -> sk::Color4f {
    sk::Color4f::from(color(c))
}

pub(crate) fn rect(r: &Rect) -> sk::Rect {
    sk::Rect::from_xywh(r.x, r.y, r.width, r.height)
}

fn point(p: Point) -> sk::Point {
    sk::Point::new(p.x, p.y)
}

/// A transform as Skia's (`x' = m11 x + m21 y + dx`, `y' = m12 x + m22 y + dy`).
pub(crate) fn matrix(m: &Matrix) -> sk::Matrix {
    sk::Matrix::new_all(m.m11, m.m21, m.dx, m.m12, m.m22, m.dy, 0.0, 0.0, 1.0)
}

/// A rounded rectangle with CSS's corner radii (Skia scales overlapping radii down as CSS
/// Backgrounds 3 §5.5 does).
pub(crate) fn rrect(r: &Rect, radii: &Radii) -> sk::RRect {
    let radii = radii.fitted(r);
    let corner = |s: Size| sk::Vector::new(s.width.max(0.0), s.height.max(0.0));
    sk::RRect::new_rect_radii(
        rect(r),
        &[
            corner(radii.top_left),
            corner(radii.top_right),
            corner(radii.bottom_right),
            corner(radii.bottom_left),
        ],
    )
}

fn fill(c: Color) -> sk::Paint {
    let mut p = sk::Paint::new(color4f(c), None);
    p.set_anti_alias(true);
    p
}

/// A path of the list as Skia's.
fn path(list: &DisplayList, range: PathRange, evenodd: bool) -> sk::Path {
    let (verbs, points) = list.path(range);
    let mut b = sk::PathBuilder::new();
    let mut at = 0usize;
    let pt = |at: &mut usize| {
        let p = points.get(*at).copied().unwrap_or(Point::new(0.0, 0.0));
        *at += 1;
        point(p)
    };
    for &v in verbs {
        match PathVerb::from_u8(v) {
            Some(PathVerb::Move) => {
                b.move_to(pt(&mut at));
            }
            Some(PathVerb::Line) => {
                b.line_to(pt(&mut at));
            }
            Some(PathVerb::Quad) => {
                let c = pt(&mut at);
                b.quad_to(c, pt(&mut at));
            }
            Some(PathVerb::Cubic) => {
                let c1 = pt(&mut at);
                let c2 = pt(&mut at);
                b.cubic_to(c1, c2, pt(&mut at));
            }
            Some(PathVerb::Close) => {
                b.close();
            }
            None => {}
        }
    }
    b.set_fill_type(if evenodd {
        sk::PathFillType::EvenOdd
    } else {
        sk::PathFillType::Winding
    });
    b.detach()
}

/// What a pop closes: the canvas's save count to go back to, and the painter's transform and
/// snapping before the push.
struct Open {
    count: usize,
    transform: Matrix,
    snap: Option<Snap>,
    kind: OpenKind,
}

enum OpenKind {
    Plain,
    /// A mask group: its layers' items, painted onto the content when it closes.
    Mask {
        layers: Vec<(std::ops::Range<usize>, MaskComposite, bool)>,
    },
    /// A `plus-lighter` group.
    Plus,
}

/// Paints a display list onto a Skia canvas.
pub(crate) struct Painter<'a> {
    canvas: &'a sk::Canvas,
    sources: Sources<'a>,
    caches: &'a mut Caches,
    /// CSS px to device pixels, as the canvas has it.
    transform: Matrix,
    snap: Option<Snap>,
    stack: Vec<Open>,
    /// Open `plus-lighter` groups: what is drawn in them is added (see [`Item::PushPlus`]).
    plus: u32,
}

/// How deep pattern tiles are painted within pattern tiles.
const MAX_PATTERN_DEPTH: u32 = 4;

impl<'a> Painter<'a> {
    pub(crate) fn new(
        canvas: &'a sk::Canvas,
        caches: &'a mut Caches,
        sources: Sources<'a>,
        base: Matrix,
    ) -> Painter<'a> {
        canvas.set_matrix(&sk::M44::from(matrix(&base)));
        Painter {
            canvas,
            sources,
            caches,
            transform: base,
            snap: Snap::of(&base),
            stack: Vec::new(),
            plus: 0,
        }
    }

    pub(crate) fn paint(&mut self, list: &DisplayList) {
        self.paint_items(list, 0..list.items.len(), 0);
        while let Some(open) = self.stack.pop() {
            self.close(list, open, 0);
        }
    }

    fn snapped(&self, r: &Rect) -> Rect {
        self.snap.map_or(*r, |s| s.rect(r))
    }

    /// A paint for drawing `c`, adding inside a `plus-lighter` group.
    fn paint_of(&self, c: Color) -> sk::Paint {
        let mut p = fill(c);
        if self.plus > 0 {
            p.set_blend_mode(sk::BlendMode::Plus);
        }
        p
    }

    fn paint_items(&mut self, list: &DisplayList, range: std::ops::Range<usize>, depth: u32) {
        let mut i = range.start;
        while i < range.end {
            let item = &list.items[i];
            match *item {
                Item::PushMask { bounds, luminance } => {
                    // The mask's items, up to its MaskContent, are its layers; they are painted
                    // when the group closes, onto its content.
                    let mut layers = Vec::new();
                    let mut start = i + 1;
                    let mut layer = (MaskComposite::SourceOver, luminance);
                    let mut nested = 0usize;
                    let mut end = i + 1;
                    while end < range.end {
                        match list.items[end] {
                            Item::PushMask { .. } => nested += 1,
                            Item::MaskContent if nested == 0 => break,
                            Item::MaskContent => nested -= 1,
                            Item::MaskLayer {
                                composite,
                                luminance,
                            } if nested == 0 => {
                                layers.push((start..end, layer.0, layer.1));
                                layer = (composite, luminance);
                                start = end + 1;
                            }
                            _ => {}
                        }
                        end += 1;
                    }
                    layers.push((start..end, layer.0, layer.1));
                    let count = self.canvas.save();
                    self.canvas
                        .clip_rect(rect(&bounds), sk::ClipOp::Intersect, true);
                    self.canvas.save_layer(&sk::canvas::SaveLayerRec::default());
                    self.stack.push(Open {
                        count,
                        transform: self.transform,
                        snap: self.snap,
                        kind: OpenKind::Mask { layers },
                    });
                    i = end + 1;
                    continue;
                }
                Item::PushFilter { filters, bounds } => {
                    let mut nested = 0usize;
                    let mut end = i + 1;
                    while end < range.end {
                        match list.items[end] {
                            Item::PushFilter { .. } => nested += 1,
                            Item::PopFilter if nested == 0 => break,
                            Item::PopFilter => nested -= 1,
                            _ => {}
                        }
                        end += 1;
                    }
                    self.filter_group(list, i + 1..end, list.filter(filters), bounds, depth);
                    i = end + 1;
                    continue;
                }
                // A group of opacity 0 is composited fully transparent (CSS Color 4 §3.2):
                // nothing of it shows, so nothing of it is drawn.
                Item::PushOpacity { opacity, .. } if opacity <= 0.0 => {
                    i = group_end(list, i, range.end) + 1;
                    continue;
                }
                _ => {}
            }
            self.item(list, item, depth);
            i += 1;
        }
    }

    fn push(&mut self, count: usize, kind: OpenKind) {
        self.stack.push(Open {
            count,
            transform: self.transform,
            snap: self.snap,
            kind,
        });
    }

    fn close(&mut self, list: &DisplayList, open: Open, depth: u32) {
        match open.kind {
            OpenKind::Plain => {}
            OpenKind::Plus => self.plus = self.plus.saturating_sub(1),
            OpenKind::Mask { layers } => {
                // The mask (its layers composited in order) onto the content, destination-in:
                // the content keeps the mask's coverage.
                self.transform = open.transform;
                self.snap = open.snap;
                let plus = std::mem::take(&mut self.plus);
                let mut onto = sk::Paint::default();
                onto.set_blend_mode(sk::BlendMode::DstIn);
                self.canvas
                    .save_layer(&sk::canvas::SaveLayerRec::default().paint(&onto));
                for (n, (items, composite, luminance)) in layers.into_iter().enumerate() {
                    let mut p = sk::Paint::default();
                    p.set_blend_mode(if n == 0 {
                        sk::BlendMode::SrcOver
                    } else {
                        mask_blend(composite)
                    });
                    if luminance {
                        p.set_color_filter(sk::luma_color_filter::new());
                    }
                    let count = self
                        .canvas
                        .save_layer(&sk::canvas::SaveLayerRec::default().paint(&p));
                    let stack = std::mem::take(&mut self.stack);
                    self.paint_items(list, items, depth);
                    while let Some(inner) = self.stack.pop() {
                        self.close(list, inner, depth);
                    }
                    self.stack = stack;
                    self.canvas.restore_to_count(count);
                }
                self.plus = plus;
            }
        }
        self.canvas.restore_to_count(open.count);
        self.transform = open.transform;
        self.snap = open.snap;
    }

    fn item(&mut self, list: &DisplayList, item: &Item, depth: u32) {
        let canvas = self.canvas;
        match *item {
            Item::FillRect { rect: r, color: c } => {
                let r = self.snapped(&r);
                canvas.draw_rect(rect(&r), &self.paint_of(c));
            }
            Item::FillRoundedRect {
                rect: r,
                radii,
                color: c,
            } => {
                let r = self.snapped(&r);
                canvas.draw_rrect(rrect(&r, &radii), &self.paint_of(c));
            }
            Item::StrokeRect {
                rect: r,
                width,
                color: c,
            } => {
                let mut p = self.paint_of(c);
                p.set_style(sk::PaintStyle::Stroke);
                p.set_stroke_width(width);
                canvas.draw_rect(rect(&r), &p);
            }
            Item::StrokeRoundedRect {
                rect: r,
                radii,
                width,
                color: c,
            } => {
                let mut p = self.paint_of(c);
                p.set_style(sk::PaintStyle::Stroke);
                p.set_stroke_width(width);
                canvas.draw_rrect(rrect(&r, &radii), &p);
            }
            Item::Border(ref b) => {
                let b = match self.snap {
                    Some(snap) => Border {
                        rect: snap.rect(&b.rect),
                        widths: snap.border_widths(&b.widths),
                        ..*b
                    },
                    None => *b,
                };
                self.border(&b);
            }
            Item::Text(ref run) => self.text(list, run),
            Item::Image {
                image,
                dest,
                src,
                opacity,
                rendering,
            } => {
                let dest = self.snapped(&dest);
                self.image(image, &dest, &src, opacity, rendering);
            }
            Item::Video {
                image,
                dest,
                clip,
                transparent,
                ..
            } => {
                let (dest, clip) = (self.snapped(&dest), self.snapped(&clip));
                match image {
                    Some(image) => {
                        canvas.save();
                        canvas.clip_rect(rect(&clip), sk::ClipOp::Intersect, false);
                        self.image(image, &dest, &Rect::ZERO, 1.0, ImageRendering::Smooth);
                        canvas.restore();
                    }
                    // (a canvas's frames: what is under them shows)
                    None if transparent => {}
                    None => {
                        canvas.draw_rect(rect(&dest.intersect(&clip)), &fill(Color::BLACK));
                    }
                }
            }
            Item::ProtectedVideo { dest, clip, .. } => {
                // (black: its frames are the compositor's to show, never drawn here)
                let shown = self.snapped(&dest).intersect(&self.snapped(&clip));
                canvas.draw_rect(rect(&shown), &fill(Color::BLACK));
            }
            Item::LinearGradient {
                rect: r,
                radii,
                start,
                end,
                stops,
                extend,
            } => {
                let r = self.snapped(&r);
                let (colors, offsets) = gradient_stops(list.stops(stops));
                let shader = gradients::linear_gradient(
                    (point(start), point(end)),
                    &gradient(&colors, &offsets, extend),
                    None,
                );
                self.fill_shader(&r, &radii, shader, list.stops(stops));
            }
            Item::RadialGradient {
                rect: r,
                radii,
                center,
                radius,
                stops,
                extend,
            } => {
                let r = self.snapped(&r);
                let (colors, offsets) = gradient_stops(list.stops(stops));
                let shader = (radius.width > 0.0 && radius.height > 0.0)
                    .then(|| {
                        // an ellipse: the circle of radius `width`, scaled vertically
                        let scale = sk::Matrix::scale_translate(
                            (1.0, radius.height / radius.width),
                            (0.0, center.y - center.y * radius.height / radius.width),
                        );
                        gradients::radial_gradient(
                            (point(center), radius.width),
                            &gradient(&colors, &offsets, extend),
                            &scale,
                        )
                    })
                    .flatten();
                self.fill_shader(&r, &radii, shader, list.stops(stops));
            }
            Item::ConicGradient {
                rect: r,
                radii,
                center,
                angle,
                stops,
                extend,
            } => {
                let r = self.snapped(&r);
                let all = list.stops(stops);
                let (colors, mut offsets) = gradient_stops(all);
                // (Skia's sweep starts on the x axis; CSS's at `angle` from up, both clockwise.
                // A repeating gradient repeats its first-to-last span.)
                let (first, last) = match (offsets.first(), offsets.last()) {
                    (Some(&a), Some(&b)) => (a, b),
                    _ => (0.0, 1.0),
                };
                let (from, to) = if extend == Extend::Repeat && last > first {
                    for o in &mut offsets {
                        *o = (*o - first) / (last - first);
                    }
                    (first * 360.0, last * 360.0)
                } else {
                    (0.0, 360.0)
                };
                let rotate = sk::Matrix::rotate_deg_pivot(angle.to_degrees() - 90.0, point(center));
                let shader = gradients::sweep_gradient(
                    point(center),
                    (from, to),
                    &gradient(&colors, &offsets, extend),
                    &rotate,
                );
                self.fill_shader(&r, &radii, shader, all);
            }
            Item::BoxShadow(ref s) => {
                let s = BoxShadow {
                    rect: self.snapped(&s.rect),
                    ..*s
                };
                self.box_shadow(&s);
            }
            Item::FillPattern {
                rect: r,
                pattern,
                transform,
            } => {
                if let Some(p) = list.patterns.get(pattern as usize) {
                    self.fill_pattern(p, &r, &transform, depth);
                }
            }
            Item::FillPath {
                path: range,
                color: c,
                evenodd,
                antialias,
                ..
            } => {
                let mut p = self.paint_of(c);
                p.set_anti_alias(antialias);
                canvas.draw_path(&path(list, range, evenodd), &p);
            }
            Item::StrokePath {
                path: range,
                color: c,
                ref stroke,
                antialias,
                ..
            } => {
                let mut p = self.paint_of(c);
                p.set_anti_alias(antialias);
                stroke_paint(&mut p, stroke, list.dashes(stroke.dashes));
                canvas.draw_path(&path(list, range, false), &p);
            }
            Item::PushClipRect { rect: r } => {
                let count = canvas.save();
                // (snapped to device pixels under a scale and translation: a hard edge, as
                // Chrome's pixel-snapped clips are)
                let aa = self.snap.is_none();
                canvas.clip_rect(rect(&self.snapped(&r)), sk::ClipOp::Intersect, aa);
                self.push(count, OpenKind::Plain);
            }
            Item::PushClipRoundedRect { rect: r, radii } => {
                let count = canvas.save();
                canvas.clip_rrect(
                    rrect(&self.snapped(&r), &radii),
                    sk::ClipOp::Intersect,
                    true,
                );
                self.push(count, OpenKind::Plain);
            }
            Item::PushClipPath {
                path: range,
                evenodd,
                ..
            } => {
                let count = canvas.save();
                canvas.clip_path(&path(list, range, evenodd), sk::ClipOp::Intersect, true);
                self.push(count, OpenKind::Plain);
            }
            Item::PushTransform { matrix: m } => {
                let count = canvas.save();
                let previous = self.transform;
                let snap = self.snap;
                self.push(count, OpenKind::Plain);
                self.transform = m.then(&previous);
                self.snap = snap
                    .filter(|s| s.keeps(&m))
                    .and_then(|_| Snap::of(&self.transform));
                canvas.concat(&matrix(&m));
            }
            Item::PushOpacity { opacity, bounds } => {
                let count = if opacity >= 1.0 {
                    canvas.save()
                } else {
                    let mut p = sk::Paint::default();
                    p.set_alpha_f(opacity);
                    if self.plus > 0 {
                        p.set_blend_mode(sk::BlendMode::Plus);
                    }
                    let b = rect(&bounds);
                    let count = canvas
                        .save_layer(&sk::canvas::SaveLayerRec::default().bounds(&b).paint(&p));
                    count
                };
                self.push(count, OpenKind::Plain);
            }
            Item::PushPlus => {
                // (the drawings are added in a layer of their own, whose sum is composited over
                // what is below once)
                let count = canvas.save_layer(&sk::canvas::SaveLayerRec::default());
                self.push(count, OpenKind::Plus);
                self.plus += 1;
            }
            Item::BackdropFilter {
                filters,
                rect: r,
                radii,
                ..
            } => self.backdrop_filter(list.filter(filters), &r, &radii),
            // (a layer is for a compositor; painted whole, the list ignores it)
            Item::PushLayer { .. } | Item::PopLayer => {}
            // (a mask's and a filter group's items are taken where their push is met)
            Item::PushMask { .. } | Item::MaskLayer { .. } | Item::MaskContent => {}
            Item::PushFilter { .. } | Item::PopFilter => {}
            Item::PopClip
            | Item::PopTransform
            | Item::PopOpacity
            | Item::PopMask
            | Item::PopPlus => {
                if let Some(open) = self.stack.pop() {
                    self.close(list, open, depth);
                }
            }
        }
    }

    /// Fills `r` rounded by `radii` with `shader`; without one (a degenerate gradient), with the
    /// last stop's color, as a gradient of zero length shows (CSS Images 3 §3.1).
    fn fill_shader(
        &self,
        r: &Rect,
        radii: &Radii,
        shader: Option<sk::Shader>,
        stops: &[GradientStop],
    ) {
        let mut p = match shader {
            Some(shader) => {
                let mut p = sk::Paint::default();
                p.set_shader(shader);
                p.set_anti_alias(true);
                p
            }
            None => match stops.last() {
                Some(s) => fill(s.color),
                None => return,
            },
        };
        if self.plus > 0 {
            p.set_blend_mode(sk::BlendMode::Plus);
        }
        if radii.is_zero() {
            self.canvas.draw_rect(rect(r), &p);
        } else {
            self.canvas.draw_rrect(rrect(r, radii), &p);
        }
    }

    fn image(
        &mut self,
        id: ImageId,
        dest: &Rect,
        src: &Rect,
        opacity: f32,
        rendering: ImageRendering,
    ) {
        let Some(image) = self.caches.image(self.sources.images, id) else {
            return;
        };
        let src = if src.is_empty() {
            sk::Rect::from_wh(image.width() as f32, image.height() as f32)
        } else {
            rect(src)
        };
        let sampling = match rendering {
            ImageRendering::Pixelated => {
                sk::SamplingOptions::new(sk::FilterMode::Nearest, sk::MipmapMode::None)
            }
            ImageRendering::Smooth => {
                sk::SamplingOptions::new(sk::FilterMode::Linear, sk::MipmapMode::Linear)
            }
        };
        let mut p = sk::Paint::default();
        p.set_alpha_f(opacity);
        p.set_anti_alias(true);
        if self.plus > 0 {
            p.set_blend_mode(sk::BlendMode::Plus);
        }
        self.canvas.draw_image_rect_with_sampling_options(
            &image,
            Some((&src, sk::canvas::SrcRectConstraint::Fast)),
            rect(dest),
            sampling,
            &p,
        );
    }

    fn text(&mut self, list: &DisplayList, run: &TextRun) {
        let Some(data) = self.sources.text.font_data(run.font) else {
            return;
        };
        let Some(face) = self.caches.typeface(&data) else {
            return;
        };
        let (ids, advances, offsets) = list.glyphs(run.glyphs);
        if ids.is_empty() {
            return;
        }
        let mut font = sk::Font::from_typeface(face, run.size);
        font.set_edging(sk::font::Edging::AntiAlias);
        font.set_subpixel(true);
        font.set_hinting(sk::FontHinting::Slight);
        font.set_embolden(data.embolden);
        if data.oblique {
            font.set_skew_x(-0.25);
        }
        // Glyphs keep their subpixel position along the line; the baseline is on a device pixel
        // row, as Chrome's text is.
        let y = self.snap.map_or(run.origin.y, |s| s.y(run.origin.y));
        let rtl = run.bidi_level % 2 == 1;
        let width: f32 = advances.iter().sum();
        let mut pen = if rtl {
            run.origin.x + width
        } else {
            run.origin.x
        };
        let mut builder = sk::TextBlobBuilder::new();
        if run.sideways {
            // Each glyph turned a quarter turn left about its vertical origin (half its advance
            // across, the ascent above), which sits on the pen.
            let metrics = font.metrics().1;
            let ascent = -metrics.ascent;
            let mut widths = vec![0.0; ids.len()];
            font.get_widths(ids, &mut widths);
            let (glyphs, xforms) = builder.alloc_run_rsxform(&font, ids.len());
            glyphs.copy_from_slice(ids);
            for i in 0..ids.len() {
                let (adv, off) = (advances[i], offsets[i]);
                let x = if rtl {
                    pen - adv - off.advance_offset
                } else {
                    pen + off.advance_offset
                };
                let half = widths[i] / 2.0;
                xforms[i] =
                    sk::RSXform::new(0.0, -1.0, (x + ascent, y - off.ascender_offset + half));
                if rtl {
                    pen -= adv;
                } else {
                    pen += adv;
                }
            }
        } else {
            let (glyphs, positions) = builder.alloc_run_pos(&font, ids.len(), None);
            glyphs.copy_from_slice(ids);
            for i in 0..ids.len() {
                let (adv, off) = (advances[i], offsets[i]);
                let x = if rtl {
                    pen - adv - off.advance_offset
                } else {
                    pen + off.advance_offset
                };
                positions[i] = sk::Point::new(x, y - off.ascender_offset);
                if rtl {
                    pen -= adv;
                } else {
                    pen += adv;
                }
            }
        }
        let Some(blob) = builder.make() else {
            return;
        };
        let mut p = self.paint_of(run.color);
        if run.stroke > 0.0 {
            p.set_style(sk::PaintStyle::Stroke);
            p.set_stroke_width(run.stroke);
        }
        self.canvas.draw_text_blob(&blob, (0.0, 0.0), &p);
    }

    /// A box's border (CSS Backgrounds 3 §4): one ring when every side is solid in one color,
    /// otherwise each side inside its region, from the outer corners through the inner corners
    /// (to the middle of the box where corners are rounded).
    fn border(&mut self, b: &Border) {
        let outer = b.rect;
        let outer_r = b.radii.fitted(&outer);
        let inner = outer.deflate(&b.widths);
        let inner_r = outer_r.inner(&b.widths);
        let widths = b.widths.to_array();
        let visible = |i: usize| {
            !b.styles[i].is_invisible() && !b.colors[i].is_transparent() && widths[i] > 0.0
        };
        let uniform = (1..4).all(|i| b.colors[i] == b.colors[0] && b.styles[i] == b.styles[0])
            && (0..4).all(visible);
        if uniform && b.styles[0] == BorderStyle::Solid {
            self.ring(&outer, &outer_r, &inner, &inner_r, b.colors[0]);
            return;
        }
        let (ol, ot, or, ob) = (outer.x, outer.y, outer.right(), outer.bottom());
        let (il, it, ir, ib) = (inner.x, inner.y, inner.right(), inner.bottom());
        let mid = (outer.x + outer.width / 2.0, outer.y + outer.height / 2.0);
        let square = outer_r.is_zero();
        let regions = [
            [(ol, ot), (or, ot), (ir, it), mid, (il, it)],
            [(or, ot), (or, ob), (ir, ib), mid, (ir, it)],
            [(or, ob), (ol, ob), (il, ib), mid, (ir, ib)],
            [(ol, ob), (ol, ot), (il, it), mid, (il, ib)],
        ];
        for (side, region) in regions.iter().enumerate() {
            if !visible(side) {
                continue;
            }
            let points: Vec<sk::Point> = if square {
                [region[0], region[1], region[2], region[4]]
                    .iter()
                    .map(|p| sk::Point::new(p.0, p.1))
                    .collect()
            } else {
                region.iter().map(|p| sk::Point::new(p.0, p.1)).collect()
            };
            let clip = sk::Path::polygon(&points, true, None, None);
            self.canvas.save();
            self.canvas.clip_path(&clip, sk::ClipOp::Intersect, true);
            self.border_side(b, side, &outer, &outer_r, &inner, &inner_r);
            self.canvas.restore();
        }
    }

    /// The ring between two rounded rectangles in `c`.
    fn ring(&self, outer: &Rect, outer_r: &Radii, inner: &Rect, inner_r: &Radii, c: Color) {
        let p = self.paint_of(c);
        if inner.is_empty() {
            self.canvas.draw_rrect(rrect(outer, outer_r), &p);
        } else {
            self.canvas
                .draw_drrect(rrect(outer, outer_r), rrect(inner, inner_r), &p);
        }
    }

    /// One side of a border, drawn over the whole ring; the caller clips it to the side.
    fn border_side(
        &self,
        b: &Border,
        side: usize,
        outer: &Rect,
        outer_r: &Radii,
        inner: &Rect,
        inner_r: &Radii,
    ) {
        let color = b.colors[side];
        let w = &b.widths;
        let scaled = |f: f32| Sides::new(w.top * f, w.right * f, w.bottom * f, w.left * f);
        // the ring between fractions a and b of the border width (0 = outer edge, 1 = inner edge)
        let band = |a: f32, bb: f32, c: Color| {
            self.ring(
                &outer.deflate(&scaled(a)),
                &outer_r.inner(&scaled(a)),
                &outer.deflate(&scaled(bb)),
                &outer_r.inner(&scaled(bb)),
                c,
            )
        };
        // top and left are the "light source" sides for inset/outset/groove/ridge
        let top_left = side == 0 || side == 3;
        let (dark, light) = (color.shade(0.6), color);
        match b.styles[side] {
            BorderStyle::None | BorderStyle::Hidden => {}
            BorderStyle::Solid => self.ring(outer, outer_r, inner, inner_r, color),
            BorderStyle::Double => {
                band(0.0, 1.0 / 3.0, color);
                band(2.0 / 3.0, 1.0, color);
            }
            BorderStyle::Inset | BorderStyle::Outset => {
                let inset = b.styles[side] == BorderStyle::Inset;
                band(0.0, 1.0, if inset == top_left { dark } else { light });
            }
            BorderStyle::Groove | BorderStyle::Ridge => {
                let groove = b.styles[side] == BorderStyle::Groove;
                let (first, second) = if groove == top_left {
                    (dark, light)
                } else {
                    (light, dark)
                };
                band(0.0, 0.5, first);
                band(0.5, 1.0, second);
            }
            BorderStyle::Dashed | BorderStyle::Dotted if outer_r.is_zero() => {
                self.dashes(b.styles[side] == BorderStyle::Dotted, side, outer, w, color);
            }
            BorderStyle::Dashed | BorderStyle::Dotted => {
                // stroked along the middle of the border, as wide as this side
                let mid = outer.deflate(&scaled(0.5));
                let mid_r = outer_r.inner(&scaled(0.5));
                let width = w.to_array()[side];
                let dotted = b.styles[side] == BorderStyle::Dotted;
                let mut p = self.paint_of(color);
                p.set_style(sk::PaintStyle::Stroke);
                p.set_stroke_width(width);
                if dotted {
                    p.set_stroke_cap(sk::PaintCap::Round);
                    p.set_path_effect(sk::PathEffect::dash(&[0.0, 2.0 * width], 0.0));
                } else {
                    p.set_path_effect(sk::PathEffect::dash(&[2.0 * width, width], 0.0));
                }
                self.canvas.draw_rrect(rrect(&mid, &mid_r), &p);
            }
        }
        let _ = inner;
    }

    /// A straight dashed or dotted side as Chrome draws it: dashes along the side's middle line
    /// from one outer corner to the other, starting and ending with a dash, the gaps stretched or
    /// shrunk to fit ([`dash_gap`]). Dashes are twice the width long with gaps of one width
    /// (three and two below 3 px); dots are one width long with gaps of one width, round from
    /// 3 px up and square below.
    fn dashes(&self, dotted: bool, side: usize, outer: &Rect, w: &Sides, c: Color) {
        let width = w.to_array()[side];
        if width <= 0.0 {
            return;
        }
        let horizontal = side == 0 || side == 2;
        let length = if horizontal {
            outer.width
        } else {
            outer.height
        };
        let (dash, gap) = match (dotted, width >= 3.0) {
            (true, _) => (width, width),
            (false, true) => (2.0 * width, width),
            (false, false) => (3.0 * width, 2.0 * width),
        };
        let Some((count, gap)) = dash_gap(length, dash, gap) else {
            return;
        };
        let (x0, y0) = match side {
            1 => (outer.right() - width, outer.y),
            2 => (outer.x, outer.bottom() - width),
            _ => (outer.x, outer.y),
        };
        let p = self.paint_of(c);
        let round = dotted && width >= 3.0;
        for i in 0..count {
            let along = i as f32 * (dash + gap);
            let r = if horizontal {
                Rect::new(x0 + along, y0, dash.min(length - along), width)
            } else {
                Rect::new(x0, y0 + along, width, dash.min(length - along))
            };
            if round {
                self.canvas.draw_circle(
                    (r.x + r.width / 2.0, r.y + r.height / 2.0),
                    width / 2.0,
                    &p,
                );
            } else {
                self.canvas.draw_rect(rect(&r), &p);
            }
        }
    }

    /// A blurred box shadow (CSS Backgrounds 3 §7.2): an outer one only outside the border box,
    /// an inset one only inside the padding box. The blur's standard deviation is half the blur
    /// radius.
    fn box_shadow(&self, s: &BoxShadow) {
        let canvas = self.canvas;
        let mut p = self.paint_of(s.color);
        let sigma = s.blur / 2.0;
        if sigma > 0.0 {
            p.set_mask_filter(sk::MaskFilter::blur(sk::BlurStyle::Normal, sigma, false));
        }
        let size = Size::new(s.rect.width, s.rect.height);
        canvas.save();
        if !s.inset {
            canvas.clip_rrect(rrect(&s.rect, &s.radii), sk::ClipOp::Difference, true);
            let shadow = s
                .rect
                .inflate(s.spread, s.spread)
                .translate(s.offset.x, s.offset.y);
            if !shadow.is_empty() {
                canvas.draw_rrect(rrect(&shadow, &s.radii.outset(s.spread, size)), &p);
            }
        } else {
            canvas.clip_rrect(rrect(&s.rect, &s.radii), sk::ClipOp::Intersect, true);
            let hole = s
                .rect
                .inflate(-s.spread, -s.spread)
                .translate(s.offset.x, s.offset.y);
            let reach = 3.0 * sigma + s.offset.x.abs().max(s.offset.y.abs()) + s.spread.abs() + 1.0;
            let outer = s.rect.inflate(reach, reach);
            if hole.is_empty() {
                canvas.draw_rect(rect(&outer), &p);
            } else {
                let hole_r = s.radii.outset(-s.spread, size);
                canvas.draw_drrect(sk::RRect::new_rect(rect(&outer)), rrect(&hole, &hole_r), &p);
            }
        }
        canvas.restore();
    }

    /// Fills `r` with a pattern's tile repeated (SVG 2 §14.3): the tile's list recorded once as a
    /// picture, drawn through a repeating picture shader.
    fn fill_pattern(&mut self, pattern: &Pattern, r: &Rect, transform: &Matrix, depth: u32) {
        if depth >= MAX_PATTERN_DEPTH || pattern.tile.width <= 0.0 || pattern.tile.height <= 0.0 {
            return;
        }
        let tile = sk::Rect::from_wh(pattern.tile.width, pattern.tile.height);
        let mut recorder = sk::PictureRecorder::new();
        {
            let canvas = recorder.begin_recording(tile, false);
            canvas.clip_rect(tile, sk::ClipOp::Intersect, false);
            let mut painter = Painter {
                canvas,
                sources: self.sources,
                caches: &mut *self.caches,
                transform: transform.then(&self.transform),
                snap: None,
                stack: Vec::new(),
                plus: 0,
            };
            painter.paint_items(&pattern.list, 0..pattern.list.items.len(), depth + 1);
            while let Some(open) = painter.stack.pop() {
                painter.close(&pattern.list, open, depth + 1);
            }
        }
        let Some(picture) = recorder.finish_recording_as_picture(Some(&tile)) else {
            return;
        };
        let local = matrix(transform);
        let shader = picture.to_shader(
            (sk::TileMode::Repeat, sk::TileMode::Repeat),
            sk::FilterMode::Linear,
            Some(&local),
            Some(&tile),
        );
        let mut p = sk::Paint::default();
        p.set_shader(shader);
        p.set_anti_alias(true);
        if self.plus > 0 {
            p.set_blend_mode(sk::BlendMode::Plus);
        }
        self.canvas.draw_rect(rect(r), &p);
    }

    /// A filter group (Filter Effects 1 §10): its content painted in a layer whose paint applies
    /// the functions in order.
    fn filter_group(
        &mut self,
        list: &DisplayList,
        items: std::ops::Range<usize>,
        ops: &[FilterOp],
        bounds: Rect,
        depth: u32,
    ) {
        let filter = filters::image_filter(ops, &bounds, self);
        let mut p = sk::Paint::default();
        if self.plus > 0 {
            p.set_blend_mode(sk::BlendMode::Plus);
        }
        let count = match filter {
            Some(Some(f)) => {
                p.set_image_filter(f);
                self.canvas.save();
                self.canvas
                    .clip_rect(rect(&bounds), sk::ClipOp::Intersect, true);
                self.canvas
                    .save_layer(&sk::canvas::SaveLayerRec::default().paint(&p));
                self.canvas.save_count() - 2
            }
            // (no filter: the content as it is)
            Some(None) => self.canvas.save(),
            // (a filter that cannot be made shows nothing, as an invalid one, §5.1)
            None => return,
        };
        let stack = std::mem::take(&mut self.stack);
        let plus = std::mem::take(&mut self.plus);
        self.paint_items(list, items, depth);
        while let Some(open) = self.stack.pop() {
            self.close(list, open, depth);
        }
        self.stack = stack;
        self.plus = plus;
        self.canvas.restore_to_count(count);
    }

    /// A backdrop filter (Filter Effects 2 §3): what is below in the current layer, through the
    /// filters, with the edges mirrored, drawn clipped to the rounded rectangle.
    fn backdrop_filter(&mut self, ops: &[FilterOp], r: &Rect, radii: &Radii) {
        let Some(Some(filter)) = filters::image_filter(ops, r, self) else {
            return;
        };
        let canvas = self.canvas;
        canvas.save();
        canvas.clip_rrect(rrect(r, radii), sk::ClipOp::Intersect, true);
        let bounds = rect(r);
        canvas.save_layer(
            &sk::canvas::SaveLayerRec::default()
                .bounds(&bounds)
                .backdrop(&filter)
                .backdrop_tile_mode(sk::TileMode::Mirror),
        );
        canvas.restore();
        canvas.restore();
    }

    pub(crate) fn caches(&mut self) -> &mut Caches {
        self.caches
    }

    pub(crate) fn images(&self) -> &'a dyn ImageResolver {
        self.sources.images
    }
}

/// The index of the pop that closes the group pushed at `push`.
fn group_end(list: &DisplayList, push: usize, end: usize) -> usize {
    let mut depth = 0usize;
    for (i, item) in list.items[push..end].iter().enumerate() {
        match item {
            Item::PushClipRect { .. }
            | Item::PushClipRoundedRect { .. }
            | Item::PushClipPath { .. }
            | Item::PushTransform { .. }
            | Item::PushOpacity { .. }
            | Item::PushLayer { .. }
            | Item::PushPlus
            | Item::PushFilter { .. }
            | Item::PushMask { .. } => depth += 1,
            Item::PopClip
            | Item::PopTransform
            | Item::PopOpacity
            | Item::PopLayer
            | Item::PopPlus
            | Item::PopFilter
            | Item::PopMask => {
                depth -= 1;
                if depth == 0 {
                    return push + i;
                }
            }
            _ => {}
        }
    }
    end.saturating_sub(1)
}

/// A mask layer's composite operator as a blend mode (the layer the source, those below the
/// destination).
fn mask_blend(op: MaskComposite) -> sk::BlendMode {
    match op {
        MaskComposite::SourceOver => sk::BlendMode::SrcOver,
        MaskComposite::SourceIn => sk::BlendMode::SrcIn,
        MaskComposite::SourceOut => sk::BlendMode::SrcOut,
        MaskComposite::SourceAtop => sk::BlendMode::SrcATop,
        MaskComposite::DestinationOver => sk::BlendMode::DstOver,
        MaskComposite::DestinationIn => sk::BlendMode::DstIn,
        MaskComposite::DestinationOut => sk::BlendMode::DstOut,
        MaskComposite::DestinationAtop => sk::BlendMode::DstATop,
        MaskComposite::Xor => sk::BlendMode::Xor,
        MaskComposite::Plus => sk::BlendMode::Plus,
        MaskComposite::Copy => sk::BlendMode::Src,
        MaskComposite::Clear => sk::BlendMode::Clear,
    }
}

/// A gradient's stops; CSS gradients interpolate in premultiplied sRGB (CSS Images 4 §3.5.1).
fn gradient<'a>(colors: &'a [sk::Color4f], offsets: &'a [f32], extend: Extend) -> Gradient<'a> {
    Gradient::new(
        Colors::new(colors, Some(offsets), tile_mode(extend), None),
        Interpolation {
            in_premul: skia_safe::gradient::interpolation::InPremul::Yes,
            ..Interpolation::default()
        },
    )
}

fn tile_mode(extend: Extend) -> sk::TileMode {
    match extend {
        Extend::Clamp => sk::TileMode::Clamp,
        Extend::Repeat => sk::TileMode::Repeat,
    }
}

fn gradient_stops(stops: &[GradientStop]) -> (Vec<sk::Color4f>, Vec<f32>) {
    let mut colors = Vec::with_capacity(stops.len().max(2));
    let mut offsets = Vec::with_capacity(stops.len().max(2));
    for s in stops {
        colors.push(color4f(s.color));
        offsets.push(s.offset.clamp(0.0, 1.0));
    }
    if stops.len() == 1 {
        colors.push(colors[0]);
        offsets.push(1.0);
    }
    (colors, offsets)
}

/// A paint's stroke as `stroke` says (SVG 2 13.5).
fn stroke_paint(p: &mut sk::Paint, stroke: &Stroke, dashes: &[f32]) {
    p.set_style(sk::PaintStyle::Stroke);
    p.set_stroke_width(stroke.width);
    p.set_stroke_cap(match stroke.cap {
        LineCap::Butt => sk::PaintCap::Butt,
        LineCap::Round => sk::PaintCap::Round,
        LineCap::Square => sk::PaintCap::Square,
    });
    p.set_stroke_join(match stroke.join {
        LineJoin::Miter => sk::PaintJoin::Miter,
        LineJoin::Round => sk::PaintJoin::Round,
        LineJoin::Bevel => sk::PaintJoin::Bevel,
    });
    p.set_stroke_miter(stroke.miter_limit);
    if !dashes.is_empty() && dashes.iter().any(|d| *d > 0.0) {
        p.set_path_effect(sk::PathEffect::dash(dashes, stroke.dash_offset));
    }
}

/// How many dashes of `dash` fit along an open line of `length` starting and ending with a dash,
/// and the gap between them: the number whose gap is nearest to `gap`. `None` when not even one
/// dash fits.
fn dash_gap(length: f32, dash: f32, gap: f32) -> Option<(usize, f32)> {
    if !(length > 0.0 && dash > 0.0) {
        return None;
    }
    if length <= dash {
        return Some((1, 0.0));
    }
    let min = ((length + gap) / (dash + gap)).floor().max(1.0);
    let max = min + 1.0;
    let gap_for = |n: f32| {
        if n <= 1.0 {
            f32::INFINITY
        } else {
            (length - n * dash) / (n - 1.0)
        }
    };
    let (g_min, g_max) = (gap_for(min), gap_for(max));
    let n = if g_max < 0.0 || (g_min - gap).abs() < (g_max - gap).abs() {
        min
    } else {
        max
    };
    let g = gap_for(n);
    Some((n as usize, if g.is_finite() { g } else { 0.0 }))
}

/// Software rendering into memory on Skia's CPU backend: what `--render`, reftests and the tests
/// use.
pub struct HeadlessRenderer {
    caches: Caches,
}

impl HeadlessRenderer {
    pub fn new() -> Result<HeadlessRenderer> {
        Ok(HeadlessRenderer {
            caches: Caches::default(),
        })
    }

    /// Grayscale text antialiasing on opaque targets too: text is always grayscale here.
    pub fn set_grayscale_text(&mut self, _on: bool) {}

    /// Renders `list` at `view`'s size and scale.
    pub fn render(
        &mut self,
        list: &DisplayList,
        sources: Sources,
        view: &Viewport,
    ) -> Result<Pixmap> {
        let (w, h) = (view.width.max(1), view.height.max(1));
        let info = sk::ImageInfo::new(
            (w as i32, h as i32),
            sk::ColorType::BGRA8888,
            sk::AlphaType::Premul,
            None,
        );
        let mut surface = sk::surfaces::raster(&info, None, None).ok_or(Error::Unsupported)?;
        let canvas = surface.canvas();
        canvas.clear(color(view.background));
        {
            let mut painter = Painter::new(
                canvas,
                &mut self.caches,
                sources,
                Matrix::scale(view.scale, view.scale),
            );
            painter.paint(list);
        }
        let mut data = vec![0u8; w as usize * h as usize * 4];
        if !surface.read_pixels(&info, &mut data, w as usize * 4, (0, 0)) {
            return Err(Error::Unsupported);
        }
        Ok(Pixmap {
            width: w,
            height: h,
            data,
        })
    }
}
