// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL 1.0 (Khronos, `docs/webgl.md`): `WebGLRenderingContext` and its objects, over the service
//! in `snowlark-gl`. This is the client half: it checks what WebGL checks before a call reaches
//! OpenGL ES (objects of this context, not deleted; argument lengths; typed array types), keeps the
//! state script can read back as objects (bindings, attachments), names objects with its own ids,
//! and encodes every call as a [`Command`] or [`Query`]. The service checks again: ANGLE in its
//! WebGL compatibility mode validates every call and translates every shader. A context that drew
//! hands its drawing buffer to the renderer at the next rendering update.

use rustc_hash::FxHashMap;
use snowlark_bindings::generated::{
    self as g, WebGLActiveInfoImpl, WebGLContextEventImpl, WebGLObjectImpl,
    WebGLRenderingContextBaseImpl, WebGLRenderingContextOverloadsImpl,
    WebGLShaderPrecisionFormatImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle, JsObject, JsValue, ToJs};
use snowlark_dom::NodeId;

use super::canvas::CanvasOwner;
use snowlark_gl::pixels;
use snowlark_gl::protocol::Limits;
use snowlark_gl::{Attributes, Backend, Command, ContextId, Kind, Query, Reply};

use super::events::EventExt;
use super::{Engine, InterfaceId, Object};

#[path = "webgl2.rs"]
mod webgl2;
use webgl2::Gl2State;

#[path = "webgl_ext.rs"]
mod webgl_ext;
use webgl_ext::ExtState;

/// The OpenGL ES 2.0 and WebGL enums this file uses.
mod c {
    pub const NO_ERROR: u32 = 0;
    pub const INVALID_ENUM: u32 = 0x0500;
    pub const INVALID_VALUE: u32 = 0x0501;
    pub const INVALID_OPERATION: u32 = 0x0502;
    pub const CONTEXT_LOST_WEBGL: u32 = 0x9242;
    pub const ARRAY_BUFFER: u32 = 0x8892;
    pub const ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
    pub const ARRAY_BUFFER_BINDING: u32 = 0x8894;
    pub const ELEMENT_ARRAY_BUFFER_BINDING: u32 = 0x8895;
    pub const CURRENT_PROGRAM: u32 = 0x8B8D;
    pub const FRAMEBUFFER: u32 = 0x8D40;
    pub const RENDERBUFFER: u32 = 0x8D41;
    pub const FRAMEBUFFER_BINDING: u32 = 0x8CA6;
    pub const RENDERBUFFER_BINDING: u32 = 0x8CA7;
    pub const TEXTURE_2D: u32 = 0x0DE1;
    pub const TEXTURE_CUBE_MAP: u32 = 0x8513;
    pub const TEXTURE_BINDING_2D: u32 = 0x8069;
    pub const TEXTURE_BINDING_CUBE_MAP: u32 = 0x8514;
    pub const TEXTURE0: u32 = 0x84C0;
    pub const FRONT_AND_BACK: u32 = 0x0408;
    pub const VERTEX_SHADER: u32 = 0x8B31;
    pub const FRAGMENT_SHADER: u32 = 0x8B30;
    pub const SHADER_TYPE: u32 = 0x8B4F;
    pub const DELETE_STATUS: u32 = 0x8B80;
    pub const COMPILE_STATUS: u32 = 0x8B81;
    pub const LINK_STATUS: u32 = 0x8B82;
    pub const VALIDATE_STATUS: u32 = 0x8B83;
    pub const ATTACHED_SHADERS: u32 = 0x8B85;
    pub const ACTIVE_UNIFORMS: u32 = 0x8B86;
    pub const ACTIVE_ATTRIBUTES: u32 = 0x8B89;
    pub const FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE: u32 = 0x8CD0;
    pub const FRAMEBUFFER_ATTACHMENT_OBJECT_NAME: u32 = 0x8CD1;
    pub const VERTEX_ATTRIB_ARRAY_BUFFER_BINDING: u32 = 0x889F;
    pub const CURRENT_VERTEX_ATTRIB: u32 = 0x8626;
    pub const VERTEX_ATTRIB_ARRAY_ENABLED: u32 = 0x8622;
    pub const VERTEX_ATTRIB_ARRAY_NORMALIZED: u32 = 0x886A;
    pub const UNPACK_FLIP_Y_WEBGL: u32 = 0x9240;
    pub const UNPACK_PREMULTIPLY_ALPHA_WEBGL: u32 = 0x9241;
    pub const UNPACK_COLORSPACE_CONVERSION_WEBGL: u32 = 0x9243;
    pub const BROWSER_DEFAULT_WEBGL: u32 = 0x9244;
    pub const PACK_ALIGNMENT: u32 = 0x0D05;
    pub const UNPACK_ALIGNMENT: u32 = 0x0CF5;
    pub const RGBA: u32 = 0x1908;
    pub const RGB: u32 = 0x1907;
    pub const ALPHA: u32 = 0x1906;
    pub const UNSIGNED_BYTE: u32 = 0x1401;
    pub const UNSIGNED_SHORT_5_6_5: u32 = 0x8363;
    pub const UNSIGNED_SHORT_4_4_4_4: u32 = 0x8033;
    pub const UNSIGNED_SHORT_5_5_5_1: u32 = 0x8034;
    pub const FLOAT: u32 = 0x1406;
    pub const VENDOR: u32 = 0x1F00;
    pub const RENDERER: u32 = 0x1F01;
    pub const VERSION: u32 = 0x1F02;
    pub const SHADING_LANGUAGE_VERSION: u32 = 0x8B8C;
    pub const RGBA8: u32 = 0x8058;
    pub const LOW_FLOAT: u32 = 0x8DF0;
    pub const HIGH_INT: u32 = 0x8DF5;
}

/// A `WebGLRenderingContext`.
pub struct ContextObj {
    /// Its canvas: an element, or an OffscreenCanvas.
    pub owner: CanvasOwner,
    pub id: ContextId,
    /// The attributes as granted.
    pub attributes: Attributes,
    pub limits: Limits,
    pub width: u32,
    pub height: u32,
    pub lost: bool,
    /// Errors this side recorded, oldest first (WebGL 1.0 §5.14.3 `getError`).
    pub errors: Vec<u32>,
    next_object: u32,
    array_buffer: Option<Handle>,
    element_buffer: Option<Handle>,
    framebuffer: Option<Handle>,
    renderbuffer: Option<Handle>,
    program: Option<Handle>,
    active_unit: u32,
    /// Per texture unit: TEXTURE_2D, TEXTURE_CUBE_MAP, and in WebGL 2.0 TEXTURE_3D,
    /// TEXTURE_2D_ARRAY.
    textures: Vec<[Option<Handle>; 4]>,
    /// WebGL 2.0's bindings (empty in a WebGL 1.0 context).
    gl2: Gl2State,
    /// The extensions offered and asked for.
    ext: ExtState,
    attrib_buffers: Vec<Option<Handle>>,
    pack_alignment: i32,
    unpack_alignment: i32,
    flip_y: bool,
    premultiply_alpha: bool,
    colorspace_conversion: u32,
    drawing_buffer_color_space: g::PredefinedColorSpace,
    unpack_color_space: g::PredefinedColorSpace,
}

/// A `WebGLBuffer`, `WebGLFramebuffer`, `WebGLProgram`, `WebGLRenderbuffer`, `WebGLShader` or
/// `WebGLTexture`.
pub struct GlObject {
    pub context: Handle,
    pub kind: Kind,
    pub id: u32,
    pub deleted: bool,
    /// Bound at least once (`isBuffer` and friends answer true only then).
    pub bound: bool,
    pub label: String,
    /// A shader's type and source.
    pub shader_type: u32,
    pub source: String,
    /// A program's attached shaders, and how many times it was linked.
    pub attached: Vec<Handle>,
    pub links: u32,
    /// A framebuffer's attachments.
    pub attachments: Vec<(u32, Handle)>,
    /// A vertex array's element array buffer and attribute buffers while it is not bound.
    pub vao: Option<(Option<Handle>, Vec<Option<Handle>>)>,
    /// A fence's checkpoint: its status is read only after the microtask checkpoint it was made
    /// in (WebGL 2.0 §3.7.14: sync objects change state only between tasks).
    pub epoch: u64,
    /// A query's target, once it was begun.
    pub target: u32,
}

/// Every WebGL object.
pub enum WebGlObject {
    Context(Box<ContextObj>),
    Object(GlObject),
    UniformLocation {
        context: Handle,
        program: Handle,
        links: u32,
        location: i32,
        ty: u32,
    },
    ActiveInfo {
        size: i32,
        ty: u32,
        name: String,
    },
    Precision([i32; 3]),
    /// The object of a WebGL extension (an index in [`webgl_ext::EXTENSIONS`]).
    Extension {
        context: Handle,
        index: usize,
    },
}

impl WebGlObject {
    pub fn interface(&self) -> InterfaceId {
        match self {
            WebGlObject::Context(c) if c.attributes.webgl2 => InterfaceId::WebGL2RenderingContext,
            WebGlObject::Context(_) => InterfaceId::WebGLRenderingContext,
            WebGlObject::Object(o) => match o.kind {
                Kind::Buffer => InterfaceId::WebGLBuffer,
                Kind::Framebuffer => InterfaceId::WebGLFramebuffer,
                Kind::Program => InterfaceId::WebGLProgram,
                Kind::Renderbuffer => InterfaceId::WebGLRenderbuffer,
                Kind::Shader => InterfaceId::WebGLShader,
                Kind::Texture => InterfaceId::WebGLTexture,
                Kind::Query => InterfaceId::WebGLQuery,
                Kind::Sampler => InterfaceId::WebGLSampler,
                Kind::Sync => InterfaceId::WebGLSync,
                Kind::TransformFeedback => InterfaceId::WebGLTransformFeedback,
                Kind::VertexArray if o.shader_type == webgl_ext::OES_VERTEX_ARRAY => {
                    InterfaceId::WebGLVertexArrayObjectOES
                }
                Kind::VertexArray => InterfaceId::WebGLVertexArrayObject,
            },
            WebGlObject::UniformLocation { .. } => InterfaceId::WebGLUniformLocation,
            WebGlObject::ActiveInfo { .. } => InterfaceId::WebGLActiveInfo,
            WebGlObject::Precision(_) => InterfaceId::WebGLShaderPrecisionFormat,
            WebGlObject::Extension { index, .. } => webgl_ext::EXTENSIONS[*index].iface,
        }
    }

    /// The handles this object keeps alive.
    pub fn gc_refs(&self, nodes: &mut Vec<NodeId>, objects: &mut Vec<Handle>) {
        match self {
            WebGlObject::Context(c) => {
                match c.owner {
                    CanvasOwner::Element(el) => nodes.push(el),
                    CanvasOwner::Offscreen(h) => objects.push(h),
                }
                objects.extend(
                    [
                        c.array_buffer,
                        c.element_buffer,
                        c.framebuffer,
                        c.renderbuffer,
                        c.program,
                    ]
                    .into_iter()
                    .flatten(),
                );
                objects.extend(c.textures.iter().flatten().flatten());
                objects.extend(c.attrib_buffers.iter().flatten());
                c.gl2.gc_refs(objects);
                objects.extend(c.ext.objects.iter().map(|(_, h)| *h));
            }
            WebGlObject::Object(o) => {
                objects.push(o.context);
                objects.extend(&o.attached);
                objects.extend(o.attachments.iter().map(|(_, h)| *h));
                if let Some((element, attribs)) = &o.vao {
                    objects.extend(element);
                    objects.extend(attribs.iter().flatten());
                }
            }
            WebGlObject::UniformLocation {
                context, program, ..
            } => objects.extend([*context, *program]),
            WebGlObject::Extension { context, .. } => objects.push(*context),
            WebGlObject::ActiveInfo { .. } | WebGlObject::Precision(_) => {}
        }
    }
}

/// Where a page's WebGL contexts run when not on this process's own WebGL thread (`snowlark_gl::Service`):
/// a renderer's link to the GPU process. Each page and worker asks it for a backend of its own, the first
/// time it makes a context.
pub trait WebGlProvider: Send + Sync {
    fn backend(&self) -> Result<Box<dyn Backend>, String>;
    /// Gets ready ahead of the first context (a renderer asks for its link at its start, as the
    /// first context would otherwise wait for it). Blocks: called on a helper thread.
    fn prepare(&self) {}
}

/// The page's WebGL: the service, the commands not yet sent, the contexts by canvas.
#[derive(Default)]
pub struct GlState {
    backend: Option<Box<dyn Backend>>,
    /// Where the backend comes from (`None`: this process's WebGL thread).
    pub(crate) provider: Option<std::sync::Arc<dyn WebGlProvider>>,
    /// The service could not start (no ANGLE): every `getContext("webgl")` returns null.
    unavailable: bool,
    queue: Vec<(ContextId, Command)>,
    next_context: ContextId,
    pub contexts: FxHashMap<NodeId, Handle>,
    /// Canvases whose drawing buffer changed since the last rendering update.
    pub dirty: Vec<CanvasOwner>,
    /// Microtask checkpoints so far (WebGL 2.0 fences change state only between them).
    epoch: u64,
}

/// The pixels a canvas shows from a drawing buffer read back by the service (RGBA, bottom row
/// first): premultiplied BGRA, top row first, opaque when the context has no alpha.
/// Converted in place: a drawing buffer is read back each frame it changes, and a new buffer of
/// its size per frame costs more than the conversion.
fn to_frame(rb: snowlark_gl::Readback, a: &Attributes) -> snowlark_paint::image::Frame {
    let (w, h) = (rb.width as usize, rb.height as usize);
    let mut pixels = rb.rgba;
    pixels.resize(w * h * 4, 0);
    // Each pixel as a little-endian word: RGBA is 0xAABBGGRR, BGRA 0xAARRGGBB.
    if !a.alpha {
        flip_and_convert(&mut pixels, w, h, |v| {
            (v & 0x0000_ff00) | ((v >> 16) & 0xff) | ((v & 0xff) << 16) | 0xff00_0000
        });
    } else if a.premultiplied_alpha {
        flip_and_convert(&mut pixels, w, h, |v| {
            (v & 0xff00_ff00) | ((v >> 16) & 0xff) | ((v & 0xff) << 16)
        });
    } else {
        flip_and_convert(&mut pixels, w, h, |v| {
            let al = v >> 24;
            let m = |c: u32| (c * al + 127) / 255;
            let (r, g, b) = (v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff);
            (al << 24) | (m(r) << 16) | (m(g) << 8) | m(b)
        });
    }
    snowlark_paint::image::Frame {
        width: rb.width,
        height: rb.height,
        pixels,
        delay_ms: 0,
    }
}

/// Turns `h` rows of `w` pixels bottom row first into top row first, each pixel (a little-endian
/// word) through `convert`: rows swapped pairwise, the middle one left in place.
fn flip_and_convert(pixels: &mut [u8], w: usize, h: usize, convert: impl Fn(u32) -> u32 + Copy) {
    let row = w * 4;
    let fix = |line: &mut [u8]| {
        for px in line.as_chunks_mut::<4>().0 {
            *px = convert(u32::from_le_bytes(*px)).to_le_bytes();
        }
    };
    for y in 0..h / 2 {
        let (top, bottom) = pixels.split_at_mut((h - 1 - y) * row);
        let (a_row, b_row) = (&mut top[y * row..(y + 1) * row], &mut bottom[..row]);
        a_row.swap_with_slice(b_row);
        fix(a_row);
        fix(b_row);
    }
    if h % 2 == 1 {
        fix(&mut pixels[(h / 2) * row..(h / 2 + 1) * row]);
    }
}

/// BGRA pixels as RGBA.
pub(crate) fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut out = bgra.to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    out
}

fn js<T: ToJs + ?Sized>(cx: &mut Cx, v: &T) -> JsValue {
    let s = cx.scope();
    let l = v.to_js(s).unwrap_or_else(|| v8::null(s).into());
    JsValue::new(s, l)
}

fn null(cx: &mut Cx) -> JsValue {
    rt::null_value(cx.scope())
}

/// A new typed array of `T`'s kind holding `bytes`.
fn typed_array(cx: &mut Cx, bytes: Vec<u8>, elem: usize, kind: u8) -> JsValue {
    let s = cx.scope();
    let n = bytes.len() / elem;
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let ab = v8::ArrayBuffer::with_backing_store(s, &store);
    let arr: Option<v8::Local<v8::Value>> = match kind {
        0 => v8::Float32Array::new(s, ab, 0, n).map(Into::into),
        1 => v8::Int32Array::new(s, ab, 0, n).map(Into::into),
        _ => v8::Uint32Array::new(s, ab, 0, n).map(Into::into),
    };
    match arr {
        Some(a) => JsValue::new(s, a),
        None => rt::null_value(s),
    }
}

fn f32_array(cx: &mut Cx, v: &[f32]) -> JsValue {
    typed_array(cx, v.iter().flat_map(|x| x.to_le_bytes()).collect(), 4, 0)
}

fn i32_array(cx: &mut Cx, v: &[i32]) -> JsValue {
    typed_array(cx, v.iter().flat_map(|x| x.to_le_bytes()).collect(), 4, 1)
}

/// A Uint32Array of the values' bits.
fn u32_array(cx: &mut Cx, v: &[i32]) -> JsValue {
    typed_array(cx, v.iter().flat_map(|x| x.to_le_bytes()).collect(), 4, 2)
}

fn bool_array(cx: &mut Cx, v: &[bool]) -> JsValue {
    let s = cx.scope();
    let items: Vec<v8::Local<v8::Value>> =
        v.iter().map(|b| v8::Boolean::new(s, *b).into()).collect();
    let a = v8::Array::new_with_elements(s, &items);
    JsValue::new(s, a.into())
}

/// The element count of a uniform of `ty`, and whether it reads as floats, ints, booleans or
/// unsigned ints (0, 1, 2, 3).
fn uniform_shape(ty: u32) -> (u8, u8) {
    match ty {
        // OpenGL ES 3.0: non-square matrices (2x3, 2x4, 3x2, 3x4, 4x2, 4x3), unsigned ints and
        // their vectors, and the samplers (int): 3D, 2D array, shadow, integer.
        0x8B65 | 0x8B67 => (6, 0),
        0x8B66 | 0x8B69 => (8, 0),
        0x8B68 | 0x8B6A => (12, 0),
        0x1405 => (1, 3),
        0x8DC6 => (2, 3),
        0x8DC7 => (3, 3),
        0x8DC8 => (4, 3),
        0x8B5F | 0x8DC1 | 0x8B62 | 0x8DC4 | 0x8DC5 | 0x8DCA..=0x8DCF | 0x8DD2..=0x8DD7 => (1, 1),
        0x1406 => (1, 0),
        0x8B50 => (2, 0),
        0x8B51 => (3, 0),
        0x8B52 => (4, 0),
        0x8B5A => (4, 0),
        0x8B5B => (9, 0),
        0x8B5C => (16, 0),
        0x1404 | 0x8B5E | 0x8B60 => (1, 1),
        0x8B53 => (2, 1),
        0x8B54 => (3, 1),
        0x8B55 => (4, 1),
        0x8B56 => (1, 2),
        0x8B57 => (2, 2),
        0x8B58 => (3, 2),
        0x8B59 => (4, 2),
        _ => (0, 0),
    }
}

/// The bytes of a typed array and its element kind: the array's constructor name.
fn view_bytes(cx: &mut Cx, o: &JsObject) -> Vec<u8> {
    super::encoding::buffer_bytes(cx, o)
}

macro_rules! view_object {
    ($v:expr, $T:ident) => {{
        use g::$T as V;
        match $v {
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
    }};
}

type View = g::Int8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView;
type BufferSource = g::ArrayBufferOrSharedArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView;

fn buffer_source_object(v: &BufferSource) -> &JsObject {
    use BufferSource as V;
    match v {
        V::ArrayBuffer(o) | V::SharedArrayBuffer(o) => o,
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

fn floats(cx: &mut Cx, v: &g::Float32ArrayOrUnrestrictedFloatSequence) -> Vec<f32> {
    match v {
        g::Float32ArrayOrUnrestrictedFloatSequence::UnrestrictedFloatSequence(s) => s.clone(),
        g::Float32ArrayOrUnrestrictedFloatSequence::Float32Array(o) => view_bytes(cx, o)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect(),
    }
}

fn ints(cx: &mut Cx, v: &g::Int32ArrayOrLongSequence) -> Vec<i32> {
    match v {
        g::Int32ArrayOrLongSequence::LongSequence(s) => s.clone(),
        g::Int32ArrayOrLongSequence::Int32Array(o) => view_bytes(cx, o)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| i32::from_le_bytes(*b))
            .collect(),
    }
}

impl Engine {
    fn gl_ctx<R>(&self, h: Handle, f: impl FnOnce(&mut ContextObj) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::WebGl(o) => match &mut **o {
                WebGlObject::Context(c) => f(c),
                _ => panic!("not a WebGLRenderingContext"),
            },
            _ => panic!("not a WebGLRenderingContext"),
        }
    }

    fn gl_obj<R>(&self, h: Handle, f: impl FnOnce(&mut GlObject) -> R) -> Option<R> {
        match &mut *self.obj_mut(h) {
            Object::WebGl(o) => match &mut **o {
                WebGlObject::Object(x) => Some(f(x)),
                _ => None,
            },
            _ => None,
        }
    }

    fn gl_backend(&self) -> Option<std::cell::RefMut<'_, Box<dyn Backend>>> {
        let mut gl = self.gl.borrow_mut();
        if gl.backend.is_none() && !gl.unavailable {
            let made = match &gl.provider {
                Some(p) => p.backend(),
                None => snowlark_gl::Service::from_env().map(|s| Box::new(s) as Box<dyn Backend>),
            };
            match made {
                Ok(b) => gl.backend = Some(b),
                Err(_) => gl.unavailable = true,
            }
        }
        std::cell::RefMut::filter_map(gl, |g| g.backend.as_mut()).ok()
    }

    /// Queues a command for the context `this` (nothing when it is lost).
    fn gl_send(&self, this: Handle, cmd: Command) {
        let (id, lost) = self.gl_ctx(this, |c| (c.id, c.lost));
        if !lost {
            self.gl.borrow_mut().queue.push((id, cmd));
        }
    }

    /// Sends the queued commands.
    fn gl_flush(&self) {
        let batch = std::mem::take(&mut self.gl.borrow_mut().queue);
        if batch.is_empty() {
            return;
        }
        if let Some(mut b) = self.gl_backend() {
            b.submit(batch);
        }
    }

    /// Runs a query for the context `this` after the queued commands (`None` when it is lost).
    fn gl_query(&self, this: Handle, q: Query) -> Option<Reply> {
        let (id, lost) = self.gl_ctx(this, |c| (c.id, c.lost));
        if lost {
            return None;
        }
        self.gl_flush();
        let mut b = self.gl_backend()?;
        Some(b.query(id, &q))
    }

    /// Records an error for `getError`.
    fn gl_error(&self, this: Handle, error: u32) {
        self.gl_ctx(this, |c| {
            if !c.errors.contains(&error) {
                c.errors.push(error);
            }
        });
    }

    /// The drawing buffer changed: it is shown at the next rendering update.
    fn gl_drew(&self, this: Handle) {
        let (el, fb) = self.gl_ctx(this, |c| (c.owner, c.framebuffer));
        if fb.is_some() {
            return;
        }
        let mut gl = self.gl.borrow_mut();
        if !gl.dirty.contains(&el) {
            gl.dirty.push(el);
        }
        drop(gl);
        self.render.borrow_mut().dirty = true;
    }

    /// WebGL 1.0 §5.14 "validate a WebGL object": `h` must be an object of `kind` made by this
    /// context (else INVALID_OPERATION) and not deleted (else INVALID_VALUE). Its id.
    fn gl_check(&self, this: Handle, h: Handle, kind: Kind) -> Option<u32> {
        let r = self.gl_obj(h, |o| (o.context, o.kind, o.deleted, o.id));
        match r {
            Some((ctx, k, deleted, id)) if ctx == this && k == kind => {
                if deleted {
                    self.gl_error(this, c::INVALID_VALUE);
                    None
                } else {
                    Some(id)
                }
            }
            _ => {
                self.gl_error(this, c::INVALID_OPERATION);
                None
            }
        }
    }

    /// An object to bind or attach (null allowed): as [`Engine::gl_check`], except that a deleted
    /// object is INVALID_OPERATION (WebGL 1.0 §5.14: binding a deleted object).
    fn gl_check_opt(&self, this: Handle, h: Option<Handle>, kind: Kind) -> Option<u32> {
        let Some(h) = h else { return Some(0) };
        if self.gl_obj(h, |o| o.context == this && o.kind == kind && o.deleted) == Some(true) {
            self.gl_error(this, c::INVALID_OPERATION);
            return None;
        }
        self.gl_check(this, h, kind)
    }

    fn gl_lost(&self, this: Handle) -> bool {
        self.gl_ctx(this, |c| c.lost)
    }

    /// The context's unpack parameters (`pixelStorei`).
    fn gl_unpack(&self, this: Handle) -> pixels::Unpack {
        self.gl_ctx(this, |c| pixels::Unpack {
            flip_y: c.flip_y,
            premultiply_alpha: c.premultiply_alpha,
            alignment: c.unpack_alignment as u32,
        })
    }

    /// `UNPACK_FLIP_Y_WEBGL` and `UNPACK_PREMULTIPLY_ALPHA_WEBGL` applied to the bytes of an
    /// `ArrayBufferView` upload (WebGL 1.0 §5.14.8: they apply to every upload).
    fn gl_unpack_view(
        &self,
        this: Handle,
        bytes: &mut [u8],
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
    ) {
        let unpack = self.gl_unpack(this);
        if (unpack.flip_y || unpack.premultiply_alpha) && width > 0 && height > 0 {
            pixels::unpack_view(bytes, width as u32, height as u32, format, type_, unpack);
        }
    }

    /// The pixels of a `TexImageSource` (WebGL 1.0 §5.14.8): width, height, RGBA top row first,
    /// and whether they are premultiplied. `Err(None)` for a source with nothing to upload yet (an
    /// image not decoded or broken), `Err(Some(_))` the exception for one script may not read;
    /// `Ok(None)` for one this engine cannot read yet (a video's frame lives on the graphics card
    /// the media pipeline decodes to).
    #[allow(clippy::type_complexity)]
    fn tex_source_pixels(
        &self,
        cx: &mut Cx,
        source: &g::ImageBitmapOrImageDataOrHTMLImageElementOrHTMLCanvasElementOrHTMLVideoElementOrOffscreenCanvas,
    ) -> Result<Option<(u32, u32, Vec<u8>, bool)>, Option<rt::Error>> {
        use g::ImageBitmapOrImageDataOrHTMLImageElementOrHTMLCanvasElementOrHTMLVideoElementOrOffscreenCanvas as S;
        match source {
            S::ImageData(h) => {
                let (w, hh, data) = self.image_data_bytes(cx, *h).ok_or(None)?;
                Ok(Some((w, hh, data, false)))
            }
            S::HTMLImageElement(h) => {
                let el = self.n(*h);
                let decoded = self
                    .with_node_renderer(el, |r, _| r.image_pixels(el))
                    .flatten()
                    .ok_or(None)?;
                // WebGL 1.0 §5.14.8: an image that is not CORS-same-origin throws (HTML's
                // origin-clean rules).
                if !self.img_origin_clean(el) {
                    return Err(Some(super::exception(
                        rt::ExceptionName::SecurityError,
                        "The image is cross-origin and not shared by CORS",
                    )));
                }
                // (the image canvas drawImage draws: an animation's default image)
                let frame = decoded.default_frame();
                if frame.pixels.len() != frame.width as usize * frame.height as usize * 4 {
                    return Err(None);
                }
                Ok(Some((
                    frame.width,
                    frame.height,
                    bgra_to_rgba(&frame.pixels),
                    true,
                )))
            }
            S::HTMLCanvasElement(_) | S::OffscreenCanvas(_) => {
                let src = match source {
                    S::OffscreenCanvas(h) => {
                        // WebGL 1.0 §5.14.8: a detached OffscreenCanvas throws.
                        if self.offscreen_detached(*h) {
                            return Err(Some(super::exception(
                                rt::ExceptionName::InvalidStateError,
                                "The OffscreenCanvas is detached",
                            )));
                        }
                        CanvasOwner::Offscreen(*h)
                    }
                    S::HTMLCanvasElement(h) => CanvasOwner::Element(self.n(*h)),
                    _ => unreachable!(),
                };
                // WebGL 1.0 §5.14.8: a canvas whose bitmap is not origin-clean throws.
                if !self.canvas_origin_clean(src) {
                    return Err(Some(super::Engine::tainted_error()));
                }
                let (w, hh, rgba) = self.canvas_source_pixels(src).ok_or(None)?;
                Ok(Some((w, hh, rgba, true)))
            }
            S::ImageBitmap(h) => {
                // WebGL 1.0 §5.14.8: a closed ImageBitmap throws InvalidStateError, one that is
                // not origin-clean SecurityError.
                let Some((frame, clean, _)) = self.bitmap_pixels(*h) else {
                    return Err(Some(super::exception(
                        rt::ExceptionName::InvalidStateError,
                        "The ImageBitmap is closed",
                    )));
                };
                if !clean {
                    return Err(Some(super::Engine::tainted_error()));
                }
                Ok(Some((
                    frame.width,
                    frame.height,
                    bgra_to_rgba(&frame.pixels),
                    true,
                )))
            }
            S::HTMLVideoElement(_) => Ok(None),
        }
    }

    /// `texImage2D` or `texSubImage2D` from a `TexImageSource`: the source's pixels packed as
    /// `format` and `type_` with the context's unpack parameters, sent as the command `upload` makes
    /// for the source's size. A format and type with no upload is sent without pixels, for the
    /// service to refuse with the error OpenGL ES gives.
    fn tex_from_source(
        &self,
        cx: &mut Cx,
        this: Handle,
        source: &g::ImageBitmapOrImageDataOrHTMLImageElementOrHTMLCanvasElementOrHTMLVideoElementOrOffscreenCanvas,
        format: u32,
        type_: u32,
        upload: impl FnOnce(i32, i32, Option<Vec<u8>>) -> Command,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let (w, h, data, premultiplied) = match self.tex_source_pixels(cx, source) {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(()),
            Err(Some(e)) => return Err(e),
            Err(None) => {
                self.gl_error(this, c::INVALID_VALUE);
                return Ok(());
            }
        };
        let src = pixels::Rgba8 {
            width: w,
            height: h,
            data: &data,
            premultiplied,
        };
        let mut unpack = self.gl_unpack(this);
        if let g::ImageBitmapOrImageDataOrHTMLImageElementOrHTMLCanvasElementOrHTMLVideoElementOrOffscreenCanvas::ImageBitmap(b) = source {
            // WebGL 2.0 §5.40 (and 1.0 with ImageBitmap): an ImageBitmap is uploaded as it is,
            // `UNPACK_FLIP_Y_WEBGL` and `UNPACK_PREMULTIPLY_ALPHA_WEBGL` ignored; its alpha is
            // premultiplied unless it was made with `premultiplyAlpha: "none"`.
            let straight = self.bitmap_pixels(*b).is_some_and(|(_, _, s)| s);
            unpack.flip_y = false;
            unpack.premultiply_alpha = !straight;
        }
        let bytes = pixels::pack(&src, format, type_, unpack);
        self.gl_send(this, upload(w as i32, h as i32, bytes));
        Ok(())
    }

    fn gl_create(&self, this: Handle, kind: Kind, shader_type: u32) -> Handle {
        let id = self.gl_ctx(this, |c| {
            c.next_object += 1;
            c.next_object
        });
        self.gl_send(
            this,
            match kind {
                Kind::Shader => Command::CreateShader(id, shader_type),
                Kind::Sync => Command::FenceSync(id),
                _ => Command::Create(kind, id),
            },
        );
        let epoch = self.gl.borrow().epoch;
        self.alloc(Object::WebGl(Box::new(WebGlObject::Object(GlObject {
            context: this,
            kind,
            id,
            deleted: false,
            bound: false,
            label: String::new(),
            shader_type,
            source: String::new(),
            attached: Vec::new(),
            links: 0,
            attachments: Vec::new(),
            vao: (kind == Kind::VertexArray).then(|| {
                let attribs = self.gl_ctx(this, |c| c.attrib_buffers.len());
                (None, vec![None; attribs])
            }),
            epoch,
            target: 0,
        }))))
    }

    /// A microtask checkpoint ran: fences made before it may report their status.
    pub(crate) fn webgl_checkpoint(&self) {
        if let Ok(mut gl) = self.gl.try_borrow_mut() {
            gl.epoch += 1;
        }
    }

    fn gl_delete(&self, this: Handle, h: Option<Handle>, kind: Kind) {
        let Some(h) = h else { return };
        if self.gl_lost(this) {
            return;
        }
        let Some(id) = self.gl_check(this, h, kind) else {
            return;
        };
        self.gl_obj(h, |o| o.deleted = true);
        // (GL unbinds a deleted object from the current context's bindings; so does WebGL's
        // state here, except the current program, which stays until another is used)
        self.gl_ctx(this, |c| {
            for slot in [
                &mut c.array_buffer,
                &mut c.element_buffer,
                &mut c.framebuffer,
                &mut c.renderbuffer,
            ] {
                if *slot == Some(h) {
                    *slot = None;
                }
            }
            for unit in &mut c.textures {
                for t in unit.iter_mut() {
                    if *t == Some(h) {
                        *t = None;
                    }
                }
            }
            for a in &mut c.attrib_buffers {
                if *a == Some(h) {
                    *a = None;
                }
            }
            c.forget(h);
        });
        self.gl_send(this, Command::Delete(kind, id));
    }

    fn gl_is(&self, this: Handle, h: Option<Handle>, kind: Kind) -> bool {
        let Some(h) = h else { return false };
        if self.gl_lost(this) {
            return false;
        }
        self.gl_obj(h, |o| {
            o.context == this
                && o.kind == kind
                && !o.deleted
                && (o.bound
                    || matches!(
                        kind,
                        Kind::Program | Kind::Shader | Kind::Sampler | Kind::Sync
                    ))
        })
        .unwrap_or(false)
    }

    /// HTML §4.12.5 `getContext("webgl")`: the canvas's WebGL context, made on first use; null when
    /// WebGL cannot run here. (An OffscreenCanvas keeps the context it gets itself.)
    pub(crate) fn webgl_get_context(
        &self,
        cx: &mut Cx,
        el: CanvasOwner,
        options: &JsValue,
        webgl2: bool,
    ) -> rt::Result<Option<Handle>> {
        if let Some(h) = self.source_webgl(el) {
            // (a canvas with a WebGL 1.0 context has no WebGL 2.0 one, and the reverse)
            return Ok((self.gl_ctx(h, |c| c.attributes.webgl2) == webgl2).then_some(h));
        }
        let o = if options.is_undefined(cx) {
            g::WebGLContextAttributes::default()
        } else {
            let s = cx.scope();
            let v = v8::Local::new(s, options.global());
            if v.is_null() {
                g::WebGLContextAttributes::default()
            } else {
                g::WebGLContextAttributes::from_value(cx, options)?
            }
        };
        let asked = Attributes {
            alpha: o.alpha,
            depth: o.depth,
            stencil: o.stencil,
            antialias: o.antialias,
            premultiplied_alpha: o.premultiplied_alpha,
            preserve_drawing_buffer: o.preserve_drawing_buffer,
            power_preference: match o.power_preference {
                g::WebGLPowerPreference::Default => 0,
                g::WebGLPowerPreference::LowPower => 1,
                g::WebGLPowerPreference::HighPerformance => 2,
            },
            fail_if_major_performance_caveat: o.fail_if_major_performance_caveat,
            desynchronized: o.desynchronized,
            webgl2,
        };
        let (w, h) = self.source_size(el);
        let id = {
            let mut gl = self.gl.borrow_mut();
            gl.next_context += 1;
            gl.next_context
        };
        let created = {
            let Some(mut b) = self.gl_backend() else {
                return Ok(None);
            };
            match b.create_context(id, &asked, w, h) {
                Ok(c) => c,
                Err(_) => return Ok(None),
            }
        };
        let units = created.limits.max_combined_texture_image_units.clamp(1, 64) as usize;
        let attribs = created.limits.max_vertex_attribs.clamp(1, 64) as usize;
        let ctx = self.alloc(Object::WebGl(Box::new(WebGlObject::Context(Box::new(
            ContextObj {
                owner: el,
                id,
                attributes: created.attributes,
                limits: created.limits,
                width: w,
                height: h,
                lost: false,
                errors: Vec::new(),
                next_object: 0,
                array_buffer: None,
                element_buffer: None,
                framebuffer: None,
                renderbuffer: None,
                program: None,
                active_unit: 0,
                textures: vec![[None; 4]; units],
                gl2: Gl2State::new(&created.limits, units),
                ext: ExtState::new(created.extensions, created.requestable),
                attrib_buffers: vec![None; attribs],
                pack_alignment: 4,
                unpack_alignment: 4,
                flip_y: false,
                premultiply_alpha: false,
                colorspace_conversion: c::BROWSER_DEFAULT_WEBGL,
                drawing_buffer_color_space: g::PredefinedColorSpace::Srgb,
                unpack_color_space: g::PredefinedColorSpace::Srgb,
            },
        )))));
        let mut gl = self.gl.borrow_mut();
        if let CanvasOwner::Element(el) = el {
            gl.contexts.insert(el, ctx);
        }
        // (the canvas shows its cleared drawing buffer from the next rendering update)
        gl.dirty.push(el);
        drop(gl);
        self.render.borrow_mut().dirty = true;
        Ok(Some(ctx))
    }

    /// A canvas's `width` or `height` changed: its WebGL drawing buffer is made again at the new
    /// size (WebGL 1.0 §2.2), cleared.
    pub(crate) fn webgl_resized(&self, el: CanvasOwner) {
        let Some(h) = self.source_webgl(el) else {
            return;
        };
        let (w, hh) = self.source_size(el);
        let id = self.gl_ctx(h, |c| {
            c.width = w;
            c.height = hh;
            c.id
        });
        self.gl_flush();
        if let Some(mut b) = self.gl_backend() {
            b.resize(id, w, hh);
        }
        let mut gl = self.gl.borrow_mut();
        if !gl.dirty.contains(&el) {
            gl.dirty.push(el);
        }
    }

    /// The rendering update's part for WebGL: the drawing buffers that changed are read back and
    /// handed to the renderer.
    pub(crate) fn flush_webgl(&self) {
        let dirty = std::mem::take(&mut self.gl.borrow_mut().dirty);
        if dirty.is_empty() {
            return;
        }
        self.gl_flush();
        for el in dirty {
            let Some(h) = self.source_webgl(el) else {
                continue;
            };
            let (id, attrs, lost) = self.gl_ctx(h, |c| (c.id, c.attributes, c.lost));
            if lost {
                continue;
            }
            let (readback, reset) = {
                let Some(mut b) = self.gl_backend() else {
                    return;
                };
                let reset = b.query(id, &Query::ResetStatus).int() != 0;
                (if reset { None } else { b.present(id) }, reset)
            };
            if reset {
                // WebGL 1.0 §5.15.2: the context is lost; script sees it from now on.
                self.gl_ctx(h, |c| {
                    c.lost = true;
                    c.errors.push(c::CONTEXT_LOST_WEBGL);
                });
                continue;
            }
            if let Some(rb) = readback {
                let frame = to_frame(rb, &attrs);
                match el {
                    CanvasOwner::Element(el) => {
                        self.show_canvas_frame(el, Some(frame));
                    }
                    // (a WebGL drawing buffer is always origin-clean)
                    CanvasOwner::Offscreen(oc) => self.commit_offscreen_frame(oc, frame, true),
                }
            }
        }
    }

    /// The drawing buffer's pixels now (`toDataURL`), un-premultiplied BGRA... as a frame.
    pub(crate) fn webgl_snapshot(&self, el: NodeId) -> Option<snowlark_paint::image::Frame> {
        let h = self.gl.borrow().contexts.get(&el).copied()?;
        self.webgl_snapshot_of(h)
    }

    /// [`Engine::webgl_snapshot`] of the context `h`.
    pub(crate) fn webgl_snapshot_of(&self, h: Handle) -> Option<snowlark_paint::image::Frame> {
        let (id, attrs, lost) = self.gl_ctx(h, |c| (c.id, c.attributes, c.lost));
        if lost {
            return None;
        }
        self.gl_flush();
        // (reading for toDataURL must not clear a buffer that is not preserved)
        let mut keep = attrs;
        keep.preserve_drawing_buffer = true;
        let rgba = {
            let mut b = self.gl_backend()?;
            let (w, hh) = self.gl_ctx(h, |c| (c.width.max(1), c.height.max(1)));
            match b.query(
                id,
                &Query::ReadPixels(
                    0,
                    0,
                    w as i32,
                    hh as i32,
                    c::RGBA,
                    c::UNSIGNED_BYTE,
                    w * hh * 4,
                ),
            ) {
                Reply::Bytes(bytes) => snowlark_gl::Readback {
                    width: w,
                    height: hh,
                    rgba: bytes,
                },
                _ => return None,
            }
        };
        Some(to_frame(rgba, &keep))
    }

    /// Whether the canvas has a WebGL context.
    pub(crate) fn has_webgl(&self, el: NodeId) -> bool {
        self.gl.borrow().contexts.contains_key(&el)
    }

    fn uniform_check(&self, this: Handle, location: Option<Handle>) -> Option<i32> {
        let location = location?;
        if self.gl_lost(this) {
            return None;
        }
        let loc = match &*self.obj(location) {
            Object::WebGl(o) => match &**o {
                WebGlObject::UniformLocation {
                    context,
                    program,
                    links,
                    location,
                    ..
                } => Some((*context, *program, *links, *location)),
                _ => None,
            },
            _ => None,
        };
        let (context, program, links, location) = loc?;
        let current = self.gl_ctx(this, |c| c.program);
        let program_links = self.gl_obj(program, |o| o.links).unwrap_or(0);
        if context != this || current != Some(program) || program_links != links {
            self.gl_error(this, c::INVALID_OPERATION);
            return None;
        }
        Some(location)
    }

    fn uniform_v(&self, this: Handle, location: Option<Handle>, n: u8, v: Vec<f32>) {
        if let Some(loc) = self.uniform_check(this, location) {
            if v.is_empty() || !v.len().is_multiple_of(n as usize) {
                return self.gl_error(this, c::INVALID_VALUE);
            }
            self.gl_send(this, Command::Uniformf(loc, n, v));
        }
    }

    fn uniform_iv(&self, this: Handle, location: Option<Handle>, n: u8, v: Vec<i32>) {
        if let Some(loc) = self.uniform_check(this, location) {
            if v.is_empty() || !v.len().is_multiple_of(n as usize) {
                return self.gl_error(this, c::INVALID_VALUE);
            }
            self.gl_send(this, Command::Uniformi(loc, n, v));
        }
    }

    fn uniform_matrix(&self, this: Handle, location: Option<Handle>, n: u8, t: bool, v: Vec<f32>) {
        if let Some(loc) = self.uniform_check(this, location) {
            let size = n as usize * n as usize;
            // WebGL 1.0 §5.14.10: transpose must be false.
            if t {
                return self.gl_error(this, c::INVALID_VALUE);
            }
            if v.is_empty() || !v.len().is_multiple_of(size) {
                return self.gl_error(this, c::INVALID_VALUE);
            }
            self.gl_send(this, Command::UniformMatrix(loc, n, t, v));
        }
    }

    fn vertex_attrib_v(&self, this: Handle, index: u32, n: usize, v: Vec<f32>) {
        if self.gl_lost(this) {
            return;
        }
        if v.len() < n {
            return self.gl_error(this, c::INVALID_VALUE);
        }
        let mut out = [0.0, 0.0, 0.0, 1.0];
        out[..n].copy_from_slice(&v[..n]);
        self.gl_send(this, Command::VertexAttrib(index, out));
    }

    fn int_param(&self, this: Handle, q: Query, cx: &mut Cx) -> JsValue {
        match self.gl_query(this, q) {
            Some(Reply::Int(v)) => js(cx, &v),
            _ => null(cx),
        }
    }
}

impl WebGLRenderingContextBaseImpl for Engine {
    fn canvas(&self, this: Handle) -> g::HTMLCanvasElementOrOffscreenCanvas {
        match self.gl_ctx(this, |c| c.owner) {
            CanvasOwner::Element(el) => {
                g::HTMLCanvasElementOrOffscreenCanvas::HTMLCanvasElement(super::node_handle(el))
            }
            CanvasOwner::Offscreen(h) => g::HTMLCanvasElementOrOffscreenCanvas::OffscreenCanvas(h),
        }
    }
    fn drawing_buffer_width(&self, this: Handle) -> i32 {
        self.gl_ctx(this, |c| if c.lost { 0 } else { c.width as i32 })
    }
    fn drawing_buffer_height(&self, this: Handle) -> i32 {
        self.gl_ctx(this, |c| if c.lost { 0 } else { c.height as i32 })
    }
    fn drawing_buffer_format(&self, _this: Handle) -> u32 {
        c::RGBA8
    }
    fn drawing_buffer_color_space(&self, this: Handle) -> g::PredefinedColorSpace {
        self.gl_ctx(this, |c| c.drawing_buffer_color_space)
    }
    fn set_drawing_buffer_color_space(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: g::PredefinedColorSpace,
    ) -> rt::Result<()> {
        self.gl_ctx(this, |c| c.drawing_buffer_color_space = value);
        Ok(())
    }
    fn unpack_color_space(&self, this: Handle) -> g::PredefinedColorSpace {
        self.gl_ctx(this, |c| c.unpack_color_space)
    }
    fn set_unpack_color_space(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: g::PredefinedColorSpace,
    ) -> rt::Result<()> {
        self.gl_ctx(this, |c| c.unpack_color_space = value);
        Ok(())
    }
    fn get_context_attributes(
        &self,
        _cx: &mut Cx,
        this: Handle,
    ) -> rt::Result<Option<g::WebGLContextAttributes>> {
        let (a, lost) = self.gl_ctx(this, |c| (c.attributes, c.lost));
        if lost {
            return Ok(None);
        }
        Ok(Some(g::WebGLContextAttributes {
            alpha: a.alpha,
            antialias: a.antialias,
            depth: a.depth,
            desynchronized: a.desynchronized,
            fail_if_major_performance_caveat: a.fail_if_major_performance_caveat,
            power_preference: match a.power_preference {
                1 => g::WebGLPowerPreference::LowPower,
                2 => g::WebGLPowerPreference::HighPerformance,
                _ => g::WebGLPowerPreference::Default,
            },
            premultiplied_alpha: a.premultiplied_alpha,
            preserve_drawing_buffer: a.preserve_drawing_buffer,
            stencil: a.stencil,
            xr_compatible: false,
        }))
    }
    fn is_context_lost(&self, _cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        Ok(self.gl_lost(this))
    }
    fn get_supported_extensions(
        &self,
        _cx: &mut Cx,
        this: Handle,
    ) -> rt::Result<Option<Vec<String>>> {
        Ok((!self.gl_lost(this)).then(|| self.gl_supported_extensions(this)))
    }
    fn get_extension(&self, cx: &mut Cx, this: Handle, name: &str) -> rt::Result<Option<JsObject>> {
        self.gl_get_extension(cx, this, name)
    }
    fn active_texture(&self, _cx: &mut Cx, this: Handle, texture: u32) -> rt::Result<()> {
        let units = self.gl_ctx(this, |c| c.textures.len() as u32);
        if (c::TEXTURE0..c::TEXTURE0 + units).contains(&texture) {
            self.gl_ctx(this, |c| c.active_unit = texture - c::TEXTURE0);
        }
        self.gl_send(this, Command::ActiveTexture(texture));
        Ok(())
    }
    fn attach_shader(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        shader: Handle,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let (Some(p), Some(s)) = (
            self.gl_check(this, program, Kind::Program),
            self.gl_check(this, shader, Kind::Shader),
        ) else {
            return Ok(());
        };
        // (a second shader of the same type, or the same one twice: INVALID_OPERATION)
        let ty = self.gl_obj(shader, |o| o.shader_type).unwrap_or(0);
        let attached = self
            .gl_obj(program, |o| o.attached.clone())
            .unwrap_or_default();
        if attached
            .iter()
            .any(|a| *a == shader || self.gl_obj(*a, |o| o.shader_type) == Some(ty))
        {
            self.gl_error(this, c::INVALID_OPERATION);
            return Ok(());
        }
        self.gl_obj(program, |o| o.attached.push(shader));
        self.gl_send(this, Command::AttachShader(p, s));
        Ok(())
    }
    fn bind_attrib_location(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        index: u32,
        name: &str,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        if let Some(p) = self.gl_check(this, program, Kind::Program) {
            self.gl_send(this, Command::BindAttribLocation(p, index, name.to_owned()));
        }
        Ok(())
    }
    fn bind_buffer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        buffer: Option<Handle>,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(id) = self.gl_check_opt(this, buffer, Kind::Buffer) else {
            return Ok(());
        };
        self.gl_ctx(this, |c| match target {
            c::ARRAY_BUFFER => c.array_buffer = buffer,
            c::ELEMENT_ARRAY_BUFFER => c.element_buffer = buffer,
            _ if c.attributes.webgl2 => {
                if let Some(slot) = webgl2::buffer_slot(target) {
                    c.gl2.buffers[slot] = buffer;
                }
            }
            _ => {}
        });
        if let Some(b) = buffer {
            self.gl_obj(b, |o| o.bound = true);
        }
        self.gl_send(this, Command::BindBuffer(target, id));
        Ok(())
    }
    fn bind_framebuffer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        framebuffer: Option<Handle>,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(id) = self.gl_check_opt(this, framebuffer, Kind::Framebuffer) else {
            return Ok(());
        };
        // (WebGL 2.0: FRAMEBUFFER binds both; DRAW_FRAMEBUFFER and READ_FRAMEBUFFER one each)
        self.gl_ctx(this, |c| match target {
            c::FRAMEBUFFER => {
                c.framebuffer = framebuffer;
                c.gl2.read_framebuffer = framebuffer;
            }
            webgl2::DRAW_FRAMEBUFFER if c.attributes.webgl2 => c.framebuffer = framebuffer,
            webgl2::READ_FRAMEBUFFER if c.attributes.webgl2 => c.gl2.read_framebuffer = framebuffer,
            _ => {}
        });
        if let Some(b) = framebuffer {
            self.gl_obj(b, |o| o.bound = true);
        }
        self.gl_send(this, Command::BindFramebuffer(target, id));
        Ok(())
    }
    fn bind_renderbuffer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        renderbuffer: Option<Handle>,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(id) = self.gl_check_opt(this, renderbuffer, Kind::Renderbuffer) else {
            return Ok(());
        };
        if target == c::RENDERBUFFER {
            self.gl_ctx(this, |c| c.renderbuffer = renderbuffer);
        }
        if let Some(b) = renderbuffer {
            self.gl_obj(b, |o| o.bound = true);
        }
        self.gl_send(this, Command::BindRenderbuffer(target, id));
        Ok(())
    }
    fn bind_texture(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        texture: Option<Handle>,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(id) = self.gl_check_opt(this, texture, Kind::Texture) else {
            return Ok(());
        };
        let webgl2 = self.webgl_version_2(this);
        let slot = match target {
            c::TEXTURE_2D => Some(0),
            c::TEXTURE_CUBE_MAP => Some(1),
            webgl2::TEXTURE_3D if webgl2 => Some(2),
            webgl2::TEXTURE_2D_ARRAY if webgl2 => Some(3),
            _ => None,
        };
        if let Some(slot) = slot {
            self.gl_ctx(this, |c| {
                let unit = c.active_unit as usize;
                if let Some(u) = c.textures.get_mut(unit) {
                    u[slot] = texture;
                }
            });
        }
        if let Some(b) = texture {
            self.gl_obj(b, |o| o.bound = true);
        }
        self.gl_send(this, Command::BindTexture(target, id));
        Ok(())
    }
    fn blend_color(
        &self,
        _cx: &mut Cx,
        this: Handle,
        r: f32,
        g_: f32,
        b: f32,
        a: f32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::BlendColor([r, g_, b, a]));
        Ok(())
    }
    fn blend_equation(&self, _cx: &mut Cx, this: Handle, mode: u32) -> rt::Result<()> {
        self.gl_send(this, Command::BlendEquationSeparate(mode, mode));
        Ok(())
    }
    fn blend_equation_separate(
        &self,
        _cx: &mut Cx,
        this: Handle,
        rgb: u32,
        alpha: u32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::BlendEquationSeparate(rgb, alpha));
        Ok(())
    }
    fn blend_func(&self, _cx: &mut Cx, this: Handle, s: u32, d: u32) -> rt::Result<()> {
        self.gl_send(this, Command::BlendFuncSeparate(s, d, s, d));
        Ok(())
    }
    fn blend_func_separate(
        &self,
        _cx: &mut Cx,
        this: Handle,
        src_rgb: u32,
        dst_rgb: u32,
        src_alpha: u32,
        dst_alpha: u32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::BlendFuncSeparate(src_rgb, dst_rgb, src_alpha, dst_alpha),
        );
        Ok(())
    }
    fn check_framebuffer_status(&self, _cx: &mut Cx, this: Handle, target: u32) -> rt::Result<u32> {
        Ok(self
            .gl_query(this, Query::CheckFramebufferStatus(target))
            .map_or(0, |r| r.int() as u32))
    }
    fn clear(&self, _cx: &mut Cx, this: Handle, mask: u32) -> rt::Result<()> {
        self.gl_send(this, Command::Clear(mask));
        self.gl_drew(this);
        Ok(())
    }
    fn clear_color(
        &self,
        _cx: &mut Cx,
        this: Handle,
        r: f32,
        g_: f32,
        b: f32,
        a: f32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::ClearColor([r, g_, b, a]));
        Ok(())
    }
    fn clear_depth(&self, _cx: &mut Cx, this: Handle, depth: f32) -> rt::Result<()> {
        self.gl_send(this, Command::ClearDepth(depth));
        Ok(())
    }
    fn clear_stencil(&self, _cx: &mut Cx, this: Handle, s: i32) -> rt::Result<()> {
        self.gl_send(this, Command::ClearStencil(s));
        Ok(())
    }
    fn color_mask(
        &self,
        _cx: &mut Cx,
        this: Handle,
        r: bool,
        g_: bool,
        b: bool,
        a: bool,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::ColorMask([r, g_, b, a]));
        Ok(())
    }
    fn compile_shader(&self, _cx: &mut Cx, this: Handle, shader: Handle) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        if let Some(s) = self.gl_check(this, shader, Kind::Shader) {
            self.gl_send(this, Command::CompileShader(s));
        }
        Ok(())
    }
    fn copy_tex_image2_d(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        border: i32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::CopyTexImage2D(target, level, internalformat, x, y, width, height, border),
        );
        Ok(())
    }
    fn copy_tex_sub_image2_d(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::CopyTexSubImage2D(target, level, xoffset, yoffset, x, y, width, height),
        );
        Ok(())
    }
    fn create_buffer(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.gl_create(this, Kind::Buffer, 0))
    }
    fn create_framebuffer(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.gl_create(this, Kind::Framebuffer, 0))
    }
    fn create_program(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.gl_create(this, Kind::Program, 0))
    }
    fn create_renderbuffer(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.gl_create(this, Kind::Renderbuffer, 0))
    }
    fn create_shader(&self, _cx: &mut Cx, this: Handle, type_: u32) -> rt::Result<Option<Handle>> {
        if type_ != c::VERTEX_SHADER && type_ != c::FRAGMENT_SHADER {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(None);
        }
        Ok(Some(self.gl_create(this, Kind::Shader, type_)))
    }
    fn create_texture(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.gl_create(this, Kind::Texture, 0))
    }
    fn cull_face(&self, _cx: &mut Cx, this: Handle, mode: u32) -> rt::Result<()> {
        self.gl_send(this, Command::CullFace(mode));
        Ok(())
    }
    fn delete_buffer(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<()> {
        self.gl_delete(this, o, Kind::Buffer);
        Ok(())
    }
    fn delete_framebuffer(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<()> {
        self.gl_delete(this, o, Kind::Framebuffer);
        Ok(())
    }
    fn delete_program(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<()> {
        self.gl_delete(this, o, Kind::Program);
        Ok(())
    }
    fn delete_renderbuffer(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<()> {
        self.gl_delete(this, o, Kind::Renderbuffer);
        Ok(())
    }
    fn delete_shader(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<()> {
        self.gl_delete(this, o, Kind::Shader);
        Ok(())
    }
    fn delete_texture(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<()> {
        self.gl_delete(this, o, Kind::Texture);
        Ok(())
    }
    fn depth_func(&self, _cx: &mut Cx, this: Handle, func: u32) -> rt::Result<()> {
        self.gl_send(this, Command::DepthFunc(func));
        Ok(())
    }
    fn depth_mask(&self, _cx: &mut Cx, this: Handle, flag: bool) -> rt::Result<()> {
        self.gl_send(this, Command::DepthMask(flag));
        Ok(())
    }
    fn depth_range(&self, _cx: &mut Cx, this: Handle, near: f32, far: f32) -> rt::Result<()> {
        // WebGL 1.0 §6.12: near greater than far is INVALID_OPERATION.
        if near > far {
            self.gl_error(this, c::INVALID_OPERATION);
            return Ok(());
        }
        self.gl_send(this, Command::DepthRange(near, far));
        Ok(())
    }
    fn detach_shader(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        shader: Handle,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let (Some(p), Some(s)) = (
            self.gl_check(this, program, Kind::Program),
            self.gl_check(this, shader, Kind::Shader),
        ) else {
            return Ok(());
        };
        let had = self
            .gl_obj(program, |o| {
                let n = o.attached.len();
                o.attached.retain(|a| *a != shader);
                n != o.attached.len()
            })
            .unwrap_or(false);
        if !had {
            self.gl_error(this, c::INVALID_OPERATION);
            return Ok(());
        }
        self.gl_send(this, Command::DetachShader(p, s));
        Ok(())
    }
    fn disable(&self, _cx: &mut Cx, this: Handle, cap: u32) -> rt::Result<()> {
        self.gl_send(this, Command::Disable(cap));
        Ok(())
    }
    fn disable_vertex_attrib_array(
        &self,
        _cx: &mut Cx,
        this: Handle,
        index: u32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::DisableVertexAttribArray(index));
        Ok(())
    }
    fn draw_arrays(
        &self,
        _cx: &mut Cx,
        this: Handle,
        mode: u32,
        first: i32,
        count: i32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::DrawArrays(mode, first, count));
        self.gl_drew(this);
        Ok(())
    }
    fn draw_elements(
        &self,
        _cx: &mut Cx,
        this: Handle,
        mode: u32,
        count: i32,
        type_: u32,
        offset: i64,
    ) -> rt::Result<()> {
        if offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(this, Command::DrawElements(mode, count, type_, offset));
        self.gl_drew(this);
        Ok(())
    }
    fn enable(&self, _cx: &mut Cx, this: Handle, cap: u32) -> rt::Result<()> {
        self.gl_send(this, Command::Enable(cap));
        Ok(())
    }
    fn enable_vertex_attrib_array(&self, _cx: &mut Cx, this: Handle, index: u32) -> rt::Result<()> {
        self.gl_send(this, Command::EnableVertexAttribArray(index));
        Ok(())
    }
    fn finish(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.gl_query(this, Query::Finish);
        Ok(())
    }
    fn flush(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.gl_send(this, Command::Flush);
        self.gl_flush();
        Ok(())
    }
    fn framebuffer_renderbuffer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        attachment: u32,
        renderbuffertarget: u32,
        renderbuffer: Option<Handle>,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(id) = self.gl_check_opt(this, renderbuffer, Kind::Renderbuffer) else {
            return Ok(());
        };
        if let Some(fb) = self.gl_ctx(this, |c| c.framebuffer) {
            self.gl_obj(fb, |o| {
                o.attachments.retain(|(a, _)| *a != attachment);
                o.attachments.extend(renderbuffer.map(|r| (attachment, r)));
            });
        }
        self.gl_send(
            this,
            Command::FramebufferRenderbuffer(target, attachment, renderbuffertarget, id),
        );
        Ok(())
    }
    fn framebuffer_texture2_d(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        attachment: u32,
        textarget: u32,
        texture: Option<Handle>,
        level: i32,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(id) = self.gl_check_opt(this, texture, Kind::Texture) else {
            return Ok(());
        };
        if let Some(fb) = self.gl_ctx(this, |c| c.framebuffer) {
            self.gl_obj(fb, |o| {
                o.attachments.retain(|(a, _)| *a != attachment);
                o.attachments.extend(texture.map(|t| (attachment, t)));
            });
        }
        self.gl_send(
            this,
            Command::FramebufferTexture2D(target, attachment, textarget, id, level),
        );
        Ok(())
    }
    fn front_face(&self, _cx: &mut Cx, this: Handle, mode: u32) -> rt::Result<()> {
        self.gl_send(this, Command::FrontFace(mode));
        Ok(())
    }
    fn generate_mipmap(&self, _cx: &mut Cx, this: Handle, target: u32) -> rt::Result<()> {
        self.gl_send(this, Command::GenerateMipmap(target));
        Ok(())
    }
    fn get_active_attrib(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        index: u32,
    ) -> rt::Result<Option<Handle>> {
        let Some(p) = self.gl_check(this, program, Kind::Program) else {
            return Ok(None);
        };
        Ok(match self.gl_query(this, Query::ActiveAttrib(p, index)) {
            Some(Reply::ActiveInfo(Some((size, ty, name)))) => {
                Some(self.alloc(Object::WebGl(Box::new(WebGlObject::ActiveInfo {
                    size,
                    ty,
                    name,
                }))))
            }
            _ => None,
        })
    }
    fn get_active_uniform(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        index: u32,
    ) -> rt::Result<Option<Handle>> {
        let Some(p) = self.gl_check(this, program, Kind::Program) else {
            return Ok(None);
        };
        Ok(match self.gl_query(this, Query::ActiveUniform(p, index)) {
            Some(Reply::ActiveInfo(Some((size, ty, name)))) => {
                Some(self.alloc(Object::WebGl(Box::new(WebGlObject::ActiveInfo {
                    size,
                    ty,
                    name,
                }))))
            }
            _ => None,
        })
    }
    fn get_attached_shaders(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
    ) -> rt::Result<Option<Vec<Handle>>> {
        if self.gl_lost(this) {
            return Ok(None);
        }
        self.gl_check(this, program, Kind::Program)
            .map(|_| {
                self.gl_obj(program, |o| o.attached.clone())
                    .unwrap_or_default()
            })
            .map_or(Ok(None), |v| Ok(Some(v)))
    }
    fn get_attrib_location(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        name: &str,
    ) -> rt::Result<i32> {
        let Some(p) = self.gl_check(this, program, Kind::Program) else {
            return Ok(-1);
        };
        Ok(self
            .gl_query(this, Query::AttribLocation(p, name.to_owned()))
            .map_or(-1, |r| r.int()))
    }
    fn get_buffer_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        pname: u32,
    ) -> rt::Result<JsValue> {
        Ok(self.int_param(this, Query::BufferParameter(target, pname), cx))
    }
    fn get_parameter(&self, cx: &mut Cx, this: Handle, pname: u32) -> rt::Result<JsValue> {
        if self.gl_lost(this) {
            return Ok(null(cx));
        }
        if self.webgl_version_2(this) {
            if let Some(v) = self.gl2_parameter(cx, this, pname) {
                return Ok(v);
            }
        }
        let object = |h: Option<Handle>, cx: &mut Cx| match h {
            Some(h) => js(cx, &h),
            None => null(cx),
        };
        let (unit, ctx) = self.gl_ctx(this, |c| {
            (
                c.textures
                    .get(c.active_unit as usize)
                    .copied()
                    .unwrap_or([None; 4]),
                (
                    c.array_buffer,
                    c.element_buffer,
                    c.program,
                    c.framebuffer,
                    c.renderbuffer,
                    c.flip_y,
                    c.premultiply_alpha,
                    c.colorspace_conversion,
                ),
            )
        });
        let floats = |n, cx: &mut Cx| match self.gl_query(this, Query::Floats(pname, n)) {
            Some(Reply::Floats(v)) => f32_array(cx, &v),
            _ => null(cx),
        };
        let ints = |n, cx: &mut Cx| match self.gl_query(this, Query::Integers(pname, n)) {
            Some(Reply::Ints(v)) => i32_array(cx, &v),
            _ => null(cx),
        };
        Ok(match pname {
            c::ARRAY_BUFFER_BINDING => object(ctx.0, cx),
            c::ELEMENT_ARRAY_BUFFER_BINDING => object(ctx.1, cx),
            c::CURRENT_PROGRAM => object(ctx.2, cx),
            c::FRAMEBUFFER_BINDING => object(ctx.3, cx),
            c::RENDERBUFFER_BINDING => object(ctx.4, cx),
            c::TEXTURE_BINDING_2D => object(unit[0], cx),
            c::TEXTURE_BINDING_CUBE_MAP => object(unit[1], cx),
            c::UNPACK_FLIP_Y_WEBGL => js(cx, &ctx.5),
            c::UNPACK_PREMULTIPLY_ALPHA_WEBGL => js(cx, &ctx.6),
            c::UNPACK_COLORSPACE_CONVERSION_WEBGL => js(cx, &ctx.7),
            // (strings that do not identify the graphics card, as the engine family answers)
            c::VENDOR => js(cx, "WebKit"),
            c::RENDERER => js(cx, "WebKit WebGL"),
            c::VERSION => js(cx, "WebGL 1.0 (OpenGL ES 2.0 Snowlark)"),
            c::SHADING_LANGUAGE_VERSION => {
                js(cx, "WebGL GLSL ES 1.0 (OpenGL ES GLSL ES 1.0 Snowlark)")
            }
            webgl_ext::UNMASKED_VENDOR_WEBGL | webgl_ext::UNMASKED_RENDERER_WEBGL
                if self.gl_ext_enabled(this, "WEBGL_debug_renderer_info") =>
            {
                let s = self.gl_unmasked(this, pname).unwrap_or_default();
                js(cx, s.as_str())
            }
            webgl_ext::VERTEX_ARRAY_BINDING_OES
                if self.gl_ext_enabled(this, "OES_vertex_array_object") =>
            {
                object(self.gl_vertex_array(this), cx)
            }
            // MAX_TEXTURE_MAX_ANISOTROPY_EXT is a float.
            0x84FF => match self.gl_query(this, Query::Floats(pname, 1)) {
                Some(Reply::Floats(v)) if !v.is_empty() => js(cx, &(v[0] as f64)),
                _ => null(cx),
            },
            // ALIASED_LINE_WIDTH_RANGE, ALIASED_POINT_SIZE_RANGE, DEPTH_RANGE
            0x846E | 0x846D | 0x0B70 => floats(2, cx),
            // BLEND_COLOR, COLOR_CLEAR_VALUE
            0x8005 | 0x0C22 => floats(4, cx),
            // MAX_VIEWPORT_DIMS
            0x0D3A => ints(2, cx),
            // SCISSOR_BOX, VIEWPORT
            0x0C10 | 0x0BA2 => ints(4, cx),
            // COMPRESSED_TEXTURE_FORMATS (none without extensions)
            0x86A3 => typed_array(cx, Vec::new(), 4, 2),
            // COLOR_WRITEMASK
            0x0C23 => match self.gl_query(this, Query::Booleans(pname, 4)) {
                Some(Reply::Bools(v)) => bool_array(cx, &v),
                _ => null(cx),
            },
            // BLEND, CULL_FACE, DEPTH_TEST, DEPTH_WRITEMASK, DITHER, POLYGON_OFFSET_FILL,
            // SAMPLE_ALPHA_TO_COVERAGE, SAMPLE_COVERAGE, SAMPLE_COVERAGE_INVERT, SCISSOR_TEST,
            // STENCIL_TEST
            0x0BE2 | 0x0B44 | 0x0B71 | 0x0B72 | 0x0BD0 | 0x8037 | 0x809E | 0x80A0 | 0x80AB
            | 0x0C11 | 0x0B90 => match self.gl_query(this, Query::Booleans(pname, 1)) {
                Some(Reply::Bools(v)) if !v.is_empty() => js(cx, &v[0]),
                _ => null(cx),
            },
            // DEPTH_CLEAR_VALUE, LINE_WIDTH, POLYGON_OFFSET_FACTOR, POLYGON_OFFSET_UNITS,
            // SAMPLE_COVERAGE_VALUE
            0x0B73 | 0x0B21 | 0x8038 | 0x2A00 | 0x80AA => {
                match self.gl_query(this, Query::Floats(pname, 1)) {
                    Some(Reply::Floats(v)) if !v.is_empty() => js(cx, &(v[0] as f64)),
                    _ => null(cx),
                }
            }
            // STENCIL_VALUE_MASK, STENCIL_WRITEMASK and their BACK forms are GLuint.
            0x0B93 | 0x0B98 | 0x8CA4 | 0x8CA5 => {
                match self.gl_query(this, Query::Integers(pname, 1)) {
                    Some(Reply::Ints(v)) if !v.is_empty() => js(cx, &(v[0] as u32)),
                    _ => null(cx),
                }
            }
            _ => match self.gl_query(this, Query::Integers(pname, 1)) {
                Some(Reply::Ints(v)) if !v.is_empty() => js(cx, &v[0]),
                _ => null(cx),
            },
        })
    }
    fn get_error(&self, _cx: &mut Cx, this: Handle) -> rt::Result<u32> {
        let mine = self.gl_ctx(this, |c| (!c.errors.is_empty()).then(|| c.errors.remove(0)));
        if let Some(e) = mine {
            return Ok(e);
        }
        Ok(self
            .gl_query(this, Query::Error)
            .map_or(c::NO_ERROR, |r| r.int() as u32))
    }
    fn get_framebuffer_attachment_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        attachment: u32,
        pname: u32,
    ) -> rt::Result<JsValue> {
        if pname == c::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME {
            let fb = self.gl_ctx(this, |c| {
                if target == webgl2::READ_FRAMEBUFFER && c.attributes.webgl2 {
                    c.gl2.read_framebuffer
                } else {
                    c.framebuffer
                }
            });
            let Some(fb) = fb else {
                self.gl_error(this, c::INVALID_OPERATION);
                return Ok(null(cx));
            };
            let o = self
                .gl_obj(fb, |o| {
                    o.attachments
                        .iter()
                        .find(|(a, _)| *a == attachment)
                        .map(|x| x.1)
                })
                .flatten();
            return Ok(match o {
                Some(h) => js(cx, &h),
                None => null(cx),
            });
        }
        let _ = c::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE;
        Ok(self.int_param(
            this,
            Query::FramebufferAttachmentParameter(target, attachment, pname),
            cx,
        ))
    }
    fn get_program_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        program: Handle,
        pname: u32,
    ) -> rt::Result<JsValue> {
        let Some(p) = self.gl_check(this, program, Kind::Program) else {
            return Ok(null(cx));
        };
        match pname {
            c::DELETE_STATUS => {
                let d = self.gl_obj(program, |o| o.deleted).unwrap_or(false);
                Ok(js(cx, &d))
            }
            c::LINK_STATUS | c::VALIDATE_STATUS => {
                let v = self
                    .gl_query(this, Query::Program(p, pname))
                    .map(|r| r.int());
                Ok(match v {
                    Some(v) => js(cx, &(v != 0)),
                    None => null(cx),
                })
            }
            c::ATTACHED_SHADERS | c::ACTIVE_ATTRIBUTES | c::ACTIVE_UNIFORMS => {
                Ok(self.int_param(this, Query::Program(p, pname), cx))
            }
            webgl_ext::COMPLETION_STATUS_KHR
                if self.gl_ext_enabled(this, "KHR_parallel_shader_compile") =>
            {
                let v = self.gl_query(this, Query::Program(p, pname));
                Ok(js(cx, &(v.map_or(1, |r| r.int()) != 0)))
            }
            // WebGL 2.0: TRANSFORM_FEEDBACK_BUFFER_MODE, TRANSFORM_FEEDBACK_VARYINGS,
            // ACTIVE_UNIFORM_BLOCKS
            0x8C7F | 0x8C83 | 0x8A36 if self.webgl_version_2(this) => {
                Ok(self.int_param(this, Query::Program(p, pname), cx))
            }
            _ => {
                self.gl_error(this, c::INVALID_ENUM);
                Ok(null(cx))
            }
        }
    }
    fn get_program_info_log(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
    ) -> rt::Result<Option<DomStr<'_>>> {
        let Some(p) = self.gl_check(this, program, Kind::Program) else {
            return Ok(None);
        };
        Ok(match self.gl_query(this, Query::ProgramInfoLog(p)) {
            Some(Reply::String(s)) => Some(DomStr::Owned(s.unwrap_or_default())),
            _ => None,
        })
    }
    fn get_renderbuffer_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        pname: u32,
    ) -> rt::Result<JsValue> {
        Ok(self.int_param(this, Query::RenderbufferParameter(target, pname), cx))
    }
    fn get_shader_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        shader: Handle,
        pname: u32,
    ) -> rt::Result<JsValue> {
        let Some(s) = self.gl_check(this, shader, Kind::Shader) else {
            return Ok(null(cx));
        };
        match pname {
            c::SHADER_TYPE => {
                let t = self.gl_obj(shader, |o| o.shader_type).unwrap_or(0);
                Ok(js(cx, &t))
            }
            c::DELETE_STATUS => {
                let d = self.gl_obj(shader, |o| o.deleted).unwrap_or(false);
                Ok(js(cx, &d))
            }
            c::COMPILE_STATUS => {
                let v = self
                    .gl_query(this, Query::Shader(s, pname))
                    .map(|r| r.int());
                Ok(match v {
                    Some(v) => js(cx, &(v != 0)),
                    None => null(cx),
                })
            }
            webgl_ext::COMPLETION_STATUS_KHR
                if self.gl_ext_enabled(this, "KHR_parallel_shader_compile") =>
            {
                let v = self.gl_query(this, Query::Shader(s, pname));
                Ok(js(cx, &(v.map_or(1, |r| r.int()) != 0)))
            }
            _ => {
                self.gl_error(this, c::INVALID_ENUM);
                Ok(null(cx))
            }
        }
    }
    fn get_shader_precision_format(
        &self,
        _cx: &mut Cx,
        this: Handle,
        shadertype: u32,
        precisiontype: u32,
    ) -> rt::Result<Option<Handle>> {
        if (shadertype != c::VERTEX_SHADER && shadertype != c::FRAGMENT_SHADER)
            || !(c::LOW_FLOAT..=c::HIGH_INT).contains(&precisiontype)
        {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(None);
        }
        Ok(
            match self.gl_query(
                this,
                Query::ShaderPrecisionFormat(shadertype, precisiontype),
            ) {
                Some(Reply::Precision(p)) => {
                    Some(self.alloc(Object::WebGl(Box::new(WebGlObject::Precision(p)))))
                }
                _ => None,
            },
        )
    }
    fn get_shader_info_log(
        &self,
        _cx: &mut Cx,
        this: Handle,
        shader: Handle,
    ) -> rt::Result<Option<DomStr<'_>>> {
        let Some(s) = self.gl_check(this, shader, Kind::Shader) else {
            return Ok(None);
        };
        Ok(match self.gl_query(this, Query::ShaderInfoLog(s)) {
            Some(Reply::String(s)) => Some(DomStr::Owned(s.unwrap_or_default())),
            _ => None,
        })
    }
    fn get_shader_source(
        &self,
        _cx: &mut Cx,
        this: Handle,
        shader: Handle,
    ) -> rt::Result<Option<DomStr<'_>>> {
        if self.gl_lost(this) {
            return Ok(None);
        }
        self.gl_check(this, shader, Kind::Shader)
            .map(|_| {
                self.gl_obj(shader, |o| o.source.clone())
                    .unwrap_or_default()
            })
            .map_or(Ok(None), |s| Ok(Some(DomStr::Owned(s))))
    }
    fn get_tex_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        pname: u32,
    ) -> rt::Result<JsValue> {
        Ok(self.int_param(this, Query::TexParameter(target, pname), cx))
    }
    fn get_uniform(
        &self,
        cx: &mut Cx,
        this: Handle,
        program: Handle,
        location: Handle,
    ) -> rt::Result<JsValue> {
        let Some(p) = self.gl_check(this, program, Kind::Program) else {
            return Ok(null(cx));
        };
        let loc = match &*self.obj(location) {
            Object::WebGl(o) => match &**o {
                WebGlObject::UniformLocation {
                    context,
                    program: lp,
                    links,
                    location,
                    ty,
                } => Some((*context, *lp, *links, *location, *ty)),
                _ => None,
            },
            _ => None,
        };
        let Some((context, lp, links, location, ty)) = loc else {
            return Ok(null(cx));
        };
        let program_links = self.gl_obj(program, |o| o.links).unwrap_or(0);
        if context != this || lp != program || links != program_links {
            self.gl_error(this, c::INVALID_OPERATION);
            return Ok(null(cx));
        }
        let (n, kind) = uniform_shape(ty);
        if n == 0 {
            return Ok(null(cx));
        }
        if kind == 3 {
            // (unsigned: a Number, or a Uint32Array)
            return Ok(match self.gl_query(this, Query::UniformU(p, location, n)) {
                Some(Reply::Ints(v)) if n == 1 => js(cx, &(v.first().copied().unwrap_or(0) as u32)),
                Some(Reply::Ints(v)) => u32_array(cx, &v),
                _ => null(cx),
            });
        }
        Ok(
            match self.gl_query(this, Query::Uniform(p, location, n, kind == 0)) {
                Some(Reply::Floats(v)) if n == 1 => {
                    js(cx, &(v.first().copied().unwrap_or(0.0) as f64))
                }
                Some(Reply::Floats(v)) => f32_array(cx, &v),
                Some(Reply::Ints(v)) if kind == 2 && n == 1 => {
                    js(cx, &(v.first().copied().unwrap_or(0) != 0))
                }
                Some(Reply::Ints(v)) if kind == 2 => {
                    bool_array(cx, &v.iter().map(|x| *x != 0).collect::<Vec<_>>())
                }
                Some(Reply::Ints(v)) if n == 1 => js(cx, &v.first().copied().unwrap_or(0)),
                Some(Reply::Ints(v)) => i32_array(cx, &v),
                _ => null(cx),
            },
        )
    }
    fn get_uniform_location(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        name: &str,
    ) -> rt::Result<Option<Handle>> {
        let Some(p) = self.gl_check(this, program, Kind::Program) else {
            return Ok(None);
        };
        let location = self
            .gl_query(this, Query::UniformLocation(p, name.to_owned()))
            .map_or(-1, |r| r.int());
        if location < 0 {
            return Ok(None);
        }
        // The uniform's type, for getUniform: the active uniform whose name is this one's array
        // base.
        let base = name.split('[').next().unwrap_or(name);
        let count = self
            .gl_query(this, Query::Program(p, c::ACTIVE_UNIFORMS))
            .map_or(0, |r| r.int())
            .clamp(0, 4096) as u32;
        let mut ty = 0;
        for i in 0..count {
            if let Some(Reply::ActiveInfo(Some((_, t, n)))) =
                self.gl_query(this, Query::ActiveUniform(p, i))
            {
                if n.split('[').next() == Some(base) {
                    ty = t;
                    break;
                }
            }
        }
        let links = self.gl_obj(program, |o| o.links).unwrap_or(0);
        Ok(Some(self.alloc(Object::WebGl(Box::new(
            WebGlObject::UniformLocation {
                context: this,
                program,
                links,
                location,
                ty,
            },
        )))))
    }
    fn get_vertex_attrib(
        &self,
        cx: &mut Cx,
        this: Handle,
        index: u32,
        pname: u32,
    ) -> rt::Result<JsValue> {
        if self.gl_lost(this) {
            return Ok(null(cx));
        }
        match pname {
            c::VERTEX_ATTRIB_ARRAY_BUFFER_BINDING => {
                let b = self.gl_ctx(this, |c| c.attrib_buffers.get(index as usize).copied());
                match b {
                    Some(Some(h)) => Ok(js(cx, &h)),
                    Some(None) => Ok(null(cx)),
                    None => {
                        self.gl_error(this, c::INVALID_VALUE);
                        Ok(null(cx))
                    }
                }
            }
            c::CURRENT_VERTEX_ATTRIB => {
                match self.gl_query(this, Query::VertexAttrib(index, pname, 4)) {
                    Some(Reply::Floats(v)) if v.len() == 4 => Ok(f32_array(cx, &v)),
                    _ => Ok(null(cx)),
                }
            }
            c::VERTEX_ATTRIB_ARRAY_ENABLED | c::VERTEX_ATTRIB_ARRAY_NORMALIZED => {
                match self.gl_query(this, Query::VertexAttrib(index, pname, 1)) {
                    Some(Reply::Floats(v)) if !v.is_empty() => Ok(js(cx, &(v[0] != 0.0))),
                    _ => Ok(null(cx)),
                }
            }
            _ => match self.gl_query(this, Query::VertexAttrib(index, pname, 1)) {
                Some(Reply::Floats(v)) if !v.is_empty() => Ok(js(cx, &(v[0] as i32))),
                _ => Ok(null(cx)),
            },
        }
    }
    fn get_vertex_attrib_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        index: u32,
        pname: u32,
    ) -> rt::Result<i64> {
        if pname != 0x8645 {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(0);
        }
        Ok(self
            .gl_query(this, Query::VertexAttribOffset(index))
            .map_or(0, |r| r.int() as i64))
    }
    fn hint(&self, _cx: &mut Cx, this: Handle, target: u32, mode: u32) -> rt::Result<()> {
        self.gl_send(this, Command::Hint(target, mode));
        Ok(())
    }
    fn is_buffer(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<bool> {
        Ok(self.gl_is(this, o, Kind::Buffer))
    }
    fn is_enabled(&self, _cx: &mut Cx, this: Handle, cap: u32) -> rt::Result<bool> {
        Ok(self
            .gl_query(this, Query::IsEnabled(cap))
            .is_some_and(|r| r.int() != 0))
    }
    fn is_framebuffer(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<bool> {
        Ok(self.gl_is(this, o, Kind::Framebuffer))
    }
    fn is_program(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<bool> {
        Ok(self.gl_is(this, o, Kind::Program))
    }
    fn is_renderbuffer(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<bool> {
        Ok(self.gl_is(this, o, Kind::Renderbuffer))
    }
    fn is_shader(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<bool> {
        Ok(self.gl_is(this, o, Kind::Shader))
    }
    fn is_texture(&self, _cx: &mut Cx, this: Handle, o: Option<Handle>) -> rt::Result<bool> {
        Ok(self.gl_is(this, o, Kind::Texture))
    }
    fn line_width(&self, _cx: &mut Cx, this: Handle, width: f32) -> rt::Result<()> {
        self.gl_send(this, Command::LineWidth(width));
        Ok(())
    }
    fn link_program(&self, _cx: &mut Cx, this: Handle, program: Handle) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        if let Some(p) = self.gl_check(this, program, Kind::Program) {
            self.gl_obj(program, |o| o.links += 1);
            self.gl_send(this, Command::LinkProgram(p));
        }
        Ok(())
    }
    fn pixel_storei(&self, _cx: &mut Cx, this: Handle, pname: u32, param: i32) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        match pname {
            c::UNPACK_FLIP_Y_WEBGL => self.gl_ctx(this, |c| c.flip_y = param != 0),
            c::UNPACK_PREMULTIPLY_ALPHA_WEBGL => {
                self.gl_ctx(this, |c| c.premultiply_alpha = param != 0)
            }
            c::UNPACK_COLORSPACE_CONVERSION_WEBGL => {
                if param as u32 == c::BROWSER_DEFAULT_WEBGL || param == 0 {
                    self.gl_ctx(this, |c| c.colorspace_conversion = param as u32)
                } else {
                    self.gl_error(this, c::INVALID_VALUE)
                }
            }
            _ => {
                if [1, 2, 4, 8].contains(&param) {
                    if pname == c::PACK_ALIGNMENT {
                        self.gl_ctx(this, |c| c.pack_alignment = param);
                    } else if pname == c::UNPACK_ALIGNMENT {
                        self.gl_ctx(this, |c| c.unpack_alignment = param);
                    }
                }
                self.gl_send(this, Command::PixelStorei(pname, param));
            }
        }
        Ok(())
    }
    fn polygon_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        factor: f32,
        units: f32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::PolygonOffset(factor, units));
        Ok(())
    }
    fn renderbuffer_storage(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        internalformat: u32,
        width: i32,
        height: i32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::RenderbufferStorage(target, internalformat, width, height),
        );
        Ok(())
    }
    fn sample_coverage(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: f32,
        invert: bool,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::SampleCoverage(value, invert));
        Ok(())
    }
    fn scissor(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::Scissor(x, y, w, h));
        Ok(())
    }
    fn shader_source(
        &self,
        _cx: &mut Cx,
        this: Handle,
        shader: Handle,
        source: &str,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        if let Some(s) = self.gl_check(this, shader, Kind::Shader) {
            self.gl_obj(shader, |o| o.source = source.to_owned());
            self.gl_send(this, Command::ShaderSource(s, source.to_owned()));
        }
        Ok(())
    }
    fn stencil_func(
        &self,
        _cx: &mut Cx,
        this: Handle,
        func: u32,
        ref_: i32,
        mask: u32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::StencilFuncSeparate(c::FRONT_AND_BACK, func, ref_, mask),
        );
        Ok(())
    }
    fn stencil_func_separate(
        &self,
        _cx: &mut Cx,
        this: Handle,
        face: u32,
        func: u32,
        ref_: i32,
        mask: u32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::StencilFuncSeparate(face, func, ref_, mask));
        Ok(())
    }
    fn stencil_mask(&self, _cx: &mut Cx, this: Handle, mask: u32) -> rt::Result<()> {
        self.gl_send(this, Command::StencilMaskSeparate(c::FRONT_AND_BACK, mask));
        Ok(())
    }
    fn stencil_mask_separate(
        &self,
        _cx: &mut Cx,
        this: Handle,
        face: u32,
        mask: u32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::StencilMaskSeparate(face, mask));
        Ok(())
    }
    fn stencil_op(
        &self,
        _cx: &mut Cx,
        this: Handle,
        fail: u32,
        zfail: u32,
        zpass: u32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::StencilOpSeparate(c::FRONT_AND_BACK, fail, zfail, zpass),
        );
        Ok(())
    }
    fn stencil_op_separate(
        &self,
        _cx: &mut Cx,
        this: Handle,
        face: u32,
        fail: u32,
        zfail: u32,
        zpass: u32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::StencilOpSeparate(face, fail, zfail, zpass));
        Ok(())
    }
    fn tex_parameterf(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        pname: u32,
        param: f32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::TexParameterf(target, pname, param));
        Ok(())
    }
    fn tex_parameteri(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        pname: u32,
        param: i32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::TexParameteri(target, pname, param));
        Ok(())
    }
    fn uniform1f(&self, _cx: &mut Cx, this: Handle, l: Option<Handle>, x: f32) -> rt::Result<()> {
        self.uniform_v(this, l, 1, vec![x]);
        Ok(())
    }
    fn uniform2f(
        &self,
        _cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        x: f32,
        y: f32,
    ) -> rt::Result<()> {
        self.uniform_v(this, l, 2, vec![x, y]);
        Ok(())
    }
    fn uniform3f(
        &self,
        _cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        x: f32,
        y: f32,
        z: f32,
    ) -> rt::Result<()> {
        self.uniform_v(this, l, 3, vec![x, y, z]);
        Ok(())
    }
    fn uniform4f(
        &self,
        _cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        x: f32,
        y: f32,
        z: f32,
        w: f32,
    ) -> rt::Result<()> {
        self.uniform_v(this, l, 4, vec![x, y, z, w]);
        Ok(())
    }
    fn uniform1i(&self, _cx: &mut Cx, this: Handle, l: Option<Handle>, x: i32) -> rt::Result<()> {
        self.uniform_iv(this, l, 1, vec![x]);
        Ok(())
    }
    fn uniform2i(
        &self,
        _cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        x: i32,
        y: i32,
    ) -> rt::Result<()> {
        self.uniform_iv(this, l, 2, vec![x, y]);
        Ok(())
    }
    fn uniform3i(
        &self,
        _cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        x: i32,
        y: i32,
        z: i32,
    ) -> rt::Result<()> {
        self.uniform_iv(this, l, 3, vec![x, y, z]);
        Ok(())
    }
    fn uniform4i(
        &self,
        _cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        x: i32,
        y: i32,
        z: i32,
        w: i32,
    ) -> rt::Result<()> {
        self.uniform_iv(this, l, 4, vec![x, y, z, w]);
        Ok(())
    }
    fn use_program(&self, _cx: &mut Cx, this: Handle, program: Option<Handle>) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(id) = self.gl_check_opt(this, program, Kind::Program) else {
            return Ok(());
        };
        self.gl_ctx(this, |c| c.program = program);
        self.gl_send(this, Command::UseProgram(id));
        Ok(())
    }
    fn validate_program(&self, _cx: &mut Cx, this: Handle, program: Handle) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        if let Some(p) = self.gl_check(this, program, Kind::Program) {
            self.gl_send(this, Command::ValidateProgram(p));
        }
        Ok(())
    }
    fn vertex_attrib1f(&self, _cx: &mut Cx, this: Handle, i: u32, x: f32) -> rt::Result<()> {
        self.vertex_attrib_v(this, i, 1, vec![x]);
        Ok(())
    }
    fn vertex_attrib2f(
        &self,
        _cx: &mut Cx,
        this: Handle,
        i: u32,
        x: f32,
        y: f32,
    ) -> rt::Result<()> {
        self.vertex_attrib_v(this, i, 2, vec![x, y]);
        Ok(())
    }
    fn vertex_attrib3f(
        &self,
        _cx: &mut Cx,
        this: Handle,
        i: u32,
        x: f32,
        y: f32,
        z: f32,
    ) -> rt::Result<()> {
        self.vertex_attrib_v(this, i, 3, vec![x, y, z]);
        Ok(())
    }
    fn vertex_attrib4f(
        &self,
        _cx: &mut Cx,
        this: Handle,
        i: u32,
        x: f32,
        y: f32,
        z: f32,
        w: f32,
    ) -> rt::Result<()> {
        self.vertex_attrib_v(this, i, 4, vec![x, y, z, w]);
        Ok(())
    }
    fn vertex_attrib1fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        i: u32,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.vertex_attrib_v(this, i, 1, v);
        Ok(())
    }
    fn vertex_attrib2fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        i: u32,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.vertex_attrib_v(this, i, 2, v);
        Ok(())
    }
    fn vertex_attrib3fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        i: u32,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.vertex_attrib_v(this, i, 3, v);
        Ok(())
    }
    fn vertex_attrib4fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        i: u32,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.vertex_attrib_v(this, i, 4, v);
        Ok(())
    }
    fn vertex_attrib_pointer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        index: u32,
        size: i32,
        type_: u32,
        normalized: bool,
        stride: i32,
        offset: i64,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        if offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        // (the buffer it reads from is the one bound to ARRAY_BUFFER now; with none, ANGLE
        // refuses the call in WebGL mode)
        self.gl_ctx(this, |c| {
            let b = c.array_buffer;
            if b.is_some() {
                if let Some(slot) = c.attrib_buffers.get_mut(index as usize) {
                    *slot = b;
                }
            }
        });
        self.gl_send(
            this,
            Command::VertexAttribPointer(index, size, type_, normalized, stride, offset),
        );
        Ok(())
    }
    fn viewport(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::Viewport(x, y, w, h));
        Ok(())
    }
}

/// The bytes per pixel of a `readPixels` or `texImage2D` format and type (0: not a valid pair).
fn bytes_per_pixel(format: u32, ty: u32) -> usize {
    let channels = match format {
        c::RGBA => 4,
        c::RGB => 3,
        c::ALPHA | 0x1909 => 1,
        0x190A => 2,
        _ => return 0,
    };
    match ty {
        c::UNSIGNED_BYTE => channels,
        c::FLOAT => channels * 4,
        c::UNSIGNED_SHORT_5_6_5 | c::UNSIGNED_SHORT_4_4_4_4 | c::UNSIGNED_SHORT_5_5_5_1 => 2,
        _ => 0,
    }
}

impl WebGLRenderingContextOverloadsImpl for Engine {
    fn buffer_data_target_size_usage(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        size: i64,
        usage: u32,
    ) -> rt::Result<()> {
        if size < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(this, Command::BufferData(target, size, None, usage));
        Ok(())
    }
    fn buffer_data_target_data_usage(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        data: Option<BufferSource>,
        usage: u32,
    ) -> rt::Result<()> {
        let Some(data) = data else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        let bytes = view_bytes(cx, buffer_source_object(&data));
        self.gl_send(this, Command::BufferData(target, 0, Some(bytes), usage));
        Ok(())
    }
    fn buffer_sub_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        offset: i64,
        data: BufferSource,
    ) -> rt::Result<()> {
        if offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        let bytes = view_bytes(cx, buffer_source_object(&data));
        self.gl_send(this, Command::BufferSubData(target, offset, bytes));
        Ok(())
    }
    fn compressed_tex_image2_d(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: u32,
        width: i32,
        height: i32,
        border: i32,
        data: View,
    ) -> rt::Result<()> {
        let bytes = view_bytes(cx, view_object!(&data, Int8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView));
        self.gl_send(
            this,
            Command::CompressedTexImage2D(
                target,
                level,
                internalformat,
                width,
                height,
                border,
                bytes,
            ),
        );
        Ok(())
    }
    fn compressed_tex_sub_image2_d(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        data: View,
    ) -> rt::Result<()> {
        let bytes = view_bytes(cx, view_object!(&data, Int8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView));
        self.gl_send(
            this,
            Command::CompressedTexSubImage2D(
                target, level, xoffset, yoffset, width, height, format, bytes,
            ),
        );
        Ok(())
    }
    /// WebGL 1.0 §5.14.12 `readPixels`.
    fn read_pixels(
        &self,
        cx: &mut Cx,
        this: Handle,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        pixels: Option<View>,
    ) -> rt::Result<()> {
        use g::Int8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView as V;
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(pixels) = pixels else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        // The array's type must be the one the type names.
        let ok_type = matches!(
            (&pixels, type_),
            (V::Uint8Array(_), c::UNSIGNED_BYTE)
                | (
                    V::Uint16Array(_),
                    c::UNSIGNED_SHORT_5_6_5 | c::UNSIGNED_SHORT_4_4_4_4 | c::UNSIGNED_SHORT_5_5_5_1,
                )
                | (V::Float32Array(_), c::FLOAT)
        );
        if !ok_type {
            self.gl_error(this, c::INVALID_OPERATION);
            return Ok(());
        }
        if width < 0 || height < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        let bpp = bytes_per_pixel(format, type_);
        let align = self.gl_ctx(this, |c| c.pack_alignment.max(1) as usize);
        let row = (width as usize * bpp).div_ceil(align) * align;
        let needed = if height == 0 || bpp == 0 {
            0
        } else {
            row * (height as usize - 1) + width as usize * bpp
        };
        let o = view_object!(&pixels, Int8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView).clone();
        let s = cx.scope();
        let local = v8::Local::new(s, o.global());
        let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(local) else {
            return Ok(());
        };
        if view.byte_length() < needed {
            self.gl_error(this, c::INVALID_OPERATION);
            return Ok(());
        }
        let Some(Reply::Bytes(bytes)) = self.gl_query(
            this,
            Query::ReadPixels(x, y, width, height, format, type_, needed as u32),
        ) else {
            return Ok(());
        };
        let s = cx.scope();
        let offset = view.byte_offset();
        if let Some(buffer) = view.buffer(s) {
            let store = buffer.get_backing_store();
            for (i, b) in bytes.into_iter().take(needed).enumerate() {
                if let Some(cell) = store.get(offset + i) {
                    cell.set(b);
                }
            }
        }
        Ok(())
    }
    fn tex_image2_d_target_level_internalformat_width_height_border_format_type_pixels(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        type_: u32,
        pixels: Option<View>,
    ) -> rt::Result<()> {
        let mut bytes = pixels.map(|p| view_bytes(cx, view_object!(&p, Int8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView)));
        if let Some(b) = &mut bytes {
            self.gl_unpack_view(this, b, width, height, format, type_);
        }
        self.gl_send(
            this,
            Command::TexImage2D(
                target,
                level,
                internalformat,
                width,
                height,
                border,
                format,
                type_,
                bytes,
            ),
        );
        Ok(())
    }
    fn tex_image2_d_target_level_internalformat_format_type_source(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: i32,
        format: u32,
        type_: u32,
        source: g::ImageBitmapOrImageDataOrHTMLImageElementOrHTMLCanvasElementOrHTMLVideoElementOrOffscreenCanvas,
    ) -> rt::Result<()> {
        self.tex_from_source(cx, this, &source, format, type_, |w, h, bytes| {
            Command::TexImage2D(target, level, internalformat, w, h, 0, format, type_, bytes)
        })
    }
    fn tex_sub_image2_d_target_level_xoffset_yoffset_width_height_format_type_pixels(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        pixels: Option<View>,
    ) -> rt::Result<()> {
        let Some(pixels) = pixels else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        let mut bytes = view_bytes(cx, view_object!(&pixels, Int8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView));
        self.gl_unpack_view(this, &mut bytes, width, height, format, type_);
        self.gl_send(
            this,
            Command::TexSubImage2D(
                target, level, xoffset, yoffset, width, height, format, type_, bytes,
            ),
        );
        Ok(())
    }
    fn tex_sub_image2_d_target_level_xoffset_yoffset_format_type_source(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        format: u32,
        type_: u32,
        source: g::ImageBitmapOrImageDataOrHTMLImageElementOrHTMLCanvasElementOrHTMLVideoElementOrOffscreenCanvas,
    ) -> rt::Result<()> {
        self.tex_from_source(cx, this, &source, format, type_, |w, h, bytes| {
            Command::TexSubImage2D(
                target,
                level,
                xoffset,
                yoffset,
                w,
                h,
                format,
                type_,
                bytes.unwrap_or_default(),
            )
        })
    }
    fn uniform1fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.uniform_v(this, l, 1, v);
        Ok(())
    }
    fn uniform2fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.uniform_v(this, l, 2, v);
        Ok(())
    }
    fn uniform3fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.uniform_v(this, l, 3, v);
        Ok(())
    }
    fn uniform4fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.uniform_v(this, l, 4, v);
        Ok(())
    }
    fn uniform1iv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v: g::Int32ArrayOrLongSequence,
    ) -> rt::Result<()> {
        let v = ints(cx, &v);
        self.uniform_iv(this, l, 1, v);
        Ok(())
    }
    fn uniform2iv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v: g::Int32ArrayOrLongSequence,
    ) -> rt::Result<()> {
        let v = ints(cx, &v);
        self.uniform_iv(this, l, 2, v);
        Ok(())
    }
    fn uniform3iv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v: g::Int32ArrayOrLongSequence,
    ) -> rt::Result<()> {
        let v = ints(cx, &v);
        self.uniform_iv(this, l, 3, v);
        Ok(())
    }
    fn uniform4iv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v: g::Int32ArrayOrLongSequence,
    ) -> rt::Result<()> {
        let v = ints(cx, &v);
        self.uniform_iv(this, l, 4, v);
        Ok(())
    }
    fn uniform_matrix2fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.uniform_matrix(this, l, 2, t, v);
        Ok(())
    }
    fn uniform_matrix3fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.uniform_matrix(this, l, 3, t, v);
        Ok(())
    }
    fn uniform_matrix4fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        v: g::Float32ArrayOrUnrestrictedFloatSequence,
    ) -> rt::Result<()> {
        let v = floats(cx, &v);
        self.uniform_matrix(this, l, 4, t, v);
        Ok(())
    }
}

impl snowlark_bindings::generated::WebGLRenderingContextImpl for Engine {}
impl snowlark_bindings::generated::WebGLBufferImpl for Engine {}
impl snowlark_bindings::generated::WebGLFramebufferImpl for Engine {}
impl snowlark_bindings::generated::WebGLProgramImpl for Engine {}
impl snowlark_bindings::generated::WebGLRenderbufferImpl for Engine {}
impl snowlark_bindings::generated::WebGLShaderImpl for Engine {}
impl snowlark_bindings::generated::WebGLTextureImpl for Engine {}
impl snowlark_bindings::generated::WebGLUniformLocationImpl for Engine {}

impl WebGLObjectImpl for Engine {
    fn label(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.gl_obj(this, |o| o.label.clone()).unwrap_or_default())
    }
    fn set_label(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.gl_obj(this, |o| o.label = value.to_owned());
        Ok(())
    }
}

impl WebGLActiveInfoImpl for Engine {
    fn size(&self, this: Handle) -> i32 {
        match &*self.obj(this) {
            Object::WebGl(o) => match &**o {
                WebGlObject::ActiveInfo { size, .. } => *size,
                _ => 0,
            },
            _ => 0,
        }
    }
    fn type_(&self, this: Handle) -> u32 {
        match &*self.obj(this) {
            Object::WebGl(o) => match &**o {
                WebGlObject::ActiveInfo { ty, .. } => *ty,
                _ => 0,
            },
            _ => 0,
        }
    }
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(match &*self.obj(this) {
            Object::WebGl(o) => match &**o {
                WebGlObject::ActiveInfo { name, .. } => name.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        })
    }
}

impl WebGLShaderPrecisionFormatImpl for Engine {
    fn range_min(&self, this: Handle) -> i32 {
        self.precision_of(this)[0]
    }
    fn range_max(&self, this: Handle) -> i32 {
        self.precision_of(this)[1]
    }
    fn precision(&self, this: Handle) -> i32 {
        self.precision_of(this)[2]
    }
}

impl Engine {
    fn precision_of(&self, this: Handle) -> [i32; 3] {
        match &*self.obj(this) {
            Object::WebGl(o) => match &**o {
                WebGlObject::Precision(p) => *p,
                _ => [0; 3],
            },
            _ => [0; 3],
        }
    }
}

impl WebGLContextEventImpl for Engine {
    fn status_message(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::WebGlContext(s) => s.clone(),
            _ => String::new(),
        }))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::WebGLContextEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::WebGLContextEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::WebGlContext(init.status_message),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three rows of one pixel each, bottom first as the service reads them back.
    fn readback() -> snowlark_gl::Readback {
        snowlark_gl::Readback {
            width: 1,
            height: 3,
            rgba: vec![10, 20, 30, 128, 40, 50, 60, 255, 200, 100, 50, 0],
        }
    }

    #[test]
    fn a_drawing_buffer_becomes_a_frame_top_row_first_in_bgra() {
        let premultiplied = to_frame(readback(), &Attributes::default());
        assert_eq!(
            premultiplied.pixels,
            [50, 100, 200, 0, 60, 50, 40, 255, 30, 20, 10, 128]
        );
        let opaque = Attributes {
            alpha: false,
            ..Attributes::default()
        };
        assert_eq!(
            to_frame(readback(), &opaque).pixels,
            [50, 100, 200, 255, 60, 50, 40, 255, 30, 20, 10, 255]
        );
        let straight = Attributes {
            premultiplied_alpha: false,
            ..Attributes::default()
        };
        // (each color component times alpha / 255, rounded)
        assert_eq!(
            to_frame(readback(), &straight).pixels,
            [0, 0, 0, 0, 60, 50, 40, 255, 15, 10, 5, 128]
        );
    }
}
