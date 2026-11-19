// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sample-rate conversion by windowed-sinc interpolation: what `decodeAudioData()` uses to bring
//! decoded audio to the context's rate (§1.1.3 "decodeAudioData", step "resampled").

use std::f64::consts::PI;

/// Zero crossings of the sinc on each side.
const HALF_WIDTH: f64 = 32.0;

fn kernel(u: f64, cutoff: f64, half: f64) -> f64 {
    if u.abs() >= half {
        return 0.0;
    }
    let s = PI * cutoff * u;
    let sinc = if s == 0.0 { 1.0 } else { s.sin() / s };
    // Blackman window over [-half, half].
    let x = (u + half) / (2.0 * half);
    let w = 0.42 - 0.5 * (2.0 * PI * x).cos() + 0.08 * (4.0 * PI * x).cos();
    cutoff * sinc * w
}

/// The number of frames `len` frames at `from` Hz make at `to` Hz.
pub fn resampled_len(len: usize, from: f64, to: f64) -> usize {
    (len as f64 * to / from) as usize
}

/// Kernel samples per unit of `u`.
const TABLE_RES: f64 = 512.0;

/// `input` at `from` Hz resampled to `to` Hz.
pub fn resample(input: &[f32], from: f64, to: f64) -> Vec<f32> {
    if from == to {
        return input.to_vec();
    }
    let out_len = resampled_len(input.len(), from, to);
    let step = from / to;
    // Below the lower of the two Nyquist frequencies.
    let cutoff = (to / from).min(1.0);
    let half = HALF_WIDTH / cutoff;
    // The kernel is even: tabulate it over [0, half] and interpolate.
    let table: Vec<f64> = (0..=(half * TABLE_RES) as usize + 1)
        .map(|i| kernel(i as f64 / TABLE_RES, cutoff, half))
        .collect();
    let at = |u: f64| {
        let x = u.abs() * TABLE_RES;
        let i = x as usize;
        if i + 1 >= table.len() {
            return 0.0;
        }
        let f = x - i as f64;
        table[i] + (table[i + 1] - table[i]) * f
    };
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let t = i as f64 * step;
        let first = (t - half).ceil().max(0.0) as usize;
        let last = ((t + half).floor() as usize).min(input.len().saturating_sub(1));
        let mut sum = 0.0;
        if first <= last {
            for (j, x) in input[first..=last].iter().enumerate() {
                sum += *x as f64 * at(t - (first + j) as f64);
            }
        }
        out.push(sum as f32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_keeps_its_frequency_and_level_across_rates() {
        let tone = |rate: f64, n: usize| -> Vec<f32> {
            (0..n)
                .map(|i| (2.0 * PI * 440.0 * i as f64 / rate).sin() as f32)
                .collect()
        };
        let input = tone(44100.0, 44100);
        let out = resample(&input, 44100.0, 48000.0);
        assert_eq!(out.len(), 48000);
        let want = tone(48000.0, 48000);
        let err = (1000..47000)
            .map(|i| (out[i] - want[i]).abs())
            .fold(0.0, f32::max);
        assert!(err < 1e-3, "{err}");
    }
}
