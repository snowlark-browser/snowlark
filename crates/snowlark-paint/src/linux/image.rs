// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Images on Linux: the decoding, store and frame types of `snowlark-graphics` (PNG, JPEG, GIF, WebP
//! and AVIF in Rust); of the formats Windows decodes through WIC, BMP, ICO and WBMP go through
//! Skia's codecs, and TIFF is refused ([`Error::Unsupported`]). Rendered pixmaps are encoded as PNG
//! by the `png` crate.

use std::path::Path;

use crate::geom::Color;
pub use snowlark_graphics::image::*;

/// Decodes an encoded image held in memory: the formats `snowlark-graphics` decodes in Rust
/// ([`snowlark_graphics::image::decode`]), and BMP, ICO and WBMP through Skia.
pub fn decode(bytes: &[u8]) -> Result<DecodedImage, Error> {
    match snowlark_graphics::image::decode(bytes) {
        Err(Error::Unsupported) => decode_skia(bytes),
        decoded => decoded,
    }
}

fn decode_skia(bytes: &[u8]) -> Result<DecodedImage, Error> {
    use skia_safe as sk;
    let mut codec =
        sk::codec::Codec::from_data(sk::Data::new_copy(bytes)).ok_or(Error::Unsupported)?;
    let size = codec.dimensions();
    if size.width <= 0 || size.height <= 0 {
        return Err(Error::Invalid);
    }
    let (width, height) = (size.width as u32, size.height as u32);
    // (refused before its pixels are allocated, as the Rust decoders refuse one)
    if width as u64 * height as u64 > MAX_PIXELS {
        return Err(Error::TooLarge);
    }
    let info = sk::ImageInfo::new(size, sk::ColorType::BGRA8888, sk::AlphaType::Premul, None);
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    match codec.get_pixels_with_options(&info, &mut pixels, width as usize * 4, None) {
        sk::codec::Result::Success | sk::codec::Result::IncompleteInput => {}
        _ => return Err(Error::Invalid),
    }
    Ok(DecodedImage::still(Frame {
        width,
        height,
        pixels,
        delay_ms: 0,
    }))
}

/// Decodes an image file.
pub fn decode_file(path: &Path) -> Result<DecodedImage, Error> {
    let bytes = std::fs::read(path).map_err(|_| Error::Invalid)?;
    decode(&bytes)
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
    pub fn write_png(&self, path: &Path) -> Result<(), Error> {
        encode_png(self.width, self.height, &self.data, path)
    }
}

/// Writes premultiplied BGRA pixels as a PNG (stored un-premultiplied, as PNG requires).
pub fn encode_png(width: u32, height: u32, pbgra: &[u8], path: &Path) -> Result<(), Error> {
    if pbgra.len() != width as usize * height as usize * 4 {
        return Err(Error::Invalid);
    }
    // PNG stores straight alpha, in RGBA order.
    let mut rgba = pbgra.to_vec();
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let platform = |e: &dyn std::fmt::Display| Error::Platform(e.to_string());
    let file = std::fs::File::create(path).map_err(|e| platform(&e))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| platform(&e))?;
    writer.write_image_data(&rgba).map_err(|e| platform(&e))?;
    writer.finish().map_err(|e| platform(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trips() {
        // premultiplied BGRA: opaque red, half-transparent blue, transparent, opaque white
        let pixels = [
            0, 0, 255, 255, 128, 0, 0, 128, 0, 0, 0, 0, 255, 255, 255, 255,
        ];
        let dir = std::env::temp_dir().join(format!("snowlark-paint-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("round.png");
        encode_png(2, 2, &pixels, &path).unwrap();
        let back = decode_file(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!((back.width(), back.height()), (2, 2));
        assert_eq!(back.frames[0].pixels, pixels);
        assert_eq!(back.frames[0].pixel(1, 0), Color::rgba(0, 0, 255, 128));
    }

    #[test]
    fn bmp_decodes_through_skia() {
        let bmp = bmp_from_rgba(2, 1, &[255, 0, 0, 255, 0, 0, 255, 128]);
        assert_eq!(
            snowlark_graphics::image::decode(&bmp).unwrap_err(),
            Error::Unsupported
        );
        let image = decode(&bmp).unwrap();
        assert_eq!((image.width(), image.height()), (2, 1));
        assert_eq!(image.frames[0].pixel(0, 0), Color::rgba(255, 0, 0, 255));
        let blue = image.frames[0].pixel(1, 0);
        assert_eq!((blue.b, blue.a), (255, 128), "{blue:?}");
    }

    #[test]
    fn what_no_decoder_knows_is_refused() {
        assert_eq!(decode(b"not an image").unwrap_err(), Error::Unsupported);
    }
}
