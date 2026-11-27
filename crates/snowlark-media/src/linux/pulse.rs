// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sound output through PulseAudio's simple API (`libpulse-simple`, loaded at run time): what
//! PulseAudio and PipeWire desktops (through `pipewire-pulse`) both serve. A blocking write paces
//! the writer at the output's rate. Without a server, there is no output device.

use std::ffi::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::ptr::null;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use libloading::Library;
use snowlark_audio::realtime::{AudioOutputs, AudioSink, DeviceInfo, SinkError, StreamConfig};

type P = *mut c_void;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

const PLAYBACK: c_int = 1;
const FLOAT32LE: c_int = 5;

struct Pulse {
    new: unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        c_int,
        *const c_char,
        *const c_char,
        *const SampleSpec,
        *const c_void,
        *const BufferAttr,
        *mut c_int,
    ) -> P,
    write: unsafe extern "C" fn(P, *const c_void, usize, *mut c_int) -> c_int,
    latency: unsafe extern "C" fn(P, *mut c_int) -> u64,
    flush: unsafe extern "C" fn(P, *mut c_int) -> c_int,
    free: unsafe extern "C" fn(P),
    _lib: Library,
}

fn pulse() -> Option<&'static Pulse> {
    static PULSE: OnceLock<Option<Pulse>> = OnceLock::new();
    PULSE
        .get_or_init(|| {
            // SAFETY: loading the system's library; each symbol is the function of that name with
            // the C signature its field repeats, and the library lives as long as the copies.
            unsafe {
                let lib = Library::new("libpulse-simple.so.0").ok()?;
                Some(Pulse {
                    new: *lib.get(b"pa_simple_new\0").ok()?,
                    write: *lib.get(b"pa_simple_write\0").ok()?,
                    latency: *lib.get(b"pa_simple_get_latency\0").ok()?,
                    flush: *lib.get(b"pa_simple_flush\0").ok()?,
                    free: *lib.get(b"pa_simple_free\0").ok()?,
                    _lib: lib,
                })
            }
        })
        .as_ref()
}

/// Whether a sound server is there to connect to: `PULSE_SERVER`, or the user's native socket.
fn server_there() -> bool {
    if std::env::var_os("PULSE_SERVER").is_some() {
        return true;
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    runtime.is_some_and(|d| d.join("pulse/native").exists())
}

/// The output the desktop's sound server offers.
pub struct PulseOutputs;

impl PulseOutputs {
    /// The outputs, when the library and a server are there.
    pub fn new() -> Option<PulseOutputs> {
        (pulse().is_some() && server_there()).then_some(PulseOutputs)
    }
}

impl AudioOutputs for PulseOutputs {
    fn device(&self) -> Option<DeviceInfo> {
        // (the server mixes and resamples whatever rate is asked for; 48 kHz stereo is its default)
        Some(DeviceInfo {
            sample_rate: 48000.0,
            channels: 2,
            min_period: 0.01,
            max_period: 0.08,
            output_latency: 0.04,
        })
    }

    fn open(&self) -> Box<dyn AudioSink> {
        Box::new(PulseSink::default())
    }
}

/// A stream on the sound server.
#[derive(Default)]
pub struct PulseSink {
    stream: Option<(usize, u32)>,
    interleaved: Vec<f32>,
}

impl PulseSink {
    fn handle(&self) -> Option<(P, &'static Pulse)> {
        Some((self.stream?.0 as P, pulse()?))
    }

    /// Seconds from a sample written now to its leaving the output.
    pub fn latency(&self) -> f64 {
        let Some((s, pa)) = self.handle() else {
            return 0.0;
        };
        let mut err = 0;
        // SAFETY: an open stream.
        let us = unsafe { (pa.latency)(s, &mut err) };
        us as f64 / 1e6
    }

    /// Drops what was written and not played.
    pub fn flush(&mut self) {
        if let Some((s, pa)) = self.handle() {
            let mut err = 0;
            // SAFETY: an open stream.
            unsafe { (pa.flush)(s, &mut err) };
        }
    }

    /// Writes interleaved samples (blocks while the server's buffer is full).
    pub fn write_interleaved(&mut self, samples: &[f32]) -> Result<(), SinkError> {
        let (s, pa) = self
            .handle()
            .ok_or_else(|| SinkError("not started".into()))?;
        let mut err = 0;
        // SAFETY: an open stream and `samples.len()` floats.
        let r = unsafe {
            (pa.write)(
                s,
                samples.as_ptr().cast(),
                std::mem::size_of_val(samples),
                &mut err,
            )
        };
        if r < 0 {
            return Err(SinkError(format!("sound server write failed ({err})")));
        }
        Ok(())
    }
}

// SAFETY: the stream is used by the one thread that owns the sink.
unsafe impl Send for PulseSink {}

impl AudioSink for PulseSink {
    fn start(&mut self, config: &StreamConfig) -> Result<(), SinkError> {
        self.stop();
        let pa = pulse().ok_or_else(|| SinkError("no libpulse".into()))?;
        let spec = SampleSpec {
            format: FLOAT32LE,
            rate: config.sample_rate as u32,
            channels: config.channels.clamp(1, 8) as u8,
        };
        let frame_bytes = 4 * spec.channels as u32;
        let period = config.period_frames.max(128) as u32 * frame_bytes;
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: period * 3,
            prebuf: u32::MAX,
            minreq: period,
            fragsize: u32::MAX,
        };
        let mut err = 0;
        // SAFETY: NUL-ended names and specs alive for the call.
        let s = unsafe {
            (pa.new)(
                null(),
                c"Snowlark".as_ptr(),
                PLAYBACK,
                null(),
                c"Playback".as_ptr(),
                &spec,
                null(),
                &attr,
                &mut err,
            )
        };
        if s.is_null() {
            return Err(SinkError(format!("no sound server ({err})")));
        }
        self.stream = Some((s as usize, spec.channels as u32));
        Ok(())
    }

    fn wait_period(&mut self) -> Result<Instant, SinkError> {
        // (the write blocks while the buffer is full, so the next period is wanted now)
        Ok(Instant::now() + Duration::from_secs_f64(self.latency()))
    }

    fn write(&mut self, channels: &[Vec<f32>]) -> Result<(), SinkError> {
        let n = channels.first().map_or(0, Vec::len);
        let mut buf = std::mem::take(&mut self.interleaved);
        buf.clear();
        for i in 0..n {
            buf.extend(channels.iter().map(|c| c[i]));
        }
        let r = self.write_interleaved(&buf);
        self.interleaved = buf;
        r
    }

    fn stop(&mut self) {
        if let Some((s, pa)) = self.handle() {
            // SAFETY: the stream is this sink's and is not used again.
            unsafe { (pa.free)(s) };
        }
        self.stream = None;
    }
}

impl Drop for PulseSink {
    fn drop(&mut self) {
        self.stop();
    }
}
