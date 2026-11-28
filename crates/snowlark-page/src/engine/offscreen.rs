// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.12.5.3 `OffscreenCanvas`: a canvas bitmap with no element, in a window or a dedicated
//! worker, with a 2D or WebGL context; `transferControlToOffscreen()` makes one whose frames show
//! in a `<canvas>` element (its placeholder), and transferring it to a worker moves it there.
//!
//! The link from an OffscreenCanvas to its placeholder is a [`Placeholder`]: the latest frame the
//! OffscreenCanvas committed and the waker of the placeholder's event loop. An OffscreenCanvas
//! commits a frame at its event loop's rendering update after it drew (a worker's runs its
//! animation frame callbacks first), replacing a frame the placeholder has not taken yet; the
//! placeholder's rendering update shows the newest one. A transferred OffscreenCanvas crosses to
//! the other event loop beside the message's serialized bytes, as a [`Transferred`] the bytes name
//! by index.

use std::sync::{Arc, Mutex};

use snowlark_bindings::generated::{self as g, OffscreenCanvasImpl};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsObject, JsPromise, JsValue};
use snowlark_dom::NodeId;
use snowlark_render::canvas::Canvas2D;

use super::canvas::{CanvasOwner, ContextObj};
use super::{exception, Engine, Object};
use crate::event_loop::Waker;

/// The ArrayBuffers among a message's transfer list (HTML §2.7.5 StructuredSerializeWithTransfer
/// steps 1-4 for them): each once, not shared, not detached and detachable, else a
/// DataCloneError.
fn transfer_buffers(
    cx: &mut Cx,
    transfer: &[JsObject],
) -> rt::Result<Vec<v8::Global<v8::ArrayBuffer>>> {
    let scope = cx.scope();
    let mut buffers: Vec<v8::Global<v8::ArrayBuffer>> = Vec::new();
    for t in transfer {
        let o = v8::Local::new(scope, t.global());
        if o.is_shared_array_buffer() {
            return Err(exception(
                ExceptionName::DataCloneError,
                "A SharedArrayBuffer cannot be transferred",
            ));
        }
        let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(o) else {
            continue;
        };
        let dup = buffers.iter().any(|b| v8::Local::new(scope, b) == ab);
        if dup || ab.was_detached() || !ab.is_detachable() {
            return Err(exception(
                ExceptionName::DataCloneError,
                "An ArrayBuffer in the transfer list cannot be transferred",
            ));
        }
        buffers.push(v8::Global::new(scope, ab));
    }
    Ok(buffers)
}

/// The largest bitmap side an OffscreenCanvas gets (a larger one has no bitmap: its context draws
/// nothing), as for a canvas element's attributes.
const MAX_SIDE: u64 = 1 << 15;

/// The latest frame an OffscreenCanvas committed for its placeholder canvas, shared between the
/// two event loops (which may be on two threads).
pub struct Placeholder {
    /// The frame and whether its bitmap was origin-clean, until the placeholder takes it.
    frame: Mutex<Option<(snowlark_paint::image::Frame, bool)>>,
    /// The placeholder's event loop.
    waker: Arc<Waker>,
}

/// What an OffscreenCanvas draws with (HTML: its context mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    None,
    TwoD(Handle),
    WebGl(Handle),
    /// Transferred: this object has no bitmap any more.
    Detached,
}

/// An `OffscreenCanvas`.
pub struct OffscreenObj {
    pub width: u64,
    pub height: u64,
    pub mode: Mode,
    pub placeholder: Option<Arc<Placeholder>>,
}

/// A placeholder canvas element's side of the link.
pub struct PlaceholderState {
    slot: Arc<Placeholder>,
    /// Whether the last frame shown was origin-clean (`toDataURL` of the placeholder throws
    /// otherwise).
    origin_clean: bool,
}

/// A transferable object on its way to another event loop (HTML §2.7.4 "transfer-receiving
/// steps" data).
pub enum Transferred {
    OffscreenCanvas {
        width: u64,
        height: u64,
        placeholder: Option<Arc<Placeholder>>,
    },
    /// A `WebAssembly.Module` (serialized, not transferred): its compiled module, which the
    /// receiving side makes a Module of without compiling again (WebAssembly JS API, Module's
    /// serialization steps: its bytes, within its agent cluster).
    WasmModule(v8::CompiledWasmModule),
    /// A `MessagePort` ([`crate::engine::channel::PortTransfer`]).
    Port(Box<crate::engine::channel::PortTransfer>),
}

impl Engine {
    fn offscreen<R>(&self, h: Handle, f: impl FnOnce(&mut OffscreenObj) -> R) -> Option<R> {
        match &mut *self.obj_mut(h) {
            Object::OffscreenCanvas(o) => Some(f(o)),
            _ => None,
        }
    }

    pub(crate) fn offscreen_detached(&self, h: Handle) -> bool {
        self.offscreen(h, |o| o.mode == Mode::Detached)
            .unwrap_or(true)
    }

    pub(crate) fn offscreen_2d(&self, h: Handle) -> Option<Handle> {
        match self.offscreen(h, |o| o.mode)? {
            Mode::TwoD(c) => Some(c),
            _ => None,
        }
    }

    pub(crate) fn offscreen_webgl(&self, h: Handle) -> Option<Handle> {
        match self.offscreen(h, |o| o.mode)? {
            Mode::WebGl(c) => Some(c),
            _ => None,
        }
    }

    /// The bitmap's size (a detached OffscreenCanvas has none: 0 x 0).
    pub(crate) fn offscreen_size(&self, h: Handle) -> (u32, u32) {
        self.offscreen(h, |o| {
            if o.mode == Mode::Detached {
                (0, 0)
            } else {
                (o.width.min(MAX_SIDE) as u32, o.height.min(MAX_SIDE) as u32)
            }
        })
        .unwrap_or((0, 0))
    }

    fn new_offscreen(
        &self,
        width: u64,
        height: u64,
        placeholder: Option<Arc<Placeholder>>,
    ) -> Handle {
        self.alloc(Object::OffscreenCanvas(Box::new(OffscreenObj {
            width,
            height,
            mode: Mode::None,
            placeholder,
        })))
    }

    /// An OffscreenCanvas's context drew: one with a placeholder commits a frame at the next
    /// rendering update.
    pub(crate) fn offscreen_drew(&self, oc: Handle) {
        if self.offscreen(oc, |o| o.placeholder.is_some()) != Some(true) {
            return;
        }
        let mut dirty = self.offscreen_dirty.borrow_mut();
        if !dirty.contains(&oc) {
            dirty.push(oc);
        }
        drop(dirty);
        self.render.borrow_mut().dirty = true;
    }

    /// Whether an OffscreenCanvas waits to commit a frame, or a placeholder canvas to show one:
    /// the event loop wants a rendering update.
    pub(crate) fn offscreen_frame_wanted(&self) -> bool {
        !self.offscreen_dirty.borrow().is_empty()
            || self
                .placeholders
                .borrow()
                .values()
                .any(|p| p.slot.frame.lock().is_ok_and(|f| f.is_some()))
    }

    /// The rendering update's part for OffscreenCanvases (after WebGL's, which commits theirs):
    /// the 2D ones that drew commit a frame to their placeholder, and each placeholder canvas
    /// shows the newest frame committed for it.
    pub(crate) fn flush_offscreen(&self) {
        let dirty = std::mem::take(&mut *self.offscreen_dirty.borrow_mut());
        for oc in dirty {
            let Some(ctx) = self.offscreen_2d(oc) else {
                continue;
            };
            let (frame, clean) = self.ctx_frame(ctx);
            if let Some(frame) = frame {
                self.commit_offscreen_frame(oc, frame, clean);
            }
        }
        let frames: Vec<(NodeId, snowlark_paint::image::Frame, bool)> = self
            .placeholders
            .borrow()
            .iter()
            .filter_map(|(el, p)| {
                let (frame, clean) = p.slot.frame.lock().ok()?.take()?;
                Some((*el, frame, clean))
            })
            .collect();
        for (el, frame, clean) in frames {
            if let Some(p) = self.placeholders.borrow_mut().get_mut(&el) {
                p.origin_clean = clean;
            }
            self.show_canvas_frame(el, Some(frame));
        }
    }

    /// Commits `frame` to the OffscreenCanvas's placeholder, if it has one.
    pub(crate) fn commit_offscreen_frame(
        &self,
        oc: Handle,
        frame: snowlark_paint::image::Frame,
        origin_clean: bool,
    ) {
        let Some(Some(slot)) = self.offscreen(oc, |o| o.placeholder.clone()) else {
            return;
        };
        if let Ok(mut f) = slot.frame.lock() {
            *f = Some((frame, origin_clean));
        }
        slot.waker.wake();
    }

    /// Whether a canvas element is a placeholder (its control went to an OffscreenCanvas).
    pub(crate) fn is_placeholder(&self, el: NodeId) -> bool {
        self.placeholders.borrow().contains_key(&el)
    }

    /// The `InvalidStateError` a placeholder canvas's `width` and `height` setters throw.
    pub(crate) fn check_not_placeholder(&self, el: NodeId) -> rt::Result<()> {
        if self.is_placeholder(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The canvas's control was transferred to an OffscreenCanvas",
            ));
        }
        Ok(())
    }

    /// The frame a placeholder canvas shows now (its OffscreenCanvas's last commit).
    pub(crate) fn placeholder_frame(&self, el: NodeId) -> Option<snowlark_paint::image::Frame> {
        if !self.is_placeholder(el) {
            return None;
        }
        let image = self
            .with_node_renderer(el, |r, _| r.canvas_pixels(el))
            .flatten()?;
        image.frames.first().cloned()
    }

    /// Whether the frame a placeholder canvas shows was origin-clean.
    pub(crate) fn placeholder_origin_clean(&self, el: NodeId) -> bool {
        self.placeholders
            .borrow()
            .get(&el)
            .is_none_or(|p| p.origin_clean)
    }

    /// HTML §4.12.5 `transferControlToOffscreen()`.
    pub(crate) fn transfer_control(&self, el: NodeId) -> rt::Result<Handle> {
        // "If this canvas element's context mode is not set to none, throw an InvalidStateError."
        if self.canvases.borrow().contains_key(&el) || self.has_webgl(el) || self.is_placeholder(el)
        {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The canvas already has a rendering context",
            ));
        }
        let slot = Arc::new(Placeholder {
            frame: Mutex::new(None),
            waker: self.waker(),
        });
        self.placeholders.borrow_mut().insert(
            el,
            PlaceholderState {
                slot: slot.clone(),
                origin_clean: true,
            },
        );
        let (w, h) = self.canvas_size(el);
        Ok(self.new_offscreen(w as u64, h as u64, Some(slot)))
    }

    /// A 2D context's bitmap as a frame, and whether it is origin-clean.
    fn ctx_frame(&self, ctx: Handle) -> (Option<snowlark_paint::image::Frame>, bool) {
        match &mut *self.obj_mut(ctx) {
            Object::Canvas2D(c) => (c.canvas.frame(), c.origin_clean),
            _ => (None, true),
        }
    }

    /// The OffscreenCanvases among a message's transfer list (HTML §2.7.5
    /// StructuredSerializeWithTransfer steps 1-4 for them): each once, not detached, with no
    /// context. Other transferables are cloned with the message, as before.
    fn offscreen_transfer_list(
        &self,
        cx: &mut Cx,
        transfer: &[JsObject],
    ) -> rt::Result<Vec<Handle>> {
        use snowlark_bindings::generated::ids;
        let mut list = Vec::new();
        for o in transfer {
            let h = {
                let scope = cx.scope();
                let obj = v8::Local::new(scope, o.global());
                rt::unwrap(&obj, ids::OFFSCREEN_CANVAS, ids::OFFSCREEN_CANVAS_END)
            };
            let Some(h) = h else { continue };
            if list.contains(&h) {
                return Err(exception(
                    ExceptionName::DataCloneError,
                    "An object is in the transfer list twice",
                ));
            }
            match self.offscreen(h, |o| o.mode) {
                Some(Mode::None) => {}
                Some(Mode::Detached) | None => {
                    return Err(exception(
                        ExceptionName::DataCloneError,
                        "The OffscreenCanvas is detached",
                    ))
                }
                // HTML §4.12.5.3 transfer steps: "If value's context mode is not equal to none,
                // then throw an InvalidStateError."
                Some(_) => {
                    return Err(exception(
                        ExceptionName::InvalidStateError,
                        "The OffscreenCanvas has a rendering context",
                    ))
                }
            }
            list.push(h);
        }
        Ok(list)
    }

    /// StructuredSerializeWithTransfer for another event loop: the bytes, and the transferred
    /// objects they name by index (the transfer steps ran: each is detached here).
    pub(crate) fn serialize_with_transfer(
        &self,
        cx: &mut Cx,
        value: &JsValue,
        transfer: &[JsObject],
    ) -> rt::Result<(Vec<u8>, Vec<Transferred>)> {
        crate::engine::channel::check_transferable(cx, transfer)?;
        let buffers = transfer_buffers(cx, transfer)?;
        let list = self.offscreen_transfer_list(cx, transfer)?;
        let bitmaps = self.bitmap_transfer_list(cx, transfer)?;
        let ports = self.port_transfer_list(cx, transfer)?;
        let prev = self.transfer_out.replace(list.clone());
        // (each port is written by its own handle: the key its transfer data carries)
        let prev_ports = self
            .transferring
            .replace(ports.iter().map(|&p| (p, p)).collect());
        // (a dedicated worker is in its owner's agent cluster: WebAssembly modules go along)
        let (bytes, modules) =
            self.sharing_wasm_modules(list.len() as u32, || self.serialize_value(cx, value));
        self.transferring.replace(prev_ports);
        self.transfer_out.replace(prev);
        let bytes = bytes?;
        // (the ImageBitmaps are checked and detached first: one that cannot be transferred
        // throws before anything is detached)
        self.transfer_bitmaps(&bitmaps)?;
        // The ArrayBuffers' contents went with the message (serialized by value); the transfer
        // steps detach them here.
        {
            let scope = cx.scope();
            for b in &buffers {
                v8::Local::new(scope, b).detach(None);
            }
        }
        let mut out = Vec::with_capacity(list.len());
        for h in list {
            // HTML §4.12.5.3 transfer steps: the data holder gets the bitmap's size and the
            // placeholder; the object is detached.
            // (a detached OffscreenCanvas has no bitmap: its width and height are 0)
            let data = self.offscreen(h, |o| {
                o.mode = Mode::Detached;
                Transferred::OffscreenCanvas {
                    width: std::mem::take(&mut o.width),
                    height: std::mem::take(&mut o.height),
                    placeholder: o.placeholder.take(),
                }
            });
            out.extend(data);
        }
        out.extend(modules.into_iter().map(Transferred::WasmModule));
        // (the ports last: the indices before them are the serialization's)
        out.extend(
            ports
                .into_iter()
                .map(|p| Transferred::Port(Box::new(self.export_port(p)))),
        );
        Ok((bytes, out))
    }

    /// Runs `f`, a serialization for a realm of this agent cluster, with WebAssembly modules
    /// shared: each Module it meets is listed beside the bytes, from index `base` on
    /// ([`Engine::share_wasm_module`]); the list comes back with `f`'s result. A serialization
    /// outside this (for storage, or to another agent cluster) throws a DataCloneError at a Module.
    pub(crate) fn sharing_wasm_modules<R>(
        &self,
        base: u32,
        f: impl FnOnce() -> R,
    ) -> (R, Vec<v8::CompiledWasmModule>) {
        let prev = self.wasm_out.replace(Some((base, Vec::new())));
        let r = f();
        let shared = self.wasm_out.replace(prev);
        (r, shared.map(|(_, list)| list).unwrap_or_default())
    }

    /// WebAssembly JS API, Module's serialization steps: the index beside the bytes of the
    /// compiled module `m`, or `None` when this serialization cannot share one
    /// ([`Engine::sharing_wasm_modules`]).
    pub(crate) fn share_wasm_module(&self, m: v8::CompiledWasmModule) -> Option<u32> {
        let mut out = self.wasm_out.borrow_mut();
        let (base, list) = out.as_mut()?;
        list.push(m);
        Some(*base + list.len() as u32 - 1)
    }

    /// The compiled module of the WebAssembly Module serialized at `index` beside the bytes being
    /// deserialized.
    pub(crate) fn receive_wasm_module(&self, index: u32) -> Option<v8::CompiledWasmModule> {
        let mut t = self.transfer_in.borrow_mut();
        let slot = t.get_mut(index as usize)?;
        if !matches!(slot, Some(Transferred::WasmModule(_))) {
            return None;
        }
        match slot.take() {
            Some(Transferred::WasmModule(m)) => Some(m),
            _ => None,
        }
    }

    /// The index in the transfer list being serialized of the object `h`, if it is in it.
    pub(crate) fn transfer_index(&self, h: Handle) -> Option<u32> {
        self.transfer_out
            .borrow()
            .iter()
            .position(|&t| t == h)
            .map(|i| i as u32)
    }

    /// StructuredDeserializeWithTransfer: `bytes` with the objects transferred beside them.
    pub(crate) fn deserialize_with_transfer(
        &self,
        cx: &mut Cx,
        bytes: &[u8],
        transferred: Vec<Transferred>,
    ) -> Result<JsValue, ()> {
        let prev = self
            .transfer_in
            .replace(transferred.into_iter().map(Some).collect());
        let r = self.deserialize_value(cx, bytes);
        self.transfer_in.replace(prev);
        r
    }

    /// The transfer-receiving steps of the transferred object at `index`: a new object here.
    pub(crate) fn receive_transferred(&self, index: u32) -> Option<Handle> {
        let t = {
            let mut t = self.transfer_in.borrow_mut();
            let slot = t.get_mut(index as usize)?;
            if !matches!(slot, Some(Transferred::OffscreenCanvas { .. })) {
                return None;
            }
            slot.take()?
        };
        match t {
            Transferred::OffscreenCanvas {
                width,
                height,
                placeholder,
            } => Some(self.new_offscreen(width, height, placeholder)),
            Transferred::WasmModule(_) | Transferred::Port(_) => None,
        }
    }

    /// The `options` of `getContext("2d", options)`'s context (a new one).
    fn new_offscreen_2d(&self, oc: Handle) -> Handle {
        let (w, h) = self.offscreen_size(oc);
        self.alloc(Object::Canvas2D(Box::new(ContextObj {
            owner: CanvasOwner::Offscreen(oc),
            canvas: Canvas2D::new(w, h),
            fill_object: None,
            stroke_object: None,
            origin_clean: true,
        })))
    }

    /// The bitmap's new size reaches the context (HTML §4.12.5.3: setting `width` or `height`
    /// resets a 2D context's bitmap and state, a WebGL context's drawing buffer).
    fn offscreen_resized(&self, oc: Handle) {
        match self.offscreen(oc, |o| o.mode) {
            Some(Mode::TwoD(ctx)) => {
                let (w, h) = self.offscreen_size(oc);
                if let Object::Canvas2D(c) = &mut *self.obj_mut(ctx) {
                    c.canvas.resize(w, h);
                }
                self.offscreen_drew(oc);
            }
            Some(Mode::WebGl(_)) => self.webgl_resized(CanvasOwner::Offscreen(oc)),
            _ => {}
        }
    }

    fn set_offscreen_dim(&self, oc: Handle, value: u64, width: bool) -> rt::Result<()> {
        if self.offscreen_detached(oc) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The OffscreenCanvas is detached",
            ));
        }
        self.offscreen(oc, |o| {
            if width {
                o.width = value;
            } else {
                o.height = value;
            }
        });
        self.offscreen_resized(oc);
        Ok(())
    }

    fn offscreen_rejected(&self, cx: &mut Cx, e: rt::Error) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        p.reject(cx, e);
        Ok(p)
    }
}

impl OffscreenCanvasImpl for Engine {
    fn width(&self, this: Handle) -> u64 {
        self.offscreen(this, |o| o.width).unwrap_or(0)
    }
    fn set_width(&self, _cx: &mut Cx, this: Handle, value: u64) -> rt::Result<()> {
        self.set_offscreen_dim(this, value, true)
    }
    fn height(&self, this: Handle) -> u64 {
        self.offscreen(this, |o| o.height).unwrap_or(0)
    }
    fn set_height(&self, _cx: &mut Cx, this: Handle, value: u64) -> rt::Result<()> {
        self.set_offscreen_dim(this, value, false)
    }
    /// HTML §4.12.5.3 `getContext(contextId, options)`: "2d", "webgl" and "webgl2"; a canvas keeps
    /// the kind it first gave (another kind is null). "bitmaprenderer" and "webgpu" are not
    /// supported here (null).
    fn get_context(
        &self,
        cx: &mut Cx,
        this: Handle,
        context_id: g::OffscreenRenderingContextId,
        options: JsValue,
    ) -> rt::Result<
        Option<g::OffscreenCanvasRenderingContext2DOrWebGLRenderingContextOrWebGL2RenderingContext>,
    > {
        use g::OffscreenCanvasRenderingContext2DOrWebGLRenderingContextOrWebGL2RenderingContext as R;
        use g::OffscreenRenderingContextId as Id;
        let mode = self.offscreen(this, |o| o.mode).unwrap_or(Mode::Detached);
        let wrap = |c: Handle, webgl2: bool| {
            if webgl2 {
                R::WebGL2RenderingContext(c)
            } else {
                R::WebGLRenderingContext(c)
            }
        };
        match (context_id, mode) {
            (_, Mode::Detached) => Err(exception(
                ExceptionName::InvalidStateError,
                "The OffscreenCanvas is detached",
            )),
            (Id::_2d, Mode::TwoD(c)) => Ok(Some(R::OffscreenCanvasRenderingContext2D(c))),
            (id @ (Id::Webgl | Id::Webgl2), Mode::WebGl(c)) => {
                let webgl2 = id == Id::Webgl2;
                Ok((self.webgl_version_2(c) == webgl2).then(|| wrap(c, webgl2)))
            }
            (Id::_2d, Mode::None) => {
                let c = self.new_offscreen_2d(this);
                self.offscreen(this, |o| o.mode = Mode::TwoD(c));
                Ok(Some(R::OffscreenCanvasRenderingContext2D(c)))
            }
            (id @ (Id::Webgl | Id::Webgl2), Mode::None) => {
                let webgl2 = id == Id::Webgl2;
                let Some(c) =
                    self.webgl_get_context(cx, CanvasOwner::Offscreen(this), &options, webgl2)?
                else {
                    return Ok(None);
                };
                self.offscreen(this, |o| o.mode = Mode::WebGl(c));
                // (a placeholder shows the cleared drawing buffer from the next update)
                self.offscreen_drew(this);
                Ok(Some(wrap(c, webgl2)))
            }
            _ => Ok(None),
        }
    }
    /// HTML §4.12.5.3 `convertToBlob(options)`: a PNG of the bitmap (the one format encoded here).
    fn convert_to_blob(
        &self,
        cx: &mut Cx,
        this: Handle,
        _options: g::ImageEncodeOptions,
    ) -> rt::Result<JsPromise> {
        if self.offscreen_detached(this) {
            let e = exception(
                ExceptionName::InvalidStateError,
                "The OffscreenCanvas is detached",
            );
            return self.offscreen_rejected(cx, e);
        }
        let src = CanvasOwner::Offscreen(this);
        if !self.canvas_origin_clean(src) {
            return self.offscreen_rejected(cx, Self::tainted_error());
        }
        let (w, h) = self.source_size(src);
        if w == 0 || h == 0 {
            let e = exception(ExceptionName::IndexSizeError, "The canvas has no pixels");
            return self.offscreen_rejected(cx, e);
        }
        let frame = self
            .source_frame(src)
            .unwrap_or_else(|| snowlark_paint::image::Frame {
                width: w,
                height: h,
                pixels: vec![0; w as usize * h as usize * 4],
                delay_ms: 0,
            });
        let png =
            snowlark_render::canvas::pixmap_from_frame(&frame).and_then(|p| p.encode_png().ok());
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        match png {
            Some(bytes) => {
                let blob = self.new_blob(bytes, "image/png");
                p.resolve(cx, blob);
            }
            None => p.reject(
                cx,
                exception(
                    ExceptionName::EncodingError,
                    "The bitmap could not be encoded",
                ),
            ),
        }
        Ok(p)
    }
    fn transfer_to_image_bitmap(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Engine::transfer_to_image_bitmap(self, this)
    }
    /// HTML §4.12.5.3 `new OffscreenCanvas(width, height)`.
    fn constructor(&self, _cx: &mut Cx, width: u64, height: u64) -> rt::Result<Handle> {
        Ok(self.new_offscreen(width, height, None))
    }
}
