// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `localStorage` as a service (`docs/process-isolation.md` sections 6 and 15): the browser owns the
//! areas, and each event loop keeps a cached copy of the ones it uses. Reads are local. A write
//! applies at once in the cache and goes out without waiting, and the service tells every other event
//! loop with that area, which fires `storage` there.
//!
//! [`WebStorage`] is the seam: every call and notice is plain data, so it can become messages on a
//! pipe with nothing changed on either side. [`WebStorageHost`] is the implementation, for a browser
//! (shared by its tabs, persisted by its profile) and for a page on its own (in memory). It touches
//! no files: an embedder that persists gives it a [`Loader`] and collects [`WebStorageHost::take_dirty`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// The quota of one storage area, in UTF-16 code units of keys and values (HTML suggests 5 MB).
pub const QUOTA: usize = 5 * 1024 * 1024;

/// An event loop, as the service knows it.
pub type ClientId = u64;

/// A storage area's contents, in order.
pub type Items = Vec<(String, String)>;

/// A change an event loop made to its cached copy of an area.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageWrite {
    Set {
        origin: String,
        key: String,
        value: String,
        /// The address of the document that changed it (`StorageEvent.url`).
        url: String,
    },
    Remove {
        origin: String,
        key: String,
        url: String,
    },
    Clear {
        origin: String,
        url: String,
    },
}

/// What the service tells an event loop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageNotice {
    /// An area changed. `by` hears it too, as the answer to its own write. A `key` of `None` is a
    /// `clear()`.
    Changed {
        origin: String,
        key: Option<String>,
        old_value: Option<String>,
        new_value: Option<String>,
        url: String,
        by: ClientId,
    },
    /// The service refused a write (an area past its quota, which an event loop can only reach by
    /// racing another): the area as it really is.
    Refused { origin: String, items: Items },
    /// The area was emptied from outside the page ("clear site data"): no `storage` event.
    Wiped { origin: String },
}

/// Where notices go: the receiving end of a pipe, in time.
pub type Notify = Arc<dyn Fn(StorageNotice) + Send + Sync>;

/// The service, as an event loop sees it.
pub trait WebStorage: Send + Sync {
    fn connect(&self, notify: Notify) -> ClientId;
    fn disconnect(&self, client: ClientId);
    /// The area of `origin`, from now on told to this client. The one call that waits for an
    /// answer, once per area per event loop.
    fn open(&self, client: ClientId, origin: &str) -> Items;
    /// Never waits.
    fn write(&self, client: ClientId, write: StorageWrite);
}

/// What the embedder has kept of an origin's area.
pub type Loader = Box<dyn Fn(&str) -> Option<Items> + Send + Sync>;

fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The size of an entry against the quota.
pub(crate) fn entry_units(key: &str, value: &str) -> usize {
    units(key) + units(value)
}

/// What a value would make an area's size.
pub(crate) fn size_after(area: &Area, key: &str, value: &str) -> usize {
    match area.get(key) {
        Some(old) => area.size - units(old) + units(value),
        None => area.size + entry_units(key, value),
    }
}

/// Whether an origin is on `site` (a host: its registrable domain and subdomains), or on any site.
pub fn site_matcher(site: Option<&str>) -> impl Fn(&str) -> bool {
    let site = site.map(|h| {
        snowlark_net::origin::registrable_domain(&url::Host::Domain(h.to_owned()))
            .unwrap_or_else(|| h.to_owned())
    });
    move |origin: &str| {
        let Some(site) = &site else {
            return true;
        };
        let host = url::Url::parse(origin)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned));
        host.is_some_and(|h| snowlark_net::cookies::domain_match(&h, site))
    }
}

/// An area's contents: in order, found by key.
#[derive(Default)]
pub(crate) struct Area {
    pub(crate) items: Items,
    /// Where each key is in `items`.
    index: HashMap<String, usize>,
    pub(crate) size: usize,
}

impl Area {
    pub(crate) fn from_items(items: Items) -> Area {
        let index = items
            .iter()
            .enumerate()
            .map(|(i, (k, _))| (k.clone(), i))
            .collect();
        let size = items.iter().map(|(k, v)| entry_units(k, v)).sum();
        Area { items, index, size }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.index.get(key).map(|i| self.items[*i].1.as_str())
    }

    pub(crate) fn set(&mut self, key: String, value: String) -> Option<String> {
        match self.index.get(&key) {
            Some(i) => {
                let old = std::mem::replace(&mut self.items[*i].1, value);
                self.size = self.size + units(&self.items[*i].1) - units(&old);
                Some(old)
            }
            None => {
                self.size += entry_units(&key, &value);
                self.index.insert(key.clone(), self.items.len());
                self.items.push((key, value));
                None
            }
        }
    }

    pub(crate) fn remove(&mut self, key: &str) -> Option<String> {
        let i = self.index.remove(key)?;
        let (k, v) = self.items.remove(i);
        self.size -= entry_units(&k, &v);
        for at in self.index.values_mut() {
            if *at > i {
                *at -= 1;
            }
        }
        Some(v)
    }
}

struct Client {
    id: ClientId,
    notify: Notify,
    origins: HashSet<String>,
}

#[derive(Default)]
struct Inner {
    next: ClientId,
    clients: Vec<Client>,
    areas: HashMap<String, Area>,
    dirty: HashSet<String>,
}

/// The areas of a browser, or of a page on its own.
#[derive(Default)]
pub struct WebStorageHost {
    inner: Mutex<Inner>,
    loader: Option<Loader>,
}

impl std::fmt::Debug for WebStorageHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebStorageHost").finish_non_exhaustive()
    }
}

impl WebStorageHost {
    /// Areas in memory, gone with the host.
    pub fn new() -> Arc<WebStorageHost> {
        Arc::default()
    }

    /// Areas that `loader` fills when they are first opened; the embedder keeps what
    /// [`WebStorageHost::take_dirty`] gives it.
    pub fn with_loader(loader: Loader) -> Arc<WebStorageHost> {
        Arc::new(WebStorageHost {
            inner: Mutex::default(),
            loader: Some(loader),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn area<'a>(&self, inner: &'a mut Inner, origin: &str) -> &'a mut Area {
        inner.areas.entry(origin.to_owned()).or_insert_with(|| {
            let items = self.loader.as_ref().and_then(|l| l(origin));
            Area::from_items(items.unwrap_or_default())
        })
    }

    /// The areas changed since the last call, as they are now.
    pub fn take_dirty(&self) -> Vec<(String, Items)> {
        let mut inner = self.lock();
        let dirty: Vec<String> = inner.dirty.drain().collect();
        dirty
            .into_iter()
            .filter_map(|o| {
                let items = inner.areas.get(&o)?.items.clone();
                Some((o, items))
            })
            .collect()
    }

    /// Empties the area of `origin` for everyone, with no `storage` event.
    pub fn wipe(&self, origin: &str) {
        let mut inner = self.lock();
        inner.areas.insert(origin.to_owned(), Area::default());
        inner.dirty.remove(origin);
        for c in inner.clients.iter().filter(|c| c.origins.contains(origin)) {
            (c.notify)(StorageNotice::Wiped {
                origin: origin.to_owned(),
            });
        }
    }

    /// The origins with an area in memory.
    pub fn origins(&self) -> Vec<String> {
        self.lock().areas.keys().cloned().collect()
    }
}

impl WebStorage for WebStorageHost {
    fn connect(&self, notify: Notify) -> ClientId {
        let mut inner = self.lock();
        inner.next += 1;
        let id = inner.next;
        inner.clients.push(Client {
            id,
            notify,
            origins: HashSet::new(),
        });
        id
    }

    fn disconnect(&self, client: ClientId) {
        self.lock().clients.retain(|c| c.id != client);
    }

    fn open(&self, client: ClientId, origin: &str) -> Items {
        let mut inner = self.lock();
        let items = self.area(&mut inner, origin).items.clone();
        if let Some(c) = inner.clients.iter_mut().find(|c| c.id == client) {
            c.origins.insert(origin.to_owned());
        }
        items
    }

    fn write(&self, client: ClientId, write: StorageWrite) {
        let mut inner = self.lock();
        let inner = &mut *inner;
        let origin = match &write {
            StorageWrite::Set { origin, .. }
            | StorageWrite::Remove { origin, .. }
            | StorageWrite::Clear { origin, .. } => origin.clone(),
        };
        let area = self.area(inner, &origin);
        let (key, old_value, new_value, url) = match write {
            StorageWrite::Set {
                key, value, url, ..
            } => {
                if size_after(area, &key, &value) > QUOTA {
                    let notice = StorageNotice::Refused {
                        origin: origin.clone(),
                        items: area.items.clone(),
                    };
                    if let Some(c) = inner.clients.iter().find(|c| c.id == client) {
                        (c.notify)(notice);
                    }
                    return;
                }
                let old = area.set(key.clone(), value.clone());
                (Some(key), old, Some(value), url)
            }
            StorageWrite::Remove { key, url, .. } => {
                let old = area.remove(&key);
                (Some(key), old, None, url)
            }
            StorageWrite::Clear { url, .. } => {
                *area = Area::default();
                (None, None, None, url)
            }
        };
        inner.dirty.insert(origin.clone());
        // (under the lock: every client hears the changes of an area in the same order)
        for c in inner.clients.iter().filter(|c| c.origins.contains(&origin)) {
            (c.notify)(StorageNotice::Changed {
                origin: origin.clone(),
                key: key.clone(),
                old_value: old_value.clone(),
                new_value: new_value.clone(),
                url: url.clone(),
                by: client,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Heard = Arc<Mutex<Vec<StorageNotice>>>;

    fn client(host: &WebStorageHost) -> (ClientId, Heard) {
        let heard: Heard = Arc::default();
        let h = heard.clone();
        let id = host.connect(Arc::new(move |n| h.lock().unwrap().push(n)));
        (id, heard)
    }

    fn set(key: &str, value: &str) -> StorageWrite {
        StorageWrite::Set {
            origin: "https://a.example".into(),
            key: key.into(),
            value: value.into(),
            url: "https://a.example/page".into(),
        }
    }

    const A: &str = "https://a.example";

    #[test]
    fn a_write_reaches_every_client_of_the_area_and_no_other() {
        let host = WebStorageHost::new();
        let (a, heard_a) = client(&host);
        let (b, heard_b) = client(&host);
        let (c, heard_c) = client(&host);
        host.open(a, A);
        host.open(b, A);
        host.open(c, "https://b.example");
        host.write(a, set("k", "1"));
        let want = StorageNotice::Changed {
            origin: A.into(),
            key: Some("k".into()),
            old_value: None,
            new_value: Some("1".into()),
            url: "https://a.example/page".into(),
            by: a,
        };
        assert_eq!(
            heard_a.lock().unwrap().as_slice(),
            std::slice::from_ref(&want)
        );
        assert_eq!(heard_b.lock().unwrap().as_slice(), [want]);
        assert!(heard_c.lock().unwrap().is_empty());
    }

    #[test]
    fn a_later_opener_sees_the_area() {
        let host = WebStorageHost::new();
        let (a, _) = client(&host);
        host.open(a, A);
        host.write(a, set("x", "1"));
        host.write(a, set("y", "2"));
        host.write(a, set("x", "3"));
        host.write(
            a,
            StorageWrite::Remove {
                origin: A.into(),
                key: "y".into(),
                url: String::new(),
            },
        );
        let (b, _) = client(&host);
        assert_eq!(host.open(b, A), [("x".to_owned(), "3".to_owned())]);
    }

    #[test]
    fn the_host_has_the_last_word_on_the_quota() {
        let host = WebStorageHost::new();
        let (a, heard) = client(&host);
        host.open(a, A);
        host.write(a, set("big", &"x".repeat(QUOTA - 3)));
        heard.lock().unwrap().clear();
        host.write(a, set("no", "too much"));
        let n = heard.lock().unwrap();
        assert!(matches!(&n[..], [StorageNotice::Refused { items, .. }] if items.len() == 1));
    }

    #[test]
    fn replacing_a_value_counts_only_the_difference() {
        let host = WebStorageHost::new();
        let (a, heard) = client(&host);
        host.open(a, A);
        host.write(a, set("big", &"x".repeat(QUOTA - 3)));
        host.write(a, set("big", &"y".repeat(QUOTA - 3)));
        assert_eq!(heard.lock().unwrap().len(), 2);
    }

    #[test]
    fn dirty_areas_are_handed_over_once() {
        let host = WebStorageHost::new();
        let (a, _) = client(&host);
        host.open(a, A);
        assert!(host.take_dirty().is_empty());
        host.write(a, set("k", "v"));
        let dirty = host.take_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].0, A);
        assert!(host.take_dirty().is_empty());
    }

    #[test]
    fn the_loader_fills_an_area_on_first_touch() {
        let host = WebStorageHost::with_loader(Box::new(|origin| {
            (origin == A).then(|| vec![("kept".to_owned(), "1".to_owned())])
        }));
        let (a, _) = client(&host);
        assert_eq!(host.open(a, A).len(), 1);
        assert!(host.open(a, "https://b.example").is_empty());
    }

    #[test]
    fn wiping_tells_the_clients_and_keeps_nothing_dirty() {
        let host = WebStorageHost::new();
        let (a, heard) = client(&host);
        host.open(a, A);
        host.write(a, set("k", "v"));
        heard.lock().unwrap().clear();
        host.wipe(A);
        assert_eq!(
            heard.lock().unwrap().as_slice(),
            [StorageNotice::Wiped { origin: A.into() }]
        );
        assert!(host.take_dirty().is_empty());
        let (b, _) = client(&host);
        assert!(host.open(b, A).is_empty());
    }

    #[test]
    fn a_client_that_left_hears_nothing() {
        let host = WebStorageHost::new();
        let (a, heard_a) = client(&host);
        let (b, _) = client(&host);
        host.open(a, A);
        host.open(b, A);
        host.disconnect(a);
        host.write(b, set("k", "v"));
        assert!(heard_a.lock().unwrap().is_empty());
    }

    #[test]
    fn removing_keeps_the_order_and_the_index() {
        let mut area = Area::default();
        for k in ["a", "b", "c"] {
            area.set(k.into(), "v".into());
        }
        assert_eq!(area.remove("a").as_deref(), Some("v"));
        assert_eq!(area.get("c"), Some("v"));
        assert_eq!(area.get("b"), Some("v"));
        assert_eq!(area.size, 4);
        assert_eq!(area.items[0].0, "b");
    }
}
