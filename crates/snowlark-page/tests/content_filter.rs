// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The content filter hook: cosmetic rules as a user sheet the CSSOM does not see, generic class
//! and id rules before the first paint, procedural filters after DOM changes, scriptlets before the
//! page's own scripts.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::content_filter::{
    DocumentFilters, ProceduralAction, ProceduralFilter, ProceduralStep, TextPattern,
};
use snowlark_page::{ContentFilter, Page, PageOptions, RenderSetup};

struct Stub;

impl ContentFilter for Stub {
    fn document_filters(&self, url: &url::Url, _top: &url::Url) -> Option<DocumentFilters> {
        if url.host_str() != Some("example.test") {
            return None;
        }
        Some(DocumentFilters {
            hide: vec![
                ".ad".into(),
                "div[data-ad]".into(),
                "::invalid-selector".into(),
            ],
            styles: vec![("#fixed".into(), "position: static !important".into())],
            procedural: vec![
                ProceduralFilter {
                    steps: vec![
                        ProceduralStep::Css(".post".into()),
                        ProceduralStep::HasText(TextPattern::parse("/sponsored/i").unwrap()),
                    ],
                    action: ProceduralAction::Hide,
                },
                ProceduralFilter {
                    steps: vec![
                        ProceduralStep::Css(".label".into()),
                        ProceduralStep::HasText(TextPattern::parse("Promoted").unwrap()),
                        ProceduralStep::UpwardCount(1),
                    ],
                    action: ProceduralAction::Remove,
                },
            ],
            script:
                "Object.defineProperty(window, 'scriptletRan', { value: document.readyState });"
                    .into(),
            generic: true,
            exceptions: HashSet::new(),
        })
    }

    fn generic_selectors(
        &self,
        _filters: &DocumentFilters,
        classes: &[String],
        ids: &[String],
    ) -> Vec<String> {
        let mut out = Vec::new();
        if classes.iter().any(|c| c == "banner") {
            out.push(".banner".into());
        }
        if ids.iter().any(|i| i == "sidebar-ad") {
            out.push("#sidebar-ad".into());
        }
        out
    }
}

fn page(net: &NetThread, filter: bool, url: &str, html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        content_filter: filter.then(|| Arc::new(Stub) as Arc<dyn ContentFilter>),
        ..Default::default()
    });
    page.load_html(html, &url::Url::parse(url).unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page.update_the_rendering();
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    page.eval(js, "test.js")
        .unwrap_or_else(|e| panic!("{js}: threw {e}"))
}

const HTML: &str = "<!doctype html><script>var seen = typeof scriptletRan === 'undefined' ? 'no' : scriptletRan;</script>\
    <div class=ad id=a>ad</div><div data-ad id=b>ad</div><div class=banner id=c>banner</div>\
    <div id=sidebar-ad>side</div><div id=fixed style='position:fixed'>f</div>\
    <div class=post id=p1>A Sponsored post</div><div class=post id=p2>A real post</div>\
    <div id=wrap><span class=label>Promoted</span></div><p id=keep>content</p>";

const DISPLAYS: &str = "['a','b','c','sidebar-ad','p1','p2','keep'].map(i => getComputedStyle(document.getElementById(i)).display).join()";

#[test]
fn cosmetic_filters_hide_without_showing_in_the_cssom() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, true, "https://example.test/", HTML);
    assert_eq!(
        eval(&mut p, DISPLAYS),
        "none,none,none,none,none,block,block"
    );
    assert_eq!(
        eval(
            &mut p,
            "getComputedStyle(document.getElementById('fixed')).position"
        ),
        "static"
    );
    // The procedural :remove() took the label's parent away.
    assert_eq!(
        eval(&mut p, "String(document.getElementById('wrap'))"),
        "null"
    );
    // The scriptlet ran before the page's first script, with the document loading.
    assert_eq!(eval(&mut p, "seen"), "loading");
    // Nothing in the CSSOM, no new global besides what the scriptlet itself defined.
    assert_eq!(eval(&mut p, "document.styleSheets.length"), "0");
    // An element added later is hidden at the next rendering update, a matching one hidden by
    // the procedural filter after its interval.
    eval(
        &mut p,
        "let n = document.createElement('div'); n.className = 'banner'; n.id = 'late'; \
         document.body.append(n); let q = document.createElement('div'); q.className = 'post'; \
         q.id = 'late-post'; q.textContent = 'sponsored by'; document.body.append(q); 1",
    );
    std::thread::sleep(Duration::from_millis(150));
    p.run_until_settled(Duration::from_secs(5));
    p.update_the_rendering();
    assert_eq!(
        eval(
            &mut p,
            "['late','late-post'].map(i => getComputedStyle(document.getElementById(i)).display).join()"
        ),
        "none,none"
    );
}

#[test]
fn nodes_removed_and_freed_in_one_batch() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, true, "https://example.test/", HTML);
    eval(
        &mut p,
        "const k = document.getElementById('keep'); for (let i = 0; i < 3; i++) {          k.innerHTML = '<span class=banner><b>x</b></span><i class=ad>y</i>'; }          document.body.innerHTML = '<div class=post id=np>sponsored</div>'; 1",
    );
    std::thread::sleep(Duration::from_millis(150));
    p.run_until_settled(Duration::from_secs(5));
    p.update_the_rendering();
    assert_eq!(
        eval(
            &mut p,
            "getComputedStyle(document.getElementById('np')).display"
        ),
        "none"
    );
}

#[test]
fn without_a_filter_or_for_other_hosts_nothing_changes() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    for (filter, url) in [
        (false, "https://example.test/"),
        (true, "https://other.test/"),
    ] {
        let mut p = page(&net, filter, url, HTML);
        assert_eq!(
            eval(&mut p, DISPLAYS),
            "block,block,block,block,block,block,block",
            "{url}"
        );
        assert_eq!(eval(&mut p, "seen"), "no");
        assert_eq!(
            eval(&mut p, "String(!!document.getElementById('wrap'))"),
            "true"
        );
        // No attribute was added anywhere.
        assert_eq!(
            eval(
                &mut p,
                "[...document.querySelectorAll('*')].some(e => [...e.attributes].some(a => a.name.startsWith('osm')))"
            ),
            "false"
        );
    }
}
