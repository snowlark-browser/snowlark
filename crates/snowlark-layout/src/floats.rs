// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Floats in a block formatting context (CSS 2 §9.5): the [`Exclusions`] a BFC accumulates (float margin
//! boxes in the BFC root's coordinates), float placement by the rules of §9.5.1, the space left beside
//! floats for line boxes and for boxes that must not overlap floats (§9.5, §9.4.1), and clearance
//! positions (§9.5.2).

use std::sync::Arc;

use snowlark_paint::Rect;

use crate::shapes::FloatShape;

/// Which side a float went to (after resolving `inline-start`/`inline-end`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FloatSide {
    Left,
    Right,
}

/// Which floats `clear` clears.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClearSide {
    Left,
    Right,
    Both,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlacedFloat {
    /// The margin box, in BFC coordinates.
    pub rect: Rect,
    pub side: FloatSide,
    /// The float area `shape-outside` gives it (CSS Shapes 1 §3), for the lines beside it; `None`
    /// is the margin box. Placing floats and boxes beside them always uses the margin box.
    pub shape: Option<Arc<FloatShape>>,
}

/// The floats placed so far in one BFC.
#[derive(Clone, Debug)]
pub(crate) struct Exclusions {
    pub floats: Vec<PlacedFloat>,
    /// No float may be placed above the top of an earlier float (rule 5).
    pub last_float_top: f32,
    /// The lowest float bottom.
    pub max_bottom: f32,
    /// How many of the floats have a shape.
    pub shapes: usize,
}

/// A horizontal band free of floats: `left..right` at the queried vertical range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Band {
    pub left: f32,
    pub right: f32,
}

impl Band {
    pub fn width(&self) -> f32 {
        (self.right - self.left).max(0.0)
    }
}

impl Exclusions {
    /// Whether a float reaches below `y`: only those can affect a box whose top is at `y` (floats
    /// wholly above it constrain neither its lines, its floats nor its clearance).
    pub fn any_below(&self, y: f32) -> bool {
        self.max_bottom > y
    }

    /// The number of floats, for rolling back a tentative layout.
    pub fn len(&self) -> usize {
        self.floats.len()
    }

    /// Drops floats added after `len`.
    pub fn truncate(&mut self, len: usize) {
        self.floats.truncate(len);
        self.last_float_top = self
            .floats
            .iter()
            .map(|f| f.rect.y)
            .fold(f32::NEG_INFINITY, f32::max)
            .max(f32::NEG_INFINITY);
        if self.floats.is_empty() {
            self.last_float_top = f32::NEG_INFINITY;
        }
        self.shapes = self.floats.iter().filter(|f| f.shape.is_some()).count();
        self.max_bottom = self
            .floats
            .iter()
            .map(|f| f.rect.bottom())
            .fold(f32::NEG_INFINITY, f32::max);
    }

    /// Adds a float placed elsewhere (a cached subtree's floats).
    pub fn push(&mut self, f: PlacedFloat) {
        self.last_float_top = self.last_float_top.max(f.rect.y);
        self.max_bottom = self.max_bottom.max(f.rect.bottom());
        self.shapes += usize::from(f.shape.is_some());
        self.floats.push(f);
    }

    pub fn new() -> Exclusions {
        Exclusions {
            floats: Vec::new(),
            last_float_top: f32::NEG_INFINITY,
            max_bottom: f32::NEG_INFINITY,
            shapes: 0,
        }
    }

    /// Whether a float has a shape: lines then take the space beside each float's float area at
    /// their own height ([`Exclusions::line_band`]).
    pub fn has_shapes(&self) -> bool {
        self.shapes > 0
    }

    /// Gives the float placed last the float area `shape`.
    pub fn set_last_shape(&mut self, shape: FloatShape) {
        if let Some(f) = self.floats.last_mut() {
            if f.shape.is_none() {
                self.shapes += 1;
            }
            f.shape = Some(Arc::new(shape));
        }
    }

    /// The space for a line from `top` `height` tall between `left` and `right` (CSS Shapes 1 §3):
    /// each float the line overlaps excludes its float area at the line's height, its shape's
    /// interval there or else its margin box; a float whose shape the line passes by excludes
    /// nothing. The line's end edge never comes before its start edge.
    pub fn line_band(&self, top: f32, height: f32, left: f32, right: f32) -> Band {
        let mut band = Band { left, right };
        for f in &self.floats {
            if !(top < f.rect.bottom() && top + height > f.rect.y)
                || f.rect.height <= 0.0 && f.rect.width <= 0.0
            {
                continue;
            }
            let (x1, x2) = match &f.shape {
                None => (f.rect.x, f.rect.right()),
                Some(s) => match s.excluded_segment(f.rect, top, height) {
                    Some(x) => x,
                    None => continue,
                },
            };
            match f.side {
                FloatSide::Left => band.left = band.left.max(x2),
                FloatSide::Right => band.right = band.right.min(x1),
            }
        }
        // (as beside margin boxes: a float wider than the containing block pushes the line past it)
        band.right = band.right.max(band.left);
        band
    }

    /// Whether a line at `y` is beside a float with a shape, within its margin box's height (a
    /// line that fits nowhere there moves down a pixel at a time, not below the whole float).
    pub fn beside_shapes(&self, y: f32) -> bool {
        self.floats
            .iter()
            .any(|f| f.shape.is_some() && f.rect.y <= y && y < f.rect.bottom())
    }

    /// The free band between `left` and `right` over `top..top+height` (a zero height means the line
    /// at `top`).
    pub fn band(&self, top: f32, height: f32, left: f32, right: f32) -> Band {
        let bottom = top + height.max(0.0);
        let mut band = Band { left, right };
        for f in &self.floats {
            let (ft, fb) = (f.rect.y, f.rect.bottom());
            // A float overlaps the range when it starts before its end and ends after its start; a
            // zero-height query is the line at `top`. Empty floats exclude nothing.
            let overlaps = if height > 0.0 {
                ft < bottom && fb > top
            } else {
                ft <= top && fb > top
            };
            if !overlaps || f.rect.height <= 0.0 && f.rect.width <= 0.0 {
                continue;
            }
            match f.side {
                FloatSide::Left => band.left = band.left.max(f.rect.right()),
                FloatSide::Right => band.right = band.right.min(f.rect.x),
            }
        }
        band
    }

    /// The lowest float bottom strictly below `y` that overlaps a band starting at `y`, i.e. the next
    /// position where the free space can change.
    pub fn next_edge_below(&self, y: f32) -> Option<f32> {
        self.floats
            .iter()
            .map(|f| f.rect.bottom())
            .filter(|b| *b > y)
            .fold(None, |acc: Option<f32>, b| {
                Some(acc.map_or(b, |a| a.min(b)))
            })
    }

    /// Where a box with `clear` must put its top border edge: below the bottom outer edges of the
    /// floats it clears. `None` when nothing is cleared.
    pub fn clearance_y(&self, clear: ClearSide) -> Option<f32> {
        self.floats
            .iter()
            .filter(|f| match clear {
                ClearSide::Both => true,
                ClearSide::Left => f.side == FloatSide::Left,
                ClearSide::Right => f.side == FloatSide::Right,
            })
            .map(|f| f.rect.bottom())
            .fold(None, |acc: Option<f32>, b| {
                Some(acc.map_or(b, |a| a.max(b)))
            })
    }

    /// The bottom of the lowest float (a BFC root's auto height includes floats, CSS 2 §10.6.7).
    pub fn bottom(&self) -> Option<f32> {
        self.clearance_y(ClearSide::Both)
    }

    /// Places a float whose margin box is `width` × `height` (CSS 2 §9.5.1): as high as possible but
    /// not above `min_top` (the current line or block position) nor above earlier floats, then as far
    /// to its side as possible, within `left..right` (its containing block's content edges). Returns
    /// the margin box.
    pub fn place(
        &mut self,
        side: FloatSide,
        width: f32,
        height: f32,
        min_top: f32,
        left: f32,
        right: f32,
    ) -> Rect {
        let mut y = min_top.max(self.last_float_top);
        loop {
            let band = self.band(y, height.max(0.0), left, right);
            // A float wider than its containing block is placed at the first position where no other
            // float is in the way (rule 8: as high as possible beats fitting).
            let fits = band.width() >= width || (band.left <= left && band.right >= right);
            if fits {
                let x = match side {
                    FloatSide::Left => band.left,
                    FloatSide::Right => band.right - width,
                };
                let rect = Rect::new(x, y, width, height.max(0.0));
                self.push(PlacedFloat {
                    rect,
                    side,
                    shape: None,
                });
                return rect;
            }
            match self.next_edge_below(y) {
                Some(next) => y = next,
                None => {
                    // Nothing below: place at y anyway.
                    let x = match side {
                        FloatSide::Left => left,
                        FloatSide::Right => right - width,
                    };
                    let rect = Rect::new(x, y, width, height.max(0.0));
                    self.push(PlacedFloat {
                        rect,
                        side,
                        shape: None,
                    });
                    return rect;
                }
            }
        }
    }

    /// The first position at or below `top` where a box `width` wide and `height` tall fits beside the
    /// floats between `left` and `right` (for boxes that must not overlap floats). Returns (y, band).
    pub fn find_space(
        &self,
        top: f32,
        width: f32,
        height: f32,
        left: f32,
        right: f32,
    ) -> (f32, Band) {
        let mut y = top;
        loop {
            let band = self.band(y, height, left, right);
            if band.width() >= width || (band.left <= left && band.right >= right) {
                return (y, band);
            }
            match self.next_edge_below(y) {
                Some(next) => y = next,
                None => return (y, Band { left, right }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_stack_sideways_then_wrap_below() {
        let mut e = Exclusions::new();
        let a = e.place(FloatSide::Left, 40.0, 20.0, 0.0, 0.0, 100.0);
        let b = e.place(FloatSide::Left, 40.0, 10.0, 0.0, 0.0, 100.0);
        // the third doesn't fit beside the first two (80 + 40 > 100): it goes below the shorter one
        let c = e.place(FloatSide::Left, 40.0, 10.0, 0.0, 0.0, 100.0);
        assert_eq!(a, Rect::new(0.0, 0.0, 40.0, 20.0));
        assert_eq!(b, Rect::new(40.0, 0.0, 40.0, 10.0));
        assert_eq!(c, Rect::new(40.0, 10.0, 40.0, 10.0));
        let r = e.place(FloatSide::Right, 20.0, 5.0, 0.0, 0.0, 100.0);
        // a right float may not be placed above the previous float's top (rule 5)
        assert_eq!(r, Rect::new(80.0, 10.0, 20.0, 5.0));
        assert_eq!(
            e.band(12.0, 0.0, 0.0, 100.0),
            Band {
                left: 80.0,
                right: 80.0
            }
        );
        assert_eq!(e.clearance_y(ClearSide::Left), Some(20.0));
        assert_eq!(e.clearance_y(ClearSide::Right), Some(15.0));
        assert_eq!(e.find_space(0.0, 50.0, 10.0, 0.0, 100.0).0, 20.0);
    }

    #[test]
    fn lines_wrap_around_a_floats_shape_at_their_own_height() {
        use crate::shapes::{FloatShape, ShapeGeometry};
        use snowlark_paint::Point;
        let mut e = Exclusions::new();
        e.place(FloatSide::Left, 100.0, 100.0, 0.0, 0.0, 300.0);
        e.set_last_shape(FloatShape {
            geometry: ShapeGeometry::Ellipse {
                center: Point::new(50.0, 50.0),
                rx: 50.0,
                ry: 50.0,
            },
            margin: 0.0,
            offset: Point::new(0.0, 0.0),
            reference_height: 100.0,
        });
        assert!(e.has_shapes());
        // across the circle's middle: its whole width
        assert_eq!(e.line_band(45.0, 10.0, 0.0, 300.0).left, 100.0);
        // a line in its lower half: the chord at the line's top, its widest there (y = 80: half
        // chord 40, so x = 90)
        let b = e.line_band(80.0, 10.0, 0.0, 300.0);
        assert!((b.left - 90.0).abs() < 0.01, "{b:?}");
        // the margin box still decides where blocks and floats go
        assert_eq!(e.band(80.0, 10.0, 0.0, 300.0).left, 100.0);
        // a line at the circle's very bottom: a sliver of it
        assert!(e.line_band(99.9, 0.0, 0.0, 300.0).left < 60.0);
        assert!(e.beside_shapes(50.0) && !e.beside_shapes(100.0));
        e.truncate(0);
        assert!(!e.has_shapes());
    }

    #[test]
    fn truncate_rolls_back() {
        let mut e = Exclusions::new();
        e.place(FloatSide::Left, 10.0, 10.0, 0.0, 0.0, 100.0);
        let n = e.len();
        e.place(FloatSide::Left, 10.0, 10.0, 50.0, 0.0, 100.0);
        e.truncate(n);
        assert_eq!(e.len(), 1);
        assert_eq!(e.last_float_top, 0.0);
    }
}
