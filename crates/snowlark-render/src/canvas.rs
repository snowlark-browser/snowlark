// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The 2D drawing context of a `<canvas>` (HTML §4.12.5.1 `CanvasRenderingContext2D`): the drawing
//! state and its stack, the current default path (points transformed by the current transformation
//! matrix as they are added), fill and stroke styles (colours, gradients, patterns), rectangles,
//! paths, clipping, compositing, shadows, images, pixel access and text.
//!
//! The bitmap is on the GPU by default: [`snowlark_paint::canvas::GpuCanvas`], Direct2D with text
//! drawn through DirectWrite, on the page thread's canvas thread, which replays the operations this
//! thread records ([`snowlark_paint::canvas_thread::RemoteCanvas`]); text is shaped here. The CPU backend (tiny-skia, with
//! text laid out by usvg; shadows not drawn) stays for comparison and for machines without Direct3D:
//! `SNOWLARK_CANVAS=skia` selects it, `SNOWLARK_CANVAS=warp` the GPU backend on Windows' software
//! rasterizer, and a canvas whose GPU bitmap cannot be made falls back to it.
//!
//! Paths remember whether they are one rectangle or one full circle in device space
//! ([`PathShape`]), so fills and strokes of those take Direct2D's primitives (and small circles its
//! sprite batches) instead of path geometries.

use std::sync::{Arc, Mutex};

use snowlark_paint::canvas::{
    CanvasClip, CompositeOp, DrawParams, FillShape, GradientStop, LineCap as GpuCap,
    LineJoin as GpuJoin, PathData, PathVerb, Shadow, StrokeParams,
};
use snowlark_paint::canvas_thread::{RemoteCanvas as GpuCanvas, RemoteImage as CanvasImage};

/// What a GPU canvas operation paints with, its images on the canvas thread.
type CanvasPaint<'a> = snowlark_paint::canvas::CanvasPaint<'a, CanvasImage>;
use snowlark_paint::image::{DecodedImage, Frame};
use snowlark_paint::{
    Direction, Driver, FontListId, FontQuery, FontStretch, FontStyle, ShapeKey, ShapeOptions,
    ShapedText, TextCache, TextEngine, VerticalText, WordCache,
};

use crate::env::{face_rank, StyleEnv, Wanted};
use resvg::tiny_skia::{
    self, BlendMode, FillRule, FilterQuality, GradientStop as SkiaStop, LineCap, LineJoin, Mask,
    Paint, PathBuilder, PathSegment, Pixmap, PixmapPaint, Point, SpreadMode, Stroke, StrokeDash,
    Transform,
};
use resvg::usvg;

pub use resvg::tiny_skia as skia;
pub use svgtypes;

/// What canvas text is laid out with: the document's text engine (system fonts and the web fonts
/// it loaded) and its `@font-face` families; for the CPU backend, usvg's system font database.
#[derive(Clone)]
pub struct Fonts {
    pub text: Arc<TextEngine>,
    pub env: Arc<StyleEnv>,
    /// The shaping cache of the document's canvas text (shared by its origin's documents on this
    /// thread, `TextEngine::shared_text_cache`, apart from layout's): canvas text shaped before, as
    /// a whole run or word by word, is not shaped again.
    pub cache: Arc<Mutex<TextCache>>,
    /// `None`: the process's copy, loaded when a CPU canvas first draws text.
    pub cpu: Option<Arc<usvg::fontdb::Database>>,
}

/// The largest bitmap a canvas gets (pixels); bigger canvases draw nothing.
pub const MAX_CANVAS_PIXELS: u64 = 64 << 20;

/// The largest side of a GPU canvas (Direct3D 11's texture limit); longer canvases use the CPU.
const MAX_GPU_SIDE: u32 = 16384;

/// A colour stop list of a gradient (`CanvasGradient`).
#[derive(Clone, Debug, Default)]
pub struct Gradient {
    pub kind: GradientKind,
    /// (offset, colour) in the order added; sorted stably by offset when painting.
    pub stops: Vec<(f32, tiny_skia::Color)>,
}

#[derive(Clone, Copy, Debug, Default)]
pub enum GradientKind {
    #[default]
    None,
    Linear {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
    },
    Radial {
        x0: f32,
        y0: f32,
        r0: f32,
        x1: f32,
        y1: f32,
        r1: f32,
    },
    Conic {
        angle: f32,
        x: f32,
        y: f32,
    },
}

/// A pattern (`CanvasPattern`): an image and how it repeats.
#[derive(Clone)]
pub struct Pattern {
    pub image: Arc<Pixmap>,
    pub repeat_x: bool,
    pub repeat_y: bool,
}

/// A fill or stroke style.
#[derive(Clone)]
pub enum Style {
    Color(tiny_skia::Color, String),
    Gradient(Arc<Gradient>),
    Pattern(Arc<Pattern>),
}

/// The clipping region of a drawing state, in the form its canvas's backend uses.
pub struct Clip {
    /// The CPU backend's coverage mask.
    pub mask: Option<Mask>,
    /// The GPU backend's device-space paths.
    pub gpu: Option<Arc<CanvasClip>>,
}

/// The drawing state (HTML §4.12.5.1.2).
#[derive(Clone)]
pub struct State {
    pub transform: Transform,
    pub fill: Style,
    pub stroke: Style,
    pub line_width: f32,
    pub line_cap: LineCap,
    pub line_join: LineJoin,
    pub miter_limit: f32,
    pub dash: Vec<f32>,
    pub dash_offset: f32,
    pub global_alpha: f32,
    pub composite: BlendMode,
    pub composite_name: String,
    pub shadow_offset: (f32, f32),
    pub shadow_blur: f32,
    pub shadow_color: String,
    pub font: String,
    pub text_align: String,
    pub text_baseline: String,
    pub direction: String,
    pub image_smoothing: bool,
    pub filter: String,
    pub clip: Option<Arc<Clip>>,
}

impl Default for State {
    fn default() -> Self {
        let black = Style::Color(tiny_skia::Color::BLACK, "#000000".into());
        State {
            transform: Transform::identity(),
            fill: black.clone(),
            stroke: black,
            line_width: 1.0,
            line_cap: LineCap::Butt,
            line_join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: Vec::new(),
            dash_offset: 0.0,
            global_alpha: 1.0,
            composite: BlendMode::SourceOver,
            composite_name: "source-over".into(),
            shadow_offset: (0.0, 0.0),
            shadow_blur: 0.0,
            shadow_color: "rgba(0, 0, 0, 0)".into(),
            font: "10px sans-serif".into(),
            text_align: "start".into(),
            text_baseline: "alphabetic".into(),
            direction: "inherit".into(),
            image_smoothing: true,
            filter: "none".into(),
            clip: None,
        }
    }
}

/// CSS colour syntax for `fillStyle`, `strokeStyle`, `shadowColor`, `addColorStop`.
pub fn parse_color(text: &str) -> Option<tiny_skia::Color> {
    let t = text.trim();
    if let Some(c) = fast_color(t) {
        return Some(c);
    }
    if t.eq_ignore_ascii_case("transparent") {
        return Some(tiny_skia::Color::TRANSPARENT);
    }
    let c: svgtypes::Color = t.parse().ok()?;
    Some(tiny_skia::Color::from_rgba8(
        c.red, c.green, c.blue, c.alpha,
    ))
}

/// A plain decimal number (`12`, `0.5`, `.25`): exactly what `str::parse::<f64>` gives for it
/// (a mantissa of up to 15 digits divided by a power of ten is correctly rounded), without its
/// generality.
fn plain_number(p: &str) -> Option<f64> {
    let b = p.as_bytes();
    if b.is_empty() || b.len() > 16 {
        return None;
    }
    let mut mantissa = 0u64;
    let mut scale = None;
    let mut digits = 0;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'0'..=b'9' => {
                mantissa = mantissa * 10 + u64::from(c - b'0');
                digits += 1;
            }
            b'.' if scale.is_none() => scale = Some(b.len() - i - 1),
            _ => return None,
        }
    }
    if digits == 0 {
        return None;
    }
    const POW10: [f64; 16] = [
        1.0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
    ];
    Some(mantissa as f64 / POW10[scale.unwrap_or(0)])
}

/// The common colour forms scripts set thousands of times (`#rrggbb`, `#rgb`, `rgb(r, g, b)` and
/// `rgba(r, g, b, a)` with plain numbers), parsed as svgtypes parses them; `None` for anything else
/// (which then goes through svgtypes).
fn fast_color(t: &str) -> Option<tiny_skia::Color> {
    let b = t.as_bytes();
    if let Some(hex) = t.strip_prefix('#') {
        let h = |c: u8| (c as char).to_digit(16).map(|v| v as u8);
        let hb = hex.as_bytes();
        return match hb.len() {
            6 => Some(tiny_skia::Color::from_rgba8(
                h(hb[0])? * 16 + h(hb[1])?,
                h(hb[2])? * 16 + h(hb[3])?,
                h(hb[4])? * 16 + h(hb[5])?,
                255,
            )),
            3 => Some(tiny_skia::Color::from_rgba8(
                h(hb[0])? * 17,
                h(hb[1])? * 17,
                h(hb[2])? * 17,
                255,
            )),
            _ => None,
        };
    }
    let args = if b.starts_with(b"rgba(") {
        &t[5..]
    } else if b.starts_with(b"rgb(") {
        &t[4..]
    } else {
        return None;
    };
    let args = args.strip_suffix(')')?;
    let mut parts = [""; 4];
    let mut n = 0;
    for p in args.split(',') {
        if n == 4 {
            return None;
        }
        parts[n] = p.trim();
        n += 1;
    }
    if n < 3 {
        return None;
    }
    let (r, g, bl) = (
        plain_number(parts[0])?,
        plain_number(parts[1])?,
        plain_number(parts[2])?,
    );
    let a = if n == 4 {
        (plain_number(parts[3])? * 255.0).round() as u8
    } else {
        255
    };
    Some(tiny_skia::Color::from_rgba8(
        r.round() as u8,
        g.round() as u8,
        bl.round() as u8,
        a,
    ))
}

/// HTML "serialization of a color" for the canvas: `#rrggbb` when opaque, `rgba(...)` otherwise.
pub fn serialize_color(c: tiny_skia::Color) -> String {
    let c8 = c.to_color_u8();
    if c8.alpha() == 255 {
        format!("#{:02x}{:02x}{:02x}", c8.red(), c8.green(), c8.blue())
    } else {
        let a = c.alpha();
        let a = if a == 0.0 {
            "0".to_string()
        } else {
            let s = format!("{a:.3}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        };
        format!("rgba({}, {}, {}, {a})", c8.red(), c8.green(), c8.blue())
    }
}

/// `globalCompositeOperation` keywords.
pub fn blend_mode(name: &str) -> Option<BlendMode> {
    Some(match name {
        "source-over" => BlendMode::SourceOver,
        "source-in" => BlendMode::SourceIn,
        "source-out" => BlendMode::SourceOut,
        "source-atop" => BlendMode::SourceAtop,
        "destination-over" => BlendMode::DestinationOver,
        "destination-in" => BlendMode::DestinationIn,
        "destination-out" => BlendMode::DestinationOut,
        "destination-atop" => BlendMode::DestinationAtop,
        "lighter" => BlendMode::Plus,
        "copy" => BlendMode::Source,
        "xor" => BlendMode::Xor,
        "multiply" => BlendMode::Multiply,
        "screen" => BlendMode::Screen,
        "overlay" => BlendMode::Overlay,
        "darken" => BlendMode::Darken,
        "lighten" => BlendMode::Lighten,
        "color-dodge" => BlendMode::ColorDodge,
        "color-burn" => BlendMode::ColorBurn,
        "hard-light" => BlendMode::HardLight,
        "soft-light" => BlendMode::SoftLight,
        "difference" => BlendMode::Difference,
        "exclusion" => BlendMode::Exclusion,
        "hue" => BlendMode::Hue,
        "saturation" => BlendMode::Saturation,
        "color" => BlendMode::Color,
        "luminosity" => BlendMode::Luminosity,
        _ => return None,
    })
}

/// The GPU backend's operator for a [`blend_mode`].
fn composite_op(mode: BlendMode) -> CompositeOp {
    match mode {
        BlendMode::SourceIn => CompositeOp::SourceIn,
        BlendMode::SourceOut => CompositeOp::SourceOut,
        BlendMode::SourceAtop => CompositeOp::SourceAtop,
        BlendMode::DestinationOver => CompositeOp::DestinationOver,
        BlendMode::DestinationIn => CompositeOp::DestinationIn,
        BlendMode::DestinationOut => CompositeOp::DestinationOut,
        BlendMode::DestinationAtop => CompositeOp::DestinationAtop,
        BlendMode::Plus => CompositeOp::Lighter,
        BlendMode::Source => CompositeOp::Copy,
        BlendMode::Xor => CompositeOp::Xor,
        BlendMode::Multiply => CompositeOp::Multiply,
        BlendMode::Screen => CompositeOp::Screen,
        BlendMode::Overlay => CompositeOp::Overlay,
        BlendMode::Darken => CompositeOp::Darken,
        BlendMode::Lighten => CompositeOp::Lighten,
        BlendMode::ColorDodge => CompositeOp::ColorDodge,
        BlendMode::ColorBurn => CompositeOp::ColorBurn,
        BlendMode::HardLight => CompositeOp::HardLight,
        BlendMode::SoftLight => CompositeOp::SoftLight,
        BlendMode::Difference => CompositeOp::Difference,
        BlendMode::Exclusion => CompositeOp::Exclusion,
        BlendMode::Hue => CompositeOp::Hue,
        BlendMode::Saturation => CompositeOp::Saturation,
        BlendMode::Color => CompositeOp::Color,
        BlendMode::Luminosity => CompositeOp::Luminosity,
        _ => CompositeOp::SourceOver,
    }
}

/// What a path is, in device space, when it is simple enough for a primitive.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum PathShape {
    /// No segments yet (at most a current point).
    #[default]
    Empty,
    /// One full circle (from `arc`/`ellipse` under a similarity transform), maybe closed.
    Circle {
        x: f32,
        y: f32,
        r: f32,
        closed: bool,
    },
    /// One device-aligned rectangle (from `rect` under a scale and translation).
    Rect {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
    },
    Other,
}

/// A path being built (`Path2D` or the current default path): its points in the coordinates they
/// were given in, transformed by the matrix given with each operation.
#[derive(Clone, Default)]
pub struct CanvasPath {
    builder: PathBuilder,
    /// The last point (transformed), for operations that start from it.
    last: Option<Point>,
    /// The subpath's first point.
    start: Option<Point>,
    shape: PathShape,
    /// A full circle not yet added to `builder` as Béziers (a primitive draws it without them).
    pending: Option<PendingArc>,
}

/// The arguments of an `ellipse` whose segments are added when something needs them.
#[derive(Clone, Copy)]
struct PendingArc {
    t: Transform,
    x: f32,
    y: f32,
    rx: f32,
    ry: f32,
    rotation: f32,
    start: f32,
    sweep: f32,
    /// A current point existed (the arc starts with a line to its start).
    had_point: bool,
    closed: bool,
}

impl PendingArc {
    fn emit(&self, b: &mut PathBuilder) -> Point {
        let last = emit_ellipse(
            b,
            &self.t,
            self.had_point,
            (self.x, self.y),
            (self.rx, self.ry),
            self.rotation,
            self.start,
            self.sweep,
        );
        if self.closed {
            b.close();
        }
        last
    }
}

/// Adds an elliptical arc to `b` as cubic Béziers (each up to a quarter turn), mapped through `t`,
/// starting with a line to its start when there is a current point; returns its end.
#[allow(clippy::too_many_arguments)]
fn emit_ellipse(
    b: &mut PathBuilder,
    t: &Transform,
    had_point: bool,
    (x, y): (f32, f32),
    (rx, ry): (f32, f32),
    rotation: f32,
    start: f32,
    sweep: f32,
) -> Point {
    let tau = std::f32::consts::TAU;
    let (sr, cr) = rotation.sin_cos();
    let d = |px: f32, py: f32| {
        let mut p = Point::from_xy(x + px * cr - py * sr, y + px * sr + py * cr);
        t.map_point(&mut p);
        p
    };
    let (s0, c0) = start.sin_cos();
    let first = d(rx * c0, ry * s0);
    if had_point {
        b.line_to(first.x, first.y);
    } else {
        b.move_to(first.x, first.y);
    }
    let segments = ((sweep.abs() / (tau / 4.0)).ceil() as usize).max(1);
    let step = sweep / segments as f32;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    let mut a = start;
    let mut last = first;
    for _ in 0..segments {
        let e = a + step;
        let (s1, c1) = a.sin_cos();
        let (s2, c2) = e.sin_cos();
        let p1 = d(rx * (c1 - k * s1), ry * (s1 + k * c1));
        let p2 = d(rx * (c2 + k * s2), ry * (s2 - k * c2));
        last = d(rx * c2, ry * s2);
        b.cubic_to(p1.x, p1.y, p2.x, p2.y, last.x, last.y);
        a = e;
    }
    last
}

fn finite(v: &[f32]) -> bool {
    v.iter().all(|v| v.is_finite())
}

/// [`snowlark_paint::Matrix::antialiases_image`] for the CPU surface's transform.
fn image_antialiased(t: &Transform, width: f32, height: f32) -> bool {
    snowlark_paint::Matrix {
        m11: t.sx,
        m12: t.ky,
        m21: t.kx,
        m22: t.sy,
        dx: t.tx,
        dy: t.ty,
    }
    .antialiases_image(width, height)
}

/// The uniform scale of a similarity transform (rotation, uniform scale, translation), if it is
/// one.
fn similarity_scale(t: &Transform) -> Option<f32> {
    let (a, b, c, d) = (t.sx, t.ky, t.kx, t.sy);
    let n1 = a * a + b * b;
    let n2 = c * c + d * d;
    let dot = a * c + b * d;
    let tolerance = 1e-5 * n1.max(n2).max(1e-12);
    ((n1 - n2).abs() <= tolerance && dot.abs() <= tolerance && n1 > 0.0).then(|| n1.sqrt())
}

impl CanvasPath {
    pub fn new() -> CanvasPath {
        CanvasPath::default()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_none() && self.builder.is_empty()
    }

    /// Adds a pending circle's segments to the builder.
    fn materialize(&mut self) {
        if let Some(p) = self.pending.take() {
            p.emit(&mut self.builder);
        }
    }

    /// What the path is in device space.
    pub fn shape(&self) -> PathShape {
        self.shape
    }

    fn map(t: &Transform, x: f32, y: f32) -> Point {
        let mut p = Point::from_xy(x, y);
        t.map_point(&mut p);
        p
    }

    /// "Ensure there is a subpath" for (x, y).
    fn ensure(&mut self, t: &Transform, x: f32, y: f32) {
        if self.last.is_none() {
            self.move_to(t, x, y);
        }
    }

    pub fn move_to(&mut self, t: &Transform, x: f32, y: f32) {
        if !finite(&[x, y]) {
            return;
        }
        self.materialize();
        let p = Self::map(t, x, y);
        self.builder.move_to(p.x, p.y);
        self.last = Some(p);
        self.start = Some(p);
    }

    pub fn line_to(&mut self, t: &Transform, x: f32, y: f32) {
        if !finite(&[x, y]) {
            return;
        }
        self.materialize();
        if self.last.is_none() {
            self.move_to(t, x, y);
            return;
        }
        let p = Self::map(t, x, y);
        self.builder.line_to(p.x, p.y);
        self.last = Some(p);
        self.shape = PathShape::Other;
    }

    pub fn quad_to(&mut self, t: &Transform, cx: f32, cy: f32, x: f32, y: f32) {
        if !finite(&[cx, cy, x, y]) {
            return;
        }
        self.materialize();
        self.ensure(t, cx, cy);
        let c = Self::map(t, cx, cy);
        let p = Self::map(t, x, y);
        self.builder.quad_to(c.x, c.y, p.x, p.y);
        self.last = Some(p);
        self.shape = PathShape::Other;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cubic_to(
        &mut self,
        t: &Transform,
        c1x: f32,
        c1y: f32,
        c2x: f32,
        c2y: f32,
        x: f32,
        y: f32,
    ) {
        if !finite(&[c1x, c1y, c2x, c2y, x, y]) {
            return;
        }
        self.materialize();
        self.ensure(t, c1x, c1y);
        let a = Self::map(t, c1x, c1y);
        let b = Self::map(t, c2x, c2y);
        let p = Self::map(t, x, y);
        self.builder.cubic_to(a.x, a.y, b.x, b.y, p.x, p.y);
        self.last = Some(p);
        self.shape = PathShape::Other;
    }

    pub fn close(&mut self) {
        if let Some(p) = &mut self.pending {
            p.closed = true;
            if let PathShape::Circle { closed, .. } = &mut self.shape {
                *closed = true;
            }
            self.last = self.start;
            return;
        }
        if self.last.is_some() {
            self.builder.close();
            self.last = self.start;
            match &mut self.shape {
                PathShape::Circle { closed, .. } => *closed = true,
                PathShape::Empty | PathShape::Rect { .. } => {}
                PathShape::Other => {}
            }
        }
    }

    pub fn rect(&mut self, t: &Transform, x: f32, y: f32, w: f32, h: f32) {
        if !finite(&[x, y, w, h]) {
            return;
        }
        let prior = self.shape;
        self.move_to(t, x, y);
        self.line_to(t, x + w, y);
        self.line_to(t, x + w, y + h);
        self.line_to(t, x, y + h);
        self.close();
        self.move_to(t, x, y);
        self.shape = if prior == PathShape::Empty && t.kx == 0.0 && t.ky == 0.0 {
            let a = Self::map(t, x, y);
            let b = Self::map(t, x + w, y + h);
            PathShape::Rect {
                x0: a.x.min(b.x),
                y0: a.y.min(b.y),
                x1: a.x.max(b.x),
                y1: a.y.max(b.y),
            }
        } else {
            PathShape::Other
        };
    }

    /// `ellipse()`/`arc()`: the arc in user space as cubic Béziers (each up to a quarter turn). One
    /// full circle on an otherwise empty path (a current point exactly at its start draws no line)
    /// is a circle primitive: its segments are only added when something needs them.
    #[allow(clippy::too_many_arguments)]
    pub fn ellipse(
        &mut self,
        t: &Transform,
        x: f32,
        y: f32,
        rx: f32,
        ry: f32,
        rotation: f32,
        start: f32,
        end: f32,
        ccw: bool,
    ) {
        if !finite(&[x, y, rx, ry, rotation, start, end]) {
            return;
        }
        self.materialize();
        let prior = self.shape;
        let prior_point = self.last;
        let tau = std::f32::consts::TAU;
        let mut sweep = end - start;
        if !ccw {
            if sweep >= tau {
                sweep = tau;
            } else {
                sweep = sweep.rem_euclid(tau);
                if sweep == 0.0 && end != start {
                    sweep = tau;
                }
            }
        } else if -sweep >= tau {
            sweep = -tau;
        } else {
            sweep = -((-sweep).rem_euclid(tau));
            if sweep == 0.0 && end != start {
                sweep = -tau;
            }
        }
        let (sr, cr) = rotation.sin_cos();
        let (s0, c0) = start.sin_cos();
        let (px, py) = (rx * c0, ry * s0);
        let first = Self::map(t, x + px * cr - py * sr, y + px * sr + py * cr);
        let had_point = prior_point.is_some();
        let joins = prior_point
            .is_none_or(|p| (p.x - first.x).abs() < 1e-3 && (p.y - first.y).abs() < 1e-3);
        let circle = similarity_scale(t).filter(|_| {
            prior == PathShape::Empty && joins && sweep.abs() >= tau && rx == ry && rx > 0.0
        });
        if !had_point {
            self.start = Some(first);
        }
        if let Some(scale) = circle {
            self.pending = Some(PendingArc {
                t: *t,
                x,
                y,
                rx,
                ry,
                rotation,
                start,
                sweep,
                had_point,
                closed: false,
            });
            // (a full circle ends where it starts)
            self.last = Some(first);
            let c = Self::map(t, x, y);
            self.shape = PathShape::Circle {
                x: c.x,
                y: c.y,
                r: rx * scale,
                closed: false,
            };
            return;
        }
        let last = emit_ellipse(
            &mut self.builder,
            t,
            had_point,
            (x, y),
            (rx, ry),
            rotation,
            start,
            sweep,
        );
        self.last = Some(last);
        self.shape = PathShape::Other;
    }

    /// `arcTo(x1, y1, x2, y2, radius)` (HTML §4.12.5.1.10).
    #[allow(clippy::too_many_arguments)]
    pub fn arc_to(&mut self, t: &Transform, x1: f32, y1: f32, x2: f32, y2: f32, r: f32) -> bool {
        if !finite(&[x1, y1, x2, y2, r]) {
            return true;
        }
        if r < 0.0 {
            return false;
        }
        self.materialize();
        self.ensure(t, x1, y1);
        // The last point in user space.
        let Some(inv) = t.invert() else {
            return true;
        };
        let mut p0 = self.last.unwrap_or(Point::zero());
        inv.map_point(&mut p0);
        let (x0, y0) = (p0.x, p0.y);
        let (dx0, dy0) = (x0 - x1, y0 - y1);
        let (dx2, dy2) = (x2 - x1, y2 - y1);
        let cross = dx0 * dy2 - dy0 * dx2;
        if (x0 == x1 && y0 == y1) || (x1 == x2 && y1 == y2) || r == 0.0 || cross.abs() < 1e-6 {
            self.line_to(t, x1, y1);
            return true;
        }
        let l0 = (dx0 * dx0 + dy0 * dy0).sqrt();
        let l2 = (dx2 * dx2 + dy2 * dy2).sqrt();
        let cos = ((dx0 * dx2 + dy0 * dy2) / (l0 * l2)).clamp(-1.0, 1.0);
        let angle = cos.acos();
        let dist = r / (angle / 2.0).tan();
        let (t0x, t0y) = (x1 + dx0 / l0 * dist, y1 + dy0 / l0 * dist);
        let (t2x, t2y) = (x1 + dx2 / l2 * dist, y1 + dy2 / l2 * dist);
        // The centre is on the bisector, `r` from both tangents.
        let (bx, by) = (dx0 / l0 + dx2 / l2, dy0 / l0 + dy2 / l2);
        let bl = (bx * bx + by * by).sqrt();
        let cd = (r * r + dist * dist).sqrt();
        let (cx, cy) = (x1 + bx / bl * cd, y1 + by / bl * cd);
        let a0 = (t0y - cy).atan2(t0x - cx);
        let a2 = (t2y - cy).atan2(t2x - cx);
        self.line_to(t, t0x, t0y);
        self.ellipse(t, cx, cy, r, r, 0.0, a0, a2, cross > 0.0);
        self.shape = PathShape::Other;
        true
    }

    /// Appends another path's segments, transformed.
    pub fn add_path(&mut self, other: &CanvasPath, transform: Transform) {
        self.materialize();
        if let Some(p) = other.finish() {
            if let Some(p) = p.transform(transform) {
                self.builder.push_path(&p);
                self.last = p.points().last().copied();
                self.shape = PathShape::Other;
            }
        }
    }

    pub fn finish(&self) -> Option<tiny_skia::Path> {
        let mut b = self.builder.clone();
        if let Some(p) = &self.pending {
            p.emit(&mut b);
        }
        b.finish()
    }
}

/// Which bitmap a canvas draws into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// tiny-skia on the CPU.
    Skia,
    /// Direct2D on the page thread's device.
    Gpu(Driver),
}

impl Backend {
    /// The backend `SNOWLARK_CANVAS` selects (`skia`, `warp`; the graphics card otherwise).
    pub fn from_env() -> Backend {
        match std::env::var("SNOWLARK_CANVAS").as_deref() {
            Ok("skia") | Ok("cpu") => Backend::Skia,
            Ok("warp") => Backend::Gpu(Driver::Warp),
            _ => Backend::Gpu(Driver::Hardware),
        }
    }

    /// Starts this thread's canvas thread for the backend, which makes the device, before its first
    /// canvas needs it ([`snowlark_paint::canvas_thread::prepare`]); nothing for the CPU.
    pub fn prepare(self) {
        if let Backend::Gpu(driver) = self {
            snowlark_paint::canvas_thread::prepare(driver);
        }
    }
}

enum Surface {
    None,
    Skia(Pixmap),
    /// A GPU bitmap not made yet, still transparent black: the first operation that needs its
    /// pixels makes it ([`Canvas2D::make_surface`]). Until then the canvas measures text and keeps
    /// its state as a GPU canvas does, and making the graphics device (a wait of about a fifth of
    /// a second for a thread's first canvas) is left to the first drawing: a canvas that only
    /// measures text never makes one.
    Unmade(Driver),
    Gpu(Box<GpuCanvas>),
}

/// A canvas's bitmap and its 2D context.
pub struct Canvas2D {
    surface: Surface,
    backend: Backend,
    width: u32,
    height: u32,
    pub state: State,
    stack: Vec<State>,
    pub path: CanvasPath,
    /// Bumped whenever the bitmap changes.
    pub generation: u64,
    /// GPU images of the patterns drawn lately (by the pattern's pixmap).
    pattern_images: Vec<(Arc<Pixmap>, CanvasImage)>,
    /// The last `font` value resolved for GPU text.
    font: Option<ResolvedFont>,
}

/// A canvas `font` value resolved against a document's fonts.
struct ResolvedFont {
    value: String,
    /// The text engine and font environment it was resolved with (by address: a web font arriving
    /// makes a new environment).
    engine: usize,
    env: usize,
    list: FontListId,
    size: f32,
    small_caps: bool,
    ascent: f32,
    descent: f32,
}

fn to_matrix(t: &Transform) -> snowlark_paint::Matrix {
    snowlark_paint::Matrix::new(t.sx, t.ky, t.kx, t.sy, t.tx, t.ty)
}

fn rgba(c: tiny_skia::Color) -> [f32; 4] {
    [c.red(), c.green(), c.blue(), c.alpha()]
}

fn ppoint(p: Point) -> snowlark_paint::Point {
    snowlark_paint::Point::new(p.x, p.y)
}

/// A tiny-skia path as the GPU backend's path data.
fn path_data(path: &tiny_skia::Path) -> PathData {
    let mut out = PathData {
        verbs: Vec::with_capacity(path.verbs().len()),
        points: Vec::with_capacity(path.points().len()),
    };
    for s in path.segments() {
        match s {
            PathSegment::MoveTo(p) => {
                out.verbs.push(PathVerb::Move);
                out.points.push(ppoint(p));
            }
            PathSegment::LineTo(p) => {
                out.verbs.push(PathVerb::Line);
                out.points.push(ppoint(p));
            }
            PathSegment::QuadTo(a, b) => {
                out.verbs.push(PathVerb::Quad);
                out.points.extend([ppoint(a), ppoint(b)]);
            }
            PathSegment::CubicTo(a, b, c) => {
                out.verbs.push(PathVerb::Cubic);
                out.points.extend([ppoint(a), ppoint(b), ppoint(c)]);
            }
            PathSegment::Close => out.verbs.push(PathVerb::Close),
        }
    }
    out
}

/// A path for stroking (HTML §4.12.5.1.7): zero-length segments removed, and subpaths left with no
/// segments dropped.
fn prune_for_stroke(path: &PathData) -> PathData {
    let mut out = PathData::default();
    let mut i = 0;
    let mut current = snowlark_paint::Point::default();
    let mut start = current;
    // The pending move of the subpath, until a segment of it is kept.
    let mut pending: Option<snowlark_paint::Point> = None;
    let mut kept = false;
    for v in &path.verbs {
        match v {
            PathVerb::Move => {
                current = path.points[i];
                start = current;
                pending = Some(current);
                kept = false;
                i += 1;
            }
            PathVerb::Close => {
                if kept {
                    out.verbs.push(PathVerb::Close);
                }
                current = start;
                pending = Some(start);
                kept = false;
            }
            _ => {
                let n = match v {
                    PathVerb::Line => 1,
                    PathVerb::Quad => 2,
                    _ => 3,
                };
                let pts = &path.points[i..i + n];
                i += n;
                if pts.iter().all(|p| *p == current) {
                    continue;
                }
                if let Some(m) = pending.take() {
                    out.verbs.push(PathVerb::Move);
                    out.points.push(m);
                }
                out.verbs.push(*v);
                out.points.extend_from_slice(pts);
                current = pts[n - 1];
                kept = true;
            }
        }
    }
    out
}

/// Whether a path with curves is stroked with a line wider than twice its smaller extent.
fn wide_on_curves(path: &PathData, width: f32) -> bool {
    if !path
        .verbs
        .iter()
        .any(|v| matches!(v, PathVerb::Quad | PathVerb::Cubic))
    {
        return false;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in &path.points {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    width / 2.0 > (x1 - x0).min(y1 - y0)
}

/// The device rectangle of the user-space rectangle (x, y, w, h) when the transform keeps it
/// axis-aligned.
fn device_rect(t: &Transform, x: f32, y: f32, w: f32, h: f32) -> Option<snowlark_paint::Rect> {
    if t.kx != 0.0 || t.ky != 0.0 {
        return None;
    }
    let mut a = Point::from_xy(x, y);
    let mut b = Point::from_xy(x + w, y + h);
    t.map_point(&mut a);
    t.map_point(&mut b);
    Some(snowlark_paint::Rect::from_ltrb(
        a.x.min(b.x),
        a.y.min(b.y),
        a.x.max(b.x),
        a.y.max(b.y),
    ))
}

/// tiny-skia's premultiplication of one channel.
fn premultiply(c: u8, a: u8) -> u8 {
    ((c as u32 * a as u32 + 127) / 255) as u8
}

/// tiny-skia's demultiplication of one channel.
fn demultiply(c: u8, a: u8) -> u8 {
    if a == 0 {
        0
    } else {
        ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
    }
}

/// Premultiplied RGBA (a tiny-skia pixmap's data) as premultiplied BGRA.
fn rgba_to_bgra(data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    out
}

/// tiny-skia's paint for a style with an alpha and blend mode, its user space mapped by `t`.
fn skia_paint(style: &Style, alpha: f32, blend: BlendMode, t: Transform) -> Option<Paint<'_>> {
    let mut paint = Paint {
        anti_alias: true,
        blend_mode: blend,
        ..Paint::default()
    };
    match style {
        Style::Color(c, _) => {
            let mut c = *c;
            c.apply_opacity(alpha);
            paint.set_color(c);
        }
        Style::Gradient(g) => {
            let mut stops: Vec<(f32, tiny_skia::Color)> = g.stops.clone();
            stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            if stops.is_empty() {
                let mut c = tiny_skia::Color::TRANSPARENT;
                c.apply_opacity(alpha);
                paint.set_color(c);
                return Some(paint);
            }
            let first = stops[0].1;
            let stops: Vec<SkiaStop> = stops
                .into_iter()
                .map(|(o, mut c)| {
                    c.apply_opacity(alpha);
                    SkiaStop::new(o, c)
                })
                .collect();
            let shader = match g.kind {
                GradientKind::Linear { x0, y0, x1, y1 } => tiny_skia::LinearGradient::new(
                    Point::from_xy(x0, y0),
                    Point::from_xy(x1, y1),
                    stops.clone(),
                    SpreadMode::Pad,
                    t,
                ),
                GradientKind::Radial {
                    x0,
                    y0,
                    r0,
                    x1,
                    y1,
                    r1,
                } => tiny_skia::RadialGradient::new(
                    Point::from_xy(x0, y0),
                    r0,
                    Point::from_xy(x1, y1),
                    r1,
                    stops.clone(),
                    SpreadMode::Pad,
                    t,
                ),
                // (conic gradients: the first stop's colour)
                GradientKind::Conic { .. } | GradientKind::None => None,
            };
            match shader {
                Some(s) => paint.shader = s,
                None => {
                    let mut c = first;
                    c.apply_opacity(alpha);
                    paint.set_color(c);
                }
            }
        }
        Style::Pattern(p) => {
            paint.shader = tiny_skia::Pattern::new(
                p.image.as_ref().as_ref(),
                if p.repeat_x || p.repeat_y {
                    SpreadMode::Repeat
                } else {
                    SpreadMode::Pad
                },
                FilterQuality::Bilinear,
                alpha,
                t,
            );
        }
    }
    Some(paint)
}

/// The shadow a state draws, if any.
fn shadow(s: &State) -> Option<Shadow> {
    if s.shadow_blur <= 0.0 && s.shadow_offset == (0.0, 0.0) {
        return None;
    }
    let color = parse_color(&s.shadow_color)?;
    (color.alpha() > 0.0).then(|| Shadow {
        offset: s.shadow_offset,
        blur: s.shadow_blur,
        color: rgba(color),
    })
}

/// The GPU backend's parameters of a drawing state.
fn draw_params(s: &State) -> DrawParams<'_> {
    DrawParams {
        transform: to_matrix(&s.transform),
        alpha: s.global_alpha,
        op: composite_op(s.composite),
        clip: s.clip.as_ref().and_then(|c| c.gpu.as_ref()),
        shadow: shadow(s),
        smoothing: s.image_smoothing,
    }
}

/// Whether a gradient is rasterized on the CPU for the GPU backend: radial gradients, and
/// gradients with several stops at one offset.
fn gradient_on_cpu(g: &Gradient) -> bool {
    match g.kind {
        GradientKind::Radial { .. } => true,
        GradientKind::Linear { .. } => {
            let mut offsets: Vec<f32> = g.stops.iter().map(|s| s.0).collect();
            offsets.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            offsets.windows(2).any(|w| w[0] == w[1])
        }
        _ => false,
    }
}

/// Runs `f` with the GPU paint for a style (`None`: not a GPU canvas, or nothing to paint).
fn gpu_paint<R>(
    surface: &mut Surface,
    patterns: &mut Vec<(Arc<Pixmap>, CanvasImage)>,
    style: &Style,
    transform: Transform,
    f: impl FnOnce(&mut GpuCanvas, &CanvasPaint) -> R,
) -> Option<R> {
    let Surface::Gpu(g) = surface else {
        return None;
    };
    match style {
        Style::Color(c, _) => Some(f(g, &CanvasPaint::Solid(rgba(*c)))),
        // Direct2D's radial gradients start from a point, not a circle, and paint outside the cone,
        // and its gradient textures blur colour stops at one offset: those are rasterized here.
        Style::Gradient(gr) if gradient_on_cpu(gr) => {
            let (w, h) = g.size();
            let mut pm = Pixmap::new(w, h)?;
            let paint = skia_paint(style, 1.0, BlendMode::Source, transform)?;
            pm.fill_rect(
                tiny_skia::Rect::from_xywh(0.0, 0.0, w as f32, h as f32)?,
                &paint,
                Transform::identity(),
                None,
            );
            let image = g.image_from_bgra(w, h, &rgba_to_bgra(pm.data())).ok()?;
            Some(f(g, &CanvasPaint::DeviceImage(&image)))
        }
        Style::Gradient(gr) => {
            let mut stops: Vec<GradientStop> = gr
                .stops
                .iter()
                .map(|(o, c)| GradientStop {
                    offset: *o,
                    color: rgba(*c),
                })
                .collect();
            stops.sort_by(|a, b| {
                a.offset
                    .partial_cmp(&b.offset)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            if stops.is_empty() {
                return Some(f(g, &CanvasPaint::Solid([0.0; 4])));
            }
            let pt = |x, y| snowlark_paint::Point::new(x, y);
            let paint = match gr.kind {
                GradientKind::Linear { x0, y0, x1, y1 } => CanvasPaint::Linear {
                    start: pt(x0, y0),
                    end: pt(x1, y1),
                    stops: &stops,
                },
                GradientKind::Radial {
                    x0,
                    y0,
                    r0,
                    x1,
                    y1,
                    r1,
                } => CanvasPaint::Radial {
                    c0: pt(x0, y0),
                    r0,
                    c1: pt(x1, y1),
                    r1,
                    stops: &stops,
                },
                // (conic gradients: the first stop's colour)
                GradientKind::Conic { .. } | GradientKind::None => {
                    CanvasPaint::Solid(stops[0].color)
                }
            };
            Some(f(g, &paint))
        }
        Style::Pattern(p) => {
            let i = match patterns
                .iter()
                .position(|(pm, _)| Arc::ptr_eq(pm, &p.image))
            {
                Some(i) => i,
                None => {
                    let image = g
                        .image_from_bgra(
                            p.image.width(),
                            p.image.height(),
                            &rgba_to_bgra(p.image.data()),
                        )
                        .ok()?;
                    if patterns.len() >= 8 {
                        patterns.remove(0);
                    }
                    patterns.push((p.image.clone(), image));
                    patterns.len() - 1
                }
            };
            let image = &patterns[i].1;
            Some(f(
                g,
                &CanvasPaint::Pattern {
                    image,
                    repeat_x: p.repeat_x,
                    repeat_y: p.repeat_y,
                },
            ))
        }
    }
}

impl Canvas2D {
    /// A transparent black bitmap of `width`×`height` on the backend `SNOWLARK_CANVAS` selects (none,
    /// drawing nothing, past the limit).
    pub fn new(width: u32, height: u32) -> Canvas2D {
        Canvas2D::with_backend(width, height, Backend::from_env())
    }

    /// A transparent black bitmap on `backend` (the CPU one when the GPU bitmap cannot be made).
    pub fn with_backend(width: u32, height: u32, backend: Backend) -> Canvas2D {
        let fits = (width as u64) * (height as u64) <= MAX_CANVAS_PIXELS;
        let mut surface = Surface::None;
        let mut used = backend;
        if fits {
            if let Backend::Gpu(driver) = backend {
                if width.max(height) <= MAX_GPU_SIDE {
                    surface = Surface::Unmade(driver);
                }
            }
            if matches!(surface, Surface::None) {
                used = Backend::Skia;
                if let Some(pm) = Pixmap::new(width.max(1), height.max(1)) {
                    surface = Surface::Skia(pm);
                }
            }
        }
        Canvas2D {
            surface,
            backend: used,
            width,
            height,
            state: State::default(),
            stack: Vec::new(),
            path: CanvasPath::new(),
            generation: 1,
            pattern_images: Vec::new(),
            font: None,
        }
    }

    /// The backend this canvas draws with.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Whether the canvas draws on the GPU, made or not: the state and text measurement follow it.
    fn on_gpu(&self) -> bool {
        matches!(self.surface, Surface::Gpu(_) | Surface::Unmade(_))
    }

    /// Makes an unmade GPU bitmap, before an operation that needs its pixels. A device that cannot
    /// be made leaves the CPU bitmap, as for a canvas made without one (clip regions set before
    /// then were kept in the GPU form only, and do not clip the CPU bitmap).
    fn make_surface(&mut self) {
        let Surface::Unmade(driver) = self.surface else {
            return;
        };
        let (w, h) = (self.width.max(1), self.height.max(1));
        self.surface = match GpuCanvas::new(w, h, driver) {
            Ok(g) => Surface::Gpu(Box::new(g)),
            Err(_) => {
                self.backend = Backend::Skia;
                Pixmap::new(w, h).map_or(Surface::None, Surface::Skia)
            }
        };
    }

    /// HTML: setting `width` or `height` resets the bitmap and the context state.
    pub fn resize(&mut self, width: u32, height: u32) {
        let backend = match self.backend {
            Backend::Skia if matches!(Backend::from_env(), Backend::Gpu(_)) => Backend::from_env(),
            b => b,
        };
        *self = Canvas2D {
            generation: self.generation + 1,
            ..Canvas2D::with_backend(width, height, backend)
        };
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn save(&mut self) {
        if self.stack.len() < 1024 {
            self.stack.push(self.state.clone());
        }
    }

    pub fn restore(&mut self) {
        if let Some(s) = self.stack.pop() {
            self.state = s;
        }
    }

    /// `reset()`: the bitmap cleared, the state and the path to their initial values.
    pub fn reset(&mut self) {
        let (w, h) = (self.width, self.height);
        self.resize(w, h);
    }

    fn touched(&mut self) {
        self.generation += 1;
    }

    // ----- the CPU backend -----

    fn paint_for<'a>(&self, style: &'a Style) -> Option<Paint<'a>> {
        skia_paint(
            style,
            self.state.global_alpha,
            self.state.composite,
            self.state.transform,
        )
    }

    fn skia_mask(&self) -> Option<&Mask> {
        self.state.clip.as_ref().and_then(|c| c.mask.as_ref())
    }

    fn skia_fill(&mut self, path: &tiny_skia::Path, rule: FillRule, style: &Style) {
        let Some(paint) = self.paint_for(style) else {
            return;
        };
        let clip = self.state.clip.clone();
        if let Surface::Skia(pm) = &mut self.surface {
            pm.fill_path(
                path,
                &paint,
                rule,
                Transform::identity(),
                clip.as_ref().and_then(|c| c.mask.as_ref()),
            );
        }
    }

    /// The CPU backend's stroke: the line styles in device pixels (scaled uniformly).
    fn skia_stroke_style(&self) -> Stroke {
        let t = self.state.transform;
        let scale = ((t.sx * t.sy - t.kx * t.ky).abs()).sqrt().max(1e-6);
        let mut stroke = Stroke {
            width: self.state.line_width * scale,
            miter_limit: self.state.miter_limit,
            line_cap: self.state.line_cap,
            line_join: self.state.line_join,
            dash: None,
        };
        if !self.state.dash.is_empty() {
            let dash: Vec<f32> = self.state.dash.iter().map(|d| d * scale).collect();
            stroke.dash = StrokeDash::new(dash, self.state.dash_offset * scale);
        }
        stroke
    }

    /// A canvas-sized image drawn by `draw` with tiny-skia, uploaded to the GPU canvas.
    fn cpu_image(&self, draw: impl FnOnce(&mut Pixmap, &Canvas2D)) -> Option<CanvasImage> {
        let Surface::Gpu(g) = &self.surface else {
            return None;
        };
        let (w, h) = g.size();
        let mut pm = Pixmap::new(w, h)?;
        draw(&mut pm, self);
        g.image_from_bgra(w, h, &rgba_to_bgra(pm.data())).ok()
    }

    fn skia_stroke(&mut self, path: &tiny_skia::Path) {
        let style = self.state.stroke.clone();
        let Some(paint) = self.paint_for(&style) else {
            return;
        };
        let stroke = self.skia_stroke_style();
        let clip = self.state.clip.clone();
        if let Surface::Skia(pm) = &mut self.surface {
            pm.stroke_path(
                path,
                &paint,
                &stroke,
                Transform::identity(),
                clip.as_ref().and_then(|c| c.mask.as_ref()),
            );
        }
    }

    // ----- the GPU backend -----

    fn gpu_stroke_params(&self) -> (f32, GpuCap, GpuJoin) {
        let cap = match self.state.line_cap {
            LineCap::Butt => GpuCap::Butt,
            LineCap::Round => GpuCap::Round,
            LineCap::Square => GpuCap::Square,
        };
        let join = match self.state.line_join {
            LineJoin::Miter | LineJoin::MiterClip => GpuJoin::Miter,
            LineJoin::Round => GpuJoin::Round,
            LineJoin::Bevel => GpuJoin::Bevel,
        };
        (self.state.line_width, cap, join)
    }

    fn gpu_fill(&mut self, shape: FillShape) {
        let p = draw_params(&self.state);
        gpu_paint(
            &mut self.surface,
            &mut self.pattern_images,
            &self.state.fill,
            self.state.transform,
            |g, paint| {
                let _ = g.fill(shape, paint, &p);
            },
        );
    }

    /// A full circle stroked in a solid colour without dashes, under a transform that keeps it a
    /// circle: its device centre, radius, line width and colour.
    fn circle_stroke(&self, shape: PathShape) -> Option<(f32, f32, f32, f32, [f32; 4])> {
        let PathShape::Circle { x, y, r, closed } = shape else {
            return None;
        };
        let Style::Color(c, _) = &self.state.stroke else {
            return None;
        };
        let scale = similarity_scale(&self.state.transform)?;
        (self.state.dash.is_empty() && (closed || self.state.line_cap == LineCap::Butt))
            .then(|| (x, y, r, self.state.line_width * scale, rgba(*c)))
    }

    fn gpu_stroke(&mut self, path: Option<&tiny_skia::Path>, shape: PathShape) {
        let circle = self.circle_stroke(shape);
        let (width, cap, join) = self.gpu_stroke_params();
        let p = draw_params(&self.state);
        if let Some((x, y, r, w, c)) = circle {
            if let Surface::Gpu(g) = &mut self.surface {
                let _ = g.stroke_circle(snowlark_paint::Point::new(x, y), r, w, c, &p);
            }
            return;
        }
        let Some(path) = path else {
            return;
        };
        // HTML: zero-length segments and subpaths are not stroked.
        let data = prune_for_stroke(&path_data(path));
        if data.is_empty() {
            return;
        }
        let scale = (self.state.transform.sx * self.state.transform.sy
            - self.state.transform.kx * self.state.transform.ky)
            .abs()
            .sqrt();
        if wide_on_curves(&data, width * scale) {
            // Direct2D's outline of a line much wider than the curves it follows folds over itself:
            // such strokes are rasterized on the CPU and composited.
            let image = self.cpu_image(|pm, c| {
                if let Some(paint) = skia_paint(
                    &c.state.stroke,
                    1.0,
                    BlendMode::SourceOver,
                    c.state.transform,
                ) {
                    pm.stroke_path(
                        path,
                        &paint,
                        &c.skia_stroke_style(),
                        Transform::identity(),
                        None,
                    );
                }
            });
            if let (Some(image), Surface::Gpu(g)) = (image, &mut self.surface) {
                let _ = g.draw_device_image(&image, &p);
            }
            return;
        }
        let stroke = StrokeParams {
            width,
            cap,
            join,
            miter_limit: self.state.miter_limit,
            dash: &self.state.dash,
            dash_offset: self.state.dash_offset,
        };
        gpu_paint(
            &mut self.surface,
            &mut self.pattern_images,
            &self.state.stroke,
            self.state.transform,
            |g, paint| {
                let _ = g.stroke(&data, &stroke, paint, &p);
            },
        );
    }

    // ----- operations -----

    /// `fillRect`.
    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if !finite(&[x, y, w, h]) || w == 0.0 || h == 0.0 {
            return;
        }
        self.make_surface();
        if matches!(self.surface, Surface::Gpu(_)) {
            if let Some(r) = device_rect(&self.state.transform, x, y, w, h) {
                self.gpu_fill(FillShape::Rect(r));
            } else {
                let mut p = CanvasPath::new();
                p.rect(&self.state.transform, x, y, w, h);
                if let Some(path) = p.finish() {
                    let data = path_data(&path);
                    self.gpu_fill(FillShape::Path(&data, false));
                }
            }
            self.touched();
            return;
        }
        let mut p = CanvasPath::new();
        p.rect(&self.state.transform, x, y, w, h);
        if let Some(path) = p.finish() {
            let style = self.state.fill.clone();
            self.skia_fill(&path, FillRule::Winding, &style);
            self.touched();
        }
    }

    /// `strokeRect`.
    pub fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if !finite(&[x, y, w, h]) {
            return;
        }
        let mut p = CanvasPath::new();
        p.rect(&self.state.transform, x, y, w, h);
        if let Some(path) = p.finish() {
            self.make_surface();
            if matches!(self.surface, Surface::Gpu(_)) {
                self.gpu_stroke(Some(&path), PathShape::Other);
            } else {
                self.skia_stroke(&path);
            }
            self.touched();
        }
    }

    /// `clearRect`: transparent black, whatever the compositing.
    pub fn clear_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if !finite(&[x, y, w, h]) {
            return;
        }
        let t = self.state.transform;
        // (an unmade bitmap is transparent black already)
        if matches!(self.surface, Surface::Unmade(_)) {
            self.touched();
            return;
        }
        if let Surface::Gpu(g) = &mut self.surface {
            let clip = self.state.clip.as_ref().and_then(|c| c.gpu.clone());
            match device_rect(&t, x, y, w, h) {
                Some(r) => {
                    let _ = g.clear(FillShape::Rect(r), clip.as_ref());
                }
                None => {
                    let mut p = CanvasPath::new();
                    p.rect(&t, x, y, w, h);
                    if let Some(path) = p.finish() {
                        let data = path_data(&path);
                        let _ = g.clear(FillShape::Path(&data, false), clip.as_ref());
                    }
                }
            }
            self.touched();
            return;
        }
        let mut p = CanvasPath::new();
        p.rect(&t, x, y, w, h);
        let Some(path) = p.finish() else {
            return;
        };
        let paint = Paint {
            blend_mode: BlendMode::Clear,
            anti_alias: true,
            ..Paint::default()
        };
        let clip = self.state.clip.clone();
        if let Surface::Skia(pm) = &mut self.surface {
            pm.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                clip.as_ref().and_then(|c| c.mask.as_ref()),
            );
        }
        self.touched();
    }

    /// `fill(path?, fillRule)`.
    pub fn fill(&mut self, path: Option<&CanvasPath>, evenodd: bool) {
        self.make_surface();
        let source = path.unwrap_or(&self.path);
        let shape = source.shape;
        if matches!(self.surface, Surface::Gpu(_)) {
            match shape {
                PathShape::Circle { x, y, r, .. } => {
                    self.gpu_fill(FillShape::Circle(snowlark_paint::Point::new(x, y), r))
                }
                PathShape::Rect { x0, y0, x1, y1 } => self.gpu_fill(FillShape::Rect(
                    snowlark_paint::Rect::from_ltrb(x0, y0, x1, y1),
                )),
                _ => {
                    let Some(p) = source.finish() else {
                        return;
                    };
                    let data = path_data(&p);
                    self.gpu_fill(FillShape::Path(&data, evenodd));
                }
            }
            self.touched();
            return;
        }
        let Some(p) = source.finish() else {
            return;
        };
        let style = self.state.fill.clone();
        let rule = if evenodd {
            FillRule::EvenOdd
        } else {
            FillRule::Winding
        };
        self.skia_fill(&p, rule, &style);
        self.touched();
    }

    /// `stroke(path?)`.
    pub fn stroke(&mut self, path: Option<&CanvasPath>) {
        self.make_surface();
        let source = path.unwrap_or(&self.path);
        let shape = source.shape;
        if matches!(self.surface, Surface::Gpu(_)) {
            // (a circle primitive needs no path)
            if self.circle_stroke(shape).is_some() {
                self.gpu_stroke(None, shape);
            } else {
                let Some(p) = source.finish() else {
                    return;
                };
                self.gpu_stroke(Some(&p), shape);
            }
            self.touched();
            return;
        }
        let Some(p) = source.finish() else {
            return;
        };
        self.skia_stroke(&p);
        self.touched();
    }

    /// `clip(path?, fillRule)`: intersects the clipping region with the path.
    pub fn clip(&mut self, path: Option<&CanvasPath>, evenodd: bool) {
        let (w, h) = (self.width.max(1), self.height.max(1));
        let source = path.unwrap_or(&self.path);
        let shape = source.shape;
        let finished = source.finish();
        let rule = if evenodd {
            FillRule::EvenOdd
        } else {
            FillRule::Winding
        };
        if self.on_gpu() {
            let mut clip = self
                .state
                .clip
                .as_ref()
                .and_then(|c| c.gpu.as_deref().cloned())
                .unwrap_or_default();
            let first = clip.parts.is_empty();
            let rect = match shape {
                PathShape::Rect { x0, y0, x1, y1 } => {
                    Some(snowlark_paint::Rect::from_ltrb(x0, y0, x1, y1))
                }
                _ => None,
            };
            match &finished {
                Some(p) => clip.parts.push((path_data(p), evenodd)),
                // An empty path clips everything out.
                None => clip.parts.push((PathData::default(), evenodd)),
            }
            clip.rect = match (rect, clip.rect, first) {
                (Some(r), _, true) => Some(r),
                (Some(r), Some(old), false) => Some(r.intersect(&old)),
                _ => None,
            };
            if finished.is_none() {
                clip.rect = Some(snowlark_paint::Rect::new(0.0, 0.0, 0.0, 0.0));
            }
            self.state.clip = Some(Arc::new(Clip {
                mask: None,
                gpu: Some(Arc::new(clip)),
            }));
            return;
        }
        let Some(p) = finished else {
            // An empty path clips everything out.
            self.state.clip = Some(Arc::new(Clip {
                mask: Mask::new(w, h),
                gpu: None,
            }));
            return;
        };
        let mask = match self.skia_mask() {
            Some(old) => {
                let mut m = old.clone();
                m.intersect_path(&p, rule, true, Transform::identity());
                m
            }
            None => {
                let Some(mut m) = Mask::new(w, h) else {
                    return;
                };
                m.fill_path(&p, rule, true, Transform::identity());
                m
            }
        };
        self.state.clip = Some(Arc::new(Clip {
            mask: Some(mask),
            gpu: None,
        }));
    }

    /// `isPointInPath(x, y)` (in user space, against the current default path or `path`).
    pub fn is_point_in_path(
        &self,
        path: Option<&CanvasPath>,
        x: f32,
        y: f32,
        evenodd: bool,
    ) -> bool {
        let Some(p) = path.unwrap_or(&self.path).finish() else {
            return false;
        };
        let mut pt = Point::from_xy(x, y);
        self.state.transform.map_point(&mut pt);
        // Rasterize the path into a 1-pixel mask around the point.
        let Some(mut m) = Mask::new(1, 1) else {
            return false;
        };
        let rule = if evenodd {
            FillRule::EvenOdd
        } else {
            FillRule::Winding
        };
        m.fill_path(
            &p,
            rule,
            false,
            Transform::from_translate(-pt.x + 0.5, -pt.y + 0.5),
        );
        m.data()[0] > 127
    }

    /// `drawImage`: the source's pixels (`src` rect, in its pixels) into the `dst` rect in user
    /// space.
    pub fn draw_image(
        &mut self,
        image: &Pixmap,
        src: (f32, f32, f32, f32),
        dst: (f32, f32, f32, f32),
    ) {
        let (sx, sy, sw, sh) = src;
        let (dx, dy, dw, dh) = dst;
        if sw == 0.0
            || sh == 0.0
            || dw == 0.0
            || dh == 0.0
            || !finite(&[sx, sy, sw, sh, dx, dy, dw, dh])
        {
            return;
        }
        self.make_surface();
        if matches!(self.surface, Surface::Gpu(_)) {
            let bgra = rgba_to_bgra(image.data());
            let (iw, ih) = (image.width(), image.height());
            self.gpu_draw_image(iw, ih, src, dst, |g, src, dst, p| {
                if let Ok(img) = g.image_from_bgra(iw, ih, &bgra) {
                    let _ = g.draw_image(&img, src, dst, p);
                }
            });
            return;
        }
        // The image as a pattern mapping the source rect onto the destination rect.
        let t = self
            .state
            .transform
            .pre_translate(dx, dy)
            .pre_scale(dw / sw, dh / sh)
            .pre_translate(-sx, -sy);
        self.draw_image_pattern(image, t, dst);
    }

    /// `drawImage` of a decoded image (HTML: its default frame; `src` in its pixels). On the GPU
    /// the frame is uploaded by its first draw and kept for the next ones
    /// ([`GpuCanvas::draw_frame`]), so a draw copies no pixels.
    pub fn draw_decoded_image(
        &mut self,
        image: &Arc<DecodedImage>,
        src: (f32, f32, f32, f32),
        dst: (f32, f32, f32, f32),
    ) {
        let (sx, sy, sw, sh) = src;
        let (dx, dy, dw, dh) = dst;
        if sw == 0.0
            || sh == 0.0
            || dw == 0.0
            || dh == 0.0
            || !finite(&[sx, sy, sw, sh, dx, dy, dw, dh])
        {
            return;
        }
        self.make_surface();
        if matches!(self.surface, Surface::Gpu(_)) {
            let frame = image.default_frame();
            let (iw, ih) = (frame.width.max(1), frame.height.max(1));
            self.gpu_draw_image(iw, ih, src, dst, |g, src, dst, p| {
                let _ = g.draw_frame(image, src, dst, p);
            });
            return;
        }
        if let Some(pm) = pixmap_from_frame(image.default_frame()) {
            self.draw_image(&pm, src, dst);
        }
    }

    /// The bitmap as an image of this thread's canvas device, for another GPU canvas to draw
    /// ([`Canvas2D::draw_canvas_image`]): a copy on the GPU. `None` off the GPU or unmade.
    pub fn gpu_snapshot(&mut self) -> Option<CanvasImage> {
        match &mut self.surface {
            Surface::Gpu(g) => Some(g.snapshot()),
            _ => None,
        }
    }

    /// `drawImage` of a GPU canvas's [`Canvas2D::gpu_snapshot`]: drawn on the device, no pixels
    /// read. False (nothing drawn) when this canvas cannot draw it: off the GPU, or on another
    /// device; the caller then draws the source's pixels.
    pub fn draw_canvas_image(
        &mut self,
        image: &CanvasImage,
        src: (f32, f32, f32, f32),
        dst: (f32, f32, f32, f32),
    ) -> bool {
        let (sx, sy, sw, sh) = src;
        let (dx, dy, dw, dh) = dst;
        if sw == 0.0
            || sh == 0.0
            || dw == 0.0
            || dh == 0.0
            || !finite(&[sx, sy, sw, sh, dx, dy, dw, dh])
        {
            return true;
        }
        self.make_surface();
        match &self.surface {
            Surface::Gpu(g) if g.can_draw(image) => {}
            _ => return false,
        }
        self.gpu_draw_image(image.width, image.height, src, dst, |g, src, dst, p| {
            let _ = g.draw_image(image, src, dst, p);
        });
        true
    }

    /// The GPU part of `drawImage`: the source rectangle clipped to the `iw`×`ih` image, and the
    /// destination in the same proportion; `draw` draws the image with them when something is
    /// drawn.
    fn gpu_draw_image(
        &mut self,
        iw: u32,
        ih: u32,
        src: (f32, f32, f32, f32),
        dst: (f32, f32, f32, f32),
        draw: impl FnOnce(&mut GpuCanvas, snowlark_paint::Rect, snowlark_paint::Rect, &DrawParams),
    ) {
        let (mut sx, mut sy, mut sw, mut sh) = src;
        let (mut dx, mut dy, mut dw, mut dh) = dst;
        {
            // HTML: negative sizes flip to positive ones; the source rectangle is clipped to the
            // image, and the destination in the same proportion.
            if sw < 0.0 {
                sx += sw;
                sw = -sw;
            }
            if sh < 0.0 {
                sy += sh;
                sh = -sh;
            }
            if dw < 0.0 {
                dx += dw;
                dw = -dw;
            }
            if dh < 0.0 {
                dy += dh;
                dh = -dh;
            }
            let (iw, ih) = (iw as f32, ih as f32);
            let (kx, ky) = (dw / sw, dh / sh);
            let x0 = sx.max(0.0);
            let y0 = sy.max(0.0);
            let x1 = (sx + sw).min(iw);
            let y1 = (sy + sh).min(ih);
            if x1 <= x0 || y1 <= y0 {
                return;
            }
            let dst = snowlark_paint::Rect::from_ltrb(
                dx + (x0 - sx) * kx,
                dy + (y0 - sy) * ky,
                dx + (x1 - sx) * kx,
                dy + (y1 - sy) * ky,
            );
            let src = snowlark_paint::Rect::from_ltrb(x0, y0, x1, y1);
            let p = draw_params(&self.state);
            if let Surface::Gpu(g) = &mut self.surface {
                draw(g, src, dst, &p);
            }
            self.touched();
        }
    }

    /// The CPU part of `drawImage`: the image as a pattern mapping the source rect onto the
    /// destination rect (`t`).
    fn draw_image_pattern(&mut self, image: &Pixmap, t: Transform, dst: (f32, f32, f32, f32)) {
        let (dx, dy, dw, dh) = dst;
        let paint = Paint {
            shader: tiny_skia::Pattern::new(
                image.as_ref(),
                SpreadMode::Pad,
                if self.state.image_smoothing {
                    FilterQuality::Bilinear
                } else {
                    FilterQuality::Nearest
                },
                self.state.global_alpha,
                t,
            ),
            blend_mode: self.state.composite,
            anti_alias: image_antialiased(&self.state.transform, dw, dh),
            ..Paint::default()
        };
        let mut p = CanvasPath::new();
        p.rect(&self.state.transform, dx, dy, dw, dh);
        let Some(path) = p.finish() else {
            return;
        };
        let clip = self.state.clip.clone();
        if let Surface::Skia(pm) = &mut self.surface {
            pm.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                clip.as_ref().and_then(|c| c.mask.as_ref()),
            );
        }
        self.touched();
    }

    /// The bitmap as a pattern image for `drawImage`/`createPattern` of this canvas.
    pub fn snapshot(&mut self) -> Option<Pixmap> {
        match &mut self.surface {
            Surface::None => None,
            Surface::Skia(pm) => Some(pm.clone()),
            Surface::Unmade(_) => Pixmap::new(self.width.max(1), self.height.max(1)),
            Surface::Gpu(g) => {
                let (w, h) = g.size();
                let bgra = g.read(0, 0, w, h).ok()?;
                let mut pm = Pixmap::new(w, h)?;
                pm.data_mut().copy_from_slice(&rgba_to_bgra(&bgra));
                Some(pm)
            }
        }
    }

    /// `getImageData`: non-premultiplied RGBA rows of the rectangle (outside the bitmap: transparent
    /// black).
    pub fn get_image_data(&mut self, x: i32, y: i32, w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        match &mut self.surface {
            Surface::None | Surface::Unmade(_) => {}
            Surface::Gpu(g) => {
                if let Ok(bgra) = g.read(x, y, w, h) {
                    for (o, i) in out
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(bgra.as_chunks::<4>().0)
                    {
                        let a = i[3];
                        o[0] = demultiply(i[2], a);
                        o[1] = demultiply(i[1], a);
                        o[2] = demultiply(i[0], a);
                        o[3] = a;
                    }
                }
            }
            Surface::Skia(pm) => {
                for row in 0..h as i32 {
                    let sy = y + row;
                    if sy < 0 || sy >= pm.height() as i32 {
                        continue;
                    }
                    for col in 0..w as i32 {
                        let sx = x + col;
                        if sx < 0 || sx >= pm.width() as i32 {
                            continue;
                        }
                        let px = pm.pixel(sx as u32, sy as u32).unwrap_or_else(|| {
                            tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, 0).unwrap()
                        });
                        let c = px.demultiply();
                        let i = ((row as usize) * (w as usize) + col as usize) * 4;
                        out[i] = c.red();
                        out[i + 1] = c.green();
                        out[i + 2] = c.blue();
                        out[i + 3] = c.alpha();
                    }
                }
            }
        }
        out
    }

    /// `putImageData`: writes non-premultiplied RGBA pixels, unaffected by state.
    #[allow(clippy::too_many_arguments)]
    pub fn put_image_data(
        &mut self,
        data: &[u8],
        w: u32,
        h: u32,
        dx: i32,
        dy: i32,
        dirty: (i32, i32, i32, i32),
    ) {
        let (mut rx, mut ry, mut rw, mut rh) = dirty;
        if rw < 0 {
            rx += rw;
            rw = -rw;
        }
        if rh < 0 {
            ry += rh;
            rh = -rh;
        }
        let x0 = rx.max(0);
        let y0 = ry.max(0);
        let x1 = (rx + rw).min(w as i32);
        let y1 = (ry + rh).min(h as i32);
        self.make_surface();
        match &mut self.surface {
            // (made above)
            Surface::None | Surface::Unmade(_) => return,
            Surface::Gpu(g) => {
                if x1 <= x0 || y1 <= y0 {
                    return;
                }
                let (cw, ch) = ((x1 - x0) as usize, (y1 - y0) as usize);
                let mut bgra = vec![0u8; cw * ch * 4];
                for row in 0..ch {
                    for col in 0..cw {
                        let i = (((y0 as usize + row) * w as usize) + x0 as usize + col) * 4;
                        if i + 3 >= data.len() {
                            continue;
                        }
                        let a = data[i + 3];
                        let o = (row * cw + col) * 4;
                        bgra[o] = premultiply(data[i + 2], a);
                        bgra[o + 1] = premultiply(data[i + 1], a);
                        bgra[o + 2] = premultiply(data[i], a);
                        bgra[o + 3] = a;
                    }
                }
                let _ = g.write(dx + x0, dy + y0, cw as u32, ch as u32, &bgra);
            }
            Surface::Skia(pm) => {
                let (pw, ph) = (pm.width() as i32, pm.height() as i32);
                let pixels = pm.pixels_mut();
                for sy in y0..y1 {
                    let ty = dy + sy;
                    if ty < 0 || ty >= ph {
                        continue;
                    }
                    for sx in x0..x1 {
                        let tx = dx + sx;
                        if tx < 0 || tx >= pw {
                            continue;
                        }
                        let i = ((sy as usize) * (w as usize) + sx as usize) * 4;
                        if i + 3 >= data.len() {
                            continue;
                        }
                        let c = tiny_skia::ColorU8::from_rgba(
                            data[i],
                            data[i + 1],
                            data[i + 2],
                            data[i + 3],
                        );
                        pixels[(ty as usize) * (pw as usize) + tx as usize] = c.premultiply();
                    }
                }
            }
        }
        self.touched();
    }

    /// The SVG text element that draws `text` at (x, y) with the current font and alignment.
    fn text_svg(
        &self,
        text: &str,
        x: f32,
        y: f32,
        fill: &str,
        stroke: Option<(&str, f32)>,
    ) -> String {
        let anchor = match (
            self.state.text_align.as_str(),
            self.state.direction.as_str(),
        ) {
            ("center", _) => "middle",
            ("right", _) | ("end", "inherit" | "ltr") => "end",
            ("start", "rtl") => "end",
            ("end", "rtl") | ("left", _) => "start",
            _ => "start",
        };
        let baseline = match self.state.text_baseline.as_str() {
            "top" => "text-before-edge",
            "hanging" => "hanging",
            "middle" => "central",
            "bottom" => "text-after-edge",
            "ideographic" => "ideographic",
            _ => "alphabetic",
        };
        let escape = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let paint = match stroke {
            Some((color, width)) => {
                format!("fill=\"none\" stroke=\"{color}\" stroke-width=\"{width}\"")
            }
            None => format!("fill=\"{fill}\""),
        };
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\"><text x=\"{x}\" y=\"{y}\" text-anchor=\"{anchor}\" dominant-baseline=\"{baseline}\" style=\"font: {font}; white-space: pre\" {paint}>{text}</text></svg>",
            w = self.width.max(1),
            h = self.height.max(1),
            font = escape(&self.state.font),
            text = escape(text),
        )
    }

    fn style_color(style: &Style) -> String {
        match style {
            Style::Color(c, _) => {
                let c8 = c.to_color_u8();
                format!(
                    "rgba({},{},{},{})",
                    c8.red(),
                    c8.green(),
                    c8.blue(),
                    c.alpha()
                )
            }
            // (gradients and patterns on text: their first colour, or black)
            Style::Gradient(g) => g
                .stops
                .first()
                .map(|(_, c)| Self::style_color(&Style::Color(*c, String::new())))
                .unwrap_or_else(|| "black".into()),
            Style::Pattern(_) => "black".into(),
        }
    }

    /// The text shaped with the current font through the document's text engine (its web fonts
    /// included), and the primary font's ascent and descent.
    fn gpu_shape(&mut self, fonts: &Fonts, text: &str) -> Option<(Arc<ShapedText>, f32, f32)> {
        let engine: &TextEngine = &fonts.text;
        let engine_id = Arc::as_ptr(&fonts.text) as usize;
        let env_id = Arc::as_ptr(&fonts.env) as usize;
        let current = self.font.as_ref().is_some_and(|f| {
            f.value == self.state.font && f.engine == engine_id && f.env == env_id
        });
        if !current {
            let spec = parse_font(&self.state.font);
            // CSS Fonts 4 §4.1: an @font-face family stands for its loaded faces, best match first.
            let mut names: Vec<&str> = Vec::new();
            for family in &spec.families {
                // A generic family (an unquoted keyword) never names an @font-face rule.
                let (name, quoted_keyword) = match family {
                    FamilyName::Generic(g) => {
                        names.push(g);
                        continue;
                    }
                    FamilyName::Named(n) => (n.as_str(), is_generic_keyword(n)),
                };
                match fonts.env.fonts.family(name) {
                    Some(web) => {
                        let mut faces: Vec<_> = web.faces.iter().collect();
                        let want = Wanted::new(spec.weight, spec.style);
                        faces.sort_by_key(|f| (face_rank(f, &want), std::cmp::Reverse(f.order)));
                        names.extend(faces.iter().map(|f| f.internal.as_str()));
                    }
                    // (a quoted keyword without such a rule names no installed family)
                    None if quoted_keyword => {}
                    None => names.push(name),
                }
            }
            let query = FontQuery {
                families: &names,
                weight: spec.weight,
                style: spec.style,
                stretch: FontStretch::NORMAL,
            };
            let list = engine
                .font_list(&query)
                .or_else(|_| {
                    engine.font_list(&FontQuery {
                        families: &["sans-serif"],
                        ..query
                    })
                })
                .ok()?;
            let m = engine.metrics(engine.primary_font(list), spec.size);
            self.font = Some(ResolvedFont {
                value: self.state.font.clone(),
                engine: engine_id,
                env: env_id,
                list,
                size: spec.size,
                small_caps: spec.small_caps,
                ascent: m.ascent,
                descent: m.descent,
            });
        }
        let f = self.font.as_ref()?;
        let (list, size, small_caps, ascent, descent) =
            (f.list, f.size, f.small_caps, f.ascent, f.descent);
        let options = ShapeOptions {
            size,
            direction: if self.state.direction == "rtl" {
                Direction::Rtl
            } else {
                Direction::Ltr
            },
            ..Default::default()
        };
        let mut cache = fonts.cache.lock().unwrap_or_else(|e| e.into_inner());
        let mut shaped = ShapedText::default();
        if small_caps {
            shape_small_caps(engine, list, &options, &mut cache.words, text, &mut shaped)?;
            return Some((Arc::new(shaped), ascent, descent));
        }
        // The text as a whole first (a label measured, then drawn; the same labels drawn again),
        // then word by word.
        let key = ShapeKey {
            text,
            list,
            size,
            rtl: options.direction == Direction::Rtl,
            vertical: VerticalText::None,
            locale: options.locale,
            features: options.features,
        };
        if let Some(s) = cache.runs.get(&key) {
            return Some((s, ascent, descent));
        }
        engine
            .shape_cached(text, list, &options, Some(&mut cache.words), &mut shaped)
            .ok()?;
        let shaped = Arc::new(shaped);
        cache.runs.insert(&key, shaped.clone());
        Some((shaped, ascent, descent))
    }

    /// `fillText`/`strokeText`.
    pub fn draw_text(&mut self, fonts: &Fonts, text: &str, x: f32, y: f32, stroke: bool) {
        if !finite(&[x, y]) || text.is_empty() {
            return;
        }
        // HTML "text preparation": ASCII whitespace becomes spaces.
        let text: String = text
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect();
        self.make_surface();
        if matches!(self.surface, Surface::Gpu(_)) {
            self.gpu_text(fonts, &text, x, y, stroke);
            self.touched();
            return;
        }
        let svg = if stroke {
            let color = Self::style_color(&self.state.stroke);
            self.text_svg(&text, x, y, "", Some((&color, self.state.line_width)))
        } else {
            let color = Self::style_color(&self.state.fill);
            self.text_svg(&text, x, y, &color, None)
        };
        let options = usvg::Options {
            fontdb: cpu_fonts(fonts),
            ..usvg::Options::default()
        };
        let Ok(tree) = usvg::Tree::from_str(&svg, &options) else {
            return;
        };
        let state = &self.state;
        let Surface::Skia(pm) = &mut self.surface else {
            return;
        };
        if state.global_alpha >= 1.0
            && state.composite == BlendMode::SourceOver
            && state.clip.is_none()
        {
            resvg::render(&tree, state.transform, &mut pm.as_mut());
        } else {
            // Draw into a layer, then composite it with the state's alpha, mode and clip. The layer
            // covers the text's device bounds (with a margin for antialiasing), not the canvas:
            // charts draw many short labels under a clip.
            let (cw, ch) = (pm.width() as i32, pm.height() as i32);
            let Some((x0, y0, w, h)) = text_layer_bounds(&tree, state.transform, cw, ch) else {
                return;
            };
            let Some(mut layer) = Pixmap::new(w, h) else {
                return;
            };
            let to_layer = state.transform.post_translate(-x0 as f32, -y0 as f32);
            resvg::render(&tree, to_layer, &mut layer.as_mut());
            let paint = PixmapPaint {
                opacity: state.global_alpha,
                blend_mode: state.composite,
                quality: FilterQuality::Nearest,
            };
            pm.draw_pixmap(
                x0,
                y0,
                layer.as_ref(),
                &paint,
                Transform::identity(),
                state.clip.as_ref().and_then(|c| c.mask.as_ref()),
            );
        }
        self.touched();
    }

    /// Text on the GPU backend (HTML §4.12.5.1.4 "text preparation algorithm": alignment by
    /// `textAlign` and `direction`, the baseline by `textBaseline` against the font's metrics).
    fn gpu_text(&mut self, fonts: &Fonts, text: &str, x: f32, y: f32, stroke: bool) {
        let Some((shaped, ascent, descent)) = self.gpu_shape(fonts, text) else {
            return;
        };
        let width = shaped.width();
        let rtl = self.state.direction == "rtl";
        let dx = match self.state.text_align.as_str() {
            "center" => -width / 2.0,
            "right" => -width,
            "end" if !rtl => -width,
            "start" if rtl => -width,
            _ => 0.0,
        };
        let dy = match self.state.text_baseline.as_str() {
            "top" => ascent,
            "hanging" => ascent * 0.8,
            "middle" => (ascent - descent) / 2.0,
            "bottom" | "ideographic" => -descent,
            _ => 0.0,
        };
        let origin = snowlark_paint::Point::new(x + dx, y + dy);
        let (width_line, cap, join) = self.gpu_stroke_params();
        let state = &self.state;
        let style = if stroke { &state.stroke } else { &state.fill };
        let p = draw_params(state);
        let s = StrokeParams {
            width: width_line,
            cap,
            join,
            miter_limit: state.miter_limit,
            dash: &state.dash,
            dash_offset: state.dash_offset,
        };
        gpu_paint(
            &mut self.surface,
            &mut self.pattern_images,
            style,
            state.transform,
            |g, paint| {
                let _ = g.draw_text(
                    &fonts.text,
                    &shaped,
                    origin,
                    paint,
                    stroke.then_some(&s),
                    &p,
                );
            },
        );
    }

    /// `measureText(text).width` and the font's ascent and descent, in CSS px.
    pub fn measure_text(&mut self, fonts: &Fonts, text: &str) -> (f32, f32, f32) {
        let size = font_size(&self.state.font);
        if text.is_empty() {
            return (0.0, size * 0.8, size * 0.2);
        }
        if self.on_gpu() {
            let text: String = text
                .chars()
                .map(|c| if c.is_whitespace() { ' ' } else { c })
                .collect();
            if let Some((shaped, ascent, descent)) = self.gpu_shape(fonts, &text) {
                return (shaped.width(), ascent, descent);
            }
            return (0.0, size * 0.8, size * 0.2);
        }
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100000\" height=\"1000\"><text x=\"0\" y=\"500\" style=\"font: {}; white-space: pre\">{}</text></svg>",
            self.state.font.replace('"', "&quot;").replace('<', "&lt;"),
            text.replace('&', "&amp;").replace('<', "&lt;")
        );
        let options = usvg::Options {
            fontdb: cpu_fonts(fonts),
            ..usvg::Options::default()
        };
        let Ok(tree) = usvg::Tree::from_str(&svg, &options) else {
            return (0.0, size * 0.8, size * 0.2);
        };
        let width = tree
            .root()
            .children()
            .first()
            .map(|n| n.abs_bounding_box().width())
            .unwrap_or(0.0);
        (width, size * 0.8, size * 0.2)
    }

    /// Queues the GPU bitmap's copy for the next frame read and sends it to the GPU without waiting:
    /// called for every canvas of a rendering update before their frames are asked for, so that the
    /// reads wait for the GPU once rather than once a canvas.
    pub fn start_frame(&mut self) {
        if let Surface::Gpu(g) = &mut self.surface {
            // (on failure the read copies as it would without this)
            let _ = g.start_read_all();
        }
    }

    /// The bitmap as a frame for the page's image store, as [`Canvas2D::frame`], without waiting for
    /// a GPU bitmap: its canvas thread reads it once it has drawn what is recorded so far and calls
    /// `then` (the page thread's wake-up), while the caller goes on. Any other bitmap's frame is
    /// ready at once.
    pub fn request_frame(
        &mut self,
        then: Option<snowlark_paint::canvas_thread::Wake>,
    ) -> FrameRequest {
        match &mut self.surface {
            Surface::Gpu(g) => FrameRequest(Pending::Gpu(g.read_later(then))),
            _ => FrameRequest(Pending::Ready(self.frame())),
        }
    }

    /// The bitmap as a frame for the page's image store (premultiplied BGRA).
    pub fn frame(&mut self) -> Option<Frame> {
        match &mut self.surface {
            Surface::None => None,
            Surface::Unmade(_) => {
                let (width, height) = (self.width.max(1), self.height.max(1));
                Some(Frame {
                    width,
                    height,
                    pixels: vec![0; width as usize * height as usize * 4],
                    delay_ms: 0,
                })
            }
            Surface::Skia(pm) => {
                let mut pixels = pm.data().to_vec();
                for px in pixels.as_chunks_mut::<4>().0 {
                    px.swap(0, 2);
                }
                Some(Frame {
                    width: pm.width(),
                    height: pm.height(),
                    pixels,
                    delay_ms: 0,
                })
            }
            Surface::Gpu(g) => {
                let (w, h) = g.size();
                let pixels = g.read(0, 0, w, h).ok()?;
                Some(Frame {
                    width: w,
                    height: h,
                    pixels,
                    delay_ms: 0,
                })
            }
        }
    }
}

/// A frame [`Canvas2D::request_frame`] asked for.
pub struct FrameRequest(Pending);

enum Pending {
    Ready(Option<Frame>),
    Gpu(snowlark_paint::canvas_thread::PendingRead),
    Taken,
}

impl FrameRequest {
    /// The frame once it is there: `None` while the canvas thread has not read it, `Some(None)`
    /// when there is none (a failed read); each frame is given once.
    pub fn try_take(&mut self) -> Option<Option<Frame>> {
        let taken = match &self.0 {
            Pending::Ready(_) | Pending::Taken => None,
            Pending::Gpu(read) => Some(read.try_take()?),
        };
        Some(match std::mem::replace(&mut self.0, Pending::Taken) {
            Pending::Ready(frame) => frame,
            Pending::Gpu(read) => gpu_frame(read.width, read.height, taken?.ok()),
            Pending::Taken => None,
        })
    }

    /// The frame, waiting for the canvas thread.
    pub fn wait(self) -> Option<Frame> {
        match self.0 {
            Pending::Ready(frame) => frame,
            Pending::Gpu(read) => {
                let (w, h) = (read.width, read.height);
                gpu_frame(w, h, read.wait().ok())
            }
            Pending::Taken => None,
        }
    }
}

fn gpu_frame(width: u32, height: u32, pixels: Option<Vec<u8>>) -> Option<Frame> {
    Some(Frame {
        width,
        height,
        pixels: pixels?,
        delay_ms: 0,
    })
}

/// A canvas `font` value (CSS Fonts 4 `font` shorthand): the family list, weight, style and size in
/// px. Values that do not parse give the canvas default, `10px sans-serif`.
pub struct FontSpec {
    pub families: Vec<FamilyName>,
    pub weight: u16,
    pub style: FontStyle,
    pub size: f32,
    /// `font-variant: small-caps`.
    pub small_caps: bool,
}

/// A `font-family` entry: a generic family keyword (unquoted), or a family name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FamilyName {
    Generic(String),
    Named(String),
}

/// CSS Fonts 4 §4.2 `<generic-family>` keywords (and `generic(...)`), ASCII case-insensitively.
fn is_generic_keyword(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "serif"
            | "sans-serif"
            | "cursive"
            | "fantasy"
            | "monospace"
            | "system-ui"
            | "math"
            | "ui-serif"
            | "ui-sans-serif"
            | "ui-monospace"
            | "ui-rounded"
    ) || n.starts_with("generic(")
}

/// CSS Fonts 4 §6.6 synthesized small capitals: lowercase letters as capitals at 70% of the size,
/// everything else as it is; the runs of both appended in order (glyphs and runs only: canvas text
/// needs no clusters).
fn shape_small_caps(
    engine: &TextEngine,
    list: FontListId,
    options: &ShapeOptions,
    words: &mut WordCache,
    text: &str,
    out: &mut ShapedText,
) -> Option<()> {
    out.clear();
    let mut piece = ShapedText::default();
    let mut rest = text;
    while !rest.is_empty() {
        let lower = |c: char| c.is_lowercase() && c.to_uppercase().next() != Some(c);
        let first = rest.chars().next()?;
        let small = lower(first);
        let end = rest
            .char_indices()
            .find(|&(_, c)| lower(c) != small)
            .map_or(rest.len(), |(i, _)| i);
        let (segment, size) = if small {
            (rest[..end].to_uppercase(), options.size * 0.7)
        } else {
            (rest[..end].to_string(), options.size)
        };
        let opts = ShapeOptions { size, ..*options };
        engine
            .shape_cached(&segment, list, &opts, Some(words), &mut piece)
            .ok()?;
        let g0 = out.glyphs.len() as u32;
        out.glyphs.extend_from_slice(&piece.glyphs);
        out.advances.extend_from_slice(&piece.advances);
        out.offsets.extend_from_slice(&piece.offsets);
        out.runs
            .extend(piece.runs.iter().map(|r| snowlark_paint::ShapedRun {
                glyphs: (r.glyphs.0 + g0, r.glyphs.1 + g0),
                clusters: (0, 0),
                ..*r
            }));
        rest = &rest[end..];
    }
    Some(())
}

/// Parses the `font` shorthand: `[style] [variant] [weight] [stretch] size[/line-height] family,
/// ...`.
pub fn parse_font(font: &str) -> FontSpec {
    let default = || FontSpec {
        families: vec![FamilyName::Generic("sans-serif".into())],
        weight: 400,
        style: FontStyle::Normal,
        size: 10.0,
        small_caps: false,
    };
    let mut weight = 400;
    let mut style = FontStyle::Normal;
    let mut small_caps = false;
    let mut rest = font.trim();
    loop {
        let (word, after) = match rest.find(char::is_whitespace) {
            Some(i) => (&rest[..i], rest[i..].trim_start()),
            None => (rest, ""),
        };
        let lower = word.to_ascii_lowercase();
        match lower.as_str() {
            "small-caps" => small_caps = true,
            "normal" | "ultra-condensed" | "extra-condensed" | "condensed" | "semi-condensed"
            | "semi-expanded" | "expanded" | "extra-expanded" | "ultra-expanded" => {}
            "italic" => style = FontStyle::Italic,
            "oblique" => style = FontStyle::Oblique,
            "bold" | "bolder" => weight = 700,
            "lighter" => weight = 300,
            w if w.len() == 3 && w.parse::<u16>().is_ok_and(|n| (1..=1000).contains(&n)) => {
                weight = w.parse().unwrap()
            }
            _ => break,
        }
        if after.is_empty() {
            return default();
        }
        rest = after;
    }
    // The size, maybe with a line height.
    let (size_word, families) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => return default(),
    };
    let size_part = size_word.split('/').next().unwrap_or(size_word);
    let Some(size) = css_font_size(size_part) else {
        return default();
    };
    let families: Vec<FamilyName> = families
        .split(',')
        .filter_map(|f| {
            let f = f.trim();
            let quoted = f.len() >= 2
                && (f.starts_with('"') && f.ends_with('"')
                    || f.starts_with('\'') && f.ends_with('\''));
            if quoted {
                Some(FamilyName::Named(f[1..f.len() - 1].to_string()))
            } else if f.is_empty() {
                None
            } else if is_generic_keyword(f) {
                Some(FamilyName::Generic(f.to_ascii_lowercase()))
            } else {
                // Unquoted names are identifiers separated by single spaces.
                Some(FamilyName::Named(
                    f.split_whitespace().collect::<Vec<_>>().join(" "),
                ))
            }
        })
        .collect();
    if families.is_empty() {
        return default();
    }
    FontSpec {
        families,
        weight,
        style,
        size,
        small_caps,
    }
}

/// A `font-size` value in px (absolute units, em/rem against 16 px, the absolute-size keywords).
fn css_font_size(s: &str) -> Option<f32> {
    let lower = s.to_ascii_lowercase();
    let keyword = match lower.as_str() {
        "xx-small" => Some(9.0),
        "x-small" => Some(10.0),
        "small" => Some(13.0),
        "medium" => Some(16.0),
        "large" => Some(18.0),
        "x-large" => Some(24.0),
        "xx-large" => Some(32.0),
        "xxx-large" => Some(48.0),
        _ => None,
    };
    if keyword.is_some() {
        return keyword;
    }
    for (unit, k) in [
        ("px", 1.0),
        ("pt", 4.0 / 3.0),
        ("pc", 16.0),
        ("in", 96.0),
        ("cm", 96.0 / 2.54),
        ("mm", 96.0 / 25.4),
        ("rem", 16.0),
        ("em", 16.0),
        ("%", 0.16),
    ] {
        if let Some(n) = lower.strip_suffix(unit) {
            let v: f32 = n.parse().ok()?;
            return (v.is_finite() && v >= 0.0).then_some(v * k);
        }
    }
    None
}

/// The system fonts for the CPU backend's text: the renderer's, or (when it gave none, because
/// canvases draw on the GPU, but this one could not) the process's, loaded once.
fn cpu_fonts(given: &Fonts) -> Arc<usvg::fontdb::Database> {
    if let Some(db) = &given.cpu {
        return db.clone();
    }
    static FONTS: std::sync::OnceLock<Arc<usvg::fontdb::Database>> = std::sync::OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            snowlark_paint::font_files::map_all(&mut db);
            Arc::new(db)
        })
        .clone()
}

/// The `font-size` of a canvas `font` value in px (CSS px, pt and em/rem against 16 px).
pub fn font_size(font: &str) -> f32 {
    for part in font.split_whitespace() {
        let part = part.split('/').next().unwrap_or(part);
        for (unit, k) in [("px", 1.0), ("pt", 4.0 / 3.0), ("rem", 16.0), ("em", 16.0)] {
            if let Some(n) = part.strip_suffix(unit) {
                if let Ok(v) = n.parse::<f32>() {
                    return v * k;
                }
            }
        }
    }
    10.0
}

/// Converts a page image (premultiplied BGRA) to a tiny-skia pixmap (premultiplied RGBA).
pub fn pixmap_from_frame(frame: &Frame) -> Option<Pixmap> {
    let mut pm = Pixmap::new(frame.width.max(1), frame.height.max(1))?;
    let data = pm.data_mut();
    if data.len() != frame.pixels.len() {
        return None;
    }
    data.copy_from_slice(&frame.pixels);
    for px in data.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Some(pm)
}

/// The device rectangle (x, y, width, height) a text layer needs: the tree's layer bounds under
/// `transform`, two pixels wider each way for antialiasing, within the `cw` x `ch` canvas. `None` when
/// nothing of it is on the canvas.
fn text_layer_bounds(
    tree: &usvg::Tree,
    transform: Transform,
    cw: i32,
    ch: i32,
) -> Option<(i32, i32, u32, u32)> {
    let r = tree.root().abs_layer_bounding_box().transform(transform)?;
    let x0 = ((r.left().floor() as i32) - 2).max(0);
    let y0 = ((r.top().floor() as i32) - 2).max(0);
    let x1 = ((r.right().ceil() as i32) + 2).min(cw);
    let y1 = ((r.bottom().ceil() as i32) + 2).min(ch);
    (x1 > x0 && y1 > y0).then(|| (x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
}

#[cfg(test)]
mod tests;
