// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Plain geometry and color types shared by the display list, text and the rasterizer. Coordinates are
//! CSS pixels (f32) unless a function says otherwise.

use serde::{Deserialize, Serialize};

/// A point (or a 2D vector).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Point {
        Point { x, y }
    }
}

/// A width and height (also an elliptical corner radius: horizontal, vertical).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Size = Size {
        width: 0.0,
        height: 0.0,
    };

    pub const fn new(width: f32, height: f32) -> Size {
        Size { width, height }
    }
}

/// An axis-aligned rectangle: origin and size. A rectangle with a non-positive width or height is empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };

    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    pub fn from_ltrb(left: f32, top: f32, right: f32, bottom: f32) -> Rect {
        Rect::new(left, top, right - left, bottom - top)
    }

    /// A rectangle so large that it never clips anything.
    pub const fn infinite() -> Rect {
        Rect::new(-1.0e9, -1.0e9, 2.0e9, 2.0e9)
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn is_empty(&self) -> bool {
        // (NaN sizes count as empty too)
        !(self.width > 0.0 && self.height > 0.0)
    }

    pub fn inflate(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(
            self.x - dx,
            self.y - dy,
            self.width + 2.0 * dx,
            self.height + 2.0 * dy,
        )
    }

    pub fn translate(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.width, self.height)
    }

    /// The overlap of two rectangles; empty (zero size) when they don't overlap.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= left || bottom <= top {
            Rect::new(left, top, 0.0, 0.0)
        } else {
            Rect::from_ltrb(left, top, right, bottom)
        }
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        !self.intersect(other).is_empty()
    }

    /// The smallest rectangle containing both (an empty rectangle contributes nothing).
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        Rect::from_ltrb(
            self.x.min(other.x),
            self.y.min(other.y),
            self.right().max(other.right()),
            self.bottom().max(other.bottom()),
        )
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.y >= self.y && p.x < self.right() && p.y < self.bottom()
    }

    /// Shrinks by per-side insets (a border box to its padding box); never below zero size.
    pub fn deflate(&self, sides: &Sides) -> Rect {
        Rect::new(
            self.x + sides.left,
            self.y + sides.top,
            (self.width - sides.left - sides.right).max(0.0),
            (self.height - sides.top - sides.bottom).max(0.0),
        )
    }
}

/// Per-side lengths in CSS order: top, right, bottom, left.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sides {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Sides {
    pub const fn new(top: f32, right: f32, bottom: f32, left: f32) -> Sides {
        Sides {
            top,
            right,
            bottom,
            left,
        }
    }

    pub const fn all(v: f32) -> Sides {
        Sides::new(v, v, v, v)
    }

    pub fn is_zero(&self) -> bool {
        self.top == 0.0 && self.right == 0.0 && self.bottom == 0.0 && self.left == 0.0
    }

    /// The sides as an array in CSS order (top, right, bottom, left).
    pub fn to_array(&self) -> [f32; 4] {
        [self.top, self.right, self.bottom, self.left]
    }
}

/// Corner radii, each elliptical (horizontal, vertical), as CSS `border-radius` computes them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Radii {
    pub top_left: Size,
    pub top_right: Size,
    pub bottom_right: Size,
    pub bottom_left: Size,
}

impl Radii {
    pub const ZERO: Radii = Radii {
        top_left: Size::ZERO,
        top_right: Size::ZERO,
        bottom_right: Size::ZERO,
        bottom_left: Size::ZERO,
    };

    /// The same circular radius on every corner.
    pub const fn uniform(r: f32) -> Radii {
        let s = Size::new(r, r);
        Radii {
            top_left: s,
            top_right: s,
            bottom_right: s,
            bottom_left: s,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.corners()
            .iter()
            .all(|c| c.width <= 0.0 || c.height <= 0.0)
    }

    /// Whether all four corners are the same ellipse (Direct2D draws those without a path).
    pub fn is_uniform(&self) -> bool {
        let c = self.corners();
        c.iter().all(|r| *r == c[0])
    }

    /// The corners clockwise from the top left.
    pub fn corners(&self) -> [Size; 4] {
        [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ]
    }

    /// Radii that fit `rect`: when adjacent radii add up to more than a side, all are scaled down by the
    /// same factor (CSS Backgrounds 3, "corner overlap").
    pub fn fitted(&self, rect: &Rect) -> Radii {
        let mut f: f32 = 1.0;
        let mut check = |sum: f32, side: f32| {
            if sum > 0.0 && sum > side {
                f = f.min(side.max(0.0) / sum);
            }
        };
        check(self.top_left.width + self.top_right.width, rect.width);
        check(self.bottom_left.width + self.bottom_right.width, rect.width);
        check(self.top_left.height + self.bottom_left.height, rect.height);
        check(
            self.top_right.height + self.bottom_right.height,
            rect.height,
        );
        let s = |c: Size| Size::new((c.width * f).max(0.0), (c.height * f).max(0.0));
        Radii {
            top_left: s(self.top_left),
            top_right: s(self.top_right),
            bottom_right: s(self.bottom_right),
            bottom_left: s(self.bottom_left),
        }
    }

    /// The radii of a shape `sides` inside (a padding edge from a border edge): each radius shrinks by
    /// the adjacent side widths, never below zero.
    pub fn inner(&self, sides: &Sides) -> Radii {
        let s =
            |c: Size, h: f32, v: f32| Size::new((c.width - h).max(0.0), (c.height - v).max(0.0));
        Radii {
            top_left: s(self.top_left, sides.left, sides.top),
            top_right: s(self.top_right, sides.right, sides.top),
            bottom_right: s(self.bottom_right, sides.right, sides.bottom),
            bottom_left: s(self.bottom_left, sides.left, sides.bottom),
        }
    }

    /// The radii of a box's shadow shape grown by `spread` (negative shrinks) from a box of
    /// `size`, CSS Backgrounds 3 §7.1.1: a non-zero radius `r` grows (shrinks) by the spread `s`,
    /// except where it is smaller than the spread and the corner's radii cover less than the
    /// box (`coverage`, the larger of twice the radius over the box's side, below 1): there it
    /// changes by `s × (1 − (1 − r/s)³ × (1 − coverage³))`, so small radii stay sharp. A zero
    /// radius stays zero.
    pub fn outset(&self, spread: f32, size: Size) -> Radii {
        let a = spread.abs();
        let g = |c: Size| {
            if c.width <= 0.0 || c.height <= 0.0 || spread == 0.0 {
                return c;
            }
            let coverage = (2.0 * c.width / size.width)
                .min(2.0 * c.height / size.height)
                .max(0.0);
            let coverage = if coverage.is_finite() { coverage } else { 0.0 };
            let one = |r: f32| {
                let delta = if r > a || coverage > 1.0 {
                    a
                } else {
                    a * (1.0 - (1.0 - r / a).powi(3) * (1.0 - coverage.powi(3)))
                };
                (r + delta * spread.signum()).max(0.0)
            };
            Size::new(one(c.width), one(c.height))
        };
        Radii {
            top_left: g(self.top_left),
            top_right: g(self.top_right),
            bottom_right: g(self.bottom_right),
            bottom_left: g(self.bottom_left),
        }
    }
}

/// A non-premultiplied sRGB color with alpha, 8 bits per channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    /// From `0xRRGGBB`.
    pub const fn hex(v: u32) -> Color {
        Color::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    pub fn is_transparent(&self) -> bool {
        self.a == 0
    }

    pub fn is_opaque(&self) -> bool {
        self.a == 255
    }

    pub fn with_alpha(self, a: u8) -> Color {
        Color { a, ..self }
    }

    /// Each channel scaled by `f` (below 1 darkens, above 1 lightens), alpha unchanged. Used for the
    /// `inset`/`outset`/`groove`/`ridge` border shades.
    pub fn shade(self, f: f32) -> Color {
        let c = |v: u8| (v as f32 * f).round().clamp(0.0, 255.0) as u8;
        Color::rgba(c(self.r), c(self.g), c(self.b), self.a)
    }
}

/// A 2D affine transform in Direct2D's row-vector convention: a point maps as
/// `x' = x*m11 + y*m21 + dx`, `y' = x*m12 + y*m22 + dy`. The layout matches `D2D1_MATRIX_3X2_F`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct Matrix {
    pub m11: f32,
    pub m12: f32,
    pub m21: f32,
    pub m22: f32,
    pub dx: f32,
    pub dy: f32,
}

impl Default for Matrix {
    fn default() -> Matrix {
        Matrix::IDENTITY
    }
}

impl Matrix {
    /// Whether a canvas image drawn into `width` × `height` of user space through this transform
    /// has its edges antialiased: only when the transform rotates or skews it, or when it covers
    /// less than a device pixel across (it would drop out otherwise). An axis-aligned image covers
    /// whole device pixels, those whose centres it covers, so images drawn edge to edge at
    /// fractional places leave no blended seams.
    pub fn antialiases_image(&self, width: f32, height: f32) -> bool {
        let (across, down) = if self.m12 == 0.0 && self.m21 == 0.0 {
            (self.m11, self.m22)
        } else if self.m11 == 0.0 && self.m22 == 0.0 {
            // (a quarter turn: the image's width lies along the device's y axis)
            (self.m12, self.m21)
        } else {
            return true;
        };
        (width * across).abs() < 1.0 || (height * down).abs() < 1.0
    }

    pub const IDENTITY: Matrix = Matrix {
        m11: 1.0,
        m12: 0.0,
        m21: 0.0,
        m22: 1.0,
        dx: 0.0,
        dy: 0.0,
    };

    pub const fn new(m11: f32, m12: f32, m21: f32, m22: f32, dx: f32, dy: f32) -> Matrix {
        Matrix {
            m11,
            m12,
            m21,
            m22,
            dx,
            dy,
        }
    }

    pub const fn translation(dx: f32, dy: f32) -> Matrix {
        Matrix::new(1.0, 0.0, 0.0, 1.0, dx, dy)
    }

    pub const fn scale(sx: f32, sy: f32) -> Matrix {
        Matrix::new(sx, 0.0, 0.0, sy, 0.0, 0.0)
    }

    /// A rotation by `radians`, clockwise on screen (y points down), about the origin: CSS `rotate()`.
    pub fn rotation(radians: f32) -> Matrix {
        let (s, c) = radians.sin_cos();
        Matrix::new(c, s, -s, c, 0.0, 0.0)
    }

    /// CSS `skew(ax, ay)` in radians.
    pub fn skew(ax: f32, ay: f32) -> Matrix {
        Matrix::new(1.0, ay.tan(), ax.tan(), 1.0, 0.0, 0.0)
    }

    /// A rotation about `center`.
    pub fn rotation_about(radians: f32, center: Point) -> Matrix {
        Matrix::translation(-center.x, -center.y)
            .then(&Matrix::rotation(radians))
            .then(&Matrix::translation(center.x, center.y))
    }

    /// `self` followed by `next`: the matrix that maps p to `next.apply(self.apply(p))`. For a transform
    /// pushed inside a parent, the world matrix is `local.then(&parent)`.
    pub fn then(&self, next: &Matrix) -> Matrix {
        let a = self;
        let b = next;
        Matrix {
            m11: a.m11 * b.m11 + a.m12 * b.m21,
            m12: a.m11 * b.m12 + a.m12 * b.m22,
            m21: a.m21 * b.m11 + a.m22 * b.m21,
            m22: a.m21 * b.m12 + a.m22 * b.m22,
            dx: a.dx * b.m11 + a.dy * b.m21 + b.dx,
            dy: a.dx * b.m12 + a.dy * b.m22 + b.dy,
        }
    }

    pub fn determinant(&self) -> f32 {
        self.m11 * self.m22 - self.m12 * self.m21
    }

    /// The inverse, or `None` when the matrix is singular (a zero scale) or not finite.
    pub fn inverse(&self) -> Option<Matrix> {
        let det = self.determinant();
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let inv = 1.0 / det;
        Some(Matrix {
            m11: self.m22 * inv,
            m12: -self.m12 * inv,
            m21: -self.m21 * inv,
            m22: self.m11 * inv,
            dx: (self.m21 * self.dy - self.m22 * self.dx) * inv,
            dy: (self.m12 * self.dx - self.m11 * self.dy) * inv,
        })
    }

    pub fn apply(&self, p: Point) -> Point {
        Point::new(
            p.x * self.m11 + p.y * self.m21 + self.dx,
            p.x * self.m12 + p.y * self.m22 + self.dy,
        )
    }

    /// The axis-aligned bounding box of the transformed rectangle.
    pub fn apply_rect(&self, r: &Rect) -> Rect {
        if self.is_axis_aligned() {
            let a = self.apply(Point::new(r.x, r.y));
            let b = self.apply(Point::new(r.right(), r.bottom()));
            return Rect::from_ltrb(a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y));
        }
        let pts = [
            self.apply(Point::new(r.x, r.y)),
            self.apply(Point::new(r.right(), r.y)),
            self.apply(Point::new(r.x, r.bottom())),
            self.apply(Point::new(r.right(), r.bottom())),
        ];
        let (mut l, mut t, mut rr, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in pts {
            l = l.min(p.x);
            t = t.min(p.y);
            rr = rr.max(p.x);
            b = b.max(p.y);
        }
        Rect::from_ltrb(l, t, rr, b)
    }

    /// Whether axis-aligned rectangles stay axis-aligned (no rotation or skew; flips allowed).
    pub fn is_axis_aligned(&self) -> bool {
        (self.m12 == 0.0 && self.m21 == 0.0) || (self.m11 == 0.0 && self.m22 == 0.0)
    }

    pub fn is_identity(&self) -> bool {
        *self == Matrix::IDENTITY
    }

    /// The average linear scale (square root of the area scale): how many device pixels a CSS pixel
    /// covers, for sizing intermediate bitmaps.
    pub fn approximate_scale(&self) -> f32 {
        self.determinant().abs().sqrt()
    }

    pub fn is_finite(&self) -> bool {
        [self.m11, self.m12, self.m21, self.m22, self.dx, self.dy]
            .iter()
            .all(|v| v.is_finite())
    }
}

/// Pixel snapping of box geometry at paint time, as Blink does it (`PixelSnappedIntRect`,
/// `SnapSizeToPixel`): layout positions stay fractional, and painting rounds each edge of a box's
/// rectangle to the nearest device pixel on its own (so the snapped size is
/// `round(origin + size) - round(origin)`: two boxes that touch still touch, and a box keeps its
/// size to within a pixel). Coordinates are first taken to Blink's layout precision, 1/64 px, so
/// an edge layout puts at exactly half a pixel rounds the same way (up).
///
/// A `Snap` maps CSS px to device pixels by a positive scale and a translation (no rotation, skew
/// or flip): what is drawn under any other transform is not snapped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snap {
    sx: f32,
    sy: f32,
    tx: f32,
    ty: f32,
}

impl Snap {
    /// The snapping for content drawn under `m` (CSS px to device pixels), if it is a positive
    /// scale and a translation.
    pub fn of(m: &Matrix) -> Option<Snap> {
        (m.m12 == 0.0 && m.m21 == 0.0 && m.m11 > 0.0 && m.m22 > 0.0 && m.is_finite()).then_some(
            Snap {
                sx: m.m11,
                sy: m.m22,
                tx: m.dx,
                ty: m.dy,
            },
        )
    }

    /// Whether snapping still applies under `local` pushed onto this one: only a translation by
    /// whole device pixels keeps it (inside any other transform, as in Blink, box edges are not
    /// moved to device pixels).
    pub fn keeps(&self, local: &Matrix) -> bool {
        let whole = |v: f32| (v - v.round()).abs() < 1.0 / 128.0;
        local.m11 == 1.0
            && local.m22 == 1.0
            && local.m12 == 0.0
            && local.m21 == 0.0
            && whole(local.dx * self.sx)
            && whole(local.dy * self.sy)
    }

    /// A device coordinate rounded as Blink rounds a `LayoutUnit` (1/64 px, half up).
    fn round(v: f32) -> f32 {
        ((v * 64.0).round() / 64.0 + 0.5).floor()
    }

    /// `x` (CSS px) moved to the nearest device pixel boundary.
    pub fn x(&self, x: f32) -> f32 {
        (Snap::round(x * self.sx + self.tx) - self.tx) / self.sx
    }

    /// `y` (CSS px) moved to the nearest device pixel boundary.
    pub fn y(&self, y: f32) -> f32 {
        (Snap::round(y * self.sy + self.ty) - self.ty) / self.sy
    }

    /// A rectangle with each edge moved to the nearest device pixel boundary. Rectangles too far
    /// out to snap meaningfully (the "infinite" clip) stay as they are.
    pub fn rect(&self, r: &Rect) -> Rect {
        const FAR: f32 = 1.0e7;
        if !(r.x.abs() < FAR && r.y.abs() < FAR && r.width.abs() < FAR && r.height.abs() < FAR) {
            return *r;
        }
        let left = self.x(r.x);
        let top = self.y(r.y);
        Rect::new(
            left,
            top,
            (self.x(r.right()) - left).max(0.0),
            (self.y(r.bottom()) - top).max(0.0),
        )
    }

    /// Border widths in whole device pixels, as CSS Values 4 "snap as a border width" does (down
    /// to whole device pixels, and at least one for a nonzero width): with the box's edges on
    /// device pixels, its padding box's are too. (Computed border widths are snapped already;
    /// this keeps the rasterizer's ring exact whatever it is given.)
    pub fn border_widths(&self, w: &Sides) -> Sides {
        let snap = |v: f32, s: f32| {
            if v > 0.0 {
                ((v * s * 64.0).round() / 64.0).floor().max(1.0) / s
            } else {
                0.0
            }
        };
        Sides::new(
            snap(w.top, self.sy),
            snap(w.right, self.sx),
            snap(w.bottom, self.sy),
            snap(w.left, self.sx),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_spread_keeps_small_radii_sharp() {
        // a 10px radius on a large box spread by 20px: 20 × (1 − 0.5³ × (1 − 0.02³)) ≈ 17.5
        let big = Size::new(1000.0, 1000.0);
        let r = Radii::uniform(10.0).outset(20.0, big);
        assert!((r.top_left.width - 27.5).abs() < 1e-3, "{r:?}");
        // radii larger than the spread grow by it; zero stays zero
        assert_eq!(Radii::uniform(30.0).outset(20.0, big).top_left.width, 50.0);
        assert_eq!(Radii::ZERO.outset(20.0, big), Radii::ZERO);
        // a fully rounded corner (coverage 1: a 2px circle) grows by the whole spread
        let dot = Radii::uniform(1.0).outset(149.0, Size::new(2.0, 2.0));
        assert_eq!(dot.top_left.width, 150.0);
        // shrinking: 30 - 20 = 10
        assert_eq!(Radii::uniform(30.0).outset(-20.0, big).top_left.width, 10.0);
    }

    #[test]
    fn snap_rounds_each_edge_to_device_pixels() {
        let s = Snap::of(&Matrix::IDENTITY).unwrap();
        // origin 10.3 -> 10, far edge 10.3 + 20.4 = 30.7 -> 31
        assert_eq!(
            s.rect(&Rect::new(10.3, 5.5, 20.4, 0.4)),
            Rect::new(10.0, 6.0, 21.0, 0.0)
        );
        // adjacent boxes stay adjacent: both share the snapped edge
        let a = s.rect(&Rect::new(0.0, 0.0, 33.333_33, 10.0));
        let b = s.rect(&Rect::new(33.333_33, 0.0, 33.333_33, 10.0));
        assert_eq!(a.right(), b.x);
        // half a pixel rounds up, even when f32 layout lands just below it
        assert_eq!(s.x(12.499_999), 13.0);
        assert_eq!(s.x(-0.4), 0.0);
    }

    #[test]
    fn snap_works_in_device_pixels() {
        // at a device scale of 1.5, 10.5 CSS px is 15.75 device px -> 16 -> 10.666 CSS px
        let s = Snap::of(&Matrix::scale(1.5, 1.5)).unwrap();
        assert!((s.x(10.5) - 16.0 / 1.5).abs() < 1e-5);
        // a tile's integer device offset keeps edges on pixels of the tile
        let t = Snap::of(&Matrix::scale(2.0, 2.0).then(&Matrix::translation(-512.0, 0.0))).unwrap();
        assert!((t.x(256.3) * 2.0 - 512.0 - 1.0).abs() < 1e-4);
        // a 1px border at 1.5x is one device pixel; a third of a CSS px is at least one
        let w = s.border_widths(&Sides::new(1.0, 0.0, 0.3, 2.0));
        assert!((w.top - 1.0 / 1.5).abs() < 1e-5 && w.right == 0.0);
        assert!((w.bottom - 1.0 / 1.5).abs() < 1e-5 && (w.left - 2.0).abs() < 1e-5);
    }

    #[test]
    fn snap_needs_an_axis_scale_and_whole_pixel_translations() {
        assert!(Snap::of(&Matrix::rotation(0.3)).is_none());
        assert!(Snap::of(&Matrix::scale(-1.0, 1.0)).is_none());
        let s = Snap::of(&Matrix::scale(2.0, 2.0)).unwrap();
        assert!(s.keeps(&Matrix::translation(10.5, 3.0)));
        assert!(!s.keeps(&Matrix::translation(10.25, 0.0)));
        assert!(!s.keeps(&Matrix::scale(1.5, 1.5)));
        // the infinite clip is left alone
        assert_eq!(s.rect(&Rect::infinite()), Rect::infinite());
    }

    fn close(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-4 && (a.y - b.y).abs() < 1e-4
    }

    #[test]
    fn then_applies_left_first() {
        let m = Matrix::scale(2.0, 3.0).then(&Matrix::translation(10.0, 20.0));
        assert_eq!(m.apply(Point::new(1.0, 1.0)), Point::new(12.0, 23.0));
        let n = Matrix::translation(10.0, 20.0).then(&Matrix::scale(2.0, 3.0));
        assert_eq!(n.apply(Point::new(1.0, 1.0)), Point::new(22.0, 63.0));
    }

    #[test]
    fn rotation_is_clockwise_on_screen() {
        let m = Matrix::rotation(std::f32::consts::FRAC_PI_2);
        assert!(close(m.apply(Point::new(1.0, 0.0)), Point::new(0.0, 1.0)));
        assert!(close(m.apply(Point::new(0.0, 1.0)), Point::new(-1.0, 0.0)));
        let about = Matrix::rotation_about(std::f32::consts::PI, Point::new(5.0, 5.0));
        assert!(close(
            about.apply(Point::new(0.0, 0.0)),
            Point::new(10.0, 10.0)
        ));
    }

    #[test]
    fn inverse_round_trips() {
        let m = Matrix::rotation(0.3)
            .then(&Matrix::scale(2.0, 0.5))
            .then(&Matrix::skew(0.2, 0.1))
            .then(&Matrix::translation(7.0, -3.0));
        let inv = m.inverse().unwrap();
        let p = Point::new(13.0, -4.5);
        assert!(close(inv.apply(m.apply(p)), p));
        let id = m.then(&inv);
        assert!(close(id.apply(Point::new(1.0, 2.0)), Point::new(1.0, 2.0)));
        assert!(Matrix::scale(0.0, 1.0).inverse().is_none());
    }

    #[test]
    fn determinant_and_scale() {
        assert_eq!(Matrix::scale(2.0, 3.0).determinant(), 6.0);
        assert!((Matrix::rotation(1.0).determinant() - 1.0).abs() < 1e-6);
        let m = Matrix::scale(2.0, 2.0).then(&Matrix::rotation(0.7));
        assert!((m.approximate_scale() - 2.0).abs() < 1e-5);
    }

    #[test]
    fn apply_rect_bounds_rotations() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Matrix::rotation(std::f32::consts::FRAC_PI_4).apply_rect(&r);
        let diagonal = 200f32.sqrt();
        assert!((b.width - diagonal).abs() < 1e-3 && (b.height - diagonal).abs() < 1e-3);
        let flipped = Matrix::scale(-1.0, 1.0).apply_rect(&r);
        assert_eq!(flipped, Rect::new(-10.0, 0.0, 10.0, 10.0));
        assert!(Matrix::scale(-1.0, 2.0).is_axis_aligned());
        assert!(!Matrix::rotation(0.1).is_axis_aligned());
    }

    #[test]
    fn rect_ops() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersect(&b), Rect::new(5.0, 5.0, 5.0, 5.0));
        assert_eq!(a.union(&b), Rect::new(0.0, 0.0, 15.0, 15.0));
        assert!(!a.intersects(&Rect::new(10.0, 0.0, 5.0, 5.0)));
        assert!(Rect::new(0.0, 0.0, f32::NAN, 1.0).is_empty());
        assert_eq!(
            a.deflate(&Sides::new(1.0, 2.0, 3.0, 4.0)),
            Rect::new(4.0, 1.0, 4.0, 6.0)
        );
    }

    #[test]
    fn radii_fit_and_shrink() {
        let r = Radii::uniform(30.0).fitted(&Rect::new(0.0, 0.0, 40.0, 100.0));
        assert_eq!(r.top_left, Size::new(20.0, 20.0));
        let inner = Radii::uniform(10.0).inner(&Sides::new(4.0, 12.0, 4.0, 4.0));
        assert_eq!(inner.top_right, Size::new(0.0, 6.0));
        assert!(Radii::ZERO.is_zero() && Radii::uniform(3.0).is_uniform());
        assert_eq!(Radii::ZERO.outset(5.0, Size::new(10.0, 10.0)), Radii::ZERO);
    }
}
