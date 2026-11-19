// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §7.4.1 session history across documents: a navigable's entries with steps, the joint
//! session history of a traversable (`history.length` counts a frame's entries), traversal that
//! loads an entry's document again, "replace" for `location.replace()` and while a document
//! loads; and the `Location` setters' steps (HTML §7.2.4).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions};

/// A tiny HTTP/1.1 server answering `/<name>` (query ignored) from `files` (path, body).
fn serve(files: Vec<(&'static str, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
            }
            let target = first.split(' ').nth(1).unwrap_or("/");
            let path = target.split(['?', '#']).next().unwrap_or("/").to_owned();
            let response = match files.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .into(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

fn wait(page: &mut Page, js: &str, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        page.run_for(Duration::from_millis(20));
        if page.eval(js, "wait.js").is_ok_and(|v| v == want) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(eval(page, js), want, "{js}");
}

fn page_at(base: &str, path: &str) -> (NetThread, Page) {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}{path}")).unwrap());
    (net, page)
}

const FRAME_PAGE: &str = "<!doctype html><script>parent.postMessage(location.pathname + \
     location.search, '*'); window.v = Math.random()</script>";

fn frame_site() -> String {
    serve(vec![
        (
            "/top.html",
            "<!doctype html><iframe id=f src='/a.html'></iframe><script>window.seen = []; \
             addEventListener('message', e => seen.push(e.data)); \
             addEventListener('load', () => window.loaded = 1)</script>"
                .into(),
        ),
        ("/a.html", FRAME_PAGE.into()),
        ("/b.html", FRAME_PAGE.into()),
        ("/c.html", FRAME_PAGE.into()),
    ])
}

/// A frame's navigation after its document loaded adds an entry to the top-level traversable's
/// joint session history; `history.back()` from the top loads the frame's earlier document again,
/// and `forward()` the later one, without changing the top-level document.
#[test]
fn frame_navigations_are_joint_session_history() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    assert_eq!(eval(&mut page, "history.length"), "1");
    eval(&mut page, "f.contentWindow.location.href = '/b.html'");
    wait(&mut page, "seen.join()", "/a.html,/b.html");
    assert_eq!(eval(&mut page, "history.length"), "2");
    eval(&mut page, "history.back()");
    wait(&mut page, "seen.join()", "/a.html,/b.html,/a.html");
    assert_eq!(
        eval(
            &mut page,
            "[history.length, location.pathname, f.contentWindow.location.pathname].join()"
        ),
        "2,/top.html,/a.html"
    );
    eval(&mut page, "history.forward()");
    wait(&mut page, "seen.join()", "/a.html,/b.html,/a.html,/b.html");
    // A new entry after going back drops the ones after the current step.
    eval(&mut page, "history.back()");
    wait(&mut page, "seen.length", "5");
    eval(&mut page, "f.contentWindow.location.href = '/c.html'");
    wait(
        &mut page,
        "seen.join()",
        "/a.html,/b.html,/a.html,/b.html,/a.html,/c.html",
    );
    assert_eq!(eval(&mut page, "history.length"), "2");
    // Nothing past the ends.
    eval(&mut page, "history.go(5)");
    page.run_for(Duration::from_millis(200));
    assert_eq!(eval(&mut page, "seen.length"), "6");
}

/// Of traversals asked for in one task, the first that moves wins: the later ones wait for its
/// document, and their own document is gone by then.
#[test]
fn traversals_in_one_task_first_one_wins() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    eval(&mut page, "f.contentWindow.location.href = '/b.html'");
    wait(&mut page, "seen.length", "2");
    eval(&mut page, "f.contentWindow.location.href = '/c.html'");
    wait(&mut page, "seen.length", "3");
    eval(
        &mut page,
        "const h = f.contentWindow.history; h.go(-10); h.go(-2); h.go(1)",
    );
    wait(&mut page, "seen.join()", "/a.html,/b.html,/c.html,/a.html");
    page.run_for(Duration::from_millis(300));
    assert_eq!(
        eval(&mut page, "seen.join()"),
        "/a.html,/b.html,/c.html,/a.html"
    );
}

/// A traversal that would change an entry of a navigable its document may not navigate (a
/// sandboxed frame going back over its parent's entry) does nothing ("initiator-disallowed").
#[test]
fn a_sandboxed_frame_cannot_traverse_its_parent() {
    let base = serve(vec![(
        "/top.html",
        "<!doctype html><iframe sandbox=allow-scripts \
         srcdoc=\"<script>onmessage = () => history.back()</script>\"></iframe>\
         <script>addEventListener('load', () => window.loaded = 1)</script>"
            .into(),
    )]);
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1", "true");
    eval(
        &mut page,
        "window.pops = 0; onpopstate = () => pops++; history.pushState(1, '', '?p'); \
         frames[0].postMessage('back', '*')",
    );
    page.run_for(Duration::from_millis(300));
    assert_eq!(
        eval(&mut page, "[pops, history.state, location.search].join()"),
        "0,1,?p"
    );
    // (the page itself may)
    eval(&mut page, "history.back()");
    wait(&mut page, "pops", "1");
}

/// The embedder's navigation (a test runner's next page) starts a session history of its own.
#[test]
fn the_embedders_navigation_starts_a_new_session_history() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    eval(&mut page, "history.pushState(null, '', '?x')");
    assert_eq!(eval(&mut page, "history.length"), "2");
    page.navigate(url::Url::parse(&format!("{base}/top.html?again")).unwrap());
    wait(&mut page, "window.loaded == 1 && location.search", "?again");
    assert_eq!(eval(&mut page, "history.length"), "1");
}

/// `location.replace()` replaces the frame's entry: no new step.
#[test]
fn location_replace_takes_the_current_entry() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    eval(&mut page, "f.contentWindow.location.href = '/b.html'");
    wait(&mut page, "seen.join()", "/a.html,/b.html");
    eval(&mut page, "f.contentWindow.location.replace('/c.html')");
    wait(&mut page, "seen.join()", "/a.html,/b.html,/c.html");
    assert_eq!(eval(&mut page, "history.length"), "2");
    // (back from c is a: b's entry is c's now)
    eval(&mut page, "history.back()");
    wait(&mut page, "seen.join()", "/a.html,/b.html,/c.html,/a.html");
}

/// Same-document entries and cross-document ones mix in one traversable: `back()` over a frame's
/// pushState entry fires `popstate` in it with the entry's state; `history.state` is one object.
#[test]
fn push_state_in_a_frame_and_traversal_back() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    eval(
        &mut page,
        "const w = f.contentWindow; w.pops = []; \
         w.addEventListener('popstate', e => w.pops.push(JSON.stringify(e.state) + w.location.search)); \
         w.history.pushState({n: 1}, '', '?one'); w.history.pushState({n: 2}, '', '?two')",
    );
    assert_eq!(
        eval(&mut page, "[history.length, f.contentWindow.history.state.n, f.contentWindow.history.state === f.contentWindow.history.state].join()"),
        "3,2,true"
    );
    eval(&mut page, "history.back()");
    wait(&mut page, "f.contentWindow.pops.join()", "{\"n\":1}?one");
    eval(&mut page, "history.go(-1)");
    wait(
        &mut page,
        "f.contentWindow.pops.join()",
        "{\"n\":1}?one,null",
    );
    assert_eq!(
        eval(&mut page, "f.contentWindow.location.pathname + seen.length"),
        "/a.html1"
    );
}

/// HTML §7.2.4 the `protocol` setter: a value that is not a scheme throws "SyntaxError".
#[test]
fn location_protocol_setter_throws_for_what_is_not_a_scheme() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    let r = eval(
        &mut page,
        "['\\x00', '\\t', ':', '\\x20x', 'a b', '\\u2020', '1x'].map(v => { \
           try { location.protocol = v; return 'none' } catch (e) { return e.name } }).join()",
    );
    assert_eq!(r, ["SyntaxError"; 7].join(","));
}

/// The `hash` setter with "" leaves an empty fragment; a Location whose browsing context is gone
/// ignores its setters (none throws); Location's prototype cannot change.
#[test]
fn location_hash_empty_and_setters_without_a_browsing_context() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    let r = eval(
        &mut page,
        "location.hash = 'test'; const a = location.hash; location.hash = ''; \
         [a, location.hash, location.href.endsWith('#')].join()",
    );
    assert_eq!(r, "#test,,true");
    let r = eval(
        &mut page,
        "const loc = f.contentWindow.location; f.remove(); \
         loc.href = 'http://test:test/'; loc.hash = 'x'; loc.assign('http://test:test/'); \
         loc.replace('http://test:test/'); loc.search = 'q'; \
         [loc.href, loc.hash].join()",
    );
    assert_eq!(r, "about:blank,");
    let r = eval(
        &mut page,
        "let threw = false; try { Object.setPrototypeOf(location, {}) } catch (e) { threw = e instanceof TypeError } \
         [threw, Reflect.setPrototypeOf(location, Object.getPrototypeOf(location)), Object.getPrototypeOf(location) === Location.prototype].join()",
    );
    assert_eq!(r, "true,true,true");
}

/// HTML §3.1.3 ancestor origins: an iframe's `referrerpolicy` when its document was made masks
/// the parent's origin, and the run of ancestors right above it with the same origin; a later
/// change of the attribute counts only for the next navigation.
#[test]
fn ancestor_origins_masked_by_the_iframe_referrer_policy() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    let origin = eval(&mut page, "location.origin");
    assert_eq!(
        eval(
            &mut page,
            "Array.from(f.contentWindow.location.ancestorOrigins).join()"
        ),
        origin
    );
    // (a frame inside the frame, masked: its parent's origin and the same one above it)
    let r = eval(
        &mut page,
        "const g = f.contentDocument.createElement('iframe'); g.referrerPolicy = 'no-referrer'; \
         f.contentDocument.body.append(g); \
         const before = Array.from(g.contentWindow.location.ancestorOrigins).join(); \
         g.referrerPolicy = ''; \
         [before, Array.from(g.contentWindow.location.ancestorOrigins).join()].join('|')",
    );
    assert_eq!(r, "null,null|null,null");
    // ("same-origin" masks only a cross-origin parent)
    let r = eval(
        &mut page,
        "const h = document.createElement('iframe'); h.referrerPolicy = 'same-origin'; \
         document.body.append(h); Array.from(h.contentWindow.location.ancestorOrigins).join()",
    );
    assert_eq!(r, origin);
    // (the attribute's value when the next navigation began)
    eval(
        &mut page,
        "g.referrerPolicy = ''; g.onload = () => window.gl = 1; g.src = '/b.html'",
    );
    wait(&mut page, "window.gl", "1");
    assert_eq!(
        eval(
            &mut page,
            "Array.from(g.contentWindow.location.ancestorOrigins).join()"
        ),
        format!("{origin},{origin}")
    );
}

/// HTML §7.2.4 `ancestorOrigins` without a relevant Document is the Location's empty list, another
/// object than the Document's.
#[test]
fn ancestor_origins_without_a_relevant_document() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    let r = eval(
        &mut page,
        "const loc = f.contentWindow.location; const a = loc.ancestorOrigins; \
         const same = a === loc.ancestorOrigins; f.remove(); const b = loc.ancestorOrigins; \
         [same, a.length, b.length, a !== b, b === loc.ancestorOrigins].join()",
    );
    assert_eq!(r, "true,1,0,true,true");
}

/// HTML §7.2.5: History's members throw "SecurityError" once its document is not fully active,
/// and the History object is the Document's: a new one for the document that replaced the
/// initial `about:blank` one in its Window.
#[test]
fn history_needs_a_fully_active_document_and_is_per_document() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    let r = eval(
        &mut page,
        "const h = f.contentWindow.history; f.remove(); \
         ['length', 'scrollRestoration', 'state'].map(k => { try { h[k]; return 'none' } \
           catch (e) { return e.name } }).concat([() => h.go(0), () => h.back(), \
           () => h.pushState(null, '')].map(fn => { try { fn(); return 'none' } \
           catch (e) { return e.name } })).join()",
    );
    assert_eq!(r, ["SecurityError"; 6].join(","));
    eval(
        &mut page,
        "const k = document.createElement('iframe'); document.body.append(k); \
         window.initial = k.contentWindow.history; window.kw = k.contentWindow; \
         k.onload = () => window.kl = 1; k.src = '/b.html'",
    );
    wait(&mut page, "window.kl", "1");
    assert_eq!(
        eval(
            &mut page,
            "[kw === k.contentWindow, initial !== kw.history].join()"
        ),
        "true,true"
    );
}

/// HTML §2.7.3: an ImageData in a History state is serialized (its data, width and height) and
/// comes back as a new ImageData.
#[test]
fn push_state_clones_image_data() {
    let base = frame_site();
    let (_net, mut page) = page_at(&base, "/top.html");
    wait(&mut page, "window.loaded == 1 && seen.join()", "/a.html");
    let r = eval(
        &mut page,
        "const d = new ImageData(2, 1); d.data[0] = 7; d.data[7] = 9; \
         history.replaceState({d}, ''); const s = history.state.d; \
         [s instanceof ImageData, s !== d, s.width, s.height, s.data[0], s.data[7]].join()",
    );
    assert_eq!(r, "true,true,2,1,7,9");
}
