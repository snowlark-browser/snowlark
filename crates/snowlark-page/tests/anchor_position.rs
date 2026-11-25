// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Anchor Positioning 1 in layout: `anchor()` and `anchor-size()` against the target anchor
//! element, their fallbacks, `position-area`, `anchor-center`, and a popover's implicit anchor
//! (its invoker).

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn page_with(html: &str) -> (NetThread, Page) {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (400, 300),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(html, &url::Url::parse("http://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    (net, page)
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// `x y w h` of an element's border box.
const RECT: &str =
    "const rect = id => { const r = document.getElementById(id).getBoundingClientRect(); \
                    return [r.x, r.y, r.width, r.height].join(' '); };";

const STYLE: &str = "<!doctype html><style>
    body { margin: 0 }
    #cb { position: relative; width: 300px; height: 200px }
    #a { position: absolute; left: 50px; top: 40px; width: 60px; height: 20px; anchor-name: --a }
    .t { position: absolute; width: 10px; height: 10px }
</style>";

#[test]
fn anchor_functions_place_and_size_the_box() {
    let (_net, mut page) = page_with(&format!(
        "{STYLE}<div id=cb><div id=a></div>
         <div id=t1 class=t style='left: anchor(--a right); top: anchor(--a bottom)'></div>
         <div id=t2 class=t style='right: anchor(--a left); bottom: anchor(--a top)'></div>
         <div id=t3 class=t style='left: anchor(--a 50%); top: 0; width: anchor-size(--a width)'></div>
         <div id=t4 class=t style='left: calc(anchor(--a left) + 5px); top: anchor(--nope top, 7px)'></div>
         </div>"
    ));
    let r = eval(
        &mut page,
        &format!("{RECT} ['t1', 't2', 't3', 't4'].map(rect).join(', ')"),
    );
    assert_eq!(
        r, "110 60 10 10, 40 30 10 10, 80 0 60 10, 55 7 10 10",
        "anchor() insets, anchor-size(), a percentage, calc() and a fallback"
    );
}

#[test]
fn position_area_is_the_containing_block() {
    let (_net, mut page) = page_with(&format!(
        "{STYLE}<div id=cb><div id=a></div>
         <div id=t1 class=t style='position-anchor: --a; position-area: bottom'></div>
         <div id=t2 class=t style='position-anchor: --a; position-area: top left'></div>
         <div id=t3 class=t style='position-anchor: --a; position-area: span-right center'></div>
         </div>"
    ));
    let r = eval(
        &mut page,
        &format!("{RECT} ['t1', 't2', 't3'].map(rect).join(', ')"),
    );
    // below and centered on the anchor; above-left, against it; from the anchor's left edge
    // rightward, vertically centered on it
    assert_eq!(r, "75 60 10 10, 40 30 10 10, 50 45 10 10");
}

#[test]
fn anchor_center_centers_on_the_anchor() {
    let (_net, mut page) = page_with(&format!(
        "{STYLE}<div id=cb><div id=a></div>
         <div id=t1 class=t style='position-anchor: --a; justify-self: anchor-center;
              top: anchor(bottom)'></div>
         </div>"
    ));
    let r = eval(&mut page, &format!("{RECT} rect('t1')"));
    assert_eq!(r, "75 60 10 10");
}

#[test]
fn an_anchor_later_in_tree_order_and_positioned_is_not_acceptable() {
    // §2.3: an absolutely positioned anchor after the box in tree order is laid out after it.
    let (_net, mut page) = page_with(&format!(
        "{STYLE}<div id=cb>
         <div id=t1 class=t style='left: anchor(--b left, 3px); top: 0'></div>
         <div id=b style='position: absolute; left: 90px; top: 0; width: 5px; height: 5px;
              anchor-name: --b'></div>
         <div id=t2 class=t style='left: anchor(--b left, 3px); top: 0'></div>
         </div>"
    ));
    let r = eval(
        &mut page,
        &format!("{RECT} ['t1', 't2'].map(rect).join(', ')"),
    );
    assert_eq!(r, "3 0 10 10, 90 0 10 10");
}

#[test]
fn the_first_position_option_that_fits_is_used() {
    // §6.5.2: the box overflows its containing block to the right; `--left` fits; the option's
    // rule changing (and not the box's own style) changes where it goes.
    let (_net, mut page) = page_with(
        "<!doctype html><style>
            body { margin: 0 }
            #cb { position: relative; width: 300px; height: 200px }
            #a { position: absolute; left: 200px; top: 40px; width: 60px; height: 20px;
                 anchor-name: --a }
            #t { position: absolute; width: 80px; height: 10px; position-anchor: --a;
                 left: anchor(right); top: anchor(top);
                 position-try-fallbacks: --nowhere, --left }
         </style>
         <style id=rules>@position-try --left { left: auto; right: anchor(left) }</style>
         <div id=cb><div id=a></div><div id=t></div></div>",
    );
    let r = eval(
        &mut page,
        &format!(
            "{RECT} const before = rect('t');
             document.getElementById('rules').textContent =
                 '@position-try --left {{ left: auto; right: anchor(left); margin-right: 10px }}';
             [before, rect('t')].join(', ')"
        ),
    );
    assert_eq!(r, "120 40 80 10, 110 40 80 10");
}

#[test]
fn a_popover_is_anchored_to_its_invoker() {
    let (_net, mut page) = page_with(
        "<!doctype html><style>
            body { margin: 0 }
            button { position: absolute; left: 100px; top: 50px; width: 40px; height: 20px;
                     margin: 0; padding: 0; border: 0 }
            [popover] { margin: 0; padding: 0; border: 0; width: 30px; height: 10px;
                        position-anchor: auto; top: anchor(bottom); left: anchor(left) }
         </style>
         <button id=b popovertarget=p>open</button><div id=p popover></div>",
    );
    let r = eval(
        &mut page,
        &format!(
            "{RECT} document.getElementById('p').showPopover({{ source: document.getElementById('b') }});
             rect('p')"
        ),
    );
    assert_eq!(r, "100 70 30 10");
}
