// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of the history page: opening it, and removing the visits its form names.

use snowlark_net::Url;

use super::Browser;
use crate::history_page::{removals, HISTORY_PAGE};

impl Browser {
    /// Ctrl+H, the menu's "History": the tab showing the history page, or the page in the
    /// active tab when that is a new tab's start page, or in a new tab.
    pub(super) fn open_history(&mut self) {
        self.open_own_page("snowlark://history", HISTORY_PAGE);
    }

    /// A page of the browser's own (`page`, at addresses starting with `prefix`): the tab showing
    /// it, or the page in the active tab when that is a new tab's start page, or in a new tab.
    pub(super) fn open_own_page(&mut self, prefix: &str, page: &str) {
        if let Some(i) = self.tabs.position(|e| e.tab.state.url.starts_with(prefix)) {
            self.tabs.select(i);
            self.activated();
            return;
        }
        let Ok(url) = Url::parse(page) else {
            return;
        };
        let start = self
            .tabs
            .active()
            .is_some_and(|e| e.crashed.is_none() && e.tab.state.url == crate::address::START_PAGE);
        if start {
            if let Some(tab) = self.active_host() {
                tab.navigate(url);
                return;
            }
        }
        self.open_tab(url, crate::tabs::Placement::Foreground);
    }

    /// The history page's form was submitted with `query`: the rows it names lose their visits
    /// (the page reloads to show what is left).
    pub(super) fn apply_history(&mut self, query: &str) {
        let list = removals(query);
        let changed = {
            let mut h = self.history.lock().unwrap_or_else(|e| e.into_inner());
            let mut changed = false;
            for r in &list {
                changed |= h.remove_visits(&r.url, r.first, r.last);
            }
            if changed {
                let _ = h.save();
            }
            changed
        };
        if changed {
            self.refresh_settings_pages();
        }
    }
}
