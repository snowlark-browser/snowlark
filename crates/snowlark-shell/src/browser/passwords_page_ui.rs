// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of the passwords page (`snowlark://passwords`): opening it, deleting a login
//! or a Never entry, and revealing a password after the OS confirmed the user (Windows Hello, or
//! the Windows sign-in prompt where Hello is not set up, on a thread of its own and not asked
//! again for a minute).

use snowlark_paint::window::{UserVerification, Window};

use crate::reauth;

/// The OS's answer to a Reveal: the login it was for, how the confirmation ended, and the
/// blank-password check it leaves for the settings.
pub(super) type RevealAnswer = (u64, UserVerification, Option<reauth::BlankCheck>);

use super::settings_ui::now_ms;
use super::Browser;
use crate::passwords_page::{action, Action, NOT_SET_UP, PASSWORDS_PAGE, REVEAL_WINDOW_MS};

impl Browser {
    /// The menu's "Passwords": the tab showing the passwords page, or the page in the active tab
    /// when that is a new tab's start page, or in a new tab.
    pub(super) fn open_passwords(&mut self) {
        self.open_own_page("snowlark://passwords", PASSWORDS_PAGE);
    }

    /// The passwords page asked for something (`query`: the apply address's).
    pub(super) fn apply_passwords(&mut self, window: &Window, query: &str) {
        let Some(act) = action(query) else {
            return;
        };
        let Some(store) = self.passwords.clone() else {
            return;
        };
        self.set_password_notice(None);
        match act {
            Action::Reveal(id) => {
                let fresh = {
                    let st = self.internal.lock().unwrap_or_else(|e| e.into_inner());
                    st.passwords_verified_ms
                        .is_some_and(|at| now_ms() - at < REVEAL_WINDOW_MS)
                };
                if fresh {
                    self.reveal_password(id, now_ms());
                } else {
                    self.verify_for_reveal(window, id);
                }
            }
            Action::Delete(id) => {
                let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
                if s.remove(id) {
                    let _ = s.save();
                    s.take_change();
                }
            }
            Action::Offer(on) => {
                if self.settings.offer_passwords != on {
                    self.settings.offer_passwords = on;
                    self.save_settings();
                }
            }
            Action::NeverRemove(origin) => {
                let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
                s.remove_never(&origin);
                let _ = s.save();
                s.take_change();
            }
        }
        self.refresh_settings_pages();
    }

    /// The OS's confirmation ([`reauth::confirm_user`]: Windows Hello, else the Windows sign-in
    /// prompt), on a thread of its own; its answer comes back through
    /// [`Browser::take_password_verification`].
    fn verify_for_reveal(&mut self, window: &Window, id: u64) {
        let os = reauth::Windows {
            owner: window.handle(),
            message: "Snowlark wants to show a saved password".to_owned(),
        };
        let mut blank = self.settings.os_password;
        let waker = window.proxy();
        let slot = self.password_verification.clone();
        let _ = std::thread::Builder::new()
            .name("snowlark user verification".into())
            .spawn(move || {
                let outcome = reauth::confirm_user(&os, &mut blank);
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((id, outcome, blank));
                waker.wake();
            });
    }

    /// Woken: the OS's answer to a Reveal. Confirmed, the login's password shows on the page for
    /// a minute; when the OS could not ask, the page says so.
    pub(super) fn take_password_verification(&mut self) {
        let answer = self
            .password_verification
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some((id, outcome, blank)) = answer else {
            return;
        };
        if blank != self.settings.os_password {
            self.settings.os_password = blank;
            self.save_settings();
        }
        match outcome {
            UserVerification::Verified => self.reveal_password(id, now_ms()),
            UserVerification::Unavailable => self.set_password_notice(Some(NOT_SET_UP)),
            UserVerification::Refused => {}
        }
        self.refresh_settings_pages();
    }

    /// Records that the user may see login `id`'s password now (`at`: when the OS confirmed them,
    /// or the earlier confirmation still counting). A new confirmation starts a new list.
    fn reveal_password(&mut self, id: u64, at: i64) {
        let mut st = self.internal.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = st
            .passwords_verified_ms
            .is_some_and(|t| at - t < REVEAL_WINDOW_MS);
        if !fresh {
            st.passwords_revealed.clear();
            st.passwords_verified_ms = Some(at);
        }
        if !st.passwords_revealed.contains(&id) {
            st.passwords_revealed.push(id);
        }
    }

    fn set_password_notice(&mut self, notice: Option<&str>) {
        self.internal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .passwords_notice = notice.map(str::to_owned);
    }
}
