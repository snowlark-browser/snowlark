// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.12.5 `<canvas>` and its 2D context: `HTMLCanvasElement` (`width`, `height`, `getContext`,
//! `toDataURL`), `CanvasRenderingContext2D` with its mixins, `CanvasGradient`, `CanvasPattern`,
//! `Path2D`, `TextMetrics` and `ImageData`, over `snowlark_render::canvas::Canvas2D`. A context that drew
//! hands its bitmap to the renderer at the next rendering update.

use std::sync::Arc;

use snowlark_bindings::generated::{
    self as g, CanvasCompositingImpl, CanvasDrawImageImpl, CanvasDrawPathImpl,
    CanvasFillStrokeStylesImpl, CanvasFiltersImpl, CanvasGradientImpl, CanvasImageDataImpl,
    CanvasImageSmoothingImpl, CanvasPathDrawingStylesImpl, CanvasPathImpl, CanvasPatternImpl,
    CanvasRectImpl, CanvasRenderingContext2DImpl, CanvasSettingsImpl, CanvasShadowStylesImpl,
    CanvasStateImpl, CanvasTextDrawingStylesImpl, CanvasTextImpl, CanvasTransformImpl,
    CanvasUserInterfaceImpl, HTMLCanvasElementImpl, ImageDataImpl, Path2DImpl, TextMetricsImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsObject, JsValue};
use snowlark_dom::NodeId;
use snowlark_render::canvas::skia::{self as tiny_skia, LineCap, LineJoin, Transform};
use snowlark_render::canvas::{
    self as cv, svgtypes, Canvas2D, CanvasPath, Fonts, Gradient, GradientKind, Style,
};

use super::{exception, Engine, Object};

/// How long a rendering update's paint waits for its canvas frames at most (ms): a canvas thread
/// that falls further behind shows its frames at later paints instead of holding every one back.
pub(crate) const CANVAS_PAINT_WAIT_MS: f64 = 50.0;

/// What a rendering context draws for: a `<canvas>` element, or an `OffscreenCanvas` (HTML
/// §4.12.5.3), which may be in a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CanvasOwner {
    Element(NodeId),
    Offscreen(Handle),
}

/// A `CanvasRenderingContext2D` (or an `OffscreenCanvasRenderingContext2D`): its canvas and its
/// bitmap.
pub struct ContextObj {
    pub owner: CanvasOwner,
    pub canvas: Canvas2D,
    /// The objects `fillStyle`/`strokeStyle` return when they are a gradient or pattern.
    pub fill_object: Option<Handle>,
    pub stroke_object: Option<Handle>,
    /// The bitmap's origin-clean flag (HTML §4.12.5.1.14): false once anything script may not
    /// read was drawn into it, after which its pixels cannot be read back. Never set again.
    pub origin_clean: bool,
}

/// An `ImageData`.
pub struct ImageDataObj {
    pub width: u32,
    pub height: u32,
    pub data: JsObject,
}

/// A drawImage, createPattern or texture source (HTML §4.12.5.1.14 `CanvasImageSource`, the kinds
/// generated).
pub(crate) type ImageSource =
    g::HTMLImageElementOrSVGImageElementOrHTMLVideoElementOrHTMLCanvasElementOrImageBitmapOrOffscreenCanvas;

/// A bitmap as a frame: premultiplied BGRA, top row first.
pub(crate) fn frame_of(pm: &tiny_skia::Pixmap) -> snowlark_paint::image::Frame {
    let mut pixels = pm.data().to_vec();
    for px in pixels.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    snowlark_paint::image::Frame {
        width: pm.width(),
        height: pm.height(),
        pixels,
        delay_ms: 0,
    }
}

fn f(v: f64) -> f32 {
    v as f32
}

impl Engine {
    fn ctx<R>(&self, h: Handle, f: impl FnOnce(&mut ContextObj) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::Canvas2D(c) => f(c),
            _ => panic!("not a CanvasRenderingContext2D"),
        }
    }

    /// Runs a drawing operation and marks the canvas for the next rendering update.
    fn draw<R>(&self, h: Handle, op: impl FnOnce(&mut Canvas2D) -> R) -> R {
        let (r, owner) = self.ctx(h, |c| (op(&mut c.canvas), c.owner));
        match owner {
            CanvasOwner::Element(element) => {
                let mut dirty = self.canvas_dirty.borrow_mut();
                if !dirty.contains(&element) {
                    dirty.push(element);
                }
                drop(dirty);
                self.render.borrow_mut().dirty = true;
            }
            CanvasOwner::Offscreen(oc) => self.offscreen_drew(oc),
        }
        r
    }

    /// The canvas element's bitmap size from its attributes (HTML: 300×150 by default).
    pub(crate) fn canvas_size(&self, el: NodeId) -> (u32, u32) {
        (
            self.dim_attr(el, "width", 300),
            self.dim_attr(el, "height", 150),
        )
    }

    /// A canvas's `width` or `height` attribute changed: its context's bitmap is reset.
    pub(crate) fn canvas_resized(&self, el: NodeId) {
        self.webgl_resized(CanvasOwner::Element(el));
        let Some(h) = self.canvases.borrow().get(&el).copied() else {
            return;
        };
        let (w, hh) = self.canvas_size(el);
        self.draw(h, |c| c.resize(w, hh));
    }

    /// The rendering update's part for canvases, before style and layout: the bitmaps that changed
    /// go to the renderer. Their frames are asked of the canvas thread, which reads each once it has
    /// drawn what script recorded before the update, and are taken after layout
    /// ([`Engine::finish_canvas_frames`]); a canvas the renderer has no frame of yet gets it now
    /// (its box is built with it). With [`Engine::set_canvas_frames_async`] no frame is waited for,
    /// a canvas's first included (its box is sized by its attributes, not its bitmap): one not read
    /// by the end of layout shows from the rendering update after it comes
    /// ([`Engine::poll_canvas_frames`]), as a canvas drawn after this update would.
    pub(crate) fn flush_canvases(&self) {
        // (frames that came since the last update)
        self.take_canvas_frames(false);
        let dirty = std::mem::take(&mut *self.canvas_dirty.borrow_mut());
        let mut known = Vec::with_capacity(dirty.len());
        let wait_for_first = !self.canvas_frames_async.get();
        for el in dirty {
            let Some(h) = self.canvases.borrow().get(&el).copied() else {
                continue;
            };
            let ask = !wait_for_first
                || self
                    .with_node_renderer(el, |r, _| r.has_canvas_frame(el))
                    .unwrap_or(false);
            if ask {
                // (every copy is queued before any is read: the reads wait for the GPU once)
                self.ctx(h, |c| c.canvas.start_frame());
                known.push((el, h));
            } else {
                let frame = self.ctx(h, |c| c.canvas.frame());
                self.show_canvas_frame(el, frame);
            }
        }
        if !known.is_empty() && self.canvas_frames_asked_at.get().is_none() {
            self.canvas_frames_asked_at.set(Some(self.now()));
        }
        for (el, h) in known {
            let waker = self.waker();
            let wake: snowlark_paint::canvas_thread::Wake = Arc::new(move || waker.wake());
            let request = self.ctx(h, |c| c.canvas.request_frame(Some(wake)));
            // The oldest frame on its way stays (a canvas thread slower than the updates still
            // shows each one it reads); a newer one still on its way is replaced by this one.
            let mut pending = self.canvas_frames_pending.borrow_mut();
            let mine: Vec<usize> = (0..pending.len()).filter(|&i| pending[i].0 == el).collect();
            if mine.len() >= 2 {
                pending.remove(*mine.last().unwrap());
            }
            pending.push((el, request));
        }
    }

    /// The frames [`Engine::flush_canvases`] asked for, once the rendering update laid out (no
    /// script runs between the two, so they are the bitmaps as the update found them): those the
    /// canvas thread read by now, or, unless frames are taken asynchronously, all of them.
    pub(crate) fn finish_canvas_frames(&self) {
        self.take_canvas_frames(!self.canvas_frames_async.get());
    }

    /// Frames that came after their rendering update: they show from the next one. Whether any
    /// came.
    pub(crate) fn poll_canvas_frames(&self) -> bool {
        if self.canvas_frames_pending.borrow().is_empty() || !self.take_canvas_frames(false) {
            return false;
        }
        self.render.borrow_mut().dirty = true;
        true
    }

    /// Whether a canvas frame is on its way (the canvas thread wakes the page thread with it).
    pub(crate) fn canvas_frames_pending(&self) -> bool {
        !self.canvas_frames_pending.borrow().is_empty()
    }

    /// Until when the paint of a rendering update waits for the canvas frames it asked for (ms
    /// since the time origin), when frames are taken asynchronously and some are still on their
    /// way. HTML's "update the rendering" shows each canvas as script left it with the rest of the
    /// document: a paint before its frame came would show the document's new state over the
    /// canvas's old drawing (a map that moves its canvas and redraws it would jump back and forth),
    /// and the frame's coming would paint the whole page a second time.
    pub(crate) fn canvas_paint_deadline(&self) -> Option<f64> {
        if !self.canvas_frames_async.get() || !self.canvas_frames_pending() {
            return None;
        }
        self.canvas_frames_asked_at
            .get()
            .map(|at| at + CANVAS_PAINT_WAIT_MS)
    }

    /// Whether the paint waits for canvas frames now ([`Engine::canvas_paint_deadline`]).
    pub(crate) fn canvas_paint_held(&self) -> bool {
        self.canvas_paint_deadline()
            .is_some_and(|until| self.now() < until)
    }

    /// Frames are taken without waiting for the canvas thread at the rendering update (the window,
    /// and runs timed as it is): a frame read after the update shows at the next one. Otherwise
    /// (`--render`, tests, reftests) every update shows the bitmaps as script left them.
    pub fn set_canvas_frames_async(&self, on: bool) {
        self.canvas_frames_async.set(on);
    }

    /// Hands the frames the canvas thread read to the renderer, waiting for the others when
    /// `wait`. Whether any was handed over.
    fn take_canvas_frames(&self, wait: bool) -> bool {
        let pending = std::mem::take(&mut *self.canvas_frames_pending.borrow_mut());
        if pending.is_empty() {
            return false;
        }
        let mut left = Vec::new();
        let mut any = false;
        for (el, mut request) in pending {
            let frame = if wait {
                request.wait()
            } else {
                match request.try_take() {
                    Some(frame) => frame,
                    None => {
                        left.push((el, request));
                        continue;
                    }
                }
            };
            // (a canvas whose context went with its element: its frame shows nowhere)
            if self.canvases.borrow().contains_key(&el) {
                any |= self.show_canvas_frame(el, frame);
            }
        }
        // (frames asked for while these were taken: none, no script runs here)
        if left.is_empty() {
            self.canvas_frames_asked_at.set(None);
        }
        self.canvas_frames_pending.borrow_mut().extend(left);
        any
    }

    pub(crate) fn show_canvas_frame(
        &self,
        el: NodeId,
        frame: Option<snowlark_paint::image::Frame>,
    ) -> bool {
        let Some(frame) = frame else {
            return false;
        };
        self.with_node_renderer(el, |r, st| {
            // A canvas's first frame rebuilds its box: when it comes after this update's layout,
            // the next update does that.
            st.dirty |= !r.has_canvas_frame(el);
            r.set_canvas_frame(el, frame);
            st.needs_paint = true;
        })
        .is_some()
    }

    /// `drawImage` of `image` into the context `this`, the source and destination rectangles from
    /// the source's size (`rects`). A canvas drawing on the GPU is drawn by a copy on the device
    /// when this context can draw it; everything else through the source's pixels.
    fn draw_image_with(
        &self,
        this: Handle,
        image: &ImageSource,
        rects: impl Fn(f32, f32) -> ((f32, f32, f32, f32), (f32, f32, f32, f32)),
    ) -> rt::Result<()> {
        self.check_canvas_source(image)?;
        // HTML §4.12.5.1.14 `drawImage()`: "If image is not origin-clean, then set the
        // CanvasRenderingContext2D's origin-clean flag to false" (once it is drawn: a source with
        // nothing to draw returns before).
        if self.draw_image_pixels(this, image, rects) && !self.source_origin_clean(image) {
            self.ctx(this, |c| c.origin_clean = false);
        }
        Ok(())
    }

    /// HTML §4.12.5.1.14 "check the usability of the image argument": an `OffscreenCanvas` that
    /// was transferred (detached) or a canvas with no pixels (width or height 0) throws
    /// `InvalidStateError`.
    fn check_canvas_source(&self, image: &ImageSource) -> rt::Result<()> {
        if let ImageSource::ImageBitmap(h) = image {
            if self.bitmap_closed(*h) {
                return Err(exception(
                    ExceptionName::InvalidStateError,
                    "The ImageBitmap is closed",
                ));
            }
        }
        let Some(src) = self.canvas_of(image) else {
            return Ok(());
        };
        if let CanvasOwner::Offscreen(h) = src {
            if self.offscreen_detached(h) {
                return Err(exception(
                    ExceptionName::InvalidStateError,
                    "The OffscreenCanvas is detached",
                ));
            }
        }
        let (w, h) = self.source_size(src);
        if w == 0 || h == 0 {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The canvas has no pixels",
            ));
        }
        Ok(())
    }

    /// The canvas a `CanvasImageSource` is, if it is one.
    fn canvas_of(&self, image: &ImageSource) -> Option<CanvasOwner> {
        match image {
            ImageSource::HTMLCanvasElement(h) => Some(CanvasOwner::Element(self.n(*h))),
            ImageSource::OffscreenCanvas(h) => Some(CanvasOwner::Offscreen(*h)),
            _ => None,
        }
    }

    /// [`Engine::draw_image_with`]'s drawing: whether the source had pixels to draw.
    fn draw_image_pixels(
        &self,
        this: Handle,
        image: &ImageSource,
        rects: impl Fn(f32, f32) -> ((f32, f32, f32, f32), (f32, f32, f32, f32)),
    ) -> bool {
        if let Some(src) = self.canvas_of(image) {
            let ctx = self.source_2d(src);
            if let Some(snapshot) = ctx.and_then(|ctx| self.ctx(ctx, |c| c.canvas.gpu_snapshot())) {
                let (src, dst) = rects(snapshot.width as f32, snapshot.height as f32);
                if self.draw(this, |c| c.draw_canvas_image(&snapshot, src, dst)) {
                    return true;
                }
            }
        }
        if let ImageSource::HTMLImageElement(h) = image {
            // (the decoded image itself: a canvas on the GPU keeps it uploaded between draws)
            let el = self.n(*h);
            let Some(decoded) = self
                .with_node_renderer(el, |r, _| r.image_pixels(el))
                .flatten()
            else {
                return false;
            };
            // HTML §4.12.5.1.14: an animated image draws "the default image of the animation
            // ... or, if there is no such image, the first frame", whichever frame shows.
            let frame = decoded.default_frame();
            let (w, h) = (frame.width.max(1), frame.height.max(1));
            if frame.pixels.len() != (w as usize) * (h as usize) * 4 {
                return false;
            }
            let (src, dst) = rects(w as f32, h as f32);
            self.draw(this, |c| c.draw_decoded_image(&decoded, src, dst));
            return true;
        }
        let Some(pm) = self.image_source(image) else {
            return false;
        };
        let (src, dst) = rects(pm.width() as f32, pm.height() as f32);
        self.draw(this, |c| c.draw_image(&pm, src, dst));
        true
    }

    /// Whether an `<img>`'s image is CORS-same-origin (HTML §2.5.4): script may read its pixels.
    /// False while it shows no image.
    pub(crate) fn img_origin_clean(&self, el: NodeId) -> bool {
        let cors_mode = self
            .dom()
            .attr(
                el,
                &snowlark_dom::ns!(),
                &snowlark_dom::LocalName::from("crossorigin"),
            )
            .is_some();
        self.with_node_renderer(el, |r, _| r.image_readable(el, cors_mode))
            .unwrap_or(false)
    }

    /// Whether a canvas's bitmap is origin-clean (HTML §4.12.5.1.14): a 2D context's flag; a WebGL
    /// drawing buffer always is (WebGL refuses sources script may not read instead of tainting),
    /// as is a canvas with no context. A placeholder canvas has its OffscreenCanvas's frames,
    /// which carry the flag they were drawn with.
    pub(crate) fn canvas_origin_clean(&self, src: CanvasOwner) -> bool {
        if let Some(ctx) = self.source_2d(src) {
            return self.ctx(ctx, |c| c.origin_clean);
        }
        match src {
            CanvasOwner::Element(el) => self.placeholder_origin_clean(el),
            CanvasOwner::Offscreen(_) => true,
        }
    }

    /// HTML §4.12.5.1.14 "the image argument is not origin-clean": an image that is not
    /// CORS-same-origin, a canvas whose bitmap is not origin-clean. (A video or SVG `image`
    /// element draws nothing here yet, so it cannot taint.)
    fn source_origin_clean(&self, source: &ImageSource) -> bool {
        if let Some(src) = self.canvas_of(source) {
            return self.canvas_origin_clean(src);
        }
        match source {
            ImageSource::HTMLImageElement(h) => self.img_origin_clean(self.n(*h)),
            ImageSource::ImageBitmap(h) => self.bitmap_pixels(*h).is_none_or(|(_, clean, _)| clean),
            _ => true,
        }
    }

    /// The `SecurityError` reading back a bitmap that is not origin-clean throws.
    pub(crate) fn tainted_error() -> rt::Error {
        exception(
            ExceptionName::SecurityError,
            "The canvas has been tainted by cross-origin data",
        )
    }

    /// The 2D context of a canvas, if it has one.
    pub(crate) fn source_2d(&self, src: CanvasOwner) -> Option<Handle> {
        match src {
            CanvasOwner::Element(el) => self.canvases.borrow().get(&el).copied(),
            CanvasOwner::Offscreen(h) => self.offscreen_2d(h),
        }
    }

    /// The WebGL context of a canvas, if it has one.
    pub(crate) fn source_webgl(&self, src: CanvasOwner) -> Option<Handle> {
        match src {
            CanvasOwner::Element(el) => self.gl.borrow().contexts.get(&el).copied(),
            CanvasOwner::Offscreen(h) => self.offscreen_webgl(h),
        }
    }

    /// A canvas's bitmap size: its attributes', or an OffscreenCanvas's own.
    pub(crate) fn source_size(&self, src: CanvasOwner) -> (u32, u32) {
        match src {
            CanvasOwner::Element(el) => self.canvas_size(el),
            CanvasOwner::Offscreen(h) => self.offscreen_size(h),
        }
    }

    /// A canvas's pixels as a frame (premultiplied BGRA, top row first): its 2D bitmap, its WebGL
    /// drawing buffer as it is now, or a placeholder canvas's last frame. `None` for a canvas with
    /// no context.
    pub(crate) fn source_frame(&self, src: CanvasOwner) -> Option<snowlark_paint::image::Frame> {
        if let Some(ctx) = self.source_2d(src) {
            let pm = self.ctx(ctx, |c| c.canvas.snapshot())?;
            return Some(frame_of(&pm));
        }
        if let Some(ctx) = self.source_webgl(src) {
            return self.webgl_snapshot_of(ctx);
        }
        match src {
            CanvasOwner::Element(el) => self.placeholder_frame(el),
            CanvasOwner::Offscreen(_) => None,
        }
    }

    /// The pixels of a `drawImage`/`createPattern` source.
    fn image_source(&self, source: &ImageSource) -> Option<tiny_skia::Pixmap> {
        if let Some(src) = self.canvas_of(source) {
            if let Some(ctx) = self.source_2d(src) {
                return self.ctx(ctx, |c| c.canvas.snapshot());
            }
            // (a WebGL canvas draws its drawing buffer as it is now)
            return cv::pixmap_from_frame(&self.source_frame(src)?);
        }
        match source {
            ImageSource::HTMLImageElement(h) => {
                let el = self.n(*h);
                let image = self
                    .with_node_renderer(el, |r, _| r.image_pixels(el))
                    .flatten()?;
                // HTML §4.12.5.1.14: an animated image draws "the default image of the animation
                // ... or, if there is no such image, the first frame", whichever frame shows.
                cv::pixmap_from_frame(image.default_frame())
            }
            ImageSource::ImageBitmap(h) => cv::pixmap_from_frame(&self.bitmap_pixels(*h)?.0),
            // (an SVG `image` element's image is not exposed to canvas yet)
            _ => None,
        }
    }

    /// Makes a 2D context's bitmap transparent black, its drawing state kept
    /// (`transferToImageBitmap`).
    pub(crate) fn clear_2d_bitmap(&self, ctx: Handle) {
        self.draw(ctx, |c| {
            let (w, h) = c.size();
            let transform = std::mem::replace(&mut c.state.transform, Transform::identity());
            let clip = c.state.clip.take();
            c.clear_rect(0.0, 0.0, w as f32, h as f32);
            c.state.transform = transform;
            c.state.clip = clip;
        });
    }

    /// A canvas's pixels as a WebGL texture upload reads them (WebGL 1.0 §5.14.8): its 2D bitmap,
    /// its WebGL drawing buffer as it is now, a placeholder's last frame, or, with no context,
    /// transparent black of its size. Width, height and premultiplied RGBA, the top row first.
    pub(crate) fn canvas_source_pixels(&self, src: CanvasOwner) -> Option<(u32, u32, Vec<u8>)> {
        if let Some(ctx) = self.source_2d(src) {
            let pm = self.ctx(ctx, |c| c.canvas.snapshot())?;
            return Some((pm.width(), pm.height(), pm.data().to_vec()));
        }
        if let Some(frame) = self.source_frame(src) {
            return Some((
                frame.width,
                frame.height,
                super::webgl::bgra_to_rgba(&frame.pixels),
            ));
        }
        let (w, h) = self.source_size(src);
        // (a quarter of a gigabyte at most: no device takes a texture larger than 16384 x 4096)
        if w as u64 * h as u64 * 4 > 1 << 28 {
            return None;
        }
        Some((w, h, vec![0; w as usize * h as usize * 4]))
    }

    fn style_value(
        &self,
        style: &Style,
        object: Option<Handle>,
    ) -> g::StringOrCanvasGradientOrCanvasPattern {
        match (style, object) {
            (Style::Gradient(_), Some(h)) => {
                g::StringOrCanvasGradientOrCanvasPattern::CanvasGradient(h)
            }
            (Style::Pattern(_), Some(h)) => {
                g::StringOrCanvasGradientOrCanvasPattern::CanvasPattern(h)
            }
            (Style::Color(c, _), _) => {
                g::StringOrCanvasGradientOrCanvasPattern::String(cv::serialize_color(*c))
            }
            _ => g::StringOrCanvasGradientOrCanvasPattern::String("#000000".into()),
        }
    }

    /// Whether `value` is the colour string `current` was set from: setting it again changes nothing,
    /// so scripts that set the same colour before each shape skip its parsing.
    fn same_color(current: &Style, value: &g::StringOrCanvasGradientOrCanvasPattern) -> bool {
        match (current, value) {
            (Style::Color(_, prev), g::StringOrCanvasGradientOrCanvasPattern::String(s)) => {
                prev.as_str() == &s[..]
            }
            _ => false,
        }
    }

    /// A style from a `fillStyle`/`strokeStyle` value (`None`: ignored, as an unparsable colour is).
    fn style_from(
        &self,
        value: &g::StringOrCanvasGradientOrCanvasPattern,
    ) -> Option<(Style, Option<Handle>)> {
        match value {
            g::StringOrCanvasGradientOrCanvasPattern::String(s) => {
                let c = cv::parse_color(s)?;
                Some((Style::Color(c, s.clone()), None))
            }
            g::StringOrCanvasGradientOrCanvasPattern::CanvasGradient(h) => match &*self.obj(*h) {
                Object::CanvasGradient(gr) => {
                    Some((Style::Gradient(Arc::new(gr.clone())), Some(*h)))
                }
                _ => None,
            },
            g::StringOrCanvasGradientOrCanvasPattern::CanvasPattern(h) => match &*self.obj(*h) {
                Object::CanvasPattern(p, _) => Some((Style::Pattern(p.clone()), Some(*h))),
                _ => None,
            },
        }
    }

    /// HTML §4.12.5.1.10: setting `fillStyle` or `strokeStyle` to a pattern that is not
    /// origin-clean sets the context's origin-clean flag to false.
    fn taint_by_style(&self, this: Handle, value: &g::StringOrCanvasGradientOrCanvasPattern) {
        if let g::StringOrCanvasGradientOrCanvasPattern::CanvasPattern(h) = value {
            if matches!(&*self.obj(*h), Object::CanvasPattern(_, false)) {
                self.ctx(this, |c| c.origin_clean = false);
            }
        }
    }

    fn path2d<R>(&self, h: Handle, f: impl FnOnce(&mut CanvasPath) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::Path2D(p) => f(p),
            _ => panic!("not a Path2D"),
        }
    }

    /// Runs a path operation on a context's current path (with its transform) or a `Path2D`
    /// (identity).
    fn with_path(&self, h: Handle, op: impl FnOnce(&mut CanvasPath, &Transform)) {
        match &mut *self.obj_mut(h) {
            Object::Canvas2D(c) => {
                let t = c.canvas.state.transform;
                op(&mut c.canvas.path, &t);
            }
            Object::Path2D(p) => op(p, &Transform::identity()),
            _ => {}
        }
    }

    fn fonts(&self) -> Option<Fonts> {
        self.with_renderer(|r, _| r.system_fonts())
    }

    pub(crate) fn new_image_data(
        &self,
        cx: &mut Cx,
        w: u32,
        h: u32,
        bytes: Option<&[u8]>,
    ) -> rt::Result<Handle> {
        if w == 0 || h == 0 {
            return Err(exception(ExceptionName::IndexSizeError, "The size is zero"));
        }
        let len = (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n <= (1 << 30))
            .ok_or_else(|| {
                exception(ExceptionName::IndexSizeError, "The image data is too large")
            })?;
        let scope = cx.scope();
        let buffer = v8::ArrayBuffer::new(scope, len);
        if let Some(bytes) = bytes {
            let store = buffer.get_backing_store();
            for (i, b) in bytes.iter().take(len).enumerate() {
                store[i].set(*b);
            }
        }
        let array = v8::Uint8ClampedArray::new(scope, buffer, 0, len).ok_or(rt::Error::Pending)?;
        let data = JsObject::new(scope, array.into());
        Ok(self.alloc(Object::ImageData(Box::new(ImageDataObj {
            width: w,
            height: h,
            data,
        }))))
    }

    pub(crate) fn image_data_bytes(&self, cx: &mut Cx, h: Handle) -> Option<(u32, u32, Vec<u8>)> {
        let (w, hh, data) = match &*self.obj(h) {
            Object::ImageData(d) => (d.width, d.height, d.data.clone()),
            _ => return None,
        };
        let scope = cx.scope();
        let obj = v8::Local::new(scope, data.global());
        let view: v8::Local<v8::ArrayBufferView> = obj.try_into().ok()?;
        let mut out = vec![0u8; view.byte_length()];
        view.copy_contents(&mut out);
        Some((w, hh, out))
    }
}

impl HTMLCanvasElementImpl for Engine {
    fn width(&self, this: Handle) -> u32 {
        self.dim_attr(self.n(this), "width", 300)
    }
    fn set_width(&self, cx: &mut Cx, this: Handle, value: u32) -> rt::Result<()> {
        self.check_not_placeholder(self.n(this))?;
        self.set_dim_attr(cx, self.n(this), "width", value, 300);
        Ok(())
    }
    fn height(&self, this: Handle) -> u32 {
        self.dim_attr(self.n(this), "height", 150)
    }
    fn set_height(&self, cx: &mut Cx, this: Handle, value: u32) -> rt::Result<()> {
        self.check_not_placeholder(self.n(this))?;
        self.set_dim_attr(cx, self.n(this), "height", value, 150);
        Ok(())
    }
    fn transfer_control_to_offscreen(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.transfer_control(self.n(this))
    }
    /// HTML §4.12.5 `getContext(contextId, options)`: "2d", "webgl" (and its old name
    /// "experimental-webgl"), "webgl2"; a canvas keeps the kind of context it first gave (another kind is
    /// null); other kinds are not supported (null). A placeholder canvas (one whose control went
    /// to an OffscreenCanvas) throws `InvalidStateError`.
    fn get_context(
        &self,
        cx: &mut Cx,
        this: Handle,
        context_id: &str,
        options: JsValue,
    ) -> rt::Result<
        Option<g::CanvasRenderingContext2DOrWebGLRenderingContextOrWebGL2RenderingContext>,
    > {
        use g::CanvasRenderingContext2DOrWebGLRenderingContextOrWebGL2RenderingContext as R;
        let el = self.n(this);
        if self.is_placeholder(el) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The canvas's control was transferred to an OffscreenCanvas",
            ));
        }
        let webgl2 = context_id == "webgl2";
        if webgl2 || context_id == "webgl" || context_id == "experimental-webgl" {
            if self.canvases.borrow().contains_key(&el) {
                return Ok(None);
            }
            let c = self.webgl_get_context(cx, CanvasOwner::Element(el), &options, webgl2)?;
            return Ok(c.map(if webgl2 {
                R::WebGL2RenderingContext
            } else {
                R::WebGLRenderingContext
            }));
        }
        if context_id != "2d" || self.has_webgl(el) {
            return Ok(None);
        }
        if let Some(h) = self.canvases.borrow().get(&el) {
            return Ok(Some(R::CanvasRenderingContext2D(*h)));
        }
        let (w, h) = self.canvas_size(el);
        let ctx = self.alloc(Object::Canvas2D(Box::new(ContextObj {
            owner: CanvasOwner::Element(el),
            canvas: Canvas2D::new(w, h),
            fill_object: None,
            stroke_object: None,
            origin_clean: true,
        })));
        self.canvases.borrow_mut().insert(el, ctx);
        Ok(Some(R::CanvasRenderingContext2D(ctx)))
    }
    /// `toDataURL()`: a PNG of the bitmap.
    fn to_data_url(
        &self,
        _cx: &mut Cx,
        this: Handle,
        _type_: &str,
        _quality: Option<JsValue>,
    ) -> rt::Result<DomStr<'_>> {
        let el = self.n(this);
        // HTML §4.12.5 `toDataURL()`: "If this canvas element's bitmap's origin-clean flag is
        // set to false, then throw a SecurityError".
        if !self.canvas_origin_clean(CanvasOwner::Element(el)) {
            return Err(Self::tainted_error());
        }
        let (w, h) = self.canvas_size(el);
        if w == 0 || h == 0 {
            return Ok(DomStr::Borrowed("data:,"));
        }
        if let Some(frame) = self
            .webgl_snapshot(el)
            .or_else(|| self.placeholder_frame(el))
        {
            let png = cv::pixmap_from_frame(&frame).and_then(|p| p.encode_png().ok());
            return Ok(DomStr::Owned(match png {
                Some(bytes) => format!("data:image/png;base64,{}", base64_encode(&bytes)),
                None => "data:,".into(),
            }));
        }
        let ctx = self.canvases.borrow().get(&el).copied();
        let pixmap = match ctx {
            Some(c) => self.ctx(c, |c| c.canvas.snapshot()),
            None => tiny_skia::Pixmap::new(w, h),
        };
        let png = pixmap.and_then(|p| p.encode_png().ok());
        Ok(DomStr::Owned(match png {
            Some(bytes) => format!("data:image/png;base64,{}", base64_encode(&bytes)),
            None => "data:,".into(),
        }))
    }
    fn to_blob(
        &self,
        _cx: &mut Cx,
        this: Handle,
        _callback: g::BlobCallback,
        _type_: &str,
        _quality: Option<JsValue>,
    ) -> rt::Result<()> {
        // HTML §4.12.5 `toBlob()`: "If this canvas element's bitmap's origin-clean flag is set
        // to false, then throw a SecurityError".
        if !self.canvas_origin_clean(CanvasOwner::Element(self.n(this))) {
            return Err(Self::tainted_error());
        }
        // (encoding a Blob is later work: the callback is not called)
        Ok(())
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

impl CanvasRenderingContext2DImpl for Engine {
    fn canvas(&self, this: Handle) -> Handle {
        match self.ctx(this, |c| c.owner) {
            CanvasOwner::Element(el) => super::node_handle(el),
            CanvasOwner::Offscreen(h) => h,
        }
    }
}

impl g::OffscreenCanvasRenderingContext2DImpl for Engine {
    fn canvas(&self, this: Handle) -> Handle {
        CanvasRenderingContext2DImpl::canvas(self, this)
    }
}

impl CanvasSettingsImpl for Engine {
    fn get_context_attributes(
        &self,
        _cx: &mut Cx,
        _this: Handle,
    ) -> rt::Result<g::CanvasRenderingContext2DSettings> {
        Ok(g::CanvasRenderingContext2DSettings::default())
    }
}

impl CanvasStateImpl for Engine {
    fn save(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.ctx(this, |c| c.canvas.save());
        Ok(())
    }
    fn restore(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.ctx(this, |c| c.canvas.restore());
        Ok(())
    }
    fn reset(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.draw(this, |c| c.reset());
        Ok(())
    }
    fn is_context_lost(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(false)
    }
}

impl CanvasTransformImpl for Engine {
    fn scale(&self, _cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<()> {
        if x.is_finite() && y.is_finite() {
            self.ctx(this, |c| {
                c.canvas.state.transform = c.canvas.state.transform.pre_scale(f(x), f(y))
            });
        }
        Ok(())
    }
    fn rotate(&self, _cx: &mut Cx, this: Handle, angle: f64) -> rt::Result<()> {
        if angle.is_finite() {
            let (s, co) = (angle as f32).sin_cos();
            let r = Transform::from_row(co, s, -s, co, 0.0, 0.0);
            self.ctx(this, |c| {
                c.canvas.state.transform = c.canvas.state.transform.pre_concat(r)
            });
        }
        Ok(())
    }
    fn translate(&self, _cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<()> {
        if x.is_finite() && y.is_finite() {
            self.ctx(this, |c| {
                c.canvas.state.transform = c.canvas.state.transform.pre_translate(f(x), f(y))
            });
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn transform(
        &self,
        _cx: &mut Cx,
        this: Handle,
        a: f64,
        b: f64,
        c: f64,
        d: f64,
        e: f64,
        ff: f64,
    ) -> rt::Result<()> {
        if [a, b, c, d, e, ff].iter().all(|v| v.is_finite()) {
            let m = Transform::from_row(f(a), f(b), f(c), f(d), f(e), f(ff));
            self.ctx(this, |cc| {
                cc.canvas.state.transform = cc.canvas.state.transform.pre_concat(m)
            });
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn set_transform_a_b_c_d_e_f(
        &self,
        _cx: &mut Cx,
        this: Handle,
        a: f64,
        b: f64,
        c: f64,
        d: f64,
        e: f64,
        ff: f64,
    ) -> rt::Result<()> {
        if [a, b, c, d, e, ff].iter().all(|v| v.is_finite()) {
            let m = Transform::from_row(f(a), f(b), f(c), f(d), f(e), f(ff));
            self.ctx(this, |cc| cc.canvas.state.transform = m);
        }
        Ok(())
    }
    fn set_transform_transform(
        &self,
        cx: &mut Cx,
        this: Handle,
        t: g::DOMMatrix2DInit,
    ) -> rt::Result<()> {
        // Canvas §4.12.5.1.7: "create a DOMMatrix from the 2D dictionary" (with its validation).
        let [a, b, c, d, e, ff] = super::geometry::matrix_from_2d_init(&t)?;
        CanvasTransformImpl::set_transform_a_b_c_d_e_f(self, cx, this, a, b, c, d, e, ff)
    }
    /// `getTransform()`: a new DOMMatrix of the current transformation matrix.
    fn get_transform(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let t = self.ctx(this, |c| c.canvas.state.transform);
        Ok(self.new_matrix(
            super::matrix::Matrix::from_2d(
                t.sx as f64,
                t.ky as f64,
                t.kx as f64,
                t.sy as f64,
                t.tx as f64,
                t.ty as f64,
            ),
            false,
        ))
    }
    fn reset_transform(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.ctx(this, |c| c.canvas.state.transform = Transform::identity());
        Ok(())
    }
}

impl CanvasCompositingImpl for Engine {
    fn global_alpha(&self, this: Handle) -> f64 {
        self.ctx(this, |c| c.canvas.state.global_alpha as f64)
    }
    fn set_global_alpha(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            self.ctx(this, |c| c.canvas.state.global_alpha = value as f32);
        }
        Ok(())
    }
    fn global_composite_operation(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ctx(this, |c| c.canvas.state.composite_name.clone()))
    }
    fn set_global_composite_operation(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: &str,
    ) -> rt::Result<()> {
        if let Some(m) = cv::blend_mode(value) {
            self.ctx(this, |c| {
                c.canvas.state.composite = m;
                c.canvas.state.composite_name = value.to_owned();
            });
        }
        Ok(())
    }
}

impl CanvasImageSmoothingImpl for Engine {
    fn image_smoothing_enabled(&self, this: Handle) -> bool {
        self.ctx(this, |c| c.canvas.state.image_smoothing)
    }
    fn set_image_smoothing_enabled(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: bool,
    ) -> rt::Result<()> {
        self.ctx(this, |c| c.canvas.state.image_smoothing = value);
        Ok(())
    }
    fn image_smoothing_quality(&self, _this: Handle) -> g::ImageSmoothingQuality {
        g::ImageSmoothingQuality::Low
    }
    fn set_image_smoothing_quality(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _value: g::ImageSmoothingQuality,
    ) -> rt::Result<()> {
        Ok(())
    }
}

impl CanvasFillStrokeStylesImpl for Engine {
    fn stroke_style(&self, this: Handle) -> g::StringOrCanvasGradientOrCanvasPattern {
        let (s, o) = self.ctx(this, |c| (c.canvas.state.stroke.clone(), c.stroke_object));
        self.style_value(&s, o)
    }
    fn set_stroke_style(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: g::StringOrCanvasGradientOrCanvasPattern,
    ) -> rt::Result<()> {
        if self.ctx(this, |c| Self::same_color(&c.canvas.state.stroke, &value)) {
            return Ok(());
        }
        if let Some((s, o)) = self.style_from(&value) {
            self.taint_by_style(this, &value);
            self.ctx(this, |c| {
                c.canvas.state.stroke = s;
                c.stroke_object = o;
            });
        }
        Ok(())
    }
    fn fill_style(&self, this: Handle) -> g::StringOrCanvasGradientOrCanvasPattern {
        let (s, o) = self.ctx(this, |c| (c.canvas.state.fill.clone(), c.fill_object));
        self.style_value(&s, o)
    }
    fn set_fill_style(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: g::StringOrCanvasGradientOrCanvasPattern,
    ) -> rt::Result<()> {
        if self.ctx(this, |c| Self::same_color(&c.canvas.state.fill, &value)) {
            return Ok(());
        }
        if let Some((s, o)) = self.style_from(&value) {
            self.taint_by_style(this, &value);
            self.ctx(this, |c| {
                c.canvas.state.fill = s;
                c.fill_object = o;
            });
        }
        Ok(())
    }
    fn create_linear_gradient(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    ) -> rt::Result<Handle> {
        if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) {
            return Err(rt::Error::Type("The coordinates must be finite".into()));
        }
        Ok(self.alloc(Object::CanvasGradient(Gradient {
            kind: GradientKind::Linear {
                x0: f(x0),
                y0: f(y0),
                x1: f(x1),
                y1: f(y1),
            },
            stops: Vec::new(),
        })))
    }
    #[allow(clippy::too_many_arguments)]
    fn create_radial_gradient(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        x0: f64,
        y0: f64,
        r0: f64,
        x1: f64,
        y1: f64,
        r1: f64,
    ) -> rt::Result<Handle> {
        if ![x0, y0, r0, x1, y1, r1].iter().all(|v| v.is_finite()) {
            return Err(rt::Error::Type("The coordinates must be finite".into()));
        }
        if r0 < 0.0 || r1 < 0.0 {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "A radius is negative",
            ));
        }
        Ok(self.alloc(Object::CanvasGradient(Gradient {
            kind: GradientKind::Radial {
                x0: f(x0),
                y0: f(y0),
                r0: f(r0),
                x1: f(x1),
                y1: f(y1),
                r1: f(r1),
            },
            stops: Vec::new(),
        })))
    }
    fn create_conic_gradient(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        start_angle: f64,
        x: f64,
        y: f64,
    ) -> rt::Result<Handle> {
        Ok(self.alloc(Object::CanvasGradient(Gradient {
            kind: GradientKind::Conic {
                angle: f(start_angle),
                x: f(x),
                y: f(y),
            },
            stops: Vec::new(),
        })))
    }
    fn create_pattern(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        image: ImageSource,
        repetition: &str,
    ) -> rt::Result<Option<Handle>> {
        let (rx, ry) = match repetition {
            "" | "repeat" => (true, true),
            "repeat-x" => (true, false),
            "repeat-y" => (false, true),
            "no-repeat" => (false, false),
            _ => {
                return Err(exception(
                    ExceptionName::SyntaxError,
                    "The repetition is not a keyword",
                ))
            }
        };
        let Some(pixmap) = self.image_source(&image) else {
            return Ok(None);
        };
        // "If image is not origin-clean, then mark pattern as not origin-clean."
        let clean = self.source_origin_clean(&image);
        Ok(Some(self.alloc(Object::CanvasPattern(
            Arc::new(cv::Pattern {
                image: Arc::new(pixmap),
                repeat_x: rx,
                repeat_y: ry,
            }),
            clean,
        ))))
    }
}

impl CanvasGradientImpl for Engine {
    fn add_color_stop(
        &self,
        _cx: &mut Cx,
        this: Handle,
        offset: f64,
        color: &str,
    ) -> rt::Result<()> {
        if !(0.0..=1.0).contains(&offset) {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The offset is out of range",
            ));
        }
        let c = cv::parse_color(color)
            .ok_or_else(|| exception(ExceptionName::SyntaxError, "The colour does not parse"))?;
        if let Object::CanvasGradient(g) = &mut *self.obj_mut(this) {
            g.stops.push((offset as f32, c));
        }
        Ok(())
    }
}

impl CanvasPatternImpl for Engine {
    fn set_transform(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        transform: g::DOMMatrix2DInit,
    ) -> rt::Result<()> {
        super::geometry::matrix_from_2d_init(&transform)?;
        Ok(())
    }
}

impl CanvasShadowStylesImpl for Engine {
    fn shadow_offset_x(&self, this: Handle) -> f64 {
        self.ctx(this, |c| c.canvas.state.shadow_offset.0 as f64)
    }
    fn set_shadow_offset_x(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value.is_finite() {
            self.ctx(this, |c| c.canvas.state.shadow_offset.0 = value as f32);
        }
        Ok(())
    }
    fn shadow_offset_y(&self, this: Handle) -> f64 {
        self.ctx(this, |c| c.canvas.state.shadow_offset.1 as f64)
    }
    fn set_shadow_offset_y(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value.is_finite() {
            self.ctx(this, |c| c.canvas.state.shadow_offset.1 = value as f32);
        }
        Ok(())
    }
    fn shadow_blur(&self, this: Handle) -> f64 {
        self.ctx(this, |c| c.canvas.state.shadow_blur as f64)
    }
    fn set_shadow_blur(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value.is_finite() && value >= 0.0 {
            self.ctx(this, |c| c.canvas.state.shadow_blur = value as f32);
        }
        Ok(())
    }
    fn shadow_color(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ctx(this, |c| c.canvas.state.shadow_color.clone()))
    }
    fn set_shadow_color(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        if let Some(col) = cv::parse_color(value) {
            self.ctx(this, |c| {
                c.canvas.state.shadow_color = cv::serialize_color(col)
            });
        }
        Ok(())
    }
}

impl CanvasFiltersImpl for Engine {
    fn filter(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ctx(this, |c| c.canvas.state.filter.clone()))
    }
    fn set_filter(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.ctx(this, |c| c.canvas.state.set_filter(value));
        Ok(())
    }
}

impl CanvasRectImpl for Engine {
    fn clear_rect(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> rt::Result<()> {
        self.draw(this, |c| c.clear_rect(f(x), f(y), f(w), f(h)));
        Ok(())
    }
    fn fill_rect(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> rt::Result<()> {
        self.draw(this, |c| c.fill_rect(f(x), f(y), f(w), f(h)));
        Ok(())
    }
    fn stroke_rect(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> rt::Result<()> {
        self.draw(this, |c| c.stroke_rect(f(x), f(y), f(w), f(h)));
        Ok(())
    }
}

impl CanvasDrawPathImpl for Engine {
    fn begin_path(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.ctx(this, |c| c.canvas.path = CanvasPath::new());
        Ok(())
    }
    fn fill_fill_rule(
        &self,
        _cx: &mut Cx,
        this: Handle,
        rule: g::CanvasFillRule,
    ) -> rt::Result<()> {
        self.draw(this, |c| c.fill(None, rule == g::CanvasFillRule::Evenodd));
        Ok(())
    }
    fn fill_path_fill_rule(
        &self,
        _cx: &mut Cx,
        this: Handle,
        path: Handle,
        rule: g::CanvasFillRule,
    ) -> rt::Result<()> {
        let p = self.path2d(path, |p| p.clone());
        let t = self.ctx(this, |c| c.canvas.state.transform);
        let mut transformed = CanvasPath::new();
        transformed.add_path(&p, t);
        self.draw(this, |c| {
            c.fill(Some(&transformed), rule == g::CanvasFillRule::Evenodd)
        });
        Ok(())
    }
    fn stroke_0(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.draw(this, |c| c.stroke(None));
        Ok(())
    }
    fn stroke_path(&self, _cx: &mut Cx, this: Handle, path: Handle) -> rt::Result<()> {
        let p = self.path2d(path, |p| p.clone());
        let t = self.ctx(this, |c| c.canvas.state.transform);
        let mut transformed = CanvasPath::new();
        transformed.add_path(&p, t);
        self.draw(this, |c| c.stroke(Some(&transformed)));
        Ok(())
    }
    fn clip_fill_rule(
        &self,
        _cx: &mut Cx,
        this: Handle,
        rule: g::CanvasFillRule,
    ) -> rt::Result<()> {
        self.ctx(this, |c| {
            c.canvas.clip(None, rule == g::CanvasFillRule::Evenodd)
        });
        Ok(())
    }
    fn clip_path_fill_rule(
        &self,
        _cx: &mut Cx,
        this: Handle,
        path: Handle,
        rule: g::CanvasFillRule,
    ) -> rt::Result<()> {
        let p = self.path2d(path, |p| p.clone());
        let t = self.ctx(this, |c| c.canvas.state.transform);
        let mut transformed = CanvasPath::new();
        transformed.add_path(&p, t);
        self.ctx(this, |c| {
            c.canvas
                .clip(Some(&transformed), rule == g::CanvasFillRule::Evenodd)
        });
        Ok(())
    }
    fn is_point_in_path_x_y_fill_rule(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
        rule: g::CanvasFillRule,
    ) -> rt::Result<bool> {
        if !x.is_finite() || !y.is_finite() {
            return Ok(false);
        }
        // (the point is in canvas coordinates, untransformed)
        Ok(self.ctx(this, |c| {
            let t = c.canvas.state.transform;
            c.canvas.state.transform = Transform::identity();
            let r = c
                .canvas
                .is_point_in_path(None, f(x), f(y), rule == g::CanvasFillRule::Evenodd);
            c.canvas.state.transform = t;
            r
        }))
    }
    fn is_point_in_path_path_x_y_fill_rule(
        &self,
        _cx: &mut Cx,
        this: Handle,
        path: Handle,
        x: f64,
        y: f64,
        rule: g::CanvasFillRule,
    ) -> rt::Result<bool> {
        if !x.is_finite() || !y.is_finite() {
            return Ok(false);
        }
        let p = self.path2d(path, |p| p.clone());
        Ok(self.ctx(this, |c| {
            let t = c.canvas.state.transform;
            let mut transformed = CanvasPath::new();
            transformed.add_path(&p, t);
            c.canvas.state.transform = Transform::identity();
            let r = c.canvas.is_point_in_path(
                Some(&transformed),
                f(x),
                f(y),
                rule == g::CanvasFillRule::Evenodd,
            );
            c.canvas.state.transform = t;
            r
        }))
    }
    fn is_point_in_stroke_x_y(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _x: f64,
        _y: f64,
    ) -> rt::Result<bool> {
        Ok(false)
    }
    fn is_point_in_stroke_path_x_y(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _path: Handle,
        _x: f64,
        _y: f64,
    ) -> rt::Result<bool> {
        Ok(false)
    }
}

impl CanvasUserInterfaceImpl for Engine {
    fn draw_focus_if_needed_element(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _element: Handle,
    ) -> rt::Result<()> {
        Ok(())
    }
    fn draw_focus_if_needed_path_element(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _path: Handle,
        _element: Handle,
    ) -> rt::Result<()> {
        Ok(())
    }
}

impl CanvasTextImpl for Engine {
    fn fill_text(
        &self,
        _cx: &mut Cx,
        this: Handle,
        text: &str,
        x: f64,
        y: f64,
        _max_width: Option<f64>,
    ) -> rt::Result<()> {
        if let Some(fonts) = self.fonts() {
            self.draw(this, |c| c.draw_text(&fonts, text, f(x), f(y), false));
        }
        Ok(())
    }
    fn stroke_text(
        &self,
        _cx: &mut Cx,
        this: Handle,
        text: &str,
        x: f64,
        y: f64,
        _max_width: Option<f64>,
    ) -> rt::Result<()> {
        if let Some(fonts) = self.fonts() {
            self.draw(this, |c| c.draw_text(&fonts, text, f(x), f(y), true));
        }
        Ok(())
    }
    fn measure_text(&self, _cx: &mut Cx, this: Handle, text: &str) -> rt::Result<Handle> {
        let (width, ascent, descent) = match self.fonts() {
            Some(fonts) => self.ctx(this, |c| c.canvas.measure_text(&fonts, text)),
            None => {
                let size = self.ctx(this, |c| cv::font_size(&c.canvas.state.font));
                (
                    text.chars().count() as f32 * size * 0.5,
                    size * 0.8,
                    size * 0.2,
                )
            }
        };
        Ok(self.alloc(Object::TextMetrics {
            width: width as f64,
            ascent: ascent as f64,
            descent: descent as f64,
        }))
    }
}

macro_rules! text_keyword {
    ($get:ident, $set:ident, $ty:ident, $field:ident, $default:expr) => {
        fn $get(&self, this: Handle) -> g::$ty {
            let v = self.ctx(this, |c| c.canvas.state.$field.clone());
            g::$ty::parse(&v).unwrap_or($default)
        }
        fn $set(&self, _cx: &mut Cx, this: Handle, value: g::$ty) -> rt::Result<()> {
            self.ctx(this, |c| c.canvas.state.$field = value.as_str().to_owned());
            Ok(())
        }
    };
}

impl CanvasTextDrawingStylesImpl for Engine {
    fn lang(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("inherit")
    }
    fn set_lang(&self, _cx: &mut Cx, _this: Handle, _value: &str) -> rt::Result<()> {
        Ok(())
    }
    fn font(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ctx(this, |c| c.canvas.state.font.clone()))
    }
    fn set_font(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        // A value that is not a CSS `font` (no size) is ignored.
        if value.split_whitespace().any(|p| {
            let p = p.split('/').next().unwrap_or(p);
            ["px", "pt", "em", "rem", "%"]
                .iter()
                .any(|u| p.strip_suffix(u).is_some_and(|n| n.parse::<f32>().is_ok()))
                || matches!(
                    p,
                    "small" | "medium" | "large" | "x-large" | "x-small" | "larger" | "smaller"
                )
        }) {
            self.ctx(this, |c| c.canvas.state.font = value.trim().to_owned());
        }
        Ok(())
    }
    text_keyword!(
        text_align,
        set_text_align,
        CanvasTextAlign,
        text_align,
        g::CanvasTextAlign::Start
    );
    text_keyword!(
        text_baseline,
        set_text_baseline,
        CanvasTextBaseline,
        text_baseline,
        g::CanvasTextBaseline::Alphabetic
    );
    text_keyword!(
        direction,
        set_direction,
        CanvasDirection,
        direction,
        g::CanvasDirection::Inherit
    );
    fn letter_spacing(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("0px")
    }
    fn set_letter_spacing(&self, _cx: &mut Cx, _this: Handle, _value: &str) -> rt::Result<()> {
        Ok(())
    }
    fn font_kerning(&self, _this: Handle) -> g::CanvasFontKerning {
        g::CanvasFontKerning::Auto
    }
    fn set_font_kerning(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _value: g::CanvasFontKerning,
    ) -> rt::Result<()> {
        Ok(())
    }
    fn font_stretch(&self, _this: Handle) -> g::CanvasFontStretch {
        g::CanvasFontStretch::Normal
    }
    fn set_font_stretch(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _value: g::CanvasFontStretch,
    ) -> rt::Result<()> {
        Ok(())
    }
    fn font_variant_caps(&self, _this: Handle) -> g::CanvasFontVariantCaps {
        g::CanvasFontVariantCaps::Normal
    }
    fn set_font_variant_caps(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _value: g::CanvasFontVariantCaps,
    ) -> rt::Result<()> {
        Ok(())
    }
    fn text_rendering(&self, _this: Handle) -> g::CanvasTextRendering {
        g::CanvasTextRendering::Auto
    }
    fn set_text_rendering(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _value: g::CanvasTextRendering,
    ) -> rt::Result<()> {
        Ok(())
    }
    fn word_spacing(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("0px")
    }
    fn set_word_spacing(&self, _cx: &mut Cx, _this: Handle, _value: &str) -> rt::Result<()> {
        Ok(())
    }
}

impl CanvasPathDrawingStylesImpl for Engine {
    fn line_width(&self, this: Handle) -> f64 {
        self.ctx(this, |c| c.canvas.state.line_width as f64)
    }
    fn set_line_width(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value.is_finite() && value > 0.0 {
            self.ctx(this, |c| c.canvas.state.line_width = value as f32);
        }
        Ok(())
    }
    fn line_cap(&self, this: Handle) -> g::CanvasLineCap {
        match self.ctx(this, |c| c.canvas.state.line_cap) {
            LineCap::Butt => g::CanvasLineCap::Butt,
            LineCap::Round => g::CanvasLineCap::Round,
            LineCap::Square => g::CanvasLineCap::Square,
        }
    }
    fn set_line_cap(&self, _cx: &mut Cx, this: Handle, value: g::CanvasLineCap) -> rt::Result<()> {
        let cap = match value {
            g::CanvasLineCap::Butt => LineCap::Butt,
            g::CanvasLineCap::Round => LineCap::Round,
            g::CanvasLineCap::Square => LineCap::Square,
        };
        self.ctx(this, |c| c.canvas.state.line_cap = cap);
        Ok(())
    }
    fn line_join(&self, this: Handle) -> g::CanvasLineJoin {
        match self.ctx(this, |c| c.canvas.state.line_join) {
            LineJoin::Round => g::CanvasLineJoin::Round,
            LineJoin::Bevel => g::CanvasLineJoin::Bevel,
            _ => g::CanvasLineJoin::Miter,
        }
    }
    fn set_line_join(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: g::CanvasLineJoin,
    ) -> rt::Result<()> {
        let join = match value {
            g::CanvasLineJoin::Round => LineJoin::Round,
            g::CanvasLineJoin::Bevel => LineJoin::Bevel,
            g::CanvasLineJoin::Miter => LineJoin::Miter,
        };
        self.ctx(this, |c| c.canvas.state.line_join = join);
        Ok(())
    }
    fn miter_limit(&self, this: Handle) -> f64 {
        self.ctx(this, |c| c.canvas.state.miter_limit as f64)
    }
    fn set_miter_limit(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value.is_finite() && value > 0.0 {
            self.ctx(this, |c| c.canvas.state.miter_limit = value as f32);
        }
        Ok(())
    }
    fn line_dash_offset(&self, this: Handle) -> f64 {
        self.ctx(this, |c| c.canvas.state.dash_offset as f64)
    }
    fn set_line_dash_offset(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value.is_finite() {
            self.ctx(this, |c| c.canvas.state.dash_offset = value as f32);
        }
        Ok(())
    }
    fn set_line_dash(&self, _cx: &mut Cx, this: Handle, segments: Vec<f64>) -> rt::Result<()> {
        if segments.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return Ok(());
        }
        let mut d: Vec<f32> = segments.iter().map(|v| *v as f32).collect();
        if d.len() % 2 == 1 {
            d.extend(d.clone());
        }
        self.ctx(this, |c| c.canvas.state.dash = d);
        Ok(())
    }
    fn get_line_dash(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<f64>> {
        Ok(self.ctx(this, |c| {
            c.canvas.state.dash.iter().map(|v| *v as f64).collect()
        }))
    }
}

impl CanvasPathImpl for Engine {
    fn close_path(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.with_path(this, |p, _| p.close());
        Ok(())
    }
    fn move_to(&self, _cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<()> {
        self.with_path(this, |p, t| p.move_to(t, f(x), f(y)));
        Ok(())
    }
    fn line_to(&self, _cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<()> {
        self.with_path(this, |p, t| p.line_to(t, f(x), f(y)));
        Ok(())
    }
    fn quadratic_curve_to(
        &self,
        _cx: &mut Cx,
        this: Handle,
        cpx: f64,
        cpy: f64,
        x: f64,
        y: f64,
    ) -> rt::Result<()> {
        self.with_path(this, |p, t| p.quad_to(t, f(cpx), f(cpy), f(x), f(y)));
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn bezier_curve_to(
        &self,
        _cx: &mut Cx,
        this: Handle,
        cp1x: f64,
        cp1y: f64,
        cp2x: f64,
        cp2y: f64,
        x: f64,
        y: f64,
    ) -> rt::Result<()> {
        self.with_path(this, |p, t| {
            p.cubic_to(t, f(cp1x), f(cp1y), f(cp2x), f(cp2y), f(x), f(y))
        });
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn arc_to(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        radius: f64,
    ) -> rt::Result<()> {
        let mut ok = true;
        self.with_path(this, |p, t| {
            ok = p.arc_to(t, f(x1), f(y1), f(x2), f(y2), f(radius))
        });
        if ok {
            Ok(())
        } else {
            Err(exception(
                ExceptionName::IndexSizeError,
                "The radius is negative",
            ))
        }
    }
    fn rect(&self, _cx: &mut Cx, this: Handle, x: f64, y: f64, w: f64, h: f64) -> rt::Result<()> {
        self.with_path(this, |p, t| p.rect(t, f(x), f(y), f(w), f(h)));
        Ok(())
    }
    fn round_rect(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        radii: g::UnrestrictedDoubleOrDOMPointInitOrUnrestrictedDoubleOrDOMPointInitSequence,
    ) -> rt::Result<()> {
        use g::UnrestrictedDoubleOrDOMPointInitOrUnrestrictedDoubleOrDOMPointInitSequence as R;
        let r = match radii {
            R::UnrestrictedDouble(v) => v,
            R::DOMPointInit(p) => p.x,
            R::UnrestrictedDoubleOrDOMPointInitSequence(v) => match v.first() {
                Some(g::UnrestrictedDoubleOrDOMPointInit::UnrestrictedDouble(d)) => *d,
                Some(g::UnrestrictedDoubleOrDOMPointInit::DOMPointInit(p)) => p.x,
                None => 0.0,
            },
        };
        if r < 0.0 {
            return Err(rt::Error::Range("A radius is negative".into()));
        }
        let r = (r as f32)
            .min((w.abs() as f32) / 2.0)
            .min((h.abs() as f32) / 2.0);
        let (x, y, w, h) = (f(x), f(y), f(w), f(h));
        self.with_path(this, |p, t| {
            if r <= 0.0 {
                p.rect(t, x, y, w, h);
                return;
            }
            use std::f32::consts::{FRAC_PI_2, PI};
            p.move_to(t, x + r, y);
            p.line_to(t, x + w - r, y);
            p.ellipse(t, x + w - r, y + r, r, r, 0.0, -FRAC_PI_2, 0.0, false);
            p.line_to(t, x + w, y + h - r);
            p.ellipse(t, x + w - r, y + h - r, r, r, 0.0, 0.0, FRAC_PI_2, false);
            p.line_to(t, x + r, y + h);
            p.ellipse(t, x + r, y + h - r, r, r, 0.0, FRAC_PI_2, PI, false);
            p.line_to(t, x, y + r);
            p.ellipse(t, x + r, y + r, r, r, 0.0, PI, PI + FRAC_PI_2, false);
            p.close();
        });
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn arc(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
        radius: f64,
        start_angle: f64,
        end_angle: f64,
        counterclockwise: bool,
    ) -> rt::Result<()> {
        if radius < 0.0 {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The radius is negative",
            ));
        }
        self.with_path(this, |p, t| {
            p.ellipse(
                t,
                f(x),
                f(y),
                f(radius),
                f(radius),
                0.0,
                f(start_angle),
                f(end_angle),
                counterclockwise,
            )
        });
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn ellipse(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
        radius_x: f64,
        radius_y: f64,
        rotation: f64,
        start_angle: f64,
        end_angle: f64,
        counterclockwise: bool,
    ) -> rt::Result<()> {
        if radius_x < 0.0 || radius_y < 0.0 {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "A radius is negative",
            ));
        }
        self.with_path(this, |p, t| {
            p.ellipse(
                t,
                f(x),
                f(y),
                f(radius_x),
                f(radius_y),
                f(rotation),
                f(start_angle),
                f(end_angle),
                counterclockwise,
            )
        });
        Ok(())
    }
}

impl CanvasDrawImageImpl for Engine {
    fn draw_image_image_dx_dy(
        &self,
        _cx: &mut Cx,
        this: Handle,
        image: ImageSource,
        dx: f64,
        dy: f64,
    ) -> rt::Result<()> {
        self.draw_image_with(this, &image, |w, h| {
            ((0.0, 0.0, w, h), (f(dx), f(dy), w, h))
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn draw_image_image_dx_dy_dw_dh(
        &self,
        _cx: &mut Cx,
        this: Handle,
        image: ImageSource,
        dx: f64,
        dy: f64,
        dw: f64,
        dh: f64,
    ) -> rt::Result<()> {
        self.draw_image_with(this, &image, |w, h| {
            ((0.0, 0.0, w, h), (f(dx), f(dy), f(dw), f(dh)))
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn draw_image_image_sx_sy_sw_sh_dx_dy_dw_dh(
        &self,
        _cx: &mut Cx,
        this: Handle,
        image: ImageSource,
        sx: f64,
        sy: f64,
        sw: f64,
        sh: f64,
        dx: f64,
        dy: f64,
        dw: f64,
        dh: f64,
    ) -> rt::Result<()> {
        self.draw_image_with(this, &image, |_, _| {
            ((f(sx), f(sy), f(sw), f(sh)), (f(dx), f(dy), f(dw), f(dh)))
        })
    }
}

impl CanvasImageDataImpl for Engine {
    fn create_image_data_sw_sh_settings(
        &self,
        cx: &mut Cx,
        _this: Handle,
        sw: i32,
        sh: i32,
        _settings: g::ImageDataSettings,
    ) -> rt::Result<Handle> {
        self.new_image_data(cx, sw.unsigned_abs(), sh.unsigned_abs(), None)
    }
    fn create_image_data_image_data(
        &self,
        cx: &mut Cx,
        _this: Handle,
        image_data: Handle,
    ) -> rt::Result<Handle> {
        let (w, h) = match &*self.obj(image_data) {
            Object::ImageData(d) => (d.width, d.height),
            _ => (1, 1),
        };
        self.new_image_data(cx, w, h, None)
    }
    #[allow(clippy::too_many_arguments)]
    fn get_image_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        sx: i32,
        sy: i32,
        sw: i32,
        sh: i32,
        _settings: g::ImageDataSettings,
    ) -> rt::Result<Handle> {
        if sw == 0 || sh == 0 {
            return Err(exception(ExceptionName::IndexSizeError, "The size is zero"));
        }
        if !self.ctx(this, |c| c.origin_clean) {
            return Err(Self::tainted_error());
        }
        let (x, w) = if sw < 0 { (sx + sw, -sw) } else { (sx, sw) };
        let (y, h) = if sh < 0 { (sy + sh, -sh) } else { (sy, sh) };
        let bytes = self.ctx(this, |c| c.canvas.get_image_data(x, y, w as u32, h as u32));
        self.new_image_data(cx, w as u32, h as u32, Some(&bytes))
    }
    fn put_image_data_image_data_dx_dy(
        &self,
        cx: &mut Cx,
        this: Handle,
        image_data: Handle,
        dx: i32,
        dy: i32,
    ) -> rt::Result<()> {
        let Some((w, h, bytes)) = self.image_data_bytes(cx, image_data) else {
            return Ok(());
        };
        self.draw(this, |c| {
            c.put_image_data(&bytes, w, h, dx, dy, (0, 0, w as i32, h as i32))
        });
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn put_image_data_image_data_dx_dy_dirty_x_dirty_y_dirty_width_dirty_height(
        &self,
        cx: &mut Cx,
        this: Handle,
        image_data: Handle,
        dx: i32,
        dy: i32,
        dirty_x: i32,
        dirty_y: i32,
        dirty_width: i32,
        dirty_height: i32,
    ) -> rt::Result<()> {
        let Some((w, h, bytes)) = self.image_data_bytes(cx, image_data) else {
            return Ok(());
        };
        self.draw(this, |c| {
            c.put_image_data(
                &bytes,
                w,
                h,
                dx,
                dy,
                (dirty_x, dirty_y, dirty_width, dirty_height),
            )
        });
        Ok(())
    }
}

impl ImageDataImpl for Engine {
    fn width(&self, this: Handle) -> u32 {
        match &*self.obj(this) {
            Object::ImageData(d) => d.width,
            _ => 0,
        }
    }
    fn height(&self, this: Handle) -> u32 {
        match &*self.obj(this) {
            Object::ImageData(d) => d.height,
            _ => 0,
        }
    }
    fn data(&self, _cx: &mut Cx, this: Handle) -> rt::Result<g::Uint8ClampedArrayOrFloat16Array> {
        match &*self.obj(this) {
            Object::ImageData(d) => Ok(g::Uint8ClampedArrayOrFloat16Array::Uint8ClampedArray(
                d.data.clone(),
            )),
            _ => Err(rt::Error::Type("not an ImageData".into())),
        }
    }
    fn pixel_format(&self, _this: Handle) -> g::ImageDataPixelFormat {
        g::ImageDataPixelFormat::RgbaUnorm8
    }
    fn color_space(&self, _this: Handle) -> g::PredefinedColorSpace {
        g::PredefinedColorSpace::Srgb
    }
    fn constructor_sw_sh_settings(
        &self,
        cx: &mut Cx,
        sw: u32,
        sh: u32,
        _settings: g::ImageDataSettings,
    ) -> rt::Result<Handle> {
        self.new_image_data(cx, sw, sh, None)
    }
    fn constructor_data_sw_sh_settings(
        &self,
        cx: &mut Cx,
        data: g::Uint8ClampedArrayOrFloat16Array,
        sw: u32,
        sh: Option<u32>,
        _settings: g::ImageDataSettings,
    ) -> rt::Result<Handle> {
        let g::Uint8ClampedArrayOrFloat16Array::Uint8ClampedArray(array) = data else {
            return Err(exception(
                ExceptionName::NotSupportedError,
                "Float16 image data is not supported",
            ));
        };
        let scope = cx.scope();
        let obj = v8::Local::new(scope, array.global());
        let len = v8::Local::<v8::ArrayBufferView>::try_from(obj)
            .map(|v| v.byte_length())
            .unwrap_or(0);
        if sw == 0 || len == 0 || len % 4 != 0 || (len / 4) % sw as usize != 0 {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The data length does not fit",
            ));
        }
        let h = (len / 4 / sw as usize) as u32;
        if sh.is_some_and(|s| s != h) {
            return Err(exception(
                ExceptionName::IndexSizeError,
                "The height does not fit",
            ));
        }
        Ok(self.alloc(Object::ImageData(Box::new(ImageDataObj {
            width: sw,
            height: h,
            data: array,
        }))))
    }
}

impl Path2DImpl for Engine {
    fn add_path(
        &self,
        _cx: &mut Cx,
        this: Handle,
        path: Handle,
        t: g::DOMMatrix2DInit,
    ) -> rt::Result<()> {
        let [a, b, c, d, e, ff] = super::geometry::matrix_from_2d_init(&t)?;
        let other = self.path2d(path, |p| p.clone());
        let m = Transform::from_row(a as f32, b as f32, c as f32, d as f32, e as f32, ff as f32);
        self.path2d(this, |p| p.add_path(&other, m));
        Ok(())
    }
    fn constructor(&self, _cx: &mut Cx, path: Option<g::Path2DOrString>) -> rt::Result<Handle> {
        let p = match path {
            Some(g::Path2DOrString::Path2D(h)) => self.path2d(h, |p| p.clone()),
            Some(g::Path2DOrString::String(d)) => svg_path(&d),
            None => CanvasPath::new(),
        };
        Ok(self.alloc(Object::Path2D(p)))
    }
}

/// `new Path2D(d)`: SVG path data.
fn svg_path(d: &str) -> CanvasPath {
    let mut p = CanvasPath::new();
    let id = Transform::identity();
    let mut last = (0.0f32, 0.0f32);
    let mut start = last;
    for seg in svgtypes::SimplifyingPathParser::from(d) {
        let Ok(seg) = seg else {
            break;
        };
        use svgtypes::SimplePathSegment as S;
        match seg {
            S::MoveTo { x, y } => {
                p.move_to(&id, x as f32, y as f32);
                last = (x as f32, y as f32);
                start = last;
            }
            S::LineTo { x, y } => {
                p.line_to(&id, x as f32, y as f32);
                last = (x as f32, y as f32);
            }
            S::Quadratic { x1, y1, x, y } => {
                p.quad_to(&id, x1 as f32, y1 as f32, x as f32, y as f32);
                last = (x as f32, y as f32);
            }
            S::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => {
                p.cubic_to(
                    &id, x1 as f32, y1 as f32, x2 as f32, y2 as f32, x as f32, y as f32,
                );
                last = (x as f32, y as f32);
            }
            S::ClosePath => {
                p.close();
                last = start;
            }
        }
    }
    let _ = last;
    p
}

impl TextMetricsImpl for Engine {
    fn width(&self, this: Handle) -> f64 {
        self.metric(this).0
    }
    fn actual_bounding_box_left(&self, _this: Handle) -> f64 {
        0.0
    }
    fn actual_bounding_box_right(&self, this: Handle) -> f64 {
        self.metric(this).0
    }
    fn font_bounding_box_ascent(&self, this: Handle) -> f64 {
        self.metric(this).1
    }
    fn font_bounding_box_descent(&self, this: Handle) -> f64 {
        self.metric(this).2
    }
    fn actual_bounding_box_ascent(&self, this: Handle) -> f64 {
        self.metric(this).1
    }
    fn actual_bounding_box_descent(&self, this: Handle) -> f64 {
        self.metric(this).2
    }
    fn em_height_ascent(&self, this: Handle) -> f64 {
        self.metric(this).1
    }
    fn em_height_descent(&self, this: Handle) -> f64 {
        self.metric(this).2
    }
    fn hanging_baseline(&self, this: Handle) -> f64 {
        self.metric(this).1 * 0.8
    }
    fn alphabetic_baseline(&self, _this: Handle) -> f64 {
        0.0
    }
    fn ideographic_baseline(&self, this: Handle) -> f64 {
        -self.metric(this).2
    }
}

impl Engine {
    fn metric(&self, h: Handle) -> (f64, f64, f64) {
        match &*self.obj(h) {
            Object::TextMetrics {
                width,
                ascent,
                descent,
            } => (*width, *ascent, *descent),
            _ => (0.0, 0.0, 0.0),
        }
    }
}
