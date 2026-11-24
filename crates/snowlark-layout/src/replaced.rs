// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Replaced elements (CSS 2 §10.3.2, §10.4, §10.6.2; css-images-3 §4.1 default sizing; css-sizing-4
//! `aspect-ratio`; css-images-3 §5.5 `object-fit` and `object-position`).

use std::sync::Arc;

use snowlark_paint::{Point, Rect, Sides, Size, TextEngine};

use crate::boxes::*;
use crate::fragment::*;
use crate::layout::*;
use crate::sizing::*;
use crate::style::*;

/// The natural sizes and ratio a replaced box sizes from (after `aspect-ratio`).
struct Natural {
    width: Option<f32>,
    height: Option<f32>,
    ratio: Option<f32>,
}

/// The view box `object-view-box` cuts out of content with natural dimensions (CSS Images 5 §5.1),
/// in the coordinates of its natural size; `None` for `none`, content without both natural
/// dimensions, and a view box with no area.
fn view_box<S: Style>(s: &S, c: &ReplacedContent) -> Option<Rect> {
    let insets = s.object_view_box()?;
    if c.control.is_some() {
        return None;
    }
    let (w, h) = (c.natural_width?, c.natural_height?);
    let top = insets[0].resolve(h);
    let right = insets[1].resolve(w);
    let bottom = insets[2].resolve(h);
    let left = insets[3].resolve(w);
    let (vw, vh) = (w - left - right, h - top - bottom);
    (vw > 0.0 && vh > 0.0 && vw.is_finite() && vh.is_finite()).then(|| Rect::new(left, top, vw, vh))
}

fn natural<S: Style>(tree: &BoxTree<S>, text: &TextEngine, b: &LayoutBox<S>, s: &S) -> Natural {
    let mut content = match &b.data {
        BoxData::Replaced(c) => c.clone(),
        _ => ReplacedContent::default(),
    };
    // The view box's size is the natural size (CSS Images 5 §5.1).
    if let Some(v) = view_box(&b.style, &content) {
        content.natural_width = Some(v.width);
        content.natural_height = Some(v.height);
        content.natural_ratio = None;
    }
    // (natural sizes are physical; the box is sized in its flow frame)
    if s.view().swaps() {
        std::mem::swap(&mut content.natural_width, &mut content.natural_height);
        content.natural_ratio = content.natural_ratio.map(|r| 1.0 / r);
    }
    // A form control has natural sizes but no natural aspect ratio: a set width doesn't scale its
    // height.
    let mut own_ratio = content.ratio();
    if let Some(control) = &content.control {
        let (w, h) = crate::controls::natural_size(tree, text, s, control);
        content.natural_width = Some(w);
        content.natural_height = Some(h);
        own_ratio = None;
    }
    // CSS Contain 2 §3.1/§3.2: a size-contained replaced element has a natural size of zero in the
    // contained axes and no natural aspect ratio.
    let contained = s.size_containment();
    if contained.width {
        content.natural_width = Some(contained.intrinsic_width.unwrap_or(0.0));
        own_ratio = None;
    }
    if contained.height {
        content.natural_height = Some(contained.intrinsic_height.unwrap_or(0.0));
        own_ratio = None;
    }
    let ar = s.aspect_ratio();
    let ratio = match (ar.auto, ar.ratio) {
        (true, Some(r)) => own_ratio.or(Some(r)),
        (false, Some(r)) => Some(r),
        _ => own_ratio,
    }
    .filter(|r| *r > 0.0 && r.is_finite());
    Natural {
        width: content.natural_width,
        height: content.natural_height,
        ratio,
    }
}

/// Whether a `width` is a percentage (or a calc() with one).
fn percent_size(v: SizeValue) -> bool {
    matches!(v, SizeValue::LengthPercentage(lp) if lp.has_percent())
}

/// The used content-box width and height (CSS 2 §10.3.2, §10.6.2, then the §10.4 constraint table).
#[allow(clippy::too_many_arguments)]
fn used_size<S: Style>(
    tree: &BoxTree<S>,
    text: &TextEngine,
    b: &LayoutBox<S>,
    s: &S,
    cb_width: Option<f32>,
    cb_height: Option<f32>,
    available: f32,
    fixed: (Option<f32>, Option<f32>),
    own_sizes: bool,
) -> (f32, f32) {
    let edges = Edges::of(s, cb_width.unwrap_or(0.0));
    let pbh = edges.pb_h();
    let pbv = edges.pb_v();
    let sizing = s.box_sizing();
    let n = natural(tree, text, b, s);
    let content_w = |v: Option<f32>| v.map(|w| (w - pbh).max(0.0));
    let content_h = |v: Option<f32>| v.map(|h| (h - pbv).max(0.0));
    let nat_w = n.width.unwrap_or(0.0);
    let mut nothing = || ContentSizes {
        min: nat_w,
        max: nat_w,
    };
    let stretch = (available - edges.margin_h()).max(0.0);
    let w = content_w(fixed.0.or_else(|| {
        own_sizes
            .then(|| resolve_width(s.width(), cb_width, pbh, sizing, stretch, &mut nothing))
            .flatten()
    }));
    let h = content_h(fixed.1.or_else(|| {
        own_sizes
            .then(|| resolve_height(s.height(), cb_height, pbv, sizing))
            .flatten()
    }));
    // The default object size is physical: 300px wide, 150px tall.
    let (default_w, default_h) = if s.view().swaps() {
        (150.0_f32, 300.0_f32)
    } else {
        (300.0, 150.0)
    };
    let (mut width, mut height) = match (w, h) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => {
            // (a `field-sizing: content` textarea is as tall as its text wrapped to its width)
            let wrapped = match &b.data {
                BoxData::Replaced(ReplacedContent {
                    control: Some(c), ..
                }) => crate::controls::content_height(tree, text, s, c, w),
                _ => None,
            };
            let h = wrapped.or(n.ratio.map(|r| w / r)).or(n.height).unwrap_or(
                default_h
                    .min(w / 2.0)
                    .max(if n.ratio.is_none() { default_h } else { 0.0 }),
            );
            (w, h)
        }
        (None, Some(h)) => {
            let w = n.ratio.map(|r| h * r).or(n.width).unwrap_or(default_w);
            (w, h)
        }
        (None, None) => match (n.width, n.height, n.ratio) {
            (Some(w), Some(h), _) => (w, h),
            (Some(w), None, Some(r)) => (w, w / r),
            (None, Some(h), Some(r)) => (h * r, h),
            (Some(w), None, None) => (w, default_h),
            (None, Some(h), None) => (default_w, h),
            (None, None, Some(r)) => {
                // Only a ratio: fill the containing block's width (css-images-3 default sizing).
                // Without one (an intrinsic size contribution, where css-sizing-3 §5.2.1c makes a
                // percentage `width` behave as `auto`), a percentage `width` takes the default
                // object size's width (CSS 2 §10.3.2's fallback, as browsers do) and `auto` none.
                let w = match cb_width {
                    Some(_) => (stretch - pbh).max(0.0),
                    None if own_sizes && percent_size(s.width()) => default_w,
                    None => 0.0,
                };
                (w, w / r)
            }
            (None, None, None) => (default_w, default_h),
        },
    };
    // min/max (content box).
    let to_c = |v: Option<f32>, pb: f32| v.map(|x| (x - pb).max(0.0));
    let (min_w, max_w) = width_bounds(s, cb_width, pbh, stretch, &mut nothing);
    let (min_h, max_h) = height_bounds(s, cb_height, pbv);
    let (min_w, max_w) = (to_c(min_w, pbh).unwrap_or(0.0), to_c(max_w, pbh));
    let (min_h, max_h) = (to_c(min_h, pbv).unwrap_or(0.0), to_c(max_h, pbv));
    if let (None, None, Some(_)) = (w, h, n.ratio) {
        // §10.4's table: keep the ratio while satisfying the constraints.
        let max_w = max_w.unwrap_or(f32::INFINITY).max(min_w);
        let max_h = max_h.unwrap_or(f32::INFINITY).max(min_h);
        let (ow, oh) = (width.max(f32::EPSILON), height.max(f32::EPSILON));
        let (nw, nh) = if ow > max_w && oh > max_h {
            if max_w / ow <= max_h / oh {
                (max_w, (max_w * oh / ow).max(min_h))
            } else {
                ((max_h * ow / oh).max(min_w), max_h)
            }
        } else if ow < min_w && oh < min_h {
            if min_w / ow <= min_h / oh {
                ((min_h * ow / oh).min(max_w), min_h)
            } else {
                (min_w, (min_w * oh / ow).min(max_h))
            }
        } else if ow > max_w {
            (max_w, (max_w * oh / ow).max(min_h))
        } else if ow < min_w {
            (min_w, (min_w * oh / ow).min(max_h))
        } else if oh > max_h {
            ((max_h * ow / oh).max(min_w), max_h)
        } else if oh < min_h {
            ((min_h * ow / oh).min(max_w), min_h)
        } else {
            (ow, oh)
        };
        width = nw;
        height = nh;
    } else {
        width = clamp(width, Some(min_w), max_w);
        height = clamp(height, Some(min_h), max_h);
        // A ratio with one auto dimension follows the clamped other one.
        if let (Some(r), true, false) = (n.ratio, w.is_none(), h.is_none()) {
            width = clamp(height * r, Some(min_w), max_w);
        }
        if let (Some(r), false, true) = (n.ratio, w.is_none(), h.is_none()) {
            height = clamp(width / r, Some(min_h), max_h);
        }
    }
    (width.max(0.0), height.max(0.0))
}

/// The content-box size a replaced box takes when its own `width` and `height` are `auto`, with either
/// dimension possibly given (border-box sizes): a flex item measured for its content size
/// (css-flexbox-1 §4.5 and §9.2 step 3E: the item's preferred sizes are applied by the flex algorithm;
/// a given cross size and the min/max constraints still transfer through the aspect ratio, CSS 2
/// §10.4). `None` when the box has neither a natural width nor a ratio and a natural height, which is
/// sized against its containing block instead.
pub(crate) fn content_size<S: Style>(
    cx: &Cx<'_, S>,
    id: BoxId,
    cb: (Option<f32>, Option<f32>),
    width: Option<f32>,
    height: Option<f32>,
) -> Option<(f32, f32)> {
    let b = cx.tree.b(id);
    // (in the frame layout is running in: an item in another writing mode is sized in its container's)
    let s = cx.style_here(id);
    let n = natural(cx.tree, cx.text, b, &s);
    if width.is_none() && height.is_none() && n.width.is_none() && n.height.zip(n.ratio).is_none() {
        return None;
    }
    let e = Edges::of(&s, cb.0.unwrap_or(0.0));
    let (w, h) = used_size(
        cx.tree,
        cx.text,
        b,
        &s,
        cb.0,
        cb.1,
        0.0,
        (width, height),
        false,
    );
    Some((w + e.pb_h(), h + e.pb_v()))
}

/// The content width a replaced box takes with no constraints (for intrinsic sizing).
pub(crate) fn natural_content_width<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) -> f32 {
    let b = cx.tree.b(id);
    used_size(
        cx.tree,
        cx.text,
        b,
        &b.style,
        None,
        None,
        0.0,
        (None, None),
        true,
    )
    .0
}

/// Whether the replaced box is a form control.
pub(crate) fn is_control<S: Style>(b: &LayoutBox<S>) -> bool {
    matches!(&b.data, BoxData::Replaced(c) if c.control.is_some())
}

/// The border-box width a replaced box with a percentage `width` contributes to a min-content size
/// (css-sizing-3 §5.2.1c), or `None` when its `width` has no percentage. A form control's percentage
/// resolves against zero (§5.2.2's compressible elements: `calc(50% + 50px)` contributes 50px, as
/// the spec's example has it), except a button-like input's (`button`, `submit`, `reset`, `color`,
/// `file`), which is not compressible: `None`, its natural width. An image or other replaced content
/// contributes its border and padding only, as engines do.
pub(crate) fn min_content_percent_width<S: Style>(b: &LayoutBox<S>, pb: f32) -> Option<f32> {
    if !percent_size(b.style.width()) {
        return None;
    }
    let control = match &b.data {
        BoxData::Replaced(c) => c.control.as_ref(),
        _ => None,
    };
    if control.is_some_and(|c| {
        matches!(
            c.kind,
            ControlKind::Button { .. } | ControlKind::Color(_) | ControlKind::File { .. }
        )
    }) {
        return None;
    }
    Some(if control.is_some() {
        crate::sizing::resolve_height(b.style.width(), Some(0.0), pb, b.style.box_sizing())
            .unwrap_or(pb)
    } else {
        pb
    })
}

/// The border-box min/max-content contributions (CSS Sizing 3 §5.2: a replaced box with a percentage
/// or auto width contributes its natural size; `max-width: 100%`-style compressible cases give zero
/// min-content).
pub(crate) fn contribution_widths<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) -> (f32, f32) {
    let b = cx.tree.b(id);
    let e = Edges::of(&b.style, 0.0);
    // css-sizing-3 §5.2.1c: in the max-content contribution, a percentage `width` behaves as `auto`
    // and a percentage `max-width` as `none` (§5.2.2 makes them zero only for min-content). A
    // percentage height of a definite parent height transfers through the ratio.
    let (w, _) = used_size(
        cx.tree,
        cx.text,
        b,
        &b.style,
        None,
        cx.pct_height_basis,
        0.0,
        (None, None),
        true,
    );
    let w = w + e.pb_h();
    let min = if let Some(min) = min_content_percent_width(b, e.pb_h()) {
        min.min(w)
    } else if matches!(b.style.max_width(), MaxSizeValue::LengthPercentage(lp) if lp.has_percent())
    {
        e.pb_h()
    } else {
        w
    };
    (min, w)
}

/// Lays out a replaced box.
pub(crate) fn layout_replaced<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    space: &Space,
) -> LayoutResult<S> {
    let b = cx.tree.b(id);
    let s = b.style.clone();
    let edges = Edges::of(&s, space.cb_width);
    let (cw, ch) = used_size(
        cx.tree,
        cx.text,
        b,
        &s,
        Some(space.cb_width),
        space.cb_height,
        space.available,
        (space.fixed_width, space.fixed_height),
        true,
    );
    let width = cw + edges.pb_h();
    let height = ch + edges.pb_v();
    let rtl = cx.cb_rtl(id);
    let (ml, mr) = if space.mode == WidthMode::Fill
        && b.level == Level::Block
        && space.fixed_width.is_none()
    {
        resolve_auto_margins(&edges, width, space.available, rtl, cx.cb_legacy_align(id))
    } else {
        (edges.ml(), edges.mr())
    };
    let content_box = Rect::new(
        edges.border.left + edges.padding.left,
        edges.border.top + edges.padding.top,
        cw,
        ch,
    );
    let content = match &b.data {
        BoxData::Replaced(c) => c.clone(),
        _ => ReplacedContent::default(),
    };
    let rect = object_fit(&s, &content, content_box);
    let padding_box = Rect::new(
        edges.border.left,
        edges.border.top,
        cw + edges.padding.left + edges.padding.right,
        ch + edges.padding.top + edges.padding.bottom,
    );
    let control = content.control.as_ref().map(|c| {
        Arc::new(crate::controls::layout_text(
            cx.tree,
            cx.text,
            &s,
            c,
            content_box,
            padding_box,
        ))
    });
    // A text control's baseline is its text's. A native checkbox or radio button has no line box;
    // CSS 2 §10.8.1 would put an inline-block's bottom margin edge on the baseline, but browsers put
    // these widgets' border-box bottom there and let the UA sheet's bottom margin hang below it
    // (Chrome's layout, which the HTML rendering section's widget margins are tuned for).
    let check = content.control.as_ref().is_some_and(|c| {
        c.native
            && matches!(
                c.kind,
                ControlKind::Checkbox { .. } | ControlKind::Radio { .. }
            )
    });
    let baseline = control
        .as_ref()
        .and_then(|c| c.baseline)
        .or_else(|| check.then_some(height));
    let fragment = box_fragment(
        cx,
        id,
        Size::new(width, height),
        edges.border,
        edges.padding,
        Vec::new(),
        baseline,
        baseline,
        BoxContent::Replaced {
            content,
            rect,
            clip: content_box,
            control,
            control_flip: crate::logical::Flip::IDENTITY,
        },
        0,
    );
    LayoutResult {
        fragment,
        margin: Sides::new(edges.mt(), mr, edges.mb(), ml),
        start: Strut::of(edges.mt()),
        end: Strut::of(edges.mb()),
        through: false,
        floats: Vec::new(),
        oof: Vec::new(),
    }
}

/// Where the content goes in the content box (css-images-3 §5.5): the concrete object size that
/// `object-fit` gives with the content box as the default object size, placed by `object-position`.
/// Content with neither natural size nor ratio fills the box.
fn object_fit<S: Style>(s: &S, c: &ReplacedContent, content_box: Rect) -> Rect {
    let view = view_box(s, c);
    let natural = match view {
        Some(v) => NaturalSize::fixed(v.width, v.height),
        None => NaturalSize {
            width: c.natural_width.filter(|w| *w >= 0.0),
            height: c.natural_height.filter(|h| *h >= 0.0),
            ratio: c.natural_ratio,
        },
    };
    // (natural sizes are physical; the box is laid out in its flow frame)
    let natural = match s.view().swaps() {
        true => NaturalSize {
            width: natural.height,
            height: natural.width,
            ratio: natural.ratio.map(|r| 1.0 / r),
        },
        false => natural,
    };
    let size = fitted_size(
        s.object_fit(),
        natural,
        (content_box.width, content_box.height),
    );
    let (px, py) = s.object_position();
    let x = px.resolve(content_box.width - size.width);
    let y = py.resolve(content_box.height - size.height);
    let fitted = Rect::new(
        content_box.x + x,
        content_box.y + y,
        size.width,
        size.height,
    );
    // With a view box, the view box lands on the fitted rectangle: the whole content is drawn
    // around it at that scale (what falls outside the content box is clipped).
    match (view, c.natural_width, c.natural_height) {
        (Some(v), Some(w), Some(h)) => {
            // (the view box and natural size are physical; the box is in its flow frame)
            let flip = s.view();
            let (x, y, vw, vh, w, h) = if flip.swaps() {
                (v.y, v.x, v.height, v.width, h, w)
            } else {
                (v.x, v.y, v.width, v.height, w, h)
            };
            let x = if flip.reverses_x() { w - x - vw } else { x };
            let y = if flip.reverses_y() { h - y - vh } else { y };
            let (sx, sy) = (fitted.width / vw, fitted.height / vh);
            Rect::new(fitted.x - x * sx, fitted.y - y * sy, w * sx, h * sy)
        }
        _ => fitted,
    }
}

/// The concrete object size `object-fit` gives content with `natural` dimensions in a `box_size`
/// content box (css-images-3 §5.5): `fill` takes the box, `contain`/`cover` the natural ratio
/// scaled to it (the box without a ratio), `none` the default sizing algorithm with the box as the
/// default object size, `scale-down` the smaller of `none` and `contain`.
pub(crate) fn fitted_size(fit: ObjectFit, natural: NaturalSize, box_size: (f32, f32)) -> Size {
    let contain_size = || natural.ratio().map_or(box_size, |r| contain(r, box_size));
    let (w, h) = match fit {
        ObjectFit::Fill => box_size,
        ObjectFit::Contain => contain_size(),
        ObjectFit::Cover => natural.ratio().map_or(box_size, |r| cover(r, box_size)),
        ObjectFit::None => natural.concrete((None, None), box_size),
        ObjectFit::ScaleDown => {
            let none = natural.concrete((None, None), box_size);
            let contained = contain_size();
            if none.0 <= contained.0 && none.1 <= contained.1 {
                none
            } else {
                contained
            }
        }
    };
    Size::new(w, h)
}

#[allow(dead_code)]
fn unused(_: Point) {}
