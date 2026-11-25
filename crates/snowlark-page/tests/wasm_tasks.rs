// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WebAssembly JavaScript Interface's asynchronous `compile` and `instantiate` finish on a V8
//! background thread, which posts a foreground task to resolve the promise. Its arrival wakes the
//! page's event loop: a page that compiles modules one after another is not held to the pace of
//! the loop's other wake-ups (a timer, a frame).

use std::time::{Duration, Instant};

use snowlark_page::{Page, PageOptions};

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// 100 compilations, each started when the one before resolves, while a far timer keeps the loop
/// waiting. Woken only by that timer's 50 ms cap, they took 5 s; woken by each task, a few ms each.
#[test]
fn sequential_async_compiles_wake_the_loop() {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><body>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    eval(
        &mut page,
        "var done = 0;
         var keep = setTimeout(() => {}, 60000);
         var bytes = new Uint8Array([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0]);
         var chain = Promise.resolve();
         for (let i = 0; i < 100; i++) {
           chain = chain
             .then(() => WebAssembly.instantiate(bytes))
             .then(r => { if (r.instance instanceof WebAssembly.Instance) done++; });
         }
         chain.then(() => clearTimeout(keep));",
    );
    let started = Instant::now();
    let finished = page.run_until(started + Duration::from_secs(20), |p| {
        p.eval("done", "test.js").ok().as_deref() == Some("100")
    });
    let elapsed = started.elapsed();
    assert!(finished, "{} of 100 resolved", eval(&mut page, "done"));
    assert!(
        elapsed < Duration::from_millis(1500),
        "100 sequential compilations took {elapsed:?}"
    );
}
