// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The HTTP cache (RFC 9111, a private cache; Fetch §4.6 "HTTP-network-or-cache fetch" uses it).
//!
//! In memory, per network thread. Responses to GET are stored by network partition (the top-level
//! origin) and URL, with the request header values their `Vary` names; a stored response is used
//! while fresh, validated with a conditional request once stale (`ETag`, `Last-Modified`), and
//! refreshed by a 304. Not stored: `no-store` (request or response), requests with `Authorization`,
//! `Vary: *`, responses that set cookies, partial content, and responses without explicit
//! freshness, a validator or a heuristically cacheable status. Bodies are kept as delivered
//! (content codings decoded). The least recently used entries go past a total size.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use crate::cookies::parse_cookie_date;
use crate::headers::HeaderList;

/// What the cache holds at most, in body bytes.
const CAPACITY: usize = 256 << 20;
/// The largest body stored.
pub(crate) const MAX_ENTRY: usize = 32 << 20;

/// RFC 9111 §4.2.2: the statuses a response may be heuristically fresh with (those "defined as
/// heuristically cacheable" by RFC 9110 §15.1).
const HEURISTIC: [u16; 11] = [200, 203, 204, 206, 300, 301, 308, 404, 405, 410, 501];

/// A cache key: the network partition and the URL (without fragment).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub partition: String,
    pub url: String,
}

/// A stored response.
#[derive(Clone)]
pub(crate) struct Entry {
    pub status: u16,
    pub reason: String,
    pub headers: HeaderList,
    pub body: Bytes,
    /// The `Vary` header fields with the request's values for them.
    vary: Vec<(String, Option<String>)>,
    /// When the request was sent and the response received (ms since the epoch).
    request_time: i64,
    response_time: i64,
    last_used: u64,
    /// When it was stored or last used (ms since the epoch).
    used_at: i64,
}

/// A response to store, with when its request was sent and it was received (ms since the epoch).
pub(crate) struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: HeaderList,
    pub body: Bytes,
    pub request_time: i64,
    pub response_time: i64,
}

/// What a lookup found.
pub(crate) enum Lookup {
    /// Nothing usable stored.
    Miss,
    /// A stored response to use as is.
    Fresh(Entry),
    /// A stored response to validate first: the conditional request headers to send.
    Stale(Entry, Vec<(&'static str, String)>),
}

#[derive(Default)]
struct Inner {
    entries: HashMap<Key, Entry>,
    size: usize,
    clock: u64,
}

#[derive(Default)]
pub(crate) struct HttpCache {
    inner: Mutex<Inner>,
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A Cache-Control header's directives, lowercased, with their values unquoted.
fn directives(headers: &HeaderList) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for value in headers.get_all("cache-control") {
        for part in value.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (name, v) = match part.split_once('=') {
                Some((n, v)) => (n.trim(), Some(v.trim().trim_matches('"').to_owned())),
                None => (part, None),
            };
            out.push((name.to_ascii_lowercase(), v));
        }
    }
    out
}

fn has(d: &[(String, Option<String>)], name: &str) -> bool {
    d.iter().any(|(n, _)| n == name)
}

fn seconds(d: &[(String, Option<String>)], name: &str) -> Option<i64> {
    d.iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, v)| v.as_deref()?.parse::<i64>().ok())
        .map(|s| s.max(0))
}

/// RFC 9111 §4.2.1: the freshness lifetime in ms (none: neither explicit nor heuristic).
fn freshness_lifetime(status: u16, headers: &HeaderList) -> Option<i64> {
    let d = directives(headers);
    if let Some(s) = seconds(&d, "max-age") {
        return Some(s * 1000);
    }
    let date = headers.get("date").and_then(|v| parse_cookie_date(&v));
    if let Some(expires) = headers.get("expires") {
        // (an invalid Expires is a time in the past)
        return Some(match (parse_cookie_date(&expires), date) {
            (Some(e), Some(d)) => (e - d).max(0),
            (Some(e), None) => (e - now_ms()).max(0),
            _ => 0,
        });
    }
    // §4.2.2: 10% of the time since Last-Modified, for the statuses that allow it.
    if HEURISTIC.contains(&status) || has(&d, "public") {
        let lm = headers
            .get("last-modified")
            .and_then(|v| parse_cookie_date(&v))?;
        let base = date.unwrap_or_else(now_ms);
        return Some(((base - lm).max(0)) / 10);
    }
    None
}

impl Entry {
    /// RFC 9111 §4.2.3: the response's current age in ms.
    fn age(&self, now: i64) -> i64 {
        let date = self
            .headers
            .get("date")
            .and_then(|v| parse_cookie_date(&v))
            .unwrap_or(self.response_time);
        let age_value = self
            .headers
            .get("age")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0)
            * 1000;
        let apparent_age = (self.response_time - date).max(0);
        let response_delay = self.response_time - self.request_time;
        let corrected_initial_age = apparent_age.max(age_value + response_delay);
        corrected_initial_age + (now - self.response_time).max(0)
    }

    /// Whether the stored response may be used without validation (RFC 9111 §4.2, with the
    /// request's own directives, §5.2.1).
    fn fresh(&self, request: &HeaderList, now: i64) -> bool {
        let response = directives(&self.headers);
        if has(&response, "no-cache") {
            return false;
        }
        let req = directives(request);
        if has(&req, "no-cache")
            || (req.is_empty()
                && request
                    .get("pragma")
                    .is_some_and(|p| p.to_ascii_lowercase().contains("no-cache")))
        {
            return false;
        }
        let Some(lifetime) = freshness_lifetime(self.status, &self.headers) else {
            return false;
        };
        let age = self.age(now);
        if let Some(max_age) = seconds(&req, "max-age") {
            if age > max_age * 1000 {
                return false;
            }
        }
        if let Some(min_fresh) = seconds(&req, "min-fresh") {
            if lifetime - age < min_fresh * 1000 {
                return false;
            }
        }
        lifetime > age
    }

    /// RFC 9111 §4.3.1: the conditional request headers for the stored response's validators.
    fn validators(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(etag) = self.headers.get("etag") {
            out.push(("If-None-Match", etag));
        }
        if let Some(lm) = self.headers.get("last-modified") {
            out.push(("If-Modified-Since", lm));
        }
        out
    }

    /// Whether the request matches the stored response's Vary (RFC 9111 §4.1).
    fn matches(&self, request: &HeaderList) -> bool {
        self.vary
            .iter()
            .all(|(name, value)| request.get(name).as_deref().map(str::trim) == value.as_deref())
    }
}

/// Whether a response may be stored (RFC 9111 §3), given the request's and the response's headers.
pub(crate) fn storable(
    method: &str,
    request: &HeaderList,
    status: u16,
    headers: &HeaderList,
) -> bool {
    if method != "GET" || !(200..600).contains(&status) || status == 206 || status == 304 {
        return false;
    }
    let req = directives(request);
    let res = directives(headers);
    if has(&req, "no-store") || has(&res, "no-store") {
        return false;
    }
    if request.contains("authorization") || request.contains("range") {
        return false;
    }
    if headers.contains("set-cookie") {
        return false;
    }
    if headers
        .get_all("vary")
        .iter()
        .any(|v| v.split(',').any(|f| f.trim() == "*"))
    {
        return false;
    }
    freshness_lifetime(status, headers).is_some()
        || headers.contains("etag")
        || headers.contains("last-modified")
}

impl HttpCache {
    /// The stored response for a request to `key` with `request` headers, by `mode` (Fetch's
    /// cache modes "default", "no-cache", "force-cache", "only-if-cached"; the others never look).
    pub(crate) fn lookup(&self, key: &Key, request: &HeaderList, any_age: bool) -> Lookup {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.clock += 1;
        let clock = inner.clock;
        let Some(entry) = inner.entries.get_mut(key) else {
            return Lookup::Miss;
        };
        if !entry.matches(request) {
            return Lookup::Miss;
        }
        entry.last_used = clock;
        entry.used_at = now_ms();
        let entry = entry.clone();
        if any_age || entry.fresh(request, now_ms()) {
            return Lookup::Fresh(entry);
        }
        let validators = entry.validators();
        if validators.is_empty() {
            return Lookup::Miss;
        }
        Lookup::Stale(entry, validators)
    }

    /// Removes the entries stored or used from `begin` (inclusive) to `end` (exclusive, ms since
    /// the epoch), as clearing the cached files of a time range does.
    pub(crate) fn clear_between(&self, begin: i64, end: i64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut freed = 0;
        inner.entries.retain(|_, e| {
            let gone = (begin..end).contains(&e.used_at);
            if gone {
                freed += e.body.len();
            }
            !gone
        });
        inner.size -= freed;
    }

    /// Stores a response (the caller checked [`storable`]).
    pub(crate) fn store(&self, key: Key, request: &HeaderList, response: Response) {
        let Response {
            status,
            reason,
            headers,
            body,
            request_time,
            response_time,
        } = response;
        if body.len() > MAX_ENTRY {
            return;
        }
        let vary = headers
            .get_all("vary")
            .iter()
            .flat_map(|v| v.split(','))
            .map(|f| f.trim().to_ascii_lowercase())
            .filter(|f| !f.is_empty())
            .map(|f| {
                let value = request.get(&f).map(|v| v.trim().to_owned());
                (f, value)
            })
            .collect();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.clock += 1;
        let entry = Entry {
            status,
            reason,
            headers,
            body,
            vary,
            request_time,
            response_time,
            last_used: inner.clock,
            used_at: now_ms(),
        };
        let size = entry.body.len();
        if let Some(old) = inner.entries.insert(key, entry) {
            inner.size -= old.body.len();
        }
        inner.size += size;
        // The least recently used go first.
        while inner.size > CAPACITY {
            let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(e) = inner.entries.remove(&oldest) {
                inner.size -= e.body.len();
            }
        }
    }

    /// RFC 9111 §4.3.4: a 304 refreshes the stored response's headers (all but the framing ones)
    /// and its times. Returns the refreshed response.
    pub(crate) fn refresh(
        &self,
        key: &Key,
        stored: Entry,
        not_modified: &HeaderList,
        request_time: i64,
        response_time: i64,
    ) -> Entry {
        let mut entry = stored;
        entry.used_at = now_ms();
        for (name, _) in not_modified.iter() {
            let lower = name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "content-length" | "content-encoding" | "transfer-encoding" | "content-range"
            ) {
                continue;
            }
            entry.headers.delete(&lower);
        }
        for (name, value) in not_modified.iter() {
            let lower = name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "content-length" | "content-encoding" | "transfer-encoding" | "content-range"
            ) {
                continue;
            }
            entry.headers.append(name, value);
        }
        entry.request_time = request_time;
        entry.response_time = response_time;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = inner.entries.get_mut(key) {
            e.headers = entry.headers.clone();
            e.request_time = request_time;
            e.response_time = response_time;
        }
        entry
    }

    /// RFC 9111 §4.4: an unsafe request's success invalidates what is stored for its URL.
    pub(crate) fn invalidate(&self, url: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let gone: Vec<Key> = inner
            .entries
            .keys()
            .filter(|k| k.url == url)
            .cloned()
            .collect();
        for k in gone {
            if let Some(e) = inner.entries.remove(&k) {
                inner.size -= e.body.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderList {
        let mut h = HeaderList::new();
        for (n, v) in pairs {
            h.append(n, v);
        }
        h
    }

    fn key(url: &str) -> Key {
        Key {
            partition: "https://top.test".into(),
            url: url.into(),
        }
    }

    fn put(cache: &HttpCache, url: &str, response: &HeaderList, request: &HeaderList, age_ms: i64) {
        let now = now_ms();
        cache.store(
            key(url),
            request,
            Response {
                status: 200,
                reason: "OK".into(),
                headers: response.clone(),
                body: Bytes::from_static(b"body"),
                request_time: now - age_ms,
                response_time: now - age_ms,
            },
        );
    }

    #[test]
    fn a_time_range_clears_the_entries_used_in_it() {
        let cache = HttpCache::default();
        let fresh = headers(&[("Cache-Control", "max-age=600")]);
        let none = HeaderList::new();
        put(&cache, "https://a.test/old", &fresh, &none, 0);
        put(&cache, "https://a.test/new", &fresh, &none, 0);
        let hour = 3_600_000;
        {
            let mut inner = cache.inner.lock().unwrap();
            inner
                .entries
                .get_mut(&key("https://a.test/old"))
                .unwrap()
                .used_at -= 2 * hour;
        }
        cache.clear_between(now_ms() - hour, i64::MAX);
        assert!(matches!(
            cache.lookup(&key("https://a.test/old"), &none, false),
            Lookup::Fresh(_)
        ));
        assert!(matches!(
            cache.lookup(&key("https://a.test/new"), &none, false),
            Lookup::Miss
        ));
        assert_eq!(cache.inner.lock().unwrap().size, 4);
        // A lookup is a use: the old entry is now in the last hour.
        cache.clear_between(now_ms() - hour, i64::MAX);
        assert_eq!(cache.inner.lock().unwrap().entries.len(), 0);
        assert_eq!(cache.inner.lock().unwrap().size, 0);
    }

    #[test]
    fn what_is_stored() {
        let none = HeaderList::new();
        let fresh = headers(&[("Cache-Control", "max-age=60")]);
        assert!(storable("GET", &none, 200, &fresh));
        assert!(!storable("POST", &none, 200, &fresh));
        assert!(!storable("GET", &none, 206, &fresh));
        assert!(!storable(
            "GET",
            &none,
            200,
            &headers(&[("Cache-Control", "no-store, max-age=60")])
        ));
        assert!(!storable(
            "GET",
            &headers(&[("Cache-Control", "no-store")]),
            200,
            &fresh
        ));
        assert!(!storable(
            "GET",
            &headers(&[("Authorization", "x")]),
            200,
            &fresh
        ));
        assert!(!storable(
            "GET",
            &none,
            200,
            &headers(&[("Cache-Control", "max-age=60"), ("Vary", "*")])
        ));
        assert!(!storable(
            "GET",
            &none,
            200,
            &headers(&[("Cache-Control", "max-age=60"), ("Set-Cookie", "a=b")])
        ));
        // Neither freshness nor a validator: nothing to reuse it with.
        assert!(!storable(
            "GET",
            &none,
            200,
            &headers(&[("Content-Type", "text/plain")])
        ));
        assert!(storable("GET", &none, 200, &headers(&[("ETag", "\"v1\"")])));
    }

    #[test]
    fn freshness_and_validation() {
        let cache = HttpCache::default();
        let req = HeaderList::new();
        put(
            &cache,
            "https://a.test/fresh",
            &headers(&[("Cache-Control", "max-age=60")]),
            &req,
            1000,
        );
        assert!(matches!(
            cache.lookup(&key("https://a.test/fresh"), &req, false),
            Lookup::Fresh(_)
        ));
        // Stale, with a validator: a conditional request.
        put(
            &cache,
            "https://a.test/stale",
            &headers(&[("Cache-Control", "max-age=1"), ("ETag", "\"v1\"")]),
            &req,
            5000,
        );
        match cache.lookup(&key("https://a.test/stale"), &req, false) {
            Lookup::Stale(_, v) => assert_eq!(v, vec![("If-None-Match", "\"v1\"".to_owned())]),
            _ => panic!("stale"),
        }
        // force-cache uses it anyway.
        assert!(matches!(
            cache.lookup(&key("https://a.test/stale"), &req, true),
            Lookup::Fresh(_)
        ));
        // The request's no-cache and max-age=0 validate.
        assert!(matches!(
            cache.lookup(
                &key("https://a.test/fresh"),
                &headers(&[("Cache-Control", "max-age=0")]),
                false
            ),
            Lookup::Miss | Lookup::Stale(..)
        ));
        // Heuristic freshness: 10% of the time since Last-Modified.
        let date = "Wed, 09 Jun 2021 10:18:14 GMT";
        let lm = "Mon, 01 Jun 2020 00:00:00 GMT";
        let h = headers(&[("Date", date), ("Last-Modified", lm)]);
        assert!(freshness_lifetime(200, &h).unwrap() > 30 * 24 * 3600 * 1000);
        assert_eq!(freshness_lifetime(302, &h), None);
        // An invalid Expires is in the past.
        assert_eq!(
            freshness_lifetime(200, &headers(&[("Expires", "0")])),
            Some(0)
        );
    }

    #[test]
    fn vary_partition_refresh_and_invalidation() {
        let cache = HttpCache::default();
        let br = headers(&[("Accept-Encoding", "br")]);
        put(
            &cache,
            "https://a.test/v",
            &headers(&[("Cache-Control", "max-age=60"), ("Vary", "Accept-Encoding")]),
            &br,
            0,
        );
        assert!(matches!(
            cache.lookup(&key("https://a.test/v"), &br, false),
            Lookup::Fresh(_)
        ));
        assert!(matches!(
            cache.lookup(
                &key("https://a.test/v"),
                &headers(&[("Accept-Encoding", "gzip")]),
                false
            ),
            Lookup::Miss
        ));
        // Another partition does not see it.
        let other = Key {
            partition: "https://other.test".into(),
            url: "https://a.test/v".into(),
        };
        assert!(matches!(cache.lookup(&other, &br, false), Lookup::Miss));
        // A 304 refreshes the headers and the times.
        put(
            &cache,
            "https://a.test/r",
            &headers(&[("Cache-Control", "max-age=1"), ("ETag", "\"1\"")]),
            &HeaderList::new(),
            5000,
        );
        let Lookup::Stale(stored, _) =
            cache.lookup(&key("https://a.test/r"), &HeaderList::new(), false)
        else {
            panic!("stale");
        };
        let now = now_ms();
        let refreshed = cache.refresh(
            &key("https://a.test/r"),
            stored,
            &headers(&[("Cache-Control", "max-age=60"), ("ETag", "\"1\"")]),
            now,
            now,
        );
        assert_eq!(
            refreshed.headers.get("cache-control").unwrap(),
            "max-age=60"
        );
        assert!(matches!(
            cache.lookup(&key("https://a.test/r"), &HeaderList::new(), false),
            Lookup::Fresh(_)
        ));
        cache.invalidate("https://a.test/r");
        assert!(matches!(
            cache.lookup(&key("https://a.test/r"), &HeaderList::new(), false),
            Lookup::Miss
        ));
    }
}
