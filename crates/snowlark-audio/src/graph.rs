// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The rendering thread's graph: nodes and AudioParams by id, their connections, and one render
//! quantum of the whole graph (§2.4 "Rendering an Audio Graph").

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::biquad::FilterType;
use crate::bus::{AudioBus, ChannelConfig};
use crate::convolver::Convolver;
use crate::node::{BufferData, Kind, Quantum};
use crate::oscillator::PeriodicWave;
use crate::panner::PannerSettings;
use crate::param::{Event, ParamDesc, Rate, Timeline};
use crate::shaper::Oversample;
use crate::RENDER_QUANTUM as N;

/// A node's id in its context. The destination is 0.
pub type NodeId = u32;
/// An AudioParam's id in its context.
pub type ParamId = u32;

/// The destination's id.
pub const DESTINATION: NodeId = 0;

/// The most AudioParams a node has.
const MAX_PARAMS: usize = 16;

static ZERO: [f32; N] = [0.0; N];

/// An AudioParam's value as the control thread reads it (`value`): the intrinsic value the
/// rendering thread computed last, as `f32` bits.
pub type SharedValue = Arc<AtomicU32>;

/// A change to an AudioParam's timeline.
#[derive(Clone, Debug)]
pub enum ParamOp {
    Insert(Event),
    Cancel(f64),
    CancelAndHold(f64),
    /// The `value` setter: the intrinsic value becomes `value` (clamped), and a SetValue event at
    /// `time` follows.
    SetValue {
        value: f32,
        time: f64,
    },
}

/// A new node's AudioParam.
#[derive(Clone, Debug)]
pub struct NewParam {
    pub id: ParamId,
    pub desc: ParamDesc,
    pub value: f32,
    pub shared: SharedValue,
}

/// A control message (§2.2 "Control Thread and Rendering Thread"): every change the control thread
/// makes to what renders.
#[derive(Debug)]
pub enum Control {
    AddNode {
        id: NodeId,
        kind: Kind,
        inputs: u32,
        outputs: u32,
        config: ChannelConfig,
        params: Vec<NewParam>,
    },
    Connect {
        from: NodeId,
        output: u32,
        to: NodeId,
        input: u32,
    },
    Disconnect {
        from: NodeId,
        output: u32,
        to: NodeId,
        input: u32,
    },
    ConnectParam {
        from: NodeId,
        output: u32,
        param: ParamId,
    },
    DisconnectParam {
        from: NodeId,
        output: u32,
        param: ParamId,
    },
    SetChannelConfig {
        node: NodeId,
        config: ChannelConfig,
    },
    Param {
        param: ParamId,
        op: ParamOp,
    },
    SetRate {
        param: ParamId,
        rate: Rate,
    },
    Start {
        node: NodeId,
        when: f64,
        /// An AudioBufferSourceNode's offset and duration.
        offset: f64,
        duration: Option<f64>,
    },
    Stop {
        node: NodeId,
        when: f64,
    },
    SetBuffer {
        node: NodeId,
        buffer: Option<Arc<BufferData>>,
    },
    SetLoop {
        node: NodeId,
        looping: bool,
        start: f64,
        end: f64,
    },
    /// An OscillatorNode's wave (`type` or `setPeriodicWave()`).
    SetWave {
        node: NodeId,
        wave: Arc<PeriodicWave>,
    },
    /// AudioParams that belong to no node (the AudioListener's).
    AddParams(Vec<NewParam>),
    /// Makes existing AudioParams (the listener's) inputs of a node too, after its own.
    AttachParams {
        node: NodeId,
        params: Vec<ParamId>,
    },
    SetFilterType {
        node: NodeId,
        kind: FilterType,
    },
    SetShaper {
        node: NodeId,
        curve: Option<Arc<[f32]>>,
        oversample: Oversample,
    },
    SetPanner {
        node: NodeId,
        settings: PannerSettings,
    },
    SetConvolver {
        node: NodeId,
        convolver: Option<Box<Convolver>>,
    },
}

/// What the rendering thread tells the control thread.
#[derive(Debug, PartialEq)]
pub enum Notice {
    /// A scheduled source stopped playing (`ended`).
    Ended(NodeId),
}

struct ParamSlot {
    desc: ParamDesc,
    rate: Rate,
    intrinsic: f32,
    timeline: Timeline,
    inputs: Vec<(NodeId, u32)>,
    values: Box<[f32; N]>,
    shared: SharedValue,
    /// The inputs' sum (reused).
    sum: AudioBus,
    /// One past the frame its values were last computed for.
    computed: u64,
}

struct NodeSlot {
    kind: Kind,
    config: ChannelConfig,
    /// Per input, the outputs connected to it.
    inputs: Vec<Vec<(NodeId, u32)>>,
    input_buses: Vec<AudioBus>,
    outputs: Vec<AudioBus>,
    params: Vec<ParamId>,
    /// Per input, whether it has a connection.
    connected: Vec<bool>,
    /// The node is actively processing (§2.4): an input is connected, or its tail has not played
    /// out since the last one was.
    active: bool,
}

/// An audio context's graph on its rendering thread.
pub struct Graph {
    sample_rate: f64,
    nodes: Vec<Option<NodeSlot>>,
    params: Vec<Option<ParamSlot>>,
    order: Vec<NodeId>,
    muted: Vec<bool>,
    dirty: bool,
    notices: Vec<Notice>,
}

impl Graph {
    pub fn new(sample_rate: f32) -> Graph {
        Graph {
            sample_rate: sample_rate as f64,
            nodes: Vec::new(),
            params: Vec::new(),
            order: Vec::new(),
            muted: Vec::new(),
            dirty: true,
            notices: Vec::new(),
        }
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Takes the notices the quanta rendered so far produced.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    fn node(&mut self, id: NodeId) -> Option<&mut NodeSlot> {
        self.nodes.get_mut(id as usize).and_then(Option::as_mut)
    }

    fn param(&mut self, id: ParamId) -> Option<&mut ParamSlot> {
        self.params.get_mut(id as usize).and_then(Option::as_mut)
    }

    fn add_param(&mut self, p: NewParam) {
        let i = p.id as usize;
        if self.params.len() <= i {
            self.params.resize_with(i + 1, || None);
        }
        let intrinsic = p.desc.clamp(p.value);
        p.shared.store(intrinsic.to_bits(), Ordering::Relaxed);
        self.params[i] = Some(ParamSlot {
            rate: p.desc.rate,
            desc: p.desc,
            intrinsic,
            timeline: Timeline::default(),
            inputs: Vec::new(),
            values: Box::new([0.0; N]),
            shared: p.shared,
            sum: AudioBus::new(1),
            computed: 0,
        });
    }

    /// Applies a control message at `now` (the context's time at the quantum boundary).
    pub fn apply(&mut self, msg: Control, now: f64) {
        match msg {
            Control::AddNode {
                id,
                kind,
                inputs,
                outputs,
                config,
                params,
            } => {
                let mut ids = Vec::with_capacity(params.len());
                for p in params {
                    ids.push(p.id);
                    self.add_param(p);
                }
                let i = id as usize;
                if self.nodes.len() <= i {
                    self.nodes.resize_with(i + 1, || None);
                }
                self.nodes[i] = Some(NodeSlot {
                    kind,
                    config,
                    inputs: vec![Vec::new(); inputs as usize],
                    input_buses: (0..inputs).map(|_| AudioBus::new(1)).collect(),
                    outputs: (0..outputs.max(1)).map(|_| AudioBus::new(1)).collect(),
                    params: ids,
                    connected: vec![false; inputs as usize],
                    active: false,
                });
                self.dirty = true;
            }
            Control::Connect {
                from,
                output,
                to,
                input,
            } => {
                if let Some(list) = self.node(to).and_then(|n| n.inputs.get_mut(input as usize)) {
                    if !list.contains(&(from, output)) {
                        list.push((from, output));
                    }
                }
                self.dirty = true;
            }
            Control::Disconnect {
                from,
                output,
                to,
                input,
            } => {
                if let Some(list) = self.node(to).and_then(|n| n.inputs.get_mut(input as usize)) {
                    list.retain(|c| *c != (from, output));
                }
                self.dirty = true;
            }
            Control::ConnectParam {
                from,
                output,
                param,
            } => {
                if let Some(p) = self.param(param) {
                    if !p.inputs.contains(&(from, output)) {
                        p.inputs.push((from, output));
                    }
                }
                self.dirty = true;
            }
            Control::DisconnectParam {
                from,
                output,
                param,
            } => {
                if let Some(p) = self.param(param) {
                    p.inputs.retain(|c| *c != (from, output));
                }
                self.dirty = true;
            }
            Control::SetChannelConfig { node, config } => {
                if let Some(n) = self.node(node) {
                    n.config = config;
                }
            }
            Control::Param { param, op } => {
                if let Some(p) = self.param(param) {
                    match op {
                        ParamOp::Insert(e) => p.timeline.insert(e, now, p.intrinsic),
                        ParamOp::Cancel(t) => p.timeline.cancel(t),
                        ParamOp::CancelAndHold(t) => p.timeline.cancel_and_hold(t, p.intrinsic),
                        ParamOp::SetValue { value, time } => {
                            p.intrinsic = p.desc.clamp(value);
                            p.timeline
                                .insert(Event::set_value(value, time), now, p.intrinsic);
                        }
                    }
                }
            }
            Control::SetRate { param, rate } => {
                if let Some(p) = self.param(param) {
                    p.rate = rate;
                }
            }
            Control::Start {
                node,
                when,
                offset,
                duration,
            } => {
                if let Some(n) = self.node(node) {
                    if let Kind::BufferSource(b) = &mut n.kind {
                        b.offset = offset;
                        b.duration = duration;
                    }
                    if let Some(s) = n.kind.schedule_mut() {
                        s.start = Some(when);
                    }
                }
            }
            Control::Stop { node, when } => {
                if let Some(s) = self.node(node).and_then(|n| n.kind.schedule_mut()) {
                    s.stop = Some(when);
                }
            }
            Control::SetBuffer { node, buffer } => {
                if let Some(Kind::BufferSource(b)) = self.node(node).map(|n| &mut n.kind) {
                    b.buffer = buffer;
                }
            }
            Control::SetWave { node, wave } => {
                if let Some(Kind::Oscillator(o)) = self.node(node).map(|n| &mut n.kind) {
                    o.wave = wave;
                }
            }
            Control::SetLoop {
                node,
                looping,
                start,
                end,
            } => {
                if let Some(Kind::BufferSource(b)) = self.node(node).map(|n| &mut n.kind) {
                    b.looping = looping;
                    b.loop_start = start;
                    b.loop_end = end;
                }
            }
            Control::AddParams(params) => {
                for p in params {
                    self.add_param(p);
                }
            }
            Control::AttachParams { node, params } => {
                if let Some(n) = self.node(node) {
                    n.params.extend(params);
                }
                self.dirty = true;
            }
            Control::SetFilterType { node, kind } => {
                if let Some(Kind::Biquad(b)) = self.node(node).map(|n| &mut n.kind) {
                    b.kind = kind;
                }
            }
            Control::SetShaper {
                node,
                curve,
                oversample,
            } => {
                if let Some(Kind::Shaper(s)) = self.node(node).map(|n| &mut n.kind) {
                    s.curve = curve;
                    s.set_oversample(oversample);
                }
            }
            Control::SetPanner { node, settings } => {
                if let Some(Kind::Panner(p)) = self.node(node).map(|n| &mut n.kind) {
                    **p = settings;
                }
            }
            Control::SetConvolver { node, convolver } => {
                if let Some(Kind::Convolver(c)) = self.node(node).map(|n| &mut n.kind) {
                    *c = convolver;
                }
            }
        }
    }

    /// The nodes each node reads from (its inputs and its AudioParams' inputs), without the
    /// inputs of the delays split for a cycle.
    fn sources(&self, split: &[bool]) -> Vec<Vec<NodeId>> {
        let mut all = vec![Vec::new(); self.nodes.len()];
        for (id, slot) in self.nodes.iter().enumerate() {
            let Some(slot) = slot else { continue };
            let mut sources: Vec<NodeId> = if split[id] {
                Vec::new()
            } else {
                slot.inputs.iter().flatten().map(|c| c.0).collect()
            };
            for p in &slot.params {
                if let Some(Some(p)) = self.params.get(*p as usize) {
                    sources.extend(p.inputs.iter().map(|c| c.0));
                }
            }
            sources.retain(|s| self.nodes.get(*s as usize).is_some_and(Option::is_some));
            sources.sort_unstable();
            sources.dedup();
            all[id] = sources;
        }
        all
    }

    /// For each node, the nodes that read from it.
    fn readers(sources: &[Vec<NodeId>]) -> Vec<Vec<NodeId>> {
        let mut readers: Vec<Vec<NodeId>> = vec![Vec::new(); sources.len()];
        for (id, s) in sources.iter().enumerate() {
            for from in s {
                readers[*from as usize].push(id as NodeId);
            }
        }
        readers
    }

    /// Kahn's algorithm over `sources`: the order, and the nodes left over (in or after a cycle).
    fn kahn(&self, sources: &[Vec<NodeId>]) -> (Vec<NodeId>, Vec<bool>) {
        let n = self.nodes.len();
        let mut indegree: Vec<usize> = sources.iter().map(Vec::len).collect();
        let readers = Self::readers(sources);
        let mut order = Vec::with_capacity(n);
        let mut ready: Vec<NodeId> = (0..n)
            .filter(|i| self.nodes[*i].is_some() && indegree[*i] == 0)
            .map(|i| i as NodeId)
            .collect();
        ready.reverse();
        while let Some(id) = ready.pop() {
            order.push(id);
            for r in &readers[id as usize] {
                indegree[*r as usize] -= 1;
                if indegree[*r as usize] == 0 {
                    ready.push(*r);
                }
            }
        }
        let left = (0..n)
            .map(|i| self.nodes[i].is_some() && indegree[i] > 0)
            .collect();
        (order, left)
    }

    /// Orders the nodes so each comes after every node it reads from (inputs and AudioParam
    /// inputs). A DelayNode in a cycle is split into a writer and a reader (§2.4): the reader
    /// reads nothing this quantum wrote, so its inputs do not order it. The nodes of a cycle
    /// without a DelayNode, and those after it, are muted.
    fn order_nodes(&mut self) {
        let n = self.nodes.len();
        let mut split = vec![false; n];
        let sources = self.sources(&split);
        let (mut order, mut left) = self.kahn(&sources);
        if left.iter().any(|l| *l) {
            // A left-over delay is in a cycle when it reaches itself through its readers.
            let readers = Self::readers(&sources);
            for d in 0..n {
                let is_delay = matches!(
                    self.nodes[d].as_ref().map(|s| &s.kind),
                    Some(Kind::Delay(_))
                );
                if !left[d] || !is_delay {
                    continue;
                }
                let mut seen = vec![false; n];
                let mut stack = readers[d].clone();
                while let Some(x) = stack.pop() {
                    if x as usize == d {
                        split[d] = true;
                        break;
                    }
                    if !std::mem::replace(&mut seen[x as usize], true) {
                        stack.extend_from_slice(&readers[x as usize]);
                    }
                }
            }
            (order, left) = self.kahn(&self.sources(&split));
        }
        for (i, slot) in self.nodes.iter_mut().enumerate() {
            if let Some(NodeSlot {
                kind: Kind::Delay(d),
                ..
            }) = slot
            {
                d.split = split[i];
            }
        }
        for (i, l) in left.iter().enumerate() {
            if *l {
                order.push(i as NodeId);
            }
        }
        self.muted = left;
        self.order = order;
        self.dirty = false;
    }

    /// Renders one quantum starting at the context's sample-frame `frame`. The destination's
    /// output holds the result ([`Graph::output`]).
    pub fn render(&mut self, frame: u64) {
        if self.dirty {
            self.order_nodes();
        }
        let q = Quantum {
            frame,
            sample_rate: self.sample_rate,
        };
        for k in 0..self.order.len() {
            let id = self.order[k];
            let Some(mut node) = self.nodes[id as usize].take() else {
                continue;
            };
            if self.muted[id as usize] {
                for o in &mut node.outputs {
                    o.zero(1);
                }
                self.nodes[id as usize] = Some(node);
                continue;
            }
            // The node's AudioParams: the timeline, plus the inputs mixed down to mono.
            for p in &node.params {
                let Some(mut slot) = self.params[*p as usize].take() else {
                    continue;
                };
                self.compute_param(&mut slot, frame);
                self.params[*p as usize] = Some(slot);
            }
            self.mix_inputs(&mut node);
            // §2.4: a node with inputs, none connected, whose tail has played out is not actively
            // processing: its output is silence and it does not run. The destination, an analyser
            // (which records the silence) and a merger (whose output keeps its channels) always run,
            // as does a delay in a cycle.
            let always = node.inputs.is_empty()
                || matches!(
                    node.kind,
                    Kind::Destination | Kind::Analyser(..) | Kind::Merger
                )
                || matches!(&node.kind, Kind::Delay(d) if d.split);
            let connected = node.connected.iter().any(|c| *c);
            if !always && !connected && !node.active {
                for o in &mut node.outputs {
                    o.zero(1);
                }
                self.nodes[id as usize] = Some(node);
                continue;
            }
            let mut values: [&[f32; N]; MAX_PARAMS] = [&ZERO; MAX_PARAMS];
            for (v, p) in values.iter_mut().zip(&node.params) {
                if let Some(s) = &self.params[*p as usize] {
                    *v = &s.values;
                }
            }
            let count = node.params.len().min(MAX_PARAMS);
            if node.kind.process(
                q,
                &node.input_buses,
                &node.connected,
                &values[..count],
                &mut node.outputs,
            ) {
                self.notices.push(Notice::Ended(id));
            }
            // Without a connection the node runs on until its output is silent: its tail.
            node.active = connected || node.outputs.iter().any(|o| !o.all_zero());
            self.nodes[id as usize] = Some(node);
        }
        // The writers of the delays split for a cycle, now that their inputs have rendered.
        for k in 0..self.order.len() {
            let id = self.order[k] as usize;
            let is_split = matches!(
                self.nodes[id].as_ref().map(|n| &n.kind),
                Some(Kind::Delay(d)) if d.split
            );
            if !is_split || self.muted[id] {
                continue;
            }
            let Some(mut node) = self.nodes[id].take() else {
                continue;
            };
            self.mix_inputs(&mut node);
            if let Kind::Delay(d) = &mut node.kind {
                d.write(&node.input_buses[0], !node.connected[0]);
            }
            self.nodes[id] = Some(node);
        }
    }

    /// The node's inputs, each the sum of its connections mixed to its computed channel count.
    fn mix_inputs(&self, node: &mut NodeSlot) {
        for (i, conns) in node.inputs.iter().enumerate() {
            let widest = conns
                .iter()
                .filter_map(|(from, out)| self.output_bus(*from, *out))
                .map(AudioBus::channels)
                .max();
            node.connected[i] = widest.is_some();
            let bus = &mut node.input_buses[i];
            bus.zero(node.config.computed_channels(widest));
            for (from, out) in conns {
                if let Some(src) = self.output_bus(*from, *out) {
                    bus.sum_from(src, node.config.interpretation);
                }
            }
        }
    }

    fn output_bus(&self, node: NodeId, output: u32) -> Option<&AudioBus> {
        self.nodes
            .get(node as usize)?
            .as_ref()?
            .outputs
            .get(output as usize)
    }

    /// §1.6.3: the param's computedValue for this quantum into its values.
    fn compute_param(&self, p: &mut ParamSlot, frame: u64) {
        if p.computed == frame + 1 {
            return;
        }
        p.computed = frame + 1;
        let mut current = p.intrinsic;
        p.timeline.render(
            frame,
            self.sample_rate,
            p.rate,
            &p.desc,
            &mut p.values[..],
            &mut current,
        );
        p.intrinsic = current;
        p.shared.store(current.to_bits(), Ordering::Relaxed);
        if p.rate == Rate::K {
            let v = p.values[0];
            p.values.fill(v);
        }
        if p.inputs.is_empty() {
            return;
        }
        p.sum.zero(1);
        for (from, out) in &p.inputs {
            if let Some(src) = self.output_bus(*from, *out) {
                p.sum.sum_from(src, crate::bus::Interpretation::Speakers);
            }
        }
        let input = p.sum.channel(0);
        match p.rate {
            Rate::A => {
                for (v, x) in p.values.iter_mut().zip(input) {
                    *v += x;
                }
            }
            Rate::K => {
                let v = p.values[0] + input[0];
                p.values.fill(v);
            }
        }
        for v in p.values.iter_mut() {
            if v.is_nan() {
                *v = p.desc.default;
            }
            *v = p.desc.clamp(*v);
        }
    }

    /// The destination's output: the last quantum rendered.
    pub fn output(&self) -> &AudioBus {
        self.output_bus(DESTINATION, 0)
            .expect("the destination exists")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{CountMode, Interpretation};
    use crate::node::Schedule;

    fn param(id: ParamId, default: f32, rate: Rate) -> NewParam {
        NewParam {
            id,
            desc: ParamDesc::unbounded(default, rate),
            value: default,
            shared: SharedValue::default(),
        }
    }

    fn graph(channels: u32) -> Graph {
        let mut g = Graph::new(128.0);
        g.apply(
            Control::AddNode {
                id: DESTINATION,
                kind: Kind::Destination,
                inputs: 1,
                outputs: 1,
                config: ChannelConfig::new(channels, CountMode::Explicit, Interpretation::Speakers),
                params: vec![],
            },
            0.0,
        );
        g
    }

    fn constant(g: &mut Graph, id: NodeId, p: ParamId, offset: f32) {
        g.apply(
            Control::AddNode {
                id,
                kind: Kind::ConstantSource(Schedule::default()),
                inputs: 0,
                outputs: 1,
                config: ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
                params: vec![param(p, offset, Rate::A)],
            },
            0.0,
        );
        g.apply(
            Control::Start {
                node: id,
                when: 0.0,
                offset: 0.0,
                duration: None,
            },
            0.0,
        );
    }

    fn gain(g: &mut Graph, id: NodeId, p: ParamId, value: f32) {
        g.apply(
            Control::AddNode {
                id,
                kind: Kind::Gain,
                inputs: 1,
                outputs: 1,
                config: ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
                params: vec![param(p, value, Rate::A)],
            },
            0.0,
        );
    }

    fn connect(g: &mut Graph, from: NodeId, to: NodeId) {
        g.apply(
            Control::Connect {
                from,
                output: 0,
                to,
                input: 0,
            },
            0.0,
        );
    }

    #[test]
    fn sources_sum_into_a_gain_and_up_mix_to_the_destination() {
        let mut g = graph(2);
        constant(&mut g, 1, 0, 0.25);
        constant(&mut g, 2, 1, 0.5);
        gain(&mut g, 3, 2, 2.0);
        connect(&mut g, 1, 3);
        connect(&mut g, 2, 3);
        connect(&mut g, 3, DESTINATION);
        g.render(0);
        let out = g.output();
        assert_eq!(out.channels(), 2);
        assert_eq!(out.channel(0)[5], 1.5);
        assert_eq!(out.channel(1)[127], 1.5);
    }

    #[test]
    fn a_node_output_into_a_param_adds_to_its_value() {
        let mut g = graph(1);
        constant(&mut g, 1, 0, 1.0);
        gain(&mut g, 2, 1, 0.5);
        constant(&mut g, 3, 2, 0.25);
        connect(&mut g, 1, 2);
        connect(&mut g, 2, DESTINATION);
        g.apply(
            Control::ConnectParam {
                from: 3,
                output: 0,
                param: 1,
            },
            0.0,
        );
        g.render(0);
        assert_eq!(g.output().channel(0)[0], 0.75);
    }

    #[test]
    fn a_cycle_is_muted_and_a_disconnect_silences() {
        let mut g = graph(1);
        constant(&mut g, 1, 0, 1.0);
        gain(&mut g, 2, 1, 1.0);
        gain(&mut g, 3, 2, 1.0);
        connect(&mut g, 1, 2);
        connect(&mut g, 2, 3);
        connect(&mut g, 3, 2);
        connect(&mut g, 3, DESTINATION);
        g.render(0);
        assert_eq!(g.output().channel(0)[0], 0.0);
        g.apply(
            Control::Disconnect {
                from: 3,
                output: 0,
                to: 2,
                input: 0,
            },
            0.0,
        );
        g.render(128);
        assert_eq!(g.output().channel(0)[0], 1.0);
        g.apply(
            Control::Disconnect {
                from: 3,
                output: 0,
                to: DESTINATION,
                input: 0,
            },
            0.0,
        );
        g.render(256);
        assert_eq!(g.output().channel(0)[0], 0.0);
    }

    #[test]
    fn a_stopped_source_reports_ended_once() {
        let mut g = graph(1);
        constant(&mut g, 1, 0, 1.0);
        connect(&mut g, 1, DESTINATION);
        g.apply(Control::Stop { node: 1, when: 1.5 }, 0.0);
        g.render(0);
        assert!(g.take_notices().is_empty());
        g.render(128);
        assert_eq!(g.output().channel(0)[63], 1.0);
        assert_eq!(g.output().channel(0)[64], 0.0);
        assert_eq!(g.take_notices(), [Notice::Ended(1)]);
        g.render(256);
        assert!(g.take_notices().is_empty());
    }

    #[test]
    fn a_delay_in_a_cycle_feeds_back_a_quantum_later_instead_of_muting() {
        let mut g = graph(1);
        constant(&mut g, 1, 0, 1.0);
        g.apply(Control::Stop { node: 1, when: 1.0 }, 0.0);
        gain(&mut g, 2, 1, 0.5);
        g.apply(
            Control::AddNode {
                id: 3,
                kind: Kind::Delay(Box::new(crate::delay::Delay::new(2.0, 128.0))),
                inputs: 1,
                outputs: 1,
                config: ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
                params: vec![NewParam {
                    id: 2,
                    desc: ParamDesc::unbounded(0.0, Rate::A).range(0.0, 2.0),
                    value: 1.0,
                    shared: SharedValue::default(),
                }],
            },
            0.0,
        );
        connect(&mut g, 1, 3);
        connect(&mut g, 3, 2);
        connect(&mut g, 2, 3);
        connect(&mut g, 3, DESTINATION);
        let mut got = Vec::new();
        for q in 0..3 {
            g.render(q * 128);
            got.push(g.output().channel(0)[7]);
        }
        assert_eq!(got, [0.0, 1.0, 0.5]);
    }

    #[test]
    fn a_merger_and_a_splitter_route_channels() {
        let mut g = graph(1);
        constant(&mut g, 1, 0, 1.0);
        constant(&mut g, 2, 1, 0.5);
        g.apply(
            Control::AddNode {
                id: 3,
                kind: Kind::Merger,
                inputs: 2,
                outputs: 1,
                config: ChannelConfig::new(1, CountMode::Explicit, Interpretation::Speakers),
                params: vec![],
            },
            0.0,
        );
        g.apply(
            Control::AddNode {
                id: 4,
                kind: Kind::Splitter,
                inputs: 1,
                outputs: 2,
                config: ChannelConfig::new(2, CountMode::Explicit, Interpretation::Discrete),
                params: vec![],
            },
            0.0,
        );
        connect(&mut g, 1, 3);
        g.apply(
            Control::Connect {
                from: 2,
                output: 0,
                to: 3,
                input: 1,
            },
            0.0,
        );
        connect(&mut g, 3, 4);
        g.apply(
            Control::Connect {
                from: 4,
                output: 1,
                to: DESTINATION,
                input: 0,
            },
            0.0,
        );
        g.render(0);
        assert_eq!(g.output().channel(0)[3], 0.5);
    }

    #[test]
    fn a_node_without_connected_inputs_does_not_process() {
        // §2.4: a compressor nothing ever fed is not actively processing, so it meters nothing.
        let mut g = graph(1);
        let reduction = Arc::new(AtomicU32::new(0));
        let k = |d| ParamDesc::unbounded(d, Rate::K);
        let params = [-24.0, 30.0, 12.0, 0.003, 0.25]
            .iter()
            .enumerate()
            .map(|(i, d)| NewParam {
                id: i as ParamId,
                desc: k(*d),
                value: *d,
                shared: SharedValue::default(),
            })
            .collect();
        g.apply(
            Control::AddNode {
                id: 1,
                kind: Kind::Compressor(
                    Box::new(crate::compressor::Compressor::new(48000.0)),
                    reduction.clone(),
                ),
                inputs: 1,
                outputs: 1,
                config: ChannelConfig::new(2, CountMode::ClampedMax, Interpretation::Speakers),
                params,
            },
            0.0,
        );
        connect(&mut g, 1, DESTINATION);
        for q in 0..40 {
            g.render(q * 128);
        }
        assert_eq!(f32::from_bits(reduction.load(Ordering::Relaxed)), 0.0);
    }
}
