// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The on-disk form of a profile's databases, and the thread that reads and writes it.
//!
//! Layout: `<root>/<storage key folder>/<database file>.idb`. The folder name is the storage key with
//! unsafe characters replaced plus a hash of the key; the file name is a hash of the database name
//! (names are arbitrary strings), and the file's header holds the name itself.
//!
//! A database file is a header (`OSMIDB` 1 0, the name's length and UTF-8 bytes) followed by commit
//! records: the payload length (u32 LE), the CRC-32 of the payload, then the payload: the committed
//! transaction's [`Change`]s. A commit is one record written with one write, so it is in the file
//! whole or, after a crash mid-write, fails its length or CRC check and is dropped (the file is cut
//! back to the last good record when it is next read). Reading replays the records; a file whose
//! records mostly hold overwritten data is compacted then (its snapshot written to a temporary file
//! and renamed over it).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::backend::Failure;
use crate::db::{encode_change, Database};
use crate::key::Reader;

const MAGIC: &[u8; 8] = b"OSMIDB\x01\x00";

/// Files larger than this are compacted on load when most of their bytes are dead.
const COMPACT_MIN: u64 = 1 << 20;

type Done<T> = Box<dyn FnOnce(T) + Send>;

enum Job {
    LoadKey(String, Done<Vec<Database>>),
    Append {
        key: String,
        name: String,
        bytes: Vec<u8>,
        sync: bool,
        done: Done<Result<(), Failure>>,
    },
    Delete {
        key: String,
        name: String,
        done: Done<Result<(), Failure>>,
    },
    Usage(String, Done<u64>),
    Flush(Sender<()>),
}

/// The disk thread.
pub struct Disk {
    tx: Mutex<Option<Sender<Job>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for Disk {
    /// Finishes the jobs sent so far and closes the files.
    fn drop(&mut self) {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(t) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            // (the last reference can go on the disk thread itself, in a job's callback)
            if t.thread().id() != std::thread::current().id() {
                let _ = t.join();
            }
        }
    }
}

impl Disk {
    /// Starts the thread over `root`.
    pub fn start(root: PathBuf) -> Disk {
        let (tx, rx) = channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("snowlark-idb-disk".into())
            .spawn(move || {
                let mut io = Io {
                    root,
                    files: HashMap::new(),
                };
                while let Ok(job) = rx.recv() {
                    io.run(job);
                }
            })
            .expect("start the IndexedDB disk thread");
        Disk {
            tx: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
        }
    }

    fn send(&self, job: Job) {
        if let Some(tx) = &*self.tx.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = tx.send(job);
        }
    }

    /// Reads every database of a storage key.
    pub fn load_key(&self, key: &str, done: Done<Vec<Database>>) {
        self.send(Job::LoadKey(key.to_owned(), done));
    }

    /// Appends one commit record to a database's file (made on first use).
    pub fn append(
        &self,
        key: &str,
        name: &str,
        bytes: Vec<u8>,
        sync: bool,
        done: Done<Result<(), Failure>>,
    ) {
        self.send(Job::Append {
            key: key.to_owned(),
            name: name.to_owned(),
            bytes,
            sync,
            done,
        });
    }

    /// Deletes a database's file.
    pub fn delete(&self, key: &str, name: &str, done: Done<Result<(), Failure>>) {
        self.send(Job::Delete {
            key: key.to_owned(),
            name: name.to_owned(),
            done,
        });
    }

    /// The bytes a storage key's database files take, after the jobs sent before have run.
    pub fn usage(&self, key: &str, done: Done<u64>) {
        self.send(Job::Usage(key.to_owned(), done));
    }

    /// Returns once every job sent before has run.
    pub fn flush(&self) {
        let (tx, rx) = channel();
        self.send(Job::Flush(tx));
        let _ = rx.recv();
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A storage key's folder name.
pub fn key_folder(key: &str) -> String {
    let safe: String = key
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let h = Sha256::digest(key.as_bytes());
    format!("{safe}-{}", hex(&h[..6]))
}

/// A database's file name.
pub fn database_file(name: &str) -> String {
    let h = Sha256::digest(name.as_bytes());
    format!("{}.idb", hex(&h[..16]))
}

fn failure(e: &std::io::Error) -> Failure {
    // (ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL; ENOSPC)
    match e.raw_os_error() {
        Some(39) | Some(112) | Some(28) => Failure::Quota,
        _ => Failure::Unknown,
    }
}

/// Encodes one commit record.
pub fn record(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn header(name: &str) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

/// Replays a database file: the database, and the length of its good prefix.
pub fn parse(bytes: &[u8]) -> Option<(Database, usize)> {
    let mut r = Reader::new(bytes);
    if r.take(8)? != MAGIC {
        return None;
    }
    let name = r.string()?;
    let mut db = Database::new(&name);
    let mut good = r.pos;
    while !r.at_end() {
        let Some(len) = r.u32() else { break };
        let Some(crc) = r.u32() else { break };
        let Some(payload) = r.take(len as usize) else {
            break;
        };
        if crc32fast::hash(payload) != crc {
            break;
        }
        let mut changes = Vec::new();
        let mut pr = Reader::new(payload);
        let mut ok = true;
        while !pr.at_end() {
            match pr.change() {
                Some(c) => changes.push(c),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            break;
        }
        for c in changes {
            db.apply(c);
        }
        good = r.pos;
    }
    Some((db, good))
}

struct Io {
    root: PathBuf,
    files: HashMap<(String, String), File>,
}

impl Io {
    fn run(&mut self, job: Job) {
        match job {
            Job::LoadKey(key, done) => done(self.load_key(&key)),
            Job::Usage(key, done) => {
                // (each file asked by its path: a listing's sizes are stale for a file held open)
                let size = std::fs::read_dir(self.root.join(key_folder(&key)))
                    .map(|dir| {
                        dir.filter_map(|e| std::fs::metadata(e.ok()?.path()).ok())
                            .filter(|m| m.is_file())
                            .map(|m| m.len())
                            .sum()
                    })
                    .unwrap_or(0);
                done(size)
            }
            Job::Append {
                key,
                name,
                bytes,
                sync,
                done,
            } => done(self.append(&key, &name, &bytes, sync)),
            Job::Delete { key, name, done } => {
                self.files.remove(&(key.clone(), name.clone()));
                let path = self.root.join(key_folder(&key)).join(database_file(&name));
                let r = match std::fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(failure(&e)),
                };
                done(r)
            }
            Job::Flush(tx) => {
                let _ = tx.send(());
            }
        }
    }

    fn load_key(&mut self, key: &str) -> Vec<Database> {
        let dir = self.root.join(key_folder(key));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            match path.extension().and_then(|x| x.to_str()) {
                Some("idb") => {}
                Some("tmp") => {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                _ => continue,
            }
            if let Some(db) = load_file(&path) {
                out.push(db);
            }
        }
        out
    }

    fn append(&mut self, key: &str, name: &str, payload: &[u8], sync: bool) -> Result<(), Failure> {
        let k = (key.to_owned(), name.to_owned());
        if !self.files.contains_key(&k) {
            let dir = self.root.join(key_folder(key));
            std::fs::create_dir_all(&dir).map_err(|e| failure(&e))?;
            let path = dir.join(database_file(name));
            let exists = path.exists();
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| failure(&e))?;
            if !exists || f.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
                f.write_all(&header(name)).map_err(|e| failure(&e))?;
            }
            self.files.insert(k.clone(), f);
        }
        let f = self.files.get_mut(&k).expect("the file");
        let rec = record(payload);
        if let Err(e) = f.write_all(&rec) {
            self.files.remove(&k);
            return Err(failure(&e));
        }
        if sync {
            f.sync_data().map_err(|e| failure(&e))?;
        }
        Ok(())
    }
}

/// Reads one database file, cutting a torn tail and compacting a file of mostly dead records.
fn load_file(path: &Path) -> Option<Database> {
    let mut bytes = Vec::new();
    File::open(path).ok()?.read_to_end(&mut bytes).ok()?;
    let (db, good) = parse(&bytes)?;
    if good < bytes.len() {
        if let Ok(f) = OpenOptions::new().write(true).open(path) {
            let _ = f.set_len(good as u64);
        }
    }
    if good as u64 > COMPACT_MIN {
        let mut payload = Vec::new();
        for c in db.snapshot() {
            encode_change(&c, &mut payload);
        }
        if payload.len() * 2 < good {
            let tmp = path.with_extension("tmp");
            let write = || -> std::io::Result<()> {
                let mut f = File::create(&tmp)?;
                f.write_all(&header(&db.name))?;
                f.write_all(&record(&payload))?;
                f.sync_all()?;
                drop(f);
                std::fs::rename(&tmp, path)
            };
            if write().is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    Some(db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Change, Record};
    use crate::key::{Key, KeyRange};
    use std::sync::Arc;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("snowlark-idb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn put(k: f64, v: &[u8]) -> Change {
        Change::Put {
            store: 1,
            key: Key::Number(k),
            record: Record {
                value: Arc::from(v),
                index_keys: vec![],
            },
        }
    }

    fn commit(disk: &Disk, changes: &[Change]) {
        let mut bytes = Vec::new();
        for c in changes {
            encode_change(c, &mut bytes);
        }
        let (tx, rx) = channel();
        disk.append(
            "https://a.test",
            "db",
            bytes,
            true,
            Box::new(move |r| tx.send(r).unwrap()),
        );
        rx.recv().unwrap().unwrap();
    }

    fn load(disk: &Disk) -> Vec<Database> {
        let (tx, rx) = channel();
        disk.load_key("https://a.test", Box::new(move |d| tx.send(d).unwrap()));
        rx.recv().unwrap()
    }

    #[test]
    fn commits_survive_a_restart_and_a_torn_tail_is_dropped() {
        let root = temp("restart");
        {
            let disk = Disk::start(root.clone());
            commit(
                &disk,
                &[
                    Change::Version(1),
                    Change::CreateStore {
                        id: 1,
                        name: "s".into(),
                        key_path: None,
                        auto_increment: true,
                    },
                    put(1.0, b"one"),
                ],
            );
            commit(&disk, &[put(2.0, b"two"), Change::Generator(1, 3)]);
        }
        // A crash in the middle of a third commit leaves half a record.
        let file = root
            .join(key_folder("https://a.test"))
            .join(database_file("db"));
        let mut payload = Vec::new();
        encode_change(&put(3.0, b"three"), &mut payload);
        let rec = record(&payload);
        let mut f = OpenOptions::new().append(true).open(&file).unwrap();
        f.write_all(&rec[..rec.len() - 3]).unwrap();
        drop(f);
        let disk = Disk::start(root.clone());
        let dbs = load(&disk);
        assert_eq!(dbs.len(), 1);
        let db = &dbs[0];
        assert_eq!((db.name.as_str(), db.version), ("db", 1));
        let s = db.store(1).unwrap();
        assert_eq!(s.records.len(), 2);
        assert_eq!(s.current, 3);
        // The torn tail was cut: a new commit after it reads back.
        commit(
            &disk,
            &[Change::DeleteRange(1, KeyRange::only(Key::Number(1.0)))],
        );
        let dbs = load(&disk);
        assert_eq!(dbs[0].store(1).unwrap().records.len(), 1);
        let (tx, rx) = channel();
        disk.delete(
            "https://a.test",
            "db",
            Box::new(move |r| tx.send(r).unwrap()),
        );
        rx.recv().unwrap().unwrap();
        assert!(load(&disk).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compaction_keeps_the_data() {
        let root = temp("compact");
        let disk = Disk::start(root.clone());
        commit(
            &disk,
            &[
                Change::Version(2),
                Change::CreateStore {
                    id: 1,
                    name: "s".into(),
                    key_path: None,
                    auto_increment: false,
                },
            ],
        );
        let big = vec![7u8; 64 * 1024];
        for _ in 0..40 {
            commit(&disk, &[put(1.0, &big)]);
        }
        let file = root
            .join(key_folder("https://a.test"))
            .join(database_file("db"));
        let before = std::fs::metadata(&file).unwrap().len();
        drop(disk);
        let disk = Disk::start(root.clone());
        let dbs = load(&disk);
        let after = std::fs::metadata(&file).unwrap().len();
        assert!(after * 10 < before, "{before} -> {after}");
        assert_eq!(dbs[0].version, 2);
        assert_eq!(dbs[0].store(1).unwrap().records.len(), 1);
        assert_eq!(load(&disk)[0].store(1).unwrap().records.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names() {
        assert_ne!(database_file("a"), database_file("A"));
        assert!(key_folder("https://example.com:8443").starts_with("https___example.com_8443-"));
    }
}
