// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The list of releases (GitHub's REST answer for a repository's releases) and which one is newer
//! than this build.
//!
//! A release of the browser has a zip named `snowlark-preview-<YYYY-MM-DD>-<commit>.zip` and, beside
//! it, `<that name>.sha256` with its SHA-256. Other releases of the repository (libraries for
//! building it) have neither and are not updates. Drafts are skipped; pre-releases are not (every
//! preview is one).

use serde::Deserialize;

#[derive(Deserialize)]
struct ApiRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// A moment as GitHub writes it (`2026-11-22T08:09:31Z`): year, month, day, hour, minute, second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Moment([u16; 6]);

impl Moment {
    /// An RFC 3339 time in UTC with whole seconds; `None` for anything else.
    pub fn parse(text: &str) -> Option<Moment> {
        let b = text.as_bytes();
        if b.len() != 20 || b[19] != b'Z' {
            return None;
        }
        let num = |from: usize, to: usize| -> Option<u16> {
            let s = text.get(from..to)?;
            s.bytes()
                .all(|c| c.is_ascii_digit())
                .then(|| s.parse().ok())
                .flatten()
        };
        let seps = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];
        if seps.iter().any(|&(i, c)| b[i] != c) {
            return None;
        }
        let m = Moment([
            num(0, 4)?,
            num(5, 7)?,
            num(8, 10)?,
            num(11, 13)?,
            num(14, 16)?,
            num(17, 19)?,
        ]);
        let [_, month, day, hour, minute, second] = m.0;
        ((1..=12).contains(&month)
            && (1..=31).contains(&day)
            && hour < 24
            && minute < 60
            && second < 61)
            .then_some(m)
    }
}

/// A build's release name, `snowlark-preview-<YYYY-MM-DD>-<commit>`: its date as (year, month, day)
/// and its commit. `None` for any other name.
pub fn release_date(name: &str) -> Option<((u16, u16, u16), &str)> {
    let rest = name.strip_prefix("snowlark-preview-")?;
    let (date, commit) = (rest.get(..10)?, rest.get(11..)?);
    if rest.as_bytes().get(10) != Some(&b'-')
        || commit.is_empty()
        || !commit.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    let m = Moment::parse(&format!("{date}T00:00:00Z"))?;
    Some(((m.0[0], m.0[1], m.0[2]), commit))
}

/// A release of the browser in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    /// Its page on the site.
    pub page: String,
    pub published: Moment,
    /// The build's release name (the zip's, without `.zip`).
    pub name: String,
    pub zip_url: String,
    pub zip_size: u64,
    pub sha256_url: String,
}

/// The releases of the browser in a releases list (JSON), in the list's order. An error for text
/// that is not a list.
pub fn parse(json: &[u8]) -> Result<Vec<Release>, String> {
    let list: Vec<ApiRelease> =
        serde_json::from_slice(json).map_err(|e| format!("not a list of releases: {e}"))?;
    let mut out = Vec::new();
    for r in list {
        if r.draft {
            continue;
        }
        let Some(published) = r.published_at.as_deref().and_then(Moment::parse) else {
            continue;
        };
        let Some(zip) = r.assets.iter().find(|a| {
            a.name
                .strip_suffix(".zip")
                .is_some_and(|n| release_date(n).is_some())
        }) else {
            continue;
        };
        let sha_name = format!("{}.sha256", zip.name);
        let Some(sha) = r.assets.iter().find(|a| a.name == sha_name) else {
            continue;
        };
        if !is_https_or_local(&zip.browser_download_url)
            || !is_https_or_local(&sha.browser_download_url)
        {
            continue;
        }
        out.push(Release {
            tag: r.tag_name,
            page: r.html_url,
            published,
            name: zip.name.trim_end_matches(".zip").to_owned(),
            zip_url: zip.browser_download_url.clone(),
            zip_size: zip.size,
            sha256_url: sha.browser_download_url.clone(),
        });
    }
    Ok(out)
}

/// A download address the updater follows (and a release page the window opens): https, or http to
/// this machine (a test's feed).
pub(crate) fn is_https_or_local(url: &str) -> bool {
    match snowlark_net::Url::parse(url) {
        Ok(u) => {
            u.scheme() == "https"
                || (u.scheme() == "http"
                    && matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
        }
        Err(_) => false,
    }
}

/// The release to update to from the build of release `own`: the newest one published after it.
/// When `own` is not in the list (taken down), the newest one of a later date than `own`'s. `None`
/// when there is none, or `own` is not a release name.
pub fn newer<'a>(releases: &'a [Release], own: &str) -> Option<&'a Release> {
    let (own_date, _) = release_date(own)?;
    let mine = releases.iter().find(|r| r.name == own);
    releases
        .iter()
        .filter(|r| r.name != own)
        .filter(|r| match mine {
            Some(m) => r.published > m.published,
            None => release_date(&r.name).is_some_and(|(d, _)| d > own_date),
        })
        .max_by_key(|r| r.published)
}

/// The SHA-256 in a checksum file: its first word, 64 hex digits (`sha256sum`'s format, or the
/// bare hash), lower-cased.
pub fn checksum(text: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(text).ok()?;
    let word = text.split_whitespace().next()?;
    (word.len() == 64 && word.bytes().all(|c| c.is_ascii_hexdigit()))
        .then(|| word.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the real list (2026-11-22), with the checksum files the releases have from now on.
    fn sample() -> String {
        let release = |tag: &str, name: &str, published: &str, sha: bool| {
            let base =
                format!("https://github.com/snowlark-browser/snowlark/releases/download/{tag}");
            let sha = if sha {
                format!(
                    r#",{{"name":"{name}.zip.sha256","size":90,"browser_download_url":"{base}/{name}.zip.sha256"}}"#
                )
            } else {
                String::new()
            };
            format!(
                r#"{{"tag_name":"{tag}","html_url":"https://github.com/snowlark-browser/snowlark/releases/tag/{tag}","draft":false,"prerelease":true,"published_at":"{published}","assets":[{{"name":"{name}.zip","size":46010193,"browser_download_url":"{base}/{name}.zip"}}{sha}]}}"#
            )
        };
        let v8 = r#"{"tag_name":"v8-158.0.0-pgo2","html_url":"x","draft":false,"prerelease":true,"published_at":"2026-11-19T14:06:58Z","assets":[{"name":"V8-NOTICES.txt","size":1,"browser_download_url":"https://github.com/x"}]}"#;
        format!(
            "[{},{},{},{v8},{}]",
            release(
                "preview-2026-11-24",
                "snowlark-preview-2026-11-24-0123abcd",
                "2026-11-24T09:00:00Z",
                true
            ),
            release(
                "preview-2026-11-22",
                "snowlark-preview-2026-11-22-c1cb0259",
                "2026-11-22T08:09:31Z",
                true
            ),
            release(
                "preview-2026-11-20",
                "snowlark-preview-2026-11-20-7c44a90b",
                "2026-11-20T11:45:49Z",
                false
            ),
            release(
                "preview-2026-11-19",
                "snowlark-preview-2026-11-19-7cf6ec3a",
                "2026-11-19T14:18:46Z",
                true
            ),
        )
    }

    #[test]
    fn the_list_gives_the_browser_releases_that_have_a_checksum() {
        let list = parse(sample().as_bytes()).unwrap();
        let names: Vec<&str> = list.iter().map(|r| r.name.as_str()).collect();
        // (the V8 library is no update, and the 20th has no checksum file)
        assert_eq!(
            names,
            [
                "snowlark-preview-2026-11-24-0123abcd",
                "snowlark-preview-2026-11-22-c1cb0259",
                "snowlark-preview-2026-11-19-7cf6ec3a"
            ]
        );
        let r = &list[1];
        assert_eq!(r.tag, "preview-2026-11-22");
        assert_eq!(r.zip_size, 46010193);
        assert!(r
            .sha256_url
            .ends_with("/snowlark-preview-2026-11-22-c1cb0259.zip.sha256"));
        assert!(parse(b"{\"message\":\"API rate limit exceeded\"}").is_err());
        assert!(parse(b"not json").is_err());
    }

    #[test]
    fn a_newer_release_is_one_published_after_this_builds() {
        let list = parse(sample().as_bytes()).unwrap();
        let next = |own: &str| newer(&list, own).map(|r| r.name.as_str());
        assert_eq!(
            next("snowlark-preview-2026-11-19-7cf6ec3a"),
            Some("snowlark-preview-2026-11-24-0123abcd")
        );
        assert_eq!(
            next("snowlark-preview-2026-11-22-c1cb0259"),
            Some("snowlark-preview-2026-11-24-0123abcd")
        );
        assert_eq!(next("snowlark-preview-2026-11-24-0123abcd"), None);
        // Not in the list: by date, so a build of the same day as the newest is not offered it.
        assert_eq!(
            next("snowlark-preview-2026-11-21-deadbeef"),
            Some("snowlark-preview-2026-11-24-0123abcd")
        );
        assert_eq!(next("snowlark-preview-2026-11-24-deadbeef"), None);
        // A build without a release name is never offered one.
        assert_eq!(next(""), None);
        assert_eq!(next("dev"), None);
    }

    #[test]
    fn a_later_release_of_the_same_day_is_newer_by_its_publication() {
        let mut list = parse(sample().as_bytes()).unwrap();
        let mut again = list[1].clone();
        again.name = "snowlark-preview-2026-11-22-ffff0000".into();
        again.published = Moment::parse("2026-11-22T18:00:00Z").unwrap();
        list.truncate(3);
        list.remove(0);
        list.insert(0, again);
        assert_eq!(
            newer(&list, "snowlark-preview-2026-11-22-c1cb0259").map(|r| r.name.as_str()),
            Some("snowlark-preview-2026-11-22-ffff0000")
        );
    }

    #[test]
    fn names_and_times_are_read_strictly() {
        assert_eq!(
            release_date("snowlark-preview-2026-11-22-c1cb0259"),
            Some(((2026, 11, 22), "c1cb0259"))
        );
        assert_eq!(release_date("snowlark-preview-2026-13-22-c1cb0259"), None);
        assert_eq!(release_date("snowlark-preview-2026-11-22"), None);
        assert_eq!(release_date("snowlark-preview-2026-11-22-xyz"), None);
        assert!(Moment::parse("2026-11-22T08:09:31Z").is_some());
        assert!(Moment::parse("2026-11-22T08:09:31+01:00").is_none());
        assert!(Moment::parse("2026-11-22 08:09:31Z").is_none());
        assert!(Moment::parse("2026-11-22T08:09:31Z") < Moment::parse("2026-11-22T08:10:00Z"));
    }

    #[test]
    fn a_checksum_file_gives_its_hash() {
        let h = "A".repeat(64);
        assert_eq!(
            checksum(format!("{h}  snowlark.zip\n").as_bytes()),
            Some("a".repeat(64))
        );
        assert_eq!(checksum(h.as_bytes()), Some("a".repeat(64)));
        assert_eq!(checksum(b"abc"), None);
        assert_eq!(checksum(b""), None);
    }

    #[test]
    fn downloads_go_over_https_or_to_this_machine() {
        assert!(is_https_or_local("https://github.com/a.zip"));
        assert!(is_https_or_local("http://127.0.0.1:8080/a.zip"));
        assert!(!is_https_or_local("http://example.com/a.zip"));
        assert!(!is_https_or_local("file:///C:/a.zip"));
    }
}
