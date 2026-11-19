// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The blob URL store (File API §8.3). The engine registers `Blob`s here when script calls
//! `URL.createObjectURL`; `fetch` resolves `blob:` URLs against it.
//!
//! Entries remember the origin of the environment that created them: that is the URL's origin
//! ([`crate::origin::origin_of`]), and revocation is only honoured from the same origin.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use url::{Origin, Url};

/// A blob's bytes and its type (already lowercased by the Blob constructor, or empty).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlobData {
    pub bytes: Bytes,
    pub mime_type: String,
}

impl BlobData {
    pub fn new(bytes: Bytes, mime_type: &str) -> Self {
        BlobData {
            bytes,
            mime_type: mime_type.to_string(),
        }
    }
}

/// A blob URL entry: the object and the origin of its environment.
#[derive(Clone, Debug)]
pub struct BlobEntry {
    pub blob: Arc<BlobData>,
    pub origin: Origin,
}

/// The store, shared between the page threads (which register and revoke) and the network thread.
///
/// `held` are the blob URL entries of URLs that were parsed (URL Standard §4.4 "URL parsing" sets a
/// `blob:` URL's blob URL entry by resolving it then): a fetch of such a URL reads the blob the URL
/// named when it was parsed, even if the URL was revoked since (Fetch §4.2 scheme fetch, `blob:`,
/// reads the entry, not the store). A hold is a token the request carries
/// ([`crate::Request::blob_url_entry`]); the entry stays until released or its origin goes.
#[derive(Debug, Default)]
pub struct BlobStore {
    entries: Mutex<HashMap<String, BlobEntry>>,
    held: Mutex<HashMap<u64, (String, BlobEntry)>>,
    next_hold: std::sync::atomic::AtomicU64,
}

fn key(url: &Url) -> String {
    let s = url.as_str();
    match s.find('#') {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// A random version 4 UUID, lowercase (File API "generate a new blob URL").
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("the operating system's random number generator failed");
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

impl BlobStore {
    /// "add an entry to the blob URL store": a new `blob:<origin>/<uuid>` URL for `blob`.
    pub fn register(&self, origin: &Origin, blob: BlobData) -> Url {
        let serialized = origin.ascii_serialization();
        let url = Url::parse(&format!("blob:{serialized}/{}", uuid_v4()))
            .expect("a blob URL always parses");
        self.entries.lock().unwrap().insert(
            key(&url),
            BlobEntry {
                blob: Arc::new(blob),
                origin: origin.clone(),
            },
        );
        url
    }

    /// "remove an entry from the blob URL store", only from the creating origin. True when removed.
    /// The key is the URL's serialization as it is (File API §8.3.2: no fragment is excluded, so a
    /// URL with one names no entry), which is why `revokeObjectURL(url + '#x')` revokes nothing.
    pub fn revoke(&self, url: &Url, origin: &Origin) -> bool {
        let mut entries = self.entries.lock().unwrap();
        let k = url.as_str().to_string();
        match entries.get(&k) {
            Some(entry) if entry.origin == *origin => {
                entries.remove(&k);
                true
            }
            _ => false,
        }
    }

    /// "resolve a blob URL": the entry for the URL (fragment ignored), if any.
    pub fn resolve(&self, url: &Url) -> Option<BlobEntry> {
        if url.scheme() != "blob" {
            return None;
        }
        self.entries.lock().unwrap().get(&key(url)).cloned()
    }

    /// Takes a hold on the entry `url` names now: the URL's blob URL entry, under a token a request
    /// of that URL carries. `None` when the URL names no entry.
    pub fn hold(&self, url: &Url) -> Option<u64> {
        let entry = self.resolve(url)?;
        let token = self
            .next_hold
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.held.lock().unwrap().insert(token, (key(url), entry));
        Some(token)
    }

    /// The entry held under `token`, if it is one of `url` (a token is only ever used with the URL
    /// it was taken for: one named with another URL is nothing).
    pub fn held(&self, token: u64, url: &Url) -> Option<BlobEntry> {
        let held = self.held.lock().unwrap();
        let (k, entry) = held.get(&token)?;
        (*k == key(url)).then(|| entry.clone())
    }

    /// Releases a hold: the request that carried it is done.
    pub fn release(&self, token: u64) {
        self.held.lock().unwrap().remove(&token);
    }

    /// Drop every entry an origin registered, and every hold on one (its environment went away).
    pub fn revoke_all_for(&self, origin: &Origin) {
        self.entries
            .lock()
            .unwrap()
            .retain(|_, entry| entry.origin != *origin);
        self.held
            .lock()
            .unwrap()
            .retain(|_, (_, entry)| entry.origin != *origin);
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_resolve_revoke() {
        let store = BlobStore::default();
        let origin = Url::parse("https://example.com/page").unwrap().origin();
        let url = store.register(
            &origin,
            BlobData::new(Bytes::from_static(b"hi"), "text/plain"),
        );
        assert!(url.as_str().starts_with("blob:https://example.com/"));
        assert_eq!(url.as_str().len(), "blob:https://example.com/".len() + 36);
        let with_fragment = Url::parse(&format!("{url}#frag")).unwrap();
        assert_eq!(
            &store.resolve(&with_fragment).unwrap().blob.bytes[..],
            b"hi"
        );
        let other = Url::parse("https://evil.example/").unwrap().origin();
        assert!(!store.revoke(&url, &other));
        // only the exact URL revokes: one with a fragment names no entry
        assert!(!store.revoke(&with_fragment, &origin));
        assert!(store.resolve(&url).is_some());
        assert!(store.revoke(&url, &origin));
        assert!(store.resolve(&url).is_none());
    }

    #[test]
    fn a_held_entry_outlives_its_revocation() {
        let store = BlobStore::default();
        let origin = Url::parse("https://example.com/page").unwrap().origin();
        let url = store.register(
            &origin,
            BlobData::new(Bytes::from_static(b"held"), "text/plain"),
        );
        let token = store.hold(&url).unwrap();
        assert!(store.revoke(&url, &origin));
        assert!(store.resolve(&url).is_none());
        // the hold still reads it, but only with the URL it was taken for
        assert_eq!(&store.held(token, &url).unwrap().blob.bytes[..], b"held");
        let another = Url::parse("blob:https://example.com/0000-1").unwrap();
        assert!(store.held(token, &another).is_none());
        assert!(
            store.hold(&url).is_none(),
            "a revoked URL names no entry to hold"
        );
        store.release(token);
        assert!(store.held(token, &url).is_none());
        // an origin's end drops its holds too
        let url = store.register(&origin, BlobData::new(Bytes::from_static(b"x"), ""));
        let token = store.hold(&url).unwrap();
        store.revoke_all_for(&origin);
        assert!(store.held(token, &url).is_none());
    }

    #[test]
    fn opaque_creator() {
        let store = BlobStore::default();
        let origin = Origin::new_opaque();
        let url = store.register(&origin, BlobData::new(Bytes::new(), ""));
        assert!(url.as_str().starts_with("blob:null/"));
        store.revoke_all_for(&origin);
        assert!(store.is_empty());
    }
}
