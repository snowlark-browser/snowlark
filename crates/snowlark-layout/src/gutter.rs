// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The space a scroll container's custom scroll bars take (CSS Overflow 3 §3: a scroll bar is
//! inserted between the inner border edge and the outer padding edge, and the space it takes is
//! subtracted from the content box). The automatic bars here are overlay bars, which take none;
//! custom bars (`::-webkit-scrollbar`, [`CustomScrollbar`]) are never overlay bars, so they do.
//!
//! A bar exists on an axis whose `overflow` is `scroll`, and on one whose `overflow` is `auto`
//! when the box overflows there. Whether it does is only known after the box is laid out, so a
//! box remembers which `auto` bars it showed (`flags::VERTICAL_BAR`, `flags::HORIZONTAL_BAR`):
//! the next layout starts from them, and a layout that finds them wrong lays the box out again
//! with the bars it found ([`crate::layout::Cx::layout`]).
//!
//! The vertical bar is at the right, at the left for `direction: rtl`; the horizontal bar at the
//! bottom. Boxes in a vertical writing mode take no space for their bars here.

use snowlark_paint::Sides;

use crate::boxes::*;
use crate::fragment::Fragment;
use crate::logical::Flip;
use crate::style::*;

/// The custom scroll bars of box `b` when they take layout space: a block, flex or grid container
/// (or table cell or caption) that is a scroll container of its own (not the viewport's: the
/// root's and a propagating body's `overflow` is the viewport's, §3.3), in a horizontal writing
/// mode.
pub(crate) fn custom_bars<S: Style>(b: &LayoutBox<S>) -> Option<CustomScrollbar> {
    let kind_applies = matches!(
        b.kind,
        BoxKind::Block | BoxKind::Flex | BoxKind::Grid | BoxKind::TableCell | BoxKind::TableCaption
    );
    let s = &b.style;
    if !kind_applies
        || b.has(flags::ROOT | flags::VIEWPORT_OVERFLOW)
        || s.view() != Flip::IDENTITY
        || s.writing_mode() != WritingMode::HorizontalTb
        || !(s.overflow_x().is_scroll_container() || s.overflow_y().is_scroll_container())
    {
        return None;
    }
    CustomScrollbar::of(s)
}

/// Which bars of box `b` exist when its `auto` axes show `vertical` and `horizontal` bars:
/// (vertical, horizontal).
fn exist<S: Style>(b: &LayoutBox<S>, vertical: bool, horizontal: bool) -> (bool, bool) {
    let on = |o: Overflow, auto: bool| match o {
        Overflow::Scroll => true,
        Overflow::Auto => auto,
        _ => false,
    };
    (
        on(b.style.overflow_y(), vertical),
        on(b.style.overflow_x(), horizontal),
    )
}

/// The space box `b`'s custom scroll bars take now, by side (zero for a box without them).
pub(crate) fn gutter<S: Style>(b: &LayoutBox<S>) -> Sides {
    gutter_with(b, b.has(flags::VERTICAL_BAR), b.has(flags::HORIZONTAL_BAR))
}

/// [`gutter`] with the `auto` axes showing bars as `vertical` and `horizontal` say.
fn gutter_with<S: Style>(b: &LayoutBox<S>, vertical: bool, horizontal: bool) -> Sides {
    let Some(c) = custom_bars(b) else {
        return Sides::default();
    };
    let (v, h) = exist(b, vertical, horizontal);
    let w = if v { c.vertical_thickness() } else { 0.0 };
    let bottom = if h { c.horizontal_thickness() } else { 0.0 };
    if b.style.direction() == Direction::Rtl {
        Sides::new(0.0, 0.0, bottom, w)
    } else {
        Sides::new(0.0, w, bottom, 0.0)
    }
}

/// Whether box `b`, laid out as fragment `f` with its present bars, needs its `auto` bars
/// otherwise: the bars an `auto` axis needs for what `f` overflows there (the `auto` bars it
/// should show: vertical, horizontal), when they differ from the ones it has.
pub(crate) fn changed_bars<S: Style>(b: &LayoutBox<S>, f: &Fragment<S>) -> Option<(bool, bool)> {
    let s = &b.style;
    let auto_y = s.overflow_y() == Overflow::Auto;
    let auto_x = s.overflow_x() == Overflow::Auto;
    if !(auto_x || auto_y) || custom_bars(b).is_none() {
        return None;
    }
    let had = (b.has(flags::VERTICAL_BAR), b.has(flags::HORIZONTAL_BAR));
    let port = f.padding_rect();
    let so = f.scrollable_overflow();
    // (less than a pixel past the scrollport is not overflow: the scroll range is in whole pixels)
    let over = |a: f32, b: f32| a - b >= 1.0;
    let mut v = auto_y && (over(so.bottom(), port.bottom()) || over(port.y, so.y));
    let mut h = auto_x && (over(so.right(), port.right()) || over(port.x, so.x));
    // Bars on both `auto` axes take space from each other: when what the box holds fits in it
    // without the bars it has, it shows none.
    if v && h && auto_x && auto_y {
        let g = f.as_box().map_or(Sides::default(), |b| b.scrollbar);
        let fits_x = !over(so.right(), port.right() + g.right) && !over(port.x - g.left, so.x);
        let fits_y = !over(so.bottom(), port.bottom() + g.bottom);
        if fits_x && fits_y {
            v = false;
            h = false;
        }
    }
    let now = (v || !auto_y && had.0, h || !auto_x && had.1);
    (now != had).then_some(now)
}

/// Records which `auto` bars box `b` shows.
pub(crate) fn set_bars<S: Style>(b: &mut LayoutBox<S>, (vertical, horizontal): (bool, bool)) {
    b.set(flags::VERTICAL_BAR, vertical);
    b.set(flags::HORIZONTAL_BAR, horizontal);
}
