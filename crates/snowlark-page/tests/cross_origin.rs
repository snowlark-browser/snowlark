// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cross-origin `Window` and `Location` objects (HTML §7.2.1.3, §7.2.3, §7.2.4), `postMessage` between
//! origins (§9.3) and `document.domain` (§7.5.2), with documents of several origins from in-process
//! servers. Most of what is asserted here is a path by which an object of one realm could reach script
//! of another origin: each must end in a `SecurityError` or in an object of the caller's own realm.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use snowlark_net::config::HostRule;
use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions};

/// A tiny HTTP/1.1 server answering `/<name>` from `files` (path, body).
fn serve(files: Vec<(&'static str, String)>) -> u16 {
    serve_on(0, files)
}

/// [`serve`] on a given port (0: any).
fn serve_on(port: u16, files: Vec<(&'static str, String)>) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
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
            let response = match files.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .into(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

fn wait(page: &mut Page, js: &str, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        page.run_for(Duration::from_millis(20));
        if page.eval(js, "wait.js").is_ok_and(|v| v == want) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(eval(page, js), want, "{js}");
}

/// The script every page below shares: `attempt(f)` is f's value, or the name of what it threw.
const HELPERS: &str = "<script>
    function attempt(f) {
      try { return String(f()); } catch (e) { return e.name; }
    }
    function isSecurityError(f) {
      try { f(); return 'no error'; } catch (e) {
        return e instanceof DOMException && e.name == 'SecurityError' ? 'ok' : 'wrong: ' + e;
      }
    }
    </script>";

/// What every top-level page below does first.
const TOP_SCRIPT: &str = "<script>
    window.got = []; window.messages = []; window.log = [];
    window.record = s => log.push(s);
    addEventListener('message', e => { got.push(e.data); messages.push(e); });
    addEventListener('load', () => window.allLoaded = true);
    </script>";

fn wait_loaded(page: &mut Page) {
    wait(page, "String(window.allLoaded)", "true");
}

struct Sites {
    /// (kept for as long as the page uses it)
    _net: NetThread,
    page: Page,
    /// The top-level page's origin.
    a: String,
    /// The other origin.
    b: String,
}

/// A page of origin `a` with `top_body`, the files `a_files` on `a` and `b_files` on `b`.
fn sites(
    top_body: &str,
    a_files: Vec<(&'static str, String)>,
    b_files: Vec<(&'static str, String)>,
) -> Sites {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let b = format!("http://127.0.0.1:{}", serve(b_files));
    let mut files = a_files;
    files.push((
        "/top.html",
        format!(
            "<!doctype html>{HELPERS}{TOP_SCRIPT}<body>{}",
            top_body.replace("{b}", &b)
        ),
    ));
    let a = format!("http://127.0.0.1:{}", serve(files));
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{a}/top.html")).unwrap());
    Sites {
        _net: net,
        page,
        a,
        b,
    }
}

fn page_with_one_frame() -> Sites {
    let mut s = sites(
        "<iframe id=x src='{b}/x.html' name=xname></iframe>",
        vec![("/plain.html", "<!doctype html><title>plain</title>".into())],
        vec![(
            "/x.html",
            format!(
                "<!doctype html>{HELPERS}<body><iframe name=inner></iframe><iframe></iframe>\
                 <script>
                 window.secret = 'secret'; window.frames = 'overridden';
                 window.seen = []; addEventListener('message', e => {{
                   seen.push(e.data);
                   window.lastPort = e.ports[0];
                   window.lastEvent = e;
                 }});
                 parent.postMessage('ready', '*');
                 </script>"
            ),
        )],
    );
    wait_loaded(&mut s.page);
    wait(&mut s.page, "got.join()", "ready");
    eval(
        &mut s.page,
        "window.w = document.getElementById('x').contentWindow; 'ok'",
    );
    s
}

#[test]
fn only_the_cross_origin_properties_are_visible() {
    let mut s = page_with_one_frame();
    let page = &mut s.page;
    // What may be read.
    assert_eq!(eval(page, "w.window === w && w.self === w"), "true");
    assert_eq!(
        eval(page, "w.frames === w"),
        "true",
        "an override is not seen"
    );
    assert_eq!(
        eval(page, "w.parent === window && w.top === window"),
        "true"
    );
    assert_eq!(eval(page, "w.length"), "2");
    assert_eq!(eval(page, "w.closed"), "false");
    assert_eq!(eval(page, "String(w.opener)"), "null");
    assert_eq!(eval(page, "typeof w[0] + typeof w.inner"), "objectobject");
    assert_eq!(eval(page, "w.then === undefined"), "true");
    assert_eq!(eval(page, "w[Symbol.toStringTag] === undefined"), "true");
    // What may not, by every internal method.
    for js in [
        "w.document",
        "w.secret",
        "w.name",
        "w.nothing",
        "w[2]",
        "w[Symbol.iterator]",
        "w.__proto__",
        "w.constructor",
        "'secret' in w",
        "Object.getOwnPropertyDescriptor(w, 'secret')",
        "Object.prototype.hasOwnProperty.call(w, 'secret')",
        "w.secret = 1",
        "w.postMessage = 1",
        "w[0] = 1",
        "delete w.secret",
        "delete w.top",
        "Object.defineProperty(w, 'x', { value: 1 })",
        "w.location.href",
        "w.location.hash",
        "w.location.origin",
        "w.location.reload()",
        "String(w)",
        "w.toString()",
        "w.location.toString()",
        "JSON.stringify(w)",
        "Object.setPrototypeOf(w, {})",
    ] {
        assert_eq!(
            eval(page, &format!("isSecurityError(() => {js})")),
            "ok",
            "{js}"
        );
    }
    assert_eq!(eval(page, "String(Object.getPrototypeOf(w))"), "null");
    assert_eq!(
        eval(page, "String(Object.getPrototypeOf(w.location))"),
        "null"
    );
    assert_eq!(eval(page, "Object.isExtensible(w)"), "true");
    assert_eq!(
        eval(page, "Object.prototype.toString.call(w)"),
        "[object Object]"
    );
    assert_eq!(
        eval(
            page,
            "Object.getOwnPropertyNames(w).filter(n => !/^\\d/.test(n)).sort().join()"
        ),
        "blur,close,closed,focus,frames,length,location,opener,parent,postMessage,self,then,top,window"
    );
    assert_eq!(eval(page, "Object.keys(w).join()"), "0,1");
    assert_eq!(
        eval(page, "Object.getOwnPropertyNames(w.location).sort().join()"),
        "href,replace,then"
    );
    // Descriptors.
    assert_eq!(
        eval(
            page,
            "const d = Object.getOwnPropertyDescriptor(w, 'location');
             [typeof d.get, typeof d.set, d.enumerable, d.configurable, d.get.name, d.set.name].join()"
        ),
        "function,function,false,true,get location,set location"
    );
    assert_eq!(
        eval(
            page,
            "const p = Object.getOwnPropertyDescriptor(w, 'postMessage');
             [typeof p.value, p.writable, p.enumerable, p.configurable, p.value.name, p.value.length].join()"
        ),
        "function,false,false,true,postMessage,1"
    );
    assert_eq!(
        eval(
            page,
            "const h = Object.getOwnPropertyDescriptor(w.location, 'href');
             [h.get, typeof h.set, h.set.length].join()"
        ),
        ",function,1"
    );
    // A frame whose document is of another origin than its parent's is named there only by its
    // container's `name` attribute (the engines' clause kept from the earlier text).
    assert_eq!(
        eval(
            page,
            "window.xname === document.getElementById('x').contentWindow"
        ),
        "true"
    );
}

/// A `this` of the other origin in the caller's own functions: Web IDL's security check.
#[test]
fn own_members_refuse_a_cross_origin_this() {
    let mut s = page_with_one_frame();
    let page = &mut s.page;
    for js in [
        "Object.getOwnPropertyDescriptor(window, 'document').get.call(w)",
        "Object.getOwnPropertyDescriptor(window, 'name').get.call(w)",
        "Object.getOwnPropertyDescriptor(window, 'name').set.call(w, 'x')",
        "Object.getOwnPropertyDescriptor(window, 'localStorage').get.call(w)",
        "Object.getOwnPropertyDescriptor(window, 'history').get.call(w)",
        "Object.getOwnPropertyDescriptor(window, 'navigator').get.call(w)",
        "Object.getOwnPropertyDescriptor(window, 'innerWidth').get.call(w)",
        "Object.getOwnPropertyDescriptor(window, 'onmessage').get.call(w)",
        "Object.getOwnPropertyDescriptor(window, 'onmessage').set.call(w, () => {})",
        "Object.getOwnPropertyDescriptor(window, 'frameElement').get.call(w)",
        "Object.getOwnPropertyDescriptor(location, 'href').get.call(w.location)",
        "Object.getOwnPropertyDescriptor(location, 'search').get.call(w.location)",
        "Object.getOwnPropertyDescriptor(location, 'hash').set.call(w.location, 'x')",
        "location.assign.call(w.location, 'about:blank')",
        "location.reload.call(w.location)",
        "location.toString.call(w.location)",
        "setTimeout.call(w, () => {})",
        "getComputedStyle.call(w, document.body)",
        "requestAnimationFrame.call(w, () => {})",
        "EventTarget.prototype.addEventListener.call(w, 'message', () => {})",
        "EventTarget.prototype.removeEventListener.call(w, 'message', () => {})",
        "EventTarget.prototype.dispatchEvent.call(w, new Event('x'))",
    ] {
        assert_eq!(
            eval(page, &format!("isSecurityError(() => {js})")),
            "ok",
            "{js}"
        );
    }
    // The cross-origin members take it.
    assert_eq!(
        eval(
            page,
            "Object.getOwnPropertyDescriptor(window, 'parent').get.call(w) === window"
        ),
        "true"
    );
    assert_eq!(
        eval(page, "postMessage.call(w, 'to-frame', '*'); 'sent'"),
        "sent"
    );
    // A promise-returning operation rejects.
    eval(
        page,
        "window.rejected = ''; fetch.call(w, 'about:blank').catch(e => rejected = e.name); 'ok'",
    );
    wait(page, "rejected", "SecurityError");
}

/// Everything a cross-origin access gives is of the caller's realm or a WindowProxy.
#[test]
fn cross_origin_functions_and_errors_are_of_the_callers_realm() {
    let mut s = page_with_one_frame();
    let page = &mut s.page;
    for js in [
        "Object.getPrototypeOf(w.postMessage) === Function.prototype",
        "Object.getPrototypeOf(w.location.replace) === Function.prototype",
        "w.close.constructor === Function",
        "Object.getPrototypeOf(Object.getOwnPropertyDescriptor(w, 'top').get) === Function.prototype",
        "(() => { try { w.document } catch (e) { return Object.getPrototypeOf(e) === DOMException.prototype } })()",
        "(() => { try { w.postMessage() } catch (e) { return e instanceof TypeError } })()",
        "(() => { try { w.postMessage(1, 'not a url') } catch (e) { return e instanceof DOMException } })()",
        "(() => { try { w.postMessage(() => 1, '*') } catch (e) { return e instanceof DOMException } })()",
        // [[CrossOriginPropertyDescriptorMap]]
        "w.close === w.close",
        "w.close !== close",
        "w.location === w.location",
        "new MouseEvent('x', { view: w }).view === w",
        "new MessageEvent('x', { source: w }).source === w",
    ] {
        assert_eq!(eval(page, js), "true", "{js}");
    }
    // Not constructors, and bound: the receiver does not matter.
    assert_eq!(
        eval(page, "attempt(() => new w.postMessage('x', '*'))"),
        "TypeError"
    );
    assert_eq!(
        eval(
            page,
            "Object.getOwnPropertyDescriptor(w, 'parent').get.call(undefined) === window"
        ),
        "true"
    );
    // Script cannot serialize them or put them in a message.
    for js in [
        "structuredClone(w)",
        "structuredClone(w.location)",
        "postMessage(w, '*')",
        "w.postMessage(w.location, '*')",
    ] {
        assert_eq!(
            eval(page, &format!("attempt(() => {js})")),
            "DataCloneError",
            "{js}"
        );
    }
}

/// `contentDocument`, `frameElement` and everything else that would hand over a node.
#[test]
fn no_node_crosses_origins() {
    let mut s = page_with_one_frame();
    let page = &mut s.page;
    assert_eq!(
        eval(page, "String(document.getElementById('x').contentDocument)"),
        "null"
    );
    assert_eq!(
        eval(
            page,
            "attempt(() => document.getElementById('x').getSVGDocument())"
        ),
        "null"
    );
    // In the frame.
    let probe = "<script>
        addEventListener('message', e => {
          e.source.postMessage({
            frameElement: String(frameElement),
            parentDocument: isSecurityError(() => parent.document),
            topLocation: isSecurityError(() => top.location.href),
            sourceDocument: isSecurityError(() => e.source.document),
            ownSource: e.source === parent,
            viaFrames: parent.frames[0] === window,
            parentProto: String(Object.getPrototypeOf(parent)),
          }, '*');
        });
        parent.postMessage('ready', '*');
        </script>";
    let mut s = sites(
        "<iframe id=x src='{b}/p.html'></iframe>",
        vec![],
        vec![("/p.html", format!("<!doctype html>{HELPERS}{probe}<body>"))],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "got.join()", "ready");
    eval(
        page,
        "document.getElementById('x').contentWindow.postMessage('run', '*'); 'ok'",
    );
    wait(page, "got.length", "2");
    assert_eq!(
        eval(
            page,
            "const r = got[1]; [r.frameElement, r.parentDocument, r.topLocation, r.sourceDocument,
              r.ownSource, r.viaFrames, r.parentProto].join()"
        ),
        "null,ok,ok,ok,true,true,null"
    );
}

/// HTML §9.3.3: the true origin and source, in both directions and between frames of two origins.
#[test]
fn post_message_carries_the_true_origin_and_source() {
    let reply = "<script>
        addEventListener('message', e => {
          e.source.postMessage({ origin: e.origin, reply: e.data, fromParent: e.source === parent,
                                 dataRealm: e.data instanceof Object }, e.origin);
        });
        parent.postMessage('hello', '*');
        </script>";
    let mut s = sites(
        "<iframe id=x src='{b}/x.html'></iframe><iframe id=same src='/same.html'></iframe>",
        vec![(
            "/same.html",
            "<!doctype html><body><script>
             addEventListener('message', e => {
               parent.record('same got ' + e.data.n + ' from ' + e.origin + ' ' + (e.source === parent[0]));
             });
             </script>"
                .into(),
        )],
        vec![(
            "/x.html",
            format!(
                "<!doctype html><body>{reply}<script>
                 window.toSibling = () => parent[1].postMessage({{ n: 1 }}, '*');
                 addEventListener('message', e => {{ if (e.data == 'sibling') toSibling(); }});
                 </script>"
            ),
        )],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "messages.length", "1");
    // Child to parent.
    assert_eq!(eval(page, "messages[0].data"), "hello");
    let b = s.b.clone();
    assert_eq!(eval(page, "messages[0].origin"), b);
    assert_eq!(
        eval(
            page,
            "messages[0].source === document.getElementById('x').contentWindow"
        ),
        "true"
    );
    assert_eq!(eval(page, "messages[0].isTrusted"), "true");
    // Parent to child and back through `source`, addressed by origin.
    eval(
        page,
        "const w = messages[0].source; w.postMessage({ n: 2 }, messages[0].origin); 'ok'",
    );
    wait(page, "messages.length", "2");
    let a = s.a.clone();
    assert_eq!(eval(page, "messages[1].data.origin"), a);
    assert_eq!(eval(page, "messages[1].data.fromParent"), "true");
    assert_eq!(eval(page, "messages[1].data.dataRealm"), "true");
    assert_eq!(eval(page, "messages[1].data.reply.n"), "2");
    // A message for another origin stays undelivered; "/" means the sender's own.
    eval(
        page,
        &format!(
            "const v = messages[0].source;
             v.postMessage('wrong', {a:?});
             v.postMessage('own', '/');
             v.postMessage('right', {b:?}); 'ok'"
        ),
    );
    wait(page, "messages.length", "3");
    assert_eq!(eval(page, "messages[2].data.reply"), "right");
    page.run_for(Duration::from_millis(50));
    assert_eq!(eval(page, "messages.length"), "3");
    // Between siblings of different origins.
    eval(page, "messages[0].source.postMessage('sibling', '*'); 'ok'");
    wait(page, "log.length", "1");
    assert_eq!(eval(page, "log[0]"), format!("same got 1 from {b} true"));
}

/// The incumbent settings object: a same-origin frame's script calling a Window's `postMessage` is
/// the source, not the Window the function came from.
#[test]
fn post_message_source_is_the_incumbent_realm() {
    let mut s = sites(
        "<iframe id=a src='/a.html'></iframe><iframe id=b src='/b.html'></iframe>",
        vec![
            (
                "/a.html",
                "<!doctype html><body><script>
                 window.send = target => target.postMessage('from a', '*');
                 </script>"
                    .into(),
            ),
            (
                "/b.html",
                "<!doctype html><body><script>
                 window.got = [];
                 addEventListener('message', e => got.push(e.source === parent[0]));
                 </script>"
                    .into(),
            ),
        ],
        vec![],
    );
    let page = &mut s.page;
    wait_loaded(page);
    eval(page, "frames[0].send(frames[1]); 'ok'");
    wait(page, "frames[1].got.join()", "true");
}

/// A transferred port is a new object of the receiving realm, of the other origin as well.
#[test]
fn transferred_ports_belong_to_the_receiving_realm() {
    let probe = "<script>
        addEventListener('message', e => {
          const p = e.ports[0];
          p.onmessage = m => p.postMessage([
            p instanceof MessagePort,
            Object.getPrototypeOf(p) === MessagePort.prototype,
            m.data instanceof Object,
            String(m.source), JSON.stringify(m.origin)].join());
        });
        parent.postMessage('ready', '*');
        </script>";
    let mut t = sites(
        "<iframe id=x src='{b}/p.html'></iframe>",
        vec![],
        vec![("/p.html", format!("<!doctype html><body>{probe}"))],
    );
    let page = &mut t.page;
    wait_loaded(page);
    wait(page, "got.join()", "ready");
    eval(
        page,
        "window.answer = ''; window.c = new MessageChannel();
         c.port1.onmessage = m => answer = m.data;
         document.getElementById('x').contentWindow.postMessage('port', '*', [c.port2]);
         c.port1.postMessage({});
         'ok'",
    );
    wait(page, "answer", "true,true,true,null,\"\"");
    assert_eq!(eval(page, "c.port2 instanceof MessagePort"), "true");
}

/// One WindowProxy for the life of a browsing context, whatever it navigates to.
#[test]
fn a_reference_to_a_window_follows_its_browsing_context() {
    let mut s = sites(
        "<iframe id=f src='/same.html'></iframe>",
        vec![(
            "/same.html",
            "<!doctype html><body><script>window.mark = 'same'; </script>".into(),
        )],
        vec![(
            "/x.html",
            format!(
                "<!doctype html>{HELPERS}<body><script>
                 window.mark = 'x';
                 addEventListener('message', e => e.source.postMessage('from x', '*'));
                 </script>"
            ),
        )],
    );
    let page = &mut s.page;
    let b = s.b.clone();
    let a = s.a.clone();
    wait_loaded(page);
    eval(
        page,
        "window.f = document.getElementById('f');
         window.w = f.contentWindow;
         window.oldDocument = w.document;
         window.oldLocation = w.location;
         window.oldFunction = w.eval('() => window.mark');
         window.loads = 0; f.onload = () => loads++;
         w.planted = 1;
         'ok'",
    );
    assert_eq!(eval(page, "w.mark"), "same");
    // To another origin: the same object, now cross-origin.
    eval(page, &format!("f.src = '{b}/x.html'; 'ok'"));
    wait(page, "loads", "1");
    assert_eq!(eval(page, "f.contentDocument === null"), "true");
    assert_eq!(eval(page, "w === f.contentWindow"), "true");
    assert_eq!(eval(page, "frames[0] === w"), "true");
    assert_eq!(eval(page, "isSecurityError(() => w.document)"), "ok");
    assert_eq!(eval(page, "isSecurityError(() => w.mark)"), "ok");
    assert_eq!(eval(page, "isSecurityError(() => w.planted)"), "ok");
    assert_eq!(eval(page, "String(Object.getPrototypeOf(w))"), "null");
    eval(page, "w.postMessage('hi', '*'); 'ok'");
    wait(page, "got.join()", "from x");
    // The old realm's code is the old document's, and of the old origin: the WindowProxy it knows
    // shows it the new document no better than anyone else.
    assert_eq!(eval(page, "attempt(() => oldFunction())"), "SecurityError");
    // Back to this origin: the same object, same-origin again.
    eval(page, &format!("f.src = '{a}/same.html'; 'ok'"));
    wait(page, "loads", "2");
    assert_eq!(eval(page, "w === f.contentWindow"), "true");
    assert_eq!(eval(page, "w.document === f.contentDocument"), "true");
    assert_eq!(eval(page, "w.document !== oldDocument"), "true");
    assert_eq!(eval(page, "w.location !== oldLocation"), "true");
    assert_eq!(eval(page, "w.mark"), "same");
    assert_eq!(eval(page, "String(w.planted)"), "undefined");
    assert_eq!(eval(page, "w.window === w"), "true");
}

/// A document of another origin gets a Window and realm of its own, also out of the initial
/// `about:blank` document, into which its parent can reach.
#[test]
fn the_initial_realm_is_not_kept_for_another_origin() {
    let mut s = sites(
        "<script>
         const f = document.createElement('iframe');
         document.body.append(f);
         window.w = f.contentWindow;
         f.contentWindow.planted = 'by the parent';
         f.contentWindow.Array.prototype.poisoned = 'by the parent';
         f.src = '{b}/x.html';
         </script>",
        vec![],
        vec![(
            "/x.html",
            "<!doctype html><body><script>
             parent.postMessage(String(window.planted) + ',' + String([].poisoned), '*');
             </script>"
                .into(),
        )],
    );
    let page = &mut s.page;
    wait(page, "got.join()", "undefined,undefined");
    // A same-origin document does keep it.
    let mut s = sites(
        "<script>
         const f = document.createElement('iframe');
         document.body.append(f);
         f.contentWindow.planted = 'kept';
         window.w = f.contentWindow;
         f.src = '/same.html';
         </script>",
        vec![(
            "/same.html",
            "<!doctype html><body><script>parent.record(String(window.planted));</script>".into(),
        )],
        vec![],
    );
    let page = &mut s.page;
    wait(page, "log.join()", "kept");
}

/// Opaque origins: every `data:` document's is its own, and `about:` documents take their creator's.
#[test]
fn opaque_and_inherited_origins() {
    let mut s = sites(
        "<iframe id=d1 src='data:text/html,<script>window.mark=1</script>'></iframe>
         <iframe id=d2 src='data:text/html,<script>window.mark=1</script>'></iframe>
         <iframe id=s sandbox='allow-scripts' srcdoc='<script>window.mark=1</script>'></iframe>
         <iframe id=blank></iframe>
         <iframe id=doc srcdoc='<script>window.mark=2</script>'></iframe>",
        vec![],
        vec![],
    );
    let page = &mut s.page;
    wait_loaded(page);
    for id in ["d1", "d2", "s"] {
        assert_eq!(
            eval(
                page,
                &format!(
                    "isSecurityError(() => document.getElementById('{id}').contentWindow.mark)"
                )
            ),
            "ok",
            "{id}"
        );
    }
    assert_eq!(
        eval(
            page,
            "String(document.getElementById('d1').contentDocument)"
        ),
        "null"
    );
    assert_eq!(
        eval(
            page,
            "String(document.getElementById('blank').contentWindow.document.URL)"
        ),
        "about:blank"
    );
    assert_eq!(
        eval(page, "document.getElementById('doc').contentWindow.mark"),
        "2"
    );
}

/// `document.domain` (HTML §7.5.2): a document of a subdomain can relax to the parent domain, and
/// both must, whereupon access is allowed both ways; alone it makes a same-origin frame cross-origin.
#[test]
fn document_domain_decides_same_origin_domain() {
    let config = NetConfig {
        host_rules: HostRule::parse_list("MAP *.example.test 127.0.0.1"),
        ..Default::default()
    };
    let net = NetThread::start(config).unwrap();
    let frame = "<!doctype html><title>frame</title><body><script>
         window.mark = location.pathname;
         addEventListener('message', e => {
           document.domain = e.data;
           e.source.postMessage(document.domain + ' ' + parent.document.title, '*');
         });
         </script>";
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    };
    // (the page needs the port of the server it is on: the frames' URLs are resolved here)
    let top_page = format!(
        "<!doctype html><title>the top</title>{HELPERS}{TOP_SCRIPT}<body>\
         <iframe id=sub src='http://sub.example.test:{port}/sub.html'></iframe>\
         <iframe id=same src='/same.html'></iframe>"
    );
    serve_on(
        port,
        vec![
            ("/top.html", top_page),
            ("/same.html", frame.into()),
            ("/sub.html", frame.into()),
        ],
    );
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("http://www.example.test:{port}/top.html")).unwrap());
    let page = &mut page;
    wait_loaded(page);
    eval(
        page,
        "window.f = document.getElementById('sub'); window.w = f.contentWindow;
         window.g = document.getElementById('same'); window.same = g.contentWindow; 'ok'",
    );
    assert_eq!(eval(page, "isSecurityError(() => w.mark)"), "ok");
    assert_eq!(eval(page, "same.mark"), "/same.html");
    // What may not be set.
    for domain in ["com", "test", "other.test", "xample.test", ""] {
        assert_eq!(
            eval(
                page,
                &format!("attempt(() => document.domain = {domain:?})")
            ),
            "SecurityError",
            "{domain}"
        );
    }
    assert_eq!(eval(page, "document.domain"), "www.example.test");
    // One side alone: still cross-origin, and now also to the same-origin frame.
    assert_eq!(
        eval(page, "document.domain = 'example.test'; document.domain"),
        "example.test"
    );
    assert_eq!(eval(page, "isSecurityError(() => w.mark)"), "ok");
    assert_eq!(eval(page, "isSecurityError(() => same.mark)"), "ok");
    assert_eq!(
        eval(page, "isSecurityError(() => same.location.href)"),
        "ok"
    );
    assert_eq!(eval(page, "String(g.contentDocument)"), "null");
    // Both sides: each reaches the other, though their origins differ.
    eval(page, "w.postMessage('example.test', '*'); 'ok'");
    wait(page, "got.join()", "example.test the top");
    assert_eq!(eval(page, "w.mark"), "/sub.html");
    assert_eq!(eval(page, "w.location.hostname"), "sub.example.test");
    assert_eq!(eval(page, "f.contentDocument.domain"), "example.test");
    assert_eq!(eval(page, "w.parent === window"), "true");
    assert_eq!(eval(page, "isSecurityError(() => same.mark)"), "ok");
}

/// A `Location` setter is an origin's way to navigate a frame it cannot read.
#[test]
fn a_cross_origin_location_can_be_set() {
    let mut s = page_with_one_frame();
    let page = &mut s.page;
    eval(
        page,
        "window.f = document.getElementById('x');
         window.loads = 0; f.onload = () => loads++;
         w.location = 'about:blank#1'; 'ok'",
    );
    wait(page, "loads", "1");
    // (`about:blank` takes the origin of the document that navigated to it)
    assert_eq!(eval(page, "f.contentDocument.URL"), "about:blank#1");
    assert_eq!(eval(page, "f.contentWindow === w"), "true");
    eval(page, "w.location.href = 'plain.html?2'; 'ok'");
    wait(page, "loads", "2");
    assert_eq!(eval(page, "f.contentWindow === w"), "true");
    assert_eq!(eval(page, "w.location.search"), "?2");
    assert_eq!(
        eval(page, "w.location.pathname"),
        "/plain.html",
        "relative to the entry settings object"
    );
    eval(page, "w.location.replace('/plain.html?3'); 'ok'");
    wait(page, "loads", "3");
    assert_eq!(eval(page, "w.location.search"), "?3");
}

/// An iframe's `sandbox` attribute (HTML §4.8.5): without `allow-same-origin` the document has an
/// origin of its own, and what needs one fails; the flags are those of when the navigation started.
#[test]
fn a_sandboxed_frame_has_an_opaque_origin() {
    let probe = "<script>
        const attempt = f => { try { return String(f()); } catch (e) { return e.name; } };
        parent.postMessage([
          attempt(() => document.cookie),
          attempt(() => localStorage.length),
          attempt(() => document.domain = 'x'),
          attempt(() => parent.document),
          attempt(() => parent.location.href),
          String(frameElement),
          attempt(() => top.location = '/nowhere.html'),
          attempt(() => top.location.href = '/nowhere.html'),
        ].join(), '*');
        </script>";
    let mut s = sites(
        "<iframe id=opaque sandbox='allow-scripts' src='/probe.html'></iframe>",
        vec![("/probe.html", format!("<!doctype html><body>{probe}"))],
        vec![],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "got.length", "1");
    assert_eq!(
        eval(page, "got[0]"),
        "SecurityError,SecurityError,SecurityError,SecurityError,SecurityError,null,\
         SecurityError,SecurityError"
    );
    assert_eq!(eval(page, "messages[0].origin"), "null");
    assert_eq!(
        eval(
            page,
            "String(document.getElementById('opaque').contentDocument)"
        ),
        "null"
    );
    // `allow-same-origin` keeps the origin; what is set after the navigation started does not count.
    let mut t = sites(
        "<iframe id=kept sandbox='allow-scripts allow-same-origin' src='/m.html'></iframe>
         <script>
           const late = document.createElement('iframe');
           late.id = 'late'; late.src = '/m.html';
           document.body.append(late);
           late.sandbox = 'allow-scripts';
         </script>",
        vec![(
            "/m.html",
            "<!doctype html><script>window.mark = 1</script>".into(),
        )],
        vec![],
    );
    let page = &mut t.page;
    wait_loaded(page);
    for id in ["kept", "late"] {
        assert_eq!(
            eval(
                page,
                &format!("document.getElementById('{id}').contentWindow.mark")
            ),
            "1",
            "{id}"
        );
    }
}

/// HTML §7.2.2.1: `event` is its own Window's current event, not another realm's.
#[test]
fn window_event_is_of_the_listeners_own_global() {
    let mut s = sites(
        "<iframe id=f src='/f.html'></iframe>",
        vec![(
            "/f.html",
            "<!doctype html><script>window.seen = () => String(window.event)</script>".into(),
        )],
        vec![],
    );
    let page = &mut s.page;
    wait_loaded(page);
    assert_eq!(
        eval(
            page,
            "window.result = []; addEventListener('x', e => {
               result.push(window.event === e, frames[0].seen());
             });
             dispatchEvent(new Event('x')); result.join()"
        ),
        "true,undefined"
    );
}

// ----- realms of what the engine makes -------------------------------------------------------------------
//
// Cross-origin attacks from a review of 2026-11-06. The two origins share a V8
// isolate, so one object of the wrong realm hands its `Function` to the other origin.

/// What a frame's realm calls itself, and `report(name, e)`: tells the top page which realm's
/// `Function` script reaches through `e`.
const WHOAMI: &str = "<script>
    window.whoami = 'frame';
    window.report = (name, e) => parent.postMessage(
        name + ': ' + e.constructor.constructor('return window.whoami')(), '*');
    </script>";

/// DOM §2.10 "fire an event": the event is created in the relevant realm of its target, so a frame's
/// listener gets none of the top page's objects, with the event fired at a Window, an element or an
/// object that is no node, to a listener or an event handler.
#[test]
fn an_event_the_engine_fires_is_of_its_targets_realm() {
    let mut s = sites(
        "<script>window.whoami = 'top';</script><iframe src='{b}/f.html'></iframe>",
        vec![],
        vec![(
            "/f.html",
            format!(
                "<!doctype html>{WHOAMI}<body>
                 <script>
                 addEventListener('DOMContentLoaded', e => report('window listener', e));
                 onload = e => report('window handler', e);
                 const xhr = new XMLHttpRequest();
                 xhr.open('GET', '/f.html');
                 xhr.addEventListener('load', e => report('xhr listener', e));
                 xhr.send();
                 AbortSignal.timeout(1).addEventListener('abort', e => report('signal listener', e));
                 </script>
                 <script src='/missing.js' onerror=\"report('element handler', event)\"></script>"
            ),
        )],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "got.length", "5");
    assert_eq!(
        eval(page, "got.sort().join()"),
        "element handler: frame,signal listener: frame,window handler: frame,\
         window listener: frame,xhr listener: frame"
    );
}

/// The reverse of the above: the top page's `Event.prototype` is not the prototype of an event
/// fired in a frame of another origin, or patching it would hand the top page the frame's nodes.
#[test]
fn a_patched_event_prototype_sees_no_event_of_another_origin() {
    let mut s = sites(
        "<script>
         window.stolen = [];
         const target = Object.getOwnPropertyDescriptor(Event.prototype, 'target').get;
         Object.defineProperty(Event.prototype, 'target', {
           get() {
             const t = target.call(this);
             stolen.push((t.ownerDocument || t).title);
             return t;
           },
         });
         </script><iframe src='{b}/f.html'></iframe>",
        vec![],
        vec![(
            "/f.html",
            "<!doctype html><title>the frame's secret</title><script>
             addEventListener('load', e => {
               e.target;
               parent.postMessage('read', '*');
             });
             </script>"
                .into(),
        )],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "got.join()", "read");
    assert_eq!(eval(page, "stolen.join()"), "");
}

/// The event's realm is its target's, not its listener's: a listener of the top page on a same-origin
/// frame's Window gets an event of the frame's realm, and its first wrapper is made for that listener.
#[test]
fn an_event_is_not_of_its_listeners_realm() {
    let mut s = sites(
        "<script>
         window.watch = target => target.addEventListener('load', e => {
           record([e instanceof target.Event, e instanceof Event].join());
         });
         </script><iframe src='/f.html'></iframe>",
        vec![(
            "/f.html",
            "<!doctype html><script>parent.watch(window)</script>".into(),
        )],
        vec![],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "log.join()", "true,false");
}

/// A platform object keeps its realm when a node is adopted, so an event fired at a form moved to
/// another frame's document is of the realm of the form's wrapper, not of that document's window.
#[test]
fn an_event_is_of_its_targets_realm_after_the_target_is_adopted() {
    let mut s = sites(
        "<iframe src='/form.html'></iframe><iframe src='/plain.html'></iframe>",
        vec![
            ("/form.html", "<!doctype html><form></form>".into()),
            ("/plain.html", "<!doctype html><body>".into()),
        ],
        vec![],
    );
    let page = &mut s.page;
    wait_loaded(page);
    assert_eq!(
        eval(
            page,
            "const form = frames[0].document.forms[0];
             frames[1].document.body.appendChild(form);
             let same;
             form.onreset = e => { same = e.constructor.constructor === form.constructor.constructor; };
             form.reset();
             String(same)"
        ),
        "true"
    );
}

/// DOM §2.9 "inner invoke": what a listener throws is reported for its callback's realm's global
/// object. Reported to the top page, the frame's exception object would cross the origins.
#[test]
fn a_listeners_exception_is_reported_to_its_own_window() {
    let mut s = sites(
        "<script>
         window.reported = [];
         addEventListener('error', e => {
           reported.push(e.message);
           e.preventDefault();
         });
         </script><iframe src='{b}/f.html'></iframe>",
        vec![],
        vec![(
            "/f.html",
            "<!doctype html><title>the frame's secret</title><script>
             addEventListener('error', e => {
               e.preventDefault();
               parent.postMessage('reported to the frame', '*');
             });
             addEventListener('load', () => { throw new Error('from the listener'); });
             setTimeout(() => parent.postMessage('end', '*'), 200);
             </script>"
                .into(),
        )],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "got.includes('end')", "true");
    assert_eq!(
        eval(page, "[got.join(), reported.join()].join(' / ')"),
        "reported to the frame,end / "
    );
}

/// A frame reading `top.location` first must not make the top Location's own `[LegacyUnforgeable]`
/// members objects of the frame's realm: the top page would get the frame's `Function.prototype` from
/// them, and through it plant properties on every object of the frame. (V8 keeps it from calling the
/// frame's `Function` itself: that constructor checks the entered realm's access.)
#[test]
fn a_locations_own_members_are_of_its_realm_when_another_origin_makes_its_wrapper() {
    let mut s = sites(
        "<iframe src='{b}/f.html'></iframe>",
        vec![],
        vec![(
            "/f.html",
            "<!doctype html><script>
             top.location;
             addEventListener('message', () => parent.postMessage(String(({}).planted), '*'));
             parent.postMessage('touched', '*');
             </script>"
                .into(),
        )],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "got.join()", "touched");
    eval(
        page,
        "got.length = 0;
         Object.getPrototypeOf(Object.getPrototypeOf(location.assign)).planted = 'by the top page';
         frames[0].postMessage('look', '*'); 'ok'",
    );
    wait(page, "got.length", "1");
    assert_eq!(eval(page, "got[0]"), "undefined");
    assert_eq!(
        eval(
            page,
            "[Object.getPrototypeOf(location.assign) === Function.prototype,
              Object.getPrototypeOf(location) === Location.prototype,
              Object.getOwnPropertyDescriptor(location, 'href').get.constructor === Function].join()"
        ),
        "true,true,true"
    );
}

/// The reverse: a parent reading a child's `location` first must leave the child's own Location
/// working in the child, and none of the parent's objects in it.
#[test]
fn a_frames_location_works_after_another_origin_made_its_wrapper() {
    let mut s = sites(
        "<iframe id=f src='{b}/f.html'></iframe>",
        vec![],
        vec![(
            "/f.html",
            format!(
                "<!doctype html>{HELPERS}<script>
                 addEventListener('message', () => parent.postMessage([
                   attempt(() => location.href),
                   attempt(() => location.replace.constructor === Function),
                   attempt(() => Object.getPrototypeOf(location) === Location.prototype),
                 ].join(), '*'));
                 parent.postMessage('ready', '*');
                 </script>"
            ),
        )],
    );
    let b = s.b.clone();
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "got.join()", "ready");
    eval(
        page,
        "got.length = 0; frames[0].location; frames[0].postMessage('go', '*'); 'ok'",
    );
    wait(page, "got.length", "1");
    assert_eq!(eval(page, "got[0]"), format!("{b}/f.html,true,true"));
}

/// HTML §8.1.3.5: with no script on the stack the incumbent settings object is the callback's (the
/// backup incumbent settings object stack), so a native function bound as a listener or an event
/// handler cannot send a message in the name of the Window it is bound to.
#[test]
fn a_bound_post_message_cannot_spoof_its_target() {
    let mut s = sites(
        "<iframe id=f src='{b}/f.html'></iframe>",
        vec![],
        vec![(
            "/f.html",
            "<!doctype html><script>
             addEventListener('load', top.postMessage.bind(top, 'listener', '*', []));
             addEventListener('load', top.postMessage.bind(top, 'to the origin', '/', []));
             onload = top.postMessage.bind(top, 'handler', '*', []);
             addEventListener('load', new Proxy(top.postMessage.bind(top, 'proxy', '*', []), {}));
             addEventListener('load', () => setTimeout(() => top.postMessage('end', '*'), 50));
             </script>"
                .into(),
        )],
    );
    let b = s.b.clone();
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "got.includes('end')", "true");
    let check = "messages.map(e => [e.data, e.origin, e.source === frames[0]]).join(' ')";
    assert_eq!(
        eval(page, check),
        format!("listener,{b},true handler,{b},true proxy,{b},true end,{b},true")
    );
}

/// Web IDL's callback context: the incumbent settings object when the listener was converted, not
/// the callback's own realm, and not the script that happens to dispatch the event.
#[test]
fn the_incumbent_of_a_native_listener_is_the_realm_that_added_it() {
    let mut s = sites(
        "<iframe src='/adds.html'></iframe><iframe src='/owns.html'></iframe>",
        vec![
            (
                "/adds.html",
                "<!doctype html><body><script>
                 window.add = f => document.body.addEventListener('x', f);
                 </script>"
                    .into(),
            ),
            ("/owns.html", "<!doctype html><body>".into()),
        ],
        vec![],
    );
    let page = &mut s.page;
    wait_loaded(page);
    eval(
        page,
        "frames[0].add(frames[1].postMessage.bind(top, 'native', '*', []));
         frames[0].document.body.dispatchEvent(new Event('x')); 'ok'",
    );
    wait(page, "got.join()", "native");
    assert_eq!(eval(page, "messages[0].source === frames[0]"), "true");
}

/// An attacker's frame on the other origin, `/evil.html`, with a same-origin child it keeps `Function`
/// and `Text` of (as `F` and `T`) and then navigates, so they are of a realm whose Window was replaced.
/// `script` runs after that, with `report(v)` posting `v` to the top page. `frame_attributes` go on the
/// attacker's iframe.
fn replaced_realm(
    top_body: &str,
    frame_attributes: &str,
    a_files: Vec<(&'static str, String)>,
    script: &str,
) -> Sites {
    let mut s = sites(
        &format!("{top_body}<iframe {frame_attributes} src='{{b}}/evil.html'></iframe>"),
        a_files,
        vec![
            ("/child.html", "<!doctype html><title>child</title>".into()),
            ("/other.html", "<!doctype html><title>other</title>".into()),
            (
                "/evil.html",
                format!(
                    "<!doctype html><body><iframe id=c src='/child.html'></iframe><script>
                     const c = document.getElementById('c');
                     const report = v => parent.postMessage(String(v), '*');
                     addEventListener('load', () => {{
                       const w = c.contentWindow;
                       window.F = w.Function; window.T = w.Text;
                       c.onload = () => {{ c.onload = null; {script} }};
                       c.src = '/other.html';
                     }});
                     </script>"
                ),
            ),
        ],
    );
    wait_loaded(&mut s.page);
    s
}

/// The review's finding 3: the current global's associated document stays its own after a navigation
/// replaced the Window. It used to fall back to the top-level document, which gave the attacker's
/// frame the victim's `Document`.
#[test]
fn a_replaced_realms_constructor_makes_nodes_of_its_own_document() {
    let mut s = replaced_realm("", "", vec![], "report(new T('').ownerDocument.URL);");
    wait(&mut s.page, "got.length", "1");
    let b = s.b.clone();
    assert_eq!(eval(&mut s.page, "got.join()"), format!("{b}/child.html"));
}

#[test]
fn a_replaced_realms_document_is_not_the_top_level_one() {
    let mut s = replaced_realm(
        "",
        "",
        vec![],
        "report(F('return document.URL')()); report(F('return window.document.URL')());",
    );
    wait(&mut s.page, "got.length", "2");
    let (a, seen) = (s.a.clone(), eval(&mut s.page, "got.join(' ')"));
    assert!(!seen.contains(&a), "{seen}");
}

/// `fetch` from such a realm is its own document's: a relative URL is not resolved against the
/// top-level document, and the top-level document is not the request's client.
#[test]
fn a_replaced_realm_does_not_fetch_as_the_top_level_document() {
    let mut s = replaced_realm(
        "",
        "",
        vec![("/secret.html", "the secret".into())],
        "F('return fetch(\"/secret.html\").then(r => r.text())')()
           .then(report, e => report(e.name));",
    );
    wait(&mut s.page, "got.length", "1");
    assert_ne!(eval(&mut s.page, "got.join()"), "the secret");
}

/// The navigation's source document is the entry realm's own: its sandbox flags apply, not the
/// top-level document's lack of them.
#[test]
fn a_replaced_realm_keeps_its_sandbox_for_navigating_the_top() {
    let mut s = replaced_realm(
        "",
        "sandbox='allow-scripts allow-same-origin'",
        vec![("/pwned.html", "<!doctype html><title>pwned</title>".into())],
        "const go = F('return () => { top.location = \"/pwned.html\"; }')();
         document.body.addEventListener('x', go);
         document.body.dispatchEvent(new Event('x'));
         setTimeout(go);
         setTimeout(() => report('done'), 100);",
    );
    wait(
        &mut s.page,
        "typeof got == 'undefined' || got.includes('done')",
        "true",
    );
    s.page.run_for(Duration::from_millis(300));
    let a = s.a.clone();
    assert_eq!(eval(&mut s.page, "location.href"), format!("{a}/top.html"));
}

/// An uncaught exception or unhandled rejection of such a realm says nothing to the top-level page,
/// and hands it none of that realm's objects.
#[test]
fn a_replaced_realms_errors_do_not_reach_the_top_level_window() {
    let mut s = replaced_realm(
        "<script>
         addEventListener('error', e => { log.push('error'); e.preventDefault(); });
         addEventListener('unhandledrejection', e => { log.push('rejection'); e.preventDefault(); });
         </script>",
        "",
        vec![],
        "setTimeout(F('return () => { throw new Error(\"of the old realm\"); }')());
         F('return Promise.reject(new Error(\"of the old realm\"))')();
         setTimeout(() => report('done'), 100);",
    );
    wait(&mut s.page, "got.includes('done')", "true");
    assert_eq!(eval(&mut s.page, "log.join()"), "");
}

/// HTML Â§8.1.5.7 "notify about rejected promises": the event is the promise's own global's. The top
/// page must not get a frame's promise and reason, nor the frame lose its event.
#[test]
fn an_unhandled_rejection_is_reported_to_its_own_window() {
    let mut s = sites(
        "<script>
         addEventListener('unhandledrejection', e => { log.push('top'); e.preventDefault(); });
         </script><iframe src='{b}/f.html'></iframe>",
        vec![],
        vec![(
            "/f.html",
            "<!doctype html><script>
             addEventListener('unhandledrejection', e => {
               e.preventDefault();
               parent.postMessage('reported to the frame', '*');
             });
             Promise.reject(new Error('the frame\\'s secret'));
             setTimeout(() => parent.postMessage('end', '*'), 200);
             </script>"
                .into(),
        )],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "got.includes('end')", "true");
    assert_eq!(
        eval(page, "[got.join(), log.join()].join(' / ')"),
        "reported to the frame,end / "
    );
}

/// `import()` in a realm whose document is gone is refused: its module map went with the document.
#[test]
fn a_replaced_realm_imports_no_module() {
    let mut s = replaced_realm(
        "",
        "",
        vec![],
        "F('return import(\"/m.js\")')().then(() => report('loaded'), e => report(e.name));",
    );
    wait(&mut s.page, "got.length", "1");
    assert_eq!(eval(&mut s.page, "got.join()"), "TypeError");
}

/// CSSOM §6.1.2: a constructed sheet's base URL comes from the constructor document, not the top one.
/// A `data:` document's URL is no base for a relative one, though the top-level document's would be.
#[test]
fn a_constructed_sheets_base_url_is_its_own_documents() {
    let mut s = sites(
        "<iframe src=\"data:text/html,<script>
           let r = 'made';
           try { new CSSStyleSheet({ baseURL: 'sub/' }); } catch (e) { r = e.name; }
           parent.postMessage(r, '*');
         </script>\"></iframe>",
        vec![],
        vec![],
    );
    wait(&mut s.page, "got.length", "1");
    assert_eq!(eval(&mut s.page, "got.join()"), "NotAllowedError");
}

// ----- child navigables and their parent's navigation ------------------------------------------------------

// The review's finding 4: a document's child navigables are destroyed with it, and an `about:` document
// keeps the origin it was made with. Before, the children lived on under the parent's next document, of
// whatever origin, and an `about:` document took its origin from that document.

/// The attacker's frame on the other origin has a `srcdoc` child whose timer writes storage later, and
/// navigates itself to the top page's origin first. An orphaned child that took its origin from the live
/// parent would write the top page's storage.
#[test]
fn an_orphaned_frame_does_not_run_under_its_parents_next_document() {
    let mut s = sites(
        "<iframe src='{b}/evil.html'></iframe><script>
         addEventListener('load', () => frames[0].postMessage(location.origin, '*'));
         </script>",
        vec![(
            "/landing.html",
            "<!doctype html><title>landing</title>".into(),
        )],
        vec![(
            "/evil.html",
            "<!doctype html><body><iframe srcdoc=\"<script>
               setTimeout(() => localStorage.setItem('pwned', 'yes'), 150);
             </script>\"></iframe><script>
             addEventListener('message', e => { location = e.data + '/landing.html'; });
             </script>"
                .into(),
        )],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "attempt(() => frames[0].document.title)", "landing");
    // (`run_for` stops when nothing is due: this waits past the attacker's timer.)
    eval(
        page,
        "window.later = false; setTimeout(() => later = true, 300); 'ok'",
    );
    wait(page, "String(later)", "true");
    assert_eq!(eval(page, "String(localStorage.getItem('pwned'))"), "null");
}

/// A same-origin frame's `srcdoc` child, kept by the top page (its Window and its `Function`), after the
/// frame navigated to the other origin: the child is discarded, its realm reads its own origin's storage,
/// not that of the document its parent shows now, and its listeners do not run.
#[test]
fn an_about_document_keeps_the_origin_it_was_made_with() {
    let mut s = sites(
        "<iframe id=x src='/x.html'></iframe>",
        vec![(
            "/x.html",
            "<!doctype html><iframe srcdoc='<p>child'></iframe>".into(),
        )],
        vec![(
            "/b.html",
            "<!doctype html><title>b</title><script>
             localStorage.setItem('k', 'the other origin\\'s secret');
             parent.postMessage('b loaded', '*');
             </script>"
                .into(),
        )],
    );
    let b = s.b.clone();
    let page = &mut s.page;
    wait_loaded(page);
    eval(
        page,
        &format!(
            "const x = document.getElementById('x');
             window.cw = x.contentWindow.frames[0]; window.F = cw.Function;
             x.src = '{b}/b.html'; 'ok'"
        ),
    );
    wait(page, "got.join()", "b loaded");
    assert_eq!(
        eval(page, "[cw.closed, cw.parent === null].join()"),
        "true,true"
    );
    assert_eq!(
        eval(page, "String(F('return localStorage.getItem(\"k\")')())"),
        "null"
    );
    assert_eq!(
        eval(
            page,
            "const g = F('window.hits = 0; return () => { hits++; }')();
             document.body.addEventListener('x', g);
             document.body.dispatchEvent(new Event('x'));
             String(F('return hits')())"
        ),
        "0"
    );
}

/// An `about:blank` document takes the origin of the document that navigated to it, for its requests as
/// well: the attacker's frame navigates the top page's other frame to `about:blank` and fetches through
/// it. Its requests used to be made with the parent's origin, so the attacker read the top page's
/// origin's responses as same-origin ones.
#[test]
fn an_about_blank_document_requests_with_its_initiators_origin() {
    let mut s = sites(
        "<iframe src='/plain.html'></iframe><iframe src='{b}/evil.html'></iframe><script>
         addEventListener('load', () => frames[1].postMessage(location.origin, '*'));
         </script>",
        vec![
            ("/plain.html", "<!doctype html><title>plain</title>".into()),
            ("/secret.html", "the secret".into()),
        ],
        vec![(
            "/evil.html",
            "<!doctype html><script>
             addEventListener('message', e => {
               const a = e.data, v = parent.frames[0];
               v.location = 'about:blank';
               const poll = setInterval(() => {
                 let doc;
                 try { doc = v.document; } catch (_) { return; }
                 if (doc.URL != 'about:blank') return;
                 clearInterval(poll);
                 v.fetch(a + '/secret.html').then(r => r.text())
                   .then(t => parent.postMessage(t, '*'), e => parent.postMessage(e.name, '*'));
               }, 10);
             });
             </script>"
                .into(),
        )],
    );
    let page = &mut s.page;
    wait_loaded(page);
    wait(page, "got.length", "1");
    assert_eq!(eval(page, "got.join()"), "TypeError");
}

/// Named access through a cross-origin parent (HTML §7.2.3.3 "document-tree child navigable target
/// name property set", with the engines' clause kept from its earlier text): a child is reached by
/// name when it is same origin with the parent or when its name is its container's `name` attribute,
/// so a frame finds itself and its sibling of its own origin; a
/// child renamed by script, and any other name, are a `SecurityError` thrown as a DOMException with a
/// `stack`, which `window.onerror` reports as "Uncaught SecurityError: ...".
#[test]
fn named_access_through_a_cross_origin_parent_reaches_children_by_container_name() {
    let mut s = sites(
        "<iframe name=kid src='{b}/kid.html'></iframe>\
         <iframe name=twin src='{b}/plain.html'></iframe>\
         <iframe name=other src='about:blank'></iframe>",
        vec![],
        vec![
            (
                "/kid.html",
                format!(
                    "<!doctype html>{HELPERS}<script>
                     window.onerror = m => {{ parent.postMessage('onerror ' + m, '*'); }};
                     parent.postMessage([
                       attempt(() => parent.frames['kid'] === window),
                       attempt(() => [0, 1, 2].some(i => parent.frames[i] === window)),
                       attempt(() => typeof parent.frames['other']),
                       attempt(() => parent.frames['nope']),
                       attempt(() => parent.frames.nope2),
                       attempt(() => typeof new DOMException('x', 'SecurityError').stack),
                       attempt(() => parent.frames['twin'] === parent.frames[1]),
                       attempt(() => (window.name = 'alias', parent.frames['alias'])),
                       attempt(() => parent.frames['kid']),
                     ].join(' '), '*');
                     parent.frames['missing'];
                     </script>"
                ),
            ),
            ("/plain.html", "<!doctype html><title>plain</title>".into()),
        ],
    );
    wait_loaded(&mut s.page);
    wait(&mut s.page, "String(got.length)", "2");
    assert_eq!(
        eval(&mut s.page, "got[0]"),
        "true true object SecurityError SecurityError string true SecurityError SecurityError"
    );
    assert!(
        eval(&mut s.page, "got[1]").contains("SecurityError"),
        "{}",
        eval(&mut s.page, "got[1]")
    );
}

#[test]
fn a_documents_referrer_is_its_navigation_requests_referrer() {
    // HTML "create and initialize a Document object" (a fetched document: its request's referrer,
    // stripped by the default policy, strict-origin-when-cross-origin) and "create a new browsing
    // context and document" (the initial about:blank: its creator's URL).
    let mut s = sites(
        "<iframe id=other src='{b}/r.html'></iframe><iframe id=same src='/plain.html'></iframe>\
         <iframe id=blank></iframe>",
        vec![("/plain.html", "<!doctype html><title>plain</title>".into())],
        vec![(
            "/r.html",
            "<!doctype html><script>parent.postMessage('ref ' + document.referrer, '*')</script>"
                .into(),
        )],
    );
    wait_loaded(&mut s.page);
    let a = s.a.clone();
    let page = &mut s.page;
    wait(page, "got.join()", &format!("ref {a}/"));
    assert_eq!(
        eval(
            page,
            "document.getElementById('same').contentDocument.referrer"
        ),
        format!("{a}/top.html")
    );
    assert_eq!(
        eval(
            page,
            "document.getElementById('blank').contentDocument.referrer"
        ),
        format!("{a}/top.html")
    );
    // The user's own navigation of the top-level page sends none.
    assert_eq!(eval(page, "document.referrer"), "");
}

// ----- window.open() and auxiliary browsing contexts (HTML §7.2.2.1, §7.3.1.7) -----------------------

/// A page of origin `a` that opens popups of `a` and `b`: each popup's document tells its opener
/// it loaded.
fn popup_sites() -> Sites {
    let popup = "<!doctype html><title>popup</title><script>
        opener && opener.postMessage('loaded ' + location.pathname + ' ' + name, '*');
        </script>";
    sites(
        "",
        vec![
            ("/popup.html", popup.into()),
            ("/plain.html", "<!doctype html><title>plain</title>".into()),
        ],
        vec![("/popup.html", popup.into())],
    )
}

#[test]
fn window_open_makes_an_auxiliary_browsing_context_with_an_opener() {
    let mut s = popup_sites();
    wait_loaded(&mut s.page);
    let page = &mut s.page;
    // The initial about:blank document is there at once, with the opener's origin.
    assert_eq!(
        eval(
            page,
            "window.w = open('/popup.html', 'pop');
             [w === window, w.opener === window, w.name, w.parent === w, w.top === w, w.closed,
              w.document.URL, w.origin === origin, w.frameElement].join()"
        ),
        "false,true,pop,true,true,false,about:blank,true,"
    );
    wait(page, "got.join()", "loaded /popup.html pop");
    assert_eq!(
        eval(
            page,
            "[w.document.title, w.opener === window, open('', 'pop') === w,
              w.document.referrer.endsWith('/top.html'), w.toolbar.visible].join()"
        ),
        "popup,true,true,true,true"
    );
    // A popup asked for with minimal features has its bars hidden.
    assert_eq!(
        eval(
            page,
            "open('', 'small', 'width=300,height=200').toolbar.visible"
        ),
        "false"
    );
    // `close()`: closing at once, closed after a task.
    assert_eq!(eval(page, "w.close(); w.closed"), "true");
    wait(page, "String(w.closed && w.opener === null)", "true");
    // A closed popup's name is free again: a new one is opened.
    assert_eq!(eval(page, "open('', 'pop') !== w"), "true");
}

#[test]
fn a_popup_of_another_origin_gets_a_cross_origin_window_proxy() {
    let mut s = popup_sites();
    wait_loaded(&mut s.page);
    let b = s.b.clone();
    let page = &mut s.page;
    eval(
        page,
        &format!("window.w = open('{b}/popup.html', 'other'); 0"),
    );
    wait(page, "got.join()", "loaded /popup.html other");
    assert_eq!(eval(page, "isSecurityError(() => w.document)"), "ok");
    assert_eq!(eval(page, "isSecurityError(() => w.name)"), "ok");
    assert_eq!(eval(page, "String(w.closed)"), "false");
    // Navigating it by name and closing it are allowed across origins.
    assert_eq!(eval(page, "w.close(); String(w.closed)"), "true");
}

#[test]
fn the_opener_attribute_can_be_cleared_or_replaced() {
    let mut s = popup_sites();
    wait_loaded(&mut s.page);
    let page = &mut s.page;
    eval(page, "window.w = open('/plain.html'); 0");
    wait(page, "w.document.title", "plain");
    assert_eq!(
        eval(
            page,
            "w.opener = null; [w.opener, w.eval('opener')].join('/')"
        ),
        "/"
    );
    assert_eq!(eval(page, "w.opener = 5; String(w.opener)"), "5");
}

#[test]
fn noopener_windows_have_no_opener_and_a_group_of_their_own() {
    let report = "<!doctype html><script>
        new BroadcastChannel('t').postMessage(
          [location.pathname, name, opener === null, document.referrer === ''].join(' '));
        </script>";
    let mut s = sites("", vec![("/report.html", report.into())], vec![]);
    wait_loaded(&mut s.page);
    let page = &mut s.page;
    assert_eq!(
        eval(
            page,
            "window.bc = new BroadcastChannel('t'); bc.onmessage = e => got.push(e.data);
             [open('/report.html', 'n1', 'noopener'), open('/report.html', 'n2', 'noreferrer=1'),
              open('/report.html', '_blank', 'noopener=0') === null].join()"
        ),
        ",,false"
    );
    wait(page, "got.length", "3");
    assert_eq!(
        eval(page, "got.sort().join('|')"),
        "/report.html  false false|/report.html n1 true false|/report.html n2 true true"
    );
    // The browser is asked for no tab: the page hosts them.
    assert!(page.engine().take_open_requests().is_empty());
    // Another browsing context group's name is not found: a new window is made.
    assert_eq!(eval(page, "open('', 'n1').document.URL"), "about:blank");
    // A URL that does not parse throws, as before.
    assert_eq!(
        eval(page, "attempt(() => open('http://[bad'))"),
        "SyntaxError"
    );
}

#[test]
fn links_and_forms_with_a_new_target_open_windows_with_their_noopener() {
    let report = "<!doctype html><script>
        new BroadcastChannel('t').postMessage(
          [location.search, name, opener === null, document.referrer === ''].join(' '));
        </script>";
    let mut s = sites("", vec![("/report.html", report.into())], vec![]);
    wait_loaded(&mut s.page);
    let page = &mut s.page;
    eval(
        page,
        "window.bc = new BroadcastChannel('t'); bc.onmessage = e => got.push(e.data);
         const a = (target, rel) => {
           const l = document.createElement('a');
           l.href = '/report.html?' + target + '-' + rel;
           l.target = target;
           if (rel) l.rel = rel;
           document.body.append(l);
           l.click();
         };
         a('_blank', ''); a('_blank', 'opener'); a('named', ''); a('named2', 'noreferrer');
         const f = document.createElement('form');
         f.action = '/report.html'; f.target = 'formwin';
         f.innerHTML = '<input name=q value=1>';
         document.body.append(f); f.submit(); 0",
    );
    wait(page, "got.length", "5");
    assert_eq!(
        eval(page, "got.sort().join('|')"),
        "?_blank-  true false|?_blank-opener  false false|?named- named false false|\
         ?named2-noreferrer named2 true true|?q=1 formwin false false"
    );
    assert!(page.engine().take_open_requests().is_empty());
}

#[test]
fn a_browser_that_opens_tabs_gets_the_url_and_its_popup_blocker_needs_activation() {
    let mut s = popup_sites();
    wait_loaded(&mut s.page);
    let a = s.a.clone();
    let page = &mut s.page;
    page.engine().set_new_windows_in_tabs(true);
    assert_eq!(eval(page, "String(open('/plain.html'))"), "null");
    let asked: Vec<String> = page
        .engine()
        .take_open_requests()
        .into_iter()
        .map(|(u, _)| u.to_string())
        .collect();
    assert_eq!(asked, vec![format!("{a}/plain.html")]);
    // Without a user gesture, the popup blocker refuses it.
    page.engine().set_popup_blocker(true);
    assert_eq!(eval(page, "String(open('/plain.html'))"), "null");
    assert!(page.engine().take_open_requests().is_empty());
}

#[test]
fn a_popup_navigated_to_about_srcdoc_shows_an_error_page_of_an_opaque_origin() {
    let mut s = popup_sites();
    wait_loaded(&mut s.page);
    let page = &mut s.page;
    assert_eq!(
        eval(
            page,
            "window.w = open(); w.location = 'about:srcdoc'; w.origin === origin"
        ),
        "true"
    );
    wait(page, "isSecurityError(() => w.document)", "ok");
    assert_eq!(eval(page, "String(w.closed)"), "false");
}

#[test]
fn a_replaced_windows_named_objects_are_its_own_documents() {
    let mut s = sites(
        "<iframe id=x src='/named.html'></iframe>",
        vec![
            (
                "/named.html",
                "<!doctype html><script>function f() { return abc }</script><div id=abc></div>"
                    .into(),
            ),
            ("/span.html", "<!doctype html><span id=abc></span>".into()),
        ],
        vec![],
    );
    wait_loaded(&mut s.page);
    let page = &mut s.page;
    eval(
        page,
        "window.f = x.contentWindow.f; window.first = f(); x.src = '/span.html'; 0",
    );
    wait(
        page,
        "String(!!x.contentDocument.querySelector('span'))",
        "true",
    );
    assert_eq!(
        eval(page, "[f() === first, first.localName].join()"),
        "true,div"
    );
}
