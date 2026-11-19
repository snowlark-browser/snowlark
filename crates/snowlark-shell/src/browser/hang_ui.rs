// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A tab whose script does not return: once a second the window asks the active tab
//! ([`crate::watchdog`]) whether it has been stuck with the user waiting on it, and offers to wait
//! or to stop the script (the page stays). In one process that stops scripts only; a tab in a
//! renderer process whose script does not stop within a second is ended (a hang in native code).

use std::time::Instant;

use snowlark_paint::window::Window;

use super::commands::{Command, DialogKind};
use super::Browser;
use crate::dialog::Dialog;
use crate::tab::TabId;
use crate::watchdog::{ASK_AGAIN_AFTER, HANG_AFTER};

impl Browser {
    /// The once-a-second check: offers to stop the active tab's script when it is stuck, and takes
    /// the offer back when it got unstuck.
    pub(super) fn hang_timer(&mut self, _window: &Window) {
        let now = Instant::now();
        let stuck = self
            .tabs
            .active()
            .filter(|e| e.crashed.is_none())
            .and_then(|e| {
                e.tab
                    .liveness()
                    .stuck(now, HANG_AFTER)
                    .map(|pass| (e.tab.id, pass))
            });
        if let Some((_, DialogKind::TabHung { tab, pass })) = &self.dialog {
            // (the offer is for a pass that is still the one the active tab is stuck in)
            if stuck != Some((*tab, *pass)) {
                self.dialog = None;
                self.overlay_dirty = true;
            }
            return;
        }
        let Some((tab, pass)) = stuck else {
            return;
        };
        if self.dialog.is_some() || self.popup.is_some() {
            return;
        }
        if let Some((t, p, until)) = self.hang_snooze {
            if (t, p) == (tab, pass) && now < until {
                return;
            }
        }
        self.offer_stop_script(tab, pass);
    }

    fn offer_stop_script(&mut self, tab: TabId, pass: u64) {
        let entry = self.tabs.iter().find(|e| e.tab.id == tab);
        let name = entry
            .map(|e| {
                if e.tab.state.title.is_empty() {
                    e.tab.state.url.clone()
                } else {
                    e.tab.state.title.clone()
                }
            })
            .unwrap_or_default();
        // (a tab in a renderer process whose script does not stop is ended)
        let or_else = if entry.is_some_and(|e| e.tab.renderer_process().is_some()) {
            " If the script cannot be stopped, the tab is closed down and can be reloaded."
        } else {
            ""
        };
        let mut d = Dialog::new(
            "This tab is not responding",
            vec![format!(
                "A script on \"{name}\" has been running for more than {} seconds. Killing it stops the script and keeps the page.{or_else}",
                HANG_AFTER.as_secs()
            )],
            Vec::new(),
            vec![
                ("Wait".to_owned(), Command::DialogCancel, false),
                ("Kill".to_owned(), Command::DialogOk, true),
            ],
        );
        d.place(None, self.content_dips(), &self.fonts);
        self.dialog = Some((d, DialogKind::TabHung { tab, pass }));
        self.overlay_dirty = true;
    }

    /// "Wait": the same stuck pass is not offered again for a while.
    pub(super) fn wait_for_tab(&mut self, tab: TabId, pass: u64) {
        self.hang_snooze = Some((tab, pass, Instant::now() + ASK_AGAIN_AFTER));
    }

    /// "Kill": stops the script the tab is running.
    pub(super) fn kill_tab_script(&mut self, tab: TabId) {
        if let Some(e) = self.tabs.iter().find(|e| e.tab.id == tab) {
            e.tab.kill_script();
        }
    }
}
