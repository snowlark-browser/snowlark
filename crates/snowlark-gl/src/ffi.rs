// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! EGL and OpenGL ES 2.0 entry points of ANGLE (`libEGL.dll` and `libGLESv2.dll` on Windows,
//! `libEGL.so` and `libGLESv2.so` on Linux), loaded at run time
//! from the folder of the running executable (or, for a test binary in `deps`, its parent). The
//! libraries are optional: without them there is no WebGL and `getContext("webgl")` returns null.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::path::{Path, PathBuf};

pub type EGLDisplay = *mut c_void;
pub type EGLConfig = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLSurface = *mut c_void;
pub type EGLint = i32;
pub type EGLBoolean = u32;

pub const EGL_NONE: EGLint = 0x3038;
pub const EGL_TRUE: EGLint = 1;
pub const EGL_FALSE: EGLint = 0;
pub const EGL_ALPHA_SIZE: EGLint = 0x3021;
pub const EGL_BLUE_SIZE: EGLint = 0x3022;
pub const EGL_GREEN_SIZE: EGLint = 0x3023;
pub const EGL_RED_SIZE: EGLint = 0x3024;
pub const EGL_DEPTH_SIZE: EGLint = 0x3025;
pub const EGL_STENCIL_SIZE: EGLint = 0x3026;
pub const EGL_SAMPLES: EGLint = 0x3031;
pub const EGL_SAMPLE_BUFFERS: EGLint = 0x3032;
pub const EGL_SURFACE_TYPE: EGLint = 0x3033;
pub const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
pub const EGL_HEIGHT: EGLint = 0x3056;
pub const EGL_WIDTH: EGLint = 0x3057;
pub const EGL_PBUFFER_BIT: EGLint = 0x0001;
pub const EGL_OPENGL_ES2_BIT: EGLint = 0x0004;
pub const EGL_OPENGL_ES3_BIT: EGLint = 0x0040;
pub const EGL_OPENGL_ES_API: u32 = 0x30A0;
pub const EGL_CONTEXT_MAJOR_VERSION: EGLint = 0x3098;
pub const EGL_CONTEXT_MINOR_VERSION: EGLint = 0x30FB;
pub const EGL_PLATFORM_ANGLE_ANGLE: u32 = 0x3202;
pub const EGL_PLATFORM_ANGLE_TYPE_ANGLE: EGLint = 0x3203;
pub const EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE: EGLint = 0x3208;
pub const EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE: EGLint = 0x3209;
pub const EGL_PLATFORM_ANGLE_DEVICE_TYPE_D3D_WARP_ANGLE: EGLint = 0x320B;
pub const EGL_PLATFORM_ANGLE_TYPE_OPENGL_ANGLE: EGLint = 0x320D;
pub const EGL_PLATFORM_ANGLE_TYPE_OPENGLES_ANGLE: EGLint = 0x320E;
pub const EGL_PLATFORM_ANGLE_DEVICE_TYPE_EGL_ANGLE: EGLint = 0x348E;
pub const EGL_PLATFORM_ANGLE_NATIVE_PLATFORM_TYPE_ANGLE: EGLint = 0x348F;
pub const EGL_PLATFORM_SURFACELESS_MESA: EGLint = 0x31DD;
pub const EGL_CONTEXT_WEBGL_COMPATIBILITY_ANGLE: EGLint = 0x33AC;
pub const EGL_CONTEXT_BIND_GENERATES_RESOURCE_CHROMIUM: EGLint = 0x33AD;
pub const EGL_CONTEXT_CLIENT_ARRAYS_ENABLED_ANGLE: EGLint = 0x3452;
pub const EGL_ROBUST_RESOURCE_INITIALIZATION_ANGLE: EGLint = 0x3453;
pub const EGL_CONTEXT_HARDENED_ANGLE: EGLint = 0x34F8;
pub const EGL_CONTEXT_OPENGL_ROBUST_ACCESS_EXT: EGLint = 0x30BF;
pub const EGL_CONTEXT_OPENGL_RESET_NOTIFICATION_STRATEGY_EXT: EGLint = 0x3138;
pub const EGL_LOSE_CONTEXT_ON_RESET_EXT: EGLint = 0x31BF;
pub const EGL_POWER_PREFERENCE_ANGLE: EGLint = 0x3482;
pub const EGL_LOW_POWER_ANGLE: EGLint = 0x0001;
pub const EGL_HIGH_POWER_ANGLE: EGLint = 0x0002;
// EGL_EXT_device_query, EGL_ANGLE_device_d3d, EGL_ANGLE_d3d_texture_client_buffer.
#[cfg(windows)]
pub const EGL_DEVICE_EXT: EGLint = 0x322C;
#[cfg(windows)]
pub const EGL_D3D11_DEVICE_ANGLE: EGLint = 0x33A1;
#[cfg(windows)]
pub const EGL_D3D_TEXTURE_ANGLE: u32 = 0x33A3;

/// `EGLAttrib`: a pointer-sized attribute value.
#[cfg(windows)]
pub type EGLAttrib = isize;

macro_rules! entry_points {
    ($name:ident { $($f:ident: fn($($a:ty),*) $(-> $r:ty)?;)* }) => {
        pub struct $name {
            $(pub $f: unsafe extern "system" fn($($a),*) $(-> $r)?,)*
        }
        impl $name {
            /// Every entry point through `get`, or the first name it does not find.
            fn load(get: &mut dyn FnMut(&str) -> *const c_void) -> Result<Self, String> {
                Ok($name {
                    $($f: {
                        let p = get(concat!(stringify!($f), "\0"));
                        if p.is_null() {
                            return Err(format!("ANGLE has no {}", stringify!($f)));
                        }
                        // SAFETY: `p` is the address of the named entry point, whose C signature
                        // (from ANGLE's headers) is the declared one, with the system calling
                        // convention of the platform.
                        unsafe { std::mem::transmute::<*const c_void, unsafe extern "system" fn($($a),*) $(-> $r)?>(p) }
                    },)*
                })
            }
        }
    };
}

entry_points!(Egl {
    eglGetPlatformDisplayEXT: fn(u32, *mut c_void, *const EGLint) -> EGLDisplay;
    eglInitialize: fn(EGLDisplay, *mut EGLint, *mut EGLint) -> EGLBoolean;
    eglBindAPI: fn(u32) -> EGLBoolean;
    eglGetConfigs: fn(EGLDisplay, *mut EGLConfig, EGLint, *mut EGLint) -> EGLBoolean;
    eglGetConfigAttrib: fn(EGLDisplay, EGLConfig, EGLint, *mut EGLint) -> EGLBoolean;
    eglCreateContext: fn(EGLDisplay, EGLConfig, EGLContext, *const EGLint) -> EGLContext;
    eglDestroyContext: fn(EGLDisplay, EGLContext) -> EGLBoolean;
    eglCreatePbufferSurface: fn(EGLDisplay, EGLConfig, *const EGLint) -> EGLSurface;
    eglDestroySurface: fn(EGLDisplay, EGLSurface) -> EGLBoolean;
    eglMakeCurrent: fn(EGLDisplay, EGLSurface, EGLSurface, EGLContext) -> EGLBoolean;
    eglGetError: fn() -> EGLint;
});

#[cfg(windows)]
entry_points!(EglD3d {
    eglQueryDisplayAttribEXT: fn(EGLDisplay, EGLint, *mut EGLAttrib) -> EGLBoolean;
    eglQueryDeviceAttribEXT: fn(*mut c_void, EGLint, *mut EGLAttrib) -> EGLBoolean;
    eglCreatePbufferFromClientBuffer: fn(EGLDisplay, u32, *mut c_void, EGLConfig, *const EGLint) -> EGLSurface;
});

entry_points!(Gles {
    glActiveTexture: fn(u32);
    glAttachShader: fn(u32, u32);
    glBindAttribLocation: fn(u32, u32, *const i8);
    glBindBuffer: fn(u32, u32);
    glBindFramebuffer: fn(u32, u32);
    glBindRenderbuffer: fn(u32, u32);
    glBindTexture: fn(u32, u32);
    glBlendColor: fn(f32, f32, f32, f32);
    glBlendEquationSeparate: fn(u32, u32);
    glBlendFuncSeparate: fn(u32, u32, u32, u32);
    glBufferData: fn(u32, isize, *const c_void, u32);
    glBufferSubData: fn(u32, isize, isize, *const c_void);
    glCheckFramebufferStatus: fn(u32) -> u32;
    glClear: fn(u32);
    glClearColor: fn(f32, f32, f32, f32);
    glClearDepthf: fn(f32);
    glClearStencil: fn(i32);
    glColorMask: fn(u8, u8, u8, u8);
    glCompileShader: fn(u32);
    glCompressedTexImage2D: fn(u32, i32, u32, i32, i32, i32, i32, *const c_void);
    glCompressedTexSubImage2D: fn(u32, i32, i32, i32, i32, i32, u32, i32, *const c_void);
    glCopyTexImage2D: fn(u32, i32, u32, i32, i32, i32, i32, i32);
    glCopyTexSubImage2D: fn(u32, i32, i32, i32, i32, i32, i32, i32);
    glCreateProgram: fn() -> u32;
    glCreateShader: fn(u32) -> u32;
    glCullFace: fn(u32);
    glDeleteBuffers: fn(i32, *const u32);
    glDeleteFramebuffers: fn(i32, *const u32);
    glDeleteProgram: fn(u32);
    glDeleteRenderbuffers: fn(i32, *const u32);
    glDeleteShader: fn(u32);
    glDeleteTextures: fn(i32, *const u32);
    glDepthFunc: fn(u32);
    glDepthMask: fn(u8);
    glDepthRangef: fn(f32, f32);
    glDetachShader: fn(u32, u32);
    glDisable: fn(u32);
    glDisableVertexAttribArray: fn(u32);
    glDrawArrays: fn(u32, i32, i32);
    glDrawElements: fn(u32, i32, u32, *const c_void);
    glEnable: fn(u32);
    glEnableVertexAttribArray: fn(u32);
    glFinish: fn();
    glFlush: fn();
    glFramebufferRenderbuffer: fn(u32, u32, u32, u32);
    glFramebufferTexture2D: fn(u32, u32, u32, u32, i32);
    glFrontFace: fn(u32);
    glGenBuffers: fn(i32, *mut u32);
    glGenFramebuffers: fn(i32, *mut u32);
    glGenRenderbuffers: fn(i32, *mut u32);
    glGenTextures: fn(i32, *mut u32);
    glGenerateMipmap: fn(u32);
    glGetActiveAttrib: fn(u32, u32, i32, *mut i32, *mut i32, *mut u32, *mut i8);
    glGetActiveUniform: fn(u32, u32, i32, *mut i32, *mut i32, *mut u32, *mut i8);
    glGetAttribLocation: fn(u32, *const i8) -> i32;
    glGetError: fn() -> u32;
    glGetGraphicsResetStatusEXT: fn() -> u32;
    glGetProgramInfoLog: fn(u32, i32, *mut i32, *mut i8);
    glGetShaderInfoLog: fn(u32, i32, *mut i32, *mut i8);
    glGetShaderPrecisionFormat: fn(u32, u32, *mut i32, *mut i32);
    glGetString: fn(u32) -> *const u8;
    glGetUniformLocation: fn(u32, *const i8) -> i32;
    glGetVertexAttribPointerv: fn(u32, u32, *mut *mut c_void);
    glHint: fn(u32, u32);
    glIsEnabled: fn(u32) -> u8;
    glLineWidth: fn(f32);
    glLinkProgram: fn(u32);
    glPixelStorei: fn(u32, i32);
    glPolygonOffset: fn(f32, f32);
    glRenderbufferStorage: fn(u32, u32, i32, i32);
    glSampleCoverage: fn(f32, u8);
    glScissor: fn(i32, i32, i32, i32);
    glShaderSource: fn(u32, i32, *const *const i8, *const i32);
    glStencilFuncSeparate: fn(u32, u32, i32, u32);
    glStencilMaskSeparate: fn(u32, u32);
    glStencilOpSeparate: fn(u32, u32, u32, u32);
    glTexParameterf: fn(u32, u32, f32);
    glTexParameteri: fn(u32, u32, i32);
    glUniform1fv: fn(i32, i32, *const f32);
    glUniform2fv: fn(i32, i32, *const f32);
    glUniform3fv: fn(i32, i32, *const f32);
    glUniform4fv: fn(i32, i32, *const f32);
    glUniform1iv: fn(i32, i32, *const i32);
    glUniform2iv: fn(i32, i32, *const i32);
    glUniform3iv: fn(i32, i32, *const i32);
    glUniform4iv: fn(i32, i32, *const i32);
    glUniformMatrix2fv: fn(i32, i32, u8, *const f32);
    glUniformMatrix3fv: fn(i32, i32, u8, *const f32);
    glUniformMatrix4fv: fn(i32, i32, u8, *const f32);
    glUseProgram: fn(u32);
    glValidateProgram: fn(u32);
    glVertexAttrib4fv: fn(u32, *const f32);
    glVertexAttribPointer: fn(u32, i32, u32, u8, i32, *const c_void);
    glViewport: fn(i32, i32, i32, i32);
    // GL_ANGLE_robust_client_memory: every call that writes or reads caller memory says how much.
    glGetBooleanvRobustANGLE: fn(u32, i32, *mut i32, *mut u8);
    glGetFloatvRobustANGLE: fn(u32, i32, *mut i32, *mut f32);
    glGetIntegervRobustANGLE: fn(u32, i32, *mut i32, *mut i32);
    glGetBufferParameterivRobustANGLE: fn(u32, u32, i32, *mut i32, *mut i32);
    glGetFramebufferAttachmentParameterivRobustANGLE: fn(u32, u32, u32, i32, *mut i32, *mut i32);
    glGetProgramivRobustANGLE: fn(u32, u32, i32, *mut i32, *mut i32);
    glGetRenderbufferParameterivRobustANGLE: fn(u32, u32, i32, *mut i32, *mut i32);
    glGetShaderivRobustANGLE: fn(u32, u32, i32, *mut i32, *mut i32);
    glGetTexParameterivRobustANGLE: fn(u32, u32, i32, *mut i32, *mut i32);
    glGetUniformfvRobustANGLE: fn(u32, i32, i32, *mut i32, *mut f32);
    glGetUniformivRobustANGLE: fn(u32, i32, i32, *mut i32, *mut i32);
    glGetVertexAttribfvRobustANGLE: fn(u32, u32, i32, *mut i32, *mut f32);
    glReadPixelsRobustANGLE: fn(i32, i32, i32, i32, u32, u32, i32, *mut i32, *mut i32, *mut i32, *mut c_void);
    glTexImage2DRobustANGLE: fn(u32, i32, i32, i32, i32, i32, u32, u32, i32, *const c_void);
    glTexSubImage2DRobustANGLE: fn(u32, i32, i32, i32, i32, i32, u32, u32, i32, *const c_void);
    // OpenGL ES 3.0 (WebGL 2.0 contexts).
    glBeginQuery: fn(u32, u32);
    glBeginTransformFeedback: fn(u32);
    glBindBufferBase: fn(u32, u32, u32);
    glBindBufferRange: fn(u32, u32, u32, isize, isize);
    glBindSampler: fn(u32, u32);
    glBindTransformFeedback: fn(u32, u32);
    glBindVertexArray: fn(u32);
    glBlitFramebuffer: fn(i32, i32, i32, i32, i32, i32, i32, i32, u32, u32);
    glClearBufferfi: fn(u32, i32, f32, i32);
    glClearBufferfv: fn(u32, i32, *const f32);
    glClearBufferiv: fn(u32, i32, *const i32);
    glClearBufferuiv: fn(u32, i32, *const u32);
    glClientWaitSync: fn(*const c_void, u32, u64) -> u32;
    glCompressedTexImage3D: fn(u32, i32, u32, i32, i32, i32, i32, i32, *const c_void);
    glCompressedTexSubImage3D: fn(u32, i32, i32, i32, i32, i32, i32, i32, u32, i32, *const c_void);
    glCopyBufferSubData: fn(u32, u32, isize, isize, isize);
    glCopyTexSubImage3D: fn(u32, i32, i32, i32, i32, i32, i32, i32, i32);
    glDeleteQueries: fn(i32, *const u32);
    glDeleteSamplers: fn(i32, *const u32);
    glDeleteSync: fn(*const c_void);
    glDeleteTransformFeedbacks: fn(i32, *const u32);
    glDeleteVertexArrays: fn(i32, *const u32);
    glDrawArraysInstanced: fn(u32, i32, i32, i32);
    glDrawBuffers: fn(i32, *const u32);
    glDrawElementsInstanced: fn(u32, i32, u32, *const c_void, i32);
    glDrawRangeElements: fn(u32, u32, u32, i32, u32, *const c_void);
    glEndQuery: fn(u32);
    glEndTransformFeedback: fn();
    glFenceSync: fn(u32, u32) -> *const c_void;
    glFramebufferTextureLayer: fn(u32, u32, u32, i32, i32);
    glGenQueries: fn(i32, *mut u32);
    glGenSamplers: fn(i32, *mut u32);
    glGenTransformFeedbacks: fn(i32, *mut u32);
    glGenVertexArrays: fn(i32, *mut u32);
    glGetActiveUniformBlockName: fn(u32, u32, i32, *mut i32, *mut i8);
    glGetActiveUniformsiv: fn(u32, i32, *const u32, u32, *mut i32);
    glGetFragDataLocation: fn(u32, *const i8) -> i32;
    glGetTransformFeedbackVarying: fn(u32, u32, i32, *mut i32, *mut i32, *mut u32, *mut i8);
    glGetUniformBlockIndex: fn(u32, *const i8) -> u32;
    glGetUniformIndices: fn(u32, i32, *const *const i8, *mut u32);
    glInvalidateFramebuffer: fn(u32, i32, *const u32);
    glInvalidateSubFramebuffer: fn(u32, i32, *const u32, i32, i32, i32, i32);
    glMapBufferRange: fn(u32, isize, isize, u32) -> *mut c_void;
    glPauseTransformFeedback: fn();
    glReadBuffer: fn(u32);
    glRenderbufferStorageMultisample: fn(u32, i32, u32, i32, i32);
    glResumeTransformFeedback: fn();
    glSamplerParameterf: fn(u32, u32, f32);
    glSamplerParameteri: fn(u32, u32, i32);
    glTexStorage2D: fn(u32, i32, u32, i32, i32);
    glTexStorage3D: fn(u32, i32, u32, i32, i32, i32);
    glTransformFeedbackVaryings: fn(u32, i32, *const *const i8, u32);
    glUniform1uiv: fn(i32, i32, *const u32);
    glUniform2uiv: fn(i32, i32, *const u32);
    glUniform3uiv: fn(i32, i32, *const u32);
    glUniform4uiv: fn(i32, i32, *const u32);
    glUniformBlockBinding: fn(u32, u32, u32);
    glUniformMatrix2x3fv: fn(i32, i32, u8, *const f32);
    glUniformMatrix2x4fv: fn(i32, i32, u8, *const f32);
    glUniformMatrix3x2fv: fn(i32, i32, u8, *const f32);
    glUniformMatrix3x4fv: fn(i32, i32, u8, *const f32);
    glUniformMatrix4x2fv: fn(i32, i32, u8, *const f32);
    glUniformMatrix4x3fv: fn(i32, i32, u8, *const f32);
    glUnmapBuffer: fn(u32) -> u8;
    glVertexAttribDivisor: fn(u32, u32);
    glVertexAttribI4iv: fn(u32, *const i32);
    glVertexAttribI4uiv: fn(u32, *const u32);
    glVertexAttribIPointer: fn(u32, i32, u32, i32, *const c_void);
    glWaitSync: fn(*const c_void, u32, u64);
    glGetActiveUniformBlockivRobustANGLE: fn(u32, u32, u32, i32, *mut i32, *mut i32);
    glGetInteger64vRobustANGLE: fn(u32, i32, *mut i32, *mut i64);
    glGetInteger64i_vRobustANGLE: fn(u32, u32, i32, *mut i32, *mut i64);
    glGetInternalformativRobustANGLE: fn(u32, u32, u32, i32, *mut i32, *mut i32);
    glGetQueryObjectuivRobustANGLE: fn(u32, u32, i32, *mut i32, *mut u32);
    glGetSamplerParameterfvRobustANGLE: fn(u32, u32, i32, *mut i32, *mut f32);
    glGetSamplerParameterivRobustANGLE: fn(u32, u32, i32, *mut i32, *mut i32);
    glGetSynciv: fn(*const c_void, u32, i32, *mut i32, *mut i32);
    glGetUniformuivRobustANGLE: fn(u32, i32, i32, *mut i32, *mut u32);
    glGetVertexAttribIivRobustANGLE: fn(u32, u32, i32, *mut i32, *mut i32);
    glGetVertexAttribIuivRobustANGLE: fn(u32, u32, i32, *mut i32, *mut u32);
    glTexImage3DRobustANGLE: fn(u32, i32, i32, i32, i32, i32, i32, u32, u32, i32, *const c_void);
    glTexSubImage3DRobustANGLE: fn(u32, i32, i32, i32, i32, i32, i32, i32, u32, u32, i32, *const c_void);
    // Extensions (WebGL's extension registry, as ANGLE offers them in its WebGL mode):
    glRequestExtensionANGLE: fn(*const i8);
    glGenVertexArraysOES: fn(i32, *mut u32);
    glDeleteVertexArraysOES: fn(i32, *const u32);
    glBindVertexArrayOES: fn(u32);
    glDrawArraysInstancedANGLE: fn(u32, i32, i32, i32);
    glDrawElementsInstancedANGLE: fn(u32, i32, u32, *const c_void, i32);
    glVertexAttribDivisorANGLE: fn(u32, u32);
    glDrawBuffersEXT: fn(i32, *const u32);
    glMultiDrawArraysANGLE: fn(u32, *const i32, *const i32, i32);
    glMultiDrawElementsANGLE: fn(u32, *const i32, u32, *const *const c_void, i32);
    glMultiDrawArraysInstancedANGLE: fn(u32, *const i32, *const i32, *const i32, i32);
    glMultiDrawElementsInstancedANGLE: fn(u32, *const i32, u32, *const *const c_void, *const i32, i32);
    glClipControlEXT: fn(u32, u32);
    glPolygonModeANGLE: fn(u32, u32);
    glProvokingVertexANGLE: fn(u32);
    glPolygonOffsetClampEXT: fn(f32, f32, f32);
    glEnableiOES: fn(u32, u32);
    glDisableiOES: fn(u32, u32);
    glBlendEquationSeparateiOES: fn(u32, u32, u32);
    glBlendFuncSeparateiOES: fn(u32, u32, u32, u32, u32);
    glColorMaskiOES: fn(u32, u8, u8, u8, u8);
    glFramebufferTextureMultiviewOVR: fn(u32, u32, u32, i32, i32, i32);
});

/// The loaded libraries. They stay loaded for the life of the process.
pub struct Angle {
    pub egl: Egl,
    pub gl: Gles,
    /// ANGLE's Direct3D 11 device and surfaces on Direct3D textures (`None` when an entry point is
    /// missing): drawing buffers presented into shared textures.
    #[cfg(windows)]
    pub d3d: Option<EglD3d>,
}

/// The folders searched for ANGLE: the executable's, then its parent (test binaries run from
/// `deps`), then, for a test binary under `build/<crate>/<hash>/out` (newer cargo's layout), the
/// profile folder that holds that `build`.
fn folders() -> Vec<PathBuf> {
    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = exe
        .ancestors()
        .skip(1)
        .take(2)
        .map(Path::to_path_buf)
        .collect();
    if let Some(profile) = exe
        .ancestors()
        .skip(1)
        .find(|p| p.file_name().is_some_and(|n| n == "build"))
        .and_then(Path::parent)
    {
        folders.push(profile.to_path_buf());
    }
    folders
}

#[cfg(windows)]
pub fn load() -> Result<Angle, String> {
    use windows::core::{HSTRING, PCSTR};
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH,
    };
    let dir = folders()
        .into_iter()
        .find(|d| d.join("libEGL.dll").is_file() && d.join("libGLESv2.dll").is_file())
        .ok_or("libEGL.dll and libGLESv2.dll are not beside the executable")?;
    let open = |name: &str| {
        let path = HSTRING::from(dir.join(name).as_os_str());
        // SAFETY: a full path to a library; the altered search path makes its own dependencies
        // (libEGL's libGLESv2) resolve from the same folder. The module is never freed.
        unsafe { LoadLibraryExW(&path, None, LOAD_WITH_ALTERED_SEARCH_PATH) }
            .map_err(|e| format!("cannot load {name}: {e}"))
    };
    let gles = open("libGLESv2.dll")?;
    let egl_module = open("libEGL.dll")?;
    let mut from_egl = |name: &str| -> *const c_void {
        // SAFETY: `name` ends in NUL; the module is loaded.
        unsafe { GetProcAddress(egl_module, PCSTR(name.as_ptr())) }
            .map_or(std::ptr::null(), |f| f as *const c_void)
    };
    let egl = Egl::load(&mut from_egl)?;
    let mut from_gles = |name: &str| -> *const c_void {
        // SAFETY: as above; libGLESv2 exports every core and extension entry point.
        unsafe { GetProcAddress(gles, PCSTR(name.as_ptr())) }
            .map_or(std::ptr::null(), |f| f as *const c_void)
    };
    let gl = Gles::load(&mut from_gles)?;
    let d3d = EglD3d::load(&mut from_egl).ok();
    Ok(Angle { egl, gl, d3d })
}

#[cfg(not(windows))]
pub fn load() -> Result<Angle, String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let dir = folders()
        .into_iter()
        .find(|d| d.join("libEGL.so").is_file() && d.join("libGLESv2.so").is_file())
        .ok_or("libEGL.so and libGLESv2.so are not beside the executable")?;
    let open = |name: &str| {
        let path = CString::new(dir.join(name).as_os_str().as_bytes())
            .map_err(|_| format!("{name}'s path holds a NUL"))?;
        // SAFETY: a full path to a library, NUL-terminated. Local binding keeps ANGLE's EGL names
        // apart from the system's EGL, which ANGLE's GL back end loads itself. The library is never
        // closed.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            // SAFETY: dlerror returns null or the NUL-terminated message of the failed call.
            let why = unsafe { libc::dlerror() };
            let why = if why.is_null() {
                String::new()
            } else {
                // SAFETY: as above.
                unsafe { std::ffi::CStr::from_ptr(why) }
                    .to_string_lossy()
                    .into_owned()
            };
            return Err(format!("cannot load {name}: {why}"));
        }
        Ok(handle)
    };
    // (libEGL finds libGLESv2 beside itself; loading it first keeps the copy found here)
    let gles = open("libGLESv2.so")?;
    let egl_module = open("libEGL.so")?;
    let symbol = |module: *mut c_void, name: &str| -> *const c_void {
        // SAFETY: `name` ends in NUL; the module is loaded and stays so.
        unsafe { libc::dlsym(module, name.as_ptr().cast()) as *const c_void }
    };
    let egl = Egl::load(&mut |name| symbol(egl_module, name))?;
    // (libGLESv2 exports every core and extension entry point)
    let gl = Gles::load(&mut |name| symbol(gles, name))?;
    Ok(Angle { egl, gl })
}
