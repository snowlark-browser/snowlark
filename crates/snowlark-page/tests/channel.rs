// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Channel messaging (HTML §9.4): MessageChannel, MessagePort queues, start(), onmessage, close().

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page_with(html: &str) -> Page {
    let mut page = Page::new(PageOptions::default());
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

#[test]
fn messages_wait_for_start_and_arrive_in_order_as_tasks() {
    let mut page = page_with("<!doctype html>");
    eval(
        &mut page,
        r#"
        var log = [];
        const { port1, port2 } = new MessageChannel();
        port1.addEventListener('message', e =>
          log.push(e.data.n + ':' + (e.target === port1) + ':' + e.ports.length + ':' + e.isTrusted));
        port2.postMessage({ n: 1 });
        port2.postMessage({ n: 2 });
        setTimeout(() => { log.push('timeout'); port1.start(); port2.postMessage({ n: 3 }); }, 0);
        Promise.resolve().then(() => log.push('microtask'));
        'ok'
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "microtask timeout 1:true:0:true 2:true:0:true 3:true:0:true"
    );
}

#[test]
fn onmessage_starts_the_port_and_close_disentangles() {
    let mut page = page_with("<!doctype html>");
    let r = eval(
        &mut page,
        r#"
        var log = [];
        var mc = new MessageChannel();
        mc.port2.onmessage = e => log.push('got:' + JSON.stringify(e.data));
        mc.port1.postMessage([1, 'a']);
        let err;
        try { mc.port1.postMessage(1, [mc.port1]); } catch (e) { err = e.name; }
        try { mc.port1.postMessage(() => 1); } catch (e) { err += ' ' + e.name; }
        err
        "#,
    );
    assert_eq!(r, "DataCloneError DataCloneError");
    page.run_until_settled(Duration::from_secs(5));
    eval(
        &mut page,
        "mc.port1.close(); mc.port1.postMessage('lost'); mc.port2.postMessage('lost'); 'ok'",
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "log.join(' ')"), r#"got:[1,"a"]"#);
}
