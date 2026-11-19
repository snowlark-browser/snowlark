// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Streams Standard, `Response.body`, `Blob.stream()`, TextDecoderStream/TextEncoderStream, and
//! CSSOM View's `screen` and `visualViewport` (what Reddit's feed needed to load more posts: its
//! modules read `screen.height`, and its partials stream `response.body` through a
//! TextDecoderStream).

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn page(net: &NetThread, html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
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

/// Runs an async script that stores its result in `window.out`, then reads it.
fn run_async(page: &mut Page, js: &str) -> String {
    eval(
        page,
        &format!("window.out = 'pending'; (async () => {{ {js} }})().then(v => out = String(v), e => out = 'threw ' + e);"),
    );
    page.run_until_settled(Duration::from_secs(5));
    eval(page, "out")
}

#[test]
fn an_auto_allocated_buffer_too_large_to_allocate_rejects_the_read() {
    // Streams §4.9 [[PullSteps]]: the ArrayBuffer constructor's RangeError goes to the read request;
    // the size is the page's, so a failed allocation must not end the process.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<!doctype html>");
    let r = run_async(
        &mut p,
        r#"
        const stream = new ReadableStream({ type: 'bytes', autoAllocateChunkSize: 2 ** 50 });
        return await stream.getReader().read().then(() => 'read', e => e.name);
        "#,
    );
    assert_eq!(r, "RangeError");
}

#[test]
fn readable_streams_read_tee_and_cancel() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<!doctype html>");
    let r = run_async(
        &mut p,
        r#"
        const out = [];
        let pulls = 0;
        const rs = new ReadableStream({
          start(c) { c.enqueue('a'); },
          pull(c) { pulls++; if (pulls < 3) c.enqueue('p' + pulls); else c.close(); },
        }, { highWaterMark: 1 });
        const r = rs.getReader();
        out.push(rs.locked);
        let x;
        while (!(x = await r.read()).done) out.push(x.value);
        out.push(await r.closed === undefined);
        r.releaseLock();
        out.push(rs.locked);
        const [b1, b2] = new ReadableStream({ start(c) { c.enqueue(1); c.enqueue(2); c.close(); } }).tee();
        const r1 = b1.getReader(), r2 = b2.getReader();
        out.push((await r1.read()).value + (await r1.read()).value, (await r2.read()).value);
        let reason;
        const c = new ReadableStream({ cancel(why) { reason = why; } });
        await c.cancel('stop');
        out.push(reason);
        try { new ReadableStream().getReader().releaseLock(), new ReadableStream({ type: 'nope' }); }
        catch (e) { out.push(e.name); }
        const err = new ReadableStream({ start(c) { c.error(new Error('boom')); } });
        try { await err.getReader().read(); } catch (e) { out.push(e.message); }
        return out.join('|');
        "#,
    );
    assert_eq!(r, "true|a|p1|p2|true|false|3|1|stop|TypeError|boom");
}

#[test]
fn writable_and_transform_streams_and_pipes() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<!doctype html>");
    let r = run_async(
        &mut p,
        r#"
        const out = [];
        const chunks = [];
        let closed = false;
        const ws = new WritableStream({ write(c) { chunks.push(c); }, close() { closed = true; } });
        await new ReadableStream({ start(c) { c.enqueue(7); c.enqueue(8); c.close(); } }).pipeTo(ws);
        out.push(chunks.join(','), closed);
        const ts = new TransformStream({ transform(ch, c) { c.enqueue(ch.toUpperCase()); } });
        const w = ts.writable.getWriter();
        w.write('x'); w.write('y'); w.close();
        const r = ts.readable.getReader();
        let s = '', x;
        while (!(x = await r.read()).done) s += x.value;
        out.push(s);
        const piped = new ReadableStream({ start(c) { c.enqueue('q'); c.close(); } })
          .pipeThrough(new TransformStream());
        out.push((await piped.getReader().read()).value);
        // An aborted pipe rejects with the signal's reason and cancels the source.
        const ac = new AbortController();
        let canceled;
        const p2 = new ReadableStream({ cancel(r) { canceled = r; } })
          .pipeTo(new WritableStream(), { signal: ac.signal });
        ac.abort('why');
        try { await p2; } catch (e) { out.push('aborted:' + e); }
        out.push(canceled);
        const wr = new WritableStream({ write() { throw new Error('sink'); } }).getWriter();
        try { await wr.write(1); } catch (e) { out.push(e.message); }
        out.push(new CountQueuingStrategy({ highWaterMark: 3 }).size(),
                 new ByteLengthQueuingStrategy({ highWaterMark: 1 }).size(new Uint8Array(5)),
                 new CountQueuingStrategy({ highWaterMark: 1 }).size === new CountQueuingStrategy({ highWaterMark: 2 }).size);
        return out.join('|');
        "#,
    );
    assert_eq!(r, "7,8|true|XY|q|aborted:why|why|sink|1|5|true");
}

#[test]
fn bodies_and_encoding_streams() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<!doctype html>");
    let r = run_async(
        &mut p,
        r#"
        const out = [];
        const res = new Response('héllo');
        const r = res.body.pipeThrough(new TextDecoderStream()).getReader();
        let t = '', x;
        while (!(x = await r.read()).done) t += x.value;
        out.push(t, res.bodyUsed, res.body === res.body);
        try { await res.text(); } catch (e) { out.push(e.name); }
        const fromStream = new Response(new ReadableStream({ start(c) {
          c.enqueue(new TextEncoder().encode('ab')); c.enqueue(new TextEncoder().encode('c')); c.close();
        } }));
        const clone = fromStream.clone();
        out.push(await fromStream.text(), await clone.text());
        out.push(await new Response(new Blob(['blob']).stream()).text());
        out.push(String(new Response(null).body));
        const enc = new ReadableStream({ start(c) { c.enqueue('a\ud83d'); c.enqueue('\ude00'); c.close(); } })
          .pipeThrough(new TextEncoderStream()).getReader();
        const bytes = [];
        while (!(x = await enc.read()).done) bytes.push(...x.value);
        out.push(bytes.join(' '));
        const d = new TextDecoderStream('latin1', { fatal: false });
        out.push(d.encoding, d.fatal, d.ignoreBOM, d.readable instanceof ReadableStream);
        return out.join('|');
        "#,
    );
    assert_eq!(
        r,
        "h\u{e9}llo|true|true|TypeError|abc|abc|blob|null|97 240 159 152 128|windows-1252|false|false|true"
    );
}

/// Fetch: `Request.clone()` clones the body, so the clone and the original both read it, and a clone
/// taken of a used body throws.
#[test]
fn a_cloned_request_reads_its_body() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<!doctype html>");
    let r = run_async(
        &mut p,
        r#"
        const out = [];
        const post = body => new Request('http://example.test/', { method: 'POST', body });
        const a = post(new Uint8Array([1, 2, 3]));
        const c = a.clone();
        out.push(a.bodyUsed, c.bodyUsed);
        out.push((await c.arrayBuffer()).byteLength, (await a.arrayBuffer()).byteLength);
        out.push(a.bodyUsed, c.bodyUsed);
        out.push(await post('abc').clone().text());
        const s = post('xyz');
        s.body;
        out.push(await s.clone().text(), await s.text());
        try { a.clone(); out.push('no throw'); } catch (e) { out.push(e.name); }
        return out.join('|');
        "#,
    );
    assert_eq!(r, "false|false|3|3|true|true|abc|xyz|xyz|TypeError");
}

#[test]
fn compression_streams() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<!doctype html>");
    let r = run_async(
        &mut p,
        r#"
        const out = [];
        async function bytes(readable) {
          const r = readable.getReader(), all = [];
          let x;
          while (!(x = await r.read()).done) all.push(...x.value);
          return new Uint8Array(all);
        }
        const input = new TextEncoder().encode('hello hello hello hello');
        for (const format of ['gzip', 'deflate', 'deflate-raw', 'brotli']) {
          const c = new Response(input).body.pipeThrough(new CompressionStream(format));
          const z = await bytes(c);
          const d = new Response(z).body.pipeThrough(new DecompressionStream(format));
          out.push(format + ':' + new TextDecoder().decode(await bytes(d)));
        }
        out.push([...(await bytes(new Response(input).body.pipeThrough(new CompressionStream('gzip')))).slice(0, 3)].join(' '));
        try { new CompressionStream('zip'); } catch (e) { out.push(e.name); }
        // Data after the end: the output first, then a TypeError.
        const z = await bytes(new Response(input).body.pipeThrough(new CompressionStream('deflate')));
        const ds = new DecompressionStream('deflate');
        const w = ds.writable.getWriter(), r = ds.readable.getReader();
        w.write(new Uint8Array([...z, 0])).catch(() => {});
        out.push((await r.read()).value.length);
        try { await r.read(); } catch (e) { out.push(e.name); }
        // (a write waits for a read: the readable side's high water mark is 0)
        const bad = new DecompressionStream('gzip');
        bad.readable.getReader().read().catch(() => {});
        try { await bad.writable.getWriter().write('text'); } catch (e) { out.push(e.name); }
        return out.join('|');
        "#,
    );
    assert_eq!(
        r,
        "gzip:hello hello hello hello|deflate:hello hello hello hello|deflate-raw:hello hello hello hello|brotli:hello hello hello hello|31 139 8|TypeError|23|TypeError|TypeError"
    );
}

#[test]
fn screen_and_visual_viewport() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><body style='margin:0'><div style='height:3000px'></div>",
    );
    let r = eval(
        &mut p,
        r#"
        scrollTo(0, 500);
        [screen.width, screen.height, screen.availWidth, screen.colorDepth, screen.orientation.type,
         screen === window.screen, visualViewport.width > 700, visualViewport.height,
         visualViewport.pageTop, visualViewport.scale, visualViewport.offsetTop].join('|')
        "#,
    );
    assert_eq!(r, "800|600|800|24|landscape-primary|true|true|600|500|1|0");
}

#[test]
fn byte_streams_byob_readers_and_byte_tee() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<!doctype html>");
    let r = run_async(
        &mut p,
        r#"
        const out = [];
        // A BYOB read is filled through byobRequest; the view's buffer is transferred.
        const rs = new ReadableStream({
          type: 'bytes',
          pull(c) {
            const v = c.byobRequest.view;
            v[0] = 7; v[1] = 8;
            c.byobRequest.respond(2);
          },
        });
        const reader = rs.getReader({ mode: 'byob' });
        const view = new Uint8Array(4);
        const { value, done } = await reader.read(view);
        out.push(view.byteLength, value.length, value[0], value[1], done);
        // Enqueued chunks fill a read with `min`; the rest stays queued.
        let ctl;
        const rs2 = new ReadableStream({ type: 'bytes', start(c) { ctl = c; } });
        const r2 = rs2.getReader({ mode: 'byob' });
        const p2 = r2.read(new Uint16Array(2), { min: 2 });
        ctl.enqueue(new Uint8Array([1, 0, 2]));
        ctl.enqueue(new Uint8Array([0, 3]));
        const got = await p2;
        out.push(Array.from(got.value).join('-'));
        ctl.close();
        const last = await r2.read(new Uint8Array(4));
        out.push(Array.from(last.value).join('-'), last.done);
        // autoAllocateChunkSize gives default readers a byobRequest too.
        const rs3 = new ReadableStream({
          type: 'bytes', autoAllocateChunkSize: 3,
          pull(c) { c.byobRequest.view.set([9, 9, 9]); c.byobRequest.respond(3); },
        });
        const v3 = await rs3.getReader().read();
        out.push(v3.value.constructor.name, v3.value.length);
        // A byte stream tees into byte streams whose chunks are copies.
        const [a, b] = new ReadableStream({ type: 'bytes', start(c) { c.enqueue(new Uint8Array([5])); c.close(); } }).tee();
        const ra = await a.getReader({ mode: 'byob' }).read(new Uint8Array(2));
        const rb = await b.getReader().read();
        out.push(ra.value[0], rb.value[0], ra.value.buffer !== rb.value.buffer);
        return out.join();
        "#,
    );
    assert_eq!(r, "0,2,7,8,false,1-2,3,false,Uint8Array,3,5,5,true");
}

#[test]
fn async_iteration_and_readable_stream_from() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<!doctype html>");
    let r = run_async(
        &mut p,
        r#"
        const out = [];
        const rs = ReadableStream.from([1, Promise.resolve(2), 3]);
        for await (const v of rs) out.push(v);
        out.push(rs.locked);
        async function* gen() { yield 'a'; yield 'b'; }
        const it = ReadableStream.from(gen()).values({ preventCancel: true });
        out.push((await it.next()).value);
        out.push(JSON.stringify(await it.return('r')));
        out.push(Object.prototype.toString.call(it));
        let cancelled = false;
        const s = new ReadableStream({ pull(c) { c.enqueue(0); }, cancel() { cancelled = true; } });
        for await (const v of s) break;
        out.push(cancelled, s.locked);
        try { ReadableStream.from(5); } catch (e) { out.push(e.name); }
        return out.join();
        "#,
    );
    assert_eq!(
        r,
        "1,2,3,false,a,{\"value\":\"r\",\"done\":true},[object ReadableStream AsyncIterator],true,false,TypeError"
    );
}
