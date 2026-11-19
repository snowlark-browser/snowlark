// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The HTTP cache (RFC 9111) against an in-process server: fresh responses are reused, stale ones
//! revalidated, `no-store` and the request cache modes respected, unsafe methods invalidate.

mod common;

use std::sync::Arc;

use common::*;
use snowlark_net::{CacheMode, Client, Request, Url};

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn get(net: &snowlark_net::NetThread, page: &str, u: &str, cache: CacheMode) -> (u16, Vec<u8>) {
    let mut r = Request::new(url(u));
    r.client = Some(Arc::new(Client::for_url(&url(page))));
    r.cache = cache;
    let response = net.client().fetch(r).read_all().unwrap();
    (response.status(), response.body_bytes().to_vec())
}

#[test]
fn fresh_responses_are_reused_and_stale_ones_revalidated() {
    let server = TestServer::start(|req| match req.path() {
        "/fresh" => TestResponse::ok("fresh").header("Cache-Control", "max-age=600"),
        "/etag" => {
            if req.header("if-none-match") == Some("\"v1\"") {
                TestResponse::status(304)
                    .header("ETag", "\"v1\"")
                    .header("Cache-Control", "max-age=0")
            } else {
                TestResponse::ok("tagged")
                    .header("ETag", "\"v1\"")
                    .header("Cache-Control", "max-age=0")
            }
        }
        "/nostore" => TestResponse::ok("no").header("Cache-Control", "no-store, max-age=600"),
        "/plain" => TestResponse::ok("plain"),
        _ => TestResponse::status(404),
    });
    let net = net(None);
    let page = server.url("/page");
    let count = |path: &str| {
        server
            .requests()
            .iter()
            .filter(|r| r.path() == path)
            .count()
    };

    // Fresh: the second request does not reach the server.
    for _ in 0..3 {
        assert_eq!(
            get(&net, &page, &server.url("/fresh"), CacheMode::Default),
            (200, b"fresh".to_vec())
        );
    }
    assert_eq!(count("/fresh"), 1);
    // "no-cache" and "reload" go to the server; "no-store" too.
    get(&net, &page, &server.url("/fresh"), CacheMode::NoCache);
    get(&net, &page, &server.url("/fresh"), CacheMode::Reload);
    get(&net, &page, &server.url("/fresh"), CacheMode::NoStore);
    assert_eq!(count("/fresh"), 4);

    // Stale with an ETag: a conditional request, and the 304 gives the stored body.
    assert_eq!(
        get(&net, &page, &server.url("/etag"), CacheMode::Default),
        (200, b"tagged".to_vec())
    );
    assert_eq!(
        get(&net, &page, &server.url("/etag"), CacheMode::Default),
        (200, b"tagged".to_vec())
    );
    let etag: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.path() == "/etag")
        .collect();
    assert_eq!(etag.len(), 2);
    assert_eq!(etag[0].header("if-none-match"), None);
    assert_eq!(etag[1].header("if-none-match"), Some("\"v1\""));
    // force-cache uses it without asking.
    get(&net, &page, &server.url("/etag"), CacheMode::ForceCache);
    assert_eq!(count("/etag"), 2);

    // no-store responses and responses with nothing to reuse them by are not kept.
    get(&net, &page, &server.url("/nostore"), CacheMode::Default);
    get(&net, &page, &server.url("/nostore"), CacheMode::Default);
    assert_eq!(count("/nostore"), 2);
    get(&net, &page, &server.url("/plain"), CacheMode::Default);
    get(&net, &page, &server.url("/plain"), CacheMode::Default);
    assert_eq!(count("/plain"), 2);
}

#[test]
fn partitions_and_unsafe_methods() {
    let server = TestServer::start(|_| {
        TestResponse::typed("x", "text/css").header("Cache-Control", "max-age=600")
    });
    let net = net(None);
    let a = server.url("/thing");
    // Another top-level site does not see this one's cache.
    get(&net, "https://one.test/", &a, CacheMode::Default);
    get(&net, "https://one.test/", &a, CacheMode::Default);
    get(&net, "https://two.test/", &a, CacheMode::Default);
    assert_eq!(server.count("GET"), 2);
    // A POST's success invalidates the stored response.
    let mut post = Request::new(url(&a));
    post.method = "POST".into();
    post.client = Some(Arc::new(Client::for_url(&url("https://one.test/"))));
    net.client().fetch(post).read_all().unwrap();
    get(&net, "https://one.test/", &a, CacheMode::Default);
    assert_eq!(server.count("GET"), 3);
}

/// A single byte range of a fresh stored complete response is answered from the cache with a
/// 206 (RFC 9111 §3.4); a range past the end, or of a stored response with a content coding,
/// goes to the server.
#[test]
fn ranges_of_a_stored_complete_response_come_from_the_cache() {
    let server = TestServer::start(|req| match req.path() {
        "/full" => TestResponse::ok("0123456789").header("Cache-Control", "max-age=600"),
        _ => TestResponse::status(404),
    });
    let net = net(None);
    let page = server.url("/page");
    let ranged = |range: &str| {
        let mut r = Request::new(url(&server.url("/full")));
        r.client = Some(Arc::new(Client::for_url(&url(&page))));
        r.headers.append("Range", range);
        let response = net.client().fetch(r).read_all().unwrap();
        (
            response.status(),
            response
                .head
                .headers
                .get("content-range")
                .unwrap_or_default(),
            String::from_utf8(response.body_bytes().to_vec()).unwrap(),
        )
    };
    assert_eq!(
        get(&net, &page, &server.url("/full"), CacheMode::Default).0,
        200
    );
    assert_eq!(
        ranged("bytes=2-4"),
        (206, "bytes 2-4/10".into(), "234".into())
    );
    assert_eq!(
        ranged("bytes=7-"),
        (206, "bytes 7-9/10".into(), "789".into())
    );
    assert_eq!(
        ranged("bytes=-3"),
        (206, "bytes 7-9/10".into(), "789".into())
    );
    let count = || {
        server
            .requests()
            .iter()
            .filter(|r| r.path() == "/full")
            .count()
    };
    assert_eq!(count(), 1);
    ranged("bytes=20-");
    assert_eq!(count(), 2);
}
