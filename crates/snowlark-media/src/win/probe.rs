// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What this system plays: containers with a Media Foundation byte stream handler registered for
//! their MIME type, and codecs with a decoder (an MFT for the format, hardware or software).

use std::collections::BTreeSet;

use windows::core::{Interface, GUID, HSTRING};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
};

use crate::caps::{Codec, Container, Profile, Support};

const HANDLERS: &str = r"SOFTWARE\Microsoft\Windows Media Foundation\ByteStreamHandlers";

fn handler_registered(name: &str) -> bool {
    let path = HSTRING::from(format!(r"{HANDLERS}\{name}"));
    let mut key = HKEY::default();
    // SAFETY: opening a registry key for reading into a local handle, closed right after.
    unsafe {
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, &path, Some(0), KEY_READ, &mut key).is_ok() {
            let _ = RegCloseKey(key);
            return true;
        }
    }
    false
}

/// The registry names (a MIME type or extension) whose handler plays a container.
fn handler_names(c: Container) -> &'static [&'static str] {
    match c {
        Container::Mp4 => &["video/mp4", ".mp4"],
        Container::WebM => &["video/webm", ".webm"],
        Container::Ogg => &["audio/ogg", ".ogg"],
        Container::Mp3 => &["audio/mpeg", ".mp3"],
        Container::Adts => &["audio/aac", ".aac"],
        Container::Wav => &["audio/wav", ".wav"],
        Container::Flac => &["audio/flac", ".flac"],
    }
}

fn subtype(c: Codec) -> Option<(GUID, GUID, GUID)> {
    let v = (MFT_CATEGORY_VIDEO_DECODER, MFMediaType_Video);
    let a = (MFT_CATEGORY_AUDIO_DECODER, MFMediaType_Audio);
    Some(match c {
        Codec::H264 => (v.0, v.1, MFVideoFormat_H264),
        Codec::Hevc => (v.0, v.1, MFVideoFormat_HEVC),
        Codec::Vp8 => (v.0, v.1, MFVideoFormat_VP80),
        Codec::Vp9 => (v.0, v.1, MFVideoFormat_VP90),
        Codec::Av1 => (v.0, v.1, MFVideoFormat_AV1),
        Codec::Aac => (a.0, a.1, MFAudioFormat_AAC),
        Codec::Mp3 => (a.0, a.1, MFAudioFormat_MP3),
        Codec::Opus => (a.0, a.1, MFAudioFormat_Opus),
        Codec::Vorbis => (a.0, a.1, MFAudioFormat_Vorbis),
        Codec::Flac => (a.0, a.1, MFAudioFormat_FLAC),
        Codec::Alac => (a.0, a.1, MFAudioFormat_ALAC),
        Codec::Ac3 => (a.0, a.1, MFAudioFormat_Dolby_AC3),
        Codec::Eac3 => (a.0, a.1, MFAudioFormat_Dolby_DDPlus),
        Codec::Pcm | Codec::Theora => return None,
    })
}

/// Whether a decoder MFT for the codec is installed.
fn decoder_installed(c: Codec) -> bool {
    if c == Codec::Pcm {
        // (PCM needs no decoder)
        return true;
    }
    let Some((category, major, sub)) = subtype(c) else {
        return false;
    };
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: major,
        guidSubtype: sub,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx fills an array of `count` activation objects allocated with
    // CoTaskMemAlloc; each is released (dropped) and the array freed here.
    unsafe {
        if MFTEnumEx(
            category,
            MFT_ENUM_FLAG_ALL,
            Some(&input),
            None,
            &mut activates,
            &mut count,
        )
        .is_err()
        {
            return false;
        }
        for i in 0..count as usize {
            std::ptr::drop_in_place(activates.add(i));
        }
        if !activates.is_null() {
            CoTaskMemFree(Some(activates as *const _));
        }
    }
    count > 0
}

/// Asks the system what it plays: the sources and decoders installed and the GPU's decoder
/// profiles, once (most of it is creating a Direct3D device, about 180 ms). No decoder is brought
/// up: what VP9 and AV1 profiles play comes from the decoder profiles the GPU lists.
pub fn probe() -> Support {
    super::ensure_com();
    let containers = Container::ALL
        .into_iter()
        .filter(|c| handler_names(*c).iter().any(|n| handler_registered(n)))
        .collect();
    let codecs: BTreeSet<Codec> = Codec::ALL
        .into_iter()
        .filter(|c| decoder_installed(*c))
        .collect();
    let gpu = decoder_profiles();
    let mut hardware = BTreeSet::new();
    let mut profiles = BTreeSet::new();
    for guid in &gpu {
        if let Some(codec) = hardware_codec(guid) {
            hardware.insert(codec);
        }
        profiles.extend(hardware_profiles(guid).iter().copied());
    }
    // Every VP9 decoder takes profile 0 at 8 bits, and every AV1 decoder its Main profile at 8 and
    // 10 bits (the profiles a decoder must support); others only where the GPU lists them.
    if codecs.contains(&Codec::Vp9) {
        profiles.insert(Profile::new(Codec::Vp9, 0, 8));
    }
    if codecs.contains(&Codec::Av1) {
        profiles.insert(Profile::new(Codec::Av1, 0, 8));
        profiles.insert(Profile::new(Codec::Av1, 0, 10));
    }
    // (a GPU profile is of no use without the codec's decoder, which drives it)
    profiles.retain(|p| codecs.contains(&p.codec));
    Support {
        containers,
        codecs,
        hardware,
        profiles,
    }
}

/// The codec a GPU decoder profile decodes, of those the playback uses the GPU for.
fn hardware_codec(profile: &GUID) -> Option<Codec> {
    Some(match *profile {
        D3D11_DECODER_PROFILE_H264_VLD_NOFGT => Codec::H264,
        D3D11_DECODER_PROFILE_HEVC_VLD_MAIN => Codec::Hevc,
        D3D11_DECODER_PROFILE_VP8_VLD => Codec::Vp8,
        D3D11_DECODER_PROFILE_VP9_VLD_PROFILE0 => Codec::Vp9,
        D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0 => Codec::Av1,
        _ => return None,
    })
}

/// The VP9, AV1 and HEVC profiles and depths a GPU decoder profile takes (DXVA's VP9, AV1 and HEVC
/// decoding specifications: AV1's profile 0 is Main, 1 High, 2 Professional). HEVC plays only
/// through these: a stream of a profile the GPU does not list is not claimed, whatever decoder is
/// installed.
fn hardware_profiles(profile: &GUID) -> &'static [Profile] {
    const VP9_0: [Profile; 1] = [Profile::new(Codec::Vp9, 0, 8)];
    const VP9_2: [Profile; 1] = [Profile::new(Codec::Vp9, 2, 10)];
    const AV1_0: [Profile; 2] = [
        Profile::new(Codec::Av1, 0, 8),
        Profile::new(Codec::Av1, 0, 10),
    ];
    const AV1_1: [Profile; 2] = [
        Profile::new(Codec::Av1, 1, 8),
        Profile::new(Codec::Av1, 1, 10),
    ];
    const AV1_2: [Profile; 2] = [
        Profile::new(Codec::Av1, 2, 8),
        Profile::new(Codec::Av1, 2, 10),
    ];
    const AV1_2_12: [Profile; 1] = [Profile::new(Codec::Av1, 2, 12)];
    // (HEVC's Main decoder takes Main Still Picture streams, which are Main's intra pictures)
    const HEVC_MAIN: [Profile; 2] = [
        Profile::new(Codec::Hevc, 1, 8),
        Profile::new(Codec::Hevc, 3, 8),
    ];
    const HEVC_MAIN10: [Profile; 1] = [Profile::new(Codec::Hevc, 2, 10)];
    match *profile {
        D3D11_DECODER_PROFILE_HEVC_VLD_MAIN => &HEVC_MAIN,
        D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10 => &HEVC_MAIN10,
        D3D11_DECODER_PROFILE_VP9_VLD_PROFILE0 => &VP9_0,
        D3D11_DECODER_PROFILE_VP9_VLD_10BIT_PROFILE2 => &VP9_2,
        D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0 => &AV1_0,
        D3D11_DECODER_PROFILE_AV1_VLD_PROFILE1 => &AV1_1,
        D3D11_DECODER_PROFILE_AV1_VLD_PROFILE2 => &AV1_2,
        D3D11_DECODER_PROFILE_AV1_VLD_12BIT_PROFILE2
        | D3D11_DECODER_PROFILE_AV1_VLD_12BIT_PROFILE2_420 => &AV1_2_12,
        _ => &[],
    }
}

/// The default adapter's video decoder profiles (none without a hardware device).
fn decoder_profiles() -> Vec<GUID> {
    let mut out = Vec::new();
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: creates a hardware device with video support into a local; on failure nothing is
    // written and none is used.
    let made = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    };
    let Some(video) = made
        .ok()
        .and(device)
        .and_then(|d| d.cast::<ID3D11VideoDevice>().ok())
    else {
        return out;
    };
    // SAFETY: the video device is alive; indices are below the count it reports.
    let count = unsafe { video.GetVideoDecoderProfileCount() };
    for i in 0..count {
        // SAFETY: as above.
        if let Ok(profile) = unsafe { video.GetVideoDecoderProfile(i) } {
            out.push(profile);
        }
    }
    out
}

/// MSE: whether Media Foundation's Media Source Extensions take this type (its own check, which
/// knows the byte stream parsers; slow, so callers cache it).
pub fn mse_type_supported(type_: &str) -> bool {
    super::ensure_com();
    windows::Media::Core::MseStreamSource::IsContentTypeSupported(&HSTRING::from(type_))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_system_plays_h264_in_mp4() {
        let s = probe();
        // Windows ships the MP4 source and the H.264 and AAC decoders.
        assert!(s.containers.contains(&Container::Mp4), "{s:?}");
        assert!(s.codecs.contains(&Codec::H264), "{s:?}");
        assert!(s.codecs.contains(&Codec::Aac), "{s:?}");
        assert!(mse_type_supported("video/mp4; codecs=\"avc1.42E01E\""));
    }

    /// `isTypeSupported()`'s table against Media Foundation's own MSE check, which it replaces (that
    /// check brings up a decoder per question, about 190 ms for each VP9 one, and takes VP9 of
    /// every profile): the table never takes a type the platform's parser refuses, and refuses
    /// only profiles and depths no decoder here lists. Ignored: it brings up decoders, which other
    /// tests' players then share the GPU with. Run it after a change to the table or on another
    /// Windows version (`--ignored --nocapture` prints every answer).
    #[test]
    #[ignore]
    fn mse_answers_match_media_foundation() {
        let s = probe();
        eprintln!("{s:?}");
        let codecs = [
            "avc1.42E01E",
            "avc1.64001F",
            "hev1.1.6.L93.B0",
            "hvc1.1.6.L93.B0",
            "vp8",
            "vp9",
            "vp09.00.10.08",
            "vp09.00.51.08",
            "vp09.01.10.08",
            "vp09.02.10.10",
            "vp09.02.10.12",
            "vp09.03.10.10",
            "av01.0.05M.08",
            "av01.0.05M.10",
            "av01.0.05M.12",
            "av01.1.05M.08",
            "av01.1.05M.10",
            "av01.2.05M.08",
            "av01.2.05M.10",
            "av01.2.05M.12",
            "mp4a.40.2",
            "mp4a.40.5",
            "mp4a.69",
            "mp4a.6B",
            "mp4a.a5",
            "mp4a.a6",
            "ac-3",
            "ec-3",
            "opus",
            "vorbis",
            "flac",
            "alac",
            "mp3",
            "theora",
        ];
        let mut types: Vec<String> = Vec::new();
        for c in ["video/mp4", "audio/mp4", "video/webm", "audio/webm"] {
            for k in codecs {
                types.push(format!("{c}; codecs=\"{k}\""));
            }
            types.push(c.to_owned());
        }
        types.extend(["audio/mpeg", "audio/aac", "audio/mp3", "audio/aacp"].map(String::from));
        let mut wrong = Vec::new();
        for t in &types {
            let start = std::time::Instant::now();
            let mf = mse_type_supported(t);
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let table = s.mse_supports(t);
            eprintln!("{mf:<6} {table:<6} {ms:>7.1} ms  {t}");
            // (where the table refuses what the platform takes, it is for a profile and depth)
            let unlisted_profile = crate::caps::parse_type(t)
                .is_some_and(|p| p.profiles.iter().flatten().any(|p| !s.profiles.contains(p)));
            // (and Dolby audio, which the platform's source cannot switch to: `caps::Container::mse_allows`)
            let dolby = t.contains("ac-3") || t.contains("ec-3") || t.contains("mp4a.a");
            if table != mf && !(mf && (unlisted_profile || dolby)) {
                wrong.push(format!("{t}: Media Foundation {mf}, table {table}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
