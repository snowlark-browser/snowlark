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

/// Script that makes `gl` (a WebGL context of the canvas `#c`), a texture `tex` bound to
/// TEXTURE_2D, `up(source)` (texImage2D of a source as RGBA bytes) and `read(w, h)` (the
/// texture's pixels through a framebuffer, first uploaded row first).
const TEXTURE: &str = "
    var gl = document.getElementById('c').getContext('webgl');
    var tex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, tex);
    function up(src) { gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, src); }
    function read(w, h) {
        const fb = gl.createFramebuffer();
        gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
        gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);
        const q = new Uint8Array(w * h * 4);
        gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, q);
        gl.bindFramebuffer(gl.FRAMEBUFFER, null);
        gl.deleteFramebuffer(fb);
        return Array.from(q).join();
    }
    function imageData(w, h, px) { const d = new ImageData(w, h); d.data.set(px); return d; }
";

#[test]
fn textures_from_image_data_with_the_unpack_parameters() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><canvas id=c width=4 height=4></canvas>",
    );
    let r = eval(
        &mut page,
        &format!(
            "{TEXTURE}
            const out = [];
            // WebGL 1.0 §5.14.8: ImageData is straight alpha; the source's top row goes first.
            up(imageData(2, 1, [255, 0, 0, 255, 0, 0, 255, 128]));
            out.push(read(2, 1));
            gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, true);
            up(imageData(2, 1, [255, 0, 0, 255, 0, 0, 255, 128]));
            out.push(read(2, 1));
            gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
            gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, true);
            up(imageData(1, 2, [1, 2, 3, 255, 4, 5, 6, 255]));
            out.push(read(1, 2));
            // ... and to an ArrayBufferView's rows too
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 1, 2, 0, gl.RGBA, gl.UNSIGNED_BYTE,
                          new Uint8Array([1, 2, 3, 255, 4, 5, 6, 255]));
            out.push(read(1, 2));
            gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
            // texSubImage2D puts the source at the offset
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 2, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
            gl.texSubImage2D(gl.TEXTURE_2D, 0, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE,
                             imageData(1, 1, [9, 8, 7, 255]));
            out.push(read(2, 1));
            // RGB rows padded to UNPACK_ALIGNMENT (4)
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGB, gl.RGB, gl.UNSIGNED_BYTE,
                          imageData(1, 2, [10, 20, 30, 255, 40, 50, 60, 255]));
            out.push(read(1, 2));
            out.push(gl.getError());
            out.join('|')"
        ),
    );
    assert_eq!(
        r,
        "255,0,0,255,0,0,255,128|255,0,0,255,0,0,128,128|4,5,6,255,1,2,3,255|\
         4,5,6,255,1,2,3,255|0,0,0,0,9,8,7,255|10,20,30,255,40,50,60,255|0"
    );
}

#[test]
fn textures_from_canvases_and_a_webgl_canvas_in_a_2d_one() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><canvas id=c width=4 height=4></canvas><canvas id=g width=2 height=2></canvas>",
    );
    let r = eval(
        &mut page,
        &format!(
            "{TEXTURE}
            const out = [];
            const c2 = document.createElement('canvas');
            c2.width = 2; c2.height = 2;
            const x = c2.getContext('2d');
            x.fillStyle = '#ff0000'; x.fillRect(0, 0, 2, 1);
            x.fillStyle = 'rgba(0, 0, 255, 0.5)'; x.fillRect(0, 1, 2, 1);
            up(c2);
            out.push(read(1, 2));
            // a WebGL canvas: its drawing buffer
            const other = document.getElementById('g').getContext('webgl');
            other.clearColor(0, 1, 0, 1);
            other.clear(other.COLOR_BUFFER_BIT);
            up(document.getElementById('g'));
            out.push(read(1, 1));
            // drawImage of a WebGL canvas into a 2D one
            const c3 = document.createElement('canvas').getContext('2d');
            c3.drawImage(document.getElementById('g'), 0, 0);
            out.push(Array.from(c3.getImageData(1, 1, 1, 1).data).join());
            // a canvas with no context is transparent black of its size
            const blank = document.createElement('canvas');
            blank.width = 3; blank.height = 1;
            up(blank);
            out.push(read(3, 1));
            // an image with nothing decoded has nothing to upload
            up(new Image());
            out.push(gl.getError() === gl.INVALID_VALUE);
            out.push(gl.getError());
            out.join('|')"
        ),
    );
    assert_eq!(
        r,
        "255,0,0,255,0,0,255,128|0,255,0,255|0,255,0,255|0,0,0,0,0,0,0,0,0,0,0,0|true|0"
    );
}

#[test]
fn a_texture_from_an_image_element() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><canvas id=c width=4 height=4></canvas>",
    );
    eval(
        &mut page,
        "const s = document.createElement('canvas');
         s.width = 1; s.height = 2;
         const x = s.getContext('2d');
         x.fillStyle = '#00ff00'; x.fillRect(0, 0, 1, 1);
         x.fillStyle = '#0000ff'; x.fillRect(0, 1, 1, 1);
         var img = new Image();
         img.onload = () => { window.loaded = true; };
         img.src = s.toDataURL();
         0",
    );
    page.run_until_settled(Duration::from_secs(5));
    let r = eval(
        &mut page,
        &format!(
            "{TEXTURE}
            const out = [window.loaded];
            up(img);
            out.push(read(1, 2));
            gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, true);
            up(img);
            out.push(read(1, 2));
            out.push(gl.getError());
            out.join('|')"
        ),
    );
    assert_eq!(r, "true|0,255,0,255,0,0,255,255|0,0,255,255,0,255,0,255|0");
}

/// A tiny HTTP/1.1 server answering `/a.png` and `/shared.png` (with
/// `Access-Control-Allow-Origin: *`) with `png`.
pub(crate) fn serve_png(png: Vec<u8>) -> u16 {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
            }
            let target = first.split(' ').nth(1).unwrap_or("/");
            let path = target.split('?').next().unwrap_or("/");
            let shared = if path == "/shared.png" {
                "Access-Control-Allow-Origin: *\r\n"
            } else {
                ""
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n{shared}Content-Length: {}\r\nConnection: close\r\n\r\n",
                png.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&png);
        }
    });
    port
}

#[test]
fn a_cross_origin_image_is_not_uploaded() {
    let dir = std::env::temp_dir().join(format!("snowlark-webgl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a.png");
    snowlark_paint::image::encode_png(1, 1, &[0, 0, 255, 255], &file).unwrap();
    let port = serve_png(std::fs::read(&file).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    let url = url::Url::parse(&format!("http://localhost:{port}/")).unwrap();
    page.load_html("<!doctype html><canvas id=c></canvas>", &url);
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
    let r = eval(
        &mut page,
        &format!(
            "{TEXTURE}
            // WebGL 1.0 §5.14.8: a source that is not CORS-same-origin throws SecurityError.
            function name(img) {{ try {{ up(img); return read(1, 1); }} catch (e) {{ return e.name; }} }}
            [name(same), name(cross), name(shared), name(sharedNoCors), gl.getError()].join('|')"
        ),
    );
    assert_eq!(r, "255,0,0,255|SecurityError|255,0,0,255|SecurityError|0");
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
