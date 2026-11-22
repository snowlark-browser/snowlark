// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where a glyph's outline crosses a horizontal band: what `text-decoration-skip-ink` (CSS Text
//! Decoration 4 §2.6) interrupts an underline or overline around. The outline comes from
//! DirectWrite (`GetGlyphRunOutline`) into a sink that flattens it and keeps the leftmost and
//! rightmost x of every piece of it inside the band; results are cached per glyph, size and band.

use std::cell::Cell;
use std::sync::Mutex;

use windows::core::implement;
use windows::Win32::Graphics::Direct2D::Common::{
    ID2D1SimplifiedGeometrySink, ID2D1SimplifiedGeometrySink_Impl, D2D1_BEZIER_SEGMENT,
    D2D1_FIGURE_BEGIN, D2D1_FIGURE_END, D2D1_FIGURE_END_CLOSED, D2D1_FILL_MODE, D2D1_PATH_SEGMENT,
};
use windows::Win32::Graphics::DirectWrite::IDWriteFontFace;
use windows_core::ComObject;
use windows_numerics::Vector2;

use crate::display_list::FontId;

/// The cached intercepts: (font, size, glyph, band top, band bottom) as bits → the glyph's extent
/// inside the band, if any.
type Key = (u32, u32, u16, u32, u32);

#[derive(Default)]
pub(crate) struct InkCache {
    map: Mutex<rustc_hash::FxHashMap<Key, Option<(f32, f32)>>>,
}

/// The most entries kept: the cache is emptied when it would grow past this.
const CAPACITY: usize = 1 << 16;

impl InkCache {
    pub(crate) fn intercept(
        &self,
        face: impl FnOnce() -> Option<IDWriteFontFace>,
        font: FontId,
        size: f32,
        glyph: u16,
        top: f32,
        bottom: f32,
    ) -> Option<(f32, f32)> {
        let key = (
            font.0,
            size.to_bits(),
            glyph,
            top.to_bits(),
            bottom.to_bits(),
        );
        if let Some(hit) = self.map.lock().unwrap().get(&key) {
            return *hit;
        }
        let found = face().and_then(|f| outline_intercept(&f, size, glyph, top, bottom));
        let mut map = self.map.lock().unwrap();
        if map.len() >= CAPACITY {
            map.clear();
        }
        map.insert(key, found);
        found
    }
}

/// The leftmost and rightmost x, from the glyph's origin, of the glyph's outline between `top` and
/// `bottom` (y down from the baseline), or `None` when no part of it is in that band.
fn outline_intercept(
    face: &IDWriteFontFace,
    size: f32,
    glyph: u16,
    top: f32,
    bottom: f32,
) -> Option<(f32, f32)> {
    let band = ComObject::new(BandSink::new(top, bottom));
    let sink: ID2D1SimplifiedGeometrySink = band.to_interface();
    // SAFETY: one glyph id, no advances or offsets (the glyph at the origin); the sink is a live COM
    // object for the length of the call.
    unsafe {
        face.GetGlyphRunOutline(size, &glyph, None, None, 1, false, false, &sink)
            .ok()?;
    }
    let (lo, hi) = (band.lo.get(), band.hi.get());
    (lo <= hi).then_some((lo, hi))
}

/// Flattens an outline and keeps the x extent of its parts inside `top..=bottom`.
#[implement(ID2D1SimplifiedGeometrySink)]
struct BandSink {
    top: f32,
    bottom: f32,
    start: Cell<(f32, f32)>,
    at: Cell<(f32, f32)>,
    lo: Cell<f32>,
    hi: Cell<f32>,
}

/// The pieces each Bézier segment is flattened into.
const BEZIER_STEPS: usize = 8;

impl BandSink {
    fn new(top: f32, bottom: f32) -> BandSink {
        BandSink {
            top,
            bottom,
            start: Cell::new((0.0, 0.0)),
            at: Cell::new((0.0, 0.0)),
            lo: Cell::new(f32::INFINITY),
            hi: Cell::new(f32::NEG_INFINITY),
        }
    }

    fn include(&self, x: f32) {
        self.lo.set(self.lo.get().min(x));
        self.hi.set(self.hi.get().max(x));
    }

    /// The straight segment from the current point to `to`: the x extent of its part in the band.
    fn line_to(&self, to: (f32, f32)) {
        let from = self.at.replace(to);
        let ((x0, y0), (x1, y1)) = (from, to);
        let (top, bottom) = (self.top, self.bottom);
        if (y0 < top && y1 < top) || (y0 > bottom && y1 > bottom) {
            return;
        }
        let x_at = |y: f32| x0 + (x1 - x0) * (y - y0) / (y1 - y0);
        let (a, b) = if y1 == y0 {
            (x0, x1)
        } else {
            let (ya, yb) = (y0.clamp(top, bottom), y1.clamp(top, bottom));
            (x_at(ya), x_at(yb))
        };
        self.include(a);
        self.include(b);
    }
}

impl ID2D1SimplifiedGeometrySink_Impl for BandSink_Impl {
    fn SetFillMode(&self, _mode: D2D1_FILL_MODE) {}

    fn SetSegmentFlags(&self, _flags: D2D1_PATH_SEGMENT) {}

    fn BeginFigure(&self, start: &Vector2, _begin: D2D1_FIGURE_BEGIN) {
        self.start.set((start.X, start.Y));
        self.at.set((start.X, start.Y));
    }

    fn AddLines(&self, points: *const Vector2, count: u32) {
        if points.is_null() {
            return;
        }
        // SAFETY: DirectWrite passes `count` points at `points`.
        let points = unsafe { std::slice::from_raw_parts(points, count as usize) };
        for p in points {
            self.line_to((p.X, p.Y));
        }
    }

    fn AddBeziers(&self, beziers: *const D2D1_BEZIER_SEGMENT, count: u32) {
        if beziers.is_null() {
            return;
        }
        // SAFETY: DirectWrite passes `count` segments at `beziers`.
        let beziers = unsafe { std::slice::from_raw_parts(beziers, count as usize) };
        for b in beziers {
            let p0 = self.at.get();
            let (p1, p2, p3) = (
                (b.point1.X, b.point1.Y),
                (b.point2.X, b.point2.Y),
                (b.point3.X, b.point3.Y),
            );
            for i in 1..=BEZIER_STEPS {
                let t = i as f32 / BEZIER_STEPS as f32;
                let u = 1.0 - t;
                let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                self.line_to((
                    a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
                    a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
                ));
            }
        }
    }

    fn EndFigure(&self, end: D2D1_FIGURE_END) {
        if end == D2D1_FIGURE_END_CLOSED {
            self.line_to(self.start.get());
        }
    }

    fn Close(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_counts_only_its_part_inside_the_band() {
        let s = BandSink::new(0.0, 1.0);
        s.at.set((0.0, -2.0));
        // (from y -2 to y 2 across x 0..4: inside the band from x 2 to x 3)
        s.line_to((4.0, 2.0));
        assert_eq!((s.lo.get(), s.hi.get()), (2.0, 3.0));
        // (wholly below the band: nothing more)
        s.line_to((9.0, 5.0));
        assert_eq!((s.lo.get(), s.hi.get()), (2.0, 3.0));
        // (a horizontal segment inside it: its whole length)
        s.at.set((-1.0, 0.5));
        s.line_to((1.0, 0.5));
        assert_eq!((s.lo.get(), s.hi.get()), (-1.0, 3.0));
    }
}
