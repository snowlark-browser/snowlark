// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Navigation in the browser (`docs/process-isolation.md`, M5): with `Engine::set_browser_navigation`
//! the engine hands each top-level navigation to the embedder (which fetches it as the browser does)
//! and commits the response it is handed back; a document's own history entries and `history.go()`
//! go to the embedder too. The test plays the browser.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snowlark_net::{FetchEvent, NetConfig, NetThread, ResponseHead, ResponseStream};
use snowlark_page::embedder::SameDocumentChange;
use snowlark_page::{CommitParams, CommitResponse, Page, PageOptions};

/// An HTTP/1.1 server over a path → (type, body) map, counting the requests per path and
/// keeping each POST body.
struct Server {
    addr: std::net::SocketAddr,
    hits: Arc<Mutex<HashMap<String, usize>>>,
    posts: Arc<Mutex<Vec<(String, String)>>>,
}

impl Server {
    fn start(resources: Vec<(&str, &str, &str)>) -> Server {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let resources: HashMap<String, (String, String)> = resources
            .into_iter()
            .map(|(p, t, b)| (p.to_owned(), (t.to_owned(), b.to_owned())))
            .collect();
        let resources = Arc::new(resources);
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let posts: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let (h, p) = (hits.clone(), posts.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (res, h, p) = (resources.clone(), h.clone(), p.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut first = String::new();
                        if reader.read_line(&mut first).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut length = 0;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                                break;
                            }
                            if let Some(v) =
                                line.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                length = v.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0; length];
                        if length > 0 {
                            reader.read_exact(&mut body).unwrap();
                        }
                        let method = first.split_whitespace().next().unwrap_or("").to_owned();
                        let path = first.split_whitespace().nth(1).unwrap_or("/").to_owned();
                        *h.lock().unwrap().entry(path.clone()).or_default() += 1;
                        if method == "POST" {
                            p.lock()
                                .unwrap()
                                .push((path.clone(), String::from_utf8_lossy(&body).into_owned()));
                        }
                        let answer = match res.get(&path) {
                            Some((mime, body)) => format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            ),
                            None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".into(),
                        };
                        if stream.write_all(answer.as_bytes()).is_err() {
                            return;
                        }
                        let _ = stream.flush();
                    }
                });
            }
        });
        Server { addr, hits, posts }
    }

    fn url(&self, path: &str) -> url::Url {
        url::Url::parse(&format!("http://{}{path}", self.addr)).unwrap()
    }

    fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

fn page(net: &NetThread) -> Page {
    let page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        ..Default::default()
    });
    page.engine().set_browser_navigation(true);
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Runs the page until the document at `href` shows and is complete (`Page::load_complete` says
/// no `about:blank` is, as the initial one never loads; a navigated-to one does, so the
/// readiness is read).
fn settle(page: &mut Page, href: &str) {
    let start = Instant::now();
    loop {
        page.run_for(Duration::from_millis(10));
        if eval(page, "location.href + ' ' + document.readyState") == format!("{href} complete") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the page at {href} loads"
        );
    }
    page.run_for(Duration::from_millis(20));
}

/// What the page asked the browser to begin: exactly one navigation.
fn begun(page: &mut Page) -> snowlark_page::embedder::BeginNavigation {
    page.run_for(Duration::from_millis(20));
    let mut list = page.engine().take_begun_navigations();
    assert_eq!(list.len(), 1, "{list:?}");
    list.remove(0)
}

/// The browser's part: fetches the request and waits for its head.
fn fetch_head(net: &NetThread, request: snowlark_net::Request) -> (ResponseHead, ResponseStream) {
    let mut stream = net.client().fetch(request);
    loop {
        match stream.next_blocking() {
            Some(FetchEvent::Head(h)) => return (h, stream),
            Some(FetchEvent::Violation(_)) => {}
            other => panic!("no head: {other:?}"),
        }
    }
}

/// Begins `url` as the user, fetches it as the browser and commits it.
fn load_as_browser(page: &mut Page, net: &NetThread, url: url::Url) {
    page.navigate(url.clone());
    let begin = begun(page);
    assert_eq!(begin.params.url, url);
    assert!(begin.params.initiator.is_none() && begin.params.user_activation);
    let (head, stream) = fetch_head(net, begin.params.into_request());
    page.commit_navigation(
        CommitParams {
            url: url.clone(),
            begun: Some(begin.seq),
            reload: false,
            response: CommitResponse::Fetched {
                head: Box::new(head),
            },
            violations: Vec::new(),
            started: Some(Instant::now()),
        },
        Some(stream),
    );
    settle(page, url.as_str());
}

#[test]
fn a_top_level_navigation_is_handed_to_the_browser_and_committed() {
    let server = Server::start(vec![
        (
            "/",
            "text/html",
            "<!doctype html><title>one</title><a id=l href=/two>two</a>",
        ),
        (
            "/two",
            "text/html",
            "<title>two</title><form id=f method=post action=/posted><input name=a value=1></form>",
        ),
        ("/posted", "text/html", "<title>posted</title>"),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    // The user's navigation: the engine fetches nothing itself; the browser's fetch commits.
    load_as_browser(&mut page, &net, server.url("/"));
    assert_eq!(eval(&mut page, "document.title"), "one");
    assert_eq!(eval(&mut page, "location.pathname"), "/");
    assert_eq!(server.hits("/"), 1, "the engine fetched the page itself");
    // The page's own: a link click. The browser gets the initiator (origin, referrer) and the
    // request it builds carries it.
    eval(&mut page, "document.getElementById('l').click()");
    let begin = begun(&mut page);
    assert_eq!(begin.params.url, server.url("/two"));
    assert!(begin.seq > 0);
    let initiator = begin.params.initiator.clone().expect("the link's document");
    assert_eq!(initiator.origin, server.url("/").origin());
    assert_eq!(initiator.referrer_source, server.url("/"));
    assert!(!begin.params.form_submission);
    let request = begin.params.into_request();
    assert!(!request.user_initiated && request.client.is_some());
    assert_eq!(request.referrer, snowlark_net::Referrer::Client);
    let (head, stream) = fetch_head(&net, request);
    let sent_referrer = head.referrer.clone();
    page.commit_navigation(
        CommitParams {
            url: server.url("/two"),
            begun: Some(begin.seq),
            reload: false,
            response: CommitResponse::Fetched {
                head: Box::new(head),
            },
            violations: Vec::new(),
            started: Some(Instant::now()),
        },
        Some(stream),
    );
    settle(&mut page, server.url("/two").as_str());
    assert_eq!(eval(&mut page, "document.title"), "two");
    // The committed document's referrer is the one its request was sent with.
    assert_eq!(sent_referrer.as_ref(), Some(&server.url("/")));
    assert_eq!(
        eval(&mut page, "document.referrer"),
        server.url("/").as_str()
    );
    // A form submission: the POST body goes with it.
    eval(&mut page, "document.getElementById('f').submit()");
    let begin = begun(&mut page);
    assert_eq!(begin.params.url, server.url("/posted"));
    assert!(begin.params.form_submission);
    assert_eq!(
        begin
            .params
            .post
            .as_ref()
            .map(|(b, t)| (b.as_slice(), t.as_str())),
        Some((&b"a=1"[..], "application/x-www-form-urlencoded"))
    );
    let (head, stream) = fetch_head(&net, begin.params.into_request());
    page.commit_navigation(
        CommitParams {
            url: server.url("/posted"),
            begun: Some(begin.seq),
            reload: false,
            response: CommitResponse::Fetched {
                head: Box::new(head),
            },
            violations: Vec::new(),
            started: Some(Instant::now()),
        },
        Some(stream),
    );
    settle(&mut page, server.url("/posted").as_str());
    assert_eq!(eval(&mut page, "document.title"), "posted");
    assert_eq!(
        server.posts.lock().unwrap().as_slice(),
        [("/posted".to_owned(), "a=1".to_owned())]
    );
    // Nothing else was begun, and the engine's own fetches stayed at none.
    assert!(page.engine().take_begun_navigations().is_empty());
    assert_eq!((server.hits("/two"), server.hits("/posted")), (1, 1));
}

#[test]
fn a_failed_navigation_commits_an_error_page_and_a_response_with_no_body_a_document() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    let url = url::Url::parse("http://127.0.0.1:1/nothing").unwrap();
    page.navigate(url.clone());
    let begin = begun(&mut page);
    page.commit_navigation(
        CommitParams {
            url: url.clone(),
            begun: Some(begin.seq),
            reload: false,
            response: CommitResponse::Failed(snowlark_net::NetError::new(
                snowlark_net::NetErrorKind::Connect,
                "nobody listens",
            )),
            violations: Vec::new(),
            started: Some(Instant::now()),
        },
        None,
    );
    settle(&mut page, "about:error");
    let doc = page.engine().document().unwrap();
    assert_eq!(page.engine().error_page_url(doc), Some(url));
    assert!(eval(&mut page, "document.body.textContent").contains("refused to connect"));
    // A head with no stream (nothing to read): an empty document at the response's URL.
    let mut head = ResponseHead::new(200, "OK");
    head.url_list = vec![url::Url::parse("http://a.test/empty").unwrap()];
    head.headers.append("Content-Type", "text/html");
    page.navigate(url::Url::parse("http://a.test/empty").unwrap());
    let begin = begun(&mut page);
    page.commit_navigation(
        CommitParams {
            url: url::Url::parse("http://a.test/empty").unwrap(),
            begun: Some(begin.seq),
            reload: false,
            response: CommitResponse::Fetched {
                head: Box::new(head),
            },
            violations: Vec::new(),
            started: None,
        },
        None,
    );
    settle(&mut page, "http://a.test/empty");
    assert_eq!(eval(&mut page, "location.href"), "http://a.test/empty");
    assert_eq!(eval(&mut page, "document.readyState"), "complete");
}

#[test]
fn about_blank_commits_directly_with_its_initiators_origin() {
    let server = Server::start(vec![(
        "/",
        "text/html",
        "<title>one</title><a id=l href='about:blank'>blank</a>",
    )]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load_as_browser(&mut page, &net, server.url("/"));
    eval(&mut page, "document.getElementById('l').click()");
    let begin = begun(&mut page);
    assert_eq!(begin.params.url.as_str(), "about:blank");
    assert!(
        begin.params.initiator.is_some(),
        "the link's document is the initiator"
    );
    page.commit_navigation(
        CommitParams {
            url: begin.params.url.clone(),
            begun: Some(begin.seq),
            reload: false,
            response: CommitResponse::Direct,
            violations: Vec::new(),
            started: None,
        },
        None,
    );
    settle(&mut page, "about:blank");
    assert_eq!(eval(&mut page, "location.href"), "about:blank");
    // HTML "determining the origin": an about:blank document takes its initiator's (its domain
    // is the server's host; an opaque origin's is empty).
    assert_eq!(eval(&mut page, "document.domain"), "127.0.0.1");
    // The user's about:blank (no initiator) is opaque.
    page.navigate(url::Url::parse("about:blank").unwrap());
    let begin = begun(&mut page);
    page.commit_navigation(
        CommitParams {
            url: begin.params.url.clone(),
            begun: None,
            reload: false,
            response: CommitResponse::Direct,
            violations: Vec::new(),
            started: None,
        },
        None,
    );
    settle(&mut page, "about:blank");
    assert_eq!(eval(&mut page, "document.domain"), "");
}

#[test]
fn same_document_changes_and_traversals_go_to_the_browser() {
    let server = Server::start(vec![(
        "/",
        "text/html",
        "<title>one</title><script>onpopstate = e => document.title = 'pop ' + JSON.stringify(e.state); onhashchange = () => document.title += ' hash';</script>",
    )]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    load_as_browser(&mut page, &net, server.url("/"));
    assert!(page.engine().take_same_document_changes().is_empty());
    eval(&mut page, "history.pushState({a: 1}, '', '/p1')");
    eval(&mut page, "history.replaceState({b: 2}, '', '/p2')");
    eval(&mut page, "location.hash = '#x'");
    page.run_for(Duration::from_millis(20));
    let changes = page.engine().take_same_document_changes();
    let change = |path: &str, push: bool, index: usize| SameDocumentChange {
        url: server.url(path).to_string(),
        push,
        index,
    };
    assert_eq!(
        changes,
        [
            change("/p1", true, 1),
            change("/p2", false, 1),
            change("/p2#x", true, 2)
        ]
    );
    // `history.back()` does nothing in the page: the browser traverses.
    eval(&mut page, "history.back()");
    eval(&mut page, "history.go(-2)");
    page.run_for(Duration::from_millis(20));
    assert_eq!(page.engine().take_traversals(), [-1, -2]);
    assert_eq!(
        eval(&mut page, "location.pathname + location.hash"),
        "/p2#x"
    );
    // The browser's traversal to the document's entry 1: `popstate` with its state, and
    // `hashchange` since the fragment went.
    page.go_in_document(1);
    page.run_for(Duration::from_millis(50));
    assert_eq!(eval(&mut page, "location.pathname + location.hash"), "/p2");
    assert_eq!(eval(&mut page, "document.title"), "pop {\"b\":2} hash");
    // Back to the first entry (the document's load): a null state.
    page.go_in_document(0);
    page.run_for(Duration::from_millis(50));
    assert_eq!(eval(&mut page, "location.pathname"), "/");
    assert_eq!(eval(&mut page, "document.title"), "pop null");
    // An entry the document does not have: nothing happens.
    page.go_in_document(7);
    page.run_for(Duration::from_millis(50));
    assert_eq!(eval(&mut page, "location.pathname"), "/");
    assert!(page.engine().take_same_document_changes().is_empty());
}

#[test]
fn without_browser_navigation_the_engine_navigates_and_traverses_itself() {
    let server = Server::start(vec![(
        "/",
        "text/html",
        "<title>one</title><script>onpopstate = () => document.title = 'pop';</script>",
    )]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        ..Default::default()
    });
    page.navigate(server.url("/"));
    settle(&mut page, server.url("/").as_str());
    assert!(page.engine().take_begun_navigations().is_empty());
    assert_eq!(eval(&mut page, "document.title"), "one");
    assert_eq!(server.hits("/"), 1);
    eval(
        &mut page,
        "history.pushState(null, '', '/p1'); history.back()",
    );
    page.run_for(Duration::from_millis(50));
    assert!(page.engine().take_traversals().is_empty());
    assert!(page.engine().take_same_document_changes().is_empty());
    assert_eq!(eval(&mut page, "location.pathname"), "/");
    assert_eq!(eval(&mut page, "document.title"), "pop");
}

/// The committed document's rendering goes on: its animation frames and timers run, and its time
/// origin is sane (the navigation's start the browser gave, which crossed as an age).
#[test]
fn a_committed_document_renders_and_keeps_time() {
    let server = Server::start(vec![(
        "/",
        "text/html",
        "<title>0</title><script>let n = 0, t = 0; requestAnimationFrame(function f() { n++; document.title = n + ' ' + t; requestAnimationFrame(f); }); setInterval(() => t++, 10);</script>",
    )]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(snowlark_page::RenderSetup {
            net: None,
            text: std::sync::Arc::new(snowlark_paint::TextEngine::new().unwrap()),
            device_pixel_ratio: 1.0,
            scrollbar_width: 0.0,
            trace: false,
            dark: false,
        }),
        ..Default::default()
    });
    page.engine().set_browser_navigation(true);
    let url = server.url("/");
    page.navigate(url.clone());
    let begin = begun(&mut page);
    // (the navigation began a while ago in the browser: the start crosses as an age)
    let (head, stream) = fetch_head(&net, begin.params.into_request());
    page.commit_navigation(
        CommitParams {
            url: url.clone(),
            begun: Some(begin.seq),
            reload: false,
            response: CommitResponse::Fetched {
                head: Box::new(head),
            },
            violations: Vec::new(),
            started: Some(Instant::now() - Duration::from_millis(120)),
        },
        Some(stream),
    );
    settle(&mut page, url.as_str());
    // (as a tab's thread drives the page: runs, then waits for the next timer or frame)
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(400) {
        page.run_for(Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(4));
    }
    let title = eval(&mut page, "document.title");
    let mut parts = title.split(' ').map(|p| p.parse::<u32>().unwrap_or(0));
    let (frames, ticks) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    assert!(frames >= 10, "animation frames ran: {title}");
    assert!(ticks >= 10, "timers ran: {title}");
    let now: f64 = eval(&mut page, "performance.now()").parse().unwrap();
    assert!((100.0..5000.0).contains(&now), "performance.now() = {now}");
    assert!(page.engine().load_stats().response_ms as i64 >= 0);
}
