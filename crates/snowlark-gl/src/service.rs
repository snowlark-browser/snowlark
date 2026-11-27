// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The service: runs the protocol's commands on ANGLE.
//!
//! Each context is an OpenGL ES 2.0 context (3.0 for WebGL 2.0) created in ANGLE's WebGL
//! compatibility mode, so ANGLE
//! applies WebGL's own validation rules and translates and validates shaders as WebGL requires;
//! with robust buffer access (no out-of-bounds reads from buffers), robust resource initialization
//! (new textures, buffers and drawing buffers read as zero), no client-side arrays, no implicit
//! object creation on bind, and lose-on-reset so a graphics reset is reported rather than hidden.
//! The drawing buffer is a pbuffer surface of the canvas's size, so framebuffer 0 is the drawing
//! buffer. Every call that reads or writes memory of the caller goes through ANGLE's robust
//! client memory entry points, which take the buffer's size.

use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::ptr::{null, null_mut};

use crate::ffi::*;
use crate::protocol::*;

#[cfg(windows)]
mod shared;

// OpenGL ES 2.0 enums the service uses itself.
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
const GL_PACK_ALIGNMENT: u32 = 0x0D05;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_DEPTH_BUFFER_BIT: u32 = 0x0100;
const GL_STENCIL_BUFFER_BIT: u32 = 0x0400;
const GL_SCISSOR_TEST: u32 = 0x0C11;
const GL_COLOR_CLEAR_VALUE: u32 = 0x0C22;
const GL_COLOR_WRITEMASK: u32 = 0x0C23;
const GL_DEPTH_CLEAR_VALUE: u32 = 0x0B73;
const GL_DEPTH_WRITEMASK: u32 = 0x0B72;
const GL_STENCIL_CLEAR_VALUE: u32 = 0x0B91;
const GL_STENCIL_WRITEMASK: u32 = 0x0B98;
const GL_STENCIL_BACK_WRITEMASK: u32 = 0x8CA5;
const GL_FRONT: u32 = 0x0404;
const GL_BACK: u32 = 0x0405;
const GL_INFO_LOG_LENGTH: u32 = 0x8B84;
const GL_ACTIVE_UNIFORM_MAX_LENGTH: u32 = 0x8B87;
const GL_ACTIVE_ATTRIBUTE_MAX_LENGTH: u32 = 0x8B8A;
const GL_VERTEX_ATTRIB_ARRAY_POINTER: u32 = 0x8645;
const GL_MAX_TEXTURE_SIZE: u32 = 0x0D33;
const GL_MAX_CUBE_MAP_TEXTURE_SIZE: u32 = 0x851C;
const GL_MAX_RENDERBUFFER_SIZE: u32 = 0x84E8;
const GL_MAX_VIEWPORT_DIMS: u32 = 0x0D3A;
const GL_MAX_VERTEX_ATTRIBS: u32 = 0x8869;
const GL_MAX_COMBINED_TEXTURE_IMAGE_UNITS: u32 = 0x8B4D;
const GL_MAX_TEXTURE_IMAGE_UNITS: u32 = 0x8872;
/// The largest readback the service makes for a page (bytes).
const MAX_READ: u32 = 1 << 30;
// OpenGL ES 3.0 state the service saves around its own drawing buffer work.
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_READ_FRAMEBUFFER_BINDING: u32 = 0x8CAA;
const GL_PIXEL_PACK_BUFFER: u32 = 0x88EB;
const GL_PIXEL_PACK_BUFFER_BINDING: u32 = 0x88ED;
const GL_PACK_ROW_LENGTH: u32 = 0x0D02;
const GL_PACK_SKIP_ROWS: u32 = 0x0D03;
const GL_PACK_SKIP_PIXELS: u32 = 0x0D04;
const GL_READ_BUFFER: u32 = 0x0C02;
const GL_RASTERIZER_DISCARD: u32 = 0x8C89;
const GL_MAP_READ_BIT: u32 = 0x0001;
const GL_UNIFORM_BLOCK_NAME_LENGTH: u32 = 0x8A41;
const GL_UNIFORM_BLOCK_ACTIVE_UNIFORMS: u32 = 0x8A42;
const GL_UNIFORM_BLOCK_ACTIVE_UNIFORM_INDICES: u32 = 0x8A43;
const GL_TRANSFORM_FEEDBACK_VARYING_MAX_LENGTH: u32 = 0x8C76;
const GL_NUM_SAMPLE_COUNTS: u32 = 0x9380;
const GL_SAMPLES: u32 = 0x80A9;

/// The length and pointer ANGLE's robust calls take for a 3D upload's pixels (an offset into the
/// bound pixel unpack buffer is passed as the pointer, with a size of 0).
fn pixels_arg(px: &Pixels) -> (i32, *const c_void) {
    match px {
        Pixels::None => (0, null()),
        Pixels::Data(d) => (d.len() as i32, d.as_ptr().cast()),
        Pixels::Offset(o) => (0, *o as usize as *const c_void),
    }
}

struct Context {
    context: EGLContext,
    surface: EGLSurface,
    config: EGLConfig,
    attributes: Attributes,
    width: u32,
    height: u32,
    /// Client ids to ANGLE's names.
    names: HashMap<(Kind, u32), u32>,
    /// Client ids of fences to ANGLE's sync objects (pointers, kept as addresses).
    syncs: HashMap<u32, usize>,
    /// A page error the service read before work of its own, for the page's next `getError`.
    held_error: u32,
    /// The shared textures the context presents into ([`Backend::share_into`]).
    #[cfg(windows)]
    shared: Option<shared::Targets>,
}

/// The displays [`Device::new`] tries, in order: each an attribute list for
/// `eglGetPlatformDisplayEXT`, ending in `EGL_NONE`.
fn display_attributes(warp: bool) -> Vec<Vec<EGLint>> {
    if cfg!(windows) {
        let mut attrs = vec![
            EGL_PLATFORM_ANGLE_TYPE_ANGLE,
            EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE,
        ];
        if warp {
            attrs.extend([
                EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE,
                EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE,
            ]);
        }
        attrs.push(EGL_NONE);
        return vec![attrs];
    }
    [
        EGL_PLATFORM_ANGLE_TYPE_OPENGL_ANGLE,
        EGL_PLATFORM_ANGLE_TYPE_OPENGLES_ANGLE,
    ]
    .into_iter()
    .map(|kind| {
        vec![
            EGL_PLATFORM_ANGLE_TYPE_ANGLE,
            kind,
            EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE,
            EGL_PLATFORM_ANGLE_DEVICE_TYPE_EGL_ANGLE,
            EGL_PLATFORM_ANGLE_NATIVE_PLATFORM_TYPE_ANGLE,
            EGL_PLATFORM_SURFACELESS_MESA,
            EGL_NONE,
        ]
    })
    .collect()
}

/// Opens and initializes ANGLE's display of `attrs`.
fn open_display(angle: &Angle, attrs: &[EGLint]) -> Result<EGLDisplay, String> {
    let e = &angle.egl;
    // SAFETY: the attribute list ends in EGL_NONE; EGL_DEFAULT_DISPLAY is null.
    let display = unsafe {
        (e.eglGetPlatformDisplayEXT)(EGL_PLATFORM_ANGLE_ANGLE, null_mut(), attrs.as_ptr())
    };
    if display.is_null() {
        return Err(format!("ANGLE has no display of {attrs:#x?}"));
    }
    let (mut major, mut minor) = (0, 0);
    // SAFETY: a display from eglGetPlatformDisplayEXT; out pointers are locals.
    if unsafe { (e.eglInitialize)(display, &mut major, &mut minor) } == 0 {
        // SAFETY: no arguments.
        return Err(format!("eglInitialize failed: {:#x}", unsafe {
            (e.eglGetError)()
        }));
    }
    Ok(display)
}

/// ANGLE on one device (Direct3D 11 on Windows, the system's OpenGL on Linux), and the contexts
/// on it. Used from one thread only: the
/// process's WebGL thread ([`crate::Service`]).
pub(crate) struct Device {
    angle: Angle,
    display: EGLDisplay,
    contexts: HashMap<ContextId, Context>,
    current: Option<ContextId>,
}

impl Device {
    /// Loads ANGLE and opens its display. On Windows: the graphics card through Direct3D 11, or
    /// WARP (Direct3D's software rasterizer) when `warp`. On Linux: the system's OpenGL through its
    /// EGL on no window system (the drawing buffers are pbuffers), desktop OpenGL first, else
    /// OpenGL ES; the system's driver is the software one where there is no graphics card, so
    /// `warp` asks nothing more.
    pub(crate) fn new(warp: bool) -> Result<Device, String> {
        let angle = load()?;
        let mut last = String::new();
        for attrs in display_attributes(warp) {
            match open_display(&angle, &attrs) {
                Ok(display) => {
                    // SAFETY: a constant enum.
                    unsafe { (angle.egl.eglBindAPI)(EGL_OPENGL_ES_API) };
                    return Ok(Device {
                        angle,
                        display,
                        contexts: HashMap::new(),
                        current: None,
                    });
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Another display made its context current on this thread: this one's is not any more.
    pub(crate) fn forget_current(&mut self) {
        self.current = None;
    }

    fn config_attrib(&self, config: EGLConfig, attr: EGLint) -> EGLint {
        let mut v = 0;
        // SAFETY: a config of this display; the out pointer is a local.
        unsafe { (self.angle.egl.eglGetConfigAttrib)(self.display, config, attr, &mut v) };
        v
    }

    /// The config closest to what the attributes ask: RGB 8 bits, alpha exactly as asked, a depth
    /// buffer when asked (24 bits before 16), stencil when asked, 4 samples for antialiasing when
    /// there is such a config. Lower scores are better; `None` when nothing fits at all.
    fn choose_config(&self, a: &Attributes) -> Option<EGLConfig> {
        let e = &self.angle.egl;
        let mut count = 0;
        // SAFETY: a null array asks for the count.
        unsafe { (e.eglGetConfigs)(self.display, null_mut(), 0, &mut count) };
        let mut configs = vec![null_mut(); count.max(0) as usize];
        // SAFETY: the array holds `count` configs.
        unsafe { (e.eglGetConfigs)(self.display, configs.as_mut_ptr(), count, &mut count) };
        configs.truncate(count.max(0) as usize);
        let mut best: Option<(u32, EGLConfig)> = None;
        for c in configs {
            let attr = |x| self.config_attrib(c, x);
            let api = if a.webgl2 {
                EGL_OPENGL_ES3_BIT
            } else {
                EGL_OPENGL_ES2_BIT
            };
            if attr(EGL_SURFACE_TYPE) & EGL_PBUFFER_BIT == 0
                || attr(EGL_RENDERABLE_TYPE) & api == 0
                || attr(EGL_RED_SIZE) != 8
                || attr(EGL_GREEN_SIZE) != 8
                || attr(EGL_BLUE_SIZE) != 8
            {
                continue;
            }
            let alpha = attr(EGL_ALPHA_SIZE);
            if (alpha > 0) != a.alpha || (a.alpha && alpha != 8) {
                continue;
            }
            let (depth, stencil, samples) = (
                attr(EGL_DEPTH_SIZE),
                attr(EGL_STENCIL_SIZE),
                attr(EGL_SAMPLES),
            );
            let mut score = 0u32;
            if a.depth {
                score += match depth {
                    24 => 0,
                    16 => 10,
                    0 => continue,
                    _ => 20,
                };
            } else if depth > 0 {
                score += 100;
            }
            if a.stencil {
                if stencil == 0 {
                    continue;
                }
            } else if stencil > 0 {
                // (a packed depth-stencil buffer when only depth is asked for)
                score += if a.depth { 1 } else { 100 };
            }
            score += match (a.antialias, samples) {
                (true, 4) | (false, 0) => 0,
                (true, 0) => 50,
                (true, _) => 5,
                (false, _) => 1000,
            };
            if best.is_none_or(|(s, _)| score < s) {
                best = Some((score, c));
            }
        }
        best.map(|(_, c)| c)
    }

    fn pbuffer(&self, config: EGLConfig, width: u32, height: u32) -> EGLSurface {
        let attrs = [
            EGL_WIDTH,
            width.clamp(1, 16384) as i32,
            EGL_HEIGHT,
            height.clamp(1, 16384) as i32,
            EGL_NONE,
        ];
        // SAFETY: a config of this display; the list ends in EGL_NONE.
        unsafe { (self.angle.egl.eglCreatePbufferSurface)(self.display, config, attrs.as_ptr()) }
    }

    /// Makes the context current. Whether it exists.
    fn make_current(&mut self, id: ContextId) -> bool {
        if self.current == Some(id) {
            return true;
        }
        let Some(c) = self.contexts.get(&id) else {
            return false;
        };
        // SAFETY: the surface and context belong to this display and are alive.
        let ok = unsafe {
            (self.angle.egl.eglMakeCurrent)(self.display, c.surface, c.surface, c.context)
        } != 0;
        self.current = ok.then_some(id);
        ok
    }

    fn name(&self, id: ContextId, kind: Kind, client: u32) -> u32 {
        if client == 0 {
            return 0;
        }
        self.contexts
            .get(&id)
            .and_then(|c| c.names.get(&(kind, client)).copied())
            .unwrap_or(0)
    }

    fn get_ints(&self, pname: u32, n: usize) -> Vec<i32> {
        let mut v = [0i32; 16];
        let n = n.min(16);
        let mut len = 0;
        // SAFETY: the buffer holds `n` values and ANGLE is told so.
        unsafe {
            (self.angle.gl.glGetIntegervRobustANGLE)(pname, n as i32, &mut len, v.as_mut_ptr())
        };
        v[..(len.clamp(0, n as i32) as usize)].to_vec()
    }

    fn get_floats(&self, pname: u32, n: usize) -> Vec<f32> {
        let mut v = [0f32; 16];
        let n = n.min(16);
        let mut len = 0;
        // SAFETY: as `get_ints`.
        unsafe {
            (self.angle.gl.glGetFloatvRobustANGLE)(pname, n as i32, &mut len, v.as_mut_ptr())
        };
        v[..(len.clamp(0, n as i32) as usize)].to_vec()
    }

    fn get_bools(&self, pname: u32, n: usize) -> Vec<bool> {
        let mut v = [0u8; 16];
        let n = n.min(16);
        let mut len = 0;
        // SAFETY: as `get_ints`.
        unsafe {
            (self.angle.gl.glGetBooleanvRobustANGLE)(pname, n as i32, &mut len, v.as_mut_ptr())
        };
        v[..(len.clamp(0, n as i32) as usize)]
            .iter()
            .map(|b| *b != 0)
            .collect()
    }

    /// The names in an extension string (`GL_EXTENSIONS` or ANGLE's requestable ones) of the
    /// current context.
    fn extension_list(&self, pname: u32) -> Vec<String> {
        // SAFETY: the context is current; the string ANGLE returns lives as long as the context
        // and is read before any other call.
        let s = unsafe { (self.angle.gl.glGetString)(pname) };
        if s.is_null() {
            return Vec::new();
        }
        // SAFETY: a NUL-terminated string from ANGLE.
        let s = unsafe { std::ffi::CStr::from_ptr(s.cast()) }.to_string_lossy();
        s.split_ascii_whitespace().map(str::to_owned).collect()
    }

    fn limits(&self, es3: bool) -> Limits {
        let one = |p| self.get_ints(p, 1).first().copied().unwrap_or(0);
        let three = |p| if es3 { one(p) } else { 0 };
        let dims = self.get_ints(GL_MAX_VIEWPORT_DIMS, 2);
        Limits {
            max_uniform_buffer_bindings: three(0x8A2F),
            max_transform_feedback_separate_attribs: three(0x8C8B),
            max_draw_buffers: three(0x8824),
            max_color_attachments: three(0x8CDF),
            max_texture_size: one(GL_MAX_TEXTURE_SIZE),
            max_cube_map_texture_size: one(GL_MAX_CUBE_MAP_TEXTURE_SIZE),
            max_renderbuffer_size: one(GL_MAX_RENDERBUFFER_SIZE),
            max_viewport_dims: [
                dims.first().copied().unwrap_or(0),
                dims.get(1).copied().unwrap_or(0),
            ],
            max_vertex_attribs: one(GL_MAX_VERTEX_ATTRIBS),
            max_combined_texture_image_units: one(GL_MAX_COMBINED_TEXTURE_IMAGE_UNITS),
            max_texture_image_units: one(GL_MAX_TEXTURE_IMAGE_UNITS),
        }
    }

    fn execute(&mut self, id: ContextId, cmd: &Command) {
        use Command as C;
        let gl = &self.angle.gl;
        let n = |kind, client| self.name(id, kind, client);
        let b = |v: bool| v as u8;
        let es2 = self.contexts.get(&id).is_some_and(|c| !c.attributes.webgl2);
        // SAFETY (every call below): the context is current; enums and integers are passed
        // through for ANGLE to validate; object names come from this context's table (unknown ids
        // become 0); pointers are to memory this function owns for the call, with its size
        // passed where the call reads or writes through it; offsets into buffers are not
        // pointers because client-side arrays are disabled in the context.
        unsafe {
            match cmd {
                C::Create(kind, client) => {
                    let mut name = 0;
                    match kind {
                        Kind::Buffer => (gl.glGenBuffers)(1, &mut name),
                        Kind::Framebuffer => (gl.glGenFramebuffers)(1, &mut name),
                        Kind::Renderbuffer => (gl.glGenRenderbuffers)(1, &mut name),
                        Kind::Texture => (gl.glGenTextures)(1, &mut name),
                        Kind::Program => name = (gl.glCreateProgram)(),
                        Kind::Query => (gl.glGenQueries)(1, &mut name),
                        Kind::Sampler => (gl.glGenSamplers)(1, &mut name),
                        Kind::TransformFeedback => (gl.glGenTransformFeedbacks)(1, &mut name),
                        Kind::VertexArray if es2 => (gl.glGenVertexArraysOES)(1, &mut name),
                        Kind::VertexArray => (gl.glGenVertexArrays)(1, &mut name),
                        Kind::Shader | Kind::Sync => {}
                    }
                    if name != 0 {
                        if let Some(c) = self.contexts.get_mut(&id) {
                            c.names.insert((*kind, *client), name);
                        }
                    }
                }
                C::CreateShader(client, ty) => {
                    let name = (gl.glCreateShader)(*ty);
                    if name != 0 {
                        if let Some(c) = self.contexts.get_mut(&id) {
                            c.names.insert((Kind::Shader, *client), name);
                        }
                    }
                }
                C::Delete(Kind::Sync, client) => {
                    if let Some(s) = self
                        .contexts
                        .get_mut(&id)
                        .and_then(|c| c.syncs.remove(client))
                    {
                        (gl.glDeleteSync)(s as *const c_void);
                    }
                }
                C::Delete(kind, client) => {
                    let Some(name) = self
                        .contexts
                        .get_mut(&id)
                        .and_then(|c| c.names.remove(&(*kind, *client)))
                    else {
                        return;
                    };
                    match kind {
                        Kind::Buffer => (gl.glDeleteBuffers)(1, &name),
                        Kind::Framebuffer => (gl.glDeleteFramebuffers)(1, &name),
                        Kind::Renderbuffer => (gl.glDeleteRenderbuffers)(1, &name),
                        Kind::Texture => (gl.glDeleteTextures)(1, &name),
                        Kind::Program => (gl.glDeleteProgram)(name),
                        Kind::Shader => (gl.glDeleteShader)(name),
                        Kind::Query => (gl.glDeleteQueries)(1, &name),
                        Kind::Sampler => (gl.glDeleteSamplers)(1, &name),
                        Kind::TransformFeedback => (gl.glDeleteTransformFeedbacks)(1, &name),
                        Kind::VertexArray if es2 => (gl.glDeleteVertexArraysOES)(1, &name),
                        Kind::VertexArray => (gl.glDeleteVertexArrays)(1, &name),
                        Kind::Sync => {}
                    }
                }
                C::ActiveTexture(t) => (gl.glActiveTexture)(*t),
                C::AttachShader(p, s) => {
                    (gl.glAttachShader)(n(Kind::Program, *p), n(Kind::Shader, *s))
                }
                C::BindAttribLocation(p, index, name) => {
                    if let Ok(name) = CString::new(name.as_str()) {
                        (gl.glBindAttribLocation)(n(Kind::Program, *p), *index, name.as_ptr())
                    }
                }
                C::BindBuffer(t, o) => (gl.glBindBuffer)(*t, n(Kind::Buffer, *o)),
                C::BindFramebuffer(t, o) => (gl.glBindFramebuffer)(*t, n(Kind::Framebuffer, *o)),
                C::BindRenderbuffer(t, o) => (gl.glBindRenderbuffer)(*t, n(Kind::Renderbuffer, *o)),
                C::BindTexture(t, o) => (gl.glBindTexture)(*t, n(Kind::Texture, *o)),
                C::BlendColor([r, g, bl, a]) => (gl.glBlendColor)(*r, *g, *bl, *a),
                C::BlendEquationSeparate(x, y) => (gl.glBlendEquationSeparate)(*x, *y),
                C::BlendFuncSeparate(a, bb, c, d) => (gl.glBlendFuncSeparate)(*a, *bb, *c, *d),
                C::BufferData(t, size, data, usage) => match data {
                    Some(d) => (gl.glBufferData)(*t, d.len() as isize, d.as_ptr().cast(), *usage),
                    None => (gl.glBufferData)(*t, (*size).max(0) as isize, null(), *usage),
                },
                C::BufferSubData(t, offset, d) => {
                    (gl.glBufferSubData)(*t, *offset as isize, d.len() as isize, d.as_ptr().cast())
                }
                C::Clear(mask) => (gl.glClear)(*mask),
                C::ClearColor([r, g, bl, a]) => (gl.glClearColor)(*r, *g, *bl, *a),
                C::ClearDepth(d) => (gl.glClearDepthf)(*d),
                C::ClearStencil(s) => (gl.glClearStencil)(*s),
                C::ColorMask([r, g, bl, a]) => (gl.glColorMask)(b(*r), b(*g), b(*bl), b(*a)),
                C::CompileShader(s) => (gl.glCompileShader)(n(Kind::Shader, *s)),
                C::CompressedTexImage2D(t, level, f, w, h, border, d) => (gl
                    .glCompressedTexImage2D)(
                    *t,
                    *level,
                    *f,
                    *w,
                    *h,
                    *border,
                    d.len() as i32,
                    d.as_ptr().cast(),
                ),
                C::CompressedTexSubImage2D(t, level, x, y, w, h, f, d) => (gl
                    .glCompressedTexSubImage2D)(
                    *t,
                    *level,
                    *x,
                    *y,
                    *w,
                    *h,
                    *f,
                    d.len() as i32,
                    d.as_ptr().cast(),
                ),
                C::CopyTexImage2D(t, level, f, x, y, w, h, border) => {
                    (gl.glCopyTexImage2D)(*t, *level, *f, *x, *y, *w, *h, *border)
                }
                C::CopyTexSubImage2D(t, level, xo, yo, x, y, w, h) => {
                    (gl.glCopyTexSubImage2D)(*t, *level, *xo, *yo, *x, *y, *w, *h)
                }
                C::CullFace(m) => (gl.glCullFace)(*m),
                C::DepthFunc(f) => (gl.glDepthFunc)(*f),
                C::DepthMask(f) => (gl.glDepthMask)(b(*f)),
                C::DepthRange(a, bb) => (gl.glDepthRangef)(*a, *bb),
                C::DetachShader(p, s) => {
                    (gl.glDetachShader)(n(Kind::Program, *p), n(Kind::Shader, *s))
                }
                C::Disable(cap) => (gl.glDisable)(*cap),
                C::DisableVertexAttribArray(i) => (gl.glDisableVertexAttribArray)(*i),
                C::DrawArrays(m, first, count) => (gl.glDrawArrays)(*m, *first, *count),
                C::DrawElements(m, count, ty, offset) => {
                    (gl.glDrawElements)(*m, *count, *ty, *offset as usize as *const c_void)
                }
                C::Enable(cap) => (gl.glEnable)(*cap),
                C::EnableVertexAttribArray(i) => (gl.glEnableVertexAttribArray)(*i),
                C::Flush => (gl.glFlush)(),
                C::FramebufferRenderbuffer(t, a, rt, r) => {
                    (gl.glFramebufferRenderbuffer)(*t, *a, *rt, n(Kind::Renderbuffer, *r))
                }
                C::FramebufferTexture2D(t, a, tt, tex, level) => {
                    (gl.glFramebufferTexture2D)(*t, *a, *tt, n(Kind::Texture, *tex), *level)
                }
                C::FrontFace(m) => (gl.glFrontFace)(*m),
                C::GenerateMipmap(t) => (gl.glGenerateMipmap)(*t),
                C::Hint(t, m) => (gl.glHint)(*t, *m),
                C::LineWidth(w) => (gl.glLineWidth)(*w),
                C::LinkProgram(p) => (gl.glLinkProgram)(n(Kind::Program, *p)),
                C::PixelStorei(p, v) => (gl.glPixelStorei)(*p, *v),
                C::PolygonOffset(f, u) => (gl.glPolygonOffset)(*f, *u),
                C::RenderbufferStorage(t, f, w, h) => (gl.glRenderbufferStorage)(*t, *f, *w, *h),
                C::SampleCoverage(v, i) => (gl.glSampleCoverage)(*v, b(*i)),
                C::Scissor(x, y, w, h) => (gl.glScissor)(*x, *y, *w, *h),
                C::ShaderSource(s, src) => {
                    let ptr = src.as_ptr().cast::<i8>();
                    let len = src.len().min(i32::MAX as usize) as i32;
                    (gl.glShaderSource)(n(Kind::Shader, *s), 1, &ptr, &len)
                }
                C::StencilFuncSeparate(face, f, r, m) => {
                    (gl.glStencilFuncSeparate)(*face, *f, *r, *m)
                }
                C::StencilMaskSeparate(face, m) => (gl.glStencilMaskSeparate)(*face, *m),
                C::StencilOpSeparate(face, a, bb, c) => {
                    (gl.glStencilOpSeparate)(*face, *a, *bb, *c)
                }
                C::TexImage2D(t, level, ifmt, w, h, border, f, ty, d) => {
                    let (len, ptr) = match d {
                        Some(d) => (d.len() as i32, d.as_ptr().cast()),
                        None => (0, null()),
                    };
                    (gl.glTexImage2DRobustANGLE)(
                        *t, *level, *ifmt, *w, *h, *border, *f, *ty, len, ptr,
                    )
                }
                C::TexSubImage2D(t, level, x, y, w, h, f, ty, d) => {
                    (gl.glTexSubImage2DRobustANGLE)(
                        *t,
                        *level,
                        *x,
                        *y,
                        *w,
                        *h,
                        *f,
                        *ty,
                        d.len() as i32,
                        d.as_ptr().cast(),
                    )
                }
                C::TexParameterf(t, p, v) => (gl.glTexParameterf)(*t, *p, *v),
                C::TexParameteri(t, p, v) => (gl.glTexParameteri)(*t, *p, *v),
                C::Uniformf(loc, size, v) => {
                    let size = *size as usize;
                    if !(1..=4).contains(&size) || v.is_empty() || v.len() % size != 0 {
                        return;
                    }
                    let count = (v.len() / size) as i32;
                    let f = [
                        gl.glUniform1fv,
                        gl.glUniform2fv,
                        gl.glUniform3fv,
                        gl.glUniform4fv,
                    ];
                    (f[size - 1])(*loc, count, v.as_ptr())
                }
                C::Uniformi(loc, size, v) => {
                    let size = *size as usize;
                    if !(1..=4).contains(&size) || v.is_empty() || v.len() % size != 0 {
                        return;
                    }
                    let count = (v.len() / size) as i32;
                    let f = [
                        gl.glUniform1iv,
                        gl.glUniform2iv,
                        gl.glUniform3iv,
                        gl.glUniform4iv,
                    ];
                    (f[size - 1])(*loc, count, v.as_ptr())
                }
                C::UniformMatrix(loc, cols, transpose, v) => {
                    let cols = *cols as usize;
                    if !(2..=4).contains(&cols) || v.is_empty() || v.len() % (cols * cols) != 0 {
                        return;
                    }
                    let count = (v.len() / (cols * cols)) as i32;
                    let f = [
                        gl.glUniformMatrix2fv,
                        gl.glUniformMatrix3fv,
                        gl.glUniformMatrix4fv,
                    ];
                    (f[cols - 2])(*loc, count, b(*transpose), v.as_ptr())
                }
                C::UseProgram(p) => (gl.glUseProgram)(n(Kind::Program, *p)),
                C::ValidateProgram(p) => (gl.glValidateProgram)(n(Kind::Program, *p)),
                C::VertexAttrib(i, v) => (gl.glVertexAttrib4fv)(*i, v.as_ptr()),
                C::VertexAttribPointer(i, size, ty, norm, stride, offset) => (gl
                    .glVertexAttribPointer)(
                    *i,
                    *size,
                    *ty,
                    b(*norm),
                    *stride,
                    *offset as usize as *const c_void,
                ),
                C::Viewport(x, y, w, h) => (gl.glViewport)(*x, *y, *w, *h),
                C::BeginQuery(t, q) => (gl.glBeginQuery)(*t, n(Kind::Query, *q)),
                C::BeginTransformFeedback(m) => (gl.glBeginTransformFeedback)(*m),
                C::BindBufferBase(t, i, bb) => (gl.glBindBufferBase)(*t, *i, n(Kind::Buffer, *bb)),
                C::BindBufferRange(t, i, bb, o, s) => {
                    (gl.glBindBufferRange)(*t, *i, n(Kind::Buffer, *bb), *o as isize, *s as isize)
                }
                C::BindSampler(u, s) => (gl.glBindSampler)(*u, n(Kind::Sampler, *s)),
                C::BindTransformFeedback(t, tf) => {
                    (gl.glBindTransformFeedback)(*t, n(Kind::TransformFeedback, *tf))
                }
                C::BindVertexArray(v) if es2 => (gl.glBindVertexArrayOES)(n(Kind::VertexArray, *v)),
                C::BindVertexArray(v) => (gl.glBindVertexArray)(n(Kind::VertexArray, *v)),
                C::BlitFramebuffer(r, mask, filter) => (gl.glBlitFramebuffer)(
                    r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], *mask, *filter,
                ),
                C::ClearBufferfv(bf, db, v) => (gl.glClearBufferfv)(*bf, *db, v.as_ptr()),
                C::ClearBufferiv(bf, db, v) => (gl.glClearBufferiv)(*bf, *db, v.as_ptr()),
                C::ClearBufferuiv(bf, db, v) => (gl.glClearBufferuiv)(*bf, *db, v.as_ptr()),
                C::ClearBufferfi(bf, db, d, s) => (gl.glClearBufferfi)(*bf, *db, *d, *s),
                C::CompressedTexImage3D(t, level, f, w, h, d, border, data) => (gl
                    .glCompressedTexImage3D)(
                    *t,
                    *level,
                    *f,
                    *w,
                    *h,
                    *d,
                    *border,
                    data.len() as i32,
                    data.as_ptr().cast(),
                ),
                C::CompressedTexSubImage3D(t, level, x, y, z, w, h, d, f, data) => (gl
                    .glCompressedTexSubImage3D)(
                    *t,
                    *level,
                    *x,
                    *y,
                    *z,
                    *w,
                    *h,
                    *d,
                    *f,
                    data.len() as i32,
                    data.as_ptr().cast(),
                ),
                C::CopyBufferSubData(r, w, ro, wo, s) => {
                    (gl.glCopyBufferSubData)(*r, *w, *ro as isize, *wo as isize, *s as isize)
                }
                C::CopyTexSubImage3D(t, level, xo, yo, zo, x, y, w, h) => {
                    (gl.glCopyTexSubImage3D)(*t, *level, *xo, *yo, *zo, *x, *y, *w, *h)
                }
                // In an OpenGL ES 2.0 context (WebGL 1.0) these come from extensions, whose entry
                // points ANGLE keeps apart from ES 3.0's.
                C::DrawArraysInstanced(m, first, count, inst) if es2 => {
                    (gl.glDrawArraysInstancedANGLE)(*m, *first, *count, *inst)
                }
                C::DrawArraysInstanced(m, first, count, inst) => {
                    (gl.glDrawArraysInstanced)(*m, *first, *count, *inst)
                }
                C::DrawBuffers(v) if es2 => (gl.glDrawBuffersEXT)(v.len() as i32, v.as_ptr()),
                C::DrawBuffers(v) => (gl.glDrawBuffers)(v.len() as i32, v.as_ptr()),
                C::DrawElementsInstanced(m, count, ty, offset, inst) if es2 => (gl
                    .glDrawElementsInstancedANGLE)(
                    *m,
                    *count,
                    *ty,
                    *offset as usize as *const c_void,
                    *inst,
                ),
                C::DrawElementsInstanced(m, count, ty, offset, inst) => (gl
                    .glDrawElementsInstanced)(
                    *m,
                    *count,
                    *ty,
                    *offset as usize as *const c_void,
                    *inst,
                ),
                C::DrawRangeElements(m, start, end, count, ty, offset) => (gl.glDrawRangeElements)(
                    *m,
                    *start,
                    *end,
                    *count,
                    *ty,
                    *offset as usize as *const c_void,
                ),
                C::EndQuery(t) => (gl.glEndQuery)(*t),
                C::EndTransformFeedback => (gl.glEndTransformFeedback)(),
                C::FenceSync(client) => {
                    // (SYNC_GPU_COMMANDS_COMPLETE, the one condition; no flags)
                    let s = (gl.glFenceSync)(0x9117, 0);
                    if !s.is_null() {
                        if let Some(c) = self.contexts.get_mut(&id) {
                            if let Some(old) = c.syncs.insert(*client, s as usize) {
                                (gl.glDeleteSync)(old as *const c_void);
                            }
                        }
                    }
                }
                C::FramebufferTextureLayer(t, a, tex, level, layer) => {
                    (gl.glFramebufferTextureLayer)(*t, *a, n(Kind::Texture, *tex), *level, *layer)
                }
                C::InvalidateFramebuffer(t, v) => {
                    (gl.glInvalidateFramebuffer)(*t, v.len() as i32, v.as_ptr())
                }
                C::InvalidateSubFramebuffer(t, v, [x, y, w, h]) => {
                    (gl.glInvalidateSubFramebuffer)(*t, v.len() as i32, v.as_ptr(), *x, *y, *w, *h)
                }
                C::PauseTransformFeedback => (gl.glPauseTransformFeedback)(),
                C::ReadBuffer(s) => (gl.glReadBuffer)(*s),
                C::ReadPixelsOffset(x, y, w, h, f, ty, offset) => {
                    // (with no pixel pack buffer bound, a buffer of size 0: ANGLE refuses the call
                    // rather than writing to the offset as an address)
                    let (mut written, mut cols, mut rows) = (0, 0, 0);
                    (gl.glReadPixelsRobustANGLE)(
                        *x,
                        *y,
                        *w,
                        *h,
                        *f,
                        *ty,
                        0,
                        &mut written,
                        &mut cols,
                        &mut rows,
                        *offset as usize as *mut c_void,
                    )
                }
                C::RenderbufferStorageMultisample(t, s, f, w, h) => {
                    (gl.glRenderbufferStorageMultisample)(*t, *s, *f, *w, *h)
                }
                C::ResumeTransformFeedback => (gl.glResumeTransformFeedback)(),
                C::SamplerParameteri(s, p, v) => {
                    (gl.glSamplerParameteri)(n(Kind::Sampler, *s), *p, *v)
                }
                C::SamplerParameterf(s, p, v) => {
                    (gl.glSamplerParameterf)(n(Kind::Sampler, *s), *p, *v)
                }
                // (an offset with no pixel unpack buffer bound: a buffer of size 0, refused as
                // above)
                C::TexImage2DOffset(t, level, ifmt, w, h, border, f, ty, offset) => (gl
                    .glTexImage2DRobustANGLE)(
                    *t,
                    *level,
                    *ifmt,
                    *w,
                    *h,
                    *border,
                    *f,
                    *ty,
                    0,
                    *offset as usize as *const c_void,
                ),
                C::TexSubImage2DOffset(t, level, x, y, w, h, f, ty, offset) => (gl
                    .glTexSubImage2DRobustANGLE)(
                    *t,
                    *level,
                    *x,
                    *y,
                    *w,
                    *h,
                    *f,
                    *ty,
                    0,
                    *offset as usize as *const c_void,
                ),
                C::TexImage3D(t, level, ifmt, w, h, d, border, f, ty, px) => {
                    let (len, ptr) = pixels_arg(px);
                    (gl.glTexImage3DRobustANGLE)(
                        *t, *level, *ifmt, *w, *h, *d, *border, *f, *ty, len, ptr,
                    )
                }
                C::TexSubImage3D(t, level, x, y, z, w, h, d, f, ty, px) => {
                    let (len, ptr) = pixels_arg(px);
                    (gl.glTexSubImage3DRobustANGLE)(
                        *t, *level, *x, *y, *z, *w, *h, *d, *f, *ty, len, ptr,
                    )
                }
                C::TexStorage2D(t, levels, f, w, h) => (gl.glTexStorage2D)(*t, *levels, *f, *w, *h),
                C::TexStorage3D(t, levels, f, w, h, d) => {
                    (gl.glTexStorage3D)(*t, *levels, *f, *w, *h, *d)
                }
                C::TransformFeedbackVaryings(p, names, mode) => {
                    let names: Vec<CString> = names
                        .iter()
                        .filter_map(|s| CString::new(s.as_str()).ok())
                        .collect();
                    let ptrs: Vec<*const i8> = names.iter().map(|s| s.as_ptr()).collect();
                    (gl.glTransformFeedbackVaryings)(
                        n(Kind::Program, *p),
                        ptrs.len() as i32,
                        ptrs.as_ptr(),
                        *mode,
                    )
                }
                C::UniformBlockBinding(p, i, bnd) => {
                    (gl.glUniformBlockBinding)(n(Kind::Program, *p), *i, *bnd)
                }
                C::Uniformui(loc, size, v) => {
                    let size = *size as usize;
                    if !(1..=4).contains(&size) || v.is_empty() || v.len() % size != 0 {
                        return;
                    }
                    let count = (v.len() / size) as i32;
                    let f = [
                        gl.glUniform1uiv,
                        gl.glUniform2uiv,
                        gl.glUniform3uiv,
                        gl.glUniform4uiv,
                    ];
                    (f[size - 1])(*loc, count, v.as_ptr())
                }
                C::UniformMatrixNxM(loc, cols, rows, transpose, v) => {
                    let f = match (*cols, *rows) {
                        (2, 3) => gl.glUniformMatrix2x3fv,
                        (2, 4) => gl.glUniformMatrix2x4fv,
                        (3, 2) => gl.glUniformMatrix3x2fv,
                        (3, 4) => gl.glUniformMatrix3x4fv,
                        (4, 2) => gl.glUniformMatrix4x2fv,
                        (4, 3) => gl.glUniformMatrix4x3fv,
                        _ => return,
                    };
                    let per = *cols as usize * *rows as usize;
                    if v.is_empty() || v.len() % per != 0 {
                        return;
                    }
                    f(*loc, (v.len() / per) as i32, b(*transpose), v.as_ptr())
                }
                C::VertexAttribDivisor(i, d) if es2 => (gl.glVertexAttribDivisorANGLE)(*i, *d),
                C::VertexAttribDivisor(i, d) => (gl.glVertexAttribDivisor)(*i, *d),
                C::VertexAttribI(i, v) => (gl.glVertexAttribI4iv)(*i, v.as_ptr()),
                C::VertexAttribIu(i, v) => (gl.glVertexAttribI4uiv)(*i, v.as_ptr()),
                C::VertexAttribIPointer(i, size, ty, stride, offset) => {
                    (gl.glVertexAttribIPointer)(
                        *i,
                        *size,
                        *ty,
                        *stride,
                        *offset as usize as *const c_void,
                    )
                }
                C::WaitSync(s) => {
                    if let Some(s) = self.contexts.get(&id).and_then(|c| c.syncs.get(s)) {
                        // (TIMEOUT_IGNORED)
                        (gl.glWaitSync)(*s as *const c_void, 0, u64::MAX)
                    }
                }
                C::RequestExtension(name) => {
                    if let Ok(name) = std::ffi::CString::new(name.as_str()) {
                        (gl.glRequestExtensionANGLE)(name.as_ptr())
                    }
                }
                C::MultiDrawArrays(m, firsts, counts, inst) => {
                    let len = firsts.len().min(counts.len());
                    match inst {
                        Some(i) => (gl.glMultiDrawArraysInstancedANGLE)(
                            *m,
                            firsts.as_ptr(),
                            counts.as_ptr(),
                            i.as_ptr(),
                            len.min(i.len()) as i32,
                        ),
                        None => (gl.glMultiDrawArraysANGLE)(
                            *m,
                            firsts.as_ptr(),
                            counts.as_ptr(),
                            len as i32,
                        ),
                    }
                }
                C::MultiDrawElements(m, counts, ty, offsets, inst) => {
                    let ptrs: Vec<*const c_void> = offsets
                        .iter()
                        .map(|o| *o as usize as *const c_void)
                        .collect();
                    let len = counts.len().min(ptrs.len());
                    match inst {
                        Some(i) => (gl.glMultiDrawElementsInstancedANGLE)(
                            *m,
                            counts.as_ptr(),
                            *ty,
                            ptrs.as_ptr(),
                            i.as_ptr(),
                            len.min(i.len()) as i32,
                        ),
                        None => (gl.glMultiDrawElementsANGLE)(
                            *m,
                            counts.as_ptr(),
                            *ty,
                            ptrs.as_ptr(),
                            len as i32,
                        ),
                    }
                }
                C::ClipControl(o, d) => (gl.glClipControlEXT)(*o, *d),
                C::PolygonMode(f, m) => (gl.glPolygonModeANGLE)(*f, *m),
                C::ProvokingVertex(m) => (gl.glProvokingVertexANGLE)(*m),
                C::PolygonOffsetClamp(f, u, c) => (gl.glPolygonOffsetClampEXT)(*f, *u, *c),
                C::EnableIndexed(t, i, true) => (gl.glEnableiOES)(*t, *i),
                C::EnableIndexed(t, i, false) => (gl.glDisableiOES)(*t, *i),
                C::BlendEquationIndexed(buf, rgb, a) => {
                    (gl.glBlendEquationSeparateiOES)(*buf, *rgb, *a)
                }
                C::BlendFuncIndexed(buf, f) => {
                    (gl.glBlendFuncSeparateiOES)(*buf, f[0], f[1], f[2], f[3])
                }
                C::ColorMaskIndexed(buf, m) => {
                    (gl.glColorMaskiOES)(*buf, b(m[0]), b(m[1]), b(m[2]), b(m[3]))
                }
                C::FramebufferTextureMultiview(t, a, tex, level, base, views) => (gl
                    .glFramebufferTextureMultiviewOVR)(
                    *t,
                    *a,
                    n(Kind::Texture, *tex),
                    *level,
                    *base,
                    *views,
                ),
            }
        }
    }

    fn info_log(&self, program: bool, name: u32) -> Option<String> {
        let gl = &self.angle.gl;
        let mut len = 0;
        let mut written = 0;
        // SAFETY: one value into a local, with its size.
        unsafe {
            if program {
                (gl.glGetProgramivRobustANGLE)(name, GL_INFO_LOG_LENGTH, 1, &mut written, &mut len)
            } else {
                (gl.glGetShaderivRobustANGLE)(name, GL_INFO_LOG_LENGTH, 1, &mut written, &mut len)
            }
        }
        if written != 1 {
            return None;
        }
        let mut buf = vec![0u8; len.clamp(1, 1 << 24) as usize];
        let mut out = 0;
        // SAFETY: the buffer holds `buf.len()` bytes and ANGLE is told so.
        unsafe {
            if program {
                (gl.glGetProgramInfoLog)(name, buf.len() as i32, &mut out, buf.as_mut_ptr().cast())
            } else {
                (gl.glGetShaderInfoLog)(name, buf.len() as i32, &mut out, buf.as_mut_ptr().cast())
            }
        }
        buf.truncate(out.clamp(0, buf.len() as i32) as usize);
        Some(String::from_utf8_lossy(&buf).into_owned())
    }

    fn active_info(&self, program: u32, index: u32, uniform: bool) -> Option<(i32, u32, String)> {
        let gl = &self.angle.gl;
        let pname = if uniform {
            GL_ACTIVE_UNIFORM_MAX_LENGTH
        } else {
            GL_ACTIVE_ATTRIBUTE_MAX_LENGTH
        };
        let (mut max, mut written) = (0, 0);
        // SAFETY: one value into a local, with its size.
        unsafe { (gl.glGetProgramivRobustANGLE)(program, pname, 1, &mut written, &mut max) };
        let mut buf = vec![0u8; max.clamp(1, 1 << 16) as usize];
        let (mut len, mut size, mut ty) = (0, 0, 0u32);
        // SAFETY: the name buffer holds `buf.len()` bytes and ANGLE is told so.
        unsafe {
            let f = if uniform {
                gl.glGetActiveUniform
            } else {
                gl.glGetActiveAttrib
            };
            f(
                program,
                index,
                buf.len() as i32,
                &mut len,
                &mut size,
                &mut ty,
                buf.as_mut_ptr().cast(),
            )
        };
        if ty == 0 {
            return None;
        }
        buf.truncate(len.clamp(0, buf.len() as i32) as usize);
        Some((size, ty, String::from_utf8_lossy(&buf).into_owned()))
    }

    fn answer(&mut self, id: ContextId, q: &Query) -> Reply {
        use Query as Q;
        if matches!(q, Q::Error) {
            // (an error of the page's the service read before its own work comes first)
            let held = self
                .contexts
                .get_mut(&id)
                .map_or(0, |c| std::mem::take(&mut c.held_error));
            if held != 0 {
                return Reply::Int(held as i32);
            }
        }
        let gl = &self.angle.gl;
        let n = |kind, client| self.name(id, kind, client);
        // SAFETY (every call below): the context is current; out pointers are locals or buffers
        // whose size is passed with them.
        unsafe {
            match q {
                Q::Error => Reply::Int((gl.glGetError)() as i32),
                Q::Integers(p, count) => Reply::Ints(self.get_ints(*p, *count as usize)),
                Q::Floats(p, count) => Reply::Floats(self.get_floats(*p, *count as usize)),
                Q::Booleans(p, count) => Reply::Bools(self.get_bools(*p, *count as usize)),
                Q::String(p) => {
                    let s = (gl.glGetString)(*p);
                    Reply::String((!s.is_null()).then(|| {
                        std::ffi::CStr::from_ptr(s.cast())
                            .to_string_lossy()
                            .into_owned()
                    }))
                }
                Q::IsEnabled(cap) => Reply::Int(((gl.glIsEnabled)(*cap) != 0) as i32),
                Q::CheckFramebufferStatus(t) => {
                    Reply::Int((gl.glCheckFramebufferStatus)(*t) as i32)
                }
                Q::Shader(s, p) => {
                    let (mut v, mut len) = (0, 0);
                    (gl.glGetShaderivRobustANGLE)(n(Kind::Shader, *s), *p, 1, &mut len, &mut v);
                    if len == 1 {
                        Reply::Int(v)
                    } else {
                        Reply::None
                    }
                }
                Q::Program(prog, p) => {
                    let (mut v, mut len) = (0, 0);
                    (gl.glGetProgramivRobustANGLE)(
                        n(Kind::Program, *prog),
                        *p,
                        1,
                        &mut len,
                        &mut v,
                    );
                    if len == 1 {
                        Reply::Int(v)
                    } else {
                        Reply::None
                    }
                }
                Q::ShaderInfoLog(s) => Reply::String(self.info_log(false, n(Kind::Shader, *s))),
                Q::ProgramInfoLog(p) => Reply::String(self.info_log(true, n(Kind::Program, *p))),
                Q::ActiveAttrib(p, i) => {
                    Reply::ActiveInfo(self.active_info(n(Kind::Program, *p), *i, false))
                }
                Q::ActiveUniform(p, i) => {
                    Reply::ActiveInfo(self.active_info(n(Kind::Program, *p), *i, true))
                }
                Q::AttribLocation(p, name) => match CString::new(name.as_str()) {
                    Ok(name) => Reply::Int((gl.glGetAttribLocation)(
                        n(Kind::Program, *p),
                        name.as_ptr(),
                    )),
                    Err(_) => Reply::Int(-1),
                },
                Q::UniformLocation(p, name) => match CString::new(name.as_str()) {
                    Ok(name) => Reply::Int((gl.glGetUniformLocation)(
                        n(Kind::Program, *p),
                        name.as_ptr(),
                    )),
                    Err(_) => Reply::Int(-1),
                },
                Q::Uniform(p, loc, count, float) => {
                    let count = (*count as usize).min(16);
                    let mut len = 0;
                    if *float {
                        let mut v = [0f32; 16];
                        (gl.glGetUniformfvRobustANGLE)(
                            n(Kind::Program, *p),
                            *loc,
                            (count * 4) as i32,
                            &mut len,
                            v.as_mut_ptr(),
                        );
                        Reply::Floats(v[..len.clamp(0, count as i32) as usize].to_vec())
                    } else {
                        let mut v = [0i32; 16];
                        (gl.glGetUniformivRobustANGLE)(
                            n(Kind::Program, *p),
                            *loc,
                            (count * 4) as i32,
                            &mut len,
                            v.as_mut_ptr(),
                        );
                        Reply::Ints(v[..len.clamp(0, count as i32) as usize].to_vec())
                    }
                }
                Q::VertexAttrib(i, p, count) => {
                    let count = (*count as usize).min(4);
                    let mut v = [0f32; 4];
                    let mut len = 0;
                    (gl.glGetVertexAttribfvRobustANGLE)(
                        *i,
                        *p,
                        count as i32,
                        &mut len,
                        v.as_mut_ptr(),
                    );
                    Reply::Floats(v[..len.clamp(0, count as i32) as usize].to_vec())
                }
                Q::VertexAttribOffset(i) => {
                    let mut p = null_mut();
                    (gl.glGetVertexAttribPointerv)(*i, GL_VERTEX_ATTRIB_ARRAY_POINTER, &mut p);
                    Reply::Int(p as usize as i32)
                }
                Q::TexParameter(t, p) => {
                    let (mut v, mut len) = (0, 0);
                    (gl.glGetTexParameterivRobustANGLE)(*t, *p, 1, &mut len, &mut v);
                    if len == 1 {
                        Reply::Int(v)
                    } else {
                        Reply::None
                    }
                }
                Q::BufferParameter(t, p) => {
                    let (mut v, mut len) = (0, 0);
                    (gl.glGetBufferParameterivRobustANGLE)(*t, *p, 1, &mut len, &mut v);
                    if len == 1 {
                        Reply::Int(v)
                    } else {
                        Reply::None
                    }
                }
                Q::RenderbufferParameter(t, p) => {
                    let (mut v, mut len) = (0, 0);
                    (gl.glGetRenderbufferParameterivRobustANGLE)(*t, *p, 1, &mut len, &mut v);
                    if len == 1 {
                        Reply::Int(v)
                    } else {
                        Reply::None
                    }
                }
                Q::FramebufferAttachmentParameter(t, a, p) => {
                    let (mut v, mut len) = (0, 0);
                    (gl.glGetFramebufferAttachmentParameterivRobustANGLE)(
                        *t, *a, *p, 1, &mut len, &mut v,
                    );
                    if len == 1 {
                        Reply::Int(v)
                    } else {
                        Reply::None
                    }
                }
                Q::ShaderPrecisionFormat(s, p) => {
                    let mut range = [0i32; 2];
                    let mut precision = 0;
                    (gl.glGetShaderPrecisionFormat)(*s, *p, range.as_mut_ptr(), &mut precision);
                    Reply::Precision([range[0], range[1], precision])
                }
                Q::ReadPixels(x, y, w, h, f, ty, len) => {
                    let mut buf = vec![0u8; (*len).min(MAX_READ) as usize];
                    let (mut written, mut cols, mut rows) = (0, 0, 0);
                    (gl.glReadPixelsRobustANGLE)(
                        *x,
                        *y,
                        *w,
                        *h,
                        *f,
                        *ty,
                        buf.len() as i32,
                        &mut written,
                        &mut cols,
                        &mut rows,
                        buf.as_mut_ptr().cast(),
                    );
                    if written <= 0 {
                        return Reply::None;
                    }
                    Reply::Bytes(buf)
                }
                Q::Finish => {
                    (gl.glFinish)();
                    Reply::None
                }
                Q::ResetStatus => Reply::Int((gl.glGetGraphicsResetStatusEXT)() as i32),
                Q::Integers64(p, count) => {
                    let count = (*count as usize).min(16);
                    let mut v = [0i64; 16];
                    let mut len = 0;
                    (gl.glGetInteger64vRobustANGLE)(*p, count as i32, &mut len, v.as_mut_ptr());
                    Reply::Int64s(v[..len.clamp(0, count as i32) as usize].to_vec())
                }
                Q::IndexedInteger64(t, i) => {
                    let (mut v, mut len) = (0i64, 0);
                    (gl.glGetInteger64i_vRobustANGLE)(*t, *i, 1, &mut len, &mut v);
                    if len == 1 {
                        Reply::Int64s(vec![v])
                    } else {
                        Reply::None
                    }
                }
                Q::ClientWaitSync(s, flags, timeout) => {
                    match self.contexts.get(&id).and_then(|c| c.syncs.get(s)) {
                        Some(s) => {
                            Reply::Int(
                                (gl.glClientWaitSync)(*s as *const c_void, *flags, *timeout) as i32
                            )
                        }
                        None => Reply::None,
                    }
                }
                Q::SyncParameter(s, p) => {
                    match self.contexts.get(&id).and_then(|c| c.syncs.get(s)) {
                        Some(s) => {
                            let (mut v, mut len) = (0, 0);
                            (gl.glGetSynciv)(*s as *const c_void, *p, 1, &mut len, &mut v);
                            if len == 1 {
                                Reply::Int(v)
                            } else {
                                Reply::None
                            }
                        }
                        None => Reply::None,
                    }
                }
                Q::QueryParameter(q, p) => {
                    let (mut v, mut len) = (0u32, 0);
                    (gl.glGetQueryObjectuivRobustANGLE)(
                        n(Kind::Query, *q),
                        *p,
                        1,
                        &mut len,
                        &mut v,
                    );
                    if len == 1 {
                        Reply::Int(v as i32)
                    } else {
                        Reply::None
                    }
                }
                Q::SamplerParameter(s, p, float) => {
                    let mut len = 0;
                    if *float {
                        let mut v = 0f32;
                        (gl.glGetSamplerParameterfvRobustANGLE)(
                            n(Kind::Sampler, *s),
                            *p,
                            1,
                            &mut len,
                            &mut v,
                        );
                        if len == 1 {
                            Reply::Floats(vec![v])
                        } else {
                            Reply::None
                        }
                    } else {
                        let mut v = 0;
                        (gl.glGetSamplerParameterivRobustANGLE)(
                            n(Kind::Sampler, *s),
                            *p,
                            1,
                            &mut len,
                            &mut v,
                        );
                        if len == 1 {
                            Reply::Int(v)
                        } else {
                            Reply::None
                        }
                    }
                }
                Q::BufferSubData(t, offset, len) => {
                    let len = (*len).min(MAX_READ);
                    if len == 0 {
                        return Reply::Bytes(Vec::new());
                    }
                    let p =
                        (gl.glMapBufferRange)(*t, *offset as isize, len as isize, GL_MAP_READ_BIT);
                    if p.is_null() {
                        return Reply::None;
                    }
                    // SAFETY: a successful read mapping of `len` bytes, read before it is unmapped.
                    let bytes = std::slice::from_raw_parts(p as *const u8, len as usize).to_vec();
                    (gl.glUnmapBuffer)(*t);
                    Reply::Bytes(bytes)
                }
                Q::UniformIndices(p, names) => {
                    let names: Vec<CString> = names
                        .iter()
                        .map(|s| CString::new(s.as_str()).unwrap_or_default())
                        .collect();
                    let ptrs: Vec<*const i8> = names.iter().map(|s| s.as_ptr()).collect();
                    let mut out = vec![0u32; ptrs.len()];
                    (gl.glGetUniformIndices)(
                        n(Kind::Program, *p),
                        ptrs.len() as i32,
                        ptrs.as_ptr(),
                        out.as_mut_ptr(),
                    );
                    Reply::Ints(out.into_iter().map(|v| v as i32).collect())
                }
                Q::ActiveUniforms(p, indices, pname) => {
                    // (sentinel values: a refused call leaves them, and is answered with nothing)
                    let mut out = vec![i32::MIN; indices.len()];
                    (gl.glGetActiveUniformsiv)(
                        n(Kind::Program, *p),
                        indices.len() as i32,
                        indices.as_ptr(),
                        *pname,
                        out.as_mut_ptr(),
                    );
                    if out.contains(&i32::MIN) {
                        Reply::None
                    } else {
                        Reply::Ints(out)
                    }
                }
                Q::UniformBlockIndex(p, name) => match CString::new(name.as_str()) {
                    Ok(name) => Reply::Int((gl.glGetUniformBlockIndex)(
                        n(Kind::Program, *p),
                        name.as_ptr(),
                    ) as i32),
                    Err(_) => Reply::Int(-1),
                },
                Q::ActiveUniformBlockParameter(p, i, pname) => {
                    let prog = n(Kind::Program, *p);
                    let count = if *pname == GL_UNIFORM_BLOCK_ACTIVE_UNIFORM_INDICES {
                        let (mut v, mut len) = (0, 0);
                        (gl.glGetActiveUniformBlockivRobustANGLE)(
                            prog,
                            *i,
                            GL_UNIFORM_BLOCK_ACTIVE_UNIFORMS,
                            1,
                            &mut len,
                            &mut v,
                        );
                        v.clamp(0, 1 << 16) as usize
                    } else {
                        1
                    };
                    let mut out = vec![0i32; count.max(1)];
                    let mut len = 0;
                    (gl.glGetActiveUniformBlockivRobustANGLE)(
                        prog,
                        *i,
                        *pname,
                        out.len() as i32,
                        &mut len,
                        out.as_mut_ptr(),
                    );
                    if len <= 0 && count > 0 {
                        return Reply::None;
                    }
                    out.truncate(len.clamp(0, out.len() as i32) as usize);
                    Reply::Ints(out)
                }
                Q::ActiveUniformBlockName(p, i) => {
                    let prog = n(Kind::Program, *p);
                    let (mut max, mut len) = (0, 0);
                    (gl.glGetActiveUniformBlockivRobustANGLE)(
                        prog,
                        *i,
                        GL_UNIFORM_BLOCK_NAME_LENGTH,
                        1,
                        &mut len,
                        &mut max,
                    );
                    if len != 1 {
                        return Reply::String(None);
                    }
                    let mut buf = vec![0u8; max.clamp(1, 1 << 16) as usize];
                    let mut out = 0;
                    (gl.glGetActiveUniformBlockName)(
                        prog,
                        *i,
                        buf.len() as i32,
                        &mut out,
                        buf.as_mut_ptr().cast(),
                    );
                    buf.truncate(out.clamp(0, buf.len() as i32) as usize);
                    Reply::String(Some(String::from_utf8_lossy(&buf).into_owned()))
                }
                Q::FragDataLocation(p, name) => match CString::new(name.as_str()) {
                    Ok(name) => Reply::Int((gl.glGetFragDataLocation)(
                        n(Kind::Program, *p),
                        name.as_ptr(),
                    )),
                    Err(_) => Reply::Int(-1),
                },
                Q::TransformFeedbackVarying(p, i) => {
                    let prog = n(Kind::Program, *p);
                    let (mut max, mut written) = (0, 0);
                    (gl.glGetProgramivRobustANGLE)(
                        prog,
                        GL_TRANSFORM_FEEDBACK_VARYING_MAX_LENGTH,
                        1,
                        &mut written,
                        &mut max,
                    );
                    let mut buf = vec![0u8; max.clamp(1, 1 << 16) as usize];
                    let (mut len, mut size, mut ty) = (0, 0, 0u32);
                    (gl.glGetTransformFeedbackVarying)(
                        prog,
                        *i,
                        buf.len() as i32,
                        &mut len,
                        &mut size,
                        &mut ty,
                        buf.as_mut_ptr().cast(),
                    );
                    if ty == 0 {
                        return Reply::ActiveInfo(None);
                    }
                    buf.truncate(len.clamp(0, buf.len() as i32) as usize);
                    Reply::ActiveInfo(Some((size, ty, String::from_utf8_lossy(&buf).into_owned())))
                }
                Q::InternalformatParameter(t, f, p) => {
                    let count = if *p == GL_SAMPLES {
                        let (mut v, mut len) = (0, 0);
                        (gl.glGetInternalformativRobustANGLE)(
                            *t,
                            *f,
                            GL_NUM_SAMPLE_COUNTS,
                            1,
                            &mut len,
                            &mut v,
                        );
                        if len != 1 {
                            return Reply::None;
                        }
                        v.clamp(0, 64) as usize
                    } else {
                        1
                    };
                    let mut out = vec![0i32; count.max(1)];
                    let mut len = 0;
                    (gl.glGetInternalformativRobustANGLE)(
                        *t,
                        *f,
                        *p,
                        out.len() as i32,
                        &mut len,
                        out.as_mut_ptr(),
                    );
                    out.truncate(len.clamp(0, count as i32) as usize);
                    Reply::Ints(out)
                }
                Q::UniformU(p, loc, count) => {
                    let count = (*count as usize).min(16);
                    let mut v = [0u32; 16];
                    let mut len = 0;
                    (gl.glGetUniformuivRobustANGLE)(
                        n(Kind::Program, *p),
                        *loc,
                        (count * 4) as i32,
                        &mut len,
                        v.as_mut_ptr(),
                    );
                    Reply::Ints(
                        v[..len.clamp(0, count as i32) as usize]
                            .iter()
                            .map(|x| *x as i32)
                            .collect(),
                    )
                }
                Q::VertexAttribI(i, p, unsigned) => {
                    let mut v = [0i32; 4];
                    let mut len = 0;
                    if *unsigned {
                        (gl.glGetVertexAttribIuivRobustANGLE)(
                            *i,
                            *p,
                            4,
                            &mut len,
                            v.as_mut_ptr().cast(),
                        );
                    } else {
                        (gl.glGetVertexAttribIivRobustANGLE)(*i, *p, 4, &mut len, v.as_mut_ptr());
                    }
                    Reply::Ints(v[..len.clamp(0, 4) as usize].to_vec())
                }
            }
        }
    }

    /// The current context's pending error, read before work of the service's own.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn take_page_error(&self) -> u32 {
        // SAFETY: no arguments; a context is current.
        unsafe { (self.angle.gl.glGetError)() }
    }

    /// Keeps `error` (from [`Device::take_page_error`]) for the page's next `getError`.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn hold_page_error(&mut self, id: ContextId, error: u32) {
        if let Some(c) = self.contexts.get_mut(&id) {
            if c.held_error == 0 {
                c.held_error = error;
            }
        }
    }

    /// Clears the drawing buffer to its defaults (WebGL 1.0 §2.2: after the drawing buffer was
    /// presented), leaving every piece of state as the page set it.
    fn clear_drawing_buffer(&self, a: &Attributes) {
        let gl = &self.angle.gl;
        let fb = self.get_ints(GL_FRAMEBUFFER_BINDING, 1);
        let scissor = self.get_bools(GL_SCISSOR_TEST, 1);
        let color = self.get_floats(GL_COLOR_CLEAR_VALUE, 4);
        let mask = self.get_bools(GL_COLOR_WRITEMASK, 4);
        let depth = self.get_floats(GL_DEPTH_CLEAR_VALUE, 1);
        let depth_mask = self.get_bools(GL_DEPTH_WRITEMASK, 1);
        let stencil = self.get_ints(GL_STENCIL_CLEAR_VALUE, 1);
        let front = self.get_ints(GL_STENCIL_WRITEMASK, 1);
        let back = self.get_ints(GL_STENCIL_BACK_WRITEMASK, 1);
        let at = |v: &[f32], i| v.get(i).copied().unwrap_or(0.0);
        let bit = |v: &[bool], i| v.get(i).copied().unwrap_or(true) as u8;
        let int = |v: &[i32]| v.first().copied().unwrap_or(0);
        // (OpenGL ES 3.0: the read framebuffer is bound apart, and rasterizer discard stops a
        // clear)
        let es3 = a.webgl2.then(|| {
            (
                int(&self.get_ints(GL_READ_FRAMEBUFFER_BINDING, 1)),
                self.get_bools(GL_RASTERIZER_DISCARD, 1).first() == Some(&true),
            )
        });
        // SAFETY: plain state calls on the current context; values were read from it.
        unsafe {
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, 0);
            if es3.is_some_and(|(_, discard)| discard) {
                (gl.glDisable)(GL_RASTERIZER_DISCARD);
            }
            (gl.glDisable)(GL_SCISSOR_TEST);
            (gl.glClearColor)(0.0, 0.0, 0.0, 0.0);
            (gl.glColorMask)(1, 1, 1, 1);
            let mut bits = GL_COLOR_BUFFER_BIT;
            if a.depth {
                (gl.glClearDepthf)(1.0);
                (gl.glDepthMask)(1);
                bits |= GL_DEPTH_BUFFER_BIT;
            }
            if a.stencil {
                (gl.glClearStencil)(0);
                (gl.glStencilMaskSeparate)(GL_FRONT, u32::MAX);
                (gl.glStencilMaskSeparate)(GL_BACK, u32::MAX);
                bits |= GL_STENCIL_BUFFER_BIT;
            }
            (gl.glClear)(bits);
            (gl.glClearColor)(at(&color, 0), at(&color, 1), at(&color, 2), at(&color, 3));
            (gl.glColorMask)(bit(&mask, 0), bit(&mask, 1), bit(&mask, 2), bit(&mask, 3));
            if a.depth {
                (gl.glClearDepthf)(at(&depth, 0));
                (gl.glDepthMask)(bit(&depth_mask, 0));
            }
            if a.stencil {
                (gl.glClearStencil)(int(&stencil));
                (gl.glStencilMaskSeparate)(GL_FRONT, int(&front) as u32);
                (gl.glStencilMaskSeparate)(GL_BACK, int(&back) as u32);
            }
            if scissor.first().copied().unwrap_or(false) {
                (gl.glEnable)(GL_SCISSOR_TEST);
            }
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, int(&fb) as u32);
            if let Some((read, discard)) = es3 {
                (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, read as u32);
                if discard {
                    (gl.glEnable)(GL_RASTERIZER_DISCARD);
                }
            }
        }
    }
}

impl Backend for Device {
    fn create_context(
        &mut self,
        id: ContextId,
        a: &Attributes,
        width: u32,
        height: u32,
    ) -> Result<Created, String> {
        self.destroy(id);
        let config = self
            .choose_config(a)
            .ok_or("no EGL config for the attributes")?;
        let mut attrs = vec![
            EGL_CONTEXT_MAJOR_VERSION,
            if a.webgl2 { 3 } else { 2 },
            EGL_CONTEXT_MINOR_VERSION,
            0,
            EGL_CONTEXT_WEBGL_COMPATIBILITY_ANGLE,
            EGL_TRUE,
            EGL_CONTEXT_BIND_GENERATES_RESOURCE_CHROMIUM,
            EGL_FALSE,
            EGL_CONTEXT_CLIENT_ARRAYS_ENABLED_ANGLE,
            EGL_FALSE,
            EGL_ROBUST_RESOURCE_INITIALIZATION_ANGLE,
            EGL_TRUE,
            EGL_CONTEXT_OPENGL_ROBUST_ACCESS_EXT,
            EGL_TRUE,
            EGL_CONTEXT_OPENGL_RESET_NOTIFICATION_STRATEGY_EXT,
            EGL_LOSE_CONTEXT_ON_RESET_EXT,
            EGL_CONTEXT_HARDENED_ANGLE,
            EGL_TRUE,
        ];
        if a.power_preference != 0 {
            attrs.extend([
                EGL_POWER_PREFERENCE_ANGLE,
                if a.power_preference == 1 {
                    EGL_LOW_POWER_ANGLE
                } else {
                    EGL_HIGH_POWER_ANGLE
                },
            ]);
        }
        let e = &self.angle.egl;
        let create = |attrs: &mut Vec<EGLint>| {
            attrs.push(EGL_NONE);
            // SAFETY: a config of this display; the list ends in EGL_NONE; no share context.
            let c =
                unsafe { (e.eglCreateContext)(self.display, config, null_mut(), attrs.as_ptr()) };
            attrs.pop();
            c
        };
        let mut context = create(&mut attrs);
        if context.is_null() && a.power_preference != 0 {
            // (an ANGLE without the power preference extension)
            attrs.truncate(attrs.len() - 2);
            context = create(&mut attrs);
        }
        if context.is_null() {
            // SAFETY: no arguments.
            return Err(format!("eglCreateContext failed: {:#x}", unsafe {
                (e.eglGetError)()
            }));
        }
        let surface = self.pbuffer(config, width, height);
        if surface.is_null() {
            // SAFETY: the context was just made and is not current.
            unsafe { (e.eglDestroyContext)(self.display, context) };
            return Err("eglCreatePbufferSurface failed".into());
        }
        let attr = |x| self.config_attrib(config, x);
        let granted = Attributes {
            alpha: attr(EGL_ALPHA_SIZE) > 0,
            depth: attr(EGL_DEPTH_SIZE) > 0,
            stencil: a.stencil && attr(EGL_STENCIL_SIZE) > 0,
            antialias: attr(EGL_SAMPLE_BUFFERS) > 0,
            ..*a
        };
        self.contexts.insert(
            id,
            Context {
                context,
                surface,
                config,
                attributes: granted,
                width,
                height,
                names: HashMap::new(),
                syncs: HashMap::new(),
                held_error: 0,
                #[cfg(windows)]
                shared: None,
            },
        );
        self.current = None;
        if !self.make_current(id) {
            self.destroy(id);
            return Err("eglMakeCurrent failed".into());
        }
        Ok(Created {
            attributes: granted,
            limits: self.limits(a.webgl2),
            extensions: self.extension_list(0x1F03),
            // GL_REQUESTABLE_EXTENSIONS_ANGLE
            requestable: self.extension_list(0x93A8),
        })
    }

    fn resize(&mut self, id: ContextId, width: u32, height: u32) {
        let Some(c) = self.contexts.get(&id) else {
            return;
        };
        if (c.width, c.height) == (width, height) {
            return;
        }
        let surface = self.pbuffer(c.config, width, height);
        if surface.is_null() {
            return;
        }
        let old = c.surface;
        let c = self.contexts.get_mut(&id).expect("looked up above");
        c.surface = surface;
        c.width = width;
        c.height = height;
        // (shared textures are of the old size)
        #[cfg(windows)]
        if let Some(t) = c.shared.take() {
            self.release_targets(t);
        }
        self.current = None;
        self.make_current(id);
        // SAFETY: the old surface is no longer current.
        unsafe { (self.angle.egl.eglDestroySurface)(self.display, old) };
    }

    fn destroy(&mut self, id: ContextId) {
        let Some(c) = self.contexts.remove(&id) else {
            return;
        };
        #[cfg(windows)]
        let mut c = c;
        #[cfg(windows)]
        if let Some(t) = c.shared.take() {
            if self.current == Some(id) {
                // SAFETY: releasing the current context before its targets' surfaces go.
                unsafe {
                    (self.angle.egl.eglMakeCurrent)(
                        self.display,
                        null_mut(),
                        null_mut(),
                        null_mut(),
                    )
                };
                self.current = None;
            }
            self.release_targets(t);
        }
        let e = &self.angle.egl;
        // SAFETY: releasing the current context first; then the context's own objects.
        unsafe {
            if self.current == Some(id) {
                (e.eglMakeCurrent)(self.display, null_mut(), null_mut(), null_mut());
                self.current = None;
            }
            (e.eglDestroySurface)(self.display, c.surface);
            (e.eglDestroyContext)(self.display, c.context);
        }
    }

    fn submit(&mut self, batch: Vec<(ContextId, Command)>) {
        for (id, cmd) in &batch {
            if self.make_current(*id) {
                self.execute(*id, cmd);
            }
        }
    }

    fn query(&mut self, id: ContextId, query: &Query) -> Reply {
        if !self.make_current(id) {
            return Reply::None;
        }
        self.answer(id, query)
    }

    fn present(&mut self, id: ContextId) -> Option<Readback> {
        if !self.make_current(id) {
            return None;
        }
        let c = &self.contexts[&id];
        let (w, h, a) = (c.width.max(1), c.height.max(1), c.attributes);
        let gl = &self.angle.gl;
        let fb = self.get_ints(GL_FRAMEBUFFER_BINDING, 1);
        let pack = self.get_ints(GL_PACK_ALIGNMENT, 1);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let (mut written, mut cols, mut rows) = (0, 0, 0);
        // OpenGL ES 3.0: the read framebuffer, the pixel pack buffer and the pack parameters
        // apply to the readback too; they are set aside and put back.
        let es3 = a.webgl2.then(|| {
            let one = |p| self.get_ints(p, 1).first().copied().unwrap_or(0);
            [
                one(GL_READ_FRAMEBUFFER_BINDING),
                one(GL_PIXEL_PACK_BUFFER_BINDING),
                one(GL_PACK_ROW_LENGTH),
                one(GL_PACK_SKIP_ROWS),
                one(GL_PACK_SKIP_PIXELS),
            ]
        });
        let mut read_buffer = None;
        // SAFETY: the buffer holds w*h*4 bytes and ANGLE is told so; state is put back after.
        unsafe {
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, 0);
            if es3.is_some() {
                (gl.glBindBuffer)(GL_PIXEL_PACK_BUFFER, 0);
                (gl.glPixelStorei)(GL_PACK_ROW_LENGTH, 0);
                (gl.glPixelStorei)(GL_PACK_SKIP_ROWS, 0);
                (gl.glPixelStorei)(GL_PACK_SKIP_PIXELS, 0);
                read_buffer = self.get_ints(GL_READ_BUFFER, 1).first().copied();
                (gl.glReadBuffer)(GL_BACK);
            }
            (gl.glPixelStorei)(GL_PACK_ALIGNMENT, 4);
            (gl.glReadPixelsRobustANGLE)(
                0,
                0,
                w as i32,
                h as i32,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                rgba.len() as i32,
                &mut written,
                &mut cols,
                &mut rows,
                rgba.as_mut_ptr().cast(),
            );
            (gl.glPixelStorei)(GL_PACK_ALIGNMENT, pack.first().copied().unwrap_or(4));
            if let Some(rb) = read_buffer {
                (gl.glReadBuffer)(rb as u32);
            }
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, fb.first().copied().unwrap_or(0) as u32);
            if let Some([read, pack_buffer, row, skip_rows, skip_pixels]) = es3 {
                (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, read as u32);
                (gl.glBindBuffer)(GL_PIXEL_PACK_BUFFER, pack_buffer as u32);
                (gl.glPixelStorei)(GL_PACK_ROW_LENGTH, row);
                (gl.glPixelStorei)(GL_PACK_SKIP_ROWS, skip_rows);
                (gl.glPixelStorei)(GL_PACK_SKIP_PIXELS, skip_pixels);
            }
        }
        if !a.preserve_drawing_buffer {
            self.clear_drawing_buffer(&a);
        }
        Some(Readback {
            width: w,
            height: h,
            rgba,
        })
    }

    #[cfg(windows)]
    fn share_into(&mut self, id: ContextId, targets: Option<SharedTargets>) -> bool {
        self.share_into_targets(id, targets)
    }

    fn present_shared(&mut self, id: ContextId) -> SharedPresent {
        #[cfg(windows)]
        return self.present_into_target(id);
        #[cfg(not(windows))]
        {
            let _ = id;
            SharedPresent::Unavailable
        }
    }

    fn read_presented(&mut self, id: ContextId) -> Option<Readback> {
        #[cfg(windows)]
        return self.read_front(id);
        #[cfg(not(windows))]
        {
            let _ = id;
            None
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let ids: Vec<ContextId> = self.contexts.keys().copied().collect();
        for id in ids {
            self.destroy(id);
        }
        // (the display is the process's one ANGLE display of its kind, so it is never terminated)
    }
}
