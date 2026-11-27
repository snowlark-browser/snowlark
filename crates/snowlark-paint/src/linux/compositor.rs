// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's compositor on Linux, on the CPU: each frame is drawn into one buffer of the whole
//! client area (the chrome in its top rows, the content below, as the Windows half draws one chain
//! across processes), with Skia's CPU backend, and handed to the window's thread, which shows it in
//! a `wl_shm` buffer and asks the desktop for a frame callback with it. The next frame waits for
//! that callback ([`Compositor::wait_for_frame`]): the compositor draws at the display's pace, as
//! the waitable swap chain makes it on Windows.
//!
//! The interface is the Windows half's ([`Compositor`], [`ContentPass`]): tiles copied pixel for
//! pixel, tiles through a layer's clips, transforms, opacity and color filters, backdrop filters,
//! display lists. What needs a GPU there is absent here: protected players' pictures under the
//! content ([`Compositor::allow_underlays`] is false) and composition surfaces of another process
//! ([`WindowSurface`] cannot be made, so a compositor always draws for a window of its own
//! process). Video frames are drawn from memory (`video_frame`).

use std::marker::PhantomData;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use skia_safe as sk;

use crate::display_list::{matrix_of, radii_of, DisplayList, Item, LayerState, StateOp};
use crate::geom::{Color, Matrix, Radii, Rect};
use crate::gpu::{Gpu, TiledFrame};
use crate::raster::{matrix, rect, rrect, Caches, FontResolver, Painter, Sources};
use crate::window::{FrameSlot, WindowHandle};
use snowlark_graphics::{Error, Result};

/// A protected player's picture under the content: never shown on Linux (no player makes one).
pub struct Underlay {
    pub key: u64,
    pub surface: Arc<OwnedFd>,
    pub dest: Rect,
    pub clip: Rect,
}

/// A window's visual tree for a compositor of another process: none can be made on Linux yet,
/// so a window's compositor runs in its own process ([`Compositor::new`]).
#[derive(Clone)]
pub struct WindowSurface(());

impl WindowSurface {
    pub fn new(_window: WindowHandle) -> Result<WindowSurface> {
        Err(Error::Unsupported)
    }

    pub fn handle(&self) -> std::io::Result<OwnedFd> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }

    pub fn renew(&self) -> Result<OwnedFd> {
        Err(Error::Unsupported)
    }

    pub fn clear(&self) -> Result<()> {
        Ok(())
    }
}

/// The window's compositor (see the module documentation). Thread-affine: make it on the thread
/// that will draw.
pub struct Compositor {
    gpu: Arc<Gpu>,
    window: WindowHandle,
    slot: Arc<FrameSlot>,
    caches: Caches,
    /// The client area, device pixels.
    size: (u32, u32),
    chrome_height: u32,
    /// The chrome's pixels (`size.0` × `chrome_height`), drawn into every frame.
    chrome: Vec<u8>,
    /// The frame being drawn: the whole client area, premultiplied BGRA, rows packed.
    frame: Vec<u8>,
    _thread_affine: PhantomData<*const ()>,
}

/// Drawing into the content area of the frame (device pixels of the content area).
pub struct ContentPass<'a> {
    canvas: &'a sk::Canvas,
    caches: &'a mut Caches,
    size: (u32, u32),
    /// Where the content area's pixel (0, 0) is in the frame (below the chrome).
    origin: (i32, i32),
}

fn image_info(size: (u32, u32)) -> sk::ImageInfo {
    sk::ImageInfo::new(
        (size.0 as i32, size.1 as i32),
        sk::ColorType::BGRA8888,
        sk::AlphaType::Premul,
        None,
    )
}

fn sk_color(c: Color) -> sk::Color {
    sk::Color::from_argb(c.a, c.r, c.g, c.b)
}

fn pixels(size: (u32, u32)) -> usize {
    size.0 as usize * size.1 as usize * 4
}

impl Compositor {
    /// The compositor of `window` (a window of this process): a `width` × `height` client area whose
    /// top `chrome_height` rows are the chrome.
    pub fn new(
        gpu: Arc<Gpu>,
        _text: &dyn FontResolver,
        window: WindowHandle,
        width: u32,
        height: u32,
        chrome_height: u32,
    ) -> Result<Compositor> {
        let slot = FrameSlot::of(window).ok_or(Error::Invalid)?;
        let mut c = Compositor {
            gpu,
            window,
            slot,
            caches: Caches::default(),
            size: (0, 0),
            chrome_height: 0,
            chrome: Vec::new(),
            frame: Vec::new(),
            _thread_affine: PhantomData,
        };
        c.resize(width, height, chrome_height)?;
        Ok(c)
    }

    /// A compositor presenting into a composition surface of another process: none exists on
    /// Linux yet ([`WindowSurface`]).
    pub fn on_surface(
        _gpu: Arc<Gpu>,
        _text: &dyn FontResolver,
        _window: WindowHandle,
        _surface: OwnedFd,
        _width: u32,
        _height: u32,
        _chrome_height: u32,
    ) -> Result<Compositor> {
        Err(Error::Unsupported)
    }

    pub fn window(&self) -> WindowHandle {
        self.window
    }

    pub fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
    }

    /// The content area's size in device pixels.
    pub fn content_size(&self) -> (u32, u32) {
        (self.size.0, (self.size.1 - self.chrome_height).max(1))
    }

    /// The chrome is drawn into each frame: a change to it shows with the next present.
    pub fn chrome_in_frame(&self) -> bool {
        true
    }

    /// A new client size: the chrome takes the top `chrome_height` rows, the content the rest. The
    /// chrome must be drawn again afterwards.
    pub fn resize(&mut self, width: u32, height: u32, chrome_height: u32) -> Result<()> {
        let size = (width.max(1), height.max(1));
        let chrome_height = chrome_height.min(size.1);
        if (size, chrome_height) != (self.size, self.chrome_height) {
            self.size = size;
            self.chrome_height = chrome_height;
            self.chrome = vec![0; pixels((size.0, chrome_height))];
            self.frame = vec![0; pixels(size)];
        }
        Ok(())
    }

    /// Nothing to commit: every frame carries the chrome and the content together.
    pub fn commit(&self) -> Result<()> {
        Ok(())
    }

    /// Waits until the display wants the next frame (at most `timeout`); false on timeout.
    pub fn wait_for_frame(&self, timeout: Duration) -> bool {
        self.slot.wait_ready(timeout)
    }

    /// Draws the chrome: `list` (in DIPs, scaled by `scale`) over `background`. It shows with the
    /// next present.
    pub fn draw_chrome(
        &mut self,
        list: &DisplayList,
        sources: Sources,
        scale: f32,
        background: Color,
    ) -> Result<()> {
        list.validate()
            .map_err(|e| Error::Platform(e.to_string()))?;
        let size = (self.size.0, self.chrome_height);
        if size.1 == 0 {
            return Ok(());
        }
        let info = image_info(size);
        let mut surface =
            sk::surfaces::wrap_pixels(&info, &mut self.chrome, size.0 as usize * 4, None)
                .ok_or(Error::Invalid)?;
        let canvas = surface.canvas();
        canvas.clear(sk_color(background));
        let mut painter = Painter::new(
            canvas,
            &mut self.caches,
            sources,
            Matrix::scale(scale, scale),
        );
        painter.paint(list);
        Ok(())
    }

    /// Draws the next frame's content with `paint`, then the chrome above it; nothing shows until
    /// [`Compositor::present`].
    pub fn draw_content(&mut self, paint: impl FnOnce(&mut ContentPass<'_>)) -> Result<()> {
        let content = self.content_size();
        let need = pixels(self.size);
        if self.frame.len() != need {
            self.frame = self
                .slot
                .take_spare()
                .filter(|b| b.len() == need)
                .unwrap_or_else(|| vec![0; need]);
        }
        let info = image_info(self.size);
        let mut surface =
            sk::surfaces::wrap_pixels(&info, &mut self.frame, self.size.0 as usize * 4, None)
                .ok_or(Error::Invalid)?;
        let canvas = surface.canvas();
        canvas.save();
        canvas.clip_rect(
            sk::Rect::from_xywh(
                0.0,
                self.chrome_height as f32,
                content.0 as f32,
                content.1 as f32,
            ),
            sk::ClipOp::Intersect,
            false,
        );
        let mut pass = ContentPass {
            canvas,
            caches: &mut self.caches,
            size: content,
            origin: (0, self.chrome_height as i32),
        };
        paint(&mut pass);
        canvas.restore();
        canvas.reset_matrix();
        if self.chrome_height > 0 {
            let row = self.size.0 as usize * 4;
            let info = image_info((self.size.0, self.chrome_height));
            // (false only for a size the buffer does not have: never here)
            let _ = canvas.write_pixels(&info, &self.chrome, row, (0, 0));
        }
        Ok(())
    }

    /// The frame drawn last (before [`Compositor::present`]): its size and pixels, BGRA, rows packed.
    pub fn read_back(&mut self) -> Result<(u32, u32, Vec<u8>)> {
        Ok((self.size.0, self.size.1, self.frame.clone()))
    }

    /// Hands the frame to the window to show. The next frame is drawn into a buffer of one shown
    /// before, so every pass draws the whole content area, as into a flip-model swap chain.
    pub fn present(&mut self) -> Result<Duration> {
        let t = Instant::now();
        let need = pixels(self.size);
        let next = self
            .slot
            .take_spare()
            .filter(|b| b.len() == need)
            .unwrap_or_else(|| vec![0; need]);
        let frame = std::mem::replace(&mut self.frame, next);
        if !self.slot.show(self.size.0, self.size.1, frame) {
            return Err(Error::Platform("the window has gone".into()));
        }
        Ok(t.elapsed())
    }

    /// Protected players' pictures cannot show under the content here.
    pub fn allow_underlays(&mut self) -> bool {
        false
    }

    pub fn set_underlays(&mut self, _underlays: &[Underlay]) -> Result<()> {
        Ok(())
    }

    pub fn underlays(&self) -> usize {
        0
    }
}

/// A block's visible part: its offset in the block, and its rectangle (x, y, width, height) in the
/// target.
type Visible = ((u32, u32), (i32, i32, u32, u32));

/// Where the visible part of a `w` × `h` block at `(x, y)` is within `clip`, in whole pixels;
/// `None` when nothing shows.
fn visible(x: i32, y: i32, w: u32, h: u32, clip: &Rect) -> Option<Visible> {
    let left = x.max(clip.x.ceil() as i32);
    let top = y.max(clip.y.ceil() as i32);
    let right = (x + w as i32).min(clip.right().floor() as i32);
    let bottom = (y + h as i32).min(clip.bottom().floor() as i32);
    if right <= left || bottom <= top {
        return None;
    }
    Some((
        ((left - x) as u32, (top - y) as u32),
        (left, top, (right - left) as u32, (bottom - top) as u32),
    ))
}

impl ContentPass<'_> {
    /// The content area's size in device pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The content area's rectangle `r` in the frame's pixels.
    fn at(&self, r: Rect) -> Rect {
        Rect::new(
            r.x + self.origin.0 as f32,
            r.y + self.origin.1 as f32,
            r.width,
            r.height,
        )
        .intersect(&Rect::new(
            self.origin.0 as f32,
            self.origin.1 as f32,
            self.size.0 as f32,
            self.size.1 as f32,
        ))
    }

    pub fn clear(&mut self, color: Color) {
        let area = self.at(Rect::new(0.0, 0.0, self.size.0 as f32, self.size.1 as f32));
        self.canvas.save();
        self.canvas.reset_matrix();
        self.canvas
            .clip_rect(rect(&area), sk::ClipOp::Intersect, false);
        self.canvas.clear(sk_color(color));
        self.canvas.restore();
    }

    /// Draws a video frame ([`crate::video::VideoStore::front_of`]) scaled into `dest`, within
    /// `clip`.
    pub fn video_frame(
        &mut self,
        front: &crate::video::VideoFront,
        dest: Rect,
        clip: Rect,
    ) -> bool {
        let f = &front.0;
        let Some(image) = sk::images::raster_from_data(
            &image_info((f.width, f.height)),
            sk::Data::new_copy(&f.pixels),
            f.width as usize * 4,
        ) else {
            return false;
        };
        let (dest, clip) = (self.at(dest), self.at(clip));
        self.canvas.save();
        self.canvas.reset_matrix();
        self.canvas
            .clip_rect(rect(&clip), sk::ClipOp::Intersect, false);
        self.canvas.draw_image_rect_with_sampling_options(
            &image,
            None,
            rect(&dest),
            sk::SamplingOptions::new(sk::FilterMode::Linear, sk::MipmapMode::None),
            &sk::Paint::default(),
        );
        self.canvas.restore();
        true
    }

    /// Clears `rect` to transparent (where a picture under the content would show: none does here).
    pub fn hole(&mut self, r: Rect) {
        let area = self.at(Rect::from_ltrb(
            r.x.round(),
            r.y.round(),
            r.right().round(),
            r.bottom().round(),
        ));
        self.canvas.save();
        self.canvas.reset_matrix();
        self.canvas
            .clip_rect(rect(&area), sk::ClipOp::Intersect, false);
        self.canvas.clear(sk::Color::TRANSPARENT);
        self.canvas.restore();
    }

    /// Draws a frame's tiles with its top-left corner at `(dx, dy)` device pixels, within `clip`
    /// (device pixels of the content area). Tiles are copied pixel for pixel.
    pub fn tiles(&mut self, frame: &TiledFrame, dx: i32, dy: i32, clip: Rect) {
        let clip = self.at(clip);
        let (dx, dy) = (dx + self.origin.0, dy + self.origin.1);
        for t in &frame.tiles {
            let w = t.size().min(frame.width.saturating_sub(t.x));
            let h = t.size().min(frame.height.saturating_sub(t.y));
            let Some(((ox, oy), (x, y, vw, vh))) =
                visible(dx + t.x as i32, dy + t.y as i32, w, h, &clip)
            else {
                continue;
            };
            let stride = t.size() as usize * 4;
            let start = oy as usize * stride + ox as usize * 4;
            let src = &t.pixels()[start..];
            if t.opaque() {
                // (an opaque tile replaces what is under it: a copy, as a blend would come to)
                let _ = self
                    .canvas
                    .write_pixels(&image_info((vw, vh)), src, stride, (x, y));
            } else {
                let Some(image) = sk::images::raster_from_data(
                    &image_info((vw, vh)),
                    sk::Data::new_copy(&copy_rows(src, stride, vw, vh)),
                    vw as usize * 4,
                ) else {
                    continue;
                };
                self.canvas.save();
                self.canvas.reset_matrix();
                self.canvas.draw_image(&image, (x as f32, y as f32), None);
                self.canvas.restore();
            }
        }
    }

    /// Draws a piece's tiles through a layer's state (see the Windows half): rastered at `scale`
    /// device pixels per CSS px with their pixel (0, 0) at `at`, under the state's clips,
    /// transforms, opacities and color filters, its list's device pixel (0, 0) at `base`, within
    /// `clip`. A whole-pixel translation draws them pixel for pixel; any other is filtered linearly.
    pub fn tiles_through(
        &mut self,
        frame: &TiledFrame,
        at: (i32, i32),
        state: &LayerState,
        scale: f32,
        base: (i32, i32),
        clip: Rect,
    ) {
        let s = if scale > 0.0 && scale.is_finite() {
            scale
        } else {
            1.0
        };
        let base = (base.0 + self.origin.0, base.1 + self.origin.1);
        let clip = self.at(clip);
        let css_device = |m: &Matrix| {
            m.then(&Matrix::scale(s, s))
                .then(&Matrix::translation(base.0 as f32, base.1 as f32))
        };
        let canvas = self.canvas;
        let count = canvas.save();
        canvas.reset_matrix();
        canvas.clip_rect(rect(&clip), sk::ClipOp::Intersect, false);
        let mut current = Matrix::IDENTITY;
        let mut opacity = 1.0f32;
        let mut color: Option<sk::ColorFilter> = None;
        for op in &state.0 {
            match op {
                StateOp::Transform(t) => current = matrix_of(t).then(&current),
                StateOp::Opacity(o) => opacity *= o,
                StateOp::Filter(f) => {
                    // (innermost first: each applies to what the ones inside it made)
                    for f in f.ops().iter().rev() {
                        if let Some(m) = f.color_matrix() {
                            let outer = crate::raster::filters::color_matrix(&m);
                            color = Some(match color {
                                Some(inner) => outer.composed(&inner).unwrap_or(inner),
                                None => outer,
                            });
                        }
                    }
                }
                StateOp::Animated(_) => {}
                StateOp::Clip(r) | StateOp::ClipRounded(r, _) => {
                    let radii = match op {
                        StateOp::ClipRounded(_, radii) => radii_of(radii),
                        _ => Radii::ZERO,
                    };
                    let m = css_device(&current);
                    canvas.set_matrix(&sk::M44::from(matrix(&m)));
                    if radii.is_zero() && m.is_axis_aligned() {
                        // (snapped to device pixels, as the painter snaps a clip)
                        let d = m.apply_rect(r);
                        let snapped = Rect::from_ltrb(
                            d.x.round(),
                            d.y.round(),
                            d.right().round(),
                            d.bottom().round(),
                        );
                        canvas.reset_matrix();
                        canvas.clip_rect(rect(&snapped), sk::ClipOp::Intersect, false);
                    } else {
                        canvas.clip_rrect(rrect(r, &radii), sk::ClipOp::Intersect, true);
                    }
                    canvas.reset_matrix();
                }
            }
        }
        if opacity > 0.0 {
            let m = Matrix::scale(1.0 / s, 1.0 / s)
                .then(&current)
                .then(&Matrix::scale(s, s))
                .then(&Matrix::translation(base.0 as f32, base.1 as f32));
            let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
            let whole =
                near(m.m11, 1.0) && near(m.m12, 0.0) && near(m.m21, 0.0) && near(m.m22, 1.0);
            let (m, sampling) = if whole {
                (
                    Matrix::translation(m.dx.round(), m.dy.round()),
                    sk::SamplingOptions::new(sk::FilterMode::Nearest, sk::MipmapMode::None),
                )
            } else {
                (
                    m,
                    sk::SamplingOptions::new(sk::FilterMode::Linear, sk::MipmapMode::None),
                )
            };
            let mut paint = sk::Paint::default();
            paint.set_alpha_f(opacity.clamp(0.0, 1.0));
            if let Some(c) = &color {
                paint.set_color_filter(c.clone());
            }
            canvas.set_matrix(&sk::M44::from(matrix(&m)));
            for t in &frame.tiles {
                let w = t.size().min(frame.width.saturating_sub(t.x));
                let h = t.size().min(frame.height.saturating_sub(t.y));
                if w == 0 || h == 0 {
                    continue;
                }
                let dest = Rect::new(
                    (t.x as i32 + at.0) as f32,
                    (t.y as i32 + at.1) as f32,
                    w as f32,
                    h as f32,
                );
                if !m.apply_rect(&dest).intersects(&clip) {
                    continue;
                }
                let size = (t.size(), t.size());
                let Some(image) = sk::images::raster_from_data(
                    &image_info(size),
                    sk::Data::new_copy(t.pixels()),
                    size.0 as usize * 4,
                ) else {
                    continue;
                };
                canvas.draw_image_rect_with_sampling_options(
                    &image,
                    Some((
                        &sk::Rect::from_wh(w as f32, h as f32),
                        sk::canvas::SrcRectConstraint::Strict,
                    )),
                    rect(&dest),
                    sampling,
                    &paint,
                );
            }
        }
        canvas.restore_to_count(count);
    }

    /// Applies a backdrop filter (Filter Effects 2 §3) to what the pass has drawn so far under the
    /// filter's box (its list's device pixel (0, 0) at `at`, `scale` device pixels per CSS px),
    /// clipped to the box's rounded shape, at its opacity, within `clip`.
    pub fn backdrop(
        &mut self,
        backdrop: &crate::display_list::LayerBackdrop,
        scale: f32,
        at: (i32, i32),
        clip: Rect,
    ) {
        if backdrop.opacity <= 0.0 || backdrop.filters.is_empty() {
            return;
        }
        let s = if scale > 0.0 && scale.is_finite() {
            scale
        } else {
            1.0
        };
        // The filter as a list of its own, painted as the list's own backdrop filter is.
        let mut list = DisplayList::default();
        let opaque = backdrop.opacity >= 1.0;
        if !opaque {
            list.items.push(Item::PushOpacity {
                opacity: backdrop.opacity.clamp(0.0, 1.0),
                bounds: backdrop.rect,
            });
        }
        list.items.push(Item::BackdropFilter {
            filters: crate::filter::FilterRange {
                start: 0,
                len: backdrop.filters.len() as u32,
            },
            rect: backdrop.rect,
            radii: backdrop.radii,
            own: u8::from(!opaque),
        });
        list.filters = backdrop.filters.clone();
        if !opaque {
            list.items.push(Item::PopOpacity);
        }
        let clip = self.at(clip);
        let base = Matrix::scale(s, s).then(&Matrix::translation(
            (at.0 + self.origin.0) as f32,
            (at.1 + self.origin.1) as f32,
        ));
        let images = crate::raster_tables::ImageTable::new();
        let no_fonts = NoFonts;
        let sources = Sources {
            text: &no_fonts,
            images: &images,
        };
        self.paint_clipped(&list, sources, base, clip);
    }

    /// Draws a video's front frame: none reach a slot on Linux yet.
    pub fn video(&mut self, _slot: &crate::video::VideoSlot, _dest: Rect, _clip: Rect) -> bool {
        false
    }

    /// Draws a display list (CSS px scaled by `scale`, then moved by `(dx, dy)` device pixels).
    pub fn list(&mut self, list: &DisplayList, sources: Sources, scale: f32, dx: f32, dy: f32) {
        if list.validate().is_err() {
            return;
        }
        let (dx, dy) = (dx + self.origin.0 as f32, dy + self.origin.1 as f32);
        let base = Matrix::scale(scale, scale).then(&Matrix::translation(dx, dy));
        let all = self.at(Rect::new(0.0, 0.0, self.size.0 as f32, self.size.1 as f32));
        self.paint_clipped(list, sources, base, all);
    }

    fn paint_clipped(&mut self, list: &DisplayList, sources: Sources, base: Matrix, clip: Rect) {
        let canvas = self.canvas;
        let count = canvas.save();
        canvas.reset_matrix();
        canvas.clip_rect(rect(&clip), sk::ClipOp::Intersect, false);
        let mut painter = Painter::new(canvas, self.caches, sources, base);
        painter.paint(list);
        canvas.restore_to_count(count);
        canvas.reset_matrix();
    }
}

/// Rows `w` × `h` of a block whose rows are `stride` bytes apart, packed.
fn copy_rows(src: &[u8], stride: usize, w: u32, h: u32) -> Vec<u8> {
    let row = w as usize * 4;
    let mut out = Vec::with_capacity(row * h as usize);
    for y in 0..h as usize {
        out.extend_from_slice(&src[y * stride..y * stride + row]);
    }
    out
}

/// The fonts of a list that has no text (a backdrop filter's).
struct NoFonts;

impl FontResolver for NoFonts {
    fn font_data(&self, _font: crate::display_list::FontId) -> Option<crate::text::FontData> {
        None
    }
}

#[cfg(test)]
#[path = "compositor_tests.rs"]
mod tests;
