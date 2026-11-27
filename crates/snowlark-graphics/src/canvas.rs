// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The operations of a `<canvas>` 2D context's bitmap as data (HTML §4.12.5.1): the shapes, paints,
//! line styles, clip and drawing state `snowlark-render`'s `Canvas2D` hands the bitmap that draws
//! them (`snowlark-paint`'s `canvas`), in device space.

use std::sync::Arc;

use crate::filter::FilterOp;
use crate::geom::{Matrix, Point, Rect};

/// A straight (not premultiplied) RGBA colour, each channel 0..=1.
pub type Rgba = [f32; 4];

/// `globalCompositeOperation` (Compositing and Blending Level 1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompositeOp {
    #[default]
    SourceOver,
    SourceIn,
    SourceOut,
    SourceAtop,
    DestinationOver,
    DestinationIn,
    DestinationOut,
    DestinationAtop,
    Lighter,
    Copy,
    Xor,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

/// A path segment kind of [`PathData`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathVerb {
    /// One point.
    Move,
    /// One point.
    Line,
    /// Two points (control, end).
    Quad,
    /// Three points (two controls, end).
    Cubic,
    Close,
}

/// A path as verbs and the points they consume.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathData {
    pub verbs: Vec<PathVerb>,
    pub points: Vec<Point>,
}

impl PathData {
    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    /// The path with every point mapped through `m`.
    pub fn transformed(&self, m: &Matrix) -> PathData {
        PathData {
            verbs: self.verbs.clone(),
            points: self.points.iter().map(|p| m.apply(*p)).collect(),
        }
    }
}

/// A gradient colour stop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientStop {
    pub offset: f32,
    pub color: Rgba,
}

/// What a fill or stroke paints with. Gradient and pattern coordinates are in user space (mapped by
/// [`DrawParams::transform`]). `I` is the image type of the canvas that draws (`snowlark-paint`'s
/// `CanvasImage` of a thread's device, or a `RemoteImage` naming one on the canvas thread).
pub enum CanvasPaint<'a, I> {
    Solid(Rgba),
    /// Stops sorted by offset.
    Linear {
        start: Point,
        end: Point,
        stops: &'a [GradientStop],
    },
    Radial {
        c0: Point,
        r0: f32,
        c1: Point,
        r1: f32,
        stops: &'a [GradientStop],
    },
    Pattern {
        image: &'a I,
        repeat_x: bool,
        repeat_y: bool,
    },
    /// An image of the paint over the whole canvas in device pixels (a gradient rasterized on the
    /// CPU where the GPU's gradients differ from the canvas's).
    DeviceImage(&'a I),
}

impl<I> Clone for CanvasPaint<'_, I> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<I> Copy for CanvasPaint<'_, I> {}

/// A shadow (`shadowOffsetX/Y`, `shadowBlur`, `shadowColor`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    pub offset: (f32, f32),
    pub blur: f32,
    pub color: Rgba,
}

/// The clipping region: the intersection of device-space paths (`true`: even-odd rule). `rect`
/// is the region when it is one axis-aligned rectangle.
#[derive(Clone, Debug, Default)]
pub struct CanvasClip {
    pub parts: Vec<(PathData, bool)>,
    pub rect: Option<Rect>,
}

/// The state an operation draws with.
#[derive(Clone, Copy)]
pub struct DrawParams<'a> {
    /// The current transformation matrix (user space to device pixels).
    pub transform: Matrix,
    pub alpha: f32,
    pub op: CompositeOp,
    pub clip: Option<&'a Arc<CanvasClip>>,
    pub shadow: Option<Shadow>,
    /// `filter` (HTML "Filters"): the functions each operation's image goes through before its
    /// shadow and compositing, lengths in canvas pixels whatever the transform. `None`: `none`.
    pub filter: Option<&'a Arc<[FilterOp]>>,
    /// `imageSmoothingEnabled`.
    pub smoothing: bool,
}

impl DrawParams<'_> {
    /// Whether an operation draws straight into the bitmap: source-over, with no shadow and no
    /// filter (the others draw into a scratch image first).
    pub fn is_direct(&self) -> bool {
        self.op == CompositeOp::SourceOver && self.shadow.is_none() && self.filter.is_none()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineCap {
    #[default]
    Butt,
    Round,
    Square,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineJoin {
    #[default]
    Miter,
    Round,
    Bevel,
}

/// The line styles of a stroke, in user units.
#[derive(Clone, Copy, Debug)]
pub struct StrokeParams<'a> {
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f32,
    pub dash: &'a [f32],
    pub dash_offset: f32,
}

/// What to fill, in device pixels.
#[derive(Clone, Copy)]
pub enum FillShape<'a> {
    Rect(Rect),
    Circle(Point, f32),
    /// A path and its fill rule (`true`: even-odd).
    Path(&'a PathData, bool),
}
