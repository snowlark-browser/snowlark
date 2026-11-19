// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Find in page over a rendered page: matching across inline elements and shadow trees, case, text
//! that is not rendered, wrapping round, zero matches, scrolling the active match into view (the
//! viewport, scroll containers, frames), the highlights painted from layout, no trace in the page
//! (no nodes, mutations or selection while searching), and a new document.

use std::time::Duration;

use snowlark_layout::find::{ACTIVE_COLOR, MATCH_COLOR};
use snowlark_net::{NetConfig, NetThread};
use snowlark_page::find::{FindSession, FindStatus};
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::{Item, Point};

fn rendered(net: &NetThread, html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
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

/// Searches for `query` to the end.
fn search(page: &mut Page, s: &mut FindSession, query: &str) -> FindStatus {
    s.set_query(page.engine(), query);
    for _ in 0..1000 {
        let w = s.work(page.engine(), true, Duration::from_millis(50));
        if !w.busy {
            break;
        }
    }
    s.status()
}

/// The (yellow, orange) highlights painted at the top of the page.
fn painted(page: &Page) -> (usize, usize) {
    let list = page.engine().paint(Point::ZERO, 1.0).expect("a list");
    let count = |c| {
        list.items
            .iter()
            .filter(|i| matches!(i, Item::FillRect { color, .. } if *color == c))
            .count()
    };
    (count(MATCH_COLOR), count(ACTIVE_COLOR))
}

#[test]
fn matches_across_inline_elements_ignoring_case_and_what_is_not_rendered() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><style>.x{display:none}</style>\
         <p id=p>The quick <b>BROWN</b> fox, the Quick brown dog</p>\
         <p>one<br>two</p><p class=x>quick hidden</p><p style='visibility:hidden'>quick ghost</p>\
         <script>var quick = 'quick in a script';</script>\
         <div id=h></div><p>qu<i>ick</i> split</p>",
    );
    let mut s = FindSession::new();
    let st = search(&mut page, &mut s, "quick");
    // The paragraph twice, and the one split by <i>; not the display:none, hidden or script text.
    assert_eq!((st.total, st.current, st.searching), (3, Some(0), false));
    assert_eq!(painted(&page), (3, 1));
    let st = search(&mut page, &mut s, "k BROWN f");
    assert_eq!(st.total, 1);
    assert_eq!(search(&mut page, &mut s, "ghost").total, 0);
    assert_eq!(search(&mut page, &mut s, "hidden").total, 0);
    assert_eq!(search(&mut page, &mut s, "in a script").total, 0);
    // (the empty query finds nothing)
    assert_eq!(search(&mut page, &mut s, "").total, 0);
    assert_eq!(painted(&page), (0, 0));
}

#[test]
fn next_and_previous_wrap_round_and_zero_matches_stay_zero() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<body><p>ab ab ab</p>");
    let mut s = FindSession::new();
    assert_eq!(search(&mut page, &mut s, "ab").total, 3);
    let order = |s: &mut FindSession, page: &Page, forward: bool| {
        s.step(page.engine(), forward);
        s.status().current
    };
    assert_eq!(order(&mut s, &page, true), Some(1));
    assert_eq!(order(&mut s, &page, true), Some(2));
    assert_eq!(order(&mut s, &page, true), Some(0));
    assert_eq!(order(&mut s, &page, false), Some(2));
    // The active match is the one painted orange.
    assert_eq!(painted(&page), (2, 1));
    let none = search(&mut page, &mut s, "zzz");
    assert_eq!((none.total, none.current), (0, None));
    s.step(page.engine(), true);
    assert_eq!(s.status().current, None);
}

#[test]
fn typing_on_keeps_the_active_match_in_place() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<body><p>cat catalog cattle cat</p>");
    let mut s = FindSession::new();
    search(&mut page, &mut s, "c");
    s.step(page.engine(), true);
    s.step(page.engine(), true);
    // (the third "c": the one in "cattle")
    assert_eq!(s.status().current, Some(2));
    let st = search(&mut page, &mut s, "cat");
    assert_eq!((st.total, st.current), (4, Some(2)));
    let st = search(&mut page, &mut s, "catt");
    assert_eq!((st.total, st.current), (1, Some(0)));
}

#[test]
fn the_active_match_scrolls_into_view_through_containers_and_frames() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <p>top needle</p><div style='height:3000px'></div>\
         <div id=box style='height:100px;overflow:auto;width:300px'>\
           <div style='height:1500px'></div><p>inner needle</p><div style='height:1500px'></div></div>\
         <div style='height:2000px'></div><p>last needle</p>\
         <iframe style='width:300px;height:100px;border:0' \
            srcdoc=\"<div style='height:900px'></div><p>frame needle</p>\"></iframe>",
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    let mut s = FindSession::new();
    let st = search(&mut page, &mut s, "needle");
    assert_eq!(st.total, 4);
    // The first is in view: nothing scrolls.
    assert_eq!(eval(&mut page, "scrollY"), "0");
    s.step(page.engine(), true);
    page.run_for(Duration::from_millis(50));
    // The second is inside the scroll container, which is far below: both scroll.
    let y = eval(&mut page, "scrollY").parse::<f32>().unwrap();
    let top = eval(&mut page, "document.getElementById('box').scrollTop")
        .parse::<f32>()
        .unwrap();
    assert!(y > 2000.0, "viewport scrolled to the container: {y}");
    assert!(top > 1000.0, "container scrolled to the text: {top}");
    let r = eval(
        &mut page,
        "[...document.querySelectorAll('p')].map(p => { const r = p.getBoundingClientRect(); return p.textContent + ':' + r.top + ':' + r.bottom; }).join(' ') + ' ' + innerHeight + ' ' + scrollY + ' box ' + JSON.stringify(document.getElementById('box').getBoundingClientRect()) + ' st ' + document.getElementById('box').scrollTop",
    );
    assert!(
        r.contains("inner needle:") && {
            let rest = r.split("inner needle:").nth(1).unwrap();
            let top: f32 = rest.split(':').next().unwrap().parse().unwrap();
            (200.0..400.0).contains(&top)
        },
        "the match is on screen: {r}"
    );
    s.step(page.engine(), true);
    let y3 = eval(&mut page, "scrollY").parse::<f32>().unwrap();
    assert!(y3 > y, "scrolled further to the last: {y3} > {y}");
    // The fourth is in the frame, below its fold: the frame scrolls and the page brings it in view.
    s.step(page.engine(), true);
    let fy = eval(
        &mut page,
        "document.querySelector('iframe').contentWindow.scrollY",
    )
    .parse::<f32>()
    .unwrap();
    assert!(fy > 500.0, "the frame scrolled: {fy}");
    // Previous goes back, wrapping round.
    s.step(page.engine(), false);
    s.step(page.engine(), false);
    s.step(page.engine(), false);
    s.step(page.engine(), false);
    assert_eq!(s.status().current, Some(3));
}

#[test]
fn open_shadow_trees_and_frames_are_searched() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body><div id=h></div><iframe srcdoc='<p>word in a frame</p>'></iframe>\
         <script>document.getElementById('h').attachShadow({mode:'open'}).innerHTML = '<p>word in a <b>shadow</b></p>';</script>\
         <p>word in the page</p>",
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    let mut s = FindSession::new();
    assert_eq!(search(&mut page, &mut s, "word in a").total, 2);
    assert_eq!(search(&mut page, &mut s, "word").total, 3);
    assert_eq!(search(&mut page, &mut s, "a shadow").total, 1);
}

#[test]
fn searching_leaves_no_trace_in_the_page() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body><p id=p>find <b>me</b> please, find me</p>\
         <script>
           window.records = 0; window.events = [];
           new MutationObserver(r => records += r.length).observe(document, {subtree: true, childList: true, attributes: true, characterData: true});
           for (const t of ['selectionchange', 'focus', 'blur', 'scroll']) document.addEventListener(t, () => events.push(t), true);
           window.html = document.documentElement.outerHTML;
           window.globalsBefore = Object.getOwnPropertyNames(window).concat(['globalsBefore']);
         </script>",
    );
    let mut s = FindSession::new();
    search(&mut page, &mut s, "find me");
    s.step(page.engine(), true);
    page.run_until_settled(Duration::from_secs(2));
    let r = eval(
        &mut page,
        "[records, events.join(), getSelection().toString() === '', getSelection().rangeCount,
          Object.getOwnPropertyNames(window).filter(n => !globalsBefore.includes(n)).join(),
          document.documentElement.outerHTML === html].join('|')",
    );
    assert_eq!(r, "0||true|0||true");
    // Closing with a match active selects it (the user's act, as in Chrome).
    s.close(page.engine(), true);
    assert_eq!(painted(&page), (0, 0));
    let r = eval(&mut page, "getSelection().toString()");
    assert_eq!(r, "find me");
    // Closing without keeps the selection as it was.
    let mut s2 = FindSession::new();
    eval(&mut page, "getSelection().removeAllRanges()");
    search(&mut page, &mut s2, "please");
    s2.close(page.engine(), false);
    assert_eq!(eval(&mut page, "getSelection().toString()"), "");
    // The selected text starts the next search.
    eval(
        &mut page,
        "const r = document.createRange(); r.selectNodeContents(document.getElementById('p').firstChild); getSelection().removeAllRanges(); getSelection().addRange(r);",
    );
    assert_eq!(page.engine().find_selected_text().as_deref(), Some("find"));
}

#[test]
fn a_new_document_clears_and_searches_again() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<body><p>needle one needle</p>");
    let mut s = FindSession::new();
    assert_eq!(search(&mut page, &mut s, "needle").total, 2);
    page.load_html(
        "<body><p>needle</p><p>needle</p><p>needle</p>",
        &url::Url::parse("http://example.test/second").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    // (while the new document loads nothing of the old one is kept)
    let w = s.work(page.engine(), false, Duration::from_millis(50));
    assert!(!w.busy);
    assert_eq!(s.status().total, 0);
    for _ in 0..20 {
        s.work(page.engine(), true, Duration::from_millis(50));
    }
    assert_eq!(s.status().total, 3);
    assert_eq!(painted(&page).0 + painted(&page).1, 3);
}

#[test]
fn a_long_document_is_searched_in_slices() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut html = String::from("<!doctype html><body>");
    for i in 0..400 {
        html.push_str(&format!(
            "<p>line {i} lorem ipsum dolor sit amet needle consectetur adipiscing elit</p>"
        ));
    }
    let page = rendered(&net, &html);
    let mut s = FindSession::new();
    s.set_query(page.engine(), "needle");
    // A zero budget: each call checks the clock after a stretch of text and gives up, so the
    // search takes several calls and never more than a slice at a time.
    let mut calls = 0;
    loop {
        let w = s.work(page.engine(), true, Duration::ZERO);
        calls += 1;
        if !w.busy {
            break;
        }
        assert!(calls < 10_000);
    }
    assert!(calls > 1, "searched in {calls} slices");
    assert_eq!(s.status().total, 400);
    assert!(!s.status().searching);
}
