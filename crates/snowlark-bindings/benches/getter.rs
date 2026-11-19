// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The cost of a generated attribute getter from optimized JS, with and without its Fast API C function:
//! `el.nodeType` (fast-eligible: a primitive from the receiver) installed both ways, plus `el.tagName`
//! (a string, always the slow path) and a plain data property for reference.
//!
//! `.\dev.ps1 bench --package snowlark-bindings`

#[path = "../tests/common/mod.rs"]
mod common;

use common::{eval, Mock};
use snowlark_bindings::{rt, InterfaceId};
use std::rc::Rc;
use std::time::Instant;

const N: u32 = 20_000_000;

fn measure(scope: &mut v8::PinScope, expr: &str) -> f64 {
    let src = format!(
        "globalThis.__run = function (n) {{ let s = 0; for (let i = 0; i < n; i++) s += ({expr}) ? 1 : 0; return s; }};"
    );
    eval(scope, &src).unwrap();
    // Warm up until optimized.
    for _ in 0..5 {
        eval(scope, "__run(200000)").unwrap();
    }
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let r = eval(scope, &format!("__run({N})")).unwrap();
        let ns = t.elapsed().as_nanos() as f64 / N as f64;
        assert_eq!(r, N.to_string());
        best = best.min(ns);
    }
    best
}

fn run(fast_api: bool) -> Vec<(&'static str, f64)> {
    let isolate = &mut v8::Isolate::new(Default::default());
    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let mock = Rc::new(Mock::default());
    let div = mock.add_element(InterfaceId::HTMLDivElement, "div");
    let options = rt::InstallOptions {
        fast_api,
        ..Default::default()
    };
    snowlark_bindings::install(scope, mock, options).unwrap();
    let w = rt::wrap(scope, div).unwrap();
    let global = context.global(scope);
    let k = v8::String::new(scope, "el").unwrap();
    global.set(scope, k.into(), w.into()).unwrap();
    eval(scope, "globalThis.plain = { nodeType: 1 }").unwrap();
    vec![
        ("empty loop", measure(scope, "true")),
        (
            "plain.nodeType (JS data property)",
            measure(scope, "plain.nodeType === 1"),
        ),
        ("el.nodeType", measure(scope, "el.nodeType === 1")),
        (
            "el.tagName (string, slow path)",
            measure(scope, "el.tagName.length === 3"),
        ),
    ]
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("run with --release (cargo bench)");
        return;
    }
    snowlark_js::init();
    let fast = run(true);
    let slow = run(false);
    println!(
        "{:<40} {:>12} {:>12}",
        "ns per iteration (best of 5)", "Fast API on", "Fast API off"
    );
    for ((name, f), (_, s)) in fast.iter().zip(&slow) {
        println!("{name:<40} {f:>12.2} {s:>12.2}");
    }
}
