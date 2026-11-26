// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §8.10 `ImageBitmap` and `createImageBitmap()`: a bitmap that can be drawn without delay,
//! from an image, canvas, OffscreenCanvas, ImageBitmap, ImageData or Blob (decoded), cropped,
//! resized and flipped as its options say; OffscreenCanvas `transferToImageBitmap()`.
//!
//! The pixels are kept premultiplied (BGRA, top row first, as frames are); a bitmap made with
//! `premultiplyAlpha: "none"` is marked so, and WebGL uploads it un-premultiplied.

use std::sync::Arc;

use snowlark_bindings::generated::{self as g, ImageBitmapImpl};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsObject, JsPromise};
use snowlark_paint::image::Frame;
use snowlark_render::canvas::skia as tiny_skia;

use super::canvas::{frame_of, CanvasOwner};
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// An `ImageBitmap`.
pub struct BitmapObj {
    /// The bitmap; `None` once closed (or transferred).
    pub frame: Option<Arc<Frame>>,
    pub origin_clean: bool,
    /// Made with `premultiplyAlpha: "none"`: WebGL uploads it with its alpha not premultiplied.
    pub straight_alpha: bool,
}

/// What `createImageBitmap` was given, once its pixels are read (HTML §8.10 steps before "in
/// parallel"): the frame, whether it is origin-clean.
enum Source {
    Ready(Frame, bool),
    /// A Blob's bytes, decoded in a task.
    Encoded(Vec<u8>),
}

/// The crop and output size of `createImageBitmap(image, sx, sy, sw, sh, options)`.
#[derive(Clone, Copy)]
struct Crop {
    rect: Option<(i32, i32, i32, i32)>,
    resize: (Option<u32>, Option<u32>),
    flip_y: bool,
    straight_alpha: bool,
    pixelated: bool,
}

/// HTML §8.10 "the crop to source rectangle with formatting": the source cropped to `crop.rect`
/// (parts outside the source transparent black), scaled to the output size, flipped when asked.
fn crop_and_scale(src: &Frame, crop: Crop) -> Option<Frame> {
    let (sx, sy, sw, sh) = crop
        .rect
        .unwrap_or((0, 0, src.width as i32, src.height as i32));
    let (sw, sh) = (sw.unsigned_abs(), sh.unsigned_abs());
    let (ow, oh) = match crop.resize {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (
            w,
            ((sh as u64 * w as u64).div_ceil(sw.max(1) as u64)) as u32,
        ),
        (None, Some(h)) => (
            ((sw as u64 * h as u64).div_ceil(sh.max(1) as u64)) as u32,
            h,
        ),
        (None, None) => (sw, sh),
    };
    if ow == 0 || oh == 0 || ow as u64 * oh as u64 > 1 << 28 {
        return None;
    }
    let unchanged = (sx, sy) == (0, 0)
        && (sw, sh) == (src.width, src.height)
        && (ow, oh) == (sw, sh)
        && !crop.flip_y;
    if unchanged {
        return Some(src.clone());
    }
    let source = snowlark_render::canvas::pixmap_from_frame(src)?;
    let mut out = tiny_skia::Pixmap::new(ow, oh)?;
    let (kx, ky) = (ow as f32 / sw as f32, oh as f32 / sh as f32);
    let mut t =
        tiny_skia::Transform::from_row(kx, 0.0, 0.0, ky, -(sx as f32) * kx, -(sy as f32) * ky);
    if crop.flip_y {
        t = t.post_concat(tiny_skia::Transform::from_row(
            1.0, 0.0, 0.0, -1.0, 0.0, oh as f32,
        ));
    }
    let quality = if crop.pixelated {
        tiny_skia::FilterQuality::Nearest
    } else {
        tiny_skia::FilterQuality::Bilinear
    };
    let paint = tiny_skia::PixmapPaint {
        quality,
        ..Default::default()
    };
    out.draw_pixmap(0, 0, source.as_ref(), &paint, t, None);
    Some(frame_of(&out))
}

impl Engine {
    fn bitmap<R>(&self, h: Handle, f: impl FnOnce(&mut BitmapObj) -> R) -> Option<R> {
        match &mut *self.obj_mut(h) {
            Object::ImageBitmap(b) => Some(f(b)),
            _ => None,
        }
    }

    pub(crate) fn new_bitmap(
        &self,
        frame: Frame,
        origin_clean: bool,
        straight_alpha: bool,
    ) -> Handle {
        self.alloc(Object::ImageBitmap(Box::new(BitmapObj {
            frame: Some(Arc::new(frame)),
            origin_clean,
            straight_alpha,
        })))
    }

    /// An ImageBitmap's pixels (`None` once closed), whether it is origin-clean, and whether its
    /// alpha is straight.
    pub(crate) fn bitmap_pixels(&self, h: Handle) -> Option<(Arc<Frame>, bool, bool)> {
        self.bitmap(h, |b| {
            b.frame
                .clone()
                .map(|f| (f, b.origin_clean, b.straight_alpha))
        })
        .flatten()
    }

    /// Whether an ImageBitmap was closed (drawing or uploading it throws InvalidStateError).
    pub(crate) fn bitmap_closed(&self, h: Handle) -> bool {
        self.bitmap(h, |b| b.frame.is_none()).unwrap_or(true)
    }

    /// HTML §8.10 ImageBitmap's serialization steps: a copy of its bitmap data and whether its
    /// alpha is straight, or the DataCloneError's message: for storage, once closed (its
    /// [[Detached]] is true, StructuredSerializeInternal), or when it is not origin-clean.
    pub(crate) fn bitmap_serialization(
        &self,
        h: Handle,
    ) -> Result<(Arc<Frame>, bool), &'static str> {
        if self.for_storage.get() {
            return Err("An ImageBitmap cannot be stored");
        }
        match self.bitmap(h, |b| (b.frame.clone(), b.origin_clean, b.straight_alpha)) {
            Some((Some(_), false, _)) => Err("The ImageBitmap is not origin-clean"),
            Some((Some(frame), true, straight)) => Ok((frame, straight)),
            _ => Err("The ImageBitmap is detached"),
        }
    }

    /// The ImageBitmaps among a message's transfer list (HTML §2.7.5 StructuredSerializeWithTransfer
    /// step 4 for them): each once, else a DataCloneError. Their bitmaps go with the message by
    /// value; once it is serialized, [`Engine::transfer_bitmaps`] checks them and detaches them.
    pub(crate) fn bitmap_transfer_list(
        &self,
        cx: &mut Cx,
        transfer: &[JsObject],
    ) -> rt::Result<Vec<Handle>> {
        use snowlark_bindings::generated::ids;
        let mut list = Vec::new();
        for o in transfer {
            let h = {
                let scope = cx.scope();
                let obj = v8::Local::new(scope, o.global());
                rt::unwrap(&obj, ids::IMAGE_BITMAP, ids::IMAGE_BITMAP_END)
            };
            let Some(h) = h else { continue };
            if list.contains(&h) {
                return Err(exception(
                    ExceptionName::DataCloneError,
                    "An object is in the transfer list twice",
                ));
            }
            list.push(h);
        }
        Ok(list)
    }

    /// HTML §2.7.5 StructuredSerializeWithTransfer steps 6-7 for the ImageBitmaps of the
    /// transfer list, after the value is serialized (so an exception the serialization throws
    /// comes first): a detached one throws a DataCloneError, as does one that is not
    /// origin-clean (§8.10 transfer steps); then each is detached ("Unset value's bitmap data":
    /// its width and height read 0, and drawing it throws).
    pub(crate) fn transfer_bitmaps(&self, list: &[Handle]) -> rt::Result<()> {
        for &h in list {
            let state = self.bitmap(h, |b| (b.frame.is_some(), b.origin_clean));
            match state {
                Some((true, true)) => {}
                Some((true, false)) => {
                    return Err(exception(
                        ExceptionName::DataCloneError,
                        "The ImageBitmap is not origin-clean",
                    ))
                }
                _ => {
                    return Err(exception(
                        ExceptionName::DataCloneError,
                        "The ImageBitmap is detached",
                    ))
                }
            }
        }
        for &h in list {
            self.bitmap(h, |b| b.frame = None);
        }
        Ok(())
    }

    /// HTML §4.12.5.3 `transferToImageBitmap()`: the bitmap as it is, and the OffscreenCanvas's
    /// bitmap made transparent black (a 2D context keeps its state).
    pub(crate) fn transfer_to_image_bitmap(&self, oc: Handle) -> rt::Result<Handle> {
        if self.offscreen_detached(oc) {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The OffscreenCanvas is detached",
            ));
        }
        let src = CanvasOwner::Offscreen(oc);
        let (w, h) = self.source_size(src);
        let ctx2d = self.source_2d(src);
        if ctx2d.is_none() && self.source_webgl(src).is_none() {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The OffscreenCanvas has no rendering context",
            ));
        }
        let clean = self.canvas_origin_clean(src);
        let frame = self.source_frame(src).unwrap_or_else(|| Frame {
            width: w,
            height: h,
            pixels: vec![0; w as usize * h as usize * 4],
            delay_ms: 0,
        });
        if let Some(ctx) = ctx2d {
            self.clear_2d_bitmap(ctx);
        }
        Ok(self.new_bitmap(frame, clean, false))
    }

    /// The pixels `createImageBitmap` reads from `image` now (HTML §8.10 "check the usability of
    /// the image argument" and the per-kind steps), or the exception it rejects with.
    fn bitmap_source(
        &self,
        cx: &mut Cx,
        image: &g::HTMLImageElementOrSVGImageElementOrHTMLVideoElementOrHTMLCanvasElementOrImageBitmapOrOffscreenCanvasOrBlobOrImageData,
    ) -> rt::Result<Source> {
        use g::HTMLImageElementOrSVGImageElementOrHTMLVideoElementOrHTMLCanvasElementOrImageBitmapOrOffscreenCanvasOrBlobOrImageData as S;
        let unusable = || {
            exception(
                ExceptionName::InvalidStateError,
                "The image has no pixels to read",
            )
        };
        let canvas = |src: CanvasOwner| -> rt::Result<Source> {
            let (w, h) = self.source_size(src);
            if w == 0 || h == 0 {
                return Err(unusable());
            }
            let frame = self.source_frame(src).unwrap_or_else(|| Frame {
                width: w,
                height: h,
                pixels: vec![0; w as usize * h as usize * 4],
                delay_ms: 0,
            });
            Ok(Source::Ready(frame, self.canvas_origin_clean(src)))
        };
        match image {
            S::HTMLImageElement(h) => {
                let el = self.n(*h);
                let decoded = self
                    .with_node_renderer(el, |r, _| r.image_pixels(el))
                    .flatten()
                    .ok_or_else(unusable)?;
                let frame = decoded.default_frame().clone();
                if frame.width == 0 || frame.height == 0 {
                    return Err(unusable());
                }
                Ok(Source::Ready(frame, self.img_origin_clean(el)))
            }
            S::HTMLCanvasElement(h) => canvas(CanvasOwner::Element(self.n(*h))),
            S::OffscreenCanvas(h) => {
                if self.offscreen_detached(*h) {
                    return Err(unusable());
                }
                canvas(CanvasOwner::Offscreen(*h))
            }
            S::ImageBitmap(h) => {
                let (frame, clean, _) = self.bitmap_pixels(*h).ok_or_else(unusable)?;
                Ok(Source::Ready((*frame).clone(), clean))
            }
            S::ImageData(h) => {
                let (w, hh, rgba) = self.image_data_bytes(cx, *h).ok_or_else(unusable)?;
                // (ImageData is straight RGBA: premultiplied BGRA as the bitmap keeps it)
                let mut pixels = rgba;
                for px in pixels.as_chunks_mut::<4>().0 {
                    let a = px[3] as u32;
                    let m = |v: u8| ((v as u32 * a + 127) / 255) as u8;
                    *px = [m(px[2]), m(px[1]), m(px[0]), px[3]];
                }
                Ok(Source::Ready(
                    Frame {
                        width: w,
                        height: hh,
                        pixels,
                        delay_ms: 0,
                    },
                    true,
                ))
            }
            S::Blob(h) => Ok(Source::Encoded(self.blob_contents(*h).0.to_vec())),
            // (a video's frame lives on the graphics card here, and an SVG `image` element's
            // image is not exposed to canvas)
            _ => Err(unusable()),
        }
    }

    /// HTML §8.10 `createImageBitmap(image, [sx, sy, sw, sh,] options)`.
    pub(crate) fn create_bitmap(
        &self,
        cx: &mut Cx,
        image: g::HTMLImageElementOrSVGImageElementOrHTMLVideoElementOrHTMLCanvasElementOrImageBitmapOrOffscreenCanvasOrBlobOrImageData,
        rect: Option<(i32, i32, i32, i32)>,
        options: g::ImageBitmapOptions,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        // "If either sw or sh is given and is 0, then return a promise rejected with a RangeError."
        if rect.is_some_and(|(_, _, w, h)| w == 0 || h == 0) {
            p.reject(
                cx,
                rt::Error::Range("The source width or height is 0".into()),
            );
            return Ok(p);
        }
        // "If either options's resizeWidth or options's resizeHeight is present and is 0, then
        // return a promise rejected with an InvalidStateError."
        if options.resize_width == Some(0) || options.resize_height == Some(0) {
            p.reject(
                cx,
                exception(ExceptionName::InvalidStateError, "The resize size is 0"),
            );
            return Ok(p);
        }
        let crop = Crop {
            rect,
            resize: (options.resize_width, options.resize_height),
            flip_y: options.image_orientation == g::ImageOrientation::FlipY,
            straight_alpha: options.premultiply_alpha == g::PremultiplyAlpha::None,
            pixelated: options.resize_quality == g::ResizeQuality::Pixelated,
        };
        let source = match self.bitmap_source(cx, &image) {
            Ok(s) => s,
            Err(e) => {
                p.reject(cx, e);
                return Ok(p);
            }
        };
        let promise = p.clone();
        // The rest runs "in parallel" and resolves in a task: here, a task of its own.
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            let made = match source {
                Source::Ready(frame, clean) => crop_and_scale(&frame, crop).map(|f| (f, clean)),
                Source::Encoded(bytes) => snowlark_paint::image::decode(&bytes)
                    .ok()
                    .and_then(|d| crop_and_scale(d.default_frame(), crop))
                    .map(|f| (f, true)),
            };
            match made {
                Some((frame, clean)) => {
                    let h = e.new_bitmap(frame, clean, crop.straight_alpha);
                    promise.resolve(cx, h);
                }
                None => promise.reject(
                    cx,
                    exception(
                        ExceptionName::InvalidStateError,
                        "The image could not be decoded",
                    ),
                ),
            }
        });
        Ok(p)
    }
}

impl ImageBitmapImpl for Engine {
    fn width(&self, this: Handle) -> u32 {
        self.bitmap(this, |b| b.frame.as_ref().map_or(0, |f| f.width))
            .unwrap_or(0)
    }
    fn height(&self, this: Handle) -> u32 {
        self.bitmap(this, |b| b.frame.as_ref().map_or(0, |f| f.height))
            .unwrap_or(0)
    }
    /// HTML §8.10 `close()`: the bitmap goes; width and height read 0 from now on.
    fn close(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.bitmap(this, |b| b.frame = None);
        Ok(())
    }
}
