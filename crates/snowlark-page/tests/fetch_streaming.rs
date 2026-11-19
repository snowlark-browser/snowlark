// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fetch §5.6 `fetch()`: the promise resolves at the response head with a Response whose body is a
//! stream the network's bytes go to as they arrive; aborting after the head errors that stream;
//! cancelling it ends the fetch. Fetch §2.2.6: an opaque-redirect response keeps its URL. XHR
//! `setRequestHeader()` combines.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

fn wait_for(page: &mut Page, condition: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let js = format!("String(!!({condition}))");
    while !page.run_until(deadline, |p| p.eval(&js, "t").is_ok_and(|v| v == "true"))
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(eval(page, &js), "true", "timed out waiting for {condition}");
}

fn chunk(stream: &mut TcpStream, data: &str) {
    let _ = write!(stream, "{:x}\r\n{data}\r\n", data.len());
    let _ = stream.flush();
}

/// A server: `/` a page, `/stream` a chunked body whose first chunk is sent at once and the rest
/// once `/release` is requested, `/redirect` a 302 to `/`. Returns its origin.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let release_tx = release_tx.clone();
            let release_rx = release_rx.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    return;
                }
                let mut x_headers = String::new();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if h.to_ascii_lowercase().starts_with("x-")
                        || h.to_ascii_lowercase().starts_with("content-type:")
                    {
                        x_headers.push_str(h.trim_end());
                        x_headers.push('|');
                    }
                }
                let target = first.split(' ').nth(1).unwrap_or("/");
                let path = target.split('?').next().unwrap_or("/").to_owned();
                let fixed = |mime: &str, body: &str| {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                match path.as_str() {
                    "/stream" => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                        );
                        chunk(&mut stream, "first");
                        let got = release_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(20));
                        if got.is_ok() {
                            chunk(&mut stream, "second");
                            let _ = stream.write_all(b"0\r\n\r\n");
                        }
                    }
                    "/big" => {
                        // (many small chunks, each a body event of its own: more network events than a
                        // fetch's stream holds before the network waits for the page to take them)
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                        );
                        let piece = "x".repeat(64);
                        for _ in 0..300 {
                            chunk(&mut stream, &piece);
                        }
                        let _ = stream.write_all(b"0\r\n\r\n");
                        let _ = stream.flush();
                    }
                    "/release" => {
                        let _ = release_tx.send(());
                        let _ = stream.write_all(fixed("text/plain", "ok").as_bytes());
                    }
                    "/echo" => {
                        let _ = stream.write_all(fixed("text/plain", &x_headers).as_bytes());
                    }
                    "/redirect" => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 302 Found\r\nLocation: /\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                    _ => {
                        let _ =
                            stream.write_all(fixed("text/html", "<!doctype html><p>s").as_bytes());
                    }
                }
            });
        }
    });
    format!("http://{addr}")
}

fn page_at(base: &str) -> (Page, snowlark_net::NetThread) {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/")).unwrap());
    page.run_until_settled(Duration::from_secs(10));
    (page, net)
}

#[test]
fn fetch_resolves_at_the_head_and_streams_the_body() {
    let base = serve();
    let (mut page, _net) = page_at(&base);
    eval(
        &mut page,
        r#"
        window.r = [];
        fetch('/stream').then(async resp => {
            r.push('head');
            const reader = resp.body.getReader();
            const dec = new TextDecoder();
            for (;;) {
                const {value, done} = await reader.read();
                if (done) { r.push('done'); break; }
                r.push(dec.decode(value));
            }
        }, e => r.push('rejected:' + e));
        'started'
        "#,
    );
    // The rest of the body waits for /release: the head and first chunk must arrive without it.
    wait_for(&mut page, "r.includes('first')");
    assert_eq!(eval(&mut page, "r.join(',')"), "head,first");
    eval(&mut page, "fetch('/release'); 'released'");
    wait_for(&mut page, "r.includes('done')");
    assert_eq!(eval(&mut page, "r.join(',')"), "head,first,second,done");
}

#[test]
fn aborting_after_the_head_errors_the_body() {
    let base = serve();
    let (mut page, _net) = page_at(&base);
    eval(
        &mut page,
        r#"
        window.r = [];
        const c = new AbortController();
        fetch('/stream', {signal: c.signal}).then(async resp => {
            const reader = resp.body.getReader();
            const first = await reader.read();
            r.push('read:' + new TextDecoder().decode(first.value));
            c.abort();
            try { await reader.read(); r.push('no error'); }
            catch (e) { r.push(e.name); }
        });
        'started'
        "#,
    );
    wait_for(&mut page, "r.length == 2");
    assert_eq!(eval(&mut page, "r.join(',')"), "read:first,AbortError");
}

#[test]
fn an_opaque_redirect_keeps_the_request_url() {
    let base = serve();
    let (mut page, _net) = page_at(&base);
    eval(
        &mut page,
        r#"
        window.r = [];
        fetch('/redirect', {redirect: 'manual'})
            .then(resp => r.push(resp.type, resp.status, resp.url), e => r.push(String(e)));
        'started'
        "#,
    );
    wait_for(&mut page, "r.length > 0");
    assert_eq!(
        eval(&mut page, "r.join(',')"),
        format!("opaqueredirect,0,{base}/redirect")
    );
}

/// Responses whose bodies script never reads do not keep their connections: the network reads
/// each body to its end (Fetch §4.6 "HTTP-network fetch": the bytes are enqueued into the
/// response's body stream whether or not anyone reads it), so a seventh fetch to the host still
/// gets one of its six connections. The suspect was a fetch whose network events nobody takes
/// keeping its connection slot for good.
#[test]
fn unread_bodies_do_not_hold_the_hosts_connections() {
    let base = serve();
    let (mut page, _net) = page_at(&base);
    eval(
        &mut page,
        r#"
        window.held = [];
        window.r = [];
        Promise.all([0, 1, 2, 3, 4, 5].map(i => fetch('/big?' + i).then(resp => held.push(resp))))
            .then(() => fetch('/echo'))
            .then(resp => resp.text())
            .then(() => r.push('seventh'), e => r.push(String(e)));
        'started'
        "#,
    );
    wait_for(&mut page, "r.length > 0");
    assert_eq!(
        eval(&mut page, "r.join(',') + ' ' + held.length"),
        "seventh 6"
    );
}

/// XHR §4.5.2 `setRequestHeader()` step 6: a header set twice is combined into one, its values
/// joined with ", ", keeping the name as script wrote it.
#[test]
fn xhr_set_request_header_combines() {
    let base = serve();
    let (mut page, _net) = page_at(&base);
    let got = eval(
        &mut page,
        r#"
        const x = new XMLHttpRequest();
        x.open('GET', '/echo', false);
        x.setRequestHeader('X-Pink-Unicorn', 't1');
        x.setRequestHeader('x-pink-unicorn', 't2');
        x.send();
        x.responseText
        "#,
    );
    assert_eq!(got, "X-Pink-Unicorn: t1, t2|");
}

/// XHR §4.5.6 `send()` step 4.5: for a string or document body, every `charset` parameter of the
/// author's `Content-Type` that is not UTF-8 becomes UTF-8 (the MIME type is reserialized);
/// other bodies keep it as written.
#[test]
fn xhr_send_sets_the_authors_charset_to_utf8() {
    let base = serve();
    let (mut page, _net) = page_at(&base);
    let got = eval(
        &mut page,
        r#"
        const out = [];
        for (const [type, body] of [
          ['text/plain;charset=shift-jis', 'x'],
          ['text/x-pink-unicorn; charset=windows-1252; charset=bogus; notrelated; charset=ascii', 'x'],
          ['text/plain;charset= utf-8', 'x'],
          ['text/plain; charset=utf-8', 'x'],
          ['text/plain;charset=shift-jis', new Blob(['x'])],
        ]) {
          const x = new XMLHttpRequest();
          x.open('POST', '/echo', false);
          x.setRequestHeader('Content-Type', type);
          x.send(body);
          out.push(x.responseText);
        }
        out.join('')
        "#,
    );
    assert_eq!(
        got,
        "Content-Type: text/plain;charset=UTF-8|\
         Content-Type: text/x-pink-unicorn;charset=UTF-8|\
         Content-Type: text/plain;charset=UTF-8|\
         Content-Type: text/plain; charset=utf-8|\
         Content-Type: text/plain;charset=shift-jis|"
    );
}
