// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<dialog>` and the top layer: showModal() renders the dialog over everything (z-index, clips and
//! transforms of its ancestors notwithstanding), centered, over its `::backdrop`; `:modal`, focus,
//! inertness, close() and Esc. Also `crypto.subtle.digest()`, which flingup's login (OAuth PKCE)
//! needed.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn page(net: &NetThread, html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(html, &url::Url::parse("https://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

const DOC: &str = "<!doctype html><body style='margin:0'>\
    <div id=over style='position:fixed;z-index:9999;left:0;top:0;width:800px;height:600px'></div>\
    <input id=outside>\
    <div style='overflow:hidden;height:10px;transform:translateZ(0);position:relative;z-index:1'>\
    <dialog id=d style='width:200px;height:fit-content;padding:0;border:0'>\
    <p style='margin:0;height:100px'>hi</p><input id=inside></dialog></div>";

#[test]
fn show_modal_puts_the_dialog_in_the_top_layer() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, DOC);
    eval(&mut p, "outside.focus(); 'ok'");
    let r = eval(
        &mut p,
        r#"
        const d = document.getElementById('d');
        const log = [];
        d.addEventListener('beforetoggle', e => log.push('before:' + e.oldState + '>' + e.newState));
        d.addEventListener('toggle', e => log.push('toggle:' + e.oldState + '>' + e.newState));
        d.showModal();
        [d.open, d.matches(':modal'), document.activeElement.id, document.querySelector(':modal') === d,
         log.join(',')].join('|')
        "#,
    );
    assert_eq!(r, "true|true|inside|true|before:closed>open");
    p.run_until_settled(Duration::from_secs(5));
    // Centered in the viewport, outside its clipped, transformed ancestor; hit above the z-index 9999
    // box; the backdrop's hit is the dialog; the rest of the document is inert.
    let r = eval(
        &mut p,
        r#"
        const r = d.getBoundingClientRect();
        [r.left, Math.abs(r.top - (600 - r.height) / 2) < 0.01, r.width, r.height > 100,
         document.elementFromPoint(400, 260) === d.querySelector('p'),
         document.elementFromPoint(10, 10) === d, log.join(','),
         (outside.focus(), document.activeElement.id)].join('|')
        "#,
    );
    assert_eq!(
        r,
        "300|true|200|true|true|true|before:closed>open,toggle:closed>open|inside"
    );
    // Esc: a cancelable cancel, then close; focus goes back.
    eval(
        &mut p,
        "d.addEventListener('cancel', () => log.push('cancel')); \
         d.addEventListener('close', () => log.push('close:' + d.returnValue)); 'ok'",
    );
    p.key_down(&snowlark_page::keyboard::KeyInput {
        vk: 0x1B,
        scan: 1,
        text: None,
        mods: snowlark_page::input::Modifiers::default(),
        repeat: false,
        paste: None,
        paste_html: None,
    });
    p.run_until_settled(Duration::from_secs(5));
    let r = eval(
        &mut p,
        "[d.open, d.matches(':modal'), document.activeElement.id, log.slice(2).join(','), \
          document.elementFromPoint(10, 10).id].join('|')",
    );
    assert_eq!(
        r,
        "false|false|outside|cancel,before:open>closed,toggle:open>closed,close:|over"
    );
}

#[test]
fn dialog_api_errors_and_close() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, DOC);
    let r = eval(
        &mut p,
        r#"
        const d = document.getElementById('d');
        const err = f => { try { f(); return 'none'; } catch (e) { return e.name; } };
        const out = [];
        d.show();
        out.push(d.open, d.matches(':modal'), err(() => d.showModal()));
        d.close('x');
        out.push(d.open, d.returnValue);
        const loose = document.createElement('dialog');
        out.push(err(() => loose.showModal()));
        d.showModal();
        d.remove();
        out.push(d.matches(':modal'));
        out.join('|')
        "#,
    );
    assert_eq!(
        r,
        "true|false|InvalidStateError|false|x|InvalidStateError|false"
    );
}

/// HTML §4.10.6 button commands: `commandfor` and `command` show and close a dialog through a
/// cancelable, composed CommandEvent at it (its source the button; a canceled one does nothing; a
/// custom `--` command only fires the event); `command` reflects known keywords lowercased; a
/// button with `command` or `commandfor` and no valid `type` is a Button (it does not submit).
#[test]
fn button_commands_drive_a_dialog() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><dialog id=d></dialog>\
         <button id=open commandfor=d command=sHoW-MoDaL>O</button>\
         <button id=shut commandfor=d command=close value=v>C</button>\
         <button id=own commandfor=d command=--mine>M</button>\
         <form><button id=auto commandfor=d command=show-modal>A</button></form>",
    );
    let r = eval(
        &mut p,
        "const d = document.getElementById('d'); const out = []; const seen = []; \
         d.addEventListener('command', e => seen.push([e.command, e.source.id, e.cancelable, \
                                                      e.composed, e instanceof CommandEvent].join(':'))); \
         const b = id => document.getElementById(id); \
         out.push(b('open').command, b('auto').type); \
         b('open').click(); out.push(d.open, d.matches(':modal')); \
         b('shut').click(); out.push(d.open, d.returnValue); \
         b('own').click(); out.push(d.open); \
         d.addEventListener('command', e => e.preventDefault(), { once: true }); \
         b('open').click(); out.push(d.open); \
         b('auto').click(); out.push(d.open); \
         out.join('|') + '#' + seen.join(',')",
    );
    assert_eq!(
        r,
        "show-modal|button|true|true|false|v|false|false|false#\
         show-modal:open:true:true:true,close:shut:true:true:true,--mine:own:true:true:true,\
         show-modal:open:true:true:true"
    );
}

/// HTML §4.11.4 `showModal()`: a dialog whose node document is not fully active (one made by
/// `createHTMLDocument`, with no browsing context) throws InvalidStateError and stays closed.
#[test]
fn show_modal_needs_a_fully_active_document() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, DOC);
    let r = eval(
        &mut p,
        "const doc = document.implementation.createHTMLDocument(); \
         const d = doc.createElement('dialog'); doc.body.append(d); \
         let name = 'none'; try { d.showModal(); } catch (e) { name = e.name; } \
         [name, d.open].join()",
    );
    assert_eq!(r, "InvalidStateError,false");
}

/// HTML §4.11.4 `requestClose()` through the dialog's close watcher: nothing on a disconnected
/// dialog, and a request from inside the `cancel` listener fires no second `cancel` (its return
/// value, none, replaces the first request's: "request to close the dialog" sets it first).
#[test]
fn request_close_runs_the_cancel_action_once() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, DOC);
    let r = eval(
        &mut p,
        "const d = document.getElementById('d'); let cancels = 0; \
         d.addEventListener('cancel', () => { cancels++; d.requestClose(); }); \
         d.showModal(); d.requestClose('x'); const out = [cancels, d.open, d.returnValue]; \
         d.show(); d.remove(); d.requestClose(); out.push(d.open, cancels); out.join()",
    );
    assert_eq!(r, "1,false,,true,1");
}

/// HTML §4.10.21.3: a form with `method=dialog` closes its nearest ancestor dialog, the result the
/// submitter's value (unchanged when the submitter has none), and navigates nowhere.
#[test]
fn a_dialog_form_submission_closes_the_dialog_with_the_submitters_value() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(
        &net,
        "<!doctype html><dialog id=d><form method=dialog><button id=yes value=ok>Y</button>\
         <button id=bare>N</button></form></dialog>",
    );
    let r = eval(
        &mut p,
        "const d = document.getElementById('d'); const out = []; \
         d.showModal(); document.getElementById('yes').click(); out.push(d.open, d.returnValue); \
         d.show(); d.returnValue = 'kept'; document.getElementById('bare').click(); \
         out.push(d.open, d.returnValue, location.href); out.join('|')",
    );
    assert_eq!(r, "false|ok|false|kept|https://example.test/");
}

#[test]
fn subtle_digest() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut p = page(&net, "<!doctype html>");
    eval(
        &mut p,
        r#"
        window.out = 'pending';
        const hex = b => [...new Uint8Array(b)].map(x => x.toString(16).padStart(2, '0')).join('');
        const data = new TextEncoder().encode('abc');
        Promise.all([
          crypto.subtle.digest('SHA-256', data).then(hex),
          crypto.subtle.digest({ name: 'sha-1' }, data).then(hex),
          crypto.subtle.digest('SHA-512', data.buffer).then(b => b.byteLength),
          crypto.subtle.digest('MD5', data).catch(e => e.name),
          crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, true, ['encrypt']).then(k => k.algorithm.name, e => e.name),
        ]).then(r => out = r.join('|') + '|' + SubtleCrypto.supports('digest', 'SHA-384'), e => out = 'threw ' + e);
        'ok'
        "#,
    );
    p.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut p, "out"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad|\
         a9993e364706816aba3e25717850c26c9cd0d89d|64|NotSupportedError|AES-GCM|true"
    );
}
