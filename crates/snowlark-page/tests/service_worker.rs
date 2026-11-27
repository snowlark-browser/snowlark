// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Service worker registration and its lifecycle through the page (W3C Service Workers §3.4
//! `register()`, §8 Register, Update, Install, Activate, Unregister): the worker runs in a
//! `ServiceWorkerGlobalScope` on a thread of its own, its states reach the page in order, and the
//! registration's security checks reject what they must.

use std::sync::Arc;
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
