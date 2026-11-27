// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Cache API through the page (Service Workers §5.4 to §5.5): entries put by one page are
//! matched by another page of the same storage key and kept across a restart, an opaque origin is
//! refused, and a Request's copy follows the abort of the signal its source follows (Fetch §5.4).

use std::time::Duration;

use snowlark_page::{CacheBackend, Page, PageOptions};

fn page_at(caches: &CacheBackend, url: &str) -> Page {
    let mut page = Page::new(PageOptions {
        caches: Some(caches.clone()),
        ..Default::default()
    });
    page.load_html("<!doctype html>", &url::Url::parse(url).unwrap());
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
        &format!("window.out = 'pending'; (async () => {{ {js} }})().then(v => out = String(v), e => out = 'threw ' + e.name);"),
    );
    page.run_until(std::time::Instant::now() + Duration::from_secs(10), |p| {
        p.global_string("out").is_some_and(|s| s != "pending")
    });
    page.global_string("out").unwrap_or_default()
}

#[test]
fn entries_are_shared_by_a_storage_key_and_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("snowlark-cache-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let storage = || {
        CacheBackend::new(std::sync::Arc::new(
            snowlark_sw::LocalCacheStorage::persistent(dir.clone()),
        ))
    };
    {
        let caches = storage();
        let mut a = page_at(&caches, "https://example.test/a");
        let r = run_async(
            &mut a,
            r#"
            const c = await caches.open('v1');
            await c.put('/app.js', new Response('let x = 1;', {headers: {'Content-Type': 'text/javascript'}}));
            await c.put(new Request('/data?id=2'), new Response(null, {status: 204}));
            const body = new Response('used');
            await c.put('/used', body);
            return [body.bodyUsed, (await caches.keys()).join()].join();
            "#,
        );
        assert_eq!(r, "true,v1");
        let mut b = page_at(&caches, "https://example.test/b");
        let r = run_async(
            &mut b,
            r#"
            const hit = await caches.match('/app.js');
            const c = await caches.open('v1');
            const keys = (await c.keys()).map(r => new URL(r.url).pathname);
            const search = await c.match('/data', {ignoreSearch: true});
            return [await hit.text(), hit.headers.get('content-type'), keys.join(' '), search.status,
                    await c.match('/nothing') === undefined].join();
            "#,
        );
        assert_eq!(r, "let x = 1;,text/javascript,/app.js /data /used,204,true");
        let mut other = page_at(&caches, "https://other.test/");
        let r = run_async(&mut other, "return (await caches.keys()).length;");
        assert_eq!(r, "0");
    }
    let caches = storage();
    let mut a = page_at(&caches, "https://example.test/");
    let r = run_async(
        &mut a,
        r#"
        const c = await caches.open('v1');
        const r = await c.match('/app.js');
        return [await r.text(), await c.delete('/app.js'), await caches.delete('v1'), await caches.has('v1')].join();
        "#,
    );
    assert_eq!(r, "let x = 1;,true,true,false");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn put_refuses_what_the_cache_cannot_hold() {
    let caches = CacheBackend::in_memory();
    let mut p = page_at(&caches, "https://example.test/");
    let r = run_async(
        &mut p,
        r#"
        const c = await caches.open('v1');
        const outcome = async f => { try { await f(); return 'ok'; } catch (e) { return e.name; } };
        return [
            await outcome(() => c.put(new Request('/', {method: 'POST', body: 'x'}), new Response('x'))),
            await outcome(() => c.put('/', new Response('x', {status: 206}))),
            await outcome(() => c.put('/', new Response('x', {headers: {Vary: '*'}}))),
            await outcome(() => c.put('data:text/plain,x', new Response('x'))),
            await outcome(() => c.addAll(['/a', '/a'])),
        ].join();
        "#,
    );
    // (the last fetches over a network the test page does not have: a TypeError too)
    assert_eq!(r, "TypeError,TypeError,TypeError,TypeError,TypeError");
}

#[test]
fn an_opaque_origin_has_no_caches() {
    let caches = CacheBackend::in_memory();
    let mut p = page_at(&caches, "https://example.test/");
    eval(
        &mut p,
        "window.out = 'pending'; onmessage = e => out = e.data; document.body.innerHTML = '<iframe sandbox=\"allow-scripts\" srcdoc=\"<script>caches.keys().then(() => parent.postMessage(\\'ok\\', \\'*\\'), e => parent.postMessage(e.name, \\'*\\'))</script>\"></iframe>';",
    );
    p.run_until(std::time::Instant::now() + Duration::from_secs(10), |p| {
        p.global_string("out").is_some_and(|s| s != "pending")
    });
    assert_eq!(p.global_string("out").as_deref(), Some("SecurityError"));
}

#[test]
fn a_request_copy_follows_the_source_of_its_signal() {
    let caches = CacheBackend::in_memory();
    let mut p = page_at(&caches, "https://example.test/");
    let r = eval(
        &mut p,
        r#"
        const controller = new AbortController();
        const request = new Request('/x', {signal: controller.signal});
        const copy = new Request(request);
        const dropped = new Request(request, {signal: null});
        controller.abort();
        [request.signal.aborted, copy.signal.aborted, dropped.signal.aborted].join()
        "#,
    );
    assert_eq!(r, "true,true,false");
}

/// Web IDL §3.3.20: `[SecureContext]` constructs are exposed only in secure contexts. A page over
/// plain HTTP has no `caches`, no `Cache` or `CacheStorage`, so its own global `var caches` is a
/// plain variable; the same page over HTTPS has them.
#[test]
fn an_insecure_page_has_no_caches_and_keeps_its_own_variable() {
    let caches = CacheBackend::in_memory();
    let mut insecure = page_at(&caches, "http://example.test/");
    assert_eq!(
        eval(
            &mut insecure,
            "[isSecureContext, 'caches' in self, 'Cache' in self, 'CacheStorage' in self, 'SubtleCrypto' in self].join()"
        ),
        "false,false,false,false,false"
    );
    assert_eq!(
        eval(
            &mut insecure,
            "var caches = {given: 1}; typeof caches.given"
        ),
        "number"
    );
    let mut secure = page_at(&caches, "https://example.test/");
    assert_eq!(
        eval(
            &mut secure,
            "[isSecureContext, 'caches' in self, 'Cache' in self, 'CacheStorage' in self, 'SubtleCrypto' in self].join()"
        ),
        "true,true,true,true,true"
    );
}

/// HTML "secure context" goes by the top-level creation URL: a `srcdoc` frame (`about:srcdoc`, a
/// potentially trustworthy URL of its own) is a secure context in an `https:` page and not in an
/// `http:` one, with the `[SecureContext]` constructs to match.
#[test]
fn a_frames_secure_context_is_its_top_level_pages() {
    let caches = CacheBackend::in_memory();
    let probe = "{ const w = f.contentWindow; [w.isSecureContext, 'subtle' in w.crypto,\
                 'caches' in w].join() }";
    for (url, want) in [
        ("http://example.test/", "false,false,false"),
        ("https://example.test/", "true,true,true"),
    ] {
        let mut page = page_at(&caches, url);
        eval(
            &mut page,
            "window.f = document.createElement('iframe'); f.srcdoc = '<p>x';\
             window.loaded = false; f.onload = () => loaded = true;\
             document.body.appendChild(f); 'ok'",
        );
        page.run_until(std::time::Instant::now() + Duration::from_secs(5), |p| {
            p.eval("loaded", "test.js").as_deref() == Ok("true")
        });
        assert_eq!(eval(&mut page, probe), want, "{url}");
    }
}

/// HTML "secure context": a dedicated worker is one when its owner is, whatever its own URL (a
/// `data:` worker's origin is opaque).
#[test]
fn a_workers_secure_context_is_its_owners() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    for (url, want) in [
        ("http://example.test/", "false,false"),
        ("https://example.test/", "true,true"),
    ] {
        let mut page = Page::new(PageOptions {
            net: Some(net.client()),
            ..Default::default()
        });
        page.load_html("<!doctype html>", &url::Url::parse(url).unwrap());
        page.run_until_settled(Duration::from_secs(5));
        let got = run_async(
            &mut page,
            "const w = new Worker(\"data:text/javascript,postMessage([isSecureContext, 'subtle' in crypto].join())\");\
             return await new Promise((ok, no) => { w.onmessage = e => ok(e.data); w.onerror = () => no(new Error('error')); });",
        );
        assert_eq!(got, want, "{url}");
    }
}

/// Web IDL §3.3.20 for the members of other interfaces: a `[SecureContext]` attribute or operation
/// of an interface exposed in an insecure realm is not there (`navigator.storage`, `crypto.subtle`,
/// `crypto.randomUUID`), while the interface's other members are; a secure realm has them all.
#[test]
fn an_insecure_page_has_no_secure_members_of_other_interfaces() {
    let caches = CacheBackend::in_memory();
    let probe = "['storage' in navigator, 'clipboard' in navigator, 'credentials' in navigator,\
                  'subtle' in crypto, 'randomUUID' in crypto, typeof crypto.getRandomValues,\
                  typeof navigator.userAgent, 'getCoalescedEvents' in PointerEvent.prototype].join()";
    let mut insecure = page_at(&caches, "http://example.test/");
    assert_eq!(
        eval(&mut insecure, probe),
        "false,false,false,false,false,function,string,false"
    );
    let mut secure = page_at(&caches, "https://example.test/");
    assert_eq!(
        eval(&mut secure, probe),
        "true,true,true,true,true,function,string,true"
    );
}
