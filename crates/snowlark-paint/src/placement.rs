// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where a window opens: the placement the last run left (its restored rectangle, whether it was
//! maximized, the DPI it was measured at) checked against the monitors there are now. Pure
//! arithmetic; [`crate::window`] reads the monitors and applies the answer.
//!
//! The rules: a saved position that is not on any monitor any more (a screen unplugged, a resolution
//! change) puts the window, at its saved size, in the middle of the primary monitor; one that is
//! partly off a monitor is moved back inside; the size is kept in device-independent pixels, so a
//! different DPI gives the same logical size; and a window never opens bigger than its monitor's work
//! area.

/// Where a window was, in screen coordinates (device pixels at `dpi`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The restored (not maximized) rectangle.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// The window was maximized (its restored rectangle is the one above).
    pub maximized: bool,
    /// The DPI of the monitor the window was on (96 is 100%).
    pub dpi: u32,
}

/// A window's rectangle (`x`, `y`, width, height) moved and shrunk into a monitor's work area
/// (`left`, `top`, `right`, `bottom`): CSSOM View lets a browser clamp what a page asks of a
/// window's place so that it stays on the screen (the `window.open()` features, `moveTo()`,
/// `resizeTo()`).
pub fn clamp_to_work_area(
    work: (i32, i32, i32, i32),
    rect: (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    let (left, top, right, bottom) = work;
    let w = rect.2.min(right - left).max(1);
    let h = rect.3.min(bottom - top).max(1);
    let x = rect.0.min(right - w).max(left);
    let y = rect.1.min(bottom - h).max(top);
    (x, y, w, h)
}

/// A monitor's work area (the screen less the taskbar), in screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub dpi: u32,
    pub primary: bool,
}

impl Monitor {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// The smallest a window is made (CSS-like pixels at 96 DPI; the window's own minimum track size).
pub const MIN_WIDTH: i32 = 400;
pub const MIN_HEIGHT: i32 = 240;
/// How much of the title strip must be on a monitor for the saved position to count as on screen
/// (device pixels at 96 DPI).
const GRAB_WIDTH: i32 = 100;
const STRIP_HEIGHT: i32 = 40;

/// What to do with a saved placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    /// The monitor the window goes to.
    pub monitor: Monitor,
    /// Its top-left corner, or `None` to center it on the monitor.
    pub position: Option<(i32, i32)>,
    /// Its size in device-independent pixels (96 DPI).
    pub width_dips: i32,
    pub height_dips: i32,
    pub maximized: bool,
}

fn overlap(a: (i32, i32, i32, i32), m: &Monitor) -> (i32, i32) {
    let w = a.2.min(m.right) - a.0.max(m.left);
    let h = a.3.min(m.bottom) - a.1.max(m.top);
    (w.max(0), h.max(0))
}

/// The saved placement made to fit the monitors; `None` when there is none, or it is nonsense (a
/// corrupt file: sizes that are zero or absurd, no DPI), or there are no monitors: the caller opens
/// the default window then.
pub fn choose(saved: Option<&Placement>, monitors: &[Monitor]) -> Option<Choice> {
    let p = saved?;
    let primary = monitors
        .iter()
        .find(|m| m.primary)
        .or_else(|| monitors.first())?;
    if p.width <= 0
        || p.height <= 0
        || p.width > 30_000
        || p.height > 30_000
        || !(48..=960).contains(&p.dpi)
        || p.x.abs() > 100_000
        || p.y.abs() > 100_000
    {
        return None;
    }
    let dpi = p.dpi as i32;
    let width_dips = (p.width * 96 / dpi).max(MIN_WIDTH);
    let height_dips = (p.height * 96 / dpi).max(MIN_HEIGHT);
    // On screen when a grabbable piece of the title strip is on a monitor.
    // (a window wider than any screen is judged by its first 1200 px: it stays where its corner is)
    let strip = (
        p.x,
        p.y,
        p.x + p.width.min(1200 * dpi / 96),
        p.y + STRIP_HEIGHT * dpi / 96,
    );
    let grab = GRAB_WIDTH * dpi / 96;
    let on_screen = monitors
        .iter()
        .filter(|m| {
            let (w, h) = overlap(strip, m);
            w >= grab.min(p.width) && h >= (STRIP_HEIGHT * dpi / 96 / 2)
        })
        .max_by_key(|m| {
            let (w, h) = overlap(strip, m);
            i64::from(w) * i64::from(h)
        });
    let (monitor, position) = match on_screen {
        Some(m) => (*m, Some((p.x, p.y))),
        None => (*primary, None),
    };
    Some(Choice {
        monitor,
        position,
        width_dips,
        height_dips,
        maximized: p.maximized,
    })
}

/// The window rectangle for a size in device pixels at `work`'s monitor: no bigger than the work
/// area, inside it, and at `position` when it has one (moved back in when it hangs out), else
/// centered. Returns (x, y, width, height).
pub fn fit_rect(
    work: &Monitor,
    position: Option<(i32, i32)>,
    width: i32,
    height: i32,
) -> (i32, i32, i32, i32) {
    let w = width.min(work.width()).max(1);
    let h = height.min(work.height()).max(1);
    let (x, y) = match position {
        Some((x, y)) => (
            x.clamp(work.left, (work.right - w).max(work.left)),
            y.clamp(work.top, (work.bottom - h).max(work.top)),
        ),
        None => (
            work.left + (work.width() - w) / 2,
            work.top + (work.height() - h) / 2,
        ),
    };
    (x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_a_page_places_stays_in_the_work_area() {
        let work = (0, 0, 1920, 1040);
        // (inside: as asked)
        assert_eq!(
            clamp_to_work_area(work, (100, 100, 516, 639)),
            (100, 100, 516, 639)
        );
        // (off the right and bottom edges: moved in)
        assert_eq!(
            clamp_to_work_area(work, (1800, 900, 516, 639)),
            (1404, 401, 516, 639)
        );
        // (off the left and top: moved in; too big: the work area's size)
        assert_eq!(
            clamp_to_work_area(work, (-50, -9999, 5000, 300)),
            (0, 0, 1920, 300)
        );
        // (another monitor's work area, left of the primary)
        let left = (-1280, 0, 0, 984);
        assert_eq!(
            clamp_to_work_area(left, (-100, 10, 400, 400)),
            (-400, 10, 400, 400)
        );
    }

    fn mon(left: i32, top: i32, w: i32, h: i32, dpi: u32, primary: bool) -> Monitor {
        Monitor {
            left,
            top,
            right: left + w,
            bottom: top + h,
            dpi,
            primary,
        }
    }

    fn placed(x: i32, y: i32, w: i32, h: i32, dpi: u32) -> Placement {
        Placement {
            x,
            y,
            width: w,
            height: h,
            maximized: false,
            dpi,
        }
    }

    const MAIN: Monitor = Monitor {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
        dpi: 96,
        primary: true,
    };

    #[test]
    fn nothing_saved_or_nonsense_saved_is_the_default() {
        assert_eq!(choose(None, &[MAIN]), None);
        assert_eq!(choose(Some(&placed(0, 0, 800, 600, 96)), &[]), None);
        for bad in [
            placed(0, 0, 0, 600, 96),
            placed(0, 0, 800, -5, 96),
            placed(0, 0, 80_000, 600, 96),
            placed(0, 0, 800, 600, 0),
            placed(0, 0, 800, 600, 5000),
            placed(9_000_000, 0, 800, 600, 96),
        ] {
            assert_eq!(choose(Some(&bad), &[MAIN]), None, "{bad:?}");
        }
    }

    #[test]
    fn a_position_on_a_monitor_is_kept() {
        let c = choose(Some(&placed(200, 100, 1000, 700, 96)), &[MAIN]).unwrap();
        assert_eq!(c.position, Some((200, 100)));
        assert_eq!((c.width_dips, c.height_dips), (1000, 700));
        assert_eq!(c.monitor, MAIN);
        assert!(!c.maximized);
        let (x, y, w, h) = fit_rect(&c.monitor, c.position, 1000, 700);
        assert_eq!((x, y, w, h), (200, 100, 1000, 700));
    }

    #[test]
    fn maximized_is_kept_with_the_restored_size() {
        let mut p = placed(300, 150, 900, 650, 96);
        p.maximized = true;
        let c = choose(Some(&p), &[MAIN]).unwrap();
        assert!(c.maximized);
        assert_eq!((c.width_dips, c.height_dips), (900, 650));
    }

    #[test]
    fn a_gone_monitor_puts_the_window_in_the_middle_of_the_primary() {
        // Saved on a second monitor to the right, which is not there any more.
        let c = choose(Some(&placed(2500, 100, 1000, 700, 96)), &[MAIN]).unwrap();
        assert_eq!(c.position, None);
        assert_eq!(c.monitor, MAIN);
        let (x, y, w, h) = fit_rect(&c.monitor, c.position, 1000, 700);
        assert_eq!((w, h), (1000, 700));
        assert_eq!((x, y), (460, 170));
        // Off every monitor in every direction.
        for (x, y) in [(-5000, 0), (0, -5000), (0, 5000), (-1000, 100), (1900, 100)] {
            let c = choose(Some(&placed(x, y, 1000, 700, 96)), &[MAIN]).unwrap();
            assert_eq!(c.position, None, "({x}, {y})");
        }
        // Only a sliver of the title strip on screen is as good as gone; a grabbable piece is not.
        let c = choose(Some(&placed(1900, 100, 1000, 700, 96)), &[MAIN]).unwrap();
        assert_eq!(c.position, None);
        let c = choose(Some(&placed(1700, 100, 1000, 700, 96)), &[MAIN]).unwrap();
        assert!(c.position.is_some());
        let (x, ..) = fit_rect(&c.monitor, c.position, 1000, 700);
        assert_eq!(x, 920, "moved back inside");
    }

    #[test]
    fn the_monitor_it_was_mostly_on_wins() {
        let left = mon(-1280, 0, 1280, 984, 96, false);
        let both = [left, MAIN];
        let c = choose(Some(&placed(-1200, 50, 1000, 700, 96)), &both).unwrap();
        assert_eq!(c.monitor, left);
        assert_eq!(c.position, Some((-1200, 50)));
        // Straddling the two: the one with more of the window.
        let c = choose(Some(&placed(-300, 50, 1000, 700, 96)), &both).unwrap();
        assert_eq!(c.monitor, MAIN);
        // A gone left monitor sends the window to the primary even though coordinates are valid there.
        let c = choose(Some(&placed(-1200, 50, 1000, 700, 96)), &[MAIN]).unwrap();
        assert_eq!(c.position, None);
    }

    #[test]
    fn a_window_larger_than_every_screen_stays_on_the_monitor_of_its_corner() {
        let right = mon(1920, 0, 2560, 1400, 96, false);
        let c = choose(Some(&placed(0, 0, 9000, 9000, 96)), &[MAIN, right]).unwrap();
        assert_eq!(c.monitor, MAIN);
        let (x, y, w, h) = fit_rect(&c.monitor, c.position, c.width_dips, c.height_dips);
        assert_eq!((x, y, w, h), (0, 0, 1920, 1040));
    }

    #[test]
    fn a_different_dpi_keeps_the_logical_size() {
        // Saved at 200% as 2000 x 1400 device px: 1000 x 700 logical.
        let c = choose(Some(&placed(100, 100, 2000, 1400, 192)), &[MAIN]).unwrap();
        assert_eq!((c.width_dips, c.height_dips), (1000, 700));
        // At 100% that is 1000 x 700 device px; at 150% it would be 1500 x 1050.
        let (_, _, w, h) = fit_rect(&c.monitor, c.position, c.width_dips, c.height_dips);
        assert_eq!((w, h), (1000, 700));
        let (_, _, w, h) = fit_rect(
            &c.monitor,
            c.position,
            c.width_dips * 150 / 100,
            c.height_dips * 150 / 100,
        );
        assert_eq!((w, h), (1500, 1040), "no taller than the work area");
        // Saved at 100%, restored at 200%.
        let c = choose(
            Some(&placed(0, 0, 1000, 700, 96)),
            &[mon(0, 0, 3840, 2080, 192, true)],
        )
        .unwrap();
        assert_eq!((c.width_dips * 2, c.height_dips * 2), (2000, 1400));
    }

    #[test]
    fn never_bigger_than_the_screen_and_never_tiny() {
        let small = mon(0, 0, 1024, 728, 96, true);
        let c = choose(Some(&placed(0, 0, 3000, 2000, 96)), &[small]).unwrap();
        let (x, y, w, h) = fit_rect(&c.monitor, c.position, c.width_dips, c.height_dips);
        assert_eq!((x, y, w, h), (0, 0, 1024, 728));
        let c = choose(Some(&placed(0, 0, 50, 20, 96)), &[MAIN]).unwrap();
        assert_eq!((c.width_dips, c.height_dips), (MIN_WIDTH, MIN_HEIGHT));
        // A work area that does not start at the origin (a taskbar at the top and left).
        let work = mon(48, 40, 1872, 1000, 96, true);
        assert_eq!(
            fit_rect(&work, None, 800, 600),
            (48 + 536, 40 + 200, 800, 600)
        );
        assert_eq!(fit_rect(&work, Some((0, 0)), 800, 600), (48, 40, 800, 600));
    }
}
