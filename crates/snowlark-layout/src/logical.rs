// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The abstract box model of CSS Writing Modes 4 §6: block and inline axes, line-relative
//! directions, and the mapping of a box's coordinates between writing modes.
//!
//! Layout runs in a *flow frame*: x along the inline axis from line-left to line-right, y along the
//! block axis from block-start to block-end, as `horizontal-tb` is physically. Every algorithm (block,
//! inline, floats, tables, flex and grid through taffy, absolute positioning) is written once for that
//! frame. A box's style is read through a [`Flip`] *view*: the frame's `top`, `right`, `bottom` and
//! `left` are the box's block-start, line-right, block-end and line-left sides, its `width` the inline
//! size, its `height` the block size. In `horizontal-tb` the view is the identity and layout reads the
//! physical values unchanged.
//!
//! Where a box's writing mode differs from its parent's (§3.2: it establishes an independent
//! formatting context), its layout runs in its own frame and its fragments are mapped into the
//! parent's frame by the [`Flip`] between the two ([`crate::layout`]); the initial containing block
//! maps the root's frame to physical coordinates. Line-relative values (`text-align`, `float` and
//! `clear` with `left`/`right`, §7.1) need no mapping: line-left is the frame's left.

use snowlark_paint::{Matrix, Point, Rect, Sides, Size};

use crate::style::{Corner, Direction, Side, WritingMode};

/// A logical side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalSide {
    BlockStart,
    BlockEnd,
    InlineStart,
    InlineEnd,
}

/// The physical side a logical side maps to.
pub fn physical(side: LogicalSide, mode: WritingMode, direction: Direction) -> Side {
    let rtl = direction == Direction::Rtl;
    let frame = match side {
        LogicalSide::BlockStart => Side::Top,
        LogicalSide::BlockEnd => Side::Bottom,
        LogicalSide::InlineStart if rtl => Side::Right,
        LogicalSide::InlineStart => Side::Left,
        LogicalSide::InlineEnd if rtl => Side::Left,
        LogicalSide::InlineEnd => Side::Right,
    };
    Flip::of(mode).side(frame)
}

/// Whether the inline axis is horizontal.
pub fn is_horizontal(mode: WritingMode) -> bool {
    mode == WritingMode::HorizontalTb
}

/// One of the eight symmetries of a rectangle, as a signed permutation matrix: how coordinates of
/// a box in one frame map to another frame (the translation that keeps the box in place follows from
/// the box's size). `x' = xx·x + xy·y`, `y' = yx·x + yy·y` (plus the box's extent on every axis that
/// is mirrored).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Flip {
    xx: i8,
    xy: i8,
    yx: i8,
    yy: i8,
}

impl Default for Flip {
    fn default() -> Flip {
        Flip::IDENTITY
    }
}

impl Flip {
    pub const IDENTITY: Flip = Flip {
        xx: 1,
        xy: 0,
        yx: 0,
        yy: 1,
    };
    /// Mirrors the y axis (a line's internals in `vertical-lr`, whose line-over is block-end).
    pub const MIRROR_Y: Flip = Flip {
        xx: 1,
        xy: 0,
        yx: 0,
        yy: -1,
    };
    /// `vertical-rl` and `sideways-rl`: inline down, block leftward (a quarter turn clockwise).
    pub const VERTICAL_RL: Flip = Flip {
        xx: 0,
        xy: -1,
        yx: 1,
        yy: 0,
    };
    /// `vertical-lr`: inline down, block rightward (a transposition).
    pub const VERTICAL_LR: Flip = Flip {
        xx: 0,
        xy: 1,
        yx: 1,
        yy: 0,
    };
    /// `sideways-lr`: inline up, block rightward (a quarter turn counterclockwise).
    pub const SIDEWAYS_LR: Flip = Flip {
        xx: 0,
        xy: 1,
        yx: -1,
        yy: 0,
    };

    /// The map from `mode`'s flow frame to physical coordinates (§6.2, §6.3).
    pub const fn of(mode: WritingMode) -> Flip {
        match mode {
            WritingMode::HorizontalTb => Flip::IDENTITY,
            WritingMode::VerticalRl | WritingMode::SidewaysRl => Flip::VERTICAL_RL,
            WritingMode::VerticalLr => Flip::VERTICAL_LR,
            WritingMode::SidewaysLr => Flip::SIDEWAYS_LR,
        }
    }

    #[inline]
    pub fn is_identity(self) -> bool {
        self == Flip::IDENTITY
    }

    /// Whether the axes trade places (an orthogonal frame).
    #[inline]
    pub fn swaps(self) -> bool {
        self.xx == 0
    }

    /// Whether the map turns the plane over (a mirror image).
    pub fn mirrors(self) -> bool {
        self.xx * self.yy - self.xy * self.yx < 0
    }

    /// Whether the source y axis runs against the target axis it maps to.
    pub fn reverses_y(self) -> bool {
        self.xy + self.yy < 0
    }

    /// Whether the source x axis runs against the target axis it maps to.
    pub fn reverses_x(self) -> bool {
        self.xx + self.yx < 0
    }

    /// `self` followed by `next`.
    pub fn then(self, next: Flip) -> Flip {
        let (a, b) = (self, next);
        Flip {
            xx: b.xx * a.xx + b.xy * a.yx,
            xy: b.xx * a.xy + b.xy * a.yy,
            yx: b.yx * a.xx + b.yy * a.yx,
            yy: b.yx * a.xy + b.yy * a.yy,
        }
    }

    pub fn inverse(self) -> Flip {
        // An orthogonal matrix: the inverse is the transpose.
        Flip {
            xx: self.xx,
            xy: self.yx,
            yx: self.xy,
            yy: self.yy,
        }
    }

    /// The map from frame `from` to frame `to`, both given as their maps to physical coordinates.
    pub fn between(from: Flip, to: Flip) -> Flip {
        from.then(to.inverse())
    }

    fn vec(self, x: i8, y: i8) -> (i8, i8) {
        (self.xx * x + self.xy * y, self.yx * x + self.yy * y)
    }

    /// The side `side` of the source frame becomes.
    #[inline]
    pub fn side(self, side: Side) -> Side {
        if self.is_identity() {
            return side;
        }
        self.side_mapped(side)
    }

    fn side_mapped(self, side: Side) -> Side {
        let (x, y) = match side {
            Side::Top => (0, -1),
            Side::Right => (1, 0),
            Side::Bottom => (0, 1),
            Side::Left => (-1, 0),
        };
        match self.vec(x, y) {
            (0, -1) => Side::Top,
            (1, 0) => Side::Right,
            (0, 1) => Side::Bottom,
            _ => Side::Left,
        }
    }

    /// The corner `corner` of the source frame becomes.
    pub fn corner(self, corner: Corner) -> Corner {
        let (x, y) = match corner {
            Corner::TopLeft => (-1, -1),
            Corner::TopRight => (1, -1),
            Corner::BottomRight => (1, 1),
            Corner::BottomLeft => (-1, 1),
        };
        match self.vec(x, y) {
            (-1, -1) => Corner::TopLeft,
            (1, -1) => Corner::TopRight,
            (1, 1) => Corner::BottomRight,
            _ => Corner::BottomLeft,
        }
    }

    /// A size in the target frame.
    #[inline]
    pub fn size(self, s: Size) -> Size {
        if self.swaps() {
            Size::new(s.height, s.width)
        } else {
            s
        }
    }

    /// Per-side values (border widths, margins) in the target frame.
    pub fn sides(self, s: Sides) -> Sides {
        if self.is_identity() {
            return s;
        }
        let mut out = Sides::default();
        for side in Side::ALL {
            set(&mut out, self.side(side), get(&s, side));
        }
        out
    }

    /// A rectangle in a box of size `within` (source frame), in the box's target-frame coordinates.
    pub fn rect(self, r: Rect, within: Size) -> Rect {
        if self.is_identity() {
            return r;
        }
        let target = self.size(within);
        let (x, y, w, h) = if self.swaps() {
            (r.y, r.x, r.height, r.width)
        } else {
            (r.x, r.y, r.width, r.height)
        };
        // Which source axis feeds each target axis, and with which sign.
        let x_sign = if self.swaps() { self.xy } else { self.xx };
        let y_sign = if self.swaps() { self.yx } else { self.yy };
        let x = if x_sign < 0 { target.width - x - w } else { x };
        let y = if y_sign < 0 { target.height - y - h } else { y };
        Rect::new(x, y, w, h)
    }

    /// A point in a box of size `within`, in target coordinates.
    pub fn point(self, p: Point, within: Size) -> Point {
        let r = self.rect(Rect::new(p.x, p.y, 0.0, 0.0), within);
        Point::new(r.x, r.y)
    }

    /// The affine map of a box of size `within` from the source frame to the target frame.
    pub fn matrix(self, within: Size) -> Matrix {
        let target = self.size(within);
        let dx = if self.xx < 0 || self.xy < 0 {
            target.width
        } else {
            0.0
        };
        let dy = if self.yx < 0 || self.yy < 0 {
            target.height
        } else {
            0.0
        };
        // (row-vector convention: x' = x·m11 + y·m21 + dx)
        Matrix::new(
            self.xx as f32,
            self.yx as f32,
            self.xy as f32,
            self.yy as f32,
            dx,
            dy,
        )
    }
}

fn get(s: &Sides, side: Side) -> f32 {
    match side {
        Side::Top => s.top,
        Side::Right => s.right,
        Side::Bottom => s.bottom,
        Side::Left => s.left,
    }
}

fn set(s: &mut Sides, side: Side, v: f32) {
    match side {
        Side::Top => s.top = v,
        Side::Right => s.right = v,
        Side::Bottom => s.bottom = v,
        Side::Left => s.left = v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sides_map_per_writing_mode() {
        use LogicalSide::*;
        assert_eq!(
            physical(InlineStart, WritingMode::HorizontalTb, Direction::Rtl),
            Side::Right
        );
        assert_eq!(
            physical(BlockStart, WritingMode::VerticalRl, Direction::Ltr),
            Side::Right
        );
        assert_eq!(
            physical(InlineEnd, WritingMode::VerticalLr, Direction::Ltr),
            Side::Bottom
        );
        assert_eq!(
            physical(InlineStart, WritingMode::SidewaysLr, Direction::Ltr),
            Side::Bottom
        );
        assert_eq!(
            physical(BlockStart, WritingMode::SidewaysLr, Direction::Ltr),
            Side::Left
        );
        assert_eq!(
            physical(InlineStart, WritingMode::VerticalRl, Direction::Rtl),
            Side::Bottom
        );
    }

    #[test]
    fn rects_map_into_the_box() {
        let within = Size::new(100.0, 40.0); // frame: 100 inline, 40 block
        let r = Rect::new(10.0, 5.0, 20.0, 8.0);
        // vertical-rl: block-start is the right edge of a 40 wide box.
        assert_eq!(
            Flip::VERTICAL_RL.rect(r, within),
            Rect::new(40.0 - 5.0 - 8.0, 10.0, 8.0, 20.0)
        );
        assert_eq!(
            Flip::VERTICAL_LR.rect(r, within),
            Rect::new(5.0, 10.0, 8.0, 20.0)
        );
        // sideways-lr: inline goes up from the bottom of a 100 tall box.
        assert_eq!(
            Flip::SIDEWAYS_LR.rect(r, within),
            Rect::new(5.0, 100.0 - 10.0 - 20.0, 8.0, 20.0)
        );
        for f in [Flip::VERTICAL_RL, Flip::VERTICAL_LR, Flip::SIDEWAYS_LR] {
            assert_eq!(f.then(f.inverse()), Flip::IDENTITY);
            let back = f.inverse().rect(f.rect(r, within), f.size(within));
            assert_eq!(back, r);
            let m = f.matrix(within);
            let a = m.apply(Point::new(r.x, r.y));
            let b = m.apply(Point::new(r.x + r.width, r.y + r.height));
            let mapped = f.rect(r, within);
            assert_eq!(a.x.min(b.x), mapped.x);
            assert_eq!(a.y.min(b.y), mapped.y);
        }
    }

    #[test]
    fn between_frames() {
        // vertical-lr content in a vertical-rl parent: the same axes, block reversed.
        let m = Flip::between(Flip::VERTICAL_LR, Flip::VERTICAL_RL);
        assert!(!m.swaps());
        assert_eq!(m, Flip::MIRROR_Y);
        assert_eq!(m.side(Side::Top), Side::Bottom);
        assert_eq!(m.side(Side::Left), Side::Left);
        assert!(m.reverses_y() && !m.reverses_x());
        // vertical-lr's lines mirrored are vertical-rl's rotation.
        assert_eq!(Flip::MIRROR_Y.then(Flip::VERTICAL_LR), Flip::VERTICAL_RL);
        assert!(Flip::VERTICAL_LR.mirrors());
        assert!(!Flip::VERTICAL_RL.mirrors());
        assert!(Flip::VERTICAL_RL.reverses_y() && !Flip::VERTICAL_RL.reverses_x());
        assert!(Flip::SIDEWAYS_LR.reverses_x() && !Flip::SIDEWAYS_LR.reverses_y());
        assert_eq!(Flip::VERTICAL_RL.corner(Corner::TopLeft), Corner::TopRight);
    }
}
