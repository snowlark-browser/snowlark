// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Page-level tests: event dispatch order (DOM §2.9), event loop ordering (HTML §8.1.7), module graphs
//! fetched from an in-process server, and `document.write` from parser-inserted scripts.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page_with(html: &str) -> Page {
    let mut page = Page::new(PageOptions::default());
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

#[test]
fn dispatch_runs_capture_target_bubble_in_order() {
    let mut page = page_with("<!doctype html><div id=outer><span id=inner></span></div>");
    let r = eval(
        &mut page,
        r#"
        const log = [];
        const outer = document.getElementById('outer');
        const inner = document.getElementById('inner');
        const add = (t, name, capture) =>
            t.addEventListener('x', e => log.push(name + ':' + e.eventPhase), capture);
        add(window, 'win-c', true); add(window, 'win-b', false);
        add(document, 'doc-c', true); add(document, 'doc-b', false);
        add(outer, 'outer-b', false); add(outer, 'outer-c', true);
        add(inner, 'inner-b', false); add(inner, 'inner-c', true);
        inner.dispatchEvent(new Event('x', { bubbles: true }));
        log.join(' ')
        "#,
    );
    assert_eq!(
        r,
        "win-c:1 doc-c:1 outer-c:1 inner-c:2 inner-b:2 outer-b:3 doc-b:3 win-b:3"
    );
}

#[test]
fn stop_propagation_once_passive_and_signal() {
    let mut page = page_with("<!doctype html><div id=a><b id=b></b></div>");
    let r = eval(
        &mut page,
        r#"
        const log = [];
        const a = document.getElementById('a'), b = document.getElementById('b');
        b.addEventListener('x', () => log.push('b1'), { once: true });
        b.addEventListener('x', e => { log.push('b2'); e.stopPropagation(); });
        b.addEventListener('x', () => log.push('b3'));
        a.addEventListener('x', () => log.push('a'));
        b.dispatchEvent(new Event('x', { bubbles: true }));
        b.dispatchEvent(new Event('x', { bubbles: true }));
        b.addEventListener('y', e => { log.push('y1'); e.stopImmediatePropagation(); });
        b.addEventListener('y', () => log.push('y2'));
        b.dispatchEvent(new Event('y'));
        b.addEventListener('p', e => e.preventDefault(), { passive: true });
        const notCanceled = b.dispatchEvent(new Event('p', { cancelable: true }));
        const ac = new AbortController();
        b.addEventListener('s', () => log.push('s'), { signal: ac.signal });
        ac.abort();
        b.dispatchEvent(new Event('s'));
        log.join(' ') + ' ' + notCanceled
        "#,
    );
    assert_eq!(r, "b1 b2 b3 b2 b3 y1 true");
}

#[test]
fn event_handler_keeps_its_first_registration_position() {
    let mut page = page_with("<!doctype html><button id=b onclick=\"log.push('attr')\"></button>");
    let r = eval(
        &mut page,
        r#"
        var log = [];
        const b = document.getElementById('b');
        b.addEventListener('click', () => log.push('listener'));
        b.onclick = () => log.push('prop');
        b.click();
        b.setAttribute('onclick', "log.push('attr2'); return false");
        const e = new MouseEvent('click', { cancelable: true });
        b.dispatchEvent(e);
        log.join(' ') + ' ' + e.defaultPrevented
        "#,
    );
    assert_eq!(r, "prop listener attr2 listener true");
}

/// HTML "getting the current value of the event handler": a body that does not compile makes the
/// value null but keeps the handler's place among the listeners (setting it again does not add a
/// second listener), and the compiled function is named after the handler.
#[test]
fn an_invalid_event_handler_keeps_its_place_and_handlers_are_named() {
    let mut page = page_with("<!doctype html><body>");
    let r = eval(
        &mut page,
        r#"
        var log = [];
        window.onerror = () => { log.push('error'); return true; };
        const d = document.createElement('div');
        d.addEventListener('click', () => log.push('one'));
        d.setAttribute('onclick', '}');
        d.addEventListener('click', () => log.push('three'));
        log.push(String(d.onclick));
        d.onclick = () => log.push('two');
        d.dispatchEvent(new Event('click'));
        d.setAttribute('onclick', 'log.push("two again")');
        d.dispatchEvent(new Event('click'));
        const e = document.createElement('span');
        e.setAttribute('onclick', 'foo');
        document.body.setAttribute('onerror', 'bar');
        log.join(' ') + '|' + e.onclick + '|' + window.onerror
        "#,
    );
    assert_eq!(
        r,
        "error null one two three one two again three\
         |function onclick(event) {\nfoo\n}\
         |function onerror(event, source, lineno, colno, error) {\nbar\n}"
    );
}

/// HTML: an element's event handler content attribute is null while scripting is disabled for its
/// document (no browsing context: it stays uncompiled and never runs), and a `body` of a document
/// that is not active has no Window-reflecting handlers (getting one is null, setting it does
/// nothing).
#[test]
fn event_handlers_of_documents_without_a_browsing_context() {
    let mut page = page_with("<!doctype html><body>");
    let r = eval(
        &mut page,
        r#"
        var ran = [];
        const doc = new DOMParser().parseFromString('<div onclick="ran.push(1)"></div>', 'text/html');
        const div = doc.body.firstChild;
        const a = String(div.onclick);
        div.dispatchEvent(new Event('click'));
        div.onclick = () => ran.push(2);
        div.dispatchEvent(new Event('click'));
        const w = () => 'window';
        window.onfocus = w;
        const b = doc.createElement('body');
        const c = String(b.onfocus);
        b.onfocus = () => 'element';
        b.setAttribute('onblur', 'x');
        [a, ran.join(), c, String(b.onfocus), String(b.onblur), window.onfocus === w].join(' ')
        "#,
    );
    assert_eq!(r, "null 2 null null null true");
}

/// HTML `reportError(e)` reports for this's relevant global object: another window's method called
/// on a frame's window fires `error` at the frame's window only.
#[test]
fn report_error_reports_to_the_window_it_is_called_on() {
    let mut page = page_with("<!doctype html><body>");
    let r = eval(
        &mut page,
        r#"
        var log = [];
        addEventListener('error', () => log.push('top'));
        const f = document.createElement('iframe');
        document.body.append(f);
        const e = new TypeError('x');
        f.contentWindow.addEventListener('error', ev => log.push('frame ' + (ev.error === e)));
        window.reportError.call(f.contentWindow, e);
        log.join()
        "#,
    );
    assert_eq!(r, "frame true");
}

/// HTML "getting the current value of the event handler": an element's handler is compiled with
/// its element and document in scope, a Window's (a `body`'s forwarded attribute) with the window
/// alone.
/// "Prepare the script element" 16: a script the parser inserts into an element a script moved to another
/// document keeps this parser's document as its parser document, so it never runs (nor fires load/error).
#[test]
fn a_parser_inserted_script_moved_to_another_document_before_its_end_tag_does_not_run() {
    let mut page = page_with(
        r#"<!doctype html><iframe id=f></iframe>
        <div id=d>
          <script>
            var log = [], div = d;
            f.contentWindow.log = log;
            f.contentDocument.body.appendChild(div);
          </script>
          <script onload="log.push('load')" onerror="log.push('error')">log.push('ran');</script>
          <script src="data:text/javascript,log.push('ran-src')" onload="log.push('load')"></script>
        </div>
        <script>log.push(div.ownerDocument === f.contentDocument ? 'moved' : 'here');</script>"#,
    );
    assert_eq!(eval(&mut page, "log.join()"), "moved");
}

/// "Fetch an inline module script graph" marks the script ready in a queued task: an inline module script a
/// script inserts runs (or reports its parse error) after a task, never within a microtask of its insertion.
#[test]
fn inserted_inline_module_scripts_run_and_report_errors_in_a_later_task() {
    let mut page = page_with(
        r#"<!doctype html><script>
          var log = [];
          window.onerror = () => log.push('error');
          const a = document.createElement('script');
          a.type = 'module';
          a.textContent = "log.push('ran')";
          document.head.append(a);
          const b = document.createElement('script');
          b.type = 'module';
          b.textContent = '+++++';
          document.head.append(b);
          log.push('inserted');
          queueMicrotask(() => log.push('microtask'));
        </script>"#,
    );
    assert_eq!(
        eval(&mut page, "log.join()"),
        "inserted,microtask,ran,error"
    );
}

#[test]
fn a_window_event_handler_has_no_element_scope() {
    let mut page = page_with("<!doctype html><body><div id=d onclick='log.push(typeof domain)'>");
    let r = eval(
        &mut page,
        r#"
        var log = [];
        document.body.setAttribute('onresize', 'log.push(typeof domain, typeof bodyOwn)');
        document.body.bodyOwn = 1;
        d.click();
        window.dispatchEvent(new Event('resize'));
        log.join()
        "#,
    );
    assert_eq!(r, "string,undefined,undefined");
}

/// HTML §8.1.8.2: Document's own handlers are no element's content attributes, and Window's only
/// `body`'s and `frameset`'s; `onwebkitanimationend` and the like handle the legacy camel-cased
/// event types.
#[test]
fn document_and_window_handlers_are_not_every_elements_content_attributes() {
    let mut page = page_with("<!doctype html><body>");
    let r = eval(
        &mut page,
        r#"
        var ran = [];
        const d = document.createElement('div');
        for (const t of ['readystatechange', 'visibilitychange', 'hashchange', 'click']) {
            d.setAttribute('on' + t, `ran.push('${t}')`);
            d.dispatchEvent(new Event(t));
        }
        // (the prefixed handlers handle the camel-cased types)
        d.setAttribute('onwebkitanimationend', "ran.push('attr')");
        d.dispatchEvent(new Event('webkitanimationend'));
        d.dispatchEvent(new Event('webkitAnimationEnd'));
        d.onwebkittransitionend = () => ran.push('idl');
        d.dispatchEvent(new Event('webkitTransitionEnd'));
        ran.join() + ' ' + String(d.onwebkitanimationend).split('(')[0]
        "#,
    );
    assert_eq!(r, "click,attr,idl function onwebkitanimationend");
}

/// Web IDL §3.7.3: an interface prototype object with `[Unscopable]` members (its mixins' included)
/// has `@@unscopables`, an object with a null prototype naming them, read-only and not enumerable;
/// an event handler's scope (`with` over its element, form owner and document) skips them.
#[test]
fn unscopable_members_are_left_out_of_with_scopes() {
    let mut page = page_with(
        "<!doctype html><form id=f><button id=b onclick='window.got = testVariable'></button></form>",
    );
    let r = eval(
        &mut page,
        r#"
        const u = Element.prototype[Symbol.unscopables];
        const d = Object.getOwnPropertyDescriptor(Element.prototype, Symbol.unscopables);
        const shape = [Object.getPrototypeOf(u) === null, Object.keys(u).sort().join(),
            d.writable, d.enumerable, d.configurable,
            Object.keys(Document.prototype[Symbol.unscopables]).sort().join(),
            Object.hasOwn(HTMLElement.prototype, Symbol.unscopables)].join(' ');
        window.testVariable = 'window';
        for (const o of [document, b, f]) {
            o[Symbol.unscopables].testVariable = true;
            o.testVariable = 'shadowed';
        }
        b.click();
        shape + ' ' + window.got
        "#,
    );
    assert_eq!(
        r,
        "true after,append,before,prepend,remove,replaceChildren,replaceWith,slot \
         false false true append,fullscreen,prepend,replaceChildren false window"
    );
}

#[test]
fn composed_path_and_retargeting_across_a_shadow_root() {
    let mut page = page_with("<!doctype html><div id=host></div>");
    let r = eval(
        &mut page,
        r#"
        const host = document.getElementById('host');
        const root = host.attachShadow({ mode: 'closed' });
        root.innerHTML = '<p><i id=t></i></p>';
        const t = root.getElementById('t');
        const seen = [];
        document.body.addEventListener('c', e => seen.push('body:' + e.target.id + ':' + e.composedPath().length));
        t.addEventListener('c', e => seen.push('t:' + e.target.id + ':' + e.composedPath().length));
        t.dispatchEvent(new Event('c', { bubbles: true, composed: true }));
        t.dispatchEvent(new Event('c', { bubbles: true, composed: false }));
        seen.join(' ')
        "#,
    );
    // Inside: i, p, shadow root, host, body, html, document, window; outside, the closed tree is hidden.
    assert_eq!(r, "t:t:8 body:host:5 t:t:3");
}

/// DOM §2.9 across nested shadow trees and a slot, with deep trees on each side: each listener
/// sees the target retargeted against its own tree (a slotted node's tree is an ancestor of the
/// slot's, so the slot's tree sees it as is), and the path goes through the slot.
#[test]
fn event_paths_through_nested_shadow_trees_and_slots() {
    let mut page = page_with(
        "<!doctype html><div id=d1><div id=d2><div id=d3><div id=outer></div></div></div></div>",
    );
    let r = eval(
        &mut page,
        r#"
        const outer = document.getElementById('outer');
        const r1 = outer.attachShadow({ mode: 'open' });
        r1.innerHTML = '<section id=s1><div id=inner><b id=light><u id=deep></u></b></div></section>';
        const inner = r1.getElementById('inner');
        const r2 = inner.attachShadow({ mode: 'open' });
        r2.innerHTML = '<p id=p1><span id=p2><slot id=slot></slot></span><i id=own><em id=owndeep></em></i></p>';
        const id = n => n === window ? 'window' : n === document ? 'document' : (n.id || n.nodeName);
        const seen = [];
        for (const n of [r2.getElementById('p2'), r2, inner, r1.getElementById('s1'), r1, outer,
                         document.getElementById('d1'), document])
            n.addEventListener('x', e => seen.push(id(n) + '>' + id(e.target) + '@' + e.eventPhase));
        const fire = (t, composed) => {
            seen.length = 0;
            let path;
            t.addEventListener('x', e => { path = e.composedPath().map(id).join(','); }, { once: true });
            t.dispatchEvent(new Event('x', { bubbles: true, composed }));
            return path + ' | ' + seen.join(' ');
        };
        [fire(r2.getElementById('owndeep'), true), fire(r1.getElementById('deep'), true),
         fire(r2.getElementById('owndeep'), false)].join('\n')
        "#,
    );
    assert_eq!(
        r,
        "owndeep,own,p1,#document-fragment,inner,s1,#document-fragment,outer,d3,d2,d1,BODY,HTML,document,window \
         | #document-fragment>owndeep@3 inner>inner@2 s1>inner@3 #document-fragment>inner@3 \
         outer>outer@2 d1>outer@3 document>outer@3\n\
         deep,light,slot,p2,p1,#document-fragment,inner,s1,#document-fragment,outer,d3,d2,d1,BODY,HTML,document,window \
         | p2>deep@3 #document-fragment>deep@3 inner>deep@3 s1>deep@3 #document-fragment>deep@3 \
         outer>outer@2 d1>outer@3 document>outer@3\n\
         owndeep,own,p1,#document-fragment | #document-fragment>owndeep@3"
    );
}

#[test]
fn microtasks_run_before_the_next_task_and_timers_by_due_time() {
    let mut page = page_with(
        r#"<!doctype html><script>
        var log = [];
        setTimeout(() => { log.push('t10'); }, 10);
        setTimeout(() => {
            log.push('t0a');
            Promise.resolve().then(() => log.push('t0a-micro'));
        }, 0);
        setTimeout(() => log.push('t0b'), 0);
        Promise.resolve().then(() => log.push('micro'));
        queueMicrotask(() => log.push('qm'));
        requestAnimationFrame(t => log.push('raf:' + (typeof t)));
        const id = setTimeout(() => log.push('cleared'), 0);
        clearTimeout(id);
        log.push('sync');
        </script>"#,
    );
    let r = eval(&mut page, "log.filter(x => !x.startsWith('raf')).join(' ')");
    assert_eq!(r, "sync micro qm t0a t0a-micro t0b t10");
    assert_eq!(
        eval(&mut page, "log.filter(x => x.startsWith('raf')).join()"),
        "raf:number"
    );
}

#[test]
fn nested_timers_and_intervals() {
    let mut page = page_with(
        r#"<!doctype html><script>
        var log = [], n = 0;
        const iv = setInterval(() => { log.push('i' + (++n)); if (n === 3) clearInterval(iv); }, 1);
        setTimeout(() => setTimeout(() => log.push('nested'), 0), 30);
        </script>"#,
    );
    assert_eq!(eval(&mut page, "log.join(' ')"), "i1 i2 i3 nested");
}

#[test]
fn mutation_observers_are_notified_at_the_microtask_checkpoint() {
    let mut page = page_with("<!doctype html><div id=d></div>");
    let r = eval(
        &mut page,
        r#"
        var log = [];
        const d = document.getElementById('d');
        new MutationObserver(rs => log.push('mo:' + rs.length)).observe(d, { childList: true, attributes: true });
        d.appendChild(document.createElement('b'));
        d.setAttribute('x', '1');
        Promise.resolve().then(() => log.push('then'));
        log.push('sync');
        'ok'
        "#,
    );
    assert_eq!(r, "ok");
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "log.join(' ')"), "sync mo:2 then");
}

#[test]
fn document_write_inserts_at_the_parser_position_reentrantly() {
    let mut page = page_with(
        r#"<!doctype html><body><script>
        var log = [];
        document.write('<script>log.push("inner1"); document.write("<b id=w>w</b>"); log.push("inner2");<\/script>');
        log.push('after:' + !!document.getElementById('w'));
        </script><i id=tail></i></body>"#,
    );
    assert_eq!(eval(&mut page, "log.join(' ')"), "inner1 inner2 after:true");
    let order = eval(
        &mut page,
        "Array.from(document.body.children, e => e.localName + (e.id ? '#' + e.id : '')).join(' ')",
    );
    assert_eq!(order, "script script b#w i#tail");
}

#[test]
fn document_write_after_load_replaces_the_document() {
    let mut page = page_with("<!doctype html><p id=old></p>");
    eval(
        &mut page,
        "document.write('<p id=new>n</p>'); document.close(); 'done'",
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(
            &mut page,
            "!!document.getElementById('new') + ' ' + !!document.getElementById('old')"
        ),
        "true false"
    );
}

/// A tiny HTTP/1.1 server on a thread: answers each request for `/<name>` from `files`, closing the
/// connection after each response.
fn serve(files: Vec<(&'static str, &'static str, &'static str)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            let mut length = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let path = first.split(' ').nth(1).unwrap_or("/");
            let path = path.split('?').next().unwrap_or("/");
            let response = match files.iter().find(|(p, _, _)| *p == path) {
                Some((_, mime, body)) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

#[test]
fn module_graphs_load_from_the_network_in_order() {
    let base = serve(vec![
        (
            "/index.html",
            "text/html",
            r#"<!doctype html><script>var log = [];</script>
            <script type=module src=/a.js></script>
            <script defer src=/defer.js></script>
            <script type=module>log.push('inline-module');</script>
            <script type=text/plain>log.push('never');</script>
            <script>log.push('classic');</script>"#,
        ),
        (
            "/a.js",
            "text/javascript",
            "import { b } from './lib/b.js'; log.push('a:' + b + ':' + new URL(import.meta.url).pathname); import('./c.js').then(m => log.push('dynamic:' + m.default));",
        ),
        (
            "/lib/b.js",
            "text/javascript",
            "import { d } from '/d.js'; export const b = 'b' + d;",
        ),
        ("/d.js", "text/javascript", "export const d = 'd';"),
        ("/c.js", "text/javascript", "export default 'c';"),
        ("/defer.js", "text/javascript", "log.push('defer');"),
    ]);
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/index.html")).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.eval(
            "String(document.readyState === 'complete' && log.length >= 5)",
            "t",
        )
        .is_ok_and(|v| v == "true")
    });
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "classic a:bd:/a.js defer inline-module dynamic:c"
    );
}

/// HTML §8.4.3 `document.write` step 6: the written characters are parsed only up to the insertion
/// point, so a script written by a script runs inside the write, but the document's next script
/// (after the insertion point) waits for the writing script to end.
#[test]
fn document_write_parses_only_up_to_the_insertion_point() {
    let mut page = page_with(
        r#"<!doctype html><body><script>
        var log = [];
        document.write('<script>log.push("written")<\/script><i id=w></i>');
        log.push('end1:' + !!document.getElementById('w') + !!document.getElementById('tail'));
        </script><script>log.push('second');</script><b id=tail></b></body>"#,
    );
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "written end1:truefalse second"
    );
}

/// HTML §13.2.6.4.8 and §8.4.3: external scripts written by one script run in the order written,
/// each one's own writes go in at its position, and the input after them waits; the insertion
/// point stays after earlier written text the parser has not reached while it waits.
#[test]
fn document_write_of_external_scripts_keeps_their_order() {
    let base = serve(vec![
        (
            "/index.html",
            "text/html",
            r#"<!doctype html><body><script>var log = [];
            log.push('inline1');
            document.write('<script src="/one.js"><\/script>');
            document.write('<script src="/three.js"><\/script>');
            document.write('<script>log.push("inline-written")<\/script>');
            document.write('<script src="/two.js"><\/script>');
            log.push('end1');
            </script><script>log.push('inline2');</script></body>"#,
        ),
        ("/one.js", "text/javascript", "log.push('one');"),
        ("/two.js", "text/javascript", "log.push('two');"),
        (
            "/three.js",
            "text/javascript",
            "log.push('three-before'); document.write('<script>log.push(\"three-written\")<\\/script>'); log.push('three-after');",
        ),
    ]);
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/index.html")).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.eval(
            "String(typeof log === 'object' && document.readyState === 'complete')",
            "t",
        )
        .is_ok_and(|v| v == "true")
    });
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "inline1 end1 one three-before three-written three-after inline-written two inline2"
    );
}

/// HTML §7.4.2.3 "navigate" step 13: a `javascript:` URL's script runs in a task queued when the
/// navigation begins, from `location` (before the load event, queued later) and from a link (not
/// inside `click()`).
#[test]
fn javascript_url_navigations_run_in_a_task_of_their_own() {
    let mut page = page_with(
        r#"<!doctype html><body><a id=a href="javascript:log.push('link')"></a><script>
        var log = [];
        onload = () => log.push('load');
        location.replace("javascript:log.push('location')");
        log.push('end1');
        document.getElementById('a').click();
        log.push('end2');
        </script></body>"#,
    );
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "end1 end2 location link load"
    );
}

/// DOM §4.2.3 "insert": a script's children changed steps (step 9) run before the post-connection
/// steps of the nodes inserted into it (step 11), so a script element given a fragment that holds
/// text and another script runs its own text first.
#[test]
fn a_script_runs_before_a_script_inserted_into_it() {
    let mut page = page_with(
        r#"<!doctype html><body><script id=outer></script><script>
        var log = [];
        const frag = document.createDocumentFragment();
        const inner = document.createElement('script');
        inner.textContent = "log.push('inner')";
        frag.append("log.push('outer');", inner);
        document.getElementById('outer').appendChild(frag);
        log.push('end');
        </script></body>"#,
    );
    assert_eq!(eval(&mut page, "log.join(' ')"), "outer inner end");
}

/// HTML §8.4.1 "document open steps" steps 12-13: an iframe's `about:blank` document opened by its
/// parent takes the parent's URL (its fragment dropped), so relative URLs written into it resolve
/// against the parent's; and it is no longer the initial `about:blank`.
#[test]
fn document_open_gives_the_document_the_entry_documents_url() {
    let mut page = page_with("<!doctype html><iframe></iframe>");
    eval(
        &mut page,
        "history.replaceState(null, '', '/dir/page.html#frag'); 1",
    );
    let r = eval(
        &mut page,
        r#"
        const doc = frames[0].document;
        const before = doc.URL;
        doc.open();
        doc.write('<a href="x.html"></a>');
        doc.close();
        [before, doc.URL, doc.querySelector('a').href].join(' ')
        "#,
    );
    assert_eq!(
        r,
        "about:blank http://example.test/dir/page.html http://example.test/dir/x.html"
    );
}

/// HTML "document open steps" steps 6-7 and the document write steps 8-9: a document being
/// unloaded (its unload counter raised) or whose parser was aborted (`window.stop()`) is neither
/// opened nor written to; an iframe's initial `about:blank` is in quirks mode until opened.
#[test]
fn document_open_bails_out_while_unloading_and_after_an_aborted_parser() {
    let mut page = page_with(
        "<!doctype html><iframe></iframe><iframe></iframe><p>Text</p>\
         <script>window.stop(); setTimeout(() => { document.open(); window.done = 1; }, 0);</script>",
    );
    let r = eval(
        &mut page,
        r#"
        window.log = [];
        const [a, b] = document.querySelectorAll('iframe');
        const d = a.contentDocument;
        log.push(d.compatMode, b.contentDocument.compatMode);
        b.contentDocument.open();
        log.push(b.contentDocument.compatMode);
        b.contentDocument.close();
        const n = d.childNodes.length;
        a.contentWindow.addEventListener('unload', () => {
          log.push(d.open() === d, d.childNodes.length === n);
          d.write('<p>written</p>');
          log.push(d.childNodes.length === n, !d.querySelector('p'));
        });
        a.src = 'about:blank?next';
        log.join(' ')
        "#,
    );
    assert_eq!(r, "BackCompat BackCompat CSS1Compat");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    page.run_until(deadline, |p| {
        p.eval("String(window.done === 1 && log.length >= 7)", "t")
            .is_ok_and(|v| v == "true")
    });
    assert_eq!(
        eval(&mut page, "log.slice(3).join(' ')"),
        "true true true true"
    );
    assert_eq!(
        eval(&mut page, "document.querySelector('p')?.textContent"),
        "Text"
    );
}

/// HTML "prepare the script element": "fetch an inline module script graph" marks the script ready in a
/// queued task, so one inserted by script runs after the inserting script and its microtasks, and before the
/// parser's own inline module script, whose task is queued later.
#[test]
fn an_inserted_inline_module_script_runs_after_the_inserting_script() {
    let mut page = page_with(
        r#"<!doctype html><body><script>
        var log = [];
        const s = document.createElement('script');
        s.type = 'module';
        s.textContent = "log.push('module')";
        document.body.append(s);
        log.push('classic');
        Promise.resolve().then(() => log.push('then'));
        onload = () => log.push('load');
        </script><script type=module>log.push('parser-module');</script>
        <script>log.push('classic2');</script></body>"#,
    );
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "classic then classic2 module parser-module load"
    );
}

#[test]
fn errors_in_scripts_reach_window_onerror() {
    let mut page = page_with(
        r#"<!doctype html><script>
        var log = [];
        window.onerror = (msg, src, line, col, err) => log.push('onerror:' + (err instanceof TypeError));
        addEventListener('unhandledrejection', e => log.push('rejection:' + e.reason));
        </script><script>null.x</script><script>Promise.reject('r')</script>"#,
    );
    assert_eq!(eval(&mut page, "log.join(' ')"), "onerror:true rejection:r");
}

#[test]
fn html_documents_are_html_document_objects() {
    // `HTMLDocument` (`legacy.idl`): an empty interface between HTML documents and `Document`.
    let mut page = page_with("<!doctype html><iframe></iframe>");
    let p = |e: &str| format!("Object.getPrototypeOf({e}) === ");
    let checks = [
        "typeof HTMLDocument === 'function'".to_string(),
        "!Object.getOwnPropertyDescriptor(window, 'HTMLDocument').enumerable".into(),
        format!("{}Document.prototype", p("HTMLDocument.prototype")),
        format!("{}HTMLDocument.prototype", p("document")),
        "document instanceof HTMLDocument && document.constructor === HTMLDocument".into(),
        "String(document) === '[object HTMLDocument]'".into(),
        format!("{}Document.prototype", p("new Document()")),
        format!(
            "{}HTMLDocument.prototype",
            p("document.implementation.createHTMLDocument('t')")
        ),
        format!(
            "{}XMLDocument.prototype",
            p("document.implementation.createDocument('http://www.w3.org/1999/xhtml', 'html')")
        ),
        format!(
            "{}HTMLDocument.prototype",
            p("new DOMParser().parseFromString('<p>', 'text/html')")
        ),
        // (DOMParser makes "a new Document" for the XML types, not an XMLDocument)
        format!(
            "{}Document.prototype",
            p("new DOMParser().parseFromString('<a/>', 'application/xml')")
        ),
        format!("{}HTMLDocument.prototype", p("document.cloneNode()")),
        "(() => { const w = frames[0]; return w.document instanceof w.HTMLDocument             && !(w.document instanceof HTMLDocument) })()"
            .into(),
        "(() => { try { new HTMLDocument(); return false } catch (e) { return e instanceof TypeError } })()"
            .into(),
    ];
    for c in checks {
        assert_eq!(eval(&mut page, &c), "true", "{c}");
    }
}

#[test]
fn iframes_get_a_window_a_realm_and_a_document() {
    let mut page = page_with(
        r#"<!doctype html><script>var log = [];</script>
        <iframe id=a srcdoc="<p id=s>x</p><script>parent.log.push('inner:' + (parent !== window) + ':' + document.getElementById('s').textContent)</script>"
            onload="log.push('a load')"></iframe>
        <script>
        const b = document.createElement('iframe');
        b.onload = () => log.push('b load');
        document.body.appendChild(b);
        log.push('b sync:' + b.contentDocument.URL + ':' + (b.contentWindow.parent === window));
        onload = () => log.push('window load');
        </script>"#,
    );
    assert_eq!(
        eval(&mut page, "log.join(' | ')"),
        "b load | b sync:about:blank:true | inner:true:x | a load | window load"
    );
    let r = eval(
        &mut page,
        r#"
        const f = document.getElementById('a');
        const w = f.contentWindow, d = f.contentDocument;
        const r = [d.body instanceof w.HTMLBodyElement, d.body instanceof HTMLBodyElement,
                   w.Node !== Node, w.frameElement === f, window.length, w.top === window];
        try { d.querySelector('[') } catch (e) { r.push(e instanceof w.DOMException) }
        f.remove();
        r.push(f.contentWindow, w.parent, w.closed);
        r.join()
        "#,
    );
    assert_eq!(r, "true,false,true,true,2,true,true,,,true");
}

#[test]
fn fetch_and_xhr_reach_the_network() {
    let base = serve(vec![
        (
            "/index.html",
            "text/html",
            "<!doctype html><title>x</title>",
        ),
        ("/data.json", "application/json", r#"{"a": [1, 2]}"#),
        ("/text.txt", "text/plain", "hello"),
        ("/doc.xml", "application/xml", "<r><c/></r>"),
    ]);
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/index.html")).unwrap());
    page.run_until_settled(Duration::from_secs(10));
    eval(
        &mut page,
        r#"
        var log = [];
        fetch('/data.json').then(r => { log.push('status:' + r.status + ':' + r.headers.get('content-type')); return r.json(); })
            .then(j => log.push('json:' + j.a.join('+')));
        fetch('/missing').then(r => log.push('missing:' + r.ok));
        const x = new XMLHttpRequest();
        x.onload = () => log.push('xhr:' + x.status + ':' + x.responseText);
        x.open('GET', '/text.txt');
        x.send();
        const s = new XMLHttpRequest();
        s.open('GET', '/doc.xml', false);
        s.send();
        log.push('sync:' + s.responseXML.documentElement.firstChild.localName);
        const ac = new AbortController();
        fetch('/text.txt', { signal: ac.signal }).catch(e => log.push('aborted:' + e.name));
        ac.abort();
        new Response('body').text().then(t => log.push('response:' + t));
        new Blob(['ab', 'c']).text().then(t => log.push('blob:' + t));
        'started'
        "#,
    );
    page.run_until_settled(Duration::from_secs(10));
    let mut got: Vec<String> = eval(&mut page, "log.join('|')")
        .split('|')
        .map(str::to_owned)
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            "aborted:AbortError",
            "blob:abc",
            "json:1+2",
            "missing:false",
            "response:body",
            "status:200:application/json",
            "sync:c",
            "xhr:200:hello"
        ]
    );
}

#[test]
fn dedicated_workers_exchange_messages() {
    let base = serve(vec![
        ("/index.html", "text/html", "<!doctype html>"),
        (
            "/w.js",
            "text/javascript",
            "importScripts('/lib.js'); onmessage = e => postMessage({echo: e.data, lib: libValue, isWorker: typeof document === 'undefined' && self instanceof DedicatedWorkerGlobalScope}); setTimeout(() => { throw new Error('boom') }, 0);",
        ),
        ("/lib.js", "text/javascript", "var libValue = 7;"),
    ]);
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/index.html")).unwrap());
    page.run_until_settled(Duration::from_secs(10));
    eval(
        &mut page,
        r#"
        var log = [];
        const w = new Worker('/w.js');
        w.onmessage = e => { log.push(JSON.stringify(e.data)); };
        w.onerror = e => { log.push('error:' + e.message); e.preventDefault(); };
        w.postMessage([1, 'two']);
        'ok'
        "#,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.eval("String(log.length >= 2)", "t")
            .is_ok_and(|v| v == "true")
    });
    let mut got: Vec<String> = eval(&mut page, "w.terminate(); log.join('|')")
        .split('|')
        .map(str::to_owned)
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            r#"error:Uncaught Error: boom"#,
            r#"{"echo":[1,"two"],"lib":7,"isWorker":true}"#,
        ]
    );
}

#[test]
fn workers_have_their_own_performance_timeline() {
    let base = serve(vec![
        ("/index.html", "text/html", "<!doctype html>"),
        (
            "/w.js",
            "text/javascript",
            r#"
            const err = f => { try { f(); return 'none'; } catch (e) { return e.name; } };
            new PerformanceObserver(list => postMessage('po:' + list.getEntries().map(e => e.name)))
              .observe({ entryTypes: ['mark', 'measure'] });
            // The PerformanceTiming names are not reserved in a worker, but measure() cannot convert
            // them there.
            const m = performance.mark('navigationStart', { detail: [1] });
            performance.measure('w', { start: 0 });
            postMessage([m instanceof PerformanceMark, m.detail[0], performance.getEntries().length,
                         err(() => performance.measure('x', 'navigationStart'))].join());
            "#,
        ),
    ]);
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/index.html")).unwrap());
    page.run_until_settled(Duration::from_secs(10));
    eval(
        &mut page,
        r#"
        var log = [];
        const w = new Worker('/w.js');
        w.onmessage = e => log.push(e.data);
        performance.getEntries().length
        "#,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.eval("String(log.length >= 2)", "t")
            .is_ok_and(|v| v == "true")
    });
    assert_eq!(
        eval(
            &mut page,
            "w.terminate(); log.join('|') + '|' + performance.getEntries().length"
        ),
        // (the window's entries: its visibility state, its navigation and the worker script's fetch)
        "true,1,2,TypeError|po:w,navigationStart|3"
    );
}

#[test]
fn json_and_text_module_scripts() {
    let base = serve(vec![
        (
            "/index.html",
            "text/html",
            r#"<!doctype html><script>var log = [];</script>
            <script type=module>
            import data from '/d.json' with { type: 'json' };
            log.push('static:' + data.k);
            const t = await import('/t.txt', { with: { type: 'text' } });
            log.push('text:' + t.default);
            await import('/d.json').catch(e => log.push('js-mime:' + e.name));
            await import('/t.txt', { with: { type: 'json' } }).catch(e => log.push('json-mime:' + e.name));
            </script>"#,
        ),
        ("/d.json", "application/json", r#"{"k": "v"}"#),
        ("/t.txt", "text/plain", "plain"),
    ]);
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/index.html")).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.eval("String(typeof log !== 'undefined' && log.length >= 4)", "t")
            .is_ok_and(|v| v == "true")
    });
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "static:v text:plain js-mime:TypeError json-mime:TypeError"
    );
}

/// HTML "fetch a single module script": a failed fetch leaves no module map entry, so a later
/// import (of it, or of a module that imports it) fetches it again; one that succeeded is reused.
#[test]
fn failed_module_fetches_are_not_kept() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new([
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ]);
    let counts = hits.clone();
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
            let path = first.split(' ').nth(1).unwrap_or("/").to_owned();
            // (`/a.js`, `/b.js` and `/c.js` fail with a 404 the first time they are asked for)
            let found = |i: usize| counts[i].fetch_add(1, Ordering::SeqCst) > 0;
            let (ok, body) = match path.as_str() {
                "/index.html" => (true, "<!doctype html><script>var log = [];</script>"),
                "/a.js" => (found(0), "export default 'a';"),
                "/b.js" => (found(1), "export default 'b';"),
                "/c.js" => (found(2), "log.push('c ran');"),
                "/throw.js" => (true, "throw {thrown: true};"),
                "/parent.js" => (true, "import b from '/b.js'; export default 'p' + b;"),
                _ => (false, ""),
            };
            let mime = if path.ends_with(".js") {
                "text/javascript"
            } else {
                "text/html"
            };
            let response = if ok {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nCache-Control: no-store\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("http://{addr}/index.html")).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.eval("String(typeof log !== 'undefined')", "t")
            .is_ok_and(|v| v == "true")
    });
    page.eval(
        "(async () => {
           const name = e => e.name;
           log.push(await import('/a.js').then(m => m.default, name));
           const a = await import('/a.js');
           log.push(a.default, (await import('/a.js')) === a);
           log.push(await import('/parent.js').then(m => m.default, name));
           log.push(await import('/parent.js').then(m => m.default, name));
           // (a script element's `error` handler inserting the same module: the failed entry is
           // gone before the handler runs, so it fetches again)
           await new Promise(done => {
             const add = (onerror, onload) => {
               const s = document.createElement('script');
               s.type = 'module';
               s.src = '/c.js';
               s.onerror = onerror;
               s.onload = onload;
               document.body.append(s);
             };
             add(() => { log.push('c error'); add(() => { log.push('c error'); done(); }, done); });
           });
           // (an errored module run again reports its error and leaves no unhandled rejection)
           addEventListener('unhandledrejection', () => log.push('unhandled'));
           addEventListener('error', e => log.push('error:' + e.error.thrown));
           for (const i of [1, 2]) {
             await new Promise(done => {
               const s = document.createElement('script');
               s.type = 'module';
               s.src = '/throw.js';
               s.onload = done;
               document.body.append(s);
             });
           }
           await new Promise(r => setTimeout(r, 50));
           log.push('end');
         })()",
        "t",
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.eval("String(log.includes('end'))", "t")
            .is_ok_and(|v| v == "true")
    });
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "TypeError a true TypeError pb c error c ran error:true error:true end"
    );
    assert_eq!(hits[0].load(Ordering::SeqCst), 2);
    assert_eq!(hits[1].load(Ordering::SeqCst), 2);
    assert_eq!(hits[2].load(Ordering::SeqCst), 2);
}

/// HTML "create a CSS module script": the default export is a constructed `CSSStyleSheet` whose
/// relative URLs resolve against the module's URL; the response must be `text/css`; one sheet per
/// module map entry.
#[test]
fn css_module_scripts() {
    let base = serve(vec![
        (
            "/index.html",
            "text/html",
            r#"<!doctype html><script>var log = [];</script><p id=p>x</p>
            <script type=module>
            import sheet from '/css/s.css' with { type: 'css' };
            log.push(sheet instanceof CSSStyleSheet, sheet.cssRules.length, sheet.cssRules[0].selectorText);
            document.adoptedStyleSheets = [sheet];
            log.push(getComputedStyle(p).color, getComputedStyle(p).backgroundImage.includes('/css/i.png'));
            const again = await import('/css/s.css', { with: { type: 'css' } });
            log.push(again.default === sheet);
            sheet.replaceSync('p { color: rgb(1, 2, 3) }');
            log.push(getComputedStyle(p).color);
            await import('/t.txt', { with: { type: 'css' } }).catch(e => log.push('css-mime:' + e.name));
            </script>"#,
        ),
        (
            "/css/s.css",
            "text/css",
            "@import 'x.css'; p { color: rgb(0, 128, 0); background-image: url(i.png) }",
        ),
        ("/t.txt", "text/plain", "p {}"),
    ]);
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(snowlark_page::RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/index.html")).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.eval("String(typeof log !== 'undefined' && log.length >= 8)", "t")
            .is_ok_and(|v| v == "true")
    });
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "true 1 p rgb(0, 128, 0) true true rgb(1, 2, 3) css-mime:TypeError"
    );
}

#[test]
fn named_access_and_element_internals() {
    let mut page = page_with(
        "<!doctype html><div id=box></div><form name=f></form><iframe name=fr></iframe><img name=a><img name=a>",
    );
    let r = eval(
        &mut page,
        r#"
        class C extends HTMLElement {
            static formAssociated = true;
            constructor() { super(); this.i = this.attachInternals(); }
        }
        customElements.define('x-c', C);
        const c = new C();
        document.body.append(c);
        c.i.states.add('on');
        c.i.ariaLabel = 'L';
        c.i.setValidity({ customError: true }, 'bad');
        [box.id, document.f.localName, fr === frames[0], window.a.length,
         c.matches(':state(on)'), [...c.i.states].join(), c.i.ariaLabel, c.getAttribute('aria-label'),
         c.i.validity.valid, c.i.validationMessage].join()
        "#,
    );
    assert_eq!(r, "box,form,true,2,true,on,L,,false,bad");
}

#[test]
fn the_heap_limit_is_reported_not_fatal() {
    let mut page = Page::new(PageOptions {
        heap_limit: Some(32 << 20),
        ..Default::default()
    });
    assert!(!page.heap_limit_reached());
    let r = eval(
        &mut page,
        "var keep = []; for (let i = 0; i < 1e6; i++) keep.push({ i, s: 'x' + i }); keep.length",
    );
    assert_eq!(r, "1000000");
    assert!(page.heap_limit_reached());
}

#[test]
fn labels_send_one_click_to_their_control() {
    let mut page = page_with(
        "<!doctype html><label id=l><input type=checkbox id=c><button id=b>b</button> text</label>",
    );
    let r = eval(
        &mut page,
        "let n = 0; const c = document.getElementById('c');
         c.addEventListener('click', () => n++);
         c.click();
         const a = [n, c.checked];
         document.getElementById('l').click();
         a.push(n, c.checked);
         document.getElementById('b').click();
         a.push(n, c.checked);
         a.join()",
    );
    // The control's own click is not sent back; a click on other interactive content is not proxied.
    assert_eq!(r, "1,true,2,false,2,false");
}

#[test]
fn failed_top_level_navigations_show_error_pages() {
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    // Connection refused: a port nothing listens on.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let refused = url::Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
    page.navigate(refused.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| p.load_complete());
    let doc = page.engine().document().unwrap();
    assert_eq!(page.engine().error_page_url(doc), Some(refused.clone()));
    assert_eq!(eval(&mut page, "document.URL"), "about:error");
    assert_eq!(eval(&mut page, "document.title"), "127.0.0.1");
    let text = eval(&mut page, "document.body.textContent");
    assert!(text.contains("refused to connect"), "{text}");
    assert!(text.contains("ERR_CONNECTION_REFUSED"), "{text}");
    assert_eq!(
        eval(&mut page, "document.getElementById('reload').href"),
        refused.as_str()
    );
    // An HTTP error without a body.
    let base = serve(vec![("/ok", "text/html", "<title>fine</title>")]);
    let missing = url::Url::parse(&format!("{base}/missing")).unwrap();
    page.navigate(missing.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.engine()
            .document()
            .and_then(|d| p.engine().error_page_url(d))
            .is_some_and(|u| u == missing)
            && p.load_complete()
    });
    let text = eval(&mut page, "document.body.textContent");
    assert!(text.contains("HTTP ERROR 404"), "{text}");
    // A working page shows itself.
    page.navigate(url::Url::parse(&format!("{base}/ok")).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.engine()
            .document()
            .is_some_and(|d| p.engine().error_page_url(d).is_none())
            && p.load_complete()
    });
    assert_eq!(eval(&mut page, "document.title"), "fine");
}

#[test]
fn assigning_to_a_named_window_property_shadows_it() {
    // Web IDL §3.7.4.1: the named properties object reports its properties writable, so a strict
    // assignment on the window creates an own property instead of throwing.
    let mut page =
        page_with("<!doctype html><script id=__NEXT_DATA__ type=x></script><div id=foo></div>");
    let r = eval(
        &mut page,
        r#"
        (() => {
          'use strict';
          const before = window.__NEXT_DATA__ instanceof HTMLScriptElement;
          window.__NEXT_DATA__ = { a: 1 };
          window.foo = 5;
          const d = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(Window.prototype), 'foo');
          return [before, window.__NEXT_DATA__.a, window.foo, Object.hasOwn(window, 'foo'), d && d.writable].join();
        })()
        "#,
    );
    assert_eq!(r, "true,1,5,true,true");
}

#[test]
fn a_frames_callbacks_report_exceptions_to_the_frames_window() {
    // Web IDL "invoke a callback function" runs the callback, and reports what it throws, in the
    // callback's realm.
    let mut page = page_with(
        r#"<!doctype html><script>
        var log = [];
        addEventListener('error', e => { log.push('top:' + e.message); e.preventDefault(); });
        </script><iframe srcdoc="<script>
        addEventListener('error', e => { parent.log.push('frame:' + e.message); e.preventDefault(); });
        setTimeout(() => { missingA(); }, 0);
        requestAnimationFrame(() => missingB());
        </script>"></iframe>"#,
    );
    page.run_until_settled(Duration::from_secs(5));
    let mut got: Vec<String> = eval(&mut page, "log.join('|')")
        .split('|')
        .map(str::to_owned)
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            "frame:Uncaught ReferenceError: missingA is not defined",
            "frame:Uncaught ReferenceError: missingB is not defined"
        ]
    );
}

#[test]
fn each_document_has_its_own_module_map() {
    // HTML §8.1.5.5: a module another document already evaluated is fetched, linked and evaluated
    // again in a new document (Speedometer loads the same module app into a fresh iframe each time).
    let base = serve(vec![
        (
            "/index.html",
            "text/html",
            "<!doctype html><script>var log = [];</script><script type=module src=/m.js></script>",
        ),
        (
            "/f.html",
            "text/html",
            "<!doctype html><script type=module src=/m.js></script>",
        ),
        (
            "/m.js",
            "text/javascript",
            "import { tag } from '/dep.js'; (window.parent.log || log).push(tag + ':' + (window === window.parent ? 'top' : 'frame') + ':' + (typeof window.seen)); window.seen = 1;",
        ),
        ("/dep.js", "text/javascript", "export const tag = 'm';"),
    ]);
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/index.html")).unwrap());
    page.run_until_settled(Duration::from_secs(10));
    eval(
        &mut page,
        "var f = document.createElement('iframe'); f.src = '/f.html'; document.body.append(f); 'ok'",
    );
    page.run_until_settled(Duration::from_secs(10));
    eval(
        &mut page,
        "f.remove(); f = document.createElement('iframe'); f.src = '/f.html'; document.body.append(f); 'ok'",
    );
    page.run_until_settled(Duration::from_secs(10));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "m:top:undefined m:frame:undefined m:frame:undefined"
    );
}

#[test]
fn document_all_is_an_undetectable_callable_collection() {
    // HTML §2.7.2.1 HTMLAllCollection: [[IsHTMLDDA]] (ECMA-262 Annex B.3.6) and [[Call]] as item().
    let mut page =
        page_with("<!doctype html><p id=y>a</p><img name=x><form name=x></form><div name=x></div>");
    let r = eval(
        &mut page,
        r#"
        const a = document.all;
        [
          typeof a, a === undefined, a == undefined, a == null, !!a,
          a === document.all, a.length === document.getElementsByTagName('*').length,
          a(0).localName, a('y').id, a.x.length, a.item('x') instanceof HTMLCollection,
          a.namedItem('y').localName, a.item(), a('nope'), a.div === undefined,
          Object.prototype.toString.call(a),
        ].join()
        "#,
    );
    assert_eq!(
        r,
        "undefined,false,true,true,false,true,true,html,y,2,true,p,,,true,[object HTMLAllCollection]"
    );
}

#[test]
fn idle_callbacks_run_in_idle_periods_between_frames_and_timers() {
    // requestIdleCallback §5: an idle period starts whenever the event loop has no task and no
    // rendering opportunity due, also on a page that animates every frame and runs timers.
    let mut page = page_with("<!doctype html><body>x");
    eval(
        &mut page,
        r#"
        window.log = [];
        const end = performance.now() + 400;
        (function frame() { if (performance.now() < end) requestAnimationFrame(frame); })();
        const busy = setInterval(() => { if (performance.now() > end) clearInterval(busy); }, 16);
        requestIdleCallback(d => log.push('idle ' + d.didTimeout + ' ' + (d.timeRemaining() > 0)
            + ' ' + (d.timeRemaining() <= 50)));
        const cancelled = requestIdleCallback(() => log.push('cancelled ran'));
        cancelIdleCallback(cancelled);
        // a callback requested during an idle period waits for the next one
        requestIdleCallback(() => { const t = requestIdleCallback(() => log.push('next')); });
        'ok'
        "#,
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "log.join()"), "idle false true true,next");
}

#[test]
fn an_idle_callback_with_a_timeout_runs_when_the_loop_stays_busy() {
    let mut page = page_with("<!doctype html><body>x");
    eval(
        &mut page,
        r#"
        window.log = [];
        const start = performance.now();
        let n = 0;
        (function busy() {
            const t = performance.now();
            while (performance.now() - t < 30) {}
            if (++n < 15) setTimeout(busy, 0);
        })();
        requestIdleCallback(d => log.push(d.didTimeout + ' ' + d.timeRemaining() + ' '
            + (performance.now() - start >= 100) + ' ' + (n < 15)), { timeout: 100 });
        'ok'
        "#,
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "log.join()"), "true 0 true true");
}

#[test]
fn navigator_user_activation_follows_activation_triggering_input() {
    // HTML §6.4.4: hasBeenActive is sticky activation, isActive transient activation; a keydown is
    // an activation-triggering input event, and "activation notification" reaches the same-origin
    // descendant navigables too (§6.4.3).
    let mut page = page_with("<!doctype html><body>x<iframe srcdoc='y'></iframe>");
    let state = r#"
        [navigator.userActivation === navigator.userActivation,
         navigator.userActivation instanceof UserActivation,
         navigator.userActivation.hasBeenActive, navigator.userActivation.isActive,
         frames[0].navigator.userActivation.hasBeenActive,
         frames[0].navigator.userActivation.isActive].join()
    "#;
    assert_eq!(eval(&mut page, state), "true,true,false,false,false,false");
    page.key_down(&snowlark_page::keyboard::KeyInput {
        vk: 0x41,
        scan: 0x1E,
        text: Some('a'),
        mods: snowlark_page::input::Modifiers::default(),
        repeat: false,
        paste: None,
        paste_html: None,
    });
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, state), "true,true,true,true,true,true");
}

#[test]
fn media_devices_enumerate_no_devices() {
    // Media Capture and Streams §9.2: the engine has no capture device and selects no audio output,
    // so the list of devices available to the user agent is empty. ([SecureContext])
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><body>x",
        &url::Url::parse("https://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    let r = eval(
        &mut page,
        r#"
        const md = navigator.mediaDevices;
        window.out = [md === navigator.mediaDevices, md instanceof MediaDevices,
          md instanceof EventTarget, typeof MediaDeviceInfo, typeof InputDeviceInfo,
          typeof md.getUserMedia, typeof md.selectAudioOutput,
          md.getSupportedConstraints().deviceId].join();
        md.enumerateDevices().then(list => {
            window.devices = Array.isArray(list) + ' ' + list.length;
        });
        out
        "#,
    );
    assert_eq!(
        r,
        "true,true,true,function,function,undefined,undefined,true"
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "devices"), "true 0");
}

#[test]
fn the_default_locale_is_the_browser_language() {
    // Chrome sets ICU's default locale from its UI language, which navigator.language reports: the
    // default locale of Intl, and of toLocaleString, is the browser's, not the system's.
    let mut page = page_with("<!doctype html><body>x");
    let r = eval(
        &mut page,
        r#"
        [navigator.language, new Intl.DateTimeFormat().resolvedOptions().locale,
         new Intl.NumberFormat().resolvedOptions().locale, (1234.5).toLocaleString(),
         new Date(Date.UTC(2020, 0, 15, 12)).toLocaleDateString(undefined, { timeZone: 'UTC' })]
         .join('|')
        "#,
    );
    assert_eq!(r, "en-US|en-US|en-US|1,234.5|1/15/2020");
}

#[test]
fn idle_callbacks_run_between_the_timers_of_a_light_loop() {
    // requestIdleCallback §5.3: the gaps of a loop of short timers 4 ms apart are idle periods
    // (Blink's minimum idle period is 1 ms): callbacks run while the loop goes on, as in Chrome.
    let mut page = page_with("<!doctype html><body>x");
    eval(
        &mut page,
        r#"
        window.during = 0;
        const end = performance.now() + 300;
        const loop = setInterval(() => { if (performance.now() > end) clearInterval(loop); }, 4);
        (function ask() {
            requestIdleCallback(d => { if (performance.now() < end) { during++; ask(); } });
        })();
        'ok'
        "#,
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    let during: u32 = eval(&mut page, "String(during)").parse().unwrap();
    assert!(during >= 5, "{during} idle callbacks ran during the loop");
}

#[test]
fn an_idle_callback_waits_for_a_loop_of_long_tasks() {
    // A loop of tasks longer than a frame, with short gaps between them, leaves no idle period
    // (WPT requestidlecallback/callback-timeout-when-busy): the callback runs once it ends.
    let mut page = page_with("<!doctype html><body>x");
    eval(
        &mut page,
        r#"
        window.log = [];
        let n = 0;
        (function busy() {
            const t = performance.now();
            while (performance.now() - t < 30) {}
            if (++n < 10) setTimeout(busy, 0);
        })();
        requestIdleCallback(d => log.push(n + ' ' + d.didTimeout));
        'ok'
        "#,
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "log.join()"), "10 false");
}

#[test]
fn navigator_plugins_device_memory_battery_and_do_not_track() {
    // (a secure context: deviceMemory and getBattery() are [SecureContext])
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><body>x",
        &url::Url::parse("https://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    let r = eval(
        &mut page,
        r#"
        window.out = [
          navigator.plugins.length, navigator.mimeTypes.length, navigator.pdfViewerEnabled,
          navigator.plugins === navigator.plugins, navigator.deviceMemory, navigator.doNotTrack,
        ].join();
        navigator.getBattery().then(b => {
          window.battery = [b instanceof BatteryManager, b.charging, b.chargingTime, b.dischargingTime,
            b.level, navigator.getBattery() === navigator.getBattery()].join();
        });
        out
        "#,
    );
    assert_eq!(r, "5,2,true,true,8,");
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "battery"), "true,true,0,Infinity,1,true");
    // HTML §8.9.1.6 with a PDF viewer (the engine's, on by default): the five plugins and two MIME
    // types.
    let r = eval(
        &mut page,
        r#"
        const p = navigator.plugins, m = navigator.mimeTypes;
        [p.length, p[0].name, p['WebKit built-in PDF'].filename, p[1].length, p[1][0].type,
         m.length, m['text/pdf'].suffixes, m[0].enabledPlugin === p[0], m[0].description,
         navigator.pdfViewerEnabled, Object.keys(p).join('/')].join('|')
        "#,
    );
    assert_eq!(
        r,
        "5|PDF Viewer|internal-pdf-viewer|2|application/pdf|2|pdf|true|Portable Document Format|true|0/1/2/3/4"
    );
    // Without one (an embedder that turns it off): empty lists.
    page.engine().set_pdf_viewer_supported(false);
    assert_eq!(
        eval(
            &mut page,
            "[navigator.plugins.length, navigator.mimeTypes.length, navigator.pdfViewerEnabled].join()"
        ),
        "0,0,false"
    );
}

#[test]
fn notifications_are_denied_and_fire_error() {
    // The Notifications API without a notification platform: permission "denied".
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><body>x",
        &url::Url::parse("https://example.test/dir/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    let r = eval(
        &mut page,
        r#"
        window.log = [];
        const data = { a: [1] };
        const n = new Notification('Hi', { body: 'b', tag: 't', icon: 'i.png', data, timestamp: 5 });
        n.onerror = () => log.push('error');
        n.onshow = () => log.push('show');
        Notification.requestPermission(p => log.push('callback ' + p)).then(p => log.push('promise ' + p));
        let renotify = '';
        try { new Notification('x', { renotify: true }); } catch (e) { renotify = e.name; }
        let actions = '';
        try { new Notification('x', { actions: [{ action: 'a', title: 'A' }] }); } catch (e) { actions = e.name; }
        [Notification.permission, Notification.maxActions, n.title, n.body, n.tag, n.icon, n.dir,
         n.timestamp, n.data !== data && n.data.a[0] === 1 && n.data === n.data, n.silent,
         renotify, actions].join()
        "#,
    );
    assert_eq!(
        r,
        "denied,2,Hi,b,t,https://example.test/dir/i.png,auto,5,true,,TypeError,TypeError"
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut page, "log.join()"),
        "error,callback denied,promise denied"
    );
}

#[test]
fn credentials_container_without_a_store_or_an_authenticator() {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><form id=f><input name=u autocomplete=username value=ann>\
         <input name=p autocomplete=current-password value=pw></form>",
        &url::Url::parse("https://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    eval(
        &mut page,
        r#"
        window.log = [];
        const note = name => v => log.push(name + ':' + (v && v.name || String(v)));
        const c = navigator.credentials;
        c.get({}).then(note('get{}'), note('get{}'));
        c.get({ password: true }).then(note('get-password'), note('get-password'));
        c.get({ password: true, publicKey: { challenge: new Uint8Array(1) } }).then(note('mixed'), note('mixed'));
        c.get({ publicKey: { challenge: new Uint8Array(1) } }).then(note('publicKey'), note('publicKey'));
        const ac = new AbortController();
        c.get({ signal: ac.signal }).then(note('aborted'), r => log.push('aborted:' + r));
        ac.abort('why');
        c.create({ password: document.getElementById('f') }).then(p => log.push('form:' + p.id + '/' + p.password + '/' + p.type));
        c.create({ federated: { id: 'i', provider: 'https://idp.test/', origin: 'x' } }).then(f => log.push('fed:' + f.provider + '/' + f.type));
        c.store(new PasswordCredential({ id: 'a', password: 'b', origin: location.origin })).then(note('store'));
        PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable().then(note('uvpa'));
        c === navigator.credentials
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "uvpa:false get{}:NotSupportedError get-password:null mixed:NotSupportedError \
         publicKey:NotAllowedError aborted:why form:ann/pw/password \
         fed:https://idp.test//federated store:undefined"
    );
}

#[test]
fn scheduler_post_task_runs_by_priority_and_yield_inherits_it() {
    let mut page = page_with("<!doctype html><body>x");
    eval(
        &mut page,
        r#"
        window.log = [];
        scheduler.postTask(() => log.push('bg'), { priority: 'background' });
        scheduler.postTask(() => log.push('uv1'));
        setTimeout(() => log.push('timeout'), 0);
        scheduler.postTask(() => log.push('uv2'));
        scheduler.postTask(() => log.push('ub'), { priority: 'user-blocking' });
        scheduler.postTask(() => 42).then(v => log.push('result ' + v));
        scheduler.postTask(() => { throw new Error('x'); }).catch(e => log.push('rejected ' + e.message));
        const tc = new TaskController({ priority: 'background' });
        tc.signal.onprioritychange = e => log.push('change ' + e.previousPriority + '>' + tc.signal.priority);
        scheduler.postTask(() => log.push('dyn'), { signal: tc.signal });
        tc.setPriority('user-blocking');
        const ac = new AbortController();
        scheduler.postTask(() => log.push('never'), { signal: ac.signal }).catch(r => log.push('abort ' + r));
        ac.abort('stop');
        scheduler.postTask(async () => {
          log.push('a1');
          await scheduler.yield();
          log.push('a2');
        }, { priority: 'user-blocking' });
        [tc instanceof AbortController, tc.signal instanceof TaskSignal, tc.signal.priority,
         navigator.scheduling.isInputPending(),
         TaskSignal.any([], { priority: tc.signal }).priority].join()
        "#,
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(
        eval(&mut page, "log.join()"),
        "change background>user-blocking,abort stop,ub,dyn,a1,a2,uv1,uv2,result 42,rejected x,timeout,bg"
    );
}

#[test]
fn a_hidden_page_has_no_rendering_opportunities_and_its_documents_turn_hidden() {
    let run = |page: &mut Page, ms: u64| {
        page.run_until(
            std::time::Instant::now() + Duration::from_millis(ms),
            |_| false,
        );
    };
    let mut page = page_with(
        r#"<!doctype html><body><script>
        var log = [];
        var frames = 0;
        document.addEventListener('visibilitychange',
            () => log.push(document.visibilityState + ':' + document.hidden));
        // (the event bubbles from the document to the window)
        window.addEventListener('visibilitychange', () => log.push('window'));
        </script>"#,
    );
    assert_eq!(
        eval(
            &mut page,
            "document.visibilityState + ':' + document.hidden"
        ),
        "visible:false"
    );
    // A document of no browsing context is hidden (HTML: its visibility state's initial value).
    assert_eq!(
        eval(
            &mut page,
            "document.implementation.createHTMLDocument('').visibilityState"
        ),
        "hidden"
    );
    eval(
        &mut page,
        "(function frame() { frames++; requestAnimationFrame(frame); })()",
    );
    run(&mut page, 150);
    assert!(
        eval(&mut page, "frames").parse::<u32>().unwrap() > 1,
        "frames run while shown"
    );

    // Hidden: `visibilitychange`, no more animation frames, no frame to wake for; timers still run.
    page.engine().set_shown(false);
    run(&mut page, 100);
    assert_eq!(eval(&mut page, "log.join()"), "hidden:true,window");
    assert_eq!(
        eval(
            &mut page,
            "document.visibilityState + ':' + document.hidden"
        ),
        "hidden:true"
    );
    let frames = eval(&mut page, "frames");
    eval(
        &mut page,
        "var timer = false; setTimeout(() => timer = true, 10)",
    );
    run(&mut page, 200);
    assert_eq!(
        eval(&mut page, "frames"),
        frames,
        "no animation frames while hidden"
    );
    assert_eq!(eval(&mut page, "timer"), "true", "timers run while hidden");
    assert!(
        page.engine().next_wakeup().is_none(),
        "a hidden page with only animation frame callbacks pending never wakes"
    );

    // Shown again: the callbacks that waited run, and the state is back.
    page.engine().set_shown(true);
    run(&mut page, 150);
    assert_eq!(
        eval(&mut page, "log.join()"),
        "hidden:true,window,visible:false,window"
    );
    assert!(
        eval(&mut page, "frames").parse::<u32>().unwrap() > frames.parse::<u32>().unwrap(),
        "frames run again"
    );
}

#[test]
fn bar_props_are_six_objects_visible_while_the_context_lives() {
    // HTML §7.2.6: each bar is its own BarProp, the same object every time; `visible` is the
    // negation of the top-level context's "is popup" (no popups here), false once the window's
    // browsing context is gone (WPT html/browsers/the-window-object/BarProp.window.js).
    let mut page = page_with("<!doctype html><body><iframe src='about:blank'></iframe>");
    let r = eval(
        &mut page,
        r#"
        const names = ['locationbar', 'menubar', 'personalbar', 'scrollbars', 'statusbar', 'toolbar'];
        const w = document.querySelector('iframe').contentWindow;
        window.bars = names.map(n => w[n]);
        [names.map(n => window[n].visible).join(),
         new Set(names.map(n => window[n])).size,
         names.every(n => window[n] === window[n]),
         bars.map(b => b.visible).join()].join('|')
        "#,
    );
    assert_eq!(
        r,
        "true,true,true,true,true,true|6|true|true,true,true,true,true,true"
    );
    let r = eval(
        &mut page,
        "document.querySelector('iframe').remove(); bars.map(b => b.visible).join()",
    );
    assert_eq!(r, "false,false,false,false,false,false");
}

#[test]
fn location_has_its_own_value_of_and_to_primitive_and_window_has_external() {
    // HTML §7.10.5: a Location object's own valueOf (%Object.prototype.valueOf%) and @@toPrimitive
    // (undefined), neither writable, enumerable nor configurable, first among its own properties.
    // HTML §16.3: `window.external`, whose methods do nothing.
    let mut page = page_with("<!doctype html><body>x");
    let r = eval(
        &mut page,
        r#"
        const d = Object.getOwnPropertyDescriptor(location, 'valueOf');
        const t = Object.getOwnPropertyDescriptor(location, Symbol.toPrimitive);
        [d.value === Object.prototype.valueOf, d.writable, d.enumerable, d.configurable,
         'value' in t, t.value, t.writable, t.configurable, Object.getOwnPropertyNames(location)[0],
         String(location) === location.href, typeof external, external === window.external,
         Object.prototype.toString.call(external), external.AddSearchProvider(),
         external.IsSearchProviderInstalled()].join()
        "#,
    );
    assert_eq!(
        r,
        "true,false,false,false,true,,false,false,valueOf,true,object,true,[object External],,"
    );
}

/// An inline script's line numbers count from the source line its text starts on (the line its
/// start tag ends on), for classic and module scripts; lines of `document.write` text are not the
/// document's, and a CR LF pair is one line break.
#[test]
fn inline_script_errors_report_their_source_lines() {
    let html = [
        "<!doctype html>",
        "<script>",
        "var log = [];",
        "addEventListener('error', e => log.push(e.lineno));",
        r"document.write('<p>\n\n\n</p>');",
        "</script>",
        "<script",
        "  data-x=1>",
        "x1;",
        "</script>",
        "<script type=module>",
        "",
        "x2;",
        "</script>",
        "<script>x3;</script>",
        "<!-- a -->\r",
        "<script>\r",
        "x4;</script>",
    ]
    .join("\n");
    let mut page = page_with(&html);
    // (the module runs after parsing)
    assert_eq!(eval(&mut page, "log.join()"), "9,15,18,13");
}

/// The document's referrer policy (its policy container's) is its response's `Referrer-Policy`
/// header's, then a `<meta name=referrer>`'s (a legacy keyword stands for its policy; changing the
/// element's content sets it again), and the document's script and module fetches use it; an
/// iframe's `referrerpolicy` is its navigation's own.
#[test]
fn document_referrer_policy_comes_from_the_header_and_meta() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            let mut referer = String::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
                if let Some(v) = h.strip_prefix("Referer:").or(h.strip_prefix("referer:")) {
                    referer = v.trim().to_owned();
                }
            }
            let path = first.split(' ').nth(1).unwrap_or("/").to_owned();
            let (mime, body) = match path.split_once('?') {
                Some(("/ref.js", tag)) => (
                    "text/javascript",
                    format!("log.push('{tag}=' + {referer:?});"),
                ),
                Some(("/ref.html", tag)) => (
                    "text/html",
                    format!("<script>parent.log.push('{tag}=' + {referer:?});</script>"),
                ),
                _ => (
                    "text/html",
                    "<!doctype html><script>var log = [];</script>
                     <script src=/ref.js?a></script>
                     <meta name=referrer content=never>
                     <script src=/ref.js?b></script>
                     <script type=module>import '/ref.js?c';</script>
                     <iframe referrerpolicy=unsafe-url src=/ref.html?e></iframe>
                     <script>
                       document.querySelector('meta').content = 'always';
                       const s = document.createElement('script');
                       s.src = '/ref.js?d';
                       document.head.append(s);
                     </script>"
                        .to_owned(),
                ),
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nReferrer-Policy: origin\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("http://{addr}/index.html")).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |p| {
        p.eval("String(typeof log !== 'undefined' && log.length >= 5)", "t")
            .is_ok_and(|v| v == "true")
    });
    // (an iframe's `referrerpolicy` is its navigation's policy)
    assert_eq!(
        page.eval("log.sort().join()", "t").unwrap(),
        format!("a=http://{addr}/,b=,c=,d=http://{addr}/index.html,e=http://{addr}/index.html")
    );
}

/// HTML "create a link request" and "fetch a modulepreload module script graph": a `link`'s fetch
/// uses its `referrerpolicy` attribute (a `Link:` header's, its `referrerpolicy` parameter), for
/// `modulepreload`, `preload` and `stylesheet`, and so does the speculative parser's early fetch of
/// a `link` it meets while a script blocks the parser.
#[test]
fn link_fetches_use_their_referrerpolicy() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let seen = seen2.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    return;
                }
                let mut referer = String::new();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if let Some(v) = h.strip_prefix("Referer:").or(h.strip_prefix("referer:")) {
                        referer = v.trim().to_owned();
                    }
                }
                let path = first.split(' ').nth(1).unwrap_or("/").to_owned();
                let (mime, link, body) = match path.split_once('?') {
                    Some(("/r", tag)) => {
                        seen.lock().unwrap().push(format!("{tag}={referer}"));
                        let mime = if tag.starts_with('s') {
                            "text/css"
                        } else {
                            "text/javascript"
                        };
                        (mime, "", String::new())
                    }
                    _ if path == "/slow.js" => {
                        // (holds the parser while the speculative parser looks ahead)
                        std::thread::sleep(Duration::from_millis(300));
                        ("text/javascript", "", String::new())
                    }
                    _ => (
                        "text/html",
                        "Link: </r?m4>; rel=modulepreload; referrerpolicy=no-referrer\r\n",
                        "<!doctype html>
                         <link rel=modulepreload href=/r?m1>
                         <link rel=modulepreload href=/r?m2 referrerpolicy=no-referrer>
                         <link rel=modulepreload href=/r?m3 referrerpolicy=origin crossorigin>
                         <link rel=preload as=script href=/r?p1 referrerpolicy=no-referrer>
                         <link rel=stylesheet href=/r?s1 referrerpolicy=origin>
                         <script src=/slow.js></script>
                         <link rel=stylesheet href=/r?s2 referrerpolicy=no-referrer>"
                            .to_owned(),
                    ),
                };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\n{link}Cache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            });
        }
    });
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("http://{addr}/index.html")).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    page.run_until(deadline, |_| seen.lock().unwrap().len() >= 7);
    let mut got = seen.lock().unwrap().clone();
    got.sort();
    let doc = format!("http://{addr}/index.html");
    let origin = format!("http://{addr}/");
    assert_eq!(
        got.join(","),
        format!("m1={doc},m2=,m3={origin},m4=,p1=,s1={origin},s2=")
    );
}

/// An error in code without a script name (`eval`, `Function`) is reported at the document's URL,
/// at its line in that code.
#[test]
fn unnamed_script_errors_report_the_document_url() {
    let mut page = page_with(
        "<!doctype html><script>var log = [];
         addEventListener('error', e => log.push(e.filename + '|' + e.lineno));</script>
         <script>eval('x1')</script><script>new Function('x2')()</script>",
    );
    assert_eq!(
        eval(&mut page, "log.join()"),
        "http://example.test/|1,http://example.test/|3"
    );
}

/// HTML "muted errors": a classic script whose response is CORS-cross-origin (a no-cors fetch
/// from another origin, or one redirected through another origin) reports its errors, and those
/// of code it evaluates, as "Script error." with no URL, position or value; a same-origin script
/// and a cross-origin one fetched with CORS report theirs in full, and so does a module script,
/// whose errors are never muted (a JSON module's parse error comes from no script that ran).
#[test]
fn cross_origin_script_errors_are_muted() {
    use std::net::SocketAddr;
    fn serve(listener: TcpListener) {
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
                let path = first.split(' ').nth(1).unwrap_or("/").to_owned();
                let response = if let Some(to) = path.strip_prefix("/redirect?") {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    let body = match path.as_str() {
                        "/throw.js" => "throw new Error('fox');",
                        "/eval.js" => "eval(\"throw new Error('in eval')\");",
                        "/cors.js" => "throw new Error('cors');",
                        _ => "",
                    };
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
    }
    let other = TcpListener::bind("127.0.0.1:0").unwrap();
    let there: SocketAddr = other.local_addr().unwrap();
    serve(other);
    let index = TcpListener::bind("127.0.0.1:0").unwrap();
    let page_addr = index.local_addr().unwrap();
    let html = format!(
        "<!doctype html><script>var log = [];
         addEventListener('error', e => log.push(e.error instanceof SyntaxError
           ? 'SyntaxError|' + e.filename.split('/').pop()
           : [e.message, e.filename.split('/').pop(), e.lineno, e.colno, e.error === null].join('|')));
         </script>
         <script type=module>import v from '/bad.json' with {{ type: 'json' }};</script>
         <script src='http://{page_addr}/throw.js'></script>
         <script src='http://{there}/throw.js'></script>
         <script src='http://{page_addr}/redirect?http://{there}/redirect?http://{page_addr}/throw.js'></script>
         <script src='http://{there}/eval.js'></script>
         <script crossorigin src='http://{there}/cors.js'></script>
         <script>var done = true;</script>"
    );
    // (the page and its same-origin scripts come from one server, the others from another port)
    std::thread::spawn(move || {
        for stream in index.incoming() {
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
            let path = first.split(' ').nth(1).unwrap_or("/").to_owned();
            let (mime, body) = match path.as_str() {
                "/index.html" => ("text/html", html.clone()),
                "/throw.js" => ("text/javascript", "throw new Error('fox');".to_owned()),
                "/bad.json" => ("application/json", "{".to_owned()),
                p => match p.strip_prefix("/redirect?") {
                    Some(to) => {
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        );
                        continue;
                    }
                    None => ("text/plain", String::new()),
                },
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("http://{page_addr}/index.html")).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    // (the module runs after parsing, last)
    page.run_until(deadline, |p| {
        p.eval("String(typeof log !== 'undefined' && log.length >= 6)", "t")
            .is_ok_and(|v| v == "true")
    });
    let r = page.eval("log.join('\\n')", "t").unwrap();
    assert_eq!(
        r,
        "Uncaught Error: fox|throw.js|1|1|false\n\
         Script error.||0|0|true\n\
         Script error.||0|0|true\n\
         Script error.||0|0|true\n\
         Uncaught Error: cors|cors.js|1|1|false\n\
         SyntaxError|index.html"
    );
}
