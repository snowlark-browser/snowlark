// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Audio API 1.1 on the control thread: BaseAudioContext, AudioContext and
//! OfflineAudioContext, AudioNode, AudioParam, AudioBuffer and the nodes of the rendering core.
//!
//! The JS objects live here; what renders lives in `snowlark-audio` on the context's rendering
//! thread. Every change the page makes to the graph is a control message on the context's link;
//! an offline context's messages wait on it until `startRendering()`, and the rendering thread
//! applies them at render quantum boundaries. An AudioContext's rendering thread starts with the
//! context and renders into an output sink once the context is allowed to start; its `resume()`,
//! `suspend()` and `close()` are control messages too, answered in order. What the rendering
//! reports (a source ended, a suspension reached, rendering complete, a state change carried out)
//! comes back as notices, which [`Engine::poll_audio`] turns into media element tasks.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use snowlark_audio::bus::{ChannelConfig, CountMode, Interpretation};
use snowlark_audio::compressor::Compressor;
use snowlark_audio::decode::DecodeJob;
use snowlark_audio::graph::{
    Control, Graph, NewParam, NodeId, Notice, ParamId, ParamOp, SharedValue,
};
use snowlark_audio::node::{BufferData, Kind, Schedule};
use snowlark_audio::offline::{self, Link, OfflineNotice, SuspendError};
use snowlark_audio::oscillator::{Oscillator, PeriodicWave, Shape};
use snowlark_audio::param::{Event, ParamDesc, Rate, Timeline};
use snowlark_audio::realtime::{
    self, AudioSink, Command, DeviceInfo, LatencyHint, NullSink, RealtimeNotice, StreamConfig,
};
use snowlark_audio::{MAX_CHANNELS, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE, RENDER_QUANTUM};
use snowlark_bindings::generated::{
    AnalyserOptions, BiquadFilterOptions, BiquadFilterType, ChannelMergerOptions,
    ChannelSplitterOptions, ConvolverOptions, DelayOptions, DistanceModelType, IIRFilterOptions,
    OverSampleType, PannerOptions, PanningModelType, StereoPannerOptions, WaveShaperOptions,
};
use snowlark_bindings::generated::{
    AudioBufferImpl, AudioBufferOptions, AudioBufferSourceNodeImpl, AudioBufferSourceOptions,
    AudioContextImpl, AudioContextLatencyCategory, AudioContextLatencyCategoryOrDouble,
    AudioContextOptions, AudioContextState, AudioDestinationNodeImpl, AudioNodeImpl,
    AudioParamImpl, AudioScheduledSourceNodeImpl, AudioTimestamp, AutomationRate,
    BaseAudioContextImpl, ChannelCountMode, ChannelInterpretation, ConstantSourceNodeImpl,
    ConstantSourceOptions, DecodeErrorCallback, DecodeSuccessCallback, DynamicsCompressorNodeImpl,
    DynamicsCompressorOptions, GainNodeImpl, GainOptions, OfflineAudioCompletionEventImpl,
    OfflineAudioCompletionEventInit, OfflineAudioContextImpl, OfflineAudioContextOptions,
    OscillatorNodeImpl, OscillatorOptions, OscillatorType, PeriodicWaveConstraints,
    PeriodicWaveImpl, PeriodicWaveOptions,
};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsObject, JsPromise, JsValue};
use snowlark_bindings::InterfaceId;

use super::events::EventExt;
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;
use crate::media::AutoplayPolicy;

mod nodes;
use nodes::{Extra, ListenerData};

/// The engine's audio state: the contexts whose rendering may still report.
#[derive(Default)]
pub struct AudioState {
    /// Offline contexts rendering.
    rendering: Vec<Handle>,
    /// AudioContexts not closed.
    realtime: Vec<Handle>,
    /// `decodeAudioData()` calls decoding.
    decoding: Vec<Decoding>,
}

/// A `decodeAudioData()` call whose data is being decoded.
struct Decoding {
    job: DecodeJob,
    context: Handle,
    promise: JsPromise,
    success: Option<DecodeSuccessCallback>,
    error: Option<DecodeErrorCallback>,
}

/// A Web Audio object.
pub enum AudioObject {
    Context(Box<ContextData>),
    Node(Box<NodeData>),
    Param(Box<ParamData>),
    Buffer(Box<BufferObj>),
    /// A PeriodicWave: its tables at the context's sample rate.
    Wave(Arc<PeriodicWave>),
    Listener(Box<ListenerData>),
}

impl AudioObject {
    pub fn interface(&self) -> InterfaceId {
        match self {
            AudioObject::Context(c) => match c.link {
                ContextLink::Offline(_) => InterfaceId::OfflineAudioContext,
                ContextLink::Realtime(_) => InterfaceId::AudioContext,
            },
            AudioObject::Node(n) => match n.kind {
                NodeKind::Destination => InterfaceId::AudioDestinationNode,
                NodeKind::Gain => InterfaceId::GainNode,
                NodeKind::ConstantSource => InterfaceId::ConstantSourceNode,
                NodeKind::BufferSource => InterfaceId::AudioBufferSourceNode,
                NodeKind::Oscillator => InterfaceId::OscillatorNode,
                NodeKind::Compressor => InterfaceId::DynamicsCompressorNode,
                NodeKind::Biquad => InterfaceId::BiquadFilterNode,
                NodeKind::Delay => InterfaceId::DelayNode,
                NodeKind::Splitter => InterfaceId::ChannelSplitterNode,
                NodeKind::Merger => InterfaceId::ChannelMergerNode,
                NodeKind::StereoPanner => InterfaceId::StereoPannerNode,
                NodeKind::Panner => InterfaceId::PannerNode,
                NodeKind::Shaper => InterfaceId::WaveShaperNode,
                NodeKind::Iir => InterfaceId::IIRFilterNode,
                NodeKind::Analyser => InterfaceId::AnalyserNode,
                NodeKind::Convolver => InterfaceId::ConvolverNode,
            },
            AudioObject::Param(_) => InterfaceId::AudioParam,
            AudioObject::Buffer(_) => InterfaceId::AudioBuffer,
            AudioObject::Wave(_) => InterfaceId::PeriodicWave,
            AudioObject::Listener(_) => InterfaceId::AudioListener,
        }
    }
}

/// An AudioContext or an OfflineAudioContext.
pub struct ContextData {
    sample_rate: f32,
    /// An offline context's channels; an AudioContext's output's (the destination's
    /// `maxChannelCount`).
    channels: u32,
    length: u32,
    /// The `state` attribute.
    state: AudioContextState,
    destination: Handle,
    link: ContextLink,
    rendering_started: bool,
    next_node: NodeId,
    next_param: ParamId,
    /// The nodes by id, for the rendering thread's notices.
    nodes: Vec<Handle>,
    render_promise: Option<JsPromise>,
    /// `suspend()` promises: an offline context's by frame, an AudioContext's by call.
    suspends: BTreeMap<u64, JsPromise>,
    /// The Window whose realm the context's tasks run in.
    global: Handle,
    /// The built-in waveforms, made on first use.
    waves: [Option<Arc<PeriodicWave>>; 4],
    /// The AudioListener, made on first use.
    listener: Option<Handle>,
}

/// The rendering thread's end, by kind of context.
enum ContextLink {
    Offline(Arc<Link>),
    Realtime(Box<Realtime>),
}

impl ContextLink {
    fn send(&self, msg: Control) {
        match self {
            ContextLink::Offline(l) => l.send(msg),
            ContextLink::Realtime(r) => r.link.send(msg),
        }
    }

    fn frame(&self) -> u64 {
        match self {
            ContextLink::Offline(l) => l.frame(),
            ContextLink::Realtime(r) => r.link.frame(),
        }
    }

    fn offline(&self) -> Option<Arc<Link>> {
        match self {
            ContextLink::Offline(l) => Some(l.clone()),
            ContextLink::Realtime(_) => None,
        }
    }
}

/// An AudioContext's own state (§1.2).
struct Realtime {
    link: Arc<realtime::Link>,
    /// [[control thread state]]: what the control thread last asked for.
    control: AudioContextState,
    /// [[suspended by user]].
    suspended_by_user: bool,
    /// [[pending resume promises]], and the promises of `resume()` calls waiting for the
    /// rendering thread: resolved in order once it runs.
    pending_resume: Vec<JsPromise>,
    close_promise: Option<JsPromise>,
    next_suspend: u64,
    base_latency: f64,
    output_latency: f64,
}

impl Drop for Realtime {
    /// The context went away with its realm: the rendering thread ends.
    fn drop(&mut self) {
        self.link.stop();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Destination,
    Gain,
    ConstantSource,
    BufferSource,
    Oscillator,
    Compressor,
    Biquad,
    Delay,
    Splitter,
    Merger,
    StereoPanner,
    Panner,
    Shaper,
    Iir,
    Analyser,
    Convolver,
}

/// Where an output goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Node(Handle, u32),
    Param(Handle),
}

/// An AudioNode.
pub struct NodeData {
    context: Handle,
    id: NodeId,
    kind: NodeKind,
    inputs: u32,
    outputs: u32,
    config: ChannelConfig,
    params: Vec<Handle>,
    /// Outgoing connections: (output, where).
    connections: Vec<(u32, Target)>,
    /// AudioScheduledSourceNode's [[source started]].
    started: bool,
    // AudioBufferSourceNode:
    buffer: Option<Handle>,
    buffer_set: bool,
    looping: bool,
    loop_start: f64,
    loop_end: f64,
    // OscillatorNode:
    osc_type: OscillatorType,
    // DynamicsCompressorNode:
    reduction: Option<Arc<AtomicU32>>,
    /// The other kinds' own state.
    extra: Extra,
}

/// An AudioParam.
pub struct ParamData {
    context: Handle,
    id: ParamId,
    desc: ParamDesc,
    rate: Rate,
    shared: SharedValue,
    /// The control thread's copy of the timeline, for the overlap checks.
    timeline: Timeline,
}

/// An AudioBuffer: its channels are Float32Arrays.
pub struct BufferObj {
    sample_rate: f32,
    length: u32,
    channels: Vec<JsObject>,
}

fn not_supported(message: &'static str) -> rt::Error {
    exception(ExceptionName::NotSupportedError, message)
}

fn invalid_state(message: &'static str) -> rt::Error {
    exception(ExceptionName::InvalidStateError, message)
}

fn index_size(message: &'static str) -> rt::Error {
    exception(ExceptionName::IndexSizeError, message)
}

fn invalid_access(message: &'static str) -> rt::Error {
    exception(ExceptionName::InvalidAccessError, message)
}

/// §1.4: the checks of an AudioBuffer's (and an offline context's) size and rate.
fn check_buffer_shape(channels: u32, length: u32, sample_rate: f32) -> rt::Result<()> {
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(not_supported("The number of channels is not supported"));
    }
    if length == 0 {
        return Err(not_supported("The length must be at least 1"));
    }
    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate) {
        return Err(not_supported("The sample rate is not supported"));
    }
    Ok(())
}

fn count_mode(m: ChannelCountMode) -> CountMode {
    match m {
        ChannelCountMode::Max => CountMode::Max,
        ChannelCountMode::ClampedMax => CountMode::ClampedMax,
        ChannelCountMode::Explicit => CountMode::Explicit,
    }
}

fn interpretation(i: ChannelInterpretation) -> Interpretation {
    match i {
        ChannelInterpretation::Speakers => Interpretation::Speakers,
        ChannelInterpretation::Discrete => Interpretation::Discrete,
    }
}

/// Reads a Float32Array (or any view) as f32 values.
fn read_f32(cx: &mut Cx, array: &JsObject) -> Vec<f32> {
    let scope = cx.scope();
    let obj = v8::Local::new(scope, array.global());
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(obj) else {
        return Vec::new();
    };
    let mut bytes = vec![0u8; view.byte_length()];
    view.copy_contents(&mut bytes);
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// Writes `data` into a Float32Array from element `at`, as far as it reaches.
fn write_f32(cx: &mut Cx, array: &JsObject, at: usize, data: &[f32]) {
    let scope = cx.scope();
    let obj = v8::Local::new(scope, array.global());
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(obj) else {
        return;
    };
    let len = view.byte_length() / 4;
    let Some(buffer) = view.buffer(scope) else {
        return;
    };
    let store = buffer.get_backing_store();
    let base = view.byte_offset();
    for (i, v) in data.iter().enumerate().take(len.saturating_sub(at)) {
        for (k, b) in v.to_le_bytes().iter().enumerate() {
            store[base + (at + i) * 4 + k].set(*b);
        }
    }
}

/// A new Float32Array holding `data`.
fn new_f32_array(cx: &mut Cx, data: &[f32]) -> rt::Result<JsObject> {
    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
    let scope = cx.scope();
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
    let array = v8::Float32Array::new(scope, ab, 0, data.len()).ok_or(rt::Error::Pending)?;
    Ok(JsObject::new(scope, array.into()))
}

impl Engine {
    fn audio_ctx<R>(&self, h: Handle, f: impl FnOnce(&mut ContextData) -> R) -> Option<R> {
        match &mut *self.obj_mut(h) {
            Object::Audio(AudioObject::Context(c)) => Some(f(c)),
            _ => None,
        }
    }

    fn audio_node<R>(&self, h: Handle, f: impl FnOnce(&mut NodeData) -> R) -> Option<R> {
        match &mut *self.obj_mut(h) {
            Object::Audio(AudioObject::Node(n)) => Some(f(n)),
            _ => None,
        }
    }

    fn audio_param<R>(&self, h: Handle, f: impl FnOnce(&mut ParamData) -> R) -> Option<R> {
        match &mut *self.obj_mut(h) {
            Object::Audio(AudioObject::Param(p)) => Some(f(p)),
            _ => None,
        }
    }

    fn audio_buffer<R>(&self, h: Handle, f: impl FnOnce(&BufferObj) -> R) -> Option<R> {
        match &*self.obj(h) {
            Object::Audio(AudioObject::Buffer(b)) => Some(f(b)),
            _ => None,
        }
    }

    /// Queues a control message on the context's link.
    fn audio_send(&self, ctx: Handle, msg: Control) {
        self.audio_ctx(ctx, |c| c.link.send(msg));
    }

    fn context_time(&self, ctx: Handle) -> f64 {
        self.audio_ctx(ctx, |c| c.link.frame() as f64 / c.sample_rate as f64)
            .unwrap_or(0.0)
    }

    /// §1.3.1 the OfflineAudioContext constructor.
    fn new_offline_context(
        &self,
        cx: &mut Cx,
        channels: u32,
        length: u32,
        sample_rate: f32,
    ) -> rt::Result<Handle> {
        check_buffer_shape(channels, length, sample_rate)?;
        let global = self
            .current_global(cx)
            .ok_or_else(|| invalid_state("No global object"))?;
        let waker = self.waker();
        let link = Link::new(Box::new(move || waker.wake()));
        let ctx = self.alloc(Object::Audio(AudioObject::Context(Box::new(ContextData {
            sample_rate,
            channels,
            length,
            state: AudioContextState::Suspended,
            destination: Handle::new(1),
            link: ContextLink::Offline(link),
            rendering_started: false,
            next_node: 0,
            next_param: 0,
            nodes: Vec::new(),
            render_promise: None,
            suspends: BTreeMap::new(),
            global,
            waves: Default::default(),
            listener: None,
        }))));
        let dest = self.new_audio_node(
            ctx,
            NodeKind::Destination,
            Kind::Destination,
            (1, 1),
            ChannelConfig::new(channels, CountMode::Explicit, Interpretation::Speakers),
            &[],
        );
        self.audio_ctx(ctx, |c| c.destination = dest);
        Ok(ctx)
    }

    /// Makes a node of `ctx` with its AudioParams (description and initial value) and tells the
    /// rendering thread.
    fn new_audio_node(
        &self,
        ctx: Handle,
        kind: NodeKind,
        render: Kind,
        (inputs, outputs): (u32, u32),
        config: ChannelConfig,
        params: &[(ParamDesc, f32)],
    ) -> Handle {
        let (id, first_param) = self
            .audio_ctx(ctx, |c| {
                let id = c.next_node;
                c.next_node += 1;
                let p = c.next_param;
                c.next_param += params.len() as ParamId;
                (id, p)
            })
            .unwrap_or_default();
        let mut handles = Vec::with_capacity(params.len());
        let mut new = Vec::with_capacity(params.len());
        for (i, (desc, value)) in params.iter().enumerate() {
            let pid = first_param + i as ParamId;
            let shared = SharedValue::default();
            shared.store(desc.clamp(*value).to_bits(), Ordering::Relaxed);
            new.push(NewParam {
                id: pid,
                desc: *desc,
                value: *value,
                shared: shared.clone(),
            });
            handles.push(
                self.alloc(Object::Audio(AudioObject::Param(Box::new(ParamData {
                    context: ctx,
                    id: pid,
                    desc: *desc,
                    rate: desc.rate,
                    shared,
                    timeline: Timeline::default(),
                })))),
            );
        }
        let h = self.alloc(Object::Audio(AudioObject::Node(Box::new(NodeData {
            context: ctx,
            id,
            kind,
            inputs,
            outputs,
            config,
            params: handles,
            connections: Vec::new(),
            started: false,
            buffer: None,
            buffer_set: false,
            looping: false,
            loop_start: 0.0,
            loop_end: 0.0,
            osc_type: OscillatorType::Sine,
            reduction: None,
            extra: Extra::None,
        }))));
        self.audio_ctx(ctx, |c| c.nodes.push(h));
        self.audio_send(
            ctx,
            Control::AddNode {
                id,
                kind: render,
                inputs,
                outputs,
                config,
                params: new,
            },
        );
        h
    }

    /// The AudioNodeOptions of a constructor (§1.5.2 "AudioNode constructors": set each member
    /// given, as the attribute setters do).
    fn apply_node_options(
        &self,
        cx: &mut Cx,
        node: Handle,
        count: Option<u32>,
        mode: Option<ChannelCountMode>,
        interp: Option<ChannelInterpretation>,
    ) -> rt::Result<()> {
        if let Some(c) = count {
            AudioNodeImpl::set_channel_count(self, cx, node, c)?;
        }
        if let Some(m) = mode {
            AudioNodeImpl::set_channel_count_mode(self, cx, node, m)?;
        }
        if let Some(i) = interp {
            AudioNodeImpl::set_channel_interpretation(self, cx, node, i)?;
        }
        Ok(())
    }

    fn new_gain(&self, cx: &mut Cx, ctx: Handle, options: &GainOptions) -> rt::Result<Handle> {
        let h = self.new_audio_node(
            ctx,
            NodeKind::Gain,
            Kind::Gain,
            (1, 1),
            ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
            &[(ParamDesc::unbounded(1.0, Rate::A), options.gain)],
        );
        self.apply_node_options(
            cx,
            h,
            options.channel_count,
            options.channel_count_mode,
            options.channel_interpretation,
        )?;
        Ok(h)
    }

    fn new_constant_source(&self, ctx: Handle, offset: f32) -> Handle {
        self.new_audio_node(
            ctx,
            NodeKind::ConstantSource,
            Kind::ConstantSource(Schedule::default()),
            (0, 1),
            ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
            &[(ParamDesc::unbounded(1.0, Rate::A), offset)],
        )
    }

    fn new_buffer_source(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        options: &AudioBufferSourceOptions,
    ) -> rt::Result<Handle> {
        let k_rate = |d| ParamDesc::unbounded(d, Rate::K).fixed();
        let h = self.new_audio_node(
            ctx,
            NodeKind::BufferSource,
            Kind::BufferSource(Box::default()),
            (0, 1),
            ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
            &[
                (k_rate(1.0), options.playback_rate),
                (k_rate(0.0), options.detune),
            ],
        );
        if options.buffer.is_some() {
            AudioBufferSourceNodeImpl::set_buffer(self, cx, h, options.buffer)?;
        }
        self.audio_node(h, |n| {
            n.looping = options.loop_;
            n.loop_start = options.loop_start;
            n.loop_end = options.loop_end;
        });
        self.send_loop(h);
        Ok(h)
    }

    fn send_loop(&self, node: Handle) {
        if let Some((ctx, id, looping, start, end)) = self.audio_node(node, |n| {
            (n.context, n.id, n.looping, n.loop_start, n.loop_end)
        }) {
            self.audio_send(
                ctx,
                Control::SetLoop {
                    node: id,
                    looping,
                    start,
                    end,
                },
            );
        }
    }

    /// §1.9 "acquire the content" of an AudioBuffer: a copy for the rendering thread.
    fn acquire_buffer(&self, cx: &mut Cx, buffer: Handle) -> Option<Arc<BufferData>> {
        let (sample_rate, length, arrays) =
            self.audio_buffer(buffer, |b| (b.sample_rate, b.length, b.channels.clone()))?;
        let channels: Vec<Vec<f32>> = arrays.iter().map(|a| read_f32(cx, a)).collect();
        // "Acquire the content": a detached channel makes every channel empty.
        if channels.iter().any(|c| c.len() != length as usize) {
            return Some(Arc::new(BufferData {
                sample_rate,
                length: 0,
                channels: vec![Vec::new(); channels.len()],
            }));
        }
        Some(Arc::new(BufferData {
            sample_rate,
            length: length as usize,
            channels,
        }))
    }

    /// A new AudioBuffer: zeroed channels, or `data`.
    fn new_audio_buffer(
        &self,
        cx: &mut Cx,
        channels: u32,
        length: u32,
        sample_rate: f32,
        data: Option<Vec<Vec<f32>>>,
    ) -> rt::Result<Handle> {
        check_buffer_shape(channels, length, sample_rate)?;
        let mut arrays = Vec::with_capacity(channels as usize);
        match data {
            Some(data) => {
                for c in &data {
                    arrays.push(new_f32_array(cx, c)?);
                }
            }
            None => {
                let zeros = vec![0.0f32; length as usize];
                for _ in 0..channels {
                    arrays.push(new_f32_array(cx, &zeros)?);
                }
            }
        }
        Ok(
            self.alloc(Object::Audio(AudioObject::Buffer(Box::new(BufferObj {
                sample_rate,
                length,
                channels: arrays,
            })))),
        )
    }

    /// Whether a rendering is under way that will report back (it keeps the event loop busy).
    pub(crate) fn audio_pending(&self) -> bool {
        let st = self.audio.borrow();
        if !st.decoding.is_empty() {
            return true;
        }
        st.rendering.iter().chain(&st.realtime).any(|h| {
            self.audio_ctx(*h, |c| match &c.link {
                ContextLink::Offline(l) => l.busy(),
                ContextLink::Realtime(r) => r.link.busy(),
            })
            .unwrap_or(false)
        })
    }

    /// Turns the rendering threads' notices into media element tasks. Returns whether there were
    /// any.
    pub(crate) fn poll_audio(&self, _cx: &mut Cx) -> bool {
        let mut any = self.poll_decoding();
        let contexts = self.audio.borrow().rendering.clone();
        for ctx in contexts {
            let Some(notices) = self
                .audio_ctx(ctx, |c| c.link.offline().map(|l| l.take_notices()))
                .flatten()
            else {
                continue;
            };
            for n in notices {
                any = true;
                if matches!(n, OfflineNotice::Complete(_)) {
                    self.audio.borrow_mut().rendering.retain(|h| *h != ctx);
                }
                self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                    e.audio_notice(cx, ctx, n)
                });
            }
        }
        let contexts = self.audio.borrow().realtime.clone();
        for ctx in contexts {
            let notices = self
                .audio_ctx(ctx, |c| match &c.link {
                    ContextLink::Realtime(r) => r.link.take_notices(),
                    ContextLink::Offline(_) => Vec::new(),
                })
                .unwrap_or_default();
            for n in notices {
                any = true;
                if n == RealtimeNotice::Closed {
                    self.audio.borrow_mut().realtime.retain(|h| *h != ctx);
                }
                self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                    e.realtime_notice(cx, ctx, n)
                });
            }
        }
        any
    }

    fn audio_notice(&self, cx: &mut Cx, ctx: Handle, notice: OfflineNotice) {
        let Some(global) = self.audio_ctx(ctx, |c| c.global) else {
            return;
        };
        self.in_global(cx, global, |cx| match notice {
            OfflineNotice::Graph(Notice::Ended(id)) => {
                if let Some(node) = self
                    .audio_ctx(ctx, |c| c.nodes.get(id as usize).copied())
                    .flatten()
                {
                    self.fire_event(cx, node, "ended", false, false);
                }
            }
            OfflineNotice::Suspended(frame) => {
                // The context is suspended, then the suspend() promise resolves.
                self.set_audio_state(ctx, AudioContextState::Suspended);
                if let Some(p) = self.audio_ctx(ctx, |c| c.suspends.remove(&frame)).flatten() {
                    p.resolve(cx, ());
                }
            }
            OfflineNotice::Complete(data) => {
                let (channels, length, sample_rate, promise) = self
                    .audio_ctx(ctx, |c| {
                        (c.channels, c.length, c.sample_rate, c.render_promise.take())
                    })
                    .unwrap_or_default();
                let buffer =
                    match self.new_audio_buffer(cx, channels, length, sample_rate, Some(data)) {
                        Ok(b) => b,
                        Err(e) => {
                            if let Some(p) = promise {
                                p.reject(cx, e);
                            }
                            return;
                        }
                    };
                // §1.3.4: resolve the promise, then fire `complete`; the context is closed.
                if let Some(p) = promise {
                    p.resolve(cx, buffer);
                }
                self.set_audio_state(ctx, AudioContextState::Closed);
                self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                    e.in_global(cx, global, |cx| {
                        let ev = e.trusted_event(
                            InterfaceId::OfflineAudioCompletionEvent,
                            "complete",
                            false,
                            false,
                            EventExt::AudioCompletion(buffer),
                        );
                        e.dispatch(cx, ev, ctx, false);
                    });
                });
            }
        });
    }

    /// Sets the context's [[control thread state]] and queues `statechange` when it changed.
    fn set_audio_state(&self, ctx: Handle, state: AudioContextState) {
        let Some((changed, global)) = self.audio_ctx(ctx, |c| {
            let changed = c.state != state && c.state != AudioContextState::Closed;
            if changed {
                c.state = state;
            }
            (changed, c.global)
        }) else {
            return;
        };
        if changed {
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                e.in_global(cx, global, |cx| {
                    e.fire_event(cx, ctx, "statechange", false, false);
                });
            });
        }
    }

    fn param_desc(&self, p: Handle) -> ParamDesc {
        self.audio_param(p, |p| p.desc)
            .unwrap_or(ParamDesc::unbounded(0.0, Rate::A))
    }

    /// Inserts an automation event (§1.6.2): the overlap check, then the control thread's copy
    /// and the rendering thread's.
    fn param_insert(&self, p: Handle, ev: Event) -> rt::Result<Handle> {
        let (ctx, id) = self
            .audio_param(p, |d| (d.context, d.id))
            .ok_or_else(|| invalid_state("Not an AudioParam"))?;
        let now = self.context_time(ctx);
        let mut ev = ev;
        // Times before the current time are clamped to it.
        if ev.time < now {
            ev.time = now;
        }
        self.audio_param(p, |d| {
            d.timeline
                .check(&ev)
                .map_err(|_| not_supported("The event overlaps a value curve"))?;
            d.timeline.insert(ev.clone(), now, 0.0);
            Ok::<_, rt::Error>(())
        })
        .unwrap_or(Ok(()))?;
        self.audio_send(
            ctx,
            Control::Param {
                param: id,
                op: ParamOp::Insert(ev),
            },
        );
        Ok(p)
    }

    /// Disconnects the node's connections that `keep` rejects.
    fn disconnect_where(&self, node: Handle, keep: impl Fn(u32, &Target) -> bool) -> usize {
        let Some((ctx, id, gone)) = self.audio_node(node, |n| {
            let mut gone = Vec::new();
            n.connections.retain(|(o, t)| {
                let k = keep(*o, t);
                if !k {
                    gone.push((*o, *t));
                }
                k
            });
            (n.context, n.id, gone)
        }) else {
            return 0;
        };
        for (output, t) in &gone {
            let msg = match *t {
                Target::Node(to, input) => {
                    let Some(to) = self.audio_node(to, |n| n.id) else {
                        continue;
                    };
                    Control::Disconnect {
                        from: id,
                        output: *output,
                        to,
                        input,
                    }
                }
                Target::Param(p) => {
                    let Some(param) = self.audio_param(p, |p| p.id) else {
                        continue;
                    };
                    Control::DisconnectParam {
                        from: id,
                        output: *output,
                        param,
                    }
                }
            };
            self.audio_send(ctx, msg);
        }
        gone.len()
    }

    fn check_output(&self, node: Handle, output: u32) -> rt::Result<()> {
        let outputs = self.audio_node(node, |n| n.outputs).unwrap_or(0);
        if output >= outputs {
            return Err(index_size("The output index is out of range"));
        }
        Ok(())
    }

    fn node_context(&self, node: Handle) -> Option<Handle> {
        self.audio_node(node, |n| n.context)
    }
}

impl BaseAudioContextImpl for Engine {
    fn destination(&self, this: Handle) -> Handle {
        self.audio_ctx(this, |c| c.destination).unwrap_or(this)
    }

    fn sample_rate(&self, this: Handle) -> f32 {
        self.audio_ctx(this, |c| c.sample_rate).unwrap_or(0.0)
    }

    fn current_time(&self, this: Handle) -> f64 {
        self.context_time(this)
    }

    fn state(&self, this: Handle) -> AudioContextState {
        self.audio_ctx(this, |c| c.state)
            .unwrap_or(AudioContextState::Closed)
    }

    fn create_buffer(
        &self,
        cx: &mut Cx,
        _this: Handle,
        number_of_channels: u32,
        length: u32,
        sample_rate: f32,
    ) -> rt::Result<Handle> {
        self.new_audio_buffer(cx, number_of_channels, length, sample_rate, None)
    }

    fn create_buffer_source(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.new_buffer_source(
            cx,
            this,
            &AudioBufferSourceOptions {
                buffer: None,
                detune: 0.0,
                loop_: false,
                loop_end: 0.0,
                loop_start: 0.0,
                playback_rate: 1.0,
            },
        )
    }

    fn create_constant_source(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.new_constant_source(this, 1.0))
    }

    fn create_gain(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.new_gain(
            cx,
            this,
            &GainOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                gain: 1.0,
            },
        )
    }

    fn create_oscillator(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.new_oscillator(
            cx,
            this,
            &OscillatorOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                detune: 0.0,
                frequency: 440.0,
                periodic_wave: None,
                type_: OscillatorType::Sine,
            },
        )
    }

    fn create_periodic_wave(
        &self,
        _cx: &mut Cx,
        this: Handle,
        real: Vec<f32>,
        imag: Vec<f32>,
        constraints: PeriodicWaveConstraints,
    ) -> rt::Result<Handle> {
        self.new_periodic_wave(
            this,
            Some(real),
            Some(imag),
            constraints.disable_normalization,
        )
    }

    fn listener(&self, this: Handle) -> Handle {
        self.audio_listener(this)
    }

    fn create_analyser(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.new_analyser(
            cx,
            this,
            &AnalyserOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                fft_size: 2048,
                max_decibels: -30.0,
                min_decibels: -100.0,
                smoothing_time_constant: 0.8,
            },
        )
    }

    fn create_biquad_filter(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.new_biquad(
            cx,
            this,
            &BiquadFilterOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                q: 1.0,
                detune: 0.0,
                frequency: 350.0,
                gain: 0.0,
                type_: BiquadFilterType::Lowpass,
            },
        )
    }

    fn create_channel_merger(
        &self,
        cx: &mut Cx,
        this: Handle,
        number_of_inputs: u32,
    ) -> rt::Result<Handle> {
        self.new_merger(
            cx,
            this,
            &ChannelMergerOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                number_of_inputs,
            },
        )
    }

    fn create_channel_splitter(
        &self,
        cx: &mut Cx,
        this: Handle,
        number_of_outputs: u32,
    ) -> rt::Result<Handle> {
        self.new_splitter(
            cx,
            this,
            &ChannelSplitterOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                number_of_outputs,
            },
        )
    }

    fn create_convolver(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.new_convolver(
            cx,
            this,
            &ConvolverOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                buffer: None,
                disable_normalization: false,
            },
        )
    }

    fn create_delay(&self, cx: &mut Cx, this: Handle, max_delay_time: f64) -> rt::Result<Handle> {
        self.new_delay(
            cx,
            this,
            &DelayOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                delay_time: 0.0,
                max_delay_time,
            },
        )
    }

    fn create_iir_filter(
        &self,
        cx: &mut Cx,
        this: Handle,
        feedforward: Vec<f64>,
        feedback: Vec<f64>,
    ) -> rt::Result<Handle> {
        self.new_iir(
            cx,
            this,
            &IIRFilterOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                feedback,
                feedforward,
            },
        )
    }

    fn create_panner(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.new_panner(
            cx,
            this,
            &PannerOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                cone_inner_angle: 360.0,
                cone_outer_angle: 360.0,
                cone_outer_gain: 0.0,
                distance_model: DistanceModelType::Inverse,
                max_distance: 10000.0,
                orientation_x: 1.0,
                orientation_y: 0.0,
                orientation_z: 0.0,
                panning_model: PanningModelType::Equalpower,
                position_x: 0.0,
                position_y: 0.0,
                position_z: 0.0,
                ref_distance: 1.0,
                rolloff_factor: 1.0,
            },
        )
    }

    fn create_stereo_panner(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.new_stereo_panner(
            cx,
            this,
            &StereoPannerOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                pan: 0.0,
            },
        )
    }

    fn create_wave_shaper(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.new_shaper(
            cx,
            this,
            &WaveShaperOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                curve: None,
                oversample: OverSampleType::None,
            },
        )
    }

    fn create_dynamics_compressor(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.new_compressor(
            cx,
            this,
            &DynamicsCompressorOptions {
                channel_count: None,
                channel_count_mode: None,
                channel_interpretation: None,
                attack: 0.003,
                knee: 30.0,
                ratio: 12.0,
                release: 0.25,
                threshold: -24.0,
            },
        )
    }

    /// §1.1.3 `decodeAudioData()`: the data is detached and decoded off the control thread, then
    /// resampled to the context's rate; a task settles the promise and calls the callback.
    fn decode_audio_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        audio_data: JsObject,
        success_callback: Option<DecodeSuccessCallback>,
        error_callback: Option<DecodeErrorCallback>,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        // 1. The context's document must be fully active.
        let global = self.audio_ctx(this, |c| c.global);
        let active = global
            .and_then(|g| self.associated_document(g))
            .is_some_and(|d| self.is_fully_active(d));
        if !active {
            p.reject(cx, invalid_state("The document is not fully active"));
            return Ok(p);
        }
        // 2-3. A detached buffer is a DataCloneError; otherwise its bytes are taken and it is
        // detached.
        let bytes = {
            let scope = cx.scope();
            let obj = v8::Local::new(scope, audio_data.global());
            match v8::Local::<v8::ArrayBuffer>::try_from(obj) {
                Ok(ab) if ab.was_detached() || !ab.is_detachable() => None,
                Ok(ab) => {
                    let len = ab.byte_length();
                    let store = ab.get_backing_store();
                    let bytes: Vec<u8> = store.iter().take(len).map(|c| c.get()).collect();
                    ab.detach(None).map(|_| bytes)
                }
                Err(_) => None,
            }
        };
        let Some(bytes) = bytes else {
            p.reject(
                cx,
                exception(ExceptionName::DataCloneError, "The ArrayBuffer is detached"),
            );
            return Ok(p);
        };
        let rate = self.audio_ctx(this, |c| c.sample_rate).unwrap_or(44100.0);
        let waker = self.waker();
        let job = DecodeJob::spawn(
            bytes,
            rate,
            decode_fallback(),
            Box::new(move || waker.wake()),
        );
        self.audio.borrow_mut().decoding.push(Decoding {
            job,
            context: this,
            promise: p.clone(),
            success: success_callback,
            error: error_callback,
        });
        Ok(p)
    }
}

/// The media stack's audio decoder, for what is not a WAVE file.
fn decode_fallback() -> Option<snowlark_audio::decode::Fallback> {
    snowlark_media::decode_audio_fallback()
}

impl Engine {
    /// Settles the `decodeAudioData()` calls whose decoding finished. Returns whether any did.
    fn poll_decoding(&self) -> bool {
        let mut done = Vec::new();
        self.audio
            .borrow_mut()
            .decoding
            .retain(|d| match d.job.take() {
                Some(result) => {
                    done.push((
                        result,
                        d.context,
                        d.promise.clone(),
                        d.success.clone(),
                        d.error.clone(),
                    ));
                    false
                }
                None => true,
            });
        let any = !done.is_empty();
        for (result, ctx, p, success, error) in done {
            let global = self.audio_ctx(ctx, |c| c.global);
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                let run = |cx: &mut Cx| {
                    let buffer = result.and_then(|d| {
                        let length = d.channels[0].len() as u32;
                        let channels = d.channels.len() as u32;
                        e.new_audio_buffer(cx, channels, length, d.sample_rate, Some(d.channels))
                            .ok()
                    });
                    match buffer {
                        Some(b) => {
                            p.resolve(cx, b);
                            if let Some(cb) = &success {
                                let _ = cb.invoke(cx, None, b);
                            }
                        }
                        None => {
                            let h = e.alloc(Object::DomException {
                                name: "EncodingError".to_owned(),
                                message: "Unable to decode audio data".to_owned(),
                            });
                            let err = {
                                let scope = cx.scope();
                                match rt::wrap(scope, h) {
                                    Some(w) => JsValue::new(scope, w.into()),
                                    None => rt::undefined_value(scope),
                                }
                            };
                            p.reject_value(cx, &err);
                            if let Some(cb) = &error {
                                let _ = cb.invoke(cx, None, h);
                            }
                        }
                    }
                };
                match global {
                    Some(g) => e.in_global(cx, g, run),
                    None => run(cx),
                }
            });
        }
        any
    }
}

/// §1.2.4 `latencyHint`.
fn latency_hint(h: &AudioContextLatencyCategoryOrDouble) -> LatencyHint {
    match h {
        AudioContextLatencyCategoryOrDouble::AudioContextLatencyCategory(c) => match c {
            AudioContextLatencyCategory::Balanced => LatencyHint::Balanced,
            AudioContextLatencyCategory::Interactive => LatencyHint::Interactive,
            AudioContextLatencyCategory::Playback => LatencyHint::Playback,
        },
        AudioContextLatencyCategoryOrDouble::Double(s) => LatencyHint::Seconds(*s),
    }
}

impl Engine {
    /// The output an AudioContext renders to, and what it offers: the embedder's default output
    /// device, through the tab's group (its mute and audible state) in a window. Without one (no
    /// outputs given, as in headless runs, or no device) a null sink, which consumes the audio at
    /// the clock rate and drops it.
    fn audio_output(&self) -> (DeviceInfo, Box<dyn AudioSink>) {
        let (outputs, group) = {
            let m = self.media.borrow();
            (m.config.audio_out.clone(), m.config.audio.clone())
        };
        let Some((outputs, device)) = outputs.and_then(|o| o.device().map(|d| (o, d))) else {
            return (NullSink::DEVICE, Box::new(NullSink::default()));
        };
        let sink = outputs.open();
        match group {
            Some(g) => (
                device,
                Box::new(snowlark_media::audio::GroupSink::new(sink, g)),
            ),
            None => (device, sink),
        }
    }

    fn audio_rt<R>(&self, ctx: Handle, f: impl FnOnce(&mut Realtime) -> R) -> Option<R> {
        self.audio_ctx(ctx, |c| match &mut c.link {
            ContextLink::Realtime(r) => Some(f(r)),
            ContextLink::Offline(_) => None,
        })
        .flatten()
    }

    /// §1.2.1 the AudioContext constructor.
    fn new_realtime_context(
        &self,
        cx: &mut Cx,
        options: &AudioContextOptions,
    ) -> rt::Result<Handle> {
        let global = self
            .current_global(cx)
            .ok_or_else(|| invalid_state("No global object"))?;
        // 1. The document must be fully active.
        let active = self
            .associated_document(global)
            .is_some_and(|d| self.is_fully_active(d));
        if !active {
            return Err(invalid_state("The document is not fully active"));
        }
        let (device, sink) = self.audio_output();
        // The sample rate asked for (an output at another rate resamples), or the output's.
        let sample_rate = match options.sample_rate {
            Some(r) if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&r) => {
                return Err(not_supported(
                    "The sample rate is outside the supported range",
                ))
            }
            Some(r) => r,
            None => device.sample_rate,
        };
        // The internal latency from the hint: the output's period, in whole render quanta.
        let period_frames = device.period_frames(latency_hint(&options.latency_hint), sample_rate);
        let waker = self.waker();
        let link = realtime::Link::new(Box::new(move || waker.wake()));
        let ctx = self.alloc(Object::Audio(AudioObject::Context(Box::new(ContextData {
            sample_rate,
            channels: device.channels,
            length: 0,
            state: AudioContextState::Suspended,
            destination: Handle::new(1),
            link: ContextLink::Realtime(Box::new(Realtime {
                link: link.clone(),
                control: AudioContextState::Suspended,
                suspended_by_user: false,
                pending_resume: Vec::new(),
                close_promise: None,
                next_suspend: 0,
                base_latency: period_frames as f64 / sample_rate as f64,
                output_latency: device.output_latency,
            })),
            rendering_started: false,
            next_node: 0,
            next_param: 0,
            nodes: Vec::new(),
            render_promise: None,
            suspends: BTreeMap::new(),
            global,
            waves: Default::default(),
            listener: None,
        }))));
        let dest = self.new_audio_node(
            ctx,
            NodeKind::Destination,
            Kind::Destination,
            (1, 1),
            ChannelConfig::new(
                device.channels.min(2),
                CountMode::Explicit,
                Interpretation::Speakers,
            ),
            &[],
        );
        self.audio_ctx(ctx, |c| c.destination = dest);
        let config = StreamConfig {
            sample_rate,
            channels: device.channels,
            period_frames,
        };
        realtime::start(Graph::new(sample_rate), sink, config, link)
            .map_err(|_| not_supported("The rendering thread could not start"))?;
        self.audio.borrow_mut().realtime.push(ctx);
        // If the context is allowed to start, start processing.
        if self.audio_allowed_to_start(ctx) {
            self.audio_start_rendering(ctx);
        }
        Ok(ctx)
    }

    /// §1.2 "allowed to start": the autoplay policy lets everything start (test runs), or the
    /// context's document has sticky activation, as for media elements with sound.
    pub(crate) fn audio_allowed_to_start(&self, ctx: Handle) -> bool {
        if self.media.borrow().config.autoplay == AutoplayPolicy::Allowed {
            return true;
        }
        self.audio_ctx(ctx, |c| c.global)
            .and_then(|g| self.associated_document(g))
            .is_some_and(|d| self.has_sticky_activation(d))
    }

    /// The control message to start (or resume) processing.
    fn audio_start_rendering(&self, ctx: Handle) {
        self.audio_rt(ctx, |r| {
            r.control = AudioContextState::Running;
            r.link.command(Command::Start);
        });
    }

    /// A source node of `ctx` started: an AudioContext that waits to be allowed to start, and that
    /// script did not suspend, starts now if it is allowed to (its pending `resume()` promises
    /// resolve once it runs).
    fn audio_source_started(&self, ctx: Handle) {
        let waiting = self
            .audio_rt(ctx, |r| {
                r.control == AudioContextState::Suspended && !r.suspended_by_user
            })
            .unwrap_or(false);
        if waiting && self.audio_allowed_to_start(ctx) {
            self.audio_start_rendering(ctx);
        }
    }

    /// The AudioContexts of `window` close with its document (§1.2.3 `close()`, "part of the
    /// document's unloading"): their rendering ends and nothing more reaches the page.
    pub(crate) fn audio_window_unloaded(&self, window: Handle) {
        let contexts = self.audio.borrow().realtime.clone();
        for ctx in contexts {
            if self.audio_ctx(ctx, |c| c.global) != Some(window) {
                continue;
            }
            self.audio_rt(ctx, |r| {
                r.control = AudioContextState::Closed;
                r.link.stop();
                r.pending_resume.clear();
                r.close_promise = None;
            });
            self.audio_ctx(ctx, |c| {
                c.state = AudioContextState::Closed;
                c.suspends.clear();
            });
            self.audio.borrow_mut().realtime.retain(|h| *h != ctx);
        }
    }

    /// The steps of a promise-returning AudioContext method on a document not fully active.
    fn audio_check_active(&self, ctx: Handle) -> rt::Result<()> {
        let active = self
            .audio_ctx(ctx, |c| c.global)
            .and_then(|g| self.associated_document(g))
            .is_some_and(|d| self.is_fully_active(d));
        if active {
            Ok(())
        } else {
            Err(invalid_state("The document is not fully active"))
        }
    }

    /// An AudioContext's rendering thread answered (the media element tasks of §1.2.3).
    fn realtime_notice(&self, cx: &mut Cx, ctx: Handle, notice: RealtimeNotice) {
        let Some(global) = self.audio_ctx(ctx, |c| c.global) else {
            return;
        };
        self.in_global(cx, global, |cx| match notice {
            RealtimeNotice::Graph(Notice::Ended(id)) => {
                if let Some(node) = self
                    .audio_ctx(ctx, |c| c.nodes.get(id as usize).copied())
                    .flatten()
                {
                    self.fire_event(cx, node, "ended", false, false);
                }
            }
            RealtimeNotice::Running => {
                // The pending resume promises resolve in order; then the state is running.
                let pending = self
                    .audio_rt(ctx, |r| std::mem::take(&mut r.pending_resume))
                    .unwrap_or_default();
                for p in pending {
                    p.resolve(cx, ());
                }
                self.set_audio_state(ctx, AudioContextState::Running);
            }
            RealtimeNotice::Suspended(tag) => {
                if let Some(p) = self.audio_ctx(ctx, |c| c.suspends.remove(&tag)).flatten() {
                    p.resolve(cx, ());
                }
                self.set_audio_state(ctx, AudioContextState::Suspended);
            }
            RealtimeNotice::Closed => {
                if let Some(p) = self.audio_rt(ctx, |r| r.close_promise.take()).flatten() {
                    p.resolve(cx, ());
                }
                self.set_audio_state(ctx, AudioContextState::Closed);
            }
            RealtimeNotice::Failed(_) => {
                // The output did not start, or failed: the pending resume promises are rejected
                // and an `error` event fires at the context.
                let pending = self
                    .audio_rt(ctx, |r| {
                        if r.control == AudioContextState::Running {
                            r.control = AudioContextState::Suspended;
                        }
                        std::mem::take(&mut r.pending_resume)
                    })
                    .unwrap_or_default();
                for p in pending {
                    p.reject(cx, invalid_state("The audio output could not start"));
                }
                self.set_audio_state(ctx, AudioContextState::Suspended);
                self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                    e.in_global(cx, global, |cx| {
                        e.fire_event(cx, ctx, "error", false, false);
                    });
                });
            }
        });
    }
}

impl AudioContextImpl for Engine {
    fn constructor(&self, cx: &mut Cx, context_options: AudioContextOptions) -> rt::Result<Handle> {
        self.new_realtime_context(cx, &context_options)
    }

    /// The processing latency between the destination and the output: one period.
    fn base_latency(&self, this: Handle) -> f64 {
        self.audio_rt(this, |r| r.base_latency).unwrap_or(0.0)
    }

    fn output_latency(&self, this: Handle) -> f64 {
        self.audio_rt(this, |r| r.output_latency).unwrap_or(0.0)
    }

    /// §1.2.3 `getOutputTimestamp()`: the first frame of the last period handed to the output and
    /// when it leaves the output; zeros before the first.
    fn get_output_timestamp(&self, _cx: &mut Cx, this: Handle) -> rt::Result<AudioTimestamp> {
        let (sample_rate, position) = self
            .audio_ctx(this, |c| {
                let position = match &c.link {
                    ContextLink::Realtime(r) => r.link.position(),
                    ContextLink::Offline(_) => None,
                };
                (c.sample_rate as f64, position)
            })
            .unwrap_or((1.0, None));
        Ok(match position {
            Some((frame, at)) => AudioTimestamp {
                context_time: Some(frame as f64 / sample_rate),
                performance_time: Some(self.high_res_time(at)),
            },
            None => AudioTimestamp {
                context_time: Some(0.0),
                performance_time: Some(0.0),
            },
        })
    }

    /// §1.2.3 `resume()`.
    fn resume(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.audio_check_active(this)?;
        let control = self
            .audio_rt(this, |r| r.control)
            .ok_or_else(|| invalid_state("Not an AudioContext"))?;
        if control == AudioContextState::Closed {
            return Err(invalid_state("The context is closed"));
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        self.audio_rt(this, |r| {
            r.suspended_by_user = false;
            r.pending_resume.push(p.clone());
        });
        // Not allowed to start: the promise waits with the pending resume promises.
        if self.audio_allowed_to_start(this) {
            self.audio_start_rendering(this);
        }
        Ok(p)
    }

    /// §1.2.3 `suspend()`.
    fn suspend(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.audio_check_active(this)?;
        let control = self
            .audio_rt(this, |r| r.control)
            .ok_or_else(|| invalid_state("Not an AudioContext"))?;
        if control == AudioContextState::Closed {
            return Err(invalid_state("The context is closed"));
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let tag = self
            .audio_rt(this, |r| {
                r.suspended_by_user = true;
                r.control = AudioContextState::Suspended;
                let tag = r.next_suspend;
                r.next_suspend += 1;
                r.link.command(Command::Suspend(tag));
                tag
            })
            .unwrap_or_default();
        self.audio_ctx(this, |c| c.suspends.insert(tag, p.clone()));
        Ok(p)
    }

    /// §1.2.3 `close()`.
    fn close(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.audio_check_active(this)?;
        let control = self
            .audio_rt(this, |r| r.control)
            .ok_or_else(|| invalid_state("Not an AudioContext"))?;
        if control == AudioContextState::Closed {
            return Err(invalid_state("The context is already closed"));
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let pending = self
            .audio_rt(this, |r| {
                r.control = AudioContextState::Closed;
                r.close_promise = Some(p.clone());
                r.link.command(Command::Close);
                std::mem::take(&mut r.pending_resume)
            })
            .unwrap_or_default();
        // A closed context never runs: the resume() promises still waiting are rejected.
        for q in pending {
            q.reject(cx, invalid_state("The context was closed"));
        }
        Ok(p)
    }
}

impl OfflineAudioContextImpl for Engine {
    fn constructor_context_options(
        &self,
        cx: &mut Cx,
        options: OfflineAudioContextOptions,
    ) -> rt::Result<Handle> {
        let length = options
            .length
            .ok_or_else(|| rt::Error::Type("The length is required".into()))?;
        self.new_offline_context(cx, options.number_of_channels, length, options.sample_rate)
    }

    fn constructor_number_of_channels_length_sample_rate(
        &self,
        cx: &mut Cx,
        number_of_channels: u32,
        length: u32,
        sample_rate: f32,
    ) -> rt::Result<Handle> {
        self.new_offline_context(cx, number_of_channels, length, sample_rate)
    }

    fn length(&self, this: Handle) -> Option<u32> {
        self.audio_ctx(this, |c| c.length)
    }

    /// §1.3.4 `startRendering()`.
    fn start_rendering(
        &self,
        cx: &mut Cx,
        this: Handle,
        _chunk_size: Option<u32>,
    ) -> rt::Result<JsPromise> {
        let (started, state, sample_rate, channels, length, link) = self
            .audio_ctx(this, |c| {
                Some((
                    c.rendering_started,
                    c.state,
                    c.sample_rate,
                    c.channels,
                    c.length,
                    c.link.offline()?,
                ))
            })
            .flatten()
            .ok_or_else(|| invalid_state("Not an OfflineAudioContext"))?;
        if started || state != AudioContextState::Suspended {
            return Err(invalid_state("Rendering has already started"));
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        self.audio_ctx(this, |c| {
            c.rendering_started = true;
            c.render_promise = Some(p.clone());
        });
        self.set_audio_state(this, AudioContextState::Running);
        if offline::start(
            Graph::new(sample_rate),
            channels as usize,
            length as u64,
            link,
        )
        .is_err()
        {
            p.reject(cx, not_supported("The rendering thread could not start"));
            return Ok(p);
        }
        self.audio.borrow_mut().rendering.push(this);
        Ok(p)
    }

    /// §1.3.4 `resume()`.
    fn resume(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let (started, state, link) = self
            .audio_ctx(this, |c| {
                Some((c.rendering_started, c.state, c.link.offline()?))
            })
            .flatten()
            .ok_or_else(|| invalid_state("Not an OfflineAudioContext"))?;
        if !started {
            return Err(invalid_state("Rendering has not started"));
        }
        if state == AudioContextState::Closed {
            return Err(invalid_state("The context is closed"));
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        if state != AudioContextState::Running {
            self.set_audio_state(this, AudioContextState::Running);
            link.resume();
        }
        p.resolve(cx, ());
        Ok(p)
    }

    /// §1.3.4 `suspend(suspendTime)`: at the render quantum boundary at or after the time.
    fn suspend(&self, cx: &mut Cx, this: Handle, suspend_time: f64) -> rt::Result<JsPromise> {
        let (state, sample_rate, length, link) = self
            .audio_ctx(this, |c| {
                Some((c.state, c.sample_rate, c.length, c.link.offline()?))
            })
            .flatten()
            .ok_or_else(|| invalid_state("Not an OfflineAudioContext"))?;
        if state == AudioContextState::Closed {
            return Err(invalid_state("The rendering is finished"));
        }
        if suspend_time < 0.0 {
            return Err(invalid_state("The suspend time is negative"));
        }
        if suspend_time >= length as f64 / sample_rate as f64 {
            return Err(invalid_state(
                "The suspend time is at or after the end of the rendering",
            ));
        }
        let q = RENDER_QUANTUM as u64;
        let frame = (suspend_time * sample_rate as f64) as u64;
        let frame = frame.div_ceil(q) * q;
        match link.add_suspend(frame) {
            Err(SuspendError::Past) => {
                return Err(invalid_state("The suspend time has already passed"))
            }
            Err(SuspendError::Taken) => {
                return Err(invalid_state("A suspend is already scheduled at that time"))
            }
            Ok(()) => {}
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        self.audio_ctx(this, |c| c.suspends.insert(frame, p.clone()));
        Ok(p)
    }
}

impl OfflineAudioCompletionEventImpl for Engine {
    fn rendered_buffer(&self, this: Handle) -> Handle {
        self.ev(this, |e| match e.ext {
            EventExt::AudioCompletion(b) => b,
            _ => this,
        })
    }

    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: OfflineAudioCompletionEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::OfflineAudioCompletionEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::AudioCompletion(init.rendered_buffer),
        ))
    }
}

impl AudioBufferImpl for Engine {
    fn constructor(&self, cx: &mut Cx, options: AudioBufferOptions) -> rt::Result<Handle> {
        self.new_audio_buffer(
            cx,
            options.number_of_channels,
            options.length,
            options.sample_rate,
            None,
        )
    }

    fn sample_rate(&self, this: Handle) -> f32 {
        self.audio_buffer(this, |b| b.sample_rate).unwrap_or(0.0)
    }

    fn length(&self, this: Handle) -> u32 {
        self.audio_buffer(this, |b| b.length).unwrap_or(0)
    }

    fn duration(&self, this: Handle) -> f64 {
        self.audio_buffer(this, |b| b.length as f64 / b.sample_rate as f64)
            .unwrap_or(0.0)
    }

    fn number_of_channels(&self, this: Handle) -> u32 {
        self.audio_buffer(this, |b| b.channels.len() as u32)
            .unwrap_or(0)
    }

    fn get_channel_data(&self, _cx: &mut Cx, this: Handle, channel: u32) -> rt::Result<JsObject> {
        self.audio_buffer(this, |b| b.channels.get(channel as usize).cloned())
            .flatten()
            .ok_or_else(|| index_size("The channel index is out of range"))
    }

    /// §1.4 `copyFromChannel()`.
    fn copy_from_channel(
        &self,
        cx: &mut Cx,
        this: Handle,
        destination: JsObject,
        channel_number: u32,
        buffer_offset: u32,
    ) -> rt::Result<()> {
        let src = self
            .audio_buffer(this, |b| b.channels.get(channel_number as usize).cloned())
            .flatten()
            .ok_or_else(|| index_size("The channel index is out of range"))?;
        let data = read_f32(cx, &src);
        let from = (buffer_offset as usize).min(data.len());
        write_f32(cx, &destination, 0, &data[from..]);
        Ok(())
    }

    /// §1.4 `copyToChannel()`.
    fn copy_to_channel(
        &self,
        cx: &mut Cx,
        this: Handle,
        source: JsObject,
        channel_number: u32,
        buffer_offset: u32,
    ) -> rt::Result<()> {
        let dst = self
            .audio_buffer(this, |b| b.channels.get(channel_number as usize).cloned())
            .flatten()
            .ok_or_else(|| index_size("The channel index is out of range"))?;
        let data = read_f32(cx, &source);
        write_f32(cx, &dst, buffer_offset as usize, &data);
        Ok(())
    }
}

impl AudioNodeImpl for Engine {
    /// §1.5.2 `connect(destinationNode, output, input)`.
    fn connect_destination_node_output_input(
        &self,
        _cx: &mut Cx,
        this: Handle,
        destination_node: Handle,
        output: u32,
        input: u32,
    ) -> rt::Result<Handle> {
        let ctx = self.node_context(this);
        if ctx != self.node_context(destination_node) {
            return Err(invalid_access("The nodes belong to different contexts"));
        }
        self.check_output(this, output)?;
        let (inputs, to) = self
            .audio_node(destination_node, |n| (n.inputs, n.id))
            .unwrap_or_default();
        if input >= inputs {
            return Err(index_size("The input index is out of range"));
        }
        let target = Target::Node(destination_node, input);
        let (from, new) = self
            .audio_node(this, |n| {
                let new = !n.connections.contains(&(output, target));
                if new {
                    n.connections.push((output, target));
                }
                (n.id, new)
            })
            .unwrap_or_default();
        if new {
            if let Some(ctx) = ctx {
                self.audio_send(
                    ctx,
                    Control::Connect {
                        from,
                        output,
                        to,
                        input,
                    },
                );
            }
        }
        Ok(destination_node)
    }

    /// §1.5.2 `connect(destinationParam, output)`.
    fn connect_destination_param_output(
        &self,
        _cx: &mut Cx,
        this: Handle,
        destination_param: Handle,
        output: u32,
    ) -> rt::Result<()> {
        let ctx = self.node_context(this);
        let (pctx, param) = self
            .audio_param(destination_param, |p| (p.context, p.id))
            .ok_or_else(|| invalid_access("Not an AudioParam"))?;
        if ctx != Some(pctx) {
            return Err(invalid_access(
                "The node and the parameter belong to different contexts",
            ));
        }
        self.check_output(this, output)?;
        let target = Target::Param(destination_param);
        let (from, new) = self
            .audio_node(this, |n| {
                let new = !n.connections.contains(&(output, target));
                if new {
                    n.connections.push((output, target));
                }
                (n.id, new)
            })
            .unwrap_or_default();
        if new {
            self.audio_send(
                pctx,
                Control::ConnectParam {
                    from,
                    output,
                    param,
                },
            );
        }
        Ok(())
    }

    fn disconnect_0(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.disconnect_where(this, |_, _| false);
        Ok(())
    }

    fn disconnect_output(&self, _cx: &mut Cx, this: Handle, output: u32) -> rt::Result<()> {
        self.check_output(this, output)?;
        self.disconnect_where(this, |o, _| o != output);
        Ok(())
    }

    fn disconnect_destination_node(
        &self,
        _cx: &mut Cx,
        this: Handle,
        destination_node: Handle,
    ) -> rt::Result<()> {
        let n = self.disconnect_where(
            this,
            |_, t| !matches!(t, Target::Node(d, _) if *d == destination_node),
        );
        if n == 0 {
            return Err(invalid_access("The node is not connected to that node"));
        }
        Ok(())
    }

    fn disconnect_destination_node_output(
        &self,
        _cx: &mut Cx,
        this: Handle,
        destination_node: Handle,
        output: u32,
    ) -> rt::Result<()> {
        self.check_output(this, output)?;
        let n = self.disconnect_where(this, |o, t| {
            !(o == output && matches!(t, Target::Node(d, _) if *d == destination_node))
        });
        if n == 0 {
            return Err(invalid_access("The output is not connected to that node"));
        }
        Ok(())
    }

    fn disconnect_destination_node_output_input(
        &self,
        _cx: &mut Cx,
        this: Handle,
        destination_node: Handle,
        output: u32,
        input: u32,
    ) -> rt::Result<()> {
        self.check_output(this, output)?;
        let inputs = self.audio_node(destination_node, |n| n.inputs).unwrap_or(0);
        if input >= inputs {
            return Err(index_size("The input index is out of range"));
        }
        let n = self.disconnect_where(this, |o, t| {
            !(o == output && *t == Target::Node(destination_node, input))
        });
        if n == 0 {
            return Err(invalid_access("The output is not connected to that input"));
        }
        Ok(())
    }

    fn disconnect_destination_param(
        &self,
        _cx: &mut Cx,
        this: Handle,
        destination_param: Handle,
    ) -> rt::Result<()> {
        let n = self.disconnect_where(this, |_, t| *t != Target::Param(destination_param));
        if n == 0 {
            return Err(invalid_access(
                "The node is not connected to that parameter",
            ));
        }
        Ok(())
    }

    fn disconnect_destination_param_output(
        &self,
        _cx: &mut Cx,
        this: Handle,
        destination_param: Handle,
        output: u32,
    ) -> rt::Result<()> {
        self.check_output(this, output)?;
        let n = self.disconnect_where(this, |o, t| {
            !(o == output && *t == Target::Param(destination_param))
        });
        if n == 0 {
            return Err(invalid_access(
                "The output is not connected to that parameter",
            ));
        }
        Ok(())
    }

    fn context(&self, this: Handle) -> Handle {
        self.node_context(this).unwrap_or(this)
    }

    fn number_of_inputs(&self, this: Handle) -> u32 {
        self.audio_node(this, |n| n.inputs).unwrap_or(0)
    }

    fn number_of_outputs(&self, this: Handle) -> u32 {
        self.audio_node(this, |n| n.outputs).unwrap_or(0)
    }

    fn channel_count(&self, this: Handle) -> u32 {
        self.audio_node(this, |n| n.config.count).unwrap_or(0)
    }

    /// §1.5.2 `channelCount`.
    fn set_channel_count(&self, _cx: &mut Cx, this: Handle, value: u32) -> rt::Result<()> {
        let (kind, current) = self
            .audio_node(this, |n| (n.kind, n.config.count))
            .ok_or_else(|| invalid_state("Not an AudioNode"))?;
        // §1.30, §1.15: a splitter's channel count is its number of outputs, a merger's 1.
        if matches!(kind, NodeKind::Splitter | NodeKind::Merger) {
            if value != current {
                return Err(invalid_state(
                    "The channel count of this node cannot change",
                ));
            }
            return Ok(());
        }
        if value == 0 || value > MAX_CHANNELS {
            return Err(not_supported("The channel count is not supported"));
        }
        // §1.19, §1.16, §1.27, §1.31: these nodes have one or two channels.
        if matches!(
            kind,
            NodeKind::Compressor | NodeKind::Convolver | NodeKind::Panner | NodeKind::StereoPanner
        ) && value > 2
        {
            return Err(not_supported("This node has at most two channels"));
        }
        // An OfflineAudioContext's destination has the context's channel count for good.
        if kind == NodeKind::Destination && value != current {
            return Err(invalid_state(
                "The channel count of an offline destination cannot change",
            ));
        }
        self.set_channel_config(this, |c| c.count = value);
        Ok(())
    }

    fn channel_count_mode(&self, this: Handle) -> ChannelCountMode {
        match self
            .audio_node(this, |n| n.config.mode)
            .unwrap_or(CountMode::Max)
        {
            CountMode::Max => ChannelCountMode::Max,
            CountMode::ClampedMax => ChannelCountMode::ClampedMax,
            CountMode::Explicit => ChannelCountMode::Explicit,
        }
    }

    fn set_channel_count_mode(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: ChannelCountMode,
    ) -> rt::Result<()> {
        let (kind, current) = self
            .audio_node(this, |n| (n.kind, n.config.mode))
            .ok_or_else(|| invalid_state("Not an AudioNode"))?;
        let mode = count_mode(value);
        if matches!(kind, NodeKind::Splitter | NodeKind::Merger) && mode != current {
            return Err(invalid_state(
                "The channel count mode of this node cannot change",
            ));
        }
        if matches!(
            kind,
            NodeKind::Compressor | NodeKind::Convolver | NodeKind::Panner | NodeKind::StereoPanner
        ) && mode == CountMode::Max
        {
            return Err(not_supported(
                "This node's channel count mode cannot be max",
            ));
        }
        if kind == NodeKind::Destination && mode != current {
            return Err(invalid_state(
                "The channel count mode of an offline destination cannot change",
            ));
        }
        self.set_channel_config(this, |c| c.mode = mode);
        Ok(())
    }

    fn channel_interpretation(&self, this: Handle) -> ChannelInterpretation {
        match self
            .audio_node(this, |n| n.config.interpretation)
            .unwrap_or(Interpretation::Speakers)
        {
            Interpretation::Speakers => ChannelInterpretation::Speakers,
            Interpretation::Discrete => ChannelInterpretation::Discrete,
        }
    }

    fn set_channel_interpretation(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: ChannelInterpretation,
    ) -> rt::Result<()> {
        let i = interpretation(value);
        let (kind, current) = self
            .audio_node(this, |n| (n.kind, n.config.interpretation))
            .ok_or_else(|| invalid_state("Not an AudioNode"))?;
        if kind == NodeKind::Splitter && i != current {
            return Err(invalid_state(
                "The channel interpretation of a splitter cannot change",
            ));
        }
        self.set_channel_config(this, |c| c.interpretation = i);
        Ok(())
    }
}

impl Engine {
    fn set_channel_config(&self, node: Handle, f: impl FnOnce(&mut ChannelConfig)) {
        if let Some((ctx, id, config)) = self.audio_node(node, |n| {
            f(&mut n.config);
            (n.context, n.id, n.config)
        }) {
            self.audio_send(ctx, Control::SetChannelConfig { node: id, config });
        }
    }

    /// §1.8 `start(when)` of every scheduled source; `offset` and `duration` an
    /// AudioBufferSourceNode's.
    fn start_source(
        &self,
        cx: &mut Cx,
        this: Handle,
        when: f64,
        offset: f64,
        duration: Option<f64>,
    ) -> rt::Result<()> {
        let (ctx, id, started, buffer) = self
            .audio_node(this, |n| (n.context, n.id, n.started, n.buffer))
            .ok_or_else(|| invalid_state("Not an AudioScheduledSourceNode"))?;
        if started {
            return Err(invalid_state("start() was already called"));
        }
        if when < 0.0 {
            return Err(rt::Error::Range("The start time is negative".into()));
        }
        self.audio_node(this, |n| n.started = true);
        if let Some(b) = buffer {
            let data = self.acquire_buffer(cx, b);
            self.audio_send(
                ctx,
                Control::SetBuffer {
                    node: id,
                    buffer: data,
                },
            );
        }
        self.audio_send(
            ctx,
            Control::Start {
                node: id,
                when,
                offset,
                duration,
            },
        );
        self.audio_source_started(ctx);
        Ok(())
    }
}

impl AudioScheduledSourceNodeImpl for Engine {
    fn start(&self, cx: &mut Cx, this: Handle, when: f64) -> rt::Result<()> {
        self.start_source(cx, this, when, 0.0, None)
    }

    /// §1.8 `stop(when)`.
    fn stop(&self, _cx: &mut Cx, this: Handle, when: f64) -> rt::Result<()> {
        let (ctx, id, started) = self
            .audio_node(this, |n| (n.context, n.id, n.started))
            .ok_or_else(|| invalid_state("Not an AudioScheduledSourceNode"))?;
        if !started {
            return Err(invalid_state("start() was not called"));
        }
        if when < 0.0 {
            return Err(rt::Error::Range("The stop time is negative".into()));
        }
        self.audio_send(ctx, Control::Stop { node: id, when });
        Ok(())
    }
}

impl AudioDestinationNodeImpl for Engine {
    fn max_channel_count(&self, this: Handle) -> u32 {
        self.node_context(this)
            .and_then(|c| self.audio_ctx(c, |c| c.channels))
            .unwrap_or(0)
    }
}

impl GainNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: GainOptions,
    ) -> rt::Result<Handle> {
        self.new_gain(cx, context, &options)
    }

    fn gain(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[0]).unwrap_or(this)
    }
}

impl ConstantSourceNodeImpl for Engine {
    fn constructor(
        &self,
        _cx: &mut Cx,
        context: Handle,
        options: ConstantSourceOptions,
    ) -> rt::Result<Handle> {
        Ok(self.new_constant_source(context, options.offset))
    }

    fn offset(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[0]).unwrap_or(this)
    }
}

impl AudioBufferSourceNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: AudioBufferSourceOptions,
    ) -> rt::Result<Handle> {
        self.new_buffer_source(cx, context, &options)
    }

    fn buffer(&self, this: Handle) -> Option<Handle> {
        self.audio_node(this, |n| n.buffer).flatten()
    }

    /// §1.9.2 `buffer`: set once to a buffer; the contents are acquired when the source has
    /// started (else at `start()`).
    fn set_buffer(&self, cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
        let (ctx, id, set, started) = self
            .audio_node(this, |n| (n.context, n.id, n.buffer_set, n.started))
            .ok_or_else(|| invalid_state("Not an AudioBufferSourceNode"))?;
        if value.is_some() && set {
            return Err(invalid_state("The buffer was already set"));
        }
        self.audio_node(this, |n| {
            if value.is_some() {
                n.buffer_set = true;
            }
            n.buffer = value;
        });
        if started {
            let data = value.and_then(|b| self.acquire_buffer(cx, b));
            self.audio_send(
                ctx,
                Control::SetBuffer {
                    node: id,
                    buffer: data,
                },
            );
        }
        Ok(())
    }

    fn playback_rate(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[0]).unwrap_or(this)
    }

    fn detune(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[1]).unwrap_or(this)
    }

    fn loop_(&self, this: Handle) -> bool {
        self.audio_node(this, |n| n.looping).unwrap_or(false)
    }

    fn set_loop(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.audio_node(this, |n| n.looping = value);
        self.send_loop(this);
        Ok(())
    }

    fn loop_start(&self, this: Handle) -> f64 {
        self.audio_node(this, |n| n.loop_start).unwrap_or(0.0)
    }

    fn set_loop_start(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.audio_node(this, |n| n.loop_start = value);
        self.send_loop(this);
        Ok(())
    }

    fn loop_end(&self, this: Handle) -> f64 {
        self.audio_node(this, |n| n.loop_end).unwrap_or(0.0)
    }

    fn set_loop_end(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.audio_node(this, |n| n.loop_end = value);
        self.send_loop(this);
        Ok(())
    }

    /// §1.9.3 `start(when, offset, duration)`.
    fn start(
        &self,
        cx: &mut Cx,
        this: Handle,
        when: f64,
        offset: Option<f64>,
        duration: Option<f64>,
    ) -> rt::Result<()> {
        if offset.is_some_and(|o| o < 0.0) {
            return Err(rt::Error::Range("The offset is negative".into()));
        }
        if duration.is_some_and(|d| d < 0.0) {
            return Err(rt::Error::Range("The duration is negative".into()));
        }
        self.start_source(cx, this, when, offset.unwrap_or(0.0), duration)
    }
}

impl AudioParamImpl for Engine {
    fn value(&self, this: Handle) -> f32 {
        self.audio_param(this, |p| f32::from_bits(p.shared.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    /// §1.6.1 `value`: the [[current value]], and setValueAtTime(value, currentTime).
    fn set_value(&self, _cx: &mut Cx, this: Handle, value: f32) -> rt::Result<()> {
        let (ctx, id, desc) = self
            .audio_param(this, |p| (p.context, p.id, p.desc))
            .ok_or_else(|| invalid_state("Not an AudioParam"))?;
        let now = self.context_time(ctx);
        let ev = Event::set_value(value, now);
        self.audio_param(this, |p| {
            p.timeline
                .check(&ev)
                .map_err(|_| not_supported("The value overlaps a value curve"))?;
            p.timeline.insert(ev, now, 0.0);
            p.shared
                .store(desc.clamp(value).to_bits(), Ordering::Relaxed);
            Ok::<_, rt::Error>(())
        })
        .unwrap_or(Ok(()))?;
        self.audio_send(
            ctx,
            Control::Param {
                param: id,
                op: ParamOp::SetValue { value, time: now },
            },
        );
        Ok(())
    }

    fn automation_rate(&self, this: Handle) -> AutomationRate {
        match self.audio_param(this, |p| p.rate).unwrap_or(Rate::A) {
            Rate::A => AutomationRate::ARate,
            Rate::K => AutomationRate::KRate,
        }
    }

    /// §1.6.1 `automationRate`.
    fn set_automation_rate(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: AutomationRate,
    ) -> rt::Result<()> {
        let rate = match value {
            AutomationRate::ARate => Rate::A,
            AutomationRate::KRate => Rate::K,
        };
        let (ctx, id, desc, current) = self
            .audio_param(this, |p| (p.context, p.id, p.desc, p.rate))
            .ok_or_else(|| invalid_state("Not an AudioParam"))?;
        if desc.fixed_rate && rate != current {
            return Err(invalid_state("The automation rate cannot change"));
        }
        self.audio_param(this, |p| p.rate = rate);
        self.audio_send(ctx, Control::SetRate { param: id, rate });
        Ok(())
    }

    fn default_value(&self, this: Handle) -> f32 {
        self.param_desc(this).default
    }

    fn min_value(&self, this: Handle) -> f32 {
        self.param_desc(this).min
    }

    fn max_value(&self, this: Handle) -> f32 {
        self.param_desc(this).max
    }

    fn set_value_at_time(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: f32,
        start_time: f64,
    ) -> rt::Result<Handle> {
        if start_time < 0.0 {
            return Err(rt::Error::Range("The start time is negative".into()));
        }
        self.param_insert(this, Event::set_value(value, start_time))
    }

    fn linear_ramp_to_value_at_time(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: f32,
        end_time: f64,
    ) -> rt::Result<Handle> {
        if end_time < 0.0 {
            return Err(rt::Error::Range("The end time is negative".into()));
        }
        self.param_insert(this, Event::linear(value, end_time))
    }

    fn exponential_ramp_to_value_at_time(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: f32,
        end_time: f64,
    ) -> rt::Result<Handle> {
        if value == 0.0 {
            return Err(rt::Error::Range("The value must not be zero".into()));
        }
        if end_time < 0.0 {
            return Err(rt::Error::Range("The end time is negative".into()));
        }
        self.param_insert(this, Event::exponential(value, end_time))
    }

    fn set_target_at_time(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: f32,
        start_time: f64,
        time_constant: f32,
    ) -> rt::Result<Handle> {
        if start_time < 0.0 {
            return Err(rt::Error::Range("The start time is negative".into()));
        }
        if time_constant < 0.0 {
            return Err(rt::Error::Range("The time constant is negative".into()));
        }
        self.param_insert(
            this,
            Event::target(target, start_time, time_constant as f64),
        )
    }

    fn set_value_curve_at_time(
        &self,
        _cx: &mut Cx,
        this: Handle,
        values: Vec<f32>,
        start_time: f64,
        duration: f64,
    ) -> rt::Result<Handle> {
        if start_time < 0.0 {
            return Err(rt::Error::Range("The start time is negative".into()));
        }
        if duration <= 0.0 {
            return Err(rt::Error::Range("The duration must be positive".into()));
        }
        if values.len() < 2 {
            return Err(invalid_state("A value curve needs at least two values"));
        }
        self.param_insert(this, Event::curve(Arc::from(values), start_time, duration))
    }

    fn cancel_scheduled_values(
        &self,
        _cx: &mut Cx,
        this: Handle,
        cancel_time: f64,
    ) -> rt::Result<Handle> {
        self.param_cancel(this, cancel_time, false)
    }

    fn cancel_and_hold_at_time(
        &self,
        _cx: &mut Cx,
        this: Handle,
        cancel_time: f64,
    ) -> rt::Result<Handle> {
        self.param_cancel(this, cancel_time, true)
    }
}

impl Engine {
    fn param_cancel(&self, p: Handle, t: f64, hold: bool) -> rt::Result<Handle> {
        if t < 0.0 {
            return Err(rt::Error::Range("The cancel time is negative".into()));
        }
        let (ctx, id) = self
            .audio_param(p, |d| (d.context, d.id))
            .ok_or_else(|| invalid_state("Not an AudioParam"))?;
        let t = t.max(self.context_time(ctx));
        self.audio_param(p, |d| {
            if hold {
                d.timeline.cancel_and_hold(t, 0.0);
            } else {
                d.timeline.cancel(t);
            }
        });
        let op = if hold {
            ParamOp::CancelAndHold(t)
        } else {
            ParamOp::Cancel(t)
        };
        self.audio_send(ctx, Control::Param { param: id, op });
        Ok(p)
    }
}

fn shape_of(t: OscillatorType) -> Option<Shape> {
    match t {
        OscillatorType::Sine => Some(Shape::Sine),
        OscillatorType::Square => Some(Shape::Square),
        OscillatorType::Sawtooth => Some(Shape::Sawtooth),
        OscillatorType::Triangle => Some(Shape::Triangle),
        OscillatorType::Custom => None,
    }
}

impl Engine {
    /// A built-in waveform at the context's rate, made once per context.
    fn basic_wave(&self, ctx: Handle, shape: Shape) -> Arc<PeriodicWave> {
        let i = shape as usize;
        self.audio_ctx(ctx, |c| {
            c.waves[i]
                .get_or_insert_with(|| Arc::new(PeriodicWave::basic(c.sample_rate, shape)))
                .clone()
        })
        .unwrap_or_else(|| Arc::new(PeriodicWave::basic(44100.0, shape)))
    }

    /// §1.27.1 the PeriodicWave constructor and `createPeriodicWave()`.
    fn new_periodic_wave(
        &self,
        ctx: Handle,
        real: Option<Vec<f32>>,
        imag: Option<Vec<f32>>,
        disable_normalization: bool,
    ) -> rt::Result<Handle> {
        let (mut real, mut imag) = match (real, imag) {
            (Some(r), Some(i)) => {
                if r.len() != i.len() {
                    return Err(index_size("real and imag differ in length"));
                }
                (r, i)
            }
            (Some(r), None) => {
                let n = r.len();
                (r, vec![0.0; n])
            }
            (None, Some(i)) => (vec![0.0; i.len()], i),
            (None, None) => (vec![0.0, 0.0], vec![0.0, 1.0]),
        };
        if real.len() < 2 {
            return Err(index_size(
                "A periodic wave needs at least two coefficients",
            ));
        }
        // The DC terms are ignored.
        real[0] = 0.0;
        imag[0] = 0.0;
        let rate = self
            .audio_ctx(ctx, |c| c.sample_rate)
            .ok_or_else(|| invalid_state("Not a BaseAudioContext"))?;
        let wave = PeriodicWave::new(rate, &real, &imag, !disable_normalization);
        Ok(self.alloc(Object::Audio(AudioObject::Wave(Arc::new(wave)))))
    }

    fn wave_of(&self, h: Handle) -> Option<Arc<PeriodicWave>> {
        match &*self.obj(h) {
            Object::Audio(AudioObject::Wave(w)) => Some(w.clone()),
            _ => None,
        }
    }

    /// §1.26.1 the OscillatorNode constructor.
    fn new_oscillator(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        options: &OscillatorOptions,
    ) -> rt::Result<Handle> {
        let (wave, osc_type) = match options.periodic_wave {
            Some(w) => (
                self.wave_of(w)
                    .ok_or_else(|| invalid_state("Not a PeriodicWave"))?,
                OscillatorType::Custom,
            ),
            None => match shape_of(options.type_) {
                Some(shape) => (self.basic_wave(ctx, shape), options.type_),
                None => return Err(invalid_state("A custom oscillator needs a periodicWave")),
            },
        };
        let nyquist = self.audio_ctx(ctx, |c| c.sample_rate / 2.0).unwrap_or(0.0);
        let detune_max = 1200.0 * f32::MAX.log2();
        let h = self.new_audio_node(
            ctx,
            NodeKind::Oscillator,
            Kind::Oscillator(Box::new(Oscillator::new(wave))),
            (0, 1),
            ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
            &[
                (
                    ParamDesc::unbounded(440.0, Rate::A).range(-nyquist, nyquist),
                    options.frequency,
                ),
                (
                    ParamDesc::unbounded(0.0, Rate::A).range(-detune_max, detune_max),
                    options.detune,
                ),
            ],
        );
        self.audio_node(h, |n| n.osc_type = osc_type);
        self.apply_node_options(
            cx,
            h,
            options.channel_count,
            options.channel_count_mode,
            options.channel_interpretation,
        )?;
        Ok(h)
    }

    fn set_wave(&self, node: Handle, wave: Arc<PeriodicWave>, osc_type: OscillatorType) {
        if let Some((ctx, id)) = self.audio_node(node, |n| {
            n.osc_type = osc_type;
            (n.context, n.id)
        }) {
            self.audio_send(ctx, Control::SetWave { node: id, wave });
        }
    }

    /// §1.19.1 the DynamicsCompressorNode constructor.
    fn new_compressor(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        options: &DynamicsCompressorOptions,
    ) -> rt::Result<Handle> {
        let rate = self.audio_ctx(ctx, |c| c.sample_rate).unwrap_or(44100.0);
        let reduction = Arc::new(AtomicU32::new(0));
        let k = |d, min, max| ParamDesc::unbounded(d, Rate::K).range(min, max).fixed();
        let h = self.new_audio_node(
            ctx,
            NodeKind::Compressor,
            Kind::Compressor(Box::new(Compressor::new(rate)), reduction.clone()),
            (1, 1),
            ChannelConfig::new(2, CountMode::ClampedMax, Interpretation::Speakers),
            &[
                (k(-24.0, -100.0, 0.0), options.threshold),
                (k(30.0, 0.0, 40.0), options.knee),
                (k(12.0, 1.0, 20.0), options.ratio),
                (k(0.003, 0.0, 1.0), options.attack),
                (k(0.25, 0.0, 1.0), options.release),
            ],
        );
        self.audio_node(h, |n| n.reduction = Some(reduction));
        self.apply_node_options(
            cx,
            h,
            options.channel_count,
            options.channel_count_mode,
            options.channel_interpretation,
        )?;
        Ok(h)
    }
}

impl PeriodicWaveImpl for Engine {
    fn constructor(
        &self,
        _cx: &mut Cx,
        context: Handle,
        options: PeriodicWaveOptions,
    ) -> rt::Result<Handle> {
        self.new_periodic_wave(
            context,
            options.real,
            options.imag,
            options.disable_normalization,
        )
    }
}

impl OscillatorNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: OscillatorOptions,
    ) -> rt::Result<Handle> {
        self.new_oscillator(cx, context, &options)
    }

    fn type_(&self, this: Handle) -> OscillatorType {
        self.audio_node(this, |n| n.osc_type)
            .unwrap_or(OscillatorType::Sine)
    }

    /// §1.26.2 `type`: "custom" only through `setPeriodicWave()`.
    fn set_type(&self, _cx: &mut Cx, this: Handle, value: OscillatorType) -> rt::Result<()> {
        let Some(shape) = shape_of(value) else {
            return Err(invalid_state("Use setPeriodicWave() for a custom wave"));
        };
        let ctx = self
            .node_context(this)
            .ok_or_else(|| invalid_state("Not an OscillatorNode"))?;
        let wave = self.basic_wave(ctx, shape);
        self.set_wave(this, wave, value);
        Ok(())
    }

    fn set_periodic_wave(
        &self,
        _cx: &mut Cx,
        this: Handle,
        periodic_wave: Handle,
    ) -> rt::Result<()> {
        let wave = self
            .wave_of(periodic_wave)
            .ok_or_else(|| invalid_state("Not a PeriodicWave"))?;
        self.set_wave(this, wave, OscillatorType::Custom);
        Ok(())
    }

    fn frequency(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[0]).unwrap_or(this)
    }

    fn detune(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[1]).unwrap_or(this)
    }
}

impl DynamicsCompressorNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: DynamicsCompressorOptions,
    ) -> rt::Result<Handle> {
        self.new_compressor(cx, context, &options)
    }

    fn threshold(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[0]).unwrap_or(this)
    }

    fn knee(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[1]).unwrap_or(this)
    }

    fn ratio(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[2]).unwrap_or(this)
    }

    fn attack(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[3]).unwrap_or(this)
    }

    fn release(&self, this: Handle) -> Handle {
        self.audio_node(this, |n| n.params[4]).unwrap_or(this)
    }

    /// §1.19.2 `reduction`: the rendering thread's last metering, in decibels.
    fn reduction(&self, this: Handle) -> f32 {
        self.audio_node(this, |n| n.reduction.clone())
            .flatten()
            .map_or(0.0, |r| f32::from_bits(r.load(Ordering::Relaxed)))
    }
}
