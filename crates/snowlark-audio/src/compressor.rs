// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DynamicsCompressorNode (§1.19): a look-ahead compressor. The spec gives the outline (a
//! detector average and a compressor gain per frame, attack and release envelopes, a soft knee,
//! the makeup gain `(1 / fullRangeGain)^0.6`, the reduction metered in decibels once per
//! quantum) and leaves the curves' exact shapes to the implementation; these are the long-standing
//! ones: an exponential knee fitted to the ratio's slope, a 6 ms pre-delay, an adaptive release
//! and a sine-warped gain.

use crate::bus::AudioBus;
use crate::RENDER_QUANTUM as N;

const METERING_RELEASE_TIME_CONSTANT: f32 = 0.325;
const PRE_DELAY: f32 = 0.006;
const MAX_PRE_DELAY_FRAMES: usize = 1024;
const MASK: usize = MAX_PRE_DELAY_FRAMES - 1;
const DEFAULT_PRE_DELAY_FRAMES: usize = 256;
const DIVISION_FRAMES: usize = 32;
const SAT_RELEASE_TIME: f32 = 0.0025;
const PI_OVER_2: f32 = std::f32::consts::FRAC_PI_2;

// The adaptive release: release time scaled by a fourth-order polynomial through four zones.
const Z1: f32 = 0.09;
const Z2: f32 = 0.16;
const Z3: f32 = 0.42;
const Z4: f32 = 0.98;
const A_BASE: f32 = 0.999_999_999_999_999_8 * Z1 + 1.843_221_968_432_392_3e-16 * Z2
    - 1.937_339_435_167_642_3e-16 * Z3
    + 8.824_516_011_816_245e-18 * Z4;
const B_BASE: f32 = -1.578_832_035_284_588_8 * Z1 + 2.330_583_703_207_428_6 * Z2
    - 0.914_119_420_484_042_9 * Z3
    + 0.162_367_752_561_203_2 * Z4;
const C_BASE: f32 = 0.533_414_286_910_642_4 * Z1 - 1.272_736_789_213_631 * Z2
    + 0.925_885_604_220_751_2 * Z3
    - 0.186_563_101_917_762_26 * Z4;
const D_BASE: f32 = 0.087_834_631_382_072_34 * Z1 - 0.169_416_296_792_562_2 * Z2
    + 0.085_880_579_515_952_72 * Z3
    - 0.004_298_914_105_462_83 * Z4;
const E_BASE: f32 = -0.042_416_883_008_123_074 * Z1 + 0.111_569_382_798_760_2 * Z2
    - 0.097_646_763_252_658_72 * Z3
    + 0.028_494_263_462_021_576 * Z4;

fn db_to_linear(db: f32) -> f32 {
    10f32.powf(0.05 * db)
}

fn linear_to_db(x: f32) -> f32 {
    20.0 * x.log10()
}

fn finite_or(x: f32, default: f32) -> f32 {
    if x.is_finite() {
        x
    } else {
        default
    }
}

/// The compressor's parameters for one quantum (all k-rate).
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub threshold: f32,
    pub knee: f32,
    pub ratio: f32,
    pub attack: f32,
    pub release: f32,
}

/// The static curve, recomputed when threshold, knee or ratio change.
#[derive(Clone, Copy, Debug, Default)]
struct Curve {
    db_threshold: f32,
    db_knee: f32,
    ratio: f32,
    linear_threshold: f32,
    slope: f32,
    knee_threshold: f32,
    db_knee_threshold: f32,
    db_yknee_threshold: f32,
    k: f32,
}

impl Curve {
    fn knee_curve(&self, x: f32, k: f32) -> f32 {
        if x < self.linear_threshold {
            return x;
        }
        self.linear_threshold + (1.0 - ((-k * (x - self.linear_threshold)) as f64).exp() as f32) / k
    }

    /// The compression curve: linear below the threshold, the knee, then `1 / ratio` in dB.
    fn saturate(&self, x: f32, k: f32) -> f32 {
        if x < self.knee_threshold {
            return self.knee_curve(x, k);
        }
        let db_x = linear_to_db(x);
        db_to_linear(self.db_yknee_threshold + self.slope * (db_x - self.db_knee_threshold))
    }

    /// The knee's `k` whose slope at the knee's end is `slope` (a geometric search).
    fn k_at_slope(&self, slope_wanted: f32) -> f32 {
        let db_x = self.db_threshold + self.db_knee;
        let x = db_to_linear(db_x);
        let (x2, db_x2) = if x < self.linear_threshold {
            (1.0, 0.0)
        } else {
            let x2 = x * 1.001;
            (x2, linear_to_db(x2))
        };
        let (mut min_k, mut max_k, mut k) = (0.1f32, 10000.0f32, 5.0f32);
        let mut slope = 1.0;
        for _ in 0..15 {
            if x >= self.linear_threshold {
                let db_y = linear_to_db(self.knee_curve(x, k));
                let db_y2 = linear_to_db(self.knee_curve(x2, k));
                slope = (db_y2 - db_y) / (db_x2 - db_x);
            }
            if slope < slope_wanted {
                max_k = k;
            } else {
                min_k = k;
            }
            k = (min_k * max_k).sqrt();
        }
        k
    }

    fn update(&mut self, threshold: f32, knee: f32, ratio: f32) -> f32 {
        if threshold != self.db_threshold || knee != self.db_knee || ratio != self.ratio {
            self.db_threshold = threshold;
            self.linear_threshold = db_to_linear(threshold);
            self.db_knee = knee;
            self.ratio = ratio;
            self.slope = 1.0 / ratio;
            let k = self.k_at_slope(1.0 / ratio);
            self.db_knee_threshold = threshold + knee;
            self.knee_threshold = db_to_linear(self.db_knee_threshold);
            self.db_yknee_threshold = linear_to_db(self.knee_curve(self.knee_threshold, k));
            self.k = k;
        }
        self.k
    }
}

/// A DynamicsCompressorNode's processing state.
#[derive(Debug)]
pub struct Compressor {
    sample_rate: f32,
    curve: Curve,
    detector_average: f32,
    compressor_gain: f32,
    metering_gain: f32,
    metering_release_k: f32,
    max_attack_compression_diff: f32,
    delay: [Box<[f32; MAX_PRE_DELAY_FRAMES]>; 2],
    pre_delay_frames: usize,
    read: usize,
    write: usize,
}

impl Compressor {
    pub fn new(sample_rate: f32) -> Compressor {
        Compressor {
            sample_rate,
            curve: Curve {
                db_threshold: -1.0,
                db_knee: -1.0,
                ratio: -1.0,
                ..Curve::default()
            },
            detector_average: 0.0,
            compressor_gain: 1.0,
            metering_gain: 1.0,
            metering_release_k: (1.0
                - (-1.0 / (sample_rate as f64 * METERING_RELEASE_TIME_CONSTANT as f64)).exp())
                as f32,
            max_attack_compression_diff: -1.0,
            delay: [
                Box::new([0.0; MAX_PRE_DELAY_FRAMES]),
                Box::new([0.0; MAX_PRE_DELAY_FRAMES]),
            ],
            pre_delay_frames: DEFAULT_PRE_DELAY_FRAMES,
            read: 0,
            write: DEFAULT_PRE_DELAY_FRAMES,
        }
    }

    /// Compresses one quantum of `input` into `out` (two channels; a mono input feeds both).
    /// Returns the reduction in decibels, the `reduction` attribute.
    pub fn process(&mut self, s: Settings, input: &AudioBus, out: &mut AudioBus) -> f32 {
        out.zero(2);
        let sr = self.sample_rate;
        let k = self.curve.update(s.threshold, s.knee, s.ratio);
        let post_gain = (1.0 / self.curve.saturate(1.0, k)).powf(0.6);
        let attack_frames = s.attack.max(0.001) * sr;
        let release_frames = sr * s.release;
        let sat_release_frames = SAT_RELEASE_TIME * sr;
        let (a, b, c, d, e) = (
            release_frames * A_BASE,
            release_frames * B_BASE,
            release_frames * C_BASE,
            release_frames * D_BASE,
            release_frames * E_BASE,
        );
        let pre_delay = ((PRE_DELAY * sr) as usize).min(MAX_PRE_DELAY_FRAMES - 1);
        if pre_delay != self.pre_delay_frames {
            self.pre_delay_frames = pre_delay;
            for d in &mut self.delay {
                d.fill(0.0);
            }
            self.read = 0;
            self.write = pre_delay;
        }
        let sources = input.channels();
        let mut frame = 0;
        while frame < N {
            self.detector_average = finite_or(self.detector_average, 1.0);
            let desired_gain = self.detector_average;
            let scaled_desired_gain = desired_gain.asin() / PI_OVER_2;
            let releasing = scaled_desired_gain > self.compressor_gain;
            let mut diff = if scaled_desired_gain == 0.0 {
                if releasing {
                    -1.0
                } else {
                    1.0
                }
            } else {
                linear_to_db(self.compressor_gain / scaled_desired_gain)
            };
            let envelope_rate = if releasing {
                self.max_attack_compression_diff = -1.0;
                diff = finite_or(diff, -1.0);
                let x = 0.25 * (diff.clamp(-12.0, 0.0) + 12.0);
                let (x2, x3, x4) = (x * x, x * x * x, x * x * x * x);
                let frames = a + b * x + c * x2 + d * x3 + e * x4;
                db_to_linear(5.0 / frames)
            } else {
                diff = finite_or(diff, 1.0);
                if self.max_attack_compression_diff == -1.0
                    || self.max_attack_compression_diff < diff
                {
                    self.max_attack_compression_diff = diff;
                }
                let eff = self.max_attack_compression_diff.max(0.5);
                1.0 - (0.25 / eff).powf(1.0 / attack_frames)
            };
            let mut detector_average = self.detector_average;
            let mut gain = self.compressor_gain;
            for _ in 0..DIVISION_FRAMES.min(N - frame) {
                let mut level = 0.0f32;
                for (ch, delay) in self.delay.iter_mut().enumerate() {
                    let v = if sources > ch {
                        input.channel(ch)[frame]
                    } else if sources > 0 {
                        input.channel(0)[frame]
                    } else {
                        0.0
                    };
                    delay[self.write] = v;
                    level = level.max(v.abs());
                }
                let shaped = self.curve.saturate(level, k);
                let attenuation = if level <= 0.0001 { 1.0 } else { shaped / level };
                let db_attenuation = (-linear_to_db(attenuation)).max(2.0);
                let sat_release_rate = db_to_linear(db_attenuation / sat_release_frames) - 1.0;
                let rate = if attenuation > detector_average {
                    sat_release_rate
                } else {
                    1.0
                };
                detector_average += (attenuation - detector_average) * rate;
                detector_average = finite_or(detector_average.min(1.0), 1.0);
                if envelope_rate < 1.0 {
                    gain += (scaled_desired_gain - gain) * envelope_rate;
                } else {
                    gain = (gain * envelope_rate).min(1.0);
                }
                let warped = ((PI_OVER_2 * gain) as f64).sin() as f32;
                let total = post_gain * warped;
                let db_real = linear_to_db(warped);
                if db_real < self.metering_gain {
                    self.metering_gain = db_real;
                } else {
                    self.metering_gain += (db_real - self.metering_gain) * self.metering_release_k;
                }
                for (ch, delay) in self.delay.iter().enumerate() {
                    out.channel_mut(ch)[frame] = delay[self.read] * total;
                }
                frame += 1;
                self.read = (self.read + 1) & MASK;
                self.write = (self.write + 1) & MASK;
            }
            self.detector_average = flush_denormal(detector_average);
            self.compressor_gain = flush_denormal(gain);
        }
        self.metering_gain
    }
}

fn flush_denormal(x: f32) -> f32 {
    if x.is_subnormal() {
        0.0
    } else {
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULTS: Settings = Settings {
        threshold: -24.0,
        knee: 30.0,
        ratio: 12.0,
        attack: 0.003,
        release: 0.25,
    };

    #[test]
    fn the_static_curve_is_linear_below_the_threshold_and_follows_the_ratio_above_the_knee() {
        let mut c = Curve::default();
        let k = c.update(-24.0, 30.0, 12.0);
        let quiet = db_to_linear(-40.0);
        assert_eq!(c.saturate(quiet, k), quiet);
        // Above the knee, 12 dB in gives 1 dB out.
        let y1 = linear_to_db(c.saturate(db_to_linear(8.0), k));
        let y2 = linear_to_db(c.saturate(db_to_linear(20.0), k));
        assert!((y2 - y1 - 1.0).abs() < 1e-3, "{}", y2 - y1);
        // The knee's slope at its end matches the ratio's.
        let x = db_to_linear(6.0);
        let s = (linear_to_db(c.knee_curve(x * 1.001, k)) - linear_to_db(c.knee_curve(x, k)))
            / (linear_to_db(x * 1.001) - 6.0);
        assert!((s - 1.0 / 12.0).abs() < 0.01, "{s}");
    }

    #[test]
    fn silence_stays_silent() {
        let mut comp = Compressor::new(44100.0);
        let input = AudioBus::new(1);
        let mut out = AudioBus::new(2);
        let r = comp.process(DEFAULTS, &input, &mut out);
        assert_eq!(out.channels(), 2);
        assert!(out.channel(0).iter().all(|v| *v == 0.0));
        // (the detector average starts at 0, so the gain dips at first: a start-up reduction)
        assert!(r.is_finite() && r <= 1.0, "{r}");
    }

    #[test]
    fn a_loud_signal_is_delayed_and_reduced() {
        let mut comp = Compressor::new(44100.0);
        let mut input = AudioBus::new(1);
        input.channel_mut(0).fill(1.0);
        let mut out = AudioBus::new(2);
        let mut reduction = 0.0;
        let mut last = 0.0;
        for _ in 0..40 {
            reduction = comp.process(DEFAULTS, &input, &mut out);
            last = out.channel(1)[N - 1];
        }
        // 264 frames of pre-delay at 44.1 kHz, then a reduced level.
        assert!(reduction < -10.0, "{reduction}");
        assert!(last > 0.0 && last < 1.0, "{last}");
        let mut comp = Compressor::new(44100.0);
        comp.process(DEFAULTS, &input, &mut out);
        assert_eq!(out.channel(0)[N - 1], 0.0);
    }
}
