// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`RenderStyle`]: stylo's `ComputedValues` as the layout crate's [`Style`].
//!
//! Each getter reads the typed computed value from the style struct (no serialization, no allocation
//! for the common cases) and maps it to layout's vocabulary: `<length-percentage>` to a length plus a
//! fraction (a linear `calc()` exactly; a non-linear one by its value at two bases), colors resolved
//! against `currentcolor` and converted to sRGB, `url()` images looked up in the document's
//! [`ImageTable`](crate::env::ImageTable), `font-family` names declared by `@font-face` replaced by the
//! text engine's internal names of the loaded faces (best match first, CSS Fonts 4 §5.2).

use std::borrow::Cow;
use std::sync::Arc;

use servo_arc::Arc as ServoArc;
use smallvec::SmallVec;
use snowlark_layout::style::{self as ls, Corner, Side};
use snowlark_paint::{Color, FilterOp, FontFeature, FontStretch, FontStyle, Matrix, Point, Rect};
use style::properties::ComputedValues;
use style::values::computed::font::{GenericFontFamily, SingleFontFamily};
use style::values::computed::{self as c, Length};

use crate::env::{face_rank, StyleEnv, Wanted};

/// A cheap handle (three pointers) to an element's, pseudo-element's or anonymous box's computed
/// values, implementing [`Style`](snowlark_layout::Style).
#[derive(Clone)]
pub struct RenderStyle {
    pub(crate) values: ServoArc<ComputedValues>,
    pub(crate) env: Arc<StyleEnv>,
    /// The computed `content` of a `::before`/`::after`/`::marker` with counters and quotes already
    /// resolved (they depend on the element's place in the document, not only on its style).
    pub(crate) content: Option<Arc<ls::Content>>,
    /// The `<clipPath>` a `clip-path: url()` names, when it names one (it depends on the document,
    /// not only on the style: see [`crate::clip_ref`]; set by the style source).
    pub(crate) clip_reference: Option<Arc<ls::ClipReference>>,
    /// What the `url()`s of `filter` and `backdrop-filter` name: each URL with its `<filter>`, or
    /// `None` for a URL that names none.
    pub(crate) filter_references: Option<Arc<crate::renderer::FilterReferences>>,
    /// The element's last remembered size (CSS Sizing 4 5.2.1), inline and block, when it has one
    /// (it depends on the element, not its style: set by the style source).
    pub(crate) remembered: Option<(Option<f32>, Option<f32>)>,
    /// The flow frame layout reads the box properties through ([`ls::Style::view`]).
    pub(crate) view: ls::Flip,
    /// What the element's current animations animate
    /// ([`snowlark_style::StyleEngine::animation_flags`]; set by the style source).
    pub(crate) animations: u8,
    /// A scroll container's custom scroll bars (its `::-webkit-scrollbar` parts' styles; set by
    /// the style source).
    pub(crate) custom_scrollbar: Option<ls::CustomScrollbar>,
}

impl std::fmt::Debug for RenderStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderStyle").finish_non_exhaustive()
    }
}

/// What the reference box of an SVG element's transform is (CSS Transforms 1 §3 `transform-box`):
/// the object bounding box, the stroke bounding box, or the nearest viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SvgReferenceBox {
    Fill,
    Stroke,
    View,
}

pub(crate) fn svg_reference_box(values: &ComputedValues) -> SvgReferenceBox {
    use style::values::computed::TransformBox as B;
    match values.get_box().transform_box {
        B::FillBox | B::ContentBox => SvgReferenceBox::Fill,
        B::StrokeBox | B::BorderBox => SvgReferenceBox::Stroke,
        B::ViewBox => SvgReferenceBox::View,
    }
}

impl RenderStyle {
    /// A computed `filter` or `backdrop-filter` list's functions, lengths in px and colors
    /// resolved, a `url()`'s filter for the border box `reference_box`; empty when a `url()`
    /// names no `<filter>` (Filter Effects 1 §5: the whole list is ignored).
    fn filter_list_ops(
        &self,
        list: &[style::values::computed::effects::Filter],
        reference_box: Rect,
    ) -> Vec<FilterOp> {
        use style::values::generics::effects::Filter as F;
        list.iter()
            .map(|f| {
                Some(match f {
                    F::Blur(l) => FilterOp::Blur(l.0.px()),
                    F::Brightness(a) => FilterOp::Brightness(a.0),
                    F::Contrast(a) => FilterOp::Contrast(a.0),
                    F::Grayscale(a) => FilterOp::Grayscale(a.0),
                    F::HueRotate(a) => FilterOp::HueRotate(a.degrees()),
                    F::Invert(a) => FilterOp::Invert(a.0),
                    F::Opacity(a) => FilterOp::Opacity(a.0),
                    F::Saturate(a) => FilterOp::Saturate(a.0),
                    F::Sepia(a) => FilterOp::Sepia(a.0),
                    F::DropShadow(s) => FilterOp::DropShadow {
                        offset: Point::new(s.horizontal.px(), s.vertical.px()),
                        sigma: s.blur.0.px(),
                        color: self.color_of(&s.color),
                    },
                    F::Url(url) => {
                        let url = crate::renderer::url_text(url);
                        let found = self
                            .filter_references
                            .as_ref()
                            .and_then(|refs| refs.iter().find(|(u, _)| *u == url))
                            .and_then(|(_, r)| r.as_ref());
                        return found.map(|r| FilterOp::Svg(Arc::new(r.resolve(reference_box))));
                    }
                })
            })
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    }

    /// CSS Contain 2 §4: `content-visibility: auto` and `hidden` give the element layout, style
    /// and paint containment.
    fn content_visibility_contains(&self) -> bool {
        use style::values::computed::ContentVisibility;
        self.values.get_box().clone_content_visibility() != ContentVisibility::Visible
    }

    /// A handle without generated content.
    pub fn new(values: ServoArc<ComputedValues>, env: Arc<StyleEnv>) -> RenderStyle {
        RenderStyle {
            values,
            env,
            content: None,
            clip_reference: None,
            filter_references: None,
            remembered: None,
            view: ls::Flip::IDENTITY,
            animations: 0,
            custom_scrollbar: None,
        }
    }

    /// The stylo computed values.
    pub fn values(&self) -> &ServoArc<ComputedValues> {
        &self.values
    }

    /// CSS Contain 3 §2: the physical axes (width, height) in which this element is a size query
    /// container (`container-type: size` both, `inline-size` the inline axis), if it is one.
    pub fn query_container_axes(&self) -> Option<(bool, bool)> {
        use style::values::computed::ContainerType;
        let t = self.values.get_box().clone_container_type();
        let horizontal = self.values.writing_mode.is_horizontal();
        if t.contains(ContainerType::SIZE) {
            Some((true, true))
        } else if t.contains(ContainerType::INLINE_SIZE) {
            Some((horizontal, !horizontal))
        } else {
            None
        }
    }

    /// The same values and generated content with another environment (a new web font table).
    pub fn with_env(&self, env: Arc<StyleEnv>) -> RenderStyle {
        RenderStyle {
            values: self.values.clone(),
            env,
            content: self.content.clone(),
            clip_reference: self.clip_reference.clone(),
            filter_references: self.filter_references.clone(),
            remembered: self.remembered,
            view: self.view,
            animations: self.animations,
            custom_scrollbar: self.custom_scrollbar,
        }
    }

    /// Whether `font-family` names one of `families` (ASCII case-insensitively).
    pub fn names_family(&self, families: &[String]) -> bool {
        self.values
            .get_font()
            .font_family
            .families
            .list
            .iter()
            .any(|f| match f {
                SingleFontFamily::FamilyName(n) => {
                    families.iter().any(|w| (*n.name).eq_ignore_ascii_case(w))
                }
                SingleFontFamily::Generic(_) => false,
            })
    }

    fn color_of(&self, color: &c::Color) -> Color {
        to_color(&self.values.resolve_color(color))
    }

    /// Whether the element's used color scheme is dark.
    fn is_dark(&self) -> bool {
        used_color_scheme(&self.values) == ls::ColorScheme::Dark
    }

    /// A computed `<image>` as layout paints it (`None` for one that is not available).
    pub(crate) fn image(&self, image: &c::Image) -> ls::Image {
        match image {
            c::Image::Url(url) => {
                use style::properties::longhands::image_orientation::computed_value::T;
                let from_image = self.values.get_inherited_box().image_orientation == T::FromImage;
                url.url()
                    .and_then(|u| self.env.images.get(u.as_str()))
                    .map(|i| i.oriented(from_image, false).for_scheme(self.is_dark()))
                    .map_or(ls::Image::None, |i| ls::Image::Raster {
                        id: i.id,
                        natural: i.natural,
                    })
            }
            c::Image::Gradient(g) => self.gradient(g),
            // CSS Images 4 §2.2: the chosen image's natural dimensions are divided by its
            // resolution (a `2x` image is half its pixel size).
            c::Image::ImageSet(set) => {
                set.items
                    .get(set.selected_index)
                    .map_or(ls::Image::None, |item| match self.image(&item.image) {
                        ls::Image::Raster { id, natural } => {
                            let dppx = item.resolution.dppx();
                            let scale = |v: Option<f32>| {
                                v.map(|v| {
                                    if dppx > 0.0 && dppx.is_finite() {
                                        v / dppx
                                    } else {
                                        v
                                    }
                                })
                            };
                            ls::Image::Raster {
                                id,
                                natural: ls::NaturalSize {
                                    width: scale(natural.width),
                                    height: scale(natural.height),
                                    ratio: natural.ratio,
                                },
                            }
                        }
                        other => other,
                    })
            }
            c::Image::LightDark(ld) => match used_color_scheme(&self.values) {
                ls::ColorScheme::Light => self.image(&ld.light),
                ls::ColorScheme::Dark => self.image(&ld.dark),
            },
            // CSS Images 4 §2.4 `image(<color>)`: a solid color image.
            c::Image::Image(color) => ls::Image::Color(self.color_of(color)),
            c::Image::CrossFade(cf) => self.cross_fade(cf),
            // CSS Images 4 §2.3: the source when it is available, else the color (the transparent
            // image without one).
            c::Image::Fallback(f) => f
                .source
                .as_ref()
                .map(|i| self.image(i))
                .filter(|i| *i != ls::Image::None)
                .unwrap_or_else(|| {
                    ls::Image::Color(
                        f.color
                            .as_ref()
                            .map_or(Color::TRANSPARENT, |c| self.color_of(c)),
                    )
                }),
            _ => ls::Image::None,
        }
    }

    /// A gradient's color stops and transition hints, and the color ramps between adjacent color
    /// stops when the gradient does not interpolate in sRGB (CSS Images 4 §3.5.2: in its
    /// `<color-interpolation-method>`, whose default stylo resolved: sRGB for legacy colors,
    /// else Oklab), sampled with stylo's color mixing.
    fn stops<T>(
        &self,
        items: &[style::values::generics::image::GradientItem<c::Color, T>],
        method: &style::color::mix::ColorInterpolationMethod,
        position: impl Fn(&T) -> Option<ls::LengthPercentage>,
    ) -> (Vec<ls::ColorStop>, Vec<ls::Ramp>) {
        use style::color::mix::{mix_many, ColorMixItem};
        use style::color::{ColorFlags, ColorSpace};
        use style::values::generics::color::ColorMixFlags;
        use style::values::generics::image::GradientItem;
        let mut out = Vec::with_capacity(items.len());
        let mut colors = Vec::with_capacity(items.len());
        for item in items {
            match item {
                GradientItem::SimpleColorStop(color) => {
                    let c = self.values.resolve_color(color);
                    out.push(ls::ColorStop::new(to_color(&c), None));
                    colors.push(c);
                }
                GradientItem::ComplexColorStop { color, position: p } => {
                    let c = self.values.resolve_color(color);
                    out.push(ls::ColorStop::new(to_color(&c), position(p)));
                    colors.push(c);
                }
                GradientItem::InterpolationHint(p) => out.push(ls::ColorStop {
                    color: Color::TRANSPARENT,
                    position: position(p),
                    hint: true,
                }),
            }
        }
        // (premultiplied sRGB is what the rasterizer blends in; missing components (`none`) take
        // the other color's, which a straight blend would not do)
        let none = ColorFlags::C0_IS_NONE
            | ColorFlags::C1_IS_NONE
            | ColorFlags::C2_IS_NONE
            | ColorFlags::ALPHA_IS_NONE;
        let missing = colors.iter().any(|c| c.flags.intersects(none));
        if (method.space == ColorSpace::Srgb && !missing) || colors.len() < 2 {
            return (out, Vec::new());
        }
        let ramps = colors
            .windows(2)
            .map(|w| {
                std::array::from_fn(|k| {
                    let t = k as f32 / ls::RAMP_STEPS as f32;
                    let mixed = mix_many(
                        *method,
                        [ColorMixItem::new(w[0], 1.0 - t), ColorMixItem::new(w[1], t)],
                        ColorMixFlags::empty(),
                    );
                    to_color(&mixed)
                })
            })
            .collect();
        (out, ramps)
    }

    fn gradient(&self, g: &c::Gradient) -> ls::Image {
        use style::values::computed::image::LineDirection;
        use style::values::generics::image::{
            Circle, Ellipse, EndingShape, GenericGradient, GradientFlags, ShapeExtent,
        };
        use style::values::specified::position::{
            HorizontalPositionKeyword, VerticalPositionKeyword,
        };
        match g {
            GenericGradient::Linear {
                direction,
                color_interpolation_method,
                items,
                flags,
                ..
            } => {
                // CSS Images 3 §3.1.1: degrees clockwise from "to top"; a corner's angle depends
                // on the box (layout computes it).
                let (angle, corner) = match direction {
                    LineDirection::Angle(a) => (a.degrees(), None),
                    LineDirection::Horizontal(HorizontalPositionKeyword::Left) => (270.0, None),
                    LineDirection::Horizontal(HorizontalPositionKeyword::Right) => (90.0, None),
                    LineDirection::Vertical(VerticalPositionKeyword::Top) => (0.0, None),
                    LineDirection::Vertical(VerticalPositionKeyword::Bottom) => (180.0, None),
                    LineDirection::Corner(h, v) => (
                        0.0,
                        Some((
                            *h == HorizontalPositionKeyword::Right,
                            *v == VerticalPositionKeyword::Bottom,
                        )),
                    ),
                };
                let (stops, ramps) = self.stops(items, color_interpolation_method, |p| Some(lp(p)));
                ls::Image::LinearGradient(Arc::new(ls::LinearGradient {
                    angle,
                    corner,
                    stops,
                    ramps,
                    repeating: flags.contains(GradientFlags::REPEATING),
                }))
            }
            GenericGradient::Radial {
                shape,
                position,
                color_interpolation_method,
                items,
                flags,
                ..
            } => {
                let extent = |e: &ShapeExtent| match e {
                    ShapeExtent::ClosestSide | ShapeExtent::Contain => ls::RadialSize::ClosestSide,
                    ShapeExtent::FarthestSide => ls::RadialSize::FarthestSide,
                    ShapeExtent::ClosestCorner => ls::RadialSize::ClosestCorner,
                    ShapeExtent::FarthestCorner | ShapeExtent::Cover => {
                        ls::RadialSize::FarthestCorner
                    }
                };
                let (circle, size) = match shape {
                    EndingShape::Circle(Circle::Radius(r)) => (
                        true,
                        ls::RadialSize::Explicit(
                            ls::LengthPercentage::px(r.0.px()),
                            ls::LengthPercentage::px(r.0.px()),
                        ),
                    ),
                    EndingShape::Circle(Circle::Extent(e)) => (true, extent(e)),
                    EndingShape::Ellipse(Ellipse::Radii(x, y)) => {
                        (false, ls::RadialSize::Explicit(lp(&x.0), lp(&y.0)))
                    }
                    EndingShape::Ellipse(Ellipse::Extent(e)) => (false, extent(e)),
                };
                let (stops, ramps) = self.stops(items, color_interpolation_method, |p| Some(lp(p)));
                ls::Image::RadialGradient(Arc::new(ls::RadialGradient {
                    circle,
                    size,
                    center: (lp(&position.horizontal), lp(&position.vertical)),
                    stops,
                    ramps,
                    repeating: flags.contains(GradientFlags::REPEATING),
                }))
            }
            GenericGradient::Conic {
                angle,
                position,
                color_interpolation_method,
                items,
                flags,
            } => {
                // CSS Images 4 §3.3.1: stop angles as fractions of a turn (a percentage of 1).
                let (stops, ramps) = self.stops(items, color_interpolation_method, |p| {
                    let turn = match p {
                        c::AngleOrPercentage::Percentage(p) => p.0,
                        c::AngleOrPercentage::Angle(a) => a.degrees() / 360.0,
                    };
                    Some(ls::LengthPercentage::linear(0.0, Some(turn)))
                });
                ls::Image::ConicGradient(Arc::new(ls::ConicGradient {
                    angle: angle.degrees(),
                    center: (lp(&position.horizontal), lp(&position.vertical)),
                    stops,
                    ramps,
                    repeating: flags.contains(GradientFlags::REPEATING),
                }))
            }
        }
    }

    /// `cross-fade()` (CSS Images 4 §2.6.1): each argument's fraction (omitted ones share what the
    /// given ones leave of 100%; all scaled down when they add up to more), and the natural
    /// dimensions: each the fraction-weighted average over the images that have it.
    fn cross_fade(&self, cf: &c::image::CrossFade) -> ls::Image {
        use style::values::generics::image::CrossFadeImage;
        let given: f32 = cf
            .elements
            .iter()
            .filter_map(|e| e.percent.as_ref().map(|p| p.0))
            .sum();
        let omitted = cf.elements.iter().filter(|e| e.percent.is_none()).count();
        let share = if omitted > 0 {
            (1.0 - given).max(0.0) / omitted as f32
        } else {
            0.0
        };
        let mut layers: Vec<(f32, ls::Image)> = cf
            .elements
            .iter()
            .map(|e| {
                let fraction = e.percent.as_ref().map_or(share, |p| p.0.clamp(0.0, 1.0));
                let image = match &e.image {
                    CrossFadeImage::Image(i) => self.image(i),
                    CrossFadeImage::Color(color) => ls::Image::Color(self.color_of(color)),
                };
                (fraction, image)
            })
            .collect();
        let total: f32 = layers.iter().map(|l| l.0).sum();
        if total > 1.0 {
            for l in &mut layers {
                l.0 /= total;
            }
        }
        let weighted = |dim: fn(&ls::NaturalSize) -> Option<f32>| {
            let (sum, weight) = layers
                .iter()
                .filter_map(|(f, i)| dim(&i.natural()).map(|v| (v * f, *f)))
                .fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
            (weight > 0.0).then(|| sum / weight)
        };
        let natural = ls::NaturalSize {
            width: weighted(|n| n.width),
            height: weighted(|n| n.height),
            ratio: None,
        };
        ls::Image::CrossFade(Arc::new(ls::CrossFade { layers, natural }))
    }

    fn side_width(&self, side: Side) -> f32 {
        let b = self.values.get_border();
        let (w, s) = match side {
            Side::Top => (&b.border_top_width, b.border_top_style),
            Side::Right => (&b.border_right_width, b.border_right_style),
            Side::Bottom => (&b.border_bottom_width, b.border_bottom_style),
            Side::Left => (&b.border_left_width, b.border_left_style),
        };
        if s.none_or_hidden() {
            0.0
        } else {
            w.0.to_f32_px()
        }
    }
}

/// Whether `values` has a transform: `transform`, `translate`, `rotate`, `scale` or `offset-path`.
pub(crate) fn has_css_transform(values: &ComputedValues) -> bool {
    use style::values::generics::motion::GenericOffsetPath;
    use style::values::generics::transform::{GenericRotate, GenericScale, GenericTranslate};
    let b = values.get_box();
    !b.transform.0.is_empty()
        || !matches!(b.translate, GenericTranslate::None)
        || !matches!(b.rotate, GenericRotate::None)
        || !matches!(b.scale, GenericScale::None)
        || !matches!(b.offset_path, GenericOffsetPath::None)
}

/// The 2D transformation matrix of `values` about `transform-origin` in the `reference` box (CSS
/// Transforms 2 §4), `None` without a transform.
pub(crate) fn css_transform(
    values: &ComputedValues,
    reference: Rect,
    containing: Option<&ls::ContainingBoxes>,
    motion_path: Option<&str>,
) -> Option<Matrix> {
    if !has_css_transform(values) {
        return None;
    }
    let b = values.get_box();
    let basis = euclid::default::Rect::new(
        euclid::default::Point2D::new(Length::new(reference.x), Length::new(reference.y)),
        euclid::default::Size2D::new(Length::new(reference.width), Length::new(reference.height)),
    );
    let origin = transform_origin(values, reference);
    let (ox, oy) = (origin.x, origin.y);
    // CSS Transforms 2 §4: the matrix is translate × rotate × scale × offset × transform about the
    // origin, so a point goes through `transform` first and `translate` last; with row vectors the
    // matrix applied first comes first in `then`.
    let mut m = Matrix::IDENTITY;
    if let Ok((t, _)) = b.transform.to_transform_3d_matrix(Some(&basis)) {
        m = Matrix::new(t.m11, t.m12, t.m21, t.m22, t.m41, t.m42);
    }
    if let Some(offset) =
        crate::motion::offset_matrix(values, reference, origin, containing, motion_path)
    {
        m = m.then(&offset);
    }
    m = m.then(&individual_transforms(values, reference));
    let full = Matrix::translation(-ox, -oy)
        .then(&m)
        .then(&Matrix::translation(ox, oy));
    full.is_finite().then_some(full)
}

/// `transform-origin` in the `reference` box's coordinates.
pub(crate) fn transform_origin(values: &ComputedValues, reference: Rect) -> Point {
    let o = &values.get_box().transform_origin;
    Point::new(
        reference.x + o.horizontal.resolve(Length::new(reference.width)).px(),
        reference.y + o.vertical.resolve(Length::new(reference.height)).px(),
    )
}

/// `scale`, then `rotate`, then `translate` (CSS Transforms 2 §4: what follows `transform` and the
/// motion path offset), as a row-vector matrix.
pub(crate) fn individual_transforms(values: &ComputedValues, reference: Rect) -> Matrix {
    use style::values::generics::transform::{GenericRotate, GenericScale, GenericTranslate};
    let b = values.get_box();
    let mut m = Matrix::IDENTITY;
    match &b.scale {
        GenericScale::None => {}
        GenericScale::Scale(x, y, _) => m = m.then(&Matrix::scale(*x, *y)),
    }
    match &b.rotate {
        GenericRotate::None => {}
        GenericRotate::Rotate(a) => m = m.then(&Matrix::rotation(a.radians())),
        GenericRotate::Rotate3D(_, _, z, a) => {
            // Only the z-axis component flattens to 2D.
            if *z != 0.0 {
                m = m.then(&Matrix::rotation(a.radians() * z.signum()));
            }
        }
    }
    match &b.translate {
        GenericTranslate::None => {}
        GenericTranslate::Translate(x, y, _) => {
            let tx = x.resolve(Length::new(reference.width)).px();
            let ty = y.resolve(Length::new(reference.height)).px();
            m = m.then(&Matrix::translation(tx, ty));
        }
    }
    m
}

/// A computed `break-before` or `break-after` value.
fn break_between(v: style::values::computed::BreakBetween) -> ls::BreakBetween {
    use style::values::computed::BreakBetween as B;
    match v {
        B::Auto => ls::BreakBetween::Auto,
        B::Avoid => ls::BreakBetween::Avoid,
        B::AvoidPage => ls::BreakBetween::AvoidPage,
        B::AvoidColumn => ls::BreakBetween::AvoidColumn,
        B::Always => ls::BreakBetween::Always,
        B::Page => ls::BreakBetween::Page,
        B::Left => ls::BreakBetween::Left,
        B::Right => ls::BreakBetween::Right,
        B::Recto => ls::BreakBetween::Recto,
        B::Verso => ls::BreakBetween::Verso,
        B::Column => ls::BreakBetween::Column,
    }
}

/// An sRGB color with 8-bit channels.
pub(crate) fn to_color(color: &style::color::AbsoluteColor) -> Color {
    let srgb = color.into_srgb_legacy();
    let [r, g, b, a] = *srgb.raw_components();
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::rgba(byte(r), byte(g), byte(b), byte(a))
}

/// The commands of `path()` or `shape()`, which differ in how they spell positions, lengths and
/// angles.
fn path_commands<A, P, L>(
    commands: &[style::values::generics::basic_shape::GenericShapeCommand<A, P, L>],
    position: &dyn Fn(&P) -> (ls::LengthPercentage, ls::LengthPercentage),
    length: &dyn Fn(&L) -> ls::LengthPercentage,
    angle: &dyn Fn(&A) -> f32,
) -> Vec<ls::PathCommand> {
    use style::values::generics::basic_shape::{
        ArcSize, ArcSweep, AxisEndPoint, AxisPosition, AxisPositionKeyword as K, CommandEndPoint,
        ControlPoint, ControlReference, GenericShapeCommand as S,
    };
    let end = |p: &CommandEndPoint<P, L>| match p {
        CommandEndPoint::ToPosition(p) => {
            let (x, y) = position(p);
            ls::PathPoint {
                x,
                y,
                relative: false,
            }
        }
        CommandEndPoint::ByCoordinate(c) => ls::PathPoint {
            x: length(&c.x),
            y: length(&c.y),
            relative: true,
        },
    };
    let control = |c: &ControlPoint<P, L>| match c {
        ControlPoint::Absolute(p) => {
            let (x, y) = position(p);
            ls::PathControl {
                x,
                y,
                reference: ls::PathReference::Origin,
            }
        }
        ControlPoint::Relative(r) => ls::PathControl {
            x: length(&r.coord.x),
            y: length(&r.coord.y),
            reference: match r.reference {
                ControlReference::Origin => ls::PathReference::Origin,
                ControlReference::Start => ls::PathReference::Start,
                ControlReference::End => ls::PathReference::End,
            },
        },
    };
    let axis = |p: &AxisEndPoint<L>| match p {
        AxisEndPoint::ByCoordinate(v) => (length(v), true),
        AxisEndPoint::ToPosition(AxisPosition::LengthPercent(v)) => (length(v), false),
        AxisEndPoint::ToPosition(AxisPosition::Keyword(k)) => (
            ls::LengthPercentage::linear(
                0.0,
                Some(match k {
                    K::Center => 0.5,
                    K::Left | K::Top | K::XStart | K::YStart => 0.0,
                    K::Right | K::Bottom | K::XEnd | K::YEnd => 1.0,
                }),
            ),
            false,
        ),
    };
    commands
        .iter()
        .map(|c| match c {
            S::Move { point } => ls::PathCommand::Move { to: end(point) },
            S::Line { point } => ls::PathCommand::Line { to: end(point) },
            S::HLine { x } => {
                let (x, relative) = axis(x);
                ls::PathCommand::HLine { x, relative }
            }
            S::VLine { y } => {
                let (y, relative) = axis(y);
                ls::PathCommand::VLine { y, relative }
            }
            S::CubicCurve {
                point,
                control1,
                control2,
            } => ls::PathCommand::Cubic {
                to: end(point),
                c1: control(control1),
                c2: control(control2),
            },
            S::QuadCurve { point, control1 } => ls::PathCommand::Quad {
                to: end(point),
                c1: control(control1),
            },
            S::SmoothCubic { point, control2 } => ls::PathCommand::SmoothCubic {
                to: end(point),
                c2: control(control2),
            },
            S::SmoothQuad { point } => ls::PathCommand::SmoothQuad { to: end(point) },
            S::Arc {
                point,
                radii,
                arc_sweep,
                arc_size,
                rotate,
            } => {
                let rx = length(&radii.rx);
                ls::PathCommand::Arc {
                    to: end(point),
                    rx,
                    ry: radii.ry.as_ref().map_or(rx, length),
                    rotate: angle(rotate),
                    large: matches!(arc_size, ArcSize::Large),
                    sweep: matches!(arc_sweep, ArcSweep::Cw),
                }
            }
            S::Close => ls::PathCommand::Close,
        })
        .collect()
}

/// A computed `<length-percentage>` as a length plus a fraction. A linear `calc()` (a sum of lengths
/// and percentages) is exact; so are `min()`, `max()` and `clamp()` of linear terms (and sums and
/// multiples of those) that fit layout's bounded form `max(lower, min(linear, upper))`
/// ([`calc_form`]). Anything else becomes the line through its values at bases 0 and 1000 px.
pub(crate) fn lp(v: &c::LengthPercentage) -> ls::LengthPercentage {
    use style::values::computed::length_percentage::Unpacked;
    match v.unpack() {
        Unpacked::Length(l) => return ls::LengthPercentage::px(l.px()),
        Unpacked::Percentage(p) => return ls::LengthPercentage::linear(0.0, Some(p.0)),
        Unpacked::Calc(calc) => {
            if let Some(f) = calc_form(calc.node()) {
                let percent = (f.lin.percent != 0.0).then_some(f.lin.percent);
                return ls::LengthPercentage {
                    length: f.lin.length,
                    percent,
                    lower: f.lower,
                    upper: f.upper,
                };
            }
        }
    }
    // Unclamped: a non-negative property clamps the value at base 0 of a `calc()` to zero (layout
    // clamps the used value).
    let at0 = v.resolve_unclamped(Length::new(0.0)).px();
    let at1000 = v.resolve_unclamped(Length::new(1000.0)).px();
    ls::LengthPercentage::linear(at0, Some((at1000 - at0) / 1000.0))
}

/// A `calc()` tree as `max(lower, min(lin, upper))` over the percentage basis.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CalcForm {
    lin: ls::Linear,
    lower: Option<ls::Linear>,
    upper: Option<ls::Linear>,
}

impl CalcForm {
    fn linear(lin: ls::Linear) -> CalcForm {
        CalcForm {
            lin,
            lower: None,
            upper: None,
        }
    }

    fn as_linear(&self) -> Option<ls::Linear> {
        (self.lower.is_none() && self.upper.is_none()).then_some(self.lin)
    }

    /// `k × self`. A negative factor swaps the bounds (`-min(a, b)` is `max(-a, -b)`; for `clamp()`
    /// that is exact while its minimum is below its maximum).
    fn scale(self, k: f32) -> CalcForm {
        let s = |l: ls::Linear| ls::Linear {
            length: l.length * k,
            percent: l.percent * k,
        };
        let (lower, upper) = if k < 0.0 {
            (self.upper.map(s), self.lower.map(s))
        } else {
            (self.lower.map(s), self.upper.map(s))
        };
        CalcForm {
            lin: s(self.lin),
            lower,
            upper,
        }
    }

    fn add(self, l: ls::Linear) -> CalcForm {
        let a = |x: ls::Linear| ls::Linear {
            length: x.length + l.length,
            percent: x.percent + l.percent,
        };
        CalcForm {
            lin: a(self.lin),
            lower: self.lower.map(a),
            upper: self.upper.map(a),
        }
    }
}

/// Whether `a ≥ b` for every basis ≥ 0 (both are lines).
fn dominates(a: ls::Linear, b: ls::Linear) -> bool {
    a.length >= b.length && a.percent >= b.percent
}

/// `min(form, l)` (or `max` with `min = false`) when it stays in the bounded form.
fn bound(f: CalcForm, l: ls::Linear, min: bool) -> Option<CalcForm> {
    // `loses(a, b)`: `a` never wins over `b` in this operation.
    let loses = |a: ls::Linear, b: ls::Linear| {
        if min {
            dominates(a, b)
        } else {
            dominates(b, a)
        }
    };
    let (same, other) = if min {
        (f.upper, f.lower)
    } else {
        (f.lower, f.upper)
    };
    let new_same = match same {
        None => Some(l),
        Some(s) if loses(l, s) => Some(s),
        Some(s) if loses(s, l) => Some(l),
        Some(_) => return None,
    };
    // The opposite bound applies after this one (`max(lower, min(lin, upper))`): `min(f, l)` keeps
    // the form when `l` never goes below the lower bound (and `max(f, l)` when it never goes above
    // the upper one).
    if other.is_some_and(|o| !loses(l, o)) {
        return None;
    }
    Some(if min {
        CalcForm {
            upper: new_same,
            ..f
        }
    } else {
        CalcForm {
            lower: new_same,
            ..f
        }
    })
}

/// CSS Values 4 §10: the bounded form of a computed `calc()` tree, or `None` when it has none.
fn calc_form(node: &c::length_percentage::CalcNode) -> Option<CalcForm> {
    use style::values::computed::calc::ComputedLeaf;
    use style::values::generics::calc::{GenericCalcNode as N, MinMaxOp};
    let number = |n: &c::length_percentage::CalcNode| match n {
        N::Leaf(ComputedLeaf::Number(k)) => Some(*k),
        _ => None,
    };
    match node {
        N::Leaf(ComputedLeaf::Length(l)) => Some(CalcForm::linear(ls::Linear {
            length: l.px(),
            percent: 0.0,
        })),
        N::Leaf(ComputedLeaf::Percentage(p)) => Some(CalcForm::linear(ls::Linear {
            length: 0.0,
            percent: p.get(),
        })),
        N::Negate(x) => Some(calc_form(x)?.scale(-1.0)),
        N::Sum(items) => {
            let mut acc: Option<CalcForm> = None;
            for item in items.iter() {
                let f = calc_form(item)?;
                acc = Some(match acc {
                    None => f,
                    Some(a) => match (a.as_linear(), f.as_linear()) {
                        (_, Some(l)) => a.add(l),
                        (Some(l), None) => f.add(l),
                        (None, None) => return None,
                    },
                });
            }
            acc
        }
        N::Product(items) => {
            let mut k = 1.0;
            let mut form = None;
            for item in items.iter() {
                match number(item) {
                    Some(n) => k *= n,
                    None if form.is_none() => form = Some(calc_form(item)?),
                    None => return None,
                }
            }
            Some(form?.scale(k))
        }
        N::MinMax(args, op) => {
            let min = matches!(op, MinMaxOp::Min);
            let mut iter = args.iter();
            let mut acc = calc_form(iter.next()?)?;
            for arg in iter {
                let f = calc_form(arg)?;
                acc = match (acc.as_linear(), f.as_linear()) {
                    (_, Some(l)) => bound(acc, l, min)?,
                    (Some(l), None) => bound(f, l, min)?,
                    (None, None) => return None,
                };
            }
            Some(acc)
        }
        N::Clamp { min, center, max } => {
            let (lo, mid, hi) = (
                calc_form(min)?.as_linear()?,
                calc_form(center)?.as_linear()?,
                calc_form(max)?.as_linear()?,
            );
            Some(CalcForm {
                lin: mid,
                lower: Some(lo),
                upper: Some(hi),
            })
        }
        _ => None,
    }
}

fn size(v: &c::Size) -> ls::SizeValue {
    use style::values::generics::length::GenericSize as S;
    match v {
        S::LengthPercentage(l) => ls::SizeValue::LengthPercentage(lp(&l.0)),
        S::Auto => ls::SizeValue::Auto,
        S::MaxContent => ls::SizeValue::MaxContent,
        S::MinContent => ls::SizeValue::MinContent,
        S::FitContent => ls::SizeValue::FitContent(None),
        S::FitContentFunction(l) => ls::SizeValue::FitContent(Some(lp(&l.0))),
        S::WebkitFillAvailable | S::Stretch => ls::SizeValue::Stretch,
        // Anchor positioning is not implemented: as `auto`.
        S::AnchorSizeFunction(_) | S::AnchorContainingCalcFunction(_) => ls::SizeValue::Auto,
    }
}

fn max_size(v: &c::MaxSize) -> ls::MaxSizeValue {
    use style::values::generics::length::GenericMaxSize as S;
    match v {
        S::LengthPercentage(l) => ls::MaxSizeValue::LengthPercentage(lp(&l.0)),
        S::None => ls::MaxSizeValue::None,
        S::MaxContent => ls::MaxSizeValue::MaxContent,
        S::MinContent => ls::MaxSizeValue::MinContent,
        S::FitContent => ls::MaxSizeValue::FitContent(None),
        S::FitContentFunction(l) => ls::MaxSizeValue::FitContent(Some(lp(&l.0))),
        S::WebkitFillAvailable | S::Stretch => ls::MaxSizeValue::Stretch,
        S::AnchorSizeFunction(_) | S::AnchorContainingCalcFunction(_) => ls::MaxSizeValue::None,
    }
}

fn display(d: style::values::specified::box_::Display) -> ls::Display {
    use style::values::specified::box_::{DisplayInside as I, DisplayOutside as O};
    if d.is_none() {
        return ls::Display::None;
    }
    let inside = d.inside();
    if inside == I::Contents {
        return ls::Display::Contents;
    }
    let internal = |i| ls::Display::Internal(i);
    match (d.outside(), inside) {
        (O::TableCaption, _) => internal(ls::DisplayInternal::TableCaption),
        (O::InternalTable, I::TableRowGroup) => internal(ls::DisplayInternal::TableRowGroup),
        (O::InternalTable, I::TableHeaderGroup) => internal(ls::DisplayInternal::TableHeaderGroup),
        (O::InternalTable, I::TableFooterGroup) => internal(ls::DisplayInternal::TableFooterGroup),
        (O::InternalTable, I::TableRow) => internal(ls::DisplayInternal::TableRow),
        (O::InternalTable, I::TableCell) => internal(ls::DisplayInternal::TableCell),
        (O::InternalTable, I::TableColumnGroup) => internal(ls::DisplayInternal::TableColumnGroup),
        (O::InternalTable, I::TableColumn) => internal(ls::DisplayInternal::TableColumn),
        (outside, inside) => ls::Display::Box {
            outside: match outside {
                O::Inline => ls::DisplayOutside::Inline,
                _ => ls::DisplayOutside::Block,
            },
            inside: match inside {
                I::FlowRoot => ls::DisplayInside::FlowRoot,
                // (`-webkit-box` without a line clamp: the legacy flexible box, laid out as a flex
                // container in the direction `-webkit-box-orient` gives, as Chrome does)
                I::Flex | I::WebkitBox => ls::DisplayInside::Flex,
                I::Grid => ls::DisplayInside::Grid,
                I::Table => ls::DisplayInside::Table,
                _ => ls::DisplayInside::Flow,
            },
            list_item: d.is_list_item(),
        },
    }
}

fn overflow(o: style::values::specified::box_::Overflow) -> ls::Overflow {
    use style::values::specified::box_::Overflow as O;
    match o {
        O::Visible => ls::Overflow::Visible,
        O::Hidden => ls::Overflow::Hidden,
        O::Scroll => ls::Overflow::Scroll,
        O::Auto => ls::Overflow::Auto,
        O::Clip => ls::Overflow::Clip,
    }
}

fn border_style(s: style::values::specified::border::BorderStyle) -> snowlark_paint::BorderStyle {
    use snowlark_paint::BorderStyle as P;
    use style::values::specified::border::BorderStyle as B;
    match s {
        B::Hidden => P::Hidden,
        B::None => P::None,
        B::Inset => P::Inset,
        B::Groove => P::Groove,
        B::Outset => P::Outset,
        B::Ridge => P::Ridge,
        B::Dotted => P::Dotted,
        B::Dashed => P::Dashed,
        B::Solid => P::Solid,
        B::Double => P::Double,
    }
}

fn align(flags: style::values::specified::align::AlignFlags) -> ls::Align {
    use style::values::specified::align::AlignFlags as F;
    match flags.value() {
        F::AUTO => ls::Align::Auto,
        F::NORMAL => ls::Align::Normal,
        F::START => ls::Align::Start,
        F::END => ls::Align::End,
        F::FLEX_START => ls::Align::FlexStart,
        F::FLEX_END => ls::Align::FlexEnd,
        F::CENTER | F::ANCHOR_CENTER => ls::Align::Center,
        F::LEFT => ls::Align::Left,
        F::RIGHT => ls::Align::Right,
        F::BASELINE => ls::Align::Baseline,
        F::LAST_BASELINE => ls::Align::LastBaseline,
        F::STRETCH => ls::Align::Stretch,
        F::SELF_START => ls::Align::SelfStart,
        F::SELF_END => ls::Align::SelfEnd,
        F::SPACE_BETWEEN => ls::Align::SpaceBetween,
        F::SPACE_AROUND => ls::Align::SpaceAround,
        F::SPACE_EVENLY => ls::Align::SpaceEvenly,
        _ => ls::Align::Normal,
    }
}

fn overflow_position(flags: style::values::specified::align::AlignFlags) -> ls::OverflowPosition {
    use style::values::specified::align::AlignFlags as F;
    if flags.contains(F::SAFE) {
        ls::OverflowPosition::Safe
    } else if flags.contains(F::UNSAFE) {
        ls::OverflowPosition::Unsafe
    } else {
        ls::OverflowPosition::Default
    }
}

fn gap(v: &c::length::NonNegativeLengthPercentageOrNormal) -> Option<ls::LengthPercentage> {
    use style::values::generics::length::GenericLengthPercentageOrNormal as G;
    match v {
        G::Normal => None,
        G::LengthPercentage(l) => Some(lp(&l.0)),
    }
}

fn track_breadth(b: &c::TrackBreadth) -> ls::TrackBreadth {
    use style::values::generics::grid::GenericTrackBreadth as B;
    match b {
        B::Breadth(l) => ls::TrackBreadth::LengthPercentage(lp(l)),
        B::Flex(f) => ls::TrackBreadth::Fr(f.0),
        B::Auto => ls::TrackBreadth::Auto,
        B::MinContent => ls::TrackBreadth::MinContent,
        B::MaxContent => ls::TrackBreadth::MaxContent,
    }
}

fn track_size(t: &c::TrackSize) -> ls::TrackSize {
    use style::values::generics::grid::{GenericTrackBreadth as B, GenericTrackSize as T};
    match t {
        T::Breadth(b) => ls::TrackSize::Breadth(track_breadth(b)),
        T::Minmax(a, b) => ls::TrackSize::MinMax(track_breadth(a), track_breadth(b)),
        T::FitContent(B::Breadth(l)) => ls::TrackSize::FitContent(lp(l)),
        T::FitContent(_) => ls::TrackSize::Breadth(ls::TrackBreadth::Auto),
    }
}

fn line_names(names: &[style::OwnedSlice<style::values::CustomIdent>]) -> Vec<Vec<Arc<str>>> {
    names
        .iter()
        .map(|set| set.iter().map(|n| Arc::from(&*n.0)).collect())
        .collect()
}

fn track_list(v: &c::GridTemplateComponent) -> ls::TrackList {
    use style::values::generics::grid::{
        GenericGridTemplateComponent as G, GenericLineNameListValue as N,
        GenericTrackListValue as V, RepeatCount,
    };
    if let G::Subgrid(list) = v {
        // css-grid-2 §9: a subgrid's `<line-name-list>` names its lines from the first; its tracks
        // are its parent's (`subgrid_axes`). A `repeat(auto-fill, ...)` is taken once.
        let mut names = Vec::new();
        for value in list.line_names.iter() {
            match value {
                N::LineNames(set) => names.push(set.iter().map(|n| Arc::from(&*n.0)).collect()),
                N::Repeat(r) => {
                    let count = match r.count {
                        RepeatCount::Number(n) => n.clamp(1, 1000) as usize,
                        _ => 1,
                    };
                    for _ in 0..count {
                        names.extend(line_names(&r.line_names));
                    }
                }
            }
        }
        return ls::TrackList {
            items: Vec::new(),
            line_names: names,
        };
    }
    let G::TrackList(list) = v else {
        // `none` and `masonry` (not implemented): no explicit tracks.
        return ls::TrackList::default();
    };
    let items = list
        .values
        .iter()
        .map(|value| match value {
            V::TrackSize(t) => ls::TrackListItem::Track(track_size(t)),
            V::TrackRepeat(r) => ls::TrackListItem::Repeat {
                count: match r.count {
                    RepeatCount::Number(n) => {
                        ls::RepeatCount::Count(n.clamp(1, u16::MAX as i32) as u16)
                    }
                    RepeatCount::AutoFill => ls::RepeatCount::AutoFill,
                    RepeatCount::AutoFit => ls::RepeatCount::AutoFit,
                },
                tracks: r.track_sizes.iter().map(track_size).collect(),
                line_names: line_names(&r.line_names),
            },
        })
        .collect();
    ls::TrackList {
        items,
        line_names: line_names(&list.line_names),
    }
}

fn grid_line(l: &c::GridLine) -> ls::GridLine {
    let name = &*l.ident.0;
    let n = l.line_num.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    match (name.is_empty(), l.is_span) {
        (true, false) if n == 0 => ls::GridLine::Auto,
        (true, false) => ls::GridLine::Line(n),
        (true, true) => ls::GridLine::Span(n.max(1) as u16),
        (false, false) => ls::GridLine::Named(Arc::from(name), n),
        (false, true) => ls::GridLine::NamedSpan(Arc::from(name), n.max(1) as u16),
    }
}

fn counter_style_name(s: &style::counter_style::CounterStyle) -> ls::ListStyleType {
    use style::counter_style::CounterStyle as C;
    match s {
        C::None => ls::ListStyleType::None,
        C::String(text) => ls::ListStyleType::String(Arc::from(text.to_string().as_str())),
        C::Name(name) => match &*name.0 {
            "disc" => ls::ListStyleType::Disc,
            "circle" => ls::ListStyleType::Circle,
            "square" => ls::ListStyleType::Square,
            "decimal" => ls::ListStyleType::Decimal,
            "decimal-leading-zero" => ls::ListStyleType::DecimalLeadingZero,
            "lower-alpha" | "lower-latin" => ls::ListStyleType::LowerAlpha,
            "upper-alpha" | "upper-latin" => ls::ListStyleType::UpperAlpha,
            "lower-roman" => ls::ListStyleType::LowerRoman,
            "upper-roman" => ls::ListStyleType::UpperRoman,
            "lower-greek" => ls::ListStyleType::LowerGreek,
            "disclosure-open" => ls::ListStyleType::DisclosureOpen,
            "disclosure-closed" => ls::ListStyleType::DisclosureClosed,
            // CSS Counter Styles 3 §3.1: an unknown name is treated as `decimal`.
            _ => ls::ListStyleType::Decimal,
        },
        // `symbols()`: the first symbol as a string marker.
        C::Symbols { .. } => ls::ListStyleType::Decimal,
    }
}

/// `font-style` as the paint crate's three styles.
pub(crate) fn font_style(s: &c::FontStyle) -> FontStyle {
    if *s == c::FontStyle::NORMAL {
        FontStyle::Normal
    } else if *s == c::FontStyle::ITALIC {
        FontStyle::Italic
    } else {
        FontStyle::Oblique
    }
}

/// The OpenType features the `font-variant-*` longhands enable (CSS Fonts 4 §6.4-6.10, the
/// feature named by each value), appended to `out` before `font-feature-settings` (§7.2).
/// `font-variant-alternates`' functions name `@font-feature-values` entries, which are not
/// resolved: only `historical-forms` maps to a feature on its own.
pub(crate) fn font_variant_features(
    caps: style::properties::longhands::font_variant_caps::computed_value::T,
    numeric: c::FontVariantNumeric,
    east_asian: c::FontVariantEastAsian,
    position: style::properties::longhands::font_variant_position::computed_value::T,
    alternates: &c::FontVariantAlternates,
    out: &mut Vec<FontFeature>,
) {
    use c::FontVariantEastAsian as E;
    use c::FontVariantNumeric as N;
    use style::properties::longhands::font_variant_caps::computed_value::T as Caps;
    use style::properties::longhands::font_variant_position::computed_value::T as Position;
    use style::values::specified::font::VariantAlternates as A;
    let on = |out: &mut Vec<FontFeature>, tag: &[u8; 4]| out.push(FontFeature::new(tag, 1));
    match caps {
        Caps::Normal => {}
        Caps::SmallCaps => on(out, b"smcp"),
        Caps::AllSmallCaps => {
            on(out, b"c2sc");
            on(out, b"smcp");
        }
        Caps::PetiteCaps => on(out, b"pcap"),
        Caps::AllPetiteCaps => {
            on(out, b"c2pc");
            on(out, b"pcap");
        }
        Caps::Unicase => on(out, b"unic"),
        Caps::TitlingCaps => on(out, b"titl"),
    }
    for (flag, tag) in [
        (N::LINING_NUMS, b"lnum"),
        (N::OLDSTYLE_NUMS, b"onum"),
        (N::PROPORTIONAL_NUMS, b"pnum"),
        (N::TABULAR_NUMS, b"tnum"),
        (N::DIAGONAL_FRACTIONS, b"frac"),
        (N::STACKED_FRACTIONS, b"afrc"),
        (N::ORDINAL, b"ordn"),
        (N::SLASHED_ZERO, b"zero"),
    ] {
        if numeric.contains(flag) {
            on(out, tag);
        }
    }
    for (flag, tag) in [
        (E::JIS78, b"jp78"),
        (E::JIS83, b"jp83"),
        (E::JIS90, b"jp90"),
        (E::JIS04, b"jp04"),
        (E::SIMPLIFIED, b"smpl"),
        (E::TRADITIONAL, b"trad"),
        (E::FULL_WIDTH, b"fwid"),
        (E::PROPORTIONAL_WIDTH, b"pwid"),
        (E::RUBY, b"ruby"),
    ] {
        if east_asian.contains(flag) {
            on(out, tag);
        }
    }
    match position {
        Position::Normal => {}
        Position::Sub => on(out, b"subs"),
        Position::Super => on(out, b"sups"),
    }
    if alternates.iter().any(|a| matches!(a, A::HistoricalForms)) {
        on(out, b"hist");
    }
}

/// `font-stretch` (a percentage) as DirectWrite's nine steps (CSS Fonts 4 §2.5 keyword table).
pub(crate) fn font_stretch(w: &c::FontWidth) -> FontStretch {
    let p = w.to_percentage().0 * 100.0;
    let steps = [50.0, 62.5, 75.0, 87.5, 100.0, 112.5, 125.0, 150.0, 200.0];
    let mut best = 4;
    for (i, s) in steps.iter().enumerate() {
        if (p - s).abs() < (p - steps[best]).abs() {
            best = i;
        }
    }
    FontStretch(best as u8 + 1)
}

/// The generic family keyword the paint crate takes.
pub(crate) fn generic_name(g: GenericFontFamily) -> &'static str {
    match g {
        GenericFontFamily::Serif | GenericFontFamily::None => "serif",
        GenericFontFamily::SansSerif => "sans-serif",
        GenericFontFamily::Monospace => "monospace",
        GenericFontFamily::Cursive => "cursive",
        GenericFontFamily::Fantasy => "fantasy",
        GenericFontFamily::SystemUi => "system-ui",
    }
}

/// The `font-family` list with `@font-face` families replaced by their loaded faces' internal names,
/// best match first, and the UA's default (serif) after a list that names no generic family.
pub struct FontFamilies<'a> {
    list: std::slice::Iter<'a, SingleFontFamily>,
    env: &'a StyleEnv,
    want: Wanted,
    pending: SmallVec<[&'a str; 4]>,
    saw_generic: bool,
    done: bool,
}

impl<'a> Iterator for FontFamilies<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        loop {
            if let Some(name) = self.pending.pop() {
                return Some(name);
            }
            match self.list.next() {
                Some(SingleFontFamily::Generic(g)) => {
                    self.saw_generic = true;
                    return Some(generic_name(*g));
                }
                Some(SingleFontFamily::FamilyName(f)) => {
                    let name: &'a str = &f.name;
                    let Some(family) = self.env.fonts.family(name) else {
                        return Some(name);
                    };
                    // CSS Fonts 4 §4.1: a family defined by @font-face hides system fonts of the
                    // same name. Faces sorted by match quality; equal ranks keep the later rule first
                    // (unicode-range subsets are searched in reverse order of definition, §4.5).
                    let mut faces: SmallVec<[&'a crate::env::WebFace; 8]> =
                        family.faces.iter().collect();
                    faces.sort_by_key(|f| (face_rank(f, &self.want), std::cmp::Reverse(f.order)));
                    // `pending` pops from the end.
                    self.pending
                        .extend(faces.iter().rev().map(|f| f.internal.as_str()));
                }
                None => {
                    if !self.saw_generic && !self.done {
                        self.done = true;
                        return Some("serif");
                    }
                    return None;
                }
            }
        }
    }
}

fn shadow(s: &c::BoxShadow, color: impl Fn(&c::Color) -> Color) -> ls::Shadow {
    ls::Shadow {
        offset_x: s.base.horizontal.px(),
        offset_y: s.base.vertical.px(),
        blur: s.base.blur.0.px(),
        spread: s.spread.px(),
        color: color(&s.base.color),
        inset: s.inset,
    }
}

impl ls::Style for RenderStyle {
    fn display(&self) -> ls::Display {
        display(self.values.get_box().display)
    }

    fn original_display(&self) -> ls::Display {
        display(self.values.get_box().original_display)
    }

    fn position(&self) -> ls::Position {
        use style::values::computed::PositionProperty as P;
        match self.values.get_box().position {
            P::Static => ls::Position::Static,
            P::Relative => ls::Position::Relative,
            P::Absolute => ls::Position::Absolute,
            P::Fixed => ls::Position::Fixed,
            P::Sticky => ls::Position::Sticky,
        }
    }

    fn float(&self) -> ls::Float {
        use style::values::computed::Float as F;
        match self.values.get_box().float {
            F::None => ls::Float::None,
            F::Left => ls::Float::Left,
            F::Right => ls::Float::Right,
            F::InlineStart => ls::Float::InlineStart,
            F::InlineEnd => ls::Float::InlineEnd,
        }
    }

    fn clear(&self) -> ls::Clear {
        use style::values::computed::Clear as C;
        match self.values.get_box().clear {
            C::None => ls::Clear::None,
            C::Left => ls::Clear::Left,
            C::Right => ls::Clear::Right,
            C::Both => ls::Clear::Both,
            C::InlineStart => ls::Clear::InlineStart,
            C::InlineEnd => ls::Clear::InlineEnd,
        }
    }

    fn phys_inset(&self, side: Side) -> ls::LengthPercentageAuto {
        use style::values::generics::position::GenericInset as I;
        let p = self.values.get_position();
        let v = match side {
            Side::Top => &p.top,
            Side::Right => &p.right,
            Side::Bottom => &p.bottom,
            Side::Left => &p.left,
        };
        match v {
            I::LengthPercentage(l) => ls::LengthPercentageAuto::LengthPercentage(lp(l)),
            _ => ls::LengthPercentageAuto::Auto,
        }
    }

    fn z_index(&self) -> Option<i32> {
        use style::values::generics::position::GenericZIndex as Z;
        match self.values.get_position().z_index {
            Z::Integer(i) => Some(i),
            Z::Auto => None,
        }
    }

    fn box_sizing(&self) -> ls::BoxSizing {
        use style::properties::longhands::box_sizing::computed_value::T;
        match self.values.get_position().box_sizing {
            T::ContentBox => ls::BoxSizing::ContentBox,
            T::BorderBox => ls::BoxSizing::BorderBox,
        }
    }

    fn phys_width(&self) -> ls::SizeValue {
        size(&self.values.get_position().width)
    }

    fn phys_height(&self) -> ls::SizeValue {
        size(&self.values.get_position().height)
    }

    fn phys_min_width(&self) -> ls::SizeValue {
        size(&self.values.get_position().min_width)
    }

    fn phys_min_height(&self) -> ls::SizeValue {
        size(&self.values.get_position().min_height)
    }

    fn phys_max_width(&self) -> ls::MaxSizeValue {
        max_size(&self.values.get_position().max_width)
    }

    fn phys_max_height(&self) -> ls::MaxSizeValue {
        max_size(&self.values.get_position().max_height)
    }

    fn phys_aspect_ratio(&self) -> ls::AspectRatio {
        use style::values::generics::position::PreferredRatio;
        let a = &self.values.get_position().aspect_ratio;
        ls::AspectRatio {
            auto: a.auto,
            ratio: match &a.ratio {
                PreferredRatio::None => None,
                PreferredRatio::Ratio(r) => {
                    let (w, h) = (r.0 .0, r.1 .0);
                    (w > 0.0 && h > 0.0).then(|| w / h)
                }
            },
        }
    }

    fn layout_containment(&self) -> bool {
        use style::values::computed::Contain;
        self.values
            .get_box()
            .clone_contain()
            .contains(Contain::LAYOUT)
            || self.content_visibility_contains()
    }

    fn paint_containment(&self) -> bool {
        use style::values::computed::Contain;
        self.values
            .get_box()
            .clone_contain()
            .contains(Contain::PAINT)
            || self.content_visibility_contains()
    }

    fn intrinsic_size_auto(&self) -> (bool, bool) {
        use style::values::generics::box_::GenericContainIntrinsicSize as I;
        let p = self.values.get_position();
        let auto = |v: &c::ContainIntrinsicSize| matches!(v, I::AutoLength(_) | I::AutoNone);
        (
            auto(&p.contain_intrinsic_width),
            auto(&p.contain_intrinsic_height),
        )
    }

    fn contents_skipped(&self) -> bool {
        use style::values::computed::ContentVisibility;
        self.values.get_box().clone_content_visibility() == ContentVisibility::Hidden
    }

    fn phys_size_containment(&self) -> ls::SizeContainment {
        use style::values::computed::{Contain, ContainerType};
        let b = self.values.get_box();
        let contain = b.clone_contain();
        let container = b.clone_container_type();
        // CSS Contain 3 §2: `container-type: size` applies size containment, `inline-size`
        // inline-size containment.
        // (CSS Contain 2 section 4: `content-visibility: hidden` also applies size containment)
        let hidden = self.contents_skipped();
        let inline = hidden
            || contain.contains(Contain::INLINE_SIZE)
            || container.intersects(ContainerType::SIZE | ContainerType::INLINE_SIZE);
        let block = hidden
            || contain.contains(Contain::BLOCK_SIZE)
            || container.contains(ContainerType::SIZE);
        if !inline && !block {
            return ls::SizeContainment::default();
        }
        // `contain-intrinsic-width`/`-height` (physical; the logical ones map to them).
        use style::values::generics::box_::GenericContainIntrinsicSize as I;
        // (`auto`: a box skipping its contents takes its last remembered size in that axis)
        let length = |v: &c::ContainIntrinsicSize, remembered: Option<f32>| match v {
            I::Length(l) => Some(l.px()),
            I::AutoLength(l) => Some(if hidden {
                remembered.unwrap_or(l.px())
            } else {
                l.px()
            }),
            I::AutoNone => {
                if hidden {
                    remembered
                } else {
                    None
                }
            }
            I::None => None,
        };
        let p = self.values.get_position();
        // (the remembered size is logical: inline and block, CSS Sizing 4 5.2.1)
        let (ri, rb) = self.remembered.unwrap_or((None, None));
        let (rw, rh) = if self.values.writing_mode.is_horizontal() {
            (ri, rb)
        } else {
            (rb, ri)
        };
        let intrinsic_width = length(&p.contain_intrinsic_width, rw);
        let intrinsic_height = length(&p.contain_intrinsic_height, rh);
        let (width, height) = if self.values.writing_mode.is_horizontal() {
            (inline, block)
        } else {
            (block, inline)
        };
        ls::SizeContainment {
            width,
            height,
            intrinsic_width,
            intrinsic_height,
        }
    }

    fn phys_margin(&self, side: Side) -> ls::LengthPercentageAuto {
        use style::values::generics::length::GenericMargin as M;
        let m = self.values.get_margin();
        let v = match side {
            Side::Top => &m.margin_top,
            Side::Right => &m.margin_right,
            Side::Bottom => &m.margin_bottom,
            Side::Left => &m.margin_left,
        };
        match v {
            M::LengthPercentage(l) => ls::LengthPercentageAuto::LengthPercentage(lp(l)),
            M::Auto => ls::LengthPercentageAuto::Auto,
            M::AnchorSizeFunction(_) | M::AnchorContainingCalcFunction(_) => {
                ls::LengthPercentageAuto::ZERO
            }
        }
    }

    fn phys_padding(&self, side: Side) -> ls::LengthPercentage {
        let p = self.values.get_padding();
        let v = match side {
            Side::Top => &p.padding_top,
            Side::Right => &p.padding_right,
            Side::Bottom => &p.padding_bottom,
            Side::Left => &p.padding_left,
        };
        lp(&v.0)
    }

    fn phys_border_width(&self, side: Side) -> f32 {
        self.side_width(side)
    }

    fn phys_border_style(&self, side: Side) -> snowlark_paint::BorderStyle {
        let b = self.values.get_border();
        border_style(match side {
            Side::Top => b.border_top_style,
            Side::Right => b.border_right_style,
            Side::Bottom => b.border_bottom_style,
            Side::Left => b.border_left_style,
        })
    }

    fn phys_border_color(&self, side: Side) -> Color {
        let b = self.values.get_border();
        self.color_of(match side {
            Side::Top => &b.border_top_color,
            Side::Right => &b.border_right_color,
            Side::Bottom => &b.border_bottom_color,
            Side::Left => &b.border_left_color,
        })
    }

    fn phys_border_radius(&self, corner: Corner) -> (ls::LengthPercentage, ls::LengthPercentage) {
        let b = self.values.get_border();
        let r = match corner {
            Corner::TopLeft => &b.border_top_left_radius,
            Corner::TopRight => &b.border_top_right_radius,
            Corner::BottomRight => &b.border_bottom_right_radius,
            Corner::BottomLeft => &b.border_bottom_left_radius,
        };
        (lp(&r.0.width.0), lp(&r.0.height.0))
    }

    fn phys_overflow_x(&self) -> ls::Overflow {
        overflow(self.values.get_box().overflow_x)
    }

    fn phys_overflow_y(&self) -> ls::Overflow {
        overflow(self.values.get_box().overflow_y)
    }

    fn scroll_padding(&self, side: Side) -> ls::LengthPercentage {
        use style::values::generics::length::GenericLengthPercentageOrAuto as L;
        let p = self.values.get_padding();
        let v = match side {
            Side::Top => &p.scroll_padding_top,
            Side::Right => &p.scroll_padding_right,
            Side::Bottom => &p.scroll_padding_bottom,
            Side::Left => &p.scroll_padding_left,
        };
        match v {
            L::LengthPercentage(l) => lp(&l.0),
            L::Auto => ls::LengthPercentage::ZERO,
        }
    }

    fn scroll_margin(&self, side: Side) -> f32 {
        let m = self.values.get_margin();
        match side {
            Side::Top => m.scroll_margin_top.px(),
            Side::Right => m.scroll_margin_right.px(),
            Side::Bottom => m.scroll_margin_bottom.px(),
            Side::Left => m.scroll_margin_left.px(),
        }
    }

    fn scroll_behavior_smooth(&self) -> bool {
        use style::properties::longhands::scroll_behavior::computed_value::T;
        self.values.get_box().scroll_behavior == T::Smooth
    }

    fn column_width(&self) -> Option<f32> {
        use style::values::generics::length::GenericLengthPercentageOrAuto as L;
        match &self.values.get_column().column_width {
            L::LengthPercentage(l) => Some(l.0.px()),
            L::Auto => None,
        }
    }

    fn column_count(&self) -> Option<u32> {
        use style::values::generics::column::GenericColumnCount as C;
        match &self.values.get_column().column_count {
            C::Integer(n) => Some(n.0.max(1) as u32),
            C::Auto => None,
        }
    }

    fn column_fill_auto(&self) -> bool {
        use style::properties::longhands::column_fill::computed_value::T as Fill;
        self.values.get_column().column_fill == Fill::Auto
    }

    fn column_span_all(&self) -> bool {
        use style::properties::longhands::column_span::computed_value::T as Span;
        self.values.get_column().column_span == Span::All
    }

    fn column_rule_width(&self) -> f32 {
        let c = self.values.get_column();
        if c.column_rule_style.none_or_hidden() {
            0.0
        } else {
            c.column_rule_width.0.to_f32_px()
        }
    }

    fn column_rule_style(&self) -> snowlark_paint::BorderStyle {
        border_style(self.values.get_column().column_rule_style)
    }

    fn column_rule_color(&self) -> Color {
        self.color_of(&self.values.get_column().column_rule_color)
    }

    fn break_before(&self) -> ls::BreakBetween {
        break_between(self.values.get_box().break_before)
    }

    fn break_after(&self) -> ls::BreakBetween {
        break_between(self.values.get_box().break_after)
    }

    fn break_inside(&self) -> ls::BreakInside {
        use style::values::computed::BreakWithin as W;
        match self.values.get_box().break_inside {
            W::Auto => ls::BreakInside::Auto,
            W::Avoid => ls::BreakInside::Avoid,
            W::AvoidPage => ls::BreakInside::AvoidPage,
            W::AvoidColumn => ls::BreakInside::AvoidColumn,
        }
    }

    fn orphans(&self) -> u32 {
        self.values.get_inherited_text().orphans.0.max(1) as u32
    }

    fn widows(&self) -> u32 {
        self.values.get_inherited_text().widows.0.max(1) as u32
    }

    fn line_clamp(&self) -> Option<ls::LineClamp> {
        use style::values::generics::box_::{BlockEllipsis, MaxLinesKeyword};
        use style::values::specified::box_::DisplayInside;
        let b = self.values.get_box();
        let c = &b.line_clamp;
        if c.max_lines.is_none() {
            return None;
        }
        // (a multi-column container is not a line-clamp container, CSS Overflow 4 §4.1)
        let col = self.values.get_column();
        if !col.column_count.is_auto() || !col.column_width.is_auto() {
            return None;
        }
        // `-webkit-line-clamp` clamps a vertical `-webkit-box` only (CSS Overflow 4 §4.6), which
        // computes to `flow-root` then.
        if c.webkit_legacy
            && (b.original_display.inside() != DisplayInside::WebkitBox
                || b.display.inside() == DisplayInside::WebkitBox)
        {
            return None;
        }
        Some(ls::LineClamp {
            lines: c.max_lines.lines_value().map(|n| n.0.max(1) as u32),
            auto: c.max_lines.kw == MaxLinesKeyword::Auto,
            ellipsis: match &c.block_ellipsis {
                BlockEllipsis::Ellipsis => Some("\u{2026}".to_string()),
                BlockEllipsis::NoEllipsis => None,
                BlockEllipsis::String(s) => Some(s.to_string()),
            },
        })
    }

    fn overflow_clip_margin(&self) -> ls::OverflowClipMargin {
        use style::values::generics::box_::OverflowClipMarginBox as B;
        let m = &self.values.get_margin().overflow_clip_margin;
        ls::OverflowClipMargin {
            reference: match m.visual_box {
                B::ContentBox => ls::BoxArea::ContentBox,
                B::PaddingBox => ls::BoxArea::PaddingBox,
                B::BorderBox => ls::BoxArea::BorderBox,
            },
            margin: m.offset.px(),
        }
    }

    fn flex_direction(&self) -> ls::FlexDirection {
        use style::properties::longhands::_webkit_box_orient::computed_value::T as Orient;
        use style::properties::longhands::flex_direction::computed_value::T;
        let b = self.values.get_box();
        if b.display.inside() == style::values::specified::box_::DisplayInside::WebkitBox {
            return match b._webkit_box_orient {
                Orient::Vertical => ls::FlexDirection::Column,
                Orient::Horizontal => ls::FlexDirection::Row,
            };
        }
        match self.values.get_position().flex_direction {
            T::Row => ls::FlexDirection::Row,
            T::RowReverse => ls::FlexDirection::RowReverse,
            T::Column => ls::FlexDirection::Column,
            T::ColumnReverse => ls::FlexDirection::ColumnReverse,
        }
    }

    fn flex_wrap(&self) -> ls::FlexWrap {
        use style::values::computed::FlexWrap as W;
        let w = self.values.get_position().flex_wrap;
        if w.contains(W::WRAP_REVERSE) {
            ls::FlexWrap::WrapReverse
        } else if w.contains(W::WRAP) {
            ls::FlexWrap::Wrap
        } else {
            ls::FlexWrap::Nowrap
        }
    }

    fn flex_grow(&self) -> f32 {
        self.values.get_position().flex_grow.0
    }

    fn flex_shrink(&self) -> f32 {
        self.values.get_position().flex_shrink.0
    }

    fn flex_basis(&self) -> ls::FlexBasis {
        use style::values::generics::flex::GenericFlexBasis as B;
        use style::values::generics::length::GenericSize as S;
        match &self.values.get_position().flex_basis {
            B::Content => ls::FlexBasis::Content,
            B::Size(S::LengthPercentage(l)) => ls::FlexBasis::LengthPercentage(lp(&l.0)),
            // `max-content` and friends as a basis behave as `content`.
            B::Size(S::MaxContent | S::MinContent | S::FitContent) => ls::FlexBasis::Content,
            B::Size(_) => ls::FlexBasis::Auto,
        }
    }

    fn order(&self) -> i32 {
        self.values.get_position().order
    }

    fn justify_content(&self) -> ls::Align {
        align(self.values.get_position().justify_content.primary())
    }

    fn align_content(&self) -> ls::Align {
        align(self.values.get_position().align_content.primary())
    }

    fn align_items(&self) -> ls::Align {
        align(self.values.get_position().align_items.0)
    }

    fn align_self(&self) -> ls::Align {
        align(self.values.get_position().align_self.0)
    }

    fn justify_items(&self) -> ls::Align {
        let j = &self.values.get_position().justify_items;
        // `legacy` alone computes to `normal`; `legacy center` and the like keep their keyword.
        align(j.computed.0 .0)
    }

    fn justify_self(&self) -> ls::Align {
        align(self.values.get_position().justify_self.0)
    }

    fn align_safety(&self) -> ls::AlignSafety {
        let p = self.values.get_position();
        ls::AlignSafety {
            justify_content: overflow_position(p.justify_content.primary()),
            align_content: overflow_position(p.align_content.primary()),
            align_items: overflow_position(p.align_items.0),
            align_self: overflow_position(p.align_self.0),
            justify_items: overflow_position(p.justify_items.computed.0 .0),
            justify_self: overflow_position(p.justify_self.0),
        }
    }

    fn row_gap(&self) -> Option<ls::LengthPercentage> {
        gap(&self.values.get_position().row_gap)
    }

    fn column_gap(&self) -> Option<ls::LengthPercentage> {
        gap(&self.values.get_position().column_gap)
    }

    fn grid_template_columns(&self) -> Cow<'_, ls::TrackList> {
        Cow::Owned(track_list(
            &self.values.get_position().grid_template_columns,
        ))
    }

    fn grid_template_rows(&self) -> Cow<'_, ls::TrackList> {
        Cow::Owned(track_list(&self.values.get_position().grid_template_rows))
    }

    fn grid_template_subgrid(&self) -> (bool, bool) {
        use style::values::generics::grid::GenericGridTemplateComponent as G;
        let p = self.values.get_position();
        (
            matches!(p.grid_template_columns, G::Subgrid(_)),
            matches!(p.grid_template_rows, G::Subgrid(_)),
        )
    }

    fn grid_template_areas(&self) -> Option<Cow<'_, ls::GridTemplateAreas>> {
        use style::values::specified::position::GridTemplateAreas as A;
        let A::Areas(areas) = &self.values.get_position().grid_template_areas else {
            return None;
        };
        let a = &areas.0;
        Some(Cow::Owned(ls::GridTemplateAreas {
            areas: a
                .areas
                .iter()
                .map(|n| ls::GridArea {
                    name: Arc::from(&*n.name),
                    row_start: n.rows.start as u16,
                    row_end: n.rows.end as u16,
                    column_start: n.columns.start as u16,
                    column_end: n.columns.end as u16,
                })
                .collect(),
            rows: a.strings.len() as u16,
            columns: a.width as u16,
        }))
    }

    fn grid_auto_columns(&self) -> Cow<'_, [ls::TrackSize]> {
        let t = &self.values.get_position().grid_auto_columns;
        Cow::Owned(t.0.iter().map(track_size).collect())
    }

    fn grid_auto_rows(&self) -> Cow<'_, [ls::TrackSize]> {
        let t = &self.values.get_position().grid_auto_rows;
        Cow::Owned(t.0.iter().map(track_size).collect())
    }

    fn grid_auto_flow(&self) -> ls::GridAutoFlow {
        use style::values::computed::GridAutoFlow as F;
        let f = self.values.get_position().grid_auto_flow;
        ls::GridAutoFlow {
            column: f.contains(F::COLUMN),
            dense: f.contains(F::DENSE),
        }
    }

    fn grid_row_start(&self) -> ls::GridLine {
        grid_line(&self.values.get_position().grid_row_start)
    }

    fn grid_row_end(&self) -> ls::GridLine {
        grid_line(&self.values.get_position().grid_row_end)
    }

    fn grid_column_start(&self) -> ls::GridLine {
        grid_line(&self.values.get_position().grid_column_start)
    }

    fn grid_column_end(&self) -> ls::GridLine {
        grid_line(&self.values.get_position().grid_column_end)
    }

    fn table_layout(&self) -> ls::TableLayoutMode {
        use style::properties::longhands::table_layout::computed_value::T;
        match self.values.get_table().table_layout {
            T::Auto => ls::TableLayoutMode::Auto,
            T::Fixed => ls::TableLayoutMode::Fixed,
        }
    }

    fn border_collapse(&self) -> ls::BorderCollapse {
        use style::properties::longhands::border_collapse::computed_value::T;
        match self.values.get_inherited_table().border_collapse {
            T::Separate => ls::BorderCollapse::Separate,
            T::Collapse => ls::BorderCollapse::Collapse,
        }
    }

    fn phys_border_spacing(&self) -> (f32, f32) {
        let s = &self.values.get_inherited_table().border_spacing;
        (s.0.width.0.px(), s.0.height.0.px())
    }

    fn caption_side(&self) -> ls::CaptionSide {
        use style::values::computed::table::CaptionSide as C;
        match self.values.get_inherited_table().caption_side {
            C::Top => ls::CaptionSide::Top,
            C::Bottom => ls::CaptionSide::Bottom,
        }
    }

    fn empty_cells(&self) -> ls::EmptyCells {
        use style::properties::longhands::empty_cells::computed_value::T;
        match self.values.get_inherited_table().empty_cells {
            T::Show => ls::EmptyCells::Show,
            T::Hide => ls::EmptyCells::Hide,
        }
    }

    fn font_family(&self) -> impl Iterator<Item = &str> + '_ {
        let font = self.values.get_font();
        FontFamilies {
            list: font.font_family.families.list.iter(),
            env: &self.env,
            want: Wanted::of(font),
            pending: SmallVec::new(),
            saw_generic: false,
            done: false,
        }
    }

    fn font_size(&self) -> f32 {
        self.values.get_font().font_size.computed_size().px()
    }

    fn font_weight(&self) -> u16 {
        self.values
            .get_font()
            .font_weight
            .value()
            .round()
            .clamp(1.0, 1000.0) as u16
    }

    fn font_style(&self) -> FontStyle {
        font_style(&self.values.get_font().font_style)
    }

    fn font_stretch(&self) -> FontStretch {
        font_stretch(&self.values.get_font().font_width)
    }

    fn font_key(&self) -> Option<(usize, usize)> {
        // The font getters read only the computed font struct (shared by every style that inherits
        // or shares it) and the web font table: their addresses, alive while this handle is.
        Some((
            std::ptr::from_ref(self.values.get_font()) as usize,
            Arc::as_ptr(&self.env) as usize,
        ))
    }

    fn font_features(&self) -> Cow<'_, [FontFeature]> {
        use style::properties::longhands::font_kerning::computed_value::T as Kerning;
        use style::properties::longhands::font_variant_caps::computed_value::T as Caps;
        use style::properties::longhands::font_variant_position::computed_value::T as Position;
        use style::values::computed::FontVariantEastAsian as E;
        use style::values::computed::FontVariantLigatures as L;
        use style::values::computed::FontVariantNumeric as N;
        let font = self.values.get_font();
        let settings = &font.font_feature_settings.0;
        let lig = font.font_variant_ligatures;
        let kerning_off = font.font_kerning == Kerning::None;
        let caps = font.font_variant_caps;
        let numeric = font.font_variant_numeric;
        let east_asian = font.font_variant_east_asian;
        let position = font.font_variant_position;
        let alternates = &font.font_variant_alternates;
        if settings.is_empty()
            && lig == L::NORMAL
            && !kerning_off
            && caps == Caps::Normal
            && numeric == N::NORMAL
            && east_asian == E::NORMAL
            && position == Position::Normal
            && alternates.is_empty()
        {
            return Cow::Borrowed(&[]);
        }
        let mut out = Vec::new();
        font_variant_features(caps, numeric, east_asian, position, alternates, &mut out);
        if kerning_off {
            out.push(FontFeature::new(b"kern", 0));
        }
        if lig.contains(L::NONE) {
            for tag in [b"liga", b"clig", b"dlig", b"hlig", b"calt"] {
                out.push(FontFeature::new(tag, 0));
            }
        } else {
            if lig.contains(L::NO_COMMON_LIGATURES) {
                out.push(FontFeature::new(b"liga", 0));
                out.push(FontFeature::new(b"clig", 0));
            }
            if lig.contains(L::DISCRETIONARY_LIGATURES) {
                out.push(FontFeature::new(b"dlig", 1));
            }
            if lig.contains(L::HISTORICAL_LIGATURES) {
                out.push(FontFeature::new(b"hlig", 1));
            }
            if lig.contains(L::NO_CONTEXTUAL) {
                out.push(FontFeature::new(b"calt", 0));
            }
        }
        // font-feature-settings last: it overrides the properties above (CSS Fonts 4 §7.2).
        for f in settings.iter() {
            out.push(FontFeature {
                tag: f.tag.0.to_be_bytes(),
                value: f.value.max(0) as u32,
            });
        }
        Cow::Owned(out)
    }

    fn locale(&self) -> &str {
        let lang = &*self.values.get_font()._x_lang.0;
        if lang.is_empty() {
            "en-us"
        } else {
            lang
        }
    }

    fn color(&self) -> Color {
        to_color(&self.values.get_inherited_text().color)
    }

    fn line_height(&self) -> ls::LineHeight {
        use style::values::generics::font::GenericLineHeight as L;
        match &self.values.get_font().line_height {
            L::Normal => ls::LineHeight::Normal,
            L::Number(n) => ls::LineHeight::Number(n.0),
            L::Length(l) => ls::LineHeight::Length(l.0.px()),
        }
    }

    fn vertical_align(&self) -> ls::VerticalAlign {
        use style::values::computed::AlignmentBaseline as A;
        use style::values::generics::box_::{BaselineShiftKeyword as K, GenericBaselineShift as B};
        let b = self.values.get_box();
        // CSS Inline 3: `vertical-align` is the shorthand of `alignment-baseline` and
        // `baseline-shift` (and `baseline-source`).
        match &b.baseline_shift {
            B::Keyword(K::Sub) => return ls::VerticalAlign::Sub,
            B::Keyword(K::Super) => return ls::VerticalAlign::Super,
            B::Keyword(K::Top) => return ls::VerticalAlign::Top,
            B::Keyword(K::Bottom) => return ls::VerticalAlign::Bottom,
            B::Keyword(K::Center) => return ls::VerticalAlign::Middle,
            B::Length(l) => {
                let v = lp(l);
                if v.length != 0.0 || v.percent.is_some_and(|p| p != 0.0) || !v.is_linear() {
                    return ls::VerticalAlign::LengthPercentage(v);
                }
            }
        }
        match b.alignment_baseline {
            A::TextTop => ls::VerticalAlign::TextTop,
            A::TextBottom => ls::VerticalAlign::TextBottom,
            A::Middle => ls::VerticalAlign::Middle,
            _ => ls::VerticalAlign::Baseline,
        }
    }

    fn text_box_trim(&self) -> ls::TextBoxTrim {
        use style::values::computed::TextBoxTrim as T;
        let t = self.values.get_text().text_box_trim;
        ls::TextBoxTrim {
            start: t.contains(T::TRIM_START),
            end: t.contains(T::TRIM_END),
        }
    }

    fn text_box_edge(&self) -> ls::TextBoxEdge {
        use style::values::specified::text::{TextBoxEdge as E, TextEdgeKeyword as K};
        let metric = |k: K| match k {
            K::Text => ls::TextEdgeMetric::Text,
            K::Ideographic => ls::TextEdgeMetric::Ideographic,
            K::IdeographicInk => ls::TextEdgeMetric::IdeographicInk,
            K::Cap => ls::TextEdgeMetric::Cap,
            K::Ex => ls::TextEdgeMetric::Ex,
            K::Alphabetic => ls::TextEdgeMetric::Alphabetic,
        };
        // `auto` takes `line-fit-edge`, whose initial `leading` means `text` (CSS Inline 3 §6.2).
        match self.values.get_inherited_text().text_box_edge {
            E::Auto => ls::TextBoxEdge::default(),
            E::TextEdge(e) => ls::TextBoxEdge {
                over: metric(e.over),
                under: metric(e.under),
            },
        }
    }

    fn text_align(&self) -> ls::TextAlign {
        use style::values::computed::TextAlign as T;
        match self.values.get_inherited_text().text_align {
            T::Start => ls::TextAlign::Start,
            T::End => ls::TextAlign::End,
            T::Left | T::MozLeft => ls::TextAlign::Left,
            T::Right | T::MozRight => ls::TextAlign::Right,
            T::Center | T::MozCenter => ls::TextAlign::Center,
            T::Justify => ls::TextAlign::Justify,
        }
    }

    fn legacy_align(&self) -> Option<ls::LegacyAlign> {
        use style::values::computed::TextAlign as T;
        match self.values.get_inherited_text().text_align {
            T::MozCenter => Some(ls::LegacyAlign::Center),
            T::MozLeft => Some(ls::LegacyAlign::Left),
            T::MozRight => Some(ls::LegacyAlign::Right),
            _ => None,
        }
    }

    fn text_align_last(&self) -> ls::TextAlignLast {
        use style::values::computed::TextAlignLast as T;
        match self.values.get_inherited_text().text_align_last {
            T::Auto => ls::TextAlignLast::Auto,
            T::Start => ls::TextAlignLast::Start,
            T::End => ls::TextAlignLast::End,
            T::Left => ls::TextAlignLast::Left,
            T::Right => ls::TextAlignLast::Right,
            T::Center => ls::TextAlignLast::Center,
            T::Justify => ls::TextAlignLast::Justify,
            // (a specified value only: it computes to the parent's)
            T::MatchParent => ls::TextAlignLast::Auto,
        }
    }

    fn text_indent(&self) -> ls::LengthPercentage {
        lp(&self.values.get_inherited_text().text_indent.length)
    }

    fn text_indent_hanging(&self) -> bool {
        self.values.get_inherited_text().text_indent.hanging
    }

    fn text_indent_each_line(&self) -> bool {
        self.values.get_inherited_text().text_indent.each_line
    }

    fn white_space_collapse(&self) -> ls::WhiteSpaceCollapse {
        use style::properties::longhands::white_space_collapse::computed_value::T;
        match self.values.get_inherited_text().white_space_collapse {
            T::Collapse => ls::WhiteSpaceCollapse::Collapse,
            T::Preserve => ls::WhiteSpaceCollapse::Preserve,
            T::PreserveBreaks => ls::WhiteSpaceCollapse::PreserveBreaks,
            T::BreakSpaces => ls::WhiteSpaceCollapse::BreakSpaces,
        }
    }

    fn text_wrap_mode(&self) -> ls::TextWrapMode {
        use style::properties::longhands::text_wrap_mode::computed_value::T;
        match self.values.get_inherited_text().text_wrap_mode {
            T::Wrap => ls::TextWrapMode::Wrap,
            T::Nowrap => ls::TextWrapMode::Nowrap,
        }
    }

    fn text_wrap_style(&self) -> ls::TextWrapStyle {
        use style::properties::longhands::text_wrap_style::computed_value::T;
        match self.values.get_inherited_text().text_wrap_style {
            T::Auto => ls::TextWrapStyle::Auto,
            T::Stable => ls::TextWrapStyle::Stable,
            T::Balance => ls::TextWrapStyle::Balance,
            T::Pretty => ls::TextWrapStyle::Pretty,
        }
    }

    fn word_break(&self) -> ls::WordBreak {
        use style::values::computed::WordBreak as W;
        match self.values.get_inherited_text().word_break {
            W::Normal => ls::WordBreak::Normal,
            W::BreakAll => ls::WordBreak::BreakAll,
            W::KeepAll => ls::WordBreak::KeepAll,
            // (CSS Text 3 §5.2: `normal` with `overflow-wrap: anywhere`, which line breaking
            // applies)
            W::BreakWord => ls::WordBreak::BreakWord,
        }
    }

    fn line_break(&self) -> ls::LineBreak {
        use style::values::computed::LineBreak as L;
        match self.values.get_inherited_text().line_break {
            L::Auto => ls::LineBreak::Auto,
            L::Loose => ls::LineBreak::Loose,
            L::Normal => ls::LineBreak::Normal,
            L::Strict => ls::LineBreak::Strict,
            L::Anywhere => ls::LineBreak::Anywhere,
        }
    }

    fn hyphens(&self) -> ls::Hyphens {
        use style::properties::longhands::hyphens::computed_value::T;
        match self.values.get_inherited_text().hyphens {
            T::None => ls::Hyphens::None,
            T::Manual => ls::Hyphens::Manual,
            T::Auto => ls::Hyphens::Auto,
        }
    }

    fn hyphenate_character(&self) -> Option<String> {
        use style::values::computed::HyphenateCharacter as H;
        match &self.values.get_inherited_text().hyphenate_character {
            H::Auto => None,
            H::String(s) => Some(s.to_string()),
        }
    }

    fn overflow_wrap(&self) -> ls::OverflowWrap {
        use style::values::computed::OverflowWrap as W;
        match self.values.get_inherited_text().overflow_wrap {
            W::Normal => ls::OverflowWrap::Normal,
            W::BreakWord => ls::OverflowWrap::BreakWord,
            W::Anywhere => ls::OverflowWrap::Anywhere,
        }
    }

    fn text_overflow(&self) -> ls::TextOverflow {
        use style::values::specified::text::TextOverflowSide as S;
        // The end side: `second` holds it for one value (logical) and two (physical: the right
        // side, the end in left-to-right text).
        match &self.values.get_text().text_overflow.second {
            S::Ellipsis | S::String(_) => ls::TextOverflow::Ellipsis,
            _ => ls::TextOverflow::Clip,
        }
    }

    fn text_overflow_string(&self) -> Option<String> {
        use style::values::specified::text::TextOverflowSide as S;
        match &self.values.get_text().text_overflow.second {
            S::Ellipsis => Some("\u{2026}".to_string()),
            S::String(s) => Some(s.to_string()),
            S::Clip => None,
        }
    }

    fn text_transform(&self) -> ls::TextTransform {
        use style::values::computed::TextTransform as T;
        let t = self.values.get_inherited_text().text_transform;
        if t.contains(T::UPPERCASE) {
            ls::TextTransform::Uppercase
        } else if t.contains(T::LOWERCASE) {
            ls::TextTransform::Lowercase
        } else if t.contains(T::CAPITALIZE) {
            ls::TextTransform::Capitalize
        } else {
            ls::TextTransform::None
        }
    }

    fn letter_spacing(&self) -> f32 {
        let v = &self.values.get_inherited_text().letter_spacing.0;
        v.resolve(self.values.get_font().font_size.computed_size())
            .px()
    }

    fn word_spacing(&self) -> f32 {
        let v = &self.values.get_inherited_text().word_spacing;
        // Percentages are of the space's advance; the font size stands in for it.
        v.resolve(self.values.get_font().font_size.computed_size())
            .px()
    }

    fn tab_size(&self) -> ls::TabSize {
        use style::values::generics::length::GenericLengthOrNumber as T;
        match &self.values.get_inherited_text().tab_size {
            T::Number(n) => ls::TabSize::Spaces(n.0),
            T::Length(l) => ls::TabSize::Length(l.0.px()),
        }
    }

    fn text_decoration_line(&self) -> ls::TextDecorationLine {
        use style::values::computed::TextDecorationLine as T;
        let t = self.values.get_text().text_decoration_line;
        let mut out = ls::TextDecorationLine::NONE;
        if t.contains(T::UNDERLINE) {
            out = out | ls::TextDecorationLine::UNDERLINE;
        }
        if t.contains(T::OVERLINE) {
            out = out | ls::TextDecorationLine::OVERLINE;
        }
        if t.contains(T::LINE_THROUGH) {
            out = out | ls::TextDecorationLine::LINE_THROUGH;
        }
        out
    }

    fn text_decoration_style(&self) -> ls::TextDecorationStyle {
        use style::properties::longhands::text_decoration_style::computed_value::T;
        match self.values.get_text().text_decoration_style {
            T::Double => ls::TextDecorationStyle::Double,
            T::Dotted => ls::TextDecorationStyle::Dotted,
            T::Dashed => ls::TextDecorationStyle::Dashed,
            T::Wavy => ls::TextDecorationStyle::Wavy,
            _ => ls::TextDecorationStyle::Solid,
        }
    }

    fn text_decoration_color(&self) -> Color {
        self.color_of(&self.values.get_text().text_decoration_color)
    }

    fn direction(&self) -> snowlark_paint::Direction {
        // (stylo's writing mode flags: `text-orientation: upright` in a vertical writing mode
        // makes the used direction ltr, CSS Writing Modes 4 §5.1)
        if self
            .values
            .writing_mode
            .contains(style::logical_geometry::WritingMode::RTL)
        {
            snowlark_paint::Direction::Rtl
        } else {
            snowlark_paint::Direction::Ltr
        }
    }

    fn unicode_bidi(&self) -> ls::UnicodeBidi {
        use style::properties::longhands::unicode_bidi::computed_value::T;
        match self.values.get_text().unicode_bidi {
            T::Normal => ls::UnicodeBidi::Normal,
            T::Embed => ls::UnicodeBidi::Embed,
            T::Isolate => ls::UnicodeBidi::Isolate,
            T::BidiOverride => ls::UnicodeBidi::BidiOverride,
            T::IsolateOverride => ls::UnicodeBidi::IsolateOverride,
            T::Plaintext => ls::UnicodeBidi::Plaintext,
        }
    }

    fn view(&self) -> ls::Flip {
        self.view
    }

    fn with_view(&self, view: ls::Flip) -> Self {
        RenderStyle {
            values: self.values.clone(),
            env: self.env.clone(),
            content: self.content.clone(),
            clip_reference: self.clip_reference.clone(),
            filter_references: self.filter_references.clone(),
            remembered: self.remembered,
            view,
            animations: self.animations,
            custom_scrollbar: self.custom_scrollbar,
        }
    }

    fn clip_rect(&self) -> Option<[Option<f32>; 4]> {
        use style::values::generics::ClipRectOrAuto as C;
        let C::Rect(r) = &self.values.get_effects().clip else {
            return None;
        };
        let v = |l: &c::LengthOrAuto| l.non_auto().map(|l| l.px());
        Some([v(&r.top), v(&r.right), v(&r.bottom), v(&r.left)])
    }

    fn text_combine_upright(&self) -> bool {
        use style::properties::longhands::text_combine_upright::computed_value::T;
        self.values.get_inherited_text().text_combine_upright == T::All
    }

    fn same_inline_preparation(&self, other: &Self) -> bool {
        // Whole style structs (a struct shared between the two styles compares by address): every
        // property preparation reads is in them, with some it does not (`color`, `line-height`),
        // whose change prepares the content again.
        fn same<T: PartialEq>(a: &T, b: &T) -> bool {
            std::ptr::eq(a, b) || a == b
        }
        let (a, b) = (&*self.values, &*other.values);
        self.view == other.view
            && Arc::ptr_eq(&self.env, &other.env)
            && a.writing_mode == b.writing_mode
            && same(a.get_font(), b.get_font())
            && same(a.get_inherited_text(), b.get_inherited_text())
            && same(a.get_inherited_box(), b.get_inherited_box())
            && a.get_text().unicode_bidi == b.get_text().unicode_bidi
    }

    fn text_orientation(&self) -> ls::TextOrientation {
        use style::properties::longhands::text_orientation::computed_value::T;
        match self.values.get_inherited_box().text_orientation {
            T::Mixed => ls::TextOrientation::Mixed,
            T::Upright => ls::TextOrientation::Upright,
            T::Sideways => ls::TextOrientation::Sideways,
        }
    }

    fn writing_mode(&self) -> ls::WritingMode {
        use style::values::computed::WritingModeProperty as W;
        match self.values.get_inherited_box().writing_mode {
            W::HorizontalTb => ls::WritingMode::HorizontalTb,
            W::VerticalRl => ls::WritingMode::VerticalRl,
            W::VerticalLr => ls::WritingMode::VerticalLr,
            W::SidewaysRl => ls::WritingMode::SidewaysRl,
            W::SidewaysLr => ls::WritingMode::SidewaysLr,
        }
    }

    fn list_style_type(&self) -> ls::ListStyleType {
        counter_style_name(&self.values.get_list().list_style_type.0)
    }

    fn list_style_position(&self) -> ls::ListStylePosition {
        use style::properties::longhands::list_style_position::computed_value::T;
        match self.values.get_list().list_style_position {
            T::Outside => ls::ListStylePosition::Outside,
            T::Inside => ls::ListStylePosition::Inside,
        }
    }

    fn list_style_image(&self) -> ls::Image {
        self.image(&self.values.get_list().list_style_image)
    }

    fn content(&self) -> ls::Content {
        if let Some(content) = &self.content {
            return (**content).clone();
        }
        use style::values::generics::counters::GenericContent as C;
        match &self.values.get_counters().content {
            C::Normal => ls::Content::Normal,
            C::None => ls::Content::None,
            // Items need the element's counters and quote depth: the style source resolves them
            // (`crate::content`); a handle made without them shows the strings and images only.
            C::Items(items) => {
                crate::content::resolve_simple(self, &items.items[..items.alt_start])
            }
        }
    }

    fn object_fit(&self) -> ls::ObjectFit {
        use style::properties::longhands::object_fit::computed_value::T;
        match self.values.get_position().object_fit {
            T::Fill => ls::ObjectFit::Fill,
            T::Contain => ls::ObjectFit::Contain,
            T::Cover => ls::ObjectFit::Cover,
            T::None => ls::ObjectFit::None,
            T::ScaleDown => ls::ObjectFit::ScaleDown,
        }
    }

    fn object_view_box(&self) -> Option<[ls::LengthPercentage; 4]> {
        use style::values::generics::basic_shape::GenericObjectViewBox;
        match &self.values.get_position().object_view_box {
            GenericObjectViewBox::None => None,
            GenericObjectViewBox::Rect(inset) => {
                let r = &inset.rect;
                Some([lp(&r.0), lp(&r.1), lp(&r.2), lp(&r.3)])
            }
        }
    }

    fn clip_path(&self) -> Option<ls::ClipPath> {
        use style::values::generics::basic_shape::{
            FillRule, GenericBasicShape as B, GenericClipPath as C, GenericShapeRadius as R,
            ShapeBox, ShapeGeometryBox as G,
        };
        use style::values::generics::position::GenericPositionOrAuto as P;
        // (CSS Masking 1 §5.1: for a CSS box `fill-box` is the content box, `stroke-box` and
        // `view-box` the border box)
        let reference = |b: &G| match b {
            G::ShapeBox(ShapeBox::MarginBox) => ls::ClipBox::Margin,
            G::ShapeBox(ShapeBox::PaddingBox) => ls::ClipBox::Padding,
            G::ShapeBox(ShapeBox::ContentBox) | G::FillBox => ls::ClipBox::Content,
            G::ShapeBox(ShapeBox::BorderBox) | G::StrokeBox | G::ViewBox | G::ElementDependent => {
                ls::ClipBox::Border
            }
        };
        let radius = |r: &c::basic_shape::ShapeRadius| match r {
            R::Length(l) => ls::ShapeRadius::Length(lp(&l.0)),
            R::ClosestSide => ls::ShapeRadius::ClosestSide,
            R::FarthestSide => ls::ShapeRadius::FarthestSide,
            R::ClosestCorner => ls::ShapeRadius::ClosestCorner,
            R::FarthestCorner => ls::ShapeRadius::FarthestCorner,
        };
        let center = |p: &P<c::Position>| match p {
            P::Position(p) => Some((lp(&p.horizontal), lp(&p.vertical))),
            P::Auto => None,
        };
        let zoom = self.values.effective_zoom.value();
        match &self.values.get_svg().clip_path {
            C::None => None,
            C::Url(_) => self.clip_reference.as_ref().map(|r| ls::ClipPath {
                shape: ls::ClipShape::Reference(r.clone()),
                reference: ls::ClipBox::Border,
                zoom,
            }),
            C::Box(b) => Some(ls::ClipPath {
                shape: ls::ClipShape::Box,
                reference: reference(b),
                zoom,
            }),
            C::Shape(shape, b) => {
                let shape = match &**shape {
                    B::Rect(i) => {
                        let corner = |r: &style::values::computed::BorderCornerRadius| {
                            (lp(&r.0.width.0), lp(&r.0.height.0))
                        };
                        ls::ClipShape::Inset {
                            sides: Box::new([
                                lp(&i.rect.0),
                                lp(&i.rect.1),
                                lp(&i.rect.2),
                                lp(&i.rect.3),
                            ]),
                            radii: Box::new([
                                corner(&i.round.top_left),
                                corner(&i.round.top_right),
                                corner(&i.round.bottom_right),
                                corner(&i.round.bottom_left),
                            ]),
                        }
                    }
                    B::Circle(s) => ls::ClipShape::Circle {
                        center: center(&s.position),
                        radius: radius(&s.radius),
                    },
                    B::Ellipse(s) => ls::ClipShape::Ellipse {
                        center: center(&s.position),
                        radii: (radius(&s.semiaxis_x), radius(&s.semiaxis_y)),
                    },
                    B::Polygon(s) => ls::ClipShape::Polygon {
                        even_odd: s.fill == FillRule::Evenodd,
                        points: s.coordinates.iter().map(|p| (lp(&p.0), lp(&p.1))).collect(),
                    },
                    B::PathOrShape(s) => {
                        use style::values::generics::basic_shape::GenericPathOrShapeFunction as F;
                        match s {
                            // (`path()` is plain numbers: CSS pixels before zoom, unlike the
                            // computed lengths of `shape()`)
                            F::Path(p) => ls::ClipShape::Path {
                                even_odd: p.fill == FillRule::Evenodd,
                                commands: path_commands(
                                    p.path.commands(),
                                    &|p| {
                                        (
                                            ls::LengthPercentage::px(p.horizontal * zoom),
                                            ls::LengthPercentage::px(p.vertical * zoom),
                                        )
                                    },
                                    &|l| ls::LengthPercentage::px(*l * zoom),
                                    &|a| a.to_radians(),
                                ),
                            },
                            F::Shape(s) => ls::ClipShape::Path {
                                even_odd: s.fill == FillRule::Evenodd,
                                commands: path_commands(
                                    &s.commands,
                                    &|p| (lp(&p.horizontal), lp(&p.vertical)),
                                    &lp,
                                    &|a| a.radians(),
                                ),
                            },
                        }
                    }
                };
                Some(ls::ClipPath {
                    shape,
                    reference: reference(b),
                    zoom,
                })
            }
        }
    }

    fn has_filter(&self) -> bool {
        use style::values::generics::effects::Filter as F;
        self.values
            .get_effects()
            .filter
            .0
            .iter()
            .any(|f| !matches!(f, F::Url(_)) || self.filter_references.is_some())
    }

    fn filter_ops(&self, reference_box: Rect) -> Vec<FilterOp> {
        self.filter_list_ops(&self.values.get_effects().filter.0, reference_box)
    }

    fn backdrop_filter_ops(&self, reference_box: Rect) -> Vec<FilterOp> {
        self.filter_list_ops(&self.values.get_effects().backdrop_filter.0, reference_box)
    }

    fn has_clip_path(&self) -> bool {
        use style::values::generics::basic_shape::GenericClipPath as C;
        match &self.values.get_svg().clip_path {
            C::None => false,
            C::Url(_) => self.clip_reference.is_some(),
            C::Box(_) | C::Shape(..) => true,
        }
    }

    fn phys_object_position(&self) -> (ls::LengthPercentage, ls::LengthPercentage) {
        let p = &self.values.get_position().object_position;
        (lp(&p.horizontal), lp(&p.vertical))
    }

    fn image_rendering(&self) -> snowlark_paint::ImageRendering {
        use style::values::computed::ImageRendering as R;
        match self.values.get_inherited_box().image_rendering {
            R::CrispEdges | R::Pixelated | R::Optimizespeed => {
                snowlark_paint::ImageRendering::Pixelated
            }
            _ => snowlark_paint::ImageRendering::Smooth,
        }
    }

    fn visibility(&self) -> ls::Visibility {
        use style::properties::longhands::visibility::computed_value::T;
        match self.values.get_inherited_box().visibility {
            T::Visible => ls::Visibility::Visible,
            T::Hidden => ls::Visibility::Hidden,
            T::Collapse => ls::Visibility::Collapse,
        }
    }

    fn opacity(&self) -> f32 {
        self.values.get_effects().opacity.clamp(0.0, 1.0)
    }

    fn background_color(&self) -> Color {
        self.color_of(&self.values.get_background().background_color)
    }

    fn background_layer_count(&self) -> usize {
        let images = &self.values.get_background().background_image.0;
        // `background-image: none` alone is one layer that paints nothing.
        if images.len() == 1 && matches!(images[0], c::Image::None) {
            0
        } else {
            images.len()
        }
    }

    fn background_color_area(&self) -> (ls::BoxArea, ls::BackgroundAttachment) {
        // (the bottom layer is the last image's, `none` included: the other lists repeat to the
        // images' count)
        let n = self.values.get_background().background_image.0.len().max(1);
        let l = self.background_layer(n - 1);
        (l.clip, l.attachment)
    }

    fn background_layer(&self, index: usize) -> ls::BackgroundLayer {
        use style::values::generics::background::GenericBackgroundSize as Bs;
        use style::values::generics::length::GenericLengthPercentageOrAuto as Lpa;
        use style::values::specified::background::BackgroundRepeatKeyword as R;
        let b = self.values.get_background();
        // CSS Backgrounds 3 §2.1: the other lists repeat to the number of images.
        fn nth<T>(list: &[T], i: usize) -> &T {
            &list[i % list.len().max(1)]
        }
        let image = match b.background_image.0.get(index) {
            Some(i) => self.image(i),
            None => ls::Image::None,
        };
        let size = match nth(&b.background_size.0, index) {
            Bs::Cover => ls::BackgroundSize::Cover,
            Bs::Contain => ls::BackgroundSize::Contain,
            Bs::ExplicitSize { width, height } => {
                let a = |v: &Lpa<c::NonNegativeLengthPercentage>| match v {
                    Lpa::Auto => None,
                    Lpa::LengthPercentage(l) => Some(lp(&l.0)),
                };
                match (a(width), a(height)) {
                    (None, None) => ls::BackgroundSize::Auto,
                    (w, h) => ls::BackgroundSize::Explicit(w, h),
                }
            }
        };
        let repeat = |k: R| match k {
            R::Repeat => ls::BackgroundRepeat::Repeat,
            R::NoRepeat => ls::BackgroundRepeat::NoRepeat,
            R::Space => ls::BackgroundRepeat::Space,
            R::Round => ls::BackgroundRepeat::Round,
        };
        let r = nth(&b.background_repeat.0, index);
        let area_origin = {
            use style::properties::longhands::background_origin::single_value::computed_value::T as O;
            match nth(&b.background_origin.0, index) {
                O::BorderBox => ls::BoxArea::BorderBox,
                O::PaddingBox => ls::BoxArea::PaddingBox,
                O::ContentBox => ls::BoxArea::ContentBox,
            }
        };
        let area_clip = {
            use style::values::computed::BackgroundClip as Cl;
            match nth(&b.background_clip.0, index) {
                Cl::BorderBox | Cl::BorderArea => ls::BoxArea::BorderBox,
                Cl::PaddingBox => ls::BoxArea::PaddingBox,
                Cl::ContentBox => ls::BoxArea::ContentBox,
            }
        };
        let attachment = {
            use style::properties::longhands::background_attachment::single_value::computed_value::T as A;
            match nth(&b.background_attachment.0, index) {
                A::Scroll => ls::BackgroundAttachment::Scroll,
                A::Fixed => ls::BackgroundAttachment::Fixed,
                A::Local => ls::BackgroundAttachment::Local,
            }
        };
        ls::BackgroundLayer {
            image,
            position: (
                lp(nth(&b.background_position_x.0, index)),
                lp(nth(&b.background_position_y.0, index)),
            ),
            size,
            repeat: (repeat(r.0), repeat(r.1)),
            origin: area_origin,
            clip: area_clip,
            attachment,
        }
    }

    fn mask_layer_count(&self) -> usize {
        let images = &self.values.get_svg().mask_image.0;
        // `mask-image: none` alone is one layer that masks nothing (no mask).
        if images.iter().all(|i| matches!(i, c::Image::None)) {
            0
        } else {
            images.len()
        }
    }

    fn mask_layer(&self, index: usize) -> ls::MaskLayer {
        use style::properties::longhands::mask_mode::single_value::computed_value::T as Mode;
        use style::properties::longhands::mask_origin::single_value::computed_value::T as O;
        use style::values::computed::BackgroundClip as Cl;
        use style::values::generics::background::GenericBackgroundSize as Bs;
        use style::values::generics::length::GenericLengthPercentageOrAuto as Lpa;
        use style::values::specified::background::BackgroundRepeatKeyword as R;
        let m = self.values.get_svg();
        // CSS Masking 1 §7: the other lists repeat to the number of images, as backgrounds' do.
        fn nth<T>(list: &[T], i: usize) -> &T {
            &list[i % list.len().max(1)]
        }
        // (an image not loaded, or that failed, is a transparent layer: it masks everything out)
        let image = match m.mask_image.0.get(index) {
            Some(i) => self.image(i),
            None => ls::Image::None,
        };
        let size = match nth(&m.mask_size.0, index) {
            Bs::Cover => ls::BackgroundSize::Cover,
            Bs::Contain => ls::BackgroundSize::Contain,
            Bs::ExplicitSize { width, height } => {
                let a = |v: &Lpa<c::NonNegativeLengthPercentage>| match v {
                    Lpa::Auto => None,
                    Lpa::LengthPercentage(l) => Some(lp(&l.0)),
                };
                match (a(width), a(height)) {
                    (None, None) => ls::BackgroundSize::Auto,
                    (w, h) => ls::BackgroundSize::Explicit(w, h),
                }
            }
        };
        let repeat = |k: R| match k {
            R::Repeat => ls::BackgroundRepeat::Repeat,
            R::NoRepeat => ls::BackgroundRepeat::NoRepeat,
            R::Space => ls::BackgroundRepeat::Space,
            R::Round => ls::BackgroundRepeat::Round,
        };
        let r = nth(&m.mask_repeat.0, index);
        let origin = match nth(&m.mask_origin.0, index) {
            O::BorderBox => ls::BoxArea::BorderBox,
            O::PaddingBox => ls::BoxArea::PaddingBox,
            O::ContentBox => ls::BoxArea::ContentBox,
        };
        let clip = match nth(&m.mask_clip.0, index) {
            Cl::PaddingBox => ls::BoxArea::PaddingBox,
            Cl::ContentBox => ls::BoxArea::ContentBox,
            _ => ls::BoxArea::BorderBox,
        };
        ls::MaskLayer {
            layer: ls::BackgroundLayer {
                image,
                position: (
                    lp(nth(&m.mask_position_x.0, index)),
                    lp(nth(&m.mask_position_y.0, index)),
                ),
                size,
                repeat: (repeat(r.0), repeat(r.1)),
                origin,
                clip,
                attachment: ls::BackgroundAttachment::Scroll,
            },
            // (`match-source` takes an image's alpha; only an SVG `<mask>` reference, not
            // supported, would take its luminance)
            luminance: *nth(&m.mask_mode.0, index) == Mode::Luminance,
        }
    }

    fn border_image(&self) -> Option<ls::BorderImage> {
        use style::values::computed::NumberOrPercentage;
        use style::values::generics::border::GenericBorderImageSideWidth as W;
        use style::values::generics::length::GenericLengthOrNumber as LN;
        use style::values::specified::border::BorderImageRepeatKeyword as K;
        let b = self.values.get_border();
        if matches!(b.border_image_source, c::Image::None) {
            return None;
        }
        let source = self.image(&b.border_image_source);
        if source == ls::Image::None {
            return None;
        }
        macro_rules! sides {
            ($r:expr) => {
                [&$r.0, &$r.1, &$r.2, &$r.3]
            };
        }
        let slices = &b.border_image_slice;
        let slice = sides!(slices.offsets).map(|v| match v.0 {
            NumberOrPercentage::Number(n) => ls::NumberOrPercent::Number(n),
            NumberOrPercentage::Percentage(p) => ls::NumberOrPercent::Percent(p.0 * 100.0),
        });
        let width = sides!(b.border_image_width).map(|v| match v {
            W::Number(n) => ls::BorderImageLength::Number(n.0),
            W::LengthPercentage(l) => ls::BorderImageLength::LengthPercentage(lp(&l.0)),
            W::Auto => ls::BorderImageLength::Auto,
        });
        let outset = sides!(b.border_image_outset).map(|v| match v {
            LN::Number(n) => ls::BorderImageLength::Number(n.0),
            LN::Length(l) => {
                ls::BorderImageLength::LengthPercentage(ls::LengthPercentage::px(l.0.px()))
            }
        });
        let repeat = |k: K| match k {
            K::Stretch => ls::BorderImageRepeat::Stretch,
            K::Repeat => ls::BorderImageRepeat::Repeat,
            K::Round => ls::BorderImageRepeat::Round,
            K::Space => ls::BorderImageRepeat::Space,
        };
        Some(ls::BorderImage {
            source,
            slice,
            fill: slices.fill,
            width,
            outset,
            repeat: (
                repeat(b.border_image_repeat.0),
                repeat(b.border_image_repeat.1),
            ),
        })
    }

    fn box_shadow_count(&self) -> usize {
        self.values.get_effects().box_shadow.0.len()
    }

    fn box_shadow(&self, index: usize) -> ls::Shadow {
        let list = &self.values.get_effects().box_shadow.0;
        shadow(&list[index], |c| self.color_of(c))
    }

    fn text_shadow_count(&self) -> usize {
        self.values.get_inherited_text().text_shadow.0.len()
    }

    fn text_shadow(&self, index: usize) -> ls::Shadow {
        let s = &self.values.get_inherited_text().text_shadow.0[index];
        ls::Shadow {
            offset_x: s.horizontal.px(),
            offset_y: s.vertical.px(),
            blur: s.blur.0.px(),
            spread: 0.0,
            color: self.color_of(&s.color),
            inset: false,
        }
    }

    fn outline_width(&self) -> f32 {
        use style::values::computed::OutlineStyle as O;
        let o = self.values.get_outline();
        match o.outline_style {
            O::BorderStyle(s) if s.none_or_hidden() => 0.0,
            _ => o.outline_width.0.to_f32_px(),
        }
    }

    fn outline_style(&self) -> snowlark_paint::BorderStyle {
        use style::values::computed::OutlineStyle as O;
        match self.values.get_outline().outline_style {
            O::Auto => snowlark_paint::BorderStyle::Solid,
            O::BorderStyle(s) => border_style(s),
        }
    }

    fn outline_color(&self) -> Color {
        self.color_of(&self.values.get_outline().outline_color)
    }

    fn outline_offset(&self) -> f32 {
        self.values.get_outline().outline_offset.to_f32_px()
    }

    fn has_transform(&self) -> bool {
        has_css_transform(&self.values)
    }

    fn phys_transform(&self, reference: Rect) -> Option<Matrix> {
        css_transform(&self.values, reference, None, None)
    }

    fn transform_in(
        &self,
        reference: Rect,
        containing: Option<&ls::ContainingBoxes>,
    ) -> Option<Matrix> {
        let path = crate::motion::url_fragment(&self.values).and_then(|id| {
            let paths = self.env.motion_paths.read().ok()?;
            paths.get(&id).cloned()
        });
        css_transform(&self.values, reference, containing, path.as_deref())
    }

    fn transform_needs_containing(&self) -> bool {
        crate::motion::needs_containing(&self.values)
    }

    fn transform_box_is_content(&self) -> bool {
        use style::values::computed::TransformBox;
        matches!(
            self.values.get_box().transform_box,
            TransformBox::ContentBox | TransformBox::FillBox
        )
    }

    fn establishes_fixed_cb(&self) -> bool {
        // (a current transform animation acts as `will-change: transform`: Web Animations 1 §5.7;
        // Blink's `ComputedStyle::HasTransform` counts `HasCurrentTransformRelatedAnimation`)
        snowlark_style::establishes_fixed_cb(&self.values)
            || self.animations & snowlark_style::ANIMATES_TRANSFORM != 0
    }

    fn forces_stacking_context(&self) -> bool {
        use style::properties::longhands::isolation::computed_value::T as Isolation;
        use style::properties::longhands::mix_blend_mode::computed_value::T as Blend;
        use style::values::computed::Contain;
        use style::values::specified::box_::WillChangeBits;
        let e = self.values.get_effects();
        let b = self.values.get_box();
        let svg = self.values.get_svg();
        !e.filter.0.is_empty()
            || !e.backdrop_filter.0.is_empty()
            // (`clip` clips the box as a whole, a stacking context's way)
            || (!e.clip.is_auto()
                && matches!(
                    self.position(),
                    ls::Position::Absolute | ls::Position::Fixed
                ))
            || e.mix_blend_mode != Blend::Normal
            || b.isolation == Isolation::Isolate
            || b.contain.contains(Contain::PAINT)
            || !matches!(
                svg.clip_path,
                style::values::generics::basic_shape::GenericClipPath::None
            )
            || svg
                .mask_image
                .0
                .iter()
                .any(|i| !matches!(i, c::Image::None))
            || b.will_change.bits.intersects(
                WillChangeBits::STACKING_CONTEXT_UNCONDITIONAL
                    | WillChangeBits::TRANSFORM
                    | WillChangeBits::OPACITY,
            )
            // (a current opacity or transform animation acts as `will-change` naming it: Web
            // Animations 1 §5.7; Blink's `ComputedStyle::ShouldCompositeForCurrentAnimations` in
            // `CalculateIsStackingContextWithoutContainment`)
            || self.animations != 0
    }

    fn forms_backdrop_root(&self) -> bool {
        use style::properties::longhands::mix_blend_mode::computed_value::T as Blend;
        use style::values::generics::basic_shape::GenericClipPath as C;
        use style::values::specified::box_::WillChangeBits;
        let e = self.values.get_effects();
        !matches!(self.values.get_svg().clip_path, C::None)
            || !e.backdrop_filter.0.is_empty()
            || e.mix_blend_mode != Blend::Normal
            || self
                .values
                .get_box()
                .will_change
                .bits
                .contains(WillChangeBits::BACKDROP_ROOT)
    }

    fn will_change_composited(&self) -> bool {
        use style::values::specified::box_::WillChangeBits;
        self.values
            .get_box()
            .will_change
            .bits
            .intersects(WillChangeBits::TRANSFORM | WillChangeBits::OPACITY)
    }

    fn pointer_events(&self) -> bool {
        use style::values::computed::PointerEvents as P;
        self.values.get_inherited_ui().pointer_events != P::None && !self.inert()
    }

    fn inert(&self) -> bool {
        use style::values::computed::Inert;
        self.values.get_inherited_ui()._moz_inert == Inert::Inert
    }

    fn color_scheme(&self) -> ls::ColorScheme {
        used_color_scheme(&self.values)
    }

    fn scrollbar_color(&self) -> Option<ls::ScrollbarColors> {
        scrollbar_colors(&self.values)
    }

    fn scrollbar_width(&self) -> ls::ScrollbarWidth {
        scrollbar_width(&self.values)
    }

    fn custom_scrollbar(&self) -> Option<ls::CustomScrollbar> {
        self.custom_scrollbar
    }

    fn resize(&self) -> ls::Resize {
        use style::values::computed::Resize as R;
        match self.values.get_box().clone_resize() {
            R::None => ls::Resize::None,
            R::Both => ls::Resize::Both,
            R::Horizontal => ls::Resize::Horizontal,
            R::Vertical => ls::Resize::Vertical,
        }
    }
}

/// The used color scheme of the box with these computed values (CSS Color Adjust 1 §2.3).
pub fn used_color_scheme(values: &ComputedValues) -> ls::ColorScheme {
    match snowlark_style::color_scheme::used_color_scheme(values) {
        snowlark_style::ColorScheme::Light => ls::ColorScheme::Light,
        snowlark_style::ColorScheme::Dark => ls::ColorScheme::Dark,
    }
}

/// `scrollbar-color` (CSS Scrollbars 1 §2): thumb and track, `None` for `auto`.
pub fn scrollbar_colors(values: &ComputedValues) -> Option<ls::ScrollbarColors> {
    use style::values::generics::ui::ScrollbarColor;
    match &values.get_inherited_ui().scrollbar_color {
        ScrollbarColor::Auto => None,
        ScrollbarColor::Colors { thumb, track } => Some(ls::ScrollbarColors {
            thumb: resolve_color(values, thumb),
            track: resolve_color(values, track),
        }),
    }
}

/// `scrollbar-width` (CSS Scrollbars 1 §3).
pub fn scrollbar_width(values: &ComputedValues) -> ls::ScrollbarWidth {
    use style::computed_values::scrollbar_width::T;
    match values.get_ui().scrollbar_width {
        T::Auto => ls::ScrollbarWidth::Auto,
        T::Thin => ls::ScrollbarWidth::Thin,
        T::None => ls::ScrollbarWidth::None,
    }
}

/// The custom scroll bars of `element`, when a rule styles its `::-webkit-scrollbar` (the parts'
/// styles, cascaded from it: [`ls::CustomScrollbar::from_parts`]).
pub(crate) fn custom_scrollbar(
    engine: &snowlark_style::StyleEngine,
    dom: &snowlark_dom::Dom,
    data: &snowlark_style::StyleData,
    env: &Arc<StyleEnv>,
    element: snowlark_dom::NodeId,
) -> Option<ls::CustomScrollbar> {
    use style::selector_parser::PseudoElement as P;
    let part = |p: P| {
        engine
            .pseudo_style(dom, data, element, &p)
            .map(|v| RenderStyle::new(v, env.clone()))
    };
    let bar = part(P::WebkitScrollbar)?;
    let thumb = part(P::WebkitScrollbarThumb);
    let track = part(P::WebkitScrollbarTrack);
    Some(ls::CustomScrollbar::from_parts(
        &bar,
        thumb.as_ref(),
        track.as_ref(),
    ))
}

/// Whether `values` make a scroll container that shows scroll bars: `overflow` `auto` or `scroll`
/// on an axis (CSS Overflow 3 §3).
pub(crate) fn shows_scrollbars(values: &ComputedValues) -> bool {
    use style::values::computed::Overflow as O;
    let b = values.get_box();
    matches!(b.overflow_x, O::Auto | O::Scroll) || matches!(b.overflow_y, O::Auto | O::Scroll)
}

/// The paint crate's color from a stylo color in the context of `style` (its `currentcolor`).
pub fn resolve_color(style: &ComputedValues, color: &c::Color) -> Color {
    to_color(&style.resolve_color(color))
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<RenderStyle>();
}
