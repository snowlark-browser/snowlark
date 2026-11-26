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

/// A page on `http://localhost:<port>/` with four images loaded: `same` (same origin), `cross`
/// (from `127.0.0.1`, no CORS), `shared` (cross-origin, CORS-shared and fetched in CORS mode) and
/// `sharedNoCors` (CORS-shared, but fetched without CORS mode).
fn page_with_images(net: &NetThread) -> Page {
    let dir = std::env::temp_dir().join(format!("snowlark-origin-clean-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a.png");
    snowlark_paint::image::encode_png(1, 1, &[0, 0, 255, 255], &file).unwrap();
    let port = crate::webgl::serve_png(std::fs::read(&file).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
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
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while eval(&mut page, "window.n") != "4" && std::time::Instant::now() < deadline {
        page.run_for(Duration::from_millis(20));
    }
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
#[cfg_attr(
    not(windows),
    ignore = "WebGL needs ANGLE, not built for this system yet (docs/linux.md stage 6)"
)]
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
