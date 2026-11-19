// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Images through WIC: decoding (PNG, JPEG, BMP, ICO, WebP and others where Windows has the codec;
//! animated GIF, APNG and WebP with their frames composited, see [`crate::animation`]) to premultiplied BGRA pixels, the store display lists refer to images
//! by, and PNG encoding of rendered pixmaps.
//!
//! Decoded pixels are plain Rust memory (`Send + Sync`); each rasterizer uploads them to its own
//! Direct2D bitmaps. A still image's EXIF orientation is applied to its pixels when it is decoded
//! (CSS Images 3 §5.3 `image-orientation: from-image`, the initial value; [`DecodedImage::unoriented`]
//! gives the pixels as stored, for `none`); density (`srcset` `x`/`w` descriptors) is the caller's.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use windows::core::{Result, GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::{E_INVALIDARG, E_OUTOFMEMORY, GENERIC_WRITE};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VT_UI1, VT_UI2};

use crate::animation::{self, Blend, Canvas, Dispose, Region, Timing};
use crate::com;
use crate::display_list::ImageId;
use crate::geom::Color;

/// Images larger than this many pixels (per frame) are refused: a hostile file can't make the
/// process allocate gigabytes.
pub const MAX_PIXELS: u64 = 100_000_000;
/// The most memory an animation's decoded frames are kept in: an animation whose frames take more
/// keeps its file instead and decodes each frame as it shows ([`Animation::encoded`],
/// [`FrameDecoder`]). 32 MiB holds 33 frames of 500 × 500, or 8 of 1000 × 1000.
pub const ANIMATION_FRAMES_BYTES: u64 = 32 << 20;

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

fn factory() -> Result<IWICImagingFactory> {
    com::ensure();
    // SAFETY: plain COM creation on a COM-initialized thread.
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER) }
}

/// Decodes an encoded image held in memory. An animated GIF, APNG or WebP decodes to all its frames
/// composited (or, past [`ANIMATION_FRAMES_BYTES`], to its first frame and its file).
pub fn decode(bytes: &[u8]) -> Result<DecodedImage> {
    decode_with_budget(bytes, ANIMATION_FRAMES_BYTES)
}

/// [`decode`] keeping an animation's frames when they take at most `budget` bytes.
fn decode_with_budget(bytes: &[u8], budget: u64) -> Result<DecodedImage> {
    let wic = factory()?;
    // SAFETY: the stream reads `bytes`, which outlive every use of the stream and decoder below (all
    // pixels are copied out before returning). Metadata PROPVARIANTs are cleared after reading.
    unsafe {
        let stream = wic.CreateStream()?;
        stream.InitializeFromMemory(bytes)?;
        let decoder =
            wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let container = decoder.GetContainerFormat()?;
        let count = decoder.GetFrameCount()?;
        if count == 0 {
            return Err(E_INVALIDARG.into());
        }
        let first = decoder.GetFrame(0)?;
        let orientation = first
            .GetMetadataQueryReader()
            .ok()
            .and_then(|r| read_u16(&r, "System.Photo.Orientation"))
            .filter(|o| (1..=8).contains(o))
            .unwrap_or(1) as u8;
        let kind = if container == GUID_ContainerFormatGif && count > 1 {
            Some(Kind::Gif)
        } else if container == GUID_ContainerFormatPng {
            animation::parse_apng(bytes)
                .filter(|a| a.frames.len() > 1)
                .map(Kind::Apng)
        } else if container == GUID_ContainerFormatWebp && count > 1 {
            animation::parse_webp_animation(bytes).map(Kind::Webp)
        } else {
            None
        };
        if let Some(kind) = kind {
            // (the still image when the animation doesn't decode)
            if let Ok(mut image) = decode_animation(&wic, Arc::from(bytes), kind, budget) {
                image.orientation = orientation;
                return Ok(image);
            }
        }
        let frame = if container == GUID_ContainerFormatIco && count > 1 {
            // An icon holds several sizes: the largest.
            let mut best = (0u64, 0u32);
            for i in 0..count {
                let (w, h) = size_of(&decoder.GetFrame(i)?)?;
                if w as u64 * h as u64 > best.0 {
                    best = (w as u64 * h as u64, i);
                }
            }
            to_frame(&wic, &decoder.GetFrame(best.1)?, 0)?
        } else {
            to_frame(&wic, &first, 0)?
        };
        // (WIC reads JPEG and TIFF orientation; a PNG's `eXIf` chunk is read here)
        let orientation = if orientation == 1 && container == GUID_ContainerFormatPng {
            png_exif_orientation(bytes).unwrap_or(1)
        } else {
            orientation
        };
        let mut image = DecodedImage::still(orient(&frame, orientation));
        image.orientation = orientation;
        Ok(image)
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

/// An animation's frames: all of them when they fit `budget`, else the first.
fn decode_animation(
    wic: &IWICImagingFactory,
    bytes: Arc<[u8]>,
    kind: Kind,
    budget: u64,
) -> Result<DecodedImage> {
    let mut decoder = FrameDecoder::with_kind(wic.clone(), bytes.clone(), kind)?;
    let frame_bytes = decoder.width as u64 * decoder.height as u64 * 4;
    let mut timing = decoder.timing.clone();
    let streamed = frame_bytes * timing.frames() as u64 > budget;
    let mut frames = Vec::new();
    let wanted = if streamed { 1 } else { timing.frames() };
    for i in 0..wanted {
        match decoder.frame(i) {
            Ok(f) => frames.push(f),
            // (frames that don't decode end the animation; without a first one there is none)
            Err(e) if i == 0 => return Err(e),
            Err(_) => break,
        }
    }
    if frames.len() < 2 && !streamed {
        return Err(E_INVALIDARG.into());
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

/// The animated formats and what their frames need beyond WIC.
enum Kind {
    /// WIC decodes each frame's own rectangle; composition and the loop count are ours.
    Gif,
    /// WIC does not decode APNG frames: each is made a PNG of its own ([`animation::Apng`]).
    Apng(animation::Apng),
    /// WIC composites WebP frames; durations and the loop count are ours.
    Webp(animation::WebpAnimation),
}

/// Decodes an animation's frames one after another (composited to full size, as they show): what
/// keeps an animation too big to hold every frame playing. Frames come in order; asking for an
/// earlier one starts again from the first. Lives on one thread (it holds WIC objects).
pub struct FrameDecoder {
    // (the WIC objects go before the bytes they read)
    decoder: Option<IWICBitmapDecoder>,
    _stream: Option<IWICStream>,
    wic: IWICImagingFactory,
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
        let wic = factory()?;
        let kind = if bytes.starts_with(b"GIF") {
            Kind::Gif
        } else if let Some(a) = animation::parse_apng(&bytes) {
            Kind::Apng(a)
        } else if let Some(w) = animation::parse_webp_animation(&bytes) {
            Kind::Webp(w)
        } else {
            return Err(E_INVALIDARG.into());
        };
        FrameDecoder::with_kind(wic, bytes, kind)
    }

    fn with_kind(wic: IWICImagingFactory, bytes: Arc<[u8]>, kind: Kind) -> Result<FrameDecoder> {
        // SAFETY: the stream reads `bytes`, which this decoder keeps alive (and unmoved: an `Arc`'s
        // data never moves) for as long as it keeps the stream and decoder, which drop first.
        // Metadata PROPVARIANTs are cleared after reading; out pointers are locals.
        unsafe {
            let (decoder, stream) = match kind {
                Kind::Apng(_) => (None, None),
                _ => {
                    let stream = wic.CreateStream()?;
                    stream.InitializeFromMemory(&bytes)?;
                    let decoder = wic.CreateDecoderFromStream(
                        &stream,
                        std::ptr::null(),
                        WICDecodeMetadataCacheOnDemand,
                    )?;
                    (Some(decoder), Some(stream))
                }
            };
            let (width, height, timing) = match &kind {
                Kind::Gif => {
                    let d = decoder.as_ref().ok_or(E_INVALIDARG)?;
                    let screen = d.GetMetadataQueryReader()?;
                    let first = d.GetFrame(0)?;
                    let (fw, fh) = size_of(&first)?;
                    let w = read_u16(&screen, "/logscrdesc/Width").map_or(fw, |v| v as u32);
                    let h = read_u16(&screen, "/logscrdesc/Height").map_or(fh, |v| v as u32);
                    let mut delays = Vec::new();
                    for i in 0..d.GetFrameCount()? {
                        let meta = d.GetFrame(i)?.GetMetadataQueryReader()?;
                        delays.push(read_u16(&meta, "/grctlext/Delay").unwrap_or(0) as u32 * 10);
                    }
                    let timing = Timing::new(delays, animation::gif_plays(&bytes));
                    (w.max(1), h.max(1), timing)
                }
                Kind::Apng(a) => (a.width, a.height, Timing::new(a.delays(), a.plays)),
                Kind::Webp(anim) => {
                    let d = decoder.as_ref().ok_or(E_INVALIDARG)?;
                    let (w, h) = size_of(&d.GetFrame(0)?)?;
                    let count = d.GetFrameCount()? as usize;
                    // (WIC's frames are the ones that count)
                    let delays = (0..count).map(|i| anim.durations.get(i).copied().unwrap_or(0));
                    (w, h, Timing::new(delays, anim.plays))
                }
            };
            if width == 0 || height == 0 || width as u64 * height as u64 > MAX_PIXELS {
                return Err(E_OUTOFMEMORY.into());
            }
            Ok(FrameDecoder {
                decoder,
                _stream: stream,
                wic,
                kind,
                bytes,
                width,
                height,
                timing,
                canvas: Canvas::new(0, 0),
                next: 0,
            })
        }
    }

    /// The animation's frame timeline.
    pub fn timing(&self) -> &Timing {
        &self.timing
    }

    /// The default image when it is not the first frame (an APNG whose `IDAT` image is not part of
    /// the animation).
    fn default_image(&self) -> Result<Option<Frame>> {
        match &self.kind {
            Kind::Apng(a) if !a.default_is_first => {
                let wic = &self.wic;
                // SAFETY: as in `decode`: the stream reads `self.bytes`, alive for the call.
                unsafe {
                    let stream = wic.CreateStream()?;
                    stream.InitializeFromMemory(&self.bytes)?;
                    let d = wic.CreateDecoderFromStream(
                        &stream,
                        std::ptr::null(),
                        WICDecodeMetadataCacheOnDemand,
                    )?;
                    Ok(Some(to_frame(wic, &d.GetFrame(0)?, 0)?))
                }
            }
            _ => Ok(None),
        }
    }

    /// Frame `index` composited to full size (with its duration).
    pub fn frame(&mut self, index: usize) -> Result<Frame> {
        if index >= self.timing.frames() {
            return Err(E_INVALIDARG.into());
        }
        if index < self.next || self.canvas.width == 0 {
            self.next = 0;
            if self.canvas.width == 0 {
                self.canvas = Canvas::new(self.width, self.height);
            } else {
                self.canvas.reset();
            }
        }
        while self.next < index {
            self.draw_next()?;
        }
        self.draw_next()?;
        Ok(Frame {
            width: self.width,
            height: self.height,
            pixels: self.canvas.pixels.clone(),
            delay_ms: self.timing.delay(index),
        })
    }

    /// Draws frame `self.next` into the canvas.
    fn draw_next(&mut self) -> Result<()> {
        let i = self.next;
        // SAFETY: WIC calls on this thread's objects, which read `self.bytes` (alive) or a local
        // PNG that outlives its stream and decoder; out pointers are locals; metadata PROPVARIANTs
        // are cleared after reading.
        unsafe {
            match &self.kind {
                Kind::Gif => {
                    let d = self.decoder.as_ref().ok_or(E_INVALIDARG)?;
                    let source = d.GetFrame(i as u32)?;
                    let meta = source.GetMetadataQueryReader()?;
                    let left = read_u16(&meta, "/imgdesc/Left").unwrap_or(0) as u32;
                    let top = read_u16(&meta, "/imgdesc/Top").unwrap_or(0) as u32;
                    let dispose = match read_u16(&meta, "/grctlext/Disposal").unwrap_or(0) {
                        2 => Dispose::Background,
                        3 => Dispose::Previous,
                        _ => Dispose::None,
                    };
                    let piece = to_frame(&self.wic, &source, 0)?;
                    let at = Region {
                        x: left,
                        y: top,
                        width: piece.width,
                        height: piece.height,
                    };
                    // (source-over: GIF pixels are fully opaque or fully transparent)
                    self.canvas
                        .draw(&piece.pixels, piece.width, at, Blend::Over, dispose);
                }
                Kind::Apng(a) => {
                    let f = &a.frames[i];
                    let png = a.frame_png(&self.bytes, i);
                    let stream = self.wic.CreateStream()?;
                    stream.InitializeFromMemory(&png)?;
                    let d = self.wic.CreateDecoderFromStream(
                        &stream,
                        std::ptr::null(),
                        WICDecodeMetadataCacheOnDemand,
                    )?;
                    let piece = to_frame(&self.wic, &d.GetFrame(0)?, 0)?;
                    drop(d);
                    drop(stream);
                    let at = Region {
                        x: f.x,
                        y: f.y,
                        width: piece.width.min(f.width),
                        height: piece.height.min(f.height),
                    };
                    self.canvas
                        .draw(&piece.pixels, piece.width, at, f.blend, f.dispose);
                }
                Kind::Webp(_) => {
                    let d = self.decoder.as_ref().ok_or(E_INVALIDARG)?;
                    let f = to_frame(&self.wic, &d.GetFrame(i as u32)?, 0)?;
                    if (f.width, f.height) != (self.width, self.height) {
                        return Err(E_INVALIDARG.into());
                    }
                    self.canvas.pixels = f.pixels;
                }
            }
        }
        self.next = i + 1;
        Ok(())
    }
}

/// Decodes an image file.
pub fn decode_file(path: &Path) -> Result<DecodedImage> {
    let bytes = std::fs::read(path).map_err(|_| windows::core::Error::from(E_INVALIDARG))?;
    decode(&bytes)
}

// SAFETY contract: called on a COM-initialized thread with live WIC objects; out pointers are locals.
unsafe fn size_of(source: &IWICBitmapFrameDecode) -> Result<(u32, u32)> {
    let (mut w, mut h) = (0, 0);
    source.GetSize(&mut w, &mut h)?;
    Ok((w, h))
}

/// A frame converted to premultiplied BGRA.
// SAFETY contract: called on a COM-initialized thread with live WIC objects; out pointers are locals.
unsafe fn to_frame(
    wic: &IWICImagingFactory,
    source: &IWICBitmapFrameDecode,
    delay_ms: u32,
) -> Result<Frame> {
    let (w, h) = size_of(source)?;
    if w == 0 || h == 0 || w as u64 * h as u64 > MAX_PIXELS {
        return Err(E_OUTOFMEMORY.into());
    }
    let converter = wic.CreateFormatConverter()?;
    converter.Initialize(
        source,
        &GUID_WICPixelFormat32bppPBGRA,
        WICBitmapDitherTypeNone,
        None,
        0.0,
        WICBitmapPaletteTypeCustom,
    )?;
    let mut pixels = vec![0u8; w as usize * h as usize * 4];
    converter.CopyPixels(std::ptr::null(), w * 4, &mut pixels)?;
    Ok(Frame {
        width: w,
        height: h,
        pixels,
        delay_ms,
    })
}

// SAFETY contract: called on a COM-initialized thread with live WIC objects; out pointers are locals.
unsafe fn read_u16(reader: &IWICMetadataQueryReader, name: &str) -> Option<u16> {
    let mut value = PROPVARIANT::default();
    let name = HSTRING::from(name);
    reader
        .GetMetadataByName(PCWSTR(name.as_ptr()), &mut value)
        .ok()?;
    let inner = &value.Anonymous.Anonymous;
    let result = if inner.vt == VT_UI2 {
        Some(inner.Anonymous.uiVal)
    } else if inner.vt == VT_UI1 {
        Some(inner.Anonymous.bVal as u16)
    } else {
        None
    };
    let _ = PropVariantClear(&mut value);
    result
}

/// What an image shows now, as a painter asks for it ([`crate::raster::ImageResolver`]).
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

/// Rendered pixels: premultiplied BGRA, rows `width * 4` bytes apart.
#[derive(Clone, Debug)]
pub struct Pixmap {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Pixmap {
    /// The pixel at (x, y), un-premultiplied.
    pub fn pixel(&self, x: u32, y: u32) -> Color {
        pixel_at(&self.data, self.width, x, y)
    }

    /// Writes the pixmap as a PNG file.
    pub fn write_png(&self, path: &Path) -> Result<()> {
        encode_png(self.width, self.height, &self.data, path)
    }
}

fn pixel_at(data: &[u8], width: u32, x: u32, y: u32) -> Color {
    let i = ((y * width + x) * 4) as usize;
    let (b, g, r, a) = (data[i], data[i + 1], data[i + 2], data[i + 3]);
    if a == 0 {
        return Color::TRANSPARENT;
    }
    let un = |v: u8| ((v as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
    Color::rgba(un(r), un(g), un(b), a)
}

/// Writes premultiplied BGRA pixels as a PNG (stored un-premultiplied, as PNG requires).
pub fn encode_png(width: u32, height: u32, pbgra: &[u8], path: &Path) -> Result<()> {
    if pbgra.len() != width as usize * height as usize * 4 {
        return Err(E_INVALIDARG.into());
    }
    // PNG stores straight alpha.
    let mut straight = pbgra.to_vec();
    for px in straight.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let wic = factory()?;
    let path = HSTRING::from(path.as_os_str());
    // SAFETY: WIC calls with owned interfaces; `straight` has `height` rows of `width * 4` bytes.
    unsafe {
        let stream = wic.CreateStream()?;
        stream.InitializeFromFilename(PCWSTR(path.as_ptr()), GENERIC_WRITE.0)?;
        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        frame.Initialize(None)?;
        frame.SetSize(width, height)?;
        let mut format: GUID = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut format)?;
        if format != GUID_WICPixelFormat32bppBGRA {
            return Err(E_INVALIDARG.into());
        }
        frame.WritePixels(height, width * 4, &straight)?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
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

    fn checker(w: u32, h: u32) -> Vec<u8> {
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let on = (x / 2 + y / 2) % 2 == 0;
                rgba.extend_from_slice(if on {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 0, 255, 128]
                });
            }
        }
        rgba
    }

    #[test]
    fn decodes_bmp_with_alpha() {
        let bmp = bmp_from_rgba(8, 4, &checker(8, 4));
        let img = decode(&bmp).unwrap();
        assert_eq!((img.width(), img.height()), (8, 4));
        assert_eq!(img.frames[0].pixel(0, 0), Color::rgb(255, 0, 0));
        let c = img.frames[0].pixel(2, 0);
        assert_eq!((c.b, c.a), (255, 128));
        assert_eq!(img.orientation, 1);
    }

    #[test]
    fn png_round_trips() {
        let bmp = bmp_from_rgba(6, 6, &checker(6, 6));
        let img = decode(&bmp).unwrap();
        let dir = std::env::temp_dir().join(format!("snowlark-paint-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("round.png");
        let f = &img.frames[0];
        encode_png(f.width, f.height, &f.pixels, &path).unwrap();
        let back = decode_file(&path).unwrap();
        assert_eq!(back.frames[0].pixels, f.pixels);
        std::fs::remove_dir_all(&dir).ok();
    }

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
    fn png_exif_orientation_is_applied() {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut c = 0xffff_ffffu32;
            for &b in bytes {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        0xedb8_8320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
            }
            !c
        }
        let bmp = bmp_from_rgba(4, 2, &checker(4, 2));
        let img = decode(&bmp).unwrap();
        let dir = std::env::temp_dir().join(format!("snowlark-paint-exif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("o.png");
        let f = &img.frames[0];
        encode_png(f.width, f.height, &f.pixels, &path).unwrap();
        let png = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        // an eXIf chunk (little-endian TIFF, one IFD entry: Orientation = 6) after IHDR
        let mut tiff = b"II*\0\x08\0\0\0\x01\0".to_vec();
        tiff.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0]);
        let mut chunk = (tiff.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(b"eXIf");
        chunk.extend_from_slice(&tiff);
        chunk.extend_from_slice(&crc32(&chunk[4..]).to_be_bytes());
        let ihdr_end = 8 + 12 + 13;
        let mut with_exif = png[..ihdr_end].to_vec();
        with_exif.extend_from_slice(&chunk);
        with_exif.extend_from_slice(&png[ihdr_end..]);
        assert_eq!(exif_orientation(&with_exif), Some(6));
        assert_eq!(exif_orientation(&png), None);
        let turned = decode(&with_exif).unwrap();
        assert_eq!(turned.orientation, 6);
        assert_eq!((turned.width(), turned.height()), (2, 4));
        let raw = turned.unoriented().unwrap();
        assert_eq!(raw.frames[0].pixels, f.pixels);
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
        let img = decode(&bmp_from_rgba(2, 2, &checker(2, 2))).unwrap();
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
