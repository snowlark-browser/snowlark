// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS UI 4 §5.1.1 `cursor`: "`auto` behaves as text over selectable text or editable elements, and
//! default otherwise".

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

/// One row of 40 px per entry of `rows`, each 40 px tall and as wide as the viewport.
fn page(net: &NetThread, rows: &[&str]) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    let body: String = rows
        .iter()
        .map(|r| format!("<div class=row>{r}</div>"))
        .collect();
    page.load_html(
        &format!(
            "<!doctype html><style>
               body {{ margin: 0; font: 20px/40px sans-serif }}
               .row {{ height: 40px; overflow: hidden }}
               .row > * {{ margin: 0; border: 0; padding: 0; height: 40px }}
             </style>{body}"
        ),
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    page.update_the_rendering();
    page
}

fn over(page: &mut Page, x: f32, row: usize) -> String {
    let at = Point::new(x, row as f32 * 40.0 + 20.0);
    page.pointer_moved(at, Modifiers::default()).cursor
}

#[test]
fn auto_is_text_over_text_and_default_beside_it() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, &["<p>some text</p>", "<div></div>"]);
    assert_eq!(over(&mut p, 10.0, 0), "text");
    // (the same block, past the end of its text)
    assert_eq!(over(&mut p, 700.0, 0), "default");
    assert_eq!(over(&mut p, 10.0, 1), "default");
}

#[test]
fn text_that_cannot_be_selected_is_not_text() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        &[
        "<p style='user-select: none'>none</p>",
        "<div style='user-select: none'><span>inside none</span></div>",
        "<div style='user-select: none'><span style='user-select: text'>text again</span></div>",
        "<button>label</button>",
        "<button><span>inside a label</span></button>",
    ],
    );
    assert_eq!(over(&mut p, 10.0, 0), "default");
    assert_eq!(over(&mut p, 10.0, 1), "default");
    assert_eq!(over(&mut p, 10.0, 2), "text");
    assert_eq!(over(&mut p, 10.0, 3), "default");
    assert_eq!(over(&mut p, 10.0, 4), "default");
}

#[test]
fn editable_elements_are_text_where_they_hold_no_text() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        &[
            "<div contenteditable></div>",
            "<div contenteditable><b>bold</b></div>",
            "<input style='width: 300px'>",
        ],
    );
    assert_eq!(over(&mut p, 700.0, 0), "text");
    assert_eq!(over(&mut p, 700.0, 1), "text");
    assert_eq!(over(&mut p, 10.0, 2), "text");
}

#[test]
fn links_and_the_cursor_property_come_before_text() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        &[
            "<a href='/x'>a link</a>",
            "<p style='cursor: crosshair'>crosshair</p>",
            "<p style='cursor: default'>default</p>",
        ],
    );
    assert_eq!(over(&mut p, 10.0, 0), "pointer");
    assert_eq!(over(&mut p, 10.0, 1), "crosshair");
    assert_eq!(over(&mut p, 10.0, 2), "default");
}

/// A press and a release answer for where they are, as a move does.
#[test]
fn a_selection_drag_keeps_the_text_cursor_until_it_ends() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, &["<p>some text</p>"]);
    let none = Modifiers::default();
    let (text, beside) = (Point::new(10.0, 20.0), Point::new(700.0, 20.0));
    p.pointer_moved(text, none);
    let down = p.pointer_pressed(text, 0, none, 1);
    assert_eq!(down.cursor, "text");
    assert_eq!(p.pointer_moved(beside, none).cursor, "text");
    let up = p.pointer_released(beside, 0, none, 1);
    assert_eq!(up.cursor, "default");
    assert_eq!(p.pointer_moved(beside, none).cursor, "default");
}
