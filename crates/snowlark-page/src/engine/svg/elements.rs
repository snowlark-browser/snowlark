// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SVG element interfaces' own members: SVG 2 §4.4.2 `SVGGraphicsElement`, §4.4.3
//! `SVGGeometryElement` and `SVGPathElement`, §5.1.3 `SVGSVGElement`, §11.10.1
//! `SVGTextContentElement`, §11.6.6 `SVGMarkerElement`, the `style`, `script`, `image` and `a`
//! elements' string attributes, and SVG Animations' `SVGAnimationElement` and time containers.

use snowlark_bindings::generated::{
    DOMMatrix2DInit, DOMPointInit, SVGAElementImpl, SVGAnimationElementImpl, SVGBoundingBoxOptions,
    SVGGeometryElementImpl, SVGGraphicsElementImpl, SVGImageElementImpl, SVGMarkerElementImpl,
    SVGPathDataImpl, SVGPathDataSettings, SVGPathElementImpl, SVGPathSegment, SVGSVGElementImpl,
    SVGScriptElementImpl, SVGStyleElementImpl, SVGTextContentElementImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};
use snowlark_dom::{ns, NodeId};
use snowlark_layout::svg::smil::clock_value;
use snowlark_layout::svg::{
    geom, Angle, Length, LengthDir, Matrix, PathCommand, PathData, Transform,
};
use snowlark_render::svg_geometry::BBoxOptions;

use super::super::geometry::Rect;
use super::super::{exception, node_handle, Engine, Object};
use super::values::matrix_of;
use super::{is_svg, ItemValue, Owner};
use rt::ExceptionName;

fn invalid_state(message: &'static str) -> rt::Error {
    exception(ExceptionName::InvalidStateError, message)
}

fn index_error() -> rt::Error {
    exception(
        ExceptionName::IndexSizeError,
        "The index is greater than or equal to the number of characters.",
    )
}

/// HTML's `crossorigin` enumerated attribute: missing → null, invalid → `anonymous`.
fn cross_origin_value(v: Option<String>) -> Option<DomStr<'static>> {
    let v = v?;
    Some(DomStr::Borrowed(
        if v.eq_ignore_ascii_case("use-credentials") {
            "use-credentials"
        } else {
            "anonymous"
        },
    ))
}

/// HTML's `referrerpolicy` enumerated attribute (invalid and missing: the empty string).
fn referrer_policy_value(v: Option<String>) -> DomStr<'static> {
    const POLICIES: &[&str] = &[
        "no-referrer",
        "no-referrer-when-downgrade",
        "same-origin",
        "origin",
        "strict-origin",
        "origin-when-cross-origin",
        "strict-origin-when-cross-origin",
        "unsafe-url",
    ];
    let v = v.unwrap_or_default();
    DomStr::Borrowed(
        POLICIES
            .iter()
            .find(|p| p.eq_ignore_ascii_case(&v))
            .copied()
            .unwrap_or(""),
    )
}

/// A `DOMPointInit`'s 2D point, `None` when not finite.
fn point_of(p: &DOMPointInit) -> Option<kurbo::Point> {
    (p.x.is_finite() && p.y.is_finite()).then(|| kurbo::Point::new(p.x, p.y))
}

impl Engine {
    fn svg_dom_rect(&self, r: Option<kurbo::Rect>) -> Handle {
        let r = r.unwrap_or(kurbo::Rect::ZERO);
        self.alloc(Object::DomRect(Rect::new(
            r.x0,
            r.y0,
            r.width(),
            r.height(),
        )))
    }

    fn svg_matrix_handle(&self, m: Matrix) -> Handle {
        self.new_matrix(matrix_of(&m), false)
    }

    /// The outermost `svg` element of `el`'s fragment (itself included).
    pub(crate) fn svg_outermost(&self, el: NodeId) -> Option<NodeId> {
        let dom = self.dom();
        let mut cur = Some(el);
        let mut last = None;
        while let Some(c) = cur {
            if !super::is_svg_element(&dom, c) {
                break;
            }
            if is_svg(&dom, c, "svg") {
                last = Some(c);
            }
            // (a `use` element's shadow tree is part of the fragment)
            cur = match dom.parent(c) {
                Some(p) if dom.is_shadow_root(p) => dom.host(p),
                _ => dom.parent_element(c),
            };
        }
        last
    }

    /// Geometry methods need a shape: its path, fill rule and stroke.
    fn svg_shape_or_error(
        &self,
        el: NodeId,
    ) -> rt::Result<(kurbo::BezPath, bool, snowlark_layout::svg::StrokeStyle)> {
        self.svg_shape(el)
            .ok_or_else(|| invalid_state("The element's geometry cannot be computed."))
    }

    fn svg_total_length(&self, el: NodeId) -> rt::Result<f32> {
        let (p, _, _) = self.svg_shape_or_error(el)?;
        Ok(geom::total_length(&p) as f32)
    }

    fn svg_point_at_length(&self, el: NodeId, distance: f32) -> rt::Result<Handle> {
        let (p, _, _) = self.svg_shape_or_error(el)?;
        let q = geom::point_at_length(&p, distance as f64);
        Ok(self.new_point(q.x, q.y, 0.0, 1.0, false))
    }

    /// A text content element's characters, one entry per UTF-16 code unit (SVG 2 §11.10.1
    /// counts addressable characters in code units; a surrogate pair's second unit repeats its
    /// character with no advance).
    fn svg_text_units(&self, el: NodeId) -> Vec<snowlark_render::svg_text::TextChar> {
        let mut out = Vec::new();
        for c in self.svg_text_chars(el) {
            let second = c.ch.len_utf16() == 2;
            if second {
                let mut tail = c.clone();
                out.push(c);
                tail.advance = 0.0;
                out.push(tail);
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The UTF-16 length of a text content element's addressable characters.
    fn svg_char_count(&self, el: NodeId) -> usize {
        self.svg_text_units(el).len()
    }
}

impl SVGGraphicsElementImpl for Engine {
    /// SVG 2 §4.4.2 `getBBox()`.
    fn get_b_box(
        &self,
        _cx: &mut Cx,
        this: Handle,
        options: SVGBoundingBoxOptions,
    ) -> rt::Result<Handle> {
        let opts = BBoxOptions {
            fill: options.fill,
            stroke: options.stroke,
            markers: options.markers,
            clipped: options.clipped,
        };
        Ok(self.svg_dom_rect(self.svg_bbox(self.n(this), opts)))
    }
    /// SVG 2 §4.4.2 `getCTM()`.
    fn get_ctm(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        let el = self.n(this);
        if !self.dom().is_connected(el) {
            return Ok(None);
        }
        Ok(self.svg_ctm(el, false).map(|m| self.svg_matrix_handle(m)))
    }
    /// SVG 2 §4.4.2 `getScreenCTM()`.
    fn get_screen_ctm(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        let el = self.n(this);
        if !self.dom().is_connected(el) {
            return Ok(None);
        }
        Ok(self.svg_ctm(el, true).map(|m| self.svg_matrix_handle(m)))
    }
}

impl SVGGeometryElementImpl for Engine {
    /// SVG 2 §4.4.3 `isPointInFill()`.
    fn is_point_in_fill(
        &self,
        _cx: &mut Cx,
        this: Handle,
        point: DOMPointInit,
    ) -> rt::Result<bool> {
        let Some(p) = point_of(&point) else {
            return Ok(false);
        };
        let (path, evenodd, _) = self.svg_shape_or_error(self.n(this))?;
        Ok(geom::contains_fill(&path, p, evenodd))
    }
    /// SVG 2 §4.4.3 `isPointInStroke()`.
    fn is_point_in_stroke(
        &self,
        _cx: &mut Cx,
        this: Handle,
        point: DOMPointInit,
    ) -> rt::Result<bool> {
        let Some(p) = point_of(&point) else {
            return Ok(false);
        };
        let (path, _, stroke) = self.svg_shape_or_error(self.n(this))?;
        Ok(geom::contains_stroke(&path, p, &stroke))
    }
    /// SVG 2 §4.4.3 `getTotalLength()`.
    fn get_total_length(&self, _cx: &mut Cx, this: Handle) -> rt::Result<f32> {
        self.svg_total_length(self.n(this))
    }
    /// SVG 2 §4.4.3 `getPointAtLength()`.
    fn get_point_at_length(&self, _cx: &mut Cx, this: Handle, distance: f32) -> rt::Result<Handle> {
        self.svg_point_at_length(self.n(this), distance)
    }
}

impl SVGSVGElementImpl for Engine {
    /// SVG 2 §5.1.3 `currentScale` (1 unless set on an outermost `svg` element).
    fn current_scale(&self, this: Handle) -> f32 {
        let el = self.n(this);
        self.svg.borrow().current.get(&el).map_or(1.0, |c| c.scale)
    }
    fn set_current_scale(&self, _cx: &mut Cx, this: Handle, value: f32) -> rt::Result<()> {
        let el = self.n(this);
        let outermost = snowlark_render::svg_geometry::is_outermost_svg(&self.dom(), el);
        if outermost {
            let mut st = self.svg.borrow_mut();
            st.current.entry(el).or_default().scale = value;
        }
        Ok(())
    }
    /// SVG 2 §5.1.3 `currentTranslate`: a `DOMPoint` (the wrapper keeps it, so it is the same one
    /// each time), linked to the element, which holds the values.
    fn current_translate(&self, this: Handle) -> Handle {
        let el = self.n(this);
        // (a non-outermost `svg` element's point is read only)
        let outermost = snowlark_render::svg_geometry::is_outermost_svg(&self.dom(), el);
        let (_, (x, y)) = self.svg_current_view(el);
        self.alloc(Object::DomPoint(super::super::geometry::Point4 {
            x,
            y,
            z: 0.0,
            w: 1.0,
            read_only: !outermost,
            svg: Some(node_handle(el)),
        }))
    }
    /// SVG 2 §5.1.3 `getIntersectionList()`: the graphics elements in the fragment whose
    /// bounding boxes intersect `rect` (in this element's user space), in document order.
    fn get_intersection_list(
        &self,
        _cx: &mut Cx,
        this: Handle,
        rect: Handle,
        reference_element: Option<Handle>,
    ) -> rt::Result<Handle> {
        let list = self.svg_rect_query(this, rect, reference_element, false);
        Ok(self.static_node_list(list))
    }
    fn get_enclosure_list(
        &self,
        _cx: &mut Cx,
        this: Handle,
        rect: Handle,
        reference_element: Option<Handle>,
    ) -> rt::Result<Handle> {
        let list = self.svg_rect_query(this, rect, reference_element, true);
        Ok(self.static_node_list(list))
    }
    fn check_intersection(
        &self,
        _cx: &mut Cx,
        this: Handle,
        element: Handle,
        rect: Handle,
    ) -> rt::Result<bool> {
        Ok(self.svg_check(this, element, rect, false))
    }
    fn check_enclosure(
        &self,
        _cx: &mut Cx,
        this: Handle,
        element: Handle,
        rect: Handle,
    ) -> rt::Result<bool> {
        Ok(self.svg_check(this, element, rect, true))
    }
    fn deselect_all(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let doc = self.dom().node_document(self.n(this));
        self.svg_deselect(cx, doc);
        Ok(())
    }
    fn create_svg_number(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Handle> {
        Ok(self.new_svg_item(ItemValue::Number(0.0), Owner::Detached, false))
    }
    fn create_svg_length(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Handle> {
        Ok(self.new_svg_item(
            ItemValue::Length(Length::ZERO, LengthDir::Other),
            Owner::Detached,
            false,
        ))
    }
    fn create_svg_angle(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Handle> {
        Ok(self.new_svg_item(ItemValue::Angle(Angle::ZERO), Owner::Detached, false))
    }
    fn create_svg_point(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Handle> {
        Ok(self.new_point(0.0, 0.0, 0.0, 1.0, false))
    }
    fn create_svg_matrix(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Handle> {
        Ok(self.svg_matrix_handle(Matrix::IDENTITY))
    }
    fn create_svg_rect(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Handle> {
        Ok(self.alloc(Object::DomRect(Rect::new(0.0, 0.0, 0.0, 0.0))))
    }
    fn create_svg_transform(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<Handle> {
        Ok(self.new_svg_item(
            ItemValue::Transform(Transform::default(), None),
            Owner::Detached,
            false,
        ))
    }
    fn create_svg_transform_from_matrix(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        matrix: DOMMatrix2DInit,
    ) -> rt::Result<Handle> {
        let m = super::super::geometry::matrix_from_2d_init(&matrix)?;
        Ok(self.new_svg_item(
            ItemValue::Transform(Transform::matrix(Matrix::new(m)), None),
            Owner::Detached,
            false,
        ))
    }
    /// SVG 2 §5.1.3 `getElementById()`: the first element in this element's subtree with the ID.
    fn get_element_by_id(
        &self,
        _cx: &mut Cx,
        this: Handle,
        element_id: &str,
    ) -> rt::Result<Option<Handle>> {
        let dom = self.dom();
        let root = self.n(this);
        Ok(dom
            .descendants(root)
            .find(|&d| dom.is_element(d) && dom.attr(d, &ns!(), &"id".into()) == Some(element_id))
            .map(node_handle))
    }
    fn suspend_redraw(&self, _cx: &mut Cx, _this: Handle, _max: u32) -> rt::Result<u32> {
        Ok(1)
    }
    fn unsuspend_redraw(&self, _cx: &mut Cx, _this: Handle, _id: u32) -> rt::Result<()> {
        Ok(())
    }
    fn unsuspend_redraw_all(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn force_redraw(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    /// SVG Animations §3.6.8 `pauseAnimations()`.
    fn pause_animations(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.smil_pause(self.n(this));
        Ok(())
    }
    fn unpause_animations(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.smil_unpause(self.n(this));
        Ok(())
    }
    fn animations_paused(&self, _cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        Ok(self.smil_paused(self.n(this)))
    }
    fn get_current_time(&self, _cx: &mut Cx, this: Handle) -> rt::Result<f32> {
        Ok(self.svg_time_now(self.n(this)) as f32)
    }
    /// SVG Animations §3.6.8 `setCurrentTime()`.
    fn set_current_time(&self, cx: &mut Cx, this: Handle, seconds: f32) -> rt::Result<()> {
        self.smil_seek(cx, self.n(this), seconds as f64);
        Ok(())
    }
}

impl Engine {
    /// The rect of an `SVGRect` argument.
    fn svg_rect_arg(&self, rect: Handle) -> kurbo::Rect {
        match &*self.obj(rect) {
            Object::DomRect(r) => kurbo::Rect::new(r.x, r.y, r.x + r.width, r.y + r.height),
            _ => kurbo::Rect::ZERO,
        }
    }

    fn svg_check(&self, this: Handle, element: Handle, rect: Handle, enclose: bool) -> bool {
        let svg = self.n(this);
        let el = self.n(element);
        let r = self.svg_rect_arg(rect);
        self.svg_layout(svg, |q| q.check_rect(svg, el, r, enclose))
            .unwrap_or(false)
    }

    fn svg_rect_query(
        &self,
        this: Handle,
        rect: Handle,
        reference: Option<Handle>,
        enclose: bool,
    ) -> Vec<NodeId> {
        let svg = self.n(this);
        let r = self.svg_rect_arg(rect);
        let reference = reference.map(|h| self.n(h));
        let candidates: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(svg)
                .filter(|&d| {
                    dom.element(d).is_some_and(|e| {
                        e.name().ns == ns!(svg)
                            && matches!(
                                &*e.name().local,
                                "circle"
                                    | "ellipse"
                                    | "image"
                                    | "line"
                                    | "path"
                                    | "polygon"
                                    | "polyline"
                                    | "rect"
                                    | "text"
                                    | "use"
                            )
                    }) && reference.is_none_or(|r| dom.is_inclusive_ancestor_of(r, d))
                })
                .collect()
        };
        self.svg_layout(svg, |q| {
            candidates
                .into_iter()
                .filter(|&c| q.check_rect(svg, c, r, enclose))
                .collect()
        })
        .unwrap_or_default()
    }

    /// `deselectAll()`: the document's selection is emptied.
    fn svg_deselect(&self, cx: &mut Cx, doc: NodeId) {
        if let Some(sel) = self.selections.borrow().get(&doc).copied() {
            let _ = snowlark_bindings::generated::SelectionImpl::remove_all_ranges(self, cx, sel);
        }
    }
}

impl SVGTextContentElementImpl for Engine {
    /// SVG 2 §11.10.1 `getNumberOfChars()`.
    fn get_number_of_chars(&self, _cx: &mut Cx, this: Handle) -> rt::Result<i32> {
        Ok(self.svg_char_count(self.n(this)) as i32)
    }
    /// `getComputedTextLength()`: the sum of the characters' advances.
    fn get_computed_text_length(&self, _cx: &mut Cx, this: Handle) -> rt::Result<f32> {
        Ok(self
            .svg_text_units(self.n(this))
            .iter()
            .map(|c| c.advance)
            .fold(0.0, |a, b| a + b))
    }
    fn get_sub_string_length(
        &self,
        _cx: &mut Cx,
        this: Handle,
        charnum: u32,
        nchars: u32,
    ) -> rt::Result<f32> {
        let chars = self.svg_text_units(self.n(this));
        let i = charnum as usize;
        if i >= chars.len() {
            return Err(index_error());
        }
        let end = i.saturating_add(nchars as usize).min(chars.len());
        Ok(chars[i..end]
            .iter()
            .map(|c| c.advance)
            .fold(0.0, |a, b| a + b))
    }
    fn get_start_position_of_char(
        &self,
        _cx: &mut Cx,
        this: Handle,
        charnum: u32,
    ) -> rt::Result<Handle> {
        let chars = self.svg_text_units(self.n(this));
        let c = chars.get(charnum as usize).ok_or_else(index_error)?;
        Ok(self.new_point(c.x as f64, c.y as f64, 0.0, 1.0, false))
    }
    fn get_end_position_of_char(
        &self,
        _cx: &mut Cx,
        this: Handle,
        charnum: u32,
    ) -> rt::Result<Handle> {
        let chars = self.svg_text_units(self.n(this));
        let c = chars.get(charnum as usize).ok_or_else(index_error)?;
        let (x, y) = c.end();
        Ok(self.new_point(x as f64, y as f64, 0.0, 1.0, false))
    }
    fn get_extent_of_char(&self, _cx: &mut Cx, this: Handle, charnum: u32) -> rt::Result<Handle> {
        let chars = self.svg_text_units(self.n(this));
        let c = chars.get(charnum as usize).ok_or_else(index_error)?;
        let r = c.extent();
        Ok(self.alloc(Object::DomRect(Rect::new(
            r.x0,
            r.y0,
            r.width(),
            r.height(),
        ))))
    }
    fn get_rotation_of_char(&self, _cx: &mut Cx, this: Handle, charnum: u32) -> rt::Result<f32> {
        let chars = self.svg_text_units(self.n(this));
        let c = chars.get(charnum as usize).ok_or_else(index_error)?;
        Ok(c.rotate)
    }
    fn get_char_num_at_position(
        &self,
        _cx: &mut Cx,
        this: Handle,
        point: DOMPointInit,
    ) -> rt::Result<i32> {
        let Some(p) = point_of(&point) else {
            return Ok(-1);
        };
        let el = self.n(this);
        let Some(i) = self.svg_char_at(el, p) else {
            return Ok(-1);
        };
        // (the character index in code units)
        let units: usize = self.svg_text_chars(el)[..i]
            .iter()
            .map(|c| c.ch.len_utf16())
            .sum();
        Ok(units as i32)
    }
    /// SVG 2 §11.10.1 `selectSubString()` (deprecated): checks the index; selecting is not modelled.
    fn select_sub_string(
        &self,
        _cx: &mut Cx,
        this: Handle,
        charnum: u32,
        _nchars: u32,
    ) -> rt::Result<()> {
        if charnum as usize >= self.svg_char_count(self.n(this)) {
            return Err(index_error());
        }
        Ok(())
    }
}

impl SVGMarkerElementImpl for Engine {
    fn orient(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.svg_attr(self.n(this), "orient").unwrap_or_default())
    }
    fn set_orient(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.svg_set_attr(cx, self.n(this), "orient", value);
        Ok(())
    }
    fn set_orient_to_auto(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.svg_set_attr(cx, self.n(this), "orient", "auto");
        Ok(())
    }
    fn set_orient_to_angle(&self, cx: &mut Cx, this: Handle, angle: Handle) -> rt::Result<()> {
        let a = match self.svg_item(angle).0 {
            ItemValue::Angle(a) => a,
            _ => Angle::ZERO,
        };
        self.svg_set_attr(cx, self.n(this), "orient", &a.to_css());
        Ok(())
    }
}

impl SVGStyleElementImpl for Engine {
    fn disabled(&self, this: Handle) -> bool {
        self.style_element_disabled(this)
    }
    fn set_disabled(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.set_style_element_disabled(cx, this, value)
    }
}

impl SVGScriptElementImpl for Engine {
    fn cross_origin(&self, this: Handle) -> Option<DomStr<'_>> {
        cross_origin_value(self.svg_attr(self.n(this), "crossorigin"))
    }
    fn set_cross_origin(&self, cx: &mut Cx, this: Handle, value: Option<&str>) -> rt::Result<()> {
        match value {
            Some(v) => self.svg_set_attr(cx, self.n(this), "crossorigin", v),
            None => self.remove_attr(cx, self.n(this), "crossorigin"),
        }
        Ok(())
    }
    fn referrer_policy(&self, this: Handle) -> DomStr<'_> {
        referrer_policy_value(self.svg_attr(self.n(this), "referrerpolicy"))
    }
    fn set_referrer_policy(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.svg_set_attr(cx, self.n(this), "referrerpolicy", value);
        Ok(())
    }
}

impl Engine {
    /// The URL an SVG `image` element refers to (SVG 2 §15.3: `href`, else `xlink:href`).
    fn svg_image_url(&self, el: NodeId) -> Option<url::Url> {
        let dom = self.dom();
        let href = dom
            .attr(el, &ns!(), &"href".into())
            .or_else(|| dom.attr(el, &ns!(xlink), &"href".into()))?
            .to_owned();
        drop(dom);
        self.base_url_of(el).join(href.trim()).ok()
    }

    /// An SVG `image` element was inserted or its reference changed: its resource is fetched for its
    /// `load` or `error` event.
    pub(crate) fn svg_image_update(&self, el: NodeId) {
        if !self.dom().is_connected(el) {
            return;
        }
        if let Some(url) = self.svg_image_url(el) {
            *self.svg.borrow_mut().image_fetches.entry(el).or_default() += 1;
            self.fetch_resource(url, crate::loader::Purpose::SvgImage(el));
        }
    }

    /// A fetch of an `image` element ended: whether it was the last one started.
    pub(crate) fn svg_image_fetched(&self, el: NodeId) -> bool {
        let mut st = self.svg.borrow_mut();
        let left = st.image_fetches.get_mut(&el).map_or(0, |n| {
            *n = n.saturating_sub(1);
            *n
        });
        if left == 0 {
            st.image_fetches.remove(&el);
        }
        left == 0 && self.dom().is_alive(el)
    }
}

impl SVGImageElementImpl for Engine {
    fn cross_origin(&self, this: Handle) -> Option<DomStr<'_>> {
        cross_origin_value(self.svg_attr(self.n(this), "crossorigin"))
    }
    fn set_cross_origin(&self, cx: &mut Cx, this: Handle, value: Option<&str>) -> rt::Result<()> {
        match value {
            Some(v) => self.svg_set_attr(cx, self.n(this), "crossorigin", v),
            None => self.remove_attr(cx, self.n(this), "crossorigin"),
        }
        Ok(())
    }
    fn referrer_policy(&self, this: Handle) -> DomStr<'_> {
        referrer_policy_value(self.svg_attr(self.n(this), "referrerpolicy"))
    }
    fn set_referrer_policy(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.svg_set_attr(cx, self.n(this), "referrerpolicy", value);
        Ok(())
    }
}

impl SVGAElementImpl for Engine {
    fn referrer_policy(&self, this: Handle) -> DomStr<'_> {
        referrer_policy_value(self.svg_attr(self.n(this), "referrerpolicy"))
    }
    fn set_referrer_policy(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.svg_set_attr(cx, self.n(this), "referrerpolicy", value);
        Ok(())
    }
}

impl SVGAnimationElementImpl for Engine {
    /// SVG Animations §3.1 the target element: the `href` reference, else the parent element.
    fn target_element(&self, this: Handle) -> Option<Handle> {
        let el = self.n(this);
        let dom = self.dom();
        let href = dom
            .attr(el, &ns!(), &"href".into())
            .or_else(|| dom.attr(el, &ns!(xlink), &"href".into()));
        let target = match href {
            Some(h) => {
                let id = h.trim().strip_prefix('#')?;
                let root = dom.root(el);
                dom.descendants(root)
                    .find(|&d| dom.is_element(d) && dom.attr(d, &ns!(), &"id".into()) == Some(id))?
            }
            None => dom.parent_element(el)?,
        };
        super::is_svg_element(&dom, target).then(|| node_handle(target))
    }
    /// The begin time of the current interval (no interval: an error).
    fn get_start_time(&self, _cx: &mut Cx, this: Handle) -> rt::Result<f32> {
        self.smil_start_time(self.n(this))
            .map(|v| v as f32)
            .ok_or_else(|| invalid_state("The animation element has no current interval."))
    }
    fn get_current_time(&self, _cx: &mut Cx, this: Handle) -> rt::Result<f32> {
        Ok(self.svg_time_now(self.n(this)) as f32)
    }
    /// The simple duration (`dur`), a NotSupportedError when indefinite.
    fn get_simple_duration(&self, _cx: &mut Cx, this: Handle) -> rt::Result<f32> {
        self.svg_attr(self.n(this), "dur")
            .and_then(|d| clock_value(&d))
            .filter(|d| *d > 0.0)
            .map(|d| d as f32)
            .ok_or_else(|| {
                exception(
                    ExceptionName::NotSupportedError,
                    "The simple duration is indefinite.",
                )
            })
    }
    /// SVG Animations §5.2 `beginElement()`: a begin instance now.
    fn begin_element(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.smil_add_instance(cx, self.n(this), true, 0.0);
        Ok(())
    }
    fn begin_element_at(&self, cx: &mut Cx, this: Handle, offset: f32) -> rt::Result<()> {
        self.smil_add_instance(cx, self.n(this), true, offset as f64);
        Ok(())
    }
    fn end_element(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.smil_add_instance(cx, self.n(this), false, 0.0);
        Ok(())
    }
    fn end_element_at(&self, cx: &mut Cx, this: Handle, offset: f32) -> rt::Result<()> {
        self.smil_add_instance(cx, self.n(this), false, offset as f64);
        Ok(())
    }
}

impl Engine {
    /// Filter Effects §15 `setStdDeviation()`: the `stdDeviation` attribute set to both values.
    fn svg_set_std_deviation(&self, cx: &mut Cx, this: Handle, x: f32, y: f32) {
        let v = format!(
            "{} {}",
            snowlark_layout::svg::fmt_number(x),
            snowlark_layout::svg::fmt_number(y)
        );
        self.svg_set_attr(cx, self.n(this), "stdDeviation", &v);
    }
}

impl snowlark_bindings::generated::SVGFEDropShadowElementImpl for Engine {
    fn set_std_deviation(
        &self,
        cx: &mut Cx,
        this: Handle,
        std_deviation_x: f32,
        std_deviation_y: f32,
    ) -> rt::Result<()> {
        self.svg_set_std_deviation(cx, this, std_deviation_x, std_deviation_y);
        Ok(())
    }
}

impl snowlark_bindings::generated::SVGFEGaussianBlurElementImpl for Engine {
    fn set_std_deviation(
        &self,
        cx: &mut Cx,
        this: Handle,
        std_deviation_x: f32,
        std_deviation_y: f32,
    ) -> rt::Result<()> {
        self.svg_set_std_deviation(cx, this, std_deviation_x, std_deviation_y);
        Ok(())
    }
}

/// The interfaces and mixins with nothing of their own beyond reflected attributes.
macro_rules! no_members {
    ($($t:ident)*) => {
        $(impl snowlark_bindings::generated::$t for Engine {})*
    };
}

no_members! {
    SVGAnimateElementImpl SVGAnimateMotionElementImpl SVGAnimateTransformElementImpl
    SVGAnimatedPointsImpl SVGCircleElementImpl SVGClipPathElementImpl
    SVGComponentTransferFunctionElementImpl SVGDefsElementImpl SVGDescElementImpl
    SVGEllipseElementImpl SVGFilterElementImpl SVGFilterPrimitiveStandardAttributesImpl
    SVGFitToViewBoxImpl SVGForeignObjectElementImpl SVGGradientElementImpl SVGLineElementImpl
    SVGLinearGradientElementImpl SVGMaskElementImpl SVGMetadataElementImpl SVGPatternElementImpl
    SVGPolygonElementImpl SVGPolylineElementImpl SVGRadialGradientElementImpl SVGRectElementImpl
    SVGSetElementImpl SVGStopElementImpl SVGSwitchElementImpl SVGSymbolElementImpl SVGTestsImpl
    SVGTextElementImpl SVGTextPathElementImpl SVGTextPositioningElementImpl SVGTitleElementImpl
    SVGUnitTypesImpl SVGUseElementImpl SVGViewElementImpl SVGFEBlendElementImpl
    SVGFEColorMatrixElementImpl SVGFEComponentTransferElementImpl SVGFECompositeElementImpl
    SVGFEConvolveMatrixElementImpl SVGFEDiffuseLightingElementImpl SVGFEDisplacementMapElementImpl
    SVGFEDistantLightElementImpl SVGFEFloodElementImpl SVGFEFuncAElementImpl SVGFEFuncBElementImpl
    SVGFEFuncGElementImpl SVGFEFuncRElementImpl SVGFEImageElementImpl SVGFEMergeElementImpl
    SVGFEMergeNodeElementImpl SVGFEMorphologyElementImpl SVGFEOffsetElementImpl
    SVGFEPointLightElementImpl SVGFESpecularLightingElementImpl SVGFESpotLightElementImpl
    SVGFETileElementImpl SVGFETurbulenceElementImpl SVGGElementImpl SVGMPathElementImpl
    SVGTSpanElementImpl SVGURIReferenceImpl
}

impl Engine {
    /// The base value of a `path` element's `d` attribute (SVG Paths: the DOM methods read the
    /// attribute, neither the `d` property nor an animation).
    fn svg_path_data(&self, this: Handle) -> PathData {
        self.svg_attr(self.n(this), "d")
            .map_or(PathData::default(), |d| PathData::parse(&d))
    }
}

fn path_segment(c: &PathCommand) -> SVGPathSegment {
    SVGPathSegment {
        type_: (c.cmd as char).to_string(),
        values: c.params().iter().map(|v| *v as f32).collect(),
    }
}

impl SVGPathDataImpl for Engine {
    /// SVG Paths `getPathData()`.
    fn get_path_data(
        &self,
        _cx: &mut Cx,
        this: Handle,
        settings: SVGPathDataSettings,
    ) -> rt::Result<Vec<SVGPathSegment>> {
        let data = self.svg_path_data(this);
        Ok(if settings.normalize {
            data.normalized().iter().map(path_segment).collect()
        } else {
            data.commands.iter().map(path_segment).collect()
        })
    }

    /// SVG Paths `setPathData()`: the segments up to the first that is not one become the `d`
    /// attribute; none removes it.
    fn set_path_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        path_data: Vec<SVGPathSegment>,
    ) -> rt::Result<()> {
        let mut d = String::new();
        for segment in &path_data {
            let [cmd] = segment.type_.as_bytes() else {
                break;
            };
            let arity = PathCommand::arity(*cmd);
            if arity != segment.values.len()
                || segment.values.iter().any(|v| !v.is_finite())
                || (d.is_empty() && !cmd.eq_ignore_ascii_case(&b'M'))
            {
                break;
            }
            if !d.is_empty() {
                d.push(' ');
            }
            // (a closepath serializes as `Z`, either way)
            d.push(if arity == 0 { 'Z' } else { *cmd as char });
            for v in &segment.values {
                d.push(' ');
                d.push_str(&snowlark_layout::svg::fmt_number(*v));
            }
        }
        let el = self.n(this);
        if d.is_empty() {
            self.dom_mut().remove_attribute(el, "d");
            self.process_reactions(cx);
        } else {
            self.svg_set_attr(cx, el, "d", &d);
        }
        Ok(())
    }
}

impl SVGPathElementImpl for Engine {
    /// SVG Paths `getPathSegmentAtLength()`.
    fn get_path_segment_at_length(
        &self,
        _cx: &mut Cx,
        this: Handle,
        distance: f32,
    ) -> rt::Result<Option<SVGPathSegment>> {
        Ok(self
            .svg_path_data(this)
            .segment_at_length(distance as f64)
            .map(path_segment))
    }
}
