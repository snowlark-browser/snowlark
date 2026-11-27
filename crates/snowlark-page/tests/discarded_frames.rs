// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Removed iframes are freed: once script no longer reaches anything of a discarded browsing context
//! (HTML §7.3.1.5 "destroy a child navigable"), its realm, Window, document and nodes go, so a page
//! that keeps making and removing frames (as benchmark harnesses do, one frame per run) runs in a
//! bounded heap with a bounded object table.

use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions};

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Collects until nothing more is freed.
fn collect(page: &mut Page) {
    for _ in 0..4 {
        assert!(page.collect_garbage());
    }
}

/// The frame's document: a script holding about 2 MB, and (`listeners`) listeners on its window, its
/// document and its elements whose closures reach those nodes, as an application framework adds.
fn frame_source(listeners: bool) -> String {
    let mut s = String::from(
        "<!doctype html><ul id=list><li>a<li>b<li>c</ul><script>\
         window.big = Array.from({length: 20000}, (_, i) => ({i, s: 'x'.repeat(50) + i}));",
    );
    if listeners {
        s.push_str(
            "const list = document.getElementById('list');\
             window.addEventListener('resize', () => list.dataset.w = innerWidth);\
             document.addEventListener('click', e => { if (e.target === list) list.remove(); });\
             for (const li of list.children) li.addEventListener('click', () => li.classList.toggle('done'));\
             list.onclick = () => big.length;",
        );
    }
    s.push_str("</script>");
    s
}

/// Runs the event loop until the frame `f` has fired its `load` event (the caller sets
/// `window.loaded = false; f.onload = () => loaded = true` first), then lets what is ready run (frame
/// callbacks, observers) for at most 300 ms. Not `run_until_settled(10 s)`: a script that registered a
/// timer a minute out keeps the loop "not settled" until the deadline, which is a wait, not a check.
fn wait_for_frame(page: &mut Page) {
    let deadline = Instant::now() + Duration::from_secs(10);
    assert!(
        page.run_until(deadline, |p| p.eval("loaded", "test.js").as_deref()
            == Ok("true")),
        "the frame loaded"
    );
    page.run_until_settled(Duration::from_millis(300));
}

/// Makes a frame of `source`, waits for its load, removes it; `rounds` times. Returns the V8 heap and
/// the live node and object counts after each round's collection.
fn churn(page: &mut Page, source: &str, rounds: usize) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for round in 0..rounds {
        eval(
            page,
            &format!(
                "window.loaded = false;\
                 window.f = document.createElement('iframe');\
                 f.onload = () => loaded = true;\
                 f.srcdoc = {};\
                 document.body.appendChild(f); 'ok'",
                serde_json::to_string(source).unwrap()
            ),
        );
        wait_for_frame(page);
        assert_eq!(
            eval(
                page,
                "loaded + ' ' + (f.contentWindow.big ? f.contentWindow.big.length : 0)\
                 + (f.contentWindow.err ? ' ' + f.contentWindow.err : '')"
            ),
            if source.contains("<script>") {
                "true 20000"
            } else {
                "true 0"
            },
            "round {round}: the frame loaded and ran its script"
        );
        eval(page, "f.remove(); f = null; 'ok'");
        page.run_until_settled(Duration::from_secs(1));
        collect(page);
        let (nodes, objects) = page.engine().live_counts();
        out.push((page.heap_used(), nodes, objects));
    }
    out
}

fn page(net: &NetThread) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    collect(&mut page);
    page
}

/// Growth from the second round to the last: nothing of the rounds in between may stay but each
/// Window's object slot (kept, never reused: tasks queued for a Window carry its handle).
fn check(name: &str, rounds: &[(usize, usize, usize)]) {
    for (i, (heap, nodes, objects)) in rounds.iter().enumerate() {
        eprintln!(
            "{name} round {i}: heap {} KiB, {nodes} nodes, {objects} objects",
            heap >> 10
        );
    }
    let (h1, n1, o1) = rounds[1];
    let (hn, nn, on) = *rounds.last().unwrap();
    assert!(
        nn <= n1 && on <= o1 + 8 && hn <= h1 + (1 << 20),
        "{name}: the removed frames stayed (round 1: heap {} KiB, {n1} nodes, {o1} objects; \
         last: heap {} KiB, {nn} nodes, {on} objects)",
        h1 >> 10,
        hn >> 10
    );
}

#[test]
fn removed_frames_are_freed() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut results = Vec::new();
    for (name, source) in [
        ("empty", "<!doctype html><p>x".to_owned()),
        ("script", frame_source(false)),
        ("listeners", frame_source(true)),
    ] {
        let mut page = page(&net);
        let rounds = churn(&mut page, &source, 8);
        results.push((name, rounds));
    }
    for (name, rounds) in &results {
        check(name, rounds);
    }
}

/// DOM "inner invoke": a listener does not run when its target's global is a Window whose document is
/// not fully active, even with a callback of a live realm (WPT
/// dom/events/event-listener-not-fully-active-target.html).
#[test]
fn a_removed_frames_targets_run_no_listeners() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    eval(
        &mut page,
        "window.loaded = false; window.f = document.createElement('iframe');         f.onload = () => loaded = true; f.srcdoc = '<p id=p>x';         document.body.appendChild(f); 'ok'",
    );
    wait_for_frame(&mut page);
    assert_eq!(
        eval(
            &mut page,
            "window.n = 0; var count = () => n++;             var p = f.contentDocument.getElementById('p');             var t = new f.contentWindow.EventTarget();             p.addEventListener('click', count); t.addEventListener('x', count);             p.click(); t.dispatchEvent(new Event('x'));             f.remove();             p.click(); t.dispatchEvent(new Event('x'));             String(n)"
        ),
        "2"
    );
}

#[test]
fn a_removed_frames_window_stays_usable_while_referenced() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    eval(
        &mut page,
        &format!(
            "window.loaded = false; window.f = document.createElement('iframe');\
             f.onload = () => loaded = true; f.srcdoc = {};\
             document.body.appendChild(f); 'ok'",
            serde_json::to_string(&frame_source(true)).unwrap()
        ),
    );
    wait_for_frame(&mut page);
    eval(
        &mut page,
        "window.w = f.contentWindow; window.d = f.contentDocument; window.li = d.querySelector('li');\
         f.remove(); f = null; 'ok'",
    );
    collect(&mut page);
    // The Window, its document, their nodes and the realm's objects are all still there, but its
    // listeners do not run (DOM "inner invoke": their global's document is not fully active; WPT
    // dom/events/event-listener-not-fully-active-callback.html).
    assert_eq!(
        eval(
            &mut page,
            "li.click(); [w.big.length, d.getElementById('list').children.length, li.className,\
             li.textContent, w.document === d].join()"
        ),
        "20000,3,,a,true"
    );
    // Once nothing reaches them they go.
    let (nodes, _) = page.engine().live_counts();
    eval(&mut page, "w = d = li = null; 'ok'");
    collect(&mut page);
    let (after, _) = page.engine().live_counts();
    assert!(after < nodes, "{after} nodes, {nodes} before");
}

/// A removed frame's NodeIterator and Range over a subtree that script removed from the frame's
/// document keep that subtree while the page holds them (DOM §6.1 NodeIterator's root and reference,
/// §5.5 a range's boundary points): the subtree is freed with the frame's group, not before it.
#[test]
fn a_removed_frames_iterator_keeps_its_removed_subtree() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    frame_of_script(&mut page, "parent.d = document;");
    eval(
        &mut page,
        "{ const b = d.body; b.append(d.createElement('p'), d.createElement('q'));\
         b.remove(); window.it = d.createNodeIterator(b); it.nextNode(); it.nextNode();\
         window.r = d.createRange(); r.setStart(b.lastChild, 0); } d = null; 'ok'",
    );
    collect(&mut page);
    assert_eq!(
        eval(
            &mut page,
            "[it.referenceNode.nodeName, it.root.nodeName, it.nextNode().nodeName,\
             r.startContainer.nodeName, r.startContainer.parentNode.nodeName].join()"
        ),
        "P,BODY,Q,Q,BODY"
    );
    // A removal elsewhere runs every live iterator's and range's removing steps.
    eval(
        &mut page,
        "document.body.appendChild(document.createElement('i')).remove(); 'ok'",
    );
    eval(&mut page, "it = r = null; 'ok'");
    for _ in 0..4 {
        assert!(page.collect_garbage());
        eval(
            &mut page,
            "document.body.appendChild(document.createElement('i')).remove(); 'ok'",
        );
    }
    assert!(page.frames_alive().is_empty(), "{:?}", page.frames_alive());
}

/// Makes a frame of `script` (run in the frame), waits for its load, and removes it.
fn frame_of_script(page: &mut Page, script: &str) {
    let source = format!("<script>window.big = Array(20000).fill(1); {script}</script>");
    eval(
        page,
        &format!(
            "window.loaded = false; window.f = document.createElement('iframe'); f.onload = () => loaded = true; f.srcdoc = {};\
             document.body.appendChild(f); 'ok'",
            serde_json::to_string(&source).unwrap()
        ),
    );
    wait_for_frame(page);
    eval(page, "f.remove(); f = null; 'ok'");
    collect(page);
}

/// A removed frame that the page keeps through a signal it holds, whose abort reason is of the
/// frame's realm, lives while the page holds the signal and goes once it lets go (the reason is
/// kept by the signal's wrapper, not by a strong handle of the engine).
#[test]
fn a_removed_frame_kept_by_a_signal_goes_with_it() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    frame_of_script(&mut page, "parent.kept = AbortSignal.abort({big});");
    assert_eq!(page.frames_alive().values().sum::<usize>(), 1);
    assert_eq!(eval(&mut page, "kept.reason.big.length"), "20000");
    eval(&mut page, "kept = null; 'ok'");
    collect(&mut page);
    assert!(page.frames_alive().is_empty(), "{:?}", page.frames_alive());
}

/// An event of a removed frame's realm is freed on its own once its wrapper dies, while the page still
/// holds that frame. The slot it leaves is taken by another frame's object, which must not count as
/// the first frame's: it would pin its own frame's document from a group that frame does not belong to.
#[test]
fn an_object_in_a_slot_a_removed_frames_event_left_is_not_that_frames() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    frame_of_script(
        &mut page,
        "parent.a = window; window.keep = Array.from({length: 500}, () => new Event('x'));",
    );
    assert_eq!(page.frames_alive().values().sum::<usize>(), 1);
    eval(&mut page, "a.keep = null; 'ok'");
    collect(&mut page);
    frame_of_script(
        &mut page,
        "window.ranges = Array.from({length: 500}, () => document.createRange());",
    );
    assert_eq!(
        page.frames_alive().values().sum::<usize>(),
        1,
        "only the frame the page holds stays: {:?}",
        page.frames_alive()
    );
    eval(&mut page, "a = null; 'ok'");
    collect(&mut page);
    assert!(page.frames_alive().is_empty(), "{:?}", page.frames_alive());
}

/// The GC report names, by field, the strong JS values the engine keeps of a removed frame's realm:
/// here a chunk queued in a stream the page holds.
#[test]
fn what_the_engine_keeps_of_a_removed_frame_is_named() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    frame_of_script(
        &mut page,
        "parent.kept = new ReadableStream({start(c) { c.enqueue({big}); }});",
    );
    assert_eq!(page.frames_alive().values().sum::<usize>(), 1);
    let report = page.gc_report_text().unwrap().join("\n");
    assert!(
        report.contains("\"ReadableStream queue\": 1"),
        "the report names the queued chunk:\n{report}"
    );
}

/// What frameworks leave in a frame's realm, each in frames that are removed in turn: the frames go
/// (their realm's heap and their nodes) whatever their scripts registered. Prints a line per kind
/// (heap, nodes and objects after the second round and after the last) and fails listing the kinds
/// whose frames stayed.
#[test]
fn removed_frames_are_freed_whatever_their_scripts_registered() {
    let kinds: &[(&str, &str)] = &[
        ("plain", ""),
        (
            "MutationObserver",
            "new MutationObserver(() => big.length).observe(document.body, {childList: true, subtree: true});
             document.body.appendChild(document.createElement('p'));",
        ),
        (
            "ResizeObserver",
            "new ResizeObserver(() => big.length).observe(document.body);",
        ),
        (
            "IntersectionObserver",
            "new IntersectionObserver(() => big.length).observe(document.body);",
        ),
        (
            "PerformanceObserver",
            "new PerformanceObserver(() => big.length).observe({entryTypes: ['mark']}); performance.mark('m');",
        ),
        (
            "custom elements",
            "customElements.define('x-a', class extends HTMLElement {
               connectedCallback() { this.textContent = big.length; }
               attributeChangedCallback() { big.length; }
               static get observedAttributes() { return ['v']; }
             });
             document.body.appendChild(document.createElement('x-a')).setAttribute('v', '1');
             customElements.whenDefined('x-b').then(() => big.length);",
        ),
        (
            "shadow roots and adopted sheets",
            "const s = new CSSStyleSheet(); s.replaceSync('p { color: red }');
             document.adoptedStyleSheets = [s];
             const r = document.body.appendChild(document.createElement('div')).attachShadow({mode: 'open'});
             r.adoptedStyleSheets = [s]; r.innerHTML = '<p>x</p><slot></slot>';
             r.addEventListener('click', () => big.length);",
        ),
        (
            "timers and frames",
            "setInterval(() => big.length, 100000); setTimeout(() => big.length, 100000);
             requestAnimationFrame(() => big.length); requestIdleCallback(() => big.length);
             queueMicrotask(() => big.length);",
        ),
        (
            "matchMedia",
            "const m = matchMedia('(min-width: 1px)');
             m.addEventListener('change', () => big.length); m.addListener(() => big.length);
             m.onchange = () => big.length;",
        ),
        (
            "AbortController",
            "const c = new AbortController();
             c.signal.addEventListener('abort', () => big.length);
             document.addEventListener('click', () => big.length, {signal: c.signal});
             window.c = c;",
        ),
        (
            "fetch and XHR",
            "window.p = new Promise(() => {});
             fetch('data:,x').then(r => r.text()).then(() => big.length);
             const x = new XMLHttpRequest(); x.open('GET', 'data:,x');
             x.onload = () => big.length; x.send();
             window.h = new Headers({a: 'b'}); window.u = new URL('http://a/');
             window.t = new TextEncoder();",
        ),
        (
            "MessageChannel",
            "const ch = new MessageChannel(); ch.port1.onmessage = () => big.length;
             ch.port2.postMessage(1); window.ch = ch;",
        ),
        (
            "animations",
            "document.body.animate([{opacity: 0}, {opacity: 1}], {duration: 100000, iterations: Infinity})
               .onfinish = () => big.length;",
        ),
        (
            "history and storage",
            "history.replaceState({big: big.length}, ''); sessionStorage.setItem('k', 'v');
             window.addEventListener('storage', () => big.length);",
        ),
        (
            "events it keeps",
            "window.evs = [];
             document.body.addEventListener('click', e => evs.push(e));
             for (let i = 0; i < 3; i++)
               document.body.dispatchEvent(new MouseEvent('click', {bubbles: true}));
             window.addEventListener('hashchange', e => evs.push(e)); location.hash = 'x';",
        ),
        (
            "module scripts",
            "const s = document.createElement('script'); s.type = 'module';
             s.textContent = 'window.mod = () => big.length; export {};';
             document.head.appendChild(s);
             import('data:text/javascript,export default 1').then(m => { window.mm = m; });",
        ),
        (
            // (Svelte's createEventDispatcher: a CustomEvent per call, its detail of the realm,
            // handed to callbacks and dropped)
            "custom event details",
            "const add = e => e.detail.f();
             for (let i = 0; i < 5; i++)
               add(new CustomEvent('add', {detail: {text: 'item ' + i, f: () => big.length}}));
             document.body.dispatchEvent(new MessageEvent('message', {data: {big}}));
             document.body.dispatchEvent(new PopStateEvent('popstate', {state: {big}}));
             document.body.dispatchEvent(new ErrorEvent('error', {error: {big}}));",
        ),
        (
            // (jQuery's support test and parseHTML: documents of their own, dropped)
            "documents it created",
            "{ const b = document.implementation.createHTMLDocument('').body;
               b.innerHTML = '<form></form><form></form>'; }
             new DOMParser().parseFromString('<p>x</p>', 'text/html').body.onclick = () => big.length;",
        ),
        (
            "trusted types policies",
            "window.tp = trustedTypes.createPolicy('p', {createHTML: s => s + big.length});
             document.body.innerHTML = tp.createHTML('<p>x</p>');",
        ),
        (
            "unhandled rejections",
            "Promise.reject(new Error(String(big.length)));",
        ),
        (
            "uncaught exceptions",
            "setTimeout(() => { throw new Error(String(big.length)); });",
        ),
        (
            "transitions, effects and entries it keeps",
            "document.body.style.transition = 'opacity 100s';
             getComputedStyle(document.body).opacity; document.body.style.opacity = '0.5';
             window.as = document.getAnimations();
             window.k = new KeyframeEffect(document.body, [{opacity: 0}], 100000);
             new IntersectionObserver(es => { window.es = es; }).observe(document.body);",
        ),
    ];
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut failed = Vec::new();
    for (name, script) in kinds {
        let mut page = Page::new(PageOptions {
            net: Some(net.client()),
            render: Some(snowlark_page::RenderSetup::new().unwrap()),
            ..Default::default()
        });
        page.load_html(
            "<!doctype html><body>",
            &url::Url::parse("http://example.test/").unwrap(),
        );
        assert!(page.run_until_settled(Duration::from_secs(5)));
        collect(&mut page);
        let source = format!(
            "<!doctype html><ul id=list><li>a<li>b</ul><script>\
             window.big = Array.from({{length: 20000}}, (_, i) => ({{i, s: 'x'.repeat(50) + i}}));\
             const list = document.getElementById('list');\
             list.addEventListener('click', () => list.remove());\
             try {{ {script} }} catch (e) {{ window.err = String(e); }}</script>"
        );
        let rounds = churn(&mut page, &source, 6);
        let (h1, n1, o1) = rounds[1];
        let (hn, nn, on) = *rounds.last().unwrap();
        // (the Rust side of platform objects that are never freed stays, holding nothing of the
        // realm: objects are reported, the heap and the nodes decide)
        let leaks = !(nn <= n1 && hn <= h1 + (1 << 20));
        eprintln!(
            "{:<34} round 1: heap {:>6} KiB {:>4} nodes {:>4} objects; last: heap {:>6} KiB {:>4} nodes {:>4} objects{}",
            name,
            h1 >> 10,
            n1,
            o1,
            hn >> 10,
            nn,
            on,
            if leaks { "  LEAKS" } else { "" }
        );
        if leaks {
            page.gc_report();
            failed.push(*name);
        }
    }
    assert!(failed.is_empty(), "the frames stayed with: {failed:?}");
}

/// A frame navigated from page to page (as benchmark harnesses do, one page per suite): each
/// navigation gives it a new Window and realm, and the previous ones go once nothing reaches them.
#[test]
fn a_navigated_frame_releases_its_previous_windows() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    eval(
        &mut page,
        "window.f = document.createElement('iframe'); f.srcdoc = '<p>first';
         document.body.appendChild(f); 'ok'",
    );
    page.run_until_settled(Duration::from_secs(10));
    let mut rounds = Vec::new();
    for round in 0..8 {
        eval(
            &mut page,
            &format!(
                "window.loaded = false; f.onload = () => loaded = true; f.srcdoc = {}; 'ok'",
                serde_json::to_string(&frame_source(true)).unwrap()
            ),
        );
        wait_for_frame(&mut page);
        assert_eq!(
            eval(&mut page, "loaded + ' ' + f.contentWindow.big.length"),
            "true 20000",
            "round {round}: the page loaded"
        );
        collect(&mut page);
        let (nodes, objects) = page.engine().live_counts();
        rounds.push((page.heap_used(), nodes, objects));
    }
    check("navigated", &rounds);
}

/// A task queued for a frame's document can outlive it: the frame's script posts to its parent, so
/// the message is ahead of the frame's `DOMContentLoaded` task, the parent removes the frame, and a
/// collection frees the document before that task runs (a tab crashed on reddit with a stale
/// `NodeId` in `dispatch`, from `advance_document`).
#[test]
fn tasks_of_a_freed_document_do_not_reach_it() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><script>window.gone = 0;\
         onmessage = () => { const f = document.querySelector('iframe');\
         f.contentWindow.postMessage('y', '*'); f.remove(); gone = 1; };</script>\
         <iframe srcdoc=\"<!doctype html><script>parent.postMessage('x', '*')</script>\"></iframe>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    let start = Instant::now();
    let mut after = 0;
    while after < 50 && start.elapsed() < Duration::from_secs(10) {
        page.turn();
        // (every turn: the document has to go between the message and the task behind it)
        collect(&mut page);
        if eval(&mut page, "gone + ''") == "1" {
            after += 1;
        }
    }
    assert!(after > 0, "the frame was removed");
}

/// "Destroy a document" removes its tasks whether or not anything collects it: script that keeps the
/// document sees no more of its load, and the parent's own load does not wait for it.
#[test]
fn a_removed_frame_gets_no_more_of_its_document_tasks() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><script>window.fired = ''; window.loaded = 0; window.gone = 0;\
         addEventListener('load', () => loaded = 1);\
         onmessage = () => { const f = document.querySelector('iframe');\
         window.kept = f.contentDocument; const w = f.contentWindow;\
         kept.addEventListener('DOMContentLoaded', () => fired += 'd');\
         w.addEventListener('load', () => fired += 'l');\
         f.remove(); gone = 1; };</script>\
         <iframe srcdoc=\"<!doctype html><script>parent.postMessage('x', '*')</script>\"></iframe>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    let start = Instant::now();
    while eval(&mut page, "loaded + ''") != "1" && start.elapsed() < Duration::from_secs(10) {
        page.turn();
    }
    assert_eq!(eval(&mut page, "gone + ''"), "1", "the frame was removed");
    assert_eq!(
        eval(&mut page, "loaded + ''"),
        "1",
        "the parent finished loading"
    );
    assert_eq!(eval(&mut page, "fired"), "");
}

/// A frame whose items run CSS transitions (TodoMVC's completed label, `transition: color 0.4s`): their
/// `CSSTransition` and `KeyframeEffect` objects (CSS Transitions 2 §3) go with the frame, whether the
/// transitions finished before it was removed or were still running.
#[test]
fn a_removed_frames_transitions_go_with_it() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut results = Vec::new();
    for (name, duration) in [("finished", "0.05s"), ("running", "10s")] {
        let source = format!(
            "<!doctype html><style>li {{ color: red; transition: color {duration} }} li.done {{ color: blue }}</style>\
             <ul id=list></ul><script>\
             window.big = {{ length: 20000 }};\
             const list = document.getElementById('list');\
             for (let i = 0; i < 100; i++) list.appendChild(document.createElement('li')).textContent = i;\
             getComputedStyle(list.firstChild).color;\
             for (const li of list.children) li.className = 'done';\
             getComputedStyle(list.firstChild).color;\
             for (let i = 0; i < 50; i++) list.lastChild.remove();\
             </script>"
        );
        let mut page = page(&net);
        let rounds = churn(&mut page, &source, 6);
        results.push((name, rounds));
    }
    for (name, rounds) in &results {
        check(name, rounds);
    }
}

/// Snapshots keep the removed frames' groups and ask only the objects allocated since: an event the
/// frame's realm makes after its group was found, dispatched at one of its nodes and kept only by its
/// Window, joins the group, so the frame still goes once the page lets go of it.
#[test]
fn a_removed_frames_objects_made_after_its_group_go_with_it() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    frame_of_script(&mut page, "parent.a = window;");
    assert_eq!(page.frames_alive().values().sum::<usize>(), 1);
    eval(
        &mut page,
        "a.kept = Array.from({length: 50}, () => { const e = new a.Event('x');\
         a.document.body.dispatchEvent(e); return e; }); a = null; 'ok'",
    );
    collect(&mut page);
    assert!(page.frames_alive().is_empty(), "{:?}", page.frames_alive());
}

/// An object older than the group that comes to refer into it (an event of the page dispatched at
/// the removed frame's node, then kept only by the frame's Window) joins it at the next walk of
/// every object, at most `GROUPING_FRESH_FOR` later: the frame goes then.
#[test]
fn a_removed_frame_goes_once_an_older_object_that_refers_into_it_joins() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = page(&net);
    eval(&mut page, "window.ev = new Event('x'); 'ok'");
    frame_of_script(&mut page, "parent.a = window;");
    eval(
        &mut page,
        "a.document.body.dispatchEvent(ev); a.kept = ev; ev = null; a = null; 'ok'",
    );
    std::thread::sleep(Duration::from_millis(2100));
    collect(&mut page);
    assert!(page.frames_alive().is_empty(), "{:?}", page.frames_alive());
}
