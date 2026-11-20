// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of session restore: keeping the session file up to date, the "Restore pages?"
//! prompt after an unexpected exit, reopening the tabs (the active one loads at once, the others when
//! they are first selected) and marking a normal close.

use snowlark_paint::Point;

use super::Browser;
use crate::restore::{self, SavedTab};
use crate::restore_bubble::{BubbleHit, RestoreBubble};
use crate::tabs::Placement;

impl Browser {
    /// Opens the profile's session store and, when the last run ended unexpectedly, the prompt.
    /// `SNOWLARK_NO_SESSION=1` turns it off.
    pub(super) fn start_session(&mut self) {
        if std::env::var_os("SNOWLARK_NO_SESSION").is_some_and(|v| v == "1") {
            return;
        }
        let (store, offer) = restore::Store::open(&self.profile.dir);
        self.session = Some(store);
        if let Some(offer) = offer {
            let area = self.content_dips();
            self.restore_bubble = Some(RestoreBubble::new(offer.tabs.len(), area));
            self.offer = Some(offer);
            self.overlay_dirty = true;
        }
    }

    /// Keeps the window's placement in the profile: when it changed (checked once a second, the
    /// write on a thread of its own), and for the last time as the window closes.
    pub(super) fn save_window(&mut self, window: &snowlark_paint::window::Window, last: bool) {
        // (an Incognito window's placement is not kept)
        let Some(p) = window.placement().filter(|_| !self.off_the_record()) else {
            return;
        };
        if self.placed == Some(p) {
            return;
        }
        self.placed = Some(p);
        let file = self.profile.window_file();
        if last {
            let _ = crate::window_state::save(&file, &p);
        } else {
            let _ = std::thread::Builder::new()
                .name("snowlark window state".into())
                .spawn(move || {
                    let _ = crate::window_state::save(&file, &p);
                });
        }
    }

    /// The tabs as the session file keeps them, and the index of the active one among them.
    fn session_snapshot(&self) -> (Vec<SavedTab>, usize) {
        let active = self.tabs.active_index();
        let mut out = Vec::new();
        let mut at = 0;
        for (i, e) in self.tabs.iter().enumerate() {
            let saved = match &e.lazy {
                Some(l) => Some((**l).clone()),
                None => {
                    let scroll = self.comp.scroll_view(e.tab.id).scroll;
                    SavedTab::from_trail(&e.tab.trail(), scroll).map(|mut t| {
                        t.zoom = e.zoom;
                        t.muted = e.mute.muted && !e.mute.by_site;
                        t
                    })
                }
            };
            if let Some(t) = saved {
                if i == active {
                    at = out.len();
                }
                out.push(t);
            }
        }
        (out, at)
    }

    /// Hands the session file's writer the tabs as they are (it writes when they changed).
    pub(super) fn save_session(&mut self) {
        if self.session.is_none() {
            return;
        }
        let (tabs, active) = self.session_snapshot();
        if let Some(s) = &mut self.session {
            s.update(tabs, active);
        }
    }

    /// The window closes for real (or Windows ends the session): the file says so.
    pub(super) fn close_session(&mut self) {
        self.save_session();
        if let Some(s) = &mut self.session {
            s.close_clean();
        }
    }

    /// The active tab shows for the first time since a restore: its history loads.
    pub(super) fn load_lazy(&mut self) {
        let Some(e) = self.tabs.active_mut() else {
            return;
        };
        let Some(saved) = e.lazy.take() else {
            return;
        };
        e.tab.state.loading = true;
        e.tab.restore(saved.to_trail());
    }

    /// Reopens the tabs of the offered session (replacing a lone start page).
    pub(super) fn restore_session(&mut self) {
        let Some(offer) = self.offer.take() else {
            return;
        };
        self.dismiss_restore();
        let lone_start_page = self.tabs.len() == 1
            && self.active_url() == crate::address::START_PAGE
            && self.tabs.active().is_some_and(|e| e.tab.history_len() <= 1);
        if lone_start_page {
            if let Some(e) = self.tabs.remove(0) {
                self.comp.send(super::CompMsg::RemoveTab(e.tab.id));
            }
        }
        let base = self.tabs.len();
        let blank = snowlark_net::Url::parse("about:blank").expect("about:blank");
        for t in offer.tabs {
            let entry = self.make_tab(blank.clone(), Some(t));
            self.tabs.add(entry, Placement::End);
        }
        self.tabs.select(base + offer.active);
        self.activated();
        self.save_session();
        self.chrome_dirty = true;
    }

    /// The prompt is answered: it and the offer go.
    pub(super) fn dismiss_restore(&mut self) {
        restore::clear_offer(&self.profile.dir);
        self.offer = None;
        if self.restore_bubble.take().is_some() {
            self.overlay_dirty = true;
        }
    }

    /// A press while the prompt shows; whether it was on the prompt.
    pub(super) fn restore_bubble_press(&mut self, cp: Point) -> bool {
        let Some(b) = &self.restore_bubble else {
            return false;
        };
        if !b.rect.contains(cp) {
            return false;
        }
        let hit = b.hit(cp, &self.fonts);
        self.swallow_release = true;
        match hit {
            Some(BubbleHit::Restore) => self.restore_session(),
            Some(BubbleHit::Close) => self.dismiss_restore(),
            None => {}
        }
        true
    }

    /// The pointer moved; whether the prompt is under it.
    pub(super) fn restore_bubble_move(&mut self, cp: Point) -> bool {
        let Some(b) = &mut self.restore_bubble else {
            return false;
        };
        if !b.rect.contains(cp) {
            if b.hover.take().is_some() {
                self.overlay_dirty = true;
            }
            return false;
        }
        let h = b.hit(cp, &self.fonts);
        if h != b.hover {
            b.hover = h;
            self.overlay_dirty = true;
        }
        true
    }

    pub(super) fn paint_restore_bubble(
        &self,
        b: &mut snowlark_paint::DisplayListBuilder,
        theme: &crate::chrome::Theme,
    ) {
        if let Some(bubble) = &self.restore_bubble {
            bubble.paint(b, &self.fonts, theme);
        }
    }
}
