// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL extensions through the page: what `getSupportedExtensions` lists, the one object per
//! extension `getExtension` returns, extensions turned on only when asked for, and the functions
//! of ANGLE_instanced_arrays, OES_vertex_array_object and WEBGL_multi_draw drawing into the page.

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

/// A WebGL 1.0 program drawing green with an instance offset; `gl`, `p` and a quarter-width
/// rectangle at the left edge in attribute 0 are left in the page's globals.
const PROGRAM1: &str = "
    var c = document.getElementById('c');
    var gl = c.getContext('webgl', { antialias: false });
    function sh(t, s) { const x = gl.createShader(t); gl.shaderSource(x, s); gl.compileShader(x); return x; }
    var p = gl.createProgram();
    gl.attachShader(p, sh(gl.VERTEX_SHADER,
        'attribute vec2 a; attribute float dx; void main() { gl_Position = vec4(a.x + dx, a.y, 0.0, 1.0); }'));
    gl.attachShader(p, sh(gl.FRAGMENT_SHADER,
        'precision mediump float; void main() { gl_FragColor = vec4(0.0, 1.0, 0.0, 1.0); }'));
    gl.bindAttribLocation(p, 0, 'a');
    gl.bindAttribLocation(p, 1, 'dx');
    gl.linkProgram(p);
    gl.useProgram(p);
";

const RECT: &str = "
    const b = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, b);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1,-1, -0.5,-1, -1,1, -0.5,-1, -0.5,1, -1,1]), gl.STATIC_DRAW);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
";

#[test]
fn each_supported_extension_is_one_object_per_context() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    let r = eval(
        &mut page,
        "const out = [];
         for (const kind of ['webgl', 'webgl2']) {
           const gl = document.createElement('canvas').getContext(kind);
           const names = gl.getSupportedExtensions();
           const bad = names.filter(n => {
             const e = gl.getExtension(n);
             return !e || e !== gl.getExtension(n) || e !== gl.getExtension(n.toLowerCase())
                 || Object.prototype.hasOwnProperty.call(Object.getPrototypeOf(e), 'constructor')
                 || n in self;
           });
           out.push(kind, names.length > 10, bad.join(' '),
             names.includes('WEBGL_debug_renderer_info'),
             gl.getExtension('NO_such_extension'), gl.getError());
         }
         out.join()",
    );
    assert_eq!(r, "webgl,true,,true,,0,webgl2,true,,true,,0");
}

#[test]
fn the_extensions_each_version_offers() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    let r = eval(
        &mut page,
        "const has = (k, n) => document.createElement('canvas').getContext(k).getSupportedExtensions().includes(n);
         [has('webgl', 'ANGLE_instanced_arrays'), has('webgl', 'OES_vertex_array_object'),
          has('webgl', 'OES_texture_float'), has('webgl', 'WEBGL_draw_buffers'),
          has('webgl2', 'ANGLE_instanced_arrays'), has('webgl2', 'OES_texture_float'),
          has('webgl2', 'EXT_color_buffer_float'), has('webgl', 'EXT_color_buffer_float'),
          has('webgl2', 'EXT_texture_filter_anisotropic'), has('webgl2', 'WEBGL_multi_draw')].join()",
    );
    assert_eq!(r, "true,true,true,true,false,false,true,false,true,true");
}

#[test]
fn an_extension_is_on_only_once_asked_for() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    // EXT_color_buffer_float: a 32-bit float color attachment is complete only with it.
    let r = eval(
        &mut page,
        "const gl = document.createElement('canvas').getContext('webgl2');
         function status() {
           const rb = gl.createRenderbuffer();
           gl.bindRenderbuffer(gl.RENDERBUFFER, rb);
           gl.renderbufferStorage(gl.RENDERBUFFER, gl.RGBA32F, 4, 4);
           const fb = gl.createFramebuffer();
           gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
           gl.framebufferRenderbuffer(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.RENDERBUFFER, rb);
           const s = gl.checkFramebufferStatus(gl.FRAMEBUFFER) === gl.FRAMEBUFFER_COMPLETE;
           gl.getError();
           return s;
         }
         const before = status();
         gl.getExtension('EXT_color_buffer_float');
         const ani = gl.getExtension('EXT_texture_filter_anisotropic');
         [before, status(), ani.TEXTURE_MAX_ANISOTROPY_EXT === 0x84FE,
          gl.getParameter(ani.MAX_TEXTURE_MAX_ANISOTROPY_EXT) >= 2, gl.getError()].join()",
    );
    assert_eq!(r, "false,true,true,true,0");
}

#[test]
fn debug_renderer_info_gives_only_maker_and_model() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    let r = eval(
        &mut page,
        "const gl = document.createElement('canvas').getContext('webgl');
         const before = gl.getParameter(0x9246);
         const enumError = gl.getError() === gl.INVALID_ENUM;
         const e = gl.getExtension('WEBGL_debug_renderer_info');
         const vendor = gl.getParameter(e.UNMASKED_VENDOR_WEBGL);
         const renderer = gl.getParameter(e.UNMASKED_RENDERER_WEBGL);
         [before, enumError, typeof vendor, vendor.length > 0, renderer.length > 0,
          /ANGLE|Direct3D|D3D11|vs_5|0x/.test(vendor + renderer),
          gl.getParameter(gl.RENDERER), gl.getError()].join()",
    );
    assert_eq!(r, ",true,string,true,true,false,WebKit WebGL,0");
}

#[test]
fn instanced_arrays_and_a_vertex_array_object_draw_in_webgl1() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    let r = eval(
        &mut page,
        &format!(
            "{PROGRAM1}
            const vaoExt = gl.getExtension('OES_vertex_array_object');
            const inst = gl.getExtension('ANGLE_instanced_arrays');
            const vao = vaoExt.createVertexArrayOES();
            vaoExt.bindVertexArrayOES(vao);
            {RECT}
            const off = gl.createBuffer();
            gl.bindBuffer(gl.ARRAY_BUFFER, off);
            gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, 1]), gl.STATIC_DRAW);
            gl.enableVertexAttribArray(1);
            gl.vertexAttribPointer(1, 1, gl.FLOAT, false, 0, 0);
            inst.vertexAttribDivisorANGLE(1, 1);
            const bound = gl.getParameter(vaoExt.VERTEX_ARRAY_BINDING_OES) === vao;
            vaoExt.bindVertexArrayOES(null);
            const unbound = gl.getParameter(vaoExt.VERTEX_ARRAY_BINDING_OES);
            vaoExt.bindVertexArrayOES(vao);
            gl.clearColor(0, 0, 0, 1);
            gl.clear(gl.COLOR_BUFFER_BIT);
            // two instances: the left quarter, and the third quarter
            inst.drawArraysInstancedANGLE(gl.TRIANGLES, 0, 6, 2);
            [vao instanceof Object, Object.prototype.toString.call(vao), bound, unbound,
             vaoExt.isVertexArrayOES(vao), gl.getError()].join()"
        ),
    );
    assert_eq!(r, "true,[object WebGLVertexArrayObjectOES],true,,true,0");
    let green = |p: snowlark_paint::Color| p.g > 200 && p.r < 50;
    assert!(green(pixel(&mut page, 2, 10)), "first instance");
    assert!(!green(pixel(&mut page, 15, 10)), "the gap between them");
    assert!(green(pixel(&mut page, 22, 10)), "second instance");
}

#[test]
fn multi_draw_draws_each_range() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, CANVAS);
    let r = eval(
        &mut page,
        &format!(
            "{PROGRAM1}
            const md = gl.getExtension('WEBGL_multi_draw');
            const b = gl.createBuffer();
            gl.bindBuffer(gl.ARRAY_BUFFER, b);
            // two rectangles: the left quarter and the right quarter
            gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([
                -1,-1, -0.5,-1, -1,1, -0.5,-1, -0.5,1, -1,1,
                0.5,-1, 1,-1, 0.5,1, 1,-1, 1,1, 0.5,1]), gl.STATIC_DRAW);
            gl.enableVertexAttribArray(0);
            gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
            gl.disableVertexAttribArray(1);
            gl.vertexAttrib1f(1, 0);
            gl.clearColor(0, 0, 0, 1);
            gl.clear(gl.COLOR_BUFFER_BIT);
            md.multiDrawArraysWEBGL(gl.TRIANGLES, [99, 0, 6], 1, new Int32Array([0, 6, 6]), 1, 2);
            const ok = gl.getError();
            // the lists must hold drawcount values from their offsets
            md.multiDrawArraysWEBGL(gl.TRIANGLES, [0], 0, [6], 0, 2);
            [ok, gl.getError() === gl.INVALID_OPERATION].join()"
        ),
    );
    assert_eq!(r, "0,true");
    let green = |p: snowlark_paint::Color| p.g > 200 && p.r < 50;
    assert!(green(pixel(&mut page, 2, 10)), "first range");
    assert!(!green(pixel(&mut page, 20, 10)), "the middle");
    assert!(green(pixel(&mut page, 37, 10)), "second range");
}
