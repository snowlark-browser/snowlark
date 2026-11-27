// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A page thread's GPU canvases drawn on a thread of their own. [`RemoteCanvas`] has
//! [`GpuCanvas`]'s operations; it records each one (owned: device-space shapes, paints and the
//! state that applies) and sends them in batches to the page thread's canvas thread, which replays
//! them into a [`GpuCanvas`] on its [`CanvasDevice`]. The page thread waits only where it needs the
//! pixels: [`RemoteCanvas::read`] (`getImageData`, `toDataURL`, the frame the page shows) replays
//! everything recorded before it first.
//!
//! So script that draws runs while the canvas thread does Direct2D's and the driver's work (path
//! geometry, stroke widening, batching, command submission), rather than doing it itself. The
//! recording is also the shape the operations take to cross to another process (a GPU process
//! drawing canvases). The canvas thread is made with the first canvas (or by [`prepare`]) and makes the device
//! itself, so the page thread never waits for that, except once for whether it could be made (a
//! canvas falls back to the CPU when it cannot).
//!
//! Operations are replayed in the order they were recorded, every canvas of the page thread on one
//! thread and one device, so a canvas drawn into another needs no copy through memory, as before.
//! Errors of an operation are dropped, as the callers dropped them when the operations ran here.

use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender};
use std::sync::{Arc, Weak};

use windows::core::Result;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};

use crate::image::DecodedImage;

use crate::canvas::{
    CanvasClip, CanvasDevice, CanvasImage, CanvasPaint, CompositeOp, DrawParams, FillShape,
    GpuCanvas, GradientStop, LineCap, LineJoin, PathData, Rgba, Shadow, StrokeParams,
};
use crate::filter::FilterOp;
use crate::geom::{Matrix, Point, Rect};
use crate::gpu::Driver;
use crate::text::{ShapedText, TextEngine};

/// Operations recorded before a batch goes to the canvas thread, unless something waits first.
const BATCH: usize = 64;

/// The most pixel bytes of decoded images a page thread keeps uploaded on its canvas thread between
/// `drawImage` calls ([`RemoteCanvas::draw_frame`]); past it the images drawn longest ago go.
const FRAME_IMAGES_BYTES: usize = 256 << 20;

/// Called on the canvas thread once a read's pixels are sent ([`RemoteCanvas::read_later`]): the
/// page thread's wake-up.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// [`DrawParams`], owned.
struct Params {
    transform: Matrix,
    alpha: f32,
    op: CompositeOp,
    clip: Option<Arc<CanvasClip>>,
    shadow: Option<Shadow>,
    filter: Option<Arc<[FilterOp]>>,
    smoothing: bool,
}

impl Params {
    fn of(p: &DrawParams) -> Params {
        Params {
            transform: p.transform,
            alpha: p.alpha,
            op: p.op,
            clip: p.clip.cloned(),
            shadow: p.shadow,
            filter: p.filter.cloned(),
            smoothing: p.smoothing,
        }
    }

    fn get(&self) -> DrawParams<'_> {
        DrawParams {
            transform: self.transform,
            alpha: self.alpha,
            op: self.op,
            clip: self.clip.as_ref(),
            shadow: self.shadow,
            filter: self.filter.as_ref(),
            smoothing: self.smoothing,
        }
    }
}

/// [`StrokeParams`], owned.
struct Stroke {
    width: f32,
    cap: LineCap,
    join: LineJoin,
    miter_limit: f32,
    dash: Vec<f32>,
    dash_offset: f32,
}

impl Stroke {
    fn of(s: &StrokeParams) -> Stroke {
        Stroke {
            width: s.width,
            cap: s.cap,
            join: s.join,
            miter_limit: s.miter_limit,
            dash: s.dash.to_vec(),
            dash_offset: s.dash_offset,
        }
    }

    fn get(&self) -> StrokeParams<'_> {
        StrokeParams {
            width: self.width,
            cap: self.cap,
            join: self.join,
            miter_limit: self.miter_limit,
            dash: &self.dash,
            dash_offset: self.dash_offset,
        }
    }
}

/// [`FillShape`], owned.
enum Shape {
    Rect(Rect),
    Circle(Point, f32),
    Path(PathData, bool),
}

impl Shape {
    fn of(s: &FillShape) -> Shape {
        match *s {
            FillShape::Rect(r) => Shape::Rect(r),
            FillShape::Circle(c, r) => Shape::Circle(c, r),
            FillShape::Path(p, evenodd) => Shape::Path(p.clone(), evenodd),
        }
    }

    fn get(&self) -> FillShape<'_> {
        match self {
            Shape::Rect(r) => FillShape::Rect(*r),
            Shape::Circle(c, r) => FillShape::Circle(*c, *r),
            Shape::Path(p, evenodd) => FillShape::Path(p, *evenodd),
        }
    }
}

/// [`CanvasPaint`], owned, its images by id.
enum Paint {
    Solid(Rgba),
    Linear {
        start: Point,
        end: Point,
        stops: Vec<GradientStop>,
    },
    Radial {
        c0: Point,
        r0: f32,
        c1: Point,
        r1: f32,
        stops: Vec<GradientStop>,
    },
    Pattern {
        image: u64,
        repeat_x: bool,
        repeat_y: bool,
    },
    DeviceImage(u64),
}

impl Paint {
    fn of(p: &CanvasPaint<RemoteImage>) -> Paint {
        match *p {
            CanvasPaint::Solid(c) => Paint::Solid(c),
            CanvasPaint::Linear { start, end, stops } => Paint::Linear {
                start,
                end,
                stops: stops.to_vec(),
            },
            CanvasPaint::Radial {
                c0,
                r0,
                c1,
                r1,
                stops,
            } => Paint::Radial {
                c0,
                r0,
                c1,
                r1,
                stops: stops.to_vec(),
            },
            CanvasPaint::Pattern {
                image,
                repeat_x,
                repeat_y,
            } => Paint::Pattern {
                image: image.id(),
                repeat_x,
                repeat_y,
            },
            CanvasPaint::DeviceImage(image) => Paint::DeviceImage(image.id()),
        }
    }

    /// The paint with its images, `None` when one of them is not there.
    fn get<'a>(&'a self, images: &'a HashMap<u64, CanvasImage>) -> Option<CanvasPaint<'a>> {
        Some(match self {
            Paint::Solid(c) => CanvasPaint::Solid(*c),
            Paint::Linear { start, end, stops } => CanvasPaint::Linear {
                start: *start,
                end: *end,
                stops,
            },
            Paint::Radial {
                c0,
                r0,
                c1,
                r1,
                stops,
            } => CanvasPaint::Radial {
                c0: *c0,
                r0: *r0,
                c1: *c1,
                r1: *r1,
                stops,
            },
            Paint::Pattern {
                image,
                repeat_x,
                repeat_y,
            } => CanvasPaint::Pattern {
                image: images.get(image)?,
                repeat_x: *repeat_x,
                repeat_y: *repeat_y,
            },
            Paint::DeviceImage(image) => CanvasPaint::DeviceImage(images.get(image)?),
        })
    }
}

/// One recorded operation, of the canvas or image with the id.
enum Op {
    New {
        id: u64,
        width: u32,
        height: u32,
    },
    Drop(u64),
    Fill {
        id: u64,
        shape: Shape,
        paint: Paint,
        p: Params,
    },
    Stroke {
        id: u64,
        path: PathData,
        stroke: Stroke,
        paint: Paint,
        p: Params,
    },
    StrokeCircle {
        id: u64,
        center: Point,
        radius: f32,
        width: f32,
        color: Rgba,
        p: Params,
    },
    Clear {
        id: u64,
        shape: Shape,
        clip: Option<Arc<CanvasClip>>,
    },
    DrawImage {
        id: u64,
        image: u64,
        src: Rect,
        dst: Rect,
        p: Params,
    },
    /// [`Op::DrawImage`] of a decoded image's default frame, which is the image `image`: uploaded
    /// from `frame` by the first of these that finds it missing.
    DrawFrame {
        id: u64,
        image: u64,
        frame: Arc<DecodedImage>,
        src: Rect,
        dst: Rect,
        p: Params,
    },
    DrawText {
        id: u64,
        text: Arc<TextEngine>,
        shaped: Arc<ShapedText>,
        origin: Point,
        paint: Paint,
        stroke: Option<Stroke>,
        p: Params,
    },
    DrawDeviceImage {
        id: u64,
        image: u64,
        p: Params,
    },
    Image {
        id: u64,
        image: u64,
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    },
    DropImage(u64),
    /// The canvas's pixels now, as the image `image` (a copy on the GPU).
    Snapshot {
        id: u64,
        image: u64,
    },
    Write {
        id: u64,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        bgra: Vec<u8>,
    },
    StartReadAll(u64),
    /// The pixels, or `None` when the canvas or the read failed; `then` is called once they are
    /// sent.
    Read {
        id: u64,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        reply: Sender<Option<Vec<u8>>>,
        then: Option<Wake>,
    },
    /// Answered once everything before it is replayed and submitted.
    Flush {
        id: u64,
        reply: Sender<()>,
    },
}

/// The canvas thread's side: the canvases and images it holds, by id.
struct Replay {
    canvases: HashMap<u64, GpuCanvas>,
    images: HashMap<u64, CanvasImage>,
    driver: Driver,
}

impl Replay {
    fn apply(&mut self, op: Op) {
        let images = &self.images;
        let canvases = &mut self.canvases;
        // (an operation of a canvas whose bitmap could not be made does nothing, as one of an
        // unmade bitmap; errors are dropped, as the callers dropped them)
        match op {
            Op::New { id, width, height } => {
                if let Ok(g) = GpuCanvas::new(width, height, self.driver) {
                    canvases.insert(id, g);
                }
            }
            Op::Drop(id) => {
                canvases.remove(&id);
            }
            Op::Fill {
                id,
                shape,
                paint,
                p,
            } => {
                if let (Some(g), Some(paint)) = (canvases.get_mut(&id), paint.get(images)) {
                    let _ = g.fill(shape.get(), &paint, &p.get());
                }
            }
            Op::Stroke {
                id,
                path,
                stroke,
                paint,
                p,
            } => {
                if let (Some(g), Some(paint)) = (canvases.get_mut(&id), paint.get(images)) {
                    let _ = g.stroke(&path, &stroke.get(), &paint, &p.get());
                }
            }
            Op::StrokeCircle {
                id,
                center,
                radius,
                width,
                color,
                p,
            } => {
                if let Some(g) = canvases.get_mut(&id) {
                    let _ = g.stroke_circle(center, radius, width, color, &p.get());
                }
            }
            Op::Clear { id, shape, clip } => {
                if let Some(g) = canvases.get_mut(&id) {
                    let _ = g.clear(shape.get(), clip.as_ref());
                }
            }
            Op::DrawImage {
                id,
                image,
                src,
                dst,
                p,
            } => {
                if let (Some(g), Some(image)) = (canvases.get_mut(&id), images.get(&image)) {
                    let _ = g.draw_image(image, src, dst, &p.get());
                }
            }
            Op::DrawFrame {
                id,
                image,
                frame,
                src,
                dst,
                p,
            } => {
                if let Some(g) = canvases.get_mut(&id) {
                    if let Entry::Vacant(slot) = self.images.entry(image) {
                        let f = frame.default_frame();
                        if let Ok(made) = g.image_from_bgra(f.width, f.height, &f.pixels) {
                            slot.insert(made);
                        }
                    }
                    if let Some(image) = self.images.get(&image) {
                        let _ = g.draw_image(image, src, dst, &p.get());
                    }
                }
            }
            Op::DrawText {
                id,
                text,
                shaped,
                origin,
                paint,
                stroke,
                p,
            } => {
                if let (Some(g), Some(paint)) = (canvases.get_mut(&id), paint.get(images)) {
                    let stroke = stroke.as_ref().map(Stroke::get);
                    let _ = g.draw_text(&text, &shaped, origin, &paint, stroke.as_ref(), &p.get());
                }
            }
            Op::DrawDeviceImage { id, image, p } => {
                if let (Some(g), Some(image)) = (canvases.get_mut(&id), images.get(&image)) {
                    let _ = g.draw_device_image(image, &p.get());
                }
            }
            Op::Image {
                id,
                image,
                width,
                height,
                bgra,
            } => {
                // (a bitmap of the device every canvas of the thread shares)
                if let Some(g) = canvases.get(&id) {
                    if let Ok(made) = g.image_from_bgra(width, height, &bgra) {
                        self.images.insert(image, made);
                    }
                }
            }
            Op::DropImage(image) => {
                self.images.remove(&image);
            }
            Op::Snapshot { id, image } => {
                if let Some(Ok(made)) = canvases.get_mut(&id).map(|g| g.snapshot()) {
                    self.images.insert(image, made);
                }
            }
            Op::Write {
                id,
                x,
                y,
                w,
                h,
                bgra,
            } => {
                if let Some(g) = canvases.get_mut(&id) {
                    let _ = g.write(x, y, w, h, &bgra);
                }
            }
            Op::StartReadAll(id) => {
                if let Some(g) = canvases.get_mut(&id) {
                    let _ = g.start_read_all();
                }
            }
            Op::Read {
                id,
                x,
                y,
                w,
                h,
                reply,
                then,
            } => {
                let pixels = canvases.get_mut(&id).and_then(|g| g.read(x, y, w, h).ok());
                let _ = reply.send(pixels);
                if let Some(then) = then {
                    then();
                }
            }
            Op::Flush { id, reply } => {
                if let Some(g) = canvases.get_mut(&id) {
                    let _ = g.flush();
                }
                let _ = reply.send(());
            }
        }
    }
}

/// The canvas thread: makes the device, says whether it could, then replays batches until the
/// page thread's link is gone.
fn run(driver: Driver, batches: Receiver<Vec<Op>>, ready: std::sync::mpsc::SyncSender<bool>) {
    let made = CanvasDevice::make_for_thread(driver).is_ok();
    let _ = ready.send(made);
    let mut replay = Replay {
        canvases: HashMap::new(),
        images: HashMap::new(),
        driver,
    };
    while let Ok(batch) = batches.recv() {
        for op in batch {
            replay.apply(op);
        }
    }
}

/// The decoded images whose default frames are images of the canvas thread, by the address of the
/// image (which each entry's weak reference keeps from being another image's).
struct FrameImages {
    entries: HashMap<usize, FrameImage>,
    /// The pixel bytes of the entries, and the most they may be ([`FRAME_IMAGES_BYTES`]).
    bytes: usize,
    budget: usize,
    /// Counts the draws, for [`FrameImage::used`].
    clock: u64,
}

struct FrameImage {
    owner: Weak<DecodedImage>,
    /// The image's id on the canvas thread.
    id: u64,
    bytes: usize,
    /// [`FrameImages::clock`] at the last draw.
    used: u64,
}

impl FrameImages {
    /// Removes the entries `gone` says, their ids into `dropped`.
    fn release(&mut self, dropped: &mut Vec<u64>, gone: impl Fn(&FrameImage) -> bool) {
        let mut freed = 0;
        self.entries.retain(|_, e| {
            let go = gone(e);
            if go {
                dropped.push(e.id);
                freed += e.bytes;
            }
            !go
        });
        self.bytes -= freed;
    }
}

/// A page thread's connection to its canvas thread for one driver.
struct Link {
    driver: Driver,
    to_thread: Sender<Vec<Op>>,
    pending: RefCell<Vec<Op>>,
    frames: RefCell<FrameImages>,
    next_id: Cell<u64>,
    ready: RefCell<Option<Receiver<bool>>>,
    made: Cell<Option<bool>>,
}

thread_local! {
    /// The calling thread's links, made by its first canvas or [`prepare`] (per driver).
    static LINKS: RefCell<Vec<Rc<Link>>> = const { RefCell::new(Vec::new()) };
}

impl Link {
    fn existing(driver: Driver) -> Option<Rc<Link>> {
        LINKS.with(|l| l.borrow().iter().find(|l| l.driver == driver).cloned())
    }

    /// The calling thread's link for `driver`, starting its canvas thread the first time.
    fn for_thread(driver: Driver) -> Option<Rc<Link>> {
        if let Some(l) = Self::existing(driver) {
            return Some(l);
        }
        let (to_thread, batches) = channel();
        let (ready_tx, ready) = sync_channel(1);
        std::thread::Builder::new()
            .name("canvas".into())
            .spawn(move || run(driver, batches, ready_tx))
            .ok()?;
        let link = Rc::new(Link {
            driver,
            to_thread,
            pending: RefCell::new(Vec::new()),
            frames: RefCell::new(FrameImages {
                entries: HashMap::new(),
                bytes: 0,
                budget: FRAME_IMAGES_BYTES,
                clock: 0,
            }),
            next_id: Cell::new(1),
            ready: RefCell::new(Some(ready)),
            made: Cell::new(None),
        });
        LINKS.with(|l| l.borrow_mut().push(link.clone()));
        Some(link)
    }

    /// Whether the canvas thread made its device (waiting for it to say, the first time).
    fn made(&self) -> bool {
        if let Some(made) = self.made.get() {
            return made;
        }
        let made = self
            .ready
            .borrow_mut()
            .take()
            .and_then(|r| r.recv().ok())
            .unwrap_or(false);
        self.made.set(Some(made));
        made
    }

    fn id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    fn push(&self, op: Op) {
        let full = {
            let mut pending = self.pending.borrow_mut();
            pending.push(op);
            pending.len() >= BATCH
        };
        if full {
            self.submit();
        }
    }

    /// The id of `image`'s default frame as an image of the canvas thread: the one it was given
    /// when first drawn, kept while the image lives and the kept images fit [`FRAME_IMAGES_BYTES`].
    /// A new one drops the images of freed sources and, past the budget, those drawn longest ago.
    fn frame_image(&self, image: &Arc<DecodedImage>) -> u64 {
        let key = Arc::as_ptr(image) as usize;
        let mut dropped = Vec::new();
        let id = {
            let mut frames = self.frames.borrow_mut();
            frames.clock += 1;
            let now = frames.clock;
            if let Some(e) = frames.entries.get_mut(&key) {
                e.used = now;
                return e.id;
            }
            frames.release(&mut dropped, |e| e.owner.strong_count() == 0);
            let bytes = image.default_frame().pixels.len();
            while frames.bytes + bytes > frames.budget {
                let Some(oldest) = frames.entries.values().map(|e| e.used).min() else {
                    break;
                };
                frames.release(&mut dropped, |e| e.used == oldest);
            }
            let id = self.id();
            frames.bytes += bytes;
            frames.entries.insert(
                key,
                FrameImage {
                    owner: Arc::downgrade(image),
                    id,
                    bytes,
                    used: now,
                },
            );
            id
        };
        for old in dropped {
            self.push(Op::DropImage(old));
        }
        id
    }

    /// Drops the kept images whose sources were freed.
    fn release_freed_frames(&self) {
        let mut dropped = Vec::new();
        self.frames
            .borrow_mut()
            .release(&mut dropped, |e| e.owner.strong_count() == 0);
        for old in dropped {
            self.push(Op::DropImage(old));
        }
    }

    /// Sends what is recorded to the canvas thread.
    fn submit(&self) {
        let batch = std::mem::take(&mut *self.pending.borrow_mut());
        if !batch.is_empty() {
            let _ = self.to_thread.send(batch);
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.submit();
    }
}

/// Whether canvases can draw on a GPU device here.
pub const AVAILABLE: bool = true;

/// Starts this thread's canvas thread for `driver` (which makes the device), unless it has one: the
/// first canvas then finds the device made, or being made off this thread.
pub fn prepare(driver: Driver) {
    let _ = Link::for_thread(driver);
}

/// Whether this thread's canvas thread for `driver` is started.
pub fn is_prepared(driver: Driver) -> bool {
    Link::existing(driver).is_some()
}

/// How many decoded images this thread keeps uploaded on its canvas thread for `driver`
/// ([`RemoteCanvas::draw_frame`]), and their pixel bytes.
pub fn kept_images(driver: Driver) -> (usize, usize) {
    Link::existing(driver).map_or((0, 0), |l| {
        let frames = l.frames.borrow();
        (frames.entries.len(), frames.bytes)
    })
}

/// An image on the canvas thread (`drawImage` sources, patterns), freed there when the last clone
/// goes.
#[derive(Clone)]
pub struct RemoteImage {
    handle: Rc<ImageHandle>,
    pub width: u32,
    pub height: u32,
}

struct ImageHandle {
    id: u64,
    link: Rc<Link>,
}

impl Drop for ImageHandle {
    fn drop(&mut self) {
        self.link.push(Op::DropImage(self.id));
    }
}

impl RemoteImage {
    fn id(&self) -> u64 {
        self.handle.id
    }
}

/// The pixels of a [`RemoteCanvas::read_later`], premultiplied BGRA, `width` × `height`.
pub struct PendingRead {
    answer: Receiver<Option<Vec<u8>>>,
    pub width: u32,
    pub height: u32,
}

impl PendingRead {
    /// The pixels if the canvas thread sent them: `None` while it has not, `Some(Err)` when the read
    /// failed (or the canvas thread is gone).
    pub fn try_take(&self) -> Option<Result<Vec<u8>>> {
        use std::sync::mpsc::TryRecvError;
        match self.answer.try_recv() {
            Ok(pixels) => Some(pixels.ok_or_else(|| E_FAIL.into())),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(E_FAIL.into())),
        }
    }

    /// The pixels, waiting for them.
    pub fn wait(self) -> Result<Vec<u8>> {
        self.answer
            .recv()
            .ok()
            .flatten()
            .ok_or_else(|| windows::core::Error::from(E_FAIL))
    }
}

/// A canvas bitmap drawn on the page thread's canvas thread: [`GpuCanvas`]'s operations, recorded.
pub struct RemoteCanvas {
    id: u64,
    width: u32,
    height: u32,
    link: Rc<Link>,
}

impl RemoteCanvas {
    /// A transparent black bitmap of `width`×`height`, an error when the canvas thread could not make
    /// its device.
    pub fn new(width: u32, height: u32, driver: Driver) -> Result<RemoteCanvas> {
        let link = Link::for_thread(driver).ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        if !link.made() {
            return Err(E_FAIL.into());
        }
        let id = link.id();
        link.push(Op::New { id, width, height });
        Ok(RemoteCanvas {
            id,
            width,
            height,
            link,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// [`GpuCanvas::fill`].
    pub fn fill(
        &mut self,
        shape: FillShape,
        paint: &CanvasPaint<RemoteImage>,
        p: &DrawParams,
    ) -> Result<()> {
        self.link.push(Op::Fill {
            id: self.id,
            shape: Shape::of(&shape),
            paint: Paint::of(paint),
            p: Params::of(p),
        });
        Ok(())
    }

    /// [`GpuCanvas::stroke`].
    pub fn stroke(
        &mut self,
        path: &PathData,
        stroke: &StrokeParams,
        paint: &CanvasPaint<RemoteImage>,
        p: &DrawParams,
    ) -> Result<()> {
        self.link.push(Op::Stroke {
            id: self.id,
            path: path.clone(),
            stroke: Stroke::of(stroke),
            paint: Paint::of(paint),
            p: Params::of(p),
        });
        Ok(())
    }

    /// [`GpuCanvas::stroke_circle`].
    pub fn stroke_circle(
        &mut self,
        center: Point,
        radius: f32,
        width: f32,
        color: Rgba,
        p: &DrawParams,
    ) -> Result<()> {
        self.link.push(Op::StrokeCircle {
            id: self.id,
            center,
            radius,
            width,
            color,
            p: Params::of(p),
        });
        Ok(())
    }

    /// [`GpuCanvas::clear`].
    pub fn clear(&mut self, shape: FillShape, clip: Option<&Arc<CanvasClip>>) -> Result<()> {
        self.link.push(Op::Clear {
            id: self.id,
            shape: Shape::of(&shape),
            clip: clip.cloned(),
        });
        Ok(())
    }

    /// [`GpuCanvas::draw_image`].
    pub fn draw_image(
        &mut self,
        image: &RemoteImage,
        src: Rect,
        dst: Rect,
        p: &DrawParams,
    ) -> Result<()> {
        self.link.push(Op::DrawImage {
            id: self.id,
            image: image.id(),
            src,
            dst,
            p: Params::of(p),
        });
        Ok(())
    }

    /// [`GpuCanvas::draw_image`] of a decoded image's default frame (`src` in its pixels). The
    /// frame is uploaded when first drawn and the image on the canvas thread is kept for the draws
    /// after it, of any canvas of this thread: while the decoded image lives, and the images kept
    /// so take no more than a budget of pixel bytes (past it, those drawn longest ago go).
    pub fn draw_frame(
        &mut self,
        image: &Arc<DecodedImage>,
        src: Rect,
        dst: Rect,
        p: &DrawParams,
    ) -> Result<()> {
        let frame = image.default_frame();
        let (w, h) = (frame.width.max(1) as usize, frame.height.max(1) as usize);
        if frame.pixels.len() < w * h * 4 {
            return Err(E_INVALIDARG.into());
        }
        let id = self.link.frame_image(image);
        self.link.push(Op::DrawFrame {
            id: self.id,
            image: id,
            frame: image.clone(),
            src,
            dst,
            p: Params::of(p),
        });
        Ok(())
    }

    /// [`GpuCanvas::draw_text`].
    pub fn draw_text(
        &mut self,
        text: &Arc<TextEngine>,
        shaped: &Arc<ShapedText>,
        origin: Point,
        paint: &CanvasPaint<RemoteImage>,
        stroke: Option<&StrokeParams>,
        p: &DrawParams,
    ) -> Result<()> {
        if shaped.glyphs.is_empty() {
            return Ok(());
        }
        self.link.push(Op::DrawText {
            id: self.id,
            text: text.clone(),
            shaped: shaped.clone(),
            origin,
            paint: Paint::of(paint),
            stroke: stroke.map(Stroke::of),
            p: Params::of(p),
        });
        Ok(())
    }

    /// [`GpuCanvas::draw_device_image`].
    pub fn draw_device_image(&mut self, image: &RemoteImage, p: &DrawParams) -> Result<()> {
        self.link.push(Op::DrawDeviceImage {
            id: self.id,
            image: image.id(),
            p: Params::of(p),
        });
        Ok(())
    }

    /// [`GpuCanvas::image_from_bgra`]: the upload starts at once.
    pub fn image_from_bgra(&self, width: u32, height: u32, bgra: &[u8]) -> Result<RemoteImage> {
        let (w, h) = (width.max(1), height.max(1));
        if bgra.len() < (w * h * 4) as usize {
            return Err(E_INVALIDARG.into());
        }
        let id = self.link.id();
        self.link.push(Op::Image {
            id: self.id,
            image: id,
            width,
            height,
            bgra: bgra.to_vec(),
        });
        self.link.submit();
        Ok(RemoteImage {
            handle: Rc::new(ImageHandle {
                id,
                link: self.link.clone(),
            }),
            width: w,
            height: h,
        })
    }

    /// [`GpuCanvas::snapshot`]: the bitmap as it is after what is recorded so far, as an image of
    /// the canvas thread (a copy on the GPU; nothing waits).
    pub fn snapshot(&mut self) -> RemoteImage {
        let id = self.link.id();
        self.link.push(Op::Snapshot {
            id: self.id,
            image: id,
        });
        RemoteImage {
            handle: Rc::new(ImageHandle {
                id,
                link: self.link.clone(),
            }),
            width: self.width.max(1),
            height: self.height.max(1),
        }
    }

    /// Whether `image` is on this canvas's thread and device, so this canvas can draw it.
    pub fn can_draw(&self, image: &RemoteImage) -> bool {
        Rc::ptr_eq(&image.handle.link, &self.link)
    }

    /// [`GpuCanvas::read`]: waits for everything recorded before it, then for the GPU.
    pub fn read(&mut self, x: i32, y: i32, w: u32, h: u32) -> Result<Vec<u8>> {
        let (reply, answer) = channel();
        self.link.push(Op::Read {
            id: self.id,
            x,
            y,
            w,
            h,
            reply,
            then: None,
        });
        self.link.submit();
        answer
            .recv()
            .ok()
            .flatten()
            .ok_or_else(|| windows::core::Error::from(E_FAIL))
    }

    /// The whole bitmap as it is after what is recorded so far, read without waiting for it: the
    /// canvas thread reads it once it has drawn everything before, and calls `then` once the
    /// pixels are in the [`PendingRead`].
    pub fn read_later(&mut self, then: Option<Wake>) -> PendingRead {
        let (reply, answer) = channel();
        let (w, h) = (self.width.max(1), self.height.max(1));
        self.link.push(Op::Read {
            id: self.id,
            x: 0,
            y: 0,
            w,
            h,
            reply,
            then,
        });
        // (once a frame: a source freed since the last one gives its image back)
        self.link.release_freed_frames();
        self.link.submit();
        PendingRead {
            answer,
            width: w,
            height: h,
        }
    }

    /// [`GpuCanvas::start_read_all`]: what is recorded goes to the canvas thread now.
    pub fn start_read_all(&mut self) -> Result<()> {
        self.link.push(Op::StartReadAll(self.id));
        self.link.submit();
        Ok(())
    }

    /// [`GpuCanvas::write`].
    pub fn write(&mut self, x: i32, y: i32, w: u32, h: u32, bgra: &[u8]) -> Result<()> {
        self.link.push(Op::Write {
            id: self.id,
            x,
            y,
            w,
            h,
            bgra: bgra.to_vec(),
        });
        self.link.submit();
        Ok(())
    }

    /// [`GpuCanvas::flush`]: waits until everything recorded is replayed and submitted.
    pub fn flush(&mut self) -> Result<()> {
        let (reply, answer) = channel();
        self.link.push(Op::Flush { id: self.id, reply });
        self.link.submit();
        answer
            .recv()
            .map_err(|_| windows::core::Error::from(E_FAIL))
    }
}

impl Drop for RemoteCanvas {
    fn drop(&mut self) {
        self.link.push(Op::Drop(self.id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> DrawParams<'static> {
        DrawParams {
            transform: Matrix::IDENTITY,
            alpha: 1.0,
            op: CompositeOp::SourceOver,
            clip: None,
            shadow: None,
            filter: None,
            smoothing: true,
        }
    }

    /// The same operations give the same pixels drawn here and on the canvas thread.
    #[test]
    fn a_remote_canvas_draws_what_a_canvas_here_draws() {
        let mut path = PathData::default();
        for i in 0..40 {
            path.verbs.push(if i == 0 {
                crate::canvas::PathVerb::Move
            } else {
                crate::canvas::PathVerb::Line
            });
            path.points.push(Point::new(
                i as f32 * 2.0,
                20.0 + (i as f32 / 3.0).sin() * 15.0,
            ));
        }
        let stroke = StrokeParams {
            width: 1.5,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: &[],
            dash_offset: 0.0,
        };
        let red = [0.8, 0.1, 0.1, 1.0];
        let p = params();
        let mut here = GpuCanvas::new(80, 40, Driver::Warp).unwrap();
        let mut there = RemoteCanvas::new(80, 40, Driver::Warp).unwrap();
        here.fill(
            FillShape::Rect(Rect::from_ltrb(4.0, 4.0, 30.0, 30.0)),
            &CanvasPaint::Solid(red),
            &p,
        )
        .unwrap();
        there
            .fill(
                FillShape::Rect(Rect::from_ltrb(4.0, 4.0, 30.0, 30.0)),
                &CanvasPaint::Solid(red),
                &p,
            )
            .unwrap();
        here.stroke(
            &path,
            &stroke,
            &CanvasPaint::Solid([0.0, 0.0, 1.0, 1.0]),
            &p,
        )
        .unwrap();
        there
            .stroke(
                &path,
                &stroke,
                &CanvasPaint::Solid([0.0, 0.0, 1.0, 1.0]),
                &p,
            )
            .unwrap();
        here.clear(
            FillShape::Rect(Rect::from_ltrb(10.0, 10.0, 14.0, 14.0)),
            None,
        )
        .unwrap();
        there
            .clear(
                FillShape::Rect(Rect::from_ltrb(10.0, 10.0, 14.0, 14.0)),
                None,
            )
            .unwrap();
        let a = here.read(0, 0, 80, 40).unwrap();
        let b = there.read(0, 0, 80, 40).unwrap();
        assert!(a.iter().any(|&v| v != 0));
        assert_eq!(a, b);
    }

    /// Writes, images and reads keep their order: a read sees the write and the image drawn before
    /// it, and an image dropped after use is still drawn.
    #[test]
    fn recorded_operations_keep_their_order() {
        let mut c = RemoteCanvas::new(4, 4, Driver::Warp).unwrap();
        let blue = [255u8, 0, 0, 255].repeat(16);
        c.write(0, 0, 4, 4, &blue).unwrap();
        assert_eq!(c.read(0, 0, 4, 4).unwrap(), blue);
        let green = [0u8, 255, 0, 255].repeat(4);
        let image = c.image_from_bgra(2, 2, &green).unwrap();
        let mut p = params();
        p.smoothing = false;
        c.draw_image(
            &image,
            Rect::from_ltrb(0.0, 0.0, 2.0, 2.0),
            Rect::from_ltrb(0.0, 0.0, 2.0, 2.0),
            &p,
        )
        .unwrap();
        drop(image);
        let px = c.read(0, 0, 4, 4).unwrap();
        assert_eq!(&px[0..4], &[0, 255, 0, 255]);
        assert_eq!(&px[12..16], &[255, 0, 0, 255]);
        c.flush().unwrap();
    }

    fn still(width: u32, height: u32, bgra: [u8; 4]) -> Arc<DecodedImage> {
        Arc::new(DecodedImage::still(crate::image::Frame {
            width,
            height,
            pixels: bgra.repeat((width * height) as usize),
            delay_ms: 0,
        }))
    }

    fn at(x: f32) -> Rect {
        Rect::from_ltrb(x, 0.0, x + 2.0, 2.0)
    }

    /// A decoded image drawn again, by any canvas of the thread, is the image its first draw
    /// uploaded; once the decoded image is freed, the next frame read gives the upload back.
    #[test]
    fn a_decoded_image_is_uploaded_once_and_kept_between_draws() {
        let mut c = RemoteCanvas::new(4, 4, Driver::Warp).unwrap();
        let mut other = RemoteCanvas::new(4, 4, Driver::Warp).unwrap();
        let green = still(2, 2, [0, 255, 0, 255]);
        let mut p = params();
        p.smoothing = false;
        c.draw_frame(&green, at(0.0), at(0.0), &p).unwrap();
        let id = c.link.frame_image(&green);
        let made = c.link.next_id.get();
        c.draw_frame(&green, at(0.0), at(2.0), &p).unwrap();
        other.draw_frame(&green, at(0.0), at(0.0), &p).unwrap();
        assert_eq!(c.link.frame_image(&green), id);
        assert_eq!(c.link.next_id.get(), made, "no other image was made");
        assert_eq!(c.link.frames.borrow().entries.len(), 1);
        assert_eq!(c.link.frames.borrow().bytes, 16);
        let px = c.read(0, 0, 4, 4).unwrap();
        assert_eq!(&px[0..4], &[0, 255, 0, 255]);
        assert_eq!(&px[8..12], &[0, 255, 0, 255]);
        assert_eq!(&px[32..36], &[0, 0, 0, 0]);
        assert_eq!(&other.read(0, 0, 4, 4).unwrap()[0..4], &[0, 255, 0, 255]);
        // Another decoded image with the same pixels is another image.
        let same = still(2, 2, [0, 255, 0, 255]);
        assert_ne!(c.link.frame_image(&same), id);
        drop(green);
        drop(same);
        assert_eq!(c.link.frames.borrow().entries.len(), 2);
        c.read_later(None).wait().unwrap();
        assert!(c.link.frames.borrow().entries.is_empty());
        assert_eq!(c.link.frames.borrow().bytes, 0);
    }

    /// Past the budget the images drawn longest ago go; one of them drawn again is uploaded again.
    #[test]
    fn kept_images_past_the_budget_go_oldest_first() {
        let mut c = RemoteCanvas::new(4, 4, Driver::Warp).unwrap();
        c.link.frames.borrow_mut().budget = 40;
        let (red, green, blue) = (
            still(2, 2, [0, 0, 255, 255]),
            still(2, 2, [0, 255, 0, 255]),
            still(2, 2, [255, 0, 0, 255]),
        );
        let mut p = params();
        p.smoothing = false;
        c.draw_frame(&red, at(0.0), at(0.0), &p).unwrap();
        c.draw_frame(&green, at(0.0), at(0.0), &p).unwrap();
        let (red_id, green_id) = (c.link.frame_image(&red), c.link.frame_image(&green));
        // Red was used last, so blue takes green's place.
        c.link.frame_image(&red);
        c.draw_frame(&blue, at(0.0), at(0.0), &p).unwrap();
        assert_eq!(c.link.frames.borrow().entries.len(), 2);
        assert_eq!(c.link.frames.borrow().bytes, 32);
        assert_eq!(c.link.frame_image(&red), red_id);
        c.draw_frame(&green, at(0.0), at(2.0), &p).unwrap();
        assert_ne!(c.link.frame_image(&green), green_id);
        let px = c.read(0, 0, 4, 4).unwrap();
        assert_eq!(&px[0..4], &[255, 0, 0, 255]);
        assert_eq!(&px[8..12], &[0, 255, 0, 255]);
        // An image larger than the budget is still drawn, alone.
        let big = still(4, 4, [0, 0, 255, 255]);
        c.draw_frame(
            &big,
            Rect::from_ltrb(0.0, 0.0, 4.0, 4.0),
            Rect::from_ltrb(0.0, 0.0, 4.0, 4.0),
            &p,
        )
        .unwrap();
        assert_eq!(c.link.frames.borrow().entries.len(), 1);
        assert_eq!(&c.read(0, 0, 4, 4).unwrap()[60..64], &[0, 0, 255, 255]);
    }

    /// The page thread does not wait for an operation to be drawn: recording a batch takes no part
    /// of the time drawing it takes.
    #[test]
    fn drawing_does_not_wait_for_the_canvas_thread() {
        let mut c = RemoteCanvas::new(512, 512, Driver::Warp).unwrap();
        c.flush().unwrap();
        let mut path = PathData::default();
        for i in 0..2000 {
            path.verbs.push(if i == 0 {
                crate::canvas::PathVerb::Move
            } else {
                crate::canvas::PathVerb::Line
            });
            path.points
                .push(Point::new((i * 37 % 509) as f32, (i * 91 % 503) as f32));
        }
        let stroke = StrokeParams {
            width: 3.0,
            cap: LineCap::Round,
            join: LineJoin::Round,
            miter_limit: 10.0,
            dash: &[],
            dash_offset: 0.0,
        };
        let p = params();
        let t = std::time::Instant::now();
        for _ in 0..20 {
            c.stroke(
                &path,
                &stroke,
                &CanvasPaint::Solid([0.0, 0.0, 0.0, 0.5]),
                &p,
            )
            .unwrap();
        }
        let recorded = t.elapsed();
        c.flush().unwrap();
        let drawn = t.elapsed();
        assert!(
            recorded * 4 < drawn,
            "recorded in {recorded:?}, drawn in {drawn:?}"
        );
    }

    /// A read for later does not wait: its pixels come once the canvas thread has drawn what was
    /// recorded before it, and it calls its wake-up after. The first read's wake-up holds the
    /// canvas thread here, so the second cannot have been read when it is asked for.
    #[test]
    fn a_read_for_later_does_not_wait_for_the_canvas_thread() {
        let mut c = RemoteCanvas::new(4, 2, Driver::Warp).unwrap();
        let red = CanvasPaint::Solid([1.0, 0.0, 0.0, 1.0]);
        c.fill(
            FillShape::Rect(Rect::new(0.0, 0.0, 4.0, 2.0)),
            &red,
            &params(),
        )
        .unwrap();
        let (hold_tx, hold) = std::sync::mpsc::channel::<()>();
        let hold = std::sync::Mutex::new(hold);
        let (woke_tx, woke) = std::sync::mpsc::channel::<()>();
        let woke_tx = std::sync::Mutex::new(woke_tx);
        let first = c.read_later(Some(Arc::new(move || {
            let _ = woke_tx.lock().unwrap().send(());
            let _ = hold.lock().unwrap().recv();
        })));
        woke.recv().unwrap();
        let pixels = first.try_take().expect("sent before the wake-up").unwrap();
        assert_eq!(&pixels[..4], &[0, 0, 255, 255]);
        let blue = CanvasPaint::Solid([0.0, 0.0, 1.0, 1.0]);
        c.fill(
            FillShape::Rect(Rect::new(0.0, 0.0, 4.0, 2.0)),
            &blue,
            &params(),
        )
        .unwrap();
        let second = c.read_later(None);
        assert!(second.try_take().is_none(), "the canvas thread is held");
        hold_tx.send(()).unwrap();
        let pixels = second.wait().unwrap();
        assert_eq!(pixels.len(), 4 * 2 * 4);
        assert_eq!(&pixels[..4], &[255, 0, 0, 255]);
    }
}
