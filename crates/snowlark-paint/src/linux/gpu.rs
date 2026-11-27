// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tiles on Linux: the device tiles are painted with ([`Gpu`]) and per-thread tile rasterizers
//! ([`TileRasterizer`]) that paint display lists into 512×512 tiles, which a compositor draws at
//! any scroll offset without painting again.
//!
//! Tiles are painted with Skia's CPU backend on either device: premultiplied BGRA pixels in
//! memory. [`Driver::Warp`] is the software path a system without a usable GPU takes on Windows
//! too: the compositor draws on the CPU as well. [`Driver::Hardware`] is OpenGL ES through EGL
//! ([`egl`]): the window's compositor draws with Skia's GL backend ("software raster, GPU
//! compositing"). Until the window shows GL frames itself, `Gpu::new()` makes the hardware device
//! only when `SNOWLARK_GPU=gl` asks for it.
//!
//! Thread rules are the Windows half's: [`Gpu`] is shared, [`TileRasterizer`] stays on the thread
//! that made it (its Skia caches), and [`TiledFrame`] is `Send` (its tiles are pixels no one
//! writes once the frame is handed over).

use std::marker::PhantomData;
use std::sync::Arc;

use skia_safe as sk;

use crate::display_list::{DisplayList, ImageId, Item};
use crate::geom::{Matrix, Rect};
use crate::image::{Pixmap, ShownImage};
use crate::raster::{Caches, FontResolver, Painter, Sources, Viewport};
use crate::tiling::{grid, tile_area, tile_hashes, FrameKey};
pub use crate::tiling::{RasterStats, RasterWork, TILE_SIZE};
use snowlark_graphics::{Error, Result};

/// Which kind of device to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    /// The graphics card.
    Hardware,
    /// A software rasterizer (no GPU needed).
    Warp,
}

#[path = "egl.rs"]
pub(crate) mod egl;

/// The device tiles are painted with (see the module documentation).
pub struct Gpu {
    driver: Driver,
    /// The hardware device's display.
    display: Option<&'static egl::Display>,
}

impl std::fmt::Debug for Gpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gpu")
            .field("driver", &self.driver)
            .field("adapter", &self.adapter())
            .finish()
    }
}

impl Gpu {
    /// The hardware device, or the software one when there is none (as on Windows); the hardware
    /// one only with `SNOWLARK_GPU=gl` so far (see the module documentation).
    pub fn new() -> Result<Arc<Gpu>> {
        if std::env::var("SNOWLARK_GPU").is_ok_and(|v| v == "gl") {
            if let Ok(gpu) = Gpu::with_driver(Driver::Hardware) {
                return Ok(gpu);
            }
        }
        Gpu::with_driver(Driver::Warp)
    }

    pub fn with_driver(driver: Driver) -> Result<Arc<Gpu>> {
        let display = match driver {
            Driver::Hardware => Some(egl::Display::get().map_err(Error::Platform)?),
            Driver::Warp => None,
        };
        Ok(Arc::new(Gpu { driver, display }))
    }

    pub fn driver(&self) -> Driver {
        self.driver
    }

    /// The device's name, for diagnostics.
    pub fn adapter(&self) -> &str {
        self.display.map_or("Skia CPU", |d| d.renderer())
    }

    /// Lets go of resources freed since the last call (nothing is deferred on the CPU).
    pub fn flush(&self) {}

    /// Whether the device was lost (a CPU device never is).
    pub fn is_lost(&self) -> bool {
        false
    }
}

/// A tile of a frame: premultiplied BGRA pixels, `size()` × `size()`, rows `size() * 4` bytes
/// apart. 512×512, or for a frame that fits in one tile, a smaller square that holds it.
pub struct Tile {
    /// The tile's top-left corner in the frame, in device pixels.
    pub x: u32,
    pub y: u32,
    pixels: Arc<Vec<u8>>,
    opaque: bool,
    size: u32,
}

impl Tile {
    /// The tile's pixels (see the type's documentation).
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The side of the tile's square, in device pixels.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Whether every pixel is opaque (the frame's background is).
    pub fn opaque(&self) -> bool {
        self.opaque
    }
}

/// A rasterized frame as tiles covering `width` × `height` device pixels.
pub struct TiledFrame {
    pub width: u32,
    pub height: u32,
    pub tiles: Vec<Tile>,
}

struct RetainedTile {
    x: u32,
    y: u32,
    hash: u64,
    pixels: Arc<Vec<u8>>,
}

struct Retained {
    slot: u64,
    key: FrameKey,
    tiles: Vec<RetainedTile>,
    /// Whether a raster since the last full frame's used it (a piece not rastered for a frame is
    /// dropped at the next one).
    used: bool,
}

/// Paints display lists into tiles on one thread (see the module documentation).
pub struct TileRasterizer {
    gpu: Arc<Gpu>,
    caches: Caches,
    pool: Vec<Tile>,
    /// The last frame's tiles with the hash of what each shows, for the full frame (slot 0) and for
    /// each piece rastered with [`TileRasterizer::raster_piece`] (by its slot): a tile of the next
    /// raster of the same slot and geometry whose items hash the same is copied from it instead of
    /// painted again.
    retained: Vec<Retained>,
    /// Item bounds and open groups, kept to reuse their memory.
    bounds: Vec<Rect>,
    open_groups: Vec<(usize, Rect)>,
    stats: RasterStats,
    work: RasterWork,
    // (its Skia caches belong to one thread)
    _thread_affine: PhantomData<*const ()>,
}

fn image_info(size: (u32, u32)) -> sk::ImageInfo {
    sk::ImageInfo::new(
        (size.0 as i32, size.1 as i32),
        sk::ColorType::BGRA8888,
        sk::AlphaType::Premul,
        None,
    )
}

fn sk_color(c: crate::geom::Color) -> sk::Color {
    sk::Color::from_argb(c.a, c.r, c.g, c.b)
}

impl TileRasterizer {
    pub fn new(gpu: Arc<Gpu>) -> Result<TileRasterizer> {
        Ok(TileRasterizer {
            gpu,
            caches: Caches::default(),
            pool: Vec::new(),
            retained: Vec::new(),
            bounds: Vec::new(),
            open_groups: Vec::new(),
            stats: RasterStats::default(),
            work: RasterWork::default(),
            _thread_affine: PhantomData,
        })
    }

    pub fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
    }

    /// A tile from the pool, or a new one. Its pixels are this rasterizer's alone: a pooled tile
    /// some other owner still shares is dropped for a new one.
    fn tile(&mut self, x: u32, y: u32, opaque: bool, size: u32) -> Tile {
        while let Some(i) = self.pool.iter().position(|t| t.size == size) {
            let mut t = self.pool.swap_remove(i);
            if Arc::get_mut(&mut t.pixels).is_some() {
                t.x = x;
                t.y = y;
                t.opaque = opaque;
                return t;
            }
        }
        Tile {
            x,
            y,
            pixels: Arc::new(vec![0u8; size as usize * size as usize * 4]),
            opaque,
            size,
        }
    }

    /// Paints `list` into the tiles covering the viewport.
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

    /// What the last full raster did.
    pub fn stats(&self) -> RasterStats {
        self.stats
    }

    /// What the last full raster (and the pieces after it) took.
    pub fn work(&self) -> RasterWork {
        self.work
    }

    /// Keeps pixels that came ahead of the lists that show them, for a
    /// [`crate::raster_tables::ImageTable`] (which keeps none) to resolve to: what `shown` says, of
    /// `pixels`, which the table has checked.
    pub fn upload(
        &mut self,
        _fonts: &dyn FontResolver,
        id: ImageId,
        shown: &ShownImage,
        pixels: &[u8],
    ) -> Result<()> {
        self.caches.upload(id, shown, pixels)
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
    /// tiles showed at the last raster are copied instead of painted. Slot 0 is the full frame's.
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
        list.validate().map_err(|_| Error::Invalid)?;
        let key = FrameKey::of(view, origin);
        let base = Matrix::scale(view.scale, view.scale)
            .then(&Matrix::translation(-origin.0 as f32, -origin.1 as f32));
        let mut bounds = std::mem::take(&mut self.bounds);
        list.item_bounds(&base, &mut bounds);
        list.group_bounds(&mut bounds, &mut self.open_groups);
        let (columns, rows, size) = grid(view);
        // Damage (full frames and pieces): the hash of each tile's content, against the last
        // frame's.
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
        // A backdrop filter reads what is painted under it, which a tile alone does not have past
        // its edges: such a frame is painted whole once and cut into tiles.
        let whole = list
            .items
            .iter()
            .any(|i| matches!(i, Item::BackdropFilter { .. }));
        let mut whole_pixels: Option<Vec<u8>> = None;
        let mut stats = RasterStats::default();
        let mut tiles = Vec::new();
        for ty in 0..rows {
            for tx in 0..columns {
                let (x, y) = (tx * TILE_SIZE, ty * TILE_SIZE);
                if slot.is_some() {
                    let hash = hashes[(ty * columns + tx) as usize];
                    let same = previous.as_ref().and_then(|r| {
                        r.tiles
                            .iter()
                            .find(|t| t.x == x && t.y == y && t.hash == hash)
                    });
                    if let Some(old) = same {
                        let t = std::time::Instant::now();
                        let mut tile = self.tile(x, y, key.opaque, size);
                        unshared(&mut tile).copy_from_slice(&old.pixels);
                        tiles.push(tile);
                        copy += ms(t);
                        stats.reused += 1;
                        continue;
                    }
                }
                let area = tile_area(tx, ty);
                if sparse && !bounds.iter().any(|b| b.intersects(&area)) {
                    continue;
                }
                let t = std::time::Instant::now();
                let mut tile = self.tile(x, y, key.opaque, size);
                if whole {
                    let (w, h) = (view.width.max(1), view.height.max(1));
                    let frame = whole_pixels.get_or_insert_with(|| {
                        let mut px = vec![0u8; w as usize * h as usize * 4];
                        paint_into(
                            &mut px,
                            (w, h),
                            &mut self.caches,
                            list,
                            sources,
                            view,
                            &base,
                        );
                        px
                    });
                    cut_tile(frame, w, h, &mut tile);
                } else {
                    let offset = Matrix::translation(-(x as f32), -(y as f32));
                    paint_into(
                        unshared(&mut tile),
                        (size, size),
                        &mut self.caches,
                        list,
                        sources,
                        view,
                        &base.then(&offset),
                    );
                }
                self.work.items += list
                    .items
                    .iter()
                    .zip(&bounds)
                    .filter(|(i, b)| i.is_drawing() && b.intersects(&area))
                    .count() as u32;
                tiles.push(tile);
                paint += ms(t);
                stats.painted += 1;
            }
        }
        self.work.paint += paint;
        self.work.copy += copy;
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
                        pixels: t.pixels.clone(),
                    })
                    .collect(),
                used: true,
            });
        }
        bounds.clear();
        self.bounds = bounds;
        Ok(TiledFrame {
            width: view.width,
            height: view.height,
            tiles,
        })
    }

    /// Returns a frame's tiles to the pool for reuse. (Recycling the last full frame itself, which
    /// the next raster would copy unchanged tiles from, forgets it: its tiles are painted over.)
    pub fn recycle(&mut self, frame: TiledFrame) {
        self.retained.retain(|r| {
            !r.tiles.iter().any(|rt| {
                frame
                    .tiles
                    .iter()
                    .any(|t| Arc::ptr_eq(&t.pixels, &rt.pixels))
            })
        });
        self.pool.extend(frame.tiles);
        self.pool.truncate(64);
    }

    /// Copies a frame to one pixmap.
    pub fn read_back(&mut self, frame: &TiledFrame) -> Result<Pixmap> {
        Ok(read_back(frame))
    }
}

/// The pixels of a tile [`TileRasterizer::tile`] gave out, which no one else holds yet.
fn unshared(tile: &mut Tile) -> &mut [u8] {
    Arc::get_mut(&mut tile.pixels).expect("a new tile's pixels are unshared")
}

/// Paints `list` with `base` into `pixels` (`size`, premultiplied BGRA, tightly packed) over the
/// view's background.
fn paint_into(
    pixels: &mut [u8],
    size: (u32, u32),
    caches: &mut Caches,
    list: &DisplayList,
    sources: Sources,
    view: &Viewport,
    base: &Matrix,
) {
    let info = image_info(size);
    let Some(mut surface) = sk::surfaces::wrap_pixels(&info, pixels, size.0 as usize * 4, None)
    else {
        return;
    };
    let canvas = surface.canvas();
    canvas.clear(sk_color(view.background));
    let mut painter = Painter::new(canvas, caches, sources, *base);
    painter.paint(list);
}

/// Copies the tile's square out of a whole frame's pixels (`w` × `h`); what lies past the frame's
/// edge is left as it was.
fn cut_tile(frame: &[u8], w: u32, h: u32, tile: &mut Tile) {
    let size = tile.size as usize;
    let (x, y) = (tile.x as usize, tile.y as usize);
    let cw = (w as usize).saturating_sub(x).min(size);
    let ch = (h as usize).saturating_sub(y).min(size);
    let px = unshared(tile);
    for row in 0..ch {
        let src = ((y + row) * w as usize + x) * 4;
        px[row * size * 4..row * size * 4 + cw * 4].copy_from_slice(&frame[src..src + cw * 4]);
    }
}

/// A frame's tiles as one pixmap of the frame's size.
pub fn read_back(frame: &TiledFrame) -> Pixmap {
    let (w, h) = (frame.width.max(1), frame.height.max(1));
    let mut data = vec![0u8; w as usize * h as usize * 4];
    for t in &frame.tiles {
        let size = t.size as usize;
        let cw = (w.saturating_sub(t.x) as usize).min(size);
        let ch = (h.saturating_sub(t.y) as usize).min(size);
        for row in 0..ch {
            let dst = ((t.y as usize + row) * w as usize + t.x as usize) * 4;
            data[dst..dst + cw * 4]
                .copy_from_slice(&t.pixels[row * size * 4..row * size * 4 + cw * 4]);
        }
    }
    if frame.tiles.first().is_none_or(|t| t.opaque) {
        for px in data.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }
    Pixmap {
        width: w,
        height: h,
        data,
    }
}

#[cfg(test)]
#[path = "gpu_tests.rs"]
mod tests;
