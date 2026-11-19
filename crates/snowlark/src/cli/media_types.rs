// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --media-types`: what this system plays, from the Media Foundation sources and decoders
//! installed (what `canPlayType()` and `MediaSource.isTypeSupported()` answer from).

use snowlark_media::caps::{Codec, Container};

pub fn run() -> i32 {
    let s = snowlark_media::support();
    println!("Containers:");
    for c in Container::ALL {
        let yes = s.containers.contains(&c);
        println!(
            "  {:<5} {:<4} {}",
            c.name(),
            if yes { "yes" } else { "no" },
            c.mime_types().join(", ")
        );
    }
    println!("Codecs:");
    for c in Codec::ALL {
        println!(
            "  {:<7} {}",
            format!("{c:?}"),
            if s.codecs.contains(&c) { "yes" } else { "no" }
        );
    }
    println!("Examples (canPlayType / MediaSource.isTypeSupported):");
    for t in [
        "video/mp4; codecs=\"avc1.42E01E, mp4a.40.2\"",
        "video/mp4; codecs=\"hev1.1.6.L93.B0\"",
        "video/mp4; codecs=\"av01.0.05M.08\"",
        "video/webm; codecs=\"vp9\"",
        "video/webm; codecs=\"vp09.00.10.08\"",
        "video/webm; codecs=\"vp8, vorbis\"",
        "audio/webm; codecs=\"opus\"",
        "audio/mp4; codecs=\"mp4a.40.2\"",
        "audio/mpeg",
        "audio/ogg; codecs=\"vorbis\"",
        "audio/flac",
        "audio/wav",
    ] {
        let mse = s.mse_supports(t);
        println!(
            "  {:<45} {:<9} {}",
            t,
            match snowlark_media::can_play_type(t).as_str() {
                "" => "\"\"",
                v => v,
            },
            if mse { "MSE" } else { "-" }
        );
    }
    0
}
