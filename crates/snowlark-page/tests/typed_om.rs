// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Typed OM Level 1 through the bindings: numeric values and their arithmetic, the inline,
//! declared and computed style property maps, and serialization.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

/// A rendered page (computed styles and style sheets need a renderer), with its network thread.
fn page_with(html: &str) -> (NetThread, Page) {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (400, 100),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(html, &url::Url::parse("http://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    (net, page)
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

#[test]
fn numeric_values_compute_and_serialize() {
    let (_net, mut page) = page_with("<!doctype html>");
    let r = eval(
        &mut page,
        r#"
        const throws = f => { try { f(); return 'no'; } catch (e) { return e.name; } };
        const sum = CSS.px(1).add(CSS.em(2), CSS.px(3));
        [
          String(CSS.px(1).add(CSS.px(2))),
          String(sum), sum.constructor.name, sum.values.length,
          JSON.stringify(CSS.px(1).add(CSS.percent(2)).type()),
          CSS.in(1).to('px').value,
          String(CSS.px(1).add(CSS.in(1)).toSum('px')),
          throws(() => CSS.px(1).add(CSS.s(1))),
          throws(() => CSS.px(1).div(0)),
          throws(() => CSS.px(1).to('nope')),
          throws(() => new CSSMathSum()),
          String(new CSSMathMax(new CSSMathSum(1, 2), 3)),
          String(CSS.px(1).sub(CSS.em(2))),
          CSSNumericValue.parse('calc(2 + 3)').constructor.name,
          new CSSUnitValue(1, 'PX').unit,
        ].join(' | ')
        "#,
    );
    assert_eq!(
        r,
        "3px | calc(1px + 2em + 3px) | CSSMathSum | 3 | {\"length\":1,\"percentHint\":\"length\"} \
         | 96 | calc(97px) | TypeError | RangeError | SyntaxError | SyntaxError | max(1 + 2, 3) \
         | calc(1px + -2em) | CSSMathSum | px"
    );
}

#[test]
fn inline_maps_set_get_and_reject_values_the_property_does_not_take() {
    let (_net, mut page) = page_with("<!doctype html><div id=d></div>");
    let r = eval(
        &mut page,
        r#"
        const d = document.getElementById('d');
        const m = d.attributeStyleMap;
        const throws = f => { try { f(); return 'no'; } catch (e) { return e.name; } };
        m.set('width', CSS.px(10));
        m.set('height', CSS.em(-2));
        m.set('transition-duration', CSS.s(1), '2s');
        const out = [
          d.style.cssText,
          m.get('width').constructor.name + ':' + m.get('width').value,
          // (out of range: wrapped in calc())
          m.get('height').constructor.name,
          m.getAll('transition-duration').map(String).join(','),
          m.size, m.has('width'), m.get('color'),
          throws(() => m.set('width', CSS.number(0))),
          throws(() => m.set('width', CSS.s(1))),
          throws(() => m.set('margin', CSS.px(1))),
          throws(() => m.set('width', CSS.px(1), CSS.px(2))),
          throws(() => m.set('lemon', 'ade')),
          d.attributeStyleMap === m,
          d.computedStyleMap() === d.computedStyleMap(),
          d.computedStyleMap().get('width').toString(),
        ];
        m.clear();
        out.push(d.getAttribute('style'));
        out.join(' | ')
        "#,
    );
    assert_eq!(
        r,
        "width: 10px; height: calc(-2em); transition-duration: 1s, 2s; | CSSUnitValue:10 \
         | CSSMathSum | 1s,2s | 3 | true |  | TypeError | TypeError | TypeError | TypeError \
         | TypeError | true | true | 10px | "
    );
}

#[test]
fn unsupported_values_and_var_references_reify_as_such() {
    let (_net, mut page) = page_with(
        "<!doctype html><style>#t { color: red; width: var(--a, 1em) }</style>\
         <div id=d style='--x: 1px; color: currentcolor'></div>",
    );
    let r = eval(
        &mut page,
        r#"
        const d = document.getElementById('d');
        const rule = document.styleSheets[0].cssRules[0];
        const width = rule.styleMap.get('width');
        const color = rule.styleMap.get('color');
        const other = document.createElement('div');
        other.attributeStyleMap.set('color', color);
        [
          color.constructor.name, String(color), other.style.color,
          width.constructor.name, width[0].variable, JSON.stringify(width[0].fallback[0]),
          String(width),
          d.attributeStyleMap.get('color').constructor.name,
          d.computedStyleMap().get('--x').constructor.name,
          String(new CSSUnparsedValue(['lem', 'on'])),
          String(new CSSKeywordValue(' a')),
        ].join(' | ')
        "#,
    );
    assert_eq!(
        r,
        "CSSStyleValue | red | red | CSSUnparsedValue | --a | \" 1em\" | var(--a, 1em) \
         | CSSKeywordValue | CSSUnparsedValue | lem/**/on | \\ a"
    );
}

#[test]
fn transforms_reify_and_give_matrices() {
    let (_net, mut page) = page_with(
        "<!doctype html><div id=d style='transform: translate(10px, 5px) rotate(90deg)'></div>",
    );
    let r = eval(
        &mut page,
        r#"
        const d = document.getElementById('d');
        const t = d.attributeStyleMap.get('transform');
        const throws = f => { try { f(); return 'no'; } catch (e) { return e.name; } };
        [
          t.constructor.name, t.length, t[0].constructor.name, t.is2D, String(t),
          String(t.toMatrix()),
          String(new CSSScale(2, 2)),
          throws(() => new CSSTranslate(CSS.s(1), CSS.px(1))),
          throws(() => new CSSTransformValue([])),
          throws(() => new CSSTranslate(CSS.em(1), CSS.px(0)).toMatrix()),
          new CSSTranslate(CSS.px(1), CSS.px(2), CSS.px(3)).is2D,
        ].join(' | ')
        "#,
    );
    assert_eq!(
        r,
        "CSSTransformValue | 2 | CSSTranslate | true | translate(10px, 5px) rotate(90deg) \
         | matrix(0, 1, -1, 0, 10, 5) | scale(2) | TypeError | TypeError | TypeError | false"
    );
}
