// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML "the popover attribute": showing and hiding with `beforetoggle` and `toggle`, the auto
//! popover stack (an auto popover hides the others unless nested in them), `:popover-open`,
//! `popovertarget` invokers, light dismiss by a click outside, and Esc.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

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
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

fn click(page: &mut Page, x: f32, y: f32) {
    let none = Modifiers::default();
    let at = Point::new(x, y);
    page.pointer_moved(at, none);
    page.pointer_pressed(at, 0, none, 1);
    page.pointer_released(at, 0, none, 1);
    page.run_until_settled(Duration::from_secs(2));
}

#[test]
fn show_and_hide_fire_toggle_events_and_match_popover_open() {
    let mut page = page_with(
        "<!doctype html><div id=p popover>P</div><script>var log = [];\
         p.addEventListener('beforetoggle', e => log.push('before ' + e.newState));\
         p.addEventListener('toggle', e => log.push('toggle ' + e.newState));</script>",
    );
    eval(
        &mut page,
        "p.showPopover(); log.push('open ' + p.matches(':popover-open'))",
    );
    page.run_until_settled(Duration::from_secs(2));
    eval(
        &mut page,
        "p.hidePopover(); log.push('open ' + p.matches(':popover-open'))",
    );
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(
        eval(&mut page, "log.join()"),
        "before open,open true,toggle open,before closed,open false,toggle closed"
    );
}

#[test]
fn an_auto_popover_hides_the_others_unless_nested_in_them() {
    let mut page = page_with(
        "<!doctype html><div id=a popover><div id=b popover></div></div><div id=c popover></div>\
         <div id=m popover=manual></div>",
    );
    let open = "[a, b, c, m].map(e => e.matches(':popover-open') ? 1 : 0).join('')";
    eval(
        &mut page,
        "m.showPopover(); a.showPopover(); b.showPopover()",
    );
    assert_eq!(eval(&mut page, open), "1101");
    eval(&mut page, "c.showPopover()");
    assert_eq!(eval(&mut page, open), "0011");
}

#[test]
fn an_invoker_is_the_source_of_closing_and_a_reset_button_in_a_form_resets_only() {
    let mut page = page_with(
        "<!doctype html><button id=b popovertarget=p>b</button><div id=p popover>P</div>\
         <form><input id=r type=reset popovertarget=p></form><script>var log = [];\
         p.addEventListener('beforetoggle', e => log.push(e.newState + ' ' + (e.source && e.source.id)));\
         </script>",
    );
    eval(&mut page, "b.click(); b.click(); r.click()");
    assert_eq!(eval(&mut page, "log.join()"), "open b,closed b");
}

#[test]
fn focus_does_not_return_to_an_element_the_hidden_popover_hides() {
    let mut page = page_with(
        "<!doctype html><button id=prior>prior</button><div id=p popover autofocus tabindex=0>P</div>",
    );
    eval(
        &mut page,
        "prior.focus(); p.showPopover(); p.appendChild(prior); p.hidePopover()",
    );
    assert_ne!(eval(&mut page, "document.activeElement.id"), "prior");
}

#[test]
fn a_dialog_shown_hides_popovers_and_a_removed_auto_popover_takes_hints_with_it() {
    let mut page = page_with(
        "<!doctype html><div id=a popover></div><div id=h popover=hint></div><dialog id=d></dialog>",
    );
    let open = "[a, h].map(e => e.matches(':popover-open') ? 1 : 0).join('')";
    eval(&mut page, "a.showPopover(); d.show()");
    assert_eq!(eval(&mut page, open), "00");
    eval(
        &mut page,
        "d.close(); a.showPopover(); a.appendChild(h); h.showPopover()",
    );
    assert_eq!(eval(&mut page, open), "11");
    eval(&mut page, "window.hint = h; a.remove()");
    assert_eq!(eval(&mut page, "hint.matches(':popover-open')"), "false");
}

#[test]
fn tab_moves_from_an_invoker_into_its_popover() {
    let mut page = page_with(
        "<!doctype html><button id=inv popovertarget=p>open</button><button id=next>next</button>\
         <div id=p popover><button id=inside>inside</button></div>",
    );
    let tab = snowlark_page::keyboard::KeyInput {
        vk: 0x09,
        scan: 0x0F,
        text: None,
        mods: Modifiers::default(),
        repeat: false,
        paste: None,
        paste_html: None,
    };
    eval(&mut page, "inv.focus(); inv.click()");
    page.run_until_settled(Duration::from_secs(2));
    page.key_down(&tab);
    page.key_up(&tab);
    assert_eq!(eval(&mut page, "document.activeElement.id"), "inside");
    page.key_down(&tab);
    page.key_up(&tab);
    assert_eq!(eval(&mut page, "document.activeElement.id"), "next");
}

#[test]
fn an_invoker_toggles_its_popover_and_a_click_outside_or_esc_hides_it() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0} button{position:absolute;left:0;top:0;width:100px;\
         height:30px} #p{margin:0;inset:auto;left:300px;top:300px;width:100px;height:100px}</style>\
         <button popovertarget=p>open</button><div id=p popover>P</div>",
    );
    let open = "p.matches(':popover-open')";
    click(&mut page, 50.0, 15.0);
    assert_eq!(eval(&mut page, open), "true");
    // (a click inside the popover keeps it open)
    click(&mut page, 350.0, 350.0);
    assert_eq!(eval(&mut page, open), "true");
    click(&mut page, 600.0, 100.0);
    assert_eq!(eval(&mut page, open), "false");
    click(&mut page, 50.0, 15.0);
    assert_eq!(eval(&mut page, open), "true");
    let esc = snowlark_page::keyboard::KeyInput {
        vk: 0x1B,
        scan: 0x01,
        text: None,
        mods: Modifiers::default(),
        repeat: false,
        paste: None,
        paste_html: None,
    };
    page.key_down(&esc);
    page.key_up(&esc);
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(eval(&mut page, open), "false");
}
