// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark [<url>]` on Linux until the browser's own window comes there: one page in a window
//! ([`snowlark_paint::window`] on Wayland), its pointer, wheel and keys given to the page, painted on
//! the CPU at the window's scale. No tabs, toolbar or address bar: the page follows its own links.
//! The page runs on the window's thread between its frames (a 16 ms timer).

use std::time::{Duration, Instant};

use snowlark_net::NetConfig;
use snowlark_page::keyboard::KeyInput;
use snowlark_page::{Page, PageOptions};
use snowlark_paint::window::{self, keys, Event, HitArea, MouseButton, Window, WindowHandler};
use snowlark_paint::{Point, Size};
use url::Url;

const FRAME: usize = 1;

/// A wheel notch, in CSS px (three lines of 16 px, as on Windows).
const NOTCH: f32 = 48.0;

pub fn run(start: Option<&str>) -> i32 {
    let url = match start.map(target) {
        Some(Some(u)) => u,
        Some(None) => {
            eprintln!("not an address or a file: {}", start.unwrap_or_default());
            return 2;
        }
        None => Url::parse("about:blank").unwrap(),
    };
    let net = match snowlark_tab::script_check::start_net(NetConfig::from_env()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("network thread: {e}");
            return 1;
        }
    };
    let options = window::WindowOptions {
        title: "Snowlark".into(),
        ..Default::default()
    };
    let client = net.client();
    let result = window::run(options, move |w| {
        let mut page = Page::new(PageOptions {
            net: Some(client),
            viewport: (1280, 860),
            render: snowlark_page::RenderSetup::new().ok(),
            ..Default::default()
        });
        page.navigate(url);
        w.set_timer(FRAME, 16);
        Box::new(Viewer {
            page,
            size: Size::new(1280.0, 860.0),
            device: (0, 0),
            scale: w.dpi() as f32 / 96.0,
            scroll: Point::new(0.0, 0.0),
            dirty: true,
            last_key: None,
        })
    });
    drop(net);
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("the window: {e}");
            1
        }
    }
}

/// What the command line names: an address, or a file.
fn target(text: &str) -> Option<Url> {
    if let Ok(u) = Url::parse(text) {
        if u.scheme().len() > 1 {
            return Some(u);
        }
    }
    let path = std::path::Path::new(text);
    if path.exists() {
        return Url::from_file_path(std::fs::canonicalize(path).ok()?).ok();
    }
    Url::parse(&format!("https://{text}")).ok()
}

struct Viewer {
    page: Page,
    /// The viewport in CSS px, and the window's client area in device pixels.
    size: Size,
    device: (u32, u32),
    scale: f32,
    scroll: Point,
    /// Something changed that the page's rendering does not know of (size, scroll).
    dirty: bool,
    /// The key last pressed, for the characters it types.
    last_key: Option<KeyInput>,
}

impl Viewer {
    fn css(&self, x: i32, y: i32) -> Point {
        Point::new(x as f32 / self.scale, y as f32 / self.scale)
    }

    fn scroll_to(&mut self, to: Point) {
        let height = self
            .page
            .engine()
            .with_renderer(|r, _| r.document_height())
            .unwrap_or(0.0);
        let max = (height - self.size.height).max(0.0);
        let to = Point::new(0.0, to.y.clamp(0.0, max));
        if to != self.scroll {
            self.scroll = to;
            self.page.engine().viewport_scrolled(to);
            self.dirty = true;
        }
    }

    fn scroll_by(&mut self, dy: f32) {
        self.scroll_to(Point::new(0.0, self.scroll.y + dy));
    }

    /// Runs the page until its next frame, then shows the frame if anything changed.
    fn frame(&mut self, window: &Window) {
        self.page
            .run_until(Instant::now() + Duration::from_millis(8), |_| false);
        if let Some(to) = self.page.engine().take_scroll_request() {
            self.scroll_to(to);
        }
        let changed = self.page.update_the_rendering();
        if !(changed || self.dirty) || self.device.0 == 0 {
            return;
        }
        self.dirty = false;
        let (size, scroll, scale) = (self.size, self.scroll, self.scale);
        let pixmap = self.page.engine().with_renderer(|r, _| {
            if r.fragments().is_none() {
                r.layout(size)?;
            }
            r.rasterize(size, scroll, scale).ok()
        });
        let Some(Some(pixmap)) = pixmap else {
            return;
        };
        let (w, h) = self.device;
        let result = window.present(w, h, |pixels, stride| {
            let row = (w.min(pixmap.width) * 4) as usize;
            for y in 0..h.min(pixmap.height) as usize {
                let src = y * pixmap.width as usize * 4;
                pixels[y * stride..y * stride + row].copy_from_slice(&pixmap.data[src..src + row]);
            }
        });
        if let Err(e) = result {
            eprintln!("[viewer] present: {e}");
        }
    }
}

/// The window's modifiers as the page's.
fn page_mods(m: window::Modifiers) -> snowlark_page::input::Modifiers {
    snowlark_page::input::Modifiers {
        ctrl: m.ctrl,
        shift: m.shift,
        alt: m.alt,
        meta: m.meta,
    }
}

fn button_code(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

impl WindowHandler for Viewer {
    fn event(&mut self, window: &Window, event: Event) -> bool {
        match event {
            Event::Resized { width, height, .. } => {
                self.device = (width, height);
                self.size = Size::new(width as f32 / self.scale, height as f32 / self.scale);
                self.page.engine().set_viewport(self.size);
                self.dirty = true;
                self.frame(window);
            }
            Event::DpiChanged(dpi) => {
                self.scale = dpi as f32 / 96.0;
                self.dirty = true;
            }
            Event::Timer(FRAME) => self.frame(window),
            Event::MouseMove { x, y, mods } => {
                let p = self.css(x, y);
                self.page.pointer_moved(p, page_mods(mods));
            }
            Event::MouseLeave => self.page.pointer_left(),
            Event::MouseDown {
                x,
                y,
                button,
                mods,
                clicks,
            } => {
                let p = self.css(x, y);
                self.page.pointer_pressed(p, button_code(button), page_mods(mods), clicks);
            }
            Event::MouseUp { x, y, button, mods } => {
                let p = self.css(x, y);
                self.page.pointer_released(p, button_code(button), page_mods(mods), 1);
            }
            Event::Wheel {
                x,
                y,
                delta_x,
                delta_y,
                mods,
                ..
            } => {
                let p = self.css(x, y);
                let delta = Point::new(
                    delta_x as f32 / 120.0 * NOTCH,
                    -delta_y as f32 / 120.0 * NOTCH,
                );
                self.page.wheel(p, delta, page_mods(mods));
                self.scroll_by(delta.y);
            }
            Event::KeyDown {
                key,
                scan,
                mods,
                repeat,
                text,
            } => {
                let input = KeyInput {
                    vk: key,
                    scan,
                    text,
                    mods: page_mods(mods),
                    repeat,
                    paste: None,
                    paste_html: None,
                };
                let outcome = self.page.key_down(&input);
                self.last_key = Some(input);
                if outcome.unhandled {
                    let page = self.size.height * 0.875;
                    match key {
                        keys::DOWN => self.scroll_by(40.0),
                        keys::UP => self.scroll_by(-40.0),
                        keys::PAGE_DOWN | keys::SPACE if !mods.shift => self.scroll_by(page),
                        keys::PAGE_UP | keys::SPACE => self.scroll_by(-page),
                        keys::HOME if mods.ctrl => self.scroll_to(Point::new(0.0, 0.0)),
                        keys::END if mods.ctrl => self.scroll_to(Point::new(0.0, f32::MAX)),
                        _ => return false,
                    }
                }
                return true;
            }
            Event::Char(c) => {
                if let Some(input) = self.last_key.clone() {
                    self.page.key_char(c, &input);
                }
            }
            Event::KeyUp { key, scan, mods } => {
                self.page.key_up(&KeyInput {
                    vk: key,
                    scan,
                    text: None,
                    mods: page_mods(mods),
                    repeat: false,
                    paste: None,
                    paste_html: None,
                });
            }
            Event::CloseRequested => window.close(),
            _ => return false,
        }
        true
    }

    fn hit_test(&mut self, _: &Window, _: i32, _: i32) -> HitArea {
        HitArea::Client
    }
}
