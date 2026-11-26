// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The compositor's tests: a frame carries the chrome over the content, tiles land where the
//! scroll puts them, a layer's state moves and fades its tiles, and the window's frame slot paces
//! the frames.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::display_list::DisplayListBuilder;
use crate::gpu::{Driver, TileRasterizer};
use crate::image::ImageStore;
use crate::raster::Viewport;
use crate::text::TextEngine;
use crate::window::FrameSlot;

const WHITE: Color = Color::rgb(255, 255, 255);
const RED: Color = Color::rgb(255, 0, 0);
const BLUE: Color = Color::rgb(0, 0, 255);
const GRAY: Color = Color::rgb(200, 200, 200);

/// Window ids no real window has.
static NEXT: AtomicU64 = AtomicU64::new(u64::MAX - 1000);

fn compositor(width: u32, height: u32, chrome: u32) -> (Compositor, Arc<FrameSlot>) {
    let (window, slot) = FrameSlot::for_test(NEXT.fetch_add(1, Ordering::Relaxed));
    let gpu = Gpu::with_driver(Driver::Warp).unwrap();
    let text = TextEngine::new().unwrap();
    let c = Compositor::new(gpu, &text, window, width, height, chrome).unwrap();
    (c, slot)
}

/// A page frame: red above y = 100, blue below, `width` × `height`.
fn page(width: u32, height: u32) -> TiledFrame {
    let mut b = DisplayListBuilder::new();
    b.fill_rect(Rect::new(0.0, 0.0, width as f32, 100.0), RED);
    b.fill_rect(Rect::new(0.0, 100.0, width as f32, height as f32), BLUE);
    let list = b.build();
    let text = TextEngine::new().unwrap();
    let images = ImageStore::default();
    let mut r = TileRasterizer::new(Gpu::with_driver(Driver::Warp).unwrap()).unwrap();
    r.raster(
        &list,
        Sources {
            text: &text,
            images: &images,
        },
        &Viewport {
            width,
            height,
            scale: 1.0,
            background: WHITE,
        },
    )
    .unwrap()
}

fn px(frame: &(u32, u32, Vec<u8>), x: u32, y: u32) -> [u8; 4] {
    let i = (y * frame.0 + x) as usize * 4;
    frame.2[i..i + 4].try_into().unwrap()
}

/// BGRA of an opaque color.
fn bgra(c: Color) -> [u8; 4] {
    [c.b, c.g, c.r, 255]
}

#[test]
fn a_frame_shows_the_chrome_over_the_scrolled_tiles() {
    let (mut c, slot) = compositor(300, 240, 40);
    assert_eq!(c.content_size(), (300, 200));
    let text = TextEngine::new().unwrap();
    let images = ImageStore::default();
    let sources = Sources {
        text: &text,
        images: &images,
    };
    c.draw_chrome(&DisplayList::default(), sources, 1.0, GRAY)
        .unwrap();
    let frame = page(300, 600);
    c.draw_content(|pass| {
        pass.clear(WHITE);
        // scrolled down by 50: the red band's last 50 rows show at the top
        pass.tiles(&frame, 0, -50, Rect::new(0.0, 0.0, 300.0, 200.0));
    })
    .unwrap();
    assert!(c.wait_for_frame(Duration::from_millis(1)));
    c.present().unwrap();
    let shown = slot.take_frame().expect("a frame for the window");
    assert_eq!((shown.0, shown.1), (300, 240));
    assert_eq!(px(&shown, 10, 10), bgra(GRAY));
    assert_eq!(px(&shown, 10, 39), bgra(GRAY));
    assert_eq!(px(&shown, 10, 40), bgra(RED));
    assert_eq!(px(&shown, 299, 89), bgra(RED));
    assert_eq!(px(&shown, 10, 90), bgra(BLUE));
    assert_eq!(px(&shown, 150, 239), bgra(BLUE));
    // Until the display asks for the next frame (a frame callback), the compositor waits.
    assert!(!c.wait_for_frame(Duration::from_millis(5)));
    slot.set_ready();
    assert!(c.wait_for_frame(Duration::from_millis(5)));
}

#[test]
fn tiles_through_a_state_move_clip_and_fade() {
    let (mut c, slot) = compositor(200, 200, 0);
    let frame = page(100, 100);
    let state = LayerState(vec![
        StateOp::Clip(Rect::new(0.0, 0.0, 150.0, 150.0)),
        StateOp::Transform([1.0, 0.0, 0.0, 1.0, 120.0, 30.0]),
        StateOp::Opacity(0.5),
    ]);
    c.draw_content(|pass| {
        pass.clear(WHITE);
        pass.tiles_through(
            &frame,
            (0, 0),
            &state,
            1.0,
            (0, 0),
            Rect::new(0.0, 0.0, 200.0, 200.0),
        );
    })
    .unwrap();
    c.present().unwrap();
    let shown = slot.take_frame().unwrap();
    // moved by (120, 30), half transparent over white, cut at x = 150
    assert_eq!(px(&shown, 119, 40), bgra(WHITE));
    let half_red = px(&shown, 130, 40);
    assert!((half_red[2] as i32 - 255).abs() <= 1 && (half_red[1] as i32 - 128).abs() <= 2);
    assert_eq!(px(&shown, 151, 40), bgra(WHITE));
}

#[test]
fn a_backdrop_filter_reads_what_is_under_it() {
    let (mut c, slot) = compositor(100, 100, 0);
    let backdrop = crate::display_list::LayerBackdrop {
        filters: vec![crate::filter::FilterOp::Invert(1.0)],
        rect: Rect::new(10.0, 10.0, 30.0, 30.0),
        radii: Radii::ZERO,
        opacity: 1.0,
    };
    c.draw_content(|pass| {
        pass.clear(WHITE);
        pass.backdrop(&backdrop, 1.0, (0, 0), Rect::new(0.0, 0.0, 100.0, 100.0));
    })
    .unwrap();
    c.present().unwrap();
    let shown = slot.take_frame().unwrap();
    assert_eq!(px(&shown, 20, 20), [0, 0, 0, 255]);
    assert_eq!(px(&shown, 50, 50), bgra(WHITE));
}
