// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pages visited (`history.json` in the profile): what the address bar suggests and completes
//! from. The file format is the C# engine's (`[{"Url", "Title", "Visits", "LastVisit"}]` with an
//! ISO 8601 UTC time), so its history can be imported as is; each entry also keeps the times of its
//! recent visits (`Times`), which the history page lists by day and clearing a time range removes.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_ENTRIES: usize = 5000;
/// The visit times kept per page (the oldest go first).
const MAX_TIMES: usize = 100;

/// One page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Entry {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub visits: u32,
    /// `YYYY-MM-DDTHH:MM:SS.fffffffZ`.
    #[serde(default)]
    pub last_visit: String,
    /// The times of the visits kept, oldest first (milliseconds since 1970, UTC). An entry from
    /// before these were kept has its last visit's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub times: Vec<i64>,
}

impl Entry {
    fn last_visit_secs(&self) -> i64 {
        parse_iso(&self.last_visit).unwrap_or(0)
    }

    /// Fills `times` of an entry written without them.
    fn normalize(&mut self) {
        if self.times.is_empty() {
            if let Some(s) = parse_iso(&self.last_visit) {
                self.times.push(s * 1000);
            }
        }
    }

    /// The domain the history page shows: the host without the scheme, path and a trivial
    /// subdomain (`www.`, `m.`); the scheme when there is no host.
    pub fn domain(&self) -> String {
        let host = self.host().to_ascii_lowercase();
        if host.is_empty() {
            let scheme = self.url.split(':').next().unwrap_or("");
            return format!("{scheme}:");
        }
        for trivial in ["www.", "m."] {
            if let Some(r) = host.strip_prefix(trivial) {
                if r.contains('.') {
                    return r.to_owned();
                }
            }
        }
        host
    }

    /// The host of the address (no port).
    fn host(&self) -> &str {
        let rest = match self.url.find("://") {
            Some(i) => &self.url[i + 3..],
            None => return "",
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = authority.rsplit('@').next().unwrap_or("");
        match host.rfind(':') {
            Some(i) if !host.ends_with(']') => &host[..i],
            _ => host,
        }
    }
}

/// A row of the history page: a page's visits on one local day.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayVisits<'a> {
    pub entry: &'a Entry,
    /// The local day (days since 1970-01-01 in local time).
    pub day: i64,
    /// The first and last visit times that day (milliseconds, UTC).
    pub first: i64,
    pub last: i64,
}

/// The browsing history.
#[derive(Debug, Default)]
pub struct History {
    file: Option<PathBuf>,
    entries: Vec<Entry>,
    dirty: bool,
}

impl History {
    /// A history kept in memory only.
    pub fn in_memory() -> History {
        History::default()
    }

    /// The history in `file` (empty when it is missing or damaged). When `file` doesn't exist yet
    /// and `import` does, that one's entries are taken over (and saved to `file` at the next save).
    pub fn load(file: &Path, import: Option<&Path>) -> History {
        let read = |p: &Path| -> Option<Vec<Entry>> {
            let text = std::fs::read_to_string(p).ok()?;
            serde_json::from_str(&text).ok()
        };
        let (mut entries, dirty) = match read(file) {
            Some(e) => (e, false),
            None if !file.exists() => match import.and_then(read) {
                Some(e) => (e, true),
                None => (Vec::new(), false),
            },
            None => (Vec::new(), false),
        };
        entries.iter_mut().for_each(Entry::normalize);
        let before = entries.len();
        entries.retain(|e| kept(&e.url));
        let dirty = dirty || entries.len() != before;
        History {
            file: Some(file.to_owned()),
            entries,
            dirty,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// A page was shown (`count`: a new visit) or its title arrived. Internal pages aren't kept.
    pub fn visit(&mut self, url: &str, title: &str, count: bool) {
        self.visit_at(url, title, count, SystemTime::now());
    }

    pub(crate) fn visit_at(&mut self, url: &str, title: &str, count: bool, now: SystemTime) {
        if !kept(url) {
            return;
        }
        let i = match self.entries.iter().position(|e| e.url == url) {
            Some(i) => i,
            None => {
                self.entries.push(Entry {
                    url: url.to_owned(),
                    title: String::new(),
                    visits: 0,
                    last_visit: String::new(),
                    times: Vec::new(),
                });
                self.entries.len() - 1
            }
        };
        let e = &mut self.entries[i];
        if !title.is_empty() && e.title != title {
            e.title = title.to_owned();
            self.dirty = true;
        }
        if count {
            e.visits += 1;
            e.last_visit = format_iso(now);
            let ms = now
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            let at = e.times.partition_point(|&t| t <= ms);
            e.times.insert(at, ms);
            if e.times.len() > MAX_TIMES {
                e.times.remove(0);
            }
            self.dirty = true;
        }
        if self.entries.len() > MAX_ENTRIES + 500 {
            self.entries
                .sort_by_key(|e| std::cmp::Reverse(e.last_visit_secs()));
            self.entries.truncate(MAX_ENTRIES);
        }
    }

    /// Entries for what is typed: addresses that start with it first (a site's front page before
    /// its deeper pages), then ones whose address or title contains it; more visited, then more
    /// recent first.
    pub fn suggest(&self, typed: &str, max: usize) -> Vec<&Entry> {
        let t = typed.trim().to_lowercase();
        if t.is_empty() {
            return Vec::new();
        }
        let bare = stripped(&t).to_owned();
        let mut scored: Vec<(&Entry, u8, bool)> = self
            .entries
            .iter()
            .filter_map(|e| {
                let s = stripped(&e.url).to_lowercase();
                let score = if s.starts_with(&bare) {
                    3
                } else if e.url.to_lowercase().contains(&t) || e.title.to_lowercase().contains(&t) {
                    1
                } else {
                    return None;
                };
                let front = match s.find('/') {
                    None => true,
                    Some(i) => i == s.len() - 1,
                };
                Some((e, score, score == 3 && front))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then(b.2.cmp(&a.2))
                .then(b.0.visits.cmp(&a.0.visits))
                .then(b.0.last_visit_secs().cmp(&a.0.last_visit_secs()))
        });
        scored.into_iter().take(max).map(|x| x.0).collect()
    }

    /// The completion of `typed` the address bar shows inline: the best suggestion's address
    /// without its scheme and `www.`, when that starts with what's typed.
    pub fn completion(&self, typed: &str) -> Option<String> {
        let t = typed.trim();
        if t.is_empty() || t.contains(' ') {
            return None;
        }
        let best = self.suggest(t, 1).into_iter().next()?;
        let candidates = [best.url.as_str(), stripped(&best.url)];
        candidates.iter().find_map(|c| {
            let c = c.strip_suffix('/').unwrap_or(c);
            (c.len() > t.len()
                && c.is_char_boundary(t.len())
                && c[..t.len()].eq_ignore_ascii_case(t))
            .then(|| c.to_owned())
        })
    }

    /// The history page's rows for `query`: each page once per local day it was visited on, the
    /// most recent first. `offset` gives local time's offset from UTC at a time (both in
    /// milliseconds). The query's words must all be in the address or the title (case ignored);
    /// a word `host:<name>` keeps the pages of that host and its subdomains.
    pub fn by_day(&self, query: &str, offset: &dyn Fn(i64) -> i64) -> Vec<DayVisits<'_>> {
        let mut host = None;
        let mut words = Vec::new();
        for w in query.split_whitespace() {
            match w.strip_prefix("host:") {
                Some(h) if host.is_none() => host = Some(h.to_ascii_lowercase()),
                _ => words.push(w.to_lowercase()),
            }
        }
        let mut rows = Vec::new();
        for e in &self.entries {
            if let Some(h) = &host {
                let own = e.host().to_ascii_lowercase();
                if !(own == *h
                    || own
                        .strip_suffix(h.as_str())
                        .is_some_and(|s| s.ends_with('.')))
                {
                    continue;
                }
            }
            if !words.is_empty() {
                let url = e.url.to_lowercase();
                let title = e.title.to_lowercase();
                if !words
                    .iter()
                    .all(|w| url.contains(w.as_str()) || title.contains(w.as_str()))
                {
                    continue;
                }
            }
            let mut i = 0;
            while i < e.times.len() {
                let day = local_day(e.times[i], offset);
                let mut j = i + 1;
                while j < e.times.len() && local_day(e.times[j], offset) == day {
                    j += 1;
                }
                rows.push(DayVisits {
                    entry: e,
                    day,
                    first: e.times[i],
                    last: e.times[j - 1],
                });
                i = j;
            }
        }
        rows.sort_by(|a, b| b.last.cmp(&a.last).then(a.entry.url.cmp(&b.entry.url)));
        rows
    }

    /// Removes the visits of `url` from `first` to `last` (inclusive, milliseconds); a page with
    /// no visit left goes. Returns whether anything was removed.
    pub fn remove_visits(&mut self, url: &str, first: i64, last: i64) -> bool {
        let Some(i) = self.entries.iter().position(|e| e.url == url) else {
            return false;
        };
        self.remove_where(|e, t| e == i && (first..=last).contains(&t))
    }

    /// Removes every visit from `begin` (inclusive) to `end` (exclusive, milliseconds); pages
    /// with no visit left go. Returns whether anything was removed.
    pub fn remove_between(&mut self, begin: i64, end: i64) -> bool {
        self.remove_where(|_, t| (begin..end).contains(&t))
    }

    fn remove_where(&mut self, mut gone: impl FnMut(usize, i64) -> bool) -> bool {
        let mut changed = false;
        for (i, e) in self.entries.iter_mut().enumerate() {
            let before = e.times.len();
            e.times.retain(|&t| !gone(i, t));
            let removed = before - e.times.len();
            if removed == 0 {
                continue;
            }
            changed = true;
            e.visits = e.visits.saturating_sub(removed as u32);
            if let Some(&t) = e.times.last() {
                e.last_visit = format_iso(UNIX_EPOCH + Duration::from_millis(t.max(0) as u64));
            }
        }
        if changed {
            self.entries.retain(|e| !e.times.is_empty());
            self.dirty = true;
        }
        changed
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Writes the file (atomically: a temporary file renamed over it) when something changed.
    pub fn save(&mut self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        if !self.dirty {
            return Ok(());
        }
        let json = serde_json::to_string(&self.entries).map_err(std::io::Error::other)?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = file.with_extension("json.tmp");
        std::fs::write(&temp, json)?;
        std::fs::rename(&temp, file)?;
        self.dirty = false;
        Ok(())
    }
}

/// Whether a page at `url` goes into the history: not the browser's own pages, a page's source,
/// `about:`, `javascript:` or `data:` addresses.
fn kept(url: &str) -> bool {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    !url.is_empty()
        && !matches!(
            scheme.as_str(),
            "about" | "data" | "javascript" | "snowlark" | "view-source"
        )
}

/// The address without its scheme and `www.`, as the address bar completes it.
pub fn stripped(url: &str) -> &str {
    let rest = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case("www.") {
        &rest[4..]
    } else {
        rest
    }
}

/// The local day of a time (days since 1970-01-01 in local time).
pub fn local_day(t: i64, offset: &dyn Fn(i64) -> i64) -> i64 {
    (t + offset(t)).div_euclid(86_400_000)
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Howard Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn format_iso(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = d.as_secs() as i64;
    let (y, m, day) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:07}Z",
        s / 3600,
        s / 60 % 60,
        s % 60,
        d.subsec_nanos() / 100
    )
}

/// Seconds since 1970 of `YYYY-MM-DDTHH:MM:SS[.f…]Z`.
fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let days = days_from_civil(n(0..4)?, n(5..7)? as u32, n(8..10)? as u32);
    Some(days * 86_400 + n(11..13)? * 3600 + n(14..16)? * 60 + n(17..19)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn iso_times_round_trip() {
        let t = at(1_793_300_000);
        let s = format_iso(t);
        assert_eq!(s.len(), "2026-10-29T12:54:29.1384997Z".len());
        assert_eq!(parse_iso(&s), Some(1_793_300_000));
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_iso("nonsense"), None);
    }

    #[test]
    fn reads_the_csharp_format() {
        let json = r#"[{"Url":"https://example.com/","Title":"Example Domain","Visits":34,"LastVisit":"2026-10-29T12:54:29.1384997Z"}]"#;
        let entries: Vec<Entry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries[0].url, "https://example.com/");
        assert_eq!(entries[0].visits, 34);
        let back = serde_json::to_string(&entries).unwrap();
        assert!(back.contains("\"LastVisit\":\"2026-10-29T12:54:29.1384997Z\""));
    }

    #[test]
    fn visits_and_titles() {
        let mut h = History::in_memory();
        h.visit_at("https://a.example/", "", true, at(10));
        h.visit_at("https://a.example/", "A", false, at(20));
        h.visit_at("https://a.example/", "", true, at(30));
        h.visit_at("about:home", "Start", true, at(30));
        // (nor the browser's own pages, nor a page's source)
        h.visit_at("snowlark://history/", "History", true, at(30));
        h.visit_at("view-source:https://a.example/", "", true, at(30));
        assert_eq!(h.entries().len(), 1);
        assert_eq!(h.entries()[0].visits, 2);
        assert_eq!(h.entries()[0].title, "A");
        assert_eq!(h.entries()[0].last_visit_secs(), 30);
    }

    #[test]
    fn suggestions_rank_prefix_front_pages_then_visits() {
        let mut h = History::in_memory();
        h.visit_at(
            "https://news.ycombinator.com/item?id=1",
            "Item",
            true,
            at(50),
        );
        for _ in 0..3 {
            h.visit_at("https://news.ycombinator.com/", "Hacker News", true, at(40));
        }
        h.visit_at("https://www.newsweek.com/", "Newsweek", true, at(60));
        h.visit_at("https://example.com/news", "Example news", true, at(70));
        let urls: Vec<&str> = h
            .suggest("news", 10)
            .iter()
            .map(|e| e.url.as_str())
            .collect();
        assert_eq!(
            urls,
            [
                "https://news.ycombinator.com/",
                "https://www.newsweek.com/",
                "https://news.ycombinator.com/item?id=1",
                "https://example.com/news",
            ]
        );
        assert_eq!(h.completion("news"), Some("news.ycombinator.com".into()));
        assert_eq!(
            h.completion("https://news.y"),
            Some("https://news.ycombinator.com".into())
        );
        assert_eq!(h.completion("news z"), None);
        assert!(h.suggest("  ", 5).is_empty());
        // Title matches count too.
        assert_eq!(
            h.suggest("hacker", 5)[0].url,
            "https://news.ycombinator.com/"
        );
    }

    #[test]
    fn saves_and_imports() {
        let dir = std::env::temp_dir().join(format!("snowlark-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.json");
        std::fs::write(
            &old,
            r#"[{"Url":"https://flingup.com/","Title":"FlingUp","Visits":36,"LastVisit":"2026-10-29T13:01:11.089814Z"}]"#,
        )
        .unwrap();
        let file = dir.join("rust").join("history.json");
        let mut h = History::load(&file, Some(&old));
        assert_eq!(h.entries().len(), 1);
        assert!(h.is_dirty());
        h.visit("https://example.com/", "Example", true);
        h.save().unwrap();
        let again = History::load(&file, None);
        assert_eq!(again.entries().len(), 2);
        assert!(!again.is_dirty());
        // A damaged file starts empty and isn't replaced by the import.
        std::fs::write(&file, "{oops").unwrap();
        assert!(History::load(&file, Some(&old)).entries().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stripping() {
        assert_eq!(stripped("https://www.example.com/a"), "example.com/a");
        assert_eq!(
            stripped("http://news.ycombinator.com/"),
            "news.ycombinator.com/"
        );
        assert_eq!(stripped("example.com"), "example.com");
    }

    const HOUR: u64 = 3600;
    const DAY: u64 = 86_400;

    #[test]
    fn the_page_lists_each_page_once_per_local_day_newest_first() {
        let mut h = History::in_memory();
        // Day 20,000 (UTC): a at 10:00 and 11:00, b at 12:00; day 20,001: a at 01:00.
        let d = 20_000 * DAY;
        h.visit_at("https://a.example/", "A", true, at(d + 10 * HOUR));
        h.visit_at("https://a.example/", "A", true, at(d + 11 * HOUR));
        h.visit_at("https://www.b.example/x", "B", true, at(d + 12 * HOUR));
        h.visit_at("https://a.example/", "A", true, at(d + DAY + HOUR));
        let utc = |_: i64| 0;
        let rows: Vec<(&str, i64, i64, i64)> = h
            .by_day("", &utc)
            .iter()
            .map(|r| (r.entry.url.as_str(), r.day, r.first / 1000, r.last / 1000))
            .collect();
        let s = |x: u64| x as i64;
        assert_eq!(
            rows,
            [
                (
                    "https://a.example/",
                    20_001,
                    s(d + DAY + HOUR),
                    s(d + DAY + HOUR)
                ),
                (
                    "https://www.b.example/x",
                    20_000,
                    s(d + 12 * HOUR),
                    s(d + 12 * HOUR)
                ),
                (
                    "https://a.example/",
                    20_000,
                    s(d + 10 * HOUR),
                    s(d + 11 * HOUR)
                ),
            ]
        );
        // Two hours behind UTC, the 01:00 visit was on the day before.
        let behind = |_: i64| -2 * 3_600_000;
        let days: Vec<i64> = h.by_day("", &behind).iter().map(|r| r.day).collect();
        assert_eq!(days, [20_000, 20_000]);
        assert_eq!(h.by_day("", &behind)[0].first / 1000, s(d + 10 * HOUR));
        // Every word, in the address or the title; `host:` keeps a host and its subdomains.
        assert_eq!(h.by_day("b X", &utc).len(), 1);
        assert_eq!(h.by_day("b zzz", &utc).len(), 0);
        assert_eq!(h.by_day("host:b.example", &utc).len(), 1);
        assert_eq!(h.by_day("host:example", &utc).len(), 3);
        assert_eq!(h.by_day("host:a.example a", &utc).len(), 2);
        assert_eq!(h.by_day("host:xa.example", &utc).len(), 0);
        assert_eq!(h.entries()[1].domain(), "b.example");
    }

    #[test]
    fn removing_a_days_visits_keeps_the_others_and_drops_an_empty_page() {
        let mut h = History::in_memory();
        let d = 20_000 * DAY;
        h.visit_at("https://a.example/", "A", true, at(d + 10 * HOUR));
        h.visit_at("https://a.example/", "A", true, at(d + 11 * HOUR));
        h.visit_at("https://a.example/", "A", true, at(d + DAY + HOUR));
        h.visit_at("https://b.example/", "B", true, at(d + 12 * HOUR));
        let ms = |x: u64| x as i64 * 1000;
        assert!(h.remove_visits("https://a.example/", ms(d + 10 * HOUR), ms(d + 11 * HOUR)));
        let a = &h.entries()[0];
        assert_eq!(a.times, [ms(d + DAY + HOUR)]);
        assert_eq!(a.visits, 1);
        assert!(h.is_dirty());
        assert!(!h.remove_visits("https://a.example/", 0, ms(d)));
        assert!(!h.remove_visits("https://nothing.example/", 0, i64::MAX));
        assert!(h.remove_visits("https://b.example/", 0, i64::MAX));
        assert_eq!(h.entries().len(), 1);
        // The address bar no longer offers a page that is gone.
        assert!(h.suggest("b.example", 5).is_empty());
    }

    #[test]
    fn a_time_range_removes_its_visits_and_moves_the_last_visit_back() {
        let mut h = History::in_memory();
        let d = 20_000 * DAY;
        h.visit_at("https://a.example/", "A", true, at(d));
        h.visit_at("https://a.example/", "A", true, at(d + 2 * HOUR));
        h.visit_at("https://b.example/", "B", true, at(d + 2 * HOUR));
        let ms = |x: u64| x as i64 * 1000;
        assert!(h.remove_between(ms(d + HOUR), i64::MAX));
        assert_eq!(h.entries().len(), 1);
        assert_eq!(h.entries()[0].last_visit_secs(), d as i64);
        assert_eq!(h.entries()[0].visits, 1);
        // The end is exclusive.
        assert!(!h.remove_between(0, ms(d)));
        assert!(h.remove_between(0, ms(d) + 1));
        assert!(h.entries().is_empty());
    }

    #[test]
    fn an_entry_written_without_times_has_its_last_visits() {
        let dir = std::env::temp_dir().join(format!("snowlark-history-t-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("history.json");
        std::fs::write(
            &file,
            r#"[{"Url":"https://a.example/","Title":"A","Visits":3,"LastVisit":"1970-01-02T00:00:00Z"},
                {"Url":"snowlark://settings/","Title":"Settings","Visits":1,"LastVisit":"1970-01-02T00:00:00Z"}]"#,
        )
        .unwrap();
        let mut h = History::load(&file, None);
        assert_eq!(h.entries()[0].times, [86_400_000]);
        // A page of the browser's own, kept by an older build, goes.
        assert_eq!(h.entries().len(), 1);
        assert!(h.is_dirty());
        h.visit_at("https://a.example/", "A", true, at(2 * DAY));
        h.save().unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(r#""Times":[86400000,172800000]"#), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
