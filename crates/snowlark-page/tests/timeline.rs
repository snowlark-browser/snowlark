// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Performance Timeline Level 2 and User Timing Level 3: the entry buffer, marks and measures with
//! their errors, PerformanceObserver delivery as a queued task, and the same API in a worker.

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
fn marks_and_measures_fill_the_buffer_in_start_time_order() {
    let mut page = page_with("<!doctype html>");
    let r = eval(
        &mut page,
        r#"
        const a = performance.mark('a', { startTime: 5 });
        performance.mark('b', { startTime: 1 });
        performance.mark('a', { startTime: 9 });
        const m = performance.measure('m', 'b', 'a');
        const d = performance.measure('d', { start: 2, duration: 3 });
        const e = performance.measure('e', { duration: 4, end: 10 });
        [
          performance.getEntries().map(e => e.entryType[1] + e.name + e.startTime).join(' '),
          m.duration, d.startTime + '+' + d.duration, e.startTime + '+' + e.duration,
          performance.getEntriesByName('a').length, performance.getEntriesByType('measure').length,
          performance.getEntriesByName('a', 'measure').length,
          a instanceof PerformanceMark && a instanceof PerformanceEntry, m instanceof PerformanceMeasure,
          a.id > 0 && m.id > a.id, a.duration, JSON.stringify(Object.keys(a.toJSON())),
        ].join('|')
        "#,
    );
    assert_eq!(
        r,
        "ivisible0 ab1 em1 ed2 aa5 ee6 aa9|8|2+3|6+4|2|3|0|true|true|true|0|\
         [\"id\",\"name\",\"entryType\",\"startTime\",\"duration\",\"navigationId\"]"
    );
    let r = eval(
        &mut page,
        r#"
        performance.clearMarks('a');
        const left = performance.getEntriesByType('mark').map(e => e.name).join();
        performance.clearMarks();
        performance.clearMeasures('d');
        const measures = performance.getEntriesByType('measure').map(e => e.name).join();
        performance.clearMeasures();
        left + '|' + measures + '|' + performance.getEntries().length
        "#,
    );
    // (the visibility-state entry stays)
    assert_eq!(r, "b|m,e|1");
}

#[test]
fn user_timing_errors() {
    let mut page = page_with("<!doctype html>");
    let r = eval(
        &mut page,
        r#"
        const err = f => { try { f(); return 'none'; } catch (e) { return e.name; } };
        [
          err(() => performance.mark('navigationStart')),
          err(() => new PerformanceMark('loadEventEnd')),
          err(() => performance.mark('x', { startTime: -1 })),
          err(() => performance.measure('m', 'no-such-mark')),
          err(() => performance.measure('m', { start: 0 }, 'x')),
          err(() => performance.measure('m', { detail: 1 })),
          err(() => performance.measure('m', { start: 0, duration: 1, end: 2 })),
          err(() => performance.measure('m', { start: -1 })),
          err(() => performance.measure('m', 'navigationStart')),
          err(() => performance.measure('m', 'navigationStart', 'unloadEventStart')),
          err(() => performance.mark('x', { detail: () => 1 })),
          err(() => structuredClone(performance.mark('y'))),
        ].join(' ')
        "#,
    );
    assert_eq!(
        r,
        "SyntaxError SyntaxError TypeError SyntaxError TypeError TypeError TypeError TypeError \
         none InvalidAccessError DataCloneError DataCloneError"
    );
}

#[test]
fn measure_converts_load_timing_names_after_load() {
    let mut page = page_with("<!doctype html><body>");
    let r = eval(
        &mut page,
        r#"
        const m = performance.measure('load', 'navigationStart', 'loadEventEnd');
        const i = performance.measure('dcl', 'domInteractive', 'domContentLoadedEventEnd');
        [m.startTime, m.duration > 0, i.startTime > 0, i.duration >= 0].join()
        "#,
    );
    assert_eq!(r, "0,true,true,true");
}

#[test]
fn detail_is_a_structured_clone_and_null_by_default() {
    let mut page = page_with("<!doctype html>");
    let r = eval(
        &mut page,
        r#"
        const detail = { list: [1, 2], when: new Date(0) };
        const a = performance.mark('a', { detail });
        const m = performance.measure('m', { start: 0, detail });
        [
          a.detail !== detail, a.detail === a.detail, JSON.stringify(a.detail.list),
          a.detail.when instanceof Date, m.detail !== a.detail, m.detail.list[1],
          performance.mark('b').detail, performance.measure('n').detail,
          performance.mark('c', { detail: null }).detail,
        ].join('|')
        "#,
    );
    assert_eq!(r, "true|true|[1,2]|true|true|2|||");
}

#[test]
fn observers_are_called_from_a_queued_task() {
    let mut page = page_with("<!doctype html>");
    eval(
        &mut page,
        r#"
        var log = [];
        const po = new PerformanceObserver(function (list, observer, options) {
          log.push('po:' + list.getEntries().map(e => e.name).join(',') + ':' +
                   (this === po && observer === po) + ':' + options.droppedEntriesCount);
          log.push('byType:' + list.getEntriesByType('measure').length +
                   ' byName:' + list.getEntriesByName('a', 'mark').length);
        });
        po.observe({ type: 'mark' });
        po.observe({ type: 'measure' });
        performance.mark('a');
        performance.measure('m');
        Promise.resolve().then(() => log.push('microtask'));
        log.push('sync');
        'ok'
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "sync microtask po:m,a:true:0 byType:1 byName:1"
    );
    // The dropped entries count comes with the first callback after observe() only.
    let r = eval(
        &mut page,
        r#"
        log = [];
        performance.mark('b');
        const taken = po.takeRecords().map(e => e.name).join();
        performance.mark('c');
        'taken:' + taken
        "#,
    );
    assert_eq!(r, "taken:b");
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(&mut page, "log[0]"),
        "po:c:true:undefined",
        "no droppedEntriesCount on later callbacks"
    );
    let r = eval(
        &mut page,
        r#"
        log = [];
        po.disconnect();
        performance.mark('d');
        'ok'
        "#,
    );
    assert_eq!(r, "ok");
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "log.length"), "0");
}

#[test]
fn observe_options_and_buffered_entries() {
    let mut page = page_with("<!doctype html>");
    let r = eval(
        &mut page,
        r#"
        const err = f => { try { f(); return 'none'; } catch (e) { return e.name; } };
        const a = new PerformanceObserver(() => {});
        const b = new PerformanceObserver(() => {});
        [
          err(() => a.observe({})),
          err(() => a.observe({ entryTypes: ['mark'], type: 'mark' })),
          err(() => a.observe({ entryTypes: ['mark'], buffered: true })),
          err(() => a.observe({ entryTypes: ['mark', 'nope'] })),
          err(() => a.observe({ type: 'mark' })),
          err(() => b.observe({ type: 'nope' })),
          err(() => b.observe({ entryTypes: ['mark'] })),
          PerformanceObserver.supportedEntryTypes.join(),
          PerformanceObserver.supportedEntryTypes === PerformanceObserver.supportedEntryTypes,
          Object.isFrozen(PerformanceObserver.supportedEntryTypes),
        ].join(' ')
        "#,
    );
    assert_eq!(
        r,
        "TypeError TypeError TypeError none InvalidModificationError none \
         InvalidModificationError event,first-input,mark,measure,navigation,paint,resource,\
         visibility-state true true"
    );
    eval(
        &mut page,
        r#"
        var seen = [];
        performance.mark('early');
        new PerformanceObserver(list => seen.push('buffered:' + list.getEntries().map(e => e.name)))
          .observe({ type: 'mark', buffered: true });
        new PerformanceObserver(list => seen.push('unbuffered:' + list.getEntries().length))
          .observe({ type: 'mark' });
        'ok'
        "#,
    );
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(eval(&mut page, "seen.join(' ')"), "buffered:early");
}

#[test]
fn a_frame_has_its_own_timeline() {
    let mut page = page_with("<!doctype html><iframe srcdoc='<!doctype html>'></iframe>");
    let r = eval(
        &mut page,
        r#"
        const w = frames[0];
        performance.mark('top');
        const m = w.performance.mark('child');
        [
          performance.getEntries().map(e => e.name).join(),
          w.performance.getEntries().map(e => e.name).join(),
          m instanceof w.PerformanceMark, m instanceof PerformanceMark,
        ].join('|')
        "#,
    );
    assert_eq!(r, "visible,top|visible,child|true|false");
}

/// HR Time "coarsen time": a context that is not cross-origin isolated reads the clock at 100 µs
/// (WPT `hr-time/clamped-time-origin.html`, `timing-attack.html`).
#[test]
fn performance_now_has_a_resolution_of_100_microseconds() {
    let mut page = page_with("<!doctype html>");
    let r = eval(
        &mut page,
        r#"
        let off = 0, changes = 0, smallest = Infinity, prev = performance.now();
        const end = prev + 20;
        while (performance.now() < end) {
          const t = performance.now();
          if (Math.round(t * 10) / 10 !== t) off++;
          if (t !== prev) { changes++; smallest = Math.min(smallest, t - prev); }
          prev = t;
        }
        // (it does advance, and never by less than a step)
        [off, changes > 50, smallest > 0.0999].join()
        "#,
    );
    assert_eq!(r, "0,true,true");
}

#[test]
fn the_timeline_records_the_visibility_state_from_time_zero() {
    let mut page = page_with("<!doctype html><iframe srcdoc='<p>frame'></iframe>");
    let entries = "(w => w.performance.getEntriesByType('visibility-state').map(e => \
        [e.constructor.name, e.entryType, e.name, e.startTime > 0 ? 'later' : e.startTime, \
        e.duration].join(':')).join(' '))";
    assert_eq!(
        eval(
            &mut page,
            &format!(
                "[{entries}(window), {entries}(frames[0]), \
                 PerformanceObserver.supportedEntryTypes.includes('visibility-state'), \
                 performance.getEntries().filter(e => e.entryType == 'visibility-state').length]\
                 .join(' | ')"
            )
        ),
        "VisibilityStateEntry:visibility-state:visible:0:0 | \
         VisibilityStateEntry:visibility-state:visible:0:0 | true | 1"
    );
    eval(
        &mut page,
        "window.seen = []; \
         new PerformanceObserver(l => seen.push(...l.getEntries().map(e => 'observed ' + e.name)))\
         .observe({type: 'visibility-state'}); \
         document.onvisibilitychange = () => \
           seen.push('event ' + performance.getEntriesByType('visibility-state').length);",
    );
    page.engine().set_shown(false);
    page.run_until_settled(Duration::from_secs(5));
    page.engine().set_shown(true);
    page.run_until_settled(Duration::from_secs(5));
    assert_eq!(
        eval(
            &mut page,
            &format!("{entries}(window) + ' | ' + seen.join()")
        ),
        "VisibilityStateEntry:visibility-state:visible:0:0 \
         VisibilityStateEntry:visibility-state:hidden:later:0 \
         VisibilityStateEntry:visibility-state:visible:later:0 | \
         event 2,observed hidden,event 3,observed visible"
    );
}
