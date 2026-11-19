// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Loading: script streaming, the code cache, the preload scanner and preloads, against an
//! in-process HTTP server that counts requests.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions};

/// One resource the server answers with.
#[derive(Clone)]
struct Resource {
    mime: String,
    body: Vec<u8>,
    /// Sent before the body (after the head).
    delay: Duration,
    headers: Vec<(String, String)>,
}

fn resource(mime: &str, body: impl Into<Vec<u8>>) -> Resource {
    Resource {
        mime: mime.into(),
        body: body.into(),
        delay: Duration::ZERO,
        headers: Vec::new(),
    }
}

/// An HTTP/1.1 server over a path → resource map, counting the requests per path.
#[allow(dead_code)]
struct Server {
    addr: std::net::SocketAddr,
    resources: Arc<Mutex<HashMap<String, Resource>>>,
    hits: Arc<Mutex<HashMap<String, usize>>>,
    /// The `Referer` of the last request for each path ("" without one).
    referers: Arc<Mutex<HashMap<String, String>>>,
}

#[allow(dead_code)]
impl Server {
    fn start(resources: Vec<(&str, Resource)>) -> Server {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let resources: Arc<Mutex<HashMap<String, Resource>>> = Arc::new(Mutex::new(
            resources
                .into_iter()
                .map(|(p, r)| (p.to_owned(), r))
                .collect(),
        ));
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let referers: Arc<Mutex<HashMap<String, String>>> = Arc::default();
        let (res, h, rf) = (resources.clone(), hits.clone(), referers.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (res, h, rf) = (res.clone(), h.clone(), rf.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut first = String::new();
                        if reader.read_line(&mut first).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut referer = String::new();
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                                break;
                            }
                            if let Some((n, v)) = line.split_once(':') {
                                if n.eq_ignore_ascii_case("referer") {
                                    referer = v.trim().to_owned();
                                }
                            }
                        }
                        let path = first.split_whitespace().nth(1).unwrap_or("/").to_owned();
                        *h.lock().unwrap().entry(path.clone()).or_default() += 1;
                        rf.lock().unwrap().insert(path.clone(), referer);
                        let r = res.lock().unwrap().get(&path).cloned();
                        let Some(r) = r else {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
                            );
                            continue;
                        };
                        let mut head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
                            r.mime,
                            r.body.len()
                        );
                        for (n, v) in &r.headers {
                            head.push_str(&format!("{n}: {v}\r\n"));
                        }
                        head.push_str("\r\n");
                        if stream.write_all(head.as_bytes()).is_err() {
                            return;
                        }
                        let _ = stream.flush();
                        if !r.delay.is_zero() {
                            std::thread::sleep(r.delay);
                        }
                        if stream.write_all(&r.body).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Server {
            addr,
            resources,
            hits,
            referers,
        }
    }

    fn referer(&self, path: &str) -> Option<String> {
        self.referers.lock().unwrap().get(path).cloned()
    }

    fn url(&self, path: &str) -> url::Url {
        url::Url::parse(&format!("http://{}{path}", self.addr)).unwrap()
    }

    fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    fn set(&self, path: &str, r: Resource) {
        self.resources.lock().unwrap().insert(path.to_owned(), r);
    }
}

fn page(net: &NetThread) -> Page {
    Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        ..Default::default()
    })
}

fn load(page: &mut Page, url: url::Url) {
    page.navigate(url);
    let start = std::time::Instant::now();
    while !page.load_complete() {
        assert!(start.elapsed() < Duration::from_secs(10), "the page loads");
        page.run_for(Duration::from_millis(10));
    }
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// A classic script of about `kib` KiB that defines `name` and records that it ran.
fn big_script(name: &str, kib: usize) -> String {
    let mut s = format!("window.ran = (window.ran || []).concat('{name}');\nfunction {name}(x) {{ return 'é' + x; }}\n");
    let mut i = 0;
    while s.len() < kib * 1024 {
        s.push_str(&format!(
            "function filler{i}(a, b) {{ return a * {i} + b; }}\n"
        ));
        i += 1;
    }
    s
}

#[test]
fn external_scripts_compile_while_they_download() {
    let delayed = Resource {
        delay: Duration::from_millis(200),
        ..resource("text/javascript", big_script("late", 64))
    };
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><meta charset=utf-8><script src=/a.js></script><script src=/late.js></script><script src=/small.js></script><script type=module src=/m.js></script>",
            ),
        ),
        ("/a.js", resource("text/javascript", big_script("a", 40))),
        ("/late.js", delayed),
        ("/small.js", resource("text/javascript", "window.ran.push('small');")),
        (
            "/m.js",
            resource(
                "text/javascript",
                format!("{}\nwindow.ran.push('m');", big_script("m", 24).replace("window.ran = (window.ran || []).concat('m');", "")),
            ),
        ),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    assert_eq!(eval(&mut page, "ran.join()"), "a,late,small,m");
    // The streamed scripts' lazily compiled functions read their source text back right.
    assert_eq!(
        eval(&mut page, "a.toString() + late('!')"),
        "function a(x) { return 'é' + x; }é!"
    );
    let stats = page.engine().load_stats();
    assert_eq!(stats.streamed, 3, "{stats:?}");
    assert_eq!(stats.stream_fallbacks, 0);
}

#[test]
fn a_stream_that_is_not_the_script_falls_back() {
    // Declared windows-1252, but the bytes start with a UTF-8 byte order mark, which wins (HTML
    // "decode"): V8's stream saw the wrong encoding, so the page compiles the decoded text.
    let mut body = vec![0xEF, 0xBB, 0xBF];
    body.extend_from_slice(big_script("bom", 32).as_bytes());
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><script src=/bom.js charset=windows-1252></script>",
            ),
        ),
        ("/bom.js", resource("text/javascript", body)),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    assert_eq!(eval(&mut page, "bom('')"), "é");
    let stats = page.engine().load_stats();
    assert_eq!(
        (stats.streamed, stats.stream_fallbacks),
        (0, 1),
        "{stats:?}"
    );
}

/// The code cache entry files in `dir`.
fn cache_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "v8c"))
                .collect()
        })
        .unwrap_or_default()
}

/// Loads `url` in a fresh page, lets the code cache task run, and returns the page's statistics
/// once the cache folder has `files` entries.
fn visit(net: &NetThread, url: url::Url, dir: &std::path::Path) -> snowlark_page::LoadStats {
    let mut page = page(net);
    load(&mut page, url);
    let start = std::time::Instant::now();
    let settled = |s: &snowlark_page::LoadStats| {
        s.cache_produced + s.cache_hits + s.cache_timestamps + s.cache_rejected > 0
    };
    while !settled(&page.engine().load_stats()) && start.elapsed() < Duration::from_secs(5) {
        page.run_for(Duration::from_millis(10));
    }
    assert_eq!(eval(&mut page, "typeof cached"), "function");
    let stats = page.engine().load_stats();
    drop(page);
    // The entry is written by the cache's own thread.
    let start = std::time::Instant::now();
    while cache_files(dir).is_empty() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    stats
}

#[test]
fn a_scripts_code_cache_is_made_used_and_rejected() {
    let profile = std::env::temp_dir().join(format!("snowlark-loading-cc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    let dir = profile.join("code-cache");
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><meta charset=utf-8><script src=/c.js></script>",
            ),
        ),
        (
            "/c.js",
            resource("text/javascript", big_script("cached", 20)),
        ),
    ]);
    let net = NetThread::start(NetConfig {
        profile_dir: Some(profile.clone()),
        ..NetConfig::default()
    })
    .unwrap();

    // (hits, timestamps, produced, streamed) of a visit
    let counts = |s: &snowlark_page::LoadStats| {
        (
            s.cache_hits,
            s.cache_timestamps,
            s.cache_produced,
            s.streamed,
        )
    };
    // The entry's data: after the magic, version tag, kind, hash, URL and data length.
    let entry_data = |file: &std::path::Path| -> (Vec<u8>, usize) {
        let bytes = std::fs::read(file).unwrap();
        let url_len = u32::from_le_bytes(bytes[48..52].try_into().unwrap()) as usize;
        (bytes, 52 + url_len + 4)
    };

    // First visit: streamed and run; it leaves a timestamp, not code (a script loaded once is
    // not worth its code cache).
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!(counts(&s), (0, 1, 0, 1), "{s:?}");
    assert_eq!(cache_files(&dir).len(), 1);

    // Second visit, within three days: the script is hot. It still streams, and its code cache is
    // made once it has run.
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!(counts(&s), (0, 0, 1, 1), "{s:?}");

    // Third visit: compiled from the cache.
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!(counts(&s), (1, 0, 0, 0), "{s:?}");
    assert_eq!(s.cache_rejected, 0, "{s:?}");

    // The script changed: the code is for another response, so it is not used, and this load
    // starts over with a timestamp; the next one makes its code, the one after uses it.
    server.set(
        "/c.js",
        resource("text/javascript", big_script("cached", 21)),
    );
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!((counts(&s), s.cache_stale), ((0, 1, 0, 0), 1), "{s:?}");
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!(counts(&s), (0, 0, 1, 1), "{s:?}");
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!(s.cache_hits, 1, "{s:?}");

    // V8 rejects an entry whose data is not its own (corrupted here): it is removed, and the
    // loads after it start over.
    let file = cache_files(&dir).pop().unwrap();
    let (mut bytes, data) = entry_data(&file);
    for b in &mut bytes[data..data + 16] {
        *b ^= 0xAA;
    }
    std::fs::write(&file, &bytes).unwrap();
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!(
        (s.cache_hits, s.cache_rejected, s.cache_produced),
        (0, 1, 0),
        "{s:?}"
    );
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!(counts(&s), (0, 1, 0, 1), "{s:?}");

    // A timestamp three days old or more is not hot: the load leaves a new one.
    let file = cache_files(&dir).pop().unwrap();
    let (mut bytes, data) = entry_data(&file);
    let then =
        std::time::SystemTime::now() - snowlark_js::code_cache::HOT_FOR - Duration::from_secs(60);
    bytes[data..data + 8].copy_from_slice(&snowlark_js::code_cache::timestamp(then));
    std::fs::write(&file, &bytes).unwrap();
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!(counts(&s), (0, 1, 0, 1), "{s:?}");
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!(counts(&s), (0, 0, 1, 1), "{s:?}");
    let s = visit(&net, server.url("/"), &dir);
    assert_eq!(s.cache_hits, 1, "{s:?}");
    let _ = std::fs::remove_dir_all(&profile);
}

/// A 1×1 PNG.
pub(crate) const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn rendered_page(net: &NetThread) -> Page {
    Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(snowlark_page::RenderSetup::new().unwrap()),
        ..Default::default()
    })
}

/// Runs the page until it has loaded and nothing is left to do (images and all).
fn settle(page: &mut Page) {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(1500) {
        page.run_for(Duration::from_millis(10));
    }
}

#[test]
fn the_speculative_parser_fetches_ahead_of_a_blocking_script_and_elements_reuse_it() {
    let slow = Resource {
        delay: Duration::from_millis(400),
        ..resource("text/javascript", "window.order = ['slow'];")
    };
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><meta charset=utf-8><script src=/slow.js></script>\
                 <link rel=stylesheet href=/s.css><script src=/next.js></script>\
                 <img src=/i.png><template><img src=/inert.png></template>\
                 <noscript><img src=/noscript.png></noscript>\
                 <picture><source media='(orientation: portrait)' srcset=/p.png><img src=/q.png></picture>                 <img loading=lazy src=/lazy.png><link rel=preload as=image type=not-a-mime href=/bad.png>\
                 <meta http-equiv=content-security-policy content=\"img-src 'none'\"><img src=/after-csp.png>",
            ),
        ),
        ("/slow.js", slow),
        ("/s.css", resource("text/css", "p { color: red }")),
        ("/next.js", resource("text/javascript", "order.push('next');")),
        ("/i.png", resource("image/png", vec![0u8; 64])),
        ("/inert.png", resource("image/png", vec![0u8; 64])),
        ("/noscript.png", resource("image/png", vec![0u8; 64])),
        ("/p.png", resource("image/png", vec![0u8; 64])),
        ("/q.png", resource("image/png", vec![0u8; 64])),
        ("/after-csp.png", resource("image/png", vec![0u8; 64])),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    settle(&mut page);
    assert_eq!(eval(&mut page, "order.join()"), "slow,next");
    // Fetched once each: the speculative fetch, taken over by the element.
    for path in ["/s.css", "/next.js", "/i.png"] {
        assert_eq!(server.hits(path), 1, "{path}");
    }
    // Nothing the parser would not fetch: template contents, noscript contents (scripting is on),
    // a picture whose source cannot be decided early; nothing past a CSP meta.
    assert_eq!(server.hits("/inert.png"), 0);
    assert_eq!(server.hits("/noscript.png"), 0);
    assert_eq!(server.hits("/p.png"), 0);
    // Nor a lazy image (the renderer fetches it, once), nor a preload of an unsupported type.
    assert!(server.hits("/lazy.png") <= 1);
    assert_eq!(server.hits("/bad.png"), 0);
    let early = page.engine().early_stats();
    assert_eq!(early.speculative, 3, "{early:?}");
    let (started, consumed) = net.client().preload_counts();
    assert_eq!((started, consumed), (3, 3));
}

/// HTML §4.12.1: a script's force async is initially true; the HTML parser sets it to false on the
/// scripts it inserts (not the fragment parser), and so does adding an `async` attribute.
#[test]
fn scripts_not_inserted_by_the_parser_are_forced_async() {
    let server = Server::start(vec![(
        "/",
        resource(
            "text/html",
            "<!doctype html><script id=p>window.x = 1</script>",
        ),
    )]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    let got = eval(
        &mut page,
        "const made = document.createElement('script');\
         const d = document.createElement('div');\
         d.innerHTML = '<script src=a.js></scr' + 'ipt>';\
         const f = document.createRange().createContextualFragment('<script src=a.js></scr' + 'ipt>');\
         const added = document.createElement('script');\
         added.setAttribute('async', ''); added.removeAttribute('async');\
         [p.async, made.async, d.firstChild.async, f.firstChild.async, added.async].join()",
    );
    assert_eq!(got, "false,true,true,true,false");
}

#[test]
fn link_preloads_fire_events_and_are_consumed_once() {
    let mut doc_head = resource(
        "text/html",
        "<!doctype html><meta charset=utf-8>\
         <link rel=preload as=script href=/p.js onload=\"window.ev = (window.ev || '') + 'load '\">\
         <link rel=preload as=style href=/missing.css onerror=\"window.ev = (window.ev || '') + 'error '\">\
         <link rel=preload as=bogus href=/bogus.js onerror=\"window.ev += 'bogus '\">\
         <link rel=modulepreload href=/m.js onload=\"window.ev += 'mload '\">\
         <link rel=preconnect href=/>\
         <script>setTimeout(() => { const s = document.createElement('script'); s.src = '/p.js'; document.head.append(s); }, 300)</script>\
         <script type=module>import {m} from '/m.js'; window.m = m;</script>",
    );
    doc_head
        .headers
        .push(("Link".into(), "</h.js>; rel=preload; as=script".into()));
    let server = Server::start(vec![
        ("/", doc_head),
        ("/p.js", resource("text/javascript", "window.p = 1;")),
        ("/h.js", resource("text/javascript", "window.h = 1;")),
        (
            "/m.js",
            resource("text/javascript", "export const m = 'mod';"),
        ),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(800) {
        page.run_for(Duration::from_millis(10));
    }
    let ev = eval(&mut page, "ev");
    assert!(
        ev.contains("load ") && ev.contains("error ") && ev.contains("mload "),
        "{ev}"
    );
    assert!(!ev.contains("bogus"), "{ev}");
    assert_eq!(eval(&mut page, "p + ',' + m"), "1,mod");
    assert_eq!(server.hits("/p.js"), 1);
    assert_eq!(server.hits("/m.js"), 1);
    assert_eq!(server.hits("/h.js"), 1, "the Link header's preload");
    assert_eq!(server.hits("/bogus.js"), 0);
}

#[test]
fn image_and_preload_fetches_report_resource_timing() {
    // Resource Timing: an element's fetch is reported with its initiator type (`img`, `css` for a
    // style image); a `link` preload reports its own fetch, and the image that takes the preloaded
    // response reports none.
    let png = || resource("image/png", vec![0u8; 64]);
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><meta charset=utf-8>                 <style>body { background-image: url(/bg.png) }</style>                 <body><img src=/i.png>                 <script>                 const l = document.createElement('link');                 l.rel = 'preload'; l.as = 'image'; l.href = '/pre.png';                 document.head.append(l);                 new Image().src = '/pre.png';                 new Image().src = '/detached.png';                 </script>",
            ),
        ),
        ("/i.png", png()),
        ("/bg.png", png()),
        ("/pre.png", png()),
        ("/detached.png", png()),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    settle(&mut page);
    let r = eval(
        &mut page,
        "['i', 'bg', 'pre', 'detached'].map(n => n + ':' + performance.getEntriesByType('resource')           .filter(e => e.name.endsWith('/' + n + '.png')).map(e => e.initiatorType).join('/')).join(' ')",
    );
    assert_eq!(r, "i:img bg:css pre:link detached:img");
    assert_eq!(server.hits("/pre.png"), 1);
}

#[test]
fn an_image_preload_takes_the_image_its_document_has() {
    // HTML §4.8.4.3.2, the list of available images: a no-CORS image preload of a URL its document
    // already loaded, or is loading, for an element is that image; the response (which no cache
    // may keep) is requested once, and the `link` fires `load`.
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><meta charset=utf-8><body><script>                 window.events = [];                 const link = () => {                   const l = document.createElement('link');                   l.rel = 'preload'; l.as = 'image'; l.href = '/a.png';                   l.onload = () => events.push('load'); l.onerror = () => events.push('error');                   return l;                 };                 const first = link();                 document.head.append(first);                 const img = new Image();                 img.onload = () => {                   document.head.replaceChild(link(), first);                   const late = new Image(); late.src = '/b.png';                   const l = link(); l.href = '/b.png'; document.head.append(l);                 };                 img.src = '/a.png';                 </script>",
            ),
        ),
        ("/a.png", resource("image/png", PNG_1X1.to_vec())),
        ("/b.png", Resource {
            delay: Duration::from_millis(300),
            ..resource("image/png", PNG_1X1.to_vec())
        }),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    settle(&mut page);
    assert_eq!(eval(&mut page, "events.join()"), "load,load,load");
    assert_eq!(server.hits("/a.png"), 1);
    assert_eq!(server.hits("/b.png"), 1);
}

#[test]
fn a_fetch_in_flight_when_the_policy_changes_is_still_reported() {
    // Resource Timing §4.6: a fetch is reported to its request client's global. A document whose
    // policy changes (a `meta` CSP) while its image is on the network keeps that fetch's entry.
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><meta charset=utf-8><body><img src=/slow.png><script>                 const m = document.createElement('meta');                 m.httpEquiv = 'Content-Security-Policy'; m.content = 'img-src *';                 setTimeout(() => document.head.append(m), 50);                 </script>",
            ),
        ),
        (
            "/slow.png",
            Resource {
                delay: Duration::from_millis(400),
                ..resource("image/png", PNG_1X1.to_vec())
            },
        ),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    settle(&mut page);
    let r = eval(
        &mut page,
        "performance.getEntriesByType('resource').filter(e => e.name.endsWith('/slow.png'))           .map(e => e.initiatorType).join()",
    );
    assert_eq!(r, "img");
}

#[test]
fn an_evaluated_module_imported_again_settles_every_time() {
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><script>window.r = []; const u = '/m.js';                 import(u).then(() => { r.push('a'); return import(u); })                 .then(() => { r.push('b'); return import(u); })                 .then(() => { r.push('c'); return import(u); })                 .then(() => r.push('d'));</script>",
            ),
        ),
        ("/m.js", resource("text/javascript", "export const m = 1;")),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        page.run_for(Duration::from_millis(10));
    }
    assert_eq!(eval(&mut page, "r.join()"), "a,b,c,d");
}

/// HTML §4.2.5.3 the refresh pragma and the `Refresh` header (the shared declarative refresh
/// steps): a frame's document navigates, replacing its entry, to the URL they name once it has
/// loaded, with itself as the referrer; not from a `meta` in a shadow tree, nor from one in a
/// document sandboxed without `allow-scripts`; a second refresh is ignored.
#[test]
fn a_declarative_refresh_navigates_the_document_once_it_has_loaded() {
    let mut header = resource("text/html", "<!doctype html>header");
    header
        .headers
        .push(("Refresh".into(), "0;url=/to-h".into()));
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><iframe id=m src=/m></iframe><iframe id=h src=/h></iframe>\
                 <iframe id=s src=/m sandbox=allow-same-origin></iframe>\
                 <iframe id=d src=/d></iframe>",
            ),
        ),
        (
            "/m",
            resource(
                "text/html",
                "<!doctype html><meta http-equiv=Refresh content=\" 0 ; URL = '/to-m'x\">\
                 <meta http-equiv=refresh content=\"0;url=/second\">meta",
            ),
        ),
        ("/h", header),
        (
            "/d",
            resource(
                "text/html",
                "<!doctype html><div id=host></div><script>\
                 host.attachShadow({mode: 'open'}).innerHTML = \
                 '<meta http-equiv=refresh content=\"0;url=/to-shadow\">';</script>dynamic",
            ),
        ),
        ("/to-m", resource("text/html", "to-m")),
        ("/to-h", resource("text/html", "to-h")),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    run_ms(&mut page, 600);
    let text = |page: &mut Page, id: &str| {
        eval(
            page,
            &format!("document.getElementById('{id}').contentDocument.body.textContent"),
        )
    };
    assert_eq!(text(&mut page, "m"), "to-m");
    assert_eq!(text(&mut page, "h"), "to-h");
    assert_eq!(
        text(&mut page, "s"),
        "meta",
        "sandboxed without allow-scripts"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('d').contentDocument.location.pathname"
        ),
        "/d",
        "a meta in a shadow tree"
    );
    assert_eq!(server.hits("/second"), 0);
    assert_eq!(server.hits("/to-shadow"), 0);
    assert_eq!(
        server.referer("/to-m").as_deref(),
        Some(server.url("/m").as_str())
    );
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('m').contentWindow.history.length"
        ),
        "1",
        "the refresh replaced the entry"
    );
}

/// HTML "update a `style` block" step 9 and the `link` element's "process the linked resource":
/// a style sheet's `load` or `error` waits for its critical subresources, the `@import`s at any
/// depth, and is `error` when one of them failed (a network error, an error status, or a
/// Content-Type other than text/css); until then the document's load event waits. A `style`
/// element fires them too, again when its text changes.
#[test]
fn a_style_sheets_load_event_waits_for_its_imports() {
    let mut slow = resource("text/css", "@import url(/nested.css);");
    slow.delay = Duration::from_millis(300);
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><script>window.ev = []; \
                 function on(e) { ev.push(e.target.id + ':' + e.type + (e.target.id == 'slow' \
                 ? ':' + getComputedStyle(document.body).color : '')); } \
                 addEventListener('load', () => window.slowFirst = ev.some(e => e.startsWith('slow')));\
                 </script>\
                 <link id=slow rel=stylesheet href=/slow-import.css onload=on(event) onerror=on(event)>\
                 <link id=missing rel=stylesheet href=/missing-import.css onload=on(event) onerror=on(event)>\
                 <style id=plain onload=on(event) onerror=on(event)>p { color: blue }</style>\
                 <style id=text onload=on(event) onerror=on(event)>@import url(/text.css);</style>\
                 <p>x",
            ),
        ),
        (
            "/slow-import.css",
            resource("text/css", "@import url(/slow.css);"),
        ),
        ("/slow.css", slow),
        ("/nested.css", resource("text/css", "body { color: rgb(0, 128, 0) }")),
        (
            "/missing-import.css",
            resource("text/css", "@import url(/ok.css); @import url(/none.css);"),
        ),
        ("/ok.css", resource("text/css", "")),
        ("/text.css", resource("text/plain", "")),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    settle(&mut page);
    let ev = eval(&mut page, "ev.sort().join(' ')");
    assert_eq!(
        ev,
        "missing:error plain:load slow:load:rgb(0, 128, 0) text:error",
    );
    // (a sheet whose imports are still coming delays the document's load event)
    assert_eq!(eval(&mut page, "window.slowFirst"), "true");
    eval(
        &mut page,
        "ev = []; document.getElementById('plain').textContent = 'q {}'",
    );
    run_ms(&mut page, 100);
    assert_eq!(eval(&mut page, "ev.join(' ')"), "plain:load");
    // (`href` or `rel` set to the value it has is no change: nothing is fetched again)
    eval(
        &mut page,
        "ev = []; const l = document.getElementById('slow'); \
         l.setAttribute('href', l.getAttribute('href')); l.rel = l.rel;",
    );
    run_ms(&mut page, 500);
    assert_eq!(eval(&mut page, "ev.join(' ')"), "");
    assert_eq!(server.hits("/slow-import.css"), 1);
    // (`HTMLStyleElement.disabled` is its sheet's disabled flag: the sheet stops applying)
    assert_eq!(
        eval(
            &mut page,
            "const s = document.getElementById('plain'); s.textContent = 'p { color: blue }'; \
             const p = document.querySelector('p'); const before = getComputedStyle(p).color; \
             s.disabled = true; \
             [before, s.disabled, s.sheet.disabled, getComputedStyle(p).color].join()"
        ),
        "rgb(0, 0, 255),true,true,rgb(0, 128, 0)"
    );
}

/// Runs the page for `ms` milliseconds.
fn run_ms(page: &mut Page, ms: u64) {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(ms) {
        page.run_for(Duration::from_millis(10));
    }
}

/// HTML HostLoadImportedModule: `import()` in code a script compiles from a string (a string
/// timer, `eval`, `Function`) resolves against that script's base URL (an inline script's is the
/// document base URL when it was prepared); in an event handler, which has no script, against
/// the document's base URL as it is now.
#[test]
fn import_from_string_code_uses_the_referencing_scripts_base_url() {
    let page_html = "<!doctype html><base href='/a/b/'><script>window.r = [];
        document.querySelector('base').href = '/c/';
        const got = (k) => (m) => r.push(k + m.from), failed = (k) => (e) => r.push(k + e.name);
        setTimeout(\"import('./m.js').then(got('timer:'), failed('timer:'))\");
        eval(\"import('./m.js').then(got('eval:'), failed('eval:'))\");
        Function(\"import('./m.js').then(got('fn:'), failed('fn:'))\")();
        const d = document.createElement('div');
        d.setAttribute('onclick', \"import('./m.js').then(got('handler:'), failed('handler:'))\");
        d.click();</script>";
    let server = Server::start(vec![
        ("/", resource("text/html", page_html)),
        (
            "/a/b/m.js",
            resource("text/javascript", "export const from = 'script';"),
        ),
        (
            "/c/m.js",
            resource("text/javascript", "export const from = 'document';"),
        ),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    run_ms(&mut page, 300);
    assert_eq!(
        eval(&mut page, "r.sort().join()"),
        "eval:script,fn:script,handler:document,timer:script"
    );
}

/// HTML "create a JavaScript module script": a parse error is the module script's error to
/// rethrow. The script element runs (`load`, the SyntaxError reported to the window), and every
/// `import()` of it rejects with that same SyntaxError.
#[test]
fn a_module_parse_error_is_rethrown_not_a_fetch_failure() {
    let page_html = "<!doctype html><script>window.r = [];
        addEventListener('error', e => r.push('reported:' + e.error.name));</script>
        <script type=module src='/bad.js' onload=\"r.push('load')\" onerror=\"r.push('error')\"></script>
        <script>Promise.all([import('/bad.js').catch(e => e), import('/bad.js').catch(e => e)])
            .then(([a, b]) => r.push('import:' + a.name + ':' + (a === b)));</script>";
    let server = Server::start(vec![
        ("/", resource("text/html", page_html)),
        ("/bad.js", resource("text/javascript", "export let = ;")),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    run_ms(&mut page, 300);
    assert_eq!(
        eval(&mut page, "r.sort().join()"),
        "import:SyntaxError:true,load,reported:SyntaxError"
    );
}

/// HTML "fetch a single imported module script": a module's imports are fetched with its URL as
/// the referrer, and with its script element's `referrerpolicy`.
#[test]
fn module_imports_send_their_importer_as_referrer_under_its_policy() {
    let page_html = "<!doctype html><script type=module src='/d/a.js'></script>
        <script type=module src='/e/a.js' referrerpolicy=no-referrer></script>";
    let server = Server::start(vec![
        ("/p.html", resource("text/html", page_html)),
        ("/d/a.js", resource("text/javascript", "import './b.js';")),
        ("/d/b.js", resource("text/javascript", "window.d = 1;")),
        ("/e/a.js", resource("text/javascript", "import './b.js';")),
        ("/e/b.js", resource("text/javascript", "window.e = 1;")),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/p.html"));
    run_ms(&mut page, 300);
    assert_eq!(eval(&mut page, "window.d + window.e"), "2");
    let page_url = server.url("/p.html").to_string();
    assert_eq!(
        server.referer("/d/a.js").as_deref(),
        Some(page_url.as_str())
    );
    let a = server.url("/d/a.js").to_string();
    assert_eq!(server.referer("/d/b.js").as_deref(), Some(a.as_str()));
    assert_eq!(server.referer("/e/a.js").as_deref(), Some(""));
    assert_eq!(server.referer("/e/b.js").as_deref(), Some(""));
}

/// HTML §4.12.1: a connected script element that was not started (no `src`, no text) is
/// prepared when it gets a `src` attribute; one that has run ignores a later `src`.
#[test]
fn a_script_element_given_a_src_later_is_prepared() {
    let page_html = "<!doctype html><body><script>window.r = [];
        const s = document.createElement('script');
        document.body.appendChild(s);
        s.src = '/a.js';
        r.push('set');</script>";
    let server = Server::start(vec![
        ("/", resource("text/html", page_html)),
        (
            "/a.js",
            resource("text/javascript", "r.push('a'); s.src = '/b.js';"),
        ),
        ("/b.js", resource("text/javascript", "r.push('b');")),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    run_ms(&mut page, 200);
    assert_eq!(eval(&mut page, "r.join()"), "set,a");
}

/// HTML "the end" step 3: deferred scripts (module scripts among them) wait until the document
/// has no style sheet that is blocking scripts, so they see its styles.
#[test]
fn deferred_and_module_scripts_wait_for_script_blocking_style_sheets() {
    let page_html = "<!doctype html><link rel=stylesheet href=/slow.css>
        <body><div id=t></div><script defer src=/d.js></script>
        <script type=module>window.m = getComputedStyle(t).position;</script>";
    let sheet = Resource {
        delay: Duration::from_millis(300),
        ..resource("text/css", "#t { position: fixed }")
    };
    let server = Server::start(vec![
        ("/", resource("text/html", page_html)),
        ("/slow.css", sheet),
        (
            "/d.js",
            resource(
                "text/javascript",
                "window.d = getComputedStyle(t).position;",
            ),
        ),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    assert_eq!(eval(&mut page, "window.d + ' ' + window.m"), "fixed fixed");
}

#[test]
fn preloads_follow_the_documents_csp() {
    let mut doc = resource("text/html", "<!doctype html><p>csp");
    doc.headers.push((
        "Content-Security-Policy".into(),
        "script-src 'nonce-abc'".into(),
    ));
    doc.headers.push((
        "Link".into(),
        "</blocked.js>; rel=preload; as=script, </allowed.js>; rel=preload; as=script; nonce=abc"
            .into(),
    ));
    let server = Server::start(vec![
        ("/", doc),
        ("/blocked.js", resource("text/javascript", "")),
        ("/allowed.js", resource("text/javascript", "")),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    let start = std::time::Instant::now();
    while server.hits("/allowed.js") == 0 && start.elapsed() < Duration::from_secs(3) {
        page.run_for(Duration::from_millis(10));
    }
    assert_eq!(server.hits("/allowed.js"), 1);
    assert_eq!(server.hits("/blocked.js"), 0);
}

#[test]
fn navigation_and_resource_timing_come_from_the_load() {
    let mut js = resource("text/javascript", "window.ran = true;");
    js.headers = vec![("Server-Timing".into(), "db;dur=5;desc=\"Database\"".into())];
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><meta charset=utf-8><link rel=stylesheet href=/c.css>\
                 <script src=/s.js></script><body>hi<script async src=/late.js></script>",
            ),
        ),
        ("/c.css", resource("text/css", "body { color: red }")),
        ("/s.js", js),
        ("/late.js", resource("text/javascript", "")),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    page.run_for(Duration::from_millis(50));
    let r = eval(
        &mut page,
        r#"
        const n = performance.getEntriesByType('navigation')[0];
        const t = performance.timing;
        const res = performance.getEntriesByType('resource');
        const s = res.find(e => e.name.endsWith('/s.js'));
        const c = res.find(e => e.name.endsWith('/c.css'));
        const late = res.find(e => e.name.endsWith('/late.js'));
        [
          n instanceof PerformanceNavigationTiming, n.name.endsWith('/'), n.startTime, n.initiatorType,
          n.type, n.nextHopProtocol, n.responseStatus, n.contentType,
          0 <= n.fetchStart && n.fetchStart <= n.requestStart && n.requestStart <= n.responseStart &&
            n.responseStart <= n.responseEnd && n.responseEnd <= n.domInteractive &&
            n.domInteractive <= n.domContentLoadedEventStart && n.domContentLoadedEventEnd <= n.loadEventStart &&
            n.loadEventStart <= n.loadEventEnd,
          n.duration === n.loadEventEnd, n.decodedBodySize > 0, n.transferSize === n.encodedBodySize + 300,
          t.navigationStart === Math.floor(performance.timeOrigin), t.navigationStart <= t.fetchStart,
          t.fetchStart <= t.responseStart, t.responseStart <= t.loadEventEnd, t.unloadEventStart,
          performance.navigation.type, performance.navigation.redirectCount, typeof t.toJSON().domLoading,
          s.initiatorType, c.initiatorType, s.renderBlockingStatus, c.renderBlockingStatus,
          late.renderBlockingStatus, s.responseStatus, s.serverTiming.map(m => m.name + m.duration + m.description).join(),
          s.fetchStart <= s.responseStart && s.responseStart <= s.responseEnd, s.duration === s.responseEnd - s.startTime,
          JSON.stringify(Object.keys(s.toJSON()).slice(0, 6)),
          PerformanceObserver.supportedEntryTypes.join(),
        ].join(' | ')
        "#,
    );
    assert_eq!(
        r,
        "true | true | 0 | navigation | navigate | http/1.1 | 200 | text/html | true | true | true | true | \
         true | true | true | true | 0 | 0 | 0 | number | script | link | blocking | blocking | \
         non-blocking | 200 | db5Database | true | true | \
         [\"id\",\"name\",\"entryType\",\"startTime\",\"duration\",\"navigationId\"] | \
         event,first-input,mark,measure,navigation,paint,resource,visibility-state"
    );
}

#[test]
fn the_time_origin_is_the_navigation_start_for_every_clock() {
    let server = Server::start(vec![(
        "/",
        resource(
            "text/html",
            "<!doctype html><meta charset=utf-8><body>clock\
             <script>requestAnimationFrame(t => { window.rafAt = t; window.nowAtRaf = performance.now(); });</script>",
        ),
    )]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(snowlark_page::RenderSetup::new().unwrap()),
        ..Default::default()
    });
    // (the page lives a while before it navigates: its clock must not count from its creation)
    page.run_for(Duration::from_millis(300));
    std::thread::sleep(Duration::from_millis(300));
    load(&mut page, server.url("/"));
    page.run_until_settled(Duration::from_secs(5));
    let r = eval(
        &mut page,
        r#"
        const now = performance.now();
        const nav = performance.getEntriesByType('navigation')[0];
        [
          nav.startTime === 0 && nav.fetchStart >= 0 && nav.fetchStart < 200,
          Math.abs(performance.timeOrigin + now - Date.now()) < 50,
          Math.abs(document.timeline.currentTime - now) < 100,
          typeof rafAt === 'number' && Math.abs(rafAt - nowAtRaf) < 50,
          now < 5000,
        ].join()
        "#,
    );
    assert_eq!(r, "true,true,true,true,true");
}

/// An image a style uses (here a background) delays the load event as an `<img>`
/// does (Chrome's image resources for style block the document's load event).
#[test]
fn style_images_delay_the_load_event() {
    let png: &[u8] = PNG_1X1;
    let slow = Resource {
        delay: Duration::from_millis(600),
        ..resource("image/png", png)
    };
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><style>div { width: 10px; height: 10px; background: url(/bg.png) }\
                 </style><div></div>\
                 <script>addEventListener('load', () => { window.at = performance.now() })</script>",
            ),
        ),
        ("/bg.png", slow),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    assert_eq!(server.hits("/bg.png"), 1);
    let at: f64 = eval(&mut page, "window.at").parse().unwrap();
    assert!(
        at >= 550.0,
        "the load event waited for the background ({at} ms)"
    );
}

/// HTML §4.8.4.3.4 "delay load event": an `img` request made while the element would lazy load
/// does not delay the load event, even once it is near the viewport and fetching; an eager one
/// does.
#[test]
fn lazy_images_do_not_delay_the_load_event() {
    let slow = || Resource {
        delay: Duration::from_millis(600),
        ..resource("image/png", PNG_1X1)
    };
    // (each page its own image, so the second does not find the first's in a cache)
    let page_with = |loading: &str| {
        format!(
            "<!doctype html><img loading={loading} src=/{loading}.png>\
             <script>addEventListener('load', () => {{ window.done = document.images[0].complete }})\
             </script>"
        )
    };
    let server = Server::start(vec![
        ("/lazy", resource("text/html", page_with("lazy").as_str())),
        ("/eager", resource("text/html", page_with("eager").as_str())),
        ("/lazy.png", slow()),
        ("/eager.png", slow()),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/lazy"));
    // (whether the image had arrived when the load event fired)
    assert_eq!(
        eval(&mut page, "window.done"),
        "false",
        "a lazy image held it"
    );
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/eager"));
    assert_eq!(
        eval(&mut page, "window.done"),
        "true",
        "an eager image delays it"
    );
}

/// HTML "parse a sizes attribute": an `img` that allows auto-sizes (`loading=lazy`,
/// `sizes=auto`) selects its source for its concrete object size width once it is being rendered,
/// before its lazy fetch starts, so the candidate for 100vw is never fetched; a `source` without
/// `sizes` before it is `auto` too; an eager one stays at 100vw; and a change of that width
/// selects again.
#[test]
fn lazy_auto_sizes_select_for_the_rendered_width() {
    let mut routes = vec![(
        "/",
        resource(
            "text/html",
            "<!doctype html><style>img { height: 10px }</style>\
             <img id=a loading=lazy sizes=auto width=10 srcset='/a-s.png 50w, /a-l.png 51w'>\
             <img id=b loading=lazy sizes=AUTO width=500 srcset='/b-s.png 50w, /b-l.png 51w'>\
             <picture><source srcset='/c-s.png 50w, /c-l.png 51w'>\
             <img id=c loading=lazy sizes=auto width=10></picture>\
             <img id=d sizes=auto width=10 srcset='/d-s.png 50w, /d-l.png 51w'>",
        ),
    )];
    for p in [
        "/a-s.png", "/a-l.png", "/b-s.png", "/b-l.png", "/c-s.png", "/c-l.png", "/d-s.png",
        "/d-l.png",
    ] {
        routes.push((p, resource("image/png", PNG_1X1)));
    }
    let server = Server::start(routes);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    settle(&mut page);
    let src = |page: &mut Page, id: &str| {
        let s = eval(page, &format!("new URL({id}.currentSrc).pathname"));
        s.trim_matches('"').to_owned()
    };
    assert_eq!(src(&mut page, "a"), "/a-s.png");
    assert_eq!(src(&mut page, "b"), "/b-l.png");
    assert_eq!(src(&mut page, "c"), "/c-s.png");
    assert_eq!(src(&mut page, "d"), "/d-l.png");
    for n in ["a", "c"] {
        assert_eq!(
            server.hits(&format!("/{n}-l.png")),
            0,
            "{n}: 100vw's candidate"
        );
    }
    eval(&mut page, "a.style.width = '500px'");
    settle(&mut page);
    assert_eq!(src(&mut page, "a"), "/a-l.png");
}

/// `document.fonts.ready` is resolved by the event loop outside any task once
/// the last web font arrives; its reactions run then (HTML §8.1.7.3: a microtask checkpoint
/// whenever the stack empties), not at the next task, which an idle page never gets.
#[test]
fn fonts_ready_reactions_run_when_the_last_font_arrives() {
    let ahem = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../snowlark-layout/tests/fixtures/Ahem.ttf"
    ))
    .unwrap();
    let slow = Resource {
        delay: Duration::from_millis(500),
        ..resource("font/ttf", ahem)
    };
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><style>@font-face { font-family: A; src: url(/a.ttf) }\
                 p { font-family: A }</style><p>x</p>                 <script>document.fonts.ready.then(() => { window.ready = performance.now() });                 addEventListener('load', () => { window.at = performance.now() })</script>",
            ),
        ),
        ("/a.ttf", slow),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    assert!(page.run_until_settled(Duration::from_secs(10)));
    assert_eq!(eval(&mut page, "typeof window.ready"), "number");
    // A web font in fetch delays the load event, as in Chrome.
    let at: f64 = eval(&mut page, "window.at").parse().unwrap();
    assert!(at >= 450.0, "the load event waited for the font ({at} ms)");
}

/// Styles kept across restyles (stylo patch 66) and the style sharing cache: parents of other
/// classes whose own rules are the same get the kept style, but not one `Arc`, or their children
/// share styles as cousins although `.min-width > .stretch` and `.max-width > .stretch` differ.
/// The restyle of everything a web font's load brings (here one that fails) is where it showed
/// (WPT css/css-sizing/keyword-sizes-for-intrinsic-contributions.html).
#[test]
fn kept_styles_do_not_let_children_of_other_classes_share() {
    let group = |class: &str, width: u32| {
        format!(
            "<div class=wrapper style='width: {width}px'>\
             <div class='test {class}'><div style='width: min-content'>XXX XXX</div></div>\
             <div class='test {class}'><div style='width: fit-content'>XXX XXX</div></div>\
             <div class='test {class}'><div style='width: max-content'>XXX XXX</div></div>\
             <div class='test {class}'><div class=stretch>XXX XXX</div></div></div>"
        )
    };
    let mut html = String::from(
        "<!doctype html><style>@font-face { font-family: A; src: url(/missing.ttf) }\
         .wrapper { display: inline-block; vertical-align: top; margin-right: 200px }\
         .test { float: left; clear: both; margin: 5px; border: 3px solid; padding: 2px;\
         font: 20px/1 A }\
         .test.min-width > div { width: 0px }\
         .test.max-width > div { width: 200px }\
         .width > .stretch { width: stretch }\
         .min-width > .stretch { min-width: stretch }\
         .max-width > .stretch { max-width: stretch }</style>",
    );
    for class in ["width", "min-width", "max-width"] {
        for width in [0, 100, 200] {
            html += &group(class, width);
        }
        html += "<hr>";
    }
    let server = Server::start(vec![("/", resource("text/html", html))]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    assert!(page.run_until_settled(Duration::from_secs(10)));
    let styles = eval(
        &mut page,
        "[...document.querySelectorAll('.stretch')].map(d => { const c = getComputedStyle(d); \
         return d.parentNode.classList[1] + ' ' + c.minWidth + ' ' + c.maxWidth; }).join(', ')",
    );
    let expected = [
        ("width", "0px none"),
        ("min-width", "stretch none"),
        ("max-width", "0px stretch"),
    ]
    .iter()
    .flat_map(|(class, values)| std::iter::repeat_n(format!("{class} {values}"), 3))
    .collect::<Vec<_>>()
    .join(", ");
    assert_eq!(styles, expected);
}

/// Runs `start` and waits for the new document it brings (the old one's global goes with it).
fn next_document(page: &mut Page, start: impl FnOnce(&mut Page)) {
    eval(page, "window.previous = 1");
    start(page);
    let begun = std::time::Instant::now();
    while eval(page, "String(window.previous)") == "1" || !page.load_complete() {
        assert!(begun.elapsed() < Duration::from_secs(10), "a new document");
        page.run_for(Duration::from_millis(10));
    }
}

fn reload(page: &mut Page, url: url::Url) {
    next_document(page, |p| p.reload(url));
}

fn cached_html(body: &str) -> Resource {
    Resource {
        headers: vec![("Cache-Control".into(), "max-age=3600".into())],
        ..resource("text/html", body)
    }
}

#[test]
fn a_reload_asks_the_server_though_the_cached_document_is_fresh() {
    let server = Server::start(vec![("/doc", cached_html("<title>one</title>"))]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net);
    let url = server.url("/doc");
    load(&mut p, url.clone());
    assert_eq!(eval(&mut p, "document.title"), "one");
    server.set("/doc", cached_html("<title>two</title>"));
    // A navigation takes the fresh response from the cache (RFC 9111 §4)...
    next_document(&mut p, |p| p.navigate(url.clone()));
    assert_eq!(eval(&mut p, "document.title"), "one");
    assert_eq!(server.hits("/doc"), 1);
    // ...and a reload is the user asking for the page as it is now.
    reload(&mut p, url);
    assert_eq!(eval(&mut p, "document.title"), "two");
    assert_eq!(server.hits("/doc"), 2);
}

#[test]
fn location_reload_is_a_reload() {
    let server = Server::start(vec![("/doc", cached_html("<title>one</title>"))]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net);
    // (with a fragment: a reload is no fragment navigation)
    load(
        &mut p,
        url::Url::parse(&format!("{}#part", server.url("/doc"))).unwrap(),
    );
    server.set("/doc", cached_html("<title>two</title>"));
    eval(&mut p, "location.reload()");
    let start = std::time::Instant::now();
    while eval(&mut p, "document.title") != "two" {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the page reloads"
        );
        p.run_for(Duration::from_millis(10));
    }
    assert_eq!(server.hits("/doc"), 2);
}

#[test]
fn a_reload_leaves_subresources_to_their_freshness() {
    let doc = cached_html("<script src=a.js></script>");
    let script = Resource {
        headers: vec![("Cache-Control".into(), "max-age=3600".into())],
        ..resource("text/javascript", "window.ran = 1")
    };
    let server = Server::start(vec![("/doc", doc), ("/a.js", script)]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net);
    load(&mut p, server.url("/doc"));
    reload(&mut p, server.url("/doc"));
    assert_eq!(eval(&mut p, "String(window.ran)"), "1");
    assert_eq!((server.hits("/doc"), server.hits("/a.js")), (2, 1));
}

/// RFC 9111 §4.3: a reload validates what is stored, and a 304 keeps it.
#[test]
fn a_reload_revalidates_and_a_304_keeps_the_stored_document() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let heads: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = heads.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        head.push_str(&line.to_ascii_lowercase());
                    }
                    let body = "<title>stored</title>";
                    let answer = if head.contains("if-none-match: \"v1\"") {
                        "HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nCache-Control: max-age=3600\r\n\r\n"
                            .to_owned()
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nETag: \"v1\"\r\n\
                             Cache-Control: max-age=3600\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    };
                    seen.lock().unwrap().push(head);
                    if stream.write_all(answer.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net);
    let url = url::Url::parse(&format!("http://{addr}/doc")).unwrap();
    load(&mut p, url.clone());
    reload(&mut p, url);
    assert_eq!(eval(&mut p, "document.title"), "stored");
    let heads = heads.lock().unwrap();
    let document: Vec<&String> = heads.iter().filter(|h| h.contains("get /doc")).collect();
    assert_eq!(document.len(), 2, "{heads:?}");
    assert!(
        document[1].contains("if-none-match: \"v1\""),
        "{}",
        document[1]
    );
    assert!(
        document[1].contains("cache-control: max-age=0"),
        "{}",
        document[1]
    );
}

/// A `use` element is remembered once its load event is reported; once removed and collected, the
/// next SVG on the page must not ask about it (svt.se's player: `stale NodeId`).
#[test]
fn a_freed_use_element_does_not_reach_the_next_svg_pass() {
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><meta charset=utf-8>                 <svg id=a width=20 height=20><use href='/s.svg#i'></use></svg>                 <svg id=b width=20 height=20><rect width=5 height=5></rect></svg>",
            ),
        ),
        (
            "/s.svg",
            resource(
                "image/svg+xml",
                "<svg xmlns='http://www.w3.org/2000/svg'><symbol id='i' viewBox='0 0 1 1'>                 <rect width='1' height='1'/></symbol></svg>",
            ),
        ),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    settle(&mut page);
    eval(&mut page, "document.getElementById('a').remove(); 1");
    for _ in 0..4 {
        assert!(page.collect_garbage());
    }
    eval(
        &mut page,
        "document.getElementById('b').firstChild.setAttribute('width', '6'); 1",
    );
    settle(&mut page);
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('b').firstChild.getAttribute('width')"
        ),
        "6"
    );
}

#[test]
fn a_parser_inserted_script_waits_for_a_stylesheet_that_blocks_scripts() {
    // HTML "has a style sheet that is blocking scripts": an inline script after a pending
    // `<link rel=stylesheet>` runs once the sheet has loaded, so it sees the sheet's rules.
    let slow = Resource {
        delay: Duration::from_millis(150),
        ..resource("text/css", "#a { display: table }")
    };
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><meta charset=utf-8><link rel=stylesheet href=/s.css>\
                 <div id=a></div><script>window.seen = getComputedStyle(a).display + ':' + document.styleSheets.length;</script>",
            ),
        ),
        ("/s.css", slow),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered_page(&net);
    load(&mut page, server.url("/"));
    settle(&mut page);
    assert_eq!(eval(&mut page, "window.seen"), "table:1");
}

/// HTML §8.1.6.1: `import()` in a frame loads into the frame's module map and evaluates the module
/// in the frame's realm (its `window` is the frame's), not in the realm that happened to be current
/// when the fetch came back. Also a module the top-level page imported first is the frame's own
/// instance, evaluated again there.
#[test]
fn a_frames_dynamic_import_evaluates_in_the_frame() {
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><script>window.r = []; import('/m.js').then(() => r.push('top:' + window.where));</script>\
                 <iframe src=/frame.html></iframe>",
            ),
        ),
        (
            "/frame.html",
            resource(
                "text/html",
                "<!doctype html><script>import('/m.js').then(m => parent.r.push('frame:' + window.where + ':' + m.realm + ':' + (typeof parent.where)));</script>",
            ),
        ),
        (
            "/m.js",
            resource(
                "text/javascript",
                "window.where = (window === window.top) ? 'top' : 'frame'; export const realm = window.where;",
            ),
        ),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        page.run_for(Duration::from_millis(10));
    }
    let mut got: Vec<String> = eval(&mut page, "r.join()")
        .split(',')
        .map(str::to_owned)
        .collect();
    got.sort();
    assert_eq!(got, ["frame:frame:frame:string", "top:top"]);
}

/// HTML §7.4.2.3.3 "navigate to a `javascript:` URL": the URL's script runs in the target's
/// document; a string result becomes a new HTML document there (with the active document's URL,
/// and the frame's `load` event), anything else leaves the document as it is.
#[test]
fn javascript_urls_navigate_frames_to_their_string_result() {
    let server = Server::start(vec![
        (
            "/",
            resource(
                "text/html",
                "<!doctype html><body><script>window.r = [];\
                 const a = document.createElement('iframe');\
                 a.src = \"javascript:'<p id=x>made</p>'\";\
                 a.onload = () => r.push('a:' + a.contentDocument.getElementById('x').textContent + ':' + a.contentWindow.location.href);\
                 document.body.append(a);\
                 const b = document.createElement('iframe');\
                 b.onload = () => {\
                   if (r.includes('b-start')) return;\
                   r.push('b-start');\
                   b.contentDocument.body.textContent = 'kept';\
                   b.contentWindow.location.href = 'javascript:void 0';\
                   b.contentWindow.location.href = \"javascript:window.ran = 1; 'replaced'\";\
                   setTimeout(() => r.push('b:' + b.contentDocument.body.textContent + ':' + b.contentWindow.ran), 200);\
                 };\
                 document.body.append(b);</script>",
            ),
        ),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load(&mut page, server.url("/"));
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(800) {
        page.run_for(Duration::from_millis(10));
    }
    // (b's window is its initial about:blank's, reused by the same-origin document that replaces it)
    assert_eq!(
        eval(&mut page, "r.sort().join()"),
        "a:made:about:blank,b-start,b:replaced:1"
    );
}
