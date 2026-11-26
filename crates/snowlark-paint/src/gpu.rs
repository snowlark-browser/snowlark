// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The GPU path: one shared Direct3D 11 device with Direct2D on top ([`Gpu`]), and per-thread tile
//! rasterizers ([`TileRasterizer`]) that paint display lists into 512×512 GPU tiles, which a
//! compositor can draw (or sample as textures) at any scroll offset without repainting.
//!
//! Thread rules:
//! - [`Gpu`] is `Send + Sync` and shared (`Arc`): the D3D11 device is free-threaded, the Direct2D
//!   factory is created multithreaded, and the D3D11 immediate context is multithread-protected so
//!   Direct2D's use of it from several threads is serialized.
//! - [`TileRasterizer`] is thread-affine (not `Send`): it owns an `ID2D1DeviceContext`, which may
//!   only be used by one thread. Each raster thread makes its own.
//! - [`TiledFrame`] is `Send`: its tile bitmaps are device resources of the shared device and may be
//!   drawn by any device context of it (a compositor thread's), but not while the rasterizer that
//!   made them is still writing to them (it hands the frame over when done).
//! - Code that uses the D3D11 immediate context directly (video frame copies) must hold
//!   [`Gpu::lock`] so it does not interleave with Direct2D's use. A flip-model swap chain's `Present`
//!   is the exception (see [`crate::compositor::Compositor::present`]).

use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::sync::Arc;

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{E_INVALIDARG, HMODULE};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows_numerics::Matrix3x2;

use crate::display_list::DisplayList;
use crate::display_list::ImageId;
use crate::geom::{Matrix, Rect};
use crate::image::Pixmap;
use crate::image::ShownImage;
use crate::raster::{
    self, color_f, matrix_3x2, FontResolver, Painter, Resources, Sources, Viewport,
};
use crate::tiling::{grid, tile_area, tile_hashes, FrameKey};
pub use crate::tiling::{RasterStats, RasterWork, TILE_SIZE};

/// Which kind of device to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    /// The graphics card.
    Hardware,
    /// WARP: Windows' software Direct3D rasterizer (no GPU needed).
    Warp,
}

/// The shared GPU device: see the module documentation for the thread rules.
pub struct Gpu {
    device: ID3D11Device,
    factory: ID2D1Factory1,
    d2d: ID2D1Device,
    dxgi: IDXGIFactory2,
    multithread: ID2D1Multithread,
    driver: Driver,
    adapter: String,
}

// SAFETY: the D3D11 device is free-threaded; the Direct2D factory is created with
// D2D1_FACTORY_TYPE_MULTI_THREADED, which makes it and the devices and resources made from it safe to
// use from several threads (Direct2D serializes internally, and the immediate context is
// multithread-protected); the DXGI factory is free-threaded.
unsafe impl Send for Gpu {}
// SAFETY: as above; `&Gpu` only exposes those free-threaded objects.
unsafe impl Sync for Gpu {}

/// Holds Direct2D's device lock (see [`Gpu::lock`]).
pub struct GpuLock<'a> {
    gpu: &'a Gpu,
}

impl Drop for GpuLock<'_> {
    fn drop(&mut self) {
        // SAFETY: balances the Enter in Gpu::lock on the same thread.
        unsafe { self.gpu.multithread.Leave() };
    }
}

impl Gpu {
    /// Submits what the immediate context holds. Direct3D destroys a released resource (a raster
    /// thread's tiles) only once the immediate context is flushed: a window's `Present` does so,
    /// but without one they stay, so whatever releases many calls this.
    pub fn flush(&self) {
        let _lock = self.lock();
        // SAFETY: the shared device's immediate context, used under Direct2D's lock (see the
        // module documentation).
        unsafe {
            if let Ok(context) = self.device.GetImmediateContext() {
                context.Flush();
            }
        }
    }

    /// The hardware device, or WARP when there is no usable graphics card.
    pub fn new() -> Result<Arc<Gpu>> {
        Gpu::with_driver(Driver::Hardware).or_else(|_| Gpu::with_driver(Driver::Warp))
    }

    pub fn with_driver(driver: Driver) -> Result<Arc<Gpu>> {
        let levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        let kind = match driver {
            Driver::Hardware => D3D_DRIVER_TYPE_HARDWARE,
            Driver::Warp => D3D_DRIVER_TYPE_WARP,
        };
        // SAFETY: device creation with valid arguments; every out pointer is a local. All interfaces
        // returned by getters (adapter, factory) are owned and released on drop, so none leak.
        unsafe {
            let mut device = None;
            D3D11CreateDevice(
                None,
                kind,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
            let device = device.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            // (returns the previous setting)
            let _ = device
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);
            let dxgi_device: IDXGIDevice1 = device.cast()?;
            // Don't let the CPU queue frames ahead of the screen.
            dxgi_device.SetMaximumFrameLatency(1)?;
            let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None)?;
            let d2d = factory.CreateDevice(&dxgi_device)?;
            let adapter = dxgi_device.GetAdapter()?;
            let desc = adapter.GetDesc()?;
            let end = desc.Description.iter().position(|c| *c == 0).unwrap_or(128);
            let dxgi: IDXGIFactory2 = adapter.GetParent()?;
            let multithread: ID2D1Multithread = factory.cast()?;
            Ok(Arc::new(Gpu {
                device,
                factory,
                d2d,
                dxgi,
                multithread,
                driver,
                adapter: String::from_utf16_lossy(&desc.Description[..end]),
            }))
        }
    }

    pub fn driver(&self) -> Driver {
        self.driver
    }

    /// The adapter's name ("NVIDIA GeForce RTX 4060", "Microsoft Basic Render Driver").
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    pub fn d3d_device(&self) -> &ID3D11Device {
        &self.device
    }

    /// The Direct2D device (for device contexts of other drawing threads, like the compositor's).
    pub(crate) fn d2d_device(&self) -> &ID2D1Device {
        &self.d2d
    }

    pub fn d2d_factory(&self) -> &ID2D1Factory1 {
        &self.factory
    }

    pub fn dxgi_factory(&self) -> &IDXGIFactory2 {
        &self.dxgi
    }

    /// Takes Direct2D's device lock, for direct use of the D3D11 immediate context.
    pub fn lock(&self) -> GpuLock<'_> {
        // SAFETY: Enter/Leave are balanced by GpuLock's Drop.
        unsafe { self.multithread.Enter() };
        GpuLock { gpu: self }
    }

    /// Whether the device was removed (driver update, GPU reset): then everything made from it must
    /// be made again from a new `Gpu`.
    pub fn is_lost(&self) -> bool {
        // SAFETY: no preconditions.
        unsafe { self.device.GetDeviceRemovedReason().is_err() }
    }
}

/// A tile of a frame: a D3D11 texture (bindable as a render target and shader resource, for a
/// compositor) with a Direct2D bitmap over its DXGI surface. 512×512, or for a frame that fits in
/// one tile, a smaller square that holds it (a power of two from 64: a small layer's tile, as
/// Chromium sizes a small layer's tiles to it).
pub struct Tile {
    /// The tile's top-left corner in the frame, in device pixels.
    pub x: u32,
    pub y: u32,
    pub texture: ID3D11Texture2D,
    pub bitmap: ID2D1Bitmap1,
    opaque: bool,
    /// The texture's width and height.
    size: u32,
}

/// A rasterized frame as tiles covering `width` × `height` device pixels.
pub struct TiledFrame {
    pub width: u32,
    pub height: u32,
    pub tiles: Vec<Tile>,
}

// SAFETY: the tiles are resources of a device made from a multithreaded Direct2D factory (see `Gpu`);
// any device context of that device may draw them from any thread. The rasterizer hands a frame over
// only after EndDraw, so no thread writes to it while another reads.
unsafe impl Send for TiledFrame {}

/// Paints display lists into tiles on one thread (see the module documentation).
pub struct TileRasterizer {
    gpu: Arc<Gpu>,
    ctx: ID2D1DeviceContext,
    res: Option<Resources>,
    pool: Vec<Tile>,
    /// The last frame's tiles with the hash of what each shows, for the full frame (slot 0) and for
    /// each piece rastered with [`TileRasterizer::raster_piece`] (by its slot): a tile of the next
    /// raster of the same slot and geometry whose items hash the same is copied from it instead of
    /// painted again.
    retained: Vec<Retained>,
    /// What the last full [`TileRasterizer::raster`] and the pieces after it did.
    stats: RasterStats,
    work: RasterWork,
    // (a device context belongs to one thread)
    _thread_affine: PhantomData<*const ()>,
}

struct RetainedTile {
    x: u32,
    y: u32,
    hash: u64,
    texture: ID3D11Texture2D,
    bitmap: ID2D1Bitmap1,
}

struct Retained {
    slot: u64,
    key: FrameKey,
    tiles: Vec<RetainedTile>,
    /// Whether a raster since the last full frame's used it (a piece not rastered for a frame is
    /// dropped at the next one).
    used: bool,
}

impl Drop for TileRasterizer {
    fn drop(&mut self) {
        // Its tiles are destroyed now, not at the window's next Present (`Gpu::flush`): a renderer
        // that ends, or crashes, takes its raster thread's memory with it.
        self.pool.clear();
        self.retained.clear();
        self.res = None;
        self.gpu.flush();
    }
}

impl TileRasterizer {
    pub fn new(gpu: Arc<Gpu>) -> Result<TileRasterizer> {
        // SAFETY: creating a device context from the shared device.
        let ctx = unsafe {
            gpu.d2d
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?
        };
        Ok(TileRasterizer {
            gpu,
            ctx,
            res: None,
            pool: Vec::new(),
            retained: Vec::new(),
            stats: RasterStats::default(),
            work: RasterWork::default(),
            _thread_affine: PhantomData,
        })
    }

    pub fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
    }

    fn tile(&mut self, x: u32, y: u32, opaque: bool, size: u32) -> Result<Tile> {
        if let Some(i) = self
            .pool
            .iter()
            .position(|t| t.opaque == opaque && t.size == size)
        {
            let mut t = self.pool.swap_remove(i);
            t.x = x;
            t.y = y;
            return Ok(t);
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size,
            Height: size,
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
        // SAFETY: texture creation with a valid description; the bitmap wraps the texture's surface
        // and keeps it alive.
        unsafe {
            let mut texture = None;
            self.gpu
                .device
                .CreateTexture2D(&desc, None, Some(&mut texture))?;
            let texture = texture.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            let surface: IDXGISurface = texture.cast()?;
            let bitmap = self.ctx.CreateBitmapFromDxgiSurface(
                &surface,
                Some(&D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        // Opaque tiles are 24-bit: ClearType text needs that.
                        alphaMode: if opaque {
                            D2D1_ALPHA_MODE_IGNORE
                        } else {
                            D2D1_ALPHA_MODE_PREMULTIPLIED
                        },
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
                    colorContext: ManuallyDrop::new(None),
                }),
            )?;
            Ok(Tile {
                x,
                y,
                texture,
                bitmap,
                opaque,
                size,
            })
        }
    }

    /// Paints `list` into the tiles covering the viewport. Items are culled per tile by their bounds.
    ///
    /// Damage: when the previous frame this rasterizer made had the same geometry (size, scale,
    /// background), a tile whose items hash the same as there is copied from that frame's tile
    /// instead of painted ([`TileRasterizer::stats`] says how many). The caller hands the previous
    /// frame's tiles back ([`TileRasterizer::recycle`]) only once it no longer shows them, as the
    /// compositor does when the next frame arrives.
    pub fn raster(
        &mut self,
        list: &DisplayList,
        sources: Sources,
        view: &Viewport,
    ) -> Result<TiledFrame> {
        self.raster_region(list, sources, view, (0, 0), false)
    }

    /// Puts pixels that came ahead of the lists that show them on the device, for a
    /// [`crate::raster_tables::ImageTable`] to resolve to: what `shown` says, of `pixels`, which the
    /// table has checked.
    pub fn upload(
        &mut self,
        fonts: &dyn FontResolver,
        id: ImageId,
        shown: &ShownImage,
        pixels: &[u8],
    ) -> Result<()> {
        if self.res.is_none() {
            self.res = Some(Resources::new(&self.ctx, fonts)?);
        }
        let res = self.res.as_mut().unwrap();
        res.upload(&self.ctx, id, shown, pixels)
    }

    /// What the last full raster did.
    pub fn stats(&self) -> RasterStats {
        self.stats
    }

    /// What the last full raster (and the pieces after it) took.
    pub fn work(&self) -> RasterWork {
        self.work
    }

    /// How many gradient stop collections this rasterizer has made so far (each distinct gradient
    /// once, while it stays among the recent ones: for tests).
    pub fn gradient_stops_made(&self) -> u64 {
        self.res.as_ref().map_or(0, |r| r.stops_made)
    }

    /// How many backdrop filter results this rasterizer has rendered so far (each backdrop filter
    /// once a frame, however many tiles and later backdrops show it: for tests).
    pub fn backdrops_made(&self) -> u64 {
        self.res.as_ref().map_or(0, |r| r.backdrops_made)
    }

    /// [`TileRasterizer::raster`] for the part of `list` whose top-left corner is at device pixel
    /// `origin` (the frame's pixel (0, 0) shows the list's device pixel `origin`). With `sparse`,
    /// tiles that nothing draws into are left out of the frame (for a transparent layer that a
    /// compositor draws over another).
    pub fn raster_region(
        &mut self,
        list: &DisplayList,
        sources: Sources,
        view: &Viewport,
        origin: (i32, i32),
        sparse: bool,
    ) -> Result<TiledFrame> {
        let slot = (!sparse).then_some(0);
        self.raster_tiles(list, sources, view, origin, sparse, slot)
    }

    /// [`TileRasterizer::raster_region`] (sparse) for a piece of a frame that a compositor draws
    /// over the full frame rastered before it: tiles of the piece that show what the same `slot`'s
    /// tiles showed at the last raster are copied instead of painted (Chromium's tiles are kept per
    /// layer the same way). Slot 0 is the full frame's.
    pub fn raster_piece(
        &mut self,
        list: &DisplayList,
        sources: Sources,
        view: &Viewport,
        origin: (i32, i32),
        slot: u64,
    ) -> Result<TiledFrame> {
        self.raster_tiles(list, sources, view, origin, true, Some(slot.max(1)))
    }

    fn raster_tiles(
        &mut self,
        list: &DisplayList,
        sources: Sources,
        view: &Viewport,
        origin: (i32, i32),
        sparse: bool,
        slot: Option<u64>,
    ) -> Result<TiledFrame> {
        let started = std::time::Instant::now();
        list.validate()
            .map_err(|e| windows::core::Error::new(E_INVALIDARG, e.to_string()))?;
        if self.res.is_none() {
            self.res = Some(Resources::new(&self.ctx, sources.text)?);
        }
        let opaque = view.background.is_opaque();
        let base = Matrix::scale(view.scale, view.scale)
            .then(&Matrix::translation(-origin.0 as f32, -origin.1 as f32));
        let mut bounds = {
            let res = self.res.as_mut().unwrap();
            res.trim(sources.images);
            raster::bounds_into(res, list, &base);
            raster::take_bounds(res)
        };
        let (columns, rows, size) = grid(view);
        // Damage (full frames only): the hash of each tile's content, against the last frame's.
        let key = FrameKey::of(view, origin);
        let hashes = if slot.is_none() {
            Vec::new()
        } else {
            let mut item_hashes = Vec::new();
            list.content_hashes(&|id| sources.images.version(id), &mut item_hashes);
            tile_hashes(list, &item_hashes, &bounds, columns, rows)
        };
        if slot == Some(0) {
            // A new frame: pieces the last one did not raster go.
            self.retained.retain(|r| r.used || r.slot == 0);
            for r in &mut self.retained {
                r.used = false;
            }
            self.stats = RasterStats::default();
            self.work = RasterWork::default();
        }
        let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1000.0;
        self.work.prepare += ms(started);
        let (mut paint, mut copy) = (0.0, 0.0);
        let previous = slot.and_then(|slot| {
            let at = self.retained.iter().position(|r| r.slot == slot)?;
            Some(self.retained.swap_remove(at)).filter(|r| r.key == key)
        });
        let mut stats = RasterStats::default();
        let mut tiles = Vec::new();
        self.res.as_mut().unwrap().begin_frame();
        let result = (|| -> Result<()> {
            for ty in 0..rows {
                for tx in 0..columns {
                    if slot.is_some() {
                        let hash = hashes[(ty * columns + tx) as usize];
                        let same = previous.as_ref().and_then(|r| {
                            r.tiles.iter().find(|t| {
                                t.x == tx * TILE_SIZE && t.y == ty * TILE_SIZE && t.hash == hash
                            })
                        });
                        if let Some(old) = same {
                            let t = std::time::Instant::now();
                            let tile = self.tile(tx * TILE_SIZE, ty * TILE_SIZE, opaque, size)?;
                            // SAFETY: both bitmaps are of this device and of the tile size and
                            // format; the old one is not being drawn into (its frame is finished).
                            let r = unsafe { tile.bitmap.CopyFromBitmap(None, &old.bitmap, None) };
                            tiles.push(tile);
                            copy += ms(t);
                            r?;
                            stats.reused += 1;
                            continue;
                        }
                    }
                    if sparse && !bounds.iter().any(|b| b.intersects(&tile_area(tx, ty))) {
                        continue;
                    }
                    let t = std::time::Instant::now();
                    let tile = self.tile(tx * TILE_SIZE, ty * TILE_SIZE, opaque, size)?;
                    let r = self.paint_tile(&tile, list, sources, view, &base, &bounds);
                    tiles.push(tile);
                    paint += ms(t);
                    r?;
                    stats.painted += 1;
                }
            }
            Ok(())
        })();
        self.res.as_mut().unwrap().end_frame();
        self.work.paint += paint;
        self.work.copy += copy;
        bounds.clear();
        raster::put_bounds(self.res.as_mut().unwrap(), bounds);
        match result {
            Ok(()) => {
                self.stats.painted += stats.painted;
                self.stats.reused += stats.reused;
                if let Some(slot) = slot {
                    let index = |t: &Tile| ((t.y / TILE_SIZE) * columns + t.x / TILE_SIZE) as usize;
                    self.retained.push(Retained {
                        slot,
                        key,
                        tiles: tiles
                            .iter()
                            .map(|t| RetainedTile {
                                x: t.x,
                                y: t.y,
                                hash: hashes[index(t)],
                                texture: t.texture.clone(),
                                bitmap: t.bitmap.clone(),
                            })
                            .collect(),
                        used: true,
                    });
                }
                Ok(TiledFrame {
                    width: view.width,
                    height: view.height,
                    tiles,
                })
            }
            Err(e) => {
                self.pool.extend(tiles);
                Err(e)
            }
        }
    }

    fn paint_tile(
        &mut self,
        tile: &Tile,
        list: &DisplayList,
        sources: Sources,
        view: &Viewport,
        base: &Matrix,
        bounds: &[Rect],
    ) -> Result<()> {
        let offset = Matrix::translation(-(tile.x as f32), -(tile.y as f32));
        let area = Rect::new(
            tile.x as f32,
            tile.y as f32,
            TILE_SIZE as f32,
            TILE_SIZE as f32,
        );
        // SAFETY: the context draws into the tile's bitmap between BeginDraw and EndDraw on this
        // thread; the target is reset afterwards so the context holds no reference to the tile.
        unsafe {
            self.ctx.SetTarget(&tile.bitmap);
            self.ctx.BeginDraw();
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx.Clear(Some(&color_f(view.background)));
            {
                let res = self.res.as_mut().unwrap();
                let mut painter =
                    Painter::new(&self.ctx, res, sources, base.then(&offset), tile.opaque)
                        .over_canvas(view.background)
                        // (the whole frame, so that a backdrop filter's result serves every
                        // tile it is in)
                        .within(
                            Rect::new(
                                -(tile.x as f32),
                                -(tile.y as f32),
                                view.width as f32,
                                view.height as f32,
                            ),
                            crate::geom::Point::new(tile.x as f32, tile.y as f32),
                        );
                // (bounds are frame device pixels: compare with the tile's area there)
                painter.paint(list, Some((bounds, area)));
                self.work.items += painter.drawn();
            }
            let r = self.ctx.EndDraw(None, None);
            self.ctx.SetTarget(None);
            r
        }
    }

    /// Returns a frame's tiles to the pool for reuse. (Recycling the last full frame itself, which
    /// the next raster would copy unchanged tiles from, forgets it: its tiles are painted over.)
    pub fn recycle(&mut self, frame: TiledFrame) {
        self.retained.retain(|r| {
            !r.tiles
                .iter()
                .any(|rt| frame.tiles.iter().any(|t| t.texture == rt.texture))
        });
        self.pool.extend(frame.tiles);
        self.pool.truncate(64);
    }

    /// Draws `base` and then `pieces` into one picture of `base`'s size, as the compositor draws a
    /// tab's frame and its layers ([`crate::ContentPass::tiles_through`]), the list's device
    /// pixel (0, 0) at the picture's: each piece is a frame, its pixel (0, 0) at its device pixel
    /// `at`, and the state it is drawn through. For tests and tools: what a split list looks
    /// like on screen. Needs a raster before it (the rasterizer's resources).
    pub fn compose(
        &mut self,
        base: &TiledFrame,
        pieces: &[(&TiledFrame, (i32, i32), &crate::display_list::LayerState)],
        scale: f32,
        background: crate::geom::Color,
    ) -> Result<Pixmap> {
        let Some(res) = self.res.as_ref() else {
            return Err(windows::core::Error::new(
                E_INVALIDARG,
                "nothing rastered yet",
            ));
        };
        let (w, h) = (base.width.max(1), base.height.max(1));
        let full = Rect::new(0.0, 0.0, w as f32, h as f32);
        let props = |options| D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: options,
            colorContext: ManuallyDrop::new(None),
        };
        let size = D2D_SIZE_U {
            width: w,
            height: h,
        };
        // SAFETY: the context draws into a bitmap of its own between BeginDraw and EndDraw on this
        // thread and lets go of it after; the CPU-readable copy is mapped once, read row by row
        // within its pitch, and unmapped.
        unsafe {
            let target =
                self.ctx
                    .CreateBitmap(size, None, 0, &props(D2D1_BITMAP_OPTIONS_TARGET))?;
            self.ctx.SetTarget(&target);
            self.ctx.BeginDraw();
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx.Clear(Some(&color_f(background)));
            let none = crate::display_list::LayerState::default();
            crate::compositor::draw_through(
                &self.ctx,
                res,
                base,
                (0, 0),
                &none,
                scale,
                (0, 0),
                full,
            );
            for (frame, at, state) in pieces {
                crate::compositor::draw_through(
                    &self.ctx,
                    res,
                    frame,
                    *at,
                    state,
                    scale,
                    (0, 0),
                    full,
                );
            }
            let r = self.ctx.EndDraw(None, None);
            self.ctx.SetTarget(None);
            r?;
            let staging = self.ctx.CreateBitmap(
                size,
                None,
                0,
                &props(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW),
            )?;
            staging.CopyFromBitmap(None, &target, None)?;
            let mapped = staging.Map(D2D1_MAP_OPTIONS_READ)?;
            let mut data = vec![0u8; w as usize * h as usize * 4];
            for y in 0..h as usize {
                let src = std::slice::from_raw_parts(
                    mapped.bits.add(y * mapped.pitch as usize),
                    w as usize * 4,
                );
                data[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(src);
            }
            staging.Unmap()?;
            Ok(Pixmap {
                width: w,
                height: h,
                data,
            })
        }
    }

    /// Copies a frame back to memory (waits for the GPU to finish it).
    pub fn read_back(&mut self, frame: &TiledFrame) -> Result<Pixmap> {
        let (w, h) = (frame.width.max(1), frame.height.max(1));
        let opaque = frame.tiles.first().is_none_or(|t| t.opaque);
        // SAFETY: a CPU-readable bitmap as large as the frame receives each tile's visible part; it is
        // mapped once, read row by row within its pitch, and unmapped.
        unsafe {
            let staging = self.ctx.CreateBitmap(
                D2D_SIZE_U {
                    width: w,
                    height: h,
                },
                None,
                0,
                &D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: if opaque {
                            D2D1_ALPHA_MODE_IGNORE
                        } else {
                            D2D1_ALPHA_MODE_PREMULTIPLIED
                        },
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    colorContext: ManuallyDrop::new(None),
                },
            )?;
            for t in &frame.tiles {
                let cw = TILE_SIZE.min(w.saturating_sub(t.x));
                let ch = TILE_SIZE.min(h.saturating_sub(t.y));
                if cw == 0 || ch == 0 {
                    continue;
                }
                staging.CopyFromBitmap(
                    Some(&D2D_POINT_2U { x: t.x, y: t.y }),
                    &t.bitmap,
                    Some(&D2D_RECT_U {
                        left: 0,
                        top: 0,
                        right: cw,
                        bottom: ch,
                    }),
                )?;
            }
            let mapped = staging.Map(D2D1_MAP_OPTIONS_READ)?;
            let mut data = vec![0u8; w as usize * h as usize * 4];
            for y in 0..h as usize {
                let src = std::slice::from_raw_parts(
                    mapped.bits.add(y * mapped.pitch as usize),
                    w as usize * 4,
                );
                data[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(src);
            }
            staging.Unmap()?;
            if opaque {
                for px in data.as_chunks_mut::<4>().0 {
                    px[3] = 255;
                }
            }
            Ok(Pixmap {
                width: w,
                height: h,
                data,
            })
        }
    }
}

/// The matrix type Direct2D takes, for callers drawing tiles themselves.
pub fn to_d2d_matrix(m: &Matrix) -> Matrix3x2 {
    matrix_3x2(m)
}
