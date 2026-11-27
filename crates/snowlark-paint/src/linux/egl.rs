// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! EGL and OpenGL ES from the system's libraries, loaded at run time (`libEGL.so.1`, then
//! `libGLESv2.so.2` for a function EGL does not hand out), so a system without them still runs
//! the CPU path.
//!
//! One display per process, made once and kept (terminating it would end every thread's contexts
//! on it): Mesa's surfaceless platform (`EGL_MESA_platform_surfaceless`), which needs no window
//! system, with a configuration of 8-bit RGBA and an 8-bit stencil. A [`Context`] is an OpenGL ES 3
//! context (2 where 3 cannot be made) current on the thread that made it, with no surface
//! (`EGL_KHR_surfaceless_context`): it draws into framebuffers of its own.

use std::ffi::{c_char, c_void, CStr, CString};
use std::marker::PhantomData;
use std::sync::OnceLock;

type Ptr = *mut c_void;

const NONE: i32 = 0x3038;
const ALPHA_SIZE: i32 = 0x3021;
const BLUE_SIZE: i32 = 0x3022;
const GREEN_SIZE: i32 = 0x3023;
const RED_SIZE: i32 = 0x3024;
const STENCIL_SIZE: i32 = 0x3026;
const SURFACE_TYPE: i32 = 0x3033;
const RENDERABLE_TYPE: i32 = 0x3040;
const OPENGL_ES2_BIT: i32 = 0x0004;
const OPENGL_ES_API: u32 = 0x30A0;
const CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const EXTENSIONS: i32 = 0x3055;
const PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
const GL_RENDERER: u32 = 0x1F01;

/// The EGL entry points used here: each field is the function of that name, of that C signature
/// (EGL 1.5, `EGL/egl.h`).
macro_rules! entry_points {
    ($($field:ident: $name:literal => $ty:ty,)*) => {
        struct Fns {
            $($field: $ty,)*
        }

        impl Fns {
            fn load(lib: &Library) -> Result<Fns, String> {
                Ok(Fns {
                    $($field: {
                        let p = lib.symbol($name);
                        if p.is_null() {
                            return Err(format!("libEGL.so.1 has no {}", $name.to_string_lossy()));
                        }
                        // SAFETY: the symbol is the EGL function of that name, whose C signature
                        // is the field's type.
                        unsafe { std::mem::transmute::<Ptr, $ty>(p) }
                    },)*
                })
            }
        }
    };
}

entry_points! {
    get_proc_address: c"eglGetProcAddress" => unsafe extern "C" fn(*const c_char) -> Ptr,
    get_platform_display: c"eglGetPlatformDisplay"
        => unsafe extern "C" fn(u32, Ptr, *const isize) -> Ptr,
    initialize: c"eglInitialize" => unsafe extern "C" fn(Ptr, *mut i32, *mut i32) -> u32,
    bind_api: c"eglBindAPI" => unsafe extern "C" fn(u32) -> u32,
    choose_config: c"eglChooseConfig"
        => unsafe extern "C" fn(Ptr, *const i32, *mut Ptr, i32, *mut i32) -> u32,
    create_context: c"eglCreateContext" => unsafe extern "C" fn(Ptr, Ptr, Ptr, *const i32) -> Ptr,
    make_current: c"eglMakeCurrent" => unsafe extern "C" fn(Ptr, Ptr, Ptr, Ptr) -> u32,
    get_current_context: c"eglGetCurrentContext" => unsafe extern "C" fn() -> Ptr,
    destroy_context: c"eglDestroyContext" => unsafe extern "C" fn(Ptr, Ptr) -> u32,
    get_error: c"eglGetError" => unsafe extern "C" fn() -> i32,
    query_string: c"eglQueryString" => unsafe extern "C" fn(Ptr, i32) -> *const c_char,
}

/// A library opened with `dlopen`, kept for the process.
struct Library(Ptr);

// SAFETY: a `dlopen` handle is a process-wide token; `dlsym` on it is thread-safe.
unsafe impl Send for Library {}
// SAFETY: as above.
unsafe impl Sync for Library {}

impl Library {
    fn open(name: &CStr) -> Option<Library> {
        // SAFETY: `name` ends in a NUL; the handle is never closed (the process keeps the library).
        let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        (!h.is_null()).then_some(Library(h))
    }

    fn symbol(&self, name: &CStr) -> Ptr {
        // SAFETY: the handle is open for the process's life; `name` ends in a NUL.
        unsafe { libc::dlsym(self.0, name.as_ptr()) }
    }
}

/// The process's display (see the module documentation).
pub(crate) struct Display {
    fns: Fns,
    gles: Option<Library>,
    display: Ptr,
    config: Ptr,
    /// The GL renderer's name (`GL_RENDERER`), for diagnostics.
    renderer: String,
}

// SAFETY: an EGLDisplay and EGLConfig are process-wide handles that EGL lets any thread use;
// nothing here is tied to the thread that made it.
unsafe impl Send for Display {}
// SAFETY: as above.
unsafe impl Sync for Display {}

static DISPLAY: OnceLock<Result<Display, String>> = OnceLock::new();

/// The last EGL error, as text.
fn error(fns: &Fns, what: &str) -> String {
    // SAFETY: eglGetError takes nothing and reads the calling thread's error.
    format!("{what} failed (EGL error {:#x})", unsafe {
        (fns.get_error)()
    })
}

impl Display {
    /// The process's display, made the first time it is asked for.
    pub(crate) fn get() -> Result<&'static Display, String> {
        DISPLAY
            .get_or_init(Display::open)
            .as_ref()
            .map_err(Clone::clone)
    }

    fn open() -> Result<Display, String> {
        // (never closed: a `Library` has no `Drop`, and its functions are in `fns`)
        let lib = Library::open(c"libEGL.so.1").ok_or("no libEGL.so.1 on this system")?;
        let fns = Fns::load(&lib)?;
        // SAFETY: each call is EGL's documented use: a platform display with no native display
        // and no attributes, initialized once, then a configuration chosen from a NONE-ended
        // list into a one-element array whose count is read back.
        unsafe {
            let client = (fns.query_string)(std::ptr::null_mut(), EXTENSIONS);
            let has_platform = !client.is_null()
                && CStr::from_ptr(client)
                    .to_string_lossy()
                    .split(' ')
                    .any(|e| e == "EGL_MESA_platform_surfaceless");
            if !has_platform {
                return Err("EGL has no surfaceless platform".into());
            }
            let display = (fns.get_platform_display)(
                PLATFORM_SURFACELESS_MESA,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if display.is_null() {
                return Err(error(&fns, "eglGetPlatformDisplay"));
            }
            let (mut major, mut minor) = (0, 0);
            if (fns.initialize)(display, &mut major, &mut minor) == 0 {
                return Err(error(&fns, "eglInitialize"));
            }
            let attributes = [
                SURFACE_TYPE,
                0,
                RENDERABLE_TYPE,
                OPENGL_ES2_BIT,
                RED_SIZE,
                8,
                GREEN_SIZE,
                8,
                BLUE_SIZE,
                8,
                ALPHA_SIZE,
                8,
                STENCIL_SIZE,
                8,
                NONE,
            ];
            let mut config: Ptr = std::ptr::null_mut();
            let mut count = 0;
            if (fns.choose_config)(display, attributes.as_ptr(), &mut config, 1, &mut count) == 0
                || count < 1
            {
                return Err(error(&fns, "eglChooseConfig"));
            }
            let mut d = Display {
                fns,
                gles: Library::open(c"libGLESv2.so.2"),
                display,
                config,
                renderer: String::new(),
            };
            // The renderer's name, from a context made for it.
            let c = Context::new(&d)?;
            d.renderer = c.renderer();
            drop(c);
            Ok(d)
        }
    }

    /// The GL renderer's name.
    pub(crate) fn renderer(&self) -> &str {
        &self.renderer
    }

    /// The address of the GL or EGL function `name`, or null.
    fn proc_address(&self, name: &str) -> *const c_void {
        let Ok(name) = CString::new(name) else {
            return std::ptr::null();
        };
        // SAFETY: `name` ends in a NUL; eglGetProcAddress returns null for a name it does not know.
        let p = unsafe { (self.fns.get_proc_address)(name.as_ptr()) };
        if !p.is_null() {
            return p;
        }
        self.gles
            .as_ref()
            .map_or(std::ptr::null_mut(), |l| l.symbol(&name))
    }
}

/// An OpenGL ES context current on the thread that made it (see the module documentation).
pub(crate) struct Context {
    display: *const Display,
    context: Ptr,
    // (current on one thread)
    _thread_affine: PhantomData<*const ()>,
}

impl Context {
    /// A context on the process's display, made current on this thread with no surface.
    pub(crate) fn current() -> Result<Context, String> {
        Context::new(Display::get()?)
    }

    fn new(d: &Display) -> Result<Context, String> {
        let f = &d.fns;
        // SAFETY: the display is initialized and its configuration came from it; the attribute
        // lists end in NONE; the context is made current on this thread with no surface (Mesa's
        // surfaceless platform has EGL_KHR_surfaceless_context).
        let context = unsafe {
            if (f.bind_api)(OPENGL_ES_API) == 0 {
                return Err(error(f, "eglBindAPI"));
            }
            let mut context = std::ptr::null_mut();
            for version in [3, 2] {
                let attributes = [CONTEXT_MAJOR_VERSION, version, NONE];
                context = (f.create_context)(
                    d.display,
                    d.config,
                    std::ptr::null_mut(),
                    attributes.as_ptr(),
                );
                if !context.is_null() {
                    break;
                }
            }
            if context.is_null() {
                return Err(error(f, "eglCreateContext"));
            }
            let none = std::ptr::null_mut();
            if (f.make_current)(d.display, none, none, context) == 0 {
                let e = error(f, "eglMakeCurrent");
                (f.destroy_context)(d.display, context);
                return Err(e);
            }
            context
        };
        Ok(Context {
            display: d,
            context,
            _thread_affine: PhantomData,
        })
    }

    fn display(&self) -> &Display {
        // SAFETY: `display` is the process's display (kept for the process's life) or, while the
        // display is being made, the one `Display::open` owns and outlives this context in.
        unsafe { &*self.display }
    }

    /// Makes this context current again (another context of this thread may have been since).
    pub(crate) fn make_current(&self) {
        let d = self.display();
        let none = std::ptr::null_mut();
        // SAFETY: the context belongs to this thread and was current on it before; making it
        // current with no surface is what `new` did.
        unsafe {
            if (d.fns.get_current_context)() != self.context {
                (d.fns.make_current)(d.display, none, none, self.context);
            }
        }
    }

    /// The address of the GL function `name` for this context, or null (for Skia's interface).
    pub(crate) fn proc_address(&self, name: &str) -> *const c_void {
        self.display().proc_address(name)
    }

    /// `GL_RENDERER`.
    fn renderer(&self) -> String {
        let p = self.proc_address("glGetString");
        if p.is_null() {
            return String::new();
        }
        // SAFETY: `p` is glGetString (`const GLubyte *glGetString(GLenum)`), called with this
        // context current; it returns a NUL-ended string GL owns, or null.
        unsafe {
            let get: unsafe extern "C" fn(u32) -> *const c_char = std::mem::transmute(p);
            let s = get(GL_RENDERER);
            if s.is_null() {
                String::new()
            } else {
                CStr::from_ptr(s).to_string_lossy().into_owned()
            }
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        let d = self.display();
        let none = std::ptr::null_mut();
        // SAFETY: the context belongs to this thread: released if it is the current one (another
        // context of the thread stays current), then destroyed.
        unsafe {
            if (d.fns.get_current_context)() == self.context {
                (d.fns.make_current)(d.display, none, none, none);
            }
            (d.fns.destroy_context)(d.display, self.context);
        }
    }
}
