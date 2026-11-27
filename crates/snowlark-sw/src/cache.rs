// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Cache API's storage (Service Workers §5.4 `Cache`, §5.5 `CacheStorage`): per storage key, a
//! name to cache map in creation order, and per cache a request-response list.
//!
//! The page side converts its `Request` and `Response` objects to [`CachedRequest`] and
//! [`CachedResponse`] and sends one [`Op`] per method call; the matching ("Query Cache", "Request
//! Matches Cached Item") and the all-or-nothing writes ("Batch Cache Operations") happen here, so a
//! renderer never holds another key's entries.
//!
//! [`LocalCacheStorage`] keeps the lists in memory, and with a folder also on disk: a folder per
//! storage key holding `index` (the names, the requests and the response heads) and a file per
//! response body, written before the index that names it. A body no index names (left by a cache
//! deleted while a page still used it, or by a write cut short) is removed when the key is next
//! loaded.
//!
//! A cache deleted from the map while a `Cache` object still represents it keeps working on its own
//! list (§5.5.5 `delete()` only removes the name): such a cache stays in memory until the key's
//! data is cleared or the storage is dropped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// A cache, as the storage names it to pages. Unique in a [`LocalCacheStorage`] across keys.
pub type CacheId = u64;

/// A request as a cache keeps it: what matching reads (method, URL, headers) and what `keys()`
/// gives back in a `Request` (its other members, as their IDL enumeration values).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub mode: String,
    pub credentials: String,
    pub cache: String,
    pub redirect: String,
    pub destination: String,
    /// "client", "" (no referrer) or a URL.
    pub referrer: String,
    pub referrer_policy: String,
    pub integrity: String,
}

/// A response as a cache keeps it. `body` is `None` for a null body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedResponse {
    /// `ResponseType`'s value: "basic", "cors", "default", "opaque", "opaqueredirect".
    pub type_: String,
    pub url_list: Vec<String>,
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// `CacheQueryOptions`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryOptions {
    pub ignore_search: bool,
    pub ignore_method: bool,
    pub ignore_vary: bool,
}

/// A cache batch operation (§5.4.9 "Batch Cache Operations").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BatchOp {
    Put {
        request: CachedRequest,
        response: CachedResponse,
    },
    Delete {
        request: CachedRequest,
        options: QueryOptions,
    },
}

/// One call on a storage key's caches.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Op {
    /// `CacheStorage.has(name)`: [`Reply::Bool`].
    Has { name: String },
    /// `CacheStorage.open(name)`, making the cache when there is none: [`Reply::Cache`].
    Open { name: String },
    /// `CacheStorage.delete(name)`: [`Reply::Bool`].
    Delete { name: String },
    /// `CacheStorage.keys()`: [`Reply::Names`], in creation order.
    Names,
    /// `CacheStorage.match()`: the first match of the caches in creation order, or of the one named
    /// (none when there is no such cache): [`Reply::Responses`] with at most one.
    MatchAny {
        cache_name: Option<String>,
        request: CachedRequest,
        options: QueryOptions,
    },
    /// `Cache.matchAll()` (`match()` with `first`): every response of the cache, or those whose
    /// request matches: [`Reply::Responses`].
    Match {
        cache: CacheId,
        request: Option<CachedRequest>,
        options: QueryOptions,
        first: bool,
    },
    /// `Cache.keys()`: [`Reply::Requests`].
    Keys {
        cache: CacheId,
        request: Option<CachedRequest>,
        options: QueryOptions,
    },
    /// `put()`, `add()`, `addAll()` and `delete()`: [`Reply::Batch`], all or nothing.
    Batch { cache: CacheId, ops: Vec<BatchOp> },
}

/// The answer to an [`Op`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Bool(bool),
    Cache(CacheId),
    Names(Vec<String>),
    Responses(Vec<CachedResponse>),
    Requests(Vec<CachedRequest>),
    /// Per operation, how many entries it removed.
    Batch(Vec<u32>),
    Error(CacheError),
}

/// Why an operation failed: the exception the page rejects with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheError {
    /// A `TypeError`.
    Type(String),
    /// An "InvalidStateError" DOMException.
    InvalidState(String),
    /// A "QuotaExceededError" DOMException: the storage could not keep the data.
    QuotaExceeded,
    /// A cache this key does not have (its data was cleared).
    UnknownCache,
}

/// The Cache API's storage as a page sees it: one call per method, answered at once.
pub trait CacheStorageService: Send + Sync {
    /// Runs `op` on the caches of storage key `key`.
    fn call(&self, key: &str, op: Op) -> Reply;
}

// ----- matching ---------------------------------------------------------------------------------

/// "Getting" a header (Fetch §2.2.2): its values combined with ", ", or `None`.
fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    let mut out: Option<String> = None;
    for (n, v) in headers {
        if n.eq_ignore_ascii_case(name) {
            match &mut out {
                Some(s) => {
                    s.push_str(", ");
                    s.push_str(v);
                }
                None => out = Some(v.clone()),
            }
        }
    }
    out
}

/// The field names a `Vary` header lists.
fn vary_fields(headers: &[(String, String)]) -> Vec<String> {
    header_value(headers, "vary")
        .map(|v| {
            v.split(',')
                .map(|f| f.trim_matches([' ', '\t']).to_owned())
                .filter(|f| !f.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a response's `Vary` header lists `*` (such a response is never stored).
pub fn varies_on_everything(headers: &[(String, String)]) -> bool {
    vary_fields(headers).iter().any(|f| f == "*")
}

/// A URL as matching compares it: without its fragment ("exclude fragments"), and without its
/// query with `ignore_search`.
fn url_key(url: &str, ignore_search: bool) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            u.set_fragment(None);
            if ignore_search {
                u.set_query(None);
            }
            u.into()
        }
        Err(_) => url.to_owned(),
    }
}

/// §5.4.8 "Request Matches Cached Item" of `query` against the stored `request` and `response`.
pub fn request_matches(
    query: &CachedRequest,
    request: &CachedRequest,
    response: Option<&CachedResponse>,
    options: QueryOptions,
) -> bool {
    if !options.ignore_method && query.method != "GET" {
        return false;
    }
    if url_key(&query.url, options.ignore_search) != url_key(&request.url, options.ignore_search) {
        return false;
    }
    let Some(response) = response else {
        return true;
    };
    if options.ignore_vary {
        return true;
    }
    for field in vary_fields(&response.headers) {
        if field == "*" {
            return false;
        }
        if header_value(&request.headers, &field) != header_value(&query.headers, &field) {
            return false;
        }
    }
    true
}

// ----- the storage ------------------------------------------------------------------------------

/// Where an entry's body is.
#[derive(Clone, Debug)]
enum Body {
    Null,
    Memory(Arc<Vec<u8>>),
    /// The key folder's `<id>.body`.
    File(u64),
}

/// A request-response list's item. `response.body` is always `None`: the bytes are in `body`.
#[derive(Clone, Debug)]
struct Entry {
    request: CachedRequest,
    response: CachedResponse,
    body: Body,
}

/// One storage key's caches.
#[derive(Default)]
struct KeyState {
    /// The name to cache map, in creation order.
    names: Vec<(String, CacheId)>,
    /// Every cache of the key, those deleted from the map but still in use included.
    caches: HashMap<CacheId, Vec<Entry>>,
    /// The id of the next body file.
    next_body: u64,
}

/// What the key's `index` file holds.
#[derive(Serialize, Deserialize)]
struct Index {
    next_body: u64,
    caches: Vec<(String, Vec<DiskEntry>)>,
}

#[derive(Serialize, Deserialize)]
struct DiskEntry {
    request: CachedRequest,
    response: CachedResponse,
    /// The body file's id, `None` for a null body.
    body: Option<u64>,
}

struct Inner {
    dir: Option<PathBuf>,
    keys: HashMap<String, KeyState>,
    /// Each cache's storage key.
    owner: HashMap<CacheId, String>,
    next_cache: CacheId,
}

/// The storage in this process: see the module documentation.
pub struct LocalCacheStorage {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for LocalCacheStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCacheStorage").finish_non_exhaustive()
    }
}

/// A storage key as a folder name: bytes other than ASCII letters, digits, `.` and `-` as `%XX`.
pub(crate) fn folder_name(key: &str) -> String {
    let mut s = String::with_capacity(key.len() * 2);
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || b == b'.' || b == b'-' {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// The storage key a folder name stands for.
pub(crate) fn key_of_folder(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = name.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Whether storage key `key` (an origin) belongs to `site` (a host: it and its subdomains).
fn key_in_site(key: &str, site: &str) -> bool {
    let Ok(u) = url::Url::parse(key) else {
        return false;
    };
    let Some(host) = u.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let site = site.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case(site)
        || (host.len() > site.len()
            && host.as_bytes()[host.len() - site.len() - 1] == b'.'
            && host[host.len() - site.len()..].eq_ignore_ascii_case(site))
}

fn body_path(folder: &Path, id: u64) -> PathBuf {
    folder.join(format!("{id}.body"))
}

impl Default for LocalCacheStorage {
    fn default() -> Self {
        LocalCacheStorage::in_memory()
    }
}

impl LocalCacheStorage {
    /// Storage that keeps nothing on disk.
    pub fn in_memory() -> LocalCacheStorage {
        LocalCacheStorage {
            inner: Mutex::new(Inner {
                dir: None,
                keys: HashMap::new(),
                owner: HashMap::new(),
                next_cache: 0,
            }),
        }
    }

    /// Storage kept in `dir`, a folder per storage key.
    pub fn persistent(dir: PathBuf) -> LocalCacheStorage {
        let s = LocalCacheStorage::in_memory();
        s.lock().dir = Some(dir);
        s
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Clears the caches of every storage key of `site` (a host: it and its subdomains), or of
    /// every key. A `Cache` object of a cleared cache then fails its operations.
    pub fn clear_site(&self, site: Option<&str>) {
        let mut inner = self.lock();
        let matches = |key: &str| site.is_none_or(|s| key_in_site(key, s));
        let keys: Vec<String> = inner.keys.keys().filter(|k| matches(k)).cloned().collect();
        for key in &keys {
            inner.keys.remove(key);
        }
        inner.owner.retain(|_, key| !matches(key));
        let Some(dir) = inner.dir.clone() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(key) = name.to_str().and_then(key_of_folder) else {
                continue;
            };
            if matches(&key) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    /// The storage keys that have at least one cache (in memory or on disk), sorted.
    pub fn keys(&self) -> Vec<String> {
        let inner = self.lock();
        let mut keys: Vec<String> = inner
            .keys
            .iter()
            .filter(|(_, k)| !k.names.is_empty())
            .map(|(key, _)| key.clone())
            .collect();
        if let Some(dir) = &inner.dir {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    if let Some(key) = e.file_name().to_str().and_then(key_of_folder) {
                        if !inner.keys.contains_key(&key) && e.path().join("index").is_file() {
                            keys.push(key);
                        }
                    }
                }
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }
}

impl Inner {
    fn folder(&self, key: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(folder_name(key)))
    }

    /// The key's caches, loaded from its folder on first use.
    fn key_state(&mut self, key: &str) -> &mut KeyState {
        if !self.keys.contains_key(key) {
            let state = self.load(key);
            self.keys.insert(key.to_owned(), state);
        }
        self.keys.get_mut(key).expect("just inserted")
    }

    fn new_cache_id(&mut self, key: &str) -> CacheId {
        self.next_cache += 1;
        self.owner.insert(self.next_cache, key.to_owned());
        self.next_cache
    }

    /// Reads the key's index, and removes the body files it does not name.
    fn load(&mut self, key: &str) -> KeyState {
        let mut state = KeyState::default();
        let Some(folder) = self.folder(key) else {
            return state;
        };
        let index = std::fs::read(folder.join("index"))
            .ok()
            .and_then(|bytes| postcard::from_bytes::<Index>(&bytes).ok());
        let mut named = std::collections::HashSet::new();
        if let Some(index) = index {
            state.next_body = index.next_body;
            for (name, entries) in index.caches {
                let id = self.new_cache_id(key);
                let list = entries
                    .into_iter()
                    .map(|e| Entry {
                        request: e.request,
                        response: e.response,
                        body: match e.body {
                            Some(b) => {
                                named.insert(b);
                                Body::File(b)
                            }
                            None => Body::Null,
                        },
                    })
                    .collect();
                state.names.push((name, id));
                state.caches.insert(id, list);
            }
        }
        if let Ok(files) = std::fs::read_dir(&folder) {
            for f in files.flatten() {
                let name = f.file_name();
                let Some(id) = name
                    .to_str()
                    .and_then(|n| n.strip_suffix(".body"))
                    .and_then(|n| n.parse::<u64>().ok())
                else {
                    continue;
                };
                if !named.contains(&id) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        state
    }

    /// Writes the key's index (its named caches); `false` when the disk refused it.
    fn save(&self, key: &str) -> bool {
        let (Some(folder), Some(state)) = (self.folder(key), self.keys.get(key)) else {
            return true;
        };
        let index = Index {
            next_body: state.next_body,
            caches: state
                .names
                .iter()
                .map(|(name, id)| {
                    let entries = state
                        .caches
                        .get(id)
                        .map(|list| {
                            list.iter()
                                .map(|e| DiskEntry {
                                    request: e.request.clone(),
                                    response: e.response.clone(),
                                    body: match e.body {
                                        Body::File(b) => Some(b),
                                        _ => None,
                                    },
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (name.clone(), entries)
                })
                .collect(),
        };
        let Ok(bytes) = postcard::to_stdvec(&index) else {
            return false;
        };
        let tmp = folder.join("index.tmp");
        std::fs::create_dir_all(&folder).is_ok()
            && std::fs::write(&tmp, bytes).is_ok()
            && std::fs::rename(&tmp, folder.join("index")).is_ok()
    }

    fn read_body(&self, key: &str, body: &Body) -> Option<Vec<u8>> {
        match body {
            Body::Null => None,
            Body::Memory(bytes) => Some(bytes.to_vec()),
            Body::File(id) => Some(
                self.folder(key)
                    .and_then(|f| std::fs::read(body_path(&f, *id)).ok())
                    .unwrap_or_default(),
            ),
        }
    }

    fn response_of(&self, key: &str, e: &Entry) -> CachedResponse {
        let mut r = e.response.clone();
        r.body = self.read_body(key, &e.body);
        r
    }

    /// The cache `cache` of `key`, or `None` when the key has no such cache.
    fn cache(&mut self, key: &str, cache: CacheId) -> Option<&mut Vec<Entry>> {
        if self.owner.get(&cache).map(String::as_str) != Some(key) {
            return None;
        }
        self.key_state(key).caches.get_mut(&cache)
    }

    /// §5.4.7 "Query Cache" over a cache's list: the matching entries' indices.
    fn query(list: &[Entry], request: &CachedRequest, options: QueryOptions) -> Vec<usize> {
        list.iter()
            .enumerate()
            .filter(|(_, e)| request_matches(request, &e.request, Some(&e.response), options))
            .map(|(i, _)| i)
            .collect()
    }

    fn call(&mut self, key: &str, op: Op) -> Reply {
        match op {
            Op::Has { name } => {
                Reply::Bool(self.key_state(key).names.iter().any(|(n, _)| *n == name))
            }
            Op::Open { name } => {
                if let Some((_, id)) = self.key_state(key).names.iter().find(|(n, _)| *n == name) {
                    return Reply::Cache(*id);
                }
                let id = self.new_cache_id(key);
                let state = self.key_state(key);
                state.names.push((name, id));
                state.caches.insert(id, Vec::new());
                if !self.save(key) {
                    let state = self.key_state(key);
                    state.names.pop();
                    state.caches.remove(&id);
                    return Reply::Error(CacheError::QuotaExceeded);
                }
                Reply::Cache(id)
            }
            Op::Delete { name } => {
                let state = self.key_state(key);
                let Some(i) = state.names.iter().position(|(n, _)| *n == name) else {
                    return Reply::Bool(false);
                };
                state.names.remove(i);
                // (the cache itself stays for the Cache objects that represent it)
                self.save(key);
                Reply::Bool(true)
            }
            Op::Names => Reply::Names(
                self.key_state(key)
                    .names
                    .iter()
                    .map(|(n, _)| n.clone())
                    .collect(),
            ),
            Op::MatchAny {
                cache_name,
                request,
                options,
            } => {
                let state = self.key_state(key);
                let ids: Vec<CacheId> = state
                    .names
                    .iter()
                    .filter(|(n, _)| cache_name.as_ref().is_none_or(|c| c == n))
                    .map(|(_, id)| *id)
                    .collect();
                for id in ids {
                    let state = self.key_state(key);
                    let list = state.caches.get(&id).map(Vec::as_slice).unwrap_or_default();
                    if let Some(&i) = Self::query(list, &request, options).first() {
                        let e = list[i].clone();
                        return Reply::Responses(vec![self.response_of(key, &e)]);
                    }
                }
                Reply::Responses(Vec::new())
            }
            Op::Match {
                cache,
                request,
                options,
                first,
            } => {
                let Some(list) = self.cache(key, cache) else {
                    return Reply::Error(CacheError::UnknownCache);
                };
                let mut found: Vec<Entry> = match &request {
                    None => list.clone(),
                    Some(r) => Self::query(list, r, options)
                        .into_iter()
                        .map(|i| list[i].clone())
                        .collect(),
                };
                if first {
                    found.truncate(1);
                }
                Reply::Responses(found.iter().map(|e| self.response_of(key, e)).collect())
            }
            Op::Keys {
                cache,
                request,
                options,
            } => {
                let Some(list) = self.cache(key, cache) else {
                    return Reply::Error(CacheError::UnknownCache);
                };
                Reply::Requests(match &request {
                    None => list.iter().map(|e| e.request.clone()).collect(),
                    Some(r) => Self::query(list, r, options)
                        .into_iter()
                        .map(|i| list[i].request.clone())
                        .collect(),
                })
            }
            Op::Batch { cache, ops } => self.batch(key, cache, ops),
        }
    }

    /// §5.4.9 "Batch Cache Operations": every operation, or none of them.
    fn batch(&mut self, key: &str, cache: CacheId, ops: Vec<BatchOp>) -> Reply {
        let Some(list) = self.cache(key, cache) else {
            return Reply::Error(CacheError::UnknownCache);
        };
        let mut work = list.clone();
        let mut added: Vec<Entry> = Vec::new();
        let mut removed: Vec<Entry> = Vec::new();
        let mut counts = Vec::with_capacity(ops.len());
        for op in ops {
            let (request, options, put) = match op {
                BatchOp::Delete { request, options } => (request, options, None),
                BatchOp::Put { request, response } => {
                    (request, QueryOptions::default(), Some(response))
                }
            };
            if !Self::query(&added, &request, options).is_empty() {
                return Reply::Error(CacheError::InvalidState(
                    "The batch has two operations on the same request".into(),
                ));
            }
            if put.is_some() {
                let scheme = request.url.split(':').next().unwrap_or("");
                if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
                    return Reply::Error(CacheError::Type(
                        "Request scheme must be http or https".into(),
                    ));
                }
                if request.method != "GET" {
                    return Reply::Error(CacheError::Type("Request method must be GET".into()));
                }
            }
            let hits = Self::query(&work, &request, options);
            counts.push(hits.len() as u32);
            for &i in hits.iter().rev() {
                removed.push(work.remove(i));
            }
            if let Some(mut response) = put {
                let body = match response.body.take() {
                    Some(bytes) => Body::Memory(Arc::new(bytes)),
                    None => Body::Null,
                };
                let e = Entry {
                    request,
                    response,
                    body,
                };
                work.push(e.clone());
                added.push(e);
            }
        }
        // On disk, the new bodies go to files before the index names them.
        let folder = self.folder(key);
        let mut written: Vec<u64> = Vec::new();
        if let Some(folder) = &folder {
            if !added.is_empty() && std::fs::create_dir_all(folder).is_err() {
                return Reply::Error(CacheError::QuotaExceeded);
            }
            for e in work.iter_mut() {
                let Body::Memory(bytes) = &e.body else {
                    continue;
                };
                let state = self.key_state(key);
                let id = state.next_body;
                state.next_body += 1;
                if std::fs::write(body_path(folder, id), &**bytes).is_err() {
                    for id in written {
                        let _ = std::fs::remove_file(body_path(folder, id));
                    }
                    return Reply::Error(CacheError::QuotaExceeded);
                }
                written.push(id);
                e.body = Body::File(id);
            }
        }
        let old = std::mem::replace(
            self.key_state(key).caches.get_mut(&cache).expect("checked"),
            work,
        );
        let named = self.key_state(key).names.iter().any(|(_, id)| *id == cache);
        if named && !self.save(key) {
            *self.key_state(key).caches.get_mut(&cache).expect("checked") = old;
            if let Some(folder) = &folder {
                for id in written {
                    let _ = std::fs::remove_file(body_path(folder, id));
                }
            }
            return Reply::Error(CacheError::QuotaExceeded);
        }
        if let Some(folder) = &folder {
            for e in removed {
                if let Body::File(id) = e.body {
                    let _ = std::fs::remove_file(body_path(folder, id));
                }
            }
        }
        Reply::Batch(counts)
    }
}

impl CacheStorageService for LocalCacheStorage {
    fn call(&self, key: &str, op: Op) -> Reply {
        self.lock().call(key, op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(url: &str) -> CachedRequest {
        CachedRequest {
            method: "GET".into(),
            url: url.into(),
            ..Default::default()
        }
    }

    fn response(body: &str, headers: &[(&str, &str)]) -> CachedResponse {
        CachedResponse {
            type_: "basic".into(),
            status: 200,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: Some(body.as_bytes().to_vec()),
            ..Default::default()
        }
    }

    const KEY: &str = "https://example.com";

    fn open(s: &LocalCacheStorage, name: &str) -> CacheId {
        match s.call(KEY, Op::Open { name: name.into() }) {
            Reply::Cache(id) => id,
            r => panic!("{r:?}"),
        }
    }

    fn put(
        s: &LocalCacheStorage,
        cache: CacheId,
        req: CachedRequest,
        res: CachedResponse,
    ) -> Reply {
        s.call(
            KEY,
            Op::Batch {
                cache,
                ops: vec![BatchOp::Put {
                    request: req,
                    response: res,
                }],
            },
        )
    }

    fn matched(
        s: &LocalCacheStorage,
        cache: CacheId,
        req: CachedRequest,
        o: QueryOptions,
    ) -> Vec<String> {
        match s.call(
            KEY,
            Op::Match {
                cache,
                request: Some(req),
                options: o,
                first: false,
            },
        ) {
            Reply::Responses(r) => r
                .into_iter()
                .map(|r| String::from_utf8(r.body.unwrap_or_default()).unwrap())
                .collect(),
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn a_put_replaces_the_entry_of_the_same_url_and_keeps_order() {
        let s = LocalCacheStorage::in_memory();
        let c = open(&s, "v1");
        put(&s, c, get("https://example.com/a"), response("a1", &[]));
        put(&s, c, get("https://example.com/b"), response("b", &[]));
        put(
            &s,
            c,
            get("https://example.com/a#frag"),
            response("a2", &[]),
        );
        let keys = s.call(
            KEY,
            Op::Keys {
                cache: c,
                request: None,
                options: QueryOptions::default(),
            },
        );
        let Reply::Requests(keys) = keys else {
            panic!()
        };
        let urls: Vec<&str> = keys.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://example.com/b", "https://example.com/a#frag"]
        );
        assert_eq!(
            matched(&s, c, get("https://example.com/a"), QueryOptions::default()),
            ["a2"]
        );
    }

    #[test]
    fn search_method_and_vary_are_matched_unless_ignored() {
        let s = LocalCacheStorage::in_memory();
        let c = open(&s, "v1");
        let mut req = get("https://example.com/q?x=1");
        req.headers = vec![("Accept".into(), "text/html".into())];
        put(&s, c, req, response("q", &[("Vary", "Accept")]));
        let none = QueryOptions::default();
        assert!(matched(&s, c, get("https://example.com/q?x=1"), none).is_empty());
        let mut html = get("https://example.com/q?x=1");
        html.headers = vec![("accept".into(), "text/html".into())];
        assert_eq!(matched(&s, c, html.clone(), none), ["q"]);
        let ignore_vary = QueryOptions {
            ignore_vary: true,
            ..none
        };
        assert_eq!(
            matched(&s, c, get("https://example.com/q?x=1"), ignore_vary),
            ["q"]
        );
        let mut other_query = html.clone();
        other_query.url = "https://example.com/q?y=2".into();
        assert!(matched(&s, c, other_query.clone(), none).is_empty());
        let ignore_search = QueryOptions {
            ignore_search: true,
            ..none
        };
        assert_eq!(matched(&s, c, other_query, ignore_search), ["q"]);
        let mut post = html;
        post.method = "POST".into();
        assert!(matched(&s, c, post.clone(), none).is_empty());
        let ignore_method = QueryOptions {
            ignore_method: true,
            ..none
        };
        assert_eq!(matched(&s, c, post, ignore_method), ["q"]);
    }

    #[test]
    fn a_failed_batch_changes_nothing() {
        let s = LocalCacheStorage::in_memory();
        let c = open(&s, "v1");
        put(&s, c, get("https://example.com/a"), response("a", &[]));
        let r = s.call(
            KEY,
            Op::Batch {
                cache: c,
                ops: vec![
                    BatchOp::Delete {
                        request: get("https://example.com/a"),
                        options: QueryOptions::default(),
                    },
                    BatchOp::Put {
                        request: get("https://example.com/b"),
                        response: response("b", &[]),
                    },
                    BatchOp::Put {
                        request: get("https://example.com/b"),
                        response: response("b2", &[]),
                    },
                ],
            },
        );
        assert!(
            matches!(r, Reply::Error(CacheError::InvalidState(_))),
            "{r:?}"
        );
        assert_eq!(
            matched(&s, c, get("https://example.com/a"), QueryOptions::default()),
            ["a"]
        );
        assert!(matched(&s, c, get("https://example.com/b"), QueryOptions::default()).is_empty());
    }

    #[test]
    fn a_deleted_cache_keeps_working_for_its_holders() {
        let s = LocalCacheStorage::in_memory();
        let c = open(&s, "v1");
        assert_eq!(
            s.call(KEY, Op::Delete { name: "v1".into() }),
            Reply::Bool(true)
        );
        assert_eq!(
            s.call(KEY, Op::Has { name: "v1".into() }),
            Reply::Bool(false)
        );
        put(&s, c, get("https://example.com/a"), response("a", &[]));
        assert_eq!(
            matched(&s, c, get("https://example.com/a"), QueryOptions::default()),
            ["a"]
        );
        assert_ne!(open(&s, "v1"), c);
    }

    #[test]
    fn another_keys_cache_is_unknown() {
        let s = LocalCacheStorage::in_memory();
        let c = open(&s, "v1");
        let r = s.call(
            "https://other.example",
            Op::Keys {
                cache: c,
                request: None,
                options: QueryOptions::default(),
            },
        );
        assert_eq!(r, Reply::Error(CacheError::UnknownCache));
    }

    #[test]
    fn caches_persist_and_clear_by_site() {
        let dir = std::env::temp_dir().join(format!("snowlark-sw-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let s = LocalCacheStorage::persistent(dir.clone());
            let a = open(&s, "a");
            open(&s, "b");
            put(&s, a, get("https://example.com/x"), response("x", &[]));
            put(&s, a, get("https://example.com/x"), response("x2", &[]));
            put(&s, a, get("https://example.com/y"), response("", &[]));
            assert_eq!(
                s.call(KEY, Op::Delete { name: "b".into() }),
                Reply::Bool(true)
            );
            s.call("https://other.example", Op::Open { name: "o".into() });
        }
        let s = LocalCacheStorage::persistent(dir.clone());
        assert_eq!(s.call(KEY, Op::Names), Reply::Names(vec!["a".into()]));
        let a = open(&s, "a");
        assert_eq!(
            matched(&s, a, get("https://example.com/x"), QueryOptions::default()),
            ["x2"]
        );
        let bodies = std::fs::read_dir(dir.join(folder_name(KEY)))
            .unwrap()
            .filter(|f| {
                f.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|e| e == "body")
            })
            .count();
        assert_eq!(bodies, 2, "the replaced body's file is gone");
        assert_eq!(s.keys(), ["https://example.com", "https://other.example"]);
        s.clear_site(Some("example.com"));
        assert_eq!(s.keys(), ["https://other.example"]);
        assert_eq!(s.call(KEY, Op::Names), Reply::Names(vec![]));
        let r = s.call(
            KEY,
            Op::Keys {
                cache: a,
                request: None,
                options: QueryOptions::default(),
            },
        );
        assert_eq!(r, Reply::Error(CacheError::UnknownCache));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_names_round_trip() {
        for key in ["https://example.com:8443", "file://", "http://[::1]:80"] {
            assert_eq!(key_of_folder(&folder_name(key)).as_deref(), Some(key));
        }
        assert!(key_in_site("https://www.example.com", "example.com"));
        assert!(!key_in_site("https://notexample.com", "example.com"));
    }
}
