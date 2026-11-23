// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Login forms for the browser's password manager (`docs/password-manager.md`): the field facts a
//! page reports ([`FormData`]) and the parser that finds the username and password fields in them
//! ([`parse`]). The page only extracts; the parser runs in the browser process, on the facts the
//! page sent, so that a compromised renderer decides nothing.

use regex::Regex;
use serde::{Deserialize, Serialize};

/// One text-like `input` of a form, as the page saw it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldData {
    /// The element's `NodeId::to_bits`: how the browser names it back in a fill.
    pub id: u64,
    /// The `type` state (`text`, `password`, `email`, ...).
    pub kind: String,
    pub name: String,
    /// The `id` attribute.
    pub id_attr: String,
    pub autocomplete: String,
    /// The value; empty in forms reported for filling.
    pub value: String,
    /// The user edited it (never set by script).
    pub user_typed: bool,
    /// It is being rendered and is not disabled: the user could reach it.
    pub focusable: bool,
    pub readonly: bool,
}

impl FieldData {
    fn is_password(&self) -> bool {
        self.kind == "password"
    }

    /// The name the store keeps for the field: `name`, else `id`.
    pub fn element_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id_attr
        } else {
            &self.name
        }
    }
}

/// A form (or a document's fields outside any form) as the page reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormData {
    /// The `form` element's `NodeId::to_bits`; 0 for fields outside any form.
    pub form: u64,
    /// The submission's action URL as the page resolved it (informational: the browser decides the
    /// origin from its own record of the tab).
    pub action: String,
    /// The text-like inputs in tree order.
    pub fields: Vec<FieldData>,
}

impl FormData {
    /// Whether the form has a password field.
    pub fn has_password_field(&self) -> bool {
        self.fields.iter().any(FieldData::is_password)
    }

    /// The form's shape, to recognise it again on the next page: each field's type and name.
    pub fn signature(&self) -> Vec<(bool, String)> {
        self.fields
            .iter()
            .map(|f| (f.is_password(), f.element_name().to_owned()))
            .collect()
    }
}

/// What the page tells the browser about its top-level document's login forms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PasswordEvent {
    /// A form passed its `submit` event and is navigating: its fields with their values.
    Submitted(FormData),
    /// The document finished loading: its forms that have a password field, without values (an
    /// empty list is news too: the page after a login has none).
    Rendered(Vec<FormData>),
}

/// A saved login the browser fills into a document of `origin`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordFill {
    /// The serialized origin the browser decided for; the page fills only a top-level document
    /// of that origin.
    pub origin: String,
    pub username_field: Option<u64>,
    pub password_field: u64,
    pub username: String,
    pub password: String,
}

/// What the form is parsed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A submitted form with values: empty fields do not count.
    Saving,
    /// A rendered form without values, to fill.
    Filling,
}

/// The fields of a login, sign-up or change-password form: indices into [`FormData::fields`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParsedForm {
    pub username: Option<usize>,
    pub password: Option<usize>,
    pub new_password: Option<usize>,
    pub confirmation: Option<usize>,
}

impl ParsedForm {
    /// The password a successful submission establishes: the new one when there is one.
    pub fn password_to_save(&self) -> Option<usize> {
        self.new_password.or(self.password)
    }
}

/// How sure it is that the user could use a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Interactability {
    Unlikely,
    Possible,
    Certain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flag {
    None,
    Username,
    CurrentPassword,
    NewPassword,
    OneTimeCode,
    CreditCard,
}

struct Processed {
    index: usize,
    flag: Flag,
    password: bool,
    interactability: Interactability,
    not_password: bool,
}

/// The patterns that mark a field as something other than a credential, by its `name` or `id`.
struct Matchers {
    card_code: Regex,
    ssn: Regex,
    otp: Regex,
}

impl Matchers {
    fn new() -> Matchers {
        let re = |s: &str| Regex::new(s).expect("password form pattern");
        Matchers {
            card_code: re(
                r"(?i)verification|card.?identification|security.?code|card.?code|security.?value|security.?number|card.?pin|c-v-v|(?:cvn|cvv|cvc|csc|cvd|ccv)(?:field)?|\bcid\b",
            ),
            ssn: re(r"(?i)ssn|social.?security.?(?:num(?:ber)?|#)*"),
            otp: re(concat!(
                r"(?i)one.?time|",
                r"(?:\b|_)(?:otp|otc|totp|sms|2fa|mfa)(?:\b|_)|",
                r"(?:otp|otc|totp|sms|2fa|mfa).?(?:code|token|input|val|pin|login|verif|pass|pwd|psw|auth|field)|",
                r"(?:verif(?:y|ication)?|email|phone|text|login|input|txt|user).?(?:otp|otc|totp|sms|2fa|mfa)|",
                r"sms.?otp|mfa.?otp|verif(?:y|ication)?.?code|(?:\b|_)vcode|(?:second|two|2).?factor|",
                r"wfls-token|email_code"
            )),
        }
    }

    fn not_password(&self, f: &FieldData, flag: Flag) -> bool {
        if matches!(flag, Flag::CreditCard | Flag::OneTimeCode) {
            return true;
        }
        [&f.name, &f.id_attr]
            .iter()
            .any(|s| self.card_code.is_match(s) || self.ssn.is_match(s) || self.otp.is_match(s))
    }
}

/// A value that is one non-word character repeated: a page's mask, not a value.
fn hidden_value(v: &str) -> bool {
    let mut chars = v.chars();
    let Some(c) = chars.next() else {
        return false;
    };
    let rest: Vec<char> = chars.collect();
    !(c.is_alphanumeric() || c == '_') && !rest.is_empty() && rest.iter().all(|&d| d == c)
}

/// The input types a username or password can be typed in.
fn is_text_like(kind: &str) -> bool {
    matches!(
        kind,
        "text" | "password" | "email" | "tel" | "url" | "search" | "number" | ""
    )
}

/// The `autocomplete` attribute's field name for a password form: its last token, a trailing
/// `webauthn` set aside.
fn autocomplete_flag(attr: &str) -> Flag {
    let mut tokens: Vec<&str> = attr.split_ascii_whitespace().collect();
    if tokens
        .last()
        .is_some_and(|t| t.eq_ignore_ascii_case("webauthn"))
    {
        tokens.pop();
    }
    let Some(last) = tokens.last() else {
        return Flag::None;
    };
    let last = last.to_ascii_lowercase();
    match last.as_str() {
        "username" => Flag::Username,
        "current-password" => Flag::CurrentPassword,
        "new-password" => Flag::NewPassword,
        "one-time-code" => Flag::OneTimeCode,
        s if s.starts_with("cc-") => Flag::CreditCard,
        _ => Flag::None,
    }
}

#[derive(Default)]
struct Found {
    username: Option<usize>,
    password: Option<usize>,
    new_password: Option<usize>,
    confirmation: Option<usize>,
}

impl Found {
    fn has_passwords(&self) -> bool {
        self.password.is_some() || self.new_password.is_some()
    }

    fn conflict(&self) -> bool {
        let all = [
            self.username,
            self.password,
            self.new_password,
            self.confirmation,
        ];
        let set: Vec<usize> = all.iter().flatten().copied().collect();
        (1..set.len()).any(|i| set[..i].contains(&set[i]))
    }
}

/// Finds the username and password fields of `form` (`docs/password-manager.md`, "Parsing"):
/// `autocomplete` first, then the fields' order. `None` when the form has no usable password field.
pub fn parse(form: &FormData, mode: Mode) -> Option<ParsedForm> {
    let m = Matchers::new();
    let saving = mode == Mode::Saving;
    let fields = &form.fields;
    // 1. Field facts.
    let processed: Vec<Processed> = fields
        .iter()
        .enumerate()
        .filter(|(_, f)| is_text_like(&f.kind))
        .filter(|(_, f)| !saving || !(f.value.is_empty() || hidden_value(&f.value)))
        .map(|(index, f)| {
            let flag = autocomplete_flag(&f.autocomplete);
            Processed {
                index,
                flag,
                password: f.is_password(),
                interactability: if f.user_typed {
                    Interactability::Certain
                } else if f.focusable {
                    Interactability::Possible
                } else {
                    Interactability::Unlikely
                },
                not_password: m.not_password(f, flag),
            }
        })
        .collect();
    if !processed.iter().any(|p| p.password) {
        return None;
    }
    let value = |i: usize| fields[i].value.as_str();

    // 2. `autocomplete`.
    let mut found = Found::default();
    {
        let mut ignore_new = false;
        let mut new_by_autocomplete = false;
        let mut usernames = Vec::new();
        for p in &processed {
            match p.flag {
                Flag::Username if !p.password => usernames.push(p.index),
                Flag::CurrentPassword if p.password && found.password.is_none() => {
                    found.password = Some(p.index)
                }
                Flag::NewPassword if p.password && !ignore_new => {
                    if found.new_password.is_none() {
                        found.new_password = Some(p.index);
                        new_by_autocomplete = true;
                    } else if found.confirmation.is_none() {
                        if saving
                            && new_by_autocomplete
                            && found.new_password.map(value) != Some(value(p.index))
                        {
                            ignore_new = true;
                            found.new_password = None;
                            continue;
                        }
                        found.confirmation = Some(p.index);
                    }
                }
                _ => {}
            }
        }
        if usernames.len() == 1 {
            found.username = Some(usernames[0]);
        }
        if found.conflict() {
            found = Found::default();
        }
    }

    // 3. Structure, for what `autocomplete` left.
    if !(found.has_passwords() && found.username.is_some()) {
        let first_password = if !found.has_passwords() {
            let best = processed
                .iter()
                .filter(|p| p.password && !p.not_password)
                .map(|p| p.interactability)
                .max()
                .unwrap_or(Interactability::Unlikely);
            let candidates: Vec<&Processed> = processed
                .iter()
                .filter(|p| p.password && p.interactability >= best)
                .filter(|p| !saving || !value(p.index).is_empty())
                .collect();
            let likely: Vec<&Processed> = candidates
                .iter()
                .copied()
                .filter(|p| {
                    let f = &fields[p.index];
                    !(f.readonly && !f.user_typed) && !p.not_password
                })
                .filter(|p| Some(p.index) != found.username)
                .collect();
            // (only likely passwords: a form whose password fields all look like something else,
            // a card's security code or a one-time code, is neither saved nor filled)
            let passwords: Vec<usize> = likely.iter().map(|p| p.index).collect();
            if passwords.is_empty() {
                return None;
            }
            locate_passwords(&mut found, &passwords, value);
            passwords[0]
        } else {
            [found.password, found.new_password, found.confirmation]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(0)
        };
        if found.username.is_none() {
            let before: Vec<&Processed> = processed
                .iter()
                .filter(|p| p.index < first_password && !p.password)
                .collect();
            let best = before
                .iter()
                .filter(|p| !p.not_password)
                .map(|p| p.interactability)
                .max()
                .unwrap_or(Interactability::Unlikely);
            let mut closest = None;
            let mut closest_focusable = None;
            for p in before.iter().rev() {
                if p.interactability < best || p.not_password {
                    continue;
                }
                if saving && probably_not_username(value(p.index)) {
                    continue;
                }
                closest.get_or_insert(p.index);
                if fields[p.index].focusable {
                    closest_focusable.get_or_insert(p.index);
                }
            }
            found.username = closest_focusable.or(closest);
        }
    }
    if !found.has_passwords() {
        return None;
    }
    Some(ParsedForm {
        username: found.username,
        password: found.password,
        new_password: found.new_password,
        confirmation: found.confirmation,
    })
}

/// Empty, or one or two digits: not a username.
fn probably_not_username(s: &str) -> bool {
    s.is_empty() || (s.chars().count() < 3 && s.chars().all(|c| c.is_ascii_digit()))
}

/// Which of the password fields (in order) is the current password, the new one and its
/// confirmation.
fn locate_passwords<'a>(found: &mut Found, p: &[usize], value: impl Fn(usize) -> &'a str) {
    match p.len() {
        1 => found.password = Some(p[0]),
        2 => {
            if !value(p[0]).is_empty() && value(p[0]) == value(p[1]) {
                // A new password and its confirmation (sign-up, or a change without the old one).
                found.new_password = Some(p[0]);
                found.confirmation = Some(p[1]);
            } else {
                found.password = Some(p[0]);
                found.new_password = Some(p[1]);
            }
        }
        _ => {
            let (a, b, c) = (value(p[0]), value(p[1]), value(p[2]));
            if !a.is_empty() && a == b && a == c {
                found.password = Some(p[0]);
            } else if b == c {
                found.password = Some(p[0]);
                found.new_password = Some(p[1]);
                found.confirmation = Some(p[2]);
            } else if a == b {
                found.new_password = Some(p[0]);
                found.confirmation = Some(p[1]);
            } else {
                found.password = Some(p[0]);
            }
        }
    }
}
