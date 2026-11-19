// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AudioContext output across processes (`docs/process-isolation.md`, §5): the renderer renders a
//! context's graph and sends each period to the browser, which plays it on the output device.
//!
//! The renderer's sink ([`RemoteOutputs`], [`RemoteSink`]) may have [`IN_FLIGHT`] periods on their
//! way or queued in the browser; each one the device takes comes back as a credit
//! ([`Down::Took`]), with when it will be heard. The browser runs a thread per stream
//! ([`AudioOutHost`]) that waits for the device to want a period, takes the renderer's next one,
//! waiting up to half a period for it, and writes silence when none came in time: a renderer
//! that falls behind glitches its own sound and holds nothing else up.
//!
//! The renderer's word is checked where it could cost the browser: a stream's configuration is
//! bounded, each period must be the size its stream was opened with, a renderer that sends more
//! than its credits or for a stream it never opened dies, and it gets at most [`MAX_STREAMS`]
//! streams at once.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use snowlark_audio::realtime::{AudioOutputs, AudioSink, DeviceInfo, SinkError, StreamConfig};
use snowlark_ipc::{Error, Replier};

use super::{BrowserPort, RendererPort};

/// The periods a stream may have sent and not yet had played.
pub const IN_FLIGHT: usize = 2;
/// The streams one renderer may have open at once.
pub const MAX_STREAMS: usize = 16;
/// How long a renderer's sink waits for a credit before the output counts as gone.
const STALL: Duration = Duration::from_secs(3);
/// How long the browser keeps what the device offers before asking it again.
const DEVICE_AGE: Duration = Duration::from_secs(1);

/// What an output offers, as it crosses ([`DeviceInfo`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub sample_rate: f32,
    pub channels: u32,
    pub min_period: f64,
    pub max_period: f64,
    pub output_latency: f64,
}

impl From<DeviceInfo> for Device {
    fn from(d: DeviceInfo) -> Device {
        Device {
            sample_rate: d.sample_rate,
            channels: d.channels,
            min_period: d.min_period,
            max_period: d.max_period,
            output_latency: d.output_latency,
        }
    }
}

impl From<Device> for DeviceInfo {
    fn from(d: Device) -> DeviceInfo {
        DeviceInfo {
            sample_rate: d.sample_rate,
            channels: d.channels,
            min_period: d.min_period,
            max_period: d.max_period,
            output_latency: d.output_latency,
        }
    }
}

/// A stream's configuration, as it crosses ([`StreamConfig`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub sample_rate: f32,
    pub channels: u32,
    pub period_frames: u32,
}

impl Config {
    /// Within what any context makes (§1.2.1: rates of 3,000 to 768,000 Hz, at most 32
    /// channels; periods of at most a second).
    fn valid(&self) -> bool {
        (3000.0..=768000.0).contains(&self.sample_rate)
            && (1..=32).contains(&self.channels)
            && self.period_frames >= 1
            && self.period_frames as f32 <= self.sample_rate
    }

    fn samples(&self) -> usize {
        self.channels as usize * self.period_frames as usize
    }
}

/// Renderer to browser.
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    /// What the default output offers (a call: `Option<Device>`).
    Device,
    /// Opens stream `id` and starts it (a call: `Result<(), String>`). Ids are never reused.
    Start { id: u32, config: Config },
    /// A period of stream `id`: its channels one after the other.
    Period { id: u32, samples: Vec<f32> },
    /// Stops stream `id`; its periods not played yet are dropped.
    Stop { id: u32 },
}

/// Browser to renderer.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    /// The device took one of stream `id`'s periods; it will be heard `delay` seconds from now.
    Took { id: u32, delay: f64 },
    /// Stream `id`'s output failed (the device went away).
    Failed { id: u32, why: String },
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A stream's credits, as the renderer counts them.
#[derive(Default)]
struct Credits {
    state: Mutex<CreditState>,
    cv: Condvar,
}

#[derive(Default)]
struct CreditState {
    in_flight: usize,
    /// When the last period taken will be heard, from the browser's word.
    delay: f64,
    failed: Option<String>,
}

struct Shared {
    port: RendererPort,
    streams: Mutex<HashMap<u32, Arc<Credits>>>,
    next: AtomicU32,
}

/// The renderer's end: the browser's output device, through the pipe.
pub struct RemoteOutputs {
    shared: Arc<Shared>,
}

impl RemoteOutputs {
    pub fn new(port: RendererPort) -> RemoteOutputs {
        RemoteOutputs {
            shared: Arc::new(Shared {
                port,
                streams: Mutex::default(),
                next: AtomicU32::new(1),
            }),
        }
    }

    pub(super) fn dispatch(&self, message: Down) {
        let id = match &message {
            Down::Took { id, .. } | Down::Failed { id, .. } => *id,
        };
        let Some(c) = lock(&self.shared.streams).get(&id).cloned() else {
            // (a stream stopped meanwhile)
            return;
        };
        let mut s = lock(&c.state);
        match message {
            Down::Took { delay, .. } => {
                s.in_flight = s.in_flight.saturating_sub(1);
                s.delay = delay;
            }
            Down::Failed { why, .. } => s.failed = Some(why),
        }
        drop(s);
        c.cv.notify_all();
    }
}

impl AudioOutputs for RemoteOutputs {
    fn device(&self) -> Option<DeviceInfo> {
        super::ask::<Option<Device>>(&self.shared.port, &super::Up::AudioOut(Up::Device))
            .flatten()
            .map(DeviceInfo::from)
    }

    fn open(&self) -> Box<dyn AudioSink> {
        Box::new(RemoteSink {
            shared: self.shared.clone(),
            stream: None,
            samples: Vec::new(),
        })
    }
}

/// An AudioContext's output in a renderer: its periods go to the browser.
pub struct RemoteSink {
    shared: Arc<Shared>,
    /// The stream's id, credits and seconds per period, while started.
    stream: Option<(u32, Arc<Credits>, f64)>,
    samples: Vec<f32>,
}

impl AudioSink for RemoteSink {
    fn start(&mut self, config: &StreamConfig) -> Result<(), SinkError> {
        self.stop();
        let id = self.shared.next.fetch_add(1, Ordering::Relaxed);
        let credits = Arc::new(Credits::default());
        lock(&self.shared.streams).insert(id, credits.clone());
        let config = Config {
            sample_rate: config.sample_rate,
            channels: config.channels,
            period_frames: config.period_frames as u32,
        };
        let started = super::ask::<Result<(), String>>(
            &self.shared.port,
            &super::Up::AudioOut(Up::Start { id, config }),
        )
        .unwrap_or_else(|| Err("The browser did not answer".into()));
        if let Err(why) = started {
            // (the browser forgets the stream too)
            lock(&self.shared.streams).remove(&id);
            let _ = self.shared.port.send(&super::Up::AudioOut(Up::Stop { id }));
            return Err(SinkError(why));
        }
        let period = config.period_frames as f64 / config.sample_rate as f64;
        self.stream = Some((id, credits, period));
        Ok(())
    }

    fn wait_period(&mut self) -> Result<Instant, SinkError> {
        let (_, credits, period) = self
            .stream
            .as_ref()
            .ok_or_else(|| SinkError("The stream is not started".into()))?;
        let deadline = Instant::now() + STALL;
        let mut s = lock(&credits.state);
        loop {
            if let Some(why) = &s.failed {
                return Err(SinkError(why.clone()));
            }
            if s.in_flight < IN_FLIGHT {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(SinkError("The output stopped taking audio".into()));
            }
            s = credits
                .cv
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        // (the periods ahead of this one, then the device's own delay)
        let ahead = s.in_flight as f64 * period + s.delay.max(0.0);
        Ok(Instant::now() + Duration::from_secs_f64(ahead))
    }

    fn write(&mut self, channels: &[Vec<f32>]) -> Result<(), SinkError> {
        let (id, credits, _) = self
            .stream
            .as_ref()
            .ok_or_else(|| SinkError("The stream is not started".into()))?;
        self.samples.clear();
        for c in channels {
            self.samples.extend_from_slice(c);
        }
        lock(&credits.state).in_flight += 1;
        let period = Up::Period {
            id: *id,
            samples: std::mem::take(&mut self.samples),
        };
        let sent = self.shared.port.send(&super::Up::AudioOut(period));
        sent.map_err(|e| SinkError(format!("The browser is gone: {e}")))
    }

    fn stop(&mut self) {
        if let Some((id, _, _)) = self.stream.take() {
            lock(&self.shared.streams).remove(&id);
            let _ = self.shared.port.send(&super::Up::AudioOut(Up::Stop { id }));
        }
    }
}

impl Drop for RemoteSink {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The periods a stream's thread plays, as the browser queues them.
#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    cv: Condvar,
}

#[derive(Default)]
struct QueueState {
    periods: VecDeque<Vec<f32>>,
    stop: bool,
}

struct HostStream {
    config: Config,
    queue: Arc<Queue>,
}

/// The browser's end for one renderer: its streams on the output device.
pub struct AudioOutHost {
    port: BrowserPort,
    outputs: Option<Arc<dyn AudioOutputs>>,
    streams: Mutex<HashMap<u32, HostStream>>,
    /// The last id the renderer opened a stream with (ids grow, so each opens once).
    last: Mutex<u32>,
    device: Mutex<Option<(Instant, Option<Device>)>>,
}

impl AudioOutHost {
    pub(super) fn new(port: BrowserPort, outputs: Option<Arc<dyn AudioOutputs>>) -> AudioOutHost {
        AudioOutHost {
            port,
            outputs,
            streams: Mutex::default(),
            last: Mutex::default(),
            device: Mutex::default(),
        }
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    /// What the output offers, asked of the device at most once a [`DEVICE_AGE`].
    fn device(&self) -> Option<Device> {
        let mut d = lock(&self.device);
        match *d {
            Some((at, device)) if at.elapsed() < DEVICE_AGE => device,
            _ => {
                let device = self
                    .outputs
                    .as_ref()
                    .and_then(|o| o.device())
                    .map(Device::from);
                *d = Some((Instant::now(), device));
                device
            }
        }
    }

    pub(super) fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        match message {
            Up::Device => super::answer(&self.port, replier, &self.device()),
            Up::Start { id, config } => {
                let Some(replier) = replier else {
                    return Err(self.bad("a stream started with nobody to answer".into()));
                };
                if !config.valid() {
                    return Err(self.bad(format!("stream {id}: configuration {config:?}")));
                }
                {
                    let mut last = lock(&self.last);
                    if id <= *last {
                        return Err(self.bad(format!("stream {id} opened after {}", *last)));
                    }
                    *last = id;
                }
                let mut streams = lock(&self.streams);
                let refused = if streams.len() >= MAX_STREAMS {
                    Some("Too many audio streams")
                } else if self.outputs.is_none() {
                    Some("No audio output")
                } else {
                    None
                };
                if let Some(why) = refused {
                    drop(streams);
                    return super::answer(&self.port, Some(replier), &Err::<(), _>(why));
                }
                let outputs = self.outputs.clone().expect("checked above");
                let queue = Arc::new(Queue::default());
                streams.insert(
                    id,
                    HostStream {
                        config,
                        queue: queue.clone(),
                    },
                );
                drop(streams);
                let port = self.port.clone();
                let spawned = std::thread::Builder::new()
                    .name("audio output".into())
                    .spawn(move || play(port, id, config, outputs.open(), queue, replier));
                if spawned.is_err() {
                    lock(&self.streams).remove(&id);
                }
                Ok(())
            }
            Up::Period { id, samples } => {
                if replier.is_some() {
                    return Err(self.bad("a period sent as a call".into()));
                }
                let streams = lock(&self.streams);
                let Some(s) = streams.get(&id) else {
                    // (a stream that failed to start is gone; one never opened is the renderer's
                    // mistake)
                    drop(streams);
                    if id <= *lock(&self.last) {
                        return Ok(());
                    }
                    return Err(self.bad(format!("a period for stream {id}, never opened")));
                };
                if samples.len() != s.config.samples() {
                    let why = format!("stream {id}: a period of {} samples", samples.len());
                    drop(streams);
                    return Err(self.bad(why));
                }
                let mut q = lock(&s.queue.state);
                if q.periods.len() >= IN_FLIGHT {
                    drop(q);
                    drop(streams);
                    return Err(self.bad(format!("stream {id}: periods beyond its credits")));
                }
                q.periods.push_back(samples);
                drop(q);
                s.queue.cv.notify_all();
                Ok(())
            }
            Up::Stop { id } => {
                if let Some(s) = lock(&self.streams).remove(&id) {
                    lock(&s.queue.state).stop = true;
                    s.queue.cv.notify_all();
                }
                Ok(())
            }
        }
    }

    /// The renderer is gone: so are its streams.
    pub(super) fn gone(&self) {
        for (_, s) in lock(&self.streams).drain() {
            lock(&s.queue.state).stop = true;
            s.queue.cv.notify_all();
        }
    }

    /// The streams playing now (for tests).
    pub fn streams(&self) -> usize {
        lock(&self.streams).len()
    }
}

/// A stream's thread: starts the sink, answers the start, then plays the renderer's periods.
fn play(
    port: BrowserPort,
    id: u32,
    config: Config,
    mut sink: Box<dyn AudioSink>,
    queue: Arc<Queue>,
    replier: Replier,
) {
    let stream = StreamConfig {
        sample_rate: config.sample_rate,
        channels: config.channels,
        period_frames: config.period_frames as usize,
    };
    let started = sink.start(&stream).map_err(|e| e.0);
    let ok = started.is_ok();
    let _ = super::answer(&port, Some(replier), &started);
    if !ok {
        return;
    }
    let frames = config.period_frames as usize;
    let half = Duration::from_secs_f64(frames as f64 / config.sample_rate as f64 / 2.0);
    let mut channels = vec![vec![0.0f32; frames]; config.channels as usize];
    loop {
        let at = match sink.wait_period() {
            Ok(at) => at,
            Err(e) => {
                let _ = port.send(&super::Down::AudioOut(Down::Failed { id, why: e.0 }));
                break;
            }
        };
        let period = {
            let mut q = lock(&queue.state);
            let deadline = Instant::now() + half;
            while q.periods.is_empty() && !q.stop {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                q = queue
                    .cv
                    .wait_timeout(q, deadline - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            if q.stop {
                break;
            }
            q.periods.pop_front()
        };
        match &period {
            Some(samples) => {
                for (c, dst) in channels.iter_mut().enumerate() {
                    dst.copy_from_slice(&samples[c * frames..(c + 1) * frames]);
                }
            }
            // (the renderer fell behind: a glitch of silence)
            None => channels.iter_mut().for_each(|c| c.fill(0.0)),
        }
        if let Err(e) = sink.write(&channels) {
            let _ = port.send(&super::Down::AudioOut(Down::Failed { id, why: e.0 }));
            break;
        }
        if period.is_some() {
            let delay = at.saturating_duration_since(Instant::now()).as_secs_f64();
            let _ = port.send(&super::Down::AudioOut(Down::Took { id, delay }));
        }
    }
    sink.stop();
}
