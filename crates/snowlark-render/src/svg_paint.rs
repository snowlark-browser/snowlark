// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inline SVG recorded as display list paint operations, as Blink paints it: `SVGRootPainter` maps
//! the outermost `svg` element's content box to its user space (the `viewBox` transform),
//! `SVGContainerPainter` paints a `g`'s children under its transform and effects, `SVGShapePainter`
//! fills and strokes a shape's path with what `SVGObjectPainter` makes of its computed `fill` and
//! `stroke`, and the text painter draws shaped glyph runs. The page thread only records; the raster
//! side draws ([`snowlark_paint::Item::FillPath`], [`snowlark_paint::Item::StrokePath`], glyph runs).
//!
//! What it does not record yet falls back to the image path ([`crate::inline_svg`], drawn by resvg):
//! [`record`] then says why, and the whole `svg` element is drawn as before. Recorded: `g` and `a`,
//! the basic shapes and `path` with solid paints (fill and stroke opacity, `fill-rule`, caps, joins,
//! dashes, `shape-rendering`, `paint-order`), `transform`, `opacity`, `visibility`, clip paths of one
//! `rect` in user space, and `text` and `tspan` of horizontal text with one position per element.

use kurbo::BezPath;
use servo_arc::Arc;
use snowlark_dom::{ns, NodeData, NodeId};
use snowlark_layout::svg::{LengthDir, Matrix};
use snowlark_layout::Style as _;
use snowlark_paint::{
    Color, DisplayList, DisplayListBuilder, LineCap, LineJoin, PathEl, ShapeOptions, ShapedText,
    Stroke, VerticalText,
};
use snowlark_style::ComputedValues;
use std::collections::HashMap;
use style::values::computed::Length;
use style::values::generics::svg::{
    GenericSVGLength as SVGLength, GenericSVGOpacity as SVGOpacity,
    GenericSVGPaintKind as SVGPaintKind, GenericSVGStrokeDashArray as SVGStrokeDashArray,
};

use crate::adapter::{to_color, RenderStyle};
use crate::svg_geometry::SvgQuery;

/// Why an inline `svg` element is not recorded (it is drawn as an image instead).
pub type Unsupported = &'static str;

/// The display list of the outermost `svg` element `svg`'s content, in its content box's
/// coordinates (origin at the box's top left), for a content box of `size` CSS px.
pub fn record(q: &SvgQuery, svg: NodeId, size: (f32, f32)) -> Result<DisplayList, Unsupported> {
    let st = q.style.as_ref().ok_or("not rendered")?;
    let text = st.text;
    // SVG 2 §8.6: a zero width or height in the `viewBox` disables rendering of the element.
    let zero_view_box = q.attr(svg, "viewBox").is_some_and(|v| {
        let n: smallvec::SmallVec<[f32; 4]> = v
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        n.len() == 4 && (n[2] == 0.0 || n[3] == 0.0)
    });
    if zero_view_box {
        return Ok(DisplayList::new());
    }
    // (layout draws no filter on the box: the image path applies the root's)
    if let Some(v) = q.values(svg) {
        unsupported_effects(q, svg, v)?;
    }
    // (CSS `zoom` scales the computed lengths but not the user space's geometry: not yet)
    if q.values(svg)
        .is_some_and(|v| v.effective_zoom != style::values::computed::Zoom::ONE)
    {
        return Err("zoom");
    }
    let (vw, vh) = q.inner_viewport(svg).ok_or("no viewport")?;
    let laid_out = q.viewport_size(svg).ok_or("no viewport")?;
    let mut m = q.local_transform(svg);
    if laid_out != size && laid_out.0 > 0.0 && laid_out.1 > 0.0 {
        m = Matrix::scale_non_uniform((size.0 / laid_out.0) as f64, (size.1 / laid_out.1) as f64)
            * m;
    }
    let mut r = Recorder {
        q,
        text,
        b: DisplayListBuilder::new(),
        diag: ((vw * vw + vh * vh) / 2.0).sqrt(),
        path: Vec::new(),
    };
    r.b.push_transform(paint_matrix(&m));
    r.children(svg)?;
    r.b.pop_transform();
    Ok(r.b.build())
}

/// What an inline svg that [`record`] draws is made of, in place of its markup
/// (`docs/svg-paint.md`, step 1): its elements' names, attributes (with animated values) and text,
/// hashed, and each element's computed values, held. Equal fingerprints draw the same.
pub(crate) struct Fingerprint {
    hash: u64,
    styles: Vec<Arc<ComputedValues>>,
    key: u64,
}

impl Fingerprint {
    /// Whether it draws as `other` does: the same content and the same computed values, element
    /// by element (the same objects, or equal ones). For the svg's own earlier fingerprint, whether
    /// it is unchanged; for another svg's, whether it can take that one's recording.
    pub(crate) fn draws_like(&self, other: &Fingerprint) -> bool {
        self.hash == other.hash
            && self.styles.len() == other.styles.len()
            && self
                .styles
                .iter()
                .zip(&other.styles)
                .all(|(a, b)| snowlark_style::computed::same_values(a, b))
    }

    /// The hash of its content (names, attributes and text), without the styles.
    pub(crate) fn content(&self) -> u64 {
        self.hash
    }

    /// The key its recordings are cached and shared by: the content's hash with the addresses of
    /// the computed values, so two svgs alive at once have equal keys only when they have the same
    /// content and share every style.
    pub(crate) fn key(&self) -> u64 {
        self.key
    }
}

/// The fingerprint of outermost `svg` element `svg`, or `None` when it needs its markup: it
/// fetches something (`use`, `image`, `feImage`, an `href` or a `url()` to another document,
/// which the writer in [`crate::inline_svg`] finds and starts), or holds what [`record`] does not
/// record (paint servers, markers, filters, masks, nested `svg`, `textPath`, `animateMotion`, a
/// clip path outside the svg). `document` and `base` are the document's URL and base URL.
pub(crate) fn fingerprint(
    dom: &snowlark_dom::Dom,
    data: &snowlark_style::StyleData,
    svg: NodeId,
    document: &snowlark_net::Url,
    base: &snowlark_net::Url,
) -> Option<Fingerprint> {
    use std::hash::{Hash, Hasher};
    struct Walk<'a> {
        dom: &'a snowlark_dom::Dom,
        data: &'a snowlark_style::StyleData,
        svg: NodeId,
        document: &'a snowlark_net::Url,
        base: &'a snowlark_net::Url,
        h: std::collections::hash_map::DefaultHasher,
        styles: Vec<Arc<ComputedValues>>,
    }
    impl Walk<'_> {
        fn node(&mut self, n: NodeId) -> Option<()> {
            match self.dom.data(n) {
                NodeData::Element(e) => {
                    let svg_ns = e.name().ns == ns!(svg);
                    let local: &str = &e.name().local;
                    (1u8, svg_ns, local).hash(&mut self.h);
                    if svg_ns
                        && (matches!(
                            local,
                            "use"
                                | "image"
                                | "feImage"
                                | "foreignObject"
                                | "textPath"
                                | "mpath"
                                | "animateMotion"
                                | "switch"
                        ) || (local == "svg" && n != self.svg))
                    {
                        return None;
                    }
                    for a in e.attrs() {
                        let name: &str = a.local_name();
                        let value = a.value();
                        if name == "href" && local != "a" {
                            let id = value.trim().strip_prefix('#')?;
                            // (a gradient taking stops or attributes from another: that one is
                            // part of what the walk sees)
                            if matches!(local, "linearGradient" | "radialGradient") {
                                self.inside_id(id)?;
                            }
                        }
                        let effect = matches!(
                            name,
                            "filter"
                                | "mask"
                                | "marker-start"
                                | "marker-mid"
                                | "marker-end"
                                | "marker"
                                | "pathLength"
                        );
                        if effect && *a.namespace() == ns!() && value.trim() != "none" {
                            return None;
                        }
                        (&**a.namespace(), name, value).hash(&mut self.h);
                    }
                    for (name, value) in self.dom.animated_attrs(n) {
                        (2u8, &**name, value).hash(&mut self.h);
                    }
                    match self.data.styles_arc(n) {
                        Some(v) => {
                            self.check(&v)?;
                            3u8.hash(&mut self.h);
                            self.styles.push(v);
                        }
                        None => 4u8.hash(&mut self.h),
                    }
                    for c in self.dom.children(n) {
                        self.node(c)?;
                    }
                    5u8.hash(&mut self.h);
                }
                NodeData::Text(_) | NodeData::CDataSection(_) => {
                    (6u8, self.dom.character_data(n).unwrap_or("")).hash(&mut self.h);
                }
                _ => {}
            }
            Some(())
        }

        /// What an element's computed values ask for that needs the markup.
        fn check(&self, v: &ComputedValues) -> Option<()> {
            let isvg = v.get_inherited_svg();
            self.paint(&isvg.fill)?;
            self.paint(&isvg.stroke)?;
            let none = style::values::computed::url::UrlOrNone::None;
            if isvg.marker_start != none || isvg.marker_mid != none || isvg.marker_end != none {
                return None;
            }
            let effects = v.get_effects();
            if !effects.filter.0.is_empty()
                || effects.mix_blend_mode
                    != style::properties::longhands::mix_blend_mode::computed_value::T::Normal
            {
                return None;
            }
            let s = v.get_svg();
            if s.mask_image
                .0
                .iter()
                .any(|i| !matches!(i, style::values::computed::Image::None))
            {
                return None;
            }
            use style::values::generics::basic_shape::GenericClipPath as ClipPath;
            match &s.clip_path {
                ClipPath::None => Some(()),
                // (the clip path is part of what the walk sees: inside this svg)
                ClipPath::Url(u) => u.url().and_then(|u| self.inside(u.as_str(), u.fragment())),
                _ => None,
            }
        }

        /// A paint the walk sees all of: none, a color, or a paint server inside this svg.
        fn paint(&self, p: &style::values::computed::SVGPaint) -> Option<()> {
            match &p.kind {
                SVGPaintKind::None | SVGPaintKind::Color(_) => Some(()),
                SVGPaintKind::PaintServer(u) => {
                    let url = u.url()?;
                    self.inside(url.as_str(), url.fragment())
                }
                _ => None,
            }
        }

        /// Whether `url` names an element of this document inside this svg.
        fn inside(&self, whole: &str, fragment: Option<&str>) -> Option<()> {
            let without = |s: &str| s.split('#').next().unwrap_or("").to_owned();
            let here = without(whole);
            if here != without(self.document.as_str()) && here != without(self.base.as_str()) {
                return None;
            }
            let id = snowlark_layout::svg::parse::percent_decoded(fragment?);
            self.inside_id(&id)
        }

        /// Whether the element with this id is inside this svg.
        fn inside_id(&self, id: &str) -> Option<()> {
            let target = self
                .dom
                .indexed_element_by_id(self.dom.root(self.svg), id)
                .flatten()?;
            std::iter::successors(Some(target), |&a| self.dom.parent(a))
                .any(|a| a == self.svg)
                .then_some(())
        }
    }
    let mut w = Walk {
        dom,
        data,
        svg,
        document,
        base,
        h: std::collections::hash_map::DefaultHasher::new(),
        styles: Vec::new(),
    };
    w.node(svg)?;
    let hash = w.h.finish();
    let mut k = std::collections::hash_map::DefaultHasher::new();
    hash.hash(&mut k);
    for v in &w.styles {
        (&**v as *const ComputedValues as usize).hash(&mut k);
    }
    Some(Fingerprint {
        hash,
        styles: w.styles,
        key: k.finish(),
    })
}

/// A kurbo affine as the display list's matrix.
fn paint_matrix(m: &Matrix) -> snowlark_paint::Matrix {
    let [a, b, c, d, e, f] = m.as_coeffs();
    snowlark_paint::Matrix::new(a as f32, b as f32, c as f32, d as f32, e as f32, f as f32)
}

fn point(p: kurbo::Point) -> snowlark_paint::Point {
    snowlark_paint::Point::new(p.x as f32, p.y as f32)
}

struct Recorder<'q, 'a> {
    q: &'q SvgQuery<'a>,
    text: &'q snowlark_paint::TextEngine,
    b: DisplayListBuilder,
    /// The normalized diagonal of the viewport (SVG 2 §8.9: what percentages of other lengths
    /// resolve against).
    diag: f32,
    path: Vec<PathEl>,
}

/// What an element is for painting.
enum Kind {
    Container,
    Shape,
    Text,
    /// Never rendered, or not rendered here (`title`, `defs`, animation elements, an unknown
    /// element...).
    Skip,
}

fn kind(name: &str) -> Result<Kind, Unsupported> {
    Ok(match name {
        "g" | "a" => Kind::Container,
        "path" | "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon" => Kind::Shape,
        "text" => Kind::Text,
        "svg" => return Err("nested svg"),
        "use" => return Err("use"),
        "image" => return Err("image"),
        "foreignObject" => return Err("foreignObject"),
        "switch" => return Err("switch"),
        "animateMotion" => return Err("animateMotion"),
        _ => Kind::Skip,
    })
}

impl Recorder<'_, '_> {
    fn values(&self, n: NodeId) -> Option<&ComputedValues> {
        self.q.values(n)
    }

    /// Records the rendered children of a container element.
    fn children(&mut self, parent: NodeId) -> Result<(), Unsupported> {
        let dom = self.q.dom();
        let children: smallvec::SmallVec<[NodeId; 16]> = dom
            .children(parent)
            .filter(|&c| dom.element(c).is_some_and(|e| e.name().ns == ns!(svg)))
            .collect();
        for c in children {
            let k = kind(self.q.dom().element(c).map_or("", |e| &e.name().local))?;
            if matches!(k, Kind::Skip) {
                continue;
            }
            let Some(v) = self.values(c) else {
                continue;
            };
            if v.get_box().display.is_none() || !self.q.passes_conditions(c) {
                continue;
            }
            self.element(c, k)?;
        }
        Ok(())
    }

    /// Records an element under its transform, clip path and opacity.
    fn element(&mut self, n: NodeId, k: Kind) -> Result<(), Unsupported> {
        let v = self.values(n).ok_or("unstyled")?;
        unsupported_effects(self.q, n, v)?;
        let effects = v.get_effects();
        if effects.mix_blend_mode
            != style::properties::longhands::mix_blend_mode::computed_value::T::Normal
        {
            return Err("mix-blend-mode");
        }
        let opacity = effects.opacity;
        if opacity <= 0.0 {
            return Ok(());
        }
        let clip = match self.clip(n, v)? {
            // (a clip path with nothing in it clips everything away)
            Clip::All => return Ok(()),
            Clip::Path(path, evenodd) => {
                convert(&path, &mut self.path);
                match self.b.add_path(&self.path) {
                    Some(range) => Clip::Range(range, evenodd),
                    None => return Ok(()),
                }
            }
            clip => clip,
        };
        let m = self.q.local_transform(n);
        let transformed = m != Matrix::IDENTITY;
        if transformed {
            self.b.push_transform(paint_matrix(&m));
        }
        match &clip {
            Clip::Rect(rect) => self.b.push_clip_rect(*rect),
            Clip::Range(range, evenodd) => self.b.push_clip_path(*range, *evenodd),
            Clip::Union(shapes, bounds) => {
                // (the union of the shapes: their opaque fills as the mask's alpha)
                self.b.push_mask(*bounds, false);
                for (path, evenodd) in shapes {
                    convert(path, &mut self.path);
                    if let Some(range) = self.b.add_path(&self.path) {
                        self.b
                            .fill_path(range, Color::rgba(0, 0, 0, 255), *evenodd, true);
                    }
                }
                self.b.mask_content();
            }
            _ => {}
        }
        let result = if opacity >= 1.0 {
            self.content(n, k, 1.0)
        } else if matches!(k, Kind::Shape) && self.paints(n) <= 1 {
            // (one paint under an opacity draws as that paint with the opacity in its alpha)
            self.content(n, k, opacity)
        } else {
            self.group(n, k, opacity)
        };
        match clip {
            Clip::Rect(_) | Clip::Range(..) => self.b.pop_clip(),
            Clip::Union(..) => self.b.pop_mask(),
            _ => {}
        }
        if transformed {
            self.b.pop_transform();
        }
        result
    }

    /// Records the element's content into a group composited with `opacity`.
    fn group(&mut self, n: NodeId, k: Kind, opacity: f32) -> Result<(), Unsupported> {
        let outer = std::mem::take(&mut self.b);
        let result = self.content(n, k, 1.0);
        let inner = std::mem::replace(&mut self.b, outer).build();
        result?;
        let mut bounds = Vec::new();
        inner.item_bounds(&snowlark_paint::Matrix::IDENTITY, &mut bounds);
        let Some(b) = bounds
            .iter()
            .filter(|r| !r.is_empty())
            .copied()
            .reduce(|a, b| a.union(&b))
        else {
            return Ok(());
        };
        self.b.push_opacity(opacity, b);
        self.b.append(&inner);
        self.b.pop_opacity();
        Ok(())
    }

    fn content(&mut self, n: NodeId, k: Kind, alpha: f32) -> Result<(), Unsupported> {
        match k {
            Kind::Container => self.children(n),
            Kind::Shape => self.shape(n, alpha),
            Kind::Text => self.text(n),
            Kind::Skip => Ok(()),
        }
    }

    /// `clip-path` referring to a `clipPath` element (CSS Masking 1 §6): its content's shapes, each
    /// under its own transform and `clip-rule`, in the element's user space through the
    /// `clipPath`'s transform and units. One shape clips as its path (a rectangle along the axes as
    /// a clip rectangle); several clip as their union, an alpha mask of their fills. (The reference
    /// engine clips by a path when the content makes one, else by a mask.)
    fn clip(&self, n: NodeId, v: &ComputedValues) -> Result<Clip, Unsupported> {
        use style::values::generics::basic_shape::GenericClipPath as ClipPath;
        // (a reference is to the fragment, whatever document the URL names, as for the image
        // path: crate::inline_svg)
        let id = match &v.get_svg().clip_path {
            ClipPath::None => return Ok(Clip::None),
            ClipPath::Url(u) => u.url().and_then(|u| u.fragment()).ok_or("clip-path url")?,
            _ => return Err("clip-path shape"),
        };
        let q = self.q;
        let dom = q.dom();
        let target = dom
            .indexed_element_by_id(dom.root(n), id)
            .flatten()
            .ok_or("clip-path target")?;
        let is_clip = dom
            .element(target)
            .is_some_and(|e| e.name().ns == ns!(svg) && &*e.name().local == "clipPath");
        if !is_clip {
            return Err("clip-path target");
        }
        if q.values(target)
            .is_some_and(|cv| !matches!(cv.get_svg().clip_path, ClipPath::None))
        {
            return Err("clipPath clip-path");
        }
        // CSS Masking 1 §6.2: the content's user space is the referencing element's, through the
        // `clipPath`'s transform, and with `objectBoundingBox` units, its bounding box.
        let mut m = q.local_transform(target);
        match q.attr(target, "clipPathUnits").map(str::trim) {
            None | Some("userSpaceOnUse") => {}
            Some("objectBoundingBox") => {
                let b = q
                    .bbox(n, crate::svg_geometry::BBoxOptions::default())
                    .filter(|b| b.width() > 0.0 && b.height() > 0.0)
                    .ok_or("clipPathUnits")?;
                m = m
                    * Matrix::translate((b.x0, b.y0))
                    * Matrix::scale_non_uniform(b.width(), b.height());
            }
            Some(_) => return Err("clipPathUnits"),
        }
        let mut shapes: Vec<(BezPath, bool)> = Vec::new();
        for c in dom.children(target) {
            let Some(e) = dom.element(c) else { continue };
            if e.name().ns != ns!(svg) {
                continue;
            }
            let name: &str = &e.name().local;
            // (§6.2: shapes, text and `use`; anything else draws nothing)
            match kind(name) {
                Ok(Kind::Shape) => {}
                Ok(Kind::Text) | Err("use") => return Err("clip path content"),
                _ => continue,
            }
            let Some(cv) = q.values(c) else { continue };
            if cv.get_box().display.is_none() || !q.passes_conditions(c) {
                continue;
            }
            if !matches!(cv.get_svg().clip_path, ClipPath::None) {
                return Err("clip path content");
            }
            if cv.get_inherited_box().visibility
                != style::properties::longhands::visibility::computed_value::T::Visible
            {
                continue;
            }
            if geometry_has_calc(cv) {
                return Err("calc() geometry");
            }
            if matches!(name, "polyline" | "polygon")
                && q.attr(c, "points")
                    .is_some_and(|p| snowlark_layout::svg::parse_points(p).is_none())
            {
                return Err("points in error");
            }
            let Some(path) = q.shape_path(c) else {
                continue;
            };
            let evenodd = cv.get_inherited_svg().clip_rule
                == style::values::generics::basic_shape::FillRule::Evenodd;
            shapes.push(((m * q.local_transform(c)) * path, evenodd));
        }
        Ok(match shapes.len() {
            0 => Clip::All,
            1 => {
                let (path, evenodd) = shapes.pop().expect("one");
                match axis_aligned_rect(&path) {
                    Some(r) if r.width() > 0.0 && r.height() > 0.0 => {
                        Clip::Rect(snowlark_paint::Rect::new(
                            r.x0 as f32,
                            r.y0 as f32,
                            r.width() as f32,
                            r.height() as f32,
                        ))
                    }
                    Some(_) => Clip::All,
                    None => Clip::Path(path, evenodd),
                }
            }
            _ => {
                let b = shapes
                    .iter()
                    .map(|(p, _)| kurbo::Shape::bounding_box(p))
                    .reduce(|a, b| a.union(b))
                    .expect("shapes");
                Clip::Union(
                    shapes,
                    snowlark_paint::Rect::new(
                        b.x0 as f32,
                        b.y0 as f32,
                        b.width() as f32,
                        b.height() as f32,
                    ),
                )
            }
        })
    }

    /// How many of `fill` and `stroke` paint (for folding an opacity into a single paint).
    fn paints(&self, n: NodeId) -> usize {
        let Some(v) = self.values(n) else { return 0 };
        let svg = v.get_inherited_svg();
        let on = |k: &SVGPaintKind<_, _>| !matches!(k, SVGPaintKind::None);
        on(&svg.fill.kind) as usize + on(&svg.stroke.kind) as usize
    }

    /// A paint's color with its opacity and `alpha` (`None`: no paint).
    fn paint_color(
        v: &ComputedValues,
        paint: &style::values::computed::SVGPaint,
        opacity: &style::values::computed::SVGOpacity,
        alpha: f32,
    ) -> Result<Option<Color>, Unsupported> {
        let c = match &paint.kind {
            SVGPaintKind::None => return Ok(None),
            SVGPaintKind::Color(c) => to_color(&v.resolve_color(c)),
            SVGPaintKind::PaintServer(_) => return Err("paint server"),
            _ => return Err("context paint"),
        };
        let o = match opacity {
            SVGOpacity::Opacity(o) => o.clamp(0.0, 1.0),
            _ => return Err("context opacity"),
        };
        let a = (c.a as f32 * o * alpha).round().clamp(0.0, 255.0) as u8;
        Ok(Some(Color::rgba(c.r, c.g, c.b, a)))
    }

    /// The stroke of an element, its dashes in `dashes` (user units).
    fn stroke(&self, v: &ComputedValues, dashes: &mut Vec<f32>) -> Result<Stroke, Unsupported> {
        use style::properties::longhands::{stroke_linecap, stroke_linejoin};
        let svg = v.get_inherited_svg();
        let diag = Length::new(self.diag);
        let width = match &svg.stroke_width {
            SVGLength::LengthPercentage(l) => l.0.resolve(diag).px(),
            _ => return Err("context stroke-width"),
        };
        dashes.clear();
        match &svg.stroke_dasharray {
            SVGStrokeDashArray::Values(list) => {
                dashes.extend(list.iter().map(|l| l.0.resolve(diag).px()));
            }
            _ => return Err("context dasharray"),
        }
        let dash_offset = match &svg.stroke_dashoffset {
            SVGLength::LengthPercentage(l) => l.resolve(diag).px(),
            _ => return Err("context dashoffset"),
        };
        Ok(Stroke {
            width,
            cap: match svg.stroke_linecap {
                stroke_linecap::computed_value::T::Butt => LineCap::Butt,
                stroke_linecap::computed_value::T::Round => LineCap::Round,
                stroke_linecap::computed_value::T::Square => LineCap::Square,
            },
            join: match svg.stroke_linejoin {
                stroke_linejoin::computed_value::T::Miter => LineJoin::Miter,
                stroke_linejoin::computed_value::T::Round => LineJoin::Round,
                stroke_linejoin::computed_value::T::Bevel => LineJoin::Bevel,
            },
            miter_limit: svg.stroke_miterlimit.0,
            dashes: Default::default(),
            dash_offset,
        })
    }

    /// Whether the stroke paints before the fill (`paint-order`, SVG 2 §13.7).
    fn stroke_first(v: &ComputedValues) -> bool {
        use style::values::specified::svg::PaintOrder;
        let order = &v.get_inherited_svg().paint_order;
        for i in 0..3 {
            match order.order_at(i) {
                PaintOrder::Fill | PaintOrder::Normal => return false,
                PaintOrder::Stroke => return true,
                PaintOrder::Markers => {}
            }
        }
        false
    }

    fn shape(&mut self, n: NodeId, alpha: f32) -> Result<(), Unsupported> {
        let q = self.q;
        let v = q.values(n).ok_or("unstyled")?;
        let svg = v.get_inherited_svg();
        if !v.get_svg().vector_effect.is_empty() {
            return Err("vector-effect");
        }
        let none = style::values::computed::url::UrlOrNone::None;
        if svg.marker_start != none || svg.marker_mid != none || svg.marker_end != none {
            let name = q.dom().element(n).map_or("", |e| &e.name().local);
            if matches!(name, "path" | "line" | "polyline" | "polygon") {
                return Err("markers");
            }
        }
        if v.get_inherited_box().visibility
            != style::properties::longhands::visibility::computed_value::T::Visible
        {
            return Ok(());
        }
        let none_paint =
            |p: &style::values::computed::SVGPaint| matches!(p.kind, SVGPaintKind::None);
        if none_paint(&svg.fill) && none_paint(&svg.stroke) {
            return Ok(());
        }
        if geometry_has_calc(v) {
            return Err("calc() geometry");
        }
        // (points in error: the image path draws the points before the error)
        let name = q.dom().element(n).map_or("", |e| &e.name().local);
        if matches!(name, "polyline" | "polygon")
            && q.attr(n, "points")
                .is_some_and(|p| snowlark_layout::svg::parse_points(p).is_none())
        {
            return Err("points in error");
        }
        let Some(path) = q.shape_path(n) else {
            return Ok(());
        };
        if path.elements().is_empty() {
            return Ok(());
        }
        // (a paint server's `objectBoundingBox` units: the shape's geometry box, SVG 2 §8.10)
        let bbox = kurbo::Shape::bounding_box(&path);
        let fill = self.shape_paint(n, v, &svg.fill, &svg.fill_opacity, alpha, bbox)?;
        let stroke_color = self.shape_paint(n, v, &svg.stroke, &svg.stroke_opacity, alpha, bbox)?;
        if fill.is_none() && stroke_color.is_none() {
            return Ok(());
        }
        convert(&path, &mut self.path);
        let Some(range) = self.b.add_path(&self.path) else {
            return Ok(());
        };
        use style::properties::longhands::shape_rendering::computed_value::T as SR;
        let antialias = !matches!(svg.shape_rendering, SR::Crispedges | SR::Optimizespeed);
        let evenodd = svg.fill_rule == style::values::generics::basic_shape::FillRule::Evenodd;
        let mut dashes = Vec::new();
        let stroke = match stroke_color {
            Some(_) => Some(self.stroke(v, &mut dashes)?),
            None => None,
        };
        if !dashes.is_empty() && q.attr(n, "pathLength").is_some() {
            return Err("pathLength");
        }
        let stroke_first = Self::stroke_first(v);
        let draw_fill = |b: &mut DisplayListBuilder| match &fill {
            Some(Paint::Solid(c)) => b.fill_path(range, *c, evenodd, antialias),
            // (the gradient under the shape's fill as a clip: SVG 2 §13.2 paints the fill area)
            Some(Paint::Gradient(g)) => {
                b.push_clip_path(range, evenodd);
                g.paint(b, bbox);
                b.pop_clip();
            }
            None => {}
        };
        if !stroke_first {
            draw_fill(&mut self.b);
        }
        match (&stroke_color, stroke.as_ref()) {
            (Some(Paint::Solid(c)), Some(s)) => {
                self.b.stroke_path(range, *c, s, &dashes, antialias)
            }
            (Some(Paint::Gradient(g)), Some(s)) => {
                // (the stroke's coverage as the gradient's mask)
                let half = s.width as f64 / 2.0;
                let reach = half * (s.miter_limit.max(std::f32::consts::SQRT_2) as f64);
                let area = bbox.inflate(reach, reach);
                self.b.push_mask(rect_of(area), false);
                self.b
                    .stroke_path(range, Color::rgba(0, 0, 0, 255), s, &dashes, antialias);
                self.b.mask_content();
                g.paint(&mut self.b, area);
                self.b.pop_mask();
            }
            _ => {}
        }
        if stroke_first {
            draw_fill(&mut self.b);
        }
        Ok(())
    }

    /// A shape's fill or stroke (`None`: nothing to paint): a color, or a linear or radial gradient
    /// (SVG 2 §14.2) in the shape's user space, `bbox` its geometry box.
    fn shape_paint(
        &self,
        n: NodeId,
        v: &ComputedValues,
        paint: &style::values::computed::SVGPaint,
        opacity: &style::values::computed::SVGOpacity,
        alpha: f32,
        bbox: kurbo::Rect,
    ) -> Result<Option<Paint>, Unsupported> {
        let SVGPaintKind::PaintServer(url) = &paint.kind else {
            return Ok(Self::paint_color(v, paint, opacity, alpha)?.map(Paint::Solid));
        };
        let o = match opacity {
            SVGOpacity::Opacity(o) => o.clamp(0.0, 1.0) * alpha,
            _ => return Err("context opacity"),
        };
        let id = url.url().and_then(|u| u.fragment()).ok_or("paint server")?;
        let q = self.q;
        let dom = q.dom();
        let target = dom
            .indexed_element_by_id(dom.root(n), id)
            .flatten()
            .ok_or("paint server")?;
        Gradient::of(q, n, target, o, bbox)
    }

    /// A text element: its characters (white space collapsed as SVG 2 §11.7.1 says) in runs, one
    /// per element and position, shaped with the element's font; each `x` or `y` starts a text
    /// chunk, anchored per `text-anchor` (§11.8.2).
    fn text(&mut self, text_el: NodeId) -> Result<(), Unsupported> {
        let q = self.q;
        let dom = q.dom();
        // The elements of the text, outer first.
        let mut elements = vec![text_el];
        for d in dom.descendants(text_el) {
            match dom.data(d) {
                NodeData::Element(e) if e.name().ns == ns!(svg) => match &*e.name().local {
                    "tspan" | "a" => elements.push(d),
                    "textPath" => return Err("textPath"),
                    _ => {}
                },
                NodeData::Element(_) => return Err("text content"),
                _ => {}
            }
        }
        for &el in &elements {
            for a in ["rotate", "textLength"] {
                if q.attr(el, a).is_some() {
                    return Err("text positioning");
                }
            }
            if el != text_el && q.values(el).is_some_and(|v| v.get_effects().opacity < 1.0) {
                return Err("tspan opacity");
            }
        }
        let mut a = Some(text_el);
        while let Some(e) = a {
            if q.values(e).is_some_and(decorated) {
                return Err("text decoration");
            }
            if crate::svg_geometry::is_outermost_svg(dom, e) {
                break;
            }
            a = dom.parent_element(e);
        }
        let chars = q.text_characters(text_el);
        if chars.is_empty() {
            return Ok(());
        }
        // Where each element's characters start, and its positions there.
        #[derive(Clone, Copy, Default)]
        struct Pos {
            x: Option<f32>,
            y: Option<f32>,
            dx: f32,
            dy: f32,
        }
        let mut pos: Vec<Pos> = vec![Pos::default(); chars.len()];
        for &el in &elements {
            let Some(first) = chars
                .iter()
                .position(|c| dom.is_inclusive_ancestor_of(el, c.0))
            else {
                continue;
            };
            let one = |name: &str, dir: LengthDir| -> Result<Option<f32>, Unsupported> {
                let list = q.length_list(el, name, dir);
                match list.len() {
                    0 => Ok(None),
                    1 => Ok(Some(list[0])),
                    _ => Err("text positioning"),
                }
            };
            let p = &mut pos[first];
            if let Some(x) = one("x", LengthDir::Horizontal)? {
                p.x = Some(x);
            }
            if let Some(y) = one("y", LengthDir::Vertical)? {
                p.y = Some(y);
            }
            if let Some(dx) = one("dx", LengthDir::Horizontal)? {
                p.dx = dx;
            }
            if let Some(dy) = one("dy", LengthDir::Vertical)? {
                p.dy = dy;
            }
        }
        // Checks per element whose characters these are, and their fonts.
        let mut owners: HashMap<NodeId, Option<Owner>> = HashMap::new();
        for &(o, _) in &chars {
            if owners.contains_key(&o) {
                continue;
            }
            let v = q.values(o).ok_or("unstyled")?;
            let svg = v.get_inherited_svg();
            if !matches!(svg.stroke.kind, SVGPaintKind::None) {
                return Err("text stroke");
            }
            if decorated(v) {
                return Err("text decoration");
            }
            for (name, ok) in [
                ("writing-mode", "horizontal-tb"),
                ("direction", "ltr"),
                ("letter-spacing", "normal"),
                ("word-spacing", "normal"),
                ("baseline-shift", "0px"),
            ] {
                if q.prop(o, name).is_some_and(|p| p != ok && p != "0px") {
                    return Err("text style");
                }
            }
            if q.prop(o, "dominant-baseline")
                .is_some_and(|p| p != "auto" && p != "alphabetic")
                || q.prop(o, "alignment-baseline")
                    .is_some_and(|p| p != "auto" && p != "baseline" && p != "alphabetic")
            {
                return Err("text baseline");
            }
            let visible = v.get_inherited_box().visibility
                == style::properties::longhands::visibility::computed_value::T::Visible;
            let color = if visible {
                Self::paint_color(v, &svg.fill, &svg.fill_opacity, 1.0)?
            } else {
                None
            };
            let owner = q.font_of(o).map(|(_, list, size)| Owner {
                list,
                size,
                color,
                style: RenderStyle::new(
                    q.style.as_ref().and_then(|s| s.data.styles_arc(o)).unwrap(),
                    q.style.as_ref().unwrap().env.clone(),
                ),
            });
            owners.insert(o, owner);
        }
        // Runs: a new one at each owner change and each position.
        let mut runs: Vec<Run> = Vec::new();
        let (mut cx, mut cy) = (0.0f32, 0.0f32);
        for (i, &(owner, ch)) in chars.iter().enumerate() {
            let p = pos[i];
            let moved = p.x.is_some() || p.y.is_some() || p.dx != 0.0 || p.dy != 0.0;
            if i == 0 || moved || runs.last().is_none_or(|r| r.owner != owner) {
                // (the previous run ends where its glyphs end)
                if let Some(r) = runs.last_mut() {
                    shape_run(self.text, &owners, r);
                    cx = r.x + r.width;
                    cy = r.y;
                }
                let chunk = i == 0 || p.x.is_some() || p.y.is_some();
                if let Some(x) = p.x {
                    cx = x;
                }
                if let Some(y) = p.y {
                    cy = y;
                }
                runs.push(Run {
                    owner,
                    text: String::new(),
                    x: cx + p.dx,
                    y: cy + p.dy,
                    shaped: ShapedText::default(),
                    width: 0.0,
                    chunk,
                });
            }
            runs.last_mut().unwrap().text.push(ch);
        }
        if let Some(r) = runs.last_mut() {
            shape_run(self.text, &owners, r);
        }
        // Anchoring per chunk.
        let mut start = 0;
        while start < runs.len() {
            let end = runs[start + 1..]
                .iter()
                .position(|r| r.chunk)
                .map_or(runs.len(), |p| start + 1 + p);
            let anchor = q
                .values(runs[start].owner)
                .map(|v| v.get_inherited_svg().text_anchor);
            use style::properties::longhands::text_anchor::computed_value::T as A;
            let total = runs[end - 1].x + runs[end - 1].width - runs[start].x;
            let shift = match anchor {
                Some(A::Middle) => -total / 2.0,
                Some(A::End) => -total,
                _ => 0.0,
            };
            for r in &mut runs[start..end] {
                r.x += shift;
            }
            start = end;
        }
        for r in &runs {
            let Some(Some(o)) = owners.get(&r.owner) else {
                continue;
            };
            let Some(color) = o.color else { continue };
            r.shaped.paint(
                0..r.shaped.clusters.len(),
                snowlark_paint::Point::new(r.x, r.y),
                color,
                &mut self.b,
            );
        }
        Ok(())
    }
}

/// The element whose characters a text run holds: its font and fill.
struct Owner {
    list: snowlark_paint::FontListId,
    size: f32,
    color: Option<Color>,
    style: RenderStyle,
}

/// A run of a text element's characters from one element at one position.
struct Run {
    owner: NodeId,
    text: String,
    x: f32,
    y: f32,
    shaped: ShapedText,
    width: f32,
    /// It starts a text chunk (an absolute `x` or `y`).
    chunk: bool,
}

/// Shapes a run with its owner's font (left-to-right, the owner's features and language).
fn shape_run(
    text: &snowlark_paint::TextEngine,
    owners: &HashMap<NodeId, Option<Owner>>,
    r: &mut Run,
) {
    let Some(Some(o)) = owners.get(&r.owner) else {
        return;
    };
    let features = o.style.font_features();
    let options = ShapeOptions {
        size: o.size,
        locale: o.style.locale(),
        direction: snowlark_paint::Direction::Ltr,
        features: &features,
        vertical: VerticalText::None,
        levels: None,
    };
    if text.shape(&r.text, o.list, &options, &mut r.shaped).is_ok() {
        r.width = r.shaped.width();
    }
}

/// Filters and masks, which are not recorded yet: as properties, and as attributes (the image path
/// reads these attributes as written, whatever the style engine made of them: `filter: url()` does
/// not parse in the style engine), with markers.
fn unsupported_effects(q: &SvgQuery, n: NodeId, v: &ComputedValues) -> Result<(), Unsupported> {
    if let Some(e) = q.dom().element(n) {
        for a in e.attrs() {
            let effect = matches!(
                &**a.local_name(),
                "filter" | "mask" | "marker-start" | "marker-mid" | "marker-end" | "marker"
            );
            if effect && *a.namespace() == ns!() && a.value().trim() != "none" {
                return Err("effect attribute");
            }
        }
    }
    if !v.get_effects().filter.0.is_empty() {
        return Err("filter");
    }
    if v.get_svg()
        .mask_image
        .0
        .iter()
        .any(|i| !matches!(i, style::values::computed::Image::None))
    {
        return Err("mask");
    }
    Ok(())
}

/// Whether a text decoration line is set on `n` (`text-decoration` is not inherited: it is drawn
/// across the descendants' text, CSS Text Decoration 3 §2).
fn decorated(v: &ComputedValues) -> bool {
    v.get_text().text_decoration_line != style::values::computed::TextDecorationLine::NONE
}

/// Whether a geometry property (SVG 2 §10.4, §10.2) is a `calc()`: [`SvgQuery::shape_path`] reads
/// their serializations as lengths, which `calc()` is not.
fn geometry_has_calc(v: &ComputedValues) -> bool {
    use style::values::computed::length_percentage::Unpacked;
    use style::values::computed::LengthPercentage;
    use style::values::generics::length::{GenericLengthPercentageOrAuto as LPAuto, GenericSize};
    let calc = |l: &LengthPercentage| matches!(l.unpack(), Unpacked::Calc(_));
    let svg = v.get_svg();
    let auto = |l: &LPAuto<style::values::computed::NonNegativeLengthPercentage>| match l {
        LPAuto::LengthPercentage(l) => calc(&l.0),
        LPAuto::Auto => false,
    };
    let pos = v.get_position();
    let size = |s: &style::values::computed::Size| match s {
        GenericSize::LengthPercentage(l) => calc(&l.0),
        _ => false,
    };
    calc(&svg.cx)
        || calc(&svg.cy)
        || calc(&svg.r.0)
        || auto(&svg.rx)
        || auto(&svg.ry)
        || calc(&svg.x)
        || calc(&svg.y)
        || size(&pos.width)
        || size(&pos.height)
}

/// A kurbo path as display list path elements (into `out`, cleared first).
/// A shape's fill or stroke.
enum Paint {
    Solid(Color),
    Gradient(Gradient),
}

/// A linear or radial gradient (SVG 2 §14.2) in its own space, which `to_user` maps to the user
/// space of the shape it paints (the units' box, then `gradientTransform`).
struct Gradient {
    kind: GradientKind,
    stops: smallvec::SmallVec<[snowlark_paint::GradientStop; 8]>,
    extend: snowlark_paint::Extend,
    to_user: Matrix,
}

enum GradientKind {
    /// The gradient vector.
    Linear(kurbo::Point, kurbo::Point),
    /// The end circle's center and radius (the focal point at its center).
    Radial(kurbo::Point, f64),
}

/// The most `href` links followed from a gradient.
const MAX_GRADIENT_CHAIN: usize = 16;

impl Gradient {
    /// The paint of the gradient element `target` for a shape `n` whose geometry box is `bbox`,
    /// its stops' opacity times `opacity`. Attributes and stops it does not have come from the
    /// gradients its `href` names (§14.2.3, §14.3.3: stops from the first with any; the
    /// coordinates only from gradients of its own kind). Zero stops paint nothing, one paints its
    /// color, as does a vector of zero length or a radius of zero (the last stop's color).
    fn of(
        q: &SvgQuery,
        n: NodeId,
        target: NodeId,
        opacity: f32,
        bbox: kurbo::Rect,
    ) -> Result<Option<Paint>, Unsupported> {
        let dom = q.dom();
        let name = |g: NodeId| -> &str {
            dom.element(g)
                .filter(|e| e.name().ns == ns!(svg))
                .map_or("", |e| &e.name().local)
        };
        let gradient = |g: NodeId| matches!(name(g), "linearGradient" | "radialGradient");
        if !gradient(target) {
            return Err("paint server");
        }
        let linear = name(target) == "linearGradient";
        let mut chain: smallvec::SmallVec<[NodeId; 4]> = smallvec::smallvec![target];
        let mut cur = target;
        while chain.len() < MAX_GRADIENT_CHAIN {
            let href = dom
                .attr(cur, &ns!(), &snowlark_dom::local_name!("href"))
                .or_else(|| dom.attr(cur, &ns!(xlink), &snowlark_dom::local_name!("href")));
            let Some(href) = href.map(str::trim) else {
                break;
            };
            let id = href.strip_prefix('#').ok_or("paint server href")?;
            let Some(next) = dom.indexed_element_by_id(dom.root(target), id).flatten() else {
                break;
            };
            if !gradient(next) || chain.contains(&next) {
                break;
            }
            chain.push(next);
            cur = next;
        }
        let own_kind = |g: &&NodeId| name(**g) == name(target);
        let attr = |a: &str| chain.iter().find_map(|&g| q.attr(g, a));
        let coordinate = |a: &str| chain.iter().filter(own_kind).find_map(|&g| q.attr(g, a));
        let obb = !matches!(attr("gradientUnits").map(str::trim), Some("userSpaceOnUse"));
        // (§14.2.2: a box of no width or height ignores the paint)
        if obb && (bbox.width() <= 0.0 || bbox.height() <= 0.0) {
            return Ok(None);
        }
        let len = |a: &str, initial: &str, dir: LengthDir| -> f64 {
            let l = coordinate(a)
                .and_then(|v| snowlark_layout::svg::Length::parse(v.trim()))
                .or_else(|| snowlark_layout::svg::Length::parse(initial))
                .unwrap_or(snowlark_layout::svg::Length::ZERO);
            if obb {
                // (fractions of the box: a percentage is one hundredth)
                match l.unit {
                    snowlark_layout::svg::LengthUnit::Percent => l.value as f64 / 100.0,
                    _ => l.value as f64,
                }
            } else {
                q.to_user(n, &l, dir) as f64
            }
        };
        // Stops (§14.4): offsets clamped to 0..1 and to at least the one before.
        let holder = chain
            .iter()
            .copied()
            .find(|&g| dom.children(g).any(|c| name(c) == "stop"));
        let mut stops: smallvec::SmallVec<[snowlark_paint::GradientStop; 8]> =
            smallvec::SmallVec::new();
        let mut last = 0.0f32;
        for s in holder
            .into_iter()
            .flat_map(|g| dom.children(g))
            .filter(|&c| name(c) == "stop")
        {
            let offset =
                q.attr(s, "offset")
                    .map(str::trim)
                    .map_or(0.0, |o| match o.strip_suffix('%') {
                        Some(p) => p.trim().parse::<f32>().map_or(0.0, |p| p / 100.0),
                        None => o.parse::<f32>().unwrap_or(0.0),
                    });
            let offset = if offset.is_finite() {
                offset.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let offset = offset.max(last);
            last = offset;
            let sv = q.values(s).ok_or("unstyled stop")?;
            let svg = sv.get_svg();
            let c = to_color(&sv.resolve_color(&svg.stop_color));
            let a = (c.a as f32 * svg.stop_opacity.clamp(0.0, 1.0) * opacity)
                .round()
                .clamp(0.0, 255.0) as u8;
            stops.push(snowlark_paint::GradientStop {
                offset,
                color: Color::rgba(c.r, c.g, c.b, a),
            });
        }
        let last_color = match stops.as_slice() {
            [] => return Ok(None),
            [only] => return Ok(Some(Paint::Solid(only.color))),
            [.., l] => l.color,
        };
        let kind = if linear {
            let p1 = kurbo::Point::new(
                len("x1", "0%", LengthDir::Horizontal),
                len("y1", "0%", LengthDir::Vertical),
            );
            let p2 = kurbo::Point::new(
                len("x2", "100%", LengthDir::Horizontal),
                len("y2", "0%", LengthDir::Vertical),
            );
            if p1 == p2 {
                return Ok(Some(Paint::Solid(last_color)));
            }
            GradientKind::Linear(p1, p2)
        } else {
            let c = kurbo::Point::new(
                len("cx", "50%", LengthDir::Horizontal),
                len("cy", "50%", LengthDir::Vertical),
            );
            let r = len("r", "50%", LengthDir::Other);
            let f = kurbo::Point::new(
                coordinate("fx").map_or(c.x, |_| len("fx", "50%", LengthDir::Horizontal)),
                coordinate("fy").map_or(c.y, |_| len("fy", "50%", LengthDir::Vertical)),
            );
            if f != c || len("fr", "0%", LengthDir::Other) != 0.0 {
                return Err("gradient focal point");
            }
            if r <= 0.0 {
                return Ok(Some(Paint::Solid(last_color)));
            }
            GradientKind::Radial(c, r)
        };
        let units = if obb {
            Matrix::translate((bbox.x0, bbox.y0))
                * Matrix::scale_non_uniform(bbox.width(), bbox.height())
        } else {
            Matrix::IDENTITY
        };
        let transform = chain
            .iter()
            .find(|&&g| q.attr(g, "gradientTransform").is_some())
            .map_or(Matrix::IDENTITY, |&g| {
                q.transform_attr(g, "gradientTransform")
            });
        let mut g = Gradient {
            kind,
            stops,
            extend: snowlark_paint::Extend::Clamp,
            to_user: units * transform,
        };
        g.straight_alpha();
        match attr("spreadMethod").map(str::trim) {
            Some("repeat") => g.repeat(),
            Some("reflect") => g.reflect(),
            _ => {}
        }
        Ok(Some(Paint::Gradient(g)))
    }

    /// Stops between two that differ in both color and alpha, so that the rasterizer's
    /// premultiplied interpolation follows SVG's, of the colors and alphas apart (§14.4,
    /// `color-interpolation: sRGB`).
    fn straight_alpha(&mut self) {
        const STEPS: usize = 8;
        let mut out: smallvec::SmallVec<[snowlark_paint::GradientStop; 8]> =
            smallvec::SmallVec::new();
        for (i, s) in self.stops.iter().enumerate() {
            if let Some(prev) = i.checked_sub(1).map(|p| self.stops[p]) {
                let (a, b) = (prev.color, s.color);
                let rgb_differs = (a.r, a.g, a.b) != (b.r, b.g, b.b);
                if a.a != b.a && rgb_differs && s.offset > prev.offset {
                    let mix =
                        |x: u8, y: u8, t: f32| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
                    for k in 1..STEPS {
                        let t = k as f32 / STEPS as f32;
                        out.push(snowlark_paint::GradientStop {
                            offset: prev.offset + (s.offset - prev.offset) * t,
                            color: Color::rgba(
                                mix(a.r, b.r, t),
                                mix(a.g, b.g, t),
                                mix(a.b, b.b, t),
                                mix(a.a, b.a, t),
                            ),
                        });
                    }
                }
            }
            out.push(*s);
        }
        self.stops = out;
    }

    /// `spreadMethod="repeat"`: the stops over the whole vector, 0 to 1, repeated (the display
    /// list repeats the first to last stop's span).
    fn repeat(&mut self) {
        let (first, last) = (self.stops[0], *self.stops.last().expect("stops"));
        if first.offset > 0.0 {
            self.stops.insert(
                0,
                snowlark_paint::GradientStop {
                    offset: 0.0,
                    ..first
                },
            );
        }
        if last.offset < 1.0 {
            self.stops.push(snowlark_paint::GradientStop {
                offset: 1.0,
                ..last
            });
        }
        self.extend = snowlark_paint::Extend::Repeat;
    }

    /// `spreadMethod="reflect"`: a vector twice as long whose second half is the first mirrored,
    /// repeated.
    fn reflect(&mut self) {
        let forth: smallvec::SmallVec<[snowlark_paint::GradientStop; 8]> = self
            .stops
            .iter()
            .map(|s| snowlark_paint::GradientStop {
                offset: s.offset / 2.0,
                ..*s
            })
            .collect();
        let back = self
            .stops
            .iter()
            .rev()
            .map(|s| snowlark_paint::GradientStop {
                offset: 1.0 - s.offset / 2.0,
                ..*s
            });
        self.stops = forth.into_iter().chain(back).collect();
        self.kind = match self.kind {
            GradientKind::Linear(p1, p2) => GradientKind::Linear(p1, p1 + (p2 - p1) * 2.0),
            GradientKind::Radial(c, r) => GradientKind::Radial(c, r * 2.0),
        };
        self.repeat();
    }

    /// Paints the gradient over `area` (user space), under whatever clip or mask is open.
    fn paint(&self, b: &mut DisplayListBuilder, area: kurbo::Rect) {
        let det = self.to_user.determinant();
        if det == 0.0 || !det.is_finite() {
            return;
        }
        // (the area in the gradient's space: the bounds of its corners mapped back)
        let inv = self.to_user.inverse();
        let corners = [
            kurbo::Point::new(area.x0, area.y0),
            kurbo::Point::new(area.x1, area.y0),
            kurbo::Point::new(area.x0, area.y1),
            kurbo::Point::new(area.x1, area.y1),
        ]
        .map(|p| inv * p);
        let local = corners
            .iter()
            .skip(1)
            .fold(kurbo::Rect::from_points(corners[0], corners[0]), |r, p| {
                r.union_pt(*p)
            });
        b.push_transform(paint_matrix(&self.to_user));
        let rect = rect_of(local);
        match self.kind {
            GradientKind::Linear(p1, p2) => b.linear_gradient(
                rect,
                snowlark_paint::Radii::default(),
                point(p1),
                point(p2),
                &self.stops,
                self.extend,
            ),
            GradientKind::Radial(c, r) => b.radial_gradient(
                rect,
                snowlark_paint::Radii::default(),
                point(c),
                snowlark_paint::Size::new(r as f32, r as f32),
                &self.stops,
                self.extend,
            ),
        }
        b.pop_transform();
    }
}

/// A kurbo rectangle as the display list's.
fn rect_of(r: kurbo::Rect) -> snowlark_paint::Rect {
    snowlark_paint::Rect::new(
        r.x0 as f32,
        r.y0 as f32,
        r.width() as f32,
        r.height() as f32,
    )
}

/// An element's clip path as recorded ([`Recorder::clip`]).
enum Clip {
    /// No `clip-path`.
    None,
    /// Clips everything away (no content, or none of it visible).
    All,
    /// A rectangle along the axes, in the element's user space.
    Rect(snowlark_paint::Rect),
    /// One shape's path and whether it fills by `evenodd`.
    Path(BezPath, bool),
    /// [`Clip::Path`] once added to the display list.
    Range(snowlark_paint::PathRange, bool),
    /// Several shapes, clipping as their union, and the union's bounds.
    Union(Vec<(BezPath, bool)>, snowlark_paint::Rect),
}

/// The rectangle a path is, when it is one closed rectangle along the axes (corners in order),
/// for the cheaper rectangle clip.
fn axis_aligned_rect(path: &BezPath) -> Option<kurbo::Rect> {
    let els = path.elements();
    let mut points: smallvec::SmallVec<[kurbo::Point; 6]> = smallvec::SmallVec::new();
    for (i, el) in els.iter().enumerate() {
        match *el {
            kurbo::PathEl::MoveTo(p) if i == 0 => points.push(p),
            kurbo::PathEl::LineTo(p) if i > 0 => points.push(p),
            kurbo::PathEl::ClosePath if i == els.len() - 1 => {}
            _ => return None,
        }
    }
    if points.len() == 5 && points[4] == points[0] {
        points.pop();
    }
    if points.len() != 4 {
        return None;
    }
    // (each side along an axis, alternating: a rectangle, not a bow tie)
    for i in 0..4 {
        let (a, b, c) = (points[i], points[(i + 1) % 4], points[(i + 2) % 4]);
        let first_horizontal = a.y == b.y && a.x != b.x;
        let first_vertical = a.x == b.x && a.y != b.y;
        let then_vertical = b.x == c.x && b.y != c.y;
        let then_horizontal = b.y == c.y && b.x != c.x;
        if !((first_horizontal && then_vertical) || (first_vertical && then_horizontal)) {
            return None;
        }
    }
    Some(kurbo::Rect::from_points(points[0], points[2]))
}

fn convert(path: &BezPath, out: &mut Vec<PathEl>) {
    out.clear();
    for el in path.elements() {
        out.push(match *el {
            kurbo::PathEl::MoveTo(p) => PathEl::MoveTo(point(p)),
            kurbo::PathEl::LineTo(p) => PathEl::LineTo(point(p)),
            kurbo::PathEl::QuadTo(a, b) => PathEl::QuadTo(point(a), point(b)),
            kurbo::PathEl::CurveTo(a, b, c) => PathEl::CurveTo(point(a), point(b), point(c)),
            kurbo::PathEl::ClosePath => PathEl::Close,
        });
    }
}
