// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The saved passwords (`docs/password-manager.md`, "Storage on Windows"): `passwords.json` in the
//! profile, each password encrypted with AES-256-GCM under a key the OS wraps for the signed-in
//! user, the origin bound in as associated data. Usernames stay readable (the passwords page lists
//! them without reauthentication).

use std::path::{Path, PathBuf};

use base64::Engine as _;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use snowlark_page::passwords::{parse, FormData, Mode, PasswordEvent, PasswordFill};
use snowlark_tab::tab::TabId;

use crate::tab_host::TabHost;

use crate::downloads::now_ms;

/// What the stored key and passwords start with.
const KEY_PREFIX: &[u8] = b"DPAPI";
const PASSWORD_PREFIX: &[u8] = b"v10";

/// Wraps the store's key so that only its owner can unwrap it.
pub trait KeyWrap: Send {
    fn wrap(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn unwrap(&self, wrapped: &[u8]) -> Option<Vec<u8>>;
    /// Why nothing can be wrapped now, if so (for the user).
    fn unavailable(&self) -> Option<&'static str> {
        None
    }
}

/// The Windows Data Protection API at user scope.
pub struct Dpapi;

impl KeyWrap for Dpapi {
    fn wrap(&self, key: &[u8]) -> Option<Vec<u8>> {
        snowlark_paint::os_crypt::protect(key)
    }
    fn unwrap(&self, wrapped: &[u8]) -> Option<Vec<u8>> {
        snowlark_paint::os_crypt::unprotect(wrapped)
    }
    fn unavailable(&self) -> Option<&'static str> {
        use snowlark_paint::os_crypt::{available, Unavailable};
        match available() {
            Ok(()) => None,
            Err(Unavailable::NoService) => {
                Some("No keyring (Secret Service) is running on this desktop to keep it safe.")
            }
            Err(Unavailable::Refused) => Some("The desktop's keyring was not unlocked."),
        }
    }
}

/// One saved login.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Login {
    pub id: u64,
    /// The serialized origin of the page the login was saved on (`https://example.com`).
    pub origin: String,
    pub username: String,
    /// The username and password fields' names on that page.
    #[serde(default)]
    pub username_element: String,
    #[serde(default)]
    pub password_element: String,
    /// `v10`, the nonce and the AES-256-GCM ciphertext with its tag, base64.
    password: String,
    pub created_ms: i64,
    pub last_used_ms: i64,
    #[serde(default)]
    pub times_used: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct Data {
    /// `DPAPI` and the wrapped key, base64; empty until the first save.
    #[serde(default)]
    key: String,
    #[serde(default)]
    logins: Vec<Login>,
    /// Origins the user chose "Never" for.
    #[serde(default)]
    never: Vec<String>,
}

/// What a successful submission of `username` and a password on an origin should offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    /// The user chose Never for the origin.
    Blocked,
    /// The same login is saved already (its last use is updated).
    Saved,
    /// The username is saved with another password.
    Update(u64),
    /// A new login.
    Save,
}

/// The saved passwords of a profile.
pub struct PasswordStore {
    file: Option<PathBuf>,
    data: Data,
    wrap: Box<dyn KeyWrap>,
    /// The unwrapped key, once used.
    key: Option<LessSafeKey>,
    /// The stored key could not be unwrapped (another user's or machine's file).
    key_lost: bool,
    rng: SystemRandom,
    generation: u64,
    shown: u64,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

impl PasswordStore {
    /// The store in `file` (empty when it is missing or unreadable).
    pub fn load(file: &Path, wrap: Box<dyn KeyWrap>) -> PasswordStore {
        let data = std::fs::read_to_string(file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        PasswordStore::with(Some(file.to_owned()), data, wrap)
    }

    /// A store that is never written (a private window's profile).
    pub fn in_memory(wrap: Box<dyn KeyWrap>) -> PasswordStore {
        PasswordStore::with(None, Data::default(), wrap)
    }

    fn with(file: Option<PathBuf>, data: Data, wrap: Box<dyn KeyWrap>) -> PasswordStore {
        PasswordStore {
            file,
            data,
            wrap,
            key: None,
            key_lost: false,
            rng: SystemRandom::new(),
            generation: 0,
            shown: 0,
        }
    }

    fn changed(&mut self) {
        self.generation += 1;
    }

    /// Whether the list changed since the last call (the passwords page is then shown again).
    pub fn take_change(&mut self) -> bool {
        let changed = self.shown != self.generation;
        self.shown = self.generation;
        changed
    }

    /// The key, made and wrapped on first use. `None` when the stored one cannot be unwrapped.
    fn key(&mut self) -> Option<&LessSafeKey> {
        if self.key.is_none() && !self.key_lost {
            let raw: Option<Vec<u8>> = if self.data.key.is_empty() {
                let mut k = vec![0u8; 32];
                self.rng.fill(&mut k).ok()?;
                let wrapped = self.wrap.wrap(&k)?;
                let mut stored = KEY_PREFIX.to_vec();
                stored.extend_from_slice(&wrapped);
                self.data.key = b64().encode(stored);
                self.changed();
                Some(k)
            } else {
                b64()
                    .decode(&self.data.key)
                    .ok()
                    .and_then(|s| s.strip_prefix(KEY_PREFIX).map(<[u8]>::to_vec))
                    .and_then(|w| self.wrap.unwrap(&w))
                    .filter(|k| k.len() == 32)
            };
            match raw.and_then(|k| UnboundKey::new(&AES_256_GCM, &k).ok()) {
                Some(k) => self.key = Some(LessSafeKey::new(k)),
                None => self.key_lost = !self.data.key.is_empty(),
            }
        }
        self.key.as_ref()
    }

    /// Why a login cannot be saved now (no key can be made or read), for the user; `None` when it
    /// can.
    pub fn cannot_save(&mut self) -> Option<&'static str> {
        if self.data.key.is_empty() {
            // (no key yet: it is made at the first save, if it can be)
            return self.wrap.unavailable();
        }
        if self.key().is_some() {
            return None;
        }
        Some(
            self.wrap
                .unavailable()
                .unwrap_or("The key the passwords are encrypted with is not available."),
        )
    }

    /// Whether the stored passwords cannot be decrypted by this user.
    pub fn unreadable(&mut self) -> bool {
        !self.data.logins.is_empty() && self.key().is_none()
    }

    fn encrypt(&mut self, origin: &str, password: &str) -> Option<String> {
        let mut nonce = [0u8; NONCE_LEN];
        self.rng.fill(&mut nonce).ok()?;
        let key = self.key()?;
        let mut buf = password.as_bytes().to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(origin.as_bytes()),
            &mut buf,
        )
        .ok()?;
        let mut out = PASSWORD_PREFIX.to_vec();
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&buf);
        Some(b64().encode(out))
    }

    fn decrypt(&mut self, origin: &str, stored: &str) -> Option<String> {
        let bytes = b64().decode(stored).ok()?;
        let rest = bytes.strip_prefix(PASSWORD_PREFIX)?;
        if rest.len() < NONCE_LEN {
            return None;
        }
        let (nonce, sealed) = rest.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let mut buf = sealed.to_vec();
        let key = self.key()?;
        let plain = key
            .open_in_place(nonce, Aad::from(origin.as_bytes()), &mut buf)
            .ok()?;
        String::from_utf8(plain.to_vec()).ok()
    }

    /// The logins saved for exactly `origin`, with their passwords (those that decrypt).
    pub fn logins_for(&mut self, origin: &str) -> Vec<(Login, String)> {
        let found: Vec<Login> = self
            .data
            .logins
            .iter()
            .filter(|l| l.origin == origin)
            .cloned()
            .collect();
        found
            .into_iter()
            .filter_map(|l| {
                let p = self.decrypt(&l.origin, &l.password)?;
                Some((l, p))
            })
            .collect()
    }

    /// What a successful submission of `username` / `password` on `origin` offers.
    pub fn offer(&mut self, origin: &str, username: &str, password: &str) -> Offer {
        if self.is_never(origin) {
            return Offer::Blocked;
        }
        for (l, p) in self.logins_for(origin) {
            if l.username == username {
                return if p == password {
                    Offer::Saved
                } else {
                    Offer::Update(l.id)
                };
            }
        }
        Offer::Save
    }

    /// Saves a login, or replaces the password of the one with the same origin and username.
    /// Returns its id; `None` when the key is not available.
    pub fn save_login(
        &mut self,
        origin: &str,
        username: &str,
        username_element: &str,
        password_element: &str,
        password: &str,
    ) -> Option<u64> {
        let sealed = self.encrypt(origin, password)?;
        let now = now_ms();
        let existing = self
            .data
            .logins
            .iter_mut()
            .find(|l| l.origin == origin && l.username == username);
        let id = match existing {
            Some(l) => {
                l.password = sealed;
                l.last_used_ms = now;
                if !password_element.is_empty() {
                    l.password_element = password_element.to_owned();
                }
                l.id
            }
            None => {
                let id = self.data.logins.iter().map(|l| l.id).max().unwrap_or(0) + 1;
                self.data.logins.push(Login {
                    id,
                    origin: origin.to_owned(),
                    username: username.to_owned(),
                    username_element: username_element.to_owned(),
                    password_element: password_element.to_owned(),
                    password: sealed,
                    created_ms: now,
                    last_used_ms: now,
                    times_used: 0,
                });
                id
            }
        };
        self.data.never.retain(|o| o != origin);
        self.changed();
        Some(id)
    }

    /// Notes that a login was used (submitted again, or filled).
    pub fn mark_used(&mut self, id: u64) {
        if let Some(l) = self.data.logins.iter_mut().find(|l| l.id == id) {
            l.last_used_ms = now_ms();
            l.times_used = l.times_used.saturating_add(1);
            self.changed();
        }
    }

    pub fn is_never(&self, origin: &str) -> bool {
        self.data.never.iter().any(|o| o == origin)
    }

    /// The user chose "Never" for `origin`.
    pub fn add_never(&mut self, origin: &str) {
        if !self.is_never(origin) {
            self.data.never.push(origin.to_owned());
            self.changed();
        }
    }

    pub fn remove_never(&mut self, origin: &str) {
        let before = self.data.never.len();
        self.data.never.retain(|o| o != origin);
        if self.data.never.len() != before {
            self.changed();
        }
    }

    /// The origins on the Never list.
    pub fn never(&self) -> &[String] {
        &self.data.never
    }

    /// Every saved login, without passwords, sorted by origin then username.
    pub fn list(&self) -> Vec<Login> {
        let mut v: Vec<Login> = self
            .data
            .logins
            .iter()
            .map(|l| Login {
                password: String::new(),
                ..l.clone()
            })
            .collect();
        v.sort_by(|a, b| (&a.origin, &a.username).cmp(&(&b.origin, &b.username)));
        v
    }

    /// One login's password (the page reveals it after the OS confirmed the user).
    pub fn reveal(&mut self, id: u64) -> Option<String> {
        let l = self.data.logins.iter().find(|l| l.id == id)?.clone();
        self.decrypt(&l.origin, &l.password)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.data.logins.len();
        self.data.logins.retain(|l| l.id != id);
        let removed = self.data.logins.len() != before;
        if removed {
            self.changed();
        }
        removed
    }

    /// Delete browsing data: the logins saved in `[begin, end)` (ms since the epoch).
    pub fn delete_between(&mut self, begin: i64, end: i64) {
        let before = self.data.logins.len();
        self.data
            .logins
            .retain(|l| !(l.created_ms >= begin && l.created_ms < end));
        if self.data.logins.len() != before {
            self.changed();
        }
    }

    pub fn len(&self) -> usize {
        self.data.logins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.logins.is_empty()
    }

    /// Writes the file (a temporary file renamed over it) when something changed.
    pub fn save(&mut self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let json = serde_json::to_string(&self.data).map_err(std::io::Error::other)?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = file.with_extension("json.tmp");
        std::fs::write(&temp, json)?;
        std::fs::rename(&temp, file)
    }
}

/// The origin whose saved logins a tab's top-level document may see: the browser's own record of
/// what the tab committed (`url`), when its renderer is locked to that URL's site (`lock`: the
/// tab's site lock, `None` when it has none yet). Only `http:` and `https:` documents have logins.
pub fn page_origin(url: &snowlark_net::Url, lock: Option<&str>) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if let Some(lock) = lock {
        if crate::tab_host::site_for(url).as_deref() != Some(lock) {
            return None;
        }
    }
    Some(url.origin().ascii_serialization())
}

/// Whether logins fill on load on `origin`: HTTPS, or a local host.
pub fn fills_on_load(origin: &str) -> bool {
    let Ok(u) = snowlark_net::Url::parse(origin) else {
        return false;
    };
    u.scheme() == "https"
        || matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        || u.host_str().is_some_and(|h| h.ends_with(".localhost"))
}

/// A submitted login waiting to be judged by the next page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingLogin {
    pub origin: String,
    pub username: String,
    pub username_element: String,
    pub password: String,
    pub password_element: String,
    /// The form's shape, to recognise it on the next page (a failed login).
    pub signature: Vec<(bool, String)>,
}

/// The login a form submitted on `origin` establishes, if it is one.
pub fn pending_login(origin: &str, form: &FormData) -> Option<PendingLogin> {
    let parsed = parse(form, Mode::Saving)?;
    let pw = parsed.password_to_save()?;
    let user = parsed.username.map(|u| &form.fields[u]);
    Some(PendingLogin {
        origin: origin.to_owned(),
        username: user.map(|u| u.value.clone()).unwrap_or_default(),
        username_element: user
            .map(|u| u.element_name().to_owned())
            .unwrap_or_default(),
        password: form.fields[pw].value.clone(),
        password_element: form.fields[pw].element_name().to_owned(),
        signature: form.signature(),
    })
}

/// Whether the page after a submission shows the submitted form again: the login failed.
pub fn login_failed(pending: &PendingLogin, rendered: &[FormData]) -> bool {
    rendered.iter().any(|f| f.signature() == pending.signature)
}

/// The logins submitted in the window's tabs, each waiting for the page after it: with the tab
/// and the count of its documents ([`TabHost::documents`]) when the form was submitted.
#[derive(Debug, Default)]
pub struct PendingLogins(Vec<(TabId, u64, PendingLogin)>);

impl PendingLogins {
    /// What tab `tab`'s page reported: a successful login, when this is the page that judges one.
    ///
    /// A submission waits for the first load report of a document after the one that submitted
    /// it, however that document came (a navigation the browser fetched and committed, in this
    /// renderer or another, or one the page made by itself). The submitting document's own report
    /// (its `load` came late) is not one.
    pub fn hear(&mut self, tab: &TabHost, e: &PasswordEvent) -> Option<PendingLogin> {
        match e {
            PasswordEvent::Submitted(form) => {
                let origin = page_origin(tab.current_url()?, tab.site())?;
                if let Some(p) = pending_login(&origin, form) {
                    self.0.retain(|(t, ..)| *t != tab.id);
                    self.0.push((tab.id, tab.documents(), p));
                }
                None
            }
            // (a sign-in by script without a navigation: the form left, the login succeeded)
            PasswordEvent::SubmittedInPlace(form) => {
                let origin = page_origin(tab.current_url()?, tab.site())?;
                let p = pending_login(&origin, form)?;
                self.0.retain(|(t, ..)| *t != tab.id);
                Some(p)
            }
            // (forms added after `load`: the document was judged by its `load` report)
            PasswordEvent::Added(_) => None,
            PasswordEvent::Rendered(forms) => {
                let k = self
                    .0
                    .iter()
                    .position(|(t, at, _)| *t == tab.id && tab.documents() > *at)?;
                let (.., p) = self.0.remove(k);
                // (an HTTP error answered the login, or its form is back: it failed)
                (!tab.http_error() && !login_failed(&p, forms)).then_some(p)
            }
        }
    }

    /// The tab's renderer was replaced, the tab numbered `to` now: its submission goes on
    /// waiting (a cross-site redirect after a login, say).
    pub fn moved(&mut self, from: TabId, to: TabId) {
        for (t, ..) in &mut self.0 {
            if *t == from {
                *t = to;
            }
        }
    }

    /// Tab `tab` closed.
    pub fn closed(&mut self, tab: TabId) {
        self.0.retain(|(t, ..)| *t != tab);
    }

    /// Whether no login waits.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What fills on load into the forms a page of `origin` rendered: the one saved login for the
/// origin, into the first form with a current-password field whose password field is empty.
pub fn choose_fill(
    store: &mut PasswordStore,
    origin: &str,
    forms: &[FormData],
) -> Option<(u64, PasswordFill)> {
    if !fills_on_load(origin) {
        return None;
    }
    let logins = store.logins_for(origin);
    let [(login, password)] = logins.as_slice() else {
        return None;
    };
    forms.iter().find_map(|form| {
        let parsed = parse(form, Mode::Filling)?;
        let pw = parsed.password?;
        if !form.fields[pw].value.is_empty() {
            return None;
        }
        Some((
            login.id,
            PasswordFill {
                origin: origin.to_owned(),
                username_field: parsed.username.map(|u| form.fields[u].id),
                password_field: form.fields[pw].id,
                username: login.username.clone(),
                password: password.clone(),
            },
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the OS: XOR with a byte, refusing anything wrapped with another byte.
    struct Fake(u8);
    impl KeyWrap for Fake {
        fn wrap(&self, key: &[u8]) -> Option<Vec<u8>> {
            let mut v = vec![self.0];
            v.extend(key.iter().map(|b| b ^ self.0));
            Some(v)
        }
        fn unwrap(&self, w: &[u8]) -> Option<Vec<u8>> {
            (w.first() == Some(&self.0)).then(|| w[1..].iter().map(|b| b ^ self.0).collect())
        }
    }

    /// An OS with nowhere to keep a key (Linux without a Secret Service).
    struct NoKeyring;
    impl KeyWrap for NoKeyring {
        fn wrap(&self, _: &[u8]) -> Option<Vec<u8>> {
            None
        }
        fn unwrap(&self, _: &[u8]) -> Option<Vec<u8>> {
            None
        }
        fn unavailable(&self) -> Option<&'static str> {
            Some("No keyring.")
        }
    }

    #[test]
    fn a_store_with_nowhere_to_keep_its_key_says_why_and_saves_nothing() {
        let mut s = PasswordStore::in_memory(Box::new(NoKeyring));
        assert_eq!(s.cannot_save(), Some("No keyring."));
        assert_eq!(s.save_login(SITE, "alice", "", "", "secret"), None);
        assert!(s.list().is_empty());
        // (where the key can be kept, nothing is said)
        let mut ok = PasswordStore::in_memory(Box::new(Fake(5)));
        assert_eq!(ok.cannot_save(), None);
        ok.save_login(SITE, "alice", "", "", "secret").unwrap();
        assert_eq!(ok.cannot_save(), None);
    }

    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("snowlark-passwords-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("passwords.json")
    }

    const SITE: &str = "https://login.example";

    #[test]
    fn round_trip_through_the_file_with_passwords_encrypted() {
        let file = temp_file("round");
        let mut s = PasswordStore::load(&file, Box::new(Fake(7)));
        let id = s
            .save_login(SITE, "alice", "user", "pw", "correct horse")
            .unwrap();
        s.save().unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("alice"), "usernames are stored readable");
        assert!(
            !text.contains("correct horse"),
            "the password is not in clear"
        );
        let mut t = PasswordStore::load(&file, Box::new(Fake(7)));
        let got = t.logins_for(SITE);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.username, "alice");
        assert_eq!(got[0].0.password_element, "pw");
        assert_eq!(got[0].1, "correct horse");
        assert_eq!(t.reveal(id).as_deref(), Some("correct horse"));
        assert!(t.logins_for("https://other.example").is_empty());
        assert!(t.logins_for("http://login.example").is_empty());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_password_moved_to_another_origin_does_not_decrypt() {
        let file = temp_file("moved");
        let mut s = PasswordStore::load(&file, Box::new(Fake(7)));
        s.save_login(SITE, "alice", "", "", "secret").unwrap();
        s.save().unwrap();
        let text = std::fs::read_to_string(&file)
            .unwrap()
            .replace(SITE, "https://evil.example");
        std::fs::write(&file, text).unwrap();
        let mut t = PasswordStore::load(&file, Box::new(Fake(7)));
        assert!(t.logins_for("https://evil.example").is_empty());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn another_users_key_reads_nothing() {
        let file = temp_file("user");
        let mut s = PasswordStore::load(&file, Box::new(Fake(7)));
        s.save_login(SITE, "alice", "", "", "secret").unwrap();
        s.save().unwrap();
        let mut t = PasswordStore::load(&file, Box::new(Fake(9)));
        assert!(t.unreadable());
        assert!(t.logins_for(SITE).is_empty());
        assert_eq!(t.list().len(), 1, "the entries stay");
        assert!(t.save_login(SITE, "bob", "", "", "x").is_none());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn offers_save_update_or_nothing() {
        let mut s = PasswordStore::in_memory(Box::new(Fake(1)));
        assert_eq!(s.offer(SITE, "alice", "one"), Offer::Save);
        let id = s.save_login(SITE, "alice", "u", "p", "one").unwrap();
        assert_eq!(s.offer(SITE, "alice", "one"), Offer::Saved);
        assert_eq!(s.offer(SITE, "alice", "two"), Offer::Update(id));
        assert_eq!(s.offer(SITE, "bob", "one"), Offer::Save);
        assert_eq!(s.save_login(SITE, "alice", "u", "p", "two"), Some(id));
        assert_eq!(s.len(), 1);
        assert_eq!(s.logins_for(SITE)[0].1, "two");
        s.add_never("https://never.example");
        assert_eq!(
            s.offer("https://never.example", "alice", "one"),
            Offer::Blocked
        );
        s.save_login("https://never.example", "alice", "", "", "x");
        assert!(!s.is_never("https://never.example"), "saving lifts Never");
    }

    #[test]
    fn delete_between_and_remove() {
        let mut s = PasswordStore::in_memory(Box::new(Fake(1)));
        let a = s.save_login(SITE, "a", "", "", "1").unwrap();
        let b = s.save_login(SITE, "b", "", "", "2").unwrap();
        s.data.logins[0].created_ms = 1_000;
        s.delete_between(5_000, i64::MAX);
        assert_eq!(s.list().iter().map(|l| l.id).collect::<Vec<_>>(), vec![a]);
        assert!(s.remove(a));
        assert!(!s.remove(b));
        assert!(s.is_empty());
    }

    #[test]
    fn list_carries_no_passwords() {
        let mut s = PasswordStore::in_memory(Box::new(Fake(1)));
        s.save_login(SITE, "alice", "", "", "secret").unwrap();
        let json = serde_json::to_string(&s.list()).unwrap();
        assert!(json.contains("\"password\":\"\""), "{json}");
    }

    fn login_form(user: &str, pass: &str) -> FormData {
        let field = |id, kind: &str, name: &str, value: &str| snowlark_page::passwords::FieldData {
            id,
            kind: kind.into(),
            name: name.into(),
            value: value.into(),
            focusable: true,
            user_typed: !value.is_empty(),
            ..Default::default()
        };
        FormData {
            form: 1,
            action: String::new(),
            fields: vec![
                field(11, "text", "user", user),
                field(12, "password", "pass", pass),
            ],
        }
    }

    #[test]
    fn the_origin_is_the_browsers_record_and_only_under_its_site_lock() {
        let url = snowlark_net::Url::parse("https://www.login.example/signin?x=1").unwrap();
        let site = crate::tab_host::site_for(&url);
        assert_eq!(
            page_origin(&url, site.as_deref()).as_deref(),
            Some("https://www.login.example")
        );
        assert_eq!(
            page_origin(&url, None).as_deref(),
            Some("https://www.login.example")
        );
        // A renderer locked to another site gets no origin, so no logins.
        assert_eq!(page_origin(&url, Some("https://evil.example")), None);
        let data = snowlark_net::Url::parse("data:text/html,x").unwrap();
        assert_eq!(page_origin(&data, None), None);
    }

    #[test]
    fn a_fill_is_only_ever_for_the_pages_own_origin() {
        let mut s = PasswordStore::in_memory(Box::new(Fake(1)));
        s.save_login(SITE, "alice", "user", "pass", "secret")
            .unwrap();
        let forms = [login_form("", "")];
        let (_, fill) = choose_fill(&mut s, SITE, &forms).unwrap();
        assert_eq!(fill.origin, SITE);
        assert_eq!((fill.username_field, fill.password_field), (Some(11), 12));
        assert_eq!(
            (fill.username.as_str(), fill.password.as_str()),
            ("alice", "secret")
        );
        // Another origin's page, even one naming the same fields, gets nothing.
        assert!(choose_fill(&mut s, "https://evil.example", &forms).is_none());
        assert!(choose_fill(&mut s, "https://sub.login.example", &forms).is_none());
        // Not over plain HTTP, except on a local host.
        s.save_login("http://login.example", "alice", "", "", "x")
            .unwrap();
        assert!(choose_fill(&mut s, "http://login.example", &forms).is_none());
        s.save_login("http://localhost:8080", "alice", "", "", "x")
            .unwrap();
        assert!(choose_fill(&mut s, "http://localhost:8080", &forms).is_some());
        // Not over a password the page filled in itself, and not when several logins match.
        assert!(choose_fill(&mut s, SITE, &[login_form("", "page's")]).is_none());
        s.save_login(SITE, "bob", "", "", "other").unwrap();
        assert!(choose_fill(&mut s, SITE, &forms).is_none());
    }

    #[test]
    fn a_login_fails_when_its_form_comes_back() {
        let p = pending_login(SITE, &login_form("alice", "pw1")).unwrap();
        assert_eq!((p.username.as_str(), p.password.as_str()), ("alice", "pw1"));
        assert_eq!(
            (p.username_element.as_str(), p.password_element.as_str()),
            ("user", "pass")
        );
        assert!(login_failed(&p, &[login_form("", "")]));
        assert!(!login_failed(&p, &[]));
        assert!(pending_login(SITE, &login_form("alice", "")).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_data_protection_api_round_trips() {
        let wrapped = Dpapi.wrap(b"0123456789abcdef0123456789abcdef").unwrap();
        assert_ne!(&wrapped[..], b"0123456789abcdef0123456789abcdef");
        assert_eq!(
            Dpapi.unwrap(&wrapped).unwrap(),
            b"0123456789abcdef0123456789abcdef"
        );
        let mut damaged = wrapped.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 0xff;
        assert!(Dpapi.unwrap(&damaged).is_none());
    }
}
