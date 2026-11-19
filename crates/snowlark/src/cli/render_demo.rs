// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --render-demo <png> [--runs N]`: paints the snowlark-paint demo display list headless
//! (Direct2D on a WIC bitmap) to `<png>`, paints it again in 512 px GPU tiles to `<png stem>.gpu.png`,
//! and prints timings: list building, headless and hardware rasterization at 1300×1000, and the cost
//! of shaping a 1,000-word paragraph.

use std::path::{Path, PathBuf};
use std::time::Instant;

use snowlark_paint::{
    demo, Driver, Gpu, HeadlessRenderer, ImageStore, ShapeOptions, ShapedText, Sources, TextEngine,
    TileRasterizer, Viewport,
};

/// The median of `runs` timings of `f`, in milliseconds (after one warm-up call).
fn median_ms(runs: usize, mut f: impl FnMut() -> Result<(), String>) -> Result<f64, String> {
    f()?;
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        f()?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    Ok(times[times.len() / 2])
}

fn gpu_path(png: &Path) -> PathBuf {
    let stem = png.file_stem().and_then(|s| s.to_str()).unwrap_or("demo");
    png.with_file_name(format!("{stem}.gpu.png"))
}

pub fn run(args: &[String]) -> i32 {
    let mut png = None;
    let mut runs = 20usize;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--runs" {
            runs = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(runs).max(1);
            i += 2;
        } else {
            png = Some(PathBuf::from(&args[i]));
            i += 1;
        }
    }
    let Some(png) = png else {
        eprintln!("usage: snowlark --render-demo <png> [--runs N]");
        return 2;
    };
    match render(&png, runs) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("render-demo: {e}");
            1
        }
    }
}

fn render(png: &Path, runs: usize) -> Result<(), String> {
    let e = |e: snowlark_paint::WinError| e.to_string();
    let text = TextEngine::new().map_err(e)?;
    let images = ImageStore::new();
    let started = Instant::now();
    let list = demo::build(&text, &images).map_err(e)?;
    let first_build = started.elapsed().as_secs_f64() * 1000.0;
    let rebuild = median_ms(runs, || {
        let scratch = ImageStore::new();
        demo::build(&text, &scratch).map(|_| ()).map_err(e)
    })?;
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let view = Viewport {
        width: demo::WIDTH,
        height: demo::HEIGHT,
        scale: 1.0,
        background: demo::BACKGROUND,
    };
    println!(
        "display list: {} items, {} glyphs; first build {first_build:.1} ms (font matching, decoding), rebuild {rebuild:.2} ms",
        list.len(),
        list.glyph_ids.len()
    );

    // Headless: Direct2D on a WIC bitmap.
    let mut headless = HeadlessRenderer::new().map_err(e)?;
    let pixmap = headless.render(&list, sources, &view).map_err(e)?;
    pixmap.write_png(png).map_err(e)?;
    let soft = median_ms(runs, || headless.render(&list, sources, &view).map(|_| ()).map_err(e))?;
    println!(
        "headless (WIC, software) {}x{}: {soft:.2} ms median of {runs} -> {}",
        view.width,
        view.height,
        png.display()
    );

    // GPU tiles, on the graphics card when there is one.
    match Gpu::new() {
        Ok(gpu) => {
            let kind = if gpu.driver() == Driver::Hardware { "hardware" } else { "WARP" };
            let adapter = gpu.adapter().to_string();
            let mut tiles = TileRasterizer::new(gpu).map_err(e)?;
            let frame = tiles.raster(&list, sources, &view).map_err(e)?;
            let n = frame.tiles.len();
            let back = tiles.read_back(&frame).map_err(e)?;
            tiles.recycle(frame);
            let gpu_png = gpu_path(png);
            back.write_png(&gpu_png).map_err(e)?;
            let submit = median_ms(runs, || {
                let f = tiles.raster(&list, sources, &view).map_err(e)?;
                tiles.recycle(f);
                Ok(())
            })?;
            let complete = median_ms(runs, || {
                let f = tiles.raster(&list, sources, &view).map_err(e)?;
                tiles.read_back(&f).map_err(e)?;
                tiles.recycle(f);
                Ok(())
            })?;
            println!(
                "{kind} tiles ({adapter}), {n} tiles of 512: raster {submit:.2} ms (submitted), {complete:.2} ms with read-back (GPU done) -> {}",
                gpu_png.display()
            );
        }
        Err(err) => println!("no GPU device: {err}"),
    }

    // Shaping a 1,000-word paragraph.
    let paragraph = demo::thousand_words();
    let words = paragraph.split_whitespace().count();
    let list_id = text
        .font_list(&snowlark_paint::FontQuery {
            families: &["Segoe UI"],
            weight: 400,
            style: snowlark_paint::FontStyle::Normal,
            stretch: snowlark_paint::FontStretch::NORMAL,
        })
        .map_err(e)?;
    let mut shaped = ShapedText::default();
    let options = ShapeOptions {
        size: 16.0,
        ..Default::default()
    };
    let shape = median_ms(runs, || text.shape(&paragraph, list_id, &options, &mut shaped).map_err(e))?;
    println!(
        "shaping {words} words ({} chars, {} glyphs, {} clusters): {shape:.2} ms ({:.2} µs/word)",
        paragraph.chars().count(),
        shaped.glyphs.len(),
        shaped.clusters.len(),
        shape * 1000.0 / words as f64
    );
    Ok(())
}
