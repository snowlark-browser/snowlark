// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Writing-mode boundaries (CSS Writing Modes 4 §3.2, §7.3): laying a box out in its own flow frame
//! when it differs from its parent's, and mapping what its layout produced (fragments, margins,
//! out-of-flow candidates) into the parent's frame. See [`crate::logical`] for the frames.
//!
//! In a document without vertical writing modes no box crosses a boundary and nothing here runs.

use std::sync::Arc;

use snowlark_paint::{Rect, Size};

use crate::boxes::*;
use crate::fragment::*;
use crate::layout::*;
use crate::logical::Flip;
use crate::sizing::{resolve_auto_margins, ContentSizes, Edges};
use crate::style::*;

/// Maps a fragment subtree by `m` (from its frame into another), giving its styles the view `view`
/// (the target frame's map to physical coordinates).
pub(crate) fn map_fragment<S: Style>(f: &Fragment<S>, m: Flip, view: Flip) -> Arc<Fragment<S>> {
    Arc::new(map_fragment_inner(f, m, view, false))
}

/// Whether `f` is part of a line (text, an inline box's piece) rather than a box of its own laid
/// out in the block frame (an atomic inline, a float, a marker).
fn in_line<S: Style>(f: &Fragment<S>) -> bool {
    match &f.kind {
        FragmentKind::Text(_) => true,
        FragmentKind::Box(b) => b.flags & fragment_flags::INLINE_BOX != 0,
        FragmentKind::Line { .. } => false,
    }
}

fn map_child<S: Style>(
    c: &FragmentChild<S>,
    m: Flip,
    view: Flip,
    within: Size,
    line: bool,
) -> FragmentChild<S> {
    let r = m.rect(
        Rect::new(
            c.offset.x,
            c.offset.y,
            c.fragment.size.width,
            c.fragment.size.height,
        ),
        within,
    );
    FragmentChild {
        offset: snowlark_paint::Point::new(r.x, r.y),
        fragment: if line && !in_line(&c.fragment) {
            c.fragment.clone()
        } else {
            Arc::new(map_fragment_inner(&c.fragment, m, view, line))
        },
        stacking_owner: c.stacking_owner,
    }
}

/// Maps the children of a line of height `height` out of the line frame of `vertical-lr` (see
/// [`crate::inline::line_view`]): the text and inline boxes are mirrored, boxes of their own only
/// move. `view` is the flow frame.
pub(crate) fn map_line_children<S: Style>(
    children: &mut [FragmentChild<S>],
    height: f32,
    view: Flip,
) {
    let within = Size::new(0.0, height);
    for c in children {
        *c = map_child(c, Flip::MIRROR_Y, view, within, true);
    }
}

fn map_fragment_inner<S: Style>(f: &Fragment<S>, m: Flip, view: Flip, line: bool) -> Fragment<S> {
    let size = f.size;
    let children = f
        .children
        .iter()
        .map(|c| map_child(c, m, view, size, line))
        .collect();
    let baseline = |b: Option<f32>| map_baseline(b, m, size);
    let kind = match &f.kind {
        FragmentKind::Box(b) => {
            let mut flags = b.flags;
            if m.swaps() {
                let x = flags & fragment_flags::CLIP_X != 0;
                let y = flags & fragment_flags::CLIP_Y != 0;
                flags &= !(fragment_flags::CLIP_X | fragment_flags::CLIP_Y);
                if x {
                    flags |= fragment_flags::CLIP_Y;
                }
                if y {
                    flags |= fragment_flags::CLIP_X;
                }
            }
            let content = match &b.content {
                BoxContent::None => BoxContent::None,
                BoxContent::Replaced {
                    content,
                    rect,
                    clip,
                    control,
                    control_flip,
                } => BoxContent::Replaced {
                    content: content.clone(),
                    rect: m.rect(*rect, size),
                    clip: m.rect(*clip, size),
                    control: control.clone(),
                    control_flip: control_flip.then(m),
                },
                BoxContent::CollapsedBorders(segs) => BoxContent::CollapsedBorders(Arc::new(
                    segs.iter()
                        .map(|s| CollapsedBorderSegment {
                            rect: m.rect(s.rect, size),
                            horizontal: s.horizontal != m.swaps(),
                            ..*s
                        })
                        .collect(),
                )),
                BoxContent::ColumnGaps(gaps) => BoxContent::ColumnGaps(
                    gaps.iter()
                        .map(|g| ColumnGap {
                            rect: m.rect(g.rect, size),
                            side: m.sides(g.side),
                        })
                        .collect(),
                ),
            };
            FragmentKind::Box(BoxFragment {
                style: b.style.with_view(view),
                box_id: b.box_id,
                node: b.node,
                pseudo: b.pseudo,
                border: m.sides(b.border),
                padding: m.sides(b.padding),
                flags,
                order: b.order,
                subtree_end: b.subtree_end,
                content,
                containing: b.containing.copied(),
                clip_reference: b.clip_reference.copied(),
                strip: b.strip.copied(),
                stitched: b.stitched.as_ref().map(|r| Box::new(m.rect(**r, size))),
            })
        }
        FragmentKind::Line { baseline: b } => FragmentKind::Line {
            baseline: map_baseline(Some(*b), m, size).unwrap_or(*b),
        },
        FragmentKind::Text(t) => {
            let mut t = t.clone();
            t.style = t.style.with_view(view);
            t.flip = t.flip.then(m);
            FragmentKind::Text(t)
        }
    };
    Fragment {
        size: m.size(size),
        kind,
        children,
        overflow: m.rect(f.overflow, size),
        inflow: f.inflow.map(|r| m.rect(r, size)),
        paint_bounds: m.rect(f.paint_bounds, size),
        baseline: baseline(f.baseline),
        last_baseline: baseline(f.last_baseline),
    }
}

/// A baseline (an offset from the top) in the mapped frame: none across axes (an orthogonal box's
/// baseline is synthesized by whoever aligns it), measured from the other edge when the block axis
/// reverses.
fn map_baseline(b: Option<f32>, m: Flip, size: Size) -> Option<f32> {
    let b = b?;
    if m.swaps() {
        None
    } else if m.reverses_y() {
        Some(size.height - b)
    } else {
        Some(b)
    }
}

/// A static position (an area and its alignment per axis) in a box of size `within`, mapped.
fn map_oof(mut o: Oof, m: Flip, within: Size, rtl: bool) -> Oof {
    // Self alignment refers to the static-position containing block's axes: past the frame it was
    // found in, the side it means is named (the start of the parent's inline and block axes).
    if o.x.align == StaticAlign::SelfAlign {
        o.x.align = if rtl {
            StaticAlign::End
        } else {
            StaticAlign::Start
        };
    }
    if o.y.align == StaticAlign::SelfAlign {
        o.y.align = StaticAlign::Start;
    }
    let r = m.rect(
        Rect::new(
            o.x.start,
            o.y.start,
            o.x.end - o.x.start,
            o.y.end - o.y.start,
        ),
        within,
    );
    let (mut x, mut y) = if m.swaps() { (o.y, o.x) } else { (o.x, o.y) };
    let rev = |a: StaticAlign| match a {
        StaticAlign::Start => StaticAlign::End,
        StaticAlign::End => StaticAlign::Start,
        a => a,
    };
    // Whether the target axis runs against the source axis it came from.
    let tx = m.inverse().reverses_x();
    let ty = m.inverse().reverses_y();
    if tx {
        x.align = rev(x.align);
        x.safe_fallback = x.safe_fallback.map(rev);
    }
    if ty {
        y.align = rev(y.align);
        y.safe_fallback = y.safe_fallback.map(rev);
    }
    x.start = r.x;
    x.end = r.x + r.width;
    y.start = r.y;
    y.end = r.y + r.height;
    Oof { x, y, ..o }
}

/// A style's fixed length (no percentage), if `v` is one.
fn fixed(v: SizeValue) -> Option<f32> {
    v.length_percentage()
        .filter(|l| !l.has_percent())
        .map(|l| l.length)
}

/// §7.3.1's fallback for the available inline space of orthogonal flow `id` in frame `own`, when
/// its containing block's block size is indefinite: the smallest of the containing block's fixed
/// maximum size (floored by its minimum), the nearest scrollport's inner size if fixed or limited
/// by a maximum, and the initial containing block's size.
fn fallback_inline<S: Style>(cx: &mut Cx<'_, S>, id: BoxId, own: Flip) -> f32 {
    cx.tree.icb_dependents.insert(id);
    let vertical = own.swaps();
    let mut c = if vertical {
        cx.icb.height
    } else {
        cx.icb.width
    };
    let max_of = |s: &S| match if vertical {
        s.phys_max_height()
    } else {
        s.phys_max_width()
    } {
        MaxSizeValue::LengthPercentage(l) if !l.has_percent() => Some(l.length),
        _ => None,
    };
    let min_of = |s: &S| {
        fixed(if vertical {
            s.phys_min_height()
        } else {
            s.phys_min_width()
        })
    };
    let mut cur = cx.tree.b(id).parent;
    if let Some(p) = cur {
        let s = &cx.tree.b(p).style;
        if let Some(m) = max_of(s) {
            c = c.min(m.max(min_of(s).unwrap_or(0.0)));
        }
    }
    while let Some(p) = cur {
        let b = cx.tree.b(p);
        let s = &b.style;
        if s.phys_overflow_x().is_scroll_container() || s.phys_overflow_y().is_scroll_container() {
            let size = fixed(if vertical {
                s.phys_height()
            } else {
                s.phys_width()
            });
            let limit = match (size, max_of(s)) {
                (Some(v), max) => Some(crate::sizing::clamp(v, min_of(s), max)),
                (None, Some(max)) => Some(max.max(min_of(s).unwrap_or(0.0))),
                (None, None) => None,
            };
            if let Some(v) = limit {
                let (a, z) = if vertical {
                    (Side::Top, Side::Bottom)
                } else {
                    (Side::Left, Side::Right)
                };
                let inner = match s.box_sizing() {
                    BoxSizing::ContentBox => v,
                    BoxSizing::BorderBox => {
                        v - s.phys_border_width(a)
                            - s.phys_border_width(z)
                            - s.phys_padding(a).resolve(0.0)
                            - s.phys_padding(z).resolve(0.0)
                    }
                };
                c = c.min(inner.max(0.0));
            }
            break;
        }
        cur = b.parent;
    }
    c
}

/// The space a box crossing into its own frame is laid out in, from the space its parent gave it.
fn inner_space<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    space: &Space,
    m: Flip,
    own: Flip,
) -> Space {
    let inv = m.inverse();
    let mut s = *space;
    s.collapsed_border = space.collapsed_border.map(|b| inv.sides(b));
    s.trim = TrimRequest::default();
    if !m.swaps() {
        return s;
    }
    // An orthogonal flow (§7.3): its inline axis is the parent's block axis. The available inline
    // space is the containing block's block size when definite, else §7.3.1's fallback constraint;
    // its `auto` inline size shrinks to fit that space (§7.3.2), unless the parent imposes a size.
    s.edge_basis = Some(space.edges_basis());
    s.inline_pct_auto = cx.measuring_across && space.cb_height.is_none();
    s.cb_width = match space.cb_height {
        Some(h) => h,
        None => fallback_inline(cx, id, own),
    };
    // (the parent's inline size is not known yet while its intrinsic sizes are worked out)
    s.cb_height = (!cx.measuring_across).then_some(space.cb_width);
    s.available = s.cb_width;
    s.mode = WidthMode::ShrinkToFit;
    s.fixed_width = space.fixed_height;
    s.fixed_height = space.fixed_width;
    s.fixed_height_definite = true;
    s.content_height = space.content_width;
    s.content_width = false;
    s
}

impl<S: Style> Cx<'_, S> {
    /// `id`'s style read in the frame layout is running in (its own view, unless it is a box in
    /// another writing mode seen from its parent).
    #[inline]
    pub fn style_here(&self, id: BoxId) -> S {
        let s = &self.tree.b(id).style;
        if s.view() == self.frame {
            s.clone()
        } else {
            s.with_view(self.frame)
        }
    }

    /// Lays out `id`, whose writing mode differs from the frame's, in its own frame, and maps the
    /// result into the frame.
    pub(crate) fn layout_across(
        &mut self,
        id: BoxId,
        kind: BoxKind,
        space: &Space,
    ) -> LayoutResult<S> {
        let outer = self.frame;
        let own = self.tree.b(id).style.view();
        let m = Flip::between(own, outer);
        let mut inner = inner_space(self, id, space, m, own);
        if m.swaps()
            && space.mode == WidthMode::Fill
            && space.fixed_width.is_none()
            && self.tree.b(id).level == Level::Block
        {
            // `justify-self: stretch` fills the parent's inline axis, which is the box's block
            // axis (css-align-3 §6.1.2; `normal` keeps fitting its content, §7.3).
            let ps = self.style_here(id);
            let stretch = match ps.justify_self() {
                Align::Auto => self
                    .tree
                    .b(id)
                    .parent
                    .is_some_and(|p| self.tree.b(p).style.justify_items() == Align::Stretch),
                a => a == Align::Stretch,
            };
            if (stretch && ps.width().is_auto()) || ps.width() == SizeValue::Stretch {
                let e = Edges::of(&ps, space.edges_basis());
                inner.fixed_height = Some((space.available - e.margin_h()).max(e.pb_h()));
            }
            // `height: stretch`: the parent's block axis, which is the box's inline axis.
            if let (SizeValue::Stretch, Some(h), None) =
                (ps.height(), space.cb_height, space.fixed_height)
            {
                let e = Edges::of(&ps, space.edges_basis());
                let (over, under) = crate::block::uncollapsed_margins(self, id, &e);
                inner.fixed_width = Some((h - over - under).max(e.pb_v()));
            }
        }
        self.frame = own;
        let r = self.dispatch(id, kind, &inner, None, snowlark_paint::Point::ZERO);
        self.frame = outer;
        let fsize = r.fragment.size;
        let fragment = map_fragment(&r.fragment, m, outer);
        let mut margin = m.sides(r.margin);
        if m.swaps() {
            // Margins in the parent's frame, percentages of its containing block's inline size
            // (css-box-4 §3), `auto` in the inline axis taking the free space of a block-level box
            // in normal flow (CSS 2 §10.3.3).
            let ps = self.style_here(id);
            let edges = Edges::of(&ps, space.edges_basis());
            margin.top = edges.mt();
            margin.bottom = edges.mb();
            margin.left = edges.ml();
            margin.right = edges.mr();
            if space.mode == WidthMode::Fill
                && space.fixed_width.is_none()
                && (edges.margin[1].is_none() || edges.margin[3].is_none())
            {
                let rtl = self.cb_rtl(id);
                let legacy = self.cb_legacy_align(id);
                let (l, r) =
                    resolve_auto_margins(&edges, fragment.size.width, space.available, rtl, legacy);
                margin.left = l;
                margin.right = r;
            } else if space.mode == WidthMode::Fill && space.fixed_width.is_none() {
                if let Some(j) = crate::block::justify_self_of(self, id, &ps) {
                    let free = space.available - fragment.size.width - edges.margin_h();
                    let rtl = self.cb_rtl(id);
                    // (its own start side, among those along this axis)
                    let inline_start = if ps.direction() == Direction::Rtl {
                        Side::Right
                    } else {
                        Side::Left
                    };
                    let to_frame = |s| outer.inverse().side(own.side(s));
                    let self_rtl =
                        [to_frame(inline_start), to_frame(Side::Top)].contains(&Side::Right);
                    let safe = ps.align_safety().justify_self == OverflowPosition::Safe;
                    let d = crate::block::justify_shift(j, free, rtl, self_rtl, safe);
                    margin.left += d;
                    margin.right += free - d;
                }
            }
        }
        LayoutResult {
            fragment,
            margin,
            start: Strut::of(margin.top),
            end: Strut::of(margin.bottom),
            through: false,
            floats: Vec::new(),
            oof: r
                .oof
                .into_iter()
                .map(|o| {
                    let rtl = self
                        .tree
                        .b(o.id)
                        .parent
                        .is_some_and(|p| self.tree.b(p).style.direction() == Direction::Rtl);
                    map_oof(o, m, fsize, rtl)
                })
                .collect(),
        }
    }

    /// `id`'s content-box min/max-content sizes along its own inline axis.
    pub(crate) fn own_content_sizes(&mut self, id: BoxId) -> ContentSizes {
        let outer = self.frame;
        self.frame = self.tree.b(id).style.view();
        let c = self.content_sizes(id);
        self.frame = outer;
        c
    }

    /// The content-box min/max-content inline sizes, in the current frame, of a box in another
    /// writing mode. Along a shared inline axis they are its own; across axes (an orthogonal flow,
    /// §7.3.2) both are its block size laid out in the fallback available space.
    pub(crate) fn content_sizes_across(&mut self, id: BoxId) -> ContentSizes {
        let outer = self.frame;
        let own = self.tree.b(id).style.view();
        let m = Flip::between(own, outer);
        if !m.swaps() {
            self.frame = own;
            let c = self.content_sizes(id);
            self.frame = outer;
            return c;
        }
        let b = self.tree.b(id);
        if let Some((pass, sizes)) = b.cache.intrinsic_across {
            if !b.is_dirty() || pass == self.pass {
                return sizes;
            }
        }
        let ps = self.style_here(id);
        let edges = Edges::of(&ps, 0.0);
        // (no definite sizes around it: its inline size comes from the fallback constraint)
        // (its available inline space is its parent's fixed block size, if it has one)
        let block = self.tree.b(id).parent.and_then(|p| {
            let s = &self.tree.b(p).style;
            let e = Edges::of(s, 0.0);
            crate::sizing::resolve_height(s.height(), None, e.pb_v(), s.box_sizing())
                .map(|h| (h - e.pb_v()).max(0.0))
        });
        let space = Space::shrink(0.0, block, 0.0);
        self.measuring_across = true;
        let r = self.layout(id, &space, None, snowlark_paint::Point::ZERO);
        self.measuring_across = false;
        let w = (r.fragment.size.width - edges.pb_h()).max(0.0);
        let sizes = ContentSizes { min: w, max: w };
        self.tree.b_mut(id).cache.intrinsic_across = Some((self.pass, sizes));
        sizes
    }
}
