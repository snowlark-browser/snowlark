// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Anchor Positioning 1 for layout ([`ls::Style::resolve_anchors`]): an absolutely positioned
//! box's `anchor()` and `anchor-size()` (in its insets, sizes and margins) resolved against the
//! anchor boxes layout found, `position-area`'s containing block and default alignment (§3.1,
//! §4.1), and `anchor-center` (§4.2). The resolved values are an [`Overlay`] the style handle reads
//! in place of the computed ones.

use std::sync::Arc;

use snowlark_layout::style::{self as ls, AnchorQuery, AnchorSpec, Anchored, Side};
use snowlark_paint::{Rect, Size};
use style::logical_geometry::{LogicalAxis, LogicalSide, PhysicalSide, WritingMode};
use style::values::computed::length_percentage::{CalcNode, ComputedLeaf, Unpacked};
use style::values::computed::position::AnchorSide;
use style::values::computed::{self as c, Length};
use style::values::generics::length::{
    AnchorSizeKeyword, GenericMargin, GenericMaxSize, GenericSize,
};
use style::values::generics::position::{AnchorSideKeyword, GenericInset, TreeScoped};
use style::values::specified::align::AlignFlags;
use style::values::specified::position::{PositionAnchorKeyword, PositionAreaKeyword};
use style::values::DashedIdent;

use crate::adapter::{self, RenderStyle};

/// What an anchored box's resolved style reads in place of its computed values. Sides are
/// physical (top, right, bottom, left).
#[derive(Debug, Default)]
pub(crate) struct Overlay {
    pub insets: [Option<ls::LengthPercentageAuto>; 4],
    pub margins: [Option<ls::LengthPercentageAuto>; 4],
    /// `width`, `height`, `min-width`, `min-height`.
    pub sizes: [Option<ls::SizeValue>; 4],
    /// `max-width`, `max-height`.
    pub max_sizes: [Option<ls::MaxSizeValue>; 2],
    /// `justify-self` and `align-self` with their overflow position (`position-area`'s
    /// default alignment, §4.1).
    pub justify_self: Option<(ls::Align, ls::OverflowPosition)>,
    pub align_self: Option<(ls::Align, ls::OverflowPosition)>,
}

pub(crate) fn side_index(s: Side) -> usize {
    match s {
        Side::Top => 0,
        Side::Right => 1,
        Side::Bottom => 2,
        Side::Left => 3,
    }
}

/// No anchor at all: what an anchor function is where it cannot refer to one (a box that is not
/// absolutely positioned), its fallback or the property's initial value.
pub(crate) struct NoAnchors;

impl AnchorQuery<RenderStyle> for NoAnchors {
    fn anchor(&self, _: AnchorSpec<'_>) -> Option<Rect> {
        None
    }
    fn containing_block(&self) -> Size {
        Size::new(0.0, 0.0)
    }
    fn containing_style(&self) -> Option<&RenderStyle> {
        None
    }
}

/// Whether the start edge of an axis (horizontal or not) is its left or top edge in `wm`.
fn start_is_low(wm: WritingMode, horizontal: bool) -> bool {
    let side = if horizontal == wm.is_vertical() {
        LogicalSide::BlockStart
    } else {
        LogicalSide::InlineStart
    };
    matches!(side.to_physical(wm), PhysicalSide::Left | PhysicalSide::Top)
}

pub(crate) struct Resolver<'a> {
    q: &'a dyn AnchorQuery<RenderStyle>,
    cb: Size,
    cb_wm: WritingMode,
    self_wm: WritingMode,
    /// The default anchor (§2.4): what an anchor function without a name refers to.
    default: Option<AnchorSpec<'a>>,
}

impl<'a> Resolver<'a> {
    pub fn none(wm: WritingMode) -> Resolver<'static> {
        Resolver {
            q: &NoAnchors,
            cb: Size::new(0.0, 0.0),
            cb_wm: wm,
            self_wm: wm,
            default: None,
        }
    }

    fn target(&self, name: &TreeScoped<DashedIdent>) -> Option<Rect> {
        if name.value.is_empty() {
            self.q.anchor(self.default?)
        } else {
            self.q.anchor(AnchorSpec::Name(&name.value.0))
        }
    }

    /// §3.2.1: the value inset `side` takes from `anchor(<side>)` of anchor box `r`; `None` when
    /// the side is not in the inset's axis.
    fn inset_of(&self, r: Rect, side: Side, a: &AnchorSide) -> Option<f32> {
        let horizontal = matches!(side, Side::Left | Side::Right);
        let (lo, hi) = if horizontal {
            (r.x, r.x + r.width)
        } else {
            (r.y, r.y + r.height)
        };
        let near = matches!(side, Side::Left | Side::Top);
        let (kw, pct) = a.keyword_and_percentage();
        use AnchorSideKeyword as K;
        let at = match kw {
            K::Left | K::Right if !horizontal => return None,
            K::Top | K::Bottom if horizontal => return None,
            K::Left | K::Top => lo,
            K::Right | K::Bottom => hi,
            K::Inside if near => lo,
            K::Inside => hi,
            K::Outside if near => hi,
            K::Outside => lo,
            K::Start | K::End | K::SelfStart | K::SelfEnd | K::Center => {
                let wm = if matches!(kw, K::SelfStart | K::SelfEnd) {
                    self.self_wm
                } else {
                    self.cb_wm
                };
                let (s, e) = if start_is_low(wm, horizontal) {
                    (lo, hi)
                } else {
                    (hi, lo)
                };
                match kw {
                    K::End | K::SelfEnd => e,
                    // (`center` and percentages are a fraction of the way from start to end)
                    _ => s + pct.0 * (e - s),
                }
            }
        };
        let size = if horizontal {
            self.cb.width
        } else {
            self.cb.height
        };
        Some(if near { at } else { size - at })
    }

    /// §5.1.1: `anchor-size(<kw>)` of anchor box `r` in a property of the horizontal axis or not.
    fn size_of(&self, r: Rect, kw: AnchorSizeKeyword, horizontal: bool) -> f32 {
        use AnchorSizeKeyword as K;
        let width = match kw {
            K::None => horizontal,
            K::Width => true,
            K::Height => false,
            K::Block => self.cb_wm.is_vertical(),
            K::Inline => !self.cb_wm.is_vertical(),
            K::SelfBlock => self.self_wm.is_vertical(),
            K::SelfInline => !self.self_wm.is_vertical(),
        };
        if width {
            r.width
        } else {
            r.height
        }
    }

    /// A `calc()` holding anchor functions, each replaced by its length or its fallback; `None`
    /// when one is invalid without a fallback (the declaration is invalid at computed-value time).
    fn calc(
        &self,
        l: &c::LengthPercentage,
        side: Option<Side>,
        horizontal: bool,
    ) -> Option<c::LengthPercentage> {
        let Unpacked::Calc(calc) = l.unpack() else {
            return Some(l.clone());
        };
        let mut node = calc.node().clone();
        node.map_node(|n| self.calc_node(n, side, horizontal))
            .ok()?;
        Some(c::LengthPercentage::new_calc(node, calc.clamping_mode()))
    }

    fn calc_node(
        &self,
        n: &CalcNode,
        side: Option<Side>,
        horizontal: bool,
    ) -> Result<Option<CalcNode>, ()> {
        let (value, fallback) = match n {
            CalcNode::Anchor(f) => {
                // (`anchor()` only in the insets)
                let side = side.ok_or(())?;
                let a = AnchorSide::from(&f.side);
                let v = self
                    .target(&f.target_element)
                    .and_then(|r| self.inset_of(r, side, &a));
                (v, f.fallback.as_ref())
            }
            CalcNode::AnchorSize(f) => {
                let v = self
                    .target(&f.target_element)
                    .map(|r| self.size_of(r, f.size, horizontal));
                (v, f.fallback.as_ref())
            }
            _ => return Ok(None),
        };
        if let Some(px) = value {
            return Ok(Some(CalcNode::Leaf(ComputedLeaf::Length(Length::new(px)))));
        }
        let mut node = fallback.ok_or(())?.node.clone();
        node.map_node(|n| self.calc_node(n, side, horizontal))?;
        Ok(Some(node))
    }

    pub fn inset(&self, v: &c::Inset, side: Side) -> ls::LengthPercentageAuto {
        use ls::LengthPercentageAuto as L;
        use GenericInset as I;
        let horizontal = matches!(side, Side::Left | Side::Right);
        let px = |v: f32| L::LengthPercentage(ls::LengthPercentage::px(v));
        match v {
            I::LengthPercentage(l) => L::LengthPercentage(adapter::lp(l)),
            I::Auto => L::Auto,
            I::AnchorFunction(f) => match self
                .target(&f.target_element)
                .and_then(|r| self.inset_of(r, side, &f.side))
            {
                Some(v) => px(v),
                None => f
                    .fallback
                    .as_ref()
                    .map_or(L::Auto, |fb| self.inset(fb, side)),
            },
            I::AnchorSizeFunction(f) => match self
                .target(&f.target_element)
                .map(|r| self.size_of(r, f.size, horizontal))
            {
                Some(v) => px(v),
                None => f
                    .fallback
                    .as_ref()
                    .map_or(L::Auto, |fb| self.inset(fb, side)),
            },
            I::AnchorContainingCalcFunction(l) => self
                .calc(l, Some(side), horizontal)
                .map_or(L::Auto, |l| L::LengthPercentage(adapter::lp(&l))),
        }
    }

    pub fn size(&self, v: &c::Size, horizontal: bool) -> ls::SizeValue {
        use GenericSize as S;
        match v {
            S::AnchorSizeFunction(f) => match self
                .target(&f.target_element)
                .map(|r| self.size_of(r, f.size, horizontal))
            {
                Some(px) => ls::SizeValue::LengthPercentage(ls::LengthPercentage::px(px.max(0.0))),
                None => f
                    .fallback
                    .as_ref()
                    .map_or(ls::SizeValue::Auto, |fb| self.size(fb, horizontal)),
            },
            S::AnchorContainingCalcFunction(l) => self
                .calc(&l.0, None, horizontal)
                .map_or(ls::SizeValue::Auto, |l| {
                    ls::SizeValue::LengthPercentage(adapter::lp(&l))
                }),
            other => adapter::size(other),
        }
    }

    pub fn max_size(&self, v: &c::MaxSize, horizontal: bool) -> ls::MaxSizeValue {
        use GenericMaxSize as S;
        match v {
            S::AnchorSizeFunction(f) => match self
                .target(&f.target_element)
                .map(|r| self.size_of(r, f.size, horizontal))
            {
                Some(px) => {
                    ls::MaxSizeValue::LengthPercentage(ls::LengthPercentage::px(px.max(0.0)))
                }
                None => f
                    .fallback
                    .as_ref()
                    .map_or(ls::MaxSizeValue::None, |fb| self.max_size(fb, horizontal)),
            },
            S::AnchorContainingCalcFunction(l) => self
                .calc(&l.0, None, horizontal)
                .map_or(ls::MaxSizeValue::None, |l| {
                    ls::MaxSizeValue::LengthPercentage(adapter::lp(&l))
                }),
            other => adapter::max_size(other),
        }
    }

    pub fn margin(&self, v: &c::Margin, horizontal: bool) -> ls::LengthPercentageAuto {
        use ls::LengthPercentageAuto as L;
        use GenericMargin as M;
        match v {
            M::LengthPercentage(l) => L::LengthPercentage(adapter::lp(l)),
            M::Auto => L::Auto,
            M::AnchorSizeFunction(f) => match self
                .target(&f.target_element)
                .map(|r| self.size_of(r, f.size, horizontal))
            {
                Some(px) => L::LengthPercentage(ls::LengthPercentage::px(px)),
                None => f
                    .fallback
                    .as_ref()
                    .map_or(L::ZERO, |fb| self.margin(fb, horizontal)),
            },
            M::AnchorContainingCalcFunction(l) => self
                .calc(l, None, horizontal)
                .map_or(L::ZERO, |l| L::LengthPercentage(adapter::lp(&l))),
        }
    }
}

fn anchored_inset(v: &c::Inset) -> bool {
    !matches!(v, GenericInset::LengthPercentage(_) | GenericInset::Auto)
}

fn anchored_size(v: &c::Size) -> bool {
    matches!(
        v,
        GenericSize::AnchorSizeFunction(_) | GenericSize::AnchorContainingCalcFunction(_)
    )
}

fn anchored_max_size(v: &c::MaxSize) -> bool {
    matches!(
        v,
        GenericMaxSize::AnchorSizeFunction(_) | GenericMaxSize::AnchorContainingCalcFunction(_)
    )
}

fn anchored_margin(v: &c::Margin) -> bool {
    matches!(
        v,
        GenericMargin::AnchorSizeFunction(_) | GenericMargin::AnchorContainingCalcFunction(_)
    )
}

/// The lines of one axis of the position-area grid (§3.1.1), and the span of `k`'s tracks.
fn area_span(k: PositionAreaKeyword, lo: f32, hi: f32, size: f32) -> (f32, f32) {
    let lines = [lo.min(0.0), lo, hi, hi.max(size)];
    // (bits 0, 1, 2: the start, center and end tracks; no track is all of them)
    let tracks = k.track().map_or(0b111, |t| t as u8);
    let first = tracks.trailing_zeros() as usize;
    let last = 7 - tracks.leading_zeros() as usize;
    (lines[first.min(2)], lines[last.min(2) + 1])
}

fn align_value(flags: AlignFlags) -> ls::Align {
    if flags == AlignFlags::START {
        ls::Align::Start
    } else if flags == AlignFlags::END {
        ls::Align::End
    } else {
        ls::Align::Center
    }
}

impl RenderStyle {
    /// A size where no anchor is resolved: an anchor function as its fallback or `auto`.
    pub(crate) fn size_or_anchor(&self, v: &c::Size, horizontal: bool) -> ls::SizeValue {
        if anchored_size(v) {
            Resolver::none(self.values.writing_mode).size(v, horizontal)
        } else {
            adapter::size(v)
        }
    }

    /// [`RenderStyle::size_or_anchor`] for `max-width`/`max-height` (`none`).
    pub(crate) fn max_size_or_anchor(&self, v: &c::MaxSize, horizontal: bool) -> ls::MaxSizeValue {
        if anchored_max_size(v) {
            Resolver::none(self.values.writing_mode).max_size(v, horizontal)
        } else {
            adapter::max_size(v)
        }
    }

    /// [`ls::Style::resolve_anchors`].
    pub(crate) fn anchored(
        &self,
        q: &dyn AnchorQuery<RenderStyle>,
    ) -> Option<Anchored<RenderStyle>> {
        let p = self.values.get_position();
        let m = self.values.get_margin();
        let insets = [&p.top, &p.right, &p.bottom, &p.left];
        let margins = [
            &m.margin_top,
            &m.margin_right,
            &m.margin_bottom,
            &m.margin_left,
        ];
        let sizes = [&p.width, &p.height, &p.min_width, &p.min_height];
        let max_sizes = [&p.max_width, &p.max_height];
        let area = p.position_area;
        let anchor_center = |f: AlignFlags| f.value() == AlignFlags::ANCHOR_CENTER;
        let (justify_center, align_center) = (
            anchor_center(p.justify_self.0),
            anchor_center(p.align_self.0),
        );
        let functions = insets.iter().any(|v| anchored_inset(v))
            || sizes.iter().any(|v| anchored_size(v))
            || max_sizes.iter().any(|v| anchored_max_size(v))
            || margins.iter().any(|v| anchored_margin(v));
        if !functions && area.is_none() && !justify_center && !align_center {
            return None;
        }
        // §2.4: the default anchor.
        let default = match &p.position_anchor.value {
            PositionAnchorKeyword::None => None,
            PositionAnchorKeyword::Normal if area.is_none() => None,
            PositionAnchorKeyword::Normal | PositionAnchorKeyword::Auto => {
                Some(AnchorSpec::Implicit)
            }
            PositionAnchorKeyword::Ident(name) => Some(AnchorSpec::Name(&name.0)),
        };
        let cb = q.containing_block();
        let self_wm = self.values.writing_mode;
        let cb_wm = q
            .containing_style()
            .map_or(self_wm, |s| s.values.writing_mode);
        let r = Resolver {
            q,
            cb,
            cb_wm,
            self_wm,
            default,
        };
        let default_box = default.and_then(|d| q.anchor(d));
        let mut o = Overlay::default();
        for (i, side) in Side::ALL.into_iter().enumerate() {
            let horizontal = matches!(side, Side::Left | Side::Right);
            o.insets[side_index(side)] = Some(r.inset(insets[i], side));
            o.margins[side_index(side)] = Some(r.margin(margins[i], horizontal));
        }
        for (i, v) in sizes.iter().enumerate() {
            o.sizes[i] = Some(r.size(v, i % 2 == 0));
        }
        for (i, v) in max_sizes.iter().enumerate() {
            o.max_sizes[i] = Some(r.max_size(v, i == 0));
        }
        let inline_horizontal = !cb_wm.is_vertical();
        let mut center = [None, None];
        let mut area_rect = None;
        if let (false, Some(a)) = (area.is_none(), default_box) {
            // §3.1: the area of the grid is the containing block; auto insets and margins are 0.
            let phys = area.to_physical(cb_wm, self_wm);
            let (x0, x1) = area_span(phys.first, a.x, a.x + a.width, cb.width);
            let (y0, y1) = area_span(phys.second, a.y, a.y + a.height, cb.height);
            area_rect = Some(Rect::new(x0, y0, x1 - x0, y1 - y0));
            // §4.1: `normal` self-alignment toward the anchor (a single non-auto inset: toward it).
            let auto = |s: Side| {
                matches!(
                    o.insets[side_index(s)],
                    Some(ls::LengthPercentageAuto::Auto)
                )
            };
            let single = |start: Side, end: Side| match (auto(start), auto(end)) {
                (false, true) => Some((ls::Align::Left, ls::OverflowPosition::Unsafe)),
                (true, false) => Some((ls::Align::Right, ls::OverflowPosition::Unsafe)),
                _ => None,
            };
            let normal = |f: AlignFlags| matches!(f.value(), AlignFlags::NORMAL | AlignFlags::AUTO);
            let (x_axis, y_axis) = if inline_horizontal {
                (LogicalAxis::Inline, LogicalAxis::Block)
            } else {
                (LogicalAxis::Block, LogicalAxis::Inline)
            };
            let x_flags = phys.first.to_self_alignment(x_axis, &cb_wm);
            let y_flags = phys.second.to_self_alignment(y_axis, &cb_wm);
            // (physical: the x axis's start is its left edge, the y axis's its top)
            let x_align = single(Side::Left, Side::Right).or_else(|| {
                x_flags.map(|f| {
                    let start = start_is_low(cb_wm, true);
                    let a = match align_value(f) {
                        ls::Align::Start if start => ls::Align::Left,
                        ls::Align::Start => ls::Align::Right,
                        ls::Align::End if start => ls::Align::Right,
                        ls::Align::End => ls::Align::Left,
                        other => other,
                    };
                    (a, ls::OverflowPosition::Default)
                })
            });
            let y_align = match (auto(Side::Top), auto(Side::Bottom)) {
                (false, true) => Some((ls::Align::Start, ls::OverflowPosition::Unsafe)),
                (true, false) => Some((ls::Align::End, ls::OverflowPosition::Unsafe)),
                _ => y_flags.map(|f| {
                    let a = match (align_value(f), start_is_low(cb_wm, false)) {
                        (ls::Align::Start, false) => ls::Align::End,
                        (ls::Align::End, false) => ls::Align::Start,
                        (a, _) => a,
                    };
                    (a, ls::OverflowPosition::Default)
                }),
            };
            let x_center = x_flags == Some(AlignFlags::ANCHOR_CENTER)
                && x_align.is_some_and(|a| a.0 == ls::Align::Center);
            let y_center = y_flags == Some(AlignFlags::ANCHOR_CENTER)
                && y_align.is_some_and(|a| a.0 == ls::Align::Center);
            let (j, al) = if inline_horizontal {
                (x_align, y_align)
            } else {
                (y_align, x_align)
            };
            if normal(p.justify_self.0) {
                o.justify_self = j;
                if inline_horizontal && x_center || !inline_horizontal && y_center {
                    center[usize::from(!inline_horizontal)] = Some(0.0);
                }
            }
            if normal(p.align_self.0) {
                o.align_self = al;
                if inline_horizontal && y_center || !inline_horizontal && x_center {
                    center[usize::from(inline_horizontal)] = Some(0.0);
                }
            }
            for v in o.insets.iter_mut().chain(o.margins.iter_mut()) {
                if matches!(v, Some(ls::LengthPercentageAuto::Auto)) {
                    *v = Some(ls::LengthPercentageAuto::ZERO);
                }
            }
        }
        // §4.2: `anchor-center` centers the box on the default anchor box.
        if let Some(a) = default_box {
            if justify_center {
                center[usize::from(!inline_horizontal)] = Some(0.0);
            }
            if align_center {
                center[usize::from(inline_horizontal)] = Some(0.0);
            }
            if center[0].is_some() {
                center[0] = Some(a.x + a.width / 2.0);
            }
            if center[1].is_some() {
                center[1] = Some(a.y + a.height / 2.0);
            }
        } else {
            center = [None, None];
        }
        let mut style = self.clone();
        style.anchored = Some(Arc::new(o));
        Some(Anchored {
            style,
            area: area_rect,
            center,
        })
    }
}
