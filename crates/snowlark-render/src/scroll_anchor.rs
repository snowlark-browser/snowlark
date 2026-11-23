// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Scroll Anchoring 1: when content above what the reader sees changes size, the scroll
//! position of the viewport, or of a scroll container, moves with it so that the visible content
//! stays where it is.
//!
//! Before a layout that may move boxes, an anchor node is selected for each scroller scrolled away
//! from its origin, from the last layout's fragments (§2.1, the anchor node selection algorithm),
//! and its block-start edge recorded relative to the scroller; after the layout, the edge's
//! movement is the scroller's adjustment (§2.2), unless a suppression trigger (§2.2.2) fired in
//! between. The anchor is selected again before each such layout: a scroll that is not an
//! adjustment never sees a stale one (§2.1.1, invalidation). The embedder applies the adjustments
//! ([`crate::renderer::Renderer::take_scroll_adjustment`],
//! [`crate::renderer::Renderer::take_element_scroll_adjustments`]).
//!
//! A scroll container inside a scroller is walked into with its own scroll position: what it
//! shows is where the scroller's reader sees it.
//!
//! The focused editable element is a priority candidate (§2.1): when it is visible in the
//! scroller, the walk starts from its box (the nearest of its ancestors' that is not inline).
//!
//! Not done yet: the find-in-page match as a priority candidate, and the limit re-snapping puts
//! on adjustments.

use std::collections::HashMap;

use servo_arc::Arc as ServoArc;
use snowlark_dom::NodeId;
use snowlark_layout::fragment_flags as ff;
use snowlark_layout::style::{Side, Style as _, WritingMode};
use snowlark_layout::{Fragment, FragmentKind, LayoutTree};
use snowlark_paint::{Point, Rect, Size};
use style::properties::ComputedValues;
use style::values::computed::PositionProperty;

use crate::adapter::RenderStyle;

/// Elements from the scroller's (the root element for the viewport) down, each with its computed
/// values at the time.
type Path = Vec<(NodeId, ServoArc<ComputedValues>)>;

/// The scroll positions of scroll containers, by element.
pub(crate) type Scrolls = HashMap<NodeId, Point>;

/// An element's computed values now (the last layout's boxes may have older ones).
pub(crate) type Styles<'a> = &'a dyn Fn(NodeId) -> Option<&'a ComputedValues>;

/// The scroll anchoring state of a document's scrollers.
#[derive(Default)]
pub(crate) struct Anchoring {
    /// Where the viewport is scrolled (as the embedder last said).
    pub scroll: Point,
    /// The focused element when it is editable: the priority candidate (§2.1).
    pub priority: Option<NodeId>,
    /// The anchors selected before the layout under way, one per scroller at most.
    queued: Vec<Queued>,
    /// The viewport's adjustments since the embedder last took them (physical CSS px).
    adjustment: Option<Point>,
    /// Scroll containers' adjustments since the embedder last took them.
    element_adjustments: Vec<(NodeId, Point)>,
}

/// The scroller's block axis, and which edge of a box is its block-start edge on it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum BlockEdge {
    /// Horizontal writing modes: the top edge.
    Top,
    /// `vertical-lr`, `sideways-lr`: the left edge.
    Left,
    /// `vertical-rl`, `sideways-rl`: the right edge.
    Right,
}

impl BlockEdge {
    fn of(mode: WritingMode) -> BlockEdge {
        match mode {
            WritingMode::HorizontalTb => BlockEdge::Top,
            WritingMode::VerticalLr | WritingMode::SidewaysLr => BlockEdge::Left,
            WritingMode::VerticalRl | WritingMode::SidewaysRl => BlockEdge::Right,
        }
    }

    /// The edge of the rectangles that bound a node's fragments.
    fn edge(self, rects: &[Rect]) -> Option<f32> {
        let mut it = rects.iter();
        let first = it.next()?;
        let pick = |r: &Rect| match self {
            BlockEdge::Top => r.y,
            BlockEdge::Left => r.x,
            BlockEdge::Right => r.right(),
        };
        let mut v = pick(first);
        for r in it {
            v = match self {
                BlockEdge::Right => v.max(pick(r)),
                _ => v.min(pick(r)),
            };
        }
        Some(v)
    }

    /// The scroll position on the block axis.
    fn scroll(self, scroll: Point) -> f32 {
        match self {
            BlockEdge::Top => scroll.y,
            _ => scroll.x,
        }
    }

    /// A movement on the block axis, as a physical offset.
    fn offset(self, d: f32) -> Point {
        match self {
            BlockEdge::Top => Point::new(0.0, d),
            _ => Point::new(d, 0.0),
        }
    }
}

struct Queued {
    /// The scroll container the anchor is for; `None` for the viewport.
    scroller: Option<NodeId>,
    node: NodeId,
    axis: BlockEdge,
    /// The anchor's block-start edge relative to the scrollport, rounded to device pixels.
    edge: f32,
    /// The scroller's scroll position it was selected at.
    scroll: Point,
    /// The scroll containers between the scroller and the anchor: their scroll positions move
    /// the anchor in the scroller.
    between: Vec<NodeId>,
    /// The elements from the scroller down to the anchor (itself included when it is one), with
    /// their computed values when it was selected (§2.2.2, the first trigger).
    path: Path,
    /// A suppression trigger fired since it was selected.
    suppressed: bool,
}

/// What examining a candidate (§2.1, the candidate examination algorithm) says to do next.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Walk {
    /// Not a candidate, nor its descendants (excluded or fully clipped).
    Skip,
    /// Not a candidate itself (a non-atomic inline, an anonymous box, a line): look at its
    /// children.
    Continue,
    /// Partially visible: the anchor unless one of its descendants is a better one.
    Constrain,
    /// Fully visible: the anchor.
    Return,
}

impl Anchoring {
    /// The anchor node selection algorithm for each scroller scrolled away from its origin, run
    /// before a layout that may move boxes, over the last layout's fragments (`root`, laid out in
    /// a viewport scrollport of `scrollport`; `scrolls` the scroll containers' positions; `styles`
    /// the computed values now, for `overflow-anchor`). An anchor already selected for this layout
    /// stays while its scroller has not scrolled.
    #[allow(clippy::too_many_arguments)]
    pub fn before_layout(
        &mut self,
        root: &Fragment<RenderStyle>,
        layout: &LayoutTree<RenderStyle>,
        scrollport: Size,
        dpr: f32,
        scrolls: &Scrolls,
        priority: &[NodeId],
        styles: Styles,
    ) {
        let viewport = self.scroll;
        self.queued.retain(|q| match q.scroller {
            None => q.scroll == viewport,
            Some(el) => scrolls.get(&el).copied().unwrap_or(Point::ZERO) == q.scroll,
        });
        if !self.queued.iter().any(|q| q.scroller.is_none()) {
            let s = (scrolls, priority, styles);
            self.select_viewport(root, layout, scrollport, dpr, s);
        }
        // (step 2, and the third suppression trigger: a scroller at its origin has none)
        let mut wanted: Vec<NodeId> = scrolls
            .iter()
            .filter(|(n, p)| {
                **p != Point::ZERO && !self.queued.iter().any(|q| q.scroller == Some(**n))
            })
            .map(|(n, _)| *n)
            .collect();
        if wanted.is_empty() {
            return;
        }
        let mut found = Vec::new();
        scroll_containers(root, Point::ZERO, &mut wanted, &mut found);
        for (el, f, at) in found {
            self.select_element(el, (f, at), layout, dpr, (scrolls, priority, styles));
        }
    }

    fn select_viewport(
        &mut self,
        root: &Fragment<RenderStyle>,
        layout: &LayoutTree<RenderStyle>,
        scrollport: Size,
        dpr: f32,
        state: (&Scrolls, &[NodeId], Styles),
    ) {
        let axis = BlockEdge::of(
            layout
                .principal_writing_mode()
                .map_or(WritingMode::HorizontalTb, |m| m.0),
        );
        if axis.scroll(self.scroll) == 0.0 {
            return;
        }
        let visible = Rect::new(
            self.scroll.x,
            self.scroll.y,
            scrollport.width,
            scrollport.height,
        );
        // The optimal viewing region: the scrollport less the root's `scroll-padding` (CSS Scroll
        // Snap 1 §4.1: the viewport's is the root element's).
        let visible = match root_style(root) {
            Some(s) => less_scroll_padding(visible, s),
            None => visible,
        };
        let scroll = self.scroll;
        let at = (root, Point::ZERO);
        self.select(None, at, visible, axis, scroll, layout, dpr, state);
    }

    /// Selects the anchor of the scroll container `el`, whose box fragment `f` is at `at`.
    fn select_element(
        &mut self,
        el: NodeId,
        (f, at): (&Fragment<RenderStyle>, Point),
        layout: &LayoutTree<RenderStyle>,
        dpr: f32,
        state: (&Scrolls, &[NodeId], Styles),
    ) {
        let (scrolls, _, styles) = state;
        let Some(b) = f.as_box() else {
            return;
        };
        // (§2.1 step 1: a scroller whose `overflow-anchor` is `none` selects no anchor)
        if excluded(b, styles) {
            return;
        }
        let axis = BlockEdge::of(b.style.writing_mode());
        let scroll = scrolls.get(&el).copied().unwrap_or(Point::ZERO);
        if axis.scroll(scroll) == 0.0 {
            return;
        }
        // Its scrollport is its padding box, moved by its scroll position over its content.
        let pad = f.padding_rect();
        let visible = Rect::new(
            at.x + pad.x + scroll.x,
            at.y + pad.y + scroll.y,
            pad.width,
            pad.height,
        );
        let visible = less_scroll_padding(visible, &b.style);
        self.select(Some(el), (f, at), visible, axis, scroll, layout, dpr, state);
    }

    #[allow(clippy::too_many_arguments)]
    fn select(
        &mut self,
        scroller: Option<NodeId>,
        (f, at): (&Fragment<RenderStyle>, Point),
        visible: Rect,
        axis: BlockEdge,
        scroll: Point,
        layout: &LayoutTree<RenderStyle>,
        dpr: f32,
        (scrolls, priority, styles): (&Scrolls, &[NodeId], Styles),
    ) {
        let mut finder = Finder {
            visible,
            scrolls,
            styles,
            path: Vec::new(),
            between: Vec::new(),
            found: None,
        };
        if !finder.walk_from_priority(f, at, priority) {
            finder.found = None;
            finder.find(f, at, true);
        }
        let Some((node, path, between)) = finder.found else {
            return;
        };
        let Some(edge) = relative_edge(axis, layout, scroller, node, &between, scrolls) else {
            return;
        };
        self.queued.push(Queued {
            scroller,
            node,
            axis,
            edge: device_round(edge - axis.scroll(scroll), dpr),
            scroll,
            between,
            path,
            suppressed: false,
        });
    }

    /// Whether an anchor is selected for the layout under way.
    pub fn is_queued(&self) -> bool {
        !self.queued.is_empty()
    }

    /// The second suppression trigger: an element in the scroller became or stopped being
    /// absolutely positioned (`old` and `new` its computed values before and after).
    /// `old` is `None` for an element that had no box (`display: none`): it becomes absolutely
    /// positioned if it is now. `inside` says whether the element is inside a scroll container.
    pub fn position_changed(
        &mut self,
        old: Option<&ComputedValues>,
        new: &ComputedValues,
        inside: impl Fn(NodeId) -> bool,
    ) {
        if old.is_some_and(out_of_flow) == out_of_flow(new) {
            return;
        }
        for q in &mut self.queued {
            if q.scroller.is_none_or(&inside) {
                q.suppressed = true;
            }
        }
    }

    /// The scroll adjustments (§2.2) after the layout: each anchor's movement, unless a
    /// suppression trigger fired or the anchor has no box any more. `styles` gives an element's
    /// computed values now.
    pub fn after_layout<'a>(
        &mut self,
        layout: &LayoutTree<RenderStyle>,
        styles: impl Fn(NodeId) -> Option<&'a ComputedValues>,
        dpr: f32,
        scrolls: &Scrolls,
    ) {
        let queued = std::mem::take(&mut self.queued);
        // Inner scrollers first: an outer scroller's anchor inside an inner one is where the inner
        // one's adjustment leaves it.
        let depth: Vec<usize> = queued
            .iter()
            .map(|q| {
                let s = q.scroller;
                queued
                    .iter()
                    .filter(|o| s.is_some_and(|s| o.between.contains(&s)))
                    .count()
            })
            .collect();
        let mut order: Vec<usize> = (0..queued.len()).collect();
        order.sort_by_key(|i| std::cmp::Reverse(depth[*i]));
        let mut now = scrolls.clone();
        for i in order {
            let q = &queued[i];
            if q.suppressed {
                continue;
            }
            let Some(edge) = relative_edge(q.axis, layout, q.scroller, q.node, &q.between, &now)
            else {
                continue;
            };
            let delta = device_round(edge - q.axis.scroll(q.scroll), dpr) - q.edge;
            if delta == 0.0 {
                continue;
            }
            // The first suppression trigger: a change on the path from the anchor to the scroller.
            let changed = q
                .path
                .iter()
                .any(|(n, old)| styles(*n).is_some_and(|new| disabling_change(old, new)));
            if changed {
                continue;
            }
            let d = q.axis.offset(delta);
            if let Some(el) = q.scroller {
                let p = now.entry(el).or_insert(Point::ZERO);
                *p = Point::new(p.x + d.x, p.y + d.y);
            }
            let a = match q.scroller {
                None => self.adjustment.get_or_insert(Point::ZERO),
                Some(el) => {
                    let i = match self.element_adjustments.iter().position(|(n, _)| *n == el) {
                        Some(i) => i,
                        None => {
                            self.element_adjustments.push((el, Point::ZERO));
                            self.element_adjustments.len() - 1
                        }
                    };
                    &mut self.element_adjustments[i].1
                }
            };
            *a = Point::new(a.x + d.x, a.y + d.y);
        }
    }

    /// The viewport's adjustments since the last call.
    pub fn take_adjustment(&mut self) -> Option<Point> {
        self.adjustment.take()
    }

    /// The scroll containers' adjustments since the last call.
    pub fn take_element_adjustments(&mut self) -> Vec<(NodeId, Point)> {
        std::mem::take(&mut self.element_adjustments)
    }
}

/// The box fragments of the scroll containers in `wanted` (taken out of it as they are found),
/// with where they are (root coordinates, unscrolled).
fn scroll_containers<'f>(
    f: &'f Fragment<RenderStyle>,
    at: Point,
    wanted: &mut Vec<NodeId>,
    found: &mut Vec<(NodeId, &'f Fragment<RenderStyle>, Point)>,
) {
    if let FragmentKind::Box(b) = &f.kind {
        if b.flags & ff::SCROLL_CONTAINER != 0 && b.pseudo.is_none() {
            if let Some(i) = b.node.and_then(|n| wanted.iter().position(|w| *w == n)) {
                found.push((wanted.swap_remove(i), f, at));
            }
        }
    }
    for c in &f.children {
        if wanted.is_empty() {
            return;
        }
        let p = Point::new(at.x + c.offset.x, at.y + c.offset.y);
        scroll_containers(&c.fragment, p, wanted, found);
    }
}

/// The block-start edge of `node` relative to its scroller's box (root coordinates for the
/// viewport), with the scroll containers between them scrolled as they are.
fn relative_edge(
    axis: BlockEdge,
    layout: &LayoutTree<RenderStyle>,
    scroller: Option<NodeId>,
    node: NodeId,
    between: &[NodeId],
    scrolls: &Scrolls,
) -> Option<f32> {
    let mut e = axis.edge(&layout.layout_rects(node))?;
    for b in between {
        e -= axis.scroll(scrolls.get(b).copied().unwrap_or(Point::ZERO));
    }
    if let Some(s) = scroller {
        e -= axis.edge(&layout.layout_rects(s))?;
    }
    Some(e)
}

/// The optimal viewing region (§2.1): `r` less the scroller's `scroll-padding`.
fn less_scroll_padding(r: Rect, s: &RenderStyle) -> Rect {
    let (w, h) = (r.width, r.height);
    let top = s.scroll_padding(Side::Top).resolve(h).max(0.0);
    let right = s.scroll_padding(Side::Right).resolve(w).max(0.0);
    let bottom = s.scroll_padding(Side::Bottom).resolve(h).max(0.0);
    let left = s.scroll_padding(Side::Left).resolve(w).max(0.0);
    Rect::new(
        r.x + left,
        r.y + top,
        (w - left - right).max(0.0),
        (h - top - bottom).max(0.0),
    )
}

/// Excluded subtrees (§2.1): `overflow-anchor: none` (as the element's style is now), fixed and
/// sticky boxes.
fn excluded(b: &snowlark_layout::BoxFragment<RenderStyle>, styles: Styles) -> bool {
    use style::computed_values::overflow_anchor::T as OverflowAnchor;
    let now = b.node.filter(|_| b.pseudo.is_none()).and_then(styles);
    let values = now.unwrap_or_else(|| &**b.style.values());
    values.get_box().overflow_anchor == OverflowAnchor::None
        || b.flags & (ff::FIXED | ff::STICKY) != 0
}

struct Finder<'s, 'y> {
    visible: Rect,
    scrolls: &'s Scrolls,
    styles: Styles<'y>,
    /// The elements from the scroller down to the fragment being examined.
    path: Path,
    /// The scroll containers from the scroller down to the fragment being examined.
    between: Vec<NodeId>,
    found: Option<(NodeId, Path, Vec<NodeId>)>,
}

/// Where the priority candidate's box is in the scroller's walk.
struct Located<'f> {
    /// Its index in the priority chain (0: the focused element itself).
    k: usize,
    fragment: &'f Fragment<RenderStyle>,
    at: Point,
    path: Path,
    between: Vec<NodeId>,
    /// It or a box between it and the scroller is excluded (§2.1).
    excluded: bool,
}

impl Finder<'_, '_> {
    /// The priority candidate step (§2.1): `chain` is the focused editable element and its
    /// ancestors, nearest first; the candidate is the first of them with a box that is not
    /// inline. When it is in the scroller (`f` at `at`), not excluded and visible, the walk starts
    /// from it, and the anchor is it or one of its descendants. Returns whether it did.
    fn walk_from_priority(
        &mut self,
        f: &Fragment<RenderStyle>,
        at: Point,
        chain: &[NodeId],
    ) -> bool {
        if chain.is_empty() {
            return false;
        }
        let mut best = None;
        self.locate(f, at, true, chain, false, &mut best);
        let Some(c) = best else {
            return false;
        };
        if c.excluded {
            return false;
        }
        self.path = c.path;
        self.between = c.between;
        let status = self.find(c.fragment, c.at, false);
        self.path.clear();
        self.between.clear();
        matches!(status, Walk::Return | Walk::Constrain)
    }

    /// Finds the box of the nearest element of `chain` (each an ancestor of the one before, so
    /// they lie on one path down the fragment tree: only that path is walked).
    fn locate<'f>(
        &mut self,
        f: &'f Fragment<RenderStyle>,
        at: Point,
        top: bool,
        chain: &[NodeId],
        excluded_above: bool,
        best: &mut Option<Located<'f>>,
    ) {
        let b = match &f.kind {
            FragmentKind::Text(_) => return,
            FragmentKind::Line { .. } => None,
            FragmentKind::Box(b) => Some(b),
        };
        let node = b.and_then(|b| b.node);
        let k = node.and_then(|n| chain.iter().position(|c| *c == n));
        if !top && node.is_some() && k.is_none() {
            return;
        }
        let excluded = excluded_above || (!top && b.is_some_and(|b| excluded(b, self.styles)));
        if let (Some(k), Some(b)) = (k, b) {
            let candidate = !top && b.pseudo.is_none() && b.flags & ff::INLINE_BOX == 0;
            if candidate && best.as_ref().is_none_or(|l| k < l.k) {
                *best = Some(Located {
                    k,
                    fragment: f,
                    at,
                    path: self.path.clone(),
                    between: self.between.clone(),
                    excluded,
                });
                if k == 0 {
                    return;
                }
            }
        }
        let (shift, pushed, nested) = self.enter(f, top);
        for c in &f.children {
            let p = Point::new(at.x + c.offset.x - shift.x, at.y + c.offset.y - shift.y);
            self.locate(&c.fragment, p, false, chain, excluded, best);
        }
        self.leave(pushed, nested);
    }

    /// Going into `f`'s children: its element onto the path, and for a scroll container inside
    /// the scroller, the shift its scroll position gives its content.
    fn enter(&mut self, f: &Fragment<RenderStyle>, top: bool) -> (Point, bool, bool) {
        let FragmentKind::Box(b) = &f.kind else {
            return (Point::ZERO, false, false);
        };
        let mut shift = Point::ZERO;
        let mut nested = false;
        if !top
            && b.flags & ff::SCROLL_CONTAINER != 0
            && b.flags & (ff::ROOT | ff::VIEWPORT_OVERFLOW) == 0
        {
            if let Some(n) = b.node {
                shift = self.scrolls.get(&n).copied().unwrap_or(Point::ZERO);
                self.between.push(n);
                nested = true;
            }
        }
        let pushed = match b.node {
            Some(n) if b.pseudo.is_none() => {
                self.path.push((n, b.style.values().clone()));
                true
            }
            _ => false,
        };
        (shift, pushed, nested)
    }

    fn leave(&mut self, pushed: bool, nested: bool) {
        if pushed {
            self.path.pop();
        }
        if nested {
            self.between.pop();
        }
    }

    /// The candidate examination algorithm for the fragment `f` at `at` (the scroller's
    /// coordinates, unscrolled); `top` for the scroller's own box.
    fn find(&mut self, f: &Fragment<RenderStyle>, at: Point, top: bool) -> Walk {
        let status = self.examine(f, at, top);
        let element = match &f.kind {
            FragmentKind::Box(b) if b.pseudo.is_none() => b.node.map(|n| (n, &b.style)),
            _ => None,
        };
        if matches!(status, Walk::Return | Walk::Constrain) {
            let node = f.node().expect("a candidate has a node");
            let mut path = self.path.clone();
            if let Some((n, s)) = element {
                path.push((n, s.values().clone()));
            }
            self.found = Some((node, path, self.between.clone()));
        }
        if matches!(status, Walk::Return | Walk::Skip) {
            return status;
        }
        // (a scroll container inside the scroller shows its content moved by its scroll position)
        let (shift, pushed, nested) = self.enter(f, top);
        let mut out = status;
        for c in f.visible_children() {
            let p = Point::new(at.x + c.offset.x - shift.x, at.y + c.offset.y - shift.y);
            let s = self.find(&c.fragment, p, false);
            if matches!(s, Walk::Return | Walk::Constrain) {
                out = s;
                break;
            }
        }
        self.leave(pushed, nested);
        out
    }

    fn examine(&self, f: &Fragment<RenderStyle>, at: Point, top: bool) -> Walk {
        let rect = match &f.kind {
            FragmentKind::Line { .. } => return Walk::Continue,
            FragmentKind::Text(t) => {
                if t.node.is_none() {
                    return Walk::Skip;
                }
                Rect::new(at.x, at.y, f.size.width, f.size.height)
            }
            FragmentKind::Box(b) => {
                // (the scroller's own box: the initial containing block for the viewport)
                if top {
                    return Walk::Continue;
                }
                if excluded(b, self.styles) {
                    return Walk::Skip;
                }
                // (non-atomic inlines are not candidates, nor are boxes the DOM does not have)
                if b.flags & ff::INLINE_BOX != 0 || b.node.is_none() || b.pseudo.is_some() {
                    return Walk::Continue;
                }
                let mut r = Rect::new(at.x, at.y, f.size.width, f.size.height);
                // The scroll anchoring bounding rect is the box's scrollable overflow rectangle:
                // a box that does not clip includes what overflows it.
                if b.flags & (ff::CLIP_X | ff::CLIP_Y) == 0 {
                    let bottom = (at.y + f.overflow.bottom()).max(r.bottom());
                    r.height = bottom - r.y;
                }
                r
            }
        };
        let v = &self.visible;
        let occupies = rect.width > 0.0 && rect.height > 0.0;
        let intersects =
            rect.x < v.right() && rect.right() > v.x && rect.y < v.bottom() && rect.bottom() > v.y;
        if !occupies || !intersects {
            return Walk::Skip;
        }
        let contained = rect.x >= v.x
            && rect.right() <= v.right()
            && rect.y >= v.y
            && rect.bottom() <= v.bottom();
        if contained {
            Walk::Return
        } else {
            Walk::Constrain
        }
    }
}

/// The root element's style, from the box the initial containing block holds.
fn root_style(root: &Fragment<RenderStyle>) -> Option<&RenderStyle> {
    fn is_root(f: &Fragment<RenderStyle>) -> Option<&snowlark_layout::BoxFragment<RenderStyle>> {
        f.as_box().filter(|b| b.flags & ff::ROOT != 0)
    }
    if let Some(b) = is_root(root) {
        return Some(&b.style);
    }
    root.children
        .iter()
        .find_map(|c| is_root(&c.fragment))
        .map(|b| &b.style)
}

/// Rounds to device pixels, where the anchor is painted (a fractional move of the anchor moves
/// what is painted by whole pixels).
fn device_round(v: f32, dpr: f32) -> f32 {
    let dpr = dpr.max(1e-3);
    (v * dpr).round() / dpr
}

fn out_of_flow(v: &ComputedValues) -> bool {
    matches!(
        v.get_box().position,
        PositionProperty::Absolute | PositionProperty::Fixed
    )
}

/// The first suppression trigger (§2.2.2): a change of `top`, `left`, `right`, `bottom`, the
/// margins, the paddings, the sizes and their limits, `position` or the transform.
fn disabling_change(old: &ComputedValues, new: &ComputedValues) -> bool {
    if std::ptr::eq(old, new) {
        return false;
    }
    let (op, np) = (old.get_position(), new.get_position());
    let (om, nm) = (old.get_margin(), new.get_margin());
    let (opd, npd) = (old.get_padding(), new.get_padding());
    let (ob, nb) = (old.get_box(), new.get_box());
    op.top != np.top
        || op.right != np.right
        || op.bottom != np.bottom
        || op.left != np.left
        || op.width != np.width
        || op.height != np.height
        || op.min_width != np.min_width
        || op.max_width != np.max_width
        || op.min_height != np.min_height
        || op.max_height != np.max_height
        || om.margin_top != nm.margin_top
        || om.margin_right != nm.margin_right
        || om.margin_bottom != nm.margin_bottom
        || om.margin_left != nm.margin_left
        || opd.padding_top != npd.padding_top
        || opd.padding_right != npd.padding_right
        || opd.padding_bottom != npd.padding_bottom
        || opd.padding_left != npd.padding_left
        || ob.position != nb.position
        || ob.transform != nb.transform
        || ob.translate != nb.translate
        || ob.rotate != nb.rotate
        || ob.scale != nb.scale
}
