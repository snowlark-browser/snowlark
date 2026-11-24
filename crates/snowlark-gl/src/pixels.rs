// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pixel conversions of WebGL's texture uploads (WebGL 1.0 §5.14.8 `texImage2D`,
//! `texSubImage2D`, and §5.14.4 `pixelStorei`'s `UNPACK_FLIP_Y_WEBGL` and
//! `UNPACK_PREMULTIPLY_ALPHA_WEBGL`): an image, canvas, video frame or `ImageData` packed into the
//! format and type a call names, and the same two parameters applied to the bytes of an
//! `ArrayBufferView`. Plain data: the client converts, the service uploads what it is sent.

const RGBA: u32 = 0x1908;
const RGB: u32 = 0x1907;
const ALPHA: u32 = 0x1906;
const LUMINANCE: u32 = 0x1909;
const LUMINANCE_ALPHA: u32 = 0x190A;
const UNSIGNED_BYTE: u32 = 0x1401;
const FLOAT: u32 = 0x1406;
const HALF_FLOAT_OES: u32 = 0x8D61;
const UNSIGNED_SHORT_5_6_5: u32 = 0x8363;
const UNSIGNED_SHORT_4_4_4_4: u32 = 0x8033;
const UNSIGNED_SHORT_5_5_5_1: u32 = 0x8034;

/// Pixels of a source: 8-bit RGBA, rows `width * 4` bytes apart, the top row first.
#[derive(Clone, Copy, Debug)]
pub struct Rgba8<'a> {
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
    /// Whether the colour channels are already multiplied by alpha.
    pub premultiplied: bool,
}

/// How an upload is unpacked: the context's `pixelStorei` state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unpack {
    pub flip_y: bool,
    pub premultiply_alpha: bool,
    /// `UNPACK_ALIGNMENT`: 1, 2, 4 or 8.
    pub alignment: u32,
}

/// The channels of a format, in the order they are stored.
fn channels(format: u32) -> Option<&'static [usize]> {
    // (indices into R, G, B, A; luminance is the red channel)
    Some(match format {
        RGBA => &[0, 1, 2, 3],
        RGB => &[0, 1, 2],
        ALPHA => &[3],
        LUMINANCE => &[0],
        LUMINANCE_ALPHA => &[0, 3],
        _ => return None,
    })
}

/// The bytes one pixel of `format` and `type_` takes, for the combinations WebGL 1 uploads.
pub fn bytes_per_pixel(format: u32, type_: u32) -> Option<usize> {
    let n = channels(format)?.len();
    match (type_, format) {
        (UNSIGNED_BYTE, _) => Some(n),
        (FLOAT, _) => Some(n * 4),
        (HALF_FLOAT_OES, _) => Some(n * 2),
        (UNSIGNED_SHORT_5_6_5, RGB)
        | (UNSIGNED_SHORT_4_4_4_4, RGBA)
        | (UNSIGNED_SHORT_5_5_5_1, RGBA) => Some(2),
        _ => None,
    }
}

/// The bytes a row of `width` pixels takes with its padding to `alignment`.
pub fn row_bytes(width: u32, bpp: usize, alignment: u32) -> usize {
    let a = (alignment.max(1)) as usize;
    (width as usize * bpp).div_ceil(a) * a
}

/// `c * a / 255`, rounded to nearest.
fn mul(c: u8, a: u8) -> u8 {
    ((c as u32 * a as u32 + 127) / 255) as u8
}

/// `c * 255 / a`, rounded to nearest (`a` above zero), at most 255.
fn div(c: u8, a: u8) -> u8 {
    ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
}

/// An IEEE 754 half-precision number nearest to `v` (for `v` in 0..=1).
fn f16_bits(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = bits & 0x7f_ffff;
    if v == 0.0 {
        return sign;
    }
    if exp <= 0 {
        // (a subnormal half: the full mantissa times 2^(exp - 14), rounded)
        let shift = 14 - exp;
        if shift > 24 {
            return sign;
        }
        let full = mant | 0x80_0000;
        return sign | ((full + (1 << (shift - 1))) >> shift) as u16;
    }
    if exp >= 31 {
        return sign | 0x7c00;
    }
    // (round to nearest; a carry out of the mantissa raises the exponent, as it should)
    let h = ((exp as u32) << 10) + ((mant + 0x1000) >> 13);
    sign | h as u16
}

fn f16_value(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let mant = (h & 0x3ff) as f32;
    sign * match exp {
        0 => mant / 1024.0 * 2f32.powi(-14),
        31 => f32::INFINITY,
        _ => (1.0 + mant / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// Packs `src` as `format` and `type_` for an upload: alpha multiplied in or divided out as
/// `unpack.premultiply_alpha` asks, the rows bottom first when `unpack.flip_y`, each row padded to
/// `unpack.alignment`. `None` for a format and type WebGL 1 has no upload of.
pub fn pack(src: &Rgba8, format: u32, type_: u32, unpack: Unpack) -> Option<Vec<u8>> {
    let order = channels(format)?;
    let bpp = bytes_per_pixel(format, type_)?;
    let (w, h) = (src.width as usize, src.height as usize);
    if src.data.len() < w * h * 4 {
        return None;
    }
    let row = row_bytes(src.width, bpp, unpack.alignment);
    let mut out = vec![0u8; row * h];
    for y in 0..h {
        let sy = if unpack.flip_y { h - 1 - y } else { y };
        let line = &src.data[sy * w * 4..(sy + 1) * w * 4];
        let dst = &mut out[y * row..y * row + w * bpp];
        for (x, p) in line.as_chunks::<4>().0.iter().enumerate() {
            let a = p[3];
            let px: [u8; 4] = match (src.premultiplied, unpack.premultiply_alpha) {
                (false, true) => [mul(p[0], a), mul(p[1], a), mul(p[2], a), a],
                (true, false) if a == 0 => [0, 0, 0, 0],
                (true, false) if a < 255 => [div(p[0], a), div(p[1], a), div(p[2], a), a],
                _ => *p,
            };
            let d = &mut dst[x * bpp..(x + 1) * bpp];
            match type_ {
                UNSIGNED_BYTE => {
                    for (o, &c) in d.iter_mut().zip(order) {
                        *o = px[c];
                    }
                }
                FLOAT => {
                    for (o, &c) in d.as_chunks_mut::<4>().0.iter_mut().zip(order) {
                        *o = (px[c] as f32 / 255.0).to_le_bytes();
                    }
                }
                HALF_FLOAT_OES => {
                    for (o, &c) in d.as_chunks_mut::<2>().0.iter_mut().zip(order) {
                        *o = f16_bits(px[c] as f32 / 255.0).to_le_bytes();
                    }
                }
                _ => {
                    let [r, g, b, a] = px.map(|v| v as u16);
                    let v = match type_ {
                        UNSIGNED_SHORT_5_6_5 => (r >> 3) << 11 | (g >> 2) << 5 | b >> 3,
                        UNSIGNED_SHORT_4_4_4_4 => {
                            (r >> 4) << 12 | (g >> 4) << 8 | (b >> 4) << 4 | a >> 4
                        }
                        _ => (r >> 3) << 11 | (g >> 3) << 6 | (b >> 3) << 1 | a >> 7,
                    };
                    d.copy_from_slice(&v.to_le_bytes());
                }
            }
        }
    }
    Some(out)
}

/// Applies `UNPACK_FLIP_Y_WEBGL` and `UNPACK_PREMULTIPLY_ALPHA_WEBGL` to the bytes of an
/// `ArrayBufferView` upload of `width` × `height` pixels of `format` and `type_`, rows padded to
/// `unpack.alignment` (WebGL 1.0 §5.14.8: they apply to every upload). Bytes past the last row are
/// left as they are; a format without alpha is only flipped.
pub fn unpack_view(
    data: &mut [u8],
    width: u32,
    height: u32,
    format: u32,
    type_: u32,
    unpack: Unpack,
) {
    let Some(bpp) = bytes_per_pixel(format, type_) else {
        return;
    };
    let (w, h) = (width as usize, height as usize);
    let row = row_bytes(width, bpp, unpack.alignment);
    if w == 0 || h == 0 || data.len() < row * (h - 1) + w * bpp {
        return;
    }
    if unpack.flip_y {
        for y in 0..h / 2 {
            let (top, rest) = data.split_at_mut((h - 1 - y) * row);
            top[y * row..y * row + w * bpp].swap_with_slice(&mut rest[..w * bpp]);
        }
    }
    if !unpack.premultiply_alpha || !matches!(format, RGBA | LUMINANCE_ALPHA) {
        return;
    }
    let n = channels(format).map_or(0, |c| c.len());
    for y in 0..h {
        let line = &mut data[y * row..y * row + w * bpp];
        match type_ {
            UNSIGNED_BYTE => {
                for px in line.chunks_exact_mut(n) {
                    let a = px[n - 1];
                    for c in &mut px[..n - 1] {
                        *c = mul(*c, a);
                    }
                }
            }
            FLOAT => {
                for px in line.chunks_exact_mut(n * 4) {
                    let a = f32::from_le_bytes(px[(n - 1) * 4..n * 4].try_into().unwrap());
                    for c in px[..(n - 1) * 4].as_chunks_mut::<4>().0 {
                        *c = (f32::from_le_bytes(*c) * a).to_le_bytes();
                    }
                }
            }
            HALF_FLOAT_OES => {
                for px in line.chunks_exact_mut(n * 2) {
                    let a = f16_value(u16::from_le_bytes([px[(n - 1) * 2], px[(n - 1) * 2 + 1]]));
                    for c in px[..(n - 1) * 2].as_chunks_mut::<2>().0 {
                        *c = f16_bits(f16_value(u16::from_le_bytes(*c)) * a).to_le_bytes();
                    }
                }
            }
            UNSIGNED_SHORT_4_4_4_4 => {
                for px in line.as_chunks_mut::<2>().0 {
                    let v = u16::from_le_bytes(*px);
                    let a = (v & 0xf) as u32;
                    let m = |c: u16| ((c as u32 * a + 7) / 15) as u16;
                    let (r, g, b) = (m(v >> 12), m((v >> 8) & 0xf), m((v >> 4) & 0xf));
                    *px = (r << 12 | g << 8 | b << 4 | a as u16).to_le_bytes();
                }
            }
            UNSIGNED_SHORT_5_5_5_1 => {
                for px in line.as_chunks_mut::<2>().0 {
                    if u16::from_le_bytes(*px) & 1 == 0 {
                        *px = [0, 0];
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPAQUE: Unpack = Unpack {
        flip_y: false,
        premultiply_alpha: false,
        alignment: 1,
    };

    fn image(data: &[u8], width: u32, premultiplied: bool) -> Rgba8<'_> {
        Rgba8 {
            width,
            height: (data.len() / 4) as u32 / width,
            data,
            premultiplied,
        }
    }

    #[test]
    fn rgba_bytes_are_copied_top_row_first() {
        let px = [1, 2, 3, 255, 4, 5, 6, 255];
        let src = image(&px, 1, false);
        assert_eq!(pack(&src, RGBA, UNSIGNED_BYTE, OPAQUE).unwrap(), px);
    }

    #[test]
    fn flip_y_puts_the_bottom_row_first() {
        let px = [1, 2, 3, 255, 4, 5, 6, 255];
        let src = image(&px, 1, false);
        let flip = Unpack {
            flip_y: true,
            ..OPAQUE
        };
        assert_eq!(
            pack(&src, RGBA, UNSIGNED_BYTE, flip).unwrap(),
            [4, 5, 6, 255, 1, 2, 3, 255]
        );
    }

    #[test]
    fn alpha_is_multiplied_in_or_divided_out() {
        let straight = [200, 100, 0, 128];
        let pre = Unpack {
            premultiply_alpha: true,
            ..OPAQUE
        };
        assert_eq!(
            pack(&image(&straight, 1, false), RGBA, UNSIGNED_BYTE, pre).unwrap(),
            [100, 50, 0, 128]
        );
        assert_eq!(
            pack(
                &image(&[100, 50, 0, 128], 1, true),
                RGBA,
                UNSIGNED_BYTE,
                OPAQUE
            )
            .unwrap(),
            [199, 100, 0, 128]
        );
        // (a transparent premultiplied pixel has no colour to recover)
        assert_eq!(
            pack(&image(&[0, 0, 0, 0], 1, true), RGBA, UNSIGNED_BYTE, OPAQUE).unwrap(),
            [0, 0, 0, 0]
        );
    }

    #[test]
    fn formats_take_their_channels() {
        let src = image(&[10, 20, 30, 40], 1, false);
        let p = |f, t| pack(&src, f, t, OPAQUE).unwrap();
        assert_eq!(p(RGB, UNSIGNED_BYTE), [10, 20, 30]);
        assert_eq!(p(ALPHA, UNSIGNED_BYTE), [40]);
        assert_eq!(p(LUMINANCE, UNSIGNED_BYTE), [10]);
        assert_eq!(p(LUMINANCE_ALPHA, UNSIGNED_BYTE), [10, 40]);
        let white = image(&[255, 255, 255, 255], 1, false);
        let p = |f, t| pack(&white, f, t, OPAQUE).unwrap();
        assert_eq!(p(RGB, UNSIGNED_SHORT_5_6_5), 0xffffu16.to_le_bytes());
        assert_eq!(p(RGBA, UNSIGNED_SHORT_4_4_4_4), 0xffffu16.to_le_bytes());
        assert_eq!(p(RGBA, UNSIGNED_SHORT_5_5_5_1), 0xffffu16.to_le_bytes());
        assert_eq!(p(ALPHA, FLOAT), 1.0f32.to_le_bytes());
        assert_eq!(p(ALPHA, HALF_FLOAT_OES), 0x3c00u16.to_le_bytes());
        assert!(pack(&white, RGB, UNSIGNED_SHORT_4_4_4_4, OPAQUE).is_none());
    }

    #[test]
    fn rows_are_padded_to_the_alignment() {
        let src = image(&[1, 2, 3, 255, 4, 5, 6, 255], 1, false);
        let four = Unpack {
            alignment: 4,
            ..OPAQUE
        };
        assert_eq!(
            pack(&src, RGB, UNSIGNED_BYTE, four).unwrap(),
            [1, 2, 3, 0, 4, 5, 6, 0]
        );
    }

    #[test]
    fn half_floats_round_to_nearest() {
        assert_eq!(f16_bits(0.5), 0x3800);
        assert_eq!(f16_bits(1.0 / 255.0), 0x1c04);
        assert!((f16_value(f16_bits(0.2)) - 0.2).abs() < 0.0005);
        // (2^-20 is a subnormal half: 2^-24 times 16)
        assert_eq!(f16_bits(2f32.powi(-20)), 16);
        assert_eq!(f16_value(16), 2f32.powi(-20));
    }

    #[test]
    fn a_view_is_flipped_and_premultiplied() {
        // two rows of one RGBA pixel, padded to 4 (no padding needed)
        let mut data = vec![200, 100, 0, 128, 10, 20, 30, 255];
        let both = Unpack {
            flip_y: true,
            premultiply_alpha: true,
            alignment: 4,
        };
        unpack_view(&mut data, 1, 2, RGBA, UNSIGNED_BYTE, both);
        assert_eq!(data, [10, 20, 30, 255, 100, 50, 0, 128]);
        // RGB rows of 3 bytes padded to 4: only the pixels move
        let mut rgb = vec![1, 2, 3, 9, 4, 5, 6];
        unpack_view(&mut rgb, 1, 2, RGB, UNSIGNED_BYTE, both);
        assert_eq!(rgb, [4, 5, 6, 9, 1, 2, 3]);
    }
}
