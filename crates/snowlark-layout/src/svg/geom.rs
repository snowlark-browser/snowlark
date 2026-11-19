// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Path geometry the SVG DOM asks about (SVG 2 §4.4.3 `SVGGeometryElement`, §9.6 distance along a
//! path): total length, the point at a distance, fill and stroke containment, bounding boxes.

use kurbo::{
    BezPath, Cap, Join, ParamCurve, ParamCurveArclen, PathEl, PathSeg, Point, Rect, Shape, Stroke,
    StrokeOpts,
};

/// A 2D affine matrix `[a b c d e f]`.
pub type Matrix = kurbo::Affine;

/// The accuracy of arc length computations, in user units.
const ARCLEN_ACCURACY: f64 = 1e-4;

/// The stroke properties that shape a stroke (SVG 2 §13.5 "computing the shape of the stroke").
#[derive(Clone, Debug, PartialEq)]
pub struct StrokeStyle {
    pub width: f64,
    /// `butt`, `round`, `square`.
    pub cap: Cap,
    /// `miter`, `round`, `bevel` (`miter-clip` and `arcs` as `miter`).
    pub join: Join,
    pub miter_limit: f64,
    /// The dash array in user units (empty: solid) and the offset.
    pub dashes: Vec<f64>,
    pub dash_offset: f64,
}

impl Default for StrokeStyle {
    fn default() -> StrokeStyle {
        StrokeStyle {
            width: 1.0,
            cap: Cap::Butt,
            join: Join::Miter,
            miter_limit: 4.0,
            dashes: Vec::new(),
            dash_offset: 0.0,
        }
    }
}

impl StrokeStyle {
    fn kurbo(&self) -> Stroke {
        let mut s = Stroke::new(self.width)
            .with_caps(self.cap)
            .with_join(self.join)
            .with_miter_limit(self.miter_limit);
        // (an all-zero or odd-length-doubled pattern per §13.5.6; a zero sum is solid)
        let sum: f64 = self.dashes.iter().sum();
        if !self.dashes.is_empty() && sum > 0.0 && self.dashes.iter().all(|d| *d >= 0.0) {
            let mut pattern = self.dashes.clone();
            if pattern.len() % 2 == 1 {
                pattern.extend_from_slice(&self.dashes);
            }
            s = s.with_dashes(self.dash_offset, pattern);
        }
        s
    }

    /// The outline of `path`'s stroke as a fill.
    pub fn outline(&self, path: &BezPath) -> BezPath {
        kurbo::stroke(path.iter(), &self.kurbo(), &StrokeOpts::default(), 0.01)
    }
}

/// The length of each segment of `path`.
fn segment_lengths(path: &BezPath) -> Vec<(PathSeg, f64)> {
    path.segments()
        .map(|s| (s, s.arclen(ARCLEN_ACCURACY)))
        .collect()
}

/// The total length of a path (SVG 2 §4.4.3 `getTotalLength`).
pub fn total_length(path: &BezPath) -> f64 {
    // (from +0: an empty sum is -0.0)
    path.segments()
        .map(|s| s.arclen(ARCLEN_ACCURACY))
        .fold(0.0, |a, b| a + b)
}

/// The point at `distance` along a path, the distance clamped to `[0, length]` (§4.4.3
/// `getPointAtLength`). A path with no segments is at its first moveto (or the origin).
pub fn point_at_length(path: &BezPath, distance: f64) -> Point {
    let segs = segment_lengths(path);
    let total: f64 = segs.iter().map(|s| s.1).sum();
    let mut d = distance.clamp(0.0, total.max(0.0));
    if segs.is_empty() {
        return path
            .elements()
            .iter()
            .find_map(|e| match e {
                PathEl::MoveTo(p) => Some(*p),
                _ => None,
            })
            .unwrap_or(Point::ZERO);
    }
    for (i, (seg, len)) in segs.iter().enumerate() {
        if d <= *len || i == segs.len() - 1 {
            if *len <= 0.0 {
                return seg.eval(0.0);
            }
            let t = seg.inv_arclen(d.min(*len), ARCLEN_ACCURACY);
            return seg.eval(t);
        }
        d -= len;
    }
    Point::ZERO
}

/// The direction (degrees) of the path at `distance`, for text on a path and markers.
pub fn angle_at_length(path: &BezPath, distance: f64) -> f64 {
    use kurbo::ParamCurveDeriv;
    let segs = segment_lengths(path);
    let mut d = distance.max(0.0);
    for (i, (seg, len)) in segs.iter().enumerate() {
        if d < *len || i == segs.len() - 1 {
            let t = if *len > 0.0 {
                seg.inv_arclen(d.min(*len), ARCLEN_ACCURACY)
            } else {
                0.0
            };
            let v = match seg {
                PathSeg::Line(l) => l.p1 - l.p0,
                PathSeg::Quad(q) => q.deriv().eval(t).to_vec2(),
                PathSeg::Cubic(c) => c.deriv().eval(t).to_vec2(),
            };
            return v.y.atan2(v.x).to_degrees();
        }
        d -= len;
    }
    0.0
}

/// Whether `p` is inside `path` filled with the rule given (open subpaths implicitly closed, points
/// on the path inside).
pub fn contains_fill(path: &BezPath, p: Point, evenodd: bool) -> bool {
    if !p.x.is_finite() || !p.y.is_finite() {
        return false;
    }
    let w = closed(path).winding(p);
    let inside = if evenodd { w % 2 != 0 } else { w != 0 };
    inside || on_path(path, p, 1e-6)
}

/// The path with every open subpath closed (fill areas close them implicitly).
fn closed(path: &BezPath) -> BezPath {
    let mut out = BezPath::new();
    let mut open = false;
    for el in path.elements() {
        match el {
            PathEl::MoveTo(_) => {
                if open {
                    out.close_path();
                }
                open = false;
            }
            PathEl::ClosePath => open = false,
            _ => open = true,
        }
        out.push(*el);
    }
    if open {
        out.close_path();
    }
    out
}

/// Whether `p` is within `tolerance` of the path's outline.
fn on_path(path: &BezPath, p: Point, tolerance: f64) -> bool {
    use kurbo::ParamCurveNearest;
    let mut start = None;
    let mut cur = Point::ZERO;
    for el in path.elements() {
        match el {
            PathEl::MoveTo(q) => {
                start = Some(*q);
                cur = *q;
                if (*q - p).hypot() <= tolerance {
                    return true;
                }
            }
            PathEl::ClosePath => {
                if let Some(s) = start {
                    let l = kurbo::Line::new(cur, s);
                    if l.nearest(p, tolerance).distance_sq <= tolerance * tolerance {
                        return true;
                    }
                    cur = s;
                }
            }
            _ => {}
        }
        if let PathEl::LineTo(q) | PathEl::QuadTo(_, q) | PathEl::CurveTo(_, _, q) = el {
            cur = *q;
        }
    }
    path.segments()
        .any(|s| s.nearest(p, tolerance).distance_sq <= tolerance * tolerance)
}

/// Whether `p` is in or on the outline of the stroke of `path` with `style`.
pub fn contains_stroke(path: &BezPath, p: Point, style: &StrokeStyle) -> bool {
    if !p.x.is_finite() || !p.y.is_finite() || style.width <= 0.0 {
        return false;
    }
    let outline = style.outline(path);
    outline.winding(p) != 0 || on_path(&outline, p, 1e-3)
}

/// The tight bounding box of `path` transformed by `m` (`None` for a path without points).
pub fn bounds(path: &BezPath, m: Matrix) -> Option<Rect> {
    if path.elements().is_empty() {
        return None;
    }
    let t = m * path.clone();
    let mut r: Option<Rect> = None;
    // (moveto-only subpaths count: a path of one point has an empty box there; a moveto right
    // before another one is replaced by it)
    let els = t.elements();
    for (i, el) in els.iter().enumerate() {
        if let PathEl::MoveTo(q) = el {
            if matches!(els.get(i + 1), Some(PathEl::MoveTo(_))) {
                continue;
            }
            r = Some(r.map_or(Rect::from_points(*q, *q), |r| r.union_pt(*q)));
        }
    }
    for s in t.segments() {
        let b = s.bounding_box();
        r = Some(r.map_or(b, |r| r.union(b)));
    }
    r
}

/// The boxes of the caps of zero-length subpaths, which `round` and `square` caps paint (SVG 2
/// §9.5.3), transformed by `m`.
pub fn zero_length_caps(path: &BezPath, style: &StrokeStyle, m: Matrix) -> Option<Rect> {
    if style.cap == Cap::Butt || style.width <= 0.0 {
        return None;
    }
    let h = style.width / 2.0;
    let mut out = None;
    // The current subpath: its start, whether it has a segment and whether all are zero length.
    let mut sub: Option<(Point, bool, bool)> = None;
    let flush = |sub: Option<(Point, bool, bool)>, out: &mut Option<Rect>| {
        if let Some((p, true, true)) = sub {
            let r = Rect::new(p.x - h, p.y - h, p.x + h, p.y + h);
            *out = union(*out, Some(m.transform_rect_bbox(r)));
        }
    };
    for el in path.elements() {
        match el {
            PathEl::MoveTo(p) => {
                flush(sub, &mut out);
                sub = Some((*p, false, true));
            }
            PathEl::LineTo(q) | PathEl::QuadTo(_, q) | PathEl::CurveTo(_, _, q) => {
                if let Some((p, _, zero)) = sub {
                    sub = Some((p, true, zero && *q == p));
                }
            }
            PathEl::ClosePath => {
                if let Some((p, _, zero)) = sub {
                    sub = Some((p, true, zero));
                }
            }
        }
    }
    flush(sub, &mut out);
    out
}

/// Unites two optional boxes.
pub fn union(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// The box of a rectangle transformed by `m`.
pub fn transform_rect(r: Rect, m: Matrix) -> Rect {
    m.transform_rect_bbox(r)
}
