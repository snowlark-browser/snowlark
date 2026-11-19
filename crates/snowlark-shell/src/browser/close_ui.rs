// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Closing the window: with more than one tab open, the window first asks whether to close them
//! all ("Close all tabs?"), unless the user said not to ask again (the `warnOnClose` setting, also
//! on the settings page). Chromium has no such question (it asks only for pages' `beforeunload`);
//! this is the shell's own, and pages observe nothing of it.

use snowlark_paint::window::Window;

use super::commands::{Command, DialogKind};
use super::Browser;
use crate::dialog::{Dialog, Field};

impl Browser {
    /// The close button, Alt+F4, the taskbar's "Close window" or the menu's Exit: closes the
    /// window, or asks first.
    pub(super) fn request_close(&mut self, window: &Window) {
        let n = self.tabs.len();
        if n < 2 || !self.settings.warn_on_close {
            window.close();
            return;
        }
        // (asking again while the question shows keeps it as it is)
        if matches!(self.dialog, Some((_, DialogKind::CloseWindow))) {
            window.unminimize();
            return;
        }
        // Another dialog gives way, as its Cancel would.
        if self.dialog.is_some() {
            self.dialog_button(Command::DialogCancel);
        }
        self.close_menu(None);
        self.close_popup();
        let mut d = Dialog::new(
            "Close all tabs?",
            vec![format!("You have {n} tabs open in this browser window.")],
            vec![Field::Check {
                label: "Don't ask me again".to_owned(),
                checked: false,
            }],
            vec![
                ("Close all".to_owned(), Command::DialogOk, true),
                ("Cancel".to_owned(), Command::DialogCancel, false),
            ],
        );
        d.place(None, self.content_dips(), &self.fonts);
        self.dialog = Some((d, DialogKind::CloseWindow));
        self.overlay_dirty = true;
        window.unminimize();
    }

    /// "Close all": keeps "Don't ask me again" and closes the window.
    pub(super) fn close_confirmed(&mut self, window: &Window) {
        let Some((d, _)) = self.dialog.take() else {
            return;
        };
        self.overlay_dirty = true;
        if d.checked(0) && self.settings.warn_on_close {
            self.settings.warn_on_close = false;
            let _ = self.settings.save(&self.profile.settings_file());
            self.refresh_settings_pages();
        }
        window.close();
    }
}
