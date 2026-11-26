// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! V8's code cache on disk: after an external script has run, V8 serializes its compiled functions
//! (`ScriptCompiler::CreateCodeCache`), and a later load of the same script consumes them
//! (`CachedData` with `kConsumeCodeCache`) instead of compiling.
//!
//! Only a script that is loaded again gets code, as in Chromium: its first load leaves a timestamp
//! ([`CacheKind::Timestamp`]); a load of the same response within [`HOT_FOR`] of it runs the
//! script and then makes the code cache, which the loads after it consume. The code is then that of
//! a returning visit: the functions it compiled, and the source positions its error stacks needed
//! (bytecode keeps those it was asked for; without them a stack read reparses the function, and
//! for a frame of a script's top level the whole script).
//!
//! Entries live in a folder of the profile, one file per script URL. An entry is used only for the
//! response whose bytes it was made from: it is keyed by the SHA-256 of the response body (computed
//! as the bytes arrive, [`ResponseHasher`]) as well as the URL, and by V8's cached data version tag
//! (V8's version and flags). The body hash is what keeps a stale or substituted entry out: V8's own
//! check of cached data against the source is its length, not its text. The folder has a size
//! budget; the least recently used entries go first (a read refreshes the file's modification time).
//! Writes and evictions run on a thread of their own.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

/// The default size budget of the folder.
pub const DEFAULT_BUDGET: u64 = 64 * 1024 * 1024;

const MAGIC: &[u8; 8] = b"OSMCODE1";

/// SHA-256 of a response body, fed chunk by chunk.
#[derive(Clone, Default)]
pub struct ResponseHasher(Sha256);

impl ResponseHasher {
    pub fn new() -> ResponseHasher {
        ResponseHasher::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

/// What kind of script an entry was made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheKind {
    Classic = 0,
    Module = 1,
    /// No code: when the response was loaded ([`timestamp`]), for the heat check.
    Timestamp = 2,
}

impl CacheKind {
    /// The kind stored as `byte` (on disk, across a pipe).
    pub fn from_u8(byte: u8) -> Option<CacheKind> {
        match byte {
            0 => Some(CacheKind::Classic),
            1 => Some(CacheKind::Module),
            2 => Some(CacheKind::Timestamp),
            _ => None,
        }
    }
}

/// How long after a load a second load of the same response makes the code cache (Chromium's
/// `kHotHours`).
pub const HOT_FOR: Duration = Duration::from_secs(72 * 60 * 60);

/// A [`CacheKind::Timestamp`] entry's data: `now` in milliseconds since the Unix epoch.
pub fn timestamp(now: SystemTime) -> Vec<u8> {
    let ms = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    ms.to_le_bytes().to_vec()
}

/// Whether a [`CacheKind::Timestamp`] entry's `data` is of a load less than [`HOT_FOR`] before
/// `now` (and not after it: a clock set back starts over).
pub fn is_hot(data: &[u8], now: SystemTime) -> bool {
    let Ok(bytes) = <[u8; 8]>::try_from(data) else {
        return false;
    };
    let then = SystemTime::UNIX_EPOCH + Duration::from_millis(u64::from_le_bytes(bytes));
    now.duration_since(then).is_ok_and(|age| age < HOT_FOR)
}

/// One entry read from disk.
pub struct Entry {
    pub kind: CacheKind,
    pub response_hash: [u8; 32],
    pub data: Vec<u8>,
}

enum Job {
    Store {
        url: String,
        kind: CacheKind,
        hash: [u8; 32],
        data: Vec<u8>,
    },
    Remove(String),
    /// Remove the entries last used from the first time (inclusive) to the second (exclusive).
    Clear(SystemTime, SystemTime),
    /// Reply when every job before it is done.
    Flush(mpsc::Sender<()>),
}

/// What a page keeps its compiled code in (`docs/process-isolation.md`, M2): a [`CodeCache`] on disk,
/// or, in a renderer, the same calls as messages to the browser, which owns the folder.
pub trait CodeCacheStore: Send + Sync {
    /// Whether there may be an entry for `url`.
    fn has(&self, url: &str) -> bool;

    /// Reads the entry for `url` off the calling thread; the receiver gets it, or `None`.
    fn read_async(&self, url: &str) -> mpsc::Receiver<Option<Entry>>;

    /// Keeps `data` for `url`, compiled from a response with the hash `hash`.
    fn store(&self, url: &str, kind: CacheKind, hash: [u8; 32], data: Vec<u8>);

    /// Forgets the entry for `url`.
    fn remove(&self, url: &str);

    /// Forgets the entries last used from `begin` (inclusive) to `end` (exclusive): clearing the
    /// cached files of a time range. Only the store that owns the folder does anything.
    fn clear_between(&self, _begin: SystemTime, _end: SystemTime) {}
}

impl CodeCacheStore for CodeCache {
    fn has(&self, url: &str) -> bool {
        CodeCache::has(self, url)
    }

    fn read_async(&self, url: &str) -> mpsc::Receiver<Option<Entry>> {
        CodeCache::read_async(self, url)
    }

    fn store(&self, url: &str, kind: CacheKind, hash: [u8; 32], data: Vec<u8>) {
        CodeCache::store(self, url, kind, hash, data);
    }

    fn remove(&self, url: &str) {
        CodeCache::remove(self, url);
    }

    fn clear_between(&self, begin: SystemTime, end: SystemTime) {
        CodeCache::clear_between(self, begin, end);
    }
}

/// A code cache folder.
pub struct CodeCache {
    dir: PathBuf,
    /// The file names present (as far as this instance knows).
    present: Arc<Mutex<HashSet<String>>>,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    version_tag: u32,
}

impl CodeCache {
    /// Opens (creating) the folder `dir` with a budget of `budget` bytes.
    pub fn open(dir: PathBuf, budget: u64) -> Arc<CodeCache> {
        let _ = std::fs::create_dir_all(&dir);
        let present: HashSet<String> = std::fs::read_dir(&dir)
            .map(|it| {
                it.filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.ends_with(".v8c"))
                    .collect()
            })
            .unwrap_or_default();
        let present = Arc::new(Mutex::new(present));
        let (tx, rx) = mpsc::channel::<Job>();
        let (wdir, wpresent) = (dir.clone(), present.clone());
        let version_tag = crate::code_cache_version_tag();
        let writer = std::thread::Builder::new()
            .name("snowlark-code-cache".into())
            .spawn(move || {
                for job in rx {
                    match job {
                        Job::Store {
                            url,
                            kind,
                            hash,
                            data,
                        } => {
                            let name = file_name(&url);
                            if write_entry(&wdir, &name, &url, kind, version_tag, &hash, &data)
                                .is_ok()
                            {
                                wpresent.lock().unwrap().insert(name);
                                evict(&wdir, budget, &wpresent);
                            }
                        }
                        Job::Remove(url) => {
                            let name = file_name(&url);
                            let _ = std::fs::remove_file(wdir.join(&name));
                            wpresent.lock().unwrap().remove(&name);
                        }
                        Job::Clear(begin, end) => {
                            let names: Vec<String> = std::fs::read_dir(&wdir)
                                .map(|it| {
                                    it.filter_map(|e| e.ok())
                                        .filter(|e| {
                                            e.metadata()
                                                .and_then(|m| m.modified())
                                                .is_ok_and(|t| t >= begin && t < end)
                                        })
                                        .filter_map(|e| e.file_name().into_string().ok())
                                        .filter(|n| n.ends_with(".v8c"))
                                        .collect()
                                })
                                .unwrap_or_default();
                            for name in names {
                                if std::fs::remove_file(wdir.join(&name)).is_ok() {
                                    wpresent.lock().unwrap().remove(&name);
                                }
                            }
                        }
                        Job::Flush(reply) => {
                            let _ = reply.send(());
                        }
                    }
                }
            })
            .ok();
        Arc::new(CodeCache {
            dir,
            present,
            jobs: Mutex::new(Some(tx)),
            writer: Mutex::new(writer),
            version_tag,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether there is an entry for `url` (of any response).
    pub fn has(&self, url: &str) -> bool {
        self.present.lock().unwrap().contains(&file_name(url))
    }

    /// Reads the entry for `url` on a thread of its own; the receiver gets it, or `None` when there
    /// is none, it is unreadable, or it was made by another V8.
    pub fn read_async(&self, url: &str) -> mpsc::Receiver<Option<Entry>> {
        let (tx, rx) = mpsc::channel();
        let path = self.dir.join(file_name(url));
        let (url, tag) = (url.to_owned(), self.version_tag);
        let spawned = std::thread::Builder::new()
            .name("snowlark-code-cache-read".into())
            .spawn(move || {
                let _ = tx.send(read_entry(&path, &url, tag));
            });
        if spawned.is_err() {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(None);
            return rx;
        }
        rx
    }

    /// Reads the entry for `url` now.
    pub fn read(&self, url: &str) -> Option<Entry> {
        read_entry(&self.dir.join(file_name(url)), url, self.version_tag)
    }

    /// Stores (replacing) the entry for `url`: V8's `data` for the response whose body hashes to
    /// `hash`.
    pub fn store(&self, url: &str, kind: CacheKind, hash: [u8; 32], data: Vec<u8>) {
        self.send(Job::Store {
            url: url.to_owned(),
            kind,
            hash,
            data,
        });
    }

    /// Removes the entry for `url` (V8 rejected it).
    pub fn remove(&self, url: &str) {
        self.present.lock().unwrap().remove(&file_name(url));
        self.send(Job::Remove(url.to_owned()));
    }

    /// Removes the entries last used (written or read) from `begin` (inclusive) to `end`
    /// (exclusive), after the writes asked for before it.
    pub fn clear_between(&self, begin: SystemTime, end: SystemTime) {
        self.send(Job::Clear(begin, end));
    }

    /// Waits until the writes asked for so far are on disk.
    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        self.send(Job::Flush(tx));
        let _ = rx.recv();
    }

    fn send(&self, job: Job) {
        if let Some(tx) = self.jobs.lock().unwrap().as_ref() {
            let _ = tx.send(job);
        }
    }
}

impl Drop for CodeCache {
    /// Finishes the writes in progress.
    fn drop(&mut self) {
        self.jobs.lock().unwrap().take();
        if let Some(w) = self.writer.lock().unwrap().take() {
            let _ = w.join();
        }
    }
}

/// The file of `url`'s entry: a hash of the URL (URLs are long and not file names).
fn file_name(url: &str) -> String {
    let h = Sha256::digest(url.as_bytes());
    let mut s = String::with_capacity(36);
    for b in &h[..16] {
        s.push_str(&format!("{b:02x}"));
    }
    s.push_str(".v8c");
    s
}

fn write_entry(
    dir: &Path,
    name: &str,
    url: &str,
    kind: CacheKind,
    tag: u32,
    hash: &[u8; 32],
    data: &[u8],
) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(60 + url.len() + data.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&(kind as u32).to_le_bytes());
    bytes.extend_from_slice(hash);
    bytes.extend_from_slice(&(url.len() as u32).to_le_bytes());
    bytes.extend_from_slice(url.as_bytes());
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(data);
    // Written aside, then renamed over the old entry: a reader never sees half an entry.
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, dir.join(name)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn read_entry(path: &Path, url: &str, tag: u32) -> Option<Entry> {
    let bytes = std::fs::read(path).ok()?;
    let mut r = Reader(&bytes);
    if r.take(8)? != MAGIC || r.u32()? != tag {
        return None;
    }
    let kind = CacheKind::from_u8(u8::try_from(r.u32()?).ok()?)?;
    let response_hash: [u8; 32] = r.take(32)?.try_into().ok()?;
    let url_len = r.u32()? as usize;
    if r.take(url_len)? != url.as_bytes() {
        return None;
    }
    let data_len = r.u32()? as usize;
    let data = r.take(data_len)?.to_vec();
    // Recently used: eviction goes by modification time.
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
    Some(Entry {
        kind,
        response_hash,
        data,
    })
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

/// Deletes the least recently used entries until the folder is under its budget.
fn evict(dir: &Path, budget: u64, present: &Mutex<HashSet<String>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf, String)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !name.ends_with(".v8c") {
                return None;
            }
            let m = e.metadata().ok()?;
            Some((
                m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                m.len(),
                e.path(),
                name,
            ))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= budget {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, path, name) in files {
        if total <= budget {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
            present.lock().unwrap().remove(&name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("snowlark-code-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_timestamp_is_hot_for_three_days_after_it() {
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let t = timestamp(then);
        assert!(is_hot(&t, then));
        assert!(is_hot(&t, then + HOT_FOR - Duration::from_secs(1)));
        assert!(!is_hot(&t, then + HOT_FOR));
        // (a clock set back, or data that is no timestamp)
        assert!(!is_hot(&t, then - Duration::from_secs(1)));
        assert!(!is_hot(&[1, 2, 3], then));
        // A timestamp entry is kept and read back as one.
        let dir = temp_dir("timestamp");
        let cache = CodeCache::open(dir.clone(), DEFAULT_BUDGET);
        cache.store(
            "https://a.test/t.js",
            CacheKind::Timestamp,
            [3; 32],
            t.clone(),
        );
        cache.flush();
        let e = cache.read("https://a.test/t.js").expect("the entry");
        assert_eq!(
            (e.kind, e.response_hash, e.data),
            (CacheKind::Timestamp, [3; 32], t)
        );
        drop(cache);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_page_uses_the_cache_through_the_store() {
        crate::init();
        let cache = CodeCache::open(temp_dir("store"), 1 << 20);
        let store: Arc<dyn CodeCacheStore> = cache.clone();
        let url = "https://a.test/x.js";
        assert!(!store.has(url));
        store.store(url, CacheKind::Classic, [7; 32], vec![1, 2, 3]);
        cache.flush();
        assert!(store.has(url));
        let entry = store.read_async(url).recv().unwrap().expect("the entry");
        assert_eq!(entry.response_hash, [7; 32]);
        store.remove(url);
        cache.flush();
        assert!(!store.has(url));
    }

    #[test]
    fn stores_reads_and_checks_the_key() {
        crate::init();
        let dir = temp_dir("rw");
        let cache = CodeCache::open(dir.clone(), DEFAULT_BUDGET);
        let mut h = ResponseHasher::new();
        h.update(b"var a");
        h.update(b" = 1;");
        let hash = h.finish();
        assert_eq!(hash, {
            let mut h = ResponseHasher::new();
            h.update(b"var a = 1;");
            h.finish()
        });
        cache.store(
            "https://a.test/a.js",
            CacheKind::Classic,
            hash,
            vec![1, 2, 3],
        );
        cache.flush();
        assert!(cache.has("https://a.test/a.js"));
        let e = cache.read("https://a.test/a.js").unwrap();
        assert_eq!(
            (e.kind, e.response_hash, e.data),
            (CacheKind::Classic, hash, vec![1, 2, 3])
        );
        assert!(cache.read("https://a.test/b.js").is_none());
        let e = cache.read_async("https://a.test/a.js").recv().unwrap();
        assert!(e.is_some());
        // Another instance on the same folder sees it.
        drop(cache);
        let cache = CodeCache::open(dir.clone(), DEFAULT_BUDGET);
        assert!(cache.has("https://a.test/a.js"));
        cache.remove("https://a.test/a.js");
        cache.flush();
        assert!(cache.read("https://a.test/a.js").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_time_range_clears_the_entries_used_in_it() {
        crate::init();
        let dir = temp_dir("range");
        let cache = CodeCache::open(dir.clone(), DEFAULT_BUDGET);
        for name in ["old", "new"] {
            cache.store(
                &format!("https://a.test/{name}.js"),
                CacheKind::Classic,
                [0; 32],
                vec![1; 10],
            );
        }
        cache.flush();
        let hour = std::time::Duration::from_secs(3600);
        let now = SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(dir.join(file_name("https://a.test/old.js")))
            .unwrap()
            .set_modified(now - 2 * hour)
            .unwrap();
        cache.clear_between(now - hour, now + hour);
        cache.flush();
        assert!(cache.has("https://a.test/old.js"));
        assert!(!cache.has("https://a.test/new.js"));
        assert!(cache.read("https://a.test/new.js").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evicts_the_least_recently_used_over_budget() {
        crate::init();
        let dir = temp_dir("lru");
        let cache = CodeCache::open(dir.clone(), 2500);
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            cache.store(
                &format!("https://a.test/{name}.js"),
                CacheKind::Classic,
                [i as u8; 32],
                vec![0; 1000],
            );
            cache.flush();
            if *name == "b" {
                // `a` is read again: `b` is now the least recently used.
                std::thread::sleep(std::time::Duration::from_millis(30));
                assert!(cache.read("https://a.test/a.js").is_some());
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        assert!(cache.has("https://a.test/a.js"));
        assert!(!cache.has("https://a.test/b.js"));
        assert!(cache.has("https://a.test/c.js"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A code cache entry V8 deserializes on a thread of its own (`ScriptCompiler::
/// StartConsumingCodeCache` and `ConsumeCodeCacheTask`, as Chromium consumes its code cache), so that
/// the page thread only finishes the script from it. Like script streaming, the `v8` crate binds none
/// of this; the entry points are declared by their MSVC x64 and Itanium x86-64 names (see
/// [`crate::streaming`]), and
/// the task is handed to V8 through the `consume_cache_task` member of the `Source` (a
/// `std::unique_ptr`, one pointer, that the `Source`'s destructor deletes).
pub struct CacheConsumer {
    data: Box<[u8]>,
    #[cfg(v8_abi)]
    task: *mut std::ffi::c_void,
    thread: Option<std::thread::JoinHandle<()>>,
    done: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(v8_abi)]
mod ffi {
    use std::ffi::c_void;

    // SAFETY (all declarations): MSVC x64 and Itanium x86-64 names of V8 functions in the library the
    // `v8` crate links, with the target's convention (see [`crate::streaming`]).
    extern "C" {
        /// `void v8::ScriptCompiler::ConsumeCodeCacheTask::Run()`.
        #[cfg_attr(
            v8_abi_msvc,
            link_name = "?Run@ConsumeCodeCacheTask@ScriptCompiler@v8@@QEAAXXZ"
        )]
        #[cfg_attr(
            v8_abi_itanium,
            link_name = "_ZN2v814ScriptCompiler20ConsumeCodeCacheTask3RunEv"
        )]
        pub fn consume_task_run(this: *mut c_void);

        /// `v8::ScriptCompiler::ConsumeCodeCacheTask::~ConsumeCodeCacheTask()` (Itanium: the
        /// complete object destructor).
        #[cfg_attr(
            v8_abi_msvc,
            link_name = "??1ConsumeCodeCacheTask@ScriptCompiler@v8@@QEAA@XZ"
        )]
        #[cfg_attr(
            v8_abi_itanium,
            link_name = "_ZN2v814ScriptCompiler20ConsumeCodeCacheTaskD1Ev"
        )]
        pub fn consume_task_destruct(this: *mut c_void);

        /// `void operator delete(void*)`.
        #[cfg_attr(v8_abi_msvc, link_name = "??3@YAXPEAX@Z")]
        #[cfg_attr(v8_abi_itanium, link_name = "_ZdlPv")]
        pub fn operator_delete(p: *mut c_void);
    }

    // MSVC: the `std::unique_ptr` argument is passed by address, and the callee moves out of it.
    #[cfg(v8_abi_msvc)]
    extern "C" {
        /// `static ConsumeCodeCacheTask* v8::ScriptCompiler::StartConsumingCodeCache(Isolate*,
        /// std::unique_ptr<CachedData>)`.
        #[link_name = "?StartConsumingCodeCache@ScriptCompiler@v8@@SAPEAVConsumeCodeCacheTask@12@PEAVIsolate@2@V?$unique_ptr@UCachedData@ScriptCompiler@v8@@U?$default_delete@UCachedData@ScriptCompiler@v8@@@__Cr@std@@@__Cr@std@@@Z"]
        pub fn start_consuming(isolate: *mut c_void, data: *mut *mut c_void) -> *mut c_void;
    }

    // Itanium: the `[[clang::trivial_abi]]` `std::unique_ptr` travels as the pointer it holds, and the
    // callee owns it.
    #[cfg(v8_abi_itanium)]
    extern "C" {
        #[link_name = "_ZN2v814ScriptCompiler23StartConsumingCodeCacheEPNS_7IsolateENSt4__Cr10unique_ptrINS0_10CachedDataENS3_14default_deleteIS5_EEEE"]
        fn start_consuming_itanium(isolate: *mut c_void, data: *mut c_void) -> *mut c_void;
    }

    /// `StartConsumingCodeCache`, taking the `CachedData` out of the `unique_ptr` storage `data`.
    #[cfg(v8_abi_itanium)]
    pub unsafe fn start_consuming(isolate: *mut c_void, data: *mut *mut c_void) -> *mut c_void {
        // SAFETY: the caller's contract, as for the MSVC declaration; the pointer moves to V8.
        unsafe {
            let owned = std::mem::replace(&mut *data, std::ptr::null_mut());
            start_consuming_itanium(isolate, owned)
        }
    }
}

/// The layout of `v8::ScriptCompiler::Source` as the `v8` crate mirrors it, to reach the
/// `consume_cache_task` member.
#[cfg(v8_abi)]
#[repr(C)]
#[allow(dead_code)]
struct SourceLayout {
    source_string: usize,
    resource_name: usize,
    resource_line_offset: i32,
    resource_column_offset: i32,
    resource_options: i32,
    source_map_url: usize,
    host_defined_options: usize,
    cached_data: usize,
    consume_cache_task: usize,
    compile_hint_callback: usize,
    compile_hint_callback_data: usize,
    compilation_details: [usize; 3],
}

#[cfg(v8_abi)]
const _: () = assert!(
    std::mem::size_of::<SourceLayout>() == std::mem::size_of::<v8::script_compiler::Source>()
);

#[cfg(v8_abi)]
struct SendPtr(*mut std::ffi::c_void);

// SAFETY: a ConsumeCodeCacheTask is made to run on another thread (V8's documented use); the pointer
// is used there only until that thread is joined.
#[cfg(v8_abi)]
unsafe impl Send for SendPtr {}

impl CacheConsumer {
    /// Starts deserializing `data` (an entry's V8 bytes) on a new thread; `None` if V8 does not do
    /// it off-thread. `on_done` runs on that thread when it is done.
    #[cfg(v8_abi)]
    pub fn start(
        isolate: &v8::Isolate,
        data: Vec<u8>,
        on_done: Box<dyn Fn() + Send>,
    ) -> Option<CacheConsumer> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let data = data.into_boxed_slice();
        let cached = v8::script_compiler::CachedData::new(&data);
        let mut raw = cached.into_raw() as *mut std::ffi::c_void;
        // SAFETY: the isolate is entered on this thread; `raw` is a CachedData that V8's `new` made
        // (`CachedData::new`), not owning its bytes, which live in `data` as long as the task does;
        // StartConsumingCodeCache takes it out of the unique_ptr storage `raw`.
        let task = unsafe {
            let iso: v8::UnsafeRawIsolatePtr = isolate.as_raw_isolate_ptr();
            let iso: *mut std::ffi::c_void = std::mem::transmute(iso);
            ffi::start_consuming(iso, &mut raw)
        };
        if !raw.is_null() {
            // (not taken: give the CachedData back to its owner type to delete it)
            // SAFETY: `raw` is still the CachedData made above, owned by nothing else.
            drop(unsafe {
                v8::UniquePtr::<v8::script_compiler::CachedData>::from_raw(
                    raw as *mut v8::script_compiler::CachedData,
                )
            });
        }
        if task.is_null() {
            return None;
        }
        let done = Arc::new(AtomicBool::new(false));
        let mut consumer = CacheConsumer {
            data,
            task,
            thread: None,
            done: done.clone(),
        };
        let run = SendPtr(task);
        let thread = std::thread::Builder::new()
            .name("snowlark-code-cache-consume".into())
            .spawn(move || {
                let run = run;
                // SAFETY: the task lives until this thread is joined; Run is called once.
                unsafe { ffi::consume_task_run(run.0) };
                done.store(true, Ordering::Release);
                on_done();
            })
            .ok()?;
        consumer.thread = Some(thread);
        Some(consumer)
    }

    #[cfg(not(v8_abi))]
    pub fn start(
        _isolate: &v8::Isolate,
        _data: Vec<u8>,
        _on_done: Box<dyn Fn() + Send>,
    ) -> Option<CacheConsumer> {
        None
    }

    /// The entry's size.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Whether the background deserialization has finished.
    pub fn is_done(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::Acquire)
    }

    fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// A `Source` for `code` that consumes this entry; `f` compiles it (with `kConsumeCodeCache`).
    /// Returns what `f` returned and whether V8 rejected the entry.
    pub fn consume<'s, R>(
        mut self,
        code: v8::Local<'s, v8::String>,
        origin: &v8::ScriptOrigin<'s>,
        f: impl FnOnce(&mut v8::script_compiler::Source) -> R,
    ) -> (R, bool) {
        self.join();
        let cached = v8::script_compiler::CachedData::new(&self.data);
        let mut source =
            v8::script_compiler::Source::new_with_cached_data(code, Some(origin), cached);
        #[cfg(v8_abi)]
        // SAFETY: `source` has the layout `SourceLayout` mirrors (checked by size above, the crate's
        // own mirror of V8's class); its consume_cache_task member is a null unique_ptr, which now
        // owns the finished task (the Source's destructor deletes it).
        unsafe {
            let p = (&mut source as *mut v8::script_compiler::Source as *mut u8)
                .add(std::mem::offset_of!(SourceLayout, consume_cache_task))
                as *mut *mut std::ffi::c_void;
            debug_assert!((*p).is_null());
            *p = self.task;
            self.task = std::ptr::null_mut();
        }
        let r = f(&mut source);
        let rejected = source.get_cached_data().is_some_and(|d| d.rejected());
        drop(source);
        (r, rejected)
    }
}

impl Drop for CacheConsumer {
    fn drop(&mut self) {
        self.join();
        #[cfg(v8_abi)]
        if !self.task.is_null() {
            // SAFETY: the task's thread has finished and nothing else owns the task (it was not
            // handed to a Source): destroyed and freed once, as `delete task` would.
            unsafe {
                ffi::consume_task_destruct(self.task);
                ffi::operator_delete(self.task);
            }
        }
    }
}

#[cfg(test)]
mod consumer_tests {
    use super::*;

    fn origin<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::ScriptOrigin<'s> {
        let name = v8::String::new(scope, "https://a.test/c.js").unwrap();
        v8::ScriptOrigin::new(
            scope,
            name.into(),
            0,
            0,
            false,
            0,
            None,
            false,
            false,
            false,
            None,
        )
    }

    fn consume_and_run(
        scope: &mut v8::PinScope<'_, '_>,
        data: Vec<u8>,
        text: &str,
    ) -> (String, bool) {
        let consumer = CacheConsumer::start(scope, data, Box::new(|| {})).unwrap();
        let code = v8::String::new(scope, text).unwrap();
        let o = origin(scope);
        let (script, rejected) = consumer.consume(code, &o, |src| {
            v8::script_compiler::compile(
                scope,
                src,
                v8::script_compiler::CompileOptions::ConsumeCodeCache,
                v8::script_compiler::NoCacheReason::NoReason,
            )
        });
        let v = script.unwrap().run(scope).unwrap();
        (v.to_rust_string_lossy(scope), rejected)
    }

    #[test]
    fn consumes_an_entry_off_thread_and_rejects_another_scripts() {
        crate::init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let context = {
            v8::scope!(let scope, &mut isolate);
            let c = v8::Context::new(scope, v8::ContextOptions::default());
            v8::Global::new(scope, c)
        };
        v8::scope_with_context!(scope, &mut isolate, &context);
        let text = "function add(a, b) { return a + b; }\nadd(40, 2)";
        let data = {
            let code = v8::String::new(scope, text).unwrap();
            let o = origin(scope);
            let script = v8::Script::compile(scope, code, Some(&o)).unwrap();
            script.run(scope).unwrap();
            script
                .get_unbound_script(scope)
                .create_code_cache()
                .unwrap()
                .to_vec()
        };
        assert_eq!(
            consume_and_run(scope, data.clone(), text),
            ("42".to_owned(), false)
        );
        // Another source of another length: V8 rejects the entry and compiles the text.
        let other = "function add(a, b) { return a - b; }\n add(40, 2)";
        assert_eq!(consume_and_run(scope, data, other), ("38".to_owned(), true));
        // Started and dropped unused.
        drop(CacheConsumer::start(scope, vec![1, 2, 3], Box::new(|| {})));
    }
}
