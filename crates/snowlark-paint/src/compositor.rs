// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's compositor: a DirectComposition visual tree over a window ([`crate::window`]) with
//! two layers, drawn on the shared [`Gpu`] device from one thread.
//!
//! ```text
//! root
//! ├── under     protected players' pictures under the content (underlays), over a black
//! │             background; the content shows them through holes — `set_underlays`
//! ├── content   a flip-model swap chain (waitable, frame latency 1) below the chrome: each frame
//! │             it receives the active tab's tiles at the scroll position, then overlays (scroll
//! │             bars, the status bubble) — `draw_content` + `present`
//! └── chrome    a composition surface on top (tab strip, toolbar), redrawn only when the chrome
//!               changes — `draw_chrome`
//! ```
//!
//! Scrolling only re-composes tiles that are already on the GPU (a dozen bitmap copies), so a frame
//! costs well under a millisecond and goes out at the display's rate. Both layers are opaque, so text
//! in them keeps ClearType.
//!
//! The waitable-swap-chain rule (the C# engine's lesson): wait for the swap chain *before* drawing
//! ([`Compositor::wait_for_frame`]) so that `Present` never blocks, and never present while holding
//! Direct2D's device lock (see [`Compositor::present`]): tab threads rasterizing on the same device
//! never stall behind the screen, and the screen never waits for them.
//!
//! A swap chain shown through DirectComposition (rather than bound to the window) is composed by DWM
//! at the display's steady rate: on a variable-refresh monitor a window swap chain made the refresh
//! rate follow our frame pace while scrolling, and some panels then showed flickering lines.
//!
//! A protected player's frames cannot be drawn: its engine presents them into a composition surface
//! of its own, which the tree shows under the content where the page's video is, and the content
//! is cleared to transparent there ([`ContentPass::hole`]), so whatever the page or the browser
//! draws over the video still shows over it. The content chain takes premultiplied alpha for that
//! once a window first shows such a video ([`Compositor::allow_underlays`]); until then it stays
//! opaque.
//!
//! Across processes ([`Compositor::on_surface`], `docs/process-isolation.md` M7) the window's process
//! keeps the visual tree, with no Direct3D device ([`WindowSurface`]), and the compositor presents
//! one swap chain for the whole client area through a composition surface handle bound to it. The
//! chrome is then a bitmap drawn into every frame above the content, so no composition can show the
//! chrome at one size and the content at another (two layers presented apart can: P0 measured 2.9%
//! of compositions during a resize).

use std::collections::HashMap;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{IUnknown, Interface, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, POINT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows_numerics::Matrix3x2;

use crate::display_list::{matrix_of, radii_of, DisplayList, LayerState, StateOp};
use crate::geom::{Color, Matrix, Radii, Rect};
use crate::gpu::{Gpu, TiledFrame, TILE_SIZE};
use crate::raster::{
    clip_geometry, color_f, matrix_3x2, rect_f, FontResolver, Painter, Resources, Sources,
};
use crate::window::WindowHandle;

const CHAIN_FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;

/// The window's layers (see the module documentation). Thread-affine: make it on the thread that
/// will draw.
pub struct Compositor {
    gpu: Arc<Gpu>,
    ctx: ID2D1DeviceContext,
    res: Resources,
    layers: Layers,
    window: WindowHandle,
    chain: IDXGISwapChain2,
    /// The swap chain's buffer size.
    chain_size: (u32, u32),
    waitable: HANDLE,
    back: Option<ID2D1Bitmap1>,
    content_size: (u32, u32),
    chrome_size: (u32, u32),
    /// The content chain has premultiplied alpha ([`Compositor::allow_underlays`]).
    alpha: bool,
    /// A new chain to show with the next present, and visual changes to commit after it.
    pending: Pending,
    _thread_affine: PhantomData<*const ()>,
}

#[derive(Default)]
struct Pending {
    chain: bool,
    commit: bool,
}

/// A protected player's picture under the content ([`Compositor::set_underlays`]), in device
/// pixels of the content area.
pub struct Underlay {
    /// The player (the same one from frame to frame).
    pub key: u64,
    /// Its engine's composition surface.
    pub surface: Arc<OwnedHandle>,
    /// Where the picture goes (the surface's size is the engine's).
    pub dest: Rect,
    /// What of it shows.
    pub clip: Rect,
}

/// One underlay's visuals: a clip holding the picture.
struct UnderlayVisual {
    holder: IDCompositionVisual,
    picture: IDCompositionVisual,
    surface: Arc<OwnedHandle>,
    /// Its surface as this device has it.
    _content: IUnknown,
    placed: (Rect, Rect),
}

/// The visuals under the content ([`Layers::Tree`]).
struct Under {
    visual: IDCompositionVisual,
    /// A black surface scaled over the content area, under the pictures (none while there are
    /// no pictures).
    background: IDCompositionVisual,
    black: Option<IDCompositionSurface>,
    pictures: HashMap<u64, UnderlayVisual>,
}

/// The black background's surface size.
const BLACK: u32 = 8;

/// How the chain and the chrome reach the screen.
enum Layers {
    /// A visual tree of the compositor's own over the window: the content chain below the chrome's
    /// composition surface ([`Compositor::new`]).
    Tree {
        dcomp: IDCompositionDevice,
        _target: IDCompositionTarget,
        _root: IDCompositionVisual,
        content: IDCompositionVisual,
        chrome: IDCompositionVisual,
        surface: Option<IDCompositionSurface>,
        under: Box<Under>,
    },
    /// One chain for the whole client area, shown through a composition surface handle of the
    /// window's visual tree ([`Compositor::on_surface`]); the chrome is a bitmap drawn into each
    /// frame.
    Surface {
        _handle: OwnedHandle,
        chrome: Option<ID2D1Bitmap1>,
    },
}

/// Drawing into the content layer's back buffer (device pixels of the content area).
pub struct ContentPass<'a> {
    ctx: &'a ID2D1DeviceContext,
    res: &'a mut Resources,
    /// The buffer being drawn into.
    target: &'a ID2D1Bitmap1,
    size: (u32, u32),
    /// Where the content area's pixel (0, 0) is in the buffer (below the chrome, in one chain).
    origin: (i32, i32),
}

/// The window's side of [`Compositor::on_surface`]: a visual tree over the window with no Direct3D
/// device, showing a composition surface that another process (or thread) presents a swap chain
/// into through [`WindowSurface::handle`]. Nothing here is done per frame: the swap chain's
/// presents reach DWM without this process. Clones are the same tree.
///
/// A surface takes one swap chain for good: once a chain's process has ended, no other chain can
/// be bound to it (`DXGI_ERROR_ALREADY_EXISTS`). A compositor in a process started again gets a
/// new surface ([`WindowSurface::renew`]); until its first present, the window shows the last
/// picture of the one before.
///
/// ```text
/// root   the surface before the newest, frozen at its last present (none at first)
/// └── top   the surface the compositor presents into now
/// ```
#[derive(Clone)]
pub struct WindowSurface(Arc<std::sync::Mutex<SurfaceTree>>);

struct SurfaceTree {
    dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    root: IDCompositionVisual,
    top: IDCompositionVisual,
    /// The surface presented into now, and its handle.
    handle: OwnedHandle,
    surface: IUnknown,
    /// The one before, shown below until the new one has a picture.
    frozen: Option<IUnknown>,
}

// SAFETY: DirectComposition's objects are free-threaded (made by its flat functions, not bound to
// a COM apartment), and every call on them goes through the WindowSurface's mutex.
unsafe impl Send for SurfaceTree {}

/// A new composition surface of `dcomp`, and its handle (ours to close, which `OwnedHandle` does).
///
/// # Safety
/// `dcomp` must be a live device.
unsafe fn new_surface(dcomp: &IDCompositionDevice) -> Result<(OwnedHandle, IUnknown)> {
    let raw = DCompositionCreateSurfaceHandle(COMPOSITIONOBJECT_ALL_ACCESS, None)?;
    let handle = OwnedHandle::from_raw_handle(raw.0);
    let surface = dcomp.CreateSurfaceFromHandle(HANDLE(handle.as_raw_handle()))?;
    Ok((handle, surface))
}

impl WindowSurface {
    /// Binds a visual tree to `window` (a window of this process) showing a new composition surface.
    pub fn new(window: WindowHandle) -> Result<WindowSurface> {
        // SAFETY: object creation; every interface is owned by the WindowSurface.
        unsafe {
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(None::<&IDXGIDevice>)?;
            let target = dcomp.CreateTargetForHwnd(window.hwnd(), true)?;
            let root = dcomp.CreateVisual()?;
            let top = dcomp.CreateVisual()?;
            let (handle, surface) = new_surface(&dcomp)?;
            top.SetContent(&surface)?;
            root.AddVisual(&top, true, None)?;
            target.SetRoot(&root)?;
            dcomp.Commit()?;
            Ok(WindowSurface(Arc::new(std::sync::Mutex::new(
                SurfaceTree {
                    dcomp,
                    _target: target,
                    root,
                    top,
                    handle,
                    surface,
                    frozen: None,
                },
            ))))
        }
    }

    fn tree(&self) -> std::sync::MutexGuard<'_, SurfaceTree> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A handle to the surface for the compositor that presents into it (its own copy, which it
    /// closes when done).
    pub fn handle(&self) -> std::io::Result<OwnedHandle> {
        self.tree().handle.try_clone()
    }

    /// Shows a new surface above the current one, which keeps its last picture below until the new
    /// one has one, and returns a handle to it (as [`WindowSurface::handle`]): for a compositor in
    /// a process started after the one presenting into the current surface ended.
    pub fn renew(&self) -> Result<OwnedHandle> {
        let mut t = self.tree();
        // SAFETY: our own device and visuals; the surface before stays referenced by `frozen`
        // while the root shows it.
        unsafe {
            let (handle, surface) = new_surface(&t.dcomp)?;
            t.root.SetContent(&t.surface)?;
            t.top.SetContent(&surface)?;
            t.dcomp.Commit()?;
            t.frozen = Some(std::mem::replace(&mut t.surface, surface));
            t.handle = handle;
        }
        t.handle.try_clone().map_err(|e| {
            windows::core::HRESULT::from_win32(e.raw_os_error().unwrap_or(0) as u32).into()
        })
    }

    /// Shows nothing until a chain presents into the surface again: for a window going away.
    pub fn clear(&self) -> Result<()> {
        let mut t = self.tree();
        t.frozen = None;
        // SAFETY: changes to our own visuals, committed at once.
        unsafe {
            t.root.SetContent(None)?;
            t.top.SetContent(None)?;
            t.dcomp.Commit()
        }
    }
}

const COMPOSITIONOBJECT_ALL_ACCESS: u32 = 0x0003;

fn chain_desc(width: u32, height: u32) -> DXGI_SWAP_CHAIN_DESC1 {
    DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: CHAIN_FLAGS.0 as u32,
    }
}

fn bitmap_props(options: D2D1_BITMAP_OPTIONS) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_IGNORE,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: options,
        colorContext: ManuallyDrop::new(None),
    }
}

impl Compositor {
    /// Binds a visual tree to `window`: a `width` × `height` client area whose top `chrome_height`
    /// rows are the chrome. `text` resolves the fonts of the chrome's and overlays' display lists.
    pub fn new(
        gpu: Arc<Gpu>,
        text: &dyn FontResolver,
        window: WindowHandle,
        width: u32,
        height: u32,
        chrome_height: u32,
    ) -> Result<Compositor> {
        let chrome_height = chrome_height.min(height);
        let content_size = (width.max(1), (height - chrome_height).max(1));
        // SAFETY: object creation on the shared device; every interface returned is owned by the
        // Compositor and released with it. The swap chain's waitable handle is closed in Drop.
        unsafe {
            let ctx = gpu
                .d2d_device()
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let res = Resources::new(&ctx, text)?;
            let dxgi_device: IDXGIDevice = gpu.d3d_device().cast()?;
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
            let target = dcomp.CreateTargetForHwnd(window.hwnd(), true)?;
            let root = dcomp.CreateVisual()?;
            let content = dcomp.CreateVisual()?;
            let chrome = dcomp.CreateVisual()?;
            let under = dcomp.CreateVisual()?;
            let background = dcomp.CreateVisual()?;
            let chain1 = gpu.dxgi_factory().CreateSwapChainForComposition(
                gpu.d3d_device(),
                &chain_desc(content_size.0, content_size.1),
                None,
            )?;
            let chain: IDXGISwapChain2 = chain1.cast()?;
            chain.SetMaximumFrameLatency(1)?;
            let waitable = chain.GetFrameLatencyWaitableObject();
            content.SetContent(&chain)?;
            content.SetOffsetY2(chrome_height as f32)?;
            under.SetOffsetY2(chrome_height as f32)?;
            under.AddVisual(&background, false, None)?;
            root.AddVisual(&content, false, None)?;
            root.AddVisual(&chrome, true, &content)?;
            root.AddVisual(&under, false, &content)?;
            target.SetRoot(&root)?;
            let mut c = Compositor {
                gpu,
                ctx,
                res,
                layers: Layers::Tree {
                    dcomp,
                    _target: target,
                    _root: root,
                    content,
                    chrome,
                    surface: None,
                    under: Box::new(Under {
                        visual: under,
                        background,
                        black: None,
                        pictures: HashMap::new(),
                    }),
                },
                window,
                chain,
                chain_size: content_size,
                waitable,
                back: None,
                content_size,
                chrome_size: (0, 0),
                alpha: false,
                pending: Pending::default(),
                _thread_affine: PhantomData,
            };
            c.make_chrome_surface(width, chrome_height)?;
            c.commit()?;
            Ok(c)
        }
    }

    /// Presents into the composition surface `surface` (a handle from the window's
    /// [`WindowSurface`], in this process or duplicated from the window's): one swap chain for the
    /// `width` × `height` client area of `window`, the chrome drawn into its top `chrome_height` rows
    /// with every frame.
    pub fn on_surface(
        gpu: Arc<Gpu>,
        text: &dyn FontResolver,
        window: WindowHandle,
        surface: OwnedHandle,
        width: u32,
        height: u32,
        chrome_height: u32,
    ) -> Result<Compositor> {
        let chrome_height = chrome_height.min(height);
        let size = (width.max(1), height.max(1));
        // SAFETY: object creation on the shared device; every interface returned is owned by the
        // Compositor. The chain holds its own reference to the surface, which `surface` names; the
        // handle stays open while the Compositor lives. The waitable handle is closed in Drop.
        unsafe {
            let ctx = gpu
                .d2d_device()
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let res = Resources::new(&ctx, text)?;
            let media: IDXGIFactoryMedia = gpu.dxgi_factory().cast()?;
            let chain1 = media.CreateSwapChainForCompositionSurfaceHandle(
                gpu.d3d_device(),
                Some(HANDLE(surface.as_raw_handle())),
                &chain_desc(size.0, size.1),
                None,
            )?;
            let chain: IDXGISwapChain2 = chain1.cast()?;
            chain.SetMaximumFrameLatency(1)?;
            let waitable = chain.GetFrameLatencyWaitableObject();
            Ok(Compositor {
                gpu,
                ctx,
                res,
                layers: Layers::Surface {
                    _handle: surface,
                    chrome: None,
                },
                window,
                chain,
                chain_size: size,
                waitable,
                back: None,
                content_size: (size.0, (size.1 - chrome_height).max(1)),
                chrome_size: (size.0, chrome_height),
                alpha: false,
                pending: Pending::default(),
                _thread_affine: PhantomData,
            })
        }
    }

    /// The window the visual tree is bound to.
    pub fn window(&self) -> WindowHandle {
        self.window
    }

    pub fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
    }

    /// The content layer's size in device pixels.
    pub fn content_size(&self) -> (u32, u32) {
        self.content_size
    }

    /// Whether the chrome is drawn into each content frame ([`Compositor::on_surface`]): a change
    /// to it shows with the next [`Compositor::present`], not with [`Compositor::draw_chrome`].
    pub fn chrome_in_frame(&self) -> bool {
        matches!(self.layers, Layers::Surface { .. })
    }

    fn make_chrome_surface(&mut self, width: u32, height: u32) -> Result<()> {
        let unchanged = (width, height) == self.chrome_size;
        self.chrome_size = (width, height);
        let (dcomp, chrome, surface) = match &mut self.layers {
            Layers::Tree {
                dcomp,
                chrome,
                surface,
                ..
            } => (dcomp, chrome, surface),
            Layers::Surface { chrome, .. } => {
                // (a bitmap of the new size is made when the chrome is drawn next)
                if !unchanged {
                    *chrome = None;
                }
                return Ok(());
            }
        };
        if unchanged && surface.is_some() {
            return Ok(());
        }
        // SAFETY: creating a surface and making it the chrome visual's content.
        unsafe {
            if width == 0 || height == 0 {
                chrome.SetContent(None)?;
                *surface = None;
            } else {
                let s = dcomp.CreateSurface(
                    width,
                    height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_IGNORE,
                )?;
                chrome.SetContent(&s)?;
                *surface = Some(s);
            }
        }
        Ok(())
    }

    /// A new client size: the chrome takes the top `chrome_height` rows, the content the rest. The
    /// chrome must be drawn again afterwards (its surface is new when its size changed), and nothing
    /// is committed here: the caller draws the content and the chrome at the new size first, then
    /// [`Compositor::draw_chrome`] (or [`Compositor::commit`]) and [`Compositor::present`] show
    /// both together, so no frame shows one layer at the new size and the other at the old.
    pub fn resize(&mut self, width: u32, height: u32, chrome_height: u32) -> Result<()> {
        let chrome_height = chrome_height.min(height);
        let content = (width.max(1), (height - chrome_height).max(1));
        let chain = match self.layers {
            Layers::Tree { .. } => content,
            Layers::Surface { .. } => (width.max(1), height.max(1)),
        };
        if chain != self.chain_size {
            self.back = None;
            // SAFETY: no reference to the buffers is held (the target bitmap was dropped above).
            unsafe {
                self.chain
                    .ResizeBuffers(2, chain.0, chain.1, DXGI_FORMAT_UNKNOWN, CHAIN_FLAGS)?;
            }
            self.chain_size = chain;
        }
        self.content_size = content;
        self.make_chrome_surface(width, chrome_height)?;
        if let Layers::Tree { content, under, .. } = &self.layers {
            // SAFETY: property changes on our own visuals (committed by the caller).
            unsafe {
                content.SetOffsetY2(chrome_height as f32)?;
                under.visual.SetOffsetY2(chrome_height as f32)?;
            }
        }
        self.cover()?;
        Ok(())
    }

    /// Commits visual tree changes (made by [`Compositor::resize`]) without drawing the chrome.
    pub fn commit(&self) -> Result<()> {
        match &self.layers {
            // SAFETY: a commit of our own device's changes.
            Layers::Tree { dcomp, .. } => unsafe { dcomp.Commit() },
            // (the window's process owns the visual tree; the chain's size goes with its present)
            Layers::Surface { .. } => Ok(()),
        }
    }

    /// Waits until the swap chain can take a frame (at most `timeout`); false on timeout.
    pub fn wait_for_frame(&self, timeout: Duration) -> bool {
        // SAFETY: the handle is the swap chain's waitable object, open until Drop.
        unsafe {
            WaitForSingleObjectEx(
                self.waitable,
                timeout.as_millis().min(u32::MAX as u128) as u32,
                true,
            ) == WAIT_OBJECT_0
        }
    }

    /// Draws the chrome layer: `list` (in DIPs, scaled by `scale`) over `background`. In one chain
    /// ([`Compositor::chrome_in_frame`]) it shows with the next present.
    pub fn draw_chrome(
        &mut self,
        list: &DisplayList,
        sources: Sources,
        scale: f32,
        background: Color,
    ) -> Result<()> {
        list.validate().map_err(|e| {
            windows::core::Error::new(windows::Win32::Foundation::E_INVALIDARG, e.to_string())
        })?;
        let (w, h) = self.chrome_size;
        let (dcomp, surface) = match &mut self.layers {
            Layers::Tree { dcomp, surface, .. } => (dcomp.clone(), surface.clone()),
            Layers::Surface { chrome, .. } => {
                if w == 0 || h == 0 {
                    *chrome = None;
                    return Ok(());
                }
                // SAFETY: a target bitmap of the context's device, drawn between BeginDraw and
                // EndDraw on this thread; the target is reset before returning.
                unsafe {
                    if chrome.is_none() {
                        *chrome = Some(self.ctx.CreateBitmap(
                            D2D_SIZE_U {
                                width: w,
                                height: h,
                            },
                            None,
                            0,
                            &bitmap_props(D2D1_BITMAP_OPTIONS_TARGET),
                        )?);
                    }
                    self.res.trim(sources.images);
                    self.ctx.SetTarget(chrome.as_ref().unwrap());
                    self.ctx.BeginDraw();
                    self.ctx.SetTransform(&Matrix3x2::identity());
                    self.ctx.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
                    self.ctx.Clear(Some(&color_f(background)));
                    {
                        let base = Matrix::scale(scale, scale);
                        let mut painter =
                            Painter::new(&self.ctx, &mut self.res, sources, base, true);
                        painter.paint(list, None);
                    }
                    self.ctx.SetTransform(&Matrix3x2::identity());
                    let r = self.ctx.EndDraw(None, None);
                    self.ctx.SetTarget(None);
                    return r;
                }
            }
        };
        let Some(surface) = surface else {
            // No chrome (a fullscreen window): nothing to draw, but the caller counts on this
            // commit for the visual tree's changes (a resize's content offset).
            // SAFETY: a commit of our own device's changes.
            unsafe { dcomp.Commit()? };
            return Ok(());
        };
        self.res.trim(sources.images);
        // SAFETY: the surface is drawn between its BeginDraw and EndDraw through a bitmap over the
        // DXGI surface it hands out, clipped to our part of it (the surface may be part of a larger
        // texture), and the context's target is reset before EndDraw.
        unsafe {
            let mut offset = POINT::default();
            let dxgi: IDXGISurface = surface.BeginDraw(None, &mut offset)?;
            let r = (|| -> Result<()> {
                let bitmap = self.ctx.CreateBitmapFromDxgiSurface(
                    &dxgi,
                    Some(&bitmap_props(
                        D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    )),
                )?;
                self.ctx.SetTarget(&bitmap);
                self.ctx.BeginDraw();
                self.ctx.SetTransform(&Matrix3x2::identity());
                let area = Rect::new(offset.x as f32, offset.y as f32, w as f32, h as f32);
                self.ctx
                    .PushAxisAlignedClip(&rect_f(&area), D2D1_ANTIALIAS_MODE_ALIASED);
                self.ctx.Clear(Some(&color_f(background)));
                {
                    let base = Matrix::scale(scale, scale)
                        .then(&Matrix::translation(offset.x as f32, offset.y as f32));
                    let mut painter = Painter::new(&self.ctx, &mut self.res, sources, base, true);
                    painter.paint(list, None);
                }
                self.ctx.SetTransform(&Matrix3x2::identity());
                self.ctx.PopAxisAlignedClip();
                let r = self.ctx.EndDraw(None, None);
                self.ctx.SetTarget(None);
                r
            })();
            let end = surface.EndDraw();
            r?;
            end?;
            dcomp.Commit()?;
        }
        Ok(())
    }

    /// Draws the next content frame with `paint` (into the back buffer; nothing shows until
    /// [`Compositor::present`]). In one chain the chrome is drawn above it.
    pub fn draw_content(&mut self, paint: impl FnOnce(&mut ContentPass<'_>)) -> Result<()> {
        let (origin, chrome) = match &self.layers {
            Layers::Tree { .. } => ((0, 0), None),
            Layers::Surface { chrome, .. } => ((0, self.chrome_size.1 as i32), Some(chrome)),
        };
        // SAFETY: the back buffer's bitmap is made once per buffer size; the context draws into it
        // between BeginDraw and EndDraw on this thread and lets go of it afterwards.
        unsafe {
            if self.back.is_none() {
                let surface: IDXGISurface = self.chain.GetBuffer(0)?;
                let mut props =
                    bitmap_props(D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW);
                if self.alpha {
                    props.pixelFormat.alphaMode = D2D1_ALPHA_MODE_PREMULTIPLIED;
                }
                self.back = Some(
                    self.ctx
                        .CreateBitmapFromDxgiSurface(&surface, Some(&props))?,
                );
            }
            self.ctx.SetTarget(self.back.as_ref().unwrap());
            self.ctx.BeginDraw();
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
            {
                let mut pass = ContentPass {
                    ctx: &self.ctx,
                    res: &mut self.res,
                    target: self.back.as_ref().unwrap(),
                    size: self.content_size,
                    origin,
                };
                paint(&mut pass);
            }
            if let Some(chrome) = chrome {
                let (w, h) = self.chrome_size;
                let area = rect_f(&Rect::new(0.0, 0.0, w as f32, h as f32));
                self.ctx.SetTransform(&Matrix3x2::identity());
                match chrome {
                    Some(bitmap) => self.ctx.DrawBitmap(
                        bitmap,
                        Some(&area),
                        1.0,
                        D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                        Some(&area),
                        None,
                    ),
                    None if h > 0 => {
                        // (no chrome drawn yet: the strip is cleared, never left undefined)
                        self.ctx
                            .PushAxisAlignedClip(&area, D2D1_ANTIALIAS_MODE_ALIASED);
                        self.ctx.Clear(Some(&color_f(Color::WHITE)));
                        self.ctx.PopAxisAlignedClip();
                    }
                    None => {}
                }
            }
            let r = self.ctx.EndDraw(None, None);
            self.ctx.SetTarget(None);
            r
        }
    }

    /// The frame drawn last (before [`Compositor::present`]): the whole buffer's size and its
    /// pixels, BGRA, rows packed. For tests and tools: it waits for the GPU.
    pub fn read_back(&mut self) -> Result<(u32, u32, Vec<u8>)> {
        let Some(back) = &self.back else {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "nothing drawn",
            ));
        };
        let (w, h) = self.chain_size;
        let mut props =
            bitmap_props(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW);
        if self.alpha {
            props.pixelFormat.alphaMode = D2D1_ALPHA_MODE_PREMULTIPLIED;
        }
        // SAFETY: a CPU-readable copy of our own back buffer, mapped and unmapped here.
        unsafe {
            let copy = self.ctx.CreateBitmap(
                D2D_SIZE_U {
                    width: w,
                    height: h,
                },
                None,
                0,
                &props,
            )?;
            copy.CopyFromBitmap(None, back, None)?;
            let map = copy.Map(D2D1_MAP_OPTIONS_READ)?;
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h as usize {
                let row = std::slice::from_raw_parts(
                    map.bits.add(y * map.pitch as usize),
                    w as usize * 4,
                );
                out.extend_from_slice(row);
            }
            copy.Unmap()?;
            Ok((w, h, out))
        }
    }

    /// Shows the frame drawn last. Returns how long `Present` took (it should not wait: the frame
    /// wait came first).
    ///
    /// Direct2D's device lock is *not* taken: with it, `Present` waited 3–7 ms behind tab threads
    /// rasterizing (measured on Wikipedia), which dropped frames. The device's immediate context is
    /// multithread-protected, so Direct3D serializes `Present` against Direct2D's own use of it, and
    /// a flip-model `Present` changes no pipeline state Direct2D relies on.
    pub fn present(&mut self) -> Result<Duration> {
        let t = Instant::now();
        // SAFETY: presenting our own swap chain on a multithread-protected device (see above); the
        // caller waited for the frame-latency object, so this does not block on the display.
        unsafe { self.chain.Present(1, DXGI_PRESENT(0)).ok()? };
        let elapsed = t.elapsed();
        if let Layers::Tree { dcomp, content, .. } = &self.layers {
            let pending = std::mem::take(&mut self.pending);
            // SAFETY: changes to our own visuals, committed after the chain they show has its
            // frame (a new chain shows nothing before its first present).
            unsafe {
                if pending.chain {
                    content.SetContent(&self.chain)?;
                }
                if pending.chain || pending.commit {
                    dcomp.Commit()?;
                }
            }
        }
        Ok(elapsed)
    }

    /// Whether the content can show protected players' pictures under it ([`ContentPass::hole`],
    /// [`Compositor::set_underlays`]): only with a visual tree of the compositor's own. The
    /// first time, the content chain is made again with premultiplied alpha (shown from the next
    /// present on), and stays so.
    pub fn allow_underlays(&mut self) -> bool {
        if !matches!(self.layers, Layers::Tree { .. }) {
            return false;
        }
        if self.alpha {
            return true;
        }
        let (w, h) = self.chain_size;
        // SAFETY: a new chain on the shared device; ours stays until it is in hand.
        let made = unsafe {
            (|| -> Result<(IDXGISwapChain2, HANDLE)> {
                let mut desc = chain_desc(w, h);
                desc.AlphaMode = DXGI_ALPHA_MODE_PREMULTIPLIED;
                let chain1 = self.gpu.dxgi_factory().CreateSwapChainForComposition(
                    self.gpu.d3d_device(),
                    &desc,
                    None,
                )?;
                let chain: IDXGISwapChain2 = chain1.cast()?;
                chain.SetMaximumFrameLatency(1)?;
                let waitable = chain.GetFrameLatencyWaitableObject();
                Ok((chain, waitable))
            })()
        };
        match made {
            Ok((chain, waitable)) => {
                // (no bitmap of the old chain's buffers is kept)
                self.back = None;
                self.chain = chain;
                // SAFETY: the old chain's waitable object, ours to close (as in Drop).
                unsafe {
                    if !self.waitable.is_invalid() {
                        let _ = CloseHandle(self.waitable);
                    }
                }
                self.waitable = waitable;
                self.alpha = true;
                self.pending.chain = true;
                true
            }
            Err(e) => {
                eprintln!("[gpu] no content chain with alpha for protected video: {e}");
                false
            }
        }
    }

    /// Shows `underlays` under the content in place of the ones before, from the next present
    /// on. Only after [`Compositor::allow_underlays`]; the content must have holes where they
    /// show ([`ContentPass::hole`]).
    pub fn set_underlays(&mut self, underlays: &[Underlay]) -> Result<()> {
        let Layers::Tree { dcomp, under, .. } = &mut self.layers else {
            return Ok(());
        };
        let had = !under.pictures.is_empty();
        let mut changed = false;
        // SAFETY: our own device and visuals; each surface is made from a handle that its `Arc`
        // keeps open while the visual shows it.
        unsafe {
            let mut gone: Vec<u64> = under.pictures.keys().copied().collect();
            gone.retain(|k| !underlays.iter().any(|u| u.key == *k));
            for k in gone {
                if let Some(v) = under.pictures.remove(&k) {
                    under.visual.RemoveVisual(&v.holder)?;
                    changed = true;
                }
            }
            for u in underlays {
                let stale = under
                    .pictures
                    .get(&u.key)
                    .is_some_and(|v| !Arc::ptr_eq(&v.surface, &u.surface));
                if stale {
                    if let Some(v) = under.pictures.remove(&u.key) {
                        under.visual.RemoveVisual(&v.holder)?;
                    }
                }
                let v = match under.pictures.entry(u.key) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let surface =
                            dcomp.CreateSurfaceFromHandle(HANDLE(u.surface.as_raw_handle()))?;
                        let holder = dcomp.CreateVisual()?;
                        let picture = dcomp.CreateVisual()?;
                        picture.SetContent(&surface)?;
                        holder.AddVisual(&picture, false, None)?;
                        under.visual.AddVisual(&holder, true, &under.background)?;
                        e.insert(UnderlayVisual {
                            holder,
                            picture,
                            surface: u.surface.clone(),
                            _content: surface,
                            placed: (Rect::new(0.0, 0.0, -1.0, -1.0), Rect::ZERO),
                        })
                    }
                };
                let at = (snap(&u.dest), snap(&u.clip));
                if v.placed != at {
                    v.picture.SetOffsetX2(at.0.x)?;
                    v.picture.SetOffsetY2(at.0.y)?;
                    v.holder.SetClip2(&D2D_RECT_F {
                        left: at.1.x,
                        top: at.1.y,
                        right: at.1.right(),
                        bottom: at.1.bottom(),
                    })?;
                    v.placed = at;
                    changed = true;
                }
            }
        }
        if had == under.pictures.is_empty() {
            changed = true;
            self.cover()?;
        }
        self.pending.commit |= changed;
        Ok(())
    }

    /// How many protected pictures show under the content.
    pub fn underlays(&self) -> usize {
        match &self.layers {
            Layers::Tree { under, .. } => under.pictures.len(),
            Layers::Surface { .. } => 0,
        }
    }

    /// The black background under the pictures, over the whole content area while there are
    /// any (a picture's engine may not cover its rectangle before its first frame).
    fn cover(&mut self) -> Result<()> {
        let (w, h) = self.content_size;
        let Layers::Tree { dcomp, under, .. } = &mut self.layers else {
            return Ok(());
        };
        // SAFETY: our own device, visuals and surface; the surface is drawn between its BeginDraw
        // and EndDraw through a bitmap over the DXGI surface it hands out, and the context's
        // target is reset before EndDraw.
        unsafe {
            if under.pictures.is_empty() {
                if under.black.take().is_some() {
                    under.background.SetContent(None)?;
                }
                return Ok(());
            }
            if under.black.is_none() {
                let black = dcomp.CreateSurface(
                    BLACK,
                    BLACK,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_IGNORE,
                )?;
                let mut offset = POINT::default();
                let dxgi: IDXGISurface = black.BeginDraw(None, &mut offset)?;
                let r = (|| -> Result<()> {
                    let bitmap = self.ctx.CreateBitmapFromDxgiSurface(
                        &dxgi,
                        Some(&bitmap_props(
                            D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                        )),
                    )?;
                    self.ctx.SetTarget(&bitmap);
                    self.ctx.BeginDraw();
                    self.ctx.SetTransform(&Matrix3x2::identity());
                    let area =
                        Rect::new(offset.x as f32, offset.y as f32, BLACK as f32, BLACK as f32);
                    self.ctx
                        .PushAxisAlignedClip(&rect_f(&area), D2D1_ANTIALIAS_MODE_ALIASED);
                    self.ctx.Clear(Some(&color_f(Color::BLACK)));
                    self.ctx.PopAxisAlignedClip();
                    let r = self.ctx.EndDraw(None, None);
                    self.ctx.SetTarget(None);
                    r
                })();
                let end = black.EndDraw();
                r?;
                end?;
                under.background.SetContent(&black)?;
                under.black = Some(black);
            }
            let scale = Matrix3x2 {
                M11: w as f32 / BLACK as f32,
                M22: h as f32 / BLACK as f32,
                ..Matrix3x2::identity()
            };
            under.background.SetTransform2(&scale)?;
        }
        self.pending.commit = true;
        Ok(())
    }
}

/// `r` with its edges on whole pixels.
fn snap(r: &Rect) -> Rect {
    let (x, y) = (r.x.round(), r.y.round());
    Rect::new(
        x,
        y,
        (r.right().round() - x).max(0.0),
        (r.bottom().round() - y).max(0.0),
    )
}

impl Drop for Compositor {
    fn drop(&mut self) {
        // SAFETY: the waitable object belongs to us (GetFrameLatencyWaitableObject hands out a
        // handle the caller must close).
        unsafe {
            if !self.waitable.is_invalid() {
                let _ = CloseHandle(self.waitable);
            }
        }
    }
}

impl ContentPass<'_> {
    /// The target's size in device pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn clear(&mut self, color: Color) {
        let area = self.at(Rect::new(0.0, 0.0, self.size.0 as f32, self.size.1 as f32));
        // SAFETY: drawing on a context between BeginDraw and EndDraw; the clip is popped.
        unsafe {
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx
                .PushAxisAlignedClip(&rect_f(&area), D2D1_ANTIALIAS_MODE_ALIASED);
            self.ctx.Clear(Some(&color_f(color)));
            self.ctx.PopAxisAlignedClip();
        }
    }

    /// Clears `rect` (device pixels of the content area, to whole pixels) to transparent, where a
    /// protected player's picture under the content shows ([`Compositor::set_underlays`]): only
    /// in a pass of a compositor that [`Compositor::allow_underlays`].
    pub fn hole(&mut self, rect: Rect) {
        let area = self.at(snap(&rect));
        // SAFETY: drawing on a context between BeginDraw and EndDraw; the clip is popped.
        unsafe {
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx
                .PushAxisAlignedClip(&rect_f(&area), D2D1_ANTIALIAS_MODE_ALIASED);
            self.ctx.Clear(Some(&color_f(Color::TRANSPARENT)));
            self.ctx.PopAxisAlignedClip();
        }
    }

    /// `r` (device pixels of the content area) in the buffer's.
    fn at(&self, r: Rect) -> Rect {
        Rect::new(
            r.x + self.origin.0 as f32,
            r.y + self.origin.1 as f32,
            r.width,
            r.height,
        )
    }

    /// Draws a frame's tiles with its top-left corner at `(dx, dy)` device pixels, within `clip`
    /// (device pixels of the target). Tiles are copied pixel for pixel (no filtering).
    pub fn tiles(&mut self, frame: &TiledFrame, dx: i32, dy: i32, clip: Rect) {
        let (dx, dy, clip) = (dx + self.origin.0, dy + self.origin.1, self.at(clip));
        // SAFETY: drawing bitmaps of the shared device (handed over by the rasterizer after
        // EndDraw) on a context between BeginDraw and EndDraw.
        unsafe {
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx
                .PushAxisAlignedClip(&rect_f(&clip), D2D1_ANTIALIAS_MODE_ALIASED);
            for t in &frame.tiles {
                let w = TILE_SIZE.min(frame.width.saturating_sub(t.x));
                let h = TILE_SIZE.min(frame.height.saturating_sub(t.y));
                if w == 0 || h == 0 {
                    continue;
                }
                let dest = Rect::new(
                    (t.x as i32 + dx) as f32,
                    (t.y as i32 + dy) as f32,
                    w as f32,
                    h as f32,
                );
                if !dest.intersects(&clip) {
                    continue;
                }
                let src = Rect::new(0.0, 0.0, w as f32, h as f32);
                self.ctx.DrawBitmap(
                    &t.bitmap,
                    Some(&rect_f(&dest)),
                    1.0,
                    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    Some(&rect_f(&src)),
                    None,
                );
            }
            self.ctx.PopAxisAlignedClip();
        }
    }

    /// Draws a piece of a split list ([`crate::DisplayList::split_layers`]) through its state: the
    /// piece's tiles, rastered at `scale` device pixels per CSS px in its own coordinates with their
    /// pixel (0, 0) at its device pixel `at`, are drawn under the state's clips, transforms and
    /// opacities, its list's device pixel (0, 0) at `base` (device pixels of the target), within
    /// `clip`. A state that comes to a whole-pixel translation draws the tiles pixel for pixel, as
    /// [`ContentPass::tiles`] does (a fraction is rounded); any other is filtered linearly, as
    /// Chromium's compositor draws a transformed layer.
    pub fn tiles_through(
        &mut self,
        frame: &TiledFrame,
        at: (i32, i32),
        state: &LayerState,
        scale: f32,
        base: (i32, i32),
        clip: Rect,
    ) {
        let base = (base.0 + self.origin.0, base.1 + self.origin.1);
        let clip = self.at(clip);
        // SAFETY: the pass's context is between BeginDraw and EndDraw on this thread.
        unsafe { draw_through(self.ctx, self.res, frame, at, state, scale, base, clip) }
    }

    /// Applies a backdrop filter (Filter Effects 2 §3) to what the pass has drawn so far: the
    /// pixels under the filter's box, whose list's device pixel (0, 0) is at `at` (device pixels
    /// of the target) with `scale` device pixels per CSS px, go through its functions (cut to the
    /// box, edges mirrored) and are drawn back clipped to the box's rounded shape, at its opacity,
    /// within `clip`. What the box's own element paints is drawn after, over it.
    pub fn backdrop(
        &mut self,
        backdrop: &crate::display_list::LayerBackdrop,
        scale: f32,
        at: (i32, i32),
        clip: Rect,
    ) {
        let at = (at.0 + self.origin.0, at.1 + self.origin.1);
        let clip = self.at(clip);
        // SAFETY: the pass's context is between BeginDraw and EndDraw on this thread, and
        // `target` is the bitmap it draws into.
        let r =
            unsafe { draw_backdrop(self.ctx, self.res, self.target, backdrop, scale, at, clip) };
        if let Err(e) = r {
            eprintln!("[gpu] backdrop filter: {e}");
        }
    }

    /// Draws a video's front frame (see [`crate::video`]) into `dest`, within `clip` (device pixels
    /// of the target), filtered linearly. Draws nothing before its first frame. Returns whether a
    /// frame was drawn.
    pub fn video(&mut self, slot: &crate::video::VideoSlot, dest: Rect, clip: Rect) -> bool {
        match slot.front() {
            Some(front) => self.video_frame(&front, dest, clip, false),
            None => false,
        }
    }

    /// Draws a video frame (a bitmap of the shared device and its size, from
    /// [`crate::video::VideoStore::front_of`]) into `dest`, within `clip`, filtered linearly.
    /// With `flip_rows`, the bitmap's rows are bottom to top (a canvas's frame as OpenGL stores
    /// it). Returns whether it drew.
    pub fn video_frame(
        &mut self,
        front: &(ID2D1Bitmap1, u32, u32),
        dest: Rect,
        clip: Rect,
        flip_rows: bool,
    ) -> bool {
        let (bitmap, w, h) = (&front.0, front.1, front.2);
        let (dest, clip) = (self.at(dest), self.at(clip));
        let clip = clip.intersect(&dest);
        if clip.is_empty() {
            return false;
        }
        // SAFETY: drawing a bitmap of the shared device on a context between BeginDraw and EndDraw;
        // the media engine writes other textures of the slot (see `video`), never the front one,
        // and a renderer's video is drawn with its key held (`VideoStore::latch`).
        unsafe {
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx
                .PushAxisAlignedClip(&rect_f(&clip), D2D1_ANTIALIAS_MODE_ALIASED);
            if flip_rows {
                // (y → top + bottom - y: the destination itself, upside down)
                self.ctx.SetTransform(&Matrix3x2 {
                    M11: 1.0,
                    M12: 0.0,
                    M21: 0.0,
                    M22: -1.0,
                    M31: 0.0,
                    M32: 2.0 * dest.y + dest.height,
                });
            }
            self.ctx.DrawBitmap(
                bitmap,
                Some(&rect_f(&dest)),
                1.0,
                D2D1_INTERPOLATION_MODE_LINEAR,
                Some(&rect_f(&Rect::new(0.0, 0.0, w as f32, h as f32))),
                None,
            );
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx.PopAxisAlignedClip();
        }
        true
    }

    /// Draws a display list (CSS px scaled by `scale`, then moved by `(dx, dy)` device pixels).
    pub fn list(&mut self, list: &DisplayList, sources: Sources, scale: f32, dx: f32, dy: f32) {
        if list.validate().is_err() {
            return;
        }
        self.res.trim(sources.images);
        let (dx, dy) = (dx + self.origin.0 as f32, dy + self.origin.1 as f32);
        let base = Matrix::scale(scale, scale).then(&Matrix::translation(dx, dy));
        let mut painter = Painter::new(self.ctx, self.res, sources, base, true);
        painter.paint(list, None);
        // SAFETY: resetting the transform on a live context.
        unsafe { self.ctx.SetTransform(&Matrix3x2::identity()) };
    }
}

/// [`ContentPass::backdrop`] on a context drawing into `target`.
// SAFETY contract: `ctx` is between BeginDraw and EndDraw on this thread with `target` as its
// target, and `res` made for it.
unsafe fn draw_backdrop(
    ctx: &ID2D1DeviceContext,
    res: &Resources,
    target: &ID2D1Bitmap1,
    backdrop: &crate::display_list::LayerBackdrop,
    scale: f32,
    at: (i32, i32),
    clip: Rect,
) -> Result<()> {
    let s = if scale > 0.0 && scale.is_finite() {
        scale
    } else {
        1.0
    };
    if backdrop.opacity <= 0.0 || backdrop.filters.is_empty() {
        return Ok(());
    }
    let to_device = Matrix::scale(s, s).then(&Matrix::translation(at.0 as f32, at.1 as f32));
    let device = to_device.apply_rect(&backdrop.rect);
    // SAFETY: reading the size of a live bitmap.
    let size = unsafe { target.GetPixelSize() };
    // (the box in whole device pixels, where it shows)
    let crop = Rect::from_ltrb(
        device.x.round(),
        device.y.round(),
        device.right().round(),
        device.bottom().round(),
    )
    .intersect(&clip)
    .intersect(&Rect::new(0.0, 0.0, size.width as f32, size.height as f32));
    if crop.is_empty() || crop.width < 1.0 || crop.height < 1.0 {
        return Ok(());
    }
    let (x, y) = (crop.x as u32, crop.y as u32);
    let (w, h) = (crop.width as u32, crop.height as u32);
    // SAFETY: bitmaps and effects of the context's own device, on its thread, inside its drawing
    // (the caller's contract). What was drawn is flushed to the target before its pixels are
    // copied, the target is set again before anything else is drawn, every push is popped, and
    // Direct2D gets pointers to locals that outlive each call.
    unsafe {
        let under = ctx.CreateBitmap(
            D2D_SIZE_U {
                width: w,
                height: h,
            },
            None,
            0,
            &bitmap_props(D2D1_BITMAP_OPTIONS_NONE),
        )?;
        ctx.Flush(None, None)?;
        // (a bitmap is not read while it is the context's target)
        ctx.SetTarget(None);
        let copied = under.CopyFromBitmap(
            None,
            target,
            Some(&D2D_RECT_U {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            }),
        );
        ctx.SetTarget(target);
        copied?;
        let local = Rect::new(0.0, 0.0, w as f32, h as f32);
        let mut no_images = |_| None;
        let out = crate::effects::backdrop(
            ctx,
            &backdrop.filters,
            &under.cast()?,
            Color::TRANSPARENT,
            &local,
            &Matrix::scale(s, s),
            &mut no_images,
        )?;
        let geometry = clip_geometry(res, &backdrop.rect, &backdrop.radii)?;
        ctx.SetTransform(&Matrix3x2::identity());
        ctx.PushAxisAlignedClip(&rect_f(&clip), D2D1_ANTIALIAS_MODE_ALIASED);
        ctx.SetTransform(&matrix_3x2(&to_device));
        let params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: rect_f(&Rect::infinite()),
            // SAFETY: a borrowed copy of the pointer without AddRef; ManuallyDrop never releases
            // it, and `geometry` outlives the PushLayer call.
            geometricMask: std::mem::transmute_copy(&geometry),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity: backdrop.opacity.clamp(0.0, 1.0),
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        ctx.PushLayer(&params, None);
        ctx.SetTransform(&Matrix3x2::identity());
        ctx.DrawImage(
            &out,
            Some(&windows_numerics::Vector2 {
                X: crop.x,
                Y: crop.y,
            }),
            None,
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            D2D1_COMPOSITE_MODE_SOURCE_OVER,
        );
        ctx.PopLayer();
        ctx.PopAxisAlignedClip();
    }
    Ok(())
}

/// [`ContentPass::tiles_through`] on any context.
// SAFETY contract: `ctx` is between BeginDraw and EndDraw on this thread, with `res` made for it.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn draw_through(
    ctx: &ID2D1DeviceContext,
    res: &Resources,
    frame: &TiledFrame,
    at: (i32, i32),
    state: &LayerState,
    scale: f32,
    base: (i32, i32),
    clip: Rect,
) {
    let s = if scale > 0.0 && scale.is_finite() {
        scale
    } else {
        1.0
    };
    // From CSS px of the state's current coordinates to the target's device pixels.
    let device = |m: &Matrix| {
        Matrix::scale(1.0 / s, 1.0 / s)
            .then(m)
            .then(&Matrix::scale(s, s))
            .then(&Matrix::translation(base.0 as f32, base.1 as f32))
    };
    let css_device = |m: &Matrix| {
        m.then(&Matrix::scale(s, s))
            .then(&Matrix::translation(base.0 as f32, base.1 as f32))
    };
    // SAFETY: drawing bitmaps of the shared device (handed over by the rasterizer after EndDraw) on
    // a context between BeginDraw and EndDraw (the caller's contract); every push below is popped
    // before returning, and Direct2D gets pointers to locals that outlive each call.
    unsafe {
        ctx.SetTransform(&Matrix3x2::identity());
        ctx.PushAxisAlignedClip(&rect_f(&clip), D2D1_ANTIALIAS_MODE_ALIASED);
        let mut opened: Vec<bool> = Vec::new();
        let mut current = Matrix::IDENTITY;
        let mut opacity = 1.0f32;
        // (the filter steps' functions, innermost step first: each applies to each pixel on its
        // own, so they apply to the tiles wherever the transforms put them)
        let mut filters: Vec<crate::filter::FilterOp> = Vec::new();
        for op in &state.0 {
            match op {
                StateOp::Transform(t) => current = matrix_of(t).then(&current),
                StateOp::Opacity(o) => opacity *= o,
                StateOp::Filter(f) => {
                    filters.splice(0..0, f.ops());
                }
                StateOp::Animated(_) => {}
                StateOp::Clip(r) | StateOp::ClipRounded(r, _) => {
                    let radii = match op {
                        StateOp::ClipRounded(_, radii) => radii_of(radii),
                        _ => Radii::ZERO,
                    };
                    let m = css_device(&current);
                    if radii.is_zero() && m.is_axis_aligned() {
                        // (snapped to device pixels, as the painter snaps a clip)
                        let d = m.apply_rect(r);
                        let snapped = Rect::from_ltrb(
                            d.x.round(),
                            d.y.round(),
                            d.right().round(),
                            d.bottom().round(),
                        );
                        ctx.SetTransform(&Matrix3x2::identity());
                        ctx.PushAxisAlignedClip(&rect_f(&snapped), D2D1_ANTIALIAS_MODE_ALIASED);
                        opened.push(false);
                        continue;
                    }
                    let Ok(geometry) = clip_geometry(res, r, &radii) else {
                        continue;
                    };
                    ctx.SetTransform(&matrix_3x2(&m));
                    let params = D2D1_LAYER_PARAMETERS1 {
                        contentBounds: rect_f(&Rect::infinite()),
                        // SAFETY: a borrowed copy of the pointer without AddRef; ManuallyDrop
                        // never releases it, and `geometry` outlives the PushLayer call.
                        geometricMask: std::mem::transmute_copy(&geometry),
                        maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                        maskTransform: Matrix3x2::identity(),
                        opacity: 1.0,
                        opacityBrush: ManuallyDrop::new(None),
                        layerOptions: D2D1_LAYER_OPTIONS1_NONE,
                    };
                    ctx.PushLayer(&params, None);
                    opened.push(true);
                }
            }
        }
        if !filters.is_empty() && opacity < 1.0 {
            // (the opacities after the filters: on one piece's pixels, as the `opacity()` function)
            filters.push(crate::filter::FilterOp::Opacity(opacity));
        }
        let m = device(&current);
        let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
        let whole = near(m.m11, 1.0) && near(m.m12, 0.0) && near(m.m21, 0.0) && near(m.m22, 1.0);
        let (m, mode) = if whole {
            (
                Matrix::translation(m.dx.round(), m.dy.round()),
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            )
        } else {
            (m, D2D1_INTERPOLATION_MODE_LINEAR)
        };
        ctx.SetTransform(&matrix_3x2(&m));
        if opacity > 0.0 {
            for t in &frame.tiles {
                let w = TILE_SIZE.min(frame.width.saturating_sub(t.x));
                let h = TILE_SIZE.min(frame.height.saturating_sub(t.y));
                if w == 0 || h == 0 {
                    continue;
                }
                let dest = Rect::new(
                    (t.x as i32 + at.0) as f32,
                    (t.y as i32 + at.1) as f32,
                    w as f32,
                    h as f32,
                );
                if !m.apply_rect(&dest).intersects(&clip) {
                    continue;
                }
                let src = Rect::new(0.0, 0.0, w as f32, h as f32);
                if !filters.is_empty() {
                    if let Ok(image) = filtered_tile(ctx, &t.bitmap, &filters) {
                        ctx.DrawImage(
                            &image,
                            Some(&windows_numerics::Vector2 {
                                X: dest.x,
                                Y: dest.y,
                            }),
                            Some(&rect_f(&src)),
                            mode,
                            D2D1_COMPOSITE_MODE_SOURCE_OVER,
                        );
                    }
                    continue;
                }
                ctx.DrawBitmap(
                    &t.bitmap,
                    Some(&rect_f(&dest)),
                    opacity,
                    mode,
                    Some(&rect_f(&src)),
                    None,
                );
            }
        }
        for layer in opened.into_iter().rev() {
            if layer {
                ctx.PopLayer();
            } else {
                ctx.PopAxisAlignedClip();
            }
        }
        ctx.SetTransform(&Matrix3x2::identity());
        ctx.PopAxisAlignedClip();
    }
}

/// A tile's bitmap through the color functions `ops` (each pixel on its own), its edges extended
/// beyond the tile as a bitmap drawn alone is sampled (linear filtering at its edges).
// SAFETY contract: `ctx` is between BeginDraw and EndDraw on this thread, `bitmap` of its device.
unsafe fn filtered_tile(
    ctx: &ID2D1DeviceContext,
    bitmap: &ID2D1Bitmap1,
    ops: &[crate::filter::FilterOp],
) -> Result<ID2D1Image> {
    // SAFETY: effects of the context's own device, on its thread, inside its drawing (the
    // caller's contract).
    unsafe {
        let border = ctx.CreateEffect(&CLSID_D2D1Border)?;
        border.SetInput(0, &bitmap.cast::<ID2D1Image>()?, true);
        for prop in [D2D1_BORDER_PROP_EDGE_MODE_X, D2D1_BORDER_PROP_EDGE_MODE_Y] {
            border.SetValue(
                prop.0 as u32,
                D2D1_PROPERTY_TYPE_ENUM,
                &(D2D1_BORDER_EDGE_MODE_CLAMP.0 as u32).to_le_bytes(),
            )?;
        }
        // (the compositor applies color functions only: no image is drawn)
        crate::effects::apply(
            ctx,
            ops,
            &border.GetOutput()?,
            &Matrix::IDENTITY,
            &mut |_| None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_list::DisplayListBuilder;
    use crate::raster_tables::{FontTable, ImageTable};
    use crate::window::HiddenWindow;

    /// The pixel at (x, y) of a BGRA buffer `w` wide, as RGB.
    fn pixel(frame: &(u32, u32, Vec<u8>), x: u32, y: u32) -> (u8, u8, u8) {
        let i = ((y * frame.0 + x) * 4) as usize;
        (frame.2[i + 2], frame.2[i + 1], frame.2[i])
    }

    #[test]
    fn one_chain_holds_the_chrome_above_the_content_at_every_size() {
        let gpu = Gpu::new().unwrap();
        let fonts = FontTable::new().unwrap();
        let images = ImageTable::new();
        let window = HiddenWindow::new().unwrap();
        let surface = WindowSurface::new(window.handle()).unwrap();
        let mut c = Compositor::on_surface(
            gpu,
            &fonts,
            window.handle(),
            surface.handle().unwrap(),
            200,
            150,
            40,
        )
        .unwrap();
        assert!(c.chrome_in_frame());
        assert_eq!(c.content_size(), (200, 110));
        let sources = || Sources {
            text: &fonts,
            images: &images,
        };
        let red = Color::rgb(255, 0, 0);
        let blue = Color::rgb(0, 0, 255);
        let green = Color::rgb(0, 255, 0);
        let mut marker = DisplayListBuilder::new();
        marker.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), green);
        let marker = marker.build();
        let empty = DisplayListBuilder::new().build();
        c.draw_chrome(&empty, sources(), 1.0, red).unwrap();
        c.draw_content(|p| {
            assert_eq!(p.size(), (200, 110));
            p.clear(blue);
            p.list(&marker, sources(), 1.0, 0.0, 0.0);
        })
        .unwrap();
        let f = c.read_back().unwrap();
        assert_eq!((f.0, f.1), (200, 150));
        assert_eq!(pixel(&f, 100, 39), (255, 0, 0), "the chrome's last row");
        assert_eq!(
            pixel(&f, 5, 45),
            (0, 255, 0),
            "content (0, 0) below the chrome"
        );
        assert_eq!(pixel(&f, 100, 40), (0, 0, 255), "the content's first row");
        assert_eq!(pixel(&f, 199, 149), (0, 0, 255));
        c.present().unwrap();

        // A resize: the chain and the chrome take the new sizes in the same frame.
        c.resize(300, 200, 60).unwrap();
        c.draw_chrome(&empty, sources(), 1.0, green).unwrap();
        c.draw_content(|p| p.clear(blue)).unwrap();
        let f = c.read_back().unwrap();
        assert_eq!((f.0, f.1), (300, 200));
        assert_eq!(pixel(&f, 299, 59), (0, 255, 0));
        assert_eq!(pixel(&f, 299, 60), (0, 0, 255));
        assert_eq!(c.content_size(), (300, 140));
        c.present().unwrap();
        drop(c);
        surface.clear().unwrap();
    }

    /// G5 step 2: a renewed surface takes a chain of its own while the one before still has its
    /// chain, and the window's handle is then the new surface's.
    #[test]
    fn a_renewed_surface_takes_a_chain_of_its_own() {
        let gpu = Gpu::new().unwrap();
        let fonts = FontTable::new().unwrap();
        let window = HiddenWindow::new().unwrap();
        let surface = WindowSurface::new(window.handle()).unwrap();
        let on = |handle| {
            Compositor::on_surface(gpu.clone(), &fonts, window.handle(), handle, 300, 200, 40)
        };
        let mut first = on(surface.handle().unwrap()).unwrap();
        first
            .draw_content(|p| p.clear(Color::rgb(0, 0, 255)))
            .unwrap();
        first.present().unwrap();
        assert!(
            on(surface.handle().unwrap()).is_err(),
            "one chain per surface"
        );
        let renewed = surface.renew().unwrap();
        let mut second = on(renewed).unwrap();
        second
            .draw_content(|p| p.clear(Color::rgb(0, 255, 0)))
            .unwrap();
        second.present().unwrap();
        // (the old surface is free again once its chain is released in this process)
        drop(first);
        assert!(
            on(surface.handle().unwrap()).is_err(),
            "the handle is the new surface's"
        );
        drop(second);
        surface.clear().unwrap();
    }

    #[test]
    fn a_tree_of_its_own_draws_content_from_the_buffers_top() {
        let gpu = Gpu::new().unwrap();
        let fonts = FontTable::new().unwrap();
        let images = ImageTable::new();
        let window = HiddenWindow::new().unwrap();
        let mut c = Compositor::new(gpu, &fonts, window.handle(), 200, 150, 40).unwrap();
        assert!(!c.chrome_in_frame());
        let mut marker = DisplayListBuilder::new();
        marker.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::rgb(0, 255, 0));
        let marker = marker.build();
        let sources = Sources {
            text: &fonts,
            images: &images,
        };
        c.draw_content(|p| {
            p.clear(Color::rgb(0, 0, 255));
            p.list(&marker, sources, 1.0, 0.0, 0.0);
        })
        .unwrap();
        let f = c.read_back().unwrap();
        assert_eq!((f.0, f.1), (200, 110));
        assert_eq!(pixel(&f, 5, 5), (0, 255, 0));
        assert_eq!(pixel(&f, 5, 45), (0, 0, 255));
    }

    /// A protected player's picture shows under the content: the chain takes alpha once, the
    /// content is transparent in the hole and opaque elsewhere (what is drawn after the hole
    /// covers it), and the picture's visual comes and goes with the list it is in.
    #[test]
    fn a_protected_picture_shows_under_a_hole_in_the_content() {
        let gpu = Gpu::new().unwrap();
        let fonts = FontTable::new().unwrap();
        let images = ImageTable::new();
        let window = HiddenWindow::new().unwrap();
        let mut c = Compositor::new(gpu, &fonts, window.handle(), 200, 150, 40).unwrap();
        assert!(c.allow_underlays());
        assert!(c.allow_underlays());
        let mut over = DisplayListBuilder::new();
        over.fill_rect(Rect::new(20.0, 50.0, 10.0, 10.0), Color::rgb(0, 255, 0));
        let over = over.build();
        let sources = Sources {
            text: &fonts,
            images: &images,
        };
        c.draw_content(|p| {
            p.clear(Color::rgb(0, 0, 255));
            p.hole(Rect::new(10.2, 20.0, 99.6, 60.0));
            p.list(&over, sources, 1.0, 0.0, 0.0);
        })
        .unwrap();
        let f = c.read_back().unwrap();
        let alpha = |x: u32, y: u32| f.2[((y * f.0 + x) * 4 + 3) as usize];
        assert_eq!((alpha(5, 5), alpha(15, 25), alpha(25, 55)), (255, 0, 255));
        assert_eq!(
            (alpha(9, 25), alpha(10, 25), alpha(109, 79), alpha(110, 79)),
            (255, 0, 0, 255)
        );
        // SAFETY: test code: a composition surface handle of our own, owned by `OwnedHandle`.
        let surface = unsafe {
            let raw = DCompositionCreateSurfaceHandle(COMPOSITIONOBJECT_ALL_ACCESS, None).unwrap();
            Arc::new(OwnedHandle::from_raw_handle(raw.0))
        };
        let picture = |key, surface: &Arc<OwnedHandle>| Underlay {
            key,
            surface: surface.clone(),
            dest: Rect::new(10.0, 20.0, 100.0, 60.0),
            clip: Rect::new(10.0, 20.0, 100.0, 60.0),
        };
        c.set_underlays(&[picture(7, &surface)]).unwrap();
        c.present().unwrap();
        assert_eq!(c.underlays(), 1);
        c.set_underlays(&[picture(7, &surface), picture(8, &surface)])
            .unwrap();
        assert_eq!(c.underlays(), 2);
        c.set_underlays(&[]).unwrap();
        c.present().unwrap();
        assert_eq!(c.underlays(), 0);
    }

    /// A backdrop filter of a layer the compositor places (a fixed bar) is applied to
    /// what the pass has drawn under it, where the layer is now: inside the filter's box the
    /// pixels drawn so far go through the functions, at the element's opacity; outside it they
    /// stay.
    #[test]
    fn a_layers_backdrop_filter_applies_to_what_is_drawn_under_it() {
        use crate::display_list::LayerBackdrop;
        use crate::filter::FilterOp;
        let gpu = Gpu::new().unwrap();
        let fonts = FontTable::new().unwrap();
        let window = HiddenWindow::new().unwrap();
        let surface = WindowSurface::new(window.handle()).unwrap();
        let mut c = Compositor::on_surface(
            gpu,
            &fonts,
            window.handle(),
            surface.handle().unwrap(),
            200,
            150,
            40,
        )
        .unwrap();
        let blue = Color::rgb(0, 0, 255);
        let clip = Rect::new(0.0, 0.0, 200.0, 110.0);
        let mut backdrop = LayerBackdrop {
            filters: vec![FilterOp::Invert(1.0)],
            rect: Rect::new(10.0, 5.0, 50.0, 20.0),
            radii: Radii::ZERO,
            opacity: 1.0,
        };
        // The layer's list is at (20, 10) of the content area, at 2 device pixels per CSS px:
        // the box is at 40..140 by 20..60 there, under the 40 rows of the chrome.
        c.draw_content(|p| {
            p.clear(blue);
            p.backdrop(&backdrop, 2.0, (20, 10), clip);
        })
        .unwrap();
        let f = c.read_back().unwrap();
        assert_eq!(pixel(&f, 90, 40 + 40), (255, 255, 0), "inside the box");
        assert_eq!(pixel(&f, 41, 40 + 21), (255, 255, 0), "its first pixel");
        assert_eq!(pixel(&f, 139, 40 + 59), (255, 255, 0), "its last pixel");
        assert_eq!(pixel(&f, 39, 40 + 40), (0, 0, 255), "left of it");
        assert_eq!(pixel(&f, 141, 40 + 40), (0, 0, 255), "right of it");
        assert_eq!(pixel(&f, 90, 40 + 19), (0, 0, 255), "above it");
        assert_eq!(pixel(&f, 90, 40 + 61), (0, 0, 255), "below it");
        c.present().unwrap();
        // At half the element's opacity, half of the filtered backdrop shows.
        backdrop.opacity = 0.5;
        c.draw_content(|p| {
            p.clear(blue);
            p.backdrop(&backdrop, 2.0, (20, 10), clip);
        })
        .unwrap();
        let f = c.read_back().unwrap();
        let (r, g, b) = pixel(&f, 90, 40 + 40);
        assert!(
            (126..=129).contains(&r) && (126..=129).contains(&g) && (126..=129).contains(&b),
            "{r} {g} {b}"
        );
        // A box cut by the clip is filtered where it shows.
        backdrop.opacity = 1.0;
        c.draw_content(|p| {
            p.clear(blue);
            p.backdrop(&backdrop, 2.0, (20, 10), Rect::new(0.0, 0.0, 100.0, 110.0));
        })
        .unwrap();
        let f = c.read_back().unwrap();
        assert_eq!(pixel(&f, 99, 40 + 40), (255, 255, 0));
        assert_eq!(pixel(&f, 101, 40 + 40), (0, 0, 255));
        drop(c);
        surface.clear().unwrap();
    }
}
