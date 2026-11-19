// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Geometry Interfaces Module Level 1 through the bindings: DOMMatrix construction from strings and
//! sequences, the stringifier, dictionary validation, DOMPoint and DOMQuad; Web IDL's default toJSON;
//! the [LegacyWindowAlias] names; and the legacy WheelEvent deltas.

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
fn matrices_parse_transform_lists_and_serialize() {
    let mut page = page_with("<!doctype html>");
    let r = eval(
        &mut page,
        r#"
        const m = new DOMMatrix('translate(10px, 20px) rotate(90deg)');
        const throws = f => { try { f(); return 'no'; } catch (e) { return e.name; } };
        [
          String(m), m.is2D, m.isIdentity,
          String(new DOMMatrixReadOnly([1, 2, 3, 4, 5, 6]).inverse()),
          new DOMMatrix('translateZ(1px)').is2D,
          throws(() => new DOMMatrix('translate(1em)')),
          throws(() => new DOMMatrix(' ')),
          throws(() => new DOMMatrix([1, 2, 3])),
          throws(() => String(new DOMMatrix([NaN, 0, 0, 1, 0, 0]))),
          throws(() => DOMMatrix.fromMatrix({ a: 1, m11: 2 })),
          DOMMatrix.fromMatrix({ m13: 1 }).is2D,
          String(new DOMMatrix().scale(2).translate(1, 1)),
          WebKitCSSMatrix === DOMMatrix && SVGPoint === DOMPoint,
          new DOMMatrix([1, 0, 0, 1, 5, 6]).toFloat64Array().join(','),
        ].join(' | ')
        "#,
    );
    assert_eq!(
        r,
        "matrix(0, 1, -1, 0, 10, 20) | true | false | matrix(-2, 1, 1.5, -0.5, 1, -2) | false | \
         SyntaxError | SyntaxError | TypeError | InvalidStateError | TypeError | false | \
         matrix(2, 0, 0, 2, 2, 2) | true | 1,0,0,0,0,1,0,0,0,0,1,0,5,6,0,1"
    );
}

#[test]
fn points_quads_and_default_to_json() {
    let mut page = page_with("<!doctype html>");
    let r = eval(
        &mut page,
        r#"
        const p = new DOMPoint(1, 2).matrixTransform({ a: 2, d: 3, e: 10 });
        const q = new DOMQuad({ x: 0, y: 0 }, { x: 4, y: 0 }, { x: 4, y: 3 }, { x: 0, y: 3 });
        q.p1.x = -1;
        const b = q.getBounds();
        [
          p.x, p.y, p instanceof DOMPoint,
          b.x, b.width, b.height, q.p1 === q.p1,
          JSON.stringify(new DOMRect(1, 2, 3, 4)),
          JSON.stringify(new DOMPointReadOnly(1, 2)),
          Object.keys(q.toJSON()).join(','),
          Object.keys(performance.toJSON()).join(','),
        ].join(' | ')
        "#,
    );
    assert_eq!(
        r,
        "12 | 6 | true | -1 | 5 | 3 | true | \
         {\"x\":1,\"y\":2,\"width\":3,\"height\":4,\"top\":2,\"right\":4,\"bottom\":6,\"left\":1} | \
         {\"x\":1,\"y\":2,\"z\":0,\"w\":1} | p1,p2,p3,p4 | timeOrigin,interactionCount,timing,navigation"
    );
}

#[test]
fn wheel_events_have_the_legacy_deltas() {
    let mut page = page_with("<!doctype html>");
    let r = eval(
        &mut page,
        r#"
        const a = new WheelEvent('wheel', { deltaY: 100.7, deltaX: -3 });
        const b = new WheelEvent('wheel', { deltaX: 50, wheelDeltaX: 7 });
        [a.wheelDeltaY, a.wheelDeltaX, a.wheelDelta, b.wheelDeltaX, b.wheelDeltaY, b.wheelDelta].join(',')
        "#,
    );
    assert_eq!(r, "-100,3,-100,7,0,7");
}
