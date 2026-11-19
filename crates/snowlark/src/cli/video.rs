// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --video <url-or-file> <png> [--time T] [--wait S] [--width W] [--height H]`: loads a page
//! (or a media file, which gets a media document), takes its first `<video>`, seeks it to `T`
//! seconds (default 0) paused, lets the page run `S` more seconds (default 0.4: pages that build
//! their layout late need more), and writes the page with that frame to a PNG. Media Foundation decodes on the GPU; the
//! frame is copied to memory for the capture. Prints the video's size, duration and the time shown.

use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, Url};
use snowlark_paint::{Point, Size};

const USAGE: &str =
    "usage: snowlark --video <url-or-file> <png> [--time T] [--wait S] [--width W] [--height H]";

fn parse_target(text: &str) -> Option<Url> {
    if let Ok(url) = Url::parse(text) {
        if url.scheme().len() > 1 {
            return Some(url);
        }
    }
    let path = std::path::Path::new(text);
    if path.exists() {
        return std::fs::canonicalize(path)
            .ok()
            .and_then(|p| Url::from_file_path(p).ok());
    }
    Url::parse(&format!("https://{text}")).ok()
}

pub fn run(args: &[String]) -> i32 {
    let mut positional = Vec::new();
    let mut time = 0.0f64;
    let mut wait = 0.4f64;
    let mut width = 1280.0f32;
    let mut height = 720.0f32;
    let mut i = 0;
    while i < args.len() {
        let num = |i: usize| args.get(i + 1).and_then(|v| v.parse::<f64>().ok());
        match args[i].as_str() {
            "--time" => match num(i) {
                Some(t) if t >= 0.0 => {
                    time = t;
                    i += 2;
                }
                _ => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            "--wait" => match num(i) {
                Some(w) if w >= 0.0 => {
                    wait = w;
                    i += 2;
                }
                _ => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            "--width" | "--height" => match num(i) {
                Some(v) if v >= 1.0 => {
                    if args[i] == "--width" {
                        width = v as f32;
                    } else {
                        height = v as f32;
                    }
                    i += 2;
                }
                _ => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            a => {
                positional.push(a.to_owned());
                i += 1;
            }
        }
    }
    let [target, png] = positional.as_slice() else {
        eprintln!("{USAGE}");
        return 2;
    };
    let Some(url) = parse_target(target) else {
        eprintln!("not a URL: {target}");
        return 2;
    };
    let wait = Duration::from_secs_f64(wait);
    match capture(&url, std::path::Path::new(png), time, wait, Size::new(width, height)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("video: {e}");
            1
        }
    }
}

fn capture(
    url: &Url,
    png: &std::path::Path,
    time: f64,
    wait: Duration,
    viewport: Size,
) -> Result<(), String> {
    let started = Instant::now();
    let thread = snowlark_tab::script_check::start_net(NetConfig::from_env()).map_err(|e| format!("network: {e}"))?;
    let setup = snowlark_page::RenderSetup::new()?;
    let mut page = snowlark_page::Page::new(snowlark_page::PageOptions {
        net: Some(thread.client()),
        viewport: (viewport.width as i32, viewport.height as i32),
        render: Some(setup),
        ..Default::default()
    });
    let gpu = snowlark_paint::Gpu::new().map_err(|e| format!("no GPU device: {e}"))?;
    page.engine()
        .set_media_config(snowlark_page::media::MediaConfig {
            videos: Some(snowlark_paint::video::VideoStore::new(Some(gpu))),
            autoplay: snowlark_page::media::AutoplayPolicy::Allowed,
            copy_frames: true,
            silent: true,
            ..Default::default()
        });
    page.navigate(url.clone());
    let deadline = started + Duration::from_secs(30);
    let state = |p: &mut snowlark_page::Page| -> Option<(u16, bool, f64)> {
        let s = p
            .eval(
                "(() => { const v = document.querySelector('video'); return v ? v.readyState + ' ' + v.seeking + ' ' + v.currentTime : ''; })()",
                "snowlark-video",
            )
            .ok()?;
        let mut it = s.split(' ');
        Some((
            it.next()?.parse().ok()?,
            it.next()? == "true",
            it.next()?.parse().ok()?,
        ))
    };
    // The metadata.
    let mut loaded = false;
    page.run_until(deadline, |p| {
        loaded = state(p).is_some_and(|s| s.0 >= 1);
        loaded
    });
    if !loaded {
        return Err("no video loaded its metadata".into());
    }
    let _ = page.eval(
        &format!("(() => {{ const v = document.querySelector('video'); v.pause(); v.currentTime = {time}; }})()"),
        "snowlark-video",
    );
    // The seek, then a moment for the frame to arrive and show.
    let mut sought = false;
    page.run_until(deadline, |p| {
        sought = state(p).is_some_and(|s| s.0 >= 2 && !s.1);
        sought
    });
    // (the frame shows once its copy is a moment old: keep the loop turning, it may have nothing
    // else to do)
    let settle = Instant::now() + wait;
    while Instant::now() < settle {
        page.turn();
        std::thread::sleep(Duration::from_millis(10));
    }
    page.update_the_rendering();
    let info = page
        .eval(
            "(() => { const v = document.querySelector('video'); return v.videoWidth + 'x' + v.videoHeight + ', duration ' + v.duration + ' s, at ' + v.currentTime + ' s'; })()",
            "snowlark-video",
        )
        .unwrap_or_default();
    let pixmap = page
        .engine()
        .with_renderer(|r, _| -> Result<_, String> {
            if r.fragments().is_none() {
                r.layout(viewport).ok_or("no document")?;
            }
            r.rasterize(viewport, Point::ZERO, 1.0)
        })
        .ok_or("no document")??;
    pixmap
        .write_png(png)
        .map_err(|e| format!("{}: {e}", png.display()))?;
    eprintln!(
        "[video] {info}{} -> {} in {:.0} ms",
        if sought { "" } else { " (the seek did not finish)" },
        png.display(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}
