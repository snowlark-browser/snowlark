// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Images: the decoding, store and frame types of `snowlark-graphics` (PNG, JPEG, GIF, WebP and AVIF
//! in Rust), with the formats Windows has a codec for (BMP, ICO, TIFF and others) decoded through WIC,
//! and PNG encoding of rendered pixmaps.

use std::path::Path;

use windows::core::{Result, GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::{E_INVALIDARG, E_OUTOFMEMORY, GENERIC_WRITE};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VT_UI1, VT_UI2};

use crate::com;
use crate::geom::Color;
pub use snowlark_graphics::image::*;

fn factory() -> Result<IWICImagingFactory> {
    com::ensure();
    // SAFETY: plain COM creation on a COM-initialized thread.
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER) }
}

/// Decodes an encoded image held in memory: the formats `snowlark-graphics` decodes in Rust
/// ([`snowlark_graphics::image::decode`]), and the others through WIC ([`decode_wic`]).
pub fn decode(bytes: &[u8]) -> std::result::Result<DecodedImage, Error> {
    match snowlark_graphics::image::decode(bytes) {
        Err(Error::Unsupported) => decode_wic(bytes).map_err(wic_error),
        decoded => decoded,
    }
}

/// A WIC failure as the decoders' error.
fn wic_error(e: windows::core::Error) -> Error {
    if e.code() == E_OUTOFMEMORY {
        Error::TooLarge
    } else if e.code() == E_INVALIDARG {
        Error::Invalid
    } else {
        Error::Platform(e.message())
    }
}

/// Decodes a still image (the first frame of an animation) through WIC: what [`decode`] does for
/// the formats it has no decoder of its own for (BMP, ICO, TIFF, and others Windows has a codec
/// for), and the decoders' benchmark compares against for the rest. No colour profile is applied.
pub fn decode_wic(bytes: &[u8]) -> Result<DecodedImage> {
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
        // (WIC reads TIFF's orientation)
        let mut image = DecodedImage::still(oriented(frame, orientation));
        image.orientation = orientation;
        Ok(image)
    }
}

/// Decodes an image file.
pub fn decode_file(path: &Path) -> std::result::Result<DecodedImage, Error> {
    let bytes = std::fs::read(path).map_err(|_| Error::Invalid)?;
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
}
