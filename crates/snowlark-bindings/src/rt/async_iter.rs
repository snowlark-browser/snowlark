// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Asynchronous iterable declarations (Web IDL §3.7.10.2 and §3.7.10.3): default asynchronous
//! iterator objects, the asynchronous iterator prototype object's `next()` and `return()` with
//! their ongoing-promise chaining, and async sequences (§3.2.25).
//!
//! The interface's own steps (the asynchronous iterator initialization steps, "get the next
//! iteration result", "asynchronous iterator return") are the engine's; the generated code calls
//! them from the step functions it passes here.

use super::iter::iter_result;
use super::*;

const AIT_TARGET: &str = "snowlark:async-iterator.target";
const AIT_STATE: &str = "snowlark:async-iterator.state";
const AIT_IFACE: &str = "snowlark:async-iterator.interface";
const AIT_ONGOING: &str = "snowlark:async-iterator.ongoing";
const AIT_FINISHED: &str = "snowlark:async-iterator.finished";

fn private<'s>(scope: &v8::PinScope<'s, '_>, key: &str) -> v8::Local<'s, v8::Private> {
    let k = name(scope, key);
    v8::Private::for_api(scope, Some(k))
}

/// The "end of iteration" value: what an engine's "get the next iteration result" promise is
/// resolved with at the end (a symbol of the embedder's registry, out of script's reach).
pub fn end_of_iteration<'s>(scope: &v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
    let d = name(scope, "snowlark:end of iteration");
    v8::Symbol::for_api(scope, d).into()
}

/// Builds the asynchronous iterator prototype object template of an interface: class string
/// `"<Interface> AsyncIterator"`, `next`, `return` when the interface has asynchronous iterator
/// return steps, and `@@toStringTag`.
pub fn async_iterator_template(
    scope: &v8::PinScope<'_, '_>,
    iface: u16,
    class_name: &str,
    next: v8::Local<v8::FunctionTemplate>,
    ret: Option<v8::Local<v8::FunctionTemplate>>,
) {
    let t = v8::FunctionTemplate::builder(illegal_constructor).build(scope);
    t.set_class_name(name(scope, class_name));
    let proto = t.prototype_template(scope);
    operation(scope, &proto, "next", next, false);
    if let Some(r) = ret {
        operation(scope, &proto, "return", r, false);
    }
    to_string_tag(scope, proto, class_name);
    let st = state(scope);
    st.async_iterator_templates
        .borrow_mut()
        .insert(iface, v8::Global::new(scope, t));
}

/// Per context: the asynchronous iterator prototype object gets %AsyncIteratorPrototype% as its
/// [[Prototype]], and the interface prototype's `@@asyncIterator` is its `values` function.
pub fn async_iterator_install(
    scope: &mut v8::PinScope<'_, '_>,
    iface: u16,
    interface_object: v8::Local<v8::Function>,
) -> Option<()> {
    let t = {
        let st = state(scope);
        let g = st.async_iterator_templates.borrow().get(&iface).cloned()?;
        v8::Local::new(scope, g)
    };
    let f = t.get_function(scope)?;
    let proto_key = name(scope, "prototype");
    let iter_proto: v8::Local<v8::Object> = f.get(scope, proto_key.into())?.try_into().ok()?;
    // %AsyncIteratorPrototype% is the [[Prototype]] of %AsyncGeneratorPrototype%, the
    // [[Prototype]] of an async generator function's `prototype`.
    let source = name(scope, "(async function* () {}).prototype");
    let script = v8::Script::compile(scope, source, None)?;
    let generator_proto: v8::Local<v8::Object> = script.run(scope)?.try_into().ok()?;
    let async_generator_proto: v8::Local<v8::Object> =
        generator_proto.get_prototype(scope)?.try_into().ok()?;
    let async_iterator_proto = async_generator_proto.get_prototype(scope)?;
    iter_proto.set_prototype(scope, async_iterator_proto)?;
    // (an asynchronous iterator prototype object has `next` and `return` only: no `constructor`)
    let ctor = name(scope, "constructor");
    iter_proto.delete(scope, ctor.into())?;
    let proto: v8::Local<v8::Object> = interface_object
        .get(scope, proto_key.into())?
        .try_into()
        .ok()?;
    let values = proto.get(scope, name(scope, "values").into())?;
    let sym = v8::Symbol::get_async_iterator(scope);
    proto.define_own_property(scope, sym.into(), values, v8::PropertyAttribute::DONT_ENUM)?;
    Some(())
}

/// A new default asynchronous iterator object for `target`, with the engine's iterator `state`.
pub fn async_iterator_new<'s>(
    scope: &v8::PinScope<'s, '_>,
    target: v8::Local<'s, v8::Object>,
    iface: u16,
    iterator_state: Handle,
) -> Option<v8::Local<'s, v8::Object>> {
    let t = {
        let st = state(scope);
        let g = st.async_iterator_templates.borrow().get(&iface).cloned()?;
        v8::Local::new(scope, g)
    };
    let it = t.instance_template(scope).new_instance(scope)?;
    it.set_private(scope, private(scope, AIT_TARGET), target.into())?;
    let s = v8::Number::new(scope, iterator_state.get() as f64);
    it.set_private(scope, private(scope, AIT_STATE), s.into())?;
    let id = v8::Integer::new(scope, iface.into());
    it.set_private(scope, private(scope, AIT_IFACE), id.into())?;
    let f = v8::Boolean::new(scope, false);
    it.set_private(scope, private(scope, AIT_FINISHED), f.into())?;
    Some(it)
}

/// The iterator object `this` when it is a default asynchronous iterator object of `iface`.
fn iterator_of<'s>(
    scope: &v8::PinScope<'s, '_>,
    this: v8::Local<'s, v8::Value>,
    iface: u16,
) -> Option<v8::Local<'s, v8::Object>> {
    let obj: v8::Local<v8::Object> = this.try_into().ok()?;
    let id = obj.get_private(scope, private(scope, AIT_IFACE))?;
    (id.is_int32() && id.int32_value(scope)? == i32::from(iface)).then_some(obj)
}

/// An iterator's target object and engine state.
pub fn async_iterator_parts<'s>(
    scope: &v8::PinScope<'s, '_>,
    it: v8::Local<'s, v8::Object>,
) -> Option<(v8::Local<'s, v8::Object>, Handle)> {
    let target: v8::Local<v8::Object> = it
        .get_private(scope, private(scope, AIT_TARGET))?
        .try_into()
        .ok()?;
    let s = it
        .get_private(scope, private(scope, AIT_STATE))?
        .number_value(scope)?;
    Some((target, Handle::new(s as u64)))
}

fn finished(scope: &v8::PinScope<'_, '_>, it: v8::Local<v8::Object>) -> bool {
    it.get_private(scope, private(scope, AIT_FINISHED))
        .is_some_and(|v| v.is_true())
}

fn set_finished(scope: &v8::PinScope<'_, '_>, it: v8::Local<v8::Object>) {
    let t = v8::Boolean::new(scope, true);
    it.set_private(scope, private(scope, AIT_FINISHED), t.into());
}

fn set_ongoing(scope: &v8::PinScope<'_, '_>, it: v8::Local<v8::Object>, p: v8::Local<v8::Value>) {
    it.set_private(scope, private(scope, AIT_ONGOING), p);
}

fn rejected<'s>(scope: &v8::PinScope<'s, '_>, message: &str) -> Option<v8::Local<'s, v8::Value>> {
    let m = name(scope, message);
    let e = v8::Exception::type_error(scope, m);
    let r = v8::PromiseResolver::new(scope)?;
    r.reject(scope, e);
    Some(r.get_promise(scope).into())
}

fn resolved<'s>(
    scope: &v8::PinScope<'s, '_>,
    v: v8::Local<'s, v8::Value>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let r = v8::PromiseResolver::new(scope)?;
    r.resolve(scope, v);
    Some(r.get_promise(scope))
}

/// `{value, done: true}`.
fn done_result<'s>(
    scope: &v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Object> {
    let o = v8::Object::new(scope);
    let vk = name(scope, "value");
    let dk = name(scope, "done");
    o.create_data_property(scope, vk.into(), value);
    let t = v8::Boolean::new(scope, true);
    o.create_data_property(scope, dk.into(), t.into());
    o
}

/// The `next()` method of an asynchronous iterator prototype object: `steps` is the generated
/// nextSteps function (its data is the iterator object), run now or once the ongoing promise
/// settles.
pub fn async_iterator_next<'s, F>(
    scope: &mut v8::PinScope<'s, '_>,
    this: v8::Local<'s, v8::Value>,
    iface: u16,
    steps: F,
) -> Option<v8::Local<'s, v8::Value>>
where
    F: for<'a, 'i> Fn(
            &mut v8::PinScope<'a, 'i>,
            v8::FunctionCallbackArguments<'a>,
            v8::ReturnValue<'a, v8::Value>,
        ) + Copy,
{
    let Some(it) = iterator_of(scope, this, iface) else {
        return rejected(
            scope,
            "next() called on an object that is not an async iterator",
        );
    };
    let f = v8::Function::builder(guarded(steps))
        .data(it.into())
        .build(scope)?;
    let ongoing = it.get_private(scope, private(scope, AIT_ONGOING))?;
    let promise: v8::Local<v8::Value> = match v8::Local::<v8::Promise>::try_from(ongoing) {
        Ok(p) => p.then2(scope, f, f)?.into(),
        Err(_) => {
            let u = v8::undefined(scope).into();
            f.call(scope, u, &[])?
        }
    };
    set_ongoing(scope, it, promise);
    Some(promise)
}

/// The nextSteps of `next()` for the iterator `it`: `get_next` is the interface's "get the next
/// iteration result" given the target and the engine state, a promise for the next value or for
/// [`end_of_iteration`].
pub fn async_iterator_next_steps<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    it: v8::Local<'s, v8::Object>,
    get_next: impl FnOnce(
        &mut v8::PinScope<'s, '_>,
        v8::Local<'s, v8::Object>,
        Handle,
    ) -> Option<v8::Local<'s, v8::Promise>>,
) -> Option<v8::Local<'s, v8::Value>> {
    if finished(scope, it) {
        let r = iter_result(scope, None);
        return Some(resolved(scope, r.into())?.into());
    }
    let (target, s) = async_iterator_parts(scope, it)?;
    let next = get_next(scope, target, s)?;
    let on_fulfilled = v8::Function::builder(guarded(next_fulfilled))
        .data(it.into())
        .length(1)
        .build(scope)?;
    let on_rejected = v8::Function::builder(guarded(next_rejected))
        .data(it.into())
        .length(1)
        .build(scope)?;
    Some(next.then2(scope, on_fulfilled, on_rejected)?.into())
}

fn next_fulfilled(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Ok(it) = v8::Local::<v8::Object>::try_from(args.data()) else {
        return;
    };
    let null = v8::null(scope).into();
    set_ongoing(scope, it, null);
    let next = args.get(0);
    let end = end_of_iteration(scope);
    if next.strict_equals(end) {
        set_finished(scope, it);
        rv.set(iter_result(scope, None).into());
    } else {
        rv.set(iter_result(scope, Some(next)).into());
    }
}

fn next_rejected(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Ok(it) = v8::Local::<v8::Object>::try_from(args.data()) else {
        return;
    };
    let null = v8::null(scope).into();
    set_ongoing(scope, it, null);
    set_finished(scope, it);
    scope.throw_exception(args.get(0));
}

/// The `return(value)` method of an asynchronous iterator prototype object: `steps` is the
/// generated returnSteps function (its data is `[iterator, value]`).
pub fn async_iterator_return<'s, F>(
    scope: &mut v8::PinScope<'s, '_>,
    this: v8::Local<'s, v8::Value>,
    iface: u16,
    value: v8::Local<'s, v8::Value>,
    steps: F,
) -> Option<v8::Local<'s, v8::Value>>
where
    F: for<'a, 'i> Fn(
            &mut v8::PinScope<'a, 'i>,
            v8::FunctionCallbackArguments<'a>,
            v8::ReturnValue<'a, v8::Value>,
        ) + Copy,
{
    let Some(it) = iterator_of(scope, this, iface) else {
        return rejected(
            scope,
            "return() called on an object that is not an async iterator",
        );
    };
    let data = v8::Array::new_with_elements(scope, &[it.into(), value]);
    let f = v8::Function::builder(guarded(steps))
        .data(data.into())
        .build(scope)?;
    let ongoing = it.get_private(scope, private(scope, AIT_ONGOING))?;
    let promise: v8::Local<v8::Value> = match v8::Local::<v8::Promise>::try_from(ongoing) {
        Ok(p) => p.then2(scope, f, f)?.into(),
        Err(_) => {
            let u = v8::undefined(scope).into();
            f.call(scope, u, &[])?
        }
    };
    set_ongoing(scope, it, promise);
    let on_fulfilled = v8::Function::builder(guarded(return_fulfilled))
        .data(value)
        .length(1)
        .build(scope)?;
    let p: v8::Local<v8::Promise> = promise.try_into().ok()?;
    Some(p.then(scope, on_fulfilled)?.into())
}

/// The returnSteps of `return()`: the iterator and value from the step function's data, and
/// `get_return` the interface's "asynchronous iterator return" given the target, the engine state
/// and the value.
pub fn async_iterator_return_steps<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    data: v8::Local<'s, v8::Value>,
    get_return: impl FnOnce(
        &mut v8::PinScope<'s, '_>,
        v8::Local<'s, v8::Object>,
        Handle,
        v8::Local<'s, v8::Value>,
    ) -> Option<v8::Local<'s, v8::Promise>>,
) -> Option<v8::Local<'s, v8::Value>> {
    let data: v8::Local<v8::Array> = data.try_into().ok()?;
    let it: v8::Local<v8::Object> = data.get_index(scope, 0)?.try_into().ok()?;
    let value = data.get_index(scope, 1)?;
    if finished(scope, it) {
        let r = done_result(scope, value);
        return Some(resolved(scope, r.into())?.into());
    }
    set_finished(scope, it);
    let (target, s) = async_iterator_parts(scope, it)?;
    Some(get_return(scope, target, s, value)?.into())
}

fn return_fulfilled(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let value = args.data();
    rv.set(done_result(scope, value).into());
}

/// The promise of a `JsPromise` an engine method returned.
pub fn promise_of<'s>(
    scope: &v8::PinScope<'s, '_>,
    p: &JsPromise,
) -> Option<v8::Local<'s, v8::Promise>> {
    let r = v8::Local::new(scope, &p.0);
    Some(r.get_promise(scope))
}

/// An IDL `async_sequence<T>` value (§3.2.25): the object, its @@asyncIterator or @@iterator
/// method, and whether that method is the sync one.
#[derive(Clone)]
pub struct AsyncSequence {
    pub object: JsValue,
    pub method: JsValue,
    pub sync: bool,
}

/// Converts `v` to an async sequence: a TypeError when it is not an object or has neither
/// method.
pub fn to_async_sequence(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
) -> Option<AsyncSequence> {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(v) else {
        return type_error(scope, "The value is not an object");
    };
    let get_method = |key: v8::Local<v8::Value>| -> Option<Option<v8::Local<v8::Value>>> {
        let m = obj.get(scope, key)?;
        if m.is_null_or_undefined() {
            return Some(None);
        }
        if !m.is_function() {
            return type_error(scope, "The iterator method is not callable");
        }
        Some(Some(m))
    };
    let async_sym = v8::Symbol::get_async_iterator(scope);
    if let Some(m) = get_method(async_sym.into())? {
        return Some(AsyncSequence {
            object: JsValue::new(scope, v),
            method: JsValue::new(scope, m),
            sync: false,
        });
    }
    let sync_sym = v8::Symbol::get_iterator(scope);
    match get_method(sync_sym.into())? {
        Some(m) => Some(AsyncSequence {
            object: JsValue::new(scope, v),
            method: JsValue::new(scope, m),
            sync: true,
        }),
        None => type_error(scope, "The value is not async iterable"),
    }
}

impl ToJs for AsyncSequence {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(v8::Local::new(scope, self.object.global()))
    }
}
