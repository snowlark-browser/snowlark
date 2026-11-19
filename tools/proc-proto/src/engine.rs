// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The engine control: today's real `snowlark_paint::Compositor` (chrome as a composition surface,
//! content as a waitable swap chain, Direct2D drawing) under the same loop, in A's process.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use snowlark_paint::window::WindowHandle;
use snowlark_paint::{
    Color, Compositor, DisplayList, DisplayListBuilder, Gpu, ImageStore, Rect, Sources, TextEngine,
    TiledFrame,
};
use windows::core::Result;

use crate::comp::{CMsg, Events, Options, Stats, CHROME_H};
use crate::marks;
use crate::scene;
use crate::wire::{qpc, qpc_ms};

const KEEP_PRESENTING: Duration = Duration::from_millis(250);

fn code(frame: u32, w: u32, h: u32) -> DisplayList {
    let mut b = DisplayListBuilder::new();
    let (x, y) = marks::ORIGIN;
    let s = marks::BLOCK as f32;
    for (i, on) in marks::bits(frame, w, h).iter().enumerate() {
        let c = if *on { Color::WHITE } else { Color::BLACK };
        b.fill_rect(Rect::new(x as f32 + i as f32 * s, y as f32, s, s), c);
    }
    b.build()
}

pub fn run(
    gpu: Arc<Gpu>,
    hwnd: isize,
    size: (u32, u32),
    rx: Receiver<CMsg>,
    tx: Sender<CMsg>,
    mut events: Events,
    options: Options,
) -> Result<()> {
    crate::raise_thread();
    let text = TextEngine::new()?;
    let images = ImageStore::new();
    // SAFETY: WindowHandle is a one-field struct over the window's handle value; nothing outside
    // the crate can make one, and this throwaway must not change the crate.
    let window: WindowHandle = unsafe { std::mem::transmute(hwnd) };
    let (w, h) = size;
    let mut comp = Compositor::new(gpu.clone(), &text, window, w, h, CHROME_H)?;

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
    let mut dirty = true;
    let mut chrome_dirty = true;
    let mut keep_until: Option<Instant> = None;
    let mut scroll = 0.0f32;
    let mut dir = 1.0f32;
    let mut pending: Option<i64> = None;
    let mut acks: Vec<(u32, i64)> = Vec::new();
    let mut tiles: Option<TiledFrame> = None;
    let mut stats = Stats::default();
    let mut number = options.frame_base;
    let mut first = true;
    let mut quit = false;

    macro_rules! handle {
        ($m:expr) => {
            match $m {
                CMsg::Quit => quit = true,
                CMsg::Hang(ms) => std::thread::sleep(Duration::from_millis(ms as u64)),
                CMsg::Scroll { dy, input } => {
                    let max = (scene::DOC_H - size.1.min(scene::DOC_H)) as f32;
                    scroll += dy * dir;
                    if scroll >= max || scroll <= 0.0 {
                        dir = -dir;
                        scroll = scroll.clamp(0.0, max);
                    }
                    if pending.is_none() {
                        pending = Some(input);
                    }
                    keep_until = Some(Instant::now() + KEEP_PRESENTING);
                    dirty = true;
                }
                CMsg::Frame(f) => {
                    if let Some(old) = tiles.replace(f) {
                        let _ = recycle_tx.send(old);
                    }
                    dirty = true;
                }
                CMsg::Resize { w, h, seq, at } => {
                    size = (w, h);
                    comp.resize(w, h, CHROME_H)?;
                    acks.push((seq, at));
                    dirty = true;
                    chrome_dirty = true;
                }
            }
        };
    }

    loop {
        let moving = keep_until.is_some_and(|t| Instant::now() < t);
        if !dirty && !chrome_dirty && !moving {
            keep_until = None;
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(m) => handle!(m),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        while let Ok(m) = rx.try_recv() {
            handle!(m);
        }
        if quit {
            break;
        }
        if dirty || keep_until.is_some_and(|t| Instant::now() < t) {
            let w0 = Instant::now();
            comp.wait_for_frame(Duration::from_millis(100));
            let waited = w0.elapsed().as_secs_f64() * 1000.0;
            while let Ok(m) = rx.try_recv() {
                handle!(m);
            }
            if quit {
                break;
            }
            let t0 = Instant::now();
            number += 1;
            let (cw, ch) = size;
            let dy = -(scroll.round() as i32);
            let list = code(number, cw, ch);
            let sources = Sources {
                text: &text,
                images: &images,
            };
            let (content_w, content_h) = comp.content_size();
            comp.draw_content(|pass| {
                pass.clear(Color::WHITE);
                if let Some(f) = &tiles {
                    let clip = Rect::new(0.0, 0.0, content_w as f32, content_h as f32);
                    pass.tiles(f, 0, dy, clip);
                }
                pass.list(&list, sources, 1.0, 0.0, 0.0);
            })?;
            let drawn = t0.elapsed().as_secs_f64() * 1000.0;
            if chrome_dirty {
                chrome_dirty = false;
                let chrome = code(0, cw, ch);
                comp.draw_chrome(&chrome, sources, 1.0, Color::hex(0x263359))?;
            }
            comp.present()?;
            let done = qpc();
            dirty = false;
            let moving = keep_until.is_some();
            stats.frame(number, moving, waited, drawn, done, pending.take());
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
