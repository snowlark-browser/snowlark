// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `querySelector` and `querySelectorAll` skip subtrees by their filters (`snowlark_dom::filter`): they must
//! find exactly what testing every element with `matches()` (which uses no filter) finds, in quirks and
//! no-quirks documents, across class and attribute changes, removals and re-insertions.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

const BODY: &str = r#"
<div class="a A" id=i1><p class="b">x<span data-x="1" class="c">y</span></p>
  <section><div class=b><i data-Y="2">z</i></div></section></div>
<svg viewBox="0 0 1 1"><g class=a><rect fill-rule=evenodd /></g></svg>
<ul><li class=item data-id=1>1</li><li class=item data-id=2>2</li></ul>
<div id=moved class=late-parent><b class=deep>d</b></div>
"#;

/// Every selector's `querySelectorAll` and `querySelector` against a filter of all elements by
/// `matches()`: an empty string when all agree, else the first selector that differs.
const COMPARE: &str = r#"
(() => {
  const selectors = ['.a', '.A', '.b', '.c', '.a .b', '.a > .b', 'p.b span.c', '[data-x]', '[DATA-X]',
    '[data-x="1"]', '[data-y]', '[data-Y="2"]', '[viewBox]', '[viewbox]', 'svg[viewBox]', '[fill-rule]',
    '.item[data-id="2"]', '#i1', '#i1 .c', ':not(.a)', ':is(.a, .item)', '.a, li', '[class~=""]',
    '.late', '.late .deep', '.deep', '.nothing', 'div', '*', '.b:first-child', 'li:nth-child(2).item'];
  const all = [...document.getElementsByTagName('*')];
  for (const s of selectors) {
    const want = all.filter(e => e.matches(s));
    const got = [...document.querySelectorAll(s)];
    if (got.length !== want.length || got.some((e, i) => e !== want[i]))
      return s + ': all ' + got.length + ' want ' + want.length;
    if (document.querySelector(s) !== (want[0] || null)) return s + ': first';
    const sub = document.getElementById('i1');
    const wantSub = want.filter(e => sub.contains(e) && e !== sub);
    const gotSub = [...sub.querySelectorAll(s)];
    if (gotSub.length !== wantSub.length || gotSub.some((e, i) => e !== wantSub[i]))
      return s + ': scoped';
  }
  return '';
})()
"#;

fn compare(page: &mut Page, step: &str) {
    let r = page.eval(COMPARE, "test.js").unwrap();
    assert_eq!(r, "", "{step}");
}

fn run(doctype: &str) {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        &format!("{doctype}<body>{BODY}"),
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    compare(&mut page, "parsed");
    page.eval(
        "document.querySelector('.c').className = 'late c'; \
         document.querySelector('i').setAttribute('data-x', '3'); \
         document.querySelector('[data-id=\"1\"]').classList.add('a');",
        "test.js",
    )
    .unwrap();
    compare(&mut page, "changed");
    page.eval(
        "const m = document.getElementById('moved'); m.remove(); \
         document.querySelector('.c').append(m); \
         document.querySelector('.item').remove(); \
         document.querySelector('section').innerHTML = '<p class=late><b class=deep>n</b></p>';",
        "test.js",
    )
    .unwrap();
    compare(&mut page, "moved");
    page.eval(
        "const c = document.querySelector('ul').cloneNode(true); document.body.prepend(c); \
         c.firstElementChild.removeAttribute('class');",
        "test.js",
    )
    .unwrap();
    compare(&mut page, "cloned");
}

#[test]
fn queries_find_what_matches_finds_in_no_quirks_mode() {
    run("<!doctype html>");
}

#[test]
fn queries_find_what_matches_finds_in_quirks_mode() {
    run("");
}
