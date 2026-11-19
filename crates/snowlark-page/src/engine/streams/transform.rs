// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Streams §6 `TransformStream` and its default controller, §7 the queuing strategies, and the
//! Encoding Standard's `TextDecoderStream` and `TextEncoderStream` (§7.4, §7.5) as transform
//! streams with engine transformers.

use std::cell::RefCell;
use std::rc::Rc;

use snowlark_bindings::generated::{
    ByteLengthQueuingStrategyImpl, CountQueuingStrategyImpl, Function, GenericTransformStreamImpl,
    QueuingStrategy, QueuingStrategyInit, TextDecoderOptions, TextDecoderStreamImpl,
    TextEncoderStreamImpl, TransformStreamDefaultControllerImpl, TransformStreamImpl,
};
use snowlark_bindings::rt::{self, Cx, Handle, JsObject, JsValue};

use super::super::{Engine, Object};
use super::readable::{extract_size, throw, uint8_value, view_bytes, NativeSource, RState};
use super::writable::{NativeSink, WState};
use super::{
    call, extract_high_water_mark, get, get_callback, handle_value, type_error, undefined,
    Deferred, JsFn,
};

pub struct TransformStreamData {
    readable: Handle,
    writable: Handle,
    backpressure: Option<bool>,
    backpressure_change: Option<Deferred>,
    controller: Option<Handle>,
}

/// A transformer the engine provides.
pub(crate) trait NativeTransformer {
    /// Transforms a chunk (enqueuing through the controller); `Err` rejects the write.
    fn transform(
        &self,
        e: &Engine,
        cx: &mut Cx,
        controller: Handle,
        chunk: JsValue,
    ) -> Result<(), JsValue>;
    fn flush(&self, e: &Engine, cx: &mut Cx, controller: Handle) -> Result<(), JsValue>;
}

enum Transformer {
    Js {
        this: JsValue,
        transform: Option<JsFn>,
        flush: Option<JsFn>,
        cancel: Option<JsFn>,
    },
    Native(Rc<dyn NativeTransformer>),
}

pub struct TransformControllerData {
    stream: Handle,
    transformer: Option<Transformer>,
    finish: Option<Deferred>,
}

impl Engine {
    fn ts<R>(&self, h: Handle, f: impl FnOnce(&mut TransformStreamData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::TransformStream(s) => f(s),
            _ => panic!("not a TransformStream"),
        }
    }

    fn tc<R>(&self, h: Handle, f: impl FnOnce(&mut TransformControllerData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::TransformController(c) => f(c),
            _ => panic!("not a TransformStreamDefaultController"),
        }
    }

    /// The readable side's controller.
    fn transform_readable_controller(&self, s: Handle) -> Option<Handle> {
        let r = self.ts(s, |s| s.readable);
        self.rs(r, |r| r.controller)
    }

    /// InitializeTransformStream, then the controller's setup (SetUpTransformStreamDefaultController).
    #[allow(clippy::too_many_arguments)]
    fn initialize_transform_stream(
        &self,
        cx: &mut Cx,
        start: &Deferred,
        writable_hwm: f64,
        writable_size: Option<JsFn>,
        readable_hwm: f64,
        readable_size: Option<JsFn>,
        transformer: Transformer,
    ) -> (Handle, Handle) {
        // (the sides refer to the stream through their algorithms: made first, filled after)
        let stream = self.alloc(Object::TransformStream(Box::new(TransformStreamData {
            readable: Handle::new(Handle::MAX),
            writable: Handle::new(Handle::MAX),
            backpressure: None,
            backpressure_change: None,
            controller: None,
        })));
        let sink = Rc::new(TransformSink {
            stream,
            start: start.clone(),
        });
        let writable = self.create_writable_stream(cx, sink, writable_hwm, writable_size);
        let source = Rc::new(TransformSource {
            stream,
            start: start.clone(),
        });
        let readable = self.create_readable_stream_sized(cx, source, readable_hwm, readable_size);
        self.ts(stream, |s| {
            s.readable = readable;
            s.writable = writable;
        });
        self.transform_set_backpressure(cx, stream, true);
        let c = self.alloc(Object::TransformController(Box::new(
            TransformControllerData {
                stream,
                transformer: Some(transformer),
                finish: None,
            },
        )));
        self.ts(stream, |s| s.controller = Some(c));
        (stream, c)
    }

    /// TransformStreamSetBackpressure.
    fn transform_set_backpressure(&self, cx: &mut Cx, s: Handle, bp: bool) {
        if let Some(p) = self.ts(s, |s| s.backpressure_change.take()) {
            p.resolve_undefined(cx);
        }
        let d = Deferred::new(cx);
        self.ts(s, |s| {
            s.backpressure_change = Some(d);
            s.backpressure = Some(bp);
        });
    }

    /// TransformStreamError.
    fn transform_error(&self, cx: &mut Cx, s: Handle, e: JsValue) {
        if let Some(rc) = self.transform_readable_controller(s) {
            self.readable_controller_error(cx, rc, e.clone());
        }
        self.transform_error_writable_and_unblock_write(cx, s, e);
    }

    /// TransformStreamErrorWritableAndUnblockWrite.
    fn transform_error_writable_and_unblock_write(&self, cx: &mut Cx, s: Handle, e: JsValue) {
        if let Some(c) = self.ts(s, |s| s.controller) {
            self.tc(c, |c| c.transformer = None);
        }
        let w = self.ts(s, |s| s.writable);
        if let Some(wc) = self.writable_controller_of(w) {
            self.writable_controller_error_if_needed(cx, wc, e);
        }
        self.transform_unblock_write(cx, s);
    }

    /// TransformStreamUnblockWrite.
    fn transform_unblock_write(&self, cx: &mut Cx, s: Handle) {
        if self.ts(s, |s| s.backpressure) == Some(true) {
            self.transform_set_backpressure(cx, s, false);
        }
    }

    /// TransformStreamDefaultControllerEnqueue: `Err` with the exception to throw.
    pub(crate) fn transform_controller_enqueue(
        &self,
        cx: &mut Cx,
        c: Handle,
        chunk: JsValue,
    ) -> Result<(), JsValue> {
        let s = self.tc(c, |c| c.stream);
        let Some(rc) = self.transform_readable_controller(s) else {
            return Ok(());
        };
        let readable = self.ts(s, |s| s.readable);
        let can = self.rc(rc, |c| !c.close_requested)
            && self.readable_state(readable) == RState::Readable;
        if !can {
            return Err(type_error(cx, "The readable side can't take more chunks"));
        }
        if let Err(e) = self.readable_controller_enqueue(cx, rc, chunk) {
            self.transform_error_writable_and_unblock_write(cx, s, e);
            return Err(self.readable_stored_error(cx, readable));
        }
        // ReadableStreamDefaultControllerHasBackpressure: !ShouldCallPull.
        let desired = self.readable_controller_desired_size(rc);
        let bp = !desired.is_some_and(|d| d > 0.0);
        if Some(bp) != self.ts(s, |s| s.backpressure) && bp {
            self.transform_set_backpressure(cx, s, true);
        }
        Ok(())
    }

    /// TransformStreamDefaultControllerTerminate.
    fn transform_controller_terminate(&self, cx: &mut Cx, c: Handle) {
        let s = self.tc(c, |c| c.stream);
        if let Some(rc) = self.transform_readable_controller(s) {
            self.readable_controller_close(cx, rc);
        }
        let e = type_error(cx, "The transform stream was terminated");
        self.transform_error_writable_and_unblock_write(cx, s, e);
    }

    /// TransformStreamDefaultControllerPerformTransform.
    fn transform_perform(&self, cx: &mut Cx, c: Handle, chunk: JsValue) -> JsValue {
        enum T {
            Js(JsValue, Option<JsFn>),
            Native(Rc<dyn NativeTransformer>),
            None,
        }
        let t = self.tc(c, |c| match &c.transformer {
            Some(Transformer::Js {
                this, transform, ..
            }) => T::Js(this.clone(), transform.clone()),
            Some(Transformer::Native(n)) => T::Native(n.clone()),
            None => T::None,
        });
        let p = match t {
            T::Js(this, Some(f)) => {
                let cv = handle_value(cx, c);
                self.promise_call(cx, Some(&f), &this, &[chunk, cv])
            }
            // The default transform: enqueue the chunk as it is.
            T::Js(_, None) => match self.transform_controller_enqueue(cx, c, chunk) {
                Ok(()) => Deferred::resolved_undefined(cx).value(cx),
                Err(e) => Deferred::rejected(cx, &e).value(cx),
            },
            T::Native(n) => match n.transform(self, cx, c, chunk) {
                Ok(()) => Deferred::resolved_undefined(cx).value(cx),
                Err(e) => Deferred::rejected(cx, &e).value(cx),
            },
            T::None => Deferred::resolved_undefined(cx).value(cx),
        };
        let out = Deferred::new(cx);
        let o = out.clone();
        let s = self.tc(c, |c| c.stream);
        self.upon(
            cx,
            &p,
            Box::new(move |e, cx, r| match r {
                Ok(_) => o.resolve_undefined(cx),
                Err(r) => {
                    e.transform_error(cx, s, r.clone());
                    o.reject(cx, &r);
                }
            }),
        );
        out.value(cx)
    }

    /// The flush or cancel algorithm's promise.
    fn transform_flush_or_cancel(
        &self,
        cx: &mut Cx,
        c: Handle,
        cancel: Option<JsValue>,
    ) -> JsValue {
        enum T {
            Js(JsValue, Option<JsFn>),
            Native(Rc<dyn NativeTransformer>),
            None,
        }
        let t = self.tc(c, |c| match &c.transformer {
            Some(Transformer::Js {
                this,
                flush,
                cancel: cf,
                ..
            }) => T::Js(
                this.clone(),
                if cancel.is_some() {
                    cf.clone()
                } else {
                    flush.clone()
                },
            ),
            Some(Transformer::Native(n)) => T::Native(n.clone()),
            None => T::None,
        });
        let p = match t {
            T::Js(this, f) => {
                let args = match &cancel {
                    Some(r) => vec![r.clone()],
                    None => vec![handle_value(cx, c)],
                };
                self.promise_call(cx, f.as_ref(), &this, &args)
            }
            T::Native(n) if cancel.is_none() => match n.flush(self, cx, c) {
                Ok(()) => Deferred::resolved_undefined(cx).value(cx),
                Err(e) => Deferred::rejected(cx, &e).value(cx),
            },
            _ => Deferred::resolved_undefined(cx).value(cx),
        };
        // TransformStreamDefaultControllerClearAlgorithms.
        self.tc(c, |c| c.transformer = None);
        p
    }
}

/// The writable side's sink (§6.4.4 the default sink algorithms).
struct TransformSink {
    stream: Handle,
    start: Deferred,
}

impl NativeSink for TransformSink {
    fn start(&self, e: &Engine, cx: &mut Cx) -> Result<JsValue, JsValue> {
        let _ = e;
        Ok(self.start.value(cx))
    }

    /// TransformStreamDefaultSinkWriteAlgorithm.
    fn write(&self, e: &Engine, cx: &mut Cx, chunk: JsValue) -> JsValue {
        let s = self.stream;
        let Some(c) = e.ts(s, |s| s.controller) else {
            return Deferred::resolved_undefined(cx).value(cx);
        };
        if e.ts(s, |s| s.backpressure) == Some(true) {
            let change = e.ts(s, |s| s.backpressure_change.clone());
            let out = Deferred::new(cx);
            let o = out.clone();
            let wait = match change {
                Some(d) => d.value(cx),
                None => Deferred::resolved_undefined(cx).value(cx),
            };
            e.upon(
                cx,
                &wait,
                Box::new(move |e, cx, _| {
                    let w = e.ts(s, |s| s.writable);
                    if e.writable_state(w) == WState::Erroring {
                        let err = e.writable_stored_error_value(cx, w);
                        o.reject(cx, &err);
                        return;
                    }
                    let p = e.transform_perform(cx, c, chunk);
                    o.resolve(cx, &p);
                }),
            );
            return out.value(cx);
        }
        e.transform_perform(cx, c, chunk)
    }

    /// TransformStreamDefaultSinkCloseAlgorithm.
    fn close(&self, e: &Engine, cx: &mut Cx) -> JsValue {
        let s = self.stream;
        let Some(c) = e.ts(s, |s| s.controller) else {
            return Deferred::resolved_undefined(cx).value(cx);
        };
        if let Some(f) = e.tc(c, |c| c.finish.clone()) {
            return f.value(cx);
        }
        let finish = Deferred::new(cx);
        e.tc(c, |c| c.finish = Some(finish.clone()));
        let flush = e.transform_flush_or_cancel(cx, c, None);
        let f = finish.clone();
        e.upon(
            cx,
            &flush,
            Box::new(move |e, cx, r| {
                let readable = e.ts(s, |s| s.readable);
                let rc = e.rs(readable, |r| r.controller);
                match r {
                    Ok(_) => {
                        if e.readable_state(readable) == RState::Errored {
                            let err = e.readable_stored_error(cx, readable);
                            f.reject(cx, &err);
                        } else {
                            if let Some(rc) = rc {
                                e.readable_controller_close(cx, rc);
                            }
                            f.resolve_undefined(cx);
                        }
                    }
                    Err(r) => {
                        if let Some(rc) = rc {
                            e.readable_controller_error(cx, rc, r.clone());
                        }
                        f.reject(cx, &r);
                    }
                }
            }),
        );
        finish.value(cx)
    }

    /// TransformStreamDefaultSinkAbortAlgorithm.
    fn abort(&self, e: &Engine, cx: &mut Cx, reason: JsValue) -> JsValue {
        let s = self.stream;
        let Some(c) = e.ts(s, |s| s.controller) else {
            return Deferred::resolved_undefined(cx).value(cx);
        };
        if let Some(f) = e.tc(c, |c| c.finish.clone()) {
            return f.value(cx);
        }
        let finish = Deferred::new(cx);
        e.tc(c, |c| c.finish = Some(finish.clone()));
        let cancel = e.transform_flush_or_cancel(cx, c, Some(reason.clone()));
        let f = finish.clone();
        e.upon(
            cx,
            &cancel,
            Box::new(move |e, cx, r| {
                let readable = e.ts(s, |s| s.readable);
                let rc = e.rs(readable, |r| r.controller);
                match r {
                    Ok(_) => {
                        if e.readable_state(readable) == RState::Errored {
                            let err = e.readable_stored_error(cx, readable);
                            f.reject(cx, &err);
                        } else {
                            if let Some(rc) = rc {
                                e.readable_controller_error(cx, rc, reason);
                            }
                            f.resolve_undefined(cx);
                        }
                    }
                    Err(r) => {
                        if let Some(rc) = rc {
                            e.readable_controller_error(cx, rc, r.clone());
                        }
                        f.reject(cx, &r);
                    }
                }
            }),
        );
        finish.value(cx)
    }
}

/// The readable side's source (§6.4.5 the default source algorithms).
struct TransformSource {
    stream: Handle,
    start: Deferred,
}

impl NativeSource for TransformSource {
    fn start(&self, _e: &Engine, cx: &mut Cx, _controller: Handle) -> Result<JsValue, JsValue> {
        Ok(self.start.value(cx))
    }

    /// TransformStreamDefaultSourcePullAlgorithm.
    fn pull(&self, e: &Engine, cx: &mut Cx, _controller: Handle) -> JsValue {
        e.transform_set_backpressure(cx, self.stream, false);
        match e.ts(self.stream, |s| s.backpressure_change.clone()) {
            Some(d) => d.value(cx),
            None => Deferred::resolved_undefined(cx).value(cx),
        }
    }

    /// TransformStreamDefaultSourceCancelAlgorithm.
    fn cancel(&self, e: &Engine, cx: &mut Cx, reason: JsValue) -> JsValue {
        let s = self.stream;
        let Some(c) = e.ts(s, |s| s.controller) else {
            return Deferred::resolved_undefined(cx).value(cx);
        };
        if let Some(f) = e.tc(c, |c| c.finish.clone()) {
            return f.value(cx);
        }
        let finish = Deferred::new(cx);
        e.tc(c, |c| c.finish = Some(finish.clone()));
        let cancel = e.transform_flush_or_cancel(cx, c, Some(reason.clone()));
        let f = finish.clone();
        e.upon(
            cx,
            &cancel,
            Box::new(move |e, cx, r| {
                let writable = e.ts(s, |s| s.writable);
                let wc = e.writable_controller_of(writable);
                match r {
                    Ok(_) => {
                        if e.writable_state(writable) == WState::Errored {
                            let err = e.writable_stored_error_value(cx, writable);
                            f.reject(cx, &err);
                        } else {
                            if let Some(wc) = wc {
                                e.writable_controller_error_if_needed(cx, wc, reason);
                            }
                            e.transform_unblock_write(cx, s);
                            f.resolve_undefined(cx);
                        }
                    }
                    Err(r) => {
                        if let Some(wc) = wc {
                            e.writable_controller_error_if_needed(cx, wc, r.clone());
                        }
                        e.transform_unblock_write(cx, s);
                        f.reject(cx, &r);
                    }
                }
            }),
        );
        finish.value(cx)
    }
}

impl TransformStreamImpl for Engine {
    fn readable(&self, this: Handle) -> Handle {
        self.ts(this, |s| s.readable)
    }

    fn writable(&self, this: Handle) -> Handle {
        self.ts(this, |s| s.writable)
    }

    fn constructor(
        &self,
        cx: &mut Cx,
        transformer: Option<JsObject>,
        writable_strategy: QueuingStrategy,
        readable_strategy: QueuingStrategy,
    ) -> rt::Result<Handle> {
        let this = match &transformer {
            Some(o) => {
                let scope = cx.scope();
                let l = v8::Local::new(scope, o.global());
                JsValue::new(scope, l.into())
            }
            None => undefined(cx),
        };
        // The Transformer dictionary, its members in lexicographic order.
        let cancel = get_callback(cx, &this, "cancel").map_err(|e| throw(cx, e))?;
        let flush = get_callback(cx, &this, "flush").map_err(|e| throw(cx, e))?;
        let readable_type = get(cx, &this, "readableType").map_err(|e| throw(cx, e))?;
        let start = get_callback(cx, &this, "start").map_err(|e| throw(cx, e))?;
        let transform = get_callback(cx, &this, "transform").map_err(|e| throw(cx, e))?;
        let writable_type = get(cx, &this, "writableType").map_err(|e| throw(cx, e))?;
        if !readable_type.is_undefined(cx) {
            return Err(rt::Error::Range(
                "Transformers have no readable type".into(),
            ));
        }
        if !writable_type.is_undefined(cx) {
            return Err(rt::Error::Range(
                "Transformers have no writable type".into(),
            ));
        }
        let readable_hwm = extract_high_water_mark(&readable_strategy, 0.0)?;
        let readable_size = extract_size(cx, &readable_strategy);
        let writable_hwm = extract_high_water_mark(&writable_strategy, 1.0)?;
        let writable_size = extract_size(cx, &writable_strategy);
        let start_promise = Deferred::new(cx);
        let (s, c) = self.initialize_transform_stream(
            cx,
            &start_promise,
            writable_hwm,
            writable_size,
            readable_hwm,
            readable_size,
            Transformer::Js {
                this: this.clone(),
                transform,
                flush,
                cancel,
            },
        );
        match start {
            Some(f) => {
                let cv = handle_value(cx, c);
                let r = call(cx, &f, &this, &[cv]).map_err(|e| throw(cx, e))?;
                start_promise.resolve(cx, &r);
            }
            None => start_promise.resolve_undefined(cx),
        }
        Ok(s)
    }
}

impl TransformStreamDefaultControllerImpl for Engine {
    fn desired_size(&self, this: Handle) -> Option<f64> {
        let s = self.tc(this, |c| c.stream);
        self.transform_readable_controller(s)
            .and_then(|rc| self.readable_controller_desired_size(rc))
    }

    fn enqueue(&self, cx: &mut Cx, this: Handle, chunk: Option<JsValue>) -> rt::Result<()> {
        let chunk = chunk.unwrap_or_else(|| undefined(cx));
        self.transform_controller_enqueue(cx, this, chunk)
            .map_err(|e| throw(cx, e))
    }

    fn error(&self, cx: &mut Cx, this: Handle, reason: Option<JsValue>) -> rt::Result<()> {
        let s = self.tc(this, |c| c.stream);
        let reason = reason.unwrap_or_else(|| undefined(cx));
        self.transform_error(cx, s, reason);
        Ok(())
    }

    fn terminate(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.transform_controller_terminate(cx, this);
        Ok(())
    }
}

// ----- queuing strategies -------------------------------------------------------------------------------

/// The size functions' callbacks: §7.2 `chunk.byteLength`, §7.3 always 1.
fn byte_length_size(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let chunk = args.get(0);
    let Some(o) = chunk.to_object(scope) else {
        return;
    };
    let Some(key) = v8::String::new(scope, "byteLength") else {
        return;
    };
    if let Some(v) = o.get(scope, key.into()) {
        rv.set(v);
    }
}

fn count_size(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let one = v8::Integer::new(scope, 1);
    rv.set(one.into());
}

impl Engine {
    fn strategy_hwm(&self, h: Handle) -> f64 {
        match &*self.obj(h) {
            Object::QueuingStrategy {
                high_water_mark, ..
            } => *high_water_mark,
            _ => panic!("not a queuing strategy"),
        }
    }

    /// The realm's size function of a strategy kind (made once per realm, named "size").
    fn strategy_size_function(&self, cx: &mut Cx, byte: bool) -> rt::Result<Function> {
        let scope = cx.scope();
        let context = scope.get_current_context();
        let realm = rt::realm_global(context).map_or(0, |h| h.get());
        if let Some(f) = self.streams.borrow().size_functions.get(&(realm, byte)) {
            let l = v8::Local::new(scope, f);
            let o: v8::Local<v8::Object> = l.into();
            return Ok(Function(JsObject::new(scope, o)));
        }
        let builder = if byte {
            v8::Function::builder(rt::guarded(byte_length_size))
        } else {
            v8::Function::builder(rt::guarded(count_size))
        };
        // (CreateBuiltinFunction: not a constructor, no prototype; the count strategy's takes no
        // argument)
        let f = builder
            .length(if byte { 1 } else { 0 })
            .constructor_behavior(v8::ConstructorBehavior::Throw)
            .build(scope)
            .ok_or(rt::Error::Pending)?;
        if let Some(name) = v8::String::new(scope, "size") {
            f.set_name(name);
        }
        self.streams
            .borrow_mut()
            .size_functions
            .insert((realm, byte), v8::Global::new(scope, f));
        let o: v8::Local<v8::Object> = f.into();
        Ok(Function(JsObject::new(scope, o)))
    }
}

impl ByteLengthQueuingStrategyImpl for Engine {
    fn high_water_mark(&self, this: Handle) -> f64 {
        self.strategy_hwm(this)
    }

    fn size(&self, cx: &mut Cx, _this: Handle) -> rt::Result<Function> {
        self.strategy_size_function(cx, true)
    }

    fn constructor(&self, _cx: &mut Cx, init: QueuingStrategyInit) -> rt::Result<Handle> {
        Ok(self.alloc(Object::QueuingStrategy {
            byte: true,
            high_water_mark: init.high_water_mark,
        }))
    }
}

impl CountQueuingStrategyImpl for Engine {
    fn high_water_mark(&self, this: Handle) -> f64 {
        self.strategy_hwm(this)
    }

    fn size(&self, cx: &mut Cx, _this: Handle) -> rt::Result<Function> {
        self.strategy_size_function(cx, false)
    }

    fn constructor(&self, _cx: &mut Cx, init: QueuingStrategyInit) -> rt::Result<Handle> {
        Ok(self.alloc(Object::QueuingStrategy {
            byte: false,
            high_water_mark: init.high_water_mark,
        }))
    }
}

// ----- Encoding §7.4, §7.5 ------------------------------------------------------------------------------

/// A TextDecoderStream's decoder.
struct DecodeTransformer {
    encoding: &'static encoding_rs::Encoding,
    fatal: bool,
    ignore_bom: bool,
    decoder: RefCell<Option<encoding_rs::Decoder>>,
}

impl DecodeTransformer {
    /// Decodes `bytes` (`last`: the end of the stream) and enqueues the output when not empty.
    fn decode(
        &self,
        e: &Engine,
        cx: &mut Cx,
        c: Handle,
        bytes: &[u8],
        last: bool,
    ) -> Result<(), JsValue> {
        let out = {
            let mut slot = self.decoder.borrow_mut();
            let decoder = slot.get_or_insert_with(|| {
                if self.ignore_bom {
                    self.encoding.new_decoder_without_bom_handling()
                } else {
                    self.encoding.new_decoder_with_bom_removal()
                }
            });
            let mut out = String::with_capacity(
                decoder
                    .max_utf8_buffer_length(bytes.len())
                    .unwrap_or(bytes.len() * 3),
            );
            if self.fatal {
                let (result, _) =
                    decoder.decode_to_string_without_replacement(bytes, &mut out, last);
                if let encoding_rs::DecoderResult::Malformed(_, _) = result {
                    None
                } else {
                    Some(out)
                }
            } else {
                let _ = decoder.decode_to_string(bytes, &mut out, last);
                Some(out)
            }
        };
        let Some(out) = out else {
            return Err(type_error(cx, "The encoded data was not valid"));
        };
        if !out.is_empty() {
            let v = {
                let scope = cx.scope();
                let s = v8::String::new(scope, &out)
                    .map(|s| s.into())
                    .unwrap_or_else(|| v8::undefined(scope).into());
                JsValue::new(scope, s)
            };
            e.transform_controller_enqueue(cx, c, v)?;
        }
        Ok(())
    }
}

impl NativeTransformer for DecodeTransformer {
    /// "Decode and enqueue a chunk": the chunk must be a buffer source.
    fn transform(&self, e: &Engine, cx: &mut Cx, c: Handle, chunk: JsValue) -> Result<(), JsValue> {
        let bytes = buffer_source_bytes(cx, &chunk)
            .ok_or_else(|| type_error(cx, "The chunk is not an ArrayBuffer or ArrayBufferView"))?;
        self.decode(e, cx, c, &bytes, false)
    }

    /// "Flush and enqueue".
    fn flush(&self, e: &Engine, cx: &mut Cx, c: Handle) -> Result<(), JsValue> {
        self.decode(e, cx, c, &[], true)
    }
}

/// The bytes of an ArrayBuffer, SharedArrayBuffer or view (a copy).
fn buffer_source_bytes(cx: &mut Cx, v: &JsValue) -> Option<Vec<u8>> {
    if let Some(b) = view_bytes(cx, v) {
        return Some(b);
    }
    let scope = cx.scope();
    let local = v8::Local::new(scope, v.global());
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) {
        let store = ab.get_backing_store();
        return Some(store.iter().map(|c| c.get()).collect());
    }
    if let Ok(sab) = v8::Local::<v8::SharedArrayBuffer>::try_from(local) {
        let store = sab.get_backing_store();
        return Some(store.iter().map(|c| c.get()).collect());
    }
    None
}

/// A TextEncoderStream's encoder: a lead surrogate left at the end of a chunk waits for the next.
struct EncodeTransformer {
    pending: RefCell<Option<u16>>,
}

impl NativeTransformer for EncodeTransformer {
    /// "Encode and enqueue a chunk".
    fn transform(&self, e: &Engine, cx: &mut Cx, c: Handle, chunk: JsValue) -> Result<(), JsValue> {
        let units: Vec<u16> = {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let local = v8::Local::new(tc, chunk.global());
            match local.to_string(tc) {
                Some(s) => {
                    let mut buf = vec![0u16; s.length()];
                    s.write_v2(tc, 0, &mut buf, v8::WriteFlags::empty());
                    buf
                }
                None => {
                    let ex = tc
                        .exception()
                        .map(|e| JsValue::new(tc, e))
                        .unwrap_or_else(|| rt::undefined_value(tc));
                    if !tc.has_terminated() {
                        tc.reset();
                    }
                    return Err(ex);
                }
            }
        };
        let mut all: Vec<u16> = Vec::with_capacity(units.len() + 1);
        if let Some(p) = self.pending.borrow_mut().take() {
            all.push(p);
        }
        all.extend_from_slice(&units);
        if let Some(&last) = all.last() {
            if (0xD800..0xDC00).contains(&last) {
                *self.pending.borrow_mut() = Some(last);
                all.pop();
            }
        }
        let text = String::from_utf16_lossy(&all);
        if !text.is_empty() {
            let v = uint8_value(cx, text.into_bytes());
            e.transform_controller_enqueue(cx, c, v)?;
        }
        Ok(())
    }

    /// "Encode and flush": a lone lead surrogate becomes U+FFFD.
    fn flush(&self, e: &Engine, cx: &mut Cx, c: Handle) -> Result<(), JsValue> {
        if self.pending.borrow_mut().take().is_some() {
            let v = uint8_value(cx, vec![0xEF, 0xBF, 0xBD]);
            e.transform_controller_enqueue(cx, c, v)?;
        }
        Ok(())
    }
}

impl Engine {
    /// A transform stream with an engine transformer (high water marks 1 and 0).
    pub(super) fn native_transform_stream(
        &self,
        cx: &mut Cx,
        t: Rc<dyn NativeTransformer>,
    ) -> Handle {
        let start = Deferred::resolved_undefined(cx);
        let (s, _) = self.initialize_transform_stream(
            cx,
            &start,
            1.0,
            None,
            0.0,
            None,
            Transformer::Native(t),
        );
        s
    }

    /// The transform stream behind a TextDecoderStream or TextEncoderStream.
    fn generic_transform(&self, h: Handle) -> Handle {
        match &*self.obj(h) {
            Object::TextDecoderStream { transform, .. }
            | Object::TextEncoderStream { transform }
            | Object::CompressionStream { transform }
            | Object::DecompressionStream { transform } => *transform,
            Object::TransformStream(_) => h,
            _ => panic!("not a GenericTransformStream"),
        }
    }

    /// CreateReadableStream with a size function.
    fn create_readable_stream_sized(
        &self,
        cx: &mut Cx,
        source: Rc<dyn NativeSource>,
        high_water_mark: f64,
        size: Option<JsFn>,
    ) -> Handle {
        let s = self.create_readable_stream(cx, source, high_water_mark, false);
        if let Some(c) = self.rs(s, |s| s.controller) {
            self.rc(c, |c| c.size = size);
        }
        s
    }
}

impl GenericTransformStreamImpl for Engine {
    fn readable(&self, this: Handle) -> Handle {
        let t = self.generic_transform(this);
        self.ts(t, |s| s.readable)
    }

    fn writable(&self, this: Handle) -> Handle {
        let t = self.generic_transform(this);
        self.ts(t, |s| s.writable)
    }
}

impl TextDecoderStreamImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        label: &str,
        options: TextDecoderOptions,
    ) -> rt::Result<Handle> {
        let trimmed = label.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r'));
        let encoding = encoding_rs::Encoding::for_label_no_replacement(trimmed.as_bytes())
            .ok_or_else(|| {
                rt::Error::Range(format!("The encoding label '{label}' is not supported").into())
            })?;
        let transform = self.native_transform_stream(
            cx,
            Rc::new(DecodeTransformer {
                encoding,
                fatal: options.fatal,
                ignore_bom: options.ignore_bom,
                decoder: RefCell::new(None),
            }),
        );
        Ok(self.alloc(Object::TextDecoderStream {
            transform,
            encoding,
            fatal: options.fatal,
            ignore_bom: options.ignore_bom,
        }))
    }
}

impl TextEncoderStreamImpl for Engine {
    fn constructor(&self, cx: &mut Cx) -> rt::Result<Handle> {
        let transform = self.native_transform_stream(
            cx,
            Rc::new(EncodeTransformer {
                pending: RefCell::new(None),
            }),
        );
        Ok(self.alloc(Object::TextEncoderStream { transform }))
    }
}
