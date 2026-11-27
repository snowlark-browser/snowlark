// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The messages between a page's WebGL contexts (the client) and the service that runs them.
//!
//! Everything here is plain data, so the same messages can cross a process boundary. Objects are
//! named by ids the client allocates (never by the implementation's names, which the client never
//! sees); the service maps each id to its own object and ignores ids it does not know. Commands
//! that return nothing are batched and sent in order; a [`Query`] first runs the commands before it
//! and answers with a [`Reply`].

use serde::{Deserialize, Serialize};

/// A context, as the client numbers them.
pub type ContextId = u32;

/// WebGL 1.0 §5.2 `WebGLContextAttributes`, as asked for and as granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attributes {
    pub alpha: bool,
    pub depth: bool,
    pub stencil: bool,
    pub antialias: bool,
    pub premultiplied_alpha: bool,
    pub preserve_drawing_buffer: bool,
    /// 0 default, 1 low-power, 2 high-performance.
    pub power_preference: u8,
    pub fail_if_major_performance_caveat: bool,
    pub desynchronized: bool,
    /// A WebGL 2.0 context, on OpenGL ES 3.0 (not a dictionary member: the context type asked).
    pub webgl2: bool,
}

impl Default for Attributes {
    /// The dictionary's defaults.
    fn default() -> Self {
        Attributes {
            webgl2: false,
            alpha: true,
            depth: true,
            stencil: false,
            antialias: true,
            premultiplied_alpha: true,
            preserve_drawing_buffer: false,
            power_preference: 0,
            fail_if_major_performance_caveat: false,
            desynchronized: false,
        }
    }
}

/// What the service made: the attributes it granted and the limits the client validates with.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Created {
    pub attributes: Attributes,
    /// `MAX_TEXTURE_SIZE` and friends, read once.
    pub limits: Limits,
    /// The OpenGL ES extensions on in the new context (`GL_EXTENSIONS`).
    pub extensions: Vec<String>,
    /// The ones it can turn on ([`Command::RequestExtension`]; ANGLE's requestable extensions).
    pub requestable: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Limits {
    pub max_texture_size: i32,
    pub max_cube_map_texture_size: i32,
    pub max_renderbuffer_size: i32,
    pub max_viewport_dims: [i32; 2],
    pub max_vertex_attribs: i32,
    pub max_combined_texture_image_units: i32,
    pub max_texture_image_units: i32,
    /// OpenGL ES 3.0 only (0 in a WebGL 1.0 context).
    pub max_uniform_buffer_bindings: i32,
    pub max_transform_feedback_separate_attribs: i32,
    pub max_draw_buffers: i32,
    pub max_color_attachments: i32,
}

/// The kinds of objects the client names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    Buffer,
    Framebuffer,
    Program,
    Renderbuffer,
    Shader,
    Texture,
    // OpenGL ES 3.0:
    Query,
    Sampler,
    /// A fence (made by [`Command::FenceSync`], not [`Command::Create`]).
    Sync,
    TransformFeedback,
    VertexArray,
}

/// The pixels of a 3D upload: none (zeros), bytes, or an offset into the bound pixel unpack buffer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Pixels {
    None,
    Data(Vec<u8>),
    Offset(i64),
}

/// A command that returns nothing. Argument order follows the OpenGL ES 2.0 call; object
/// arguments are client ids (0: none).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Create(Kind, u32),
    /// A shader of the given type.
    CreateShader(u32, u32),
    Delete(Kind, u32),
    ActiveTexture(u32),
    AttachShader(u32, u32),
    BindAttribLocation(u32, u32, String),
    BindBuffer(u32, u32),
    BindFramebuffer(u32, u32),
    BindRenderbuffer(u32, u32),
    BindTexture(u32, u32),
    BlendColor([f32; 4]),
    BlendEquationSeparate(u32, u32),
    BlendFuncSeparate(u32, u32, u32, u32),
    /// target, size, data (none: `size` zero bytes), usage
    BufferData(u32, i64, Option<Vec<u8>>, u32),
    BufferSubData(u32, i64, Vec<u8>),
    Clear(u32),
    ClearColor([f32; 4]),
    ClearDepth(f32),
    ClearStencil(i32),
    ColorMask([bool; 4]),
    CompileShader(u32),
    /// target, level, internalformat, width, height, border, data
    CompressedTexImage2D(u32, i32, u32, i32, i32, i32, Vec<u8>),
    /// target, level, xoffset, yoffset, width, height, format, data
    CompressedTexSubImage2D(u32, i32, i32, i32, i32, i32, u32, Vec<u8>),
    /// target, level, internalformat, x, y, width, height, border
    CopyTexImage2D(u32, i32, u32, i32, i32, i32, i32, i32),
    /// target, level, xoffset, yoffset, x, y, width, height
    CopyTexSubImage2D(u32, i32, i32, i32, i32, i32, i32, i32),
    CullFace(u32),
    DepthFunc(u32),
    DepthMask(bool),
    DepthRange(f32, f32),
    DetachShader(u32, u32),
    Disable(u32),
    DisableVertexAttribArray(u32),
    DrawArrays(u32, i32, i32),
    /// mode, count, type, byte offset into the element array buffer
    DrawElements(u32, i32, u32, i64),
    Enable(u32),
    EnableVertexAttribArray(u32),
    Flush,
    FramebufferRenderbuffer(u32, u32, u32, u32),
    FramebufferTexture2D(u32, u32, u32, u32, i32),
    FrontFace(u32),
    GenerateMipmap(u32),
    Hint(u32, u32),
    LineWidth(f32),
    LinkProgram(u32),
    PixelStorei(u32, i32),
    PolygonOffset(f32, f32),
    RenderbufferStorage(u32, u32, i32, i32),
    SampleCoverage(f32, bool),
    Scissor(i32, i32, i32, i32),
    ShaderSource(u32, String),
    StencilFuncSeparate(u32, u32, i32, u32),
    StencilMaskSeparate(u32, u32),
    StencilOpSeparate(u32, u32, u32, u32),
    /// target, level, internalformat, width, height, border, format, type, data (none: zeros)
    TexImage2D(u32, i32, i32, i32, i32, i32, u32, u32, Option<Vec<u8>>),
    /// target, level, xoffset, yoffset, width, height, format, type, data
    TexSubImage2D(u32, i32, i32, i32, i32, i32, u32, u32, Vec<u8>),
    TexParameterf(u32, u32, f32),
    TexParameteri(u32, u32, i32),
    /// location (the service's, from [`Reply::Int`] of [`Query::UniformLocation`]), components
    /// per element (1 to 4), values
    Uniformf(i32, u8, Vec<f32>),
    Uniformi(i32, u8, Vec<i32>),
    /// location, columns (2 to 4), transpose, values
    UniformMatrix(i32, u8, bool, Vec<f32>),
    UseProgram(u32),
    ValidateProgram(u32),
    /// index, the four components (missing ones are 0, 0, 0, 1)
    VertexAttrib(u32, [f32; 4]),
    /// index, size, type, normalized, stride, byte offset into the array buffer
    VertexAttribPointer(u32, i32, u32, bool, i32, i64),
    Viewport(i32, i32, i32, i32),
    // OpenGL ES 3.0 (WebGL 2.0):
    BeginQuery(u32, u32),
    BeginTransformFeedback(u32),
    /// target, index, buffer
    BindBufferBase(u32, u32, u32),
    /// target, index, buffer, offset, size
    BindBufferRange(u32, u32, u32, i64, i64),
    BindSampler(u32, u32),
    BindTransformFeedback(u32, u32),
    BindVertexArray(u32),
    /// source and destination rectangles (x0, y0, x1, y1 each), mask, filter
    BlitFramebuffer([i32; 8], u32, u32),
    ClearBufferfv(u32, i32, [f32; 4]),
    ClearBufferiv(u32, i32, [i32; 4]),
    ClearBufferuiv(u32, i32, [u32; 4]),
    ClearBufferfi(u32, i32, f32, i32),
    /// target, level, internalformat, width, height, depth, border, data
    CompressedTexImage3D(u32, i32, u32, i32, i32, i32, i32, Vec<u8>),
    /// target, level, xoffset, yoffset, zoffset, width, height, depth, format, data
    CompressedTexSubImage3D(u32, i32, i32, i32, i32, i32, i32, i32, u32, Vec<u8>),
    /// read target, write target, read offset, write offset, size
    CopyBufferSubData(u32, u32, i64, i64, i64),
    /// target, level, xoffset, yoffset, zoffset, x, y, width, height
    CopyTexSubImage3D(u32, i32, i32, i32, i32, i32, i32, i32, i32),
    /// mode, first, count, instances
    DrawArraysInstanced(u32, i32, i32, i32),
    DrawBuffers(Vec<u32>),
    /// mode, count, type, byte offset, instances
    DrawElementsInstanced(u32, i32, u32, i64, i32),
    /// mode, start, end, count, type, byte offset
    DrawRangeElements(u32, u32, u32, i32, u32, i64),
    EndQuery(u32),
    EndTransformFeedback,
    /// A new fence with the client id (SYNC_GPU_COMMANDS_COMPLETE, no flags).
    FenceSync(u32),
    /// target, attachment, texture, level, layer
    FramebufferTextureLayer(u32, u32, u32, i32, i32),
    InvalidateFramebuffer(u32, Vec<u32>),
    /// target, attachments, x, y, width, height
    InvalidateSubFramebuffer(u32, Vec<u32>, [i32; 4]),
    PauseTransformFeedback,
    ReadBuffer(u32),
    /// x, y, width, height, format, type, offset into the bound pixel pack buffer
    ReadPixelsOffset(i32, i32, i32, i32, u32, u32, i64),
    /// target, samples, internalformat, width, height
    RenderbufferStorageMultisample(u32, i32, u32, i32, i32),
    ResumeTransformFeedback,
    SamplerParameteri(u32, u32, i32),
    SamplerParameterf(u32, u32, f32),
    /// target, level, internalformat, width, height, border, format, type, offset into the bound
    /// pixel unpack buffer
    TexImage2DOffset(u32, i32, i32, i32, i32, i32, u32, u32, i64),
    /// target, level, internalformat, width, height, depth, border, format, type, pixels
    TexImage3D(u32, i32, i32, i32, i32, i32, i32, u32, u32, Pixels),
    /// target, levels, internalformat, width, height
    TexStorage2D(u32, i32, u32, i32, i32),
    /// target, levels, internalformat, width, height, depth
    TexStorage3D(u32, i32, u32, i32, i32, i32),
    /// target, level, xoffset, yoffset, width, height, format, type, offset into the bound pixel
    /// unpack buffer
    TexSubImage2DOffset(u32, i32, i32, i32, i32, i32, u32, u32, i64),
    /// target, level, xoffset, yoffset, zoffset, width, height, depth, format, type, pixels
    TexSubImage3D(u32, i32, i32, i32, i32, i32, i32, i32, u32, u32, Pixels),
    /// program, varyings, buffer mode
    TransformFeedbackVaryings(u32, Vec<String>, u32),
    UniformBlockBinding(u32, u32, u32),
    /// location, components per element (1 to 4), values
    Uniformui(i32, u8, Vec<u32>),
    /// location, columns, rows (2 to 4 each, not equal), transpose, values
    UniformMatrixNxM(i32, u8, u8, bool, Vec<f32>),
    VertexAttribDivisor(u32, u32),
    VertexAttribI(u32, [i32; 4]),
    VertexAttribIu(u32, [u32; 4]),
    /// index, size, type, stride, byte offset into the array buffer
    VertexAttribIPointer(u32, i32, u32, i32, i64),
    /// A fence the server waits for (no flags, GL's TIMEOUT_IGNORED).
    WaitSync(u32),
    // Extensions (a WebGL extension's object turns its OpenGL ES extensions on first):
    /// Turns on one of [`Created::requestable`].
    RequestExtension(String),
    /// mode, firsts, counts, instance counts (none: not instanced); the lists have one length
    MultiDrawArrays(u32, Vec<i32>, Vec<i32>, Option<Vec<i32>>),
    /// mode, counts, type, byte offsets into the element array buffer, instance counts
    MultiDrawElements(u32, Vec<i32>, u32, Vec<i64>, Option<Vec<i32>>),
    ClipControl(u32, u32),
    PolygonMode(u32, u32),
    ProvokingVertex(u32),
    PolygonOffsetClamp(f32, f32, f32),
    /// target, index, enabled
    EnableIndexed(u32, u32, bool),
    /// draw buffer, RGB mode, alpha mode
    BlendEquationIndexed(u32, u32, u32),
    /// draw buffer, source RGB, destination RGB, source alpha, destination alpha
    BlendFuncIndexed(u32, [u32; 4]),
    ColorMaskIndexed(u32, [bool; 4]),
    /// target, attachment, texture, level, base view index, number of views
    FramebufferTextureMultiview(u32, u32, u32, i32, i32, i32),
}

/// A command that answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Query {
    /// `getError`: the oldest error the implementation recorded.
    Error,
    /// `glGetIntegerv` (pname, number of values).
    Integers(u32, u8),
    Floats(u32, u8),
    Booleans(u32, u8),
    /// `glGetString`.
    String(u32),
    IsEnabled(u32),
    CheckFramebufferStatus(u32),
    Shader(u32, u32),
    Program(u32, u32),
    ShaderInfoLog(u32),
    ProgramInfoLog(u32),
    ActiveAttrib(u32, u32),
    ActiveUniform(u32, u32),
    AttribLocation(u32, String),
    UniformLocation(u32, String),
    /// program, location, number of values, whether they are floats
    Uniform(u32, i32, u8, bool),
    /// index, pname, number of values (floats)
    VertexAttrib(u32, u32, u8),
    VertexAttribOffset(u32),
    TexParameter(u32, u32),
    BufferParameter(u32, u32),
    RenderbufferParameter(u32, u32),
    FramebufferAttachmentParameter(u32, u32, u32),
    ShaderPrecisionFormat(u32, u32),
    /// x, y, width, height, format, type, the byte length the client can take
    ReadPixels(i32, i32, i32, i32, u32, u32, u32),
    Finish,
    /// Whether the context was lost (a graphics reset): `glGetGraphicsResetStatus`.
    ResetStatus,
    // OpenGL ES 3.0 (WebGL 2.0):
    /// `glGetInteger64v` (pname, number of values).
    Integers64(u32, u8),
    /// `glGetInteger64i_v` (target, index).
    IndexedInteger64(u32, u32),
    /// fence, flags, timeout in nanoseconds: the status `glClientWaitSync` returns.
    ClientWaitSync(u32, u32, u64),
    SyncParameter(u32, u32),
    QueryParameter(u32, u32),
    /// sampler, pname, whether the value is a float
    SamplerParameter(u32, u32, bool),
    /// target, offset, length: the bytes of the bound buffer.
    BufferSubData(u32, i64, u32),
    UniformIndices(u32, Vec<String>),
    /// program, uniform indices, pname
    ActiveUniforms(u32, Vec<u32>, u32),
    UniformBlockIndex(u32, String),
    /// program, block index, pname
    ActiveUniformBlockParameter(u32, u32, u32),
    ActiveUniformBlockName(u32, u32),
    FragDataLocation(u32, String),
    TransformFeedbackVarying(u32, u32),
    /// target, internalformat, pname
    InternalformatParameter(u32, u32, u32),
    /// program, location, number of values: unsigned values (as their bits)
    UniformU(u32, i32, u8),
    /// index, pname, unsigned: an integer attribute's current value
    VertexAttribI(u32, u32, bool),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    None,
    Int(i32),
    Ints(Vec<i32>),
    Floats(Vec<f32>),
    Bools(Vec<bool>),
    String(Option<String>),
    /// size, type, name
    ActiveInfo(Option<(i32, u32, String)>),
    /// rangeMin, rangeMax, precision
    Precision([i32; 3]),
    Bytes(Vec<u8>),
    Int64s(Vec<i64>),
}

impl Reply {
    pub fn int(&self) -> i32 {
        match self {
            Reply::Int(v) => *v,
            Reply::Ints(v) => v.first().copied().unwrap_or(0),
            _ => 0,
        }
    }
}

/// The drawing buffer's pixels for the page's rendering: RGBA, rows bottom to top as OpenGL reads
/// them, with the alpha the context's `premultipliedAlpha` says.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Readback {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Textures of another process (the browser's, which draws them) that a context's frames can be
/// presented into instead of read back: each a `width` × `height` BGRA texture with a keyed mutex,
/// as NT handles. A texture is written with its key at 0 and given up with key 1; the other side
/// gives it back with key 0 once it no longer shows it.
#[cfg(windows)]
pub struct SharedTargets {
    pub width: u32,
    pub height: u32,
    pub handles: Vec<std::os::windows::io::OwnedHandle>,
}

/// What [`Backend::present_shared`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedPresent {
    /// The drawing buffer is in target `index` (rows bottom to top, as OpenGL stores them; its
    /// alpha as the page wrote it, premultiplied), given up with key 1.
    Presented(u32),
    /// Every target is still the other side's: nothing was presented and the drawing buffer is as
    /// it was (it was not composited, so it is not cleared either).
    Busy,
    /// No targets, or they cannot take this context's frames: present with [`Backend::present`].
    Unavailable,
}

/// Where a page's contexts run: on its process's WebGL thread today ([`crate::Service`]), across a
/// process boundary later. Every call is one message.
pub trait Backend {
    /// A context with a drawing buffer of `width` × `height` (at least 1 × 1).
    fn create_context(
        &mut self,
        id: ContextId,
        attributes: &Attributes,
        width: u32,
        height: u32,
    ) -> Result<Created, String>;
    /// A new drawing buffer of the size (its contents are cleared; state is kept).
    fn resize(&mut self, id: ContextId, width: u32, height: u32);
    fn destroy(&mut self, id: ContextId);
    /// Commands in order, each for its context.
    fn submit(&mut self, batch: Vec<(ContextId, Command)>);
    fn query(&mut self, id: ContextId, query: &Query) -> Reply;
    /// The drawing buffer for the rendering; then, unless the context preserves it, the buffer is
    /// cleared (WebGL 1.0 §2.2).
    fn present(&mut self, id: ContextId) -> Option<Readback>;
    /// The context's frames go into `targets` from now on (`None`: no longer). Whether they were
    /// opened for it: they are of the drawing buffer's size, and the context is one whose frames
    /// can be copied into them (a WebGL 2.0 context with premultiplied alpha). A new size of the
    /// drawing buffer lets them go.
    #[cfg(windows)]
    fn share_into(&mut self, _id: ContextId, _targets: Option<SharedTargets>) -> bool {
        false
    }
    /// [`Backend::present`] into a free texture of [`Backend::share_into`]'s, with no readback:
    /// the drawing buffer is copied on the GPU (its samples resolved) and, unless the context
    /// preserves it, cleared.
    fn present_shared(&mut self, _id: ContextId) -> SharedPresent {
        SharedPresent::Unavailable
    }
    /// The last frame [`Backend::present_shared`] presented, read back as [`Backend::present`]
    /// gives it (a copy the context keeps of each: a drawing buffer that is not preserved was
    /// cleared after it). For a canvas whose frames leave the shared textures.
    fn read_presented(&mut self, _id: ContextId) -> Option<Readback> {
        None
    }
}
