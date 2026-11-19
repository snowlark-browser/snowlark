// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DOM micro-benchmarks, run on demand: `.\dev.ps1 test --release --package snowlark-page --test it bench::
//! -- --ignored --nocapture`. They print ns per operation for the DOM alone and through the bindings.

use std::time::Instant;

use snowlark_page::{Page, PageOptions};

fn report(name: &str, n: usize, f: impl FnOnce()) {
    let t = Instant::now();
    f();
    let ns = t.elapsed().as_nanos() as f64 / n as f64;
    println!("{name}: {ns:.1} ns/op");
}

#[test]
#[ignore]
fn dom_create_and_append() {
    let n = 200_000;
    let mut dom = snowlark_dom::Dom::new();
    let doc = dom.create_html_document(None);
    report("dom: create_element + append_child", n, || {
        let root = dom.create_element(doc, "div", None).unwrap();
        for _ in 0..n {
            let e = dom.create_element(doc, "div", None).unwrap();
            dom.append_child(root, e).unwrap();
        }
    });
}

#[test]
#[ignore]
fn js_create_and_append() {
    let mut page = Page::new(PageOptions::default());
    let r = page
        .eval(
            r#"
            const out = [];
            function time(name, n, f) {
                f(Math.min(n, 10000));
                const t0 = performance.now();
                f(n);
                out.push(name + ': ' + ((performance.now() - t0) * 1e6 / n).toFixed(1) + ' ns/op');
            }
            time('js: createElement + appendChild', 200000, n => {
                const root = document.createElement('div');
                for (let i = 0; i < n; i++) root.appendChild(document.createElement('div'));
            });
            time('js: createElement', 200000, n => {
                for (let i = 0; i < n; i++) document.createElement('div');
            });
            const c = document.createElement('span');
            document.createElement('div').appendChild(c);
            time('js: parentNode', 2000000, n => {
                let x;
                for (let i = 0; i < n; i++) x = c.parentNode;
                return x;
            });
            time('js: nodeType', 2000000, n => {
                let x;
                for (let i = 0; i < n; i++) x = c.nodeType;
                return x;
            });
            out.join('\n')
            "#,
            "bench.js",
        )
        .unwrap();
    println!("{r}");
}

#[test]
#[ignore]
fn wrapper_creation() {
    let mut page = Page::new(PageOptions::default());
    let n = 200_000;
    page.with(|e, cx| {
        let doc = e.document().unwrap();
        let nodes: Vec<_> = {
            let mut dom = e.dom_mut();
            (0..n).map(|_| dom.create_text_node(doc, "x")).collect()
        };
        let scope = cx.scope();
        report("rust: rt::wrap of new nodes", n, || {
            for &id in &nodes {
                v8::scope!(let s, scope);
                let _ = snowlark_bindings::rt::wrap(s, snowlark_page::engine::node_handle(id));
            }
        });
        report("rust: rt::wrap of wrapped nodes", n, || {
            for &id in &nodes {
                v8::scope!(let s, scope);
                let _ = snowlark_bindings::rt::wrap(s, snowlark_page::engine::node_handle(id));
            }
        });
        report("rust: dom create_text_node", n, || {
            let mut dom = e.dom_mut();
            for _ in 0..n {
                dom.create_text_node(doc, "x");
            }
        });
    });
}

#[test]
#[ignore]
fn create_element_parts() {
    use snowlark_bindings::rt::Host;
    let mut page = Page::new(PageOptions::default());
    let n = 200_000;
    page.with(|e, cx| {
        let doc = e.document().unwrap();
        report("rust: dom.create_element", n, || {
            for _ in 0..n {
                e.dom_mut().create_element(doc, "div", None).unwrap();
            }
        });
        report("rust: push+pop CE reactions", n, || {
            for _ in 0..n {
                e.push_ce_reactions();
                e.pop_ce_reactions(cx);
            }
        });
        report("rust: Host::interface_of", n, || {
            let el = e.dom_mut().create_element(doc, "div", None).unwrap();
            let h = snowlark_page::engine::node_handle(el);
            for _ in 0..n {
                std::hint::black_box(e.interface_of(h));
            }
        });
    });
}

/// The event dispatch cases of [`event_dispatch`]: `cases.push([name, n, f])`, and `time(i)`, one round
/// of case `i` in ns per operation.
const DISPATCH_CASES: &str = r#"
    window.cases = [];
    window.time = i => {
        const [, n, f] = cases[i];
        const t0 = performance.now();
        f(n);
        return (performance.now() - t0) * 1e6 / n;
    };
    window.suite = (label, w) => {
        const listen = w.eval('(t, type) => t.addEventListener(type, () => {})');
        const div = w.document.createElement('div');
        listen(div, 'x');
        const e = new w.Event('x');
        cases.push([label + ': dispatchEvent, 1 listener', 200000, n => {
            for (let i = 0; i < n; i++) div.dispatchEvent(e);
        }]);
        cases.push([label + ': new Event + dispatchEvent, 1 listener', 20000, n => {
            for (let i = 0; i < n; i++) div.dispatchEvent(new w.Event('x'));
        }]);
        const none = w.document.createElement('div');
        cases.push([label + ': dispatchEvent, no listener', 200000, n => {
            for (let i = 0; i < n; i++) none.dispatchEvent(e);
        }]);
        const many = w.document.createElement('div');
        for (let i = 0; i < 10; i++) listen(many, 'x');
        cases.push([label + ': dispatchEvent, 10 listeners', 50000, n => {
            for (let i = 0; i < n; i++) many.dispatchEvent(e);
        }]);
        const clicked = w.document.createElement('button');
        listen(clicked, 'click');
        cases.push([label + ': click(), event made by the engine', 20000, n => {
            for (let i = 0; i < n; i++) clicked.click();
        }]);
        cases.push([label + ': createElement', 20000, n => {
            for (let i = 0; i < n; i++) w.document.createElement('div');
        }]);
    };
    "#;

/// Prints the best of nine rounds of each case in `cases`, and empties it. Rounds of the cases
/// alternate, and the event loop turns between them, so that wrappers are collected as in a page.
fn dispatch_rounds(page: &mut Page) {
    let count: usize = page
        .eval("cases.length", "bench.js")
        .unwrap()
        .parse()
        .unwrap();
    let mut best = vec![f64::INFINITY; count];
    for round in 0..10 {
        for (i, b) in best.iter_mut().enumerate() {
            let ns: f64 = page
                .eval(&format!("time({i})"), "bench.js")
                .unwrap()
                .parse()
                .unwrap();
            // (the first round warms up)
            if round > 0 {
                *b = b.min(ns);
            }
            page.run_for(std::time::Duration::from_millis(2));
        }
    }
    for (i, b) in best.iter().enumerate() {
        let name = page.eval(&format!("cases[{i}][0]"), "bench.js").unwrap();
        println!("{name}: {b:.1} ns/op");
    }
    page.eval("cases.length = 0", "bench.js").unwrap();
}

/// What dispatching an event costs, in a page of one realm and of two (Speedometer's suites run in a
/// frame), with the listener in the realm of the running code and in another one. The best of the
/// rounds: a machine shared with other work only adds to a time.
#[test]
#[ignore]
fn event_dispatch() {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><body>",
        &url::Url::parse("http://bench.test/").unwrap(),
    );
    page.eval(DISPATCH_CASES, "bench.js").unwrap();
    page.eval("suite('1 realm', window)", "bench.js").unwrap();
    dispatch_rounds(&mut page);
    page.eval(
        r#"
        const frame = document.createElement('iframe');
        document.body.appendChild(frame);
        const w = frame.contentWindow;
        suite('2 realms, top', window);
        suite('2 realms, frame', w);
        const div = w.document.createElement('div');
        w.eval('(d) => d.addEventListener("x", () => {})')(div);
        const e = new w.Event('x');
        const dispatch = EventTarget.prototype.dispatchEvent;
        cases.push(['realm switch: top code, frame listener', 200000, n => {
            for (let i = 0; i < n; i++) dispatch.call(div, e);
        }]);
        "#,
        "bench.js",
    )
    .unwrap();
    dispatch_rounds(&mut page);
}
