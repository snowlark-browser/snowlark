// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a press tells the embedder (middle-button autoscroll starts from it): whether the page
//! canceled the `mousedown`, and whether the press is on a link, a form control or an editable
//! element.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

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

const MIDDLE: u16 = 1;

fn middle_press(page: &mut Page, x: f32, y: f32) -> snowlark_page::input::PointerOutcome {
    let none = Modifiers::default();
    let o = page.pointer_pressed(Point::new(x, y), MIDDLE, none, 1);
    page.pointer_released(Point::new(x, y), MIDDLE, none, 1);
    o
}

#[test]
fn a_middle_press_says_whether_it_was_canceled_and_whether_it_is_on_something_else() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0;font:10px monospace'>
         <div id=plain style='height:50px'>plain</div>
         <div id=cancel style='height:50px'>cancel</div>
         <a id=link href='http://example.test/x' style='display:block;height:50px'>link</a>
         <input id=field style='display:block;width:200px;height:30px;box-sizing:border-box'>
         <div id=edit contenteditable style='height:50px'>edit</div>
         <div id=editfalse contenteditable=false style='height:50px'>no</div>
         <button id=btn style='display:block;height:30px;width:100px'>button</button>
         <script>
           document.getElementById('cancel').addEventListener('mousedown', e => e.preventDefault());
           window.log = [];
           for (const t of ['mousedown', 'mouseup', 'auxclick'])
             document.getElementById('plain').addEventListener(t, e => log.push(t + e.button));
         </script>",
    );
    // A plain element: autoscroll may start, and the page still saw the whole press.
    let o = middle_press(&mut page, 20.0, 20.0);
    assert!(!o.press_canceled && !o.autoscroll_blocked, "{o:?}");
    assert_eq!(
        page.eval("log.join()", "t").unwrap(),
        "mousedown1,mouseup1,auxclick1",
        "mousedown, mouseup and auxclick reach the page"
    );
    // The page canceled the mousedown.
    let o = middle_press(&mut page, 20.0, 70.0);
    assert!(o.press_canceled, "{o:?}");
    // A link, a field, an editable element, a button.
    let o = middle_press(&mut page, 20.0, 120.0);
    assert!(o.autoscroll_blocked && !o.press_canceled, "link {o:?}");
    let o = middle_press(&mut page, 20.0, 165.0);
    assert!(o.autoscroll_blocked, "field {o:?}");
    let o = middle_press(&mut page, 20.0, 200.0);
    assert!(o.autoscroll_blocked, "contenteditable {o:?}");
    let o = middle_press(&mut page, 20.0, 250.0);
    assert!(!o.autoscroll_blocked, "contenteditable=false {o:?}");
    let o = middle_press(&mut page, 20.0, 295.0);
    assert!(o.autoscroll_blocked, "button {o:?}");
    // Other input leaves the flags alone.
    let o = page.pointer_moved(Point::new(20.0, 20.0), Modifiers::default());
    assert!(!o.press_canceled && !o.autoscroll_blocked);
}
