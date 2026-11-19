// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Animated images (HTML §4.8.4.3) through the renderer: `<img>`, CSS `background-image` and
//! `list-style-image` show the frame of the document timeline's time, loop as their files say, and
//! ask for rendering updates only while painted in view. The fixtures are made by the script in
//! `fixtures/README.md`: 16 × 16 frames red (100 ms), lime (200 ms), blue (300 ms).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use snowlark_net::{NetConfig, NetThread, Url};
use snowlark_paint::{Color, Point, Rect, Size, TextEngine};
use snowlark_render::{Renderer, TimelineClock};
use snowlark_style::DeviceConfig;

const RED: Color = Color::rgb(255, 0, 0);
const LIME: Color = Color::rgb(0, 255, 0);
const BLUE: Color = Color::rgb(0, 0, 255);
const VIEWPORT: Size = Size {
    width: 200.0,
    height: 100.0,
};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A renderer of `body` (a document in the fixtures folder) on a manual clock at 0, settled.
fn load(net: &NetThread, body: &str, clock: TimelineClock) -> Renderer {
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            scripting: false,
            ..Default::default()
        },
    );
    r.set_timeline_clock(clock);
    let html = format!(
        "<!DOCTYPE html><style>body {{ margin: 0 }} img {{ display: block }}</style>{body}"
    );
    let url = Url::from_file_path(fixtures().join("animated.html")).unwrap();
    r.load_html(html.as_bytes(), &url, None);
    let settled = r.render_until_settled(Duration::from_secs(20));
    assert!(settled.complete, "{settled:?}");
    assert_eq!(r.resource_counts().failed, 0);
    r
}

/// The pixel at (x, y) of a rendering update and paint at `ms` on the timeline.
fn at(r: &mut Renderer, ms: u64, points: &[(u32, u32)]) -> Vec<Color> {
    r.set_timeline_clock(TimelineClock::Manual(Duration::from_millis(ms)));
    assert!(r.update_rendering(VIEWPORT).is_some());
    let pixmap = r.rasterize(VIEWPORT, Point::ZERO, 1.0).unwrap();
    points.iter().map(|&(x, y)| pixmap.pixel(x, y)).collect()
}

fn manual() -> TimelineClock {
    TimelineClock::Manual(Duration::ZERO)
}

#[test]
fn img_frames_follow_the_timeline_and_loop() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    // GIF: forever; APNG: two plays; WebP: one.
    let mut r = load(
        &net,
        "<img src=anim-rgb.gif><img src=anim-rgb.png><img src=anim-rgb.webp>",
        manual(),
    );
    let pts = [(8, 8), (8, 24), (8, 40)];
    assert_eq!(at(&mut r, 0, &pts), [RED; 3]);
    assert_eq!(at(&mut r, 99, &pts), [RED; 3]);
    assert_eq!(at(&mut r, 100, &pts), [LIME; 3]);
    assert_eq!(at(&mut r, 350, &pts), [BLUE; 3]);
    // the second play
    assert_eq!(at(&mut r, 650, &pts), [RED, RED, BLUE]);
    assert_eq!(at(&mut r, 800, &pts), [LIME, LIME, BLUE]);
    // the third
    assert_eq!(at(&mut r, 1250, &pts), [RED, BLUE, BLUE]);
    assert_eq!(at(&mut r, 60_000 + 450, &pts), [BLUE; 3]);
}

#[test]
fn css_images_animate_too() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = load(
        &net,
        "<div style='width: 16px; height: 16px; background: url(anim-move.gif) no-repeat'></div>
         <div style='width: 32px; height: 32px; background: url(anim-rgb.gif)'></div>
         <ul style='margin: 0; padding-left: 40px; list-style-image: url(anim-rgb.png)'>
           <li style='line-height: 16px'>&nbsp;</ul>",
        manual(),
    );
    // (a background tile, and one of a repeated background)
    let pts = [(5, 5), (8, 24), (24, 40)];
    let backgrounds = |r: &mut Renderer, ms| {
        let px = at(r, ms, &pts);
        (px[0], px[1], px[2])
    };
    let white = Color::rgb(255, 255, 255);
    assert_eq!(backgrounds(&mut r, 50), (RED, RED, RED));
    assert_eq!(backgrounds(&mut r, 150), (white, LIME, LIME));
    assert_eq!(backgrounds(&mut r, 400), (white, BLUE, BLUE));
    // (the marker sits left of the item, somewhere in its row)
    let row = |r: &mut Renderer, ms| {
        r.set_timeline_clock(TimelineClock::Manual(Duration::from_millis(ms)));
        r.update_rendering(VIEWPORT).unwrap();
        let p = r.rasterize(VIEWPORT, Point::ZERO, 1.0).unwrap();
        (0..40).map(|x| p.pixel(x, 56)).collect::<Vec<_>>()
    };
    // (the timeline never moves back: the APNG, playing twice, is on its third frame, then its
    // second play)
    let blue = row(&mut r, 450);
    assert!(
        blue.contains(&BLUE),
        "list marker on the third frame: {blue:?}"
    );
    let red = row(&mut r, 650);
    assert!(
        red.contains(&RED),
        "list marker on the first frame: {red:?}"
    );
}

#[test]
fn frames_are_due_at_their_delays_and_only_in_view() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = load(&net, "<img src=anim-rgb.png>", manual());
    at(&mut r, 0, &[]);
    let all = None;
    let view = Some(Rect::new(0.0, 0.0, 200.0, 100.0));
    let elsewhere = Some(Rect::new(0.0, 500.0, 200.0, 100.0));
    let hidden = Some(Rect::ZERO);
    assert!(!r.image_frame_due(all));
    r.set_timeline_clock(TimelineClock::Manual(Duration::from_millis(100)));
    assert!(r.image_frame_due(all));
    assert!(r.image_frame_due(view));
    assert!(!r.image_frame_due(elsewhere));
    assert!(!r.image_frame_due(hidden));
    // (a manual clock does not move by itself: no wake-up time)
    assert_eq!(r.next_image_frame(view), None);
    at(&mut r, 100, &[]);
    assert!(!r.image_frame_due(view));
    // after the last play nothing is due
    at(&mut r, 1250, &[]);
    r.set_timeline_clock(TimelineClock::Manual(Duration::from_secs(100)));
    assert!(!r.image_frame_due(all));
}

#[test]
fn a_browser_clock_starts_animations_at_their_first_paint() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = load(&net, "<img src=anim-rgb.gif>", TimelineClock::Monotonic);
    let view = Some(Rect::new(0.0, 0.0, 200.0, 100.0));
    assert!(r.update_rendering(VIEWPORT).is_some());
    // not painted yet: nothing to wake up for
    assert_eq!(r.next_image_frame(view), None);
    r.paint(VIEWPORT, Point::ZERO);
    let next = r
        .next_image_frame(view)
        .expect("a wake-up for the second frame");
    let wait = next.saturating_duration_since(std::time::Instant::now());
    assert!(wait <= Duration::from_millis(100), "{wait:?}");
    // scrolled out of view: paused
    r.paint(VIEWPORT, Point::new(0.0, 50.0));
    assert!(r.next_image_frame(view).is_some());
    assert_eq!(
        r.next_image_frame(Some(Rect::new(0.0, 400.0, 200.0, 100.0))),
        None
    );
}

/// A GIF of `colors.len()` frames of 100 ms looping forever on a `size` × `size` canvas: the
/// first frame fills it with `colors[0]`, the others paint the top left 100 × 50 px. The LZW data
/// is "uncompressed": 8-bit codes with a clear code every 100 pixels, so the table never grows.
fn big_gif(size: u16, colors: &[[u8; 3]]) -> Vec<u8> {
    let mut g = b"GIF89a".to_vec();
    g.extend_from_slice(&size.to_le_bytes());
    g.extend_from_slice(&size.to_le_bytes());
    // a global color table of 128 entries (LZW minimum code size 7, codes of 8 bits)
    g.extend_from_slice(&[0x86, 0, 0]);
    for i in 0..128 {
        g.extend_from_slice(colors.get(i).unwrap_or(&[0, 0, 0]));
    }
    g.extend_from_slice(b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00");
    for (i, _) in colors.iter().enumerate() {
        let (w, h) = if i == 0 { (size, size) } else { (100, 50) };
        // graphic control: keep, 100 ms
        g.extend_from_slice(&[0x21, 0xF9, 4, 0x04, 10, 0, 0, 0]);
        g.push(0x2C);
        for v in [0u16, 0, w, h] {
            g.extend_from_slice(&v.to_le_bytes());
        }
        g.push(0);
        let mut codes = Vec::new();
        for p in 0..w as usize * h as usize {
            if p % 100 == 0 {
                codes.push(128);
            }
            codes.push(i as u8);
        }
        codes.push(129);
        g.push(7);
        for block in codes.chunks(255) {
            g.push(block.len() as u8);
            g.extend_from_slice(block);
        }
        g.push(0);
    }
    g.push(0x3B);
    g
}

#[test]
fn big_animations_decode_frames_as_they_show_while_in_view() {
    let colors = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [0, 255, 255],
        [255, 0, 255],
    ];
    // 6 frames of 1200 × 1200: 34.6 MB composited, more than the 32 MB kept
    let gif = big_gif(1200, &colors);
    let image = snowlark_paint::image::decode(&gif).unwrap();
    assert_eq!(image.frames.len(), 1);
    assert!(image.animation.as_ref().unwrap().encoded.is_some());
    let dir = std::env::temp_dir().join(format!("snowlark-anim-big-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("big.gif"), &gif).unwrap();
    let html = "<!DOCTYPE html><style>body { margin: 0 }</style><img src=big.gif>";
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            scripting: false,
            ..Default::default()
        },
    );
    r.set_timeline_clock(manual());
    let url = Url::from_file_path(dir.join("page.html")).unwrap();
    r.load_html(html.as_bytes(), &url, None);
    assert!(r.render_until_settled(Duration::from_secs(20)).complete);
    let color = |c: [u8; 3]| Color::rgb(c[0], c[1], c[2]);
    let pt = [(10, 10), (150, 80)];
    let view = Rect::new(0.0, 0.0, VIEWPORT.width, VIEWPORT.height);
    r.set_image_animation_view(Some(view));
    assert_eq!(at(&mut r, 0, &pt), [color(colors[0]); 2]);
    assert_eq!(at(&mut r, 150, &pt)[0], color(colors[1]));
    assert_eq!(at(&mut r, 250, &pt)[0], color(colors[2]));
    // hidden: no frame is decoded
    r.set_image_animation_view(Some(Rect::ZERO));
    assert_eq!(at(&mut r, 350, &pt)[0], color(colors[2]));
    r.set_image_animation_view(Some(view));
    assert_eq!(at(&mut r, 450, &pt)[0], color(colors[4]));
    // the next play starts over from the first frame
    assert_eq!(at(&mut r, 650, &pt), [color(colors[0]); 2]);
    assert_eq!(at(&mut r, 760, &pt), [color(colors[1]), color(colors[0])]);
    std::fs::remove_dir_all(&dir).ok();
}
