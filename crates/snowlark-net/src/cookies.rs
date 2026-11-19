// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cookies per RFC 6265bis (draft-ietf-httpbis-rfc6265bis): `Set-Cookie` parsing (§5.6), the storage
//! model (§5.7) with Domain/Path/Secure/HttpOnly/SameSite, the `__Secure-`/`__Host-` prefixes and the public
//! suffix check, retrieval (§5.8.3), and persistence of persistent cookies to the profile folder as JSON
//! with batched saves.
//!
//! The store is shared: the network thread uses the HTTP API, page threads `document.cookie` (the
//! non-HTTP API). All times are milliseconds since the Unix epoch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::origin::{is_public_suffix, is_url_potentially_trustworthy, registrable_domain};

/// The longest a cookie may live: 400 days (§5.6.1, §5.6.2).
const MAX_AGE_LIMIT_MS: i64 = 400 * 24 * 3600 * 1000;
const PER_DOMAIN_LIMIT: usize = 180;
const TOTAL_LIMIT: usize = 3000;

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The SameSite attribute ("Default" when absent or unrecognised, treated as Lax on retrieval).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SameSite {
    None,
    Lax,
    Strict,
    Default,
}

/// A stored cookie (§5.7 storage model fields).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// i64::MAX for a session cookie.
    pub expiry: i64,
    pub persistent: bool,
    pub host_only: bool,
    pub secure_only: bool,
    pub http_only: bool,
    pub same_site: SameSite,
    /// Creation time in microseconds, unique within the store (it orders cookies of equal path length).
    pub creation: i64,
    pub last_access: i64,
}

/// A parsed `Set-Cookie` (§5.6), before the storage model decides about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetCookie {
    pub name: String,
    pub value: String,
    pub expires: Option<i64>,
    pub max_age: Option<i64>,
    pub domain: Option<String>,
    pub path: Option<String>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<SameSite>,
}

fn is_wsp(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn has_ctl(s: &str) -> bool {
    s.chars()
        .any(|c| (c <= '\x08') || ('\x0A'..='\x1F').contains(&c) || c == '\x7F')
}

impl SetCookie {
    /// §5.6 "parse a Set-Cookie header", at time `now`.
    pub fn parse(header: &str, now: i64) -> Option<SetCookie> {
        if has_ctl(header) {
            return None;
        }
        let (pair, mut unparsed) = match header.find(';') {
            Some(i) => (&header[..i], &header[i..]),
            None => (header, ""),
        };
        let (name, value) = match pair.find('=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => ("", pair),
        };
        let name = name.trim_matches(is_wsp);
        let value = value.trim_matches(is_wsp);
        if name.len() + value.len() > 4096 || (name.is_empty() && value.is_empty()) {
            return None;
        }
        let mut cookie = SetCookie {
            name: name.to_string(),
            value: value.to_string(),
            ..SetCookie::default()
        };
        while !unparsed.is_empty() {
            unparsed = &unparsed[1..]; // the ';'
            let av = match unparsed.find(';') {
                Some(i) => {
                    let av = &unparsed[..i];
                    unparsed = &unparsed[i..];
                    av
                }
                None => std::mem::take(&mut unparsed),
            };
            let (attr_name, attr_value) = match av.find('=') {
                Some(i) => (&av[..i], &av[i + 1..]),
                None => (av, ""),
            };
            let attr_name = attr_name.trim_matches(is_wsp);
            let attr_value = attr_value.trim_matches(is_wsp);
            if attr_value.len() > 1024 {
                continue;
            }
            match attr_name.to_ascii_lowercase().as_str() {
                "expires" => {
                    if let Some(t) = parse_cookie_date(attr_value) {
                        cookie.expires = Some(t.min(now.saturating_add(MAX_AGE_LIMIT_MS)));
                    }
                }
                "max-age" => {
                    let bytes = attr_value.as_bytes();
                    let valid = !bytes.is_empty()
                        && (bytes[0].is_ascii_digit() || bytes[0] == b'-')
                        && bytes[1..].iter().all(u8::is_ascii_digit)
                        && attr_value != "-";
                    if valid {
                        // (very large values saturate; the 400-day cap applies anyway)
                        let delta: i64 = attr_value.parse().unwrap_or(if bytes[0] == b'-' {
                            i64::MIN
                        } else {
                            i64::MAX
                        });
                        cookie.max_age = Some(if delta <= 0 {
                            0
                        } else {
                            now.saturating_add(delta.saturating_mul(1000).min(MAX_AGE_LIMIT_MS))
                        });
                    }
                }
                "domain" => {
                    if attr_value.is_empty() {
                        continue;
                    }
                    let d = attr_value.strip_prefix('.').unwrap_or(attr_value);
                    cookie.domain = Some(d.to_ascii_lowercase());
                }
                "path" => {
                    cookie.path = if attr_value.starts_with('/') {
                        Some(attr_value.to_string())
                    } else {
                        None
                    };
                }
                "secure" => cookie.secure = true,
                "httponly" => cookie.http_only = true,
                "samesite" => {
                    cookie.same_site = Some(match attr_value.to_ascii_lowercase().as_str() {
                        "none" => SameSite::None,
                        "strict" => SameSite::Strict,
                        "lax" => SameSite::Lax,
                        _ => SameSite::Default,
                    });
                }
                _ => {}
            }
        }
        Some(cookie)
    }
}

/// §5.1.1 the cookie-date algorithm. Milliseconds since the epoch, or `None`.
pub fn parse_cookie_date(input: &str) -> Option<i64> {
    fn is_delimiter(b: u8) -> bool {
        b == 0x09
            || (0x20..=0x2F).contains(&b)
            || (0x3B..=0x40).contains(&b)
            || (0x5B..=0x60).contains(&b)
            || (0x7B..=0x7E).contains(&b)
    }
    // n digits (min..=max) at the start, then end or a non-digit
    fn leading_digits(token: &[u8], min: usize, max: usize) -> Option<u32> {
        let n = token.iter().take_while(|b| b.is_ascii_digit()).count();
        if n < min || n > max {
            return None;
        }
        std::str::from_utf8(&token[..n]).ok()?.parse().ok()
    }
    fn time(token: &[u8]) -> Option<(u32, u32, u32)> {
        let mut parts = [0u32; 3];
        let mut pos = 0;
        for (i, part) in parts.iter_mut().enumerate() {
            let n = token[pos..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            if n == 0 || n > 2 {
                return None;
            }
            *part = std::str::from_utf8(&token[pos..pos + n])
                .ok()?
                .parse()
                .ok()?;
            pos += n;
            if i < 2 {
                if token.get(pos) != Some(&b':') {
                    return None;
                }
                pos += 1;
            } else if token.get(pos).is_some_and(u8::is_ascii_digit) {
                return None;
            }
        }
        Some((parts[0], parts[1], parts[2]))
    }
    const MONTHS: [&[u8]; 12] = [
        b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct", b"nov",
        b"dec",
    ];
    let bytes = input.as_bytes();
    let mut found_time = None;
    let mut day = None;
    let mut month = None;
    let mut year = None;
    for token in bytes.split(|&b| is_delimiter(b)).filter(|t| !t.is_empty()) {
        if found_time.is_none() {
            if let Some(t) = time(token) {
                found_time = Some(t);
                continue;
            }
        }
        if day.is_none() {
            if let Some(d) = leading_digits(token, 1, 2) {
                day = Some(d);
                continue;
            }
        }
        if month.is_none() && token.len() >= 3 {
            let prefix = token[..3].to_ascii_lowercase();
            if let Some(m) = MONTHS.iter().position(|m| *m == prefix.as_slice()) {
                month = Some(m as u32 + 1);
                continue;
            }
        }
        if year.is_none() {
            if let Some(y) = leading_digits(token, 2, 4) {
                year = Some(y);
                continue;
            }
        }
    }
    let (hour, minute, second) = found_time?;
    let (day, month, mut year) = (day?, month?, year?);
    if (70..=99).contains(&year) {
        year += 1900;
    } else if year <= 69 {
        year += 2000;
    }
    if !(1..=31).contains(&day) || year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day > month_days[month as usize - 1] {
        return None;
    }
    let days = days_from_civil(year as i64, month as i64, day as i64);
    Some(((days * 24 + hour as i64) * 60 + minute as i64) * 60_000 + second as i64 * 1000)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// §5.1.3 domain-match.
pub fn domain_match(host: &str, domain: &str) -> bool {
    if host == domain {
        return true;
    }
    let is_ip = host.starts_with('[') || host.parse::<std::net::Ipv4Addr>().is_ok();
    !is_ip
        && host.len() > domain.len()
        && host.ends_with(domain)
        && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
}

/// §5.1.4 default-path.
pub fn default_path(url: &Url) -> String {
    let path = url.path();
    if !path.starts_with('/') {
        return "/".into();
    }
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].to_string(),
    }
}

/// §5.1.4 path-match.
pub fn path_match(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/')
                || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

/// Where a cookie operation comes from, as the storage and retrieval models need it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CookieContext {
    /// The request URL (or the document's URL for `document.cookie`).
    pub url: Url,
    /// True for HTTP (`Set-Cookie`/`Cookie`), false for `document.cookie`.
    pub http_api: bool,
    /// The request is same-site (§5.2): its site for cookies is same-site with every URL it went through.
    pub same_site: bool,
    /// The request navigates a top-level traversable.
    pub top_level_navigation: bool,
    /// The request's method is safe (GET, HEAD, OPTIONS, TRACE).
    pub safe_method: bool,
}

impl CookieContext {
    /// A same-site HTTP request for `url` (what a typed navigation is).
    pub fn same_site_http(url: Url) -> Self {
        CookieContext {
            url,
            http_api: true,
            same_site: true,
            top_level_navigation: false,
            safe_method: true,
        }
    }
}

fn canonical_host(url: &Url) -> Option<String> {
    url.host_str()
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
}

fn is_secure_request(url: &Url) -> bool {
    matches!(url.scheme(), "https" | "wss") || is_url_potentially_trustworthy(url)
}

#[derive(Default)]
struct Jar {
    by_domain: HashMap<String, Vec<Cookie>>,
    count: usize,
    last_creation: i64,
    dirty: bool,
}

impl Jar {
    fn next_creation(&mut self, now: i64) -> i64 {
        let t = (now * 1000).max(self.last_creation + 1);
        self.last_creation = t;
        t
    }

    fn insert(&mut self, cookie: Cookie) {
        if cookie.persistent {
            self.dirty = true;
        }
        self.last_creation = self.last_creation.max(cookie.creation);
        self.by_domain
            .entry(cookie.domain.clone())
            .or_default()
            .push(cookie);
        self.count += 1;
    }

    fn remove_expired(&mut self, now: i64) {
        let mut removed = 0;
        let mut dirty = false;
        for list in self.by_domain.values_mut() {
            list.retain(|c| {
                let keep = c.expiry > now;
                if !keep {
                    removed += 1;
                    dirty |= c.persistent;
                }
                keep
            });
        }
        self.by_domain.retain(|_, l| !l.is_empty());
        self.count -= removed;
        self.dirty |= dirty;
    }

    /// §5.7 step 26: evict the least recently used cookies past the per-site and total limits.
    fn enforce_limits(&mut self, site: &str) {
        let mut site_cookies: Vec<(String, i64, i64)> = self
            .by_domain
            .iter()
            .filter(|(d, _)| registrable_or_self(d) == site)
            .flat_map(|(d, l)| {
                l.iter()
                    .map(move |c| (d.clone(), c.last_access, c.creation))
            })
            .collect();
        if site_cookies.len() > PER_DOMAIN_LIMIT {
            site_cookies.sort_by_key(|(_, access, _)| *access);
            let excess = site_cookies.len() - PER_DOMAIN_LIMIT;
            for (domain, _, creation) in site_cookies.into_iter().take(excess) {
                self.remove_where(&domain, |c| c.creation == creation);
            }
        }
        if self.count > TOTAL_LIMIT {
            let mut all: Vec<(String, i64, i64)> = self
                .by_domain
                .iter()
                .flat_map(|(d, l)| {
                    l.iter()
                        .map(move |c| (d.clone(), c.last_access, c.creation))
                })
                .collect();
            all.sort_by_key(|(_, access, _)| *access);
            let excess = self.count - TOTAL_LIMIT;
            for (domain, _, creation) in all.into_iter().take(excess) {
                self.remove_where(&domain, |c| c.creation == creation);
            }
        }
    }

    fn remove_where(&mut self, domain: &str, pred: impl Fn(&Cookie) -> bool) {
        if let Some(list) = self.by_domain.get_mut(domain) {
            let before = list.len();
            let mut dirty = false;
            list.retain(|c| {
                let hit = pred(c);
                dirty |= hit && c.persistent;
                !hit
            });
            self.count -= before - list.len();
            self.dirty |= dirty;
            if list.is_empty() {
                self.by_domain.remove(domain);
            }
        }
    }

    /// The domains whose cookies a request to `host` can see: the host and each parent domain.
    fn candidate_domains(host: &str) -> Vec<&str> {
        let mut out = vec![host];
        if host.starts_with('[') || host.parse::<std::net::Ipv4Addr>().is_ok() {
            return out;
        }
        let mut rest = host;
        while let Some(i) = rest.find('.') {
            rest = &rest[i + 1..];
            if !rest.is_empty() {
                out.push(rest);
            }
        }
        out
    }
}

fn registrable_or_self(domain: &str) -> String {
    registrable_domain(&Host::Domain(domain.to_string())).unwrap_or_else(|| domain.to_string())
}

/// The cookie store.
pub struct CookieStore {
    jar: Mutex<Jar>,
    file: Option<PathBuf>,
}

impl Default for CookieStore {
    fn default() -> Self {
        CookieStore::in_memory()
    }
}

impl std::fmt::Debug for CookieStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieStore")
            .field("file", &self.file)
            .field("count", &self.len())
            .finish()
    }
}

impl CookieStore {
    /// A store that is never saved.
    pub fn in_memory() -> Self {
        CookieStore {
            jar: Mutex::new(Jar::default()),
            file: None,
        }
    }

    /// A store persisted to `file` (`cookies.json` in the profile folder); loads what is there now.
    /// A damaged file starts the jar empty.
    pub fn open(file: &Path) -> Self {
        let store = CookieStore {
            jar: Mutex::new(Jar::default()),
            file: Some(file.to_path_buf()),
        };
        if let Ok(text) = std::fs::read_to_string(file) {
            if let Ok(cookies) = serde_json::from_str::<Vec<Cookie>>(&text) {
                let now = now_ms();
                let mut jar = store.jar.lock().unwrap();
                for c in cookies
                    .into_iter()
                    .filter(|c| c.persistent && c.expiry > now)
                {
                    jar.insert(c);
                }
                jar.dirty = false;
            }
        }
        store
    }

    pub fn len(&self) -> usize {
        self.jar.lock().unwrap().count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether there are unsaved changes to persistent cookies.
    pub fn is_dirty(&self) -> bool {
        self.jar.lock().unwrap().dirty
    }

    /// Process one `Set-Cookie` header (or a `document.cookie` assignment). True when stored.
    pub fn set_cookie(&self, header: &str, ctx: &CookieContext) -> bool {
        self.set_cookie_at(header, ctx, now_ms())
    }

    /// [`Self::set_cookie`] at a given time.
    pub fn set_cookie_at(&self, header: &str, ctx: &CookieContext, now: i64) -> bool {
        match SetCookie::parse(header, now) {
            Some(parsed) => self.store_at(parsed, ctx, now),
            None => false,
        }
    }

    /// §5.7 the storage model.
    pub fn store_at(&self, parsed: SetCookie, ctx: &CookieContext, now: i64) -> bool {
        let Some(request_host) = canonical_host(&ctx.url) else {
            return false;
        };
        if !matches!(ctx.url.scheme(), "http" | "https" | "ws" | "wss") {
            return false;
        }
        let secure_request = is_secure_request(&ctx.url);
        let (persistent, expiry) = match (parsed.max_age, parsed.expires) {
            (Some(t), _) | (None, Some(t)) => (true, t),
            (None, None) => (false, i64::MAX),
        };
        let mut domain_attribute = match &parsed.domain {
            Some(d) => match Host::parse(d) {
                Ok(Host::Domain(d)) => d.trim_end_matches('.').to_string(),
                Ok(other) => crate::origin::host_string(&other),
                Err(_) => return false,
            },
            None => String::new(),
        };
        if !domain_attribute.is_empty() && is_public_suffix(&domain_attribute) {
            if domain_attribute == request_host {
                domain_attribute.clear();
            } else {
                return false;
            }
        }
        let (host_only, domain) = if !domain_attribute.is_empty() {
            if !domain_match(&request_host, &domain_attribute) {
                return false;
            }
            (false, domain_attribute)
        } else {
            (true, request_host.clone())
        };
        let path = parsed
            .path
            .clone()
            .unwrap_or_else(|| default_path(&ctx.url));
        let secure_only = parsed.secure;
        if secure_only && !secure_request {
            return false;
        }
        if !ctx.http_api && parsed.http_only {
            return false;
        }
        let same_site = parsed.same_site.unwrap_or(SameSite::Default);
        if same_site != SameSite::None && !ctx.same_site && !ctx.top_level_navigation {
            return false;
        }
        if same_site == SameSite::None && !secure_only {
            return false;
        }
        let lower_name = parsed.name.to_ascii_lowercase();
        if lower_name.starts_with("__secure-") && !secure_only {
            return false;
        }
        if lower_name.starts_with("__host-")
            && !(secure_only && host_only && parsed.path.as_deref() == Some("/"))
        {
            return false;
        }
        if parsed.name.is_empty() {
            let lower_value = parsed.value.to_ascii_lowercase();
            if lower_value.starts_with("__secure-") || lower_value.starts_with("__host-") {
                return false;
            }
        }

        let mut jar = self.jar.lock().unwrap();
        // §5.7 step 16: an insecure request can't shadow a secure cookie of the same name.
        if !secure_only && !secure_request {
            let shadows = Jar::candidate_domains(&domain)
                .into_iter()
                .chain(
                    jar.by_domain
                        .keys()
                        .map(String::as_str)
                        .filter(|d| domain_match(d, &domain)),
                )
                .collect::<Vec<_>>()
                .into_iter()
                .filter_map(|d| jar.by_domain.get(d))
                .flatten()
                .any(|c| {
                    c.name == parsed.name
                        && c.secure_only
                        && (domain_match(&c.domain, &domain) || domain_match(&domain, &c.domain))
                        && path_match(&path, &c.path)
                });
            if shadows {
                return false;
            }
        }
        let mut creation = None;
        if let Some(list) = jar.by_domain.get(&domain) {
            if let Some(old) = list
                .iter()
                .find(|c| c.name == parsed.name && c.host_only == host_only && c.path == path)
            {
                if !ctx.http_api && old.http_only {
                    return false;
                }
                creation = Some(old.creation);
            }
        }
        if creation.is_some() {
            let name = parsed.name.clone();
            let p = path.clone();
            jar.remove_where(&domain, |c| {
                c.name == name && c.host_only == host_only && c.path == p
            });
        }
        let creation = match creation {
            Some(c) => c,
            None => jar.next_creation(now),
        };
        let cookie = Cookie {
            name: parsed.name,
            value: parsed.value,
            domain: domain.clone(),
            path,
            expiry,
            persistent,
            host_only,
            secure_only,
            http_only: parsed.http_only,
            same_site,
            creation,
            last_access: now,
        };
        let stored = cookie.expiry > now;
        if stored {
            jar.insert(cookie);
        }
        jar.remove_expired(now);
        if stored {
            jar.enforce_limits(&registrable_or_self(&domain));
        }
        stored
    }

    /// §5.8.3 the cookies a request may carry, sorted (longer paths first, then older). Updates their
    /// last-access time.
    pub fn cookies_for(&self, ctx: &CookieContext) -> Vec<Cookie> {
        self.cookies_for_at(ctx, now_ms())
    }

    pub fn cookies_for_at(&self, ctx: &CookieContext, now: i64) -> Vec<Cookie> {
        let Some(host) = canonical_host(&ctx.url) else {
            return Vec::new();
        };
        if !matches!(ctx.url.scheme(), "http" | "https" | "ws" | "wss") {
            return Vec::new();
        }
        let secure = is_secure_request(&ctx.url);
        let path = ctx.url.path().to_string();
        let mut jar = self.jar.lock().unwrap();
        jar.remove_expired(now);
        let mut out = Vec::new();
        for domain in Jar::candidate_domains(&host) {
            let Some(list) = jar.by_domain.get_mut(domain) else {
                continue;
            };
            for c in list.iter_mut() {
                let host_ok = if c.host_only {
                    c.domain == host
                } else {
                    domain_match(&host, &c.domain)
                };
                if !host_ok || !path_match(&path, &c.path) {
                    continue;
                }
                if c.secure_only && !secure {
                    continue;
                }
                if c.http_only && !ctx.http_api {
                    continue;
                }
                if !ctx.same_site && c.same_site != SameSite::None {
                    let lax_ok = ctx.top_level_navigation && ctx.safe_method;
                    if c.same_site == SameSite::Strict || !lax_ok {
                        continue;
                    }
                }
                c.last_access = now;
                out.push(c.clone());
            }
        }
        out.sort_by(|a, b| {
            b.path
                .len()
                .cmp(&a.path.len())
                .then(a.creation.cmp(&b.creation))
        });
        out
    }

    /// The `Cookie` header value (or `document.cookie`) for a context: "name=value; name2=value2".
    pub fn cookie_string(&self, ctx: &CookieContext) -> String {
        serialize_cookies(&self.cookies_for(ctx))
    }

    pub fn cookie_string_at(&self, ctx: &CookieContext, now: i64) -> String {
        serialize_cookies(&self.cookies_for_at(ctx, now))
    }

    /// Every cookie a host could receive (for the site information panel).
    pub fn all_for_host(&self, host: &str) -> Vec<Cookie> {
        let jar = self.jar.lock().unwrap();
        Jar::candidate_domains(host)
            .into_iter()
            .filter_map(|d| jar.by_domain.get(d))
            .flatten()
            .filter(|c| {
                if c.host_only {
                    c.domain == host
                } else {
                    domain_match(host, &c.domain)
                }
            })
            .cloned()
            .collect()
    }

    /// The sites (registrable domains) with cookies, and how many each has, by name.
    pub fn sites(&self) -> Vec<(String, usize)> {
        let jar = self.jar.lock().unwrap();
        let mut by_site: HashMap<String, usize> = HashMap::new();
        for (domain, list) in &jar.by_domain {
            if !list.is_empty() {
                *by_site.entry(registrable_or_self(domain)).or_default() += list.len();
            }
        }
        let mut out: Vec<(String, usize)> = by_site.into_iter().collect();
        out.sort();
        out
    }

    /// Remove every cookie of a site (the host's registrable domain and its subdomains).
    pub fn delete_site(&self, host: &str) {
        let site = registrable_or_self(host);
        let mut jar = self.jar.lock().unwrap();
        let domains: Vec<String> = jar
            .by_domain
            .keys()
            .filter(|d| domain_match(d, &site))
            .cloned()
            .collect();
        for d in domains {
            jar.remove_where(&d, |_| true);
        }
    }

    /// Deletes the cookies created from `begin` (inclusive) to `end` (exclusive), in milliseconds
    /// since 1970: what clearing the browsing data of a time range takes. A cookie replaced keeps
    /// its first creation time (RFC 6265 §5.3 step 11.3).
    pub fn delete_created_between(&self, begin: i64, end: i64) {
        let (begin, end) = (begin.saturating_mul(1000), end.saturating_mul(1000));
        let mut jar = self.jar.lock().unwrap();
        let domains: Vec<String> = jar.by_domain.keys().cloned().collect();
        for d in domains {
            jar.remove_where(&d, |c| (begin..end).contains(&c.creation));
        }
    }

    pub fn clear(&self) {
        let mut jar = self.jar.lock().unwrap();
        let had_persistent = jar.by_domain.values().flatten().any(|c| c.persistent);
        jar.by_domain.clear();
        jar.count = 0;
        jar.dirty |= had_persistent;
    }

    /// Write the persistent cookies if anything changed: to a temporary file first, then renamed over the
    /// old one so a crash mid-write can't leave a truncated jar.
    pub fn flush(&self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let json = {
            let mut jar = self.jar.lock().unwrap();
            if !jar.dirty {
                return Ok(());
            }
            jar.dirty = false;
            let persistent: Vec<&Cookie> = jar
                .by_domain
                .values()
                .flatten()
                .filter(|c| c.persistent)
                .collect();
            serde_json::to_string(&persistent).map_err(std::io::Error::other)?
        };
        write_atomically(file, json.as_bytes())
    }
}

pub(crate) fn write_atomically(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = file.with_extension("tmp");
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, file)
}

/// §5.8.3 step 4: "name=value" pairs joined with "; " (a nameless cookie is just its value).
pub fn serialize_cookies(cookies: &[Cookie]) -> String {
    let mut out = String::new();
    for c in cookies {
        if !out.is_empty() {
            out.push_str("; ");
        }
        if !c.name.is_empty() {
            out.push_str(&c.name);
            out.push('=');
        }
        out.push_str(&c.value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000; // 2023-11-14

    fn ctx(url: &str) -> CookieContext {
        CookieContext::same_site_http(Url::parse(url).unwrap())
    }

    fn set(store: &CookieStore, url: &str, header: &str) -> bool {
        store.set_cookie_at(header, &ctx(url), NOW)
    }

    fn get(store: &CookieStore, url: &str) -> String {
        store.cookie_string_at(&ctx(url), NOW)
    }

    #[test]
    fn a_time_range_deletes_the_cookies_created_in_it() {
        let store = CookieStore::in_memory();
        let hour = 3_600_000;
        assert!(store.set_cookie_at(
            "old=1; Max-Age=86400",
            &ctx("https://a.example/"),
            NOW - 2 * hour
        ));
        assert!(store.set_cookie_at("new=1; Max-Age=86400", &ctx("https://a.example/"), NOW));
        assert!(store.set_cookie_at("b=1", &ctx("https://b.example/"), NOW - hour / 2));
        // Replaced within the hour, the old cookie keeps its creation time.
        assert!(store.set_cookie_at("old=2; Max-Age=86400", &ctx("https://a.example/"), NOW));
        store.delete_created_between(NOW - hour, i64::MAX);
        assert_eq!(get(&store, "https://a.example/"), "old=2");
        assert_eq!(get(&store, "https://b.example/"), "");
        assert_eq!(store.len(), 1);
        // The end is exclusive.
        store.delete_created_between(0, NOW - 2 * hour);
        assert_eq!(store.len(), 1);
        store.delete_created_between(0, NOW - 2 * hour + 1);
        assert!(store.is_empty());
    }

    #[test]
    fn sites_are_listed_and_cleared_one_at_a_time() {
        let s = CookieStore::in_memory();
        assert!(set(&s, "https://www.example.com/", "a=1; Max-Age=3600"));
        assert!(set(&s, "https://example.com/", "b=2; Domain=example.com"));
        assert!(set(&s, "https://shop.example.com/", "c=3"));
        assert!(set(&s, "https://other.org/", "d=4"));
        assert_eq!(
            s.sites(),
            [("example.com".to_owned(), 3), ("other.org".to_owned(), 1)]
        );
        // A host of the site clears all of the site's cookies, not another site's.
        s.delete_site("shop.example.com");
        assert_eq!(s.sites(), [("other.org".to_owned(), 1)]);
        assert_eq!(get(&s, "https://www.example.com/"), "");
        assert_eq!(get(&s, "https://other.org/"), "d=4");
        // Clearing a persistent cookie marks the jar for saving.
        assert!(s.is_dirty());
    }

    #[test]
    fn dates() {
        // RFC 6265 §5.1.1 and the formats in the wild.
        let expected = Some(784111777000); // Sun, 06 Nov 1994 08:49:37 GMT
        assert_eq!(parse_cookie_date("Sun, 06 Nov 1994 08:49:37 GMT"), expected);
        assert_eq!(
            parse_cookie_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            expected
        );
        assert_eq!(parse_cookie_date("Sun Nov  6 08:49:37 1994"), expected);
        assert_eq!(
            parse_cookie_date("Wed, 09 Jun 2021 10:18:14 GMT"),
            Some(1623233894000)
        );
        assert_eq!(parse_cookie_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_cookie_date("Thu, 31 Feb 2021 00:00:00 GMT"), None);
        assert_eq!(parse_cookie_date("Thu, 01 Jan 1600 00:00:00 GMT"), None);
        assert_eq!(parse_cookie_date("Thu, 01 Jan 2020 24:00:00 GMT"), None);
        assert_eq!(parse_cookie_date("garbage"), None);
        assert_eq!(parse_cookie_date("1 Jan 2020 1:2:3"), Some(1577840523000));
    }

    #[test]
    fn rfc_6265_section_3_1_examples() {
        let store = CookieStore::in_memory();
        assert!(set(
            &store,
            "https://www.example.com/",
            "SID=31d4d96e407aad42"
        ));
        assert_eq!(
            get(&store, "https://www.example.com/"),
            "SID=31d4d96e407aad42"
        );
        // host-only: not sent to a subdomain or parent
        assert_eq!(get(&store, "https://example.com/"), "");

        let store = CookieStore::in_memory();
        assert!(set(
            &store,
            "https://www.example.com/",
            "SID=31d4d96e407aad42; Path=/; Domain=example.com"
        ));
        assert_eq!(get(&store, "https://example.com/"), "SID=31d4d96e407aad42");
        assert_eq!(
            get(&store, "https://a.b.example.com/x"),
            "SID=31d4d96e407aad42"
        );

        let store = CookieStore::in_memory();
        assert!(set(
            &store,
            "https://www.example.com/",
            "SID=31d4d96e407aad42; Path=/; Secure; HttpOnly"
        ));
        assert!(set(
            &store,
            "https://www.example.com/",
            "lang=en-US; Path=/; Domain=example.com"
        ));
        assert_eq!(
            get(&store, "https://www.example.com/"),
            "SID=31d4d96e407aad42; lang=en-US"
        );
        assert_eq!(get(&store, "http://www.example.com/"), "lang=en-US");
        // document.cookie can't see the HttpOnly one
        let mut doc = ctx("https://www.example.com/");
        doc.http_api = false;
        assert_eq!(store.cookie_string_at(&doc, NOW), "lang=en-US");

        // persistent, then removed by an expiry in the past
        let store = CookieStore::in_memory();
        assert!(set(
            &store,
            "https://www.example.com/",
            "lang=en-US; Expires=Wed, 09 Jun 2031 10:18:14 GMT"
        ));
        assert_eq!(get(&store, "https://www.example.com/"), "lang=en-US");
        assert!(!set(
            &store,
            "https://www.example.com/",
            "lang=; Expires=Sun, 06 Nov 1994 08:49:37 GMT"
        ));
        assert_eq!(get(&store, "https://www.example.com/"), "");
    }

    #[test]
    fn domain_and_public_suffix() {
        let store = CookieStore::in_memory();
        assert!(!set(
            &store,
            "https://www.example.com/",
            "a=1; Domain=other.com"
        ));
        assert!(!set(&store, "https://www.example.com/", "a=1; Domain=com"));
        assert!(!set(
            &store,
            "https://www.example.co.uk/",
            "a=1; Domain=co.uk"
        ));
        assert!(set(
            &store,
            "https://www.example.co.uk/",
            "a=1; Domain=.EXAMPLE.co.uk"
        ));
        assert_eq!(get(&store, "https://shop.example.co.uk/"), "a=1");
        // a host that is itself a public suffix may set a host-only cookie for itself
        let store = CookieStore::in_memory();
        assert!(set(&store, "https://github.io/", "a=1; Domain=github.io"));
        assert_eq!(get(&store, "https://github.io/"), "a=1");
        assert_eq!(get(&store, "https://x.github.io/"), "");
        // IP addresses only match exactly
        let store = CookieStore::in_memory();
        assert!(set(&store, "http://127.0.0.1/", "a=1"));
        assert_eq!(get(&store, "http://127.0.0.1/"), "a=1");
    }

    #[test]
    fn paths() {
        let store = CookieStore::in_memory();
        assert!(set(&store, "https://a.example/docs/page", "d=1"));
        assert!(set(&store, "https://a.example/", "r=1; Path=/"));
        assert!(set(&store, "https://a.example/", "x=1; Path=/docs/web"));
        assert_eq!(get(&store, "https://a.example/docs/web/x"), "x=1; d=1; r=1");
        assert_eq!(get(&store, "https://a.example/docsx"), "r=1");
        assert_eq!(get(&store, "https://a.example/docs"), "d=1; r=1");
        assert_eq!(default_path(&Url::parse("https://a/b/c").unwrap()), "/b");
        assert_eq!(default_path(&Url::parse("https://a/b").unwrap()), "/");
        assert!(path_match("/docs/x", "/docs/"));
        assert!(!path_match("/docsx", "/docs"));
    }

    #[test]
    fn secure_and_prefixes() {
        let store = CookieStore::in_memory();
        assert!(!set(&store, "http://a.example/", "s=1; Secure"));
        assert!(set(&store, "http://localhost/", "s=1; Secure"));
        assert!(!set(&store, "https://a.example/", "__Secure-a=1"));
        assert!(set(&store, "https://a.example/", "__Secure-a=1; Secure"));
        assert!(!set(&store, "https://a.example/", "__secure-b=1"));
        assert!(!set(&store, "https://a.example/", "__Host-a=1; Secure"));
        assert!(!set(
            &store,
            "https://a.example/",
            "__Host-a=1; Secure; Path=/; Domain=a.example"
        ));
        assert!(set(
            &store,
            "https://a.example/",
            "__Host-a=1; Secure; Path=/"
        ));
        assert!(!set(&store, "https://a.example/", "=__Host-x"));
        // insecure origins can't shadow a secure cookie
        assert!(set(&store, "https://a.example/", "sid=secure; Secure"));
        assert!(!set(&store, "http://a.example/", "sid=evil"));
        assert!(set(&store, "http://a.example/", "other=fine"));
    }

    #[test]
    fn http_only_overwrite() {
        let store = CookieStore::in_memory();
        assert!(set(&store, "https://a.example/", "h=1; HttpOnly"));
        let mut doc = ctx("https://a.example/");
        doc.http_api = false;
        assert!(!store.set_cookie_at("h=2", &doc, NOW));
        assert!(!store.set_cookie_at("j=2; HttpOnly", &doc, NOW));
        assert!(store.set_cookie_at("j=2", &doc, NOW));
        assert_eq!(get(&store, "https://a.example/"), "h=1; j=2");
    }

    #[test]
    fn same_site() {
        let store = CookieStore::in_memory();
        assert!(set(
            &store,
            "https://a.example/",
            "strict=1; SameSite=Strict"
        ));
        assert!(set(&store, "https://a.example/", "lax=1; SameSite=Lax"));
        assert!(set(&store, "https://a.example/", "default=1"));
        assert!(set(
            &store,
            "https://a.example/",
            "none=1; SameSite=None; Secure"
        ));
        assert!(!set(
            &store,
            "https://a.example/",
            "insecure=1; SameSite=None"
        ));
        let mut cross = ctx("https://a.example/");
        cross.same_site = false;
        assert_eq!(store.cookie_string_at(&cross, NOW), "none=1");
        cross.top_level_navigation = true;
        assert_eq!(
            store.cookie_string_at(&cross, NOW),
            "lax=1; default=1; none=1"
        );
        cross.safe_method = false;
        assert_eq!(store.cookie_string_at(&cross, NOW), "none=1");
        // a cross-site subresource can't set SameSite cookies
        cross.top_level_navigation = false;
        assert!(!store.set_cookie_at("x=1; SameSite=Lax", &cross, NOW));
        assert!(store.set_cookie_at("y=1; SameSite=None; Secure", &cross, NOW));
    }

    #[test]
    fn parse_details() {
        let c = SetCookie::parse(
            "  n = v ; max-age=10; expires=Sun, 06 Nov 1994 08:49:37 GMT; path=x",
            NOW,
        )
        .unwrap();
        assert_eq!((c.name.as_str(), c.value.as_str()), ("n", "v"));
        assert_eq!(c.max_age, Some(NOW + 10_000));
        assert_eq!(c.path, None);
        let nameless = SetCookie::parse("justvalue", NOW).unwrap();
        assert_eq!(
            (nameless.name.as_str(), nameless.value.as_str()),
            ("", "justvalue")
        );
        assert!(SetCookie::parse("a=b\x01", NOW).is_none());
        assert!(SetCookie::parse("=", NOW).is_none());
        let capped = SetCookie::parse("a=b; Max-Age=999999999", NOW).unwrap();
        assert_eq!(capped.max_age, Some(NOW + MAX_AGE_LIMIT_MS));
        assert_eq!(
            SetCookie::parse("a=b; Max-Age=-1", NOW).unwrap().max_age,
            Some(0)
        );
        assert_eq!(
            SetCookie::parse("a=b; Max-Age=1x", NOW).unwrap().max_age,
            None
        );
        // Max-Age wins over Expires regardless of order
        let store = CookieStore::in_memory();
        assert!(set(
            &store,
            "https://a.example/",
            "a=1; Max-Age=60; Expires=Sun, 06 Nov 1994 08:49:37 GMT"
        ));
        assert_eq!(get(&store, "https://a.example/"), "a=1");
    }

    #[test]
    fn replacement_keeps_creation_order() {
        let store = CookieStore::in_memory();
        assert!(store.set_cookie_at("a=1", &ctx("https://a.example/"), NOW));
        assert!(store.set_cookie_at("b=1", &ctx("https://a.example/"), NOW + 1));
        assert!(store.set_cookie_at("a=2", &ctx("https://a.example/"), NOW + 2));
        assert_eq!(get(&store, "https://a.example/"), "a=2; b=1");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn persistence() {
        let dir = std::env::temp_dir().join(format!("snowlark-cookies-{}", std::process::id()));
        let file = dir.join("cookies.json");
        let _ = std::fs::remove_file(&file);
        {
            let store = CookieStore::open(&file);
            let now = now_ms();
            assert!(store.set_cookie_at("p=1; Max-Age=3600", &ctx("https://a.example/"), now));
            assert!(store.set_cookie_at("s=1", &ctx("https://a.example/"), now));
            assert!(store.is_dirty());
            store.flush().unwrap();
            assert!(!store.is_dirty());
        }
        let reopened = CookieStore::open(&file);
        assert_eq!(reopened.cookie_string(&ctx("https://a.example/")), "p=1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn limits() {
        let store = CookieStore::in_memory();
        for i in 0..200 {
            assert!(store.set_cookie_at(&format!("c{i}=1"), &ctx("https://a.example/"), NOW + i));
        }
        assert_eq!(store.len(), PER_DOMAIN_LIMIT);
        assert!(!get(&store, "https://a.example/").contains("c0="));
    }
}
