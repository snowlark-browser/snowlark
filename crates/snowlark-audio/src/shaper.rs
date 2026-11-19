// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WaveShaperNode (§1.33): the curve applied to each sample, at the context's rate or oversampled
//! two or four times. Oversampling doubles the rate with a half-band interpolator (the input
//! delayed for the even samples, a 128-tap windowed sinc for the odd ones) and halves it again with
//! a 256-tap windowed-sinc low-pass that only computes every other sample.

use std::f64::consts::PI;
use std::sync::Arc;

use crate::bus::AudioBus;
use crate::RENDER_QUANTUM as N;

/// `OverSampleType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Oversample {
    None,
    X2,
    X4,
}

/// A Blackman window (α = 0.16) at `x` in `[0, 1]`.
fn blackman(x: f64) -> f64 {
    let alpha = 0.16;
    0.5 * (1.0 - alpha) - 0.5 * (2.0 * PI * x).cos() + 0.5 * alpha * (4.0 * PI * x).cos()
}

fn sinc(s: f64) -> f64 {
    if s == 0.0 {
        1.0
    } else {
        s.sin() / s
    }
}

/// A direct-form FIR filter over consecutive blocks.
#[derive(Debug, Clone)]
struct Fir {
    kernel: Vec<f32>,
    /// The last `kernel.len() - 1` inputs, then the current block.
    buf: Vec<f32>,
}

impl Fir {
    fn new(kernel: Vec<f32>) -> Fir {
        Fir {
            buf: vec![0.0; kernel.len() - 1],
            kernel,
        }
    }

    /// `y[n] = Σ h[k] x[n-k]`.
    fn process(&mut self, x: &[f32], y: &mut [f32]) {
        let k = self.kernel.len();
        self.buf.truncate(k - 1);
        self.buf.extend_from_slice(x);
        for (n, out) in y.iter_mut().enumerate() {
            // x[n - j] is buf[k - 1 + n - j].
            let mut s = 0.0f32;
            for (j, h) in self.kernel.iter().enumerate() {
                s += h * self.buf[k - 1 + n - j];
            }
            *out = s;
        }
        let len = self.buf.len();
        self.buf.copy_within(len - (k - 1).., 0);
    }
}

/// Doubles the rate of blocks of `block` frames.
#[derive(Debug, Clone)]
struct UpSampler {
    fir: Fir,
    /// The last 64 inputs, then the current block.
    delayed: Vec<f32>,
    odd: Vec<f32>,
}

const UP_KERNEL: usize = 128;
const DOWN_KERNEL: usize = 256;

impl UpSampler {
    fn new(block: usize) -> UpSampler {
        let n = UP_KERNEL as f64;
        let half = n / 2.0;
        let kernel = (0..UP_KERNEL)
            .map(|i| {
                let i = i as f64;
                (sinc(PI * (i - half + 0.5)) * blackman((i + 0.5) / n)) as f32
            })
            .collect();
        UpSampler {
            fir: Fir::new(kernel),
            delayed: vec![0.0; UP_KERNEL / 2 + block],
            odd: vec![0.0; block],
        }
    }

    fn process(&mut self, x: &[f32], y: &mut [f32]) {
        let half = UP_KERNEL / 2;
        let b = x.len();
        self.delayed[half..half + b].copy_from_slice(x);
        self.fir.process(x, &mut self.odd);
        for i in 0..b {
            y[2 * i] = self.delayed[i];
            y[2 * i + 1] = self.odd[i];
        }
        self.delayed.copy_within(b..b + half, 0);
    }
}

/// Halves the rate of blocks of `block` frames.
#[derive(Debug, Clone)]
struct DownSampler {
    fir: Fir,
    /// The last 128 inputs, then the current block.
    input: Vec<f32>,
    odd: Vec<f32>,
}

impl DownSampler {
    fn new(block: usize) -> DownSampler {
        let n = DOWN_KERNEL as f64;
        let half = n / 2.0;
        // Only the odd taps of the 256-tap half-band filter are non-zero, besides the centre.
        let kernel = (1..DOWN_KERNEL)
            .step_by(2)
            .map(|i| {
                let i = i as f64;
                (0.5 * sinc(0.5 * PI * (i - half)) * blackman(i / n)) as f32
            })
            .collect();
        DownSampler {
            fir: Fir::new(kernel),
            input: vec![0.0; DOWN_KERNEL / 2 + block],
            odd: vec![0.0; block / 2],
        }
    }

    fn process(&mut self, x: &[f32], y: &mut [f32]) {
        let half = DOWN_KERNEL / 2;
        let b = x.len();
        self.input[half..half + b].copy_from_slice(x);
        for i in 0..b / 2 {
            self.odd[i] = self.input[half - 1 + 2 * i];
        }
        let odd = std::mem::take(&mut self.odd);
        self.fir.process(&odd, y);
        self.odd = odd;
        for (i, v) in y.iter_mut().enumerate() {
            *v += 0.5 * self.input[2 * i];
        }
        self.input.copy_within(b..b + half, 0);
    }
}

/// One channel's resamplers.
#[derive(Debug, Clone)]
struct Lane {
    up1: UpSampler,
    up2: UpSampler,
    down1: DownSampler,
    down2: DownSampler,
}

impl Lane {
    fn new() -> Lane {
        Lane {
            up1: UpSampler::new(N),
            up2: UpSampler::new(2 * N),
            down1: DownSampler::new(2 * N),
            down2: DownSampler::new(4 * N),
        }
    }
}

/// A WaveShaperNode's curve, oversampling and resampler state.
#[derive(Debug)]
pub struct Shaper {
    pub curve: Option<Arc<[f32]>>,
    pub oversample: Oversample,
    lanes: Vec<Lane>,
}

/// §1.33: the curve at input `x`.
pub fn shape(curve: &[f32], x: f32) -> f32 {
    let l = curve.len();
    let v = (l - 1) as f32 * 0.5 * (x + 1.0);
    if v.is_nan() {
        return curve[(l - 1) / 2];
    }
    if v <= 0.0 {
        return curve[0];
    }
    if v >= (l - 1) as f32 {
        return curve[l - 1];
    }
    let k = v.floor();
    let f = v - k;
    let k = k as usize;
    (1.0 - f) * curve[k] + f * curve[k + 1]
}

impl Shaper {
    pub fn new() -> Shaper {
        Shaper {
            curve: None,
            oversample: Oversample::None,
            lanes: Vec::new(),
        }
    }

    pub fn process(&mut self, input: &AudioBus, out: &mut AudioBus) {
        let Some(curve) = self.curve.clone() else {
            // No curve: the input passes unchanged.
            out.copy_from(input);
            return;
        };
        let channels = input.channels();
        out.zero(channels);
        if self.oversample != Oversample::None && self.lanes.len() < channels {
            self.lanes.resize_with(channels, Lane::new);
        }
        let mut x2 = [0.0f32; 2 * N];
        let mut x4 = [0.0f32; 4 * N];
        for c in 0..channels {
            let x = input.channel(c);
            let o = out.channel_mut(c);
            match self.oversample {
                Oversample::None => {
                    for (o, x) in o.iter_mut().zip(x) {
                        *o = shape(&curve, *x);
                    }
                }
                Oversample::X2 => {
                    let lane = &mut self.lanes[c];
                    lane.up1.process(x, &mut x2);
                    for v in &mut x2 {
                        *v = shape(&curve, *v);
                    }
                    lane.down1.process(&x2, o);
                }
                Oversample::X4 => {
                    let lane = &mut self.lanes[c];
                    lane.up1.process(x, &mut x2);
                    lane.up2.process(&x2, &mut x4);
                    for v in &mut x4 {
                        *v = shape(&curve, *v);
                    }
                    lane.down2.process(&x4, &mut x2);
                    lane.down1.process(&x2, o);
                }
            }
        }
    }

    /// `oversample` changed: the resamplers start over.
    pub fn set_oversample(&mut self, o: Oversample) {
        if o != self.oversample {
            self.lanes.clear();
        }
        self.oversample = o;
    }
}

impl Default for Shaper {
    fn default() -> Self {
        Shaper::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curve_is_read_with_linear_interpolation_and_clamped() {
        let c = [-1.0, 0.0, 1.0];
        assert_eq!(shape(&c, -2.0), -1.0);
        assert_eq!(shape(&c, 0.5), 0.5);
        assert_eq!(shape(&c, 1.0), 1.0);
        assert_eq!(shape(&c, 3.0), 1.0);
    }

    #[test]
    fn oversampling_an_identity_curve_delays_a_low_tone_without_changing_it() {
        for os in [Oversample::X2, Oversample::X4] {
            let mut s = Shaper::new();
            s.curve = Some(Arc::from(vec![-1.0f32, 1.0]));
            s.set_oversample(os);
            let mut out = AudioBus::new(1);
            let mut seen = Vec::new();
            let tone = |i: usize| (2.0 * PI * i as f64 / 64.0).sin() as f32 * 0.5;
            for q in 0..8 {
                let mut input = AudioBus::new(1);
                for (i, v) in input.channel_mut(0).iter_mut().enumerate() {
                    *v = tone(q * N + i);
                }
                s.process(&input, &mut out);
                seen.extend_from_slice(out.channel(0));
            }
            // Find the latency, then compare.
            let lag = (0..400)
                .min_by(|a, b| {
                    let err = |l: usize| {
                        (600..700)
                            .map(|i| (seen[i] - tone(i - l)).abs())
                            .sum::<f32>()
                    };
                    err(*a).total_cmp(&err(*b))
                })
                .unwrap();
            let max = (600..1000)
                .map(|i| (seen[i] - tone(i - lag)).abs())
                .fold(0.0, f32::max);
            assert!(max < 1e-3, "{os:?}: lag {lag}, error {max}");
        }
    }
}
