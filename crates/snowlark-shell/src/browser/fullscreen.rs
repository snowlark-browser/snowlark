// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fullscreen: the browser's own (F11) and a page's (the Fullscreen API), as two separate states.
//!
//! Either one shows the window borderless over its whole monitor ([`Window::set_fullscreen`]) and
//! hides the tab strip, toolbar and bookmarks bar (the chrome is zero high). A page asks with
//! [`TabEvent::Fullscreen`]: the browser grants only the active tab's page, with a user activation
//! the browser saw itself (input it gave that tab in the last five seconds, so a renderer cannot
//! fullscreen the window on its own), and answers with [`TabCommand::Fullscreen`] once the window has
//! its new size and the tab its new viewport (so the page's `resize` and `fullscreenchange` come in
//! the same rendering update). Leaving: the page asks, or the user presses Esc (which the page then
//! does not get), switches or closes the tab, the tab navigates to another document or its renderer
//! crashes; the page is then told its viewport is no longer fullscreen. A page's fullscreen inside
//! F11 fullscreen leaves the window as it is, and leaving it goes back to F11 fullscreen; F11 during
//! a page's fullscreen leaves both.
//!
//! A bubble at the top says how to leave ("Press Esc to exit full screen"; "Press F11 ..."), fading
//! in and out, for a few seconds after entering.
//!
//! [`TabEvent::Fullscreen`]: snowlark_tab::tab::TabEvent::Fullscreen
//! [`TabCommand::Fullscreen`]: snowlark_tab::tab::TabCommand::Fullscreen

use std::time::{Duration, Instant};

use snowlark_paint::window::Window;
use snowlark_paint::{Color, DisplayListBuilder, Radii, Rect};
use snowlark_tab::tab::{TabCommand, TabId};

use super::Browser;
use crate::chrome::{Face, Fonts};

/// How long a user activation the browser saw allows a fullscreen request (HTML's transient
/// activation duration, as the page measures it).
const ACTIVATION: Duration = Duration::from_secs(5);
/// The bubble shows this long, fading in and out within it.
const BUBBLE_SHOWN: Duration = Duration::from_millis(3800);
const FADE_IN: f32 = 350.0;
const FADE_OUT: f32 = 700.0;
/// How long a tab's answer waits for the window's new size before it goes anyway.
const RESIZE_WAIT: Duration = Duration::from_millis(400);
pub(super) const TIMER_FULLSCREEN: usize = 10;

#[derive(Default)]
pub(super) struct FullscreenUi {
    /// The browser's own fullscreen (F11).
    pub browser: bool,
    /// The tab whose page is fullscreen.
    pub tab: Option<TabId>,
    /// The tab to tell its new state (fullscreen or not) once the window has its new size, and
    /// since when.
    pending: Option<(TabId, bool, Instant)>,
    /// The last user input given to a tab (a press, a key other than Esc).
    activation: Option<(TabId, Instant)>,
    bubble: Option<(&'static str, Instant)>,
}

impl FullscreenUi {
    /// Whether the window is fullscreen (the chrome is hidden).
    pub fn on(&self) -> bool {
        self.browser || self.tab.is_some()
    }

    /// User input reached tab `tab` (an activation-triggering input event).
    pub fn activated(&mut self, tab: TabId) {
        self.activation = Some((tab, Instant::now()));
    }

    fn has_activation(&self, tab: TabId) -> bool {
        self.activation
            .is_some_and(|(t, at)| t == tab && at.elapsed() < ACTIVATION)
    }

    /// The bubble's opacity now (0 when it is gone).
    fn bubble_opacity(&self) -> f32 {
        let Some((_, at)) = self.bubble else {
            return 0.0;
        };
        let t = at.elapsed().as_secs_f32() * 1000.0;
        let total = BUBBLE_SHOWN.as_secs_f32() * 1000.0;
        if t >= total {
            0.0
        } else if t < FADE_IN {
            t / FADE_IN
        } else if t > total - FADE_OUT {
            (total - t) / FADE_OUT
        } else {
            1.0
        }
    }
}

impl Browser {
    /// A page asks for fullscreen (`on`) or to leave it.
    pub(super) fn tab_asks_fullscreen(&mut self, window: &Window, tab: TabId, on: bool) {
        if on {
            let active = self.active_id() == Some(tab);
            if !active || !self.fullscreen.has_activation(tab) || self.dialog.is_some() {
                self.send_to_tab(tab, TabCommand::Fullscreen(false));
                return;
            }
            // (the activation is used up, as the page's own is)
            self.fullscreen.activation = None;
            if self.fullscreen.tab == Some(tab) {
                self.send_to_tab(tab, TabCommand::Fullscreen(true));
                return;
            }
            let was_on = self.fullscreen.on();
            self.fullscreen.tab = Some(tab);
            self.fullscreen.bubble = Some(("Press Esc to exit full screen", Instant::now()));
            window.set_timer(TIMER_FULLSCREEN, 16);
            self.overlay_dirty = true;
            if was_on {
                self.send_to_tab(tab, TabCommand::Fullscreen(true));
            } else {
                self.fullscreen.pending = Some((tab, true, Instant::now()));
                self.window_fullscreen(window, true);
            }
        } else if self.fullscreen.tab == Some(tab) {
            self.exit_tab_fullscreen(window);
        } else {
            // (not fullscreen here: the page's exit needs no window change)
            self.send_to_tab(tab, TabCommand::Fullscreen(false));
        }
    }

    /// The page's fullscreen ends (its own exit, or the user's): the window goes back to how it
    /// was, then the page is told.
    pub(super) fn exit_tab_fullscreen(&mut self, window: &Window) {
        let Some(tab) = self.fullscreen.tab.take() else {
            return;
        };
        self.fullscreen.bubble = None;
        self.overlay_dirty = true;
        if self.fullscreen.browser {
            self.send_to_tab(tab, TabCommand::Fullscreen(false));
        } else {
            self.fullscreen.pending = Some((tab, false, Instant::now()));
            window.set_timer(TIMER_FULLSCREEN, 16);
            self.window_fullscreen(window, false);
        }
    }

    /// F11: the browser's own fullscreen on or off (off ends a page's fullscreen too).
    pub(super) fn toggle_browser_fullscreen(&mut self, window: &Window) {
        if self.fullscreen.on() {
            self.fullscreen.browser = false;
            if self.fullscreen.tab.is_some() {
                self.exit_tab_fullscreen(window);
            } else {
                self.fullscreen.bubble = None;
                self.window_fullscreen(window, false);
            }
        } else {
            self.fullscreen.browser = true;
            self.fullscreen.bubble = Some(("Press F11 to exit full screen", Instant::now()));
            window.set_timer(TIMER_FULLSCREEN, 16);
            self.window_fullscreen(window, true);
        }
        self.overlay_dirty = true;
    }

    fn window_fullscreen(&mut self, window: &Window, on: bool) {
        // (the menus and panels anchored to the chrome close with it)
        self.close_menu(None);
        self.close_popup();
        self.close_site_info();
        self.close_blocker_panel();
        self.chrome_dirty = true;
        if window.is_fullscreen() != on {
            window.set_fullscreen(on);
        } else {
            // (no new size is coming: the chrome's height changed alone)
            self.resized();
        }
    }

    /// The window has its new size and the tabs their new viewports: the page waiting for its
    /// answer gets it.
    pub(super) fn fullscreen_after_resize(&mut self) {
        if let Some((tab, on, _)) = self.fullscreen.pending.take() {
            self.send_to_tab(tab, TabCommand::Fullscreen(on));
        }
    }

    /// Before each frame of the window: a page's fullscreen ends when its tab is no longer the
    /// active one (another tab, the tab closed) or its renderer crashed.
    pub(super) fn fullscreen_sync(&mut self, window: &Window) {
        if let Some(tab) = self.fullscreen.tab {
            let crashed = self
                .tabs
                .active()
                .is_some_and(|e| e.tab.id == tab && e.crashed.is_some());
            if self.active_id() != Some(tab) || crashed {
                self.exit_tab_fullscreen(window);
            }
        }
    }

    /// The fullscreen timer: the bubble's fade, and an answer whose resize never came.
    pub(super) fn fullscreen_tick(&mut self, window: &Window) {
        if self
            .fullscreen
            .pending
            .is_some_and(|(_, _, at)| at.elapsed() > RESIZE_WAIT)
        {
            self.resized();
        }
        if self.fullscreen.bubble.is_some() {
            self.overlay_dirty = true;
            if self.fullscreen.bubble_opacity() <= 0.0
                && self
                    .fullscreen
                    .bubble
                    .is_some_and(|(_, at)| at.elapsed() > BUBBLE_SHOWN)
            {
                self.fullscreen.bubble = None;
            }
        }
        if self.fullscreen.bubble.is_none() && self.fullscreen.pending.is_none() {
            window.kill_timer(TIMER_FULLSCREEN);
        }
    }

    pub(super) fn send_to_tab(&self, tab: TabId, c: TabCommand) {
        if let Some(e) = self
            .tabs
            .iter()
            .find(|e| e.tab.id == tab)
            .filter(|e| e.crashed.is_none())
        {
            e.tab.send(c);
        }
    }

    /// The bubble, centred near the top of the window.
    pub(super) fn paint_fullscreen_bubble(&self, b: &mut DisplayListBuilder) {
        let Some((text, _)) = self.fullscreen.bubble else {
            return;
        };
        let opacity = self.fullscreen.bubble_opacity();
        if opacity <= 0.0 {
            return;
        }
        let area = self.page_dips();
        paint_bubble(b, &self.fonts, text, area.width, opacity);
    }
}

/// "Press <key> to exit full screen", the key in a key cap, on a dark rounded box.
fn paint_bubble(b: &mut DisplayListBuilder, fonts: &Fonts, text: &str, width: f32, opacity: f32) {
    const SIZE: f32 = 15.0;
    const HEIGHT: f32 = 46.0;
    let (before, key, after) = split_key(text);
    let wb = fonts.width(before, SIZE);
    let wk = fonts.width(key, SIZE) + 16.0;
    let wa = fonts.width(after, SIZE);
    let w = wb + wk + wa + 48.0;
    let r = Rect::new(((width - w) / 2.0).max(0.0), 45.0, w, HEIGHT);
    b.push_opacity(opacity, r);
    b.fill_rounded_rect(r, Radii::uniform(8.0), Color::rgba(32, 33, 36, 230));
    let white = Color::rgba(255, 255, 255, 255);
    let cy = r.y + HEIGHT / 2.0;
    let mut x = r.x + 24.0;
    fonts.label_in(b, Face::Ui, before, x, cy, SIZE, white, wb + 1.0);
    x += wb;
    let cap = Rect::new(x + 2.0, cy - 13.0, wk - 4.0, 26.0);
    b.fill_rounded_rect(cap, Radii::uniform(5.0), Color::rgba(255, 255, 255, 220));
    b.fill_rounded_rect(
        cap.inflate(-1.5, -1.5),
        Radii::uniform(4.0),
        Color::rgba(32, 33, 36, 255),
    );
    fonts.label_in(b, Face::Ui, key, x + 8.0, cy, SIZE, white, wk);
    x += wk;
    fonts.label_in(b, Face::Ui, after, x, cy, SIZE, white, wa + 1.0);
    b.pop_opacity();
}

/// "Press Esc to exit full screen" as ("Press ", "Esc", " to exit full screen").
fn split_key(text: &str) -> (&str, &str, &str) {
    let start = text.find(' ').map_or(0, |i| i + 1);
    let end = text[start..].find(' ').map_or(text.len(), |i| start + i);
    (&text[..start], &text[start..end], &text[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bubble_names_the_key() {
        assert_eq!(
            split_key("Press Esc to exit full screen"),
            ("Press ", "Esc", " to exit full screen")
        );
        assert_eq!(split_key("Press F11 to exit full screen").1, "F11");
    }

    #[test]
    fn activation_is_the_tabs_own_and_recent() {
        let mut f = FullscreenUi::default();
        assert!(!f.has_activation(1));
        f.activated(1);
        assert!(f.has_activation(1));
        assert!(!f.has_activation(2));
        f.activation = Some((1, Instant::now() - ACTIVATION - Duration::from_millis(1)));
        assert!(!f.has_activation(1));
    }

    #[test]
    fn the_bubble_fades_in_and_out() {
        let mut f = FullscreenUi::default();
        assert_eq!(f.bubble_opacity(), 0.0);
        f.bubble = Some(("x", Instant::now()));
        assert!(f.bubble_opacity() < 0.5);
        f.bubble = Some(("x", Instant::now() - Duration::from_millis(1000)));
        assert_eq!(f.bubble_opacity(), 1.0);
        f.bubble = Some(("x", Instant::now() - BUBBLE_SHOWN));
        assert_eq!(f.bubble_opacity(), 0.0);
    }
}
