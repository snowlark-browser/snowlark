// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.12.5.1.14 the origin-clean flag of a canvas bitmap: drawing an image that is not
//! CORS-same-origin (or a canvas that is not origin-clean, or setting a pattern made of one as a
//! style) sets it to false, after which `getImageData`, `toDataURL`, `toBlob` and WebGL's
//! `texImage2D` of the canvas throw `SecurityError`.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// A 1 by 1 PNG of one opaque blue pixel.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x60, 0xf8, 0xff,
    0x1f, 0x00, 0x03, 0x02, 0x01, 0xff, 0xe6, 0x77, 0x0b, 0xae, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// A page on `http://localhost:<port>/` with four images loaded: `same` (same origin), `cross`
/// (from `127.0.0.1`, no CORS), `shared` (cross-origin, CORS-shared and fetched in CORS mode) and
/// `sharedNoCors` (CORS-shared, but fetched without CORS mode). Panics unless all four loaded:
/// an image that is not available draws nothing and so cannot taint.
fn page_with_images(net: &NetThread) -> Page {
    let port = crate::webgl::serve_png(PNG.to_vec());
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    let url = url::Url::parse(&format!("http://localhost:{port}/")).unwrap();
    page.load_html(
        "<!doctype html><canvas id=c width=1 height=1></canvas>",
        &url,
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    eval(
        &mut page,
        &format!(
            "window.n = 0;
             function load(src, cors) {{
                 const i = new Image();
                 if (cors) i.crossOrigin = 'anonymous';
                 i.onload = i.onerror = () => window.n++;
                 i.src = src;
                 return i;
             }}
             var same = load('/a.png'), cross = load('http://127.0.0.1:{port}/a.png'),
                 shared = load('http://127.0.0.1:{port}/shared.png', true),
                 sharedNoCors = load('http://127.0.0.1:{port}/shared.png?plain'); 0"
        ),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while eval(&mut page, "window.n") != "4" && std::time::Instant::now() < deadline {
        page.run_for(Duration::from_millis(20));
    }
    let loaded = "[same, cross, shared, sharedNoCors]
                  .map(i => i.complete && i.naturalWidth == 1).join()";
    assert_eq!(
        eval(&mut page, loaded),
        "true,true,true,true",
        "the images load"
    );
    page
}

const HELPERS: &str = "
    function ctx2d() { const c = document.createElement('canvas'); c.width = c.height = 1; return c.getContext('2d'); }
    function tryIt(f) { try { f(); return 'ok'; } catch (e) { return e.name; } }
    // getImageData, toDataURL and toBlob of a context's canvas
    function reads(ctx) {
        return [tryIt(() => ctx.getImageData(0, 0, 1, 1)), tryIt(() => ctx.canvas.toDataURL()),
                tryIt(() => ctx.canvas.toBlob(() => {}))].join(',');
    }
    function drawn(img) { const ctx = ctx2d(); ctx.drawImage(img, 0, 0); return ctx; }
";

#[test]
fn drawing_an_image_that_is_not_cors_same_origin_taints_the_canvas() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page_with_images(&net);
    let r = eval(
        &mut page,
        &format!(
            "{HELPERS}
            [reads(drawn(same)), reads(drawn(cross)), reads(drawn(shared)),
             reads(drawn(sharedNoCors))].join('|')"
        ),
    );
    assert_eq!(
        r,
        "ok,ok,ok|SecurityError,SecurityError,SecurityError|ok,ok,ok|\
         SecurityError,SecurityError,SecurityError"
    );
}

#[test]
fn the_flag_follows_canvases_and_patterns_and_never_comes_back() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page_with_images(&net);
    let r = eval(
        &mut page,
        &format!(
            "{HELPERS}
            const out = [];
            // a tainted canvas drawn into another taints it
            const tainted = drawn(cross);
            out.push(reads(drawn(tainted.canvas)));
            // a clean canvas does not
            out.push(reads(drawn(drawn(same).canvas)));
            // a pattern of a cross-origin image taints only once it is set as a style
            const a = ctx2d(), p = a.createPattern(cross, 'repeat');
            out.push(reads(a));
            a.strokeStyle = p;
            out.push(reads(a));
            const b = ctx2d();
            b.fillStyle = b.createPattern(tainted.canvas, 'repeat');
            out.push(reads(b));
            // resetting the bitmap (a new width) keeps the flag
            tainted.canvas.width = 2;
            out.push(reads(tainted));
            out.join('|')"
        ),
    );
    assert_eq!(
        r,
        "SecurityError,SecurityError,SecurityError|ok,ok,ok|ok,ok,ok|\
         SecurityError,SecurityError,SecurityError|SecurityError,SecurityError,SecurityError|\
         SecurityError,SecurityError,SecurityError"
    );
}

#[test]
fn webgl_refuses_a_canvas_that_is_not_origin_clean() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page_with_images(&net);
    let r = eval(
        &mut page,
        &format!(
            "{HELPERS}
            const gl = document.getElementById('c').getContext('webgl');
            gl.bindTexture(gl.TEXTURE_2D, gl.createTexture());
            function up(src) {{ gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, src); }}
            [tryIt(() => up(drawn(same).canvas)), tryIt(() => up(drawn(cross).canvas)),
             gl.getError()].join('|')"
        ),
    );
    assert_eq!(r, "ok|SecurityError|0");
}
