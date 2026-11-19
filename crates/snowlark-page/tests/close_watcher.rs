// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML close watchers (Esc as a close request, groups by user activation, a `cancel` that can only
//! prevent the close after an activation), the dialog `closedby` attribute and dialog light
//! dismiss, the dialog's toggle event sources, and the popover stacks' hint rules (the hint stack
//! parent; no popover shown while one is being hidden).

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::keyboard::KeyInput;
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

fn esc(page: &mut Page) {
    let esc = KeyInput {
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
}

/// Dialogs at 300,300 (100 by 100), a button at the top left that shows nothing.
const DIALOGS: &str = "<!doctype html><style>body{margin:0} dialog{position:fixed;inset:auto;\
    left:300px;top:300px;width:100px;height:100px;margin:0;padding:0;border:0}\
    button{position:absolute;left:0;top:0;width:50px;height:30px}</style>\
    <button id=b>b</button><dialog id=any closedby=any>any</dialog>\
    <dialog id=req closedby=closerequest>req</dialog><dialog id=none closedby=none>none</dialog>\
    <dialog id=auto>auto</dialog>";

#[test]
fn closed_by_reflects_the_computed_state() {
    let mut page = page_with(DIALOGS);
    let r = eval(
        &mut page,
        "const s = () => [any, req, none, auto].map(d => d.closedBy).join(',');
         const a = s(); auto.showModal(); const b = auto.closedBy; auto.close();
         auto.setAttribute('closedby', 'bogus'); const c = auto.closedBy;
         auto.showModal(); const d = auto.closedBy; auto.close();
         [a, b, c, d].join('|')",
    );
    assert_eq!(
        r,
        "any,closerequest,none,none|closerequest|none|closerequest"
    );
}

#[test]
fn a_closedby_any_dialog_is_light_dismissed_by_a_click_outside_it() {
    let mut page = page_with(DIALOGS);
    eval(
        &mut page,
        "window.log = []; for (const d of [any, req])
           for (const t of ['cancel', 'close']) d.addEventListener(t, e => log.push(d.id + ':' + t));
         any.show(); 'ok'",
    );
    // (a click inside keeps it open; outside, its close watcher is asked to close it)
    click(&mut page, 350.0, 350.0);
    assert_eq!(eval(&mut page, "any.open + ''"), "true");
    click(&mut page, 600.0, 100.0);
    assert_eq!(
        eval(&mut page, "[any.open, log.join(',')].join('|')"),
        "false|any:cancel,any:close"
    );
    // A closedby=closerequest dialog is not light dismissed; a modal closedby=any dialog is, by a
    // click on its backdrop.
    eval(&mut page, "log = []; req.show(); 'ok'");
    click(&mut page, 600.0, 100.0);
    assert_eq!(eval(&mut page, "req.open + ''"), "true");
    eval(&mut page, "req.close(); any.showModal(); 'ok'");
    click(&mut page, 350.0, 350.0);
    assert_eq!(eval(&mut page, "any.open + ''"), "true");
    click(&mut page, 600.0, 100.0);
    assert_eq!(eval(&mut page, "any.open + ''"), "false");
    // (a canceled `cancel` keeps it open)
    eval(
        &mut page,
        "any.addEventListener('cancel', e => e.preventDefault(), {once: true}); any.show(); 'ok'",
    );
    click(&mut page, 600.0, 100.0);
    assert_eq!(eval(&mut page, "any.open + ''"), "true");
}

#[test]
fn esc_closes_the_dialogs_whose_closedby_state_allows_it() {
    let mut page = page_with(DIALOGS);
    // (a non-modal dialog closes on Esc only with closedby=any or closerequest)
    eval(&mut page, "auto.show(); 'ok'");
    esc(&mut page);
    assert_eq!(eval(&mut page, "auto.open + ''"), "true");
    eval(&mut page, "auto.close(); req.show(); 'ok'");
    esc(&mut page);
    assert_eq!(eval(&mut page, "req.open + ''"), "false");
    // (a modal dialog closes unless closedby=none)
    eval(&mut page, "none.showModal(); 'ok'");
    esc(&mut page);
    assert_eq!(eval(&mut page, "none.open + ''"), "true");
    eval(&mut page, "none.close(); auto.showModal(); 'ok'");
    esc(&mut page);
    assert_eq!(eval(&mut page, "auto.open + ''"), "false");
}

#[test]
fn a_cancel_listener_prevents_esc_only_after_a_user_activation() {
    let mut page = page_with(DIALOGS);
    eval(
        &mut page,
        "window.log = []; auto.addEventListener('cancel', e => {
           log.push(e.cancelable); e.preventDefault(); });
         auto.showModal(); 'ok'",
    );
    // (no activation: the `cancel` cannot be canceled, the dialog closes)
    esc(&mut page);
    assert_eq!(
        eval(&mut page, "[auto.open, log.join(',')].join('|')"),
        "false|false"
    );
    // (after a click: canceled once, which uses up the activation; the next Esc closes it)
    eval(&mut page, "log = []; 'ok'");
    click(&mut page, 25.0, 15.0);
    eval(&mut page, "auto.showModal(); 'ok'");
    esc(&mut page);
    assert_eq!(
        eval(&mut page, "[auto.open, log.join(',')].join('|')"),
        "true|true"
    );
    esc(&mut page);
    assert_eq!(
        eval(&mut page, "[auto.open, log.join(',')].join('|')"),
        "false|true,false"
    );
}

#[test]
fn close_watchers_shown_without_an_activation_between_them_close_together() {
    let mut page = page_with(DIALOGS);
    // (two dialogs shown by script: one group, one Esc closes both)
    eval(&mut page, "req.show(); any.show(); 'ok'");
    esc(&mut page);
    assert_eq!(
        eval(&mut page, "[req.open, any.open].join(',')"),
        "false,false"
    );
    // (a click between them: a group each, Esc closes the newer)
    eval(&mut page, "req.show(); 'ok'");
    click(&mut page, 25.0, 15.0);
    eval(&mut page, "any.show(); 'ok'");
    esc(&mut page);
    assert_eq!(
        eval(&mut page, "[req.open, any.open].join(',')"),
        "true,false"
    );
}

#[test]
fn dialog_commands_are_the_toggle_events_source_and_a_prevented_request_leaves_none() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0} dialog{position:fixed;inset:auto;left:300px;\
         top:300px;width:100px;height:100px;margin:0;padding:0;border:0}</style>\
         <button id=showb commandfor=d command=show-modal>o</button>\
         <dialog id=d closedby=any><button id=req commandfor=d command=request-close value=r>r\
         </button><button id=cl commandfor=d command=close value=v>c</button></dialog>",
    );
    let r = eval(
        &mut page,
        "window.log = [];
         d.addEventListener('beforetoggle', e => log.push(e.newState + ':' + (e.source && e.source.id)));
         showb.click(); cl.click();
         showb.click();
         d.addEventListener('cancel', e => e.preventDefault(), {once: true});
         req.click(); [d.open, d.returnValue].join('|')",
    );
    assert_eq!(r, "true|v");
    // (light dismiss after the prevented request: no source, no new return value)
    click(&mut page, 600.0, 100.0);
    assert_eq!(
        eval(
            &mut page,
            "[d.open, d.returnValue, log.join(',')].join('|')"
        ),
        "false|v|open:showb,closed:cl,open:showb,closed:null"
    );
}

/// Popovers: an auto one at the top left with a button inside, a hint one elsewhere.
const POPOVERS: &str = "<!doctype html><style>body{margin:0} [popover]{margin:0;inset:auto;\
    width:100px;height:100px} #a{left:0;top:0} #h{left:300px;top:300px}\
    #h2{left:500px;top:300px} #a2{left:0;top:400px}</style>\
    <div id=a popover=auto><button id=inner popovertarget=h2>in</button></div>\
    <div id=a2 popover=auto>a2</div><div id=h popover=hint>h</div><div id=h2 popover=hint>h2</div>";

#[test]
fn a_hint_popover_leaves_unrelated_autos_open_and_closes_with_its_parent() {
    let mut page = page_with(POPOVERS);
    let open = "[a, a2, h, h2].map(p => p.matches(':popover-open') ? 1 : 0).join('')";
    // (an unrelated hint leaves the auto open, and stays when it hides; an auto closes the hints)
    eval(&mut page, "a.showPopover(); h.showPopover(); 'ok'");
    assert_eq!(eval(&mut page, open), "1010");
    eval(&mut page, "a.hidePopover(); 'ok'");
    assert_eq!(eval(&mut page, open), "0010");
    eval(&mut page, "a2.showPopover(); 'ok'");
    assert_eq!(eval(&mut page, open), "0100");
    // (a hint whose source is in an auto popover has it as its hint stack parent: hiding the
    // parent hides the hint)
    eval(
        &mut page,
        "a2.hidePopover(); a.showPopover(); h2.showPopover({source: inner}); 'ok'",
    );
    assert_eq!(eval(&mut page, open), "1001");
    eval(&mut page, "a.hidePopover(); 'ok'");
    assert_eq!(eval(&mut page, open), "0000");
    // (an auto popover shown from inside a hint is shown as a hint, nested in it)
    let r = eval(
        &mut page,
        "h.showPopover(); a2.showPopover({source: h}); const both = [h, a2].every(p =>
           p.matches(':popover-open')); h.hidePopover(); [both, a2.matches(':popover-open')].join(',')",
    );
    assert_eq!(r, "true,false");
}

#[test]
fn no_popover_is_shown_while_one_is_being_hidden() {
    let mut page = page_with(POPOVERS);
    let r = eval(
        &mut page,
        "let err = 'none';
         a.addEventListener('beforetoggle', e => {
           if (e.newState == 'closed') try { a2.showPopover(); } catch (x) { err = x.name; }
         });
         a.showPopover(); a.hidePopover();
         [err, a.matches(':popover-open'), a2.matches(':popover-open')].join(',')",
    );
    assert_eq!(r, "InvalidStateError,false,false");
}
