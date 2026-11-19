// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `localStorage` across event loops, through the service: two pages stand for two tabs.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use snowlark_page::web_storage::{WebStorage, WebStorageHost};
use snowlark_page::{Page, PageOptions};

/// A page on a thread of its own, as a tab is: V8 keeps one isolate entered per thread.
struct Tab {
    work: mpsc::Sender<Work>,
    thread: Option<std::thread::JoinHandle<()>>,
}

enum Work {
    Eval(String, mpsc::Sender<String>),
    Settle(mpsc::Sender<()>),
}

impl Drop for Tab {
    fn drop(&mut self) {
        // (closing the channel ends the thread; a test that failed in the page says so here)
        let (gone, _) = mpsc::channel();
        self.work = gone;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn tab(host: &Arc<WebStorageHost>, url: &str) -> Tab {
    let (work, inbox) = mpsc::channel();
    let (ready, loaded) = mpsc::channel();
    let service = host.clone() as Arc<dyn WebStorage>;
    let url = url::Url::parse(url).unwrap();
    let thread = std::thread::spawn(move || {
        let mut page = Page::new(PageOptions {
            web_storage: Some(service),
            ..Default::default()
        });
        page.load_html("<title>t</title>", &url);
        assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
        ready.send(()).unwrap();
        while let Ok(w) = inbox.recv() {
            match w {
                Work::Eval(js, answer) => {
                    let value = page.eval(&js, "test.js");
                    let _ = answer.send(value.unwrap_or_else(|e| panic!("{js}: {e}")));
                }
                Work::Settle(answer) => {
                    // (a turn, as the waker gives a tab: what the service sent is taken in there)
                    page.run_for(Duration::from_millis(50));
                    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
                    let _ = answer.send(());
                }
            }
        }
    });
    loaded.recv().expect("the page loads");
    Tab {
        work,
        thread: Some(thread),
    }
}

fn eval(tab: &mut Tab, js: &str) -> String {
    let (answer, value) = mpsc::channel();
    tab.work.send(Work::Eval(js.to_owned(), answer)).unwrap();
    value.recv().expect("the page answers")
}

/// Lets a page take in what the service sent it.
fn settle(tab: &mut Tab) {
    let (answer, done) = mpsc::channel();
    tab.work.send(Work::Settle(answer)).unwrap();
    done.recv().expect("the page settles");
}

const LOG: &str = "window.log = []; addEventListener('storage', e => log.push([e.key, e.oldValue, \
                   e.newValue, e.url, e.storageArea === localStorage, e.isTrusted].join('|')));";

#[test]
fn a_write_reaches_a_second_tab_with_its_storage_event() {
    let host = WebStorageHost::new();
    let mut a = tab(&host, "https://a.example/one");
    let mut b = tab(&host, "https://a.example/two");
    eval(&mut b, "localStorage.length");
    eval(&mut b, LOG);
    eval(&mut a, "localStorage.setItem('k', 'v')");
    settle(&mut b);
    assert_eq!(eval(&mut b, "localStorage.getItem('k')"), "v");
    assert_eq!(
        eval(&mut b, "log.join(';')"),
        "k||v|https://a.example/one|true|true"
    );
    // The writer hears no event of its own.
    eval(&mut a, LOG);
    eval(&mut a, "localStorage.setItem('k', 'w')");
    settle(&mut a);
    assert_eq!(eval(&mut a, "log.length"), "0");
    settle(&mut b);
    assert_eq!(
        eval(&mut b, "log[1]"),
        "k|v|w|https://a.example/one|true|true"
    );
}

#[test]
fn removing_and_clearing_reach_the_other_tab() {
    let host = WebStorageHost::new();
    let mut a = tab(&host, "https://a.example/");
    let mut b = tab(&host, "https://a.example/");
    eval(
        &mut a,
        "localStorage.x = '1'; localStorage.y = '2'; localStorage.z = '3'",
    );
    eval(&mut b, "localStorage.length");
    eval(&mut b, LOG);
    eval(&mut a, "localStorage.removeItem('x')");
    settle(&mut b);
    assert_eq!(eval(&mut b, "localStorage.length"), "2");
    eval(&mut a, "localStorage.clear()");
    settle(&mut b);
    assert_eq!(eval(&mut b, "localStorage.length"), "0");
    assert_eq!(
        eval(&mut b, "log.join(';')"),
        "x|1||https://a.example/|true|true;|||https://a.example/|true|true"
    );
}

#[test]
fn a_tab_opened_later_sees_what_is_there() {
    let host = WebStorageHost::new();
    let mut a = tab(&host, "https://a.example/");
    eval(&mut a, "localStorage.k = 'v'");
    let mut b = tab(&host, "https://a.example/");
    assert_eq!(eval(&mut b, "localStorage.k"), "v");
}

#[test]
fn other_origins_see_nothing() {
    let host = WebStorageHost::new();
    let mut a = tab(&host, "https://a.example/");
    let mut b = tab(&host, "https://b.example/");
    eval(&mut b, "localStorage.length");
    eval(&mut b, LOG);
    eval(&mut a, "localStorage.k = 'v'");
    settle(&mut b);
    assert_eq!(
        eval(&mut b, "localStorage.length + ':' + log.length"),
        "0:0"
    );
}

#[test]
fn nothing_is_said_when_nothing_changed() {
    let host = WebStorageHost::new();
    let mut a = tab(&host, "https://a.example/");
    let mut b = tab(&host, "https://a.example/");
    eval(&mut a, "localStorage.k = 'v'");
    eval(&mut b, "localStorage.length");
    eval(&mut b, LOG);
    eval(
        &mut a,
        "localStorage.setItem('k', 'v'); localStorage.removeItem('none')",
    );
    settle(&mut b);
    assert_eq!(eval(&mut b, "log.length"), "0");
    eval(&mut a, "localStorage.clear(); localStorage.clear()");
    settle(&mut b);
    assert_eq!(eval(&mut b, "log.length"), "1");
}

#[test]
fn a_tab_keeps_its_own_write_over_an_older_one_from_elsewhere() {
    let host = WebStorageHost::new();
    let mut a = tab(&host, "https://a.example/");
    let mut b = tab(&host, "https://a.example/");
    eval(&mut a, "localStorage.length");
    eval(&mut b, "localStorage.length");
    // Neither has heard of the other's write when it makes its own. The service took a's first, so
    // b's is the last: b must not fall back to a's value, and a must end up with b's.
    eval(&mut a, "localStorage.k = 'a'");
    eval(&mut b, "localStorage.k = 'b'");
    settle(&mut b);
    assert_eq!(eval(&mut b, "localStorage.k"), "b");
    settle(&mut a);
    assert_eq!(eval(&mut a, "localStorage.k"), "b");
}

#[test]
fn wiping_empties_a_tab_with_no_event() {
    let host = WebStorageHost::new();
    let mut a = tab(&host, "https://a.example/");
    eval(&mut a, "localStorage.k = 'v'");
    eval(&mut a, LOG);
    host.wipe("https://a.example");
    settle(&mut a);
    assert_eq!(
        eval(&mut a, "localStorage.length + ':' + log.length"),
        "0:0"
    );
}

#[test]
fn an_opaque_origin_keeps_nothing_in_the_service() {
    let host = WebStorageHost::new();
    let mut a = tab(&host, "data:text/html,x");
    eval(&mut a, "try { localStorage.k = 'v' } catch (e) {}");
    assert!(host.origins().is_empty(), "{:?}", host.origins());
    assert!(host.take_dirty().is_empty());
}

#[test]
fn the_area_is_handed_over_for_keeping() {
    let host = WebStorageHost::new();
    let mut a = tab(&host, "https://a.example/");
    eval(&mut a, "localStorage.k = 'v'");
    let dirty = host.take_dirty();
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].0, "https://a.example");
    assert_eq!(dirty[0].1, [("k".to_owned(), "v".to_owned())]);
}

#[test]
fn session_storage_stays_in_its_tab() {
    let host = WebStorageHost::new();
    let mut a = tab(&host, "https://a.example/");
    let mut b = tab(&host, "https://a.example/");
    eval(&mut a, "sessionStorage.k = 'v'");
    settle(&mut b);
    assert_eq!(eval(&mut b, "sessionStorage.length"), "0");
    assert!(host.origins().is_empty());
}
