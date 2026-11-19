// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AnalyserNode (§1.11): the rendering thread keeps the most recent input frames, down-mixed to
//! mono, in a ring the control thread reads when the page asks for data; the windowed FFT and the
//! smoothing over time run on the control thread (§1.11.5 "FFT Windowing and smoothing over time").

use std::sync::{Arc, Mutex};

use crate::bus::{AudioBus, Interpretation};
use crate::fft::Fft;
use crate::RENDER_QUANTUM as N;

/// The largest `fftSize`.
pub const MAX_FFT: usize = 32768;

#[derive(Debug)]
struct Ring {
    data: Vec<f32>,
    /// Frames written so far.
    written: u64,
}

/// The frames the rendering thread wrote, shared with the control thread.
#[derive(Debug, Clone)]
pub struct AnalyserInput(Arc<Mutex<Ring>>);

impl Default for AnalyserInput {
    fn default() -> Self {
        AnalyserInput(Arc::new(Mutex::new(Ring {
            data: vec![0.0; MAX_FFT],
            written: 0,
        })))
    }
}

impl AnalyserInput {
    /// The rendering thread: one quantum of input.
    pub fn write(&self, input: &AudioBus, mono: &mut AudioBus) {
        mono.zero(1);
        mono.sum_from(input, Interpretation::Speakers);
        let Ok(mut ring) = self.0.lock() else { return };
        let at = (ring.written % MAX_FFT as u64) as usize;
        ring.data[at..at + N].copy_from_slice(mono.channel(0));
        ring.written += N as u64;
    }

    /// The last `out.len()` frames, oldest first, and the frame count they end at.
    fn read(&self, out: &mut [f32]) -> u64 {
        let Ok(ring) = self.0.lock() else { return 0 };
        let n = out.len();
        let end = ring.written;
        for (i, v) in out.iter_mut().enumerate() {
            let t = end as i64 - n as i64 + i as i64;
            *v = if t < 0 {
                0.0
            } else {
                ring.data[(t as u64 % MAX_FFT as u64) as usize]
            };
        }
        end
    }
}

/// An AnalyserNode's control-thread state.
#[derive(Debug)]
pub struct Analyser {
    pub input: AnalyserInput,
    pub fft_size: usize,
    pub min_decibels: f64,
    pub max_decibels: f64,
    pub smoothing: f64,
    fft: Fft,
    /// The previous block's smoothed magnitudes.
    smoothed: Vec<f32>,
    /// The frame count the smoothed magnitudes were computed at.
    computed_at: Option<u64>,
}

impl Analyser {
    pub fn new(input: AnalyserInput) -> Analyser {
        Analyser {
            input,
            fft_size: 2048,
            min_decibels: -100.0,
            max_decibels: -30.0,
            smoothing: 0.8,
            fft: Fft::new(2048),
            smoothed: vec![0.0; 1024],
            computed_at: None,
        }
    }

    pub fn set_fft_size(&mut self, size: usize) {
        if size != self.fft_size {
            self.fft_size = size;
            self.fft = Fft::new(size);
            self.smoothed = vec![0.0; size / 2];
            self.computed_at = None;
        }
    }

    pub fn frequency_bin_count(&self) -> usize {
        self.fft_size / 2
    }

    /// The current time-domain data: the last `fftSize` frames.
    pub fn time_domain(&self) -> Vec<f32> {
        let mut v = vec![0.0; self.fft_size];
        self.input.read(&mut v);
        v
    }

    /// The current frequency data in decibels, computed at most once per render quantum
    /// (§1.11.5).
    pub fn frequency_db(&mut self) -> Vec<f32> {
        let n = self.fft_size;
        let mut re = vec![0.0; n];
        let end = self.input.read(&mut re);
        if self.computed_at != Some(end) {
            self.computed_at = Some(end);
            // Blackman window.
            let (a0, a1, a2) = (0.42, 0.5, 0.08);
            for (i, v) in re.iter_mut().enumerate() {
                let x = i as f64 / n as f64;
                let w = a0 - a1 * (2.0 * std::f64::consts::PI * x).cos()
                    + a2 * (4.0 * std::f64::consts::PI * x).cos();
                *v *= w as f32;
            }
            let mut im = vec![0.0; n];
            self.fft.forward(&mut re, &mut im);
            let tau = self.smoothing as f32;
            for k in 0..n / 2 {
                let mag = re[k].hypot(im[k]) / n as f32;
                let prev = self.smoothed[k];
                let mut s = tau * prev + (1.0 - tau) * mag;
                if !s.is_finite() {
                    s = 0.0;
                }
                self.smoothed[k] = s;
            }
        }
        self.smoothed.iter().map(|m| 20.0 * m.log10()).collect()
    }

    /// `getByteFrequencyData()`: decibels scaled from `[minDecibels, maxDecibels]` to bytes.
    pub fn to_byte(&self, db: f32) -> u8 {
        let (min, max) = (self.min_decibels, self.max_decibels);
        let v = (255.0 / (max - min) * (db as f64 - min)).floor();
        if v.is_nan() {
            0
        } else {
            v.clamp(0.0, 255.0) as u8
        }
    }
}

/// `getByteTimeDomainData()`: a sample scaled from `[-1, 1]` to bytes.
pub fn time_to_byte(x: f32) -> u8 {
    (128.0 * (1.0 + x as f64)).floor().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_peaks_in_its_bin_and_smoothing_waits_for_the_next_quantum() {
        let input = AnalyserInput::default();
        let mut a = Analyser::new(input.clone());
        a.set_fft_size(256);
        a.smoothing = 0.5;
        let mut mono = AudioBus::new(1);
        for q in 0..2 {
            let mut bus = AudioBus::new(1);
            for (i, v) in bus.channel_mut(0).iter_mut().enumerate() {
                let t = (q * N + i) as f64;
                *v = (2.0 * std::f64::consts::PI * 16.0 * t / 256.0).sin() as f32;
            }
            input.write(&bus, &mut mono);
        }
        let db = a.frequency_db();
        let peak = (0..128).max_by(|x, y| db[*x].total_cmp(&db[*y])).unwrap();
        assert_eq!(peak, 16);
        // Blackman's coherent gain 0.42, half the amplitude per bin, smoothed once by 0.5.
        let expected = 20.0 * (0.42f32 * 0.5 * 0.5).log10();
        assert!((db[16] - expected).abs() < 0.05, "{} {expected}", db[16]);
        assert_eq!(a.frequency_db()[16], db[16]);
        assert_eq!(time_to_byte(0.0), 128);
        assert_eq!(a.to_byte(-30.0), 255);
    }
}
