// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A profile's downloads: what was saved, from where, when, and how it ended, for the downloads
//! page. The regular profile keeps them in its folder (`downloads.json`); an off-the-record
//! profile keeps them in memory ([`DownloadHistory::in_memory`]), so they go when it does (the
//! files stay).
//!
//! An entry keeps the file's path (and the partial file's while it is written), the chain of
//! URLs it came through (the last one served it), the referrer and the page of the tab it began
//! in, its type, its start and end, the bytes received and expected, and its state. A download
//! still in progress when the browser closed reads as interrupted at the next start.
//!
//! Removing entries (one, or all that are done) leaves the files alone and can be undone while
//! the browser runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How a download stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    InProgress,
    Complete,
    Cancelled,
    Interrupted,
}

/// Why a download stopped before it was complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Interrupt {
    /// The connection failed.
    Network,
    /// The server answered with an error.
    Server,
    /// The file could not be written.
    File,
    /// The browser closed while it ran.
    Shutdown,
}

impl Interrupt {
    /// What the downloads page says of it.
    pub fn text(self) -> &'static str {
        match self {
            Interrupt::Network => "Failed - Network error",
            Interrupt::Server => "Failed - Server problem",
            Interrupt::File => "Failed - Couldn't save the file",
            Interrupt::Shutdown => "Failed - The browser closed",
        }
    }
}

/// One download.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub id: u64,
    /// Where the file is saved.
    pub target_path: PathBuf,
    /// Where it is written while in progress (empty once it is done).
    #[serde(default, skip_serializing_if = "is_empty_path")]
    pub current_path: PathBuf,
    /// The URLs requested, redirects included: the last one served the file.
    pub url_chain: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub referrer: String,
    /// The page of the tab the download began in.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tab_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime_type: String,
    /// Milliseconds since the epoch.
    pub start_time: i64,
    #[serde(default)]
    pub end_time: i64,
    #[serde(default)]
    pub received_bytes: u64,
    /// 0 when the server did not say.
    #[serde(default)]
    pub total_bytes: u64,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt: Option<Interrupt>,
}

fn is_empty_path(p: &Path) -> bool {
    p.as_os_str().is_empty()
}

impl Download {
    /// The URL that served the file (the copied link).
    pub fn url(&self) -> &str {
        self.url_chain.last().map_or("", String::as_str)
    }

    /// The URL first requested.
    pub fn original_url(&self) -> &str {
        self.url_chain.first().map_or("", String::as_str)
    }

    /// The file's name.
    pub fn file_name(&self) -> String {
        self.target_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Whether `query` (words, each of which must be found, in any case and with or without
    /// accents: [`search_fold`]) matches the file's path or either URL.
    pub fn matches(&self, query: &str) -> bool {
        let path = search_fold(&self.target_path.to_string_lossy());
        let first = search_fold(self.original_url());
        let last = search_fold(self.url());
        query.split_whitespace().all(|w| {
            let w = search_fold(w);
            path.contains(&w) || first.contains(&w) || last.contains(&w)
        })
    }
}

/// Text as the downloads search compares it: lower case, decomposed (NFD) with its combining
/// marks left out, so "Café" and "cafe" are the same. The page's script folds what is typed the
/// same way.
pub fn search_fold(s: &str) -> String {
    let lower = s.to_lowercase();
    icu_normalizer::DecomposingNormalizerBorrowed::new_nfd()
        .normalize(&lower)
        .chars()
        .filter(|&c| !is_combining_mark(c))
        .collect()
}

/// The combining diacritical mark blocks (what the page's script also leaves out).
fn is_combining_mark(c: char) -> bool {
    matches!(c as u32, 0x300..=0x36F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// What a new download starts with.
#[derive(Clone, Debug, Default)]
pub struct Start {
    pub target_path: PathBuf,
    pub current_path: PathBuf,
    pub url_chain: Vec<String>,
    pub referrer: String,
    pub tab_url: String,
    pub mime_type: String,
    pub start_time: i64,
    pub total_bytes: u64,
}

/// Entries taken off the list together, to put back with Undo.
#[derive(Debug)]
struct Removal {
    entries: Vec<Download>,
    /// What the page's notice says of it.
    label: String,
    /// When (milliseconds since the epoch).
    at: i64,
}

/// How many removals Undo can take back.
const UNDO_DEPTH: usize = 16;

/// The downloads of a profile, oldest first.
#[derive(Debug, Default)]
pub struct DownloadHistory {
    file: Option<PathBuf>,
    entries: Vec<Download>,
    next_id: u64,
    dirty: bool,
    /// Bumped by every change, so the window knows when to show the page again.
    generation: u64,
    /// Downloads the user cancelled that their thread has not seen yet.
    cancels: HashSet<u64>,
    removals: Vec<Removal>,
    /// Download threads running.
    workers: usize,
    /// The generation the window last showed.
    shown: u64,
}

impl DownloadHistory {
    /// Downloads kept in memory only (an off-the-record profile's, tests).
    pub fn in_memory() -> DownloadHistory {
        DownloadHistory {
            next_id: 1,
            ..Default::default()
        }
    }

    /// The downloads in `file` (none when it is missing or damaged). One that was in progress when
    /// the browser closed is interrupted.
    pub fn load(file: &Path) -> DownloadHistory {
        let mut entries: Vec<Download> = std::fs::read_to_string(file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let mut dirty = false;
        for e in entries.iter_mut().filter(|e| e.state == State::InProgress) {
            e.state = State::Interrupted;
            e.interrupt = Some(Interrupt::Shutdown);
            dirty = true;
        }
        entries.sort_by_key(|e| e.start_time);
        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        DownloadHistory {
            file: Some(file.to_owned()),
            entries,
            next_id,
            dirty,
            ..Default::default()
        }
    }

    pub fn entries(&self) -> &[Download] {
        &self.entries
    }

    pub fn get(&self, id: u64) -> Option<&Download> {
        self.entries.iter().find(|e| e.id == id)
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut Download> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Changes with every change of the list or of an entry.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn changed(&mut self) {
        self.dirty = true;
        self.generation += 1;
    }

    /// The entries that match `query` (see [`Download::matches`]), newest first.
    pub fn query(&self, query: &str) -> Vec<&Download> {
        let mut out: Vec<&Download> = self.entries.iter().filter(|e| e.matches(query)).collect();
        out.sort_by(|a, b| b.start_time.cmp(&a.start_time).then(b.id.cmp(&a.id)));
        out
    }

    /// Whether any download is in progress.
    pub fn any_in_progress(&self) -> bool {
        self.entries.iter().any(|e| e.state == State::InProgress)
    }

    /// A download begins: its id.
    pub fn start(&mut self, s: Start) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        self.entries.push(Download {
            id,
            target_path: s.target_path,
            current_path: s.current_path,
            url_chain: s.url_chain,
            referrer: s.referrer,
            tab_url: s.tab_url,
            mime_type: s.mime_type,
            start_time: s.start_time,
            end_time: 0,
            received_bytes: 0,
            total_bytes: s.total_bytes,
            state: State::InProgress,
            interrupt: None,
        });
        self.changed();
        id
    }

    /// `received` bytes of a download are in.
    pub fn progress(&mut self, id: u64, received: u64) {
        if let Some(e) = self.get_mut(id).filter(|e| e.state == State::InProgress) {
            e.received_bytes = received;
            if e.total_bytes != 0 && received > e.total_bytes {
                e.total_bytes = 0;
            }
            self.generation += 1;
        }
    }

    /// A download ended: complete, cancelled or interrupted (`interrupt` says why).
    pub fn finish(&mut self, id: u64, state: State, interrupt: Option<Interrupt>, end_time: i64) {
        self.cancels.remove(&id);
        if let Some(e) = self.get_mut(id).filter(|e| e.state == State::InProgress) {
            e.state = state;
            e.interrupt = interrupt.filter(|_| state == State::Interrupted);
            e.end_time = end_time;
            e.current_path = PathBuf::new();
            if state == State::Complete {
                e.total_bytes = e.received_bytes;
            }
            self.changed();
        }
    }

    /// The user cancelled a download in progress: its thread stops at its next step.
    pub fn cancel(&mut self, id: u64) -> bool {
        let running = self.get(id).is_some_and(|e| e.state == State::InProgress);
        if running {
            self.cancels.insert(id);
        }
        running
    }

    /// Whether the user cancelled this download.
    pub fn is_cancelled(&self, id: u64) -> bool {
        self.cancels.contains(&id)
    }

    /// Takes entries off the list (not those in progress), as one step Undo puts back; the files
    /// stay. Whether any went.
    fn remove_where(&mut self, label: String, now: i64, f: impl Fn(&Download) -> bool) -> bool {
        let (gone, kept): (Vec<Download>, Vec<Download>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|e| e.state != State::InProgress && f(e));
        self.entries = kept;
        if gone.is_empty() {
            return false;
        }
        if self.removals.len() == UNDO_DEPTH {
            self.removals.remove(0);
        }
        self.removals.push(Removal {
            entries: gone,
            label,
            at: now,
        });
        self.changed();
        true
    }

    /// "Remove from list" on one entry.
    pub fn remove(&mut self, id: u64, now: i64) -> bool {
        let Some(name) = self.get(id).map(Download::file_name) else {
            return false;
        };
        self.remove_where(format!("Removed \u{2018}{name}\u{2019}"), now, |e| {
            e.id == id
        })
    }

    /// "Clear all": every entry that is not in progress.
    pub fn clear_all(&mut self, now: i64) -> bool {
        self.remove_where("Cleared all".into(), now, |_| true)
    }

    /// Deleting browsing data: the entries that began in `begin..end` (milliseconds since the
    /// epoch) and are not in progress leave the list for good (Undo cannot bring them back, nor
    /// those removed before); the files stay. Whether any went.
    pub fn delete_started_between(&mut self, begin: i64, end: i64) -> bool {
        let gone = |e: &Download| {
            e.state != State::InProgress && e.start_time >= begin && e.start_time < end
        };
        let before = self.entries.len();
        self.entries.retain(|e| !gone(e));
        let undo_had = self.removals.len();
        for r in &mut self.removals {
            r.entries.retain(|e| !gone(e));
        }
        self.removals.retain(|r| !r.entries.is_empty());
        let changed = self.entries.len() != before || self.removals.len() != undo_had;
        if changed {
            self.changed();
        }
        changed
    }

    /// Puts back what the last removal took.
    pub fn undo(&mut self) -> bool {
        let Some(r) = self.removals.pop() else {
            return false;
        };
        self.entries.extend(r.entries);
        self.entries.sort_by_key(|e| (e.start_time, e.id));
        self.changed();
        true
    }

    /// The last removal's notice and when it was made, while Undo can take it back.
    pub fn last_removal(&self) -> Option<(&str, i64)> {
        self.removals.last().map(|r| (r.label.as_str(), r.at))
    }

    /// A thread that runs a download began (it may still be asking where to save).
    pub fn worker_started(&mut self) {
        self.workers += 1;
    }

    pub fn worker_ended(&mut self) {
        self.workers = self.workers.saturating_sub(1);
    }

    /// Whether a download's thread is running.
    pub fn busy(&self) -> bool {
        self.workers > 0
    }

    /// Whether the list changed since the last call (the window then shows the page again).
    pub fn take_change(&mut self) -> bool {
        let changed = self.shown != self.generation;
        self.shown = self.generation;
        changed
    }

    /// Writes the file (atomically: a temporary file renamed over it) when something changed.
    pub fn save(&mut self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            self.dirty = false;
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

/// The time now, in milliseconds since the epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Marks a downloaded file as coming from the internet (its `Zone.Identifier` stream, as Windows'
/// attachment services read it): opening it then gets the system's checks. The URLs are kept only
/// when they are http(s), without their credentials; the source is `about:internet` otherwise (an
/// off-the-record profile passes empty URLs).
pub fn mark_from_internet(path: &Path, url: &str, referrer: &str) -> std::io::Result<()> {
    let clean = |u: &str| -> Option<String> {
        let mut u = snowlark_net::Url::parse(u).ok()?;
        if !matches!(u.scheme(), "http" | "https") {
            return None;
        }
        let _ = u.set_username("");
        let _ = u.set_password(None);
        Some(u.as_str().to_owned())
    };
    let mut text = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Some(r) = clean(referrer) {
        text.push_str(&format!("ReferrerUrl={r}\r\n"));
    }
    let host = clean(url).unwrap_or_else(|| "about:internet".into());
    text.push_str(&format!("HostUrl={host}\r\n"));
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    std::fs::write(PathBuf::from(stream), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(name: &str, url: &str, at: i64) -> Start {
        Start {
            target_path: PathBuf::from(format!("C:\\Downloads\\{name}")),
            current_path: PathBuf::from(format!("C:\\Downloads\\{name}.part")),
            url_chain: vec![url.into(), format!("{url}?served")],
            referrer: "https://ref.example/".into(),
            tab_url: "https://tab.example/".into(),
            mime_type: "application/zip".into(),
            start_time: at,
            total_bytes: 100,
        }
    }

    #[test]
    fn a_download_runs_ends_and_is_listed_newest_first() {
        let mut h = DownloadHistory::in_memory();
        let a = h.start(start("a.zip", "https://a.example/a.zip", 10));
        let b = h.start(start("b.pdf", "https://b.example/b.pdf", 20));
        assert_ne!(a, b);
        assert!(h.any_in_progress());
        let g = h.generation();
        h.progress(a, 40);
        assert!(h.generation() > g);
        assert_eq!(h.get(a).unwrap().received_bytes, 40);
        h.progress(a, 100);
        h.finish(a, State::Complete, None, 30);
        let e = h.get(a).unwrap();
        assert_eq!(
            (e.state, e.end_time, e.total_bytes),
            (State::Complete, 30, 100)
        );
        assert!(e.current_path.as_os_str().is_empty());
        // (a second ending changes nothing)
        h.finish(a, State::Cancelled, None, 40);
        assert_eq!(h.get(a).unwrap().state, State::Complete);
        h.finish(b, State::Interrupted, Some(Interrupt::Network), 50);
        assert_eq!(h.get(b).unwrap().interrupt, Some(Interrupt::Network));
        assert!(!h.any_in_progress());
        let ids: Vec<u64> = h.query("").iter().map(|e| e.id).collect();
        assert_eq!(ids, [b, a]);
        assert_eq!(h.get(b).unwrap().url(), "https://b.example/b.pdf?served");
        assert_eq!(h.get(b).unwrap().original_url(), "https://b.example/b.pdf");
        assert_eq!(h.get(b).unwrap().file_name(), "b.pdf");
    }

    #[test]
    fn the_search_matches_every_word_in_the_path_or_a_url() {
        let mut h = DownloadHistory::in_memory();
        h.start(start("Report.PDF", "https://files.example/x", 1));
        h.start(start("photo.jpg", "https://pics.example/report", 2));
        let names = |q: &str| -> Vec<String> { h.query(q).iter().map(|e| e.file_name()).collect() };
        assert_eq!(names("report"), ["photo.jpg", "Report.PDF"]);
        assert_eq!(names("REPORT pdf"), ["Report.PDF"]);
        assert_eq!(names("files.example"), ["Report.PDF"]);
        // (the folder is part of the path)
        assert_eq!(names("downloads").len(), 2);
        assert!(names("report zebra").is_empty());
        assert_eq!(names("  ").len(), 2);
    }

    #[test]
    fn removing_leaves_downloads_in_progress_and_undo_puts_entries_back() {
        let mut h = DownloadHistory::in_memory();
        let a = h.start(start("a.zip", "https://a.example/", 1));
        let b = h.start(start("b.zip", "https://b.example/", 2));
        let c = h.start(start("c.zip", "https://c.example/", 3));
        h.finish(a, State::Complete, None, 4);
        h.finish(b, State::Cancelled, None, 5);
        // c is still in progress: neither Remove nor Clear all takes it.
        assert!(!h.remove(c, 6));
        assert!(h.remove(a, 6));
        assert_eq!(h.last_removal(), Some(("Removed \u{2018}a.zip\u{2019}", 6)));
        assert!(h.clear_all(7));
        assert_eq!(h.last_removal(), Some(("Cleared all", 7)));
        assert_eq!(h.entries().iter().map(|e| e.id).collect::<Vec<_>>(), [c]);
        assert!(!h.clear_all(8));
        assert!(h.undo());
        assert_eq!(h.entries().iter().map(|e| e.id).collect::<Vec<_>>(), [b, c]);
        assert!(h.undo());
        assert_eq!(
            h.entries().iter().map(|e| e.id).collect::<Vec<_>>(),
            [a, b, c]
        );
        assert!(!h.undo());
        assert_eq!(h.last_removal(), None);
    }

    #[test]
    fn a_cancel_reaches_only_a_download_in_progress() {
        let mut h = DownloadHistory::in_memory();
        let a = h.start(start("a.zip", "https://a.example/", 1));
        assert!(!h.is_cancelled(a));
        assert!(h.cancel(a));
        assert!(h.is_cancelled(a));
        h.finish(a, State::Cancelled, None, 2);
        assert!(!h.is_cancelled(a));
        assert!(!h.cancel(a));
    }

    #[test]
    fn saves_and_loads_and_an_unfinished_download_reads_as_interrupted() {
        let dir = std::env::temp_dir().join(format!("snowlark-downloads-{}", std::process::id()));
        let file = dir.join("downloads.json");
        let _ = std::fs::remove_dir_all(&dir);
        let mut h = DownloadHistory::load(&file);
        assert!(h.entries().is_empty());
        let a = h.start(start("a.zip", "https://a.example/", 1));
        let b = h.start(start("b.zip", "https://b.example/", 2));
        h.progress(b, 7);
        h.finish(a, State::Complete, None, 3);
        h.save().unwrap();
        let mut back = DownloadHistory::load(&file);
        assert_eq!(back.get(a), h.get(a));
        let e = back.get(b).unwrap();
        assert_eq!(
            (e.state, e.interrupt, e.received_bytes),
            (State::Interrupted, Some(Interrupt::Shutdown), 7)
        );
        // New ids follow the old ones.
        assert_eq!(back.start(start("c.zip", "https://c.example/", 4)), b + 1);
        // A damaged file reads as none.
        std::fs::write(&file, "{").unwrap();
        assert!(DownloadHistory::load(&file).entries().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_downloaded_file_is_marked_as_from_the_internet() {
        let dir = std::env::temp_dir().join(format!("snowlark-motw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.exe");
        std::fs::write(&file, b"x").unwrap();
        let zone = |p: &Path| {
            let mut s = p.as_os_str().to_owned();
            s.push(":Zone.Identifier");
            std::fs::read_to_string(PathBuf::from(s)).unwrap()
        };
        mark_from_internet(&file, "https://u:p@a.example/a.exe", "https://b.example/").unwrap();
        assert_eq!(
            zone(&file),
            "[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://b.example/\r\nHostUrl=https://a.example/a.exe\r\n"
        );
        mark_from_internet(&file, "data:,x", "").unwrap();
        assert_eq!(
            zone(&file),
            "[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=about:internet\r\n"
        );
        // (the file itself is untouched)
        assert_eq!(std::fs::read(&file).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_window_hears_of_each_change_once() {
        let mut h = DownloadHistory::in_memory();
        assert!(!h.take_change());
        let a = h.start(start("a.zip", "https://a.example/", 10));
        assert!(h.take_change());
        assert!(!h.take_change());
        h.progress(a, 5);
        assert!(h.take_change());
        assert!(!h.busy());
        h.worker_started();
        assert!(h.busy());
        h.worker_ended();
        h.worker_ended();
        assert!(!h.busy());
    }
}
