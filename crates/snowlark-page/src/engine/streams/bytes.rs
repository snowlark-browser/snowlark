// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Streams §4.5, §4.7, §4.8 and the byte parts of §4.9: the `ReadableByteStreamController` (its
//! queue of transferred buffers and its pending pull-into descriptors), `ReadableStreamBYOBReader`,
//! `ReadableStreamBYOBRequest` and ReadableByteStreamTee. Written from the spec's abstract
//! operations, named after them.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use snowlark_bindings::rt::{self, Cx, Handle, JsObject, JsValue};

use super::super::{Engine, Object};
use super::readable::{NativeSource, RState, ReadRequest, ReadSteps};
use super::{array, error_value, iter_result, type_error, undefined, Deferred};

// ----- ArrayBuffers and views -------------------------------------------------------------------------

/// A view constructor of a pull-into descriptor: a typed array constructor or `%DataView%`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ViewKind {
    Int8,
    Uint8,
    Uint8Clamped,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float16,
    Float32,
    Float64,
    BigInt64,
    BigUint64,
    DataView,
}

impl ViewKind {
    fn of(v: &v8::Value) -> Option<ViewKind> {
        Some(if v.is_int8_array() {
            ViewKind::Int8
        } else if v.is_uint8_array() {
            ViewKind::Uint8
        } else if v.is_uint8_clamped_array() {
            ViewKind::Uint8Clamped
        } else if v.is_int16_array() {
            ViewKind::Int16
        } else if v.is_uint16_array() {
            ViewKind::Uint16
        } else if v.is_int32_array() {
            ViewKind::Int32
        } else if v.is_uint32_array() {
            ViewKind::Uint32
        } else if v.is_float16_array() {
            ViewKind::Float16
        } else if v.is_float32_array() {
            ViewKind::Float32
        } else if v.is_float64_array() {
            ViewKind::Float64
        } else if v.is_big_int64_array() {
            ViewKind::BigInt64
        } else if v.is_big_uint64_array() {
            ViewKind::BigUint64
        } else if v.is_data_view() {
            ViewKind::DataView
        } else {
            return None;
        })
    }

    /// The element size of the typed array constructors table (1 for `DataView`).
    pub(crate) fn element_size(self) -> usize {
        match self {
            ViewKind::Int8 | ViewKind::Uint8 | ViewKind::Uint8Clamped | ViewKind::DataView => 1,
            ViewKind::Int16 | ViewKind::Uint16 | ViewKind::Float16 => 2,
            ViewKind::Int32 | ViewKind::Uint32 | ViewKind::Float32 => 4,
            ViewKind::Float64 | ViewKind::BigInt64 | ViewKind::BigUint64 => 8,
        }
    }

    /// Construct(ctor, « buffer, byteOffset, length »), `length` in elements.
    fn construct<'s>(
        self,
        scope: &v8::PinScope<'s, '_>,
        buf: v8::Local<'s, v8::ArrayBuffer>,
        offset: usize,
        length: usize,
    ) -> Option<v8::Local<'s, v8::Value>> {
        Some(match self {
            ViewKind::Int8 => v8::Int8Array::new(scope, buf, offset, length)?.into(),
            ViewKind::Uint8 => v8::Uint8Array::new(scope, buf, offset, length)?.into(),
            ViewKind::Uint8Clamped => {
                v8::Uint8ClampedArray::new(scope, buf, offset, length)?.into()
            }
            ViewKind::Int16 => v8::Int16Array::new(scope, buf, offset, length)?.into(),
            ViewKind::Uint16 => v8::Uint16Array::new(scope, buf, offset, length)?.into(),
            ViewKind::Int32 => v8::Int32Array::new(scope, buf, offset, length)?.into(),
            ViewKind::Uint32 => v8::Uint32Array::new(scope, buf, offset, length)?.into(),
            ViewKind::Float16 => {
                // (the v8 crate has no Local<Float16Array> to Local<Value> conversion: construct
                // through the realm's Float16Array)
                let global = scope.get_current_context().global(scope);
                let name = v8::String::new(scope, "Float16Array")?;
                let ctor = global.get(scope, name.into())?;
                let ctor = v8::Local::<v8::Function>::try_from(ctor).ok()?;
                let args = [
                    buf.into(),
                    v8::Number::new(scope, offset as f64).into(),
                    v8::Number::new(scope, length as f64).into(),
                ];
                ctor.new_instance(scope, &args)?.into()
            }
            ViewKind::Float32 => v8::Float32Array::new(scope, buf, offset, length)?.into(),
            ViewKind::Float64 => v8::Float64Array::new(scope, buf, offset, length)?.into(),
            ViewKind::BigInt64 => v8::BigInt64Array::new(scope, buf, offset, length)?.into(),
            ViewKind::BigUint64 => v8::BigUint64Array::new(scope, buf, offset, length)?.into(),
            ViewKind::DataView => v8::DataView::new(scope, buf, offset, length).into(),
        })
    }
}

/// An ArrayBufferView's internal slots.
pub(crate) struct ViewInfo {
    pub buffer: v8::Global<v8::ArrayBuffer>,
    pub offset: usize,
    pub length: usize,
    pub kind: ViewKind,
    /// IsDetachedBuffer of the viewed buffer.
    pub detached: bool,
    /// The viewed buffer is a SharedArrayBuffer.
    pub shared: bool,
    /// The viewed buffer's [[ArrayBufferByteLength]].
    pub buffer_length: usize,
}

pub(crate) fn view_info(cx: &mut Cx, v: &JsValue) -> Option<ViewInfo> {
    let scope = cx.scope();
    let local = v8::Local::new(scope, v.global());
    let kind = ViewKind::of(&local)?;
    let view = v8::Local::<v8::ArrayBufferView>::try_from(local).ok()?;
    let buf = view.buffer(scope)?;
    let shared = buf.is_shared_array_buffer();
    Some(ViewInfo {
        offset: view.byte_offset(),
        length: view.byte_length(),
        kind,
        detached: !shared && buf.was_detached(),
        shared,
        buffer_length: buf.byte_length(),
        buffer: v8::Global::new(scope, buf),
    })
}

fn is_detached(cx: &mut Cx, buf: &v8::Global<v8::ArrayBuffer>) -> bool {
    let scope = cx.scope();
    v8::Local::new(scope, buf).was_detached()
}

fn buffer_length(cx: &mut Cx, buf: &v8::Global<v8::ArrayBuffer>) -> usize {
    let scope = cx.scope();
    v8::Local::new(scope, buf).byte_length()
}

/// §8.3 TransferArrayBuffer: detaches `buf` and returns a new ArrayBuffer over its data; a
/// TypeError for a detached, shared or non-detachable buffer (a `WebAssembly.Memory`'s).
pub(crate) fn transfer_array_buffer(
    cx: &mut Cx,
    buf: &v8::Global<v8::ArrayBuffer>,
) -> Result<v8::Global<v8::ArrayBuffer>, JsValue> {
    let transferred = {
        let scope = cx.scope();
        let ab = v8::Local::new(scope, buf);
        if ab.is_shared_array_buffer() || ab.was_detached() || !ab.is_detachable() {
            None
        } else {
            let store = ab.get_backing_store();
            match ab.detach(None) {
                Some(true) => {
                    let n = v8::ArrayBuffer::with_backing_store(scope, &store);
                    Some(v8::Global::new(scope, n))
                }
                _ => None,
            }
        }
    };
    transferred.ok_or_else(|| type_error(cx, "The buffer can't be transferred"))
}

/// CopyDataBlockBytes between two buffers (bounds checked, as CanCopyDataBlockBytes asks).
fn copy_bytes(
    cx: &mut Cx,
    to: &v8::Global<v8::ArrayBuffer>,
    to_index: usize,
    from: &v8::Global<v8::ArrayBuffer>,
    from_index: usize,
    count: usize,
) {
    let scope = cx.scope();
    let to = v8::Local::new(scope, to).get_backing_store();
    let from = v8::Local::new(scope, from).get_backing_store();
    if to_index + count > to.len() || from_index + count > from.len() {
        return;
    }
    for i in 0..count {
        to[to_index + i].set(from[from_index + i].get());
    }
}

/// CloneArrayBuffer(buffer, offset, length, %ArrayBuffer%).
fn clone_array_buffer(
    cx: &mut Cx,
    from: &v8::Global<v8::ArrayBuffer>,
    offset: usize,
    length: usize,
) -> v8::Global<v8::ArrayBuffer> {
    let g = {
        let scope = cx.scope();
        let n = v8::ArrayBuffer::new(scope, length);
        v8::Global::new(scope, n)
    };
    copy_bytes(cx, &g, 0, from, offset, length);
    g
}

/// A view of `kind` over `buf` (length in elements).
pub(crate) fn make_view(
    cx: &mut Cx,
    kind: ViewKind,
    buf: &v8::Global<v8::ArrayBuffer>,
    offset: usize,
    length: usize,
) -> JsValue {
    let scope = cx.scope();
    let b = v8::Local::new(scope, buf);
    match kind.construct(scope, b, offset, length) {
        Some(v) => JsValue::new(scope, v),
        None => rt::undefined_value(scope),
    }
}

/// CloneAsUint8Array.
fn clone_as_uint8_array(cx: &mut Cx, chunk: &JsValue) -> Result<JsValue, JsValue> {
    let Some(info) = view_info(cx, chunk) else {
        return Err(type_error(cx, "The chunk is not an ArrayBufferView"));
    };
    if info.detached {
        return Err(type_error(cx, "The chunk's buffer is detached"));
    }
    let buf = clone_array_buffer(cx, &info.buffer, info.offset, info.length);
    Ok(make_view(cx, ViewKind::Uint8, &buf, 0, info.length))
}

// ----- controller state -------------------------------------------------------------------------------

/// A pull-into descriptor's reader type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReaderType {
    Default,
    Byob,
    None,
}

/// A readable byte stream queue entry.
pub(crate) struct ByteEntry {
    buffer: v8::Global<v8::ArrayBuffer>,
    offset: usize,
    length: usize,
}

/// A pull-into descriptor.
#[derive(Clone)]
pub(crate) struct PullInto {
    buffer: v8::Global<v8::ArrayBuffer>,
    buffer_byte_length: usize,
    byte_offset: usize,
    byte_length: usize,
    bytes_filled: usize,
    minimum_fill: usize,
    element_size: usize,
    ctor: ViewKind,
    reader_type: ReaderType,
}

/// What a `ReadableByteStreamController` has beyond the default controller's slots.
#[derive(Default)]
pub(crate) struct ByteState {
    pub auto_allocate_chunk_size: Option<u64>,
    pub byob_request: Option<Handle>,
    pub queue: VecDeque<ByteEntry>,
    pub pending: VecDeque<PullInto>,
}

pub struct ByobRequestData {
    pub(crate) controller: Option<Handle>,
    pub(crate) view: Option<JsObject>,
    /// The region [[view]] covers (buffer, byte offset, byte length), until the view object is
    /// made: on first access, since the `byobRequest` getter has no script scope (nothing can
    /// change the region before, as the request is invalidated first).
    region: Option<(v8::Global<v8::ArrayBuffer>, usize, usize)>,
}

/// What a read-into request does with the outcome (§4.5.2 "read-into request").
pub(crate) trait ReadIntoSteps {
    fn chunk(self: Box<Self>, e: &Engine, cx: &mut Cx, chunk: JsValue);
    fn close(self: Box<Self>, e: &Engine, cx: &mut Cx, chunk: Option<JsValue>);
    fn error(self: Box<Self>, e: &Engine, cx: &mut Cx, error: JsValue);
}

pub(crate) enum ReadIntoRequest {
    /// `byobReader.read()`'s promise.
    Promise(Deferred),
    Steps(Box<dyn ReadIntoSteps>),
}

impl ReadIntoRequest {
    pub(crate) fn chunk(self, e: &Engine, cx: &mut Cx, chunk: JsValue) {
        match self {
            ReadIntoRequest::Promise(p) => {
                let r = iter_result(cx, &chunk, false);
                p.resolve(cx, &r);
            }
            ReadIntoRequest::Steps(s) => s.chunk(e, cx, chunk),
        }
    }

    pub(crate) fn close(self, e: &Engine, cx: &mut Cx, chunk: Option<JsValue>) {
        match self {
            ReadIntoRequest::Promise(p) => {
                let v = chunk.unwrap_or_else(|| undefined(cx));
                let r = iter_result(cx, &v, true);
                p.resolve(cx, &r);
            }
            ReadIntoRequest::Steps(s) => s.close(e, cx, chunk),
        }
    }

    pub(crate) fn error(self, e: &Engine, cx: &mut Cx, error: JsValue) {
        match self {
            ReadIntoRequest::Promise(p) => p.reject(cx, &error),
            ReadIntoRequest::Steps(s) => s.error(e, cx, error),
        }
    }
}

fn range_error(cx: &mut Cx, message: &'static str) -> JsValue {
    error_value(cx, rt::Error::Range(message.into()))
}

impl Engine {
    fn rb<R>(&self, c: Handle, f: impl FnOnce(&mut ByteState) -> R) -> R {
        self.rc(c, |c| {
            f(c.bytes.as_mut().expect("a byte stream controller"))
        })
    }

    pub(crate) fn is_byte_controller(&self, c: Handle) -> bool {
        self.rc(c, |c| c.bytes.is_some())
    }

    fn byob_request_data<R>(&self, h: Handle, f: impl FnOnce(&mut ByobRequestData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::ReadableStreamBYOBRequest(r) => f(r),
            _ => panic!("not a ReadableStreamBYOBRequest"),
        }
    }

    fn byte_queue_total(&self, c: Handle) -> usize {
        self.rc(c, |c| c.queue_total as usize)
    }

    fn byte_stream_of(&self, c: Handle) -> Handle {
        self.rc(c, |c| c.stream)
    }

    // ----- §4.9.5 byte stream controllers -------------------------------------------------------------

    /// ReadableByteStreamControllerClose: `Err` with the error to throw (a partial element in the
    /// first pull-into descriptor; the stream is errored with it).
    pub(crate) fn byte_controller_close(&self, cx: &mut Cx, c: Handle) -> Result<(), JsValue> {
        let stream = self.byte_stream_of(c);
        let close_requested = self.rc(c, |c| c.close_requested);
        if close_requested || self.readable_state(stream) != RState::Readable {
            return Ok(());
        }
        if self.byte_queue_total(c) > 0 {
            self.rc(c, |c| c.close_requested = true);
            return Ok(());
        }
        let partial = self.rb(c, |b| {
            b.pending
                .front()
                .is_some_and(|p| p.bytes_filled % p.element_size != 0)
        });
        if partial {
            let e = type_error(cx, "The stream closed in the middle of an element");
            self.readable_controller_error(cx, c, e.clone());
            return Err(e);
        }
        self.readable_controller_clear_algorithms(c);
        self.readable_stream_close(cx, stream);
        Ok(())
    }

    /// ReadableByteStreamControllerCommitPullIntoDescriptor.
    fn byte_commit_pull_into(&self, cx: &mut Cx, stream: Handle, pid: PullInto) {
        let done = self.readable_state(stream) == RState::Closed;
        let reader_type = pid.reader_type;
        let filled = self.byte_convert_pull_into(cx, &pid);
        if reader_type == ReaderType::Default {
            self.readable_fulfill_read_request(cx, stream, filled, done);
        } else {
            self.readable_fulfill_read_into_request(cx, stream, filled, done);
        }
    }

    /// ReadableByteStreamControllerConvertPullIntoDescriptor.
    fn byte_convert_pull_into(&self, cx: &mut Cx, pid: &PullInto) -> JsValue {
        match transfer_array_buffer(cx, &pid.buffer) {
            Ok(buffer) => make_view(
                cx,
                pid.ctor,
                &buffer,
                pid.byte_offset,
                pid.bytes_filled / pid.element_size,
            ),
            Err(e) => e,
        }
    }

    /// ReadableByteStreamControllerEnqueue: `Err` with the exception to throw.
    pub(crate) fn byte_controller_enqueue(
        &self,
        cx: &mut Cx,
        c: Handle,
        chunk: &JsValue,
    ) -> Result<(), JsValue> {
        let stream = self.byte_stream_of(c);
        let close_requested = self.rc(c, |c| c.close_requested);
        if close_requested || self.readable_state(stream) != RState::Readable {
            return Ok(());
        }
        let Some(info) = view_info(cx, chunk) else {
            return Err(type_error(cx, "The chunk is not an ArrayBufferView"));
        };
        if info.detached {
            return Err(type_error(cx, "The chunk's buffer is detached"));
        }
        let transferred = transfer_array_buffer(cx, &info.buffer)?;
        let first = self.rb(c, |b| b.pending.front().map(|p| p.buffer.clone()));
        if let Some(first_buffer) = first {
            if is_detached(cx, &first_buffer) {
                return Err(type_error(cx, "The pending read's buffer is detached"));
            }
            self.byte_invalidate_byob_request(c);
            let moved = transfer_array_buffer(cx, &first_buffer)?;
            let none = self.rb(c, |b| {
                let p = b.pending.front_mut().expect("a pending pull-into");
                p.buffer = moved;
                p.reader_type == ReaderType::None
            });
            if none {
                self.byte_enqueue_detached_pull_into_to_queue(cx, c)?;
            }
        }
        if self.readable_has_default_reader(stream) {
            self.byte_process_read_requests_using_queue(cx, c);
            if self.readable_num_read_requests(stream) == 0 {
                self.byte_enqueue_chunk_to_queue(c, transferred, info.offset, info.length);
            } else {
                let shift = self.rb(c, |b| !b.pending.is_empty());
                if shift {
                    self.byte_shift_pending_pull_into(c);
                }
                let view = make_view(cx, ViewKind::Uint8, &transferred, info.offset, info.length);
                self.readable_fulfill_read_request(cx, stream, view, false);
            }
        } else if self.readable_has_byob_reader(stream) {
            self.byte_enqueue_chunk_to_queue(c, transferred, info.offset, info.length);
            let filled = self.byte_process_pull_intos_using_queue(cx, c);
            for pid in filled {
                self.byte_commit_pull_into(cx, stream, pid);
            }
        } else {
            self.byte_enqueue_chunk_to_queue(c, transferred, info.offset, info.length);
        }
        self.readable_controller_call_pull_if_needed(cx, c);
        Ok(())
    }

    /// ReadableByteStreamControllerEnqueueChunkToQueue.
    fn byte_enqueue_chunk_to_queue(
        &self,
        c: Handle,
        buffer: v8::Global<v8::ArrayBuffer>,
        offset: usize,
        length: usize,
    ) {
        self.rc(c, |c| {
            c.queue_total += length as f64;
            c.bytes
                .as_mut()
                .expect("a byte stream controller")
                .queue
                .push_back(ByteEntry {
                    buffer,
                    offset,
                    length,
                });
        });
    }

    /// ReadableByteStreamControllerEnqueueClonedChunkToQueue.
    fn byte_enqueue_cloned_chunk_to_queue(
        &self,
        cx: &mut Cx,
        c: Handle,
        buffer: &v8::Global<v8::ArrayBuffer>,
        offset: usize,
        length: usize,
    ) {
        let clone = clone_array_buffer(cx, buffer, offset, length);
        self.byte_enqueue_chunk_to_queue(c, clone, 0, length);
    }

    /// ReadableByteStreamControllerEnqueueDetachedPullIntoToQueue.
    fn byte_enqueue_detached_pull_into_to_queue(
        &self,
        cx: &mut Cx,
        c: Handle,
    ) -> Result<(), JsValue> {
        let first = self.rb(c, |b| b.pending.front().cloned());
        if let Some(p) = first {
            if p.bytes_filled > 0 {
                self.byte_enqueue_cloned_chunk_to_queue(
                    cx,
                    c,
                    &p.buffer,
                    p.byte_offset,
                    p.bytes_filled,
                );
            }
            self.byte_shift_pending_pull_into(c);
        }
        Ok(())
    }

    /// ReadableByteStreamControllerFillPullIntoDescriptorFromQueue.
    fn byte_fill_pull_into_from_queue(&self, cx: &mut Cx, c: Handle, pid: &mut PullInto) -> bool {
        let total = self.byte_queue_total(c);
        let max_bytes_to_copy = total.min(pid.byte_length - pid.bytes_filled);
        let max_bytes_filled = pid.bytes_filled + max_bytes_to_copy;
        let mut remaining = max_bytes_to_copy;
        let mut ready = false;
        let remainder = max_bytes_filled % pid.element_size;
        let max_aligned = max_bytes_filled - remainder;
        if max_aligned >= pid.minimum_fill {
            remaining = max_aligned - pid.bytes_filled;
            ready = true;
        }
        while remaining > 0 {
            let Some((buffer, offset, length)) = self.rb(c, |b| {
                b.queue
                    .front()
                    .map(|h| (h.buffer.clone(), h.offset, h.length))
            }) else {
                break;
            };
            let n = remaining.min(length);
            let dest_start = pid.byte_offset + pid.bytes_filled;
            copy_bytes(cx, &pid.buffer, dest_start, &buffer, offset, n);
            self.rc(c, |c| {
                let b = c.bytes.as_mut().expect("a byte stream controller");
                if length == n {
                    b.queue.pop_front();
                } else if let Some(h) = b.queue.front_mut() {
                    h.offset += n;
                    h.length -= n;
                }
                c.queue_total = (c.queue_total - n as f64).max(0.0);
            });
            pid.bytes_filled += n;
            remaining -= n;
        }
        ready
    }

    /// ReadableByteStreamControllerFillReadRequestFromQueue.
    fn byte_fill_read_request_from_queue(&self, cx: &mut Cx, c: Handle, request: ReadRequest) {
        let entry = self.rc(c, |c| {
            let e = c.bytes.as_mut().and_then(|b| b.queue.pop_front());
            if let Some(e) = &e {
                c.queue_total = (c.queue_total - e.length as f64).max(0.0);
            }
            e
        });
        let Some(entry) = entry else {
            return;
        };
        self.byte_handle_queue_drain(cx, c);
        let view = make_view(
            cx,
            ViewKind::Uint8,
            &entry.buffer,
            entry.offset,
            entry.length,
        );
        request.chunk(self, cx, view);
    }

    /// ReadableByteStreamControllerGetBYOBRequest.
    pub(crate) fn byte_get_byob_request(&self, c: Handle) -> Option<Handle> {
        let (existing, first) = self.rb(c, |b| (b.byob_request, b.pending.front().cloned()));
        if existing.is_some() {
            return existing;
        }
        let first = first?;
        let request = self.alloc(Object::ReadableStreamBYOBRequest(Box::new(
            ByobRequestData {
                controller: Some(c),
                view: None,
                region: Some((
                    first.buffer.clone(),
                    first.byte_offset + first.bytes_filled,
                    first.byte_length - first.bytes_filled,
                )),
            },
        )));
        self.rb(c, |b| b.byob_request = Some(request));
        Some(request)
    }

    /// The request's [[view]] (a Uint8Array over the first pull-into descriptor's free region), or
    /// null once the request was invalidated.
    pub(crate) fn byob_request_view(&self, cx: &mut Cx, request: Handle) -> Option<JsObject> {
        let (view, region) = self.byob_request_data(request, |r| (r.view.clone(), r.region.take()));
        if view.is_some() {
            return view;
        }
        let (buffer, offset, length) = region?;
        let v = make_view(cx, ViewKind::Uint8, &buffer, offset, length);
        let o = {
            let scope = cx.scope();
            let l = v8::Local::new(scope, v.global());
            v8::Local::<v8::Object>::try_from(l)
                .ok()
                .map(|o| JsObject::new(scope, o))
        };
        self.byob_request_data(request, |r| r.view = o.clone());
        o
    }

    /// A request's [[controller]].
    pub(crate) fn byob_request_controller(&self, request: Handle) -> Option<Handle> {
        self.byob_request_data(request, |r| r.controller)
    }

    /// ReadableByteStreamControllerHandleQueueDrain.
    fn byte_handle_queue_drain(&self, cx: &mut Cx, c: Handle) {
        let (total, close_requested, stream) =
            self.rc(c, |c| (c.queue_total, c.close_requested, c.stream));
        if total == 0.0 && close_requested {
            self.readable_controller_clear_algorithms(c);
            self.readable_stream_close(cx, stream);
        } else {
            self.readable_controller_call_pull_if_needed(cx, c);
        }
    }

    /// ReadableByteStreamControllerInvalidateBYOBRequest.
    pub(crate) fn byte_invalidate_byob_request(&self, c: Handle) {
        let Some(request) = self.rb(c, |b| b.byob_request.take()) else {
            return;
        };
        self.byob_request_data(request, |r| {
            r.controller = None;
            r.view = None;
            r.region = None;
        });
    }

    /// ReadableByteStreamControllerClearPendingPullIntos.
    pub(crate) fn byte_clear_pending_pull_intos(&self, c: Handle) {
        self.byte_invalidate_byob_request(c);
        self.rb(c, |b| b.pending.clear());
    }

    /// ReadableByteStreamControllerProcessPullIntoDescriptorsUsingQueue.
    fn byte_process_pull_intos_using_queue(&self, cx: &mut Cx, c: Handle) -> Vec<PullInto> {
        let mut filled = Vec::new();
        loop {
            if self.byte_queue_total(c) == 0 {
                break;
            }
            let Some(mut pid) = self.rb(c, |b| b.pending.pop_front()) else {
                break;
            };
            if self.byte_fill_pull_into_from_queue(cx, c, &mut pid) {
                filled.push(pid);
            } else {
                self.rb(c, |b| b.pending.push_front(pid));
            }
        }
        filled
    }

    /// ReadableByteStreamControllerProcessReadRequestsUsingQueue.
    fn byte_process_read_requests_using_queue(&self, cx: &mut Cx, c: Handle) {
        let stream = self.byte_stream_of(c);
        loop {
            if self.byte_queue_total(c) == 0 {
                return;
            }
            let Some(reader) = self.rs(stream, |s| s.reader) else {
                return;
            };
            let Some(request) = self.rr(reader, |r| r.requests.pop_front()) else {
                return;
            };
            self.byte_fill_read_request_from_queue(cx, c, request);
        }
    }

    /// ReadableByteStreamControllerPullInto.
    fn byte_pull_into(
        &self,
        cx: &mut Cx,
        c: Handle,
        view: ViewInfo,
        min: u64,
        request: ReadIntoRequest,
    ) {
        let stream = self.byte_stream_of(c);
        let element_size = view.kind.element_size();
        let minimum_fill = (min as usize).saturating_mul(element_size);
        let buffer = match transfer_array_buffer(cx, &view.buffer) {
            Ok(b) => b,
            Err(e) => {
                request.error(self, cx, e);
                return;
            }
        };
        let buffer_byte_length = buffer_length(cx, &buffer);
        let mut pid = PullInto {
            buffer,
            buffer_byte_length,
            byte_offset: view.offset,
            byte_length: view.length,
            bytes_filled: 0,
            minimum_fill,
            element_size,
            ctor: view.kind,
            reader_type: ReaderType::Byob,
        };
        let has_pending = self.rb(c, |b| !b.pending.is_empty());
        if has_pending {
            self.rb(c, |b| b.pending.push_back(pid));
            self.readable_add_read_into_request(stream, request);
            return;
        }
        if self.readable_state(stream) == RState::Closed {
            let empty = make_view(cx, pid.ctor, &pid.buffer, pid.byte_offset, 0);
            request.close(self, cx, Some(empty));
            return;
        }
        if self.byte_queue_total(c) > 0 {
            if self.byte_fill_pull_into_from_queue(cx, c, &mut pid) {
                let filled = self.byte_convert_pull_into(cx, &pid);
                self.byte_handle_queue_drain(cx, c);
                request.chunk(self, cx, filled);
                return;
            }
            if self.rc(c, |c| c.close_requested) {
                let e = type_error(cx, "The stream is closing");
                self.readable_controller_error(cx, c, e.clone());
                request.error(self, cx, e);
                return;
            }
        }
        self.rb(c, |b| b.pending.push_back(pid));
        self.readable_add_read_into_request(stream, request);
        self.readable_controller_call_pull_if_needed(cx, c);
    }

    /// ReadableByteStreamControllerRespond.
    pub(crate) fn byte_respond(
        &self,
        cx: &mut Cx,
        c: Handle,
        bytes_written: u64,
    ) -> Result<(), JsValue> {
        let Some(first) = self.rb(c, |b| b.pending.front().cloned()) else {
            return Err(type_error(cx, "There is no pending read"));
        };
        let stream = self.byte_stream_of(c);
        if self.readable_state(stream) == RState::Closed {
            if bytes_written != 0 {
                return Err(type_error(
                    cx,
                    "A closed stream can only respond with 0 bytes",
                ));
            }
        } else {
            if bytes_written == 0 {
                return Err(type_error(cx, "bytesWritten must be positive"));
            }
            if first.bytes_filled as u64 + bytes_written > first.byte_length as u64 {
                return Err(range_error(cx, "bytesWritten is larger than the view"));
            }
        }
        let moved = transfer_array_buffer(cx, &first.buffer)?;
        self.rb(c, |b| {
            if let Some(p) = b.pending.front_mut() {
                p.buffer = moved;
            }
        });
        self.byte_respond_internal(cx, c, bytes_written as usize)
    }

    /// ReadableByteStreamControllerRespondInClosedState.
    fn byte_respond_in_closed_state(&self, cx: &mut Cx, c: Handle) {
        let none = self.rb(c, |b| {
            b.pending
                .front()
                .is_some_and(|p| p.reader_type == ReaderType::None)
        });
        if none {
            self.byte_shift_pending_pull_into(c);
        }
        let stream = self.byte_stream_of(c);
        if self.readable_has_byob_reader(stream) {
            let mut filled = Vec::new();
            while filled.len() < self.readable_num_read_into_requests(stream) {
                match self.byte_shift_pending_pull_into(c) {
                    Some(p) => filled.push(p),
                    None => break,
                }
            }
            for pid in filled {
                self.byte_commit_pull_into(cx, stream, pid);
            }
        }
    }

    /// ReadableByteStreamControllerRespondInReadableState.
    fn byte_respond_in_readable_state(
        &self,
        cx: &mut Cx,
        c: Handle,
        bytes_written: usize,
    ) -> Result<(), JsValue> {
        let stream = self.byte_stream_of(c);
        let (reader_type, filled, minimum) = self.rb(c, |b| {
            let p = b.pending.front_mut().expect("a pending pull-into");
            p.bytes_filled += bytes_written;
            (p.reader_type, p.bytes_filled, p.minimum_fill)
        });
        if reader_type == ReaderType::None {
            self.byte_enqueue_detached_pull_into_to_queue(cx, c)?;
            let filled = self.byte_process_pull_intos_using_queue(cx, c);
            for pid in filled {
                self.byte_commit_pull_into(cx, stream, pid);
            }
            return Ok(());
        }
        if filled < minimum {
            return Ok(());
        }
        let Some(mut pid) = self.byte_shift_pending_pull_into(c) else {
            return Ok(());
        };
        let remainder = pid.bytes_filled % pid.element_size;
        if remainder > 0 {
            let end = pid.byte_offset + pid.bytes_filled;
            self.byte_enqueue_cloned_chunk_to_queue(cx, c, &pid.buffer, end - remainder, remainder);
        }
        pid.bytes_filled -= remainder;
        let filled = self.byte_process_pull_intos_using_queue(cx, c);
        self.byte_commit_pull_into(cx, stream, pid);
        for p in filled {
            self.byte_commit_pull_into(cx, stream, p);
        }
        Ok(())
    }

    /// ReadableByteStreamControllerRespondInternal.
    fn byte_respond_internal(
        &self,
        cx: &mut Cx,
        c: Handle,
        bytes_written: usize,
    ) -> Result<(), JsValue> {
        self.byte_invalidate_byob_request(c);
        let stream = self.byte_stream_of(c);
        if self.readable_state(stream) == RState::Closed {
            self.byte_respond_in_closed_state(cx, c);
        } else {
            self.byte_respond_in_readable_state(cx, c, bytes_written)?;
        }
        self.readable_controller_call_pull_if_needed(cx, c);
        Ok(())
    }

    /// ReadableByteStreamControllerRespondWithNewView.
    pub(crate) fn byte_respond_with_new_view(
        &self,
        cx: &mut Cx,
        c: Handle,
        view: &JsValue,
    ) -> Result<(), JsValue> {
        let Some(info) = view_info(cx, view) else {
            return Err(type_error(cx, "The view is not an ArrayBufferView"));
        };
        let Some(first) = self.rb(c, |b| b.pending.front().cloned()) else {
            return Err(type_error(cx, "There is no pending read"));
        };
        let stream = self.byte_stream_of(c);
        if self.readable_state(stream) == RState::Closed {
            if info.length != 0 {
                return Err(type_error(
                    cx,
                    "A closed stream can only respond with an empty view",
                ));
            }
        } else if info.length == 0 {
            return Err(type_error(cx, "The view is empty"));
        }
        if first.byte_offset + first.bytes_filled != info.offset {
            return Err(range_error(
                cx,
                "The view's offset doesn't match the request",
            ));
        }
        if first.buffer_byte_length != info.buffer_length {
            return Err(range_error(
                cx,
                "The view's buffer length doesn't match the request",
            ));
        }
        if first.bytes_filled + info.length > first.byte_length {
            return Err(range_error(cx, "The view is larger than the request"));
        }
        let moved = transfer_array_buffer(cx, &info.buffer)?;
        self.rb(c, |b| {
            if let Some(p) = b.pending.front_mut() {
                p.buffer = moved;
            }
        });
        self.byte_respond_internal(cx, c, info.length)
    }

    /// ReadableByteStreamControllerShiftPendingPullInto.
    fn byte_shift_pending_pull_into(&self, c: Handle) -> Option<PullInto> {
        self.rb(c, |b| b.pending.pop_front())
    }

    /// The byte controller's [[PullSteps]].
    pub(crate) fn byte_pull_steps(&self, cx: &mut Cx, c: Handle, request: ReadRequest) {
        let stream = self.byte_stream_of(c);
        if self.byte_queue_total(c) > 0 {
            self.byte_fill_read_request_from_queue(cx, c, request);
            return;
        }
        if let Some(size) = self.rb(c, |b| b.auto_allocate_chunk_size) {
            // (`autoAllocateChunkSize` is the page's: `ArrayBuffer::new` aborts the process when the
            // allocation fails, where the ArrayBuffer constructor throws a RangeError)
            let mut bytes = Vec::new();
            if bytes.try_reserve_exact(size as usize).is_err() {
                let error = range_error(cx, "Array buffer allocation failed");
                request.error(self, cx, error);
                return;
            }
            bytes.resize(size as usize, 0);
            let buffer = {
                let scope = cx.scope();
                let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
                let b = v8::ArrayBuffer::with_backing_store(scope, &store);
                v8::Global::new(scope, b)
            };
            self.rb(c, |b| {
                b.pending.push_back(PullInto {
                    buffer,
                    buffer_byte_length: size as usize,
                    byte_offset: 0,
                    byte_length: size as usize,
                    bytes_filled: 0,
                    minimum_fill: 1,
                    element_size: 1,
                    ctor: ViewKind::Uint8,
                    reader_type: ReaderType::Default,
                })
            });
        }
        self.readable_add_read_request(stream, request);
        self.readable_controller_call_pull_if_needed(cx, c);
    }

    /// The byte controller's [[ReleaseSteps]].
    pub(crate) fn byte_release_steps(&self, c: Handle) {
        self.rb(c, |b| {
            if let Some(mut first) = b.pending.pop_front() {
                first.reader_type = ReaderType::None;
                b.pending.clear();
                b.pending.push_back(first);
            }
        });
    }

    /// The byte controller's [[CancelSteps]] parts before the cancel algorithm, and
    /// ReadableByteStreamControllerError's: the pending pull-intos and the queue are dropped.
    pub(crate) fn byte_reset(&self, c: Handle) {
        self.byte_clear_pending_pull_intos(c);
        self.rc(c, |c| {
            if let Some(b) = c.bytes.as_mut() {
                b.queue.clear();
            }
            c.queue_total = 0.0;
        });
    }

    // ----- BYOB readers -----------------------------------------------------------------------------------

    /// AcquireReadableStreamBYOBReader (SetUpReadableStreamBYOBReader).
    pub(crate) fn acquire_byob_reader(&self, cx: &mut Cx, s: Handle) -> rt::Result<Handle> {
        if self.readable_locked(s) {
            return Err(rt::Error::Type("The stream is locked to a reader".into()));
        }
        let byte = self
            .rs(s, |s| s.controller)
            .is_some_and(|c| self.is_byte_controller(c));
        if !byte {
            return Err(rt::Error::Type(
                "BYOB readers need a readable byte stream".into(),
            ));
        }
        Ok(self.new_reader(cx, s, true))
    }

    /// ReadableStreamBYOBReaderRead.
    pub(crate) fn byob_reader_read(
        &self,
        cx: &mut Cx,
        reader: Handle,
        view: ViewInfo,
        min: u64,
        request: ReadIntoRequest,
    ) {
        let Some(s) = self.rr(reader, |r| r.stream) else {
            let e = type_error(cx, "The reader is released");
            request.error(self, cx, e);
            return;
        };
        self.rs(s, |s| s.disturbed = true);
        if self.readable_state(s) == RState::Errored {
            let e = self.readable_stored_error(cx, s);
            request.error(self, cx, e);
            return;
        }
        if let Some(c) = self.rs(s, |s| s.controller) {
            self.byte_pull_into(cx, c, view, min, request);
        }
    }

    /// The BYOB reader's `read(view, options)` method steps: the promise, or the rejection.
    pub(crate) fn byob_read_method(
        &self,
        cx: &mut Cx,
        reader: Handle,
        view: &JsValue,
        min: u64,
    ) -> Deferred {
        let reject = |cx: &mut Cx, e: JsValue| Deferred::rejected(cx, &e);
        let Some(info) = view_info(cx, view) else {
            let e = type_error(cx, "The view is not an ArrayBufferView");
            return reject(cx, e);
        };
        if info.shared {
            let e = type_error(cx, "The view's buffer is shared");
            return reject(cx, e);
        }
        if info.length == 0 {
            let e = type_error(cx, "The view is empty");
            return reject(cx, e);
        }
        if info.buffer_length == 0 {
            let e = type_error(cx, "The view's buffer is empty");
            return reject(cx, e);
        }
        if info.detached {
            let e = type_error(cx, "The view's buffer is detached");
            return reject(cx, e);
        }
        if min == 0 {
            let e = type_error(cx, "min must be positive");
            return reject(cx, e);
        }
        let elements = (info.length / info.kind.element_size()) as u64;
        if min > elements {
            let e = range_error(cx, "min is larger than the view");
            return reject(cx, e);
        }
        if self.rr(reader, |r| r.stream.is_none()) {
            let e = type_error(cx, "The reader is released");
            return reject(cx, e);
        }
        let p = Deferred::new(cx);
        self.byob_reader_read(cx, reader, info, min, ReadIntoRequest::Promise(p.clone()));
        p
    }

    // ----- ReadableByteStreamTee ------------------------------------------------------------------------

    /// ReadableByteStreamTee.
    pub(crate) fn readable_byte_tee(&self, cx: &mut Cx, s: Handle) -> rt::Result<(Handle, Handle)> {
        let reader = self.acquire_default_reader(cx, s)?;
        let cancel_promise = Deferred::new(cx);
        let state = Rc::new(RefCell::new(ByteTee {
            stream: s,
            reader,
            reader_byob: false,
            reading: false,
            read_again: [false, false],
            canceled: [false, false],
            reasons: [None, None],
            branches: [None, None],
            cancel_promise,
        }));
        let b1 = self.create_readable_stream(
            cx,
            Rc::new(ByteTeeSource {
                state: state.clone(),
                branch: 0,
            }),
            0.0,
            true,
        );
        let b2 = self.create_readable_stream(
            cx,
            Rc::new(ByteTeeSource {
                state: state.clone(),
                branch: 1,
            }),
            0.0,
            true,
        );
        state.borrow_mut().branches = [Some(b1), Some(b2)];
        forward_reader_error(self, cx, &state, reader);
        Ok((b1, b2))
    }
}

impl Engine {
    fn branch_controller(&self, b: Option<Handle>) -> Option<Handle> {
        b.and_then(|b| self.rs(b, |s| s.controller))
    }
}

struct ByteTee {
    stream: Handle,
    reader: Handle,
    reader_byob: bool,
    reading: bool,
    read_again: [bool; 2],
    canceled: [bool; 2],
    reasons: [Option<JsValue>; 2],
    branches: [Option<Handle>; 2],
    cancel_promise: Deferred,
}

/// "forwardReaderError": the reader's closed promise rejecting errors both branches.
fn forward_reader_error(e: &Engine, cx: &mut Cx, state: &Rc<RefCell<ByteTee>>, reader: Handle) {
    let closed = e.rr(reader, |r| r.closed.clone());
    let st = state.clone();
    e.upon_deferred(
        cx,
        &closed,
        Box::new(move |e, cx, r| {
            let Err(r) = r else {
                return;
            };
            let (current, branches, canceled, cancel) = {
                let s = st.borrow();
                (s.reader, s.branches, s.canceled, s.cancel_promise.clone())
            };
            if current != reader {
                return;
            }
            for b in branches {
                if let Some(c) = e.branch_controller(b) {
                    e.readable_controller_error(cx, c, r.clone());
                }
            }
            if !canceled[0] || !canceled[1] {
                cancel.resolve_undefined(cx);
            }
        }),
    );
}

fn tee_pull(e: &Engine, cx: &mut Cx, state: &Rc<RefCell<ByteTee>>, branch: usize) {
    let branch_stream = {
        let mut s = state.borrow_mut();
        if s.reading {
            s.read_again[branch] = true;
            return;
        }
        s.reading = true;
        s.branches[branch]
    };
    let request = e
        .branch_controller(branch_stream)
        .and_then(|c| e.byte_get_byob_request(c));
    match request {
        None => pull_with_default_reader(e, cx, state),
        Some(r) => {
            let view = e.byob_request_view(cx, r);
            match view {
                Some(v) => {
                    let v = {
                        let scope = cx.scope();
                        let l = v8::Local::new(scope, v.global());
                        JsValue::new(scope, l.into())
                    };
                    pull_with_byob_reader(e, cx, state, v, branch == 1)
                }
                None => pull_with_default_reader(e, cx, state),
            }
        }
    }
}

fn after_read(e: &Engine, cx: &mut Cx, state: &Rc<RefCell<ByteTee>>) {
    let again = {
        let mut s = state.borrow_mut();
        s.reading = false;
        s.read_again
    };
    if again[0] {
        tee_pull(e, cx, state, 0);
    } else if again[1] {
        tee_pull(e, cx, state, 1);
    }
}

fn pull_with_default_reader(e: &Engine, cx: &mut Cx, state: &Rc<RefCell<ByteTee>>) {
    let (byob, reader, stream) = {
        let s = state.borrow();
        (s.reader_byob, s.reader, s.stream)
    };
    let reader = if byob {
        e.reader_release(cx, reader);
        let Ok(r) = e.acquire_default_reader(cx, stream) else {
            return;
        };
        {
            let mut s = state.borrow_mut();
            s.reader = r;
            s.reader_byob = false;
        }
        forward_reader_error(e, cx, state, r);
        r
    } else {
        reader
    };
    e.reader_read(
        cx,
        reader,
        ReadRequest::Steps(Box::new(TeeDefaultRead {
            state: state.clone(),
        })),
    );
}

struct TeeDefaultRead {
    state: Rc<RefCell<ByteTee>>,
}

impl ReadSteps for TeeDefaultRead {
    fn chunk(self: Box<Self>, e: &Engine, cx: &mut Cx, chunk: JsValue) {
        let state = self.state;
        e.queue_stream_microtask(cx, move |e, cx| {
            let (branches, canceled, stream, cancel) = {
                let mut s = state.borrow_mut();
                s.read_again = [false, false];
                (s.branches, s.canceled, s.stream, s.cancel_promise.clone())
            };
            let chunk1 = chunk.clone();
            let mut chunk2 = chunk;
            if !canceled[0] && !canceled[1] {
                match clone_as_uint8_array(cx, &chunk2) {
                    Ok(c) => chunk2 = c,
                    Err(err) => {
                        for b in branches {
                            if let Some(c) = e.branch_controller(b) {
                                e.readable_controller_error(cx, c, err.clone());
                            }
                        }
                        let r = e.readable_stream_cancel(cx, stream, err);
                        cancel.resolve(cx, &r);
                        return;
                    }
                }
            }
            if !canceled[0] {
                if let Some(c) = e.branch_controller(branches[0]) {
                    let _ = e.byte_controller_enqueue(cx, c, &chunk1);
                }
            }
            if !canceled[1] {
                if let Some(c) = e.branch_controller(branches[1]) {
                    let _ = e.byte_controller_enqueue(cx, c, &chunk2);
                }
            }
            after_read(e, cx, &state);
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
                if let Some(c) = e.branch_controller(branches[i]) {
                    let _ = e.byte_controller_close(cx, c);
                }
            }
        }
        for b in branches {
            if let Some(c) = e.branch_controller(b) {
                if e.rb(c, |b| !b.pending.is_empty()) {
                    let _ = e.byte_respond(cx, c, 0);
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

fn pull_with_byob_reader(
    e: &Engine,
    cx: &mut Cx,
    state: &Rc<RefCell<ByteTee>>,
    view: JsValue,
    for_branch2: bool,
) {
    let (byob, reader, stream) = {
        let s = state.borrow();
        (s.reader_byob, s.reader, s.stream)
    };
    let reader = if !byob {
        e.reader_release(cx, reader);
        let Ok(r) = e.acquire_byob_reader(cx, stream) else {
            return;
        };
        {
            let mut s = state.borrow_mut();
            s.reader = r;
            s.reader_byob = true;
        }
        forward_reader_error(e, cx, state, r);
        r
    } else {
        reader
    };
    let Some(info) = view_info(cx, &view) else {
        return;
    };
    e.byob_reader_read(
        cx,
        reader,
        info,
        1,
        ReadIntoRequest::Steps(Box::new(TeeByobRead {
            state: state.clone(),
            for_branch2,
        })),
    );
}

struct TeeByobRead {
    state: Rc<RefCell<ByteTee>>,
    for_branch2: bool,
}

impl TeeByobRead {
    /// (byob branch, other branch, byobCanceled, otherCanceled)
    fn sides(
        state: &Rc<RefCell<ByteTee>>,
        for_branch2: bool,
    ) -> (Option<Handle>, Option<Handle>, bool, bool) {
        let s = state.borrow();
        let (b, o) = if for_branch2 { (1, 0) } else { (0, 1) };
        (s.branches[b], s.branches[o], s.canceled[b], s.canceled[o])
    }
}

impl ReadIntoSteps for TeeByobRead {
    fn chunk(self: Box<Self>, e: &Engine, cx: &mut Cx, chunk: JsValue) {
        let state = self.state;
        let for_branch2 = self.for_branch2;
        e.queue_stream_microtask(cx, move |e, cx| {
            state.borrow_mut().read_again = [false, false];
            let (byob_branch, other_branch, byob_canceled, other_canceled) =
                TeeByobRead::sides(&state, for_branch2);
            let byob_c = e.branch_controller(byob_branch);
            let other_c = e.branch_controller(other_branch);
            if !other_canceled {
                let cloned = match clone_as_uint8_array(cx, &chunk) {
                    Ok(c) => c,
                    Err(err) => {
                        for c in [byob_c, other_c].into_iter().flatten() {
                            e.readable_controller_error(cx, c, err.clone());
                        }
                        let (stream, cancel) = {
                            let s = state.borrow();
                            (s.stream, s.cancel_promise.clone())
                        };
                        let r = e.readable_stream_cancel(cx, stream, err);
                        cancel.resolve(cx, &r);
                        return;
                    }
                };
                if !byob_canceled {
                    if let Some(c) = byob_c {
                        let _ = e.byte_respond_with_new_view(cx, c, &chunk);
                    }
                }
                if let Some(c) = other_c {
                    let _ = e.byte_controller_enqueue(cx, c, &cloned);
                }
            } else if !byob_canceled {
                if let Some(c) = byob_c {
                    let _ = e.byte_respond_with_new_view(cx, c, &chunk);
                }
            }
            after_read(e, cx, &state);
        });
    }

    fn close(self: Box<Self>, e: &Engine, cx: &mut Cx, chunk: Option<JsValue>) {
        self.state.borrow_mut().reading = false;
        let (byob_branch, other_branch, byob_canceled, other_canceled) =
            TeeByobRead::sides(&self.state, self.for_branch2);
        let byob_c = e.branch_controller(byob_branch);
        let other_c = e.branch_controller(other_branch);
        if !byob_canceled {
            if let Some(c) = byob_c {
                let _ = e.byte_controller_close(cx, c);
            }
        }
        if !other_canceled {
            if let Some(c) = other_c {
                let _ = e.byte_controller_close(cx, c);
            }
        }
        if let Some(chunk) = chunk {
            if !byob_canceled {
                if let Some(c) = byob_c {
                    let _ = e.byte_respond_with_new_view(cx, c, &chunk);
                }
            }
            if !other_canceled {
                if let Some(c) = other_c {
                    if e.rb(c, |b| !b.pending.is_empty()) {
                        let _ = e.byte_respond(cx, c, 0);
                    }
                }
            }
        }
        if !byob_canceled || !other_canceled {
            let cancel = self.state.borrow().cancel_promise.clone();
            cancel.resolve_undefined(cx);
        }
    }

    fn error(self: Box<Self>, _e: &Engine, _cx: &mut Cx, _error: JsValue) {
        self.state.borrow_mut().reading = false;
    }
}

struct ByteTeeSource {
    state: Rc<RefCell<ByteTee>>,
    branch: usize,
}

impl NativeSource for ByteTeeSource {
    fn pull(&self, e: &Engine, cx: &mut Cx, _controller: Handle) -> JsValue {
        tee_pull(e, cx, &self.state, self.branch);
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
