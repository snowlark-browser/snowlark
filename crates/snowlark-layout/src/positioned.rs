// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Absolutely positioned boxes (css-position-3 §3.5 and §4, css-align-3 §6.1.2, §6.2.2 and
//! §4.4.1.2): per axis, the inset-modified containing block (auto insets resolved from the static
//! position and the box's self-alignment, a negative size undone on the weaker inset), the
//! automatic size (stretch-fit or fit-content), auto margins, and the margin box aligned in the
//! inset-modified containing block.

use snowlark_paint::{Point, Rect, Size};

use crate::boxes::*;
use crate::fragment::FragmentChild;
use crate::layout::*;
use crate::sizing::*;
use crate::style::*;

/// A self-alignment value in one physical axis: `Start` is left or top whatever the direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AxisAlign {
    Normal,
    Stretch,
    Start,
    Center,
    End,
}

/// The physical meaning of a `justify-self`/`align-self` keyword: `rtl` is the direction of the
/// alignment container (for `start`/`end`), `self_rtl` the box's own (`self-start`/`self-end`);
/// both false for the block axis. Baseline and distributed values take their fallback.
pub(crate) fn axis_align(a: Align, rtl: bool, self_rtl: bool) -> AxisAlign {
    let start = |r: bool| if r { AxisAlign::End } else { AxisAlign::Start };
    let end = |r: bool| if r { AxisAlign::Start } else { AxisAlign::End };
    match a {
        Align::Auto | Align::Normal => AxisAlign::Normal,
        Align::Stretch => AxisAlign::Stretch,
        Align::Start | Align::FlexStart | Align::Baseline | Align::SpaceBetween => start(rtl),
        Align::End | Align::FlexEnd | Align::LastBaseline => end(rtl),
        Align::SelfStart => start(self_rtl),
        Align::SelfEnd => end(self_rtl),
        Align::Left => AxisAlign::Start,
        Align::Right => AxisAlign::End,
        Align::Center | Align::SpaceAround | Align::SpaceEvenly => AxisAlign::Center,
    }
}

/// The self-alignment a box gives its static position: its own `justify-self`/`align-self` when
/// that is a position (`auto`, `normal` and `stretch` keep the static position's start).
fn explicit_align(a: Align, rtl: bool, self_rtl: bool) -> AxisAlign {
    match a {
        Align::Auto | Align::Normal | Align::Stretch => AxisAlign::Normal,
        a => axis_align(a, rtl, self_rtl),
    }
}

/// One axis of an absolutely positioned box, in the containing block's padding box coordinates
/// (0 at its left or top edge).
#[derive(Clone, Copy, Debug)]
pub(crate) struct AxisInput {
    /// The containing block's size.
    pub cb: f32,
    /// The left and right (or top and bottom) insets; `None` for `auto`.
    pub insets: [Option<f32>; 2],
    /// The left and right (or top and bottom) margins; `None` for `auto`.
    pub margins: [Option<f32>; 2],
    /// The static-position rectangle in this axis.
    pub static_pos: StaticPos,
    /// The box's self-alignment for a [`StaticAlign::SelfAlign`] static position.
    pub static_align: AxisAlign,
    /// The static-position containing block's start edge is its right one.
    pub static_rtl: bool,
    /// The box's self-alignment (`auto` as `normal`).
    pub align: AxisAlign,
    pub overflow: OverflowPosition,
    /// The containing block's start edge in this axis is its right edge (rtl inline axis).
    pub rtl: bool,
    /// The containing block's inline axis (auto margins with negative space: §4.2).
    pub inline: bool,
}

impl AxisInput {
    fn auto_inset(&self) -> bool {
        self.insets[0].is_none() || self.insets[1].is_none()
    }

    /// The physical alignment of the box in its static-position rectangle (§3.5.1: `normal` as
    /// start, `stretch` as its fallback).
    fn static_alignment(&self) -> AxisAlign {
        let a = match self.static_pos.align {
            StaticAlign::Start => AxisAlign::Start,
            StaticAlign::Center => AxisAlign::Center,
            StaticAlign::End => AxisAlign::End,
            StaticAlign::SelfAlign => self.static_align,
        };
        match a {
            AxisAlign::Normal | AxisAlign::Stretch if self.static_rtl => AxisAlign::End,
            AxisAlign::Normal | AxisAlign::Stretch => AxisAlign::Start,
            a => a,
        }
    }

    /// The inset-modified containing block's start and end edges (css-position-3 §3.5.1, §3.5.2).
    pub fn imcb(&self) -> (f32, f32) {
        let cb = self.cb;
        let (mut start, mut end) = match self.insets {
            [None, None] => {
                let (s0, s1) = (self.static_pos.start, self.static_pos.end);
                match self.static_alignment() {
                    AxisAlign::End => (0.0, s1),
                    AxisAlign::Center => {
                        let c = (s0 + s1) / 2.0;
                        if c <= cb - c {
                            (0.0, 2.0 * c)
                        } else {
                            (2.0 * c - cb, cb)
                        }
                    }
                    _ => (s0, cb),
                }
            }
            [l, r] => (l.unwrap_or(0.0), cb - r.unwrap_or(0.0)),
        };
        if end < start {
            // The weaker inset gives way: the auto one, else the end edge's.
            let start_weaker = match self.insets {
                [None, Some(_)] => true,
                [Some(_), None] => false,
                _ => self.rtl,
            };
            if start_weaker {
                start = end;
            } else {
                end = start;
            }
        }
        (start, end)
    }

    /// Whether an automatic size is the stretch-fit size (§4.1; otherwise it is fit-content).
    pub fn stretches(&self, replaced_or_table: bool) -> bool {
        !self.auto_inset()
            && match self.align {
                AxisAlign::Stretch => true,
                AxisAlign::Normal => !replaced_or_table,
                _ => false,
            }
    }

    /// The border box's start edge and the used start and end margins for a border-box size
    /// `size` (§4.2 auto margins, §4.3 alignment, css-align-3 §4.4.1.2 overflow safety).
    pub fn place(&self, size: f32) -> (f32, f32, f32) {
        let (i0, i1) = self.imcb();
        let room = i1 - i0;
        let [ms, me] = self.margins;
        if self.auto_inset() {
            let (ms, me) = (ms.unwrap_or(0.0), me.unwrap_or(0.0));
            let m = size + ms + me;
            let x = match self.insets {
                // Statically positioned: aligned in the static-position rectangle.
                [None, None] => {
                    let (s0, s1) = (self.static_pos.start, self.static_pos.end);
                    // (`safe` alignment in a flex or grid area: the fallback when the box
                    // overflows its alignment container, the containing block: css-align-3 §4.4,
                    // css-position-3 §4)
                    let align = match self.static_pos.safe_fallback {
                        Some(f) if m > self.cb => match f {
                            StaticAlign::Center => AxisAlign::Center,
                            StaticAlign::End => AxisAlign::End,
                            _ => AxisAlign::Start,
                        },
                        _ => self.static_alignment(),
                    };
                    match align {
                        AxisAlign::End => s1 - m,
                        AxisAlign::Center => (s0 + s1 - m) / 2.0,
                        _ => s0,
                    }
                }
                // Against the edge of the stronger (non-auto) inset.
                [Some(_), None] => i0,
                _ => i1 - m,
            };
            return (x + ms, ms, me);
        }
        if ms.is_none() || me.is_none() {
            let remaining = room - size - ms.unwrap_or(0.0) - me.unwrap_or(0.0);
            let (ms, me) = match (ms, me) {
                // In the inline axis the start margin is zero and the end one takes the
                // negative space.
                (None, None) if self.inline && remaining < 0.0 => {
                    if self.rtl {
                        (remaining, 0.0)
                    } else {
                        (0.0, remaining)
                    }
                }
                (None, None) => (remaining / 2.0, remaining / 2.0),
                (None, Some(me)) => (remaining, me),
                (Some(ms), _) => (ms, remaining),
            };
            return (i0 + ms, ms, me);
        }
        let (ms, me) = (ms.unwrap_or(0.0), me.unwrap_or(0.0));
        let m = size + ms + me;
        let free = room - m;
        let align = match self.align {
            AxisAlign::Normal | AxisAlign::Stretch if self.rtl => AxisAlign::End,
            AxisAlign::Normal | AxisAlign::Stretch => AxisAlign::Start,
            a => a,
        };
        let mut x = i0
            + match align {
                AxisAlign::Center => free / 2.0,
                AxisAlign::End => free,
                _ => 0.0,
            };
        let safe = self.align != AxisAlign::Normal && self.overflow != OverflowPosition::Unsafe;
        if safe && free < 0.0 {
            // css-align-3 §4.4.1.2: cover the inset-modified containing block within the
            // bounding box of it and the containing block, as aligned as that allows; failing
            // that, start-aligned in that box.
            let (l0, l1) = (i0.min(0.0), i1.max(self.cb));
            if m <= l1 - l0 {
                let lo = (i1 - m).max(l0);
                let hi = i0.min(l1 - m);
                if lo <= hi {
                    x = x.clamp(lo, hi);
                }
            } else if self.rtl {
                x = l1 - m;
            } else {
                x = l0;
            }
        }
        (x + ms, ms, me)
    }
}

/// The stacking context an out-of-flow box `id` is painted in when it lies between the box and its
/// containing block `cb` (`None`: the initial containing block): its nearest ancestor that
/// establishes a stacking context (CSS 2 Appendix E, css-position-3 §6: the box is painted as part
/// of that context, in tree order, wherever its containing block is), as that box's order; 0 when
/// that is the stacking context the containing block is in (or for a top layer box, which belongs
/// to the root's top layer).
pub(crate) fn stacking_owner<S: Style>(tree: &BoxTree<S>, id: BoxId, cb: Option<BoxId>) -> u32 {
    if tree.b(id).has(flags::TOP_LAYER) {
        return 0;
    }
    let mut cur = tree.b(id).parent;
    while let Some(c) = cur {
        if Some(c) == cb {
            return 0;
        }
        let b = tree.b(c);
        if crate::paint::is_stacking_context(&b.style, box_flags(b)) {
            return b.order;
        }
        cur = b.parent;
    }
    0
}

/// Lays out an out-of-flow box in its containing block's padding box (`padding_box`, in the
/// containing block fragment's coordinates). Returns its fragment link and the candidates it passes on.
pub(crate) fn layout_absolute<S: Style>(
    cx: &mut Cx<'_, S>,
    oof: Oof,
    padding_box: Rect,
) -> (FragmentChild<S>, Vec<Oof>) {
    let id = oof.id;
    // (in the containing block's frame: a box in another writing mode is sized and placed in it,
    // and laid out in its own, CSS Writing Modes 4 §7.3)
    let style = cx.style_here(id);
    let kind = cx.tree.b(id).kind;
    let replaced = kind == BoxKind::Replaced;
    // Form controls are semi-replaced: their automatic size stretches like a non-replaced box's
    // (css-position-3 §4.1 counts them as non-replaced).
    let control = matches!(&cx.tree.b(id).data, BoxData::Replaced(c) if c.control.is_some());
    let replaced_or_table = (replaced && !control) || kind == BoxKind::TableWrapper;
    let cbw = padding_box.width;
    let cbh = padding_box.height;
    let edges = Edges::of(&style, cbw);
    let pbh = edges.pb_h();
    let pbv = edges.pb_v();
    let self_rtl = style.direction() == Direction::Rtl;
    // Whether the box's own start runs against each axis of this frame (`self-start`, `self-end`).
    let across = crate::logical::Flip::between(cx.tree.b(id).style.view(), cx.frame);
    let (self_x, self_y) = if across.swaps() {
        (across.reverses_y(), self_rtl != across.reverses_x())
    } else {
        (self_rtl != across.reverses_x(), across.reverses_y())
    };
    // The containing block's direction (the root's for the initial containing block), and the
    // static-position containing block's (the parent's).
    let cb_rtl = oof.cb.or(cx.tree.root).map_or(self_rtl, |c| {
        cx.tree.b(c).style.direction() == Direction::Rtl
    });
    let static_rtl = cx.tree.b(id).parent.map_or(self_rtl, |p| {
        cx.tree.b(p).style.direction() == Direction::Rtl
    });
    let inset = |side, basis| style.inset(side).resolve(basis);
    let sizing = style.box_sizing();

    let mut h = AxisInput {
        cb: cbw,
        insets: [inset(Side::Left, cbw), inset(Side::Right, cbw)],
        margins: [edges.margin[3], edges.margin[1]],
        static_pos: StaticPos {
            start: oof.x.start - padding_box.x,
            end: oof.x.end - padding_box.x,
            align: oof.x.align,
            safe_fallback: oof.x.safe_fallback,
        },
        static_align: explicit_align(style.justify_self(), static_rtl, self_x),
        static_rtl,
        align: axis_align(style.justify_self(), cb_rtl, self_x),
        overflow: style.align_safety().justify_self,
        rtl: cb_rtl,
        inline: true,
    };
    let mut v = AxisInput {
        cb: cbh,
        insets: [inset(Side::Top, cbh), inset(Side::Bottom, cbh)],
        margins: [edges.margin[0], edges.margin[2]],
        static_pos: StaticPos {
            start: oof.y.start - padding_box.y,
            end: oof.y.end - padding_box.y,
            align: oof.y.align,
            safe_fallback: oof.y.safe_fallback,
        },
        static_align: explicit_align(style.align_self(), false, self_y),
        static_rtl: false,
        align: axis_align(style.align_self(), false, self_y),
        overflow: style.align_safety().align_self,
        rtl: false,
        inline: false,
    };

    if cx.tree.b(id).has(flags::TOP_LAYER) {
        // css-position-3 §3.5.3: a top layer box's static-position rectangle is the initial
        // containing block, in its writing mode.
        h.static_pos = StaticPos::block_level(cbw);
        h.static_rtl = cb_rtl;
        v.static_pos = StaticPos::block_level(cbh);
    }

    // Replaced boxes have a used size of their own (CSS 2 §10.3.8, §10.6.5).
    let replaced_size = if replaced {
        let r = cx.layout(id, &Space::shrink(cbw, Some(cbh), cbw), None, Point::ZERO);
        Some(r.fragment.size)
    } else {
        None
    };

    // The width, against the inset-modified containing block (auto margins as zero).
    let (h0, h1) = h.imcb();
    let stretch_w = (h1 - h0 - edges.margin_h()).max(0.0);
    let (v0, v1) = v.imcb();
    let stretch_h = (v1 - v0 - edges.margin_v()).max(0.0);
    // `stretch` on a replaced box fills the axis; the other axis follows through the ratio
    // (css-position-3 §4.1, css-sizing-4 §5.1).
    let replaced_size = replaced_size.map(|s| {
        if control {
            return s;
        }
        let (cw, ch) = ((s.width - pbh).max(0.0), (s.height - pbv).max(0.0));
        let fills_w = h.stretches(true) && style.width().is_auto();
        let fills_h = v.stretches(true) && style.height().is_auto();
        match (fills_w, fills_h) {
            (true, true) => Size::new(stretch_w.max(pbh), stretch_h.max(pbv)),
            (true, false) if cw > 0.0 && style.height().is_auto() => {
                let w = stretch_w.max(pbh);
                Size::new(w, (w - pbh) * ch / cw + pbv)
            }
            (true, false) => Size::new(stretch_w.max(pbh), s.height),
            (false, true) if ch > 0.0 && style.width().is_auto() => {
                let hh = stretch_h.max(pbv);
                Size::new((hh - pbv) * cw / ch + pbh, hh)
            }
            (false, true) => Size::new(s.width, stretch_h.max(pbv)),
            (false, false) => s,
        }
    });
    // The height where it does not depend on the width: an orthogonal flow's width is its block
    // size, what its content takes in that height, and percentages in the content resolve
    // against it (css-sizing-3 §5.2).
    let known_h = if replaced {
        None
    } else {
        let (min_h, max_h) = height_bounds(&style, Some(cbh), pbv);
        match resolve_height(style.height(), Some(cbh), pbv, sizing) {
            Some(hh) => Some(clamp(hh, min_h, max_h)),
            None if style.height().is_auto() && v.stretches(replaced_or_table) => {
                Some(clamp(stretch_h, min_h, max_h).max(pbv))
            }
            None => None,
        }
    };
    let across_h = known_h.filter(|_| across.swaps());
    let mut sizes_cache: Option<ContentSizes> = None;
    let mut content = || {
        if let Some(c) = sizes_cache {
            return c;
        }
        let c = match across_h {
            Some(hh) => {
                let mut across = Space::shrink(cbw, Some(cbh), cbw);
                across.fixed_height = Some(hh);
                let w = cx
                    .layout(id, &across, None, Point::ZERO)
                    .fragment
                    .size
                    .width;
                let w = (w - pbh).max(0.0);
                ContentSizes { min: w, max: w }
            }
            None => match known_h {
                Some(hh) => {
                    let saved = cx.pct_height_basis.replace((hh - pbv).max(0.0));
                    let c = cx.content_sizes(id);
                    cx.pct_height_basis = saved;
                    c
                }
                None => cx.content_sizes(id),
            },
        };
        sizes_cache = Some(c);
        c
    };
    let specified_w = match replaced_size {
        Some(s) => Some(s.width),
        None => resolve_width(
            style.width(),
            Some(cbw),
            pbh,
            sizing,
            stretch_w,
            &mut content,
        ),
    };
    let (min_w, max_w) = width_bounds(&style, Some(cbw), pbh, stretch_w, &mut content);
    let stretch_width = h.stretches(replaced_or_table) && style.width().is_auto();
    let width = match specified_w {
        Some(_) if control && stretch_width => clamp(stretch_w, min_w, max_w).max(pbh),
        Some(w) if replaced => w,
        Some(w) => clamp(w, min_w, max_w),
        None if h.stretches(replaced_or_table) => clamp(stretch_w, min_w, max_w).max(pbh),
        // (through `aspect-ratio` from a height known here, css-sizing-4 §5.1)
        None if known_h.is_some() && style_ratio(&style).is_some() => {
            let (hh, r) = (known_h.unwrap_or(0.0), style_ratio(&style).unwrap_or(1.0));
            let w = match sizing {
                BoxSizing::BorderBox => hh * r,
                BoxSizing::ContentBox => (hh - pbv).max(0.0) * r + pbh,
            };
            // (the automatic minimum size: its content's, css-sizing-4 §5.1)
            let auto_min = style.min_width().is_auto()
                && !style.overflow_x().is_scroll_container()
                && !style.overflow_y().is_scroll_container();
            let w = if auto_min {
                w.max(content().min + pbh)
            } else {
                w
            };
            clamp(w, min_w, max_w).max(pbh)
        }
        None => clamp(
            content().shrink_to_fit((stretch_w - pbh).max(0.0)) + pbh,
            min_w,
            max_w,
        )
        .max(pbh),
    };

    // The height: laid out at the width to learn the content height when not given.
    let specified_h = match replaced_size {
        Some(s) => Some(s.height),
        // (`stretch` fills the inset-modified containing block less the margins)
        None if style.height() == SizeValue::Stretch => Some(stretch_h.max(pbv)),
        None => resolve_height(style.height(), Some(cbh), pbv, sizing),
    };
    let (min_h, max_h) = height_bounds(&style, Some(cbh), pbv);
    let stretch_height = style.height().is_auto() && v.stretches(replaced_or_table);
    let fixed_h = match specified_h {
        Some(_) if control && stretch_height => Some(clamp(stretch_h, min_h, max_h).max(pbv)),
        Some(hh) if replaced => Some(hh),
        Some(hh) => Some(clamp(hh, min_h, max_h)),
        // (`fit-content`, `min-content` and `max-content` are the content's height)
        None if stretch_height => Some(clamp(stretch_h, min_h, max_h).max(pbv)),
        None => None,
    };
    let mut space = Space::shrink(cbw, Some(cbh), cbw);
    space.fixed_width = Some(width);
    // `min-height`/`max-height` given as the content's height (css-sizing-3 §3.1:
    // `min-content`, `max-content`, `fit-content` are its auto height in the block axis) bound a
    // stretched height by that height.
    let content_bound = |v: SizeValue| {
        matches!(
            v,
            SizeValue::MinContent | SizeValue::MaxContent | SizeValue::FitContent(_)
        )
    };
    let max_content = matches!(
        style.max_height(),
        MaxSizeValue::MinContent | MaxSizeValue::MaxContent | MaxSizeValue::FitContent(_)
    );
    let fixed_h = match fixed_h {
        Some(hh) if !replaced && (max_content || content_bound(style.min_height())) => {
            let auto = cx
                .layout(id, &space, None, Point::ZERO)
                .fragment
                .size
                .height;
            let mut hh = hh;
            if max_content {
                hh = hh.min(auto);
            }
            if content_bound(style.min_height()) {
                hh = hh.max(auto);
            }
            Some(hh)
        }
        other => other,
    };
    space.fixed_height = fixed_h;
    let result = cx.layout(id, &space, None, Point::ZERO);
    // (a table can come out wider than asked: its width is a minimum)
    let (x, _, _) = h.place(result.fragment.size.width);
    let (y, _, _) = v.place(result.fragment.size.height);

    let offset = Point::new(padding_box.x + x, padding_box.y + y);
    let rest = result
        .oof
        .iter()
        .map(|o| o.shifted(offset.x, offset.y))
        .collect();
    (
        FragmentChild {
            offset,
            fragment: result.fragment,
            stacking_owner: stacking_owner(cx.tree, id, oof.cb),
        },
        rest,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis(cb: f32, l: Option<f32>, r: Option<f32>) -> AxisInput {
        AxisInput {
            cb,
            insets: [l, r],
            margins: [Some(0.0), Some(0.0)],
            static_pos: StaticPos::at(0.0),
            static_align: AxisAlign::Normal,
            static_rtl: false,
            align: AxisAlign::Normal,
            overflow: OverflowPosition::Default,
            rtl: false,
            inline: true,
        }
    }

    #[test]
    fn negative_imcb_gives_way_on_the_weaker_inset() {
        // inset: 12px in a 20px containing block: the end inset is reduced.
        let a = axis(20.0, Some(12.0), Some(12.0));
        assert_eq!(a.imcb(), (12.0, 12.0));
        // In rtl the end edge is the left one.
        let a = AxisInput { rtl: true, ..a };
        assert_eq!(a.imcb(), (8.0, 8.0));
        // With one auto inset, the auto one is weaker.
        let a = axis(20.0, None, Some(30.0));
        assert_eq!(a.imcb(), (-10.0, -10.0));
    }

    #[test]
    fn auto_margins_with_negative_space() {
        // Inline axis: the start margin is zero.
        let mut a = axis(20.0, Some(8.0), Some(8.0));
        a.margins = [None, None];
        assert_eq!(a.place(18.0), (8.0, 0.0, -14.0));
        // rtl: the right margin is zero.
        a.rtl = true;
        assert_eq!(a.place(18.0), (-6.0, -14.0, 0.0));
        // Block axis: split evenly.
        let mut v = axis(20.0, Some(8.0), Some(8.0));
        v.margins = [None, None];
        v.inline = false;
        assert_eq!(v.place(18.0), (1.0, -7.0, -7.0));
    }

    #[test]
    fn alignment_and_overflow_safety() {
        // unsafe center in a zero-size IMCB at 15: centered on it.
        let mut a = axis(20.0, Some(15.0), Some(15.0));
        a.align = AxisAlign::Center;
        a.overflow = OverflowPosition::Unsafe;
        assert_eq!(a.place(10.0).0, 10.0);
        // Default (safe) center of a box larger than its IMCB: it covers the IMCB and stays in
        // the containing block.
        let mut a = axis(100.0, Some(80.0), Some(10.0));
        a.align = AxisAlign::Center;
        assert_eq!(a.place(30.0).0, 70.0);
        // normal is start, without safety: CSS 2's over-constrained rule.
        let a = axis(100.0, Some(80.0), Some(10.0));
        assert_eq!(a.place(30.0).0, 80.0);
    }

    #[test]
    fn static_position_center_doubles_the_closest_distance() {
        // css-align-3 §6.5: a static position centered at 30 in 100: available 60.
        let mut a = axis(100.0, None, None);
        a.static_pos = StaticPos {
            start: 20.0,
            end: 40.0,
            align: StaticAlign::Center,
            safe_fallback: None,
        };
        assert_eq!(a.imcb(), (0.0, 60.0));
        assert_eq!(a.place(10.0).0, 25.0);
        // An end-aligned block-level static position (rtl parent): the box ends at the end.
        let mut a = axis(100.0, None, None);
        a.static_pos = StaticPos::block_level(80.0);
        a.static_rtl = true;
        assert_eq!(a.imcb(), (0.0, 80.0));
        assert_eq!(a.place(30.0).0, 50.0);
    }
}
