// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How a page opens others (HTML §7.1.3, §7.3.1.7, §7.2.2.1): Cross-Origin-Opener-Policy's browsing
//! context group switch, sandbox flags propagated to auxiliary browsing contexts, the `blob:` URL
//! check of "get noopener for window open", and a navigation's own referrer policy. Documents come
//! from in-process servers that send the headers each test names.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use snowlark_net::config::HostRule;
use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions};

/// A served file: its path, extra header lines ("" for none; a `Location` first makes it a 302) and
/// body.
type File = (&'static str, &'static str, String);

/// A tiny HTTP/1.1 server answering each path of `files` with its headers and body.
fn serve(files: Vec<File>) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
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
            let path = target.split('?').next().unwrap_or("/");
            let response = match files.iter().find(|(p, _, _)| *p == path) {
                Some((_, headers, body)) => format!(
                    "HTTP/1.1 {}\r\nContent-Type: text/html\r\n{headers}Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    if headers.starts_with("Location:") {
                        "302 Found"
                    } else {
                        "200 OK"
                    },
                    body.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .into(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
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

fn settle(page: &mut Page, ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        page.run_for(Duration::from_millis(20));
    }
}

/// What every top-level page below does first: messages to it land in `got`.
const TOP_SCRIPT: &str = "<script>
    window.got = [];
    addEventListener('message', e => got.push(e.data));
    addEventListener('load', () => window.allLoaded = true);
    </script>";

const COOP_SAME_ORIGIN: &str = "Cross-Origin-Opener-Policy: same-origin\r\n";

/// The network, a page loaded from `/top.html` of a server with `files` (and `top_headers` and
/// `top_body` for the top page), and the server's origin.
struct Site {
    _net: NetThread,
    page: Page,
    origin: String,
}

fn site(top_headers: &'static str, top_body: &str, files: Vec<File>) -> Site {
    site_on("127.0.0.1", top_headers, top_body, files)
}

/// [`site`] with the top page loaded from `host` (`*.example.test` names this machine).
fn site_on(host: &str, top_headers: &'static str, top_body: &str, mut files: Vec<File>) -> Site {
    let config = NetConfig {
        host_rules: HostRule::parse_list("MAP *.example.test 127.0.0.1"),
        ..Default::default()
    };
    let net = NetThread::start(config).unwrap();
    files.push((
        "/top.html",
        top_headers,
        format!("<!doctype html>{TOP_SCRIPT}<body>{top_body}"),
    ));
    let origin = format!("http://{host}:{}", serve(files));
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{origin}/top.html")).unwrap());
    wait(&mut page, "String(window.allLoaded)", "true");
    Site {
        _net: net,
        page,
        origin,
    }
}

/// A popup that says through a cookie whether it has an opener, and its name, every 20 ms.
fn reporting_popup() -> String {
    "<!doctype html><script>
     setInterval(() => document.cookie = 'popup=' + (opener === null ? 'no-opener' : 'opener') +
       '-' + (name || 'unnamed'), 20);
     </script>"
        .into()
}

fn cookie(page: &mut Page, name: &str) -> String {
    eval(
        page,
        &format!(
            "(document.cookie.split('; ').find(c => c.startsWith('{name}=')) || '').slice({})",
            name.len() + 1
        ),
    )
}

#[test]
fn a_popup_whose_response_does_not_match_the_opener_policy_is_cut_off() {
    // HTML "checking if popup COOP values require a browsing context group switch": the popup's
    // initial about:blank took its opener's `same-origin`; a response without the policy needs a
    // group switch, so the popup has no opener and no name, and the opener's WindowProxy is closed.
    let mut s = site(
        COOP_SAME_ORIGIN,
        "",
        vec![("/popup.html", "", reporting_popup())],
    );
    let page = &mut s.page;
    eval(page, "window.w = open('/popup.html', 'pop'); 0");
    wait(page, "String(document.cookie.includes('popup='))", "true");
    settle(page, 100);
    assert_eq!(cookie(page, "popup"), "no-opener-unnamed");
    assert_eq!(eval(page, "String(w.closed)"), "true");
    assert_eq!(eval(page, "String(w.length)"), "0");
    // (its name is no target for this group any more: a new window opens)
    assert_eq!(eval(page, "String(open('', 'pop') === w)"), "false");
}

#[test]
fn every_redirect_of_a_popups_navigation_is_checked_against_the_opener_policy() {
    // HTML "create navigation params by fetching" enforces the opener policy of each response: a
    // redirect without the policy needs the switch, though the final response matches the opener's.
    let mut s = site(
        COOP_SAME_ORIGIN,
        "",
        vec![
            ("/redirect.html", "Location: /popup.html\r\n", String::new()),
            ("/popup.html", COOP_SAME_ORIGIN, reporting_popup()),
        ],
    );
    let page = &mut s.page;
    eval(page, "window.w = open('/redirect.html', 'pop'); 0");
    wait(page, "String(document.cookie.includes('popup='))", "true");
    settle(page, 100);
    assert_eq!(cookie(page, "popup"), "no-opener-unnamed");
    assert_eq!(eval(page, "String(w.closed)"), "true");
}
#[test]
fn a_popup_with_the_same_opener_policy_and_origin_keeps_its_opener() {
    let mut s = site(
        COOP_SAME_ORIGIN,
        "",
        vec![("/popup.html", COOP_SAME_ORIGIN, reporting_popup())],
    );
    let page = &mut s.page;
    eval(page, "window.w = open('/popup.html', 'pop'); 0");
    wait(page, "String(document.cookie.includes('popup='))", "true");
    settle(page, 100);
    assert_eq!(cookie(page, "popup"), "opener-pop");
    assert_eq!(eval(page, "String(w.closed)"), "false");
}

#[test]
fn a_response_sandboxed_by_its_own_csp_loads_in_a_group_of_its_own() {
    // The response's CSP `sandbox` makes its origin opaque, which matches no policy: a group switch,
    // not a network error (that is for a navigation whose own sandboxing flags are not empty).
    let mut s = site(
        COOP_SAME_ORIGIN,
        "",
        vec![(
            "/sandboxed.html",
            "Cross-Origin-Opener-Policy: same-origin\r\n\
             Content-Security-Policy: sandbox allow-scripts\r\n",
            "<!doctype html><p>sandboxed".into(),
        )],
    );
    let page = &mut s.page;
    eval(page, "window.w = open('/sandboxed.html', 'pop'); 0");
    wait(page, "String(w.closed)", "true");
}
#[test]
fn a_javascript_url_document_keeps_the_opener_policy() {
    // HTML "evaluate a `javascript:` URL": the new document's navigation params take the active
    // document's opener policy, so a popup it opens takes `same-origin` and keeps its opener when
    // its own response has the same policy.
    let mut s = site(
        COOP_SAME_ORIGIN,
        "",
        vec![
            (
                "/subject.html",
                COOP_SAME_ORIGIN,
                "<!doctype html><script>window.loaded = true</script>".into(),
            ),
            ("/popup.html", COOP_SAME_ORIGIN, reporting_popup()),
        ],
    );
    let page = &mut s.page;
    eval(page, "window.w = open('/subject.html', 'subject'); 0");
    wait(page, "String(!!(w.loaded))", "true");
    eval(
        page,
        "w.location = \"javascript:'<script>open(\\\"/popup.html\\\", \\\"pop\\\")<\\/script>'\"; 0",
    );
    wait(page, "String(document.cookie.includes('popup='))", "true");
    settle(page, 100);
    assert_eq!(cookie(page, "popup"), "opener-pop");
}
#[test]
fn an_opener_that_navigates_to_another_opener_policy_loses_its_popups() {
    // The page's own top-level traversable switches groups: the popup it opened has no opener.
    let mut s = site(
        "",
        "",
        vec![
            ("/popup.html", "", reporting_popup()),
            (
                "/coop.html",
                COOP_SAME_ORIGIN,
                "<!doctype html><script>window.arrived = true</script>".into(),
            ),
        ],
    );
    let origin = s.origin.clone();
    let page = &mut s.page;
    eval(page, "window.w = open('/popup.html', 'pop'); 0");
    wait(
        page,
        "String(document.cookie.includes('popup=opener'))",
        "true",
    );
    page.navigate(url::Url::parse(&format!("{origin}/coop.html")).unwrap());
    wait(page, "String(window.arrived)", "true");
    settle(page, 100);
    assert_eq!(cookie(page, "popup"), "no-opener-pop");
}

#[test]
fn a_cross_origin_frame_of_a_same_origin_policy_page_opens_with_no_opener() {
    // HTML "the rules for choosing a navigable": a document whose (top-level) opener policy is
    // `same-origin` and whose origin is not the top-level origin opens a window with no opener.
    let other = serve(vec![(
        "/frame.html",
        "",
        "<!doctype html><script>parent.postMessage(String(open('about:blank')), '*')</script>"
            .into(),
    )]);
    let mut s = site(
        COOP_SAME_ORIGIN,
        &format!("<iframe src='http://127.0.0.1:{other}/frame.html'></iframe>"),
        vec![],
    );
    wait(&mut s.page, "got.join()", "null");
}

/// A page with a sandboxed frame (`sandbox` its flags) that opens `/popup.html`, which posts its
/// origin to its opener; the frame forwards what it hears to the top.
fn sandboxed_opener(sandbox: &str, popup_headers: &'static str) -> Site {
    site(
        "",
        &format!("<iframe sandbox='{sandbox}' src='/frame.html'></iframe>"),
        vec![
            (
                "/frame.html",
                "",
                "<!doctype html><script>
                 onmessage = e => parent.postMessage(e.data, '*');
                 window.w = open('/popup.html');
                 parent.postMessage('opened ' + (w !== null), '*');
                 </script>"
                    .into(),
            ),
            (
                "/popup.html",
                popup_headers,
                "<!doctype html><script>opener.postMessage('origin ' + origin, '*')</script>"
                    .into(),
            ),
            (
                "/plain.html",
                "",
                "<!doctype html><script>opener.postMessage('plain', '*')</script>".into(),
            ),
        ],
    )
}

#[test]
fn a_sandboxed_documents_popup_inherits_its_sandbox() {
    // "sandbox propagates to auxiliary browsing contexts": without `allow-same-origin` the popup's
    // document has an opaque origin.
    let mut s = sandboxed_opener("allow-scripts allow-popups", "");
    wait(&mut s.page, "got.join()", "opened true,origin null");
}

#[test]
fn allow_popups_to_escape_sandbox_leaves_the_popup_unsandboxed() {
    let mut s = sandboxed_opener(
        "allow-scripts allow-popups allow-popups-to-escape-sandbox",
        "",
    );
    let origin = s.origin.clone();
    wait(
        &mut s.page,
        "got.join()",
        &format!("opened true,origin {origin}"),
    );
}

#[test]
fn a_sandboxed_popup_may_not_load_a_document_with_an_opener_policy() {
    // "create navigation params by fetching": a non-empty sandboxing flag set with an opener policy
    // other than unsafe-none is a network error. The popup's page never runs; a second popup to a
    // page without the policy does.
    let mut s = sandboxed_opener(
        "allow-scripts allow-popups allow-same-origin",
        COOP_SAME_ORIGIN,
    );
    let page = &mut s.page;
    wait(page, "got.join()", "opened true");
    eval(
        page,
        "document.querySelector('iframe').contentWindow.open('/plain.html'); 0",
    );
    wait(page, "String(got.includes('plain'))", "true");
    settle(page, 200);
    assert_eq!(eval(page, "got.join()"), "opened true,plain");
    // (and the popup is cut off from its opener)
    assert_eq!(
        eval(
            page,
            "String(document.querySelector('iframe').contentWindow.w.closed)"
        ),
        "true"
    );
}

#[test]
fn window_open_of_a_cross_site_blob_url_has_no_opener() {
    // HTML "get noopener for window open": the blob URL's entry is of another site than the top.
    let other = serve(vec![(
        "/frame.html",
        "",
        "<!doctype html><script>
         parent.postMessage(URL.createObjectURL(new Blob(['<p>b</p>'], {type: 'text/html'})), '*');
         </script>"
            .into(),
    )]);
    // (a frame of another site: `example.test` and `127.0.0.1`)
    let mut s = site_on(
        "a.example.test",
        "",
        &format!("<iframe src='http://127.0.0.1:{other}/frame.html'></iframe>"),
        vec![],
    );
    let page = &mut s.page;
    wait(page, "String(got.length)", "1");
    assert_eq!(eval(page, "String(open(got[0]))"), "null");
    // (a same-site one keeps the opener)
    assert_eq!(
        eval(
            page,
            "String(open(URL.createObjectURL(new Blob(['x'], {type: 'text/html'}))) !== null)"
        ),
        "true"
    );
}

#[test]
fn a_links_referrer_policy_rules_its_navigation() {
    // HTML "follow the hyperlink": the `referrerpolicy` attribute is the navigation's referrer
    // policy, and `rel=noreferrer` makes it "no-referrer"; `window.open()`'s `noreferrer` too.
    let mut s = site(
        "",
        "<iframe name=f></iframe>
         <a id=none target=f href='/r.html?1' referrerpolicy=no-referrer>x</a>
         <a id=origin target=f href='/r.html?2' referrerpolicy=origin>x</a>
         <a id=rel target=f href='/r.html?3' rel=noreferrer>x</a>
         <a id=plain target=f href='/r.html?4'>x</a>",
        vec![(
            "/r.html",
            "",
            "<!doctype html><script>
             (parent !== window ? parent : opener || parent)
               .postMessage(location.search + ' ' + document.referrer, '*');
             </script>"
                .into(),
        )],
    );
    let origin = s.origin.clone();
    let page = &mut s.page;
    for (id, want) in [
        ("none", "?1 ".to_owned()),
        ("origin", format!("?2 {origin}/")),
        ("rel", "?3 ".to_owned()),
        ("plain", format!("?4 {origin}/top.html")),
    ] {
        eval(
            page,
            &format!("got = []; document.getElementById('{id}').click(); 0"),
        );
        wait(page, "got.join()", &want);
    }
}
