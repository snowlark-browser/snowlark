// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decoding a whole encoded audio file in memory to float PCM with Media Foundation's source
//! reader (`decodeAudioData()` for the formats the page crates do not read themselves).

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSourceReader, MFAudioFormat_Float, MFCreateMFByteStreamOnStream,
    MFCreateMediaType, MFCreateSourceReaderFromByteStream, MFMediaType_Audio,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM,
};
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::STREAM_SEEK_SET;

use super::ensure_com;

/// The longest decoded audio accepted, in samples over all channels (about 2 GiB of floats).
const MAX_SAMPLES: usize = 1 << 29;

/// Decodes `bytes`: the sample rate and the channels, or `None` when Media Foundation cannot.
pub fn decode_audio(bytes: &[u8]) -> Option<(f32, Vec<Vec<f32>>)> {
    if bytes.is_empty() {
        return None;
    }
    ensure_com();
    let reader = open(bytes)?;
    read_all(&reader)
}

fn open(bytes: &[u8]) -> Option<IMFSourceReader> {
    // SAFETY: CreateStreamOnHGlobal with a null HGLOBAL allocates its own memory; the stream is
    // written through its own Write and rewound before use. The byte stream and the reader hold
    // their own references to the stream.
    unsafe {
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true).ok()?;
        let mut written = 0u32;
        stream
            .Write(
                bytes.as_ptr().cast(),
                u32::try_from(bytes.len()).ok()?,
                Some(&mut written),
            )
            .ok()
            .ok()?;
        if written as usize != bytes.len() {
            return None;
        }
        stream.Seek(0, STREAM_SEEK_SET, None).ok()?;
        let byte_stream = MFCreateMFByteStreamOnStream(&stream).ok()?;
        let reader = MFCreateSourceReaderFromByteStream(&byte_stream, None).ok()?;
        let all = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
        let first = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        reader.SetStreamSelection(all, false).ok()?;
        reader.SetStreamSelection(first, true).ok()?;
        let wanted: IMFMediaType = MFCreateMediaType().ok()?;
        wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).ok()?;
        wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float).ok()?;
        reader.SetCurrentMediaType(first, None, &wanted).ok()?;
        Some(reader)
    }
}

fn read_all(reader: &IMFSourceReader) -> Option<(f32, Vec<Vec<f32>>)> {
    let first = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
    // SAFETY: the reader is a valid source reader; each sample's buffer is locked only while its
    // bytes are copied, and unlocked after.
    unsafe {
        let ty = reader.GetCurrentMediaType(first).ok()?;
        let channels = ty.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).ok()? as usize;
        let rate = ty.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).ok()?;
        if channels == 0 || rate == 0 {
            return None;
        }
        let mut interleaved: Vec<f32> = Vec::new();
        loop {
            let mut flags = 0u32;
            let mut sample = None;
            reader
                .ReadSample(first, 0, None, Some(&mut flags), None, Some(&mut sample))
                .ok()?;
            if let Some(sample) = sample {
                let buffer = sample.ConvertToContiguousBuffer().ok()?;
                let mut data = std::ptr::null_mut();
                let mut len = 0u32;
                buffer.Lock(&mut data, None, Some(&mut len)).ok()?;
                let bytes = std::slice::from_raw_parts(data, len as usize);
                interleaved.extend(
                    bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b)),
                );
                let _ = buffer.Unlock();
                if interleaved.len() > MAX_SAMPLES {
                    return None;
                }
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
        }
        let frames = interleaved.len() / channels;
        if frames == 0 {
            return None;
        }
        let mut out = vec![Vec::with_capacity(frames); channels];
        for frame in interleaved.chunks_exact(channels) {
            for (c, v) in frame.iter().enumerate() {
                out[c].push(*v);
            }
        }
        Some((rate as f32, out))
    }
}
