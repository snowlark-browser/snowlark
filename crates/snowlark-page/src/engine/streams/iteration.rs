// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Streams §4.2.5 (asynchronous iteration of a `ReadableStream`) and §4.9.1
//! ReadableStreamFromIterable (`ReadableStream.from()`), with the Web IDL async sequence steps it
//! uses (opening, getting the next value, closing; CreateAsyncFromSyncIterator for sync iterables).

use std::rc::Rc;

use snowlark_bindings::rt::{self, Cx, Handle, JsPromise, JsValue};

use super::super::Engine;
use super::readable::{NativeSource, ReadRequest, ReadSteps};
use super::{call, get, type_error, Deferred};

/// The "end of iteration" value.
fn end_of_iteration(cx: &mut Cx) -> JsValue {
    let scope = cx.scope();
    let v = rt::end_of_iteration(scope);
    JsValue::new(scope, v)
}

fn is_end(cx: &mut Cx, v: &JsValue) -> bool {
    let scope = cx.scope();
    let l = v8::Local::new(scope, v.global());
    l.strict_equals(rt::end_of_iteration(scope))
}

fn is_object(cx: &mut Cx, v: &JsValue) -> bool {
    let scope = cx.scope();
    v8::Local::new(scope, v.global()).is_object()
}

fn to_boolean(cx: &mut Cx, v: &JsValue) -> bool {
    let scope = cx.scope();
    v8::Local::new(scope, v.global()).boolean_value(scope)
}

fn is_nullish(cx: &mut Cx, v: &JsValue) -> bool {
    let scope = cx.scope();
    v8::Local::new(scope, v.global()).is_null_or_undefined()
}

fn as_function(cx: &mut Cx, v: &JsValue) -> Option<super::JsFn> {
    let scope = cx.scope();
    let l = v8::Local::new(scope, v.global());
    v8::Local::<v8::Function>::try_from(l)
        .ok()
        .map(|f| v8::Global::new(scope, f))
}

/// The reader of an async iterator's read requests ("get the next iteration result").
struct IteratorRead {
    promise: Deferred,
    reader: Handle,
}

impl ReadSteps for IteratorRead {
    fn chunk(self: Box<Self>, _e: &Engine, cx: &mut Cx, chunk: JsValue) {
        self.promise.resolve(cx, &chunk);
    }

    fn close(self: Box<Self>, e: &Engine, cx: &mut Cx) {
        e.reader_release(cx, self.reader);
        let end = end_of_iteration(cx);
        self.promise.resolve(cx, &end);
    }

    fn error(self: Box<Self>, e: &Engine, cx: &mut Cx, error: JsValue) {
        e.reader_release(cx, self.reader);
        self.promise.reject(cx, &error);
    }
}

impl Engine {
    /// The asynchronous iterator initialization steps of a `ReadableStream`: its state is the
    /// reader.
    pub(crate) fn readable_async_iterator_init(
        &self,
        cx: &mut Cx,
        stream: Handle,
        prevent_cancel: bool,
    ) -> rt::Result<Handle> {
        let reader = self.acquire_default_reader(cx, stream)?;
        self.rr(reader, |r| r.iterator_prevent_cancel = prevent_cancel);
        Ok(reader)
    }

    /// The "get the next iteration result" steps of a `ReadableStream`.
    pub(crate) fn readable_async_iterator_next(&self, cx: &mut Cx, reader: Handle) -> JsPromise {
        if self.rr(reader, |r| r.stream.is_none()) {
            let e = type_error(cx, "The iterator's reader was released");
            return Deferred::rejected(cx, &e).p;
        }
        let promise = Deferred::new(cx);
        self.reader_read(
            cx,
            reader,
            ReadRequest::Steps(Box::new(IteratorRead {
                promise: promise.clone(),
                reader,
            })),
        );
        promise.p
    }

    /// The asynchronous iterator return steps of a `ReadableStream`.
    pub(crate) fn readable_async_iterator_return(
        &self,
        cx: &mut Cx,
        reader: Handle,
        value: JsValue,
    ) -> JsPromise {
        let prevent_cancel = self.rr(reader, |r| r.iterator_prevent_cancel);
        let Some(stream) = self.rr(reader, |r| r.stream) else {
            return Deferred::resolved_undefined(cx).p;
        };
        if !prevent_cancel {
            // ReadableStreamReaderGenericCancel.
            let result = self.readable_stream_cancel(cx, stream, value);
            self.reader_release(cx, reader);
            let d = Deferred::new(cx);
            d.resolve(cx, &result);
            return d.p;
        }
        self.reader_release(cx, reader);
        Deferred::resolved_undefined(cx).p
    }

    /// ReadableStreamFromIterable: opens the async sequence and makes a stream pulling from it.
    pub(crate) fn readable_stream_from_iterable(
        &self,
        cx: &mut Cx,
        sequence: rt::AsyncSequence,
    ) -> Result<Handle, JsValue> {
        // "Open an async sequence": GetIteratorFromMethod.
        let method = as_function(cx, &sequence.method)
            .ok_or_else(|| type_error(cx, "The iterator method is not callable"))?;
        let iterator = call(cx, &method, &sequence.object, &[])?;
        if !is_object(cx, &iterator) {
            return Err(type_error(cx, "The iterator is not an object"));
        }
        let next = get(cx, &iterator, "next")?;
        let source = Rc::new(IterableSource {
            iterator,
            next,
            sync: sequence.sync,
        });
        Ok(self.create_readable_stream(cx, source, 0.0, false))
    }
}

/// The underlying source of `ReadableStream.from()`.
struct IterableSource {
    iterator: JsValue,
    next: JsValue,
    /// A sync iterator, seen through CreateAsyncFromSyncIterator.
    sync: bool,
}

impl IterableSource {
    /// "Get the next value of an async iterator": a promise for the value or the end of iteration.
    fn next_value(&self, e: &Engine, cx: &mut Cx) -> JsValue {
        let rejected = |cx: &mut Cx, err: JsValue| Deferred::rejected(cx, &err).value(cx);
        let Some(next) = as_function(cx, &self.next) else {
            let err = type_error(cx, "The iterator's next is not a function");
            return rejected(cx, err);
        };
        let result = match call(cx, &next, &self.iterator, &[]) {
            Ok(r) => r,
            Err(err) => return rejected(cx, err),
        };
        let out = Deferred::new(cx);
        if self.sync {
            // AsyncFromSyncIteratorContinuation: the iterator result, then its value awaited.
            if !is_object(cx, &result) {
                let err = type_error(cx, "The iterator result is not an object");
                return rejected(cx, err);
            }
            let done = match get(cx, &result, "done") {
                Ok(d) => to_boolean(cx, &d),
                Err(err) => return rejected(cx, err),
            };
            let value = match get(cx, &result, "value") {
                Ok(v) => v,
                Err(err) => return rejected(cx, err),
            };
            let d = out.clone();
            let iterator = self.iterator.clone();
            e.upon(
                cx,
                &value,
                Box::new(move |_, cx, r| match r {
                    Ok(_) if done => {
                        let end = end_of_iteration(cx);
                        d.resolve(cx, &end);
                    }
                    Ok(v) => d.resolve(cx, &v),
                    Err(err) => {
                        if !done {
                            // IteratorClose with a throw completion: `return()` is called and
                            // its outcome dropped.
                            if let Ok(ret) = get(cx, &iterator, "return") {
                                if let Some(f) = as_function(cx, &ret) {
                                    let _ = call(cx, &f, &iterator, &[]);
                                }
                            }
                        }
                        d.reject(cx, &err);
                    }
                }),
            );
        } else {
            let d = out.clone();
            e.upon(
                cx,
                &result,
                Box::new(move |_, cx, r| match r {
                    Ok(iter_result) => {
                        if !is_object(cx, &iter_result) {
                            let err = type_error(cx, "The iterator result is not an object");
                            d.reject(cx, &err);
                            return;
                        }
                        let done = match get(cx, &iter_result, "done") {
                            Ok(v) => to_boolean(cx, &v),
                            Err(err) => {
                                d.reject(cx, &err);
                                return;
                            }
                        };
                        if done {
                            let end = end_of_iteration(cx);
                            d.resolve(cx, &end);
                            return;
                        }
                        match get(cx, &iter_result, "value") {
                            Ok(v) => d.resolve(cx, &v),
                            Err(err) => d.reject(cx, &err),
                        }
                    }
                    Err(err) => d.reject(cx, &err),
                }),
            );
        }
        out.value(cx)
    }
}

impl NativeSource for IterableSource {
    fn pull(&self, e: &Engine, cx: &mut Cx, controller: Handle) -> JsValue {
        let next = self.next_value(e, cx);
        let out = Deferred::new(cx);
        let d = out.clone();
        e.upon(
            cx,
            &next,
            Box::new(move |e, cx, r| {
                match r {
                    Ok(v) if is_end(cx, &v) => e.readable_controller_close(cx, controller),
                    Ok(v) => {
                        let _ = e.readable_controller_enqueue(cx, controller, v);
                    }
                    Err(r) => e.readable_controller_error(cx, controller, r),
                }
                d.resolve_undefined(cx);
            }),
        );
        out.value(cx)
    }

    /// "Close an async iterator" with the reason.
    fn cancel(&self, e: &Engine, cx: &mut Cx, reason: JsValue) -> JsValue {
        let rejected = |cx: &mut Cx, err: JsValue| Deferred::rejected(cx, &err).value(cx);
        let ret = match get(cx, &self.iterator, "return") {
            Ok(r) => r,
            Err(err) => return rejected(cx, err),
        };
        if is_nullish(cx, &ret) {
            return Deferred::resolved_undefined(cx).value(cx);
        }
        let Some(ret) = as_function(cx, &ret) else {
            let err = type_error(cx, "The iterator's return is not a function");
            return rejected(cx, err);
        };
        let result = match call(cx, &ret, &self.iterator, &[reason]) {
            Ok(r) => r,
            Err(err) => return rejected(cx, err),
        };
        let out = Deferred::new(cx);
        let d = out.clone();
        if self.sync {
            // %AsyncFromSyncIteratorPrototype%.return: the result object, its value awaited.
            if !is_object(cx, &result) {
                let err = type_error(cx, "The iterator result is not an object");
                return rejected(cx, err);
            }
            let value = match get(cx, &result, "value") {
                Ok(v) => v,
                Err(err) => return rejected(cx, err),
            };
            e.upon(
                cx,
                &value,
                Box::new(move |_, cx, r| match r {
                    Ok(_) => d.resolve_undefined(cx),
                    Err(err) => d.reject(cx, &err),
                }),
            );
        } else {
            e.upon(
                cx,
                &result,
                Box::new(move |_, cx, r| match r {
                    Ok(v) => {
                        if is_object(cx, &v) {
                            d.resolve_undefined(cx);
                        } else {
                            let err =
                                type_error(cx, "The iterator's return result is not an object");
                            d.reject(cx, &err);
                        }
                    }
                    Err(err) => d.reject(cx, &err),
                }),
            );
        }
        out.value(cx)
    }
}
