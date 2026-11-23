// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The password manager in the window (`docs/password-manager.md`): a tab's login form reports
//! ([`TabEvent::Passwords`]) become a pending login, judged by the next page; a successful one is
//! offered for saving in [`PasswordBubble`]; a page's login forms are filled with the one login
//! saved for its origin. Every origin is the browser's own record of the tab, never the page's.
//!
//! [`TabEvent::Passwords`]: snowlark_tab::tab::TabEvent::Passwords

use snowlark_page::passwords::PasswordEvent;
use snowlark_paint::Point;
use snowlark_tab::tab::{TabCommand, TabId};

use super::Browser;
use crate::password_bubble::{PasswordBubble, PasswordHit};
use crate::passwords::{
    choose_fill, login_failed, page_origin, pending_login, Offer, PendingLogin,
};

impl Browser {
    /// The origin of tab `i`'s top-level document for the password manager, from the browser's own
    /// record (its session history and site lock).
    fn password_origin(&self, i: usize) -> Option<String> {
        let t = &self.tabs.get(i)?.tab;
        page_origin(t.current_url()?, t.site())
    }

    /// A tab's page reported its login forms.
    pub(super) fn tab_passwords(&mut self, i: usize, id: TabId, e: PasswordEvent) {
        let Some(store) = self.passwords.clone() else {
            // (an Incognito window neither saves nor fills)
            return;
        };
        let Some(origin) = self.password_origin(i) else {
            return;
        };
        match e {
            PasswordEvent::Submitted(form) => {
                if let Some(p) = pending_login(&origin, &form) {
                    self.pending_logins.retain(|(t, _)| *t != id);
                    self.pending_logins.push((id, p));
                }
            }
            PasswordEvent::Rendered(forms) => {
                // The page after a submission: the login failed when its form is back.
                if let Some(k) = self
                    .pending_logins
                    .iter()
                    .position(|(t, p)| *t == id && p.committed)
                {
                    let (_, p) = self.pending_logins.remove(k);
                    if !login_failed(&p, &forms) {
                        self.login_succeeded(p);
                    }
                }
                let fill = {
                    let mut s = store.lock().unwrap();
                    choose_fill(&mut s, &origin, &forms)
                };
                if let Some((_, fill)) = fill {
                    self.send_to_tab(id, TabCommand::FillPassword(fill));
                }
            }
        }
    }

    /// Tab `id` committed a document: the first commit after a submission is the page that judges
    /// it; a later one ends the wait.
    pub(super) fn password_committed(&mut self, id: TabId) {
        self.pending_logins.retain_mut(|(t, p)| {
            if *t != id {
                return true;
            }
            if p.committed {
                return false;
            }
            p.committed = true;
            true
        });
    }

    fn login_succeeded(&mut self, p: PendingLogin) {
        let Some(store) = self.passwords.clone() else {
            return;
        };
        let offer = store
            .lock()
            .unwrap()
            .offer(&p.origin, &p.username, &p.password);
        let update = match offer {
            Offer::Blocked => return,
            Offer::Saved => {
                let mut s = store.lock().unwrap();
                if let Some((l, _)) = s
                    .logins_for(&p.origin)
                    .into_iter()
                    .find(|(l, _)| l.username == p.username)
                {
                    s.mark_used(l.id);
                    let _ = s.save();
                }
                return;
            }
            Offer::Update(id) => Some(id),
            Offer::Save => None,
        };
        let host = snowlark_net::Url::parse(&p.origin)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| p.origin.clone());
        let area = self.content_dips();
        self.password_bubble = Some((PasswordBubble::new(update, &p.username, &host, area), p));
        self.overlay_dirty = true;
    }

    fn answer_password_bubble(&mut self, hit: PasswordHit) {
        let Some((bubble, p)) = self.password_bubble.take() else {
            return;
        };
        self.overlay_dirty = true;
        let Some(store) = self.passwords.clone() else {
            return;
        };
        let mut s = store.lock().unwrap();
        match (hit, bubble.update) {
            (PasswordHit::Accept, _) => {
                s.save_login(
                    &p.origin,
                    &p.username,
                    &p.username_element,
                    &p.password_element,
                    &p.password,
                );
            }
            (PasswordHit::Decline, None) => s.add_never(&p.origin),
            _ => return,
        }
        let _ = s.save();
    }

    /// A press while the prompt shows; whether it was on the prompt.
    pub(super) fn password_bubble_press(&mut self, cp: Point) -> bool {
        let Some((b, _)) = &self.password_bubble else {
            return false;
        };
        if !b.rect.contains(cp) {
            return false;
        }
        let hit = b.hit(cp, &self.fonts);
        self.swallow_release = true;
        if let Some(hit) = hit {
            self.answer_password_bubble(hit);
        }
        true
    }

    /// The pointer moved; whether the prompt is under it.
    pub(super) fn password_bubble_move(&mut self, cp: Point) -> bool {
        let Some((b, _)) = &mut self.password_bubble else {
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

    pub(super) fn place_password_bubble(&mut self, area: snowlark_paint::Size) {
        if let Some((b, _)) = &mut self.password_bubble {
            b.place(area);
        }
    }

    pub(super) fn paint_password_bubble(
        &self,
        b: &mut snowlark_paint::DisplayListBuilder,
        theme: &crate::chrome::Theme,
    ) {
        if let Some((bubble, _)) = &self.password_bubble {
            bubble.paint(b, &self.fonts, theme);
        }
    }
}
