// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `clip-path` (CSS Masking 1 §5, §6): a box's clipping path as geometry in the painter's
//! coordinates, the one form the painter and the hit test both use, so that what shows is what
//! takes the pointer. Curves are flattened here, against the reference box of the fragment.

use std::f32::consts::{PI, TAU};

use snowlark_paint::{Matrix, Point, Radii, Rect, Size};

use crate::fragment::{BoxFragment, Fragment, ReferenceBoxes};
use crate::paint::{inset_sides, radii};
use crate::style::*;

/// One clip a box's content is clipped to, in the coordinates the box is painted in.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ClipGeometry {
    /// A rectangle with rounded corners (a box alone, or `inset()`): clipped as one, so its corners
    /// are arcs and not segments.
    Rounded(Rect, Radii),
    /// A path: its contours (each closed by its last point joining its first) and whether it fills
    /// by the even-odd rule. With no contour it clips everything away.
    Path {
        contours: Vec<Vec<Point>>,
        even_odd: bool,
    },
}

/// Whether the fill of a path of `contours` has `p`, by the non-zero or the even-odd rule.
pub fn path_contains(contours: &[Vec<Point>], even_odd: bool, p: Point) -> bool {
    let (mut winding, mut crossings) = (0i32, 0u32);
    for contour in contours {
        let Some(&last) = contour.last() else {
            continue;
        };
        let mut a = last;
        for &b in contour {
            if (a.y <= p.y) != (b.y <= p.y) {
                let x = a.x + (p.y - a.y) / (b.y - a.y) * (b.x - a.x);
                if x > p.x {
                    crossings += 1;
                    winding += if b.y > a.y { 1 } else { -1 };
                }
            }
            a = b;
        }
    }
    if even_odd {
        crossings % 2 == 1
    } else {
        winding != 0
    }
}

impl ReferenceBoxes {
    pub(crate) fn of<S: Style>(f: &Fragment<S>, b: &BoxFragment<S>, at: Point) -> ReferenceBoxes {
        let border = Rect::new(at.x, at.y, f.size.width, f.size.height);
        let padding = border.deflate(&b.border);
        ReferenceBoxes {
            border,
            padding,
            content: padding.deflate(&b.padding),
        }
    }

    pub(crate) fn translate(&self, dx: f32, dy: f32) -> ReferenceBoxes {
        ReferenceBoxes {
            border: self.border.translate(dx, dy),
            padding: self.padding.translate(dx, dy),
            content: self.content.translate(dx, dy),
        }
    }
}

/// The clips a box's content is clipped to, each inside the one before (a `<clipPath>` with a
/// `clip-path` of its own is more than one). Empty when the box has no `clip-path`. The reference
/// boxes are the fragment's own unless the fragment holds others (an inline box's later piece).
pub(crate) fn clip_geometry<S: Style>(
    f: &Fragment<S>,
    b: &BoxFragment<S>,
    at: Point,
) -> Vec<ClipGeometry> {
    let Some(clip) = b.style.clip_path() else {
        return Vec::new();
    };
    let boxes = match b.clip_reference.get() {
        Some(r) => r.translate(at.x, at.y),
        None => ReferenceBoxes::of(f, b, at),
    };
    let area = match clip.reference {
        ClipBox::Margin => margin_box(&b.style, boxes.border),
        ClipBox::Border => boxes.border,
        ClipBox::Padding => boxes.padding,
        ClipBox::Content => boxes.content,
    };
    let (w, h) = (area.width, area.height);
    let center = |c: &Option<(LengthPercentage, LengthPercentage)>| match c {
        Some((x, y)) => Point::new(area.x + x.resolve(w), area.y + y.resolve(h)),
        None => Point::new(area.x + w / 2.0, area.y + h / 2.0),
    };
    // (from a center, its distances to the sides of the box along each axis)
    let sides = |c: Point| {
        let (dx, dy) = (c.x - area.x, c.y - area.y);
        ((dx.abs(), (w - dx).abs()), (dy.abs(), (h - dy).abs()))
    };
    let one = |contour: Vec<Point>| {
        vec![ClipGeometry::Path {
            contours: vec![contour],
            even_odd: false,
        }]
    };
    match &clip.shape {
        ClipShape::Box => {
            let outer = radii(&b.style, f.size);
            let corners = match clip.reference {
                // (a corner that is rounded grows by the margins of its two sides, as the padding
                // edge's shrinks by the border's: CSS Backgrounds 3 §5.1)
                ClipBox::Margin => {
                    let grown = |c: Size, h: f32, v: f32| {
                        Size::new(
                            if c.width > 0.0 { c.width + h } else { 0.0 },
                            if c.height > 0.0 { c.height + v } else { 0.0 },
                        )
                    };
                    let (l, t) = (boxes.border.x - area.x, boxes.border.y - area.y);
                    let (r, bt) = (
                        area.right() - boxes.border.right(),
                        area.bottom() - boxes.border.bottom(),
                    );
                    Radii {
                        top_left: grown(outer.top_left, l, t),
                        top_right: grown(outer.top_right, r, t),
                        bottom_right: grown(outer.bottom_right, r, bt),
                        bottom_left: grown(outer.bottom_left, l, bt),
                    }
                }
                ClipBox::Border => outer,
                ClipBox::Padding => outer.inner(&b.border),
                ClipBox::Content => outer.inner(&inset_sides(&b.border, &b.padding)),
            };
            vec![ClipGeometry::Rounded(area, corners.fitted(&area))]
        }
        ClipShape::Inset { sides: s, radii: r } => {
            // (insets that overlap meet: nothing is left)
            let (top, right) = (s[0].resolve(h), s[1].resolve(w));
            let (bottom, left) = (s[2].resolve(h), s[3].resolve(w));
            let rect = Rect::new(
                area.x + left,
                area.y + top,
                (w - left - right).max(0.0),
                (h - top - bottom).max(0.0),
            );
            let corner = |c: &(LengthPercentage, LengthPercentage)| {
                Size::new(c.0.resolve(w).max(0.0), c.1.resolve(h).max(0.0))
            };
            let corners = Radii {
                top_left: corner(&r[0]),
                top_right: corner(&r[1]),
                bottom_right: corner(&r[2]),
                bottom_left: corner(&r[3]),
            };
            vec![ClipGeometry::Rounded(rect, corners.fitted(&rect))]
        }
        ClipShape::Circle { center: c, radius } => {
            let c = center(c);
            let ((l, r), (t, b)) = sides(c);
            let reach = match radius {
                // (percentages of the box's diagonal over the square root of two: CSS Shapes 1)
                ShapeRadius::Length(v) => v.resolve((w * w + h * h).sqrt() / 2f32.sqrt()),
                ShapeRadius::ClosestSide => l.min(r).min(t).min(b),
                ShapeRadius::FarthestSide => l.max(r).max(t).max(b),
                ShapeRadius::ClosestCorner => l.min(r).hypot(t.min(b)),
                ShapeRadius::FarthestCorner => l.max(r).hypot(t.max(b)),
            };
            one(ellipse_points(c, reach, reach))
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
                // (through the corner, keeping the sides' proportions)
                ShapeRadius::ClosestCorner => near.0.min(near.1) * 2f32.sqrt(),
                ShapeRadius::FarthestCorner => near.0.max(near.1) * 2f32.sqrt(),
            };
            one(ellipse_points(
                c,
                axis(rx, horizontal, w),
                axis(ry, vertical, h),
            ))
        }
        ClipShape::Polygon { even_odd, points } => vec![ClipGeometry::Path {
            contours: vec![points
                .iter()
                .map(|(x, y)| Point::new(area.x + x.resolve(w), area.y + y.resolve(h)))
                .collect()],
            even_odd: *even_odd,
        }],
        ClipShape::Path { even_odd, commands } => vec![ClipGeometry::Path {
            contours: flatten_path(commands, area),
            even_odd: *even_odd,
        }],
        ClipShape::Reference(r) => r
            .levels
            .iter()
            .map(|level| {
                // CSS Masking 1 §6.1: user space has its origin at the reference box's top left,
                // in CSS pixels before zoom; `objectBoundingBox` units are fractions of the box
                // (before zoom too: the element's transform is scaled with them).
                let zoom = if clip.zoom > 0.0 { clip.zoom } else { 1.0 };
                let units = if level.bounding_box_units {
                    Matrix::new(w / zoom, 0.0, 0.0, h / zoom, 0.0, 0.0)
                } else {
                    Matrix::IDENTITY
                };
                let m = units
                    .then(&level.transform)
                    .then(&Matrix::new(zoom, 0.0, 0.0, zoom, area.x, area.y));
                ClipGeometry::Path {
                    contours: level
                        .contours
                        .iter()
                        .map(|c| c.iter().map(|&p| m.apply(p)).collect())
                        .collect(),
                    even_odd: level.even_odd,
                }
            })
            .collect(),
    }
}

/// The box with its margins (CSS Masking 1 §5.1 `margin-box`). Fragments keep no used margins, so
/// they are the specified ones: a percentage of the box's own width, and `auto` as zero.
fn margin_box<S: Style>(s: &S, border_box: Rect) -> Rect {
    let m = |side| s.margin(side).resolve_or_zero(border_box.width);
    Rect::from_ltrb(
        border_box.x - m(Side::Left),
        border_box.y - m(Side::Top),
        border_box.right() + m(Side::Right),
        border_box.bottom() + m(Side::Bottom),
    )
}

/// The union of the shapes of a `<clipPath>` (CSS Masking 1 §6.1: "the union of the shapes it
/// contains") as one path filled by the non-zero rule: each shape's contours are turned so that
/// a shape's outside never cancels another's inside (an even-odd shape's contours alternate
/// with their nesting; a non-zero shape whose outer contours run against the sum is reversed
/// whole). One shape keeps its own rule.
pub fn union_of_shapes(shapes: Vec<(Vec<Vec<Point>>, bool)>) -> (Vec<Vec<Point>>, bool) {
    let mut shapes: Vec<(Vec<Vec<Point>>, bool)> = shapes
        .into_iter()
        .map(|(contours, even_odd)| {
            let contours: Vec<Vec<Point>> = contours.into_iter().filter(|c| c.len() > 2).collect();
            (contours, even_odd)
        })
        .filter(|(contours, _)| !contours.is_empty())
        .collect();
    if shapes.len() == 1 {
        return shapes.pop().unwrap();
    }
    let mut out = Vec::new();
    for (mut contours, even_odd) in shapes {
        let depth: Vec<usize> = (0..contours.len())
            .map(|i| {
                let p = contours[i][0];
                (0..contours.len())
                    .filter(|&j| j != i && path_contains(&contours[j..j + 1], true, p))
                    .count()
            })
            .collect();
        if even_odd {
            for (c, d) in contours.iter_mut().zip(&depth) {
                if (signed_area(c) < 0.0) != (d % 2 == 1) {
                    c.reverse();
                }
            }
        } else {
            let outer: f32 = contours
                .iter()
                .zip(&depth)
                .filter(|(_, &d)| d == 0)
                .map(|(c, _)| signed_area(c))
                .sum();
            if outer < 0.0 {
                for c in &mut contours {
                    c.reverse();
                }
            }
        }
        out.append(&mut contours);
    }
    (out, false)
}

/// Twice the signed area of a contour: positive when it runs with increasing angles in a y-down
/// system (clockwise on the screen).
fn signed_area(c: &[Point]) -> f32 {
    let mut sum = 0.0;
    let mut a = c[c.len() - 1];
    for &b in c {
        sum += a.x * b.y - b.x * a.y;
        a = b;
    }
    sum
}

/// An ellipse as a polygon, fine enough that its edge is within a tenth of a pixel.
fn ellipse_points(center: Point, rx: f32, ry: f32) -> Vec<Point> {
    let n = (rx.max(ry).max(0.0).sqrt() * 8.0).ceil().clamp(32.0, 256.0) as u32;
    (0..n)
        .map(|i| {
            let a = TAU * i as f32 / n as f32;
            Point::new(center.x + rx * a.cos(), center.y + ry * a.sin())
        })
        .collect()
}

/// The contours of `path()` or `shape()` against the reference box `area` (CSS Shapes 1 §3.1, the
/// path data rules of SVG 2 §9.3), curves as line segments.
fn flatten_path(commands: &[PathCommand], area: Rect) -> Vec<Vec<Point>> {
    let (w, h) = (area.width, area.height);
    let mut out: Vec<Vec<Point>> = Vec::new();
    let mut contour: Vec<Point> = Vec::new();
    let (mut at, mut start) = (Point::new(area.x, area.y), Point::new(area.x, area.y));
    // The last control point of a curve just drawn, for the smooth commands.
    let (mut cubic, mut quad): (Option<Point>, Option<Point>) = (None, None);
    let end = |p: &PathPoint, from: Point| {
        let (x, y) = (p.x.resolve(w), p.y.resolve(h));
        if p.relative {
            Point::new(from.x + x, from.y + y)
        } else {
            Point::new(area.x + x, area.y + y)
        }
    };
    let control = |c: &PathControl, from: Point, to: Point| {
        let base = match c.reference {
            PathReference::Origin => Point::new(area.x, area.y),
            PathReference::Start => from,
            PathReference::End => to,
        };
        Point::new(base.x + c.x.resolve(w), base.y + c.y.resolve(h))
    };
    for command in commands {
        // (a command after a closed contour starts the next one where it closed)
        if contour.is_empty() && !matches!(command, PathCommand::Move { .. }) {
            contour.push(at);
        }
        let (mut next_cubic, mut next_quad) = (None, None);
        match command {
            PathCommand::Move { to } => {
                if contour.len() > 1 {
                    out.push(std::mem::take(&mut contour));
                }
                contour.clear();
                at = end(to, at);
                start = at;
                contour.push(at);
            }
            PathCommand::Line { to } => {
                at = end(to, at);
                contour.push(at);
            }
            PathCommand::HLine { x, relative } => {
                let v = x.resolve(w);
                at.x = if *relative { at.x + v } else { area.x + v };
                contour.push(at);
            }
            PathCommand::VLine { y, relative } => {
                let v = y.resolve(h);
                at.y = if *relative { at.y + v } else { area.y + v };
                contour.push(at);
            }
            PathCommand::Cubic { to, c1, c2 } => {
                let e = end(to, at);
                let (a, b) = (control(c1, at, e), control(c2, at, e));
                cubic_points(&mut contour, at, a, b, e);
                next_cubic = Some(b);
                at = e;
            }
            PathCommand::SmoothCubic { to, c2 } => {
                let e = end(to, at);
                let a = cubic.map_or(at, |c| Point::new(2.0 * at.x - c.x, 2.0 * at.y - c.y));
                let b = control(c2, at, e);
                cubic_points(&mut contour, at, a, b, e);
                next_cubic = Some(b);
                at = e;
            }
            PathCommand::Quad { to, c1 } => {
                let e = end(to, at);
                let a = control(c1, at, e);
                quad_points(&mut contour, at, a, e);
                next_quad = Some(a);
                at = e;
            }
            PathCommand::SmoothQuad { to } => {
                let e = end(to, at);
                let a = quad.map_or(at, |c| Point::new(2.0 * at.x - c.x, 2.0 * at.y - c.y));
                quad_points(&mut contour, at, a, e);
                next_quad = Some(a);
                at = e;
            }
            PathCommand::Arc {
                to,
                rx,
                ry,
                rotate,
                large,
                sweep,
            } => {
                let e = end(to, at);
                let radii = (rx.resolve(w).abs(), ry.resolve(h).abs());
                arc_points(&mut contour, at, e, radii, *rotate, *large, *sweep);
                at = e;
            }
            PathCommand::Close => {
                at = start;
                if contour.len() > 1 {
                    out.push(std::mem::take(&mut contour));
                }
                contour.clear();
            }
        }
        (cubic, quad) = (next_cubic, next_quad);
    }
    if contour.len() > 1 {
        out.push(contour);
    }
    out
}

/// Line segments enough for a curve whose control polygon is `length` long.
fn segments(length: f32) -> u32 {
    (length.max(0.0).sqrt() * 3.0).ceil().clamp(4.0, 128.0) as u32
}

fn distance(a: Point, b: Point) -> f32 {
    (a.x - b.x).hypot(a.y - b.y)
}

fn cubic_points(out: &mut Vec<Point>, p0: Point, p1: Point, p2: Point, p3: Point) {
    let n = segments(distance(p0, p1) + distance(p1, p2) + distance(p2, p3));
    for i in 1..=n {
        let t = i as f32 / n as f32;
        let u = 1.0 - t;
        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        out.push(Point::new(
            a * p0.x + b * p1.x + c * p2.x + d * p3.x,
            a * p0.y + b * p1.y + c * p2.y + d * p3.y,
        ));
    }
}

fn quad_points(out: &mut Vec<Point>, p0: Point, p1: Point, p2: Point) {
    let n = segments(distance(p0, p1) + distance(p1, p2));
    for i in 1..=n {
        let t = i as f32 / n as f32;
        let u = 1.0 - t;
        let (a, b, c) = (u * u, 2.0 * u * t, t * t);
        out.push(Point::new(
            a * p0.x + b * p1.x + c * p2.x,
            a * p0.y + b * p1.y + c * p2.y,
        ));
    }
}

/// An elliptical arc from `from` to `to` (SVG 2 appendix B.2: endpoint to center parameterization,
/// out-of-range radii scaled up, a zero radius a straight line). `rotate` is in radians.
fn arc_points(
    out: &mut Vec<Point>,
    from: Point,
    to: Point,
    (rx, ry): (f32, f32),
    rotate: f32,
    large: bool,
    sweep: bool,
) {
    if distance(from, to) == 0.0 {
        return;
    }
    if rx == 0.0 || ry == 0.0 {
        out.push(to);
        return;
    }
    let (sin, cos) = rotate.sin_cos();
    let (dx, dy) = ((from.x - to.x) / 2.0, (from.y - to.y) / 2.0);
    let (x1, y1) = (cos * dx + sin * dy, -sin * dx + cos * dy);
    let scale = ((x1 / rx).powi(2) + (y1 / ry).powi(2)).max(1.0).sqrt();
    let (rx, ry) = (rx * scale, ry * scale);
    let numerator = (rx * ry).powi(2) - (rx * y1).powi(2) - (ry * x1).powi(2);
    let denominator = (rx * y1).powi(2) + (ry * x1).powi(2);
    let mut k = (numerator / denominator).max(0.0).sqrt();
    if large == sweep {
        k = -k;
    }
    let (cx1, cy1) = (k * rx * y1 / ry, -k * ry * x1 / rx);
    let center = Point::new(
        cos * cx1 - sin * cy1 + (from.x + to.x) / 2.0,
        sin * cx1 + cos * cy1 + (from.y + to.y) / 2.0,
    );
    let start = ((y1 - cy1) / ry).atan2((x1 - cx1) / rx);
    let mut delta = ((-y1 - cy1) / ry).atan2((-x1 - cx1) / rx) - start;
    if sweep && delta < 0.0 {
        delta += TAU;
    } else if !sweep && delta > 0.0 {
        delta -= TAU;
    }
    let n = ((delta.abs() / (PI / 32.0)).ceil() as u32).max(2);
    for i in 1..=n {
        let a = start + delta * i as f32 / n as f32;
        let (x, y) = (rx * a.cos(), ry * a.sin());
        out.push(Point::new(
            center.x + cos * x - sin * y,
            center.y + sin * x + cos * y,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f32, y: f32, size: f32, clockwise: bool) -> Vec<Point> {
        let mut c = vec![
            Point::new(x, y),
            Point::new(x + size, y),
            Point::new(x + size, y + size),
            Point::new(x, y + size),
        ];
        if !clockwise {
            c.reverse();
        }
        c
    }

    #[test]
    fn a_path_fills_by_its_rule_over_all_its_contours() {
        let ring = vec![square(0.0, 0.0, 30.0, true), square(10.0, 10.0, 10.0, true)];
        assert!(path_contains(&ring, false, Point::new(15.0, 15.0)));
        assert!(!path_contains(&ring, true, Point::new(15.0, 15.0)));
        assert!(path_contains(&ring, true, Point::new(5.0, 5.0)));
        assert!(!path_contains(&ring, true, Point::new(35.0, 5.0)));
        let hole = vec![
            square(0.0, 0.0, 30.0, true),
            square(10.0, 10.0, 10.0, false),
        ];
        assert!(!path_contains(&hole, false, Point::new(15.0, 15.0)));
    }

    /// Two shapes drawn against each other still fill where either does; a shape's hole stays.
    #[test]
    fn the_union_of_shapes_never_cancels() {
        let (contours, even_odd) = union_of_shapes(vec![
            (vec![square(0.0, 0.0, 20.0, true)], false),
            (vec![square(10.0, 0.0, 20.0, false)], false),
            (
                vec![
                    square(50.0, 0.0, 30.0, true),
                    square(60.0, 10.0, 10.0, true),
                ],
                true,
            ),
        ]);
        assert!(!even_odd);
        for (x, inside) in [(5.0, true), (15.0, true), (25.0, true), (35.0, false)] {
            assert_eq!(
                path_contains(&contours, even_odd, Point::new(x, 5.0)),
                inside
            );
        }
        assert!(path_contains(&contours, even_odd, Point::new(55.0, 5.0)));
        assert!(!path_contains(&contours, even_odd, Point::new(65.0, 15.0)));
        // one shape keeps its rule
        let (_, even_odd) = union_of_shapes(vec![(vec![square(0.0, 0.0, 9.0, true)], true)]);
        assert!(even_odd);
    }

    #[test]
    fn arcs_go_the_long_or_the_short_way_round() {
        let lp = LengthPercentage::px;
        let to = PathPoint {
            x: lp(100.0),
            y: lp(50.0),
            relative: false,
        };
        let arc = |large, sweep| {
            let commands = vec![
                PathCommand::Move {
                    to: PathPoint {
                        x: lp(0.0),
                        y: lp(50.0),
                        relative: false,
                    },
                },
                PathCommand::Arc {
                    to,
                    rx: lp(50.0),
                    ry: lp(50.0),
                    rotate: 0.0,
                    large,
                    sweep,
                },
                PathCommand::Close,
            ];
            flatten_path(&commands, Rect::new(0.0, 0.0, 100.0, 100.0))
        };
        // sweep: the upper half disc; the other way the lower
        assert!(path_contains(
            &arc(false, true),
            false,
            Point::new(50.0, 10.0)
        ));
        assert!(!path_contains(
            &arc(false, true),
            false,
            Point::new(50.0, 90.0)
        ));
        assert!(path_contains(
            &arc(false, false),
            false,
            Point::new(50.0, 90.0)
        ));
        assert!(!path_contains(
            &arc(false, false),
            false,
            Point::new(50.0, 10.0)
        ));
    }
}
