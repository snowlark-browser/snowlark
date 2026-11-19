// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scrolling math: smooth scroll animation ([`SmoothScroll`]), wheel and keyboard step sizes, and
//! scroll bar geometry ([`Scrollbar`]). Positions are CSS px.

use std::time::Duration;

use snowlark_paint::{Point, Size};

/// Pixels per wheel notch at the default three lines per notch (what Chrome and Edge scroll).
pub const PIXELS_PER_NOTCH: f32 = 100.0;
pub use snowlark_tab::driver::{page_step, LINE_STEP};
/// How long the smooth scroll takes to cover ~63% of the remaining distance.
const TIME_CONSTANT: f32 = 0.045;

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

/// A scroll position moving smoothly towards a target, clamped to `0..=max`. The animation is an
/// exponential approach (frame-rate independent, retargets without a jolt when more input arrives).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SmoothScroll {
    pos: Point,
    target: Point,
    max: Point,
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
        self.target = self.clamp(self.target);
    }

    fn clamp(&self, p: Point) -> Point {
        Point::new(p.x.clamp(0.0, self.max.x), p.y.clamp(0.0, self.max.y))
    }

    /// Scrolls by `delta`, animated or at once. Returns whether anything will move.
    pub fn scroll_by(&mut self, delta: Point, smooth: bool) -> bool {
        let to = Point::new(self.target.x + delta.x, self.target.y + delta.y);
        self.scroll_to(to, smooth)
    }

    pub fn scroll_to(&mut self, to: Point, smooth: bool) -> bool {
        let to = self.clamp(to);
        let before = (self.pos, self.target);
        self.target = to;
        if !smooth {
            self.pos = to;
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
        self.pos != self.target
    }

    /// Advances the animation by `dt`. Returns whether the position changed.
    pub fn tick(&mut self, dt: Duration) -> bool {
        if !self.is_animating() {
            return false;
        }
        let k = 1.0 - (-dt.as_secs_f32() / TIME_CONSTANT).exp();
        let step = |p: f32, t: f32| {
            let n = p + (t - p) * k;
            if (t - n).abs() < 0.5 {
                t
            } else {
                n
            }
        };
        let before = self.pos;
        self.pos = Point::new(
            step(self.pos.x, self.target.x),
            step(self.pos.y, self.target.y),
        );
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

    #[test]
    fn animates_to_the_target_and_stops() {
        let mut s = page(5000.0);
        assert!(s.scroll_by(Point::new(0.0, 300.0), true));
        assert_eq!(s.position().y, 0.0);
        let mut frames = 0;
        let mut last = 0.0;
        while s.tick(FRAME) {
            frames += 1;
            assert!(s.position().y > last, "monotonic");
            last = s.position().y;
            assert!(frames < 200);
        }
        assert_eq!(s.position().y, 300.0);
        assert!(!s.is_animating());
        // ~0.3 s at 180 Hz, most of it in the first 100 ms.
        assert!((30..80).contains(&frames), "{frames} frames");
    }

    #[test]
    fn frame_rate_independent() {
        let mut a = page(5000.0);
        let mut b = page(5000.0);
        a.scroll_by(Point::new(0.0, 500.0), true);
        b.scroll_by(Point::new(0.0, 500.0), true);
        for _ in 0..18 {
            a.tick(FRAME);
        }
        for _ in 0..6 {
            b.tick(FRAME * 3);
        }
        assert!((a.position().y - b.position().y).abs() < 1.0);
    }

    #[test]
    fn input_accumulates_on_the_target_and_clamps() {
        let mut s = page(1000.0);
        s.scroll_by(Point::new(0.0, 100.0), true);
        s.tick(FRAME);
        s.scroll_by(Point::new(0.0, 100.0), true);
        assert_eq!(s.target().y, 200.0);
        s.scroll_by(Point::new(0.0, 1000.0), true);
        // (the range is 1000 - 800)
        assert_eq!(s.target().y, 200.0);
        assert_eq!(s.max().y, 200.0);
        assert!(!s.can_scroll(Point::new(0.0, 10.0)));
        assert!(s.can_scroll(Point::new(0.0, -10.0)));
        s.scroll_to(Point::new(0.0, -50.0), false);
        assert_eq!(s.position().y, 0.0);
        assert!(!s.can_scroll(Point::new(0.0, -10.0)));
        assert!(!s.can_scroll(Point::new(10.0, 0.0)));
    }

    #[test]
    fn shrinking_content_clamps_the_position() {
        let mut s = page(5000.0);
        s.scroll_to(Point::new(0.0, 4000.0), false);
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
