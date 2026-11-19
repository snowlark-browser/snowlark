// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Streams Standard (https://streams.spec.whatwg.org/): readable streams with default and byte
//! controllers and default readers (§4), tee and pipes, writable streams (§5), transform streams
//! (§6), the queuing strategies (§7), from the Encoding Standard `TextDecoderStream` and
//! `TextEncoderStream`, and from the Compression Standard `CompressionStream` and
//! `DecompressionStream`. Written from the spec's abstract operations, named after them.
//!
//! Promises: each promise the spec keeps (a reader's `closed`, a writer's `ready`...) is a
//! [`Deferred`] whose state is tracked here, since some algorithms ask whether it is still pending.
//! Reacting to a promise ("upon fulfillment/rejection") attaches two internal functions whose data
//! names a Rust reaction ([`Engine::upon`]); "queue a microtask" is a reaction to a resolved promise.
//!
//! Not implemented yet: transferring streams. Stream objects stay alive for the page's lifetime, as
//! other platform objects without a garbage collection class do.

pub(crate) mod bytes;
mod compression;
mod iteration;
pub(crate) mod readable;
mod transform;
mod writable;

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use snowlark_bindings::generated::QueuingStrategy;
use snowlark_bindings::rt::{self, Cx, Handle, JsPromise, JsValue};

use super::Engine;

pub(crate) use bytes::ByobRequestData;
pub(crate) use readable::{ReadableControllerData, ReadableStreamData, ReaderData};
pub(crate) use transform::{TransformControllerData, TransformStreamData};
pub(crate) use writable::{WritableControllerData, WritableStreamData, WriterData};

/// A step to run later.
pub(crate) type Then = Box<dyn FnOnce(&Engine, &mut Cx)>;
/// An underlying source's or sink's start algorithm (given the controller).
pub(crate) type StartAlgorithm =
    Box<dyn FnOnce(&Engine, &mut Cx, Handle) -> Result<JsValue, JsValue>>;
/// What gets the bytes a stream was read to the end for.
pub(crate) type BytesDone = Box<dyn FnOnce(&Engine, &mut Cx, Result<Vec<u8>, JsValue>)>;

/// A reaction to a settled promise: `Ok` with the fulfillment value, `Err` with the reason.
pub(crate) type Reaction = Box<dyn FnOnce(&Engine, &mut Cx, Result<JsValue, JsValue>)>;

/// The reactions waiting for their promise, by id.
#[derive(Default)]
pub struct StreamsState {
    next: u32,
    reactions: rustc_hash::FxHashMap<u32, Reaction>,
    /// `ByteLengthQueuingStrategy`'s and `CountQueuingStrategy`'s size functions by realm (the
    /// realm's global handle) and kind: one function object per realm (§7.2.2, §7.3.2).
    pub(crate) size_functions: HashMap<(u64, bool), v8::Global<v8::Function>>,
    /// Abort algorithms streams added to abort signals (pipes with a `signal` option).
    pub(crate) abort_algorithms: Vec<(Handle, Option<readable::StreamAbort>)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Settled {
    Pending,
    Fulfilled,
    Rejected,
}

/// A promise the streams keep, with its state.
#[derive(Clone)]
pub(crate) struct Deferred {
    pub p: JsPromise,
    state: Rc<Cell<Settled>>,
}

impl Deferred {
    pub fn new(cx: &mut Cx) -> Deferred {
        Deferred {
            p: JsPromise::new(cx).expect("a promise resolver"),
            state: Rc::new(Cell::new(Settled::Pending)),
        }
    }

    /// "A promise resolved with" `v`.
    pub fn resolved(cx: &mut Cx, v: &JsValue) -> Deferred {
        let d = Deferred::new(cx);
        d.resolve(cx, v);
        d
    }

    /// A promise resolved with undefined.
    pub fn resolved_undefined(cx: &mut Cx) -> Deferred {
        let u = undefined(cx);
        Deferred::resolved(cx, &u)
    }

    /// "A promise rejected with" `e`.
    pub fn rejected(cx: &mut Cx, e: &JsValue) -> Deferred {
        let d = Deferred::new(cx);
        d.reject(cx, e);
        d
    }

    /// Whether two kept promises are the same.
    pub fn same(&self, other: &Deferred) -> bool {
        Rc::ptr_eq(&self.state, &other.state)
    }

    pub fn pending(&self) -> bool {
        self.state.get() == Settled::Pending
    }

    pub fn resolve(&self, cx: &mut Cx, v: &JsValue) {
        if self.pending() {
            self.state.set(Settled::Fulfilled);
            self.p.resolve(cx, v.clone());
        }
    }

    pub fn resolve_undefined(&self, cx: &mut Cx) {
        let u = undefined(cx);
        self.resolve(cx, &u);
    }

    pub fn reject(&self, cx: &mut Cx, e: &JsValue) {
        if self.pending() {
            self.state.set(Settled::Rejected);
            self.p.reject_value(cx, e);
        }
    }

    /// "Set promise.[[PromiseIsHandled]] to true".
    pub fn mark_handled(&self, cx: &mut Cx) {
        self.p.mark_handled(cx);
    }

    /// The promise object.
    pub fn value(&self, cx: &mut Cx) -> JsValue {
        self.p.promise(cx)
    }
}

pub(crate) fn undefined(cx: &mut Cx) -> JsValue {
    rt::undefined_value(cx.scope())
}

/// The JS value of an exception the engine raises.
pub(crate) fn error_value(cx: &mut Cx, e: rt::Error) -> JsValue {
    let scope = cx.scope();
    match rt::error_value(scope, e) {
        Some(v) => JsValue::new(scope, v),
        None => rt::undefined_value(scope),
    }
}

pub(crate) fn type_error(cx: &mut Cx, message: &'static str) -> JsValue {
    error_value(cx, rt::Error::Type(message.into()))
}

/// A platform object's wrapper as a JS value.
pub(crate) fn handle_value(cx: &mut Cx, h: Handle) -> JsValue {
    let scope = cx.scope();
    match rt::wrap(scope, h) {
        Some(w) => JsValue::new(scope, w.into()),
        None => rt::undefined_value(scope),
    }
}

/// A JS function the streams hold (an underlying source's `pull`...).
pub(crate) type JsFn = v8::Global<v8::Function>;

/// The exception a TryCatch caught (undefined when execution was terminated), cleared.
fn caught(tc: &mut v8::PinScope<'_, '_>, exception: Option<v8::Local<v8::Value>>) -> JsValue {
    match exception {
        Some(e) => JsValue::new(tc, e),
        None => rt::undefined_value(tc),
    }
}

/// Calls `f` with `this` and `args`; `Err` with what it threw.
pub(crate) fn call(
    cx: &mut Cx,
    f: &JsFn,
    this: &JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsValue> {
    let func = v8::Local::new(cx.scope(), f);
    rt::in_realm_of_callback(cx, func.into(), None, |cx| {
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let recv = v8::Local::new(tc, this.global());
        let argv: Vec<v8::Local<v8::Value>> = args
            .iter()
            .map(|a| v8::Local::new(tc, a.global()))
            .collect();
        match func.call(tc, recv, &argv) {
            Some(v) => Ok(JsValue::new(tc, v)),
            None => {
                let ex = tc.exception();
                let e = caught(tc, ex);
                if !tc.has_terminated() {
                    tc.reset();
                }
                Err(e)
            }
        }
    })
}

/// `obj[name]` (`undefined` for a null or undefined `obj`); `Err` with what a getter threw.
pub(crate) fn get(cx: &mut Cx, obj: &JsValue, name: &str) -> Result<JsValue, JsValue> {
    let scope = cx.scope();
    v8::tc_scope!(let tc, scope);
    let o = v8::Local::new(tc, obj.global());
    if o.is_null_or_undefined() {
        return Ok(rt::undefined_value(tc));
    }
    let Some(o) = o.to_object(tc) else {
        let ex = tc.exception();
        let e = caught(tc, ex);
        tc.reset();
        return Err(e);
    };
    let Some(key) = v8::String::new(tc, name) else {
        return Ok(rt::undefined_value(tc));
    };
    match o.get(tc, key.into()) {
        Some(v) => Ok(JsValue::new(tc, v)),
        None => {
            let ex = tc.exception();
            let e = caught(tc, ex);
            if !tc.has_terminated() {
                tc.reset();
            }
            Err(e)
        }
    }
}

/// Web IDL's conversion of a dictionary's callback member: `undefined` is absent, a callable is
/// kept, anything else is a TypeError. `obj` is `undefined` or `null` for an empty dictionary.
pub(crate) fn get_callback(
    cx: &mut Cx,
    obj: &JsValue,
    name: &str,
) -> Result<Option<JsFn>, JsValue> {
    let v = get(cx, obj, name)?;
    let scope = cx.scope();
    let local = v8::Local::new(scope, v.global());
    if local.is_undefined() {
        return Ok(None);
    }
    match v8::Local::<v8::Function>::try_from(local) {
        Ok(f) => Ok(Some(v8::Global::new(scope, f))),
        Err(_) => Err(error_value(
            cx,
            rt::Error::Type(format!("The '{name}' member is not a function").into()),
        )),
    }
}

/// `{value, done}` (CreateIterResultObject, what a default reader's read requests resolve with).
pub(crate) fn iter_result(cx: &mut Cx, value: &JsValue, done: bool) -> JsValue {
    let scope = cx.scope();
    let o = v8::Object::new(scope);
    let v = v8::Local::new(scope, value.global());
    if let (Some(kv), Some(kd)) = (
        v8::String::new(scope, "value"),
        v8::String::new(scope, "done"),
    ) {
        let d = v8::Boolean::new(scope, done);
        o.create_data_property(scope, kv.into(), v);
        o.create_data_property(scope, kd.into(), d.into());
    }
    JsValue::new(scope, o.into())
}

/// A JS array of values.
pub(crate) fn array(cx: &mut Cx, values: &[JsValue]) -> JsValue {
    let scope = cx.scope();
    let elements: Vec<v8::Local<v8::Value>> = values
        .iter()
        .map(|v| v8::Local::new(scope, v.global()))
        .collect();
    let a = v8::Array::new_with_elements(scope, &elements);
    JsValue::new(scope, a.into())
}

/// §7.4 ExtractHighWaterMark.
pub(crate) fn extract_high_water_mark(strategy: &QueuingStrategy, default: f64) -> rt::Result<f64> {
    match strategy.high_water_mark {
        None => Ok(default),
        Some(h) if h.is_nan() || h < 0.0 => {
            Err(rt::Error::Range("The high water mark is invalid".into()))
        }
        Some(h) => Ok(h),
    }
}

/// Reaction callbacks: the data is the reaction id times two, plus one for rejection.
fn reaction_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data().int32_value(scope).unwrap_or(0) as u32;
    let (id, rejected) = (data >> 1, data & 1 == 1);
    let value = JsValue::new(scope, args.get(0));
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    let reaction = engine.streams.borrow_mut().reactions.remove(&id);
    let Some(reaction) = reaction else {
        return;
    };
    let mut cx = Cx::new(scope);
    reaction(
        engine,
        &mut cx,
        if rejected { Err(value) } else { Ok(value) },
    );
}

impl Engine {
    /// "Upon fulfillment/rejection" of the promise "resolved with" `value` (a promise, or any value
    /// as `Promise.resolve` takes it): `f` runs once, in a microtask, with the outcome.
    pub(crate) fn upon(&self, cx: &mut Cx, value: &JsValue, f: Reaction) {
        let id = {
            let mut s = self.streams.borrow_mut();
            s.next = (s.next + 1) & 0x3FFF_FFFF;
            let id = s.next;
            s.reactions.insert(id, f);
            id
        };
        let scope = cx.scope();
        let v = v8::Local::new(scope, value.global());
        // Web IDL "react to a promise": a promise is reacted to as it is (PerformPromiseThen, no
        // lookup of a `then` a page may have replaced); another value is resolved into a new one.
        let promise = match v8::Local::<v8::Promise>::try_from(v) {
            Ok(p) => p,
            Err(_) => {
                let Some(resolver) = v8::PromiseResolver::new(scope) else {
                    return;
                };
                resolver.resolve(scope, v);
                resolver.get_promise(scope)
            }
        };
        let ok_data = v8::Integer::new(scope, (id << 1) as i32);
        let err_data = v8::Integer::new(scope, ((id << 1) | 1) as i32);
        let (Some(ok), Some(err)) = (
            v8::Function::builder(rt::guarded(reaction_callback))
                .data(ok_data.into())
                .build(scope),
            v8::Function::builder(rt::guarded(reaction_callback))
                .data(err_data.into())
                .build(scope),
        ) else {
            return;
        };
        let _ = promise.then2(scope, ok, err);
    }

    /// [`Engine::upon`] a kept promise.
    pub(crate) fn upon_deferred(&self, cx: &mut Cx, d: &Deferred, f: Reaction) {
        let v = d.value(cx);
        self.upon(cx, &v, f);
    }

    /// "Queue a microtask" running `f`.
    pub(crate) fn queue_stream_microtask(
        &self,
        cx: &mut Cx,
        f: impl FnOnce(&Engine, &mut Cx) + 'static,
    ) {
        let u = undefined(cx);
        self.upon(cx, &u, Box::new(move |e, cx, _| f(e, cx)));
    }

    /// Invokes an optional callback with `this` and `args` and promise-resolves the result: a
    /// promise for undefined without a callback, one rejected with what it threw.
    pub(crate) fn promise_call(
        &self,
        cx: &mut Cx,
        f: Option<&JsFn>,
        this: &JsValue,
        args: &[JsValue],
    ) -> JsValue {
        let d = match f {
            None => Deferred::resolved_undefined(cx),
            Some(f) => match call(cx, f, this, args) {
                Ok(v) => Deferred::resolved(cx, &v),
                Err(e) => Deferred::rejected(cx, &e),
            },
        };
        d.value(cx)
    }
}
