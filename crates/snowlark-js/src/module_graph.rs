// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Evaluating a graph of ES modules given as source texts, for code the browser runs for itself
//! (not a page's): an isolate of its own, a context with ECMAScript's globals only (no host
//! objects, no network, nothing of the process), a heap limit and a time limit. The content
//! blocker reads uBlock Origin's scriptlet modules this way: evaluating them is how their exported
//! table of functions is obtained (each function's source is then its `toString()`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

/// The modules of one evaluation: sources by name, compiled modules, and each module's name by
/// identity hash (to resolve a specifier against its referrer).
#[derive(Default)]
struct Graph {
    sources: HashMap<String, String>,
    compiled: HashMap<String, v8::Global<v8::Module>>,
    names: HashMap<i32, String>,
}

/// `spec` imported by the module named `referrer` (names are slash-separated paths).
fn join(referrer: &str, spec: &str) -> String {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return spec.to_owned();
    }
    let mut parts: Vec<&str> = referrer.split('/').collect();
    parts.pop();
    for p in spec.split('/') {
        match p {
            "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

fn throw(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    if let Some(m) = v8::String::new(scope, message) {
        let e = v8::Exception::error(scope, m);
        scope.throw_exception(e);
    }
}

fn module<'s>(scope: &mut v8::PinScope<'s, '_>, name: &str) -> Option<v8::Local<'s, v8::Module>> {
    let graph = scope.get_slot::<Rc<RefCell<Graph>>>()?.clone();
    if let Some(m) = graph.borrow().compiled.get(name) {
        return Some(v8::Local::new(scope, m));
    }
    let Some(source) = graph.borrow().sources.get(name).cloned() else {
        throw(scope, &format!("no module {name}"));
        return None;
    };
    let code = v8::String::new(scope, &source)?;
    let resource = v8::String::new(scope, name)?;
    let origin = v8::ScriptOrigin::new(
        scope,
        resource.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        true,
        None,
    );
    let mut src = v8::script_compiler::Source::new(code, Some(&origin));
    let m = v8::script_compiler::compile_module(scope, &mut src)?;
    let mut g = graph.borrow_mut();
    g.names.insert(m.get_identity_hash().get(), name.to_owned());
    g.compiled
        .insert(name.to_owned(), v8::Global::new(scope, m));
    Some(m)
}

fn resolve<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    specifier: &str,
    _ty: &str,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    let from = scope
        .get_slot::<Rc<RefCell<Graph>>>()?
        .borrow()
        .names
        .get(&referrer.get_identity_hash().get())
        .cloned()
        .unwrap_or_default();
    module(scope, &join(&from, specifier))
}

/// Evaluates the module `entry` of `sources` (module names to source texts; imports are resolved
/// relative to the importing module's name), then calls `then` (the source of a function) with
/// the entry's namespace and returns its result as a string. Fails with the exception's text, or
/// when the evaluation takes longer than `limit`.
pub fn eval_module_graph(
    sources: HashMap<String, String>,
    entry: &str,
    then: &str,
    limit: Duration,
) -> Result<String, String> {
    crate::init();
    let params = v8::CreateParams::default().heap_limits(0, 512 << 20);
    let mut isolate = v8::Isolate::new(params);
    let handle = isolate.thread_safe_handle();
    let (done, wait) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = wait.recv_timeout(limit) {
            handle.terminate_execution();
        }
    });
    crate::hooks::set_resolve_hook(&mut isolate, resolve);
    isolate.set_slot(Rc::new(RefCell::new(Graph {
        sources,
        ..Default::default()
    })));
    let result = {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, v8::ContextOptions::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(let tc, scope);
        let run = (|| -> Option<String> {
            let m = module(tc, entry)?;
            if crate::hooks::instantiate(tc, m) != Some(true) {
                return None;
            }
            m.evaluate(tc)?;
            tc.perform_microtask_checkpoint();
            if m.get_status() != v8::ModuleStatus::Evaluated {
                if m.get_status() == v8::ModuleStatus::Errored {
                    let e = m.get_exception();
                    tc.throw_exception(e);
                }
                return None;
            }
            let ns = m.get_module_namespace();
            let code = v8::String::new(tc, &format!("({then})"))?;
            let f = v8::Script::compile(tc, code, None)?.run(tc)?;
            let f = v8::Local::<v8::Function>::try_from(f).ok()?;
            let undefined = v8::undefined(tc).into();
            let r = f.call(tc, undefined, &[ns])?;
            Some(r.to_rust_string_lossy(tc))
        })();
        match run {
            Some(s) => Ok(s),
            None => Err(tc
                .exception()
                .map(|e| e.to_rust_string_lossy(tc))
                .unwrap_or_else(|| "the evaluation was stopped".into())),
        }
    };
    drop(done);
    let _ = watchdog.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(files: &[(&str, &str)]) -> HashMap<String, String> {
        files
            .iter()
            .map(|(n, s)| ((*n).to_owned(), (*s).to_owned()))
            .collect()
    }

    #[test]
    fn evaluates_a_graph_and_reads_its_exports() {
        let g = graph(&[
            (
                "res/main.js",
                "import { helper } from './lib/helper.js'; import './side.js';\n\
                 export const table = [{ name: 'a', fn: function a(x) { return helper(x); } }];",
            ),
            (
                "res/lib/helper.js",
                "export function helper(x) { return x + 1; }",
            ),
            ("res/side.js", "globalThis.sideEffect = 1;"),
        ]);
        let out = eval_module_graph(
            g,
            "res/main.js",
            "ns => JSON.stringify(ns.table.map(t => [t.name, t.fn.toString(), t.fn(1), typeof sideEffect]))",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            out,
            r#"[["a","function a(x) { return helper(x); }",2,"number"]]"#
        );
    }

    #[test]
    fn errors_missing_modules_and_endless_loops() {
        let e = eval_module_graph(
            graph(&[("m.js", "throw new Error('boom');")]),
            "m.js",
            "ns => 1",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(e.contains("boom"), "{e}");
        let e = eval_module_graph(
            graph(&[("m.js", "import './gone.js';")]),
            "m.js",
            "ns => 1",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(e.contains("gone.js"), "{e}");
        let e = eval_module_graph(
            graph(&[("m.js", "for (;;) {}")]),
            "m.js",
            "ns => 1",
            Duration::from_millis(200),
        );
        assert!(e.is_err());
        // Nothing of the host: no window, no fetch.
        let out = eval_module_graph(
            graph(&[(
                "m.js",
                "export const t = [typeof window, typeof fetch, typeof print].join();",
            )]),
            "m.js",
            "ns => ns.t",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out, "undefined,undefined,undefined");
    }
}
