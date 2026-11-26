// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The bitmap of a `<canvas>` 2D context on the GPU (HTML §4.12.5.1): a Direct2D device context
//! drawing into a bitmap of a Direct3D 11 texture, for `snowlark-render`'s `Canvas2D`, which keeps the
//! drawing state and paths and calls [`GpuCanvas`] with device-space shapes and the state that
//! applies to each operation ([`DrawParams`]).
//!
//! **Device.** Each thread that draws canvases gets one [`CanvasDevice`] (a single-threaded D3D11
//! device with Direct2D and DirectWrite on top), made when its first canvas is; every canvas of the
//! thread shares it, so a canvas's bitmap can be drawn into another canvas without a copy through
//! memory. A canvas has its own device context, which stays between `BeginDraw` and `EndDraw` while
//! script draws, so Direct2D batches the operations; it ends drawing when pixels are read back
//! ([`GpuCanvas::read`], the frame the page shows), which waits for the GPU.
//!
//! **Operations.** Fills of device-aligned rectangles and of circles, and strokes of circles, take
//! Direct2D's primitives; other paths become path geometries. Strokes are drawn in user space (the
//! path mapped back through the inverse of the current transform, the transform set on the context),
//! so line widths, dashes and joins transform as the spec says. Small circles in a solid colour with
//! `source-over` (charts draw thousands) are sprites: each circle size is drawn once into an atlas at
//! 8×8 subpixel phases and stamped with a per-sprite colour by `DrawSpriteBatch`, which costs a few
//! dozen nanoseconds a circle instead of the microsecond of a `FillEllipse`.
//!
//! **Compositing, shadows.** `source-over` draws straight into the bitmap. Other operators draw the
//! shape into a canvas-sized scratch bitmap, then composite that over the whole canvas (within the
//! clip) with the Porter-Duff mode of `DrawImage`, or for the blend modes through the Blend effect
//! over a copy of the canvas. Shadows are the Shadow effect of the same scratch image (blur standard
//! deviation = `shadowBlur / 2`), composited before the shape.
//!
//! **Clipping.** A [`CanvasClip`] is the intersection of device-space paths; one axis-aligned
//! rectangle is an axis-aligned clip, anything else a layer per path with the path as its geometric
//! mask. The clip stays pushed across operations until the state's clip changes.
//!
//! Thread rules: everything here is thread-affine (not `Send`): a device context and the device's
//! resources are used by the thread they belong to. The one exception is the device itself, which
//! [`CanvasDevice::prepare`] may have made on a helper thread and handed over before any use (making
//! one costs about 150 ms in a process that has no Direct3D device yet; Chromium's canvases find the
//! GPU process's device already made).

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::rc::Rc;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use windows::core::{Interface, Result};
use windows::Win32::Foundation::{E_INVALIDARG, HMODULE};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows_numerics::{Matrix3x2, Vector2};

use crate::geom::{Matrix, Point, Rect};
use crate::gpu::Driver;
use crate::raster::matrix_3x2;
use crate::text::{ShapedText, TextEngine};

pub use snowlark_graphics::canvas::*;

/// What a fill or stroke paints with (see [`snowlark_graphics::canvas::CanvasPaint`]): `I` is a
/// [`CanvasImage`] of this thread's device unless said otherwise.
pub type CanvasPaint<'a, I = CanvasImage> = snowlark_graphics::canvas::CanvasPaint<'a, I>;

/// Direct2D's modes for the composite operators.
trait CompositeModes {
    fn porter_duff(self) -> Option<D2D1_COMPOSITE_MODE>;
    fn blend(self) -> D2D1_BLEND_MODE;
}

impl CompositeModes for CompositeOp {
    /// The Porter-Duff operators `DrawImage` composites with, or `None` for the blend modes.
    fn porter_duff(self) -> Option<D2D1_COMPOSITE_MODE> {
        Some(match self {
            CompositeOp::SourceOver => D2D1_COMPOSITE_MODE_SOURCE_OVER,
            CompositeOp::SourceIn => D2D1_COMPOSITE_MODE_SOURCE_IN,
            CompositeOp::SourceOut => D2D1_COMPOSITE_MODE_SOURCE_OUT,
            CompositeOp::SourceAtop => D2D1_COMPOSITE_MODE_SOURCE_ATOP,
            CompositeOp::DestinationOver => D2D1_COMPOSITE_MODE_DESTINATION_OVER,
            CompositeOp::DestinationIn => D2D1_COMPOSITE_MODE_DESTINATION_IN,
            CompositeOp::DestinationOut => D2D1_COMPOSITE_MODE_DESTINATION_OUT,
            CompositeOp::DestinationAtop => D2D1_COMPOSITE_MODE_DESTINATION_ATOP,
            CompositeOp::Lighter => D2D1_COMPOSITE_MODE_PLUS,
            CompositeOp::Copy => D2D1_COMPOSITE_MODE_SOURCE_COPY,
            CompositeOp::Xor => D2D1_COMPOSITE_MODE_XOR,
            _ => return None,
        })
    }

    fn blend(self) -> D2D1_BLEND_MODE {
        match self {
            CompositeOp::Multiply => D2D1_BLEND_MODE_MULTIPLY,
            CompositeOp::Screen => D2D1_BLEND_MODE_SCREEN,
            CompositeOp::Overlay => D2D1_BLEND_MODE_OVERLAY,
            CompositeOp::Darken => D2D1_BLEND_MODE_DARKEN,
            CompositeOp::Lighten => D2D1_BLEND_MODE_LIGHTEN,
            CompositeOp::ColorDodge => D2D1_BLEND_MODE_COLOR_DODGE,
            CompositeOp::ColorBurn => D2D1_BLEND_MODE_COLOR_BURN,
            CompositeOp::HardLight => D2D1_BLEND_MODE_HARD_LIGHT,
            CompositeOp::SoftLight => D2D1_BLEND_MODE_SOFT_LIGHT,
            CompositeOp::Difference => D2D1_BLEND_MODE_DIFFERENCE,
            CompositeOp::Exclusion => D2D1_BLEND_MODE_EXCLUSION,
            CompositeOp::Hue => D2D1_BLEND_MODE_HUE,
            CompositeOp::Saturation => D2D1_BLEND_MODE_SATURATION,
            CompositeOp::Color => D2D1_BLEND_MODE_COLOR,
            _ => D2D1_BLEND_MODE_LUMINOSITY,
        }
    }
}

/// An image a canvas draws (`drawImage`, patterns): a bitmap of its thread's [`CanvasDevice`].
#[derive(Clone)]
pub struct CanvasImage {
    bitmap: ID2D1Bitmap1,
    pub width: u32,
    pub height: u32,
}

/// The per-thread device every canvas of the thread draws with (see the module documentation).
pub struct CanvasDevice {
    d3d: ID3D11Device,
    factory: ID2D1Factory1,
    device: ID2D1Device,
    driver: Driver,
    /// Made by [`CanvasDevice::prepare`]'s helper thread rather than by the first canvas.
    prepared: bool,
    /// That thread drew with it once and read the result back ([`warm`]).
    warmed: bool,
    atlas: RefCell<Atlas>,
    pool: RefCell<Vec<Parts>>,
}

/// A device's Direct3D and Direct2D objects as a helper thread makes them, before any use.
struct Made {
    d3d: ID3D11Device,
    factory: ID2D1Factory1,
    device: ID2D1Device,
    warmed: bool,
}

type Pending = (Driver, std::thread::JoinHandle<Result<Made>>);

thread_local! {
    /// The calling thread's canvas device, made by the first canvas (per driver).
    static DEVICES: RefCell<Vec<Rc<CanvasDevice>>> = const { RefCell::new(Vec::new()) };
    /// Devices [`CanvasDevice::prepare`] is making for this thread.
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
}

impl CanvasDevice {
    /// The calling thread's device for `driver`, made the first time.
    pub fn for_thread(driver: Driver) -> Result<Rc<CanvasDevice>> {
        if let Some(d) = Self::existing(driver) {
            return Ok(d);
        }
        let pending = PENDING.with(|p| {
            let mut p = p.borrow_mut();
            let i = p.iter().position(|(d, _)| *d == driver)?;
            Some(p.swap_remove(i).1)
        });
        // A device the helper thread made (waiting for it if it is not done: that is still sooner
        // than starting now); one it failed to make, or one that was lost, is made here.
        let prepared = pending
            .and_then(|h| h.join().ok())
            .and_then(|made| made.ok())
            .map(|made| CanvasDevice::from_made(made, driver, true))
            .filter(|d| !d.is_lost());
        let device = Rc::new(match prepared {
            Some(d) => d,
            None => CanvasDevice::new(driver)?,
        });
        DEVICES.with(|d| {
            let mut d = d.borrow_mut();
            d.retain(|d| d.driver != driver);
            d.push(device.clone());
        });
        Ok(device)
    }

    /// Starts making this thread's device for `driver` on a helper thread, unless it has one or
    /// one is being made: the first canvas then finds it made. Called when a document gets a canvas
    /// element, so that the device is ready by the time script asks for a context.
    pub fn prepare(driver: Driver) {
        if Self::existing(driver).is_some()
            || PENDING.with(|p| p.borrow().iter().any(|(d, _)| *d == driver))
        {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("canvas-device".into())
            .spawn(move || {
                let mut made = Self::make(driver)?;
                // (a device that cannot draw is found out by the canvas that uses it)
                made.warmed = warm(&made).is_ok();
                Ok(made)
            });
        if let Ok(handle) = spawned {
            PENDING.with(|p| p.borrow_mut().push((driver, handle)));
        }
    }

    /// Whether this thread has a device for `driver`, or one is being made for it.
    pub fn is_prepared(driver: Driver) -> bool {
        Self::existing(driver).is_some()
            || PENDING.with(|p| p.borrow().iter().any(|(d, _)| *d == driver))
    }

    /// Whether the device was made by [`CanvasDevice::prepare`]'s helper thread.
    pub fn was_prepared(&self) -> bool {
        self.prepared
    }

    /// Whether that thread also drew with it once and read the result back, so that the first
    /// canvas does not wait for the process's first drawing ([`warm`]).
    pub fn was_warmed(&self) -> bool {
        self.warmed
    }

    /// This thread's device for `driver`, if it has one that works.
    fn existing(driver: Driver) -> Option<Rc<CanvasDevice>> {
        DEVICES.with(|d| {
            d.borrow()
                .iter()
                .find(|d| d.driver == driver && !d.is_lost())
                .cloned()
        })
    }

    fn new(driver: Driver) -> Result<CanvasDevice> {
        Ok(Self::from_made(Self::make(driver)?, driver, false))
    }

    /// The calling thread's device for `driver`, made and warmed here ([`warm`]) the first time:
    /// for a thread that exists to draw canvases, so nothing waits on it while it does.
    pub fn make_for_thread(driver: Driver) -> Result<Rc<CanvasDevice>> {
        if let Some(d) = Self::existing(driver) {
            return Ok(d);
        }
        let mut made = Self::make(driver)?;
        made.warmed = warm(&made).is_ok();
        let device = Rc::new(Self::from_made(made, driver, true));
        DEVICES.with(|d| {
            let mut d = d.borrow_mut();
            d.retain(|d| d.driver != driver);
            d.push(device.clone());
        });
        Ok(device)
    }

    fn from_made(made: Made, driver: Driver, prepared: bool) -> CanvasDevice {
        CanvasDevice {
            d3d: made.d3d,
            factory: made.factory,
            device: made.device,
            driver,
            prepared,
            warmed: made.warmed,
            atlas: RefCell::new(Atlas::default()),
            pool: RefCell::new(Vec::new()),
        }
    }

    /// The device's Direct3D and Direct2D objects. On any thread: the objects are single-threaded,
    /// and the thread that makes them hands them over before anything else uses them.
    fn make(driver: Driver) -> Result<Made> {
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
        // SAFETY: device creation with valid arguments; out pointers are locals; every returned
        // interface is owned and released on drop. The device is single-threaded: only one thread
        // uses it at a time (this one, then the thread it is handed to, which owns the `Rc`).
        unsafe {
            let mut d3d = None;
            D3D11CreateDevice(
                None,
                kind,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_SINGLETHREADED,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                None,
            )?;
            let d3d = d3d.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let device = factory.CreateDevice(&dxgi)?;
            Ok(Made {
                d3d,
                factory,
                device,
                warmed: false,
            })
        }
    }

    pub fn driver(&self) -> Driver {
        self.driver
    }

    fn is_lost(&self) -> bool {
        // SAFETY: no preconditions.
        unsafe { self.d3d.GetDeviceRemovedReason().is_err() }
    }
}

/// Draws once with a new device, as a canvas does, and reads the result back to the CPU, so that a
/// page's first canvas does not wait for what a process's first drawing costs: the driver making
/// the shaders Direct2D asks for (fills, geometry, strokes, sprites), and the first copy to a
/// CPU-readable bitmap and its mapping. The window's process has done all of this for its
/// compositor; a renderer process had not (its first chart's frame waited about 5 ms on the GPU
/// for it). Chromium's canvases draw in the GPU process, warm from compositing, which is also given
/// the shaders cached on disk as it starts (`GpuProcessHost`, the shader disk cache).
fn warm(made: &Made) -> Result<()> {
    const SIDE: u32 = 64;
    // SAFETY: objects made on and used only by this thread, on a device no other thread uses yet;
    // BeginDraw is balanced by EndDraw; the mapping is unmapped before returning.
    unsafe {
        let ctx = made
            .device
            .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        let size = D2D_SIZE_U {
            width: SIDE,
            height: SIDE,
        };
        let target = ctx.CreateBitmap(size, None, 0, &bitmap_props(D2D1_BITMAP_OPTIONS_TARGET))?;
        let atlas = ctx.CreateBitmap(
            D2D_SIZE_U {
                width: 8,
                height: 8,
            },
            None,
            0,
            &bitmap_props(D2D1_BITMAP_OPTIONS_NONE),
        )?;
        ctx.SetTarget(&target);
        ctx.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
        let brush = ctx.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: 0.2,
                g: 0.4,
                b: 0.8,
                a: 0.5,
            },
            None,
        )?;
        let path = made.factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        sink.BeginFigure(Vector2 { X: 4.0, Y: 4.0 }, D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLine(Vector2 { X: 60.0, Y: 10.0 });
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: Vector2 { X: 40.0, Y: 40.0 },
            point2: Vector2 { X: 20.0, Y: 50.0 },
            point3: Vector2 { X: 8.0, Y: 60.0 },
        });
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        let ellipse = D2D1_ELLIPSE {
            point: Vector2 { X: 32.0, Y: 32.0 },
            radiusX: 10.0,
            radiusY: 10.0,
        };
        ctx.BeginDraw();
        ctx.Clear(Some(&D2D1_COLOR_F::default()));
        ctx.FillRectangle(
            &D2D_RECT_F {
                left: 1.5,
                top: 1.5,
                right: 30.0,
                bottom: 20.0,
            },
            &brush,
        );
        ctx.FillGeometry(&path, &brush, None);
        ctx.DrawGeometry(&path, &brush, 2.0, None);
        ctx.FillEllipse(&ellipse, &brush);
        ctx.DrawEllipse(&ellipse, &brush, 1.5, None);
        ctx.DrawLine(
            Vector2 { X: 0.0, Y: 63.0 },
            Vector2 { X: 63.0, Y: 0.0 },
            &brush,
            1.0,
            None,
        );
        ctx.PushAxisAlignedClip(
            &D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: 40.0,
                bottom: 40.0,
            },
            D2D1_ANTIALIAS_MODE_ALIASED,
        );
        let ctx3: ID2D1DeviceContext3 = ctx.cast()?;
        let batch = ctx3.CreateSpriteBatch()?;
        let dst = D2D_RECT_F {
            left: 10.0,
            top: 10.0,
            right: 18.0,
            bottom: 18.0,
        };
        let src = D2D_RECT_U {
            left: 0,
            top: 0,
            right: 8,
            bottom: 8,
        };
        let color = D2D1_COLOR_F {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        batch.AddSprites(1, &dst, Some(&src), Some(&color), None, 0, 0, 0, 0)?;
        ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
        ctx3.DrawSpriteBatch(
            &batch,
            0,
            1,
            &atlas,
            D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            D2D1_SPRITE_OPTIONS_NONE,
        );
        ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        ctx.PopAxisAlignedClip();
        ctx.EndDraw(None, None)?;
        let staging = ctx.CreateBitmap(
            size,
            None,
            0,
            &bitmap_props(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW),
        )?;
        staging.CopyFromBitmap(None, &target, None)?;
        let mapped = staging.Map(D2D1_MAP_OPTIONS_READ)?;
        let _ = std::ptr::read_volatile(mapped.bits);
        staging.Unmap()?;
        ctx.SetTarget(None);
    }
    Ok(())
}

/// An atlas cell: its top-left corner, the side of each phase's square and the circle's centre
/// offset in it (the circle is drawn at `corner + phase * size + offset + phase / 8`).
#[derive(Clone, Copy)]
struct Cell {
    x: u32,
    y: u32,
    size: u32,
    offset: f32,
}

/// The device's circle atlas: each circle kind (radius, and stroke width or 0 for a fill) drawn in
/// white at 8×8 subpixel phases, packed in shelves. When full it starts over in a new bitmap (a new
/// generation; canvases flush sprites queued against the old one first).
#[derive(Default)]
struct Atlas {
    bitmap: Option<ID2D1Bitmap1>,
    generation: u64,
    size: u32,
    /// The shelf packer's state: next free x, y and the shelf's height.
    next: (u32, u32, u32),
    cells: FxHashMap<(u32, u32), Cell>,
    /// The context that draws into the atlas.
    ctx: Option<ID2D1DeviceContext>,
}

/// The device objects of a canvas that went away, for the next canvas of the same size.
struct Parts {
    width: u32,
    height: u32,
    ctx: ID2D1DeviceContext,
    target: ID2D1Bitmap1,
    brush: ID2D1SolidColorBrush,
    batch: ID2D1SpriteBatch,
    staging: Option<ID2D1Bitmap1>,
}

/// Released canvases kept for reuse.
const POOL: usize = 4;

impl CanvasDevice {
    /// The atlas cell for a circle kind, drawing it the first time; with the atlas's generation.
    fn atlas_cell(&self, radius: f32, width: f32) -> Result<(u64, Cell)> {
        let key = (radius.to_bits(), width.to_bits());
        {
            let atlas = self.atlas.borrow();
            if let Some(c) = atlas.cells.get(&key) {
                return Ok((atlas.generation, *c));
            }
        }
        let mut atlas = self.atlas.borrow_mut();
        let extent = radius + width / 2.0;
        let offset = (extent + 2.0).ceil();
        let size = (2.0 * offset + 1.0) as u32;
        let block = size * PHASES;
        let fits = |a: &Atlas| {
            let (x, y, shelf) = a.next;
            if x + block <= a.size {
                y + block.max(shelf) <= a.size
            } else {
                y + shelf + block <= a.size
            }
        };
        if atlas.bitmap.is_none() || !fits(&atlas) {
            let size_px = (if atlas.bitmap.is_some() {
                atlas.size * 2
            } else {
                1024
            })
            .max(block.next_power_of_two())
            .min(4096);
            if block > size_px {
                return Err(E_INVALIDARG.into());
            }
            if atlas.ctx.is_none() {
                // SAFETY: creating a device context of this device.
                let ctx = unsafe {
                    self.device
                        .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?
                };
                // SAFETY: a state setter on the new context.
                unsafe { ctx.SetUnitMode(D2D1_UNIT_MODE_PIXELS) };
                atlas.ctx = Some(ctx);
            }
            // SAFETY: plain bitmap creation.
            let bitmap = unsafe {
                atlas.ctx.as_ref().unwrap().CreateBitmap(
                    D2D_SIZE_U {
                        width: size_px,
                        height: size_px,
                    },
                    None,
                    0,
                    &bitmap_props(D2D1_BITMAP_OPTIONS_TARGET),
                )?
            };
            atlas.bitmap = Some(bitmap);
            atlas.generation += 1;
            atlas.size = size_px;
            atlas.next = (0, 0, 0);
            atlas.cells.clear();
        }
        let (mut x, mut y, mut shelf) = atlas.next;
        if x + block > atlas.size {
            y += shelf;
            x = 0;
            shelf = 0;
        }
        atlas.next = (x + block, y, shelf.max(block));
        let ctx = atlas.ctx.clone().unwrap();
        let bitmap = atlas.bitmap.clone().unwrap();
        // SAFETY: the atlas context draws into a region of the atlas that no sprite refers to yet
        // (cells are only handed out after they are drawn), between its own BeginDraw and EndDraw;
        // every pushed clip is popped.
        unsafe {
            ctx.SetTarget(&bitmap);
            ctx.BeginDraw();
            ctx.PushAxisAlignedClip(
                &D2D_RECT_F {
                    left: x as f32,
                    top: y as f32,
                    right: (x + block) as f32,
                    bottom: (y + block) as f32,
                },
                D2D1_ANTIALIAS_MODE_ALIASED,
            );
            ctx.Clear(Some(&D2D1_COLOR_F::default()));
            let white = ctx.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )?;
            for py in 0..PHASES {
                for px in 0..PHASES {
                    let (ox, oy) = (x + px * size, y + py * size);
                    ctx.PushAxisAlignedClip(
                        &D2D_RECT_F {
                            left: ox as f32,
                            top: oy as f32,
                            right: (ox + size) as f32,
                            bottom: (oy + size) as f32,
                        },
                        D2D1_ANTIALIAS_MODE_ALIASED,
                    );
                    let e = D2D1_ELLIPSE {
                        point: Vector2 {
                            X: ox as f32 + offset + px as f32 / PHASES as f32,
                            Y: oy as f32 + offset + py as f32 / PHASES as f32,
                        },
                        radiusX: radius,
                        radiusY: radius,
                    };
                    if width == 0.0 {
                        ctx.FillEllipse(&e, &white);
                    } else {
                        ctx.DrawEllipse(&e, &white, width, None);
                    }
                    ctx.PopAxisAlignedClip();
                }
            }
            ctx.PopAxisAlignedClip();
            ctx.EndDraw(None, None)?;
            ctx.SetTarget(None);
        }
        let cell = Cell { x, y, size, offset };
        atlas.cells.insert(key, cell);
        Ok((atlas.generation, cell))
    }
}

fn straight(c: Rgba, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c[0],
        g: c[1],
        b: c[2],
        a: c[3] * alpha,
    }
}

fn rect_f(r: &Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.x,
        top: r.y,
        right: r.x + r.width,
        bottom: r.y + r.height,
    }
}

fn v2(p: Point) -> Vector2 {
    Vector2 { X: p.x, Y: p.y }
}

const INFINITE_RECT: D2D_RECT_F = D2D_RECT_F {
    left: -f32::MAX,
    top: -f32::MAX,
    right: f32::MAX,
    bottom: f32::MAX,
};

fn bitmap_props(options: D2D1_BITMAP_OPTIONS) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: options,
        colorContext: ManuallyDrop::new(None),
    }
}

/// What is pushed on the context for the applied clip.
enum Pushed {
    AxisClip,
    Layer,
}

/// Sprites waiting for one `DrawSpriteBatch`, and the atlas they are cut from.
struct Sprites {
    batch: ID2D1SpriteBatch,
    atlas: Option<ID2D1Bitmap1>,
    /// The atlas generation of `atlas` (0: none yet).
    generation: u64,
    dst: Vec<D2D_RECT_F>,
    src: Vec<D2D_RECT_U>,
    colors: Vec<D2D1_COLOR_F>,
}

/// Circles up to this radius (device pixels, with half the stroke) are sprites.
const MAX_SPRITE_RADIUS: f32 = 24.0;
const PHASES: u32 = 8;

/// A canvas's bitmap on the GPU: see the module documentation.
pub struct GpuCanvas {
    dev: Rc<CanvasDevice>,
    ctx: ID2D1DeviceContext,
    ctx3: ID2D1DeviceContext3,
    target: ID2D1Bitmap1,
    width: u32,
    height: u32,
    brush: ID2D1SolidColorBrush,
    drawing: bool,
    /// The bitmap (made uninitialized) was cleared to transparent black: the first drawing does it.
    cleared: bool,
    /// The clip pushed on the context and how.
    clip: Option<Arc<CanvasClip>>,
    pushed: Vec<Pushed>,
    sprites: Sprites,
    /// Canvas-sized bitmaps: the scratch an operation draws into before compositing, and a copy of
    /// the canvas for the blend modes.
    scratch: Option<ID2D1Bitmap1>,
    copy: Option<ID2D1Bitmap1>,
    /// A CPU-readable bitmap for read-back.
    staging: Option<ID2D1Bitmap1>,
    /// `staging` holds a copy of the whole bitmap queued by [`GpuCanvas::start_read_all`], not
    /// drawn over since.
    staged: bool,
    /// Stroke styles by their parameters.
    strokes: HashMap<(u8, u8, u32, Vec<u32>, u32), ID2D1StrokeStyle>,
}

impl GpuCanvas {
    /// A transparent black canvas of `width` × `height` pixels (each at least 1) on the calling
    /// thread's device for `driver`.
    pub fn new(width: u32, height: u32, driver: Driver) -> Result<GpuCanvas> {
        let dev = CanvasDevice::for_thread(driver)?;
        let (width, height) = (width.max(1), height.max(1));
        let reused = {
            let mut pool = dev.pool.borrow_mut();
            pool.iter()
                .position(|p| p.width == width && p.height == height)
                .map(|i| pool.remove(i))
        };
        let parts = match reused {
            Some(p) => p,
            // SAFETY: object creation on a live device; the bitmap's pixels are undefined until the
            // first drawing clears it (`cleared`).
            None => unsafe {
                let ctx = dev
                    .device
                    .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
                let target = ctx.CreateBitmap(
                    D2D_SIZE_U { width, height },
                    None,
                    0,
                    &bitmap_props(D2D1_BITMAP_OPTIONS_TARGET),
                )?;
                ctx.SetTarget(&target);
                ctx.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
                ctx.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                let brush = ctx.CreateSolidColorBrush(&D2D1_COLOR_F::default(), None)?;
                let batch = ctx.cast::<ID2D1DeviceContext3>()?.CreateSpriteBatch()?;
                Parts {
                    width,
                    height,
                    ctx,
                    target,
                    brush,
                    batch,
                    staging: None,
                }
            },
        };
        let ctx3: ID2D1DeviceContext3 = parts.ctx.cast()?;
        Ok(GpuCanvas {
            dev,
            ctx: parts.ctx,
            ctx3,
            target: parts.target,
            width,
            height,
            brush: parts.brush,
            drawing: false,
            cleared: false,
            clip: None,
            pushed: Vec::new(),
            sprites: Sprites {
                batch: parts.batch,
                atlas: None,
                generation: 0,
                dst: Vec::with_capacity(1024),
                src: Vec::with_capacity(1024),
                colors: Vec::with_capacity(1024),
            },
            scratch: None,
            copy: None,
            staging: parts.staging,
            staged: false,
            strokes: HashMap::new(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn device(&self) -> &Rc<CanvasDevice> {
        &self.dev
    }

    fn begin(&mut self) {
        self.staged = false;
        if !self.drawing {
            // SAFETY: BeginDraw on the canvas's own context, balanced by `end`.
            unsafe {
                self.ctx.BeginDraw();
                self.ctx.SetTransform(&Matrix3x2::identity());
                self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                self.ctx.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_SOURCE_OVER);
                if !self.cleared {
                    self.ctx.Clear(Some(&D2D1_COLOR_F::default()));
                    self.cleared = true;
                }
            }
            self.drawing = true;
        }
    }

    /// Ends drawing: pending sprites drawn, clips popped, everything submitted to the GPU (and the
    /// bitmap cleared if nothing drew yet).
    fn end(&mut self) -> Result<()> {
        if !self.cleared {
            self.begin();
        }
        if !self.drawing {
            return Ok(());
        }
        self.flush_sprites();
        self.pop_clip();
        self.drawing = false;
        // SAFETY: balances `begin`.
        unsafe { self.ctx.EndDraw(None, None) }
    }

    fn pop_clip(&mut self) {
        while let Some(p) = self.pushed.pop() {
            // SAFETY: pops what `apply_clip` pushed on this context, in reverse order.
            unsafe {
                match p {
                    Pushed::AxisClip => self.ctx.PopAxisAlignedClip(),
                    Pushed::Layer => self.ctx.PopLayer(),
                }
            }
        }
        self.clip = None;
    }

    /// Makes `clip` the context's clip (drawing must have begun).
    fn apply_clip(&mut self, clip: Option<&Arc<CanvasClip>>) -> Result<()> {
        let same = match (&self.clip, clip) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if same {
            return Ok(());
        }
        self.flush_sprites();
        self.pop_clip();
        let Some(clip) = clip else {
            return Ok(());
        };
        // SAFETY: pushes on a context between BeginDraw and EndDraw; `pop_clip` pops them. The layer
        // parameters' ManuallyDrop fields are released right after the call.
        unsafe {
            self.ctx.SetTransform(&Matrix3x2::identity());
            if let Some(r) = clip.rect {
                self.ctx
                    .PushAxisAlignedClip(&rect_f(&r), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                self.pushed.push(Pushed::AxisClip);
            } else {
                for (path, evenodd) in &clip.parts {
                    let geometry = self.geometry(path, *evenodd, true)?;
                    let params = D2D1_LAYER_PARAMETERS1 {
                        contentBounds: INFINITE_RECT,
                        geometricMask: ManuallyDrop::new(Some(geometry.cast()?)),
                        maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                        maskTransform: Matrix3x2::identity(),
                        opacity: 1.0,
                        opacityBrush: ManuallyDrop::new(None),
                        layerOptions: D2D1_LAYER_OPTIONS1_NONE,
                    };
                    self.ctx.PushLayer(&params, None);
                    let _ = ManuallyDrop::into_inner(params.geometricMask);
                    self.pushed.push(Pushed::Layer);
                }
            }
        }
        self.clip = Some(clip.clone());
        Ok(())
    }

    /// A path geometry for `path` (`fill`: subpaths are filled figures).
    fn geometry(&self, path: &PathData, evenodd: bool, fill: bool) -> Result<ID2D1PathGeometry1> {
        // SAFETY: plain geometry creation and sink calls; the sink is closed before the geometry is
        // used.
        unsafe {
            let g = self.dev.factory.CreatePathGeometry()?;
            let sink = g.Open()?;
            sink.SetFillMode(if evenodd {
                D2D1_FILL_MODE_ALTERNATE
            } else {
                D2D1_FILL_MODE_WINDING
            });
            let begin = if fill {
                D2D1_FIGURE_BEGIN_FILLED
            } else {
                D2D1_FIGURE_BEGIN_HOLLOW
            };
            let mut open = false;
            let mut start = Point::default();
            let mut current = Point::default();
            let mut i = 0usize;
            let pts = &path.points;
            for v in &path.verbs {
                match v {
                    PathVerb::Move => {
                        if open {
                            sink.EndFigure(D2D1_FIGURE_END_OPEN);
                        }
                        start = pts[i];
                        current = start;
                        sink.BeginFigure(v2(start), begin);
                        open = true;
                        i += 1;
                    }
                    PathVerb::Line => {
                        if !open {
                            sink.BeginFigure(v2(current), begin);
                            open = true;
                        }
                        sink.AddLine(v2(pts[i]));
                        current = pts[i];
                        i += 1;
                    }
                    PathVerb::Quad => {
                        if !open {
                            sink.BeginFigure(v2(current), begin);
                            open = true;
                        }
                        sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                            point1: v2(pts[i]),
                            point2: v2(pts[i + 1]),
                        });
                        current = pts[i + 1];
                        i += 2;
                    }
                    PathVerb::Cubic => {
                        if !open {
                            sink.BeginFigure(v2(current), begin);
                            open = true;
                        }
                        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                            point1: v2(pts[i]),
                            point2: v2(pts[i + 1]),
                            point3: v2(pts[i + 2]),
                        });
                        current = pts[i + 2];
                        i += 3;
                    }
                    PathVerb::Close => {
                        if open {
                            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                            open = false;
                        }
                        current = start;
                    }
                }
            }
            if open {
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
            }
            sink.Close()?;
            Ok(g)
        }
    }

    /// A brush for `paint`, with the canvas's global alpha, whose user space maps through
    /// `transform` to the context's current (local) space, and device pixels through `device`.
    fn brush(
        &self,
        paint: &CanvasPaint,
        alpha: f32,
        transform: &Matrix,
        device: &Matrix,
    ) -> Result<ID2D1Brush> {
        // SAFETY: brush creation on a live context; all results are owned.
        unsafe {
            let props = D2D1_BRUSH_PROPERTIES {
                opacity: alpha,
                transform: matrix_3x2(transform),
            };
            match *paint {
                CanvasPaint::Solid(c) => {
                    self.brush.SetColor(&straight(c, alpha));
                    self.brush.SetOpacity(1.0);
                    Ok(self.brush.cast()?)
                }
                CanvasPaint::Linear { start, end, stops } => {
                    if start == end || stops.is_empty() {
                        // Nothing is painted (HTML: a degenerate linear gradient).
                        return self.transparent();
                    }
                    let stops = self.stops(stops, None)?;
                    Ok(self
                        .ctx
                        .CreateLinearGradientBrush(
                            &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                                startPoint: v2(start),
                                endPoint: v2(end),
                            },
                            Some(&props),
                            &stops,
                        )?
                        .cast()?)
                }
                CanvasPaint::Radial {
                    c0,
                    r0,
                    c1,
                    r1,
                    stops,
                } => {
                    if (c0 == c1 && r0 == r1) || stops.is_empty() || r1 <= 0.0 {
                        return self.transparent();
                    }
                    // Direct2D's radial gradient starts at a point: a start circle of radius r0 is
                    // the first r0 / r1 of the way out (exact for concentric circles).
                    let lead = if r0 > 0.0 && r0 < r1 { r0 / r1 } else { 0.0 };
                    let stops = self.stops(stops, Some(lead))?;
                    Ok(self
                        .ctx
                        .CreateRadialGradientBrush(
                            &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                                center: v2(c1),
                                gradientOriginOffset: Vector2 {
                                    X: c0.x - c1.x,
                                    Y: c0.y - c1.y,
                                },
                                radiusX: r1,
                                radiusY: r1,
                            },
                            Some(&props),
                            &stops,
                        )?
                        .cast()?)
                }
                CanvasPaint::DeviceImage(image) => {
                    let props = D2D1_BRUSH_PROPERTIES {
                        opacity: alpha,
                        transform: matrix_3x2(device),
                    };
                    Ok(self
                        .ctx
                        .CreateBitmapBrush(
                            &image.bitmap,
                            Some(&D2D1_BITMAP_BRUSH_PROPERTIES1 {
                                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                                interpolationMode: D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                            }),
                            Some(&props),
                        )?
                        .cast()?)
                }
                CanvasPaint::Pattern {
                    image,
                    repeat_x,
                    repeat_y,
                } => {
                    // A pattern that does not repeat is transparent outside the image: clamp to a
                    // one-pixel transparent border (see `pattern_bitmap`).
                    let (bitmap, offset) = if repeat_x && repeat_y {
                        (image.bitmap.clone(), (0.0, 0.0))
                    } else {
                        self.bordered(image, !repeat_x, !repeat_y)?
                    };
                    let mode = |repeat| {
                        if repeat {
                            D2D1_EXTEND_MODE_WRAP
                        } else {
                            D2D1_EXTEND_MODE_CLAMP
                        }
                    };
                    let props = D2D1_BRUSH_PROPERTIES {
                        opacity: alpha,
                        transform: matrix_3x2(
                            &Matrix::translation(-offset.0, -offset.1).then(transform),
                        ),
                    };
                    Ok(self
                        .ctx
                        .CreateBitmapBrush(
                            &bitmap,
                            Some(&D2D1_BITMAP_BRUSH_PROPERTIES1 {
                                extendModeX: mode(repeat_x),
                                extendModeY: mode(repeat_y),
                                interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR,
                            }),
                            Some(&props),
                        )?
                        .cast()?)
                }
            }
        }
    }

    fn transparent(&self) -> Result<ID2D1Brush> {
        // SAFETY: a setter on our own brush.
        unsafe { self.brush.SetColor(&D2D1_COLOR_F::default()) };
        self.brush.cast()
    }

    /// A gradient stop collection (sRGB interpolation of unpremultiplied colours, padded); with
    /// `lead`, the stops squeezed into `lead..1`.
    fn stops(
        &self,
        stops: &[GradientStop],
        lead: Option<f32>,
    ) -> Result<ID2D1GradientStopCollection1> {
        let lead = lead.unwrap_or(0.0);
        let mut list: Vec<D2D1_GRADIENT_STOP> = stops
            .iter()
            .map(|s| D2D1_GRADIENT_STOP {
                position: lead + s.offset * (1.0 - lead),
                color: straight(s.color, 1.0),
            })
            .collect();
        if lead > 0.0 {
            list.insert(
                0,
                D2D1_GRADIENT_STOP {
                    position: 0.0,
                    color: straight(stops[0].color, 1.0),
                },
            );
        }
        // SAFETY: the stop slice outlives the call.
        unsafe {
            self.ctx.CreateGradientStopCollection(
                &list,
                D2D1_COLOR_SPACE_SRGB,
                D2D1_COLOR_SPACE_SRGB,
                D2D1_BUFFER_PRECISION_8BPC_UNORM,
                D2D1_EXTEND_MODE_CLAMP,
                D2D1_COLOR_INTERPOLATION_MODE_STRAIGHT,
            )
        }
    }

    /// A copy of `image` with a transparent one-pixel border on the axes that do not repeat, and
    /// where the image's (0, 0) is in it.
    fn bordered(
        &self,
        image: &CanvasImage,
        x: bool,
        y: bool,
    ) -> Result<(ID2D1Bitmap1, (f32, f32))> {
        let (bx, by) = (u32::from(x), u32::from(y));
        let (w, h) = (image.width + 2 * bx, image.height + 2 * by);
        // SAFETY: a zero-filled bitmap (transparent) receives the image's pixels at the border
        // offset by a GPU copy.
        unsafe {
            let zeros = vec![0u8; (w * h * 4) as usize];
            let bitmap = self.ctx.CreateBitmap(
                D2D_SIZE_U {
                    width: w,
                    height: h,
                },
                Some(zeros.as_ptr().cast()),
                w * 4,
                &bitmap_props(D2D1_BITMAP_OPTIONS_NONE),
            )?;
            bitmap.CopyFromBitmap(Some(&D2D_POINT_2U { x: bx, y: by }), &image.bitmap, None)?;
            Ok((bitmap, (bx as f32, by as f32)))
        }
    }

    fn stroke_style(&mut self, s: &StrokeParams) -> Result<Option<ID2D1StrokeStyle>> {
        let simple = s.cap == LineCap::Butt
            && s.join == LineJoin::Miter
            && s.dash.is_empty()
            && s.miter_limit == 10.0;
        if simple {
            return Ok(None);
        }
        let key = (
            s.cap as u8,
            s.join as u8,
            s.miter_limit.to_bits(),
            s.dash.iter().map(|d| d.to_bits()).collect::<Vec<_>>(),
            s.dash_offset.to_bits(),
        );
        if let Some(st) = self.strokes.get(&key) {
            return Ok(Some(st.clone()));
        }
        let cap = match s.cap {
            LineCap::Butt => D2D1_CAP_STYLE_FLAT,
            LineCap::Round => D2D1_CAP_STYLE_ROUND,
            LineCap::Square => D2D1_CAP_STYLE_SQUARE,
        };
        let join = match s.join {
            LineJoin::Miter => D2D1_LINE_JOIN_MITER_OR_BEVEL,
            LineJoin::Round => D2D1_LINE_JOIN_ROUND,
            LineJoin::Bevel => D2D1_LINE_JOIN_BEVEL,
        };
        // Direct2D's dash lengths and offset are in stroke widths.
        let w = s.width.max(f32::MIN_POSITIVE);
        let dashes: Vec<f32> = s.dash.iter().map(|d| d / w).collect();
        // SAFETY: plain object creation; the dash slice outlives the call.
        let style = unsafe {
            self.dev
                .factory
                .CreateStrokeStyle(
                    &D2D1_STROKE_STYLE_PROPERTIES1 {
                        startCap: cap,
                        endCap: cap,
                        dashCap: cap,
                        lineJoin: join,
                        miterLimit: s.miter_limit,
                        dashStyle: if dashes.is_empty() {
                            D2D1_DASH_STYLE_SOLID
                        } else {
                            D2D1_DASH_STYLE_CUSTOM
                        },
                        dashOffset: s.dash_offset / w,
                        transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
                    },
                    if dashes.is_empty() {
                        None
                    } else {
                        Some(&dashes)
                    },
                )?
                .cast::<ID2D1StrokeStyle>()?
        };
        if self.strokes.len() > 64 {
            self.strokes.clear();
        }
        self.strokes.insert(key, style.clone());
        Ok(Some(style))
    }

    /// The canvas-sized scratch bitmap, cleared, as the target; restores the canvas as the target
    /// when `f` returns.
    fn in_scratch(&mut self, f: impl FnOnce(&mut GpuCanvas) -> Result<()>) -> Result<()> {
        if self.scratch.is_none() {
            // SAFETY: plain bitmap creation.
            self.scratch = Some(unsafe {
                self.ctx.CreateBitmap(
                    D2D_SIZE_U {
                        width: self.width,
                        height: self.height,
                    },
                    None,
                    0,
                    &bitmap_props(D2D1_BITMAP_OPTIONS_TARGET),
                )?
            });
        }
        let scratch = self.scratch.clone().unwrap();
        // The clip belongs to the canvas, not to the scratch.
        self.flush_sprites();
        self.pop_clip();
        // SAFETY: switching the target of a context in drawing; nothing is pushed on it.
        unsafe {
            self.ctx.SetTarget(&scratch);
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx.Clear(Some(&D2D1_COLOR_F::default()));
        }
        let r = f(self);
        self.flush_sprites();
        // SAFETY: as above.
        unsafe { self.ctx.SetTarget(&self.target) };
        r
    }

    /// Runs a drawing operation with the state's alpha, compositing, shadow and clip. `draw` draws
    /// the shape with source-over into whatever is the target; it sets its own transform.
    ///
    /// Direct2D composites a layer (a geometric clip) onto the canvas with source-over, so inside one
    /// the other operators would only see the layer: under such a clip they are computed over copies
    /// and merged with the clip's coverage ([`GpuCanvas::masked_composite`]).
    fn operation(
        &mut self,
        p: &DrawParams,
        draw: impl Fn(&mut GpuCanvas) -> Result<()>,
    ) -> Result<()> {
        self.begin();
        let direct = p.op == CompositeOp::SourceOver && p.shadow.is_none();
        if direct {
            self.apply_clip(p.clip)?;
            return draw(self);
        }
        self.in_scratch(&draw)?;
        let scratch: ID2D1Image = self.scratch.clone().unwrap().cast()?;
        let shadow = match p.shadow {
            Some(s) => Some(self.shadow_image(&scratch, &s)?),
            None => None,
        };
        let geometric = p.clip.filter(|c| c.rect.is_none());
        match geometric {
            Some(clip) if p.op != CompositeOp::SourceOver => {
                let mask = self.clip_mask(clip)?;
                self.apply_clip(None)?;
                if let Some(image) = &shadow {
                    self.masked_composite(image, p.op, &mask)?;
                }
                self.masked_composite(&scratch, p.op, &mask)?;
            }
            _ => {
                self.apply_clip(p.clip)?;
                if let Some(image) = &shadow {
                    self.composite(image, p.op)?;
                }
                self.composite(&scratch, p.op)?;
            }
        }
        Ok(())
    }

    /// The shadow of `image` (HTML: blurred with a standard deviation of half `shadowBlur`, in the
    /// shadow colour, moved by the offset).
    fn shadow_image(&self, image: &ID2D1Image, s: &Shadow) -> Result<ID2D1Image> {
        // SAFETY: effect creation and property setters with buffers of the property's size.
        unsafe {
            let shadow = self.ctx.CreateEffect(&CLSID_D2D1Shadow)?;
            shadow.SetInput(0, image, true);
            shadow.SetValue(
                D2D1_SHADOW_PROP_BLUR_STANDARD_DEVIATION.0 as u32,
                D2D1_PROPERTY_TYPE_FLOAT,
                &(s.blur / 2.0).to_le_bytes(),
            )?;
            let color: Vec<u8> = s.color.iter().flat_map(|c| c.to_le_bytes()).collect();
            shadow.SetValue(
                D2D1_SHADOW_PROP_COLOR.0 as u32,
                D2D1_PROPERTY_TYPE_VECTOR4,
                &color,
            )?;
            let out = shadow.GetOutput()?;
            if s.offset == (0.0, 0.0) {
                return Ok(out);
            }
            let t = self.ctx.CreateEffect(&CLSID_D2D12DAffineTransform)?;
            t.SetInput(0, &out, true);
            let m = Matrix3x2::translation(s.offset.0, s.offset.1);
            let bytes = std::slice::from_raw_parts(
                (&m as *const Matrix3x2).cast::<u8>(),
                std::mem::size_of::<Matrix3x2>(),
            );
            t.SetValue(
                D2D1_2DAFFINETRANSFORM_PROP_TRANSFORM_MATRIX.0 as u32,
                D2D1_PROPERTY_TYPE_MATRIX_3X2,
                bytes,
            )?;
            t.GetOutput()
        }
    }

    /// A copy of the canvas as it is (pending drawing included).
    fn canvas_copy(&mut self) -> Result<ID2D1Bitmap1> {
        // SAFETY: plain bitmap creation; the context is flushed so the copy sees what was drawn.
        unsafe {
            if self.copy.is_none() {
                self.copy = Some(self.ctx.CreateBitmap(
                    D2D_SIZE_U {
                        width: self.width,
                        height: self.height,
                    },
                    None,
                    0,
                    &bitmap_props(D2D1_BITMAP_OPTIONS_NONE),
                )?);
            }
            let copy = self.copy.clone().unwrap();
            self.flush_sprites();
            self.ctx.Flush(None, None)?;
            copy.CopyFromBitmap(None, &self.target, None)?;
            Ok(copy)
        }
    }

    /// `destination` and `source` combined with `op` (Composite or Blend effect).
    fn combined(
        &self,
        destination: &ID2D1Image,
        source: &ID2D1Image,
        op: CompositeOp,
    ) -> Result<ID2D1Image> {
        // SAFETY: effect creation and an enum property set with its 4-byte value.
        unsafe {
            let (effect, value) = match op.porter_duff() {
                Some(mode) => (
                    self.ctx.CreateEffect(&CLSID_D2D1Composite)?,
                    (D2D1_COMPOSITE_PROP_MODE.0 as u32, mode.0 as u32),
                ),
                None => (
                    self.ctx.CreateEffect(&CLSID_D2D1Blend)?,
                    (D2D1_BLEND_PROP_MODE.0 as u32, op.blend().0 as u32),
                ),
            };
            effect.SetInput(0, destination, true);
            effect.SetInput(1, source, true);
            effect.SetValue(value.0, D2D1_PROPERTY_TYPE_ENUM, &value.1.to_le_bytes())?;
            effect.GetOutput()
        }
    }

    /// Composites `image` over the whole canvas with `op` (within the applied clip, which is none
    /// or axis-aligned unless `op` is source-over).
    fn composite(&mut self, image: &ID2D1Image, op: CompositeOp) -> Result<()> {
        if let Some(mode) = op.porter_duff() {
            // SAFETY: a draw on a live context in drawing.
            unsafe {
                self.ctx.SetTransform(&Matrix3x2::identity());
                self.ctx.DrawImage(
                    image,
                    None,
                    None,
                    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    mode,
                );
            }
            return Ok(());
        }
        let copy: ID2D1Image = self.canvas_copy()?.cast()?;
        let out = self.combined(&copy, image, op)?;
        // SAFETY: as above.
        unsafe {
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx.DrawImage(
                &out,
                None,
                None,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                D2D1_COMPOSITE_MODE_SOURCE_COPY,
            );
        }
        Ok(())
    }

    /// The coverage of a clip, in a canvas-sized bitmap's alpha.
    fn clip_mask(&mut self, clip: &Arc<CanvasClip>) -> Result<ID2D1Image> {
        // SAFETY: plain bitmap creation.
        let mask = unsafe {
            self.ctx.CreateBitmap(
                D2D_SIZE_U {
                    width: self.width,
                    height: self.height,
                },
                None,
                0,
                &bitmap_props(D2D1_BITMAP_OPTIONS_TARGET),
            )?
        };
        self.flush_sprites();
        self.pop_clip();
        // SAFETY: the context draws the clip's coverage into the mask between the target switches;
        // the clip pushed for it is popped before switching back.
        unsafe {
            self.ctx.SetTarget(&mask);
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx.Clear(Some(&D2D1_COLOR_F::default()));
            self.apply_clip(Some(clip))?;
            self.brush.SetColor(&D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            });
            self.ctx.FillRectangle(
                &D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: self.width as f32,
                    bottom: self.height as f32,
                },
                &self.brush,
            );
            self.pop_clip();
            self.ctx.SetTarget(&self.target);
        }
        mask.cast()
    }

    /// The canvas becomes `op(canvas, image)` where the mask covers it and stays itself elsewhere,
    /// in proportion to the coverage: `combined * mask + canvas * (1 - mask)`.
    fn masked_composite(
        &mut self,
        image: &ID2D1Image,
        op: CompositeOp,
        mask: &ID2D1Image,
    ) -> Result<()> {
        let copy: ID2D1Image = self.canvas_copy()?.cast()?;
        let combined = self.combined(&copy, image, op)?;
        let inside = self.combined(mask, &combined, CompositeOp::SourceIn)?;
        let outside = self.combined(&copy, mask, CompositeOp::DestinationOut)?;
        let out = self.combined(&outside, &inside, CompositeOp::Lighter)?;
        // SAFETY: a draw on a live context in drawing, with no clip pushed.
        unsafe {
            self.ctx.SetTransform(&Matrix3x2::identity());
            self.ctx.DrawImage(
                &out,
                None,
                None,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                D2D1_COMPOSITE_MODE_SOURCE_COPY,
            );
        }
        Ok(())
    }

    /// Fills a device-space shape.
    pub fn fill(&mut self, shape: FillShape, paint: &CanvasPaint, p: &DrawParams) -> Result<()> {
        if let (FillShape::Circle(c, r), CanvasPaint::Solid(color)) = (shape, paint) {
            if p.op == CompositeOp::SourceOver && p.shadow.is_none() && r <= MAX_SPRITE_RADIUS {
                self.begin();
                self.apply_clip(p.clip)?;
                return self.sprite(c, r, 0.0, straight(*color, p.alpha));
            }
        }
        let geometry = match shape {
            FillShape::Path(path, evenodd) => Some(self.geometry(path, evenodd, true)?),
            _ => None,
        };
        let transform = p.transform;
        self.operation(p, |c| {
            c.flush_sprites();
            let brush = c.brush(paint, p.alpha, &transform, &Matrix::IDENTITY)?;
            // SAFETY: drawing on a live context in drawing, device-space coordinates.
            unsafe {
                c.ctx.SetTransform(&Matrix3x2::identity());
                match shape {
                    FillShape::Rect(r) => c.ctx.FillRectangle(&rect_f(&r), &brush),
                    FillShape::Circle(center, r) => c.ctx.FillEllipse(
                        &D2D1_ELLIPSE {
                            point: v2(center),
                            radiusX: r,
                            radiusY: r,
                        },
                        &brush,
                    ),
                    FillShape::Path(..) => {
                        c.ctx.FillGeometry(geometry.as_ref().unwrap(), &brush, None)
                    }
                }
            }
            Ok(())
        })
    }

    /// Strokes a device-space path with line styles in user units (the path is stroked in user
    /// space, through the inverse of the transform).
    pub fn stroke(
        &mut self,
        path: &PathData,
        stroke: &StrokeParams,
        paint: &CanvasPaint,
        p: &DrawParams,
    ) -> Result<()> {
        let Some(inverse) = p.transform.inverse() else {
            return Ok(());
        };
        if stroke.width.is_nan() || stroke.width <= 0.0 {
            return Ok(());
        }
        let user = path.transformed(&inverse);
        let geometry = self.geometry(&user, false, false)?;
        let style = self.stroke_style(stroke)?;
        let transform = p.transform;
        self.operation(p, |c| {
            c.flush_sprites();
            let brush = c.brush(paint, p.alpha, &Matrix::IDENTITY, &inverse)?;
            // SAFETY: drawing on a live context in drawing.
            unsafe {
                c.ctx.SetTransform(&matrix_3x2(&transform));
                c.ctx
                    .DrawGeometry(&geometry, &brush, stroke.width, style.as_ref());
                c.ctx.SetTransform(&Matrix3x2::identity());
            }
            Ok(())
        })
    }

    /// Strokes a device-space circle with a solid colour and a device-space line width (the
    /// transform is a similarity, and the stroke has no dashes): a sprite when small.
    pub fn stroke_circle(
        &mut self,
        center: Point,
        radius: f32,
        width: f32,
        color: Rgba,
        p: &DrawParams,
    ) -> Result<()> {
        if width.is_nan() || width <= 0.0 {
            return Ok(());
        }
        if p.op == CompositeOp::SourceOver
            && p.shadow.is_none()
            && radius + width / 2.0 <= MAX_SPRITE_RADIUS
        {
            self.begin();
            self.apply_clip(p.clip)?;
            return self.sprite(center, radius, width, straight(color, p.alpha));
        }
        self.operation(p, |c| {
            c.flush_sprites();
            let brush = c.brush(
                &CanvasPaint::Solid(color),
                p.alpha,
                &Matrix::IDENTITY,
                &Matrix::IDENTITY,
            )?;
            // SAFETY: drawing on a live context in drawing.
            unsafe {
                c.ctx.SetTransform(&Matrix3x2::identity());
                c.ctx.DrawEllipse(
                    &D2D1_ELLIPSE {
                        point: v2(center),
                        radiusX: radius,
                        radiusY: radius,
                    },
                    &brush,
                    width,
                    None,
                );
            }
            Ok(())
        })
    }

    /// `clearRect`: the shape's pixels become transparent black (within the clip), whatever the
    /// compositing, alpha and shadows.
    pub fn clear(&mut self, shape: FillShape, clip: Option<&Arc<CanvasClip>>) -> Result<()> {
        self.begin();
        self.apply_clip(clip)?;
        self.flush_sprites();
        // SAFETY: drawing on a live context in drawing; the axis-aligned clip pushed here is popped
        // right after.
        unsafe {
            self.ctx.SetTransform(&Matrix3x2::identity());
            if let FillShape::Rect(r) = shape {
                if self.pushed.iter().all(|p| matches!(p, Pushed::AxisClip)) {
                    // (Clear respects axis-aligned clips)
                    self.ctx
                        .PushAxisAlignedClip(&rect_f(&r), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                    self.ctx.Clear(Some(&D2D1_COLOR_F::default()));
                    self.ctx.PopAxisAlignedClip();
                    return Ok(());
                }
            }
            self.ctx.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_COPY);
            let brush = self.transparent()?;
            match shape {
                FillShape::Rect(r) => self.ctx.FillRectangle(&rect_f(&r), &brush),
                FillShape::Circle(c, r) => self.ctx.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: v2(c),
                        radiusX: r,
                        radiusY: r,
                    },
                    &brush,
                ),
                FillShape::Path(path, evenodd) => {
                    let g = self.geometry(path, evenodd, true)?;
                    self.ctx.FillGeometry(&g, &brush, None)
                }
            }
            self.ctx.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_SOURCE_OVER);
        }
        Ok(())
    }

    /// `drawImage`: the `src` rectangle of `image` (in its pixels, within it) into the `dst`
    /// rectangle in user space.
    pub fn draw_image(
        &mut self,
        image: &CanvasImage,
        src: Rect,
        dst: Rect,
        p: &DrawParams,
    ) -> Result<()> {
        let transform = p.transform;
        let interpolation = if p.smoothing {
            D2D1_INTERPOLATION_MODE_LINEAR
        } else {
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
        };
        let aliased = !transform.antialiases_image(dst.width, dst.height);
        self.operation(p, |c| {
            c.flush_sprites();
            // SAFETY: drawing on a live context in drawing; the bitmap is a resource of this device.
            unsafe {
                c.ctx.SetTransform(&matrix_3x2(&transform));
                let mode = c.ctx.GetAntialiasMode();
                if aliased {
                    c.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
                }
                c.ctx.DrawBitmap(
                    &image.bitmap,
                    Some(&rect_f(&dst)),
                    p.alpha,
                    interpolation,
                    Some(&rect_f(&src)),
                    None,
                );
                if aliased {
                    c.ctx.SetAntialiasMode(mode);
                }
                c.ctx.SetTransform(&Matrix3x2::identity());
            }
            Ok(())
        })
    }

    /// `fillText`/`strokeText`: `shaped` (shaped with `text`) with its baseline's left end at
    /// `origin` in user space; strokes outline the glyphs.
    pub fn draw_text(
        &mut self,
        text: &TextEngine,
        shaped: &ShapedText,
        origin: Point,
        paint: &CanvasPaint,
        stroke: Option<&StrokeParams>,
        p: &DrawParams,
    ) -> Result<()> {
        if shaped.glyphs.is_empty() {
            return Ok(());
        }
        let style = match stroke {
            Some(s) => self.stroke_style(s)?,
            None => None,
        };
        let transform = p.transform;
        let local = Matrix::translation(origin.x, origin.y).then(&transform);
        let Some(inverse) = local.inverse() else {
            return Ok(());
        };
        let factory = self.dev.factory.clone();
        self.operation(p, |c| {
            c.flush_sprites();
            // The paint's user space seen from the text's origin.
            let brush = c.brush(
                paint,
                p.alpha,
                &Matrix::translation(-origin.x, -origin.y),
                &inverse,
            )?;
            // SAFETY: drawing on a live context in drawing. Each glyph run borrows its face (never
            // released: ManuallyDrop through transmute_copy) while `face` lives.
            unsafe {
                c.ctx.SetTransform(&matrix_3x2(&local));
                let mut x = 0.0f32;
                for run in &shaped.runs {
                    let Some((face, _)) = text.face(run.font) else {
                        continue;
                    };
                    let g = run.glyphs.0 as usize..run.glyphs.1 as usize;
                    let ids = &shaped.glyphs[g.clone()];
                    let advances = &shaped.advances[g.clone()];
                    let offsets = &shaped.offsets[g];
                    let width: f32 = advances.iter().sum();
                    let glyph_run = DWRITE_GLYPH_RUN {
                        fontFace: std::mem::transmute_copy(&face),
                        fontEmSize: run.size,
                        glyphCount: ids.len() as u32,
                        glyphIndices: ids.as_ptr(),
                        glyphAdvances: advances.as_ptr(),
                        glyphOffsets: offsets.as_ptr().cast(),
                        isSideways: false.into(),
                        bidiLevel: u32::from(run.bidi_level),
                    };
                    // Right-to-left runs are drawn from their right end.
                    let at = if run.bidi_level % 2 == 1 {
                        x + width
                    } else {
                        x
                    };
                    match stroke {
                        None => c.ctx.DrawGlyphRun(
                            Vector2 { X: at, Y: 0.0 },
                            &glyph_run,
                            None,
                            &brush,
                            DWRITE_MEASURING_MODE_NATURAL,
                        ),
                        Some(s) => {
                            let g = factory.CreatePathGeometry()?;
                            let sink = g.Open()?;
                            face.GetGlyphRunOutline(
                                run.size,
                                ids.as_ptr(),
                                Some(advances.as_ptr()),
                                Some(offsets.as_ptr().cast()),
                                ids.len() as u32,
                                false,
                                run.bidi_level % 2 == 1,
                                &sink,
                            )?;
                            sink.Close()?;
                            c.ctx.SetTransform(&matrix_3x2(
                                &Matrix::translation(at, 0.0).then(&local),
                            ));
                            c.ctx.DrawGeometry(&g, &brush, s.width, style.as_ref());
                            c.ctx.SetTransform(&matrix_3x2(&local));
                        }
                    }
                    x += width;
                }
                c.ctx.SetTransform(&Matrix3x2::identity());
            }
            Ok(())
        })
    }

    /// Composites an image covering the canvas (device pixels, from (0, 0)) with the state's alpha,
    /// compositing, shadow and clip: an operation rasterized elsewhere.
    pub fn draw_device_image(&mut self, image: &CanvasImage, p: &DrawParams) -> Result<()> {
        self.operation(p, |c| {
            c.flush_sprites();
            // SAFETY: drawing on a live context in drawing; the bitmap is of this device.
            unsafe {
                c.ctx.SetTransform(&Matrix3x2::identity());
                c.ctx.DrawBitmap(
                    &image.bitmap,
                    None,
                    p.alpha,
                    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    None,
                    None,
                );
            }
            Ok(())
        })
    }

    /// An image of this device from premultiplied BGRA rows (`width * 4` bytes apart).
    pub fn image_from_bgra(&self, width: u32, height: u32, bgra: &[u8]) -> Result<CanvasImage> {
        let (w, h) = (width.max(1), height.max(1));
        if bgra.len() < (w * h * 4) as usize {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: the source covers `h` rows of `w * 4` bytes and outlives the call (copied).
        let bitmap = unsafe {
            self.ctx.CreateBitmap(
                D2D_SIZE_U {
                    width: w,
                    height: h,
                },
                Some(bgra.as_ptr().cast()),
                w * 4,
                &bitmap_props(D2D1_BITMAP_OPTIONS_NONE),
            )?
        };
        Ok(CanvasImage {
            bitmap,
            width: w,
            height: h,
        })
    }

    /// The canvas's current pixels as an image (a GPU copy).
    pub fn snapshot(&mut self) -> Result<CanvasImage> {
        self.end()?;
        // SAFETY: a new bitmap receives a GPU copy of the canvas, which is not in drawing.
        unsafe {
            let bitmap = self.ctx.CreateBitmap(
                D2D_SIZE_U {
                    width: self.width,
                    height: self.height,
                },
                None,
                0,
                &bitmap_props(D2D1_BITMAP_OPTIONS_NONE),
            )?;
            bitmap.CopyFromBitmap(None, &self.target, None)?;
            Ok(CanvasImage {
                bitmap,
                width: self.width,
                height: self.height,
            })
        }
    }

    /// Premultiplied BGRA pixels of a rectangle (transparent black outside the canvas). Waits for
    /// the GPU.
    pub fn read(&mut self, x: i32, y: i32, w: u32, h: u32) -> Result<Vec<u8>> {
        let mut out = vec![0u8; w as usize * h as usize * 4];
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x as i64 + w as i64).min(self.width as i64) as i32;
        let y1 = (y as i64 + h as i64).min(self.height as i64) as i32;
        if x1 <= x0 || y1 <= y0 {
            return Ok(out);
        }
        let staged = std::mem::replace(&mut self.staged, false);
        let r = D2D_RECT_U {
            left: x0 as u32,
            top: y0 as u32,
            right: x1 as u32,
            bottom: y1 as u32,
        };
        if !staged {
            self.stage(r)?;
        }
        let staging = self.staging.as_ref().unwrap();
        let ch = (y1 - y0) as u32;
        let cw = (x1 - x0) as u32;
        // SAFETY: the copied rectangle is within both bitmaps; the mapping is read within its
        // pitch for the copied rows and unmapped before returning.
        unsafe {
            let mapped = staging.Map(D2D1_MAP_OPTIONS_READ)?;
            for row in 0..ch as usize {
                let sy = y0 as usize + row;
                let src = std::slice::from_raw_parts(
                    mapped
                        .bits
                        .add(sy * mapped.pitch as usize + x0 as usize * 4),
                    cw as usize * 4,
                );
                let dy = (sy as i64 - y as i64) as usize;
                let dx = (x0 - x) as usize;
                let start = (dy * w as usize + dx) * 4;
                out[start..start + cw as usize * 4].copy_from_slice(src);
            }
            staging.Unmap()?;
        }
        Ok(out)
    }

    /// Submits the drawing and queues a copy of the rectangle `r` (within the bitmap) into the
    /// staging bitmap, without waiting for the GPU.
    fn stage(&mut self, r: D2D_RECT_U) -> Result<()> {
        self.end()?;
        if self.staging.is_none() {
            // SAFETY: plain bitmap creation.
            self.staging = Some(unsafe {
                self.ctx.CreateBitmap(
                    D2D_SIZE_U {
                        width: self.width,
                        height: self.height,
                    },
                    None,
                    0,
                    &bitmap_props(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW),
                )?
            });
        }
        let staging = self.staging.as_ref().unwrap();
        // SAFETY: the rectangle is within both bitmaps (same size); the target is not in drawing.
        unsafe {
            staging.CopyFromBitmap(
                Some(&D2D_POINT_2U {
                    x: r.left,
                    y: r.top,
                }),
                &self.target,
                Some(&r),
            )
        }
    }

    /// The first half of a [`GpuCanvas::read`] of the whole bitmap: the drawing so far is submitted
    /// and its copy queued and sent to the GPU, without waiting for it, so that the GPU works while
    /// the caller does and the reads of several canvases wait once. The next `read` of the whole bitmap, with nothing drawn or written in between, waits for
    /// that copy; anything else takes its own.
    pub fn start_read_all(&mut self) -> Result<()> {
        self.staged = false;
        self.stage(D2D_RECT_U {
            left: 0,
            top: 0,
            right: self.width,
            bottom: self.height,
        })?;
        // (Direct3D keeps commands until its buffer fills or something waits: sent now, the GPU
        // works while the caller does)
        // SAFETY: the thread's own device's immediate context.
        unsafe { self.dev.d3d.GetImmediateContext()?.Flush() };
        self.staged = true;
        Ok(())
    }

    /// Writes premultiplied BGRA pixels (`w * 4` bytes a row) at (x, y), clipped to the canvas,
    /// ignoring the drawing state.
    pub fn write(&mut self, x: i32, y: i32, w: u32, h: u32, bgra: &[u8]) -> Result<()> {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x as i64 + w as i64).min(self.width as i64) as i32;
        let y1 = (y as i64 + h as i64).min(self.height as i64) as i32;
        if x1 <= x0 || y1 <= y0 || bgra.len() < (w as usize * h as usize * 4) {
            return Ok(());
        }
        self.end()?;
        self.staged = false;
        let offset = ((y0 - y) as usize * w as usize + (x0 - x) as usize) * 4;
        // SAFETY: the source pointer starts at the first written pixel and each of the copied rows
        // (pitch `w * 4`) lies within `bgra`; the target is not in drawing.
        unsafe {
            self.target.CopyFromMemory(
                Some(&D2D_RECT_U {
                    left: x0 as u32,
                    top: y0 as u32,
                    right: x1 as u32,
                    bottom: y1 as u32,
                }),
                bgra.as_ptr().add(offset).cast(),
                w * 4,
            )
        }
    }

    /// Draws the pending sprites.
    fn flush_sprites(&mut self) {
        let s = &mut self.sprites;
        if s.dst.is_empty() {
            return;
        }
        if let Some(atlas) = &s.atlas {
            // SAFETY: the three arrays have one entry per sprite, with the strides given; the batch
            // is drawn and cleared while the context is in drawing (sprites are only added then).
            unsafe {
                let n = s.dst.len() as u32;
                if s.batch
                    .AddSprites(
                        n,
                        s.dst.as_ptr(),
                        Some(s.src.as_ptr()),
                        Some(s.colors.as_ptr()),
                        None,
                        std::mem::size_of::<D2D_RECT_F>() as u32,
                        std::mem::size_of::<D2D_RECT_U>() as u32,
                        std::mem::size_of::<D2D1_COLOR_F>() as u32,
                        0,
                    )
                    .is_ok()
                {
                    self.ctx.SetTransform(&Matrix3x2::identity());
                    self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
                    self.ctx3.DrawSpriteBatch(
                        &s.batch,
                        0,
                        n,
                        atlas,
                        D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                        D2D1_SPRITE_OPTIONS_NONE,
                    );
                    self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                }
                s.batch.Clear();
            }
        }
        s.dst.clear();
        s.src.clear();
        s.colors.clear();
    }

    /// Queues a circle (fill when `width` is 0, else a ring) as a sprite.
    fn sprite(
        &mut self,
        center: Point,
        radius: f32,
        width: f32,
        color: D2D1_COLOR_F,
    ) -> Result<()> {
        if radius.is_nan() || radius <= 0.0 || !(center.x.abs() < 1e7 && center.y.abs() < 1e7) {
            return Ok(());
        }
        let (generation, cell) = self.dev.atlas_cell(radius, width)?;
        if self.sprites.generation != generation {
            // Sprites queued so far refer to the previous atlas.
            self.flush_sprites();
            self.sprites.atlas = self.dev.atlas.borrow().bitmap.clone();
            self.sprites.generation = generation;
        }
        let Cell {
            x: cx,
            y: cy,
            size,
            offset,
        } = cell;
        // The nearest eighth of a pixel: whole pixels and the phase.
        let quantize = |v: f32| -> (f32, u32) {
            let t = (v - offset) * PHASES as f32 + 0.5;
            let mut q = t as i64;
            if (q as f32) > t {
                q -= 1;
            }
            (
                q.div_euclid(PHASES as i64) as f32,
                q.rem_euclid(PHASES as i64) as u32,
            )
        };
        let (left, px) = quantize(center.x);
        let (top, py) = quantize(center.y);
        let sx = cx + px * size;
        let sy = cy + py * size;
        let s = &mut self.sprites;
        s.dst.push(D2D_RECT_F {
            left,
            top,
            right: left + size as f32,
            bottom: top + size as f32,
        });
        s.src.push(D2D_RECT_U {
            left: sx,
            top: sy,
            right: sx + size,
            bottom: sy + size,
        });
        s.colors.push(color);
        if s.dst.len() >= 1 << 16 {
            self.flush_sprites();
        }
        Ok(())
    }

    /// Submits pending drawing (the canvas's pixels are complete on the GPU afterwards).
    pub fn flush(&mut self) -> Result<()> {
        self.end()
    }
}

impl Drop for GpuCanvas {
    fn drop(&mut self) {
        if self.end().is_err() {
            return;
        }
        let mut pool = self.dev.pool.borrow_mut();
        if pool.len() >= POOL {
            pool.remove(0);
        }
        pool.push(Parts {
            width: self.width,
            height: self.height,
            ctx: self.ctx.clone(),
            target: self.target.clone(),
            brush: self.brush.clone(),
            batch: self.sprites.batch.clone(),
            staging: self.staging.take(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test runs on a thread of its own, which has no device yet.

    #[test]
    fn the_first_canvas_gets_the_device_prepare_made() {
        assert!(!CanvasDevice::is_prepared(Driver::Warp));
        CanvasDevice::prepare(Driver::Warp);
        assert!(CanvasDevice::is_prepared(Driver::Warp));
        let device = CanvasDevice::for_thread(Driver::Warp).unwrap();
        assert!(device.was_prepared());
        // The thread has its device now: preparing again makes nothing.
        CanvasDevice::prepare(Driver::Warp);
        assert!(PENDING.with(|p| p.borrow().is_empty()));
        let again = CanvasDevice::for_thread(Driver::Warp).unwrap();
        assert!(Rc::ptr_eq(&device, &again));
        assert!(GpuCanvas::new(16, 16, Driver::Warp).is_ok());
    }

    #[test]
    fn a_read_started_early_returns_the_bitmap_as_it_is_when_read() {
        let mut c = GpuCanvas::new(4, 3, Driver::Warp).unwrap();
        let red = [0u8, 0, 255, 255].repeat(12);
        let blue = [255u8, 0, 0, 255].repeat(12);
        c.write(0, 0, 4, 3, &red).unwrap();
        c.start_read_all().unwrap();
        assert_eq!(c.read(0, 0, 4, 3).unwrap(), red);
        // (a part of the bitmap, from the copy of the whole)
        c.start_read_all().unwrap();
        assert_eq!(c.read(1, 1, 2, 2).unwrap(), [0u8, 0, 255, 255].repeat(4));
        // Written after the copy was queued: the read takes a copy of its own.
        c.start_read_all().unwrap();
        c.write(0, 0, 4, 3, &blue).unwrap();
        assert_eq!(c.read(0, 0, 4, 3).unwrap(), blue);
        // Cleared after the copy was queued.
        c.start_read_all().unwrap();
        let all = Rect {
            x: 0.0,
            y: 0.0,
            width: 4.0,
            height: 3.0,
        };
        c.clear(FillShape::Rect(all), None).unwrap();
        assert_eq!(c.read(0, 0, 4, 3).unwrap(), vec![0u8; 48]);
    }

    #[test]
    fn without_prepare_the_first_canvas_makes_the_device() {
        let device = CanvasDevice::for_thread(Driver::Warp).unwrap();
        assert!(!device.was_prepared());
        assert!(CanvasDevice::is_prepared(Driver::Warp));
    }
}
