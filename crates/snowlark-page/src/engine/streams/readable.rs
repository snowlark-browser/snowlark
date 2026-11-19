// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Streams §4: `ReadableStream`, its default controller (the byte controller's own operations are
//! in `bytes.rs`), the readers, tee (§4.9.1 ReadableStreamDefaultTee) and pipes (§4.9.1
//! ReadableStreamPipeTo).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use snowlark_bindings::generated::{
    Int8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView as ArrayBufferView,
    QueuingStrategy, ReadableByteStreamControllerImpl, ReadableStreamBYOBReaderImpl,
    ReadableStreamBYOBReaderReadOptions, ReadableStreamBYOBRequestImpl,
    ReadableStreamDefaultControllerImpl, ReadableStreamDefaultReaderImpl,
    ReadableStreamDefaultReaderOrReadableStreamBYOBReader as ReaderUnion,
    ReadableStreamGenericReaderImpl, ReadableStreamGetReaderOptions, ReadableStreamImpl,
    ReadableStreamIteratorOptions, ReadableStreamReaderMode, ReadableWritablePair,
    StreamPipeOptions,
};
use snowlark_bindings::rt::{self, Cx, Handle, JsObject, JsPromise, JsValue};

use super::super::{Engine, Object};
use super::bytes::{view_info, ByteState, ReadIntoRequest};
use super::writable::WState;
use super::{
    array, call, error_value, extract_high_water_mark, get, get_callback, handle_value,
    iter_result, type_error, undefined, BytesDone, Deferred, JsFn, StartAlgorithm, Then,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RState {
    Readable,
    Closed,
    Errored,
}

pub struct ReadableStreamData {
    pub(crate) state: RState,
    pub(crate) stored_error: Option<JsValue>,
    pub(crate) disturbed: bool,
    pub(crate) reader: Option<Handle>,
    pub(crate) controller: Option<Handle>,
}

/// What a read request does with the outcome (§4.4.2 "read request").
pub(crate) trait ReadSteps {
    fn chunk(self: Box<Self>, e: &Engine, cx: &mut Cx, chunk: JsValue);
    fn close(self: Box<Self>, e: &Engine, cx: &mut Cx);
    fn error(self: Box<Self>, e: &Engine, cx: &mut Cx, error: JsValue);
}

pub(crate) enum ReadRequest {
    /// `reader.read()`'s promise.
    Promise(Deferred),
    Steps(Box<dyn ReadSteps>),
}

impl ReadRequest {
    pub(crate) fn chunk(self, e: &Engine, cx: &mut Cx, chunk: JsValue) {
        match self {
            ReadRequest::Promise(p) => {
                let r = iter_result(cx, &chunk, false);
                p.resolve(cx, &r);
            }
            ReadRequest::Steps(s) => s.chunk(e, cx, chunk),
        }
    }

    pub(crate) fn close(self, e: &Engine, cx: &mut Cx) {
        match self {
            ReadRequest::Promise(p) => {
                let u = undefined(cx);
                let r = iter_result(cx, &u, true);
                p.resolve(cx, &r);
            }
            ReadRequest::Steps(s) => s.close(e, cx),
        }
    }

    pub(crate) fn error(self, e: &Engine, cx: &mut Cx, error: JsValue) {
        match self {
            ReadRequest::Promise(p) => p.reject(cx, &error),
            ReadRequest::Steps(s) => s.error(e, cx, error),
        }
    }
}

/// A `ReadableStreamDefaultReader`, or (`byob`) a `ReadableStreamBYOBReader`.
pub struct ReaderData {
    pub(crate) stream: Option<Handle>,
    pub(crate) closed: Deferred,
    pub(crate) byob: bool,
    /// A default reader's [[readRequests]].
    pub(crate) requests: VecDeque<ReadRequest>,
    /// A BYOB reader's [[readIntoRequests]].
    pub(crate) into_requests: VecDeque<ReadIntoRequest>,
    /// The `preventCancel` of the async iterator this reader belongs to.
    pub(crate) iterator_prevent_cancel: bool,
}

/// An underlying source the engine provides.
pub(crate) trait NativeSource {
    /// The start algorithm's result (a value or a promise).
    fn start(&self, _e: &Engine, cx: &mut Cx, _controller: Handle) -> Result<JsValue, JsValue> {
        Ok(undefined(cx))
    }
    /// The pull algorithm: a promise.
    fn pull(&self, _e: &Engine, cx: &mut Cx, _controller: Handle) -> JsValue {
        Deferred::resolved_undefined(cx).value(cx)
    }
    /// The cancel algorithm: a promise.
    fn cancel(&self, _e: &Engine, cx: &mut Cx, _reason: JsValue) -> JsValue {
        Deferred::resolved_undefined(cx).value(cx)
    }
}

pub(crate) enum Source {
    Js {
        this: JsValue,
        pull: Option<JsFn>,
        cancel: Option<JsFn>,
    },
    Native(Rc<dyn NativeSource>),
}

/// A `ReadableStreamDefaultController` or (`byte`) a `ReadableByteStreamController`.
pub struct ReadableControllerData {
    pub(crate) stream: Handle,
    pub(crate) byte: bool,
    pub(crate) queue: VecDeque<(JsValue, f64)>,
    pub(crate) queue_total: f64,
    pub(crate) started: bool,
    pub(crate) close_requested: bool,
    pub(crate) pull_again: bool,
    pub(crate) pulling: bool,
    pub(crate) high_water_mark: f64,
    pub(crate) size: Option<JsFn>,
    /// The pull and cancel algorithms (`None` once cleared).
    pub(crate) source: Option<Source>,
    /// A byte stream controller's own slots.
    pub(crate) bytes: Option<Box<ByteState>>,
}

/// Calls a strategy's size function: the chunk's size, or what it threw.
pub(crate) fn call_size(cx: &mut Cx, f: &JsFn, chunk: &JsValue) -> Result<f64, JsValue> {
    let u = undefined(cx);
    let v = call(cx, f, &u, std::slice::from_ref(chunk))?;
    let scope = cx.scope();
    v8::tc_scope!(let tc, scope);
    let local = v8::Local::new(tc, v.global());
    match local.number_value(tc) {
        Some(n) => Ok(n),
        None => {
            let e = tc
                .exception()
                .map(|e| JsValue::new(tc, e))
                .unwrap_or_else(|| rt::undefined_value(tc));
            if !tc.has_terminated() {
                tc.reset();
            }
            Err(e)
        }
    }
}

/// §7.4 ExtractSizeAlgorithm: the strategy's size function, if any.
pub(crate) fn extract_size(cx: &mut Cx, strategy: &QueuingStrategy) -> Option<JsFn> {
    let s = strategy.size.as_ref()?;
    let scope = cx.scope();
    let o = v8::Local::new(scope, s.0.global());
    let f = v8::Local::<v8::Function>::try_from(o).ok()?;
    Some(v8::Global::new(scope, f))
}

/// The bytes of an ArrayBufferView chunk (a copy).
pub(crate) fn view_bytes(cx: &mut Cx, v: &JsValue) -> Option<Vec<u8>> {
    let scope = cx.scope();
    let local = v8::Local::new(scope, v.global());
    let view = v8::Local::<v8::ArrayBufferView>::try_from(local).ok()?;
    let mut out = vec![0; view.byte_length()];
    view.copy_contents(&mut out);
    Some(out)
}

/// A new Uint8Array over a copy of `bytes`, as a value.
pub(crate) fn uint8_value(cx: &mut Cx, bytes: Vec<u8>) -> JsValue {
    match super::super::blob::uint8_array(cx, bytes) {
        Ok(o) => {
            let scope = cx.scope();
            let l = v8::Local::new(scope, o.global());
            JsValue::new(scope, l.into())
        }
        Err(_) => undefined(cx),
    }
}

fn view_object(v: &ArrayBufferView) -> &JsObject {
    use ArrayBufferView as V;
    match v {
        V::Int8Array(o)
        | V::Int16Array(o)
        | V::Int32Array(o)
        | V::Uint8Array(o)
        | V::Uint16Array(o)
        | V::Uint32Array(o)
        | V::Uint8ClampedArray(o)
        | V::BigInt64Array(o)
        | V::BigUint64Array(o)
        | V::Float16Array(o)
        | V::Float32Array(o)
        | V::Float64Array(o)
        | V::DataView(o) => o,
    }
}

impl Engine {
    // ----- object access ------------------------------------------------------------------------------

    pub(crate) fn rs<R>(&self, h: Handle, f: impl FnOnce(&mut ReadableStreamData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::ReadableStream(s) => f(s),
            _ => panic!("not a ReadableStream"),
        }
    }

    pub(crate) fn rc<R>(&self, h: Handle, f: impl FnOnce(&mut ReadableControllerData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::ReadableController(c) => f(c),
            _ => panic!("not a readable stream controller"),
        }
    }

    pub(crate) fn rr<R>(&self, h: Handle, f: impl FnOnce(&mut ReaderData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::ReadableStreamDefaultReader(r) | Object::ReadableStreamBYOBReader(r) => f(r),
            _ => panic!("not a readable stream reader"),
        }
    }

    // ----- §4.9.1 working with readable streams -----------------------------------------------------------

    /// IsReadableStreamLocked.
    pub(crate) fn readable_locked(&self, s: Handle) -> bool {
        self.rs(s, |s| s.reader.is_some())
    }

    pub(crate) fn readable_disturbed(&self, s: Handle) -> bool {
        self.rs(s, |s| s.disturbed)
    }

    pub(crate) fn readable_state(&self, s: Handle) -> RState {
        self.rs(s, |s| s.state)
    }

    pub(crate) fn readable_stored_error(&self, cx: &mut Cx, s: Handle) -> JsValue {
        self.rs(s, |s| s.stored_error.clone())
            .unwrap_or_else(|| undefined(cx))
    }

    fn new_readable_stream(&self) -> Handle {
        self.alloc(Object::ReadableStream(Box::new(ReadableStreamData {
            state: RState::Readable,
            stored_error: None,
            disturbed: false,
            reader: None,
            controller: None,
        })))
    }

    /// CreateReadableStream with a native source (high water mark 1, no size function unless
    /// given), or (`byte`) the byte-stream setup with high water mark 0.
    pub(crate) fn create_readable_stream(
        &self,
        cx: &mut Cx,
        source: Rc<dyn NativeSource>,
        high_water_mark: f64,
        byte: bool,
    ) -> Handle {
        let s = self.new_readable_stream();
        let start = source.clone();
        let _ = self.set_up_readable_controller(
            cx,
            s,
            byte.then(ByteState::default),
            Box::new(move |e, cx, c| start.start(e, cx, c)),
            Source::Native(source),
            high_water_mark,
            None,
        );
        s
    }

    /// SetUpReadableStreamDefaultController, or (with `bytes`) SetUpReadableByteStreamController.
    #[allow(clippy::too_many_arguments)]
    fn set_up_readable_controller(
        &self,
        cx: &mut Cx,
        stream: Handle,
        bytes: Option<ByteState>,
        start: StartAlgorithm,
        source: Source,
        high_water_mark: f64,
        size: Option<JsFn>,
    ) -> Result<(), JsValue> {
        let c = self.alloc(Object::ReadableController(Box::new(
            ReadableControllerData {
                stream,
                byte: bytes.is_some(),
                queue: VecDeque::new(),
                queue_total: 0.0,
                started: false,
                close_requested: false,
                pull_again: false,
                pulling: false,
                high_water_mark,
                size,
                source: Some(source),
                bytes: bytes.map(Box::new),
            },
        )));
        self.rs(stream, |s| s.controller = Some(c));
        let result = start(self, cx, c)?;
        self.upon(
            cx,
            &result,
            Box::new(move |e, cx, r| match r {
                Ok(_) => {
                    e.rc(c, |c| c.started = true);
                    e.readable_controller_call_pull_if_needed(cx, c);
                }
                Err(r) => e.readable_controller_error(cx, c, r),
            }),
        );
        Ok(())
    }

    /// ReadableStreamDefaultControllerCanCloseOrEnqueue.
    fn readable_controller_can_close_or_enqueue(&self, c: Handle) -> bool {
        let (stream, close_requested) = self.rc(c, |c| (c.stream, c.close_requested));
        !close_requested && self.readable_state(stream) == RState::Readable
    }

    /// ReadableStreamDefaultControllerGetDesiredSize.
    pub(crate) fn readable_controller_desired_size(&self, c: Handle) -> Option<f64> {
        let (stream, hwm, total) = self.rc(c, |c| (c.stream, c.high_water_mark, c.queue_total));
        match self.readable_state(stream) {
            RState::Errored => None,
            RState::Closed => Some(0.0),
            RState::Readable => Some(hwm - total),
        }
    }

    /// ReadableStreamDefaultControllerShouldCallPull.
    fn readable_controller_should_call_pull(&self, c: Handle) -> bool {
        if !self.readable_controller_can_close_or_enqueue(c) {
            return false;
        }
        let (stream, started) = self.rc(c, |c| (c.stream, c.started));
        if !started {
            return false;
        }
        if self.readable_has_default_reader(stream) && self.readable_num_read_requests(stream) > 0 {
            return true;
        }
        if self.readable_has_byob_reader(stream) && self.readable_num_read_into_requests(stream) > 0
        {
            return true;
        }
        self.readable_controller_desired_size(c)
            .is_some_and(|d| d > 0.0)
    }

    /// ReadableStreamDefaultControllerCallPullIfNeeded.
    pub(crate) fn readable_controller_call_pull_if_needed(&self, cx: &mut Cx, c: Handle) {
        if !self.readable_controller_should_call_pull(c) {
            return;
        }
        let pulling = self.rc(c, |c| {
            if c.pulling {
                c.pull_again = true;
                true
            } else {
                c.pulling = true;
                false
            }
        });
        if pulling {
            return;
        }
        let p = self.readable_pull_algorithm(cx, c);
        self.upon(
            cx,
            &p,
            Box::new(move |e, cx, r| match r {
                Ok(_) => {
                    let again = e.rc(c, |c| {
                        c.pulling = false;
                        std::mem::take(&mut c.pull_again)
                    });
                    if again {
                        e.readable_controller_call_pull_if_needed(cx, c);
                    }
                }
                Err(r) => e.readable_controller_error(cx, c, r),
            }),
        );
    }

    fn readable_pull_algorithm(&self, cx: &mut Cx, c: Handle) -> JsValue {
        enum Pull {
            Js(JsValue, Option<JsFn>),
            Native(Rc<dyn NativeSource>),
            None,
        }
        let pull = self.rc(c, |c| match &c.source {
            Some(Source::Js { this, pull, .. }) => Pull::Js(this.clone(), pull.clone()),
            Some(Source::Native(n)) => Pull::Native(n.clone()),
            None => Pull::None,
        });
        match pull {
            Pull::Js(this, f) => {
                let cv = handle_value(cx, c);
                self.promise_call(cx, f.as_ref(), &this, &[cv])
            }
            Pull::Native(n) => n.pull(self, cx, c),
            Pull::None => Deferred::resolved_undefined(cx).value(cx),
        }
    }

    /// ReadableStreamDefaultControllerClearAlgorithms (ReadableByteStreamControllerClearAlgorithms).
    pub(crate) fn readable_controller_clear_algorithms(&self, c: Handle) {
        self.rc(c, |c| {
            c.source = None;
            c.size = None;
        });
    }

    /// ReadableStreamDefaultControllerClose (ReadableByteStreamControllerClose for a byte controller,
    /// whose error is dropped: the callers are the spec's "!" calls).
    pub(crate) fn readable_controller_close(&self, cx: &mut Cx, c: Handle) {
        if self.is_byte_controller(c) {
            let _ = self.byte_controller_close(cx, c);
            return;
        }
        if !self.readable_controller_can_close_or_enqueue(c) {
            return;
        }
        let (stream, empty) = self.rc(c, |c| {
            c.close_requested = true;
            (c.stream, c.queue.is_empty())
        });
        if empty {
            self.readable_controller_clear_algorithms(c);
            self.readable_stream_close(cx, stream);
        }
    }

    /// ReadableStreamDefaultControllerEnqueue: `Err` with the exception to throw (a size function
    /// that threw or gave an invalid size; the stream is errored with it).
    pub(crate) fn readable_controller_enqueue(
        &self,
        cx: &mut Cx,
        c: Handle,
        chunk: JsValue,
    ) -> Result<(), JsValue> {
        if self.is_byte_controller(c) {
            return self.byte_controller_enqueue(cx, c, &chunk);
        }
        if !self.readable_controller_can_close_or_enqueue(c) {
            return Ok(());
        }
        let stream = self.rc(c, |c| c.stream);
        if self.readable_locked(stream) && self.readable_num_read_requests(stream) > 0 {
            self.readable_fulfill_read_request(cx, stream, chunk, false);
        } else {
            let (size_fn, byte) = self.rc(c, |c| (c.size.clone(), c.byte));
            let size = if byte {
                Ok(view_bytes(cx, &chunk).map_or(0, |b| b.len()) as f64)
            } else {
                match size_fn {
                    Some(f) => call_size(cx, &f, &chunk),
                    None => Ok(1.0),
                }
            };
            let size = match size {
                Ok(s) => s,
                Err(e) => {
                    self.readable_controller_error(cx, c, e.clone());
                    return Err(e);
                }
            };
            // EnqueueValueWithSize.
            if !size.is_finite() || size < 0.0 {
                let e = error_value(cx, rt::Error::Range("The chunk size is invalid".into()));
                self.readable_controller_error(cx, c, e.clone());
                return Err(e);
            }
            self.rc(c, |c| {
                c.queue.push_back((chunk, size));
                c.queue_total += size;
            });
        }
        self.readable_controller_call_pull_if_needed(cx, c);
        Ok(())
    }

    /// ReadableStreamDefaultControllerError (ReadableByteStreamControllerError).
    pub(crate) fn readable_controller_error(&self, cx: &mut Cx, c: Handle, e: JsValue) {
        let stream = self.rc(c, |c| c.stream);
        if self.readable_state(stream) != RState::Readable {
            return;
        }
        if self.is_byte_controller(c) {
            self.byte_reset(c);
        }
        self.rc(c, |c| {
            c.queue.clear();
            c.queue_total = 0.0;
        });
        self.readable_controller_clear_algorithms(c);
        self.readable_stream_error(cx, stream, e);
    }

    /// The controller's [[CancelSteps]].
    fn readable_controller_cancel_steps(&self, cx: &mut Cx, c: Handle, reason: JsValue) -> JsValue {
        if self.is_byte_controller(c) {
            self.byte_reset(c);
        }
        enum Cancel {
            Js(JsValue, Option<JsFn>),
            Native(Rc<dyn NativeSource>),
            None,
        }
        let cancel = self.rc(c, |c| {
            c.queue.clear();
            c.queue_total = 0.0;
            match &c.source {
                Some(Source::Js { this, cancel, .. }) => Cancel::Js(this.clone(), cancel.clone()),
                Some(Source::Native(n)) => Cancel::Native(n.clone()),
                None => Cancel::None,
            }
        });
        let result = match cancel {
            Cancel::Js(this, f) => self.promise_call(cx, f.as_ref(), &this, &[reason]),
            Cancel::Native(n) => n.cancel(self, cx, reason),
            Cancel::None => Deferred::resolved_undefined(cx).value(cx),
        };
        self.readable_controller_clear_algorithms(c);
        result
    }

    /// The controller's [[PullSteps]].
    fn readable_controller_pull_steps(&self, cx: &mut Cx, c: Handle, request: ReadRequest) {
        if self.is_byte_controller(c) {
            self.byte_pull_steps(cx, c, request);
            return;
        }
        let (stream, next) = self.rc(c, |c| {
            let next = c.queue.pop_front().map(|(v, size)| {
                c.queue_total = (c.queue_total - size).max(0.0);
                (v, c.close_requested && c.queue.is_empty())
            });
            (c.stream, next)
        });
        match next {
            Some((chunk, close)) => {
                if close {
                    self.readable_controller_clear_algorithms(c);
                    self.readable_stream_close(cx, stream);
                } else {
                    self.readable_controller_call_pull_if_needed(cx, c);
                }
                request.chunk(self, cx, chunk);
            }
            None => {
                self.readable_add_read_request(stream, request);
                self.readable_controller_call_pull_if_needed(cx, c);
            }
        }
    }

    // ----- the stream ---------------------------------------------------------------------------------

    /// ReadableStreamGetNumReadRequests (0 without a default reader).
    pub(crate) fn readable_num_read_requests(&self, s: Handle) -> usize {
        match self.rs(s, |s| s.reader) {
            Some(r) => self.rr(r, |r| r.requests.len()),
            None => 0,
        }
    }

    /// ReadableStreamGetNumReadIntoRequests (0 without a BYOB reader).
    pub(crate) fn readable_num_read_into_requests(&self, s: Handle) -> usize {
        match self.rs(s, |s| s.reader) {
            Some(r) => self.rr(r, |r| r.into_requests.len()),
            None => 0,
        }
    }

    /// ReadableStreamHasDefaultReader.
    pub(crate) fn readable_has_default_reader(&self, s: Handle) -> bool {
        self.rs(s, |s| s.reader)
            .is_some_and(|r| !self.rr(r, |r| r.byob))
    }

    /// ReadableStreamHasBYOBReader.
    pub(crate) fn readable_has_byob_reader(&self, s: Handle) -> bool {
        self.rs(s, |s| s.reader)
            .is_some_and(|r| self.rr(r, |r| r.byob))
    }

    /// ReadableStreamAddReadRequest.
    pub(crate) fn readable_add_read_request(&self, s: Handle, request: ReadRequest) {
        if let Some(r) = self.rs(s, |s| s.reader) {
            self.rr(r, |r| r.requests.push_back(request));
        }
    }

    /// ReadableStreamAddReadIntoRequest.
    pub(crate) fn readable_add_read_into_request(&self, s: Handle, request: ReadIntoRequest) {
        if let Some(r) = self.rs(s, |s| s.reader) {
            self.rr(r, |r| r.into_requests.push_back(request));
        }
    }

    /// ReadableStreamFulfillReadIntoRequest.
    pub(crate) fn readable_fulfill_read_into_request(
        &self,
        cx: &mut Cx,
        s: Handle,
        chunk: JsValue,
        done: bool,
    ) {
        let Some(reader) = self.rs(s, |s| s.reader) else {
            return;
        };
        let Some(request) = self.rr(reader, |r| r.into_requests.pop_front()) else {
            return;
        };
        if done {
            request.close(self, cx, Some(chunk));
        } else {
            request.chunk(self, cx, chunk);
        }
    }

    /// ReadableStreamFulfillReadRequest.
    pub(crate) fn readable_fulfill_read_request(
        &self,
        cx: &mut Cx,
        s: Handle,
        chunk: JsValue,
        done: bool,
    ) {
        let Some(reader) = self.rs(s, |s| s.reader) else {
            return;
        };
        let Some(request) = self.rr(reader, |r| r.requests.pop_front()) else {
            return;
        };
        if done {
            request.close(self, cx);
        } else {
            request.chunk(self, cx, chunk);
        }
    }

    /// ReadableStreamCancel.
    pub(crate) fn readable_stream_cancel(
        &self,
        cx: &mut Cx,
        s: Handle,
        reason: JsValue,
    ) -> JsValue {
        self.rs(s, |s| s.disturbed = true);
        match self.readable_state(s) {
            RState::Closed => return Deferred::resolved_undefined(cx).value(cx),
            RState::Errored => {
                let e = self.readable_stored_error(cx, s);
                return Deferred::rejected(cx, &e).value(cx);
            }
            RState::Readable => {}
        }
        self.readable_stream_close(cx, s);
        if let Some(reader) = self.rs(s, |s| s.reader) {
            let into = self.rr(reader, |r| std::mem::take(&mut r.into_requests));
            for request in into {
                request.close(self, cx, None);
            }
        }
        let Some(c) = self.rs(s, |s| s.controller) else {
            return Deferred::resolved_undefined(cx).value(cx);
        };
        let source_cancel = self.readable_controller_cancel_steps(cx, c, reason);
        let out = Deferred::new(cx);
        let d = out.clone();
        self.upon(
            cx,
            &source_cancel,
            Box::new(move |_, cx, r| match r {
                Ok(_) => d.resolve_undefined(cx),
                Err(e) => d.reject(cx, &e),
            }),
        );
        out.value(cx)
    }

    /// ReadableStreamClose.
    pub(crate) fn readable_stream_close(&self, cx: &mut Cx, s: Handle) {
        let reader = self.rs(s, |s| {
            s.state = RState::Closed;
            s.reader
        });
        let Some(reader) = reader else {
            return;
        };
        let (closed, requests) = self.rr(reader, |r| {
            (r.closed.clone(), std::mem::take(&mut r.requests))
        });
        closed.resolve_undefined(cx);
        for request in requests {
            request.close(self, cx);
        }
    }

    /// ReadableStreamError.
    pub(crate) fn readable_stream_error(&self, cx: &mut Cx, s: Handle, e: JsValue) {
        let reader = self.rs(s, |s| {
            s.state = RState::Errored;
            s.stored_error = Some(e.clone());
            s.reader
        });
        let Some(reader) = reader else {
            return;
        };
        let (closed, requests, into) = self.rr(reader, |r| {
            (
                r.closed.clone(),
                std::mem::take(&mut r.requests),
                std::mem::take(&mut r.into_requests),
            )
        });
        closed.reject(cx, &e);
        closed.mark_handled(cx);
        for request in requests {
            request.error(self, cx, e.clone());
        }
        for request in into {
            request.error(self, cx, e.clone());
        }
    }

    // ----- readers --------------------------------------------------------------------------------------

    /// AcquireReadableStreamDefaultReader (SetUpReadableStreamDefaultReader).
    pub(crate) fn acquire_default_reader(&self, cx: &mut Cx, s: Handle) -> rt::Result<Handle> {
        if self.readable_locked(s) {
            return Err(rt::Error::Type("The stream is locked to a reader".into()));
        }
        Ok(self.new_reader(cx, s, false))
    }

    /// A new reader of `s` (ReadableStreamReaderGenericInitialize).
    pub(crate) fn new_reader(&self, cx: &mut Cx, s: Handle, byob: bool) -> Handle {
        let closed = Deferred::new(cx);
        let data = Box::new(ReaderData {
            stream: Some(s),
            closed: closed.clone(),
            byob,
            requests: VecDeque::new(),
            into_requests: VecDeque::new(),
            iterator_prevent_cancel: false,
        });
        let reader = self.alloc(if byob {
            Object::ReadableStreamBYOBReader(data)
        } else {
            Object::ReadableStreamDefaultReader(data)
        });
        // ReadableStreamReaderGenericInitialize.
        self.rs(s, |s| s.reader = Some(reader));
        match self.readable_state(s) {
            RState::Readable => {}
            RState::Closed => closed.resolve_undefined(cx),
            RState::Errored => {
                let e = self.readable_stored_error(cx, s);
                closed.reject(cx, &e);
                closed.mark_handled(cx);
            }
        }
        reader
    }

    /// ReadableStreamDefaultReaderRead.
    pub(crate) fn reader_read(&self, cx: &mut Cx, reader: Handle, request: ReadRequest) {
        let Some(s) = self.rr(reader, |r| r.stream) else {
            let e = type_error(cx, "The reader is released");
            request.error(self, cx, e);
            return;
        };
        self.rs(s, |s| s.disturbed = true);
        match self.readable_state(s) {
            RState::Closed => request.close(self, cx),
            RState::Errored => {
                let e = self.readable_stored_error(cx, s);
                request.error(self, cx, e);
            }
            RState::Readable => {
                if let Some(c) = self.rs(s, |s| s.controller) {
                    self.readable_controller_pull_steps(cx, c, request);
                }
            }
        }
    }

    /// ReadableStreamDefaultReaderRelease (ReadableStreamReaderGenericRelease, then the read
    /// requests are errored).
    pub(crate) fn reader_release(&self, cx: &mut Cx, reader: Handle) {
        let Some(s) = self.rr(reader, |r| r.stream) else {
            return;
        };
        let e = type_error(cx, "The reader was released");
        let readable = self.readable_state(s) == RState::Readable;
        let closed = self.rr(reader, |r| r.closed.clone());
        if readable {
            closed.reject(cx, &e);
        } else {
            let d = Deferred::rejected(cx, &e);
            self.rr(reader, |r| r.closed = d.clone());
        }
        let closed = self.rr(reader, |r| r.closed.clone());
        closed.mark_handled(cx);
        // The controller's [[ReleaseSteps]].
        if let Some(c) = self.rs(s, |s| s.controller) {
            if self.is_byte_controller(c) {
                self.byte_release_steps(c);
            }
        }
        self.rs(s, |s| s.reader = None);
        let (requests, into) = self.rr(reader, |r| {
            r.stream = None;
            (
                std::mem::take(&mut r.requests),
                std::mem::take(&mut r.into_requests),
            )
        });
        for request in requests {
            request.error(self, cx, e.clone());
        }
        for request in into {
            request.error(self, cx, e.clone());
        }
    }

    // ----- tee ------------------------------------------------------------------------------------------

    /// ReadableStreamTee: ReadableByteStreamTee for a byte stream, else ReadableStreamDefaultTee,
    /// whose second branch gets a structured clone of each chunk when `clone_for_branch2` (the
    /// "tee a ReadableStream" other specs use, as Fetch's "clone a body" does).
    pub(crate) fn readable_tee(
        &self,
        cx: &mut Cx,
        s: Handle,
        clone_for_branch2: bool,
    ) -> rt::Result<(Handle, Handle)> {
        if self
            .rs(s, |s| s.controller)
            .is_some_and(|c| self.is_byte_controller(c))
        {
            return self.readable_byte_tee(cx, s);
        }
        let reader = self.acquire_default_reader(cx, s)?;
        let cancel_promise = Deferred::new(cx);
        let state = Rc::new(RefCell::new(TeeState {
            stream: s,
            reader,
            clone_for_branch2,
            reading: false,
            read_again: false,
            canceled: [false, false],
            reasons: [None, None],
            branches: [None, None],
            cancel_promise: cancel_promise.clone(),
        }));
        let b1 = self.create_readable_stream(
            cx,
            Rc::new(TeeSource {
                state: state.clone(),
                branch: 0,
            }),
            1.0,
            false,
        );
        let b2 = self.create_readable_stream(
            cx,
            Rc::new(TeeSource {
                state: state.clone(),
                branch: 1,
            }),
            1.0,
            false,
        );
        state.borrow_mut().branches = [Some(b1), Some(b2)];
        let closed = self.rr(reader, |r| r.closed.clone());
        let st = state.clone();
        self.upon_deferred(
            cx,
            &closed,
            Box::new(move |e, cx, r| {
                if let Err(r) = r {
                    let (branches, canceled, cancel) = {
                        let s = st.borrow();
                        (s.branches, s.canceled, s.cancel_promise.clone())
                    };
                    for b in branches.into_iter().flatten() {
                        if let Some(c) = e.rs(b, |s| s.controller) {
                            e.readable_controller_error(cx, c, r.clone());
                        }
                    }
                    if !canceled[0] || !canceled[1] {
                        cancel.resolve_undefined(cx);
                    }
                }
            }),
        );
        Ok((b1, b2))
    }

    // ----- pipes ----------------------------------------------------------------------------------------

    /// ReadableStreamPipeTo.
    pub(crate) fn readable_pipe_to(
        &self,
        cx: &mut Cx,
        source: Handle,
        dest: Handle,
        options: StreamPipeOptions,
    ) -> rt::Result<Deferred> {
        let reader = self.acquire_default_reader(cx, source)?;
        let writer = match self.acquire_writer(cx, dest) {
            Ok(w) => w,
            Err(e) => {
                self.reader_release(cx, reader);
                return Err(e);
            }
        };
        self.rs(source, |s| s.disturbed = true);
        let promise = Deferred::new(cx);
        let current_write = Deferred::resolved_undefined(cx);
        let pipe = Rc::new(RefCell::new(PipeState {
            source,
            dest,
            reader,
            writer,
            prevent_close: options.prevent_close,
            prevent_abort: options.prevent_abort,
            prevent_cancel: options.prevent_cancel,
            signal: options.signal,
            shutting_down: false,
            current_write,
            promise: promise.clone(),
        }));
        if let Some(signal) = options.signal {
            if self.signal_aborted(signal) {
                pipe_abort(self, cx, &pipe);
                return Ok(promise);
            }
            self.register_stream_abort(signal, StreamAbort::Pipe(pipe.clone()));
        }
        // Errors and closings, as soon as they happen.
        let closed = self.rr(reader, |r| r.closed.clone());
        let p = pipe.clone();
        self.upon_deferred(
            cx,
            &closed,
            Box::new(move |e, cx, r| match r {
                Ok(_) => pipe_source_closed(e, cx, &p),
                Err(err) => pipe_source_errored(e, cx, &p, err),
            }),
        );
        let wclosed = self.writer_closed_deferred(writer);
        let p = pipe.clone();
        self.upon_deferred(
            cx,
            &wclosed,
            Box::new(move |e, cx, r| {
                if let Err(err) = r {
                    pipe_dest_errored(e, cx, &p, err);
                }
            }),
        );
        // A stream that is already errored or closed is handled by the reactions above; else pump.
        pipe_pump(self, cx, &pipe);
        Ok(promise)
    }
}

// ----- tee state ----------------------------------------------------------------------------------------

struct TeeState {
    stream: Handle,
    reader: Handle,
    clone_for_branch2: bool,
    reading: bool,
    read_again: bool,
    canceled: [bool; 2],
    reasons: [Option<JsValue>; 2],
    branches: [Option<Handle>; 2],
    cancel_promise: Deferred,
}

struct TeeSource {
    state: Rc<RefCell<TeeState>>,
    branch: usize,
}

struct TeeRead {
    state: Rc<RefCell<TeeState>>,
}

fn tee_pull(e: &Engine, cx: &mut Cx, state: &Rc<RefCell<TeeState>>) {
    let reader = {
        let mut s = state.borrow_mut();
        if s.reading {
            s.read_again = true;
            return;
        }
        s.reading = true;
        s.reader
    };
    e.reader_read(
        cx,
        reader,
        ReadRequest::Steps(Box::new(TeeRead {
            state: state.clone(),
        })),
    );
}

impl NativeSource for TeeSource {
    fn pull(&self, e: &Engine, cx: &mut Cx, _controller: Handle) -> JsValue {
        tee_pull(e, cx, &self.state);
        Deferred::resolved_undefined(cx).value(cx)
    }

    fn cancel(&self, e: &Engine, cx: &mut Cx, reason: JsValue) -> JsValue {
        let (both, stream, cancel_promise) = {
            let mut s = self.state.borrow_mut();
            s.canceled[self.branch] = true;
            s.reasons[self.branch] = Some(reason);
            (
                s.canceled[1 - self.branch],
                s.stream,
                s.cancel_promise.clone(),
            )
        };
        if both {
            let reasons = {
                let s = self.state.borrow();
                [s.reasons[0].clone(), s.reasons[1].clone()]
            };
            let u = undefined(cx);
            let composite = array(
                cx,
                &[
                    reasons[0].clone().unwrap_or_else(|| u.clone()),
                    reasons[1].clone().unwrap_or(u),
                ],
            );
            let result = e.readable_stream_cancel(cx, stream, composite);
            cancel_promise.resolve(cx, &result);
        }
        cancel_promise.value(cx)
    }
}

impl ReadSteps for TeeRead {
    fn chunk(self: Box<Self>, e: &Engine, cx: &mut Cx, chunk: JsValue) {
        let state = self.state;
        e.queue_stream_microtask(cx, move |e, cx| {
            let (branches, canceled, clone, stream, cancel) = {
                let mut s = state.borrow_mut();
                s.read_again = false;
                (
                    s.branches,
                    s.canceled,
                    s.clone_for_branch2,
                    s.stream,
                    s.cancel_promise.clone(),
                )
            };
            let mut chunks = [chunk.clone(), chunk];
            if !canceled[1] && clone {
                match e.structured_clone_caught(cx, &chunks[1]) {
                    Ok(c) => chunks[1] = c,
                    Err(err) => {
                        // A chunk that cannot be cloned errors both branches and cancels the
                        // stream with that error.
                        for b in branches.into_iter().flatten() {
                            if let Some(c) = e.rs(b, |s| s.controller) {
                                e.readable_controller_error(cx, c, err.clone());
                            }
                        }
                        let result = e.readable_stream_cancel(cx, stream, err);
                        cancel.resolve(cx, &result);
                        return;
                    }
                }
            }
            for i in 0..2 {
                if !canceled[i] {
                    if let Some(c) = branches[i].and_then(|b| e.rs(b, |s| s.controller)) {
                        let _ = e.readable_controller_enqueue(cx, c, chunks[i].clone());
                    }
                }
            }
            let again = {
                let mut s = state.borrow_mut();
                s.reading = false;
                s.read_again
            };
            if again {
                tee_pull(e, cx, &state);
            }
        });
    }

    fn close(self: Box<Self>, e: &Engine, cx: &mut Cx) {
        let (branches, canceled, cancel) = {
            let mut s = self.state.borrow_mut();
            s.reading = false;
            (s.branches, s.canceled, s.cancel_promise.clone())
        };
        for i in 0..2 {
            if !canceled[i] {
                if let Some(c) = branches[i].and_then(|b| e.rs(b, |s| s.controller)) {
                    e.readable_controller_close(cx, c);
                }
            }
        }
        if !canceled[0] || !canceled[1] {
            cancel.resolve_undefined(cx);
        }
    }

    fn error(self: Box<Self>, _e: &Engine, _cx: &mut Cx, _error: JsValue) {
        self.state.borrow_mut().reading = false;
    }
}

// ----- pipe state ---------------------------------------------------------------------------------------

pub(crate) struct PipeState {
    source: Handle,
    dest: Handle,
    reader: Handle,
    writer: Handle,
    prevent_close: bool,
    prevent_abort: bool,
    prevent_cancel: bool,
    signal: Option<Handle>,
    shutting_down: bool,
    current_write: Deferred,
    promise: Deferred,
}

/// What an abort signal aborts besides fetches and listeners.
pub(crate) enum StreamAbort {
    Pipe(Rc<RefCell<PipeState>>),
}

impl Engine {
    fn register_stream_abort(&self, signal: Handle, what: StreamAbort) {
        let mut s = self.streams.borrow_mut();
        s.abort_algorithms.push((signal, Some(what)));
    }

    /// The abort algorithms streams added to `signal` (DOM "run the abort steps").
    pub(crate) fn run_stream_abort_algorithms(&self, cx: &mut Cx, signal: Handle) {
        let found: Vec<StreamAbort> = {
            let mut s = self.streams.borrow_mut();
            let mut out = Vec::new();
            for slot in s.abort_algorithms.iter_mut() {
                if slot.0 == signal {
                    if let Some(a) = slot.1.take() {
                        out.push(a);
                    }
                }
            }
            s.abort_algorithms.retain(|a| a.1.is_some());
            out
        };
        for a in found {
            match a {
                StreamAbort::Pipe(p) => pipe_abort(self, cx, &p),
            }
        }
    }

    fn remove_stream_abort(&self, pipe: &Rc<RefCell<PipeState>>) {
        let mut s = self.streams.borrow_mut();
        s.abort_algorithms
            .retain(|a| !matches!(&a.1, Some(StreamAbort::Pipe(p)) if Rc::ptr_eq(p, pipe)));
    }
}

/// The signal aborted: the actions the options allow, then shutdown with its reason.
fn pipe_abort(e: &Engine, cx: &mut Cx, pipe: &Rc<RefCell<PipeState>>) {
    let (signal, prevent_abort, prevent_cancel, source, dest) = {
        let p = pipe.borrow();
        (
            p.signal,
            p.prevent_abort,
            p.prevent_cancel,
            p.source,
            p.dest,
        )
    };
    let error = signal
        .and_then(|s| e.signal_reason(cx, s))
        .unwrap_or_else(|| undefined(cx));
    let err = error.clone();
    shutdown_with_action(
        e,
        cx,
        pipe,
        Box::new(move |e, cx| {
            let mut actions = Vec::new();
            if !prevent_abort {
                actions.push(if e.writable_state(dest) == WState::Writable {
                    e.writable_stream_abort(cx, dest, err.clone()).value(cx)
                } else {
                    Deferred::resolved_undefined(cx).value(cx)
                });
            }
            if !prevent_cancel {
                actions.push(if e.readable_state(source) == RState::Readable {
                    e.readable_stream_cancel(cx, source, err.clone())
                } else {
                    Deferred::resolved_undefined(cx).value(cx)
                });
            }
            all_settled_first_rejection(e, cx, actions)
        }),
        Some(error),
    );
}

/// A promise fulfilled when all of `promises` are, rejected with the first rejection ("waiting for
/// all").
fn all_settled_first_rejection(e: &Engine, cx: &mut Cx, promises: Vec<JsValue>) -> JsValue {
    let out = Deferred::new(cx);
    if promises.is_empty() {
        out.resolve_undefined(cx);
        return out.value(cx);
    }
    let remaining = Rc::new(std::cell::Cell::new(promises.len()));
    for p in promises {
        let out2 = out.clone();
        let remaining = remaining.clone();
        e.upon(
            cx,
            &p,
            Box::new(move |_, cx, r| match r {
                Ok(_) => {
                    remaining.set(remaining.get() - 1);
                    if remaining.get() == 0 {
                        out2.resolve_undefined(cx);
                    }
                }
                Err(err) => out2.reject(cx, &err),
            }),
        );
    }
    out.value(cx)
}

fn pipe_source_errored(e: &Engine, cx: &mut Cx, pipe: &Rc<RefCell<PipeState>>, err: JsValue) {
    let (prevent_abort, dest) = {
        let p = pipe.borrow();
        (p.prevent_abort, p.dest)
    };
    if !prevent_abort {
        let er = err.clone();
        shutdown_with_action(
            e,
            cx,
            pipe,
            Box::new(move |e, cx| e.writable_stream_abort(cx, dest, er).value(cx)),
            Some(err),
        );
    } else {
        shutdown(e, cx, pipe, Some(err));
    }
}

fn pipe_dest_errored(e: &Engine, cx: &mut Cx, pipe: &Rc<RefCell<PipeState>>, err: JsValue) {
    let (prevent_cancel, source) = {
        let p = pipe.borrow();
        (p.prevent_cancel, p.source)
    };
    if !prevent_cancel {
        let er = err.clone();
        shutdown_with_action(
            e,
            cx,
            pipe,
            Box::new(move |e, cx| e.readable_stream_cancel(cx, source, er)),
            Some(err),
        );
    } else {
        shutdown(e, cx, pipe, Some(err));
    }
}

fn pipe_source_closed(e: &Engine, cx: &mut Cx, pipe: &Rc<RefCell<PipeState>>) {
    let (prevent_close, writer) = {
        let p = pipe.borrow();
        (p.prevent_close, p.writer)
    };
    if !prevent_close {
        shutdown_with_action(
            e,
            cx,
            pipe,
            Box::new(move |e, cx| e.writer_close_with_error_propagation(cx, writer).value(cx)),
            None,
        );
    } else {
        shutdown(e, cx, pipe, None);
    }
}

/// The destination is closing or closed: cancel the source (unless prevented) with a TypeError.
fn pipe_dest_closed(e: &Engine, cx: &mut Cx, pipe: &Rc<RefCell<PipeState>>) {
    let (prevent_cancel, source) = {
        let p = pipe.borrow();
        (p.prevent_cancel, p.source)
    };
    let err = type_error(cx, "The destination stream is closed");
    if !prevent_cancel {
        let er = err.clone();
        shutdown_with_action(
            e,
            cx,
            pipe,
            Box::new(move |e, cx| e.readable_stream_cancel(cx, source, er)),
            Some(err),
        );
    } else {
        shutdown(e, cx, pipe, Some(err));
    }
}

/// One step of the pipe loop: wait for the writer to be ready, read a chunk, write it.
fn pipe_pump(e: &Engine, cx: &mut Cx, pipe: &Rc<RefCell<PipeState>>) {
    let (shutting_down, dest, writer) = {
        let p = pipe.borrow();
        (p.shutting_down, p.dest, p.writer)
    };
    if shutting_down {
        return;
    }
    if e.writable_close_queued_or_in_flight(dest) || e.writable_state(dest) == WState::Closed {
        pipe_dest_closed(e, cx, pipe);
        return;
    }
    let ready = e.writer_ready_deferred(writer);
    let p = pipe.clone();
    e.upon_deferred(
        cx,
        &ready,
        Box::new(move |e, cx, r| {
            if r.is_err() || p.borrow().shutting_down {
                return;
            }
            let (source, reader, dest) = {
                let s = p.borrow();
                (s.source, s.reader, s.dest)
            };
            if e.readable_state(source) != RState::Readable
                || e.writable_state(dest) != WState::Writable
            {
                return;
            }
            if e.writable_close_queued_or_in_flight(dest) {
                pipe_dest_closed(e, cx, &p);
                return;
            }
            e.reader_read(
                cx,
                reader,
                ReadRequest::Steps(Box::new(PipeRead { pipe: p.clone() })),
            );
        }),
    );
}

struct PipeRead {
    pipe: Rc<RefCell<PipeState>>,
}

impl ReadSteps for PipeRead {
    fn chunk(self: Box<Self>, e: &Engine, cx: &mut Cx, chunk: JsValue) {
        // (written in a microtask: a chunk read in `enqueue()` must not reach the destination's write
        // algorithm synchronously, as the spec's "in parallel" pipe loop would not)
        let pipe = self.pipe;
        e.queue_stream_microtask(cx, move |e, cx| {
            let writer = pipe.borrow().writer;
            let w = e.writer_write(cx, writer, chunk);
            w.mark_handled(cx);
            pipe.borrow_mut().current_write = w;
            pipe_pump(e, cx, &pipe);
        });
    }

    // (closing and erroring are seen through the reader's closed promise)
    fn close(self: Box<Self>, _e: &Engine, _cx: &mut Cx) {}

    fn error(self: Box<Self>, _e: &Engine, _cx: &mut Cx, _error: JsValue) {}
}

type PipeAction = Box<dyn FnOnce(&Engine, &mut Cx) -> JsValue>;

/// "Shutdown with an action".
fn shutdown_with_action(
    e: &Engine,
    cx: &mut Cx,
    pipe: &Rc<RefCell<PipeState>>,
    action: PipeAction,
    original_error: Option<JsValue>,
) {
    if std::mem::replace(&mut pipe.borrow_mut().shutting_down, true) {
        return;
    }
    let p = pipe.clone();
    let rest: Then = Box::new(move |e, cx| {
        let promise = action(e, cx);
        let p2 = p.clone();
        e.upon(
            cx,
            &promise,
            Box::new(move |e, cx, r| match r {
                Ok(_) => finalize(e, cx, &p2, original_error),
                Err(err) => finalize(e, cx, &p2, Some(err)),
            }),
        );
    });
    after_pending_writes(e, cx, pipe, rest);
}

/// "Shutdown".
fn shutdown(e: &Engine, cx: &mut Cx, pipe: &Rc<RefCell<PipeState>>, error: Option<JsValue>) {
    if std::mem::replace(&mut pipe.borrow_mut().shutting_down, true) {
        return;
    }
    let p = pipe.clone();
    after_pending_writes(
        e,
        cx,
        pipe,
        Box::new(move |e, cx| finalize(e, cx, &p, error)),
    );
}

/// When the destination is still writable and not closing, waits until every chunk read has been
/// written before `then`.
fn after_pending_writes(e: &Engine, cx: &mut Cx, pipe: &Rc<RefCell<PipeState>>, then: Then) {
    let (dest, current) = {
        let p = pipe.borrow();
        (p.dest, p.current_write.clone())
    };
    if e.writable_state(dest) == WState::Writable && !e.writable_close_queued_or_in_flight(dest) {
        let p = pipe.clone();
        let waited = current.clone();
        e.upon_deferred(
            cx,
            &current,
            Box::new(move |e, cx, _| {
                // (a write may have started meanwhile)
                let now = p.borrow().current_write.clone();
                if now.pending() && !now.same(&waited) {
                    after_pending_writes(e, cx, &p, then);
                } else {
                    then(e, cx);
                }
            }),
        );
    } else {
        then(e, cx);
    }
}

/// "Finalize".
fn finalize(e: &Engine, cx: &mut Cx, pipe: &Rc<RefCell<PipeState>>, error: Option<JsValue>) {
    let (writer, reader, promise) = {
        let p = pipe.borrow();
        (p.writer, p.reader, p.promise.clone())
    };
    e.writer_release(cx, writer);
    e.reader_release(cx, reader);
    e.remove_stream_abort(pipe);
    match error {
        Some(err) => promise.reject(cx, &err),
        None => promise.resolve_undefined(cx),
    }
}

// ----- underlying sources from script -----------------------------------------------------------------

/// A JS underlying source's start algorithm.
fn js_start(this: JsValue, start: Option<JsFn>) -> StartAlgorithm {
    Box::new(move |_, cx, c| match start {
        Some(f) => {
            let cv = handle_value(cx, c);
            call(cx, &f, &this, &[cv])
        }
        None => Ok(undefined(cx)),
    })
}

impl ReadableStreamImpl for Engine {
    fn locked(&self, this: Handle) -> bool {
        self.readable_locked(this)
    }

    fn cancel(&self, cx: &mut Cx, this: Handle, reason: Option<JsValue>) -> rt::Result<JsPromise> {
        if self.readable_locked(this) {
            let e = type_error(cx, "The stream is locked to a reader");
            return Ok(Deferred::rejected(cx, &e).p);
        }
        let reason = reason.unwrap_or_else(|| undefined(cx));
        let p = self.readable_stream_cancel(cx, this, reason);
        Ok(promise_of(cx, &p))
    }

    fn get_reader(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: ReadableStreamGetReaderOptions,
    ) -> rt::Result<ReaderUnion> {
        match options.mode {
            None => Ok(ReaderUnion::ReadableStreamDefaultReader(
                self.acquire_default_reader(cx, this)?,
            )),
            Some(ReadableStreamReaderMode::Byob) => Ok(ReaderUnion::ReadableStreamBYOBReader(
                self.acquire_byob_reader(cx, this)?,
            )),
        }
    }

    fn pipe_through(
        &self,
        cx: &mut Cx,
        this: Handle,
        transform: ReadableWritablePair,
        options: StreamPipeOptions,
    ) -> rt::Result<Handle> {
        if self.readable_locked(this) {
            return Err(rt::Error::Type("The stream is locked to a reader".into()));
        }
        if self.writable_locked(transform.writable) {
            return Err(rt::Error::Type(
                "The transform's writable side is locked".into(),
            ));
        }
        let p = self.readable_pipe_to(cx, this, transform.writable, options)?;
        p.mark_handled(cx);
        Ok(transform.readable)
    }

    fn pipe_to(
        &self,
        cx: &mut Cx,
        this: Handle,
        destination: Handle,
        options: StreamPipeOptions,
    ) -> rt::Result<JsPromise> {
        if self.readable_locked(this) {
            let e = type_error(cx, "The stream is locked to a reader");
            return Ok(Deferred::rejected(cx, &e).p);
        }
        if self.writable_locked(destination) {
            let e = type_error(cx, "The destination is locked to a writer");
            return Ok(Deferred::rejected(cx, &e).p);
        }
        match self.readable_pipe_to(cx, this, destination, options) {
            Ok(p) => Ok(p.p),
            Err(e) => {
                let e = error_value(cx, e);
                Ok(Deferred::rejected(cx, &e).p)
            }
        }
    }

    fn tee(&self, cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        let (a, b) = self.readable_tee(cx, this, false)?;
        Ok(vec![a, b])
    }

    fn async_iterator_init(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: ReadableStreamIteratorOptions,
    ) -> rt::Result<Handle> {
        self.readable_async_iterator_init(cx, this, options.prevent_cancel)
    }

    fn async_iterator_next(
        &self,
        cx: &mut Cx,
        _this: Handle,
        state: Handle,
    ) -> rt::Result<JsPromise> {
        Ok(self.readable_async_iterator_next(cx, state))
    }

    fn async_iterator_return(
        &self,
        cx: &mut Cx,
        _this: Handle,
        state: Handle,
        value: JsValue,
    ) -> rt::Result<JsPromise> {
        Ok(self.readable_async_iterator_return(cx, state, value))
    }

    fn static_from(&self, cx: &mut Cx, async_iterable: rt::AsyncSequence) -> rt::Result<Handle> {
        self.readable_stream_from_iterable(cx, async_iterable)
            .map_err(|e| throw(cx, e))
    }

    fn constructor(
        &self,
        cx: &mut Cx,
        underlying_source: Option<JsObject>,
        strategy: QueuingStrategy,
    ) -> rt::Result<Handle> {
        let this = match &underlying_source {
            Some(o) => {
                let scope = cx.scope();
                let l = v8::Local::new(scope, o.global());
                JsValue::new(scope, l.into())
            }
            None => undefined(cx),
        };
        let pending = |cx: &mut Cx, e: JsValue| -> rt::Error {
            let scope = cx.scope();
            let v = v8::Local::new(scope, e.global());
            scope.throw_exception(v);
            rt::Error::Pending
        };
        // The UnderlyingSource dictionary, its members in lexicographic order.
        let auto = get(cx, &this, "autoAllocateChunkSize").map_err(|e| pending(cx, e))?;
        let auto = if auto.is_undefined(cx) {
            None
        } else {
            Some(enforce_range_u64(cx, &auto)?)
        };
        let cancel = get_callback(cx, &this, "cancel").map_err(|e| pending(cx, e))?;
        let pull = get_callback(cx, &this, "pull").map_err(|e| pending(cx, e))?;
        let start = get_callback(cx, &this, "start").map_err(|e| pending(cx, e))?;
        let type_ = get(cx, &this, "type").map_err(|e| pending(cx, e))?;
        let byte = if type_.is_undefined(cx) {
            false
        } else {
            // (the enum conversion's ToString may throw: that exception propagates)
            let s = {
                let scope = cx.scope();
                v8::tc_scope!(let tc, scope);
                let l = v8::Local::new(tc, type_.global());
                match l.to_string(tc) {
                    Some(s) => Ok(s.to_rust_string_lossy(tc)),
                    None => {
                        let e = tc
                            .exception()
                            .map(|e| JsValue::new(tc, e))
                            .unwrap_or_else(|| rt::undefined_value(tc));
                        if !tc.has_terminated() {
                            tc.reset();
                        }
                        Err(e)
                    }
                }
            };
            let s = match s {
                Ok(s) => Some(s),
                Err(e) => return Err(pending(cx, e)),
            };
            match s.as_deref() {
                Some("bytes") => true,
                _ => {
                    return Err(rt::Error::Type(
                        "The underlying source's type must be \"bytes\"".into(),
                    ))
                }
            }
        };
        let s = self.new_readable_stream();
        let (hwm, size) = if byte {
            if strategy.size.is_some() {
                return Err(rt::Error::Range(
                    "A byte stream's strategy can't have a size function".into(),
                ));
            }
            (extract_high_water_mark(&strategy, 0.0)?, None)
        } else {
            let size = extract_size(cx, &strategy);
            (extract_high_water_mark(&strategy, 1.0)?, size)
        };
        // SetUpReadableByteStreamControllerFromUnderlyingSource.
        if byte && auto == Some(0) {
            return Err(rt::Error::Type(
                "autoAllocateChunkSize must be positive".into(),
            ));
        }
        let bytes = byte.then(|| ByteState {
            auto_allocate_chunk_size: auto,
            ..ByteState::default()
        });
        self.set_up_readable_controller(
            cx,
            s,
            bytes,
            js_start(this.clone(), start),
            Source::Js { this, pull, cancel },
            hwm,
            size,
        )
        .map_err(|e| pending(cx, e))?;
        Ok(s)
    }
}

/// A JsPromise that follows a promise value (for returning an algorithm's promise).
fn promise_of(cx: &mut Cx, p: &JsValue) -> JsPromise {
    let d = Deferred::new(cx);
    d.resolve(cx, p);
    d.p
}

impl ReadableStreamDefaultReaderImpl for Engine {
    fn read(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        if self.rr(this, |r| r.stream.is_none()) {
            let e = type_error(cx, "The reader is released");
            return Ok(Deferred::rejected(cx, &e).p);
        }
        let p = Deferred::new(cx);
        self.reader_read(cx, this, ReadRequest::Promise(p.clone()));
        Ok(p.p)
    }

    fn release_lock(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.reader_release(cx, this);
        Ok(())
    }

    fn constructor(&self, cx: &mut Cx, stream: Handle) -> rt::Result<Handle> {
        self.acquire_default_reader(cx, stream)
    }
}

impl ReadableStreamGenericReaderImpl for Engine {
    fn closed(&self, _cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        Ok(self.rr(this, |r| r.closed.p.clone()))
    }

    fn cancel(&self, cx: &mut Cx, this: Handle, reason: Option<JsValue>) -> rt::Result<JsPromise> {
        let Some(s) = self.rr(this, |r| r.stream) else {
            let e = type_error(cx, "The reader is released");
            return Ok(Deferred::rejected(cx, &e).p);
        };
        let reason = reason.unwrap_or_else(|| undefined(cx));
        let p = self.readable_stream_cancel(cx, s, reason);
        Ok(promise_of(cx, &p))
    }
}

impl ReadableStreamBYOBReaderImpl for Engine {
    fn read(
        &self,
        cx: &mut Cx,
        this: Handle,
        view: ArrayBufferView,
        options: ReadableStreamBYOBReaderReadOptions,
    ) -> rt::Result<JsPromise> {
        let v = object_value(cx, view_object(&view));
        Ok(self.byob_read_method(cx, this, &v, options.min).p)
    }

    fn release_lock(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        // ReadableStreamBYOBReaderRelease.
        self.reader_release(cx, this);
        Ok(())
    }

    fn constructor(&self, cx: &mut Cx, stream: Handle) -> rt::Result<Handle> {
        self.acquire_byob_reader(cx, stream)
    }
}

/// An object as a value.
fn object_value(cx: &mut Cx, o: &JsObject) -> JsValue {
    let scope = cx.scope();
    let l = v8::Local::new(scope, o.global());
    JsValue::new(scope, l.into())
}

/// Web IDL's `[EnforceRange] unsigned long long` conversion: a TypeError for a non-finite value or
/// one out of range after truncation.
fn enforce_range_u64(cx: &mut Cx, v: &JsValue) -> rt::Result<u64> {
    let n = {
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let l = v8::Local::new(tc, v.global());
        match l.number_value(tc) {
            Some(n) => n,
            None => {
                if let Some(e) = tc.exception() {
                    let e = v8::Global::new(tc, e);
                    tc.reset();
                    let e = v8::Local::new(tc, &e);
                    tc.throw_exception(e);
                }
                return Err(rt::Error::Pending);
            }
        }
    };
    let n = n.trunc();
    if !n.is_finite() || !(0.0..=9_007_199_254_740_991.0).contains(&n) {
        return Err(rt::Error::Type("The value is out of range".into()));
    }
    Ok(n as u64)
}

impl ReadableStreamDefaultControllerImpl for Engine {
    fn desired_size(&self, this: Handle) -> Option<f64> {
        self.readable_controller_desired_size(this)
    }

    fn close(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        if !self.readable_controller_can_close_or_enqueue(this) {
            return Err(rt::Error::Type("The stream can't be closed".into()));
        }
        self.readable_controller_close(cx, this);
        Ok(())
    }

    fn enqueue(&self, cx: &mut Cx, this: Handle, chunk: Option<JsValue>) -> rt::Result<()> {
        if !self.readable_controller_can_close_or_enqueue(this) {
            return Err(rt::Error::Type("The stream can't take more chunks".into()));
        }
        let chunk = chunk.unwrap_or_else(|| undefined(cx));
        self.readable_controller_enqueue(cx, this, chunk)
            .map_err(|e| throw(cx, e))
    }

    fn error(&self, cx: &mut Cx, this: Handle, e: Option<JsValue>) -> rt::Result<()> {
        let e = e.unwrap_or_else(|| undefined(cx));
        self.readable_controller_error(cx, this, e);
        Ok(())
    }
}

/// Throws `e` in the calling scope (a pending exception).
pub(crate) fn throw(cx: &mut Cx, e: JsValue) -> rt::Error {
    let scope = cx.scope();
    let v = v8::Local::new(scope, e.global());
    scope.throw_exception(v);
    rt::Error::Pending
}

impl ReadableByteStreamControllerImpl for Engine {
    fn byob_request(&self, this: Handle) -> Option<Handle> {
        self.byte_get_byob_request(this)
    }

    fn desired_size(&self, this: Handle) -> Option<f64> {
        self.readable_controller_desired_size(this)
    }

    fn close(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        if !self.readable_controller_can_close_or_enqueue(this) {
            return Err(rt::Error::Type("The stream can't be closed".into()));
        }
        self.byte_controller_close(cx, this)
            .map_err(|e| throw(cx, e))
    }

    fn enqueue(&self, cx: &mut Cx, this: Handle, chunk: ArrayBufferView) -> rt::Result<()> {
        let v = object_value(cx, view_object(&chunk));
        let Some(info) = view_info(cx, &v) else {
            return Err(rt::Error::Type(
                "The chunk is not an ArrayBufferView".into(),
            ));
        };
        if info.length == 0 {
            return Err(rt::Error::Type("The chunk is empty".into()));
        }
        if info.buffer_length == 0 {
            return Err(rt::Error::Type("The chunk's buffer is empty".into()));
        }
        if !self.readable_controller_can_close_or_enqueue(this) {
            return Err(rt::Error::Type("The stream can't take more chunks".into()));
        }
        self.byte_controller_enqueue(cx, this, &v)
            .map_err(|e| throw(cx, e))
    }

    fn error(&self, cx: &mut Cx, this: Handle, e: Option<JsValue>) -> rt::Result<()> {
        let e = e.unwrap_or_else(|| undefined(cx));
        self.readable_controller_error(cx, this, e);
        Ok(())
    }
}

impl ReadableStreamBYOBRequestImpl for Engine {
    fn view(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<JsObject>> {
        Ok(self.byob_request_view(cx, this))
    }

    fn respond(&self, cx: &mut Cx, this: Handle, bytes_written: u64) -> rt::Result<()> {
        let Some(controller) = self.byob_request_controller(this) else {
            return Err(rt::Error::Type("The request was already answered".into()));
        };
        if let Some(view) = self.byob_request_view(cx, this) {
            let v = object_value(cx, &view);
            if view_info(cx, &v).is_none_or(|i| i.detached) {
                return Err(rt::Error::Type("The request's buffer is detached".into()));
            }
        }
        self.byte_respond(cx, controller, bytes_written)
            .map_err(|e| throw(cx, e))
    }

    fn respond_with_new_view(
        &self,
        cx: &mut Cx,
        this: Handle,
        view: ArrayBufferView,
    ) -> rt::Result<()> {
        let Some(controller) = self.byob_request_controller(this) else {
            return Err(rt::Error::Type("The request was already answered".into()));
        };
        let v = object_value(cx, view_object(&view));
        if view_info(cx, &v).is_none_or(|i| i.detached) {
            return Err(rt::Error::Type("The view's buffer is detached".into()));
        }
        self.byte_respond_with_new_view(cx, controller, &v)
            .map_err(|e| throw(cx, e))
    }
}

// ----- engine sources -----------------------------------------------------------------------------------

/// A source that enqueues one chunk (unless empty) and closes: a buffered body as a byte stream.
pub(crate) struct BytesSource {
    pub bytes: RefCell<Option<Vec<u8>>>,
}

impl NativeSource for BytesSource {
    fn start(&self, e: &Engine, cx: &mut Cx, controller: Handle) -> Result<JsValue, JsValue> {
        if let Some(bytes) = self.bytes.borrow_mut().take() {
            if !bytes.is_empty() {
                let v = uint8_value(cx, bytes);
                e.readable_controller_enqueue(cx, controller, v)?;
            }
        }
        e.readable_controller_close(cx, controller);
        Ok(undefined(cx))
    }
}

/// A source that enqueues one string chunk (unless empty) and closes.
pub(crate) struct TextSource {
    pub text: RefCell<Option<String>>,
}

impl NativeSource for TextSource {
    fn start(&self, e: &Engine, cx: &mut Cx, controller: Handle) -> Result<JsValue, JsValue> {
        if let Some(text) = self.text.borrow_mut().take() {
            if !text.is_empty() {
                let v = {
                    let scope = cx.scope();
                    let s = v8::String::new(scope, &text)
                        .map(|s| s.into())
                        .unwrap_or_else(|| v8::undefined(scope).into());
                    JsValue::new(scope, s)
                };
                e.readable_controller_enqueue(cx, controller, v)?;
            }
        }
        e.readable_controller_close(cx, controller);
        Ok(undefined(cx))
    }
}

/// Reads a stream to its end ("read all bytes"): `done` gets the bytes, or the error (a chunk that
/// is not a Uint8Array is a TypeError).
pub(crate) fn read_all_bytes(e: &Engine, cx: &mut Cx, reader: Handle, done: BytesDone) {
    let state = Rc::new(RefCell::new(ReadAll {
        bytes: Vec::new(),
        done: Some(done),
        reader,
    }));
    read_all_step(e, cx, &state);
}

struct ReadAll {
    bytes: Vec<u8>,
    done: Option<BytesDone>,
    reader: Handle,
}

struct ReadAllStep(Rc<RefCell<ReadAll>>);

fn read_all_step(e: &Engine, cx: &mut Cx, state: &Rc<RefCell<ReadAll>>) {
    let reader = state.borrow().reader;
    e.reader_read(
        cx,
        reader,
        ReadRequest::Steps(Box::new(ReadAllStep(state.clone()))),
    );
}

impl ReadSteps for ReadAllStep {
    fn chunk(self: Box<Self>, e: &Engine, cx: &mut Cx, chunk: JsValue) {
        let is_u8 = {
            let scope = cx.scope();
            v8::Local::new(scope, chunk.global()).is_uint8_array()
        };
        if !is_u8 {
            let err = type_error(cx, "A body chunk is not a Uint8Array");
            let done = self.0.borrow_mut().done.take();
            if let Some(done) = done {
                done(e, cx, Err(err));
            }
            return;
        }
        if let Some(b) = view_bytes(cx, &chunk) {
            self.0.borrow_mut().bytes.extend_from_slice(&b);
        }
        // (the next read in a microtask, so a long stream does not recurse)
        let state = self.0.clone();
        e.queue_stream_microtask(cx, move |e, cx| read_all_step(e, cx, &state));
    }

    fn close(self: Box<Self>, e: &Engine, cx: &mut Cx) {
        let (done, bytes) = {
            let mut s = self.0.borrow_mut();
            (s.done.take(), std::mem::take(&mut s.bytes))
        };
        if let Some(done) = done {
            done(e, cx, Ok(bytes));
        }
    }

    fn error(self: Box<Self>, e: &Engine, cx: &mut Cx, error: JsValue) {
        let done = self.0.borrow_mut().done.take();
        if let Some(done) = done {
            done(e, cx, Err(error));
        }
    }
}
