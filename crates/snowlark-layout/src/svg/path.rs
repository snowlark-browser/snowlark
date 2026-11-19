// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Path data (SVG 2 §9.3): the `d` attribute's grammar (§9.3.9) with its error handling (§9.5.4:
//! everything up to the command holding the first error is kept), and the conversion to absolute
//! Bézier geometry with the implementation notes of §9.5 (out-of-range arc parameters, reflected
//! control points).

use kurbo::{Arc, BezPath, ParamCurveArclen, PathEl, Point, SvgArc, Vec2};

use super::parse::{fmt_f64, Scanner};

/// One path segment as written: its command letter (case gives absolute or relative) and its
/// parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathCommand {
    pub cmd: u8,
    pub args: [f64; 7],
}

impl PathCommand {
    /// The number of parameters of a command letter.
    pub fn arity(cmd: u8) -> usize {
        match cmd.to_ascii_uppercase() {
            b'Z' => 0,
            b'H' | b'V' => 1,
            b'M' | b'L' | b'T' => 2,
            b'S' | b'Q' => 4,
            b'C' => 6,
            b'A' => 7,
            _ => usize::MAX,
        }
    }

    pub fn params(&self) -> &[f64] {
        &self.args[..Self::arity(self.cmd)]
    }
}

/// Parsed path data: the segments up to the first error, and whether there was one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathData {
    pub commands: Vec<PathCommand>,
    pub error: bool,
}

impl PathData {
    /// Parses path data (§9.3.9), keeping what precedes the first error (§9.5.4).
    pub fn parse(s: &str) -> PathData {
        let mut sc = Scanner::new(s);
        let mut out = PathData::default();
        sc.skip_wsp();
        let mut first = true;
        let mut prev: Option<u8> = None;
        while !sc.at_end() {
            let c = sc.peek().unwrap();
            let cmd = if c.is_ascii_alphabetic() {
                sc.i += 1;
                if PathCommand::arity(c) == usize::MAX {
                    out.error = true;
                    return out;
                }
                c
            } else {
                // An implicit repeat of the previous command (a moveto repeats as lineto); none
                // after a closepath.
                match prev {
                    Some(p) if !p.eq_ignore_ascii_case(&b'Z') => match p {
                        b'M' => b'L',
                        b'm' => b'l',
                        p => p,
                    },
                    _ => {
                        out.error = true;
                        return out;
                    }
                }
            };
            if first && !cmd.eq_ignore_ascii_case(&b'M') {
                out.error = true;
                return out;
            }
            first = false;
            let n = PathCommand::arity(cmd);
            let mut args = [0.0; 7];
            sc.skip_wsp();
            for (k, slot) in args.iter_mut().enumerate().take(n) {
                if k > 0 {
                    sc.skip_comma_wsp();
                }
                let v = if cmd.eq_ignore_ascii_case(&b'A') && (k == 3 || k == 4) {
                    sc.flag().map(|f| if f { 1.0 } else { 0.0 })
                } else {
                    sc.number()
                };
                match v {
                    Some(v) => *slot = v,
                    None => {
                        out.error = true;
                        return out;
                    }
                }
            }
            out.commands.push(PathCommand { cmd, args });
            prev = Some(cmd);
            if n == 0 {
                sc.skip_wsp();
            } else {
                // (a comma may follow a segment's parameters only when more parameters follow)
                let comma = sc.skip_comma_wsp();
                if comma && (sc.at_end() || sc.peek().is_some_and(|b| b.is_ascii_alphabetic())) {
                    out.error = true;
                    return out;
                }
            }
        }
        out
    }

    /// Serializes the segments as written.
    pub fn to_css(&self) -> String {
        let mut s = String::new();
        for c in &self.commands {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push(c.cmd as char);
            for v in c.params() {
                s.push(' ');
                s.push_str(&fmt_f64(*v));
            }
        }
        s
    }

    /// The absolute geometry: lines and cubic and quadratic Béziers, arcs as cubics (§9.5.1).
    pub fn to_bezpath(&self) -> BezPath {
        self.geometry().0
    }

    /// The segments reduced to absolute M, L, C and Z (SVG Paths, `getPathData({normalize: true})`).
    pub fn normalized(&self) -> Vec<PathCommand> {
        let command = |cmd: u8, a: &[f64]| {
            let mut args = [0.0; 7];
            args[..a.len()].copy_from_slice(a);
            PathCommand { cmd, args }
        };
        let path = self.to_bezpath();
        let mut cur = Point::ZERO;
        let mut start = Point::ZERO;
        let mut out = Vec::with_capacity(path.elements().len());
        for e in path.elements() {
            match *e {
                PathEl::MoveTo(p) => {
                    out.push(command(b'M', &[p.x, p.y]));
                    cur = p;
                    start = p;
                }
                PathEl::LineTo(p) => {
                    out.push(command(b'L', &[p.x, p.y]));
                    cur = p;
                }
                PathEl::QuadTo(c, p) => {
                    let c1 = cur + (c - cur) * (2.0 / 3.0);
                    let c2 = p + (c - p) * (2.0 / 3.0);
                    out.push(command(b'C', &[c1.x, c1.y, c2.x, c2.y, p.x, p.y]));
                    cur = p;
                }
                PathEl::CurveTo(a, b, p) => {
                    out.push(command(b'C', &[a.x, a.y, b.x, b.y, p.x, p.y]));
                    cur = p;
                }
                PathEl::ClosePath => {
                    out.push(command(b'Z', &[]));
                    cur = start;
                }
            }
        }
        out
    }

    /// The segment at `distance` along the path, as written (`getPathSegmentAtLength`): the first
    /// that reaches it, a moveto only at the very start; the last when the path is shorter.
    pub fn segment_at_length(&self, distance: f64) -> Option<&PathCommand> {
        let (path, ends) = self.geometry();
        let mut along = 0.0;
        let mut element = 0;
        let mut last = None;
        for (i, c) in self.commands.iter().enumerate() {
            while element < ends[i] {
                element += 1;
                if element > 1 {
                    if let Some(seg) = path.get_seg(element - 1) {
                        along += seg.arclen(1e-6);
                    }
                }
            }
            if i > 0 && c.cmd.eq_ignore_ascii_case(&b'M') {
                continue;
            }
            last = Some(c);
            if distance <= along {
                return last;
            }
        }
        last
    }

    /// The vertices markers go on (SVG 2 §13.7.3): the first of the path data, the end of each segment
    /// and each moveto; a closed subpath's last vertex is its first. With each, the angle (radians)
    /// §13.7.4 orients a marker at: the path direction at the ends of an open subpath, else half way
    /// between the direction arriving and the direction leaving.
    pub fn marker_vertices(&self) -> Vec<(Point, f64)> {
        struct Vertex {
            at: Point,
            arrive: Option<Vec2>,
            leave: Option<Vec2>,
        }
        let unit = |v: Vec2| (v.hypot() > 0.0).then(|| v.normalize());
        // (the first control point that is not the end point gives a curve's direction)
        let tangents = |seg: kurbo::PathSeg| -> (Option<Vec2>, Option<Vec2>) {
            let c = seg.to_cubic();
            let first = [c.p1 - c.p0, c.p2 - c.p0, c.p3 - c.p0];
            let last = [c.p3 - c.p2, c.p3 - c.p1, c.p3 - c.p0];
            (
                first.into_iter().find_map(unit),
                last.into_iter().find_map(unit),
            )
        };
        let (path, ends) = self.geometry();
        let mut out: Vec<Vertex> = Vec::new();
        let mut first = 0;
        let mut element = 0;
        for (i, c) in self.commands.iter().enumerate() {
            let mut segments = Vec::new();
            let mut moved = None;
            while element < ends[i] {
                match path.elements()[element] {
                    PathEl::MoveTo(p) => moved = Some(p),
                    PathEl::ClosePath => {}
                    _ => segments.extend(path.get_seg(element)),
                }
                element += 1;
            }
            let upper = c.cmd.to_ascii_uppercase();
            if upper == b'M' {
                first = out.len();
                out.push(Vertex {
                    at: moved.unwrap_or(Point::ZERO),
                    arrive: None,
                    leave: None,
                });
            } else if upper == b'Z' {
                let Some(from) = out.last().map(|v| v.at) else {
                    continue;
                };
                let start = out[first].at;
                // (a subpath already back at its start: that vertex is the one that closes it)
                if unit(start - from).is_none() && first + 1 < out.len() {
                    let leave = out[first].leave;
                    out[first].arrive = out.last().and_then(|v| v.arrive);
                    if let Some(last) = out.last_mut() {
                        last.leave = leave;
                    }
                    first = out.len() - 1;
                    continue;
                }
                let arrive = unit(start - from).or(out.last().and_then(|v| v.arrive));
                if let Some(last) = out.last_mut() {
                    last.leave = last.leave.or(unit(start - from));
                }
                out[first].arrive = arrive;
                let leave = out[first].leave;
                out.push(Vertex {
                    at: start,
                    arrive,
                    leave,
                });
                first = out.len() - 1;
            } else if let (Some(a), Some(b)) = (segments.first(), segments.last()) {
                let (leave, _) = tangents(*a);
                let (_, arrive) = tangents(*b);
                if let Some(v) = out.last_mut() {
                    v.leave = v.leave.or(leave);
                }
                let at = b.to_cubic().p3;
                out.push(Vertex {
                    at,
                    arrive: arrive.or(out.last().and_then(|v| v.arrive)),
                    leave: None,
                });
            }
        }
        out.iter()
            .map(|v| {
                let angle = match (v.arrive, v.leave) {
                    (Some(a), Some(l)) => {
                        let (a, l) = (a.atan2(), l.atan2());
                        let a = if (a - l).abs() > std::f64::consts::PI {
                            a + std::f64::consts::TAU * (l - a).signum()
                        } else {
                            a
                        };
                        (a + l) / 2.0
                    }
                    (Some(d), None) | (None, Some(d)) => d.atan2(),
                    (None, None) => 0.0,
                };
                (v.at, angle)
            })
            .collect()
    }

    /// The geometry, and how many of its elements end each command.
    fn geometry(&self) -> (BezPath, Vec<usize>) {
        let mut p = BezPath::new();
        let mut ends = Vec::with_capacity(self.commands.len());
        let mut cur = Point::ZERO;
        let mut start = Point::ZERO;
        // The last control point of a C/S (for S) or Q/T (for T) segment.
        let mut last_cubic: Option<Point> = None;
        let mut last_quad: Option<Point> = None;
        let mut open = false;
        for c in &self.commands {
            let rel = c.cmd.is_ascii_lowercase();
            let a = &c.args;
            let pt = |x: f64, y: f64| -> Point {
                if rel {
                    Point::new(cur.x + x, cur.y + y)
                } else {
                    Point::new(x, y)
                }
            };
            let upper = c.cmd.to_ascii_uppercase();
            let mut cubic_ctrl = None;
            let mut quad_ctrl = None;
            if upper != b'M' && upper != b'Z' && !open {
                // (a drawing command after a closepath starts at the subpath's start)
                p.move_to(cur);
                open = true;
            }
            match upper {
                b'M' => {
                    cur = pt(a[0], a[1]);
                    start = cur;
                    p.move_to(cur);
                    open = true;
                }
                b'Z' => {
                    p.close_path();
                    cur = start;
                    open = false;
                }
                b'L' => {
                    cur = pt(a[0], a[1]);
                    p.line_to(cur);
                }
                b'H' => {
                    cur = Point::new(if rel { cur.x + a[0] } else { a[0] }, cur.y);
                    p.line_to(cur);
                }
                b'V' => {
                    cur = Point::new(cur.x, if rel { cur.y + a[0] } else { a[0] });
                    p.line_to(cur);
                }
                b'C' => {
                    let (c1, c2, e) = (pt(a[0], a[1]), pt(a[2], a[3]), pt(a[4], a[5]));
                    p.curve_to(c1, c2, e);
                    cubic_ctrl = Some(c2);
                    cur = e;
                }
                b'S' => {
                    let c1 = last_cubic
                        .map_or(cur, |l| Point::new(2.0 * cur.x - l.x, 2.0 * cur.y - l.y));
                    let (c2, e) = (pt(a[0], a[1]), pt(a[2], a[3]));
                    p.curve_to(c1, c2, e);
                    cubic_ctrl = Some(c2);
                    cur = e;
                }
                b'Q' => {
                    let (c1, e) = (pt(a[0], a[1]), pt(a[2], a[3]));
                    p.quad_to(c1, e);
                    quad_ctrl = Some(c1);
                    cur = e;
                }
                b'T' => {
                    let c1 =
                        last_quad.map_or(cur, |l| Point::new(2.0 * cur.x - l.x, 2.0 * cur.y - l.y));
                    let e = pt(a[0], a[1]);
                    p.quad_to(c1, e);
                    quad_ctrl = Some(c1);
                    cur = e;
                }
                b'A' => {
                    let e = pt(a[5], a[6]);
                    arc_to(&mut p, cur, e, a[0], a[1], a[2], a[3] != 0.0, a[4] != 0.0);
                    cur = e;
                }
                _ => {}
            }
            last_cubic = cubic_ctrl;
            last_quad = quad_ctrl;
            ends.push(p.elements().len());
        }
        (p, ends)
    }
}

/// An elliptical arc from `from` to `to` (§9.5.1: an arc to the current point is omitted, a zero
/// radius makes a line, negative radii are made positive, too small radii are scaled up).
#[allow(clippy::too_many_arguments)]
pub fn arc_to(
    p: &mut BezPath,
    from: Point,
    to: Point,
    rx: f64,
    ry: f64,
    rotation_deg: f64,
    large: bool,
    sweep: bool,
) {
    if from == to {
        return;
    }
    if rx == 0.0 || ry == 0.0 {
        p.line_to(to);
        return;
    }
    let arc = SvgArc {
        from,
        to,
        radii: Vec2::new(rx.abs(), ry.abs()),
        x_rotation: rotation_deg.to_radians(),
        large_arc: large,
        sweep,
    };
    match Arc::from_svg_arc(&arc) {
        Some(a) => a.to_cubic_beziers(0.01, |c1, c2, e| p.curve_to(c1, c2, e)),
        None => p.line_to(to),
    }
}
