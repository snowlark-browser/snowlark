// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Legacy platform objects (indexed and named properties through interceptors), constructors with
//! dictionaries, `[SameObject]`, `[LegacyUnforgeable]`, `[LegacyOverrideBuiltIns]` and DOMException.

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
    mock.nodes.borrow_mut()[0]
        .attrs
        .push(("id".into(), "foo".into()));
    let coll = mock.add_collection(InterfaceId::HTMLCollection, vec![div, img]);
    let list = mock.add_collection(InterfaceId::NodeList, vec![div, img]);
    snowlark_bindings::install(scope, mock.clone(), rt::InstallOptions::default())
        .expect("install");
    let global = context.global(scope);
    for (name, h) in [("el", div), ("img", img), ("coll", coll), ("list", list)] {
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
fn indexed_and_named_properties() {
    with_page(|scope, _| {
        assert_true(
            scope,
            "coll.length === 2 && coll[0] === el && coll[1] === img && coll[2] === undefined",
        );
        assert_true(
            scope,
            "coll.item(5) === null && coll.item(0) === el && coll.namedItem('foo') === el",
        );
        assert_true(
            scope,
            "coll.foo === el && 'foo' in coll && !('bar' in coll) && 0 in coll && !(2 in coll)",
        );
        // [LegacyUnenumerableNamedProperties]: indices enumerate, names do not.
        assert_true(scope, "Object.keys(coll).join() === '0,1'");
        // Named properties never shadow the prototype (no [LegacyOverrideBuiltIns]).
        assert_true(scope, "typeof coll.item === 'function'");
        // No indexed setter: assignment is ignored, and throws in strict code.
        assert_true(scope, "(coll[0] = 5, coll[0] === el)");
        assert_true(
            scope,
            "(() => { 'use strict'; try { coll[0] = 5; return false } catch (e) { return e instanceof TypeError } })()",
        );
        assert_true(scope, "delete coll[7] && !delete coll[0]");
        assert_true(
            scope,
            "(() => { 'use strict'; try { delete coll[0]; return false } catch (e) { return e instanceof TypeError } })()",
        );
        // Value iterator: the Array.prototype functions.
        assert_true(
            scope,
            "NodeList.prototype.forEach === Array.prototype.forEach && NodeList.prototype[Symbol.iterator] === Array.prototype.values",
        );
        assert_true(scope, "[...list].length === 2 && [...list][1] === img");
        assert_true(
            scope,
            "list instanceof NodeList && !(list instanceof HTMLCollection)",
        );
    });
}

/// The names generated code keeps per isolate (`rt::Name`: dictionary members, `[SameObject]` keys,
/// a callback interface's operation): each isolate makes its own, so every one after the first reads
/// and keeps values as the first does, and a member is still read from the dictionary on every call.
#[test]
fn kept_names_work_in_every_isolate() {
    for _ in 0..3 {
        with_page(|scope, mock| {
            assert_true(scope, "el.dataset === el.dataset");
            assert_true(
                scope,
                "new Event('x', { bubbles: true }).bubbles && !new Event('x', { bubbles: false }).bubbles",
            );
            assert_true(
                scope,
                "(() => { let n = 0; const d = { get bubbles() { n++; return true } }; new Event('x', d); new Event('x', d); return n === 2 })()",
            );
            assert_true(
                scope,
                "(() => { let seen = 0; el.addEventListener('k', { handleEvent() { seen++ } }); el.dispatchEvent(new Event('k')); el.dispatchEvent(new Event('k')); return seen === 2 })()",
            );
            assert_eq!(
                mock.log.borrow().iter().filter(|l| *l == "dataset").count(),
                1,
                "[SameObject] caches the result in this isolate"
            );
        });
    }
}

#[test]
fn named_setter_deleter_and_override_builtins() {
    with_page(|scope, mock| {
        assert_true(
            scope,
            "el.dataset === el.dataset && el.dataset instanceof DOMStringMap",
        );
        assert_eq!(
            mock.log.borrow().iter().filter(|l| *l == "dataset").count(),
            1,
            "[SameObject] caches the result"
        );
        assert_true(scope, "(el.dataset.x = 'y', el.dataset.x === 'y')");
        assert_eq!(
            mock.attr(rt::Handle::new(0), "data-x").as_deref(),
            Some("y")
        );
        assert_true(scope, "Object.keys(el.dataset).join() === 'x'");
        assert_true(scope, "(delete el.dataset.x, el.dataset.x === undefined)");
        // [LegacyOverrideBuiltIns]: a named property wins over the prototype's toString.
        assert_true(
            scope,
            "(el.dataset.toString = 'z', el.dataset.toString === 'z')",
        );
    });
}

#[test]
fn callback_interfaces_and_unions() {
    with_page(|scope, _| {
        // A function listener gets the callback this value (the target); an object listener is called
        // through its handleEvent with itself as this.
        assert_true(
            scope,
            "(() => { let seen = []; el.addEventListener('x', function (e) { seen.push(this === el && e.type) }); const o = { handleEvent(e) { seen.push(this === o && e.type) } }; el.addEventListener('x', o, { once: true }); el.addEventListener('y', () => seen.push('no'), true); el.dispatchEvent(new Event('x')); return seen.join() === 'x,x' })()",
        );
        // An object without a callable handleEvent: the TypeError reaches the dispatcher.
        let r = eval(
            scope,
            "el.addEventListener('z', {}); el.dispatchEvent(new Event('z'))",
        );
        assert!(r.as_ref().unwrap_err().contains("handleEvent"), "{r:?}");
        // A listener's exception propagates as itself.
        let r = eval(scope, "el.addEventListener('w', () => { throw new RangeError('boom') }); el.dispatchEvent(new Event('w'))");
        assert!(
            r.as_ref().unwrap_err().contains("RangeError: boom"),
            "{r:?}"
        );
        // Non-callable, non-object listeners are rejected by the conversion.
        let r = eval(scope, "el.addEventListener('v', 5)");
        assert!(
            r.as_ref().unwrap_err().contains("is not an object"),
            "{r:?}"
        );
        // The options union takes a boolean or a dictionary; null is the dictionary's defaults.
        assert_true(scope, "el.addEventListener('u', null, null) === undefined");
    });
}

#[test]
fn static_members() {
    with_page(|scope, _| {
        assert_true(scope, "typeof AbortSignal.abort === 'function' && AbortSignal.abort.length === 0 && AbortSignal.timeout.length === 1");
        assert_true(scope, "!('abort' in AbortSignal.prototype) || typeof AbortSignal.prototype.abort !== 'function'");
    });
}

#[test]
fn constructors_dictionaries_and_unforgeable() {
    with_page(|scope, _| {
        assert_true(
            scope,
            "(e => e instanceof Event && e.type === 'x' && e.bubbles)(new Event('x', { bubbles: true }))",
        );
        assert_true(
            scope,
            "!new Event('x').bubbles && !new Event('x', null).bubbles",
        );
        assert_true(
            scope,
            "(() => { try { Event('x'); return false } catch (e) { return e instanceof TypeError } })()",
        );
        assert_true(
            scope,
            "(() => { try { new Event('x', 5); return false } catch (e) { return e instanceof TypeError } })()",
        );
        // [LegacyUnforgeable] isTrusted is an own, non-configurable accessor of each instance.
        assert_true(
            scope,
            "(d => !!d && !d.configurable && typeof d.get === 'function')(Object.getOwnPropertyDescriptor(new Event('x'), 'isTrusted'))",
        );
        assert_true(scope, "!('isTrusted' in Event.prototype)");
        // Subclassing keeps the subclass prototype.
        assert_true(
            scope,
            "(() => { class My extends Event {} const m = new My('q'); return m instanceof My && m.type === 'q' })()",
        );
        assert_true(
            scope,
            "(e => e.name === 'NotFoundError' && e.code === 8 && e.message === 'm')(new DOMException('m', 'NotFoundError'))",
        );
        assert_true(
            scope,
            "Object.prototype.toString.call(new DOMException()) === '[object DOMException]' && new DOMException().name === 'Error'",
        );
    });
}

#[test]
fn event_handler_attributes() {
    with_page(|scope, mock| {
        assert_true(scope, "el.onclick === null");
        assert_true(
            scope,
            "(() => { const f = () => 1; el.onclick = f; return el.onclick === f })()",
        );
        // [LegacyTreatNonObjectAsNull]: a non-object is null; any object is kept.
        assert_true(scope, "(el.onclick = 5, el.onclick === null)");
        assert_true(
            scope,
            "(() => { const o = {}; el.onkeydown = o; return el.onkeydown === o })()",
        );
        assert!(mock.log.borrow().iter().any(|l| l == "handler:click"));
        assert_true(scope, "(d => typeof d.get === 'function' && typeof d.set === 'function' && d.enumerable)(Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'onclick'))");
    });
}

#[test]
fn namespace_objects() {
    with_page(|scope, _| {
        assert_true(
            scope,
            "typeof console === 'object' && Object.getPrototypeOf(console) === Object.prototype",
        );
        assert_true(
            scope,
            "Object.prototype.toString.call(console) === '[object console]'",
        );
        assert_true(scope, "(d => d.writable && d.configurable && !d.enumerable)(Object.getOwnPropertyDescriptor(globalThis, 'console'))");
        assert_true(scope, "typeof console.log === 'function' && console.log.length === 0 && console.count.length === 0");
        assert_true(scope, "typeof NodeFilter === 'function' && NodeFilter.SHOW_ELEMENT === 1 && NodeFilter.FILTER_SKIP === 3 && !('prototype' in NodeFilter)");
    });
}

/// A realm whose global object is made from the `[Global]` interface's template: its members are own
/// properties of the global, the prototype chain is Window's, and the global's handle is its receiver.
#[test]
fn global_object() {
    snowlark_js::init();
    let isolate = &mut v8::Isolate::new(Default::default());
    v8::scope!(let scope, isolate);
    let mock = Rc::new(Mock::default());
    let window = mock.add_element(InterfaceId::Window, "");
    {
        // Templates need a context to be built in; any will do.
        let bootstrap = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, bootstrap);
        snowlark_bindings::generated::init_isolate(
            scope,
            mock.clone(),
            rt::InstallOptions::default(),
        );
    }
    let global =
        snowlark_bindings::generated::global_template(scope).expect("a [Global] interface");
    let context = v8::Context::new(
        scope,
        v8::ContextOptions {
            global_template: Some(global),
            ..Default::default()
        },
    );
    let scope = &mut v8::ContextScope::new(scope, context);
    snowlark_bindings::generated::install_context::<Mock>(scope, Some(window)).expect("install");
    // The named properties object (WindowProperties) sits between Window.prototype and its parent
    // (Web IDL §3.7.4), and the global object's prototype cannot change (HTML §7.2.3.2).
    assert_true(
        scope,
        "Object.getPrototypeOf(globalThis) === Window.prototype",
    );
    assert_true(
        scope,
        "Object.getPrototypeOf(Window.prototype)[Symbol.toStringTag] === 'WindowProperties' && !Object.getPrototypeOf(Window.prototype).hasOwnProperty('addEventListener')",
    );
    assert_true(
        scope,
        "Object.getPrototypeOf(Object.getPrototypeOf(Window.prototype)) === EventTarget.prototype",
    );
    assert_true(
        scope,
        "(() => { try { Object.setPrototypeOf(globalThis, {}) } catch (e) { return e instanceof TypeError } })()",
    );
    assert_true(scope, "globalThis instanceof EventTarget");
    assert_true(
        scope,
        "Object.prototype.toString.call(globalThis) === '[object Window]'",
    );
    // Window's own members are on the global itself, inherited ones on the prototypes.
    assert_true(scope, "Object.getOwnPropertyNames(globalThis).includes('setTimeout') && !Window.prototype.hasOwnProperty('setTimeout')");
    assert_true(scope, "(d => !d.configurable && !!d.get)(Object.getOwnPropertyDescriptor(globalThis, 'document'))");
    assert_true(
        scope,
        "!globalThis.hasOwnProperty('addEventListener') && typeof addEventListener === 'function'",
    );
    // Unqualified calls and window-qualified calls both reach the engine with the global's handle.
    assert_true(
        scope,
        "(addEventListener('x', () => {}), globalThis.addEventListener('y', {}), true)",
    );
    let listeners = mock.listeners.borrow();
    assert_eq!(listeners.len(), 2);
    assert!(listeners.iter().all(|(h, _, _)| *h == window));
}
