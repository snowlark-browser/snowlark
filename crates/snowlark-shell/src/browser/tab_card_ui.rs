// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of the tab hover card: when the pointer has rested on a tab, the card shows
//! (the title, the site, the audio state) and the tab is asked how much memory it uses; the answer
//! fills in the card's memory row when it arrives, and is asked again every few seconds while the
//! card stays. The window never waits for the tab: a busy tab answers late (the card has no figure
//! until it does), a crashed or not yet loaded tab is not asked.

use snowlark_net::Url;
use snowlark_paint::window::Window;
use snowlark_paint::Rect;

use super::{Browser, TIMER_CARD};
use crate::chrome::Hit;
use crate::tab::{MemoryUse, TabCommand, TabId};
use crate::tab_card::TabCard;

/// How often a card that stays asks its tab again.
pub(super) const REFRESH_MS: u32 = 3000;

impl Browser {
    /// The tab a chrome element belongs to, when resting on it shows the card.
    pub(super) fn tab_card_target(hit: Hit) -> Option<usize> {
        match hit {
            Hit::Tab(i) | Hit::TabClose(i) | Hit::TabAudio(i) => Some(i),
            _ => None,
        }
    }

    /// Whether tab `i` can be asked for its memory (it has a page on a thread that runs).
    fn tab_measurable(&self, i: usize) -> bool {
        self.tabs
            .get(i)
            .is_some_and(|e| e.crashed.is_none() && e.lazy.is_none())
    }

    /// The pointer rested on tab `i`: the card shows, and the tab is asked about its memory.
    pub(super) fn show_tab_card(&mut self, window: &Window, i: usize) {
        let Some(e) = self.tabs.get(i) else {
            return;
        };
        let Some(anchor) = self.layout().tabs.get(i).copied() else {
            return;
        };
        let title = if e.tab.state.title.is_empty() {
            crate::address::display_url(&e.tab.state.url)
        } else {
            e.tab.state.title.clone()
        };
        let site = Url::parse(&e.tab.state.url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let audio = self.tab_audio(i).tooltip();
        let area = self.content_dips();
        let side = self.side_tabs_on();
        let anchor = if side {
            Rect::new(
                anchor.x,
                anchor.y - self.chrome_h(),
                anchor.width,
                anchor.height,
            )
        } else {
            anchor
        };
        let card = TabCard::new(e.tab.id, anchor, &title, &site, audio, area, &self.fonts);
        self.tab_card = Some(if side { card.beside(&self.fonts) } else { card });
        self.overlay_dirty = true;
        if self.tab_measurable(i) {
            e.tab.send(TabCommand::Memory);
            window.set_timer(TIMER_CARD, REFRESH_MS);
        }
    }

    /// A tab said how much memory it uses.
    pub(super) fn tab_memory(&mut self, id: TabId, m: MemoryUse) {
        if let Some(c) = &mut self.tab_card {
            if c.tab == id {
                c.set_memory(m.total(), &self.fonts);
                self.overlay_dirty = true;
            }
        }
    }

    /// The card has stayed: ask again.
    pub(super) fn refresh_tab_card(&mut self, window: &Window) {
        let Some(id) = self.tab_card.as_ref().map(|c| c.tab) else {
            window.kill_timer(TIMER_CARD);
            return;
        };
        match self.tabs.position(|e| e.tab.id == id) {
            Some(i) if self.tab_measurable(i) => {
                if let Some(e) = self.tabs.get(i) {
                    e.tab.send(TabCommand::Memory);
                }
            }
            _ => window.kill_timer(TIMER_CARD),
        }
    }
}
