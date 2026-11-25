// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL 2.0 through the page: `getContext("webgl2")` on a canvas and an OffscreenCanvas, a
//! drawing through a vertex array with instancing, uniform blocks, fences that change state only
//! between tasks, and pixels read back through a pixel pack buffer.

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

const CANVAS: &str =
    "<!doctype html><body style='margin:0'><canvas id=c width=40 height=20></canvas>";

/// A GLSL ES 3.00 program; `gl` and `p` are left in the page's globals.
const PROGRAM: &str = "
    var c = document.getElementById('c');
    var gl = c.getContext('webgl2', { antialias: false });
    function sh(t, s) { const x = gl.createShader(t); gl.shaderSource(x, s); gl.compileShader(x); return x; }
    var p = gl.createProgram();
    gl.attachShader(p, sh(gl.VERTEX_SHADER, `#version 300 es
        layout(location = 0) in vec2 a; layout(location = 1) in float dx;
        void main() { gl_Position = vec4(a.x + dx, a.y, 0.0, 1.0); }`));
    gl.attachShader(p, sh(gl.FRAGMENT_SHADER, `#version 300 es
        precision mediump float; uniform Colors { vec4 color; }; out vec4 o;
        void main() { o = color; }`));
    gl.linkProgram(p);
    gl.useProgram(p);
";

#[test]
fn a_webgl2_context_is_given_and_reports_itself() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    let r = eval(
        &mut page,
        "const c = document.getElementById('c');
         const gl = c.getContext('webgl2');
         [gl instanceof WebGL2RenderingContext, gl === c.getContext('webgl2'),
          c.getContext('webgl'), gl.getParameter(gl.VERSION).startsWith('WebGL 2.0'),
          gl.getParameter(gl.SHADING_LANGUAGE_VERSION).startsWith('WebGL GLSL ES 3.00'),
          gl.getParameter(gl.MAX_CLIENT_WAIT_TIMEOUT_WEBGL),
          gl.getParameter(gl.MAX_DRAW_BUFFERS) >= 4,
          typeof gl.getParameter(gl.MAX_UNIFORM_BLOCK_SIZE),
          gl.getContextAttributes().antialias,
          document.createElement('canvas').getContext('webgl') instanceof WebGL2RenderingContext,
          gl.getError()].join()",
    );
    assert_eq!(r, "true,true,,true,true,0,true,number,true,false,0");
}

#[test]
fn a_vertex_array_with_instancing_and_a_uniform_block_draws_into_the_page() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    let r = eval(
        &mut page,
        &format!(
            "{PROGRAM}
            const vao = gl.createVertexArray();
            gl.bindVertexArray(vao);
            const b = gl.createBuffer();
            gl.bindBuffer(gl.ARRAY_BUFFER, b);
            // a quarter-width rectangle at the left edge
            gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1,-1, -0.5,-1, -1,1, -0.5,-1, -0.5,1, -1,1]), gl.STATIC_DRAW);
            gl.enableVertexAttribArray(0);
            gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
            const inst = gl.createBuffer();
            gl.bindBuffer(gl.ARRAY_BUFFER, inst);
            gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, 1]), gl.STATIC_DRAW);
            gl.enableVertexAttribArray(1);
            gl.vertexAttribPointer(1, 1, gl.FLOAT, false, 0, 0);
            gl.vertexAttribDivisor(1, 1);
            const e = gl.createBuffer();
            gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, e);
            const block = gl.getUniformBlockIndex(p, 'Colors');
            gl.uniformBlockBinding(p, block, 1);
            const ubo = gl.createBuffer();
            gl.bindBufferBase(gl.UNIFORM_BUFFER, 1, ubo);
            gl.bufferData(gl.UNIFORM_BUFFER, new Float32Array([0, 1, 0, 1]), gl.STATIC_DRAW);
            const state = [gl.getParameter(gl.VERTEX_ARRAY_BINDING) === vao,
                gl.getParameter(gl.ELEMENT_ARRAY_BUFFER_BINDING) === e,
                gl.getIndexedParameter(gl.UNIFORM_BUFFER_BINDING, 1) === ubo,
                gl.getActiveUniformBlockName(p, block),
                gl.getActiveUniformBlockParameter(p, block, gl.UNIFORM_BLOCK_DATA_SIZE),
                gl.getActiveUniformBlockParameter(p, block, gl.UNIFORM_BLOCK_BINDING)];
            // the element array buffer is the vertex array's: none in the default one
            gl.bindVertexArray(null);
            state.push(gl.getParameter(gl.ELEMENT_ARRAY_BUFFER_BINDING));
            gl.bindVertexArray(vao);
            state.push(gl.getParameter(gl.ELEMENT_ARRAY_BUFFER_BINDING) === e);
            gl.clearBufferfv(gl.COLOR, 0, [0, 0, 1, 1]);
            gl.drawArraysInstanced(gl.TRIANGLES, 0, 6, 2);
            state.push(gl.getError());
            state.join()"
        ),
    );
    assert_eq!(r, "true,true,true,Colors,16,1,,true,0");
    // Copies at 0-10 and 20-30 px are green; the rest is the blue clear.
    assert_eq!(
        pixel(&mut page, 2, 10),
        snowlark_paint::Color::rgba(0, 255, 0, 255)
    );
    assert_eq!(
        pixel(&mut page, 22, 10),
        snowlark_paint::Color::rgba(0, 255, 0, 255)
    );
    assert_eq!(
        pixel(&mut page, 15, 10),
        snowlark_paint::Color::rgba(0, 0, 255, 255)
    );
}

#[test]
fn a_fence_signals_only_after_the_task_that_made_it() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    let r = eval(
        &mut page,
        "var gl = document.getElementById('c').getContext('webgl2');
         gl.clearColor(1, 0, 0, 1);
         gl.clear(gl.COLOR_BUFFER_BIT);
         var sync = gl.fenceSync(gl.SYNC_GPU_COMMANDS_COMPLETE, 0);
         [gl.isSync(sync), gl.clientWaitSync(sync, gl.SYNC_FLUSH_COMMANDS_BIT, 0) === gl.TIMEOUT_EXPIRED,
          gl.getSyncParameter(sync, gl.SYNC_STATUS) === gl.UNSIGNALED,
          gl.getSyncParameter(sync, gl.OBJECT_TYPE) === gl.SYNC_FENCE,
          gl.fenceSync(0, 0), gl.getError() === gl.INVALID_ENUM,
          gl.clientWaitSync(sync, 0, 1) === gl.WAIT_FAILED, gl.getError() === gl.INVALID_VALUE].join()",
    );
    assert_eq!(r, "true,true,true,true,,true,true,true");
    page.run_until_settled(Duration::from_secs(2));
    let r = eval(
        &mut page,
        "gl.finish();
         const s = gl.clientWaitSync(sync, 0, 0);
         const st = gl.getSyncParameter(sync, gl.SYNC_STATUS);
         gl.deleteSync(sync);
         [s === gl.ALREADY_SIGNALED || s === gl.CONDITION_SATISFIED, st === gl.SIGNALED,
          gl.isSync(sync)].join()",
    );
    assert_eq!(r, "true,true,false");
}

#[test]
fn pixels_come_back_through_a_pixel_pack_buffer() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    let r = eval(
        &mut page,
        "const gl = document.getElementById('c').getContext('webgl2');
         gl.clearColor(1, 0, 0, 1);
         gl.clear(gl.COLOR_BUFFER_BIT);
         const pbo = gl.createBuffer();
         gl.bindBuffer(gl.PIXEL_PACK_BUFFER, pbo);
         gl.bufferData(gl.PIXEL_PACK_BUFFER, 16, gl.STREAM_READ);
         gl.readPixels(0, 0, 2, 2, gl.RGBA, gl.UNSIGNED_BYTE, 0);
         const e1 = gl.getError();
         // a readback into client memory while a pack buffer is bound
         gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array(4));
         const e2 = gl.getError() === gl.INVALID_OPERATION;
         const out = new Uint8Array(20);
         gl.getBufferSubData(gl.PIXEL_PACK_BUFFER, 4, out, 2, 8);
         gl.bindBuffer(gl.PIXEL_PACK_BUFFER, null);
         // an offset with no pack buffer bound
         gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, 0);
         const e3 = gl.getError() === gl.INVALID_OPERATION;
         const direct = new Uint8Array(8);
         gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, direct, 4);
         [e1, e2, Array.from(out).join(' '), e3, Array.from(direct).join(' '), gl.getError()].join()",
    );
    assert_eq!(
        r,
        "0,true,0 0 255 0 0 255 255 0 0 255 0 0 0 0 0 0 0 0 0 0,true,0 0 0 0 255 0 0 255,0"
    );
}

#[test]
fn an_offscreen_canvas_gives_a_webgl2_context() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    let r = eval(
        &mut page,
        "const oc = new OffscreenCanvas(4, 4);
         const gl = oc.getContext('webgl2', { alpha: false, depth: false, antialias: false,
                                              failIfMajorPerformanceCaveat: true });
         gl.clearColor(0, 1, 0, 1);
         gl.clear(gl.COLOR_BUFFER_BIT);
         const px = new Uint8Array(4);
         gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px);
         [gl instanceof WebGL2RenderingContext, oc.getContext('webgl'),
          oc.getContext('webgl2') === gl, Array.from(px).join(' ')].join()",
    );
    assert_eq!(r, "true,,true,0 255 0 255");
}
