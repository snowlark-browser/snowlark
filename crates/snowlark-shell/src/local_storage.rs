// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser's `localStorage`: [`WebStorageHost`] with its areas kept in the profile, a file per
//! origin. Areas load when first opened. Changes reach the disk from a thread of their own, at most a
//! second late, and all of them when the browser closes; a crash loses that second.
//! An Incognito window's ([`LocalStorage::in_memory`]) keeps its areas in memory only.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use snowlark_page::web_storage::{Items, WebStorageHost};

const FLUSH_EVERY: Duration = Duration::from_secs(1);

#[derive(Serialize, Deserialize)]
struct File {
    origin: String,
    items: Items,
}

/// An origin's file name: its bytes in hex, as any character can be in one.
fn file_name(origin: &str) -> String {
    let hex: String = origin.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{hex}.json")
}

fn origin_of(name: &str) -> Option<String> {
    let hex = name.strip_suffix(".json")?;
    let bytes: Option<Vec<u8>> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

fn load(dir: &Path, origin: &str) -> Option<Items> {
    let text = std::fs::read(dir.join(file_name(origin))).ok()?;
    let file: File = serde_json::from_slice(&text).ok()?;
    // (a file renamed by hand must not give one site another's data)
    (file.origin == origin).then_some(file.items)
}

/// Writes an area: to a temporary file renamed over the old one, so a kill never leaves half a file.
fn save(dir: &Path, origin: String, items: Items) {
    let path = dir.join(file_name(&origin));
    if items.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    let Ok(text) = serde_json::to_vec(&File { origin, items }) else {
        return;
    };
    let temporary = path.with_extension("tmp");
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
}

fn flush(host: &WebStorageHost, dir: Option<&Path>) {
    let Some(dir) = dir else {
        return;
    };
    for (origin, items) in host.take_dirty() {
        save(dir, origin, items);
    }
}

/// See the module documentation. Dropping it writes what is left.
pub struct LocalStorage {
    host: Arc<WebStorageHost>,
    /// The profile's folder of areas (`None`: in memory only).
    dir: Option<PathBuf>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl LocalStorage {
    pub fn open(dir: PathBuf) -> LocalStorage {
        let loading = dir.clone();
        let host = WebStorageHost::with_loader(Box::new(move |o| load(&loading, o)));
        let (stop, stopped) = channel::<()>();
        let (flushing, flush_dir) = (host.clone(), dir.clone());
        let thread = std::thread::Builder::new()
            .name("snowlark local storage".into())
            .spawn(move || loop {
                match stopped.recv_timeout(FLUSH_EVERY) {
                    Err(RecvTimeoutError::Timeout) => flush(&flushing, Some(&flush_dir)),
                    _ => return,
                }
            })
            .ok();
        LocalStorage {
            host,
            dir: Some(dir),
            stop: Some(stop),
            thread,
        }
    }

    /// Areas that live in memory only and are gone when this is dropped: an Incognito window's.
    pub fn in_memory() -> LocalStorage {
        LocalStorage {
            host: WebStorageHost::new(),
            dir: None,
            stop: None,
            thread: None,
        }
    }

    pub fn host(&self) -> Arc<WebStorageHost> {
        self.host.clone()
    }

    /// The origins with an area, in memory or on disk.
    fn origins(&self) -> Vec<String> {
        let mut origins = self.host.origins();
        if let Some(Ok(entries)) = self.dir.as_ref().map(std::fs::read_dir) {
            origins.extend(
                entries
                    .flatten()
                    .filter_map(|e| origin_of(&e.file_name().to_string_lossy())),
            );
        }
        origins.sort();
        origins.dedup();
        origins
    }

    /// Empties the areas of `site` (a host: its registrable domain and subdomains), or all of them:
    /// everywhere at once, whichever tabs have them open.
    pub fn clear_site(&self, site: Option<&str>) {
        let on_site = snowlark_page::web_storage::site_matcher(site);
        for origin in self.origins().into_iter().filter(|o| on_site(o)) {
            self.host.wipe(&origin);
            if let Some(dir) = &self.dir {
                let _ = std::fs::remove_file(dir.join(file_name(&origin)));
            }
        }
    }
}

impl LocalStorage {
    /// Empties the areas last changed from `begin` to `end` (both inclusive): clearing the site
    /// data of a time range. What is waiting to be written goes to disk first, so an area's file
    /// time is when it last changed.
    pub fn clear_modified_between(&self, begin: SystemTime, end: SystemTime) {
        let Some(dir) = &self.dir else {
            // (in memory: every area changed while the window was open, so within any range
            // that reaches now; ranges are always "from then until now")
            for origin in self.host.origins() {
                self.host.wipe(&origin);
            }
            return;
        };
        flush(&self.host, Some(dir));
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let Some(origin) = origin_of(&e.file_name().to_string_lossy()) else {
                continue;
            };
            let changed = e.metadata().and_then(|m| m.modified());
            if changed.is_ok_and(|t| t >= begin && t <= end) {
                self.host.wipe(&origin);
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

impl Drop for LocalStorage {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        flush(&self.host, self.dir.as_deref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_page::web_storage::{StorageWrite, WebStorage};

    const A: &str = "https://a.example";

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("snowlark-ls-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn set(store: &LocalStorage, origin: &str, key: &str, value: &str) {
        let host = store.host();
        let client = host.connect(Arc::new(|_| {}));
        host.open(client, origin);
        host.write(
            client,
            StorageWrite::Set {
                origin: origin.into(),
                key: key.into(),
                value: value.into(),
                url: String::new(),
            },
        );
        host.disconnect(client);
    }

    fn get(store: &LocalStorage, origin: &str) -> Items {
        let host = store.host();
        let client = host.connect(Arc::new(|_| {}));
        let items = host.open(client, origin);
        host.disconnect(client);
        items
    }

    #[test]
    fn an_area_survives_a_restart() {
        let d = dir("restart");
        {
            let s = LocalStorage::open(d.clone());
            set(&s, A, "k", "héllo \"quoted\"\n");
        }
        let s = LocalStorage::open(d.clone());
        assert_eq!(
            get(&s, A),
            [("k".to_owned(), "héllo \"quoted\"\n".to_owned())]
        );
        drop(s);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn changes_reach_the_disk_without_a_clean_exit() {
        let d = dir("flush");
        let s = LocalStorage::open(d.clone());
        set(&s, A, "k", "v");
        let path = d.join(file_name(A));
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while !path.exists() && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(path.exists());
        drop(s);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn clearing_a_site_reaches_areas_never_opened_in_this_run() {
        let d = dir("clear");
        {
            let s = LocalStorage::open(d.clone());
            set(&s, "https://shop.a.example", "k", "v");
            set(&s, "https://b.example", "k", "v");
        }
        let s = LocalStorage::open(d.clone());
        s.clear_site(Some("a.example"));
        assert!(get(&s, "https://shop.a.example").is_empty());
        assert_eq!(get(&s, "https://b.example").len(), 1);
        drop(s);
        let s = LocalStorage::open(d.clone());
        assert!(get(&s, "https://shop.a.example").is_empty());
        s.clear_site(None);
        assert!(get(&s, "https://b.example").is_empty());
        drop(s);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_time_range_empties_the_areas_changed_in_it() {
        let d = dir("range");
        let s = LocalStorage::open(d.clone());
        set(&s, "https://old.example", "k", "v");
        set(&s, "https://new.example", "k", "v");
        flush(&s.host, s.dir.as_deref());
        let hour = Duration::from_secs(3600);
        let now = SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(d.join(file_name("https://old.example")))
            .unwrap()
            .set_modified(now - 2 * hour)
            .unwrap();
        s.clear_modified_between(now - hour, now + hour);
        assert_eq!(get(&s, "https://old.example").len(), 1);
        assert!(get(&s, "https://new.example").is_empty());
        drop(s);
        let s = LocalStorage::open(d.clone());
        assert_eq!(get(&s, "https://old.example").len(), 1);
        assert!(get(&s, "https://new.example").is_empty());
        drop(s);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn areas_in_memory_are_shared_by_its_clients_and_gone_with_it() {
        let s = LocalStorage::in_memory();
        set(&s, A, "k", "v");
        assert_eq!(get(&s, A), [("k".to_owned(), "v".to_owned())]);
        s.clear_site(Some("a.example"));
        assert!(get(&s, A).is_empty());
        set(&s, A, "k", "v");
        let now = SystemTime::now();
        s.clear_modified_between(now - Duration::from_secs(60), now);
        assert!(get(&s, A).is_empty());
        drop(s);
        assert!(get(&LocalStorage::in_memory(), A).is_empty());
    }

    #[test]
    fn an_emptied_area_leaves_no_file() {
        let d = dir("empty");
        let s = LocalStorage::open(d.clone());
        set(&s, A, "k", "v");
        let host = s.host();
        let c = host.connect(Arc::new(|_| {}));
        host.open(c, A);
        host.write(
            c,
            StorageWrite::Clear {
                origin: A.into(),
                url: String::new(),
            },
        );
        drop(s);
        assert!(!d.join(file_name(A)).exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_corrupt_or_misplaced_file_is_an_empty_area() {
        let d = dir("corrupt");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(file_name(A)), "not json").unwrap();
        assert!(load(&d, A).is_none());
        let other = serde_json::to_vec(&File {
            origin: "https://other.example".into(),
            items: vec![("k".into(), "v".into())],
        })
        .unwrap();
        std::fs::write(d.join(file_name(A)), other).unwrap();
        assert!(load(&d, A).is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn names_round_trip() {
        for origin in [A, "https://[::1]:8080", "https://xn--bcher-kva.example"] {
            assert_eq!(origin_of(&file_name(origin)).as_deref(), Some(origin));
        }
        assert_eq!(origin_of("readme.txt"), None);
        assert_eq!(origin_of("zz.json"), None);
    }
}
