// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Scroll Anchoring 1 for the viewport: when content above what the reader sees changes size,
//! the viewport's scroll position moves with it so that the visible content stays where it is.
//!
//! Before a layout that may move boxes, an anchor node is selected from the last layout's
//! fragments (§2.1, the anchor node selection algorithm) and its block-start edge recorded; after
//! the layout, the edge's movement is the scroll adjustment (§2.2), unless a suppression trigger
//! (§2.2.2) fired in between. The anchor is selected again before each such layout: a scroll that
//! is not an adjustment never sees a stale one (§2.1.1, invalidation). The embedder applies the
//! adjustment ([`crate::renderer::Renderer::take_scroll_adjustment`]).
//!
//! Not done yet: priority candidates (the focused editable element, the find-in-page match),
//! anchoring inside scroll containers other than the viewport (a nested scroll container is a
//! candidate of its own, not descended into), and the limit re-snapping puts on adjustments.

use servo_arc::Arc as ServoArc;
use snowlark_dom::NodeId;
use snowlark_layout::fragment_flags as ff;
use snowlark_layout::style::{Side, Style as _, WritingMode};
use snowlark_layout::{Fragment, FragmentKind, LayoutTree};
use snowlark_paint::{Point, Rect, Size};
use style::properties::ComputedValues;
use style::values::computed::PositionProperty;

use crate::adapter::RenderStyle;

/// Elements from the root element down, each with its computed values at the time.
type Path = Vec<(NodeId, ServoArc<ComputedValues>)>;

/// The viewport's scroll anchoring state.
#[derive(Default)]
pub(crate) struct Anchoring {
    /// Where the viewport is scrolled (as the embedder last said).
    pub scroll: Point,
    /// The anchor selected before the layout under way.
    queued: Option<Queued>,
    /// The adjustments made since the embedder last took them (physical CSS px).
    adjustment: Option<Point>,
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
    node: NodeId,
    axis: BlockEdge,
    /// The anchor's block-start edge relative to the scrollport, rounded to device pixels.
    edge: f32,
    /// The scroll position it was selected at.
    scroll: Point,
    /// The elements from the root element down to the anchor (itself included when it is one),
    /// with their computed values when it was selected (§2.2.2, the first trigger).
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
    /// The anchor node selection algorithm for the viewport, run before a layout that may move
    /// boxes, over the last layout's fragments (`root`, laid out in a scrollport of `scrollport`).
    /// An anchor already selected for this layout stays.
    pub fn before_layout(
        &mut self,
        root: &Fragment<RenderStyle>,
        layout: &LayoutTree<RenderStyle>,
        scrollport: Size,
        dpr: f32,
    ) {
        if self
            .queued
            .as_ref()
            .is_some_and(|q| q.scroll == self.scroll)
        {
            return;
        }
        self.queued = None;
        let axis = BlockEdge::of(
            layout
                .principal_writing_mode()
                .map_or(WritingMode::HorizontalTb, |m| m.0),
        );
        // (step 2, and the third suppression trigger: not scrolled away from the origin)
        if axis.scroll(self.scroll) == 0.0 {
            return;
        }
        let mut visible = Rect::new(
            self.scroll.x,
            self.scroll.y,
            scrollport.width,
            scrollport.height,
        );
        // The optimal viewing region: the scrollport less the root's `scroll-padding` (CSS Scroll
        // Snap 1 §4.1: the viewport's is the root element's).
        if let Some(s) = root_style(root) {
            let (w, h) = (visible.width, visible.height);
            let top = s.scroll_padding(Side::Top).resolve(h).max(0.0);
            let right = s.scroll_padding(Side::Right).resolve(w).max(0.0);
            let bottom = s.scroll_padding(Side::Bottom).resolve(h).max(0.0);
            let left = s.scroll_padding(Side::Left).resolve(w).max(0.0);
            visible = Rect::new(
                visible.x + left,
                visible.y + top,
                (w - left - right).max(0.0),
                (h - top - bottom).max(0.0),
            );
        }
        let mut finder = Finder {
            visible,
            path: Vec::new(),
            found: None,
        };
        finder.find(root, Point::ZERO, true);
        let Some((node, path)) = finder.found else {
            return;
        };
        let Some(edge) = axis.edge(&layout.layout_rects(node)) else {
            return;
        };
        self.queued = Some(Queued {
            node,
            axis,
            edge: device_round(edge - axis.scroll(self.scroll), dpr),
            scroll: self.scroll,
            path,
            suppressed: false,
        });
    }

    /// Whether an anchor is selected for the layout under way.
    pub fn is_queued(&self) -> bool {
        self.queued.is_some()
    }

    /// The second suppression trigger: an element in the scroller became or stopped being
    /// absolutely positioned (`old` and `new` its computed values before and after).
    /// `old` is `None` for an element that had no box (`display: none`): it becomes absolutely
    /// positioned if it is now.
    pub fn position_changed(&mut self, old: Option<&ComputedValues>, new: &ComputedValues) {
        if old.is_some_and(out_of_flow) != out_of_flow(new) {
            if let Some(q) = &mut self.queued {
                q.suppressed = true;
            }
        }
    }

    /// The scroll adjustment (§2.2) after the layout: the anchor's movement, unless a suppression
    /// trigger fired or the anchor has no box any more. `styles` gives an element's computed values
    /// now.
    pub fn after_layout<'a>(
        &mut self,
        layout: &LayoutTree<RenderStyle>,
        styles: impl Fn(NodeId) -> Option<&'a ComputedValues>,
        dpr: f32,
    ) {
        let Some(q) = self.queued.take() else {
            return;
        };
        if q.suppressed {
            return;
        }
        let Some(edge) = q.axis.edge(&layout.layout_rects(q.node)) else {
            return;
        };
        let delta = device_round(edge - q.axis.scroll(q.scroll), dpr) - q.edge;
        if delta == 0.0 {
            return;
        }
        // The first suppression trigger: a change on the path from the anchor to the scroller.
        let changed = q
            .path
            .iter()
            .any(|(n, old)| styles(*n).is_some_and(|new| disabling_change(old, new)));
        if changed {
            return;
        }
        let d = q.axis.offset(delta);
        let a = self.adjustment.get_or_insert(Point::ZERO);
        *a = Point::new(a.x + d.x, a.y + d.y);
    }

    /// The adjustments since the last call.
    pub fn take_adjustment(&mut self) -> Option<Point> {
        self.adjustment.take()
    }
}

struct Finder {
    visible: Rect,
    /// The elements from the root element down to the fragment being examined.
    path: Path,
    found: Option<(NodeId, Path)>,
}

impl Finder {
    /// The candidate examination algorithm for the fragment `f` at `at` (root coordinates).
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
            self.found = Some((node, path));
        }
        if matches!(status, Walk::Return | Walk::Skip) {
            return status;
        }
        // (a scroll container inside the viewport is a candidate of its own: what it scrolls is
        // not looked into)
        if let FragmentKind::Box(b) = &f.kind {
            let nested = b.flags & ff::SCROLL_CONTAINER != 0
                && b.flags & (ff::ROOT | ff::VIEWPORT_OVERFLOW) == 0;
            if nested && status == Walk::Constrain {
                return status;
            }
        }
        let pushed = element.is_some();
        if let Some((n, s)) = element {
            self.path.push((n, s.values().clone()));
        }
        let mut out = status;
        for c in f.visible_children() {
            let p = Point::new(at.x + c.offset.x, at.y + c.offset.y);
            let s = self.find(&c.fragment, p, false);
            if matches!(s, Walk::Return | Walk::Constrain) {
                out = s;
                break;
            }
        }
        if pushed {
            self.path.pop();
        }
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
                // (the initial containing block: the scroller's own box)
                if top && b.flags & ff::INITIAL_CONTAINING_BLOCK != 0 {
                    return Walk::Continue;
                }
                // Excluded subtrees (§2.1): `overflow-anchor: none`, fixed and sticky boxes.
                use style::computed_values::overflow_anchor::T as OverflowAnchor;
                if b.style.values().get_box().overflow_anchor == OverflowAnchor::None
                    || b.flags & (ff::FIXED | ff::STICKY) != 0
                {
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
