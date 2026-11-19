// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Audio output through the Windows Audio Session API: an AudioContext's periods played on the
//! default output device ([`WasapiOutputs`], [`WasapiSink`]).
//!
//! The stream is shared mode and event driven, 32-bit float at the context's own rate and channel
//! count: the audio engine converts the rate and the channel layout to the device's mix format
//! (`AUTOCONVERTPCM`), so a context at a rate the device does not run at needs no resampler here.
//! The device's mix rate is the context's default rate, its default period the shortest period a
//! context asks for.

use std::time::{Duration, Instant};

use snowlark_audio::realtime::{AudioOutputs, AudioSink, DeviceInfo, SinkError, StreamConfig};
use windows::core::{w, GUID};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_NOPERSIST,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};

/// `WAVE_FORMAT_EXTENSIBLE` (mmreg.h).
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` (ksmedia.h).
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
/// The longest period a context asks for.
const MAX_PERIOD: f64 = 0.08;
/// How long the device may give no buffer space before the output counts as stopped.
const STALL: Duration = Duration::from_secs(2);

/// The default output device of this process's audio engine.
#[derive(Default)]
pub struct WasapiOutputs;

impl WasapiOutputs {
    pub fn new() -> WasapiOutputs {
        WasapiOutputs
    }
}

fn hr(what: &str) -> impl Fn(windows::core::Error) -> SinkError + '_ {
    move |e| SinkError(format!("{what}: {e}"))
}

/// The default render endpoint's audio client.
fn default_client() -> Result<IAudioClient, SinkError> {
    super::ensure_com();
    // SAFETY: COM is initialized on this thread; the enumerator and device are released by their
    // wrappers when they drop.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(hr("no enumerator"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(hr("no output device"))?;
        device
            .Activate::<IAudioClient>(CLSCTX_ALL, None)
            .map_err(hr("the device would not activate"))
    }
}

/// The device's mix format: (rate, channels, channel mask).
fn mix_format(client: &IAudioClient) -> Result<(u32, u16, u32), SinkError> {
    // SAFETY: GetMixFormat returns a CoTaskMemAlloc'd WAVEFORMATEX (a WAVEFORMATEXTENSIBLE when
    // its tag says so and cbSize covers the extension), read here unaligned and freed once.
    unsafe {
        let p = client.GetMixFormat().map_err(hr("no mix format"))?;
        let f = std::ptr::read_unaligned(p);
        let mask = if f.wFormatTag == WAVE_FORMAT_EXTENSIBLE && f.cbSize >= 22 {
            let x = std::ptr::read_unaligned(p as *const WAVEFORMATEXTENSIBLE);
            x.dwChannelMask
        } else {
            default_mask(f.nChannels)
        };
        CoTaskMemFree(Some(p as *const _));
        Ok((f.nSamplesPerSec, f.nChannels, mask))
    }
}

/// The usual speaker positions for `channels` (ksmedia.h's `KSAUDIO_SPEAKER_*`).
fn default_mask(channels: u16) -> u32 {
    match channels {
        1 => 0x4,
        2 => 0x3,
        4 => 0x33,
        6 => 0x3F,
        8 => 0x63F,
        _ => 0,
    }
}

impl AudioOutputs for WasapiOutputs {
    fn device(&self) -> Option<DeviceInfo> {
        let client = default_client().ok()?;
        let (rate, channels, _) = mix_format(&client).ok()?;
        let mut default = 0i64;
        // SAFETY: the out pointer is valid for the call.
        unsafe { client.GetDevicePeriod(Some(&mut default), None) }.ok()?;
        // (a shared-mode stream is fed once per engine period, the default one)
        let engine_period = default as f64 * 1e-7;
        Some(DeviceInfo {
            sample_rate: rate as f32,
            channels: channels as u32,
            min_period: engine_period,
            max_period: MAX_PERIOD.max(engine_period),
            // (the engine mixes a period ahead of the device)
            output_latency: engine_period,
        })
    }

    fn open(&self) -> Box<dyn AudioSink> {
        Box::new(WasapiSink::default())
    }
}

/// A running shared-mode stream.
struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    /// The buffer's frames, and seconds per frame.
    buffer: u32,
    frame: f64,
    /// The stream's latency in seconds.
    latency: f64,
    period: u32,
    channels: usize,
    /// The thread's "Pro Audio" scheduling class, while the stream runs.
    mmcss: Option<HANDLE>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the client and event belong to this stream; Stop on a stopped client is
        // harmless, the event is closed once, and the MMCSS handle is the one this thread got.
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
            if let Some(h) = self.mmcss {
                let _ = AvRevertMmThreadCharacteristics(h);
            }
        }
    }
}

/// An AudioContext's output on the default device.
#[derive(Default)]
pub struct WasapiSink {
    stream: Option<Stream>,
}

// SAFETY: the stream's COM objects are made in `start` and used only by the thread that drives the
// sink (an AudioContext's rendering thread, or a host's thread per stream), which also drops them;
// the WASAPI interfaces are free-threaded in the multithreaded apartment either way.
unsafe impl Send for WasapiSink {}

impl WasapiSink {
    fn open(config: &StreamConfig) -> Result<Stream, SinkError> {
        let client = default_client()?;
        let (_, mix_channels, mix_mask) = mix_format(&client)?;
        let channels = config.channels.clamp(1, 32) as u16;
        let mask = if channels == mix_channels {
            mix_mask
        } else {
            default_mask(channels)
        };
        let rate = config.sample_rate.round() as u32;
        let block = channels * 4;
        let format = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: channels,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * block as u32,
                nBlockAlign: block,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: mask,
            SubFormat: SUBTYPE_IEEE_FLOAT,
        };
        let period = config.period_frames as u32;
        // Room for three of the context's periods: one being played, one written, one to spare
        // against the engine's own period being shorter than ours.
        let seconds = 3.0 * period as f64 / rate as f64;
        let duration = (seconds * 1e7).round() as i64;
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_NOPERSIST
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        // SAFETY: the format outlives the call; the event handle is owned by the stream (closed in
        // its Drop, or here if a later step fails).
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    duration,
                    0,
                    &format as *const _ as *const WAVEFORMATEX,
                    None,
                )
                .map_err(hr("the stream would not open"))?;
            let event = CreateEventW(None, false, false, None).map_err(hr("no event"))?;
            let mut stream = Stream {
                render: match client.GetService::<IAudioRenderClient>() {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = CloseHandle(event);
                        return Err(hr("no render client")(e));
                    }
                },
                client,
                event,
                buffer: 0,
                frame: 1.0 / rate as f64,
                latency: 0.0,
                period,
                channels: channels as usize,
                mmcss: None,
            };
            stream
                .client
                .SetEventHandle(event)
                .map_err(hr("no event handle"))?;
            stream.buffer = stream
                .client
                .GetBufferSize()
                .map_err(hr("no buffer size"))?;
            if stream.buffer < period {
                return Err(SinkError(
                    "The device buffer is shorter than a period".into(),
                ));
            }
            stream.latency = stream.client.GetStreamLatency().unwrap_or(0) as f64 * 1e-7;
            let mut task = 0u32;
            stream.mmcss = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task).ok();
            stream
                .client
                .Start()
                .map_err(hr("the stream would not start"))?;
            Ok(stream)
        }
    }
}

impl AudioSink for WasapiSink {
    fn start(&mut self, config: &StreamConfig) -> Result<(), SinkError> {
        self.stream = None;
        self.stream = Some(WasapiSink::open(config)?);
        Ok(())
    }

    fn wait_period(&mut self) -> Result<Instant, SinkError> {
        let s = self
            .stream
            .as_ref()
            .ok_or_else(|| SinkError("The stream is not started".into()))?;
        let mut waited = Duration::ZERO;
        loop {
            // SAFETY: the client is running; GetCurrentPadding has no other precondition.
            let padding =
                unsafe { s.client.GetCurrentPadding() }.map_err(hr("the output went away"))?;
            if s.buffer - padding.min(s.buffer) >= s.period {
                let ahead = padding as f64 * s.frame + s.latency;
                return Ok(Instant::now() + Duration::from_secs_f64(ahead));
            }
            // SAFETY: the event is valid while the stream lives.
            let woke = unsafe { WaitForSingleObject(s.event, 200) };
            if woke != WAIT_OBJECT_0 {
                waited += Duration::from_millis(200);
                if waited >= STALL {
                    return Err(SinkError("The output stopped taking audio".into()));
                }
            }
        }
    }

    fn write(&mut self, channels: &[Vec<f32>]) -> Result<(), SinkError> {
        let s = self
            .stream
            .as_ref()
            .ok_or_else(|| SinkError("The stream is not started".into()))?;
        let frames = s.period as usize;
        // SAFETY: wait_period saw room for `frames`; GetBuffer hands out frames * channels f32
        // slots (the stream's block is channels * 4 bytes), written in full and released once.
        unsafe {
            let p = s.render.GetBuffer(frames as u32).map_err(hr("no buffer"))? as *mut f32;
            let out = std::slice::from_raw_parts_mut(p, frames * s.channels);
            for c in 0..s.channels {
                let src = channels.get(c).map(Vec::as_slice).unwrap_or(&[]);
                for f in 0..frames {
                    out[f * s.channels + c] = src.get(f).copied().unwrap_or(0.0);
                }
            }
            s.render
                .ReleaseBuffer(frames as u32, 0)
                .map_err(hr("the buffer would not release"))
        }
    }

    fn stop(&mut self) {
        self.stream = None;
    }
}
