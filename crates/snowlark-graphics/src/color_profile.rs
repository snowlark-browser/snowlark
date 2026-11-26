// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Colour management of decoded images: an image's colour space (an embedded ICC profile, a PNG's
//! `cICP`, or its `gAMA` and `cHRM` chunks) converted to sRGB, the space pages and painting work in
//! (CSS Color 4 §10: untagged and sRGB images are drawn as they are; a tagged image is converted).
//! The conversion uses qcms; an image whose space is sRGB, or close enough that no 8-bit value
//! changes by more than one, is left as it is.

use std::sync::OnceLock;

use qcms::{DataType, Intent, Profile, Transform};

/// The sRGB profile conversions go to, with its output tables computed once.
fn srgb() -> &'static Profile {
    static SRGB: OnceLock<Box<Profile>> = OnceLock::new();
    SRGB.get_or_init(|| {
        let mut p = Profile::new_sRGB();
        p.precache_output_transform();
        p
    })
}

/// Where an image's colour space comes from, in the order a PNG decoder looks (PNG 3rd edition
/// §4.3: `cICP` first, then `iCCP`, then `sRGB`, then `gAMA` with `cHRM`).
#[derive(Clone, Copy, Debug, Default)]
pub struct ColorInfo<'a> {
    /// Coding-independent code points (ITU-T H.273): colour primaries, transfer characteristics,
    /// matrix coefficients, full range.
    pub cicp: Option<(u8, u8, u8, bool)>,
    /// An embedded ICC profile.
    pub icc: Option<&'a [u8]>,
    /// The image says it is sRGB (a PNG `sRGB` chunk).
    pub srgb: bool,
    /// The file gamma (PNG `gAMA`: encoded = linear ^ gamma).
    pub gamma: Option<f32>,
    /// The chromaticities (PNG `cHRM`): white, red, green and blue, each (x, y).
    pub chromaticities: Option<[(f32, f32); 4]>,
}

/// A conversion of straight-alpha RGBA or BGRA pixels to sRGB.
pub struct ColorTransform {
    kind: Kind,
}

enum Kind {
    /// An RGB space: qcms converts each pixel.
    Rgb { rgba: Transform, bgra: Transform },
    /// A gray space: each gray level maps to an sRGB colour (the pixel's red, green and blue
    /// channels hold the same level).
    Gray(Box<[[u8; 3]; 256]>),
}

impl ColorTransform {
    fn from_profile(profile: &Profile) -> Option<ColorTransform> {
        if profile.is_sRGB() {
            return None;
        }
        let out = srgb();
        let rgba = Transform::new(profile, out, DataType::RGBA8, Intent::default())?;
        let bgra = Transform::new(profile, out, DataType::BGRA8, Intent::default())?;
        let t = ColorTransform {
            kind: Kind::Rgb { rgba, bgra },
        };
        (!t.is_identity()).then_some(t)
    }

    fn gray(profile: &Profile) -> Option<ColorTransform> {
        let t = Transform::new_to(
            profile,
            srgb(),
            DataType::Gray8,
            DataType::RGB8,
            Intent::default(),
        )?;
        let levels: Vec<u8> = (0..=255).collect();
        let mut rgb = vec![0u8; 256 * 3];
        t.convert(&levels, &mut rgb);
        let mut table = Box::new([[0u8; 3]; 256]);
        for (i, c) in rgb.as_chunks::<3>().0.iter().enumerate() {
            table[i] = *c;
        }
        let identity = table
            .iter()
            .enumerate()
            .all(|(i, c)| c.iter().all(|&v| v.abs_diff(i as u8) <= 1));
        (!identity).then_some(ColorTransform {
            kind: Kind::Gray(table),
        })
    }

    /// Whether the conversion moves no 8-bit value by more than one (an sRGB profile written
    /// out as a table, say): such images are drawn as they are, as an untagged one is.
    fn is_identity(&self) -> bool {
        // (every combination of 16 levels per channel, the extremes included)
        let mut samples = Vec::with_capacity(16 * 16 * 16 * 4);
        for r in 0..16u32 {
            for g in 0..16u32 {
                for b in 0..16u32 {
                    let level = |v: u32| (v * 17) as u8;
                    samples.extend_from_slice(&[level(r), level(g), level(b), 255]);
                }
            }
        }
        let original = samples.clone();
        self.apply_rgba(&mut samples);
        samples
            .iter()
            .zip(&original)
            .all(|(a, b)| a.abs_diff(*b) <= 1)
    }

    /// Converts straight-alpha RGBA pixels in place (alpha is left as it is).
    pub fn apply_rgba(&self, pixels: &mut [u8]) {
        match &self.kind {
            Kind::Rgb { rgba, .. } => rgba.apply(pixels),
            Kind::Gray(table) => {
                for px in pixels.as_chunks_mut::<4>().0 {
                    let c = table[px[0] as usize];
                    px[..3].copy_from_slice(&c);
                }
            }
        }
    }

    /// Converts straight-alpha BGRA pixels in place (alpha is left as it is).
    pub fn apply_bgra(&self, pixels: &mut [u8]) {
        match &self.kind {
            Kind::Rgb { bgra, .. } => bgra.apply(pixels),
            Kind::Gray(table) => {
                for px in pixels.as_chunks_mut::<4>().0 {
                    let [r, g, b] = table[px[0] as usize];
                    px[..3].copy_from_slice(&[b, g, r]);
                }
            }
        }
    }
}

/// The colour space an ICC profile describes (its header's data colour space, ICC.1:2022 §7.2.6).
fn icc_color_space(icc: &[u8]) -> Option<[u8; 4]> {
    icc.get(16..20)?.try_into().ok()
}

/// The profile `info` describes, following the order and the rules a PNG decoder uses (the same
/// rules serve the other formats, which give only `icc`).
fn profile_of(info: &ColorInfo) -> Option<ProfileKind> {
    if let Some((primaries, transfer, matrix, full_range)) = info.cicp {
        // (RGB is the only colour model: matrix coefficients 0, and full range)
        if matrix == 0 && full_range {
            let p = Profile::new_cicp(primaries.into(), transfer.into());
            if let Some(p) = p {
                return Some(ProfileKind::Rgb(p));
            }
        }
    }
    if let Some(icc) = info.icc {
        let space = icc_color_space(icc);
        if matches!(&space, Some(s) if s == b"RGB " || s == b"GRAY") {
            if let Some(p) = Profile::new_from_slice(icc, false) {
                return Some(if space == Some(*b"GRAY") {
                    ProfileKind::Gray(p)
                } else {
                    ProfileKind::Rgb(p)
                });
            }
        }
        // (a profile that does not parse, or of another colour space, is ignored: what follows
        // still applies)
    }
    if info.srgb {
        return None;
    }
    // Gamma without chromaticities, and chromaticities without gamma: a gamma close to sRGB's
    // (within 5% of 1/2.2) alone is not a colour space; chromaticities alone cannot be applied
    // without guessing a gamma, so they are ignored.
    let gamma = info.gamma.filter(|g| *g > 0.0 && g.is_finite())?;
    let (white, primaries) = match info.chromaticities {
        Some([w, r, g, b]) => (
            qcms::CIE_xyY {
                x: w.0 as f64,
                y: w.1 as f64,
                Y: 1.0,
            },
            qcms::CIE_xyYTRIPLE {
                red: qcms::CIE_xyY {
                    x: r.0 as f64,
                    y: r.1 as f64,
                    Y: 1.0,
                },
                green: qcms::CIE_xyY {
                    x: g.0 as f64,
                    y: g.1 as f64,
                    Y: 1.0,
                },
                blue: qcms::CIE_xyY {
                    x: b.0 as f64,
                    y: b.1 as f64,
                    Y: 1.0,
                },
            },
        ),
        None => {
            let neutral = gamma * 2.2;
            if neutral > 0.95 && neutral < 1.05 {
                return None;
            }
            srgb_primaries()
        }
    };
    let exponent = 1.0 / gamma;
    Profile::new_rgb_with_gamma_set(white, primaries, exponent, exponent, exponent)
        .map(ProfileKind::Rgb)
}

/// sRGB's white point (D65) and primaries (ITU-R BT.709).
fn srgb_primaries() -> (qcms::CIE_xyY, qcms::CIE_xyYTRIPLE) {
    let xy = |x: f64, y: f64| qcms::CIE_xyY { x, y, Y: 1.0 };
    (
        xy(0.3127, 0.3290),
        qcms::CIE_xyYTRIPLE {
            red: xy(0.64, 0.33),
            green: xy(0.30, 0.60),
            blue: xy(0.15, 0.06),
        },
    )
}

enum ProfileKind {
    Rgb(Box<Profile>),
    Gray(Box<Profile>),
}

impl ColorTransform {
    fn from_kind(kind: &ProfileKind) -> Option<ColorTransform> {
        match kind {
            ProfileKind::Rgb(p) => ColorTransform::from_profile(p),
            ProfileKind::Gray(p) => ColorTransform::gray(p),
        }
    }
}

/// The conversion of an image's pixels to sRGB that `info` asks for; `None` when the image is to
/// be drawn as it is (sRGB, untagged, or a description that cannot be used).
pub fn transform_for(info: &ColorInfo) -> Option<ColorTransform> {
    ColorTransform::from_kind(&profile_of(info)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_and_untagged_images_are_left_alone() {
        assert!(transform_for(&ColorInfo::default()).is_none());
        let srgb = ColorInfo {
            srgb: true,
            gamma: Some(0.45455),
            ..ColorInfo::default()
        };
        assert!(transform_for(&srgb).is_none());
        // (cICP BT.709 primaries with the sRGB transfer)
        let cicp = ColorInfo {
            cicp: Some((1, 13, 0, true)),
            ..ColorInfo::default()
        };
        assert!(transform_for(&cicp).is_none());
        // a gamma near 1/2.2 alone is not a colour space; chromaticities alone are ignored
        let neutral = ColorInfo {
            gamma: Some(0.4545),
            ..ColorInfo::default()
        };
        assert!(transform_for(&neutral).is_none());
        let chrm_only = ColorInfo {
            chromaticities: Some([(0.3127, 0.329), (0.64, 0.33), (0.3, 0.6), (0.15, 0.06)]),
            ..ColorInfo::default()
        };
        assert!(transform_for(&chrm_only).is_none());
    }

    #[test]
    fn a_linear_gamma_brightens_the_midtones() {
        // gAMA 1.0: stored values are linear light; sRGB encodes 50% linear as about 188.
        let linear = ColorInfo {
            gamma: Some(1.0),
            ..ColorInfo::default()
        };
        let t = transform_for(&linear).expect("a conversion");
        let mut px = [128, 128, 128, 200];
        t.apply_rgba(&mut px);
        assert!((186..=190).contains(&px[0]), "{px:?}");
        assert_eq!(px[0], px[1]);
        assert_eq!(px[3], 200, "alpha is kept");
        // BGRA gives the same colours in its own order
        let mut b = [0, 128, 255, 255];
        let mut r = [255, 128, 0, 255];
        t.apply_bgra(&mut b);
        t.apply_rgba(&mut r);
        assert_eq!([b[2], b[1], b[0]], [r[0], r[1], r[2]]);
    }

    #[test]
    fn display_p3_red_is_out_of_srgb_gamut() {
        // cICP: SMPTE EG 432-1 (Display P3) primaries, sRGB transfer.
        let p3 = ColorInfo {
            cicp: Some((12, 13, 0, true)),
            ..ColorInfo::default()
        };
        let t = transform_for(&p3).expect("a conversion");
        let mut px = [255, 0, 0, 255, 0, 255, 0, 255];
        t.apply_rgba(&mut px);
        // P3's red is redder than sRGB's: clipped to sRGB red
        assert_eq!(px[0], 255);
        assert!(px[1] <= 2 && px[2] <= 2, "{px:?}");
        // a mid P3 colour moves: sRGB needs more saturation to show it
        let mut mid = [200, 100, 100, 255];
        t.apply_rgba(&mut mid);
        assert!(mid[0] > 200 && mid[1] < 100, "{mid:?}");
    }

    #[test]
    fn unusable_profiles_are_ignored() {
        let garbage = ColorInfo {
            icc: Some(b"not a profile at all, just bytes that are long enough to look"),
            ..ColorInfo::default()
        };
        assert!(transform_for(&garbage).is_none());
        // a broken profile falls through to what comes after it
        let fallback = ColorInfo {
            icc: Some(b"short"),
            gamma: Some(1.0),
            ..ColorInfo::default()
        };
        assert!(transform_for(&fallback).is_some());
    }
}
