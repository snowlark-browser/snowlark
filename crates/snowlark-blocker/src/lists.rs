// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The filter lists: the ones uBlock Origin enables by default (its `assets/assets.json`), the ones
//! it offers off by default, and the user's own, with what the updater knows of each copy kept in
//! the profile (`lists.json`, the texts under `lists/`).
//!
//! The lists are downloaded at run time from their canonical addresses; none is in the binary.
//! Licences: EasyList and EasyPrivacy are dual GPL-3.0 / CC BY-SA 3.0; uBlock Origin's lists are
//! GPL-3.0; Peter Lowe's list is free for personal use under its terms; the Online Malicious URL
//! Blocklist is CC0 (URLhaus data). See THIRD-PARTY.md.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::BlockerConfig;
use crate::engine::ListFormat;

/// A list of the catalog.
#[derive(Clone, Copy, Debug)]
pub struct ListInfo {
    /// uBlock Origin's asset id.
    pub id: &'static str,
    pub title: &'static str,
    pub group: &'static str,
    /// The canonical address, then mirrors tried when it fails.
    pub urls: &'static [&'static str],
    pub format: ListFormat,
    /// uBlock Origin's own lists: their trusted scriptlets may run.
    pub trusted: bool,
    pub default_on: bool,
    /// How often to update when the list says nothing (`! Expires:`), in hours (uBlock Origin's
    /// `updateAfter`, else its default of 5 days).
    pub update_after_hours: u64,
}

const DAYS: u64 = 24;

/// The lists uBlock Origin ships with, on and off as it has them.
pub const CATALOG: &[ListInfo] = &[
    ListInfo {
        id: "ublock-filters",
        title: "uBlock filters - Ads",
        group: "uBlock Origin",
        urls: &[
            "https://ublockorigin.github.io/uAssets/filters/filters.txt",
            "https://ublockorigin.github.io/uAssetsCDN/filters/filters.min.txt",
            "https://cdn.jsdelivr.net/gh/uBlockOrigin/uAssetsCDN@main/filters/filters.min.txt",
        ],
        format: ListFormat::Filters,
        trusted: true,
        default_on: true,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "ublock-badware",
        title: "uBlock filters - Badware risks",
        group: "uBlock Origin",
        urls: &[
            "https://ublockorigin.github.io/uAssets/filters/badware.txt",
            "https://ublockorigin.github.io/uAssetsCDN/filters/badware.min.txt",
            "https://cdn.jsdelivr.net/gh/uBlockOrigin/uAssetsCDN@main/filters/badware.min.txt",
        ],
        format: ListFormat::Filters,
        trusted: true,
        default_on: true,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "ublock-privacy",
        title: "uBlock filters - Privacy",
        group: "uBlock Origin",
        urls: &[
            "https://ublockorigin.github.io/uAssets/filters/privacy.txt",
            "https://ublockorigin.github.io/uAssetsCDN/filters/privacy.min.txt",
            "https://cdn.jsdelivr.net/gh/uBlockOrigin/uAssetsCDN@main/filters/privacy.min.txt",
        ],
        format: ListFormat::Filters,
        trusted: true,
        default_on: true,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "ublock-unbreak",
        title: "uBlock filters - Unbreak",
        group: "uBlock Origin",
        urls: &[
            "https://ublockorigin.github.io/uAssets/filters/unbreak.txt",
            "https://ublockorigin.github.io/uAssetsCDN/filters/unbreak.min.txt",
            "https://cdn.jsdelivr.net/gh/uBlockOrigin/uAssetsCDN@main/filters/unbreak.min.txt",
        ],
        format: ListFormat::Filters,
        trusted: true,
        default_on: true,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "ublock-quick-fixes",
        title: "uBlock filters - Quick fixes",
        group: "uBlock Origin",
        urls: &[
            "https://ublockorigin.github.io/uAssets/filters/quick-fixes.txt",
            "https://ublockorigin.github.io/uAssetsCDN/filters/quick-fixes.min.txt",
            "https://cdn.jsdelivr.net/gh/uBlockOrigin/uAssetsCDN@main/filters/quick-fixes.min.txt",
        ],
        format: ListFormat::Filters,
        trusted: true,
        default_on: true,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "easylist",
        title: "EasyList",
        group: "Ads",
        urls: &[
            "https://ublockorigin.github.io/uAssets/thirdparties/easylist.txt",
            "https://cdn.jsdelivr.net/gh/uBlockOrigin/uAssetsCDN@main/thirdparties/easylist.txt",
            "https://ublockorigin.pages.dev/thirdparties/easylist.txt",
        ],
        format: ListFormat::Filters,
        trusted: false,
        default_on: true,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "easyprivacy",
        title: "EasyPrivacy",
        group: "Privacy",
        urls: &[
            "https://ublockorigin.github.io/uAssets/thirdparties/easyprivacy.txt",
            "https://cdn.jsdelivr.net/gh/uBlockOrigin/uAssetsCDN@main/thirdparties/easyprivacy.txt",
            "https://ublockorigin.pages.dev/thirdparties/easyprivacy.txt",
        ],
        format: ListFormat::Filters,
        trusted: false,
        default_on: true,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "urlhaus-1",
        title: "Online Malicious URL Blocklist",
        group: "Malware protection, security",
        urls: &[
            "https://malware-filter.gitlab.io/urlhaus-filter/urlhaus-filter-vivaldi-online.txt",
            "https://curbengh.github.io/malware-filter/urlhaus-filter-vivaldi-online.txt",
        ],
        format: ListFormat::Filters,
        trusted: false,
        default_on: true,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "plowe-0",
        title: "Peter Lowe's Ad and tracking server list",
        group: "Multipurpose",
        urls: &[
            "https://pgl.yoyo.org/adservers/serverlist.php?hostformat=hosts&showintro=1&mimetype=plaintext",
        ],
        format: ListFormat::Hosts,
        trusted: false,
        default_on: true,
        update_after_hours: 13 * DAYS,
    },
    ListInfo {
        id: "ublock-annoyances",
        title: "uBlock filters - Annoyances",
        group: "Annoyances",
        urls: &[
            "https://ublockorigin.github.io/uAssets/filters/annoyances.txt",
            "https://ublockorigin.github.io/uAssetsCDN/filters/annoyances.min.txt",
        ],
        format: ListFormat::Filters,
        trusted: true,
        default_on: false,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "ublock-cookies-easylist",
        title: "uBlock filters - Cookie notices",
        group: "Annoyances",
        urls: &["https://ublockorigin.github.io/uAssets/filters/annoyances-cookies.txt"],
        format: ListFormat::Filters,
        trusted: true,
        default_on: false,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "easylist-annoyances",
        title: "EasyList - Other annoyances",
        group: "Annoyances",
        urls: &["https://ublockorigin.github.io/uAssets/thirdparties/easylist-annoyances.txt"],
        format: ListFormat::Filters,
        trusted: false,
        default_on: false,
        update_after_hours: 5 * DAYS,
    },
    ListInfo {
        id: "fanboy-cookiemonster",
        title: "EasyList - Cookie notices",
        group: "Annoyances",
        urls: &["https://ublockorigin.github.io/uAssets/thirdparties/easylist-cookies.txt"],
        format: ListFormat::Filters,
        trusted: false,
        default_on: false,
        update_after_hours: 5 * DAYS,
    },
];

/// A list to compile: from the catalog or the user's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListSpec {
    pub id: String,
    pub title: String,
    pub urls: Vec<String>,
    pub format: ListFormat,
    pub trusted: bool,
    pub update_after_hours: u64,
}

/// The id of a user's list: `custom-` and a hash of its address.
pub fn custom_id(url: &str) -> String {
    // FNV-1a, stable across builds.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in url.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("custom-{h:016x}")
}

/// Whether a catalog list is on under `config`.
pub fn is_enabled(info: &ListInfo, config: &BlockerConfig) -> bool {
    config
        .lists
        .get(info.id)
        .copied()
        .unwrap_or(info.default_on)
}

/// The lists `config` turns on, in catalog order, then the user's.
pub fn enabled_lists(config: &BlockerConfig) -> Vec<ListSpec> {
    let mut out: Vec<ListSpec> = CATALOG
        .iter()
        .filter(|i| is_enabled(i, config))
        .map(|i| ListSpec {
            id: i.id.to_owned(),
            title: i.title.to_owned(),
            urls: i.urls.iter().map(|u| (*u).to_owned()).collect(),
            format: i.format,
            trusted: i.trusted,
            update_after_hours: i.update_after_hours,
        })
        .collect();
    for url in &config.custom_lists {
        out.push(ListSpec {
            id: custom_id(url),
            title: url.clone(),
            urls: vec![url.clone()],
            format: ListFormat::Filters,
            trusted: false,
            update_after_hours: 5 * DAYS,
        });
    }
    out
}

/// A hosts file rather than filters: most of its first rules are `0.0.0.0 host` / `127.0.0.1 host`.
pub fn looks_like_hosts(text: &str) -> bool {
    let mut hosts = 0;
    let mut other = 0;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
        .take(50)
    {
        if line.starts_with("0.0.0.0 ")
            || line.starts_with("127.0.0.1 ")
            || line.starts_with("0.0.0.0\t")
            || line.starts_with("127.0.0.1\t")
            || line.starts_with(":: ")
        {
            hosts += 1;
        } else {
            other += 1;
        }
    }
    hosts > other
}

/// What the updater knows of its copy of a list.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ListState {
    /// The address the copy came from.
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// When the copy was last checked successfully (Unix seconds).
    pub checked: u64,
    /// How long the copy is good for (seconds), from its `! Expires:`.
    pub expires: u64,
    pub bytes: usize,
    pub lines: usize,
    /// The last failure, and how many in a row.
    pub error: Option<String>,
    pub failures: u32,
    /// When the last attempt failed (Unix seconds).
    pub failed_at: u64,
    /// The files the list includes (`!#include name`), with their ETags.
    pub includes: BTreeMap<String, Option<String>>,
}

impl ListState {
    /// When the list should be checked again (Unix seconds): when the copy expires, or after a
    /// failure an hour, doubling with each failure in a row up to a day.
    pub fn due(&self, has_copy: bool) -> u64 {
        if self.failures > 0 {
            let backoff = (3600u64 << self.failures.min(6).saturating_sub(1)).min(86_400);
            return self.failed_at + backoff;
        }
        if !has_copy {
            return 0;
        }
        self.checked + self.expires.max(3600)
    }
}

/// The lists' copies in the profile: `lists.json` and `lists/<id>.txt`.
pub struct ListStore {
    dir: PathBuf,
    pub states: BTreeMap<String, ListState>,
    /// The lists and filters the saved engine was compiled from ([`ListStore::signature`]).
    pub built: String,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Saved {
    states: BTreeMap<String, ListState>,
    built: String,
}

impl ListStore {
    pub fn open(dir: &Path) -> ListStore {
        let saved: Saved = std::fs::read(dir.join("lists.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        ListStore {
            dir: dir.to_owned(),
            states: saved.states,
            built: saved.built,
        }
    }

    pub fn save(&self) {
        let saved = Saved {
            states: self.states.clone(),
            built: self.built.clone(),
        };
        let path = self.dir.join("lists.json");
        let tmp = self.dir.join("lists.json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&saved).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    fn text_path(&self, id: &str) -> PathBuf {
        self.dir.join("lists").join(format!("{id}.txt"))
    }

    /// The saved copy of a list.
    pub fn text(&self, id: &str) -> Option<String> {
        std::fs::read(self.text_path(id))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    pub fn has_text(&self, id: &str) -> bool {
        self.text_path(id).is_file()
    }

    pub fn write_text(&self, id: &str, text: &str) -> std::io::Result<()> {
        self.write_file(&self.text_path(id), text)
    }

    fn write_file(&self, path: &Path, text: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(path.parent().expect("lists/"))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    /// A list's own file as downloaded, before its includes are put in (`part` "" for the list,
    /// else the name of an included file).
    fn part_path(&self, id: &str, part: &str) -> PathBuf {
        let name: String = part
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join("lists").join(id).join(if name.is_empty() {
            "main.txt".to_owned()
        } else {
            format!("include-{name}")
        })
    }

    pub fn part(&self, id: &str, part: &str) -> Option<String> {
        std::fs::read(self.part_path(id, part))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    pub fn write_part(&self, id: &str, part: &str, text: &str) -> std::io::Result<()> {
        self.write_file(&self.part_path(id, part), text)
    }

    /// What an engine compiled from `lists` and `custom_filters` is made of (to tell whether the
    /// saved one is current): the ids and when each copy was fetched, and the user's filters.
    pub fn signature(&self, lists: &[ListSpec], custom_filters: &str) -> String {
        let mut s = String::new();
        for l in lists {
            let st = self.states.get(&l.id);
            s.push_str(&format!(
                "{}:{}:{};",
                l.id,
                st.map_or(0, |s| s.bytes),
                st.and_then(|s| s.etag.clone())
                    .or_else(|| st.and_then(|s| s.last_modified.clone()))
                    .unwrap_or_default()
            ));
        }
        s.push_str(&format!(
            "custom:{}",
            crate::lists::custom_id(custom_filters)
        ));
        s
    }
}

/// How long a list says its copy is good for (`! Expires: 5 days`), in seconds.
pub fn expires_of(text: &str) -> Option<u64> {
    use adblock::lists::ExpiresInterval;
    match adblock::lists::read_list_metadata(text).expires? {
        ExpiresInterval::Hours(h) => Some(h as u64 * 3600),
        ExpiresInterval::Days(d) => Some(d as u64 * 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_lists_are_ublock_origins() {
        let on: Vec<String> = enabled_lists(&BlockerConfig::default())
            .into_iter()
            .map(|l| l.id)
            .collect();
        assert_eq!(
            on,
            [
                "ublock-filters",
                "ublock-badware",
                "ublock-privacy",
                "ublock-unbreak",
                "ublock-quick-fixes",
                "easylist",
                "easyprivacy",
                "urlhaus-1",
                "plowe-0"
            ]
        );
        let mut c = BlockerConfig::default();
        c.lists.insert("easylist".into(), false);
        c.lists.insert("ublock-annoyances".into(), true);
        c.custom_lists.push("https://example.com/list.txt".into());
        let ids: Vec<String> = enabled_lists(&c).into_iter().map(|l| l.id).collect();
        assert!(!ids.contains(&"easylist".to_owned()));
        assert!(ids.contains(&"ublock-annoyances".to_owned()));
        assert_eq!(
            ids.last().unwrap(),
            &custom_id("https://example.com/list.txt")
        );
    }

    #[test]
    fn expiry_and_backoff() {
        assert_eq!(
            expires_of("! Title: X\n! Expires: 6 days (update frequency)\n||a^"),
            Some(6 * 86_400)
        );
        assert_eq!(expires_of("! Expires: 12 hours\n"), Some(12 * 3600));
        assert_eq!(expires_of("||a^\n"), None);
        let mut s = ListState {
            checked: 1000,
            expires: 7200,
            ..Default::default()
        };
        assert_eq!(s.due(true), 8200);
        assert_eq!(s.due(false), 0);
        s.failures = 1;
        s.failed_at = 5000;
        assert_eq!(s.due(true), 5000 + 3600);
        s.failures = 3;
        assert_eq!(s.due(true), 5000 + 4 * 3600);
        s.failures = 30;
        assert_eq!(s.due(true), 5000 + 86_400);
    }

    #[test]
    fn hosts_files_are_recognized() {
        assert!(looks_like_hosts(
            "# hosts\n0.0.0.0 a.example\n0.0.0.0 b.example\n"
        ));
        assert!(!looks_like_hosts("! filters\n||a.example^\n##.ad\n"));
    }

    #[test]
    fn store_keeps_texts_and_states() {
        let dir =
            std::env::temp_dir().join(format!("snowlark-blocker-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = ListStore::open(&dir);
        s.write_text("easylist", "||a^\n").unwrap();
        s.states.insert(
            "easylist".into(),
            ListState {
                etag: Some("\"x\"".into()),
                bytes: 5,
                ..Default::default()
            },
        );
        s.built = "sig".into();
        s.save();
        let s = ListStore::open(&dir);
        assert_eq!(s.text("easylist").as_deref(), Some("||a^\n"));
        assert_eq!(s.states["easylist"].etag.as_deref(), Some("\"x\""));
        assert_eq!(s.built, "sig");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
