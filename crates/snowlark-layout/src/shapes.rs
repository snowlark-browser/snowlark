// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A float's float area (CSS Shapes 1 §3): the shape `shape-outside` gives it, grown by
//! `shape-margin` (§5.3), as the horizontal interval it excludes from each line beside it. Shapes are
//! in the coordinates of their reference box (§4), whose top left is `offset` into the float's margin
//! box; the float area is clipped to the margin box (§3: "the float area ... is limited to the margin
//! box").

use snowlark_paint::{Point, Radii, Rect, Sides, Size};

use crate::logical::Flip;
use crate::paint::{inset_sides, radii};
use crate::style::{
    ClipBox, ClipShape, Corner, LengthPercentage, ShapeImage, ShapeOutside, ShapeRadius, Side,
    Style,
};

/// The float area of a float with `style`, read through its containing block's flow frame (CSS
/// Shapes 1 §3, §4): its border box is `size` and its used margins `margin`; percentages of its
/// padding and `shape-margin` are of `cb_width`. `None` when `shape-outside` is `none`.
pub(crate) fn float_shape<S: Style>(
    style: &S,
    size: Size,
    margin: Sides,
    cb_width: f32,
) -> Option<FloatShape> {
    let outside = style.shape_outside()?;
    let side = |f: &dyn Fn(Side) -> f32| {
        Sides::new(f(Side::Top), f(Side::Right), f(Side::Bottom), f(Side::Left))
    };
    let border = side(&|s| style.border_width(s));
    let padding = side(&|s| style.padding(s).resolve(cb_width));
    let reference = match &outside {
        ShapeOutside::Shape(_, b) | ShapeOutside::Box(b) => *b,
        // (an image is drawn from the content box's top left)
        ShapeOutside::Image(_) | ShapeOutside::Gradient(_) => ClipBox::Content,
    };
    let border_box = Rect::new(margin.left, margin.top, size.width, size.height);
    let r = match reference {
        ClipBox::Margin => Rect::new(
            0.0,
            0.0,
            size.width + margin.left + margin.right,
            size.height + margin.top + margin.bottom,
        ),
        ClipBox::Border => border_box,
        ClipBox::Padding => border_box.deflate(&border),
        ClipBox::Content => border_box.deflate(&inset_sides(&border, &padding)),
    };
    let r = Rect::new(r.x, r.y, r.width.max(0.0), r.height.max(0.0));
    let (w, h) = (r.width, r.height);
    let geometry = match &outside {
        ShapeOutside::Box(_) => {
            let outer = radii(style, size);
            let corners = match reference {
                // (a rounded corner grows by its two sides' margins, CSS Backgrounds 3 §5.1)
                ClipBox::Margin => {
                    let grown = |c: Size, x: f32, y: f32| {
                        Size::new(
                            if c.width > 0.0 { c.width + x } else { 0.0 },
                            if c.height > 0.0 { c.height + y } else { 0.0 },
                        )
                    };
                    Radii {
                        top_left: grown(outer.top_left, margin.left, margin.top),
                        top_right: grown(outer.top_right, margin.right, margin.top),
                        bottom_right: grown(outer.bottom_right, margin.right, margin.bottom),
                        bottom_left: grown(outer.bottom_left, margin.left, margin.bottom),
                    }
                }
                ClipBox::Border => outer,
                ClipBox::Padding => outer.inner(&border),
                ClipBox::Content => outer.inner(&inset_sides(&border, &padding)),
            };
            let rect = Rect::new(0.0, 0.0, w, h);
            ShapeGeometry::Rounded {
                rect,
                radii: if corners.is_zero() {
                    Radii::default()
                } else {
                    corners.fitted(&rect)
                },
            }
        }
        ShapeOutside::Shape(shape, _) => {
            // (the shape's coordinates are physical: it is resolved in the physical reference box
            // and turned into the float's flow frame, which lines run along)
            let view = style.view();
            let physical = view.size(Size::new(w, h));
            let g = basic_shape(shape, physical.width, physical.height);
            if view.is_identity() {
                g
            } else {
                g.in_frame(view.inverse(), physical)
            }
        }
        ShapeOutside::Image(image) => {
            let image = in_flow_frame(image, style.view(), w, h);
            raster(&image, style.shape_image_threshold(), h)
        }
        ShapeOutside::Gradient(g) => {
            // (a gradient has no natural size: it is drawn the size of the reference box, in
            // physical coordinates)
            let physical = style.view().size(Size::new(w, h));
            let (iw, ih) = (
                physical.width.round().max(0.0) as u32,
                physical.height.round().max(0.0) as u32,
            );
            let alpha = if u64::from(iw) * u64::from(ih) <= MAX_RASTER_PIXELS {
                crate::paint::gradient_alpha(g, iw, ih)
            } else {
                None
            };
            let image = ShapeImage {
                width: iw,
                height: ih,
                alpha: alpha.unwrap_or_default(),
            };
            let image = in_flow_frame(&image, style.view(), w, h);
            raster(&image, style.shape_image_threshold(), h)
        }
    };
    let shape_margin = style.shape_margin().resolve(cb_width).max(0.0);
    let geometry = match geometry {
        ShapeGeometry::Raster { top, rows } if shape_margin > 0.0 => {
            // (no farther than the margin box's diagonal: it is clipped to it anyway)
            let most = (size.width + margin.left + margin.right)
                .max(size.height + margin.top + margin.bottom)
                * std::f32::consts::SQRT_2;
            let (top, rows) = raster_margin(top, &rows, shape_margin.min(most.max(0.0)));
            ShapeGeometry::Raster { top, rows }
        }
        g => g,
    };
    Some(FloatShape {
        margin: if matches!(geometry, ShapeGeometry::Raster { .. }) {
            0.0
        } else {
            shape_margin
        },
        geometry,
        offset: Point::new(r.x, r.y),
        reference_height: h,
    })
}

/// A basic shape in a reference box `w` × `h` (CSS Shapes 1 §3.1).
fn basic_shape(shape: &ClipShape, w: f32, h: f32) -> ShapeGeometry {
    let center = |c: &Option<(LengthPercentage, LengthPercentage)>| match c {
        Some((x, y)) => Point::new(x.resolve(w), y.resolve(h)),
        None => Point::new(w / 2.0, h / 2.0),
    };
    let sides = |c: Point| ((c.x.abs(), (w - c.x).abs()), (c.y.abs(), (h - c.y).abs()));
    match shape {
        ClipShape::Inset {
            sides: s,
            radii: rr,
        } => {
            let (top, right) = (s[0].resolve(h), s[1].resolve(w));
            let (bottom, left) = (s[2].resolve(h), s[3].resolve(w));
            let rect = Rect::new(
                left,
                top,
                (w - left - right).max(0.0),
                (h - top - bottom).max(0.0),
            );
            let corner = |c: &(LengthPercentage, LengthPercentage)| {
                Size::new(c.0.resolve(w).max(0.0), c.1.resolve(h).max(0.0))
            };
            let radii = Radii {
                top_left: corner(&rr[0]),
                top_right: corner(&rr[1]),
                bottom_right: corner(&rr[2]),
                bottom_left: corner(&rr[3]),
            };
            ShapeGeometry::Rounded {
                rect,
                radii: if radii.is_zero() {
                    Radii::default()
                } else {
                    radii.fitted(&rect)
                },
            }
        }
        ClipShape::Circle { center: c, radius } => {
            let c = center(c);
            let ((l, r), (t, b)) = sides(c);
            let reach = match radius {
                // (percentages of the box's diagonal over the square root of two)
                ShapeRadius::Length(v) => v.resolve((w * w + h * h).sqrt() / 2f32.sqrt()),
                ShapeRadius::ClosestSide => l.min(r).min(t).min(b),
                ShapeRadius::FarthestSide => l.max(r).max(t).max(b),
                ShapeRadius::ClosestCorner => l.min(r).hypot(t.min(b)),
                ShapeRadius::FarthestCorner => l.max(r).hypot(t.max(b)),
            }
            .max(0.0);
            ShapeGeometry::Ellipse {
                center: c,
                rx: reach,
                ry: reach,
            }
        }
        ClipShape::Ellipse {
            center: c,
            radii: (rx, ry),
        } => {
            let c = center(c);
            let (horizontal, vertical) = sides(c);
            let axis = |radius: &ShapeRadius, near: (f32, f32), basis: f32| match radius {
                ShapeRadius::Length(v) => v.resolve(basis),
                ShapeRadius::ClosestSide => near.0.min(near.1),
                ShapeRadius::FarthestSide => near.0.max(near.1),
                ShapeRadius::ClosestCorner => near.0.min(near.1) * 2f32.sqrt(),
                ShapeRadius::FarthestCorner => near.0.max(near.1) * 2f32.sqrt(),
            };
            ShapeGeometry::Ellipse {
                center: c,
                rx: axis(rx, horizontal, w).max(0.0),
                ry: axis(ry, vertical, h).max(0.0),
            }
        }
        ClipShape::Polygon { points, .. } => ShapeGeometry::polygon(&[points
            .iter()
            .map(|(x, y)| Point::new(x.resolve(w), y.resolve(h)))
            .collect()]),
        ClipShape::Path { commands, .. } => ShapeGeometry::polygon(&crate::clip::flatten_path(
            commands,
            Rect::new(0.0, 0.0, w, h),
        )),
        // (the style gives neither for `shape-outside`)
        ClipShape::Box | ClipShape::Reference(_) => ShapeGeometry::Rounded {
            rect: Rect::new(0.0, 0.0, w, h),
            radii: Radii::default(),
        },
    }
}

/// An image drawn from the physical top left of a reference box `w` × `h` (flow frame) as the
/// pixels of that box in the flow frame of `view` (which maps the flow frame to physical
/// coordinates); the image itself when the frame is physical.
fn in_flow_frame(
    image: &ShapeImage,
    view: Flip,
    w: f32,
    h: f32,
) -> std::borrow::Cow<'_, ShapeImage> {
    if view.is_identity() {
        return std::borrow::Cow::Borrowed(image);
    }
    let (fw, fh) = (w.round().max(0.0) as u32, h.round().max(0.0) as u32);
    if u64::from(fw) * u64::from(fh) > MAX_RASTER_PIXELS {
        return std::borrow::Cow::Owned(ShapeImage::default());
    }
    let within = Size::new(w, h);
    let mut alpha = Vec::with_capacity((fw * fh) as usize);
    for y in 0..fh {
        for x in 0..fw {
            let p = view.point(Point::new(x as f32 + 0.5, y as f32 + 0.5), within);
            let (px, py) = (p.x.floor(), p.y.floor());
            let inside =
                px >= 0.0 && py >= 0.0 && (px as u32) < image.width && (py as u32) < image.height;
            alpha.push(if inside {
                image
                    .alpha
                    .get((py as u32 * image.width + px as u32) as usize)
                    .copied()
                    .unwrap_or(0)
            } else {
                0
            });
        }
    }
    std::borrow::Cow::Owned(ShapeImage {
        width: fw,
        height: fh,
        alpha,
    })
}

/// The most pixels a gradient is drawn with for its shape.
const MAX_RASTER_PIXELS: u64 = 4096 * 4096;

/// An image's pixels as a raster shape (§3.2): per row, the extent of the pixels whose alpha is
/// greater than the threshold's share of the full alpha (§5.2). Rows below the reference box's
/// height (`height`) are not part of it.
fn raster(image: &ShapeImage, threshold: f32, height: f32) -> ShapeGeometry {
    let limit = (threshold.clamp(0.0, 1.0) * 255.0) as u8;
    let w = image.width as usize;
    let rows = (0..(image.height as usize).min(height.max(0.0).floor() as usize))
        .map(|y| {
            let row = image.alpha.get(y * w..(y + 1) * w)?;
            let first = row.iter().position(|&a| a > limit)?;
            let last = row.iter().rposition(|&a| a > limit)?;
            Some((first as i32, last as i32 + 1))
        })
        .collect();
    ShapeGeometry::Raster { top: 0, rows }
}

/// A float's shape for wrapping lines around it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FloatShape {
    pub geometry: ShapeGeometry,
    /// `shape-margin`, in px (§5.3).
    pub margin: f32,
    /// The reference box's top left, from the float's margin box's top left.
    pub offset: Point,
    /// The reference box's height.
    pub reference_height: f32,
}

/// A shape in its reference box's coordinates.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ShapeGeometry {
    /// `circle()` and `ellipse()`.
    Ellipse { center: Point, rx: f32, ry: f32 },
    /// `inset()` (and `rect()`, `xywh()`) and the box values: a rectangle with rounded corners.
    Rounded { rect: Rect, radii: Radii },
    /// `polygon()`, and the contours of `path()` and `shape()`: the edges of the area, each from
    /// one vertex to the next.
    Polygon {
        edges: Vec<(Point, Point)>,
        bounds: Rect,
    },
    /// An image's or a gradient's pixels (§3.2): per pixel row from `top`, the extent of the pixels
    /// whose alpha is above the threshold, already grown by the margin.
    Raster {
        top: i32,
        rows: Vec<Option<(i32, i32)>>,
    },
}

impl ShapeGeometry {
    /// A polygon of `contours` (each closed by its last vertex joining its first); none when it has
    /// no area to speak of (fewer than three vertices, or a flat bounding box).
    pub fn polygon(contours: &[Vec<Point>]) -> ShapeGeometry {
        let mut edges = Vec::new();
        let mut bounds: Option<Rect> = None;
        let mut vertices = 0;
        for c in contours {
            let Some(&last) = c.last() else {
                continue;
            };
            let mut a = last;
            for &b in c {
                vertices += 1;
                bounds = Some(match bounds {
                    None => Rect::new(b.x, b.y, 0.0, 0.0),
                    Some(r) => Rect::from_ltrb(
                        r.x.min(b.x),
                        r.y.min(b.y),
                        r.right().max(b.x),
                        r.bottom().max(b.y),
                    ),
                });
                if a != b {
                    edges.push((a, b));
                }
                a = b;
            }
        }
        let bounds = bounds.unwrap_or_default();
        if vertices < 3 || bounds.width <= 0.0 || bounds.height <= 0.0 {
            edges.clear();
        }
        ShapeGeometry::Polygon { edges, bounds }
    }

    /// The shape in a box of size `within` mapped by `flip` (from physical coordinates to a flow
    /// frame).
    fn in_frame(self, flip: Flip, within: Size) -> ShapeGeometry {
        let point = |p: Point| flip.point(p, within);
        match self {
            ShapeGeometry::Ellipse { center, rx, ry } => {
                let (rx, ry) = if flip.swaps() { (ry, rx) } else { (rx, ry) };
                ShapeGeometry::Ellipse {
                    center: point(center),
                    rx,
                    ry,
                }
            }
            ShapeGeometry::Rounded { rect, radii } => {
                let mut out = Radii::default();
                for (corner, size) in [
                    (Corner::TopLeft, radii.top_left),
                    (Corner::TopRight, radii.top_right),
                    (Corner::BottomRight, radii.bottom_right),
                    (Corner::BottomLeft, radii.bottom_left),
                ] {
                    let size = flip.size(size);
                    match flip.corner(corner) {
                        Corner::TopLeft => out.top_left = size,
                        Corner::TopRight => out.top_right = size,
                        Corner::BottomRight => out.bottom_right = size,
                        Corner::BottomLeft => out.bottom_left = size,
                    }
                }
                ShapeGeometry::Rounded {
                    rect: flip.rect(rect, within),
                    radii: out,
                }
            }
            ShapeGeometry::Polygon { edges, bounds } => ShapeGeometry::Polygon {
                edges: edges
                    .into_iter()
                    .map(|(a, b)| (point(a), point(b)))
                    .collect(),
                bounds: flip.rect(bounds, within),
            },
            raster @ ShapeGeometry::Raster { .. } => raster,
        }
    }

    /// The shape grown by `margin`'s bounding box; `None` when it is empty.
    fn margin_bounds(&self, margin: f32) -> Option<Rect> {
        let r = match self {
            ShapeGeometry::Ellipse { center, rx, ry } => {
                let (rx, ry) = (rx + margin, ry + margin);
                Rect::new(center.x - rx, center.y - ry, rx * 2.0, ry * 2.0)
            }
            ShapeGeometry::Rounded { rect, .. } => rect.inflate(margin, margin),
            ShapeGeometry::Polygon { edges, bounds } => {
                if edges.is_empty() {
                    return None;
                }
                bounds.inflate(margin, margin)
            }
            ShapeGeometry::Raster { top, rows } => {
                let mut b: Option<(i32, i32, i32, i32)> = None;
                for (i, row) in rows.iter().enumerate() {
                    if let Some((x1, x2)) = *row {
                        let y = top + i as i32;
                        b = Some(match b {
                            None => (x1, y, x2, y + 1),
                            Some((l, t, r, _)) => (l.min(x1), t, r.max(x2), y + 1),
                        });
                    }
                }
                let (l, t, r, bt) = b?;
                Rect::from_ltrb(l as f32, t as f32, r as f32, bt as f32)
            }
        };
        (r.width > 0.0 && r.height > 0.0).then_some(r)
    }

    /// Whether a line from `top` `height` tall overlaps the shape grown by `margin` (a line of no
    /// height overlaps it at its top).
    fn line_overlaps(&self, margin: f32, top: f32, height: f32) -> bool {
        let Some(r) = self.margin_bounds(margin) else {
            return false;
        };
        (top < r.bottom() && top + height > r.y) || (height == 0.0 && top == r.y)
    }

    /// The interval of `x` the shape grown by `margin` takes up between `y1` and `y1 + height`.
    fn excluded(&self, margin: f32, y1: f32, height: f32) -> Option<(f32, f32)> {
        let y2 = y1 + height;
        match self {
            ShapeGeometry::Ellipse { center, rx, ry } => {
                let (rx, ry) = (rx + margin, ry + margin);
                if rx <= 0.0 || ry <= 0.0 {
                    return None;
                }
                let (top, bottom) = (center.y - ry, center.y + ry);
                if y2 < top || y1 >= bottom {
                    return None;
                }
                let mut x = rx;
                if y1 > center.y || y2 < center.y {
                    let dy = if y1 > center.y {
                        y1 - center.y
                    } else {
                        y2 - center.y
                    };
                    x = rx * (1.0 - (dy * dy) / (ry * ry)).max(0.0).sqrt();
                }
                Some((center.x - x, center.x + x))
            }
            ShapeGeometry::Rounded { rect, radii } => {
                let (rect, radii) = if margin > 0.0 {
                    let grow = |c: Size| Size::new(c.width + margin, c.height + margin);
                    (
                        rect.inflate(margin, margin),
                        Radii {
                            top_left: grow(radii.top_left),
                            top_right: grow(radii.top_right),
                            bottom_right: grow(radii.bottom_right),
                            bottom_left: grow(radii.bottom_left),
                        },
                    )
                } else {
                    (*rect, *radii)
                };
                if rect.width <= 0.0 || rect.height <= 0.0 {
                    return None;
                }
                if radii.is_zero() {
                    return Some((rect.x, rect.right()));
                }
                let tl = Rect::new(rect.x, rect.y, radii.top_left.width, radii.top_left.height);
                let tr = Rect::new(
                    rect.right() - radii.top_right.width,
                    rect.y,
                    radii.top_right.width,
                    radii.top_right.height,
                );
                let bl = Rect::new(
                    rect.x,
                    rect.bottom() - radii.bottom_left.height,
                    radii.bottom_left.width,
                    radii.bottom_left.height,
                );
                let br = Rect::new(
                    rect.right() - radii.bottom_right.width,
                    rect.bottom() - radii.bottom_right.height,
                    radii.bottom_right.width,
                    radii.bottom_right.height,
                );
                // A line across the straight part of both sides takes the whole width.
                let top_corners = tl.bottom().max(tr.bottom());
                let bottom_corners = bl.y.min(br.y);
                if top_corners <= bottom_corners && y1 <= top_corners && y2 >= bottom_corners {
                    return Some((rect.x, rect.right()));
                }
                let (mut x1, mut x2) = (rect.right(), rect.x);
                if y1 <= tl.bottom() && y2 >= bl.y {
                    x1 = rect.x;
                }
                if y1 <= tr.bottom() && y2 >= br.y {
                    x2 = rect.right();
                }
                for y in [y1, y2] {
                    if let Some((a, b)) = rounded_x_intercepts(&rect, [tl, tr, bl, br], y) {
                        x1 = x1.min(a);
                        x2 = x2.max(b);
                    }
                }
                (x2 >= x1).then_some((x1, x2))
            }
            ShapeGeometry::Polygon { edges, bounds } => {
                if edges.is_empty() {
                    return None;
                }
                let (ly1, ly2) = (y1 - margin, y2 + margin);
                if !(ly2 >= ly1 && ly2 >= bounds.y && ly1 <= bounds.bottom()) {
                    return None;
                }
                let mut out: Option<(f32, f32)> = None;
                let mut unite = |i: Option<(f32, f32)>| {
                    if let Some((a, b)) = i {
                        out = Some(match out {
                            None => (a, b),
                            Some((x, y)) => (x.min(a), y.max(b)),
                        });
                    }
                };
                for &(a, b) in edges {
                    let (min_y, max_y) = (a.y.min(b.y), a.y.max(b.y));
                    if min_y == max_y || max_y < ly1 || min_y > ly2 {
                        continue;
                    }
                    if margin <= 0.0 {
                        unite(clipped_edge_x_range(a, b, y1, y2));
                    } else {
                        let n = inward_normal(a, b);
                        let (ox, oy) = (n.x * margin, n.y * margin);
                        let shift = |p: Point, s: f32| Point::new(p.x + ox * s, p.y + oy * s);
                        unite(clipped_edge_x_range(shift(a, -1.0), shift(b, -1.0), y1, y2));
                        unite(clipped_edge_x_range(shift(a, 1.0), shift(b, 1.0), y1, y2));
                        unite(clipped_circle_x_range(a, margin, y1, y2));
                        unite(clipped_circle_x_range(b, margin, y1, y2));
                    }
                }
                out
            }
            ShapeGeometry::Raster { top, rows } => {
                let bounds = self.margin_bounds(0.0)?;
                let (mut a, mut b) = (y1.floor() as i32, y2.floor() as i32);
                if b < bounds.y as i32 || a >= bounds.bottom() as i32 {
                    return None;
                }
                a = a.max(bounds.y as i32);
                b = b.min(bounds.bottom() as i32);
                let row = |y: i32| -> Option<(i32, i32)> {
                    rows.get(usize::try_from(y - top).ok()?).copied().flatten()
                };
                let mut out: Option<(i32, i32)> = None;
                let ys: Vec<i32> = if a == b { vec![a] } else { (a..b).collect() };
                for y in ys {
                    if let Some((x1, x2)) = row(y) {
                        out = Some(match out {
                            None => (x1, x2),
                            Some((l, r)) => (l.min(x1), r.max(x2)),
                        });
                    }
                }
                out.map(|(l, r)| (l as f32, r as f32))
            }
        }
    }
}

/// The left and right ends of a rounded rectangle's row at `y` (the corners are elliptical arcs).
fn rounded_x_intercepts(rect: &Rect, corners: [Rect; 4], y: f32) -> Option<(f32, f32)> {
    if y < rect.y || y > rect.bottom() {
        return None;
    }
    let [tl, tr, bl, br] = corners;
    let nonempty = |r: &Rect| r.width > 0.0 && r.height > 0.0;
    let intercept =
        |dy: f32, c: &Rect| c.width * (1.0 - (dy * dy) / (c.height * c.height)).max(0.0).sqrt();
    let min = if nonempty(&tl) && y >= tl.y && y < tl.bottom() {
        tl.right() - intercept(tl.bottom() - y, &tl)
    } else if nonempty(&bl) && y >= bl.y && y <= bl.bottom() {
        bl.right() - intercept(y - bl.y, &bl)
    } else {
        rect.x
    };
    let max = if nonempty(&tr) && y >= tr.y && y < tr.bottom() {
        tr.x + intercept(tr.bottom() - y, &tr)
    } else if nonempty(&br) && y >= br.y && y <= br.bottom() {
        br.x + intercept(y - br.y, &br)
    } else {
        rect.right()
    };
    Some((min, max))
}

/// The unit normal of the edge `a`-`b` pointing into a polygon whose vertices run clockwise on
/// the screen.
fn inward_normal(a: Point, b: Point) -> Point {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    if dx == 0.0 {
        return Point::new(if dy > 0.0 { -1.0 } else { 1.0 }, 0.0);
    }
    if dy == 0.0 {
        return Point::new(0.0, if dx > 0.0 { 1.0 } else { -1.0 });
    }
    let len = dx.hypot(dy);
    Point::new(-dy / len, dx / len)
}

/// The `x` range of the edge `a`-`b` between `y1` and `y2`; none when it only touches the range
/// at one of its ends.
fn clipped_edge_x_range(a: Point, b: Point, y1: f32, y2: f32) -> Option<(f32, f32)> {
    let (min_y, max_y) = (a.y.min(b.y), a.y.max(b.y));
    let (min_x, max_x) = (a.x.min(b.x), a.x.max(b.x));
    if !(y2 >= min_y && y1 <= max_y) || (y1 == max_y && min_y <= y1) || (y2 == min_y && max_y >= y2)
    {
        return None;
    }
    if min_y >= y1 && max_y <= y2 {
        return Some((min_x, max_x));
    }
    let x_at = |y: f32| {
        if a.y == b.y || a.x == b.x {
            min_x
        } else if y == min_y {
            if a.y < b.y {
                a.x
            } else {
                b.x
            }
        } else if y == max_y {
            if a.y > b.y {
                a.x
            } else {
                b.x
            }
        } else {
            a.x + (y - a.y) * (b.x - a.x) / (b.y - a.y)
        }
    };
    let (lo, hi) = if a.y < b.y { (a, b) } else { (b, a) };
    let x1 = if lo.y < y1 { x_at(y1) } else { lo.x };
    let x2 = if hi.y > y2 { x_at(y2) } else { hi.x };
    Some((x1.min(x2), x1.max(x2)))
}

/// The `x` range of a circle between `y1` and `y2`.
fn clipped_circle_x_range(center: Point, radius: f32, y1: f32, y2: f32) -> Option<(f32, f32)> {
    if y1 >= center.y + radius || y2 <= center.y - radius {
        return None;
    }
    if center.y >= y1 && center.y <= y2 {
        return Some((center.x - radius, center.x + radius));
    }
    let dy = if y2 < center.y { y2 } else { y1 } - center.y;
    let x = radius * (1.0 - (dy * dy) / (radius * radius)).max(0.0).sqrt();
    Some((center.x - x, center.x + x))
}

impl FloatShape {
    /// The interval of `x` (in the coordinates of `margin_box`, the float's margin box) the float
    /// area takes up beside a line from `top` `height` tall; `None` when it excludes nothing there.
    pub fn excluded_segment(&self, margin_box: Rect, top: f32, height: f32) -> Option<(f32, f32)> {
        let rel = top - (margin_box.y + self.offset.y);
        if !self.geometry.line_overlaps(self.margin, rel, height) {
            return None;
        }
        // (a line taller than the reference box counts as one as tall)
        let h = height.min(self.reference_height);
        let (x1, x2) = self.geometry.excluded(self.margin, rel, h)?;
        let clamp =
            |x: f32| (x + self.offset.x).clamp(0.0, margin_box.width.max(0.0)) + margin_box.x;
        Some((clamp(x1), clamp(x2)))
    }
}

/// Grows the rows of a raster shape by `margin` (§5.3: every point within `margin` of the shape):
/// each row's extent reaches the rows above and below within the margin by the half chord of a
/// circle of that radius.
pub(crate) fn raster_margin(
    top: i32,
    rows: &[Option<(i32, i32)>],
    margin: f32,
) -> (i32, Vec<Option<(i32, i32)>>) {
    let m = margin.ceil().max(0.0) as i32;
    if m == 0 {
        return (top, rows.to_vec());
    }
    let reach: Vec<i32> = (0..=m)
        .map(|dy| (f64::from(m) * f64::from(m) - f64::from(dy) * f64::from(dy)).sqrt() as i32)
        .collect();
    let new_top = top - m;
    let mut out: Vec<Option<(i32, i32)>> = vec![None; rows.len() + 2 * m as usize];
    for (i, row) in rows.iter().enumerate() {
        let Some((x1, x2)) = *row else {
            continue;
        };
        for dy in -m..=m {
            let dx = reach[dy.unsigned_abs() as usize];
            let j = (i as i32 + dy + m) as usize;
            out[j] = Some(match out[j] {
                None => (x1 - dx, x2 + dx),
                Some((a, b)) => (a.min(x1 - dx), b.max(x2 + dx)),
            });
        }
    }
    (new_top, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(geometry: ShapeGeometry, margin: f32) -> FloatShape {
        FloatShape {
            geometry,
            margin,
            offset: Point::ZERO,
            reference_height: 100.0,
        }
    }

    #[test]
    fn a_circle_excludes_its_chord_at_each_line() {
        let s = shape(
            ShapeGeometry::Ellipse {
                center: Point::new(50.0, 50.0),
                rx: 50.0,
                ry: 50.0,
            },
            0.0,
        );
        let mb = Rect::new(0.0, 0.0, 100.0, 100.0);
        // across the middle: the whole diameter
        assert_eq!(s.excluded_segment(mb, 40.0, 20.0), Some((0.0, 100.0)));
        // the top line: the chord at its bottom edge (y = 10: half chord 30)
        let (l, r) = s.excluded_segment(mb, 0.0, 10.0).unwrap();
        assert!(
            (l - 20.0).abs() < 0.001 && (r - 80.0).abs() < 0.001,
            "{l} {r}"
        );
        // below the circle: nothing
        assert_eq!(s.excluded_segment(mb, 100.0, 10.0), None);
    }

    #[test]
    fn a_polygon_excludes_its_edges_between_the_lines_edges() {
        // a triangle with its right angle at the top left
        let s = shape(
            ShapeGeometry::polygon(&[vec![
                Point::new(0.0, 0.0),
                Point::new(100.0, 0.0),
                Point::new(0.0, 100.0),
            ]]),
            0.0,
        );
        let mb = Rect::new(10.0, 0.0, 100.0, 100.0);
        assert_eq!(s.excluded_segment(mb, 50.0, 10.0), Some((10.0, 60.0)));
        // the margin grows it: the hypotenuse moves out by 10 along its normal
        let m = shape(s.geometry.clone(), 10.0);
        let (_, right) = m.excluded_segment(mb, 50.0, 10.0).unwrap();
        assert!(
            (right - (10.0 + 50.0 + 10.0 * 2f32.sqrt())).abs() < 0.01,
            "{right}"
        );
    }

    #[test]
    fn a_rounded_inset_follows_its_corners_and_margins_round_square_corners() {
        let s = shape(
            ShapeGeometry::Rounded {
                rect: Rect::new(10.0, 10.0, 80.0, 80.0),
                radii: Radii::default(),
            },
            0.0,
        );
        let mb = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(s.excluded_segment(mb, 20.0, 10.0), Some((10.0, 90.0)));
        assert_eq!(s.excluded_segment(mb, 0.0, 5.0), None);
        // with a margin of 10 the corner is a quarter circle around (10, 10)
        let m = shape(s.geometry.clone(), 10.0);
        let (l, r) = m.excluded_segment(mb, 0.0, 0.0).unwrap();
        assert_eq!((l, r), (10.0, 90.0));
        let (l, _) = m.excluded_segment(mb, 5.0, 0.0).unwrap();
        assert!((l - (10.0 - 75f32.sqrt())).abs() < 0.01, "{l}");
    }

    #[test]
    fn raster_rows_grow_by_the_margin() {
        let (top, rows) = raster_margin(10, &[Some((5, 6))], 2.0);
        assert_eq!(top, 8);
        assert_eq!(
            rows,
            vec![
                Some((5, 6)),
                Some((4, 7)),
                Some((3, 8)),
                Some((4, 7)),
                Some((5, 6))
            ]
        );
        // a margin whose square does not fit 32 bits
        let (top, rows) = raster_margin(0, &[Some((0, 10))], 60000.0);
        assert_eq!((top, rows[60000]), (-60000, Some((-60000, 60010))));
    }
}
