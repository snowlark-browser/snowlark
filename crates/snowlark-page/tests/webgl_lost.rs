// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A lost WebGL context (WebGL 1.0 §5.15): `WEBGL_lose_context`, the `webglcontextlost` and
//! `webglcontextrestored` events, what the context's methods return while it is lost, and a
//! context lost with the process that ran it (a GPU process restart) and restored after it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use snowlark_gl::{Attributes, Backend, Command, ContextId, Created, Query, Readback, Reply};
use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup, WebGlProvider};

fn page(net: &NetThread, webgl: Option<Arc<dyn WebGlProvider>>) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        webgl,
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body style='margin:0'><canvas id=c width=8 height=8></canvas>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Runs the page until `js` is true, at most `wait`.
fn until(page: &mut Page, js: &str, wait: Duration) -> bool {
    let end = Instant::now() + wait;
    loop {
        page.run_until_settled(Duration::from_millis(100));
        if eval(page, js) == "true" {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

const SETUP: &str = "
    var c = document.getElementById('c');
    var gl = c.getContext('webgl');
    var ext = gl.getExtension('WEBGL_lose_context');
    var log = [];
    var b = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, b);
    var p = gl.createProgram();
    var keep = true;
    c.addEventListener('webglcontextlost', e => {
        log.push('lost ' + (e instanceof WebGLContextEvent) + ' ' + JSON.stringify(e.statusMessage)
            + ' ' + e.cancelable + ' ' + gl.isContextLost());
        if (keep) e.preventDefault();
    });
    c.addEventListener('webglcontextrestored', e => log.push('restored ' + gl.isContextLost()));
    function green() {
        gl.clearColor(0, 1, 0, 1);
        gl.clear(gl.COLOR_BUFFER_BIT);
        const px = new Uint8Array(4);
        gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px);
        return px.join(',');
    }
    String(ext !== null)
";

/// `loseContext` loses the context at once and fires `webglcontextlost` in a task; while lost the
/// methods return their defaults (null, 0, false, -1) and `getError` gives CONTEXT_LOST_WEBGL once;
/// a canceled event lets `restoreContext` restore it, with `webglcontextrestored`, its state as
/// made and the objects of before invalidated.
#[test]
fn lose_context_and_restore_context_run_the_steps_of_a_lost_context() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net, None);
    assert_eq!(eval(&mut page, SETUP), "true");
    assert_eq!(eval(&mut page, "green()"), "0,255,0,255");
    let lost = eval(
        &mut page,
        "ext.loseContext();
        [gl.isContextLost(), gl.getError(), gl.getError(), gl.getParameter(gl.VERSION),
         gl.getContextAttributes(), gl.getExtension('OES_texture_float'),
         gl.getSupportedExtensions(), gl.isBuffer(b), gl.isEnabled(gl.BLEND),
         gl.checkFramebufferStatus(gl.FRAMEBUFFER), gl.getAttribLocation(p, 'a'),
         gl.createShader(gl.VERTEX_SHADER), gl.drawingBufferWidth, log.length].join(' ')",
    );
    // (null joins as nothing: getParameter, getContextAttributes, getExtension,
    // getSupportedExtensions and createShader return null; the event comes in a task of its own)
    assert_eq!(lost, "true 37442 0     false false 36061 -1  0 0");
    // A context lost by loseContext is restored only by restoreContext, and only once the event
    // was canceled: before it was dispatched, that is INVALID_OPERATION.
    assert_eq!(
        eval(&mut page, "ext.restoreContext(); gl.getError()"),
        "1282"
    );
    assert!(until(&mut page, "log.length == 1", Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "log[0]"), "lost true \"\" true true");
    // A buffer made while lost is invalidated from the start; another loseContext is
    // INVALID_OPERATION.
    eval(&mut page, "var made = gl.createBuffer(); ext.loseContext()");
    assert_eq!(eval(&mut page, "gl.getError()"), "1282");
    eval(&mut page, "ext.restoreContext()");
    assert!(until(&mut page, "log.length == 2", Duration::from_secs(5)));
    assert_eq!(
        eval(
            &mut page,
            "[log[1], gl.isContextLost(), gl.getError(), gl.getExtension('WEBGL_lose_context') === ext,
              gl.getParameter(gl.ARRAY_BUFFER_BINDING)].join(' ')"
        ),
        "restored false false 0 true "
    );
    // The objects of before, and the one made while lost, are invalidated.
    assert_eq!(
        eval(
            &mut page,
            "gl.bindBuffer(gl.ARRAY_BUFFER, b); const e1 = gl.getError();
             gl.bindBuffer(gl.ARRAY_BUFFER, made); const e2 = gl.getError();
             [e1, e2, gl.isBuffer(b)].join(' ')"
        ),
        "1282 1282 false"
    );
    assert_eq!(eval(&mut page, "green()"), "0,255,0,255");
}

/// A `webglcontextlost` event not canceled leaves the context lost: `restoreContext` is then
/// INVALID_OPERATION, and nothing restores it.
#[test]
fn a_lost_event_not_canceled_keeps_the_context_lost() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net, None);
    assert_eq!(eval(&mut page, SETUP), "true");
    eval(&mut page, "keep = false; ext.loseContext()");
    assert!(until(&mut page, "log.length == 1", Duration::from_secs(5)));
    assert_eq!(
        eval(
            &mut page,
            "[gl.getError(), (ext.restoreContext(), gl.getError()), gl.getError()].join(' ')"
        ),
        "37442 1282 0"
    );
    assert!(!until(
        &mut page,
        "log.length > 1",
        Duration::from_millis(300)
    ));
    assert_eq!(
        eval(&mut page, "[gl.isContextLost(), log.length].join(' ')"),
        "true 1"
    );
}

/// The contexts on this process's WebGL thread, standing for a GPU process that can end:
/// while `gone`, every context made before is lost and no new one can be made.
struct Gpu {
    gone: Arc<AtomicBool>,
}

struct GpuBackend {
    service: Box<dyn Backend>,
    gone: Arc<AtomicBool>,
}

impl WebGlProvider for Gpu {
    fn backend(&self) -> Result<Box<dyn Backend>, String> {
        Ok(Box::new(GpuBackend {
            service: Box::new(snowlark_gl::Service::from_env()?),
            gone: self.gone.clone(),
        }))
    }
}

impl Backend for GpuBackend {
    fn create_context(
        &mut self,
        id: ContextId,
        attributes: &Attributes,
        width: u32,
        height: u32,
    ) -> Result<Created, String> {
        if self.gone.load(Ordering::SeqCst) {
            return Err("the GPU process is gone".into());
        }
        self.service.create_context(id, attributes, width, height)
    }
    fn resize(&mut self, id: ContextId, width: u32, height: u32) {
        self.service.resize(id, width, height)
    }
    fn destroy(&mut self, id: ContextId) {
        self.service.destroy(id)
    }
    fn submit(&mut self, batch: Vec<(ContextId, Command)>) {
        self.service.submit(batch)
    }
    fn query(&mut self, id: ContextId, query: &Query) -> Reply {
        self.service.query(id, query)
    }
    fn present(&mut self, id: ContextId) -> Option<Readback> {
        self.service.present(id)
    }
    fn lost(&mut self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }
}

/// The process running a page's contexts ends: the context is lost (`webglcontextlost`), and,
/// as the page canceled the event, restored once a new context can be made, tried again a second
/// after each attempt that could not (`webglcontextrestored`).
#[test]
fn a_context_lost_with_its_gpu_process_is_restored_once_another_runs() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let gone = Arc::new(AtomicBool::new(false));
    let gpu = Arc::new(Gpu { gone: gone.clone() }) as Arc<dyn WebGlProvider>;
    let mut page = page(&net, Some(gpu));
    assert_eq!(eval(&mut page, SETUP), "true");
    assert_eq!(eval(&mut page, "green()"), "0,255,0,255");
    gone.store(true, Ordering::SeqCst);
    // (the page sees it at its next rendering update)
    eval(&mut page, "gl.clear(gl.COLOR_BUFFER_BIT)");
    assert!(until(&mut page, "log.length == 1", Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "log[0]"), "lost true \"\" true true");
    // The first attempt, a second after the event, finds no process to make the context in.
    assert!(!until(
        &mut page,
        "log.length > 1",
        Duration::from_millis(1500)
    ));
    assert_eq!(
        eval(&mut page, "[gl.isContextLost(), log.length].join(' ')"),
        "true 1"
    );
    gone.store(false, Ordering::SeqCst);
    assert!(until(&mut page, "log.length == 2", Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "log[1]"), "restored false");
    assert_eq!(eval(&mut page, "green()"), "0,255,0,255");
}
