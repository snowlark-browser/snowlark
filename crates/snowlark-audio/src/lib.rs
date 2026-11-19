// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Audio API 1.1 rendering, the rendering thread's half of an audio context.
//!
//! The control thread (the page) owns the JS objects; it describes every change to the graph as a
//! [`graph::Control`] message (the spec's control message queue). The rendering thread owns a
//! [`graph::Graph`]: the nodes' processing state by id, their connections and their AudioParams'
//! timelines. At each render quantum it applies the messages queued so far, orders the nodes,
//! computes each AudioParam and each input, and runs each node (§2.4 "Rendering an Audio Graph").
//! What the page must hear about (a source that ended, a suspension reached, the rendered buffer)
//! goes back as [`graph::Notice`]s.
//!
//! [`offline`] renders an OfflineAudioContext on a thread of its own; [`realtime`] an
//! AudioContext, period by period into an output sink at the rate the sink consumes it.

pub mod analyser;
pub mod biquad;
pub mod bus;
pub mod compressor;
pub mod convolver;
pub mod decode;
pub mod delay;
pub mod fft;
pub mod graph;
pub mod iir;
pub mod node;
pub mod offline;
pub mod oscillator;
pub mod panner;
pub mod param;
pub mod realtime;
pub mod resample;
pub mod shaper;

/// The number of sample-frames in a render quantum (§1.1 "render quantum size", the default).
pub const RENDER_QUANTUM: usize = 128;

/// The most channels an AudioBuffer, an OfflineAudioContext or a node's `channelCount` may have
/// (§1.4 "The AudioBuffer Interface": "at least 32").
pub const MAX_CHANNELS: u32 = 32;

/// The lowest and highest sample rates an AudioBuffer or a context may have (§1.4: "at least
/// 8000 to 96000", here the range of current engines).
pub const MIN_SAMPLE_RATE: f32 = 3000.0;
pub const MAX_SAMPLE_RATE: f32 = 768000.0;
