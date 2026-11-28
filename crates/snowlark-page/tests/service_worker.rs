// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Service worker registration and its lifecycle through the page (W3C Service Workers §3.4
//! `register()`, §8 Register, Update, Install, Activate, Unregister): the worker runs in a
//! `ServiceWorkerGlobalScope` on a thread of its own, its states reach the page in order, and the
//! registration's security checks reject what they must.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use snowlark_page::{Page, PageOptions, ServiceWorkerBackend};

/// A registry whose script fetches get `scripts` (URL, MIME type, body); every fetch must carry
/// `Service-Worker: script`.
fn registry(scripts: &[(&str, &str, &str)]) -> ServiceWorkerBackend {
    let scripts: Vec<(String, String, String)> = scripts
        .iter()
        .map(|(u, m, b)| (u.to_string(), m.to_string(), b.to_string()))
        .collect();
    let fetch: snowlark_sw::Fetch = Arc::new(move |request: snowlark_net::Request| {
        assert_eq!(
            request.headers.get("Service-Worker").as_deref(),
            Some("script")
        );
        let url = request.url().to_string();
        let Some((_, mime, body)) = scripts.iter().find(|(u, _, _)| *u == url) else {
            let mut head = snowlark_net::ResponseHead::new(404, "Not Found");
            head.url_list = vec![request.url().clone()];
            return Ok(snowlark_net::Response { head, body: None });
        };
        let mut head = snowlark_net::ResponseHead::new(200, "OK");
        head.url_list = vec![request.url().clone()];
        head.headers.append("Content-Type", mime);
        Ok(snowlark_net::Response {
            head,
            body: Some(body.clone().into_bytes().into()),
        })
    });
    ServiceWorkerBackend::new(Arc::new(snowlark_sw::LocalServiceWorkers::with_fetch(
        fetch, None,
    )))
}

fn page_at(sw: &ServiceWorkerBackend, url: &str) -> Page {
    let mut page = Page::new(PageOptions {
        service_workers: Some(sw.clone()),
        ..Default::default()
    });
    page.load_html("<!doctype html>", &url::Url::parse(url).unwrap());
    page.run_until_settled(Duration::from_secs(5));
    page
}

/// Runs an async script that stores its result in `window.out`, then reads it.
fn run_async(page: &mut Page, js: &str) -> String {
    let wrapped = format!(
        "window.out = 'pending'; (async () => {{ {js} }})().then(v => out = String(v), e => out = 'threw ' + e.name + ': ' + e.message);"
    );
    if let Err(e) = page.eval(&wrapped, "test.js") {
        panic!("{js}: threw {e}");
    }
    page.run_until(std::time::Instant::now() + Duration::from_secs(20), |p| {
        p.global_string("out").is_some_and(|s| s != "pending")
    });
    page.global_string("out").unwrap_or_default()
}

const SW: &str = r#"
self.addEventListener('install', e => {
    const seen = [self.registration.installing && self.registration.installing.state,
                  self.serviceWorker.state, self instanceof ServiceWorkerGlobalScope,
                  e instanceof ExtendableEvent].join(' ');
    e.waitUntil(new Promise(r => setTimeout(r, 50))
        .then(() => caches.open('sw'))
        .then(c => c.put('/marker', new Response(seen))));
});
self.addEventListener('activate', e => e.waitUntil(self.clients.claim()));
"#;

#[test]
fn a_registered_worker_installs_and_activates_in_its_own_global() {
    let sw = registry(&[("https://a.test/sw.js", "text/javascript", SW)]);
    let mut page = page_at(&sw, "https://a.test/index.html");
    let got = run_async(
        &mut page,
        r#"
        const reg = await navigator.serviceWorker.register('/sw.js');
        const w = reg.installing;
        const states = [w.state];
        await new Promise(ok => w.onstatechange = () => {
            states.push(w.state);
            if (w.state === 'activated') ok();
        });
        const ready = await navigator.serviceWorker.ready;
        const marker = await (await caches.match('/marker')).text();
        const found = await navigator.serviceWorker.getRegistration();
        const all = await navigator.serviceWorker.getRegistrations();
        return [states.join(' '), reg.scope, w.scriptURL, ready === reg, reg.active === w,
                reg.installing, found === reg, all.length, marker].join();
        "#,
    );
    assert_eq!(
        got,
        "installing installed activating activated,https://a.test/,https://a.test/sw.js,true,true,,true,1,installing installing true true"
    );
}

#[test]
fn registration_refuses_scripts_and_scopes_the_checks_reject() {
    let sw = registry(&[
        ("https://a.test/plain.js", "text/plain", "1"),
        (
            "https://a.test/throws.js",
            "text/javascript",
            "throw new Error('no')",
        ),
        ("https://a.test/sub/w.js", "text/javascript", ""),
    ]);
    let mut page = page_at(&sw, "https://a.test/");
    let got = run_async(
        &mut page,
        r#"
        const names = [];
        for (const [url, opts] of [['/plain.js', {}], ['/throws.js', {}], ['/sub/w.js', {scope: '/'}],
                                   ['https://b.test/w.js', {}], ['/missing.js', {}], ['/sub/w.js', {scope: '/a%2fb/'}]]) {
            try { await navigator.serviceWorker.register(url, opts); names.push('ok'); }
            catch (e) { names.push(e.name); }
        }
        return names.join(' ') + ' ' + (await navigator.serviceWorker.getRegistrations()).length;
        "#,
    );
    assert_eq!(
        got,
        "SecurityError TypeError SecurityError SecurityError TypeError TypeError 0"
    );
}

#[test]
fn unregister_makes_the_worker_redundant_and_forgets_the_registration() {
    let sw = registry(&[("https://a.test/w.js", "text/javascript", "")]);
    let mut page = page_at(&sw, "https://a.test/");
    let got = run_async(
        &mut page,
        r#"
        const reg = await navigator.serviceWorker.register('w.js');
        await navigator.serviceWorker.ready;
        const w = reg.active;
        const redundant = new Promise(ok => w.onstatechange = () => w.state === 'redundant' && ok());
        const first = await reg.unregister();
        await redundant;
        const second = await reg.unregister();
        return [first, second, w.state, reg.active,
                (await navigator.serviceWorker.getRegistrations()).length,
                await navigator.serviceWorker.getRegistration() === undefined].join();
        "#,
    );
    assert_eq!(got, "true,false,redundant,,0,true");
}

#[test]
fn claim_makes_the_worker_the_pages_controller_until_it_goes() {
    let sw = registry(&[(
        "https://a.test/sw/w.js",
        "text/javascript",
        "self.onactivate = e => e.waitUntil(clients.claim());",
    )]);
    let mut page = page_at(&sw, "https://a.test/sw/page.html");
    let got = run_async(
        &mut page,
        r#"
        const before = navigator.serviceWorker.controller;
        const changed = new Promise(ok => navigator.serviceWorker.oncontrollerchange = ok);
        const reg = await navigator.serviceWorker.register('w.js');
        const event = await changed;
        const c = navigator.serviceWorker.controller;
        const facts = [before, event.type, c === reg.active, c.scriptURL];
        // (unregistered while it controls the page: the worker stays until the page goes)
        facts.push(await reg.unregister(), await navigator.serviceWorker.getRegistration(),
                   navigator.serviceWorker.controller === c, c.state);
        return facts.join();
        "#,
    );
    assert_eq!(
        got,
        ",controllerchange,true,https://a.test/sw/w.js,true,,true,activated"
    );
}

#[test]
fn an_insecure_page_has_no_service_worker_container() {
    let sw = registry(&[]);
    let mut page = page_at(&sw, "http://a.test/");
    let got = run_async(&mut page, "return 'serviceWorker' in navigator;");
    assert_eq!(got, "false");
}

/// A tiny HTTP/1.1 server answering `/<name>` from `files` (path, body; all JavaScript), and how
/// many times each path was asked for (and, as `<path> sec-ch-ua`, with a `Sec-CH-UA` header).
fn serve(files: Vec<(&'static str, &'static str)>) -> (String, Arc<Mutex<HashMap<String, usize>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(Mutex::new(HashMap::new()));
    let counted = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            let path = first.split(' ').nth(1).unwrap_or("/").to_owned();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
                if h.to_ascii_lowercase().starts_with("sec-ch-ua:") {
                    *counted
                        .lock()
                        .unwrap()
                        .entry(format!("{path} sec-ch-ua"))
                        .or_insert(0) += 1;
                }
            }
            *counted.lock().unwrap().entry(path.clone()).or_insert(0) += 1;
            let response = match files.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .into(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}"), hits)
}

/// A page at `origin` (a loopback origin is potentially trustworthy) over the network, with a
/// registry of its own that fetches through it.
fn served_page(origin: &str) -> (Page, snowlark_net::NetThread) {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let client = net.client();
    let sw = ServiceWorkerBackend::new(Arc::new(snowlark_sw::LocalServiceWorkers::new(
        Some(client.backend().clone()),
        None,
    )));
    let mut page = Page::new(PageOptions {
        net: Some(client),
        service_workers: Some(sw),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html>",
        &url::Url::parse(&format!("{origin}/index.html")).unwrap(),
    );
    page.run_until_settled(Duration::from_secs(5));
    (page, net)
}

/// Registers `script` and waits for it to activate; returns what the worker put in the cache
/// `t` under `/out`.
const REGISTER_AND_READ: &str = r#"
    const reg = await navigator.serviceWorker.register(SCRIPT, OPTIONS);
    const w = reg.installing;
    await new Promise(ok => {
        if (w.state === 'activated') ok();
        w.onstatechange = () => { if (w.state === 'activated') ok(); };
    });
    const r = await caches.match('/out', { cacheName: 't' });
    return r ? await r.text() : 'no result';
"#;

fn register_and_read(page: &mut Page, script: &str, options: &str) -> String {
    run_async(
        page,
        &REGISTER_AND_READ
            .replace("SCRIPT", &format!("'{script}'"))
            .replace("OPTIONS", options),
    )
}

/// W3C Service Workers §6.3.2: while the worker is parsed or installing, `importScripts()`
/// fetches into its script resource map; afterwards it runs only what the map has (a script
/// imported before is not fetched again), and anything else is a network error.
#[test]
fn import_scripts_fills_the_workers_map_then_serves_only_from_it() {
    let (origin, hits) = serve(vec![
        (
            "/sw.js",
            r#"
            importScripts('a.js');
            const seen = [];
            self.addEventListener('install', () => { importScripts('b.js'); seen.push(self.A, self.B); });
            self.addEventListener('activate', e => {
                try { importScripts('c.js'); seen.push('ran'); } catch (x) { seen.push(x.name); }
                try { importScripts('a.js'); seen.push(self.A); } catch (x) { seen.push(x.name); }
                e.waitUntil(caches.open('t').then(c => c.put('/out', new Response(seen.join()))));
            });
            "#,
        ),
        ("/a.js", "self.A = (self.A || 0) + 1;"),
        ("/b.js", "self.B = 'b';"),
        ("/c.js", "self.C = 'c';"),
    ]);
    let (mut page, _net) = served_page(&origin);
    let got = register_and_read(&mut page, "/sw.js", "{}");
    assert_eq!(got, "1,b,NetworkError,2");
    let hits = hits.lock().unwrap();
    assert_eq!(hits.get("/a.js"), Some(&1), "{hits:?}");
    assert_eq!(hits.get("/c.js"), None, "{hits:?}");
    // (a service worker's script fetch carries no client hints, as the engine family's does not;
    // the scripts it imports do)
    assert_eq!(hits.get("/sw.js sec-ch-ua"), None, "{hits:?}");
    assert_eq!(hits.get("/a.js sec-ch-ua"), Some(&1), "{hits:?}");
}

/// A module service worker (`type: "module"`): its static imports load, `importScripts()` and
/// `import()` are TypeErrors in it (HTML), and a graph with top-level await is refused (§8 Update
/// step 8, "Is Async Module").
#[test]
fn a_module_worker_runs_its_graph_without_import_scripts_or_dynamic_import() {
    let (origin, _) = serve(vec![
        (
            "/m.js",
            r#"
            import { v } from './dep.js';
            const seen = [v];
            try { importScripts('dep.js'); seen.push('ran'); } catch (x) { seen.push(x.name); }
            self.addEventListener('install', e => e.waitUntil(
                import('./dep.js').then(() => 'ran', x => x.name)
                    .then(d => caches.open('t').then(c => c.put('/out', new Response(seen.concat(d).join()))))));
            "#,
        ),
        ("/dep.js", "export const v = 'dep';"),
        ("/tla.js", "await 0;"),
    ]);
    let (mut page, _net) = served_page(&origin);
    let got = register_and_read(&mut page, "/m.js", "{ type: 'module' }");
    assert_eq!(got, "dep,TypeError,TypeError");
    let got = run_async(
        &mut page,
        "return navigator.serviceWorker.register('/tla.js', { type: 'module', scope: '/tla/' }).then(() => 'registered', e => e.name);",
    );
    assert_eq!(got, "TypeError");
}
