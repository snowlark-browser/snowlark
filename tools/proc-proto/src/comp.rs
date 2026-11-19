// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The compositor loop of the prototype: `composite.rs`'s frame pacing and latency measure over
//! swap chains that come from one of the presentation options. The same code runs in A (the
//! same-process control) and in B (the options), so what differs between runs is the boundary.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use snowlark_paint::{Color, Gpu, Rect, TiledFrame, TILE_SIZE};
use windows::core::{Interface, Result};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::WaitForSingleObjectEx;

use crate::marks;
use crate::scene;
use crate::wire::{qpc, qpc_ms};

pub const CHROME_H: u32 = 80;
/// `composite.rs`'s KEEP_PRESENTING.
const KEEP_PRESENTING: Duration = Duration::from_millis(250);
pub const CHAIN_FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;

pub enum CMsg {
    Scroll {
        dy: f32,
        input: i64,
    },
    Resize {
        w: u32,
        h: u32,
        seq: u32,
        at: i64,
    },
    Frame(TiledFrame),
    /// Stop answering for this many ms, as a hung compositor would.
    Hang(u32),
    Quit,
}

/// Where the swap chains come from.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    /// `CreateSwapChainForComposition` on a visual tree of our own over this window.
    Hwnd(isize),
    /// `CreateSwapChainForCompositionSurfaceHandle` on handles from A (option 1).
    Surfaces { content: isize, chrome: isize },
    /// One chain bound to the window itself (`CreateSwapChainForHwnd`): the known-bad pacing case.
    WindowChain(isize),
    /// Option 1 with one chain for chrome and content: nothing can show them at different sizes.
    OneSurface(isize),
}

pub struct Events {
    pub ack: Box<dyn FnMut(u32, i64) + Send>,
    pub first: Box<dyn FnMut(i64) + Send>,
}

#[derive(Clone)]
pub struct Options {
    pub raster_every: Option<Duration>,
    pub frame_base: u32,
    pub log: Option<PathBuf>,
    /// Sleep this long at random (up to) before each frame: an uneven frame pace.
    pub jitter_ms: f64,
    pub present_chrome_first: bool,
}

pub struct Chain {
    pub sc: IDXGISwapChain2,
    pub waitable: HANDLE,
    pub size: (u32, u32),
    back: Option<(ID3D11Texture2D, ID3D11RenderTargetView)>,
}

pub fn chain_desc(w: u32, h: u32) -> DXGI_SWAP_CHAIN_DESC1 {
    DXGI_SWAP_CHAIN_DESC1 {
        Width: w,
        Height: h,
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

impl Chain {
    fn wrap(sc: IDXGISwapChain1, w: u32, h: u32) -> Result<Chain> {
        let sc: IDXGISwapChain2 = sc.cast()?;
        // SAFETY: plain calls on a live swap chain made with the waitable flag.
        unsafe {
            sc.SetMaximumFrameLatency(1)?;
            let waitable = sc.GetFrameLatencyWaitableObject();
            Ok(Chain {
                sc,
                waitable,
                size: (w, h),
                back: None,
            })
        }
    }

    pub fn composition(gpu: &Gpu, w: u32, h: u32) -> Result<Chain> {
        // SAFETY: a valid description; the result is owned.
        let sc = unsafe {
            gpu.dxgi_factory().CreateSwapChainForComposition(
                gpu.d3d_device(),
                &chain_desc(w, h),
                None,
            )?
        };
        Chain::wrap(sc, w, h)
    }

    pub fn for_handle(gpu: &Gpu, handle: isize, w: u32, h: u32) -> Result<Chain> {
        let media: IDXGIFactoryMedia = gpu.dxgi_factory().cast()?;
        // SAFETY: `handle` is a composition surface handle duplicated into this process.
        let sc = unsafe {
            media.CreateSwapChainForCompositionSurfaceHandle(
                gpu.d3d_device(),
                Some(HANDLE(handle as *mut core::ffi::c_void)),
                &chain_desc(w, h),
                None,
            )?
        };
        Chain::wrap(sc, w, h)
    }

    pub fn for_window(gpu: &Gpu, hwnd: isize, w: u32, h: u32) -> Result<Chain> {
        // SAFETY: a live window of this process.
        let sc = unsafe {
            gpu.dxgi_factory().CreateSwapChainForHwnd(
                gpu.d3d_device(),
                HWND(hwnd as *mut core::ffi::c_void),
                &chain_desc(w, h),
                None,
                None,
            )?
        };
        Chain::wrap(sc, w, h)
    }

    pub fn back(&mut self, gpu: &Gpu) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView)> {
        if self.back.is_none() {
            // SAFETY: buffer 0 of a flip-model chain is the current back buffer under D3D11.
            unsafe {
                let tex: ID3D11Texture2D = self.sc.GetBuffer(0)?;
                let mut rtv = None;
                gpu.d3d_device()
                    .CreateRenderTargetView(&tex, None, Some(&mut rtv))?;
                self.back = Some((tex, rtv.unwrap()));
            }
        }
        Ok(self.back.clone().unwrap())
    }

    pub fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        if (w, h) == self.size {
            return Ok(());
        }
        self.back = None;
        // SAFETY: no reference to the buffers is held any more.
        unsafe {
            self.sc
                .ResizeBuffers(2, w, h, DXGI_FORMAT_UNKNOWN, CHAIN_FLAGS)?
        };
        self.size = (w, h);
        Ok(())
    }

    pub fn wait(&self, ms: u32) -> bool {
        // SAFETY: the chain's own waitable handle, open until drop.
        unsafe { WaitForSingleObjectEx(self.waitable, ms, true) == WAIT_OBJECT_0 }
    }

    pub fn present(&self) -> Result<()> {
        // SAFETY: as `Compositor::present`.
        unsafe { self.sc.Present(1, DXGI_PRESENT(0)).ok() }
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        // SAFETY: the handle from GetFrameLatencyWaitableObject belongs to us.
        let _ = unsafe { CloseHandle(self.waitable) };
    }
}

/// A's visual tree: root, content and chrome over the window, as `Compositor::new` makes it.
pub struct Tree {
    pub dev: IDCompositionDevice,
    pub target: IDCompositionTarget,
    pub root: IDCompositionVisual,
    pub content: IDCompositionVisual,
    pub chrome: IDCompositionVisual,
}

impl Tree {
    /// With no `dxgi` the DirectComposition device has no Direct3D device at all.
    pub fn new(dxgi: Option<&IDXGIDevice>, hwnd: isize) -> Result<Tree> {
        // SAFETY: object creation; every interface is owned by the Tree.
        unsafe {
            let dev: IDCompositionDevice = DCompositionCreateDevice(dxgi)?;
            let target = dev.CreateTargetForHwnd(HWND(hwnd as *mut core::ffi::c_void), true)?;
            let root = dev.CreateVisual()?;
            let content = dev.CreateVisual()?;
            let chrome = dev.CreateVisual()?;
            content.SetOffsetY2(CHROME_H as f32)?;
            root.AddVisual(&content, false, None)?;
            root.AddVisual(&chrome, true, &content)?;
            target.SetRoot(&root)?;
            Ok(Tree {
                dev,
                target,
                root,
                content,
                chrome,
            })
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub intervals: Vec<f64>,
    pub draws: Vec<f64>,
    pub waits: Vec<f64>,
    pub latencies: Vec<f64>,
    pub resize_acks: Vec<f64>,
    pub rows: Vec<String>,
    last_done: Option<i64>,
    pub frames: u32,
}

pub fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

impl Stats {
    /// One frame, as `composite.rs::frame` counts it.
    pub fn frame(
        &mut self,
        number: u32,
        moving: bool,
        wait: f64,
        compose: f64,
        done: i64,
        input: Option<i64>,
    ) {
        let interval = self.last_done.map(|t| qpc_ms(done - t));
        self.last_done = Some(done);
        self.frames += 1;
        let latency = input.map(|t| qpc_ms(done - t));
        if moving {
            if let Some(i) = interval {
                if i < 100.0 {
                    self.intervals.push(i);
                }
            }
            self.draws.push(compose);
            self.waits.push(wait);
            if let Some(l) = latency {
                self.latencies.push(l);
            }
        }
        // SAFETY: an out struct of the right size.
        let vblank = unsafe {
            let mut info = DWM_TIMING_INFO {
                cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
                ..Default::default()
            };
            let _ = DwmGetCompositionTimingInfo(HWND::default(), &mut info);
            info.qpcVBlank
        };
        self.rows.push(format!(
            "{number}\t{done}\t{:.3}\t{:.3}\t{}\t{}\t{vblank}",
            wait,
            compose,
            input.unwrap_or(0),
            moving as u8
        ));
    }

    pub fn summary(&mut self) -> String {
        let n = self.intervals.len();
        let k = self.latencies.len();
        format!(
            "[p0] {n} frames, interval p50 {:.2} p95 {:.2} max {:.2} ms, compose p50 {:.2} max {:.2} ms, frame wait p50 {:.2} ms; input->present ({k} inputs) p50 {:.2} p95 {:.2} max {:.2} ms; resize acks {} p50 {:.2} p95 {:.2} max {:.2} ms",
            pct(&mut self.intervals, 0.5),
            pct(&mut self.intervals, 0.95),
            pct(&mut self.intervals, 1.0),
            pct(&mut self.draws, 0.5),
            pct(&mut self.draws, 1.0),
            pct(&mut self.waits, 0.5),
            pct(&mut self.latencies, 0.5),
            pct(&mut self.latencies, 0.95),
            pct(&mut self.latencies, 1.0),
            self.resize_acks.len(),
            pct(&mut self.resize_acks, 0.5),
            pct(&mut self.resize_acks, 0.95),
            pct(&mut self.resize_acks, 1.0),
        )
    }

    pub fn finish(&mut self, log: &Option<PathBuf>) {
        let s = self.summary();
        println!("{s}");
        if let Some(path) = log {
            let written = (|| -> std::io::Result<()> {
                // (buffered: a line per write took seconds on a busy disk)
                let f = std::fs::File::create(path.with_extension("frames.tsv"))?;
                let mut f = std::io::BufWriter::new(f);
                writeln!(f, "frame\tdone\twait\tcompose\tinput\tmoving\tvblank")?;
                for r in &self.rows {
                    writeln!(f, "{r}")?;
                }
                f.flush()?;
                std::fs::write(path.with_extension("summary.txt"), s + "\n")
            })();
            if let Err(e) = written {
                println!("[p0] log write failed: {e}");
            }
        }
    }
}

/// The part of `composite.rs`'s `State` that pacing depends on.
struct Pace {
    dirty: bool,
    keep_until: Option<Instant>,
    scroll: f32,
    dir: f32,
    pending_input: Option<i64>,
}

impl Pace {
    fn new() -> Pace {
        Pace {
            dirty: true,
            keep_until: None,
            scroll: 0.0,
            dir: 1.0,
            pending_input: None,
        }
    }

    fn moving(&self, now: Instant) -> bool {
        self.keep_until.is_some_and(|t| now < t)
    }

    fn scroll_by(&mut self, dy: f32, input: i64, view_h: u32) {
        let max = (scene::DOC_H - view_h.min(scene::DOC_H)) as f32;
        self.scroll += dy * self.dir;
        if self.scroll >= max || self.scroll <= 0.0 {
            self.dir = -self.dir;
            self.scroll = self.scroll.clamp(0.0, max);
        }
        if self.pending_input.is_none() {
            self.pending_input = Some(input);
        }
        self.keep_until = Some(Instant::now() + KEEP_PRESENTING);
        self.dirty = true;
    }
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.0 >> 33) as f64 / (1u64 << 31) as f64
    }
}

fn clear_view(ctx: &ID3D11DeviceContext1, rtv: &ID3D11RenderTargetView, c: [f32; 4], r: &[RECT]) {
    // SAFETY: a live view on this device; the caller holds the device lock.
    unsafe { ctx.ClearView(rtv, &c, Some(r)) };
}

fn write_marks(
    ctx: &ID3D11DeviceContext1,
    rtv: &ID3D11RenderTargetView,
    frame: u32,
    w: u32,
    h: u32,
    top: u32,
) {
    let (x, y) = marks::ORIGIN;
    let y = y + top;
    let b = marks::BLOCK as i32;
    let bits = marks::bits(frame, w, h);
    let rect = |i: usize| RECT {
        left: x as i32 + i as i32 * b,
        top: y as i32,
        right: x as i32 + (i as i32 + 1) * b,
        bottom: y as i32 + b,
    };
    let on: Vec<RECT> = (0..bits.len()).filter(|i| bits[*i]).map(rect).collect();
    let off: Vec<RECT> = (0..bits.len()).filter(|i| !bits[*i]).map(rect).collect();
    clear_view(ctx, rtv, [1.0, 1.0, 1.0, 1.0], &on);
    clear_view(ctx, rtv, [0.0, 0.0, 0.0, 1.0], &off);
}

fn copy_tiles(
    ctx: &ID3D11DeviceContext1,
    back: &ID3D11Texture2D,
    frame: &TiledFrame,
    dy: i32,
    size: (u32, u32),
    top: i32,
) {
    for t in &frame.tiles {
        let (x0, y0) = (t.x as i32, t.y as i32 + dy + top);
        let w = TILE_SIZE.min(frame.width.saturating_sub(t.x)) as i32;
        let h = TILE_SIZE.min(frame.height.saturating_sub(t.y)) as i32;
        let (cx0, cy0) = (x0.max(0), y0.max(top));
        let (cx1, cy1) = ((x0 + w).min(size.0 as i32), (y0 + h).min(size.1 as i32));
        if cx1 <= cx0 || cy1 <= cy0 {
            continue;
        }
        let src = D3D11_BOX {
            left: (cx0 - x0) as u32,
            top: (cy0 - y0) as u32,
            front: 0,
            right: (cx1 - x0) as u32,
            bottom: (cy1 - y0) as u32,
            back: 1,
        };
        // SAFETY: both textures are of this device, same format; the box is inside both; the caller
        // holds the device lock.
        unsafe {
            ctx.CopySubresourceRegion(
                back,
                0,
                cx0 as u32,
                cy0 as u32,
                0,
                &t.texture,
                0,
                Some(&src),
            )
        };
    }
}

/// The content and chrome layers of a [`Target`].
enum Layers {
    Two {
        content: Chain,
        chrome: Chain,
    },
    /// One chain for both (the chrome strip at its top).
    One(Chain),
}

/// Runs the compositor until [`CMsg::Quit`] or until `rx` closes. Call on a thread of its own.
pub fn run(
    gpu: Arc<Gpu>,
    target: Target,
    size: (u32, u32),
    rx: Receiver<CMsg>,
    tx: Sender<CMsg>,
    mut events: Events,
    options: Options,
) -> Result<()> {
    crate::raise_thread();
    let (w, h) = size;
    let ctx: ID3D11DeviceContext1 =
        // SAFETY: the device's immediate context, used under the device lock below.
        unsafe { gpu.d3d_device().GetImmediateContext()? }.cast()?;
    let mut tree = None;
    let mut layers = match target {
        Target::Hwnd(hwnd) => {
            let dxgi: IDXGIDevice = gpu.d3d_device().cast()?;
            let t = Tree::new(Some(&dxgi), hwnd)?;
            let content = Chain::composition(&gpu, w, h - CHROME_H)?;
            let chrome = Chain::composition(&gpu, w, CHROME_H)?;
            // SAFETY: setting our own visuals' content.
            unsafe {
                t.content.SetContent(&content.sc)?;
                t.chrome.SetContent(&chrome.sc)?;
                t.dev.Commit()?;
            }
            tree = Some(t);
            Layers::Two { content, chrome }
        }
        Target::Surfaces { content, chrome } => Layers::Two {
            content: Chain::for_handle(&gpu, content, w, h - CHROME_H)?,
            chrome: Chain::for_handle(&gpu, chrome, w, CHROME_H)?,
        },
        Target::WindowChain(hwnd) => Layers::One(Chain::for_window(&gpu, hwnd, w, h)?),
        Target::OneSurface(handle) => Layers::One(Chain::for_handle(&gpu, handle, w, h)?),
    };
    let _tree = tree;

    // The raster thread.
    let stop = Arc::new(AtomicBool::new(false));
    let (recycle_tx, recycle_rx) = channel();
    let (frames_tx, frames_rx) = channel();
    {
        let (gpu, stop, every) = (gpu.clone(), stop.clone(), options.raster_every);
        std::thread::spawn(move || {
            scene::raster_thread(gpu, every, frames_tx, recycle_rx, stop);
        });
    }
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(f) = frames_rx.recv() {
                if tx.send(CMsg::Frame(f)).is_err() {
                    return;
                }
            }
        });
    }

    let mut size = (w, h);
    let mut pace = Pace::new();
    let mut chrome_dirty = true;
    let mut acks: Vec<(u32, i64)> = Vec::new();
    let mut tiles: Option<TiledFrame> = None;
    let mut stats = Stats::default();
    let mut number = options.frame_base;
    let mut first = true;
    let mut random = Random(0x9e3779b97f4a7c15);
    let mut quit = false;

    let handle = |m: CMsg,
                  layers: &mut Layers,
                  pace: &mut Pace,
                  size: &mut (u32, u32),
                  chrome_dirty: &mut bool,
                  acks: &mut Vec<(u32, i64)>,
                  tiles: &mut Option<TiledFrame>,
                  quit: &mut bool| match m {
        CMsg::Quit => *quit = true,
        CMsg::Hang(ms) => std::thread::sleep(Duration::from_millis(ms as u64)),
        CMsg::Scroll { dy, input } => pace.scroll_by(dy, input, size.1),
        CMsg::Frame(f) => {
            if let Some(old) = tiles.replace(f) {
                let _ = recycle_tx.send(old);
            }
            pace.dirty = true;
        }
        CMsg::Resize { w, h, seq, at } => {
            *size = (w, h);
            match layers {
                Layers::Two { content, chrome } => {
                    content.resize(w, h - CHROME_H).expect("resize content");
                    chrome.resize(w, CHROME_H).expect("resize chrome");
                }
                Layers::One(chain) => chain.resize(w, h).expect("resize chain"),
            }
            acks.push((seq, at));
            pace.dirty = true;
            *chrome_dirty = true;
        }
    };

    loop {
        let now = Instant::now();
        if !pace.dirty && !chrome_dirty && !pace.moving(now) {
            pace.keep_until = None;
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(m) => handle(
                    m,
                    &mut layers,
                    &mut pace,
                    &mut size,
                    &mut chrome_dirty,
                    &mut acks,
                    &mut tiles,
                    &mut quit,
                ),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        while let Ok(m) = rx.try_recv() {
            handle(
                m,
                &mut layers,
                &mut pace,
                &mut size,
                &mut chrome_dirty,
                &mut acks,
                &mut tiles,
                &mut quit,
            );
        }
        if quit {
            break;
        }
        if pace.dirty || pace.moving(Instant::now()) {
            let w0 = Instant::now();
            match &layers {
                Layers::Two { content, .. } => content.wait(100),
                Layers::One(chain) => chain.wait(100),
            };
            let waited = w0.elapsed().as_secs_f64() * 1000.0;
            while let Ok(m) = rx.try_recv() {
                handle(
                    m,
                    &mut layers,
                    &mut pace,
                    &mut size,
                    &mut chrome_dirty,
                    &mut acks,
                    &mut tiles,
                    &mut quit,
                );
            }
            if quit {
                break;
            }
            if options.jitter_ms > 0.0 {
                let ms = random.next() * options.jitter_ms;
                std::thread::sleep(Duration::from_secs_f64(ms / 1000.0));
            }
            let t0 = Instant::now();
            number += 1;
            let dy = -(pace.scroll.round() as i32);
            let (cw, ch) = size;
            {
                let _lock = gpu.lock();
                let bg = [0.95, 0.95, 0.97, 1.0];
                match &mut layers {
                    Layers::Two { content, chrome } => {
                        let (back, rtv) = content.back(&gpu)?;
                        clear_view(&ctx, &rtv, bg, &[]);
                        if let Some(f) = &tiles {
                            copy_tiles(&ctx, &back, f, dy, content.size, 0);
                        }
                        write_marks(&ctx, &rtv, number, cw, ch, 0);
                        if chrome_dirty {
                            let (_, rtv) = chrome.back(&gpu)?;
                            clear_view(&ctx, &rtv, [0.15, 0.2, 0.35, 1.0], &[]);
                            write_marks(&ctx, &rtv, 0, cw, ch, 0);
                        }
                    }
                    Layers::One(chain) => {
                        let (back, rtv) = chain.back(&gpu)?;
                        clear_view(&ctx, &rtv, bg, &[]);
                        if let Some(f) = &tiles {
                            copy_tiles(&ctx, &back, f, dy, chain.size, CHROME_H as i32);
                        }
                        let strip = RECT {
                            left: 0,
                            top: 0,
                            right: cw as i32,
                            bottom: CHROME_H as i32,
                        };
                        clear_view(&ctx, &rtv, [0.15, 0.2, 0.35, 1.0], &[strip]);
                        write_marks(&ctx, &rtv, 0, cw, ch, 0);
                        write_marks(&ctx, &rtv, number, cw, ch, CHROME_H);
                    }
                }
            }
            let drawn = t0.elapsed().as_secs_f64() * 1000.0;
            match &layers {
                Layers::Two { content, chrome } => {
                    if chrome_dirty {
                        chrome.wait(20);
                        if options.present_chrome_first {
                            chrome.present()?;
                            content.present()?;
                        } else {
                            content.present()?;
                            chrome.present()?;
                        }
                    } else {
                        content.present()?;
                    }
                }
                Layers::One(chain) => chain.present()?,
            }
            chrome_dirty = false;
            let done = qpc();
            pace.dirty = false;
            let moving = pace.moving(Instant::now()) || pace.keep_until.is_some();
            let input = pace.pending_input.take();
            stats.frame(number, moving, waited, drawn, done, input);
            if first && tiles.is_some() {
                first = false;
                (events.first)(done);
            }
            for (seq, at) in acks.drain(..) {
                stats.resize_acks.push(qpc_ms(done - at));
                (events.ack)(seq, done);
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    stats.finish(&options.log);
    Ok(())
}

/// A rectangle of the engine's kind, for the engine control.
pub fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect::new(x, y, w, h)
}

pub const ENGINE_BG: Color = Color::WHITE;
