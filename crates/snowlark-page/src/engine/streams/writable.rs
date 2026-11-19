// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Streams §5: `WritableStream`, its default writer and default controller.

use std::collections::VecDeque;
use std::rc::Rc;

use snowlark_bindings::generated::{
    QueuingStrategy, WritableStreamDefaultControllerImpl, WritableStreamDefaultWriterImpl,
    WritableStreamImpl,
};
use snowlark_bindings::rt::{self, Cx, Handle, JsObject, JsPromise, JsValue};

use super::super::{Engine, Object};
use super::readable::{call_size, extract_size, throw};
use super::{
    call, error_value, extract_high_water_mark, get, get_callback, handle_value, type_error,
    undefined, Deferred, JsFn, StartAlgorithm,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WState {
    Writable,
    Closed,
    Erroring,
    Errored,
}

struct PendingAbort {
    promise: Deferred,
    reason: JsValue,
    was_already_erroring: bool,
}

pub struct WritableStreamData {
    state: WState,
    stored_error: Option<JsValue>,
    writer: Option<Handle>,
    controller: Option<Handle>,
    in_flight_write: Option<Deferred>,
    close_request: Option<Deferred>,
    in_flight_close: Option<Deferred>,
    pending_abort: Option<PendingAbort>,
    write_requests: VecDeque<Deferred>,
    backpressure: bool,
}

pub struct WriterData {
    stream: Option<Handle>,
    closed: Deferred,
    ready: Deferred,
}

/// An underlying sink the engine provides (a transform stream's writable side).
pub(crate) trait NativeSink {
    fn start(&self, _e: &Engine, cx: &mut Cx) -> Result<JsValue, JsValue> {
        Ok(undefined(cx))
    }
    fn write(&self, e: &Engine, cx: &mut Cx, chunk: JsValue) -> JsValue;
    fn close(&self, e: &Engine, cx: &mut Cx) -> JsValue;
    fn abort(&self, e: &Engine, cx: &mut Cx, reason: JsValue) -> JsValue;
}

pub(crate) enum Sink {
    Js {
        this: JsValue,
        write: Option<JsFn>,
        close: Option<JsFn>,
        abort: Option<JsFn>,
    },
    Native(Rc<dyn NativeSink>),
}

enum QueueItem {
    Chunk(JsValue),
    /// The close sentinel.
    Close,
}

pub struct WritableControllerData {
    stream: Handle,
    queue: VecDeque<(QueueItem, f64)>,
    queue_total: f64,
    started: bool,
    high_water_mark: f64,
    size: Option<JsFn>,
    sink: Option<Sink>,
    signal: Handle,
}

impl Engine {
    fn ws<R>(&self, h: Handle, f: impl FnOnce(&mut WritableStreamData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::WritableStream(s) => f(s),
            _ => panic!("not a WritableStream"),
        }
    }

    fn wc<R>(&self, h: Handle, f: impl FnOnce(&mut WritableControllerData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::WritableController(c) => f(c),
            _ => panic!("not a WritableStreamDefaultController"),
        }
    }

    fn ww<R>(&self, h: Handle, f: impl FnOnce(&mut WriterData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::WritableStreamDefaultWriter(w) => f(w),
            _ => panic!("not a WritableStreamDefaultWriter"),
        }
    }

    pub(crate) fn writable_state(&self, s: Handle) -> WState {
        self.ws(s, |s| s.state)
    }

    /// IsWritableStreamLocked.
    pub(crate) fn writable_locked(&self, s: Handle) -> bool {
        self.ws(s, |s| s.writer.is_some())
    }

    pub(crate) fn writable_stored_error_value(&self, cx: &mut Cx, s: Handle) -> JsValue {
        self.ws(s, |s| s.stored_error.clone())
            .unwrap_or_else(|| undefined(cx))
    }

    pub(crate) fn writable_controller_of(&self, s: Handle) -> Option<Handle> {
        self.ws(s, |s| s.controller)
    }

    fn new_writable_stream(&self) -> Handle {
        self.alloc(Object::WritableStream(Box::new(WritableStreamData {
            state: WState::Writable,
            stored_error: None,
            writer: None,
            controller: None,
            in_flight_write: None,
            close_request: None,
            in_flight_close: None,
            pending_abort: None,
            write_requests: VecDeque::new(),
            backpressure: false,
        })))
    }

    /// CreateWritableStream with a native sink.
    pub(crate) fn create_writable_stream(
        &self,
        cx: &mut Cx,
        sink: Rc<dyn NativeSink>,
        high_water_mark: f64,
        size: Option<JsFn>,
    ) -> Handle {
        let s = self.new_writable_stream();
        let start = sink.clone();
        let _ = self.set_up_writable_controller(
            cx,
            s,
            Box::new(move |e, cx, _| start.start(e, cx)),
            Sink::Native(sink),
            high_water_mark,
            size,
        );
        s
    }

    /// SetUpWritableStreamDefaultController.
    fn set_up_writable_controller(
        &self,
        cx: &mut Cx,
        stream: Handle,
        start: StartAlgorithm,
        sink: Sink,
        high_water_mark: f64,
        size: Option<JsFn>,
    ) -> Result<(), JsValue> {
        let signal = self.new_abort_signal();
        let c = self.alloc(Object::WritableController(Box::new(
            WritableControllerData {
                stream,
                queue: VecDeque::new(),
                queue_total: 0.0,
                started: false,
                high_water_mark,
                size,
                sink: Some(sink),
                signal,
            },
        )));
        self.ws(stream, |s| s.controller = Some(c));
        let bp = self.writable_controller_backpressure(c);
        self.writable_update_backpressure(cx, stream, bp);
        let result = start(self, cx, c)?;
        self.upon(
            cx,
            &result,
            Box::new(move |e, cx, r| match r {
                Ok(_) => {
                    e.wc(c, |c| c.started = true);
                    e.writable_controller_advance_queue_if_needed(cx, c);
                }
                Err(r) => {
                    e.wc(c, |c| c.started = true);
                    e.writable_deal_with_rejection(cx, stream, r);
                }
            }),
        );
        Ok(())
    }

    // ----- §5.5 the stream ------------------------------------------------------------------------------

    /// WritableStreamAbort.
    pub(crate) fn writable_stream_abort(
        &self,
        cx: &mut Cx,
        s: Handle,
        reason: JsValue,
    ) -> Deferred {
        if matches!(self.writable_state(s), WState::Closed | WState::Errored) {
            return Deferred::resolved_undefined(cx);
        }
        if let Some(c) = self.writable_controller_of(s) {
            let signal = self.wc(c, |c| c.signal);
            self.signal_abort(cx, signal, Some(reason.clone()));
        }
        let state = self.writable_state(s);
        if matches!(state, WState::Closed | WState::Errored) {
            return Deferred::resolved_undefined(cx);
        }
        if let Some(p) = self.ws(s, |s| s.pending_abort.as_ref().map(|a| a.promise.clone())) {
            return p;
        }
        let was_already_erroring = state == WState::Erroring;
        let reason = if was_already_erroring {
            undefined(cx)
        } else {
            reason
        };
        let promise = Deferred::new(cx);
        self.ws(s, |s| {
            s.pending_abort = Some(PendingAbort {
                promise: promise.clone(),
                reason: reason.clone(),
                was_already_erroring,
            })
        });
        if !was_already_erroring {
            self.writable_start_erroring(cx, s, reason);
        }
        promise
    }

    /// WritableStreamClose.
    fn writable_stream_close(&self, cx: &mut Cx, s: Handle) -> Deferred {
        if matches!(self.writable_state(s), WState::Closed | WState::Errored) {
            let e = type_error(cx, "The stream is closed or errored");
            return Deferred::rejected(cx, &e);
        }
        let promise = Deferred::new(cx);
        let (writer, bp, state) = self.ws(s, |s| {
            s.close_request = Some(promise.clone());
            (s.writer, s.backpressure, s.state)
        });
        if let Some(w) = writer {
            if bp && state == WState::Writable {
                let ready = self.ww(w, |w| w.ready.clone());
                ready.resolve_undefined(cx);
            }
        }
        if let Some(c) = self.writable_controller_of(s) {
            // WritableStreamDefaultControllerClose.
            self.wc(c, |c| c.queue.push_back((QueueItem::Close, 0.0)));
            self.writable_controller_advance_queue_if_needed(cx, c);
        }
        promise
    }

    /// WritableStreamCloseQueuedOrInFlight.
    pub(crate) fn writable_close_queued_or_in_flight(&self, s: Handle) -> bool {
        self.ws(s, |s| {
            s.close_request.is_some() || s.in_flight_close.is_some()
        })
    }

    /// WritableStreamDealWithRejection.
    fn writable_deal_with_rejection(&self, cx: &mut Cx, s: Handle, error: JsValue) {
        if self.writable_state(s) == WState::Writable {
            self.writable_start_erroring(cx, s, error);
            return;
        }
        self.writable_finish_erroring(cx, s);
    }

    /// WritableStreamStartErroring.
    fn writable_start_erroring(&self, cx: &mut Cx, s: Handle, reason: JsValue) {
        let (writer, controller) = self.ws(s, |s| {
            s.state = WState::Erroring;
            s.stored_error = Some(reason.clone());
            (s.writer, s.controller)
        });
        if let Some(w) = writer {
            self.writer_ensure_ready_rejected(cx, w, reason);
        }
        let in_flight = self.ws(s, |s| {
            s.in_flight_write.is_some() || s.in_flight_close.is_some()
        });
        let started = controller.is_some_and(|c| self.wc(c, |c| c.started));
        if !in_flight && started {
            self.writable_finish_erroring(cx, s);
        }
    }

    /// WritableStreamFinishErroring.
    fn writable_finish_erroring(&self, cx: &mut Cx, s: Handle) {
        let (controller, requests, abort) = self.ws(s, |s| {
            s.state = WState::Errored;
            (
                s.controller,
                std::mem::take(&mut s.write_requests),
                s.pending_abort.take(),
            )
        });
        if let Some(c) = controller {
            // [[ErrorSteps]]: ResetQueue.
            self.wc(c, |c| {
                c.queue.clear();
                c.queue_total = 0.0;
            });
        }
        let stored = self.writable_stored_error_value(cx, s);
        for r in requests {
            r.reject(cx, &stored);
        }
        let Some(abort) = abort else {
            self.writable_reject_close_and_closed(cx, s);
            return;
        };
        if abort.was_already_erroring {
            abort.promise.reject(cx, &stored);
            self.writable_reject_close_and_closed(cx, s);
            return;
        }
        let promise = match controller {
            Some(c) => self.writable_controller_abort_steps(cx, c, abort.reason.clone()),
            None => Deferred::resolved_undefined(cx).value(cx),
        };
        let p = abort.promise;
        self.upon(
            cx,
            &promise,
            Box::new(move |e, cx, r| {
                // (rejected with the sink's abort rejection, not the abort reason)
                match r {
                    Ok(_) => p.resolve_undefined(cx),
                    Err(r) => p.reject(cx, &r),
                }
                e.writable_reject_close_and_closed(cx, s);
            }),
        );
    }

    /// WritableStreamRejectCloseAndClosedPromiseIfNeeded.
    fn writable_reject_close_and_closed(&self, cx: &mut Cx, s: Handle) {
        let stored = self.writable_stored_error_value(cx, s);
        let (close, writer) = self.ws(s, |s| (s.close_request.take(), s.writer));
        if let Some(c) = close {
            c.reject(cx, &stored);
        }
        if let Some(w) = writer {
            let closed = self.ww(w, |w| w.closed.clone());
            closed.reject(cx, &stored);
            closed.mark_handled(cx);
        }
    }

    /// WritableStreamUpdateBackpressure.
    fn writable_update_backpressure(&self, cx: &mut Cx, s: Handle, bp: bool) {
        let (writer, old) = self.ws(s, |s| (s.writer, s.backpressure));
        if let Some(w) = writer {
            if bp != old {
                if bp {
                    let d = Deferred::new(cx);
                    self.ww(w, |w| w.ready = d);
                } else {
                    let ready = self.ww(w, |w| w.ready.clone());
                    ready.resolve_undefined(cx);
                }
            }
        }
        self.ws(s, |s| s.backpressure = bp);
    }

    fn writable_finish_in_flight_write(&self, cx: &mut Cx, s: Handle) {
        if let Some(p) = self.ws(s, |s| s.in_flight_write.take()) {
            p.resolve_undefined(cx);
        }
    }

    fn writable_finish_in_flight_write_with_error(&self, cx: &mut Cx, s: Handle, error: JsValue) {
        if let Some(p) = self.ws(s, |s| s.in_flight_write.take()) {
            p.reject(cx, &error);
        }
        self.writable_deal_with_rejection(cx, s, error);
    }

    fn writable_finish_in_flight_close(&self, cx: &mut Cx, s: Handle) {
        if let Some(p) = self.ws(s, |s| s.in_flight_close.take()) {
            p.resolve_undefined(cx);
        }
        let abort = self.ws(s, |s| {
            let a = if s.state == WState::Erroring {
                s.stored_error = None;
                s.pending_abort.take()
            } else {
                None
            };
            s.state = WState::Closed;
            a
        });
        if let Some(a) = abort {
            a.promise.resolve_undefined(cx);
        }
        if let Some(w) = self.ws(s, |s| s.writer) {
            let closed = self.ww(w, |w| w.closed.clone());
            closed.resolve_undefined(cx);
        }
    }

    fn writable_finish_in_flight_close_with_error(&self, cx: &mut Cx, s: Handle, error: JsValue) {
        if let Some(p) = self.ws(s, |s| s.in_flight_close.take()) {
            p.reject(cx, &error);
        }
        if let Some(a) = self.ws(s, |s| s.pending_abort.take()) {
            a.promise.reject(cx, &error);
        }
        self.writable_deal_with_rejection(cx, s, error);
    }

    // ----- §5.6 writers -------------------------------------------------------------------------------

    /// AcquireWritableStreamDefaultWriter (SetUpWritableStreamDefaultWriter).
    pub(crate) fn acquire_writer(&self, cx: &mut Cx, s: Handle) -> rt::Result<Handle> {
        if self.writable_locked(s) {
            return Err(rt::Error::Type("The stream is locked to a writer".into()));
        }
        let state = self.writable_state(s);
        let (ready, closed) = match state {
            WState::Writable => {
                let bp = self.ws(s, |s| s.backpressure);
                let ready = if !self.writable_close_queued_or_in_flight(s) && bp {
                    Deferred::new(cx)
                } else {
                    Deferred::resolved_undefined(cx)
                };
                (ready, Deferred::new(cx))
            }
            WState::Erroring => {
                let e = self.writable_stored_error_value(cx, s);
                let ready = Deferred::rejected(cx, &e);
                ready.mark_handled(cx);
                (ready, Deferred::new(cx))
            }
            WState::Closed => (
                Deferred::resolved_undefined(cx),
                Deferred::resolved_undefined(cx),
            ),
            WState::Errored => {
                let e = self.writable_stored_error_value(cx, s);
                let ready = Deferred::rejected(cx, &e);
                ready.mark_handled(cx);
                let closed = Deferred::rejected(cx, &e);
                closed.mark_handled(cx);
                (ready, closed)
            }
        };
        let w = self.alloc(Object::WritableStreamDefaultWriter(Box::new(WriterData {
            stream: Some(s),
            closed,
            ready,
        })));
        self.ws(s, |s| s.writer = Some(w));
        Ok(w)
    }

    pub(crate) fn writer_ready_deferred(&self, w: Handle) -> Deferred {
        self.ww(w, |w| w.ready.clone())
    }

    pub(crate) fn writer_closed_deferred(&self, w: Handle) -> Deferred {
        self.ww(w, |w| w.closed.clone())
    }

    fn writer_ensure_ready_rejected(&self, cx: &mut Cx, w: Handle, error: JsValue) {
        let ready = self.ww(w, |w| w.ready.clone());
        if ready.pending() {
            ready.reject(cx, &error);
        } else {
            let d = Deferred::rejected(cx, &error);
            self.ww(w, |w| w.ready = d);
        }
        let ready = self.ww(w, |w| w.ready.clone());
        ready.mark_handled(cx);
    }

    fn writer_ensure_closed_rejected(&self, cx: &mut Cx, w: Handle, error: JsValue) {
        let closed = self.ww(w, |w| w.closed.clone());
        if closed.pending() {
            closed.reject(cx, &error);
        } else {
            let d = Deferred::rejected(cx, &error);
            self.ww(w, |w| w.closed = d);
        }
        let closed = self.ww(w, |w| w.closed.clone());
        closed.mark_handled(cx);
    }

    /// WritableStreamDefaultWriterCloseWithErrorPropagation.
    pub(crate) fn writer_close_with_error_propagation(&self, cx: &mut Cx, w: Handle) -> Deferred {
        let Some(s) = self.ww(w, |w| w.stream) else {
            let e = type_error(cx, "The writer is released");
            return Deferred::rejected(cx, &e);
        };
        let state = self.writable_state(s);
        if self.writable_close_queued_or_in_flight(s) || state == WState::Closed {
            return Deferred::resolved_undefined(cx);
        }
        if state == WState::Errored {
            let e = self.writable_stored_error_value(cx, s);
            return Deferred::rejected(cx, &e);
        }
        self.writable_stream_close(cx, s)
    }

    /// WritableStreamDefaultWriterRelease.
    pub(crate) fn writer_release(&self, cx: &mut Cx, w: Handle) {
        let Some(s) = self.ww(w, |w| w.stream) else {
            return;
        };
        let released = type_error(cx, "The writer was released");
        self.writer_ensure_ready_rejected(cx, w, released.clone());
        self.writer_ensure_closed_rejected(cx, w, released);
        self.ws(s, |s| s.writer = None);
        self.ww(w, |w| w.stream = None);
    }

    /// WritableStreamDefaultWriterWrite.
    pub(crate) fn writer_write(&self, cx: &mut Cx, w: Handle, chunk: JsValue) -> Deferred {
        let Some(s) = self.ww(w, |w| w.stream) else {
            let e = type_error(cx, "The writer is released");
            return Deferred::rejected(cx, &e);
        };
        let Some(c) = self.writable_controller_of(s) else {
            return Deferred::resolved_undefined(cx);
        };
        let size = self.writable_controller_chunk_size(cx, c, &chunk);
        if self.ww(w, |w| w.stream) != Some(s) {
            let e = type_error(cx, "The writer is released");
            return Deferred::rejected(cx, &e);
        }
        let state = self.writable_state(s);
        if state == WState::Errored {
            let e = self.writable_stored_error_value(cx, s);
            return Deferred::rejected(cx, &e);
        }
        if self.writable_close_queued_or_in_flight(s) || state == WState::Closed {
            let e = type_error(cx, "The stream is closing or closed");
            return Deferred::rejected(cx, &e);
        }
        if state == WState::Erroring {
            let e = self.writable_stored_error_value(cx, s);
            return Deferred::rejected(cx, &e);
        }
        let promise = Deferred::new(cx);
        self.ws(s, |s| s.write_requests.push_back(promise.clone()));
        self.writable_controller_write(cx, c, chunk, size);
        promise
    }

    /// WritableStreamDefaultWriterGetDesiredSize.
    fn writer_desired_size(&self, w: Handle) -> Option<f64> {
        let s = self.ww(w, |w| w.stream)?;
        match self.writable_state(s) {
            WState::Errored | WState::Erroring => None,
            WState::Closed => Some(0.0),
            WState::Writable => self
                .writable_controller_of(s)
                .map(|c| self.writable_controller_desired_size(c)),
        }
    }

    // ----- §5.7 the controller --------------------------------------------------------------------------

    fn writable_controller_desired_size(&self, c: Handle) -> f64 {
        self.wc(c, |c| c.high_water_mark - c.queue_total)
    }

    fn writable_controller_backpressure(&self, c: Handle) -> bool {
        self.writable_controller_desired_size(c) <= 0.0
    }

    fn writable_controller_clear_algorithms(&self, c: Handle) {
        self.wc(c, |c| {
            c.sink = None;
            c.size = None;
        });
    }

    /// WritableStreamDefaultControllerError.
    fn writable_controller_error(&self, cx: &mut Cx, c: Handle, error: JsValue) {
        let stream = self.wc(c, |c| c.stream);
        self.writable_controller_clear_algorithms(c);
        self.writable_start_erroring(cx, stream, error);
    }

    /// WritableStreamDefaultControllerErrorIfNeeded.
    pub(crate) fn writable_controller_error_if_needed(
        &self,
        cx: &mut Cx,
        c: Handle,
        error: JsValue,
    ) {
        let stream = self.wc(c, |c| c.stream);
        if self.writable_state(stream) == WState::Writable {
            self.writable_controller_error(cx, c, error);
        }
    }

    /// WritableStreamDefaultControllerGetChunkSize.
    fn writable_controller_chunk_size(&self, cx: &mut Cx, c: Handle, chunk: &JsValue) -> f64 {
        let Some(f) = self.wc(c, |c| c.size.clone()) else {
            return 1.0;
        };
        match call_size(cx, &f, chunk) {
            Ok(n) => n,
            Err(e) => {
                self.writable_controller_error_if_needed(cx, c, e);
                1.0
            }
        }
    }

    /// WritableStreamDefaultControllerWrite.
    fn writable_controller_write(&self, cx: &mut Cx, c: Handle, chunk: JsValue, size: f64) {
        if !size.is_finite() || size < 0.0 {
            let e = error_value(cx, rt::Error::Range("The chunk size is invalid".into()));
            self.writable_controller_error_if_needed(cx, c, e);
            return;
        }
        let stream = self.wc(c, |c| {
            c.queue.push_back((QueueItem::Chunk(chunk), size));
            c.queue_total += size;
            c.stream
        });
        if !self.writable_close_queued_or_in_flight(stream)
            && self.writable_state(stream) == WState::Writable
        {
            let bp = self.writable_controller_backpressure(c);
            self.writable_update_backpressure(cx, stream, bp);
        }
        self.writable_controller_advance_queue_if_needed(cx, c);
    }

    /// WritableStreamDefaultControllerAdvanceQueueIfNeeded.
    fn writable_controller_advance_queue_if_needed(&self, cx: &mut Cx, c: Handle) {
        let (stream, started) = self.wc(c, |c| (c.stream, c.started));
        if !started || self.ws(stream, |s| s.in_flight_write.is_some()) {
            return;
        }
        if self.writable_state(stream) == WState::Erroring {
            self.writable_finish_erroring(cx, stream);
            return;
        }
        enum Next {
            None,
            Close,
            Write(JsValue),
        }
        let next = self.wc(c, |c| match c.queue.front() {
            None => Next::None,
            Some((QueueItem::Close, _)) => Next::Close,
            Some((QueueItem::Chunk(v), _)) => Next::Write(v.clone()),
        });
        match next {
            Next::None => {}
            Next::Close => self.writable_controller_process_close(cx, c),
            Next::Write(v) => self.writable_controller_process_write(cx, c, v),
        }
    }

    fn dequeue(&self, c: Handle) {
        self.wc(c, |c| {
            if let Some((_, size)) = c.queue.pop_front() {
                c.queue_total = (c.queue_total - size).max(0.0);
            }
        });
    }

    /// WritableStreamDefaultControllerProcessClose.
    fn writable_controller_process_close(&self, cx: &mut Cx, c: Handle) {
        let stream = self.wc(c, |c| c.stream);
        self.ws(stream, |s| s.in_flight_close = s.close_request.take());
        self.dequeue(c);
        let sink = self.wc(c, |c| sink_parts(&c.sink));
        let promise = match sink {
            SinkParts::Js { this, close, .. } => self.promise_call(cx, close.as_ref(), &this, &[]),
            SinkParts::Native(n) => n.close(self, cx),
            SinkParts::None => Deferred::resolved_undefined(cx).value(cx),
        };
        self.writable_controller_clear_algorithms(c);
        self.upon(
            cx,
            &promise,
            Box::new(move |e, cx, r| match r {
                Ok(_) => e.writable_finish_in_flight_close(cx, stream),
                Err(r) => e.writable_finish_in_flight_close_with_error(cx, stream, r),
            }),
        );
    }

    /// WritableStreamDefaultControllerProcessWrite.
    fn writable_controller_process_write(&self, cx: &mut Cx, c: Handle, chunk: JsValue) {
        let stream = self.wc(c, |c| c.stream);
        self.ws(stream, |s| s.in_flight_write = s.write_requests.pop_front());
        let sink = self.wc(c, |c| sink_parts(&c.sink));
        let promise = match sink {
            SinkParts::Js { this, write, .. } => {
                let cv = handle_value(cx, c);
                self.promise_call(cx, write.as_ref(), &this, &[chunk, cv])
            }
            SinkParts::Native(n) => n.write(self, cx, chunk),
            SinkParts::None => Deferred::resolved_undefined(cx).value(cx),
        };
        self.upon(
            cx,
            &promise,
            Box::new(move |e, cx, r| match r {
                Ok(_) => {
                    e.writable_finish_in_flight_write(cx, stream);
                    let state = e.writable_state(stream);
                    e.dequeue(c);
                    if !e.writable_close_queued_or_in_flight(stream) && state == WState::Writable {
                        let bp = e.writable_controller_backpressure(c);
                        e.writable_update_backpressure(cx, stream, bp);
                    }
                    e.writable_controller_advance_queue_if_needed(cx, c);
                }
                Err(r) => {
                    if e.writable_state(stream) == WState::Writable {
                        e.writable_controller_clear_algorithms(c);
                    }
                    e.writable_finish_in_flight_write_with_error(cx, stream, r);
                }
            }),
        );
    }

    /// The controller's [[AbortSteps]].
    fn writable_controller_abort_steps(&self, cx: &mut Cx, c: Handle, reason: JsValue) -> JsValue {
        let sink = self.wc(c, |c| sink_parts(&c.sink));
        let result = match sink {
            SinkParts::Js { this, abort, .. } => {
                self.promise_call(cx, abort.as_ref(), &this, &[reason])
            }
            SinkParts::Native(n) => n.abort(self, cx, reason),
            SinkParts::None => Deferred::resolved_undefined(cx).value(cx),
        };
        self.writable_controller_clear_algorithms(c);
        result
    }
}

enum SinkParts {
    Js {
        this: JsValue,
        write: Option<JsFn>,
        close: Option<JsFn>,
        abort: Option<JsFn>,
    },
    Native(Rc<dyn NativeSink>),
    None,
}

fn sink_parts(sink: &Option<Sink>) -> SinkParts {
    match sink {
        Some(Sink::Js {
            this,
            write,
            close,
            abort,
        }) => SinkParts::Js {
            this: this.clone(),
            write: write.clone(),
            close: close.clone(),
            abort: abort.clone(),
        },
        Some(Sink::Native(n)) => SinkParts::Native(n.clone()),
        None => SinkParts::None,
    }
}

impl WritableStreamImpl for Engine {
    fn locked(&self, this: Handle) -> bool {
        self.writable_locked(this)
    }

    fn abort(&self, cx: &mut Cx, this: Handle, reason: Option<JsValue>) -> rt::Result<JsPromise> {
        if self.writable_locked(this) {
            let e = type_error(cx, "The stream is locked to a writer");
            return Ok(Deferred::rejected(cx, &e).p);
        }
        let reason = reason.unwrap_or_else(|| undefined(cx));
        Ok(self.writable_stream_abort(cx, this, reason).p)
    }

    fn close(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        if self.writable_locked(this) {
            let e = type_error(cx, "The stream is locked to a writer");
            return Ok(Deferred::rejected(cx, &e).p);
        }
        if self.writable_close_queued_or_in_flight(this) {
            let e = type_error(cx, "The stream is already closing");
            return Ok(Deferred::rejected(cx, &e).p);
        }
        Ok(self.writable_stream_close(cx, this).p)
    }

    fn get_writer(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.acquire_writer(cx, this)
    }

    fn constructor(
        &self,
        cx: &mut Cx,
        underlying_sink: Option<JsObject>,
        strategy: QueuingStrategy,
    ) -> rt::Result<Handle> {
        let this = match &underlying_sink {
            Some(o) => {
                let scope = cx.scope();
                let l = v8::Local::new(scope, o.global());
                JsValue::new(scope, l.into())
            }
            None => undefined(cx),
        };
        // The UnderlyingSink dictionary, its members in lexicographic order.
        let abort = get_callback(cx, &this, "abort").map_err(|e| throw(cx, e))?;
        let close = get_callback(cx, &this, "close").map_err(|e| throw(cx, e))?;
        let start = get_callback(cx, &this, "start").map_err(|e| throw(cx, e))?;
        let type_ = get(cx, &this, "type").map_err(|e| throw(cx, e))?;
        let write = get_callback(cx, &this, "write").map_err(|e| throw(cx, e))?;
        if !type_.is_undefined(cx) {
            return Err(rt::Error::Range("Writable streams have no type".into()));
        }
        let size = extract_size(cx, &strategy);
        let hwm = extract_high_water_mark(&strategy, 1.0)?;
        let s = self.new_writable_stream();
        let start_this = this.clone();
        self.set_up_writable_controller(
            cx,
            s,
            Box::new(move |_, cx, c| match start {
                Some(f) => {
                    let cv = handle_value(cx, c);
                    call(cx, &f, &start_this, &[cv])
                }
                None => Ok(undefined(cx)),
            }),
            Sink::Js {
                this,
                write,
                close,
                abort,
            },
            hwm,
            size,
        )
        .map_err(|e| throw(cx, e))?;
        Ok(s)
    }
}

impl WritableStreamDefaultWriterImpl for Engine {
    fn closed(&self, _cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        Ok(self.ww(this, |w| w.closed.p.clone()))
    }

    fn desired_size(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<f64>> {
        if self.ww(this, |w| w.stream.is_none()) {
            return Err(rt::Error::Type("The writer's lock was released".into()));
        }
        Ok(self.writer_desired_size(this))
    }

    fn ready(&self, _cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        Ok(self.ww(this, |w| w.ready.p.clone()))
    }

    fn abort(&self, cx: &mut Cx, this: Handle, reason: Option<JsValue>) -> rt::Result<JsPromise> {
        let Some(s) = self.ww(this, |w| w.stream) else {
            let e = type_error(cx, "The writer is released");
            return Ok(Deferred::rejected(cx, &e).p);
        };
        let reason = reason.unwrap_or_else(|| undefined(cx));
        Ok(self.writable_stream_abort(cx, s, reason).p)
    }

    fn close(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let Some(s) = self.ww(this, |w| w.stream) else {
            let e = type_error(cx, "The writer is released");
            return Ok(Deferred::rejected(cx, &e).p);
        };
        if self.writable_close_queued_or_in_flight(s) {
            let e = type_error(cx, "The stream is already closing");
            return Ok(Deferred::rejected(cx, &e).p);
        }
        Ok(self.writable_stream_close(cx, s).p)
    }

    fn release_lock(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.writer_release(cx, this);
        Ok(())
    }

    fn write(&self, cx: &mut Cx, this: Handle, chunk: Option<JsValue>) -> rt::Result<JsPromise> {
        if self.ww(this, |w| w.stream.is_none()) {
            let e = type_error(cx, "The writer is released");
            return Ok(Deferred::rejected(cx, &e).p);
        }
        let chunk = chunk.unwrap_or_else(|| undefined(cx));
        Ok(self.writer_write(cx, this, chunk).p)
    }

    fn constructor(&self, cx: &mut Cx, stream: Handle) -> rt::Result<Handle> {
        self.acquire_writer(cx, stream)
    }
}

impl WritableStreamDefaultControllerImpl for Engine {
    fn signal(&self, this: Handle) -> Handle {
        self.wc(this, |c| c.signal)
    }

    fn error(&self, cx: &mut Cx, this: Handle, e: Option<JsValue>) -> rt::Result<()> {
        let stream = self.wc(this, |c| c.stream);
        if self.writable_state(stream) != WState::Writable {
            return Ok(());
        }
        let e = e.unwrap_or_else(|| undefined(cx));
        self.writable_controller_error(cx, this, e);
        Ok(())
    }
}
