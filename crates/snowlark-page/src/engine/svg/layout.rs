// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SVG DOM's geometry over the renderer ([`snowlark_render::svg_geometry`]): the context lengths
//! resolve in, bounding boxes, CTMs, shape geometry and text boxes; and the per-element SVG state
//! that is not in the DOM (`currentScale`/`currentTranslate`, the SMIL time containers).

use kurbo::{BezPath, Rect};
use snowlark_dom::NodeId;
use snowlark_layout::svg::{LengthContext, Matrix, StrokeStyle};
use snowlark_render::svg_geometry::{BBoxOptions, SvgQuery};
use snowlark_render::svg_text::TextChar;

use super::super::Engine;
use super::smil::Smil;
use super::use_tree::UseTrees;

/// An outermost `svg` element's `currentScale` and `currentTranslate` (SVG 2 §5.1.3). The point
/// object script holds mirrors these; it lives and dies with its wrapper.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CurrentView {
    pub scale: f32,
    pub translate: (f64, f64),
}

impl Default for CurrentView {
    fn default() -> Self {
        CurrentView {
            scale: 1.0,
            translate: (0.0, 0.0),
        }
    }
}

#[derive(Default)]
pub(crate) struct SvgState {
    pub current: rustc_hash::FxHashMap<NodeId, CurrentView>,
    /// The animation elements and time containers.
    pub smil: Smil,
    /// The shadow trees of `use` elements.
    pub uses: UseTrees,
    /// Within `moveBefore()`: SVG's `script` element has no moving steps that run it.
    pub moving: bool,
    /// How many fetches of each `image` element are in flight.
    pub image_fetches: rustc_hash::FxHashMap<NodeId, u32>,
}

impl SvgState {
    /// Freed nodes leave the side tables.
    pub(crate) fn nodes_freed(&mut self, freed: &snowlark_dom::NodeSet) {
        self.current.retain(|n, _| !freed.contains(n));
        self.image_fetches.retain(|n, _| !freed.contains(n));
        self.smil.nodes_freed(freed);
        self.uses.nodes_freed(freed);
    }
}

impl Engine {
    /// Runs `f` over the SVG geometry of `el`'s document, laid out (`None` when `el` is not in a
    /// rendered document).
    pub(crate) fn svg_layout<R>(&self, el: NodeId, f: impl FnOnce(&SvgQuery) -> R) -> Option<R> {
        let mut f = Some(f);
        let motions = self.smil_motions();
        let laid_out = self
            .with_layout(el, |r, _| {
                let mut q = r.svg_query()?;
                q.motion = motions.clone();
                let f = f.take()?;
                Some(f(&q))
            })
            .flatten();
        if laid_out.is_some() {
            return laid_out;
        }
        // (a document without rendering: geometry from the attributes alone)
        let f = f.take()?;
        if !self.dom().is_alive(el) {
            return None;
        }
        let mut q = SvgQuery::unstyled(self.dom.borrow());
        q.motion = motions;
        Some(f(&q))
    }

    /// The context `el`'s lengths resolve in (SVG 2 §4.5.2): its viewport and font; the detached
    /// defaults when it is not rendered.
    pub(crate) fn svg_length_context(&self, el: NodeId) -> LengthContext {
        self.svg_layout(el, |q| q.length_context(el))
            .unwrap_or_default()
    }

    pub(crate) fn svg_bbox(&self, el: NodeId, opts: BBoxOptions) -> Option<Rect> {
        self.svg_layout(el, |q| q.bbox(el, opts)).flatten()
    }

    /// `getCTM()` (`screen`: `getScreenCTM()` in client coordinates).
    pub(crate) fn svg_ctm(&self, el: NodeId, screen: bool) -> Option<Matrix> {
        let (m, root) = self.svg_layout(el, |q| (q.ctm(el, screen), q.outermost_svg(el)))?;
        if !screen {
            return Some(m);
        }
        let root = root?;
        let origin = self.with_layout(root, |r, st| {
            let dom = self.dom();
            let shift = super::super::view::scroll_shift(r, st, &dom, root);
            r.geometry(root)
                .map(|g| (g.content_box.x - shift.x, g.content_box.y - shift.y))
        })??;
        let (scale, translate) = self.svg_current_view(root);
        Some(
            Matrix::translate((origin.0 as f64, origin.1 as f64))
                * Matrix::translate(translate)
                * Matrix::scale(scale as f64)
                * m,
        )
    }

    /// An outermost `svg` element's `currentScale` and `currentTranslate`.
    pub(crate) fn svg_current_view(&self, svg: NodeId) -> (f32, (f64, f64)) {
        let v = self
            .svg
            .borrow()
            .current
            .get(&svg)
            .copied()
            .unwrap_or_default();
        (v.scale, v.translate)
    }

    /// A write to `currentTranslate`'s point.
    pub(crate) fn svg_set_current_translate(&self, svg: NodeId, x: f64, y: f64) {
        if self.dom().is_alive(svg) {
            let mut st = self.svg.borrow_mut();
            st.current.entry(svg).or_default().translate = (x, y);
        }
    }

    /// The equivalent path, fill rule and stroke of a shape.
    pub(crate) fn svg_shape(&self, el: NodeId) -> Option<(BezPath, bool, StrokeStyle)> {
        self.svg_layout(el, |q| {
            q.shape_path(el)
                .map(|p| (p, q.fill_evenodd(el), q.stroke_style(el)))
        })
        .flatten()
    }

    /// The characters of a text content element in its `text` element's layout.
    pub(crate) fn svg_text_chars(&self, el: NodeId) -> Vec<TextChar> {
        self.svg_layout(el, |q| {
            q.text_chars(el)
                .map(|(l, r)| l.chars[r].to_vec())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// `getCharNumAtPosition`: the index of the character at `p` among `el`'s.
    pub(crate) fn svg_char_at(&self, el: NodeId, p: kurbo::Point) -> Option<usize> {
        self.svg_layout(el, |q| q.char_at(el, p)).flatten()
    }

    /// CSSOM View §6.1 `getClientRects()` of an SVG element that is not an outermost `svg`: its
    /// bounding box through its screen CTM (`Some(None)`: not rendered, no rectangle; `None`: not
    /// such an element).
    pub(crate) fn svg_client_rect(
        &self,
        el: NodeId,
    ) -> Option<Option<super::super::geometry::Rect>> {
        {
            let dom = self.dom();
            if !super::is_svg_element(&dom, el)
                || snowlark_render::svg_geometry::is_outermost_svg(&dom, el)
            {
                return None;
            }
        }
        if self
            .svg_layout(el, |q| q.in_never_rendered(el))
            .unwrap_or(true)
        {
            return Some(None);
        }
        let bbox = self.svg_bbox(el, BBoxOptions::default());
        let Some(b) = bbox else {
            return Some(None);
        };
        let Some(m) = self.svg_ctm(el, true) else {
            return Some(None);
        };
        let r = snowlark_layout::svg::geom::transform_rect(b, m);
        Some(Some(super::super::geometry::Rect::read_only(
            r.x0,
            r.y0,
            r.width(),
            r.height(),
        )))
    }
}
