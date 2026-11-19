// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Motion Path Level 1: the transform `offset-path`, `offset-distance`, `offset-rotate`,
//! `offset-anchor` and `offset-position` give a box, applied after layout between `scale` and
//! `transform` (CSS Transforms 2 §4). Layout's fragments give the box's containing block
//! ([`ls::ContainingBoxes`]); everything else here is geometry.
//!
//! A path is flattened to a polyline, measured by arc length and read at `offset-distance`. Points
//! are relative to the box's own origin: `path()` is drawn from the box's position, shapes and
//! `<coord-box>` in the reference box, a `ray()` from its starting position.

use snowlark_layout::style as ls;
use snowlark_paint::{Matrix, Point, Rect};
use style::properties::ComputedValues;
use style::values::computed as c;
use style::values::computed::Length;
use style::values::generics::motion::{GenericOffsetPath, GenericOffsetPosition, RaySize};
use style::values::generics::position::GenericPositionOrAuto;

/// How many straight pieces a curve is measured by.
const CURVE_STEPS: usize = 128;

/// A flattened path: its points, and whether it ends where it started by closing.
#[derive(Debug, Default, PartialEq)]
struct Polyline {
    points: Vec<(f32, f32)>,
    closed: bool,
}

impl Polyline {
    fn length(&self) -> f32 {
        self.points
            .windows(2)
            .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1))
            .sum()
    }

    /// The point `d` along the path and the direction there (radians clockwise from +x).
    fn at(&self, d: f32) -> Option<((f32, f32), f32)> {
        let first = *self.points.first()?;
        let segments: Vec<Segment> = self
            .points
            .windows(2)
            .filter_map(|w| {
                let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
                let len = dx.hypot(dy);
                (len > 0.0).then_some((w[0], (dx, dy), len))
            })
            .collect();
        let angle = |i: usize| {
            let (dx, dy) = segments[i].1;
            dy.atan2(dx)
        };
        let mut left = d.max(0.0);
        for (i, &(from, (dx, dy), len)) in segments.iter().enumerate() {
            if left <= len || i + 1 == segments.len() {
                let along = left.min(len);
                let t = along / len;
                let a = angle(i);
                // A curve's direction turns along each of its chords (a corner's does not).
                let before = i
                    .checked_sub(1)
                    .or(self.closed.then(|| segments.len() - 1))
                    .map_or(0.0, |p| smooth_turn(angle(p), a));
                let after = (i + 1 < segments.len())
                    .then_some(i + 1)
                    .or(self.closed.then_some(0))
                    .map_or(0.0, |n| smooth_turn(a, angle(n)));
                let turn = -before / 2.0 * (1.0 - t) + after / 2.0 * t;
                let point = (from.0 + dx * along / len, from.1 + dy * along / len);
                return Some((point, a + turn));
            }
            left -= len;
        }
        Some((first, 0.0))
    }

    fn translated(mut self, x: f32, y: f32) -> Polyline {
        for p in &mut self.points {
            p.0 += x;
            p.1 += y;
        }
        self
    }
}

/// A chord of a [`Polyline`]: where it starts, its vector and its length.
type Segment = ((f32, f32), (f32, f32), f32);

/// The turn from direction `a` to `b` where a flattened curve makes it, 0 at a corner.
fn smooth_turn(a: f32, b: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let turn = (b - a + PI).rem_euclid(TAU) - PI;
    if turn.abs() < 0.1 {
        turn
    } else {
        0.0
    }
}

/// SVG path data as its commands and their numbers.
fn path_commands(d: &str) -> Vec<(char, Vec<f32>)> {
    let mut numbers: Vec<f32> = Vec::new();
    let mut chars = d.char_indices().peekable();
    let mut commands: Vec<(char, Vec<f32>)> = Vec::new();
    let mut command = None;
    while let Some((i, ch)) = chars.next() {
        if ch.is_ascii_alphabetic() && ch != 'e' && ch != 'E' {
            if let Some(c) = command.take() {
                commands.push((c, std::mem::take(&mut numbers)));
            }
            command = Some(ch);
        } else if ch.is_ascii_digit() || matches!(ch, '-' | '+' | '.') {
            let mut end = i + ch.len_utf8();
            let mut seen_dot = ch == '.';
            while let Some(&(j, n)) = chars.peek() {
                let exponent = matches!(n, 'e' | 'E');
                let sign =
                    matches!(n, '-' | '+') && matches!(d[..j].chars().last(), Some('e' | 'E'));
                if n.is_ascii_digit() || (n == '.' && !seen_dot) || exponent || sign {
                    seen_dot |= n == '.';
                    end = j + n.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            numbers.extend(d[i..end].parse::<f32>().ok());
        }
    }
    if let Some(c) = command {
        commands.push((c, numbers));
    }
    commands
}

/// Path data with its bearing commands (`B`, `b`: SVG Paths' PathDataBearingCommands) worked out: what
/// follows a bearing and is relative goes along it. `None` when there are none.
pub(crate) fn without_bearings(d: &str) -> Option<String> {
    use std::fmt::Write;
    if !d.contains(['B', 'b']) {
        return None;
    }
    let mut bearing = 0.0f32;
    let mut out = String::new();
    for (cmd, args) in path_commands(d) {
        let (s, c) = bearing.to_radians().sin_cos();
        let turned = |x: f32, y: f32| (x * c - y * s, x * s + y * c);
        match cmd {
            'B' => bearing = args.last().copied().unwrap_or(bearing),
            'b' => bearing += args.iter().sum::<f32>(),
            'h' | 'v' => {
                for &n in &args {
                    let (x, y) = if cmd == 'h' {
                        turned(n, 0.0)
                    } else {
                        turned(0.0, n)
                    };
                    write!(out, " l {x} {y}").ok()?;
                }
            }
            'm' | 'l' | 't' | 'c' | 's' | 'q' => {
                write!(out, " {cmd}").ok()?;
                for p in args.as_chunks::<2>().0 {
                    let (x, y) = turned(p[0], p[1]);
                    write!(out, " {x} {y}").ok()?;
                }
            }
            'a' => {
                for p in args.as_chunks::<7>().0 {
                    let (x, y) = turned(p[5], p[6]);
                    let rotation = p[2] + bearing;
                    write!(
                        out,
                        " a {} {} {rotation} {} {} {x} {y}",
                        p[0], p[1], p[3], p[4]
                    )
                    .ok()?;
                }
            }
            _ => {
                write!(out, " {cmd}").ok()?;
                for n in &args {
                    write!(out, " {n}").ok()?;
                }
            }
        }
    }
    Some(out)
}

/// Flattens SVG path data (`M L H V C S Q T A Z`, absolute or relative: SVG 2 §9.3) to a polyline.
fn flatten_path(d: &str) -> Polyline {
    let mut out = Polyline::default();
    let commands = path_commands(d);
    let (mut x, mut y) = (0.0f32, 0.0f32);
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    // (the last control point, for S and T)
    let mut control: Option<(f32, f32)> = None;
    let cubic =
        |out: &mut Polyline, p0: (f32, f32), c1: (f32, f32), c2: (f32, f32), p: (f32, f32)| {
            for k in 1..=CURVE_STEPS {
                let t = k as f32 / CURVE_STEPS as f32;
                let u = 1.0 - t;
                let (a, b, c, e) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                out.points.push((
                    a * p0.0 + b * c1.0 + c * c2.0 + e * p.0,
                    a * p0.1 + b * c1.1 + c * c2.1 + e * p.1,
                ));
            }
        };
    for (cmd, args) in commands {
        let rel = cmd.is_ascii_lowercase();
        let (ox, oy) = (x, y);
        let origin = |px: f32, py: f32| {
            (
                if rel { ox + px } else { px },
                if rel { oy + py } else { py },
            )
        };
        out.closed = false;
        match cmd.to_ascii_uppercase() {
            'M' | 'L' | 'T' => {
                for (i, p) in args.as_chunks::<2>().0.iter().enumerate() {
                    let (bx, by) = if rel { (x, y) } else { (0.0, 0.0) };
                    let (nx, ny) = (bx + p[0], by + p[1]);
                    if cmd.eq_ignore_ascii_case(&'M') && i == 0 {
                        // (a later subpath continues the measure from where it starts)
                        out.points.push((nx, ny));
                        (sx, sy) = (nx, ny);
                    } else if cmd.eq_ignore_ascii_case(&'T') {
                        let c = control.map_or((x, y), |c| (2.0 * x - c.0, 2.0 * y - c.1));
                        let c1 = (x + 2.0 / 3.0 * (c.0 - x), y + 2.0 / 3.0 * (c.1 - y));
                        let c2 = (nx + 2.0 / 3.0 * (c.0 - nx), ny + 2.0 / 3.0 * (c.1 - ny));
                        cubic(&mut out, (x, y), c1, c2, (nx, ny));
                        control = Some(c);
                        (x, y) = (nx, ny);
                        continue;
                    } else {
                        out.points.push((nx, ny));
                    }
                    (x, y) = (nx, ny);
                }
                if !cmd.eq_ignore_ascii_case(&'T') {
                    control = None;
                }
            }
            'H' => {
                for v in &args {
                    x = if rel { x + v } else { *v };
                    out.points.push((x, y));
                }
                control = None;
            }
            'V' => {
                for v in &args {
                    y = if rel { y + v } else { *v };
                    out.points.push((x, y));
                }
                control = None;
            }
            'C' | 'S' => {
                let smooth = cmd.eq_ignore_ascii_case(&'S');
                for p in args.chunks_exact(if smooth { 4 } else { 6 }) {
                    let (bx, by) = if rel { (x, y) } else { (0.0, 0.0) };
                    let (c1, rest) = if smooth {
                        (
                            control.map_or((x, y), |c| (2.0 * x - c.0, 2.0 * y - c.1)),
                            p,
                        )
                    } else {
                        ((bx + p[0], by + p[1]), &p[2..])
                    };
                    let c2 = (bx + rest[0], by + rest[1]);
                    let end = (bx + rest[2], by + rest[3]);
                    cubic(&mut out, (x, y), c1, c2, end);
                    control = Some(c2);
                    (x, y) = end;
                }
            }
            'Q' => {
                for p in args.as_chunks::<4>().0 {
                    let (bx, by) = if rel { (x, y) } else { (0.0, 0.0) };
                    let c = (bx + p[0], by + p[1]);
                    let end = (bx + p[2], by + p[3]);
                    let c1 = (x + 2.0 / 3.0 * (c.0 - x), y + 2.0 / 3.0 * (c.1 - y));
                    let c2 = (
                        end.0 + 2.0 / 3.0 * (c.0 - end.0),
                        end.1 + 2.0 / 3.0 * (c.1 - end.1),
                    );
                    cubic(&mut out, (x, y), c1, c2, end);
                    control = Some(c);
                    (x, y) = end;
                }
            }
            'A' => {
                for p in args.as_chunks::<7>().0 {
                    let (bx, by) = if rel { (x, y) } else { (0.0, 0.0) };
                    let end = (bx + p[5], by + p[6]);
                    arc(
                        &mut out,
                        (x, y),
                        (p[0], p[1]),
                        p[2],
                        p[3] != 0.0,
                        p[4] != 0.0,
                        end,
                    );
                    (x, y) = end;
                }
                control = None;
            }
            'Z' => {
                out.points.push((sx, sy));
                (x, y) = (sx, sy);
                out.closed = true;
                control = None;
            }
            _ => {}
        }
        let _ = origin;
    }
    out
}

/// An elliptical arc flattened (SVG 2 Appendix B.2.4, endpoint to center parameterization).
fn arc(
    out: &mut Polyline,
    from: (f32, f32),
    radii: (f32, f32),
    rotation: f32,
    large: bool,
    sweep: bool,
    to: (f32, f32),
) {
    let (mut rx, mut ry) = (radii.0.abs() as f64, radii.1.abs() as f64);
    if rx == 0.0 || ry == 0.0 || from == to {
        out.points.push(to);
        return;
    }
    let phi = (rotation as f64).to_radians();
    let (s, c) = phi.sin_cos();
    let (x1, y1) = (from.0 as f64, from.1 as f64);
    let (x2, y2) = (to.0 as f64, to.1 as f64);
    let (dx, dy) = ((x1 - x2) / 2.0, (y1 - y2) / 2.0);
    let (xp, yp) = (c * dx + s * dy, -s * dx + c * dy);
    let lambda = (xp * xp) / (rx * rx) + (yp * yp) / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let num = (rx * rx * ry * ry - rx * rx * yp * yp - ry * ry * xp * xp).max(0.0);
    let den = rx * rx * yp * yp + ry * ry * xp * xp;
    let mut k = if den > 0.0 { (num / den).sqrt() } else { 0.0 };
    if large == sweep {
        k = -k;
    }
    let (cxp, cyp) = (k * rx * yp / ry, -k * ry * xp / rx);
    let (cx, cy) = (
        c * cxp - s * cyp + (x1 + x2) / 2.0,
        s * cxp + c * cyp + (y1 + y2) / 2.0,
    );
    let angle = |ux: f64, uy: f64, vx: f64, vy: f64| (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
    let (ux, uy) = ((xp - cxp) / rx, (yp - cyp) / ry);
    let (vx, vy) = ((-xp - cxp) / rx, (-yp - cyp) / ry);
    let start = angle(1.0, 0.0, ux, uy);
    let mut delta = angle(ux, uy, vx, vy);
    if !sweep && delta > 0.0 {
        delta -= std::f64::consts::TAU;
    } else if sweep && delta < 0.0 {
        delta += std::f64::consts::TAU;
    }
    for i in 1..=CURVE_STEPS {
        let t = start + delta * i as f64 / CURVE_STEPS as f64;
        let (st, ct) = t.sin_cos();
        out.points.push((
            (cx + rx * ct * c - ry * st * s) as f32,
            (cy + rx * ct * s + ry * st * c) as f32,
        ));
    }
}

fn lp(v: &c::LengthPercentage, base: f32) -> f32 {
    v.resolve(Length::new(base)).px()
}

/// A rectangle with rounded corners as a closed path, clockwise from the end of the top left
/// corner (CSS Motion Path 1 §2.1, `<basic-shape>` and `<coord-box>`). `radii`: top left, top right,
/// bottom right, bottom left.
fn rounded_rect(r: Rect, radii: [(f32, f32); 4]) -> Polyline {
    let [tl, tr, br, bl] = fit_radii(r, radii);
    let mut d = format!("M {} {} H {}", r.x + tl.0, r.y, r.right() - tr.0);
    let corner = |rad: (f32, f32), x: f32, y: f32| {
        if rad.0 > 0.0 && rad.1 > 0.0 {
            format!(" A {} {} 0 0 1 {} {}", rad.0, rad.1, x, y)
        } else {
            format!(" L {x} {y}")
        }
    };
    d.push_str(&corner(tr, r.right(), r.y + tr.1));
    d.push_str(&format!(" V {}", r.bottom() - br.1));
    d.push_str(&corner(br, r.right() - br.0, r.bottom()));
    d.push_str(&format!(" H {}", r.x + bl.0));
    d.push_str(&corner(bl, r.x, r.bottom() - bl.1));
    d.push_str(&format!(" V {}", r.y + tl.1));
    d.push_str(&corner(tl, r.x + tl.0, r.y));
    d.push_str(" Z");
    flatten_path(&d)
}

/// A polygon's path with corners of radius `r` (`polygon(round r, ...)`), each radius reduced to what
/// half of the shorter neighbouring edge allows.
fn rounded_polygon(points: &[(f32, f32)], r: f32) -> String {
    use std::fmt::Write;
    let n = points.len();
    let mut d = String::new();
    for i in 0..n {
        let (p, v, q) = (points[(i + n - 1) % n], points[i], points[(i + 1) % n]);
        let (ax, ay, bx, by) = (p.0 - v.0, p.1 - v.1, q.0 - v.0, q.1 - v.1);
        let (la, lb) = (ax.hypot(ay), bx.hypot(by));
        let command = if i == 0 { 'M' } else { 'L' };
        let turn = (ax * by - ay * bx).atan2(ax * bx + ay * by).abs();
        let tangent = (turn / 2.0).tan();
        if la <= 0.0 || lb <= 0.0 || tangent <= 0.0 || !tangent.is_finite() {
            let _ = write!(d, " {command} {} {}", v.0, v.1);
            continue;
        }
        // (from the corner to where the arc touches each edge)
        let reach = (r / tangent).min(la / 2.0).min(lb / 2.0);
        let radius = reach * tangent;
        let (sx, sy) = (v.0 + ax / la * reach, v.1 + ay / la * reach);
        let (ex, ey) = (v.0 + bx / lb * reach, v.1 + by / lb * reach);
        let sweep = u8::from(ax * by - ay * bx < 0.0);
        let _ = write!(
            d,
            " {command} {sx} {sy} A {radius} {radius} 0 0 {sweep} {ex} {ey}"
        );
    }
    d.push_str(" Z");
    d
}

/// Radii scaled down so that adjacent corners do not overlap (CSS Backgrounds 3 §5.5).
fn fit_radii(r: Rect, radii: [(f32, f32); 4]) -> [(f32, f32); 4] {
    let [tl, tr, br, bl] = radii;
    let f = [
        r.width / (tl.0 + tr.0),
        r.width / (bl.0 + br.0),
        r.height / (tl.1 + bl.1),
        r.height / (tr.1 + br.1),
    ]
    .into_iter()
    .filter(|v| v.is_finite())
    .fold(1.0f32, f32::min);
    radii.map(|(x, y)| (x * f, y * f))
}

/// An ellipse as a closed path, clockwise from its rightmost point.
fn ellipse(cx: f32, cy: f32, rx: f32, ry: f32) -> Polyline {
    let mut out = Polyline {
        points: Vec::with_capacity(4 * CURVE_STEPS + 1),
        closed: true,
    };
    for i in 0..=4 * CURVE_STEPS {
        let t = std::f32::consts::TAU * i as f32 / (4 * CURVE_STEPS) as f32;
        out.points.push((cx + rx * t.cos(), cy + ry * t.sin()));
    }
    out
}

/// The radius `closest-side` or `farthest-side` gives along one axis, from `at` in `0..size`.
fn side_radius(r: &c::basic_shape::ShapeRadius, at: f32, size: f32) -> f32 {
    use style::values::generics::basic_shape::GenericShapeRadius as R;
    match r {
        R::Length(l) => lp(&l.0, size),
        R::ClosestSide | R::ClosestCorner => at.abs().min((size - at).abs()),
        R::FarthestSide | R::FarthestCorner => at.abs().max((size - at).abs()),
    }
}

/// The offset starting position in a reference box of `size` (§2.2): `auto` is where the box is
/// (`own`, from the reference box's origin), `normal` the center.
fn offset_position(values: &ComputedValues, size: (f32, f32), own: (f32, f32)) -> (f32, f32) {
    match &values.get_box().offset_position {
        GenericOffsetPosition::Position(p) => (lp(&p.horizontal, size.0), lp(&p.vertical, size.1)),
        GenericOffsetPosition::Auto => own,
        GenericOffsetPosition::Normal => (size.0 / 2.0, size.1 / 2.0),
    }
}

/// A `<basic-shape>` as a path in its reference box's coordinates, from (0, 0) to `size`.
fn shape_path(
    shape: &c::basic_shape::BasicShape,
    values: &ComputedValues,
    size: (f32, f32),
    own: (f32, f32),
) -> Option<Polyline> {
    use style::values::generics::basic_shape::{
        GenericBasicShape as S, GenericPathOrShapeFunction as P,
    };
    let (w, h) = size;
    let center = |p: &GenericPositionOrAuto<c::Position>| match p {
        GenericPositionOrAuto::Position(p) => (lp(&p.horizontal, w), lp(&p.vertical, h)),
        GenericPositionOrAuto::Auto => offset_position(values, size, own),
    };
    Some(match shape {
        S::Rect(inset) => {
            let (t, r, b, l) = (
                lp(&inset.rect.0, h),
                lp(&inset.rect.1, w),
                lp(&inset.rect.2, h),
                lp(&inset.rect.3, w),
            );
            let rect = Rect::new(l, t, (w - l - r).max(0.0), (h - t - b).max(0.0));
            let corner =
                |c: &style::values::generics::border::GenericBorderCornerRadius<
                    style::values::generics::NonNegative<c::LengthPercentage>,
                >| (lp(&c.0.width.0, rect.width), lp(&c.0.height.0, rect.height));
            rounded_rect(
                rect,
                [
                    corner(&inset.round.top_left),
                    corner(&inset.round.top_right),
                    corner(&inset.round.bottom_right),
                    corner(&inset.round.bottom_left),
                ],
            )
        }
        S::Circle(circle) => {
            let (cx, cy) = center(&circle.position);
            use style::values::generics::basic_shape::GenericShapeRadius as R;
            let r = match &circle.radius {
                R::Length(l) => lp(&l.0, (w.hypot(h)) / std::f32::consts::SQRT_2),
                R::ClosestSide => cx
                    .abs()
                    .min((w - cx).abs())
                    .min(cy.abs())
                    .min((h - cy).abs()),
                R::FarthestSide => cx
                    .abs()
                    .max((w - cx).abs())
                    .max(cy.abs())
                    .max((h - cy).abs()),
                R::ClosestCorner => cx
                    .abs()
                    .min((w - cx).abs())
                    .hypot(cy.abs().min((h - cy).abs())),
                R::FarthestCorner => cx
                    .abs()
                    .max((w - cx).abs())
                    .hypot(cy.abs().max((h - cy).abs())),
            };
            ellipse(cx, cy, r, r)
        }
        S::Ellipse(e) => {
            let (cx, cy) = center(&e.position);
            ellipse(
                cx,
                cy,
                side_radius(&e.semiaxis_x, cx, w),
                side_radius(&e.semiaxis_y, cy, h),
            )
        }
        S::Polygon(p) => {
            let mut points: Vec<(f32, f32)> = p
                .coordinates
                .iter()
                .map(|c| (lp(&c.0, w), lp(&c.1, h)))
                .collect();
            if let Some(r) = p.round.as_ref().map(|r| lp(r, 0.0)).filter(|r| *r > 0.0) {
                return Some(flatten_path(&rounded_polygon(&points, r)));
            }
            points.extend(points.first().copied());
            Polyline {
                points,
                closed: true,
            }
        }
        S::PathOrShape(P::Path(_)) => return None,
        S::PathOrShape(P::Shape(s)) => {
            let mut d = String::new();
            shape_commands(s, size, &mut d)?;
            flatten_path(&d)
        }
    })
}

/// `shape()` commands as SVG path data, its lengths resolved in a reference box of `size`.
fn shape_commands(
    s: &style::values::computed::basic_shape::Shape,
    size: (f32, f32),
    out: &mut String,
) -> Option<()> {
    use std::fmt::Write;
    use style::values::generics::basic_shape::{
        ArcSize, ArcSweep, AxisEndPoint, AxisPosition, CommandEndPoint, ControlPoint,
        ControlReference, GenericShapeCommand as C,
    };
    let (w, h) = size;
    // (the end point of a command starting at (x, y))
    let end = |p: &CommandEndPoint<c::Position, c::LengthPercentage>, x: f32, y: f32| match p {
        CommandEndPoint::ToPosition(p) => (lp(&p.horizontal, w), lp(&p.vertical, h)),
        CommandEndPoint::ByCoordinate(p) => (x + lp(&p.x, w), y + lp(&p.y, h)),
    };
    let control = |p: &ControlPoint<c::Position, c::LengthPercentage>,
                   start: (f32, f32),
                   end: (f32, f32)| match p {
        ControlPoint::Absolute(p) => (lp(&p.horizontal, w), lp(&p.vertical, h)),
        ControlPoint::Relative(r) => {
            let from = match r.reference {
                ControlReference::Start => start,
                ControlReference::End => end,
                ControlReference::Origin => (0.0, 0.0),
            };
            (from.0 + lp(&r.coord.x, w), from.1 + lp(&r.coord.y, h))
        }
    };
    let axis = |p: &AxisEndPoint<c::LengthPercentage>, at: f32, base: f32| match p {
        AxisEndPoint::ToPosition(AxisPosition::LengthPercent(l)) => lp(l, base),
        AxisEndPoint::ToPosition(AxisPosition::Keyword(k)) => k.as_percentage().0 * base,
        AxisEndPoint::ByCoordinate(l) => at + lp(l, base),
    };
    let (mut x, mut y) = (0.0f32, 0.0f32);
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    for cmd in s.commands.iter() {
        match cmd {
            C::Move { point } => {
                (x, y) = end(point, x, y);
                (sx, sy) = (x, y);
                write!(out, " M {x} {y}").ok()?;
            }
            C::Line { point } => {
                (x, y) = end(point, x, y);
                write!(out, " L {x} {y}").ok()?;
            }
            C::HLine { x: v } => {
                x = axis(v, x, w);
                write!(out, " L {x} {y}").ok()?;
            }
            C::VLine { y: v } => {
                y = axis(v, y, h);
                write!(out, " L {x} {y}").ok()?;
            }
            C::CubicCurve {
                point,
                control1,
                control2,
            } => {
                let e = end(point, x, y);
                let a = control(control1, (x, y), e);
                let b = control(control2, (x, y), e);
                write!(out, " C {} {} {} {} {} {}", a.0, a.1, b.0, b.1, e.0, e.1).ok()?;
                (x, y) = e;
            }
            C::QuadCurve { point, control1 } => {
                let e = end(point, x, y);
                let a = control(control1, (x, y), e);
                write!(out, " Q {} {} {} {}", a.0, a.1, e.0, e.1).ok()?;
                (x, y) = e;
            }
            C::SmoothCubic { point, control2 } => {
                let e = end(point, x, y);
                let b = control(control2, (x, y), e);
                write!(out, " S {} {} {} {}", b.0, b.1, e.0, e.1).ok()?;
                (x, y) = e;
            }
            C::SmoothQuad { point } => {
                (x, y) = end(point, x, y);
                write!(out, " T {x} {y}").ok()?;
            }
            C::Arc {
                point,
                radii,
                arc_sweep,
                arc_size,
                rotate,
            } => {
                let e = end(point, x, y);
                // (one radius is both, its percentages of the box's normalized diagonal)
                let (rx, ry) = match radii.ry.as_ref() {
                    Some(ry) => (lp(&radii.rx, w), lp(ry, h)),
                    None => {
                        let r = lp(&radii.rx, w.hypot(h) / std::f32::consts::SQRT_2);
                        (r, r)
                    }
                };
                let large = matches!(arc_size, ArcSize::Large) as u8;
                let sweep = matches!(arc_sweep, ArcSweep::Cw) as u8;
                let degrees = rotate.degrees();
                write!(
                    out,
                    " A {rx} {ry} {degrees} {large} {sweep} {} {}",
                    e.0, e.1
                )
                .ok()?;
                (x, y) = e;
            }
            C::Close => {
                write!(out, " Z").ok()?;
                (x, y) = (sx, sy);
            }
        }
    }
    Some(())
}

/// The computed path of `offset-path: path()`, as SVG path data.
fn svg_path_data(path: &style::values::generics::basic_shape::Path) -> Option<String> {
    let mut d = String::new();
    let mut writer = style_traits::CssWriter::new(&mut d);
    path.path.to_css(&mut writer, false).ok()?;
    Some(d)
}

/// The reference box of `coord_box` relative to the box's own origin, and the corner radii of the
/// containing block's border box scaled to it.
fn reference_box(
    coord_box: style::values::specified::motion::CoordBox,
    containing: Option<&ls::ContainingBoxes>,
    own: Rect,
) -> (Rect, [(f32, f32); 4]) {
    use style::values::specified::motion::CoordBox as B;
    let Some(c) = containing else {
        return (own, [(0.0, 0.0); 4]);
    };
    let (r, inset) = match coord_box {
        B::ContentBox => (
            c.content,
            (c.content.x - c.border.x, c.content.y - c.border.y),
        ),
        B::PaddingBox => (
            c.padding,
            (c.padding.x - c.border.x, c.padding.y - c.border.y),
        ),
        _ => (c.border, (0.0, 0.0)),
    };
    (
        r,
        c.radii
            .map(|(x, y)| ((x - inset.0).max(0.0), (y - inset.1).max(0.0))),
    )
}

/// The length of a `ray()` from `start` in a box of `size` (CSS Motion Path 1 §2.1.1).
fn ray_length(size: RaySize, angle: f32, start: (f32, f32), box_size: (f32, f32)) -> f32 {
    let (x, y) = start;
    let (w, h) = box_size;
    let sides = [x, w - x, y, h - y].map(f32::abs);
    let corners = [
        x.hypot(y),
        (w - x).hypot(y),
        x.hypot(h - y),
        (w - x).hypot(h - y),
    ];
    match size {
        RaySize::ClosestSide => sides.into_iter().fold(f32::INFINITY, f32::min),
        RaySize::FarthestSide => sides.into_iter().fold(0.0, f32::max),
        RaySize::ClosestCorner => corners.into_iter().fold(f32::INFINITY, f32::min),
        RaySize::FarthestCorner => corners.into_iter().fold(0.0, f32::max),
        RaySize::Sides => {
            // (to where the ray leaves the box; zero from outside it)
            if x < 0.0 || y < 0.0 || x > w || y > h {
                return 0.0;
            }
            let (s, c) = angle.sin_cos();
            let (dx, dy) = (s, -c);
            let tx = if dx > 1e-6 {
                (w - x) / dx
            } else if dx < -1e-6 {
                -x / dx
            } else {
                f32::INFINITY
            };
            let ty = if dy > 1e-6 {
                (h - y) / dy
            } else if dy < -1e-6 {
                -y / dy
            } else {
                f32::INFINITY
            };
            tx.min(ty)
        }
    }
}

/// The ID `offset-path: url()` names.
pub(crate) fn url_fragment(values: &ComputedValues) -> Option<String> {
    use style::values::computed::motion::OffsetPathFunction as F;
    let GenericOffsetPath::OffsetPath { path, .. } = &values.get_box().offset_path else {
        return None;
    };
    let F::Url(url) = &**path else {
        return None;
    };
    Some(url.url()?.fragment()?.to_owned())
}

/// Whether the box's `offset-path` is worked out in its containing block: all but `none` and
/// `path()`, which starts from the box's own position.
pub(crate) fn needs_containing(values: &ComputedValues) -> bool {
    use style::values::computed::motion::OffsetPathFunction as F;
    use style::values::generics::basic_shape::{
        GenericBasicShape as S, GenericPathOrShapeFunction as P,
    };
    match &values.get_box().offset_path {
        GenericOffsetPath::None => false,
        GenericOffsetPath::OffsetPath { path, .. } => {
            !matches!(&**path, F::Shape(S::PathOrShape(P::Path(_))))
        }
        GenericOffsetPath::CoordBox(_) => true,
    }
}

/// The `offset-*` transform of a box, as a matrix about `origin` (its transform origin, in the same
/// coordinates as `own`, its box; `containing` is what its containing block's boxes are from it).
/// `None` without an `offset-path` or for one that cannot be drawn.
pub(crate) fn offset_matrix(
    values: &ComputedValues,
    own: Rect,
    origin: Point,
    containing: Option<&ls::ContainingBoxes>,
    url_path: Option<&str>,
) -> Option<Matrix> {
    use style::values::computed::motion::OffsetPathFunction as F;
    let b = values.get_box();
    let (function, coord_box) = match &b.offset_path {
        GenericOffsetPath::None => return None,
        GenericOffsetPath::OffsetPath { path, coord_box } => (Some(&**path), *coord_box),
        GenericOffsetPath::CoordBox(coord_box) => (None, *coord_box),
    };
    let (reference, radii) = reference_box(coord_box, containing, own);
    let size = (reference.width, reference.height);
    let at = (own.x - reference.x, own.y - reference.y);
    // offset-anchor: `auto` is the point `transform-origin` indicates (§2.4)
    let anchor = match &b.offset_anchor {
        GenericPositionOrAuto::Position(p) => (
            own.x + lp(&p.horizontal, own.width),
            own.y + lp(&p.vertical, own.height),
        ),
        GenericPositionOrAuto::Auto => (origin.x, origin.y),
    };
    let rotate = &b.offset_rotate;
    let extra = rotate.angle.radians();
    let (point, rotation) = match function {
        Some(F::Ray(ray)) => {
            let angle = ray.angle.radians();
            let start = match &ray.position {
                GenericPositionOrAuto::Position(p) => {
                    (lp(&p.horizontal, size.0), lp(&p.vertical, size.1))
                }
                GenericPositionOrAuto::Auto => offset_position(values, size, at),
            };
            let mut length = ray_length(ray.size, angle, start, size);
            // `contain`: shorter by half the box's larger side, floored at zero (§2.1.1)
            if ray.contain {
                length = (length - own.width.max(own.height) / 2.0).max(0.0);
            }
            // (added in degrees: a turn that comes to a round angle is that angle exactly)
            let rotation = if rotate.auto {
                (ray.angle.degrees() - 90.0 + rotate.angle.degrees()).to_radians()
            } else {
                extra
            };
            let mut distance = lp(&b.offset_distance, length);
            if ray.contain {
                distance = distance.clamp(-length, length);
            }
            let (s, c) = angle.sin_cos();
            (
                (
                    reference.x + start.0 + distance * s,
                    reference.y + start.1 - distance * c,
                ),
                rotation,
            )
        }
        path => {
            use style::values::generics::basic_shape::{
                GenericBasicShape as S, GenericPathOrShapeFunction as P,
            };
            let polyline = match path {
                // path(): drawn from the box's own position
                Some(F::Shape(S::PathOrShape(P::Path(p)))) => flatten_path(&svg_path_data(p)?),
                // url(): the shape's user coordinates are the reference box's; a reference to
                // anything else is path("m 0 0")
                Some(F::Url(_)) => match url_path {
                    Some(d) => flatten_path(d).translated(reference.x, reference.y),
                    None => flatten_path("m 0 0"),
                },
                Some(F::Shape(shape)) => {
                    shape_path(shape, values, size, at)?.translated(reference.x, reference.y)
                }
                // a <coord-box> alone: the box with the containing block's corners
                _ => rounded_rect(reference, radii),
            };
            let length = polyline.length();
            let d = lp(&b.offset_distance, length);
            let d = if polyline.closed && length > 0.0 {
                d.rem_euclid(length)
            } else {
                d.clamp(0.0, length.max(0.0))
            };
            let (p, tangent) = polyline.at(d)?;
            (p, if rotate.auto { tangent + extra } else { extra })
        }
    };
    // about the transform origin: the anchor goes to the point, turned around the anchor
    let shift = (anchor.0 - origin.x, anchor.1 - origin.y);
    Some(
        Matrix::translation(-shift.0, -shift.1)
            .then(&Matrix::rotation(rotation))
            .then(&Matrix::translation(
                point.0 - anchor.0 + shift.0,
                point.1 - anchor.1 + shift.1,
            )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_measured_along_their_length() {
        let p = flatten_path("m 0 0 h 200 v 150 z");
        assert!(p.closed);
        assert!((p.length() - 600.0).abs() < 1e-3);
        assert_eq!(p.at(120.0), Some(((120.0, 0.0), 0.0)));
        let ((x, y), a) = p.at(250.0).unwrap();
        assert_eq!((x, y), (200.0, 50.0));
        assert!((a - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    }

    #[test]
    fn curves_and_relative_commands_flatten() {
        let p = flatten_path("M10,10C10,10,20,10,30,10");
        assert!((p.length() - 20.0).abs() < 1e-2);
        assert_eq!(p.points.last(), Some(&(30.0, 10.0)));
        // a half circle of radius 10
        let p = flatten_path("M 0 0 A 10 10 0 0 1 20 0");
        assert!((p.length() - std::f32::consts::PI * 10.0).abs() < 0.05);
        let p = flatten_path("M0-1.5e1l10,0");
        assert_eq!(p.points, vec![(0.0, -15.0), (10.0, -15.0)]);
    }

    #[test]
    fn shapes_start_where_the_tests_expect() {
        // a rectangle from its top left corner, clockwise
        let p = rounded_rect(Rect::new(10.0, 10.0, 300.0, 200.0), [(0.0, 0.0); 4]);
        assert!((p.length() - 1000.0).abs() < 1e-2);
        let ((x, y), _) = p.at(400.0).unwrap();
        assert!(
            (x - 310.0).abs() < 1e-3 && (y - 110.0).abs() < 1e-3,
            "{x} {y}"
        );
        // a circle-like rounded rectangle from the top: a quarter of the way is its right side
        let p = rounded_rect(Rect::new(0.0, 0.0, 420.0, 420.0), [(210.0, 210.0); 4]);
        let ((x, y), _) = p.at(p.length() / 4.0).unwrap();
        assert!(
            (x - 420.0).abs() < 0.5 && (y - 210.0).abs() < 0.5,
            "{x} {y}"
        );
        // an ellipse from its rightmost point: a quarter of the way is its bottom
        let p = ellipse(300.0, 200.0, 200.0, 200.0);
        let ((x, y), a) = p.at(p.length() / 4.0).unwrap();
        assert!(
            (x - 300.0).abs() < 0.5 && (y - 400.0).abs() < 0.5,
            "{x} {y}"
        );
        assert!((a.to_degrees().abs() - 180.0).abs() < 2.0, "{a}");
    }

    #[test]
    fn contained_rays_are_shorter_by_half_the_larger_side() {
        // (`ray_length` is the whole ray; `contain` takes half of 100 off what it gives)
        let length = ray_length(RaySize::ClosestSide, 0.0, (150.0, 150.0), (300.0, 300.0));
        assert_eq!((length - 100.0_f32.max(40.0) / 2.0).max(0.0), 100.0);
    }

    #[test]
    fn bearings_turn_what_follows() {
        let d = without_bearings("M 20 150 B -90 h 120 B 0 h 140 b 90 h 120 z").unwrap();
        let p = flatten_path(&d);
        assert!(p.closed);
        let near =
            |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3;
        assert!(near(p.points[1], (20.0, 30.0)), "{:?}", p.points);
        assert!(near(p.points[2], (160.0, 30.0)), "{:?}", p.points);
        assert!(near(p.points[3], (160.0, 150.0)), "{:?}", p.points);
        assert_eq!(without_bearings("M 0 0 h 10"), None);
    }

    #[test]
    fn ray_sizes() {
        let s = (200.0, 100.0);
        assert_eq!(ray_length(RaySize::ClosestSide, 0.0, (50.0, 50.0), s), 50.0);
        assert_eq!(
            ray_length(RaySize::FarthestSide, 0.0, (50.0, 50.0), s),
            150.0
        );
        let sides = ray_length(RaySize::Sides, std::f32::consts::FRAC_PI_2, (50.0, 50.0), s);
        assert!((sides - 150.0).abs() < 1e-3);
    }
}
