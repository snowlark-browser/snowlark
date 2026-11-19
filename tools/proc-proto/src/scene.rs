// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The test page and the raster thread: a display list of boxes and text, rasterized into
//! `TileRasterizer`'s 512-pixel tiles on a thread of its own, as `raster_thread.rs` does for a tab.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use snowlark_paint::{
    Color, DisplayList, DisplayListBuilder, FontQuery, FontStretch, FontStyle, Gpu, ImageStore,
    Point, Rect, ShapeOptions, ShapedText, Sources, TextEngine, TileRasterizer, TiledFrame,
    Viewport,
};

pub const DOC_W: u32 = 1400;
pub const DOC_H: u32 = 3072;

/// The page; `version` changes a box in every row so that a new raster repaints tiles.
pub fn page(text: &TextEngine, version: u32) -> DisplayList {
    let mut b = DisplayListBuilder::new();
    let list = text
        .font_list(&FontQuery {
            families: &["Segoe UI"],
            weight: 400,
            style: FontStyle::Normal,
            stretch: FontStretch::NORMAL,
        })
        .expect("font");
    let mut shaped = ShapedText::default();
    let options = ShapeOptions {
        size: 15.0,
        ..Default::default()
    };
    let line = "Process isolation prototype: tiles rasterized here never cross a process boundary.";
    let mut y = 0.0;
    let mut i = 0u32;
    while y < DOC_H as f32 {
        let tone = 0xe8 + (i % 3) * 8;
        let bg = Color::hex(tone << 16 | tone << 8 | 0xf8);
        b.fill_rect(Rect::new(0.0, y, DOC_W as f32, 96.0), bg);
        let stripe = Color::hex((version.wrapping_mul(2654435761) ^ i) & 0xff_ff_ff);
        b.fill_rect(Rect::new(1180.0, y + 8.0, 200.0, 80.0), stripe);
        for row in 0..4 {
            text.shape(line, list, &options, &mut shaped)
                .expect("shape");
            let n = shaped.clusters.len();
            shaped.paint(
                0..n,
                Point::new(24.0, y + 22.0 + row as f32 * 21.0),
                Color::hex(0x202030),
                &mut b,
            );
        }
        y += 96.0;
        i += 1;
    }
    b.build()
}

/// Runs the raster thread: one full frame at once, then (with `every`) a new one at that period,
/// so that raster work shares the device with the compositor as it does on a real page.
pub fn raster_thread(
    gpu: Arc<Gpu>,
    every: Option<Duration>,
    out: Sender<TiledFrame>,
    recycle: Receiver<TiledFrame>,
    stop: Arc<AtomicBool>,
) {
    let text = TextEngine::new().expect("text engine");
    let images = ImageStore::new();
    let mut raster = TileRasterizer::new(gpu).expect("rasterizer");
    let view = Viewport {
        width: DOC_W,
        height: DOC_H,
        scale: 1.0,
        background: Color::WHITE,
    };
    let mut version = 0;
    loop {
        let started = Instant::now();
        let list = page(&text, version);
        version += 1;
        let frame = raster
            .raster(
                &list,
                Sources {
                    text: &text,
                    images: &images,
                },
                &view,
            )
            .expect("raster");
        if out.send(frame).is_err() {
            return;
        }
        while let Ok(old) = recycle.try_recv() {
            raster.recycle(old);
        }
        let Some(period) = every else {
            // (nothing more to paint; live until told to stop)
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            return;
        };
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(period.saturating_sub(started.elapsed()));
    }
}
