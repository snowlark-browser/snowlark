// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Confirming the signed-in Windows user before the browser shows a saved password (the
//! passwords page's Reveal). Windows Hello first; where it is not set up, the Windows sign-in
//! prompt for the current user. An account with no Windows password at all, on a machine that is
//! not in a domain, has nothing to confirm with and passes, as the OS itself lets anyone at the
//! machine in. The OS calls are behind [`SystemAuth`], so the order of the checks is tested
//! without a prompt ever opening.

use serde::{Deserialize, Serialize};
use snowlark_paint::window::{self, UserVerification, WindowHandle};

/// The OS's checks, in the order [`confirm_user`] asks them.
pub trait SystemAuth {
    /// Windows Hello's prompt (face, fingerprint or PIN).
    fn hello(&self) -> UserVerification;
    /// Whether the machine is in a domain (its accounts always have a password).
    fn domain_joined(&self) -> bool;
    /// When the account's Windows password was last changed (ms since the Unix epoch), `None`
    /// when the OS does not say.
    fn password_changed_ms(&self) -> Option<i64>;
    /// Whether the account's Windows password is blank. A try with the wrong (empty) password
    /// counts against the account, so its answer is kept in [`BlankCheck`].
    fn password_blank(&self) -> bool;
    /// The Windows sign-in prompt, checked as the current user's.
    fn sign_in(&self) -> UserVerification;
}

/// The last answer to [`SystemAuth::password_blank`], kept in the settings: asked again only
/// after the password changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlankCheck {
    /// When the password had last changed at that check (plus a second for clock skew).
    pub changed_ms: i64,
    pub blank: bool,
}

/// Asks the OS to confirm the user: Windows Hello, then, where it is not set up, the Windows
/// sign-in prompt. `blank` is the remembered blank-password check, updated when it was redone.
pub fn confirm_user(os: &dyn SystemAuth, blank: &mut Option<BlankCheck>) -> UserVerification {
    match os.hello() {
        UserVerification::Unavailable => {}
        answer => return answer,
    }
    if !has_sign_in(os, blank) {
        return UserVerification::Verified;
    }
    os.sign_in()
}

/// Whether the account has a sign-in to confirm: always in a domain; otherwise unless its
/// password is blank (assumed not when the OS cannot say when it changed).
fn has_sign_in(os: &dyn SystemAuth, blank: &mut Option<BlankCheck>) -> bool {
    if os.domain_joined() {
        return true;
    }
    let Some(changed) = os.password_changed_ms() else {
        return true;
    };
    let is_blank = match *blank {
        Some(b) if b.changed_ms > 0 && changed <= b.changed_ms => b.blank,
        _ => {
            let is_blank = os.password_blank();
            *blank = Some(BlankCheck {
                changed_ms: changed + 1000,
                blank: is_blank,
            });
            is_blank
        }
    };
    !is_blank
}

/// [`SystemAuth`] by the OS, with prompts owned by the browser's window.
pub struct Windows {
    pub owner: WindowHandle,
    /// What the prompts say the browser wants to do.
    pub message: String,
}

impl SystemAuth for Windows {
    fn hello(&self) -> UserVerification {
        window::verify_user(self.owner, &self.message)
    }
    fn domain_joined(&self) -> bool {
        window::domain_joined()
    }
    fn password_changed_ms(&self) -> Option<i64> {
        window::password_changed_ms()
    }
    fn password_blank(&self) -> bool {
        window::password_blank()
    }
    fn sign_in(&self) -> UserVerification {
        window::verify_sign_in(self.owner, "Snowlark", &self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct Fake {
        hello: UserVerification,
        domain: bool,
        changed: Option<i64>,
        blank: bool,
        sign_in: UserVerification,
        asked: RefCell<Vec<&'static str>>,
        blank_checks: Cell<u32>,
    }

    impl Fake {
        fn new(hello: UserVerification) -> Fake {
            Fake {
                hello,
                domain: false,
                changed: Some(5_000),
                blank: false,
                sign_in: UserVerification::Verified,
                asked: RefCell::default(),
                blank_checks: Cell::new(0),
            }
        }
        fn asked(&self) -> Vec<&'static str> {
            self.asked.borrow().clone()
        }
    }

    impl SystemAuth for Fake {
        fn hello(&self) -> UserVerification {
            self.asked.borrow_mut().push("hello");
            self.hello
        }
        fn domain_joined(&self) -> bool {
            self.domain
        }
        fn password_changed_ms(&self) -> Option<i64> {
            self.changed
        }
        fn password_blank(&self) -> bool {
            self.blank_checks.set(self.blank_checks.get() + 1);
            self.blank
        }
        fn sign_in(&self) -> UserVerification {
            self.asked.borrow_mut().push("sign-in");
            self.sign_in
        }
    }

    #[test]
    fn hello_answers_when_it_is_set_up() {
        for answer in [UserVerification::Verified, UserVerification::Refused] {
            let os = Fake::new(answer);
            assert_eq!(confirm_user(&os, &mut None), answer);
            assert_eq!(
                os.asked(),
                ["hello"],
                "no sign-in prompt after Hello answered"
            );
        }
    }

    #[test]
    fn without_hello_the_sign_in_prompt_decides() {
        for answer in [
            UserVerification::Verified,
            UserVerification::Refused,
            UserVerification::Unavailable,
        ] {
            let mut os = Fake::new(UserVerification::Unavailable);
            os.sign_in = answer;
            let mut blank = None;
            assert_eq!(confirm_user(&os, &mut blank), answer);
            assert_eq!(os.asked(), ["hello", "sign-in"]);
            assert_eq!(
                blank,
                Some(BlankCheck {
                    changed_ms: 6_000,
                    blank: false
                })
            );
        }
    }

    #[test]
    fn an_account_without_a_password_passes_without_a_prompt() {
        let mut os = Fake::new(UserVerification::Unavailable);
        os.blank = true;
        os.sign_in = UserVerification::Refused;
        assert_eq!(confirm_user(&os, &mut None), UserVerification::Verified);
        assert_eq!(os.asked(), ["hello"]);
    }

    #[test]
    fn a_domain_account_is_always_asked() {
        let mut os = Fake::new(UserVerification::Unavailable);
        os.domain = true;
        os.blank = true;
        os.sign_in = UserVerification::Refused;
        assert_eq!(confirm_user(&os, &mut None), UserVerification::Refused);
        assert_eq!(os.blank_checks.get(), 0, "no logon tried on a domain");
    }

    #[test]
    fn an_unknown_password_age_is_asked() {
        let mut os = Fake::new(UserVerification::Unavailable);
        os.changed = None;
        os.blank = true;
        os.sign_in = UserVerification::Refused;
        assert_eq!(confirm_user(&os, &mut None), UserVerification::Refused);
        assert_eq!(os.blank_checks.get(), 0);
    }

    #[test]
    fn the_blank_check_is_redone_only_after_the_password_changes() {
        let mut os = Fake::new(UserVerification::Unavailable);
        os.blank = true;
        let mut blank = None;
        confirm_user(&os, &mut blank);
        confirm_user(&os, &mut blank);
        assert_eq!(os.blank_checks.get(), 1, "the remembered answer is used");
        // The user sets a password: the account changed after the check, so it is redone.
        os.changed = Some(60_000);
        os.blank = false;
        os.sign_in = UserVerification::Refused;
        assert_eq!(confirm_user(&os, &mut blank), UserVerification::Refused);
        assert_eq!(os.blank_checks.get(), 2);
        assert_eq!(os.asked().last(), Some(&"sign-in"));
    }
}
