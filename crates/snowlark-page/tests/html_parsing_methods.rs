// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §8.5: `DOMParser.parseFromString` and the HTML insertion methods (`appendHTML`,
//! `beforeHTML`... and their Unsafe counterparts).

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn page_with(html: &str) -> Page {
    static NET: std::sync::OnceLock<NetThread> = std::sync::OnceLock::new();
    let net = NET.get_or_init(|| NetThread::start(NetConfig::default()).unwrap());
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(html, &url::Url::parse("http://example.test/top").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// The new document's URL is that of the parser's relevant global object's document, whichever
/// realm the method comes from; an XML type makes a Document, not an XMLDocument.
#[test]
fn a_parsed_document_takes_the_parsers_realms_url() {
    let mut page = page_with("<!doctype html><iframe srcdoc='<p>child'></iframe>");
    let got = eval(
        &mut page,
        "const w = frames[0];\
         const ours = new DOMParser(), theirs = new w.DOMParser();\
         const a = w.DOMParser.prototype.parseFromString.call(ours, '<p>', 'text/html').URL;\
         const b = DOMParser.prototype.parseFromString.call(theirs, '<x/>', 'text/xml').URL;\
         const xml = ours.parseFromString('<x/>', 'application/xml');\
         [a, b, xml instanceof XMLDocument, xml instanceof Document].join()",
    );
    assert_eq!(got, "http://example.test/top,about:srcdoc,false,true");
}

/// HTML "parent for HTML insertion" and "filter and pre-insert HTML": no parsed HTML goes into an
/// html element, a template's children or a Document; a template takes it in its contents.
#[test]
fn insertion_methods_throw_where_a_fragment_cannot_go() {
    let mut page = page_with("<!doctype html><template id=t><p>x</p></template>");
    let got = eval(
        &mut page,
        "const name = f => { try { f(); return 'ok'; } catch (e) { return e.name; } };\
         const t = document.getElementById('t');\
         t.appendHTML('<td>cell</td>');\
         [name(() => document.documentElement.appendHTML('<p>')),\
          name(() => document.body.beforeHTML('<p>')),\
          name(() => t.appendChild(document.createElement('i')).afterHTML('<p>')),\
          name(() => document.documentElement.beforeHTML('<p>')),\
          name(() => document.createElement('p').afterHTML('<p>')),\
          name(() => t.content.firstChild.afterHTML('<b>')),\
          t.content.lastChild.localName, t.content.childNodes.length].join()",
    );
    assert_eq!(
        got,
        "HierarchyRequestError,HierarchyRequestError,HierarchyRequestError,\
         HierarchyRequestError,ok,ok,td,3"
    );
}

/// `runScripts` lets an Unsafe method's scripts run once inserted; the safe methods and the default
/// never run them.
#[test]
fn only_unsafe_insertions_with_run_scripts_run_scripts() {
    let mut page = page_with("<!doctype html><div id=d></div>");
    let got = eval(
        &mut page,
        "window.ran = [];\
         const s = n => '<script>ran.push(' + n + ')</scr' + 'ipt>';\
         d.appendHTMLUnsafe(s(1), { runScripts: true });\
         d.appendHTMLUnsafe(s(2));\
         d.appendHTML(s(3));\
         d.setHTMLUnsafe(s(4), { runScripts: true });\
         d.firstChild.afterHTMLUnsafe(s(5), { runScripts: true });\
         ran.join()",
    );
    assert_eq!(got, "1,4,5");
}

/// HTML §8.6: the safe methods sanitize with the default configuration (only its elements and
/// attributes, no comments, no `javascript:` navigations) or the options' own, from which the
/// unsafe parts are removed; the unsafe methods sanitize only when given a sanitizer.
#[test]
fn insertion_methods_sanitize_with_the_options_sanitizer() {
    let mut page = page_with("<!doctype html><div id=d></div>");
    let got = eval(
        &mut page,
        "const out = [];\
         const html = '<p id=p class=c onclick=x()>a<!--c--><b>b</b><img src=x>' +\
           '<a href=\"javascript:x\">l</a><a href=\"/ok\">m</a></p>';\
         d.setHTML(html); out.push(d.innerHTML);\
         d.setHTML(html, { sanitizer: { removeElements: ['b'] } }); out.push(d.innerHTML);\
         d.setHTML(html, { sanitizer: { replaceWithChildrenElements: ['b'], comments: true } });\
         out.push(d.innerHTML);\
         d.setHTMLUnsafe(html); out.push(d.innerHTML);\
         d.setHTMLUnsafe(html, { sanitizer: { elements: ['p', 'b'] } }); out.push(d.innerHTML);\
         try { d.setHTML('x', { sanitizer: { elements: [], removeElements: [] } }); }\
         catch (e) { out.push(e.name); }\
         out.join('|')",
    );
    assert_eq!(
        got,
        [
            "<p>a<b>b</b><a>l</a><a href=\"/ok\">m</a></p>",
            "<p id=\"p\" class=\"c\">a<img src=\"x\"><a>l</a><a href=\"/ok\">m</a></p>",
            "<p id=\"p\" class=\"c\">a<!--c-->b<img src=\"x\"><a>l</a><a href=\"/ok\">m</a></p>",
            "<p id=\"p\" class=\"c\" onclick=\"x()\">a<!--c--><b>b</b><img src=\"x\">\
             <a href=\"javascript:x\">l</a><a href=\"/ok\">m</a></p>",
            "<p id=\"p\" class=\"c\" onclick=\"x()\">a<!--c--><b>b</b></p>",
            "TypeError",
        ]
        .join("|")
    );
}

/// Trusted Types §3.4-3.6: where trusted types are required, an unsafe method takes markup as is
/// with `TrustedHTMLParserOptions` (a policy's `createParserOptions`, whose sanitizer applies),
/// needs a policy for options that need vetting, and runs the default policy's
/// `createParserOptions`, whose options then replace the caller's.
#[test]
fn trusted_parser_options_satisfy_trusted_types_and_carry_their_sanitizer() {
    let mut page = page_with(
        "<!doctype html><meta http-equiv=Content-Security-Policy \
         content=\"require-trusted-types-for 'script'\"><div id=d></div>",
    );
    let got = eval(
        &mut page,
        "const out = [];\
         const p = trustedTypes.createPolicy('p', { createHTML: s => s,\
           createParserOptions: o => ({ ...o, sanitizer: { removeElements: ['b'] } }) });\
         const opts = p.createParserOptions({ runScripts: true });\
         out.push(Object.prototype.toString.call(opts));\
         d.setHTMLUnsafe('<b>x</b><i>y</i>', opts); out.push(d.innerHTML);\
         try { d.setHTMLUnsafe('<i>z</i>'); } catch (e) { out.push(e.name); }\
         try { d.setHTMLUnsafe(p.createHTML('<i>z</i>'), { runScripts: true }); }\
         catch (e) { out.push(e.name); }\
         d.setHTMLUnsafe(p.createHTML('<i>w</i>')); out.push(d.innerHTML);\
         trustedTypes.createPolicy('default', { createParserOptions: (o, t, s) => {\
           out.push(t + ' ' + s); return { sanitizer: { removeElements: ['i'] } }; } });\
         d.setHTMLUnsafe(p.createHTML('<i>v</i><u>u</u>')); out.push(d.innerHTML);\
         d.firstChild.afterHTMLUnsafe('<i>v</i><u>s</u>'); out.push(d.innerHTML);\
         const q = trustedTypes.createPolicy('q', { createParserOptions: () => null });\
         try { q.createParserOptions(); } catch (e) { out.push(e.name); }\
         out.join('|')",
    );
    assert_eq!(
        got,
        "[object TrustedHTMLParserOptions]|<i>y</i>|TypeError|TypeError|<i>w</i>|\
         TrustedHTMLParserOptions Element setHTMLUnsafe|<u>u</u>|\
         TrustedHTMLParserOptions Node afterHTMLUnsafe|<u>u</u><u>s</u>|TypeError"
    );
}

/// HTML §8.6.2: a `Sanitizer`'s configuration, changed by its methods, sanitizes when passed as
/// the `sanitizer` option, and `get()` returns it canonical and sorted.
#[test]
fn a_sanitizer_object_configures_and_reports_its_configuration() {
    let mut page = page_with("<!doctype html><div id=d></div>");
    let got = eval(
        &mut page,
        "const out = [];\
         const s = new Sanitizer({ elements: ['p', 'em'], attributes: ['id'] });\
         out.push(s.allowElement('span'), s.allowElement('span'), s.removeElement('em'));\
         out.push(s.setComments(false), s.setComments(false), s.setComments(true), s.removeUnsafe());\
         const c = s.get();\
         out.push(c.elements.map(e => e.name).join(), c.attributes[0].name,\
                  String(c.attributes[0].namespace), c.comments, c.dataAttributes);\
         d.setHTMLUnsafe('<p id=a title=t><em>e</em><span>s</span><!--k--></p>', { sanitizer: s });\
         out.push(d.innerHTML);\
         out.push(JSON.stringify(new Sanitizer().get().processingInstructions));\
         try { new Sanitizer({ replaceWithChildrenElements: ['html'] }); }\
         catch (e) { out.push(e.name); }\
         out.join('|')",
    );
    assert_eq!(
        got,
        "true|false|true|true|false|true|true|p,span|id|null|true|true|\
         <p id=\"a\"><span>s</span><!--k--></p>|[]|TypeError"
    );
}
