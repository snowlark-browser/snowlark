// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DOM §8 XPath: `document.evaluate` and `XPathResult`'s kinds, `XPathEvaluator`, expressions with
//! the XPath 1.0 axes, functions and operators, HTML documents' name rules, the resolver (an
//! exception it throws is reported, then a NamespaceError), and iterators invalidated by mutation.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page_with(html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        viewport: (800, 600),
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

const DOC: &str = r#"<!DOCTYPE html><head><link rel="preload" as="fetch" href="/a"><link rel="stylesheet" href="/b"></head>
<body><div id="d"><p class="x">one</p><p>two <b>bold</b></p><span lang="en">three</span></div><ul><li>1</li><li>2</li><li>3</li></ul></body>"#;

#[test]
fn first_ordered_node_with_attribute_predicates() {
    let mut page = page_with(DOC);
    // The expression a map page's legacy build runs at load.
    let js = r#"(() => {
        const r = document.evaluate("//link[@as='fetch' and @rel='preload']", document.head, null,
            XPathResult.FIRST_ORDERED_NODE_TYPE, null);
        return [r.resultType, r.singleNodeValue.getAttribute('href')].join();
    })()"#;
    assert_eq!(eval(&mut page, js), "9,/a");
}

#[test]
fn result_kinds_and_wrong_type_errors() {
    let mut page = page_with(DOC);
    let js = r#"(() => {
        const out = [];
        const n = document.evaluate("count(//p)", document, null, 0, null);
        out.push(n.resultType, n.numberValue);
        const s = document.evaluate("string(//p[1])", document, null, 0, null);
        out.push(s.resultType, s.stringValue);
        const b = document.evaluate("//p = 'two bold'", document, null, 0, null);
        out.push(b.resultType, b.booleanValue);
        try { n.stringValue; out.push('no'); } catch (e) { out.push(e.name); }
        try { document.evaluate("1", document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null); }
        catch (e) { out.push(e.name); }
        const snap = document.evaluate("//li", document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
        out.push(snap.snapshotLength, snap.snapshotItem(2).textContent, snap.snapshotItem(3));
        try { snap.iterateNext(); } catch (e) { out.push(e.name); }
        return out.join();
    })()"#;
    assert_eq!(
        eval(&mut page, js),
        "1,2,2,one,3,true,TypeError,TypeError,3,3,,TypeError"
    );
}

#[test]
fn iterator_is_invalidated_by_a_mutation() {
    let mut page = page_with(DOC);
    let js = r#"(() => {
        const out = [];
        const it = document.evaluate("//li", document, null, XPathResult.ORDERED_NODE_ITERATOR_TYPE, null);
        out.push(it.iterateNext().textContent, it.invalidIteratorState);
        document.querySelector('ul').append(document.createElement('li'));
        out.push(it.invalidIteratorState);
        try { it.iterateNext(); } catch (e) { out.push(e.name); }
        return out.join();
    })()"#;
    assert_eq!(eval(&mut page, js), "1,false,true,InvalidStateError");
}

#[test]
fn axes_functions_and_operators() {
    let mut page = page_with(DOC);
    let js = r#"(() => {
        const s = (e, c) => document.evaluate(e, c || document, null, XPathResult.STRING_TYPE, null).stringValue;
        const n = (e, c) => document.evaluate(e, c || document, null, XPathResult.NUMBER_TYPE, null).numberValue;
        const d = document.getElementById('d');
        return [
            s("name(//b/ancestor::*[2])"),
            n("count(//p[1]/following::*)"),
            n("count(//li[3]/preceding::li)"),
            s("//li[last()]"),
            s("(//li)[position() = 2]"),
            n("count(//p | //b | //p)"),
            s("local-name(//*[@class='x']/@class)"),
            s("//span/preceding-sibling::p[1]"),
            s("normalize-space('  a   b ')"),
            s("translate('abc', 'abc', 'AB')"),
            s("substring('12345', 1.5, 2.6)"),
            s("substring-after('a=b', '=')"),
            n("7 mod 3 + 10 div 4 - -1"),
            s("1 div 0"),
            n("round(-0.5) + round(2.5) + floor(-1.5) + ceiling(1.2)"),
            s("concat(string(1.50), '|', string(true()), '|', 0.1 + 0.2)"),
            n("count(id('d')/p)"),
            n("sum(//li)"),
            s("boolean(//nothing) or 3 > 2"),
            n("count(child::*)", d),
            s("string(.)", d.firstChild.firstChild),
            n("count(//DIV)"),
        ].join('|');
    })()"#;
    assert_eq!(
        eval(&mut page, js),
        "div|7|2|3|2|3|class|two bold|a b|AB|234|b|4.5|Infinity|3|1.5|true|0.30000000000000004|2|6|true|3|one|1"
    );
}

#[test]
fn syntax_and_context_errors() {
    let mut page = page_with(DOC);
    let js = r#"(() => {
        const out = [];
        for (const e of ["//p[", "foo(", "1 +", "'x", "//p bar", "　 ."]) {
            try { document.evaluate(e, document, null, 0, null); out.push('ok'); }
            catch (err) { out.push(err.name); }
        }
        try { document.evaluate("x:y", document, null, 0, null); } catch (err) { out.push(err.name); }
        try { document.evaluate(".", document.doctype, null, 0, null); } catch (err) { out.push(err.name); }
        try { document.evaluate("$v", document, null, 0, null); } catch (err) { out.push(err.name); }
        return out.join();
    })()"#;
    assert_eq!(
        eval(&mut page, js),
        "SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,NamespaceError,NotSupportedError,SyntaxError"
    );
}

#[test]
fn resolver_namespaces_and_reported_exceptions() {
    let mut page = page_with(DOC);
    let js = r#"(() => {
        const out = [];
        const doc = new DOMParser().parseFromString(
            '<r xmlns:a="urn:a"><a:x n="1"/><x n="2"/></r>', 'text/xml');
        const ev = new XPathEvaluator();
        const r = ev.evaluate("//p:x/@n", doc, prefix => prefix === 'p' ? 'urn:a' : null,
            XPathResult.STRING_TYPE, null);
        out.push(r.stringValue);
        out.push(doc.evaluate("count(//x)", doc, null, 1, null).numberValue);
        const expr = document.createExpression("//a:x", doc.createNSResolver(doc.documentElement));
        out.push(expr.evaluate(doc, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE).snapshotLength);
        // An empty string resolves a prefix (to no namespace); only null leaves it unresolved.
        out.push(document.evaluate("/q:x", document, () => "", 0, null).resultType);
        let reported = null;
        window.addEventListener('error', e => { reported = e.error; });
        try { document.evaluate("/q:x", document, () => { throw 'boom'; }); }
        catch (e) { out.push(e.name); }
        out.push(reported);
        return out.join();
    })()"#;
    assert_eq!(eval(&mut page, js), "1,1,1,4,NamespaceError,boom");
}
