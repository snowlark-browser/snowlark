// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Keepalive fetches the page let go of (Fetch: the keepalive flag; Chromium's `KeepAliveURLLoader`):
//! they run on to their response head, at most `keepalive_detached_timeout`.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use snowlark_net::{NetConfig, NetThread, Request, Url};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn net_with(detached: Duration) -> NetThread {
    NetThread::start(NetConfig {
        response_timeout: Duration::from_secs(10),
        read_timeout: Duration::from_secs(10),
        keepalive_detached_timeout: detached,
        ..NetConfig::default()
    })
    .unwrap()
}

fn request(url: &str, keepalive: bool) -> Request {
    let mut r = Request::new(Url::parse(url).unwrap());
    r.keepalive = keepalive;
    r
}

fn wait_for(deadline: Duration, done: impl Fn() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

/// `/a…` answers, 300 ms after the request, with a redirect to `/b…`, which counts its requests.
fn redirecting_server(hits: Arc<Mutex<Vec<String>>>) -> RawServer {
    RawServer::start(move |mut s, _| {
        let hits = hits.clone();
        async move {
            let Some(head) = read_head(&mut s).await else {
                return;
            };
            let path = head.split(' ').nth(1).unwrap_or("").to_string();
            if let Some(rest) = path.strip_prefix("/a") {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let answer = format!(
                    "HTTP/1.1 302 Found\r\nLocation: /b{rest}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                write_all(&mut s, answer.as_bytes()).await;
            } else {
                hits.lock().unwrap().push(path);
                write_all(
                    &mut s,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .await;
            }
        }
    })
}

#[test]
fn a_keepalive_fetch_dropped_before_its_response_runs_on_through_redirects() {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let server = redirecting_server(hits.clone());
    let net = net_with(Duration::from_secs(30));
    // A fetch without the flag stops when its stream goes: the redirect is never followed.
    drop(net.client().fetch(request(&server.url("/a?plain"), false)));
    // One with it reaches the redirect's target.
    drop(
        net.client()
            .fetch(request(&server.url("/a?keepalive"), true)),
    );
    assert!(
        wait_for(Duration::from_secs(5), || hits.lock().unwrap().len() == 1),
        "the keepalive fetch never reached /b: {:?}",
        hits.lock().unwrap()
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(*hits.lock().unwrap(), vec!["/b?keepalive".to_string()]);
}

#[test]
fn a_detached_keepalive_fetch_ends_at_its_response_head() {
    // The head comes after 300 ms, then a body that never ends: nobody wants the body, so the fetch
    // closes the connection once it has the head.
    let closed = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(AtomicUsize::new(0));
    let (c, r) = (closed.clone(), requests.clone());
    let server = RawServer::start(move |mut s, _| {
        let (closed, requests) = (c.clone(), r.clone());
        async move {
            if read_head(&mut s).await.is_none() {
                return;
            }
            requests.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(300)).await;
            if s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000000\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            for _ in 0..400 {
                if s.write_all(&[b'x'; 1024]).await.is_err() || s.flush().await.is_err() {
                    closed.store(true, Ordering::SeqCst);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    });
    let net = net_with(Duration::from_secs(30));
    drop(net.client().fetch(request(&server.url("/"), true)));
    assert!(wait_for(Duration::from_secs(3), || requests
        .load(Ordering::SeqCst)
        == 1));
    assert!(
        wait_for(Duration::from_secs(3), || closed.load(Ordering::SeqCst)),
        "the fetch kept reading a body nobody takes"
    );
}

#[test]
fn a_detached_keepalive_fetch_ends_at_the_time_limit() {
    // A server that never answers: the fetch ends at the detached time limit, not at once (as a
    // fetch without the flag) nor at the response timeout (10 s).
    let ended = Arc::new(Mutex::new(None::<Instant>));
    let e = ended.clone();
    let server = RawServer::start(move |mut s, _| {
        let ended = e.clone();
        async move {
            if read_head(&mut s).await.is_none() {
                return;
            }
            let mut byte = [0u8; 1];
            let _ = s.read(&mut byte).await;
            *ended.lock().unwrap() = Some(Instant::now());
        }
    });
    let net = net_with(Duration::from_millis(600));
    let start = Instant::now();
    drop(net.client().fetch(request(&server.url("/"), true)));
    assert!(
        wait_for(Duration::from_secs(5), || ended.lock().unwrap().is_some()),
        "the fetch did not end"
    );
    let after = ended.lock().unwrap().unwrap() - start;
    assert!(
        after >= Duration::from_millis(550) && after < Duration::from_secs(3),
        "ended {after:?} after the stream was dropped"
    );
}
