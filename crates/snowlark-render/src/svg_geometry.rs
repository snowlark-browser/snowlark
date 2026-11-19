// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The geometry of inline SVG content for the SVG DOM and hit testing (SVG 2 §8 coordinate systems,
//! §9 and §10 the equivalent paths of `path` and the basic shapes, §4.4.2 bounding boxes and
//! CTMs): element transforms, SVG viewports, the user units of lengths, shape geometry, a simple
//! model of `text` boxes, and which element a point hits per `pointer-events` (§16.6).
//!
//! Inline SVG is painted as an image ([`crate::inline_svg`]); this module computes the same
//! geometry from the DOM and the computed styles directly, with the outermost `svg` element's
//! content box from layout.

use kurbo::{BezPath, Point, Rect};
use smallvec::SmallVec;
use snowlark_dom::{ns, Dom, NodeId};
use snowlark_layout::svg::{
    geom, transform as tf, AspectRatio, Length, LengthContext, LengthDir, LengthUnit, Matrix,
    PathData, StrokeStyle, ViewBox,
};
use snowlark_layout::Style as _;
use snowlark_paint::{FontQuery, TextEngine};
use snowlark_style::{ComputedValues, StyleData};
use std::cell::Ref;
use style::values::specified::{DProperty, SVGPathData};

use crate::adapter::RenderStyle;
use crate::renderer::Renderer;

/// What `getBBox` includes (SVG 2 §4.4.2 `SVGBoundingBoxOptions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BBoxOptions {
    pub fill: bool,
    pub stroke: bool,
    pub markers: bool,
    pub clipped: bool,
}

impl Default for BBoxOptions {
    fn default() -> BBoxOptions {
        BBoxOptions {
            fill: true,
            stroke: false,
            markers: false,
            clipped: false,
        }
    }
}

/// The user's preferred languages, most preferred first (`navigator.languages`, SVG 2 §5.6
/// `systemLanguage`).
pub const USER_LANGUAGES: &[&str] = &["en-US", "en"];

/// The SVG geometry for one query (the DOM borrowed for its duration), with the renderer's styles
/// when the document is rendered, else from attributes alone.
pub struct SvgQuery<'a> {
    pub(crate) dom: DomRef<'a>,
    pub(crate) style: Option<Styled<'a>>,
    /// Supplemental transforms of `animateMotion` (SVG Animations §3.8), by element.
    pub motion: std::collections::HashMap<NodeId, Matrix>,
}

/// The document a query looks at: the page's, or a resource document of a `use`.
pub(crate) enum DomRef<'a> {
    Page(Ref<'a, Dom>),
    Resource(&'a Dom),
}

impl std::ops::Deref for DomRef<'_> {
    type Target = Dom;
    fn deref(&self) -> &Dom {
        match self {
            DomRef::Page(d) => d,
            DomRef::Resource(d) => d,
        }
    }
}

/// What a rendered document adds: computed styles, fonts, layout.
pub(crate) struct Styled<'a> {
    pub data: &'a StyleData,
    pub env: &'a std::sync::Arc<crate::env::StyleEnv>,
    pub text: &'a TextEngine,
    pub renderer: &'a Renderer,
    /// The initial containing block.
    pub icb: (f32, f32),
}

fn is_svg(dom: &Dom, n: NodeId) -> bool {
    dom.element(n).is_some_and(|e| e.name().ns == ns!(svg))
}

fn local(dom: &Dom, n: NodeId) -> &str {
    dom.element(n).map_or("", |e| &e.name().local)
}

/// The parent element for SVG's purposes (a shadow root's host).
fn svg_parent(dom: &Dom, n: NodeId) -> Option<NodeId> {
    let p = dom.parent(n)?;
    if dom.is_shadow_root(p) {
        return dom.host(p);
    }
    dom.is_element(p).then_some(p)
}

/// Whether `n` is an outermost `svg` element (an `svg` whose parent is not an SVG element).
pub fn is_outermost_svg(dom: &Dom, n: NodeId) -> bool {
    is_svg(dom, n) && local(dom, n) == "svg" && !svg_parent(dom, n).is_some_and(|p| is_svg(dom, p))
}

/// Elements that establish an SVG viewport (SVG 2 §8.1).
fn is_viewport_element(dom: &Dom, n: NodeId) -> bool {
    is_svg(dom, n)
        && (matches!(local(dom, n), "svg" | "image" | "foreignObject")
            || (local(dom, n) == "symbol"
                && svg_parent(dom, n).is_some_and(|p| local(dom, p) == "use")))
}

/// Elements whose children are rendered (SVG 2 §5.1 "container elements" that render).
fn is_rendered_container(name: &str) -> bool {
    matches!(name, "svg" | "g" | "a" | "switch" | "use")
}

/// Elements never rendered directly (SVG 2 §5.1 "never-rendered elements").
fn never_rendered(name: &str) -> bool {
    matches!(
        name,
        "clipPath"
            | "defs"
            | "linearGradient"
            | "marker"
            | "mask"
            | "pattern"
            | "radialGradient"
            | "script"
            | "style"
            | "symbol"
            | "title"
            | "desc"
            | "metadata"
            | "filter"
    )
}

impl Renderer {
    /// The SVG geometry of the current document (after a restyle; layout for the outermost `svg`
    /// boxes when it ran). `None` without a document.
    pub fn svg_query(&self) -> Option<SvgQuery<'_>> {
        let (data, env, last_viewport) = self.svg_doc()?;
        let viewport = last_viewport
            .map(|s| (s.width, s.height))
            .unwrap_or(self.device_config().viewport);
        Some(SvgQuery {
            dom: DomRef::Page(self.dom()?),
            style: Some(Styled {
                data,
                env,
                text: self.text(),
                renderer: self,
                icb: viewport,
            }),
            motion: Default::default(),
        })
    }
}

impl<'a> SvgQuery<'a> {
    /// The geometry of a document that is not rendered: attributes and initial values only.
    pub fn unstyled(dom: Ref<'a, Dom>) -> SvgQuery<'a> {
        SvgQuery {
            dom: DomRef::Page(dom),
            style: None,
            motion: Default::default(),
        }
    }

    /// A query over a resource document, from its attributes.
    fn of_resource(dom: &'a Dom) -> SvgQuery<'a> {
        SvgQuery {
            dom: DomRef::Resource(dom),
            style: None,
            motion: Default::default(),
        }
    }
}

impl SvgQuery<'_> {
    pub fn dom(&self) -> &Dom {
        &self.dom
    }

    pub(crate) fn values(&self, n: NodeId) -> Option<&ComputedValues> {
        self.style.as_ref()?.data.styles(n)
    }

    /// A property's computed value (unrendered: its presentation attribute, if any).
    pub(crate) fn prop(&self, n: NodeId, name: &str) -> Option<String> {
        if self.style.is_none() {
            return self.attr(n, name).map(str::to_owned);
        }
        snowlark_style::computed::computed_property_by_name(self.values(n)?, name)
    }

    pub(crate) fn attr(&self, n: NodeId, name: &str) -> Option<&str> {
        if !self.dom.is_element(n) {
            return None;
        }
        self.dom.presented_attr(n, &name.into())
    }

    /// The text engine, the font list and the font size of an element's style.
    pub(crate) fn font_of(
        &self,
        n: NodeId,
    ) -> Option<(&TextEngine, snowlark_paint::FontListId, f32)> {
        let st = self.style.as_ref()?;
        let values = st.data.styles_arc(n)?;
        let s = RenderStyle::new(values, st.env.clone());
        let families: SmallVec<[&str; 8]> = s.font_family().collect();
        let query = FontQuery {
            families: &families,
            weight: s.font_weight(),
            style: s.font_style(),
            stretch: s.font_stretch(),
        };
        let size = s.font_size();
        let list = st.text.font_list(&query).ok()?;
        Some((st.text, list, size))
    }

    /// The context an element's lengths resolve in (SVG 2 §4.5.2, §8.9).
    pub fn length_context(&self, n: NodeId) -> LengthContext {
        self.context_for(n, Needs::ALL)
    }

    /// The context lengths of `n` in these units resolve in, with only what the units read: the
    /// first available font's metrics for `ex` and `cap`, its `0` and `水` advances (a shaping
    /// each) for `ch` and `ic`, the viewport for percentages. User units and `em`, nearly every
    /// length in an SVG, need at most the font size. (Blink, too, reads a font metric only for a
    /// length expressed in it.)
    pub fn length_context_for<'u>(
        &self,
        n: NodeId,
        units: impl IntoIterator<Item = &'u LengthUnit>,
    ) -> LengthContext {
        let needs = units
            .into_iter()
            .fold(Needs::default(), |a, u| Needs(a.0 | Needs::of(u).0));
        self.context_for(n, needs)
    }

    fn context_for(&self, n: NodeId, needs: Needs) -> LengthContext {
        let mut ctx = LengthContext {
            icb: self.style.as_ref().map(|s| s.icb),
            ..LengthContext::default()
        };
        if needs.has(Needs::VIEWPORT) {
            ctx.viewport = self.viewport_of(n);
        }
        if needs.has(Needs::FONT) {
            if let Some((text, list, size)) = self.font_of(n) {
                ctx.font_size = Some(size);
                if needs.has(Needs::METRICS) {
                    let m = text.metrics(text.primary_font(list), size);
                    ctx.x_height = (m.x_height > 0.0).then_some(m.x_height);
                    ctx.cap_height = (m.cap_height > 0.0).then_some(m.cap_height);
                }
                if needs.has(Needs::CH) {
                    ctx.ch = Some(text.measure("0", list, size).unwrap_or(size * 0.5));
                }
                if needs.has(Needs::IC) {
                    ctx.ic = Some(text.measure("\u{6C34}", list, size).unwrap_or(size));
                }
            }
        }
        if needs.has(Needs::LINE_HEIGHT) {
            if let Some(v) = self.values(n) {
                ctx.line_height = line_height(v);
            }
        }
        if needs.has(Needs::ROOT) {
            if let Some(root) = self.dom.document_element(self.dom.node_document(n)) {
                if let Some(v) = self.values(root) {
                    ctx.root_font_size = Some(v.get_font().font_size.computed_size().px());
                    ctx.root_line_height = line_height(v);
                }
            }
        }
        ctx
    }

    /// A length attribute in user units (`initial` when absent or invalid).
    fn length(&self, n: NodeId, name: &str, dir: LengthDir, initial: &str) -> f32 {
        let l = self
            .geometry(n, name)
            .or_else(|| Length::parse(initial))
            .unwrap_or(Length::ZERO);
        self.to_user(n, &l, dir)
    }

    /// A length of `n` in user units, with no more of its context than the unit needs.
    pub(crate) fn to_user(&self, n: NodeId, l: &Length, dir: LengthDir) -> f32 {
        if l.unit.is_absolute() {
            return l.to_user(dir, &LengthContext::default());
        }
        l.to_user(dir, &self.length_context_for(n, [&l.unit]))
    }

    /// A length attribute of `n`: the computed geometry property it is the presentation attribute
    /// of (SVG 2 §6.6; `None` for `auto`), else the attribute.
    fn geometry(&self, n: NodeId, name: &str) -> Option<Length> {
        if self.style.is_some() {
            let outermost = is_outermost_svg(&self.dom, n);
            let element = local(&self.dom, n);
            // (svgwg#1059: `width` and `height` of `use` and `symbol` are attributes)
            let attribute =
                matches!(element, "use" | "symbol") && matches!(name, "width" | "height");
            if !attribute
                && snowlark_style::svg_presentation_property(element, outermost, name).is_some()
            {
                let computed = self
                    .values(n)
                    .and_then(|c| snowlark_style::computed::computed_property_by_name(c, name));
                if let Some(v) = computed {
                    if v == "auto" {
                        return None;
                    }
                    if let Some(l) = Length::parse(&v) {
                        return Some(l);
                    }
                }
            }
        }
        self.attr(n, name).and_then(Length::parse)
    }

    /// The size of the SVG viewport `n`'s percentages resolve against: the nearest ancestor viewport
    /// element's.
    pub fn viewport_of(&self, n: NodeId) -> Option<(f32, f32)> {
        let mut cur = svg_parent(&self.dom, n);
        while let Some(p) = cur {
            if !is_svg(&self.dom, p) {
                return None;
            }
            if is_viewport_element(&self.dom, p) {
                return self.inner_viewport(p);
            }
            cur = svg_parent(&self.dom, p);
        }
        None
    }

    /// The size of the viewport `v` establishes for its content: its `viewBox`'s when it has one,
    /// else its own size.
    pub(crate) fn inner_viewport(&self, v: NodeId) -> Option<(f32, f32)> {
        if let Some(vb) = self.attr(v, "viewBox").and_then(ViewBox::parse) {
            return Some((vb.width as f32, vb.height as f32));
        }
        self.viewport_size(v)
    }

    /// The size of the viewport element `v` itself.
    pub fn viewport_size(&self, v: NodeId) -> Option<(f32, f32)> {
        if is_outermost_svg(&self.dom, v) {
            if let Some(g) = self.style.as_ref().and_then(|s| s.renderer.geometry(v)) {
                return Some((g.content_box.width, g.content_box.height));
            }
            // (not laid out: its width and height attributes, as a detached `svg` would have)
            let ctx = LengthContext::default();
            let w = self
                .attr(v, "width")
                .and_then(Length::parse)
                .map_or(300.0, |l| l.to_user(LengthDir::Horizontal, &ctx));
            let h = self
                .attr(v, "height")
                .and_then(Length::parse)
                .map_or(150.0, |l| l.to_user(LengthDir::Vertical, &ctx));
            return Some((w, h));
        }
        let initial = if local(&self.dom, v) == "svg" {
            "100%"
        } else {
            "0"
        };
        Some((
            self.length(v, "width", LengthDir::Horizontal, initial),
            self.length(v, "height", LengthDir::Vertical, initial),
        ))
    }

    /// `n`'s `transform` (SVG 2 §8.5): the computed property, whatever set it (the presentation
    /// attribute, a style rule, an animation), about its `transform-origin` in the nearest viewport
    /// (CSS Transforms 1 §3: the `view-box` reference box); the attribute where there is no style.
    /// An outermost `svg` element's attribute is not a presentation attribute.
    fn own_transform(&self, n: NodeId) -> Matrix {
        let Some(st) = self
            .style
            .as_ref()
            .filter(|_| !is_outermost_svg(&self.dom, n))
        else {
            return self.transform_attr(n, "transform");
        };
        let Some(values) = st.data.styles_arc(n) else {
            return self.transform_attr(n, "transform");
        };
        let s = RenderStyle::new(values, st.env.clone());
        if !s.has_transform() {
            return Matrix::IDENTITY;
        }
        use crate::adapter::{svg_reference_box, SvgReferenceBox};
        let kind = st
            .data
            .styles(n)
            .map_or(SvgReferenceBox::View, svg_reference_box);
        let options = BBoxOptions {
            stroke: kind == SvgReferenceBox::Stroke,
            ..BBoxOptions::default()
        };
        let reference = match kind {
            SvgReferenceBox::View => None,
            _ => self.bbox(n, options),
        };
        let reference = match reference {
            Some(b) => snowlark_paint::Rect::new(
                b.x0 as f32,
                b.y0 as f32,
                b.width() as f32,
                b.height() as f32,
            ),
            None => {
                let (width, height) = self.viewport_of(n).unwrap_or((0.0, 0.0));
                snowlark_paint::Rect::new(0.0, 0.0, width, height)
            }
        };
        s.transform(reference).map_or(Matrix::IDENTITY, |m| {
            Matrix::new([
                m.m11 as f64,
                m.m12 as f64,
                m.m21 as f64,
                m.m22 as f64,
                m.dx as f64,
                m.dy as f64,
            ])
        })
    }

    /// The transform list attribute `name`'s matrix.
    pub(crate) fn transform_attr(&self, n: NodeId, name: &str) -> Matrix {
        self.attr(n, name)
            .and_then(tf::parse_transform_list)
            .map_or(Matrix::IDENTITY, |l| tf::list_matrix(&l))
    }

    /// The `viewBox` transform of a viewport element of size `w` × `h`.
    fn view_box_transform(&self, n: NodeId, w: f64, h: f64) -> Matrix {
        let Some(vb) = self.attr(n, "viewBox").and_then(ViewBox::parse) else {
            return Matrix::IDENTITY;
        };
        let par = self
            .attr(n, "preserveAspectRatio")
            .and_then(AspectRatio::parse)
            .unwrap_or_default();
        par.transform(&vb, w, h)
    }

    /// The transform from `n`'s user space to its parent's (SVG 2 §8.3; for an outermost `svg`, to
    /// its content box).
    pub fn local_transform(&self, n: NodeId) -> Matrix {
        let name = local(&self.dom, n);
        let own = self.own_transform(n);
        // (animateMotion's transform applies on top of the element's own)
        let own = match self.motion.get(&n) {
            Some(m) => *m * own,
            None => own,
        };
        match name {
            "svg" => {
                let (w, h) = self.viewport_size(n).unwrap_or((0.0, 0.0));
                let vb = self.view_box_transform(n, w as f64, h as f64);
                if is_outermost_svg(&self.dom, n) {
                    own * vb
                } else {
                    let x = self.length(n, "x", LengthDir::Horizontal, "0") as f64;
                    let y = self.length(n, "y", LengthDir::Vertical, "0") as f64;
                    own * Matrix::translate((x, y)) * vb
                }
            }
            "use" => {
                let x = self.length(n, "x", LengthDir::Horizontal, "0") as f64;
                let y = self.length(n, "y", LengthDir::Vertical, "0") as f64;
                own * Matrix::translate((x, y))
            }
            _ => own,
        }
    }

    /// SVG 2 §4.4.2 `getCTM()` (`screen`: `getScreenCTM()`, up to the outermost `svg` element's
    /// content box, whose position the caller adds).
    pub fn ctm(&self, n: NodeId, screen: bool) -> Matrix {
        let mut m = Matrix::IDENTITY;
        let mut cur = Some(n);
        while let Some(c) = cur {
            if !is_svg(&self.dom, c) {
                break;
            }
            m = self.local_transform(c) * m;
            if !screen && c != n && is_viewport_element(&self.dom, c) {
                break;
            }
            cur = svg_parent(&self.dom, c);
        }
        m
    }

    /// The outermost `svg` element holding `n`.
    pub fn outermost_svg(&self, n: NodeId) -> Option<NodeId> {
        let mut cur = Some(n);
        while let Some(c) = cur {
            if !is_svg(&self.dom, c) {
                return None;
            }
            if is_outermost_svg(&self.dom, c) {
                return Some(c);
            }
            cur = svg_parent(&self.dom, c);
        }
        None
    }

    // ----- shapes ------------------------------------------------------------------------------------

    /// The equivalent path of a `path` or basic shape (SVG 2 §9, §10) in its user space; `None` for
    /// other elements.
    pub fn shape_path(&self, n: NodeId) -> Option<BezPath> {
        let name = local(&self.dom, n);
        let len = |a: &str, d: LengthDir| self.length(n, a, d, "0") as f64;
        let auto = |a: &str, d: LengthDir| -> Option<f64> {
            self.geometry(n, a).map(|l| self.to_user(n, &l, d) as f64)
        };
        Some(match name {
            // (the `d` property's value: the path data the style parsed once from the attribute
            // or a rule, else the attribute's, which keeps what precedes an error)
            "path" => match self.values(n).map(|v| &v.get_svg().d) {
                Some(DProperty::Path(d)) => path_data_of(d).to_bezpath(),
                _ => self
                    .attr(n, "d")
                    .map_or_else(BezPath::new, |d| PathData::parse(d).to_bezpath()),
            },
            "rect" => {
                let (x, y) = (
                    len("x", LengthDir::Horizontal),
                    len("y", LengthDir::Vertical),
                );
                let (w, h) = (
                    len("width", LengthDir::Horizontal),
                    len("height", LengthDir::Vertical),
                );
                let mut p = BezPath::new();
                if w <= 0.0 || h <= 0.0 {
                    return Some(p);
                }
                // §10.2: an auto rx or ry takes the other; both clamp to half the size.
                let (rx, ry) = match (
                    auto("rx", LengthDir::Horizontal).filter(|v| *v >= 0.0),
                    auto("ry", LengthDir::Vertical).filter(|v| *v >= 0.0),
                ) {
                    (None, None) => (0.0, 0.0),
                    (Some(a), None) => (a, a),
                    (None, Some(b)) => (b, b),
                    (Some(a), Some(b)) => (a, b),
                };
                let (rx, ry) = (rx.min(w / 2.0), ry.min(h / 2.0));
                if rx > 0.0 && ry > 0.0 {
                    p.move_to((x + rx, y));
                    p.line_to((x + w - rx, y));
                    snowlark_layout::svg::path::arc_to(
                        &mut p,
                        Point::new(x + w - rx, y),
                        Point::new(x + w, y + ry),
                        rx,
                        ry,
                        0.0,
                        false,
                        true,
                    );
                    p.line_to((x + w, y + h - ry));
                    snowlark_layout::svg::path::arc_to(
                        &mut p,
                        Point::new(x + w, y + h - ry),
                        Point::new(x + w - rx, y + h),
                        rx,
                        ry,
                        0.0,
                        false,
                        true,
                    );
                    p.line_to((x + rx, y + h));
                    snowlark_layout::svg::path::arc_to(
                        &mut p,
                        Point::new(x + rx, y + h),
                        Point::new(x, y + h - ry),
                        rx,
                        ry,
                        0.0,
                        false,
                        true,
                    );
                    p.line_to((x, y + ry));
                    snowlark_layout::svg::path::arc_to(
                        &mut p,
                        Point::new(x, y + ry),
                        Point::new(x + rx, y),
                        rx,
                        ry,
                        0.0,
                        false,
                        true,
                    );
                    p.close_path();
                } else {
                    p.move_to((x, y));
                    p.line_to((x + w, y));
                    p.line_to((x + w, y + h));
                    p.line_to((x, y + h));
                    p.close_path();
                }
                p
            }
            "circle" | "ellipse" => {
                let (cx, cy) = (
                    len("cx", LengthDir::Horizontal),
                    len("cy", LengthDir::Vertical),
                );
                let (rx, ry) = if name == "circle" {
                    let r = len("r", LengthDir::Other);
                    (r, r)
                } else {
                    let rx = auto("rx", LengthDir::Horizontal).filter(|v| *v >= 0.0);
                    let ry = auto("ry", LengthDir::Vertical).filter(|v| *v >= 0.0);
                    match (rx, ry) {
                        (Some(a), Some(b)) => (a, b),
                        (Some(a), None) => (a, a),
                        (None, Some(b)) => (b, b),
                        (None, None) => (0.0, 0.0),
                    }
                };
                let mut p = BezPath::new();
                if rx <= 0.0 || ry <= 0.0 {
                    return Some(p);
                }
                // (§10.3/§10.4: starting at (cx + rx, cy), clockwise)
                p.move_to((cx + rx, cy));
                for (from, to) in [
                    ((cx + rx, cy), (cx, cy + ry)),
                    ((cx, cy + ry), (cx - rx, cy)),
                    ((cx - rx, cy), (cx, cy - ry)),
                    ((cx, cy - ry), (cx + rx, cy)),
                ] {
                    snowlark_layout::svg::path::arc_to(
                        &mut p,
                        from.into(),
                        to.into(),
                        rx,
                        ry,
                        0.0,
                        false,
                        true,
                    );
                }
                p.close_path();
                p
            }
            "line" => {
                let mut p = BezPath::new();
                p.move_to((
                    len("x1", LengthDir::Horizontal),
                    len("y1", LengthDir::Vertical),
                ));
                p.line_to((
                    len("x2", LengthDir::Horizontal),
                    len("y2", LengthDir::Vertical),
                ));
                p
            }
            "polyline" | "polygon" => {
                let pts = self
                    .attr(n, "points")
                    .and_then(snowlark_layout::svg::parse_points)
                    .unwrap_or_default();
                let mut p = BezPath::new();
                for (i, (x, y)) in pts.iter().enumerate() {
                    if i == 0 {
                        p.move_to((*x, *y));
                    } else {
                        p.line_to((*x, *y));
                    }
                }
                if name == "polygon" && !pts.is_empty() {
                    p.close_path();
                }
                p
            }
            _ => return None,
        })
    }

    /// The stroke properties of an element in its user units.
    pub fn stroke_style(&self, n: NodeId) -> StrokeStyle {
        let len = |v: &str| -> f64 {
            Length::parse(v.trim()).map_or(0.0, |l| self.to_user(n, &l, LengthDir::Other) as f64)
        };
        let width = self.prop(n, "stroke-width").map_or(1.0, |v| len(&v));
        let cap = match self.prop(n, "stroke-linecap").as_deref() {
            Some("round") => kurbo::Cap::Round,
            Some("square") => kurbo::Cap::Square,
            _ => kurbo::Cap::Butt,
        };
        let join = match self.prop(n, "stroke-linejoin").as_deref() {
            Some("round") => kurbo::Join::Round,
            Some("bevel") => kurbo::Join::Bevel,
            _ => kurbo::Join::Miter,
        };
        let miter_limit = self
            .prop(n, "stroke-miterlimit")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(4.0);
        let dashes = match self.prop(n, "stroke-dasharray") {
            Some(v) if v != "none" => v
                .split(|c: char| c == ',' || c.is_ascii_whitespace())
                .filter(|t| !t.is_empty())
                .map(len)
                .collect(),
            _ => Vec::new(),
        };
        let dash_offset = self.prop(n, "stroke-dashoffset").map_or(0.0, |v| len(&v));
        StrokeStyle {
            width,
            cap,
            join,
            miter_limit,
            dashes,
            dash_offset,
        }
    }

    /// Whether the element's `fill-rule` is `evenodd`.
    pub fn fill_evenodd(&self, n: NodeId) -> bool {
        self.prop(n, "fill-rule").as_deref() == Some("evenodd")
    }

    // ----- text: see crate::svg_text --------------------------------------------------------------

    // ----- bounding boxes ----------------------------------------------------------------------------

    /// Whether an element is rendered at all (`display` not `none` on it and its ancestors in the
    /// SVG fragment).
    fn displayed(&self, n: NodeId) -> bool {
        if self.style.is_none() {
            return self.attr(n, "display") != Some("none");
        }
        match self.values(n) {
            Some(v) => !v.get_box().display.is_none(),
            None => false,
        }
    }

    /// Whether `n` or an ancestor in its SVG fragment is a never-rendered element (`defs`,
    /// `clipPath`, `mask`, `marker`, `pattern`, gradients, `filter`, `symbol`...): no layout box.
    pub fn in_never_rendered(&self, n: NodeId) -> bool {
        let mut cur = Some(n);
        while let Some(c) = cur {
            if !is_svg(&self.dom, c) {
                return false;
            }
            if never_rendered(local(&self.dom, c)) {
                return true;
            }
            cur = svg_parent(&self.dom, c);
        }
        false
    }

    /// SVG 2 §5.6 conditional processing: whether `systemLanguage` and `requiredExtensions`
    /// evaluate to true (the user's languages are [`USER_LANGUAGES`]; no extension is supported but
    /// HTML and MathML, as `foreignObject` content).
    pub fn passes_conditions(&self, n: NodeId) -> bool {
        if let Some(v) = self.attr(n, "systemLanguage") {
            let matches = v.split(',').map(str::trim).any(|tag| {
                USER_LANGUAGES.iter().any(|r| {
                    tag.eq_ignore_ascii_case(r)
                        || (tag.len() > r.len()
                            && tag.as_bytes()[r.len()] == b'-'
                            && tag[..r.len()].eq_ignore_ascii_case(r))
                })
            });
            if !matches {
                return false;
            }
        }
        if let Some(v) = self.attr(n, "requiredExtensions") {
            let tokens: Vec<&str> = v.split_ascii_whitespace().collect();
            let supported = |t: &&str| {
                matches!(
                    *t,
                    "http://www.w3.org/1999/xhtml" | "http://www.w3.org/1998/Math/MathML"
                )
            };
            if tokens.is_empty() || !tokens.iter().all(supported) {
                return false;
            }
        }
        true
    }

    /// Whether a child of a `switch` element is the one it renders (SVG 2 §5.7: the first direct
    /// child whose conditions are true); true for children of other elements.
    fn switch_chooses(&self, n: NodeId) -> bool {
        let Some(p) = svg_parent(&self.dom, n) else {
            return true;
        };
        if !is_svg(&self.dom, p) || local(&self.dom, p) != "switch" {
            return true;
        }
        let chosen = self.dom.children(p).find(|&c| {
            is_svg(&self.dom, c)
                && !never_rendered(local(&self.dom, c))
                && self.displayed(c)
                && self.passes_conditions(c)
        });
        chosen == Some(n)
    }

    /// SVG 2 §8.10 the bounding box of `n` in its user space (`None`: nothing to bound).
    pub fn bbox(&self, n: NodeId, opts: BBoxOptions) -> Option<Rect> {
        if !self.is_rendered(n) {
            return None;
        }
        self.bbox_in(n, Matrix::IDENTITY, opts)
    }

    /// Whether an element is rendered: displayed, as are its ancestors in the SVG fragment, and
    /// not in a `defs` or an uninstantiated `symbol` (SVG 2 §5.1 "never-rendered elements").
    pub fn is_rendered(&self, n: NodeId) -> bool {
        let mut cur = Some(n);
        while let Some(c) = cur {
            if !is_svg(&self.dom, c) {
                return true;
            }
            if !self.displayed(c)
                || matches!(local(&self.dom, c), "defs" | "symbol")
                || !self.passes_conditions(c)
                || !self.switch_chooses(c)
            {
                return false;
            }
            if is_outermost_svg(&self.dom, c) {
                return true;
            }
            cur = svg_parent(&self.dom, c);
        }
        true
    }

    /// The bounding box of `n`'s geometry transformed by `m` (from its user space).
    fn bbox_in(&self, n: NodeId, m: Matrix, opts: BBoxOptions) -> Option<Rect> {
        let name = local(&self.dom, n).to_owned();
        if let Some(path) = self.shape_path(n) {
            let mut r = if opts.fill {
                geom::bounds(&path, m)
            } else {
                None
            };
            if opts.stroke && self.prop(n, "stroke").as_deref() != Some("none") {
                let style = self.stroke_style(n);
                let outline = style.outline(&path);
                r = geom::union(r, geom::bounds(&outline, m));
                r = geom::union(r, geom::zero_length_caps(&path, &style, m));
            }
            return r;
        }
        match name.as_str() {
            "image" | "foreignObject" => {
                let x = self.length(n, "x", LengthDir::Horizontal, "0") as f64;
                let y = self.length(n, "y", LengthDir::Vertical, "0") as f64;
                let w = self.length(n, "width", LengthDir::Horizontal, "0") as f64;
                let h = self.length(n, "height", LengthDir::Vertical, "0") as f64;
                // (a zero or negative size is not rendered)
                if w <= 0.0 || h <= 0.0 {
                    return None;
                }
                Some(geom::transform_rect(Rect::new(x, y, x + w, y + h), m))
            }
            "text" | "tspan" | "textPath" => self.text_bbox(n, m),
            "use" => match self.use_target(n) {
                Some(target) => {
                    let tm = self.local_transform(target);
                    self.bbox_in(target, m * tm, opts)
                }
                None => {
                    let (r, fragment) = self.use_resource(n)?;
                    let target = r.target(fragment.as_deref())?;
                    let q = SvgQuery::of_resource(&r.dom);
                    q.bbox_in(target, m * q.local_transform(target), opts)
                }
            },
            _ => {
                // A container: the union of its rendered children's boxes in its user space.
                let mut r = None;
                for c in self.dom.children(n) {
                    if !is_svg(&self.dom, c)
                        || !self.displayed(c)
                        || !self.passes_conditions(c)
                        || !self.switch_chooses(c)
                    {
                        continue;
                    }
                    let cname = local(&self.dom, c);
                    if never_rendered(cname) {
                        continue;
                    }
                    let cm = m * self.local_transform(c);
                    r = geom::union(r, self.bbox_in(c, cm, opts));
                }
                r
            }
        }
    }

    /// The resource document a `use` element references.
    fn use_resource(
        &self,
        n: NodeId,
    ) -> Option<(
        std::rc::Rc<crate::svg_resource::ResourceDocument>,
        Option<String>,
    )> {
        self.style.as_ref()?.renderer.svg_resource_of(&self.dom, n)
    }

    /// The element a `use` element shows: the clone in its shadow tree, else what it references in
    /// its document (`href` then `xlink:href`).
    pub fn use_target(&self, n: NodeId) -> Option<NodeId> {
        if let Some(shadow) = self.dom.shadow_root(n) {
            return self.dom.children(shadow).find(|&c| self.dom.is_element(c));
        }
        let href = self
            .dom
            .attr(n, &ns!(), &"href".into())
            .or_else(|| self.dom.attr(n, &ns!(xlink), &"href".into()))?;
        let id = href.trim().strip_prefix('#')?;
        let root = self.dom.root(n);
        self.dom
            .descendants(root)
            .find(|&d| self.dom.is_element(d) && self.dom.attr(d, &ns!(), &"id".into()) == Some(id))
            .filter(|&t| !self.dom.is_inclusive_ancestor_of(t, n))
    }

    // ----- hit testing -------------------------------------------------------------------------------

    /// The topmost element of the SVG fragment `svg` (outermost) that `p`, in the outermost
    /// element's content box coordinates, hits per `pointer-events` (SVG 2 §16.5, §16.6); `None`
    /// when it hits no graphics element (the `svg` element's own box then decides).
    pub fn hit(&self, svg: NodeId, p: Point) -> Option<NodeId> {
        let m = self.local_transform(svg);
        self.hit_children(svg, m, p)
    }

    fn hit_children(&self, parent: NodeId, m: Matrix, p: Point) -> Option<NodeId> {
        let children: Vec<NodeId> = self.dom.children(parent).collect();
        for &c in children.iter().rev() {
            if !is_svg(&self.dom, c)
                || !self.displayed(c)
                || !self.passes_conditions(c)
                || !self.switch_chooses(c)
            {
                continue;
            }
            let name = local(&self.dom, c);
            if never_rendered(name) {
                continue;
            }
            let cm = m * self.local_transform(c);
            if let Some(hit) = self.hit_element(c, cm, p) {
                return Some(hit);
            }
        }
        None
    }

    /// Which areas of `n` take pointer events (SVG 2 §16.6 `pointer-events`): (fill, stroke);
    /// `None` for `none`.
    fn hit_areas(&self, n: NodeId) -> Option<(bool, bool)> {
        let pe = self
            .prop(n, "pointer-events")
            .unwrap_or_else(|| "auto".into());
        let visible = match self.prop(n, "visibility") {
            Some(v) => v == "visible",
            None => true,
        };
        let fill_paint = self.prop(n, "fill").as_deref() != Some("none");
        let stroke_paint = match self.prop(n, "stroke") {
            Some(v) => v != "none",
            None => false,
        };
        Some(match pe.as_str() {
            "none" => return None,
            "visibleFill" | "visiblefill" => (visible, false),
            "visibleStroke" | "visiblestroke" => (false, visible),
            "visible" => (visible, visible),
            "painted" => (fill_paint, stroke_paint),
            "fill" => (true, false),
            "stroke" => (false, true),
            "all" => (true, true),
            _ => (visible && fill_paint, visible && stroke_paint),
        })
    }

    fn hit_element(&self, n: NodeId, m: Matrix, p: Point) -> Option<NodeId> {
        let name = local(&self.dom, n);
        let pe = self
            .prop(n, "pointer-events")
            .unwrap_or_else(|| "auto".into());
        if pe == "none" {
            return None;
        }
        if name == "use" {
            if let Some(target) = self.use_target(n) {
                let tm = m * self.local_transform(target);
                // (an instance is what is hit, as a clone in the shadow tree: the event target
                // script sees is the `use`)
                let hit = self.hit_element(target, tm, p)?;
                return Some(if self.dom.shadow_root(n).is_some() {
                    hit
                } else {
                    n
                });
            }
            let (r, fragment) = self.use_resource(n)?;
            let target = r.target(fragment.as_deref())?;
            let q = SvgQuery::of_resource(&r.dom);
            let tm = m * q.local_transform(target);
            return q.hit_element(target, tm, p).map(|_| n);
        }
        // (a degenerate transform maps nothing back)
        if m.determinant().abs() < 1e-12 {
            return None;
        }
        let q = m.inverse() * p;
        if pe == "bounding-box" {
            if let Some(hit) = is_rendered_container(name)
                .then(|| self.hit_children(n, m, p))
                .flatten()
            {
                return Some(hit);
            }
            return self
                .bbox(n, BBoxOptions::default())
                .filter(|r| r.contains(q))
                .map(|_| n);
        }
        if is_rendered_container(name) {
            return self.hit_children(n, m, p);
        }
        if name == "text" {
            return self.hit_text(n, q);
        }
        let (fill, stroke) = self.hit_areas(n)?;
        if let Some(path) = self.shape_path(n) {
            if fill && geom::contains_fill(&path, q, self.fill_evenodd(n)) {
                return Some(n);
            }
            if stroke && geom::contains_stroke(&path, q, &self.stroke_style(n)) {
                return Some(n);
            }
            return None;
        }
        let visible = self.prop(n, "visibility").is_none_or(|v| v == "visible");
        match name {
            "image" | "foreignObject" => {
                // (images and text hit in their box)
                let r = self.bbox(n, BBoxOptions::default())?;
                (visible && r.contains(q)).then_some(n)
            }
            _ => None,
        }
    }

    /// The element of a `text` element whose glyph cell `q` hits, by the pointer events of the
    /// element each character is in (its `tspan`, `textPath`, `a` or the `text` itself).
    fn hit_text(&self, text_el: NodeId, q: Point) -> Option<NodeId> {
        let layout = self.text_layout(text_el)?;
        for c in layout.chars.iter().rev() {
            if c.hidden {
                continue;
            }
            let Some((fill, stroke)) = self.hit_areas(c.owner) else {
                continue;
            };
            // (SVG 2 §16.6: text hits by character cell, so that a point in a cell is over its
            // fill and its stroke alike; a stroke reaches beyond the cell too)
            let cell = c.cell();
            if ((fill || stroke) && geom::contains_fill(&cell, q, false))
                || (stroke && geom::contains_stroke(&cell, q, &self.stroke_style(c.owner)))
            {
                return Some(c.owner);
            }
        }
        None
    }

    /// Whether `r` (in `svg`'s user space) intersects `n`'s bounding box or encloses it (SVG 2
    /// §5.1.4 `checkIntersection`/`checkEnclosure`, by bounding boxes).
    pub fn check_rect(&self, svg: NodeId, n: NodeId, r: Rect, enclose: bool) -> bool {
        let Some(b) = self.bbox(n, BBoxOptions::default()) else {
            return false;
        };
        // (n's box in svg's user space: n's CTM relative to svg)
        let to_svg = self.ctm(svg, false).inverse() * self.ctm(n, false);
        let b = geom::transform_rect(b, to_svg);
        if enclose {
            b.x0 >= r.x0 && b.y0 >= r.y0 && b.x1 <= r.x1 && b.y1 <= r.y1
        } else {
            b.x0 <= r.x1 && b.x1 >= r.x0 && b.y0 <= r.y1 && b.y1 >= r.y0 && b.area() >= 0.0
        }
    }
}

/// The path data of a computed `d` (SVG 2 §9.4.1), as the style parsed it, in absolute commands: the
/// shape's path is drawn and measured from it without parsing the text again. (Relative commands
/// and control points are made absolute from the current point; smooth commands keep their letter,
/// and [`PathData::to_bezpath`] reflects their control points, §9.5.2.)
pub(crate) fn path_data_of(d: &SVGPathData) -> PathData {
    use style::values::generics::basic_shape::{
        ArcSize, ArcSweep, AxisEndPoint, AxisPosition, CommandEndPoint, ControlPoint,
        ControlReference, GenericShapeCommand as C,
    };
    use style::values::generics::Optional;
    use style::values::specified::svg_path::SVGPathPosition;
    type P = (f64, f64);
    let pos = |p: &SVGPathPosition| (p.horizontal as f64, p.vertical as f64);
    let end = |p: &CommandEndPoint<SVGPathPosition, f32>, cur: P| match p {
        CommandEndPoint::ToPosition(p) => pos(p),
        CommandEndPoint::ByCoordinate(c) => (cur.0 + c.x as f64, cur.1 + c.y as f64),
    };
    let control = |c: &ControlPoint<SVGPathPosition, f32>, from: P, to: P| match c {
        ControlPoint::Absolute(p) => pos(p),
        ControlPoint::Relative(r) => {
            let base = match r.reference {
                ControlReference::Origin => (0.0, 0.0),
                ControlReference::Start => from,
                ControlReference::End => to,
            };
            (base.0 + r.coord.x as f64, base.1 + r.coord.y as f64)
        }
    };
    // (path data has no keywords: the parser makes none)
    let axis = |a: &AxisEndPoint<f32>, cur: f64| match a {
        AxisEndPoint::ToPosition(AxisPosition::LengthPercent(v)) => *v as f64,
        AxisEndPoint::ToPosition(AxisPosition::Keyword(_)) => cur,
        AxisEndPoint::ByCoordinate(v) => cur + *v as f64,
    };
    let mut out = PathData {
        commands: Vec::with_capacity(d.commands().len()),
        error: false,
    };
    let (mut cur, mut start): (P, P) = ((0.0, 0.0), (0.0, 0.0));
    for c in d.commands() {
        let (cmd, args) = match c {
            C::Move { point } => {
                cur = end(point, cur);
                start = cur;
                (b'M', [cur.0, cur.1, 0.0, 0.0, 0.0, 0.0, 0.0])
            }
            C::Line { point } => {
                cur = end(point, cur);
                (b'L', [cur.0, cur.1, 0.0, 0.0, 0.0, 0.0, 0.0])
            }
            C::HLine { x } => {
                cur.0 = axis(x, cur.0);
                (b'H', [cur.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            }
            C::VLine { y } => {
                cur.1 = axis(y, cur.1);
                (b'V', [cur.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            }
            C::CubicCurve {
                point,
                control1,
                control2,
            } => {
                let p = end(point, cur);
                let (c1, c2) = (control(control1, cur, p), control(control2, cur, p));
                cur = p;
                (b'C', [c1.0, c1.1, c2.0, c2.1, p.0, p.1, 0.0])
            }
            C::QuadCurve { point, control1 } => {
                let p = end(point, cur);
                let c1 = control(control1, cur, p);
                cur = p;
                (b'Q', [c1.0, c1.1, p.0, p.1, 0.0, 0.0, 0.0])
            }
            C::SmoothCubic { point, control2 } => {
                let p = end(point, cur);
                let c2 = control(control2, cur, p);
                cur = p;
                (b'S', [c2.0, c2.1, p.0, p.1, 0.0, 0.0, 0.0])
            }
            C::SmoothQuad { point } => {
                cur = end(point, cur);
                (b'T', [cur.0, cur.1, 0.0, 0.0, 0.0, 0.0, 0.0])
            }
            C::Arc {
                point,
                radii,
                arc_sweep,
                arc_size,
                rotate,
            } => {
                cur = end(point, cur);
                let rx = radii.rx as f64;
                let ry = match radii.ry {
                    Optional::Some(ry) => ry as f64,
                    Optional::None => rx,
                };
                let large = if *arc_size == ArcSize::Large {
                    1.0
                } else {
                    0.0
                };
                let sweep = if *arc_sweep == ArcSweep::Cw { 1.0 } else { 0.0 };
                (b'A', [rx, ry, *rotate as f64, large, sweep, cur.0, cur.1])
            }
            C::Close => {
                cur = start;
                (b'Z', [0.0; 7])
            }
        };
        out.commands
            .push(snowlark_layout::svg::path::PathCommand { cmd, args });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The style's parsed path data draws the same path as the text parsed by the attribute's
    /// parser: every command, absolute and relative, implicit repeats, smooth reflections, arcs.
    #[test]
    fn path_data_of_a_computed_d_draws_as_the_attribute_does() {
        for text in [
            "M10 20 L30 40 H50 V60 Z",
            "m10 20 l30 40 h50 v60 z m5 5 l1 1",
            "M0 0 10 10 20 0",
            "m1 1 2 2 3 3z l4 4",
            "M10 10 C20 0 40 0 50 10 S80 20 90 10",
            "m10 10 c10-10 30-10 40 0 s30 10 40 0",
            "M10 80 Q52.5 10 95 80 T180 80",
            "m10 80 q42.5-70 85 0 t85 0 t10 10",
            "M10 10 A20 30 15 1 0 50 50 a20 20 0 0 1 10 10",
            "M0 0 a10 10 0 1110 10",
            "M1.5e1 .5L-3-4.25",
        ] {
            let (d, ok) = SVGPathData::parse_bytes(text.as_bytes());
            assert!(ok, "{text}");
            let ours: Vec<_> = path_data_of(&d).to_bezpath().elements().to_vec();
            let theirs: Vec<_> = PathData::parse(text).to_bezpath().elements().to_vec();
            assert_eq!(ours.len(), theirs.len(), "{text}");
            for (a, b) in ours.iter().zip(&theirs) {
                let (a, b) = (format!("{a:.3?}"), format!("{b:.3?}"));
                assert_eq!(a, b, "{text}");
            }
        }
    }
}

/// A computed `line-height` in px (`normal`: 1.2 × the font size).
fn line_height(v: &ComputedValues) -> Option<f32> {
    let font = v.get_font();
    let size = font.font_size.computed_size().px();
    let s = snowlark_style::computed::computed_property_by_name(v, "line-height")?;
    if s == "normal" {
        return Some(size * 1.2);
    }
    if let Some(px) = s.strip_suffix("px") {
        return px.trim().parse().ok();
    }
    s.trim().parse::<f32>().ok().map(|k| k * size)
}

/// What of an element's length context the units of its lengths read
/// ([`SvgQuery::length_context_for`]).
#[derive(Clone, Copy, Default)]
struct Needs(u8);

impl Needs {
    const VIEWPORT: u8 = 1;
    const FONT: u8 = 2;
    const METRICS: u8 = 4;
    const CH: u8 = 8;
    const IC: u8 = 16;
    const LINE_HEIGHT: u8 = 32;
    const ROOT: u8 = 64;
    const ALL: Needs = Needs(127);

    /// What [`LengthContext::unit_size`] and [`LengthContext::basis`] read for `unit`.
    fn of(unit: &LengthUnit) -> Needs {
        Needs(match unit {
            LengthUnit::Percent => Self::VIEWPORT,
            LengthUnit::Em => Self::FONT,
            LengthUnit::Ex => Self::FONT | Self::METRICS,
            LengthUnit::Other(u) => match &**u {
                "cap" => Self::FONT | Self::METRICS,
                "ch" => Self::FONT | Self::CH,
                "ic" => Self::FONT | Self::IC,
                "lh" => Self::FONT | Self::LINE_HEIGHT,
                "rem" | "rlh" | "rex" | "rch" | "ric" | "rcap" => Self::ROOT,
                // (`q` is absolute; the viewport units read the initial containing block, which
                // every context carries)
                _ => 0,
            },
            _ => 0,
        })
    }

    fn has(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
}
