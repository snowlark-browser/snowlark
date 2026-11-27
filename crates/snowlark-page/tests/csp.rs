// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Content Security Policy enforcement, end to end: documents with a policy (headers or `<meta>`)
//! served by an in-process HTTP server that counts requests and keeps the bodies POSTed to it. Each
//! test asserts that what the policy blocks is never fetched (or run), the `securitypolicyviolation`
//! event's fields, and the report sent to `report-uri` / `report-to`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions};

#[derive(Clone)]
struct Resource {
    status: u16,
    mime: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

fn resource(mime: &str, body: impl Into<Vec<u8>>) -> Resource {
    Resource {
        status: 200,
        mime: mime.into(),
        body: body.into(),
        headers: Vec::new(),
    }
}

fn html(body: &str) -> Resource {
    resource(
        "text/html",
        format!("<!doctype html><meta charset=utf-8>{body}"),
    )
}

/// A document with `policy` as its `Content-Security-Policy` header.
fn with_csp(mut r: Resource, policy: &str) -> Resource {
    r.headers
        .push(("Content-Security-Policy".into(), policy.into()));
    r
}

fn redirect(to: &str) -> Resource {
    Resource {
        status: 302,
        mime: "text/plain".into(),
        body: Vec::new(),
        headers: vec![("Location".into(), to.into())],
    }
}

/// An HTTP/1.1 server over a path → resource map that counts requests per path and keeps POST bodies.
/// `ORIGIN` in a header value stands for the server's origin.
#[allow(dead_code)]
struct Server {
    addr: std::net::SocketAddr,
    resources: Arc<Mutex<HashMap<String, Resource>>>,
    hits: Arc<Mutex<HashMap<String, usize>>>,
    posts: Arc<Mutex<Vec<(String, String, String)>>>,
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
        let posts: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
        let (res, h, po) = (resources.clone(), hits.clone(), posts.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (res, h, po) = (res.clone(), h.clone(), po.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut first = String::new();
                        if reader.read_line(&mut first).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut length = 0usize;
                        let mut content_type = String::new();
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                                break;
                            }
                            let lower = line.to_ascii_lowercase();
                            if let Some(v) = lower.strip_prefix("content-length:") {
                                length = v.trim().parse().unwrap_or(0);
                            }
                            if let Some(v) = lower.strip_prefix("content-type:") {
                                content_type = v.trim().to_owned();
                            }
                        }
                        let mut body = vec![0u8; length];
                        if length > 0 && reader.read_exact(&mut body).is_err() {
                            return;
                        }
                        let mut parts = first.split_whitespace();
                        let method = parts.next().unwrap_or("GET").to_owned();
                        let path = parts.next().unwrap_or("/").to_owned();
                        let path = path.split('?').next().unwrap_or("/").to_owned();
                        *h.lock().unwrap().entry(path.clone()).or_default() += 1;
                        if method == "POST" {
                            po.lock().unwrap().push((
                                path.clone(),
                                content_type,
                                String::from_utf8_lossy(&body).into_owned(),
                            ));
                        }
                        let mut r =
                            res.lock()
                                .unwrap()
                                .get(&path)
                                .cloned()
                                .unwrap_or_else(|| Resource {
                                    status: 404,
                                    ..resource("text/plain", "")
                                });
                        // "ORIGIN" in headers stands for the server's own origin.
                        let origin = format!("http://{addr}");
                        for (_, v) in r.headers.iter_mut() {
                            *v = v.replace("ORIGIN", &origin);
                        }
                        let mut head = format!(
                            "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
                            r.status,
                            r.mime,
                            r.body.len()
                        );
                        for (n, v) in &r.headers {
                            head.push_str(&format!("{n}: {v}\r\n"));
                        }
                        head.push_str("\r\n");
                        if stream.write_all(head.as_bytes()).is_err()
                            || stream.write_all(&r.body).is_err()
                        {
                            return;
                        }
                        let _ = stream.flush();
                    }
                });
            }
        });
        Server {
            addr,
            resources,
            hits,
            posts,
        }
    }

    fn url(&self, path: &str) -> url::Url {
        url::Url::parse(&format!("http://{}{path}", self.addr)).unwrap()
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    fn set(&self, path: &str, r: Resource) {
        self.resources.lock().unwrap().insert(path.to_owned(), r);
    }

    /// The bodies POSTed to `path`, with their content types.
    fn posts(&self, path: &str) -> Vec<(String, String)> {
        self.posts
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _, _)| p == path)
            .map(|(_, t, b)| (t.clone(), b.clone()))
            .collect()
    }
}

fn page(net: &NetThread, rendered: bool) -> Page {
    Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: rendered.then(|| snowlark_page::RenderSetup::new().unwrap()),
        ..Default::default()
    })
}

/// Loads `url` and runs the page until it is quiet (subresources, tasks and reports).
fn load(page: &mut Page, url: url::Url) {
    page.navigate(url);
    let start = Instant::now();
    while !page.load_complete() {
        assert!(start.elapsed() < Duration::from_secs(10), "the page loads");
        page.run_for(Duration::from_millis(10));
    }
    settle(page);
}

fn settle(page: &mut Page) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(600) {
        page.run_for(Duration::from_millis(10));
    }
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Records each `securitypolicyviolation` event's fields in `window.v` (a nonce'd inline script, so
/// it runs under any of these policies).
const RECORD: &str = "<script nonce=rec>window.v = []; document.addEventListener('securitypolicyviolation', e => v.push({ d: e.effectiveDirective, vd: e.violatedDirective, b: e.blockedURI, disp: e.disposition, p: e.originalPolicy, doc: e.documentURI, s: e.statusCode, t: e.target === document ? 'document' : e.target.nodeName, sample: e.sample, trusted: e.isTrusted, bubbles: e.bubbles, composed: e.composed }));</script>";

fn violations(page: &mut Page) -> String {
    eval(page, "JSON.stringify(v)")
}

fn net() -> NetThread {
    NetThread::start(NetConfig::default()).unwrap()
}

#[test]
fn script_src_blocks_external_and_inline_scripts_and_reports_them() {
    let policy = "script-src 'nonce-rec' 'nonce-n'; report-uri /csp-report";
    let server = Server::start(vec![
        (
            "/",
            with_csp(
                html(&format!(
                    "{RECORD}<script src=/blocked.js></script><script nonce=n src=/allowed.js></script>\
                     <script>window.inline = 1</script><button id=b onclick='window.clicked = 1'>b</button>"
                )),
                policy,
            ),
        ),
        ("/blocked.js", resource("text/javascript", "window.blocked = 1")),
        ("/allowed.js", resource("text/javascript", "window.allowed = 1")),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(
        server.hits("/blocked.js"),
        0,
        "a blocked script is not fetched"
    );
    assert_eq!(server.hits("/allowed.js"), 1);
    assert_eq!(eval(&mut page, "String(window.allowed)"), "1");
    assert_eq!(eval(&mut page, "String(window.blocked)"), "undefined");
    assert_eq!(eval(&mut page, "String(window.inline)"), "undefined");
    eval(&mut page, "document.getElementById('b').click()");
    assert_eq!(eval(&mut page, "String(window.clicked)"), "undefined");
    settle(&mut page);
    let v = violations(&mut page);
    let blocked = server.url("/blocked.js");
    assert!(
        v.contains(&format!(
            "{{\"d\":\"script-src-elem\",\"vd\":\"script-src-elem\",\"b\":\"{blocked}\",\"disp\":\"enforce\",\"p\":\"{}\",\"doc\":\"{}\",\"s\":200,\"t\":\"document\"",
            policy,
            server.url("/")
        )),
        "{v}"
    );
    // The inline script and the event handler attribute: at their elements, "inline".
    assert!(
        v.contains("\"d\":\"script-src-elem\",\"vd\":\"script-src-elem\",\"b\":\"inline\""),
        "{v}"
    );
    assert!(v.contains("\"t\":\"SCRIPT\""), "{v}");
    assert!(
        v.contains("\"d\":\"script-src-attr\",\"vd\":\"script-src-attr\",\"b\":\"inline\""),
        "{v}"
    );
    assert!(v.contains("\"t\":\"BUTTON\""), "{v}");
    assert!(
        v.contains("\"trusted\":true,\"bubbles\":true,\"composed\":true"),
        "{v}"
    );
    // One report per violation, as JSON to report-uri.
    let reports = server.posts("/csp-report");
    assert!(
        reports.iter().all(|(t, _)| t == "application/csp-report"),
        "{reports:?}"
    );
    let r = reports
        .iter()
        .find(|(_, b)| b.contains(blocked.as_str()))
        .expect("a report of the blocked script");
    let body: serde_json::Value = serde_json::from_str(&r.1).unwrap();
    assert_eq!(body["csp-report"]["blocked-uri"], blocked.as_str());
    assert_eq!(body["csp-report"]["effective-directive"], "script-src-elem");
    assert_eq!(body["csp-report"]["disposition"], "enforce");
    assert_eq!(body["csp-report"]["document-uri"], server.url("/").as_str());
    assert_eq!(reports.len(), 3, "{reports:?}");
    let console = page.console_messages();
    assert!(
        console.iter().any(|(level, m)| *level == "error"
            && m.contains("Refused to load")
            && m.contains("blocked.js")),
        "{console:?}"
    );
}

#[test]
fn strict_dynamic_allows_scripts_a_trusted_script_inserts() {
    let server = Server::start(vec![
        (
            "/",
            with_csp(
                html(&format!(
                    "{RECORD}<script nonce=n>const s = document.createElement('script'); s.src = '/dynamic.js'; document.head.append(s);</script>\
                     <script src=/parser.js></script>"
                )),
                "script-src 'nonce-rec' 'nonce-n' 'strict-dynamic'",
            ),
        ),
        ("/dynamic.js", resource("text/javascript", "window.dynamic = 1")),
        ("/parser.js", resource("text/javascript", "window.parser = 1")),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(server.hits("/dynamic.js"), 1);
    assert_eq!(eval(&mut page, "String(window.dynamic)"), "1");
    assert_eq!(server.hits("/parser.js"), 0, "parser-inserted, no nonce");
}

#[test]
fn modules_carry_their_nonce_to_their_imports() {
    let server = Server::start(vec![
        (
            "/",
            with_csp(
                html(
                    "<script type=module nonce=n src=/m.js></script>\
                     <script type=module nonce=n>import('/dyn.js').then(() => window.dyn = 1)</script>",
                ),
                "script-src 'nonce-n'",
            ),
        ),
        (
            "/m.js",
            resource("text/javascript", "import '/child.js'; window.m = 1;"),
        ),
        ("/child.js", resource("text/javascript", "window.child = 1;")),
        ("/dyn.js", resource("text/javascript", "")),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(eval(&mut page, "[window.m, window.child].join()"), "1,1");
    assert_eq!(eval(&mut page, "String(window.dyn)"), "1");
}

#[test]
fn style_img_and_font_src_block_the_renderers_fetches() {
    let policy = "default-src 'self'; style-src 'self' 'nonce-s'; img-src ORIGIN/allowed/; font-src 'none'; script-src 'nonce-rec'; report-uri /r";
    let server = Server::start(vec![
        (
            "/",
            with_csp(
                html(&format!(
                    "{RECORD}<script nonce=rec>window.imgs = []; for (const t of ['load', 'error']) {{                        document.addEventListener(t, e => {{ if (e.target.tagName === 'IMG') {{                          imgs.push(t + ' ' + e.target.getAttribute('src')); }} }}, true); }}</script>                     <link rel=stylesheet href=/s.css><link rel=stylesheet href=http://localhost:4/x.css>\
                     <style>p {{ color: red }}</style><style nonce=s>@import '/imported.css';</style>\
                     <img src=/i.png><img src=/allowed/ok.png><img src=/redirect.png><p>text</p>                     <p id=attr style='color: rgb(255, 0, 0)'>attr</p><p id=cssom>cssom</p>                     <script nonce=rec>document.getElementById('cssom').style.color = 'rgb(0, 128, 0)';</script>"
                )),
                policy,
            ),
        ),
        (
            "/s.css",
            resource(
                "text/css",
                "@font-face { font-family: f; src: url(/f.woff) } p { font-family: f; background: url(/bg.png) }",
            ),
        ),
        ("/imported.css", resource("text/css", "")),
        ("/i.png", resource("image/png", vec![0u8; 16])),
        ("/allowed/ok.png", resource("image/png", vec![0u8; 16])),
        ("/redirect.png", redirect("/allowed/../i.png")),
        ("/bg.png", resource("image/png", vec![0u8; 16])),
        ("/f.woff", resource("font/woff", vec![0u8; 16])),
    ]);
    let net = net();
    let mut page = page(&net, true);
    load(&mut page, server.url("/"));
    page.update_the_rendering();
    settle(&mut page);
    assert_eq!(server.hits("/s.css"), 1);
    assert_eq!(
        server.hits("/imported.css"),
        1,
        "a nonce'd <style>'s import"
    );
    assert_eq!(server.hits("/i.png"), 0, "img-src");
    assert_eq!(server.hits("/bg.png"), 0, "img-src for CSS images");
    assert_eq!(server.hits("/allowed/ok.png"), 1);
    assert_eq!(server.hits("/f.woff"), 0, "font-src");
    // A blocked image's `error` event (a parser-inserted script waits for the style sheets before it, so
    // the listener is put first); the allowed one loads (it is no image: error too).
    assert_eq!(
        eval(&mut page, "imgs.sort().join()"),
        "error /allowed/ok.png,error /i.png,error /redirect.png"
    );
    // style-src-attr: a style attribute from markup is blocked; the CSSOM's is not.
    assert_eq!(
        eval(
            &mut page,
            "[getComputedStyle(attr).color, getComputedStyle(cssom).color].join(' ')"
        ),
        "rgb(0, 0, 0) rgb(0, 128, 0)"
    );
    // The un-nonce'd <style> has no sheet.
    assert_eq!(
        eval(&mut page, "String(document.styleSheets.length)"),
        "2",
        "s.css and the nonce'd style (the cross-origin sheet is blocked)"
    );
    let v = violations(&mut page);
    for d in ["img-src", "font-src", "style-src-elem", "style-src-attr"] {
        assert!(v.contains(&format!("\"d\":\"{d}\"")), "{d}: {v}");
    }
    assert!(v.contains("\"t\":\"STYLE\""), "{v}");
    assert!(v.contains("http://localhost:4/x.css"), "{v}");
    let reports = server.posts("/r");
    assert!(
        reports.iter().any(|(_, b)| b.contains("\"font-src\"")),
        "{reports:?}"
    );
}

#[test]
fn a_redirect_is_checked_again() {
    let server = Server::start(vec![
        (
            "/",
            with_csp(
                html(&format!(
                    "{RECORD}<script nonce=n src=/go.js></script><script nonce=n>fetch('/data/go').then(() => window.fetched = 'yes', () => window.fetched = 'no')</script>"
                )),
                "script-src 'nonce-rec' 'nonce-n' http://localhost:1/; connect-src 'self'",
            ),
        ),
        ("/go.js", redirect("http://localhost:2/elsewhere.js")),
        ("/data/go", redirect("http://localhost:3/data")),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    // A nonce allows the first URL; the redirect target is checked without the nonce... a nonce'd
    // request stays allowed (CSP §6.7.1.1), so the redirect is followed to an unreachable host,
    // while fetch()'s redirect leaves 'self' and is blocked.
    assert_eq!(eval(&mut page, "String(window.fetched)"), "no");
    let v = violations(&mut page);
    assert!(v.contains("\"d\":\"connect-src\""), "{v}");
    // The violation names the original URL, not where it was redirected (cross-origin).
    assert!(v.contains(server.url("/data/go").as_str()), "{v}");
}

#[test]
fn a_blocked_redirects_violation_is_queued_before_the_scripts_error_event() {
    // CSP 3 §5.5 queues the violation's event when the redirect's check fails, before the fetch
    // returns its network error to the script element: a task the `error` handler queues runs after
    // it (as WPT's content-security-policy/gen tests check).
    let server = Server::start(vec![
        (
            "/",
            with_csp(
                html(&format!(
                    "{RECORD}<script nonce=rec>window.order = [];\
                     for (let i = 0; i < 8; i++) {{ const s = document.createElement('script');\
                     s.src = '/go.js?' + i;\
                     s.onerror = () => setTimeout(() => order.push(v.some(x => x.b.endsWith('/go.js?' + i)) ? 'after' : 'before'));\
                     document.head.append(s); }}</script>"
                )),
                "script-src 'self' 'nonce-rec'",
            ),
        ),
        ("/go.js", redirect("http://localhost:2/elsewhere.js")),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(eval(&mut page, "String(v.length)"), "8");
    assert_eq!(
        eval(&mut page, "order.join()"),
        ["after"; 8].join(","),
        "a task queued at each error event runs after its violation's event"
    );
}

#[test]
fn connect_src_blocks_fetch_xhr_and_beacons() {
    let server = Server::start(vec![
        (
            "/",
            with_csp(
                html(&format!(
                    "{RECORD}<script nonce=n>\
                     fetch('/blocked').then(() => window.f = 'ok', e => window.f = e.name);\
                     const xhr = new XMLHttpRequest(); xhr.open('GET', '/xhr'); xhr.onerror = () => window.x = 'error'; xhr.send();\
                     window.beacon = navigator.sendBeacon('/beacon', 'b');\
                     fetch('/allowed/a').then(() => window.a = 'ok');\
                     </script>"
                )),
                "connect-src ORIGIN/allowed/; script-src 'nonce-rec' 'nonce-n'",
            ),
        ),
        ("/allowed/a", resource("text/plain", "a")),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(server.hits("/blocked"), 0);
    assert_eq!(server.hits("/xhr"), 0);
    assert_eq!(server.hits("/beacon"), 0);
    assert_eq!(server.hits("/allowed/a"), 1);
    assert_eq!(eval(&mut page, "[f, x, a].join()"), "TypeError,error,ok");
    let v = violations(&mut page);
    assert_eq!(v.matches("\"d\":\"connect-src\"").count(), 3, "{v}");
}

#[test]
fn frame_src_blocks_a_frame_and_leaves_it_an_opaque_empty_document() {
    let server = Server::start(vec![
        (
            "/",
            with_csp(
                html(&format!(
                    "{RECORD}<body><script nonce=rec>const f = document.createElement('iframe'); f.id = 'blocked';\
                     f.onload = () => window.loaded = (window.loaded || 0) + 1; f.src = '/frame.html'; document.body.append(f);</script>\
                     <iframe id=srcdoc srcdoc='<script>parent.srcdocRan = 1</script>'></iframe>"
                )),
                "frame-src 'none'; script-src 'nonce-rec'",
            ),
        ),
        ("/frame.html", html("<script>parent.framed = 1</script>")),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(server.hits("/frame.html"), 0);
    assert_eq!(eval(&mut page, "String(window.framed)"), "undefined");
    assert_eq!(
        eval(&mut page, "String(window.loaded)"),
        "1",
        "the iframe's load event"
    );
    assert_eq!(
        eval(
            &mut page,
            "String(document.getElementById('blocked').contentDocument)"
        ),
        "null",
        "cross-origin (opaque)"
    );
    // A srcdoc frame inherits the policy: its inline script (no nonce) does not run.
    assert_eq!(eval(&mut page, "String(window.srcdocRan)"), "undefined");
    let v = violations(&mut page);
    assert!(v.contains("\"d\":\"frame-src\""), "{v}");
}

#[test]
fn frame_ancestors_and_x_frame_options_block_framing() {
    let mut denied = html("<script>parent.denied = 1</script>");
    denied.headers.push((
        "Content-Security-Policy".into(),
        "frame-ancestors 'none'; report-uri /fa-report".into(),
    ));
    let mut xfo = html("<script>parent.xfo = 1</script>");
    xfo.headers.push(("X-Frame-Options".into(), "DENY".into()));
    let mut sameorigin = html("<script>parent.same = 1</script>");
    sameorigin
        .headers
        .push(("X-Frame-Options".into(), "SAMEORIGIN".into()));
    let mut self_only = html("<script>parent.selfOnly = 1</script>");
    self_only.headers.push((
        "Content-Security-Policy".into(),
        "frame-ancestors 'self'".into(),
    ));
    let server = Server::start(vec![
        (
            "/",
            html(
                "<iframe src=/denied.html></iframe><iframe src=/xfo.html></iframe>\
                 <iframe src=/same.html></iframe><iframe src=/self.html></iframe>",
            ),
        ),
        ("/denied.html", denied),
        ("/xfo.html", xfo),
        ("/same.html", sameorigin),
        ("/self.html", self_only),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(
        eval(
            &mut page,
            "[window.denied, window.xfo, window.same, window.selfOnly].join()"
        ),
        ",,1,1"
    );
    // The frames that may not show their response have empty, cross-origin documents.
    assert_eq!(
        eval(
            &mut page,
            "Array.from(document.querySelectorAll('iframe'), f => f.contentDocument === null).join()"
        ),
        "true,true,false,false"
    );
    let reports = server.posts("/fa-report");
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].1.contains("\"frame-ancestors\""), "{reports:?}");
}

#[test]
fn worker_src_blocks_workers_and_workers_keep_their_own_policy() {
    let mut worker = resource(
        "text/javascript",
        "self.addEventListener('securitypolicyviolation', e => postMessage('spv ' + e.effectiveDirective));\
         try { eval('1'); postMessage('eval ran'); } catch (e) { postMessage('eval ' + e.name); }\
         fetch('/from-worker').then(() => postMessage('fetched'), () => postMessage('fetch failed'));",
    );
    worker.headers.push((
        "Content-Security-Policy".into(),
        "connect-src 'none'".into(),
    ));
    let server = Server::start(vec![
        (
            "/",
            with_csp(
                html(&format!(
                    "{RECORD}<script nonce=n>window.msgs = [];\
                     const blocked = new Worker('/blocked-worker.js'); blocked.onerror = () => window.blockedError = 1;\
                     const w = new Worker('/allowed/w.js'); w.onmessage = e => msgs.push(e.data);</script>"
                )),
                "worker-src ORIGIN/allowed/; script-src 'nonce-rec' 'nonce-n'",
            ),
        ),
        ("/blocked-worker.js", resource("text/javascript", "")),
        ("/allowed/w.js", worker),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    let start = Instant::now();
    while eval(&mut page, "msgs.length") != "3" && start.elapsed() < Duration::from_secs(5) {
        page.run_for(Duration::from_millis(10));
    }
    assert_eq!(server.hits("/blocked-worker.js"), 0);
    assert_eq!(eval(&mut page, "String(window.blockedError)"), "1");
    assert_eq!(
        server.hits("/from-worker"),
        0,
        "the worker's own connect-src"
    );
    // The worker's policy has no script-src: eval runs there.
    let msgs = eval(&mut page, "msgs.sort().join('|')");
    assert_eq!(msgs, "eval ran|fetch failed|spv connect-src", "{msgs}");
    let v = violations(&mut page);
    assert!(v.contains("\"d\":\"worker-src\""), "{v}");
}

#[test]
fn form_action_blocks_submissions_and_redirects() {
    let server = Server::start(vec![
        (
            "/",
            with_csp(
                html(&format!(
                    "{RECORD}<iframe name=t></iframe><form id=a action=/submit target=t><input name=q value=1></form>\
                     <form id=b action=/allowed/redirect target=t></form>"
                )),
                "form-action ORIGIN/allowed/; script-src 'nonce-rec'",
            ),
        ),
        ("/submit", html("submitted")),
        ("/allowed/redirect", redirect("http://localhost:4/elsewhere")),
        ("/elsewhere", html("redirected")),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    eval(&mut page, "document.getElementById('a').submit()");
    settle(&mut page);
    assert_eq!(server.hits("/submit"), 0);
    eval(&mut page, "document.getElementById('b').submit()");
    settle(&mut page);
    // (after a redirect a source's path no longer matters, so the redirect leaves the origin)
    assert_eq!(server.hits("/allowed/redirect"), 1);
    let v = violations(&mut page);
    assert_eq!(v.matches("\"d\":\"form-action\"").count(), 2, "{v}");
}

#[test]
fn base_uri_keeps_the_fallback_base_url() {
    let server = Server::start(vec![(
        "/dir/page.html",
        with_csp(
            html(&format!(
                "{RECORD}<base href=http://example.invalid/other/>"
            )),
            "base-uri 'self'; script-src 'nonce-rec'",
        ),
    )]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/dir/page.html"));
    assert_eq!(
        eval(&mut page, "document.baseURI"),
        server.url("/dir/page.html").as_str()
    );
    let v = violations(&mut page);
    assert!(v.contains("\"d\":\"base-uri\""), "{v}");
    assert!(v.contains("\"t\":\"BASE\""), "{v}");
}

#[test]
fn eval_and_wasm_follow_script_src() {
    let server = Server::start(vec![(
        "/",
        with_csp(
            html(&format!(
                "{RECORD}<script nonce=n>\
                 try {{ eval('1'); window.e = 'ran'; }} catch (x) {{ window.e = x.name; }}\
                 try {{ new Function('return 1'); window.fn = 'ran'; }} catch (x) {{ window.fn = x.name; }}\
                 setTimeout('window.timer = 1', 0);\
                 window.w = WebAssembly.compile(new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0])).then(() => 'ran', x => x.name);\
                 </script>"
            )),
            "script-src 'nonce-rec' 'nonce-n' 'report-sample'",
        ),
    )]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(eval(&mut page, "[e, fn].join()"), "EvalError,EvalError");
    assert_eq!(eval(&mut page, "String(window.timer)"), "undefined");
    eval(&mut page, "w.then(r => window.wr = r)");
    settle(&mut page);
    assert_eq!(eval(&mut page, "wr"), "CompileError");
    let v = violations(&mut page);
    assert!(v.contains("\"b\":\"eval\""), "{v}");
    assert!(v.contains("\"b\":\"wasm-eval\""), "{v}");
    assert!(v.contains("\"sample\":\"1\""), "report-sample: {v}");
    // 'unsafe-eval' in a report-only policy reports and runs.
    let server = Server::start(vec![("/", {
        let mut r = html(&format!(
            "{RECORD}<script nonce=rec>window.e = eval('2');</script>"
        ));
        r.headers.push((
            "Content-Security-Policy-Report-Only".into(),
            "script-src 'nonce-rec'".into(),
        ));
        r
    })]);
    let mut page2 = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    load(&mut page2, server.url("/"));
    assert_eq!(eval(&mut page2, "String(window.e)"), "2");
    let v = violations(&mut page2);
    assert!(v.contains("\"b\":\"eval\",\"disp\":\"report\""), "{v}");
}

#[test]
fn report_only_policies_report_without_blocking_and_report_to_uses_the_endpoint() {
    let mut doc = html(&format!("{RECORD}<script src=/s.js></script>"));
    doc.headers.push((
        "Content-Security-Policy-Report-Only".into(),
        "script-src 'nonce-rec'; report-to csp; report-uri /ignored".into(),
    ));
    doc.headers
        .push(("Reporting-Endpoints".into(), "csp=\"/reports\"".into()));
    let server = Server::start(vec![
        ("/", doc),
        ("/s.js", resource("text/javascript", "window.ran = 1")),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(server.hits("/s.js"), 1);
    assert_eq!(eval(&mut page, "String(window.ran)"), "1");
    let v = violations(&mut page);
    assert!(v.contains("\"disp\":\"report\""), "{v}");
    assert!(server.posts("/ignored").is_empty(), "report-to wins");
    let reports = server.posts("/reports");
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].0, "application/reports+json");
    let body: serde_json::Value = serde_json::from_str(&reports[0].1).unwrap();
    assert_eq!(body[0]["type"], "csp-violation");
    assert_eq!(body[0]["body"]["disposition"], "report");
    assert_eq!(body[0]["body"]["blockedURL"], server.url("/s.js").as_str());
    let console = page.console_messages();
    assert!(
        console
            .iter()
            .any(|(_, m)| m.starts_with("[Report Only] Refused")),
        "{console:?}"
    );
}

#[test]
fn a_meta_policy_governs_only_what_follows_it() {
    let server = Server::start(vec![
        (
            "/",
            html(&format!(
                "<script src=/before.js></script>\
                 <meta http-equiv=Content-Security-Policy content=\"script-src 'nonce-rec'; report-uri /no\">\
                 {RECORD}<script src=/after.js></script>"
            )),
        ),
        ("/before.js", resource("text/javascript", "")),
        ("/after.js", resource("text/javascript", "")),
    ]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(server.hits("/before.js"), 1);
    assert_eq!(server.hits("/after.js"), 0);
    // report-uri is ignored in a meta policy.
    assert!(server.posts("/no").is_empty());
    let v = violations(&mut page);
    assert!(v.contains("after.js"), "{v}");
}

#[test]
fn sandbox_turns_scripts_off_and_gives_an_opaque_origin() {
    let server = Server::start(vec![
        (
            "/",
            html("<iframe src=/off.html></iframe><iframe src=/scripts.html></iframe><iframe src=/forms.html></iframe>"),
        ),
        (
            "/off.html",
            with_csp(html("<script>parent.off = 1</script>"), "sandbox"),
        ),
        (
            "/scripts.html",
            with_csp(
                html("<script>fetch('/origin-' + origin + '-' + document.domain)</script>"),
                "sandbox allow-scripts",
            ),
        ),
        (
            "/forms.html",
            with_csp(
                html("<form action=/submitted></form><script>document.forms[0].submit()</script>"),
                "sandbox allow-scripts",
            ),
        ),
    ]);
    let net = net();
    let mut page = page(&net, false);
    page.load_html("<p>top", &server.url("/blank"));
    eval(
        &mut page,
        &format!(
            "for (const p of ['/off.html', '/scripts.html', '/forms.html']) {{ const f = document.createElement('iframe'); f.src = '{}' + p; document.body.append(f); }}",
            server.origin()
        ),
    );
    settle(&mut page);
    assert_eq!(eval(&mut page, "String(window.off)"), "undefined");
    assert_eq!(
        server.hits("/origin-null-"),
        1,
        "an opaque origin: {:?}",
        server.hits.lock().unwrap()
    );
    assert_eq!(server.hits("/submitted"), 0, "no allow-forms");
    // Sandboxed frames are cross-origin to their parent.
    assert_eq!(
        eval(
            &mut page,
            "Array.from(document.querySelectorAll('iframe'), f => f.contentDocument === null).join()"
        ),
        "true,true,true"
    );
}

#[test]
fn upgrade_insecure_requests_and_block_all_mixed_content() {
    // Over http the upgrade goes to https, which this server does not speak: nothing arrives in
    // plain http.
    let other = Server::start(vec![("/i.js", resource("text/javascript", "window.i = 1"))]);
    let server = Server::start(vec![(
        "/",
        with_csp(
            html(&format!("<script src={}/i.js></script>", other.origin())),
            "upgrade-insecure-requests",
        ),
    )]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(other.hits("/i.js"), 0);
    assert_eq!(eval(&mut page, "String(window.i)"), "undefined");
}

#[test]
fn nonces_are_hidden_from_the_dom_under_a_header_policy() {
    let server = Server::start(vec![(
        "/",
        with_csp(
            html("<script nonce=abc id=s>window.ran = 1</script>"),
            "script-src 'nonce-abc'",
        ),
    )]);
    let net = net();
    let mut page = page(&net, false);
    load(&mut page, server.url("/"));
    assert_eq!(eval(&mut page, "String(window.ran)"), "1");
    assert_eq!(
        eval(
            &mut page,
            "[s.getAttribute('nonce'), s.nonce, document.querySelector('[nonce=abc]')].join()"
        ),
        ",abc,"
    );
}

/// The `nonce` IDL attribute sets the element's `[[CryptographicNonce]]` (HTML), which is what CSP's
/// inline check reads ("Is element nonceable?", "Does a source list allow all inline behavior"): a
/// `<style>` made by script with only that set gets its style sheet under a nonce-only policy, and its
/// clone keeps the slot (the nonce's cloning steps). Without the nonce it has none, and the
/// content attribute alone still works.
#[test]
fn a_nonce_set_through_the_idl_attribute_lets_a_style_block_apply() {
    let server = Server::start(vec![(
        "/",
        with_csp(html("<p id=p>x</p>"), "style-src 'nonce-abc'"),
    )]);
    let net = net();
    let mut page = page(&net, true);
    load(&mut page, server.url("/"));
    assert_eq!(
        eval(
            &mut page,
            "const make = (how, css) => {
               const s = document.createElement('style');
               if (how == 'idl') s.nonce = 'abc';
               if (how == 'attr') s.setAttribute('nonce', 'abc');
               s.textContent = css;
               return s;
             };
             const add = s => (document.head.appendChild(s), s.sheet ? 'sheet' : 'null');
             const idl = make('idl', '#p { color: rgb(0, 128, 0) }');
             const out = [add(make('none', '')), add(idl), add(make('attr', '')),
                          idl.getAttribute('nonce'), getComputedStyle(p).color];
             const copy = idl.cloneNode(true);
             idl.remove();
             out.push(add(copy), copy.nonce);
             out.join()"
        ),
        "null,sheet,sheet,,rgb(0, 128, 0),sheet,abc"
    );
}

#[test]
fn a_document_without_a_policy_reports_nothing() {
    let server = Server::start(vec![
        (
            "/",
            html(
                "<script src=/a.js></script><script>eval('window.e = 1')</script><img src=/i.png>",
            ),
        ),
        ("/a.js", resource("text/javascript", "window.a = 1")),
    ]);
    let net = net();
    let mut page = page(&net, true);
    load(&mut page, server.url("/"));
    assert_eq!(eval(&mut page, "[a, e].join()"), "1,1");
    assert!(page
        .console_messages()
        .iter()
        .all(|(_, m)| !m.contains("Content Security Policy")));
}
