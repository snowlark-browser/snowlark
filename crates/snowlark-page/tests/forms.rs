// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.10.21.3 form submission: the entry list sent as a GET query or a POST body
//! (`application/x-www-form-urlencoded`, `multipart/form-data`, `text/plain`), submitter overrides,
//! cancellation, and constraint validation, against an in-process server that echoes the request.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions};

/// A server answering every request with a page whose title is "METHOD /path?query" and whose
/// `#body` holds the request body and `#type` its content type.
fn echo_server(form: &'static str) -> String {
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
            let mut content_type = String::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
                let lower = h.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                if lower.starts_with("content-type:") {
                    content_type = h["content-type:".len()..].trim().to_owned();
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let mut parts = first.split(' ');
            let method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("/");
            let page = if target == "/form" {
                form.to_owned()
            } else {
                let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;");
                format!(
                    "<!doctype html><title>{} {}</title><pre id=body>{}</pre><p id=type>{}</p>",
                    method,
                    esc(target),
                    esc(&String::from_utf8_lossy(&body)),
                    esc(&content_type)
                )
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn load(net: &NetThread, base: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/form")).unwrap());
    page.run_until(std::time::Instant::now() + Duration::from_secs(5), |p| {
        p.load_complete()
    });
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    page.eval(js, "test.js")
        .unwrap_or_else(|e| panic!("{js}: {e}"))
}

/// Runs the page until its title starts with `prefix` (the navigation arrived).
fn wait_for_title(page: &mut Page, prefix: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut title = String::new();
    page.run_until(deadline, |p| {
        title = p.eval("document.title", "t.js").unwrap_or_default();
        title.starts_with(prefix)
    });
    title
}

const FORM: &str = "<!doctype html><form id=f action=/submit>\
    <input name=q value='a b&c'><input type=checkbox name=c checked><input type=checkbox name=u>\
    <select name=s><option>one<option selected>two</select><textarea name=t>x\ny</textarea>\
    <button name=b value=go id=btn>Go</button><input type=submit name=other value=no>\
    <input name=req id=req></form>";

#[test]
fn get_submission_navigates_with_the_query() {
    let base = echo_server(FORM);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = load(&net, &base);
    eval(&mut page, "document.getElementById('btn').click()");
    let title = wait_for_title(&mut page, "GET");
    assert_eq!(
        title,
        "GET /submit?q=a+b%26c&c=on&s=two&t=x%0D%0Ay&b=go&req="
    );
}

#[test]
fn post_bodies_by_enctype() {
    let base = echo_server(FORM);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = load(&net, &base);
    eval(
        &mut page,
        "const f = document.getElementById('f'); f.method = 'post'; f.requestSubmit()",
    );
    assert_eq!(wait_for_title(&mut page, "POST"), "POST /submit");
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('body').textContent + '|' + document.getElementById('type').textContent"
        ),
        "q=a+b%26c&c=on&s=two&t=x%0D%0Ay&req=|application/x-www-form-urlencoded"
    );
    let mut page = load(&net, &base);
    eval(
        &mut page,
        "const f = document.getElementById('f'); f.method = 'post'; f.enctype = 'multipart/form-data';
         f.innerHTML = '<input name=n value=v>'; f.submit()",
    );
    assert_eq!(wait_for_title(&mut page, "POST"), "POST /submit");
    let r = eval(
        &mut page,
        "document.getElementById('body').textContent.includes('Content-Disposition: form-data; name=\"n\"\\n\\nv') + '|' +
         document.getElementById('type').textContent.startsWith('multipart/form-data; boundary=')",
    );
    assert_eq!(r, "true|true");
}

/// HTML §4.10.21.3 "pick an encoding for the form": the first `accept-charset` label that names an
/// encoding, else the document's; the query is in that encoding, code points it lacks are character
/// references, and a hidden `_charset_` field submits the encoding's name.
#[test]
fn a_form_submits_in_its_accept_charset_encoding() {
    const LEGACY: &str = "<!doctype html><meta charset=utf-8>\
        <form id=f action=/submit accept-charset='bogus windows-1252 utf-8'>\
        <input name=q value='\u{e9} \u{3b1}'><input type=hidden name=_charset_></form>\
        <form id=g action=/submit><input name=q value='\u{e9}'></form>";
    let base = echo_server(LEGACY);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = load(&net, &base);
    eval(&mut page, "document.getElementById('f').submit()");
    assert_eq!(
        wait_for_title(&mut page, "GET"),
        "GET /submit?q=%E9+%26%23945%3B&_charset_=windows-1252"
    );
    // (no accept-charset: the document's encoding, UTF-8 here)
    let mut page = load(&net, &base);
    eval(&mut page, "document.getElementById('g').submit()");
    assert_eq!(wait_for_title(&mut page, "GET"), "GET /submit?q=%C3%A9");
}

/// HTML §4.10.21.3: a submission through the submit button replaces the navigation the form's
/// `submit()` planned in its click handler, even into another frame; `submit()` calls alone each
/// keep theirs.
#[test]
fn a_submit_buttons_submission_replaces_the_forms_planned_navigation() {
    const FRAMES: &str = "<!doctype html><iframe name=f1></iframe><iframe name=f2></iframe>\
        <iframe name=f3></iframe><iframe name=f4></iframe>\
        <form id=f target=f1 action=/submit><input name=n value=1><button id=b>go</button></form>\
        <form id=g action=/other><input name=m value=2></form>";
    let base = echo_server(FRAMES);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = load(&net, &base);
    eval(
        &mut page,
        "f.addEventListener('click', () => { f.submit(); f.target = 'f2'; }); b.click(); \
         g.target = 'f3'; g.submit(); g.target = 'f4'; g.submit();",
    );
    let titles = "[0, 1, 2, 3].map(i => frames[i].document.title).join('|')";
    let mut seen = String::new();
    let mut loaded = |page: &mut Page, n: usize, wait: Duration| {
        page.run_until(std::time::Instant::now() + wait, |p| {
            seen = p.eval(titles, "t.js").unwrap_or_default();
            seen.split('|').filter(|t| t.starts_with("GET")).count() == n
        });
    };
    loaded(&mut page, 3, Duration::from_secs(5));
    // (f1 stays: the button's submission removed the navigation f's `submit()` planned there)
    loaded(&mut page, 4, Duration::from_millis(500));
    assert_eq!(seen, "|GET /submit?n=1|GET /other?m=2|GET /other?m=2");
}

/// HTML §4.10.21.4 "constructing the entry list": a bubbling, non-cancelable `formdata` event
/// carries the FormData being built; what its listeners add is submitted; the entry list keeps the
/// values' newlines (FormData sees "x\ny"), which only the encodings normalize.
#[test]
fn the_formdata_event_shapes_the_submission() {
    let base = echo_server(FORM);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = load(&net, &base);
    eval(
        &mut page,
        "window.seen = ''; document.addEventListener('formdata', e => { \
           seen = [e.bubbles, e.cancelable, e instanceof FormDataEvent, \
                   JSON.stringify(e.formData.get('t'))].join(); \
           e.formData.append('extra', 'a\\nb'); }); \
         const f = document.getElementById('f'); \
         window.raw = JSON.stringify(new FormData(f).get('t')); \
         f.method = 'post'; f.requestSubmit()",
    );
    assert_eq!(wait_for_title(&mut page, "POST"), "POST /submit");
    assert_eq!(
        eval(&mut page, "document.getElementById('body').textContent"),
        "q=a+b%26c&c=on&s=two&t=x%0D%0Ay&req=&extra=a%0D%0Ab"
    );
}

/// HTML §4.10.21.4: a file field contributes its selected files (here given by script through a
/// DataTransfer, HTML §6.11.3) to the entry list, and an empty File named "" when it has none.
#[test]
fn file_fields_submit_their_files() {
    let base = echo_server(FORM);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = load(&net, &base);
    eval(
        &mut page,
        "const f = document.getElementById('f'); f.method = 'post'; f.enctype = 'multipart/form-data'; \
         f.innerHTML = '<input type=file name=up id=up><input type=file name=none>'; \
         const dt = new DataTransfer(); \
         dt.items.add(new File(['hello'], 'a.txt', { type: 'text/plain' })); \
         window.meta = [dt.items.length, dt.items[0].kind, dt.types.join(), dt.files.length].join(); \
         document.getElementById('up').files = dt.files; f.submit()",
    );
    assert_eq!(wait_for_title(&mut page, "POST"), "POST /submit");
    let r = eval(
        &mut page,
        "const b = document.getElementById('body').textContent; \
         [b.includes('name=\"up\"; filename=\"a.txt\"\\nContent-Type: text/plain\\n\\nhello'), \
          b.includes('name=\"none\"; filename=\"\"\\nContent-Type: application/octet-stream')].join()",
    );
    assert_eq!(r, "true,true");
}

#[test]
fn the_formdata_event_fires_for_the_formdata_constructor() {
    let base = echo_server(FORM);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = load(&net, &base);
    let r = eval(
        &mut page,
        "let seen = ''; document.addEventListener('formdata', e => { \
           seen = [e.bubbles, e.cancelable, e instanceof FormDataEvent].join(); \
           e.formData.append('extra', '1'); }); \
         const d = new FormData(document.getElementById('f')); \
         [seen, JSON.stringify(d.get('t')), d.get('extra')].join('|')",
    );
    assert_eq!(r, "true,false,true|\"x\\ny\"|1");
}

#[test]
fn cancelled_and_invalid_submissions_stay() {
    let base = echo_server(FORM);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = load(&net, &base);
    let r = eval(
        &mut page,
        "const f = document.getElementById('f'); window.log = [];
         f.addEventListener('submit', e => { log.push('submit ' + (e.submitter && e.submitter.id)); e.preventDefault(); });
         document.getElementById('btn').click();
         document.getElementById('req').required = true;
         document.getElementById('req').addEventListener('invalid', () => log.push('invalid'));
         f.requestSubmit();
         log.join()",
    );
    assert_eq!(r, "submit btn,invalid");
    page.run_until_settled(Duration::from_secs(1));
    assert_eq!(eval(&mut page, "location.pathname"), "/form");
}
