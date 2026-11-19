// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PeriodicWave and OscillatorNode (§1.27, §1.26): band-limited wave tables, built by inverse FFT
//! so that no partial lies above the Nyquist frequency, and the oscillator's phase accumulation
//! through them.

use std::f64::consts::PI;
use std::sync::Arc;

use crate::node::{Quantum, Schedule};
use crate::RENDER_QUANTUM as N;

/// Ranges of wave tables per octave.
const RANGES_PER_OCTAVE: u32 = 3;
const CENTS_PER_RANGE: f32 = 1200.0 / RANGES_PER_OCTAVE as f32;

/// Below this phase increment (table samples per frame) a 3-point, below the next a 5-point
/// interpolation reads the table; above it, 2 points.
const INTERPOLATE_2_POINT: f32 = 0.3;
const INTERPOLATE_3_POINT: f32 = 0.16;

/// The built-in waveforms (`OscillatorType` but "custom").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Sine,
    Square,
    Sawtooth,
    Triangle,
}

/// A PeriodicWave's tables for one sample rate: one per range of fundamental frequencies, each
/// with the partials that stay below Nyquist for the range's highest frequency.
#[derive(Debug)]
pub struct PeriodicWave {
    size: usize,
    lowest_fundamental: f32,
    rate_scale: f32,
    tables: Vec<Vec<f32>>,
}

/// The table size for a sample rate.
fn table_size(sample_rate: f32) -> usize {
    if sample_rate <= 24000.0 {
        2048
    } else if sample_rate <= 88200.0 {
        4096
    } else {
        16384
    }
}

/// In-place radix-2 inverse DFT without scaling: `x[n] = Σ X[k] e^{2πikn/N}`.
fn inverse_fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let step = 2.0 * PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (s, c) = (step * k as f64).sin_cos();
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * c - im[b] * s;
                let ti = re[b] * s + im[b] * c;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len *= 2;
    }
}

impl PeriodicWave {
    /// §1.27.3 "Waveform Generation": the wave `Σ real[k] cos(2πkt) + imag[k] sin(2πkt)` for
    /// k ≥ 1, scaled so its peak is 1 unless normalization is disabled.
    pub fn new(sample_rate: f32, real: &[f32], imag: &[f32], normalize: bool) -> PeriodicWave {
        let size = table_size(sample_rate);
        let half = size / 2;
        let ranges = (RANGES_PER_OCTAVE as f32 * (size as f32).log2()).round() as usize;
        let components = real.len().min(imag.len()).min(half);
        let mut scale = 1.0f64;
        let mut tables = Vec::with_capacity(ranges);
        let mut re = vec![0.0f64; size];
        let mut im = vec![0.0f64; size];
        for range in 0..ranges {
            // The range's partials: those below Nyquist at its highest fundamental.
            let cull = 2f32.powf(-(range as f32 * CENTS_PER_RANGE) / 1200.0);
            let partials = (cull * half as f32) as usize;
            re.fill(0.0);
            im.fill(0.0);
            for k in 1..components.min(partials + 1) {
                re[k] = real[k] as f64;
                im[k] = -(imag[k] as f64);
            }
            inverse_fft(&mut re, &mut im);
            if range == 0 && normalize {
                let max = re.iter().fold(0.0f64, |m, v| m.max(v.abs()));
                if max > 0.0 {
                    scale = 1.0 / max;
                }
            }
            tables.push(re.iter().map(|v| (v * scale) as f32).collect());
        }
        PeriodicWave {
            size,
            lowest_fundamental: sample_rate / size as f32,
            rate_scale: size as f32 / sample_rate,
            tables,
        }
    }

    /// §1.26.1: the built-in waveforms' Fourier coefficients (sine coefficients only).
    pub fn basic(sample_rate: f32, shape: Shape) -> PeriodicWave {
        let half = table_size(sample_rate) / 2;
        let real = vec![0.0f32; half];
        let mut imag = vec![0.0f32; half];
        for (n, b) in imag.iter_mut().enumerate().skip(1) {
            let pi_factor = 2.0 / (n as f64 * PI);
            let odd = n % 2 == 1;
            *b = match shape {
                Shape::Sine => f64::from(n == 1),
                Shape::Square if odd => 2.0 * pi_factor,
                Shape::Square => 0.0,
                Shape::Sawtooth => pi_factor * if odd { 1.0 } else { -1.0 },
                Shape::Triangle if odd => {
                    let sign = if (n - 1) / 2 % 2 == 1 { -1.0 } else { 1.0 };
                    2.0 * pi_factor * pi_factor * sign
                }
                Shape::Triangle => 0.0,
            } as f32;
        }
        PeriodicWave::new(sample_rate, &real, &imag, true)
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// The two tables around `frequency` and the weight of the lower-partials one.
    fn tables_for(&self, frequency: f32) -> (&[f32], &[f32], f32) {
        let f = frequency.abs();
        let ratio = if f > 0.0 {
            f / self.lowest_fundamental
        } else {
            0.5
        };
        let last = (self.tables.len() - 1) as f32;
        let range = (1.0 + ratio.log2() * 1200.0 / CENTS_PER_RANGE).clamp(0.0, last);
        let i1 = range as usize;
        let i2 = (i1 + 1).min(self.tables.len() - 1);
        (&self.tables[i2], &self.tables[i1], range - i1 as f32)
    }

    /// The wave at table position `index` for a phase increment `incr` and fundamental
    /// `frequency`.
    fn sample(&self, index: f64, incr: f32, frequency: f32) -> f32 {
        let (lower, higher, factor) = self.tables_for(frequency);
        let mask = self.size - 1;
        let i0 = index as usize;
        let (lo, hi) = if incr >= INTERPOLATE_2_POINT {
            let t = (index as f32 - i0 as f32) as f64;
            let (a, b) = (i0 & mask, (i0 + 1) & mask);
            (
                (1.0 - t) * lower[a] as f64 + t * lower[b] as f64,
                (1.0 - t) * higher[a] as f64 + t * higher[b] as f64,
            )
        } else if incr >= INTERPOLATE_3_POINT {
            let t = index - i0 as f64;
            let w = [0.5 * t * (t - 1.0), 1.0 - t * t, 0.5 * t * (t + 1.0)];
            let mut s = (0.0, 0.0);
            for (k, w) in w.iter().enumerate() {
                let i = (i0 + k + mask) & mask;
                s.0 += w * lower[i] as f64;
                s.1 += w * higher[i] as f64;
            }
            s
        } else {
            let t = index - i0 as f64;
            let t2 = t * t;
            let w = [
                t * (t2 - 1.0) * (t - 2.0) / 24.0,
                -t * (t - 1.0) * (t2 - 4.0) / 6.0,
                (t2 - 1.0) * (t2 - 4.0) / 4.0,
                -t * (t + 1.0) * (t2 - 4.0) / 6.0,
                t * (t2 - 1.0) * (t + 2.0) / 24.0,
            ];
            let mut s = (0.0, 0.0);
            for (k, w) in w.iter().enumerate() {
                let i = (i0 + k + 2 * mask) & mask;
                s.0 += w * lower[i] as f64;
                s.1 += w * higher[i] as f64;
            }
            s
        };
        ((1.0 - factor) as f64 * hi + factor as f64 * lo) as f32
    }
}

/// An OscillatorNode's state.
#[derive(Debug)]
pub struct Oscillator {
    pub schedule: Schedule,
    pub wave: Arc<PeriodicWave>,
    /// The phase, in table samples.
    phase: f64,
    started: bool,
}

impl Oscillator {
    pub fn new(wave: Arc<PeriodicWave>) -> Oscillator {
        Oscillator {
            schedule: Schedule::default(),
            wave,
            phase: 0.0,
            started: false,
        }
    }

    /// One quantum into `out` (one channel): `params` are frequency and detune. Returns whether
    /// the oscillator ended in this quantum.
    pub fn process(&mut self, q: Quantum, params: &[&[f32; N]], out: &mut [f32; N]) -> bool {
        let (frames, ended) = self.schedule.frames(q);
        let Some((from, to)) = frames else {
            return ended;
        };
        let nyquist = (q.sample_rate / 2.0) as f32;
        let size = self.wave.size() as f64;
        let rate_scale = self.wave.rate_scale;
        for (i, slot) in out.iter_mut().enumerate().take(to).skip(from) {
            // §1.26.2: computedOscillatorFrequency = frequency · 2^(detune / 1200).
            let f = params[0][i] * (params[1][i] / 1200.0).exp2();
            let f = if f.is_nan() {
                nyquist
            } else {
                f.clamp(-nyquist, nyquist)
            };
            let incr = f * rate_scale;
            if !self.started {
                // A start between frames: the phase the wave reached by this frame.
                self.started = true;
                let start = self.schedule.start.unwrap_or(0.0) * q.sample_rate;
                let late = (q.frame + i as u64) as f64 - start;
                self.phase = (late.max(0.0) * incr as f64).rem_euclid(size);
            }
            *slot = self.wave.sample(self.phase, incr.abs(), f);
            self.phase += incr as f64;
            self.phase -= (self.phase / size).floor() * size;
        }
        ended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inverse_fft_sums_the_partials() {
        let n = 16;
        let mut re = vec![0.0; n];
        let mut im = vec![0.0; n];
        re[1] = 0.5;
        im[3] = -2.0;
        inverse_fft(&mut re, &mut im);
        for (i, v) in re.iter().enumerate() {
            let t = 2.0 * PI * i as f64 / n as f64;
            let want = 0.5 * t.cos() + 2.0 * (3.0 * t).sin();
            assert!((v - want).abs() < 1e-12, "{i}: {v} vs {want}");
        }
    }

    #[test]
    fn a_sine_wave_table_is_a_unit_sine() {
        let w = PeriodicWave::basic(44100.0, Shape::Sine);
        assert_eq!(w.size(), 4096);
        for (i, v) in w.tables[0].iter().enumerate().step_by(97) {
            let want = (2.0 * PI * i as f64 / 4096.0).sin() as f32;
            assert!((v - want).abs() < 1e-6);
        }
    }

    #[test]
    fn custom_waves_follow_the_spec_sum_and_normalization() {
        // real [0, 1], imag [0, 1]: cos + sin, peak √2.
        let w = PeriodicWave::new(44100.0, &[0.0, 1.0], &[0.0, 1.0], true);
        let peak = w.tables[0].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - 1.0).abs() < 1e-6);
        let raw = PeriodicWave::new(44100.0, &[0.0, 1.0], &[0.0, 1.0], false);
        assert!((raw.tables[0][0] - 1.0).abs() < 1e-6);
        assert!((raw.tables[0][1024] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn higher_ranges_drop_the_partials_above_nyquist() {
        let w = PeriodicWave::basic(44100.0, Shape::Square);
        // 36 ranges for 4096-point tables; the last keeps only the fundamental.
        assert_eq!(w.tables.len(), 36);
        let last = &w.tables[35];
        let scale = last[1024];
        for (i, v) in last.iter().enumerate().step_by(101) {
            let want = scale * (2.0 * PI * i as f64 / 4096.0).sin() as f32;
            assert!((v - want).abs() < 1e-5);
        }
    }

    #[test]
    fn an_oscillator_plays_its_frequency() {
        let sr = 44100.0;
        let mut osc = Oscillator::new(Arc::new(PeriodicWave::basic(sr as f32, Shape::Sine)));
        osc.schedule.start = Some(0.0);
        let freq = [441.0f32; N];
        let detune = [0.0f32; N];
        let mut out = [0.0f32; N];
        let q = Quantum {
            frame: 0,
            sample_rate: sr,
        };
        osc.process(q, &[&freq, &detune], &mut out);
        for (i, v) in out.iter().enumerate() {
            let want = (2.0 * PI * 441.0 * i as f64 / sr).sin() as f32;
            assert!((v - want).abs() < 1e-4, "{i}: {v} vs {want}");
        }
    }
}
