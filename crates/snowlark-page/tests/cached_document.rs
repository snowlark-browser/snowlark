// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `Window.document` as a cached accessor (`[CachedAccessor]`, `rt::set_cached_attribute`): V8 reads it
//! from a private of the global, so it must be there from the start and follow every change of a
//! Window's document, with the getter itself still behaving as Web IDL says.

use snowlark_page::{Page, PageOptions};

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

fn page() -> Page {
    Page::new(PageOptions::default())
}

/// Reads `document` often enough that the read is optimised.
const HOT: &str =
    "function hot() { let d; for (let i = 0; i < 200000; i++) d = document; return d }";

#[test]
fn every_way_of_reading_it_gives_the_one_document() {
    let mut p = page();
    eval(&mut p, HOT);
    assert_eq!(
        eval(
            &mut p,
            "[document === window.document, document === self.document, hot() === document, \
             globalThis.document === hot(), document.nodeType, document instanceof Document].join()"
        ),
        "true,true,true,true,9,true"
    );
}

#[test]
fn the_getter_is_still_the_getter_web_idl_describes() {
    let mut p = page();
    assert_eq!(
        eval(
            &mut p,
            "const d = Object.getOwnPropertyDescriptor(window, 'document');
             const threw = f => { try { f(); return 'no' } catch (e) { return e.name } };
             [typeof d.get, d.get.name, d.get.length, d.set, d.enumerable, d.configurable,
              d.get.call(window) === document, d.get.call(undefined) === document,
              threw(() => d.get.call({})), threw(() => new d.get()),
              'prototype' in d.get].join()"
        ),
        "function,get document,0,,true,false,true,true,TypeError,TypeError,false"
    );
}

#[test]
fn it_cannot_be_replaced_or_seen_as_a_property() {
    let mut p = page();
    assert_eq!(
        eval(
            &mut p,
            "const before = document;
             window.document = 1;
             const deleted = delete window.document;
             [document === before, deleted,
              Object.getOwnPropertyNames(window).filter(n => /Window\\.document/.test(n)).length,
              Object.getOwnPropertySymbols(window).length].join()"
        ),
        "true,false,0,0"
    );
}

#[test]
fn an_inheriting_object_does_not_read_the_cache() {
    let mut p = page();
    assert_eq!(
        eval(
            &mut p,
            "try { Object.create(window).document; 'no' } catch (e) { e.name }"
        ),
        "TypeError"
    );
}

#[test]
fn a_new_frame_has_its_own_document_at_once() {
    let mut p = page();
    assert_eq!(
        eval(
            &mut p,
            "const f = document.createElement('iframe');
             document.body.append(f);
             const w = f.contentWindow;
             [w.document === f.contentDocument, w.document !== document,
              w.document.defaultView === w,
              Object.getPrototypeOf(w.document) === w.HTMLDocument.prototype].join()"
        ),
        "true,true,true,true"
    );
}

#[test]
fn a_kept_window_follows_its_new_document() {
    // HTML: leaving the initial about:blank for a same origin-domain document keeps the Window.
    let mut p = page();
    eval(
        &mut p,
        "window.f = document.createElement('iframe');
         document.body.append(f);
         window.w = f.contentWindow;
         window.first = w.document;
         w.eval(`function hot() { let d; for (let i = 0; i < 200000; i++) d = document; return d }`);
         w.hot();
         f.srcdoc = '<p id=there>';",
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while eval(
        &mut p,
        "String(!!f.contentDocument.getElementById('there'))",
    ) != "true"
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the frame never loaded"
        );
        p.run_for(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        eval(
            &mut p,
            "[f.contentWindow === w, w.document === f.contentDocument, w.document !== first,
              f.contentWindow.eval('document') === f.contentDocument].join()"
        ),
        "true,true,true,true"
    );
}
