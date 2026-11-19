// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP Strict Transport Security (RFC 6797): parsing `Strict-Transport-Security` (§6.1), noting known
//! HSTS hosts from responses received over secure transport (§8.1), matching them (§8.2), the URL upgrade
//! (§8.3), and persistence to `hsts.json` in the profile folder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::cookies::{now_ms, write_atomically};
use crate::headers::HeaderList;

/// A parsed STS header value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StsDirectives {
    pub max_age: u64,
    pub include_subdomains: bool,
}

/// §6.1 the STS header grammar: `directive *( ";" directive )`, names case-insensitive, values tokens or
/// quoted strings. A repeated directive or a missing/invalid max-age makes the whole header invalid.
pub fn parse_sts(value: &str) -> Option<StsDirectives> {
    let mut max_age = None;
    let mut include_subdomains = false;
    let mut seen: Vec<String> = Vec::new();
    for directive in value.split(';') {
        let directive = directive.trim_matches(|c| c == ' ' || c == '\t');
        if directive.is_empty() {
            continue;
        }
        let (name, val) = match directive.find('=') {
            Some(i) => (
                directive[..i].trim_matches(|c| c == ' ' || c == '\t'),
                Some(directive[i + 1..].trim_matches(|c| c == ' ' || c == '\t')),
            ),
            None => (directive, None),
        };
        let lower = name.to_ascii_lowercase();
        if !crate::mime::is_token(&lower) || seen.contains(&lower) {
            return None;
        }
        seen.push(lower.clone());
        let val = val.map(|v| {
            if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
                v[1..v.len() - 1].to_string()
            } else {
                v.to_string()
            }
        });
        match lower.as_str() {
            "max-age" => {
                let v = val?;
                if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                max_age = Some(v.parse::<u64>().unwrap_or(u64::MAX));
            }
            "includesubdomains" => {
                if val.is_some() {
                    return None;
                }
                include_subdomains = true;
            }
            _ => {}
        }
    }
    Some(StsDirectives {
        max_age: max_age?,
        include_subdomains,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    expires: i64,
    include_subdomains: bool,
}

#[derive(Default)]
struct Hosts {
    map: HashMap<String, Entry>,
    dirty: bool,
}

/// The known HSTS hosts.
pub struct HstsStore {
    hosts: Mutex<Hosts>,
    file: Option<PathBuf>,
}

impl Default for HstsStore {
    fn default() -> Self {
        HstsStore::in_memory()
    }
}

impl std::fmt::Debug for HstsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HstsStore")
            .field("file", &self.file)
            .finish()
    }
}

/// The longest max-age honoured: two years (browsers cap it; it keeps a typo from pinning a host forever).
const MAX_AGE_CAP_SECS: u64 = 2 * 365 * 24 * 3600;

impl HstsStore {
    pub fn in_memory() -> Self {
        HstsStore {
            hosts: Mutex::new(Hosts::default()),
            file: None,
        }
    }

    /// A store persisted to `file`; a damaged file starts it empty.
    pub fn open(file: &Path) -> Self {
        let mut hosts = Hosts::default();
        if let Ok(text) = std::fs::read_to_string(file) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, Entry>>(&text) {
                let now = now_ms();
                hosts.map = map.into_iter().filter(|(_, e)| e.expires > now).collect();
            }
        }
        HstsStore {
            hosts: Mutex::new(hosts),
            file: Some(file.to_path_buf()),
        }
    }

    /// §8.1: note the `Strict-Transport-Security` header of a response to `url` received over a secure
    /// transport without certificate errors. Only the first STS header counts; IP-literal hosts are ignored.
    pub fn observe(&self, url: &Url, headers: &HeaderList) {
        self.observe_at(url, headers, now_ms())
    }

    pub fn observe_at(&self, url: &Url, headers: &HeaderList, now: i64) {
        if url.scheme() != "https" {
            return;
        }
        let Some(Host::Domain(host)) = url.host() else {
            return;
        };
        let Some(first) = headers
            .get_all("strict-transport-security")
            .into_iter()
            .next()
        else {
            return;
        };
        let Some(sts) = parse_sts(first) else {
            return;
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let mut hosts = self.hosts.lock().unwrap();
        if sts.max_age == 0 {
            if hosts.map.remove(&host).is_some() {
                hosts.dirty = true;
            }
            return;
        }
        let expires = now + (sts.max_age.min(MAX_AGE_CAP_SECS) as i64) * 1000;
        let changed = match hosts.map.get(&host) {
            // (rewriting the file for every response would be wasteful: only a day's drift or a flag change)
            Some(old) => {
                old.include_subdomains != sts.include_subdomains
                    || expires - old.expires > 24 * 3600 * 1000
            }
            None => true,
        };
        hosts.map.insert(
            host,
            Entry {
                expires,
                include_subdomains: sts.include_subdomains,
            },
        );
        hosts.dirty |= changed;
    }

    /// §8.2 "known HSTS host": a congruent match, or a superdomain match with includeSubDomains.
    pub fn is_known_host(&self, host: &str) -> bool {
        self.is_known_host_at(host, now_ms())
    }

    pub fn is_known_host_at(&self, host: &str, now: i64) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let hosts = self.hosts.lock().unwrap();
        if hosts.map.get(&host).is_some_and(|e| e.expires > now) {
            return true;
        }
        let mut rest = host.as_str();
        while let Some(i) = rest.find('.') {
            rest = &rest[i + 1..];
            if hosts
                .map
                .get(rest)
                .is_some_and(|e| e.include_subdomains && e.expires > now)
            {
                return true;
            }
        }
        false
    }

    /// Fetch main fetch step 8 / RFC 6797 §8.3: an http (or ws) URL to a known HSTS host, as https (wss).
    /// An explicit port 80 becomes 443; other explicit ports stay. `None` when no upgrade applies.
    pub fn upgrade(&self, url: &Url) -> Option<Url> {
        let secure_scheme = match url.scheme() {
            "http" => "https",
            "ws" => "wss",
            _ => return None,
        };
        let Some(Host::Domain(host)) = url.host() else {
            return None;
        };
        if !self.is_known_host(host) {
            return None;
        }
        Some(upgrade_scheme(url, secure_scheme))
    }

    /// Forget a host (the site information panel).
    pub fn remove(&self, host: &str) {
        let mut hosts = self.hosts.lock().unwrap();
        if hosts.map.remove(&host.to_ascii_lowercase()).is_some() {
            hosts.dirty = true;
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.hosts.lock().unwrap().dirty
    }

    /// Write the store if it changed.
    pub fn flush(&self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let json = {
            let mut hosts = self.hosts.lock().unwrap();
            if !hosts.dirty {
                return Ok(());
            }
            hosts.dirty = false;
            serde_json::to_string(&hosts.map).map_err(std::io::Error::other)?
        };
        write_atomically(file, json.as_bytes())
    }
}

/// `url` with its scheme changed to the secure one, the default port following the scheme.
pub(crate) fn upgrade_scheme(url: &Url, secure_scheme: &str) -> Url {
    let explicit_port = url.port();
    let mut text = String::from(secure_scheme);
    text.push_str(&url.as_str()[url.scheme().len()..]);
    let mut upgraded = Url::parse(&text).unwrap_or_else(|_| url.clone());
    if explicit_port == Some(80) {
        let _ = upgraded.set_port(None);
    }
    upgraded
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    fn headers(v: &str) -> HeaderList {
        let mut h = HeaderList::new();
        h.append("Strict-Transport-Security", v);
        h
    }

    #[test]
    fn parsing() {
        assert_eq!(
            parse_sts("max-age=31536000; includeSubDomains"),
            Some(StsDirectives {
                max_age: 31536000,
                include_subdomains: true
            })
        );
        assert_eq!(
            parse_sts("Max-Age=\"10\""),
            Some(StsDirectives {
                max_age: 10,
                include_subdomains: false
            })
        );
        assert_eq!(parse_sts("includeSubDomains"), None);
        assert_eq!(parse_sts("max-age=1; max-age=2"), None);
        assert_eq!(parse_sts("max-age=-1"), None);
        assert_eq!(parse_sts("max-age=abc"), None);
        assert_eq!(
            parse_sts("max-age=5; preload; foo=bar").map(|d| d.max_age),
            Some(5)
        );
        assert_eq!(parse_sts("max-age=5; includeSubDomains=1"), None);
    }

    #[test]
    fn observe_and_match() {
        let store = HstsStore::in_memory();
        let url = Url::parse("https://example.com/").unwrap();
        store.observe_at(&url, &headers("max-age=100; includeSubDomains"), NOW);
        assert!(store.is_known_host_at("example.com", NOW));
        assert!(store.is_known_host_at("a.b.EXAMPLE.com", NOW));
        assert!(!store.is_known_host_at("notexample.com", NOW));
        assert!(!store.is_known_host_at("example.com", NOW + 101_000));

        let store = HstsStore::in_memory();
        store.observe_at(&url, &headers("max-age=100"), NOW);
        assert!(!store.is_known_host_at("www.example.com", NOW));
        // max-age=0 removes
        store.observe_at(&url, &headers("max-age=0"), NOW);
        assert!(!store.is_known_host_at("example.com", NOW));
        // over http, and for IP addresses, the header is ignored
        store.observe_at(
            &Url::parse("http://example.com/").unwrap(),
            &headers("max-age=100"),
            NOW,
        );
        store.observe_at(
            &Url::parse("https://127.0.0.1/").unwrap(),
            &headers("max-age=100"),
            NOW,
        );
        assert!(!store.is_known_host_at("example.com", NOW));
        assert!(!store.is_known_host_at("127.0.0.1", NOW));
    }

    #[test]
    fn upgrade() {
        let store = HstsStore::in_memory();
        store.observe(
            &Url::parse("https://example.com/").unwrap(),
            &headers("max-age=1000"),
        );
        let up = |s: &str| {
            store
                .upgrade(&Url::parse(s).unwrap())
                .map(|u| u.to_string())
        };
        assert_eq!(
            up("http://example.com/a?b#c").as_deref(),
            Some("https://example.com/a?b#c")
        );
        assert_eq!(
            up("http://example.com:80/").as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(
            up("http://example.com:8080/").as_deref(),
            Some("https://example.com:8080/")
        );
        assert_eq!(
            up("ws://example.com/s").as_deref(),
            Some("wss://example.com/s")
        );
        assert_eq!(up("https://example.com/"), None);
        assert_eq!(up("http://other.com/"), None);
    }

    #[test]
    fn persistence() {
        let dir = std::env::temp_dir().join(format!("snowlark-hsts-{}", std::process::id()));
        let file = dir.join("hsts.json");
        {
            let store = HstsStore::open(&file);
            store.observe(
                &Url::parse("https://example.com/").unwrap(),
                &headers("max-age=1000; includeSubDomains"),
            );
            assert!(store.is_dirty());
            store.flush().unwrap();
        }
        let store = HstsStore::open(&file);
        assert!(store.is_known_host("www.example.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
