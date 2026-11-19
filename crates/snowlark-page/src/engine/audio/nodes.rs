// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Audio API 1.1's processing nodes past the core, on the control thread: BiquadFilterNode,
//! DelayNode, ChannelSplitterNode and ChannelMergerNode, StereoPannerNode, PannerNode with the
//! context's AudioListener, WaveShaperNode, IIRFilterNode, AnalyserNode and ConvolverNode. What
//! they render is in `snowlark-audio`; here are their attributes, checks and options.

use snowlark_audio::analyser::{time_to_byte, Analyser, AnalyserInput, MAX_FFT};
use snowlark_audio::biquad::{Biquad, Coefs, FilterType};
use snowlark_audio::convolver::{normalization_scale, Convolver};
use snowlark_audio::delay::Delay;
use snowlark_audio::iir::{Iir, IirCoefs, MAX_ORDER};
use snowlark_audio::panner::{DistanceModel, PannerSettings};
use snowlark_audio::shaper::{Oversample, Shaper};
use snowlark_bindings::generated::{
    AnalyserNodeImpl, AnalyserOptions, AudioListenerImpl, BiquadFilterNodeImpl,
    BiquadFilterOptions, BiquadFilterType, ChannelMergerNodeImpl, ChannelMergerOptions,
    ChannelSplitterNodeImpl, ChannelSplitterOptions, ConvolverNodeImpl, ConvolverOptions,
    DelayNodeImpl, DelayOptions, DistanceModelType, IIRFilterNodeImpl, IIRFilterOptions,
    OverSampleType, PannerNodeImpl, PannerOptions, PanningModelType, StereoPannerNodeImpl,
    StereoPannerOptions, WaveShaperNodeImpl, WaveShaperOptions,
};

use super::*;

/// What a node of these kinds keeps beyond the common [`NodeData`].
pub enum Extra {
    None,
    Biquad(BiquadFilterType),
    Shaper {
        curve: Option<Vec<f32>>,
        oversample: OverSampleType,
    },
    Panner {
        model: PanningModelType,
        settings: PannerSettings,
    },
    Iir(IirCoefs),
    Analyser(Box<Analyser>),
    Convolver {
        buffer: Option<Handle>,
        normalize: bool,
    },
}

/// The AudioListener of a context: its nine AudioParams.
pub struct ListenerData {
    params: [Handle; 9],
    ids: Vec<ParamId>,
}

fn index_size_error(message: &'static str) -> rt::Error {
    exception(ExceptionName::IndexSizeError, message)
}

/// The largest detune whose computed frequency is still finite: `1200 log2(FLT_MAX)`.
fn max_detune() -> f32 {
    1200.0 * f32::MAX.log2()
}

fn filter_type(t: BiquadFilterType) -> FilterType {
    match t {
        BiquadFilterType::Lowpass => FilterType::Lowpass,
        BiquadFilterType::Highpass => FilterType::Highpass,
        BiquadFilterType::Bandpass => FilterType::Bandpass,
        BiquadFilterType::Lowshelf => FilterType::Lowshelf,
        BiquadFilterType::Highshelf => FilterType::Highshelf,
        BiquadFilterType::Peaking => FilterType::Peaking,
        BiquadFilterType::Notch => FilterType::Notch,
        BiquadFilterType::Allpass => FilterType::Allpass,
    }
}

fn oversample(o: OverSampleType) -> Oversample {
    match o {
        OverSampleType::None => Oversample::None,
        OverSampleType::_2x => Oversample::X2,
        OverSampleType::_4x => Oversample::X4,
    }
}

fn distance_model(d: DistanceModelType) -> DistanceModel {
    match d {
        DistanceModelType::Linear => DistanceModel::Linear,
        DistanceModelType::Inverse => DistanceModel::Inverse,
        DistanceModelType::Exponential => DistanceModel::Exponential,
    }
}

/// Writes `data` into a typed array of 1-byte elements, as far as it reaches.
fn write_u8(cx: &mut Cx, array: &JsObject, data: &[u8]) {
    let scope = cx.scope();
    let obj = v8::Local::new(scope, array.global());
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(obj) else {
        return;
    };
    let len = view.byte_length();
    let Some(buffer) = view.buffer(scope) else {
        return;
    };
    let store = buffer.get_backing_store();
    let base = view.byte_offset();
    for (i, v) in data.iter().enumerate().take(len) {
        store[base + i].set(*v);
    }
}

/// The element count of a typed array.
fn f32_len(cx: &mut Cx, array: &JsObject) -> usize {
    let scope = cx.scope();
    let obj = v8::Local::new(scope, array.global());
    v8::Local::<v8::ArrayBufferView>::try_from(obj).map_or(0, |v| v.byte_length() / 4)
}

impl Engine {
    fn node_params(&self, node: Handle, i: usize) -> Handle {
        self.audio_node(node, |n| n.params.get(i).copied())
            .flatten()
            .unwrap_or(node)
    }

    fn with_extra<R>(&self, node: Handle, f: impl FnOnce(&mut Extra) -> R) -> Option<R> {
        self.audio_node(node, |n| f(&mut n.extra))
    }

    fn param_now(&self, p: Handle) -> f32 {
        self.audio_param(p, |d| f32::from_bits(d.shared.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    /// The context's AudioListener, made on first use (§1.1.2 `listener`).
    pub(super) fn audio_listener(&self, ctx: Handle) -> Handle {
        if let Some(l) = self.audio_ctx(ctx, |c| c.listener).flatten() {
            return l;
        }
        let first = self
            .audio_ctx(ctx, |c| {
                let p = c.next_param;
                c.next_param += 9;
                p
            })
            .unwrap_or_default();
        let defaults = [0.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, 1.0, 0.0];
        let mut new = Vec::with_capacity(9);
        let mut handles = [ctx; 9];
        for (i, d) in defaults.iter().enumerate() {
            let id = first + i as ParamId;
            let desc = ParamDesc::unbounded(*d, Rate::A);
            let shared = SharedValue::default();
            shared.store(d.to_bits(), Ordering::Relaxed);
            new.push(NewParam {
                id,
                desc,
                value: *d,
                shared: shared.clone(),
            });
            handles[i] = self.alloc(Object::Audio(AudioObject::Param(Box::new(ParamData {
                context: ctx,
                id,
                desc,
                rate: Rate::A,
                shared,
                timeline: Timeline::default(),
            }))));
        }
        let l = self.alloc(Object::Audio(AudioObject::Listener(Box::new(
            ListenerData {
                params: handles,
                ids: (first..first + 9).collect(),
            },
        ))));
        self.audio_ctx(ctx, |c| c.listener = Some(l));
        self.audio_send(ctx, Control::AddParams(new));
        l
    }

    fn listener_param(&self, l: Handle, i: usize) -> Handle {
        match &*self.obj(l) {
            Object::Audio(AudioObject::Listener(d)) => d.params[i],
            _ => l,
        }
    }

    fn node_options(
        &self,
        cx: &mut Cx,
        h: Handle,
        count: Option<u32>,
        mode: Option<ChannelCountMode>,
        interp: Option<ChannelInterpretation>,
    ) -> rt::Result<Handle> {
        self.apply_node_options(cx, h, count, mode, interp)?;
        Ok(h)
    }

    pub(super) fn new_biquad(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        o: &BiquadFilterOptions,
    ) -> rt::Result<Handle> {
        let nyquist = self.audio_ctx(ctx, |c| c.sample_rate).unwrap_or(44100.0) / 2.0;
        let a = |d| ParamDesc::unbounded(d, Rate::A);
        let h = self.new_audio_node(
            ctx,
            NodeKind::Biquad,
            Kind::Biquad(Box::new(Biquad::new(filter_type(o.type_)))),
            (1, 1),
            ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
            &[
                (a(350.0).range(0.0, nyquist), o.frequency),
                (a(0.0).range(-max_detune(), max_detune()), o.detune),
                (a(1.0), o.q),
                (a(0.0).range(f32::MIN, 40.0 * f32::MAX.log10()), o.gain),
            ],
        );
        self.with_extra(h, |e| *e = Extra::Biquad(o.type_));
        self.node_options(
            cx,
            h,
            o.channel_count,
            o.channel_count_mode,
            o.channel_interpretation,
        )
    }

    pub(super) fn new_delay(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        o: &DelayOptions,
    ) -> rt::Result<Handle> {
        // §1.17.1: the maximum delay is above 0 and below three minutes.
        if !(o.max_delay_time > 0.0 && o.max_delay_time < 180.0) {
            return Err(not_supported(
                "The maximum delay time must be above 0 and below 180 seconds",
            ));
        }
        let rate = self.audio_ctx(ctx, |c| c.sample_rate).unwrap_or(44100.0);
        let h = self.new_audio_node(
            ctx,
            NodeKind::Delay,
            Kind::Delay(Box::new(Delay::new(o.max_delay_time, rate as f64))),
            (1, 1),
            ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
            &[(
                ParamDesc::unbounded(0.0, Rate::A).range(0.0, o.max_delay_time as f32),
                o.delay_time as f32,
            )],
        );
        self.node_options(
            cx,
            h,
            o.channel_count,
            o.channel_count_mode,
            o.channel_interpretation,
        )
    }

    pub(super) fn new_splitter(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        o: &ChannelSplitterOptions,
    ) -> rt::Result<Handle> {
        let n = o.number_of_outputs;
        if n == 0 || n > MAX_CHANNELS {
            return Err(index_size_error("The number of outputs is out of range"));
        }
        let h = self.new_audio_node(
            ctx,
            NodeKind::Splitter,
            Kind::Splitter,
            (1, n),
            ChannelConfig::new(n, CountMode::Explicit, Interpretation::Discrete),
            &[],
        );
        self.node_options(
            cx,
            h,
            o.channel_count,
            o.channel_count_mode,
            o.channel_interpretation,
        )
    }

    pub(super) fn new_merger(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        o: &ChannelMergerOptions,
    ) -> rt::Result<Handle> {
        let n = o.number_of_inputs;
        if n == 0 || n > MAX_CHANNELS {
            return Err(index_size_error("The number of inputs is out of range"));
        }
        let h = self.new_audio_node(
            ctx,
            NodeKind::Merger,
            Kind::Merger,
            (n, 1),
            ChannelConfig::new(1, CountMode::Explicit, Interpretation::Speakers),
            &[],
        );
        self.node_options(
            cx,
            h,
            o.channel_count,
            o.channel_count_mode,
            o.channel_interpretation,
        )
    }

    pub(super) fn new_stereo_panner(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        o: &StereoPannerOptions,
    ) -> rt::Result<Handle> {
        let h = self.new_audio_node(
            ctx,
            NodeKind::StereoPanner,
            Kind::StereoPanner,
            (1, 1),
            ChannelConfig::new(2, CountMode::ClampedMax, Interpretation::Speakers),
            &[(ParamDesc::unbounded(0.0, Rate::A).range(-1.0, 1.0), o.pan)],
        );
        self.node_options(
            cx,
            h,
            o.channel_count,
            o.channel_count_mode,
            o.channel_interpretation,
        )
    }

    pub(super) fn new_panner(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        o: &PannerOptions,
    ) -> rt::Result<Handle> {
        let a = |d| ParamDesc::unbounded(d, Rate::A);
        let settings = PannerSettings::default();
        let h = self.new_audio_node(
            ctx,
            NodeKind::Panner,
            Kind::Panner(Box::new(settings)),
            (1, 1),
            ChannelConfig::new(2, CountMode::ClampedMax, Interpretation::Speakers),
            &[
                (a(0.0), o.position_x),
                (a(0.0), o.position_y),
                (a(0.0), o.position_z),
                (a(1.0), o.orientation_x),
                (a(0.0), o.orientation_y),
                (a(0.0), o.orientation_z),
            ],
        );
        self.with_extra(h, |e| {
            *e = Extra::Panner {
                model: o.panning_model,
                settings,
            }
        });
        let listener = self.audio_listener(ctx);
        let ids = match &*self.obj(listener) {
            Object::Audio(AudioObject::Listener(d)) => d.ids.clone(),
            _ => Vec::new(),
        };
        let id = self.audio_node(h, |n| n.id).unwrap_or_default();
        self.audio_send(
            ctx,
            Control::AttachParams {
                node: id,
                params: ids,
            },
        );
        self.node_options(
            cx,
            h,
            o.channel_count,
            o.channel_count_mode,
            o.channel_interpretation,
        )?;
        PannerNodeImpl::set_distance_model(self, cx, h, o.distance_model)?;
        PannerNodeImpl::set_ref_distance(self, cx, h, o.ref_distance)?;
        PannerNodeImpl::set_max_distance(self, cx, h, o.max_distance)?;
        PannerNodeImpl::set_rolloff_factor(self, cx, h, o.rolloff_factor)?;
        PannerNodeImpl::set_cone_inner_angle(self, cx, h, o.cone_inner_angle)?;
        PannerNodeImpl::set_cone_outer_angle(self, cx, h, o.cone_outer_angle)?;
        PannerNodeImpl::set_cone_outer_gain(self, cx, h, o.cone_outer_gain)?;
        Ok(h)
    }

    /// Changes a PannerNode's settings and tells the rendering thread.
    fn panner_settings(&self, node: Handle, f: impl FnOnce(&mut PannerSettings)) -> rt::Result<()> {
        let (ctx, id, settings) = self
            .audio_node(node, |n| match &mut n.extra {
                Extra::Panner { settings, .. } => {
                    f(settings);
                    Some((n.context, n.id, *settings))
                }
                _ => None,
            })
            .flatten()
            .ok_or_else(|| invalid_state("Not a PannerNode"))?;
        self.audio_send(ctx, Control::SetPanner { node: id, settings });
        Ok(())
    }

    fn panner_get(&self, node: Handle, f: impl FnOnce(&PannerSettings) -> f64) -> f64 {
        self.audio_node(node, |n| match &n.extra {
            Extra::Panner { settings, .. } => f(settings),
            _ => 0.0,
        })
        .unwrap_or(0.0)
    }

    pub(super) fn new_shaper(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        o: &WaveShaperOptions,
    ) -> rt::Result<Handle> {
        let h = self.new_audio_node(
            ctx,
            NodeKind::Shaper,
            Kind::Shaper(Box::new(Shaper::new())),
            (1, 1),
            ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
            &[],
        );
        self.with_extra(h, |e| {
            *e = Extra::Shaper {
                curve: None,
                oversample: OverSampleType::None,
            }
        });
        if let Some(curve) = &o.curve {
            self.set_shaper(h, Some(curve.clone()), None)?;
        }
        self.set_shaper(h, None, Some(o.oversample))?;
        self.node_options(
            cx,
            h,
            o.channel_count,
            o.channel_count_mode,
            o.channel_interpretation,
        )
    }

    /// Sets a WaveShaperNode's curve (`Some`) and/or oversampling.
    fn set_shaper(
        &self,
        node: Handle,
        curve: Option<Vec<f32>>,
        over: Option<OverSampleType>,
    ) -> rt::Result<()> {
        if curve.as_ref().is_some_and(|c| c.len() < 2) {
            return Err(invalid_state("A curve has at least two values"));
        }
        let (ctx, id, curve, over) = self
            .audio_node(node, |n| match &mut n.extra {
                Extra::Shaper {
                    curve: c,
                    oversample: o,
                } => {
                    if let Some(new) = curve {
                        *c = Some(new);
                    }
                    if let Some(new) = over {
                        *o = new;
                    }
                    Some((n.context, n.id, c.clone(), *o))
                }
                _ => None,
            })
            .flatten()
            .ok_or_else(|| invalid_state("Not a WaveShaperNode"))?;
        self.audio_send(
            ctx,
            Control::SetShaper {
                node: id,
                curve: curve.map(Arc::from),
                oversample: oversample(over),
            },
        );
        Ok(())
    }

    pub(super) fn new_iir(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        o: &IIRFilterOptions,
    ) -> rt::Result<Handle> {
        // §1.24.2.
        for (c, name) in [(&o.feedforward, "feedforward"), (&o.feedback, "feedback")] {
            if c.is_empty() || c.len() > MAX_ORDER {
                return Err(not_supported(if name == "feedforward" {
                    "There must be 1 to 20 feedforward coefficients"
                } else {
                    "There must be 1 to 20 feedback coefficients"
                }));
            }
        }
        if o.feedforward.iter().all(|b| *b == 0.0) {
            return Err(invalid_state("The feedforward coefficients are all zero"));
        }
        if o.feedback[0] == 0.0 {
            return Err(invalid_state("The first feedback coefficient is zero"));
        }
        let coefs = IirCoefs::new(&o.feedforward, &o.feedback);
        let h = self.new_audio_node(
            ctx,
            NodeKind::Iir,
            Kind::Iir(Box::new(Iir::new(coefs.clone()))),
            (1, 1),
            ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
            &[],
        );
        self.with_extra(h, |e| *e = Extra::Iir(coefs));
        self.node_options(
            cx,
            h,
            o.channel_count,
            o.channel_count_mode,
            o.channel_interpretation,
        )
    }

    pub(super) fn new_analyser(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        o: &AnalyserOptions,
    ) -> rt::Result<Handle> {
        let input = AnalyserInput::default();
        let h = self.new_audio_node(
            ctx,
            NodeKind::Analyser,
            Kind::Analyser(input.clone(), Box::default()),
            (1, 1),
            ChannelConfig::new(2, CountMode::Max, Interpretation::Speakers),
            &[],
        );
        self.with_extra(h, |e| *e = Extra::Analyser(Box::new(Analyser::new(input))));
        self.node_options(
            cx,
            h,
            o.channel_count,
            o.channel_count_mode,
            o.channel_interpretation,
        )?;
        AnalyserNodeImpl::set_fft_size(self, cx, h, o.fft_size)?;
        if o.min_decibels >= o.max_decibels {
            return Err(index_size_error(
                "minDecibels must be less than maxDecibels",
            ));
        }
        self.analyser(h, |a| {
            a.min_decibels = o.min_decibels;
            a.max_decibels = o.max_decibels;
        });
        AnalyserNodeImpl::set_smoothing_time_constant(self, cx, h, o.smoothing_time_constant)?;
        Ok(h)
    }

    fn analyser<R>(&self, node: Handle, f: impl FnOnce(&mut Analyser) -> R) -> Option<R> {
        self.audio_node(node, |n| match &mut n.extra {
            Extra::Analyser(a) => Some(f(a)),
            _ => None,
        })
        .flatten()
    }

    pub(super) fn new_convolver(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        o: &ConvolverOptions,
    ) -> rt::Result<Handle> {
        let h = self.new_audio_node(
            ctx,
            NodeKind::Convolver,
            Kind::Convolver(None),
            (1, 1),
            ChannelConfig::new(2, CountMode::ClampedMax, Interpretation::Speakers),
            &[],
        );
        self.with_extra(h, |e| {
            *e = Extra::Convolver {
                buffer: None,
                normalize: !o.disable_normalization,
            }
        });
        self.node_options(
            cx,
            h,
            o.channel_count,
            o.channel_count_mode,
            o.channel_interpretation,
        )?;
        if o.buffer.is_some() {
            ConvolverNodeImpl::set_buffer(self, cx, h, o.buffer)?;
        }
        Ok(h)
    }

    /// `getFrequencyResponse()`'s shared steps: the three arrays have one length, and each
    /// frequency's response is written.
    fn frequency_response(
        &self,
        cx: &mut Cx,
        frequency_hz: &JsObject,
        mag: &JsObject,
        phase: &JsObject,
        response: impl Fn(f64) -> (f32, f32),
    ) -> rt::Result<()> {
        let n = f32_len(cx, frequency_hz);
        if f32_len(cx, mag) != n || f32_len(cx, phase) != n {
            return Err(invalid_access(
                "The frequency, magnitude and phase arrays must have the same length",
            ));
        }
        let freqs = read_f32(cx, frequency_hz);
        let (m, p): (Vec<f32>, Vec<f32>) = freqs.iter().map(|f| response(*f as f64)).unzip();
        write_f32(cx, mag, 0, &m);
        write_f32(cx, phase, 0, &p);
        Ok(())
    }
}

impl BiquadFilterNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: BiquadFilterOptions,
    ) -> rt::Result<Handle> {
        self.new_biquad(cx, context, &options)
    }

    fn type_(&self, this: Handle) -> BiquadFilterType {
        self.audio_node(this, |n| match n.extra {
            Extra::Biquad(t) => t,
            _ => BiquadFilterType::Lowpass,
        })
        .unwrap_or(BiquadFilterType::Lowpass)
    }

    fn set_type(&self, _cx: &mut Cx, this: Handle, value: BiquadFilterType) -> rt::Result<()> {
        if let Some((ctx, id)) = self.audio_node(this, |n| {
            n.extra = Extra::Biquad(value);
            (n.context, n.id)
        }) {
            self.audio_send(
                ctx,
                Control::SetFilterType {
                    node: id,
                    kind: filter_type(value),
                },
            );
        }
        Ok(())
    }

    fn frequency(&self, this: Handle) -> Handle {
        self.node_params(this, 0)
    }

    fn detune(&self, this: Handle) -> Handle {
        self.node_params(this, 1)
    }

    fn q(&self, this: Handle) -> Handle {
        self.node_params(this, 2)
    }

    fn gain(&self, this: Handle) -> Handle {
        self.node_params(this, 3)
    }

    /// §1.13.3: the response of the filter as the current parameter values make it.
    fn get_frequency_response(
        &self,
        cx: &mut Cx,
        this: Handle,
        frequency_hz: JsObject,
        mag_response: JsObject,
        phase_response: JsObject,
    ) -> rt::Result<()> {
        let kind = filter_type(BiquadFilterNodeImpl::type_(self, this));
        let ctx = self
            .node_context(this)
            .ok_or_else(|| invalid_state("Not a BiquadFilterNode"))?;
        let rate = self.audio_ctx(ctx, |c| c.sample_rate).unwrap_or(44100.0) as f64;
        let v = |i| self.param_now(self.node_params(this, i)) as f64;
        let frequency = v(0) * 2f64.powf(v(1) / 1200.0);
        let coefs = Coefs::new(kind, frequency, v(2), v(3), rate);
        self.frequency_response(cx, &frequency_hz, &mag_response, &phase_response, |f| {
            coefs.response(f, rate)
        })
    }
}

impl DelayNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: DelayOptions,
    ) -> rt::Result<Handle> {
        self.new_delay(cx, context, &options)
    }

    fn delay_time(&self, this: Handle) -> Handle {
        self.node_params(this, 0)
    }
}

impl ChannelSplitterNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: ChannelSplitterOptions,
    ) -> rt::Result<Handle> {
        self.new_splitter(cx, context, &options)
    }
}

impl ChannelMergerNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: ChannelMergerOptions,
    ) -> rt::Result<Handle> {
        self.new_merger(cx, context, &options)
    }
}

impl StereoPannerNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: StereoPannerOptions,
    ) -> rt::Result<Handle> {
        self.new_stereo_panner(cx, context, &options)
    }

    fn pan(&self, this: Handle) -> Handle {
        self.node_params(this, 0)
    }
}

impl PannerNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: PannerOptions,
    ) -> rt::Result<Handle> {
        self.new_panner(cx, context, &options)
    }

    fn panning_model(&self, this: Handle) -> PanningModelType {
        self.audio_node(this, |n| match n.extra {
            Extra::Panner { model, .. } => model,
            _ => PanningModelType::Equalpower,
        })
        .unwrap_or(PanningModelType::Equalpower)
    }

    /// `panningModel`: "HRTF" is kept, and renders with the equal-power model (no HRTF database
    /// yet).
    fn set_panning_model(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: PanningModelType,
    ) -> rt::Result<()> {
        self.audio_node(this, |n| {
            if let Extra::Panner { model, .. } = &mut n.extra {
                *model = value;
            }
        });
        Ok(())
    }

    fn position_x(&self, this: Handle) -> Handle {
        self.node_params(this, 0)
    }

    fn position_y(&self, this: Handle) -> Handle {
        self.node_params(this, 1)
    }

    fn position_z(&self, this: Handle) -> Handle {
        self.node_params(this, 2)
    }

    fn orientation_x(&self, this: Handle) -> Handle {
        self.node_params(this, 3)
    }

    fn orientation_y(&self, this: Handle) -> Handle {
        self.node_params(this, 4)
    }

    fn orientation_z(&self, this: Handle) -> Handle {
        self.node_params(this, 5)
    }

    fn distance_model(&self, this: Handle) -> DistanceModelType {
        match self
            .audio_node(this, |n| match &n.extra {
                Extra::Panner { settings, .. } => settings.distance_model,
                _ => DistanceModel::Inverse,
            })
            .unwrap_or(DistanceModel::Inverse)
        {
            DistanceModel::Linear => DistanceModelType::Linear,
            DistanceModel::Inverse => DistanceModelType::Inverse,
            DistanceModel::Exponential => DistanceModelType::Exponential,
        }
    }

    fn set_distance_model(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: DistanceModelType,
    ) -> rt::Result<()> {
        self.panner_settings(this, |s| s.distance_model = distance_model(value))
    }

    fn ref_distance(&self, this: Handle) -> f64 {
        self.panner_get(this, |s| s.ref_distance)
    }

    fn set_ref_distance(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value < 0.0 {
            return Err(rt::Error::Range("refDistance is negative".into()));
        }
        self.panner_settings(this, |s| s.ref_distance = value)
    }

    fn max_distance(&self, this: Handle) -> f64 {
        self.panner_get(this, |s| s.max_distance)
    }

    fn set_max_distance(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value <= 0.0 {
            return Err(rt::Error::Range("maxDistance is not positive".into()));
        }
        self.panner_settings(this, |s| s.max_distance = value)
    }

    fn rolloff_factor(&self, this: Handle) -> f64 {
        self.panner_get(this, |s| s.rolloff_factor)
    }

    fn set_rolloff_factor(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if value < 0.0 {
            return Err(rt::Error::Range("rolloffFactor is negative".into()));
        }
        self.panner_settings(this, |s| s.rolloff_factor = value)
    }

    fn cone_inner_angle(&self, this: Handle) -> f64 {
        self.panner_get(this, |s| s.cone_inner_angle)
    }

    fn set_cone_inner_angle(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.panner_settings(this, |s| s.cone_inner_angle = value)
    }

    fn cone_outer_angle(&self, this: Handle) -> f64 {
        self.panner_get(this, |s| s.cone_outer_angle)
    }

    fn set_cone_outer_angle(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.panner_settings(this, |s| s.cone_outer_angle = value)
    }

    fn cone_outer_gain(&self, this: Handle) -> f64 {
        self.panner_get(this, |s| s.cone_outer_gain)
    }

    fn set_cone_outer_gain(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        if !(0.0..=1.0).contains(&value) {
            return Err(invalid_state("coneOuterGain is outside [0, 1]"));
        }
        self.panner_settings(this, |s| s.cone_outer_gain = value)
    }

    fn set_position(&self, cx: &mut Cx, this: Handle, x: f32, y: f32, z: f32) -> rt::Result<()> {
        for (i, v) in [x, y, z].into_iter().enumerate() {
            AudioParamImpl::set_value(self, cx, self.node_params(this, i), v)?;
        }
        Ok(())
    }

    fn set_orientation(&self, cx: &mut Cx, this: Handle, x: f32, y: f32, z: f32) -> rt::Result<()> {
        for (i, v) in [x, y, z].into_iter().enumerate() {
            AudioParamImpl::set_value(self, cx, self.node_params(this, 3 + i), v)?;
        }
        Ok(())
    }
}

impl AudioListenerImpl for Engine {
    fn position_x(&self, this: Handle) -> Handle {
        self.listener_param(this, 0)
    }

    fn position_y(&self, this: Handle) -> Handle {
        self.listener_param(this, 1)
    }

    fn position_z(&self, this: Handle) -> Handle {
        self.listener_param(this, 2)
    }

    fn forward_x(&self, this: Handle) -> Handle {
        self.listener_param(this, 3)
    }

    fn forward_y(&self, this: Handle) -> Handle {
        self.listener_param(this, 4)
    }

    fn forward_z(&self, this: Handle) -> Handle {
        self.listener_param(this, 5)
    }

    fn up_x(&self, this: Handle) -> Handle {
        self.listener_param(this, 6)
    }

    fn up_y(&self, this: Handle) -> Handle {
        self.listener_param(this, 7)
    }

    fn up_z(&self, this: Handle) -> Handle {
        self.listener_param(this, 8)
    }

    fn set_position(&self, cx: &mut Cx, this: Handle, x: f32, y: f32, z: f32) -> rt::Result<()> {
        for (i, v) in [x, y, z].into_iter().enumerate() {
            AudioParamImpl::set_value(self, cx, self.listener_param(this, i), v)?;
        }
        Ok(())
    }

    fn set_orientation(
        &self,
        cx: &mut Cx,
        this: Handle,
        x: f32,
        y: f32,
        z: f32,
        x_up: f32,
        y_up: f32,
        z_up: f32,
    ) -> rt::Result<()> {
        for (i, v) in [x, y, z, x_up, y_up, z_up].into_iter().enumerate() {
            AudioParamImpl::set_value(self, cx, self.listener_param(this, 3 + i), v)?;
        }
        Ok(())
    }
}

impl WaveShaperNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: WaveShaperOptions,
    ) -> rt::Result<Handle> {
        self.new_shaper(cx, context, &options)
    }

    /// `curve`: a copy of the curve set (§1.33.2).
    fn curve(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<JsObject>> {
        let curve = self
            .audio_node(this, |n| match &n.extra {
                Extra::Shaper { curve, .. } => curve.clone(),
                _ => None,
            })
            .flatten();
        curve.map(|c| new_f32_array(cx, &c)).transpose()
    }

    fn set_curve(&self, cx: &mut Cx, this: Handle, value: Option<JsObject>) -> rt::Result<()> {
        match value {
            Some(array) => {
                let curve = read_f32(cx, &array);
                self.set_shaper(this, Some(curve), None)
            }
            None => {
                let (ctx, id, over) = self
                    .audio_node(this, |n| match &mut n.extra {
                        Extra::Shaper { curve, oversample } => {
                            *curve = None;
                            Some((n.context, n.id, *oversample))
                        }
                        _ => None,
                    })
                    .flatten()
                    .ok_or_else(|| invalid_state("Not a WaveShaperNode"))?;
                self.audio_send(
                    ctx,
                    Control::SetShaper {
                        node: id,
                        curve: None,
                        oversample: oversample(over),
                    },
                );
                Ok(())
            }
        }
    }

    fn oversample(&self, this: Handle) -> OverSampleType {
        self.audio_node(this, |n| match n.extra {
            Extra::Shaper { oversample, .. } => oversample,
            _ => OverSampleType::None,
        })
        .unwrap_or(OverSampleType::None)
    }

    fn set_oversample(&self, _cx: &mut Cx, this: Handle, value: OverSampleType) -> rt::Result<()> {
        self.set_shaper(this, None, Some(value))
    }
}

impl IIRFilterNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: IIRFilterOptions,
    ) -> rt::Result<Handle> {
        self.new_iir(cx, context, &options)
    }

    fn get_frequency_response(
        &self,
        cx: &mut Cx,
        this: Handle,
        frequency_hz: JsObject,
        mag_response: JsObject,
        phase_response: JsObject,
    ) -> rt::Result<()> {
        let coefs = self
            .audio_node(this, |n| match &n.extra {
                Extra::Iir(c) => Some(c.clone()),
                _ => None,
            })
            .flatten()
            .ok_or_else(|| invalid_state("Not an IIRFilterNode"))?;
        let ctx = self.node_context(this).unwrap_or(this);
        let rate = self.audio_ctx(ctx, |c| c.sample_rate).unwrap_or(44100.0) as f64;
        self.frequency_response(cx, &frequency_hz, &mag_response, &phase_response, |f| {
            coefs.response(f, rate)
        })
    }
}

impl AnalyserNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: AnalyserOptions,
    ) -> rt::Result<Handle> {
        self.new_analyser(cx, context, &options)
    }

    fn fft_size(&self, this: Handle) -> u32 {
        self.analyser(this, |a| a.fft_size as u32).unwrap_or(2048)
    }

    /// §1.11.3 `fftSize`: a power of two from 32 to 32768.
    fn set_fft_size(&self, _cx: &mut Cx, this: Handle, value: u32) -> rt::Result<()> {
        let v = value as usize;
        if !(32..=MAX_FFT).contains(&v) || !v.is_power_of_two() {
            return Err(index_size_error(
                "fftSize must be a power of two from 32 to 32768",
            ));
        }
        self.analyser(this, |a| a.set_fft_size(v));
        Ok(())
    }

    fn frequency_bin_count(&self, this: Handle) -> u32 {
        self.analyser(this, |a| a.frequency_bin_count() as u32)
            .unwrap_or(1024)
    }

    fn min_decibels(&self, this: Handle) -> f64 {
        self.analyser(this, |a| a.min_decibels).unwrap_or(-100.0)
    }

    fn set_min_decibels(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        let max = AnalyserNodeImpl::max_decibels(self, this);
        if value >= max {
            return Err(index_size_error("minDecibels must be below maxDecibels"));
        }
        self.analyser(this, |a| a.min_decibels = value);
        Ok(())
    }

    fn max_decibels(&self, this: Handle) -> f64 {
        self.analyser(this, |a| a.max_decibels).unwrap_or(-30.0)
    }

    fn set_max_decibels(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        let min = AnalyserNodeImpl::min_decibels(self, this);
        if value <= min {
            return Err(index_size_error("maxDecibels must be above minDecibels"));
        }
        self.analyser(this, |a| a.max_decibels = value);
        Ok(())
    }

    fn smoothing_time_constant(&self, this: Handle) -> f64 {
        self.analyser(this, |a| a.smoothing).unwrap_or(0.8)
    }

    fn set_smoothing_time_constant(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: f64,
    ) -> rt::Result<()> {
        if !(0.0..=1.0).contains(&value) {
            return Err(index_size_error(
                "smoothingTimeConstant must be from 0 to 1",
            ));
        }
        self.analyser(this, |a| a.smoothing = value);
        Ok(())
    }

    fn get_float_frequency_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        array: JsObject,
    ) -> rt::Result<()> {
        if let Some(db) = self.analyser(this, |a| a.frequency_db()) {
            write_f32(cx, &array, 0, &db);
        }
        Ok(())
    }

    fn get_byte_frequency_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        array: JsObject,
    ) -> rt::Result<()> {
        if let Some(bytes) = self.analyser(this, |a| {
            let db = a.frequency_db();
            db.iter().map(|d| a.to_byte(*d)).collect::<Vec<u8>>()
        }) {
            write_u8(cx, &array, &bytes);
        }
        Ok(())
    }

    fn get_float_time_domain_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        array: JsObject,
    ) -> rt::Result<()> {
        if let Some(t) = self.analyser(this, |a| a.time_domain()) {
            write_f32(cx, &array, 0, &t);
        }
        Ok(())
    }

    fn get_byte_time_domain_data(
        &self,
        cx: &mut Cx,
        this: Handle,
        array: JsObject,
    ) -> rt::Result<()> {
        if let Some(t) = self.analyser(this, |a| a.time_domain()) {
            let bytes: Vec<u8> = t.iter().map(|x| time_to_byte(*x)).collect();
            write_u8(cx, &array, &bytes);
        }
        Ok(())
    }
}

impl ConvolverNodeImpl for Engine {
    fn constructor(
        &self,
        cx: &mut Cx,
        context: Handle,
        options: ConvolverOptions,
    ) -> rt::Result<Handle> {
        self.new_convolver(cx, context, &options)
    }

    fn buffer(&self, this: Handle) -> Option<Handle> {
        self.audio_node(this, |n| match n.extra {
            Extra::Convolver { buffer, .. } => buffer,
            _ => None,
        })
        .flatten()
    }

    /// §1.16.2 `buffer`: 1, 2 or 4 channels at the context's rate; the response is copied and,
    /// with `normalize`, scaled.
    fn set_buffer(&self, cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
        let (ctx, id, normalize) = self
            .audio_node(this, |n| match n.extra {
                Extra::Convolver { normalize, .. } => Some((n.context, n.id, normalize)),
                _ => None,
            })
            .flatten()
            .ok_or_else(|| invalid_state("Not a ConvolverNode"))?;
        let convolver = match value {
            None => None,
            Some(b) => {
                let rate = self.audio_ctx(ctx, |c| c.sample_rate).unwrap_or(0.0);
                let data = self
                    .acquire_buffer(cx, b)
                    .ok_or_else(|| invalid_state("Not an AudioBuffer"))?;
                if !matches!(data.channels.len(), 1 | 2 | 4) {
                    return Err(not_supported("The buffer must have 1, 2 or 4 channels"));
                }
                if data.sample_rate != rate {
                    return Err(not_supported(
                        "The buffer's sample rate differs from the context's",
                    ));
                }
                let scale = if normalize {
                    normalization_scale(&data.channels, data.sample_rate)
                } else {
                    1.0
                };
                Some(Box::new(Convolver::new(&data.channels, scale)))
            }
        };
        self.audio_node(this, |n| {
            if let Extra::Convolver { buffer, .. } = &mut n.extra {
                *buffer = value;
            }
        });
        self.audio_send(
            ctx,
            Control::SetConvolver {
                node: id,
                convolver,
            },
        );
        Ok(())
    }

    fn normalize(&self, this: Handle) -> bool {
        self.audio_node(this, |n| match n.extra {
            Extra::Convolver { normalize, .. } => normalize,
            _ => true,
        })
        .unwrap_or(true)
    }

    fn set_normalize(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        self.audio_node(this, |n| {
            if let Extra::Convolver { normalize, .. } = &mut n.extra {
                *normalize = value;
            }
        });
        Ok(())
    }
}
