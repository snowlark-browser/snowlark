// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Whole encoded audio files decoded to float PCM (`decodeAudioData()`), through FFmpeg.

use std::sync::Arc;

use super::ffmpeg::{self, Decoded, Demuxer, Frame, Kind, Memory, Resampler};

/// The file's first audio stream: its sample rate and one vector per channel.
pub fn decode_audio(bytes: &[u8]) -> Option<(f32, Vec<Vec<f32>>)> {
    let ff = ffmpeg::get()?;
    let mut demuxer = Demuxer::open(ff, Box::new(Memory(Arc::from(bytes)))).ok()?;
    let stream = demuxer
        .streams()
        .iter()
        .find(|s| s.kind == Kind::Audio)?
        .clone();
    let (rate, channels) = (stream.sample_rate, stream.channels.max(1));
    if rate == 0 {
        return None;
    }
    let mut decoder = demuxer.decoder(stream.index).ok()?;
    let mut resampler = Resampler::new(ff, rate, channels, true);
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); channels as usize];
    let mut frame = Frame::new(ff);
    let mut take = |decoder: &mut ffmpeg::Decoder, out: &mut Vec<Vec<f32>>| -> Option<bool> {
        loop {
            match decoder.receive(&mut frame).ok()? {
                Decoded::Frame => {
                    for (o, plane) in out.iter_mut().zip(resampler.convert(&frame)?) {
                        o.extend(plane);
                    }
                }
                Decoded::Again => return Some(false),
                Decoded::End => return Some(true),
            }
        }
    };
    while let Ok(Some(pkt)) = demuxer.read() {
        if pkt.stream() == stream.index {
            decoder.send(Some(&pkt)).ok()?;
            take(&mut decoder, &mut out)?;
        }
    }
    decoder.send(None).ok()?;
    take(&mut decoder, &mut out)?;
    (!out[0].is_empty()).then_some((rate as f32, out))
}
