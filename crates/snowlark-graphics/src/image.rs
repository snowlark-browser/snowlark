// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Images: decoding to premultiplied BGRA pixels (PNG, JPEG, GIF, WebP and AVIF in Rust with their
//! colour profiles applied, `crate::decoders`; animated GIF, APNG, WebP and AVIF with their frames
//! composited, see [`crate::animation`]), and the store display lists refer to images by. The
//! formats not decoded here (BMP, ICO, TIFF and others) are the platform's: [`decode`] says
//! [`Error::Unsupported`] for them, and `snowlark-paint`'s `image::decode` hands them on.
//!
//! Decoded pixels are plain Rust memory (`Send + Sync`); each rasterizer uploads them to its own
//! bitmaps. A still image's EXIF orientation is applied to its pixels when it is decoded
//! (CSS Images 3 §5.3 `image-orientation: from-image`, the initial value; [`DecodedImage::unoriented`]
//! gives the pixels as stored, for `none`); density (`srcset` `x`/`w` descriptors) is the caller's.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use crate::animation::{self, Blend, Canvas, Dispose, Region, Timing};
use crate::color_profile::ColorTransform;
use crate::decoders::{self, Format, GifDecoder};
use crate::display_list::ImageId;
use crate::geom::Color;
pub use crate::Error;
use crate::Result;

/// Images larger than this many pixels (per frame) are refused: a hostile file can't make the
/// process allocate gigabytes.
pub const MAX_PIXELS: u64 = 100_000_000;
/// The most memory an animation's decoded frames are kept in: an animation whose frames take more
/// keeps its file instead and decodes each frame as it shows ([`Animation::encoded`],
/// [`FrameDecoder`]). 32 MiB holds 33 frames of 500 × 500, or 8 of 1000 × 1000.
pub const ANIMATION_FRAMES_BYTES: u64 = 32 << 20;

/// Whether AVIF images are decoded here (CrabbyAvif with dav1d), with their colour profiles and
/// image sequences; a build without the dav1d library leaves them to the platform's decoders.
pub const AVIF: bool = decoders::AVIF;

/// One frame: premultiplied BGRA, rows `width * 4` bytes apart.
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// How long the frame shows in an animation, in milliseconds (after
    /// [`crate::animation::frame_delay`]).
    pub delay_ms: u32,
}

impl Frame {
    /// The pixel at (x, y), un-premultiplied.
    pub fn pixel(&self, x: u32, y: u32) -> Color {
        pixel_at(&self.pixels, self.width, x, y)
    }
}

/// What makes an image animated (HTML §4.8.4.3: "an animated image").
#[derive(Debug)]
pub struct Animation {
    pub timing: Timing,
    /// The image shown where the animation is not ("the default image of the animation", which
    /// canvas `drawImage` draws: HTML §4.12.5.1.14), when it is not [`DecodedImage::frames`]`[0]`:
    /// an APNG's `IDAT` image that is not one of its frames, and the first frame of an animation
    /// whose frames are decoded as they show.
    pub default_image: Option<Frame>,
    /// The file, for an animation whose frames are not all kept ([`ANIMATION_FRAMES_BYTES`]): the
    /// image holds the frame shown now, which a [`FrameDecoder`] of this file replaces
    /// ([`ImageStore::show_frame`]).
    pub encoded: Option<Arc<[u8]>>,
}

/// A decoded image: one frame, or an animation's frames already composited to full size.
#[derive(Clone, Debug)]
pub struct DecodedImage {
    pub frames: Vec<Frame>,
    /// The EXIF orientation (1..=8; 1 is upright), already applied to the pixels of a still image
    /// decoded by [`decode`] (an animation's frames are left as stored).
    pub orientation: u8,
    /// Set for an animated image (two frames or more).
    pub animation: Option<Arc<Animation>>,
}

impl DecodedImage {
    /// An image of one frame.
    pub fn still(frame: Frame) -> DecodedImage {
        DecodedImage {
            frames: vec![frame],
            orientation: 1,
            animation: None,
        }
    }

    /// The memory the pixels (and the file kept for a long animation) take.
    pub fn bytes(&self) -> u64 {
        let frames: usize = self.frames.iter().map(|f| f.pixels.len()).sum();
        let extra = self.animation.as_ref().map_or(0, |a| {
            a.default_image.as_ref().map_or(0, |f| f.pixels.len())
                + a.encoded.as_ref().map_or(0, |e| e.len())
        });
        (frames + extra) as u64
    }

    pub fn width(&self) -> u32 {
        self.frames[0].width
    }

    pub fn height(&self) -> u32 {
        self.frames[0].height
    }

    /// The image to use where an animation does not play (canvas `drawImage`, HTML §4.12.5.1.14:
    /// "the default image of the animation ... or, if there is no such image, the first frame").
    pub fn default_frame(&self) -> &Frame {
        self.animation
            .as_ref()
            .and_then(|a| a.default_image.as_ref())
            .unwrap_or(&self.frames[0])
    }
}

/// Decodes an encoded image held in memory. An animated GIF, APNG or WebP decodes to all its frames
/// composited (or, past [`ANIMATION_FRAMES_BYTES`], to its first frame and its file). A format not
/// decoded here is [`Error::Unsupported`].
pub fn decode(bytes: &[u8]) -> Result<DecodedImage> {
    decode_with_budget(bytes, ANIMATION_FRAMES_BYTES)
}

/// [`decode`] keeping an animation's frames when they take at most `budget` bytes.
fn decode_with_budget(bytes: &[u8], budget: u64) -> Result<DecodedImage> {
    let Some(format) = decoders::sniff(bytes) else {
        // (BMP, ICO, TIFF and the formats the platform has a codec for)
        return Err(Error::Unsupported);
    };
    let kind = match format {
        Format::Png => animation::parse_apng(bytes)
            .filter(|a| a.frames.len() > 1)
            .map(|a| Kind::Apng(a, None)),
        Format::Gif => Some(Kind::Gif(None)),
        Format::Webp => animation::parse_webp_animation(bytes).map(Kind::Webp),
        Format::Jpeg => None,
        Format::Avif => return decode_avif(bytes, budget),
    };
    if let Some(kind) = kind {
        // (the still image when the animation doesn't decode)
        match decode_animation(Arc::from(bytes), kind, budget) {
            Ok(image) => return Ok(image),
            Err(e) if format != Format::Png => return Err(e),
            Err(_) => {}
        }
    }
    let (frame, orientation) = match format {
        Format::Png => (
            decoders::png(bytes, None)?.0,
            png_exif_orientation(bytes).unwrap_or(1),
        ),
        Format::Jpeg => (decoders::jpeg(bytes)?, exif_orientation(bytes).unwrap_or(1)),
        // (no EXIF orientation for WebP, as in other engines)
        Format::Webp => (decoders::webp(bytes)?, 1),
        Format::Gif | Format::Avif => return Err(Error::Invalid),
    };
    let mut image = DecodedImage::still(oriented(frame, orientation));
    image.orientation = orientation;
    Ok(image)
}

/// An AVIF image, or an image sequence's frames (each a whole image: no blending or disposal).
/// A still image's `irot` and `imir` turn it as an EXIF orientation would.
fn decode_avif(bytes: &[u8], budget: u64) -> Result<DecodedImage> {
    let mut d = decoders::avif_decoder(bytes)?;
    if d.frames() > 1 {
        return decode_animation(Arc::from(bytes), Kind::Avif(Some(Box::new(d))), budget);
    }
    let orientation = d.orientation;
    let frame = decoders::avif_next(&mut d)?;
    let mut image = DecodedImage::still(oriented(frame, orientation));
    image.orientation = orientation;
    Ok(image)
}

/// `frame` as shown with EXIF `orientation`, without a copy when it is upright.
pub fn oriented(frame: Frame, orientation: u8) -> Frame {
    if (2..=8).contains(&orientation) {
        orient(&frame, orientation)
    } else {
        frame
    }
}

/// Where the pixel (x, y) of an image shown with EXIF `orientation` comes from in the stored
/// `w` x `h` image (EXIF 2.32 §4.6.4 Orientation: 2 mirrors, 3 turns 180°, 4 flips, 5 transposes,
/// 6 turns 90° clockwise, 7 transverses, 8 turns 90° counter-clockwise).
fn oriented_source(orientation: u8, (w, h): (u32, u32), (x, y): (u32, u32)) -> (u32, u32) {
    match orientation {
        2 => (w - 1 - x, y),
        3 => (w - 1 - x, h - 1 - y),
        4 => (x, h - 1 - y),
        5 => (y, x),
        6 => (y, h - 1 - x),
        7 => (w - 1 - y, h - 1 - x),
        8 => (w - 1 - y, x),
        _ => (x, y),
    }
}

/// `frame` as shown with EXIF `orientation` (its size swapped for 5 to 8).
pub fn orient(frame: &Frame, orientation: u8) -> Frame {
    if !(2..=8).contains(&orientation) || frame.width == 0 || frame.height == 0 {
        return frame.clone();
    }
    let (w, h) = (frame.width, frame.height);
    let (ow, oh) = if orientation >= 5 { (h, w) } else { (w, h) };
    let mut pixels = vec![0u8; frame.pixels.len()];
    for y in 0..oh {
        for x in 0..ow {
            let (sx, sy) = oriented_source(orientation, (w, h), (x, y));
            let from = ((sy * w + sx) * 4) as usize;
            let to = ((y * ow + x) * 4) as usize;
            pixels[to..to + 4].copy_from_slice(&frame.pixels[from..from + 4]);
        }
    }
    Frame {
        width: ow,
        height: oh,
        pixels,
        delay_ms: frame.delay_ms,
    }
}

/// The inverse of [`orient`]: the stored pixels of a frame shown with `orientation`.
fn unorient(frame: &Frame, orientation: u8) -> Frame {
    // (mirrors and half turns are their own inverses; a quarter turn's is the other one)
    let inverse = match orientation {
        6 => 8,
        8 => 6,
        o => o,
    };
    orient(frame, inverse)
}

impl DecodedImage {
    /// The image as stored, with its EXIF orientation not applied (`image-orientation: none`), when
    /// it was applied (`None` for an upright image or an animation).
    pub fn unoriented(&self) -> Option<DecodedImage> {
        if self.orientation <= 1 || self.animation.is_some() {
            return None;
        }
        Some(DecodedImage {
            frames: self
                .frames
                .iter()
                .map(|f| unorient(f, self.orientation))
                .collect(),
            orientation: 1,
            animation: None,
        })
    }
}

/// The EXIF orientation an encoded JPEG (its APP1 `Exif` segment) or PNG (its `eXIf` chunk) says,
/// read without decoding the image; `None` when it has none.
pub fn exif_orientation(bytes: &[u8]) -> Option<u8> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png_exif_orientation(bytes);
    }
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    // JPEG markers up to the start of scan: APP1 holds "Exif\0\0" and a TIFF structure.
    let mut at = 2;
    while at + 4 <= bytes.len() && bytes[at] == 0xff {
        let marker = bytes[at + 1];
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        let len = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
        let data = bytes.get(at + 4..at + 2 + len)?;
        if marker == 0xe1 && data.starts_with(b"Exif\0\0") {
            return tiff_orientation(&data[6..]);
        }
        at += 2 + len;
    }
    None
}

/// The orientation in a PNG's `eXIf` chunk (PNG 3rd edition §11.3.6.1), if it has one.
fn png_exif_orientation(png: &[u8]) -> Option<u8> {
    let mut at = 8;
    while at + 12 <= png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().ok()?) as usize;
        let kind = &png[at + 4..at + 8];
        let data = png.get(at + 8..at + 8 + len)?;
        if kind == b"eXIf" {
            return tiff_orientation(data);
        }
        if kind == b"IDAT" || kind == b"IEND" {
            // (an eXIf chunk after the image data is not to be used, PNG §11.3.6.1)
            return None;
        }
        at += 12 + len;
    }
    None
}

/// The Orientation tag (0x0112) of an EXIF TIFF structure's first IFD.
fn tiff_orientation(tiff: &[u8]) -> Option<u8> {
    let le = match tiff.get(0..4)? {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return None,
    };
    let u16_at = |i: usize| -> Option<u16> {
        let b: [u8; 2] = tiff.get(i..i + 2)?.try_into().ok()?;
        Some(if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |i: usize| -> Option<u32> {
        let b: [u8; 4] = tiff.get(i..i + 4)?.try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    for i in 0..count.min(512) {
        let entry = ifd + 2 + i * 12;
        if u16_at(entry)? == 0x0112 {
            let v = u16_at(entry + 8)?;
            return (1..=8).contains(&v).then_some(v as u8);
        }
    }
    None
}

/// An animation's frames: all of them when they fit `budget`, else the first. A GIF or animated
/// WebP of one frame (or whose later frames don't decode) is a still image; an APNG's is its
/// `IDAT` image, which the caller decodes.
fn decode_animation(bytes: Arc<[u8]>, kind: Kind, budget: u64) -> Result<DecodedImage> {
    let still_of_one = !matches!(kind, Kind::Apng(..));
    let mut decoder = FrameDecoder::with_kind(bytes.clone(), kind)?;
    let frame_bytes = decoder.width as u64 * decoder.height as u64 * 4;
    let mut timing = decoder.timing.clone();
    let streamed = timing.frames() > 1 && frame_bytes * timing.frames() as u64 > budget;
    let mut frames = Vec::new();
    let wanted = if streamed { 1 } else { timing.frames() };
    for i in 0..wanted {
        let frame = if i + 1 == wanted {
            decoder.last_frame(i)
        } else {
            decoder.frame(i)
        };
        match frame {
            Ok(f) => frames.push(f),
            // (frames that don't decode end the animation; without a first one there is none)
            Err(e) if i == 0 => return Err(e),
            Err(_) => break,
        }
    }
    if frames.len() < 2 && !streamed {
        if still_of_one {
            let mut frame = frames.remove(0);
            frame.delay_ms = 0;
            return Ok(DecodedImage::still(frame));
        }
        return Err(Error::Invalid);
    }
    if !streamed {
        timing.truncate(frames.len());
    }
    let default_image = match decoder.default_image()? {
        Some(f) => Some(f),
        None if streamed => Some(frames[0].clone()),
        None => None,
    };
    Ok(DecodedImage {
        frames,
        orientation: 1,
        animation: Some(Arc::new(Animation {
            timing,
            default_image,
            encoded: streamed.then_some(bytes),
        })),
    })
}

/// The animated formats, with the decoder each keeps between frames (`None` until the first
/// frame, and again when it starts over).
enum Kind {
    /// The `gif` crate decodes each frame's own rectangle; composition and the loop count are ours.
    Gif(Option<Box<GifDecoder>>),
    /// Each frame is made a PNG of its own ([`animation::Apng`]) for the PNG decoder; the colour
    /// conversion the file asks for is worked out once.
    Apng(animation::Apng, Option<Option<ColorTransform>>),
    /// Each frame is made a WebP of its own ([`animation::WebpAnimation::frame_webp`]) for the
    /// WebP decoder; composition is ours.
    Webp(animation::WebpAnimation),
    /// An AVIF image sequence: CrabbyAvif decodes its frames in order (each depends on the ones
    /// before it, back to a key frame).
    Avif(Option<Box<decoders::AvifDecoder>>),
}

/// Decodes an animation's frames one after another (composited to full size, as they show): what
/// keeps an animation too big to hold every frame playing. Frames come in order; asking for an
/// earlier one starts again from the first.
pub struct FrameDecoder {
    kind: Kind,
    bytes: Arc<[u8]>,
    width: u32,
    height: u32,
    timing: Timing,
    canvas: Canvas,
    /// The frame the next [`FrameDecoder::frame`] call decodes without starting over.
    next: usize,
}

impl FrameDecoder {
    /// A decoder of an animated image's file (as [`Animation::encoded`] holds it).
    pub fn new(bytes: Arc<[u8]>) -> Result<FrameDecoder> {
        let kind = match decoders::sniff(&bytes) {
            Some(Format::Gif) => Kind::Gif(None),
            Some(Format::Png) => {
                Kind::Apng(animation::parse_apng(&bytes).ok_or(Error::Invalid)?, None)
            }
            Some(Format::Webp) => {
                Kind::Webp(animation::parse_webp_animation(&bytes).ok_or(Error::Invalid)?)
            }
            Some(Format::Avif) => Kind::Avif(None),
            _ => return Err(Error::Invalid),
        };
        FrameDecoder::with_kind(bytes, kind)
    }

    fn with_kind(bytes: Arc<[u8]>, mut kind: Kind) -> Result<FrameDecoder> {
        if let Kind::Avif(d @ None) = &mut kind {
            *d = Some(Box::new(decoders::avif_decoder(&bytes)?));
        }
        let (width, height, timing) = match &kind {
            Kind::Gif(_) => {
                let layout = decoders::gif_layout(bytes.clone())?;
                let timing = Timing::new(layout.delays, animation::gif_plays(&bytes));
                (layout.width, layout.height, timing)
            }
            Kind::Apng(a, _) => (a.width, a.height, Timing::new(a.delays(), a.plays)),
            Kind::Webp(a) => (
                a.width,
                a.height,
                Timing::new(a.durations.iter().copied(), a.plays),
            ),
            Kind::Avif(d) => {
                let d = d.as_ref().ok_or(Error::Invalid)?;
                (
                    d.width,
                    d.height,
                    Timing::new(d.durations.iter().copied(), d.plays),
                )
            }
        };
        decoders::check_size(width, height)?;
        Ok(FrameDecoder {
            kind,
            bytes,
            width,
            height,
            timing,
            canvas: Canvas::new(0, 0),
            next: 0,
        })
    }

    /// The animation's frame timeline.
    pub fn timing(&self) -> &Timing {
        &self.timing
    }

    /// The default image when it is not the first frame (an APNG whose `IDAT` image is not part of
    /// the animation).
    fn default_image(&self) -> Result<Option<Frame>> {
        match &self.kind {
            Kind::Apng(a, _) if !a.default_is_first => {
                Ok(Some(decoders::png(&self.bytes, None)?.0))
            }
            _ => Ok(None),
        }
    }

    /// Frame `index` composited to full size (with its duration).
    pub fn frame(&mut self, index: usize) -> Result<Frame> {
        self.draw_to(index)?;
        Ok(Frame {
            width: self.width,
            height: self.height,
            pixels: self.canvas.pixels.clone(),
            delay_ms: self.timing.delay(index),
        })
    }

    /// [`FrameDecoder::frame`] for the last frame asked of this decoder: its pixels are moved out
    /// rather than copied (a later call starts again from the first frame).
    fn last_frame(&mut self, index: usize) -> Result<Frame> {
        self.draw_to(index)?;
        let canvas = std::mem::replace(&mut self.canvas, Canvas::new(0, 0));
        Ok(Frame {
            width: self.width,
            height: self.height,
            pixels: canvas.pixels,
            delay_ms: self.timing.delay(index),
        })
    }

    /// Draws the frames up to `index` into the canvas (from the first, when `index` comes before
    /// the one drawn last).
    fn draw_to(&mut self, index: usize) -> Result<()> {
        if index >= self.timing.frames() {
            return Err(Error::Invalid);
        }
        if index < self.next || self.canvas.width == 0 {
            // (a decoder that has gone past the first frame starts again from the file)
            if let Kind::Avif(d) = &mut self.kind {
                if self.next > 0 {
                    *d = None;
                }
            }
            self.next = 0;
            if self.canvas.width == 0 {
                self.canvas = Canvas::new(self.width, self.height);
            } else {
                self.canvas.reset();
            }
            if let Kind::Gif(d) = &mut self.kind {
                *d = None;
            }
        }
        while self.next <= index {
            self.draw_next()?;
        }
        Ok(())
    }

    /// Draws frame `self.next` into the canvas.
    fn draw_next(&mut self) -> Result<()> {
        let i = self.next;
        match &mut self.kind {
            Kind::Gif(decoder) => {
                if decoder.is_none() {
                    *decoder = Some(Box::new(decoders::gif_decoder(self.bytes.clone(), true)?));
                }
                let d = decoder.as_mut().ok_or(Error::Invalid)?;
                let f = decoders::gif_next(d)?.ok_or(Error::Invalid)?;
                // (source-over where the frame has transparent pixels: the others are opaque)
                let blend = if f.transparent {
                    Blend::Over
                } else {
                    Blend::Source
                };
                self.canvas
                    .draw(&f.pixels, f.at.width, f.at, blend, f.dispose);
            }
            Kind::Apng(a, color) => {
                let f = &a.frames[i];
                let png = a.frame_png(&self.bytes, i);
                let (piece, own) = decoders::png(&png, color.as_ref().map(Option::as_ref))?;
                if color.is_none() {
                    *color = Some(own);
                }
                let at = Region {
                    x: f.x,
                    y: f.y,
                    width: piece.width.min(f.width),
                    height: piece.height.min(f.height),
                };
                self.canvas
                    .draw(&piece.pixels, piece.width, at, f.blend, f.dispose);
            }
            Kind::Webp(a) => {
                let f = &a.frames[i];
                let piece = decoders::webp(&a.frame_webp(&self.bytes, i))?;
                let at = Region {
                    x: f.x,
                    y: f.y,
                    width: piece.width.min(f.width),
                    height: piece.height.min(f.height),
                };
                self.canvas
                    .draw(&piece.pixels, piece.width, at, f.blend, f.dispose);
            }
            Kind::Avif(d) => {
                if d.is_none() {
                    *d = Some(Box::new(decoders::avif_decoder(&self.bytes)?));
                }
                let d = d.as_mut().ok_or(Error::Invalid)?;
                let piece = decoders::avif_next(d)?;
                let at = Region {
                    x: 0,
                    y: 0,
                    width: piece.width.min(self.width),
                    height: piece.height.min(self.height),
                };
                self.canvas
                    .draw(&piece.pixels, piece.width, at, Blend::Source, Dispose::None);
            }
        }
        self.next = i + 1;
        Ok(())
    }
}

/// What an image shows now, as a painter asks for it (`snowlark-paint`'s `ImageResolver`).
#[derive(Clone, Debug)]
pub struct ShownImage {
    /// Which of the image's frames.
    pub frame: usize,
    /// [`ImageStore::version`]: which pixels these are.
    pub version: u64,
    pub width: u32,
    pub height: u32,
    /// The pixels, where they are still at hand: a table of the raster side gives them to the
    /// device when they come and has none.
    pub image: Option<Arc<DecodedImage>>,
}

/// The images a page's display lists refer to. Shared between the page thread (which adds images and
/// advances animations) and the threads that draw them or send them to the raster side.
#[derive(Default)]
pub struct ImageStore {
    slots: RwLock<Vec<Option<Slot>>>,
    /// Numbers the pixels each [`ImageStore::add`] and [`ImageStore::replace`] stores.
    versions: std::sync::atomic::AtomicU64,
    /// The bytes of every image stored (kept as images come and go, so reading it walks nothing).
    bytes: std::sync::atomic::AtomicU64,
}

struct Slot {
    image: Arc<DecodedImage>,
    frame: AtomicU32,
    version: u64,
}

impl ImageStore {
    pub fn new() -> ImageStore {
        ImageStore::default()
    }

    /// The memory the stored images take.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    fn grew(&self, by: u64) {
        self.bytes.fetch_add(by, Ordering::Relaxed);
    }

    fn shrank(&self, by: u64) {
        let _ = self
            .bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                Some(b.saturating_sub(by))
            });
    }

    pub fn add(&self, image: DecodedImage) -> ImageId {
        self.grew(image.bytes());
        let mut slots = self.slots.write().unwrap();
        let slot = Slot {
            image: Arc::new(image),
            frame: AtomicU32::new(0),
            version: self.versions.fetch_add(1, Ordering::Relaxed) + 1,
        };
        if let Some(i) = slots.iter().position(Option::is_none) {
            slots[i] = Some(slot);
            ImageId(i as u32)
        } else {
            slots.push(Some(slot));
            ImageId(slots.len() as u32 - 1)
        }
    }

    /// Replaces the pixels of an image (a canvas that drew): painters upload them again.
    pub fn replace(&self, id: ImageId, image: DecodedImage) {
        if let Some(s) = self.slots.write().unwrap().get_mut(id.0 as usize) {
            self.shrank(s.as_ref().map_or(0, |o| o.image.bytes()));
            self.grew(image.bytes());
            *s = Some(Slot {
                image: Arc::new(image),
                frame: AtomicU32::new(0),
                version: self.versions.fetch_add(1, Ordering::Relaxed) + 1,
            });
        }
    }

    /// Frees an image; its id may be reused.
    pub fn remove(&self, id: ImageId) {
        if let Some(s) = self.slots.write().unwrap().get_mut(id.0 as usize) {
            self.shrank(s.as_ref().map_or(0, |o| o.image.bytes()));
            *s = None;
        }
    }

    pub fn get(&self, id: ImageId) -> Option<Arc<DecodedImage>> {
        let slots = self.slots.read().unwrap();
        slots.get(id.0 as usize)?.as_ref().map(|s| s.image.clone())
    }

    /// The image and the frame currently shown.
    pub fn current(&self, id: ImageId) -> Option<(Arc<DecodedImage>, usize)> {
        let slots = self.slots.read().unwrap();
        let s = slots.get(id.0 as usize)?.as_ref()?;
        let frame = s.frame.load(Ordering::Relaxed) as usize;
        Some((s.image.clone(), frame.min(s.image.frames.len() - 1)))
    }

    /// Which pixels the store holds for `id` now: changes when the image is replaced (a canvas that
    /// drew) or shows another frame, and differs between images.
    pub fn version(&self, id: ImageId) -> u64 {
        let slots = self.slots.read().unwrap();
        match slots.get(id.0 as usize).and_then(Option::as_ref) {
            Some(s) => s.version ^ (u64::from(s.frame.load(Ordering::Relaxed)) << 40),
            None => 0,
        }
    }

    /// The frame `id` shows now, in one look at the store (its frame and version belong together).
    pub fn shown(&self, id: ImageId) -> Option<ShownImage> {
        let slots = self.slots.read().unwrap();
        let s = slots.get(id.0 as usize)?.as_ref()?;
        let stored = s.frame.load(Ordering::Relaxed);
        let frame = (stored as usize).min(s.image.frames.len() - 1);
        let f = &s.image.frames[frame];
        Some(ShownImage {
            frame,
            version: s.version ^ (u64::from(stored) << 40),
            width: f.width,
            height: f.height,
            image: Some(s.image.clone()),
        })
    }

    /// Shows another frame of an animation whose frames the store holds.
    pub fn set_frame(&self, id: ImageId, frame: usize) {
        if let Some(Some(s)) = self.slots.read().unwrap().get(id.0 as usize) {
            s.frame.store(frame as u32, Ordering::Relaxed);
        }
    }

    /// Shows a frame of an animation whose frames are decoded as they show
    /// ([`Animation::encoded`]): it replaces the one the store holds (painters upload it again).
    pub fn show_frame(&self, id: ImageId, frame: Frame) {
        if let Some(Some(s)) = self.slots.write().unwrap().get_mut(id.0 as usize) {
            s.image = Arc::new(DecodedImage {
                frames: vec![frame],
                orientation: s.image.orientation,
                animation: s.image.animation.clone(),
            });
            s.frame = AtomicU32::new(0);
            s.version = self.versions.fetch_add(1, Ordering::Relaxed) + 1;
        }
    }
}

/// The pixel at (x, y) of premultiplied BGRA rows `width * 4` bytes apart, un-premultiplied.
pub fn pixel_at(data: &[u8], width: u32, x: u32, y: u32) -> Color {
    let i = ((y * width + x) * 4) as usize;
    let (b, g, r, a) = (data[i], data[i + 1], data[i + 2], data[i + 3]);
    if a == 0 {
        return Color::TRANSPARENT;
    }
    let un = |v: u8| ((v as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
    Color::rgba(un(r), un(g), un(b), a)
}

/// A 32-bit BMP file of straight-alpha RGBA pixels (for tests and the demo: a format every Windows
/// has a WIC decoder for, built without an encoder).
pub fn bmp_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let image = width * height * 4;
    let mut out = Vec::with_capacity(122 + image as usize);
    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(122 + image).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&122u32.to_le_bytes());
    // BITMAPV4HEADER: 32 bpp BI_BITFIELDS with an alpha mask, top-down rows
    out.extend_from_slice(&108u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(-(height as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
    out.extend_from_slice(&image.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    for mask in [0x00ff_0000u32, 0x0000_ff00, 0x0000_00ff, 0xff00_0000] {
        out.extend_from_slice(&mask.to_le_bytes());
    }
    out.extend_from_slice(b"BGRs"); // LCS_WINDOWS_COLOR_SPACE
    out.extend_from_slice(&[0; 48]); // endpoints and gamma
    for px in rgba.as_chunks::<4>().0 {
        out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(w: u32, h: u32) -> Frame {
        let mut pixels = Vec::new();
        for i in 0..w * h {
            pixels.extend_from_slice(&[i as u8, 0, 0, 255]);
        }
        Frame {
            width: w,
            height: h,
            pixels,
            delay_ms: 0,
        }
    }

    #[test]
    fn exif_orientations_turn_and_mirror() {
        // 3 x 2: 0 1 2 / 3 4 5
        let f = numbered(3, 2);
        let first = |f: &Frame| -> Vec<u8> { f.pixels.chunks(4).map(|p| p[0]).collect() };
        let expect: [(u8, (u32, u32), [u8; 6]); 8] = [
            (1, (3, 2), [0, 1, 2, 3, 4, 5]),
            (2, (3, 2), [2, 1, 0, 5, 4, 3]),
            (3, (3, 2), [5, 4, 3, 2, 1, 0]),
            (4, (3, 2), [3, 4, 5, 0, 1, 2]),
            (5, (2, 3), [0, 3, 1, 4, 2, 5]),
            (6, (2, 3), [3, 0, 4, 1, 5, 2]),
            (7, (2, 3), [5, 2, 4, 1, 3, 0]),
            (8, (2, 3), [2, 5, 1, 4, 0, 3]),
        ];
        for (o, size, pixels) in expect {
            let shown = orient(&f, o);
            assert_eq!((shown.width, shown.height), size, "orientation {o}");
            assert_eq!(first(&shown), pixels, "orientation {o}");
            assert_eq!(
                first(&unorient(&shown, o)),
                first(&f),
                "orientation {o} back"
            );
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"definitely not an image").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn decodes_animated_gif_frames() {
        // A 2x1 GIF with two frames: the second paints only the right pixel and keeps the first.
        let gif: &[u8] = &[
            b'G', b'I', b'F', b'8', b'9', b'a', 2, 0, 1, 0, 0x80, 0,
            0, // screen, 2-color table
            255, 0, 0, 0, 0, 255, // red, blue
            0x21, 0xFF, 11, b'N', b'E', b'T', b'S', b'C', b'A', b'P', b'E', b'2', b'.', b'0', 3, 1,
            0, 0, 0, // loop forever
            0x21, 0xF9, 4, 0x04, 10, 0, 0, 0, // GCE: disposal 1 (keep), 100 ms
            0x2C, 0, 0, 0, 0, 2, 0, 1, 0, 0, // frame 1: 2x1 at 0,0
            2, 2, 0x04, 0x0A, 0, // LZW: red, red
            0x21, 0xF9, 4, 0x04, 5, 0, 0, 0, // GCE: keep, 50 ms
            0x2C, 1, 0, 0, 0, 1, 0, 1, 0, 0, // frame 2: 1x1 at 1,0
            2, 2, 0x4C, 0x01, 0, // LZW: blue
            0x3B,
        ];
        let img = decode(gif).unwrap();
        assert_eq!(img.frames.len(), 2);
        assert_eq!(img.frames[0].pixel(1, 0), Color::rgb(255, 0, 0));
        assert_eq!(img.frames[1].pixel(0, 0), Color::rgb(255, 0, 0));
        assert_eq!(img.frames[1].pixel(1, 0), Color::rgb(0, 0, 255));
        assert_eq!(img.frames[0].delay_ms, 100);
        assert_eq!(img.frames[1].delay_ms, 50);
    }

    /// A fixture made by `snowlark-render/tests/fixtures/README.md`'s script.
    fn fixture(name: &str) -> Vec<u8> {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../snowlark-render/tests/fixtures/"
        );
        std::fs::read(format!("{dir}{name}")).unwrap()
    }

    const RGB: [Color; 3] = [
        Color::rgb(255, 0, 0),
        Color::rgb(0, 255, 0),
        Color::rgb(0, 0, 255),
    ];

    #[test]
    fn decodes_animated_gif_apng_and_webp() {
        // (red 100 ms, lime 200 ms, blue 300 ms)
        for (name, plays) in [
            ("anim-rgb.gif", None),
            ("anim-rgb.png", Some(2)),
            ("anim-rgb.webp", Some(1)),
        ] {
            let img = decode(&fixture(name)).unwrap();
            let a = img.animation.as_ref().expect(name);
            assert_eq!(img.frames.len(), 3, "{name}");
            assert_eq!(a.timing.frames(), 3, "{name}");
            assert_eq!(a.timing.plays(), plays, "{name}");
            assert!(a.encoded.is_none(), "{name}");
            for (i, f) in img.frames.iter().enumerate() {
                assert_eq!((f.width, f.height), (16, 16), "{name}");
                assert_eq!(f.pixel(8, 8), RGB[i], "{name} frame {i}");
                assert_eq!(f.delay_ms, [100, 200, 300][i], "{name} frame {i}");
                assert_eq!(a.timing.delay(i), [100, 200, 300][i], "{name}");
            }
            assert_eq!(img.default_frame().pixel(0, 0), RGB[0], "{name}");
        }
    }

    #[test]
    fn an_apng_default_image_outside_the_animation_is_kept_apart() {
        let img = decode(&fixture("anim-default.png")).unwrap();
        assert_eq!(img.frames.len(), 3);
        for (i, f) in img.frames.iter().enumerate() {
            assert_eq!(f.pixel(3, 3), RGB[i]);
        }
        // (what canvas drawImage draws)
        assert_eq!(img.default_frame().pixel(3, 3), Color::rgb(255, 255, 0));
    }

    /// The colour at (x, y), each channel within `tolerance` of `want`.
    fn assert_near(f: &Frame, (x, y): (u32, u32), want: [u8; 3], tolerance: u8, what: &str) {
        let c = f.pixel(x, y);
        let got = [c.r, c.g, c.b];
        let near = got
            .iter()
            .zip(want)
            .all(|(g, w)| g.abs_diff(w) <= tolerance);
        assert!(near, "{what} at ({x}, {y}): {got:?}, want {want:?}");
        assert_eq!(c.a, 255, "{what}");
    }

    #[test]
    fn icc_profiles_are_converted_to_srgb() {
        // A Display P3 profile on PNG (iCCP), JPEG (APP2) and WebP (ICCP): the sRGB values are
        // what other engines paint for these files.
        let want = [[255, 0, 0], [0, 255, 0], [215, 93, 97], [128, 128, 128]];
        for (name, tolerance) in [
            ("p3-patches.png", 1),
            ("p3-patches.jpg", 2),
            ("p3-patches.webp", 1),
        ] {
            let img = decode(&fixture(name)).unwrap();
            assert_eq!((img.width(), img.height()), (32, 8), "{name}");
            for (i, w) in want.iter().enumerate() {
                assert_near(&img.frames[0], (i as u32 * 8 + 4, 4), *w, tolerance, name);
            }
        }
    }

    #[test]
    fn cmyk_jpeg_is_read_as_inverted_cmyk() {
        // Cyan (C 255) and 50% black (K 128), as Adobe stores them: other engines paint (0, 255, 255)
        // and (127, 127, 127).
        let img = decode(&fixture("cmyk.jpg")).unwrap();
        assert_eq!((img.width(), img.height()), (16, 8));
        assert_near(&img.frames[0], (4, 4), [0, 255, 255], 2, "cyan");
        assert_near(&img.frames[0], (12, 4), [127, 127, 127], 2, "gray");
    }

    #[test]
    fn png_gamma_without_chromaticities_is_applied() {
        // gAMA 1.0: 128 is half the light, which sRGB stores as 188.
        let img = decode(&fixture("gamma-1.png")).unwrap();
        assert_near(&img.frames[0], (4, 4), [188, 188, 188], 1, "gamma-1.png");
    }

    #[test]
    fn progressive_jpeg_with_exif_orientation() {
        // Stored 16 x 8, red | blue, orientation 6: shown 8 x 16, red above blue.
        let bytes = fixture("orient-6.jpg");
        assert_eq!(exif_orientation(&bytes), Some(6));
        let img = decode(&bytes).unwrap();
        assert_eq!(img.orientation, 6);
        assert_eq!((img.width(), img.height()), (8, 16));
        assert_near(&img.frames[0], (4, 3), [255, 0, 0], 8, "top");
        assert_near(&img.frames[0], (4, 12), [0, 0, 255], 8, "bottom");
        let raw = img.unoriented().unwrap();
        assert_eq!((raw.width(), raw.height()), (16, 8));
        assert_near(&raw.frames[0], (3, 4), [255, 0, 0], 8, "stored left");
    }

    // The AVIF tests need the dav1d library (`AVIF`); without it AVIF goes to WIC, which reads
    // neither the colour profile, alpha, odd sizes nor `irot`/`imir` as other engines do. The
    // expected values are the ones headless Chrome paints for these files.

    #[test]
    fn avif_colour_profiles_are_converted_to_srgb() {
        // The P3 patches, tagged by an ICC profile and by an nclx colour description.
        let want = [[255, 0, 0], [0, 255, 0], [215, 93, 97], [128, 128, 128]];
        for name in ["p3-patches.avif", "p3-nclx.avif"] {
            let img = decode(&fixture(name)).unwrap();
            assert_eq!((img.width(), img.height()), (32, 8), "{name}");
            assert!(img.animation.is_none(), "{name}");
            for (i, w) in want.iter().enumerate() {
                assert_near(&img.frames[0], (i as u32 * 8 + 4, 4), *w, 1, name);
            }
        }
    }

    #[test]
    fn avif_alpha_is_premultiplied() {
        let img = decode(&fixture("alpha.avif")).unwrap();
        let f = &img.frames[0];
        assert_eq!((f.width, f.height), (16, 8));
        // red at alpha 128: premultiplied BGRA (0, 0, 128, 128)
        let at = |x: usize, y: usize| &f.pixels[(y * 16 + x) * 4..][..4];
        let red = at(3, 4);
        assert!(
            red[0] <= 1 && red[1] <= 1 && red[2].abs_diff(128) <= 1,
            "{red:?}"
        );
        assert!(red[3].abs_diff(128) <= 1, "{red:?}");
        assert_near(f, (9, 4), [0, 255, 0], 2, "lime");
        assert_eq!(at(14, 4), &[0, 0, 0, 0], "transparent");
    }

    #[test]
    fn avif_odd_sizes_and_chroma_subsampling() {
        let img = decode(&fixture("odd-7x5.avif")).unwrap();
        let f = &img.frames[0];
        assert_eq!((f.width, f.height), (7, 5));
        assert_near(f, (0, 0), [0, 0, 253], 2, "blue");
        assert_near(f, (3, 2), [0, 1, 253], 2, "blue");
        // the last row is yellow blended with the blue chroma above it, as Chrome upsamples it
        assert_near(f, (2, 4), [240, 241, 113], 2, "bottom row");
    }

    #[test]
    fn avif_rotation_and_mirroring_turn_the_image() {
        // Stored 16 x 8, red | blue, with 'irot' and 'imir' making orientation 6: shown 8 x 16,
        // red above blue.
        let img = decode(&fixture("orient-6.avif")).unwrap();
        assert_eq!(img.orientation, 6);
        assert_eq!((img.width(), img.height()), (8, 16));
        assert_near(&img.frames[0], (4, 3), [255, 0, 0], 2, "top");
        assert_near(&img.frames[0], (4, 12), [0, 0, 254], 2, "bottom");
        let raw = img.unoriented().unwrap();
        assert_eq!((raw.width(), raw.height()), (16, 8));
    }

    #[test]
    fn avif_image_sequences_animate() {
        let img = decode(&fixture("anim-rgb.avif")).unwrap();
        let a = img.animation.as_ref().expect("an animation");
        assert_eq!(img.frames.len(), 3);
        assert!(a.encoded.is_none());
        for (i, f) in img.frames.iter().enumerate() {
            assert_eq!((f.width, f.height), (16, 16));
            let want = [[255, 0, 0], [0, 255, 0], [0, 0, 255]][i];
            assert_near(f, (8, 8), want, 2, &format!("frame {i}"));
            assert_eq!(f.delay_ms, [100, 200, 300][i], "frame {i}");
        }
        // (no edit list: it loops forever)
        assert_eq!(a.timing.plays(), None);
        // decoded again as it shows, starting over after the last frame
        let mut d = FrameDecoder::new(Arc::from(fixture("anim-rgb.avif"))).unwrap();
        assert_near(&d.frame(2).unwrap(), (8, 8), [0, 0, 255], 2, "frame 2");
        assert_near(
            &d.frame(0).unwrap(),
            (8, 8),
            [255, 0, 0],
            2,
            "frame 0 again",
        );
        assert_near(&d.frame(1).unwrap(), (8, 8), [0, 255, 0], 2, "frame 1");
    }

    #[test]
    fn high_bit_depth_avif_sequences_decode() {
        // 12 bits per channel, five frames of a second, key frames 0, 2 and 3
        let img = decode(&fixture("anim-12bit.avif")).unwrap();
        let a = img.animation.as_ref().expect("an animation");
        assert_eq!(img.frames.len(), 5);
        assert_eq!((img.width(), img.height()), (64, 64));
        assert!(img.frames.iter().all(|f| f.delay_ms == 1000));
        assert_eq!(a.timing.plays(), None);
        assert_near(&img.frames[0], (32, 32), [255, 120, 255], 1, "frame 0");
        // frame 4 follows frame 3 (a key frame): the same picture as when decoded in order
        let mut d = FrameDecoder::new(Arc::from(fixture("anim-12bit.avif"))).unwrap();
        let f4 = d.frame(4).unwrap();
        assert_eq!(f4.pixels, img.frames[4].pixels);
    }

    #[test]
    fn broken_avif_files_are_refused() {
        let avif = fixture("p3-patches.avif");
        assert!(decode(&avif[..avif.len() / 2]).is_err());
        assert!(decode(&avif[..avif.len() - 8]).is_err());
        // an 'ftyp' with the AVIF brand and nothing after it
        assert!(decode(&avif[..32]).is_err());
    }

    /// A PNG of `width` x 1 pixels of the given colour type and depth, with `extra` chunks
    /// (`PLTE`, `tRNS`) before its data.
    fn png_of(
        color: png::ColorType,
        depth: png::BitDepth,
        width: u32,
        data: &[u8],
        palette: Option<&[u8]>,
        trns: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut e = png::Encoder::new(&mut out, width, 1);
            e.set_color(color);
            e.set_depth(depth);
            if let Some(p) = palette {
                e.set_palette(p.to_vec());
            }
            if let Some(t) = trns {
                e.set_trns(t.to_vec());
            }
            let mut w = e.write_header().unwrap();
            w.write_image_data(data).unwrap();
        }
        out
    }

    #[test]
    fn png_color_types_decode_to_premultiplied_bgra() {
        use png::{BitDepth, ColorType};
        let px = |img: &DecodedImage, x: u32| {
            let c = img.frames[0].pixel(x, 0);
            [c.r, c.g, c.b, c.a]
        };
        // gray 8, gray + alpha, RGB 16 (high bytes kept), palette with tRNS, gray 1-bit
        let gray = decode(&png_of(
            ColorType::Grayscale,
            BitDepth::Eight,
            2,
            &[0, 200],
            None,
            None,
        ))
        .unwrap();
        assert_eq!(
            [px(&gray, 0), px(&gray, 1)],
            [[0, 0, 0, 255], [200, 200, 200, 255]]
        );
        let ga = decode(&png_of(
            ColorType::GrayscaleAlpha,
            BitDepth::Eight,
            1,
            &[200, 128],
            None,
            None,
        ))
        .unwrap();
        // (un-premultiplied again: 100 * 255 / 128 = 199)
        assert_eq!(px(&ga, 0), [199, 199, 199, 128]);
        // (stored premultiplied: 200 * 128 / 255 = 100)
        assert_eq!(&ga.frames[0].pixels, &[100, 100, 100, 128]);
        let rgb16 = decode(&png_of(
            ColorType::Rgb,
            BitDepth::Sixteen,
            1,
            &[0xff, 0x00, 0x80, 0x10, 0x00, 0xff],
            None,
            None,
        ))
        .unwrap();
        assert_eq!(px(&rgb16, 0), [255, 128, 0, 255]);
        let indexed = decode(&png_of(
            ColorType::Indexed,
            BitDepth::Eight,
            2,
            &[0, 1],
            Some(&[255, 0, 0, 0, 0, 255]),
            Some(&[0]),
        ))
        .unwrap();
        assert_eq!(
            [px(&indexed, 0), px(&indexed, 1)],
            [[0, 0, 0, 0], [0, 0, 255, 255]]
        );
        let bits = decode(&png_of(
            ColorType::Grayscale,
            BitDepth::One,
            2,
            &[0b0100_0000],
            None,
            None,
        ))
        .unwrap();
        assert_eq!(
            [px(&bits, 0), px(&bits, 1)],
            [[0, 0, 0, 255], [255, 255, 255, 255]]
        );
    }

    #[test]
    fn png_chunks_with_wrong_crcs_are_read() {
        // A 1 x 1 PNG whose IDAT CRC is wrong (a web-platform test's, which expects it to load,
        // as it does in other engines).
        let png: [u8; 70] = [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xda, 0x63, 0xfc, 0xcf, 0xc0, 0x50, 0x0f, 0x00, 0x03, 0x85, 0x01, 0x80, 0x5a, 0x34,
            0x7d, 0x6b, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let img = decode(&png).unwrap();
        assert_eq!((img.width(), img.height()), (1, 1));
    }

    #[test]
    fn truncated_and_oversized_files_are_refused() {
        let png = fixture("p3-patches.png");
        assert!(decode(&png[..png.len() / 2]).is_err());
        let jpg = fixture("p3-patches.jpg");
        assert!(decode(&jpg[..20]).is_err());
        let webp = fixture("p3-patches.webp");
        assert!(decode(&webp[..30]).is_err());
        assert!(decode(b"GIF89a").is_err());
        // a PNG header claiming 100 000 x 100 000 pixels
        let mut huge = png_of(
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            1,
            &[0],
            None,
            None,
        );
        huge[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        let crc = crc32fast::hash(&huge[12..29]);
        huge[29..33].copy_from_slice(&crc.to_be_bytes());
        assert!(decode(&huge).is_err());
    }

    #[test]
    fn gif_frames_composite_their_rectangles() {
        // A red square moving right over white; frames 2 and 3 are 20 px wide pieces with
        // transparent pixels where nothing changed.
        let img = decode(&fixture("anim-move.gif")).unwrap();
        assert_eq!(img.frames.len(), 3);
        for (i, f) in img.frames.iter().enumerate() {
            assert_eq!((f.width, f.height), (30, 10));
            for square in 0..3 {
                let want = if square == i {
                    Color::rgb(255, 0, 0)
                } else {
                    Color::rgb(255, 255, 255)
                };
                assert_eq!(f.pixel(square as u32 * 10 + 5, 5), want, "frame {i}");
            }
        }
    }

    #[test]
    fn big_animations_decode_frames_as_they_show() {
        for name in [
            "anim-rgb.gif",
            "anim-rgb.png",
            "anim-rgb.webp",
            "anim-move.gif",
        ] {
            let bytes = fixture(name);
            let full = decode(&bytes).unwrap();
            // (a budget smaller than the frames: only the first is kept, with the file)
            let img = decode_with_budget(&bytes, 1000).unwrap();
            assert_eq!(img.frames.len(), 1, "{name}");
            assert_eq!(img.frames[0].pixels, full.frames[0].pixels, "{name}");
            let a = img.animation.as_ref().unwrap();
            assert_eq!(a.timing, full.animation.as_ref().unwrap().timing, "{name}");
            assert_eq!(
                a.default_image.as_ref().unwrap().pixels,
                full.frames[0].pixels
            );
            let mut d = FrameDecoder::new(a.encoded.clone().unwrap()).unwrap();
            // in order, then backwards (starting over), then skipping
            for i in [0, 1, 2, 1, 0, 2, 2] {
                let f = d.frame(i).unwrap();
                assert_eq!(f.pixels, full.frames[i].pixels, "{name} frame {i}");
                assert_eq!(f.delay_ms, full.frames[i].delay_ms);
            }
            assert!(d.frame(3).is_err());
            // the store shows the decoded frame in place of the one it held
            let store = ImageStore::new();
            let id = store.add(img);
            let v = store.version(id);
            store.show_frame(id, d.frame(1).unwrap());
            assert_ne!(store.version(id), v);
            let (now, frame) = store.current(id).unwrap();
            assert_eq!(frame, 0);
            assert_eq!(now.frames[0].pixels, full.frames[1].pixels);
            assert!(now.animation.is_some());
        }
    }

    #[test]
    fn store_reuses_slots_and_tracks_frames() {
        let store = ImageStore::new();
        let img = DecodedImage::still(numbered(2, 2));
        let a = store.add(img.clone());
        let b = store.add(img.clone());
        assert_ne!(a, b);
        store.remove(a);
        assert!(store.get(a).is_none());
        let c = store.add(img);
        assert_eq!(c, a);
        store.set_frame(b, 5);
        // (clamped to the frames there are)
        assert_eq!(store.current(b).unwrap().1, 0);
    }
}
