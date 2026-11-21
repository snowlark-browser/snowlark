// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Indexed Database API 3.0 through the page: databases that persist in a profile folder across a
//! restart, upgrades seen by other pages (`versionchange`, `blocked`), aborts rolling back, and the
//! transaction staying active in the microtasks of its request's success listener.

use std::time::Duration;

use snowlark_page::{IdbBackend, Page, PageOptions};

fn page(idb: &IdbBackend) -> Page {
    let mut page = Page::new(PageOptions {
        idb: Some(idb.clone()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    page.run_until_settled(Duration::from_secs(5));
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Runs an async script that stores its result in `window.out`, then reads it.
fn run_async(page: &mut Page, js: &str) -> String {
    eval(
        page,
        &format!("window.out = 'pending'; (async () => {{ {js} }})().then(v => out = String(v), e => out = 'threw ' + e);"),
    );
    page.run_until(std::time::Instant::now() + Duration::from_secs(10), |p| {
        p.global_string("out").is_some_and(|s| s != "pending")
    });
    eval(page, "out")
}

/// Helpers the scripts share: promises over requests and transactions.
const HELPERS: &str = r#"
    const req = r => new Promise((ok, err) => { r.onsuccess = () => ok(r.result); r.onerror = () => err(r.error); });
    const done = t => new Promise((ok, err) => { t.oncomplete = ok; t.onabort = () => err(t.error); });
"#;

fn temp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("snowlark-page-idb-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// "Convert a value to a key": an array key nested deeper than 2,000 levels is refused with a
/// "DataError" (the specification sets no depth; deeper keys overflowed a thread's stack in the
/// storage), and one of 2,000 levels is a key.
#[test]
fn array_keys_nested_too_deep_are_a_data_error() {
    let dir = temp("deep-keys");
    let idb = IdbBackend::persistent(dir.clone());
    let mut p = page(&idb);
    let r = eval(
        &mut p,
        "const nest = n => { let k = 0; for (let i = 0; i < n; i++) k = [k]; return k; };
         const cmp = n => { try { return String(indexedDB.cmp(nest(n), 1)); } catch (e) { return e.name; } };
         [cmp(2000), cmp(2001), cmp(10000)].join()",
    );
    assert_eq!(r, "1,DataError,DataError");
    drop(p);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn databases_survive_a_restart() {
    let dir = temp("restart");
    {
        let idb = IdbBackend::persistent(dir.clone());
        let mut p = page(&idb);
        let r = run_async(
            &mut p,
            &format!(
                r#"{HELPERS}
                const open = indexedDB.open('books', 3);
                open.onupgradeneeded = () => {{
                  const s = open.result.createObjectStore('s', {{ keyPath: 'id', autoIncrement: true }});
                  s.createIndex('by_title', 'title', {{ unique: true }});
                  s.put({{ title: 'a', blob: new Blob(['xyz'], {{ type: 'text/plain' }}) }});
                }};
                const db = await req(open);
                const t = db.transaction('s', 'readwrite', {{ durability: 'strict' }});
                t.objectStore('s').put({{ title: 'b', when: new Date(5) }});
                t.objectStore('s').put({{ title: 'c' }}, undefined);
                await done(t);
                // An aborted transaction leaves nothing behind.
                const t2 = db.transaction('s', 'readwrite');
                t2.objectStore('s').put({{ title: 'never' }});
                t2.abort();
                db.close();
                return 'stored';
                "#
            ),
        );
        assert_eq!(r, "stored");
        drop(p);
        idb.flush();
    }
    let idb = IdbBackend::persistent(dir.clone());
    let mut p = page(&idb);
    let r = run_async(
        &mut p,
        &format!(
            r#"{HELPERS}
            const list = await indexedDB.databases();
            const db = await req(indexedDB.open('books'));
            const s = db.transaction('s').objectStore('s');
            const all = await req(s.getAll());
            const byTitle = await req(s.index('by_title').getKey('b'));
            const blob = all[0].blob;
            const text = await blob.text();
            const next = db.transaction('s', 'readwrite').objectStore('s').add({{ title: 'd' }});
            const key = await req(next);
            return [JSON.stringify(list), db.version, all.map(r => r.id + r.title).join(), byTitle,
                    text, blob.type, all[1].when.getTime(), key].join(' ');
            "#
        ),
    );
    assert_eq!(
        r,
        r#"[{"name":"books","version":3}] 3 1a,2b,3c 2 xyz text/plain 5 4"#
    );
    drop(p);
    drop(idb);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs `page`'s event loop until `js` evaluates to "true" (or 10 s pass).
fn run_until_true(page: &mut Page, js: &str) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        page.run_until_settled(Duration::from_millis(20));
        if page.eval(js, "test.js").is_ok_and(|v| v == "true") {
            return true;
        }
    }
    false
}

#[test]
fn an_upgrade_from_another_page_fires_versionchange_and_blocked() {
    use std::sync::mpsc::channel;
    // Each page on its own thread (a thread runs one isolate at a time), sharing the backend.
    let idb = IdbBackend::in_memory();
    let (opened_tx, opened_rx) = channel::<()>();
    let (close_tx, close_rx) = channel::<()>();
    let a_idb = idb.clone();
    let a = std::thread::spawn(move || {
        let mut a = page(&a_idb);
        eval(
            &mut a,
            r#"window.seen = [];
            const o = indexedDB.open('shared', 1);
            o.onsuccess = () => {
              window.db = o.result;
              db.onversionchange = e => seen.push('versionchange ' + e.oldVersion + '>' + e.newVersion);
            };"#,
        );
        assert!(run_until_true(&mut a, "!!window.db"));
        opened_tx.send(()).unwrap();
        // Page A keeps its connection open until told to close it.
        while close_rx.try_recv().is_err() {
            a.run_until_settled(Duration::from_millis(20));
            std::thread::sleep(Duration::from_millis(2));
        }
        let seen = eval(&mut a, "seen.join()");
        eval(&mut a, "db.close()");
        a.run_until_settled(Duration::from_millis(100));
        seen
    });
    opened_rx.recv().unwrap();
    let mut b = page(&idb);
    eval(
        &mut b,
        r#"window.events = [];
        const o = indexedDB.open('shared', 2);
        o.onblocked = e => events.push('blocked ' + e.oldVersion + '>' + e.newVersion);
        o.onupgradeneeded = e => events.push('upgradeneeded ' + e.oldVersion);
        o.onsuccess = () => events.push('success ' + o.result.version);"#,
    );
    assert!(run_until_true(&mut b, "events.length == 1"));
    assert_eq!(eval(&mut b, "events.join()"), "blocked 1>2");
    // Closing the first connection lets the upgrade go on.
    close_tx.send(()).unwrap();
    let seen = a.join().unwrap();
    assert_eq!(seen, "versionchange 1>2");
    assert!(run_until_true(&mut b, "events.length == 3"));
    assert_eq!(
        eval(&mut b, "events.join()"),
        "blocked 1>2,upgradeneeded 1,success 2"
    );
}

#[test]
fn a_transaction_stays_active_in_its_success_microtasks() {
    let idb = IdbBackend::in_memory();
    let mut p = page(&idb);
    let r = run_async(
        &mut p,
        &format!(
            r#"{HELPERS}
            const open = indexedDB.open('m', 1);
            open.onupgradeneeded = () => open.result.createObjectStore('s');
            const db = await req(open);
            const t = db.transaction('s', 'readwrite');
            const s = t.objectStore('s');
            // Each await resumes in a microtask of the previous request's success event.
            await req(s.put(1, 'a'));
            await req(s.put(2, 'b'));
            const n = await req(s.count());
            await done(t);
            let threw = '';
            try {{ s.put(3, 'c'); }} catch (e) {{ threw = e.name; }}
            return n + ' ' + threw;
            "#
        ),
    );
    assert_eq!(r, "2 TransactionInactiveError");
}

#[test]
fn finished_requests_are_collected() {
    let idb = IdbBackend::in_memory();
    let mut p = page(&idb);
    let r = run_async(
        &mut p,
        &format!(
            r#"{HELPERS}
            const open = indexedDB.open('gc', 1);
            open.onupgradeneeded = () => open.result.createObjectStore('s');
            const db = await req(open);
            const t = db.transaction('s', 'readwrite');
            const s = t.objectStore('s');
            for (let i = 0; i < 3000; i++) {{
              s.put({{ big: 'x'.repeat(100), i }}, i);
              s.get(i);
              IDBKeyRange.bound(i, i + 1);
            }}
            await done(t);
            return 'ok';
            "#
        ),
    );
    assert_eq!(r, "ok");
    for _ in 0..4 {
        assert!(p.collect_garbage());
    }
    // The 6000 requests and 3000 key ranges are gone (their results with them).
    let (_, objects) = p.engine().live_counts();
    assert!(objects < 2000, "{objects} objects live");
}

/// A request the page holds no reference to keeps its listeners while its answer is on the way: a
/// collection between any two turns of the loop loses none of its events.
#[test]
fn a_request_waiting_for_its_answer_keeps_its_listeners() {
    let idb = IdbBackend::in_memory();
    let mut p = page(&idb);
    let r = run_async(
        &mut p,
        &format!(
            r#"{HELPERS}
            const open = indexedDB.open('waiting', 1);
            open.onupgradeneeded = () => open.result.createObjectStore('s').put('v', 1);
            window.db = await req(open);
            return 'ok';
            "#
        ),
    );
    assert_eq!(r, "ok");
    eval(
        &mut p,
        "window.fired = 0; { const s = db.transaction('s').objectStore('s'); \
         for (let i = 0; i < 50; i++) s.get(1).onsuccess = () => fired++; }",
    );
    p.run_until(std::time::Instant::now() + Duration::from_secs(10), |p| {
        p.collect_garbage();
        p.global_string("fired").as_deref() == Some("50")
    });
    assert_eq!(eval(&mut p, "String(fired)"), "50");
}

/// Requests a backend received and has not run: (loop, transaction, number, request).
type Requests = Vec<(snowlark_idb::LoopId, u64, u64, snowlark_idb::Request)>;

/// The in-memory backend, except that while `held` is `Some` it keeps the transactions' requests
/// there, unanswered, until [`Held::release`].
struct Held {
    idb: snowlark_idb::LocalIdb,
    held: std::sync::Mutex<Option<Requests>>,
}

impl Held {
    fn hold(&self) {
        *self.held.lock().unwrap() = Some(Vec::new());
    }

    /// How many requests it holds.
    fn holding(&self) -> usize {
        self.held.lock().unwrap().as_ref().map_or(0, Vec::len)
    }

    fn release(&self) {
        let held = self.held.lock().unwrap().take().unwrap_or_default();
        for (l, txn, seq, request) in held {
            self.idb.request(l, txn, seq, request);
        }
    }
}

impl snowlark_idb::IdbService for Held {
    fn register_loop(&self, inbox: &std::sync::Arc<snowlark_idb::Inbox>) -> snowlark_idb::LoopId {
        self.idb.register_loop(inbox)
    }
    fn loop_gone(&self, l: snowlark_idb::LoopId) {
        self.idb.loop_gone(l)
    }
    fn open(&self, l: snowlark_idb::LoopId, req: u64, key: &str, name: &str, v: Option<u64>) {
        self.idb.open(l, req, key, name, v)
    }
    fn delete_database(&self, l: snowlark_idb::LoopId, req: u64, key: &str, name: &str) {
        self.idb.delete_database(l, req, key, name)
    }
    fn databases(&self, l: snowlark_idb::LoopId, id: u64, key: &str) {
        self.idb.databases(l, id, key)
    }
    fn estimate(&self, l: snowlark_idb::LoopId, id: u64, key: &str) {
        self.idb.estimate(l, id, key)
    }
    fn versionchange_fired(&self, conn: u64) {
        self.idb.versionchange_fired(conn)
    }
    fn connection_closed(&self, conn: u64) {
        self.idb.connection_closed(conn)
    }
    fn upgrade_finished(&self, txn: u64, committed: bool) {
        self.idb.upgrade_finished(txn, committed)
    }
    fn create_transaction(&self, conn: u64, mode: snowlark_idb::Mode, scope: Vec<u64>) -> u64 {
        self.idb.create_transaction(conn, mode, scope)
    }
    fn commit(&self, txn: u64, durability: snowlark_idb::Durability) {
        self.idb.commit(txn, durability)
    }
    fn transaction_finished(&self, txn: u64) {
        self.idb.transaction_finished(txn)
    }
    fn change(&self, txn: u64, change: snowlark_idb::Change) {
        snowlark_idb::IdbService::change(&self.idb, txn, change)
    }
    fn revert(&self, txn: u64) {
        self.idb.revert(txn)
    }
    fn request(&self, l: snowlark_idb::LoopId, txn: u64, seq: u64, request: snowlark_idb::Request) {
        let mut held = self.held.lock().unwrap();
        match held.as_mut() {
            Some(h) => h.push((l, txn, seq, request)),
            None => {
                drop(held);
                self.idb.request(l, txn, seq, request)
            }
        }
    }
    fn flush(&self) {
        self.idb.flush()
    }
}

/// §6.1 with a key generator: `add()`s with no key all go to the backend before the first is
/// answered (the backend generates the keys), and a generated in-line key is in the value, in the
/// indexes that read it, and in every way the record is read.
#[test]
fn adds_to_a_key_generator_go_without_waiting_and_read_back_with_their_keys() {
    let held = std::sync::Arc::new(Held {
        idb: snowlark_idb::LocalIdb::in_memory(),
        held: Default::default(),
    });
    let idb = IdbBackend::new(held.clone());
    let mut p = page(&idb);
    let r = run_async(
        &mut p,
        &format!(
            r#"{HELPERS}
            const open = indexedDB.open('generated', 1);
            open.onupgradeneeded = () => {{
                const s = open.result.createObjectStore('s', {{keyPath: 'id', autoIncrement: true}});
                s.createIndex('by_id', 'id', {{unique: true}});
                s.createIndex('pair', ['x', 'id']);
                s.createIndex('deep', 'id.no');
                open.result.createObjectStore('out', {{autoIncrement: true}});
            }};
            window.db = await req(open);
            return 'ok';
            "#
        ),
    );
    assert_eq!(r, "ok");
    held.hold();
    eval(
        &mut p,
        "window.tx = db.transaction(['s', 'out'], 'readwrite'); window.keys = []; \
         { const s = tx.objectStore('s'), o = tx.objectStore('out'); \
           for (let i = 1; i <= 10; i++) s.add({x: 'k' + i}).onsuccess = e => keys.push(e.target.result); \
           o.add('a'); o.add('b'); }",
    );
    p.run_until_settled(Duration::from_millis(200));
    // Every request was sent, none answered: nothing waited for a key.
    assert_eq!(held.holding(), 12);
    held.release();
    let r = run_async(
        &mut p,
        &format!(
            r#"{HELPERS}
            await done(tx);
            const t = db.transaction(['s', 'out']);
            const s = t.objectStore('s');
            const got = await req(s.get(4));
            const byId = await req(s.index('by_id').get(4));
            const pair = await req(s.index('pair').getKey(['k4', 4]));
            const deep = await req(s.index('deep').count());
            const all = (await req(s.getAll())).map(v => v.id + v.x).join(',');
            const cur = await new Promise(ok => {{ s.openCursor(7).onsuccess = e => ok(e.target.result.value); }});
            const out = (await req(t.objectStore('out').getAll())).join('');
            const outKeys = (await req(t.objectStore('out').getAllKeys())).join(',');
            return [keys.join(','), JSON.stringify(got), JSON.stringify(byId), pair, deep, all,
                    JSON.stringify(cur), out, outKeys].join(' | ');
            "#
        ),
    );
    assert_eq!(
        r,
        "1,2,3,4,5,6,7,8,9,10 | {\"x\":\"k4\",\"id\":4} | {\"x\":\"k4\",\"id\":4} | 4 | 0 | \
         1k1,2k2,3k3,4k4,5k5,6k6,7k7,8k8,9k9,10k10 | {\"x\":\"k7\",\"id\":7} | ab | 1,2"
    );
}

/// A `CryptoKey` is serializable (WebCrypto §14 "Structured clone"): a non-extractable key pair
/// stored in a database reads back after a restart as keys that still work together, with their
/// type, algorithm, usages and extractability.
#[test]
fn crypto_keys_are_stored_and_survive_a_restart() {
    let dir = temp("cryptokey");
    let secure_page = |idb: &IdbBackend| {
        let mut page = Page::new(PageOptions {
            idb: Some(idb.clone()),
            ..Default::default()
        });
        page.load_html(
            "<!doctype html>",
            &url::Url::parse("https://example.test/").unwrap(),
        );
        page.run_until_settled(Duration::from_secs(5));
        page
    };
    let open = r#"
        const open = indexedDB.open('keys', 1);
        open.onupgradeneeded = () => open.result.createObjectStore('k');
        const db = await req(open);
    "#;
    {
        let idb = IdbBackend::persistent(dir.clone());
        let mut p = secure_page(&idb);
        let r = run_async(
            &mut p,
            &format!(
                r#"{HELPERS}{open}
                const pair = await crypto.subtle.generateKey({{name: 'RSA-OAEP', modulusLength: 2048,
                    publicExponent: new Uint8Array([1, 0, 1]), hash: 'SHA-1'}}, false, ['wrapKey', 'unwrapKey']);
                const hmac = await crypto.subtle.generateKey({{name: 'HMAC', hash: 'SHA-256'}}, false, ['sign']);
                const t = db.transaction('k', 'readwrite');
                t.objectStore('k').put({{pair, hmac}}, 'msl');
                await done(t);
                const back = await req(db.transaction('k').objectStore('k').get('msl'));
                return back.pair.privateKey instanceof CryptoKey && back.hmac.algorithm.hash.name;
                "#
            ),
        );
        assert_eq!(r, "SHA-256");
    }
    let idb = IdbBackend::persistent(dir.clone());
    let mut p = secure_page(&idb);
    let r = run_async(
        &mut p,
        &format!(
            r#"{HELPERS}{open}
            const {{pair, hmac}} = await req(db.transaction('k').objectStore('k').get('msl'));
            const aes = await crypto.subtle.generateKey({{name: 'AES-CBC', length: 128}}, true, ['encrypt']);
            const wrapped = await crypto.subtle.wrapKey('jwk', aes, pair.publicKey, {{name: 'RSA-OAEP'}});
            const un = await crypto.subtle.unwrapKey('jwk', wrapped, pair.privateKey, {{name: 'RSA-OAEP'}},
                'AES-CBC', false, ['encrypt']);
            const mac = await crypto.subtle.sign('HMAC', hmac, new Uint8Array(4));
            const k = pair.privateKey;
            return [k.type, k.extractable, k.usages.join(), k.algorithm.name, k.algorithm.hash.name,
                    k.algorithm.modulusLength, un.algorithm.length, mac.byteLength].join(' ');
            "#
        ),
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(r, "private false unwrapKey RSA-OAEP SHA-1 2048 128 32");
}
