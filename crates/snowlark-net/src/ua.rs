// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The user agent a browser reports: the presets Settings offers, what may be stored and sent
//! (the `User-Agent` header is built from user text), and the profile every place a site can read it
//! derives from, so the `User-Agent` header, `navigator.userAgent`, `appVersion`, `platform`,
//! `vendor`, `productSub`, `oscpu`, `navigator.userAgentData` and the `Sec-CH-UA` request headers
//! cannot disagree about which browser this is.
//!
//! The default is Snowlark's own identity: the compatibility tokens of the family whose navigator
//! compatibility mode the engine follows, then `Snowlark/<version>`; its brands are that family's
//! compatibility brand, `Snowlark` and the grease brand. It names no other browser.

use crate::metadata::USER_AGENT as DEFAULT_UA;

/// The longest user agent kept or sent (bytes).
pub const MAX_LEN: usize = 512;

/// The browser's own product token and version (major and minor), as the default user agent and its
/// brands carry them.
pub const PRODUCT: &str = "Snowlark";
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR")
);

/// The version of the compatibility brand (the family's equivalence brand in `userAgentData` and the
/// client hints): the release whose web-facing behaviour the engine follows. The default user agent
/// string carries no token with it.
pub const COMPAT_VERSION: &str = "150.0.0.0";

/// The Chrome preset's value.
const CHROME_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";

/// The Edge preset's value, which the sites in [`EDGE_SITES`] also get.
const EDGE_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36 Edg/150.0.0.0";

/// A choice in Settings. The values are the strings the browsers send today; update the versions
/// here, this table is the only place they are written.
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    /// The `User-Agent` value.
    pub value: &'static str,
}

/// The presets, in the order Settings lists them. `default` is Snowlark's own identity (see
/// [`crate::metadata::USER_AGENT`]); the others are the strings those browsers send.
pub const PRESETS: [Preset; 5] = [
    Preset {
        id: "default",
        name: "Default (Snowlark)",
        value: DEFAULT_UA,
    },
    Preset {
        id: "chrome",
        name: "Chrome",
        value: CHROME_UA,
    },
    Preset {
        id: "edge",
        name: "Edge",
        value: EDGE_UA,
    },
    Preset {
        id: "firefox",
        name: "Firefox",
        value: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:150.0) Gecko/20100101 Firefox/150.0",
    },
    Preset {
        id: "safari",
        name: "Safari",
        value: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
    },
];

/// What may be stored, shown and sent as a user agent: printable ASCII and spaces only (no
/// control characters, so no line break can end the header and start another; nothing outside ASCII,
/// which a header may not carry as text), trimmed and cut to [`MAX_LEN`]. `None` when nothing is
/// left.
pub fn sanitize(text: &str) -> Option<String> {
    let kept: String = text
        .chars()
        .filter(|c| (' '..='~').contains(c))
        .take(MAX_LEN)
        .collect();
    let kept = kept.trim();
    (!kept.is_empty()).then(|| kept.to_owned())
}

/// The user agent of a browser: its value and whether the rest of what a page can read follows it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserAgent {
    /// The `User-Agent` value; never empty, never anything [`sanitize`] removes.
    pub value: String,
    /// `true`: the string's browser family decides all of what a site can read ([`Profile`]); every
    /// user agent the browser makes is strict, the default included. `false`: the engine's identity
    /// before 2026-11-09 (`navigator.vendor` empty, `userAgentData` and `oscpu` both present, no client
    /// hint headers), kept only so a value of that shape still means what it did.
    pub strict: bool,
    /// Whether the sites in [`COMPAT_HOSTS`] get [`compat_value`], and those in [`EDGE_SITES`] the
    /// Edge preset's string, in place of the default string ([`UserAgent::for_url`]). Only the window
    /// turns it on, and only for the default identity: a preset or custom string is sent as chosen,
    /// and the headless modes see no site differently.
    #[serde(default)]
    pub site_compat: bool,
    /// The compatibility string of a listed site ([`UserAgent::for_url`]): its brands leave out the
    /// browser's own (`Snowlark`), as an unbranded build of the family lists them.
    #[serde(default)]
    pub unbranded: bool,
}

impl Default for UserAgent {
    fn default() -> Self {
        UserAgent {
            value: DEFAULT_UA.to_owned(),
            strict: true,
            site_compat: false,
            unbranded: false,
        }
    }
}

impl UserAgent {
    /// A chosen user agent; the default one when `value` is nothing usable once sanitized.
    pub fn chosen(value: &str) -> UserAgent {
        match sanitize(value) {
            Some(value) => UserAgent {
                value,
                strict: true,
                site_compat: false,
                unbranded: false,
            },
            None => UserAgent::default(),
        }
    }

    /// The preset with this id (`custom` and unknown ids are not presets).
    pub fn preset(id: &str) -> Option<UserAgent> {
        let p = PRESETS.iter().find(|p| p.id == id)?;
        Some(if p.id == "default" {
            UserAgent::default()
        } else {
            UserAgent::chosen(p.value)
        })
    }

    /// The value as the header carries it. Checked again here (the setting was sanitized where it
    /// was stored), because this is the line a stray line break would break.
    pub fn header_value(&self) -> String {
        sanitize(&self.value).unwrap_or_else(|| DEFAULT_UA.to_owned())
    }

    pub fn profile(&self) -> Profile {
        Profile::of(self)
    }

    /// The default identity with the per-site compatibility strings on, as the window sends it.
    pub fn default_with_site_compat() -> UserAgent {
        UserAgent {
            site_compat: true,
            ..UserAgent::default()
        }
    }

    /// The user agent a request to `url`, or a document or worker of its origin, gets: when
    /// [`UserAgent::site_compat`] is on and this is the default identity, the compatibility string for
    /// a host in [`COMPAT_HOSTS`] and the Edge preset's for a host in [`EDGE_SITES`]; otherwise this
    /// one.
    pub fn for_url(&self, url: &url::Url) -> std::borrow::Cow<'_, UserAgent> {
        match url.origin() {
            url::Origin::Tuple(_, host, _) => self.for_host(&host.to_string()),
            url::Origin::Opaque(_) => std::borrow::Cow::Borrowed(self),
        }
    }

    /// [`UserAgent::for_url`] by host name.
    pub fn for_host(&self, host: &str) -> std::borrow::Cow<'_, UserAgent> {
        let host = host.trim_end_matches('.');
        if !self.site_compat || self.value != DEFAULT_UA {
            return std::borrow::Cow::Borrowed(self);
        }
        if COMPAT_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            std::borrow::Cow::Owned(UserAgent {
                value: compat_value(),
                unbranded: true,
                ..self.clone()
            })
        } else if EDGE_SITES.iter().any(|d| in_domain(host, d)) {
            std::borrow::Cow::Owned(UserAgent {
                value: EDGE_UA.to_owned(),
                unbranded: false,
                ..self.clone()
            })
        } else {
            std::borrow::Cow::Borrowed(self)
        }
    }
}

/// Whether `host` is `domain` or one of its subdomains (ASCII case-insensitive).
fn in_domain(host: &str, domain: &str) -> bool {
    let (h, d) = (host.as_bytes(), domain.as_bytes());
    h.len() >= d.len()
        && h[h.len() - d.len()..].eq_ignore_ascii_case(d)
        && (h.len() == d.len() || h[h.len() - d.len() - 1] == b'.')
}

/// Domains (each with all its subdomains) of a service that picks its DRM system by the browser it
/// reads from the user agent string and offers PlayReady, the system this engine plays, only to
/// Edge. They get the Edge preset's string ([`UserAgent::for_host`]), and with it the Edge brand
/// (`Microsoft Edge`) in `navigator.userAgentData` and `Sec-CH-UA`; the rest of `navigator` is the
/// same as the default's. The page, its scripts, its API and its licence server all see that one
/// identity. Like [`COMPAT_HOSTS`], keep it short and give each entry its evidence.
pub const EDGE_SITES: [&str; 3] = [
    // HBO Max: its player sends its server the browser a user agent parser reads from
    // `navigator.userAgent`; the default string (no `Chrome/` token) reads as Safari and is offered
    // only Apple's DRM, the Edge string is offered PlayReady (2026-11-21, a window trace). The app
    // and its API are on `hbomax.com`, its licence server on `discomax.com`, its media and older
    // addresses on `max.com`.
    "hbomax.com",
    "discomax.com",
    "max.com",
];

/// Hosts whose servers refuse the default string and accept the same identity once it carries the
/// compatibility product token (`Chrome/<major>` of a recent release), as other engines keep a short
/// list of such sites rather than claim another browser everywhere. Exact host names; each entry
/// names what it fixes. Keep it short: an entry is for a site that turns the default string away,
/// shown, or serves it measurably slower code, never for a site that only looks different.
pub const COMPAT_HOSTS: [&str; 5] = [
    // Writes "This browser version is no longer supported" into every document page for any string
    // without a `Chrome/` token of major 130 or more, a Firefox or a macOS Safari string (client hints
    // change nothing; checked 2026-11-12 on a public document, signed out).
    "docs.google.com",
    // Redirects the signed-in app to `/u/0/error/browser-not-supported/` (2026-11-12, a window trace),
    // which also leaves the mail app on its loading screen, as it embeds this one.
    "chat.google.com",
    // The mail app moves to its chat-embedding view for the default string, and loads its inbox with
    // the compatibility token (2026-11-12, a window trace).
    "mail.google.com",
    // Judges the string as the document host does, and blocks a string it does not know (reported by
    // another engine's port, 2024).
    "drive.google.com",
    // Serves the default string its legacy bundles (the watch page's `kevlar_base` and the player
    // transpiled to ES5) and the compatibility string its 2021 and ES6 ones: the app's first script
    // ran 239-517 ms against 113-153 ms on first visits (2026-11-15, headless).
    "www.youtube.com",
];

/// The compatibility string: the default identity with the compatibility product token (and the
/// `Safari/` token that always follows it in this family) before `Snowlark/`. It derives the same
/// [`Profile`] as the default (same family, brand, version and compatibility version); a listed site
/// gets it unbranded ([`UserAgent::unbranded`]): its brands and `Sec-CH-UA` are the grease brand and
/// `Chromium` alone, since the mail app also turns away a brand list whose own entry it does not
/// know (`Snowlark` 0; checked 2026-11-12 in the window: the same string with that entry's
/// place taken by a known one loads the inbox).
pub fn compat_value() -> String {
    let own = format!("{PRODUCT}/");
    match DEFAULT_UA.find(&own) {
        Some(at) => format!(
            "{}Chrome/{COMPAT_VERSION} Safari/537.36 {}",
            &DEFAULT_UA[..at],
            &DEFAULT_UA[at..]
        ),
        None => DEFAULT_UA.to_owned(),
    }
}

/// Which engine a user agent claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// `Firefox/`.
    Gecko,
    /// `Chrome/`, `Chromium/` or `Snowlark/` (with `Edg/` for Edge).
    Chromium,
    /// `Safari/` without `Chrome/`.
    WebKit,
    /// Anything else: only the string itself is known.
    Other,
}

/// Everything derived from a user agent that a site can read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub strict: bool,
    /// The brands leave out the browser's own ([`UserAgent::unbranded`]).
    pub unbranded: bool,
    pub family: Family,
    pub edge: bool,
    /// The browser's product name (`Firefox`, `Snowlark`), version and its major.
    pub brand: String,
    pub full: String,
    pub major: String,
    /// For the `Chromium` family: the version of the compatibility brand (its token's, else
    /// [`COMPAT_VERSION`]). Empty for the others.
    pub compat: String,
    /// The operating system as `userAgentData.platform` names it.
    pub os: &'static str,
    pub platform_version: String,
    pub architecture: &'static str,
    pub bitness: &'static str,
    pub wow64: bool,
    pub mobile: bool,
}

/// The grease brand of a user agent that is not strict (UA-CH §5.1: so sites cannot match the list
/// exactly).
const GREASE: (&str, &str) = ("Not.A/Brand", "99");

/// UA-CH "create arbitrary brands": the grease brand and its version, chosen from the characters and
/// versions the spec suggests by `seed` (the compatibility brand's major), so they change from one
/// release to the next and stay the same within one, as the family's browsers do.
fn grease(seed: usize, full: bool) -> (String, String) {
    const CHARS: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
    const VERSIONS: [&str; 3] = ["8", "99", "24"];
    let name = format!(
        "Not{}A{}Brand",
        CHARS[seed % CHARS.len()],
        CHARS[(seed + 1) % CHARS.len()]
    );
    let v = VERSIONS[seed % VERSIONS.len()];
    (
        name,
        if full {
            format!("{v}.0.0.0")
        } else {
            v.to_owned()
        },
    )
}

/// UA-CH §5.1 step 3: the brands in an order chosen by `seed`, the same within a release, so that no
/// site can rely on one brand's position.
fn shuffle(list: Vec<(String, String)>, seed: usize) -> Vec<(String, String)> {
    const THREE: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let order: Vec<usize> = match list.len() {
        3 => THREE[seed % THREE.len()].to_vec(),
        n => (0..n).map(|i| (seed + i) % n).collect(),
    };
    let mut out = vec![(String::new(), String::new()); list.len()];
    for (item, &at) in list.into_iter().zip(&order) {
        out[at] = item;
    }
    out
}

impl Profile {
    pub fn of(ua: &UserAgent) -> Profile {
        Profile {
            unbranded: ua.unbranded,
            ..Profile::parse(&ua.value, ua.strict)
        }
    }

    pub fn parse(ua: &str, strict: bool) -> Profile {
        let product = ua
            .split_whitespace()
            .rev()
            .find(|t| t.contains('/') && !t.starts_with('('))
            .unwrap_or("Snowlark/1");
        let (brand, full) = product.split_once('/').unwrap_or((product, "1"));
        let token = |name: &str| {
            ua.split_whitespace()
                .find_map(|t| t.strip_prefix(name))
                .map(str::to_owned)
        };
        let edge = token("Edg/").is_some();
        let own = token("Snowlark/");
        let compat_token = token("Chrome/").or_else(|| token("Chromium/"));
        let family = if compat_token.is_some() || own.is_some() {
            Family::Chromium
        } else if token("Firefox/").is_some() {
            Family::Gecko
        } else if token("Safari/").is_some() {
            Family::WebKit
        } else {
            Family::Other
        };
        // The browser a string of this family names: its own product token if it has one (they come
        // last, after the compatibility tokens), else the compatibility token's browser.
        let (brand, full) = match (family, token("Chrome/")) {
            (Family::Chromium, _) if edge => ("Edge".to_owned(), token("Edg/").unwrap_or_default()),
            (Family::Chromium, _) if own.is_some() => (PRODUCT.to_owned(), own.unwrap_or_default()),
            (Family::Chromium, Some(v)) => ("Chrome".to_owned(), v),
            _ => (brand.to_owned(), full.to_owned()),
        };
        let compat = match family {
            Family::Chromium => compat_token.unwrap_or_else(|| COMPAT_VERSION.to_owned()),
            _ => String::new(),
        };
        let major = full.split('.').next().unwrap_or(&full).to_string();
        let os = if ua.contains("Android") {
            "Android"
        } else if ua.contains("Windows") {
            "Windows"
        } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
            "macOS"
        } else if ua.contains("CrOS") {
            "Chrome OS"
        } else if ua.contains("Linux") {
            "Linux"
        } else {
            "Unknown"
        };
        let platform_version = ua
            .split("Windows NT ")
            .nth(1)
            .and_then(|rest| rest.split([';', ')']).next())
            .map(|v| {
                let mut parts: Vec<&str> = v.trim().split('.').collect();
                while parts.len() < 3 {
                    parts.push("0");
                }
                parts.join(".")
            })
            .unwrap_or_default();
        let arm = ua.contains("ARM64") || ua.contains("aarch64");
        let wide = ua.contains("Win64") || ua.contains("x64") || ua.contains("x86_64") || arm;
        Profile {
            strict,
            unbranded: false,
            family,
            edge,
            brand,
            full,
            major,
            compat,
            os,
            platform_version,
            architecture: if arm { "arm" } else { "x86" },
            bitness: if wide { "64" } else { "32" },
            wow64: ua.contains("WOW64"),
            mobile: ua.contains("Mobile"),
        }
    }

    /// `navigator.platform`.
    pub fn nav_platform(&self) -> &'static str {
        if !self.strict {
            return "Win32";
        }
        match self.os {
            "macOS" => "MacIntel",
            "Linux" | "Chrome OS" => "Linux x86_64",
            "Android" => "Linux armv81",
            _ => "Win32",
        }
    }

    /// `navigator.vendor`.
    pub fn vendor(&self) -> &'static str {
        if !self.strict {
            return "";
        }
        match self.family {
            Family::Chromium => "Google Inc.",
            Family::WebKit => "Apple Computer, Inc.",
            _ => "",
        }
    }

    /// `navigator.productSub`.
    pub fn product_sub(&self) -> &'static str {
        if self.strict && self.family == Family::Gecko {
            "20100101"
        } else {
            "20030107"
        }
    }

    /// `navigator.oscpu`, which only Firefox has.
    pub fn oscpu(&self) -> &'static str {
        match self.os {
            "macOS" => "Intel Mac OS X 10.15",
            "Linux" | "Chrome OS" => "Linux x86_64",
            _ => "Windows NT 10.0; Win64; x64",
        }
    }

    /// Whether `navigator.oscpu` exists: not on the browsers it is not on.
    pub fn has_oscpu(&self) -> bool {
        !self.strict || self.family == Family::Gecko
    }

    /// Whether `navigator.userAgentData` exists and client hint headers are sent: Chromium's.
    pub fn has_ua_data(&self) -> bool {
        !self.strict || self.family == Family::Chromium
    }

    /// The brands list (UA-CH §5.1), with the major version or the full one. For a strict user agent
    /// of the `Chromium` family: the grease brand, the compatibility brand and the browser's own
    /// brand (none for a string that names no browser of its own), in the order the grease seeds.
    pub fn brands(&self, full: bool) -> Vec<(String, String)> {
        let version = |f: &str| {
            if full {
                f.to_owned()
            } else {
                f.split('.').next().unwrap_or(f).to_owned()
            }
        };
        if self.strict && self.family == Family::Chromium {
            let seed = self
                .compat
                .split('.')
                .next()
                .and_then(|m| m.parse::<usize>().ok())
                .unwrap_or(0);
            let own = match self.brand.as_str() {
                "Edge" => Some("Microsoft Edge"),
                "Chrome" => Some("Google Chrome"),
                PRODUCT => Some(PRODUCT),
                _ => None,
            };
            let mut list = vec![
                grease(seed, full),
                ("Chromium".to_owned(), version(&self.compat)),
            ];
            if let Some(own) = own.filter(|_| !self.unbranded) {
                list.push((own.to_owned(), version(&self.full)));
            }
            return shuffle(list, seed);
        }
        let grease = if full {
            (GREASE.0.to_owned(), "99.0.0.0".to_owned())
        } else {
            (GREASE.0.to_owned(), GREASE.1.to_owned())
        };
        vec![grease, (self.brand.clone(), version(&self.full))]
    }

    /// `uaFullVersion` (UA-CH §6.1): for the `Chromium` family the compatibility brand's full version,
    /// which the string's compatibility token carries; the browser's own version for the others.
    pub fn ua_full_version(&self) -> String {
        if self.family == Family::Chromium && !self.edge {
            self.compat.clone()
        } else {
            self.full.clone()
        }
    }

    /// The low-entropy client hint headers a Chromium browser sends with every request to a
    /// potentially trustworthy origin; none for any other.
    pub fn client_hint_headers(&self) -> Vec<(&'static str, String)> {
        if !(self.strict && self.family == Family::Chromium) {
            return Vec::new();
        }
        let list = self
            .brands(false)
            .iter()
            .map(|(b, v)| format!("\"{b}\";v=\"{v}\""))
            .collect::<Vec<_>>()
            .join(", ");
        vec![
            ("Sec-CH-UA", list),
            (
                "Sec-CH-UA-Mobile",
                if self.mobile { "?1" } else { "?0" }.to_owned(),
            ),
            ("Sec-CH-UA-Platform", format!("\"{}\"", self.os)),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_stored_and_sent_is_one_clean_line() {
        assert_eq!(
            sanitize("Mozilla/5.0 (X11)").as_deref(),
            Some("Mozilla/5.0 (X11)")
        );
        assert_eq!(
            sanitize("a\r\nX-Injected: 1").as_deref(),
            Some("aX-Injected: 1"),
            "no line break survives, so nothing starts a new header"
        );
        assert_eq!(
            sanitize("tab\there\u{0}nul\u{7f}del").as_deref(),
            Some("tabherenuldel")
        );
        assert_eq!(sanitize("café ünïcode ✓").as_deref(), Some("caf ncode"));
        assert_eq!(sanitize("  padded  ").as_deref(), Some("padded"));
        assert_eq!(sanitize(""), None);
        assert_eq!(sanitize("\r\n\t\u{1}"), None);
        assert_eq!(sanitize(&"x".repeat(5000)).unwrap().len(), MAX_LEN);
        // The header is made clean again from whatever the struct holds.
        let raw = UserAgent {
            value: "evil\r\nSet-Cookie: x=1".into(),
            strict: true,
            site_compat: false,
            unbranded: false,
        };
        assert!(!raw.header_value().contains(['\r', '\n']));
        assert_eq!(
            UserAgent {
                value: "\r\n".into(),
                strict: true,
                site_compat: false,
                unbranded: false,
            }
            .header_value(),
            DEFAULT_UA
        );
    }

    #[test]
    fn presets_say_one_browser_everywhere() {
        let ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(ids, ["default", "chrome", "edge", "firefox", "safari"]);
        for p in &PRESETS {
            assert_eq!(sanitize(p.value).as_deref(), Some(p.value), "{}", p.id);
        }
        let get = |id: &str| UserAgent::preset(id).unwrap().profile();
        let chrome = get("chrome");
        assert_eq!(chrome.family, Family::Chromium);
        assert_eq!(
            (chrome.brand.as_str(), chrome.major.as_str()),
            ("Chrome", "150")
        );
        assert!(chrome.has_ua_data() && !chrome.has_oscpu());
        assert_eq!(
            (chrome.vendor(), chrome.product_sub(), chrome.nav_platform()),
            ("Google Inc.", "20030107", "Win32")
        );
        let hints = chrome.client_hint_headers();
        assert_eq!(
            hints[0].1,
            "\"Not;A=Brand\";v=\"8\", \"Chromium\";v=\"150\", \"Google Chrome\";v=\"150\""
        );
        assert_eq!(hints[1].1, "?0");
        assert_eq!(hints[2].1, "\"Windows\"");
        let edge = get("edge");
        assert!(edge.edge && edge.has_ua_data());
        assert!(edge
            .brands(false)
            .iter()
            .any(|(b, v)| b == "Microsoft Edge" && v == "150"));
        let firefox = get("firefox");
        assert_eq!(firefox.family, Family::Gecko);
        assert!(!firefox.has_ua_data() && firefox.has_oscpu());
        assert_eq!((firefox.vendor(), firefox.product_sub()), ("", "20100101"));
        assert!(firefox.client_hint_headers().is_empty());
        let safari = get("safari");
        assert_eq!(safari.family, Family::WebKit);
        assert!(!safari.has_ua_data() && !safari.has_oscpu());
        assert_eq!(
            (safari.vendor(), safari.nav_platform(), safari.os),
            ("Apple Computer, Inc.", "MacIntel", "macOS")
        );
        assert!(safari.client_hint_headers().is_empty());
    }

    #[test]
    fn listed_sites_get_the_compatibility_string_with_the_same_profile() {
        let ua = UserAgent::default_with_site_compat();
        let compat = compat_value();
        assert_eq!(
            compat,
            format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{COMPAT_VERSION} Safari/537.36 {PRODUCT}/{VERSION}"
            )
        );
        // Everything else a site reads stays the default's, but for the brands: the browser's own is
        // left out.
        assert_eq!(
            Profile::parse(&compat, true),
            UserAgent::default().profile()
        );
        let mail = url::Url::parse("https://mail.google.com/").unwrap();
        let listed = ua.for_url(&mail).profile();
        assert!(listed.unbranded);
        let names = |p: &Profile| -> Vec<String> {
            let mut n: Vec<String> = p.brands(false).into_iter().map(|(b, _)| b).collect();
            n.sort();
            n
        };
        let mut expected = names(&UserAgent::default().profile());
        expected.retain(|b| b != PRODUCT);
        assert_eq!(names(&listed), expected);
        assert!(names(&listed).iter().any(|b| b == "Chromium"));
        assert_eq!(
            listed.ua_full_version(),
            UserAgent::default().profile().ua_full_version()
        );
        let other = url::Url::parse("https://example.com/").unwrap();
        assert!(!ua.for_url(&other).profile().unbranded);
        let at = |u: &str| ua.for_url(&url::Url::parse(u).unwrap()).value.clone();
        for listed in [
            "https://mail.google.com/mail/u/0/",
            "https://chat.google.com/u/0/app",
            "https://docs.google.com/document/d/x/edit",
            "https://drive.google.com/",
            "https://www.youtube.com/watch?v=x",
            "https://www.youtube.com/s/player/x/player_ias.vflset/en_US/base.js",
            "https://MAIL.google.com./x",
            "blob:https://mail.google.com/1234",
        ] {
            assert_eq!(at(listed), compat, "{listed}");
        }
        for other in [
            "https://google.com/",
            "https://www.google.com/search",
            "https://accounts.google.com/v3/signin",
            "https://challenges.cloudflare.com/",
            "https://mail.google.com.evil.example/",
            "https://evilmail.google.com/",
            "https://www.youtube.com.evil.example/",
            "data:text/html,x",
            "about:blank",
        ] {
            assert_eq!(at(other), DEFAULT_UA, "{other}");
        }
        // Off unless the window turns it on; never for a chosen string.
        let off = UserAgent::default();
        let mail = url::Url::parse("https://mail.google.com/").unwrap();
        assert_eq!(off.for_url(&mail).value, DEFAULT_UA);
        let mut chosen = UserAgent::chosen("Custom/1.0");
        chosen.site_compat = true;
        assert_eq!(chosen.for_url(&mail).value, "Custom/1.0");
        assert!(matches!(
            chosen.for_url(&mail),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn hbo_max_gets_the_edge_identity_in_the_header_and_the_hints() {
        let ua = UserAgent::default_with_site_compat();
        let edge = UserAgent::preset("edge").unwrap();
        let at = |u: &str| ua.for_url(&url::Url::parse(u).unwrap()).into_owned();
        for listed in [
            "https://hbomax.com/",
            "https://www.hbomax.com/play/assets/main.js",
            "https://play.hbomax.com/video/watch/x/y",
            "https://default.any-any.prd.api.hbomax.com/any/playback/v1/playbackInfo",
            "https://busy.any-any.prd.api.discomax.com/drm-proxy/x/key",
            "https://akm.eu.prd.media.max.com/x.mp4",
            "https://max.com/",
            "https://PLAY.HBOMax.com./x",
            "blob:https://play.hbomax.com/1234",
        ] {
            let got = at(listed);
            assert_eq!(got.value, edge.value, "{listed}");
            assert_eq!(got.header_value(), edge.header_value(), "{listed}");
            // The same identity everywhere a site reads it: the brands, the full version and the
            // client hint headers are the Edge preset's.
            assert_eq!(got.profile(), edge.profile(), "{listed}");
            let hints = got.profile().client_hint_headers();
            assert_eq!(hints, edge.profile().client_hint_headers());
            assert!(
                hints[0].1.contains("\"Microsoft Edge\";v=\"150\"")
                    && hints[0].1.contains("\"Chromium\";v=\"150\"")
                    && !hints[0].1.contains(PRODUCT),
                "{}",
                hints[0].1
            );
            assert_eq!(hints[1].1, "?0");
            assert_eq!(hints[2].1, "\"Windows\"");
        }
        let p = at("https://play.hbomax.com/").profile();
        assert!(p.edge && p.family == Family::Chromium && !p.unbranded);
        assert_eq!(p.os, "Windows");
        assert_eq!(p.ua_full_version(), "150.0.0.0");
        // The rest of `navigator` is the default's, so nothing a tab decided at start changes.
        let d = UserAgent::default().profile();
        assert_eq!(
            (p.vendor(), p.nav_platform(), p.product_sub()),
            (d.vendor(), d.nav_platform(), d.product_sub())
        );
        assert_eq!(
            (p.has_ua_data(), p.has_oscpu()),
            (d.has_ua_data(), d.has_oscpu())
        );
        for other in [
            "https://example.com/",
            "https://nothbomax.com/",
            "https://hbomax.com.evil.example/",
            "https://hbomax.co/",
            "https://minmax.com/",
            "https://max.co/",
            "https://discomax.com.evil.example/",
            "https://www.google.com/",
        ] {
            assert_eq!(at(other).value, DEFAULT_UA, "{other}");
        }
        // Off unless the window turns it on; a chosen preset or custom string wins.
        let page = url::Url::parse("https://play.hbomax.com/").unwrap();
        assert_eq!(UserAgent::default().for_url(&page).value, DEFAULT_UA);
        for mut chosen in [
            UserAgent::preset("chrome").unwrap(),
            UserAgent::preset("firefox").unwrap(),
            UserAgent::chosen("Custom/1.0"),
        ] {
            chosen.site_compat = true;
            assert_eq!(chosen.for_url(&page).value, chosen.value);
        }
    }

    #[test]
    fn the_default_names_snowlark_and_no_other_browser() {
        let d = UserAgent::default();
        assert_eq!(d.value, DEFAULT_UA);
        assert!(d.strict);
        assert_eq!(UserAgent::preset("default"), Some(d.clone()));
        assert_ne!(UserAgent::preset("chrome"), Some(d.clone()));
        // The engine's layout tokens and Snowlark's product token; no other browser's product token
        // (a bot check holds `Chrome/` to that browser's API surface, and a video site
        // reads `Safari/` without it as Safari and serves native HLS).
        assert_eq!(
            d.value,
            format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 {PRODUCT}/{VERSION}"
            )
        );
        for other in [
            "Chrome/",
            "Chromium/",
            "Edg/",
            "Firefox/",
            "Safari/",
            "Version/",
        ] {
            assert!(!d.value.contains(other), "{}", d.value);
        }
        let p = d.profile();
        assert_eq!(p.compat, COMPAT_VERSION);
        assert_eq!(p.family, Family::Chromium);
        assert_eq!((p.brand.as_str(), p.full.as_str()), (PRODUCT, VERSION));
        assert!(p.has_ua_data() && !p.has_oscpu());
        // HTML's navigator compatibility mode of the family.
        assert_eq!(
            (p.vendor(), p.nav_platform(), p.product_sub()),
            ("Google Inc.", "Win32", "20030107")
        );
        let major = VERSION.split('.').next().unwrap().to_owned();
        let brands = p.brands(false);
        assert_eq!(
            brands,
            vec![
                ("Not;A=Brand".to_owned(), "8".to_owned()),
                ("Chromium".to_owned(), "150".to_owned()),
                ("Snowlark".to_owned(), major.clone())
            ]
        );
        assert_eq!(
            p.brands(true),
            vec![
                ("Not;A=Brand".to_owned(), "8.0.0.0".to_owned()),
                ("Chromium".to_owned(), COMPAT_VERSION.to_owned()),
                ("Snowlark".to_owned(), VERSION.to_owned())
            ]
        );
        assert_eq!(p.ua_full_version(), COMPAT_VERSION);
        let hints = p.client_hint_headers();
        assert_eq!(
            hints[0],
            (
                "Sec-CH-UA",
                format!(
                    "\"Not;A=Brand\";v=\"8\", \"Chromium\";v=\"150\", \"Snowlark\";v=\"{major}\""
                )
            )
        );
        assert!(!hints[0].1.contains("Google"));
        assert_eq!(hints.len(), 3);
        // Firefox stays a preset, with Firefox's shape (no userAgentData, oscpu present).
        let f = UserAgent::preset("firefox").unwrap();
        assert!(f.value.contains("Firefox/150.0"));
        assert!(f.strict && !f.profile().has_ua_data() && f.profile().has_oscpu());
    }

    #[test]
    fn the_grease_brand_and_order_follow_the_release() {
        // What the family's browsers sent in two releases, by their user agent strings.
        let header = |ua: &str| {
            UserAgent::chosen(ua).profile().client_hint_headers()[0]
                .1
                .clone()
        };
        assert_eq!(
            header("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"),
            "\"Not_A Brand\";v=\"8\", \"Chromium\";v=\"120\", \"Google Chrome\";v=\"120\""
        );
        assert_eq!(
            header("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36"),
            "\"Chromium\";v=\"124\", \"Google Chrome\";v=\"124\", \"Not-A.Brand\";v=\"99\""
        );
        // A string that names no browser of its own lists the compatibility brand alone.
        let p = UserAgent::chosen("Mozilla/5.0 Chromium/121.0.0.0").profile();
        assert_eq!(
            p.brands(false)
                .iter()
                .map(|(b, _)| b.as_str())
                .collect::<Vec<_>>(),
            ["Chromium", "Not A(Brand"]
        );
    }

    #[test]
    fn custom_strings_are_read_by_their_tokens() {
        let p = UserAgent::chosen(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/123.0.6312.58 Safari/537.36",
        )
        .profile();
        assert_eq!(
            (p.family, p.os, p.nav_platform()),
            (Family::Chromium, "Linux", "Linux x86_64")
        );
        // The browser's own token alone is its family, with the compatibility brand's default version.
        let p = UserAgent::chosen(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Snowlark/0.1 Safari/537.36",
        )
        .profile();
        assert_eq!(
            (p.family, p.brand.as_str(), p.vendor()),
            (Family::Chromium, "Snowlark", "Google Inc.")
        );
        assert_eq!(
            p.brands(false),
            vec![
                ("Not;A=Brand".to_owned(), "8".to_owned()),
                ("Chromium".to_owned(), "150".to_owned()),
                ("Snowlark".to_owned(), "0".to_owned())
            ]
        );
        let p = UserAgent::chosen("MyBot/1.0").profile();
        assert_eq!(p.family, Family::Other);
        assert!(!p.has_ua_data() && !p.has_oscpu());
        assert_eq!(UserAgent::chosen("\r\n"), UserAgent::default());
    }
}
