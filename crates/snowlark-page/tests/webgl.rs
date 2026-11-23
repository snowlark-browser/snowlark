// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL 1.0 through the page: `getContext`, a triangle, `readPixels`, the drawing buffer in the
//! rendering, and the checks the client makes before a call reaches ANGLE.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn rendered(net: &NetThread, html: &str) -> Page {
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

/// The page's pixel at (x, y) after a rendering update.
fn pixel(page: &mut Page, x: u32, y: u32) -> snowlark_paint::Color {
    page.run_until_settled(Duration::from_secs(2));
    page.engine()
        .with_renderer(|r, _| {
            let pm = r
                .rasterize(
                    snowlark_paint::Size::new(200.0, 100.0),
                    snowlark_paint::Point::ZERO,
                    1.0,
                )
                .unwrap();
            pm.pixel(x, y)
        })
        .unwrap()
}

const TRIANGLE: &str = "
    const c = document.getElementById('c');
    const gl = c.getContext('webgl', { antialias: false });
    function sh(t, s) { const x = gl.createShader(t); gl.shaderSource(x, s); gl.compileShader(x); return x; }
    const p = gl.createProgram();
    gl.attachShader(p, sh(gl.VERTEX_SHADER, 'attribute vec2 a; void main(){ gl_Position = vec4(a, 0.0, 1.0); }'));
    gl.attachShader(p, sh(gl.FRAGMENT_SHADER, 'precision mediump float; uniform vec4 u; void main(){ gl_FragColor = u; }'));
    gl.linkProgram(p);
    gl.useProgram(p);
    const b = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, b);
    // the left half of the canvas: (-1,-1) (0,-1) (-1,1) and (0,-1) (0,1) (-1,1)
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 0, -1, -1, 1, 0, -1, 0, 1, -1, 1]), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(p, 'a');
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
    gl.clearColor(0, 0, 1, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.uniform4f(gl.getUniformLocation(p, 'u'), 1, 0, 0, 1);
    gl.drawArrays(gl.TRIANGLES, 0, 6);
";

#[test]
fn a_webgl_canvas_draws_into_the_page() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><canvas id=c width=40 height=20></canvas>",
    );
    let r = eval(
        &mut page,
        &format!(
            "{TRIANGLE}
            const left = new Uint8Array(4), right = new Uint8Array(4);
            gl.readPixels(5, 5, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, left);
            gl.readPixels(35, 15, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, right);
            [gl === c.getContext('webgl'), gl === c.getContext('experimental-webgl'),
             c.getContext('2d'), gl.canvas === c, gl.drawingBufferWidth, gl.drawingBufferHeight,
             Array.from(left), Array.from(right), gl.getError(),
             gl.getProgramParameter(p, gl.LINK_STATUS), gl.getParameter(gl.CURRENT_PROGRAM) === p,
             gl.getParameter(gl.ARRAY_BUFFER_BINDING) === b, Array.from(gl.getParameter(gl.VIEWPORT)),
             gl.isContextLost(), gl.getContextAttributes().antialias,
             gl instanceof WebGLRenderingContext, p instanceof WebGLProgram].join('|')"
        ),
    );
    assert_eq!(
        r,
        "true|true||true|40|20|255,0,0,255|0,0,255,255|0|true|true|true|0,0,40,20|false|false|true|true"
    );
    // The page shows the drawing buffer: red on the left, blue on the right.
    assert_eq!(
        pixel(&mut page, 5, 5),
        snowlark_paint::Color::rgb(255, 0, 0)
    );
    assert_eq!(
        pixel(&mut page, 35, 15),
        snowlark_paint::Color::rgb(0, 0, 255)
    );
    // WebGL 1.0 §2.2: after the rendering presented it, the drawing buffer is cleared.
    let r = eval(
        &mut page,
        "const q = new Uint8Array(4); gl.readPixels(5, 5, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, q); Array.from(q).join()",
    );
    assert_eq!(r, "0,0,0,0");
    // The page keeps showing what was presented.
    assert_eq!(
        pixel(&mut page, 5, 5),
        snowlark_paint::Color::rgb(255, 0, 0)
    );
}

#[test]
fn a_canvas_keeps_its_first_kind_of_context() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><canvas id=a></canvas><canvas id=b></canvas>",
    );
    let r = eval(
        &mut page,
        "const a = document.getElementById('a'), b = document.getElementById('b');
         const g = a.getContext('2d');
         [a.getContext('webgl'), b.getContext('webgl') !== null, b.getContext('2d'),
          b.getContext('webgl2'), b.getContext('webgl').drawingBufferWidth].join('|')",
    );
    assert_eq!(r, "|true|||300");
}

#[test]
fn objects_of_another_context_or_deleted_ones_are_refused() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><canvas id=a></canvas><canvas id=b></canvas>",
    );
    let r = eval(
        &mut page,
        "const g1 = document.getElementById('a').getContext('webgl');
         const g2 = document.getElementById('b').getContext('webgl');
         const buf = g2.createBuffer();
         g1.bindBuffer(g1.ARRAY_BUFFER, buf);
         const e1 = g1.getError();
         const t = g1.createTexture();
         g1.deleteTexture(t);
         g1.bindTexture(g1.TEXTURE_2D, t);
         const e2 = g1.getError();
         const before = g1.isTexture(t);
         const t2 = g1.createTexture();
         const unbound = g1.isTexture(t2);
         g1.bindTexture(g1.TEXTURE_2D, t2);
         const s = g1.createShader(g1.VERTEX_SHADER);
         g1.shaderSource(s, 'void main() { gl_Position = vec4(0.0); }');
         const px = new Uint8Array(4);
         g1.readPixels(0, 0, 1, 1, g1.RGBA, g1.UNSIGNED_BYTE, new Float32Array(4));
         const e3 = g1.getError();
         g1.readPixels(0, 0, 2, 2, g1.RGBA, g1.UNSIGNED_BYTE, px);
         const e4 = g1.getError();
         g1.uniformMatrix2fv(null, false, [1, 2, 3]);
         [e1 === g1.INVALID_OPERATION, e2 === g1.INVALID_OPERATION, before, unbound,
          g1.isTexture(t2), g1.getShaderSource(s).length > 0,
          g1.getShaderParameter(s, g1.SHADER_TYPE) === g1.VERTEX_SHADER,
          e3 === g1.INVALID_OPERATION, e4 === g1.INVALID_OPERATION, g1.getError()].join('|')",
    );
    assert_eq!(r, "true|true|false|false|true|true|true|true|true|0");
}

#[test]
fn a_preserved_drawing_buffer_and_to_data_url() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><canvas id=c width=4 height=4></canvas>",
    );
    eval(
        &mut page,
        "var gl = document.getElementById('c').getContext('webgl', { preserveDrawingBuffer: true });
         gl.clearColor(0, 1, 0, 1); gl.clear(gl.COLOR_BUFFER_BIT); 0",
    );
    page.run_until_settled(Duration::from_secs(2));
    let r = eval(
        &mut page,
        "const q = new Uint8Array(4); gl.readPixels(1, 1, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, q);
         [Array.from(q).join(), gl.getContextAttributes().preserveDrawingBuffer,
          document.getElementById('c').toDataURL().startsWith('data:image/png;base64,iVBOR')].join('|')",
    );
    assert_eq!(r, "0,255,0,255|true|true");
}
