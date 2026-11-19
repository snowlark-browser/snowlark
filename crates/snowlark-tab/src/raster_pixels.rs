// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pixels a tab's raster thread makes, byte for byte. Every frame here goes the way a tab's does:
//! a [`PaintedFrame`] posted to a [`RasterThread`], which draws it with a `TileRasterizer` on a real
//! `Gpu` and sends the frame to the compositor's channel, where it is read back and handed back to
//! be recycled once the next frame has come, as the compositor does (so tiles are reused too).
//!
//! The frames cover what a list takes from the page's world: text in system fonts (real and simulated
//! faces, a face of a collection, named instances of a variable font, color glyphs, fallback, bidi),
//! text in web fonts, images (and everything that changes one under its id), gradients, groups,
//! layers and the patches over a video, at 100% and 150%, on the graphics card and on WARP.
//!
//! Without settings the test proves the frames are the same in two runs and not blank (see
//! [`TRIES`] for what "the same" can take). Pixels can
//! only be compared with an earlier build by saving them: with `SNOWLARK_PIXEL_SAVE=<folder>` a run
//! stores every frame there, and with `SNOWLARK_PIXEL_BASELINE=<folder>` it compares every frame with
//! the stored one and fails on any byte that differs. The fonts are the machine's, so the stored
//! frames are good for the machine that made them only and stay out of the repository.
//!
//! Only one named instance of a variable font file appears in a run: `TextEngine` gives every instance
//! of a file one `FontId`, and which fonts then get which ids changes from run to run.

use std::path::Path;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use snowlark_paint::{
    Border, BorderStyle, BoxShadow, Color, DecodedImage, Direction, DisplayList,
    DisplayListBuilder, Driver, Extend, FontId, FontQuery, FontStretch, FontStyle, Frame, Gpu,
    GradientStop, ImageId, ImageRendering, ImageStore, LayerKind, Matrix, Point, Radii, Rect,
    ShapeOptions, ShapedText, Sides, Size, Sources, StickyConstraint, TextEngine, TileRasterizer,
    TiledFrame, VideoId, Viewport,
};

use crate::comp::{CompInput, FrameData};
use crate::raster_thread::{FrameMeta, PaintedFrame, RasterThread};

/// How many times a comparison is made before a difference counts: text in a system font comes out
/// a few pixels apart in about one run in a hundred, by itself. (Measured before any of the seam
/// existed: two runs of the same frames in one process differed in 2 of 250 processes, in the
/// system fonts' frame only.) A real difference is there every time.
const TRIES: usize = 5;

const WIDTH: f32 = 760.0;
const HEIGHT: f32 = 620.0;

/// One read-back frame.
#[derive(PartialEq)]
struct Shot {
    name: String,
    width: u32,
    height: u32,
    data: Vec<u8>,
}

/// What a page holds: its text engine with its web fonts, and its images.
struct Page {
    text: Arc<TextEngine>,
    images: Arc<ImageStore>,
    /// Internal family names of the web fonts.
    web: Vec<String>,
}

/// A pattern of premultiplied BGRA pixels; `seed` makes each image different. Without `alpha` every
/// pixel is opaque.
fn pattern(width: u32, height: u32, seed: u32, alpha: bool) -> Frame {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let v = x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503) ^ seed.wrapping_mul(97);
            let a = if alpha {
                ((x * 255) / width.max(1)) as u8
            } else {
                255
            };
            let channel = |c: u32| ((c & 0xff) * a as u32 / 255) as u8;
            let smooth = (x * 200 / width.max(1) + y * 55 / height.max(1) + seed * 31) & 0xff;
            pixels.extend_from_slice(&[
                channel(smooth),
                channel((v >> 8) & 0x3f | (y * 255 / height.max(1))),
                channel(255 - smooth),
                a,
            ]);
        }
    }
    Frame {
        width,
        height,
        pixels,
        delay_ms: 0,
    }
}

fn still(width: u32, height: u32, seed: u32, alpha: bool) -> DecodedImage {
    DecodedImage::still(pattern(width, height, seed, alpha))
}

/// A line of text in a font list, as computed style asks for it.
struct Line<'a> {
    families: &'a [&'a str],
    weight: u16,
    style: FontStyle,
    size: f32,
    direction: Direction,
    color: Color,
}

impl Line<'_> {
    fn draw(&self, page: &Page, b: &mut DisplayListBuilder, s: &str, at: Point) {
        let list = page
            .text
            .font_list(&FontQuery {
                families: self.families,
                weight: self.weight,
                style: self.style,
                stretch: FontStretch::NORMAL,
            })
            .unwrap();
        let mut shaped = ShapedText::default();
        let options = ShapeOptions {
            size: self.size,
            direction: self.direction,
            ..Default::default()
        };
        page.text.shape(s, list, &options, &mut shaped).unwrap();
        shaped.paint(0..shaped.clusters.len(), at, self.color, b);
    }
}

fn plain<'a>(families: &'a [&'a str], size: f32) -> Line<'a> {
    Line {
        families,
        weight: 400,
        style: FontStyle::Normal,
        size,
        direction: Direction::Ltr,
        color: Color::rgb(20, 24, 32),
    }
}

fn system_text(page: &Page, b: &mut DisplayListBuilder) {
    let pangram = "Sphinx of black quartz, judge my vow 0123456789";
    let hello =
        "\u{645}\u{631}\u{62D}\u{628}\u{627} \u{628}\u{627}\u{644}\u{639}\u{627}\u{644}\u{645}";
    let rows = [
        (plain(&["Segoe UI"], 16.0), pangram),
        (plain(&["Arial"], 13.0), pangram),
        (
            Line {
                weight: 700,
                ..plain(&["Arial"], 13.0)
            },
            pangram,
        ),
        (
            Line {
                style: FontStyle::Italic,
                ..plain(&["Times New Roman"], 20.0)
            },
            pangram,
        ),
        (plain(&["Consolas"], 14.0), pangram),
        // (faces with no bold or italic of their own: DirectWrite simulates them)
        (
            Line {
                weight: 700,
                ..plain(&["Impact"], 18.0)
            },
            "Simulated bold",
        ),
        (
            Line {
                style: FontStyle::Italic,
                ..plain(&["Impact"], 18.0)
            },
            "Simulated oblique",
        ),
        // (named instances of variable font files: one of each, see the module's note)
        (
            Line {
                weight: 700,
                ..plain(&["Bahnschrift"], 18.0)
            },
            "Bahnschrift bold",
        ),
        (plain(&["Segoe UI Variable Text"], 18.0), pangram),
        // (a later face of a font collection file)
        (
            plain(&["MS PGothic"], 18.0),
            "MS PGothic \u{65E5}\u{672C}\u{8A9E}",
        ),
        // (fallback from a font without these characters)
        (
            plain(&["Arial"], 18.0),
            "\u{4E2D}\u{6587} \u{D55C}\u{AD6D}\u{C5B4}",
        ),
        (
            Line {
                direction: Direction::Rtl,
                ..plain(&["Segoe UI"], 20.0)
            },
            hello,
        ),
        // (color glyphs, and color glyphs in translucent ink)
        (
            plain(&["Segoe UI Emoji"], 24.0),
            "\u{1F600}\u{1F680}\u{2764}\u{FE0F} text",
        ),
        (
            Line {
                color: Color::rgba(0, 0, 0, 128),
                ..plain(&["Segoe UI Emoji"], 24.0)
            },
            "\u{1F44D}\u{1F3FD} faint",
        ),
    ];
    let mut y = 24.0;
    for (line, s) in &rows {
        line.draw(page, b, s, Point::new(10.0, y));
        y += line.size + 11.0;
    }
}

fn web_text(page: &Page, b: &mut DisplayListBuilder) {
    let pangram = "Sphinx of black quartz, judge my vow 0123456789";
    let mut y = 30.0;
    for (i, family) in page.web.iter().enumerate() {
        let f: &[&str] = &[family.as_str()];
        let rows = [
            Line {
                weight: 700,
                ..plain(f, 19.0)
            },
            plain(f, 15.0),
            Line {
                style: FontStyle::Italic,
                ..plain(f, 23.0)
            },
        ];
        // (the last web font is a variable one: one named instance of it)
        let rows = if i + 1 == page.web.len() {
            &rows[..1]
        } else {
            &rows[..]
        };
        for line in rows {
            line.draw(page, b, pangram, Point::new(10.0, y));
            y += line.size + 12.0;
        }
    }
    // (a web font in a family list that falls back)
    let f: &[&str] = &[page.web[0].as_str(), "Arial"];
    let s = "Web \u{4E2D}\u{6587} fallback";
    plain(f, 18.0).draw(page, b, s, Point::new(10.0, y + 12.0));
}

fn gradients(b: &mut DisplayListBuilder) {
    let stops = [
        GradientStop {
            offset: 0.0,
            color: Color::rgb(230, 30, 60),
        },
        GradientStop {
            offset: 0.4,
            color: Color::rgba(40, 200, 90, 160),
        },
        GradientStop {
            offset: 1.0,
            color: Color::rgb(20, 40, 220),
        },
    ];
    let hard = [
        GradientStop {
            offset: 0.5,
            color: Color::rgb(250, 200, 0),
        },
        GradientStop {
            offset: 0.5,
            color: Color::rgb(0, 60, 80),
        },
    ];
    let cell = |i: f32, j: f32| Rect::new(10.0 + i * 185.0, 10.0 + j * 150.0, 175.0, 140.0);
    let round = Radii::uniform(26.0);
    let r = cell(0.0, 0.0);
    let (from, to) = (Point::new(r.x, r.y), Point::new(r.right(), r.bottom()));
    b.linear_gradient(r, Radii::ZERO, from, to, &stops, Extend::Clamp);
    let r = cell(1.0, 0.0);
    let (from, to) = (Point::new(r.x, r.y), Point::new(r.x + 30.0, r.y + 12.0));
    b.linear_gradient(r, round, from, to, &stops, Extend::Repeat);
    let r = cell(2.0, 0.0);
    let (from, to) = (Point::new(r.x, r.y), Point::new(r.right(), r.y));
    b.linear_gradient(r, Radii::ZERO, from, to, &hard, Extend::Clamp);
    let r = cell(3.0, 0.0);
    let middle = Point::new(r.x + 80.0, r.y + 60.0);
    b.radial_gradient(
        r,
        round,
        middle,
        Size::new(90.0, 50.0),
        &stops,
        Extend::Clamp,
    );
    let r = cell(0.0, 1.0);
    let middle = Point::new(r.x + 80.0, r.y + 60.0);
    let ring = Size::new(24.0, 24.0);
    b.radial_gradient(r, Radii::ZERO, middle, ring, &stops, Extend::Repeat);
    let r = cell(1.0, 1.0);
    let middle = Point::new(r.x + 80.0, r.y + 60.0);
    b.conic_gradient(r, Radii::ZERO, middle, 0.7, &stops, Extend::Clamp);
    let r = cell(2.0, 1.0);
    let middle = Point::new(r.x + 60.0, r.y + 90.0);
    b.conic_gradient(r, round, middle, 0.0, &hard, Extend::Repeat);
    // Shadows, borders and groups.
    let r = cell(3.0, 1.0);
    b.box_shadow(BoxShadow {
        rect: r,
        radii: round,
        offset: Point::new(5.0, 7.0),
        blur: 14.0,
        spread: 2.0,
        color: Color::rgba(0, 0, 0, 170),
        inset: false,
    });
    b.border(Border {
        rect: r,
        radii: round,
        widths: Sides {
            top: 6.0,
            right: 3.0,
            bottom: 6.0,
            left: 9.0,
        },
        colors: [
            Color::rgb(200, 0, 0),
            Color::rgb(0, 150, 0),
            Color::rgb(0, 0, 200),
            Color::rgb(90, 90, 0),
        ],
        styles: [
            BorderStyle::Solid,
            BorderStyle::Dashed,
            BorderStyle::Dotted,
            BorderStyle::Double,
        ],
    });
    let r = cell(0.0, 2.0);
    b.push_opacity(0.6, r);
    b.push_clip_rounded_rect(r, round);
    b.fill_rect(r, Color::rgb(0, 120, 200));
    b.pop_clip();
    b.pop_opacity();
    let r = cell(1.0, 2.0);
    b.push_mask(r, true);
    let (from, to) = (Point::new(r.x, r.y), Point::new(r.right(), r.y));
    let fade = [
        GradientStop {
            offset: 0.0,
            color: Color::WHITE,
        },
        GradientStop {
            offset: 1.0,
            color: Color::BLACK,
        },
    ];
    b.linear_gradient(r, Radii::ZERO, from, to, &fade, Extend::Clamp);
    b.mask_content();
    b.fill_rect(r, Color::rgb(180, 30, 120));
    b.pop_mask();
    let r = cell(2.0, 2.0);
    b.push_transform(turned(0.3, Point::new(r.x + 80.0, r.y + 70.0)));
    let (from, to) = (Point::new(r.x, r.y), Point::new(r.right(), r.bottom()));
    b.linear_gradient(r, Radii::ZERO, from, to, &stops, Extend::Clamp);
    b.pop_transform();
}

/// A rotation by `angle` radians about `center`.
fn turned(angle: f32, center: Point) -> Matrix {
    let (sin, cos) = angle.sin_cos();
    Matrix::translation(-center.x, -center.y)
        .then(&Matrix::new(cos, sin, -sin, cos, 0.0, 0.0))
        .then(&Matrix::translation(center.x, center.y))
}

/// The images of [`images`] by their part in it.
struct Pictures {
    opaque: ImageId,
    faded: ImageId,
    tiny: ImageId,
    animated: ImageId,
    decoded_as_shown: ImageId,
}

fn pictures(store: &ImageStore, seed: u32) -> Pictures {
    Pictures {
        opaque: store.add(still(300, 200, seed, false)),
        faded: store.add(still(128, 128, seed + 1, true)),
        tiny: store.add(still(5, 4, seed + 2, false)),
        animated: store.add(DecodedImage {
            frames: (0..3)
                .map(|i| pattern(90, 90, seed + 3 + i, false))
                .collect(),
            orientation: 1,
            animation: None,
        }),
        decoded_as_shown: store.add(still(70, 50, seed + 7, false)),
    }
}

fn images(p: &Pictures, b: &mut DisplayListBuilder) {
    use ImageRendering::{Pixelated, Smooth};
    let at = Rect::new;
    b.fill_rect(at(0.0, 0.0, WIDTH, HEIGHT), Color::rgb(240, 235, 225));
    b.image(p.opaque, at(10.0, 10.0, 300.0, 200.0), None, 1.0, Smooth);
    b.image(p.opaque, at(330.0, 10.0, 137.0, 93.0), None, 1.0, Smooth);
    b.image(p.opaque, at(480.0, 10.0, 262.0, 201.0), None, 0.7, Smooth);
    let part = Some(Rect::new(40.0, 30.0, 100.0, 80.0));
    b.image(p.opaque, at(330.0, 110.0, 130.0, 104.0), part, 1.0, Smooth);
    b.image(p.faded, at(10.0, 220.0, 128.0, 128.0), None, 1.0, Smooth);
    b.image(p.faded, at(150.0, 220.0, 200.0, 170.0), None, 0.5, Smooth);
    b.image(p.tiny, at(360.0, 220.0, 150.0, 120.0), None, 1.0, Pixelated);
    b.image(p.tiny, at(520.0, 220.0, 150.0, 120.0), None, 1.0, Smooth);
    b.image(p.animated, at(10.0, 400.0, 90.0, 90.0), None, 1.0, Smooth);
    let shown = p.decoded_as_shown;
    b.image(shown, at(120.0, 400.0, 70.0, 50.0), None, 1.0, Smooth);
    b.push_transform(turned(0.4, Point::new(300.0, 480.0)));
    b.image(p.opaque, at(220.0, 420.0, 150.0, 100.0), None, 1.0, Smooth);
    b.pop_transform();
    // (an image inside a mask, as an SVG or `mask-image` paints one)
    let r = at(420.0, 400.0, 128.0, 128.0);
    b.push_mask(r, false);
    b.image(p.faded, r, None, 1.0, Smooth);
    b.mask_content();
    b.fill_rect(r, Color::rgb(20, 90, 200));
    b.pop_mask();
}

/// Fixed and sticky layers (grayscale text, images) and a video with content over it.
fn layers(page: &Page, p: &Pictures, b: &mut DisplayListBuilder) {
    let ui = plain(&["Segoe UI"], 15.0);
    let page_area = Rect::new(0.0, 0.0, WIDTH, HEIGHT);
    b.fill_rect(page_area, Color::rgb(250, 250, 244));
    ui.draw(page, b, "Under the layers", Point::new(12.0, 240.0));
    let video = Rect::new(380.0, 240.0, 320.0, 180.0);
    b.video(VideoId(7), Some(p.opaque), video, video);
    let box_over = Rect::new(420.0, 300.0, 90.0, 40.0);
    b.fill_rect(box_over, Color::rgba(250, 20, 20, 200));
    ui.draw(page, b, "Over the video", Point::new(430.0, 400.0));

    b.push_layer(LayerKind::Fixed);
    b.fill_rect(
        Rect::new(20.0, 20.0, 480.0, 60.0),
        Color::rgba(10, 60, 120, 230),
    );
    let header = Line {
        weight: 600,
        color: Color::WHITE,
        ..plain(&["Segoe UI"], 18.0)
    };
    header.draw(page, b, "Fixed header text", Point::new(34.0, 56.0));
    let icon = Rect::new(420.0, 24.0, 52.0, 52.0);
    b.image(p.faded, icon, None, 1.0, ImageRendering::Smooth);
    b.pop_layer();

    let bar = Rect::new(30.0, 120.0, 300.0, 70.0);
    b.push_layer(LayerKind::Sticky(StickyConstraint {
        rect: bar,
        container: page_area,
        insets: [Some(0.0), None, None, None],
    }));
    b.fill_rect(bar, Color::rgb(255, 230, 120));
    let web: &[&str] = &[page.web[0].as_str()];
    plain(web, 18.0).draw(page, b, "Sticky web font", Point::new(42.0, 160.0));
    b.pop_layer();
}

/// The frames of `gpu`'s raster thread, with the compositor's end of its channel.
struct Pipe {
    raster: RasterThread,
    frames: Receiver<CompInput>,
    reader: TileRasterizer,
    /// The frame on show: recycled once the next one arrives.
    shown: Option<Box<FrameData>>,
    scale: f32,
    generation: u64,
    shots: Vec<Shot>,
}

impl Pipe {
    fn new(gpu: Arc<Gpu>, scale: f32) -> Pipe {
        Pipe::with(gpu, scale, false)
    }

    /// `seam`: drawn as the GPU process draws, by a [`crate::raster_thread::SeamRaster`] fed each
    /// message in its wire form.
    fn with(gpu: Arc<Gpu>, scale: f32, seam: bool) -> Pipe {
        let (sender, frames) = channel();
        let raster = if seam {
            RasterThread::through_seam(1, gpu.clone(), sender)
        } else {
            RasterThread::start(1, gpu.clone(), sender)
        };
        Pipe {
            raster: raster.unwrap(),
            frames,
            reader: TileRasterizer::new(gpu).unwrap(),
            shown: None,
            scale,
            generation: 1,
            shots: Vec::new(),
        }
    }

    /// `list` the way a tab paints it: split into layers, drawn, and read back.
    fn frame(&mut self, name: &str, page: &Page, list: &DisplayList, base_at: (i32, i32)) {
        let split = list.split_layers();
        let mut pieces = split.layers.into_iter().peekable();
        let base = match pieces.peek() {
            Some(l) if l.kind.is_none() && l.state.0.is_empty() => pieces.next().unwrap().list,
            _ => DisplayList::new(),
        };
        let scale = self.scale;
        let background = Color::WHITE;
        let view = Viewport {
            width: (WIDTH * scale).round() as u32,
            height: (HEIGHT * scale).round() as u32,
            scale,
            background,
        };
        self.raster.post(PaintedFrame {
            base,
            layers: pieces.collect(),
            view,
            viewport: Size::new(WIDTH, HEIGHT),
            base_at,
            text: page.text.clone(),
            images: page.images.clone(),
            meta: FrameMeta {
                origin: Point::ZERO,
                scale,
                content: Size::new(WIDTH, HEIGHT),
                scrollbar: 0.0,
                color_scheme: crate::driver::ColorScheme::Light,
                scrollbar_colors: None,
                background,
                generation: self.generation,
                number: self.generation,
                restore: None,
                scroll: None,
            },
            band: (0.0, HEIGHT),
            pinned: split.pinned,
            animations: list.animations.clone(),
        });
        let data = loop {
            let input = self.frames.recv();
            if let CompInput::Frame(_, data) = input.expect("the raster thread sends the frame") {
                break data;
            }
        };
        self.shoot(name, &data.frame);
        for (i, layer) in data.layers.iter().enumerate() {
            self.shoot(&format!("{name}.layer{i}"), &layer.frame);
            for (j, quad) in layer.videos.iter().enumerate() {
                if let Some((patch, _)) = &quad.patch {
                    self.shoot(&format!("{name}.layer{i}.patch{j}"), patch);
                }
            }
        }
        for (j, quad) in data.videos.iter().enumerate() {
            if let Some((patch, _)) = &quad.patch {
                self.shoot(&format!("{name}.patch{j}"), patch);
            }
        }
        if let Some(old) = self.shown.replace(data) {
            let old = *old;
            let recycler = old.recycler;
            recycler.recycle(old.frame);
            for layer in old.layers {
                recycler.recycle(layer.frame);
            }
        }
    }

    fn shoot(&mut self, name: &str, frame: &TiledFrame) {
        let pixmap = self.reader.read_back(frame).unwrap();
        self.shots.push(Shot {
            name: name.to_string(),
            width: pixmap.width,
            height: pixmap.height,
            data: pixmap.data,
        });
    }
}

fn built(paint: impl FnOnce(&mut DisplayListBuilder)) -> DisplayList {
    let mut b = DisplayListBuilder::new();
    paint(&mut b);
    b.build()
}

/// Every frame of one run on a raster thread and a device of their own: the page's fonts and images
/// are made new. (Direct2D's glyph cache is the device's, and what a run of text looks like can
/// depend on what the device has drawn: frames are only comparable after the same history.)
fn run(driver: Driver, scale: f32, fonts: &[Vec<u8>]) -> Vec<Shot> {
    run_with(driver, scale, fonts, false)
}

/// [`run`], drawn by the GPU process's raster path when `seam` ([`Pipe::with`]).
fn run_with(driver: Driver, scale: f32, fonts: &[Vec<u8>], seam: bool) -> Vec<Shot> {
    let gpu = Gpu::with_driver(driver).unwrap();
    let text = Arc::new(TextEngine::new().unwrap());
    let web = fonts
        .iter()
        .map(|f| text.add_font_data(f).unwrap())
        .collect();
    let mut page = Page {
        text,
        images: Arc::new(ImageStore::new()),
        web,
    };
    let mut pipe = Pipe::with(gpu, scale, seam);

    let list = built(|b| system_text(&page, b));
    pipe.frame("system-text", &page, &list, (0, 0));
    pipe.frame("system-text-again", &page, &list, (0, 0));
    let list = built(|b| web_text(&page, b));
    pipe.frame("web-text", &page, &list, (0, 0));
    let list = built(gradients);
    pipe.frame("gradients", &page, &list, (0, 0));
    pipe.frame("gradients-offset", &page, &list, (0, 37));

    let p = pictures(&page.images, 1);
    let list = built(|b| images(&p, b));
    pipe.frame("images", &page, &list, (0, 0));
    // A canvas that drew: new pixels under the same id.
    page.images.replace(p.opaque, still(300, 200, 40, false));
    pipe.frame("image-replaced", &page, &list, (0, 0));
    // A size that changes with them.
    page.images.replace(p.tiny, still(9, 6, 41, false));
    pipe.frame("image-resized", &page, &list, (0, 0));
    // An animation's frames, and back to one drawn before.
    page.images.set_frame(p.animated, 1);
    pipe.frame("animation-frame-1", &page, &list, (0, 0));
    page.images.set_frame(p.animated, 2);
    pipe.frame("animation-frame-2", &page, &list, (0, 0));
    page.images.set_frame(p.animated, 0);
    pipe.frame("animation-frame-0", &page, &list, (0, 0));
    // An animation decoded as it shows.
    page.images
        .show_frame(p.decoded_as_shown, pattern(70, 50, 42, false));
    pipe.frame("frame-decoded-as-shown", &page, &list, (0, 0));
    // A freed image whose id is taken by another.
    page.images.remove(p.faded);
    pipe.frame("image-removed", &page, &list, (0, 0));
    assert_eq!(page.images.add(still(128, 128, 43, true)), p.faded);
    pipe.frame("image-id-reused", &page, &list, (0, 0));
    pipe.frame("images-unchanged", &page, &list, (0, 0));

    let list = built(|b| layers(&page, &p, b));
    pipe.frame("layers", &page, &list, (0, 0));
    pipe.frame("layers-offset", &page, &list, (0, 24));

    // A new document: a store of its own, with the same ids for other pixels.
    page.images = Arc::new(ImageStore::new());
    pipe.generation += 1;
    let q = pictures(&page.images, 60);
    assert_eq!(q.opaque, ImageId(0));
    let list = built(|b| images(&q, b));
    pipe.frame("new-store", &page, &list, (0, 0));
    // (see the module's note: this would make the frames depend on the run)
    for font in (0..)
        .map(FontId)
        .take_while(|f| page.text.font_revision(*f).is_some())
    {
        let family = page.text.family_name(font);
        assert_eq!(page.text.font_revision(font), Some(0), "{family:?}");
    }
    std::mem::take(&mut pipe.shots)
}

fn file_name(prefix: &str, shot: &Shot) -> String {
    format!("{prefix}-{}.bgra", shot.name)
}

fn save(dir: &Path, prefix: &str, shots: &[Shot]) {
    std::fs::create_dir_all(dir).unwrap();
    for shot in shots {
        let mut bytes = Vec::with_capacity(shot.data.len() + 8);
        bytes.extend_from_slice(&shot.width.to_le_bytes());
        bytes.extend_from_slice(&shot.height.to_le_bytes());
        bytes.extend_from_slice(&shot.data);
        std::fs::write(dir.join(file_name(prefix, shot)), bytes).unwrap();
    }
}

/// What differs from the frames stored in `dir`, one line per frame.
fn compare(dir: &Path, prefix: &str, shots: &[Shot]) -> Vec<String> {
    let mut wrong = Vec::new();
    for shot in shots {
        let path = dir.join(file_name(prefix, shot));
        let Ok(bytes) = std::fs::read(&path) else {
            wrong.push(format!("{prefix} {}: nothing stored", shot.name));
            continue;
        };
        let (size, stored) = bytes.split_at(8);
        let (w, h) = (
            u32::from_le_bytes(size[..4].try_into().unwrap()),
            u32::from_le_bytes(size[4..].try_into().unwrap()),
        );
        if (w, h) != (shot.width, shot.height) {
            wrong.push(format!(
                "{prefix} {}: {}x{}, stored {w}x{h}",
                shot.name, shot.width, shot.height
            ));
            continue;
        }
        let differ: Vec<usize> = stored
            .iter()
            .zip(&shot.data)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();
        if let Some(first) = differ.first() {
            let px = first / 4;
            wrong.push(format!(
                "{prefix} {}: {} bytes differ, the first at ({}, {})",
                shot.name,
                differ.len(),
                px % w as usize,
                px / w as usize
            ));
        }
    }
    wrong
}

/// What differs between two frames of a size: enough to tell what was drawn differently, with both
/// frames left in the temporary folder.
fn differences(a: &Shot, b: &Shot) -> String {
    let width = a.width as usize * 4;
    let differ: Vec<usize> = (0..a.data.len().min(b.data.len()))
        .filter(|&i| a.data[i] != b.data[i])
        .collect();
    let mut rows: Vec<(usize, usize)> = Vec::new();
    for row in differ.iter().map(|i| i / width) {
        match rows.last_mut() {
            Some(last) if row <= last.1 + 1 => last.1 = row,
            _ => rows.push((row, row)),
        }
    }
    let dir = std::env::temp_dir();
    let mut written = String::new();
    for (shot, tag) in [(a, "a"), (b, "b")] {
        let path = dir.join(format!("snowlark-pixels-{}-{tag}.png", a.name));
        let saved = snowlark_paint::image::encode_png(shot.width, shot.height, &shot.data, &path);
        if saved.is_ok() {
            written = format!(" ({})", path.display());
        }
    }
    let count = differ.len();
    format!("{}: {count} bytes in rows {rows:?}{written}", a.name)
}

fn web_fonts() -> Vec<Vec<u8>> {
    let dir = Path::new(r"C:\Windows\Fonts");
    ["georgia.ttf", "cambria.ttc", "bahnschrift.ttf"]
        .iter()
        .map(|f| std::fs::read(dir.join(f)).unwrap())
        .collect()
}

/// What goes through the raster thread's own tables comes out as what the page's text engine and
/// image store draw, with nothing stored to compare with.
#[test]
fn the_raster_side_draws_what_the_page_holds() {
    let text = Arc::new(TextEngine::new().unwrap());
    let web = web_fonts()
        .iter()
        .map(|f| text.add_font_data(f).unwrap())
        .collect();
    let page = Page {
        text,
        images: Arc::new(ImageStore::new()),
        web,
    };
    let p = pictures(&page.images, 1);
    let lists = [
        built(|b| system_text(&page, b)),
        built(|b| web_text(&page, b)),
        built(|b| images(&p, b)),
    ];
    let mut wrong = Vec::new();
    for _ in 0..TRIES {
        wrong = differs_from_direct(&page, &lists);
        if wrong.is_empty() {
            break;
        }
    }
    assert!(wrong.is_empty(), "pixels differ:\n{}", wrong.join("\n"));
}

/// Draws each of `lists` through a raster thread, and with the page's own text engine and image
/// store. Each on a device of its own, with the same history.
fn differs_from_direct(page: &Page, lists: &[DisplayList]) -> Vec<String> {
    let view = Viewport {
        width: WIDTH as u32,
        height: HEIGHT as u32,
        scale: 1.0,
        background: Color::WHITE,
    };
    let gpu = Gpu::with_driver(Driver::Warp).unwrap();
    let mut pipe = Pipe::new(gpu, 1.0);
    let gpu = Gpu::with_driver(Driver::Warp).unwrap();
    let mut direct = TileRasterizer::new(gpu).unwrap();
    let mut wrong = Vec::new();
    for list in lists {
        pipe.frame("frame", page, list, (0, 0));
        let sources = Sources {
            text: &page.text,
            images: &page.images,
        };
        let frame = direct.raster(list, sources, &view).unwrap();
        let expected = direct.read_back(&frame).unwrap();
        let shot = pipe.shots.pop().unwrap();
        let drawn = Shot {
            name: "direct".to_string(),
            width: expected.width,
            height: expected.height,
            data: expected.data,
        };
        if shot.data != drawn.data {
            wrong.push(differences(&shot, &drawn));
        }
    }
    wrong
}

/// M7 G3: the GPU process's raster path (a `SeamRaster` fed each message as it crosses, checked on
/// the way in) draws every frame as the in-process one does, on WARP and the graphics card, at 100%
/// and 150%.
#[test]
fn the_gpu_process_raster_path_draws_the_same_pixels() {
    let fonts = web_fonts();
    let mut count = 0;
    for (driver, label) in [(Driver::Warp, "warp"), (Driver::Hardware, "gpu")] {
        if let Err(e) = Gpu::with_driver(driver) {
            assert!(driver != Driver::Warp, "no WARP device: {e}");
            continue;
        }
        for (scale, percent) in [(1.0, 100), (1.5, 150)] {
            let mut wrong = Vec::new();
            for _ in 0..TRIES {
                let here = run_with(driver, scale, &fonts, false);
                let there = run_with(driver, scale, &fonts, true);
                assert_eq!(here.len(), there.len());
                count = count.max(here.len());
                wrong = here
                    .iter()
                    .zip(&there)
                    .filter(|(a, b)| a != b)
                    .map(|(a, b)| format!("{label}-{percent}: {}", differences(a, b)))
                    .collect();
                if wrong.is_empty() {
                    break;
                }
            }
            assert!(wrong.is_empty(), "pixels differ:\n{}", wrong.join("\n"));
        }
    }
    eprintln!("[pixels] {count} frames a run, the same through the seam");
}

/// What is wrong with two runs of `driver`'s frames, which it returns too: where they differ from
/// each other, and from the frames stored in `baseline`.
fn two_runs(
    driver: Driver,
    scale: f32,
    fonts: &[Vec<u8>],
    prefix: &str,
    baseline: Option<&Path>,
) -> (Vec<Shot>, Vec<String>) {
    let shots = run(driver, scale, fonts);
    let again = run(driver, scale, fonts);
    assert_eq!(shots.len(), again.len());
    let mut wrong: Vec<String> = shots
        .iter()
        .zip(&again)
        .filter(|(a, b)| a != b)
        .map(|(a, b)| format!("{prefix}: two runs differ: {}", differences(a, b)))
        .collect();
    if let Some(dir) = baseline {
        wrong.extend(compare(dir, prefix, &shots));
    }
    (shots, wrong)
}

#[test]
fn raster_thread_pixels_are_the_stored_ones() {
    let fonts = web_fonts();
    let saving = std::env::var_os("SNOWLARK_PIXEL_SAVE");
    let baseline = std::env::var_os("SNOWLARK_PIXEL_BASELINE");
    let baseline = baseline.as_ref().map(Path::new);
    let mut wrong = Vec::new();
    let mut count = 0;
    for (driver, label) in [(Driver::Warp, "warp"), (Driver::Hardware, "gpu")] {
        if let Err(e) = Gpu::with_driver(driver) {
            assert!(driver != Driver::Warp, "no WARP device: {e}");
            eprintln!("no graphics card, frames on WARP only: {e}");
            continue;
        }
        for (scale, percent) in [(1.0, 100), (1.5, 150)] {
            let prefix = format!("{label}-{percent}");
            let (mut shots, mut errors) = two_runs(driver, scale, &fonts, &prefix, baseline);
            for _ in 1..TRIES {
                if errors.is_empty() {
                    break;
                }
                eprintln!("[pixels] once more: {}", errors.join("; "));
                (shots, errors) = two_runs(driver, scale, &fonts, &prefix, baseline);
            }
            wrong.extend(errors);
            for shot in &shots {
                let first = &shot.data[..4];
                assert!(
                    shot.data
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|px| px[..] != *first),
                    "{prefix} {}: all one color",
                    shot.name
                );
            }
            count += shots.len();
            if let Some(dir) = &saving {
                save(Path::new(dir), &prefix, &shots);
            }
        }
    }
    eprintln!("[pixels] {count} frames compared");
    assert!(wrong.is_empty(), "pixels differ:\n{}", wrong.join("\n"));
    // With `SNOWLARK_IPC=loopback`, every frame and resource went through the wire format: without
    // this, a run with the variable lost on the way would pass and prove nothing.
    let crossed = crate::raster_link::CROSSED.load(std::sync::atomic::Ordering::Relaxed);
    let asked = std::env::var("SNOWLARK_IPC").is_ok_and(|v| v == "loopback");
    assert_eq!(crossed > 0, asked, "{crossed} raster messages crossed");
    if asked {
        eprintln!("[pixels] {crossed} raster messages crossed the wire format");
    }
}
