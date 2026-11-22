// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The computed values layout reads, as one trait: [`Style`]. Layout never sees stylo; `snowlark-render`
//! implements [`Style`] for a cheap-to-clone handle to stylo's `ComputedValues`, and the tests implement
//! it with the plain [`crate::TestStyle`] struct. [`StyleSource`] is how layout asks for the style of an
//! element, a pseudo-element or an anonymous box, and for the content of replaced elements.
//!
//! Values are *computed* values (CSS Cascade 4 §4.4): lengths are absolute CSS px, percentages stay
//! percentages (resolved by layout against the right basis), keywords are typed enums. Getters return
//! small `Copy` values; the few list-valued properties return `Cow` slices so an implementation can lend
//! stored data or convert on the fly (they are read once per box construction, not per layout).
//!
//! Physical sides throughout ([`Side`]): layout maps them to logical sides with [`crate::logical`] using
//! [`Style::writing_mode`] and [`Style::direction`].

use std::borrow::Cow;
use std::sync::Arc;

pub use crate::logical::Flip;

pub use snowlark_paint::{
    BorderStyle, Color, Direction, FilterOp, FontFeature, FontStretch, FontStyle, ImageId,
    ImageRendering, Matrix, Point, Rect,
};

/// A physical side of a box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Top, Side::Right, Side::Bottom, Side::Left];
}

/// A corner of a box (for `border-*-radius`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomRight,
    BottomLeft,
}

/// `<length-percentage>` as a computed value: `length` px plus an optional percentage (a fraction: 0.5 is
/// `50%`). `calc(10px + 5%)` is `{ length: 10, percent: Some(0.05) }`. A non-linear `calc()` (`min()`,
/// `max()`, `clamp()` mixing lengths and percentages) adds linear bounds: its value is
/// `max(lower, min(length + percent × basis, upper))` (CSS Values 4 §10.2: `clamp(MIN, VAL, MAX)` is
/// `max(MIN, min(VAL, MAX))`). The style side maps what it cannot express this way as best it can.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LengthPercentage {
    pub length: f32,
    pub percent: Option<f32>,
    pub lower: Option<Linear>,
    pub upper: Option<Linear>,
}

/// A radius of `circle()` or `ellipse()` (CSS Shapes 1 §3.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShapeRadius {
    Length(LengthPercentage),
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
}

/// A shape of `clip-path` (CSS Masking 1 §5.1, CSS Shapes 1 §3.1), its lengths and percentages
/// relative to the reference box.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipShape {
    /// The reference box itself, with its corners.
    Box,
    /// `inset()` (and `rect()`, `xywh()`, which compute to it): top, right, bottom, left, and the
    /// corner radii from the top left, clockwise (boxed: the variant is many times the others).
    Inset {
        sides: Box<[LengthPercentage; 4]>,
        radii: Box<[(LengthPercentage, LengthPercentage); 4]>,
    },
    /// `circle()`; no center is the middle of the box.
    Circle {
        center: Option<(LengthPercentage, LengthPercentage)>,
        radius: ShapeRadius,
    },
    /// `ellipse()`.
    Ellipse {
        center: Option<(LengthPercentage, LengthPercentage)>,
        radii: (ShapeRadius, ShapeRadius),
    },
    /// `polygon()`.
    Polygon {
        even_odd: bool,
        points: Vec<(LengthPercentage, LengthPercentage)>,
    },
    /// `path()` and `shape()` (CSS Shapes 1 §3.1, CSS Shapes 2 §3.2).
    Path {
        even_odd: bool,
        commands: Vec<PathCommand>,
    },
    /// `url()` naming a `<clipPath>` element (CSS Masking 1 §6), resolved by whoever reads the
    /// document; one that names nothing is no clip.
    Reference(Arc<ClipReference>),
}

/// A `<clipPath>` element's geometry: its own, then the `<clipPath>` its `clip-path` names, and so
/// on, each clipping inside the one before. Each level is in that element's own units.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipReference {
    pub levels: Vec<ClipLevel>,
}

/// One `<clipPath>` of a [`ClipReference`]: the union of its shapes as one path.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipLevel {
    /// `clipPathUnits="objectBoundingBox"`: the contours are fractions of the reference box;
    /// otherwise user units, CSS pixels before zoom from the reference box's origin.
    pub bounding_box_units: bool,
    /// The element's own `transform`, in the coordinates the units map to (for `objectBoundingBox`
    /// it applies after the fractions are scaled up, CSS Masking 1 §6.1).
    pub transform: Matrix,
    /// The contours of the path, flattened, and whether it fills by the even-odd rule.
    pub contours: Vec<Vec<Point>>,
    pub even_odd: bool,
}

/// An end point of a path command: from the reference box's origin, or from the current point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathPoint {
    pub x: LengthPercentage,
    pub y: LengthPercentage,
    pub relative: bool,
}

/// What a control point of a path command is relative to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathReference {
    Origin,
    Start,
    End,
}

/// A control point of a path command.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathControl {
    pub x: LengthPercentage,
    pub y: LengthPercentage,
    pub reference: PathReference,
}

/// A command of `path()` or `shape()` (CSS Shapes 1 §3.1, CSS Shapes 2 §3.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathCommand {
    Move {
        to: PathPoint,
    },
    Line {
        to: PathPoint,
    },
    HLine {
        x: LengthPercentage,
        relative: bool,
    },
    VLine {
        y: LengthPercentage,
        relative: bool,
    },
    Cubic {
        to: PathPoint,
        c1: PathControl,
        c2: PathControl,
    },
    SmoothCubic {
        to: PathPoint,
        c2: PathControl,
    },
    Quad {
        to: PathPoint,
        c1: PathControl,
    },
    SmoothQuad {
        to: PathPoint,
    },
    /// `rotate` in radians.
    Arc {
        to: PathPoint,
        rx: LengthPercentage,
        ry: LengthPercentage,
        rotate: f32,
        large: bool,
        sweep: bool,
    },
    Close,
}

/// The box a `clip-path` shape is relative to (CSS Masking 1 §5.1). `fill-box`, `stroke-box` and
/// `view-box` are the content and border boxes for a CSS box (CSS Box 4 §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipBox {
    Margin,
    Border,
    Padding,
    Content,
}

/// A `clip-path` of the kinds layout resolves.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipPath {
    pub shape: ClipShape,
    pub reference: ClipBox,
    /// The used `zoom`: what a `<clipPath>`'s user space is scaled by (its lengths, unlike CSS
    /// lengths, come without it).
    pub zoom: f32,
}

/// A linear `length + percent × basis` term (a bound of a non-linear `calc()`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Linear {
    pub length: f32,
    /// A fraction (0.5 is `50%`).
    pub percent: f32,
}

impl Linear {
    #[inline]
    pub fn at(&self, basis: f32) -> f32 {
        self.length + self.percent * basis
    }
}

impl LengthPercentage {
    pub const ZERO: LengthPercentage = LengthPercentage::linear(0.0, None);

    /// `length` px plus an optional fraction of the basis.
    pub const fn linear(length: f32, percent: Option<f32>) -> LengthPercentage {
        LengthPercentage {
            length,
            percent,
            lower: None,
            upper: None,
        }
    }

    pub const fn px(length: f32) -> LengthPercentage {
        LengthPercentage::linear(length, None)
    }

    /// A percentage, given in percent (`percent(50.0)` is `50%`).
    pub const fn percent(percent: f32) -> LengthPercentage {
        LengthPercentage::linear(0.0, Some(percent / 100.0))
    }

    pub fn has_percent(&self) -> bool {
        self.percent.is_some()
            || self.lower.is_some_and(|l| l.percent != 0.0)
            || self.upper.is_some_and(|u| u.percent != 0.0)
    }

    /// Whether this is `length + percent × basis` with no `min()`/`max()` bounds.
    #[inline]
    pub fn is_linear(&self) -> bool {
        self.lower.is_none() && self.upper.is_none()
    }

    /// Resolves against a definite basis.
    #[inline]
    pub fn resolve(&self, basis: f32) -> f32 {
        // (The multiply reads 0 for an absent percentage, never its uninitialized bytes: see
        // `clamp`. The sum still adds exactly 0 then, as `0 × basis` can be NaN.)
        let part = self.percent.unwrap_or(0.0) * basis;
        let v = self.length + if self.percent.is_some() { part } else { 0.0 };
        if self.is_linear() {
            return v;
        }
        self.clamp(v, basis)
    }

    /// `v` between the bounds of a non-linear `calc()`. Out of line, and an absent bound is the
    /// neutral one (+∞ above, -∞ below), chosen before the arithmetic and always applied: the
    /// compiler would otherwise compute `at` on an absent bound's bytes before testing it (they are
    /// uninitialized: often half a stale pointer, a subnormal float that costs every multiply and
    /// add on it a microcode assist), and drop the result. The value is the same as testing first:
    /// with a finite basis the neutral bound leaves `v` as it is, and where `0 × basis` is NaN,
    /// `min`/`max` return `v`.
    #[inline(never)]
    fn clamp(&self, v: f32, basis: f32) -> f32 {
        let upper = self.upper.unwrap_or(Linear {
            length: f32::INFINITY,
            percent: 0.0,
        });
        let lower = self.lower.unwrap_or(Linear {
            length: f32::NEG_INFINITY,
            percent: 0.0,
        });
        v.min(upper.at(basis)).max(lower.at(basis))
    }

    /// Resolves against a possibly indefinite basis: `None` when a percentage meets an indefinite basis.
    #[inline]
    pub fn resolve_opt(&self, basis: Option<f32>) -> Option<f32> {
        match basis {
            Some(b) => Some(self.resolve(b)),
            None if self.has_percent() => None,
            None => Some(self.resolve(0.0)),
        }
    }

    /// Resolves, treating a percentage against an indefinite basis as zero (padding, gaps).
    #[inline]
    pub fn resolve_or_zero(&self, basis: Option<f32>) -> f32 {
        self.resolve(basis.unwrap_or(0.0))
    }
}

/// `<length-percentage> | auto` (margins, insets).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum LengthPercentageAuto {
    #[default]
    Auto,
    LengthPercentage(LengthPercentage),
}

impl LengthPercentageAuto {
    pub const ZERO: LengthPercentageAuto =
        LengthPercentageAuto::LengthPercentage(LengthPercentage::ZERO);

    pub const fn px(v: f32) -> LengthPercentageAuto {
        LengthPercentageAuto::LengthPercentage(LengthPercentage::px(v))
    }

    pub const fn percent(v: f32) -> LengthPercentageAuto {
        LengthPercentageAuto::LengthPercentage(LengthPercentage::percent(v))
    }

    pub fn is_auto(&self) -> bool {
        matches!(self, LengthPercentageAuto::Auto)
    }

    /// `None` for `auto`.
    #[inline]
    pub fn resolve(&self, basis: f32) -> Option<f32> {
        match self {
            LengthPercentageAuto::Auto => None,
            LengthPercentageAuto::LengthPercentage(lp) => Some(lp.resolve(basis)),
        }
    }

    /// `auto` as zero.
    #[inline]
    pub fn resolve_or_zero(&self, basis: f32) -> f32 {
        self.resolve(basis).unwrap_or(0.0)
    }
}

/// `width`, `height`, `min-width`, `min-height` (css-sizing-3 §3.1). `min-*: auto` is [`SizeValue::Auto`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum SizeValue {
    #[default]
    Auto,
    LengthPercentage(LengthPercentage),
    MinContent,
    MaxContent,
    /// `fit-content` (`None`) or `fit-content(<length-percentage>)`.
    FitContent(Option<LengthPercentage>),
    /// `stretch` (`-webkit-fill-available`).
    Stretch,
}

impl SizeValue {
    pub const fn px(v: f32) -> SizeValue {
        SizeValue::LengthPercentage(LengthPercentage::px(v))
    }

    pub const fn percent(v: f32) -> SizeValue {
        SizeValue::LengthPercentage(LengthPercentage::percent(v))
    }

    pub fn is_auto(&self) -> bool {
        matches!(self, SizeValue::Auto)
    }

    /// The length-percentage, if the value is one.
    pub fn length_percentage(&self) -> Option<LengthPercentage> {
        match self {
            SizeValue::LengthPercentage(lp) => Some(*lp),
            _ => None,
        }
    }
}

/// `max-width`, `max-height`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum MaxSizeValue {
    #[default]
    None,
    LengthPercentage(LengthPercentage),
    MinContent,
    MaxContent,
    FitContent(Option<LengthPercentage>),
    Stretch,
}

impl MaxSizeValue {
    pub const fn px(v: f32) -> MaxSizeValue {
        MaxSizeValue::LengthPercentage(LengthPercentage::px(v))
    }

    pub const fn percent(v: f32) -> MaxSizeValue {
        MaxSizeValue::LengthPercentage(LengthPercentage::percent(v))
    }
}

/// `display` (CSS Display 3 §2) as its computed value. Blockification (floats, absolute positioning,
/// flex/grid items, the root) is part of computing the value, but layout blockifies again where the
/// box tree rules require it, so an implementation that doesn't is still laid out correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    None,
    Contents,
    Box {
        outside: DisplayOutside,
        inside: DisplayInside,
        list_item: bool,
    },
    Internal(DisplayInternal),
}

impl Default for Display {
    fn default() -> Display {
        Display::INLINE
    }
}

impl Display {
    pub const BLOCK: Display = Display::Box {
        outside: DisplayOutside::Block,
        inside: DisplayInside::Flow,
        list_item: false,
    };
    pub const INLINE: Display = Display::Box {
        outside: DisplayOutside::Inline,
        inside: DisplayInside::Flow,
        list_item: false,
    };
    pub const INLINE_BLOCK: Display = Display::Box {
        outside: DisplayOutside::Inline,
        inside: DisplayInside::FlowRoot,
        list_item: false,
    };
    pub const FLOW_ROOT: Display = Display::Box {
        outside: DisplayOutside::Block,
        inside: DisplayInside::FlowRoot,
        list_item: false,
    };
    pub const LIST_ITEM: Display = Display::Box {
        outside: DisplayOutside::Block,
        inside: DisplayInside::Flow,
        list_item: true,
    };
    pub const FLEX: Display = Display::Box {
        outside: DisplayOutside::Block,
        inside: DisplayInside::Flex,
        list_item: false,
    };
    pub const INLINE_FLEX: Display = Display::Box {
        outside: DisplayOutside::Inline,
        inside: DisplayInside::Flex,
        list_item: false,
    };
    pub const GRID: Display = Display::Box {
        outside: DisplayOutside::Block,
        inside: DisplayInside::Grid,
        list_item: false,
    };
    pub const INLINE_GRID: Display = Display::Box {
        outside: DisplayOutside::Inline,
        inside: DisplayInside::Grid,
        list_item: false,
    };
    pub const TABLE: Display = Display::Box {
        outside: DisplayOutside::Block,
        inside: DisplayInside::Table,
        list_item: false,
    };
    pub const INLINE_TABLE: Display = Display::Box {
        outside: DisplayOutside::Inline,
        inside: DisplayInside::Table,
        list_item: false,
    };
    pub const TABLE_ROW_GROUP: Display = Display::Internal(DisplayInternal::TableRowGroup);
    pub const TABLE_HEADER_GROUP: Display = Display::Internal(DisplayInternal::TableHeaderGroup);
    pub const TABLE_FOOTER_GROUP: Display = Display::Internal(DisplayInternal::TableFooterGroup);
    pub const TABLE_ROW: Display = Display::Internal(DisplayInternal::TableRow);
    pub const TABLE_CELL: Display = Display::Internal(DisplayInternal::TableCell);
    pub const TABLE_COLUMN_GROUP: Display = Display::Internal(DisplayInternal::TableColumnGroup);
    pub const TABLE_COLUMN: Display = Display::Internal(DisplayInternal::TableColumn);
    pub const TABLE_CAPTION: Display = Display::Internal(DisplayInternal::TableCaption);
}

/// The outer display type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayOutside {
    Block,
    Inline,
    /// Laid out as `block` (run-in boxes are not merged into the next block).
    RunIn,
}

/// The inner display type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayInside {
    Flow,
    FlowRoot,
    Table,
    Flex,
    Grid,
    /// Laid out as `flow` (ruby layout is not implemented).
    Ruby,
}

/// Layout-internal display types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayInternal {
    TableRowGroup,
    TableHeaderGroup,
    TableFooterGroup,
    TableRow,
    TableCell,
    TableColumnGroup,
    TableColumn,
    TableCaption,
    RubyBase,
    RubyText,
    RubyBaseContainer,
    RubyTextContainer,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Position {
    #[default]
    Static,
    Relative,
    Absolute,
    Fixed,
    Sticky,
}

impl Position {
    /// `absolute` or `fixed`.
    pub fn is_out_of_flow(self) -> bool {
        matches!(self, Position::Absolute | Position::Fixed)
    }

    /// Anything but `static`.
    pub fn is_positioned(self) -> bool {
        self != Position::Static
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Float {
    #[default]
    None,
    Left,
    Right,
    InlineStart,
    InlineEnd,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Clear {
    #[default]
    None,
    Left,
    Right,
    Both,
    InlineStart,
    InlineEnd,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoxSizing {
    #[default]
    ContentBox,
    BorderBox,
}

/// A `break-before` or `break-after` value (CSS Fragmentation 3 §3.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BreakBetween {
    #[default]
    Auto,
    Avoid,
    AvoidPage,
    AvoidColumn,
    Always,
    Page,
    Left,
    Right,
    Recto,
    Verso,
    Column,
}

/// A `break-inside` value (CSS Fragmentation 3 §3.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BreakInside {
    #[default]
    Auto,
    Avoid,
    AvoidPage,
    AvoidColumn,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overflow {
    #[default]
    Visible,
    Hidden,
    Clip,
    Scroll,
    Auto,
}

impl Overflow {
    /// `hidden`, `scroll` and `auto` make a scroll container (css-overflow-3 §3).
    pub fn is_scroll_container(self) -> bool {
        matches!(self, Overflow::Hidden | Overflow::Scroll | Overflow::Auto)
    }

    /// Whether content is clipped in this axis.
    pub fn clips(self) -> bool {
        self != Overflow::Visible
    }
}

/// A line-clamp container's `line-clamp` (CSS Overflow 4 §4): `max-lines` and `block-ellipsis`.
#[derive(Clone, Debug, PartialEq)]
pub struct LineClamp {
    /// The number of lines before the clamp point (`None`: no line count, only `auto`).
    pub lines: Option<u32>,
    /// `auto`: the clamp point also goes where the content would overflow the box's (maximum)
    /// block size.
    pub auto: bool,
    /// The block overflow ellipsis (`block-ellipsis`): `None` for `no-ellipsis`.
    pub ellipsis: Option<String>,
}

/// `overflow-clip-margin` (CSS Overflow 3 §3.2): how far outside its reference box a box with
/// `overflow: clip` paints before it is clipped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverflowClipMargin {
    /// The `<visual-box>` (`padding-box` when only a length is given).
    pub reference: BoxArea,
    /// The length, in px (negative values move the clip edge inward).
    pub margin: f32,
}

impl Default for OverflowClipMargin {
    fn default() -> Self {
        OverflowClipMargin {
            reference: BoxArea::PaddingBox,
            margin: 0.0,
        }
    }
}

/// An element's used color scheme (CSS Color Adjust 1 §2.3): what the user agent's own drawing for
/// it (scroll bars, the canvas surface) follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorScheme {
    #[default]
    Light,
    Dark,
}

/// `scrollbar-width` (CSS Scrollbars 1 §3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollbarWidth {
    #[default]
    Auto,
    Thin,
    /// No scroll bar is shown; the box still scrolls.
    None,
}

/// The used value of `resize` (CSS UI 4 §5.1): which of the box's physical sizes the user may
/// change (the logical `block` and `inline` are resolved against the writing mode).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Resize {
    #[default]
    None,
    Both,
    Horizontal,
    Vertical,
}

impl Resize {
    /// Whether the user may change the box's width.
    pub fn horizontal(self) -> bool {
        matches!(self, Resize::Both | Resize::Horizontal)
    }
    /// Whether the user may change the box's height.
    pub fn vertical(self) -> bool {
        matches!(self, Resize::Both | Resize::Vertical)
    }
}

/// `scrollbar-color: <color> <color>` (CSS Scrollbars 1 §2): the thumb's and the track's colors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarColors {
    pub thumb: Color,
    pub track: Color,
}

/// A scroll container's custom scroll bars, from the styles of its `::-webkit-scrollbar` parts (a
/// compat feature of the web platform; `scrollbar-width` and `scrollbar-color`, CSS Scrollbars 1,
/// take precedence when either is not `auto`). The parts are drawn as boxes filling the bar: the
/// track's background along the edge and the thumb over it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CustomScrollbar {
    /// The vertical bar's width (CSS px, `::-webkit-scrollbar`'s `width`): `None` for `auto` (the
    /// automatic bar's), 0 for no bar (`display: none`, or a zero width).
    pub vertical: Option<f32>,
    /// The horizontal bar's height, as `vertical`.
    pub horizontal: Option<f32>,
    /// The thumb (`::-webkit-scrollbar-thumb`), `None` when it is not drawn: no rule styles it,
    /// or `display: none`.
    pub thumb: Option<CustomScrollbarThumb>,
    /// The track's color (`::-webkit-scrollbar-track`'s background, else
    /// `::-webkit-scrollbar`'s), `None` when transparent.
    pub track: Option<Color>,
}

/// The thumb of a [`CustomScrollbar`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CustomScrollbarThumb {
    pub color: Color,
    /// The corner radius: a length (CSS px) plus a fraction of the thumb's thickness.
    pub radius: (f32, f32),
    /// How far the painted thumb stands in from each side of the bar across it: its border, when
    /// `background-clip` leaves the border out (CSS px).
    pub inset: f32,
}

impl CustomScrollbar {
    /// The bars the styles of the parts say: `bar` is `::-webkit-scrollbar`'s, `thumb` and `track`
    /// those of `-thumb` and `-track` when a rule styles them. A part is drawn as its background
    /// color; a bar's thickness is its `width` (vertical) or `height` (horizontal), percentages
    /// against nothing (zero), `auto` the automatic bar's, and none for `display: none`.
    pub fn from_parts<S: Style>(bar: &S, thumb: Option<&S>, track: Option<&S>) -> CustomScrollbar {
        let hidden = bar.display() == Display::None;
        let thickness = |v: SizeValue| match v {
            _ if hidden => Some(0.0),
            SizeValue::LengthPercentage(l) => Some(l.resolve(0.0).max(0.0)),
            _ => None,
        };
        let fill = |s: &S| Some(s.background_color()).filter(|c| !c.is_transparent());
        let thumb = thumb
            .filter(|t| t.display() != Display::None)
            .and_then(|t| {
                let (r, _) = t.phys_border_radius(Corner::TopLeft);
                Some(CustomScrollbarThumb {
                    color: fill(t)?,
                    radius: (r.length, r.percent.unwrap_or(0.0)),
                    inset: if t.background_color_area().0 == BoxArea::BorderBox {
                        0.0
                    } else {
                        t.phys_border_width(Side::Left)
                    },
                })
            });
        CustomScrollbar {
            vertical: thickness(bar.phys_width()),
            horizontal: thickness(bar.phys_height()),
            thumb,
            track: track.and_then(fill).or_else(|| fill(bar)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
    Collapse,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WritingMode {
    #[default]
    HorizontalTb,
    VerticalRl,
    VerticalLr,
    SidewaysRl,
    SidewaysLr,
}

/// `text-orientation`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextOrientation {
    #[default]
    Mixed,
    Upright,
    Sideways,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnicodeBidi {
    #[default]
    Normal,
    Embed,
    Isolate,
    BidiOverride,
    IsolateOverride,
    Plaintext,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextAlign {
    #[default]
    Start,
    End,
    Left,
    Right,
    Center,
    Justify,
    MatchParent,
}

/// The legacy `text-align` values (`-webkit-left`, `-webkit-right`, `-webkit-center`; the `center`
/// element and `align` attributes) that, besides aligning text, "align descendants" (HTML §15.3.3):
/// block-level descendants whose inline margins are both non-`auto` and over-constrained get the
/// leftover space on the side the keyword says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyAlign {
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextAlignLast {
    #[default]
    Auto,
    Start,
    End,
    Left,
    Right,
    Center,
    Justify,
    MatchParent,
}

/// `text-box-trim` (CSS Inline 3 §6.1): which sides of a block container's first/last formatted
/// line, or of an inline box's content area, are trimmed to the `text-box-edge` metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextBoxTrim {
    pub start: bool,
    pub end: bool,
}

/// A `<text-edge>` keyword (CSS Inline 3 §6.2): a font metric a box edge is trimmed to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TextEdgeMetric {
    /// The text-over / text-under baselines (the ascent / descent).
    #[default]
    Text,
    /// The cap-height baseline (over only).
    Cap,
    /// The x-height baseline (over only).
    Ex,
    /// The ideographic-over / -under baselines.
    Ideographic,
    /// The ideographic-ink-over / -under baselines.
    IdeographicInk,
    /// The alphabetic baseline (under only).
    Alphabetic,
}

/// `text-box-edge` (CSS Inline 3 §6.2), with `auto` resolved (to `text` on both sides, as
/// `line-fit-edge: leading` gives).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextBoxEdge {
    pub over: TextEdgeMetric,
    pub under: TextEdgeMetric,
}

/// `vertical-align` (CSS 2 §10.8.1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum VerticalAlign {
    #[default]
    Baseline,
    Sub,
    Super,
    TextTop,
    TextBottom,
    Middle,
    Top,
    Bottom,
    /// Raises by a length, or a percentage of the box's `line-height`.
    LengthPercentage(LengthPercentage),
}

/// Computed `line-height`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum LineHeight {
    #[default]
    Normal,
    /// A multiple of the element's font size.
    Number(f32),
    Length(f32),
}

/// `white-space-collapse` (CSS Text 4 §3; the first half of `white-space`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WhiteSpaceCollapse {
    #[default]
    Collapse,
    Preserve,
    PreserveBreaks,
    PreserveSpaces,
    BreakSpaces,
}

/// `text-wrap-mode` (the other half of `white-space`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextWrapMode {
    #[default]
    Wrap,
    Nowrap,
}

/// `text-wrap-style` (CSS Text 4 §6.2): how wrapped lines choose their break points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextWrapStyle {
    #[default]
    Auto,
    Stable,
    Balance,
    Pretty,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WordBreak {
    #[default]
    Normal,
    BreakAll,
    KeepAll,
    /// The legacy `word-break: break-word` (normal plus `overflow-wrap: anywhere`).
    BreakWord,
}

/// `hyphens` (CSS Text 3 §6.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hyphens {
    None,
    #[default]
    Manual,
    Auto,
}

/// `line-break` (CSS Text 3 §5.3): how strictly CJK line breaking rules apply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineBreak {
    #[default]
    Auto,
    Loose,
    Normal,
    Strict,
    Anywhere,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverflowWrap {
    #[default]
    Normal,
    BreakWord,
    Anywhere,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextOverflow {
    #[default]
    Clip,
    Ellipsis,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextTransform {
    #[default]
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

/// `text-decoration-line` as bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextDecorationLine(pub u8);

impl TextDecorationLine {
    pub const NONE: TextDecorationLine = TextDecorationLine(0);
    pub const UNDERLINE: TextDecorationLine = TextDecorationLine(1);
    pub const OVERLINE: TextDecorationLine = TextDecorationLine(2);
    pub const LINE_THROUGH: TextDecorationLine = TextDecorationLine(4);

    pub fn contains(self, other: TextDecorationLine) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }

    pub fn is_none(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for TextDecorationLine {
    type Output = TextDecorationLine;
    fn bitor(self, rhs: TextDecorationLine) -> TextDecorationLine {
        TextDecorationLine(self.0 | rhs.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextDecorationStyle {
    #[default]
    Solid,
    Double,
    Dotted,
    Dashed,
    Wavy,
}

/// `tab-size`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TabSize {
    /// A number of space advances.
    Spaces(f32),
    Length(f32),
}

impl Default for TabSize {
    fn default() -> TabSize {
        TabSize::Spaces(8.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListStylePosition {
    #[default]
    Outside,
    Inside,
}

/// `list-style-type` (the counter styles of CSS Counter Styles 3 that the UA sheet and pages use most).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ListStyleType {
    None,
    #[default]
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalLeadingZero,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
    LowerGreek,
    DisclosureOpen,
    DisclosureClosed,
    /// A `<string>` marker.
    String(Arc<str>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableLayoutMode {
    #[default]
    Auto,
    Fixed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BorderCollapse {
    #[default]
    Separate,
    Collapse,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptionSide {
    #[default]
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmptyCells {
    #[default]
    Show,
    Hide,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FlexDirection {
    #[default]
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FlexWrap {
    #[default]
    Nowrap,
    Wrap,
    WrapReverse,
}

/// The Box Alignment 3 keywords, for `justify-content`, `align-content`, `align-items`, `align-self`,
/// `justify-items` and `justify-self`; each property uses its subset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    /// `auto` (`*-self` only).
    Auto,
    #[default]
    Normal,
    Stretch,
    Start,
    End,
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
    LastBaseline,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
    SelfStart,
    SelfEnd,
    Left,
    Right,
}

/// An alignment property's overflow-position keyword (css-align-3 §4.4): `safe`, `unsafe` or none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverflowPosition {
    #[default]
    Default,
    Safe,
    Unsafe,
}

/// The overflow-position keywords of the six alignment properties.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlignSafety {
    pub justify_content: OverflowPosition,
    pub align_content: OverflowPosition,
    pub align_items: OverflowPosition,
    pub align_self: OverflowPosition,
    pub justify_items: OverflowPosition,
    pub justify_self: OverflowPosition,
}

/// `flex-basis`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum FlexBasis {
    /// `auto`: use the main size property.
    #[default]
    Auto,
    Content,
    LengthPercentage(LengthPercentage),
}

/// `aspect-ratio`: `auto`, `<ratio>`, or `auto && <ratio>` (`auto: true` with a ratio).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AspectRatio {
    pub auto: bool,
    /// Width divided by height.
    pub ratio: Option<f32>,
}

/// Size containment in each physical axis (CSS Contain 2 §3.1/§3.2: `contain: size`/`inline-size`;
/// CSS Contain 3 §2: `container-type: size`/`inline-size` applies it too), already mapped from the
/// inline/block axes through the writing mode.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SizeContainment {
    pub width: bool,
    pub height: bool,
    /// CSS Sizing 4 §5.1 `contain-intrinsic-width`/`-height`: the content-box size a contained box
    /// takes in place of its content's (`None` for `none`; `auto <length>` is the length, as no
    /// last remembered size is kept).
    pub intrinsic_width: Option<f32>,
    pub intrinsic_height: Option<f32>,
}

/// One breadth of a grid track sizing function.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrackBreadth {
    LengthPercentage(LengthPercentage),
    /// `<flex>` in `fr`.
    Fr(f32),
    MinContent,
    MaxContent,
    Auto,
}

/// A track size: a breadth, `minmax()` or `fit-content()`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrackSize {
    Breadth(TrackBreadth),
    MinMax(TrackBreadth, TrackBreadth),
    FitContent(LengthPercentage),
}

/// `repeat()`'s first argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatCount {
    Count(u16),
    AutoFill,
    AutoFit,
}

/// One component of `grid-template-columns`/`-rows`, with the line names before it.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackListItem {
    Track(TrackSize),
    Repeat {
        count: RepeatCount,
        tracks: Vec<TrackSize>,
        /// One set per line of a repetition (`tracks.len() + 1` sets), or empty.
        line_names: Vec<Vec<Arc<str>>>,
    },
}

/// A computed `grid-template-columns`/`-rows` track list (`none` is empty). `line_names` has one set per
/// line between the items (`items.len() + 1`), or is empty.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackList {
    pub items: Vec<TrackListItem>,
    pub line_names: Vec<Vec<Arc<str>>>,
}

/// A named area of `grid-template-areas`, in 1-based line numbers.
#[derive(Clone, Debug, PartialEq)]
pub struct GridArea {
    pub name: Arc<str>,
    pub row_start: u16,
    pub row_end: u16,
    pub column_start: u16,
    pub column_end: u16,
}

/// Computed `grid-template-areas`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GridTemplateAreas {
    pub areas: Vec<GridArea>,
    pub rows: u16,
    pub columns: u16,
}

/// `grid-auto-flow`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GridAutoFlow {
    pub column: bool,
    pub dense: bool,
}

/// One of `grid-row-start` … `grid-column-end`.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum GridLine {
    #[default]
    Auto,
    /// A line number (negative counts from the end).
    Line(i16),
    Span(u16),
    /// `<custom-ident> <integer>?`: the nth line with this name (an area name matches `name-start`/`-end`).
    Named(Arc<str>, i16),
    NamedSpan(Arc<str>, u16),
}

/// What a background layer, list marker or generated image shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Image {
    #[default]
    None,
    /// An image in the paint crate's [`snowlark_paint::ImageStore`] (a decoded bitmap, or an SVG image
    /// the renderer rasterizes at the size it is drawn), with its natural dimensions in CSS px.
    Raster {
        id: ImageId,
        natural: NaturalSize,
    },
    LinearGradient(Arc<LinearGradient>),
    RadialGradient(Arc<RadialGradient>),
    ConicGradient(Arc<ConicGradient>),
    /// `image(<color>)` (CSS Images 4 §2.4): a solid color with no natural dimensions.
    Color(Color),
    /// `cross-fade()` (CSS Images 4 §2.6).
    CrossFade(Arc<CrossFade>),
}

impl Image {
    /// The image's natural dimensions (CSS Images 3 §4.1): a raster image's, a cross-fade's;
    /// gradients and colors have none.
    pub fn natural(&self) -> NaturalSize {
        match self {
            Image::Raster { natural, .. } => *natural,
            Image::CrossFade(c) => c.natural,
            _ => NaturalSize::default(),
        }
    }

    /// A bitmap of `width` x `height` CSS px.
    pub fn raster(id: ImageId, width: f32, height: f32) -> Image {
        Image::Raster {
            id,
            natural: NaturalSize::fixed(width, height),
        }
    }
}

/// An image's natural dimensions (CSS Images 3 §4.1): a bitmap has both sizes; an SVG image may have
/// only a width, only a height, only a ratio (a `viewBox` without `width`/`height`), or nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NaturalSize {
    pub width: Option<f32>,
    pub height: Option<f32>,
    /// Width / height, for when it isn't implied by both sizes.
    pub ratio: Option<f32>,
}

impl NaturalSize {
    pub fn fixed(width: f32, height: f32) -> NaturalSize {
        NaturalSize {
            width: Some(width),
            height: Some(height),
            ratio: None,
        }
    }

    /// The natural aspect ratio: from both natural sizes when they are positive, else the explicit
    /// ratio.
    pub fn ratio(&self) -> Option<f32> {
        match (self.width, self.height) {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some(w / h),
            _ => self.ratio,
        }
        .filter(|r| *r > 0.0 && r.is_finite())
    }

    /// CSS Images 3 §5.2 "default sizing algorithm": the concrete object size from a specified size
    /// (`None` is `auto`), these natural dimensions and the default object size.
    pub fn concrete(
        &self,
        specified: (Option<f32>, Option<f32>),
        default: (f32, f32),
    ) -> (f32, f32) {
        let ratio = self.ratio();
        match specified {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => (w, ratio.map(|r| w / r).or(self.height).unwrap_or(default.1)),
            (None, Some(h)) => (ratio.map(|r| h * r).or(self.width).unwrap_or(default.0), h),
            (None, None) => match (self.width, self.height, ratio) {
                (Some(w), Some(h), _) => (w, h),
                (Some(w), None, r) => (w, r.map_or(default.1, |r| w / r)),
                (None, Some(h), r) => (r.map_or(default.0, |r| h * r), h),
                // Only a ratio: the largest size with it inside the default object size (a contain
                // constraint).
                (None, None, Some(r)) => contain(r, default),
                (None, None, None) => default,
            },
        }
    }
}

/// The largest size with ratio `r` inside `area` (CSS Images 3 §5.3 contain constraint).
pub fn contain(r: f32, area: (f32, f32)) -> (f32, f32) {
    if area.0 / r <= area.1 {
        (area.0, area.0 / r)
    } else {
        (area.1 * r, area.1)
    }
}

/// The smallest size with ratio `r` covering `area` (CSS Images 3 §5.3 cover constraint).
pub fn cover(r: f32, area: (f32, f32)) -> (f32, f32) {
    if area.0 / r >= area.1 {
        (area.0, area.0 / r)
    } else {
        (area.1 * r, area.1)
    }
}

/// A gradient color stop, or a transition hint; `position` `None` is auto (spread per CSS Images 3
/// §3.5.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorStop {
    pub color: Color,
    pub position: Option<LengthPercentage>,
    /// A transition hint (CSS Images 4 §3.5.3: where between the color stops on either side the
    /// color is halfway between theirs), not a color stop: `color` is unused, `position` set.
    pub hint: bool,
}

impl ColorStop {
    /// A color stop at `position` (`None`: auto).
    pub fn new(color: Color, position: Option<LengthPercentage>) -> ColorStop {
        ColorStop {
            color,
            position,
            hint: false,
        }
    }
}

/// How finely a gradient's colors are sampled between two color stops when they are not a straight
/// premultiplied sRGB blend (a color space or hue method other than sRGB's, a transition hint).
pub const RAMP_STEPS: usize = 64;

/// The colors `k / RAMP_STEPS` of the way (k = 0 ..= RAMP_STEPS) from one color stop to the next,
/// interpolated in the gradient's color space (CSS Images 4 §3.5.2, CSS Color 4 §12).
pub type Ramp = [Color; RAMP_STEPS + 1];

/// `linear-gradient()`; `angle` in degrees clockwise from "to top" (`to right` is 90), or `corner`
/// (`to <side> <side>`: right, bottom), whose angle depends on the box (CSS Images 3 §3.1.1).
#[derive(Clone, Debug, PartialEq)]
pub struct LinearGradient {
    pub angle: f32,
    pub corner: Option<(bool, bool)>,
    pub stops: Vec<ColorStop>,
    /// One per pair of adjacent color stops when the gradient interpolates in another color space
    /// than premultiplied sRGB; empty otherwise.
    pub ramps: Vec<Ramp>,
    pub repeating: bool,
}

/// `radial-gradient()`.
#[derive(Clone, Debug, PartialEq)]
pub struct RadialGradient {
    pub circle: bool,
    pub size: RadialSize,
    pub center: (LengthPercentage, LengthPercentage),
    pub stops: Vec<ColorStop>,
    pub ramps: Vec<Ramp>,
    pub repeating: bool,
}

/// `conic-gradient()` (CSS Images 4 §3.3): `angle` (degrees clockwise from up) is where the
/// gradient starts; stop positions are fractions of a turn (a percentage of 1).
#[derive(Clone, Debug, PartialEq)]
pub struct ConicGradient {
    pub angle: f32,
    pub center: (LengthPercentage, LengthPercentage),
    pub stops: Vec<ColorStop>,
    pub ramps: Vec<Ramp>,
    pub repeating: bool,
}

/// `cross-fade()`: images (or colors, as [`Image::Color`]) with the fraction of each, which sum to
/// at most 1 (CSS Images 4 §2.6.1), and the natural dimensions of the result.
#[derive(Clone, Debug, PartialEq)]
pub struct CrossFade {
    pub layers: Vec<(f32, Image)>,
    pub natural: NaturalSize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RadialSize {
    ClosestSide,
    ClosestCorner,
    FarthestSide,
    FarthestCorner,
    /// Explicit radii (for a circle both are the same).
    Explicit(LengthPercentage, LengthPercentage),
}

/// The boxes of a box's containing block relative to the box's border box origin, with the block's
/// corner radii: the reference boxes of `offset-path` (CSS Motion Path 1 §2.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContainingBoxes {
    pub border: Rect,
    pub padding: Rect,
    pub content: Rect,
    /// Top left, top right, bottom right, bottom left.
    pub radii: [(f32, f32); 4],
}

/// `background-size` of one layer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum BackgroundSize {
    Cover,
    Contain,
    /// `auto` is `None` in either axis.
    #[default]
    Auto,
    Explicit(Option<LengthPercentage>, Option<LengthPercentage>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackgroundRepeat {
    #[default]
    Repeat,
    NoRepeat,
    Space,
    Round,
}

/// `background-attachment` of one layer (CSS Backgrounds 3 §3.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackgroundAttachment {
    /// Fixed to the element's box (it does not scroll with the element's own contents).
    #[default]
    Scroll,
    /// Fixed to the viewport: positioned in it, and it does not move when the page scrolls.
    Fixed,
    /// Fixed to the element's contents: it scrolls with them, positioned in the scrollable area.
    Local,
}

/// A box edge for `background-origin`/`-clip`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoxArea {
    #[default]
    BorderBox,
    PaddingBox,
    ContentBox,
}

/// One background layer.
#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundLayer {
    pub image: Image,
    pub position: (LengthPercentage, LengthPercentage),
    pub size: BackgroundSize,
    pub repeat: (BackgroundRepeat, BackgroundRepeat),
    pub origin: BoxArea,
    pub clip: BoxArea,
    pub attachment: BackgroundAttachment,
}

impl Default for BackgroundLayer {
    fn default() -> BackgroundLayer {
        BackgroundLayer {
            image: Image::None,
            position: (LengthPercentage::ZERO, LengthPercentage::ZERO),
            size: BackgroundSize::Auto,
            repeat: (BackgroundRepeat::Repeat, BackgroundRepeat::Repeat),
            origin: BoxArea::PaddingBox,
            clip: BoxArea::BorderBox,
            attachment: BackgroundAttachment::Scroll,
        }
    }
}

/// `border-image-repeat` per axis (CSS Backgrounds 3 §6.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BorderImageRepeat {
    #[default]
    Stretch,
    Repeat,
    Round,
    Space,
}

/// A `border-image-slice` side: image pixels, or a percentage of the image's size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumberOrPercent {
    Number(f32),
    Percent(f32),
}

/// A `border-image-width` or `-outset` side: a multiple of the border width, a length (or for
/// widths a percentage of the border image area), or (widths only) `auto`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BorderImageLength {
    Number(f32),
    LengthPercentage(LengthPercentage),
    Auto,
}

/// `border-image` (CSS Backgrounds 3 §6), sides in CSS order (top, right, bottom, left).
#[derive(Clone, Debug, PartialEq)]
pub struct BorderImage {
    pub source: Image,
    pub slice: [NumberOrPercent; 4],
    /// `fill`: the middle part is drawn too.
    pub fill: bool,
    pub width: [BorderImageLength; 4],
    pub outset: [BorderImageLength; 4],
    /// Horizontal, vertical.
    pub repeat: (BorderImageRepeat, BorderImageRepeat),
}

/// One mask layer (CSS Masking 1 §7): `mask-image` placed by `mask-position`, `-size`, `-repeat`,
/// `-origin` and `-clip` as a background layer is, and whether `mask-mode` takes its luminance
/// (else its alpha).
#[derive(Clone, Debug, PartialEq)]
pub struct MaskLayer {
    pub layer: BackgroundLayer,
    pub luminance: bool,
}

impl Default for MaskLayer {
    fn default() -> MaskLayer {
        MaskLayer {
            layer: BackgroundLayer {
                origin: BoxArea::BorderBox,
                ..BackgroundLayer::default()
            },
            luminance: false,
        }
    }
}

/// One `box-shadow`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
    pub inset: bool,
}

/// `object-fit`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ObjectFit {
    #[default]
    Fill,
    Contain,
    Cover,
    None,
    ScaleDown,
}

/// One item of a computed `content` value.
#[derive(Clone, Debug, PartialEq)]
pub enum ContentItem {
    /// Text (strings, resolved `attr()`, quotes and counters as the style side produced them).
    Text(Arc<str>),
    Image(Image),
}

/// Computed `content` (for `::before`, `::after` and `::marker`).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Content {
    #[default]
    Normal,
    None,
    Items(Vec<ContentItem>),
}

/// The pseudo-elements that generate boxes. `FirstLine` and `FirstLetter` are hooks: layout asks for
/// them nowhere yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PseudoElement {
    Before,
    After,
    Marker,
    FirstLine,
    FirstLetter,
    /// The box under a top layer element (CSS Position 4 §4.2).
    Backdrop,
    /// A `details` element's content slot (HTML 15.5.5), around everything but its summary.
    DetailsContent,
    /// The default summary of a `details` element without a `summary` child (HTML 15.5.5).
    DetailsSummary,
    /// A part of a `progress` or `meter` element drawn without native appearance.
    Part(FormPart),
}

/// The parts of the internal tree of a `progress` or `meter` element (HTML 15.5.14-15), which
/// draw it when it has `appearance: none`: an inner element holding the bar, and in the bar the
/// value. A meter's value part is named after the gauge region its value is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FormPart {
    ProgressInner,
    ProgressBar,
    ProgressValue,
    MeterInner,
    MeterBar,
    MeterOptimumValue,
    MeterSuboptimumValue,
    MeterEvenLessGoodValue,
}

/// Why layout needs an anonymous box's style (CSS 2 §9.2.1.1, CSS Tables 3 §3.2, Display 3 §2.5).
/// Anonymous boxes inherit the inherited properties from `parent`; everything else is initial, except
/// `display`, which the source sets as the kind says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnonymousBox {
    /// An anonymous block box (wrapping inline content among blocks, or a flex/grid item around text).
    Block,
    /// An anonymous `table` or `inline-table` box around misparented table-internal boxes.
    Table {
        inline: bool,
    },
    TableRow,
    TableCell,
}

/// What a replaced element shows and its natural dimensions (css-images-3 §4.1), from the page: an
/// `<img>`'s decoded image, a `<canvas>`, `<video>`, `<iframe>`, an SVG root, a form control.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplacedContent {
    /// The image to paint, if any (an image that hasn't loaded has none).
    pub image: Option<ImageId>,
    pub natural_width: Option<f32>,
    pub natural_height: Option<f32>,
    /// Width / height, when the content has one without natural sizes (an SVG with a viewBox).
    pub natural_ratio: Option<f32>,
    /// A form control (HTML §15.5): layout gives it its natural size from its font and paints its
    /// text and native parts; the natural sizes above are then unused.
    pub control: Option<Arc<Control>>,
    /// A media element's frame and controls ([`crate::media`]), painted over `image` (the poster).
    pub media: Option<Arc<crate::media::MediaContent>>,
    /// An image that is not a stored bitmap (a gradient or a cross-fade from `content`), painted
    /// into the object-fit rectangle.
    pub generated: Option<Image>,
}

/// A form control widget as HTML §15.5 renders it: an `input`, `select`, `textarea`, `progress` or
/// `meter` element. Its box's border, padding, background and font come from CSS (the UA sheet gives
/// the native look, which author styles override); layout sizes its content box from the font
/// (`size`, `cols`, `rows`, the widest option...) and paints the text and, when `native`, the
/// widget's own parts (check marks, the drop-down arrow, bars).
#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub kind: ControlKind,
    /// `appearance` is not `none`.
    pub native: bool,
    pub disabled: bool,
    pub readonly: bool,
    /// The color of placeholder text (`::placeholder`).
    pub placeholder_color: Color,
    /// A focused text field's caret and selection.
    pub editing: Option<TextEditing>,
}

/// The caret and selection of a focused text field, as byte offsets into its displayed text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextEditing {
    pub selection: std::ops::Range<usize>,
    pub caret: usize,
    /// How far a one-line field's text is scrolled to the left (CSS px), keeping the caret in view.
    pub scroll: f32,
    /// An IME composition's text (underlined).
    pub composition: Option<std::ops::Range<usize>>,
}

/// What kind of widget, with its state.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlKind {
    /// A one-line text field (`text`, `search`, `url`, `tel`, `email`, `password` with its value
    /// already masked, `number`): `text` is the value, or the placeholder when `placeholder`.
    TextField {
        text: Arc<str>,
        placeholder: bool,
        /// The `size` attribute (characters).
        size: u32,
    },
    /// `date`, `time`, `datetime-local`, `month`, `week`: the value in the display format (or the
    /// empty format) and a picker icon.
    DateTime {
        text: Arc<str>,
    },
    /// A multi-line text field.
    TextArea {
        text: Arc<str>,
        placeholder: bool,
        rows: u32,
        cols: u32,
    },
    Checkbox {
        checked: bool,
        indeterminate: bool,
    },
    Radio {
        checked: bool,
    },
    /// `submit`, `reset` and `button` inputs: a button with a text label.
    Button {
        label: Arc<str>,
    },
    /// A file input: its button and the chosen-file label.
    File {
        button: Arc<str>,
        label: Arc<str>,
    },
    Color(Color),
    /// The thumb's position along the track (0..=1).
    Range {
        fraction: f32,
    },
    /// A `select`: a drop-down box (`list_rows: None`) showing the selected option, or a list box
    /// of `list_rows` rows.
    Select {
        options: Arc<[SelectOption]>,
        list_rows: Option<u32>,
    },
    /// `None` is indeterminate.
    Progress {
        fraction: Option<f32>,
    },
    Meter {
        fraction: f32,
        level: MeterLevel,
    },
}

/// A row of a select's list: an option, or an optgroup's label.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectOption {
    pub label: Arc<str>,
    pub selected: bool,
    pub disabled: bool,
    /// An `optgroup` label (not selectable; bold).
    pub group: bool,
    /// Inside an optgroup.
    pub indented: bool,
}

/// HTML §4.10.14: which region of the gauge a meter's value is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeterLevel {
    Optimum,
    Suboptimum,
    EvenLessGood,
}

impl ReplacedContent {
    /// An image with natural size `width` × `height` CSS px.
    pub fn image(image: ImageId, width: f32, height: f32) -> ReplacedContent {
        ReplacedContent {
            image: Some(image),
            natural_width: Some(width),
            natural_height: Some(height),
            natural_ratio: None,
            control: None,
            media: None,
            generated: None,
        }
    }

    /// An image with natural dimensions `natural`.
    pub fn with_natural(image: ImageId, natural: NaturalSize) -> ReplacedContent {
        ReplacedContent {
            image: Some(image),
            natural_width: natural.width,
            natural_height: natural.height,
            natural_ratio: natural.ratio,
            control: None,
            media: None,
            generated: None,
        }
    }

    /// The natural aspect ratio: from the natural sizes, else the explicit ratio.
    pub fn ratio(&self) -> Option<f32> {
        match (self.natural_width, self.natural_height) {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some(w / h),
            _ => self.natural_ratio,
        }
    }
}

/// The computed values layout and painting read. Implemented for a cheap handle (an `Arc`): layout clones
/// it into boxes and fragments. Getters with defaults are optional: the default is the property's
/// initial value.
pub trait Style: Clone {
    // ----- the flow frame (CSS Writing Modes 4 §6, see `crate::logical`) -----
    /// The view this handle reads physical box properties through: the map from the layout frame
    /// they are read in to physical coordinates. The getters below that name a side, corner or axis
    /// (`margin`, `width`, `overflow_x`, …) answer in that frame; `phys_*` are the physical values.
    /// Identity (physical) unless the handle came from [`Style::with_view`].
    fn view(&self) -> Flip {
        Flip::IDENTITY
    }
    /// The same style read through another view.
    fn with_view(&self, view: Flip) -> Self;
    /// The view of the box's own flow frame (its `writing-mode`).
    fn own_view(&self) -> Self {
        let v = Flip::of(self.writing_mode());
        if self.view() == v {
            self.clone()
        } else {
            self.with_view(v)
        }
    }

    #[inline]
    fn inset(&self, side: Side) -> LengthPercentageAuto {
        self.phys_inset(self.view().side(side))
    }
    #[inline]
    fn width(&self) -> SizeValue {
        if self.view().swaps() {
            self.phys_height()
        } else {
            self.phys_width()
        }
    }
    #[inline]
    fn height(&self) -> SizeValue {
        if self.view().swaps() {
            self.phys_width()
        } else {
            self.phys_height()
        }
    }
    #[inline]
    fn min_width(&self) -> SizeValue {
        if self.view().swaps() {
            self.phys_min_height()
        } else {
            self.phys_min_width()
        }
    }
    #[inline]
    fn min_height(&self) -> SizeValue {
        if self.view().swaps() {
            self.phys_min_width()
        } else {
            self.phys_min_height()
        }
    }
    #[inline]
    fn max_width(&self) -> MaxSizeValue {
        if self.view().swaps() {
            self.phys_max_height()
        } else {
            self.phys_max_width()
        }
    }
    #[inline]
    fn max_height(&self) -> MaxSizeValue {
        if self.view().swaps() {
            self.phys_max_width()
        } else {
            self.phys_max_height()
        }
    }
    /// `aspect-ratio` as the frame's width over its height.
    fn aspect_ratio(&self) -> AspectRatio {
        let mut r = self.phys_aspect_ratio();
        if self.view().swaps() {
            r.ratio = r.ratio.map(|v| 1.0 / v);
        }
        r
    }
    fn size_containment(&self) -> SizeContainment {
        let mut c = self.phys_size_containment();
        if self.view().swaps() {
            std::mem::swap(&mut c.width, &mut c.height);
            std::mem::swap(&mut c.intrinsic_width, &mut c.intrinsic_height);
        }
        c
    }
    #[inline]
    fn margin(&self, side: Side) -> LengthPercentageAuto {
        self.phys_margin(self.view().side(side))
    }
    #[inline]
    fn padding(&self, side: Side) -> LengthPercentage {
        self.phys_padding(self.view().side(side))
    }
    #[inline]
    fn border_width(&self, side: Side) -> f32 {
        self.phys_border_width(self.view().side(side))
    }
    fn border_style(&self, side: Side) -> BorderStyle {
        self.phys_border_style(self.view().side(side))
    }
    fn border_color(&self, side: Side) -> Color {
        self.phys_border_color(self.view().side(side))
    }
    /// (frame-horizontal, frame-vertical) radii.
    fn border_radius(&self, corner: Corner) -> (LengthPercentage, LengthPercentage) {
        let v = self.view();
        let r = self.phys_border_radius(v.corner(corner));
        if v.swaps() {
            (r.1, r.0)
        } else {
            r
        }
    }
    #[inline]
    fn overflow_x(&self) -> Overflow {
        if self.view().swaps() {
            self.phys_overflow_y()
        } else {
            self.phys_overflow_x()
        }
    }
    #[inline]
    fn overflow_y(&self) -> Overflow {
        if self.view().swaps() {
            self.phys_overflow_x()
        } else {
            self.phys_overflow_y()
        }
    }
    /// `border-spacing`: its first value is the spacing along the table's inline axis, in any
    /// writing mode (CSS Writing Modes 4 §7.6 leaves it out of the physical properties).
    fn border_spacing(&self) -> (f32, f32) {
        self.phys_border_spacing()
    }
    /// `object-position` from the frame's top-left corner.
    fn object_position(&self) -> (LengthPercentage, LengthPercentage) {
        let v = self.view();
        let (x, y) = self.phys_object_position();
        if v.is_identity() {
            return (x, y);
        }
        // Each frame axis is measured from the physical side its start side maps to; from the far
        // side, the offset of the free space is its complement.
        let flip = |lp: LengthPercentage| {
            LengthPercentage::linear(-lp.length, Some(1.0 - lp.percent.unwrap_or(0.0)))
        };
        let from = |start: Side| match start {
            Side::Left => x,
            Side::Right => flip(x),
            Side::Top => y,
            Side::Bottom => flip(y),
        };
        (from(v.side(Side::Left)), from(v.side(Side::Top)))
    }
    /// The transform in the frame's coordinates of a box whose frame border box is `reference`.
    fn transform(&self, reference: Rect) -> Option<Matrix> {
        let v = self.view();
        if v.is_identity() {
            return self.phys_transform(reference);
        }
        let size = snowlark_paint::Size::new(
            reference.x + reference.width,
            reference.y + reference.height,
        );
        let to_phys = v.matrix(size);
        let phys_ref = v.rect(reference, size);
        let m = self.phys_transform(phys_ref)?;
        let back = v.inverse().matrix(v.size(size));
        Some(to_phys.then(&m).then(&back))
    }

    // ----- box generation and positioning (CSS Display 3, CSS 2 §9) -----
    fn display(&self) -> Display;
    /// `display` before blockification: an absolutely positioned box's static position depends on
    /// whether it was inline-level (CSS 2 §10.3.7, css-position-3 §3.4).
    fn original_display(&self) -> Display {
        self.display()
    }
    fn position(&self) -> Position;
    fn float(&self) -> Float;
    fn clear(&self) -> Clear;
    /// `inset` per side (`top`/`right`/`bottom`/`left`).
    fn phys_inset(&self, side: Side) -> LengthPercentageAuto;
    /// `None` is `z-index: auto`.
    fn z_index(&self) -> Option<i32>;

    // ----- sizing (css-sizing-3, css-box-4) -----
    fn box_sizing(&self) -> BoxSizing;
    fn phys_width(&self) -> SizeValue;
    fn phys_height(&self) -> SizeValue;
    fn phys_min_width(&self) -> SizeValue;
    fn phys_min_height(&self) -> SizeValue;
    fn phys_max_width(&self) -> MaxSizeValue;
    fn phys_max_height(&self) -> MaxSizeValue;
    fn phys_aspect_ratio(&self) -> AspectRatio {
        AspectRatio {
            auto: true,
            ratio: None,
        }
    }
    /// Size containment (none by default). It applies only to boxes containment applies to: the
    /// caller leaves out internal table boxes and non-atomic inlines.
    /// Layout containment (CSS Contain 2 §3.2: `contain: layout`, `content`, `strict`).
    fn layout_containment(&self) -> bool {
        false
    }
    /// Paint containment (CSS Contain 2 §3.4: `contain: paint`, `content`, `strict`): the box
    /// clips its content to its overflow clip edge.
    fn paint_containment(&self) -> bool {
        false
    }
    fn phys_size_containment(&self) -> SizeContainment {
        SizeContainment::default()
    }
    /// `content-visibility: hidden` (CSS Contain 2 section 4): the element's contents are skipped.
    /// Their boxes are still generated and laid out (geometry queries give real sizes); they are
    /// neither painted, hit tested, selected nor found ([`Fragment::visible_children`]).
    fn contents_skipped(&self) -> bool {
        false
    }
    /// Whether `contain-intrinsic-width` / `-height` (physical) are `auto <length>` or `auto none`
    /// (CSS Sizing 4 5.2.1): the box's content size is the last remembered size when it is skipping
    /// its contents, and the renderer records it while the box is laid out with its contents.
    fn intrinsic_size_auto(&self) -> (bool, bool) {
        (false, false)
    }
    fn phys_margin(&self, side: Side) -> LengthPercentageAuto;
    fn phys_padding(&self, side: Side) -> LengthPercentage;
    /// The computed border width: already 0 when the side's style is `none` or `hidden`.
    fn phys_border_width(&self, side: Side) -> f32;
    fn phys_border_style(&self, side: Side) -> BorderStyle;
    fn phys_border_color(&self, side: Side) -> Color;
    /// `border-*-radius`: (horizontal, vertical).
    fn phys_border_radius(&self, _corner: Corner) -> (LengthPercentage, LengthPercentage) {
        (LengthPercentage::ZERO, LengthPercentage::ZERO)
    }
    fn phys_overflow_x(&self) -> Overflow;
    fn phys_overflow_y(&self) -> Overflow;
    /// `overflow-clip-margin` (CSS Overflow 3 §3.2).
    fn overflow_clip_margin(&self) -> OverflowClipMargin {
        OverflowClipMargin::default()
    }
    /// `scroll-padding-*` (CSS Scroll Snap 1 §5.1), insetting the optimal viewing region of a
    /// scroll container; `auto` is 0.
    fn scroll_padding(&self, _side: Side) -> LengthPercentage {
        LengthPercentage::ZERO
    }
    /// `scroll-margin-*` in px (CSS Scroll Snap 1 §5.2), outsetting a box's scroll snap area.
    fn scroll_margin(&self, _side: Side) -> f32 {
        0.0
    }
    /// `scroll-behavior: smooth` (CSSOM View §13.1).
    fn scroll_behavior_smooth(&self) -> bool {
        false
    }
    /// `line-clamp` (CSS Overflow 4 §4), when it makes the box a line-clamp container: `None` for
    /// `none`, and for `-webkit-line-clamp` on a box that was not a vertical `-webkit-box`.
    fn line_clamp(&self) -> Option<LineClamp> {
        None
    }
    /// `column-width` in px (CSS Multi-column Layout 1 §3.1); `None` is `auto`.
    fn column_width(&self) -> Option<f32> {
        None
    }
    /// `column-count` (CSS Multi-column Layout 1 §3.2); `None` is `auto`.
    fn column_count(&self) -> Option<u32> {
        None
    }
    /// `column-fill: auto` (CSS Multi-column Layout 1 §7.1); else `balance`.
    fn column_fill_auto(&self) -> bool {
        false
    }
    /// `column-span: all` (CSS Multi-column Layout 1 §6.1); else `none`.
    fn column_span_all(&self) -> bool {
        false
    }
    /// `column-rule-width` in px (CSS Multi-column Layout 1 §4.3): 0 when `column-rule-style` is
    /// `none` or `hidden`.
    fn column_rule_width(&self) -> f32 {
        0.0
    }
    /// `column-rule-style` (CSS Multi-column Layout 1 §4.2).
    fn column_rule_style(&self) -> BorderStyle {
        BorderStyle::None
    }
    /// `column-rule-color` (CSS Multi-column Layout 1 §4.1), `currentcolor` resolved.
    fn column_rule_color(&self) -> Color {
        self.color()
    }
    /// A multi-column container (CSS Multi-column Layout 1 §2): `column-width` or `column-count`
    /// is not `auto`.
    fn is_multicol(&self) -> bool {
        self.column_width().is_some() || self.column_count().is_some()
    }
    /// `break-before` (CSS Fragmentation 3 §3.1).
    fn break_before(&self) -> BreakBetween {
        BreakBetween::Auto
    }
    /// `break-after` (CSS Fragmentation 3 §3.1).
    fn break_after(&self) -> BreakBetween {
        BreakBetween::Auto
    }
    /// `break-inside` (CSS Fragmentation 3 §3.2).
    fn break_inside(&self) -> BreakInside {
        BreakInside::Auto
    }
    /// `orphans` (CSS Fragmentation 3 §3.3).
    fn orphans(&self) -> u32 {
        2
    }
    /// `widows` (CSS Fragmentation 3 §3.3).
    fn widows(&self) -> u32 {
        2
    }

    // ----- flex (css-flexbox-1) and alignment (css-align-3) -----
    fn flex_direction(&self) -> FlexDirection {
        FlexDirection::Row
    }
    fn flex_wrap(&self) -> FlexWrap {
        FlexWrap::Nowrap
    }
    fn flex_grow(&self) -> f32 {
        0.0
    }
    fn flex_shrink(&self) -> f32 {
        1.0
    }
    fn flex_basis(&self) -> FlexBasis {
        FlexBasis::Auto
    }
    fn order(&self) -> i32 {
        0
    }
    fn justify_content(&self) -> Align {
        Align::Normal
    }
    fn align_content(&self) -> Align {
        Align::Normal
    }
    fn align_items(&self) -> Align {
        Align::Normal
    }
    fn align_self(&self) -> Align {
        Align::Auto
    }
    fn justify_items(&self) -> Align {
        Align::Normal
    }
    fn justify_self(&self) -> Align {
        Align::Auto
    }
    /// The `safe`/`unsafe` keywords of the alignment properties.
    fn align_safety(&self) -> AlignSafety {
        AlignSafety::default()
    }
    /// `row-gap`; `None` is `normal`.
    fn row_gap(&self) -> Option<LengthPercentage> {
        None
    }
    /// `column-gap`; `None` is `normal`.
    fn column_gap(&self) -> Option<LengthPercentage> {
        None
    }

    // ----- grid (css-grid-2) -----
    fn grid_template_columns(&self) -> Cow<'_, TrackList> {
        Cow::Owned(TrackList::default())
    }
    fn grid_template_rows(&self) -> Cow<'_, TrackList> {
        Cow::Owned(TrackList::default())
    }
    /// Whether `grid-template-columns`/`-rows` is `subgrid` (columns, rows): that track list then holds
    /// the subgrid's `<line-name-list>` only (css-grid-2 §9).
    fn grid_template_subgrid(&self) -> (bool, bool) {
        (false, false)
    }
    fn grid_template_areas(&self) -> Option<Cow<'_, GridTemplateAreas>> {
        None
    }
    fn grid_auto_columns(&self) -> Cow<'_, [TrackSize]> {
        Cow::Borrowed(&[])
    }
    fn grid_auto_rows(&self) -> Cow<'_, [TrackSize]> {
        Cow::Borrowed(&[])
    }
    fn grid_auto_flow(&self) -> GridAutoFlow {
        GridAutoFlow::default()
    }
    fn grid_row_start(&self) -> GridLine {
        GridLine::Auto
    }
    fn grid_row_end(&self) -> GridLine {
        GridLine::Auto
    }
    fn grid_column_start(&self) -> GridLine {
        GridLine::Auto
    }
    fn grid_column_end(&self) -> GridLine {
        GridLine::Auto
    }

    // ----- tables (CSS 2 §17, css-tables-3) -----
    fn table_layout(&self) -> TableLayoutMode {
        TableLayoutMode::Auto
    }
    fn border_collapse(&self) -> BorderCollapse {
        BorderCollapse::Separate
    }
    /// `border-spacing`: (horizontal, vertical).
    fn phys_border_spacing(&self) -> (f32, f32) {
        (0.0, 0.0)
    }
    fn caption_side(&self) -> CaptionSide {
        CaptionSide::Top
    }
    fn empty_cells(&self) -> EmptyCells {
        EmptyCells::Show
    }

    // ----- fonts (css-fonts-4) -----
    /// The `font-family` list in order: family names, or generic families (`serif`, `sans-serif`,
    /// `monospace`, …) by their keyword, as the paint crate's `FontQuery` takes them.
    fn font_family(&self) -> impl Iterator<Item = &str> + '_;
    /// Computed `font-size` in px.
    fn font_size(&self) -> f32;
    /// 1..=1000.
    fn font_weight(&self) -> u16;
    fn font_style(&self) -> FontStyle;
    fn font_stretch(&self) -> FontStretch {
        FontStretch::NORMAL
    }
    /// An identity for what the font getters above answer: two handles with the same key have the
    /// same `font-family` list (as resolved against the same web fonts), `font-size`, `font-weight`,
    /// `font-style` and `font-stretch`, so they resolve to the same fonts. The key names that state
    /// only while a clone of this handle is alive (the tree's font cache keeps one with the key).
    /// `None`: no identity, the font is resolved each time.
    fn font_key(&self) -> Option<(usize, usize)> {
        None
    }
    /// `font-feature-settings`, plus what `font-kerning: none` and `font-variant-ligatures` turn off.
    fn font_features(&self) -> Cow<'_, [FontFeature]> {
        Cow::Borrowed(&[])
    }
    /// The content language (BCP 47), for shaping.
    fn locale(&self) -> &str {
        "en-us"
    }

    // ----- text (css-text-3, css-inline-3, CSS 2 §10.8) -----
    fn color(&self) -> Color;
    fn line_height(&self) -> LineHeight;
    fn vertical_align(&self) -> VerticalAlign;
    fn text_box_trim(&self) -> TextBoxTrim {
        TextBoxTrim::default()
    }
    fn text_box_edge(&self) -> TextBoxEdge {
        TextBoxEdge::default()
    }
    fn text_align(&self) -> TextAlign;
    fn text_align_last(&self) -> TextAlignLast {
        TextAlignLast::Auto
    }
    /// The legacy alignment of block-level children, when `text-align` is one of the legacy
    /// keywords (inherited like `text-align`).
    fn legacy_align(&self) -> Option<LegacyAlign> {
        None
    }
    fn text_indent(&self) -> LengthPercentage {
        LengthPercentage::ZERO
    }
    /// `text-indent`'s `hanging` (CSS Text 3 §7.1).
    fn text_indent_hanging(&self) -> bool {
        false
    }
    /// `text-indent`'s `each-line` (CSS Text 3 §7.1).
    fn text_indent_each_line(&self) -> bool {
        false
    }
    fn white_space_collapse(&self) -> WhiteSpaceCollapse;
    fn text_wrap_mode(&self) -> TextWrapMode;
    fn text_wrap_style(&self) -> TextWrapStyle {
        TextWrapStyle::Auto
    }
    fn word_break(&self) -> WordBreak {
        WordBreak::Normal
    }
    fn line_break(&self) -> LineBreak {
        LineBreak::Auto
    }
    fn hyphens(&self) -> Hyphens {
        Hyphens::Manual
    }
    /// `hyphenate-character` (CSS Text 4 §6.2): the string, or `None` for `auto`.
    fn hyphenate_character(&self) -> Option<String> {
        None
    }
    fn overflow_wrap(&self) -> OverflowWrap {
        OverflowWrap::Normal
    }
    fn text_overflow(&self) -> TextOverflow {
        TextOverflow::Clip
    }
    /// What `text-overflow` puts at the end of a clipped line (CSS UI 4 §5.2): U+2026 for
    /// `ellipsis`, the string for a `<string>`; `None` for `clip`.
    fn text_overflow_string(&self) -> Option<String> {
        (self.text_overflow() == TextOverflow::Ellipsis).then(|| "\u{2026}".to_string())
    }
    fn text_transform(&self) -> TextTransform {
        TextTransform::None
    }
    /// Computed `letter-spacing` in px (`normal` is 0).
    fn letter_spacing(&self) -> f32 {
        0.0
    }
    /// Computed `word-spacing` in px (`normal` is 0).
    fn word_spacing(&self) -> f32 {
        0.0
    }
    fn tab_size(&self) -> TabSize {
        TabSize::Spaces(8.0)
    }
    fn text_decoration_line(&self) -> TextDecorationLine {
        TextDecorationLine::NONE
    }
    fn text_decoration_style(&self) -> TextDecorationStyle {
        TextDecorationStyle::Solid
    }
    /// `currentcolor` already resolved.
    fn text_decoration_color(&self) -> Color {
        self.color()
    }
    fn direction(&self) -> Direction;
    fn unicode_bidi(&self) -> UnicodeBidi {
        UnicodeBidi::Normal
    }
    fn writing_mode(&self) -> WritingMode {
        WritingMode::HorizontalTb
    }
    /// `text-orientation` (CSS Writing Modes 4 §5.1; applies in vertical writing modes).
    fn text_orientation(&self) -> TextOrientation {
        TextOrientation::Mixed
    }
    /// `text-combine-upright: all` (§9.1).
    fn text_combine_upright(&self) -> bool {
        false
    }
    /// Whether `other` prepares inline content as this style does: every property preparation
    /// reads is the same (the font, the inherited text properties: white space, wrapping and line
    /// breaking, spacing, `text-transform`, the content language; `direction`, the writing mode and
    /// `text-orientation`; `unicode-bidi`), so the prepared items stay valid with `other` in place
    /// of this style. Properties read at layout (margins, `vertical-align`, `line-height`...) may
    /// differ. `false` when not known (the default): the content is prepared again.
    fn same_inline_preparation(&self, _other: &Self) -> bool {
        false
    }
    /// `clip: rect()` (CSS Masking 1 §6.1) as top, right, bottom and left offsets from the border
    /// box's top-left corner, `None` for an `auto` edge; `None` for `clip: auto`.
    fn clip_rect(&self) -> Option<[Option<f32>; 4]> {
        None
    }

    // ----- lists (css-lists-3) and generated content -----
    fn list_style_type(&self) -> ListStyleType {
        ListStyleType::Disc
    }
    fn list_style_position(&self) -> ListStylePosition {
        ListStylePosition::Outside
    }
    fn list_style_image(&self) -> Image {
        Image::None
    }
    /// `content` (meaningful on `::before`, `::after`, `::marker`).
    fn content(&self) -> Content {
        Content::Normal
    }

    // ----- replaced content (css-images-3) -----
    fn object_fit(&self) -> ObjectFit {
        ObjectFit::Fill
    }
    fn phys_object_position(&self) -> (LengthPercentage, LengthPercentage) {
        (
            LengthPercentage::percent(50.0),
            LengthPercentage::percent(50.0),
        )
    }
    /// `object-view-box` (CSS Images 5 §5.1): the view box as insets (top, right, bottom, left)
    /// from the edges of the natural size (percentages of it); `None` for `none`.
    fn object_view_box(&self) -> Option<[LengthPercentage; 4]> {
        None
    }
    fn image_rendering(&self) -> ImageRendering {
        ImageRendering::Smooth
    }

    // ----- painting (css-backgrounds-3, css-ui-4, css-color-4, css-transforms-1) -----
    fn visibility(&self) -> Visibility;
    fn opacity(&self) -> f32 {
        1.0
    }
    fn background_color(&self) -> Color {
        Color::TRANSPARENT
    }
    /// How many `background-image` layers there are (the length of the `background-image` list).
    fn background_layer_count(&self) -> usize {
        0
    }
    /// Layer `index`, 0 being the top layer (the first in the CSS list).
    fn background_layer(&self, _index: usize) -> BackgroundLayer {
        BackgroundLayer::default()
    }
    /// The bottom layer's `background-clip` and `background-attachment`, which place the
    /// background color (CSS Backgrounds 3 §3.10), also when no layer has an image.
    fn background_color_area(&self) -> (BoxArea, BackgroundAttachment) {
        match self.background_layer_count() {
            0 => (BoxArea::BorderBox, BackgroundAttachment::Scroll),
            n => {
                let l = self.background_layer(n - 1);
                (l.clip, l.attachment)
            }
        }
    }
    /// How many `mask-image` layers there are (CSS Masking 1 §7; none when the only one is `none`).
    fn mask_layer_count(&self) -> usize {
        0
    }
    /// Mask layer `index`, 0 being the top layer.
    fn mask_layer(&self, _index: usize) -> MaskLayer {
        MaskLayer::default()
    }
    /// `border-image`, when its source is an image that can be drawn (else the border styles
    /// paint).
    fn border_image(&self) -> Option<BorderImage> {
        None
    }
    fn box_shadow_count(&self) -> usize {
        0
    }
    fn box_shadow(&self, _index: usize) -> Shadow {
        Shadow {
            offset_x: 0.0,
            offset_y: 0.0,
            blur: 0.0,
            spread: 0.0,
            color: Color::TRANSPARENT,
            inset: false,
        }
    }
    /// How many `text-shadow`s the text has (CSS Text Decoration 3 §4).
    fn text_shadow_count(&self) -> usize {
        0
    }
    /// One `text-shadow` (no spread, never inset); the first is painted on top.
    fn text_shadow(&self, _index: usize) -> Shadow {
        Shadow {
            offset_x: 0.0,
            offset_y: 0.0,
            blur: 0.0,
            spread: 0.0,
            color: Color::TRANSPARENT,
            inset: false,
        }
    }
    /// The computed outline width (0 when `outline-style` is `none`).
    fn outline_width(&self) -> f32 {
        0.0
    }
    /// `auto` maps to `solid`.
    fn outline_style(&self) -> BorderStyle {
        BorderStyle::None
    }
    fn outline_color(&self) -> Color {
        self.color()
    }
    fn outline_offset(&self) -> f32 {
        0.0
    }
    /// Whether `transform` (or `translate`/`rotate`/`scale`) is not `none`.
    fn has_transform(&self) -> bool {
        false
    }
    /// The transform as a 2D matrix in the box's local coordinates (origin at the border box's top-left),
    /// with `transform-origin` applied, for a reference box (the border box) `reference`. `None` when
    /// there is no transform. 3D transforms are flattened.
    fn phys_transform(&self, _reference: Rect) -> Option<Matrix> {
        None
    }
    /// [`Style::transform`] with the box's containing block known: what `offset-path` needs
    /// (CSS Motion Path 1).
    fn transform_in(
        &self,
        reference: Rect,
        _containing: Option<&ContainingBoxes>,
    ) -> Option<Matrix> {
        self.transform(reference)
    }
    /// Whether `transform-box` makes the content box the transform's reference box (`content-box`,
    /// `fill-box`) rather than the border box (CSS Transforms 1 §6.2).
    fn transform_box_is_content(&self) -> bool {
        false
    }
    /// Whether the box's transform depends on its containing block.
    fn transform_needs_containing(&self) -> bool {
        false
    }
    /// Whether the box is the containing block of its absolutely and fixed positioned descendants
    /// whatever its `position` (css-position-3 §2.1): a transform (css-transforms-1 §2, -2 §6 for
    /// `perspective` and `transform-style: preserve-3d`), a filter or backdrop filter (Filter
    /// Effects 1 §5, 2 §3), layout or paint containment (CSS Contain 2 §3.2, §3.3, also through
    /// `container-type`), or `will-change` naming one of those (CSS Will Change 1 §3).
    fn establishes_fixed_cb(&self) -> bool {
        self.has_transform()
    }
    /// Whether some other property makes a stacking context (`filter`, `isolation: isolate`,
    /// `mix-blend-mode`, `mask`, `clip-path`, `contain: paint`, `will-change` naming one of those).
    fn forces_stacking_context(&self) -> bool {
        false
    }
    /// `will-change` names `transform`, `rotate`, `scale`, `translate` or `opacity`: a direct
    /// compositing reason (Blink's `CompositingReasonsForWillChange`).
    fn will_change_composited(&self) -> bool {
        false
    }
    /// The used value of `pointer-events` is not `none` (an inert box's is: HTML §6.3, inert nodes
    /// are absent for targeting user interaction events).
    fn pointer_events(&self) -> bool {
        true
    }
    /// The box is in an inert subtree (HTML §6.3): not hit, and its text not selectable.
    fn inert(&self) -> bool {
        false
    }
    /// `clip-path`; `none`, and a `url()` that names no `<clipPath>`, clip nothing.
    fn clip_path(&self) -> Option<ClipPath> {
        None
    }
    /// `filter`'s functions in order (Filter Effects 1 §10), lengths in px and colors resolved;
    /// empty for `none`, and for a list with a `url()` that names no `<filter>` (§5: the whole
    /// list is ignored). `reference_box` is the border box, where a `url()`'s filter resolves.
    fn filter_ops(&self, _reference_box: Rect) -> Vec<FilterOp> {
        Vec::new()
    }
    /// Whether [`Style::filter_ops`] has a function (without building the list).
    fn has_filter(&self) -> bool {
        false
    }
    /// Whether the element is a backdrop root (Filter Effects 2 §3.1) by something other than an
    /// opacity below 1, a filter or a mask, which group what it paints anyway: a clip path, a
    /// backdrop filter, a blend mode, or `will-change` naming a property that makes one.
    fn forms_backdrop_root(&self) -> bool {
        false
    }
    /// `backdrop-filter`'s functions in order (Filter Effects 2 §3), as [`Style::filter_ops`].
    fn backdrop_filter_ops(&self, _reference_box: Rect) -> Vec<FilterOp> {
        Vec::new()
    }
    /// Whether [`Style::clip_path`] is `Some` (without building it).
    fn has_clip_path(&self) -> bool {
        self.clip_path().is_some()
    }

    // ----- user agent drawing (css-color-adjust-1, css-scrollbars-1) -----
    /// The used color scheme.
    fn color_scheme(&self) -> ColorScheme {
        ColorScheme::Light
    }
    /// `scrollbar-color`: `None` for `auto` (the user agent's colors for the used color scheme).
    fn scrollbar_color(&self) -> Option<ScrollbarColors> {
        None
    }
    fn scrollbar_width(&self) -> ScrollbarWidth {
        ScrollbarWidth::Auto
    }
    /// The element's custom scroll bars, when a rule styles its `::-webkit-scrollbar` (only
    /// asked of scroll containers).
    fn custom_scrollbar(&self) -> Option<CustomScrollbar> {
        None
    }
    /// The used `resize` (CSS UI 4 §5.1).
    fn resize(&self) -> Resize {
        Resize::None
    }
}

/// Where layout gets styles. Implemented by the page (over stylo and the image cache) and by
/// [`crate::TestStyles`].
/// How a `details` element's children divide between its two slots (HTML 15.5.5).
#[derive(Clone, Debug, Default)]
pub struct DetailsParts {
    /// The first `summary` child, which the first slot takes.
    pub summary: Option<snowlark_dom::NodeId>,
    /// Every other child, which the second slot takes (shown by `::details-content`).
    pub content: Vec<snowlark_dom::NodeId>,
    /// Whether the element has the `open` attribute (its default summary shows the open marker).
    pub open: bool,
}

pub trait StyleSource {
    type Style: Style;

    /// An element's computed style; `None` when the element has none (it is in a `display: none`
    /// subtree, or isn't styled).
    fn element_style(&self, element: snowlark_dom::NodeId) -> Option<Self::Style>;

    /// A pseudo-element's style, or `None` when it doesn't exist (for `::before`/`::after`: when
    /// `content` computes to `none`/`normal`; for `::marker`: when the element isn't a list item).
    fn pseudo_style(
        &self,
        element: snowlark_dom::NodeId,
        pseudo: PseudoElement,
    ) -> Option<Self::Style>;

    /// The style of an anonymous box inside a box with style `parent`.
    fn anonymous_style(&self, parent: &Self::Style, kind: AnonymousBox) -> Self::Style;
    /// Whether any rule beyond the user agent's may style `::marker`. When none does, a list item
    /// whose `list-style-type` is `none` with no `list-style-image` has no marker, and its
    /// `::marker` style is not asked for.
    fn may_style_markers(&self) -> bool {
        true
    }

    /// The replaced content of an element, `None` for a non-replaced element.
    fn replaced_content(&self, _element: snowlark_dom::NodeId) -> Option<ReplacedContent> {
        None
    }

    /// The text a non-replaced element shows in place of its children: an `img` that represents
    /// its `alt` text (HTML §15.4.4, "a non-replaced phrasing element whose content is the text").
    fn alt_text(&self, _element: snowlark_dom::NodeId) -> Option<String> {
        None
    }

    /// The children of `parent` in the flat tree (CSS Scoping 1 §3.1), when they are not its DOM
    /// children: a shadow host's are its shadow root's children, a slot's are its assigned nodes
    /// (when it has any; else its own children, the fallback content). `None`: the DOM children.
    fn flat_children(&self, _parent: snowlark_dom::NodeId) -> Option<Vec<snowlark_dom::NodeId>> {
        None
    }

    /// The slots of an HTML `details` element, `None` for any other element. A `details` element
    /// renders its summary, then a `::details-content` box around the rest.
    fn details_parts(&self, _element: snowlark_dom::NodeId) -> Option<DetailsParts> {
        None
    }

    /// Whether `element` is drawn by the parts of its internal tree rather than its children: a
    /// `progress` or `meter` element that is not a native control (`appearance: none`).
    fn has_form_parts(&self, _element: snowlark_dom::NodeId) -> bool {
        false
    }

    /// The parts of such an element, outermost first, each inside the one before it, with their
    /// styles (`None` for other elements). They are rendered instead of the element's children.
    fn form_parts(&self, _element: snowlark_dom::NodeId) -> Option<Vec<(FormPart, Self::Style)>> {
        None
    }

    /// The document's top layer (CSS Position 4 §4.1), bottom first: elements rendered above
    /// everything else, each over its `::backdrop`, instead of where they are in the tree.
    fn top_layer(&self) -> &[snowlark_dom::NodeId] {
        &[]
    }

    /// The document's fullscreen element (Fullscreen API): while it is not the root element, the
    /// viewport's overflow is hidden (no scrollbar beside the element covering it).
    fn fullscreen_element(&self) -> Option<snowlark_dom::NodeId> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{LengthPercentage, Linear};

    /// What `resolve` gave when it tested each bound before applying it.
    fn tested_first(lp: &LengthPercentage, basis: f32) -> f32 {
        let mut v = lp.length + lp.percent.map_or(0.0, |p| p * basis);
        if let Some(u) = lp.upper {
            v = v.min(u.at(basis));
        }
        if let Some(l) = lp.lower {
            v = v.max(l.at(basis));
        }
        v
    }

    #[test]
    fn absent_bounds_resolve_as_if_tested_first() {
        let bound = Some(Linear {
            length: 5.0,
            percent: 0.25,
        });
        let mut values = Vec::new();
        for percent in [None, Some(0.0), Some(0.5), Some(-0.5)] {
            for (lower, upper) in [(None, None), (bound, None), (None, bound), (bound, bound)] {
                for length in [0.0, -0.0, 12.0, -3.0] {
                    values.push(LengthPercentage {
                        length,
                        percent,
                        lower,
                        upper,
                    });
                }
            }
        }
        let bases = [
            0.0,
            -0.0,
            100.0,
            -40.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            f32::MAX,
        ];
        for lp in &values {
            for basis in bases {
                let (got, want) = (lp.resolve(basis), tested_first(lp, basis));
                assert!(
                    got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()),
                    "{lp:?} at {basis}: {got} against {want}"
                );
            }
        }
    }
}
