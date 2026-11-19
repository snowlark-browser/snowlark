// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML 4.11.1 the `details` element and 15.5.5 its rendering: the content slot hidden while closed
//! (`content-visibility: hidden` on `::details-content`), `toggle` events with coalescing, the `name`
//! exclusivity group, the summary activation behavior, and revealing by fragment navigation.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn page_with(html: &str) -> Page {
    // (a page with a renderer: geometry needs layout; the net thread lives as long as the process)
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

const BASIC: &str =
    "<!doctype html><style>body{margin:0}pre{margin:0;font:15px/15px monospace}</style>\
    <details id=d><summary>s</summary><pre id=p>x</pre></details><div id=after>after</div>";

#[test]
fn closed_details_hides_its_content_and_open_shows_it() {
    let mut page = page_with(BASIC);
    // The pre keeps a real size when closed (the contents are skipped, not removed: layout queries
    // answer as in Chrome), but takes no room, paints nothing and is not hit.
    let r = eval(
        &mut page,
        r#"
        const afterTop = () => document.getElementById('after').getBoundingClientRect().top;
        const hit = () => document.elementFromPoint(5, afterTop() + 2) === document.getElementById('after');
        const ph = () => document.getElementById('p').getBoundingClientRect().height;
        const phit = () => document.elementFromPoint(5, document.getElementById('p').getBoundingClientRect().top + 2) === document.getElementById('p');
        const out = [[ph(), afterTop(), hit()].join(',')];
        d.open = true;
        out.push([ph(), afterTop(), phit()].join(','));
        d.open = false;
        out.push([ph(), afterTop(), hit()].join(','));
        out.join(' ')
        "#,
    );
    let parts: Vec<Vec<String>> = r
        .split(' ')
        .map(|p| p.split(',').map(str::to_owned).collect())
        .collect();
    let n = |v: &str| v.parse::<f64>().unwrap();
    assert!(
        n(&parts[0][0]) >= 15.0,
        "closed content keeps its size: {r}"
    );
    assert_eq!(parts[0][2], "true", "closed content is not hit: {r}");
    assert!(
        n(&parts[1][1]) > n(&parts[0][1]),
        "after moves down when open: {r}"
    );
    assert_eq!(parts[1][2], "true", "open content is hit: {r}");
    assert_eq!(parts[2], parts[0], "closed again, as at first: {r}");
}

#[test]
fn a_content_visibility_hidden_element_skips_its_contents_but_keeps_their_layout() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}</style>\
         <div id=h style='content-visibility:hidden'><p id=q style='margin:0'>text</p></div><div id=n>n</div>",
    );
    let r = eval(
        &mut page,
        r#"
        [h.getBoundingClientRect().height, q.getBoundingClientRect().height > 0,
         document.elementFromPoint(5, 5) === q, document.elementFromPoint(5, 5) === h].join(',')
        "#,
    );
    // size containment: no height from the contents; the contents have a real size and are not hit
    assert_eq!(r, "0,true,false,false");
}

#[test]
fn summary_click_toggles_open_and_fires_one_coalesced_toggle_event() {
    let mut page = page_with(
        "<!doctype html><details id=d><summary id=s>s</summary>c</details><script>\
         window.log = []; d.addEventListener('toggle', e => log.push(e.oldState + '>' + e.newState + ':' + (e instanceof ToggleEvent)));\
         </script>",
    );
    eval(&mut page, "s.click(); s.click(); s.click(); ''");
    assert_eq!(eval(&mut page, "String(d.open)"), "true");
    // events are queued tasks: none yet, then one with the first old state and the last new state
    assert_eq!(eval(&mut page, "log.length + ''"), "0");
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "log.join()"), "closed>open:true");
    eval(
        &mut page,
        "d.open = false; d.open = true; d.open = false; ''",
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(
        eval(&mut page, "log.join()"),
        "closed>open:true,open>closed:true"
    );
}

#[test]
fn only_the_first_summary_toggles() {
    let mut page = page_with(
        "<!doctype html><details id=d><summary>1</summary><summary id=s2>2</summary></details>\
         <div id=notdetails><summary id=s3>3</summary></div>",
    );
    assert_eq!(
        eval(&mut page, "s2.click(); s3.click(); String(d.open)"),
        "false"
    );
    assert_eq!(
        eval(&mut page, "d.firstElementChild.click(); String(d.open)"),
        "true"
    );
}

#[test]
fn details_with_the_same_name_are_exclusive() {
    let mut page = page_with(
        "<!doctype html><details id=a name=g open>a</details><details id=b name=g>b</details>\
         <details id=c name=h open>c</details>",
    );
    let r = eval(
        &mut page,
        r#"
        const o = () => [a.open, b.open, c.open].join();
        const out = [o()];
        b.open = true; out.push(o());           // closes a, not c
        a.setAttribute('open', ''); out.push(o()); // closes b
        c.name = 'g'; out.push(o());            // c is open and a group member is open: c closes
        const e = document.createElement('details'); e.name = 'g'; e.open = true;
        document.body.append(e); out.push(o() + '/' + a.open + e.open); // inserting an open one closes it
        out.join(' ')
        "#,
    );
    assert_eq!(
        r,
        "true,false,true false,true,true true,false,true true,false,false true,false,false/truefalse"
    );
}

#[test]
fn a_fragment_inside_a_closed_details_opens_it() {
    let mut page = page_with(
        "<!doctype html><div style='height:3000px'></div><details id=d><summary>s</summary>\
         <p id=t>target</p></details>",
    );
    eval(&mut page, "location.hash = '#t'; ''");
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, "String(d.open)"), "true");
    assert_ne!(eval(&mut page, "String(window.scrollY)"), "0");
}

#[test]
fn details_content_pseudo_element_is_styled_by_author_rules() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}details::details-content{background:rgb(1,2,3)}\
         details[open]::details-content{padding-top:7px}</style>\
         <details id=d open><summary>s</summary><div id=c>c</div></details>",
    );
    // the content slot has a box whose padding shows in the layout: c starts below summary + 7px
    let r = eval(
        &mut page,
        "(c.getBoundingClientRect().top - d.firstElementChild.getBoundingClientRect().bottom) + ''",
    );
    assert_eq!(r, "7");
}

#[test]
fn a_details_without_a_summary_shows_a_default_summary_that_toggles() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}</style>         <details id=d><p id=c>content</p></details><div id=after>after</div>",
    );
    // closed: the default summary is one line; the content has a size but takes no room
    let r = eval(
        &mut page,
        "[d.getBoundingClientRect().height > 0, d.getBoundingClientRect().height < 30,           c.getBoundingClientRect().height > 0,           document.elementFromPoint(10, c.getBoundingClientRect().top + 2) === c].join()",
    );
    assert_eq!(r, "true,true,true,false");
    // clicking the default summary opens the element
    let none = Modifiers::default();
    page.pointer_pressed(Point::new(10.0, 5.0), 0, none, 1);
    page.pointer_released(Point::new(10.0, 5.0), 0, none, 1);
    assert_eq!(eval(&mut page, "String(d.open)"), "true");
    assert!(
        eval(&mut page, "String(c.getBoundingClientRect().height)")
            .parse::<f64>()
            .unwrap()
            > 0.0
    );
    // a click on the content does not toggle it
    let y = eval(&mut page, "String(c.getBoundingClientRect().top + 2)");
    page.pointer_pressed(Point::new(10.0, y.parse().unwrap()), 0, none, 1);
    page.pointer_released(Point::new(10.0, y.parse().unwrap()), 0, none, 1);
    assert_eq!(eval(&mut page, "String(d.open)"), "true");
    // a script click on the element does not use the default summary
    assert_eq!(eval(&mut page, "d.click(); String(d.open)"), "true");
}

/// Whether the element is what the pointer finds at its own top-left corner: contents skipped by a
/// closed `details` have a size but are not hit.
const SHOWN: &str = "globalThis.shown = e => { const r = e.getBoundingClientRect(); \
    return document.elementFromPoint(r.left + 2, r.top + 2) === e; };";

#[test]
fn nested_details_open_and_close_independently() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}</style><details id=o><summary>o</summary>\
         <details id=i><summary>i</summary><p id=p>deep</p></details></details>",
    );
    let mut q = |js: &str| eval(&mut page, &format!("{SHOWN}{js}"));
    assert_eq!(q("String(shown(p))"), "false");
    assert_eq!(q("o.open = true; String(shown(p))"), "false");
    assert_eq!(q("i.open = true; String(shown(p))"), "true");
    assert_eq!(q("o.open = false; String(shown(p))"), "false");
    assert_eq!(q("o.open = true; String(shown(p))"), "true");
}

#[test]
fn details_as_a_flex_or_grid_item_hides_and_shows_its_content() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}.f{display:flex}.g{display:grid}</style>\
         <div class=f><details id=a><summary>a</summary><p id=pa>x</p></details></div>\
         <div class=g><details id=b><summary>b</summary><p id=pb>y</p></details></div>",
    );
    let mut q = |js: &str| eval(&mut page, &format!("{SHOWN}{js}"));
    assert_eq!(q("[shown(pa), shown(pb)].join()"), "false,false");
    assert_eq!(
        q("a.open = b.open = true; [shown(pa), shown(pb)].join()"),
        "true,true"
    );
    assert_eq!(
        q("a.open = b.open = false; [shown(pa), shown(pb)].join()"),
        "false,false"
    );
}

#[test]
fn script_can_read_the_descendants_of_closed_content() {
    let mut page = page_with(
        "<!doctype html><details id=d><summary>s</summary><div id=a><span id=b>t</span> text</div></details>",
    );
    let r = eval(
        &mut page,
        r#"
        const out = [];
        for (const e of [a, b, d.lastChild]) {
          const r = e.getBoundingClientRect();
          out.push(r.width, e.offsetHeight, e.clientWidth, e.getClientRects().length,
                   typeof e.scrollHeight, e.checkVisibility());
        }
        const range = document.createRange(); range.selectNodeContents(a);
        out.push(range.getBoundingClientRect().height, getComputedStyle(b).display,
                 document.elementFromPoint(5, 40) !== b);
        window.getSelection().selectAllChildren(d);
        out.join()
        "#,
    );
    assert!(!r.is_empty());
}

/// HTML §4.11.1: each insertion runs "ensure details exclusivity by closing the given element if
/// needed" as it happens, so the parser (and `innerHTML`) keep the first open member of a name group
/// open and close the later ones; a parsed `open` attribute's change steps queue `toggle`.
#[test]
fn inserted_open_details_keep_the_first_of_a_name_group() {
    let mut page = page_with(
        "<!doctype html><script>window.toggles = [];\
         document.addEventListener('toggle', e => toggles.push(e.target.id + ':' + e.newState), true);\
         </script><details id=a name=g open></details><details id=b name=g open></details>\
         <details id=c name=g open></details><div id=box></div>",
    );
    let open =
        "[...document.querySelectorAll('details')].map(d => d.id + (d.open ? '+' : '-')).join()";
    assert_eq!(eval(&mut page, open), "a+,b-,c-");
    // (b and c opened and closed before their tasks ran: one coalesced toggle each, closed to closed)
    assert_eq!(
        eval(&mut page, "toggles.join()"),
        "a:open,b:closed,c:closed"
    );
    eval(
        &mut page,
        "box.innerHTML = '<details id=d name=h open></details><details id=e name=h open></details>'",
    );
    assert_eq!(eval(&mut page, open), "a+,b-,c-,d+,e-");
}
