// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drawing buffers presented into another process's textures, without a readback.
//!
//! The browser makes a pool of shared textures for a canvas (as for a renderer's video: NT handles,
//! keyed mutexes), and the renderer opens them here on ANGLE's own Direct3D 11 device
//! (`EGL_EXT_device_query`, `EGL_ANGLE_device_d3d`), each as a pbuffer over the texture
//! (`EGL_ANGLE_d3d_texture_client_buffer`). To present, the context draws into a free one: with
//! that pbuffer as its draw surface and the drawing buffer as its read surface, one
//! `glBlitFramebuffer` from framebuffer 0 to framebuffer 0 copies the drawing buffer (and resolves
//! its samples, which needs the same rectangle on both sides, so it cannot flip the rows too) on
//! the GPU. The texture then holds the frame as OpenGL stores it, bottom row first: the compositor
//! draws it flipped (a surface over a texture takes no `EGL_ANGLE_surface_orientation`). A
//! multisampled context's targets get a config like its own without samples. A texture is written
//! with its key at 0 and given up with key 1 to the browser, which gives it back with key 0 when it
//! no longer shows it.
//!
//! Each present also copies the frame into a texture of the context's own (the front buffer, as a
//! drawing buffer that is not preserved has been cleared after it): a canvas that leaves the
//! shared textures shows its last frame from there ([`Device::read_front`]).

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::null_mut;

use windows::core::Interface;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device1, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;

use super::*;

const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
// (the same name as GL_FRAMEBUFFER_BINDING in OpenGL ES 3.0)
const GL_DRAW_FRAMEBUFFER_BINDING: u32 = GL_FRAMEBUFFER_BINDING;
const GL_NEAREST: u32 = 0x2600;

/// A texture on ANGLE's device and the pbuffer over it.
struct Surface {
    surface: EGLSurface,
    /// (kept alive with its surface)
    _texture: ID3D11Texture2D,
}

/// One shared texture, opened on ANGLE's device.
struct Target {
    surface: Surface,
    keyed: IDXGIKeyedMutex,
}

/// A context's shared textures, and its front buffer.
pub(super) struct Targets {
    width: u32,
    height: u32,
    list: Vec<Target>,
    /// The one to try first.
    next: usize,
    front: Surface,
}

/// `AcquireSync` with `key`, not waiting: false when another holder has it (a success code).
fn try_acquire(k: &IDXGIKeyedMutex, key: u64) -> windows::core::Result<bool> {
    // SAFETY: a call through the interface's own vtable with its own pointer.
    let hr = unsafe { (Interface::vtable(k).AcquireSync)(Interface::as_raw(k), key, 0) };
    hr.ok()?;
    Ok(hr.0 == 0)
}

fn trace(what: &str) {
    if std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1") {
        eprintln!("[webgl] {what}");
    }
}

/// The GL state a blit between framebuffers 0 changes, read before and put back after.
struct BlitState {
    draw: i32,
    read: i32,
    read_buffer: i32,
    scissor: bool,
    discard: bool,
}

impl Device {
    /// ANGLE's Direct3D 11 device, which draws every context of this display.
    fn d3d_device(&self) -> Option<ID3D11Device1> {
        let d = self.angle.d3d.as_ref()?;
        let (mut device, mut d3d): (EGLAttrib, EGLAttrib) = (0, 0);
        // SAFETY: this display; the out pointers are locals.
        if unsafe { (d.eglQueryDisplayAttribEXT)(self.display, EGL_DEVICE_EXT, &mut device) } == 0
            || device == 0
        {
            return None;
        }
        // SAFETY: the display's device; the out pointer is a local.
        if unsafe {
            (d.eglQueryDeviceAttribEXT)(device as *mut c_void, EGL_D3D11_DEVICE_ANGLE, &mut d3d)
        } == 0
            || d3d == 0
        {
            return None;
        }
        let raw = d3d as *mut c_void;
        // SAFETY: ANGLE's device, alive with the display; borrowed, then referenced by the clone.
        let device = unsafe { ID3D11Device::from_raw_borrowed(&raw) }?.clone();
        device.cast().ok()
    }

    /// A pbuffer over `texture` (of ANGLE's device) for a context of `config`.
    fn surface_over(
        &self,
        config: EGLConfig,
        texture: ID3D11Texture2D,
        w: u32,
        h: u32,
    ) -> Option<Surface> {
        let d = self.angle.d3d.as_ref()?;
        let attrs = [EGL_WIDTH, w as i32, EGL_HEIGHT, h as i32, EGL_NONE];
        let mut last = 0;
        // (the context's config, or for a multisampled one, one like it without samples: a surface
        // over a texture has none)
        for config in self.target_configs(config) {
            // SAFETY: a texture of ANGLE's own device, kept alive with the surface (`Surface`); a
            // config of this display; the list ends in EGL_NONE.
            let surface = unsafe {
                (d.eglCreatePbufferFromClientBuffer)(
                    self.display,
                    EGL_D3D_TEXTURE_ANGLE,
                    texture.as_raw(),
                    config,
                    attrs.as_ptr(),
                )
            };
            if !surface.is_null() {
                return Some(Surface {
                    surface,
                    _texture: texture,
                });
            }
            // SAFETY: no arguments.
            last = unsafe { (self.angle.egl.eglGetError)() };
        }
        trace(&format!("no surface over a texture: {last:#x}"));
        None
    }

    /// The configs a surface over a texture can have for a context of `config`: that one, then
    /// those with the same color, depth and stencil sizes and no samples.
    fn target_configs(&self, config: EGLConfig) -> Vec<EGLConfig> {
        let e = &self.angle.egl;
        let mut count = 0;
        // SAFETY: a null array asks for the count.
        unsafe { (e.eglGetConfigs)(self.display, null_mut(), 0, &mut count) };
        let mut all = vec![null_mut(); count.max(0) as usize];
        // SAFETY: the array holds `count` configs.
        unsafe { (e.eglGetConfigs)(self.display, all.as_mut_ptr(), count, &mut count) };
        all.truncate(count.max(0) as usize);
        let sizes = |c| {
            [
                EGL_RED_SIZE,
                EGL_GREEN_SIZE,
                EGL_BLUE_SIZE,
                EGL_ALPHA_SIZE,
                EGL_DEPTH_SIZE,
                EGL_STENCIL_SIZE,
            ]
            .map(|a| self.config_attrib(c, a))
        };
        let want = sizes(config);
        let mut out = vec![config];
        out.extend(all.into_iter().filter(|&c| {
            c != config
                && self.config_attrib(c, EGL_SAMPLES) == 0
                && self.config_attrib(c, EGL_SURFACE_TYPE) & EGL_PBUFFER_BIT != 0
                && sizes(c) == want
        }));
        out
    }

    /// Opens `handles` as the context's targets, with a front buffer of its own.
    fn open_targets(
        &self,
        c: &Context,
        width: u32,
        height: u32,
        handles: &[OwnedHandle],
    ) -> Option<Targets> {
        let device = self.d3d_device()?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut front = None;
        // SAFETY: a valid description; the out pointer is a local.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut front)) }.ok()?;
        let front = self.surface_over(c.config, front?, width, height)?;
        let mut targets = Targets {
            width,
            height,
            list: Vec::with_capacity(handles.len()),
            next: 0,
            front,
        };
        for handle in handles {
            // SAFETY: an open NT handle of this process's, which the runtime only reads.
            let opened = unsafe { device.OpenSharedResource1(HANDLE(handle.as_raw_handle())) };
            let texture: ID3D11Texture2D = match opened {
                Ok(t) => t,
                Err(e) => {
                    trace(&format!("a shared texture does not open: {e}"));
                    self.release_targets(targets);
                    return None;
                }
            };
            let mut desc = Default::default();
            // SAFETY: an out parameter of this function's.
            unsafe { texture.GetDesc(&mut desc) };
            let keyed = texture.cast::<IDXGIKeyedMutex>().ok();
            let fits = (desc.Width, desc.Height) == (width, height)
                && desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM
                && desc.SampleDesc.Count == 1;
            let surface = match (keyed, fits) {
                (Some(keyed), true) => self
                    .surface_over(c.config, texture, width, height)
                    .map(|surface| Target { surface, keyed }),
                _ => {
                    trace("a shared texture is not the drawing buffer's kind");
                    None
                }
            };
            let Some(target) = surface else {
                self.release_targets(targets);
                return None;
            };
            targets.list.push(target);
        }
        // The context draws with another surface as its draw surface at each present: tried once
        // here.
        let e = &self.angle.egl;
        // SAFETY: surfaces and the context of this display; the context is made current with its
        // own surface again at once (the caller forgets which context is current).
        let works = unsafe {
            let ok =
                (e.eglMakeCurrent)(self.display, targets.front.surface, c.surface, c.context) != 0;
            (e.eglMakeCurrent)(self.display, c.surface, c.surface, c.context);
            ok
        };
        if !works || targets.list.is_empty() {
            trace("a context cannot draw into a surface over a texture");
            self.release_targets(targets);
            return None;
        }
        Some(targets)
    }

    /// Destroys the targets' surfaces (their textures go with them).
    pub(super) fn release_targets(&self, targets: Targets) {
        let surfaces = targets
            .list
            .into_iter()
            .map(|t| t.surface)
            .chain([targets.front]);
        for s in surfaces {
            // SAFETY: a surface of this display, current nowhere (presents put the drawing buffer
            // back as the draw surface).
            unsafe { (self.angle.egl.eglDestroySurface)(self.display, s.surface) };
        }
    }

    pub(super) fn share_into_targets(
        &mut self,
        id: ContextId,
        targets: Option<SharedTargets>,
    ) -> bool {
        let Some(c) = self.contexts.get_mut(&id) else {
            return false;
        };
        let old = c.shared.take();
        if let Some(old) = old {
            if self.current == Some(id) {
                // (a surface of the old targets may not be current: the context's own is made
                // current again below)
                self.current = None;
            }
            self.release_targets(old);
        }
        let Some(t) = targets else {
            return false;
        };
        let c = &self.contexts[&id];
        let a = c.attributes;
        // (WebGL 1.0's OpenGL ES 2.0 context has no blit; frames that are not premultiplied would
        // need converting)
        if !a.webgl2
            || !a.premultiplied_alpha
            || t.handles.is_empty()
            || (t.width, t.height) != (c.width.max(1), c.height.max(1))
        {
            return false;
        }
        let opened = self.open_targets(c, t.width, t.height, &t.handles);
        // (opening made the context current)
        self.current = None;
        let ok = opened.is_some();
        self.contexts.get_mut(&id).expect("looked up above").shared = opened;
        ok
    }

    /// Saves the state [`Device::blit_whole`] changes.
    fn blit_state(&self) -> BlitState {
        let int = |v: Vec<i32>| v.first().copied().unwrap_or(0);
        BlitState {
            draw: int(self.get_ints(GL_DRAW_FRAMEBUFFER_BINDING, 1)),
            read: int(self.get_ints(GL_READ_FRAMEBUFFER_BINDING, 1)),
            read_buffer: int(self.get_ints(GL_READ_BUFFER, 1)),
            scissor: self.get_bools(GL_SCISSOR_TEST, 1).first() == Some(&true),
            discard: self.get_bools(GL_RASTERIZER_DISCARD, 1).first() == Some(&true),
        }
    }

    /// With `draw` as the draw surface and `read` as the read one (of the current context, then
    /// its own surface again): framebuffer 0 copied whole from one to the other. The GL error it
    /// raised (0: none).
    fn blit_whole(&self, c: &Context, draw: EGLSurface, read: EGLSurface, w: i32, h: i32) -> u32 {
        let (e, gl) = (&self.angle.egl, &self.angle.gl);
        // SAFETY: surfaces and the context of this display, alive; plain state calls and a blit on
        // the current context, the state put back after.
        unsafe {
            if (e.eglMakeCurrent)(self.display, draw, read, c.context) == 0 {
                // (EGL_BAD_MATCH, as a GL error would say; never 0)
                return 0x3009;
            }
            let s = self.blit_state();
            (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, 0);
            (gl.glBindFramebuffer)(GL_DRAW_FRAMEBUFFER, 0);
            (gl.glReadBuffer)(GL_BACK);
            (gl.glDisable)(GL_SCISSOR_TEST);
            (gl.glDisable)(GL_RASTERIZER_DISCARD);
            (gl.glBlitFramebuffer)(0, 0, w, h, 0, 0, w, h, GL_COLOR_BUFFER_BIT, GL_NEAREST);
            let error = (gl.glGetError)();
            if s.scissor {
                (gl.glEnable)(GL_SCISSOR_TEST);
            }
            if s.discard {
                (gl.glEnable)(GL_RASTERIZER_DISCARD);
            }
            (gl.glReadBuffer)(s.read_buffer as u32);
            (gl.glBindFramebuffer)(GL_DRAW_FRAMEBUFFER, s.draw as u32);
            (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, s.read as u32);
            (e.eglMakeCurrent)(self.display, c.surface, c.surface, c.context);
            error
        }
    }

    pub(super) fn present_into_target(&mut self, id: ContextId) -> SharedPresent {
        if !self.make_current(id) {
            return SharedPresent::Unavailable;
        }
        let c = &self.contexts[&id];
        let Some(targets) = c.shared.as_ref() else {
            return SharedPresent::Unavailable;
        };
        let (w, h) = (targets.width as i32, targets.height as i32);
        let n = targets.list.len();
        let mut free = None;
        for k in 0..n {
            let j = (targets.next + k) % n;
            match try_acquire(&targets.list[j].keyed, 0) {
                Ok(true) => {
                    free = Some(j);
                    break;
                }
                Ok(false) => {}
                Err(e) => {
                    trace(&format!("a shared texture's key cannot be had: {e}"));
                    return SharedPresent::Unavailable;
                }
            }
        }
        let Some(j) = free else {
            return SharedPresent::Busy;
        };
        let target = &targets.list[j];
        // (the page's own error is kept for its next getError; the blits' are the service's)
        let pending = self.take_page_error();
        let mut error = self.blit_whole(c, target.surface.surface, c.surface, w, h);
        if error == 0 {
            error = self.blit_whole(c, targets.front.surface, c.surface, w, h);
        }
        // SAFETY: no arguments: the copies are submitted before the key is given up.
        unsafe { (self.angle.gl.glFlush)() };
        let presented = error == 0;
        // SAFETY: the key was taken above; the browser takes it with key 1.
        let _ = unsafe { target.keyed.ReleaseSync(if presented { 1 } else { 0 }) };
        let attributes = c.attributes;
        self.hold_page_error(id, pending);
        if !presented {
            trace(&format!("the drawing buffer does not copy: {error:#x}"));
            return SharedPresent::Unavailable;
        }
        if let Some(t) = self.contexts.get_mut(&id).and_then(|c| c.shared.as_mut()) {
            t.next = (j + 1) % n;
        }
        if !attributes.preserve_drawing_buffer {
            self.clear_drawing_buffer(&attributes);
        }
        SharedPresent::Presented(j as u32)
    }

    /// The last frame presented into the targets, from the front buffer, as [`Backend::present`]
    /// gives it (rows bottom to top).
    pub(super) fn read_front(&mut self, id: ContextId) -> Option<Readback> {
        if !self.make_current(id) {
            return None;
        }
        let c = &self.contexts[&id];
        let t = c.shared.as_ref()?;
        let (w, h) = (t.width, t.height);
        let (e, gl) = (&self.angle.egl, &self.angle.gl);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let pending = self.take_page_error();
        // SAFETY: the front buffer's surface and the context of this display; the buffer holds
        // w*h*4 bytes and ANGLE is told so; the state is put back after.
        let read = unsafe {
            if (e.eglMakeCurrent)(self.display, t.front.surface, t.front.surface, c.context) == 0 {
                false
            } else {
                let s = self.blit_state();
                let pack = self.get_ints(GL_PACK_ALIGNMENT, 1);
                let pack_buffer = self.get_ints(GL_PIXEL_PACK_BUFFER_BINDING, 1);
                let rows = [GL_PACK_ROW_LENGTH, GL_PACK_SKIP_ROWS, GL_PACK_SKIP_PIXELS]
                    .map(|p| self.get_ints(p, 1).first().copied().unwrap_or(0));
                (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, 0);
                (gl.glReadBuffer)(GL_BACK);
                (gl.glBindBuffer)(GL_PIXEL_PACK_BUFFER, 0);
                (gl.glPixelStorei)(GL_PACK_ROW_LENGTH, 0);
                (gl.glPixelStorei)(GL_PACK_SKIP_ROWS, 0);
                (gl.glPixelStorei)(GL_PACK_SKIP_PIXELS, 0);
                (gl.glPixelStorei)(GL_PACK_ALIGNMENT, 4);
                let (mut written, mut cols, mut rows_read) = (0, 0, 0);
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
                    &mut rows_read,
                    rgba.as_mut_ptr().cast(),
                );
                let ok = (gl.glGetError)() == 0;
                (gl.glPixelStorei)(GL_PACK_ALIGNMENT, pack.first().copied().unwrap_or(4));
                (gl.glPixelStorei)(GL_PACK_ROW_LENGTH, rows[0]);
                (gl.glPixelStorei)(GL_PACK_SKIP_ROWS, rows[1]);
                (gl.glPixelStorei)(GL_PACK_SKIP_PIXELS, rows[2]);
                (gl.glBindBuffer)(
                    GL_PIXEL_PACK_BUFFER,
                    pack_buffer.first().copied().unwrap_or(0) as u32,
                );
                (gl.glReadBuffer)(s.read_buffer as u32);
                (gl.glBindFramebuffer)(GL_READ_FRAMEBUFFER, s.read as u32);
                (e.eglMakeCurrent)(self.display, c.surface, c.surface, c.context);
                ok
            }
        };
        self.hold_page_error(id, pending);
        read.then_some(Readback {
            width: w,
            height: h,
            rgba,
        })
    }
}
