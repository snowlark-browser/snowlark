// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Linux media: the system's FFmpeg for demuxing and software decoding ([`ffmpeg`]), sound through
//! the desktop's sound server ([`pulse`]), and the [`player`] that joins them for one media element.

pub mod decode;
pub mod ffmpeg;
mod ffmpeg_layout;
pub mod player;
pub mod probe;
pub mod pulse;
