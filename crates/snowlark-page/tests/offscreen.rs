// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.12.5.3 `OffscreenCanvas`: its 2D and WebGL contexts, `convertToBlob`, a placeholder
//! canvas (`transferControlToOffscreen`) showing the frames it commits, and the transfer of one to
//! a dedicated worker, which draws there (in its animation frame callbacks or out of them).

use std::time::{Duration, Instant};

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

/// Runs the page until `js` evaluates to `want` (at most 10 s); what it last gave.
fn wait_for(page: &mut Page, js: &str, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let v = eval(page, js);
        if v == want || Instant::now() > deadline {
            return v;
        }
        page.run_for(Duration::from_millis(20));
    }
}

/// `px(canvas)`: the top-left pixel of any canvas source, drawn into a 2D canvas and read back.
const PX: &str = "
    function px(src) {
        const c = document.createElement('canvas'); c.width = c.height = 1;
        const ctx = c.getContext('2d'); ctx.drawImage(src, 0, 0);
        return Array.from(ctx.getImageData(0, 0, 1, 1).data).join();
    }
    function tryIt(f) { try { return String(f()); } catch (e) { return e.name; } }
";

#[test]
fn an_offscreen_canvas_draws_in_the_window() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net, "<!doctype html><body>");
    let r = eval(
        &mut page,
        &format!(
            "{PX}
            const oc = new OffscreenCanvas(4, 2);
            const ctx = oc.getContext('2d');
            ctx.fillStyle = '#ff0000'; ctx.fillRect(0, 0, 4, 2);
            const out = [oc.width, oc.height, ctx.canvas === oc, ctx instanceof OffscreenCanvasRenderingContext2D,
                         oc.getContext('2d') === ctx, String(oc.getContext('webgl')),
                         Array.from(ctx.getImageData(0, 0, 1, 1).data).join(), px(oc)];
            // a new size resets the bitmap
            oc.width = 8;
            out.push(Array.from(ctx.getImageData(0, 0, 1, 1).data).join());
            out.join('|')"
        ),
    );
    assert_eq!(r, "4|2|true|true|true|null|255,0,0,255|255,0,0,255|0,0,0,0");
    eval(
        &mut page,
        "window.blob = null;
         const oc2 = new OffscreenCanvas(3, 3);
         oc2.getContext('2d').fillRect(0, 0, 3, 3);
         oc2.convertToBlob().then(b => window.blob = b.type + ' ' + (b.size > 20));
         new OffscreenCanvas(0, 3).convertToBlob().catch(e => window.zero = e.name); 0",
    );
    assert_eq!(
        wait_for(
            &mut page,
            "window.blob + ' ' + window.zero",
            "image/png true IndexSizeError"
        ),
        "image/png true IndexSizeError"
    );
}

/// OffscreenCanvases and their contexts that script no longer reaches are freed (a map makes one
/// per label); one reached through its context alone stays usable.
#[test]
fn unreached_offscreen_canvases_are_freed() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net, "<!doctype html><body>");
    for _ in 0..4 {
        assert!(page.collect_garbage());
    }
    let (_, before) = page.engine().live_counts();
    eval(
        &mut page,
        "for (let i = 0; i < 2000; i++) {
             const c = new OffscreenCanvas(8, 8).getContext('2d'); c.fillRect(0, 0, 8, 8);
         }
         window.kept = new OffscreenCanvas(2, 2).getContext('2d'); 0",
    );
    for _ in 0..4 {
        assert!(page.collect_garbage());
    }
    let (_, after) = page.engine().live_counts();
    assert!(after < before + 100, "{after} objects, {before} before");
    assert_eq!(
        eval(
            &mut page,
            "kept.fillStyle = '#ff0000'; kept.fillRect(0, 0, 2, 2);
             [kept.canvas.width, Array.from(kept.getImageData(0, 0, 1, 1).data).join()].join('|')"
        ),
        "2|255,0,0,255"
    );
}

#[test]
fn a_placeholder_canvas_shows_what_its_offscreen_canvas_drew() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        "<!doctype html><canvas id=c width=4 height=4></canvas>",
    );
    let r = eval(
        &mut page,
        &format!(
            "{PX}
            const c = document.getElementById('c');
            window.oc = c.transferControlToOffscreen();
            const out = [oc.width, oc.height, tryIt(() => c.getContext('2d')),
                         tryIt(() => c.transferControlToOffscreen()), tryIt(() => {{ c.width = 5; }})];
            const ctx = oc.getContext('2d');
            ctx.fillStyle = '#0000ff'; ctx.fillRect(0, 0, 4, 4);
            out.join('|')"
        ),
    );
    assert_eq!(
        r,
        "4|4|InvalidStateError|InvalidStateError|InvalidStateError"
    );
    // The frame reaches the placeholder at the rendering update.
    assert_eq!(
        wait_for(&mut page, "px(document.getElementById('c'))", "0,0,255,255"),
        "0,0,255,255"
    );
}

#[test]
fn image_bitmaps_from_image_data_canvases_and_blobs() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net, "<!doctype html><body>");
    eval(
        &mut page,
        &format!(
            "{PX}
            window.out = null;
            (async () => {{
                const d = new ImageData(2, 2);
                d.data.set([255,0,0,255, 0,255,0,255, 0,0,255,255, 255,255,255,255]);
                const a = await createImageBitmap(d);
                // crop to the bottom-right pixel; flipped, the bottom row comes first
                const b = await createImageBitmap(d, 1, 1, 1, 1);
                const f = await createImageBitmap(d, {{ imageOrientation: 'flipY' }});
                const r = await createImageBitmap(d, {{ resizeWidth: 4, resizeHeight: 6, resizeQuality: 'pixelated' }});
                const oc = new OffscreenCanvas(3, 1); const c = oc.getContext('2d');
                c.translate(1, 0); c.fillStyle = '#00ff00'; c.fillRect(-1, 0, 3, 1);
                const t = oc.transferToImageBitmap();
                const blob = await new OffscreenCanvas(1, 1).getContext('2d').canvas.convertToBlob();
                const fromBlob = await createImageBitmap(await (async () => {{
                    const o = new OffscreenCanvas(1, 1); const x = o.getContext('2d');
                    x.fillStyle = '#0000ff'; x.fillRect(0, 0, 1, 1); return o.convertToBlob(); }})());
                const res = [a.width + 'x' + a.height, px(a), px(b), px(f), r.width + 'x' + r.height,
                             t.width, px(t), Array.from(c.getImageData(1, 0, 1, 1).data).join(),
                             c.getTransform().e, px(fromBlob)];
                a.close();
                res.push(a.width, tryIt(() => px(a)));
                let err = '';
                try {{ await createImageBitmap(d, 0, 0, 0, 1); }} catch (e) {{ err = e.name; }}
                res.push(err);
                try {{ await createImageBitmap(new Blob(['not an image'])); }} catch (e) {{ err = e.name; }}
                res.push(err);
                window.out = res.join('|');
            }})(); 0"
        ),
    );
    assert_eq!(
        wait_for(
            &mut page,
            "window.out",
            "2x2|255,0,0,255|255,255,255,255|0,0,255,255|4x6|3|0,255,0,255|0,0,0,0|1|0,0,255,255|0|InvalidStateError|RangeError|InvalidStateError"
        ),
        "2x2|255,0,0,255|255,255,255,255|0,0,255,255|4x6|3|0,255,0,255|0,0,0,0|1|0,0,255,255|0|InvalidStateError|RangeError|InvalidStateError"
    );
}

#[test]
fn a_worker_uploads_an_image_bitmap_from_a_blob_to_webgl() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net, "<!doctype html><body>");
    // (half-transparent red: premultiplyAlpha "none" uploads it straight, the default premultiplied)
    let w = worker(
        "onmessage = async e => {
             const gl = new OffscreenCanvas(1, 1).getContext('webgl');
             if (!gl) { postMessage('no webgl'); return; }
             const read = async (opts) => {
                 const bmp = await createImageBitmap(e.data, opts);
                 const tex = gl.createTexture(); gl.bindTexture(gl.TEXTURE_2D, tex);
                 gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, true);
                 gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, bmp);
                 const fb = gl.createFramebuffer(); gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
                 gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);
                 const p = new Uint8Array(4); gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, p);
                 return Array.from(p).join(' ');
             };
             postMessage([await read({ premultiplyAlpha: 'none' }), await read({}), gl.getError()].join(','));
         };",
    );
    eval(
        &mut page,
        &format!(
            "window.got = '';
            const o = new OffscreenCanvas(1, 1); const x = o.getContext('2d');
            x.fillStyle = 'rgba(255, 0, 0, 0.5)'; x.fillRect(0, 0, 1, 1);
            const w = {w};
            w.onmessage = e => window.got = e.data;
            o.convertToBlob().then(b => w.postMessage(b)); 0"
        ),
    );
    let got = wait_for(&mut page, "window.got", "255 0 0 128,128 0 0 128,0");
    if got == "no webgl" {
        eprintln!(
            "a_worker_uploads_an_image_bitmap_from_a_blob_to_webgl: no WebGL here, not checked"
        );
        return;
    }
    assert_eq!(got, "255 0 0 128,128 0 0 128,0");
}

/// A worker script from a blob URL.
fn worker(script: &str) -> String {
    format!(
        "new Worker(URL.createObjectURL(new Blob([{}])))",
        serde_json::to_string(script).unwrap()
    )
}

#[test]
fn a_transferred_offscreen_canvas_draws_from_a_worker() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        "<!doctype html><canvas id=c width=4 height=4></canvas>",
    );
    let w = worker(
        "onmessage = e => {
             const oc = e.data.canvas;
             const ctx = oc.getContext('2d');
             postMessage([oc.width, oc.height, typeof requestAnimationFrame].join());
             requestAnimationFrame(() => { ctx.fillStyle = '#00ff00'; ctx.fillRect(0, 0, 4, 4); });
         };",
    );
    let r = eval(
        &mut page,
        &format!(
            "{PX}
            window.got = '';
            const oc = document.getElementById('c').transferControlToOffscreen();
            const w = {w};
            w.onmessage = e => window.got = e.data;
            // a canvas with a context cannot go; a transferred one is detached
            const busy = new OffscreenCanvas(1, 1); busy.getContext('2d');
            const out = [tryIt(() => w.postMessage(busy, [busy]))];
            w.postMessage({{ canvas: oc }}, [oc]);
            out.push(oc.width, tryIt(() => oc.getContext('2d')), tryIt(() => w.postMessage(oc, [oc])));
            out.join('|')"
        ),
    );
    assert_eq!(r, "InvalidStateError|0|InvalidStateError|DataCloneError");
    assert_eq!(
        wait_for(&mut page, "window.got", "4,4,function"),
        "4,4,function"
    );
    assert_eq!(
        wait_for(&mut page, "px(document.getElementById('c'))", "0,255,0,255"),
        "0,255,0,255"
    );
}

#[test]
fn webgl_draws_from_a_worker_to_a_placeholder() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(
        &net,
        "<!doctype html><canvas id=c width=4 height=4></canvas>",
    );
    let w = worker(
        "onmessage = e => {
             const gl = e.data.getContext('webgl');
             if (!gl) { postMessage('no webgl'); return; }
             gl.clearColor(1, 0, 1, 1); gl.clear(gl.COLOR_BUFFER_BIT);
             const p = new Uint8Array(4); gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, p);
             postMessage([gl.canvas === e.data, gl.drawingBufferWidth, Array.from(p).join(' ')].join());
         };",
    );
    eval(
        &mut page,
        &format!(
            "{PX}
            window.got = '';
            const oc = document.getElementById('c').transferControlToOffscreen();
            const w = {w};
            w.onmessage = e => window.got = e.data;
            w.postMessage(oc, [oc]); 0"
        ),
    );
    let got = wait_for(&mut page, "window.got", "true,4,255 0 255 255");
    if got == "no webgl" {
        // (no ANGLE beside the test executable)
        eprintln!("webgl_draws_from_a_worker_to_a_placeholder: no WebGL here, not checked");
        return;
    }
    assert_eq!(got, "true,4,255 0 255 255");
    assert_eq!(
        wait_for(
            &mut page,
            "px(document.getElementById('c'))",
            "255,0,255,255"
        ),
        "255,0,255,255"
    );
}
