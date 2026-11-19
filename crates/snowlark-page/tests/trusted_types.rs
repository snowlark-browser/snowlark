// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Trusted Types: policies and the trusted objects they make, CSP's `trusted-types` and
//! `require-trusted-types-for 'script'` at the sinks (with the default policy), and the minimal
//! same-document view transition.

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
fn policies_make_trusted_values_without_a_policy_requirement() {
    let mut page = page_with("<!doctype html><body>");
    let r = eval(
        &mut page,
        r#"
        const p = trustedTypes.createPolicy('p', { createHTML: (s, a) => s.toUpperCase() + (a || '') });
        const h = p.createHTML('<i>x</i>', '!');
        const d = document.createElement('div');
        d.innerHTML = h;
        const plain = document.createElement('div');
        plain.innerHTML = '<b>ok</b>';
        const err = f => { try { f(); return 'none'; } catch (e) { return e.name; } };
        [
          String(h), JSON.stringify(h), trustedTypes.isHTML(h), trustedTypes.isScript(h), trustedTypes.isHTML('s'),
          d.innerHTML, plain.innerHTML, p.name, err(() => p.createScript('x')),
          trustedTypes.createPolicy('default', {}) === trustedTypes.defaultPolicy,
          err(() => trustedTypes.createPolicy('default', {})),
          trustedTypes.getAttributeType('div', 'onclick'), trustedTypes.getAttributeType('div', 'title'),
          trustedTypes.getPropertyType('script', 'text'), String(trustedTypes.emptyScript),
        ].join(' | ')
        "#,
    );
    assert_eq!(
        r,
        "<I>X</I>! | \"<I>X</I>!\" | true | false | false | <i>X</i>! | <b>ok</b> | p | TypeError | true | \
         TypeError | TrustedScript |  | TrustedScript | "
    );
}

#[test]
fn required_trusted_types_guard_the_sinks() {
    let mut page = page_with(
        "<!doctype html><meta http-equiv=Content-Security-Policy \
         content=\"require-trusted-types-for 'script'; trusted-types safe default\"><body>",
    );
    let r = eval(
        &mut page,
        r#"
        window.violations = [];
        document.addEventListener('securitypolicyviolation', e => violations.push(e.effectiveDirective + ':' + e.sample));
        const err = f => { try { f(); return 'none'; } catch (e) { return e.name; } };
        const safe = trustedTypes.createPolicy('safe', { createHTML: s => s, createScript: s => s });
        const d = document.createElement('div');
        [
          err(() => trustedTypes.createPolicy('other', {})),
          err(() => { d.innerHTML = '<b>a</b>'; }),
          err(() => { d.innerHTML = safe.createHTML('<b>b</b>'); }), d.innerHTML,
          err(() => d.setAttribute('onclick', 'x()')),
          err(() => d.setAttribute('title', 'fine')),
          err(() => eval('1 + 1')),
          eval(safe.createScript('6 * 7')), new Function(safe.createScript('return 5'))(),
          new Function(safe.createScript('a'), safe.createScript('return a + 1'))(1),
          err(() => { safe.createScript('return 9'); return new Function('return 9'); }),
          err(() => new Function('a', safe.createScript('return a'))),
          err(() => new Function(String(safe.createScript('return 3')))),
        ].join(' | ')
        "#,
    );
    assert_eq!(
        r,
        "TypeError | TypeError | none | <b>b</b> | TypeError | none | EvalError | 42 | 5 | 2 | EvalError | EvalError \
         | EvalError"
    );
    // A default policy converts the strings.
    let r = eval(
        &mut page,
        r#"
        trustedTypes.createPolicy('default', { createHTML: (s, type, sink) => s + '<!--' + sink + '-->', createScript: s => s });
        const e = document.createElement('div');
        e.innerHTML = 'x';
        [e.innerHTML, eval('2 + 2')].join(' | ')
        "#,
    );
    assert_eq!(r, "x<!--Element innerHTML--> | 4");
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(
        eval(&mut page, "violations.join(' ')"),
        "trusted-types:other require-trusted-types-for:Element innerHTML|<b>a</b> \
         require-trusted-types-for:Element setAttribute|x() require-trusted-types-for:eval|1 + 1 \
         require-trusted-types-for:eval|(function anonymous(\n) {\nreturn 9\n}) \
         require-trusted-types-for:eval|(function anonymous(a\n) {\nreturn a\n}) \
         require-trusted-types-for:eval|(function anonymous(\n) {\nreturn 3\n})"
    );
}

#[test]
fn view_transitions_run_the_update_and_settle() {
    let mut page = page_with("<!doctype html><body><p id=p>old</p>");
    eval(
        &mut page,
        r#"
        window.log = [];
        const t = document.startViewTransition({ update: () => { p.textContent = 'new'; log.push('update'); }, types: ['slide'] });
        log.push(document.activeViewTransition === t, t.transitionRoot === document.documentElement, [...t.types].join());
        t.updateCallbackDone.then(() => log.push('done'));
        t.ready.then(() => log.push('ready'));
        t.finished.then(() => log.push('finished:' + document.activeViewTransition));
        const s = document.body.startViewTransition(() => { throw new Error('boom') });
        s.updateCallbackDone.catch(e => log.push('rejected:' + e.message));
        s.finished.catch(() => log.push('finished rejected'));
        const k = p.startViewTransition();
        k.skipTransition();
        k.ready.catch(e => log.push('skip:' + e.name));
        'ok'
        "#,
    );
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(
        eval(&mut page, "log.join(' ') + ' | ' + p.textContent"),
        "true true slide skip:AbortError update done ready finished:null rejected:boom finished rejected | new"
    );
}
