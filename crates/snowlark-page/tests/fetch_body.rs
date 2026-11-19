// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Fetch Body mixin (Fetch §6.2): what reading a Request or Response body gives.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page_with(html: &str) -> Page {
    let mut page = Page::new(PageOptions::default());
    page.load_html(html, &url::Url::parse("http://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// `blob()`'s type is the body's MIME type: "extract a MIME type" from the header list (Fetch
/// §3.1.7), not the combined `Content-Type` value. The last valid value wins, a `*/*` is skipped,
/// and a charset carries over only while the essence stays the same.
#[test]
fn blob_type_is_the_extracted_mime_type_of_the_header_list() {
    let mut page = page_with("<!doctype html>");
    eval(
        &mut page,
        r#"
        globalThis.log = [];
        const cases = [
          [' ', 'text/plain'],
          ['text/html', 'text/plain'],
          ['text/plain;charset=gbk', 'text/plain'],
          ['text/plain;charset=gbk', 'text/html'],
          ['text/html;charset=gbk', '*/*'],
          ['text/plain;charset=gbk', 'text/plain;charset=windows-1252'],
          ['text/html;charset=GBK'],
          ['bogus'],
        ];
        (async () => {
          for (const values of cases) {
            for (const ctor of ['Request', 'Response']) {
              const r = ctor === 'Request' ? new Request('about:blank') : new Response();
              values.forEach(v => r.headers.append('Content-Type', v));
              log.push(ctor + ':' + (await r.blob()).type);
            }
          }
        })();
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    let want = [
        "text/plain",
        "text/plain",
        "text/plain;charset=gbk",
        "text/html",
        "text/html;charset=gbk",
        "text/plain;charset=windows-1252",
        "text/html;charset=GBK",
        "",
    ];
    let want: Vec<String> = want
        .iter()
        .flat_map(|t| [format!("Request:{t}"), format!("Response:{t}")])
        .collect();
    assert_eq!(eval(&mut page, "log.join('|')"), want.join("|"));
}

/// Fetching a blob URL sets `Content-Type` to the blob's type even when it is empty (Fetch's
/// scheme fetch, "blob").
#[test]
fn a_blob_url_response_has_the_blobs_type_even_when_empty() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    eval(
        &mut page,
        r#"
        globalThis.log = [];
        (async () => {
          for (const b of [new Blob([]), new Blob(['x'], { type: 'Text/Plain' })]) {
            const r = await fetch(URL.createObjectURL(b));
            log.push(JSON.stringify(r.headers.get('Content-Type')) + ':' + r.headers.get('Content-Length'));
          }
        })();
        "#,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !page.run_until(deadline, |p| {
        p.eval("String(log.length == 2)", "t")
            .is_ok_and(|v| v == "true")
    }) && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(eval(&mut page, "log.join('|')"), r#""":0|"text/plain":1"#);
}
/// Web IDL's record conversion goes key by key: `[[OwnPropertyKeys]]`, then per key
/// `[[GetOwnProperty]]`, the key's conversion and `[[Get]]`; a symbol key that is enumerable
/// throws, a non-enumerable one is skipped.
#[test]
fn a_record_is_converted_key_by_key() {
    let mut page = page_with("<!doctype html>");
    let got = eval(
        &mut page,
        r#"
        const log = [];
        const handler = {};
        for (const trap of ['get', 'ownKeys', 'getOwnPropertyDescriptor']) {
          handler[trap] = (...args) => {
            const key = args[1];
            if (trap !== 'ownKeys') log.push(trap + ':' + String(key));
            else log.push(trap);
            return Reflect[trap](...args);
          };
        }
        const record = { a: 'b', c: 'd' };
        Object.defineProperty(record, 'hidden', { value: 'x', enumerable: false });
        Object.defineProperty(record, Symbol('s'), { value: 'x', enumerable: false });
        const h = new Headers(new Proxy(record, handler));
        const out = [log.join(','), [...h].join(';')];
        let threw = 'no';
        try { new Headers({ [Symbol('t')]: 'v' }); } catch (e) { threw = e.name; }
        out.push(threw);
        log.length = 0;
        try { new Headers(new Proxy({ a: '\u0100', c: 'd' }, handler)); } catch (e) { out.push(e.name); }
        out.push(log.join(','));
        out.join('|')
        "#,
    );
    assert_eq!(
        got,
        "get:Symbol(Symbol.iterator),ownKeys,getOwnPropertyDescriptor:a,get:a,\
         getOwnPropertyDescriptor:c,get:c,getOwnPropertyDescriptor:hidden,\
         getOwnPropertyDescriptor:Symbol(s)|a,b;c,d|TypeError|TypeError|\
         get:Symbol(Symbol.iterator),ownKeys,getOwnPropertyDescriptor:a,get:a"
    );
}
/// `Response.clone()` tees the body with "clone for branch 2" (Fetch's "clone a body"): the
/// clone reads a structured clone of each chunk, the original the chunk itself; `tee()` from
/// script clones nothing.
#[test]
fn a_response_clone_reads_structured_clones_of_the_chunks() {
    let mut page = page_with("<!doctype html>");
    eval(
        &mut page,
        r#"
        globalThis.log = [];
        (async () => {
          const chunk = new Uint8Array([1, 2, 3]);
          const r = new Response(new ReadableStream({ start(c) { c.enqueue(chunk); c.close(); } }));
          const copy = r.clone();
          const a = (await r.body.getReader().read()).value;
          const b = (await copy.body.getReader().read()).value;
          log.push(a === chunk, b !== chunk && b.buffer !== chunk.buffer, b.join(','));
          const s = new ReadableStream({ start(c) { c.enqueue(chunk); c.close(); } });
          const [t1, t2] = s.tee();
          log.push((await t2.getReader().read()).value === chunk);
        })();
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "log.join('|')"), "true|true|1,2,3|true");
}
/// `new Request(input)` throws when the input's body is unusable: read, or its stream locked by a
/// reader (Fetch §5.4, "if input is unusable, throw a TypeError").
#[test]
fn a_request_with_a_locked_body_cannot_be_copied() {
    let mut page = page_with("<!doctype html>");
    let got = eval(
        &mut page,
        r#"
        const tryCopy = r => { try { new Request(r); return 'ok'; } catch (e) { return e.name; } };
        const make = () => new Request('about:blank', { method: 'POST', body: 'x' });
        const fresh = make();
        const locked = make();
        locked.body.getReader();
        const released = make();
        released.body.getReader().releaseLock();
        [tryCopy(fresh), tryCopy(locked), tryCopy(released)].join('|')
        "#,
    );
    assert_eq!(got, "ok|TypeError|ok");
}
