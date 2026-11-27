// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The off-the-record partition ([`crate::partition`]): what an Incognito tab stores stays in
//! memory, is seen by the other Incognito tabs and by no regular one, and leaves the profile folder
//! as it was.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::partition::Partition;

/// A server for the test's pages: `/store` sets a cookie by header, `/cached` may be kept for ten
/// minutes (each request for it is counted), anything else is `/probe`'s page, never kept.
fn serve(store: String, probe: String) -> (String, Arc<AtomicUsize>) {
    use std::io::{BufRead, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let fetched = Arc::new(AtomicUsize::new(0));
    let counter = fetched.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let (store, probe, counter) = (store.clone(), probe.clone(), counter.clone());
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
                let path = first.split(' ').nth(1).unwrap_or("").to_owned();
                let (kind, headers, body) = match path.as_str() {
                    "/store" => ("text/html", "Set-Cookie: hdr=1; Path=/\r\n", store),
                    "/cached" => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        ("text/plain", "Cache-Control: max-age=600\r\n", "x".into())
                    }
                    _ => ("text/html", "Cache-Control: no-store\r\n", probe),
                };
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(answer.as_bytes());
            });
        }
    });
    (base, fetched)
}

/// Sets a cookie by script (and one by header), a `localStorage` item and an IndexedDB record, and
/// fetches a cacheable response twice.
const STORE: &str = r#"<script>
document.cookie = 'js=1; path=/';
localStorage.setItem('k', 'v');
fetch('/cached').then(r => r.text()).then(() => fetch('/cached')).then(r => r.text()).then(() => {
  const r = indexedDB.open('db', 1);
  r.onupgradeneeded = () => r.result.createObjectStore('s');
  r.onerror = () => document.title = 'probe idb failed';
  r.onsuccess = () => {
    const db = r.result;
    const t = db.transaction('s', 'readwrite');
    t.objectStore('s').put('v', 'k');
    t.oncomplete = () => { db.close(); document.title = 'probe stored'; };
  };
});
</script>"#;

/// Says what the page sees of the cookies, `localStorage` and IndexedDB, after fetching the
/// cacheable response once.
const PROBE: &str = r#"<script>
const out = { cookie: document.cookie, ls: localStorage.getItem('k') };
fetch('/cached').then(r => r.text()).then(() => {
  const r = indexedDB.open('db');
  r.onupgradeneeded = () => { out.idb = 'none'; };
  r.onsuccess = () => {
    if (!out.idb) out.idb = r.result.objectStoreNames.length ? 'db' : 'empty';
    r.result.close();
    document.title = 'probe ' + JSON.stringify(out);
  };
});
</script>"#;

/// A tab context whose services are `p`'s.
fn context_of(
    p: &Partition,
) -> (
    TabContext,
    std::sync::mpsc::Receiver<(TabId, TabEvent)>,
    std::sync::mpsc::Receiver<crate::composite::CompInput>,
) {
    let (compositor, comp_rx) = channel();
    let (events, rx) = snowlark_ipc::channel();
    let ctx = TabContext {
        gpu: Gpu::with_driver(snowlark_paint::Driver::Warp).unwrap(),
        videos: None,
        net: p.net.clone(),
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
        clipboard: Arc::new(MemoryClipboard::default()),
        internal: no_internal_pages(),
        content_filter: None,
        audio: None,
        audio_out: None,
        broadcast: p.broadcast.clone(),
        web_storage: p.local_storage.host(),
        idb: p.idb.clone(),
        caches: snowlark_page::CacheBackend::new(p.caches.clone()),
        service_workers: Some(snowlark_page::ServiceWorkerBackend::new(
            p.service_workers.clone(),
        )),
        cdm: None,
    };
    (ctx, rx, comp_rx)
}

/// Loads `url` in a tab of `p` and returns the title its page ends with (`probe ...`).
fn load(p: &Partition, url: &str, renderer: Renderer) -> String {
    let (ctx, rx, _comp_rx) = context_of(p);
    let url = Url::parse(url).unwrap();
    let mut tab = TabHost::spawn(1, ctx, Default::default(), DEVICE, url, renderer);
    next_of(&rx, &mut tab, 1, |e| match e {
        TabEvent::State(s) if s.title.starts_with("probe ") => Some(s.title),
        TabEvent::Crashed(why) => panic!("the tab crashed: {why}"),
        _ => None,
    })
}

/// Every file under `dir` with its bytes, by its path from there.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.push((p.strip_prefix(base).unwrap().to_owned(), Vec::new()));
                walk(base, &p, out);
            } else {
                let bytes = std::fs::read(&p).unwrap_or_default();
                out.push((p.strip_prefix(base).unwrap().to_owned(), bytes));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

const SEES_ALL: &str = r#"probe {"cookie":"hdr=1; js=1","ls":"v","idb":"db"}"#;
const SEES_NOTHING: &str = r#"probe {"cookie":"","ls":null,"idb":"none"}"#;

fn incognito_keeps_to_itself(renderer: impl Fn() -> Renderer) {
    let dir = std::env::temp_dir().join(format!(
        "snowlark-incognito-{}-{}",
        std::process::id(),
        random()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let profile = crate::profile::Profile::at(&dir);
    let (base, fetched) = serve(STORE.into(), PROBE.into());
    let regular = Partition::regular(&profile);
    assert!(!regular.is_off_the_record());
    // (what opening the profile writes, settled)
    std::thread::sleep(std::time::Duration::from_millis(500));
    let before = snapshot(&dir);

    let incognito = Partition::off_the_record();
    assert!(incognito.is_off_the_record());
    assert_eq!(
        load(&incognito, &format!("{base}/store"), renderer()),
        "probe stored"
    );
    // (the second fetch was the cache's)
    assert_eq!(fetched.load(Ordering::SeqCst), 1);
    // Another Incognito tab sees all of it, and its fetch is the same cache's.
    assert_eq!(
        load(&incognito, &format!("{base}/probe"), renderer()),
        SEES_ALL
    );
    assert_eq!(fetched.load(Ordering::SeqCst), 1);
    // The last Incognito window closes: its partition goes, and the profile is as it was, with
    // the network's save interval passed.
    drop(incognito);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(snapshot(&dir) == before, "the profile folder changed");

    // A regular tab sees none of it (and the server is asked again: no cache is shared).
    assert_eq!(
        load(&regular, &format!("{base}/probe"), renderer()),
        SEES_NOTHING
    );
    assert_eq!(fetched.load(Ordering::SeqCst), 2);
    // Nor does the next Incognito window's.
    let next = Partition::off_the_record();
    assert_eq!(
        load(&next, &format!("{base}/probe"), renderer()),
        SEES_NOTHING
    );
    assert_eq!(fetched.load(Ordering::SeqCst), 3);
    drop((next, regular));
    let _ = std::fs::remove_dir_all(&dir);
}

fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

#[test]
fn an_incognito_tab_keeps_to_its_partition() {
    incognito_keeps_to_itself(|| Renderer::Thread(crate::page_driver::PageDriver::factory()));
}

/// The same with each tab's page in a renderer process: its services cross the pipe to its
/// partition's.
#[test]
fn an_incognito_tab_in_a_renderer_process_keeps_to_its_partition() {
    incognito_keeps_to_itself(|| remote(child_launch()));
}
