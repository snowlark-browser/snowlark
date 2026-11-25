// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL 2.0 (Khronos): `WebGL2RenderingContext`'s own members, over an OpenGL ES 3.0 context of
//! the service. As for WebGL 1.0, this side checks what WebGL checks before a call reaches OpenGL
//! ES (objects of this context, ranges into typed arrays, uploads and readbacks against the bound
//! pixel buffers) and keeps the state script reads back as objects; ANGLE's WebGL compatibility
//! mode validates every call again. The members WebGL 2.0 shares with WebGL 1.0 are in the parent
//! module.

use snowlark_bindings::generated::{
    self as g, WebGL2RenderingContextBaseImpl, WebGL2RenderingContextOverloadsImpl,
    WebGLRenderingContextOverloadsImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle, JsObject, JsValue};
use snowlark_gl::protocol::Limits;
use snowlark_gl::{Command, Kind, Pixels, Query, Reply};

use super::super::{Engine, Object};
use super::{
    c, floats, i32_array, ints, js, null, u32_array, view_bytes, BufferSource, ContextObj, View,
    WebGlObject,
};

pub(super) const READ_FRAMEBUFFER: u32 = 0x8CA8;
pub(super) const DRAW_FRAMEBUFFER: u32 = 0x8CA9;
pub(super) const TEXTURE_3D: u32 = 0x806F;
pub(super) const TEXTURE_2D_ARRAY: u32 = 0x8C1A;
const PIXEL_PACK_BUFFER: u32 = 0x88EB;
const PIXEL_UNPACK_BUFFER: u32 = 0x88EC;
const TRANSFORM_FEEDBACK_BUFFER: u32 = 0x8C8E;
const UNIFORM_BUFFER: u32 = 0x8A11;
const COLOR: u32 = 0x1800;
const DEPTH: u32 = 0x1801;
const STENCIL: u32 = 0x1802;
const SYNC_GPU_COMMANDS_COMPLETE: u32 = 0x9117;
const SYNC_FLUSH_COMMANDS_BIT: u32 = 0x1;
const TIMEOUT_EXPIRED: u32 = 0x911B;
const WAIT_FAILED: u32 = 0x911D;
const UNSIGNALED: i32 = 0x9118;
const CURRENT_QUERY: u32 = 0x8865;
const QUERY_RESULT: u32 = 0x8866;
const QUERY_RESULT_AVAILABLE: u32 = 0x8867;

/// The slot of an indexed or generic buffer binding WebGL 2.0 adds: COPY_READ_BUFFER,
/// COPY_WRITE_BUFFER, PIXEL_PACK_BUFFER, PIXEL_UNPACK_BUFFER, TRANSFORM_FEEDBACK_BUFFER,
/// UNIFORM_BUFFER.
pub(super) fn buffer_slot(target: u32) -> Option<usize> {
    match target {
        0x8F36 => Some(0),
        0x8F37 => Some(1),
        PIXEL_PACK_BUFFER => Some(2),
        PIXEL_UNPACK_BUFFER => Some(3),
        TRANSFORM_FEEDBACK_BUFFER => Some(4),
        UNIFORM_BUFFER => Some(5),
        _ => None,
    }
}

/// The query targets: ANY_SAMPLES_PASSED, ANY_SAMPLES_PASSED_CONSERVATIVE,
/// TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN.
fn query_slot(target: u32) -> Option<usize> {
    match target {
        0x8C2F => Some(0),
        0x8D6A => Some(1),
        0x8C88 => Some(2),
        _ => None,
    }
}

/// WebGL 2.0's bindings of a context, as script reads them back.
#[derive(Default)]
pub struct Gl2State {
    /// By [`buffer_slot`].
    pub(super) buffers: [Option<Handle>; 6],
    pub(super) read_framebuffer: Option<Handle>,
    vertex_array: Option<Handle>,
    /// The default vertex array's element array buffer and attribute buffers while another one is
    /// bound.
    default_vao: Option<(Option<Handle>, Vec<Option<Handle>>)>,
    /// Per texture unit.
    samplers: Vec<Option<Handle>>,
    transform_feedback: Option<Handle>,
    /// The active query of each target, by [`query_slot`].
    queries: [Option<Handle>; 3],
    uniform_bindings: Vec<Option<Handle>>,
    feedback_bindings: Vec<Option<Handle>>,
}

impl Gl2State {
    pub(super) fn new(limits: &Limits, units: usize) -> Gl2State {
        let n = |v: i32| v.clamp(0, 256) as usize;
        Gl2State {
            samplers: vec![None; units],
            uniform_bindings: vec![None; n(limits.max_uniform_buffer_bindings)],
            feedback_bindings: vec![None; n(limits.max_transform_feedback_separate_attribs)],
            ..Gl2State::default()
        }
    }

    pub(super) fn gc_refs(&self, objects: &mut Vec<Handle>) {
        objects.extend(self.buffers.iter().flatten());
        objects.extend(
            [
                self.read_framebuffer,
                self.vertex_array,
                self.transform_feedback,
            ]
            .into_iter()
            .flatten(),
        );
        if let Some((element, attribs)) = &self.default_vao {
            objects.extend(element);
            objects.extend(attribs.iter().flatten());
        }
        objects.extend(self.samplers.iter().flatten());
        objects.extend(self.queries.iter().flatten());
        objects.extend(self.uniform_bindings.iter().flatten());
        objects.extend(self.feedback_bindings.iter().flatten());
    }
}

impl ContextObj {
    /// A deleted object leaves WebGL 2.0's bindings; a deleted bound vertex array gives the
    /// default one back (OpenGL ES 3.0 §2.10).
    pub(super) fn forget(&mut self, h: Handle) {
        let s = &mut self.gl2;
        for slot in s
            .buffers
            .iter_mut()
            .chain([&mut s.read_framebuffer, &mut s.transform_feedback])
            .chain(s.samplers.iter_mut())
            .chain(s.uniform_bindings.iter_mut())
            .chain(s.feedback_bindings.iter_mut())
        {
            if *slot == Some(h) {
                *slot = None;
            }
        }
        if s.vertex_array == Some(h) {
            s.vertex_array = None;
            if let Some((element, attribs)) = s.default_vao.take() {
                self.element_buffer = element;
                self.attrib_buffers = attribs;
            }
        }
    }
}

type Source =
    g::ImageBitmapOrImageDataOrHTMLImageElementOrHTMLCanvasElementOrHTMLVideoElementOrOffscreenCanvas;

/// The element size of a typed array (1 for a DataView).
fn elem_size(v: &View) -> usize {
    use View as V;
    match v {
        V::Int8Array(_) | V::Uint8Array(_) | V::Uint8ClampedArray(_) | V::DataView(_) => 1,
        V::Int16Array(_) | V::Uint16Array(_) | V::Float16Array(_) => 2,
        V::Int32Array(_) | V::Uint32Array(_) | V::Float32Array(_) => 4,
        V::BigInt64Array(_) | V::BigUint64Array(_) | V::Float64Array(_) => 8,
    }
}

fn view_obj(v: &View) -> &JsObject {
    use View as V;
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

/// WebGL 2.0 §3.7: the bytes of `v` from element `offset`, `length` elements (0: to the end);
/// `None` when the range is outside the array (INVALID_VALUE).
fn sub_view(cx: &mut Cx, v: &View, offset: u64, length: u64) -> Option<Vec<u8>> {
    let size = elem_size(v) as u64;
    let bytes = view_bytes(cx, view_obj(v));
    let total = bytes.len() as u64 / size;
    if offset > total {
        return None;
    }
    let length = if length == 0 { total - offset } else { length };
    if offset.checked_add(length)? > total {
        return None;
    }
    Some(bytes[(offset * size) as usize..((offset + length) * size) as usize].to_vec())
}

/// Values from `offset`, `length` of them (0: to the end); `None` outside the list.
fn sub_list<T: Clone>(v: Vec<T>, offset: u64, length: u32) -> Option<Vec<T>> {
    let total = v.len() as u64;
    if offset > total {
        return None;
    }
    let length = if length == 0 {
        total - offset
    } else {
        length as u64
    };
    if offset + length > total {
        return None;
    }
    Some(v[offset as usize..(offset + length) as usize].to_vec())
}

fn uints(cx: &mut Cx, v: &g::Uint32ArrayOrUnsignedLongSequence) -> Vec<u32> {
    match v {
        g::Uint32ArrayOrUnsignedLongSequence::UnsignedLongSequence(s) => s.clone(),
        g::Uint32ArrayOrUnsignedLongSequence::Uint32Array(o) => view_bytes(cx, o)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect(),
    }
}

/// Writes `bytes` into the typed array from byte `at`.
fn write_view(cx: &mut Cx, o: &JsObject, at: usize, bytes: &[u8]) {
    let s = cx.scope();
    let local = v8::Local::new(s, o.global());
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(local) else {
        return;
    };
    let offset = view.byte_offset() + at;
    let end = view.byte_offset() + view.byte_length();
    if let Some(buffer) = view.buffer(s) {
        let store = buffer.get_backing_store();
        for (i, b) in bytes.iter().enumerate() {
            if offset + i >= end {
                break;
            }
            if let Some(cell) = store.get(offset + i) {
                cell.set(*b);
            }
        }
    }
}

fn num_array(cx: &mut Cx, v: &[f64]) -> JsValue {
    let s = cx.scope();
    let items: Vec<v8::Local<v8::Value>> =
        v.iter().map(|n| v8::Number::new(s, *n).into()).collect();
    let a = v8::Array::new_with_elements(s, &items);
    JsValue::new(s, a.into())
}

fn bool_list(cx: &mut Cx, v: &[i32]) -> JsValue {
    super::bool_array(cx, &v.iter().map(|x| *x != 0).collect::<Vec<_>>())
}

/// The bytes per pixel of an OpenGL ES 3.0 format and type (0: not a known pair).
fn bytes_per_pixel(format: u32, ty: u32) -> usize {
    let channels = match format {
        0x1908 | 0x8D99 => 4,                            // RGBA, RGBA_INTEGER
        0x1907 | 0x8D98 => 3,                            // RGB, RGB_INTEGER
        0x8227 | 0x8228 | 0x190A => 2,                   // RG, RG_INTEGER, LUMINANCE_ALPHA
        0x1903 | 0x8D94 | 0x1906 | 0x1909 | 0x1902 => 1, // RED, RED_INTEGER, ALPHA, LUMINANCE, DEPTH
        0x84F9 => 1,                                     // DEPTH_STENCIL (packed types)
        _ => return 0,
    };
    match ty {
        0x1400 | 0x1401 => channels,              // BYTE, UNSIGNED_BYTE
        0x1402 | 0x1403 | 0x140B => channels * 2, // SHORT, UNSIGNED_SHORT, HALF_FLOAT
        0x1404..=0x1406 => channels * 4,          // INT, UNSIGNED_INT, FLOAT
        0x8363 | 0x8033 | 0x8034 => 2,            // packed 16-bit
        0x8368 | 0x8C3B | 0x8C3E | 0x84FA => 4,   // packed 32-bit
        0x8DAD => 8,                              // FLOAT_32_UNSIGNED_INT_24_8_REV
        _ => 0,
    }
}

/// WebGL 2.0 §5.14.12 (and 1.0): the typed array a readback or upload of `ty` takes.
fn view_fits_type(v: &View, ty: u32) -> bool {
    use View as V;
    match ty {
        0x1401 => matches!(v, V::Uint8Array(_) | V::Uint8ClampedArray(_)),
        0x1400 => matches!(v, V::Int8Array(_)),
        0x1403 | 0x140B | 0x8363 | 0x8033 | 0x8034 => matches!(v, V::Uint16Array(_)),
        0x1402 => matches!(v, V::Int16Array(_)),
        0x1405 | 0x8368 | 0x8C3B | 0x8C3E | 0x84FA => matches!(v, V::Uint32Array(_)),
        0x1404 => matches!(v, V::Int32Array(_)),
        0x1406 => matches!(v, V::Float32Array(_)),
        _ => true,
    }
}

impl Engine {
    /// Whether the context `h` is a WebGL 2.0 one.
    pub(crate) fn webgl_version_2(&self, h: Handle) -> bool {
        self.gl_ctx(h, |c| c.attributes.webgl2)
    }

    /// The buffer bound to a WebGL 2.0 target.
    fn gl2_bound(&self, this: Handle, target: u32) -> Option<Handle> {
        let slot = buffer_slot(target)?;
        self.gl_ctx(this, |c| c.gl2.buffers[slot])
    }

    /// WebGL 2.0 §5.14.8 / §5.14.12: an upload from client memory while a pixel unpack buffer is
    /// bound (or a readback into client memory while a pack buffer is) is INVALID_OPERATION; so is
    /// an offset into such a buffer when none is bound. Whether the call may go on.
    fn gl2_pixel_buffer(&self, this: Handle, target: u32, wants_buffer: bool) -> bool {
        if self.gl_lost(this) {
            return false;
        }
        if self.gl2_bound(this, target).is_some() != wants_buffer {
            self.gl_error(this, c::INVALID_OPERATION);
            return false;
        }
        true
    }

    /// `getParameter` for what WebGL 2.0 adds or answers differently; `None` for the rest.
    pub(super) fn gl2_parameter(&self, cx: &mut Cx, this: Handle, pname: u32) -> Option<JsValue> {
        let object = |h: Option<Handle>, cx: &mut Cx| match h {
            Some(h) => js(cx, &h),
            None => null(cx),
        };
        let (s, unit) = self.gl_ctx(this, |c| {
            let u = c.active_unit as usize;
            (
                (
                    c.gl2.buffers,
                    c.gl2.read_framebuffer,
                    c.gl2.vertex_array,
                    c.gl2.samplers.get(u).copied().flatten(),
                    c.gl2.transform_feedback,
                ),
                c.textures.get(u).copied().unwrap_or([None; 4]),
            )
        });
        Some(match pname {
            c::VERSION => js(cx, "WebGL 2.0 (OpenGL ES 3.0 Snowlark)"),
            c::SHADING_LANGUAGE_VERSION => {
                js(cx, "WebGL GLSL ES 3.00 (OpenGL ES GLSL ES 3.0 Snowlark)")
            }
            0x8F36 => object(s.0[0], cx),
            0x8F37 => object(s.0[1], cx),
            0x88ED => object(s.0[2], cx),
            0x88EF => object(s.0[3], cx),
            0x8C8F => object(s.0[4], cx),
            0x8A28 => object(s.0[5], cx),
            0x8CAA => object(s.1, cx),
            0x85B5 => object(s.2, cx),
            0x8919 => object(s.3, cx),
            0x8E25 => object(s.4, cx),
            0x806A => object(unit[2], cx),
            0x8C1D => object(unit[3], cx),
            // MAX_CLIENT_WAIT_TIMEOUT_WEBGL: clientWaitSync never blocks.
            0x9247 => js(cx, &0.0f64),
            // RASTERIZER_DISCARD, TRANSFORM_FEEDBACK_ACTIVE, TRANSFORM_FEEDBACK_PAUSED
            0x8C89 | 0x8E24 | 0x8E23 => match self.gl_query(this, Query::Booleans(pname, 1)) {
                Some(Reply::Bools(v)) if !v.is_empty() => js(cx, &v[0]),
                _ => null(cx),
            },
            // MAX_ELEMENT_INDEX, MAX_SERVER_WAIT_TIMEOUT, MAX_UNIFORM_BLOCK_SIZE,
            // MAX_COMBINED_VERTEX_UNIFORM_COMPONENTS, MAX_COMBINED_FRAGMENT_UNIFORM_COMPONENTS
            0x8D6B | 0x9111 | 0x8A30 | 0x8A31 | 0x8A33 => {
                match self.gl_query(this, Query::Integers64(pname, 1)) {
                    Some(Reply::Int64s(v)) if !v.is_empty() => js(cx, &(v[0] as f64)),
                    _ => null(cx),
                }
            }
            // MAX_TEXTURE_LOD_BIAS
            0x84FD => match self.gl_query(this, Query::Floats(pname, 1)) {
                Some(Reply::Floats(v)) if !v.is_empty() => js(cx, &(v[0] as f64)),
                _ => null(cx),
            },
            _ => return None,
        })
    }

    fn gl2_bind(&self, this: Handle, h: Option<Handle>, kind: Kind) -> Option<u32> {
        if self.gl_lost(this) {
            return None;
        }
        let id = self.gl_check_opt(this, h, kind)?;
        if let Some(h) = h {
            self.gl_obj(h, |o| o.bound = true);
        }
        Some(id)
    }

    fn gl2_program(&self, this: Handle, program: Handle) -> Option<u32> {
        if self.gl_lost(this) {
            return None;
        }
        self.gl_check(this, program, Kind::Program)
    }

    fn uniform_uv(&self, this: Handle, location: Option<Handle>, n: u8, v: Vec<u32>) {
        if let Some(loc) = self.uniform_check(this, location) {
            if v.is_empty() || !v.len().is_multiple_of(n as usize) {
                return self.gl_error(this, c::INVALID_VALUE);
            }
            self.gl_send(this, Command::Uniformui(loc, n, v));
        }
    }

    /// `uniformMatrix*fv` of WebGL 2.0 (transpose allowed): `cols` × `rows`.
    #[allow(clippy::too_many_arguments)]
    fn uniform_matrix2(
        &self,
        cx: &mut Cx,
        this: Handle,
        location: Option<Handle>,
        cols: u8,
        rows: u8,
        transpose: bool,
        data: &g::Float32ArrayOrUnrestrictedFloatSequence,
        offset: u64,
        length: u32,
    ) {
        let Some(v) = sub_list(floats(cx, data), offset, length) else {
            return self.gl_error(this, c::INVALID_VALUE);
        };
        if let Some(loc) = self.uniform_check(this, location) {
            let size = cols as usize * rows as usize;
            if v.is_empty() || !v.len().is_multiple_of(size) {
                return self.gl_error(this, c::INVALID_VALUE);
            }
            self.gl_send(
                this,
                if cols == rows {
                    Command::UniformMatrix(loc, cols, transpose, v)
                } else {
                    Command::UniformMatrixNxM(loc, cols, rows, transpose, v)
                },
            );
        }
    }

    /// A texture upload from a `TexImageSource` in a WebGL 2.0 context (no unpack buffer bound).
    fn gl2_tex_source(
        &self,
        cx: &mut Cx,
        this: Handle,
        source: &Source,
        format: u32,
        type_: u32,
        upload: impl FnOnce(i32, i32, Option<Vec<u8>>) -> Command,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        self.tex_from_source(cx, this, source, format, type_, upload)
    }

    /// Reads pixels into `dst` from byte `at` (WebGL 2.0 §5.14.12 with a destination offset).
    #[allow(clippy::too_many_arguments)]
    fn gl2_read_pixels(
        &self,
        cx: &mut Cx,
        this: Handle,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        dst: &View,
        dst_offset: u64,
    ) {
        if !self.gl2_pixel_buffer(this, PIXEL_PACK_BUFFER, false) {
            return;
        }
        if !view_fits_type(dst, type_) {
            return self.gl_error(this, c::INVALID_OPERATION);
        }
        if width < 0 || height < 0 {
            return self.gl_error(this, c::INVALID_VALUE);
        }
        let o = view_obj(dst).clone();
        let at = dst_offset as usize * elem_size(dst);
        let length = view_bytes(cx, &o).len();
        if at > length {
            return self.gl_error(this, c::INVALID_VALUE);
        }
        let bpp = bytes_per_pixel(format, type_);
        let align = self.gl_ctx(this, |c| c.pack_alignment.max(1) as usize);
        let row = (width as usize * bpp).div_ceil(align) * align;
        let needed = if height == 0 || width == 0 {
            0
        } else if bpp == 0 {
            // (a format and type the service refuses: it answers with nothing)
            length - at
        } else {
            row * (height as usize - 1) + width as usize * bpp
        };
        if needed > length - at {
            return self.gl_error(this, c::INVALID_OPERATION);
        }
        let Some(Reply::Bytes(bytes)) = self.gl_query(
            this,
            Query::ReadPixels(x, y, width, height, format, type_, needed as u32),
        ) else {
            return;
        };
        write_view(cx, &o, at, &bytes[..needed.min(bytes.len())]);
    }
}

impl WebGL2RenderingContextBaseImpl for Engine {
    fn copy_buffer_sub_data(
        &self,
        _cx: &mut Cx,
        this: Handle,
        read_target: u32,
        write_target: u32,
        read_offset: i64,
        write_offset: i64,
        size: i64,
    ) -> rt::Result<()> {
        if read_offset < 0 || write_offset < 0 || size < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            Command::CopyBufferSubData(read_target, write_target, read_offset, write_offset, size),
        );
        Ok(())
    }
    /// WebGL 2.0 §5.14.4 `getBufferSubData`.
    fn get_buffer_sub_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        src_byte_offset: i64,
        dst_buffer: View,
        dst_offset: u64,
        length: u32,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        if src_byte_offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        let size = elem_size(&dst_buffer) as u64;
        let o = view_obj(&dst_buffer).clone();
        let total = view_bytes(cx, &o).len() as u64 / size;
        if dst_offset > total || (length != 0 && dst_offset + length as u64 > total) {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        let count = if length == 0 {
            total - dst_offset
        } else {
            length as u64
        };
        let bytes = count * size;
        let bound = self.gl_ctx(this, |c| match target {
            c::ARRAY_BUFFER => c.array_buffer,
            c::ELEMENT_ARRAY_BUFFER => c.element_buffer,
            t => buffer_slot(t).and_then(|s| c.gl2.buffers[s]),
        });
        if bound.is_none() {
            // (an unknown target is the service's INVALID_ENUM)
            if target == c::ARRAY_BUFFER
                || target == c::ELEMENT_ARRAY_BUFFER
                || buffer_slot(target).is_some()
            {
                self.gl_error(this, c::INVALID_OPERATION);
                return Ok(());
            }
        }
        if bytes == 0 {
            return Ok(());
        }
        let Some(Reply::Bytes(data)) = self.gl_query(
            this,
            Query::BufferSubData(target, src_byte_offset, bytes.min(u32::MAX as u64) as u32),
        ) else {
            return Ok(());
        };
        write_view(cx, &o, (dst_offset * size) as usize, &data);
        Ok(())
    }
    fn blit_framebuffer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        src_x0: i32,
        src_y0: i32,
        src_x1: i32,
        src_y1: i32,
        dst_x0: i32,
        dst_y0: i32,
        dst_x1: i32,
        dst_y1: i32,
        mask: u32,
        filter: u32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::BlitFramebuffer(
                [
                    src_x0, src_y0, src_x1, src_y1, dst_x0, dst_y0, dst_x1, dst_y1,
                ],
                mask,
                filter,
            ),
        );
        self.gl_drew(this);
        Ok(())
    }
    fn framebuffer_texture_layer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        attachment: u32,
        texture: Option<Handle>,
        level: i32,
        layer: i32,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(id) = self.gl_check_opt(this, texture, Kind::Texture) else {
            return Ok(());
        };
        let fb = self.gl_ctx(this, |c| {
            if target == READ_FRAMEBUFFER {
                c.gl2.read_framebuffer
            } else {
                c.framebuffer
            }
        });
        if let Some(fb) = fb {
            self.gl_obj(fb, |o| {
                o.attachments.retain(|(a, _)| *a != attachment);
                o.attachments.extend(texture.map(|t| (attachment, t)));
            });
        }
        self.gl_send(
            this,
            Command::FramebufferTextureLayer(target, attachment, id, level, layer),
        );
        Ok(())
    }
    fn invalidate_framebuffer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        attachments: Vec<u32>,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::InvalidateFramebuffer(target, attachments));
        Ok(())
    }
    fn invalidate_sub_framebuffer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        attachments: Vec<u32>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::InvalidateSubFramebuffer(target, attachments, [x, y, width, height]),
        );
        Ok(())
    }
    fn read_buffer(&self, _cx: &mut Cx, this: Handle, src: u32) -> rt::Result<()> {
        self.gl_send(this, Command::ReadBuffer(src));
        Ok(())
    }
    fn get_internalformat_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        internalformat: u32,
        pname: u32,
    ) -> rt::Result<JsValue> {
        // SAMPLES: the sample counts, most first
        if pname != 0x80A9 {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(null(cx));
        }
        Ok(
            match self.gl_query(
                this,
                Query::InternalformatParameter(target, internalformat, pname),
            ) {
                Some(Reply::Ints(v)) => i32_array(cx, &v),
                _ => null(cx),
            },
        )
    }
    fn renderbuffer_storage_multisample(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        samples: i32,
        internalformat: u32,
        width: i32,
        height: i32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::RenderbufferStorageMultisample(target, samples, internalformat, width, height),
        );
        Ok(())
    }
    fn tex_storage2_d(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        levels: i32,
        internalformat: u32,
        width: i32,
        height: i32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::TexStorage2D(target, levels, internalformat, width, height),
        );
        Ok(())
    }
    fn tex_storage3_d(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        levels: i32,
        internalformat: u32,
        width: i32,
        height: i32,
        depth: i32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::TexStorage3D(target, levels, internalformat, width, height, depth),
        );
        Ok(())
    }
    fn tex_image3_d_target_level_internalformat_width_height_depth_border_format_type_pbo_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        format: u32,
        type_: u32,
        pbo_offset: i64,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, true) {
            return Ok(());
        }
        if pbo_offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            Command::TexImage3D(
                target,
                level,
                internalformat,
                width,
                height,
                depth,
                border,
                format,
                type_,
                Pixels::Offset(pbo_offset),
            ),
        );
        Ok(())
    }
    fn tex_image3_d_target_level_internalformat_width_height_depth_border_format_type_source(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: i32,
        _width: i32,
        _height: i32,
        depth: i32,
        border: i32,
        format: u32,
        type_: u32,
        source: Source,
    ) -> rt::Result<()> {
        // (one layer of the source's size; a source cut into several layers by
        // UNPACK_IMAGE_HEIGHT is not done yet)
        if depth != 1 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl2_tex_source(cx, this, &source, format, type_, |w, h, bytes| {
            Command::TexImage3D(
                target,
                level,
                internalformat,
                w,
                h,
                1,
                border,
                format,
                type_,
                bytes.map_or(Pixels::None, Pixels::Data),
            )
        })
    }
    fn tex_image3_d_target_level_internalformat_width_height_depth_border_format_type_src_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        format: u32,
        type_: u32,
        src_data: Option<View>,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        let px = match src_data {
            Some(v) => Pixels::Data(view_bytes(cx, view_obj(&v))),
            None => Pixels::None,
        };
        self.gl_send(
            this,
            Command::TexImage3D(
                target,
                level,
                internalformat,
                width,
                height,
                depth,
                border,
                format,
                type_,
                px,
            ),
        );
        Ok(())
    }
    fn tex_image3_d_target_level_internalformat_width_height_depth_border_format_type_src_data_src_offset(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        format: u32,
        type_: u32,
        src_data: View,
        src_offset: u64,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        let Some(bytes) = sub_view(cx, &src_data, src_offset, 0) else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        self.gl_send(
            this,
            Command::TexImage3D(
                target,
                level,
                internalformat,
                width,
                height,
                depth,
                border,
                format,
                type_,
                Pixels::Data(bytes),
            ),
        );
        Ok(())
    }
    fn tex_sub_image3_d_target_level_xoffset_yoffset_zoffset_width_height_depth_format_type_pbo_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        type_: u32,
        pbo_offset: i64,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, true) {
            return Ok(());
        }
        if pbo_offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            Command::TexSubImage3D(
                target,
                level,
                xoffset,
                yoffset,
                zoffset,
                width,
                height,
                depth,
                format,
                type_,
                Pixels::Offset(pbo_offset),
            ),
        );
        Ok(())
    }
    fn tex_sub_image3_d_target_level_xoffset_yoffset_zoffset_width_height_depth_format_type_source(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        _width: i32,
        _height: i32,
        depth: i32,
        format: u32,
        type_: u32,
        source: Source,
    ) -> rt::Result<()> {
        if depth != 1 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl2_tex_source(cx, this, &source, format, type_, |w, h, bytes| {
            Command::TexSubImage3D(
                target,
                level,
                xoffset,
                yoffset,
                zoffset,
                w,
                h,
                1,
                format,
                type_,
                bytes.map_or(Pixels::None, Pixels::Data),
            )
        })
    }
    fn tex_sub_image3_d_target_level_xoffset_yoffset_zoffset_width_height_depth_format_type_src_data_src_offset(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        type_: u32,
        src_data: Option<View>,
        src_offset: u64,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        let Some(src_data) = src_data else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        let Some(bytes) = sub_view(cx, &src_data, src_offset, 0) else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        self.gl_send(
            this,
            Command::TexSubImage3D(
                target,
                level,
                xoffset,
                yoffset,
                zoffset,
                width,
                height,
                depth,
                format,
                type_,
                Pixels::Data(bytes),
            ),
        );
        Ok(())
    }
    fn copy_tex_sub_image3_d(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::CopyTexSubImage3D(
                target, level, xoffset, yoffset, zoffset, x, y, width, height,
            ),
        );
        Ok(())
    }
    fn compressed_tex_image3_d_target_level_internalformat_width_height_depth_border_image_size_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        _target: u32,
        _level: i32,
        _internalformat: u32,
        _width: i32,
        _height: i32,
        _depth: i32,
        _border: i32,
        _image_size: i32,
        _offset: i64,
    ) -> rt::Result<()> {
        // (compressed uploads from a pixel unpack buffer are not done yet: as with none bound)
        self.gl_error(this, c::INVALID_OPERATION);
        Ok(())
    }
    fn compressed_tex_image3_d_target_level_internalformat_width_height_depth_border_src_data_src_offset_src_length_override(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: u32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        src_data: View,
        src_offset: u64,
        src_length_override: u32,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        let Some(bytes) = sub_view(cx, &src_data, src_offset, src_length_override as u64) else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        self.gl_send(
            this,
            Command::CompressedTexImage3D(
                target,
                level,
                internalformat,
                width,
                height,
                depth,
                border,
                bytes,
            ),
        );
        Ok(())
    }
    fn compressed_tex_sub_image3_d_target_level_xoffset_yoffset_zoffset_width_height_depth_format_image_size_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        _target: u32,
        _level: i32,
        _xoffset: i32,
        _yoffset: i32,
        _zoffset: i32,
        _width: i32,
        _height: i32,
        _depth: i32,
        _format: u32,
        _image_size: i32,
        _offset: i64,
    ) -> rt::Result<()> {
        self.gl_error(this, c::INVALID_OPERATION);
        Ok(())
    }
    fn compressed_tex_sub_image3_d_target_level_xoffset_yoffset_zoffset_width_height_depth_format_src_data_src_offset_src_length_override(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        src_data: View,
        src_offset: u64,
        src_length_override: u32,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        let Some(bytes) = sub_view(cx, &src_data, src_offset, src_length_override as u64) else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        self.gl_send(
            this,
            Command::CompressedTexSubImage3D(
                target, level, xoffset, yoffset, zoffset, width, height, depth, format, bytes,
            ),
        );
        Ok(())
    }
    fn get_frag_data_location(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        name: &str,
    ) -> rt::Result<i32> {
        let Some(p) = self.gl2_program(this, program) else {
            return Ok(-1);
        };
        Ok(self
            .gl_query(this, Query::FragDataLocation(p, name.to_owned()))
            .map_or(-1, |r| r.int()))
    }
    fn uniform1ui(&self, _cx: &mut Cx, this: Handle, l: Option<Handle>, v0: u32) -> rt::Result<()> {
        self.uniform_uv(this, l, 1, vec![v0]);
        Ok(())
    }
    fn uniform2ui(
        &self,
        _cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v0: u32,
        v1: u32,
    ) -> rt::Result<()> {
        self.uniform_uv(this, l, 2, vec![v0, v1]);
        Ok(())
    }
    fn uniform3ui(
        &self,
        _cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v0: u32,
        v1: u32,
        v2: u32,
    ) -> rt::Result<()> {
        self.uniform_uv(this, l, 3, vec![v0, v1, v2]);
        Ok(())
    }
    fn uniform4ui(
        &self,
        _cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        v0: u32,
        v1: u32,
        v2: u32,
        v3: u32,
    ) -> rt::Result<()> {
        self.uniform_uv(this, l, 4, vec![v0, v1, v2, v3]);
        Ok(())
    }
    fn uniform1uiv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Uint32ArrayOrUnsignedLongSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(uints(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_uv(this, l, 1, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform2uiv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Uint32ArrayOrUnsignedLongSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(uints(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_uv(this, l, 2, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform3uiv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Uint32ArrayOrUnsignedLongSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(uints(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_uv(this, l, 3, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform4uiv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Uint32ArrayOrUnsignedLongSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(uints(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_uv(this, l, 4, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform_matrix3x2fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        self.uniform_matrix2(cx, this, l, 3, 2, t, &data, src_offset, src_length);
        Ok(())
    }
    fn uniform_matrix4x2fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        self.uniform_matrix2(cx, this, l, 4, 2, t, &data, src_offset, src_length);
        Ok(())
    }
    fn uniform_matrix2x3fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        self.uniform_matrix2(cx, this, l, 2, 3, t, &data, src_offset, src_length);
        Ok(())
    }
    fn uniform_matrix4x3fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        self.uniform_matrix2(cx, this, l, 4, 3, t, &data, src_offset, src_length);
        Ok(())
    }
    fn uniform_matrix2x4fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        self.uniform_matrix2(cx, this, l, 2, 4, t, &data, src_offset, src_length);
        Ok(())
    }
    fn uniform_matrix3x4fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        self.uniform_matrix2(cx, this, l, 3, 4, t, &data, src_offset, src_length);
        Ok(())
    }
    fn vertex_attrib_i4i(
        &self,
        _cx: &mut Cx,
        this: Handle,
        index: u32,
        x: i32,
        y: i32,
        z: i32,
        w: i32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::VertexAttribI(index, [x, y, z, w]));
        Ok(())
    }
    fn vertex_attrib_i4iv(
        &self,
        cx: &mut Cx,
        this: Handle,
        index: u32,
        values: g::Int32ArrayOrLongSequence,
    ) -> rt::Result<()> {
        let v = ints(cx, &values);
        if v.len() < 4 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            Command::VertexAttribI(index, [v[0], v[1], v[2], v[3]]),
        );
        Ok(())
    }
    fn vertex_attrib_i4ui(
        &self,
        _cx: &mut Cx,
        this: Handle,
        index: u32,
        x: u32,
        y: u32,
        z: u32,
        w: u32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::VertexAttribIu(index, [x, y, z, w]));
        Ok(())
    }
    fn vertex_attrib_i4uiv(
        &self,
        cx: &mut Cx,
        this: Handle,
        index: u32,
        values: g::Uint32ArrayOrUnsignedLongSequence,
    ) -> rt::Result<()> {
        let v = uints(cx, &values);
        if v.len() < 4 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            Command::VertexAttribIu(index, [v[0], v[1], v[2], v[3]]),
        );
        Ok(())
    }
    fn vertex_attrib_i_pointer(
        &self,
        _cx: &mut Cx,
        this: Handle,
        index: u32,
        size: i32,
        type_: u32,
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
        // WebGL 1.0 §6.6 (and 2.0): no array buffer bound with a non-zero offset
        let buffer = self.gl_ctx(this, |c| c.array_buffer);
        if buffer.is_none() && offset != 0 {
            self.gl_error(this, c::INVALID_OPERATION);
            return Ok(());
        }
        self.gl_ctx(this, |c| {
            if let Some(a) = c.attrib_buffers.get_mut(index as usize) {
                *a = buffer;
            }
        });
        self.gl_send(
            this,
            Command::VertexAttribIPointer(index, size, type_, stride, offset),
        );
        Ok(())
    }
    fn vertex_attrib_divisor(
        &self,
        _cx: &mut Cx,
        this: Handle,
        index: u32,
        divisor: u32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::VertexAttribDivisor(index, divisor));
        Ok(())
    }
    fn draw_arrays_instanced(
        &self,
        _cx: &mut Cx,
        this: Handle,
        mode: u32,
        first: i32,
        count: i32,
        instance_count: i32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::DrawArraysInstanced(mode, first, count, instance_count),
        );
        self.gl_drew(this);
        Ok(())
    }
    fn draw_elements_instanced(
        &self,
        _cx: &mut Cx,
        this: Handle,
        mode: u32,
        count: i32,
        type_: u32,
        offset: i64,
        instance_count: i32,
    ) -> rt::Result<()> {
        if offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            Command::DrawElementsInstanced(mode, count, type_, offset, instance_count),
        );
        self.gl_drew(this);
        Ok(())
    }
    fn draw_range_elements(
        &self,
        _cx: &mut Cx,
        this: Handle,
        mode: u32,
        start: u32,
        end: u32,
        count: i32,
        type_: u32,
        offset: i64,
    ) -> rt::Result<()> {
        if offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            Command::DrawRangeElements(mode, start, end, count, type_, offset),
        );
        self.gl_drew(this);
        Ok(())
    }
    fn draw_buffers(&self, _cx: &mut Cx, this: Handle, buffers: Vec<u32>) -> rt::Result<()> {
        self.gl_send(this, Command::DrawBuffers(buffers));
        Ok(())
    }
    /// WebGL 2.0 §5.14.11 `clearBufferfv`: enough values from `srcOffset` (4 for COLOR, 1 for
    /// DEPTH), else INVALID_VALUE.
    fn clear_bufferfv(
        &self,
        cx: &mut Cx,
        this: Handle,
        buffer: u32,
        drawbuffer: i32,
        values: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
    ) -> rt::Result<()> {
        let v = sub_list(floats(cx, &values), src_offset, 0).unwrap_or_default();
        let need = if buffer == COLOR { 4 } else { 1 };
        if (buffer == COLOR || buffer == DEPTH) && v.len() < need {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        let mut a = [0.0; 4];
        for (d, s) in a.iter_mut().zip(&v) {
            *d = *s;
        }
        self.gl_send(this, Command::ClearBufferfv(buffer, drawbuffer, a));
        self.gl_drew(this);
        Ok(())
    }
    fn clear_bufferiv(
        &self,
        cx: &mut Cx,
        this: Handle,
        buffer: u32,
        drawbuffer: i32,
        values: g::Int32ArrayOrLongSequence,
        src_offset: u64,
    ) -> rt::Result<()> {
        let v = sub_list(ints(cx, &values), src_offset, 0).unwrap_or_default();
        let need = if buffer == COLOR { 4 } else { 1 };
        if (buffer == COLOR || buffer == STENCIL) && v.len() < need {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        let mut a = [0; 4];
        for (d, s) in a.iter_mut().zip(&v) {
            *d = *s;
        }
        self.gl_send(this, Command::ClearBufferiv(buffer, drawbuffer, a));
        self.gl_drew(this);
        Ok(())
    }
    fn clear_bufferuiv(
        &self,
        cx: &mut Cx,
        this: Handle,
        buffer: u32,
        drawbuffer: i32,
        values: g::Uint32ArrayOrUnsignedLongSequence,
        src_offset: u64,
    ) -> rt::Result<()> {
        let v = sub_list(uints(cx, &values), src_offset, 0).unwrap_or_default();
        if buffer == COLOR && v.len() < 4 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        let mut a = [0; 4];
        for (d, s) in a.iter_mut().zip(&v) {
            *d = *s;
        }
        self.gl_send(this, Command::ClearBufferuiv(buffer, drawbuffer, a));
        self.gl_drew(this);
        Ok(())
    }
    fn clear_bufferfi(
        &self,
        _cx: &mut Cx,
        this: Handle,
        buffer: u32,
        drawbuffer: i32,
        depth: f32,
        stencil: i32,
    ) -> rt::Result<()> {
        self.gl_send(
            this,
            Command::ClearBufferfi(buffer, drawbuffer, depth, stencil),
        );
        self.gl_drew(this);
        Ok(())
    }
    fn create_query(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.gl_create(this, Kind::Query, 0))
    }
    fn delete_query(&self, _cx: &mut Cx, this: Handle, query: Option<Handle>) -> rt::Result<()> {
        if let Some(q) = query {
            self.gl_ctx(this, |c| {
                for slot in &mut c.gl2.queries {
                    if *slot == Some(q) {
                        *slot = None;
                    }
                }
            });
        }
        self.gl_delete(this, query, Kind::Query);
        Ok(())
    }
    fn is_query(&self, _cx: &mut Cx, this: Handle, query: Option<Handle>) -> rt::Result<bool> {
        Ok(self.gl_is(this, query, Kind::Query))
    }
    fn begin_query(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        query: Handle,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(id) = self.gl_check(this, query, Kind::Query) else {
            return Ok(());
        };
        let Some(slot) = query_slot(target) else {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(());
        };
        // (a query already active, or one begun with another target: INVALID_OPERATION)
        let busy = self.gl_ctx(this, |c| {
            c.gl2.queries[slot].is_some() || c.gl2.queries.contains(&Some(query))
        });
        let other = self
            .gl_obj(query, |o| o.target != 0 && o.target != target)
            .unwrap_or(false);
        if busy || other {
            self.gl_error(this, c::INVALID_OPERATION);
            return Ok(());
        }
        self.gl_obj(query, |o| {
            o.target = target;
            o.bound = true;
        });
        self.gl_ctx(this, |c| c.gl2.queries[slot] = Some(query));
        self.gl_send(this, Command::BeginQuery(target, id));
        Ok(())
    }
    fn end_query(&self, _cx: &mut Cx, this: Handle, target: u32) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(slot) = query_slot(target) else {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(());
        };
        let Some(q) = self.gl_ctx(this, |c| c.gl2.queries[slot].take()) else {
            self.gl_error(this, c::INVALID_OPERATION);
            return Ok(());
        };
        // (its result is available from the next checkpoint at the earliest)
        let epoch = self.gl.borrow().epoch;
        self.gl_obj(q, |o| o.epoch = epoch);
        self.gl_send(this, Command::EndQuery(target));
        Ok(())
    }
    fn get_query(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        pname: u32,
    ) -> rt::Result<Option<Handle>> {
        if self.gl_lost(this) {
            return Ok(None);
        }
        let Some(slot) = query_slot(target).filter(|_| pname == CURRENT_QUERY) else {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(None);
        };
        Ok(self.gl_ctx(this, |c| c.gl2.queries[slot]))
    }
    /// WebGL 2.0 §5.14.? `getQueryParameter`: a result is never available in the task that ended
    /// the query (§3.7.12).
    fn get_query_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        query: Handle,
        pname: u32,
    ) -> rt::Result<JsValue> {
        if self.gl_lost(this) {
            return Ok(null(cx));
        }
        let Some(id) = self.gl_check(this, query, Kind::Query) else {
            return Ok(null(cx));
        };
        let active = self.gl_ctx(this, |c| c.gl2.queries.contains(&Some(query)));
        let (ended, epoch) = self
            .gl_obj(query, |o| (o.target != 0, o.epoch))
            .unwrap_or((false, 0));
        if active || !ended {
            self.gl_error(this, c::INVALID_OPERATION);
            return Ok(null(cx));
        }
        let ready = epoch != self.gl.borrow().epoch;
        match pname {
            QUERY_RESULT_AVAILABLE if !ready => Ok(js(cx, &false)),
            QUERY_RESULT_AVAILABLE => {
                let v = self.gl_query(this, Query::QueryParameter(id, pname));
                Ok(js(cx, &(v.map_or(0, |r| r.int()) != 0)))
            }
            QUERY_RESULT => {
                let v = self.gl_query(this, Query::QueryParameter(id, pname));
                Ok(js(cx, &(v.map_or(0, |r| r.int()) as u32)))
            }
            _ => {
                self.gl_error(this, c::INVALID_ENUM);
                Ok(null(cx))
            }
        }
    }
    fn create_sampler(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.gl_create(this, Kind::Sampler, 0))
    }
    fn delete_sampler(
        &self,
        _cx: &mut Cx,
        this: Handle,
        sampler: Option<Handle>,
    ) -> rt::Result<()> {
        self.gl_delete(this, sampler, Kind::Sampler);
        Ok(())
    }
    fn is_sampler(&self, _cx: &mut Cx, this: Handle, sampler: Option<Handle>) -> rt::Result<bool> {
        Ok(self.gl_is(this, sampler, Kind::Sampler))
    }
    fn bind_sampler(
        &self,
        _cx: &mut Cx,
        this: Handle,
        unit: u32,
        sampler: Option<Handle>,
    ) -> rt::Result<()> {
        let Some(id) = self.gl2_bind(this, sampler, Kind::Sampler) else {
            return Ok(());
        };
        let ok = self.gl_ctx(this, |c| match c.gl2.samplers.get_mut(unit as usize) {
            Some(s) => {
                *s = sampler;
                true
            }
            None => false,
        });
        if !ok {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(this, Command::BindSampler(unit, id));
        Ok(())
    }
    fn sampler_parameteri(
        &self,
        _cx: &mut Cx,
        this: Handle,
        sampler: Handle,
        pname: u32,
        param: i32,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        if let Some(id) = self.gl_check(this, sampler, Kind::Sampler) {
            self.gl_send(this, Command::SamplerParameteri(id, pname, param));
        }
        Ok(())
    }
    fn sampler_parameterf(
        &self,
        _cx: &mut Cx,
        this: Handle,
        sampler: Handle,
        pname: u32,
        param: f32,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        if let Some(id) = self.gl_check(this, sampler, Kind::Sampler) {
            self.gl_send(this, Command::SamplerParameterf(id, pname, param));
        }
        Ok(())
    }
    fn get_sampler_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        sampler: Handle,
        pname: u32,
    ) -> rt::Result<JsValue> {
        if self.gl_lost(this) {
            return Ok(null(cx));
        }
        let Some(id) = self.gl_check(this, sampler, Kind::Sampler) else {
            return Ok(null(cx));
        };
        // TEXTURE_MIN_LOD and TEXTURE_MAX_LOD are floats; the filters, wraps and compare
        // parameters enums.
        let float = matches!(pname, 0x813A | 0x813B);
        if !float
            && !matches!(
                pname,
                0x2800 | 0x2801 | 0x2802 | 0x2803 | 0x8072 | 0x884C | 0x884D
            )
        {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(null(cx));
        }
        Ok(
            match self.gl_query(this, Query::SamplerParameter(id, pname, float)) {
                Some(Reply::Floats(v)) if !v.is_empty() => js(cx, &(v[0] as f64)),
                Some(Reply::Int(v)) => js(cx, &(v as u32)),
                _ => null(cx),
            },
        )
    }
    /// WebGL 2.0 §5.14.? `fenceSync`.
    fn fence_sync(
        &self,
        _cx: &mut Cx,
        this: Handle,
        condition: u32,
        flags: u32,
    ) -> rt::Result<Option<Handle>> {
        if self.gl_lost(this) {
            return Ok(None);
        }
        if condition != SYNC_GPU_COMMANDS_COMPLETE {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(None);
        }
        if flags != 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(None);
        }
        Ok(Some(self.gl_create(this, Kind::Sync, 0)))
    }
    fn is_sync(&self, _cx: &mut Cx, this: Handle, sync: Option<Handle>) -> rt::Result<bool> {
        Ok(self.gl_is(this, sync, Kind::Sync))
    }
    fn delete_sync(&self, _cx: &mut Cx, this: Handle, sync: Option<Handle>) -> rt::Result<()> {
        self.gl_delete(this, sync, Kind::Sync);
        Ok(())
    }
    /// WebGL 2.0 §3.7.14 `clientWaitSync`: never blocks (MAX_CLIENT_WAIT_TIMEOUT_WEBGL is 0), and
    /// reports TIMEOUT_EXPIRED until a checkpoint passed since the fence.
    fn client_wait_sync(
        &self,
        _cx: &mut Cx,
        this: Handle,
        sync: Handle,
        flags: u32,
        timeout: u64,
    ) -> rt::Result<u32> {
        if self.gl_lost(this) {
            return Ok(WAIT_FAILED);
        }
        let Some(id) = self.gl_check(this, sync, Kind::Sync) else {
            return Ok(WAIT_FAILED);
        };
        if flags & !SYNC_FLUSH_COMMANDS_BIT != 0 || timeout > 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(WAIT_FAILED);
        }
        let epoch = self.gl_obj(sync, |o| o.epoch).unwrap_or(0);
        if epoch == self.gl.borrow().epoch {
            if flags & SYNC_FLUSH_COMMANDS_BIT != 0 {
                self.gl_send(this, Command::Flush);
            }
            return Ok(TIMEOUT_EXPIRED);
        }
        Ok(self
            .gl_query(this, Query::ClientWaitSync(id, flags, 0))
            .map_or(WAIT_FAILED, |r| r.int() as u32))
    }
    fn wait_sync(
        &self,
        _cx: &mut Cx,
        this: Handle,
        sync: Handle,
        flags: u32,
        timeout: i64,
    ) -> rt::Result<()> {
        if self.gl_lost(this) {
            return Ok(());
        }
        let Some(id) = self.gl_check(this, sync, Kind::Sync) else {
            return Ok(());
        };
        // (no flags, and TIMEOUT_IGNORED)
        if flags != 0 || timeout != -1 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(this, Command::WaitSync(id));
        Ok(())
    }
    fn get_sync_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        sync: Handle,
        pname: u32,
    ) -> rt::Result<JsValue> {
        if self.gl_lost(this) {
            return Ok(null(cx));
        }
        let Some(id) = self.gl_check(this, sync, Kind::Sync) else {
            return Ok(null(cx));
        };
        let epoch = self.gl_obj(sync, |o| o.epoch).unwrap_or(0);
        Ok(match pname {
            // OBJECT_TYPE: SYNC_FENCE; SYNC_CONDITION; SYNC_FLAGS
            0x9112 => js(cx, &0x9116u32),
            0x9113 => js(cx, &SYNC_GPU_COMMANDS_COMPLETE),
            0x9115 => js(cx, &0u32),
            // SYNC_STATUS
            0x9114 if epoch == self.gl.borrow().epoch => js(cx, &(UNSIGNALED as u32)),
            0x9114 => match self.gl_query(this, Query::SyncParameter(id, pname)) {
                Some(Reply::Int(v)) => js(cx, &(v as u32)),
                _ => null(cx),
            },
            _ => {
                self.gl_error(this, c::INVALID_ENUM);
                null(cx)
            }
        })
    }
    fn create_transform_feedback(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.gl_create(this, Kind::TransformFeedback, 0))
    }
    fn delete_transform_feedback(
        &self,
        _cx: &mut Cx,
        this: Handle,
        tf: Option<Handle>,
    ) -> rt::Result<()> {
        self.gl_delete(this, tf, Kind::TransformFeedback);
        Ok(())
    }
    fn is_transform_feedback(
        &self,
        _cx: &mut Cx,
        this: Handle,
        tf: Option<Handle>,
    ) -> rt::Result<bool> {
        Ok(self.gl_is(this, tf, Kind::TransformFeedback))
    }
    fn bind_transform_feedback(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        tf: Option<Handle>,
    ) -> rt::Result<()> {
        let Some(id) = self.gl2_bind(this, tf, Kind::TransformFeedback) else {
            return Ok(());
        };
        // TRANSFORM_FEEDBACK
        if target == 0x8E22 {
            self.gl_ctx(this, |c| c.gl2.transform_feedback = tf);
        }
        self.gl_send(this, Command::BindTransformFeedback(target, id));
        Ok(())
    }
    fn begin_transform_feedback(
        &self,
        _cx: &mut Cx,
        this: Handle,
        primitive_mode: u32,
    ) -> rt::Result<()> {
        self.gl_send(this, Command::BeginTransformFeedback(primitive_mode));
        Ok(())
    }
    fn end_transform_feedback(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.gl_send(this, Command::EndTransformFeedback);
        Ok(())
    }
    fn transform_feedback_varyings(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        varyings: Vec<String>,
        buffer_mode: u32,
    ) -> rt::Result<()> {
        if let Some(p) = self.gl2_program(this, program) {
            self.gl_send(
                this,
                Command::TransformFeedbackVaryings(p, varyings, buffer_mode),
            );
        }
        Ok(())
    }
    fn get_transform_feedback_varying(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        index: u32,
    ) -> rt::Result<Option<Handle>> {
        let Some(p) = self.gl2_program(this, program) else {
            return Ok(None);
        };
        Ok(
            match self.gl_query(this, Query::TransformFeedbackVarying(p, index)) {
                Some(Reply::ActiveInfo(Some((size, ty, name)))) => {
                    Some(self.alloc(Object::WebGl(Box::new(WebGlObject::ActiveInfo {
                        size,
                        ty,
                        name,
                    }))))
                }
                _ => None,
            },
        )
    }
    fn pause_transform_feedback(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.gl_send(this, Command::PauseTransformFeedback);
        Ok(())
    }
    fn resume_transform_feedback(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.gl_send(this, Command::ResumeTransformFeedback);
        Ok(())
    }
    fn bind_buffer_base(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        index: u32,
        buffer: Option<Handle>,
    ) -> rt::Result<()> {
        self.bind_buffer_range(cx, this, target, index, buffer, 0, -1)
    }
    /// `bindBufferRange` (and `bindBufferBase`, as a size of -1): the indexed binding and the
    /// generic one.
    fn bind_buffer_range(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        index: u32,
        buffer: Option<Handle>,
        offset: i64,
        size: i64,
    ) -> rt::Result<()> {
        let base = size == -1;
        if !base && (offset < 0 || (buffer.is_some() && size <= 0)) {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        let Some(id) = self.gl2_bind(this, buffer, Kind::Buffer) else {
            return Ok(());
        };
        let ok = self.gl_ctx(this, |c| {
            let list = match target {
                UNIFORM_BUFFER => &mut c.gl2.uniform_bindings,
                TRANSFORM_FEEDBACK_BUFFER => &mut c.gl2.feedback_bindings,
                // (the service's INVALID_ENUM)
                _ => return true,
            };
            let Some(slot) = list.get_mut(index as usize) else {
                return false;
            };
            *slot = buffer;
            if let Some(s) = buffer_slot(target) {
                c.gl2.buffers[s] = buffer;
            }
            true
        });
        if !ok {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            if base {
                Command::BindBufferBase(target, index, id)
            } else {
                Command::BindBufferRange(target, index, id, offset, size)
            },
        );
        Ok(())
    }
    fn get_indexed_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        index: u32,
    ) -> rt::Result<JsValue> {
        if self.gl_lost(this) {
            return Ok(null(cx));
        }
        let binding = |c: &mut ContextObj, list_of_uniform: bool| {
            let list = if list_of_uniform {
                &c.gl2.uniform_bindings
            } else {
                &c.gl2.feedback_bindings
            };
            list.get(index as usize).copied()
        };
        let object = match target {
            // TRANSFORM_FEEDBACK_BUFFER_BINDING, UNIFORM_BUFFER_BINDING
            0x8C8F => Some(self.gl_ctx(this, |c| binding(c, false))),
            0x8A28 => Some(self.gl_ctx(this, |c| binding(c, true))),
            _ => None,
        };
        if let Some(o) = object {
            return Ok(match o {
                None => {
                    self.gl_error(this, c::INVALID_VALUE);
                    null(cx)
                }
                Some(Some(h)) => js(cx, &h),
                Some(None) => null(cx),
            });
        }
        // *_START and *_SIZE (GLintptr, GLsizeiptr)
        if !matches!(target, 0x8C84 | 0x8C85 | 0x8A29 | 0x8A2A) {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(null(cx));
        }
        Ok(
            match self.gl_query(this, Query::IndexedInteger64(target, index)) {
                Some(Reply::Int64s(v)) if !v.is_empty() => js(cx, &(v[0] as f64)),
                _ => null(cx),
            },
        )
    }
    fn get_uniform_indices(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        uniform_names: Vec<String>,
    ) -> rt::Result<Option<Vec<u32>>> {
        let Some(p) = self.gl2_program(this, program) else {
            return Ok(None);
        };
        Ok(
            match self.gl_query(this, Query::UniformIndices(p, uniform_names)) {
                Some(Reply::Ints(v)) => Some(v.into_iter().map(|x| x as u32).collect()),
                _ => None,
            },
        )
    }
    fn get_active_uniforms(
        &self,
        cx: &mut Cx,
        this: Handle,
        program: Handle,
        uniform_indices: Vec<u32>,
        pname: u32,
    ) -> rt::Result<JsValue> {
        let Some(p) = self.gl2_program(this, program) else {
            return Ok(null(cx));
        };
        // UNIFORM_TYPE, UNIFORM_SIZE (unsigned), UNIFORM_BLOCK_INDEX, UNIFORM_OFFSET,
        // UNIFORM_ARRAY_STRIDE, UNIFORM_MATRIX_STRIDE (signed), UNIFORM_IS_ROW_MAJOR (boolean)
        if !matches!(pname, 0x8A37 | 0x8A38 | 0x8A3A..=0x8A3E) {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(null(cx));
        }
        Ok(
            match self.gl_query(this, Query::ActiveUniforms(p, uniform_indices, pname)) {
                Some(Reply::Ints(v)) if pname == 0x8A3E => bool_list(cx, &v),
                Some(Reply::Ints(v)) if matches!(pname, 0x8A37 | 0x8A38) => {
                    num_array(cx, &v.iter().map(|x| *x as u32 as f64).collect::<Vec<_>>())
                }
                Some(Reply::Ints(v)) => {
                    num_array(cx, &v.iter().map(|x| *x as f64).collect::<Vec<_>>())
                }
                _ => null(cx),
            },
        )
    }
    fn get_uniform_block_index(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        uniform_block_name: &str,
    ) -> rt::Result<u32> {
        // INVALID_INDEX
        let Some(p) = self.gl2_program(this, program) else {
            return Ok(u32::MAX);
        };
        Ok(self
            .gl_query(
                this,
                Query::UniformBlockIndex(p, uniform_block_name.to_owned()),
            )
            .map_or(u32::MAX, |r| r.int() as u32))
    }
    fn get_active_uniform_block_parameter(
        &self,
        cx: &mut Cx,
        this: Handle,
        program: Handle,
        uniform_block_index: u32,
        pname: u32,
    ) -> rt::Result<JsValue> {
        let Some(p) = self.gl2_program(this, program) else {
            return Ok(null(cx));
        };
        // UNIFORM_BLOCK_BINDING, _DATA_SIZE, _ACTIVE_UNIFORMS, _ACTIVE_UNIFORM_INDICES,
        // _REFERENCED_BY_VERTEX_SHADER, _REFERENCED_BY_FRAGMENT_SHADER
        if !matches!(pname, 0x8A3F | 0x8A40 | 0x8A42 | 0x8A43 | 0x8A44 | 0x8A46) {
            self.gl_error(this, c::INVALID_ENUM);
            return Ok(null(cx));
        }
        Ok(
            match self.gl_query(
                this,
                Query::ActiveUniformBlockParameter(p, uniform_block_index, pname),
            ) {
                Some(Reply::Ints(v)) if pname == 0x8A43 => u32_array(cx, &v),
                Some(Reply::Ints(v)) if !v.is_empty() && matches!(pname, 0x8A44 | 0x8A46) => {
                    js(cx, &(v[0] != 0))
                }
                Some(Reply::Ints(v)) if !v.is_empty() => js(cx, &(v[0] as u32)),
                _ => null(cx),
            },
        )
    }
    fn get_active_uniform_block_name(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        uniform_block_index: u32,
    ) -> rt::Result<Option<DomStr<'_>>> {
        let Some(p) = self.gl2_program(this, program) else {
            return Ok(None);
        };
        Ok(
            match self.gl_query(this, Query::ActiveUniformBlockName(p, uniform_block_index)) {
                Some(Reply::String(Some(s))) => Some(DomStr::Owned(s)),
                _ => None,
            },
        )
    }
    fn uniform_block_binding(
        &self,
        _cx: &mut Cx,
        this: Handle,
        program: Handle,
        uniform_block_index: u32,
        uniform_block_binding: u32,
    ) -> rt::Result<()> {
        if let Some(p) = self.gl2_program(this, program) {
            self.gl_send(
                this,
                Command::UniformBlockBinding(p, uniform_block_index, uniform_block_binding),
            );
        }
        Ok(())
    }
    fn create_vertex_array(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.gl_create(this, Kind::VertexArray, 0))
    }
    fn delete_vertex_array(
        &self,
        _cx: &mut Cx,
        this: Handle,
        vertex_array: Option<Handle>,
    ) -> rt::Result<()> {
        self.gl_delete(this, vertex_array, Kind::VertexArray);
        Ok(())
    }
    fn is_vertex_array(
        &self,
        _cx: &mut Cx,
        this: Handle,
        vertex_array: Option<Handle>,
    ) -> rt::Result<bool> {
        Ok(self.gl_is(this, vertex_array, Kind::VertexArray))
    }
    /// `bindVertexArray`: the element array buffer and attribute buffers script reads back are
    /// the bound vertex array's.
    fn bind_vertex_array(
        &self,
        _cx: &mut Cx,
        this: Handle,
        array: Option<Handle>,
    ) -> rt::Result<()> {
        let Some(id) = self.gl2_bind(this, array, Kind::VertexArray) else {
            return Ok(());
        };
        let previous = self.gl_ctx(this, |c| c.gl2.vertex_array);
        if previous != array {
            let current = self.gl_ctx(this, |c| (c.element_buffer, c.attrib_buffers.clone()));
            match previous {
                Some(p) => {
                    self.gl_obj(p, |o| o.vao = Some(current));
                }
                None => self.gl_ctx(this, |c| c.gl2.default_vao = Some(current)),
            }
            let next = match array {
                Some(a) => self.gl_obj(a, |o| o.vao.clone()).flatten(),
                None => self.gl_ctx(this, |c| c.gl2.default_vao.take()),
            };
            self.gl_ctx(this, |c| {
                let (element, attribs) =
                    next.unwrap_or_else(|| (None, vec![None; c.attrib_buffers.len()]));
                c.element_buffer = element;
                c.attrib_buffers = attribs;
                c.gl2.vertex_array = array;
            });
        }
        self.gl_send(this, Command::BindVertexArray(id));
        Ok(())
    }
}

impl WebGL2RenderingContextOverloadsImpl for Engine {
    fn buffer_data_target_size_usage(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        size: i64,
        usage: u32,
    ) -> rt::Result<()> {
        WebGLRenderingContextOverloadsImpl::buffer_data_target_size_usage(
            self, cx, this, target, size, usage,
        )
    }
    fn buffer_data_target_src_data_usage(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        src_data: Option<BufferSource>,
        usage: u32,
    ) -> rt::Result<()> {
        WebGLRenderingContextOverloadsImpl::buffer_data_target_data_usage(
            self, cx, this, target, src_data, usage,
        )
    }
    fn buffer_data_target_src_data_usage_src_offset_length(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        src_data: View,
        usage: u32,
        src_offset: u64,
        length: u32,
    ) -> rt::Result<()> {
        let Some(bytes) = sub_view(cx, &src_data, src_offset, length as u64) else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        self.gl_send(this, Command::BufferData(target, 0, Some(bytes), usage));
        Ok(())
    }
    fn buffer_sub_data_target_dst_byte_offset_src_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        dst_byte_offset: i64,
        src_data: BufferSource,
    ) -> rt::Result<()> {
        WebGLRenderingContextOverloadsImpl::buffer_sub_data(
            self,
            cx,
            this,
            target,
            dst_byte_offset,
            src_data,
        )
    }
    fn buffer_sub_data_target_dst_byte_offset_src_data_src_offset_length(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        dst_byte_offset: i64,
        src_data: View,
        src_offset: u64,
        length: u32,
    ) -> rt::Result<()> {
        if dst_byte_offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        let Some(bytes) = sub_view(cx, &src_data, src_offset, length as u64) else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        self.gl_send(this, Command::BufferSubData(target, dst_byte_offset, bytes));
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
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        WebGLRenderingContextOverloadsImpl::tex_image2_d_target_level_internalformat_width_height_border_format_type_pixels(
            self, cx, this, target, level, internalformat, width, height, border, format, type_, pixels,
        )
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
        source: Source,
    ) -> rt::Result<()> {
        self.gl2_tex_source(cx, this, &source, format, type_, |w, h, bytes| {
            Command::TexImage2D(target, level, internalformat, w, h, 0, format, type_, bytes)
        })
    }
    fn tex_image2_d_target_level_internalformat_width_height_border_format_type_pbo_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        type_: u32,
        pbo_offset: i64,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, true) {
            return Ok(());
        }
        if pbo_offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            Command::TexImage2DOffset(
                target,
                level,
                internalformat,
                width,
                height,
                border,
                format,
                type_,
                pbo_offset,
            ),
        );
        Ok(())
    }
    fn tex_image2_d_target_level_internalformat_width_height_border_format_type_source(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: i32,
        _width: i32,
        _height: i32,
        border: i32,
        format: u32,
        type_: u32,
        source: Source,
    ) -> rt::Result<()> {
        // (the source at its own size; a sub-rectangle by width, height and UNPACK_SKIP_* is not
        // done yet)
        self.gl2_tex_source(cx, this, &source, format, type_, |w, h, bytes| {
            Command::TexImage2D(
                target,
                level,
                internalformat,
                w,
                h,
                border,
                format,
                type_,
                bytes,
            )
        })
    }
    fn tex_image2_d_target_level_internalformat_width_height_border_format_type_src_data_src_offset(
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
        src_data: View,
        src_offset: u64,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        let Some(mut bytes) = sub_view(cx, &src_data, src_offset, 0) else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        self.gl_unpack_view(this, &mut bytes, width, height, format, type_);
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
                Some(bytes),
            ),
        );
        Ok(())
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
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        WebGLRenderingContextOverloadsImpl::tex_sub_image2_d_target_level_xoffset_yoffset_width_height_format_type_pixels(
            self, cx, this, target, level, xoffset, yoffset, width, height, format, type_, pixels,
        )
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
        source: Source,
    ) -> rt::Result<()> {
        self.gl2_tex_source(cx, this, &source, format, type_, |w, h, bytes| {
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
    fn tex_sub_image2_d_target_level_xoffset_yoffset_width_height_format_type_pbo_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        pbo_offset: i64,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, true) {
            return Ok(());
        }
        if pbo_offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            Command::TexSubImage2DOffset(
                target, level, xoffset, yoffset, width, height, format, type_, pbo_offset,
            ),
        );
        Ok(())
    }
    fn tex_sub_image2_d_target_level_xoffset_yoffset_width_height_format_type_source(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        _width: i32,
        _height: i32,
        format: u32,
        type_: u32,
        source: Source,
    ) -> rt::Result<()> {
        self.gl2_tex_source(cx, this, &source, format, type_, |w, h, bytes| {
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
    fn tex_sub_image2_d_target_level_xoffset_yoffset_width_height_format_type_src_data_src_offset(
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
        src_data: View,
        src_offset: u64,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        let Some(mut bytes) = sub_view(cx, &src_data, src_offset, 0) else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        self.gl_unpack_view(this, &mut bytes, width, height, format, type_);
        self.gl_send(
            this,
            Command::TexSubImage2D(
                target, level, xoffset, yoffset, width, height, format, type_, bytes,
            ),
        );
        Ok(())
    }
    fn compressed_tex_image2_d_target_level_internalformat_width_height_border_image_size_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        _target: u32,
        _level: i32,
        _internalformat: u32,
        _width: i32,
        _height: i32,
        _border: i32,
        _image_size: i32,
        _offset: i64,
    ) -> rt::Result<()> {
        // (compressed uploads from a pixel unpack buffer are not done yet)
        self.gl_error(this, c::INVALID_OPERATION);
        Ok(())
    }
    fn compressed_tex_image2_d_target_level_internalformat_width_height_border_src_data_src_offset_src_length_override(
        &self,
        cx: &mut Cx,
        this: Handle,
        target: u32,
        level: i32,
        internalformat: u32,
        width: i32,
        height: i32,
        border: i32,
        src_data: View,
        src_offset: u64,
        src_length_override: u32,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        let Some(bytes) = sub_view(cx, &src_data, src_offset, src_length_override as u64) else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
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
    fn compressed_tex_sub_image2_d_target_level_xoffset_yoffset_width_height_format_image_size_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        _target: u32,
        _level: i32,
        _xoffset: i32,
        _yoffset: i32,
        _width: i32,
        _height: i32,
        _format: u32,
        _image_size: i32,
        _offset: i64,
    ) -> rt::Result<()> {
        self.gl_error(this, c::INVALID_OPERATION);
        Ok(())
    }
    fn compressed_tex_sub_image2_d_target_level_xoffset_yoffset_width_height_format_src_data_src_offset_src_length_override(
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
        src_data: View,
        src_offset: u64,
        src_length_override: u32,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_UNPACK_BUFFER, false) {
            return Ok(());
        }
        let Some(bytes) = sub_view(cx, &src_data, src_offset, src_length_override as u64) else {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        };
        self.gl_send(
            this,
            Command::CompressedTexSubImage2D(
                target, level, xoffset, yoffset, width, height, format, bytes,
            ),
        );
        Ok(())
    }
    fn uniform1fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(floats(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_v(this, l, 1, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform2fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(floats(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_v(this, l, 2, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform3fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(floats(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_v(this, l, 3, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform4fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(floats(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_v(this, l, 4, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform1iv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Int32ArrayOrLongSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(ints(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_iv(this, l, 1, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform2iv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Int32ArrayOrLongSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(ints(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_iv(this, l, 2, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform3iv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Int32ArrayOrLongSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(ints(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_iv(this, l, 3, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform4iv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        data: g::Int32ArrayOrLongSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        match sub_list(ints(cx, &data), src_offset, src_length) {
            Some(v) => self.uniform_iv(this, l, 4, v),
            None => self.gl_error(this, c::INVALID_VALUE),
        }
        Ok(())
    }
    fn uniform_matrix2fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        self.uniform_matrix2(cx, this, l, 2, 2, t, &data, src_offset, src_length);
        Ok(())
    }
    fn uniform_matrix3fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        self.uniform_matrix2(cx, this, l, 3, 3, t, &data, src_offset, src_length);
        Ok(())
    }
    fn uniform_matrix4fv(
        &self,
        cx: &mut Cx,
        this: Handle,
        l: Option<Handle>,
        t: bool,
        data: g::Float32ArrayOrUnrestrictedFloatSequence,
        src_offset: u64,
        src_length: u32,
    ) -> rt::Result<()> {
        self.uniform_matrix2(cx, this, l, 4, 4, t, &data, src_offset, src_length);
        Ok(())
    }
    fn read_pixels_x_y_width_height_format_type_dst_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        dst_data: Option<View>,
    ) -> rt::Result<()> {
        let Some(dst) = dst_data else {
            if self.gl2_pixel_buffer(this, PIXEL_PACK_BUFFER, false) {
                self.gl_error(this, c::INVALID_VALUE);
            }
            return Ok(());
        };
        self.gl2_read_pixels(cx, this, x, y, width, height, format, type_, &dst, 0);
        Ok(())
    }
    fn read_pixels_x_y_width_height_format_type_offset(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        offset: i64,
    ) -> rt::Result<()> {
        if !self.gl2_pixel_buffer(this, PIXEL_PACK_BUFFER, true) {
            return Ok(());
        }
        if offset < 0 {
            self.gl_error(this, c::INVALID_VALUE);
            return Ok(());
        }
        self.gl_send(
            this,
            Command::ReadPixelsOffset(x, y, width, height, format, type_, offset),
        );
        Ok(())
    }
    fn read_pixels_x_y_width_height_format_type_dst_data_dst_offset(
        &self,
        cx: &mut Cx,
        this: Handle,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        dst_data: View,
        dst_offset: u64,
    ) -> rt::Result<()> {
        self.gl2_read_pixels(
            cx, this, x, y, width, height, format, type_, &dst_data, dst_offset,
        );
        Ok(())
    }
}

impl g::WebGL2RenderingContextImpl for Engine {}
impl g::WebGLQueryImpl for Engine {}
impl g::WebGLSamplerImpl for Engine {}
impl g::WebGLSyncImpl for Engine {}
impl g::WebGLTransformFeedbackImpl for Engine {}
impl g::WebGLVertexArrayObjectImpl for Engine {}
