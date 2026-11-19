// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `decodeAudioData()`'s decoding (§1.1.3): on a thread of its own, the encoded bytes become one
//! linear PCM float channel per channel, resampled to the context's rate. WAVE files (integer and
//! float PCM) are read here; any other format goes to the decoder the caller supplies (the media
//! stack's).

use std::sync::{Arc, Mutex};
use std::thread;

use crate::resample::{resample, resampled_len};

/// Decoded audio: the file's sample rate and its channels.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub sample_rate: f32,
    pub channels: Vec<Vec<f32>>,
}

/// A decoder for formats this crate does not read: the sample rate and the channels.
pub type Fallback = fn(&[u8]) -> Option<(f32, Vec<Vec<f32>>)>;

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// A RIFF WAVE file with PCM (8, 16, 24 or 32 bits) or IEEE float (32 or 64 bits) samples.
pub fn wav(bytes: &[u8]) -> Option<Decoded> {
    if bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut at = 12;
    let mut fmt: Option<(u16, usize, u32, usize)> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(bytes, at + 4)? as usize;
        let body = at + 8;
        if id == b"fmt " {
            let mut format = u16_at(bytes, body)?;
            let channels = u16_at(bytes, body + 2)? as usize;
            let rate = u32_at(bytes, body + 4)?;
            let bits = u16_at(bytes, body + 14)? as usize;
            if format == 0xFFFE {
                // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID starts with the format tag.
                format = u16_at(bytes, body + 24)?;
            }
            fmt = Some((format, channels, rate, bits));
        } else if id == b"data" {
            let (format, channels, rate, bits) = fmt?;
            if channels == 0 || rate == 0 {
                return None;
            }
            let end = (body + size).min(bytes.len());
            let data = &bytes[body..end];
            let width = bits / 8;
            if width == 0 {
                return None;
            }
            let frames = data.len() / (width * channels);
            let read: fn(&[u8]) -> f32 = match (format, bits) {
                (1, 8) => |s| (s[0] as f32 - 128.0) / 128.0,
                (1, 16) => |s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
                (1, 24) => |s| (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8388608.0,
                (1, 32) => |s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2147483648.0,
                (3, 32) => |s| f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
                (3, 64) => |s| f64::from_le_bytes(s[..8].try_into().unwrap_or([0; 8])) as f32,
                _ => return None,
            };
            let mut out = vec![Vec::with_capacity(frames); channels];
            for frame in data.chunks_exact(width * channels).take(frames) {
                for (c, s) in frame.chunks_exact(width).enumerate() {
                    out[c].push(read(s));
                }
            }
            return Some(Decoded {
                sample_rate: rate as f32,
                channels: out,
            });
        }
        at = body + size + (size & 1);
    }
    None
}

/// Decodes `bytes` and resamples to `rate`. `None`: not audio this engine can decode.
pub fn decode(bytes: &[u8], rate: f32, fallback: Option<Fallback>) -> Option<Decoded> {
    let d = wav(bytes).or_else(|| {
        let (sample_rate, channels) = fallback?(bytes)?;
        Some(Decoded {
            sample_rate,
            channels,
        })
    })?;
    if d.channels.is_empty() || d.channels[0].is_empty() {
        return None;
    }
    // An AudioBuffer's length is a 32-bit count of frames.
    if resampled_len(d.channels[0].len(), d.sample_rate as f64, rate as f64) > u32::MAX as usize {
        return None;
    }
    if d.sample_rate == rate {
        return Some(d);
    }
    Some(Decoded {
        sample_rate: rate,
        channels: d
            .channels
            .iter()
            .map(|c| resample(c, d.sample_rate as f64, rate as f64))
            .collect(),
    })
}

/// A decoding under way.
#[derive(Clone, Debug, Default)]
pub struct DecodeJob(Arc<Mutex<Option<Option<Decoded>>>>);

impl DecodeJob {
    /// Starts decoding on a thread of its own; `wake` runs once the result is in.
    pub fn spawn(
        bytes: Vec<u8>,
        rate: f32,
        fallback: Option<Fallback>,
        wake: Box<dyn Fn() + Send>,
    ) -> DecodeJob {
        let job = DecodeJob::default();
        let slot = job.0.clone();
        let spawned = thread::Builder::new()
            .name("audio decode".into())
            .spawn(move || {
                let r = decode(&bytes, rate, fallback);
                if let Ok(mut s) = slot.lock() {
                    *s = Some(r);
                }
                wake();
            });
        if spawned.is_err() {
            if let Ok(mut s) = job.0.lock() {
                *s = Some(None);
            }
        }
        job
    }

    /// The result once decoding has finished: `Some(None)` when the data could not be decoded.
    pub fn take(&self) -> Option<Option<Decoded>> {
        self.0.lock().ok()?.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_file(format: u16, bits: u16, channels: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&format.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * (bits / 8 * channels) as u32).to_le_bytes());
        b.extend_from_slice(&(bits / 8 * channels).to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(b"LIST\x03\0\0\0abc\0");
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn pcm_and_float_wave_files_decode_to_their_samples() {
        let d = wav(&wav_file(
            1,
            16,
            2,
            8000,
            &[0, 0x40, 0, 0xC0, 0xFF, 0x7F, 0, 0x80],
        ))
        .unwrap();
        assert_eq!(d.sample_rate, 8000.0);
        assert_eq!(
            d.channels,
            vec![vec![0.5, 32767.0 / 32768.0], vec![-0.5, -1.0]]
        );
        let d = wav(&wav_file(1, 24, 1, 8000, &[0, 0, 0x40])).unwrap();
        assert_eq!(d.channels, vec![vec![0.5]]);
        let d = wav(&wav_file(3, 32, 1, 8000, &0.25f32.to_le_bytes())).unwrap();
        assert_eq!(d.channels, vec![vec![0.25]]);
        assert!(wav(b"RIFF\0\0\0\0WAVEjunk").is_none());
        assert!(decode(b"not audio", 8000.0, None).is_none());
    }
}
