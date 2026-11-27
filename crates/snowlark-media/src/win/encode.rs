// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Writes test clips with Media Foundation's sink writer: H.264 video of a generated pattern (and
//! an AAC tone) in MP4. The pattern tells the time: the background is [`pattern_color`] of the
//! second, and a white bar moves across with the frame number, so a test can check which frame
//! it is shown.

use std::path::Path;

use windows::core::HSTRING;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::*;

/// The background of second `s` of a test clip (BGR order: blue, green, red).
pub fn pattern_color(s: u32) -> (u8, u8, u8) {
    const COLORS: [(u8, u8, u8); 4] = [
        (0x20, 0x20, 0xE0),
        (0x20, 0xC0, 0x20),
        (0xE0, 0x40, 0x20),
        (0x20, 0xC0, 0xE0),
    ];
    COLORS[s as usize % COLORS.len()]
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Writes `seconds` of a `width` × `height` pattern at `fps` (with a 440 Hz tone when `audio`).
pub fn write_test_clip(
    path: &Path,
    width: u32,
    height: u32,
    seconds: u32,
    fps: u32,
    audio: bool,
) -> windows::core::Result<()> {
    write_clip(path, width, height, seconds * fps, fps, audio, false)
}

/// Writes rames frames of a width × height pattern at ps, without sound: clips shorter
/// than a second.
pub fn write_video_frames(
    path: &Path,
    width: u32,
    height: u32,
    frames: u32,
    fps: u32,
) -> windows::core::Result<()> {
    write_clip(path, width, height, frames, fps, false, false)
}

/// As [`write_test_clip`], as fragmented MP4 (an initialization segment, `ftyp` and `moov`, then
/// `moof`/`mdat` media segments): what Media Source Extensions take.
pub fn write_fragmented_test_clip(
    path: &Path,
    width: u32,
    height: u32,
    seconds: u32,
    fps: u32,
    audio: bool,
) -> windows::core::Result<()> {
    write_clip(path, width, height, seconds * fps, fps, audio, true)
}

fn write_clip(
    path: &Path,
    width: u32,
    height: u32,
    frames: u32,
    fps: u32,
    audio: bool,
    fragmented: bool,
) -> windows::core::Result<()> {
    super::ensure_com();
    let _ = std::fs::remove_file(path);
    // SAFETY: Media Foundation calls with valid arguments; each buffer is locked, written within
    // its length and unlocked before it is handed over.
    unsafe {
        let mut attributes = None;
        if fragmented {
            MFCreateAttributes(&mut attributes, 1)?;
            if let Some(a) = &attributes {
                a.SetGUID(
                    &MF_TRANSCODE_CONTAINERTYPE,
                    &MFTranscodeContainerType_FMPEG4,
                )?;
            }
        }
        let writer =
            MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, attributes.as_ref())?;
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, 400_000)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        let video = writer.AddStream(&out)?;
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)?;
        writer.SetInputMediaType(video, &input, None)?;
        let rate = 48_000u32;
        let sound = if audio {
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000)?;
            let stream = writer.AddStream(&out)?;
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            writer.SetInputMediaType(stream, &input, None)?;
            Some(stream)
        } else {
            None
        };
        writer.BeginWriting()?;
        let frame_len = 10_000_000i64 / fps as i64;
        let size = width * height * 4;
        for n in 0..frames {
            let (b, g, r) = pattern_color(n / fps);
            let bar = (n % fps) * width / fps;
            let buffer = MFCreateMemoryBuffer(size)?;
            let mut p: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut p, None, None)?;
            let px = std::slice::from_raw_parts_mut(p, size as usize);
            for y in 0..height {
                for x in 0..width {
                    let i = ((y * width + x) * 4) as usize;
                    let on_bar =
                        x >= bar && x < bar + width / 16 && y > height / 3 && y < height * 2 / 3;
                    let c = if on_bar { (255, 255, 255) } else { (b, g, r) };
                    px[i] = c.0;
                    px[i + 1] = c.1;
                    px[i + 2] = c.2;
                    px[i + 3] = 255;
                }
            }
            buffer.Unlock()?;
            buffer.SetCurrentLength(size)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(n as i64 * frame_len)?;
            sample.SetSampleDuration(frame_len)?;
            writer.WriteSample(video, &sample)?;
        }
        if let Some(stream) = sound {
            // One-second buffers of a 440 Hz sine, stereo.
            for s in 0..frames / fps {
                let frames = rate as usize;
                let bytes = (frames * 4) as u32;
                let buffer = MFCreateMemoryBuffer(bytes)?;
                let mut p: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut p, None, None)?;
                let out = std::slice::from_raw_parts_mut(p as *mut i16, frames * 2);
                for i in 0..frames {
                    let t = (s as usize * frames + i) as f64 / rate as f64;
                    let v = ((t * 440.0 * std::f64::consts::TAU).sin() * 6000.0) as i16;
                    out[2 * i] = v;
                    out[2 * i + 1] = v;
                }
                buffer.Unlock()?;
                buffer.SetCurrentLength(bytes)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(s as i64 * 10_000_000)?;
                sample.SetSampleDuration(10_000_000)?;
                writer.WriteSample(stream, &sample)?;
            }
        }
        writer.Finalize()?;
    }
    if fragmented {
        let bytes =
            std::fs::read(path).map_err(|e| windows::core::Error::new(E_FAIL, e.to_string()))?;
        let mse = mse_fragments(&bytes)
            .ok_or_else(|| windows::core::Error::new(E_FAIL, "unexpected fragments"))?;
        std::fs::write(path, mse).map_err(|e| windows::core::Error::new(E_FAIL, e.to_string()))?;
    }
    Ok(())
}

/// Media Foundation's fragmented MP4 gives each track fragment an absolute base data offset and no
/// decode time; MSE's ISO BMFF byte stream format wants neither the first (a `tfhd` with
/// base-data-offset-present is an append error) and requires a `tfdt` in every `traf`. Rewrites each
/// `moof` so: `default-base-is-moof`, the `trun` data offset from the `moof`, and a `tfdt` with the
/// sum of the durations before it. Only the one-track layout the sink writer makes.
fn mse_fragments(file: &[u8]) -> Option<Vec<u8>> {
    let be32 = |b: &[u8], at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
    };
    let boxed = |kind: &[u8; 4], body: &[u8]| -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    };
    let mut out = Vec::with_capacity(file.len() + 1024);
    let mut decode_time = 0u64;
    let mut at = 0;
    while at < file.len() {
        let size = be32(file, at)? as usize;
        let kind = file.get(at + 4..at + 8)?;
        let whole = file.get(at..at + size)?;
        at += size;
        if kind != b"moof" {
            out.extend_from_slice(whole);
            continue;
        }
        let (mut mfhd, mut traf) = (None, None);
        let mut i = 8;
        while i < whole.len() {
            let s = be32(whole, i)? as usize;
            match whole.get(i + 4..i + 8)? {
                b"mfhd" => mfhd = Some(whole.get(i..i + s)?),
                b"traf" => traf = Some(whole.get(i + 8..i + s)?),
                _ => return None,
            }
            i += s;
        }
        let (mfhd, traf) = (mfhd?, traf?);
        let (mut tfhd, mut trun) = (None, None);
        let mut i = 0;
        while i < traf.len() {
            let s = be32(traf, i)? as usize;
            match traf.get(i + 4..i + 8)? {
                b"tfhd" => tfhd = Some(traf.get(i + 8..i + s)?),
                b"trun" => trun = Some(traf.get(i + 8..i + s)?.to_vec()),
                _ => return None,
            }
            i += s;
        }
        let (tfhd, mut trun) = (tfhd?, trun?);
        // tfhd: version and flags, track id, then the optional fields in flag order.
        let flags = be32(tfhd, 0)? & 0xFF_FFFF;
        let mut new_tfhd = ((flags & !1) | 0x02_0000).to_be_bytes().to_vec();
        new_tfhd.extend_from_slice(tfhd.get(4..8)?);
        new_tfhd.extend_from_slice(tfhd.get(if flags & 1 != 0 { 16 } else { 8 }..)?);
        // trun: the samples' durations (each sample's, as the sink writer writes them).
        let tflags = be32(&trun, 0)? & 0xFF_FFFF;
        if tflags & 0x101 != 0x101 {
            return None;
        }
        let count = be32(&trun, 4)? as usize;
        let mut first = 12;
        if tflags & 4 != 0 {
            first += 4;
        }
        let per = [0x100, 0x200, 0x400, 0x800]
            .iter()
            .filter(|&&f| tflags & f != 0)
            .count()
            * 4;
        let mut duration = 0u64;
        for n in 0..count {
            duration += be32(&trun, first + n * per)? as u64;
        }
        let mut tfdt = vec![1, 0, 0, 0];
        tfdt.extend_from_slice(&decode_time.to_be_bytes());
        decode_time += duration;
        // The data offset, from the start of the moof to the first byte of the mdat's data.
        let len = |trun: &[u8]| {
            8 + mfhd.len() + 8 + (8 + new_tfhd.len()) + (8 + tfdt.len()) + (8 + trun.len())
        };
        let offset = (len(&trun) + 8) as u32;
        trun[8..12].copy_from_slice(&offset.to_be_bytes());
        let traf = [
            boxed(b"tfhd", &new_tfhd),
            boxed(b"tfdt", &tfdt),
            boxed(b"trun", &trun),
        ]
        .concat();
        let mut moof = mfhd.to_vec();
        moof.extend(boxed(b"traf", &traf));
        out.extend(boxed(b"moof", &moof));
        // The mdat must follow its moof.
        if file.get(at + 4..at + 8)? != b"mdat" {
            return None;
        }
    }
    Some(out)
}
