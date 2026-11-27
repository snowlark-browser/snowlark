// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Data only the signed-in user can read back, on Linux: the counterpart of Windows' Data
//! Protection API is a secret the desktop's Secret Service (GNOME Keyring, KWallet, KeePassXC:
//! `org.freedesktop.secrets` on the session bus) keeps for the user. The browser keeps one secret
//! there, an item of the default collection with the attributes `application: snowlark` and
//! `purpose: safe-storage` (made with 32 random bytes the first time), and seals data with
//! AES-256-GCM under the SHA-256 of it. Where no Secret Service answers (or the user refuses to
//! unlock it), nothing can be protected: [`available`] says why, and the password store keeps no
//! key (it saves no passwords) rather than writing one any program of the user could read.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::Connection;
use dbus::Path;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

use crate::desktop_bus::{self, call, call_and_wait};

const SERVICE: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
const PROMPT_IFACE: &str = "org.freedesktop.Secret.Prompt";

/// What sealed data starts with.
const TAG: &[u8] = b"v11";

/// How long the user has to answer the keyring's prompt (to unlock it, or to make it).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a failure to reach the secret is kept before the next try.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// The secret item's attributes.
fn attributes() -> HashMap<String, String> {
    HashMap::from([
        ("application".to_owned(), "snowlark".to_owned()),
        ("purpose".to_owned(), "safe-storage".to_owned()),
    ])
}

/// Why data cannot be protected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// No session bus, or nothing on it answers as the Secret Service.
    NoService,
    /// The Secret Service answered but gave no secret (locked, refused, or failed).
    Refused,
}

enum Cached {
    Key([u8; 32]),
    Failed(Unavailable, Instant),
}

static KEY: Mutex<Option<Cached>> = Mutex::new(None);

/// The key data is sealed under, from the Secret Service (asked once; after a failure, again after
/// a while).
fn key() -> Result<[u8; 32], Unavailable> {
    let mut cached = KEY.lock().unwrap_or_else(|e| e.into_inner());
    match &*cached {
        Some(Cached::Key(k)) => return Ok(*k),
        Some(Cached::Failed(why, at)) if at.elapsed() < RETRY_AFTER => return Err(*why),
        _ => {}
    }
    let got = secret().map(|s| {
        let digest = ring::digest::digest(&ring::digest::SHA256, &s);
        let mut k = [0u8; 32];
        k.copy_from_slice(digest.as_ref());
        k
    });
    *cached = Some(match got {
        Ok(k) => Cached::Key(k),
        Err(why) => Cached::Failed(why, Instant::now()),
    });
    got
}

/// Whether data can be protected now, and if not, why.
pub fn available() -> Result<(), Unavailable> {
    key().map(|_| ())
}

/// `data` protected for the current user; none where the Secret Service is not there
/// ([`available`] says why).
pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
    seal(&key().ok()?, data)
}

/// What [`protect`] made, read back.
pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    open(&key().ok()?, data)
}

fn seal(key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).ok()?);
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new().fill(&mut nonce).ok()?;
    let mut sealed = data.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::empty(),
        &mut sealed,
    )
    .ok()?;
    let mut out = TAG.to_vec();
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Some(out)
}

fn open(key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
    let rest = data.strip_prefix(TAG)?;
    if rest.len() < NONCE_LEN {
        return None;
    }
    let (nonce, sealed) = rest.split_at(NONCE_LEN);
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).ok()?);
    let mut buf = sealed.to_vec();
    let plain = key
        .open_in_place(
            Nonce::try_assume_unique_for_key(nonce).ok()?,
            Aad::empty(),
            &mut buf,
        )
        .ok()?;
    Some(plain.to_vec())
}

/// The browser's secret, from the Secret Service: the item's, or a new one stored there.
fn secret() -> Result<Vec<u8>, Unavailable> {
    let conn = desktop_bus::session().ok_or(Unavailable::NoService)?;
    if !desktop_bus::has_service(&conn, SERVICE) {
        return Err(Unavailable::NoService);
    }
    secret_on(&conn).ok_or(Unavailable::Refused)
}

/// The Secret Service API's steps ("Default collection", "Unlocking", "Transfer of secrets" in
/// plain): the default collection (made if there is none), unlocked; a session; the item found by
/// its attributes, or made.
fn secret_on(conn: &Connection) -> Option<Vec<u8>> {
    let collection = default_collection(conn)?;
    let (unlocked, prompt): (Vec<Path<'static>>, Path<'static>) = call(
        conn,
        SERVICE,
        SERVICE_PATH,
        SERVICE_IFACE,
        "Unlock",
        (vec![collection.clone()],),
    )?;
    if unlocked.is_empty() {
        prompt_for(conn, &prompt)?;
    }
    let plain: Box<dyn RefArg> = Box::new(String::new());
    let (_, session): (Variant<Box<dyn RefArg>>, Path<'static>) = call(
        conn,
        SERVICE,
        SERVICE_PATH,
        SERVICE_IFACE,
        "OpenSession",
        ("plain", Variant(plain)),
    )?;
    let answer = item_secret(conn, &collection, &session);
    let _: Option<()> = call(
        conn,
        SERVICE,
        &session,
        "org.freedesktop.Secret.Session",
        "Close",
        (),
    );
    answer
}

/// The default collection's path; a new one (after the keyring's prompt) where there is none.
fn default_collection(conn: &Connection) -> Option<Path<'static>> {
    let (path,): (Path<'static>,) = call(
        conn,
        SERVICE,
        SERVICE_PATH,
        SERVICE_IFACE,
        "ReadAlias",
        ("default",),
    )?;
    if &*path != "/" {
        return Some(path);
    }
    let mut props = PropMap::new();
    props.insert(
        "org.freedesktop.Secret.Collection.Label".to_owned(),
        Variant(Box::new("Default keyring".to_owned())),
    );
    let (made, prompt): (Path<'static>, Path<'static>) = call(
        conn,
        SERVICE,
        SERVICE_PATH,
        SERVICE_IFACE,
        "CreateCollection",
        (props, "default"),
    )?;
    if &*made != "/" {
        return Some(made);
    }
    let result = prompt_for(conn, &prompt)?;
    result.0.as_str().and_then(|p| Path::new(p.to_owned()).ok())
}

/// Shows the Secret Service's prompt and waits for the user; the prompt's result, if not
/// dismissed. A prompt of `/` is no prompt.
fn prompt_for(conn: &Connection, prompt: &Path<'static>) -> Option<Variant<Box<dyn RefArg>>> {
    if &**prompt == "/" {
        return Some(Variant(Box::new(String::new())));
    }
    let (dismissed, result): (bool, Variant<Box<dyn RefArg>>) = call_and_wait(
        conn,
        SERVICE,
        prompt,
        PROMPT_IFACE,
        "Completed",
        PROMPT_TIMEOUT,
        || call::<(), _>(conn, SERVICE, prompt, PROMPT_IFACE, "Prompt", ("",)),
    )?;
    (!dismissed).then_some(result)
}

type Secret = (Path<'static>, Vec<u8>, Vec<u8>, String);

/// The secret of the browser's item in `collection`, made if there is none.
fn item_secret(
    conn: &Connection,
    collection: &Path<'static>,
    session: &Path<'static>,
) -> Option<Vec<u8>> {
    let (items,): (Vec<Path<'static>>,) = call(
        conn,
        SERVICE,
        collection,
        COLLECTION_IFACE,
        "SearchItems",
        (attributes(),),
    )?;
    if let Some(item) = items.first() {
        let (secrets,): (HashMap<Path<'static>, Secret>,) = call(
            conn,
            SERVICE,
            SERVICE_PATH,
            SERVICE_IFACE,
            "GetSecrets",
            (vec![item.clone()], session.clone()),
        )?;
        let value = secrets.get(item)?.2.clone();
        return (!value.is_empty()).then_some(value);
    }
    let mut value = [0u8; 32];
    SystemRandom::new().fill(&mut value).ok()?;
    let mut props = PropMap::new();
    props.insert(
        "org.freedesktop.Secret.Item.Label".to_owned(),
        Variant(Box::new("Snowlark Safe Storage".to_owned())),
    );
    props.insert(
        "org.freedesktop.Secret.Item.Attributes".to_owned(),
        Variant(Box::new(attributes())),
    );
    let secret: Secret = (
        session.clone(),
        Vec::new(),
        value.to_vec(),
        "application/octet-stream".to_owned(),
    );
    let (made, prompt): (Path<'static>, Path<'static>) = call(
        conn,
        SERVICE,
        collection,
        COLLECTION_IFACE,
        "CreateItem",
        (props, secret, true),
    )?;
    if &*made == "/" {
        prompt_for(conn, &prompt)?;
    }
    Some(value.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_data_opens_only_under_its_key() {
        let k = [7u8; 32];
        let sealed = seal(&k, b"the store's key").unwrap();
        assert!(sealed.starts_with(TAG));
        assert_eq!(open(&k, &sealed).unwrap(), b"the store's key");
        assert!(open(&[8u8; 32], &sealed).is_none());
        let mut bent = sealed.clone();
        *bent.last_mut().unwrap() ^= 1;
        assert!(open(&k, &bent).is_none());
        // (a fresh nonce each time)
        assert_ne!(seal(&k, b"x").unwrap(), seal(&k, b"x").unwrap());
    }

    /// Against a running Secret Service (`SNOWLARK_TEST_SECRET_SERVICE=1`, a session bus with an
    /// unlocked keyring): the secret is made once, then found again.
    #[test]
    fn the_secret_is_kept_by_the_secret_service() {
        if std::env::var_os("SNOWLARK_TEST_SECRET_SERVICE").is_none() {
            return;
        }
        let first = secret().expect("a secret");
        assert_eq!(first.len(), 32);
        assert_eq!(secret().expect("the same secret"), first);
        let sealed = protect(b"key").unwrap();
        assert_eq!(unprotect(&sealed).unwrap(), b"key");
    }

    /// With no session bus there is no Secret Service, and nothing is protected.
    #[test]
    fn without_a_session_bus_nothing_is_protected() {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
            return;
        }
        assert_eq!(secret(), Err(Unavailable::NoService));
    }
}
