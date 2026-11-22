// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Painting the fragment tree into a display list in CSS 2 Appendix E order, and hit testing in the
//! same order.
//!
//! A stacking context paints: its root's background and borders, child stacking contexts with negative
//! `z-index`, its in-flow block-level descendants' backgrounds and borders, its floats (each as if it
//! were a stacking context), its inline content (text, inline boxes, atomic inlines painted atomically,
//! replaced content), its positioned descendants with `z-index: auto` or `0` in tree order, its child
//! stacking contexts with positive `z-index`, and finally outlines. Positioned descendants and nested
//! stacking contexts are collected into the enclosing stacking context's lists with the overflow clips
//! that apply to them (an out-of-flow fragment hangs off its containing block's fragment, so every
//! clipping fragment ancestor clips it).
//!
//! Stacking contexts come from the root, positioned boxes with an integer `z-index`, fixed boxes,
//! `opacity` below 1, transforms, and what [`Style::forces_stacking_context`] reports.
//!
//! A positioned box belongs to its nearest ancestor stacking context in the box tree, which need not
//! contain its containing block (a fixed box inside a `z-index: 0` wrapper hangs off the initial
//! containing block's fragment but is painted by the wrapper's stacking context): such a box's
//! fragment link carries that context ([`FragmentChild::stacking_owner`]); collecting puts it (and
//! what it paints as part of the same context) in an adopted list that the owning context takes
//! when it is painted. Its clip still comes from its containing block chain, so an owning context
//! with adopted members applies its own ancestors' clip to its other content only.
//!
//! The traversal is generic over a [`Sink`]: [`DisplayListSink`] emits display items; the hit tester
//! records hit regions and searches them back to front.
//!
//! With [`Painter::layers`], content whose position depends on the viewport's scroll position is
//! marked as layers for a compositor ([`snowlark_paint::Item::PushLayer`]): the root stacking
//! context's fixed boxes ([`LayerKind::Fixed`]) and sticky boxes whose scrollport is the viewport,
//! with what moves with them ([`LayerKind::Sticky`]); sticky content the compositor can't place (in a
//! clip, in a nested stacking context) is marked [`LayerKind::Pinned`].

use snowlark_dom::NodeId;
use snowlark_paint::{
    Border, BoxShadow, Color, DisplayListBuilder, Extend, GradientStop, ImageRendering, LayerKind,
    Matrix, PathEl, Point, Radii, Rect, Sides, Size, StickyConstraint, TextEngine,
};

use crate::clip::ClipGeometry;
use crate::fragment::*;
use crate::style::*;

/// What painting writes to.
pub(crate) trait Sink<S: Style> {
    /// Background, shadows and border of a box fragment at `at`; `scroll` is its scroll position
    /// when it is a scroll container of its own (not the viewport's), for `background-attachment:
    /// local`.
    fn decorations(
        &mut self,
        f: &Fragment<S>,
        b: &BoxFragment<S>,
        at: Point,
        canvas: bool,
        scroll: Option<Point>,
    );
    /// A text fragment of `size` (in the text's own frame: see [`TextFragment::flip`]) at `at`.
    fn text(&mut self, size: Size, t: &TextFragment<S>, at: Point);
    fn replaced(&mut self, f: &Fragment<S>, b: &BoxFragment<S>, at: Point);
    /// A selected replaced element's highlight, over its content (CSS Pseudo-Elements 4 §3.6.4);
    /// `line` is the line box it is on, in the coordinates of `at`.
    fn selected_replaced(
        &mut self,
        _f: &Fragment<S>,
        _b: &BoxFragment<S>,
        _at: Point,
        _line: Option<Rect>,
    ) {
    }
    fn collapsed_borders(&mut self, segments: &[CollapsedBorderSegment], at: Point);
    /// The column rules of multi-column container `b` (at `at`, its content scrolled by
    /// `scroll`), in `gaps`.
    fn column_rules(
        &mut self,
        _f: &Fragment<S>,
        _b: &BoxFragment<S>,
        _gaps: &[ColumnGap],
        _at: Point,
        _scroll: Option<Point>,
    ) {
    }
    fn outline(&mut self, f: &Fragment<S>, b: &BoxFragment<S>, at: Point);
    fn push_clip(&mut self, rect: Rect, radii: Radii);
    fn pop_clip(&mut self);
    fn push_transform(&mut self, m: Matrix);
    fn pop_transform(&mut self);
    fn push_opacity(&mut self, opacity: f32, bounds: Rect);
    fn pop_opacity(&mut self);
    /// Puts what follows, up to [`Sink::pop_filter`], through `ops` (Filter Effects 1 §10); the
    /// result shows within `bounds` (the current coordinates).
    fn push_filter(&mut self, _ops: &[FilterOp], _bounds: Rect) {}
    fn pop_filter(&mut self) {}
    /// A backdrop filter of `ops` over `rect` with `radii` (Filter Effects 2 §3), inside `own`
    /// opacity and mask groups of the element's own.
    fn backdrop_filter(&mut self, _ops: &[FilterOp], _rect: Rect, _radii: Radii, _own: u8) {}
    /// Content a compositor may draw as a layer of its own (see the module documentation).
    fn push_layer(&mut self, _kind: LayerKind) {}
    fn pop_layer(&mut self) {}
    /// An animation a compositor runs for the composited box just pushed.
    fn add_animation(&mut self, _a: snowlark_paint::layer_animation::LayerAnimation) {}
    /// Clips what follows to the fill of a path (`clip-path`, CSS Masking 1 §5), in the current
    /// coordinates: its contours, each closed, and whether it fills by the even-odd rule. A path
    /// with no contour clips everything away. Ended by [`Sink::pop_clip_path`]. (A `clip-path`
    /// that is a rectangle with rounded corners goes through [`Sink::push_clip`].)
    fn push_clip_path(&mut self, _contours: &[Vec<Point>], _even_odd: bool) {}
    fn pop_clip_path(&mut self) {}
    /// Clips what follows to a rectangle with rounded corners that is a `clip-path` (a box alone,
    /// or `inset()` with `round`): a path, its edges where they are, where [`Sink::push_clip`]'s
    /// are snapped to pixels. Ended by [`Sink::pop_clip_rounded_path`].
    fn push_clip_rounded_path(&mut self, rect: Rect, radii: Radii) {
        self.push_clip(rect, radii);
    }
    fn pop_clip_rounded_path(&mut self) {
        self.pop_clip();
    }
    /// Starts masking what follows by the box's mask layers (CSS Masking 1 §7), if it has any;
    /// whether it did ([`Sink::pop_mask`] ends it then).
    fn push_mask(&mut self, _f: &Fragment<S>, _b: &BoxFragment<S>, _at: Point) -> bool {
        false
    }
    fn pop_mask(&mut self) {}
    /// Starts the `background-clip: text` layers of box `b` (at `at`; CSS Backgrounds 4 §2.1),
    /// if the sink paints them: what follows, up to [`Sink::text_clip_layers`], is the box's text
    /// as their mask, painted as glyph coverage alone. Whether it did.
    fn push_text_clip(&mut self, _f: &Fragment<S>, _b: &BoxFragment<S>, _at: Point) -> bool {
        false
    }
    /// Ends the mask [`Sink::push_text_clip`] began and paints the box's text-clipped layers in
    /// it (the background colour too when the bottom layer is one).
    fn text_clip_layers(
        &mut self,
        _f: &Fragment<S>,
        _b: &BoxFragment<S>,
        _at: Point,
        _scroll: Option<Point>,
    ) {
    }
    /// Whether the text of a `background-clip: text` mask is being painted.
    fn in_text_clip(&self) -> bool {
        false
    }
    /// A scroll container's scroll bars, over its content: `scroll` is its scroll position,
    /// `device_scale` the device pixels per CSS px.
    fn scrollbars(
        &mut self,
        _f: &Fragment<S>,
        _b: &BoxFragment<S>,
        _at: Point,
        _scroll: Point,
        _device_scale: f32,
    ) {
    }
    /// Hit-testing bookkeeping: entering/leaving a fragment with a node.
    fn enter(&mut self, _node: Option<NodeId>) {}
    fn leave(&mut self) {}
    /// Whether the sink wants what paints inside `bounds` (a fragment's paint bounds, in the
    /// current coordinates): a query about one point (hit testing, caret placement) or a part of
    /// the page skips the subtrees that cannot matter to it.
    fn wants(&self, _bounds: Rect) -> bool {
        true
    }
}

/// Whether a box has a `background-clip: text` layer that paints (the colour counts when the
/// bottom layer is one).
fn has_text_clip<S: Style>(s: &S) -> bool {
    if s.visibility() != Visibility::Visible {
        return false;
    }
    let n = s.background_layer_count();
    (0..n).any(|i| {
        let l = s.background_layer(i);
        l.clip_text && l.image != Image::None
    }) || (s.background_color_layer().clip_text && !s.background_color().is_transparent())
}

/// Whether a box fragment's style makes a stacking context.
fn makes_stacking_context<S: Style>(b: &BoxFragment<S>) -> bool {
    is_stacking_context(&b.style, b.flags)
}

/// Whether a box with style `s` and fragment flags `flags` makes a stacking context.
pub(crate) fn is_stacking_context<S: Style>(s: &S, flags: u32) -> bool {
    let positioned = flags & fragment_flags::POSITIONED != 0;
    let z_applies = positioned || flags & fragment_flags::FLEX_GRID_ITEM != 0;
    flags & fragment_flags::ROOT != 0
        || (z_applies && s.z_index().is_some())
        || s.opacity() < 1.0
        || s.has_transform()
        || s.forces_stacking_context()
        || flags & fragment_flags::FIXED != 0
        || flags & fragment_flags::STICKY != 0 && s.z_index().is_some()
}

fn is_positioned<S: Style>(b: &BoxFragment<S>) -> bool {
    b.flags & fragment_flags::POSITIONED != 0
}

fn is_float<S: Style>(b: &BoxFragment<S>) -> bool {
    b.flags & fragment_flags::FLOAT != 0
}

/// Painted atomically, as if it were a stacking context: atomic inlines (CSS 2 Appendix E), and flex
/// and grid items, which paint exactly as inline blocks (css-flexbox-1 §4, css-grid-2 §9).
fn is_atomic<S: Style>(b: &BoxFragment<S>) -> bool {
    b.flags & (fragment_flags::ATOMIC_INLINE | fragment_flags::FLEX_GRID_ITEM) != 0
}

/// The clip of a stacking context member: the intersection of its clipping ancestors' rectangles,
/// and the rounded ones among them (CSS Backgrounds 3 §5.3: a box whose `overflow` clips with
/// rounded corners clips to its padding box curve), each pushed in turn.
#[derive(Clone, Debug, Default)]
struct Clip {
    rect: Option<Rect>,
    rounded: Vec<(Rect, Radii)>,
}

/// The clips a collecting stacking context has open: per clipping ancestor, the intersection so
/// far, how the clip moves when the viewport scrolls, its rounded shape if it has one, and its own
/// rectangle.
type ClipStack = Vec<ClipEntry>;
type ClipEntry = (Rect, Option<Anchor>, Option<(Rect, Radii)>, Rect);

impl Clip {
    fn of(clips: &[ClipEntry]) -> Clip {
        Clip {
            rect: clips.last().map(|c| c.0),
            rounded: clips.iter().filter_map(|c| c.2).collect(),
        }
    }

    /// A sticky member's clips (`anchor` is a sticky box's) split where those of boxes that scroll
    /// with the document, or the viewport's, end and those of boxes that move with the sticky box
    /// begin: what is drawn outside its layer, and what inside. None for other members, or clips
    /// in another order.
    fn split_sticky(clips: &ClipStack, anchor: Anchor) -> Option<(Clip, Clip)> {
        if !matches!(anchor, Anchor::Sticky(_)) {
            return None;
        }
        let n = clips
            .iter()
            .take_while(|c| matches!(c.1, None | Some(Anchor::Scroll)))
            .count();
        let (outside, inside) = clips.split_at(n);
        if !inside.iter().all(|c| c.1 == Some(anchor)) {
            return None;
        }
        Some((
            Clip::of(outside),
            Clip {
                rect: inside.iter().map(|c| c.3).reduce(|a, b| a.intersect(&b)),
                rounded: inside.iter().filter_map(|c| c.2).collect(),
            },
        ))
    }

    fn is_none(&self) -> bool {
        self.rect.is_none() && self.rounded.is_empty()
    }

    /// Pushes the clip; returns how many clips to pop.
    fn push<S: Style, K: Sink<S>>(&self, sink: &mut K) -> u32 {
        let mut n = 0;
        if let Some(r) = self.rect {
            sink.push_clip(r, Radii::ZERO);
            n += 1;
        }
        for &(r, radii) in &self.rounded {
            sink.push_clip(r, radii);
            n += 1;
        }
        n
    }
}

/// A stacking context member waiting for its turn.
struct Deferred<'f, S: Style> {
    f: &'f Fragment<S>,
    at: Point,
    /// The clip (in the stacking context's coordinates) of the fragment's clipping ancestors.
    clip: Clip,
    z: i32,
    order: u32,
    /// A real stacking context (else a positioned box painted as if it were one).
    real: bool,
    /// The nodes of its fragment ancestors, innermost last (for hit testing).
    path: Vec<Option<NodeId>>,
    /// How it moves when the viewport scrolls.
    anchor: Anchor,
    /// The scrollport of its nearest scroll container ancestor (`None`: the viewport's).
    port: Option<Rect>,
    /// Clipped by a box that moves differently when the viewport scrolls (not the root or the body
    /// whose overflow is the viewport's, and anchored otherwise).
    doc_clip: bool,
    /// A sticky member's clip as two: of the boxes that scroll with the document (or the
    /// viewport's), which can be drawn outside its layer by a compositor moving the layer within
    /// them, and of those that move with it ([`Clip::split_sticky`]).
    sticky_clip: Option<(Clip, Clip)>,
    /// Only the scroll bars of `f`, a scroll container that is not a stacking context: they are
    /// painted after its descendants in tree order (its positioned descendants included), as a
    /// positioned box at its last descendant would be, so that later positioned boxes and higher
    /// stacking contexts cover them.
    scrollbars: bool,
}

/// How painted content moves when the viewport scrolls.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Anchor {
    /// With the document.
    Scroll,
    /// Not at all (a fixed box).
    Viewport,
    /// With a sticky box (itself or what it contains).
    Sticky(StickyConstraint),
    /// Some other way (a sticky box in a clip or in a nested stacking context).
    Pinned,
}

/// Members collected for a stacking context other than the one collecting them (see the module
/// documentation), by the owning context's box order.
type Adopted<'f, S> = std::collections::BTreeMap<u32, Vec<Deferred<'f, S>>>;

/// Whether members are waiting in `adopted` for a stacking context in the subtree of the box
/// fragment `b` (itself included).
fn adopts_within<S: Style>(adopted: &Adopted<'_, S>, b: &BoxFragment<S>) -> bool {
    !adopted.is_empty()
        && adopted
            .range(b.order..=b.subtree_end.max(b.order))
            .next()
            .is_some()
}

/// Puts a collected member in its stacking context's list: `lists` (by its `z-index`), or the
/// adopted list of its owner.
fn route<'f, S: Style>(
    d: Deferred<'f, S>,
    owner: Option<u32>,
    lists: &mut Lists<'f, S>,
    adopted: &mut Adopted<'f, S>,
) {
    match owner {
        Some(o) => adopted.entry(o).or_default().push(d),
        None => match d.z.cmp(&0) {
            std::cmp::Ordering::Less => lists.negative.push(d),
            std::cmp::Ordering::Equal => lists.zero.push(d),
            std::cmp::Ordering::Greater => lists.positive.push(d),
        },
    }
}

/// A sticky box's constraints (css-position-3 §3.4) for a box at `at` (unshifted) whose containing
/// block's content box is `container`: kept inside the sticky view rectangle (the scrollport `port`
/// less the insets, which resolve against its size) as far as its margin box stays inside its
/// containing block. `None` for a box that isn't sticky.
pub(crate) fn sticky_constraint<S: Style>(
    f: &Fragment<S>,
    b: &BoxFragment<S>,
    at: Point,
    container: Rect,
    port: Rect,
) -> Option<StickyConstraint> {
    if b.flags & fragment_flags::STICKY == 0 {
        return None;
    }
    let s = &b.style;
    let inset = |side: Side, basis: f32| s.inset(side).resolve(basis);
    let mut insets = [
        inset(Side::Top, port.height),
        inset(Side::Right, port.width),
        inset(Side::Bottom, port.height),
        inset(Side::Left, port.width),
    ];
    // A sticky view rectangle smaller than the border box: the end-edge inset gives way (in the
    // containing block's writing mode).
    let rtl = s.direction() == Direction::Rtl;
    if let [Some(t), _, Some(bm), _] = insets {
        let room = port.height - t - bm;
        if room < f.size.height {
            insets[2] = Some(bm - (f.size.height - room));
        }
    }
    if let [_, Some(r), _, Some(l)] = insets {
        let room = port.width - l - r;
        if room < f.size.width {
            let end = if rtl { 3 } else { 1 };
            insets[end] = insets[end].map(|v| v - (f.size.width - room));
        }
    }
    // The position box is the margin box (a margin reaching past the containing block counts
    // only up to its edge, which the shift's limits already give).
    let cbw = container.width;
    let m = |side: Side| s.margin(side).resolve_or_zero(cbw);
    let container = Rect::from_ltrb(
        container.x + m(Side::Left),
        container.y + m(Side::Top),
        container.right() - m(Side::Right),
        container.bottom() - m(Side::Bottom),
    );
    Some(StickyConstraint {
        rect: Rect::new(at.x, at.y, f.size.width, f.size.height),
        container,
        insets,
    })
}

/// The rectangle sticky boxes inside `f` (at `at`) are kept in view of, `outer` being the one
/// outside it (`None`: the viewport `viewport`): in each axis in which `f` is a scroll container,
/// its scrollport less its padding (a stuck box keeps clear of the scroll container's padding, as
/// in browsers); in another axis, the outer one's (single-axis scroll containers, css-position-3
/// §3.4). `None` while it is still the viewport's.
fn sticky_port<S: Style>(
    f: &Fragment<S>,
    at: Point,
    outer: Option<Rect>,
    viewport: Rect,
) -> Option<Rect> {
    let Some(b) = f.as_box() else {
        return outer;
    };
    let viewport_box = b.flags
        & (fragment_flags::INITIAL_CONTAINING_BLOCK
            | fragment_flags::ROOT
            | fragment_flags::VIEWPORT_OVERFLOW)
        != 0;
    if viewport_box || b.flags & fragment_flags::SCROLL_CONTAINER == 0 {
        return outer;
    }
    let x = b.style.overflow_x().is_scroll_container();
    let y = b.style.overflow_y().is_scroll_container();
    let c = f.content_rect().translate(at.x, at.y);
    let o = outer.unwrap_or(viewport);
    Some(Rect::from_ltrb(
        if x { c.x } else { o.x },
        if y { c.y } else { o.y },
        if x { c.right() } else { o.right() },
        if y { c.bottom() } else { o.bottom() },
    ))
}

/// The content box of `f` at `at` when it is the containing block of sticky boxes in it (a block
/// container or other box that is not an inline box; for table rows and row groups, the table
/// as in browsers), else `outer`, the one around it. A scroll container scrolled by `scrolled`
/// contains its sticky children with its scrollable area (less its padding), which moves as it
/// scrolls.
fn sticky_cb<S: Style>(f: &Fragment<S>, at: Point, outer: Rect, scrolled: Option<Point>) -> Rect {
    // (anonymous blocks are not containing blocks here: an inline's is its block container)
    let anonymous = |b: &BoxFragment<S>| {
        b.node.is_none()
            && b.pseudo.is_none()
            && b.flags & fragment_flags::INITIAL_CONTAINING_BLOCK == 0
    };
    match f.as_box() {
        Some(b)
            if b.flags & (fragment_flags::INLINE_BOX | fragment_flags::TABLE_PART) == 0
                && !anonymous(b) =>
        {
            let content = f.content_rect();
            let viewport_box = b.flags
                & (fragment_flags::INITIAL_CONTAINING_BLOCK
                    | fragment_flags::ROOT
                    | fragment_flags::VIEWPORT_OVERFLOW)
                != 0;
            if b.flags & fragment_flags::SCROLL_CONTAINER != 0 && !viewport_box {
                let area = f.scrollable_overflow();
                let s = scrolled.unwrap_or(Point::ZERO);
                // (the area reaches before the content box where the scroll origin is at the
                // end: rtl, reversed flex directions)
                return Rect::from_ltrb(
                    content.x.min(area.x + b.padding.left),
                    content.y.min(area.y + b.padding.top),
                    content.right().max(area.right() - b.padding.right),
                    content.bottom().max(area.bottom() - b.padding.bottom),
                )
                .translate(at.x - s.x, at.y - s.y);
            }
            content.translate(at.x, at.y)
        }
        _ => outer,
    }
}

/// Where a child of `parent` (at `at`) is drawn when the viewport is scrolled by `viewport_scroll`
/// and scroll containers by `scroll` (as [`Painter::child_at`]).
fn scrolled_child_at<S: Style>(
    parent: &Fragment<S>,
    at: Point,
    child: &FragmentChild<S>,
    viewport_scroll: Point,
    scroll: &std::collections::HashMap<NodeId, Point>,
) -> Point {
    let mut p = Point::new(at.x + child.offset.x, at.y + child.offset.y);
    if let Some(b) = parent.as_box() {
        if b.flags & fragment_flags::INITIAL_CONTAINING_BLOCK != 0 {
            let fixed = child
                .fragment
                .as_box()
                .is_some_and(|cb| cb.flags & fragment_flags::FIXED != 0);
            if !fixed {
                p.x -= viewport_scroll.x;
                p.y -= viewport_scroll.y;
            }
        } else if b.flags & fragment_flags::SCROLL_CONTAINER != 0 {
            if let Some(s) = b.node.and_then(|n| scroll.get(&n)) {
                p.x -= s.x;
                p.y -= s.y;
            }
        }
    }
    p
}

/// How far sticky positioning (css-position-3 §3.4, §3.4.1) moves the box fragment numbered
/// `order` (its own shift and its sticky ancestors'), with the viewport `viewport` scrolled by
/// `viewport_scroll` and scroll containers by `scroll`; as painting places it.
pub(crate) fn sticky_shift<S: Style>(
    root: &Fragment<S>,
    order: u32,
    viewport: Size,
    viewport_scroll: Point,
    scroll: &std::collections::HashMap<NodeId, Point>,
) -> Option<Point> {
    #[allow(clippy::too_many_arguments)]
    fn walk<S: Style>(
        f: &Fragment<S>,
        at: Point,
        port: Option<Rect>,
        cb: Rect,
        shift: Point,
        order: u32,
        viewport: Rect,
        viewport_scroll: Point,
        scroll: &std::collections::HashMap<NodeId, Point>,
    ) -> Option<Point> {
        let port = sticky_port(f, at, port, viewport);
        let cb = sticky_cb(
            f,
            at,
            cb,
            f.as_box()
                .and_then(|b| b.node)
                .and_then(|n| scroll.get(&n).copied()),
        );
        for child in &f.children {
            let cf = &*child.fragment;
            let mut cat = scrolled_child_at(f, at, child, viewport_scroll, scroll);
            let mut shift = shift;
            if let Some(b) = cf.as_box() {
                // (only the subtree holding the box; anonymous boxes' numbers need not bound
                // their subtrees)
                if b.node.is_some() && (order < b.order || order > b.subtree_end.max(b.order)) {
                    continue;
                }
                if let Some(c) = sticky_constraint(cf, b, cat, cb, port.unwrap_or(viewport)) {
                    let d = c.offset(port.unwrap_or(viewport));
                    cat = Point::new(cat.x + d.x, cat.y + d.y);
                    shift = Point::new(shift.x + d.x, shift.y + d.y);
                }
                if b.order == order {
                    return Some(shift);
                }
            }
            if let Some(found) = walk(
                cf,
                cat,
                port,
                cb,
                shift,
                order,
                viewport,
                viewport_scroll,
                scroll,
            ) {
                return Some(found);
            }
        }
        None
    }
    walk(
        root,
        Point::ZERO,
        None,
        Rect::new(0.0, 0.0, viewport.width, viewport.height),
        Point::ZERO,
        order,
        Rect::new(0.0, 0.0, viewport.width, viewport.height),
        viewport_scroll,
        scroll,
    )
}

struct Lists<'f, S: Style> {
    negative: Vec<Deferred<'f, S>>,
    zero: Vec<Deferred<'f, S>>,
    positive: Vec<Deferred<'f, S>>,
    /// Top layer boxes (CSS Position 4 §4.1), each a stacking context painted last.
    top: Vec<Deferred<'f, S>>,
}

/// The painter state.
pub(crate) struct Painter<'k, K> {
    pub sink: &'k mut K,
    /// The viewport (the scrollport of sticky boxes without a scrolling ancestor).
    pub viewport: Rect,
    /// How far the viewport is scrolled (moves everything but fixed boxes).
    pub viewport_scroll: Point,
    /// Scroll positions of scroll containers, by node.
    pub scroll: Option<&'k std::collections::HashMap<NodeId, Point>>,
    /// Mark fixed and sticky content as layers (see the module documentation).
    pub layers: bool,
    /// Clip, transform and opacity groups open around what is being painted (from the stacking
    /// contexts and clipped positioned boxes it is in): a layer must be outside all of them.
    pub groups: u32,
    /// Painting a layer's content.
    pub in_layer: bool,
    /// Painting the content of a layer attached to the viewport ([`LayerKind::Fixed`]).
    pub in_fixed: bool,
    /// The scrollport of the nearest scroll container around what is being painted (`None`: the
    /// viewport's).
    pub port: Option<Rect>,
    /// Device pixels per CSS px.
    pub device_scale: f32,
    /// The content box of the containing block of the sticky boxes being collected (the nearest
    /// box around them that is not an inline box).
    pub sticky_cb: std::cell::Cell<Rect>,
    /// See [`crate::PaintOptions::composited`].
    pub composited: Option<&'k dyn Fn(NodeId) -> bool>,
    /// See [`crate::PaintOptions::animations`].
    pub animations: Option<&'k crate::CompositorAnimations<'k>>,
}

impl<K> Painter<'_, K> {
    /// A scroll offset as painting applies it: in whole device pixels, as Chrome paints scrolled
    /// content (its scroll offsets are integers), so that boxes on pixel edges stay on them.
    fn snapped_scroll(&self, s: Point) -> Point {
        let d = self.device_scale;
        Point::new((s.x * d).round() / d, (s.y * d).round() / d)
    }

    /// The scroll position of a scroll container of its own (the root's and a propagating body's
    /// is the viewport's: `None`, as for a box that is no scroll container).
    fn own_scroll<S: Style>(&self, b: &BoxFragment<S>) -> Option<Point> {
        let viewport = fragment_flags::ROOT
            | fragment_flags::VIEWPORT_OVERFLOW
            | fragment_flags::INITIAL_CONTAINING_BLOCK;
        if b.flags & fragment_flags::SCROLL_CONTAINER == 0 || b.flags & viewport != 0 {
            return None;
        }
        let s = match (self.scroll, b.node) {
            (Some(map), Some(n)) => map.get(&n).copied().unwrap_or(Point::ZERO),
            _ => Point::ZERO,
        };
        Some(self.snapped_scroll(s))
    }

    /// With layers, a box with a `background-attachment: fixed` layer paints differently at each
    /// scroll position of the viewport: its decorations are marked so (a layer pushed here and
    /// popped by the caller when this returns true). `canvas`: they are the canvas background's,
    /// under everything and outside every group; when all its images are attached to the viewport
    /// nothing of it moves with the document (its color covers the whole canvas), and it is a
    /// layer that stays in place ([`LayerKind::Fixed`]). Any other is [`LayerKind::Pinned`].
    fn pin_fixed_background<S: Style>(&mut self, s: &S, canvas: bool) -> bool
    where
        K: Sink<S>,
    {
        if !self.layers {
            return false;
        }
        let (mut fixed, mut scrolls) = (false, false);
        for i in 0..s.background_layer_count() {
            let l = s.background_layer(i);
            if l.image == Image::None {
                continue;
            }
            if l.attachment == BackgroundAttachment::Fixed {
                fixed = true;
            } else {
                scrolls = true;
            }
        }
        if fixed {
            let stays = canvas && !scrolls && self.groups == 0 && !self.in_layer;
            self.sink.push_layer(if stays {
                LayerKind::Fixed
            } else {
                LayerKind::Pinned
            });
        }
        fixed
    }

    /// Where a child of `parent` (at `at`) paints: its offset, minus the parent's scroll position
    /// when the parent is a scroll container (and minus the viewport's for the initial containing
    /// block's non-fixed children).
    fn child_at<S: Style>(
        &self,
        parent: &Fragment<S>,
        at: Point,
        child: &FragmentChild<S>,
    ) -> Point {
        let mut p = Point::new(at.x + child.offset.x, at.y + child.offset.y);
        if let Some(b) = parent.as_box() {
            if b.flags & fragment_flags::INITIAL_CONTAINING_BLOCK != 0 {
                let fixed = child
                    .fragment
                    .as_box()
                    .is_some_and(|cb| cb.flags & fragment_flags::FIXED != 0);
                if !fixed {
                    let s = self.snapped_scroll(self.viewport_scroll);
                    p.x -= s.x;
                    p.y -= s.y;
                }
            } else if b.flags & fragment_flags::SCROLL_CONTAINER != 0 {
                if let (Some(map), Some(node)) = (self.scroll, b.node) {
                    if let Some(s) = map.get(&node) {
                        let s = self.snapped_scroll(*s);
                        p.x -= s.x;
                        p.y -= s.y;
                    }
                }
            }
        }
        p
    }

    /// Clips what follows to one of a box's `clip-path` clips ([`crate::clip::clip_geometry`]).
    fn push_clip_geometry<S: Style>(&mut self, clip: &ClipGeometry)
    where
        K: Sink<S>,
    {
        match clip {
            // (a rectangle without rounded corners is a rectangle clip, its edges snapped as
            // the boxes' are, as the reference browser has it)
            ClipGeometry::Rounded(rect, radii) if radii.is_zero() => {
                self.sink.push_clip(*rect, *radii)
            }
            ClipGeometry::Rounded(rect, radii) => self.sink.push_clip_rounded_path(*rect, *radii),
            ClipGeometry::Path { contours, even_odd } => {
                self.sink.push_clip_path(contours, *even_odd)
            }
        }
        self.groups += 1;
    }

    fn pop_clip_geometry<S: Style>(&mut self, clip: &ClipGeometry)
    where
        K: Sink<S>,
    {
        match clip {
            ClipGeometry::Rounded(_, radii) if radii.is_zero() => self.sink.pop_clip(),
            ClipGeometry::Rounded(..) => self.sink.pop_clip_rounded_path(),
            ClipGeometry::Path { .. } => self.sink.pop_clip_path(),
        }
        self.groups -= 1;
    }

    /// Paints a fragment tree whose root fragment is the initial containing block.
    pub fn paint_root<S: Style>(&mut self, root: &Fragment<S>)
    where
        K: Sink<S>,
    {
        // The canvas background (CSS Backgrounds 3 §2.11).
        let mut canvas: Option<(&Fragment<S>, Point)> = None;
        find_canvas(root, Point::ZERO, &mut canvas);
        if let Some((f, at)) = canvas {
            if let Some(b) = f.as_box() {
                // (positioned against the root element's box, also when the background is the
                // body's: CSS Backgrounds 3 §2.11.1)
                let root_box = root
                    .children
                    .iter()
                    .find(|c| {
                        c.fragment
                            .as_box()
                            .is_some_and(|b| b.flags & fragment_flags::ROOT != 0)
                    })
                    .map(|c| (&*c.fragment, c.offset));
                let (rf, rat) = root_box.unwrap_or((f, at));
                // (a `clip-path` on the root element clips the canvas too: the canvas is painted
                // as part of the root element's stacking context, which the clip applies to)
                let clips = match rf.as_box() {
                    Some(rb) if rb.flags & fragment_flags::ROOT != 0 => {
                        crate::clip::clip_geometry(rf, rb, rat)
                    }
                    _ => Vec::new(),
                };
                // (a canvas the root's `clip-path` cuts moves with that clip)
                let pinned = self.pin_fixed_background(&b.style, clips.is_empty());
                for clip in &clips {
                    self.push_clip_geometry(clip);
                }
                self.sink.decorations(rf, b, rat, true, None);
                for clip in clips.iter().rev() {
                    self.pop_clip_geometry(clip);
                }
                if pinned {
                    self.sink.pop_layer();
                }
            }
        }
        let mut adopted = Adopted::new();
        self.stacking_context(root, Point::ZERO, &mut Vec::new(), &mut adopted, None);
        // (members whose stacking context was not painted: there are none unless the fragment
        // tree is inconsistent; they are painted rather than lost)
        for d in std::mem::take(&mut adopted).into_values().flatten() {
            self.paint_deferred(&d, &mut adopted);
        }
    }

    /// Paints a stacking context rooted at `f` (its transform and opacity are applied by the caller).
    /// `base_clip` is the clip of its ancestors when the caller did not apply it because members
    /// adopted from outside (which it must not clip) are painted with it: it then clips the
    /// context's own content and its other members.
    fn stacking_context<'f, S: Style>(
        &mut self,
        f: &'f Fragment<S>,
        at: Point,
        path: &mut Vec<Option<NodeId>>,
        adopted: &mut Adopted<'f, S>,
        base_clip: Option<&Clip>,
    ) where
        K: Sink<S>,
    {
        let mut lists = Lists {
            negative: Vec::new(),
            zero: Vec::new(),
            positive: Vec::new(),
            top: Vec::new(),
        };
        path.push(f.node());
        // (the base clip comes from elsewhere: content under it is never a free layer, as when it
        // is pushed around the context)
        let mut clips: ClipStack = Vec::new();
        if let Some(c) = base_clip {
            let mut rect = c.rect.unwrap_or_else(Rect::infinite);
            clips.push((rect, Some(Anchor::Pinned), None, rect));
            for &(r, radii) in &c.rounded {
                rect = rect.intersect(&r);
                clips.push((rect, Some(Anchor::Pinned), Some((r, radii)), r));
            }
        }
        self.collect(
            f,
            at,
            &mut clips,
            &mut lists,
            path,
            Anchor::Scroll,
            self.port,
            adopted,
            None,
        );
        if let Some(mine) = f.as_box().and_then(|b| adopted.remove(&b.order)) {
            for d in mine {
                match d.z.cmp(&0) {
                    std::cmp::Ordering::Less => lists.negative.push(d),
                    std::cmp::Ordering::Equal => lists.zero.push(d),
                    std::cmp::Ordering::Greater => lists.positive.push(d),
                }
            }
        }
        lists.negative.sort_by_key(|d| (d.z, d.order));
        lists.positive.sort_by_key(|d| (d.z, d.order));
        lists.zero.sort_by_key(|d| (d.order, d.scrollbars));
        lists.top.sort_by_key(|d| d.order);
        let mut opened = 0;
        let mut own_clip = |p: &mut Self, on: bool| {
            if let Some(c) = base_clip {
                if on {
                    opened = c.push::<S, K>(p.sink);
                    p.groups += opened;
                } else {
                    for _ in 0..opened {
                        p.sink.pop_clip();
                    }
                    p.groups -= opened;
                }
            }
        };
        // 1. the root's background and borders
        self.sink.enter(f.node());
        if let Some(b) = f.as_box() {
            own_clip(self, true);
            self.decorations_of(f, b, at);
            own_clip(self, false);
        }
        // 2. negative z-index
        for d in &lists.negative {
            self.paint_deferred(d, adopted);
        }
        // 3–5. in-flow blocks, floats, inline content
        own_clip(self, true);
        self.paint_flow(f, at, true);
        own_clip(self, false);
        // 6. positioned descendants with z-index auto/0
        for d in &lists.zero {
            self.paint_deferred(d, adopted);
        }
        // 7. positive z-index
        for d in &lists.positive {
            self.paint_deferred(d, adopted);
        }
        // 10. outlines
        own_clip(self, true);
        self.outlines(f, at, true);
        own_clip(self, false);
        // The top layer, above everything, in its order.
        for d in &lists.top {
            self.paint_deferred(d, adopted);
        }
        self.sink.leave();
        path.pop();
    }

    fn paint_deferred<'f, S: Style>(&mut self, d: &Deferred<'f, S>, adopted: &mut Adopted<'f, S>)
    where
        K: Sink<S>,
    {
        // (a clip of the document's would have to move with the document)
        let free = !d.doc_clip && self.groups == 0 && !self.in_layer;
        // A sticky box clipped only by boxes that move with the document: its clip is pushed
        // outside its layer, and a compositor moves the layer within it (a stacking context with
        // adopted members in it takes its clip inside, off them, so it stays pinned).
        let detached = d.real && d.f.as_box().is_some_and(|b| adopts_within(adopted, b));
        let clipped = d.doc_clip
            && d.sticky_clip.is_some()
            && !detached
            && self.groups == 0
            && !self.in_layer
            && matches!(d.anchor, Anchor::Sticky(_));
        let layer = match d.anchor {
            _ if !self.layers => None,
            Anchor::Scroll => None,
            // (a fixed box painted in a fixed box's layer, whose stacking context it belongs to,
            // stays in place with it)
            Anchor::Viewport if self.in_fixed && !d.doc_clip => None,
            Anchor::Viewport if free => Some(LayerKind::Fixed),
            Anchor::Sticky(c) if free || clipped => Some(LayerKind::Sticky(c)),
            _ => Some(LayerKind::Pinned),
        };
        let split = d
            .sticky_clip
            .as_ref()
            .filter(|_| clipped && layer.is_some());
        let opened = match split {
            Some((outside, _)) => outside.push::<S, K>(self.sink),
            None => 0,
        };
        self.groups += opened;
        if let Some(kind) = layer {
            self.sink.push_layer(kind);
        }
        let was = (self.in_layer, self.in_fixed);
        self.in_layer = was.0 || layer.is_some();
        self.in_fixed = match layer {
            Some(LayerKind::Fixed) => true,
            Some(_) => false,
            None => was.1,
        };
        self.paint_deferred_content(d, adopted, split.map_or(&d.clip, |(_, inside)| inside));
        (self.in_layer, self.in_fixed) = was;
        if layer.is_some() {
            self.sink.pop_layer();
        }
        for _ in 0..opened {
            self.sink.pop_clip();
        }
        self.groups -= opened;
    }

    fn paint_deferred_content<'f, S: Style>(
        &mut self,
        d: &Deferred<'f, S>,
        adopted: &mut Adopted<'f, S>,
        clip: &Clip,
    ) where
        K: Sink<S>,
    {
        // (a stacking context with adopted members in it takes its clip inside, off them)
        let detached = d.real && d.f.as_box().is_some_and(|b| adopts_within(adopted, b));
        let opened = if detached {
            0
        } else {
            clip.push::<S, K>(self.sink)
        };
        self.groups += opened;
        let port = std::mem::replace(&mut self.port, d.port);
        let mut path = d.path.clone();
        for n in &path {
            self.sink.enter(*n);
        }
        if d.scrollbars {
            if let Some(b) = d.f.as_box() {
                self.sink.enter(d.f.node());
                self.scrollbars_of(d.f, b, d.at);
                self.sink.leave();
            }
        } else if d.real {
            let base = (detached && !d.clip.is_none()).then_some(&d.clip);
            self.with_effects(d.f, d.at, &mut path, adopted, base);
        } else {
            // A positioned box with z-index auto: atomically, its positioned descendants excluded.
            self.sink.enter(d.f.node());
            if let Some(b) = d.f.as_box() {
                self.decorations_of(d.f, b, d.at);
            }
            self.paint_flow(d.f, d.at, true);
            self.outlines(d.f, d.at, true);
            self.sink.leave();
        }
        for _ in &path {
            self.sink.leave();
        }
        self.port = port;
        for _ in 0..opened {
            self.sink.pop_clip();
        }
        self.groups -= opened;
    }

    /// Applies a stacking context root's transform and opacity around painting it.
    fn with_effects<'f, S: Style>(
        &mut self,
        f: &'f Fragment<S>,
        at: Point,
        path: &mut Vec<Option<NodeId>>,
        adopted: &mut Adopted<'f, S>,
        base_clip: Option<&Clip>,
    ) where
        K: Sink<S>,
    {
        let Some(b) = f.as_box() else {
            self.stacking_context(f, at, path, adopted, base_clip);
            return;
        };
        let s = &b.style;
        // A direct compositing reason (Blink's `CompositingReasonFinder::DirectReasonsForPaintProperties`:
        // a transform or opacity animation in effect, `will-change: transform | opacity`) makes the
        // stacking context a layer of its own inside its transform and opacity groups, which a
        // compositor draws through them: a frame that changes only those repaints nothing. Not
        // inside a fixed or sticky layer, which moves as a whole.
        let composited = (self.layers && !self.in_layer)
            .then_some(b.node)
            .flatten()
            .filter(|&n| self.composited.is_some_and(|c| c(n)) || s.will_change_composited());
        // The animations a compositor runs for it (S2): only for a transform in the box's own
        // coordinates (no writing mode flip, no motion path, which need more than a list).
        let mut animations = Vec::new();
        if let (Some(node), Some(f_anim)) = (composited, self.animations) {
            if s.view().is_identity() && !s.transform_needs_containing() {
                let mut reference = Rect::new(0.0, 0.0, f.size.width, f.size.height);
                if s.transform_box_is_content() {
                    reference = reference.deflate(&b.border).deflate(&b.padding);
                }
                f_anim(node, reference, at, &mut animations);
            }
        }
        // (a box with compositor animations always has its own transform and opacity groups, the
        // two state steps the compositor writes, and one whose filter animation it runs its filter
        // group around its layer, the third)
        let animated = !animations.is_empty();
        let filter_animated = animations
            .iter()
            .any(|a| a.property() == snowlark_paint::layer_animation::Property::Filter);
        let transform = b
            .used_transform(f.size)
            .or(animated.then_some(Matrix::IDENTITY));
        if let Some(m) = transform {
            let full = Matrix::translation(-at.x, -at.y)
                .then(&m)
                .then(&Matrix::translation(at.x, at.y));
            self.sink.push_transform(full);
            self.groups += 1;
        }
        let opacity = s.opacity();
        // (a backdrop root groups what it paints, at full opacity if that is all: what its
        // descendants' backdrop filters read starts there, Filter Effects 2 §3.1)
        let grouped = opacity < 1.0 || animated || s.forms_backdrop_root();
        let border_box = Rect::new(at.x, at.y, f.size.width, f.size.height);
        let filters = if s.has_filter() {
            s.filter_ops(border_box)
        } else {
            Vec::new()
        };
        // A group's bounds: where the subtree paints (its filters' outsets included), and the
        // adopted members, which are painted in the group too.
        let group_bounds = || {
            let mut bounds = f.paint_bounds.translate(at.x, at.y);
            if adopts_within(adopted, b) {
                for d in adopted
                    .range(b.order..=b.subtree_end.max(b.order))
                    .flat_map(|(_, v)| v)
                {
                    bounds = bounds.union(&d.f.paint_bounds.translate(d.at.x, d.at.y));
                }
            }
            bounds
        };
        if grouped {
            self.sink.push_opacity(opacity.max(0.0), group_bounds());
            self.groups += 1;
        }
        // (color functions apply to each pixel on its own, so they commute with the box's clips and
        // mask: outside the layer, the compositor can write them)
        if filter_animated {
            self.sink.push_filter(&filters, group_bounds());
            self.groups += 1;
        }
        if let Some(node) = composited {
            self.sink.push_layer(LayerKind::Composited(node.to_bits()));
            for a in animations {
                self.sink.add_animation(a);
            }
        }
        let clips = crate::clip::clip_geometry(f, b, at);
        for clip in &clips {
            self.push_clip_geometry(clip);
        }
        let masked = self.sink.push_mask(f, b, at);
        if masked {
            self.groups += 1;
        }
        // CSS Masking 1 §6.1: `clip` applies to absolutely positioned boxes.
        let clip = if b.flags & (fragment_flags::ABSOLUTE | fragment_flags::FIXED) != 0 {
            s.clip_rect()
        } else {
            None
        };
        if let Some([t, r, bottom, l]) = clip {
            let rect = Rect::from_ltrb(
                at.x + l.unwrap_or(0.0),
                at.y + t.unwrap_or(0.0),
                at.x + r.unwrap_or(f.size.width),
                at.y + bottom.unwrap_or(f.size.height),
            );
            self.sink.push_clip(rect, Radii::default());
            self.groups += 1;
        }
        // The backdrop filter (Filter Effects 2 §3): what is under the border box, filtered, then
        // the element over it, the element's opacity, clip-path and mask applying to both. Its own
        // filter applies to the backdrop too (the spec's filter is on the whole of it; the
        // backdrop is drawn outside the filter group, so it gets the functions after its own).
        // (the root element has no backdrop: it is where every backdrop root's content starts)
        let backdrop = if b.flags & fragment_flags::ROOT == 0 {
            s.backdrop_filter_ops(border_box)
        } else {
            Vec::new()
        };
        if !backdrop.is_empty() {
            let mut ops = backdrop;
            ops.extend_from_slice(&filters);
            let rect = Rect::new(at.x, at.y, f.size.width, f.size.height);
            let own = u8::from(grouped) + u8::from(masked);
            self.sink.backdrop_filter(&ops, rect, radii(s, f.size), own);
        }
        // The filter applies to the content first (Filter Effects 1 §5: before clipping, masking
        // and opacity), and what clips the box clips its result.
        let inner_filter = !filters.is_empty() && !filter_animated;
        if inner_filter {
            self.sink.push_filter(&filters, group_bounds());
            self.groups += 1;
        }
        self.stacking_context(f, at, path, adopted, base_clip);
        if inner_filter {
            self.sink.pop_filter();
            self.groups -= 1;
        }
        if clip.is_some() {
            self.sink.pop_clip();
            self.groups -= 1;
        }
        if masked {
            self.sink.pop_mask();
            self.groups -= 1;
        }
        for clip in clips.iter().rev() {
            self.pop_clip_geometry(clip);
        }
        if composited.is_some() {
            self.sink.pop_layer();
        }
        if filter_animated {
            self.sink.pop_filter();
            self.groups -= 1;
        }
        if grouped {
            self.sink.pop_opacity();
            self.groups -= 1;
        }
        if transform.is_some() {
            self.sink.pop_transform();
            self.groups -= 1;
        }
    }

    /// Collects the stacking context members under `f` (not descending into nested stacking
    /// contexts), with the clips that apply to them and how they move when the viewport scrolls
    /// (`anchor`: as `f` does). `port` is the scrollport around `f` (`None`: the viewport's).
    /// Members that belong to a nested stacking context instead (a positioned box whose fragment
    /// link names its owner, and what is collected under it) go to `adopted`; `target` is the
    /// owner of what is collected under `f`, if any. Children with an owner are collected first,
    /// so that the subtree holding their owner is not culled before they are known.
    #[allow(clippy::too_many_arguments)]
    fn collect<'f, S: Style>(
        &self,
        f: &'f Fragment<S>,
        at: Point,
        clips: &mut ClipStack,
        lists: &mut Lists<'f, S>,
        path: &mut Vec<Option<NodeId>>,
        anchor: Anchor,
        port: Option<Rect>,
        adopted: &mut Adopted<'f, S>,
        target: Option<u32>,
    ) where
        K: Sink<S>,
    {
        let is_icb = f
            .as_box()
            .is_some_and(|b| b.flags & fragment_flags::INITIAL_CONTAINING_BLOCK != 0);
        // (the root's and a propagating body's overflow is the viewport's: CSS Overflow 3 §3.3)
        let viewport_box = f.as_box().is_some_and(|b| {
            b.flags & (fragment_flags::ROOT | fragment_flags::VIEWPORT_OVERFLOW) != 0
        });
        let pushed = f.as_box().and_then(|b| clip_rect(f, b, at));
        if let Some(c) = pushed {
            let rounded = f
                .as_box()
                .map(|b| clip_radii(f, b))
                .filter(|r| !r.is_zero() && !viewport_box)
                .map(|r| (c, r));
            let own = c;
            let c = clips.last().map_or(c, |p| p.0.intersect(&c));
            clips.push((c, (!viewport_box).then_some(anchor), rounded, own));
        }
        // A scroll container is the scrollport of the sticky boxes in it (css-position-3 §3.4).
        let port = sticky_port(f, at, port, self.viewport);
        let outer_cb = self.sticky_cb.get();
        let scrolled = f
            .as_box()
            .and_then(|b| b.node)
            .and_then(|n| self.scroll.and_then(|m| m.get(&n).copied()));
        self.sticky_cb.set(sticky_cb(f, at, outer_cb, scrolled));
        let owned = f.visible_children().iter().any(|c| c.stacking_owner != 0);
        let passes = if owned { 2 } else { 1 };
        for pass in 0..passes {
            for child in f.visible_children() {
                if owned && (child.stacking_owner != 0) != (pass == 0) {
                    continue;
                }
                let cat = self.child_at(f, at, child);
                let cf = &*child.fragment;
                // (a box's own transform is not in its bounds: it is painted, and tested, under
                // it; a subtree holding the owner of adopted members is not skipped)
                let transformed = cf.as_box().is_some_and(|b| b.style.has_transform());
                if !transformed
                    && cf
                        .as_box()
                        .is_none_or(|b| b.flags & fragment_flags::STICKY == 0)
                    && !self.sink.wants(cf.paint_bounds.translate(cat.x, cat.y))
                {
                    let holds_owner = match (cf.as_box(), f.as_box()) {
                        (Some(b), _) | (None, Some(b)) => adopts_within(adopted, b),
                        // (a line inside a line: its box is not known here)
                        (None, None) => !adopted.is_empty(),
                    };
                    if !holds_owner {
                        continue;
                    }
                }
                let owner = match child.stacking_owner {
                    0 => target,
                    o => Some(o),
                };
                if let Some(cb) = cf.as_box() {
                    let view = port.unwrap_or(self.viewport);
                    // (one that fills its containing block is never shifted: it moves as what
                    // holds it does, as any other box there, and needs no layer)
                    let constraint = sticky_constraint(cf, cb, cat, self.sticky_cb.get(), view)
                        .filter(StickyConstraint::moves);
                    let cat = match &constraint {
                        Some(c) => {
                            let shift = c.offset(view);
                            Point::new(cat.x + shift.x, cat.y + shift.y)
                        }
                        None => cat,
                    };
                    let anchor = if cb.flags & fragment_flags::FIXED != 0 && is_icb {
                        Anchor::Viewport
                    } else if port.is_some() {
                        // (sticky in a scroll container: it moves as that does)
                        anchor
                    } else if let Some(c) = constraint {
                        // (only sticky boxes outside groups, not nested in another sticky box
                        // or a layer, and clipped by nothing or by boxes that scroll with the
                        // document, scroll with the viewport alone)
                        if self.groups == 0
                            && !self.in_layer
                            && scroll_clip(clips)
                            && anchor == Anchor::Scroll
                        {
                            Anchor::Sticky(c)
                        } else {
                            Anchor::Pinned
                        }
                    } else {
                        anchor
                    };
                    if cb.flags & fragment_flags::TOP_LAYER != 0 && is_icb {
                        lists.top.push(Deferred {
                            f: cf,
                            at: cat,
                            clip: Clip::of(clips),
                            z: 0,
                            order: cb.order,
                            real: true,
                            path: path.clone(),
                            anchor,
                            port,
                            doc_clip: moving_clip(clips, anchor),
                            sticky_clip: Clip::split_sticky(clips, anchor),
                            scrollbars: false,
                        });
                        continue;
                    }
                    if makes_stacking_context(cb) {
                        let z_applies =
                            is_positioned(cb) || cb.flags & fragment_flags::FLEX_GRID_ITEM != 0;
                        let z = if z_applies {
                            cb.style.z_index().unwrap_or(0)
                        } else {
                            0
                        };
                        let d = Deferred {
                            f: cf,
                            at: cat,
                            clip: Clip::of(clips),
                            z,
                            order: cb.order,
                            real: true,
                            path: path.clone(),
                            anchor,
                            port,
                            doc_clip: moving_clip(clips, anchor),
                            sticky_clip: Clip::split_sticky(clips, anchor),
                            scrollbars: false,
                        };
                        route(d, owner, lists, adopted);
                        continue;
                    }
                    if is_positioned(cb) {
                        let d = Deferred {
                            f: cf,
                            at: cat,
                            clip: Clip::of(clips),
                            z: 0,
                            order: cb.order,
                            real: false,
                            path: path.clone(),
                            anchor,
                            port,
                            doc_clip: moving_clip(clips, anchor),
                            sticky_clip: Clip::split_sticky(clips, anchor),
                            scrollbars: false,
                        };
                        route(d, owner, lists, adopted);
                    }
                    if has_own_scrollbars(cb) {
                        let d = Deferred {
                            f: cf,
                            at: cat,
                            clip: Clip::of(clips),
                            z: 0,
                            order: cb.subtree_end.max(cb.order),
                            real: false,
                            path: path.clone(),
                            anchor,
                            port,
                            doc_clip: moving_clip(clips, anchor),
                            sticky_clip: Clip::split_sticky(clips, anchor),
                            scrollbars: true,
                        };
                        route(d, owner, lists, adopted);
                    }
                    path.push(cb.node);
                    self.collect(cf, cat, clips, lists, path, anchor, port, adopted, owner);
                    path.pop();
                } else if !cf.visible_children().is_empty() {
                    path.push(None);
                    self.collect(cf, cat, clips, lists, path, anchor, port, adopted, owner);
                    path.pop();
                }
            }
        }
        self.sticky_cb.set(outer_cb);
        if pushed.is_some() {
            clips.pop();
        }
    }

    fn decorations_of<S: Style>(&mut self, f: &Fragment<S>, b: &BoxFragment<S>, at: Point)
    where
        K: Sink<S>,
    {
        if b.flags & fragment_flags::INITIAL_CONTAINING_BLOCK != 0 {
            return;
        }
        if b.flags & fragment_flags::BACKGROUND_TO_CANVAS != 0 {
            // Only the border here; the background went to the canvas.
            self.sink.decorations(f, b, at, false, None);
            return;
        }
        let scroll = self.own_scroll(b);
        let pinned = self.pin_fixed_background(&b.style, false);
        self.sink.decorations(f, b, at, false, scroll);
        if pinned {
            self.sink.pop_layer();
        }
        // `background-clip: text` (CSS Backgrounds 4 §2.1): those layers show through the glyphs
        // of the box's text and its in-flow and floated descendants' (not through its stacking
        // contexts' or positioned boxes').
        if has_text_clip(&b.style)
            && !self.sink.in_text_clip()
            && self.sink.push_text_clip(f, b, at)
        {
            if b.flags & fragment_flags::INLINE_BOX != 0 {
                self.inline_content(f, at, None);
            } else {
                self.paint_flow(f, at, false);
            }
            self.sink.text_clip_layers(f, b, at, scroll);
        }
        match &b.content {
            BoxContent::CollapsedBorders(segments) => self.sink.collapsed_borders(segments, at),
            BoxContent::ColumnGaps(gaps) => self.sink.column_rules(f, b, gaps, at, scroll),
            _ => {}
        }
    }

    /// Phases 3–5 for the subtree of `f` (excluding stacking contexts and positioned descendants).
    fn paint_flow<S: Style>(&mut self, f: &Fragment<S>, at: Point, top: bool)
    where
        K: Sink<S>,
    {
        self.blocks(f, at, top);
        self.floats(f, at, top);
        self.inline_content(f, at, None);
    }

    /// Children of `f` that this pass descends into, with the clip of `f` applied.
    fn with_clip<S: Style>(&mut self, f: &Fragment<S>, at: Point, body: &mut dyn FnMut(&mut Self))
    where
        K: Sink<S>,
    {
        let clip = f.as_box().and_then(|b| clip_rect(f, b, at));
        if let (Some(c), Some(b)) = (clip, f.as_box()) {
            self.sink.push_clip(c, clip_radii(f, b));
        }
        body(self);
        if clip.is_some() {
            self.sink.pop_clip();
        }
    }

    /// Phase 3: block-level backgrounds and borders.
    fn blocks<S: Style>(&mut self, f: &Fragment<S>, at: Point, _top: bool)
    where
        K: Sink<S>,
    {
        self.with_clip(f, at, &mut |p| {
            for c in f.visible_children() {
                let cat = p.child_at(f, at, c);
                let cf = &*c.fragment;
                let Some(cb) = cf.as_box() else {
                    continue;
                };
                if !p.sink.wants(cf.paint_bounds.translate(cat.x, cat.y)) {
                    continue;
                }
                if skip_in_flow(cb)
                    || is_float(cb)
                    || is_atomic(cb)
                    || cb.flags & fragment_flags::INLINE_BOX != 0
                {
                    continue;
                }
                p.sink.enter(cb.node);
                p.decorations_of(cf, cb, cat);
                if let BoxContent::Replaced { .. } = cb.content {
                    // (block-level replaced content is painted in the inline phase)
                }
                p.blocks(cf, cat, false);
                p.sink.leave();
            }
        });
    }

    /// Phase 4: floats, each painted atomically.
    fn floats<S: Style>(&mut self, f: &Fragment<S>, at: Point, _top: bool)
    where
        K: Sink<S>,
    {
        // A clamped line-clamp container clips its floats at its block-end content edge as the
        // clamp point leaves it (CSS Overflow 4 §4.2). (The top is not the infinite rectangle's:
        // at that magnitude an f32 bottom edge is rounded to 64 px.)
        let clamp_clip = f.line_clamp_edge().map(|edge| {
            let inf = Rect::infinite();
            Rect::from_ltrb(inf.x, at.y - 1.0e5, inf.right(), at.y + edge)
        });
        if let Some(c) = clamp_clip {
            self.sink.push_clip(c, Radii::ZERO);
        }
        self.with_clip(f, at, &mut |p| {
            for c in f.visible_children() {
                let cat = p.child_at(f, at, c);
                let cf = &*c.fragment;
                if !p.sink.wants(cf.paint_bounds.translate(cat.x, cat.y)) {
                    continue;
                }
                match cf.as_box() {
                    Some(cb) if skip_in_flow(cb) => {}
                    Some(cb) if is_float(cb) => {
                        p.sink.enter(cb.node);
                        p.decorations_of(cf, cb, cat);
                        p.paint_flow(cf, cat, false);
                        p.sink.leave();
                    }
                    Some(cb) if is_atomic(cb) => {}
                    Some(cb) => {
                        p.sink.enter(cb.node);
                        p.floats(cf, cat, false);
                        p.sink.leave();
                    }
                    None => {
                        if !cf.visible_children().is_empty() {
                            p.floats(cf, cat, false);
                        }
                    }
                }
            }
        });
        if clamp_clip.is_some() {
            self.sink.pop_clip();
        }
    }

    /// Phase 5: line boxes (text, inline boxes, atomic inlines) and replaced content. `line` is
    /// the line box `f` is on when it is an inline box, in the coordinates of `at`.
    fn inline_content<S: Style>(&mut self, f: &Fragment<S>, at: Point, line: Option<Rect>)
    where
        K: Sink<S>,
    {
        if let Some(b) = f.as_box() {
            if let BoxContent::Replaced { .. } = b.content {
                if b.style.visibility() == Visibility::Visible {
                    self.sink.replaced(f, b, at);
                    // (an atomic inline's highlight is painted from its line, which it reaches)
                    if b.flags & fragment_flags::ATOMIC_INLINE == 0 {
                        self.sink.selected_replaced(f, b, at, None);
                    }
                }
            }
        }
        self.with_clip(f, at, &mut |p| {
            for c in f.visible_children() {
                let cat = p.child_at(f, at, c);
                let cf = &*c.fragment;
                if !p.sink.wants(cf.paint_bounds.translate(cat.x, cat.y)) {
                    continue;
                }
                match &cf.kind {
                    FragmentKind::Text(t) if t.flip.is_identity() => {
                        p.sink.text(cf.size, t, cat);
                    }
                    FragmentKind::Text(t) if t.combined.is_some() => {
                        // text-combine-upright: horizontal text in the middle of its square,
                        // compressed to fit (CSS Writing Modes 4 §9.1.2, §9.1.3).
                        let w = t.combined.unwrap_or(0.0);
                        let local = t.flip.inverse().size(cf.size);
                        let em = local.width;
                        let mut glyphs = t.clone();
                        if !t.decorations.is_empty() {
                            // Decorated as a single glyph: along the line, not the composition.
                            let mut line = t.clone();
                            line.clusters = t.clusters.start..t.clusters.start;
                            let m = t
                                .flip
                                .matrix(local)
                                .then(&Matrix::translation(cat.x, cat.y));
                            p.sink.push_transform(m);
                            p.sink.text(local, &line, Point::ZERO);
                            p.sink.pop_transform();
                            glyphs.decorations.clear();
                        }
                        let s = if w > em { em / w } else { 1.0 };
                        let m = Matrix::scale(s, 1.0).then(&Matrix::translation(
                            (cat.x + (cf.size.width - w * s) / 2.0).floor(),
                            (cat.y + (cf.size.height - local.height) / 2.0).floor(),
                        ));
                        p.sink.push_transform(m);
                        p.sink
                            .text(Size::new(w, local.height), &glyphs, Point::ZERO);
                        p.sink.pop_transform();
                    }
                    FragmentKind::Text(t) => {
                        // Vertical text (CSS Writing Modes 4): drawn in its own frame, turned.
                        let local = t.flip.inverse().size(cf.size);
                        let m = t
                            .flip
                            .matrix(local)
                            .then(&Matrix::translation(cat.x, cat.y));
                        p.sink.push_transform(m);
                        p.sink.text(local, t, Point::ZERO);
                        p.sink.pop_transform();
                    }
                    FragmentKind::Line { .. } => {
                        let line = Rect::new(cat.x, cat.y, cf.size.width, cf.size.height);
                        p.inline_content(cf, cat, Some(line))
                    }
                    FragmentKind::Box(cb) => {
                        if skip_in_flow(cb) || is_float(cb) {
                            continue;
                        }
                        p.sink.enter(cb.node);
                        if is_atomic(cb) {
                            // Painted as if it were a stacking context.
                            p.decorations_of(cf, cb, cat);
                            p.paint_flow(cf, cat, false);
                            if cb.flags & fragment_flags::ATOMIC_INLINE != 0
                                && matches!(cb.content, BoxContent::Replaced { .. })
                                && cb.style.visibility() == Visibility::Visible
                            {
                                p.sink.selected_replaced(cf, cb, cat, line);
                            }
                        } else if cb.flags & fragment_flags::INLINE_BOX != 0 {
                            p.decorations_of(cf, cb, cat);
                            p.inline_content(cf, cat, line);
                        } else {
                            p.inline_content(cf, cat, line);
                        }
                        p.sink.leave();
                    }
                }
            }
        });
    }

    /// The scroll bars of a scroll container (none for the viewport's, which are the embedder's).
    fn scrollbars_of<S: Style>(&mut self, f: &Fragment<S>, b: &BoxFragment<S>, at: Point)
    where
        K: Sink<S>,
    {
        let viewport = fragment_flags::ROOT
            | fragment_flags::VIEWPORT_OVERFLOW
            | fragment_flags::INITIAL_CONTAINING_BLOCK;
        if b.flags & fragment_flags::SCROLL_CONTAINER != 0 && b.flags & viewport == 0 {
            let scroll = match (self.scroll, b.node) {
                (Some(map), Some(n)) => map.get(&n).copied().unwrap_or(Point::ZERO),
                _ => Point::ZERO,
            };
            self.sink.scrollbars(f, b, at, scroll, self.device_scale);
        }
    }

    /// Outlines of the subtree (not of nested stacking contexts or positioned boxes).
    fn outlines<S: Style>(&mut self, f: &Fragment<S>, at: Point, top: bool)
    where
        K: Sink<S>,
    {
        if let Some(b) = f.as_box() {
            // (an inline box's outline goes around its fragments' rectangles, and an empty one,
            // with no width, has none: CSS UI 4 §5.1 leaves the shape to the UA, and Chrome
            // skips empty rectangles)
            let empty_inline = b.flags & fragment_flags::INLINE_BOX != 0
                && (f.size.width <= 0.0 || f.size.height <= 0.0);
            if b.style.outline_width() > 0.0
                && b.style.visibility() == Visibility::Visible
                && !empty_inline
            {
                self.sink.outline(f, b, at);
            }
            // (a stacking context's own scroll bars are its last paint; other scroll containers'
            // are members of their stacking context)
            if top && makes_stacking_context(b) {
                self.scrollbars_of(f, b, at);
            }
        }
        for c in f.visible_children() {
            let cat = self.child_at(f, at, c);
            let cf = &*c.fragment;
            if !self.sink.wants(cf.paint_bounds.translate(cat.x, cat.y)) {
                continue;
            }
            if let Some(cb) = cf.as_box() {
                if skip_in_flow(cb) {
                    continue;
                }
            }
            self.outlines(cf, cat, false);
        }
    }
}

/// Whether a clip in `clips` moves otherwise than content anchored by `anchor` when the viewport
/// scrolls (clips that move as it does don't keep such content from being a layer; the root's and
/// a propagating body's, `None`, don't move).
fn moving_clip(clips: &ClipStack, anchor: Anchor) -> bool {
    clips.iter().any(|c| c.1.is_some_and(|a| a != anchor))
}

fn scroll_clip(clips: &ClipStack) -> bool {
    clips
        .iter()
        .all(|c| matches!(c.1, None | Some(Anchor::Scroll)))
}

fn find_canvas<'f, S: Style>(
    f: &'f Fragment<S>,
    at: Point,
    out: &mut Option<(&'f Fragment<S>, Point)>,
) {
    if out.is_some() {
        return;
    }
    if let Some(b) = f.as_box() {
        if b.flags & fragment_flags::BACKGROUND_TO_CANVAS != 0 {
            *out = Some((f, at));
            return;
        }
    }
    for c in &f.children {
        find_canvas(
            &c.fragment,
            Point::new(at.x + c.offset.x, at.y + c.offset.y),
            out,
        );
    }
}

/// Whether a box's scroll bars are a member of its stacking context's lists (a scroll container
/// other than the viewport's that is not a stacking context itself).
fn has_own_scrollbars<S: Style>(b: &BoxFragment<S>) -> bool {
    let viewport = fragment_flags::ROOT
        | fragment_flags::VIEWPORT_OVERFLOW
        | fragment_flags::INITIAL_CONTAINING_BLOCK;
    b.flags & fragment_flags::SCROLL_CONTAINER != 0
        && b.flags & viewport == 0
        && !makes_stacking_context(b)
}

/// Stacking contexts and positioned boxes are painted from their stacking context's lists.
fn skip_in_flow<S: Style>(b: &BoxFragment<S>) -> bool {
    makes_stacking_context(b) || is_positioned(b)
}

/// The rectangle a box clips its content to (its padding box), in absolute coordinates.
fn clip_rect<S: Style>(f: &Fragment<S>, b: &BoxFragment<S>, at: Point) -> Option<Rect> {
    let cx = b.flags & fragment_flags::CLIP_X != 0;
    let cy = b.flags & fragment_flags::CLIP_Y != 0;
    // (the root's and a propagating body's `overflow` applies to the viewport, not to their own
    // boxes: CSS Overflow 3 §3.3)
    let viewport = fragment_flags::ROOT | fragment_flags::VIEWPORT_OVERFLOW;
    if !cx && !cy || b.flags & viewport != 0 {
        return None;
    }
    // (`overflow: clip` clips at its overflow clip edge, CSS Overflow 3 §3.2)
    let p = f.overflow_clip_rect().translate(at.x, at.y);
    let inf = Rect::infinite();
    Some(Rect::new(
        if cx { p.x } else { inf.x },
        if cy { p.y } else { inf.y },
        if cx { p.width } else { inf.width },
        if cy { p.height } else { inf.height },
    ))
}

/// A rectangle with its corners rounded by `radii` (already fitted to it) as a path, each corner
/// a quarter ellipse as one cubic curve, clockwise from the top left.
fn rounded_rect_path(rect: Rect, radii: &Radii) -> Vec<PathEl> {
    // (the control point distance that fits a cubic to a quarter circle)
    const K: f32 = 0.552_284_8;
    let (l, t, r, b) = (rect.x, rect.y, rect.right(), rect.bottom());
    let (tl, tr, br, bl) = (
        radii.top_left,
        radii.top_right,
        radii.bottom_right,
        radii.bottom_left,
    );
    let mut path = Vec::with_capacity(10);
    path.push(PathEl::MoveTo(Point::new(l + tl.width, t)));
    path.push(PathEl::LineTo(Point::new(r - tr.width, t)));
    if tr.width > 0.0 && tr.height > 0.0 {
        path.push(PathEl::CurveTo(
            Point::new(r - tr.width + K * tr.width, t),
            Point::new(r, t + tr.height - K * tr.height),
            Point::new(r, t + tr.height),
        ));
    }
    path.push(PathEl::LineTo(Point::new(r, b - br.height)));
    if br.width > 0.0 && br.height > 0.0 {
        path.push(PathEl::CurveTo(
            Point::new(r, b - br.height + K * br.height),
            Point::new(r - br.width + K * br.width, b),
            Point::new(r - br.width, b),
        ));
    }
    path.push(PathEl::LineTo(Point::new(l + bl.width, b)));
    if bl.width > 0.0 && bl.height > 0.0 {
        path.push(PathEl::CurveTo(
            Point::new(l + bl.width - K * bl.width, b),
            Point::new(l, b - bl.height + K * bl.height),
            Point::new(l, b - bl.height),
        ));
    }
    path.push(PathEl::LineTo(Point::new(l, t + tl.height)));
    if tl.width > 0.0 && tl.height > 0.0 {
        path.push(PathEl::CurveTo(
            Point::new(l, t + tl.height - K * tl.height),
            Point::new(l + tl.width - K * tl.width, t),
            Point::new(l + tl.width, t),
        ));
    }
    path.push(PathEl::Close);
    path
}

/// The rectangle a polygon is, when it is four corners of one with axis-aligned sides (in either
/// direction around).
fn axis_aligned_rect(points: &[Point]) -> Option<Rect> {
    let [a, b, c, d] = points else {
        return None;
    };
    let horizontal_first = a.y == b.y && b.x == c.x && c.y == d.y && d.x == a.x;
    let vertical_first = a.x == b.x && b.y == c.y && c.x == d.x && d.y == a.y;
    if !horizontal_first && !vertical_first {
        return None;
    }
    let (x0, x1) = (a.x.min(c.x), a.x.max(c.x));
    let (y0, y1) = (a.y.min(c.y), a.y.max(c.y));
    Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// Whether `p` is inside `rect` with its corners rounded by `radii` (elliptical arcs).
pub(crate) fn rounded_rect_contains(rect: Rect, radii: &Radii, p: Point) -> bool {
    if !rect.contains(p) {
        return false;
    }
    let corner = |r: Size, cx: f32, cy: f32| {
        if r.width <= 0.0 || r.height <= 0.0 {
            return true;
        }
        let dx = (p.x - cx) / r.width;
        let dy = (p.y - cy) / r.height;
        dx * dx + dy * dy <= 1.0
    };
    let (l, t, rr, b) = (rect.x, rect.y, rect.right(), rect.bottom());
    let tl = radii.top_left;
    let tr = radii.top_right;
    let br = radii.bottom_right;
    let bl = radii.bottom_left;
    if p.x < l + tl.width && p.y < t + tl.height && !corner(tl, l + tl.width, t + tl.height) {
        return false;
    }
    if p.x > rr - tr.width && p.y < t + tr.height && !corner(tr, rr - tr.width, t + tr.height) {
        return false;
    }
    if p.x > rr - br.width && p.y > b - br.height && !corner(br, rr - br.width, b - br.height) {
        return false;
    }
    if p.x < l + bl.width && p.y > b - bl.height && !corner(bl, l + bl.width, b - bl.height) {
        return false;
    }
    true
}

/// The corner radii of a box's overflow clip (CSS Backgrounds 3 §5.3: the padding box curve, the
/// border radii less the border widths) when it clips on both axes; zero otherwise.
fn clip_radii<S: Style>(f: &Fragment<S>, b: &BoxFragment<S>) -> Radii {
    let both = fragment_flags::CLIP_X | fragment_flags::CLIP_Y;
    if b.flags & both != both {
        return Radii::ZERO;
    }
    let outer = radii(&b.style, f.size);
    if f.overflow_clip_edge().is_some() {
        // CSS Overflow 3 §3.2: the overflow clip edge is the reference box's curve, its non-zero
        // radii grown by the margin.
        let m = b.style.overflow_clip_margin();
        let base = match m.reference {
            BoxArea::BorderBox => outer,
            BoxArea::PaddingBox => outer.inner(&b.border),
            BoxArea::ContentBox => outer.inner(&inset_sides(&b.border, &b.padding)),
        };
        let grow = |c: Size| {
            if c.width > 0.0 && c.height > 0.0 {
                Size::new(c.width + m.margin, c.height + m.margin)
            } else {
                c
            }
        };
        return Radii {
            top_left: grow(base.top_left),
            top_right: grow(base.top_right),
            bottom_right: grow(base.bottom_right),
            bottom_left: grow(base.bottom_left),
        };
    }
    outer.inner(&b.border)
}

// ----- the display list sink -----

/// Writes display items.
pub(crate) struct DisplayListSink<'a> {
    pub out: &'a mut DisplayListBuilder,
    pub text: &'a TextEngine,
    /// The canvas size (backgrounds of the root fill it).
    pub canvas: Rect,
    /// The viewport, as painted (`background-attachment: fixed` positions in it).
    pub viewport: Rect,
    /// Nested documents' lists by their replaced element ([`crate::PaintOptions::embeds`]).
    pub embeds: Option<&'a std::collections::HashMap<NodeId, snowlark_paint::DisplayList>>,
    /// The document's selection to highlight ([`crate::PaintOptions::selection`]).
    pub selection: Option<&'a TextSelection>,
    /// The find-in-page matches to highlight ([`crate::PaintOptions::find`]).
    pub find: Option<&'a crate::find::FindHighlights>,
    /// Only what paints in this rectangle (of the root's coordinates) is wanted.
    pub cull: Option<Rect>,
    /// Device pixels per CSS px.
    pub device_scale: f32,
    /// Transforms open around what is being painted (culling compares untransformed bounds).
    transforms: u32,
    /// Painting the text of a `background-clip: text` mask: glyphs and their decorations only,
    /// opaque, no shadows, highlights or boxes.
    text_clip: bool,
    glyphs: Vec<u16>,
    advances: Vec<f32>,
    offsets: Vec<snowlark_paint::GlyphOffset>,
}

impl<'a> DisplayListSink<'a> {
    pub fn new(out: &'a mut DisplayListBuilder, text: &'a TextEngine, canvas: Rect) -> Self {
        DisplayListSink {
            out,
            text,
            canvas,
            viewport: canvas,
            embeds: None,
            selection: None,
            find: None,
            cull: None,
            device_scale: 1.0,
            transforms: 0,
            text_clip: false,
            glyphs: Vec::new(),
            advances: Vec::new(),
            offsets: Vec::new(),
        }
    }

    /// The geometry the background layers of box `b` (fragment `f` at `at`, scrolled by
    /// `scroll`) are placed in and painted to.
    fn layer_area<S: Style>(
        &self,
        f: &Fragment<S>,
        b: &BoxFragment<S>,
        border_box: Rect,
        radii: Radii,
        at: Point,
        scroll: Option<Point>,
    ) -> LayerArea {
        let padding_box = border_box.deflate(&b.border);
        let content_box = padding_box.deflate(&b.padding);
        LayerArea {
            boxes: [border_box, padding_box, content_box],
            radii: [
                radii,
                radii.inner(&b.border),
                radii.inner(&inset_sides(&b.border, &b.padding)),
            ],
            // `fixed` backgrounds are positioned in the viewport, except under a transform,
            // where they are treated as `scroll` (CSS Transforms 1 §6).
            viewport: (self.transforms == 0).then_some(self.viewport),
            local: scroll.map(|sc| scrolled_area(f, b, at, sc)),
            clip_override: None,
            strip: (b.flags & fragment_flags::INLINE_BOX != 0)
                .then(|| b.strip.get())
                .flatten()
                .map(|s| s.map(|r| r.translate(at.x, at.y))),
        }
    }
}

/// Used border radii of a box (CSS Backgrounds 3 §5.5: scaled down when they don't fit).
pub(crate) fn radii<S: Style>(s: &S, size: Size) -> Radii {
    let r = |c| {
        let (h, v) = s.border_radius(c);
        snowlark_paint::Size::new(
            h.resolve(size.width).max(0.0),
            v.resolve(size.height).max(0.0),
        )
    };
    let radii = Radii {
        top_left: r(Corner::TopLeft),
        top_right: r(Corner::TopRight),
        bottom_right: r(Corner::BottomRight),
        bottom_left: r(Corner::BottomLeft),
    };
    if radii.is_zero() {
        return radii;
    }
    radii.fitted(&Rect::new(0.0, 0.0, size.width, size.height))
}

/// HTML §15.3.13: a fieldset's border box for painting starts at its block-start border, which
/// is centered on the rendered legend; the border is not painted behind the legend (the returned
/// gap, in the same coordinates).
fn fieldset_border<S: Style>(
    f: &Fragment<S>,
    b: &BoxFragment<S>,
    border_box: Rect,
) -> (Rect, Option<Rect>) {
    if b.flags & fragment_flags::FIELDSET == 0 {
        return (border_box, None);
    }
    let Some(legend) = f.children.iter().find(|c| {
        c.fragment
            .as_box()
            .is_some_and(|l| l.flags & fragment_flags::RENDERED_LEGEND != 0)
    }) else {
        return (border_box, None);
    };
    let center = legend.offset.y + legend.fragment.size.height / 2.0;
    let y0 = (center - b.border.top / 2.0).max(0.0);
    let rect = Rect::new(
        border_box.x,
        border_box.y + y0,
        border_box.width,
        (border_box.height - y0).max(0.0),
    );
    let gap = Rect::new(
        border_box.x + legend.offset.x,
        border_box.y,
        legend.fragment.size.width,
        y0 + b.border.top,
    );
    (rect, Some(gap))
}

impl<S: Style> Sink<S> for DisplayListSink<'_> {
    fn decorations(
        &mut self,
        f: &Fragment<S>,
        b: &BoxFragment<S>,
        at: Point,
        canvas: bool,
        scroll: Option<Point>,
    ) {
        let s = &b.style;
        if self.text_clip
            || s.visibility() != Visibility::Visible
            || b.flags & fragment_flags::HIDE_EMPTY != 0
        {
            return;
        }
        let border_box = Rect::new(at.x, at.y, f.size.width, f.size.height);
        let (border_box, legend_gap) = fieldset_border(f, b, border_box);
        let radii = radii(s, Size::new(border_box.width, border_box.height));
        // (`f` is the root's box when painting the canvas, `b` the box the background came from)
        let positioned = f.as_box().unwrap_or(b);
        // `fixed` backgrounds are positioned in the viewport, except under a transform, where they
        // are treated as `scroll` (CSS Transforms 1 §6).
        let viewport = (self.transforms == 0).then_some(self.viewport);
        if canvas {
            // The canvas: the whole area gets the root's (or body's) background; images are sized
            // and positioned against the element's box (CSS Backgrounds 3 §2.11.1).
            let area = self.canvas.union(&border_box);
            self.out.fill_rect(area, s.background_color());
            let padding_box = border_box.deflate(&positioned.border);
            let content_box = padding_box.deflate(&positioned.padding);
            let geo = LayerArea {
                boxes: [border_box, padding_box, content_box],
                radii: [Radii::ZERO; 3],
                viewport,
                local: None,
                clip_override: Some(area),
                strip: None,
            };
            paint_background_layers(self.out, s, &geo, None);
            return;
        }
        let to_canvas = b.flags & fragment_flags::BACKGROUND_TO_CANVAS != 0;
        if !to_canvas {
            // outer shadows
            for i in (0..s.box_shadow_count()).rev() {
                let sh = s.box_shadow(i);
                if !sh.inset {
                    self.out.box_shadow(BoxShadow {
                        rect: border_box,
                        radii,
                        offset: Point::new(sh.offset_x, sh.offset_y),
                        blur: sh.blur,
                        spread: sh.spread,
                        color: sh.color,
                        inset: false,
                    });
                }
            }
            if b.flags & fragment_flags::THEMED != 0 {
                // The widget's own frame in place of the CSS background and border: its background
                // color over the border box with 2px corners, under a 1px border of the border
                // color (the UA sheet's, as the page has set neither).
                let radii = Radii::uniform(THEMED_RADIUS).fitted(&border_box);
                self.out
                    .fill_rounded_rect(border_box, radii, s.background_color());
                let color = s.border_color(Side::Top);
                self.out.border(Border {
                    rect: border_box,
                    radii,
                    widths: Sides::all(1.0),
                    colors: [color; 4],
                    styles: [BorderStyle::Solid; 4],
                });
                return;
            }
            let padding_box = border_box.deflate(&b.border);
            let geo = self.layer_area(f, b, border_box, radii, at, scroll);
            // The color is painted under the bottom layer, clipped as it is (§3.10); a layer
            // clipped to the text, and the color under one, are painted with the text as their
            // mask (`text_clip_layers`).
            let bottom = s.background_color_layer();
            if !bottom.clip_text {
                let (_, color_rect, color_radii) = geo.layer_areas(&bottom);
                self.out
                    .fill_rounded_rect(color_rect, color_radii, s.background_color());
            }
            paint_background_layers(self.out, s, &geo, Some(false));
            for i in (0..s.box_shadow_count()).rev() {
                let sh = s.box_shadow(i);
                if sh.inset {
                    self.out.box_shadow(BoxShadow {
                        rect: padding_box,
                        radii: radii.inner(&b.border),
                        offset: Point::new(sh.offset_x, sh.offset_y),
                        blur: sh.blur,
                        spread: sh.spread,
                        color: sh.color,
                        inset: true,
                    });
                }
            }
        }
        if let Some(bi) = s.border_image() {
            if b.flags & fragment_flags::NO_BORDER_PAINT == 0 {
                paint_border_image(
                    self.out,
                    &bi,
                    border_box,
                    &b.border,
                    s.image_rendering(),
                    self.device_scale,
                );
                return;
            }
        }
        if b.flags & fragment_flags::NO_BORDER_PAINT == 0 && !b.border.is_zero() {
            let st = |side| s.border_style(side);
            let border = Border {
                rect: border_box,
                radii,
                widths: b.border,
                colors: [
                    s.border_color(Side::Top),
                    s.border_color(Side::Right),
                    s.border_color(Side::Bottom),
                    s.border_color(Side::Left),
                ],
                styles: [
                    st(Side::Top),
                    st(Side::Right),
                    st(Side::Bottom),
                    st(Side::Left),
                ],
            };
            match legend_gap {
                // Not behind the rendered legend: the border on either side of it and below it.
                Some(gap) => {
                    let full = border_box.inflate(1.0, 1.0);
                    for clip in [
                        Rect::new(full.x, full.y, gap.x - full.x, full.height),
                        Rect::new(gap.right(), full.y, full.right() - gap.right(), full.height),
                        Rect::new(gap.x, gap.bottom(), gap.width, full.bottom() - gap.bottom()),
                    ] {
                        if clip.width > 0.0 && clip.height > 0.0 {
                            self.out.push_clip_rect(clip);
                            self.out.border(border);
                            self.out.pop_clip();
                        }
                    }
                }
                None => self.out.border(border),
            }
        }
    }

    fn text(&mut self, size: Size, t: &TextFragment<S>, at: Point) {
        let s = &t.style;
        if s.visibility() != Visibility::Visible {
            return;
        }
        let shaped = &t.shaped;
        let Some(run) = shaped.runs.get(t.run as usize) else {
            return;
        };
        let (c0, c1) = (t.clusters.start as usize, t.clusters.end as usize);
        // (no clusters: a composition's decorations, drawn along the line)
        if (c0 >= c1 && t.decorations.is_empty()) || c1 > shaped.clusters.len() {
            return;
        }
        let g0 = shaped.clusters[c0..c1]
            .iter()
            .map(|c| c.glyphs.0)
            .min()
            .unwrap_or(0) as usize;
        let g1 = shaped.clusters[c0..c1]
            .iter()
            .map(|c| c.glyphs.1)
            .max()
            .unwrap_or(0) as usize;
        self.glyphs.clear();
        self.advances.clear();
        self.offsets.clear();
        self.glyphs.extend_from_slice(&shaped.glyphs[g0..g1]);
        self.advances.extend_from_slice(&shaped.advances[g0..g1]);
        self.offsets.extend_from_slice(&shaped.offsets[g0..g1]);
        let spacing = s.letter_spacing();
        let word = s.word_spacing();
        if t.combined.is_none() && (spacing != 0.0 || word != 0.0 || t.justify != 0.0) {
            for c in &shaped.clusters[c0..c1] {
                if c.glyphs.1 <= c.glyphs.0 {
                    continue;
                }
                let last = c.glyphs.1 as usize - 1 - g0;
                let mut extra = spacing;
                if c.whitespace {
                    extra += word + t.justify;
                }
                self.advances[last] += extra;
            }
        }
        let mut origin = Point::new(at.x, at.y + t.baseline);
        if t.central && !run.upright {
            // (a rotated run's own font centered on the line, as an upright glyph's is; its
            // alphabetic baseline is below the central one)
            let m = crate::inline::line_metrics(self.text.metrics(run.font, run.size));
            origin.y += m.ascent - crate::inline::centered(m).ascent;
        }
        // (glyphs are filled with `-webkit-text-fill-color`; decorations keep their own colour)
        let color = s.text_fill_color();
        // Decorations (CSS Text Decoration 3 §2.3): underlines and overlines paint below the text,
        // line-throughs above it.
        let metrics = if t.decorations.is_empty() {
            None
        } else {
            Some(self.text.metrics(run.font, run.size))
        };
        let width = size.width;
        // (`lines`: underlines and overlines, line-throughs or both; `shift` and `paint` move and
        // recolor them for a shadow)
        let decorate = |out: &mut DisplayListBuilder,
                        lines: Lines,
                        shift: Point,
                        paint: Option<Color>| {
            let Some(m) = metrics else {
                return;
            };
            let thickness = m.underline_thickness.max(1.0);
            let (x, base) = (at.x + shift.x, origin.y + shift.y);
            for d in &t.decorations {
                let color = paint.unwrap_or(d.color);
                let mut line = |y: f32, th: f32| {
                    out.fill_rect(Rect::new(x, y, width, th), color);
                    if d.style == TextDecorationStyle::Double {
                        out.fill_rect(Rect::new(x, y + th * 2.0, width, th), color);
                    }
                };
                if lines != Lines::Through {
                    if d.line.contains(TextDecorationLine::UNDERLINE) {
                        line(base + m.underline_offset, thickness);
                    }
                    if d.line.contains(TextDecorationLine::OVERLINE) {
                        line(base - m.ascent, thickness);
                    }
                }
                if lines != Lines::UnderOver && d.line.contains(TextDecorationLine::LINE_THROUGH) {
                    line(base + m.strikeout_offset, m.strikeout_thickness.max(1.0));
                }
            }
        };
        let glyphs = (&self.glyphs, &self.advances, &self.offsets);
        let draw = |out: &mut DisplayListBuilder, origin: Point, color: Color| {
            out.text(
                glyphs.0,
                glyphs.1,
                glyphs.2,
                origin,
                run.font,
                run.size,
                color,
                run.bidi_level,
            );
            if run.upright && c0 < c1 {
                out.last_text_sideways();
            }
        };
        // Text shadows (CSS Text Decoration 3 §4): below the text and its decorations, each the
        // text and decorations in the shadow's color, moved by its offset and blurred with a
        // standard deviation of half its blur radius; the first shadow on top.
        let shadow = |out: &mut DisplayListBuilder, sh: &Shadow| {
            if sh.color.is_transparent() {
                return;
            }
            let shift = Point::new(sh.offset_x, sh.offset_y);
            let sigma = sh.blur.max(0.0) / 2.0;
            if sigma > 0.0 {
                // (the glyphs' reach as the partly selected clips below take it, then the blur's)
                let content = Rect::new(
                    at.x + shift.x - 2.0 * run.size,
                    origin.y + shift.y - 4.0 * run.size,
                    width.abs() + 4.0 * run.size,
                    8.0 * run.size,
                );
                let blur = [FilterOp::Blur(sigma)];
                out.push_filter(&blur, snowlark_paint::filter::map_rect(&blur, content));
            }
            decorate(out, Lines::UnderOver, shift, Some(sh.color));
            if c0 < c1 {
                draw(
                    out,
                    Point::new(origin.x + shift.x, origin.y + shift.y),
                    sh.color,
                );
            }
            decorate(out, Lines::Through, shift, Some(sh.color));
            if sigma > 0.0 {
                out.pop_filter();
            }
        };
        if self.text_clip {
            // A `background-clip: text` mask: only the coverage counts, so glyphs and
            // decorations in one opaque colour, without shadows or highlights.
            let opaque = Some(Color::BLACK);
            decorate(self.out, Lines::UnderOver, Point::ZERO, opaque);
            if c0 < c1 {
                draw(self.out, origin, Color::BLACK);
            }
            decorate(self.out, Lines::Through, Point::ZERO, opaque);
            return;
        }
        for i in (0..s.text_shadow_count()).rev() {
            shadow(self.out, &s.text_shadow(i));
        }
        // Find in page: the matches behind the text (the selection, below, over them).
        if let Some((find, n)) = self.find.zip(t.node) {
            if let Some(ranges) = find.ranges.get(&n) {
                let m = self.text.metrics(run.font, run.size);
                for &(a, b, id) in ranges {
                    let (lo, hi) = (a.max(t.text_range.start), b.min(t.text_range.end));
                    if lo >= hi {
                        continue;
                    }
                    let x0 = t.x_of(t.processed_offset(lo));
                    let x1 = t.x_of(t.processed_offset(hi));
                    if x1 > x0 {
                        let color = if find.current == Some(id) {
                            crate::find::ACTIVE_COLOR
                        } else {
                            crate::find::MATCH_COLOR
                        };
                        self.out.fill_rect(
                            Rect::new(
                                at.x + x0,
                                origin.y - m.ascent,
                                x1 - x0,
                                m.ascent + m.descent,
                            ),
                            color,
                        );
                    }
                }
            }
        }
        // The selection (CSS Pseudo-Elements 4 §3.6): the highlight's background behind the selected
        // part, its `text-shadow`s over that, and the part's text and the originating element's
        // decorations in the highlight's `color` (§3.6.3; the text's own color where it has none).
        let mut part: Option<SelectedPart> = None;
        let selected = self
            .selection
            .zip(t.node)
            .and_then(|(sel, n)| sel.ranges.get(&n).map(|r| (sel, *r)));
        if let Some((sel, r)) = selected {
            let (lo, hi) = (r.start.max(t.text_range.start), r.end.min(t.text_range.end));
            if lo < hi {
                let xa = t.x_of(t.processed_offset(lo));
                let xb = t.x_of(t.processed_offset(hi));
                let (x0, x1) = (xa.min(xb), xa.max(xb));
                if x1 > x0 {
                    let (background, highlight_color) =
                        r.highlight.unwrap_or((sel.background, sel.color));
                    if !background.is_transparent() {
                        // The highlighted area (§3.4): the text's own height united with its line
                        // box's, and, where the selection goes on past the end of the line, a
                        // space's width more at that end.
                        let m = self.text.metrics(run.font, run.size);
                        let top = (origin.y - m.ascent).min(at.y + t.line.0);
                        let bottom = (origin.y + m.descent).max(at.y + t.line.1);
                        let (mut left, mut right) = (x0, x1);
                        let past_end = hi == t.text_range.end && (r.end > hi || r.continues);
                        if t.line_end && past_end {
                            let space = self.text.space_advance(run.font, run.size);
                            if run.bidi_level % 2 == 1 {
                                left -= space;
                            } else {
                                right += space;
                            }
                        }
                        self.out.fill_rect(
                            Rect::new(at.x + left, top, right - left, bottom - top),
                            background,
                        );
                    }
                    let shadows = (r.shadows as usize)
                        .checked_sub(1)
                        .and_then(|i| sel.shadows.get(i))
                        .map_or(&[][..], |v| &v[..]);
                    part = Some((
                        (lo > t.text_range.start).then_some(x0),
                        (hi < t.text_range.end).then_some(x1),
                        highlight_color.unwrap_or(color),
                        shadows,
                    ));
                }
            }
        }
        // Partly selected, each pass is painted once per part, clipped to it, so that a glyph the
        // boundary crosses (a ligature, an overhang) and a decoration change color at the boundary.
        // (the clips are open where the fragment ends, and taller than any glyph; `Some` is the
        // selected part's color)
        let reach = size.width.abs() + 4.0 * run.size;
        let (clip_top, clip_height) = (origin.y - 4.0 * run.size, 8.0 * run.size);
        let (start, end) = (at.x - reach, at.x + reach);
        let pieces = part.map(|(left, right, c, _)| {
            let (x0, x1) = (
                left.map_or(start, |x| at.x + x),
                right.map_or(end, |x| at.x + x),
            );
            [
                left.map(|_| (start, x0, None)),
                Some((x0, x1, Some(c))),
                right.map(|_| (x1, end, None)),
            ]
        });
        let in_pieces =
            |out: &mut DisplayListBuilder, f: &dyn Fn(&mut DisplayListBuilder, Option<Color>)| {
                match &pieces {
                    None => f(out, None),
                    Some([None, Some((_, _, c)), None]) => f(out, *c),
                    Some(list) => {
                        for &(a, b, c) in list.iter().flatten() {
                            out.push_clip_rect(Rect::new(a, clip_top, b - a, clip_height));
                            f(out, c);
                            out.pop_clip();
                        }
                    }
                }
            };
        if let Some((left, right, _, shadows)) = part.filter(|p| !p.3.is_empty()) {
            let x0 = left.map_or(start, |x| at.x + x);
            let x1 = right.map_or(end, |x| at.x + x);
            self.out
                .push_clip_rect(Rect::new(x0, clip_top, x1 - x0, clip_height));
            for sh in shadows.iter().rev() {
                shadow(self.out, sh);
            }
            self.out.pop_clip();
        }
        let zero = Point::new(0.0, 0.0);
        in_pieces(self.out, &|out, c| decorate(out, Lines::UnderOver, zero, c));
        match part {
            Some((_, _, c, _)) if c != color => {
                in_pieces(self.out, &|out, c| draw(out, origin, c.unwrap_or(color)))
            }
            _ => draw(self.out, origin, color),
        }
        in_pieces(self.out, &|out, c| decorate(out, Lines::Through, zero, c));
        // The caret of an editing host.
        if let (Some(sel), Some(n)) = (self.selection, t.node) {
            if let Some((cn, at_byte)) = sel.caret {
                let last = t.text_range.end == at_byte
                    && t.clusters.end as usize == t.shaped.clusters.len();
                if cn == n
                    && at_byte >= t.text_range.start
                    && (at_byte < t.text_range.end || last || t.text_range.is_empty())
                {
                    let x = t.x_of(t.processed_offset(at_byte));
                    let m = self.text.metrics(run.font, run.size);
                    self.out.fill_rect(
                        Rect::new(at.x + x, origin.y - m.ascent, 1.0, m.ascent + m.descent),
                        sel.caret_color.unwrap_or(color),
                    );
                }
            }
        }
    }

    fn selected_replaced(
        &mut self,
        f: &Fragment<S>,
        b: &BoxFragment<S>,
        at: Point,
        line: Option<Rect>,
    ) {
        if self.text_clip {
            return;
        }
        let Some(&color) = self
            .selection
            .zip(b.node)
            .and_then(|(sel, n)| sel.replaced.get(&n))
        else {
            return;
        };
        // Its border box, as tall as its line box across the line (§3.4).
        let (mut lo, mut hi) = ((at.x, at.y), (at.x + f.size.width, at.y + f.size.height));
        if let Some(l) = line {
            if crate::logical::is_horizontal(b.style.writing_mode()) {
                (lo.1, hi.1) = (lo.1.min(l.y), hi.1.max(l.y + l.height));
            } else {
                (lo.0, hi.0) = (lo.0.min(l.x), hi.0.max(l.x + l.width));
            }
        }
        self.out
            .fill_rect(Rect::new(lo.0, lo.1, hi.0 - lo.0, hi.1 - lo.1), color);
    }

    fn replaced(&mut self, f: &Fragment<S>, b: &BoxFragment<S>, at: Point) {
        if self.text_clip {
            return;
        }
        if let BoxContent::Replaced {
            content,
            rect,
            clip,
            control,
            control_flip,
        } = &b.content
        {
            if let Some(c) = &content.control {
                if b.style.visibility() == Visibility::Visible {
                    if control_flip.is_identity() {
                        crate::controls::paint(
                            self.out,
                            &b.style,
                            c,
                            control.as_deref(),
                            clip.translate(at.x, at.y),
                            at,
                        );
                    } else {
                        // A vertical control: the horizontal one turned a quarter turn (mirrored
                        // frames are the same turn, as for text).
                        let flip = if control_flip.mirrors() {
                            crate::logical::Flip::MIRROR_Y.then(*control_flip)
                        } else {
                            *control_flip
                        };
                        let local = flip.inverse().size(f.size);
                        let m = flip.matrix(local).then(&Matrix::translation(at.x, at.y));
                        self.out.push_transform(m);
                        crate::controls::paint(
                            self.out,
                            &b.style,
                            c,
                            control.as_deref(),
                            flip.inverse().rect(*clip, f.size),
                            Point::ZERO,
                        );
                        self.out.pop_transform();
                    }
                    // (a textarea is not a scroll container in the fragment tree: its resizer
                    // is painted with it)
                    if crate::controls::resizer_axes(f).is_some()
                        && b.flags & fragment_flags::SCROLL_CONTAINER == 0
                    {
                        crate::controls::paint_resizer(
                            self.out,
                            crate::controls::resizer_rect(f).translate(at.x, at.y),
                            b.style.color_scheme(),
                        );
                    }
                }
                return;
            }
            if let Some(list) = b.node.and_then(|n| self.embeds?.get(&n)) {
                // A nested document: its viewport is the content box.
                if b.style.visibility() == Visibility::Visible {
                    let clip = clip.translate(at.x, at.y);
                    // (the nested viewport's origin on a device pixel, as the content box's
                    // painted edge is: its own boxes snap to device pixels too)
                    let d = self.device_scale;
                    let origin = Point::new((clip.x * d).round() / d, (clip.y * d).round() / d);
                    self.out.push_clip_rect(clip);
                    self.out
                        .push_transform(Matrix::translation(origin.x, origin.y));
                    self.out.append(list);
                    self.out.pop_transform();
                    self.out.pop_clip();
                }
                return;
            }
            if content.image.is_some() || content.generated.is_some() {
                // An image is clipped by its `overflow` (CSS Overflow 3 §3: the UA style sheet's
                // `overflow: clip; overflow-clip-margin: content-box` clips it to its content
                // box), and paints outside its box where that is `visible`.
                let overflow_clip = clip_rect(f, b, at);
                let radii = clip_radii(f, b);
                match overflow_clip {
                    Some(c) if !radii.is_zero() => self.out.push_clip_rounded_rect(c, radii),
                    Some(c) => self.out.push_clip_rect(c),
                    None => {}
                }
                let dest = rect.translate(at.x, at.y);
                if let Some(image) = content.image {
                    self.out
                        .image(image, dest, None, 1.0, b.style.image_rendering());
                }
                if let Some(image) = &content.generated {
                    draw_image(self.out, image, dest, b.style.image_rendering());
                }
                if overflow_clip.is_some() {
                    self.out.pop_clip();
                }
            }
            if let Some(media) = &content.media {
                crate::media::paint(
                    self.out,
                    self.text,
                    media,
                    rect.translate(at.x, at.y),
                    clip.translate(at.x, at.y),
                );
            }
        }
    }

    fn collapsed_borders(&mut self, segments: &[CollapsedBorderSegment], at: Point) {
        if self.text_clip {
            return;
        }
        for s in segments {
            if !s.style.is_invisible() {
                self.out.fill_rect(s.rect.translate(at.x, at.y), s.color);
            }
        }
    }

    fn column_rules(
        &mut self,
        f: &Fragment<S>,
        b: &BoxFragment<S>,
        gaps: &[ColumnGap],
        at: Point,
        scroll: Option<Point>,
    ) {
        if self.text_clip {
            return;
        }
        let s = &b.style;
        let width = s.column_rule_width();
        // (CSS Multi-column Layout 1 §4.2: the styles are those of the collapsing border model,
        // where `inset` is `ridge` and `outset` is `groove`, CSS Backgrounds 3 §4.2)
        let style = match s.column_rule_style() {
            BorderStyle::Inset => BorderStyle::Ridge,
            BorderStyle::Outset => BorderStyle::Groove,
            other => other,
        };
        let color = s.column_rule_color();
        if s.visibility() != Visibility::Visible
            || width <= 0.0
            || style.is_invisible()
            || color.is_transparent()
        {
            return;
        }
        // (a scroll container's rules move with its columns, inside its padding box)
        let origin = match scroll {
            Some(sc) => {
                let padding_box =
                    Rect::new(at.x, at.y, f.size.width, f.size.height).deflate(&b.border);
                self.out.push_clip_rect(padding_box);
                Point::new(at.x - sc.x, at.y - sc.y)
            }
            None => at,
        };
        // Each rule is centred in its gap, snapped to device pixels, and drawn as a border on the
        // columns' inline-start side (§4: rules are drawn in the middle of the gap, and may be
        // wider than it).
        let d = self.device_scale;
        let snap = |v: f32| (v * d).round() / d;
        for g in gaps {
            let r = g.rect.translate(origin.x, origin.y);
            let across = g.side.left > 0.0 || g.side.right > 0.0;
            let rect = if across {
                let x = r.x + (r.width - width) / 2.0;
                let (x0, x1) = (snap(x), snap(x + width));
                Rect::from_ltrb(x0, snap(r.y), x1, snap(r.bottom()))
            } else {
                let y = r.y + (r.height - width) / 2.0;
                let (y0, y1) = (snap(y), snap(y + width));
                Rect::from_ltrb(snap(r.x), y0, snap(r.right()), y1)
            };
            let w = if across { rect.width } else { rect.height };
            let widths = Sides::new(
                g.side.top * w,
                g.side.right * w,
                g.side.bottom * w,
                g.side.left * w,
            );
            self.out.border(Border {
                rect,
                radii: Radii::ZERO,
                widths,
                colors: [color; 4],
                styles: [style; 4],
            });
        }
        if scroll.is_some() {
            self.out.pop_clip();
        }
    }

    fn outline(&mut self, f: &Fragment<S>, b: &BoxFragment<S>, at: Point) {
        if self.text_clip {
            return;
        }
        let s = &b.style;
        let w = s.outline_width();
        let o = s.outline_offset();
        let rect = Rect::new(at.x, at.y, f.size.width, f.size.height).inflate(o + w, o + w);
        let style = s.outline_style();
        self.out.border(Border {
            rect,
            radii: Radii::ZERO,
            widths: Sides::all(w),
            colors: [s.outline_color(); 4],
            styles: [style; 4],
        });
    }

    fn push_clip(&mut self, rect: Rect, radii: Radii) {
        self.out.push_clip_rounded_rect(rect, radii);
    }

    fn pop_clip(&mut self) {
        self.out.pop_clip();
    }

    fn push_clip_path(&mut self, contours: &[Vec<Point>], even_odd: bool) {
        // Blink clips by the shape's path (a clip node with the path and its bounds,
        // `ClipPathClipper::PathBasedClip`); a shape that is an axis-aligned rectangle (`inset()`
        // without radii) is a rectangle clip, its edges snapped as the boxes' are.
        if let [points] = contours {
            if let Some(rect) = axis_aligned_rect(points) {
                self.out.push_clip_rect(rect);
                return;
            }
        }
        let mut path = Vec::with_capacity(contours.iter().map(|c| c.len() + 1).sum());
        for contour in contours.iter().filter(|c| c.len() > 2) {
            for (i, p) in contour.iter().enumerate() {
                path.push(if i == 0 {
                    PathEl::MoveTo(*p)
                } else {
                    PathEl::LineTo(*p)
                });
            }
            path.push(PathEl::Close);
        }
        match self.out.add_path(&path) {
            Some(range) if !path.is_empty() => self.out.push_clip_path(range, even_odd),
            // (no contour, or a coordinate that is not finite: nothing shows)
            _ => self.out.push_clip_rect(Rect::ZERO),
        }
    }

    fn pop_clip_path(&mut self) {
        self.out.pop_clip();
    }

    fn push_clip_rounded_path(&mut self, rect: Rect, radii: Radii) {
        match self.out.add_path(&rounded_rect_path(rect, &radii)) {
            Some(range) => self.out.push_clip_path(range, false),
            None => self.out.push_clip_rect(Rect::ZERO),
        }
    }

    fn pop_clip_rounded_path(&mut self) {
        self.out.pop_clip();
    }

    fn push_transform(&mut self, m: Matrix) {
        self.transforms += 1;
        self.out.push_transform(m);
    }

    fn pop_transform(&mut self) {
        self.transforms -= 1;
        self.out.pop_transform();
    }

    fn wants(&self, bounds: Rect) -> bool {
        // (closed intervals: an empty box's bounds still count where they are, as its shadow or
        // outline paints around it)
        self.transforms > 0
            || self.cull.is_none_or(|c| {
                bounds.x <= c.right()
                    && bounds.right() >= c.x
                    && bounds.y <= c.bottom()
                    && bounds.bottom() >= c.y
            })
    }

    fn push_opacity(&mut self, opacity: f32, bounds: Rect) {
        self.out.push_opacity(opacity, bounds);
    }

    fn pop_opacity(&mut self) {
        self.out.pop_opacity();
    }

    fn push_filter(&mut self, ops: &[FilterOp], bounds: Rect) {
        self.out.push_filter(ops, bounds);
    }

    fn pop_filter(&mut self) {
        self.out.pop_filter();
    }

    fn backdrop_filter(&mut self, ops: &[FilterOp], rect: Rect, radii: Radii, own: u8) {
        self.out.backdrop_filter(ops, rect, radii, own);
    }

    fn push_layer(&mut self, kind: LayerKind) {
        self.out.push_layer(kind);
    }

    fn add_animation(&mut self, a: snowlark_paint::layer_animation::LayerAnimation) {
        self.out.add_animation(a);
    }

    fn pop_layer(&mut self) {
        self.out.pop_layer();
    }

    fn push_mask(&mut self, f: &Fragment<S>, b: &BoxFragment<S>, at: Point) -> bool {
        let s = &b.style;
        let n = s.mask_layer_count();
        if n == 0 {
            return false;
        }
        // The mask painting area: outside the layers' clip boxes (the border box at most, as
        // `no-clip` is not supported) the element shows nothing.
        let border_box = Rect::new(at.x, at.y, f.size.width, f.size.height);
        let padding_box = border_box.deflate(&b.border);
        let content_box = padding_box.deflate(&b.padding);
        let layers: Vec<MaskLayer> = (0..n).rev().map(|i| s.mask_layer(i)).collect();
        // (one mode for the whole mask: the top layer's)
        let luminance = layers.last().is_some_and(|l| l.luminance);
        self.out.push_mask(border_box, luminance);
        // Layers composite with `add` (source-over), the initial `mask-composite`.
        let geo = LayerArea {
            boxes: [border_box, padding_box, content_box],
            radii: [Radii::ZERO; 3],
            viewport: (self.transforms == 0).then_some(self.viewport),
            local: None,
            clip_override: None,
            strip: None,
        };
        paint_image_layers(
            self.out,
            layers.into_iter().map(|l| l.layer),
            s.image_rendering(),
            &geo,
        );
        self.out.mask_content();
        true
    }

    fn pop_mask(&mut self) {
        self.out.pop_mask();
    }

    fn push_text_clip(&mut self, f: &Fragment<S>, _b: &BoxFragment<S>, at: Point) -> bool {
        // (the layers are clipped to the border box too: the mask shows nothing outside it)
        let border_box = Rect::new(at.x, at.y, f.size.width, f.size.height);
        self.out.push_mask(border_box, false);
        self.text_clip = true;
        true
    }

    fn text_clip_layers(
        &mut self,
        f: &Fragment<S>,
        b: &BoxFragment<S>,
        at: Point,
        scroll: Option<Point>,
    ) {
        self.text_clip = false;
        self.out.mask_content();
        let s = &b.style;
        let border_box = Rect::new(at.x, at.y, f.size.width, f.size.height);
        let (border_box, _) = fieldset_border(f, b, border_box);
        let radii = radii(s, Size::new(border_box.width, border_box.height));
        let geo = self.layer_area(f, b, border_box, radii, at, scroll);
        let bottom = s.background_color_layer();
        if bottom.clip_text {
            let (_, color_rect, color_radii) = geo.layer_areas(&bottom);
            self.out
                .fill_rounded_rect(color_rect, color_radii, s.background_color());
        }
        paint_background_layers(self.out, s, &geo, Some(true));
        self.out.pop_mask();
    }

    fn in_text_clip(&self) -> bool {
        self.text_clip
    }

    fn scrollbars(
        &mut self,
        f: &Fragment<S>,
        b: &BoxFragment<S>,
        at: Point,
        scroll: Point,
        device_scale: f32,
    ) {
        let s = &b.style;
        if self.text_clip || s.visibility() != Visibility::Visible {
            return;
        }
        if let Some(look) = ScrollbarLook::of(s) {
            self.bars(f, at, scroll, device_scale, look);
        }
        // (the resizer is over the bars, in the corner where they meet: CSS UI 4 §5.1)
        if crate::controls::resizer_axes(f).is_some() {
            crate::controls::paint_resizer(
                self.out,
                crate::controls::resizer_rect(f).translate(at.x, at.y),
                s.color_scheme(),
            );
        }
    }
}

impl DisplayListSink<'_> {
    /// A scroll container's scroll bars as `look` draws them.
    fn bars<S: Style>(
        &mut self,
        f: &Fragment<S>,
        at: Point,
        scroll: Point,
        device_scale: f32,
        look: ScrollbarLook,
    ) {
        let s = &f.as_box().expect("a scroll container's box").style;
        let pb = f.padding_rect().translate(at.x, at.y);
        let (range, start) = user_scroll_range(f, s, device_scale);
        let scroll = Point::new(scroll.x - start.x, scroll.y - start.y);
        let inset = look.inset;
        // (a bar does not show in a scrollport too narrow for it)
        let w = look.vertical;
        let gutter = w + 2.0 * inset;
        if w > 0.0 && range.y > 0.0 && pb.width >= gutter {
            if let Some(track) = look.track {
                self.out.fill_rect(
                    Rect::new(pb.right() - gutter, pb.y, gutter, pb.height),
                    track,
                );
            }
            let t = w - 2.0 * look.thumb_inset;
            if let (Some(thumb), true) = (look.thumb, t > 0.0) {
                if let Some((start, len)) = scroll_thumb(
                    pb.height,
                    pb.height + range.y,
                    scroll.y,
                    pb.y + inset,
                    pb.height - 2.0 * inset,
                ) {
                    self.out.fill_rounded_rect(
                        Rect::new(pb.right() - inset - w + look.thumb_inset, start, t, len),
                        Radii::uniform(look.thumb_radius(t)),
                        thumb,
                    );
                }
            }
        }
        let h = look.horizontal;
        let gutter = h + 2.0 * inset;
        if h > 0.0 && range.x > 0.0 && pb.height >= gutter {
            if let Some(track) = look.track {
                self.out.fill_rect(
                    Rect::new(pb.x, pb.bottom() - gutter, pb.width, gutter),
                    track,
                );
            }
            let t = h - 2.0 * look.thumb_inset;
            if let (Some(thumb), true) = (look.thumb, t > 0.0) {
                if let Some((start, len)) = scroll_thumb(
                    pb.width,
                    pb.width + range.x,
                    scroll.x,
                    pb.x + inset,
                    pb.width - 2.0 * inset,
                ) {
                    self.out.fill_rounded_rect(
                        Rect::new(start, pb.bottom() - inset - h + look.thumb_inset, len, t),
                        Radii::uniform(look.thumb_radius(t)),
                        thumb,
                    );
                }
            }
        }
    }
}

/// How a scroll container's own scroll bars are drawn: overlay thumbs over the content (layout
/// reserves no gutter for them), colored for the box's used color scheme (CSS Color Adjust 1 §2.4:
/// scroll bars follow it), or with `scrollbar-color`'s thumb and track (CSS Scrollbars 1 §2), as
/// thick as `scrollbar-width` says (§3); or, when neither is set, as the box's custom scroll bar
/// parts say ([`CustomScrollbar`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollbarLook {
    /// The thumb's color (`None`: no thumb).
    pub thumb: Option<Color>,
    /// The track, painted under the thumb along the edge (`scrollbar-color` and custom bars only:
    /// the automatic overlay bar has none).
    pub track: Option<Color>,
    /// The vertical bar's width and the horizontal bar's height (CSS px; 0: no bar).
    pub vertical: f32,
    pub horizontal: f32,
    /// The space around a bar inside the scrollport's edges (CSS px).
    pub inset: f32,
    /// How far the thumb stands in from the bar's sides across it (CSS px).
    pub thumb_inset: f32,
    /// The thumb's corner radius: a length plus a fraction of its thickness.
    pub thumb_radius: (f32, f32),
}

impl ScrollbarLook {
    /// `None` for `scrollbar-width: none` (no bar; the box still scrolls).
    pub(crate) fn of<S: Style>(s: &S) -> Option<ScrollbarLook> {
        const AUTO: f32 = 6.0;
        let standard = s.scrollbar_width() != ScrollbarWidth::Auto || s.scrollbar_color().is_some();
        if let Some(c) = s.custom_scrollbar().filter(|_| !standard) {
            return Some(ScrollbarLook {
                thumb: c.thumb.map(|t| t.color),
                track: c.track,
                vertical: c.vertical.unwrap_or(AUTO),
                horizontal: c.horizontal.unwrap_or(AUTO),
                inset: 0.0,
                thumb_inset: c.thumb.map_or(0.0, |t| t.inset),
                thumb_radius: c.thumb.map_or((0.0, 0.0), |t| t.radius),
            });
        }
        let thickness = match s.scrollbar_width() {
            ScrollbarWidth::Auto => AUTO,
            ScrollbarWidth::Thin => 4.0,
            ScrollbarWidth::None => return None,
        };
        let (thumb, track) = match s.scrollbar_color() {
            Some(c) => (c.thumb, Some(c.track)),
            None => (
                match s.color_scheme() {
                    ColorScheme::Light => Color::rgba(0, 0, 0, 110),
                    ColorScheme::Dark => Color::rgba(255, 255, 255, 120),
                },
                None,
            ),
        };
        Some(ScrollbarLook {
            thumb: Some(thumb),
            track,
            vertical: thickness,
            horizontal: thickness,
            inset: 2.0,
            thumb_inset: 0.0,
            thumb_radius: (0.0, 0.5),
        })
    }

    /// The thumb's corner radius for a thumb `thickness` thick.
    fn thumb_radius(&self, thickness: f32) -> f32 {
        (self.thumb_radius.0 + self.thumb_radius.1 * thickness)
            .min(thickness / 2.0)
            .max(0.0)
    }
}

/// How far the user can scroll a scroll container on each axis (CSS Overflow 3 §3): an axis whose
/// `overflow` is `hidden` (or `clip`) has no scroll bar and is not user-scrollable, and scrollable
/// overflow past the padding box by less than a device pixel (`device_scale` per CSS px) does not
/// count. Script can still scroll a `hidden` axis.
///
/// Returns the length of each axis's range and where the range starts: scroll positions are
/// measured from the scroll origin (CSS Overflow 3 §2.3), so the start is negative on an axis whose
/// origin is at the right or bottom edge.
pub(crate) fn user_scroll_range<S: Style>(
    f: &Fragment<S>,
    s: &S,
    device_scale: f32,
) -> (Point, Point) {
    let pb = f.padding_rect();
    let so = f.scrollable_overflow();
    let (right, bottom) = f.scroll_origin_end();
    let axis = |overflow: Overflow, extra: f32| {
        let user = matches!(overflow, Overflow::Auto | Overflow::Scroll);
        if user && extra * device_scale.max(1e-3) >= 1.0 {
            extra
        } else {
            0.0
        }
    };
    let len = Point::new(
        axis(s.overflow_x(), (so.width - pb.width).max(0.0)),
        axis(s.overflow_y(), (so.height - pb.height).max(0.0)),
    );
    let start = Point::new(
        if right { -len.x } else { 0.0 },
        if bottom { -len.y } else { 0.0 },
    );
    (len, start)
}

/// A scroll bar thumb's start and length on a track (`track_start`, `track_len`) for a scrollport
/// of `viewport` over `content`, scrolled by `scroll`.
fn scroll_thumb(
    viewport: f32,
    content: f32,
    scroll: f32,
    track_start: f32,
    track_len: f32,
) -> Option<(f32, f32)> {
    const MIN_THUMB: f32 = 24.0;
    let range = content - viewport;
    if range <= 0.0 || track_len <= 0.0 || viewport <= 0.0 {
        return None;
    }
    let len = (track_len * viewport / content)
        .max(MIN_THUMB)
        .min(track_len);
    let start = track_start + (track_len - len) * (scroll / range).clamp(0.0, 1.0);
    Some((start, len))
}

pub(crate) fn inset_sides(a: &Sides, b: &Sides) -> Sides {
    Sides::new(
        a.top + b.top,
        a.right + b.right,
        a.bottom + b.bottom,
        a.left + b.left,
    )
}

/// Where a box's image layers are positioned and painted (CSS Backgrounds 3 §3.5, §3.7, §3.8).
struct LayerArea {
    /// The border, padding and content boxes.
    boxes: [Rect; 3],
    /// The rounding of each box's edge (§5.3), for clipping to it.
    radii: [Radii; 3],
    /// The viewport, for `background-attachment: fixed` (`None` treats it as `scroll`).
    viewport: Option<Rect>,
    /// For a scroll container, the border, padding and content boxes of its scrollable area as
    /// scrolled, for `background-attachment: local` (`None` treats it as `scroll`).
    local: Option<[Rect; 3]>,
    /// The canvas: every layer is painted over it, whatever its `background-clip`.
    clip_override: Option<Rect>,
    /// For a piece of an inline box on several lines, the border, padding and content boxes of
    /// its strip ([`crate::fragment::BoxFragment::strip`]), where layers are positioned; they are
    /// painted in the piece's own `boxes`.
    strip: Option<[Rect; 3]>,
}

impl LayerArea {
    /// A layer's background positioning area, and the painting area it is clipped to with that
    /// clip's rounding.
    fn layer_areas(&self, layer: &BackgroundLayer) -> (Rect, Rect, Radii) {
        let index = |a: BoxArea| match a {
            BoxArea::BorderBox => 0,
            BoxArea::PaddingBox => 1,
            BoxArea::ContentBox => 2,
        };
        let (o, c) = (index(layer.origin), index(layer.clip));
        let (area, clip, radii) = match (layer.attachment, self.viewport, self.local) {
            // Positioned in the viewport, painted in the box's own clip area.
            (BackgroundAttachment::Fixed, Some(viewport), _) => {
                (viewport, self.boxes[c], self.radii[c])
            }
            // Positioned in the scrollable area and scrolled with it. The painting area is that
            // area too, which the scroll container's padding box clips: a `border-box` clip is
            // the padding box (the scrollable area has no border).
            (BackgroundAttachment::Local, _, Some(local)) => {
                if c == 2 {
                    (local[o], self.boxes[1].intersect(&local[2]), Radii::ZERO)
                } else {
                    (local[o], self.boxes[1], self.radii[1])
                }
            }
            _ => (
                self.strip.map_or(self.boxes[o], |s| s[o]),
                self.boxes[c],
                self.radii[c],
            ),
        };
        match self.clip_override {
            Some(all) => (area, all, Radii::ZERO),
            None => (area, clip, radii),
        }
    }
}

/// The border, padding and content boxes of a scroll container's scrollable area (CSS Overflow 3
/// §2.2: its content and its padding), at `at` scrolled by `scroll`.
fn scrolled_area<S: Style>(
    f: &Fragment<S>,
    b: &BoxFragment<S>,
    at: Point,
    scroll: Point,
) -> [Rect; 3] {
    let pb = f.padding_rect();
    let mut content = Rect::ZERO;
    for c in &f.children {
        let cf = &c.fragment;
        let clips = cf
            .as_box()
            .is_some_and(|cb| cb.flags & (fragment_flags::CLIP_X | fragment_flags::CLIP_Y) != 0);
        let r = if clips { cf.border_rect() } else { cf.overflow };
        content = content.union(&r.translate(c.offset.x, c.offset.y));
    }
    let padding = if content.is_empty() {
        pb
    } else {
        Rect::from_ltrb(
            pb.x,
            pb.y,
            pb.right().max(content.right() + b.padding.right),
            pb.bottom().max(content.bottom() + b.padding.bottom),
        )
    }
    .translate(at.x - scroll.x, at.y - scroll.y);
    let w = &b.border;
    let border = Rect::new(
        padding.x - w.left,
        padding.y - w.top,
        padding.width + w.left + w.right,
        padding.height + w.top + w.bottom,
    );
    [border, padding, padding.deflate(&b.padding)]
}

/// Paints `background-image` layers, bottom layer first (CSS Backgrounds 3 §3): those clipped to
/// the text (`Some(true)`), the others (`Some(false)`), or all of them (the canvas: its painting area is the whole canvas, §2.11.1).
fn paint_background_layers<S: Style>(
    out: &mut DisplayListBuilder,
    s: &S,
    geo: &LayerArea,
    text: Option<bool>,
) {
    let n = s.background_layer_count();
    paint_image_layers(
        out,
        (0..n)
            .rev()
            .map(|i| s.background_layer(i))
            .filter(|l| text.is_none_or(|t| l.clip_text == t)),
        s.image_rendering(),
        geo,
    );
}

/// Where the tiles of a layer start along one axis (CSS Backgrounds 3 §3.4): from `start` (the
/// positioned image) every `tile` for `repeat` and `round` (already sized to fit), spaced to fill
/// the positioning area (`area_start`, `area_len`) for `space`, over the painting area `lo..hi`.
fn tile_starts(
    repeat: BackgroundRepeat,
    start: f32,
    tile: f32,
    (area_start, area_len): (f32, f32),
    (lo, hi): (f32, f32),
) -> Vec<f32> {
    let grid = |first: f32, step: f32| {
        let mut v = Vec::new();
        let mut p = first - ((first - lo) / step).ceil() * step;
        while p < hi && v.len() < 4096 {
            v.push(p);
            p += step;
        }
        v
    };
    match repeat {
        BackgroundRepeat::NoRepeat => vec![start],
        BackgroundRepeat::Space => {
            // As many whole images as fit, the first and last touching the area's edges; with
            // room for fewer than two, one image where background-position puts it.
            let n = (area_len / tile + 1.0 / 64.0).floor();
            if n < 2.0 {
                vec![start]
            } else {
                grid(
                    area_start,
                    tile + (area_len - n * tile).max(0.0) / (n - 1.0),
                )
            }
        }
        BackgroundRepeat::Repeat | BackgroundRepeat::Round => grid(start, tile),
    }
}

/// Paints image layers (bottom first) as CSS Backgrounds 3 places them: sized, positioned in their
/// positioning area, repeated, clipped to their painting area.
fn paint_image_layers(
    out: &mut DisplayListBuilder,
    layers: impl Iterator<Item = BackgroundLayer>,
    rendering: ImageRendering,
    geo: &LayerArea,
) {
    for layer in layers {
        if layer.image == Image::None {
            continue;
        }
        let (area, clip, clip_radii) = geo.layer_areas(&layer);
        // (an empty positioning area places an image, it does not hide one: a root element of no
        // height still has its background over the whole canvas)
        if clip.is_empty() {
            continue;
        }
        // The image's natural dimensions (gradients have none).
        let natural = layer.image.natural();
        let mut size = background_size(layer.size, natural, area);
        // `round` (§3.4): scaled so a whole number of images fits; the other dimension keeps the
        // ratio when its background-size is `auto`.
        let (round_x, round_y) = (
            layer.repeat.0 == BackgroundRepeat::Round,
            layer.repeat.1 == BackgroundRepeat::Round,
        );
        if (round_x || round_y) && size.width > 0.0 && size.height > 0.0 {
            let (w0, h0) = (size.width, size.height);
            if round_x {
                size.width = area.width / (area.width / w0).round().max(1.0);
            }
            if round_y {
                size.height = area.height / (area.height / h0).round().max(1.0);
            }
            let auto = |x: bool| match layer.size {
                BackgroundSize::Auto => true,
                BackgroundSize::Explicit(w, h) => if x { w } else { h }.is_none(),
                _ => false,
            };
            if round_x && !round_y && auto(false) {
                size.height = h0 * size.width / w0;
            } else if round_y && !round_x && auto(true) {
                size.width = w0 * size.height / h0;
            }
        }
        if size.width <= 0.0 || size.height <= 0.0 {
            continue;
        }
        let x0 = area.x + layer.position.0.resolve(area.width - size.width);
        let y0 = area.y + layer.position.1.resolve(area.height - size.height);
        out.push_clip_rounded_rect(clip, clip_radii);
        let xs = tile_starts(
            layer.repeat.0,
            x0,
            size.width,
            (area.x, area.width),
            (clip.x, clip.right()),
        );
        let ys = tile_starts(
            layer.repeat.1,
            y0,
            size.height,
            (area.y, area.height),
            (clip.y, clip.bottom()),
        );
        // Images that are the same all along an axis, repeated edge to edge along it, are one
        // image over the tiles' span: a vertical gradient repeated across a box is that gradient
        // across the box (as one pattern draw, not an item per tile).
        let span = |starts: &[f32], step: f32, repeat: BackgroundRepeat| match starts {
            [first, .., last]
                if matches!(repeat, BackgroundRepeat::Repeat | BackgroundRepeat::Round) =>
            {
                Some((*first, *last + step - *first))
            }
            _ => None,
        };
        let (xs, width) = match span(&xs, size.width, layer.repeat.0) {
            Some((x, w)) if constant_along(&layer.image, true) => (vec![x], w),
            _ => (xs, size.width),
        };
        let (ys, height) = match span(&ys, size.height, layer.repeat.1) {
            Some((y, h)) if constant_along(&layer.image, false) => (vec![y], h),
            _ => (ys, size.height),
        };
        for &y in &ys {
            for &x in &xs {
                draw_image(out, &layer.image, Rect::new(x, y, width, height), rendering);
            }
        }
        out.pop_clip();
    }
}

/// Whether `image` is the same at every point along the x axis (`horizontal`) or the y axis, at
/// any size: a color, or a linear gradient whose line runs along the other axis (its colors vary
/// only along the gradient line, CSS Images 3 §3.1.1).
fn constant_along(image: &Image, horizontal: bool) -> bool {
    match image {
        Image::Color(_) => true,
        Image::LinearGradient(g) if g.corner.is_none() => {
            let a = g.angle.rem_euclid(180.0);
            if horizontal {
                a == 0.0
            } else {
                a == 90.0
            }
        }
        _ => false,
    }
}

/// Draws an image (a raster image, a gradient, a color, a cross-fade) scaled into `tile`.
fn draw_image(out: &mut DisplayListBuilder, image: &Image, tile: Rect, rendering: ImageRendering) {
    draw_image_faded(out, image, tile, rendering, 1.0);
}

/// [`draw_image`] with the image's alpha multiplied by `opacity` (a cross-fade's layers).
fn draw_image_faded(
    out: &mut DisplayListBuilder,
    image: &Image,
    tile: Rect,
    rendering: ImageRendering,
    opacity: f32,
) {
    let faded = opacity < 1.0;
    match image {
        Image::Raster { id, .. } => {
            out.image(*id, tile, None, opacity, rendering);
            return;
        }
        Image::Color(c) => {
            let a = (c.a as f32 * opacity).round() as u8;
            out.fill_rect(tile, c.with_alpha(a));
            return;
        }
        Image::None => return,
        _ => {}
    }
    if faded {
        out.push_opacity(opacity, tile);
    }
    match image {
        Image::LinearGradient(g) => {
            let angle = match g.corner {
                Some((right, bottom)) => corner_angle(right, bottom, tile.width, tile.height),
                None => g.angle,
            };
            let (a, b) = gradient_line(angle, tile);
            let len = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
            let stops = interpolated_stops(&resolve_stops(&g.stops, len), &g.ramps);
            let (from, scale, stops) = fit_stops(stops, g.repeating, false);
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            let a2 = Point::new(a.x + dx * from, a.y + dy * from);
            let b2 = Point::new(a2.x + dx * scale, a2.y + dy * scale);
            out.linear_gradient(
                tile,
                Radii::ZERO,
                a2,
                b2,
                &stops,
                if g.repeating {
                    Extend::Repeat
                } else {
                    Extend::Clamp
                },
            );
        }
        Image::RadialGradient(g) => {
            let cx = tile.x + finite_length(g.center.0.resolve(tile.width));
            let cy = tile.y + finite_length(g.center.1.resolve(tile.height));
            let (rx, ry) = radial_size(g, tile, cx, cy);
            let (rx, ry) = (finite_length(rx), finite_length(ry));
            // (stop lengths are along the gradient ray, which runs horizontally)
            let stops = interpolated_stops(&resolve_stops(&g.stops, rx), &g.ramps);
            let (_, scale, stops) = fit_stops(stops, g.repeating, true);
            out.radial_gradient(
                tile,
                Radii::ZERO,
                Point::new(cx, cy),
                snowlark_paint::Size::new(rx * scale, ry * scale),
                &stops,
                if g.repeating {
                    Extend::Repeat
                } else {
                    Extend::Clamp
                },
            );
        }
        Image::ConicGradient(g) => {
            let cx = tile.x + finite_length(g.center.0.resolve(tile.width));
            let cy = tile.y + finite_length(g.center.1.resolve(tile.height));
            // (positions are fractions of a turn: a percentage of 1)
            let mut stops = interpolated_stops(&resolve_stops(&g.stops, 1.0), &g.ramps);
            if let (true, Some(first), Some(&last)) = (g.repeating, stops.first(), stops.last()) {
                // (no period: a solid image of the last color, CSS Images 4 §3.6)
                if last.offset - first.offset <= 1e-6 {
                    stops = vec![last, last];
                }
            }
            out.conic_gradient(
                tile,
                Radii::ZERO,
                Point::new(cx, cy),
                if g.angle.is_finite() {
                    g.angle.to_radians()
                } else {
                    0.0
                },
                &stops,
                if g.repeating {
                    Extend::Repeat
                } else {
                    Extend::Clamp
                },
            );
        }
        Image::CrossFade(c) => {
            // CSS Images 4 §2.6.2: each image at the cross-fade's size, its alpha multiplied by
            // its fraction, the results added (premultiplied: `plus-lighter`), in a group of its
            // own so the sum is composited over what is below once.
            out.push_opacity(1.0, tile);
            out.push_plus();
            for (fraction, layer) in &c.layers {
                if *fraction > 0.0 {
                    draw_image_faded(out, layer, tile, rendering, fraction.min(1.0));
                }
            }
            out.pop_plus();
            out.pop_opacity();
        }
        Image::Raster { .. } | Image::Color(_) | Image::None => {}
    }
    if faded {
        out.pop_opacity();
    }
}

/// The angle (degrees clockwise from up) of `linear-gradient(to <corner>)` over a `w` x `h` box
/// (CSS Images 3 §3.1.1): the gradient line is perpendicular to the diagonal between the two
/// neighbouring corners, pointing into the named one.
fn corner_angle(right: bool, bottom: bool, w: f32, h: f32) -> f32 {
    let a = h.atan2(w).to_degrees();
    let a = if a.is_finite() { a } else { 45.0 };
    match (right, bottom) {
        (true, false) => a,
        (true, true) => 180.0 - a,
        (false, true) => 180.0 + a,
        (false, false) => 360.0 - a,
    }
}

/// Where the tiles of a border image part go along one axis (CSS Backgrounds 3 §6.6): over
/// `start..start + len`, with tiles `tile` long (the slice scaled to the border width). Returns
/// the tiles' starts and their length.
fn border_image_tiles(
    repeat: BorderImageRepeat,
    start: f32,
    len: f32,
    tile: f32,
) -> (Vec<f32>, f32) {
    const MAX: usize = 4096;
    if len <= 0.0 {
        return (Vec::new(), 0.0);
    }
    if repeat == BorderImageRepeat::Stretch || tile.is_nan() || tile <= 0.0 || !tile.is_finite() {
        return (vec![start], len);
    }
    match repeat {
        BorderImageRepeat::Round => {
            let n = (len / tile).round().max(1.0).min(MAX as f32);
            let t = len / n;
            ((0..n as usize).map(|i| start + i as f32 * t).collect(), t)
        }
        BorderImageRepeat::Space => {
            let n = (len / tile + 1.0 / 64.0).floor().min(MAX as f32);
            if n < 1.0 {
                return (Vec::new(), tile);
            }
            let gap = ((len - n * tile) / (n + 1.0)).max(0.0);
            (
                (0..n as usize)
                    .map(|i| start + gap + i as f32 * (tile + gap))
                    .collect(),
                tile,
            )
        }
        _ => {
            // centered: a tile's middle on the area's middle
            let first = start + len / 2.0 - tile / 2.0;
            let back = ((first - start) / tile).ceil();
            let mut p = first - back * tile;
            let mut v = Vec::new();
            while p < start + len && v.len() < MAX {
                v.push(p);
                p += tile;
            }
            (v, tile)
        }
    }
}

/// Paints a border image (CSS Backgrounds 3 §6) for a box whose border box is `border_box` and
/// border widths `borders`: the source image sliced in nine by `border-image-slice`, the edge
/// and corner parts drawn in the border image area's border regions (`border-image-width`, the
/// area being the border box grown by `border-image-outset`), edges and the `fill` middle
/// scaled and tiled per `border-image-repeat`.
fn paint_border_image(
    out: &mut DisplayListBuilder,
    bi: &BorderImage,
    border_box: Rect,
    borders: &Sides,
    rendering: ImageRendering,
    device_scale: f32,
) {
    // (the area and the widths in whole device pixels, as Blink's nine-piece grid has them: the
    // parts meet on pixel edges, and the parts drawn under a scale stay on them)
    let d = device_scale.max(1e-3);
    let px = |v: f32| (v * d).round() / d;
    let b = [borders.top, borders.right, borders.bottom, borders.left];
    // §6.5 border-image-outset: multiples of the border width, or lengths
    let o: Vec<f32> = (0..4)
        .map(|i| match bi.outset[i] {
            BorderImageLength::Number(n) => n * b[i],
            BorderImageLength::LengthPercentage(l) => l.resolve(0.0),
            BorderImageLength::Auto => 0.0,
        })
        .map(|v| finite_length(v).max(0.0))
        .collect();
    let area = Rect::from_ltrb(
        px(border_box.x - o[3]),
        px(border_box.y - o[0]),
        px(border_box.right() + o[1]),
        px(border_box.bottom() + o[2]),
    );
    if area.is_empty() {
        return;
    }
    // the image's size: its natural one, else (gradients) the border image area
    let (w, h) = bi
        .source
        .natural()
        .concrete((None, None), (area.width, area.height));
    if !(w > 0.0 && h > 0.0) {
        return;
    }
    // §6.2 border-image-slice, clamped to the image
    let dim = |i: usize| if i.is_multiple_of(2) { h } else { w };
    let sl: Vec<f32> = (0..4)
        .map(|i| match bi.slice[i] {
            NumberOrPercent::Number(n) => n,
            NumberOrPercent::Percent(p) => p / 100.0 * dim(i),
        })
        .enumerate()
        .map(|(i, v)| v.clamp(0.0, dim(i)))
        .collect();
    // §6.4 border-image-width, then scaled down together when opposite ones overlap
    let area_dim = |i: usize| {
        if i.is_multiple_of(2) {
            area.height
        } else {
            area.width
        }
    };
    let mut wd: Vec<f32> = (0..4)
        .map(|i| match bi.width[i] {
            BorderImageLength::Number(n) => n * b[i],
            BorderImageLength::LengthPercentage(l) => l.resolve(area_dim(i)),
            BorderImageLength::Auto => sl[i],
        })
        .map(|v| finite_length(v).max(0.0))
        .collect();
    let f = [area.height / (wd[0] + wd[2]), area.width / (wd[1] + wd[3])]
        .into_iter()
        .filter(|v| v.is_finite())
        .fold(1.0f32, f32::min);
    for v in &mut wd {
        *v = px(*v * f.min(1.0));
    }
    let (st, sr, sb, slf) = (sl[0], sl[1], sl[2], sl[3]);
    let (wt, wr, wb, wl) = (wd[0], wd[1], wd[2], wd[3]);
    // source parts (image coordinates) and destination parts, row by row
    let sx = [0.0, slf, w - sr, w];
    let sy = [0.0, st, h - sb, h];
    let dx = [area.x, area.x + wl, area.right() - wr, area.right()];
    let dy = [area.y, area.y + wt, area.bottom() - wb, area.bottom()];
    // the scale of each edge across its border: top, right, bottom, left (none when 0 or ∞)
    let factor = |dest: f32, src: f32| {
        let v = dest / src;
        (v.is_finite() && v > 0.0).then_some(v)
    };
    let (ft, fr, fb, fl) = (
        factor(wt, st),
        factor(wr, sr),
        factor(wb, sb),
        factor(wl, slf),
    );
    for row in 0..3 {
        for col in 0..3 {
            let middle = row == 1 && col == 1;
            if middle && !bi.fill {
                continue;
            }
            let src = Rect::from_ltrb(sx[col], sy[row], sx[col + 1], sy[row + 1]);
            let dest = Rect::from_ltrb(dx[col], dy[row], dx[col + 1], dy[row + 1]);
            if src.width <= 0.0 || src.height <= 0.0 || dest.width <= 0.0 || dest.height <= 0.0 {
                continue;
            }
            // tile sizes: corners are stretched; an edge keeps its slice's proportions scaled to
            // its border; the middle scales as the top (else bottom) and left (else right) edges
            let (tw, th) = match (row, col) {
                (1, 1) => (
                    ft.or(fb).map_or(src.width, |s| src.width * s),
                    fl.or(fr).map_or(src.height, |s| src.height * s),
                ),
                (0, 1) => (src.width * dest.height / src.height, dest.height),
                (2, 1) => (src.width * dest.height / src.height, dest.height),
                (1, 0) => (dest.width, src.height * dest.width / src.width),
                (1, 2) => (dest.width, src.height * dest.width / src.width),
                _ => (dest.width, dest.height),
            };
            let (rx, ry) = match (row, col) {
                (1, 1) => (bi.repeat.0, bi.repeat.1),
                (_, 1) => (bi.repeat.0, BorderImageRepeat::Stretch),
                (1, _) => (BorderImageRepeat::Stretch, bi.repeat.1),
                _ => (BorderImageRepeat::Stretch, BorderImageRepeat::Stretch),
            };
            let (xs, tw) = border_image_tiles(rx, dest.x, dest.width, tw);
            let (ys, th) = border_image_tiles(ry, dest.y, dest.height, th);
            if xs.is_empty() || ys.is_empty() {
                continue;
            }
            // each tile shows `src`: the whole image is drawn scaled so that `src` lands on the
            // tile, clipped to the part's region
            let (kx, ky) = (tw / src.width, th / src.height);
            out.push_clip_rect(dest);
            for &y in &ys {
                for &x in &xs {
                    let tile = Rect::new(x, y, tw, th);
                    out.push_clip_rect(tile);
                    let (ox, oy) = (x - src.x * kx, y - src.y * ky);
                    if let Image::Raster { id, .. } = bi.source {
                        // (a raster image at its own size, scaled per axis: an SVG image is
                        // rendered at its concrete size, then stretched like a bitmap)
                        out.push_transform(Matrix::new(kx, 0.0, 0.0, ky, ox, oy));
                        out.image(id, Rect::new(0.0, 0.0, w, h), None, 1.0, rendering);
                        out.pop_transform();
                    } else {
                        draw_image(
                            out,
                            &bi.source,
                            Rect::new(ox, oy, w * kx, h * ky),
                            rendering,
                        );
                    }
                    out.pop_clip();
                }
            }
            out.pop_clip();
        }
    }
}

/// `background-size` (CSS Backgrounds 3 §3.9): `contain`/`cover` scale the natural ratio to the
/// positioning area (an image without one fills it); explicit and `auto` sizes go through CSS Images 3's
/// default sizing algorithm with the positioning area as the default object size.
pub(crate) fn background_size(size: BackgroundSize, natural: NaturalSize, area: Rect) -> Size {
    let a = (area.width, area.height);
    let (w, h) = match size {
        BackgroundSize::Cover | BackgroundSize::Contain => match natural.ratio() {
            Some(r) if size == BackgroundSize::Cover => cover(r, a),
            Some(r) => contain(r, a),
            None => a,
        },
        BackgroundSize::Explicit(w, h) => natural.concrete(
            (
                w.map(|l| l.resolve(area.width)),
                h.map(|l| l.resolve(area.height)),
            ),
            a,
        ),
        BackgroundSize::Auto => natural.concrete((None, None), a),
    };
    Size::new(w, h)
}

/// The gradient line of a `linear-gradient()` at `angle` degrees over a box (CSS Images 3 §3.1.1).
fn gradient_line(angle: f32, r: Rect) -> (Point, Point) {
    let a = angle.to_radians();
    let (sin, cos) = a.sin_cos();
    let half = (r.width * sin.abs() + r.height * cos.abs()) / 2.0;
    let c = Point::new(r.x + r.width / 2.0, r.y + r.height / 2.0);
    let d = Point::new(sin * half, -cos * half);
    (
        Point::new(c.x - d.x, c.y - d.y),
        Point::new(c.x + d.x, c.y + d.y),
    )
}

fn radial_size(g: &RadialGradient, r: Rect, cx: f32, cy: f32) -> (f32, f32) {
    let l = cx - r.x;
    let rt = r.right() - cx;
    let t = cy - r.y;
    let b = r.bottom() - cy;
    let (sx, sy) = match g.size {
        RadialSize::ClosestSide => (l.min(rt), t.min(b)),
        RadialSize::FarthestSide => (l.max(rt), t.max(b)),
        RadialSize::ClosestCorner => {
            let (x, y) = (l.min(rt), t.min(b));
            (x * std::f32::consts::SQRT_2, y * std::f32::consts::SQRT_2)
        }
        RadialSize::FarthestCorner => {
            let (x, y) = (l.max(rt), t.max(b));
            (x * std::f32::consts::SQRT_2, y * std::f32::consts::SQRT_2)
        }
        RadialSize::Explicit(a, b2) => (
            finite_length(a.resolve(r.width)),
            finite_length(b2.resolve(r.height)),
        ),
    };
    if g.circle {
        let rad = match g.size {
            RadialSize::ClosestSide => l.min(rt).min(t).min(b),
            RadialSize::FarthestSide => l.max(rt).max(t).max(b),
            RadialSize::ClosestCorner => (l.min(rt).powi(2) + t.min(b).powi(2)).sqrt(),
            RadialSize::FarthestCorner => (l.max(rt).powi(2) + t.max(b).powi(2)).sqrt(),
            RadialSize::Explicit(a, _) => finite_length(a.resolve(r.width)),
        };
        (rad, rad)
    } else {
        (sx, sy)
    }
}

/// The largest length layout keeps (Blink's `LayoutUnit` range): an infinite or huge computed
/// length (`calc(infinity * 1px)`, CSS Values 4 §10.9) is clamped to it rather than painted as
/// infinity.
const MAX_LENGTH: f32 = 33_554_428.0;

/// The corner radius of a themed control's frame ([`fragment_flags::THEMED`]).
const THEMED_RADIUS: f32 = 2.0;

fn finite_length(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(-MAX_LENGTH, MAX_LENGTH)
    }
}

/// The color `t` of the way from `a` to `b`, interpolated in premultiplied sRGB (CSS Images 4
/// §3.5.2: gradients interpolate premultiplied; with a transparent end the other's hue stays).
fn mix(a: Color, b: Color, t: f32) -> Color {
    let (aa, ba) = (a.a as f32 / 255.0, b.a as f32 / 255.0);
    let alpha = aa + (ba - aa) * t;
    if alpha <= 0.0 {
        return Color::TRANSPARENT;
    }
    let c = |x: u8, y: u8| {
        let v = (x as f32 * aa + (y as f32 * ba - x as f32 * aa) * t) / alpha;
        v.round().clamp(0.0, 255.0) as u8
    };
    Color::rgba(
        c(a.r, b.r),
        c(a.g, b.g),
        c(a.b, b.b),
        (alpha * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

/// The color of a list of stops at `t` (clamped at its ends).
fn color_at(stops: &[GradientStop], t: f32) -> Color {
    let (Some(first), Some(last)) = (stops.first(), stops.last()) else {
        return Color::TRANSPARENT;
    };
    if t <= first.offset {
        return first.color;
    }
    for w in stops.windows(2) {
        if t <= w[1].offset {
            let span = w[1].offset - w[0].offset;
            return if span > 0.0 {
                mix(w[0].color, w[1].color, (t - w[0].offset) / span)
            } else {
                w[1].color
            };
        }
    }
    last.color
}

/// Fits resolved stops to the 0..1 range a rasterizer's gradient brush covers: the returned
/// `(from, scale, stops)` say that the brush's 0..1 is `from..from + scale` of the original
/// gradient line (or ray). A clamped linear gradient's line is extended to its outermost stops;
/// a radial gradient's starts at its center (stops before it take the color there). A repeating
/// gradient's period is its first to last stop (CSS Images 3 §3.6): the brush covers one period
/// from the line's start, its stops wrapped to that phase; with no period it is one solid color,
/// the average of the stops.
fn fit_stops(
    stops: Vec<GradientStop>,
    repeating: bool,
    radial: bool,
) -> (f32, f32, Vec<GradientStop>) {
    let (Some(first), Some(last)) = (stops.first().copied(), stops.last().copied()) else {
        return (0.0, 1.0, stops);
    };
    if repeating {
        let period = last.offset - first.offset;
        if period.is_nan() || period <= 1e-6 {
            // CSS Images 4 §3.6: a repeating gradient whose first and last stops are at one
            // place is a solid image of its last stop's color.
            let c = last.color;
            let solid = vec![
                GradientStop {
                    offset: 0.0,
                    color: c,
                },
                GradientStop {
                    offset: 1.0,
                    color: c,
                },
            ];
            return (0.0, 1.0, solid);
        }
        // Brush position u (0..1 of a period from the line's start) shows the pattern at
        // (u - phase) mod 1, the pattern starting at the first stop.
        let phase = (first.offset / period).rem_euclid(1.0);
        let pattern: Vec<GradientStop> = stops
            .iter()
            .map(|s| GradientStop {
                offset: (s.offset - first.offset) / period,
                color: s.color,
            })
            .collect();
        let edge = color_at(&pattern, 1.0 - phase);
        let mut wrapped: Vec<(f32, u8, Color)> = pattern
            .iter()
            .map(|s| {
                let u = s.offset + phase;
                if u >= 1.0 && phase > 0.0 {
                    (u - 1.0, 0, s.color)
                } else {
                    (u.min(1.0), 1, s.color)
                }
            })
            .collect();
        // (the stops wrapped past the end come first; a stable sort keeps their order)
        wrapped.sort_by(|a, b| (a.0, a.1).partial_cmp(&(b.0, b.1)).unwrap());
        let mut out = Vec::with_capacity(wrapped.len() + 2);
        if phase > 0.0 {
            out.push(GradientStop {
                offset: 0.0,
                color: edge,
            });
        }
        out.extend(
            wrapped
                .into_iter()
                .map(|(offset, _, color)| GradientStop { offset, color }),
        );
        if phase > 0.0 {
            out.push(GradientStop {
                offset: 1.0,
                color: edge,
            });
        }
        return (0.0, period, out);
    }
    let from = if radial { 0.0 } else { first.offset.min(0.0) };
    let to = last.offset.max(1.0);
    let scale = (to - from).max(1e-6);
    let mut out: Vec<GradientStop> = Vec::with_capacity(stops.len() + 1);
    if radial && first.offset < 0.0 {
        // (the part before the center is not drawn: the color at the center starts it)
        out.push(GradientStop {
            offset: 0.0,
            color: color_at(&stops, 0.0),
        });
    }
    out.extend(
        stops
            .iter()
            .filter(|s| !(radial && s.offset < 0.0))
            .map(|s| GradientStop {
                offset: (s.offset - from) / scale,
                color: s.color,
            }),
    );
    (from, scale, out)
}

/// A color stop or transition hint with its position as a fraction of the gradient line.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ResolvedStop {
    offset: f32,
    color: Color,
    hint: bool,
}

/// Color stop and transition hint positions as fractions of the gradient line (CSS Images 4 §3.5.3
/// color stop fix-up): the first and last color stops default to the line's ends, a position below
/// an earlier one is raised to it, and runs of color stops without positions are spaced evenly
/// between the positioned ones around them.
#[allow(clippy::needless_range_loop)]
fn resolve_stops(stops: &[ColorStop], len: f32) -> Vec<ResolvedStop> {
    let n = stops.len();
    let mut pos: Vec<Option<f32>> = stops
        .iter()
        .map(|s| {
            s.position.map(|p| {
                if len > 0.0 {
                    finite_length(p.resolve(len)) / len
                } else {
                    0.0
                }
            })
        })
        .collect();
    let colors: Vec<usize> = (0..n).filter(|&i| !stops[i].hint).collect();
    let (Some(&first), Some(&last)) = (colors.first(), colors.last()) else {
        return Vec::new();
    };
    if pos[first].is_none() {
        pos[first] = Some(0.0);
    }
    if pos[last].is_none() {
        pos[last] = Some(1.0);
    }
    // Monotonic: a position below one before it takes the largest before it.
    let mut largest = f32::NEG_INFINITY;
    for p in pos.iter_mut().flatten() {
        if *p < largest {
            *p = largest;
        }
        largest = *p;
    }
    // Spread runs of unpositioned color stops evenly between the color stops around them.
    let mut k = 0;
    while k < colors.len() {
        if pos[colors[k]].is_none() {
            let start = k - 1;
            let mut end = k;
            while pos[colors[end]].is_none() {
                end += 1;
            }
            let (a, b) = (pos[colors[start]].unwrap(), pos[colors[end]].unwrap());
            for j in k..end {
                pos[colors[j]] = Some(a + (b - a) * (j - start) as f32 / (end - start) as f32);
            }
            k = end;
        }
        k += 1;
    }
    // (a hint before a color stop that was spread past it moves with it)
    let mut largest = f32::NEG_INFINITY;
    let mut out = Vec::with_capacity(n);
    for (s, p) in stops.iter().zip(pos) {
        let offset = p.unwrap_or(largest).max(largest);
        largest = offset;
        out.push(ResolvedStop {
            offset,
            color: s.color,
            hint: s.hint,
        });
    }
    out
}

/// The stops a rasterizer's gradient brush takes (which blends adjacent stops linearly in
/// premultiplied sRGB) for resolved color stops and hints: a transition hint bends the blend
/// between its color stops (CSS Images 4 §3.5.3: at fraction `p` the color is `p^(ln 0.5 / ln H)`
/// of the way, `H` the hint's fraction), and `ramps` give the colors of another interpolation
/// color space (whose ends can differ from the stops' own colors: a missing component takes the
/// other stop's); both are sampled [`RAMP_STEPS`] times between the two stops.
fn interpolated_stops(resolved: &[ResolvedStop], ramps: &[Ramp]) -> Vec<GradientStop> {
    let colors: Vec<usize> = (0..resolved.len()).filter(|&i| !resolved[i].hint).collect();
    let mut out = Vec::with_capacity(colors.len());
    let push = |out: &mut Vec<GradientStop>, offset: f32, color: Color| {
        // (a repeat of the last stop adds nothing)
        if out
            .last()
            .is_some_and(|l: &GradientStop| l.offset == offset && l.color == color)
        {
            return;
        }
        out.push(GradientStop { offset, color });
    };
    for (j, &i) in colors.iter().enumerate() {
        let a = resolved[i];
        let Some(&next) = colors.get(j + 1) else {
            push(&mut out, a.offset, a.color);
            break;
        };
        let b = resolved[next];
        let ramp = ramps.get(j);
        let at = |k: usize| match ramp {
            Some(r) => r[k.min(RAMP_STEPS)],
            None if k == 0 => a.color,
            None if k >= RAMP_STEPS => b.color,
            None => mix(a.color, b.color, k as f32 / RAMP_STEPS as f32),
        };
        push(&mut out, a.offset, at(0));
        let span = b.offset - a.offset;
        let hint = resolved[i + 1..next].iter().find(|s| s.hint).map(|h| {
            if span > 0.0 {
                (h.offset - a.offset) / span
            } else {
                0.5
            }
        });
        if span.is_nan() || span <= 0.0 || (hint.is_none() && ramp.is_none()) {
            if ramp.is_some() {
                push(&mut out, b.offset, at(RAMP_STEPS));
            }
            continue;
        }
        match hint {
            // (a hint at either stop: a hard transition there)
            Some(h) if h <= 0.0 => {
                push(&mut out, a.offset, at(RAMP_STEPS));
                push(&mut out, b.offset, at(RAMP_STEPS));
                continue;
            }
            Some(h) if h >= 1.0 => {
                push(&mut out, b.offset, at(0));
                push(&mut out, b.offset, at(RAMP_STEPS));
                continue;
            }
            _ => {}
        }
        for k in 1..=RAMP_STEPS {
            let p = k as f32 / RAMP_STEPS as f32;
            let color = match hint {
                Some(h) if (h - 0.5).abs() > 1e-6 => {
                    // the bent fraction, read off the samples (or blended directly in sRGB)
                    let c = p.powf(0.5f32.ln() / h.ln());
                    if ramp.is_none() {
                        mix(a.color, b.color, c)
                    } else {
                        let c = c * RAMP_STEPS as f32;
                        let k0 = (c.floor() as usize).min(RAMP_STEPS - 1);
                        mix(at(k0), at(k0 + 1), c - k0 as f32)
                    }
                }
                _ => at(k),
            };
            push(&mut out, a.offset + span * p, color);
        }
    }
    transparent_fixup(out)
}

/// Gives each fully transparent stop the color channels of its neighbours (two stops at its
/// offset when they differ), which premultiplied interpolation ignores anyway: a brush that
/// quantizes its colors then blends a transparent stop the same whatever color it was written in
/// (`transparent`, `rgba(0 0 255 / 0)`).
fn transparent_fixup(stops: Vec<GradientStop>) -> Vec<GradientStop> {
    if !stops.iter().any(|s| s.color.a == 0) || stops.iter().all(|s| s.color.a == 0) {
        return stops;
    }
    let opaque_near = |from: usize, back: bool| -> Option<Color> {
        if back {
            stops[..from]
                .iter()
                .rev()
                .find(|s| s.color.a > 0)
                .map(|s| s.color)
        } else {
            stops[from + 1..]
                .iter()
                .find(|s| s.color.a > 0)
                .map(|s| s.color)
        }
    };
    let mut out = Vec::with_capacity(stops.len() + 2);
    for (i, s) in stops.iter().enumerate() {
        if s.color.a != 0 {
            out.push(*s);
            continue;
        }
        let before = opaque_near(i, true);
        let after = opaque_near(i, false);
        let clear = |c: Color| GradientStop {
            offset: s.offset,
            color: c.with_alpha(0),
        };
        match (before, after) {
            (Some(b), Some(a)) if b.with_alpha(0) != a.with_alpha(0) => {
                out.push(clear(b));
                out.push(clear(a));
            }
            (Some(c), _) | (None, Some(c)) => out.push(clear(c)),
            (None, None) => out.push(*s),
        }
    }
    out
}

#[allow(dead_code)]
fn unused(_: Color) {}

/// Which text decoration lines a pass paints: underlines and overlines go below the text,
/// line-throughs over it (CSS Text Decoration 3 §2.3).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lines {
    UnderOver,
    Through,
}

/// The selected part of a text fragment as the painter takes it: its left and right edges (`None`
/// at an end of the fragment), its text's color and its highlight's shadows.
type SelectedPart<'s> = (Option<f32>, Option<f32>, Color, &'s [Shadow]);

// ----- selection -----

/// A document selection to paint: the selected part of each text node with its `::selection`
/// colors, the caret of a focused editing host, and the highlight colors of text no `::selection`
/// style gives any.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextSelection {
    pub ranges: std::collections::HashMap<NodeId, SelectedText>,
    /// The replaced elements the selection contains, with the translucent color painted over each
    /// (CSS Pseudo-Elements 4 §3.6.4).
    pub replaced: std::collections::HashMap<NodeId, Color>,
    /// The `text-shadow` lists of the selection's highlights, which [`SelectedText::shadows`]
    /// index.
    pub shadows: Vec<Vec<Shadow>>,
    pub caret: Option<(NodeId, u32)>,
    /// The user agent's highlight background (CSS Pseudo-Elements 4 §3.3), where a text node has
    /// no [`SelectedText::highlight`].
    pub background: Color,
    /// The user agent's color of selected text (`None`: unchanged).
    pub color: Option<Color>,
    pub caret_color: Option<Color>,
    /// A caret where there is no text (an empty editing host): a rectangle in the document's
    /// viewport coordinates, painted by the caller over the page.
    pub caret_box: Option<Rect>,
}

/// The selected part of a text node: a byte range of its DOM text.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SelectedText {
    pub start: u32,
    pub end: u32,
    /// The colors its originating element's `::selection` style gives (CSS Pseudo-Elements 4
    /// §3.5): the background (transparent when only a `color` is given, §3.3.1) and the text's
    /// color (`None`: the text keeps its own). `None`: the user agent's highlight colors.
    pub highlight: Option<(Color, Option<Color>)>,
    /// The selection goes on past this node's text (it is not the last selected text node).
    pub continues: bool,
    /// Its highlight's `text-shadow`s: one more than their index in [`TextSelection::shadows`],
    /// 0 for none.
    pub shadows: u32,
}

/// The shaped text offset of the cluster boundary nearest to `x` (from the fragment's start).
pub(crate) fn cluster_boundary_at<S: Style>(t: &TextFragment<S>, x: f32) -> u32 {
    let clusters = &t.shaped.clusters[t.clusters.start as usize..t.clusters.end as usize];
    let mut cx = 0.0;
    for c in clusters {
        let w = c.advance + t.letter_spacing + if c.whitespace { t.justify } else { 0.0 };
        if x < cx + w / 2.0 {
            return c.text.0;
        }
        cx += w;
    }
    clusters.last().map_or(0, |c| c.text.1)
}

/// What a [`CaretSink`] looks for.
pub(crate) enum CaretQuery<'q> {
    /// The caret position for a point: among the text fragments `keep` accepts, one on the point's
    /// line (the nearest horizontally), else the nearest line.
    Position {
        point: Point,
        keep: &'q dyn Fn(NodeId) -> bool,
    },
    /// The caret rectangle of a DOM byte offset in a text node.
    Rect { node: NodeId, byte: u32 },
}

/// Finds a caret position or rectangle among the text fragments as they are painted (with their
/// transforms and clips). It keeps only the best answer so far: nothing per fragment is stored, so a
/// query costs one walk of the fragment tree and no allocation per text fragment.
pub(crate) struct CaretSink<'q> {
    query: CaretQuery<'q>,
    /// The transforms in effect, each with its inverse.
    transforms: Vec<(Matrix, Option<Matrix>)>,
    clips: Vec<Rect>,
    /// [`CaretQuery::Position`]: (vertical distance, horizontal distance, node, DOM byte offset).
    best: Option<(f32, f32, NodeId, u32)>,
    /// [`CaretQuery::Position`]: look only at what is on the point's line (a first pass).
    pub line_only: bool,
    /// [`CaretQuery::Rect`]: the rectangle found, and whether it is final.
    rect: Option<Rect>,
    done: bool,
}

impl<'q> CaretSink<'q> {
    pub fn new(query: CaretQuery<'q>) -> Self {
        CaretSink {
            query,
            transforms: vec![(Matrix::IDENTITY, Some(Matrix::IDENTITY))],
            clips: vec![Rect::infinite()],
            best: None,
            rect: None,
            done: false,
            line_only: false,
        }
    }

    /// The answer to a [`CaretQuery::Position`]: the text node and DOM byte offset.
    pub fn position(&self) -> Option<(NodeId, u32)> {
        self.best.map(|(_, _, n, o)| (n, o))
    }

    /// The answer to a [`CaretQuery::Rect`] (1 px wide, the line's height), in the painted
    /// coordinates.
    pub fn caret_rect(&self) -> Option<Rect> {
        self.rect
    }
}

impl<S: Style> Sink<S> for CaretSink<'_> {
    fn decorations(
        &mut self,
        _f: &Fragment<S>,
        _b: &BoxFragment<S>,
        _at: Point,
        _canvas: bool,
        _scroll: Option<Point>,
    ) {
    }

    fn text(&mut self, size: Size, t: &TextFragment<S>, at: Point) {
        let Some(node) = t.node else {
            return;
        };
        if t.style.visibility() != Visibility::Visible {
            return;
        }
        let rect = Rect::new(at.x, at.y, size.width, size.height);
        let (transform, inverse) = *self.transforms.last().unwrap();
        match self.query {
            CaretQuery::Position { point, keep } => {
                let Some(inv) = inverse else {
                    return;
                };
                if !self
                    .clips
                    .last()
                    .unwrap()
                    .intersects(&transform.apply_rect(&rect))
                    || !keep(node)
                {
                    return;
                }
                let p = inv.apply(point);
                if self.line_only && (p.y < rect.y || p.y > rect.bottom()) {
                    return;
                }
                let dy = if p.y < rect.y {
                    rect.y - p.y
                } else if p.y > rect.bottom() {
                    p.y - rect.bottom()
                } else {
                    0.0
                };
                let dx = if p.x < rect.x {
                    rect.x - p.x
                } else if p.x > rect.right() {
                    p.x - rect.right()
                } else {
                    0.0
                };
                let better = match self.best {
                    None => true,
                    Some((by, bx, _, _)) => dy < by || (dy == by && dx < bx),
                };
                if better {
                    let processed = cluster_boundary_at(t, p.x - rect.x);
                    self.best = Some((dy, dx, node, t.dom_offset(processed)));
                }
            }
            CaretQuery::Rect { node: want, byte } => {
                if self.done || node != want {
                    return;
                }
                if byte < t.text_range.start || byte > t.text_range.end {
                    return;
                }
                let x = rect.x + t.x_of(t.processed_offset(byte));
                self.rect = Some(transform.apply_rect(&Rect::new(x, rect.y, 1.0, rect.height)));
                // (an offset inside the fragment is final; one at its end may start the next)
                if byte < t.text_range.end {
                    self.done = true;
                }
            }
        }
    }

    fn replaced(&mut self, _f: &Fragment<S>, _b: &BoxFragment<S>, _at: Point) {}

    fn collapsed_borders(&mut self, _segments: &[CollapsedBorderSegment], _at: Point) {}

    fn outline(&mut self, _f: &Fragment<S>, _b: &BoxFragment<S>, _at: Point) {}

    fn push_clip(&mut self, rect: Rect, _radii: Radii) {
        // (in the painted coordinates: a transform may lie between a clip and what it clips)
        let painted = self.transforms.last().unwrap().0.apply_rect(&rect);
        let c = self.clips.last().unwrap().intersect(&painted);
        self.clips.push(c);
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn push_transform(&mut self, m: Matrix) {
        let t = m.then(&self.transforms.last().unwrap().0);
        self.transforms.push((t, t.inverse()));
    }

    fn pop_transform(&mut self) {
        self.transforms.pop();
    }

    fn push_opacity(&mut self, _opacity: f32, _bounds: Rect) {}

    fn pop_opacity(&mut self) {}

    fn wants(&self, overflow: Rect) -> bool {
        let CaretQuery::Position { point, .. } = self.query else {
            return true;
        };
        let Some(inv) = self.transforms.last().unwrap().1 else {
            return true;
        };
        let p = inv.apply(point);
        // (the text inside is at least as far from the point as the overflow rectangle)
        let dy = (overflow.y - p.y).max(p.y - overflow.bottom()).max(0.0);
        if self.line_only {
            return dy == 0.0;
        }
        let dx = (overflow.x - p.x).max(p.x - overflow.right()).max(0.0);
        match self.best {
            None => true,
            Some((by, bx, _, _)) => dy < by || (dy == by && dx < bx),
        }
    }
}

// ----- hit testing -----

/// What [`crate::hit_test`] finds.
#[derive(Clone, Debug, PartialEq)]
pub struct HitTestResult {
    /// The innermost node hit (a text node for text, else an element).
    pub node: NodeId,
    /// The nodes of the hit fragment and its fragment ancestors, innermost first.
    pub chain: Vec<NodeId>,
    /// For a text hit: the byte offset in the text node's (white-space processed) text nearest the
    /// point.
    pub text_offset: Option<u32>,
    /// For a replaced element: the point relative to its content box (through any transforms), where
    /// a nested document's viewport point is.
    pub content_point: Option<Point>,
    /// The point is on the default summary of a `details` element without a `summary` child (the
    /// hit node is then that element).
    pub default_summary: bool,
}

/// Finds the topmost fragment under a point. Fragments come in painting order, so the last one that
/// contains the point is the answer: each is tested as it comes and only the latest hit is kept (no
/// allocation per fragment).
pub(crate) struct HitSink {
    /// The transforms in effect, each with its inverse (identity when not invertible).
    transforms: Vec<(Matrix, Matrix)>,
    /// The clips in effect, each with its radii (CSS Backgrounds 3 §5.3: overflow clips follow the
    /// padding box curve) and the inverse transform of the coordinates it was set in: a clip
    /// applies in the space of the box that sets it (CSS Overflow 3 §3, CSS Transforms 1 §3), so
    /// clips across a transform cannot be intersected as rectangles, and each is tested on its own.
    clips: Vec<(Rect, Radii, Matrix)>,
    /// The `clip-path` paths in effect (CSS Masking 1 §5: what they clip out takes no pointer
    /// events), each with its fill rule and the inverse transform of its coordinates.
    paths: Vec<(Vec<Vec<Point>>, bool, Matrix)>,
    path: Vec<Option<NodeId>>,
    point: Point,
    best: Option<HitTestResult>,
    /// Every hit in painting order, when all are wanted (`elementsFromPoint`).
    all: Option<Vec<HitTestResult>>,
    /// The border box of the last default summary painted (in the coordinates it was painted in).
    summary_rect: Option<Rect>,
}

impl HitSink {
    pub fn new(point: Point) -> HitSink {
        HitSink {
            transforms: vec![(Matrix::IDENTITY, Matrix::IDENTITY)],
            clips: Vec::new(),
            paths: Vec::new(),
            path: Vec::new(),
            point,
            best: None,
            all: None,
            summary_rect: None,
        }
    }

    /// A sink that keeps every fragment under the point ([`HitSink::all_results`]).
    pub fn collecting_all(point: Point) -> HitSink {
        HitSink {
            all: Some(Vec::new()),
            ..HitSink::new(point)
        }
    }

    /// Every fragment under the point, topmost first.
    pub fn all_results(self) -> Vec<HitTestResult> {
        let mut all = self.all.unwrap_or_default();
        all.reverse();
        all
    }

    /// The point in the coordinates of what is being painted.
    fn local_point(&self) -> Point {
        self.transforms.last().unwrap().1.apply(self.point)
    }

    /// Whether `rect` (in the current coordinates) is under the point, inside the clips.
    fn hits(&self, rect: Rect) -> Option<Point> {
        let p = self.local_point();
        if !rect.contains(p) {
            return None;
        }
        let in_clips = self.clips.iter().all(|(r, radii, inverse)| {
            let q = inverse.apply(self.point);
            if radii.is_zero() {
                r.contains(q)
            } else {
                rounded_rect_contains(*r, radii, q)
            }
        });
        let in_paths = self.paths.iter().all(|(contours, even_odd, inverse)| {
            crate::clip::path_contains(contours, *even_odd, inverse.apply(self.point))
        });
        (in_clips && in_paths).then_some(p)
    }

    fn record(&mut self, rect: Rect, text_offset: Option<u32>) {
        self.record_with(rect, text_offset, None);
    }

    fn record_with(&mut self, rect: Rect, text_offset: Option<u32>, content: Option<Point>) {
        let Some(p) = self.hits(rect) else {
            return;
        };
        let chain: Vec<NodeId> = self.path.iter().rev().flatten().copied().collect();
        let Some(&node) = chain.first() else {
            return;
        };
        let hit = HitTestResult {
            node,
            chain,
            text_offset,
            content_point: content.map(|c| Point::new(p.x - c.x, p.y - c.y)),
            default_summary: self.summary_rect.is_some_and(|r| r.contains(p)),
        };
        if let Some(all) = &mut self.all {
            all.push(hit);
        } else {
            self.best = Some(hit);
        }
    }

    /// The topmost fragment containing the point.
    pub fn result(self) -> Option<HitTestResult> {
        self.best
    }
}

impl<S: Style> Sink<S> for HitSink {
    fn decorations(
        &mut self,
        f: &Fragment<S>,
        b: &BoxFragment<S>,
        at: Point,
        canvas: bool,
        _scroll: Option<Point>,
    ) {
        if b.pseudo == Some(PseudoElement::DetailsSummary) {
            self.summary_rect = Some(Rect::new(at.x, at.y, f.size.width, f.size.height));
        }
        if canvas || !b.style.pointer_events() || b.style.visibility() != Visibility::Visible {
            return;
        }
        // (table rows and row groups take no hit of their own: the point is on a cell, or on the
        // table where there is none, as browsers have it)
        if matches!(
            b.style.display(),
            Display::Internal(
                DisplayInternal::TableRow
                    | DisplayInternal::TableRowGroup
                    | DisplayInternal::TableHeaderGroup
                    | DisplayInternal::TableFooterGroup
            )
        ) {
            return;
        }
        self.record(Rect::new(at.x, at.y, f.size.width, f.size.height), None);
    }

    fn text(&mut self, size: Size, t: &TextFragment<S>, at: Point) {
        if !t.style.pointer_events() || t.style.visibility() != Visibility::Visible {
            return;
        }
        let rect = Rect::new(at.x, at.y, size.width, size.height);
        let Some(local) = self.hits(rect) else {
            return;
        };
        // The cluster under the point (ltr runs).
        let offset = t.dom_offset(cluster_boundary_at(t, local.x - at.x));
        self.path.push(t.node);
        self.record(rect, Some(offset));
        self.path.pop();
    }

    fn replaced(&mut self, f: &Fragment<S>, b: &BoxFragment<S>, at: Point) {
        // (an inert replaced element's content, an SVG document fragment or a nested document,
        // is inert with it: HTML §6.3; `pointer-events` alone does not decide, since an SVG
        // element inside may set it back)
        if b.style.inert() {
            return;
        }
        let content = match &b.content {
            BoxContent::Replaced { clip, .. } => Some(Point::new(at.x + clip.x, at.y + clip.y)),
            _ => None,
        };
        self.record_with(
            Rect::new(at.x, at.y, f.size.width, f.size.height),
            None,
            content,
        );
    }

    fn collapsed_borders(&mut self, _segments: &[CollapsedBorderSegment], _at: Point) {}

    fn outline(&mut self, _f: &Fragment<S>, _b: &BoxFragment<S>, _at: Point) {}

    fn push_clip(&mut self, rect: Rect, radii: Radii) {
        let inverse = self.transforms.last().unwrap().1;
        self.clips.push((rect, radii, inverse));
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn push_clip_path(&mut self, contours: &[Vec<Point>], even_odd: bool) {
        let inverse = self.transforms.last().unwrap().1;
        self.paths.push((contours.to_vec(), even_odd, inverse));
    }

    fn pop_clip_path(&mut self) {
        self.paths.pop();
    }

    fn push_transform(&mut self, m: Matrix) {
        let t = m.then(&self.transforms.last().unwrap().0);
        self.transforms
            .push((t, t.inverse().unwrap_or(Matrix::IDENTITY)));
    }

    fn pop_transform(&mut self) {
        self.transforms.pop();
    }

    fn push_opacity(&mut self, _opacity: f32, _bounds: Rect) {}

    fn pop_opacity(&mut self) {}

    fn enter(&mut self, node: Option<NodeId>) {
        self.path.push(node);
    }

    fn leave(&mut self) {
        self.path.pop();
    }

    fn wants(&self, overflow: Rect) -> bool {
        overflow.contains(self.local_point())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(offset: f32, color: Color) -> GradientStop {
        GradientStop { offset, color }
    }

    #[test]
    fn border_image_tiles_per_repeat() {
        use BorderImageRepeat::*;
        // stretch: one tile over the whole edge
        assert_eq!(
            border_image_tiles(Stretch, 10.0, 100.0, 30.0),
            (vec![10.0], 100.0)
        );
        // round: 100 / 30 rounds to 3 tiles of 33.33
        let (xs, t) = border_image_tiles(Round, 0.0, 100.0, 30.0);
        assert_eq!(xs.len(), 3);
        assert!((t - 100.0 / 3.0).abs() < 1e-4);
        // space: 3 whole tiles, the 10 px left over in four gaps of 2.5
        assert_eq!(
            border_image_tiles(Space, 0.0, 100.0, 30.0),
            (vec![2.5, 35.0, 67.5], 30.0)
        );
        // repeat: centered, the middle tile's center on the edge's middle
        let (xs, _) = border_image_tiles(Repeat, 0.0, 100.0, 30.0);
        assert_eq!(xs, vec![-25.0, 5.0, 35.0, 65.0, 95.0]);
    }

    #[test]
    fn border_images_slice_nine_ways() {
        // a 30x30 image sliced 10 all round into a 100x100 border box with 20 px borders: four
        // corners, four stretched edges, no middle without `fill`
        let bi = BorderImage {
            source: Image::raster(snowlark_paint::ImageId(7), 30.0, 30.0),
            slice: [NumberOrPercent::Number(10.0); 4],
            fill: false,
            width: [BorderImageLength::Number(1.0); 4],
            outset: [BorderImageLength::Number(0.0); 4],
            repeat: (BorderImageRepeat::Stretch, BorderImageRepeat::Stretch),
        };
        let mut b = DisplayListBuilder::new();
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        paint_border_image(
            &mut b,
            &bi,
            r,
            &Sides::all(20.0),
            ImageRendering::Smooth,
            1.0,
        );
        let list = b.build();
        let images = list
            .items
            .iter()
            .filter(|i| matches!(i, snowlark_paint::Item::Image { .. }))
            .count();
        assert_eq!(images, 8);
        // the top edge: the slice 10 px wide scaled to 60 x 20, so the image is 180 x 60
        let scales: Vec<(f32, f32)> = list
            .items
            .iter()
            .filter_map(|i| match i {
                snowlark_paint::Item::PushTransform { matrix } => Some((matrix.m11, matrix.m22)),
                _ => None,
            })
            .collect();
        assert!(scales.contains(&(6.0, 2.0)), "{scales:?}");
        assert!(scales.contains(&(2.0, 2.0)));
        // with `fill` the middle too
        let mut b = DisplayListBuilder::new();
        let bi = BorderImage { fill: true, ..bi };
        paint_border_image(
            &mut b,
            &bi,
            r,
            &Sides::all(20.0),
            ImageRendering::Smooth,
            1.0,
        );
        let images = b
            .build()
            .items
            .iter()
            .filter(|i| matches!(i, snowlark_paint::Item::Image { .. }))
            .count();
        assert_eq!(images, 9);
    }

    #[test]
    fn corner_directions_depend_on_the_box() {
        // CSS Images 3 §3.1.1: `to top right` on a square box is 45 degrees; on a box twice as
        // wide as tall the line is perpendicular to the other diagonal
        assert!((corner_angle(true, false, 100.0, 100.0) - 45.0).abs() < 1e-4);
        let a = corner_angle(true, false, 200.0, 100.0);
        assert!((a - 26.565).abs() < 1e-3, "{a}");
        assert!((corner_angle(true, true, 200.0, 100.0) - (180.0 - a)).abs() < 1e-4);
        assert!((corner_angle(false, true, 200.0, 100.0) - (180.0 + a)).abs() < 1e-4);
        assert!((corner_angle(false, false, 200.0, 100.0) - (360.0 - a)).abs() < 1e-4);
    }

    #[test]
    fn colors_blend_premultiplied() {
        // halfway from opaque red to transparent: red at half alpha, not a darkened red
        let c = mix(Color::rgb(255, 0, 0), Color::TRANSPARENT, 0.5);
        assert_eq!(c, Color::rgba(255, 0, 0, 128));
    }

    #[test]
    fn transition_hints_bend_the_blend() {
        let (a, b) = (Color::rgb(0, 0, 0), Color::rgb(255, 255, 255));
        let hint = ColorStop {
            color: Color::TRANSPARENT,
            position: Some(LengthPercentage::px(25.0)),
            hint: true,
        };
        let resolved = resolve_stops(
            &[ColorStop::new(a, None), hint, ColorStop::new(b, None)],
            100.0,
        );
        assert_eq!(resolved[1].offset, 0.25);
        let stops = interpolated_stops(&resolved, &[]);
        // the halfway color sits at the hint
        let at_hint = stops
            .iter()
            .find(|s| (s.offset - 0.25).abs() < 1e-6)
            .unwrap();
        assert!((at_hint.color.r as i32 - 128).abs() <= 1, "{at_hint:?}");
        assert_eq!(stops.first().unwrap().color, a);
        assert_eq!(stops.last().unwrap().color, b);
        // a hint at the first stop is a hard transition there
        let at_start = ColorStop {
            position: Some(LengthPercentage::px(0.0)),
            ..hint
        };
        let stops = interpolated_stops(
            &resolve_stops(
                &[ColorStop::new(a, None), at_start, ColorStop::new(b, None)],
                100.0,
            ),
            &[],
        );
        assert_eq!(
            stops
                .iter()
                .map(|s| (s.offset, s.color))
                .collect::<Vec<_>>(),
            vec![(0.0, a), (0.0, b), (1.0, b)]
        );
    }

    #[test]
    fn transparent_stops_take_their_neighbours_channels() {
        let blue = Color::rgb(0, 0, 255);
        let stops = transparent_fixup(vec![stop(0.0, blue), stop(1.0, Color::TRANSPARENT)]);
        assert_eq!(stops[1].color, Color::rgba(0, 0, 255, 0));
        // between two colors: one transparent stop of each at its offset
        let red = Color::rgb(255, 0, 0);
        let stops = transparent_fixup(vec![
            stop(0.0, red),
            stop(0.5, Color::TRANSPARENT),
            stop(1.0, blue),
        ]);
        assert_eq!(
            stops
                .iter()
                .map(|s| (s.offset, s.color))
                .collect::<Vec<_>>(),
            vec![
                (0.0, red),
                (0.5, Color::rgba(255, 0, 0, 0)),
                (0.5, Color::rgba(0, 0, 255, 0)),
                (1.0, blue)
            ]
        );
    }

    #[test]
    fn ramps_replace_the_straight_blend() {
        let (a, b) = (Color::rgb(255, 0, 0), Color::rgb(0, 0, 255));
        let mut ramp = [Color::rgb(0, 255, 0); RAMP_STEPS + 1];
        ramp[0] = a;
        ramp[RAMP_STEPS] = b;
        let stops = interpolated_stops(
            &resolve_stops(&[ColorStop::new(a, None), ColorStop::new(b, None)], 100.0),
            &[ramp],
        );
        assert_eq!(stops.len(), RAMP_STEPS + 1);
        assert_eq!(stops[RAMP_STEPS / 2].color, Color::rgb(0, 255, 0));
    }

    #[test]
    fn stop_fixup_keeps_a_negative_first_stop() {
        // CSS Images 3 §3.5.1: only a position below an earlier one is raised.
        let px = |v: f32| ColorStop::new(Color::BLACK, Some(LengthPercentage::px(v)));
        let stops = resolve_stops(&[px(-50.0), px(100.0), px(20.0)], 100.0);
        let offsets: Vec<f32> = stops.iter().map(|s| s.offset).collect();
        assert_eq!(offsets, vec![-0.5, 1.0, 1.0]);
    }

    #[test]
    fn infinite_lengths_are_clamped() {
        let inf = ColorStop::new(Color::BLACK, Some(LengthPercentage::px(f32::INFINITY)));
        let stops = resolve_stops(&[inf], 100.0);
        assert!(stops[0].offset.is_finite() && stops[0].offset > 1.0e5);
        assert_eq!(finite_length(f32::NAN), 0.0);
    }

    #[test]
    fn stops_are_fitted_to_the_brush() {
        let (a, b) = (Color::rgb(255, 0, 0), Color::rgb(0, 0, 255));
        // a clamped linear gradient past both ends of its line: the line grows to the stops
        let (from, scale, s) = fit_stops(vec![stop(-0.5, a), stop(1.5, b)], false, false);
        assert_eq!((from, scale), (-0.5, 2.0));
        assert_eq!((s[0].offset, s[1].offset), (0.0, 1.0));
        // radial: nothing before the center, which takes the color there
        let (_, scale, s) = fit_stops(vec![stop(-1.0, a), stop(1.0, b)], false, true);
        assert_eq!(scale, 1.0);
        assert_eq!(s[0].offset, 0.0);
        assert_eq!(s[0].color, Color::rgb(128, 0, 128));
        // repeating from 0.25 to 0.75: a period of 0.5 whose pattern starts half way in
        let (_, scale, s) = fit_stops(
            vec![stop(0.25, a), stop(0.5, a), stop(0.5, b), stop(0.75, b)],
            true,
            false,
        );
        assert_eq!(scale, 0.5);
        let got: Vec<(f32, Color)> = s.iter().map(|s| (s.offset, s.color)).collect();
        assert_eq!(
            got,
            // (brush 0..0.5 is the pattern's second half, blue; 0.5..1 its first, red)
            vec![(0.0, a), (0.0, a), (0.0, b), (0.5, b), (0.5, a), (1.0, a)],
            "{got:?}"
        );
        // no period: the average color
        let (_, _, s) = fit_stops(vec![stop(0.3, a), stop(0.3, b)], true, false);
        assert_eq!(s[0].color, s[1].color);
    }
}
