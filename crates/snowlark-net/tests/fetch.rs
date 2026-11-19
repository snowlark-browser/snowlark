// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration tests: the Fetch layer against in-process servers.

mod common;

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use snowlark_net::csp::{CspList, Disposition, Policy, PolicySource};
use snowlark_net::{
    BlockReason, Body, Client, CredentialsMode, Destination, FetchEvent, Mode, NetErrorKind,
    RedirectMode, Request, ResponseType, TlsTrust, Url,
};

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn client_for(page: &str) -> Arc<Client> {
    Arc::new(Client::for_url(&url(page)))
}

#[test]
fn get_over_http1_with_default_headers() {
    let server = TestServer::start(|_| TestResponse::ok("hello").header("X-Test", "1"));
    let net = net(None);
    let response = net
        .client()
        .fetch(Request::navigation(url(&server.url("/a?b=c"))))
        .read_all()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.head.status_text, "OK");
    assert_eq!(response.body_bytes(), b"hello");
    assert_eq!(response.head.headers.get("x-test").as_deref(), Some("1"));
    assert_eq!(response.head.response_type, ResponseType::Basic);
    assert_eq!(response.head.connection.protocol, "http/1.1");
    let seen = &server.requests()[0];
    assert_eq!(seen.target, "/a?b=c");
    assert!(seen.header("user-agent").unwrap().contains(" Snowlark/"));
    assert!(seen.header("accept").unwrap().starts_with("text/html"));
    assert_eq!(
        seen.header("accept-encoding"),
        Some("gzip, deflate, br, zstd")
    );
    assert_eq!(seen.header("accept-language"), Some("en-US,en;q=0.9"));
    // 127.0.0.1 is potentially trustworthy, so Fetch Metadata is sent
    assert_eq!(seen.header("sec-fetch-site"), Some("none"));
    assert_eq!(seen.header("sec-fetch-mode"), Some("navigate"));
    assert_eq!(seen.header("sec-fetch-dest"), Some("document"));
    assert_eq!(seen.header("sec-fetch-user"), Some("?1"));
    assert_eq!(
        seen.header("host").map(str::to_owned),
        Some(format!("127.0.0.1:{}", server.port()))
    );
}

#[test]
fn keep_alive_reuses_the_connection() {
    let server = TestServer::start(|r| TestResponse::ok(r.path()));
    let net = net(None);
    let c = net.client();
    let first = c
        .fetch(Request::navigation(url(&server.url("/1"))))
        .read_all()
        .unwrap();
    let second = c
        .fetch(Request::navigation(url(&server.url("/2"))))
        .read_all()
        .unwrap();
    assert_eq!(second.body_bytes(), b"/2");
    assert!(!first.head.connection.reused);
    assert!(second.head.connection.reused);
    assert_eq!(
        server.connections.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn stale_pooled_connection_is_retried_on_a_fresh_socket() {
    // The server answers the first request on each connection, then closes the connection when the next
    // request arrives on it, without a response: the reused socket was stale.
    let server = RawServer::start(|mut stream, _| async move {
        if read_head(&mut stream).await.is_none() {
            return;
        }
        write_all(
            &mut stream,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        )
        .await;
        let _ = read_head(&mut stream).await;
        drop(stream);
    });
    let net = net(None);
    let c = net.client();
    assert_eq!(
        c.fetch(Request::navigation(url(&server.url("/1"))))
            .read_all()
            .unwrap()
            .body_bytes(),
        b"ok"
    );
    let second = c
        .fetch(Request::navigation(url(&server.url("/2"))))
        .read_all()
        .unwrap();
    assert_eq!(second.body_bytes(), b"ok");
    assert!(!second.head.connection.reused);
    assert_eq!(
        server.connections.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

#[test]
fn chunked_body_streams_in_pieces() {
    let server = RawServer::start(|mut stream, _| async move {
        if read_head(&mut stream).await.is_none() {
            return;
        }
        write_all(
            &mut stream,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        )
        .await;
        for piece in ["first ", "second ", "third"] {
            write_all(
                &mut stream,
                format!("{:x}\r\n{piece}\r\n", piece.len()).as_bytes(),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        write_all(&mut stream, b"0\r\n\r\n").await;
    });
    let net = net(None);
    let mut stream = net
        .client()
        .fetch(Request::navigation(url(&server.url("/"))));
    let mut chunks = Vec::new();
    let mut saw_head = false;
    while let Some(event) = stream.next_blocking() {
        match event {
            FetchEvent::Head(h) => {
                assert_eq!(h.status, 200);
                saw_head = true;
            }
            FetchEvent::Body(b) => {
                assert!(saw_head);
                chunks.push(b);
            }
            FetchEvent::Done => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(chunks.len() >= 3, "{chunks:?}");
    assert_eq!(chunks.concat(), b"first second third");
    assert!(stream.is_finished());
}

/// The pool report says which origin's connections are in use and who waits for one: a fetch
/// whose events nobody takes keeps its HTTP/1.1 connection (the network waits for the page once
/// the stream is full, as a full data pipe stops a loader reading its socket), so with one
/// connection per host the next request to it waits, and the report shows it.
#[test]
fn the_pool_report_shows_a_held_connection_and_a_waiting_request() {
    let server = RawServer::start(|mut stream, _| async move {
        while read_head(&mut stream).await.is_some() {
            write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await;
            // (more chunks, each a body event, than a fetch's stream holds)
            for _ in 0..300 {
                write_all(&mut stream, b"10\r\n0123456789abcdef\r\n").await;
            }
            write_all(&mut stream, b"0\r\n\r\n").await;
        }
    });
    let thread = snowlark_net::NetThread::start(snowlark_net::NetConfig {
        max_connections_per_host: 1,
        ..Default::default()
    })
    .unwrap();
    let c = thread.client();
    assert!(c.pool_report().is_empty());
    let mut held = c.fetch(Request::navigation(url(&server.url("/held"))));
    assert!(matches!(held.next_blocking(), Some(FetchEvent::Head(_))));
    let next = c.fetch(Request::navigation(url(&server.url("/next"))));
    let origin = format!("pool http://127.0.0.1:{}", server.addr.port());
    let expected = format!("{origin}: 1 of 1 connections in use, 1 waiting, 0 idle");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while c.pool_report() != [expected.clone()] && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(c.pool_report(), [expected]);
    // Taking the held fetch's events frees its connection for the request that waits.
    let held = {
        let mut body = 0;
        while let Some(event) = held.next_blocking() {
            match event {
                FetchEvent::Body(b) => body += b.len(),
                FetchEvent::Done => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        body
    };
    assert_eq!(held, 300 * 16);
    let response = next.read_all().unwrap();
    assert_eq!(response.body_bytes().len(), 300 * 16);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !c.pool_report().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(c.pool_report().is_empty(), "{:?}", c.pool_report());
}

#[test]
fn response_timeout() {
    let server = RawServer::start(|mut stream, _| async move {
        let _ = read_head(&mut stream).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let thread = snowlark_net::NetThread::start(snowlark_net::NetConfig {
        response_timeout: Duration::from_millis(300),
        ..Default::default()
    })
    .unwrap();
    let err = thread
        .client()
        .fetch(Request::navigation(url(&server.url("/"))))
        .read_all()
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Timeout);
}

#[test]
fn content_codings_are_decoded() {
    let text = "Snowlark decodes this. ".repeat(200);
    let t = text.clone();
    let server = TestServer::start(move |r| {
        let body = t.as_bytes();
        let (coding, encoded): (&str, Vec<u8>) = match r.path() {
            "/gzip" => {
                let mut e =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(body).unwrap();
                ("gzip", e.finish().unwrap())
            }
            "/deflate" => {
                let mut e =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(body).unwrap();
                ("deflate", e.finish().unwrap())
            }
            "/br" => {
                let mut e = brotli::CompressorWriter::new(Vec::new(), 4096, 9, 22);
                e.write_all(body).unwrap();
                ("br", e.into_inner())
            }
            "/zstd" => ("zstd", zstd::encode_all(body, 3).unwrap()),
            _ => ("identity", body.to_vec()),
        };
        TestResponse::ok("")
            .header("Content-Encoding", coding)
            .body(encoded)
    });
    let net = net(None);
    for coding in ["gzip", "deflate", "br", "zstd", "identity"] {
        let r = net
            .client()
            .fetch(Request::navigation(url(&server.url(&format!("/{coding}")))))
            .read_all()
            .unwrap();
        assert_eq!(r.body_bytes(), text.as_bytes(), "{coding}");
        assert_eq!(
            r.head.headers.get("content-encoding").as_deref(),
            Some(coding)
        );
    }
}

#[test]
fn redirects_follow_the_spec() {
    let server = TestServer::start(|r| match r.path() {
        "/301" => TestResponse::status(301).header("Location", "/echo"),
        "/302" => TestResponse::status(302).header("Location", "/echo"),
        "/303" => TestResponse::status(303).header("Location", "/echo"),
        "/307" => TestResponse::status(307).header("Location", "/echo"),
        "/308" => TestResponse::status(308).header("Location", "echo"),
        "/frag" => TestResponse::status(302).header("Location", "/echo"),
        "/loop" => TestResponse::status(302).header("Location", "/loop"),
        "/echo" => TestResponse::ok(&format!(
            "{} {} {}",
            r.method,
            String::from_utf8_lossy(&r.body),
            r.header("content-type").unwrap_or("-")
        )),
        _ => TestResponse::status(404),
    });
    let net = net(None);
    let post = |path: &str| {
        let mut r = Request::script_fetch(url(&server.url(path)), client_for(&server.url("/")));
        r.method = "POST".into();
        r.body = Some(Body::new("data"));
        r.headers.append("Content-Type", "text/plain");
        net.client().fetch(r).read_all().unwrap()
    };
    let text = |r: &snowlark_net::Response| String::from_utf8_lossy(r.body_bytes()).into_owned();
    assert_eq!(text(&post("/301")), "GET  -");
    assert_eq!(text(&post("/302")), "GET  -");
    assert_eq!(text(&post("/303")), "GET  -");
    assert_eq!(text(&post("/307")), "POST data text/plain");
    let r = post("/308");
    assert_eq!(text(&r), "POST data text/plain");
    assert!(r.head.redirected());
    assert_eq!(r.head.url().unwrap().path(), "/echo");

    let frag = net
        .client()
        .fetch(Request::navigation(url(&server.url("/frag#top"))))
        .read_all()
        .unwrap();
    assert_eq!(frag.head.url().unwrap().fragment(), Some("top"));

    let err = net
        .client()
        .fetch(Request::navigation(url(&server.url("/loop"))))
        .read_all()
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::TooManyRedirects);
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|r| r.path() == "/loop")
            .count(),
        21
    );

    let mut error_mode =
        Request::script_fetch(url(&server.url("/302")), client_for(&server.url("/")));
    error_mode.redirect = RedirectMode::Error;
    assert_eq!(
        net.client().fetch(error_mode).read_all().unwrap_err().kind,
        NetErrorKind::Redirect
    );

    let mut manual = Request::script_fetch(url(&server.url("/302")), client_for(&server.url("/")));
    manual.redirect = RedirectMode::Manual;
    let r = net.client().fetch(manual).read_all().unwrap();
    assert_eq!(r.head.response_type, ResponseType::OpaqueRedirect);
    assert_eq!(r.head.visible_status(), 0);
    assert!(r.head.visible_headers().is_empty());
}

#[test]
fn cross_origin_redirect_drops_authorization() {
    let b = TestServer::start(|r| TestResponse::ok(r.header("authorization").unwrap_or("none")));
    let target = b.url("/");
    let a = TestServer::start(move |r| match r.path() {
        "/same" => TestResponse::status(302).header("Location", "/echo"),
        "/echo" => TestResponse::ok(r.header("authorization").unwrap_or("none")),
        _ => TestResponse::status(302)
            .header("Location", &target)
            .header("Access-Control-Allow-Origin", "*"),
    });
    let net = net(None);
    let mut nav = Request::navigation(url(&a.url("/same")));
    nav.headers.append("Authorization", "Bearer t");
    assert_eq!(
        net.client().fetch(nav).read_all().unwrap().body_bytes(),
        b"Bearer t"
    );
    let mut nav = Request::navigation(url(&a.url("/away")));
    nav.headers.append("Authorization", "Bearer t");
    assert_eq!(
        net.client().fetch(nav).read_all().unwrap().body_bytes(),
        b"none"
    );
}

#[test]
fn cors_simple_requests() {
    let api = TestServer::start(|r| match r.path() {
        "/open" => TestResponse::ok("data")
            .header("Access-Control-Allow-Origin", "*")
            .header("Access-Control-Expose-Headers", "X-Visible")
            .header("X-Visible", "1")
            .header("X-Hidden", "2"),
        "/creds" => TestResponse::ok("data")
            .header(
                "Access-Control-Allow-Origin",
                r.header("origin").unwrap_or(""),
            )
            .header("Access-Control-Allow-Credentials", "true"),
        "/sheet" => TestResponse::typed("closed", "text/css"),
        _ => TestResponse::ok("closed"),
    });
    let page = TestServer::start(|_| TestResponse::ok(""));
    let client = client_for(&page.url("/"));
    let net = net(None);
    let c = net.client();

    let r = c
        .fetch(Request::script_fetch(
            url(&api.url("/open")),
            client.clone(),
        ))
        .read_all()
        .unwrap();
    assert_eq!(r.head.response_type, ResponseType::Cors);
    let visible = r.head.visible_headers().names();
    assert!(
        visible.contains(&"x-visible".to_string()) && visible.contains(&"content-type".to_string())
    );
    assert!(!visible.contains(&"x-hidden".to_string()));
    let sent = api.requests().pop().unwrap();
    assert_eq!(
        sent.header("origin").map(str::to_owned),
        Some(format!("http://127.0.0.1:{}", page.port()))
    );
    assert_eq!(sent.header("sec-fetch-mode"), Some("cors"));

    let err = c
        .fetch(Request::script_fetch(
            url(&api.url("/closed")),
            client.clone(),
        ))
        .read_all()
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Cors);

    let mut creds = Request::script_fetch(url(&api.url("/open")), client.clone());
    creds.credentials = CredentialsMode::Include;
    assert_eq!(
        c.fetch(creds).read_all().unwrap_err().kind,
        NetErrorKind::Cors
    );
    let mut creds = Request::script_fetch(url(&api.url("/creds")), client.clone());
    creds.credentials = CredentialsMode::Include;
    assert!(c.fetch(creds).read_all().is_ok());

    // no-cors: an opaque response of a type such a request may get (a style sheet) still reaches
    // the page, with nothing of it visible to script
    let sheet = Request::subresource(url(&api.url("/sheet")), Destination::Style, client.clone());
    let r = c.fetch(sheet).read_all().unwrap();
    assert_eq!(r.head.response_type, ResponseType::Opaque);
    assert_eq!(r.head.visible_status(), 0);
    assert!(!r.head.body_visible());
    assert_eq!(r.body_bytes(), b"closed");

    // same-origin mode
    let mut same = Request::script_fetch(url(&api.url("/open")), client);
    same.mode = Mode::SameOrigin;
    assert_eq!(
        c.fetch(same).read_all().unwrap_err().kind,
        NetErrorKind::Cors
    );
}

/// Fetch "clear cache entries": a request whose CORS check fails drops the preflight cache entries
/// for its origin and URL, so the next request is preflighted again.
#[test]
fn a_failed_cors_check_clears_the_preflight_cache() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let fail_next = Arc::new(AtomicBool::new(false));
    let fail = fail_next.clone();
    let api = TestServer::start(move |r| {
        if r.method == "OPTIONS" {
            return TestResponse::status(204)
                .header("Access-Control-Allow-Origin", "*")
                .header("Access-Control-Allow-Headers", "X-Custom")
                .header("Access-Control-Max-Age", "60");
        }
        if fail.swap(false, Ordering::SeqCst) {
            return TestResponse::ok("no CORS headers");
        }
        TestResponse::ok("ok").header("Access-Control-Allow-Origin", "*")
    });
    let client = client_for("http://127.0.0.1:1/");
    let net = net(None);
    let custom = || {
        let mut r = Request::script_fetch(url(&api.url("/thing")), client.clone());
        r.headers.append("X-Custom", "yes");
        r
    };
    net.client().fetch(custom()).read_all().unwrap();
    assert_eq!(api.count("OPTIONS"), 1);
    fail_next.store(true, Ordering::SeqCst);
    let err = net.client().fetch(custom()).read_all().unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Cors);
    assert_eq!(api.count("OPTIONS"), 1, "the cached preflight was used");
    net.client().fetch(custom()).read_all().unwrap();
    assert_eq!(api.count("OPTIONS"), 2, "the entry was cleared");
}

#[test]
fn cors_preflight_and_its_cache() {
    let api = TestServer::start(|r| {
        if r.method == "OPTIONS" {
            if r.header("access-control-request-method") == Some("PUT") {
                return TestResponse::status(204)
                    .header("Access-Control-Allow-Origin", "*")
                    .header("Access-Control-Allow-Methods", "PUT")
                    .header("Access-Control-Allow-Headers", "X-Custom")
                    .header("Access-Control-Max-Age", "60");
            }
            return TestResponse::status(204).header("Access-Control-Allow-Origin", "*");
        }
        TestResponse::ok(&format!(
            "{} {}",
            r.method,
            r.header("x-custom").unwrap_or("-")
        ))
        .header("Access-Control-Allow-Origin", "*")
    });
    let client = client_for("http://127.0.0.1:1/");
    let net = net(None);
    let put = || {
        let mut r = Request::script_fetch(url(&api.url("/thing")), client.clone());
        r.method = "PUT".into();
        r.headers.append("X-Custom", "yes");
        r
    };
    let r = net.client().fetch(put()).read_all().unwrap();
    assert_eq!(r.body_bytes(), b"PUT yes");
    let preflight = api
        .requests()
        .into_iter()
        .find(|r| r.method == "OPTIONS")
        .unwrap();
    assert_eq!(
        preflight.header("access-control-request-method"),
        Some("PUT")
    );
    assert_eq!(
        preflight.header("access-control-request-headers"),
        Some("x-custom")
    );
    assert!(preflight.header("x-custom").is_none());
    assert_eq!(preflight.header("accept"), Some("*/*"));
    // cached: no second preflight
    net.client().fetch(put()).read_all().unwrap();
    assert_eq!(api.count("OPTIONS"), 1);
    // a method the preflight doesn't allow
    let mut delete = Request::script_fetch(url(&api.url("/thing")), client.clone());
    delete.method = "DELETE".into();
    assert_eq!(
        net.client().fetch(delete).read_all().unwrap_err().kind,
        NetErrorKind::Cors
    );
    assert_eq!(api.count("DELETE"), 0);
}

#[test]
fn cookies_round_trip() {
    let server = TestServer::start(|r| match r.path() {
        "/set" => TestResponse::ok("set")
            .header("Set-Cookie", "session=abc; Path=/; HttpOnly")
            .header("Set-Cookie", "theme=dark; Max-Age=3600"),
        _ => TestResponse::typed(r.header("cookie").unwrap_or(""), "text/css"),
    });
    let net = net(None);
    let c = net.client();
    let origin = format!("http://a.test:{}", server.port());
    let set = c
        .fetch(Request::navigation(url(&format!("{origin}/set"))))
        .read_all()
        .unwrap();
    // Set-Cookie is in the internal response, but script never sees it
    assert_eq!(set.head.headers.get_all("set-cookie").len(), 2);
    assert!(set.head.visible_headers().get("set-cookie").is_none());
    let echo = c
        .fetch(Request::navigation(url(&format!("{origin}/echo"))))
        .read_all()
        .unwrap();
    assert_eq!(echo.body_bytes(), b"session=abc; theme=dark");
    // a same-site subresource carries them; a cross-site one doesn't (third-party cookies are blocked)
    let same = Request::subresource(
        url(&format!("{origin}/echo")),
        Destination::Image,
        client_for(&format!("{origin}/page")),
    );
    assert_eq!(
        c.fetch(same).read_all().unwrap().body_bytes(),
        b"session=abc; theme=dark"
    );
    let cross = Request::subresource(
        url(&format!("{origin}/echo")),
        Destination::Image,
        client_for(&format!("http://b.test:{}/page", server.port())),
    );
    assert_eq!(c.fetch(cross).read_all().unwrap().body_bytes(), b"");
    // credentials: omit
    let mut omit = Request::navigation(url(&format!("{origin}/echo")));
    omit.credentials = CredentialsMode::Omit;
    assert_eq!(c.fetch(omit).read_all().unwrap().body_bytes(), b"");
    assert_eq!(c.cookies().len(), 2);
}

#[test]
fn http2_over_tls_multiplexes() {
    let certs = test_certs();
    let server = TestServer::start_tls(server_tls(&certs, &[b"h2", b"http/1.1"]), |r| {
        TestResponse::ok(&format!("{} {}", r.version, r.path()))
            .header("Strict-Transport-Security", "max-age=600")
    });
    let net = net(Some(client_trust(&certs)));
    let c = net.client();
    let base = format!("https://a.test:{}", server.port());
    let streams: Vec<_> = (0..8)
        .map(|i| c.fetch(Request::navigation(url(&format!("{base}/{i}")))))
        .collect();
    for (i, s) in streams.into_iter().enumerate() {
        let r = s.read_all().unwrap();
        assert_eq!(r.head.connection.protocol, "h2");
        assert_eq!(r.body_bytes(), format!("HTTP/2.0 /{i}").as_bytes());
        assert_eq!(r.head.status_text, "");
    }
    assert_eq!(
        server.connections.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    // HSTS from a secure response
    assert!(c.hsts().is_known_host("a.test"));
    // the :authority is the URL's host, not the address connected to
    let seen = server.requests().pop().unwrap();
    assert_eq!(
        seen.header(":authority").map(str::to_owned),
        Some(format!("a.test:{}", server.port()))
    );
}

/// RFC 9113 §6.5.2: a response header section larger than hyper's 16 KiB default
/// SETTINGS_MAX_HEADER_LIST_SIZE (YouTube's home page) is accepted, over HTTP/2 and HTTP/1.1, and so
/// is an HTTP/1.1 response with more than hyper's default 100 fields.
#[test]
fn large_response_header_sections_are_accepted() {
    let certs = test_certs();
    let big = |_: &TestRequest| {
        let mut r = TestResponse::ok("big");
        for i in 0..6 {
            r = r.header(&format!("X-Big-{i}"), &"v".repeat(8 * 1024));
        }
        for i in 0..150 {
            r = r.header(&format!("X-Many-{i}"), "1");
        }
        r
    };
    let h2 = TestServer::start_tls(server_tls(&certs, &[b"h2"]), big);
    let h1 = TestServer::start(big);
    let net = net(Some(client_trust(&certs)));
    let c = net.client();
    for (target, protocol) in [
        (format!("https://a.test:{}/", h2.port()), "h2"),
        (h1.url("/"), "http/1.1"),
    ] {
        let r = c
            .fetch(Request::navigation(url(&target)))
            .read_all()
            .unwrap_or_else(|e| panic!("{protocol}: {e}"));
        assert_eq!(r.head.connection.protocol, protocol);
        assert_eq!(r.body_bytes(), b"big");
        assert_eq!(r.head.headers.get("x-big-5").map(|v| v.len()), Some(8192));
        assert_eq!(r.head.headers.get("x-many-149").as_deref(), Some("1"));
    }
}

/// RFC 9113 §8.2.2: connection-specific fields never go on an HTTP/2 stream (h2 refuses to send a
/// request with them), and `TE` only as "trailers".
#[test]
fn http2_requests_carry_no_connection_specific_fields() {
    let certs = test_certs();
    let server = TestServer::start_tls(server_tls(&certs, &[b"h2"]), |_| TestResponse::ok("ok"));
    let net = net(Some(client_trust(&certs)));
    let c = net.client();
    let base = format!("https://a.test:{}", server.port());
    for (te, sent) in [("gzip", None), ("trailers", Some("trailers"))] {
        let mut req = Request::navigation(url(&format!("{base}/{te}")));
        for (n, v) in [
            ("Connection", "keep-alive"),
            ("Keep-Alive", "timeout=5"),
            ("Proxy-Connection", "keep-alive"),
            ("Transfer-Encoding", "chunked"),
            ("Upgrade", "websocket"),
            ("TE", te),
        ] {
            req.headers.append(n, v);
        }
        let r = c.fetch(req).read_all().unwrap();
        assert_eq!(r.body_bytes(), b"ok");
        let seen = server.requests().pop().unwrap();
        for n in [
            "connection",
            "keep-alive",
            "proxy-connection",
            "transfer-encoding",
            "upgrade",
        ] {
            assert_eq!(seen.header(n), None, "{n}");
        }
        assert_eq!(seen.header("te"), sent);
    }
}

#[test]
fn http1_over_tls_and_certificate_errors() {
    let certs = test_certs();
    let server = TestServer::start_tls(server_tls(&certs, &[b"http/1.1"]), |_| {
        TestResponse::ok("secure")
    });
    let net = net(Some(client_trust(&certs)));
    let r = net
        .client()
        .fetch(Request::navigation(url(&format!(
            "https://a.test:{}/",
            server.port()
        ))))
        .read_all()
        .unwrap();
    assert_eq!(r.head.connection.protocol, "http/1.1");
    assert!(r.head.connection.tls_version.is_some());
    // a host the certificate doesn't name
    let err = net
        .client()
        .fetch(Request::navigation(url(&format!(
            "https://other.test:{}/",
            server.port()
        ))))
        .read_all()
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Tls);
    // the platform store doesn't trust the test CA
    let platform = common::net(None);
    let err = platform
        .client()
        .fetch(Request::navigation(url(&format!(
            "https://a.test:{}/",
            server.port()
        ))))
        .read_all()
        .unwrap_err();
    assert_eq!(err.kind, NetErrorKind::Tls);
}

#[test]
fn local_schemes() {
    let net = net(None);
    let c = net.client();
    let data = c
        .fetch(Request::navigation(url("data:text/plain;base64,SGk=")))
        .read_all()
        .unwrap();
    assert_eq!(data.body_bytes(), b"Hi");
    assert_eq!(
        data.head.headers.get("content-type").as_deref(),
        Some("text/plain")
    );
    let blank = c
        .fetch(Request::navigation(url("about:blank")))
        .read_all()
        .unwrap();
    assert_eq!(blank.status(), 200);
    assert!(c
        .fetch(Request::navigation(url("about:nothing")))
        .read_all()
        .is_err());

    let page = url("https://example.com/page");
    let blob_url = c.blobs().register(
        &page.origin(),
        snowlark_net::blob::BlobData::new(bytes::Bytes::from_static(b"0123456789"), "text/plain"),
    );
    let client = Arc::new(Client::for_url(&page));
    let r = c
        .fetch(Request::script_fetch(blob_url.clone(), client.clone()))
        .read_all()
        .unwrap();
    assert_eq!(r.body_bytes(), b"0123456789");
    assert_eq!(r.head.response_type, ResponseType::Basic);
    let mut ranged = Request::script_fetch(blob_url.clone(), client.clone());
    ranged.headers.append("Range", "bytes=2-4");
    let r = c.fetch(ranged).read_all().unwrap();
    assert_eq!((r.status(), r.body_bytes()), (206, &b"234"[..]));
    // a page of another origin can't read it
    let other = client_for("https://evil.example/");
    assert_eq!(
        c.fetch(Request::script_fetch(blob_url.clone(), other))
            .read_all()
            .unwrap_err()
            .kind,
        NetErrorKind::Cors
    );
    c.blobs().revoke(&blob_url, &page.origin());
    assert!(c
        .fetch(Request::script_fetch(blob_url, client))
        .read_all()
        .is_err());

    let dir = std::env::temp_dir().join(format!("snowlark-net-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("page.html");
    std::fs::write(&file, "<p>local</p>").unwrap();
    let file_url = Url::from_file_path(&file).unwrap();
    let r = c
        .fetch(Request::navigation(file_url.clone()))
        .read_all()
        .unwrap();
    assert_eq!(r.body_bytes(), b"<p>local</p>");
    assert_eq!(
        r.head.headers.get("content-type").as_deref(),
        Some("text/html")
    );
    // a web page can't read local files
    let web = Request::subresource(
        file_url.clone(),
        Destination::Image,
        client_for("https://example.com/"),
    );
    assert_eq!(
        c.fetch(web).read_all().unwrap_err().kind,
        NetErrorKind::Blocked(BlockReason::FileAccess)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn csp_and_mixed_content_block_requests() {
    let server = TestServer::start(|_| {
        TestResponse::typed("x", "text/javascript").header("Access-Control-Allow-Origin", "*")
    });
    let net = net(None);
    let c = net.client();
    let page = url("https://a.example/page");
    let with_policy = |text: &str, disposition: Disposition| {
        let mut list = CspList::new();
        list.push(Policy::parse(text, PolicySource::Header, disposition, page.origin()).0);
        let mut client = Client::for_url(&page);
        client.csp = Some(Arc::new(list));
        Arc::new(client)
    };
    let enforced = with_policy("connect-src 'self'", Disposition::Enforce);
    let collected = c
        .fetch(Request::script_fetch(url(&server.url("/api")), enforced))
        .collect();
    assert_eq!(
        collected.result.unwrap_err().kind,
        NetErrorKind::Blocked(BlockReason::Csp)
    );
    assert_eq!(collected.violations.len(), 1);
    assert_eq!(collected.violations[0].effective_directive, "connect-src");
    let report = with_policy("connect-src 'self'", Disposition::Report);
    let collected = c
        .fetch(Request::script_fetch(url(&server.url("/api")), report))
        .collect();
    assert!(collected.result.is_ok());
    assert_eq!(collected.violations.len(), 1);

    // an https page can't load an http script, and 127.0.0.1 is the exception (a priori authenticated)
    let secure = client_for("https://a.example/");
    let script = Request::subresource(
        url("http://insecure.example/s.js"),
        Destination::Script,
        secure.clone(),
    );
    assert_eq!(
        c.fetch(script).read_all().unwrap_err().kind,
        NetErrorKind::Blocked(BlockReason::MixedContent)
    );
    let local = Request::subresource(url(&server.url("/s.js")), Destination::Script, secure);
    assert!(c.fetch(local).read_all().is_ok());
    // bad ports
    let bad = Request::navigation(url("http://127.0.0.1:25/"));
    assert_eq!(
        c.fetch(bad).read_all().unwrap_err().kind,
        NetErrorKind::Blocked(BlockReason::BadPort)
    );
}

#[test]
fn form_action_is_checked_at_each_redirect() {
    let server = TestServer::start(|r| match r.path() {
        "/form/go" => TestResponse::status(302).header("Location", "http://localhost:4/elsewhere"),
        "/form/stay" => TestResponse::status(302).header("Location", "/done"),
        _ => TestResponse::ok("done"),
    });
    let net = net(None);
    let page = url(&server.url("/page"));
    let mut list = CspList::new();
    list.push(
        Policy::parse(
            &format!("form-action {}/form/", page.origin().ascii_serialization()),
            PolicySource::Header,
            Disposition::Enforce,
            page.origin(),
        )
        .0,
    );
    let mut client = Client::for_url(&page);
    client.csp = Some(Arc::new(list));
    let client = Arc::new(client);
    let submit = |path: &str| {
        let mut r = Request::navigation(url(&server.url(path)));
        r.client = Some(client.clone());
        r.user_initiated = false;
        r.form_submission = true;
        net.client().fetch(r).collect()
    };
    // Allowed by path, then redirected within the origin (paths no longer count after a redirect).
    let ok = submit("/form/stay");
    assert!(ok.result.is_ok());
    assert!(ok.violations.is_empty());
    // Redirected to another origin: blocked there, with a violation.
    let blocked = submit("/form/go");
    assert_eq!(
        blocked.result.unwrap_err().kind,
        NetErrorKind::Blocked(BlockReason::Csp)
    );
    assert_eq!(blocked.violations.len(), 1);
    assert_eq!(blocked.violations[0].effective_directive, "form-action");
    // Not a form: form-action does not apply.
    let mut r = Request::navigation(url(&server.url("/elsewhere")));
    r.client = Some(client.clone());
    assert!(net.client().fetch(r).collect().result.is_ok());
    // The first URL is checked too.
    assert!(submit("/other").result.is_err());
}

#[test]
fn referrer_and_origin_headers() {
    let server =
        TestServer::start(|_| TestResponse::ok("").header("Access-Control-Allow-Origin", "*"));
    let net = net(None);
    let page = format!("http://a.test:{}/dir/page.html?q=1#frag", server.port());
    let client = client_for(&page);
    // same-origin: the full URL without the fragment
    let same = Request::subresource(
        url(&format!("http://a.test:{}/img.png", server.port())),
        Destination::Image,
        client.clone(),
    );
    net.client().fetch(same).read_all().unwrap();
    let seen = server.requests().pop().unwrap();
    assert_eq!(
        seen.header("referer").map(str::to_owned),
        Some(format!("http://a.test:{}/dir/page.html?q=1", server.port()))
    );
    assert_eq!(seen.header("origin"), None);
    assert_eq!(
        seen.header("accept").map(|a| a.starts_with("image/")),
        Some(true)
    );
    // cross-origin: only the origin; a CORS request also says where it comes from
    let cross = Request::script_fetch(
        url(&format!("http://b.test:{}/api", server.port())),
        client.clone(),
    );
    net.client().fetch(cross).read_all().unwrap();
    let seen = server.requests().pop().unwrap();
    assert_eq!(
        seen.header("referer").map(str::to_owned),
        Some(format!("http://a.test:{}/", server.port()))
    );
    assert_eq!(
        seen.header("origin").map(str::to_owned),
        Some(format!("http://a.test:{}", server.port()))
    );
    // http (not trustworthy) gets no Fetch Metadata
    assert!(seen.header("sec-fetch-site").is_none());
}

#[test]
fn integrity_and_nosniff() {
    let server = TestServer::start(|r| match r.path() {
        "/empty.js" => TestResponse::ok("").header("Content-Type", "text/javascript"),
        _ => TestResponse::ok("alert(1)").header("X-Content-Type-Options", "nosniff"),
    });
    let net = net(None);
    let client = client_for(&server.url("/"));
    let mut good = Request::subresource(
        url(&server.url("/empty.js")),
        Destination::Script,
        client.clone(),
    );
    good.mode = Mode::Cors;
    good.integrity = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=".into();
    assert!(net.client().fetch(good).read_all().is_ok());
    let mut bad = Request::subresource(
        url(&server.url("/empty.js")),
        Destination::Script,
        client.clone(),
    );
    bad.integrity = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into();
    assert_eq!(
        net.client().fetch(bad).read_all().unwrap_err().kind,
        NetErrorKind::Integrity
    );
    let sniffed = Request::subresource(
        url(&server.url("/plain")),
        Destination::Script,
        client.clone(),
    );
    assert_eq!(
        net.client().fetch(sniffed).read_all().unwrap_err().kind,
        NetErrorKind::Blocked(BlockReason::Nosniff)
    );
    // the same response as a fetch() (destination "") is fine
    assert!(net
        .client()
        .fetch(Request::script_fetch(url(&server.url("/plain")), client))
        .read_all()
        .is_ok());
}

#[test]
fn notify_callback_and_cancel() {
    let server = TestServer::start(|_| TestResponse::ok("x"));
    let net = net(None);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let k = count.clone();
    let stream = net.client().fetch_with_notify(
        Request::navigation(url(&server.url("/"))),
        Arc::new(move || {
            k.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }),
    );
    assert!(stream.read_all().is_ok());
    // head, body, done. The callback runs on the network thread after each event is queued, so the
    // consumer can see the last event before its wake-up has been counted: allow it a moment.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while count.load(std::sync::atomic::Ordering::SeqCst) < 3
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(count.load(std::sync::atomic::Ordering::SeqCst) >= 3);
    // dropping a stream cancels it without disturbing the thread
    let slow = RawServer::start(|mut s, _| async move {
        let _ = read_head(&mut s).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    net.client()
        .fetch(Request::navigation(url(&slow.url("/"))))
        .cancel();
    assert!(net
        .client()
        .fetch(Request::navigation(url(&server.url("/"))))
        .read_all()
        .is_ok());
}

#[test]
fn persistence_through_the_profile() {
    let dir = std::env::temp_dir().join(format!("snowlark-net-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server =
        TestServer::start(|_| TestResponse::ok("").header("Set-Cookie", "kept=1; Max-Age=3600"));
    {
        let thread = snowlark_net::NetThread::start(snowlark_net::NetConfig {
            profile_dir: Some(dir.clone()),
            ..Default::default()
        })
        .unwrap();
        thread
            .client()
            .fetch(Request::navigation(url(&server.url("/"))))
            .read_all()
            .unwrap();
    } // dropping the thread saves
    let saved = std::fs::read_to_string(dir.join("cookies.json")).unwrap();
    assert!(saved.contains("\"kept\""));
    let thread = snowlark_net::NetThread::start(snowlark_net::NetConfig {
        profile_dir: Some(dir.clone()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(thread.client().cookies().len(), 1);
    drop(thread);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "network: fetches https://example.com"]
fn real_fetch_of_example_com() {
    let thread = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let r = thread
        .client()
        .fetch(Request::navigation(url("https://example.com/")))
        .read_all()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(String::from_utf8_lossy(r.body_bytes()).contains("Example Domain"));
}

#[test]
fn an_observer_sees_every_event_on_the_network_thread() {
    let server = TestServer::start(|_| TestResponse::ok("observed body"));
    let net = net(None);
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let observer: snowlark_net::Observer = Arc::new(move |e: &FetchEvent| {
        log.lock().unwrap().push(match e {
            FetchEvent::Head(h) => format!("head {}", h.status),
            FetchEvent::Body(b) => format!("body {}", String::from_utf8_lossy(b)),
            FetchEvent::Done => "done".into(),
            FetchEvent::Error(e) => format!("error {:?}", e.kind),
            FetchEvent::Violation(_) => "violation".into(),
        });
    });
    let stream = net.client().fetch_observed(
        Request::navigation(url(&server.url("/o"))),
        Arc::new(|| {}),
        observer.clone(),
    );
    let response = stream.read_all().unwrap();
    assert_eq!(response.body_bytes(), b"observed body");
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["head 200", "body observed body", "done"]
    );

    // A fetch whose stream is dropped before it ends: the observer hears it was aborted.
    let slow = RawServer::start(|mut s, _| async move {
        let _ = read_head(&mut s).await;
        write_all(
            &mut s,
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: text/plain\r\n\r\nabc",
        )
        .await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    seen.lock().unwrap().clear();
    let mut stream = net.client().fetch_observed(
        Request::navigation(url(&slow.url("/s"))),
        Arc::new(|| {}),
        observer,
    );
    while !matches!(stream.next_blocking(), Some(FetchEvent::Body(_))) {}
    drop(stream);
    let start = std::time::Instant::now();
    while !seen.lock().unwrap().iter().any(|e| e.starts_with("error")) {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            seen.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(seen.lock().unwrap().last().unwrap(), "error Aborted");
}

#[test]
fn a_fetch_consumes_the_preload_with_its_key() {
    let server = TestServer::start(|r| TestResponse::ok(r.path()));
    let net = net(None);
    let c = net.client();
    let page = client_for(&server.url("/page.html"));
    let image =
        |path: &str| Request::subresource(url(&server.url(path)), Destination::Image, page.clone());
    // Preloaded, then fetched with the same key: one request, the preload's response.
    let preload = c.preload(image("/a.png"), None).unwrap();
    assert!(
        c.preload(image("/a.png"), None).is_some(),
        "a second preload watches the first"
    );
    assert_eq!(preload.read_all().unwrap().body_bytes(), b"/a.png");
    assert!(c.is_preloaded(&image("/a.png")));
    let got = c.fetch(image("/a.png")).read_all().unwrap();
    assert_eq!(got.body_bytes(), b"/a.png");
    assert_eq!(server.count("GET"), 1);
    assert!(!c.is_preloaded(&image("/a.png")), "consumed once");
    assert_eq!(c.preload_counts(), (1, 1));
    // Another mode is another key; so is another document.
    let _ = c.preload(image("/b.png"), None).unwrap().read_all();
    let mut cors = image("/b.png");
    cors.mode = Mode::Cors;
    cors.credentials = CredentialsMode::SameOrigin;
    let _ = c.fetch(cors).read_all().unwrap();
    let other_doc = Request::subresource(
        url(&server.url("/b.png")),
        Destination::Image,
        client_for(&server.url("/other.html")),
    );
    let _ = c.fetch(other_doc).read_all().unwrap();
    assert_eq!(server.count("GET"), 4);
    // A consumer with integrity metadata takes only a preload made with the same.
    let mut with_integrity = image("/b.png");
    with_integrity.integrity = "sha256-abc".into();
    let r = c.fetch(with_integrity).read_all();
    assert!(r.is_err(), "fetched again, and the integrity check fails");
    assert_eq!(server.count("GET"), 5);
    assert!(c.is_preloaded(&image("/b.png")));
    // A navigation never consumes one.
    let _ = c.preload(image("/c.png"), None).unwrap().read_all();
    let _ = c
        .fetch(Request::navigation(url(&server.url("/c.png"))))
        .read_all();
    assert_eq!(server.count("GET"), 7);
}

#[test]
fn preconnect_pools_a_connection_for_the_next_request() {
    let server = TestServer::start(|r| TestResponse::ok(r.path()));
    let net = net(None);
    let c = net.client();
    c.preconnect(&url(&server.url("/")));
    let start = std::time::Instant::now();
    while server.connections.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(50));
    let r = c
        .fetch(Request::navigation(url(&server.url("/x"))))
        .read_all()
        .unwrap();
    assert!(r.head.connection.reused, "the preconnected connection");
    assert_eq!(
        server.connections.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    c.dns_prefetch("localhost", 80);
}

/// `SNOWLARK_EXTRA_ROOT_CERTS`' trust: a server whose certificate a test CA signed is trusted once the
/// CA's PEM is added to the platform's roots, and refused with the platform's roots alone.
#[test]
fn extra_root_certificates_from_a_pem_file_are_trusted() {
    use base64::Engine as _;
    let certs = test_certs();
    let b64 = base64::engine::general_purpose::STANDARD.encode(certs.ca.as_ref());
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    let pem = format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        lines.join("\n")
    );
    let roots = snowlark_net::extra_roots_from_pem(pem.as_bytes()).unwrap();
    assert_eq!(roots, vec![certs.ca.clone()]);
    // (the server sends its own certificate only, as wptserve does: a chain ending in the test CA
    // would be checked against the platform's roots alone)
    let leaf_only = TestCerts {
        ca: certs.ca.clone(),
        chain: certs.chain[..1].to_vec(),
        key: certs.key.clone_key(),
    };
    let server = TestServer::start_tls(server_tls(&leaf_only, &[b"http/1.1"]), |_| {
        TestResponse::ok("trusted")
    });
    let target = url(&format!("https://a.test:{}/", server.port()));
    let trusted = net(Some(TlsTrust::PlatformWithExtraRoots(roots)));
    let r = trusted
        .client()
        .fetch(Request::navigation(target.clone()))
        .read_all()
        .unwrap_or_else(|e| panic!("not trusted: {e}"));
    assert_eq!(r.body_bytes(), b"trusted");
    let platform = net(Some(TlsTrust::Platform));
    assert!(platform
        .client()
        .fetch(Request::navigation(target))
        .read_all()
        .is_err());
}

/// A server that ends a close-delimited HTTP/1.1 body by closing the TCP connection without a TLS
/// `close_notify` (as wptserve does): the body is complete, not an error.
#[test]
fn a_tls_close_without_close_notify_ends_a_close_delimited_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let certs = test_certs();
    let config = server_tls(&certs, &[b"http/1.1"]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = tokio_rustls::TlsAcceptor::from(config)
                .accept(tcp)
                .await
                .unwrap();
            let mut buf = [0u8; 4096];
            let _ = tls.read(&mut buf).await.unwrap();
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nwhole body")
                .await
                .unwrap();
            tls.flush().await.unwrap();
            // (no close_notify: the TCP connection just ends)
            let (tcp, _) = tls.into_inner();
            drop(tcp);
        });
    });
    let net = net(Some(client_trust(&certs)));
    let r = net
        .client()
        .fetch(Request::navigation(url(&format!("https://a.test:{port}/"))))
        .read_all()
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(r.body_bytes(), b"whole body");
}

/// M5 step 3: a child navigable's response is checked for `X-Frame-Options` and `frame-ancestors`
/// in the network layer (`Request::frame_ancestors`): a refused response is a network error with
/// no body delivered, a `frame-ancestors` violation is reported to the policy's `report-uri` from
/// here and sent to the requester for its console; a top-level navigation is not checked.
#[test]
fn a_frame_response_that_refuses_framing_is_a_network_error() {
    let server = TestServer::start(|r| match r.target.as_str() {
        "/deny" => TestResponse::ok("<p>secret").header("X-Frame-Options", "DENY"),
        "/same" => TestResponse::ok("<p>same").header("X-Frame-Options", "SAMEORIGIN"),
        "/fa" => TestResponse::ok("<p>secret").header(
            "Content-Security-Policy",
            "frame-ancestors 'none'; report-uri /report",
        ),
        "/report" => TestResponse::status(204),
        _ => TestResponse::ok("<p>plain"),
    });
    let net = net(None);
    let own = url(&server.url("/")).origin();
    let other = Url::parse("https://other.test/").unwrap().origin();
    let framed = |path: &str, ancestors: Option<Vec<url::Origin>>| {
        let mut r = Request::navigation(url(&server.url(path)));
        r.client = Some(client_for(&server.url("/parent")));
        r.frame_ancestors = ancestors;
        let mut events = Vec::new();
        let mut stream = net.client().fetch(r);
        while let Some(e) = stream.next_blocking() {
            events.push(e);
        }
        events
    };
    let kinds = |events: &[FetchEvent]| -> Vec<String> {
        events
            .iter()
            .map(|e| match e {
                FetchEvent::Head(_) => "head".into(),
                FetchEvent::Body(_) => "body".into(),
                FetchEvent::Done => "done".into(),
                FetchEvent::Violation(v) => format!("violation:{}", v.effective_directive),
                FetchEvent::Error(e) => format!("error:{:?}", e.kind),
            })
            .collect()
    };
    // DENY: refused whoever frames it; the body never leaves the network layer.
    let got = framed("/deny", Some(vec![own.clone()]));
    assert_eq!(kinds(&got), ["error:Blocked(Framing)"], "{got:?}");
    // SAMEORIGIN: the same-origin ancestor may frame it, another may not.
    assert_eq!(
        kinds(&framed("/same", Some(vec![own.clone()]))),
        ["head", "body", "done"]
    );
    assert_eq!(
        kinds(&framed("/same", Some(vec![own.clone(), other.clone()]))),
        ["error:Blocked(Framing)"]
    );
    // frame-ancestors 'none': refused, the violation reported to the policy's endpoint from here
    // and sent to the requester.
    let got = framed("/fa", Some(vec![own.clone()]));
    assert_eq!(
        kinds(&got),
        ["violation:frame-ancestors", "error:Blocked(Framing)"],
        "{got:?}"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let report = loop {
        if let Some(r) = server
            .requests()
            .into_iter()
            .find(|r| r.target == "/report")
        {
            break r;
        }
        assert!(std::time::Instant::now() < deadline, "no report arrived");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        report.header("content-type"),
        Some("application/csp-report")
    );
    let body = String::from_utf8(report.body.clone()).unwrap();
    assert!(
        body.contains("\"effective-directive\":\"frame-ancestors\""),
        "{body}"
    );
    // A top-level navigation (no ancestors) is not checked.
    assert_eq!(kinds(&framed("/deny", None)), ["head", "body", "done"]);
}

/// M5 step 3, second half: opaque-response blocking (Fetch "opaque-response-safelist check") in
/// the network layer: a cross-origin `no-cors` response that is neither labelled nor sniffed as a
/// script, style sheet, image or media, and does not parse as a script, is a network error with no
/// body delivered; a same-origin or CORS response is not checked.
#[test]
fn an_opaque_response_of_an_unsafe_type_is_a_network_error() {
    let png: Vec<u8> = b"\x89PNG\r\n\x1A\n\x00\x00\x00\rIHDR".to_vec();
    // MPEG-1 layer III frames, 128 kbit/s at 44.1 kHz: 417 bytes each.
    let mut mp3 = vec![0u8; 900];
    for at in [0, 417] {
        mp3[at] = 0xFF;
        mp3[at + 1] = 0xFB;
        mp3[at + 2] = 0x90;
    }
    let png_len = png.len();
    // An AVIF file's first box: `ftyp`, major brand `avif`, compatible brands `mif1`, `avif`, `miaf`.
    let avif: Vec<u8> =
        b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00mif1avifmiaf\x00\x00\x00\xd6meta".to_vec();
    let avif_len = avif.len();
    let server = TestServer::start(move |r| match r.target.as_str() {
        "/avif" => TestResponse::status(200)
            .header("Content-Type", "image/avif")
            .body(avif.clone()),
        "/text-as-png" => TestResponse::ok("hello").header("Content-Type", "image/png"),
        "/text" => TestResponse::ok("hello"),
        "/json" => TestResponse::ok("{\"a\": 1}").header("Content-Type", "application/json"),
        "/json-untyped" => TestResponse::status(200).body("{\"a\": 1}"),
        "/text-nosniff" => TestResponse::ok("hello").header("X-Content-Type-Options", "nosniff"),
        "/png" => TestResponse::status(200)
            .header("Content-Type", "image/png")
            .body(png.clone()),
        "/png-as-html" => TestResponse::status(200)
            .header("Content-Type", "text/html")
            .body(png.clone()),
        "/png-mid" => TestResponse::status(206)
            .header("Content-Type", "image/png")
            .header("Content-Range", "bytes 10-99/1010")
            .body(png.clone()),
        "/css" => TestResponse::ok("a{}").header("Content-Type", "text/css"),
        "/pdf" => TestResponse::ok("%PDF").header("Content-Type", "application/pdf"),
        "/script-untyped" => TestResponse::status(200).body("var a = 1;"),
        "/script-as-text" => TestResponse::ok("var a = 1;"),
        "/html-as-text" => TestResponse::ok("<p>secret"),
        "/mp3" => match r.header("range") {
            Some("bytes=417-") => TestResponse::status(206)
                .header("Content-Range", "bytes 417-899/900")
                .body(mp3[417..].to_vec()),
            _ => TestResponse::status(200).body(mp3.clone()),
        },
        _ => TestResponse::status(404),
    });
    // A script parser the test controls: `var` scripts parse, nothing else does.
    struct Stub;
    impl snowlark_net::orb::ScriptCheck for Stub {
        fn parses_as_script(&self, source: String, reply: Box<dyn FnOnce(bool) + Send>) {
            reply(source.starts_with("var "));
        }
    }
    let net = net(None);
    net.set_script_check(Arc::new(Stub));
    let other = client_for("http://other.test/");
    let fetch =
        |path: &str, client: &Arc<Client>, destination: Destination, range: Option<&str>| {
            let mut r = Request::subresource(url(&server.url(path)), destination, client.clone());
            if let Some(range) = range {
                r.headers.set("Range", range);
            }
            let mut stream = net.client().fetch(r);
            let mut events = Vec::new();
            while let Some(e) = stream.next_blocking() {
                events.push(match e {
                    FetchEvent::Head(_) => "head".to_string(),
                    FetchEvent::Body(b) => format!("body:{}", b.len()),
                    FetchEvent::Done => "done".into(),
                    FetchEvent::Violation(_) => "violation".into(),
                    FetchEvent::Error(e) => format!("error:{:?}", e.kind),
                });
            }
            events
        };
    let blocked = ["error:Blocked(OpaqueResponse)".to_string()];
    let allowed = |n: usize| ["head".to_string(), format!("body:{n}"), "done".into()];
    let no_cors = |path: &str| fetch(path, &other, Destination::Empty, None);
    // Labelled types: the safelist, the never-sniffed list, nosniff.
    assert_eq!(no_cors("/css"), allowed(3));
    assert_eq!(no_cors("/pdf"), blocked);
    assert_eq!(no_cors("/text-nosniff"), blocked);
    // Sniffed: an image whatever its label; a partial image not from the start.
    assert_eq!(no_cors("/png"), allowed(png_len));
    assert_eq!(no_cors("/png-as-html"), allowed(png_len));
    assert_eq!(no_cors("/png-mid"), blocked);
    // An AVIF image passes; a body labelled as an image that is none does not.
    assert_eq!(no_cors("/avif"), allowed(avif_len));
    assert_eq!(no_cors("/text-as-png"), blocked);
    // Parsed: a script however it is labelled, nothing else; an unlabelled body is let through.
    assert_eq!(no_cors("/script-untyped"), allowed(10));
    assert_eq!(no_cors("/json-untyped"), allowed(8));
    assert_eq!(no_cors("/script-as-text"), allowed(10));
    assert_eq!(no_cors("/html-as-text"), blocked);
    assert_eq!(no_cors("/text"), blocked);
    assert_eq!(no_cors("/json"), blocked);
    // A media element's first fetch sniffs as audio, and its later range request from the middle
    // of the same URL is let through for that client, not for another.
    assert_eq!(
        fetch("/mp3", &other, Destination::Audio, None),
        allowed(900)
    );
    assert_eq!(
        fetch("/mp3", &other, Destination::Audio, Some("bytes=417-")),
        allowed(483)
    );
    let third = client_for("http://third.test/");
    assert_eq!(
        fetch("/mp3", &third, Destination::Audio, Some("bytes=417-")),
        blocked
    );
    // A same-origin response is not checked, nor one reached through a cross-origin redirect
    // that lands on the client's own origin.
    let same = client_for(&server.url("/"));
    assert_eq!(fetch("/text", &same, Destination::Empty, None), allowed(5));
    let text = server.url("/text");
    let elsewhere = TestServer::start(move |_| TestResponse::status(302).header("Location", &text));
    let r = Request::subresource(url(&elsewhere.url("/go")), Destination::Empty, same.clone());
    let got = net.client().fetch(r).collect();
    let body = got.result.map(|r| r.body_bytes().to_vec());
    assert_eq!(body, Ok(b"hello".to_vec()));
}
