// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Contain 2 `content-visibility: hidden` and CSS Sizing 4 5.2.1 the last remembered size of
//! `contain-intrinsic-size: auto`.

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};
use std::time::Duration;

fn page_with(html: &str) -> Page {
    static NET: std::sync::OnceLock<NetThread> = std::sync::OnceLock::new();
    let net = NET.get_or_init(|| NetThread::start(NetConfig::default()).unwrap());
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
    page.eval(js, "t.js")
        .unwrap_or_else(|e| panic!("{js}: {e}"))
}

const PAGE: &str = "<!doctype html><style>\
    #target{width:max-content;height:max-content}\
    .cis-auto{contain-intrinsic-size:auto 1px auto 2px}\
    .skip{content-visibility:hidden}\
    .s100{width:100px;height:50px}.s75{width:75px;height:25px}\
    </style><div id=target><div id=contents></div></div>";

fn size(page: &mut Page) -> String {
    eval(page, "target.clientWidth + 'x' + target.clientHeight")
}

#[test]
fn hidden_contents_take_the_intrinsic_size_not_the_contents_size() {
    let mut page = page_with(PAGE);
    eval(
        &mut page,
        "target.className = 'cis-auto skip'; contents.className = 's100'; ''",
    );
    // size containment with no last remembered size: the length after `auto`
    assert_eq!(size(&mut page), "1x2");
    eval(&mut page, "target.className = 'cis-auto'; ''");
    assert_eq!(size(&mut page), "100x50");
}

#[test]
fn the_last_remembered_size_is_recorded_when_the_rendering_updates() {
    let mut page = page_with(PAGE);
    eval(
        &mut page,
        "target.className = 'cis-auto'; contents.className = 's100'; ''",
    );
    assert_eq!(size(&mut page), "100x50");
    // not remembered until a rendering update
    eval(&mut page, "target.classList.add('skip'); ''");
    assert_eq!(size(&mut page), "1x2");
    eval(&mut page, "target.classList.remove('skip'); ''");
    assert_eq!(size(&mut page), "100x50");
    assert!(page.update_the_rendering());
    eval(&mut page, "target.classList.add('skip'); ''");
    assert_eq!(size(&mut page), "100x50", "uses the last remembered size");
    // the contents change while skipped: still the remembered size; sized normally: the new one
    eval(&mut page, "contents.className = 's75'; ''");
    assert_eq!(size(&mut page), "100x50");
    eval(&mut page, "target.classList.remove('skip'); ''");
    assert_eq!(size(&mut page), "75x25");
    // going back, no update in between: the old remembered size
    eval(&mut page, "target.classList.add('skip'); ''");
    assert_eq!(size(&mut page), "100x50");
    eval(&mut page, "target.classList.remove('skip'); ''");
    assert!(page.update_the_rendering());
    eval(&mut page, "target.classList.add('skip'); ''");
    assert_eq!(size(&mut page), "75x25", "the new last remembered size");
    // an element that loses `auto` forgets it
    eval(&mut page, "target.className = 'skip'; ''");
    assert!(page.update_the_rendering());
    eval(&mut page, "target.className = 'cis-auto skip'; ''");
    assert_eq!(size(&mut page), "1x2");
}

#[test]
fn losing_auto_forgets_the_remembered_size_at_once() {
    let mut page = page_with(PAGE);
    eval(
        &mut page,
        "target.className = 'cis-auto'; contents.className = 's100'; ''",
    );
    assert_eq!(size(&mut page), "100x50");
    assert!(page.update_the_rendering());
    // no `auto` any more (and a layout in between): forgotten without a rendering update
    eval(&mut page, "target.className = 'skip'; ''");
    assert_eq!(size(&mut page), "0x0");
    eval(&mut page, "target.className = 'cis-auto skip'; ''");
    assert_eq!(size(&mut page), "1x2");
}

#[test]
fn the_remembered_size_is_logical() {
    let mut page = page_with(PAGE);
    eval(
        &mut page,
        "target.className = 'cis-auto'; contents.className = 's100'; ''",
    );
    assert!(page.update_the_rendering());
    // the inline size was 100 and the block size 50: in a vertical writing mode width is the
    // block size
    eval(
        &mut page,
        "target.style.writingMode = 'vertical-lr'; target.classList.add('skip'); ''",
    );
    assert_eq!(size(&mut page), "50x100");
}
