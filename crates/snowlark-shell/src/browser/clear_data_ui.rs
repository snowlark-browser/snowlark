// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of deleting browsing data: opening the dialog, and deleting what it chose
//! from the browser's stores and the tabs' session storage.

use snowlark_net::Url;

use super::Browser;
use crate::clear_data::{delete, Choice, Stores, CLEAR_PAGE};
use crate::tab::TabCommand;

impl Browser {
    /// Ctrl+Shift+Delete: the dialog, in the settings tab (or a new one).
    pub(super) fn open_clear_browsing_data(&mut self) {
        let Ok(url) = Url::parse(CLEAR_PAGE) else {
            return;
        };
        if let Some(i) = self
            .tabs
            .position(|e| crate::about::is_settings_tab(&e.tab.state.url))
        {
            self.tabs.select(i);
            self.activated();
            if let Some(tab) = self.active_host() {
                tab.navigate(url);
            }
            return;
        }
        self.open_tab(url, crate::tabs::Placement::Foreground);
    }

    /// The dialog's "Delete data": deletes what `choice` names, keeps the choice for next time.
    pub(super) fn clear_browsing_data(&mut self, choice: Choice) {
        choice.keep(&mut self.settings);
        let _ = self.settings.save(&self.profile.settings_file());
        if choice.any() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            delete(
                &Stores {
                    history: &self.history,
                    net: &self.tab_ctx.net,
                    local_storage: &self.local_storage,
                    code_cache: self.code_cache.as_deref(),
                },
                choice,
                now,
            );
            if choice.history {
                // (the closed tabs' list forgets the pages the history no longer has)
                let h = self.history.lock().unwrap_or_else(|e| e.into_inner());
                self.closed
                    .retain(|u| h.entries().iter().any(|e| e.url == *u));
            }
            if choice.cookies {
                // Session storage lives in the tabs, and goes whatever the range.
                for e in self.tabs.iter().filter(|e| e.crashed.is_none()) {
                    e.tab.send(TabCommand::ClearSiteData(None));
                }
            }
        }
        self.settings_notice = None;
        self.refresh_settings_pages();
        // The dialog closes: its tabs show Privacy and security again (after their reload).
        if let Ok(privacy) = Url::parse(&crate::settings::Category::Privacy.url()) {
            for e in self.tabs.iter_mut().filter(|e| e.crashed.is_none()) {
                if e.tab.state.url.starts_with(CLEAR_PAGE) {
                    e.tab.navigate(privacy.clone());
                }
            }
        }
        self.overlay_dirty = true;
    }
}
