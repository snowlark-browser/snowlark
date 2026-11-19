// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Middle-button autoscroll, as Windows browsers have it: a middle press on a page that can scroll
//! shows a marker at the press and enters autoscroll; moving the pointer away from the marker scrolls
//! toward it, faster the farther it is, in each direction the target can scroll, with a dead zone
//! around the marker and a cursor that points where the page goes.
//!
//! Two ways in, as in Chrome: press, move and release ends it (the button was held for the
//! scrolling); a click, released without having left the dead zone, leaves it on until the next click,
//! wheel turn or Escape.
//!
//! The page sees the press as any other (`mousedown`, `mouseup`, `auxclick` reach it); autoscroll
//! starts only when the page did not cancel the `mousedown` and the press was not on a link, a form
//! control or an editable element. The target is the smallest scroll container under the pointer
//! that the user can scroll (frames included; `overflow: hidden` axes are not), else the viewport.
//! The scrolling itself is the compositor's, once a display frame ([`CompMsg::AutoScroll`]), through
//! the paths of the wheel.

use snowlark_paint::window::{Cursor, Window};
use snowlark_paint::{Color, DisplayListBuilder, Point, Radii, Rect};

use super::Browser;
use crate::composite::{AutoTarget, CompMsg};
use crate::driver::{PressOutcome, Scroller};
use crate::tab::TabId;

/// How far (DIPs) the pointer must be from the marker, along an axis, before that axis scrolls.
pub const DEAD_ZONE: f32 = 12.0;
/// The fastest scrolling (CSS px a second).
const MAX_SPEED: f32 = 3000.0;

/// The axes the target can scroll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Axes {
    pub x: bool,
    pub y: bool,
}

/// How fast one axis scrolls for the pointer `offset` DIPs from the marker (CSS px a second,
/// signed): none inside the dead zone, then growing with the distance past it, a little more than
/// linearly.
pub fn axis_speed(offset: f32) -> f32 {
    let past = offset.abs() - DEAD_ZONE;
    if past <= 0.0 {
        return 0.0;
    }
    (past * 8.0 + past * past * 0.04).min(MAX_SPEED) * offset.signum()
}

/// The scrolling velocity for the pointer `offset` from the marker.
pub fn velocity(offset: Point, axes: Axes) -> Point {
    Point::new(
        if axes.x { axis_speed(offset.x) } else { 0.0 },
        if axes.y { axis_speed(offset.y) } else { 0.0 },
    )
}

/// The direction the pointer is from the marker, as the cursor shows it (each -1, 0 or 1).
pub fn direction(offset: Point, axes: Axes) -> (i8, i8) {
    let v = velocity(offset, axes);
    (
        v.x.signum() as i8 * i8::from(v.x != 0.0),
        v.y.signum() as i8 * i8::from(v.y != 0.0),
    )
}

/// What a middle press at the document point `doc` scrolls: the smallest container under it that
/// has something to scroll, else the viewport (whose scroll range is `view_max`); `None` when
/// nothing there scrolls. The scrollers are the ones the user can scroll, frames' included.
pub fn pick_target(
    scrollers: &[Scroller],
    doc: Point,
    view_max: Point,
) -> Option<(AutoTarget, Axes)> {
    let best = scrollers
        .iter()
        .filter(|s| s.rect.contains(doc) && (s.max.x > 0.0 || s.max.y > 0.0))
        .min_by(|a, b| {
            (a.rect.width * a.rect.height)
                .partial_cmp(&(b.rect.width * b.rect.height))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    if let Some(s) = best {
        return Some((
            AutoTarget::Container(s.node),
            Axes {
                x: s.max.x > 0.0,
                y: s.max.y > 0.0,
            },
        ));
    }
    (view_max.x > 0.0 || view_max.y > 0.0).then_some((
        AutoTarget::Viewport,
        Axes {
            x: view_max.x > 0.0,
            y: view_max.y > 0.0,
        },
    ))
}

/// Whether a middle press that the page answered with `press` may start autoscroll.
pub fn may_start(press: PressOutcome) -> bool {
    !press.canceled && !press.blocked
}

/// What a button release does to an autoscroll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// Autoscroll goes on until the next click, wheel turn or Escape.
    Keep,
    /// It ends.
    Stop,
}

/// An autoscroll in progress.
#[derive(Clone, Copy, Debug)]
pub struct Session {
    pub tab: TabId,
    /// The marker (window DIPs).
    pub origin: Point,
    pub target: AutoTarget,
    pub axes: Axes,
    /// The button is still down (press and drag).
    pub held: bool,
    /// The pointer has been out of the dead zone.
    pub moved: bool,
}

impl Session {
    pub fn new(tab: TabId, origin: Point, target: AutoTarget, axes: Axes, held: bool) -> Session {
        Session {
            tab,
            origin,
            target,
            axes,
            held,
            moved: false,
        }
    }

    /// The pointer is at `p` (window DIPs): the velocity to scroll at.
    pub fn moved_to(&mut self, p: Point) -> Point {
        let v = velocity(
            Point::new(p.x - self.origin.x, p.y - self.origin.y),
            self.axes,
        );
        if v != Point::ZERO {
            self.moved = true;
        }
        v
    }

    /// The cursor direction for the pointer at `p`.
    pub fn cursor_at(&self, p: Point) -> (i8, i8) {
        direction(
            Point::new(p.x - self.origin.x, p.y - self.origin.y),
            self.axes,
        )
    }

    /// The button went up: a drag that went somewhere ends here; a click stays on.
    pub fn released(&mut self) -> Release {
        if !self.held {
            return Release::Keep;
        }
        self.held = false;
        if self.moved {
            Release::Stop
        } else {
            Release::Keep
        }
    }
}

/// A middle press waiting for the page's answer to its `mousedown`.
#[derive(Clone, Copy, Debug)]
pub struct PendingPress {
    pub tab: TabId,
    pub origin: Point,
    /// The button went up before the answer came (a quick click).
    pub released: bool,
}

impl Browser {
    pub(super) fn autoscroll_active(&self) -> bool {
        self.autoscroll.is_some()
    }

    /// A middle press in the page, sent to it: autoscroll waits for the page's answer.
    pub(super) fn autoscroll_press(&mut self, dip: Point) {
        let Some(e) = self.tabs.active().filter(|e| e.crashed.is_none()) else {
            return;
        };
        self.autoscroll_pending = Some(PendingPress {
            tab: e.tab.id,
            origin: dip,
            released: false,
        });
    }

    /// The page answered a press of tab `tab`: autoscroll starts when it may.
    pub(super) fn autoscroll_answer(&mut self, window: &Window, tab: TabId, press: PressOutcome) {
        let Some(pending) = self.autoscroll_pending.filter(|p| p.tab == tab) else {
            return;
        };
        self.autoscroll_pending = None;
        if !may_start(press) || self.active_id() != Some(tab) {
            return;
        }
        let Some(e) = self.tabs.active() else {
            return;
        };
        let view = self.comp.scroll_view(tab);
        let page = self.page_point(pending.origin);
        let doc = Point::new(page.x + view.scroll.x, page.y + view.scroll.y);
        let Some((target, axes)) = pick_target(&e.tab.state.scrollers, doc, view.max) else {
            return;
        };
        self.autoscroll = Some(Session::new(
            tab,
            pending.origin,
            target,
            axes,
            !pending.released,
        ));
        self.set_autoscroll_cursor(window, pending.origin);
        self.overlay_dirty = true;
    }

    /// The pointer moved to `dip`: autoscroll's velocity and cursor follow.
    pub(super) fn autoscroll_moved(&mut self, window: &Window, dip: Point) {
        let Some(mut s) = self.autoscroll else {
            return;
        };
        let v = s.moved_to(dip);
        self.autoscroll = Some(s);
        self.comp.send(CompMsg::AutoScroll {
            tab: s.tab,
            target: s.target,
            velocity: Some(v),
        });
        self.set_autoscroll_cursor(window, dip);
    }

    fn set_autoscroll_cursor(&mut self, window: &Window, dip: Point) {
        if let Some(s) = &self.autoscroll {
            let (dx, dy) = s.cursor_at(dip);
            let c = Cursor::AutoScroll(dx, dy);
            window.set_cursor(c);
            self.sent_cursor = c;
        }
    }

    /// The middle button went up.
    pub(super) fn autoscroll_released(&mut self, window: &Window) {
        if let Some(p) = &mut self.autoscroll_pending {
            p.released = true;
        }
        let Some(mut s) = self.autoscroll else {
            return;
        };
        let end = s.released() == Release::Stop;
        self.autoscroll = Some(s);
        if end {
            self.stop_autoscroll(window);
        }
    }

    /// Ends autoscroll where there is no window at hand (another tab shows, the page navigated or
    /// crashed): the compositor stops scrolling and the marker goes.
    pub(super) fn end_autoscroll(&mut self) {
        self.autoscroll_pending = None;
        let Some(s) = self.autoscroll.take() else {
            return;
        };
        self.comp.send(CompMsg::AutoScroll {
            tab: s.tab,
            target: s.target,
            velocity: None,
        });
        self.overlay_dirty = true;
    }

    /// Ends autoscroll (a click, the wheel, Escape, the window losing focus...), and the cursor is
    /// the page's again.
    pub(super) fn stop_autoscroll(&mut self, window: &Window) {
        if self.autoscroll.is_none() {
            self.autoscroll_pending = None;
            return;
        }
        self.end_autoscroll();
        // (the page's cursor again)
        let cursor = self.tabs.active().map_or(Cursor::Arrow, |e| e.cursor);
        window.set_cursor(cursor);
        self.sent_cursor = cursor;
    }

    /// The marker, in the overlay (content-area DIPs).
    pub(super) fn paint_autoscroll(&self, b: &mut DisplayListBuilder) {
        let Some(s) = &self.autoscroll else {
            return;
        };
        let c = Point::new(s.origin.x, s.origin.y - self.chrome_h());
        let r = 13.0;
        let ring = Rect::new(c.x - r, c.y - r, 2.0 * r, 2.0 * r);
        let dark = Color::rgb(0x20, 0x21, 0x24);
        b.fill_rounded_rect(ring, Radii::uniform(r), Color::rgba(255, 255, 255, 235));
        b.stroke_rounded_rect(ring, Radii::uniform(r), 1.5, dark);
        let dot = Rect::new(c.x - 2.0, c.y - 2.0, 4.0, 4.0);
        b.fill_rounded_rect(dot, Radii::uniform(2.0), dark);
        // A small arrow head on each side the target scrolls toward.
        let head = |b: &mut DisplayListBuilder, dx: f32, dy: f32| {
            for i in 0..3 {
                let w = 7.0 - 2.0 * i as f32;
                let off = 7.0 + i as f32 * 2.0;
                let (cx, cy) = (c.x + dx * off, c.y + dy * off);
                let rect = if dx == 0.0 {
                    Rect::new(cx - w / 2.0, cy - 1.0, w, 2.0)
                } else {
                    Rect::new(cx - 1.0, cy - w / 2.0, 2.0, w)
                };
                b.fill_rect(rect, dark);
            }
        };
        if s.axes.y {
            head(b, 0.0, -1.0);
            head(b, 0.0, 1.0);
        }
        if s.axes.x {
            head(b, -1.0, 0.0);
            head(b, 1.0, 0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_dom::NodeId;

    fn node(index: u32) -> NodeId {
        postcard::from_bytes(&postcard::to_stdvec(&(index, 3u32)).unwrap()).unwrap()
    }

    fn scroller(i: u32, x: f32, y: f32, w: f32, h: f32, max: (f32, f32)) -> Scroller {
        Scroller {
            node: node(i),
            rect: Rect::new(x, y, w, h),
            scroll: Point::ZERO,
            max: Point::new(max.0, max.1),
        }
    }

    const BOTH: Axes = Axes { x: true, y: true };

    #[test]
    fn nothing_scrolls_in_the_dead_zone_and_speed_grows_with_distance() {
        assert_eq!(axis_speed(0.0), 0.0);
        assert_eq!(axis_speed(DEAD_ZONE), 0.0);
        assert_eq!(axis_speed(-DEAD_ZONE + 1.0), 0.0);
        let near = axis_speed(DEAD_ZONE + 5.0);
        let far = axis_speed(DEAD_ZONE + 50.0);
        let farther = axis_speed(DEAD_ZONE + 150.0);
        assert!(near > 0.0 && far > near * 5.0 && farther > far * 2.0);
        // Down and right are positive, up and left negative, symmetric.
        assert_eq!(axis_speed(-(DEAD_ZONE + 50.0)), -far);
        // Capped.
        assert_eq!(axis_speed(100_000.0), MAX_SPEED);
        // Only the axes the target scrolls.
        let v = velocity(Point::new(80.0, 80.0), Axes { x: false, y: true });
        assert_eq!(v.x, 0.0);
        assert!(v.y > 0.0);
        assert_eq!(
            velocity(Point::new(80.0, -80.0), BOTH).y,
            -velocity(Point::new(80.0, 80.0), BOTH).y
        );
        // The cursor points where it scrolls.
        assert_eq!(direction(Point::ZERO, BOTH), (0, 0));
        assert_eq!(direction(Point::new(0.0, -60.0), BOTH), (0, -1));
        assert_eq!(direction(Point::new(60.0, 60.0), BOTH), (1, 1));
        assert_eq!(direction(Point::new(-60.0, 5.0), BOTH), (-1, 0));
        assert_eq!(
            direction(Point::new(60.0, 60.0), Axes { x: false, y: true }),
            (0, 1)
        );
    }

    #[test]
    fn the_target_is_the_smallest_scrolling_container_under_the_pointer() {
        let outer = scroller(1, 0.0, 0.0, 800.0, 600.0, (0.0, 900.0));
        let inner = scroller(2, 100.0, 100.0, 300.0, 200.0, (0.0, 500.0));
        let frame = scroller(3, 150.0, 150.0, 100.0, 100.0, (40.0, 0.0));
        let all = [outer, inner, frame];
        let view = Point::new(0.0, 3000.0);
        // In the frame: the frame, scrolling sideways only.
        let (t, axes) = pick_target(&all, Point::new(200.0, 200.0), view).unwrap();
        assert_eq!(t, AutoTarget::Container(node(3)));
        assert_eq!(axes, Axes { x: true, y: false });
        // In the inner container but outside the frame: the inner one.
        let (t, _) = pick_target(&all, Point::new(120.0, 120.0), view).unwrap();
        assert_eq!(t, AutoTarget::Container(node(2)));
        // Outside every container: the viewport.
        let (t, axes) = pick_target(&all, Point::new(900.0, 700.0), view).unwrap();
        // (inside the outer container, the outer one)
        let (t2, _) = pick_target(&all, Point::new(780.0, 580.0), view).unwrap();
        assert_eq!(t2, AutoTarget::Container(node(1)));
        assert_eq!(
            (t, axes),
            (AutoTarget::Viewport, Axes { x: false, y: true })
        );
        let (t, _) = pick_target(&[], Point::new(5.0, 5.0), view).unwrap();
        assert_eq!(t, AutoTarget::Viewport);
        // A container with nothing to scroll is not a target (its page may be), and a page that
        // does not scroll has none.
        let flat = scroller(4, 0.0, 0.0, 800.0, 600.0, (0.0, 0.0));
        assert_eq!(
            pick_target(&[flat], Point::new(5.0, 5.0), view).map(|t| t.0),
            Some(AutoTarget::Viewport)
        );
        assert_eq!(
            pick_target(&[flat], Point::new(5.0, 5.0), Point::ZERO),
            None
        );
        assert_eq!(pick_target(&[], Point::new(5.0, 5.0), Point::ZERO), None);
    }

    #[test]
    fn a_press_starts_autoscroll_only_when_the_page_and_the_target_allow() {
        let ok = PressOutcome::default();
        assert!(may_start(ok));
        // The page canceled the mousedown; a link, a form control or an editable element.
        assert!(!may_start(PressOutcome {
            canceled: true,
            blocked: false
        }));
        assert!(!may_start(PressOutcome {
            canceled: false,
            blocked: true
        }));
    }

    #[test]
    fn a_drag_ends_on_release_and_a_click_stays_on() {
        let target = AutoTarget::Viewport;
        // Press, move out, release: it ends.
        let mut s = Session::new(1, Point::new(100.0, 100.0), target, BOTH, true);
        assert_eq!(s.moved_to(Point::new(104.0, 103.0)), Point::ZERO);
        assert!(s.moved_to(Point::new(100.0, 200.0)).y > 0.0);
        assert_eq!(s.released(), Release::Stop);
        // Press and release within the dead zone: it stays, and the next release is nothing.
        let mut s = Session::new(1, Point::new(100.0, 100.0), target, BOTH, true);
        s.moved_to(Point::new(102.0, 101.0));
        assert_eq!(s.released(), Release::Keep);
        assert!(!s.held);
        // Moving and releasing the (middle) button again does not end it: only a click does.
        s.moved_to(Point::new(100.0, 300.0));
        assert_eq!(s.released(), Release::Keep);
        // Started by a click already released (the page answered late).
        let mut s = Session::new(1, Point::new(100.0, 100.0), target, BOTH, false);
        s.moved_to(Point::new(100.0, 300.0));
        assert_eq!(s.released(), Release::Keep);
    }

    #[test]
    fn the_compositor_steps_whole_pixels_and_carries_the_rest() {
        use crate::composite::autoscroll_step;
        use std::time::Duration;
        let frame = Duration::from_micros(16_667);
        // 30 px a second is half a pixel a frame: a pixel every other frame, none lost.
        let v = Point::new(0.0, 30.0);
        let mut carry = Point::ZERO;
        let mut total = 0.0;
        for _ in 0..60 {
            let s = autoscroll_step(v, frame, carry);
            assert_eq!(s.delta.y.fract(), 0.0);
            total += s.delta.y;
            carry = s.carry;
        }
        assert!((total - 30.0).abs() <= 1.0, "{total}");
        // Upward is negative and carries the same way.
        let s = autoscroll_step(Point::new(-1000.0, -1000.0), frame, Point::ZERO);
        assert_eq!(s.delta, Point::new(-16.0, -16.0));
        assert!(s.carry.x < 0.0 && s.carry.x > -1.0);
        // A long frame (a stall) covers the whole time it was.
        let s = autoscroll_step(
            Point::new(0.0, 600.0),
            Duration::from_millis(50),
            Point::ZERO,
        );
        assert_eq!(s.delta.y, 30.0);
    }
}
