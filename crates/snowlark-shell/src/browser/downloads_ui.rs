// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of downloads: saving a navigation's download where the user says (on a
//! thread of its own, recorded in the profile's download history as it runs), the downloads page
//! (Ctrl+J) and what its buttons ask for, and showing the page again while downloads change.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use snowlark_net::{FetchEvent, NetClient, Request, ResponseHead, ResponseStream, Url};
use snowlark_paint::window::{self, Window};

use super::Browser;
use crate::downloads::{mark_from_internet, now_ms, DownloadHistory, Interrupt, Start, State};
use crate::downloads_page::{action, Action, DOWNLOADS_PAGE};

/// The timer that shows the downloads page again while downloads run.
pub(super) const TIMER_DOWNLOADS: usize = 20;

/// How often the page shows a running download's progress (milliseconds).
const TICK_MS: u32 = 1000;

impl Browser {
    /// Ctrl+J, the menu's "Downloads": the tab showing the downloads page, or the page in the
    /// active tab when that is a new tab's start page, or in a new tab.
    pub(super) fn open_downloads(&mut self) {
        self.open_own_page("snowlark://downloads", DOWNLOADS_PAGE);
    }

    /// A navigation whose response is a download (`tab_host::is_download`): saved where the user
    /// says (the Save As dialog and the body run on a thread of their own). `tab_url` is the page
    /// of the tab it began in.
    pub(super) fn save_download(
        &mut self,
        window: &Window,
        download: crate::tab_host::Download,
        tab_url: String,
    ) {
        let crate::tab_host::Download { url, head, stream } = download;
        let name = super::context_menu::download_file_name(&url, &head);
        let mut chain: Vec<String> = head
            .url_list
            .iter()
            .map(|u| u.as_str().to_owned())
            .collect();
        if chain.is_empty() {
            chain.push(url.as_str().to_owned());
        }
        let job = Job {
            name,
            chain,
            referrer: head
                .referrer
                .as_ref()
                .map_or(String::new(), |r| r.as_str().to_owned()),
            tab_url,
        };
        self.spawn_download(window, job, Fetch::Started(Box::new((head, stream))));
    }

    /// "Retry" on a cancelled or failed download: its URL again, saved where the user says (a new
    /// entry).
    fn retry_download(&mut self, window: &Window, id: u64) {
        let job = {
            let h = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
            let Some(e) = h
                .get(id)
                .filter(|e| matches!(e.state, State::Cancelled | State::Interrupted))
            else {
                return;
            };
            Job {
                name: e.file_name(),
                chain: vec![e.url().to_owned()],
                referrer: e.referrer.clone(),
                tab_url: e.tab_url.clone(),
            }
        };
        let Ok(url) = Url::parse(job.url()) else {
            return;
        };
        self.spawn_download(window, job, Fetch::Later(self.tab_ctx.net.clone(), url));
    }

    fn spawn_download(&mut self, window: &Window, job: Job, fetch: Fetch) {
        let owner = window.handle();
        let downloads = self.downloads.clone();
        downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .worker_started();
        window.set_timer(TIMER_DOWNLOADS, TICK_MS);
        let spawned = std::thread::Builder::new()
            .name("snowlark download".into())
            .spawn({
                let downloads = downloads.clone();
                move || {
                    if let Some(path) = ask_where(owner, &job.name) {
                        run(&downloads, job, path, fetch);
                    }
                    downloads
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .worker_ended();
                }
            });
        if spawned.is_err() {
            downloads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .worker_ended();
        }
    }

    /// The downloads timer: tabs showing the downloads page show what changed; the timer stops
    /// when no download runs.
    pub(super) fn downloads_tick(&mut self, window: &Window) {
        let (changed, busy) = {
            let mut h = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
            (h.take_change(), h.busy() || h.any_in_progress())
        };
        if changed {
            self.reload_downloads_pages();
        }
        if !busy {
            window.kill_timer(TIMER_DOWNLOADS);
        }
    }

    /// Tabs showing the downloads page show it again.
    fn reload_downloads_pages(&mut self) {
        for e in self.tabs.iter_mut().filter(|e| e.crashed.is_none()) {
            if e.tab.state.url.starts_with("snowlark://downloads") {
                e.tab.reload();
            }
        }
    }

    /// The downloads page asked for something (`query`: the apply address's).
    pub(super) fn apply_downloads(&mut self, window: &Window, query: &str) {
        let Some(act) = action(query) else {
            return;
        };
        let now = now_ms();
        let path_of = |b: &Browser, id: u64| {
            let h = b.downloads.lock().unwrap_or_else(|e| e.into_inner());
            h.get(id)
                .filter(|e| e.state == State::Complete)
                .map(|e| e.target_path.clone())
        };
        let changed = match act {
            Action::Open(id) => {
                if let Some(p) = path_of(self, id).filter(|p| p.is_file()) {
                    open_file(&p);
                }
                false
            }
            Action::Show(id) => {
                if let Some(p) = path_of(self, id).filter(|p| p.exists()) {
                    show_in_folder(&p);
                }
                false
            }
            Action::CopyLink(id) => {
                let url = {
                    let h = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                    h.get(id).map(|e| e.url().to_owned())
                };
                if let Some(url) = url.filter(|u| !u.is_empty()) {
                    self.tab_ctx.clipboard.set_text(&url);
                }
                false
            }
            Action::Retry(id) => {
                self.retry_download(window, id);
                false
            }
            Action::Cancel(id) => {
                let mut h = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                h.cancel(id);
                false
            }
            Action::Remove(id) => self.edit_downloads(|h| h.remove(id, now)),
            Action::ClearAll => self.edit_downloads(|h| h.clear_all(now)),
            Action::Undo => self.edit_downloads(DownloadHistory::undo),
        };
        if changed {
            self.downloads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take_change();
            self.reload_downloads_pages();
        }
    }

    /// Changes the list and saves it: whether it changed.
    fn edit_downloads(&mut self, f: impl FnOnce(&mut DownloadHistory) -> bool) -> bool {
        let mut h = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
        let changed = f(&mut h);
        if changed {
            let _ = h.save();
        }
        changed
    }
}

/// What a download thread saves.
struct Job {
    /// The file name suggested.
    name: String,
    chain: Vec<String>,
    referrer: String,
    tab_url: String,
}

impl Job {
    fn url(&self) -> &str {
        self.chain.last().map_or("", String::as_str)
    }
}

/// A download's response: begun (a navigation's), or to fetch once the user chose where.
enum Fetch {
    Started(Box<(ResponseHead, ResponseStream)>),
    Later(NetClient, Url),
}

/// The Save As dialog, with `name` suggested (and its extension as the type).
fn ask_where(owner: window::WindowHandle, name: &str) -> Option<PathBuf> {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| e.len() <= 5 && !e.is_empty());
    let filter = ext.map(|e| (format!("{} file", e.to_uppercase()), format!("*.{e}")));
    let filters: Vec<(&str, &str)> = filter
        .iter()
        .map(|(d, p)| (d.as_str(), p.as_str()))
        .collect();
    window::save_file_dialog(owner, name, &filters)
}

/// The partial file a download is written to until it is complete: `<file>.part`.
fn partial_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Runs a download to `path`, recording it in `downloads` as it goes.
fn run(downloads: &Mutex<DownloadHistory>, mut job: Job, path: PathBuf, fetch: Fetch) {
    let lock = || downloads.lock().unwrap_or_else(|e| e.into_inner());
    let partial = partial_path(&path);
    let mut start = Start {
        target_path: path.clone(),
        current_path: partial.clone(),
        url_chain: Vec::new(),
        referrer: std::mem::take(&mut job.referrer),
        tab_url: std::mem::take(&mut job.tab_url),
        mime_type: String::new(),
        start_time: now_ms(),
        total_bytes: 0,
    };
    // (a retry's response: its head first)
    let mut failed = None;
    let (head, mut stream) = match fetch {
        Fetch::Started(b) => {
            let (head, stream) = *b;
            (Some(head), stream)
        }
        Fetch::Later(net, url) => {
            let mut stream = net.fetch(Request::navigation(url));
            let mut head = None;
            while let Some(e) = stream.next_blocking() {
                match e {
                    FetchEvent::Head(h) => {
                        head = Some(h);
                        break;
                    }
                    FetchEvent::Error(_) | FetchEvent::Done => break,
                    _ => {}
                }
            }
            if head.as_ref().is_some_and(|h| h.status / 100 != 2) {
                failed = Some(Interrupt::Server);
            } else if head.is_none() {
                failed = Some(Interrupt::Network);
            }
            (head, stream)
        }
    };
    start.url_chain = match &head {
        Some(h) if !h.url_list.is_empty() => {
            h.url_list.iter().map(|u| u.as_str().to_owned()).collect()
        }
        _ => std::mem::take(&mut job.chain),
    };
    if let Some(h) = &head {
        start.mime_type = snowlark_net::mime::extract_mime_type(&h.headers)
            .map(|m| m.essence())
            .unwrap_or_default();
        start.total_bytes = h
            .headers
            .get("content-length")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
    }
    let url = start.url_chain.last().cloned().unwrap_or_default();
    let referrer = start.referrer.clone();
    let id = {
        let mut h = lock();
        let id = h.start(start);
        let _ = h.save();
        id
    };
    let finish = |state: State, why: Option<Interrupt>| {
        let mut h = lock();
        h.finish(id, state, why, now_ms());
        let _ = h.save();
    };
    if let Some(why) = failed {
        finish(State::Interrupted, Some(why));
        return;
    }
    let file = std::fs::File::create(&partial);
    let Ok(file) = file else {
        finish(State::Interrupted, Some(Interrupt::File));
        return;
    };
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut received = 0u64;
    let outcome = loop {
        if lock().is_cancelled(id) {
            break Err(None);
        }
        match stream.next_blocking() {
            Some(FetchEvent::Body(b)) => {
                if out.write_all(&b).is_err() {
                    break Err(Some(Interrupt::File));
                }
                received += b.len() as u64;
                lock().progress(id, received);
            }
            Some(FetchEvent::Error(_)) => break Err(Some(Interrupt::Network)),
            Some(FetchEvent::Done) | None => break Ok(()),
            Some(_) => {}
        }
    };
    let written = outcome.and_then(|()| {
        out.flush().map_err(|_| Some(Interrupt::File))?;
        drop(out);
        std::fs::rename(&partial, &path).map_err(|_| Some(Interrupt::File))
    });
    match written {
        Ok(()) => {
            if let Err(e) = mark_from_internet(&path, &url, &referrer) {
                eprintln!("[download] {}: {e}", path.display());
            }
            finish(State::Complete, None);
        }
        Err(why) => {
            let _ = std::fs::remove_file(&partial);
            match why {
                None => finish(State::Cancelled, None),
                Some(w) => finish(State::Interrupted, Some(w)),
            }
        }
    }
}

/// Opens a downloaded file as the system opens it (its default program).
fn open_file(path: &Path) {
    use std::os::windows::process::CommandExt as _;
    // (a Windows path cannot hold a quote, so quoting it is enough)
    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(format!("\"{}\"", path.display()))
        .spawn();
}

/// Opens the file's folder with the file selected.
fn show_in_folder(path: &Path) {
    use std::os::windows::process::CommandExt as _;
    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_net::{NetConfig, NetThread};
    use std::io::BufRead as _;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A server answering `/file` with `parts` (the head first), `gap` apart; any other path 404.
    fn serve(parts: Vec<Vec<u8>>, gap: Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let parts = parts.clone();
                std::thread::spawn(move || {
                    let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
                    let mut first = String::new();
                    let _ = reader.read_line(&mut first);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                    }
                    if first.split(' ').nth(1) != Some("/file") {
                        let _ = conn.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        return;
                    }
                    for p in parts {
                        if conn.write_all(&p).and_then(|()| conn.flush()).is_err() {
                            return;
                        }
                        std::thread::sleep(gap);
                    }
                });
            }
        });
        base
    }

    fn head(len: usize) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n").into_bytes()
    }

    fn job(url: &str) -> Job {
        Job {
            name: "a.zip".into(),
            chain: vec![url.into()],
            referrer: "https://ref.example/".into(),
            tab_url: "https://tab.example/".into(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snowlark-dl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_download_is_written_beside_its_file_until_complete() {
        assert_eq!(
            partial_path(Path::new("C:\\d\\a.zip")),
            PathBuf::from("C:\\d\\a.zip.part")
        );
        let net = NetThread::start(NetConfig::default()).unwrap();
        let base = serve(
            vec![head(6), b"abc".to_vec(), b"def".to_vec()],
            Duration::ZERO,
        );
        let dir = temp_dir("ok");
        let path = dir.join("a.zip");
        let downloads = Mutex::new(DownloadHistory::in_memory());
        let url = Url::parse(&format!("{base}/file")).unwrap();
        run(
            &downloads,
            job(url.as_str()),
            path.clone(),
            Fetch::Later(net.client(), url.clone()),
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
        assert!(!partial_path(&path).exists());
        let h = downloads.lock().unwrap();
        let e = &h.entries()[0];
        assert_eq!(e.state, State::Complete);
        assert_eq!((e.received_bytes, e.total_bytes), (6, 6));
        assert_eq!(e.url(), url.as_str());
        assert_eq!(e.mime_type, "application/zip");
        assert_eq!(e.referrer, "https://ref.example/");
        assert_eq!(e.tab_url, "https://tab.example/");
        let mut zone = path.as_os_str().to_owned();
        zone.push(":Zone.Identifier");
        assert!(std::fs::read_to_string(PathBuf::from(zone))
            .unwrap()
            .contains("ZoneId=3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_or_cancelled_download_leaves_no_file() {
        let net = NetThread::start(NetConfig::default()).unwrap();
        let dir = temp_dir("fail");
        // An error status: interrupted, nothing written.
        let base = serve(vec![head(1), b"x".to_vec()], Duration::ZERO);
        let missing = Url::parse(&format!("{base}/nothing")).unwrap();
        let downloads = Arc::new(Mutex::new(DownloadHistory::in_memory()));
        let path = dir.join("a.zip");
        run(
            &downloads,
            job(missing.as_str()),
            path.clone(),
            Fetch::Later(net.client(), missing),
        );
        {
            let h = downloads.lock().unwrap();
            let e = &h.entries()[0];
            assert_eq!(
                (e.state, e.interrupt),
                (State::Interrupted, Some(Interrupt::Server))
            );
        }
        assert!(!path.exists() && !partial_path(&path).exists());
        // Cancelled while it runs: the partial file goes.
        let parts = std::iter::once(head(40))
            .chain((0..20).map(|_| b"xx".to_vec()))
            .collect();
        let base = serve(parts, Duration::from_millis(100));
        let url = Url::parse(&format!("{base}/file")).unwrap();
        let canceller = {
            let downloads = downloads.clone();
            std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_secs(10);
                while Instant::now() < until {
                    {
                        let mut h = downloads.lock().unwrap();
                        let running = h
                            .entries()
                            .iter()
                            .find(|e| e.state == State::InProgress && e.received_bytes > 0)
                            .map(|e| e.id);
                        if let Some(id) = running {
                            assert!(h.cancel(id));
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                panic!("the download never ran");
            })
        };
        run(
            &downloads,
            job(url.as_str()),
            path.clone(),
            Fetch::Later(net.client(), url),
        );
        canceller.join().unwrap();
        let h = downloads.lock().unwrap();
        let e = h.entries().last().unwrap();
        assert_eq!(e.state, State::Cancelled);
        assert!(e.received_bytes < 40);
        assert!(!path.exists() && !partial_path(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
