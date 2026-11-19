// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Event dispatch passes over the items of its path that had no listeners when it began (DOM §2.9):
//! a listener added during the dispatch to such an item still runs, and the event's target and
//! relatedTarget after the dispatch are those of the last struct, invoked or not.

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
fn listeners_added_during_dispatch_to_items_without_any_run() {
    let mut page =
        page_with("<!doctype html><div id=a><div id=b><div id=c><p id=t></p></div></div></div>");
    let r = eval(
        &mut page,
        r#"
        const out = [];
        const t = document.getElementById('t');
        // An ancestor that listens, but not to this type.
        document.getElementById('a').addEventListener('pong', () => out.push('pong'));
        // Only the target listens when the dispatch begins; its listener adds bubbling listeners to
        // ancestors that had none, and a capturing one, which must not run (capturing is over).
        t.addEventListener('ping', (e) => {
          out.push('t');
          document.getElementById('b').addEventListener('ping', (e) => {
            out.push('b:' + e.eventPhase + ':' + e.currentTarget.id + ':' + e.target.id);
            // added to a later item by a listener that was itself added during the dispatch
            document.body.addEventListener('ping', () => out.push('body'));
          });
          document.getElementById('a').addEventListener('ping', () => out.push('a-capture'), true);
          document.getElementById('a').addEventListener('ping', () => out.push('a'));
          window.addEventListener('ping', (e) => out.push('window:' + e.eventPhase));
        });
        const e = new Event('ping', { bubbles: true });
        t.dispatchEvent(e);
        out.push(e.eventPhase, e.currentTarget, e.target.id);
        out.join()
        "#,
    );
    assert_eq!(r, "t,b:3:b:t,a,body,window:3,0,,t");
}

#[test]
fn the_target_after_dispatch_is_the_last_structs_when_nothing_listens_there() {
    let mut page = page_with("<!doctype html><div id=host></div>");
    let r = eval(
        &mut page,
        r#"
        const host = document.getElementById('host');
        const root = host.attachShadow({ mode: 'open' });
        root.innerHTML = '<div><span id=inner></span></div>';
        const inner = root.getElementById('inner');
        const out = [];
        // Composed and bubbling: the last struct is the Window's, whose target is the host. Only
        // the inner span listens.
        inner.addEventListener('ping', (e) => out.push('inner:' + e.target.id));
        const e = new Event('ping', { bubbles: true, composed: true });
        inner.dispatchEvent(e);
        // (the last struct with a shadow-adjusted target is the host's, outside the shadow tree: the
        // target is not cleared)
        out.push(e.target.id);
        // Not composed: the path ends at the shadow root, whose struct has no shadow-adjusted
        // target; the inner span's struct is in the shadow tree, so the target is cleared.
        const e1 = new Event('ping', { bubbles: true });
        inner.dispatchEvent(e1);
        out.push(String(e1.target));
        // Not bubbling, not composed: the last struct invoked is the target's own.
        const e2 = new Event('ping');
        host.dispatchEvent(e2);
        out.push(e2.target.id);
        // Bubbling at a light-tree node with no listener anywhere on the path.
        const e3 = new Event('ping', { bubbles: true });
        host.dispatchEvent(e3);
        out.push(e3.target.id, e3.eventPhase, String(e3.currentTarget));
        out.join()
        "#,
    );
    assert_eq!(r, "inner:inner,host,inner:inner,null,host,host,0,null");
}

#[test]
fn a_mouse_events_related_target_after_dispatch_is_the_last_structs() {
    let mut page = page_with("<!doctype html><div id=a><p id=t></p></div><div id=r></div>");
    let r = eval(
        &mut page,
        r#"
        const t = document.getElementById('t');
        const rel = document.getElementById('r');
        const e = new MouseEvent('mouseover', { bubbles: true, relatedTarget: rel });
        t.dispatchEvent(e);
        e.relatedTarget.id + ',' + e.target.id
        "#,
    );
    assert_eq!(r, "r,t");
}
