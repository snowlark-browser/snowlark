// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Restoring the open tabs after the browser ended unexpectedly (a crash, a kill from Task Manager).
//!
//! A process that is killed runs no exit code, so the session is kept on disk while the browser
//! runs: [`Store`] gets a snapshot of each regular window (its tabs and its place on the screen)
//! whenever it changes, and a writer thread (never the
//! window's) writes it to `session-<pid>.json` through a temporary file and a rename, so a kill in
//! the middle of a write leaves the last good file. A real close (the window destroyed, or Windows
//! ending the session) writes the file once more with `clean` set. At the next start, the files of
//! processes that are gone and not clean are an unexpected end: the newest one that has a page worth
//! restoring becomes the *offer* (`session-offer.json`), which the window shows as a prompt; it
//! stays there until restored, dismissed or the browser is closed normally, so crashing again
//! before the answer does not lose it, and nothing is ever reopened without the user's click (a
//! page that crashes the browser cannot make it crash on every start).
//!
//! Every process has its own file and holds `session-<pid>.lock` open without sharing for as long
//! as it lives, so a second window or process never touches this one's file and a file whose lock
//! can be opened belongs to a process that is gone (a process id that is reused cannot be mistaken
//! for the owner).
//!
//! The process's regular windows share the file. A window closed while another stays open leaves
//! it; the last one to close leaves it as it is, marked clean. So a restore after a crash brings
//! back the windows that were open at the end, each with its tabs, where it was.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};
use snowlark_net::Url;
use snowlark_paint::Point;

use crate::session::{SessionEntry, Trail};

/// The file format's version (1: one window's tabs; 2: a list of windows).
const VERSION: u32 = 2;
/// Tabs and history entries read from a file: more is not a session of ours.
const MAX_TABS: usize = 500;
const MAX_ENTRIES: usize = 100;
const MAX_URL: usize = 16 * 1024;

/// The schemes a restored page may have (a file is trusted no more than that).
const SCHEMES: [&str; 6] = ["http", "https", "file", "about", "snowlark", "view-source"];

/// One entry of a tab's back/forward history.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedEntry {
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// Where the page was scrolled to (CSS px).
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
}

/// One tab: its history (the current entry is `index`), zoom and mute.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    pub entries: Vec<SavedEntry>,
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub zoom: f32,
    #[serde(default)]
    pub muted: bool,
}

impl SavedTab {
    /// A tab's history as it is now, the current entry scrolled to `scroll`; `None` when it has none.
    pub fn from_trail(trail: &Trail, scroll: Point) -> Option<SavedTab> {
        if trail.entries.is_empty() {
            return None;
        }
        let index = trail.index.min(trail.entries.len() - 1);
        let entries = trail
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let at = if i == index { scroll } else { e.scroll };
                SavedEntry {
                    url: e.url.to_string(),
                    title: e.title.clone(),
                    x: at.x,
                    y: at.y,
                }
            })
            .collect();
        Some(SavedTab {
            entries,
            index,
            zoom: 1.0,
            muted: false,
        })
    }

    /// The history a restored tab starts with (an entry that is not an address is dropped).
    pub fn to_trail(&self) -> Trail {
        let mut entries = Vec::new();
        let mut index = 0;
        for (i, e) in self.entries.iter().enumerate() {
            let Ok(url) = Url::parse(&e.url) else {
                continue;
            };
            if i <= self.index {
                index = entries.len();
            }
            let mut entry = SessionEntry::new(url);
            entry.title = e.title.clone();
            entry.scroll = Point::new(e.x, e.y);
            entries.push(entry);
        }
        Trail { entries, index }
    }

    pub fn current(&self) -> Option<&SavedEntry> {
        self.entries.get(self.index)
    }

    /// Whether the tab shows something worth reopening (not the start page).
    pub fn worth_restoring(&self) -> bool {
        self.current()
            .is_some_and(|e| e.url != "about:home" && e.url != "about:blank" && !e.url.is_empty())
    }
}

/// One regular window: its tabs, the index of the active one, and where it was on the screen.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub tabs: Vec<SavedTab>,
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub placement: Option<crate::window_state::SavedPlacement>,
}

impl SavedWindow {
    pub fn new(tabs: Vec<SavedTab>, active: usize) -> SavedWindow {
        SavedWindow {
            tabs,
            active,
            placement: None,
        }
    }

    /// The window's tabs with only the entries a restore may open (`None` when none is left):
    /// a tab whose current entry may not be opened goes, and the active one stays the active one.
    fn checked(mut self) -> Option<SavedWindow> {
        let mut active_url = None;
        let was_active = self.active;
        let mut tabs = Vec::new();
        for (n, t) in std::mem::take(&mut self.tabs).into_iter().enumerate() {
            let Some(t) = t.checked() else {
                continue;
            };
            if n == was_active {
                active_url = Some(tabs.len());
            }
            tabs.push(t);
        }
        if tabs.is_empty() {
            return None;
        }
        self.active = active_url.unwrap_or(0);
        self.tabs = tabs;
        Some(self)
    }
}

/// The regular windows of one browser process at one moment, in the order they opened.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionFile {
    pub version: u32,
    /// The browser was closed normally.
    pub clean: bool,
    /// When it was written (seconds since the Unix epoch); the newest unclean file is offered.
    pub saved: u64,
    pub pid: u32,
    pub windows: Vec<SavedWindow>,
}

/// A session file as read: version 1 had one window's `tabs` and `active` at the top.
#[derive(Deserialize)]
struct RawFile {
    version: u32,
    clean: bool,
    saved: u64,
    pid: u32,
    #[serde(default)]
    windows: Vec<SavedWindow>,
    #[serde(default)]
    tabs: Vec<SavedTab>,
    #[serde(default)]
    active: usize,
}

impl SessionFile {
    pub fn new(windows: Vec<SavedWindow>) -> SessionFile {
        SessionFile {
            version: VERSION,
            clean: false,
            saved: now(),
            pid: std::process::id(),
            windows,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// A session from a file's text; `None` for anything that is not a usable one (corrupt,
    /// another version, nothing in it). Entries that are not web, file or browser pages are
    /// dropped, a tab left with none is too, and so is a window left with no tab. A version 1
    /// file is one window.
    pub fn parse(text: &str) -> Option<SessionFile> {
        let raw: RawFile = serde_json::from_str(text).ok()?;
        let windows = match raw.version {
            1 => vec![SavedWindow::new(raw.tabs, raw.active)],
            VERSION => raw.windows,
            _ => return None,
        };
        if windows.iter().map(|w| w.tabs.len()).sum::<usize>() > MAX_TABS
            || windows
                .iter()
                .any(|w| w.tabs.iter().any(|t| t.entries.len() > MAX_ENTRIES))
        {
            return None;
        }
        let windows: Vec<SavedWindow> = windows
            .into_iter()
            .filter_map(SavedWindow::checked)
            .collect();
        if windows.is_empty() {
            return None;
        }
        Some(SessionFile {
            version: VERSION,
            clean: raw.clean,
            saved: raw.saved,
            pid: raw.pid,
            windows,
        })
    }

    /// Whether any tab has a page worth reopening.
    pub fn worth_offering(&self) -> bool {
        self.windows
            .iter()
            .any(|w| w.tabs.iter().any(SavedTab::worth_restoring))
    }

    /// How many tabs the session has, in all its windows.
    pub fn tab_count(&self) -> usize {
        self.windows.iter().map(|w| w.tabs.len()).sum()
    }
}

impl SavedTab {
    /// The tab with only the entries a restore may open; `None` when its current entry is not
    /// one of them.
    fn checked(mut self) -> Option<SavedTab> {
        // (a session saved before the rename: its internal pages under today's scheme)
        for e in &mut self.entries {
            if let std::borrow::Cow::Owned(url) = crate::address::renamed(&e.url) {
                e.url = url;
            }
        }
        let current = self.index.min(self.entries.len().saturating_sub(1));
        let mut kept = Vec::new();
        let mut index = 0;
        for (i, e) in self.entries.iter().enumerate() {
            if e.url.len() > MAX_URL || !allowed(&e.url) {
                continue;
            }
            if i <= current {
                index = kept.len();
            }
            kept.push(e.clone());
        }
        // (the current entry itself must be a page we may open)
        if kept.is_empty() || !self.entries.get(current).is_some_and(|e| allowed(&e.url)) {
            return None;
        }
        self.entries = kept;
        self.index = index;
        if !self.zoom.is_finite() || !(0.25..=5.0).contains(&self.zoom) {
            self.zoom = 1.0;
        }
        Some(self)
    }
}

fn allowed(url: &str) -> bool {
    url.split_once(':')
        .is_some_and(|(s, _)| SCHEMES.contains(&s.to_ascii_lowercase().as_str()))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Writes `bytes` to `path` so that a reader (or the next start after a kill) sees the old file or
/// the new one, never half of it: a temporary file in the same folder, flushed to disk, renamed
/// over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn file_of(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("session-{pid}.json"))
}

fn lock_of(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("session-{pid}.lock"))
}

/// The windows of a browser that restarted into an update ([`Store::close_for_restart`]).
pub fn restart_file(dir: &Path) -> PathBuf {
    dir.join("session-restart.json")
}

/// The windows a restart left for this start, taken (the file goes: they are opened once).
pub fn take_restart(dir: &Path) -> Option<SessionFile> {
    let path = restart_file(dir);
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    SessionFile::parse(&text)
}

/// The offer's file.
pub fn offer_file(dir: &Path) -> PathBuf {
    dir.join("session-offer.json")
}

/// Whether the process that owns `session-<pid>.*` still runs: it holds its lock file open
/// without sharing.
#[cfg(windows)]
fn owner_alive(dir: &Path, pid: u32) -> bool {
    match OpenOptions::new()
        .write(true)
        .share_mode(0)
        .open(lock_of(dir, pid))
    {
        Ok(_) => false,
        // ERROR_SHARING_VIOLATION (32), ERROR_ACCESS_DENIED (5)
        Err(e) => matches!(e.raw_os_error(), Some(32 | 5)),
    }
}

/// Whether the process that owns `session-<pid>.*` still runs: it holds an exclusive lock on its
/// lock file (`flock`, released when the process ends however it ends).
#[cfg(not(windows))]
fn owner_alive(dir: &Path, pid: u32) -> bool {
    let Ok(file) = OpenOptions::new().write(true).open(lock_of(dir, pid)) else {
        return false;
    };
    matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock))
}

/// This process's lock file, held for as long as the process lives ([`owner_alive`]).
fn take_lock(path: &Path) -> Option<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(windows)]
    options.share_mode(0);
    let file = options.open(path).ok()?;
    #[cfg(not(windows))]
    file.try_lock().ok()?;
    Some(file)
}

/// Looks for the session of a browser that ended unexpectedly and makes it the offer (or keeps the
/// offer already there); returns the offer. Files of finished processes are cleaned up.
pub fn find_offer(dir: &Path, own_pid: u32) -> Option<SessionFile> {
    let mut best: Option<SessionFile> = None;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(pid) = name
                .strip_prefix("session-")
                .and_then(|r| r.split('.').next())
                .and_then(|p| p.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == own_pid || owner_alive(dir, pid) {
                continue;
            }
            if name.ends_with(".json") {
                let session = fs::read_to_string(e.path())
                    .ok()
                    .and_then(|t| SessionFile::parse(&t));
                if let Some(s) = session {
                    if !s.clean
                        && s.worth_offering()
                        && best.as_ref().is_none_or(|b| s.saved >= b.saved)
                    {
                        best = Some(s);
                    }
                }
            }
            // (a finished process's files, whatever they held: temporary files, locks, clean sessions)
            let _ = fs::remove_file(e.path());
        }
    }
    let offer = offer_file(dir);
    if let Some(b) = best {
        let _ = write_atomic(&offer, b.to_json().as_bytes());
    }
    fs::read_to_string(&offer)
        .ok()
        .and_then(|t| SessionFile::parse(&t))
        .filter(SessionFile::worth_offering)
}

enum Msg {
    Save(SessionFile),
    /// The last write, marked clean; the thread ends.
    Close(SessionFile),
}

/// This process's session file and its writer thread: one for the regular windows of the
/// process, each window updating its own part (by an id of its own).
pub struct Store {
    dir: PathBuf,
    pid: u32,
    tx: Option<Sender<Msg>>,
    thread: Option<JoinHandle<()>>,
    /// The lock file, open for as long as the process lives.
    lock: Option<File>,
    /// The windows as last given, in the order they opened.
    windows: Vec<(u64, SavedWindow)>,
    /// The regular windows open now.
    open: Vec<u64>,
}

impl Store {
    /// Takes the profile folder's session files for this process. Returns the store and the offer
    /// (the session of an unexpected end, if there is one).
    pub fn open(dir: &Path) -> (Store, Option<SessionFile>) {
        let pid = std::process::id();
        let _ = fs::create_dir_all(dir);
        let lock = take_lock(&lock_of(dir, pid));
        let offer = find_offer(dir, pid);
        let (tx, rx) = channel::<Msg>();
        let path = file_of(dir, pid);
        let thread = std::thread::Builder::new()
            .name("snowlark session".into())
            .spawn(move || {
                while let Ok(mut m) = rx.recv() {
                    // (only the newest snapshot matters when several are waiting)
                    while let Ok(next) = rx.try_recv() {
                        let closing = matches!(m, Msg::Close(_));
                        if closing {
                            break;
                        }
                        m = next;
                    }
                    match m {
                        Msg::Save(s) => {
                            let _ = write_atomic(&path, s.to_json().as_bytes());
                        }
                        Msg::Close(mut s) => {
                            s.clean = true;
                            let _ = write_atomic(&path, s.to_json().as_bytes());
                            break;
                        }
                    }
                }
            })
            .ok();
        let store = Store {
            dir: dir.to_owned(),
            pid,
            tx: Some(tx),
            thread,
            lock,
            windows: Vec::new(),
            open: Vec::new(),
        };
        (store, offer)
    }

    /// Whether the file is still being kept (not closed yet).
    pub fn is_open(&self) -> bool {
        self.tx.is_some()
    }

    fn snapshot(&self) -> SessionFile {
        SessionFile::new(self.windows.iter().map(|(_, w)| w.clone()).collect())
    }

    fn write(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Save(self.snapshot()));
        }
    }

    /// Window `id`'s tabs and placement as they are now; the file is written (off the caller's
    /// thread) when they differ from what was last given.
    pub fn update(&mut self, id: u64, window: SavedWindow) {
        if !self.is_open() {
            return;
        }
        match self.windows.iter_mut().find(|(i, _)| *i == id) {
            Some((_, w)) if *w == window => return,
            Some((_, w)) => *w = window,
            None => self.windows.push((id, window)),
        }
        self.write();
    }

    /// Regular window `id` opened: it is part of the session until it closes.
    pub fn register(&mut self, id: u64) {
        if !self.open.contains(&id) {
            self.open.push(id);
        }
    }

    /// Regular window `id` closed. While another regular window stays open it is no longer part
    /// of the session (a restore after a crash does not bring it back); the last one to close
    /// closes the file normally, keeping every window it had.
    pub fn window_closed(&mut self, id: u64) {
        self.open.retain(|i| *i != id);
        if self.open.is_empty() {
            self.close_clean();
            return;
        }
        let before = self.windows.len();
        self.windows.retain(|(i, _)| *i != id);
        if self.windows.len() != before {
            self.write();
        }
    }

    /// The browser restarts (into an update): every window it has now is kept in the restart file
    /// ([`take_restart`]), which the next process opens without asking, and the file is closed as
    /// on a normal close. Later updates and closes change nothing.
    pub fn close_for_restart(&mut self) {
        if !self.is_open() {
            return;
        }
        let _ = write_atomic(
            &restart_file(&self.dir),
            self.snapshot().to_json().as_bytes(),
        );
        self.close_clean();
    }

    /// The browser is closing normally (its last regular window, or Windows ends the session):
    /// the file says so, with every window it still had, and the call returns once it is written.
    pub fn close_clean(&mut self) {
        let s = self.snapshot();
        let Some(tx) = self.tx.take() else {
            return;
        };
        let _ = tx.send(Msg::Close(s));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // An offer nobody answered is dropped with a normal close.
        let _ = fs::remove_file(offer_file(&self.dir));
        // The lock goes first so the file (clean) is a finished process's the next time.
        self.lock.take();
        let _ = fs::remove_file(lock_of(&self.dir, self.pid));
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // (dropped without a clean close: the thread ends, the file stays unclean)
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The offer is answered (restored or dismissed).
pub fn clear_offer(dir: &Path) {
    let _ = fs::remove_file(offer_file(dir));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("snowlark-restore-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn tab(urls: &[&str], index: usize) -> SavedTab {
        SavedTab {
            entries: urls
                .iter()
                .map(|u| SavedEntry {
                    url: (*u).into(),
                    title: format!("title of {u}"),
                    x: 0.0,
                    y: 12.5,
                })
                .collect(),
            index,
            zoom: 1.25,
            muted: true,
        }
    }

    fn session(tabs: Vec<SavedTab>, active: usize) -> SessionFile {
        SessionFile::new(vec![SavedWindow::new(tabs, active)])
    }

    /// The file as the writer thread leaves it, once it has something.
    fn written(path: &Path) -> String {
        for _ in 0..200 {
            let text = fs::read_to_string(path).unwrap_or_default();
            if !text.is_empty() {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        String::new()
    }

    /// Waits until the file holds what `ok` accepts.
    fn written_as(path: &Path, ok: impl Fn(&SessionFile) -> bool) -> SessionFile {
        for _ in 0..200 {
            if let Some(s) = fs::read_to_string(path)
                .ok()
                .and_then(|t| SessionFile::parse(&t))
                .filter(|s| ok(s))
            {
                return s;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the session file never got there");
    }

    fn placement(x: i32) -> crate::window_state::SavedPlacement {
        snowlark_paint::placement::Placement {
            x,
            y: 40,
            width: 1000,
            height: 700,
            maximized: false,
            dpi: 144,
        }
        .into()
    }

    /// A session file as a process that was killed would have left it, owned by `pid` (which is
    /// not running: no lock is held).
    fn dead_file(dir: &Path, pid: u32, s: &SessionFile, clean: bool) {
        let mut s = s.clone();
        s.pid = pid;
        s.clean = clean;
        fs::write(file_of(dir, pid), s.to_json()).unwrap();
        fs::write(lock_of(dir, pid), b"").unwrap();
    }

    #[test]
    fn the_file_format_round_trips() {
        let s = session(
            vec![
                tab(&["https://a.example/", "https://a.example/next"], 1),
                tab(&["about:home"], 0),
            ],
            1,
        );
        let back = SessionFile::parse(&s.to_json()).unwrap();
        assert_eq!(back, s);
        let tabs = &back.windows[0].tabs;
        assert_eq!(tabs[0].current().unwrap().url, "https://a.example/next");
        assert_eq!(tabs[0].entries[0].y, 12.5);
        assert!(tabs[0].muted && tabs[0].zoom == 1.25);
        assert!(back.worth_offering());
        // Only start pages: nothing worth restoring.
        assert!(!session(vec![tab(&["about:home"], 0)], 0).worth_offering());
    }

    #[test]
    fn a_tabs_history_goes_to_the_file_and_back() {
        let mut h = crate::session::SessionHistory::default();
        h.push(Url::parse("https://a.example/").unwrap());
        h.set_title("A");
        h.save_scroll(Point::new(0.0, 300.0));
        h.push(Url::parse("https://b.example/x").unwrap());
        h.set_title("B");
        let saved = SavedTab::from_trail(&h.trail(), Point::new(5.0, 77.0)).unwrap();
        assert_eq!(saved.index, 1);
        assert_eq!(saved.entries[0].y, 300.0);
        assert_eq!((saved.entries[1].x, saved.entries[1].y), (5.0, 77.0));
        let back = saved.to_trail();
        let mut r = crate::session::SessionHistory::from_trail(back);
        assert_eq!(r.current().unwrap().title, "B");
        assert_eq!(r.current().unwrap().scroll, Point::new(5.0, 77.0));
        assert_eq!(r.go(-1).unwrap().scroll.y, 300.0);
        assert!(SavedTab::from_trail(&Trail::default(), Point::ZERO).is_none());
        // An entry that is no address is dropped and the current one stays the current one.
        let mut odd = saved.clone();
        odd.entries.insert(
            0,
            SavedEntry {
                url: "not a url".into(),
                ..Default::default()
            },
        );
        odd.index = 2;
        let t = odd.to_trail();
        assert_eq!((t.entries.len(), t.index), (2, 1));
    }

    #[test]
    fn corrupt_or_foreign_files_are_no_session() {
        assert!(SessionFile::parse("").is_none());
        assert!(SessionFile::parse("{not json").is_none());
        assert!(SessionFile::parse("[1,2]").is_none());
        let s = session(vec![tab(&["https://a.example/"], 0)], 0);
        let cut = &s.to_json()[..30];
        assert!(SessionFile::parse(cut).is_none(), "a truncated write");
        let other = s.to_json().replace("\"version\":2", "\"version\":9");
        assert!(SessionFile::parse(&other).is_none());
        assert!(SessionFile::parse(&session(vec![], 0).to_json()).is_none());
    }

    #[test]
    fn only_pages_we_may_open_come_back() {
        let s = session(
            vec![
                tab(&["javascript:alert(1)"], 0),
                tab(
                    &[
                        "https://a.example/",
                        "data:text/html,x",
                        "file:///C:/x.html",
                    ],
                    2,
                ),
                tab(&["https://b.example/", "chrome-extension://x/"], 1),
            ],
            1,
        );
        let back = SessionFile::parse(&s.to_json()).unwrap();
        // The first and last tabs are gone (their current entry is not openable); the second keeps
        // its web and file entries, current still the file.
        let w = &back.windows[0];
        assert_eq!(w.tabs.len(), 1);
        let urls: Vec<&str> = w.tabs[0].entries.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(urls, ["https://a.example/", "file:///C:/x.html"]);
        assert_eq!(w.tabs[0].index, 1);
        assert_eq!(w.active, 0);
        // A bad zoom becomes 1.
        let mut z = session(vec![tab(&["https://a.example/"], 0)], 0);
        z.windows[0].tabs[0].zoom = 99.0;
        let z = SessionFile::parse(&z.to_json()).unwrap();
        assert_eq!(z.windows[0].tabs[0].zoom, 1.0);
    }

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temporary() {
        let d = dir("atomic");
        let p = d.join("x.json");
        write_atomic(&p, b"first").unwrap();
        write_atomic(&p, b"second").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "second");
        let names: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["x.json"]);
        // A failed write (the folder is gone) leaves nothing and reports it.
        assert!(write_atomic(&d.join("no").join("y.json"), b"z").is_err());
        // A kill in the middle of a write leaves a temporary file beside the good one: the good
        // file is still read,; the sweep of a finished process's files also removes such temporary files.
        fs::write(d.join("x.json.123.tmp"), b"hal").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "second");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unclean_end_is_offered_and_a_clean_one_is_not() {
        let d = dir("offer");
        let s = session(vec![tab(&["https://a.example/"], 0)], 0);
        // A clean file of a finished process: no offer, and it is swept.
        dead_file(&d, 4_000_001, &s, true);
        assert!(find_offer(&d, 1).is_none());
        assert!(!file_of(&d, 4_000_001).exists() && !lock_of(&d, 4_000_001).exists());
        // An unclean one: offered, moved to the offer file.
        dead_file(&d, 4_000_002, &s, false);
        let offer = find_offer(&d, 1).expect("an offer");
        assert_eq!(offer.windows, s.windows);
        assert!(offer_file(&d).exists() && !file_of(&d, 4_000_002).exists());
        // The offer stays across starts until answered, even when the next run (which crashed
        // before saving anything worth keeping) left only a start page.
        dead_file(
            &d,
            4_000_003,
            &session(vec![tab(&["about:home"], 0)], 0),
            false,
        );
        assert_eq!(find_offer(&d, 1).unwrap().windows, s.windows);
        // A newer unclean session replaces it.
        let mut newer = session(vec![tab(&["https://b.example/"], 0)], 0);
        newer.saved += 10;
        dead_file(&d, 4_000_004, &newer, false);
        assert_eq!(find_offer(&d, 1).unwrap().windows, newer.windows);
        // Answered: gone.
        clear_offer(&d);
        assert!(find_offer(&d, 1).is_none());
        // A corrupt file is no session.
        fs::write(file_of(&d, 4_000_005), "{\"version\":1,\"tabs\":[").unwrap();
        assert!(find_offer(&d, 1).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_running_process_files_are_left_alone() {
        let d = dir("alive");
        let s = session(vec![tab(&["https://a.example/"], 0)], 0);
        // Another process of the profile: its lock is held, so its (unclean, as it runs) file is
        // neither offered nor deleted.
        let held = take_lock(&lock_of(&d, 4_000_010)).unwrap();
        let mut other = s.clone();
        other.pid = 4_000_010;
        fs::write(file_of(&d, 4_000_010), other.to_json()).unwrap();
        assert!(owner_alive(&d, 4_000_010));
        assert!(find_offer(&d, 1).is_none());
        assert!(file_of(&d, 4_000_010).exists());
        drop(held);
        assert!(!owner_alive(&d, 4_000_010));
        assert!(
            find_offer(&d, 1).is_some(),
            "it died: now it is an unclean end"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_store_writes_unclean_until_a_clean_close() {
        let d = dir("store");
        let (mut store, offer) = Store::open(&d);
        assert!(offer.is_none());
        let pid = std::process::id();
        let tabs = vec![
            tab(&["https://a.example/"], 0),
            tab(&["https://b.example/"], 0),
        ];
        store.register(1);
        store.update(1, SavedWindow::new(tabs.clone(), 1));
        // The writer thread writes; wait for it.
        let path = file_of(&d, pid);
        let saved = SessionFile::parse(&written(&path)).expect("a session was written");
        assert!(!saved.clean);
        assert_eq!(saved.windows, [SavedWindow::new(tabs.clone(), 1)]);
        // Our own file is not an unexpected end of another process, even unclean.
        assert!(find_offer(&d, pid).is_none());
        // A second store in the folder (another window) leaves ours alone: the lock is ours.
        assert!(owner_alive(&d, pid));
        // Closing normally marks the file clean and the next start offers nothing.
        store.close_clean();
        let closed = SessionFile::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(closed.clean);
        assert_eq!(closed.windows[0].tabs, tabs);
        assert!(!owner_alive(&d, pid));
        assert!(find_offer(&d, 1).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_store_dropped_without_closing_is_an_unexpected_end() {
        let d = dir("killed");
        let (mut store, _) = Store::open(&d);
        store.update(
            1,
            SavedWindow::new(vec![tab(&["https://a.example/"], 0)], 0),
        );
        drop(store); // (the writer finishes what it was given; no clean mark)
        let offer = find_offer(&d, 1).expect("offered at the next start");
        assert_eq!(offer.tab_count(), 1);
        assert!(!offer.clean);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unanswered_offer_is_dropped_by_a_normal_close() {
        let d = dir("ignored");
        let s = session(vec![tab(&["https://a.example/"], 0)], 0);
        dead_file(&d, 4_000_020, &s, false);
        let (mut store, offer) = Store::open(&d);
        assert!(offer.is_some());
        store.close_clean();
        let (_store, offer) = Store::open(&d);
        assert!(offer.is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn two_windows_round_trip_with_their_placements() {
        let mut a = SavedWindow::new(
            vec![
                tab(&["https://a.example/"], 0),
                tab(&["https://a.example/2"], 0),
            ],
            1,
        );
        a.placement = Some(placement(-1200));
        let mut b = SavedWindow::new(vec![tab(&["https://b.example/"], 0)], 0);
        b.placement = Some(placement(300));
        let s = SessionFile::new(vec![a.clone(), b.clone()]);
        let back = SessionFile::parse(&s.to_json()).unwrap();
        assert_eq!(back.windows, [a, b]);
        assert_eq!(back.tab_count(), 3);
        // A window left with no tab it may open is dropped, the others stay.
        let only_bad = SavedWindow::new(vec![tab(&["javascript:x"], 0)], 0);
        let s = SessionFile::new(vec![only_bad, back.windows[1].clone()]);
        let back = SessionFile::parse(&s.to_json()).unwrap();
        assert_eq!(back.windows.len(), 1);
        assert_eq!(back.windows[0].tabs[0].entries[0].url, "https://b.example/");
    }

    #[test]
    fn a_version_1_file_reads_as_one_window() {
        let v1 = r#"{"version":1,"clean":false,"saved":5,"pid":7,"active":1,"tabs":[
            {"entries":[{"url":"https://a.example/","title":"A","x":0,"y":3}],"index":0,"zoom":1.0,"muted":false},
            {"entries":[{"url":"https://b.example/","title":"B"}],"index":0}]}"#;
        let s = SessionFile::parse(v1).expect("a version 1 session");
        assert_eq!((s.version, s.saved, s.pid, s.clean), (VERSION, 5, 7, false));
        assert_eq!(s.windows.len(), 1);
        let w = &s.windows[0];
        assert_eq!((w.tabs.len(), w.active, w.placement), (2, 1, None));
        assert_eq!(w.tabs[1].current().unwrap().title, "B");
        assert!(s.worth_offering());
        // Written again, it is version 2.
        assert!(s.to_json().contains("\"version\":2"));
    }

    #[test]
    fn a_window_closed_beside_another_leaves_the_session_and_the_last_keeps_it() {
        let d = dir("windows");
        let (mut store, _) = Store::open(&d);
        let path = file_of(&d, std::process::id());
        let a = SavedWindow::new(vec![tab(&["https://a.example/"], 0)], 0);
        let b = SavedWindow::new(vec![tab(&["https://b.example/"], 0)], 0);
        let c = SavedWindow::new(vec![tab(&["https://c.example/"], 0)], 0);
        for (id, w) in [(1, &a), (2, &b), (3, &c)] {
            store.register(id);
            store.update(id, w.clone());
        }
        written_as(&path, |s| s.windows.len() == 3);
        // Window 2 closes while 1 and 3 stay: it is gone from the file, which stays unclean.
        store.window_closed(2);
        let s = written_as(&path, |s| s.windows.len() == 2);
        assert!(!s.clean);
        assert_eq!(s.windows, [a.clone(), c.clone()]);
        assert!(store.is_open());
        // A window that never saved anything closes: nothing changes.
        store.register(4);
        store.window_closed(4);
        // Window 1 closes, then 3, the last: the file is closed clean, still with window 3.
        store.window_closed(1);
        written_as(&path, |s| s.windows == [c.clone()]);
        store.window_closed(3);
        assert!(!store.is_open());
        let s = SessionFile::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(s.clean);
        assert_eq!(s.windows, [c]);
        // Updates after the close write nothing.
        store.update(5, a);
        let again = SessionFile::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(again.windows, s.windows);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_restart_keeps_every_window_for_the_next_start_once() {
        let d = dir("restart");
        let (mut store, _) = Store::open(&d);
        let a = SavedWindow::new(vec![tab(&["https://a.example/"], 0)], 0);
        let b = SavedWindow::new(vec![tab(&["https://b.example/"], 0)], 0);
        for (id, w) in [(1, &a), (2, &b)] {
            store.register(id);
            store.update(id, w.clone());
        }
        store.close_for_restart();
        assert!(!store.is_open());
        // The windows closing afterwards change nothing; the file is clean, so nothing is offered.
        store.window_closed(1);
        store.window_closed(2);
        drop(store);
        assert!(find_offer(&d, 0).is_none());
        let s = take_restart(&d).unwrap();
        assert_eq!(s.windows, [a, b]);
        assert!(take_restart(&d).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_crash_with_two_windows_open_offers_both() {
        let d = dir("two-crash");
        let (mut store, _) = Store::open(&d);
        let a = SavedWindow::new(vec![tab(&["https://a.example/"], 0)], 0);
        let mut b = SavedWindow::new(vec![tab(&["https://b.example/"], 0)], 0);
        b.placement = Some(placement(500));
        store.register(1);
        store.register(2);
        store.update(1, a.clone());
        store.update(2, b.clone());
        drop(store);
        let offer = find_offer(&d, 1).expect("offered at the next start");
        assert_eq!(offer.windows, [a, b]);
        let _ = fs::remove_dir_all(&d);
    }
}
