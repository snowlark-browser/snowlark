// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WebAssembly Web API (§3): `WebAssembly.compileStreaming` and `instantiateStreaming` take a
//! Response, or a promise of one, and compile its body ("compile a potential WebAssembly
//! response"): only an `application/wasm`, CORS-same-origin response with an ok status whose body
//! was not read.

use std::time::{Duration, Instant};

use snowlark_page::{Page, PageOptions};

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Runs `js`, which sets `result` when it is done, and returns `result`.
fn run(js: &str) -> String {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><body>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    eval(
        &mut page,
        &format!(
            "var result = undefined;
             var keep = setTimeout(() => {{}}, 60000);
             var bytes = new Uint8Array([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0]);
             var wasm = (init = {{}}) => new Response(bytes,
               {{ headers: {{ 'Content-Type': 'application/wasm' }}, ...init }});
             var outcome = p => p.then(v => 'ok ' + v, e => e.name + ': ' + e.message);
             (async () => {{ {js} }})().then(r => {{ result = r; clearTimeout(keep); }},
                                            e => {{ result = 'threw ' + e; clearTimeout(keep); }});"
        ),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let done = page.run_until(deadline, |p| {
        p.eval("result !== undefined", "test.js").ok().as_deref() == Some("true")
    });
    assert!(done, "{js}: never settled");
    eval(&mut page, "result")
}

#[test]
fn the_streaming_members_exist() {
    assert_eq!(
        run("return [typeof WebAssembly.compileStreaming, typeof WebAssembly.instantiateStreaming].join()"),
        "function,function"
    );
}

#[test]
fn compile_streaming_compiles_a_wasm_response() {
    assert_eq!(
        run("const m = await WebAssembly.compileStreaming(wasm());
             return m instanceof WebAssembly.Module;"),
        "true"
    );
    // (a promise of a Response, and a MIME type with spaces and other case)
    assert_eq!(
        run("const r = await WebAssembly.instantiateStreaming(Promise.resolve(
               wasm({ headers: { 'Content-Type': ' Application/WASM\\t' } })));
             return [r.module instanceof WebAssembly.Module, r.instance instanceof WebAssembly.Instance].join();"),
        "true,true"
    );
}

#[test]
fn the_response_must_be_usable_wasm() {
    let type_error = |js: &str| {
        let r = run(js);
        assert!(r.starts_with("TypeError: "), "{js}: {r}");
    };
    type_error("return outcome(WebAssembly.compileStreaming(bytes));");
    type_error("return outcome(WebAssembly.compileStreaming(new Response(bytes)));");
    type_error(
        "return outcome(WebAssembly.compileStreaming(
           wasm({ headers: { 'Content-Type': 'application/wasm; charset=utf-8' } })));",
    );
    type_error("return outcome(WebAssembly.compileStreaming(wasm({ status: 404 })));");
    type_error(
        "const r = wasm(); await r.arrayBuffer();
         return outcome(WebAssembly.compileStreaming(r));",
    );
    type_error("return outcome(WebAssembly.compileStreaming(Response.error()));");
}

#[test]
fn a_rejected_source_rejects_with_its_reason() {
    assert_eq!(
        run("return outcome(WebAssembly.compileStreaming(Promise.reject(new RangeError('no'))));"),
        "RangeError: no"
    );
}

#[test]
fn invalid_bytes_reject_with_a_compile_error() {
    assert_eq!(
        run("const r = new Response(new Uint8Array([1, 2, 3]),
               { headers: { 'Content-Type': 'application/wasm' } });
             const e = await WebAssembly.compileStreaming(r).catch(e => e);
             return e instanceof WebAssembly.CompileError;"),
        "true"
    );
}
