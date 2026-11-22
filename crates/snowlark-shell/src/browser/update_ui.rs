// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of the updates ([`crate::update`]): the question on the first start, the
//! check started while the setting says yes, the prompt once a new version is ready, and the
//! restart into it, which brings the tabs back.

use snowlark_paint::window::Window;
use snowlark_paint::Point;

use super::Browser;
use crate::tabs::Placement;
use crate::update::Outcome;
use crate::update_bubble::{UpdateBubble, UpdateHit, UpdatePrompt};

impl Browser {
    /// A regular window opened: the windows a restart into an update left open again in it (the
    /// first one here, the others in windows of their own), with nothing asked.
    pub(super) fn reopen_after_restart(&mut self) {
        if self.offer.is_some() {
            return;
        }
        let Some(file) = self.app.restart_session.borrow_mut().take() else {
            return;
        };
        let mut windows = file.windows.into_iter();
        if let Some(first) = windows.next() {
            self.restore_window(first);
        }
        for w in windows {
            let placement = w.placement.map(Into::into);
            self.app.open_window(None, placement, Some(w));
        }
    }

    /// Once a second in a regular window: the check runs while the setting says yes; the question
    /// is asked once (not over the restore prompt); what the check found is shown once.
    pub(super) fn update_tick(&mut self) {
        if self.off_the_record() || self.popup_window || !crate::update::Updates::available() {
            return;
        }
        let updates = &self.app.updates;
        updates.set_wanted(self.settings.update_check == Some(true), &self.profile.dir);
        if self.update_bubble.is_some() || self.restore_bubble.is_some() || self.dialog.is_some() {
            return;
        }
        let prompt = if self.settings.update_check.is_none() && !updates.asked.get() {
            updates.asked.set(true);
            Some(UpdatePrompt::Consent)
        } else if updates.told.get() || self.settings.update_check != Some(true) {
            None
        } else {
            match updates.outcome() {
                Some(Outcome::Staged(_)) => Some(UpdatePrompt::Restart),
                Some(Outcome::CannotWrite { page, .. }) => Some(UpdatePrompt::Manual { page }),
                _ => None,
            }
        };
        let Some(prompt) = prompt else {
            return;
        };
        if prompt != UpdatePrompt::Consent {
            updates.told.set(true);
        }
        if crate::update::trace() {
            eprintln!("[update] prompt: {prompt:?}");
        }
        self.update_bubble = Some(UpdateBubble::new(prompt, self.content_dips(), &self.fonts));
        self.overlay_dirty = true;
    }

    /// The question's answer: kept in the settings (and the check starts on the next tick).
    fn answer_update_question(&mut self, yes: bool) {
        self.settings.update_check = Some(yes);
        self.save_settings();
        self.app.updates.set_wanted(yes, &self.profile.dir);
    }

    /// "Restart": the tabs of every window are kept, every window closes, and the new version
    /// starts when the last one has.
    fn restart_into_update(&mut self, window: &Window) {
        self.save_window(window, true);
        self.save_session();
        self.app.restart_into_update(&self.me);
        window.close();
    }

    /// Woken: the user chose to restart into an update in another window, so this one closes too.
    pub(super) fn closing_for_restart(&mut self, window: &Window) -> bool {
        if !self.app.updates.restart.get() {
            return false;
        }
        window.close();
        true
    }

    /// A press while the prompt shows; whether it was on the prompt.
    pub(super) fn update_bubble_press(&mut self, window: &Window, cp: Point) -> bool {
        let Some(b) = &self.update_bubble else {
            return false;
        };
        if !b.rect.contains(cp) {
            return false;
        }
        let hit = b.hit(cp, &self.fonts);
        let prompt = b.prompt.clone();
        self.swallow_release = true;
        let Some(hit) = hit else {
            return true;
        };
        self.update_bubble = None;
        self.overlay_dirty = true;
        match (prompt, hit) {
            // (closed unanswered: asked again on the next start)
            (_, UpdateHit::Close) => {}
            (UpdatePrompt::Consent, UpdateHit::Button(i)) => self.answer_update_question(i == 1),
            (UpdatePrompt::Restart, UpdateHit::Button(_)) => self.restart_into_update(window),
            (UpdatePrompt::Manual { page }, UpdateHit::Button(_)) => {
                let url = crate::update::feed::is_https_or_local(&page)
                    .then(|| snowlark_net::Url::parse(&page).ok())
                    .flatten();
                if let Some(url) = url {
                    self.open_tab(url, Placement::End);
                    self.chrome_dirty = true;
                }
            }
        }
        true
    }

    /// The pointer moved; whether the prompt is under it.
    pub(super) fn update_bubble_move(&mut self, cp: Point) -> bool {
        let Some(b) = &mut self.update_bubble else {
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

    pub(super) fn paint_update_bubble(
        &self,
        b: &mut snowlark_paint::DisplayListBuilder,
        theme: &crate::chrome::Theme,
    ) {
        if let Some(bubble) = &self.update_bubble {
            bubble.paint(b, &self.fonts, theme);
        }
    }
}
