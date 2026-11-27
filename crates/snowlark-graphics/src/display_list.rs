// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The display list: what layout hands to painting. A flat sequence of items in paint order, with clips,
//! transforms and opacity groups as balanced push/pop pairs. Glyphs and gradient stops live in side
//! vectors that items index by range, so building a list allocates nothing per item: a list that is
//! cleared and rebuilt every frame reuses its vectors.
//!
//! Fonts and images are referenced by id ([`FontId`] from `TextEngine` (`snowlark-paint`), [`ImageId`] from
//! `ImageStore`), so a list is plain data: it can be encoded to bytes ([`DisplayList::encode`])
//! and decoded again ([`DisplayList::decode`]), which checks every range and the push/pop nesting, so a
//! list from another process can't make the rasterizer read out of bounds.

use serde::{Deserialize, Serialize};

use crate::filter::{ColorFilters, FilterOp, FilterRange};
use crate::geom::{Color, Matrix, Point, Radii, Rect, Sides, Size};
use crate::svg_filter::SvgFilter;

/// A font face registered with the `TextEngine` (`snowlark-paint`) that shaped the glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FontId(pub u32);

/// A decoded image registered with an `ImageStore`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImageId(pub u32);

/// The axes on which a replaced element's content is clipped to its content box (CSS Overflow 3:
/// an `overflow` other than `visible` on that axis, or paint containment).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipAxes {
    pub x: bool,
    pub y: bool,
}

impl ClipAxes {
    /// Clipped on both axes.
    pub const BOTH: ClipAxes = ClipAxes { x: true, y: true };
}

/// A video element's frames in a `VideoStore` (`snowlark-paint`): the compositor draws the latest one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VideoId(pub u32);

/// A glyph's offset from its nominal position, as DirectWrite's `DWRITE_GLYPH_OFFSET` (same layout):
/// `advance_offset` along the run's direction, `ascender_offset` upwards.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct GlyphOffset {
    pub advance_offset: f32,
    pub ascender_offset: f32,
}

/// A range of glyphs in [`DisplayList::glyph_ids`] (and the parallel advance and offset vectors).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlyphRange {
    pub start: u32,
    pub len: u32,
}

/// A range of stops in [`DisplayList::stops`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StopRange {
    pub start: u32,
    pub len: u32,
}

/// A path's segments: `verbs` in [`PathData::verbs`] (each a [`PathVerb`]) and the points
/// they take, in order, from [`PathData::points`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathRange {
    pub verbs: u32,
    pub verbs_len: u32,
    pub points: u32,
    pub points_len: u32,
}

/// A path segment kind, as stored in [`PathData::verbs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PathVerb {
    /// Starts a subpath at one point.
    Move = 0,
    /// A line to one point.
    Line = 1,
    /// A quadratic Bezier: a control point and the end.
    Quad = 2,
    /// A cubic Bezier: two control points and the end.
    Cubic = 3,
    /// Closes the subpath (back to its start); takes no point.
    Close = 4,
}

impl PathVerb {
    pub fn from_u8(v: u8) -> Option<PathVerb> {
        Some(match v {
            0 => PathVerb::Move,
            1 => PathVerb::Line,
            2 => PathVerb::Quad,
            3 => PathVerb::Cubic,
            4 => PathVerb::Close,
            _ => return None,
        })
    }

    /// How many points the verb takes.
    pub fn points(self) -> usize {
        match self {
            PathVerb::Move | PathVerb::Line => 1,
            PathVerb::Quad => 2,
            PathVerb::Cubic => 3,
            PathVerb::Close => 0,
        }
    }
}

/// One element of a path given to [`DisplayListBuilder::add_path`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathEl {
    MoveTo(Point),
    LineTo(Point),
    QuadTo(Point, Point),
    CurveTo(Point, Point, Point),
    Close,
}

/// How a stroke's open ends are drawn (SVG 2 13.5.3 `stroke-linecap`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum LineCap {
    #[default]
    Butt = 0,
    Round = 1,
    Square = 2,
}

/// How a stroke's corners are drawn (SVG 2 13.5.4 `stroke-linejoin`; a miter longer than the
/// limit is a bevel).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum LineJoin {
    #[default]
    Miter = 0,
    Round = 1,
    Bevel = 2,
}

/// A range of dash lengths in [`PathData::dashes`] (empty: a solid stroke).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DashRange {
    pub start: u32,
    pub len: u32,
}

/// The shape of a path's stroke (SVG 2 13.5), in the path's coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stroke {
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f32,
    /// Dash and gap lengths, alternating, an even number of them (empty: solid).
    pub dashes: DashRange,
    pub dash_offset: f32,
}

impl Default for Stroke {
    fn default() -> Stroke {
        Stroke {
            width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 4.0,
            dashes: DashRange::default(),
            dash_offset: 0.0,
        }
    }
}

/// CSS `border-style` values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum BorderStyle {
    #[default]
    None = 0,
    Hidden = 1,
    Solid = 2,
    Dashed = 3,
    Dotted = 4,
    Double = 5,
    Groove = 6,
    Ridge = 7,
    Inset = 8,
    Outset = 9,
}

impl BorderStyle {
    /// Whether a side with this style paints nothing.
    pub fn is_invisible(self) -> bool {
        matches!(self, BorderStyle::None | BorderStyle::Hidden)
    }

    fn from_u8(v: u8) -> Option<BorderStyle> {
        use BorderStyle::*;
        Some(match v {
            0 => None,
            1 => Hidden,
            2 => Solid,
            3 => Dashed,
            4 => Dotted,
            5 => Double,
            6 => Groove,
            7 => Ridge,
            8 => Inset,
            9 => Outset,
            _ => return Option::None,
        })
    }
}

/// How an image is resampled when drawn at another size (CSS `image-rendering`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum ImageRendering {
    #[default]
    Smooth = 0,
    Pixelated = 1,
}

/// What a gradient does past its last stop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Extend {
    #[default]
    Clamp = 0,
    /// `repeating-linear-gradient` / `repeating-radial-gradient`.
    Repeat = 1,
}

/// A gradient color stop; `offset` is 0..=1 along the gradient line (or ray).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GradientStop {
    pub offset: f32,
    pub color: Color,
}

/// A box's border: `rect` is the border box, `radii` its outer corner radii, sides in CSS order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Border {
    pub rect: Rect,
    pub radii: Radii,
    pub widths: Sides,
    /// top, right, bottom, left
    pub colors: [Color; 4],
    /// top, right, bottom, left
    pub styles: [BorderStyle; 4],
}

/// A blurred box shadow. For an outer shadow, `rect`/`radii` are the border box and the shadow is
/// painted only outside it; for `inset`, they are the padding box and it is painted only inside.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxShadow {
    pub rect: Rect,
    pub radii: Radii,
    pub offset: Point,
    /// The CSS blur radius (the Gaussian's standard deviation is half of it).
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
    pub inset: bool,
}

/// A run of glyphs from one font face at one size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextRun {
    pub glyphs: GlyphRange,
    /// The left end of the run's baseline. For a right-to-left run (odd `bidi_level`) too: glyphs are
    /// stored in logical order and the rasterizer places them leftwards from the right end.
    pub origin: Point,
    pub font: FontId,
    /// The em size in CSS px.
    pub size: f32,
    pub color: Color,
    pub bidi_level: u8,
    /// The glyphs are set for vertical layout (DirectWrite's `isSideways`): turned a quarter turn
    /// left and placed by their vertical metrics, the run being drawn under a quarter turn right.
    pub sideways: bool,
    /// Above 0, the glyphs' outlines are stroked with `color` at this width (CSS px, centered on
    /// the outline: Compat §5.5 `-webkit-text-stroke`) instead of filled.
    pub stroke: f32,
}

/// How a layer of a display list (see [`Item::PushLayer`]) moves when the viewport scrolls, so that a
/// compositor can place it at any scroll position without painting it again.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum LayerKind {
    /// `position: fixed` content attached to the viewport: it stays where it is painted while the
    /// viewport scrolls.
    Fixed,
    /// `position: sticky` content whose scrollport is the viewport: it scrolls with the document
    /// and is shifted per [`StickyConstraint::offset`] at each scroll position.
    Sticky(StickyConstraint),
    /// Content whose position depends on the viewport's scroll position in a way no layer
    /// describes (a sticky box inside a clip or a group): the whole list must be painted again for
    /// each scroll position.
    Pinned,
    /// A box with a direct compositing reason (a running transform or opacity animation,
    /// `will-change: transform | opacity`; Blink's `CompositingReasonFinder`), keyed by its element:
    /// its stacking context, inside the box's own transform and opacity groups. A compositor draws it
    /// as a piece of its own through the clips, transforms and opacities around it
    /// ([`LayerState`]), so that a frame that changes only those repaints nothing. It scrolls with
    /// the document.
    Composited(u64),
}

/// One step of the property state a piece of a split list is drawn through ([`SplitLayer::state`]),
/// in the list's coordinates as the groups it was split from apply them: what Blink's paint chunks
/// carry as their transform, clip and effect nodes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum StateOp {
    /// A transform group's matrix (`m11, m12, m21, m22, dx, dy`): applies to what follows.
    Transform([f32; 6]),
    /// A rectangle clip, in the coordinates of the transforms before it.
    Clip(Rect),
    /// A rounded rectangle clip: the rectangle and the corners' radii (horizontal and vertical,
    /// top left, top right, bottom right, bottom left).
    ClipRounded(Rect, [f32; 8]),
    /// A group opacity (0 to 1).
    Opacity(f32),
    /// A filter of color functions only (Filter Effects 1 §10), applied to each pixel of what
    /// follows: a box's own filter whose animation a compositor may run.
    Filter(ColorFilters),
    /// Draws nothing: marks the box with this key ([`LayerKind::Composited`]) whose transform,
    /// opacity or filter animations a compositor runs ([`crate::layer_animation::LayerAnimation`]);
    /// the steps before it are that box's own transform, opacity and, if it has one there, filter,
    /// which the compositor writes at each display frame.
    Animated(u64),
}

/// What a piece of a split list is drawn through: the clips, transforms and opacities it sat under,
/// outermost first. Empty for content drawn as painted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LayerState(pub Vec<StateOp>);

impl LayerState {
    /// The most steps a state may have (deeper nesting is drawn with the list, not split off).
    pub const MAX_OPS: usize = 64;

    /// Whether every number is finite, opacities and radii in range, and the state not too deep.
    pub fn is_valid(&self) -> bool {
        self.0.len() <= LayerState::MAX_OPS
            && self.0.iter().all(|op| match op {
                StateOp::Transform(m) => m.iter().all(|v| v.is_finite()),
                StateOp::Clip(r) => rect_finite(r),
                StateOp::ClipRounded(r, radii) => {
                    rect_finite(r) && radii.iter().all(|v| v.is_finite() && *v >= 0.0)
                }
                StateOp::Opacity(o) => (0.0..=1.0).contains(o),
                StateOp::Filter(f) => f.is_valid(),
                StateOp::Animated(_) => true,
            })
    }

    /// The matrix from the piece's coordinates to the list's: every transform, innermost first.
    pub fn transform(&self) -> Matrix {
        self.0
            .iter()
            .rev()
            .fold(Matrix::IDENTITY, |m, op| match op {
                StateOp::Transform(t) => m.then(&matrix_of(t)),
                _ => m,
            })
    }

    /// The product of the opacities.
    pub fn opacity(&self) -> f32 {
        self.0.iter().fold(1.0, |o, op| match op {
            StateOp::Opacity(v) => o * v,
            _ => o,
        })
    }
}

/// A [`StateOp::Transform`]'s matrix.
pub fn matrix_of(t: &[f32; 6]) -> Matrix {
    Matrix::new(t[0], t[1], t[2], t[3], t[4], t[5])
}

/// A [`StateOp::ClipRounded`]'s radii.
pub fn radii_of(r: &[f32; 8]) -> Radii {
    Radii {
        top_left: Size::new(r[0], r[1]),
        top_right: Size::new(r[2], r[3]),
        bottom_right: Size::new(r[4], r[5]),
        bottom_left: Size::new(r[6], r[7]),
    }
}

/// The step a group's push item adds to a piece's state, if it can be one.
/// Whether `items` show anything: a drawing item outside every opacity group at 0.
fn shows_anything(items: &[Item]) -> bool {
    // (per open opacity group: whether it is at 0)
    let mut open: Vec<bool> = Vec::new();
    let mut hidden = 0usize;
    for item in items {
        match *item {
            Item::PushOpacity { opacity, .. } => {
                let zero = opacity <= 0.0;
                hidden += usize::from(zero);
                open.push(zero);
            }
            Item::PopOpacity => {
                if open.pop() == Some(true) {
                    hidden -= 1;
                }
            }
            _ if hidden == 0 && item.is_drawing() => return true,
            _ => {}
        }
    }
    false
}

fn state_op(list: &DisplayList, item: &Item) -> Option<StateOp> {
    Some(match *item {
        Item::PushTransform { matrix: m } => {
            StateOp::Transform([m.m11, m.m12, m.m21, m.m22, m.dx, m.dy])
        }
        Item::PushClipRect { rect } => StateOp::Clip(rect),
        Item::PushClipRoundedRect { rect, radii } => {
            let [a, b, c, d] = radii.corners();
            StateOp::ClipRounded(
                rect,
                [
                    a.width, a.height, b.width, b.height, c.width, c.height, d.width, d.height,
                ],
            )
        }
        Item::PushOpacity { opacity, .. } => StateOp::Opacity(opacity),
        Item::PushFilter { filters, .. } => {
            StateOp::Filter(ColorFilters::of(list.filter(filters))?)
        }
        _ => return None,
    })
}

/// A sticky box's constraints (CSS Positioned Layout 3 §3.4), in the list's coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StickyConstraint {
    /// The box's border box where it would be without the sticky shift.
    pub rect: Rect,
    /// The rectangle the box stays inside (its containing block).
    pub container: Rect,
    /// The used `top`, `right`, `bottom` and `left` insets; `None` for `auto`.
    pub insets: [Option<f32>; 4],
}

impl LayerKind {
    /// Whether every coordinate is finite (what [`DisplayList::validate`] asks of a layer's kind).
    pub fn is_valid(&self) -> bool {
        match self {
            LayerKind::Sticky(c) => {
                rect_finite(&c.rect)
                    && rect_finite(&c.container)
                    && c.insets.iter().flatten().all(|v| v.is_finite())
            }
            LayerKind::Fixed | LayerKind::Pinned | LayerKind::Composited(_) => true,
        }
    }
}

impl StickyConstraint {
    /// Whether the box can be shifted at all: each side with an inset needs room between the box
    /// and its container's edge on the far side ([`StickyConstraint::offset`] never shifts a box
    /// out of its container). One that fills its container is never shifted, at any scroll
    /// position.
    pub fn moves(&self) -> bool {
        let (r, c) = (self.rect, self.container);
        let [top, right, bottom, left] = self.insets;
        (top.is_some() && c.y + c.height > r.y + r.height)
            || (bottom.is_some() && r.y > c.y)
            || (left.is_some() && c.x + c.width > r.x + r.width)
            || (right.is_some() && r.x > c.x)
    }

    /// How far the box is shifted when the scrollport is `view` (list coordinates): each side with
    /// an inset keeps the box's edge that far inside the scrollport's edge, without moving the box
    /// out of its container; `top` wins over `bottom` and `left` over `right`.
    pub fn offset(&self, view: Rect) -> Point {
        let r = self.rect;
        let c = self.container;
        let [top, right, bottom, left] = self.insets;
        let mut shift = Point::ZERO;
        if let Some(b) = bottom {
            let max_bottom = view.y + view.height - b;
            if r.y + r.height > max_bottom {
                let room = (r.y - c.y).max(0.0);
                shift.y = -(r.y + r.height - max_bottom).min(room);
            }
        }
        if let Some(t) = top {
            let min_top = view.y + t;
            if r.y + shift.y < min_top {
                let room = (c.y + c.height - (r.y + r.height)).max(0.0);
                shift.y = (min_top - r.y).min(room).max(shift.y);
            }
        }
        if let Some(rt) = right {
            let max_right = view.x + view.width - rt;
            if r.x + r.width > max_right {
                let room = (r.x - c.x).max(0.0);
                shift.x = -(r.x + r.width - max_right).min(room);
            }
        }
        if let Some(l) = left {
            let min_left = view.x + l;
            if r.x + shift.x < min_left {
                let room = (c.x + c.width - (r.x + r.width)).max(0.0);
                shift.x = (min_left - r.x).min(room).max(shift.x);
            }
        }
        shift
    }
}

/// How a mask layer is composited onto the layers below it ([`Item::MaskLayer`]): a Porter-Duff
/// operator (Compositing 1 §9.1), the layer the source and the layers below the destination.
/// `mask-composite`'s `add`, `subtract`, `intersect` and `exclude` (CSS Masking 1 §7.7) are
/// `SourceOver`, `SourceOut`, `SourceIn` and `Xor`; the others are the legacy
/// `-webkit-mask-composite` operators (`Plus` is `plus-lighter`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MaskComposite {
    #[default]
    SourceOver,
    SourceIn,
    SourceOut,
    SourceAtop,
    DestinationOver,
    DestinationIn,
    DestinationOut,
    DestinationAtop,
    Xor,
    Plus,
    Copy,
    Clear,
}

impl MaskComposite {
    const ALL: [MaskComposite; 12] = [
        MaskComposite::SourceOver,
        MaskComposite::SourceIn,
        MaskComposite::SourceOut,
        MaskComposite::SourceAtop,
        MaskComposite::DestinationOver,
        MaskComposite::DestinationIn,
        MaskComposite::DestinationOut,
        MaskComposite::DestinationAtop,
        MaskComposite::Xor,
        MaskComposite::Plus,
        MaskComposite::Copy,
        MaskComposite::Clear,
    ];
}

/// One display item.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Item {
    FillRect {
        rect: Rect,
        color: Color,
    },
    FillRoundedRect {
        rect: Rect,
        radii: Radii,
        color: Color,
    },
    /// A stroke centered on the rectangle's edges.
    StrokeRect {
        rect: Rect,
        width: f32,
        color: Color,
    },
    StrokeRoundedRect {
        rect: Rect,
        radii: Radii,
        width: f32,
        color: Color,
    },
    Border(Border),
    Text(TextRun),
    Image {
        image: ImageId,
        dest: Rect,
        /// The part of the image (in image pixels) to draw; an empty rect means all of it.
        src: Rect,
        opacity: f32,
        rendering: ImageRendering,
    },
    /// A video's current frame drawn into `dest` (its `object-fit` rectangle), clipped to `clip` (the
    /// element's content box). A rasterizer draws `image` (the frame copied to memory, or nothing:
    /// black); a compositor that has the video's frames on the GPU draws the latest one over it, at
    /// the display's rate (see `snowlark-paint`'s `video`).
    ///
    /// A `transparent` one is a canvas's frames (premultiplied, drawn over what is under them):
    /// without `image` a rasterizer draws nothing in its place, and a compositor draws it as a quad
    /// only where it composites as a plain source-over draw ([`DisplayList::video_quad_ok`]).
    Video {
        video: VideoId,
        image: Option<ImageId>,
        dest: Rect,
        clip: Rect,
        transparent: bool,
    },
    /// The picture of a protected player of the embedder's key system (`player`: the page's id
    /// of it) in `dest`, clipped to `clip`, as [`Item::Video`]. Its frames never reach the page or
    /// the rasterizer, which draws black; a compositor shows the player's own surface over it.
    ProtectedVideo {
        player: u64,
        dest: Rect,
        clip: Rect,
    },
    /// Fills `rect` (rounded by `radii`) with a gradient along the line `start` → `end`.
    LinearGradient {
        rect: Rect,
        radii: Radii,
        start: Point,
        end: Point,
        stops: StopRange,
        extend: Extend,
    },
    /// Fills `rect` (rounded by `radii`) with an elliptical gradient.
    RadialGradient {
        rect: Rect,
        radii: Radii,
        center: Point,
        radius: Size,
        stops: StopRange,
        extend: Extend,
    },
    /// Fills `rect` (rounded by `radii`) with a conic gradient around `center` (CSS Images 4
    /// §3.3): stop offsets are fractions of a turn clockwise from `angle` (radians from up);
    /// `Repeat` repeats the stops' first-to-last span around the turn.
    ConicGradient {
        rect: Rect,
        radii: Radii,
        center: Point,
        angle: f32,
        stops: StopRange,
        extend: Extend,
    },
    BoxShadow(BoxShadow),
    /// What is drawn up to the matching pop is added to what is below it (premultiplied: Compositing
    /// 1 `plus-lighter`) instead of composited over it: `cross-fade()`'s sum.
    PushPlus,
    PopPlus,
    PushClipRect {
        rect: Rect,
    },
    PushClipRoundedRect {
        rect: Rect,
        radii: Radii,
    },
    /// Clips what follows, up to the matching [`Item::PopClip`], to a path's fill by the nonzero
    /// rule, or `evenodd`, antialiased (`clip-path`'s shapes, CSS Masking 1 §5). `bounds` (local
    /// coordinates) contain the path: nothing shows outside them (Blink's clip node of a
    /// path-based clip-path holds the path and this rectangle).
    PushClipPath {
        path: PathRange,
        bounds: Rect,
        evenodd: bool,
    },
    PopClip,
    /// Multiplies `matrix` onto the current transform (it applies first, in the pushed coordinates).
    PushTransform {
        matrix: Matrix,
    },
    PopTransform,
    /// Paints what follows, up to the matching pop, into a group composited with `opacity`. `bounds`
    /// (local coordinates) bound the group's content; it limits the intermediate surface.
    PushOpacity {
        opacity: f32,
        bounds: Rect,
    },
    PopOpacity,
    /// Paints what follows, up to the matching [`Item::PopFilter`], into a group whose content goes
    /// through `filters` (Filter Effects 1 §10: the functions in order, the first on the content)
    /// and shows their result. `bounds` (local coordinates) is where the result can show (the
    /// content's bounds mapped through the functions: [`crate::filter::map_rect`]); it limits the
    /// intermediate surface.
    PushFilter {
        filters: FilterRange,
        bounds: Rect,
    },
    PopFilter,
    /// A backdrop filter (Filter Effects 2 §3): what was painted since the backdrop root, under
    /// `rect` (local coordinates: the element's border box), cropped to the rectangle with its
    /// edges mirrored, put through `filters` (the first applied first) and drawn clipped to `rect`
    /// with `radii`. `own` is how many of the groups open around the item are the element's own
    /// (its opacity and mask groups, which apply to the filtered backdrop too): the backdrop root
    /// is the nearest one outside them, or the list's start.
    BackdropFilter {
        filters: FilterRange,
        rect: Rect,
        radii: Radii,
        own: u8,
    },
    /// What follows, up to the matching pop, is content a compositor may draw as a layer of its own
    /// (see [`DisplayList::split_layers`]). Painting the list as a whole ignores it: the content is
    /// already where it shows at the scroll position the list was painted for.
    PushLayer {
        kind: LayerKind,
    },
    PopLayer,
    /// A mask group (CSS Masking 1 §7): the items up to [`Item::MaskContent`] paint the mask, whose
    /// alpha (or with `luminance`, its luminance) is the coverage of what follows up to
    /// [`Item::PopMask`]. `bounds` (local coordinates) bound the masked content: outside it nothing
    /// shows.
    PushMask {
        bounds: Rect,
        luminance: bool,
    },
    /// Between a mask's items (directly in them, not in a group of theirs): the items before it,
    /// since the mask's start or the previous one, are one mask layer, and those after it, up to
    /// the next one or [`Item::MaskContent`], the next layer (CSS Masking 1 §7.7): composited onto
    /// the layers below by `composite`, its alpha (or with `luminance`, its luminance) its coverage.
    /// The mask's first layer takes the mode of [`Item::PushMask`].
    MaskLayer {
        composite: MaskComposite,
        luminance: bool,
    },
    /// Ends a mask's items: what follows is the masked content.
    MaskContent,
    PopMask,
    /// Fills a path (SVG 2 13.4: open subpaths as if closed) by the nonzero rule, or `evenodd`.
    /// `bounds` (local coordinates) contain the fill; `antialias` false draws whole pixels
    /// (`shape-rendering: crispEdges`).
    FillPath {
        path: PathRange,
        bounds: Rect,
        color: Color,
        evenodd: bool,
        antialias: bool,
    },
    /// Strokes a path; `bounds` (local coordinates) contain the stroke.
    StrokePath {
        path: PathRange,
        bounds: Rect,
        color: Color,
        stroke: Stroke,
        antialias: bool,
    },
    /// Fills `rect` (local coordinates) with a pattern (SVG 2 §14.3): the list of
    /// [`DisplayList::patterns`]`[pattern]` drawn in its tile, `(0, 0)` to its size in the tile's own
    /// space and clipped to it, repeated in both directions; `transform` maps the tile's space to
    /// local coordinates.
    FillPattern {
        rect: Rect,
        pattern: u32,
        transform: Matrix,
    },
}

/// A pattern's tile ([`Item::FillPattern`]): what is drawn in it, in its own space, and its size.
/// Its list holds no patterns of its own, no layers, videos or backdrop filters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pattern {
    pub tile: Size,
    pub list: DisplayList,
}

impl Item {
    /// Whether the item draws (as opposed to a push or pop).
    /// A video's picture ([`Item::Video`] or [`Item::ProtectedVideo`]), which a compositor shows
    /// over the tiles.
    pub fn is_video(&self) -> bool {
        matches!(self, Item::Video { .. } | Item::ProtectedVideo { .. })
    }

    pub fn is_drawing(&self) -> bool {
        !matches!(
            self,
            Item::PushClipRect { .. }
                | Item::PushClipRoundedRect { .. }
                | Item::PushClipPath { .. }
                | Item::PopClip
                | Item::PushTransform { .. }
                | Item::PopTransform
                | Item::PushOpacity { .. }
                | Item::PopOpacity
                | Item::PushFilter { .. }
                | Item::PopFilter
                | Item::PushLayer { .. }
                | Item::PopLayer
                | Item::PushMask { .. }
                | Item::MaskLayer { .. }
                | Item::MaskContent
                | Item::PopMask
                | Item::PushPlus
                | Item::PopPlus
        )
    }
}

impl Item {
    /// Whether the item's bounds ([`DisplayList::item_bounds`]) are an area it can paint: a
    /// drawing, or a filter group, whose result reaches past its content (a blur's or a shadow's
    /// spread, Filter Effects 1 §5).
    fn covers_area(&self) -> bool {
        self.is_drawing() || matches!(self, Item::PushFilter { .. })
    }
}

/// The kinds of nesting group, for checking that pushes and pops match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Clip,
    Transform,
    Opacity,
    /// A filter group ([`Item::PushFilter`]).
    Filter,
    Layer,
    /// A mask's items ([`Item::PushMask`] up to [`Item::MaskContent`]).
    Mask,
    /// A mask's content (up to [`Item::PopMask`]).
    MaskBody,
    Plus,
}

/// A finished display list. The fields are public for the rasterizer and for tests; build lists with
/// [`DisplayListBuilder`], which keeps them consistent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DisplayList {
    pub items: Vec<Item>,
    pub glyph_ids: Vec<u16>,
    pub glyph_advances: Vec<f32>,
    pub glyph_offsets: Vec<GlyphOffset>,
    pub stops: Vec<GradientStop>,
    /// What path items refer to (boxed: most lists have none, and lists travel in messages).
    pub paths: Box<PathData>,
    /// The filter functions of the filter groups (see [`FilterRange`]).
    pub filters: Vec<FilterOp>,
    /// The tiles of the pattern fills ([`Item::FillPattern`]).
    pub patterns: Vec<Pattern>,
    /// The transform and opacity animations of its composited boxes that a compositor runs
    /// ([`crate::layer_animation`]). Not part of the list's items or its encoding: whoever hands the
    /// list's pieces to a compositor sends them beside it.
    pub animations: Vec<crate::layer_animation::LayerAnimation>,
}

/// The side vectors of a list's paths and strokes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathData {
    /// The segment kinds ([`PathVerb`] values) and points (see [`PathRange`]).
    pub verbs: Vec<u8>,
    pub points: Vec<Point>,
    /// The strokes' dash lengths (see [`DashRange`]).
    pub dashes: Vec<f32>,
}

/// Why a list is malformed (from [`DisplayList::validate`] or [`DisplayList::decode`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListError {
    /// The bytes end in the middle of an item.
    Truncated,
    /// An unknown item tag or enum value at this byte offset.
    BadTag(usize),
    /// Item `n` has a NaN or infinite coordinate, or an opacity outside 0..=1.
    BadValue(usize),
    /// Item `n` refers to glyphs or stops outside the side vectors.
    BadRange(usize),
    /// Item `n` pops a group that isn't open, or a different kind of group.
    Unbalanced(usize),
    /// Groups are still open at the end.
    Unclosed,
    /// The header is not a display list of this version.
    BadHeader,
    /// More bytes or a larger frame than a raster side takes.
    TooLarge,
}

impl std::fmt::Display for ListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed display list: {self:?}")
    }
}

impl std::error::Error for ListError {}

impl DisplayList {
    pub fn new() -> DisplayList {
        DisplayList::default()
    }

    /// For each video item ([`Item::is_video`]), its index and whether a compositor can draw it as
    /// a quad over the tiles: only under transforms that keep it axis-aligned, and a transparent
    /// [`Item::Video`] (a canvas's frames) also only outside the groups that change how it
    /// composites with what is under it (opacity, filters, masks, plus-lighter) and outside clips
    /// that are not rectangles, all of which a quad drawn over the tiles would skip.
    pub fn video_quads(&self) -> Vec<(usize, bool)> {
        let mut out = Vec::new();
        if !self.items.iter().any(Item::is_video) {
            return out;
        }
        let mut transforms = vec![Matrix::IDENTITY];
        // (per clip and group pushed: whether it is one a transparent quad cannot show)
        let mut clips: Vec<bool> = Vec::new();
        let mut groups = 0u32;
        for (i, item) in self.items.iter().enumerate() {
            match item {
                Item::PushClipRect { .. } => clips.push(false),
                Item::PushClipRoundedRect { .. } | Item::PushClipPath { .. } => clips.push(true),
                Item::PopClip => {
                    clips.pop();
                }
                Item::PushTransform { matrix } => {
                    let m = matrix.then(transforms.last().unwrap());
                    transforms.push(m);
                }
                Item::PopTransform => {
                    transforms.pop();
                }
                Item::PushOpacity { .. }
                | Item::PushFilter { .. }
                | Item::PushMask { .. }
                | Item::PushPlus => groups += 1,
                Item::PopOpacity | Item::PopFilter | Item::PopMask | Item::PopPlus => {
                    groups = groups.saturating_sub(1)
                }
                Item::Video { .. } | Item::ProtectedVideo { .. } => {
                    let aligned = transforms.last().unwrap().is_axis_aligned();
                    let plain = groups == 0 && !clips.iter().any(|&c| c);
                    let transparent = matches!(
                        item,
                        Item::Video {
                            transparent: true,
                            ..
                        }
                    );
                    out.push((i, aligned && (plain || !transparent)));
                }
                _ => {}
            }
        }
        out
    }

    /// Empties the list, keeping the vectors' capacity.
    pub fn clear(&mut self) {
        self.items.clear();
        self.glyph_ids.clear();
        self.glyph_advances.clear();
        self.glyph_offsets.clear();
        self.stops.clear();
        self.paths.verbs.clear();
        self.paths.points.clear();
        self.paths.dashes.clear();
        self.filters.clear();
        self.patterns.clear();
        self.animations.clear();
    }

    /// The functions of a filter group (empty for a range outside the list).
    pub fn filter(&self, r: FilterRange) -> &[FilterOp] {
        let start = r.start as usize;
        match start.checked_add(r.len as usize) {
            Some(end) if end <= self.filters.len() => &self.filters[start..end],
            _ => &[],
        }
    }

    fn filter_ok(&self, r: FilterRange) -> bool {
        (r.start as usize)
            .checked_add(r.len as usize)
            .is_some_and(|end| end <= self.filters.len())
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The glyph ids, advances and offsets of a run.
    pub fn glyphs(&self, r: GlyphRange) -> (&[u16], &[f32], &[GlyphOffset]) {
        let a = r.start as usize;
        let b = a + r.len as usize;
        (
            &self.glyph_ids[a..b],
            &self.glyph_advances[a..b],
            &self.glyph_offsets[a..b],
        )
    }

    pub fn stops(&self, r: StopRange) -> &[GradientStop] {
        &self.stops[r.start as usize..(r.start + r.len) as usize]
    }

    /// A path's verbs and points.
    pub fn path(&self, r: PathRange) -> (&[u8], &[Point]) {
        (
            &self.paths.verbs[r.verbs as usize..(r.verbs + r.verbs_len) as usize],
            &self.paths.points[r.points as usize..(r.points + r.points_len) as usize],
        )
    }

    /// The rectangle a path is, when it is one closed rectangle along the axes (corners in order;
    /// its last line back to the start may be left to the close): a clip by it can be a rectangle
    /// clip, antialiased and not snapped to pixels.
    pub fn path_rect(&self, r: PathRange) -> Option<Rect> {
        let (verbs, points) = self.path(r);
        let mut corners: [Point; 5] = [Point::ZERO; 5];
        let mut n = 0;
        for (i, &v) in verbs.iter().enumerate() {
            match PathVerb::from_u8(v)? {
                PathVerb::Move if i == 0 => {}
                PathVerb::Line if i > 0 && n < 5 => {}
                PathVerb::Close if i == verbs.len() - 1 => break,
                _ => return None,
            }
            corners[n] = *points.get(n)?;
            n += 1;
        }
        if n == 5 && corners[4] == corners[0] {
            n = 4;
        }
        if n != 4
            || points.len()
                != verbs
                    .iter()
                    .filter(|&&v| v != PathVerb::Close as u8)
                    .count()
        {
            return None;
        }
        // (each side along an axis, alternating: a rectangle, not a bow tie)
        for i in 0..4 {
            let (a, b, c) = (corners[i], corners[(i + 1) % 4], corners[(i + 2) % 4]);
            let first_horizontal = a.y == b.y && a.x != b.x;
            let first_vertical = a.x == b.x && a.y != b.y;
            let then_vertical = b.x == c.x && b.y != c.y;
            let then_horizontal = b.y == c.y && b.x != c.x;
            if !((first_horizontal && then_vertical) || (first_vertical && then_horizontal)) {
                return None;
            }
        }
        let (a, c) = (corners[0], corners[2]);
        Some(Rect::new(
            a.x.min(c.x),
            a.y.min(c.y),
            (a.x - c.x).abs(),
            (a.y - c.y).abs(),
        ))
    }

    /// A stroke's dash lengths.
    pub fn dashes(&self, r: DashRange) -> &[f32] {
        &self.paths.dashes[r.start as usize..(r.start + r.len) as usize]
    }

    /// Whether a path range is inside the side vectors and its verbs take exactly its points.
    pub fn path_ok(&self, r: PathRange) -> bool {
        let verbs_end = r.verbs as u64 + r.verbs_len as u64;
        let points_end = r.points as u64 + r.points_len as u64;
        if verbs_end > self.paths.verbs.len() as u64 || points_end > self.paths.points.len() as u64
        {
            return false;
        }
        let mut need = 0usize;
        for &v in &self.paths.verbs[r.verbs as usize..verbs_end as usize] {
            match PathVerb::from_u8(v) {
                Some(v) => need += v.points(),
                None => return false,
            }
        }
        need == r.points_len as usize
    }

    /// The total advance of a text run.
    pub fn run_width(&self, run: &TextRun) -> f32 {
        self.glyphs(run.glyphs).1.iter().sum()
    }

    /// Checks ranges, values and nesting. Lists from [`DisplayListBuilder`] always pass.
    pub fn validate(&self) -> Result<(), ListError> {
        let n = self.glyph_ids.len();
        if self.glyph_advances.len() != n || self.glyph_offsets.len() != n {
            return Err(ListError::BadRange(0));
        }
        let mut stack: Vec<Group> = Vec::new();
        let glyphs_ok = |r: GlyphRange| (r.start as u64 + r.len as u64) <= n as u64;
        let stops_ok =
            |r: StopRange| r.len >= 1 && (r.start as u64 + r.len as u64) <= self.stops.len() as u64;
        for (i, item) in self.items.iter().enumerate() {
            if !item_finite(item) {
                return Err(ListError::BadValue(i));
            }
            let pop = |stack: &mut Vec<Group>, g: Group| {
                if stack.pop() == Some(g) {
                    Ok(())
                } else {
                    Err(ListError::Unbalanced(i))
                }
            };
            match item {
                Item::Text(t) => {
                    if !glyphs_ok(t.glyphs) {
                        return Err(ListError::BadRange(i));
                    }
                    if t.bidi_level > 125 {
                        return Err(ListError::BadValue(i));
                    }
                }
                Item::LinearGradient { stops, .. }
                | Item::RadialGradient { stops, .. }
                | Item::ConicGradient { stops, .. } => {
                    if !stops_ok(*stops) {
                        return Err(ListError::BadRange(i));
                    }
                }
                Item::PushPlus => stack.push(Group::Plus),
                Item::PopPlus => pop(&mut stack, Group::Plus)?,
                Item::Image { opacity, .. } | Item::PushOpacity { opacity, .. } => {
                    if !(0.0..=1.0).contains(opacity) {
                        return Err(ListError::BadValue(i));
                    }
                    if matches!(item, Item::PushOpacity { .. }) {
                        stack.push(Group::Opacity);
                    }
                }
                Item::PushClipRect { .. } | Item::PushClipRoundedRect { .. } => {
                    stack.push(Group::Clip)
                }
                Item::PushClipPath { path, .. } => {
                    if !self.path_ok(*path) {
                        return Err(ListError::BadRange(i));
                    }
                    stack.push(Group::Clip)
                }
                Item::PushTransform { .. } => stack.push(Group::Transform),
                Item::PushFilter { filters, .. } => {
                    if !self.filter_ok(*filters) {
                        return Err(ListError::BadRange(i));
                    }
                    if !self.filter(*filters).iter().all(FilterOp::is_valid) {
                        return Err(ListError::BadValue(i));
                    }
                    stack.push(Group::Filter)
                }
                Item::BackdropFilter { filters, .. } => {
                    if !self.filter_ok(*filters) {
                        return Err(ListError::BadRange(i));
                    }
                    if !self.filter(*filters).iter().all(FilterOp::is_valid) {
                        return Err(ListError::BadValue(i));
                    }
                }
                Item::PopClip => pop(&mut stack, Group::Clip)?,
                Item::PopTransform => pop(&mut stack, Group::Transform)?,
                Item::PopOpacity => pop(&mut stack, Group::Opacity)?,
                Item::PopFilter => pop(&mut stack, Group::Filter)?,
                Item::PushLayer { .. } => stack.push(Group::Layer),
                Item::PopLayer => pop(&mut stack, Group::Layer)?,
                Item::PushMask { .. } => stack.push(Group::Mask),
                Item::MaskLayer { .. } => {
                    if stack.last() != Some(&Group::Mask) {
                        return Err(ListError::Unbalanced(i));
                    }
                }
                Item::MaskContent => {
                    pop(&mut stack, Group::Mask)?;
                    stack.push(Group::MaskBody);
                }
                Item::PopMask => pop(&mut stack, Group::MaskBody)?,
                Item::FillPath { path, .. } => {
                    if !self.path_ok(*path) {
                        return Err(ListError::BadRange(i));
                    }
                }
                Item::StrokePath { path, stroke, .. } => {
                    let dashes_ok = (stroke.dashes.start as u64 + stroke.dashes.len as u64)
                        <= self.paths.dashes.len() as u64
                        && stroke.dashes.len % 2 == 0;
                    if !self.path_ok(*path) || !dashes_ok {
                        return Err(ListError::BadRange(i));
                    }
                    if stroke.width < 0.0 || stroke.miter_limit < 0.0 {
                        return Err(ListError::BadValue(i));
                    }
                }
                Item::FillPattern { pattern, .. } if *pattern as usize >= self.patterns.len() => {
                    return Err(ListError::BadRange(i));
                }
                _ => {}
            }
        }
        for p in &self.patterns {
            let size_ok = |v: f32| v.is_finite() && v > 0.0;
            if !size_ok(p.tile.width) || !size_ok(p.tile.height) {
                return Err(ListError::BadValue(0));
            }
            // (a tile is painted on its own: nothing in it reaches out of it)
            let inside_only = p.list.patterns.is_empty()
                && !p.list.items.iter().any(|it| {
                    it.is_video()
                        || matches!(it, Item::PushLayer { .. } | Item::BackdropFilter { .. })
                });
            if !inside_only {
                return Err(ListError::BadValue(0));
            }
            p.list.validate()?;
        }
        if self
            .paths
            .points
            .iter()
            .any(|p| !p.x.is_finite() || !p.y.is_finite())
            || self.paths.dashes.iter().any(|d| !d.is_finite() || *d < 0.0)
        {
            return Err(ListError::BadValue(0));
        }
        for s in &self.stops {
            if !s.offset.is_finite() {
                return Err(ListError::BadValue(0));
            }
        }
        if self.glyph_advances.iter().any(|v| !v.is_finite())
            || self
                .glyph_offsets
                .iter()
                .any(|o| !o.advance_offset.is_finite() || !o.ascender_offset.is_finite())
        {
            return Err(ListError::BadValue(0));
        }
        if stack.is_empty() {
            Ok(())
        } else {
            Err(ListError::Unclosed)
        }
    }

    /// A hash of each item's content (`out` is cleared and reused): what it draws, with the glyphs
    /// and gradient stops it refers to by content (not by their index in this list) and, for an
    /// image, which pixels the store holds for it now (`image_version`: a canvas that drew or an
    /// animation's frame changes them under the same id). Equal hashes mean the item paints the same
    /// (with overwhelming probability); a rasterizer compares them to repaint only what changed.
    pub fn content_hashes(&self, image_version: &dyn Fn(ImageId) -> u64, out: &mut Vec<u64>) {
        out.clear();
        out.reserve(self.items.len());
        let mut scratch = Vec::with_capacity(64);
        for item in &self.items {
            scratch.clear();
            let mut e = Enc(&mut scratch);
            match *item {
                Item::Text(t) => {
                    let mut t2 = t;
                    t2.glyphs.start = 0;
                    e.item(&Item::Text(t2));
                    let r = t.glyphs.start as usize..(t.glyphs.start + t.glyphs.len) as usize;
                    for g in self.glyph_ids.get(r.clone()).unwrap_or(&[]) {
                        e.u16(*g);
                    }
                    for a in self.glyph_advances.get(r.clone()).unwrap_or(&[]) {
                        e.f32(*a);
                    }
                    for o in self.glyph_offsets.get(r).unwrap_or(&[]) {
                        e.f32(o.advance_offset);
                        e.f32(o.ascender_offset);
                    }
                }
                Item::LinearGradient { stops, .. }
                | Item::RadialGradient { stops, .. }
                | Item::ConicGradient { stops, .. } => {
                    let mut i2 = *item;
                    if let Item::LinearGradient { stops: s, .. }
                    | Item::RadialGradient { stops: s, .. }
                    | Item::ConicGradient { stops: s, .. } = &mut i2
                    {
                        s.start = 0;
                    }
                    e.item(&i2);
                    let r = stops.start as usize..(stops.start + stops.len) as usize;
                    for st in self.stops.get(r).unwrap_or(&[]) {
                        e.f32(st.offset);
                        e.color(st.color);
                    }
                }
                Item::Image { image, .. }
                | Item::Video {
                    image: Some(image), ..
                } => {
                    e.item(item);
                    e.bytes(&image_version(image).to_le_bytes());
                }
                Item::FillPath { path, .. }
                | Item::StrokePath { path, .. }
                | Item::PushClipPath { path, .. } => {
                    let mut i2 = *item;
                    let mut dashes = DashRange::default();
                    match &mut i2 {
                        Item::FillPath { path: p, .. } | Item::PushClipPath { path: p, .. } => {
                            *p = PathRange::default()
                        }
                        Item::StrokePath {
                            path: p, stroke, ..
                        } => {
                            *p = PathRange::default();
                            dashes = stroke.dashes;
                            stroke.dashes.start = 0;
                        }
                        _ => {}
                    }
                    e.item(&i2);
                    if self.path_ok(path) {
                        let (verbs, points) = self.path(path);
                        e.bytes(verbs);
                        for p in points {
                            e.point(*p);
                        }
                    }
                    let r = dashes.start as usize..(dashes.start + dashes.len) as usize;
                    for d in self.paths.dashes.get(r).unwrap_or(&[]) {
                        e.f32(*d);
                    }
                }
                Item::PushFilter { filters, bounds } => {
                    e.item(&Item::PushFilter {
                        filters: FilterRange::default(),
                        bounds,
                    });
                    for op in self.filter(filters) {
                        e.filter_op(op);
                        for image in op.images() {
                            e.bytes(&image_version(image).to_le_bytes());
                        }
                    }
                }
                Item::BackdropFilter {
                    filters,
                    rect,
                    radii,
                    own,
                } => {
                    e.item(&Item::BackdropFilter {
                        filters: FilterRange::default(),
                        rect,
                        radii,
                        own,
                    });
                    for op in self.filter(filters) {
                        e.filter_op(op);
                        for image in op.images() {
                            e.bytes(&image_version(image).to_le_bytes());
                        }
                    }
                }
                Item::FillPattern {
                    rect,
                    pattern,
                    transform,
                } => {
                    e.item(&Item::FillPattern {
                        rect,
                        pattern: 0,
                        transform,
                    });
                    // (the tile by its content: its size and its items' hashes)
                    if let Some(p) = self.patterns.get(pattern as usize) {
                        e.f32(p.tile.width);
                        e.f32(p.tile.height);
                        let mut inner = Vec::new();
                        p.list.content_hashes(image_version, &mut inner);
                        for h in inner {
                            e.bytes(&h.to_le_bytes());
                        }
                    }
                }
                _ => e.item(item),
            }
            out.push(hash_bytes(&scratch));
        }
    }

    /// Conservative device-space bounds of every item under `base` (for tile culling): the item's
    /// painted area through the current transform, cut to the current clip. Pushes and pops get an
    /// empty rect. `out` is cleared and reused.
    pub fn item_bounds(&self, base: &Matrix, out: &mut Vec<Rect>) {
        out.clear();
        out.reserve(self.items.len());
        // (transform, clip in device space) per open group
        let mut transforms: Vec<Matrix> = vec![*base];
        let mut clips: Vec<Rect> = vec![Rect::infinite()];
        for item in &self.items {
            let m = *transforms.last().unwrap();
            let clip = *clips.last().unwrap();
            let local = match item {
                Item::FillRect { rect, .. }
                | Item::FillRoundedRect { rect, .. }
                | Item::LinearGradient { rect, .. }
                | Item::RadialGradient { rect, .. }
                | Item::ConicGradient { rect, .. }
                | Item::FillPattern { rect, .. } => Some(*rect),
                Item::Border(b) => Some(b.rect),
                Item::StrokeRect { rect, width, .. }
                | Item::StrokeRoundedRect { rect, width, .. } => {
                    Some(rect.inflate(*width / 2.0, *width / 2.0))
                }
                Item::Image { dest, .. } => Some(*dest),
                Item::FillPath { bounds, .. } | Item::StrokePath { bounds, .. } => Some(*bounds),
                Item::Video { dest, clip, .. } | Item::ProtectedVideo { dest, clip, .. } => {
                    Some(dest.intersect(clip))
                }
                Item::BoxShadow(s) => Some(if s.inset {
                    s.rect
                } else {
                    let grow = s.spread.max(0.0) + s.blur * 1.5 + 1.0;
                    s.rect
                        .translate(s.offset.x, s.offset.y)
                        .inflate(grow, grow)
                        .union(&s.rect)
                }),
                Item::Text(t) => {
                    let w = self.run_width(t);
                    // (glyphs reach above the ascent and below the descent; diacritics, emoji;
                    // a stroke half its width past the outline)
                    let s = t.stroke.max(0.0) / 2.0;
                    Some(Rect::new(
                        t.origin.x - t.size * 0.5 - s,
                        t.origin.y - t.size * 1.5 - s,
                        w + t.size + 2.0 * s,
                        t.size * 2.2 + 2.0 * s,
                    ))
                }
                Item::PushClipRect { rect }
                | Item::PushClipRoundedRect { rect, .. }
                | Item::PushClipPath { bounds: rect, .. } => {
                    clips.push(m.apply_rect(rect).intersect(&clip));
                    None
                }
                Item::PopClip => {
                    clips.pop();
                    None
                }
                Item::PushTransform { matrix } => {
                    transforms.push(matrix.then(&m));
                    None
                }
                Item::PopTransform => {
                    transforms.pop();
                    None
                }
                Item::PushMask { bounds, .. } => {
                    clips.push(m.apply_rect(bounds).intersect(&clip));
                    None
                }
                Item::PopMask => {
                    clips.pop();
                    None
                }
                // (its result shows within its bounds: a change of them repaints that much)
                Item::PushFilter { bounds, .. } => Some(*bounds),
                Item::BackdropFilter { rect, .. } => Some(*rect),
                Item::PushOpacity { .. }
                | Item::PopOpacity
                | Item::PopFilter
                | Item::PushLayer { .. }
                | Item::PopLayer
                | Item::MaskLayer { .. }
                | Item::MaskContent
                | Item::PushPlus
                | Item::PopPlus => None,
            };
            out.push(match local {
                // (one more pixel for antialiasing)
                Some(r) => m.apply_rect(&r).inflate(1.0, 1.0).intersect(&clip),
                None => Rect::ZERO,
            });
        }
    }

    /// Gives each clip, transform, opacity, layer and plus group's push, in `bounds` (from
    /// [`DisplayList::item_bounds`]), the union of what the group's content can paint: the
    /// bounds of its drawings and filter groups, nested groups' included. A group whose union
    /// misses an area draws nothing there, its clip or layer included, so playing back that area
    /// can skip it from its push to its pop. Filter and mask pushes keep their own bounds.
    /// `open` is scratch space, cleared and reused.
    pub fn group_bounds(&self, bounds: &mut [Rect], open: &mut Vec<(usize, Rect)>) {
        open.clear();
        for (i, item) in self.items.iter().enumerate() {
            match item {
                Item::PushClipRect { .. }
                | Item::PushClipRoundedRect { .. }
                | Item::PushClipPath { .. }
                | Item::PushTransform { .. }
                | Item::PushOpacity { .. }
                | Item::PushLayer { .. }
                | Item::PushPlus
                | Item::PushFilter { .. }
                | Item::PushMask { .. } => open.push((i, Rect::ZERO)),
                Item::PopClip
                | Item::PopTransform
                | Item::PopOpacity
                | Item::PopLayer
                | Item::PopPlus
                | Item::PopFilter
                | Item::PopMask => {
                    let Some((at, content)) = open.pop() else {
                        continue;
                    };
                    let shown = match self.items[at] {
                        // (a filter's or a mask's push keeps its own bounds; what it can change
                        // reaches its filter region)
                        Item::PushFilter { .. } => content.union(&bounds[at]),
                        Item::PushMask { .. } => content,
                        _ => {
                            bounds[at] = content;
                            content
                        }
                    };
                    if let Some((_, outer)) = open.last_mut() {
                        *outer = outer.union(&shown);
                    }
                }
                _ => {
                    if item.covers_area() {
                        if let Some((_, outer)) = open.last_mut() {
                            *outer = outer.union(&bounds[i]);
                        }
                    }
                }
            }
        }
    }

    /// The device-space area the list's content can change under `base`: the union of what its
    /// drawings cover ([`DisplayList::item_bounds`]) and of its filter groups' bounds, where a
    /// filter moves pixels past its content (a blur's or a shadow's reach, Filter Effects 1 §5). A
    /// compositor layer drawn from the list needs no more than this; a smaller one cuts its
    /// content.
    pub fn drawn_bounds(&self, base: &Matrix) -> Rect {
        let mut bounds = Vec::new();
        self.item_bounds(base, &mut bounds);
        self.items
            .iter()
            .zip(&bounds)
            .filter(|(i, _)| i.covers_area())
            .fold(Rect::ZERO, |acc, (_, b)| acc.union(b))
    }

    /// Every image the list draws: its image items', its video items' frames and its SVG filters'
    /// `feImage`s, and those of its patterns' tiles (an image drawn twice is there twice).
    pub fn images(&self) -> impl Iterator<Item = ImageId> + '_ {
        fn own(list: &DisplayList) -> impl Iterator<Item = ImageId> + '_ {
            let items = list.items.iter().filter_map(|item| match *item {
                Item::Image { image, .. }
                | Item::Video {
                    image: Some(image), ..
                } => Some(image),
                _ => None,
            });
            items.chain(list.filters.iter().flat_map(FilterOp::images))
        }
        // (a pattern's tile holds no patterns of its own)
        own(self).chain(self.patterns.iter().flat_map(|p| own(&p.list)))
    }

    /// Where the images `wanted` accepts are drawn (in a video item too): each drawing's bounds as
    /// [`DisplayList::item_bounds`] gives them under the identity (the list's coordinates, cut to
    /// its clips; empty when clipped away), and whether it is inside an [`Item::PushLayer`] group
    /// (fixed or sticky content, which a compositor moves). `out` is cleared and reused.
    pub fn image_bounds(
        &self,
        wanted: &dyn Fn(ImageId) -> bool,
        out: &mut Vec<(ImageId, Rect, bool)>,
    ) {
        out.clear();
        let image_of = |item: &Item| match *item {
            Item::Image { image, .. }
            | Item::Video {
                image: Some(image), ..
            } => Some(image),
            _ => None,
        };
        if !self.items.iter().filter_map(image_of).any(wanted) {
            return;
        }
        let mut bounds = Vec::new();
        self.item_bounds(&Matrix::IDENTITY, &mut bounds);
        let mut layers = 0usize;
        for (item, b) in self.items.iter().zip(&bounds) {
            match item {
                Item::PushLayer { .. } => layers += 1,
                Item::PopLayer => layers = layers.saturating_sub(1),
                _ => {
                    if let Some(image) = image_of(item).filter(|&i| wanted(i)) {
                        out.push((image, *b, layers > 0));
                    }
                }
            }
        }
    }

    /// The list with image items drawn as other lists: each [`Item::Image`] drawing its whole image
    /// (an empty `src`) for which `replacement` gives a list, a matrix and the axes it clips on is
    /// replaced by that list, under the matrix (from its coordinates to this list's), clipped to
    /// the image's `dest` on the axes it clips on ([`ClipAxes`]: a replaced element's `overflow`)
    /// and composited with the image's opacity. `None` when nothing was replaced. Replacement lists
    /// that do not validate are left out, as the image is.
    pub fn replace_images<'a>(
        &self,
        replacement: &mut dyn FnMut(ImageId, Rect) -> Option<(&'a DisplayList, Matrix, ClipAxes)>,
    ) -> Option<DisplayList> {
        let mut out: Option<DisplayList> = None;
        // Whether each list validates, by address: images that share one (a page's repeated
        // icons) check it once.
        let mut valid: std::collections::HashMap<*const DisplayList, bool> =
            std::collections::HashMap::new();
        for (i, item) in self.items.iter().enumerate() {
            let found = match *item {
                Item::Image {
                    image,
                    dest,
                    src,
                    opacity,
                    ..
                } if src.is_empty() => {
                    replacement(image, dest).map(|(list, m, axes)| (list, m, dest, axes, opacity))
                }
                _ => None,
            };
            let Some((list, m, dest, axes, opacity)) = found else {
                if let Some(out) = &mut out {
                    copy_item(out, self, item);
                }
                continue;
            };
            let out = out.get_or_insert_with(|| {
                // (what came before, as it is)
                let mut head = DisplayList::new();
                for earlier in &self.items[..i] {
                    copy_item(&mut head, self, earlier);
                }
                head
            });
            if opacity <= 0.0
                || !*valid
                    .entry(list as *const DisplayList)
                    .or_insert_with(|| list.validate().is_ok())
            {
                continue;
            }
            // (an axis that does not clip reaches as far as the list draws)
            let clip = if axes.x && axes.y {
                dest
            } else {
                let drawn = list.drawn_bounds(&m).union(&dest);
                let (x0, x1) = match axes.x {
                    true => (dest.x, dest.right()),
                    false => (drawn.x, drawn.right()),
                };
                let (y0, y1) = match axes.y {
                    true => (dest.y, dest.bottom()),
                    false => (drawn.y, drawn.bottom()),
                };
                Rect::new(x0, y0, x1 - x0, y1 - y0)
            };
            out.items.push(Item::PushClipRect { rect: clip });
            if opacity < 1.0 {
                out.items.push(Item::PushOpacity {
                    opacity,
                    bounds: clip,
                });
            }
            out.items.push(Item::PushTransform { matrix: m });
            append_list(out, list);
            out.items.push(Item::PopTransform);
            if opacity < 1.0 {
                out.items.push(Item::PopOpacity);
            }
            out.items.push(Item::PopClip);
        }
        out
    }

    /// Splits a list into what a compositor draws as separate pieces, in paint order, as Blink's
    /// `PaintArtifactCompositor` makes layers of paint chunks: the content between layers
    /// (`kind` `None`: it scrolls with the document), each top-level fixed or sticky
    /// [`Item::PushLayer`] group (without its push and pop), and each
    /// [`LayerKind::Composited`] group with the clips, transforms and opacities it sits under as its
    /// [`SplitLayer::state`] (its list is in its own coordinates, so a frame that changes only those
    /// leaves the piece's list as it was). Content after a split piece goes on in a new piece that
    /// opens the groups around it again. A fixed or sticky group inside a clip, transform or opacity
    /// group stays where it is, as do groups nested in a fixed or sticky layer. A fixed or sticky
    /// layer is cut at each backdrop filter in it that reads what is under the layer (one whose
    /// backdrop root is outside it): the piece that starts there has the filter as its first item,
    /// inside the opacity groups of the filter's own element, for a compositor to apply to what it
    /// has drawn under the piece ([`DisplayList::take_leading_backdrop`]); such a filter under any
    /// other group of the layer keeps the layer in place. A composited group
    /// is split off only under rectangle clips, rounded clips, transforms and its own opacity (an
    /// opacity group around nothing but it); under a clip path, a mask, a `plus-lighter` group, a
    /// layer kept in place or with a video in it, it stays in place. `pinned` tells whether any of
    /// the list's content depends on the scroll position in a way the layers don't describe (a
    /// [`LayerKind::Pinned`] group, or a fixed or sticky one that could not be split off). Pieces
    /// that draw nothing, or whose state fades them out entirely, are left out.
    pub fn split_layers(&self) -> SplitList {
        /// A piece being collected: its kind, its state, and the groups open in it (item indices,
        /// which a piece that resumes it opens again).
        struct Context {
            kind: Option<LayerKind>,
            state: LayerState,
            groups: Vec<usize>,
        }
        #[derive(Clone, Copy, PartialEq)]
        enum Role {
            /// A fixed or sticky group split off, or a composited one.
            Split,
            /// Left in place: its push and pop are not copied.
            Inline,
        }
        fn flush(
            builder: &mut DisplayListBuilder,
            ctx: &Context,
            prune: bool,
            out: &mut SplitList,
        ) {
            let mut list = std::mem::take(builder).build();
            // (a piece cut at a split, or one that opened its groups again after one, may hold
            // groups around nothing: an animated transform around nothing would change the
            // piece's list at every frame)
            if prune {
                list.drop_empty_groups();
            }
            // (a piece faded out entirely shows nothing: YouTube keeps hundreds of
            // `will-change: opacity` boxes at 0)
            // (a box whose animation a compositor runs is kept at opacity 0: its opacity may be
            // the animated one)
            let animated = ctx
                .state
                .0
                .iter()
                .any(|op| matches!(op, StateOp::Animated(_)));
            if list.items.iter().any(Item::is_drawing) && (ctx.state.opacity() > 0.0 || animated) {
                out.layers.push(SplitLayer {
                    kind: ctx.kind,
                    state: ctx.state.clone(),
                    list,
                });
            }
        }
        let items = &self.items;
        /// What the backdrop filters in a layer's items read.
        #[derive(PartialEq)]
        enum Backdrops {
            /// There are none, or only ones whose backdrop root is in the layer.
            Within,
            /// Some read what is under the layer, each inside nothing but its own opacity groups:
            /// the layer can be cut there.
            Under,
            /// One reads what is under the layer from inside another group of the layer.
            Held,
        }
        // (`inner`: the layer's items, between its push and its pop)
        let backdrops = |inner: &[Item]| -> Backdrops {
            /// An open group, as a backdrop filter in it sees it.
            #[derive(Clone, Copy, PartialEq)]
            enum Open {
                /// An opacity group (at 0 or not): a backdrop root, which a cut opens again.
                Opacity(bool),
                /// A mask: a backdrop root, which no cut opens again.
                Mask,
                /// A rectangle clip or a transform that scales and translates: a cut's backdrop is drawn
                /// through it ([`DisplayList::take_leading_backdrop`]).
                Through,
                Other,
            }
            let mut open: Vec<Open> = Vec::new();
            let mut found = Backdrops::Within;
            for item in inner {
                match item {
                    Item::PushOpacity { opacity, .. } => open.push(Open::Opacity(*opacity <= 0.0)),
                    Item::PushMask { .. } => open.push(Open::Mask),
                    Item::PushClipRect { .. } => open.push(Open::Through),
                    // (a scale and a translation: no rotation of the backdrop's corners)
                    Item::PushTransform { matrix } if matrix.m12 == 0.0 && matrix.m21 == 0.0 => {
                        open.push(Open::Through)
                    }
                    Item::PushClipRoundedRect { .. }
                    | Item::PushClipPath { .. }
                    | Item::PushTransform { .. }
                    | Item::PushFilter { .. }
                    | Item::PushLayer { .. }
                    | Item::PushPlus => open.push(Open::Other),
                    Item::PopClip
                    | Item::PopTransform
                    | Item::PopOpacity
                    | Item::PopFilter
                    | Item::PopLayer
                    | Item::PopMask
                    | Item::PopPlus => {
                        open.pop();
                    }
                    Item::BackdropFilter { own, .. } => {
                        let roots = open
                            .iter()
                            .filter(|g| matches!(g, Open::Opacity(_) | Open::Mask))
                            .count();
                        // (one inside the layer's own backdrop root reads nothing under it, and
                        // one at opacity 0 shows nothing)
                        if roots > *own as usize || open.contains(&Open::Opacity(true)) {
                            continue;
                        }
                        if open
                            .iter()
                            .all(|g| matches!(g, Open::Opacity(_) | Open::Through))
                        {
                            found = Backdrops::Under;
                        } else {
                            return Backdrops::Held;
                        }
                    }
                    _ => {}
                }
            }
            found
        };
        // Whether content that scrolls with the document (outside every fixed and sticky layer)
        // has a backdrop filter: it reads what is painted before it, which a compositor does not
        // have where that is a piece of its own.
        let scrolling_backdrop = {
            let mut moving = 0usize;
            let mut open: Vec<bool> = Vec::new();
            let mut any = false;
            for item in items {
                match item {
                    Item::PushLayer { kind } => {
                        let m = matches!(kind, LayerKind::Fixed | LayerKind::Sticky(_));
                        moving += usize::from(m);
                        open.push(m);
                    }
                    Item::PopLayer => {
                        if open.pop() == Some(true) {
                            moving -= 1;
                        }
                    }
                    Item::BackdropFilter { .. } if moving == 0 => {
                        any = true;
                        break;
                    }
                    _ => {}
                }
            }
            any
        };
        // The matching pop of the layer group pushed at `i`, if any.
        let layer_end = |i: usize| -> Option<usize> {
            let mut depth = 0usize;
            for (j, item) in items.iter().enumerate().skip(i) {
                match item {
                    Item::PushLayer { .. } => depth += 1,
                    Item::PopLayer => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(j);
                        }
                    }
                    _ => {}
                }
            }
            None
        };
        // Opens the groups pushed at `groups` again in a piece that goes on after a cut.
        let reopen = |current: &mut DisplayListBuilder, groups: &[usize]| {
            for &g in groups {
                current.stack.push(match items[g] {
                    Item::PushClipRect { .. }
                    | Item::PushClipRoundedRect { .. }
                    | Item::PushClipPath { .. } => Group::Clip,
                    Item::PushTransform { .. } => Group::Transform,
                    Item::PushOpacity { .. } => Group::Opacity,
                    Item::PushFilter { .. } => Group::Filter,
                    Item::PushMask { .. } => Group::Mask,
                    _ => Group::Plus,
                });
                copy_item(&mut current.list, self, &items[g]);
            }
        };
        let mut out = SplitList::default();
        let mut current = DisplayListBuilder::new();
        let mut contexts = vec![Context {
            kind: None,
            state: LayerState::default(),
            groups: Vec::new(),
        }];
        let mut layers: Vec<Role> = Vec::new();
        let mut resumed = false;
        for (i, item) in items.iter().enumerate() {
            match item {
                Item::PushLayer { kind: k } => {
                    let ctx = contexts.last().unwrap();
                    let in_moving =
                        matches!(ctx.kind, Some(LayerKind::Fixed | LayerKind::Sticky(_)));
                    let inline_open = layers.contains(&Role::Inline);
                    // (content that reads what is painted under it, a backdrop filter, is painted
                    // with what is under it: a composited layer with one is left in place; a
                    // fixed or sticky one is cut at the filter where it can be, and else pins the
                    // list)
                    let reads =
                        layer_end(i).map_or(Backdrops::Held, |j| backdrops(&items[i + 1..j]));
                    let reads_backdrop = layer_end(i).is_some_and(|j| {
                        items[i..j]
                            .iter()
                            .any(|it| matches!(it, Item::BackdropFilter { .. }))
                    });
                    // (a fixed layer under everything, the canvas background's: what scrolls over
                    // it with a backdrop filter is painted with it)
                    let under_readers = i == 0 && scrolling_backdrop;
                    // (a sticky group under clips of content that scrolls with the document:
                    // drawn through them, which stay with the document while the compositor
                    // moves the piece; none of its backdrop filters reads what is under it, and
                    // a video in it is under rectangle clips only, which its quad can take)
                    let clipped_sticky = matches!(k, LayerKind::Sticky(_))
                        && reads == Backdrops::Within
                        && !under_readers
                        && !inline_open
                        && contexts.len() == 1
                        && ctx.state.0.is_empty()
                        && !ctx.groups.is_empty()
                        && ctx.groups.len() < LayerState::MAX_OPS
                        && ctx.groups.iter().all(|&g| match items[g] {
                            Item::PushClipRect { .. } => true,
                            Item::PushClipRoundedRect { .. } => layer_end(i)
                                .is_some_and(|j| !items[i..j].iter().any(Item::is_video)),
                            _ => false,
                        });
                    // (a fixed group straight inside a sticky piece that is outside every group:
                    // placed by the viewport alone, it is cut out of the sticky piece, which goes
                    // on after it)
                    let fixed_in_sticky = matches!(k, LayerKind::Fixed)
                        && matches!(ctx.kind, Some(LayerKind::Sticky(_)))
                        && contexts.len() == 2
                        && ctx.groups.is_empty()
                        && reads != Backdrops::Held
                        && !inline_open;
                    let split = (match k {
                        LayerKind::Pinned => false,
                        LayerKind::Fixed if fixed_in_sticky => true,
                        // (also in a composited box with no clip, transform or opacity around it,
                        // which `will-change` alone makes)
                        LayerKind::Fixed | LayerKind::Sticky(_) => {
                            reads != Backdrops::Held
                                && !under_readers
                                && !in_moving
                                && !inline_open
                                && ctx.groups.is_empty()
                                && ctx.state.0.is_empty()
                        }
                        LayerKind::Composited(_) => {
                            !reads_backdrop
                                && !inline_open
                                && !in_moving
                                && ctx.state.0.len() + ctx.groups.len() < LayerState::MAX_OPS
                                && ctx.groups.iter().enumerate().all(|(n, &g)| match items[g] {
                                    Item::PushTransform { .. }
                                    | Item::PushClipRect { .. }
                                    | Item::PushClipRoundedRect { .. } => true,
                                    // (its own opacity: around this group and nothing else,
                                    // or around its own filter around it)
                                    Item::PushOpacity { .. } => {
                                        let filter = ctx.groups.get(n + 1) == Some(&(g + 1))
                                            && matches!(items[g + 1], Item::PushFilter { .. });
                                        (n + 1 == ctx.groups.len()
                                            && g + 1 == i
                                            && layer_end(i).is_some_and(|j| {
                                                items.get(j + 1) == Some(&Item::PopOpacity)
                                            }))
                                            || (filter
                                                && n + 2 == ctx.groups.len()
                                                && g + 2 == i
                                                && layer_end(i).is_some_and(|j| {
                                                    items.get(j + 2) == Some(&Item::PopOpacity)
                                                }))
                                    }
                                    // (its own filter, of color functions only, which apply to
                                    // each pixel and so commute with its clips and mask: around
                                    // this group and nothing else)
                                    Item::PushFilter { filters, .. } => {
                                        n + 1 == ctx.groups.len()
                                            && g + 1 == i
                                            && layer_end(i).is_some_and(|j| {
                                                items.get(j + 1) == Some(&Item::PopFilter)
                                            })
                                            && ColorFilters::of(self.filter(filters)).is_some()
                                    }
                                    _ => false,
                                })
                                && layer_end(i)
                                    .is_some_and(|j| !items[i..j].iter().any(|it| it.is_video()))
                        }
                    }) || clipped_sticky;
                    if !split {
                        // (a sticky or fixed group within a fixed or sticky layer moves with it)
                        let moves = matches!(k, LayerKind::Fixed | LayerKind::Sticky(_));
                        // (one that shows nothing, all of it at opacity 0, is right at any scroll
                        // position)
                        let shows = layer_end(i).is_none_or(|j| shows_anything(&items[i + 1..j]));
                        if shows && (matches!(k, LayerKind::Pinned) || (moves && !in_moving)) {
                            out.pinned = true;
                            if out.pin_why.is_none() {
                                let reason = if matches!(k, LayerKind::Pinned) {
                                    "marked pinned"
                                } else if reads == Backdrops::Held {
                                    "backdrop filter held in a group"
                                } else if under_readers {
                                    "under content with a backdrop filter"
                                } else if inline_open {
                                    "inside a layer left in place"
                                } else if !ctx.state.0.is_empty() {
                                    "inside a composited piece"
                                } else {
                                    "inside groups"
                                };
                                out.pin_why = Some(PinWhy {
                                    reason,
                                    at: i,
                                    end: layer_end(i).unwrap_or(items.len()),
                                });
                            }
                        }
                        layers.push(Role::Inline);
                        continue;
                    }
                    flush(&mut current, ctx, true, &mut out);
                    let state = if clipped_sticky {
                        LayerState(
                            ctx.groups
                                .iter()
                                .filter_map(|&g| state_op(self, &items[g]))
                                .collect(),
                        )
                    } else if let LayerKind::Composited(key) = k {
                        let mut ops = ctx.state.0.clone();
                        ops.extend(ctx.groups.iter().filter_map(|&g| state_op(self, &items[g])));
                        // (a box with compositor animations: its own transform and opacity are
                        // the last two steps, which paint always pushes for it, or the two before
                        // its own filter)
                        let own = matches!(
                            ops[ops.len().saturating_sub(2)..],
                            [StateOp::Transform(_), StateOp::Opacity(_)]
                        ) || matches!(
                            ops[ops.len().saturating_sub(3)..],
                            [
                                StateOp::Transform(_),
                                StateOp::Opacity(_),
                                StateOp::Filter(_)
                            ]
                        );
                        if own && self.animations.iter().any(|a| a.element == *key) {
                            ops.push(StateOp::Animated(*key));
                        }
                        LayerState(ops)
                    } else {
                        LayerState::default()
                    };
                    contexts.push(Context {
                        kind: Some(*k),
                        state,
                        groups: Vec::new(),
                    });
                    layers.push(Role::Split);
                    continue;
                }
                Item::PopLayer => {
                    if layers.pop() == Some(Role::Split) && contexts.len() > 1 {
                        let done = contexts.pop().unwrap();
                        flush(&mut current, &done, true, &mut out);
                        // The enclosing piece goes on with its groups open again.
                        let ctx = contexts.last().unwrap();
                        resumed = true;
                        reopen(&mut current, &ctx.groups);
                    }
                    continue;
                }
                Item::BackdropFilter { own, .. } => {
                    // In a fixed or sticky piece, one that reads what is under the layer starts
                    // a piece (inside its own opacity groups, opened again).
                    let ctx = contexts.last().unwrap();
                    let roots = ctx
                        .groups
                        .iter()
                        .filter(|&&g| {
                            matches!(items[g], Item::PushOpacity { .. } | Item::PushMask { .. })
                        })
                        .count();
                    if contexts.len() > 1
                        && matches!(ctx.kind, Some(LayerKind::Fixed | LayerKind::Sticky(_)))
                        && !layers.contains(&Role::Inline)
                        && roots <= *own as usize
                    {
                        flush(&mut current, ctx, true, &mut out);
                        resumed = true;
                        reopen(&mut current, &ctx.groups);
                    }
                }
                Item::PushClipRect { .. }
                | Item::PushClipRoundedRect { .. }
                | Item::PushClipPath { .. } => {
                    contexts.last_mut().unwrap().groups.push(i);
                    current.stack.push(Group::Clip);
                }
                Item::PushTransform { .. } => {
                    contexts.last_mut().unwrap().groups.push(i);
                    current.stack.push(Group::Transform);
                }
                Item::PushOpacity { .. } => {
                    contexts.last_mut().unwrap().groups.push(i);
                    current.stack.push(Group::Opacity);
                }
                Item::PushFilter { .. } => {
                    contexts.last_mut().unwrap().groups.push(i);
                    current.stack.push(Group::Filter);
                }
                Item::PushMask { .. } => {
                    contexts.last_mut().unwrap().groups.push(i);
                    current.stack.push(Group::Mask);
                }
                Item::PushPlus => {
                    contexts.last_mut().unwrap().groups.push(i);
                    current.stack.push(Group::Plus);
                }
                Item::MaskContent => {
                    current.stack.pop();
                    current.stack.push(Group::MaskBody);
                }
                Item::PopClip
                | Item::PopTransform
                | Item::PopOpacity
                | Item::PopFilter
                | Item::PopMask
                | Item::PopPlus => {
                    contexts.last_mut().unwrap().groups.pop();
                    current.stack.pop();
                }
                _ => {}
            }
            copy_item(&mut current.list, self, item);
        }
        flush(&mut current, contexts.last().unwrap(), resumed, &mut out);
        out.merge_plain_pieces();
        out
    }

    /// For diagnostics: the layer group that pins this list (from [`DisplayList::split_layers`]),
    /// its kind, the groups open around it (outermost first) and the bounds of what it draws in
    /// the list's coordinates.
    pub fn describe_pin(&self, why: &PinWhy) -> String {
        let mut open: Vec<&'static str> = Vec::new();
        for item in self.items.iter().take(why.at) {
            match item {
                Item::PushClipRect { .. } => open.push("clip"),
                Item::PushClipRoundedRect { .. } => open.push("rounded clip"),
                Item::PushClipPath { .. } => open.push("clip path"),
                Item::PushTransform { .. } => open.push("transform"),
                Item::PushOpacity { .. } => open.push("opacity"),
                Item::PushFilter { .. } => open.push("filter"),
                Item::PushMask { .. } => open.push("mask"),
                Item::PushPlus => open.push("plus-lighter"),
                Item::PushLayer { kind } => open.push(match kind {
                    LayerKind::Fixed => "fixed layer",
                    LayerKind::Sticky(_) => "sticky layer",
                    LayerKind::Pinned => "pinned layer",
                    LayerKind::Composited(_) => "composited layer",
                }),
                Item::PopClip
                | Item::PopTransform
                | Item::PopOpacity
                | Item::PopFilter
                | Item::PopMask
                | Item::PopPlus
                | Item::PopLayer => {
                    open.pop();
                }
                _ => {}
            }
        }
        let kind = match self.items.get(why.at) {
            Some(Item::PushLayer { kind }) => format!("{kind:?}"),
            _ => "?".into(),
        };
        let mut rects = Vec::new();
        self.item_bounds(&Matrix::IDENTITY, &mut rects);
        let mut bounds: Option<Rect> = None;
        let end = why.end.min(self.items.len());
        for (item, r) in self.items[why.at..end].iter().zip(&rects[why.at..end]) {
            if item.is_drawing() && r.width > 0.0 && r.height > 0.0 {
                bounds = Some(bounds.map_or(*r, |b| b.union(r)));
            }
        }
        let b = bounds.unwrap_or_default();
        format!(
            "{} ({kind}, {} items) in [{}] at {:.0},{:.0} {:.0}x{:.0}",
            why.reason,
            end - why.at,
            open.join(", "),
            b.x,
            b.y,
            b.width,
            b.height
        )
    }

    /// Takes out the backdrop filter a piece of a split list starts with
    /// ([`DisplayList::split_layers`] cuts a fixed or sticky layer there): the first item that is
    /// not the push of an opacity group, a rectangle clip or a transform that scales and
    /// translates, when it is a backdrop filter whose own groups are those opacity groups. Its
    /// rectangle is mapped through the transforms and cut by the clips (a cut one loses its
    /// rounded corners). A compositor applies it to what it has drawn under the piece; the
    /// piece's own raster, which has nothing under it, is drawn without.
    pub fn take_leading_backdrop(&mut self) -> Option<LayerBackdrop> {
        let mut opacity = 1.0f32;
        let mut opacities = 0usize;
        let mut m = Matrix::IDENTITY;
        let mut clip = Rect::infinite();
        for (i, item) in self.items.iter().enumerate() {
            match *item {
                Item::PushOpacity { opacity: o, .. } => {
                    opacity *= o;
                    opacities += 1;
                }
                Item::PushClipRect { rect } => clip = clip.intersect(&m.apply_rect(&rect)),
                Item::PushTransform { matrix } if matrix.m12 == 0.0 && matrix.m21 == 0.0 => {
                    m = matrix.then(&m)
                }
                Item::BackdropFilter {
                    filters,
                    rect,
                    radii,
                    own,
                } if own as usize == opacities => {
                    let filters = self.filter(filters).to_vec();
                    let mapped = m.apply_rect(&rect);
                    let shown = mapped.intersect(&clip);
                    if shown.is_empty() {
                        return None;
                    }
                    let radii = if shown == mapped {
                        let (sx, sy) = (m.m11.abs(), m.m22.abs());
                        let c = |s: Size| Size::new(s.width * sx, s.height * sy);
                        Radii {
                            top_left: c(radii.top_left),
                            top_right: c(radii.top_right),
                            bottom_right: c(radii.bottom_right),
                            bottom_left: c(radii.bottom_left),
                        }
                    } else {
                        Radii::ZERO
                    };
                    self.items.remove(i);
                    return Some(LayerBackdrop {
                        filters,
                        rect: shown,
                        radii,
                        opacity: opacity.clamp(0.0, 1.0),
                    });
                }
                _ => return None,
            }
        }
        None
    }

    /// Removes every group (push to matching pop) with no drawing item in it: it shows nothing.
    fn drop_empty_groups(&mut self) {
        // (per open group: its push's index and whether something drew in it)
        let mut open: Vec<(usize, bool)> = Vec::new();
        let mut gone = vec![false; self.items.len()];
        let mut any = false;
        for (i, item) in self.items.iter().enumerate() {
            match item {
                Item::PushClipRect { .. }
                | Item::PushClipRoundedRect { .. }
                | Item::PushClipPath { .. }
                | Item::PushTransform { .. }
                | Item::PushOpacity { .. }
                | Item::PushFilter { .. }
                | Item::PushLayer { .. }
                | Item::PushMask { .. }
                | Item::PushPlus => open.push((i, false)),
                Item::PopClip
                | Item::PopTransform
                | Item::PopOpacity
                | Item::PopFilter
                | Item::PopLayer
                | Item::PopMask
                | Item::PopPlus => {
                    let Some((start, drew)) = open.pop() else {
                        return;
                    };
                    if drew {
                        if let Some(parent) = open.last_mut() {
                            parent.1 = true;
                        }
                    } else {
                        gone[start..=i].iter_mut().for_each(|g| *g = true);
                        any = true;
                    }
                }
                Item::MaskContent => {}
                _ => {
                    if let Some(g) = open.last_mut() {
                        g.1 = true;
                    }
                }
            }
        }
        if any {
            let mut flags = gone.into_iter();
            self.items.retain(|_| !flags.next().unwrap_or(false));
        }
    }

    // ----- encoding -----

    const MAGIC: [u8; 4] = *b"ODL5";

    /// Appends the list's binary encoding (little-endian, versioned) to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut e = Enc(out);
        e.bytes(&Self::MAGIC);
        e.u32(self.items.len() as u32);
        e.u32(self.glyph_ids.len() as u32);
        e.u32(self.stops.len() as u32);
        e.u32(self.paths.verbs.len() as u32);
        e.u32(self.paths.points.len() as u32);
        e.u32(self.paths.dashes.len() as u32);
        e.u32(self.filters.len() as u32);
        e.u32(self.patterns.len() as u32);
        for g in &self.glyph_ids {
            e.u16(*g);
        }
        for a in &self.glyph_advances {
            e.f32(*a);
        }
        for o in &self.glyph_offsets {
            e.f32(o.advance_offset);
            e.f32(o.ascender_offset);
        }
        for s in &self.stops {
            e.f32(s.offset);
            e.color(s.color);
        }
        e.bytes(&self.paths.verbs);
        for p in &self.paths.points {
            e.point(*p);
        }
        for d in &self.paths.dashes {
            e.f32(*d);
        }
        for op in &self.filters {
            e.filter_op(op);
        }
        for p in &self.patterns {
            e.f32(p.tile.width);
            e.f32(p.tile.height);
            // (the tile's list in its own encoding, after its length)
            let at = e.0.len();
            e.u32(0);
            p.list.encode(e.0);
            let len = (e.0.len() - at - 4) as u32;
            e.0[at..at + 4].copy_from_slice(&len.to_le_bytes());
        }
        for item in &self.items {
            e.item(item);
        }
    }

    /// Decodes and validates a list made by [`DisplayList::encode`].
    pub fn decode(bytes: &[u8]) -> Result<DisplayList, ListError> {
        let mut list = DisplayList::new();
        list.decode_from(bytes)?;
        Ok(list)
    }

    /// [`DisplayList::decode`] into this list, keeping the capacity of its vectors (it is left empty
    /// when the bytes are refused).
    pub fn decode_from(&mut self, bytes: &[u8]) -> Result<(), ListError> {
        self.clear();
        let result = self.read(bytes, false);
        if result.is_err() {
            self.clear();
        }
        result
    }

    /// Reads a list; `tile` for a pattern's tile, which holds no patterns of its own.
    fn read(&mut self, bytes: &[u8], tile: bool) -> Result<(), ListError> {
        let mut d = Dec { b: bytes, pos: 0 };
        if d.take(4)? != Self::MAGIC {
            return Err(ListError::BadHeader);
        }
        let items = d.u32()? as usize;
        let glyphs = d.u32()? as usize;
        let stops = d.u32()? as usize;
        let verbs = d.u32()? as usize;
        let points = d.u32()? as usize;
        let dashes = d.u32()? as usize;
        let filters = d.u32()? as usize;
        let patterns = d.u32()? as usize;
        if tile && patterns > 0 {
            return Err(ListError::BadValue(0));
        }
        // (counts are checked against the remaining bytes before anything is reserved)
        let remaining = bytes.len() - d.pos;
        if items > remaining
            || glyphs.saturating_mul(14) > remaining
            || stops.saturating_mul(8) > remaining
            || verbs > remaining
            || points.saturating_mul(8) > remaining
            || dashes.saturating_mul(4) > remaining
            || filters.saturating_mul(5) > remaining
            || patterns.saturating_mul(12) > remaining
        {
            return Err(ListError::Truncated);
        }
        self.items.reserve(items);
        self.glyph_ids.reserve(glyphs);
        self.glyph_advances.reserve(glyphs);
        self.glyph_offsets.reserve(glyphs);
        self.stops.reserve(stops);
        for _ in 0..glyphs {
            self.glyph_ids.push(d.u16()?);
        }
        for _ in 0..glyphs {
            self.glyph_advances.push(d.f32()?);
        }
        for _ in 0..glyphs {
            self.glyph_offsets.push(GlyphOffset {
                advance_offset: d.f32()?,
                ascender_offset: d.f32()?,
            });
        }
        for _ in 0..stops {
            self.stops.push(GradientStop {
                offset: d.f32()?,
                color: d.color()?,
            });
        }
        self.paths.verbs.extend_from_slice(d.take(verbs)?);
        self.paths.points.reserve(points);
        for _ in 0..points {
            self.paths.points.push(d.point()?);
        }
        self.paths.dashes.reserve(dashes);
        for _ in 0..dashes {
            self.paths.dashes.push(d.f32()?);
        }
        self.filters.reserve(filters);
        for _ in 0..filters {
            self.filters.push(d.filter_op()?);
        }
        self.patterns.reserve(patterns);
        for _ in 0..patterns {
            let tile = Size::new(d.f32()?, d.f32()?);
            let len = d.u32()? as usize;
            let mut list = DisplayList::new();
            list.read(d.take(len)?, true)?;
            self.patterns.push(Pattern { tile, list });
        }
        for _ in 0..items {
            self.items.push(d.item()?);
        }
        if d.pos != bytes.len() {
            return Err(ListError::BadTag(d.pos));
        }
        self.validate()
    }
}

// A list crosses a process boundary in its own validated encoding, whatever format carries the
// message around it: bytes that don't decode to a well-formed list fail the message.
impl Serialize for DisplayList {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut bytes = Vec::new();
        self.encode(&mut bytes);
        serializer.serialize_bytes(&bytes)
    }
}

impl<'de> Deserialize<'de> for DisplayList {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Bytes;
        impl serde::de::Visitor<'_> for Bytes {
            type Value = DisplayList;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an encoded display list")
            }
            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<DisplayList, E> {
                DisplayList::decode(bytes).map_err(|e| E::custom(format!("{e:?}")))
            }
        }
        deserializer.deserialize_bytes(Bytes)
    }
}

/// A display list split into layers ([`DisplayList::split_layers`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplitList {
    /// In paint order.
    pub layers: Vec<SplitLayer>,
    /// Some content can only be shown right by painting the list again at each scroll position.
    pub pinned: bool,
    /// When pinned, the first layer group that pins it and why (for diagnostics:
    /// [`DisplayList::describe_pin`]).
    pub pin_why: Option<PinWhy>,
}

/// The layer group that pins a split list ([`SplitList::pin_why`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PinWhy {
    pub reason: &'static str,
    /// The group's push item and its pop (or the list's end), in the list split.
    pub at: usize,
    pub end: usize,
}

impl SplitList {
    /// Puts each piece that is drawn as painted (no kind, no state) into the nearest such piece
    /// before it when it overlaps none of the pieces in between, as Blink's
    /// `PaintArtifactCompositor` merges a paint chunk into an earlier pending layer: content after a
    /// composited box joins the page again instead of making a layer of its own. Overlap is by
    /// the pieces' bounds in the list's coordinates (through their transforms); a fixed or sticky
    /// piece, which moves, overlaps everything.
    fn merge_plain_pieces(&mut self) {
        if self.layers.len() < 3 {
            return;
        }
        let plain = |l: &SplitLayer| l.kind.is_none() && l.state.0.is_empty();
        let mut scratch = Vec::new();
        let mut bounds_of = |l: &SplitLayer| -> Option<Rect> {
            if matches!(
                l.kind,
                Some(LayerKind::Fixed | LayerKind::Sticky(_) | LayerKind::Pinned)
            ) {
                return None;
            }
            l.list.item_bounds(&l.state.transform(), &mut scratch);
            Some(
                l.list
                    .items
                    .iter()
                    .zip(&scratch)
                    .filter(|(i, b)| i.covers_area() && !b.is_empty())
                    .fold(
                        Rect::ZERO,
                        |acc, (_, b)| {
                            if acc.is_empty() {
                                *b
                            } else {
                                acc.union(b)
                            }
                        },
                    ),
            )
        };
        let mut out: Vec<SplitLayer> = Vec::with_capacity(self.layers.len());
        let mut bounds: Vec<Option<Rect>> = Vec::with_capacity(self.layers.len());
        for layer in std::mem::take(&mut self.layers) {
            let b = bounds_of(&layer);
            if plain(&layer) {
                if let (Some(j), Some(mine)) = (out.iter().rposition(plain), b) {
                    let clear = bounds[j + 1..]
                        .iter()
                        .all(|o| o.is_some_and(|o| o.is_empty() || !o.intersects(&mine)));
                    if clear {
                        append_list(&mut out[j].list, &layer.list);
                        bounds[j] =
                            bounds[j].map(|o| if o.is_empty() { mine } else { o.union(&mine) });
                        continue;
                    }
                }
            }
            out.push(layer);
            bounds.push(b);
        }
        self.layers = out;
    }
}

/// A backdrop filter (Filter Effects 2 §3) a compositor applies to what it has drawn under a piece
/// of a split list, before the piece ([`DisplayList::take_leading_backdrop`]).
#[derive(Clone, Debug, PartialEq)]
pub struct LayerBackdrop {
    /// The filter functions, the first applied first.
    pub filters: Vec<FilterOp>,
    /// The element's border box and its corners, in the piece's coordinates: what is under it
    /// is filtered, and the result is clipped to it.
    pub rect: Rect,
    pub radii: Radii,
    /// The element's own opacity, which applies to the filtered backdrop too.
    pub opacity: f32,
}

/// A piece of a split list ([`DisplayList::split_layers`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplitLayer {
    /// `None` for content that scrolls with the document.
    pub kind: Option<LayerKind>,
    /// What the piece is drawn through (empty: as painted).
    pub state: LayerState,
    /// Its items, in the coordinates inside its state.
    pub list: DisplayList,
}

/// Appends all of `from` (a balanced list) to `to`, its ranges moved to `to`'s side vectors.
fn append_list(to: &mut DisplayList, from: &DisplayList) {
    let glyphs = to.glyph_ids.len() as u32;
    let stops = to.stops.len() as u32;
    to.glyph_ids.extend_from_slice(&from.glyph_ids);
    to.glyph_advances.extend_from_slice(&from.glyph_advances);
    to.glyph_offsets.extend_from_slice(&from.glyph_offsets);
    to.stops.extend_from_slice(&from.stops);
    let verbs = to.paths.verbs.len() as u32;
    let points = to.paths.points.len() as u32;
    let dashes = to.paths.dashes.len() as u32;
    to.paths.verbs.extend_from_slice(&from.paths.verbs);
    to.paths.points.extend_from_slice(&from.paths.points);
    to.paths.dashes.extend_from_slice(&from.paths.dashes);
    let filters = to.filters.len() as u32;
    to.filters.extend_from_slice(&from.filters);
    let patterns = to.patterns.len() as u32;
    to.patterns.extend_from_slice(&from.patterns);
    to.items.reserve(from.items.len());
    for item in &from.items {
        let mut item = *item;
        match &mut item {
            Item::Text(run) => run.glyphs.start += glyphs,
            Item::PushFilter { filters: f, .. } | Item::BackdropFilter { filters: f, .. } => {
                f.start += filters
            }
            Item::LinearGradient { stops: s, .. }
            | Item::RadialGradient { stops: s, .. }
            | Item::ConicGradient { stops: s, .. } => s.start += stops,
            Item::FillPath { path, .. } | Item::PushClipPath { path, .. } => {
                path.verbs += verbs;
                path.points += points;
            }
            Item::StrokePath { path, stroke, .. } => {
                path.verbs += verbs;
                path.points += points;
                stroke.dashes.start += dashes;
            }
            Item::FillPattern { pattern, .. } => *pattern += patterns,
            _ => {}
        }
        to.items.push(item);
    }
}

/// Appends `item` of `from` to `to`, with its glyphs and stops.
fn copy_item(to: &mut DisplayList, from: &DisplayList, item: &Item) {
    let mut item = *item;
    match &mut item {
        Item::Text(run) => {
            let (ids, advances, offsets) = from.glyphs(run.glyphs);
            run.glyphs.start = to.glyph_ids.len() as u32;
            to.glyph_ids.extend_from_slice(ids);
            to.glyph_advances.extend_from_slice(advances);
            to.glyph_offsets.extend_from_slice(offsets);
        }
        Item::LinearGradient { stops, .. }
        | Item::RadialGradient { stops, .. }
        | Item::ConicGradient { stops, .. } => {
            let s = from.stops(*stops);
            stops.start = to.stops.len() as u32;
            to.stops.extend_from_slice(s);
        }
        Item::FillPath { path, .. }
        | Item::StrokePath { path, .. }
        | Item::PushClipPath { path, .. } => {
            let (verbs, points) = from.path(*path);
            path.verbs = to.paths.verbs.len() as u32;
            path.points = to.paths.points.len() as u32;
            to.paths.verbs.extend_from_slice(verbs);
            to.paths.points.extend_from_slice(points);
        }
        Item::FillPattern { pattern, .. } => {
            let p = from
                .patterns
                .get(*pattern as usize)
                .cloned()
                .unwrap_or_default();
            *pattern = to.patterns.len() as u32;
            to.patterns.push(p);
        }
        _ => {}
    }
    if let Item::PushFilter { filters, .. } | Item::BackdropFilter { filters, .. } = &mut item {
        let ops = from.filter(*filters);
        filters.start = to.filters.len() as u32;
        filters.len = ops.len() as u32;
        to.filters.extend_from_slice(ops);
    }
    if let Item::StrokePath { stroke, .. } = &mut item {
        let d = from.dashes(stroke.dashes);
        stroke.dashes.start = to.paths.dashes.len() as u32;
        to.paths.dashes.extend_from_slice(d);
    }
    to.items.push(item);
}

fn rect_finite(r: &Rect) -> bool {
    r.x.is_finite() && r.y.is_finite() && r.width.is_finite() && r.height.is_finite()
}

fn radii_finite(r: &Radii) -> bool {
    r.corners()
        .iter()
        .all(|c| c.width.is_finite() && c.height.is_finite())
}

/// Whether a gradient item (its stops aside) and its stops are drawable: finite, with stops.
fn gradient_ok(item: &Item, stops: &[GradientStop]) -> bool {
    !stops.is_empty() && item_finite(item) && stops.iter().all(|s| s.offset.is_finite())
}

fn item_finite(item: &Item) -> bool {
    let p = |p: &Point| p.x.is_finite() && p.y.is_finite();
    match item {
        Item::FillRect { rect, .. } | Item::PushClipRect { rect } => rect_finite(rect),
        Item::FillRoundedRect { rect, radii, .. } | Item::PushClipRoundedRect { rect, radii } => {
            rect_finite(rect) && radii_finite(radii)
        }
        Item::StrokeRect { rect, width, .. } => rect_finite(rect) && width.is_finite(),
        Item::StrokeRoundedRect {
            rect, radii, width, ..
        } => rect_finite(rect) && radii_finite(radii) && width.is_finite(),
        Item::Border(b) => {
            rect_finite(&b.rect)
                && radii_finite(&b.radii)
                && b.widths.to_array().iter().all(|w| w.is_finite())
        }
        Item::Text(t) => p(&t.origin) && t.size.is_finite() && t.stroke.is_finite(),
        Item::Image {
            dest, src, opacity, ..
        } => rect_finite(dest) && rect_finite(src) && opacity.is_finite(),
        Item::Video { dest, clip, .. } | Item::ProtectedVideo { dest, clip, .. } => {
            rect_finite(dest) && rect_finite(clip)
        }
        Item::LinearGradient {
            rect,
            radii,
            start,
            end,
            ..
        } => rect_finite(rect) && radii_finite(radii) && p(start) && p(end),
        Item::RadialGradient {
            rect,
            radii,
            center,
            radius,
            ..
        } => {
            rect_finite(rect)
                && radii_finite(radii)
                && p(center)
                && radius.width.is_finite()
                && radius.height.is_finite()
        }
        Item::ConicGradient {
            rect,
            radii,
            center,
            angle,
            ..
        } => rect_finite(rect) && radii_finite(radii) && p(center) && angle.is_finite(),
        Item::BoxShadow(s) => {
            rect_finite(&s.rect)
                && radii_finite(&s.radii)
                && p(&s.offset)
                && s.blur.is_finite()
                && s.spread.is_finite()
        }
        Item::PushTransform { matrix } => matrix.is_finite(),
        Item::PushOpacity { opacity, bounds } => opacity.is_finite() && rect_finite(bounds),
        Item::PushFilter { bounds, .. } => rect_finite(bounds),
        Item::BackdropFilter { rect, radii, .. } => rect_finite(rect) && radii_finite(radii),
        Item::PushLayer { kind } => kind.is_valid(),
        Item::PushMask { bounds, .. } => rect_finite(bounds),
        Item::FillPath { bounds, .. } | Item::PushClipPath { bounds, .. } => rect_finite(bounds),
        Item::StrokePath { bounds, stroke, .. } => {
            rect_finite(bounds)
                && stroke.width.is_finite()
                && stroke.miter_limit.is_finite()
                && stroke.dash_offset.is_finite()
        }
        Item::FillPattern {
            rect, transform, ..
        } => rect_finite(rect) && transform.is_finite(),
        Item::PopClip
        | Item::PopTransform
        | Item::PopOpacity
        | Item::PopFilter
        | Item::PopLayer
        | Item::MaskLayer { .. }
        | Item::MaskContent
        | Item::PopMask
        | Item::PushPlus
        | Item::PopPlus => true,
    }
}

/// Builds a [`DisplayList`], keeping pushes and pops balanced and dropping items that would paint
/// nothing (transparent colors, empty rectangles).
#[derive(Default)]
pub struct DisplayListBuilder {
    list: DisplayList,
    stack: Vec<Group>,
}

impl DisplayListBuilder {
    pub fn new() -> DisplayListBuilder {
        DisplayListBuilder::default()
    }

    /// A builder that reuses a previous list's vectors.
    pub fn reuse(mut list: DisplayList) -> DisplayListBuilder {
        list.clear();
        DisplayListBuilder {
            list,
            stack: Vec::new(),
        }
    }

    /// The list so far.
    pub fn list(&self) -> &DisplayList {
        &self.list
    }

    /// How many groups are open.
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Finishes the list, closing any groups still open.
    pub fn build(mut self) -> DisplayList {
        while let Some(g) = self.stack.pop() {
            if g == Group::Mask {
                self.list.items.push(Item::MaskContent);
            }
            self.list.items.push(match g {
                Group::Clip => Item::PopClip,
                Group::Transform => Item::PopTransform,
                Group::Opacity => Item::PopOpacity,
                Group::Filter => Item::PopFilter,
                Group::Layer => Item::PopLayer,
                Group::Mask | Group::MaskBody => Item::PopMask,
                Group::Plus => Item::PopPlus,
            });
        }
        self.list
    }

    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        if !color.is_transparent() && !rect.is_empty() {
            self.list.items.push(Item::FillRect { rect, color });
        }
    }

    pub fn fill_rounded_rect(&mut self, rect: Rect, radii: Radii, color: Color) {
        if color.is_transparent() || rect.is_empty() {
            return;
        }
        if radii.is_zero() {
            self.list.items.push(Item::FillRect { rect, color });
        } else {
            self.list
                .items
                .push(Item::FillRoundedRect { rect, radii, color });
        }
    }

    pub fn stroke_rect(&mut self, rect: Rect, width: f32, color: Color) {
        if !color.is_transparent() && width > 0.0 {
            self.list
                .items
                .push(Item::StrokeRect { rect, width, color });
        }
    }

    pub fn stroke_rounded_rect(&mut self, rect: Rect, radii: Radii, width: f32, color: Color) {
        if color.is_transparent() || width <= 0.0 {
            return;
        }
        if radii.is_zero() {
            self.stroke_rect(rect, width, color);
        } else {
            self.list.items.push(Item::StrokeRoundedRect {
                rect,
                radii,
                width,
                color,
            });
        }
    }

    pub fn border(&mut self, border: Border) {
        let visible = (0..4).any(|i| {
            !border.styles[i].is_invisible()
                && !border.colors[i].is_transparent()
                && border.widths.to_array()[i] > 0.0
        });
        if visible && !border.rect.is_empty() {
            self.list.items.push(Item::Border(border));
        }
    }

    /// A glyph run; the three slices are parallel (one entry per glyph) and are copied into the list.
    /// Whether it was added (an empty or transparent run is not).
    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &mut self,
        glyphs: &[u16],
        advances: &[f32],
        offsets: &[GlyphOffset],
        origin: Point,
        font: FontId,
        size: f32,
        color: Color,
        bidi_level: u8,
    ) -> bool {
        assert!(
            glyphs.len() == advances.len() && glyphs.len() == offsets.len(),
            "glyph slices differ in length"
        );
        if glyphs.is_empty() || color.is_transparent() || size <= 0.0 {
            return false;
        }
        let start = self.list.glyph_ids.len() as u32;
        self.list.glyph_ids.extend_from_slice(glyphs);
        self.list.glyph_advances.extend_from_slice(advances);
        self.list.glyph_offsets.extend_from_slice(offsets);
        self.list.items.push(Item::Text(TextRun {
            glyphs: GlyphRange {
                start,
                len: glyphs.len() as u32,
            },
            origin,
            font,
            size,
            color,
            bidi_level: bidi_level.min(125),
            sideways: false,
            stroke: 0.0,
        }));
        true
    }

    /// Marks the run [`DisplayListBuilder::text`] just added as set for vertical layout.
    pub fn last_text_sideways(&mut self) {
        if let Some(Item::Text(t)) = self.list.items.last_mut() {
            t.sideways = true;
        }
    }

    /// Makes the run [`DisplayListBuilder::text`] just added (when it returned true) a stroke of
    /// its glyphs' outlines `width` wide ([`TextRun::stroke`]).
    pub fn last_text_stroke(&mut self, width: f32) {
        if let Some(Item::Text(t)) = self.list.items.last_mut() {
            t.stroke = width;
        }
    }

    /// An image into `dest`; `src` selects part of it (image pixels), `None` for all of it.
    pub fn image(
        &mut self,
        image: ImageId,
        dest: Rect,
        src: Option<Rect>,
        opacity: f32,
        rendering: ImageRendering,
    ) {
        if dest.is_empty() || opacity <= 0.0 {
            return;
        }
        self.list.items.push(Item::Image {
            image,
            dest,
            src: src.unwrap_or(Rect::ZERO),
            opacity: opacity.min(1.0),
            rendering,
        });
    }

    /// A video's current frame into `dest`, clipped to `clip` (see [`Item::Video`]).
    pub fn video(&mut self, video: VideoId, image: Option<ImageId>, dest: Rect, clip: Rect) {
        if dest.intersect(&clip).is_empty() {
            return;
        }
        self.list.items.push(Item::Video {
            video,
            image,
            dest,
            clip,
            transparent: false,
        });
    }

    /// A canvas's frames on the GPU into `dest`, clipped to `clip`: a transparent
    /// [`Item::Video`], drawn over what is under it, with nothing in its place until a frame shows.
    pub fn canvas_video(&mut self, video: VideoId, dest: Rect, clip: Rect) {
        if dest.intersect(&clip).is_empty() {
            return;
        }
        self.list.items.push(Item::Video {
            video,
            image: None,
            dest,
            clip,
            transparent: true,
        });
    }

    /// A protected player's picture into `dest`, clipped to `clip` (see [`Item::ProtectedVideo`]).
    pub fn protected_video(&mut self, player: u64, dest: Rect, clip: Rect) {
        if dest.intersect(&clip).is_empty() {
            return;
        }
        self.list
            .items
            .push(Item::ProtectedVideo { player, dest, clip });
    }

    fn push_stops(&mut self, stops: &[GradientStop]) -> StopRange {
        let start = self.list.stops.len() as u32;
        self.list.stops.extend_from_slice(stops);
        StopRange {
            start,
            len: stops.len() as u32,
        }
    }

    pub fn linear_gradient(
        &mut self,
        rect: Rect,
        radii: Radii,
        start: Point,
        end: Point,
        stops: &[GradientStop],
        extend: Extend,
    ) {
        // (a gradient with a non-finite coordinate or stop is left out rather than making the
        // list malformed: lists from the builder always validate)
        let item = Item::LinearGradient {
            rect,
            radii,
            start,
            end,
            stops: StopRange::default(),
            extend,
        };
        if rect.is_empty() || !gradient_ok(&item, stops) {
            return;
        }
        let stops = self.push_stops(stops);
        self.list.items.push(Item::LinearGradient {
            rect,
            radii,
            start,
            end,
            stops,
            extend,
        });
    }

    pub fn radial_gradient(
        &mut self,
        rect: Rect,
        radii: Radii,
        center: Point,
        radius: Size,
        stops: &[GradientStop],
        extend: Extend,
    ) {
        let item = Item::RadialGradient {
            rect,
            radii,
            center,
            radius,
            stops: StopRange::default(),
            extend,
        };
        if rect.is_empty() || !gradient_ok(&item, stops) {
            return;
        }
        let stops = self.push_stops(stops);
        self.list.items.push(Item::RadialGradient {
            rect,
            radii,
            center,
            radius,
            stops,
            extend,
        });
    }

    /// A conic gradient (see [`Item::ConicGradient`]).
    pub fn conic_gradient(
        &mut self,
        rect: Rect,
        radii: Radii,
        center: Point,
        angle: f32,
        stops: &[GradientStop],
        extend: Extend,
    ) {
        let item = Item::ConicGradient {
            rect,
            radii,
            center,
            angle,
            stops: StopRange::default(),
            extend,
        };
        if rect.is_empty() || !gradient_ok(&item, stops) {
            return;
        }
        let stops = self.push_stops(stops);
        self.list.items.push(Item::ConicGradient {
            rect,
            radii,
            center,
            angle,
            stops,
            extend,
        });
    }

    /// Starts a group whose drawings are added to what is below them (see [`Item::PushPlus`]).
    /// Adds a path's segments to the side vectors, for [`DisplayListBuilder::fill_path`] and
    /// [`DisplayListBuilder::stroke_path`] (one path can be filled and stroked); `None` when a
    /// coordinate is not finite. A segment before any `MoveTo` starts at the origin.
    pub fn add_path(&mut self, path: &[PathEl]) -> Option<PathRange> {
        let range = PathRange {
            verbs: self.list.paths.verbs.len() as u32,
            verbs_len: 0,
            points: self.list.paths.points.len() as u32,
            points_len: 0,
        };
        let ok = |p: &Point| p.x.is_finite() && p.y.is_finite();
        let all_ok = path.iter().all(|el| match el {
            PathEl::MoveTo(a) | PathEl::LineTo(a) => ok(a),
            PathEl::QuadTo(a, b) => ok(a) && ok(b),
            PathEl::CurveTo(a, b, c) => ok(a) && ok(b) && ok(c),
            PathEl::Close => true,
        });
        if !all_ok {
            return None;
        }
        let l = &mut self.list;
        for el in path {
            match *el {
                PathEl::MoveTo(a) => {
                    l.paths.verbs.push(PathVerb::Move as u8);
                    l.paths.points.push(a);
                }
                PathEl::LineTo(a) => {
                    l.paths.verbs.push(PathVerb::Line as u8);
                    l.paths.points.push(a);
                }
                PathEl::QuadTo(a, b) => {
                    l.paths.verbs.push(PathVerb::Quad as u8);
                    l.paths.points.extend_from_slice(&[a, b]);
                }
                PathEl::CurveTo(a, b, c) => {
                    l.paths.verbs.push(PathVerb::Cubic as u8);
                    l.paths.points.extend_from_slice(&[a, b, c]);
                }
                PathEl::Close => l.paths.verbs.push(PathVerb::Close as u8),
            }
        }
        Some(PathRange {
            verbs_len: l.paths.verbs.len() as u32 - range.verbs,
            points_len: l.paths.points.len() as u32 - range.points,
            ..range
        })
    }

    /// The bounds of a path's points (its control points included: they contain the curves).
    fn path_bounds(&self, path: PathRange) -> Option<Rect> {
        if !self.list.path_ok(path) {
            return None;
        }
        let (_, points) = self.list.path(path);
        let first = points.first()?;
        let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x, first.y);
        for p in &points[1..] {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
    }

    /// Fills a path from [`DisplayListBuilder::add_path`] (nothing for a path with no area).
    pub fn fill_path(&mut self, path: PathRange, color: Color, evenodd: bool, antialias: bool) {
        if color.is_transparent() {
            return;
        }
        let Some(bounds) = self.path_bounds(path) else {
            return;
        };
        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return;
        }
        self.list.items.push(Item::FillPath {
            path,
            bounds,
            color,
            evenodd,
            antialias,
        });
    }

    /// Strokes a path from [`DisplayListBuilder::add_path`]; `dashes` alternate dash and gap
    /// lengths (an odd list is repeated, SVG 2 13.5.6) and are left out (a solid stroke) unless
    /// all are finite and at least zero with a positive sum.
    pub fn stroke_path(
        &mut self,
        path: PathRange,
        color: Color,
        stroke: &Stroke,
        dashes: &[f32],
        antialias: bool,
    ) {
        let finite = stroke.width.is_finite()
            && stroke.miter_limit.is_finite()
            && stroke.dash_offset.is_finite();
        if color.is_transparent() || !finite || stroke.width <= 0.0 {
            return;
        }
        let Some(bounds) = self.path_bounds(path) else {
            return;
        };
        let mut stroke = *stroke;
        stroke.miter_limit = stroke.miter_limit.max(1.0);
        let sum: f32 = dashes.iter().sum();
        let dashed = !dashes.is_empty()
            && sum.is_finite()
            && sum > 0.0
            && dashes.iter().all(|d| d.is_finite() && *d >= 0.0);
        stroke.dashes = DashRange {
            start: self.list.paths.dashes.len() as u32,
            len: 0,
        };
        if dashed {
            self.list.paths.dashes.extend_from_slice(dashes);
            if dashes.len() % 2 == 1 {
                self.list.paths.dashes.extend_from_slice(dashes);
            }
            stroke.dashes.len = self.list.paths.dashes.len() as u32 - stroke.dashes.start;
        } else {
            stroke.dash_offset = 0.0;
        }
        // (half the width each side; a miter reaches out to the limit, a square cap a diagonal)
        let half = stroke.width / 2.0;
        let reach = match stroke.join {
            LineJoin::Miter => half * stroke.miter_limit.max(std::f32::consts::SQRT_2),
            _ => half * std::f32::consts::SQRT_2,
        };
        let bounds = bounds.inflate(reach, reach);
        self.list.items.push(Item::StrokePath {
            path,
            bounds,
            color,
            stroke,
            antialias,
        });
    }

    pub fn push_plus(&mut self) {
        self.stack.push(Group::Plus);
        self.list.items.push(Item::PushPlus);
    }

    pub fn pop_plus(&mut self) {
        self.pop(Group::Plus, Item::PopPlus);
    }

    pub fn box_shadow(&mut self, shadow: BoxShadow) {
        // (an outer shadow of an empty box still shows when its spread gives it a size)
        let shape = if shadow.inset {
            shadow.rect
        } else {
            shadow.rect.inflate(shadow.spread, shadow.spread)
        };
        if !shadow.color.is_transparent() && !shape.is_empty() {
            self.list.items.push(Item::BoxShadow(BoxShadow {
                blur: shadow.blur.max(0.0),
                ..shadow
            }));
        }
    }

    pub fn push_clip_rect(&mut self, rect: Rect) {
        self.stack.push(Group::Clip);
        self.list.items.push(Item::PushClipRect { rect });
    }

    pub fn push_clip_rounded_rect(&mut self, rect: Rect, radii: Radii) {
        self.stack.push(Group::Clip);
        if radii.is_zero() {
            self.list.items.push(Item::PushClipRect { rect });
        } else {
            self.list
                .items
                .push(Item::PushClipRoundedRect { rect, radii });
        }
    }

    /// Clips what follows, up to [`DisplayListBuilder::pop_clip`], to the fill of a path from
    /// [`DisplayListBuilder::add_path`] (see [`Item::PushClipPath`]). A path that is not in this
    /// list, or has no points, clips everything away.
    pub fn push_clip_path(&mut self, path: PathRange, evenodd: bool) {
        self.stack.push(Group::Clip);
        match self.path_bounds(path) {
            Some(bounds) => self.list.items.push(Item::PushClipPath {
                path,
                bounds,
                evenodd,
            }),
            None => self
                .list
                .items
                .push(Item::PushClipRect { rect: Rect::ZERO }),
        }
    }

    pub fn pop_clip(&mut self) {
        self.pop(Group::Clip, Item::PopClip);
    }

    pub fn push_transform(&mut self, matrix: Matrix) {
        self.stack.push(Group::Transform);
        self.list.items.push(Item::PushTransform { matrix });
    }

    pub fn pop_transform(&mut self) {
        self.pop(Group::Transform, Item::PopTransform);
    }

    pub fn push_opacity(&mut self, opacity: f32, bounds: Rect) {
        self.stack.push(Group::Opacity);
        self.list.items.push(Item::PushOpacity {
            opacity: opacity.clamp(0.0, 1.0),
            bounds,
        });
    }

    pub fn pop_opacity(&mut self) {
        self.pop(Group::Opacity, Item::PopOpacity);
    }

    /// Starts a filter group ([`Item::PushFilter`]) of `ops` (in order, the first applied to the
    /// content), whose result shows within `bounds`; ended by [`DisplayListBuilder::pop_filter`].
    /// An invalid function (a NaN, an amount out of range) is left out.
    pub fn push_filter(&mut self, ops: &[FilterOp], bounds: Rect) {
        let start = self.list.filters.len() as u32;
        self.list
            .filters
            .extend(ops.iter().filter(|op| op.is_valid()).cloned());
        let len = self.list.filters.len() as u32 - start;
        self.stack.push(Group::Filter);
        self.list.items.push(Item::PushFilter {
            filters: FilterRange { start, len },
            bounds,
        });
    }

    pub fn pop_filter(&mut self) {
        self.pop(Group::Filter, Item::PopFilter);
    }

    /// A backdrop filter ([`Item::BackdropFilter`]) of `ops` over `rect` with `radii`, inside `own`
    /// groups of the element's own. An invalid function is left out; with none left, or an empty
    /// rectangle, nothing is added.
    pub fn backdrop_filter(&mut self, ops: &[FilterOp], rect: Rect, radii: Radii, own: u8) {
        if rect.is_empty() || !ops.iter().any(FilterOp::is_valid) {
            return;
        }
        let start = self.list.filters.len() as u32;
        self.list
            .filters
            .extend(ops.iter().filter(|op| op.is_valid()).cloned());
        let len = self.list.filters.len() as u32 - start;
        self.list.items.push(Item::BackdropFilter {
            filters: FilterRange { start, len },
            rect,
            radii,
            own,
        });
    }

    /// An animation of a composited box that a compositor runs ([`DisplayList::animations`]).
    pub fn add_animation(&mut self, animation: crate::layer_animation::LayerAnimation) {
        self.list.animations.push(animation);
    }

    /// Starts content a compositor may draw as a layer of its own ([`Item::PushLayer`]).
    pub fn push_layer(&mut self, kind: LayerKind) {
        self.stack.push(Group::Layer);
        self.list.items.push(Item::PushLayer { kind });
    }

    pub fn pop_layer(&mut self) {
        self.pop(Group::Layer, Item::PopLayer);
    }

    /// Starts a mask group ([`Item::PushMask`]): the mask's items follow, then
    /// [`DisplayListBuilder::mask_content`], the masked content and [`DisplayListBuilder::pop_mask`].
    pub fn push_mask(&mut self, bounds: Rect, luminance: bool) {
        self.stack.push(Group::Mask);
        self.list.items.push(Item::PushMask { bounds, luminance });
    }

    /// Between a mask's items: what follows is its next layer ([`Item::MaskLayer`]), composited
    /// onto what came before by `composite`.
    pub fn mask_layer(&mut self, composite: MaskComposite, luminance: bool) {
        assert_eq!(
            self.stack.last(),
            Some(&Group::Mask),
            "mask layer outside a mask's items"
        );
        self.list.items.push(Item::MaskLayer {
            composite,
            luminance,
        });
    }

    /// Ends the mask's items: what follows is masked.
    pub fn mask_content(&mut self) {
        self.pop(Group::Mask, Item::MaskContent);
        self.stack.push(Group::MaskBody);
    }

    pub fn pop_mask(&mut self) {
        self.pop(Group::MaskBody, Item::PopMask);
    }

    /// Appends a finished list (a nested document's, painted in its own coordinates: push a clip and
    /// a transform around it), its glyph and stop ranges moved to this list's side vectors. A list
    /// that does not validate is left out.
    pub fn append(&mut self, list: &DisplayList) {
        if list.validate().is_err() {
            return;
        }
        append_list(&mut self.list, list);
    }

    /// Fills `rect` with a pattern ([`Item::FillPattern`]): `tile`'s list drawn in a tile of
    /// `size` and repeated, through `transform` from the tile's space to this list's. Nothing for
    /// an empty area or tile, a tile that draws nothing, or one that cannot be a tile (patterns,
    /// layers, videos or backdrop filters in it, or a list that does not validate).
    pub fn fill_pattern(&mut self, rect: Rect, size: Size, transform: Matrix, tile: DisplayList) {
        if rect.is_empty()
            || !(size.width > 0.0 && size.height > 0.0)
            || !transform.is_finite()
            || !tile.items.iter().any(Item::is_drawing)
        {
            return;
        }
        let p = Pattern {
            tile: size,
            list: tile,
        };
        let probe = DisplayList {
            patterns: vec![p],
            ..DisplayList::default()
        };
        if probe.validate().is_err() {
            return;
        }
        let pattern = self.list.patterns.len() as u32;
        self.list.patterns.extend(probe.patterns);
        self.list.items.push(Item::FillPattern {
            rect,
            pattern,
            transform,
        });
    }

    fn pop(&mut self, group: Group, item: Item) {
        match self.stack.last() {
            Some(g) if *g == group => {
                self.stack.pop();
                self.list.items.push(item);
            }
            other => panic!("pop {group:?} with {other:?} open"),
        }
    }
}

// ----- binary encoding -----

/// A fast 64-bit hash of bytes (FxHash's mixing over 8-byte words): not for adversarial input, only
/// for telling apart what a page painted from one frame to the next.
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mut h = bytes.len() as u64;
    let (chunks, rest) = bytes.as_chunks::<8>();
    for c in chunks {
        h = (h.rotate_left(5) ^ u64::from_le_bytes(*c)).wrapping_mul(K);
    }
    if !rest.is_empty() {
        let mut w = [0u8; 8];
        w[..rest.len()].copy_from_slice(rest);
        h = (h.rotate_left(5) ^ u64::from_le_bytes(w)).wrapping_mul(K);
    }
    h
}

/// Folds `v` into the running hash `h` (order matters).
pub fn hash_combine(h: u64, v: u64) -> u64 {
    (h.rotate_left(5) ^ v).wrapping_mul(0x517c_c1b7_2722_0a95)
}

struct Enc<'a>(&'a mut Vec<u8>);

impl Enc<'_> {
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.bytes(&v.to_le_bytes());
    }
    fn color(&mut self, c: Color) {
        self.bytes(&[c.r, c.g, c.b, c.a]);
    }
    fn point(&mut self, p: Point) {
        self.f32(p.x);
        self.f32(p.y);
    }
    fn size(&mut self, s: Size) {
        self.f32(s.width);
        self.f32(s.height);
    }
    fn rect(&mut self, r: &Rect) {
        self.f32(r.x);
        self.f32(r.y);
        self.f32(r.width);
        self.f32(r.height);
    }
    fn radii(&mut self, r: &Radii) {
        for c in r.corners() {
            self.size(c);
        }
    }
    fn sides(&mut self, s: &Sides) {
        for v in s.to_array() {
            self.f32(v);
        }
    }
    fn filter_op(&mut self, op: &FilterOp) {
        match *op {
            FilterOp::Blur(v) => {
                self.u8(0);
                self.f32(v);
            }
            FilterOp::Brightness(v) => {
                self.u8(1);
                self.f32(v);
            }
            FilterOp::Contrast(v) => {
                self.u8(2);
                self.f32(v);
            }
            FilterOp::Grayscale(v) => {
                self.u8(3);
                self.f32(v);
            }
            FilterOp::HueRotate(v) => {
                self.u8(4);
                self.f32(v);
            }
            FilterOp::Invert(v) => {
                self.u8(5);
                self.f32(v);
            }
            FilterOp::Opacity(v) => {
                self.u8(6);
                self.f32(v);
            }
            FilterOp::Saturate(v) => {
                self.u8(7);
                self.f32(v);
            }
            FilterOp::Sepia(v) => {
                self.u8(8);
                self.f32(v);
            }
            FilterOp::DropShadow {
                offset,
                sigma,
                color,
            } => {
                self.u8(9);
                self.point(offset);
                self.f32(sigma);
                self.color(color);
            }
            FilterOp::Svg(ref f) => {
                self.u8(10);
                self.svg_filter(f);
            }
        }
    }
    fn svg_filter(&mut self, f: &SvgFilter) {
        use crate::svg_filter::{Input, Kind, Light};
        self.rect(&f.region);
        self.u32(f.primitives.len() as u32);
        for p in &f.primitives {
            self.rect(&p.subregion);
            self.u8(p.linear_rgb as u8);
            self.u32(p.inputs.len() as u32);
            for i in &p.inputs {
                match *i {
                    Input::SourceGraphic => self.u8(0),
                    Input::SourceAlpha => self.u8(1),
                    Input::Transparent => self.u8(2),
                    Input::Result(r) => {
                        self.u8(3);
                        self.u16(r);
                    }
                }
            }
            let light = |e: &mut Self, l: &Light| match *l {
                Light::Distant { azimuth, elevation } => {
                    e.u8(0);
                    e.f32(azimuth);
                    e.f32(elevation);
                }
                Light::Point { x, y, z } => {
                    e.u8(1);
                    for v in [x, y, z] {
                        e.f32(v);
                    }
                }
                Light::Spot {
                    x,
                    y,
                    z,
                    at_x,
                    at_y,
                    at_z,
                    exponent,
                    cone,
                } => {
                    e.u8(2);
                    for v in [x, y, z, at_x, at_y, at_z, exponent] {
                        e.f32(v);
                    }
                    e.u8(cone.is_some() as u8);
                    e.f32(cone.unwrap_or(0.0));
                }
            };
            match &p.kind {
                Kind::Flood(c) => {
                    self.u8(0);
                    self.color(*c);
                }
                Kind::ColorMatrix(m) => {
                    self.u8(1);
                    for v in m.iter().flatten() {
                        self.f32(*v);
                    }
                }
                Kind::ComponentTransfer(funcs) => {
                    self.u8(2);
                    for t in funcs.iter() {
                        self.u8(t.kind as u8);
                        for v in [t.slope, t.intercept, t.amplitude, t.exponent, t.offset] {
                            self.f32(v);
                        }
                        self.u32(t.table.len() as u32);
                        for v in &t.table {
                            self.f32(*v);
                        }
                    }
                }
                Kind::GaussianBlur { x, y } => {
                    self.u8(3);
                    self.f32(*x);
                    self.f32(*y);
                }
                Kind::Offset { dx, dy } => {
                    self.u8(4);
                    self.f32(*dx);
                    self.f32(*dy);
                }
                Kind::Merge => self.u8(5),
                Kind::Blend(mode) => {
                    self.u8(6);
                    self.u8(*mode);
                }
                Kind::Composite { op, k } => {
                    self.u8(7);
                    self.u8(*op as u8);
                    for v in k {
                        self.f32(*v);
                    }
                }
                Kind::DropShadow {
                    dx,
                    dy,
                    x,
                    y,
                    color,
                } => {
                    self.u8(8);
                    for v in [*dx, *dy, *x, *y] {
                        self.f32(v);
                    }
                    self.color(*color);
                }
                Kind::Morphology { dilate, rx, ry } => {
                    self.u8(9);
                    self.u8(*dilate as u8);
                    self.f32(*rx);
                    self.f32(*ry);
                }
                Kind::Turbulence {
                    base_x,
                    base_y,
                    octaves,
                    seed,
                    stitch,
                    fractal_noise,
                } => {
                    self.u8(10);
                    self.f32(*base_x);
                    self.f32(*base_y);
                    self.u32(*octaves);
                    self.f32(*seed);
                    self.u8(*stitch as u8);
                    self.u8(*fractal_noise as u8);
                }
                Kind::Tile => self.u8(11),
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
                    self.u8(12);
                    for v in [*order_x, *order_y, *target_x, *target_y] {
                        self.u32(v);
                    }
                    self.f32(*divisor);
                    self.f32(*bias);
                    self.u8(*edge as u8);
                    self.u8(*preserve_alpha as u8);
                    self.u32(kernel.len() as u32);
                    for v in kernel {
                        self.f32(*v);
                    }
                }
                Kind::DisplacementMap { scale, x, y } => {
                    self.u8(13);
                    self.f32(*scale);
                    self.u8(*x as u8);
                    self.u8(*y as u8);
                }
                Kind::DiffuseLighting {
                    surface_scale,
                    constant,
                    color,
                    light: l,
                } => {
                    self.u8(14);
                    self.f32(*surface_scale);
                    self.f32(*constant);
                    self.color(*color);
                    light(self, l);
                }
                Kind::SpecularLighting {
                    surface_scale,
                    constant,
                    exponent,
                    color,
                    light: l,
                } => {
                    self.u8(15);
                    self.f32(*surface_scale);
                    self.f32(*constant);
                    self.f32(*exponent);
                    self.color(*color);
                    light(self, l);
                }
                Kind::Image { image, dest, src } => {
                    self.u8(16);
                    self.u32(image.0);
                    self.rect(dest);
                    self.rect(src);
                }
            }
        }
    }
    fn item(&mut self, item: &Item) {
        match item {
            Item::FillRect { rect, color } => {
                self.u8(1);
                self.rect(rect);
                self.color(*color);
            }
            Item::FillRoundedRect { rect, radii, color } => {
                self.u8(2);
                self.rect(rect);
                self.radii(radii);
                self.color(*color);
            }
            Item::StrokeRect { rect, width, color } => {
                self.u8(3);
                self.rect(rect);
                self.f32(*width);
                self.color(*color);
            }
            Item::StrokeRoundedRect {
                rect,
                radii,
                width,
                color,
            } => {
                self.u8(4);
                self.rect(rect);
                self.radii(radii);
                self.f32(*width);
                self.color(*color);
            }
            Item::Border(b) => {
                self.u8(5);
                self.rect(&b.rect);
                self.radii(&b.radii);
                self.sides(&b.widths);
                for c in b.colors {
                    self.color(c);
                }
                for s in b.styles {
                    self.u8(s as u8);
                }
            }
            Item::Text(t) => {
                self.u8(6);
                self.u32(t.glyphs.start);
                self.u32(t.glyphs.len);
                self.point(t.origin);
                self.u32(t.font.0);
                self.f32(t.size);
                self.color(t.color);
                self.u8(t.bidi_level);
                self.u8(t.sideways as u8);
                self.f32(t.stroke);
            }
            Item::Image {
                image,
                dest,
                src,
                opacity,
                rendering,
            } => {
                self.u8(7);
                self.u32(image.0);
                self.rect(dest);
                self.rect(src);
                self.f32(*opacity);
                self.u8(*rendering as u8);
            }
            Item::LinearGradient {
                rect,
                radii,
                start,
                end,
                stops,
                extend,
            } => {
                self.u8(8);
                self.rect(rect);
                self.radii(radii);
                self.point(*start);
                self.point(*end);
                self.u32(stops.start);
                self.u32(stops.len);
                self.u8(*extend as u8);
            }
            Item::RadialGradient {
                rect,
                radii,
                center,
                radius,
                stops,
                extend,
            } => {
                self.u8(9);
                self.rect(rect);
                self.radii(radii);
                self.point(*center);
                self.size(*radius);
                self.u32(stops.start);
                self.u32(stops.len);
                self.u8(*extend as u8);
            }
            Item::BoxShadow(s) => {
                self.u8(10);
                self.rect(&s.rect);
                self.radii(&s.radii);
                self.point(s.offset);
                self.f32(s.blur);
                self.f32(s.spread);
                self.color(s.color);
                self.u8(s.inset as u8);
            }
            Item::PushClipRect { rect } => {
                self.u8(11);
                self.rect(rect);
            }
            Item::PushClipRoundedRect { rect, radii } => {
                self.u8(12);
                self.rect(rect);
                self.radii(radii);
            }
            Item::PushClipPath {
                path,
                bounds,
                evenodd,
            } => {
                self.u8(29);
                self.path_range(path);
                self.rect(bounds);
                self.u8(*evenodd as u8);
            }
            Item::PopClip => self.u8(13),
            Item::PushTransform { matrix: m } => {
                self.u8(14);
                for v in [m.m11, m.m12, m.m21, m.m22, m.dx, m.dy] {
                    self.f32(v);
                }
            }
            Item::PopTransform => self.u8(15),
            Item::PushOpacity { opacity, bounds } => {
                self.u8(16);
                self.f32(*opacity);
                self.rect(bounds);
            }
            Item::PopOpacity => self.u8(17),
            Item::PushFilter { filters, bounds } => {
                self.u8(30);
                self.u32(filters.start);
                self.u32(filters.len);
                self.rect(bounds);
            }
            Item::PopFilter => self.u8(31),
            Item::BackdropFilter {
                filters,
                rect,
                radii,
                own,
            } => {
                self.u8(32);
                self.u32(filters.start);
                self.u32(filters.len);
                self.rect(rect);
                self.radii(radii);
                self.u8(*own);
            }
            Item::PushLayer { kind } => {
                self.u8(18);
                match kind {
                    LayerKind::Fixed => self.u8(0),
                    LayerKind::Sticky(c) => {
                        self.u8(1);
                        self.rect(&c.rect);
                        self.rect(&c.container);
                        for v in c.insets {
                            self.u8(v.is_some() as u8);
                            self.f32(v.unwrap_or(0.0));
                        }
                    }
                    LayerKind::Pinned => self.u8(2),
                    LayerKind::Composited(key) => {
                        self.u8(3);
                        self.u32(*key as u32);
                        self.u32((*key >> 32) as u32);
                    }
                }
            }
            Item::PopLayer => self.u8(19),
            Item::Video {
                video,
                image,
                dest,
                clip,
                transparent,
            } => {
                self.u8(23);
                self.u32(video.0);
                // (bit 0: an image; bit 1: transparent)
                self.u8(image.is_some() as u8 | (*transparent as u8) << 1);
                self.u32(image.map_or(0, |i| i.0));
                self.rect(dest);
                self.rect(clip);
            }
            Item::ProtectedVideo { player, dest, clip } => {
                self.u8(33);
                self.u32(*player as u32);
                self.u32((*player >> 32) as u32);
                self.rect(dest);
                self.rect(clip);
            }
            Item::PushMask { bounds, luminance } => {
                self.u8(20);
                self.rect(bounds);
                self.u8(*luminance as u8);
            }
            Item::MaskContent => self.u8(21),
            Item::PopMask => self.u8(22),
            Item::MaskLayer {
                composite,
                luminance,
            } => {
                self.u8(35);
                self.u8(*composite as u8);
                self.u8(*luminance as u8);
            }
            Item::ConicGradient {
                rect,
                radii,
                center,
                angle,
                stops,
                extend,
            } => {
                self.u8(24);
                self.rect(rect);
                self.radii(radii);
                self.point(*center);
                self.f32(*angle);
                self.u32(stops.start);
                self.u32(stops.len);
                self.u8(*extend as u8);
            }
            Item::PushPlus => self.u8(25),
            Item::PopPlus => self.u8(26),
            Item::FillPath {
                path,
                bounds,
                color,
                evenodd,
                antialias,
            } => {
                self.u8(27);
                self.path_range(path);
                self.rect(bounds);
                self.color(*color);
                self.u8(*evenodd as u8);
                self.u8(*antialias as u8);
            }
            Item::StrokePath {
                path,
                bounds,
                color,
                stroke,
                antialias,
            } => {
                self.u8(28);
                self.path_range(path);
                self.rect(bounds);
                self.color(*color);
                self.f32(stroke.width);
                self.u8(stroke.cap as u8);
                self.u8(stroke.join as u8);
                self.f32(stroke.miter_limit);
                self.u32(stroke.dashes.start);
                self.u32(stroke.dashes.len);
                self.f32(stroke.dash_offset);
                self.u8(*antialias as u8);
            }
            Item::FillPattern {
                rect,
                pattern,
                transform: m,
            } => {
                self.u8(34);
                self.rect(rect);
                self.u32(*pattern);
                for v in [m.m11, m.m12, m.m21, m.m22, m.dx, m.dy] {
                    self.f32(v);
                }
            }
        }
    }

    fn path_range(&mut self, r: &PathRange) {
        self.u32(r.verbs);
        self.u32(r.verbs_len);
        self.u32(r.points);
        self.u32(r.points_len);
    }
}

struct Dec<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ListError> {
        let end = self.pos.checked_add(n).ok_or(ListError::Truncated)?;
        let s = self.b.get(self.pos..end).ok_or(ListError::Truncated)?;
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, ListError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ListError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32, ListError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn f32(&mut self) -> Result<f32, ListError> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn color(&mut self) -> Result<Color, ListError> {
        let s = self.take(4)?;
        Ok(Color::rgba(s[0], s[1], s[2], s[3]))
    }
    fn point(&mut self) -> Result<Point, ListError> {
        Ok(Point::new(self.f32()?, self.f32()?))
    }
    fn size(&mut self) -> Result<Size, ListError> {
        Ok(Size::new(self.f32()?, self.f32()?))
    }
    fn rect(&mut self) -> Result<Rect, ListError> {
        Ok(Rect::new(
            self.f32()?,
            self.f32()?,
            self.f32()?,
            self.f32()?,
        ))
    }
    fn radii(&mut self) -> Result<Radii, ListError> {
        Ok(Radii {
            top_left: self.size()?,
            top_right: self.size()?,
            bottom_right: self.size()?,
            bottom_left: self.size()?,
        })
    }
    fn sides(&mut self) -> Result<Sides, ListError> {
        Ok(Sides::new(
            self.f32()?,
            self.f32()?,
            self.f32()?,
            self.f32()?,
        ))
    }
    fn flag(&mut self) -> Result<bool, ListError> {
        let at = self.pos;
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ListError::BadTag(at)),
        }
    }
    fn path_range(&mut self) -> Result<PathRange, ListError> {
        Ok(PathRange {
            verbs: self.u32()?,
            verbs_len: self.u32()?,
            points: self.u32()?,
            points_len: self.u32()?,
        })
    }
    fn extend(&mut self) -> Result<Extend, ListError> {
        let at = self.pos;
        match self.u8()? {
            0 => Ok(Extend::Clamp),
            1 => Ok(Extend::Repeat),
            _ => Err(ListError::BadTag(at)),
        }
    }
    fn filter_op(&mut self) -> Result<FilterOp, ListError> {
        let at = self.pos;
        Ok(match self.u8()? {
            0 => FilterOp::Blur(self.f32()?),
            1 => FilterOp::Brightness(self.f32()?),
            2 => FilterOp::Contrast(self.f32()?),
            3 => FilterOp::Grayscale(self.f32()?),
            4 => FilterOp::HueRotate(self.f32()?),
            5 => FilterOp::Invert(self.f32()?),
            6 => FilterOp::Opacity(self.f32()?),
            7 => FilterOp::Saturate(self.f32()?),
            8 => FilterOp::Sepia(self.f32()?),
            9 => FilterOp::DropShadow {
                offset: self.point()?,
                sigma: self.f32()?,
                color: self.color()?,
            },
            10 => FilterOp::Svg(std::sync::Arc::new(self.svg_filter()?)),
            _ => return Err(ListError::BadTag(at)),
        })
    }
    /// A count of at most `max` things.
    fn count(&mut self, max: usize) -> Result<usize, ListError> {
        let at = self.pos;
        let n = self.u32()? as usize;
        if n > max {
            return Err(ListError::BadTag(at));
        }
        Ok(n)
    }
    fn svg_light(&mut self) -> Result<crate::svg_filter::Light, ListError> {
        use crate::svg_filter::Light;
        let at = self.pos;
        Ok(match self.u8()? {
            0 => Light::Distant {
                azimuth: self.f32()?,
                elevation: self.f32()?,
            },
            1 => Light::Point {
                x: self.f32()?,
                y: self.f32()?,
                z: self.f32()?,
            },
            2 => {
                let mut v = [0.0; 7];
                for v in v.iter_mut() {
                    *v = self.f32()?;
                }
                let has_cone = self.u8()? != 0;
                let cone = self.f32()?;
                Light::Spot {
                    x: v[0],
                    y: v[1],
                    z: v[2],
                    at_x: v[3],
                    at_y: v[4],
                    at_z: v[5],
                    exponent: v[6],
                    cone: has_cone.then_some(cone),
                }
            }
            _ => return Err(ListError::BadTag(at)),
        })
    }
    fn svg_filter(&mut self) -> Result<SvgFilter, ListError> {
        use crate::svg_filter::*;
        let region = self.rect()?;
        let n = self.count(MAX_PRIMITIVES)?;
        let mut primitives = Vec::with_capacity(n);
        for _ in 0..n {
            let subregion = self.rect()?;
            let linear_rgb = self.u8()? != 0;
            let inputs_len = self.count(MAX_MERGE_INPUTS)?;
            let mut inputs = Vec::with_capacity(inputs_len);
            for _ in 0..inputs_len {
                let at = self.pos;
                inputs.push(match self.u8()? {
                    0 => Input::SourceGraphic,
                    1 => Input::SourceAlpha,
                    2 => Input::Transparent,
                    3 => Input::Result(self.u16()?),
                    _ => return Err(ListError::BadTag(at)),
                });
            }
            let at = self.pos;
            let kind = match self.u8()? {
                0 => Kind::Flood(self.color()?),
                1 => {
                    let mut m = [[0.0; 5]; 4];
                    for v in m.iter_mut().flatten() {
                        *v = self.f32()?;
                    }
                    Kind::ColorMatrix(m)
                }
                2 => {
                    let mut funcs: [Transfer; 4] = Default::default();
                    for t in funcs.iter_mut() {
                        let at = self.pos;
                        t.kind = match self.u8()? {
                            0 => TransferKind::Identity,
                            1 => TransferKind::Table,
                            2 => TransferKind::Discrete,
                            3 => TransferKind::Linear,
                            4 => TransferKind::Gamma,
                            _ => return Err(ListError::BadTag(at)),
                        };
                        t.slope = self.f32()?;
                        t.intercept = self.f32()?;
                        t.amplitude = self.f32()?;
                        t.exponent = self.f32()?;
                        t.offset = self.f32()?;
                        let n = self.count(MAX_TABLE)?;
                        for _ in 0..n {
                            t.table.push(self.f32()?);
                        }
                    }
                    Kind::ComponentTransfer(Box::new(funcs))
                }
                3 => Kind::GaussianBlur {
                    x: self.f32()?,
                    y: self.f32()?,
                },
                4 => Kind::Offset {
                    dx: self.f32()?,
                    dy: self.f32()?,
                },
                5 => Kind::Merge,
                6 => Kind::Blend(self.u8()?),
                7 => {
                    let at = self.pos;
                    let op = match self.u8()? {
                        0 => CompositeOp::Over,
                        1 => CompositeOp::In,
                        2 => CompositeOp::Out,
                        3 => CompositeOp::Atop,
                        4 => CompositeOp::Xor,
                        5 => CompositeOp::Lighter,
                        6 => CompositeOp::Arithmetic,
                        _ => return Err(ListError::BadTag(at)),
                    };
                    Kind::Composite {
                        op,
                        k: [self.f32()?, self.f32()?, self.f32()?, self.f32()?],
                    }
                }
                8 => Kind::DropShadow {
                    dx: self.f32()?,
                    dy: self.f32()?,
                    x: self.f32()?,
                    y: self.f32()?,
                    color: self.color()?,
                },
                9 => Kind::Morphology {
                    dilate: self.u8()? != 0,
                    rx: self.f32()?,
                    ry: self.f32()?,
                },
                10 => Kind::Turbulence {
                    base_x: self.f32()?,
                    base_y: self.f32()?,
                    octaves: self.u32()?,
                    seed: self.f32()?,
                    stitch: self.u8()? != 0,
                    fractal_noise: self.u8()? != 0,
                },
                11 => Kind::Tile,
                12 => {
                    let [order_x, order_y, target_x, target_y] =
                        [self.u32()?, self.u32()?, self.u32()?, self.u32()?];
                    let divisor = self.f32()?;
                    let bias = self.f32()?;
                    let at = self.pos;
                    let edge = match self.u8()? {
                        0 => EdgeMode::None,
                        1 => EdgeMode::Duplicate,
                        2 => EdgeMode::Wrap,
                        3 => EdgeMode::Mirror,
                        _ => return Err(ListError::BadTag(at)),
                    };
                    let preserve_alpha = self.u8()? != 0;
                    let n = self.count((MAX_ORDER * MAX_ORDER) as usize)?;
                    let mut kernel = Vec::with_capacity(n);
                    for _ in 0..n {
                        kernel.push(self.f32()?);
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
                    }
                }
                13 => {
                    let scale = self.f32()?;
                    let mut channel = || {
                        let at = self.pos;
                        Ok(match self.u8()? {
                            0 => Channel::R,
                            1 => Channel::G,
                            2 => Channel::B,
                            3 => Channel::A,
                            _ => return Err(ListError::BadTag(at)),
                        })
                    };
                    let x = channel()?;
                    let y = channel()?;
                    Kind::DisplacementMap { scale, x, y }
                }
                14 => Kind::DiffuseLighting {
                    surface_scale: self.f32()?,
                    constant: self.f32()?,
                    color: self.color()?,
                    light: self.svg_light()?,
                },
                15 => Kind::SpecularLighting {
                    surface_scale: self.f32()?,
                    constant: self.f32()?,
                    exponent: self.f32()?,
                    color: self.color()?,
                    light: self.svg_light()?,
                },
                16 => Kind::Image {
                    image: ImageId(self.u32()?),
                    dest: self.rect()?,
                    src: self.rect()?,
                },
                _ => return Err(ListError::BadTag(at)),
            };
            primitives.push(Primitive {
                kind,
                inputs,
                subregion,
                linear_rgb,
            });
        }
        Ok(SvgFilter { region, primitives })
    }
    fn item(&mut self) -> Result<Item, ListError> {
        let at = self.pos;
        Ok(match self.u8()? {
            1 => Item::FillRect {
                rect: self.rect()?,
                color: self.color()?,
            },
            2 => Item::FillRoundedRect {
                rect: self.rect()?,
                radii: self.radii()?,
                color: self.color()?,
            },
            3 => Item::StrokeRect {
                rect: self.rect()?,
                width: self.f32()?,
                color: self.color()?,
            },
            4 => Item::StrokeRoundedRect {
                rect: self.rect()?,
                radii: self.radii()?,
                width: self.f32()?,
                color: self.color()?,
            },
            5 => {
                let rect = self.rect()?;
                let radii = self.radii()?;
                let widths = self.sides()?;
                let colors = [self.color()?, self.color()?, self.color()?, self.color()?];
                let mut styles = [BorderStyle::None; 4];
                for s in &mut styles {
                    let p = self.pos;
                    *s = BorderStyle::from_u8(self.u8()?).ok_or(ListError::BadTag(p))?;
                }
                Item::Border(Border {
                    rect,
                    radii,
                    widths,
                    colors,
                    styles,
                })
            }
            6 => Item::Text(TextRun {
                glyphs: GlyphRange {
                    start: self.u32()?,
                    len: self.u32()?,
                },
                origin: self.point()?,
                font: FontId(self.u32()?),
                size: self.f32()?,
                color: self.color()?,
                bidi_level: self.u8()?,
                sideways: self.u8()? != 0,
                stroke: self.f32()?,
            }),
            7 => {
                let image = ImageId(self.u32()?);
                let dest = self.rect()?;
                let src = self.rect()?;
                let opacity = self.f32()?;
                let p = self.pos;
                let rendering = match self.u8()? {
                    0 => ImageRendering::Smooth,
                    1 => ImageRendering::Pixelated,
                    _ => return Err(ListError::BadTag(p)),
                };
                Item::Image {
                    image,
                    dest,
                    src,
                    opacity,
                    rendering,
                }
            }
            8 => Item::LinearGradient {
                rect: self.rect()?,
                radii: self.radii()?,
                start: self.point()?,
                end: self.point()?,
                stops: StopRange {
                    start: self.u32()?,
                    len: self.u32()?,
                },
                extend: self.extend()?,
            },
            9 => Item::RadialGradient {
                rect: self.rect()?,
                radii: self.radii()?,
                center: self.point()?,
                radius: self.size()?,
                stops: StopRange {
                    start: self.u32()?,
                    len: self.u32()?,
                },
                extend: self.extend()?,
            },
            10 => {
                let rect = self.rect()?;
                let radii = self.radii()?;
                let offset = self.point()?;
                let blur = self.f32()?;
                let spread = self.f32()?;
                let color = self.color()?;
                let p = self.pos;
                let inset = match self.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(ListError::BadTag(p)),
                };
                Item::BoxShadow(BoxShadow {
                    rect,
                    radii,
                    offset,
                    blur,
                    spread,
                    color,
                    inset,
                })
            }
            11 => Item::PushClipRect { rect: self.rect()? },
            12 => Item::PushClipRoundedRect {
                rect: self.rect()?,
                radii: self.radii()?,
            },
            13 => Item::PopClip,
            29 => Item::PushClipPath {
                path: self.path_range()?,
                bounds: self.rect()?,
                evenodd: self.flag()?,
            },
            14 => Item::PushTransform {
                matrix: Matrix::new(
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                ),
            },
            15 => Item::PopTransform,
            16 => Item::PushOpacity {
                opacity: self.f32()?,
                bounds: self.rect()?,
            },
            17 => Item::PopOpacity,
            30 => Item::PushFilter {
                filters: FilterRange {
                    start: self.u32()?,
                    len: self.u32()?,
                },
                bounds: self.rect()?,
            },
            31 => Item::PopFilter,
            32 => Item::BackdropFilter {
                filters: FilterRange {
                    start: self.u32()?,
                    len: self.u32()?,
                },
                rect: self.rect()?,
                radii: self.radii()?,
                own: self.u8()?,
            },
            18 => {
                let p = self.pos;
                let kind = match self.u8()? {
                    0 => LayerKind::Fixed,
                    1 => {
                        let rect = self.rect()?;
                        let container = self.rect()?;
                        let mut insets = [None; 4];
                        for v in &mut insets {
                            let p = self.pos;
                            let set = match self.u8()? {
                                0 => false,
                                1 => true,
                                _ => return Err(ListError::BadTag(p)),
                            };
                            let value = self.f32()?;
                            *v = set.then_some(value);
                        }
                        LayerKind::Sticky(StickyConstraint {
                            rect,
                            container,
                            insets,
                        })
                    }
                    2 => LayerKind::Pinned,
                    3 => {
                        let lo = self.u32()? as u64;
                        let hi = self.u32()? as u64;
                        LayerKind::Composited(lo | hi << 32)
                    }
                    _ => return Err(ListError::BadTag(p)),
                };
                Item::PushLayer { kind }
            }
            19 => Item::PopLayer,
            20 => {
                let bounds = self.rect()?;
                let p = self.pos;
                let luminance = match self.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(ListError::BadTag(p)),
                };
                Item::PushMask { bounds, luminance }
            }
            21 => Item::MaskContent,
            23 => {
                let video = VideoId(self.u32()?);
                let p = self.pos;
                let flags = self.u8()?;
                if flags > 3 {
                    return Err(ListError::BadTag(p));
                }
                let image = ImageId(self.u32()?);
                Item::Video {
                    video,
                    image: (flags & 1 != 0).then_some(image),
                    dest: self.rect()?,
                    clip: self.rect()?,
                    transparent: flags & 2 != 0,
                }
            }
            33 => {
                let low = u64::from(self.u32()?);
                let high = u64::from(self.u32()?);
                Item::ProtectedVideo {
                    player: low | high << 32,
                    dest: self.rect()?,
                    clip: self.rect()?,
                }
            }
            22 => Item::PopMask,
            24 => Item::ConicGradient {
                rect: self.rect()?,
                radii: self.radii()?,
                center: self.point()?,
                angle: self.f32()?,
                stops: StopRange {
                    start: self.u32()?,
                    len: self.u32()?,
                },
                extend: self.extend()?,
            },
            25 => Item::PushPlus,
            26 => Item::PopPlus,
            27 => Item::FillPath {
                path: self.path_range()?,
                bounds: self.rect()?,
                color: self.color()?,
                evenodd: self.flag()?,
                antialias: self.flag()?,
            },
            28 => {
                let path = self.path_range()?;
                let bounds = self.rect()?;
                let color = self.color()?;
                let width = self.f32()?;
                let p = self.pos;
                let cap = match self.u8()? {
                    0 => LineCap::Butt,
                    1 => LineCap::Round,
                    2 => LineCap::Square,
                    _ => return Err(ListError::BadTag(p)),
                };
                let p = self.pos;
                let join = match self.u8()? {
                    0 => LineJoin::Miter,
                    1 => LineJoin::Round,
                    2 => LineJoin::Bevel,
                    _ => return Err(ListError::BadTag(p)),
                };
                Item::StrokePath {
                    path,
                    bounds,
                    color,
                    stroke: Stroke {
                        width,
                        cap,
                        join,
                        miter_limit: self.f32()?,
                        dashes: DashRange {
                            start: self.u32()?,
                            len: self.u32()?,
                        },
                        dash_offset: self.f32()?,
                    },
                    antialias: self.flag()?,
                }
            }
            34 => Item::FillPattern {
                rect: self.rect()?,
                pattern: self.u32()?,
                transform: Matrix::new(
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                ),
            },
            35 => {
                let p = self.pos;
                let composite = *MaskComposite::ALL
                    .get(self.u8()? as usize)
                    .ok_or(ListError::BadTag(p))?;
                let p = self.pos;
                let luminance = match self.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(ListError::BadTag(p)),
                };
                Item::MaskLayer {
                    composite,
                    luminance,
                }
            }
            _ => return Err(ListError::BadTag(at)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed rectangle along the axes is a rectangle (corners either way round, the line back
    /// to the start given or left to the close); a bow tie, a slanted or curved side, an open
    /// polyline of three points or a second subpath is not.
    #[test]
    fn a_path_along_the_axes_with_four_corners_is_a_rectangle() {
        let p = Point::new;
        let rect = |els: &[PathEl]| {
            let mut b = DisplayListBuilder::new();
            let range = b.add_path(els).unwrap();
            b.build().path_rect(range)
        };
        let (m, l, c) = (PathEl::MoveTo, PathEl::LineTo, PathEl::Close);
        let want = Some(Rect::new(1.5, 2.0, 8.5, 3.25));
        assert_eq!(
            rect(&[
                m(p(1.5, 2.0)),
                l(p(10.0, 2.0)),
                l(p(10.0, 5.25)),
                l(p(1.5, 5.25)),
                c
            ]),
            want
        );
        assert_eq!(
            rect(&[
                m(p(10.0, 5.25)),
                l(p(10.0, 2.0)),
                l(p(1.5, 2.0)),
                l(p(1.5, 5.25)),
                l(p(10.0, 5.25)),
                c
            ]),
            want
        );
        assert_eq!(
            rect(&[
                m(p(0.0, 0.0)),
                l(p(10.0, 10.0)),
                l(p(10.0, 0.0)),
                l(p(0.0, 10.0)),
                c
            ]),
            None
        );
        assert_eq!(
            rect(&[
                m(p(0.0, 0.0)),
                l(p(10.0, 1.0)),
                l(p(10.0, 10.0)),
                l(p(0.0, 10.0)),
                c
            ]),
            None
        );
        assert_eq!(
            rect(&[
                m(p(0.0, 0.0)),
                PathEl::QuadTo(p(5.0, 0.0), p(10.0, 0.0)),
                l(p(10.0, 10.0)),
                l(p(0.0, 10.0)),
                c
            ]),
            None
        );
        assert_eq!(
            rect(&[m(p(0.0, 0.0)), l(p(10.0, 0.0)), l(p(10.0, 10.0)), c]),
            None
        );
        assert_eq!(
            rect(&[
                m(p(0.0, 0.0)),
                l(p(10.0, 0.0)),
                l(p(10.0, 10.0)),
                l(p(0.0, 10.0)),
                c,
                m(p(20.0, 0.0)),
                l(p(30.0, 0.0)),
                c
            ]),
            None
        );
    }

    /// A composited layer whose content reads its backdrop is painted with what is under it: it
    /// is not split off (it scrolls with the document, so nothing is pinned). A fixed one is a
    /// piece that starts with the filter, which keeps its functions.
    #[test]
    fn a_layer_with_a_backdrop_filter() {
        let layer = |kind: LayerKind, backdrop: bool| {
            let mut b = DisplayListBuilder::new();
            b.fill_rect(Rect::new(0.0, 0.0, 100.0, 100.0), Color::WHITE);
            b.push_layer(kind);
            if backdrop {
                b.backdrop_filter(
                    &[FilterOp::Blur(4.0)],
                    Rect::new(0.0, 0.0, 100.0, 20.0),
                    Radii::ZERO,
                    0,
                );
            }
            b.fill_rect(Rect::new(0.0, 0.0, 100.0, 20.0), Color::rgba(0, 0, 0, 100));
            b.pop_layer();
            b.build()
        };
        let plain = layer(LayerKind::Fixed, false).split_layers();
        assert!(!plain.pinned);
        assert_eq!(plain.layers.len(), 2);
        let split = layer(LayerKind::Composited(7), true).split_layers();
        assert!(!split.pinned);
        assert_eq!(split.layers.len(), 1);
        assert_eq!(split.layers[0].kind, None);
        assert!(split.layers[0]
            .list
            .items
            .iter()
            .any(|i| matches!(i, Item::BackdropFilter { .. })));
        assert_eq!(split.layers[0].list.filters, [FilterOp::Blur(4.0)]);
        let split = layer(LayerKind::Fixed, true).split_layers();
        assert!(!split.pinned);
        assert_eq!(split.layers.len(), 2);
        assert_eq!(split.layers[1].kind, Some(LayerKind::Fixed));
        assert!(matches!(
            split.layers[1].list.items[0],
            Item::BackdropFilter { .. }
        ));
        assert_eq!(split.layers[1].list.filters, [FilterOp::Blur(4.0)]);
    }

    /// A backdrop filter's functions and edge are checked and cross the wire.
    #[test]
    fn a_backdrop_filter_round_trips_and_bad_ones_are_refused() {
        let mut b = DisplayListBuilder::new();
        b.backdrop_filter(
            &[FilterOp::Invert(1.0), FilterOp::Blur(3.0)],
            Rect::new(1.0, 2.0, 30.0, 40.0),
            Radii::uniform(4.0),
            2,
        );
        // (nothing for an empty box, or no valid function)
        b.backdrop_filter(&[FilterOp::Blur(3.0)], Rect::ZERO, Radii::ZERO, 0);
        b.backdrop_filter(
            &[FilterOp::Blur(f32::NAN)],
            Rect::new(0.0, 0.0, 9.0, 9.0),
            Radii::ZERO,
            0,
        );
        let list = b.build();
        assert_eq!(list.items.len(), 1);
        assert!(list.validate().is_ok());
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes).unwrap(), list);
        let mut bad = list.clone();
        if let Item::BackdropFilter { filters, .. } = &mut bad.items[0] {
            filters.len = 9;
        }
        assert!(bad.validate().is_err());
        let mut bad = list.clone();
        bad.filters[1] = FilterOp::Blur(-1.0);
        assert!(bad.validate().is_err());
    }

    fn sample() -> DisplayList {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 100.0, 50.0), Color::hex(0x336699));
        b.push_clip_rounded_rect(Rect::new(5.0, 5.0, 90.0, 40.0), Radii::uniform(6.0));
        b.push_transform(Matrix::rotation(0.2));
        b.push_opacity(0.5, Rect::new(0.0, 0.0, 100.0, 50.0));
        b.text(
            &[10, 11, 12],
            &[7.0, 8.0, 9.0],
            &[GlyphOffset::default(); 3],
            Point::new(10.0, 30.0),
            FontId(2),
            16.0,
            Color::BLACK,
            0,
        );
        b.linear_gradient(
            Rect::new(0.0, 0.0, 10.0, 10.0),
            Radii::ZERO,
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            &[
                GradientStop {
                    offset: 0.0,
                    color: Color::WHITE,
                },
                GradientStop {
                    offset: 1.0,
                    color: Color::BLACK,
                },
            ],
            Extend::Repeat,
        );
        b.push_filter(
            &[
                FilterOp::Blur(2.5),
                FilterOp::Grayscale(0.5),
                FilterOp::DropShadow {
                    offset: Point::new(3.0, 4.0),
                    sigma: 1.0,
                    color: Color::hex(0x112233),
                },
                FilterOp::Svg(std::sync::Arc::new(crate::svg_filter::tests::every_kind())),
            ],
            Rect::new(-10.0, -10.0, 120.0, 70.0),
        );
        b.fill_rect(Rect::new(1.0, 1.0, 2.0, 2.0), Color::hex(0xabcdef));
        b.pop_filter();
        b.backdrop_filter(
            &[FilterOp::Blur(4.0), FilterOp::Saturate(1.5)],
            Rect::new(2.0, 3.0, 40.0, 20.0),
            Radii::uniform(5.0),
            1,
        );
        b.pop_opacity();
        b.pop_transform();
        b.border(Border {
            rect: Rect::new(0.0, 0.0, 30.0, 30.0),
            radii: Radii::uniform(4.0),
            widths: Sides::new(1.0, 2.0, 3.0, 4.0),
            colors: [
                Color::BLACK,
                Color::WHITE,
                Color::hex(0xff0000),
                Color::BLACK,
            ],
            styles: [
                BorderStyle::Solid,
                BorderStyle::Dashed,
                BorderStyle::Dotted,
                BorderStyle::Double,
            ],
        });
        b.box_shadow(BoxShadow {
            rect: Rect::new(10.0, 10.0, 20.0, 20.0),
            radii: Radii::ZERO,
            offset: Point::new(2.0, 3.0),
            blur: 4.0,
            spread: 1.0,
            color: Color::rgba(0, 0, 0, 128),
            inset: true,
        });
        b.image(
            ImageId(7),
            Rect::new(1.0, 2.0, 3.0, 4.0),
            None,
            1.0,
            ImageRendering::Pixelated,
        );
        b.fill_pattern(
            Rect::new(0.0, 0.0, 40.0, 30.0),
            Size::new(4.0, 3.0),
            Matrix::rotation(0.3),
            pattern_tile(),
        );
        // A mask of two layers, the upper one excluded from the lower, over a stroked run.
        b.push_mask(Rect::new(0.0, 0.0, 30.0, 30.0), false);
        b.fill_rect(Rect::new(0.0, 0.0, 30.0, 30.0), Color::BLACK);
        b.mask_layer(MaskComposite::Xor, true);
        b.fill_rect(Rect::new(4.0, 4.0, 22.0, 22.0), Color::WHITE);
        b.mask_content();
        assert!(b.text(
            &[13],
            &[9.0],
            &[GlyphOffset::default()],
            Point::new(2.0, 20.0),
            FontId(2),
            24.0,
            Color::hex(0x804020),
            0,
        ));
        b.last_text_stroke(1.5);
        b.pop_mask();
        b.build()
    }

    /// A pattern's tile: a square and a path in a 4 by 3 tile.
    fn pattern_tile() -> DisplayList {
        let mut t = DisplayListBuilder::new();
        t.fill_rect(Rect::new(0.0, 0.0, 2.0, 2.0), Color::hex(0x204080));
        let path = t
            .add_path(&[
                PathEl::MoveTo(Point::new(2.0, 0.0)),
                PathEl::LineTo(Point::new(4.0, 3.0)),
                PathEl::LineTo(Point::new(2.0, 3.0)),
                PathEl::Close,
            ])
            .unwrap();
        t.fill_path(path, Color::hex(0xff8000), false, true);
        t.build()
    }

    /// A pattern fill keeps its tile through encoding, appending and copying, and a tile that holds
    /// a pattern of its own, a layer or a video, or a fill naming no tile, is refused.
    #[test]
    fn pattern_tiles_travel_with_their_fills() {
        let list = sample();
        assert_eq!(list.patterns.len(), 1);
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes).as_ref(), Ok(&list));
        // every shorter prefix is refused, none panics
        for n in 0..bytes.len() {
            assert!(DisplayList::decode(&bytes[..n]).is_err());
        }
        // appended after another list, the fill names the tile at its new place
        let mut b = DisplayListBuilder::new();
        b.fill_pattern(
            Rect::new(0.0, 0.0, 8.0, 8.0),
            Size::new(1.0, 1.0),
            Matrix::IDENTITY,
            pattern_tile(),
        );
        b.append(&list);
        let both = b.build();
        assert_eq!(both.validate(), Ok(()));
        let fills: Vec<u32> = both
            .items
            .iter()
            .filter_map(|i| match i {
                Item::FillPattern { pattern, .. } => Some(*pattern),
                _ => None,
            })
            .collect();
        assert_eq!(fills, [0, 1]);
        assert_eq!(both.patterns[1], list.patterns[0]);
        // the content hash follows the tile's content, not its index
        let none = |_: ImageId| 0;
        let (mut a, mut c) = (Vec::new(), Vec::new());
        list.content_hashes(&none, &mut a);
        let mut changed = list.clone();
        changed.patterns[0].list.items[0] = Item::FillRect {
            rect: Rect::new(0.0, 0.0, 2.0, 2.0),
            color: Color::hex(0x204081),
        };
        changed.content_hashes(&none, &mut c);
        let at = list
            .items
            .iter()
            .position(|i| matches!(i, Item::FillPattern { .. }))
            .unwrap();
        assert_ne!(a[at], c[at]);
        assert_eq!(a[..at], c[..at]);
        // tiles that reach out of themselves, and fills without a tile
        let mut nested = list.clone();
        nested.patterns[0]
            .list
            .patterns
            .push(list.patterns[0].clone());
        assert!(nested.validate().is_err());
        let mut layered = list.clone();
        layered.patterns[0].list.items.insert(
            0,
            Item::PushLayer {
                kind: LayerKind::Fixed,
            },
        );
        layered.patterns[0].list.items.push(Item::PopLayer);
        assert!(layered.validate().is_err());
        let mut missing = list.clone();
        missing.patterns.clear();
        assert_eq!(missing.validate(), Err(ListError::BadRange(at)));
        let mut bad = list.clone();
        bad.patterns[0].tile.width = 0.0;
        assert!(bad.validate().is_err());
        // a tile's own encoding that holds a pattern is refused before it is read
        let mut nested_bytes = Vec::new();
        nested.encode(&mut nested_bytes);
        assert!(DisplayList::decode(&nested_bytes).is_err());
    }

    #[test]
    fn builder_closes_groups_and_ranges_index_side_vectors() {
        let list = sample();
        assert_eq!(list.validate(), Ok(()));
        // the clip opened before the transform is closed by build()
        assert_eq!(list.items.last(), Some(&Item::PopClip));
        let Item::Text(run) = list.items[4] else {
            panic!("expected text, got {:?}", list.items[4]);
        };
        assert_eq!(list.glyphs(run.glyphs).0, &[10, 11, 12]);
        assert_eq!(list.run_width(&run), 24.0);
        assert_eq!(list.stops.len(), 2);
    }

    #[test]
    fn builder_drops_invisible_items() {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::TRANSPARENT);
        b.fill_rect(Rect::new(0.0, 0.0, 0.0, 10.0), Color::BLACK);
        b.stroke_rect(Rect::new(0.0, 0.0, 10.0, 10.0), 0.0, Color::BLACK);
        b.text(&[], &[], &[], Point::ZERO, FontId(0), 12.0, Color::BLACK, 0);
        b.border(Border {
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            radii: Radii::ZERO,
            widths: Sides::all(2.0),
            colors: [Color::BLACK; 4],
            styles: [BorderStyle::None; 4],
        });
        b.fill_rounded_rect(Rect::new(0.0, 0.0, 5.0, 5.0), Radii::ZERO, Color::BLACK);
        let list = b.build();
        // only the rounded rect with zero radii survives, as a plain rect
        assert_eq!(
            list.items,
            vec![Item::FillRect {
                rect: Rect::new(0.0, 0.0, 5.0, 5.0),
                color: Color::BLACK
            }]
        );
    }

    #[test]
    #[should_panic]
    fn mismatched_pop_panics() {
        let mut b = DisplayListBuilder::new();
        b.push_clip_rect(Rect::new(0.0, 0.0, 1.0, 1.0));
        b.pop_transform();
    }

    #[test]
    fn plus_groups_and_conic_gradients_cross_the_wire() {
        let mut b = DisplayListBuilder::new();
        b.push_plus();
        b.conic_gradient(
            Rect::new(0.0, 0.0, 10.0, 10.0),
            Radii::ZERO,
            Point::new(5.0, 5.0),
            0.5,
            &[
                GradientStop {
                    offset: 0.0,
                    color: Color::WHITE,
                },
                GradientStop {
                    offset: 1.0,
                    color: Color::BLACK,
                },
            ],
            Extend::Clamp,
        );
        b.pop_plus();
        let list = b.build();
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        let back = DisplayList::decode(&bytes).expect("a list the builder made");
        assert_eq!(back.items, list.items);

        // (a group that is never closed is refused, as any other)
        let mut open = list.clone();
        open.items.pop();
        assert!(open.validate().is_err());
    }

    #[test]
    fn encode_decode_round_trips() {
        let list = sample();
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes), Ok(list));
    }

    #[test]
    fn a_protected_video_round_trips_and_a_bad_one_is_refused() {
        let mut b = DisplayListBuilder::new();
        let r = Rect::new(4.0, 8.0, 320.0, 180.0);
        b.protected_video(0x1_0000_0002, r, Rect::new(0.0, 0.0, 200.0, 100.0));
        // (nothing of it shows: no item)
        b.protected_video(3, r, Rect::new(500.0, 500.0, 10.0, 10.0));
        let list = b.build();
        assert_eq!(list.items.len(), 1);
        assert!(list.items[0].is_video() && list.items[0].is_drawing());
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes), Ok(list.clone()));
        let mut bad = list;
        bad.items[0] = Item::ProtectedVideo {
            player: 1,
            dest: Rect::new(f32::NAN, 0.0, 1.0, 1.0),
            clip: r,
        };
        let mut bytes = Vec::new();
        bad.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes), Err(ListError::BadValue(0)));
    }

    /// A canvas's frames are a transparent video item, which round-trips; a compositor draws it as
    /// a quad only where it composites as a plain draw, a video wherever it stays axis-aligned.
    #[test]
    fn a_canvas_video_is_a_quad_only_outside_groups_and_shaped_clips() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        let mut b = DisplayListBuilder::new();
        b.canvas_video(VideoId(1), r, r);
        b.push_opacity(0.5, r);
        b.canvas_video(VideoId(2), r, r);
        b.video(VideoId(3), None, r, r);
        b.pop_opacity();
        b.push_clip_rounded_rect(r, Radii::uniform(4.0));
        b.canvas_video(VideoId(4), r, r);
        b.pop_clip();
        b.push_clip_rect(r);
        b.push_transform(Matrix::translation(3.0, 4.0));
        b.canvas_video(VideoId(5), r, r);
        b.pop_transform();
        b.push_transform(Matrix::rotation(0.3));
        b.video(VideoId(6), None, r, r);
        b.pop_transform();
        b.pop_clip();
        let list = b.build();
        let ok: Vec<bool> = list.video_quads().into_iter().map(|(_, ok)| ok).collect();
        assert_eq!(ok, [true, false, true, false, true, false]);
        assert!(matches!(
            list.items[0],
            Item::Video {
                transparent: true,
                image: None,
                ..
            }
        ));
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes), Ok(list));
    }

    #[test]
    fn decoding_into_a_list_replaces_what_it_held() {
        let list = sample();
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        let mut small = DisplayListBuilder::new();
        small.fill_rect(Rect::new(0.0, 0.0, 1.0, 1.0), Color::BLACK);
        let small = small.build();
        let mut out = list.clone();
        let mut small_bytes = Vec::new();
        small.encode(&mut small_bytes);
        out.decode_from(&small_bytes).unwrap();
        assert_eq!(out, small);
        out.decode_from(&bytes).unwrap();
        assert_eq!(out, list);
        // (nothing half-decoded stays behind)
        let refused = out.decode_from(&bytes[..bytes.len() - 1]);
        assert_eq!(refused, Err(ListError::Truncated));
        assert!(out.is_empty() && out.glyph_ids.is_empty() && out.stops.is_empty());
    }

    #[test]
    fn decode_rejects_corrupt_lists() {
        let list = sample();
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(
            DisplayList::decode(&bytes[..bytes.len() - 1]),
            Err(ListError::Truncated)
        );
        assert_eq!(DisplayList::decode(b"XXXX"), Err(ListError::BadHeader));
        // a text run pointing past the glyphs
        let mut bad = list.clone();
        if let Item::Text(t) = &mut bad.items[4] {
            t.glyphs.len = 99;
        }
        let mut bytes = Vec::new();
        bad.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes), Err(ListError::BadRange(4)));
        // a pop with nothing open
        let mut bad = list.clone();
        bad.items.insert(0, Item::PopOpacity);
        assert_eq!(bad.validate(), Err(ListError::Unbalanced(0)));
        // a filter group pointing past the functions, and one with an amount out of range
        let at = list
            .items
            .iter()
            .position(|i| matches!(i, Item::PushFilter { .. }))
            .unwrap();
        let mut bad = list.clone();
        if let Item::PushFilter { filters, .. } = &mut bad.items[at] {
            filters.len = 99;
        }
        let mut bytes = Vec::new();
        bad.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes), Err(ListError::BadRange(at)));
        let mut bad = list.clone();
        bad.filters[1] = FilterOp::Grayscale(2.0);
        let mut bytes = Vec::new();
        bad.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes), Err(ListError::BadValue(at)));
        // an unknown filter function
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        let ops_at = bytes
            .windows(5)
            .position(|w| w == [0u8, 0, 0, 0x20, 0x40])
            .unwrap();
        bytes[ops_at] = 200;
        assert_eq!(DisplayList::decode(&bytes), Err(ListError::BadTag(ops_at)));
        // a NaN coordinate
        let mut bad = list;
        bad.items[0] = Item::FillRect {
            rect: Rect::new(f32::NAN, 0.0, 1.0, 1.0),
            color: Color::BLACK,
        };
        assert_eq!(bad.validate(), Err(ListError::BadValue(0)));
    }

    fn triangle_list() -> DisplayList {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 1.0, 1.0), Color::BLACK);
        let p = |x, y| Point::new(x, y);
        let path = b
            .add_path(&[
                PathEl::MoveTo(p(10.0, 10.0)),
                PathEl::LineTo(p(30.0, 10.0)),
                PathEl::QuadTo(p(35.0, 20.0), p(30.0, 30.0)),
                PathEl::CurveTo(p(20.0, 35.0), p(15.0, 35.0), p(10.0, 30.0)),
                PathEl::Close,
            ])
            .unwrap();
        b.fill_path(path, Color::hex(0x336699), true, true);
        let stroke = Stroke {
            width: 2.0,
            cap: LineCap::Round,
            join: LineJoin::Bevel,
            ..Stroke::default()
        };
        // (an odd dash list is repeated)
        b.stroke_path(path, Color::hex(0x993366), &stroke, &[4.0, 2.0, 1.0], false);
        b.build()
    }

    #[test]
    fn paths_cross_the_wire_and_keep_their_segments() {
        let list = triangle_list();
        assert_eq!(list.validate(), Ok(()));
        assert_eq!(list.paths.verbs, vec![0, 1, 2, 3, 4]);
        assert_eq!(list.paths.points.len(), 7);
        assert_eq!(list.paths.dashes, vec![4.0, 2.0, 1.0, 4.0, 2.0, 1.0]);
        let Item::StrokePath { stroke, bounds, .. } = list.items[2] else {
            panic!("{:?}", list.items[2]);
        };
        assert_eq!(stroke.dashes.len, 6);
        // (the points' bounds, out by the bevel's reach)
        assert!(bounds.x < 10.0 && bounds.right() > 35.0 && bounds.bottom() > 35.0);
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes), Ok(list.clone()));
        // appended after other paths and dashes, the ranges move and the content hashes stay
        let mut b = DisplayListBuilder::new();
        b.append(&list);
        b.append(&list);
        let twice = b.build();
        assert_eq!(twice.validate(), Ok(()));
        let (mut one, mut two) = (Vec::new(), Vec::new());
        list.content_hashes(&|_| 0, &mut one);
        twice.content_hashes(&|_| 0, &mut two);
        assert_eq!(&two[..3], &one[..]);
        assert_eq!(&two[3..], &one[..]);
        let (Item::FillPath { path: a, .. }, Item::FillPath { path: b, .. }) =
            (twice.items[4], list.items[1])
        else {
            panic!("not fills");
        };
        assert_ne!(a, b);
        assert_eq!(twice.path(a), list.path(b));
        // a split copies them too
        // (a path filled and stroked is copied for each)
        let split = list.split_layers();
        let mut parts = Vec::new();
        split.layers[0].list.content_hashes(&|_| 0, &mut parts);
        assert_eq!(split.layers[0].list.validate(), Ok(()));
        assert_eq!(parts, one);
    }

    /// A list clipped by a triangle, with a fill inside the clip.
    fn clip_path_list() -> DisplayList {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 1.0, 1.0), Color::BLACK);
        let p = |x, y| Point::new(x, y);
        let path = b
            .add_path(&[
                PathEl::MoveTo(p(10.0, 10.0)),
                PathEl::LineTo(p(50.0, 10.0)),
                PathEl::LineTo(p(10.0, 40.0)),
                PathEl::Close,
            ])
            .unwrap();
        b.push_clip_path(path, true);
        b.fill_rect(Rect::new(0.0, 0.0, 100.0, 100.0), Color::hex(0x336699));
        b.pop_clip();
        b.build()
    }

    #[test]
    fn clip_paths_cross_the_wire_and_bound_what_they_clip() {
        let list = clip_path_list();
        assert_eq!(list.validate(), Ok(()));
        let Item::PushClipPath {
            path,
            bounds,
            evenodd,
        } = list.items[1]
        else {
            panic!("{:?}", list.items[1]);
        };
        assert!(evenodd);
        assert_eq!(bounds, Rect::new(10.0, 10.0, 40.0, 30.0));
        assert_eq!(list.path(path).0, &[0, 1, 1, 4]);
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes), Ok(list.clone()));
        // the fill's bounds are cut to the path's
        let mut b = Vec::new();
        list.item_bounds(&Matrix::IDENTITY, &mut b);
        assert_eq!(b[2], Rect::new(10.0, 10.0, 40.0, 30.0));
        // appended after other paths, the range moves and the content hashes stay; a split
        // copies the path
        let mut builder = DisplayListBuilder::new();
        builder.append(&triangle_list());
        builder.append(&list);
        let both = builder.build();
        assert_eq!(both.validate(), Ok(()));
        let (mut one, mut two) = (Vec::new(), Vec::new());
        list.content_hashes(&|_| 0, &mut one);
        both.content_hashes(&|_| 0, &mut two);
        assert_eq!(&two[3..], &one[..]);
        let Item::PushClipPath { path: moved, .. } = both.items[4] else {
            panic!("{:?}", both.items[4]);
        };
        assert_ne!(moved, path);
        assert_eq!(both.path(moved), list.path(path));
        let split = both.split_layers();
        assert_eq!(split.layers[0].list.validate(), Ok(()));
        // another path hashes otherwise
        let mut other = list.clone();
        other.paths.points[1].x = 60.0;
        let mut three = Vec::new();
        other.content_hashes(&|_| 0, &mut three);
        assert_ne!(three[1], one[1]);
        // a path that is not in the list clips everything away
        let mut b = DisplayListBuilder::new();
        b.push_clip_path(path, false);
        b.pop_clip();
        let empty = b.build();
        assert_eq!(empty.items[0], Item::PushClipRect { rect: Rect::ZERO });
    }

    #[test]
    fn malformed_clip_paths_are_refused() {
        let list = clip_path_list();
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        // every truncation
        for n in 0..bytes.len() {
            assert!(DisplayList::decode(&bytes[..n]).is_err(), "{n} bytes");
        }
        // a range past the points
        let mut bad = list.clone();
        if let Item::PushClipPath { path, .. } = &mut bad.items[1] {
            path.points_len += 1;
        }
        assert_eq!(bad.validate(), Err(ListError::BadRange(1)));
        let mut bytes = Vec::new();
        bad.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes), Err(ListError::BadRange(1)));
        // NaN bounds
        let mut bad = list.clone();
        if let Item::PushClipPath { bounds, .. } = &mut bad.items[1] {
            bounds.width = f32::NAN;
        }
        assert_eq!(bad.validate(), Err(ListError::BadValue(1)));
        // a clip path closed by another kind of pop, or never closed
        let mut bad = list.clone();
        bad.items[3] = Item::PopOpacity;
        assert_eq!(bad.validate(), Err(ListError::Unbalanced(3)));
        let mut bad = list.clone();
        bad.items.pop();
        assert_eq!(bad.validate(), Err(ListError::Unclosed));
        // a fill-rule flag that is not 0 or 1
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        let mut item = Vec::new();
        Enc(&mut item).item(&list.items[1]);
        let at = bytes
            .windows(item.len())
            .position(|w| w == item.as_slice())
            .expect("the item's bytes")
            + item.len()
            - 1;
        assert_eq!(bytes[at], 1, "the evenodd flag");
        bytes[at] = 2;
        assert!(DisplayList::decode(&bytes).is_err());
    }

    #[test]
    fn malformed_paths_are_refused() {
        let list = triangle_list();
        // a range past the points
        let mut bad = list.clone();
        if let Item::FillPath { path, .. } = &mut bad.items[1] {
            path.points_len += 1;
        }
        assert_eq!(bad.validate(), Err(ListError::BadRange(1)));
        // verbs that take more points than the range has
        let mut bad = list.clone();
        bad.paths.verbs[1] = PathVerb::Cubic as u8;
        assert_eq!(bad.validate(), Err(ListError::BadRange(1)));
        // an unknown verb
        let mut bad = list.clone();
        bad.paths.verbs[4] = 9;
        assert_eq!(bad.validate(), Err(ListError::BadRange(1)));
        // an odd dash count, a negative dash, a NaN point, a negative width
        let mut bad = list.clone();
        if let Item::StrokePath { stroke, .. } = &mut bad.items[2] {
            stroke.dashes.len = 5;
        }
        assert_eq!(bad.validate(), Err(ListError::BadRange(2)));
        let mut bad = list.clone();
        bad.paths.dashes[0] = -1.0;
        assert_eq!(bad.validate(), Err(ListError::BadValue(0)));
        let mut bad = list.clone();
        bad.paths.points[3].y = f32::NAN;
        assert_eq!(bad.validate(), Err(ListError::BadValue(0)));
        let mut bad = list.clone();
        if let Item::StrokePath { stroke, .. } = &mut bad.items[2] {
            stroke.width = -1.0;
        }
        assert_eq!(bad.validate(), Err(ListError::BadValue(2)));
        // a bad cap in the encoding
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        let at = bytes.len() - 1 - 4 - 8 - 4 - 2;
        assert_eq!(bytes[at], LineCap::Round as u8);
        bytes[at] = 7;
        assert_eq!(DisplayList::decode(&bytes), Err(ListError::BadTag(at)));
    }

    #[test]
    fn images_are_replaced_by_lists_clipped_to_them() {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 5.0, 5.0), Color::BLACK);
        let dest = Rect::new(10.0, 20.0, 30.0, 40.0);
        b.image(ImageId(7), dest, None, 0.5, ImageRendering::Smooth);
        // (a part of an image is left as it is)
        let part = Rect::new(0.0, 0.0, 2.0, 2.0);
        b.image(ImageId(7), dest, Some(part), 1.0, ImageRendering::Smooth);
        b.image(ImageId(8), dest, None, 1.0, ImageRendering::Smooth);
        let page = b.build();
        let content = triangle_list();
        let m = Matrix::translation(10.0, 20.0);
        let mut asked = Vec::new();
        let out = page
            .replace_images(&mut |id, d| {
                asked.push((id, d));
                (id == ImageId(7)).then_some((&content, m, ClipAxes::BOTH))
            })
            .unwrap();
        assert_eq!(asked, vec![(ImageId(7), dest), (ImageId(8), dest)]);
        assert_eq!(out.validate(), Ok(()));
        let kinds: Vec<&Item> = out.items.iter().collect();
        assert_eq!(kinds[0], &page.items[0]);
        assert_eq!(kinds[1], &Item::PushClipRect { rect: dest });
        assert!(matches!(kinds[2], Item::PushOpacity { opacity, .. } if *opacity == 0.5));
        assert_eq!(kinds[3], &Item::PushTransform { matrix: m });
        let n = content.items.len();
        assert_eq!(kinds[4 + n], &Item::PopTransform);
        assert_eq!(kinds[5 + n], &Item::PopOpacity);
        assert_eq!(kinds[6 + n], &Item::PopClip);
        assert_eq!(&out.items[7 + n..], &page.items[2..]);
        // the content draws what it drew
        let (mut a, mut b2) = (Vec::new(), Vec::new());
        content.content_hashes(&|_| 0, &mut a);
        out.content_hashes(&|_| 0, &mut b2);
        assert_eq!(&b2[4..4 + n], &a[..]);
        // nothing to replace: no list
        assert!(page.replace_images(&mut |_, _| None).is_none());
    }

    /// A replacement that does not clip on an axis (a replaced element with `overflow: visible`
    /// there) is clipped to what it draws on that axis, past the image's `dest`.
    #[test]
    fn images_replaced_without_a_clip_on_an_axis_draw_past_them() {
        let mut b = DisplayListBuilder::new();
        let dest = Rect::new(10.0, 20.0, 30.0, 40.0);
        b.image(ImageId(7), dest, None, 1.0, ImageRendering::Smooth);
        let page = b.build();
        // (content from 5 above to 5 below the box, and 50 to its right)
        let mut c = DisplayListBuilder::new();
        c.fill_rect(Rect::new(0.0, -5.0, 80.0, 50.0), Color::BLACK);
        let content = c.build();
        let m = Matrix::translation(10.0, 20.0);
        let clip = |axes| {
            let out = page
                .replace_images(&mut |_, _| Some((&content, m, axes)))
                .unwrap();
            match out.items[0] {
                Item::PushClipRect { rect } => rect,
                ref i => panic!("{i:?}"),
            }
        };
        // (what the content draws, from 10 to 90 across and 15 to 65 down, with its margin)
        let d = content.drawn_bounds(&m);
        assert!(d.x <= 10.0 && d.right() >= 90.0 && d.y <= 15.0 && d.bottom() >= 65.0);
        assert_eq!(clip(ClipAxes::BOTH), dest);
        assert_eq!(
            clip(ClipAxes { x: false, y: true }),
            Rect::new(d.x, 20.0, d.width, 40.0)
        );
        assert_eq!(
            clip(ClipAxes { x: true, y: false }),
            Rect::new(10.0, d.y, 30.0, d.height)
        );
        assert_eq!(clip(ClipAxes { x: false, y: false }), d);
    }

    #[test]
    fn path_builder_drops_what_paints_nothing() {
        let mut b = DisplayListBuilder::new();
        let p = |x, y| Point::new(x, y);
        assert!(b
            .add_path(&[PathEl::MoveTo(p(f32::INFINITY, 0.0))])
            .is_none());
        let line = b
            .add_path(&[PathEl::MoveTo(p(0.0, 0.0)), PathEl::LineTo(p(10.0, 0.0))])
            .unwrap();
        // a line has no area to fill, a zero width no stroke
        b.fill_path(line, Color::BLACK, false, true);
        let none = Stroke {
            width: 0.0,
            ..Stroke::default()
        };
        b.stroke_path(line, Color::BLACK, &none, &[], true);
        b.stroke_path(line, Color::TRANSPARENT, &Stroke::default(), &[], true);
        // all-zero dashes are a solid stroke
        b.stroke_path(line, Color::BLACK, &Stroke::default(), &[0.0, 0.0], true);
        let list = b.build();
        assert_eq!(list.items.len(), 1);
        assert!(list.paths.dashes.is_empty());
    }

    #[test]
    fn bounds_follow_transforms_and_clips() {
        let mut b = DisplayListBuilder::new();
        b.push_transform(Matrix::translation(100.0, 0.0));
        b.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::BLACK);
        b.push_clip_rect(Rect::new(0.0, 0.0, 5.0, 5.0));
        b.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::BLACK);
        b.pop_clip();
        b.pop_transform();
        b.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::BLACK);
        let list = b.build();
        let mut bounds = Vec::new();
        list.item_bounds(&Matrix::scale(2.0, 2.0), &mut bounds);
        assert_eq!(bounds.len(), list.len());
        assert_eq!(bounds[1], Rect::new(199.0, -1.0, 22.0, 22.0));
        assert_eq!(bounds[3], Rect::new(200.0, 0.0, 10.0, 10.0));
        assert_eq!(bounds[6], Rect::new(-1.0, -1.0, 22.0, 22.0));
        assert!(bounds[0].is_empty());
    }

    #[test]
    fn image_bounds_report_clips_and_layers() {
        let smooth = ImageRendering::Smooth;
        let mut b = DisplayListBuilder::new();
        b.image(
            ImageId(1),
            Rect::new(0.0, 0.0, 10.0, 10.0),
            None,
            1.0,
            smooth,
        );
        b.push_clip_rect(Rect::new(100.0, 100.0, 5.0, 5.0));
        b.image(
            ImageId(1),
            Rect::new(0.0, 0.0, 10.0, 10.0),
            None,
            1.0,
            smooth,
        );
        b.pop_clip();
        b.push_layer(LayerKind::Fixed);
        b.image(
            ImageId(2),
            Rect::new(20.0, 0.0, 10.0, 10.0),
            None,
            1.0,
            smooth,
        );
        b.pop_layer();
        b.image(
            ImageId(3),
            Rect::new(0.0, 0.0, 10.0, 10.0),
            None,
            1.0,
            smooth,
        );
        let list = b.build();
        let mut out = Vec::new();
        list.image_bounds(&|id| id != ImageId(3), &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0],
            (ImageId(1), Rect::new(-1.0, -1.0, 12.0, 12.0), false)
        );
        // (clipped away)
        assert!(out[1].1.is_empty());
        assert_eq!(out[2].0, ImageId(2));
        assert!(out[2].2);
        list.image_bounds(&|id| id == ImageId(9), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn reuse_keeps_capacity() {
        let list = sample();
        let cap = list.items.capacity();
        let b = DisplayListBuilder::reuse(list);
        assert!(b.list().is_empty());
        assert_eq!(b.list().items.capacity(), cap);
    }

    #[test]
    fn appended_lists_keep_their_glyphs_and_stops() {
        let inner = sample();
        let mut b = DisplayListBuilder::new();
        b.text(
            &[1],
            &[4.0],
            &[GlyphOffset::default()],
            Point::ZERO,
            FontId(1),
            10.0,
            Color::BLACK,
            0,
        );
        b.push_clip_rect(Rect::new(0.0, 0.0, 50.0, 50.0));
        b.append(&inner);
        b.pop_clip();
        let list = b.build();
        assert!(list.validate().is_ok());
        let runs: Vec<&TextRun> = list
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Text(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 3);
        assert_eq!(list.glyphs(runs[1].glyphs).0, &[10, 11, 12]);
        assert_eq!(list.glyphs(runs[2].glyphs).0, &[13]);
        assert_eq!(runs[2].stroke, 1.5);
        let grad = list.items.iter().find_map(|i| match i {
            Item::LinearGradient { stops, .. } => Some(*stops),
            _ => None,
        });
        assert_eq!(list.stops(grad.unwrap()), inner.stops.as_slice());
        // A malformed list is left out.
        let mut bad = DisplayList::new();
        bad.items.push(Item::PopClip);
        let mut b = DisplayListBuilder::new();
        b.append(&bad);
        assert!(b.build().is_empty());
    }

    #[test]
    fn a_sticky_box_moves_only_with_room_in_its_container() {
        let fills = StickyConstraint {
            rect: Rect::new(0.0, 0.0, 200.0, 64.0),
            container: Rect::new(0.0, 0.0, 200.0, 64.0),
            insets: [Some(0.0), Some(0.0), Some(0.0), Some(0.0)],
        };
        assert!(!fills.moves());
        for y in [-1000.0, 0.0, 30.0, 1000.0] {
            assert_eq!(fills.offset(Rect::new(0.0, y, 100.0, 50.0)), Point::ZERO);
        }
        // Room below it: `top` can move it down, `bottom` cannot move it up.
        let below = StickyConstraint {
            container: Rect::new(0.0, 0.0, 200.0, 100.0),
            ..fills
        };
        assert!(below.moves());
        let bottom_only = StickyConstraint {
            insets: [None, None, Some(0.0), None],
            ..below
        };
        assert!(!bottom_only.moves());
        let left_only = StickyConstraint {
            insets: [None, None, None, Some(0.0)],
            container: Rect::new(0.0, 0.0, 300.0, 64.0),
            ..fills
        };
        assert!(left_only.moves());
    }

    #[test]
    fn sticky_offsets_keep_the_box_in_view_and_in_its_container() {
        let c = StickyConstraint {
            rect: Rect::new(0.0, 100.0, 50.0, 20.0),
            container: Rect::new(0.0, 0.0, 200.0, 300.0),
            insets: [Some(10.0), None, None, None],
        };
        let view = |y: f32| Rect::new(0.0, y, 200.0, 100.0);
        // Not reached yet, then stuck 10px below the scrollport's top, then pushed out by the
        // container's end (it ends at 300: the box's bottom can't pass it).
        assert_eq!(c.offset(view(0.0)), Point::ZERO);
        assert_eq!(c.offset(view(150.0)), Point::new(0.0, 60.0));
        assert_eq!(c.offset(view(400.0)), Point::new(0.0, 180.0));
        // `bottom`: kept above the scrollport's bottom, not above the container's start.
        let b = StickyConstraint {
            insets: [None, None, Some(0.0), None],
            ..c
        };
        assert_eq!(b.offset(view(30.0)), Point::ZERO);
        assert_eq!(b.offset(view(-50.0)), Point::new(0.0, -70.0));
        assert_eq!(b.offset(view(-500.0)), Point::new(0.0, -100.0));
        // Both: `top` wins.
        let both = StickyConstraint {
            insets: [Some(0.0), None, Some(0.0), None],
            rect: Rect::new(0.0, 100.0, 50.0, 150.0),
            ..c
        };
        assert_eq!(both.offset(view(120.0)), Point::new(0.0, 20.0));
    }

    #[test]
    fn layers_split_off_with_their_glyphs_and_encode() {
        let mut b = DisplayListBuilder::new();
        let text = |b: &mut DisplayListBuilder, g: u16| {
            b.text(
                &[g],
                &[5.0],
                &[GlyphOffset::default()],
                Point::new(1.0, 10.0),
                FontId(1),
                12.0,
                Color::BLACK,
                0,
            )
        };
        b.fill_rect(Rect::new(0.0, 0.0, 100.0, 100.0), Color::WHITE);
        text(&mut b, 1);
        b.push_layer(LayerKind::Fixed);
        text(&mut b, 2);
        b.pop_layer();
        let sticky = StickyConstraint {
            rect: Rect::new(0.0, 40.0, 100.0, 10.0),
            container: Rect::new(0.0, 0.0, 100.0, 500.0),
            insets: [Some(0.0), None, None, Some(2.5)],
        };
        b.push_layer(LayerKind::Sticky(sticky));
        b.fill_rect(Rect::new(0.0, 40.0, 100.0, 10.0), Color::BLACK);
        b.pop_layer();
        text(&mut b, 3);
        // A layer in a clip can't be split off: it stays, and the list is pinned.
        b.push_clip_rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        b.push_layer(LayerKind::Fixed);
        text(&mut b, 4);
        b.pop_layer();
        b.pop_clip();
        let list = b.build();
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes).unwrap(), list);

        let split = list.split_layers();
        assert!(split.pinned);
        let kinds: Vec<_> = split.layers.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                None,
                Some(LayerKind::Fixed),
                Some(LayerKind::Sticky(sticky)),
                None
            ]
        );
        let glyphs = |l: &DisplayList| -> Vec<u16> {
            l.items
                .iter()
                .filter_map(|i| match i {
                    Item::Text(r) => Some(l.glyphs(r.glyphs).0[0]),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(glyphs(&split.layers[0].list), [1]);
        assert_eq!(glyphs(&split.layers[1].list), [2]);
        assert_eq!(glyphs(&split.layers[3].list), [3, 4]);
        for l in split.layers.iter().map(|p| &p.list) {
            assert!(l.validate().is_ok());
        }
        // Nothing to split: one piece, not pinned.
        let mut plain = DisplayListBuilder::new();
        plain.fill_rect(Rect::new(0.0, 0.0, 1.0, 1.0), Color::BLACK);
        let split = plain.build().split_layers();
        assert_eq!(split.layers.len(), 1);
        assert!(!split.pinned);
    }

    /// A spinner's shape: a rotating box, a clip in it, a rotating box in that.
    fn spinner(outer: f32, inner: f32) -> DisplayList {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 100.0, 100.0), Color::WHITE);
        b.push_transform(Matrix::rotation_about(outer, Point::new(50.0, 50.0)));
        b.push_layer(LayerKind::Composited(1 << 40 | 7));
        b.fill_rect(Rect::new(20.0, 20.0, 5.0, 5.0), Color::BLACK);
        b.push_clip_rect(Rect::new(20.0, 20.0, 30.0, 60.0));
        b.push_transform(Matrix::rotation_about(inner, Point::new(50.0, 50.0)));
        b.push_layer(LayerKind::Composited(8));
        b.stroke_rect(Rect::new(20.0, 20.0, 60.0, 60.0), 6.0, Color::hex(0xdddddd));
        b.pop_layer();
        b.pop_transform();
        b.pop_clip();
        b.fill_rect(Rect::new(70.0, 70.0, 5.0, 5.0), Color::BLACK);
        b.pop_layer();
        b.pop_transform();
        b.fill_rect(Rect::new(0.0, 90.0, 100.0, 10.0), Color::hex(0x336699));
        b.build()
    }

    #[test]
    fn composited_groups_split_off_with_their_state() {
        let list = spinner(0.5, -1.0);
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes).as_ref(), Ok(&list));
        let split = list.split_layers();
        assert!(!split.pinned);
        let kinds: Vec<_> = split.layers.iter().map(|l| l.kind).collect();
        let (c1, c2) = (
            Some(LayerKind::Composited(1 << 40 | 7)),
            Some(LayerKind::Composited(8)),
        );
        assert_eq!(kinds, [None, c1, c2, c1, None]);
        let t = |a: f32| {
            let m = Matrix::rotation_about(a, Point::new(50.0, 50.0));
            StateOp::Transform([m.m11, m.m12, m.m21, m.m22, m.dx, m.dy])
        };
        let clip = StateOp::Clip(Rect::new(20.0, 20.0, 30.0, 60.0));
        let states: Vec<_> = split.layers.iter().map(|l| l.state.0.clone()).collect();
        assert_eq!(
            states,
            [
                vec![],
                vec![t(0.5)],
                vec![t(0.5), clip, t(-1.0)],
                vec![t(0.5)],
                vec![]
            ]
        );
        // Each piece draws what it held, its own groups (opened again after a split) balanced.
        let drawn: Vec<usize> = split
            .layers
            .iter()
            .map(|l| l.list.items.iter().filter(|i| i.is_drawing()).count())
            .collect();
        assert_eq!(drawn, [1, 1, 1, 1, 1]);
        for l in &split.layers {
            assert_eq!(l.list.validate(), Ok(()));
            assert!(l.state.is_valid());
        }
        // The resumed pieces open the groups they sat in again, and drop them when they hold
        // nothing: the outer box's last rectangle alone, the page's last one alone.
        assert!(matches!(
            split.layers[3].list.items[..],
            [Item::FillRect { .. }]
        ));
        assert!(matches!(
            split.layers[4].list.items[..],
            [Item::FillRect { .. }]
        ));
        // The state maps the piece's coordinates to the list's.
        let m = split.layers[2].state.transform();
        let expect = Matrix::rotation_about(-1.0, Point::new(50.0, 50.0))
            .then(&Matrix::rotation_about(0.5, Point::new(50.0, 50.0)));
        let p = Point::new(20.0, 30.0);
        let (a, b) = (m.apply(p), expect.apply(p));
        assert!((a.x - b.x).abs() < 1e-3 && (a.y - b.y).abs() < 1e-3);
    }

    #[test]
    fn a_frame_that_only_turns_a_composited_box_leaves_every_piece_list_as_it_was() {
        let a = spinner(0.5, -1.0).split_layers();
        let b = spinner(2.0, 0.25).split_layers();
        assert_eq!(a.layers.len(), b.layers.len());
        for (x, y) in a.layers.iter().zip(&b.layers) {
            assert_eq!(x.list, y.list);
            assert_eq!(x.kind, y.kind);
        }
        assert_ne!(a.layers[2].state, b.layers[2].state);
    }

    #[test]
    fn composited_groups_stay_in_place_where_a_state_cannot_describe_them() {
        let composited = |b: &mut DisplayListBuilder| {
            b.push_layer(LayerKind::Composited(3));
            b.fill_rect(Rect::new(0.0, 0.0, 5.0, 5.0), Color::BLACK);
            b.pop_layer();
        };
        let pieces = |b: DisplayListBuilder| {
            let split = b.build().split_layers();
            split.layers.iter().map(|l| l.kind).collect::<Vec<_>>()
        };
        // Its own opacity: split off, the opacity in its state.
        let mut b = DisplayListBuilder::new();
        b.push_opacity(0.5, Rect::new(0.0, 0.0, 5.0, 5.0));
        composited(&mut b);
        b.pop_opacity();
        let split = b.build().split_layers();
        assert_eq!(split.layers[0].state.0, [StateOp::Opacity(0.5)]);
        assert_eq!(split.layers[0].state.opacity(), 0.5);
        // Faded out entirely: nothing to draw, no piece.
        let mut b = DisplayListBuilder::new();
        b.push_opacity(0.0, Rect::new(0.0, 0.0, 5.0, 5.0));
        composited(&mut b);
        b.pop_opacity();
        assert!(b.build().split_layers().layers.is_empty());
        // An opacity group around more than it: in place.
        let mut b = DisplayListBuilder::new();
        b.push_opacity(0.5, Rect::new(0.0, 0.0, 9.0, 9.0));
        b.fill_rect(Rect::new(6.0, 6.0, 3.0, 3.0), Color::BLACK);
        composited(&mut b);
        b.pop_opacity();
        assert_eq!(pieces(b), [None]);
        let mut b = DisplayListBuilder::new();
        b.push_opacity(0.5, Rect::new(0.0, 0.0, 9.0, 9.0));
        composited(&mut b);
        b.fill_rect(Rect::new(6.0, 6.0, 3.0, 3.0), Color::BLACK);
        b.pop_opacity();
        assert_eq!(pieces(b), [None]);
        // A mask, a clip path, a plus-lighter group, a pinned layer, a fixed layer: in place.
        let mut b = DisplayListBuilder::new();
        b.push_mask(Rect::new(0.0, 0.0, 9.0, 9.0), false);
        b.fill_rect(Rect::new(0.0, 0.0, 9.0, 9.0), Color::BLACK);
        b.mask_content();
        composited(&mut b);
        b.pop_mask();
        assert_eq!(pieces(b), [None]);
        let mut b = DisplayListBuilder::new();
        let path = b
            .add_path(&[
                PathEl::MoveTo(Point::new(0.0, 0.0)),
                PathEl::LineTo(Point::new(9.0, 0.0)),
                PathEl::LineTo(Point::new(0.0, 9.0)),
                PathEl::Close,
            ])
            .unwrap();
        b.push_clip_path(path, false);
        composited(&mut b);
        b.pop_clip();
        assert_eq!(pieces(b), [None]);
        let mut b = DisplayListBuilder::new();
        b.push_plus();
        composited(&mut b);
        b.pop_plus();
        assert_eq!(pieces(b), [None]);
        let mut b = DisplayListBuilder::new();
        b.push_layer(LayerKind::Pinned);
        composited(&mut b);
        b.pop_layer();
        let split = b.build().split_layers();
        assert!(split.pinned);
        assert_eq!(split.layers.len(), 1);
        assert_eq!(split.layers[0].kind, None);
        let mut b = DisplayListBuilder::new();
        b.push_layer(LayerKind::Fixed);
        composited(&mut b);
        b.pop_layer();
        assert_eq!(pieces(b), [Some(LayerKind::Fixed)]);
        // A fixed box in a composited one that nothing clips, moves or fades: split off too.
        let mut b = DisplayListBuilder::new();
        b.push_layer(LayerKind::Composited(3));
        b.fill_rect(Rect::new(0.0, 0.0, 5.0, 5.0), Color::BLACK);
        b.push_layer(LayerKind::Fixed);
        b.fill_rect(Rect::new(0.0, 0.0, 5.0, 5.0), Color::WHITE);
        b.pop_layer();
        b.fill_rect(Rect::new(5.0, 0.0, 5.0, 5.0), Color::BLACK);
        b.pop_layer();
        let split = b.build().split_layers();
        assert!(!split.pinned);
        let kinds: Vec<_> = split.layers.iter().map(|l| l.kind).collect();
        let c = Some(LayerKind::Composited(3));
        assert_eq!(kinds, [c, Some(LayerKind::Fixed), c]);
        // A video in it: in place (the compositor draws videos as painted).
        let mut b = DisplayListBuilder::new();
        b.push_layer(LayerKind::Composited(3));
        b.video(
            VideoId(1),
            None,
            Rect::new(0.0, 0.0, 9.0, 9.0),
            Rect::new(0.0, 0.0, 9.0, 9.0),
        );
        b.pop_layer();
        assert_eq!(pieces(b), [None]);
        // Under a rounded clip and a transform: split off.
        let mut b = DisplayListBuilder::new();
        b.push_clip_rounded_rect(Rect::new(0.0, 0.0, 9.0, 9.0), Radii::uniform(2.0));
        b.push_transform(Matrix::translation(1.0, 2.0));
        composited(&mut b);
        b.pop_transform();
        b.fill_rect(Rect::new(6.0, 6.0, 3.0, 3.0), Color::BLACK);
        b.pop_clip();
        let split = b.build().split_layers();
        assert_eq!(split.layers.len(), 2);
        // (what follows it in the clip is still clipped)
        assert!(matches!(
            split.layers[1].list.items[..],
            [
                Item::PushClipRoundedRect { .. },
                Item::FillRect { .. },
                Item::PopClip
            ]
        ));
        assert_eq!(
            split.layers[0].state.0,
            [
                StateOp::ClipRounded(Rect::new(0.0, 0.0, 9.0, 9.0), [2.0; 8]),
                StateOp::Transform([1.0, 0.0, 0.0, 1.0, 1.0, 2.0])
            ]
        );
    }

    #[test]
    fn content_after_a_composited_box_joins_the_page_unless_it_overlaps_the_box() {
        let page = |after: Rect| {
            let mut b = DisplayListBuilder::new();
            b.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::WHITE);
            b.push_transform(Matrix::translation(50.0, 0.0));
            b.push_layer(LayerKind::Composited(5));
            b.fill_rect(Rect::new(0.0, 0.0, 20.0, 20.0), Color::BLACK);
            b.pop_layer();
            b.pop_transform();
            b.fill_rect(after, Color::hex(0x336699));
            b.build().split_layers()
        };
        let apart = page(Rect::new(0.0, 40.0, 10.0, 10.0));
        let kinds: Vec<_> = apart.layers.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [None, Some(LayerKind::Composited(5))]);
        let drawn = |l: &DisplayList| l.items.iter().filter(|i| i.is_drawing()).count();
        assert_eq!(drawn(&apart.layers[0].list), 2);
        assert_eq!(apart.layers[0].list.validate(), Ok(()));
        // (the box is at 50..70 in the list, through its transform)
        let over = page(Rect::new(60.0, 10.0, 10.0, 10.0));
        let kinds: Vec<_> = over.layers.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [None, Some(LayerKind::Composited(5)), None]);
        // A fixed layer in between moves: nothing merges across it.
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::WHITE);
        b.push_layer(LayerKind::Fixed);
        b.fill_rect(Rect::new(0.0, 0.0, 5.0, 5.0), Color::BLACK);
        b.pop_layer();
        b.fill_rect(Rect::new(0.0, 40.0, 10.0, 10.0), Color::WHITE);
        let kinds: Vec<_> = b
            .build()
            .split_layers()
            .layers
            .iter()
            .map(|l| l.kind)
            .collect();
        assert_eq!(kinds, [None, Some(LayerKind::Fixed), None]);
    }

    /// A composited box's blur reaches past its content (its filter group's bounds): content after
    /// it under that reach is painted over the blur, so it does not join the page under the box.
    #[test]
    fn content_under_a_composited_boxs_blur_does_not_join_the_page() {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::WHITE);
        b.push_layer(LayerKind::Composited(5));
        b.push_filter(&[FilterOp::Blur(10.0)], Rect::new(20.0, 0.0, 80.0, 80.0));
        b.fill_rect(Rect::new(50.0, 30.0, 20.0, 20.0), Color::BLACK);
        b.pop_filter();
        b.pop_layer();
        // (outside the box's content, inside its blur)
        b.fill_rect(Rect::new(30.0, 30.0, 10.0, 10.0), Color::hex(0x336699));
        let split = b.build().split_layers();
        let kinds: Vec<_> = split.layers.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [None, Some(LayerKind::Composited(5)), None]);
        // The box's piece covers its blur.
        let reach = split.layers[1].list.drawn_bounds(&Matrix::IDENTITY);
        let blur = Rect::new(20.0, 0.0, 80.0, 80.0);
        assert_eq!(reach.union(&blur), reach, "{reach:?}");
    }

    #[test]
    fn layer_states_refuse_what_cannot_be_drawn() {
        assert!(LayerState(vec![StateOp::Opacity(1.0)]).is_valid());
        assert!(!LayerState(vec![StateOp::Opacity(1.5)]).is_valid());
        assert!(!LayerState(vec![StateOp::Opacity(f32::NAN)]).is_valid());
        assert!(!LayerState(vec![StateOp::Transform([
            1.0,
            0.0,
            0.0,
            f32::INFINITY,
            0.0,
            0.0
        ])])
        .is_valid());
        assert!(!LayerState(vec![StateOp::Clip(Rect::new(f32::NAN, 0.0, 1.0, 1.0))]).is_valid());
        assert!(!LayerState(vec![StateOp::ClipRounded(
            Rect::new(0.0, 0.0, 1.0, 1.0),
            [-1.0; 8]
        )])
        .is_valid());
        assert!(!LayerState(vec![StateOp::Opacity(1.0); LayerState::MAX_OPS + 1]).is_valid());
        // A composited layer's key crosses whole; a truncated one is refused.
        let mut b = DisplayListBuilder::new();
        b.push_layer(LayerKind::Composited(u64::MAX - 1));
        b.fill_rect(Rect::new(0.0, 0.0, 1.0, 1.0), Color::BLACK);
        b.pop_layer();
        let list = b.build();
        let mut bytes = Vec::new();
        list.encode(&mut bytes);
        assert_eq!(DisplayList::decode(&bytes).as_ref(), Ok(&list));
        for n in 0..bytes.len() {
            assert!(DisplayList::decode(&bytes[..n]).is_err());
        }
    }
}

#[cfg(test)]
mod backdrop_split_tests {
    use super::*;

    const BOX: Rect = Rect::new(10.0, 10.0, 300.0, 40.0);

    /// A page, then a fixed layer: something of its own, then a box with a backdrop filter in the
    /// box's own opacity group (inside `inner`, a clip, when asked), then the page again.
    fn bar(inner_clip: bool, opacity: f32) -> DisplayList {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 800.0, 3000.0), Color::rgb(0, 0, 255));
        b.push_layer(LayerKind::Fixed);
        b.fill_rect(Rect::new(0.0, 0.0, 800.0, 5.0), Color::rgb(1, 1, 1));
        if inner_clip {
            b.push_clip_rect(Rect::new(0.0, 0.0, 400.0, 60.0));
        }
        b.push_opacity(opacity, BOX);
        b.backdrop_filter(&[FilterOp::Blur(16.0)], BOX, Radii::ZERO, 1);
        b.fill_rect(BOX, Color::rgba(5, 11, 26, 190));
        b.pop_opacity();
        if inner_clip {
            b.pop_clip();
        }
        b.pop_layer();
        b.fill_rect(Rect::new(0.0, 3000.0, 800.0, 10.0), Color::rgb(2, 2, 2));
        b.build()
    }

    fn has_backdrop(l: &DisplayList) -> bool {
        l.items
            .iter()
            .any(|i| matches!(i, Item::BackdropFilter { .. }))
    }

    /// A fixed bar with a backdrop filter is a layer a compositor can keep in place: it
    /// is cut at the filter, which the compositor applies to what it has drawn under the piece.
    /// (It was left in the page's list, which then had to be painted again at every scroll
    /// position: the bar trailed its place while the page scrolled.)
    #[test]
    fn a_fixed_layer_is_cut_at_a_backdrop_filter_that_reads_under_it() {
        let mut split = bar(false, 0.5).split_layers();
        assert!(!split.pinned);
        let kinds: Vec<_> = split.layers.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [None, Some(LayerKind::Fixed), Some(LayerKind::Fixed), None]
        );
        // What the layer paints before the box is a piece without the filter.
        assert!(!has_backdrop(&split.layers[1].list));
        // The piece with the box starts with the filter, inside the box's own opacity.
        let piece = &mut split.layers[2].list;
        assert!(matches!(
            piece.items[..2],
            [Item::PushOpacity { .. }, Item::BackdropFilter { .. }]
        ));
        assert!(piece.validate().is_ok());
        let taken = piece.take_leading_backdrop().expect("the filter");
        assert_eq!(taken.filters, [FilterOp::Blur(16.0)]);
        assert_eq!((taken.rect, taken.opacity), (BOX, 0.5));
        // The piece's own raster is drawn without it, and still has the box's paint.
        assert!(!has_backdrop(piece));
        assert!(piece.validate().is_ok());
        assert!(piece
            .items
            .iter()
            .any(|i| matches!(i, Item::FillRect { rect, .. } if *rect == BOX)));
        assert_eq!(piece.take_leading_backdrop(), None);
        // The page's own pieces have none to take.
        assert_eq!(split.layers[0].list.take_leading_backdrop(), None);
    }

    /// Under a rectangle clip and a translation of its layer, the filter is drawn through them: the
    /// layer is cut there too, and the backdrop's rectangle is moved by the transform and cut by
    /// the clip (a cut one loses its corners). Under a rounded clip, which no backdrop rectangle
    /// can take, the layer stays in the list, which is painted at each scroll position.
    #[test]
    fn a_backdrop_filter_under_a_clip_of_its_layer_is_drawn_through_it() {
        let mut split = bar(true, 1.0).split_layers();
        assert!(!split.pinned);
        let taken = split.layers[2]
            .list
            .take_leading_backdrop()
            .expect("the filter");
        assert_eq!(taken.rect, BOX);
        let bar = |rounded: bool| {
            let mut b = DisplayListBuilder::new();
            b.fill_rect(Rect::new(0.0, 0.0, 800.0, 3000.0), Color::rgb(0, 0, 255));
            b.push_layer(LayerKind::Fixed);
            if rounded {
                b.push_clip_rounded_rect(Rect::new(0.0, 0.0, 200.0, 60.0), Radii::uniform(4.0));
            } else {
                b.push_clip_rect(Rect::new(0.0, 0.0, 200.0, 60.0));
            }
            b.push_transform(Matrix::translation(0.0, 5.0));
            b.backdrop_filter(&[FilterOp::Blur(16.0)], BOX, Radii::uniform(8.0), 0);
            b.fill_rect(BOX, Color::rgba(5, 11, 26, 190));
            b.pop_transform();
            b.pop_clip();
            b.pop_layer();
            b.build()
        };
        let mut split = bar(false).split_layers();
        assert!(!split.pinned);
        let piece = &mut split
            .layers
            .iter_mut()
            .find(|l| has_backdrop(&l.list))
            .expect("the cut piece")
            .list;
        let taken = piece.take_leading_backdrop().expect("the filter");
        assert_eq!(taken.rect, Rect::new(10.0, 15.0, 190.0, 40.0));
        assert_eq!(taken.radii, Radii::ZERO);
        assert!(!has_backdrop(piece));
        let split = bar(true).split_layers();
        assert!(split.pinned);
        assert!(split.layers.iter().all(|l| l.kind.is_none()));
    }

    /// A fixed layer under everything (the canvas background's) is a piece of its own, kept in
    /// place under the page; not where content that scrolls over it has a backdrop filter, which
    /// reads it.
    #[test]
    fn a_fixed_layer_under_the_page_is_a_piece_unless_the_page_reads_its_backdrop() {
        let list = |reads: bool| {
            let mut b = DisplayListBuilder::new();
            b.push_layer(LayerKind::Fixed);
            b.fill_rect(Rect::new(0.0, 0.0, 800.0, 600.0), Color::rgb(5, 11, 26));
            b.pop_layer();
            b.fill_rect(Rect::new(0.0, 100.0, 800.0, 50.0), Color::rgb(0, 0, 255));
            if reads {
                b.push_opacity(1.0, BOX);
                b.backdrop_filter(&[FilterOp::Blur(4.0)], BOX, Radii::ZERO, 1);
                b.pop_opacity();
            }
            b.build()
        };
        let split = list(false).split_layers();
        assert!(!split.pinned);
        let kinds: Vec<_> = split.layers.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [Some(LayerKind::Fixed), None]);
        let split = list(true).split_layers();
        assert!(split.pinned);
        assert!(split.layers.iter().all(|l| l.kind.is_none()));
    }

    /// A layer that cannot be split off pins the list only when it shows something: a fixed
    /// overlay at opacity 0 (a menu's backdrop, waiting to fade in) is right at any scroll position.
    #[test]
    fn a_layer_that_shows_nothing_pins_nothing() {
        let list = |opacity: f32| {
            let mut b = DisplayListBuilder::new();
            b.fill_rect(Rect::new(0.0, 0.0, 9.0, 9.0), Color::BLACK);
            b.push_layer(LayerKind::Pinned);
            b.push_opacity(opacity, Rect::new(0.0, 0.0, 800.0, 600.0));
            b.backdrop_filter(
                &[FilterOp::Blur(20.0)],
                Rect::new(0.0, 0.0, 800.0, 600.0),
                Radii::ZERO,
                1,
            );
            b.fill_rect(Rect::new(0.0, 0.0, 800.0, 600.0), Color::rgba(0, 0, 0, 100));
            b.pop_opacity();
            b.pop_layer();
            b.build()
        };
        assert!(!list(0.0).split_layers().pinned);
        assert!(list(0.5).split_layers().pinned);
        // In a fixed bar, the overlay's backdrop filter at 0 reads nothing under the bar: the bar
        // is still a piece of its own.
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 800.0, 3000.0), Color::BLACK);
        b.push_layer(LayerKind::Fixed);
        b.fill_rect(Rect::new(0.0, 0.0, 800.0, 40.0), Color::WHITE);
        b.push_layer(LayerKind::Pinned);
        b.push_opacity(0.0, Rect::new(0.0, 0.0, 800.0, 600.0));
        b.backdrop_filter(
            &[FilterOp::Blur(20.0)],
            Rect::new(0.0, 0.0, 800.0, 600.0),
            Radii::ZERO,
            1,
        );
        b.pop_opacity();
        b.pop_layer();
        b.pop_layer();
        let split = b.build().split_layers();
        assert!(!split.pinned);
        assert!(split
            .layers
            .iter()
            .any(|l| l.kind == Some(LayerKind::Fixed)));
    }

    #[test]
    fn a_sticky_layer_under_clips_splits_off_with_them_as_its_state() {
        let sticky = LayerKind::Sticky(StickyConstraint {
            rect: Rect::new(0.0, 100.0, 50.0, 50.0),
            container: Rect::new(0.0, 100.0, 50.0, 400.0),
            insets: [Some(0.0), None, None, None],
        });
        let list = |rounded: bool, video: bool| {
            let mut b = DisplayListBuilder::new();
            b.fill_rect(Rect::new(0.0, 0.0, 9.0, 9.0), Color::BLACK);
            b.push_clip_rect(Rect::new(0.0, 50.0, 100.0, 500.0));
            if rounded {
                b.push_clip_rounded_rect(Rect::new(0.0, 60.0, 100.0, 400.0), Radii::uniform(4.0));
            }
            b.push_layer(sticky);
            b.fill_rect(Rect::new(0.0, 100.0, 50.0, 50.0), Color::WHITE);
            if video {
                let r = Rect::new(0.0, 100.0, 50.0, 30.0);
                b.video(VideoId(1), None, r, r);
            }
            b.pop_layer();
            b.fill_rect(Rect::new(0.0, 450.0, 9.0, 9.0), Color::BLACK);
            if rounded {
                b.pop_clip();
            }
            b.pop_clip();
            b.build()
        };
        // Under a rectangle clip, with a video: a piece, drawn through the clip.
        let split = list(false, true).split_layers();
        assert!(!split.pinned);
        let kinds: Vec<_> = split.layers.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [None, Some(sticky), None]);
        assert_eq!(
            split.layers[1].state.0,
            [StateOp::Clip(Rect::new(0.0, 50.0, 100.0, 500.0))]
        );
        assert!(!split.layers[1]
            .list
            .items
            .iter()
            .any(|i| matches!(i, Item::PushClipRect { .. })));
        // (what follows it is still clipped)
        assert!(matches!(
            split.layers[2].list.items[..],
            [
                Item::PushClipRect { .. },
                Item::FillRect { .. },
                Item::PopClip
            ]
        ));
        // Under a rounded clip too: its state has both.
        let split = list(true, false).split_layers();
        assert!(!split.pinned);
        assert_eq!(split.layers[1].state.0.len(), 2);
        // A video under a rounded clip: its quad cannot take the corners, so it stays in place.
        let split = list(true, true).split_layers();
        assert!(split.pinned);
        assert!(split.layers.iter().all(|l| l.kind.is_none()));
    }
}
