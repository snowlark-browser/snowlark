// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What each kind of AudioNode does with a render quantum.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::analyser::AnalyserInput;
use crate::biquad::Biquad;
use crate::bus::AudioBus;
use crate::compressor::{Compressor, Settings};
use crate::convolver::Convolver;
use crate::delay::Delay;
use crate::iir::Iir;
use crate::oscillator::Oscillator;
use crate::panner::{spatial_pan, stereo_pan, PannerSettings};
use crate::shaper::Shaper;
use crate::RENDER_QUANTUM as N;

/// An AudioBuffer's contents as the rendering thread holds them (acquired from the page).
#[derive(Debug, Default)]
pub struct BufferData {
    pub sample_rate: f32,
    pub length: usize,
    pub channels: Vec<Vec<f32>>,
}

impl BufferData {
    pub fn duration(&self) -> f64 {
        self.length as f64 / self.sample_rate as f64
    }
}

/// The render quantum being processed.
#[derive(Clone, Copy, Debug)]
pub struct Quantum {
    /// The context's sample-frame at the start of the quantum.
    pub frame: u64,
    pub sample_rate: f64,
}

/// An AudioScheduledSourceNode's `start()` and `stop()` (§1.8).
#[derive(Clone, Copy, Debug, Default)]
pub struct Schedule {
    pub start: Option<f64>,
    pub stop: Option<f64>,
    pub ended: bool,
}

impl Schedule {
    /// The frames of this quantum the source plays, `[from, to)` (from the first frame at or after
    /// `start` to the first at or after `stop`), and whether it ends in this quantum. `None`:
    /// silent.
    pub(crate) fn frames(&mut self, q: Quantum) -> (Option<(usize, usize)>, bool) {
        if self.ended {
            return (None, false);
        }
        let Some(start) = self.start else {
            return (None, false);
        };
        let sr = q.sample_rate;
        let first = (start * sr).ceil().max(0.0) as u64;
        let end = self.stop.map(|s| (s * sr).ceil().max(0.0) as u64);
        let q_end = q.frame + N as u64;
        if let Some(e) = end {
            if e <= q.frame {
                self.ended = true;
                return (None, true);
            }
        }
        if first >= q_end {
            return (None, false);
        }
        let from = first.saturating_sub(q.frame) as usize;
        let (to, ends) = match end {
            Some(e) if e <= q_end => ((e - q.frame) as usize, true),
            _ => (N, false),
        };
        if ends {
            self.ended = true;
        }
        if from >= to {
            return (None, ends);
        }
        (Some((from, to)), ends)
    }
}

/// An AudioBufferSourceNode's state (§1.9).
#[derive(Debug, Default)]
pub struct BufferSource {
    pub schedule: Schedule,
    pub buffer: Option<Arc<BufferData>>,
    pub looping: bool,
    pub loop_start: f64,
    pub loop_end: f64,
    /// `start()`'s offset and duration.
    pub offset: f64,
    pub duration: Option<f64>,
    started: bool,
    entered_loop: bool,
    /// The playhead, in sample-frames of the buffer (a whole number of frames advances exactly
    /// when the buffer is at the context's rate).
    buffer_time: f64,
    /// Buffer frames played so far, against `duration`.
    elapsed: f64,
}

/// A node kind with what its processing keeps between quanta.
#[derive(Debug)]
pub enum Kind {
    /// The AudioDestinationNode: its input is what the context renders.
    Destination,
    Gain,
    ConstantSource(Schedule),
    BufferSource(Box<BufferSource>),
    Oscillator(Box<Oscillator>),
    /// A DynamicsCompressorNode, and where its `reduction` goes (f32 bits).
    Compressor(Box<Compressor>, Arc<AtomicU32>),
    Biquad(Box<Biquad>),
    Delay(Box<Delay>),
    /// A ChannelSplitterNode: input channel `i` to output `i`.
    Splitter,
    /// A ChannelMergerNode: input `i` to output channel `i`.
    Merger,
    StereoPanner,
    Panner(Box<PannerSettings>),
    Shaper(Box<Shaper>),
    Iir(Box<Iir>),
    /// An AnalyserNode: its input passes through, and goes down-mixed into the ring the control
    /// thread reads (with a scratch bus for the down-mix).
    Analyser(AnalyserInput, Box<AudioBus>),
    Convolver(Option<Box<Convolver>>),
}

impl Kind {
    /// The scheduled-source state, for `start()` and `stop()`.
    pub fn schedule_mut(&mut self) -> Option<&mut Schedule> {
        match self {
            Kind::ConstantSource(s) => Some(s),
            Kind::BufferSource(b) => Some(&mut b.schedule),
            Kind::Oscillator(o) => Some(&mut o.schedule),
            _ => None,
        }
    }

    /// Processes one quantum: `inputs` are the computed input buses (`connected`: whether each
    /// has a connection), `params` the computed values of the node's AudioParams in their order,
    /// `outputs` the node's outputs. Returns whether a scheduled source ended in this quantum.
    pub fn process(
        &mut self,
        q: Quantum,
        inputs: &[AudioBus],
        connected: &[bool],
        params: &[&[f32; N]],
        outputs: &mut [AudioBus],
    ) -> bool {
        let silent = !connected.first().copied().unwrap_or(false);
        let (out, rest) = outputs.split_first_mut().expect("a node has an output");
        match self {
            Kind::Destination => {
                out.copy_from(&inputs[0]);
                false
            }
            Kind::Gain => {
                // §1.20: the input times the gain, per frame.
                let input = &inputs[0];
                out.zero(input.channels());
                if input.is_silent() {
                    return false;
                }
                let gain = params[0];
                for (o, i) in out.all_mut().iter_mut().zip(input.all()) {
                    for f in 0..N {
                        o[f] = i[f] * gain[f];
                    }
                }
                false
            }
            Kind::ConstantSource(schedule) => {
                // §1.14: the offset while playing.
                out.zero(1);
                let (frames, ended) = schedule.frames(q);
                if let Some((from, to)) = frames {
                    let offset = params[0];
                    out.channel_mut(0)[from..to].copy_from_slice(&offset[from..to]);
                }
                ended
            }
            Kind::BufferSource(b) => b.process(q, params, out),
            Kind::Oscillator(o) => {
                out.zero(1);
                let mut wave = [0.0f32; N];
                let ended = o.process(q, params, &mut wave);
                out.channel_mut(0).copy_from_slice(&wave);
                ended
            }
            Kind::Compressor(c, reduction) => {
                let s = Settings {
                    threshold: params[0][0],
                    knee: params[1][0],
                    ratio: params[2][0],
                    attack: params[3][0],
                    release: params[4][0],
                };
                let r = c.process(s, &inputs[0], out);
                reduction.store(r.to_bits(), Ordering::Relaxed);
                false
            }
            Kind::Biquad(b) => {
                b.process(q.sample_rate, params, &inputs[0], silent, out);
                false
            }
            Kind::Delay(d) => {
                // In a cycle only the reader runs here; the graph writes after every node.
                if !d.split {
                    d.write(&inputs[0], silent);
                    d.read(d.written() - N as u64, params[0], q.sample_rate, out);
                } else {
                    d.read(d.written(), params[0], q.sample_rate, out);
                }
                false
            }
            Kind::Splitter => {
                let input = &inputs[0];
                for (i, o) in std::iter::once(out).chain(rest.iter_mut()).enumerate() {
                    o.zero(1);
                    if i < input.channels() && !input.is_silent() {
                        o.channel_mut(0).copy_from_slice(input.channel(i));
                    }
                }
                false
            }
            Kind::Merger => {
                out.zero(inputs.len());
                for (i, input) in inputs.iter().enumerate() {
                    if !input.is_silent() {
                        out.channel_mut(i).copy_from_slice(input.channel(0));
                    }
                }
                false
            }
            Kind::StereoPanner => {
                stereo_pan(&inputs[0], params[0], out);
                false
            }
            Kind::Panner(settings) => {
                spatial_pan(settings, params, &inputs[0], out);
                false
            }
            Kind::Shaper(s) => {
                s.process(&inputs[0], out);
                false
            }
            Kind::Iir(f) => {
                f.process(&inputs[0], silent, out);
                false
            }
            Kind::Analyser(ring, mono) => {
                ring.write(&inputs[0], mono);
                out.copy_from(&inputs[0]);
                false
            }
            Kind::Convolver(c) => {
                Convolver::process(c.as_deref_mut(), &inputs[0], silent, out);
                false
            }
        }
    }
}

impl BufferSource {
    /// §1.9.5 "Playback of AudioBuffer Contents".
    fn process(&mut self, q: Quantum, params: &[&[f32; N]], out: &mut AudioBus) -> bool {
        let channels = self.buffer.as_ref().map_or(1, |b| b.channels.len().max(1));
        out.zero(channels);
        if self.schedule.ended {
            return false;
        }
        let Some(start) = self.schedule.start else {
            return false;
        };
        // No buffer (yet): silence until `stop()`; a buffer set later plays.
        let Some(buffer) = self.buffer.clone() else {
            return self.schedule.frames(q).1;
        };
        let sr = q.sample_rate;
        let dt = 1.0 / sr;
        // playbackRate and detune are k-rate.
        let rate = params[0][0] as f64 * 2f64.powf(params[1][0] as f64 / 1200.0);
        let buffer_duration = buffer.duration();
        let (loop_start, loop_end) = if self.looping {
            if self.loop_start >= 0.0 && self.loop_end > 0.0 && self.loop_start < self.loop_end {
                (self.loop_start, self.loop_end.min(buffer_duration))
            } else {
                (0.0, buffer_duration)
            }
        } else {
            self.entered_loop = false;
            (0.0, buffer_duration)
        };
        let stop = self.schedule.stop.unwrap_or(f64::INFINITY);
        let duration = self.duration.unwrap_or(f64::INFINITY);
        let start_frame = (start * sr).ceil();
        // The loop and the buffer's end in buffer frames, and the playhead's step per frame.
        let bsr = buffer.sample_rate as f64;
        // Times that are whole frames up to rounding are whole frames.
        let frames = |t: f64| {
            let x = t * bsr;
            if (x - x.round()).abs() < 1e-6 {
                x.round()
            } else {
                x
            }
        };
        let end_f = buffer.length as f64;
        let (ls_f, le_f) = (frames(loop_start).min(end_f), frames(loop_end).min(end_f));
        // `duration` is buffer time played, counted in buffer frames like the playhead.
        let duration_f = frames(duration);
        let step = rate * bsr / sr;
        let mut ended = false;
        for i in 0..N {
            let frame = (q.frame + i as u64) as f64;
            let time = frame * dt;
            if frame < start_frame {
                continue;
            }
            if time >= stop || self.elapsed >= duration_f {
                ended = true;
                break;
            }
            if !self.started {
                let mut offset = self.offset;
                if self.looping && rate >= 0.0 && offset >= loop_end {
                    offset = loop_end;
                }
                if rate < 0.0 && offset < loop_start {
                    offset = loop_start;
                }
                // A start between two frames advances the playhead by the part of a frame
                // already past.
                let late = (time - start).max(0.0) * rate;
                self.buffer_time = (offset + late) * bsr;
                self.elapsed = late.abs() * bsr;
                self.offset = offset;
                self.started = true;
            }
            if self.looping {
                if !self.entered_loop {
                    if self.offset < loop_end && self.buffer_time >= ls_f {
                        self.entered_loop = true;
                    }
                    if self.offset >= loop_end && self.buffer_time < le_f {
                        self.entered_loop = true;
                    }
                }
                if self.entered_loop && le_f > ls_f {
                    let span = le_f - ls_f;
                    while self.buffer_time >= le_f {
                        self.buffer_time -= span;
                    }
                    while self.buffer_time < ls_f {
                        self.buffer_time += span;
                    }
                }
            } else if (rate >= 0.0 && self.buffer_time >= end_f)
                || (rate < 0.0 && self.buffer_time < 0.0)
            {
                // The playhead left the buffer: playback is over.
                ended = true;
                break;
            }
            if self.buffer_time >= 0.0 && self.buffer_time < end_f {
                let loop_from = if self.looping && self.entered_loop {
                    Some((ls_f, le_f))
                } else {
                    None
                };
                for (c, data) in buffer.channels.iter().enumerate() {
                    out.channel_mut(c)[i] = sample_at(data, self.buffer_time, loop_from);
                }
            }
            self.buffer_time += step;
            self.elapsed += step.abs();
        }
        if ended {
            self.schedule.ended = true;
        }
        ended
    }
}

/// The buffer's signal at frame position `pos`, linearly interpolated between sample-frames. Past
/// the last frame the next frame is the loop's start (`looping`, in frames) when looping, else the
/// line through the last two frames continues.
fn sample_at(data: &[f32], pos: f64, looping: Option<(f64, f64)>) -> f32 {
    let len = data.len();
    if len == 0 {
        return 0.0;
    }
    let i = (pos as usize).min(len - 1);
    let frac = pos - i as f64;
    if frac == 0.0 {
        return data[i];
    }
    let next = match looping {
        Some((start, end)) if (i + 1) as f64 >= end => {
            let wrapped = pos + 1.0 - (end - start);
            Some(data[(wrapped.max(0.0) as usize).min(len - 1)])
        }
        _ if i + 1 < len => Some(data[i + 1]),
        _ => None,
    };
    let v = match next {
        Some(n) => (1.0 - frac) * data[i] as f64 + frac * n as f64,
        None if i >= 1 => data[i] as f64 + (data[i] as f64 - data[i - 1] as f64) * frac,
        None => data[i] as f64,
    };
    v as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q0: Quantum = Quantum {
        frame: 0,
        sample_rate: 128.0,
    };

    fn ones() -> [f32; N] {
        [1.0; N]
    }

    #[test]
    fn a_constant_source_plays_from_the_first_frame_at_or_after_start() {
        let mut k = Kind::ConstantSource(Schedule {
            start: Some(10.5 / 128.0),
            stop: Some(20.0 / 128.0),
            ended: false,
        });
        let mut out = AudioBus::new(1);
        let off = [0.5f32; N];
        let ended = k.process(Q0, &[], &[], &[&off], std::slice::from_mut(&mut out));
        let c = out.channel(0);
        assert_eq!(c[10], 0.0);
        assert_eq!(c[11], 0.5);
        assert_eq!(c[19], 0.5);
        assert_eq!(c[20], 0.0);
        assert!(ended);
    }

    #[test]
    fn a_buffer_source_plays_the_buffer_then_ends() {
        let data: Vec<f32> = (0..64).map(|i| i as f32).collect();
        let mut b = BufferSource {
            buffer: Some(Arc::new(BufferData {
                sample_rate: 128.0,
                length: 64,
                channels: vec![data],
            })),
            ..Default::default()
        };
        b.schedule.start = Some(0.0);
        let mut out = AudioBus::new(1);
        let one = ones();
        let zero = [0.0; N];
        let ended = b.process(Q0, &[&one, &zero], &mut out);
        let c = out.channel(0);
        assert_eq!(c[0], 0.0);
        assert_eq!(c[63], 63.0);
        assert_eq!(c[64], 0.0);
        assert!(ended);
    }

    #[test]
    fn a_half_rate_buffer_source_interpolates_between_frames() {
        let mut b = BufferSource {
            buffer: Some(Arc::new(BufferData {
                sample_rate: 128.0,
                length: 4,
                channels: vec![vec![0.0, 2.0, 4.0, 6.0]],
            })),
            ..Default::default()
        };
        b.schedule.start = Some(0.0);
        let mut out = AudioBus::new(1);
        let half = [0.5f32; N];
        let zero = [0.0; N];
        b.process(Q0, &[&half, &zero], &mut out);
        let c = out.channel(0);
        assert_eq!(&c[..8], [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(c[8], 0.0);
    }

    #[test]
    fn a_sub_sample_start_offsets_the_playhead() {
        let data: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let mut b = BufferSource {
            buffer: Some(Arc::new(BufferData {
                sample_rate: 128.0,
                length: 16,
                channels: vec![data],
            })),
            ..Default::default()
        };
        // Start a quarter frame after frame 2: frame 3 plays the buffer at 0.75 frames.
        b.schedule.start = Some(2.25 / 128.0);
        let mut out = AudioBus::new(1);
        b.process(Q0, &[&ones(), &[0.0; N]], &mut out);
        let c = out.channel(0);
        assert_eq!(c[2], 0.0);
        assert!((c[3] - 0.75).abs() < 1e-6, "{}", c[3]);
        assert!((c[4] - 1.75).abs() < 1e-6);
    }

    #[test]
    fn a_looping_buffer_source_wraps_inside_its_loop() {
        let mut b = BufferSource {
            buffer: Some(Arc::new(BufferData {
                sample_rate: 128.0,
                length: 4,
                channels: vec![vec![0.0, 1.0, 2.0, 3.0]],
            })),
            looping: true,
            loop_start: 1.0 / 128.0,
            loop_end: 3.0 / 128.0,
            ..Default::default()
        };
        b.schedule.start = Some(0.0);
        let mut out = AudioBus::new(1);
        let ended = b.process(Q0, &[&ones(), &[0.0; N]], &mut out);
        assert_eq!(&out.channel(0)[..7], [0.0, 1.0, 2.0, 1.0, 2.0, 1.0, 2.0]);
        assert!(!ended);
    }

    #[test]
    fn a_buffer_at_the_context_rate_plays_back_exactly() {
        // The playhead advances by whole frames, so no frame is interpolated.
        let sr = 44100.0;
        let data: Vec<f32> = (0..4410)
            .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / sr).sin() as f32)
            .collect();
        let mut k = Kind::BufferSource(Box::new(BufferSource {
            schedule: Schedule {
                start: Some(0.0),
                stop: None,
                ended: false,
            },
            buffer: Some(Arc::new(BufferData {
                sample_rate: sr as f32,
                length: data.len(),
                channels: vec![data.clone()],
            })),
            ..Default::default()
        }));
        let mut out = AudioBus::new(1);
        let (rate, detune) = ([1.0f32; N], [0.0f32; N]);
        for q in 0..34 {
            let quantum = Quantum {
                frame: q * N as u64,
                sample_rate: sr,
            };
            k.process(
                quantum,
                &[],
                &[],
                &[&rate, &detune],
                std::slice::from_mut(&mut out),
            );
            for (i, v) in out.channel(0).iter().enumerate() {
                let f = q as usize * N + i;
                assert_eq!(*v, data[f], "frame {f}");
            }
        }
    }

    #[test]
    fn a_whole_buffer_loop_wraps_to_its_first_frame() {
        // loopEnd is the buffer's duration in seconds; in frames it is the length exactly, though
        // (65201 / 48000) * 48000 is not 65201.
        let sr = 48000.0;
        let len = 65201;
        let data: Vec<f32> = (0..len).map(|i| 0.1 + (i % 7) as f32 * 0.01).collect();
        let duration = len as f64 / sr;
        let mut k = Kind::BufferSource(Box::new(BufferSource {
            schedule: Schedule {
                start: Some(0.0),
                stop: None,
                ended: false,
            },
            buffer: Some(Arc::new(BufferData {
                sample_rate: sr as f32,
                length: len,
                channels: vec![data.clone()],
            })),
            looping: true,
            loop_start: 0.0,
            loop_end: duration,
            ..Default::default()
        }));
        let mut out = AudioBus::new(1);
        let (rate, detune) = ([1.0f32; N], [0.0f32; N]);
        for q in 0..(2 * len / N + 1) as u64 {
            let quantum = Quantum {
                frame: q * N as u64,
                sample_rate: sr,
            };
            k.process(
                quantum,
                &[],
                &[],
                &[&rate, &detune],
                std::slice::from_mut(&mut out),
            );
            for (i, v) in out.channel(0).iter().enumerate() {
                let f = q as usize * N + i;
                assert_eq!(*v, data[f % len], "frame {f}");
            }
        }
    }
}
