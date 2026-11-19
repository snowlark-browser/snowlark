// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Installs the generated bindings on a context with a mock engine and checks the JS-visible shape: the
//! prototype chain, class strings, constants, brand checks, reflection, overload resolution, exceptions.

mod common;

use common::{eval, Mock};
use snowlark_bindings::{rt, InterfaceId};
use std::rc::Rc;

fn with_page(f: impl FnOnce(&mut v8::PinScope, &Mock)) {
    snowlark_js::init();
    let isolate = &mut v8::Isolate::new(Default::default());
    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let mock = Rc::new(Mock::default());
    let div = mock.add_element(InterfaceId::HTMLDivElement, "div");
    let img = mock.add_element(InterfaceId::HTMLImageElement, "img");
    snowlark_bindings::install(scope, mock.clone(), rt::InstallOptions::default())
        .expect("install");
    let global = context.global(scope);
    for (name, h) in [("el", div), ("img", img)] {
        let w = rt::wrap(scope, h).unwrap();
        let k = v8::String::new(scope, name).unwrap();
        global.set(scope, k.into(), w.into()).unwrap();
    }
    f(scope, &mock);
}

fn assert_true(scope: &mut v8::PinScope, src: &str) {
    assert_eq!(eval(scope, src), Ok("true".to_string()), "{src}");
}

#[test]
fn prototype_chain_and_class_string() {
    with_page(|scope, _| {
        assert_true(scope, "el instanceof HTMLDivElement && el instanceof HTMLElement && el instanceof Element && el instanceof Node && el instanceof EventTarget");
        assert_true(
            scope,
            "Object.prototype.toString.call(el) === '[object HTMLDivElement]'",
        );
        assert_true(
            scope,
            "Object.prototype.toString.call(Node.prototype) === '[object Node]'",
        );
        assert_true(
            scope,
            "Object.getPrototypeOf(HTMLElement.prototype) === Element.prototype",
        );
        assert_true(scope, "Object.getPrototypeOf(HTMLElement) === Element");
        assert_true(
            scope,
            "!(img instanceof HTMLDivElement) && img instanceof HTMLImageElement",
        );
        assert_true(
            scope,
            "el === el && el.nodeType === 1 && el.tagName === 'DIV'",
        );
        assert_true(
            scope,
            "!Object.getOwnPropertyDescriptor(window = globalThis, 'Node').enumerable",
        );
        assert_true(
            scope,
            "!Object.getOwnPropertyDescriptor(Node, 'prototype').writable",
        );
    });
}

#[test]
fn constants_on_interface_and_prototype() {
    with_page(|scope, _| {
        assert_true(
            scope,
            "Node.ELEMENT_NODE === 1 && Node.prototype.DOCUMENT_NODE === 9 && el.TEXT_NODE === 3",
        );
        assert_true(scope, "(d => !d.writable && !d.configurable && d.enumerable)(Object.getOwnPropertyDescriptor(Node, 'ELEMENT_NODE'))");
        assert_true(scope, "DOMException.INVALID_CHARACTER_ERR === 5");
    });
}

#[test]
fn function_names_lengths_and_constructors() {
    with_page(|scope, _| {
        assert_true(scope, "Object.getOwnPropertyDescriptor(Node.prototype, 'nodeType').get.name === 'get nodeType'");
        assert_true(scope, "Node.prototype.appendChild.name === 'appendChild' && Node.prototype.appendChild.length === 1");
        assert_true(scope, "Node.name === 'Node' && Event.length === 1");
        assert_true(scope, "!('prototype' in Node.prototype.appendChild)");
        assert_true(scope, "(() => { try { new Node(); return false } catch (e) { return e instanceof TypeError } })()");
        assert_true(scope, "(() => { try { new Node.prototype.appendChild(); return false } catch (e) { return e instanceof TypeError } })()");
    });
}

#[test]
fn illegal_invocation_on_wrong_receivers() {
    with_page(|scope, _| {
        let r = eval(
            scope,
            "Object.getOwnPropertyDescriptor(Node.prototype, 'nodeType').get.call({})",
        );
        assert!(
            r.as_ref().unwrap_err().contains("Illegal invocation"),
            "{r:?}"
        );
        let r = eval(scope, "Node.prototype.appendChild.call({}, el)");
        assert!(
            r.as_ref().unwrap_err().contains("Illegal invocation"),
            "{r:?}"
        );
        let r = eval(scope, "Object.getOwnPropertyDescriptor(Element.prototype, 'tagName').get.call(Node.prototype)");
        assert!(
            r.as_ref().unwrap_err().contains("Illegal invocation"),
            "{r:?}"
        );
        // An argument of the wrong interface fails the brand check too.
        let r = eval(scope, "el.appendChild({})");
        assert!(
            r.as_ref().unwrap_err().contains("is not of type 'Node'"),
            "{r:?}"
        );
    });
}

#[test]
fn reflected_attributes() {
    with_page(|scope, mock| {
        // Enumerated (limited to only known values): dir.
        assert_true(scope, "el.dir === ''");
        assert_true(scope, "el.dir = 'RTL', el.dir === 'rtl'");
        assert_eq!(mock.attr(rt::Handle::new(0), "dir").as_deref(), Some("RTL"));
        assert_true(scope, "el.dir = 'sideways', el.dir === ''");
        // Nullable enumerated with an empty-string keyword: crossOrigin.
        assert_true(scope, "img.crossOrigin === null");
        assert_true(
            scope,
            "img.crossOrigin = '', img.crossOrigin === 'anonymous'",
        );
        assert_true(
            scope,
            "img.crossOrigin = 'USE-credentials', img.crossOrigin === 'use-credentials'",
        );
        assert_true(scope, "img.crossOrigin = null, img.crossOrigin === null");
        assert_eq!(mock.attr(rt::Handle::new(1), "crossorigin"), None);
        // Strings, booleans, URLs and unsigned longs.
        assert_true(scope, "el.title = 42, el.title === '42'");
        assert_true(
            scope,
            "el.inert === false && (el.inert = 1, el.inert === true)",
        );
        assert_true(scope, "el.inert = false, el.inert === false");
        assert_true(
            scope,
            "img.src === '' && (img.src = 'a.png', img.src === 'https://example.test/a.png')",
        );
        assert_true(
            scope,
            "img.hspace === 0 && (img.hspace = 640, img.hspace === 640)",
        );
        assert_true(scope, "img.setAttribute('hspace', 'x'), img.hspace === 0");
        assert_eq!(
            mock.attr(rt::Handle::new(1), "hspace").as_deref(),
            Some("x")
        );
        // [CEReactions] brackets the setter.
        assert!(mock.log.borrow().iter().any(|l| l == "ce:pop"));
    });
}

#[test]
fn overload_resolution_with_two_signatures() {
    with_page(|scope, mock| {
        assert_true(scope, "el.scroll() instanceof Promise");
        assert_true(
            scope,
            "el.scroll({ left: 5, behavior: 'smooth' }) instanceof Promise",
        );
        assert_true(scope, "el.scroll(1, '2') instanceof Promise");
        assert_eq!(
            *mock.log.borrow(),
            vec![
                "scroll(options left=None top=None behavior=auto)".to_string(),
                "scroll(options left=Some(5.0) top=None behavior=smooth)".to_string(),
                "scroll(x=1 y=2)".to_string(),
            ]
        );
        // A bad enum value rejects the returned promise (Promise-returning operations never throw).
        assert_true(scope, "el.scroll({ behavior: 'fast' }) instanceof Promise");
        assert_eq!(mock.log.borrow().len(), 3);
    });
}

#[test]
fn engine_errors_become_dom_exceptions() {
    with_page(|scope, _| {
        assert_true(
            scope,
            "(() => { try { el.setAttribute('1x', 'v'); return false } catch (e) { return e instanceof DOMException && e instanceof Error && e.name === 'InvalidCharacterError' && e.code === 5 } })()",
        );
        assert_true(scope, "el.setAttribute('data-x', 7) === undefined");
        let r = eval(scope, "el.setAttribute('x')");
        assert!(
            r.as_ref().unwrap_err().contains("2 argument(s) required"),
            "{r:?}"
        );
    });
}

#[test]
fn getters_stay_correct_when_optimized() {
    with_page(|scope, _| {
        assert_true(
            scope,
            "(() => { let s = 0; for (let i = 0; i < 200000; i++) s += el.nodeType; return s === 200000 })()",
        );
        let r = eval(scope, "(() => { const g = Object.getOwnPropertyDescriptor(Node.prototype, 'nodeType').get; for (let i = 0; i < 100000; i++) g.call(el); return g.call({}) })()");
        assert!(
            r.as_ref().unwrap_err().contains("Illegal invocation"),
            "{r:?}"
        );
    });
}

/// A panic in engine code under a V8 callback cannot unwind through V8's C ABI trampolines (it would
/// abort the process): the guard stops it at the callback, terminates the script and records it.
#[test]
fn a_panic_in_a_callback_terminates_the_script_instead_of_aborting() {
    with_page(|scope, mock| {
        // A slow callback: `closest` is not implemented by the mock (its stub panics). The script
        // stops at its next interrupt check (a call, a loop's back edge); until then no engine code
        // runs for it (the mock logs `scroll`).
        let before = mock.log.borrow().len();
        let r = eval(
            scope,
            "(() => { try { el.closest('div') } catch (e) {} el.scroll(1, 2); while (true) {} })()",
        );
        assert!(r.is_err(), "the script must stop: {r:?}");
        assert_eq!(mock.log.borrow().len(), before);
        let failure = snowlark_js::guard::take().expect("the panic is recorded");
        assert!(failure.contains("Element.closest"), "{failure}");
        scope.cancel_terminate_execution();
        assert_true(scope, "el.nodeType === 1");

        // A Fast API getter, once optimized code calls it directly.
        mock.node_type_panics_after.set(Some(150_000));
        let r = eval(
            scope,
            "(() => { let s = 0; for (let i = 0; i < 200000; i++) s += el.nodeType; return s })()",
        );
        assert!(r.is_err(), "{r:?}");
        assert_eq!(
            snowlark_js::guard::take().as_deref(),
            Some("nodeType failed")
        );
        scope.cancel_terminate_execution();
        mock.node_type_panics_after.set(None);
        assert_true(scope, "el.nodeType === 1");

        // After a failure, no more engine code runs from script until the failure is taken.
        snowlark_js::guard::record("earlier".into());
        let before = mock.log.borrow().len();
        let r = eval(scope, "el.scroll(1, 2); while (true) {}");
        assert!(r.is_err(), "{r:?}");
        assert_eq!(mock.log.borrow().len(), before);
        assert_eq!(snowlark_js::guard::take().as_deref(), Some("earlier"));
        scope.cancel_terminate_execution();
    });
}
