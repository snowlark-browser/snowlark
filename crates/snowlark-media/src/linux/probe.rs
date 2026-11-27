// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What plays on this system: the containers and codecs the installed FFmpeg has a demuxer and a
//! decoder for (distributions build it with different codecs).

use crate::caps::{Codec, Container, Profile, Support};

use super::ffmpeg;

/// The demuxer each container needs.
const DEMUXERS: &[(Container, &str)] = &[
    (Container::Mp4, "mov"),
    (Container::WebM, "matroska"),
    (Container::Ogg, "ogg"),
    (Container::Mp3, "mp3"),
    (Container::Adts, "aac"),
    (Container::Wav, "wav"),
    (Container::Flac, "flac"),
];

/// The decoders that take each codec (any one is enough).
const DECODERS: &[(Codec, &[&str])] = &[
    (Codec::H264, &["h264"]),
    (Codec::Hevc, &["hevc"]),
    (Codec::Vp8, &["vp8", "libvpx"]),
    (Codec::Vp9, &["vp9", "libvpx-vp9"]),
    (Codec::Av1, &["libdav1d", "av1", "libaom-av1"]),
    (Codec::Theora, &["theora"]),
    (Codec::Aac, &["aac", "libfdk_aac"]),
    (Codec::Mp3, &["mp3float", "mp3"]),
    (Codec::Opus, &["opus", "libopus"]),
    (Codec::Vorbis, &["vorbis", "libvorbis"]),
    (Codec::Flac, &["flac"]),
    (Codec::Alac, &["alac"]),
    (Codec::Ac3, &["ac3"]),
    (Codec::Eac3, &["eac3"]),
    (Codec::Pcm, &["pcm_s16le"]),
];

/// What the system's FFmpeg plays (nothing without one).
pub fn probe() -> Support {
    let Some(ff) = ffmpeg::get() else {
        return Support::default();
    };
    let mut support = Support::default();
    for (container, name) in DEMUXERS {
        if ff.has_demuxer(name) {
            support.containers.insert(*container);
        }
    }
    for (codec, names) in DECODERS {
        if names.iter().any(|n| ff.has_decoder(n)) {
            support.codecs.insert(*codec);
        }
    }
    // (FFmpeg's software decoders take every VP9 profile and depth, and AV1's main, high and
    // professional profiles)
    for depth in [8, 10, 12] {
        if support.codecs.contains(&Codec::Vp9) {
            for profile in 0..4 {
                if profile >= 2 || depth == 8 {
                    support
                        .profiles
                        .insert(Profile::new(Codec::Vp9, profile, depth));
                }
            }
        }
        if support.codecs.contains(&Codec::Av1) {
            for profile in 0..3 {
                support
                    .profiles
                    .insert(Profile::new(Codec::Av1, profile, depth));
            }
        }
    }
    support
}
