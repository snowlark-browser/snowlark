// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Storage Standard's `navigator.storage` through the page: one StorageManager per global,
//! `persisted()` and `persist()` answering false in a task, and `estimate()` reporting what the
//! origin's IndexedDB files take on disk and the quota.

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

/// Runs an async script and returns what it resolves to (or "threw ...").
fn run_async(page: &mut Page, js: &str) -> String {
    page.eval(
        &format!("window.out = 'pending'; (async () => {{ {js} }})().then(v => out = String(v), e => out = 'threw ' + e);"),
        "test.js",
    )
    .unwrap();
    page.run_until(std::time::Instant::now() + Duration::from_secs(10), |p| {
        p.global_string("out").is_some_and(|s| s != "pending")
    });
    page.eval("out", "test.js").unwrap()
}

#[test]
fn persisted_and_persist_answer_false_in_a_task() {
    let mut p = page(&IdbBackend::in_memory());
    let r = run_async(
        &mut p,
        r#"
        const s = navigator.storage;
        if (s !== navigator.storage || !(s instanceof StorageManager)) return 'not the same object';
        const order = [];
        const a = s.persisted().then(v => order.push('persisted ' + v));
        Promise.resolve().then(() => order.push('microtask'));
        const b = s.persist().then(v => order.push('persist ' + v));
        await Promise.all([a, b]);
        return order.join(',');
        "#,
    );
    assert_eq!(r, "microtask,persisted false,persist false");
}

#[test]
fn estimate_reports_the_origins_database_files_and_the_quota() {
    let dir = std::env::temp_dir().join(format!("snowlark-page-storage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let idb = IdbBackend::persistent(dir.clone());
    let mut p = page(&idb);
    let before = run_async(
        &mut p,
        "const e = await navigator.storage.estimate(); return JSON.stringify(e);",
    );
    assert_eq!(before, r#"{"quota":10737418240,"usage":0}"#);
    let after = run_async(
        &mut p,
        r#"
        const open = indexedDB.open('d', 1);
        open.onupgradeneeded = () => open.result.createObjectStore('s');
        await new Promise((ok, err) => { open.onsuccess = ok; open.onerror = err; });
        const t = open.result.transaction('s', 'readwrite');
        t.objectStore('s').put('x'.repeat(5000), 1);
        await new Promise((ok, err) => { t.oncomplete = ok; t.onabort = err; });
        return (await navigator.storage.estimate()).usage;
        "#,
    );
    idb.service().flush();
    let on_disk: u64 = std::fs::read_dir(&dir)
        .unwrap()
        .flat_map(|d| std::fs::read_dir(d.unwrap().path()).unwrap())
        .map(|f| std::fs::metadata(f.unwrap().path()).unwrap().len())
        .sum();
    assert!(on_disk > 5000, "{on_disk} on disk, {after} estimated");
    assert_eq!(after, on_disk.to_string());
    drop(p);
    let _ = std::fs::remove_dir_all(&dir);
}
