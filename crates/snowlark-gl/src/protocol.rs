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
}

impl Default for Attributes {
    /// The dictionary's defaults.
    fn default() -> Self {
        Attributes {
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
}
