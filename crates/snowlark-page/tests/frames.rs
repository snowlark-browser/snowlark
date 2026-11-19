// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Child navigables rendered: frames' documents laid out in their containers' content boxes, painted
//! into the parent's display list, hit tested and given input, with same- and cross-origin documents
//! from in-process servers.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::{Color, Item, Point};

/// A tiny HTTP/1.1 server answering `/<name>` from `files` (path, content type, body).
fn serve(files: Vec<(String, &'static str, String)>) -> String {
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
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
            }
            let path = first.split(' ').nth(1).unwrap_or("/").to_owned();
            let response = match files.iter().find(|(p, _, _)| *p == path) {
                Some((_, mime, body)) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .into(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
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

/// The fill colors in a display list, through nested groups.
fn fills(items: &[Item]) -> Vec<Color> {
    items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { color, .. } => Some(*color),
            _ => None,
        })
        .collect()
}

/// HTML §2.4.1 the fallback base URL: an iframe `srcdoc` document's, and its initial
/// `about:blank` document's, is its about base URL, its parent's document base URL when it was
/// made (not live; a clone keeps it). A `base` whose URL is `data:` or `javascript:` is no frozen
/// base URL.
#[test]
fn frame_documents_without_urls_of_their_own_take_their_parents_base_url() {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><base href=/dir/><iframe id=s srcdoc='<a id=a href=x>x</a>'></iframe>\
         <iframe id=b></iframe>",
        &url::Url::parse("http://example.test/page").unwrap(),
    );
    wait(
        &mut page,
        "!!(s.contentDocument && s.contentDocument.getElementById('a'))",
        "true",
    );
    let got = eval(
        &mut page,
        "const d = b.contentDocument, base = d.createElement('base'); base.setAttribute('href', 'y'); \
         const before = [s.contentDocument.getElementById('a').href, base.href, d.baseURI]; \
         document.querySelector('base').href = 'data:/,x'; \
         before.concat([document.baseURI, s.contentDocument.baseURI, d.baseURI, \
         d.cloneNode(false).baseURI]).join(' ')",
    );
    assert_eq!(
        got,
        "http://example.test/dir/x http://example.test/dir/y http://example.test/dir/ \
         http://example.test/page http://example.test/dir/ http://example.test/dir/ \
         http://example.test/dir/"
    );
}

#[test]
fn frames_render_nest_and_take_input() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    // Another origin (another port) for the cross-origin frame.
    let other = serve(vec![(
        "/x.html".into(),
        "text/html",
        "<!doctype html><body style='margin:0;background:rgb(0,0,250)'>cross\
         <script>window.ran = 1</script>"
            .into(),
    )]);
    let base = serve(vec![
        (
            "/top.html".into(),
            "text/html",
            format!(
                "<!doctype html><body style='margin:0'>\
                 <iframe id=a src='/a.html' style='border:0;width:200px;height:100px;display:block'></iframe>\
                 <iframe id=x src='{other}/x.html' style='border:5px solid;width:100px;height:50px;display:block'></iframe>\
                 <iframe id=s srcdoc=\"<body style='margin:0;background:rgb(0,250,0)'>s\" \
                  style='border:0;width:50px;height:50px;display:block'></iframe>\
                 <script>window.loads = []; for (const f of document.querySelectorAll('iframe')) \
                  f.onload = () => loads.push(f.id + ':' + (f.id == 'x' ? 'x' : f.contentDocument.readyState));</script>"
            ),
        ),
        (
            "/a.html".into(),
            "text/html",
            "<!doctype html><body style='margin:0;background:rgb(250,0,0)'>\
             <div id=d style='height:400px'>a</div>\
             <iframe id=n srcdoc=\"<body style='background:rgb(250,250,0)'>\" style='border:0;width:20px;height:20px'></iframe>\
             <script>window.clicks = []; document.addEventListener('click', e => clicks.push(e.target.id + '@' + e.clientX + ',' + e.clientY));</script>"
                .into(),
        ),
    ]);
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/top.html")).unwrap());
    wait(&mut page, "loads.length", "3");
    // Each frame's load event fired after its document finished loading.
    let loads = eval(&mut page, "loads.slice().sort().join()");
    assert_eq!(loads, "a:complete,s:complete,x:x");
    // The frames' viewports are their containers' content boxes.
    assert_eq!(
        eval(
            &mut page,
            "const a = document.getElementById('a').contentWindow; [a.innerWidth, a.innerHeight].join()"
        ),
        "200,100"
    );
    // Cross-origin content renders but its realm stays apart.
    assert_eq!(
        eval(
            &mut page,
            "String(document.getElementById('x').contentDocument)"
        ),
        "null"
    );
    page.update_the_rendering();
    let list = page
        .engine()
        .paint(Point::ZERO, 1.0)
        .expect("a display list");
    assert!(list.validate().is_ok());
    let colors = fills(&list.items);
    for c in [
        Color::rgb(250, 0, 0),   // the same-origin frame
        Color::rgb(0, 0, 250),   // the cross-origin frame
        Color::rgb(0, 250, 0),   // the srcdoc frame
        Color::rgb(250, 250, 0), // the frame nested in the first
    ] {
        assert!(colors.contains(&c), "{c:?} painted: {colors:?}");
    }
    // A click inside the first frame goes to its document, at the point in its viewport.
    let none = Modifiers::default();
    page.pointer_pressed(Point::new(30.0, 40.0), 0, none, 1);
    page.pointer_released(Point::new(30.0, 40.0), 0, none, 1);
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('a').contentWindow.clicks.join()"
        ),
        "d@30,40"
    );
    // The frame took the focus: its document's active element is the parent's.
    assert_eq!(eval(&mut page, "document.activeElement.id"), "a");
    // Scrolling the frame's viewport through its container.
    let a = {
        let e = page.engine().clone();
        let dom = e.dom();
        let doc = e.document().unwrap();
        dom.descendants(doc)
            .find(|&n| dom.is_element(n) && dom.get_attribute(n, "id") == Some("a"))
            .unwrap()
    };
    let scrollers = page.engine().frame_scrollers();
    assert!(
        scrollers.iter().any(|s| s.0 == a && s.3.y > 0.0),
        "the frame scrolls: {scrollers:?}"
    );
    assert!(page.engine().scroll_container_by(a, Point::new(0.0, 60.0)));
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('a').contentWindow.scrollY"
        ),
        "60"
    );
    // A click lands where the frame's content now is.
    page.pointer_pressed(Point::new(10.0, 10.0), 0, none, 1);
    page.pointer_released(Point::new(10.0, 10.0), 0, none, 1);
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('a').contentWindow.clicks[1]"
        ),
        "d@10,10"
    );
    // Removing a frame stops rendering its document.
    eval(&mut page, "document.getElementById('s').remove()");
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    assert!(!fills(&list.items).contains(&Color::rgb(0, 250, 0)));
}

#[test]
fn framesets_lay_out_their_frames_as_a_grid() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let base = serve(vec![
        (
            "/fs.html".into(),
            "text/html",
            "<!doctype html><frameset cols='100,*' rows='50%,*'>\
             <frame id=f1 src='/c.html' frameborder=0><frame id=f2 src='/c.html' frameborder=0>\
             <frame id=f3 src='/c.html' frameborder=0><frame id=f4 src='/c.html' frameborder=0></frameset>"
                .into(),
        ),
        (
            "/c.html".into(),
            "text/html",
            "<body style='margin:0;background:rgb(1,2,3)'>c".into(),
        ),
    ]);
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/fs.html")).unwrap());
    wait(
        &mut page,
        "document.querySelectorAll('frame').length == 4 && [...document.querySelectorAll('frame')].every(f => f.contentDocument && f.contentDocument.body) + ''",
        "true",
    );
    let r = eval(
        &mut page,
        "[...document.querySelectorAll('frame')].map(f => { const r = f.getBoundingClientRect(); \
         return [r.x, r.y, r.width, r.height].join(' ') }).join()",
    );
    assert_eq!(r, "0 0 100 300,100 0 700 300,0 300 100 300,100 300 700 300");
    wait(
        &mut page,
        "document.getElementById('f2').contentWindow.innerWidth + ''",
        "700",
    );
}

#[test]
fn objects_without_a_document_show_their_fallback() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><object id=p type=application/x-shockwave-flash data=x.swf width=600 height=600>Fallback \
         <span id=f>content</span></object><object id=d data='data:text/html,x' width=120 height=40></object>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    page.run_until_settled(Duration::from_secs(5));
    let r = eval(
        &mut page,
        "const p = document.getElementById('p').getBoundingClientRect(); \
         const d = document.getElementById('d').getBoundingClientRect(); \
         [p.width < 300, p.height < 300, document.getElementById('f').getClientRects().length, \
          d.width, d.height, !!document.getElementById('d').contentWindow].join()",
    );
    assert_eq!(r, "true,true,1,120,40,true");
}

/// A PDF file of pages (width, height in points), the first with a red square (as in
/// `snowlark-paint`'s `pdf` tests).
fn small_pdf(pages: &[(u32, u32)]) -> String {
    let n = pages.len();
    let mut objects: Vec<String> = vec!["<< /Type /Catalog /Pages 2 0 R >>".into()];
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + 2 * i)).collect();
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {n} >>",
        kids.join(" ")
    ));
    for (i, &(w, h)) in pages.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents {} 0 R /Resources << >> >>",
            4 + 2 * i
        ));
        let content = if i == 0 {
            format!("1 0 0 rg 72 {} 72 72 re f", h - 144)
        } else {
            String::new()
        };
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len() + 1
        ));
    }
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
    }
    let xref = out.len();
    out.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    ));
    for off in offsets {
        out.push_str(&format!("{off:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    out
}

#[test]
fn pdfs_show_in_the_pdf_viewer_in_frames_objects_and_at_the_top() {
    // HTML §7.4.6 and §4.8.6/§4.8.7 with "PDF viewer supported": a PDF navigation makes a document
    // of the file's pages (images of the PDF renderer's output), in an iframe, in an `object` or
    // `embed` of a PDF type (a nested document, not the fallback), and at the top level.
    let pdf = small_pdf(&[(612, 792), (595, 420)]);
    let base = serve(vec![
        ("/doc%20one.pdf".into(), "application/pdf", pdf),
        (
            "/top.html".into(),
            "text/html",
            "<!doctype html><iframe id=i src='doc%20one.pdf' width=900 height=700></iframe>\
             <object id=o type=application/pdf data='doc%20one.pdf' width=600 height=400>fallback \
             <span id=f>x</span></object>\
             <embed id=e type=application/pdf src='doc%20one.pdf' width=300 height=200>             <iframe id=s sandbox=allow-same-origin src=sandboxed.html width=700 height=500></iframe>"
                .into(),
        ),
        ("/bad.pdf".into(), "application/pdf", "not a pdf".into()),
        (
            "/sandboxed.html".into(),
            "text/html",
            "<!doctype html><object id=o type=application/pdf data='doc%20one.pdf' width=600              height=400>fallback <span id=f>x</span></object>"
                .into(),
        ),
    ]);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (1000, 800),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/top.html")).unwrap());
    let shown = "(w => !!w && !!w.document && w.document.images.length == 2 \
                 && [...w.document.images].every(i => i.complete && i.naturalWidth > 0))";
    wait(
        &mut page,
        &format!(
            "(document.getElementById('e') && frames.length == 4 \n             && document.getElementById('s').contentDocument.readyState == 'complete' && ['i', 'o'].every(id => {shown}(document.getElementById(id).contentWindow))) + ''"
        ),
        "true",
    );
    let r = eval(
        &mut page,
        "const d = document.getElementById('i').contentDocument; const img = d.images; \
         [d.contentType, d.title, img[0].alt, img[0].width, img[0].naturalWidth, \
          img[0].naturalHeight, img[1].alt, img[1].naturalWidth, img[0].src.startsWith('blob:'), \
          document.getElementById('f').getClientRects().length, \
          Math.round(document.getElementById('o').getBoundingClientRect().width)].join()",
    );
    assert_eq!(
        r,
        "application/pdf,doc one.pdf,Page 1,816,1632,2112,Page 2,1587,true,0,600"
    );
    // HTML §4.8.6 (whatwg/html#6946): a sandboxed document's PDF object shows its fallback content.
    let r = eval(
        &mut page,
        "const s = document.getElementById('s').contentDocument;          const o = s.getElementById('o'); const rect = o.getBoundingClientRect();          [rect.width < 300, rect.height < 300, s.getElementById('f').getClientRects().length,           o.contentDocument === null].join()",
    );
    assert_eq!(r, "true,true,1,true");
    // At the top level.
    page.navigate(url::Url::parse(&format!("{base}/doc%20one.pdf")).unwrap());
    wait(&mut page, &format!("{shown}(window) + ''"), "true");
    assert_eq!(
        eval(
            &mut page,
            "[document.contentType, document.images.length].join()"
        ),
        "application/pdf,2"
    );
    // Not a PDF after all: the viewer says it cannot show it.
    page.navigate(url::Url::parse(&format!("{base}/bad.pdf")).unwrap());
    wait(
        &mut page,
        "(!!document.body && document.body.textContent.startsWith('This PDF cannot be shown')) + ''",
        "true",
    );
}

/// A server that accepts connections and never answers: what is fetched from it stays loading.
fn hang() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    format!("http://{addr}")
}

/// HTML "process the iframe attributes" with `loading=lazy`: the navigation of an iframe below the
/// viewport waits until the iframe comes near it, and a lazy loaded iframe's navigation does not
/// delay its document's load event ("current navigation was lazy loaded").
#[test]
fn lazy_iframes_wait_for_the_viewport_and_do_not_delay_the_load_event() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let never = hang();
    let base = serve(vec![
        (
            "/top.html".into(),
            "text/html",
            "<!doctype html><body style='margin:0'>\
             <iframe id=near loading=lazy src='/slow.html' style='width:100px;height:50px'></iframe>\
             <div style='height:20000px'></div>\
             <iframe id=far loading=lazy src='/far.html' style='width:100px;height:50px'></iframe>\
             <script>window.order = []; addEventListener('load', () => order.push('window'));\
              for (const f of document.querySelectorAll('iframe')) f.onload = () => order.push(f.id);</script>"
                .into(),
        ),
        (
            "/slow.html".into(),
            "text/html",
            format!("<!doctype html><img src='{never}/i.png'>"),
        ),
        ("/far.html".into(), "text/html", "<!doctype html>far".into()),
    ]);
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/top.html")).unwrap());
    // The near frame's document is still loading (its image never arrives), yet the window loads.
    wait(&mut page, "order.join()", "window");
    wait(
        &mut page,
        "document.getElementById('near').contentWindow.location.pathname",
        "/slow.html",
    );
    // The far frame did not start loading.
    page.run_for(Duration::from_millis(300));
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('far').contentWindow.location.href"
        ),
        "about:blank"
    );
    // Scrolled near, it loads.
    eval(&mut page, "document.getElementById('far').scrollIntoView()");
    wait(&mut page, "order.includes('far') + ''", "true");
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('far').contentDocument.body.textContent"
        ),
        "far"
    );
}

/// HTML "navigate" ends a lazy loaded iframe's wait when it is called, before its fragment
/// navigation step: a `location.replace()` before a new `src` leaves the `src` to load once the
/// iframe is near the viewport, and a link to a fragment of the initial `about:blank` keeps it.
/// `location.reload()` is not a navigation and leaves the wait. A resumed navigation fires
/// `beforeunload` at the document it replaces, and not before it resumes.
#[test]
fn navigations_and_reloads_of_a_lazy_iframe_before_it_loads() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let page_ = |q: &str| {
        (
            format!("/x.html?{q}"),
            "text/html",
            format!("<!doctype html>{q}"),
        )
    };
    let base = serve(vec![
        (
            "/top.html".into(),
            "text/html",
            "<!doctype html><body style='margin:0'>\
             <iframe id=a loading=lazy hidden></iframe>\
             <iframe id=b loading=lazy hidden src='/x.html?src'></iframe>\
             <iframe id=c name=c loading=lazy hidden src='/x.html?src'></iframe>\
             <iframe id=d loading=lazy src='/r.html?first'></iframe>\
             <a id=link href='about:blank#fragment' target=c></a>\
             <script>window.bu = []; onmessage = e => bu.push(e.data);\
              const [a, b, c, d] = document.querySelectorAll('iframe');\
              a.contentWindow.location.replace('/x.html?nav'); a.src = '/x.html?src';\
              b.contentWindow.location.reload();\
              link.click();\
              for (const f of [a, b, c]) f.hidden = false;</script>"
                .into(),
        ),
        page_("src"),
        page_("nav"),
        (
            "/r.html?first".into(),
            "text/html",
            "<!doctype html><script>onbeforeunload = () => parent.postMessage('first', '*')</script>"
                .into(),
        ),
        ("/r.html?second".into(), "text/html", "second".into()),
    ]);
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/top.html")).unwrap());
    wait(
        &mut page,
        "(d.contentDocument.readyState == 'complete' && d.contentWindow.location.search) + ''",
        "?first",
    );
    page.run_for(Duration::from_millis(500));
    let src = format!("{base}/x.html?src");
    assert_eq!(
        eval(
            &mut page,
            "[a, b, c].map(f => f.contentWindow.location.href).join()"
        ),
        format!("{src},{src},about:blank#fragment")
    );
    // A new `src` while hidden waits, without unloading the first document.
    eval(&mut page, "d.hidden = true; d.src = '/r.html?second'");
    page.run_for(Duration::from_millis(300));
    assert_eq!(
        eval(
            &mut page,
            "[bu.join(), d.contentWindow.location.search].join()"
        ),
        ",?first"
    );
    eval(&mut page, "d.hidden = false");
    wait(&mut page, "d.contentWindow.location.search", "?second");
    wait(&mut page, "bu.join()", "first");
}

/// HTML §4.8.7: an `object` whose `data` does not parse or whose load fails (an HTTP error) fires
/// `error`, not `load`, and represents its fallback content, with no navigable; a type it cannot
/// show falls back without an event, and the `object` elements of that fallback content load. An
/// `object` inside one that shows its resource loads nothing; a `javascript:` URL is a load that
/// failed.
#[test]
fn objects_fall_back_when_their_resource_fails_or_cannot_be_shown() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let base = serve(vec![
        (
            "/top.html".into(),
            "text/html",
            "<!doctype html><body><script>window.log = [];\
              const on = (o) => { o.onload = () => log.push(o.id + ' load');\
                o.onerror = () => log.push(o.id + ' error'); };</script>\
             <object id=a type=text/html data='/missing.html'><button id=fa>fallback</button></object>\
             <object id=b data='http://test:test'></object>\
             <object id=c data='data:application/x-does-not-exist,test'>\
               <object id=c1 data='/inner.html'></object></object>\
             <object id=d data='/inner.html'><object id=d1 data='/inner.html'></object></object>\
             <object id=e data='javascript:1'></object>\
             <script>for (const o of document.querySelectorAll('object')) on(o);</script>"
                .into(),
        ),
        ("/inner.html".into(), "text/html", "<!doctype html>inner".into()),
    ]);
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/top.html")).unwrap());
    wait(
        &mut page,
        "(typeof log == 'object' && document.readyState)",
        "complete",
    );
    page.run_for(Duration::from_millis(300));
    let r = eval(
        &mut page,
        "const o = id => document.getElementById(id);
         [log.sort().join(), o('a').contentWindow, o('b').contentWindow, o('c').contentWindow,
          !!o('c1').contentWindow, !!o('d').contentWindow, o('d1').contentWindow, frames.length,
          o('fa').getBoundingClientRect().width > 0].join('|')",
    );
    assert_eq!(
        r,
        "a error,b error,c1 load,d load,e error||||true|true||2|true"
    );
}

/// HTML "navigate to a `javascript:` URL" with initialInsertion: an iframe inserted with a
/// `javascript:` `src` whose result is not a string fires one `load`, for its initial
/// `about:blank` document; a string result fires one, for the document it makes.
#[test]
fn an_inserted_iframe_with_a_javascript_src_fires_one_load() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let base = serve(vec![(
        "/top.html".into(),
        "text/html",
        "<!doctype html><body><script>window.seen = [];\
         for (const [id, src] of [['u', 'javascript:void(0)'], ['n', 'javascript:1'], ['s', 'javascript:\"foo\"']]) {\
           const f = document.createElement('iframe'); f.id = id; f.src = src;\
           f.onload = () => seen.push(id + ':' + f.contentDocument.URL + ':' + f.contentDocument.body.textContent);\
           document.body.append(f); }</script>"
            .into(),
    )]);
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/top.html")).unwrap());
    wait(&mut page, "seen.length", "3");
    page.run_for(Duration::from_millis(200));
    assert_eq!(
        eval(&mut page, "seen.slice().sort().join()"),
        "n:about:blank:,s:about:blank:foo,u:about:blank:"
    );
}

/// A navigation that begins before a queued `javascript:` URL navigation runs cancels it: an
/// iframe inserted with `src=javascript:false` and at once targeted by a form fires one `load`,
/// for the form's response.
#[test]
fn a_navigation_cancels_a_queued_javascript_url() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let base = serve(vec![
        (
            "/top.html".into(),
            "text/html",
            "<!doctype html><body><script>window.seen = [];\
             const f = document.createElement('iframe'); f.name = 'myframe';\
             f.onload = () => seen.push(f.contentWindow.location.pathname);\
             const form = document.createElement('form'); form.target = 'myframe';\
             form.action = '/formaction.html'; document.body.append(form);\
             f.src = 'javascript:false'; document.body.append(f); form.submit();</script>"
                .into(),
        ),
        (
            "/formaction.html".into(),
            "text/html",
            "<!doctype html>ok".into(),
        ),
    ]);
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/top.html")).unwrap());
    wait(&mut page, "seen.length > 0", "true");
    page.run_for(Duration::from_millis(200));
    assert_eq!(eval(&mut page, "seen.join()"), "/formaction.html");
}

/// HTML "checking if unloading is canceled" and "unload a document and its descendants": a frame
/// navigated by its `src` fires `beforeunload` at once, and `pagehide` and `unload` (its own child
/// frame's first) when the new document comes.
#[test]
fn a_navigated_frame_unloads_its_documents() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let base = serve(vec![
        (
            "/top.html".into(),
            "text/html",
            "<!doctype html><body><iframe id=f src='/a.html'></iframe>\
             <script>window.seen = [];</script>"
                .into(),
        ),
        (
            "/a.html".into(),
            "text/html",
            "<!doctype html><body><iframe id=c srcdoc='c'></iframe><script>\
             for (const t of ['beforeunload', 'pagehide', 'unload'])\
               addEventListener(t, e => parent.seen.push('a:' + t + ':' + (e.target === document)));\
             document.getElementById('c').onload = () => {\
               for (const t of ['beforeunload', 'pagehide', 'unload'])\
                 c.contentWindow.addEventListener(t, () => parent.seen.push('c:' + t)); parent.ready = 1; };\
             </script>"
                .into(),
        ),
        ("/b.html".into(), "text/html", "<!doctype html>b".into()),
    ]);
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/top.html")).unwrap());
    wait(&mut page, "window.ready + ''", "1");
    page.run_for(Duration::from_millis(100));
    eval(&mut page, "f.src = '/b.html'; window.at_once = seen.join()");
    assert_eq!(
        eval(&mut page, "at_once"),
        "a:beforeunload:true,c:beforeunload"
    );
    wait(
        &mut page,
        "f.contentDocument && f.contentDocument.body && f.contentDocument.body.textContent",
        "b",
    );
    assert_eq!(
        eval(&mut page, "seen.slice(2).join()"),
        "c:pagehide,c:unload,a:pagehide:true,a:unload:true"
    );
}

/// HTML §4.8.7: an `object` (re)determines what it represents after each `data` or `type`
/// change, not only for its first fetch. A later `data` that fails to load fires `error` and
/// shows the fallback content, whatever document it showed; a change to an image type that cannot
/// be rendered destroys its child navigable in a queued task (its document gets `pagehide` first),
/// then fires `error` and shows the fallback content.
#[test]
fn an_object_falls_back_when_a_later_resource_fails() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let base = serve(vec![
        (
            "/top.html".into(),
            "text/html",
            "<!doctype html><body>\
             <object id=d data='/inner.html'><button id=fd>fallback</button></object>\
             <object id=g data='/inner.html'><button id=fg>fallback</button></object>"
                .into(),
        ),
        (
            "/inner.html".into(),
            "text/html",
            "<!doctype html>inner".into(),
        ),
    ]);
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/top.html")).unwrap());
    wait(&mut page, "!!document.getElementById('g')", "true");
    wait(&mut page, "document.readyState", "complete");
    page.run_for(Duration::from_millis(200));
    let r = eval(
        &mut page,
        "window.log = []; const o = id => document.getElementById(id);
         for (const id of ['d', 'g']) {
           o(id).onload = () => log.push(id + ' load');
           o(id).onerror = () => log.push(id + ' error');
         }
         o('g').contentWindow.addEventListener('pagehide', () => log.push('g pagehide'));
         o('d').data = '/missing.html';
         o('g').data = '/missing.png';
         [!!o('d').contentWindow, !!o('g').contentWindow, log.length].join('|')",
    );
    // (nothing happens before the queued tasks and the fetches)
    assert_eq!(r, "true|true|0");
    wait(&mut page, "log.length", "3");
    assert_eq!(
        eval(
            &mut page,
            "[log.filter(e => e[0] == 'g').join(), log.filter(e => e[0] == 'd').join(),
              o('d').contentWindow, o('g').contentWindow,
              o('fd').getBoundingClientRect().width > 0,
              o('fg').getBoundingClientRect().width > 0].join('|')"
        ),
        "g pagehide,g error|d error|||true|true"
    );
}

/// An `object` or `embed` showing an SVG document has the natural dimensions of its root `svg`
/// element (its `width` and `height`, or one of them and its `viewBox`'s ratio), and the 300x150
/// default once its navigable shows another document. An `embed` that had no `src` gets a
/// navigable when one is set. Its scrolling area is its box's, as for any atomic inline.
#[test]
fn objects_and_embeds_take_their_svg_documents_size() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body><embed id=e>\
         <object id=o data='data:image/svg+xml,<svg xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 2 1\" width=\"80\"></svg>'></object>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    page.run_for(Duration::from_millis(200));
    let size = "var r = id => { const b = document.getElementById(id).getBoundingClientRect(); \
                return b.width + 'x' + b.height; }; ";
    wait(
        &mut page,
        &format!("{size} r('e').split('x')[0] + '|' + r('o')"),
        "0|80x40",
    );
    eval(
        &mut page,
        "window.loads = 0; const e = document.getElementById('e'); e.onload = () => loads++;
         e.src = 'data:image/svg+xml,<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" \
         height=\"50\"></svg>'; 'ok'",
    );
    wait(&mut page, "loads", "1");
    assert_eq!(
        eval(&mut page, &format!("{size} r('e') + '|' + e.scrollWidth")),
        "100x50|100"
    );
    eval(&mut page, "e.src = 'about:blank'; 'ok'");
    wait(&mut page, "loads", "2");
    assert_eq!(eval(&mut page, &format!("{size} r('e')")), "300x150");
}

/// HTML §4.8.5 "shared attribute processing steps": an iframe's `src` is encoding-parsed relative
/// to its node document, so the query takes the document's encoding.
#[test]
fn an_iframes_src_query_takes_the_documents_encoding() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let base = serve(vec![
        (
            "/top.html".into(),
            "text/html; charset=windows-1252",
            "<!doctype html><body><iframe id=f></iframe>".into(),
        ),
        (
            "/inner.html".into(),
            "text/html",
            "<!doctype html>inner".into(),
        ),
    ]);
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/top.html")).unwrap());
    wait(&mut page, "!!document.getElementById('f')", "true");
    eval(
        &mut page,
        "document.getElementById('f').src = '/inner.html?\\u00df'; 'ok'",
    );
    wait(
        &mut page,
        "document.getElementById('f').contentWindow.location.search",
        "?%DF",
    );
}

/// HTML "create a new child navigable" needs a fully active parent: an iframe moved into its own
/// child document leaves that navigable destroyed and gets none there.
#[test]
fn an_iframe_appended_to_its_own_child_document_has_no_navigable() {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><body><iframe id=x></iframe>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    page.run_for(Duration::from_millis(50));
    assert_eq!(
        eval(
            &mut page,
            "(() => { const f = document.getElementById('x'); const w = f.contentWindow; \
             const before = w.parent === window; w.document.body.appendChild(f); \
             return [before, w.parent, f.contentWindow, w.document.body.firstChild === f, \
             frames.length].join('|'); })()"
        ),
        "true|||true|0"
    );
}

/// Permissions Policy "the declared origin": an iframe's `allow` entry without origins (`'src'`)
/// matches the document its `src` brings, also a `data:` URL's, whose origin is opaque; without
/// the entry the feature is `'self'` only, which that document is not.
#[test]
fn an_allow_attribute_enables_a_feature_for_a_data_url_document() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body><script>window.log = [];\
         addEventListener('message', e => log.push(e.data)); \
         const doc = n => 'data:text/html,<script>parent.postMessage(\"' + n + ':\" + \
         document.fullscreenEnabled, \"*\")</scr' + 'ipt>';</script>\
         <script>for (const [n, allow] of [['a', 'fullscreen'], ['b', null], ['c', 'payment']]) {\
         const f = document.createElement('iframe'); if (allow) f.allow = allow; \
         f.src = doc(n); document.body.append(f); }</script>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    wait(&mut page, "log.length", "3");
    assert_eq!(
        eval(&mut page, "log.sort().join()"),
        "a:true,b:false,c:false"
    );
}
