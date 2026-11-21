// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which media types the installed decoders play: the answer behind `canPlayType()` (HTML
//! §4.8.11.3) and `MediaSource.isTypeSupported()` (MSE §3.4), from a [`Support`] table that the
//! platform fills from what is installed ([`crate::probe`]).
//!
//! A type names a container (the MIME type's essence) and, in its `codecs` parameter (RFC 6381),
//! the codecs in it. A type is playable when its container has a source and every codec it lists
//! has a decoder and belongs in that container.

use std::collections::BTreeSet;

use snowlark_net::mime::MimeType;

/// A container format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Container {
    /// ISO BMFF (`video/mp4`, `audio/mp4`, fragmented MP4 for MSE).
    Mp4,
    WebM,
    Ogg,
    /// MPEG audio (`audio/mpeg`).
    Mp3,
    /// ADTS AAC (`audio/aac`).
    Adts,
    Wav,
    Flac,
}

/// A codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Vp8,
    Vp9,
    Av1,
    Theora,
    Aac,
    Mp3,
    Opus,
    Vorbis,
    Flac,
    Alac,
    Ac3,
    Eac3,
    Pcm,
}

impl Codec {
    pub fn is_video(self) -> bool {
        matches!(
            self,
            Codec::H264 | Codec::Hevc | Codec::Vp8 | Codec::Vp9 | Codec::Av1 | Codec::Theora
        )
    }

    pub const ALL: [Codec; 15] = [
        Codec::H264,
        Codec::Hevc,
        Codec::Vp8,
        Codec::Vp9,
        Codec::Av1,
        Codec::Theora,
        Codec::Aac,
        Codec::Mp3,
        Codec::Opus,
        Codec::Vorbis,
        Codec::Flac,
        Codec::Alac,
        Codec::Ac3,
        Codec::Eac3,
        Codec::Pcm,
    ];
}

impl Container {
    pub const ALL: [Container; 7] = [
        Container::Mp4,
        Container::WebM,
        Container::Ogg,
        Container::Mp3,
        Container::Adts,
        Container::Wav,
        Container::Flac,
    ];

    /// The codecs that can be in this container.
    pub fn allows(self, codec: Codec) -> bool {
        use Codec::*;
        match self {
            Container::Mp4 => matches!(
                codec,
                H264 | Hevc | Vp9 | Av1 | Aac | Mp3 | Opus | Flac | Alac | Ac3 | Eac3
            ),
            Container::WebM => matches!(codec, Vp8 | Vp9 | Av1 | Opus | Vorbis),
            Container::Ogg => matches!(codec, Theora | Vorbis | Opus | Flac),
            Container::Mp3 => codec == Mp3,
            Container::Adts => codec == Aac,
            Container::Wav => codec == Pcm,
            Container::Flac => codec == Flac,
        }
    }

    /// What a type without a `codecs` parameter implies is in it (none: unknown).
    fn implied(self) -> Option<Codec> {
        match self {
            Container::Mp3 => Some(Codec::Mp3),
            Container::Adts => Some(Codec::Aac),
            Container::Flac => Some(Codec::Flac),
            Container::Wav => Some(Codec::Pcm),
            _ => None,
        }
    }

    /// Whether MSE byte streams exist for it (the MSE byte stream format registry: ISO BMFF, WebM,
    /// MPEG audio, ADTS).
    pub fn mse(self) -> bool {
        matches!(
            self,
            Container::Mp4 | Container::WebM | Container::Mp3 | Container::Adts
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            Container::Mp4 => "MP4",
            Container::WebM => "WebM",
            Container::Ogg => "Ogg",
            Container::Mp3 => "MP3",
            Container::Adts => "ADTS",
            Container::Wav => "WAV",
            Container::Flac => "FLAC",
        }
    }

    /// The MIME types naming this container.
    pub fn mime_types(self) -> &'static [&'static str] {
        match self {
            Container::Mp4 => &[
                "video/mp4",
                "audio/mp4",
                "video/x-m4v",
                "audio/x-m4a",
                "video/quicktime",
            ],
            Container::WebM => &["video/webm", "audio/webm"],
            Container::Ogg => &["video/ogg", "audio/ogg", "application/ogg"],
            Container::Mp3 => &["audio/mpeg", "audio/mp3"],
            Container::Adts => &["audio/aac", "audio/aacp"],
            Container::Wav => &["audio/wav", "audio/wave", "audio/x-wav", "audio/vnd.wave"],
            Container::Flac => &["audio/flac", "audio/x-flac"],
        }
    }
}

/// The container a MIME type essence names.
pub fn container_of(essence: &str) -> Option<Container> {
    Container::ALL
        .into_iter()
        .find(|c| c.mime_types().contains(&essence))
}

/// The codec an RFC 6381 codec string names, if it is one Snowlark knows and its parameters are
/// valid. `None` both for unknown codecs and for malformed parameters (both are unplayable).
pub fn codec_of(s: &str) -> Option<Codec> {
    let s = s.trim();
    let (fourcc, rest) = match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    Some(match fourcc {
        "avc1" | "avc3" => {
            // avc1.PPCCLL (hex profile, constraints, level), or none.
            if let Some(r) = rest {
                if r.len() != 6 || !r.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
            }
            Codec::H264
        }
        "hev1" | "hvc1" => {
            hevc_profile(rest?)?;
            Codec::Hevc
        }
        "vp8" => {
            if rest.is_some_and(|r| r != "0") {
                return None;
            }
            Codec::Vp8
        }
        "vp9" => {
            if rest.is_some_and(|r| r != "0") {
                return None;
            }
            Codec::Vp9
        }
        "vp09" => {
            if !valid_vp09(rest?) {
                return None;
            }
            Codec::Vp9
        }
        "av01" => {
            if !valid_av01(rest?) {
                return None;
            }
            Codec::Av1
        }
        "mp4a" => match rest? {
            "40.2" | "40.02" | "40.5" | "40.05" | "40.29" | "40.1" | "40.01" | "40.42" | "67"
            | "66" | "68" | "40" => Codec::Aac,
            "69" | "6B" | "6b" | "40.34" => Codec::Mp3,
            "a5" | "A5" => Codec::Ac3,
            "a6" | "A6" => Codec::Eac3,
            _ => return None,
        },
        "mp3" if rest.is_none() => Codec::Mp3,
        "opus" | "Opus" if rest.is_none() => Codec::Opus,
        "vorbis" if rest.is_none() => Codec::Vorbis,
        "flac" | "fLaC" if rest.is_none() => Codec::Flac,
        "alac" if rest.is_none() => Codec::Alac,
        "theora" if rest.is_none() => Codec::Theora,
        "ac-3" | "ec-3" if rest.is_none() => {
            if fourcc == "ac-3" {
                Codec::Ac3
            } else {
                Codec::Eac3
            }
        }
        "1" if rest.is_none() => Codec::Pcm,
        _ => return None,
    })
}

fn two_digits(s: &str) -> Option<u32> {
    (s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// VP9's codec string (the VP9 ISO BMFF binding §"Codecs Parameter String"):
/// `PP.LL.DD[.CC.cp.tc.mc.FF]`.
fn valid_vp09(rest: &str) -> bool {
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() < 3 || parts.len() > 8 {
        return false;
    }
    let Some(v): Option<Vec<u32>> = parts.iter().map(|p| two_digits(p)).collect() else {
        return false;
    };
    let level_ok = [10, 11, 20, 21, 30, 31, 40, 41, 50, 51, 52, 60, 61, 62].contains(&v[1]);
    if v[0] > 3 || !level_ok || ![8, 10, 12].contains(&v[2]) {
        return false;
    }
    if v[0] < 2 && v[2] != 8 {
        return false;
    }
    let ok = |i: usize, f: &dyn Fn(u32) -> bool| v.get(i).is_none_or(|x| f(*x));
    ok(3, &|c| c <= 3)
        && ok(4, &|p| matches!(p, 1 | 2 | 4..=12 | 22))
        && ok(5, &|t| matches!(t, 1 | 2 | 4..=18))
        && ok(6, &|m| m <= 14 && m != 3)
        && ok(7, &|f| f <= 1)
}

/// AV1's codec string (AV1 ISO BMFF binding §5): `P.LLT.DD[.M.CCC.cp.tc.mc.F]`.
fn valid_av01(rest: &str) -> bool {
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() < 3 {
        return false;
    }
    let profile_ok = matches!(parts[0], "0" | "1" | "2");
    let lt = parts[1];
    let level_ok = lt.len() == 3
        && two_digits(&lt[..2]).is_some_and(|l| l <= 31)
        && matches!(&lt[2..], "M" | "H");
    let depth_ok = matches!(parts[2], "08" | "10" | "12");
    profile_ok && level_ok && depth_ok
}

/// The HEVC profile an `hev1`/`hvc1` codec string names (ISO/IEC 14496-15 Annex E.3, after the
/// four-character code): `[A|B|C]idc.flags.(L|H)level[.constraint]*`, with the profile space, the
/// decimal `general_profile_idc`, the 32 compatibility flags in hex (flag j in bit j), the tier and
/// decimal level, and up to six constraint bytes in hex. The profile is `general_profile_idc`, or
/// where that is 0 the first of Main, Main 10 and Main Still Picture the flags claim; `None` for a
/// malformed string or one that names no profile.
fn hevc_profile(rest: &str) -> Option<u8> {
    let parts: Vec<&str> = rest.split('.').collect();
    if !(3..=9).contains(&parts.len()) {
        return None;
    }
    let idc = parts[0].trim_start_matches(['A', 'B', 'C']);
    if idc.len() + 1 < parts[0].len() || idc.is_empty() || !idc.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let idc: u8 = idc.parse().ok().filter(|i| *i <= 31)?;
    let flags = parts[1];
    if flags.is_empty() || flags.len() > 8 || !flags.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let flags = u32::from_str_radix(flags, 16).ok()?;
    let tier_level = parts[2];
    let level = tier_level
        .strip_prefix('L')
        .or_else(|| tier_level.strip_prefix('H'))?;
    if level.is_empty()
        || !level.bytes().all(|b| b.is_ascii_digit())
        || level.parse::<u16>().ok()? > 255
    {
        return None;
    }
    if !parts[3..]
        .iter()
        .all(|c| !c.is_empty() && c.len() <= 2 && c.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    if idc != 0 {
        return Some(idc);
    }
    [1, 2, 3].into_iter().find(|j| flags & (1 << j) != 0)
}

/// A VP9, AV1 or HEVC profile at a bit depth: what decides whether a decoder takes a stream of a
/// codec it has (the level does not). HEVC's profile is its `general_profile_idc` (1 Main, 2 Main
/// 10, 3 Main Still Picture, 4 and up the range and other extensions), its depth the profile's
/// highest (8, 10 for Main 10, 0 for the extensions, which no table here lists).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Profile {
    pub codec: Codec,
    pub profile: u8,
    pub depth: u8,
}

impl Profile {
    pub const fn new(codec: Codec, profile: u8, depth: u8) -> Profile {
        Profile {
            codec,
            profile,
            depth,
        }
    }
}

/// xHE-AAC (MPEG-D USAC, audio object type 42 of ISO/IEC 14496-3 §1.5.1.1, `mp4a.40.42` in RFC
/// 6381): an AAC "profile" that only some AAC decoders take.
pub const XHE_AAC: Profile = Profile::new(Codec::Aac, 42, 0);

/// The profile and bit depth a valid VP9, AV1 or HEVC codec string names (`vp9` is profile 0 at 8
/// bits), or [`XHE_AAC`]; `None` for other codecs.
pub fn profile_of(s: &str) -> Option<Profile> {
    codec_of(s)?;
    let parts: Vec<&str> = s.trim().split('.').collect();
    let number = |p: &str| p.parse::<u8>().ok();
    match parts.as_slice() {
        ["vp9"] | ["vp9", "0"] => Some(Profile::new(Codec::Vp9, 0, 8)),
        ["vp09", profile, _level, depth, ..] => {
            Some(Profile::new(Codec::Vp9, number(profile)?, number(depth)?))
        }
        ["av01", profile, _level_tier, depth, ..] => {
            Some(Profile::new(Codec::Av1, number(profile)?, number(depth)?))
        }
        ["hev1" | "hvc1", ..] => {
            let profile = hevc_profile(s.trim().split_once('.')?.1)?;
            let depth = match profile {
                1 | 3 => 8,
                2 => 10,
                _ => 0,
            };
            Some(Profile::new(Codec::Hevc, profile, depth))
        }
        ["mp4a", "40", "42"] => Some(XHE_AAC),
        _ => None,
    }
}

/// What the platform plays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Support {
    pub containers: BTreeSet<Container>,
    pub codecs: BTreeSet<Codec>,
    /// Video codecs the GPU decodes.
    pub hardware: BTreeSet<Codec>,
    /// The VP9, AV1 and HEVC profiles and depths a decoder takes, and [`XHE_AAC`] where the AAC
    /// decoder takes it (a stream of one not listed does not play, though its codec is in
    /// `codecs`).
    pub profiles: BTreeSet<Profile>,
}

/// `canPlayType()`'s answer (HTML §4.8.11.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanPlay {
    No,
    Maybe,
    Probably,
}

impl CanPlay {
    pub fn as_str(self) -> &'static str {
        match self {
            CanPlay::No => "",
            CanPlay::Maybe => "maybe",
            CanPlay::Probably => "probably",
        }
    }
}

/// A media type split for the checks: the container and the listed codecs (`None` for a codec
/// string Snowlark can't use).
pub struct ParsedType {
    pub container: Option<Container>,
    pub essence: String,
    pub codecs: Option<Vec<Option<Codec>>>,
    /// The profile each listed codec names (`None` for codecs without one), in the same order.
    pub profiles: Vec<Option<Profile>>,
}

pub fn parse_type(type_: &str) -> Option<ParsedType> {
    let mime = MimeType::parse(type_)?;
    let essence = mime.essence();
    let names: Option<Vec<&str>> = mime.parameter("codecs").map(|c| {
        c.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    });
    Some(ParsedType {
        container: container_of(&essence),
        essence,
        codecs: names
            .as_ref()
            .map(|n| n.iter().map(|s| codec_of(s)).collect()),
        profiles: names
            .unwrap_or_default()
            .into_iter()
            .map(profile_of)
            .collect(),
    })
}

/// The MIME types of the MSE byte stream formats (the MSE Byte Stream Format Registry: ISO BMFF,
/// WebM, MPEG audio).
const MSE_TYPES: [&str; 6] = [
    "video/mp4",
    "audio/mp4",
    "video/webm",
    "audio/webm",
    "audio/mpeg",
    "audio/aac",
];

impl Container {
    /// The codecs the platform's MSE byte stream parser takes in this container (Media Foundation's
    /// `MseStreamSource`: Opus and ALAC only outside MP4), as a browser's MSE lists its own parsers'
    /// codecs per container.
    ///
    /// AC-3 and E-AC-3 are left out, though their decoder plays them in a file and as a
    /// `SourceBuffer`'s first initialization segment: MSE `changeType()` lets a page switch a
    /// `SourceBuffer` to any type `isTypeSupported()` takes, and the platform's source fails a
    /// switch to another decoder once a track is set up ("the media format is recognized but not
    /// supported"). Adaptive players offer Dolby audio as the high rung of the same audio stream as
    /// AAC and switch to it mid-playback, so claiming it turns a playable stream into a decode
    /// error; without it they stay on AAC, as on browsers that do not offer Dolby.
    fn mse_allows(self, codec: Codec) -> bool {
        self.mse()
            && self.allows(codec)
            && !(self == Container::Mp4 && matches!(codec, Codec::Opus | Codec::Alac))
            && !matches!(codec, Codec::Ac3 | Codec::Eac3)
    }
}

impl Support {
    /// Everything (for tests).
    pub fn everything() -> Support {
        let mut profiles = BTreeSet::new();
        for depth in [8, 10, 12] {
            for profile in 0..4 {
                if profile >= 2 || depth == 8 {
                    profiles.insert(Profile::new(Codec::Vp9, profile, depth));
                }
            }
            for profile in 0..3 {
                profiles.insert(Profile::new(Codec::Av1, profile, depth));
            }
        }
        profiles.insert(XHE_AAC);
        Support {
            containers: Container::ALL.into_iter().collect(),
            codecs: Codec::ALL.into_iter().collect(),
            hardware: Codec::ALL.into_iter().filter(|c| c.is_video()).collect(),
            profiles,
        }
    }

    /// Whether the listed codecs all decode here (with their profiles) and `allows` takes each.
    fn codecs_ok(&self, t: &ParsedType, allows: impl Fn(Codec) -> bool) -> bool {
        let Some(codecs) = &t.codecs else {
            return true;
        };
        codecs.iter().zip(&t.profiles).all(|(c, p)| {
            c.is_some_and(|c| self.codecs.contains(&c) && allows(c))
                && p.is_none_or(|p| self.profiles.contains(&p))
        })
    }

    /// HTML §4.8.11.3 `canPlayType(type)`: "" when the type surely can't play (an unknown or
    /// unsupported container, an unsupported codec, `application/octet-stream`), "probably" when
    /// it lists codecs that all play, "maybe" for a playable container without codecs.
    pub fn can_play_type(&self, type_: &str) -> CanPlay {
        let Some(t) = parse_type(type_) else {
            return CanPlay::No;
        };
        let Some(container) = t.container.filter(|c| self.containers.contains(c)) else {
            return CanPlay::No;
        };
        // An audio/* type can't hold video.
        let audio_only = t.essence.starts_with("audio/");
        match &t.codecs {
            Some(codecs) if !codecs.is_empty() => {
                if !self.codecs_ok(&t, |c| container.allows(c))
                    || (audio_only && codecs.iter().flatten().any(|c| c.is_video()))
                {
                    CanPlay::No
                } else {
                    CanPlay::Probably
                }
            }
            Some(_) => CanPlay::No,
            None => match container.implied() {
                Some(c) if !self.codecs.contains(&c) => CanPlay::No,
                _ => CanPlay::Maybe,
            },
        }
    }

    /// MSE §3.4 `isTypeSupported(type)`: a valid MIME type of an MSE byte stream format whose
    /// container plays here, and every listed codec one the platform's MSE parser takes in it and a
    /// decoder takes at its profile and depth. Decided from the type and its `codecs` parameter
    /// alone, other parameters ignored, each codec on its own, VP9 and AV1 by profile and depth and
    /// never their level; and from what was found once at startup (no decoder is brought up per
    /// question).
    pub fn mse_supports(&self, type_: &str) -> bool {
        let Some(t) = parse_type(type_) else {
            return false;
        };
        if !MSE_TYPES.contains(&t.essence.as_str()) {
            return false;
        }
        let Some(container) = t
            .container
            .filter(|c| c.mse() && self.containers.contains(c))
        else {
            return false;
        };
        let audio_only = t.essence.starts_with("audio/");
        match &t.codecs {
            Some(codecs) => {
                !codecs.is_empty()
                    && self.codecs_ok(&t, |c| container.mse_allows(c))
                    && !(audio_only && codecs.iter().flatten().any(|c| c.is_video()))
            }
            // (the platform's WebM parser needs the codecs named)
            None if container == Container::WebM => false,
            None => container.implied().is_none_or(|c| self.codecs.contains(&c)),
        }
    }
}

/// Media Capabilities "check MIME type validity" for a configuration's `contentType`, with the
/// media type the configuration is for (`video` or `audio`), which `application/` may stand for: a
/// type that implies its codec has no parameters; any other names exactly one codec in a single
/// `codecs` parameter, which for an `application/` type is a known codec of that media type. For
/// `webrtc`, only the media type is checked.
pub fn capabilities_type_valid(content_type: &str, video: bool, webrtc: bool) -> bool {
    let Some(mime) = MimeType::parse(content_type) else {
        return false;
    };
    let kind = if video { "video" } else { "audio" };
    if mime.type_ != kind && (webrtc || mime.type_ != "application") {
        return false;
    }
    if webrtc {
        return true;
    }
    if container_of(&mime.essence()).is_some_and(|c| c.implied().is_some()) {
        return mime.parameters.is_empty();
    }
    let [(name, value)] = mime.parameters.as_slice() else {
        return false;
    };
    let mut codecs = value.split(',').map(str::trim).filter(|c| !c.is_empty());
    let (Some(codec), None) = (codecs.next(), codecs.next()) else {
        return false;
    };
    // (an `application/` type says what it holds through its codec alone)
    name == "codecs"
        && (mime.type_ != "application" || codec_of(codec).is_some_and(|c| c.is_video() == video))
}

impl Support {
    /// Media Capabilities: whether a valid `contentType` decodes here from a file: the same answer
    /// as `canPlayType()` ("probably", or a playable type that implies its codec).
    pub fn decodes_file(&self, content_type: &str) -> bool {
        match self.can_play_type(content_type) {
            CanPlay::Probably => true,
            CanPlay::Maybe => parse_type(content_type)
                .and_then(|t| t.container)
                .is_some_and(|c| c.implied().is_some()),
            CanPlay::No => false,
        }
    }

    /// Whether the GPU decodes the video codec a valid `contentType` names.
    pub fn decodes_in_hardware(&self, content_type: &str) -> bool {
        parse_type(content_type)
            .and_then(|t| t.codecs)
            .is_some_and(|c| {
                c.iter()
                    .all(|c| c.is_some_and(|c| self.hardware.contains(&c)))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h264_aac() -> Support {
        Support {
            containers: [Container::Mp4, Container::Mp3, Container::Wav]
                .into_iter()
                .collect(),
            codecs: [Codec::H264, Codec::Aac, Codec::Mp3, Codec::Pcm]
                .into_iter()
                .collect(),
            hardware: [Codec::H264].into_iter().collect(),
            profiles: BTreeSet::new(),
        }
    }

    #[test]
    fn codec_strings() {
        assert_eq!(codec_of("avc1.42E01E"), Some(Codec::H264));
        assert_eq!(codec_of("avc1.4d401f"), Some(Codec::H264));
        assert_eq!(codec_of("avc1.42E0"), None);
        assert_eq!(codec_of("mp4a.40.2"), Some(Codec::Aac));
        assert_eq!(codec_of("mp4a.69"), Some(Codec::Mp3));
        assert_eq!(codec_of("vp09.00.10.08"), Some(Codec::Vp9));
        assert_eq!(codec_of("vp09.02.51.10.01.09.16.09.00"), Some(Codec::Vp9));
        // (YouTube's probe for a decoder that checks the color values)
        assert_eq!(codec_of("vp09.02.51.10.01.09.99.99.00"), None);
        assert_eq!(codec_of("av01.0.05M.08"), Some(Codec::Av1));
        assert_eq!(codec_of("av99.0.05M.08"), None);
        assert_eq!(codec_of("av01.0.05X.08"), None);
        assert_eq!(codec_of("opus"), Some(Codec::Opus));
        assert_eq!(codec_of("bogus"), None);
    }

    #[test]
    fn can_play_type_answers() {
        let s = h264_aac();
        assert_eq!(s.can_play_type("video/mp4"), CanPlay::Maybe);
        assert_eq!(
            s.can_play_type("video/mp4; codecs=\"avc1.42E01E, mp4a.40.2\""),
            CanPlay::Probably
        );
        assert_eq!(
            s.can_play_type("video/mp4; codecs=\"avc1.42E01E, bogus\""),
            CanPlay::No
        );
        assert_eq!(s.can_play_type("video/webm"), CanPlay::No);
        assert_eq!(s.can_play_type("application/octet-stream"), CanPlay::No);
        assert_eq!(s.can_play_type("video/x-new-fictional-format"), CanPlay::No);
        assert_eq!(s.can_play_type("audio/mpeg"), CanPlay::Maybe);
        assert_eq!(
            s.can_play_type("audio/mp4; codecs=\"avc1.42E01E\""),
            CanPlay::No
        );
        assert_eq!(s.can_play_type("VIDEO/MP4"), CanPlay::Maybe);
        assert_eq!(s.can_play_type(""), CanPlay::No);
        assert_eq!(s.can_play_type("audio/flac"), CanPlay::No);
    }

    #[test]
    fn mse_types() {
        let s = Support::everything();
        assert!(s.mse_supports("video/mp4; codecs=\"avc1.4d401f\""));
        assert!(s.mse_supports("video/webm; codecs=\"vp9\""));
        assert!(s.mse_supports("video/mp4"));
        assert!(!s.mse_supports("video/ogg; codecs=\"theora\""));
        assert!(!s.mse_supports("audio/wav"));
        assert!(!s.mse_supports(""));
        assert!(!s.mse_supports("video/mp4; codecs=\"\""));
        assert!(!s.mse_supports("video/webm; codecs=\"avc1.4d401f\""));
    }

    /// What a GPU with VP9 profiles 0 and 2 (10 bits) and AV1 profile 0 decodes, with the
    /// platform's software decoders.
    fn gpu_vp9_av1() -> Support {
        Support {
            containers: [
                Container::Mp4,
                Container::WebM,
                Container::Mp3,
                Container::Adts,
            ]
            .into_iter()
            .collect(),
            codecs: [
                Codec::H264,
                Codec::Vp8,
                Codec::Vp9,
                Codec::Av1,
                Codec::Aac,
                Codec::Mp3,
                Codec::Opus,
                Codec::Vorbis,
                Codec::Flac,
                Codec::Alac,
            ]
            .into_iter()
            .collect(),
            hardware: [Codec::H264, Codec::Vp9, Codec::Av1].into_iter().collect(),
            profiles: [
                Profile::new(Codec::Vp9, 0, 8),
                Profile::new(Codec::Vp9, 2, 10),
                Profile::new(Codec::Av1, 0, 8),
                Profile::new(Codec::Av1, 0, 10),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn profiles_of_codec_strings() {
        assert_eq!(profile_of("vp9"), Some(Profile::new(Codec::Vp9, 0, 8)));
        assert_eq!(profile_of("vp9.0"), Some(Profile::new(Codec::Vp9, 0, 8)));
        assert_eq!(
            profile_of("vp09.02.51.10.01.09.16.09.00"),
            Some(Profile::new(Codec::Vp9, 2, 10))
        );
        assert_eq!(
            profile_of("av01.0.05M.10"),
            Some(Profile::new(Codec::Av1, 0, 10))
        );
        assert_eq!(profile_of("avc1.42E01E"), None);
        // (not a valid VP9 string: profile 0 is 8 bits)
        assert_eq!(profile_of("vp09.00.51.10"), None);
    }

    /// HEVC's codec string names its profile (ISO/IEC 14496-15 E.3), and a type plays only at a
    /// profile the GPU decodes: an HEVC decoder installed without the GPU's profile claims nothing.
    #[test]
    fn hevc_by_codec_string_and_gpu_profile() {
        assert_eq!(codec_of("hvc1.1.6.L93.B0"), Some(Codec::Hevc));
        assert_eq!(codec_of("hev1.A1.60000000.L93.B0"), Some(Codec::Hevc));
        for bad in [
            "hvc1",
            "hev1",
            "hvc1.1.6",
            "hvc1.x.6.L93",
            "hvc1.32.6.L93",
            "hvc1.AB1.6.L93",
            "hvc1.1.g.L93",
            "hvc1.1.6.X93",
            "hvc1.1.6.L256",
            "hvc1.1.6.L93.B00",
            "hvc1.1.6.L93.B0.0.0.0.0.0.0",
            "hvc1.0.0.L93",
        ] {
            assert_eq!(codec_of(bad), None, "{bad}");
        }
        assert_eq!(
            profile_of("hvc1.1.6.L93.B0"),
            Some(Profile::new(Codec::Hevc, 1, 8))
        );
        assert_eq!(
            profile_of("hev1.2.4.L153.90"),
            Some(Profile::new(Codec::Hevc, 2, 10))
        );
        // (an idc of 0 takes the profile from the compatibility flags)
        assert_eq!(
            profile_of("hvc1.0.4.L93"),
            Some(Profile::new(Codec::Hevc, 2, 10))
        );
        assert_eq!(
            profile_of("hvc1.4.10.L93.B0"),
            Some(Profile::new(Codec::Hevc, 4, 0))
        );

        let mut s = gpu_vp9_av1();
        s.codecs.insert(Codec::Hevc);
        let main = "video/mp4; codecs=\"hvc1.1.6.L93.B0\"";
        let main10 = "video/mp4; codecs=\"hvc1.2.4.L123.B0\"";
        let rext = "video/mp4; codecs=\"hvc1.4.10.L93.B0\"";
        // a decoder, no GPU profile: nothing
        for t in [main, main10, rext] {
            assert!(!s.mse_supports(t), "{t}");
            assert_eq!(s.can_play_type(t), CanPlay::No, "{t}");
        }
        s.profiles.insert(Profile::new(Codec::Hevc, 1, 8));
        assert!(s.mse_supports(main));
        assert_eq!(s.can_play_type(main), CanPlay::Probably);
        assert!(!s.mse_supports(main10));
        s.profiles.insert(Profile::new(Codec::Hevc, 2, 10));
        assert!(s.mse_supports(main10));
        assert!(!s.mse_supports(rext));
        assert!(!s.mse_supports("video/mp4; codecs=\"hvc1\""));
        // a GPU profile without the decoder: nothing
        s.codecs.remove(&Codec::Hevc);
        assert!(!s.mse_supports(main));
    }

    /// xHE-AAC (`mp4a.40.42`) plays only where the AAC decoder takes it: an AAC decoder alone
    /// answers the other AAC object types, not this one.
    #[test]
    fn xhe_aac_only_where_the_decoder_takes_it() {
        assert_eq!(profile_of("mp4a.40.42"), Some(XHE_AAC));
        assert_eq!(profile_of("mp4a.40.2"), None);
        assert_eq!(profile_of("mp4a.40.5"), None);
        let mut s = Support::everything();
        s.profiles.remove(&XHE_AAC);
        let xhe = "audio/mp4; codecs=\"mp4a.40.42\"";
        let both = "video/mp4; codecs=\"avc1.42E01E, mp4a.40.42\"";
        for t in [xhe, both] {
            assert_eq!(s.can_play_type(t), CanPlay::No, "{t}");
            assert!(!s.mse_supports(t), "{t}");
            assert!(!s.decodes_file(t), "{t}");
        }
        // (other AAC still plays)
        assert!(s.mse_supports("audio/mp4; codecs=\"mp4a.40.2\""));
        assert!(s.mse_supports("audio/mp4; codecs=\"mp4a.40.29\""));
        s.profiles.insert(XHE_AAC);
        for t in [xhe, both] {
            assert_eq!(s.can_play_type(t), CanPlay::Probably, "{t}");
            assert!(s.mse_supports(t), "{t}");
        }
    }

    /// Dolby audio plays from a file, but MSE does not offer it: a `SourceBuffer` could not be
    /// switched to it with `changeType()`.
    #[test]
    fn dolby_audio_in_files_not_through_mse() {
        let s = Support::everything();
        for t in [
            "audio/mp4; codecs=\"ac-3\"",
            "audio/mp4; codecs=\"ec-3\"",
            "audio/mp4; codecs=\"mp4a.a5\"",
            "audio/mp4; codecs=\"mp4a.a6\"",
            "video/mp4; codecs=\"avc1.42E01E, ec-3\"",
        ] {
            assert_eq!(s.can_play_type(t), CanPlay::Probably, "{t}");
            assert!(s.decodes_file(t), "{t}");
            assert!(!s.mse_supports(t), "{t}");
        }
        assert!(s.mse_supports("audio/mp4; codecs=\"mp4a.40.5\""));
    }

    /// `isTypeSupported()` answers from the type's essence and codecs only, each codec at its
    /// profile and depth, as the platform's MSE parser and decoders take them (measured against
    /// Media Foundation's own answers: `win::probe::tests::mse_answers_match_media_foundation`).
    #[test]
    fn mse_answers_by_parser_decoder_and_profile() {
        let s = gpu_vp9_av1();
        // (parameters other than `codecs` do not count: YouTube's probes)
        for t in [
            "video/webm; codecs=\"vp9\"",
            "video/webm; codecs=\"vp9\"; height=99999",
            "video/webm; codecs=\"vp9\"; eotf=catavision",
            "video/webm; codecs=\"vp09.00.51.08.01.01.01.01.00\"",
            "video/webm; codecs=\"vp09.02.51.10.01.09.16.09.00\"",
            "video/mp4; codecs=\"vp09.02.10.10\"",
            "video/mp4; codecs=\"av01.0.05M.10\"",
            "video/webm; codecs=\"av01.0.05M.08\"",
            "video/mp4; codecs=\"avc1.42E01E, mp4a.40.2\"; eotf=x",
            "video/mp4",
            "audio/mp4; codecs=\"flac\"",
            "audio/webm; codecs=\"opus\"",
            "audio/mpeg",
            "audio/aac",
        ] {
            assert!(s.mse_supports(t), "{t}");
        }
        for t in [
            // profiles and depths no decoder here takes
            "video/webm; codecs=\"vp09.01.10.08\"",
            "video/webm; codecs=\"vp09.02.10.12\"",
            "video/webm; codecs=\"vp09.03.10.10\"",
            "video/mp4; codecs=\"av01.1.05M.08\"",
            "video/mp4; codecs=\"av01.0.05M.12\"",
            // codecs the platform's MP4 parser does not take
            "audio/mp4; codecs=\"opus\"",
            "audio/mp4; codecs=\"alac\"",
            // types outside the MSE byte stream formats, or WebM without its codecs
            "audio/mp3",
            "audio/aacp",
            "video/webm",
            "audio/webm",
        ] {
            assert!(!s.mse_supports(t), "{t}");
        }
        // canPlayType() takes profiles into account the same way
        assert_eq!(
            s.can_play_type("video/webm; codecs=\"vp09.02.10.10\""),
            CanPlay::Probably
        );
        assert_eq!(
            s.can_play_type("video/webm; codecs=\"vp09.02.10.12\""),
            CanPlay::No
        );
    }
}
