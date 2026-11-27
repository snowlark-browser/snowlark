// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A hidden page (a background tab) loads as a shown one does: HTML §8.1.7.3 leaves its documents
//! out of "update the rendering", but fetching, parsing, scripts and the `load` event (HTML §13.2.7
//! "the end") do not wait on rendering opportunities.

use std::path::Path;
use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn hidden_page(net: &NetThread) -> Page {
    let page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.engine().set_shown(false);
    page
}

/// Loads `html` in a hidden page; returns how long its load took.
fn loads_while_hidden(page: &mut Page, html: &str) -> Duration {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../snowlark-render/tests/fixtures/");
    let url = url::Url::from_file_path(dir.join("hidden.html")).unwrap();
    page.load_html(html, &url);
    let start = Instant::now();
    while !page.load_complete() && start.elapsed() < Duration::from_secs(5) {
        page.run_for(Duration::from_millis(10));
    }
    assert!(page.load_complete(), "the load completes while hidden");
    start.elapsed()
}

#[test]
fn a_hidden_page_with_images_loads() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = hidden_page(&net);
    let took = loads_while_hidden(
        &mut page,
        "<!doctype html><script>var log = []; onload = () => log.push('window')</script>\
         <img src=anim-rgb.gif width=18 height=18 onload=\"log.push('img')\">\
         <img src=anim-rgb.gif height=1 width=0>",
    );
    assert!(took < Duration::from_secs(2), "{took:?}");
    // (the image's request delayed the load event: it completed first)
    assert_eq!(page.eval("log.join()", "t.js").unwrap(), "img,window");
    assert_eq!(page.eval("document.hidden", "t.js").unwrap(), "true");
}

#[test]
fn a_hidden_page_with_a_style_image_loads() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = hidden_page(&net);
    let took = loads_while_hidden(
        &mut page,
        "<!doctype html><div style='width: 10px; height: 10px; \
         background: url(anim-rgb.gif)'></div><img src=anim-rgb.gif>",
    );
    assert!(took < Duration::from_secs(2), "{took:?}");
}
