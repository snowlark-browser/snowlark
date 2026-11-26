// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The image formats decoded in Rust: PNG (`png`), JPEG (`zune-jpeg`), GIF (`gif`), WebP
//! (`image-webp`) and AVIF (`crabby-avif`, whose AV1 frames dav1d decodes, in a build that has
//! it), each to premultiplied BGRA with its colour profile applied
//! ([`crate::color_profile`]). Which format a file is comes from its signature (MIME Sniffing
//! §6.1, "Matching an image type pattern"), not from its Content-Type. Animations are put
//! together by [`crate::image`] from the frames these give.

use std::io::Cursor;
use std::sync::Arc;

use crate::color_profile::{transform_for, ColorInfo, ColorTransform};
use crate::image::{Frame, MAX_PIXELS};
use crate::{Error, Result};

/// The formats decoded here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
    /// (only in a build with dav1d: [`AVIF`])
    Avif,
}

/// The format `bytes` start like (MIME Sniffing §6.1's image patterns, and AVIF's `ftyp` brands as
/// other engines sniff them); `None` for the others.
pub(crate) fn sniff(bytes: &[u8]) -> Option<Format> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Format::Jpeg)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Format::Png)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(Format::Gif)
    } else if bytes.len() >= 14 && &bytes[0..4] == b"RIFF" && &bytes[8..14] == b"WEBPVP" {
        Some(Format::Webp)
    } else if AVIF && is_avif(bytes) {
        Some(Format::Avif)
    } else {
        None
    }
}

pub(crate) fn invalid() -> Error {
    Error::Invalid
}

/// An image's size, refused when it is empty or larger than [`MAX_PIXELS`].
pub(crate) fn check_size(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(invalid());
    }
    if width as u64 * height as u64 > MAX_PIXELS {
        return Err(Error::TooLarge);
    }
    Ok(())
}

/// `c * a / 255`, rounded to nearest.
#[inline]
fn mul_div_255(c: u8, a: u8) -> u8 {
    let t = c as u32 * a as u32 + 128;
    ((t + (t >> 8)) >> 8) as u8
}

/// Straight-alpha RGBA to premultiplied BGRA, in place.
pub(crate) fn rgba_to_pbgra(pixels: &mut [u8]) {
    for px in pixels.as_chunks_mut::<4>().0 {
        let a = px[3];
        let (r, g, b) = (px[0], px[1], px[2]);
        *px = match a {
            255 => [b, g, r, 255],
            0 => [0, 0, 0, 0],
            _ => [mul_div_255(b, a), mul_div_255(g, a), mul_div_255(r, a), a],
        };
    }
}

/// Pixels of `N` bytes each, `f` making four bytes of each.
fn expand<const N: usize>(src: &[u8], f: impl Fn(&[u8; N]) -> [u8; 4]) -> Vec<u8> {
    let src = src.as_chunks::<N>().0;
    let mut out = vec![0u8; src.len() * 4];
    for (o, i) in out.as_chunks_mut::<4>().0.iter_mut().zip(src) {
        *o = f(i);
    }
    out
}

/// Opaque BGRA pixels with `color` applied, as a frame.
fn opaque(width: u32, height: u32, mut bgra: Vec<u8>, color: Option<&ColorTransform>) -> Frame {
    if let Some(t) = color {
        t.apply_bgra(&mut bgra);
    }
    Frame {
        width,
        height,
        pixels: bgra,
        delay_ms: 0,
    }
}

/// Straight-alpha RGBA pixels with `color` applied, as a (premultiplied BGRA) frame.
fn with_alpha(width: u32, height: u32, mut rgba: Vec<u8>, color: Option<&ColorTransform>) -> Frame {
    if let Some(t) = color {
        t.apply_rgba(&mut rgba);
    }
    rgba_to_pbgra(&mut rgba);
    Frame {
        width,
        height,
        pixels: rgba,
        delay_ms: 0,
    }
}

// --- PNG ---

/// A PNG's colour description (PNG 3rd edition §11.3.2).
fn png_color(info: &png::Info) -> Option<ColorTransform> {
    let xy = |p: &(png::ScaledFloat, png::ScaledFloat)| (p.0.into_value(), p.1.into_value());
    let color = ColorInfo {
        cicp: info.coding_independent_code_points.map(|c| {
            (
                c.color_primaries,
                c.transfer_function,
                c.matrix_coefficients,
                c.is_video_full_range_image,
            )
        }),
        icc: info.icc_profile.as_deref(),
        srgb: info.srgb.is_some(),
        gamma: info.gama_chunk.map(|g| g.into_value()),
        chromaticities: info
            .chrm_chunk
            .map(|c| [xy(&c.white), xy(&c.red), xy(&c.green), xy(&c.blue)]),
    };
    transform_for(&color)
}

/// An 8-bit palette's 256 colours as premultiplied BGRA with `color` applied (`PLTE` with its
/// `tRNS` alphas; indices past the palette take its last colour, or black).
fn palette_table(info: &png::Info, color: Option<&ColorTransform>) -> Option<Vec<[u8; 4]>> {
    let plte = info.palette.as_deref()?.as_chunks::<3>().0;
    let trns = info.trns.as_deref().unwrap_or(&[]);
    let mut rgba = Vec::with_capacity(256 * 4);
    for i in 0..256 {
        let [r, g, b] = plte.get(i).or(plte.last()).copied().unwrap_or([0; 3]);
        let a = if i < plte.len() {
            trns.get(i).copied().unwrap_or(255)
        } else {
            255
        };
        rgba.extend_from_slice(&[r, g, b, a]);
    }
    if let Some(t) = color {
        t.apply_rgba(&mut rgba);
    }
    rgba_to_pbgra(&mut rgba);
    Some(rgba.as_chunks::<4>().0.to_vec())
}

/// Decodes a PNG's image (the `IDAT` image of an animated one). `color` is the conversion to
/// use, when the caller already has it (`None`: read it from the file); the one used is returned.
pub(crate) fn png(
    bytes: &[u8],
    color: Option<Option<&ColorTransform>>,
) -> Result<(Frame, Option<ColorTransform>)> {
    use png::{BitDepth, ColorType, Transformations};
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_ignore_text_chunk(true);
    // (a chunk whose CRC does not match is read all the same, as other engines read it)
    decoder.ignore_checksums(true);
    let header = decoder.read_header_info().map_err(|_| invalid())?;
    let (width, height) = (header.width, header.height);
    check_size(width, height)?;
    // An 8-bit palette image maps its indices through a table of its colours; the others have
    // their palettes and low bit depths widened to 8 bits, tRNS made alpha, and 16 bits cut to 8.
    let indexed = header.color_type == ColorType::Indexed && header.bit_depth == BitDepth::Eight;
    decoder.set_transformations(if indexed {
        Transformations::IDENTITY
    } else {
        Transformations::EXPAND | Transformations::STRIP_16
    });
    let mut reader = decoder.read_info().map_err(|_| invalid())?;
    let own = match color {
        Some(_) => None,
        None => png_color(reader.info()),
    };
    let used = match color {
        Some(c) => c,
        None => own.as_ref(),
    };
    let table = if indexed {
        Some(palette_table(reader.info(), used).ok_or_else(invalid)?)
    } else {
        None
    };
    let size = reader.output_buffer_size().ok_or_else(invalid)?;
    let mut buf = vec![0u8; size];
    let out = reader.next_frame(&mut buf).map_err(|_| invalid())?;
    buf.truncate(out.buffer_size());
    let frame = match (out.color_type, table) {
        (ColorType::Indexed, Some(table)) => {
            let mut pixels = vec![0u8; buf.len() * 4];
            for (o, &i) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(&buf) {
                *o = table[i as usize];
            }
            Frame {
                width,
                height,
                pixels,
                delay_ms: 0,
            }
        }
        (ColorType::Rgba, _) => with_alpha(width, height, buf, used),
        (ColorType::Rgb, _) => {
            let bgra = expand(&buf, |&[r, g, b]| [b, g, r, 255]);
            opaque(width, height, bgra, used)
        }
        (ColorType::GrayscaleAlpha, _) => {
            let rgba = expand(&buf, |&[v, a]| [v, v, v, a]);
            with_alpha(width, height, rgba, used)
        }
        (ColorType::Grayscale, _) => {
            let bgra = expand(&buf, |&[v]| [v, v, v, 255]);
            opaque(width, height, bgra, used)
        }
        _ => return Err(invalid()),
    };
    Ok((frame, own))
}

// --- JPEG ---

/// Decodes a JPEG (baseline or progressive; gray, YCbCr, RGB, CMYK or YCCK) with its ICC
/// profile applied. The EXIF orientation is the caller's.
pub(crate) fn jpeg(bytes: &[u8]) -> Result<Frame> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        .set_max_width(u16::MAX as usize)
        .set_max_height(u16::MAX as usize);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(|_| invalid())?;
    let info = decoder.info().ok_or_else(invalid)?;
    let (width, height) = (info.width as u32, info.height as u32);
    check_size(width, height)?;
    // CMYK and YCCK (Adobe's inverted CMYK, as other engines read it: R = C * K / 255) convert
    // to RGB only with three channels out.
    let four = matches!(
        decoder.input_colorspace(),
        Some(ColorSpace::CMYK | ColorSpace::YCCK)
    );
    if four {
        decoder.set_options(options.jpeg_set_out_colorspace(ColorSpace::RGB));
    }
    let channels = if four { 3 } else { 4 };
    let size = decoder.output_buffer_size().ok_or_else(invalid)?;
    if size != width as usize * height as usize * channels {
        return Err(invalid());
    }
    let mut pixels = vec![0u8; size];
    decoder.decode_into(&mut pixels).map_err(|_| invalid())?;
    if four {
        // (a CMYK profile does not describe the RGB made here, so none is applied)
        let bgra = expand(&pixels, |&[r, g, b]| [b, g, r, 255]);
        return Ok(opaque(width, height, bgra, None));
    }
    let icc = decoder.icc_profile();
    let color = transform_for(&ColorInfo {
        icc: icc.as_deref(),
        ..ColorInfo::default()
    });
    if let Some(t) = &color {
        t.apply_rgba(&mut pixels);
    }
    // (opaque: only the channel order changes)
    for px in pixels.as_chunks_mut::<4>().0 {
        *px = [px[2], px[1], px[0], 255];
    }
    Ok(opaque(width, height, pixels, None))
}

// --- GIF ---

/// A GIF decoder reading its own copy of the file's bytes.
pub(crate) type GifDecoder = gif::Decoder<Cursor<Arc<[u8]>>>;

/// A decoder of a GIF's frames: each frame's own rectangle, as colour indices (`decode`), or only
/// their headers (`!decode`).
pub(crate) fn gif_decoder(bytes: Arc<[u8]>, decode: bool) -> Result<GifDecoder> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::Indexed);
    options.skip_frame_decoding(!decode);
    options.allow_unknown_blocks(true);
    options.set_memory_limit(gif::MemoryLimit::Bytes(
        std::num::NonZeroU64::new(MAX_PIXELS * 4).expect("non-zero"),
    ));
    options.read_info(Cursor::new(bytes)).map_err(|_| invalid())
}

/// A GIF's canvas and frames, read from the headers only.
pub(crate) struct GifLayout {
    pub width: u32,
    pub height: u32,
    /// Each frame's delay, in milliseconds (as the file gives it).
    pub delays: Vec<u32>,
}

/// The canvas size of a GIF and its frames' delays. The canvas is the logical screen, grown to
/// hold the first frame when that is larger (a logical screen of 0 × 0 is common).
pub(crate) fn gif_layout(bytes: Arc<[u8]>) -> Result<GifLayout> {
    let mut decoder = gif_decoder(bytes, false)?;
    let (mut width, mut height) = (decoder.width() as u32, decoder.height() as u32);
    let mut delays = Vec::new();
    loop {
        match decoder.read_next_frame() {
            Ok(Some(f)) => {
                if delays.is_empty() {
                    width = width.max(f.left as u32 + f.width as u32);
                    height = height.max(f.top as u32 + f.height as u32);
                }
                delays.push(f.delay as u32 * 10);
            }
            Ok(None) => break,
            // (a frame that does not read ends the animation)
            Err(_) => break,
        }
    }
    if delays.is_empty() {
        return Err(invalid());
    }
    check_size(width, height)?;
    Ok(GifLayout {
        width,
        height,
        delays,
    })
}

/// One GIF frame (GIF89a §20-23): its pixels as premultiplied BGRA (its own rectangle), where it
/// goes, its disposal, and whether it has a transparent colour (without one it replaces what is
/// under it).
pub(crate) struct GifFrame {
    pub pixels: Vec<u8>,
    pub at: crate::animation::Region,
    pub dispose: crate::animation::Dispose,
    pub transparent: bool,
}

/// The next GIF frame; `None` after the last.
pub(crate) fn gif_next(decoder: &mut GifDecoder) -> Result<Option<GifFrame>> {
    use crate::animation::{Dispose, Region};
    // (the global colour table, for frames without a local one)
    let global: Option<Vec<u8>> = decoder.global_palette().map(<[u8]>::to_vec);
    let Some(f) = decoder.read_next_frame().map_err(|_| invalid())? else {
        return Ok(None);
    };
    let at = Region {
        x: f.left as u32,
        y: f.top as u32,
        width: f.width as u32,
        height: f.height as u32,
    };
    let dispose = match f.dispose {
        gif::DisposalMethod::Background => Dispose::Background,
        gif::DisposalMethod::Previous => Dispose::Previous,
        _ => Dispose::None,
    };
    if f.buffer.len() != at.width as usize * at.height as usize {
        return Err(invalid());
    }
    let palette = f
        .palette
        .as_deref()
        .or(global.as_deref())
        .ok_or_else(invalid)?
        .as_chunks::<3>()
        .0;
    // Indices past the colour table are black, as in other engines.
    let mut table = [[0, 0, 0, 255u8]; 256];
    for (t, &[r, g, b]) in table.iter_mut().zip(palette) {
        *t = [b, g, r, 255];
    }
    if let Some(i) = f.transparent {
        table[i as usize] = [0; 4];
    }
    let mut pixels = vec![0u8; f.buffer.len() * 4];
    for (o, &i) in pixels
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(f.buffer.iter())
    {
        *o = table[i as usize];
    }
    Ok(Some(GifFrame {
        pixels,
        at,
        dispose,
        transparent: f.transparent.is_some(),
    }))
}

// --- WebP ---

/// Decodes a still WebP (lossy, lossless, with or without alpha) with its ICC profile applied. An
/// animation's frames come here one by one, each made a still file of its own
/// ([`crate::animation::WebpAnimation::frame_webp`]), and are composited by the caller (whose
/// frames carry no profile: an animation's is not applied, as in other engines).
pub(crate) fn webp(bytes: &[u8]) -> Result<Frame> {
    let mut decoder = image_webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|_| invalid())?;
    let (width, height) = decoder.dimensions();
    check_size(width, height)?;
    if decoder.is_animated() {
        return Err(invalid());
    }
    let alpha = decoder.has_alpha();
    let size = decoder.output_buffer_size().ok_or_else(invalid)?;
    let mut pixels = vec![0u8; size];
    decoder.read_image(&mut pixels).map_err(|_| invalid())?;
    let icc = decoder.icc_profile().ok().flatten();
    let color = transform_for(&ColorInfo {
        icc: icc.as_deref(),
        ..ColorInfo::default()
    });
    Ok(if alpha {
        with_alpha(width, height, pixels, color.as_ref())
    } else {
        let bgra = expand(&pixels, |&[r, g, b]| [b, g, r, 255]);
        opaque(width, height, bgra, color.as_ref())
    })
}

// --- AVIF ---

/// Whether AVIF is decoded here: the build has the dav1d library (`dav1d_sys::LINKED`). Without
/// it AVIF goes to WIC.
pub(crate) const AVIF: bool = dav1d_sys::LINKED;

fn avif_error(e: crabby_avif::AvifError) -> Error {
    match e {
        crabby_avif::AvifError::OutOfMemory => Error::TooLarge,
        _ => invalid(),
    }
}

/// An AVIF file being decoded (AV1 Image File Format, with the defaults of MIAF §7.3.6.4 for what
/// it leaves unsaid): the image, or an image sequence's frames one after another.
pub(crate) struct AvifDecoder {
    decoder: Box<crabby_avif::decoder::Decoder>,
    color: Option<ColorTransform>,
    /// The size shown: the clean aperture's, when it has one starting at the origin (a valid
    /// `clap` elsewhere, or an invalid one, is ignored, as other engines do).
    pub width: u32,
    pub height: u32,
    /// The EXIF orientation that its `irot` and `imir` properties make (MIAF §7.3.6.7: rotation,
    /// then mirroring).
    pub orientation: u8,
    /// The frames' durations (ms) and how many times the sequence plays (`None`: forever).
    pub durations: Vec<u32>,
    pub plays: Option<u32>,
}

/// Opens an AVIF file and reads its container (ISO/IEC 23000-22): size, colour, orientation and,
/// for an image sequence, the frames' timing. Its pixels come from [`avif_next`].
pub(crate) fn avif_decoder(bytes: &[u8]) -> Result<AvifDecoder> {
    use crabby_avif::decoder::{Decoder, StrictnessFlag};
    use crabby_avif::{ColorPrimaries, RepetitionCount, TransferCharacteristics};
    if !AVIF {
        return Err(invalid());
    }
    let mut decoder = Box::new(Decoder::default());
    let settings = &mut decoder.settings;
    settings.codec_choice = crabby_avif::CodecChoice::Dav1d;
    settings.max_threads = 2;
    settings.ignore_exif = true;
    settings.ignore_xmp = true;
    // (a 'clap' is checked here, and a missing 'pixi' is allowed: older encoders left it out)
    settings.strictness = crabby_avif::decoder::Strictness::SpecificExclude(vec![
        StrictnessFlag::ClapValid,
        StrictnessFlag::PixiRequired,
    ]);
    settings.image_size_limit = std::num::NonZeroU32::new(MAX_PIXELS as u32);
    decoder.set_io_vec(bytes.to_vec());
    decoder.parse().map_err(avif_error)?;
    let count = decoder.image_count();
    let image = decoder.image().ok_or_else(invalid)?;
    if image.depth == 0 || count == 0 {
        return Err(invalid());
    }
    let (mut width, mut height) = (image.width, image.height);
    if let Some(clap) = &image.clap {
        let rect = crabby_avif::utils::clap::CropRect::create_from(
            clap,
            image.width,
            image.height,
            image.yuv_format,
        );
        if let Ok(r) = rect {
            if r.x == 0 && r.y == 0 && r.width > 0 && r.height > 0 {
                (width, height) = (r.width, r.height);
            }
        }
    }
    check_size(width, height)?;
    // An ICC profile, else the colour description (H.273), its unspecified primaries and transfer
    // taken as sRGB's. The pixels are converted to full-range RGB first, so the matrix and range
    // are not part of the colour space here.
    let color = if !image.icc.is_empty() {
        transform_for(&ColorInfo {
            icc: Some(&image.icc),
            ..ColorInfo::default()
        })
    } else if image.color_primaries != ColorPrimaries::Unspecified
        || image.transfer_characteristics != TransferCharacteristics::Unspecified
    {
        let primaries = match image.color_primaries {
            ColorPrimaries::Unspecified => ColorPrimaries::Srgb,
            p => p,
        };
        let transfer = match image.transfer_characteristics {
            TransferCharacteristics::Unspecified => TransferCharacteristics::Srgb,
            t => t,
        };
        transform_for(&ColorInfo {
            cicp: Some((primaries as u8, transfer as u8, 0, true)),
            ..ColorInfo::default()
        })
    } else {
        None
    };
    // (angle × 90° anti-clockwise, then mirroring: axis 0 swaps top and bottom, 1 left and right)
    const ORIENTATION: [[u8; 4]; 3] = [[1, 8, 3, 6], [4, 5, 2, 7], [2, 7, 4, 5]];
    let angle = image.irot_angle.unwrap_or(0).min(3) as usize;
    let axis = image.imir_axis.map_or(0, |a| a.min(1) as usize + 1);
    let orientation = ORIENTATION[axis][angle];
    let durations = (0..count)
        .map(|i| {
            decoder.nth_image_timing(i).map_or(0, |t| {
                (t.duration * 1000.0).round().clamp(0.0, u32::MAX as f64) as u32
            })
        })
        .collect();
    // (a sequence without an edit list loops forever, as before edit lists were read)
    let plays = match decoder.repetition_count() {
        RepetitionCount::Finite(n) => Some(n.saturating_add(1)),
        RepetitionCount::Infinite | RepetitionCount::Unknown => None,
    };
    Ok(AvifDecoder {
        decoder,
        color,
        width,
        height,
        orientation,
        durations,
        plays,
    })
}

impl AvifDecoder {
    pub fn frames(&self) -> usize {
        self.durations.len()
    }
}

/// The next image of an AVIF file (its first image after [`avif_decoder`]) as premultiplied BGRA,
/// cropped to the clean aperture, with its colour profile applied, before orientation.
pub(crate) fn avif_next(d: &mut AvifDecoder) -> Result<Frame> {
    use crabby_avif::reformat::rgb;
    d.decoder.next_image().map_err(avif_error)?;
    let image = d.decoder.image().ok_or_else(invalid)?;
    if image.width < d.width || image.height < d.height {
        return Err(invalid());
    }
    let mut out = rgb::Image::create_from_yuv(image);
    out.depth = 8;
    out.max_threads = 2;
    // Straight alpha: the colour profile applies before premultiplying.
    out.format = if image.alpha_present {
        rgb::Format::Rgba
    } else {
        rgb::Format::Bgra
    };
    out.allocate().map_err(avif_error)?;
    out.convert_from_yuv(image).map_err(avif_error)?;
    let (w, h) = (d.width as usize, d.height as usize);
    let mut pixels = Vec::with_capacity(w * h * 4);
    for y in 0..d.height {
        let row = out.row(y).map_err(avif_error)?;
        pixels.extend_from_slice(row.get(..w * 4).ok_or_else(invalid)?);
    }
    Ok(if image.alpha_present {
        with_alpha(d.width, d.height, pixels, d.color.as_ref())
    } else {
        opaque(d.width, d.height, pixels, d.color.as_ref())
    })
}

/// Whether `bytes` are an AVIF file (an ISO BMFF `ftyp` box with an AVIF brand).
fn is_avif(bytes: &[u8]) -> bool {
    crabby_avif::decoder::Decoder::peek_compatible_file_type(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_by_signature() {
        assert_eq!(sniff(b"\xff\xd8\xff\xe0rest"), Some(Format::Jpeg));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some(Format::Png));
        assert_eq!(sniff(b"GIF89a..."), Some(Format::Gif));
        assert_eq!(sniff(b"GIF87a..."), Some(Format::Gif));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::Webp));
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt "), None);
        assert_eq!(sniff(b"BM...."), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn premultiplies_and_swaps() {
        let mut px = [255, 128, 0, 255, 255, 128, 0, 128, 10, 20, 30, 0];
        rgba_to_pbgra(&mut px);
        assert_eq!(px, [0, 128, 255, 255, 0, 64, 128, 128, 0, 0, 0, 0]);
        // rounding: 200 * 100 / 255 = 78.43
        assert_eq!(mul_div_255(200, 100), 78);
        assert_eq!(mul_div_255(255, 255), 255);
        assert_eq!(mul_div_255(1, 128), 1);
    }
}
