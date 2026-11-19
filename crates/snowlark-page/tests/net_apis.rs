// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Networking and security APIs: import maps (HTML §8.1.5.3), the exposure of members by global
//! (`[Exposed]`, `SharedArrayBuffer` without cross-origin isolation, Location's own members).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// An HTTP/1.1 server of fixed files on a loopback port; returns its origin.
fn serve(files: Vec<(&'static str, &'static str, &'static str)>) -> String {
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
            let mut length = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let path = first.split(' ').nth(1).unwrap_or("/");
            let path = path.split('?').next().unwrap_or("/");
            let response = match files.iter().find(|(p, _, _)| *p == path) {
                Some((_, mime, body)) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
                None => {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                }
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn page_at(base: &str, path: &str) -> (Page, snowlark_net::NetThread) {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}{path}")).unwrap());
    page.run_until_settled(Duration::from_secs(10));
    (page, net)
}

fn wait_for(page: &mut Page, condition: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let js = format!("String(!!({condition}))");
    // (run_until returns when the page is idle: a peer may still be about to answer)
    while !page.run_until(deadline, |p| p.eval(&js, "t").is_ok_and(|v| v == "true"))
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn import_maps_resolve_bare_specifiers() {
    let base = serve(vec![
        (
            "/index.html",
            "text/html",
            r#"<!doctype html>
            <script>var log = [];</script>
            <script type="importmap">{"imports": {"lib": "/lib/main.js", "lib/": "/lib/", "dup": "/a.js"}}</script>
            <script type="importmap">{"imports": {"dup": "/b.js", "other": "/lib/other.js"},
                                      "scopes": {"/lib/": {"lib": "/lib/scoped.js"}}}</script>
            <script type="importmap">{"imports": </script>
            <script type="module">
              import main from "lib";
              import other from "other";
              log.push(main, other, import.meta.resolve("lib/x.js"), import.meta.resolve("dup"));
              import("lib/dyn.js").then(m => log.push(m.default));
              import("nope").catch(e => log.push(e.name));
            </script>
            <script type="module">import "unmapped";</script>"#,
        ),
        (
            "/lib/main.js",
            "text/javascript",
            "import s from 'lib'; export default 'main+' + s;",
        ),
        (
            "/lib/scoped.js",
            "text/javascript",
            "export default 'scoped';",
        ),
        (
            "/lib/other.js",
            "text/javascript",
            "export default 'other';",
        ),
        ("/lib/dyn.js", "text/javascript", "export default 'dyn';"),
    ]);
    let (mut page, _net) = page_at(&base, "/index.html");
    eval(
        &mut page,
        "var errors = []; window.onerror = (m) => errors.push(m); 'ok'",
    );
    wait_for(&mut page, "log.length >= 6");
    let got = eval(&mut page, "log.join('|')");
    let mut parts: Vec<&str> = got.split('|').collect();
    let tail: Vec<&str> = parts.split_off(4);
    assert_eq!(
        parts,
        [
            "main+scoped",
            "other",
            format!("{base}/lib/x.js").as_str(),
            format!("{base}/a.js").as_str(),
        ]
    );
    let mut tail = tail;
    tail.sort();
    assert_eq!(tail, ["TypeError", "dyn"]);
}

/// "Prepare the script element" 11-15: only the classic check strips whitespace from the type attribute;
/// `module` and `importmap` are matched as written.
#[test]
fn script_types_other_than_classic_are_matched_unstripped() {
    let base = serve(vec![(
        "/index.html",
        "text/html",
        r#"<!doctype html>
        <script>var log = [];</script>
        <script type=" text/javascript ">log.push('classic');</script>
        <script type="Module">log.push('module');</script>
        <script type="module ">log.push('module-space');</script>
        <script type=" module">log.push('space-module');</script>
        <script type="importmap ">{"imports": {"x": "/x.js"}}</script>
        <script type="module">
          try { import.meta.resolve("x"); log.push('mapped'); } catch (e) { log.push(e.name); }
          log.push('end');
        </script>"#,
    )]);
    let (mut page, _net) = page_at(&base, "/index.html");
    wait_for(&mut page, "log.includes('end')");
    assert_eq!(
        eval(&mut page, "log.join()"),
        "classic,module,TypeError,end"
    );
}

#[test]
fn bare_specifier_failure_is_reported_not_an_error_event() {
    let base = serve(vec![(
        "/index.html",
        "text/html",
        r#"<!doctype html>
        <script>
          var log = [];
          window.addEventListener('error', e => log.push('window:' + e.error.name));
        </script>
        <script type="module" onerror="log.push('script-error')">import "unmapped"; log.push('ran');</script>"#,
    )]);
    let (mut page, _net) = page_at(&base, "/index.html");
    wait_for(&mut page, "log.length >= 1");
    assert_eq!(eval(&mut page, "log.join()"), "window:TypeError");
}

#[test]
fn members_are_exposed_by_global() {
    let base = serve(vec![
        ("/index.html", "text/html", "<!doctype html>"),
        (
            "/w.js",
            "text/javascript",
            r#"postMessage([typeof CSS, 'responseXML' in XMLHttpRequest.prototype,
                           'responseText' in XMLHttpRequest.prototype, typeof SharedArrayBuffer].join());"#,
        ),
    ]);
    let (mut page, _net) = page_at(&base, "/index.html");
    eval(
        &mut page,
        "var log = []; new Worker('/w.js').onmessage = e => log.push(e.data); 'ok'",
    );
    wait_for(&mut page, "log.length >= 1");
    assert_eq!(eval(&mut page, "log[0]"), "undefined,false,true,undefined");
    assert_eq!(
        eval(
            &mut page,
            "[typeof CSS, 'responseXML' in XMLHttpRequest.prototype, typeof SharedArrayBuffer, \
             Object.hasOwn(location, 'toString'), Object.hasOwn(Location.prototype, 'toString'), \
             Object.getOwnPropertyDescriptor(location, 'toString').configurable, String(location) === location.href].join()"
        ),
        "object,true,undefined,true,false,false,true"
    );
}

/// A WebSocket echo server for one connection (RFC 6455, server side): echoes text and binary
/// messages; answers a Close frame with the same code and closes. Returns its port.
fn ws_echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut key = String::new();
        let mut protocol = None;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                break;
            }
            let lower = h.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("sec-websocket-key:") {
                key = h[h.len() - v.len()..].trim().to_owned();
            }
            if let Some(v) = lower.strip_prefix("sec-websocket-protocol:") {
                protocol = v.split(',').next().map(|p| p.trim().to_owned());
            }
        }
        let accept = snowlark_net::websocket::accept_key(&key);
        let mut response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n"
        );
        if let Some(p) = protocol {
            response.push_str(&format!("Sec-WebSocket-Protocol: {p}\r\n"));
        }
        response.push_str("\r\n");
        stream.write_all(response.as_bytes()).unwrap();
        loop {
            let mut head = [0u8; 2];
            if reader.read_exact(&mut head).is_err() {
                return;
            }
            let opcode = head[0] & 0x0f;
            let mut len = (head[1] & 0x7f) as usize;
            if len == 126 {
                let mut b = [0u8; 2];
                reader.read_exact(&mut b).unwrap();
                len = u16::from_be_bytes(b) as usize;
            } else if len == 127 {
                let mut b = [0u8; 8];
                reader.read_exact(&mut b).unwrap();
                len = u64::from_be_bytes(b) as usize;
            }
            let mut mask = [0u8; 4];
            reader.read_exact(&mut mask).unwrap();
            let mut payload = vec![0u8; len];
            reader.read_exact(&mut payload).unwrap();
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
            let mut frame = vec![0x80 | opcode];
            if len < 126 {
                frame.push(len as u8);
            } else {
                frame.push(126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            frame.extend_from_slice(&payload);
            stream.write_all(&frame).unwrap();
            if opcode == 8 {
                return;
            }
        }
    });
    port
}

#[test]
fn websocket_echo_and_closing_handshake() {
    let base = serve(vec![("/index.html", "text/html", "<!doctype html>")]);
    let port = ws_echo_server();
    let (mut page, _net) = page_at(&base, "/index.html");
    eval(
        &mut page,
        &format!(
            r#"
            var log = [];
            const ws = new WebSocket('ws://127.0.0.1:{port}/echo', ['chat', 'v2']);
            ws.binaryType = 'arraybuffer';
            log.push('state:' + ws.readyState + ':' + ws.url);
            try {{ ws.send('early'); }} catch (e) {{ log.push(e.name); }}
            ws.onopen = () => {{
                log.push('open:' + ws.protocol + ':' + ws.readyState);
                ws.send('hi');
                ws.send(new Uint8Array([1, 2, 3]));
                log.push('buffered:' + ws.bufferedAmount);
            }};
            ws.onmessage = e => {{
                log.push(typeof e.data === 'string' ? 'text:' + e.data + ':' + e.origin
                                                   : 'bin:' + new Uint8Array(e.data).join(','));
                if (typeof e.data !== 'string') ws.close(3001, 'bye');
            }};
            ws.onerror = () => log.push('error');
            ws.onclose = e => log.push('close:' + e.wasClean + ':' + e.code + ':' + e.reason + ':' + ws.readyState);
            'ok'
            "#
        ),
    );
    wait_for(&mut page, "log.some(l => l.startsWith('close'))");
    assert_eq!(
        eval(&mut page, "log.join('|')"),
        format!(
            "state:0:ws://127.0.0.1:{port}/echo|InvalidStateError|open:chat:1|buffered:5|text:hi:ws://127.0.0.1:{port}|bin:1,2,3|close:true:3001:bye:3"
        )
    );
}

#[test]
fn websocket_constructor_and_close_checks() {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    page.run_until_settled(Duration::from_secs(5));
    let r = eval(
        &mut page,
        r#"
        const err = f => { try { f(); return 'ok'; } catch (e) { return e.name; } };
        [err(() => new WebSocket('ftp://example.test/')),
         err(() => new WebSocket('ws://example.test/#x')),
         err(() => new WebSocket('ws://example.test/', ['a', 'a'])),
         err(() => new WebSocket('ws://example.test/', 'a b')),
         new WebSocket('http://example.test/x').url,
         new WebSocket('/rel').url,
         err(() => new WebSocket('ws://example.test/').close(1001)),
         err(() => new WebSocket('ws://example.test/').close(1000, 'x'.repeat(124))),
         new CloseEvent('close', {code: 4000, reason: 'r', wasClean: true}).code,
         WebSocket.CLOSING].join()
        "#,
    );
    assert_eq!(
        r,
        "SyntaxError,SyntaxError,SyntaxError,SyntaxError,ws://example.test/x,ws://example.test/rel,InvalidAccessError,SyntaxError,4000,2"
    );
}

#[test]
fn broadcast_channel_reaches_workers_and_other_channels() {
    let base = serve(vec![
        ("/index.html", "text/html", "<!doctype html>"),
        (
            "/w.js",
            "text/javascript",
            r#"const c = new BroadcastChannel('news');
               c.onmessage = e => c.postMessage('worker got ' + e.data.n + ' from ' + e.origin);
               postMessage('ready');"#,
        ),
    ]);
    let (mut page, _net) = page_at(&base, "/index.html");
    eval(
        &mut page,
        r#"
        var log = [];
        const a = new BroadcastChannel('news'), b = new BroadcastChannel('news'),
              other = new BroadcastChannel('other');
        other.onmessage = () => log.push('wrong channel');
        a.onmessage = e => log.push('a:' + e.data);
        b.onmessage = e => log.push('b:' + (typeof e.data === 'string' ? e.data : e.data.n));
        const w = new Worker('/w.js');
        w.onmessage = () => a.postMessage({n: 7});
        'ok'
        "#,
    );
    wait_for(&mut page, "log.length >= 3");
    let mut got: Vec<String> = eval(&mut page, "log.join('|')")
        .split('|')
        .map(str::to_owned)
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            format!("a:worker got 7 from {base}"),
            "b:7".to_string(),
            format!("b:worker got 7 from {base}"),
        ]
    );
    let r = eval(
        &mut page,
        "a.close(); const e = (() => { try { a.postMessage(1); } catch (x) { return x.name; } })(); [a.name, e].join()",
    );
    assert_eq!(r, "news,InvalidStateError");
}

/// URL Standard §4.4: parsing a `blob:` URL resolves its blob URL entry, and Fetch's scheme fetch
/// reads that entry, so a worker, fetch, Request, XHR or frame made of a blob URL reads the blob
/// even when the URL is revoked right after; only the exact URL revokes (File API §8.3.2); and a
/// blob worker script needs no JavaScript MIME type (HTML "fetch a classic worker script").
#[test]
fn blob_urls_revoked_right_after_use_still_fetch() {
    let base = serve(vec![(
        "/",
        "text/html",
        r#"<!doctype html><script>
        window.result = {};
        const u1 = URL.createObjectURL(new Blob(["onmessage = e => postMessage('w:' + e.data)"], {type: 'text/plain'}));
        const w = new Worker(u1);
        URL.revokeObjectURL(u1);
        w.onmessage = e => { result.worker = e.data; };
        w.onerror = () => { result.worker = 'error'; };
        w.postMessage('hi');
        const u2 = URL.createObjectURL(new Blob(["fetched"]));
        fetch(u2).then(r => r.text()).then(t => { result.fetch = t; }, e => { result.fetch = 'error'; });
        URL.revokeObjectURL(u2);
        const u3 = URL.createObjectURL(new Blob(["requested"]));
        const req = new Request(u3);
        URL.revokeObjectURL(u3);
        Promise.all([fetch(req.clone()), fetch(req)]).then(rs => Promise.all(rs.map(r => r.text())))
            .then(ts => { result.request = ts.join('+'); }, e => { result.request = 'error'; });
        const u4 = URL.createObjectURL(new Blob(["xhrd"]));
        const x = new XMLHttpRequest();
        x.open('GET', u4);
        URL.revokeObjectURL(u4);
        x.onload = () => { result.xhr = x.responseText; };
        x.onerror = () => { result.xhr = 'error'; };
        x.send();
        const u5 = URL.createObjectURL(new Blob(["<!doctype html><script>window.test_result = 'framed'<\/script>"], {type: 'text/html'}));
        const f = document.createElement('iframe');
        f.src = u5;
        f.onload = () => { result.frame = f.contentWindow.test_result; };
        document.documentElement.appendChild(f);
        URL.revokeObjectURL(u5);
        const u7 = URL.createObjectURL(new Blob(["x = 1"], {type: 'text/plain'}));
        const w2 = new Worker(URL.createObjectURL(new Blob(["try { importScripts('" + u7 + "'); postMessage('imported') } catch (e) { postMessage('import ' + e.name) }"])));
        w2.onmessage = e => { result.imports = e.data; };
        const u6 = URL.createObjectURL(new Blob(["exact"]));
        URL.revokeObjectURL(u6 + '#fragment');
        fetch(u6).then(r => r.text()).then(t => { result.exact = t; }, e => { result.exact = 'error'; });
        </script>"#,
    )]);
    let (mut page, _net) = page_at(&base, "/");
    wait_for(&mut page, "Object.keys(result).length === 7");
    assert_eq!(
        eval(
            &mut page,
            "[result.worker, result.fetch, result.request, result.xhr, result.frame, result.exact, result.imports].join(' ')"
        ),
        // (importScripts keeps HTML's MIME requirement, which the main script's fetch has for
        // HTTP(S) only)
        "w:hi fetched requested+requested xhrd framed exact import NetworkError"
    );
}

/// HTML "prepare the script element" and "create a link request" make a potential-CORS request of
/// the `crossorigin` attribute, and SRI §3.3.5 checks `integrity` against a basic or CORS response
/// only: a cross-origin script or sheet with `crossorigin` and the right digest loads, one without
/// `crossorigin` is opaque and fails, a wrong digest fails, and malformed metadata checks nothing.
#[test]
fn scripts_and_sheets_follow_crossorigin_and_integrity() {
    let script = "window.ran = (window.ran || 0) + 1";
    let sheet = "body { color: rgb(1, 2, 3) }";
    let digest = |s: &str| snowlark_net::csp::HashAlgorithm::Sha256.digest_base64(s.as_bytes());
    // (the second header rides on the content type: the server writes it as given)
    let other = serve(vec![
        (
            "/s.js",
            "text/javascript\r\nAccess-Control-Allow-Origin: *",
            script,
        ),
        ("/plain.js", "text/javascript", "window.plain = true"),
        (
            "/s.css",
            "text/css\r\nAccess-Control-Allow-Origin: *",
            sheet,
        ),
        (
            "/plain.css",
            "text/plain\r\nAccess-Control-Allow-Origin: *",
            sheet,
        ),
    ]);
    let html = format!(
        r#"<!doctype html>
        <script src="{other}/s.js" crossorigin="anonymous" integrity="sha256-{js}"></script>
        <script src="{other}/s.js" integrity="sha256-{js}" onerror="window.nocors = 'error'"></script>
        <script src="{other}/plain.js" integrity="sha256- sha256-..."></script>
        <link rel="stylesheet" href="{other}/s.css" crossorigin="anonymous" integrity="sha256-{css}" onload="window.sheetok = true">
        <link rel="stylesheet" href="{other}/s.css" crossorigin="anonymous" integrity="sha256-AAAA" onerror="window.sheet = 'error'">
        <link rel="stylesheet" href="{other}/plain.css" crossorigin="anonymous" onload="window.plaincss = 'load'" onerror="window.plaincss = 'error'">
        <script>
        // (a document without a browsing context fetches no linked resource)
        const d = document.implementation.createHTMLDocument();
        const l = d.createElement('link');
        l.rel = 'stylesheet';
        l.href = '{other}/s.css';
        l.onload = l.onerror = e => {{ window.nobc = e.type; }};
        d.head.appendChild(l);
        </script>
        "#,
        js = digest(script),
        css = digest(sheet),
    );
    let base = serve(vec![("/", "text/html", Box::leak(html.into_boxed_str()))]);
    let (mut page, _net) = page_at(&base, "/");
    wait_for(
        &mut page,
        "window.ran === 1 && window.nocors === 'error' && window.plain && window.sheetok && window.sheet === 'error' && window.plaincss",
    );
    assert_eq!(
        eval(
            &mut page,
            "[window.ran, window.nocors, window.plain, window.sheetok, window.sheet, window.plaincss, window.nobc].join(' ')"
        ),
        // (a sheet of another type than text/css errors outside quirks mode, CORS or not; the
        // context-less document's link fires nothing)
        "1 error true true error error "
    );
}

#[test]
fn message_ports_transfer_and_blobs_clone() {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    page.run_until_settled(Duration::from_secs(5));
    eval(
        &mut page,
        r#"
        var log = [];
        const outer = new MessageChannel(), inner = new MessageChannel();
        // A message waiting in the transferred port's queue goes with it.
        inner.port1.postMessage('queued');
        outer.port2.onmessage = e => {
            const p = e.ports[0];
            log.push('ports:' + e.ports.length + ':' + (p instanceof MessagePort) + ':' + (p !== inner.port2));
            p.onmessage = m => log.push('via:' + m.data);
            inner.port1.postMessage('later');
        };
        outer.port1.postMessage('take', [inner.port2]);
        const err = f => { try { f(); return 'ok'; } catch (e) { return e.name; } };
        log.push('again:' + err(() => outer.port1.postMessage('x', [inner.port2])));
        log.push('dup:' + err(() => { const c = new MessageChannel(); outer.port1.postMessage('x', [c.port1, c.port1]); }));
        log.push('untransferred:' + err(() => outer.port1.postMessage(new MessageChannel().port1)));
        const blobs = new MessageChannel();
        blobs.port2.onmessage = e => e.data.text().then(t =>
            log.push('blob:' + t + ':' + e.data.type + ':' + (e.data instanceof File) + ':' + e.data.name));
        blobs.port1.postMessage(new File(['hello'], 'f.txt', {type: 'text/plain'}));
        'ok'
        "#,
    );
    wait_for(&mut page, "log.length >= 7");
    let mut got: Vec<String> = eval(&mut page, "log.join('|')")
        .split('|')
        .map(str::to_owned)
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            "again:DataCloneError",
            "blob:hello:text/plain:true:f.txt",
            "dup:DataCloneError",
            "ports:1:true:true",
            "untransferred:DataCloneError",
            "via:later",
            "via:queued",
        ]
    );
}

#[test]
fn urls_encoding_parsed_with_the_document_encoding() {
    // HTML "encoding-parsing a URL": a hyperlink's query is percent-encoded in the document's
    // encoding (the fragment stays UTF-8); the URL API itself is UTF-8.
    let base = serve(vec![(
        "/",
        "text/html; charset=windows-1252",
        "<!doctype html><a href='?&#x2020;#&#x2020;'>x</a>",
    )]);
    let (mut page, _net) = page_at(&base, "/");
    let r = eval(
        &mut page,
        "const a = document.querySelector('a'); [document.characterSet, a.search, a.hash, new URL('?\\u2020', location.href).search].join()",
    );
    assert_eq!(r, "windows-1252,?%86,#%E2%80%A0,?%E2%80%A0");
    // URL "percent-encode after encoding": a code point the encoding cannot represent.
    let r = eval(&mut page, "a.setAttribute('href', '?\\u3042'); a.search");
    assert_eq!(r, "?%26%2312354%3B");
    // Web IDL record conversion: keys that convert to the same USVString keep one entry, the last
    // value, at the first one's place.
    let r = eval(
        &mut page,
        "[...new URLSearchParams({'\\uD835x': '1', 'xx': '2', '\\uD83Dx': '3'})].join(';')",
    );
    assert_eq!(r, "\u{FFFD}x,3;xx,2");
}

/// Like [`serve`], one thread per connection, and `/slow` answers after `slow`.
fn serve_with_slow(page: &'static str, slow: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    return;
                }
                let mut length = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let path = first.split(' ').nth(1).unwrap_or("/");
                if path.starts_with("/slow") {
                    std::thread::sleep(slow);
                }
                let (mime, body) = if path == "/" {
                    ("text/html", page)
                } else {
                    ("text/plain", "ok")
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            });
        }
    });
    format!("http://{addr}")
}

/// Fetch, HTTP-network-or-cache fetch: a document's keepalive fetches and beacons share 64 KiB of
/// request bodies in flight; one over it is a network error (`sendBeacon` returns false), and the
/// bytes come back when a fetch ends. Fetches without the flag are not limited.
#[test]
fn keepalive_fetches_and_beacons_share_a_64_kib_quota() {
    let base = serve_with_slow("<!doctype html><p>quota", Duration::from_millis(800));
    let (mut page, _net) = page_at(&base, "/");
    eval(
        &mut page,
        r#"
        window.r = [];
        const post = (url, n, keepalive) => fetch(url, {method: 'POST', body: '*'.repeat(n), keepalive})
            .then(() => 'ok', e => e.name);
        post('/x', 65537, true).then(v => r.push('over:' + v));
        post('/slow', 65535, true).then(v => r.push('fill:' + v));
        r.push('beacon1:' + navigator.sendBeacon('/x', '*'));
        r.push('beacon2:' + navigator.sendBeacon('/x', '*'));
        post('/x', 2, true).then(v => r.push('full:' + v));
        post('/x', 200000, false).then(v => r.push('plain:' + v));
        'started'
        "#,
    );
    wait_for(&mut page, "r.includes('fill:ok') && r.length == 6");
    eval(
        &mut page,
        "post('/x', 65536, true).then(v => r.push('after:' + v)); 'again'",
    );
    wait_for(&mut page, "r.length == 7");
    let mut got: Vec<String> = eval(&mut page, "r.join(',')")
        .split(',')
        .map(String::from)
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            "after:ok",
            "beacon1:true",
            "beacon2:false",
            "fill:ok",
            "full:TypeError",
            "over:TypeError",
            "plain:ok"
        ]
    );
}

/// HTML §10.2.4 "run a worker" with `type: "module"`: the main script is a module script graph
/// (its static imports fetched and linked), run with the worker's global as `self`; the parent's
/// messages posted before the graph loaded wait for it (the port message queue is enabled after
/// the script runs); `importScripts` is a TypeError there; a graph that fails to load fires a
/// plain `error` event at the Worker object.
#[test]
fn module_workers_run_their_module_graph() {
    let base = serve(vec![
        ("/index.html", "text/html", "<!doctype html>"),
        (
            "/m.js",
            "text/javascript",
            r#"import { dep } from './dep.js';
               let imported;
               try { importScripts('/dep.js'); imported = 'ran'; } catch (e) { imported = e.name; }
               onmessage = e => postMessage([e.data, dep, imported, typeof DedicatedWorkerGlobalScope].join());"#,
        ),
        ("/dep.js", "text/javascript", "export const dep = 'dep';"),
        ("/broken.js", "text/javascript", "import './missing.js';"),
    ]);
    let (mut page, _net) = page_at(&base, "/index.html");
    eval(
        &mut page,
        "var log = []; const w = new Worker('/m.js', { type: 'module' }); \
         w.onmessage = e => log.push(e.data); w.postMessage('early'); \
         const b = new Worker('/broken.js', { type: 'module' }); \
         b.onerror = e => log.push(e.constructor.name); 'ok'",
    );
    wait_for(&mut page, "log.length >= 2");
    let mut got: Vec<String> = eval(&mut page, "log.join('|')")
        .split('|')
        .map(str::to_owned)
        .collect();
    got.sort();
    assert_eq!(got, ["Event", "early,dep,TypeError,function"]);
}
