// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scrolling math: smooth scroll animation ([`SmoothScroll`]), wheel and keyboard step sizes, and
//! scroll bar geometry ([`Scrollbar`]). Positions are CSS px.

use std::time::Duration;

use snowlark_paint::scroll_curve::ScrollCurve;
pub use snowlark_paint::scroll_curve::ScrollKind;
use snowlark_paint::{Point, Size};

/// Pixels per wheel notch at the default three lines per notch (what Chrome and Edge scroll).
pub const PIXELS_PER_NOTCH: f32 = 100.0;
pub use snowlark_tab::driver::{page_step, LINE_STEP};

/// Windows' "one screen per notch" wheel setting (SPI_GETWHEELSCROLLLINES = WHEEL_PAGESCROLL).
pub const WHEEL_PAGE: u32 = u32::MAX;

/// How far a wheel delta (120 per notch) scrolls, in CSS px, with `lines` lines per notch (the
/// system setting) and a `page` height for the one-screen-per-notch setting.
pub fn wheel_pixels(delta: i32, lines: u32, page: f32) -> f32 {
    let notches = delta as f32 / 120.0;
    if lines == WHEEL_PAGE {
        return notches * page;
    }
    notches * PIXELS_PER_NOTCH * lines.max(1) as f32 / 3.0
}

/// Whether a wheel delta comes from a device with fine steps (a precision touchpad, a free-spinning
/// wheel): those are applied as they come rather than animated.
pub fn is_precise(delta: i32) -> bool {
    delta % 120 != 0
}

/// A scroll position moving smoothly towards a target, clamped to `0..=max`, along a
/// [`ScrollCurve`]: time-based (the same path at any frame rate), and more input while it runs
/// retargets it at the speed it has.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SmoothScroll {
    pos: Point,
    target: Point,
    max: Point,
    anim: Option<Animation>,
}

/// A smooth scroll under way: its curve and how far along it is (seconds).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Animation {
    curve: ScrollCurve,
    elapsed: f64,
    started: bool,
}

impl SmoothScroll {
    pub fn position(&self) -> Point {
        self.pos
    }

    pub fn target(&self) -> Point {
        self.target
    }

    pub fn max(&self) -> Point {
        self.max
    }

    /// The scroll range from the content and viewport sizes; the position is clamped into it.
    pub fn set_extent(&mut self, content: Size, viewport: Size) {
        self.max = Point::new(
            (content.width - viewport.width).max(0.0),
            (content.height - viewport.height).max(0.0),
        );
        self.pos = self.clamp(self.pos);
        let target = self.clamp(self.target);
        if target != self.target {
            self.target = target;
            if let Some(a) = &mut self.anim {
                a.curve.update_target(a.elapsed, target);
            }
        }
    }

    fn clamp(&self, p: Point) -> Point {
        Point::new(p.x.clamp(0.0, self.max.x), p.y.clamp(0.0, self.max.y))
    }

    /// Scrolls by `delta` from where a scroll under way is going, animated as `smooth` says or
    /// (`None`) at once. Returns whether anything will move.
    pub fn scroll_by(&mut self, delta: Point, smooth: Option<ScrollKind>) -> bool {
        let to = Point::new(self.target.x + delta.x, self.target.y + delta.y);
        self.scroll_to(to, smooth)
    }

    /// Scrolls to `to`: a smooth scroll under way is retargeted there (whatever started it: it
    /// keeps its speed), else one of `smooth`'s kind starts; `None` jumps.
    pub fn scroll_to(&mut self, to: Point, smooth: Option<ScrollKind>) -> bool {
        let to = self.clamp(to);
        let before = (self.pos, self.target);
        self.target = to;
        match (smooth, &mut self.anim) {
            (None, _) => {
                self.pos = to;
                self.anim = None;
            }
            (Some(_), Some(a)) => a.curve.update_target(a.elapsed, to),
            (Some(kind), None) => {
                if to != self.pos {
                    self.anim = Some(Animation {
                        curve: ScrollCurve::new(kind, self.pos, to),
                        elapsed: 0.0,
                        started: false,
                    });
                }
            }
        }
        (self.pos, self.target) != before
    }

    /// Moves the position and the target by `delta` together (clamped): the content moved under
    /// the view (scroll anchoring), and a smooth scroll under way goes on as far from it as it
    /// was, its whole path moved. Returns whether anything moved.
    pub fn shift(&mut self, delta: Point) -> bool {
        let before = (self.pos, self.target);
        self.pos = self.clamp(Point::new(self.pos.x + delta.x, self.pos.y + delta.y));
        self.target = self.clamp(Point::new(self.target.x + delta.x, self.target.y + delta.y));
        if let Some(a) = &mut self.anim {
            a.curve.adjust(delta);
            if a.curve.target() != self.target {
                a.curve.update_target(a.elapsed, self.target);
            }
        }
        (self.pos, self.target) != before
    }

    /// Whether `delta` would move anything (at the edges it doesn't: the wheel then goes to an
    /// outer scroller).
    pub fn can_scroll(&self, delta: Point) -> bool {
        (delta.y < 0.0 && self.target.y > 0.0)
            || (delta.y > 0.0 && self.target.y < self.max.y)
            || (delta.x < 0.0 && self.target.x > 0.0)
            || (delta.x > 0.0 && self.target.x < self.max.x)
    }

    pub fn is_animating(&self) -> bool {
        self.anim.is_some()
    }

    /// Advances the animation by `dt`, its first frame by one display refresh `interval` at most (the time since the frame before is long when nothing was moving). Returns
    /// whether the position changed.
    pub fn tick(&mut self, dt: Duration, interval: Duration) -> bool {
        let Some(a) = &mut self.anim else {
            return false;
        };
        let dt = if a.started { dt } else { dt.min(interval) };
        a.started = true;
        a.elapsed += dt.as_secs_f64();
        let before = self.pos;
        if a.curve.is_done(a.elapsed) {
            self.pos = self.target;
            self.anim = None;
        } else {
            let p = a.curve.value(a.elapsed);
            self.pos = self.clamp(p);
        }
        self.pos != before
    }
}

/// A scroll bar's geometry along one axis: the track, and the thumb's place in it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scrollbar {
    /// The track's start and length.
    pub track_start: f32,
    pub track_len: f32,
    /// The thumb's start (absolute, like `track_start`) and length.
    pub thumb_start: f32,
    pub thumb_len: f32,
    /// The scroll range the track maps (content minus viewport).
    pub range: f32,
}

/// The smallest thumb.
const MIN_THUMB: f32 = 24.0;

impl Scrollbar {
    /// The bar for a viewport of `viewport` showing `content` scrolled to `scroll`, in a track at
    /// `track_start` of `track_len`. `None` when nothing scrolls.
    pub fn new(
        viewport: f32,
        content: f32,
        scroll: f32,
        track_start: f32,
        track_len: f32,
    ) -> Option<Scrollbar> {
        let range = content - viewport;
        if range < 1.0 || track_len <= 0.0 || viewport <= 0.0 {
            return None;
        }
        let thumb_len = (track_len * viewport / content)
            .max(MIN_THUMB)
            .min(track_len);
        let travel = track_len - thumb_len;
        let thumb_start = track_start + travel * (scroll / range).clamp(0.0, 1.0);
        Some(Scrollbar {
            track_start,
            track_len,
            thumb_start,
            thumb_len,
            range,
        })
    }

    /// The scroll position that puts the thumb's start at `thumb_start` (dragging it).
    pub fn scroll_for_thumb(&self, thumb_start: f32) -> f32 {
        let travel = self.track_len - self.thumb_len;
        if travel <= 0.0 {
            return 0.0;
        }
        ((thumb_start - self.track_start) / travel).clamp(0.0, 1.0) * self.range
    }

    pub fn thumb_contains(&self, p: f32) -> bool {
        p >= self.thumb_start && p < self.thumb_start + self.thumb_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_micros(5556); // 180 Hz

    fn page(content: f32) -> SmoothScroll {
        let mut s = SmoothScroll::default();
        s.set_extent(Size::new(1000.0, content), Size::new(1000.0, 800.0));
        s
    }

    #[test]
    fn wheel_steps() {
        assert_eq!(wheel_pixels(-120, 3, 800.0), -100.0);
        assert_eq!(wheel_pixels(240, 3, 800.0), 200.0);
        assert_eq!(wheel_pixels(-120, 6, 800.0), -200.0);
        assert_eq!(wheel_pixels(-120, WHEEL_PAGE, 800.0), -800.0);
        assert_eq!(wheel_pixels(-12, 3, 800.0), -10.0);
        assert!(is_precise(-12));
        assert!(!is_precise(-240));
        assert_eq!(page_step(800.0), 720.0);
        assert_eq!(page_step(100.0), 87.5);
        assert_eq!(page_step(20.0), 40.0);
    }

    const WHEEL: Option<ScrollKind> = Some(ScrollKind::Wheel);
    const HZ60: Duration = Duration::from_micros(16_667);

    #[test]
    fn animates_to_the_target_and_stops() {
        let mut s = page(5000.0);
        assert!(s.scroll_by(Point::new(0.0, 300.0), WHEEL));
        assert_eq!(s.position().y, 0.0);
        let mut frames = 0;
        let mut last = 0.0;
        while s.tick(FRAME, FRAME) {
            frames += 1;
            assert!(s.position().y > last, "monotonic");
            last = s.position().y;
            assert!(frames < 200);
        }
        assert_eq!(s.position().y, 300.0);
        assert!(!s.is_animating());
        // (300 px of wheel: 9 frames at 60 Hz, 150 ms, 27 at 180 Hz)
        assert!((27..=28).contains(&frames), "{frames} frames");
    }

    /// One notch eases in and out over 12 frames at 60 Hz (200 ms), a new position each frame,
    /// on the wheel's curve.
    #[test]
    fn one_notch_moves_every_frame_on_the_curve() {
        let mut s = page(5000.0);
        s.scroll_by(Point::new(0.0, 100.0), WHEEL);
        let curve = ScrollCurve::new(ScrollKind::Wheel, Point::ZERO, Point::new(0.0, 100.0));
        let mut ys = Vec::new();
        while s.tick(HZ60, HZ60) {
            ys.push(s.position().y);
        }
        assert_eq!(ys.len(), 12, "{ys:?}");
        for (i, y) in ys.iter().enumerate().take(11) {
            let want = curve.value((i + 1) as f64 * HZ60.as_secs_f64()).y;
            assert!((y - want).abs() < 1e-3, "frame {i}: {y} against {want}");
        }
        // (slow at the start and the end, half way at half time)
        assert!(ys[0] < 2.0 && ys[0] > 0.0, "{ys:?}");
        assert!((ys[5] - 50.0).abs() < 0.5, "{ys:?}");
        assert!(100.0 - ys[10] < 2.0, "{ys:?}");
        assert_eq!(ys[11], 100.0);
    }

    /// Notches while a wheel scroll runs keep it going: no frame stands still or goes back, and
    /// the speed where a notch comes in carries on.
    #[test]
    fn notches_during_a_scroll_retarget_it() {
        let mut s = page(50_000.0);
        s.scroll_by(Point::new(0.0, 100.0), WHEEL);
        let mut ys = vec![0.0];
        for frame in 1..60 {
            // (a notch every 50 ms for half a second)
            if frame % 3 == 0 && frame <= 30 {
                let (a, b) = (ys[ys.len() - 2.min(ys.len())], ys[ys.len() - 1]);
                s.scroll_by(Point::new(0.0, 100.0), WHEEL);
                s.tick(HZ60, HZ60);
                let c = s.position().y;
                // (the step after the notch is about as long as the one before it at least:
                // the speed carries on instead of starting again from rest)
                assert!(c - b >= (b - a) * 0.8, "{a} {b} {c}");
                ys.push(c);
                continue;
            }
            if !s.tick(HZ60, HZ60) {
                break;
            }
            ys.push(s.position().y);
        }
        for w in ys.windows(2) {
            assert!(w[1] > w[0], "{ys:?}");
        }
        assert_eq!(*ys.last().unwrap(), 1100.0);
    }

    /// The first frame of a scroll after the page sat still moves as far as one refresh, not as
    /// far as the time since the last frame.
    #[test]
    fn the_first_frame_after_idle_is_one_frame() {
        let mut s = page(5000.0);
        s.scroll_by(Point::new(0.0, 100.0), WHEEL);
        s.tick(Duration::from_millis(50), FRAME);
        let curve = ScrollCurve::new(ScrollKind::Wheel, Point::ZERO, Point::new(0.0, 100.0));
        let want = curve.value(FRAME.as_secs_f64()).y;
        assert!((s.position().y - want).abs() < 1e-4);
    }

    #[test]
    fn frame_rate_independent() {
        let mut a = page(5000.0);
        let mut b = page(5000.0);
        a.scroll_by(Point::new(0.0, 500.0), WHEEL);
        b.scroll_by(Point::new(0.0, 500.0), WHEEL);
        for _ in 0..18 {
            a.tick(FRAME, FRAME);
        }
        for _ in 0..6 {
            b.tick(FRAME * 3, FRAME * 3);
        }
        assert!((a.position().y - b.position().y).abs() < 1.0);
    }

    #[test]
    fn input_accumulates_on_the_target_and_clamps() {
        let mut s = page(1000.0);
        s.scroll_by(Point::new(0.0, 100.0), WHEEL);
        s.tick(FRAME, FRAME);
        s.scroll_by(Point::new(0.0, 100.0), WHEEL);
        assert_eq!(s.target().y, 200.0);
        s.scroll_by(Point::new(0.0, 1000.0), WHEEL);
        // (the range is 1000 - 800)
        assert_eq!(s.target().y, 200.0);
        assert_eq!(s.max().y, 200.0);
        assert!(!s.can_scroll(Point::new(0.0, 10.0)));
        assert!(s.can_scroll(Point::new(0.0, -10.0)));
        s.scroll_to(Point::new(0.0, -50.0), None);
        assert_eq!(s.position().y, 0.0);
        assert!(!s.can_scroll(Point::new(0.0, -10.0)));
        assert!(!s.can_scroll(Point::new(10.0, 0.0)));
    }

    /// A scroll anchoring shift during a smooth scroll moves where it is and where it goes alike:
    /// the rest of the wheel's scroll is not lost.
    #[test]
    fn a_shift_keeps_a_smooth_scroll_going() {
        let mut s = page(5000.0);
        s.scroll_by(Point::new(0.0, 300.0), WHEEL);
        s.tick(FRAME, FRAME);
        let (pos, target) = (s.position().y, s.target().y);
        assert!(s.shift(Point::new(0.0, 120.0)));
        assert_eq!(s.position().y, pos + 120.0);
        assert_eq!(s.target().y, target + 120.0);
        assert!(s.is_animating());
        // (the next frame goes on from the shifted place, not back to where it was)
        s.tick(FRAME, FRAME);
        assert!(s.position().y > pos + 120.0 && s.position().y < target + 120.0);
        // (clamped to the range)
        s.shift(Point::new(0.0, 10_000.0));
        assert_eq!(s.position().y, 4200.0);
    }

    #[test]
    fn shrinking_content_clamps_the_position() {
        let mut s = page(5000.0);
        s.scroll_to(Point::new(0.0, 4000.0), None);
        s.set_extent(Size::new(1000.0, 1500.0), Size::new(1000.0, 800.0));
        assert_eq!(s.position().y, 700.0);
        assert!(!s.is_animating());
    }

    #[test]
    fn scrollbar_geometry() {
        assert!(Scrollbar::new(800.0, 800.0, 0.0, 0.0, 800.0).is_none());
        let b = Scrollbar::new(800.0, 3200.0, 0.0, 0.0, 800.0).unwrap();
        assert_eq!(b.thumb_len, 200.0);
        assert_eq!(b.thumb_start, 0.0);
        let b = Scrollbar::new(800.0, 3200.0, 2400.0, 0.0, 800.0).unwrap();
        assert_eq!(b.thumb_start, 600.0);
        assert_eq!(b.scroll_for_thumb(300.0), 1200.0);
        assert_eq!(b.scroll_for_thumb(-50.0), 0.0);
        assert_eq!(b.scroll_for_thumb(5000.0), 2400.0);
        assert!(b.thumb_contains(650.0));
        // A very long document keeps a grabbable thumb.
        let b = Scrollbar::new(800.0, 1_000_000.0, 0.0, 10.0, 780.0).unwrap();
        assert_eq!(b.thumb_len, MIN_THUMB);
        assert_eq!(b.thumb_start, 10.0);
    }
}
