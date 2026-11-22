// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Scroll Snap 1 §6 "choosing snap positions": given a scroll snap container's snapport, its
//! snap areas and a scroll's intent, the position each axis snaps to.
//!
//! Everything is in the container's scrolled content coordinates at scroll position 0, so a snap
//! position is a scroll position. Alignments are physical here (`Start` is the left or top edge):
//! the caller resolves `block`/`inline` and the writing mode.

use snowlark_paint::{Point, Rect};

/// The axes a container snaps on (`scroll-snap-type`'s axis, made physical).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapAxis {
    X,
    Y,
    Both,
}

/// `scroll-snap-type`'s strictness (§5.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strictness {
    /// The container rests on a snap position whenever it has one.
    Mandatory,
    /// It snaps only to a position near where the scroll would end.
    Proximity,
}

/// A snap area's alignment on one physical axis (§5.2.2 `scroll-snap-align`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    None,
    /// The area's left (top) edge on the snapport's.
    Start,
    Center,
    /// The area's right (bottom) edge on the snapport's.
    End,
}

/// `scroll-snap-type`'s axis as written (§5.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeAxis {
    X,
    Y,
    Block,
    Inline,
    Both,
}

/// How the snap container's writing mode lays its axes out (the block and inline axes of
/// `scroll-snap-type` and `scroll-snap-align` are the container's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Axes {
    /// The inline axis is horizontal.
    pub horizontal: bool,
    /// The block axis runs right to left (`vertical-rl`).
    pub block_reversed: bool,
    /// The inline axis runs right to left or bottom to top.
    pub inline_reversed: bool,
}

impl TypeAxis {
    pub fn physical(self, axes: Axes) -> SnapAxis {
        match (self, axes.horizontal) {
            (TypeAxis::X, _) | (TypeAxis::Inline, true) | (TypeAxis::Block, false) => SnapAxis::X,
            (TypeAxis::Y, _) | (TypeAxis::Block, true) | (TypeAxis::Inline, false) => SnapAxis::Y,
            (TypeAxis::Both, _) => SnapAxis::Both,
        }
    }
}

impl Align {
    fn reversed(self, r: bool) -> Align {
        match (self, r) {
            (Align::Start, true) => Align::End,
            (Align::End, true) => Align::Start,
            (a, _) => a,
        }
    }

    /// `scroll-snap-align`'s (block, inline) values as (x, y) alignments.
    pub fn physical(block: Align, inline: Align, axes: Axes) -> (Align, Align) {
        let b = block.reversed(axes.block_reversed);
        let i = inline.reversed(axes.inline_reversed);
        if axes.horizontal {
            (i, b)
        } else {
            (b, i)
        }
    }
}

/// One snap area (§5.2.1: a box's border box outset by its `scroll-margin`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapArea {
    pub rect: Rect,
    pub align_x: Align,
    pub align_y: Align,
    /// `scroll-snap-stop: always` (§5.2.3): a directional scroll may not pass it.
    pub stop_always: bool,
}

/// A scroll snap container (§5.1).
#[derive(Clone, Debug, PartialEq)]
pub struct SnapContainer {
    pub axis: SnapAxis,
    pub strictness: Strictness,
    /// The snapport at scroll position 0: the scrollport inset by `scroll-padding`.
    pub snapport: Rect,
    /// The scroll range.
    pub min: Point,
    pub max: Point,
    /// In tree order.
    pub areas: Vec<SnapArea>,
}

/// What a scroll means to do (§6.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Intent {
    /// An intended end position only (`scrollTo()`, `scrollTop`, `scrollIntoView()`): the snap
    /// position nearest `to` on each axis that scrolled.
    End { to: Point, x: bool, y: bool },
    /// An intended direction only (an arrow key): the nearest snap position past `from` in the
    /// direction of `step`.
    Direction { from: Point, step: Point },
    /// A direction and a distance (a wheel notch, `scrollBy()`): a snap position past `from` in the
    /// direction of `delta`, the one nearest `from + delta`.
    Displacement { from: Point, delta: Point },
}

/// The part of the snapport a proximity container snaps within: a third of its size, as browsers
/// use.
const PROXIMITY_RATIO: f32 = 1.0 / 3.0;

/// How far past the start a directional snap position must be (below this it is where the scroll
/// starts).
const EPSILON: f32 = 0.5;

#[derive(Clone, Copy, PartialEq)]
enum Ax {
    X,
    Y,
}

fn get(p: Point, a: Ax) -> f32 {
    match a {
        Ax::X => p.x,
        Ax::Y => p.y,
    }
}

/// A rect's (start, end) on an axis.
fn span(r: Rect, a: Ax) -> (f32, f32) {
    match a {
        Ax::X => (r.x, r.x + r.width),
        Ax::Y => (r.y, r.y + r.height),
    }
}

/// One axis of an [`Intent`].
#[derive(Clone, Copy)]
struct AxisIntent {
    /// Where the scroll starts.
    from: f32,
    /// Where the scroll would end without snapping.
    intended: f32,
    /// What distances are measured from.
    base: f32,
    /// The direction (0: none, an end position).
    dir: f32,
    /// Only `scroll-snap-stop: always` areas.
    stops_only: bool,
}

/// One candidate snap position.
#[derive(Clone, Copy)]
struct Candidate {
    pos: f32,
    area: usize,
}

impl SnapContainer {
    /// Whether the container snaps on `a`.
    fn snaps_on(&self, a: Ax) -> bool {
        matches!(
            (self.axis, a),
            (SnapAxis::Both, _) | (SnapAxis::X, Ax::X) | (SnapAxis::Y, Ax::Y)
        )
    }

    /// The snap positions a scroll with `intent` comes to rest at: `None` on an axis that does not
    /// snap (the scroll ends where it would without snapping).
    pub fn snap(&self, intent: Intent) -> (Option<f32>, Option<f32>) {
        let (from, intended, base, step, x, y) = match intent {
            Intent::End { to, x, y } => (to, to, to, Point::ZERO, x, y),
            Intent::Direction { from, step } => (
                from,
                Point::new(from.x + step.x, from.y + step.y),
                from,
                step,
                step.x != 0.0,
                step.y != 0.0,
            ),
            Intent::Displacement { from, delta } => {
                let to = Point::new(from.x + delta.x, from.y + delta.y);
                (from, to, to, delta, delta.x != 0.0, delta.y != 0.0)
            }
        };
        // (the cross axis position each axis's search sees: where the scroll ends there)
        let cross = Point::new(
            intended.x.clamp(self.min.x, self.max.x),
            intended.y.clamp(self.min.y, self.max.y),
        );
        let axis = |a: Ax, on: bool| -> Option<f32> {
            if !on || !self.snaps_on(a) {
                return None;
            }
            let ai = AxisIntent {
                from: get(from, a),
                intended: get(intended, a),
                base: get(base, a),
                dir: get(step, a).signum() * f32::from(get(step, a) != 0.0),
                stops_only: false,
            };
            self.search(a, ai, cross)
        };
        (axis(Ax::X, x), axis(Ax::Y, y))
    }

    /// §6.2 on one axis: the best snap position for `ai`, with `scroll-snap-stop` and, for a
    /// mandatory container, a position in any direction when none lies in the scroll's.
    fn search(&self, a: Ax, ai: AxisIntent, cross: Point) -> Option<f32> {
        let mut found = self.closest(a, ai, cross);
        if ai.dir != 0.0 {
            if let Some(r) = found {
                // (§5.2.3: a directional scroll stops at the first `always` area it would pass)
                let stops = AxisIntent {
                    base: ai.from,
                    stops_only: true,
                    ..ai
                };
                if let Some(s) = self.closest(a, stops, cross) {
                    if (s - ai.from).abs() < (r - ai.from).abs() {
                        found = Some(s);
                    }
                }
            } else if self.strictness == Strictness::Mandatory {
                let relaxed = AxisIntent {
                    intended: ai.from,
                    base: ai.from,
                    dir: 0.0,
                    ..ai
                };
                found = self.closest(a, relaxed, cross);
            }
        }
        found
    }

    /// The candidate nearest `ai.base` that is valid for `ai`, preferring one that keeps an area
    /// larger than the snapport covering it at the intended position (§6.2 "the snapport is
    /// covered").
    fn closest(&self, a: Ax, ai: AxisIntent, cross: Point) -> Option<f32> {
        let (min, max) = (get(self.min, a), get(self.max, a));
        let (port_start, port_end) = span(self.snapport, a);
        let port = port_end - port_start;
        let limit = match self.strictness {
            Strictness::Mandatory => f32::INFINITY,
            Strictness::Proximity => port * PROXIMITY_RATIO,
        };
        let cross_axis = if a == Ax::X { Ax::Y } else { Ax::X };
        let (cp_start, cp_end) = span(self.snapport, cross_axis);
        let cross_at = get(cross, cross_axis);
        let valid = |pos: f32| -> bool {
            if (pos - ai.base).abs() > limit {
                return false;
            }
            let d = pos - ai.from;
            ai.dir == 0.0 || (d * ai.dir > EPSILON)
        };
        let mut best: Option<Candidate> = None;
        let mut covering: Option<Candidate> = None;
        let consider = |c: Candidate, best: &mut Option<Candidate>| {
            if !valid(c.pos) {
                return;
            }
            let d = (c.pos - ai.base).abs();
            // (the first in tree order wins a tie)
            if best.is_none_or(|b| d < (b.pos - ai.base).abs()) {
                *best = Some(c);
            }
        };
        for (i, area) in self.areas.iter().enumerate() {
            let align = if a == Ax::X {
                area.align_x
            } else {
                area.align_y
            };
            if align == Align::None || (ai.stops_only && !area.stop_always) {
                continue;
            }
            // (only areas the snapport shows on the cross axis: §6.2 "visible")
            let (c_start, c_end) = span(area.rect, cross_axis);
            if c_end <= cp_start + cross_at || c_start >= cp_end + cross_at {
                continue;
            }
            let (s, e) = span(area.rect, a);
            let aligned = match align {
                Align::Start => s - port_start,
                Align::End => e - port_end,
                _ => (s + e) / 2.0 - (port_start + port_end) / 2.0,
            }
            .clamp(min, max);
            consider(
                Candidate {
                    pos: aligned,
                    area: i,
                },
                &mut best,
            );
            // An area larger than the snapport covers it at every position between its edges'
            // alignments.
            if e - s > port {
                let lo = (s - port_start).clamp(min, max);
                let hi = (e - port_end).clamp(min, max);
                if ai.intended > lo && ai.intended < hi && valid(ai.intended) {
                    covering.get_or_insert(Candidate {
                        pos: ai.intended,
                        area: i,
                    });
                } else {
                    consider(Candidate { pos: lo, area: i }, &mut best);
                    consider(Candidate { pos: hi, area: i }, &mut best);
                }
            }
        }
        match (best, covering) {
            (Some(b), Some(c)) => {
                // A directional scroll takes an aligned position it reaches before the covering
                // one; otherwise the covering area keeps the intended position.
                if b.area != c.area && ai.dir != 0.0 && (b.pos - c.pos) * ai.dir < 0.0 {
                    Some(b.pos)
                } else {
                    Some(c.pos)
                }
            }
            (b, c) => b.or(c).map(|c| c.pos),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A horizontal carousel: a 300 px snapport, five 300 px slides aligned at `align`.
    fn carousel(strictness: Strictness, align: Align) -> SnapContainer {
        SnapContainer {
            axis: SnapAxis::X,
            strictness,
            snapport: Rect::new(0.0, 0.0, 300.0, 100.0),
            min: Point::ZERO,
            max: Point::new(1200.0, 0.0),
            areas: (0..5)
                .map(|i| SnapArea {
                    rect: Rect::new(i as f32 * 300.0, 0.0, 300.0, 100.0),
                    align_x: align,
                    align_y: Align::None,
                    stop_always: false,
                })
                .collect(),
        }
    }

    fn end(x: f32) -> Intent {
        Intent::End {
            to: Point::new(x, 0.0),
            x: true,
            y: false,
        }
    }

    #[test]
    fn an_end_position_snaps_to_the_nearest_area() {
        let c = carousel(Strictness::Mandatory, Align::Start);
        assert_eq!(c.snap(end(140.0)), (Some(0.0), None));
        assert_eq!(c.snap(end(160.0)), (Some(300.0), None));
        assert_eq!(c.snap(end(5000.0)), (Some(1200.0), None));
        // (the axis that does not snap is left alone)
        let y = Intent::End {
            to: Point::new(10.0, 40.0),
            x: false,
            y: true,
        };
        assert_eq!(c.snap(y), (None, None));
    }

    #[test]
    fn center_and_end_alignment() {
        let mut c = carousel(Strictness::Mandatory, Align::Center);
        c.snapport = Rect::new(0.0, 0.0, 400.0, 100.0);
        // (slide 1 is 300..600: its center 450 on the snapport's 200)
        assert_eq!(c.snap(end(240.0)), (Some(250.0), None));
        for a in &mut c.areas {
            a.align_x = Align::End;
        }
        // (slide 1's right edge 600 on the snapport's 400)
        assert_eq!(c.snap(end(190.0)), (Some(200.0), None));
    }

    #[test]
    fn a_wheel_notch_goes_to_the_next_area_in_its_direction() {
        let c = carousel(Strictness::Mandatory, Align::Start);
        let notch = |from: f32, dx: f32| Intent::Displacement {
            from: Point::new(from, 0.0),
            delta: Point::new(dx, 0.0),
        };
        // (100 px from 0 is nearer 0, but 0 is not in the direction of the scroll)
        assert_eq!(c.snap(notch(0.0, 100.0)), (Some(300.0), None));
        assert_eq!(c.snap(notch(300.0, -100.0)), (Some(0.0), None));
        assert_eq!(c.snap(notch(0.0, 700.0)), (Some(600.0), None));
        // (at the end, a mandatory container stays on its last position)
        assert_eq!(c.snap(notch(1200.0, 100.0)), (Some(1200.0), None));
        let key = Intent::Direction {
            from: Point::new(300.0, 0.0),
            step: Point::new(40.0, 0.0),
        };
        assert_eq!(c.snap(key), (Some(600.0), None));
    }

    #[test]
    fn proximity_snaps_only_near_the_end_position() {
        let c = carousel(Strictness::Proximity, Align::Start);
        // (a third of the 300 px snapport: within 100 px)
        assert_eq!(c.snap(end(90.0)), (Some(0.0), None));
        assert_eq!(c.snap(end(150.0)), (None, None));
        assert_eq!(c.snap(end(210.0)), (Some(300.0), None));
    }

    #[test]
    fn snap_stop_always_is_not_passed() {
        let mut c = carousel(Strictness::Mandatory, Align::Start);
        c.areas[1].stop_always = true;
        let fling = Intent::Displacement {
            from: Point::ZERO,
            delta: Point::new(900.0, 0.0),
        };
        assert_eq!(c.snap(fling), (Some(300.0), None));
        // (an end position is not a directional scroll: it may land past it)
        assert_eq!(c.snap(end(900.0)), (Some(900.0), None));
    }

    #[test]
    fn an_area_larger_than_the_snapport_scrolls_freely_inside() {
        let c = SnapContainer {
            axis: SnapAxis::Y,
            strictness: Strictness::Mandatory,
            snapport: Rect::new(0.0, 0.0, 100.0, 200.0),
            min: Point::ZERO,
            max: Point::new(0.0, 1000.0),
            areas: vec![
                SnapArea {
                    rect: Rect::new(0.0, 0.0, 100.0, 200.0),
                    align_x: Align::None,
                    align_y: Align::Start,
                    stop_always: false,
                },
                // (800 px tall: covers the snapport from 200 to 800)
                SnapArea {
                    rect: Rect::new(0.0, 200.0, 100.0, 800.0),
                    align_x: Align::None,
                    align_y: Align::Start,
                    stop_always: false,
                },
            ],
        };
        let to = |y: f32| Intent::End {
            to: Point::new(0.0, y),
            x: false,
            y: true,
        };
        assert_eq!(c.snap(to(500.0)), (None, Some(500.0)));
        assert_eq!(c.snap(to(90.0)), (None, Some(0.0)));
        assert_eq!(c.snap(to(950.0)), (None, Some(800.0)));
    }

    #[test]
    fn areas_out_of_view_on_the_cross_axis_are_not_snapped_to() {
        let mut c = carousel(Strictness::Mandatory, Align::Start);
        c.axis = SnapAxis::Both;
        c.max = Point::new(1200.0, 500.0);
        // (slide 1 is moved below the snapport's view at y 0)
        c.areas[1].rect.y = 300.0;
        assert_eq!(c.snap(end(280.0)), (Some(0.0), None));
    }
}
