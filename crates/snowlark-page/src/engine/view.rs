// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSSOM View (https://drafts.csswg.org/cssom-view/) over the renderer's layout: element geometry
//! (§6 `getClientRects`, `getBoundingClientRect`, `client*`, `scroll*`, §7 `offset*`), scrolling of the
//! viewport and of scroll containers (§4 `scrollTo`/`scrollBy`, §6 `scrollIntoView`, `scrollTop`),
//! and hit testing (§5 `elementFromPoint`).

use snowlark_bindings::generated::{ScrollBehavior, ScrollResult};
use snowlark_bindings::rt::{Cx, JsPromise};
use snowlark_dom::{local_name, Dom, NodeId, QuirksMode};
use snowlark_layout::style::{
    Display, DisplayOutside, Overflow, Position, Side, Style, WritingMode,
};
use snowlark_layout::{fragment_flags, BoxGeometry, PaintOptions};
use snowlark_paint::{Point, Rect};
use snowlark_render::Renderer;

use super::geometry;
use super::Engine;
use crate::render::RenderState;

/// The parent across shadow roots but not into slots: a shadow root's host for a shadow root's child.
/// Walks along boxes use [`Renderer::flat_parent`] instead (a slotted element's box is in its slot's).
fn layout_parent(dom: &Dom, n: NodeId) -> Option<NodeId> {
    let p = dom.parent(n)?;
    if dom.is_shadow_root(p) {
        return dom.host(p);
    }
    dom.is_element(p).then_some(p)
}

/// How far `el`'s boxes are moved by scrolling: the scroll positions of the scroll containers on its
/// containing block chain (an absolutely positioned box skips the boxes up to its positioned
/// containing block, a fixed one those up to one that is the containing block of all positioned
/// descendants) and of the viewport (unless a fixed box holds it in place), as layout's containing
/// blocks are found ([`snowlark_layout::LayoutTree`]'s `containing_block_of`).
pub(crate) fn scroll_shift(r: &Renderer, st: &RenderState, dom: &Dom, el: NodeId) -> Point {
    let mut shift = Point::ZERO;
    let mut position = r.element_style(el).map(|s| s.position());
    let mut n = r.flat_parent(dom, el);
    while let Some(id) = n {
        let s = r.element_style(id);
        let pos = s.as_ref().map_or(Position::Static, |s| s.position());
        let establishes = s.as_ref().is_some_and(|s| s.establishes_fixed_cb());
        let in_chain = match position {
            Some(Position::Absolute) => pos != Position::Static || establishes,
            Some(Position::Fixed) => establishes,
            _ => true,
        };
        if in_chain {
            if let Some(sc) = st.element_scroll.get(&id) {
                shift.x += sc.x;
                shift.y += sc.y;
            }
            position = Some(pos);
        }
        n = r.flat_parent(dom, id);
    }
    // (a fixed box whose containing block is the viewport does not move with it)
    if position != Some(Position::Fixed) {
        shift.x += st.scroll.x;
        shift.y += st.scroll.y;
    }
    shift
}

/// The SVG graphics element at `p` (relative to the content box) of `svg` when it is an outermost
/// `svg` element.
fn svg_hit(r: &Renderer, svg: NodeId, p: Point) -> Option<NodeId> {
    let q = r.svg_query()?;
    if !snowlark_render::svg_geometry::is_outermost_svg(q.dom(), svg) {
        return None;
    }
    q.hit(svg, kurbo::Point::new(p.x as f64, p.y as f64))
}

/// Whether `el` is an HTML element whose box stays replaced with `display: inline` (an atomic
/// inline, not an inline box), for the scroll and client sizes.
fn inline_replaced(dom: &Dom, el: NodeId) -> bool {
    matches!(
        dom.local_name(el).as_ref(),
        "img" | "video" | "canvas" | "iframe" | "embed" | "object"
    ) && dom.namespace(el) == &snowlark_dom::ns!(html)
}

/// Whether `el` generates an inline box (`display: inline`, not an atomic inline). A `button` never
/// does: with an inline `display` it behaves as `inline-block` (button layout, HTML §15.5.3).
fn is_inline_box(r: &Renderer, el: NodeId) -> bool {
    if r.dom()
        .is_some_and(|d| d.is_html_element_named(el, &local_name!("button")))
    {
        return false;
    }
    r.element_style(el).is_some_and(|s| {
        matches!(
            s.display(),
            Display::Box {
                outside: DisplayOutside::Inline,
                inside: snowlark_layout::style::DisplayInside::Flow,
                ..
            }
        )
    })
}

/// Where the viewport's scroll origin is (CSS Overflow 3 §2.3 with CSS Writing Modes 4 §8: the
/// principal writing mode, the root element's): at the right edge (else the left) and at the
/// bottom (else the top).
fn viewport_origin_end(r: &Renderer) -> (bool, bool) {
    use snowlark_layout::style::{Direction, WritingMode};
    let Some((mode, direction)) = r.principal_writing_mode() else {
        return (false, false);
    };
    let rtl = direction == Direction::Rtl;
    match mode {
        WritingMode::HorizontalTb => (rtl, false),
        WritingMode::VerticalRl | WritingMode::SidewaysRl => (true, rtl),
        WritingMode::VerticalLr => (false, rtl),
        WritingMode::SidewaysLr => (false, !rtl),
    }
}

/// The document's scrolling area (CSSOM View §2: the initial containing block united with the
/// document's scrollable overflow), less what lies before the viewport's scroll origin.
fn scrolling_area(r: &Renderer, st: &RenderState) -> Rect {
    let v = r.layout_viewport(st.viewport);
    let icb = Rect::new(0.0, 0.0, v.width, v.height);
    let area = match r.fragments() {
        Some(root) => icb.union(&root.overflow),
        None => icb,
    };
    let (right, bottom) = viewport_origin_end(r);
    let (l, rr) = if right {
        (area.x, icb.right())
    } else {
        (0.0, area.right())
    };
    let (t, b) = if bottom {
        (area.y, icb.bottom())
    } else {
        (0.0, area.bottom())
    };
    Rect::from_ltrb(l, t, rr, b)
}

/// The range of the viewport's scroll positions: `min` negative on an axis whose scroll origin is
/// at the end (an rtl or vertical-rl root).
pub(crate) fn viewport_range(r: &Renderer, st: &RenderState) -> (Point, Point) {
    let area = scrolling_area(r, st);
    // (the scrollport is the viewport without its scroll bar)
    let v = r.layout_viewport(st.viewport);
    let (right, bottom) = viewport_origin_end(r);
    let x = if right {
        (area.x.min(0.0), 0.0)
    } else {
        (0.0, (area.right() - v.width).max(0.0))
    };
    let y = if bottom {
        (area.y.min(0.0), 0.0)
    } else {
        (0.0, (area.bottom() - v.height).max(0.0))
    };
    (Point::new(x.0, y.0), Point::new(x.1, y.1))
}

/// The range of scroll positions of a scroll container, if `el` is one: (min, max) per axis,
/// measured from its scroll origin (CSS Overflow 3 §2.3), so `min` is negative on an axis whose
/// origin is at the right or bottom edge (rtl, reversed flex containers).
fn element_range(r: &Renderer, dom: &Dom, el: NodeId) -> Option<(Point, Point)> {
    element_range_at(r, dom, el, &r.geometry(el)?)
}

/// [`element_range`] with the element's geometry already at hand.
fn element_range_at(
    r: &Renderer,
    dom: &Dom,
    el: NodeId,
    g: &BoxGeometry,
) -> Option<(Point, Point)> {
    let style = r.element_style(el)?;
    if !style.overflow_x().is_scroll_container() && !style.overflow_y().is_scroll_container() {
        return None;
    }
    // CSS Overflow 3 §3.3: with a root whose overflow is visible, the body's overflow propagates to
    // the viewport and the body is not a scroll container.
    let doc = dom.node_document(el);
    if dom.body(doc) == Some(el) && dom.is_html_element_named(el, &local_name!("body")) {
        let root_visible = dom.document_element(doc).is_some_and(|root| {
            r.element_style(root).is_some_and(|s| {
                s.overflow_x() == Overflow::Visible && s.overflow_y() == Overflow::Visible
            })
        });
        if root_visible {
            return None;
        }
    }
    let (mut min, mut max) = g.scroll_range();
    // (an axis that `overflow: clip` clips is not scrollable, not even by script)
    if style.overflow_x() == Overflow::Clip {
        min.x = 0.0;
        max.x = 0.0;
    }
    if style.overflow_y() == Overflow::Clip {
        min.y = 0.0;
        max.y = 0.0;
    }
    Some((min, max))
}

/// Whether `el` or an ancestor (in the flat tree) is `position: fixed`: the viewport's scrolling
/// does not move it.
fn fixed_position_chain(r: &Renderer, dom: &Dom, el: NodeId) -> bool {
    let mut n = Some(el);
    while let Some(id) = n {
        if r.element_style(id)
            .is_some_and(|s| s.position() == Position::Fixed)
        {
            return true;
        }
        n = r.flat_parent(dom, id);
    }
    false
}

/// A `ScrollLogicalPosition` keyword with the static lifetime of the keywords themselves.
fn leak(mode: &str) -> &'static str {
    match mode {
        "start" => "start",
        "end" => "end",
        "center" => "center",
        "find" => "find",
        _ => "nearest",
    }
}

fn snap(v: f32) -> i32 {
    v.round() as i32
}

/// What a programmatic scroll moves: a document's viewport or a scroll container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scroller {
    Viewport(NodeId),
    Element(NodeId),
}

impl Scroller {
    /// The document (for a viewport) or the element: its scroll events' target.
    pub fn key(self) -> NodeId {
        match self {
            Scroller::Viewport(d) | Scroller::Element(d) => d,
        }
    }
}

/// A smooth scroll in progress (CSSOM View §4 "perform a scroll": the scrolling box's position is
/// updated over time, in a user-agent-defined way: here an ease-in-out curve over
/// [`SMOOTH_SCROLL_MS`]).
pub(crate) struct SmoothScroll {
    pub scroller: Scroller,
    pub from: Point,
    pub to: Point,
    /// The page time (ms) of its first frame.
    pub start: Option<f64>,
    /// The promises the scroll methods returned, settled when it completes or is interrupted.
    pub promises: Vec<JsPromise>,
}

/// How long a smooth scroll takes.
const SMOOTH_SCROLL_MS: f64 = 250.0;

/// What `scrollTop` and friends act on.
enum ScrollSubject {
    Viewport,
    Element(NodeId),
    Nothing,
}

/// What is at a point of a document's viewport.
pub(crate) struct ViewHit {
    pub element: NodeId,
    /// For a replaced element: the point from its content box.
    pub content: Option<Point>,
    /// Whether the point is on text, not just in `element`.
    pub text: bool,
}

impl Engine {
    /// CSSOM View §6.1 `getClientRects()`: the border boxes of `el`'s fragments relative to the
    /// viewport.
    pub(crate) fn view_client_rects(&self, el: NodeId) -> Vec<geometry::Rect> {
        // CSSOM View §6.1: an element with an SVG layout box (an SVG element other than an
        // outermost `svg`) has one rectangle, its bounding box transformed to the viewport.
        if let Some(rect) = self.svg_client_rect(el) {
            return rect.into_iter().collect();
        }
        self.with_layout(el, |r, st| {
            let dom = self.dom();
            let mut shift = scroll_shift(r, st, &dom, el);
            // (css-position-3 §3.4.1: sticky boxes at their offset position)
            let sticky = r.sticky_offset(el, st.viewport, st.scroll, &st.element_scroll);
            shift.x -= sticky.x;
            shift.y -= sticky.y;
            r.client_rects(el)
                .into_iter()
                .map(|b| {
                    geometry::Rect::read_only(
                        (b.x - shift.x) as f64,
                        (b.y - shift.y) as f64,
                        b.width as f64,
                        b.height as f64,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
    }

    /// CSSOM View §6.1 `getBoundingClientRect()`.
    pub(crate) fn view_bounding_rect(&self, el: NodeId) -> geometry::Rect {
        bounding_rect(self.view_client_rects(el))
    }

    /// CSSOM View §6.2 `Range.getClientRects()`: the border boxes of the elements the range selects
    /// whose parent it does not select, and the parts of the text boxes of the text nodes it
    /// selects or partially selects within it (one zero-width box for a range collapsed in text),
    /// relative to the viewport; nothing for a range that is not in a document.
    pub(crate) fn view_range_client_rects(
        &self,
        start: (NodeId, u32),
        end: (NodeId, u32),
    ) -> Vec<geometry::Rect> {
        let doc = {
            let dom = self.dom();
            if !dom.is_alive(start.0) || !dom.is_alive(end.0) {
                return Vec::new();
            }
            let root = dom.root(start.0);
            if !dom.is_document(root) {
                return Vec::new();
            }
            root
        };
        self.with_layout(doc, |r, st| {
            let dom = self.dom();
            let mut out = Vec::new();
            let mut push = |rects: Vec<Rect>, shift: Point| {
                for b in rects {
                    out.push(geometry::Rect::read_only(
                        (b.x - shift.x) as f64,
                        (b.y - shift.y) as f64,
                        b.width as f64,
                        b.height as f64,
                    ));
                }
            };
            let is_data = |n: NodeId| {
                matches!(
                    dom.data(n),
                    snowlark_dom::NodeData::Text(_)
                        | snowlark_dom::NodeData::Comment(_)
                        | snowlark_dom::NodeData::ProcessingInstruction { .. }
                )
            };
            let text_part = |n: NodeId, a: u32, b: u32, push: &mut dyn FnMut(Vec<Rect>, Point)| {
                let s = dom.character_data(n).unwrap_or("");
                let (a, b) = (utf16_to_byte(s, a), utf16_to_byte(s, b));
                if let Some(el) = dom.parent_element(n) {
                    let shift = scroll_shift(r, st, &dom, el);
                    push(r.text_range_rects(n, a as u32, b as u32), shift);
                }
            };
            // A range collapsed in a text node: the caret's box.
            if start == end {
                if dom.is_text(start.0) {
                    text_part(start.0, start.1, end.1, &mut push);
                }
                return out;
            }
            // (selected: after the start and before the end, so an inclusive ancestor of neither)
            let selected = |n: NodeId| {
                !dom.is_inclusive_ancestor_of(n, start.0) && !dom.is_inclusive_ancestor_of(n, end.0)
            };
            let first = if is_data(start.0) {
                Some(start.0)
            } else {
                dom.child_at(start.0, start.1 as usize)
                    .or_else(|| dom.next_skipping_children(start.0, None))
            };
            let stop = if is_data(end.0) {
                None
            } else {
                dom.child_at(end.0, end.1 as usize)
                    .or_else(|| dom.next_skipping_children(end.0, None))
            };
            let mut cur = first;
            while let Some(n) = cur {
                if Some(n) == stop {
                    break;
                }
                if dom.is_text(n) {
                    let len = dom
                        .character_data(n)
                        .map_or(0, |s| s.encode_utf16().count() as u32);
                    let a = if n == start.0 { start.1 } else { 0 };
                    let b = if n == end.0 { end.1 } else { len };
                    if a < b {
                        text_part(n, a, b, &mut push);
                    }
                } else if dom.is_element(n)
                    && selected(n)
                    && !dom.parent_element(n).is_some_and(selected)
                {
                    let shift = scroll_shift(r, st, &dom, n);
                    push(r.client_rects(n), shift);
                }
                if is_data(end.0) && n == end.0 {
                    break;
                }
                cur = dom.next_in_tree_order(n, None);
            }
            out
        })
        .unwrap_or_default()
    }

    /// CSSOM View §6.2 `Range.getBoundingClientRect()`.
    pub(crate) fn view_range_bounding_rect(
        &self,
        start: (NodeId, u32),
        end: (NodeId, u32),
    ) -> geometry::Rect {
        bounding_rect(self.view_range_client_rects(start, end))
    }
}

/// The UTF-8 byte offset of a UTF-16 offset in `s` (clamped to its end).
fn utf16_to_byte(s: &str, utf16: u32) -> usize {
    let mut n = 0u32;
    for (i, c) in s.char_indices() {
        if n >= utf16 {
            return i;
        }
        n += c.len_utf16() as u32;
    }
    s.len()
}

/// CSSOM View §6.1/§6.2 `getBoundingClientRect()` of a list of client rects: the union of those
/// whose width or height is not zero (a `<br>`'s 0-wide box counts), else the first one, else a
/// zero rect.
pub(crate) fn bounding_rect(rects: Vec<geometry::Rect>) -> geometry::Rect {
    let Some(first) = rects.first().copied() else {
        return geometry::Rect::new(0.0, 0.0, 0.0, 0.0);
    };
    let sized: Vec<&geometry::Rect> = rects
        .iter()
        .filter(|r| r.width != 0.0 || r.height != 0.0)
        .collect();
    if sized.is_empty() {
        return geometry::Rect {
            read_only: false,
            ..first
        };
    }
    let left = sized.iter().map(|r| r.x).fold(f64::INFINITY, f64::min);
    let top = sized.iter().map(|r| r.y).fold(f64::INFINITY, f64::min);
    let right = sized
        .iter()
        .map(|r| r.x + r.width)
        .fold(f64::NEG_INFINITY, f64::max);
    let bottom = sized
        .iter()
        .map(|r| r.y + r.height)
        .fold(f64::NEG_INFINITY, f64::max);
    geometry::Rect::new(left, top, right - left, bottom - top)
}

impl Engine {
    /// CSSOM View §7 `offsetParent`.
    pub(crate) fn view_offset_parent(&self, el: NodeId) -> Option<NodeId> {
        self.with_layout(el, |r, _| {
            let dom = self.dom();
            let doc = dom.node_document(el);
            if r.box_rects(el).is_none() && r.client_rects(el).is_empty() {
                return None;
            }
            if Some(el) == dom.document_element(doc)
                || dom.is_html_element_named(el, &local_name!("body"))
            {
                return None;
            }
            let style = r.element_style(el)?;
            // The boxes up the flat tree. One in a shadow tree that is not one of the element's
            // own trees is never the answer (closed-shadow-hidden, csswg-drafts#159) but still
            // decides whether the walk is inside a fixed box. Inside one, only a box that contains
            // fixed descendants ends the walk; none means null.
            let mut in_fixed = style.position() == Position::Fixed;
            let mut a = r.flat_parent(&dom, el);
            while let Some(p) = a {
                a = r.flat_parent(&dom, p);
                let Some(ps) = r.element_style(p) else {
                    continue;
                };
                if snowlark_layout::Style::display(&ps) == snowlark_layout::style::Display::Contents
                {
                    continue;
                }
                let contains_fixed = ps.establishes_fixed_cb();
                let fixed = ps.position() == Position::Fixed;
                if !dom.is_shadow_including_inclusive_ancestor_of(dom.root(p), el) {
                    if contains_fixed {
                        in_fixed = false;
                    } else if fixed {
                        in_fixed = true;
                    }
                    continue;
                }
                if in_fixed {
                    if contains_fixed {
                        return Some(p);
                    }
                } else {
                    let table_part = ["td", "th", "table"]
                        .iter()
                        .any(|t| dom.is_html_element_named(p, &snowlark_dom::LocalName::from(*t)));
                    // (a table cell or table is the offset parent of an element that is not
                    // positioned itself)
                    if ps.position() != Position::Static
                        || contains_fixed
                        || dom.is_html_element_named(p, &local_name!("body"))
                        || (table_part && style.position() == Position::Static)
                    {
                        return Some(p);
                    }
                }
                in_fixed |= fixed;
            }
            None
        })
        .flatten()
    }

    /// CSSOM View §7 `offsetTop`, `offsetLeft`, `offsetWidth`, `offsetHeight`.
    pub(crate) fn view_offset(&self, el: NodeId) -> (i32, i32, i32, i32) {
        let parent = self.view_offset_parent(el);
        self.with_layout(el, |r, st| {
            // (css-position-3 §3.4.1: sticky boxes at their offset position)
            let sticky = |n: NodeId| r.sticky_offset(n, st.viewport, st.scroll, &st.element_scroll);
            let dom = self.dom();
            // (layout boxes: transforms do not apply to `offset*`)
            let rects = r.layout_rects(el);
            let Some(first) = r
                .box_rects(el)
                .map(|g| g.border_box)
                .or(rects.first().copied())
            else {
                return (0, 0, 0, 0);
            };
            // The bounding box of all fragments for the size.
            let bounds = rects.iter().fold(first, |a, b| a.union(b));
            // (a box snaps from its block-start edge, the right one where blocks flow leftwards)
            let flipped = dom
                .parent(el)
                .and_then(|p| r.element_style(p))
                .is_some_and(|s| {
                    matches!(
                        s.writing_mode(),
                        WritingMode::VerticalRl | WritingMode::SidewaysRl
                    )
                });
            let w = if flipped {
                let from_right = |x: f32| (0.5 - x).floor() as i32;
                from_right(bounds.x) - from_right(bounds.right())
            } else {
                snap(bounds.right()) - snap(bounds.x)
            };
            let (w, h) = (w, snap(bounds.bottom()) - snap(bounds.y));
            let doc = dom.node_document(el);
            if dom.is_html_element_named(el, &local_name!("body"))
                || Some(el) == dom.document_element(doc)
            {
                return (0, 0, w, h);
            }
            let origin = match parent {
                None => Point::ZERO,
                Some(p) => {
                    // A static `body` offset parent measures from the initial containing block
                    // (as engines do; the padding edge of a positioned one otherwise).
                    let static_body = dom.is_html_element_named(p, &local_name!("body"))
                        && r.element_style(p)
                            .is_some_and(|s| s.position() == Position::Static);
                    match r.box_rects(p) {
                        Some(_) if static_body => Point::ZERO,
                        Some(g) => {
                            let d = sticky(p);
                            Point::new(g.padding_box.x + d.x, g.padding_box.y + d.y)
                        }
                        None => Point::ZERO,
                    }
                }
            };
            let d = sticky(el);
            (
                snap(first.y + d.y - origin.y),
                snap(first.x + d.x - origin.x),
                w,
                h,
            )
        })
        .unwrap_or((0, 0, 0, 0))
    }

    /// CSSOM View §6 `clientTop`, `clientLeft`, `clientWidth`, `clientHeight`.
    pub(crate) fn view_client(&self, el: NodeId) -> (i32, i32, i32, i32) {
        let own_scroll = self.is_potentially_scrollable_body(el);
        self.with_layout(el, |r, st| {
            let dom = self.dom();
            let doc = dom.node_document(el);
            let quirks = dom.quirks_mode(doc) == QuirksMode::Quirks;
            let root = Some(el) == dom.document_element(doc);
            let quirks_body = quirks && dom.body(doc) == Some(el) && !own_scroll;
            if root || quirks_body {
                // CSSOM View §6: the viewport's size excluding a rendered scroll bar (overlay
                // scroll bars take no room).
                if !root || !quirks {
                    let v = r.layout_viewport(st.viewport);
                    let (vw, vh) = (v.width, v.height);
                    let border = r.box_rects(el).map(|g| {
                        (
                            g.padding_box.y - g.border_box.y,
                            g.padding_box.x - g.border_box.x,
                        )
                    });
                    let (t, l) = border.unwrap_or((0.0, 0.0));
                    return (snap(t), snap(l), snap(vw), snap(vh));
                }
            }
            let Some(g) = r.box_rects(el) else {
                return (0, 0, 0, 0);
            };
            // (a replaced element with display: inline is an atomic inline, not an inline box)
            if !inline_replaced(&dom, el) && is_inline_box(r, el) {
                return (0, 0, 0, 0);
            }
            (
                snap(g.padding_box.y - g.border_box.y),
                snap(g.padding_box.x - g.border_box.x),
                snap(g.padding_box.width),
                snap(g.padding_box.height),
            )
        })
        .unwrap_or((0, 0, 0, 0))
    }

    /// CSSOM View §6 `scrollWidth`, `scrollHeight`.
    pub(crate) fn view_scroll_size(&self, el: NodeId) -> (i32, i32) {
        let own_scroll = self.is_potentially_scrollable_body(el);
        self.with_layout(el, |r, st| {
            let dom = self.dom();
            let doc = dom.node_document(el);
            let quirks = dom.quirks_mode(doc) == QuirksMode::Quirks;
            let root = Some(el) == dom.document_element(doc);
            if (root && !quirks) || (quirks && dom.body(doc) == Some(el) && !own_scroll) {
                let area = scrolling_area(r, st);
                return (snap(area.width), snap(area.height));
            }
            let Some(g) = r.geometry(el) else {
                return (0, 0);
            };
            // (a replaced element with display: inline is an atomic inline, which has a scrolling
            // area; an inline box has none)
            if is_inline_box(r, el) && !inline_replaced(&dom, el) {
                return (0, 0);
            }
            let area = g.scrollable_overflow;
            // (a textarea's lines are not fragments: `style.height = scrollHeight + 'px'` grows it)
            let text = dom
                .is_html_element_named(el, &local_name!("textarea"))
                .then(|| r.with_box_fragment(el, snowlark_layout::controls::textarea_scroll_height))
                .flatten()
                .flatten();
            (snap(area.width), snap(area.height.max(text.unwrap_or(0.0))))
        })
        .unwrap_or((0, 0))
    }

    /// Whether `el` is a quirks-mode document's body that is potentially scrollable (it then
    /// measures and scrolls itself rather than the viewport).
    fn is_potentially_scrollable_body(&self, el: NodeId) -> bool {
        let quirks_body = {
            let dom = self.dom();
            let doc = dom.node_document(el);
            dom.quirks_mode(doc) == QuirksMode::Quirks && dom.body(doc) == Some(el)
        };
        quirks_body && self.body_potentially_scrollable(el)
    }

    /// CSSOM View §5 "potentially scrollable" body: it has a box, and it is a scroll container of
    /// its own (the root's `overflow` is not `visible`, so the body's does not propagate to the
    /// viewport, and the body's is neither `visible` nor `clip`).
    pub(crate) fn body_potentially_scrollable(&self, body: NodeId) -> bool {
        let Some(root) = self.dom().document_element(self.dom().node_document(body)) else {
            return false;
        };
        self.with_layout(body, |r, _| {
            r.geometry(body).is_some()
                && r.element_style(root).is_some_and(|s| {
                    s.overflow_x() != Overflow::Visible || s.overflow_y() != Overflow::Visible
                })
                && r.element_style(body).is_some_and(|s| {
                    s.overflow_x().is_scroll_container() || s.overflow_y().is_scroll_container()
                })
        })
        .unwrap_or(false)
    }

    /// What `el.scrollTop` and friends act on (CSSOM View §6 "scrollTop" getter steps 1-7).
    fn scroll_subject(&self, el: NodeId) -> ScrollSubject {
        let dom = self.dom();
        let doc = dom.node_document(el);
        if !self.is_rendered_document(doc) {
            return ScrollSubject::Nothing;
        }
        let quirks = dom.quirks_mode(doc) == QuirksMode::Quirks;
        let root = Some(el) == dom.document_element(doc);
        if root {
            return if quirks {
                ScrollSubject::Nothing
            } else {
                ScrollSubject::Viewport
            };
        }
        if quirks && dom.body(doc) == Some(el) {
            drop(dom);
            // (a potentially scrollable body scrolls itself)
            return if self.body_potentially_scrollable(el) {
                ScrollSubject::Element(el)
            } else {
                ScrollSubject::Viewport
            };
        }
        ScrollSubject::Element(el)
    }

    /// `el.scrollTop` / `el.scrollLeft`.
    pub(crate) fn view_scroll_position(&self, el: NodeId) -> Point {
        match self.scroll_subject(el) {
            ScrollSubject::Viewport => self
                .with_node_renderer(el, |_, st| st.scroll)
                .unwrap_or(Point::ZERO),
            ScrollSubject::Element(e) => self
                .with_node_renderer(e, |_, st| st.element_scroll.get(&e).copied())
                .flatten()
                .unwrap_or(Point::ZERO),
            ScrollSubject::Nothing => Point::ZERO,
        }
    }

    /// Scrolls `el`'s subject (CSSOM View §6 "scroll an element") with `behavior`; `promise`, when
    /// given, is taken by a smooth scroll that starts (and settled when it ends), else left for
    /// the caller to resolve. `snap`: what the scroll means for a box that snaps.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn view_scroll_element_with(
        &self,
        cx: &mut Cx,
        el: NodeId,
        x: Option<f64>,
        y: Option<f64>,
        behavior: ScrollBehavior,
        promise: &mut Option<JsPromise>,
        snap: SnapIntent,
    ) {
        let scroller = match self.scroll_subject(el) {
            ScrollSubject::Viewport => Scroller::Viewport(self.dom().node_document(el)),
            ScrollSubject::Element(e) => Scroller::Element(e),
            ScrollSubject::Nothing => return,
        };
        self.perform_scroll(cx, scroller, x, y, behavior, promise, Some(snap));
    }

    /// The scroll position of a scroller.
    fn scroller_position(&self, s: Scroller) -> Point {
        match s {
            Scroller::Viewport(doc) => self
                .with_doc_renderer(doc, |_, st| st.scroll)
                .unwrap_or(Point::ZERO),
            Scroller::Element(e) => self
                .with_node_renderer(e, |_, st| st.element_scroll.get(&e).copied())
                .flatten()
                .unwrap_or(Point::ZERO),
        }
    }

    /// Moves a scroller at once (clamped); returns whether it moved.
    fn scroller_set(&self, s: Scroller, to: Point) -> bool {
        match s {
            Scroller::Viewport(doc) => {
                self.view_scroll_viewport_to(doc, Some(to.x as f64), Some(to.y as f64))
            }
            Scroller::Element(e) => self.scroll_container_to(e, to),
        }
    }

    /// Whether a scroll with `behavior` is smooth (CSSOM View §4: `auto` takes the
    /// `scroll-behavior` of the element, or of the root element for the viewport). `to_origin`: the
    /// scroll needs no layout, so only style is brought up to date (a scroll elsewhere lays out
    /// once here, for the clamp that follows too).
    fn scroll_is_smooth(&self, s: Scroller, behavior: ScrollBehavior, to_origin: bool) -> bool {
        match behavior {
            ScrollBehavior::Smooth => true,
            ScrollBehavior::Instant => false,
            ScrollBehavior::Auto => {
                let el = match s {
                    Scroller::Element(e) => Some(e),
                    Scroller::Viewport(doc) => self.dom().document_element(doc),
                };
                let smooth = |r: &Renderer, e: NodeId| {
                    r.element_style(e)
                        .is_some_and(|st| st.scroll_behavior_smooth())
                };
                el.and_then(|e| {
                    if to_origin {
                        self.with_style(e, |r| smooth(r, e))
                    } else {
                        self.with_layout(e, |r, _| smooth(r, e))
                    }
                })
                .unwrap_or(false)
            }
        }
    }

    /// Settles the promises of a scroll that ended (`interrupted`: another scroll of its box took
    /// over).
    fn settle_scroll(cx: &mut Cx, promises: Vec<JsPromise>, interrupted: bool) {
        for p in promises {
            p.resolve(cx, ScrollResult { interrupted });
        }
    }

    /// CSSOM View §4 "perform a scroll" of `s` to (`x`, `y`) (`None`: the axis keeps its
    /// position): a smooth scroll in progress on the box is interrupted; a smooth one starts an
    /// animation stepped at each rendering update, an instant one moves at once. A scroll that
    /// changed the position completes with a `scrollend` event. `snap`: what the scroll means, for a
    /// box that snaps (CSS Scroll Snap 1 §6.2); `None` for a position already snapped.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn perform_scroll(
        &self,
        cx: &mut Cx,
        s: Scroller,
        x: Option<f64>,
        y: Option<f64>,
        behavior: ScrollBehavior,
        promise: &mut Option<JsPromise>,
        snap: Option<SnapIntent>,
    ) {
        let key = s.key();
        // A smooth scroll to where one in progress is already going does not interrupt it (it
        // takes the promise too).
        let going = self
            .with_node_renderer(key, |_, st| {
                st.smooth.iter().find(|a| a.scroller == s).map(|a| a.to)
            })
            .flatten();
        let to_origin = x == Some(0.0) && y == Some(0.0);
        if let Some(going) = going.filter(|_| self.scroll_is_smooth(s, behavior, to_origin)) {
            let target = Point::new(
                x.map_or(going.x, |v| finite32(v as f32)),
                y.map_or(going.y, |v| finite32(v as f32)),
            );
            if self.clamped_target(s, target) == going {
                self.with_node_renderer(key, |_, st| {
                    if let Some(a) = st.smooth.iter_mut().find(|a| a.scroller == s) {
                        a.promises.extend(promise.take());
                    }
                });
                return;
            }
        }
        let interrupted = self
            .with_node_renderer(key, |_, st| {
                let (gone, keep): (Vec<SmoothScroll>, Vec<SmoothScroll>) =
                    std::mem::take(&mut st.smooth)
                        .into_iter()
                        .partition(|a| a.scroller == s);
                st.smooth = keep;
                gone
            })
            .unwrap_or_default();
        for a in interrupted {
            Self::settle_scroll(cx, a.promises, true);
        }
        let cur = self.scroller_position(s);
        let mut target = Point::new(
            x.map_or(cur.x, |v| finite32(v as f32)),
            y.map_or(cur.y, |v| finite32(v as f32)),
        );
        if let Some(intent) = snap {
            target = self.snapped_target(s, cur, target, (x.is_some(), y.is_some()), intent);
        }
        if !self.scroll_is_smooth(s, behavior, target == Point::ZERO) {
            if self.scroller_set(s, target) {
                self.queue_scrollend(key);
            }
            return;
        }
        // (the end position, as clamped: where the animation goes)
        let to = self.clamped_target(s, target);
        if to == cur {
            return;
        }
        let promises = promise.take().into_iter().collect();
        self.with_node_renderer(key, |_, st| {
            st.smooth.push(SmoothScroll {
                scroller: s,
                from: cur,
                to,
                start: None,
                promises,
            });
            st.dirty = true;
        });
    }

    /// Where a scroll to `target` would end, clamped to the box's scroll range.
    fn clamped_target(&self, s: Scroller, target: Point) -> Point {
        // (the origin is inside every scroll range: no layout to clamp it)
        if target == Point::ZERO {
            return Point::ZERO;
        }
        match s {
            Scroller::Viewport(doc) => self
                .with_layout(doc, |r, st| viewport_range(r, st))
                .map_or(Point::ZERO, |(min, max)| {
                    Point::new(target.x.clamp(min.x, max.x), target.y.clamp(min.y, max.y))
                }),
            Scroller::Element(e) => self
                .with_layout(e, |r, _| element_range(r, &self.dom(), e))
                .flatten()
                .map_or(Point::ZERO, |(min, max)| {
                    Point::new(target.x.clamp(min.x, max.x), target.y.clamp(min.y, max.y))
                }),
        }
    }

    /// Queues a `scrollend` event at a scroller's key for the next rendering update.
    fn queue_scrollend(&self, key: NodeId) {
        self.with_node_renderer(key, |_, st| {
            if !st.pending_scrollend.contains(&key) {
                st.pending_scrollend.push(key);
            }
        });
    }

    /// Moves the smooth scrolls of every rendered document to their positions at page time `now`
    /// (ms); those that reach their end complete.
    pub(crate) fn step_smooth_scrolls(&self, cx: &mut Cx, now: f64) {
        for doc in self.rendered_documents() {
            let running = self
                .with_doc_renderer(doc, |_, st| std::mem::take(&mut st.smooth))
                .unwrap_or_default();
            if running.is_empty() {
                continue;
            }
            let mut keep = Vec::new();
            for mut a in running {
                let start = *a.start.get_or_insert(now);
                let t = ((now - start) / SMOOTH_SCROLL_MS).clamp(0.0, 1.0) as f32;
                // (ease-in-out)
                let e = if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                };
                let p = Point::new(
                    a.from.x + (a.to.x - a.from.x) * e,
                    a.from.y + (a.to.y - a.from.y) * e,
                );
                self.scroller_set(a.scroller, p);
                if t >= 1.0 {
                    self.queue_scrollend(a.scroller.key());
                    Self::settle_scroll(cx, std::mem::take(&mut a.promises), false);
                } else {
                    keep.push(a);
                }
            }
            if !keep.is_empty() {
                self.with_doc_renderer(doc, |_, st| {
                    keep.append(&mut st.smooth);
                    st.smooth = keep;
                });
            }
        }
    }

    /// Moves a scroll container's scroll position, clamped to its range; a `scroll` event follows at
    /// the next rendering update. Returns whether it moved.
    pub fn scroll_container_to(&self, el: NodeId, target: Point) -> bool {
        // (a box at its origin scrolled to its origin stays: the origin is inside every scroll
        // range, so no layout is needed to know nothing moves)
        if target == Point::ZERO
            && self
                .with_node_renderer(el, |_, st| st.element_scroll.get(&el).copied())
                .flatten()
                .unwrap_or(Point::ZERO)
                == Point::ZERO
        {
            return false;
        }
        self.with_layout(el, |r, _| element_range(r, &self.dom(), el))
            .flatten()
            .is_some_and(|(min, max)| {
                let to = Point::new(
                    finite32(target.x).clamp(min.x, max.x),
                    finite32(target.y).clamp(min.y, max.y),
                );
                self.with_node_renderer(el, |r, st| {
                    let current = st.element_scroll.get(&el).copied().unwrap_or(Point::ZERO);
                    if current == to {
                        return false;
                    }
                    st.element_scroll.insert(el, to);
                    r.set_scroll(el, to);
                    if !st.pending_scroll.contains(&el) {
                        st.pending_scroll.push(el);
                    }
                    st.dirty = true;
                    st.needs_paint = true;
                    true
                })
                .unwrap_or(false)
            })
    }

    /// The user scrolled a scroll container by `delta` (wheel, scroll bar). Returns whether it moved.
    /// A frame element's node scrolls its document's viewport.
    pub fn scroll_container_by(&self, el: NodeId, delta: Point) -> bool {
        // (the window keeps scrollers between frames: the page may have removed this one)
        if !self.dom().is_alive(el) {
            return false;
        }
        let frame = {
            let doc = self.dom().node_document(el);
            self.rendered_frames_of(doc)
                .into_iter()
                .find(|(c, _)| *c == el)
        };
        if let Some((_, doc)) = frame {
            let at = self
                .with_doc_renderer(doc, |_, st| st.scroll)
                .unwrap_or(Point::ZERO);
            return self.view_scroll_viewport_to(
                doc,
                Some((at.x + delta.x) as f64),
                Some((at.y + delta.y) as f64),
            );
        }
        let at = self
            .with_node_renderer(el, |_, st| st.element_scroll.get(&el).copied())
            .flatten()
            .unwrap_or(Point::ZERO);
        self.scroll_container_to(el, Point::new(at.x + delta.x, at.y + delta.y))
    }

    /// CSSOM View §4 `window.scroll()`: scrolls a rendered document's viewport, clamped to the
    /// scrolling area (the top-level one's through the embedder). Returns whether it moved.
    pub(crate) fn view_scroll_viewport_to(
        &self,
        doc: NodeId,
        x: Option<f64>,
        y: Option<f64>,
    ) -> bool {
        // (the origin is inside every scroll range, so a scroll there needs no layout to clamp it)
        let to_origin = self.is_rendered_document(doc)
            && self.with_doc_renderer(doc, |_, st| {
                x.map_or(st.scroll.x, |v| finite32(v as f32)) == 0.0
                    && y.map_or(st.scroll.y, |v| finite32(v as f32)) == 0.0
            }) == Some(true);
        let (min, max) = if to_origin {
            (Point::ZERO, Point::ZERO)
        } else {
            let Some(range) = self.with_layout(doc, |r, st| viewport_range(r, st)) else {
                return false;
            };
            range
        };
        let top = self.rendered_top() == Some(doc);
        self.with_doc_renderer(doc, |_, st| {
            let to = Point::new(
                x.map_or(st.scroll.x, |v| finite32(v as f32))
                    .clamp(min.x, max.x),
                y.map_or(st.scroll.y, |v| finite32(v as f32))
                    .clamp(min.y, max.y),
            );
            if to == st.scroll {
                return false;
            }
            st.scroll = to;
            if top {
                st.scroll_request = Some(to);
            }
            if !st.pending_scroll.contains(&doc) {
                st.pending_scroll.push(doc);
            }
            st.dirty = true;
            st.needs_paint = true;
            true
        })
        .unwrap_or(false)
    }

    /// CSSOM View §6.1 `scrollIntoView()`: scrolls each scroll container ancestor and the viewport
    /// so that `el`'s border box is in view, aligned per `block` and `inline` ("start", "center",
    /// "end", "nearest"). The positions it scrolls to are whole device pixels, as Chrome's are
    /// (scroll offsets are integers there): the boxes brought into view paint on pixel edges.
    pub(crate) fn view_scroll_into_view(&self, el: NodeId, block: &str, inline: &str) {
        self.view_scroll_rect_into_view(el, None, block, inline);
    }

    /// [`Engine::view_scroll_into_view`] for `area` (unscrolled, in the coordinates of `el`'s
    /// boxes: a line of a match inside `el`) instead of `el`'s border box. The alignment `"find"`
    /// (the browser's find in page) leaves what is in view alone and centers what is not.
    pub(crate) fn view_scroll_rect_into_view(
        &self,
        el: NodeId,
        area: Option<Rect>,
        block: &str,
        inline: &str,
    ) {
        let dpr = self
            .render
            .borrow()
            .setup
            .as_ref()
            .map_or(1.0, |s| s.device_pixel_ratio)
            .max(1e-3);
        let device = |p: Point| Point::new((p.x * dpr).round() / dpr, (p.y * dpr).round() / dpr);
        let containers = self.scroll_containers_around(el);
        let align = |mode: &str, start: f32, size: f32, view_start: f32, view_size: f32| -> f32 {
            match mode {
                "start" => start,
                "end" => start + size - view_size,
                "center" => start + size / 2.0 - view_size / 2.0,
                "find" => {
                    if start >= view_start && start + size <= view_start + view_size {
                        view_start
                    } else {
                        start + size / 2.0 - view_size / 2.0
                    }
                }
                _ => {
                    // "nearest": no move when it is in view, else the closer edge.
                    if start >= view_start && start + size <= view_start + view_size {
                        view_start
                    } else if start < view_start || size > view_size {
                        start
                    } else {
                        start + size - view_size
                    }
                }
            }
        };
        // The physical alignment on each axis for a scrolling box with `style` (CSSOM View §6.1
        // "scroll an element into view" steps: the edges are the scrolling box's block-start and
        // inline-start ones): `start` and `end` swap on an axis whose start edge is the right or
        // bottom one.
        let physical = |style: Option<&snowlark_render::RenderStyle>| -> (&str, &str) {
            use snowlark_layout::style::{Direction, WritingMode};
            let (mode, rtl) = style.map_or((WritingMode::HorizontalTb, false), |s| {
                (s.writing_mode(), s.direction() == Direction::Rtl)
            });
            let (x_axis, x_flip, y_axis, y_flip) = match mode {
                WritingMode::HorizontalTb => (inline, rtl, block, false),
                WritingMode::VerticalRl | WritingMode::SidewaysRl => (block, true, inline, rtl),
                WritingMode::VerticalLr => (block, false, inline, rtl),
                WritingMode::SidewaysLr => (block, false, inline, !rtl),
            };
            let flip = |m: &'static str, f: bool| match (m, f) {
                ("start", true) => "end",
                ("end", true) => "start",
                (m, _) => m,
            };
            (flip(leak(x_axis), x_flip), flip(leak(y_axis), y_flip))
        };
        // The element's scroll snap area (CSS Scroll Snap 1 §5.2): its border box outset by its
        // `scroll-margin`.
        let snap_area = |r: &Renderer, b: Rect| -> Rect {
            match r.element_style(el) {
                Some(s) => Rect::from_ltrb(
                    b.x - s.scroll_margin(Side::Left),
                    b.y - s.scroll_margin(Side::Top),
                    b.right() + s.scroll_margin(Side::Right),
                    b.bottom() + s.scroll_margin(Side::Bottom),
                ),
                None => b,
            }
        };
        // A scrolling box's `scroll-padding` (CSS Scroll Snap 1 §5.1), insetting the region the
        // element is aligned in: (left, top, right, bottom).
        let padding = |s: Option<&snowlark_render::RenderStyle>, port: Rect| -> [f32; 4] {
            match s {
                Some(s) => [
                    s.scroll_padding(Side::Left).resolve(port.width),
                    s.scroll_padding(Side::Top).resolve(port.height),
                    s.scroll_padding(Side::Right).resolve(port.width),
                    s.scroll_padding(Side::Bottom).resolve(port.height),
                ],
                None => [0.0; 4],
            }
        };
        for c in containers {
            let pos = self.with_layout(c, |r, st| {
                let g = r.geometry(el)?;
                let cg = r.geometry(c)?;
                let cur = st.element_scroll.get(&c).copied().unwrap_or(Point::ZERO);
                // The element's box in the container's scrolled content coordinates.
                let dom = self.dom();
                let mut inner = Point::ZERO;
                let mut a = r.flat_parent(&dom, el);
                while let Some(p) = a {
                    if p == c {
                        break;
                    }
                    if let Some(s) = st.element_scroll.get(&p) {
                        inner.x += s.x;
                        inner.y += s.y;
                    }
                    a = r.flat_parent(&dom, p);
                }
                let b = area.unwrap_or_else(|| snap_area(r, g.border_box));
                let pb = cg.padding_box;
                let cs = r.element_style(c);
                let [pl, pt, pr, pbm] = padding(cs.as_ref(), pb);
                let (ax, ay) = physical(cs.as_ref());
                let (x, y) = (b.x - pb.x - inner.x, b.y - pb.y - inner.y);
                Some(Point::new(
                    align(ax, x, b.width, cur.x + pl, pb.width - pl - pr) - pl,
                    align(ay, y, b.height, cur.y + pt, pb.height - pt - pbm) - pt,
                ))
            });
            if let Some(Some(p)) = pos {
                self.scroll_container_to(c, device(p));
            }
        }
        let target = self
            .with_layout(el, |r, st| {
                let dom = self.dom();
                let g = r.geometry(el)?;
                // A fixed-position box does not move with the viewport: scrolling it brings
                // nothing into view.
                if fixed_position_chain(r, &dom, el) {
                    return None;
                }
                let shift = scroll_shift(r, st, &dom, el);
                // In document coordinates: undo the containers' scrolling, keep the viewport's.
                let b = area.unwrap_or_else(|| snap_area(r, g.border_box));
                let (x, y) = (b.x - shift.x + st.scroll.x, b.y - shift.y + st.scroll.y);
                // (the viewport takes the root element's writing mode and `scroll-padding`)
                let root = dom.document_element(dom.node_document(el));
                let rs = root.and_then(|e| r.element_style(e));
                let v = st.viewport;
                let [pl, pt, pr, pbm] =
                    padding(rs.as_ref(), Rect::new(0.0, 0.0, v.width, v.height));
                let (ax, ay) = physical(rs.as_ref());
                Some(Point::new(
                    align(ax, x, b.width, st.scroll.x + pl, v.width - pl - pr) - pl,
                    align(ay, y, b.height, st.scroll.y + pt, v.height - pt - pbm) - pt,
                ))
            })
            .flatten();
        if let Some(p) = target.map(device) {
            let doc = self.dom().node_document(el);
            self.view_scroll_viewport_to(doc, Some(p.x as f64), Some(p.y as f64));
        }
    }

    /// The scroll containers around `el` (innermost first; the root element's scrolling is the
    /// viewport's).
    fn scroll_containers_around(&self, el: NodeId) -> Vec<NodeId> {
        self.with_layout(el, |r, _| {
            let dom = self.dom();
            let mut out = Vec::new();
            let mut a = r.flat_parent(&dom, el);
            while let Some(p) = a {
                if element_range(r, &dom, p).is_some()
                    && Some(p) != dom.document_element(dom.node_document(el))
                {
                    out.push(p);
                }
                a = r.flat_parent(&dom, p);
            }
            out
        })
        .unwrap_or_default()
    }

    /// `scrollIntoView()` with a `behavior` (CSSOM View §6.1): the positions each scrolling box
    /// ends at are those of an instant scroll; boxes that scroll smoothly go there from where they
    /// are over time. `promise` as for [`Engine::perform_scroll`].
    pub(crate) fn view_scroll_into_view_with(
        &self,
        cx: &mut Cx,
        el: NodeId,
        block: &str,
        inline: &str,
        behavior: ScrollBehavior,
        promise: &mut Option<JsPromise>,
    ) {
        let doc = self.dom().node_document(el);
        let boxes: Vec<Scroller> = self
            .scroll_containers_around(el)
            .into_iter()
            .map(Scroller::Element)
            .chain(std::iter::once(Scroller::Viewport(doc)))
            .collect();
        // (the end positions of smooth scrolls in progress: where those boxes are going)
        let old: Vec<(Scroller, Point)> = boxes
            .iter()
            .map(|&b| (b, self.scroller_position(b)))
            .collect();
        self.view_scroll_into_view(el, block, inline);
        for (b, from) in old {
            let to = self.scroller_position(b);
            if to != from {
                self.scroller_set(b, from);
                self.perform_scroll(
                    cx,
                    b,
                    Some(to.x as f64),
                    Some(to.y as f64),
                    behavior,
                    promise,
                    Some(SnapIntent::End),
                );
            }
        }
    }

    /// CSSOM View §5.1 `elementsFromPoint()`: the elements under the viewport point, topmost first,
    /// ending with the root element. Empty outside the viewport.
    pub(crate) fn view_elements_from_point(&self, doc: NodeId, x: f64, y: f64) -> Vec<NodeId> {
        self.view_elements_from_point_in(doc, doc, x, y)
    }

    /// `elementsFromPoint()` of a document or shadow root `scope`: each element retargeted against
    /// it (an element in a shadow tree inside `scope`'s tree gives its host there).
    pub(crate) fn view_elements_from_point_in(
        &self,
        doc: NodeId,
        scope: NodeId,
        x: f64,
        y: f64,
    ) -> Vec<NodeId> {
        if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
            return Vec::new();
        }
        self.with_layout(doc, |r, st| {
            if x as f32 > st.viewport.width || y as f32 > st.viewport.height {
                return Vec::new();
            }
            let dom = self.dom();
            // (shadow trees are retargeted against the scope: to their hosts in its tree)
            let retargeted = |e: NodeId| {
                if dom.node_document(e) == doc {
                    self.retarget(&dom, e, scope)
                } else {
                    e
                }
            };
            let mut out: Vec<NodeId> = Vec::new();
            if let Some(root) = r.fragments() {
                let options = PaintOptions {
                    viewport: st.viewport,
                    viewport_scroll: st.scroll,
                    scroll: Some(&st.element_scroll),
                    embeds: None,
                    selection: None,
                    find: None,
                    layers: false,
                    device_scale: 1.0,
                    composited: None,
                    animations: None,
                };
                // (CSSOM View §5: the elements of every box the point is in, topmost first; an
                // ancestor whose box does not hold the point is not one of them)
                let hits = snowlark_layout::hit_test_all_with(
                    root,
                    Point::new(x as f32, y as f32),
                    &options,
                );
                let push = |n: NodeId, out: &mut Vec<NodeId>| {
                    let mut n = Some(n);
                    // Text: its parent element; shadow trees: retargeted to the host.
                    while let Some(id) = n {
                        if dom.is_element(id) {
                            break;
                        }
                        n = layout_parent(&dom, id);
                    }
                    if let Some(e) = n.map(retargeted) {
                        if !out.contains(&e) {
                            out.push(e);
                        }
                    }
                };
                for hit in &hits {
                    // SVG 2 §16.5: in an outermost `svg` element, the graphics element hit and its
                    // ancestors up to it come first.
                    if let Some(p) = hit.content_point {
                        if let Some(target) = svg_hit(r, hit.node, p) {
                            let mut c = Some(target);
                            while let Some(id) = c {
                                if id == hit.node {
                                    break;
                                }
                                push(id, &mut out);
                                // (out of a `use` element's tree, on through the `use`)
                                c = dom.parent_element(id).or_else(|| dom.host(dom.root(id)));
                            }
                        }
                    }
                    push(hit.node, &mut out);
                }
            }
            if let Some(root) = dom.document_element(doc) {
                if out.last() != Some(&root) {
                    out.retain(|&e| e != root);
                    out.push(root);
                }
            }
            out
        })
        .unwrap_or_default()
    }

    /// The topmost element at a viewport point of a rendered document, for input: the innermost one,
    /// in whichever shadow tree it is (a text node gives its flat-tree parent, a slot for slotted
    /// text), since dispatch retargets for each listener (DOM §2.9), and, for a replaced element,
    /// the point relative to its content box.
    pub(crate) fn view_hit(&self, doc: NodeId, p: Point) -> Option<ViewHit> {
        if !p.x.is_finite() || !p.y.is_finite() || p.x < 0.0 || p.y < 0.0 {
            return None;
        }
        self.with_layout(doc, |r, st| {
            if p.x > st.viewport.width || p.y > st.viewport.height {
                return None;
            }
            let dom = self.dom();
            let options = PaintOptions {
                viewport: st.viewport,
                viewport_scroll: st.scroll,
                scroll: Some(&st.element_scroll),
                embeds: None,
                selection: None,
                find: None,
                layers: false,
                device_scale: 1.0,
                composited: None,
                animations: None,
            };
            let hit = snowlark_layout::hit_test_with(r.fragments()?, p, &options);
            let Some(hit) = hit else {
                return dom.document_element(doc).map(|element| ViewHit {
                    element,
                    content: None,
                    text: false,
                });
            };
            let mut n = Some(hit.node);
            while let Some(id) = n {
                if dom.is_element(id) {
                    break;
                }
                n = self.flat_parent(id);
            }
            let e = n?;
            self.html.borrow_mut().default_summary_hit = hit.default_summary.then_some(e);
            // SVG 2 §16.5: inside an outermost `svg` element, the graphics element hit.
            if let Some(p) = hit.content_point.filter(|_| e == hit.node) {
                if let Some(target) = svg_hit(r, e, p) {
                    return Some(ViewHit {
                        element: target,
                        content: None,
                        text: false,
                    });
                }
                // HTML §4.8.14.2: over an image with an image map, the area under the point.
                if let Some(area) = super::image_map::area_at(&dom, e, p) {
                    return Some(ViewHit {
                        element: area,
                        content: None,
                        text: false,
                    });
                }
            }
            Some(ViewHit {
                element: e,
                content: (e == hit.node).then_some(hit.content_point).flatten(),
                text: dom.is_text(hit.node),
            })
        })
        .flatten()
    }

    /// The scroll containers of the top-level document's last layout (for the embedder: scroll
    /// bars and wheel scrolling), with their padding boxes in document coordinates, scroll positions
    /// and ranges.
    pub fn scroll_containers(&self) -> Vec<(NodeId, Rect, Point, Point)> {
        match self.rendered_top() {
            Some(doc) => self.scroll_containers_of(doc),
            None => Vec::new(),
        }
    }

    /// The scroll containers the user can scroll (with the wheel, and that show scroll bars), with
    /// their padding boxes, scroll positions and the furthest the user can scroll each axis (CSS
    /// Overflow 3 §3): an axis whose `overflow` is `hidden` is scrollable by script only, overflow by
    /// less than a device pixel does not count, and a container that is not visible is left out.
    pub fn user_scroll_containers(&self) -> Vec<(NodeId, Rect, Point, Point)> {
        match self.rendered_top() {
            Some(doc) => self.user_scroll_containers_of(doc),
            None => Vec::new(),
        }
    }

    /// [`Engine::user_scroll_containers`] of a rendered document.
    pub(crate) fn user_scroll_containers_of(
        &self,
        doc: NodeId,
    ) -> Vec<(NodeId, Rect, Point, Point)> {
        let dpr = self
            .render
            .borrow()
            .setup
            .as_ref()
            .map_or(1.0, |s| s.device_pixel_ratio)
            .max(1e-3);
        let mut out = self.scroll_containers_of(doc);
        out.retain_mut(|(n, _, _, max)| {
            let style = self
                .with_doc_renderer(doc, |r, _| {
                    r.element_style(*n)
                        .map(|s| (s.overflow_x(), s.overflow_y(), s.visibility()))
                })
                .flatten();
            let Some((ox, oy, visibility)) = style else {
                return false;
            };
            let user = |o: Overflow, m: f32| {
                if matches!(o, Overflow::Auto | Overflow::Scroll) && m * dpr >= 1.0 {
                    m
                } else {
                    0.0
                }
            };
            *max = Point::new(user(ox, max.x), user(oy, max.y));
            visibility == snowlark_layout::style::Visibility::Visible
                && (max.x > 0.0 || max.y > 0.0)
        });
        out
    }

    /// [`Engine::scroll_containers`] of a rendered document.
    pub(crate) fn scroll_containers_of(&self, doc: NodeId) -> Vec<(NodeId, Rect, Point, Point)> {
        self.with_doc_renderer(doc, |r, st| {
            // One walk: each scroll container's first fragment gives its geometry (looking each
            // up again walked the tree once per container).
            let mut nodes: Vec<(NodeId, BoxGeometry)> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            if let Some(root) = r.fragments() {
                root.walk(&mut |f, at| {
                    if let Some(b) = f.as_box() {
                        if let Some(n) = b.node {
                            if b.flags & fragment_flags::SCROLL_CONTAINER != 0 && seen.insert(n) {
                                nodes.push((n, BoxGeometry::of(f, at)));
                            }
                        }
                    }
                });
            }
            let dom = self.dom();
            // (positions from the start of the range: an origin at the right or bottom edge
            // makes the range negative, which the embedder's scroll bars need not know)
            nodes
                .into_iter()
                .filter_map(|(n, g)| {
                    let (min, max) = element_range_at(r, &dom, n, &g)?;
                    let at = st.element_scroll.get(&n).copied().unwrap_or(Point::ZERO);
                    Some((
                        n,
                        g.padding_box,
                        Point::new(at.x - min.x, at.y - min.y),
                        Point::new(max.x - min.x, max.y - min.y),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
    }
}

fn finite32(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// Web IDL `unrestricted double` scroll coordinates: non-finite values are 0 (CSSOM View
/// "normalize non-finite values").
pub(crate) fn finite(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// A `ScrollLogicalPosition` as the alignment keyword.
pub(crate) fn logical(p: snowlark_bindings::generated::ScrollLogicalPosition) -> &'static str {
    use snowlark_bindings::generated::ScrollLogicalPosition as P;
    match p {
        P::Start => "start",
        P::Center => "center",
        P::End => "end",
        P::Nearest => "nearest",
    }
}

/// `el`'s padding box with its and its ancestors' transforms applied, scrolled and at its sticky
/// offset, relative to its document's viewport: where its content clip is, placed as
/// [`Engine::view_client_rects`] places its border box (Blink maps each clip from its own transform
/// space, `GeometryMapper::LocalToAncestorVisualRect`; for a transform that is not a translation or
/// scale this is the bounding box of the mapped clip).
fn clip_box(r: &Renderer, st: &RenderState, dom: &Dom, el: NodeId) -> Option<geometry::Rect> {
    let b = r.transformed_padding_box(el)?;
    let mut shift = scroll_shift(r, st, dom, el);
    let sticky = r.sticky_offset(el, st.viewport, st.scroll, &st.element_scroll);
    shift.x -= sticky.x;
    shift.y -= sticky.y;
    Some(geometry::Rect::read_only(
        (b.x - shift.x) as f64,
        (b.y - shift.y) as f64,
        b.width as f64,
        b.height as f64,
    ))
}

/// A content clip on an element's containing block chain (Intersection Observer "compute the
/// intersection" step 3): the clipping box's padding box in its document's viewport coordinates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClipStep {
    pub rect: geometry::Rect,
    /// Whether the clipping box is a scroll container (its clip takes the scroll margin).
    pub scroll_container: bool,
}

impl Engine {
    /// An element's padding box relative to its document's viewport, and whether it has a content
    /// clip (`overflow` other than `visible` in either axis). `None` when it has no box.
    /// An element's padding box where it is on screen (its and its ancestors' transforms applied,
    /// as for its client rects), relative to its document's viewport, and whether it has a
    /// content clip: an observer root's clip. `None` when it has no box.
    pub(crate) fn view_clip_box(&self, el: NodeId) -> Option<(geometry::Rect, bool)> {
        self.with_layout(el, |r, st| {
            let clips = r
                .element_style(el)
                .is_some_and(|s| s.overflow_x().clips() || s.overflow_y().clips());
            Some((clip_box(r, st, &self.dom(), el)?, clips))
        })
        .flatten()
    }

    pub(crate) fn view_padding_box(&self, el: NodeId) -> Option<(geometry::Rect, bool)> {
        self.with_layout(el, |r, st| {
            let dom = self.dom();
            let g = r.geometry(el)?;
            let clips = r
                .element_style(el)
                .is_some_and(|s| s.overflow_x().clips() || s.overflow_y().clips());
            let shift = scroll_shift(r, st, &dom, el);
            let b = g.padding_box;
            Some((
                geometry::Rect::read_only(
                    (b.x - shift.x) as f64,
                    (b.y - shift.y) as f64,
                    b.width as f64,
                    b.height as f64,
                ),
                clips,
            ))
        })
        .flatten()
    }

    /// An element's content box relative to its document's viewport (a frame's viewport origin in
    /// its parent's coordinates).
    pub(crate) fn view_content_box(&self, el: NodeId) -> Option<geometry::Rect> {
        self.with_layout(el, |r, st| {
            let dom = self.dom();
            let g = r.geometry(el)?;
            let shift = scroll_shift(r, st, &dom, el);
            let b = g.content_box;
            Some(geometry::Rect::read_only(
                (b.x - shift.x) as f64,
                (b.y - shift.y) as f64,
                b.width as f64,
                b.height as f64,
            ))
        })
        .flatten()
    }

    /// The content clips on `el`'s containing block chain within its document, from `el` outward,
    /// stopping before `stop`; and whether `stop` is on that chain. An absolutely positioned box
    /// skips the boxes up to its containing block (positioned, or transformed, filtered or
    /// contained), a fixed-position box those up to one that is transformed, filtered or contained,
    /// and without one leaves its document's boxes (its containing block is the viewport), as
    /// [`scroll_shift`] finds them. The root element and the body propagate
    /// their overflow to the viewport, so they do not clip here. `None` when `el` has no layout.
    pub(crate) fn view_containing_clips(
        &self,
        el: NodeId,
        stop: Option<NodeId>,
    ) -> Option<(Vec<ClipStep>, bool)> {
        self.with_layout(el, |r, st| {
            let dom = self.dom();
            let doc = dom.node_document(el);
            let viewport_boxes = [dom.document_element(doc), dom.body(doc)];
            let mut out = Vec::new();
            let mut position = r.element_style(el).map(|s| s.position());
            let mut n = r.flat_parent(&dom, el);
            while let Some(id) = n {
                let style = r.element_style(id);
                let pos = style
                    .as_ref()
                    .map(|s| s.position())
                    .unwrap_or(Position::Static);
                // (a transformed box, or one with a filter or containment, is the containing
                // block of its positioned descendants, static or not: CSS Transforms 1 §2)
                let establishes = style.as_ref().is_some_and(|s| s.establishes_fixed_cb());
                let in_chain = match position {
                    Some(Position::Absolute) => pos != Position::Static || establishes,
                    Some(Position::Fixed) => establishes,
                    _ => true,
                };
                if in_chain {
                    // (an element root is an ancestor on the chain, or the target is not
                    // intersecting: Intersection Observer §3.2.8)
                    if Some(id) == stop {
                        return (out, true);
                    }
                    position = Some(pos);
                    let clips = style
                        .as_ref()
                        .is_some_and(|s| s.overflow_x().clips() || s.overflow_y().clips());
                    if clips && !viewport_boxes.contains(&Some(id)) {
                        if let Some(rect) = clip_box(r, st, &dom, id) {
                            let scroll_container = style.as_ref().is_some_and(|s| {
                                s.overflow_x().is_scroll_container()
                                    || s.overflow_y().is_scroll_container()
                            });
                            out.push(ClipStep {
                                rect,
                                scroll_container,
                            });
                        }
                    }
                }
                n = r.flat_parent(&dom, id);
            }
            (out, stop.is_none())
        })
    }

    /// A rendered document's viewport size.
    pub(crate) fn view_viewport_size(&self, doc: NodeId) -> Option<(f64, f64)> {
        self.with_doc_renderer(doc, |_, st| {
            (st.viewport.width as f64, st.viewport.height as f64)
        })
    }
}

/// What Resize Observer measures of an element's box: physical sizes in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BoxSizes {
    pub border: (f64, f64),
    pub content: (f64, f64),
    /// The padding box's top-left inset from the content box (`contentRect`'s x and y).
    pub padding_left: f64,
    pub padding_top: f64,
    /// Whether the writing mode is vertical (inline sizes are heights).
    pub vertical: bool,
    pub device_pixel_ratio: f64,
}

impl Engine {
    /// An element's border and content box sizes from the last layout; `None` when it has no box.
    pub(crate) fn view_box_sizes(&self, el: NodeId) -> Option<BoxSizes> {
        let dpr = self
            .render
            .borrow()
            .setup
            .as_ref()
            .map_or(1.0, |s| s.device_pixel_ratio as f64);
        self.with_layout(el, |r, _| {
            // A non-replaced inline box has no content area of its own size: it is 0×0, as for an
            // element without a box.
            if is_inline_box(r, el) && !is_replaced_element(&self.dom(), el) {
                return None;
            }
            let g = r.geometry(el)?;
            let vertical = r.element_style(el).is_some_and(|s| {
                !matches!(
                    s.writing_mode(),
                    snowlark_layout::style::WritingMode::HorizontalTb
                )
            });
            Some(BoxSizes {
                border: (g.border_box.width as f64, g.border_box.height as f64),
                content: (g.content_box.width as f64, g.content_box.height as f64),
                padding_left: (g.content_box.x - g.padding_box.x) as f64,
                padding_top: (g.content_box.y - g.padding_box.y) as f64,
                vertical,
                device_pixel_ratio: dpr,
            })
        })
        .flatten()
    }
}

/// Whether `el` is a replaced element whose `display: inline` box is still atomic (HTML §15.4).
fn is_replaced_element(dom: &Dom, el: NodeId) -> bool {
    use snowlark_dom::ns;
    let Some(e) = dom.element(el) else {
        return false;
    };
    let name = e.name();
    if name.ns == ns!(svg) {
        return name.local == local_name!("svg");
    }
    name.ns == ns!(html)
        && matches!(
            name.local,
            local_name!("img")
                | local_name!("video")
                | local_name!("canvas")
                | local_name!("iframe")
                | local_name!("embed")
                | local_name!("object")
                | local_name!("input")
                | local_name!("textarea")
                | local_name!("select")
                | local_name!("audio")
        )
}

// ----- CSS Scroll Snap 1 -------------------------------------------------------------------------

/// How a scroll means to end, for snapping (CSS Scroll Snap 1 §6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapIntent {
    /// An intended end position (`scrollTo()`, `scrollTop`, `scrollIntoView()`).
    End,
    /// A direction and a distance from where the box is (`scrollBy()`, a wheel notch).
    By,
    /// A direction only (an arrow key, Page Down).
    Direction,
}

/// The snap container's axes as its writing mode lays them out.
fn snap_axes(s: &snowlark_render::RenderStyle) -> snowlark_layout::scroll_snap::Axes {
    use snowlark_layout::scroll_snap::Axes;
    use snowlark_layout::style::Direction;
    let rtl = s.direction() == Direction::Rtl;
    let (horizontal, block_reversed, inline_reversed) = match s.writing_mode() {
        WritingMode::HorizontalTb => (true, false, rtl),
        WritingMode::VerticalRl | WritingMode::SidewaysRl => (false, true, rtl),
        WritingMode::VerticalLr => (false, false, rtl),
        WritingMode::SidewaysLr => (false, false, !rtl),
    };
    Axes {
        horizontal,
        block_reversed,
        inline_reversed,
    }
}

impl Engine {
    /// The snap container `s` is, with its snap areas (CSS Scroll Snap 1 §5), when its
    /// `scroll-snap-type` is not `none` (the root element's is the viewport's).
    fn snap_container(&self, s: Scroller) -> Option<snowlark_layout::scroll_snap::SnapContainer> {
        use snowlark_layout::scroll_snap::{Align, SnapArea, SnapContainer};
        let (doc, c) = match s {
            Scroller::Viewport(doc) => (doc, self.dom().document_element(doc)?),
            Scroller::Element(e) => (self.dom().node_document(e), e),
        };
        self.with_layout(c, |r, st| {
            let cs = r.element_style(c)?;
            let (axis, strictness) = cs.scroll_snap_type()?;
            let axes = snap_axes(&cs);
            let dom = self.dom();
            // (the scrollport, where the container's content coordinates start, its scroll range)
            let (port, origin, (min, max)) = match s {
                Scroller::Viewport(_) => {
                    let v = r.layout_viewport(st.viewport);
                    (
                        Rect::new(0.0, 0.0, v.width, v.height),
                        Point::ZERO,
                        viewport_range(r, st),
                    )
                }
                Scroller::Element(e) => {
                    let g = r.geometry(e)?;
                    let pb = g.padding_box;
                    let range = element_range_at(r, &dom, e, &g)?;
                    (
                        Rect::new(0.0, 0.0, pb.width, pb.height),
                        Point::new(pb.x, pb.y),
                        range,
                    )
                }
            };
            // (§5.1.3: the snapport is the scrollport inset by `scroll-padding`)
            let pad = |side: Side, len: f32| cs.scroll_padding(side).resolve(len);
            let snapport = Rect::from_ltrb(
                pad(Side::Left, port.width),
                pad(Side::Top, port.height),
                port.width - pad(Side::Right, port.width),
                port.height - pad(Side::Bottom, port.height),
            );
            let container = match s {
                Scroller::Element(e) => Some(e),
                Scroller::Viewport(_) => None,
            };
            let root = dom.document_element(doc);
            let mut areas = Vec::new();
            for el in dom.shadow_including_descendants(doc) {
                if !dom.is_element(el) {
                    continue;
                }
                let Some(es) = r.element_style(el) else {
                    continue;
                };
                let (block, inline) = es.scroll_snap_align();
                if block == Align::None && inline == Align::None {
                    continue;
                }
                // (§5.2: a box's snap container is its nearest scroll container ancestor)
                let mut a = r.flat_parent(&dom, el);
                let nearest = loop {
                    match a {
                        None => break None,
                        Some(p) if Some(p) == root => break None,
                        Some(p) if element_range(r, &dom, p).is_some() => break Some(p),
                        Some(p) => a = r.flat_parent(&dom, p),
                    }
                };
                if nearest != container {
                    continue;
                }
                let Some(g) = r.geometry(el) else {
                    continue;
                };
                // (a fixed box does not move with the viewport it would snap)
                if container.is_none() && fixed_position_chain(r, &dom, el) {
                    continue;
                }
                // (§5.2.1: the snap area is the border box outset by `scroll-margin`)
                let b = g.border_box;
                let rect = Rect::from_ltrb(
                    b.x - es.scroll_margin(Side::Left) - origin.x,
                    b.y - es.scroll_margin(Side::Top) - origin.y,
                    b.right() + es.scroll_margin(Side::Right) - origin.x,
                    b.bottom() + es.scroll_margin(Side::Bottom) - origin.y,
                );
                let (align_x, align_y) = Align::physical(block, inline, axes);
                areas.push(SnapArea {
                    rect,
                    align_x,
                    align_y,
                    stop_always: es.scroll_snap_stop_always(),
                });
            }
            Some(SnapContainer {
                axis: axis.physical(axes),
                strictness,
                snapport,
                min,
                max,
                areas,
            })
        })
        .flatten()
    }

    /// Where a scroll of `s` from `from` toward `to` comes to rest (CSS Scroll Snap 1 §6.2): `to`
    /// unless `s` is a snap container. `(x, y)`: the axes an end position names.
    pub(crate) fn snapped_target(
        &self,
        s: Scroller,
        from: Point,
        to: Point,
        (x, y): (bool, bool),
        intent: SnapIntent,
    ) -> Point {
        use snowlark_layout::scroll_snap::Intent;
        // (style alone says whether the box snaps: a box that does not needs no layout here)
        let el = match s {
            Scroller::Viewport(doc) => self.dom().document_element(doc),
            Scroller::Element(e) => Some(e),
        };
        if !el.is_some_and(|e| self.has_snap_type(e)) {
            return to;
        }
        let Some(c) = self.snap_container(s) else {
            return to;
        };
        let d = Point::new(to.x - from.x, to.y - from.y);
        let (sx, sy) = c.snap(match intent {
            SnapIntent::End => Intent::End { to, x, y },
            SnapIntent::By => Intent::Displacement { from, delta: d },
            SnapIntent::Direction => Intent::Direction { from, step: d },
        });
        Point::new(sx.unwrap_or(to.x), sy.unwrap_or(to.y))
    }

    /// The scroller a user scroll of `node` moves: a frame element's document's viewport, a
    /// document's viewport, else the scroll container.
    fn user_scroller(&self, node: NodeId) -> Scroller {
        if self.dom().is_document(node) {
            return Scroller::Viewport(node);
        }
        let doc = self.dom().node_document(node);
        match self
            .rendered_frames_of(doc)
            .into_iter()
            .find(|(c, _)| *c == node)
        {
            Some((_, d)) => Scroller::Viewport(d),
            None => Scroller::Element(node),
        }
    }

    /// Whether `node`'s scrolling snaps (`scroll-snap-type` is not `none`): a scroll container's,
    /// or, for a frame element or a document, its document's viewport's.
    pub fn scroll_snaps(&self, node: NodeId) -> bool {
        let el = match self.user_scroller(node) {
            Scroller::Viewport(doc) => self.dom().document_element(doc),
            Scroller::Element(e) => Some(e),
        };
        el.is_some_and(|e| self.has_snap_type(e))
    }

    /// Whether `el`'s `scroll-snap-type` is not `none` (style only: no layout).
    fn has_snap_type(&self, el: NodeId) -> bool {
        self.with_style(el, |r| {
            r.element_style(el)
                .is_some_and(|s| s.scroll_snap_type().is_some())
        })
        .unwrap_or(false)
    }

    /// The user turned the mouse wheel by a notch (`key`: pressed an arrow or page key) to scroll
    /// `node` (a scroll container, a frame element or a document for its viewport, `None` for the
    /// top-level viewport) by `delta`: the box goes smoothly to the snap position the scroll
    /// chooses (CSS Scroll Snap 1 §6.2), or by `delta` when it does not snap. A notch while the
    /// box is still going to a snap position counts from that position. Returns whether it moves.
    pub fn snap_scroll_by(
        &self,
        cx: &mut Cx,
        node: Option<NodeId>,
        delta: Point,
        key: bool,
    ) -> bool {
        let s = match node {
            None => match self.rendered_top() {
                Some(doc) => Scroller::Viewport(doc),
                None => return false,
            },
            Some(n) if !self.dom().is_alive(n) => return false,
            Some(n) => self.user_scroller(n),
        };
        let going = self
            .with_node_renderer(s.key(), |_, st| {
                st.smooth.iter().find(|a| a.scroller == s).map(|a| a.to)
            })
            .flatten();
        let before = self.scroller_position(s);
        let from = going.unwrap_or(before);
        let to = Point::new(from.x + delta.x, from.y + delta.y);
        let intent = if key {
            SnapIntent::Direction
        } else {
            SnapIntent::By
        };
        let target = self.snapped_target(s, from, to, (true, true), intent);
        self.perform_scroll(
            cx,
            s,
            Some(target.x as f64),
            Some(target.y as f64),
            ScrollBehavior::Smooth,
            &mut None,
            None,
        );
        self.clamped_target(s, target) != before
    }
}
