// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The service: runs the protocol's commands on ANGLE.
//!
//! Each context is an OpenGL ES 2.0 context created in ANGLE's WebGL compatibility mode, so ANGLE
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

struct Context {
    context: EGLContext,
    surface: EGLSurface,
    config: EGLConfig,
    attributes: Attributes,
    width: u32,
    height: u32,
    /// Client ids to ANGLE's names.
    names: HashMap<(Kind, u32), u32>,
}

/// ANGLE on one Direct3D 11 device, and the contexts on it. Used from one thread only: the
/// process's WebGL thread ([`crate::Service`]).
pub(crate) struct Device {
    angle: Angle,
    display: EGLDisplay,
    contexts: HashMap<ContextId, Context>,
    current: Option<ContextId>,
}

impl Device {
    /// Loads ANGLE and opens its display: the graphics card, or WARP (Direct3D's software
    /// rasterizer) when `warp`.
    pub(crate) fn new(warp: bool) -> Result<Device, String> {
        let angle = load()?;
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
        let e = &angle.egl;
        // SAFETY: the attribute list ends in EGL_NONE; EGL_DEFAULT_DISPLAY is null.
        let display = unsafe {
            (e.eglGetPlatformDisplayEXT)(EGL_PLATFORM_ANGLE_ANGLE, null_mut(), attrs.as_ptr())
        };
        if display.is_null() {
            return Err("ANGLE has no Direct3D 11 display".into());
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: a display from eglGetPlatformDisplayEXT; out pointers are locals.
        if unsafe { (e.eglInitialize)(display, &mut major, &mut minor) } == 0 {
            // SAFETY: no arguments.
            return Err(format!("eglInitialize failed: {:#x}", unsafe {
                (e.eglGetError)()
            }));
        }
        // SAFETY: a constant enum.
        unsafe { (e.eglBindAPI)(EGL_OPENGL_ES_API) };
        Ok(Device {
            angle,
            display,
            contexts: HashMap::new(),
            current: None,
        })
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
            if attr(EGL_SURFACE_TYPE) & EGL_PBUFFER_BIT == 0
                || attr(EGL_RENDERABLE_TYPE) & EGL_OPENGL_ES2_BIT == 0
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

    fn limits(&self) -> Limits {
        let one = |p| self.get_ints(p, 1).first().copied().unwrap_or(0);
        let dims = self.get_ints(GL_MAX_VIEWPORT_DIMS, 2);
        Limits {
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
                        Kind::Shader => {}
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
        // SAFETY: plain state calls on the current context; values were read from it.
        unsafe {
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, 0);
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
            2,
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
            },
        );
        self.current = None;
        if !self.make_current(id) {
            self.destroy(id);
            return Err("eglMakeCurrent failed".into());
        }
        Ok(Created {
            attributes: granted,
            limits: self.limits(),
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
        self.current = None;
        self.make_current(id);
        // SAFETY: the old surface is no longer current.
        unsafe { (self.angle.egl.eglDestroySurface)(self.display, old) };
    }

    fn destroy(&mut self, id: ContextId) {
        let Some(c) = self.contexts.remove(&id) else {
            return;
        };
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
        // SAFETY: the buffer holds w*h*4 bytes and ANGLE is told so; state is put back after.
        unsafe {
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, 0);
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
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, fb.first().copied().unwrap_or(0) as u32);
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
