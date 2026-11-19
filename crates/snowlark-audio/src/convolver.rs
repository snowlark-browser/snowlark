// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ConvolverNode (§1.16): linear convolution with an impulse response, by uniformly partitioned
//! overlap-save in blocks of one render quantum (no added latency), with the spec's normalization
//! and its routing of input, response and output channels.

use crate::bus::AudioBus;
use crate::fft::Fft;
use crate::RENDER_QUANTUM as N;

const FFT: usize = 2 * N;
const BINS: usize = N + 1;

/// §1.16.3 "normalize": the scale of a response.
pub fn normalization_scale(channels: &[Vec<f32>], sample_rate: f32) -> f32 {
    const GAIN_CALIBRATION: f32 = -58.0;
    const GAIN_CALIBRATION_SAMPLE_RATE: f32 = 44100.0;
    const MIN_POWER: f32 = 0.000125;
    let len = channels.first().map_or(0, Vec::len);
    let mut power: f32 = channels
        .iter()
        .map(|c| c.iter().map(|x| x * x).sum::<f32>())
        .sum();
    power = (power / (channels.len() * len) as f32).sqrt();
    if !power.is_finite() || power < MIN_POWER {
        power = MIN_POWER;
    }
    let mut scale = 1.0 / power;
    scale *= 10f32.powf(GAIN_CALIBRATION * 0.05);
    scale *= GAIN_CALIBRATION_SAMPLE_RATE / sample_rate;
    if channels.len() == 4 {
        scale *= 0.5;
    }
    scale
}

/// One response channel convolved with one input stream.
#[derive(Debug)]
struct Lane {
    /// The response's partitions in the frequency domain (in doubles: the result must stay
    /// within about -124 dB of the exact convolution).
    parts: Vec<(Vec<f64>, Vec<f64>)>,
    /// The input's spectra, newest at `head`.
    history: Vec<(Vec<f64>, Vec<f64>)>,
    head: usize,
    /// The previous input block.
    prev: [f32; N],
    /// The response's absolute sum, and per history entry the input's peak in its window: their
    /// product bounds the output.
    l1: f64,
    peaks: Vec<f64>,
}

impl Lane {
    fn new(fft: &Fft, response: &[f32], scale: f32) -> Lane {
        let parts: Vec<_> = response
            .chunks(N)
            .map(|chunk| {
                let mut re = vec![0.0f64; FFT];
                let mut im = vec![0.0f64; FFT];
                for (r, x) in re.iter_mut().zip(chunk) {
                    *r = *x as f64 * scale as f64;
                }
                fft.forward(&mut re, &mut im);
                re.truncate(BINS);
                im.truncate(BINS);
                (re, im)
            })
            .collect();
        let count = parts.len().max(1);
        Lane {
            parts,
            history: vec![(vec![0.0; BINS], vec![0.0; BINS]); count],
            l1: response
                .iter()
                .map(|h| (*h as f64 * scale as f64).abs())
                .sum(),
            peaks: vec![0.0; count],
            head: 0,
            prev: [0.0; N],
        }
    }

    fn process(&mut self, fft: &Fft, x: &[f32; N], y: &mut [f32; N], scratch: &mut Scratch) {
        let Scratch {
            re,
            im,
            acc_re,
            acc_im,
        } = scratch;
        for (r, v) in re.iter_mut().zip(self.prev.iter().chain(x.iter())) {
            *r = *v as f64;
        }
        im.fill(0.0);
        self.prev = *x;
        let peak = re.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        fft.forward(re, im);
        let count = self.history.len();
        self.head = (self.head + count - 1) % count;
        self.peaks[self.head] = peak;
        let slot = &mut self.history[self.head];
        slot.0.copy_from_slice(&re[..BINS]);
        slot.1.copy_from_slice(&im[..BINS]);
        acc_re.fill(0.0);
        acc_im.fill(0.0);
        for (p, h) in self.parts.iter().enumerate() {
            let s = &self.history[(self.head + p) % count];
            for k in 0..BINS {
                acc_re[k] += s.0[k] * h.0[k] - s.1[k] * h.1[k];
                acc_im[k] += s.0[k] * h.1[k] + s.1[k] * h.0[k];
            }
        }
        // The full spectrum of a real signal.
        re[..BINS].copy_from_slice(acc_re);
        im[..BINS].copy_from_slice(acc_im);
        for k in 1..N {
            re[FFT - k] = acc_re[k];
            im[FFT - k] = -acc_im[k];
        }
        fft.inverse(re, im);
        // What is 260 dB below the bound on the output is the transform's rounding noise, not
        // signal: a sample that is exactly zero stays zero.
        let floor = self.l1 * self.peaks.iter().fold(0.0f64, |m, v| m.max(*v)) * 1e-13;
        for (o, v) in y.iter_mut().zip(&re[N..]) {
            *o = if v.abs() < floor { 0.0 } else { *v as f32 };
        }
    }
}

#[derive(Debug)]
struct Scratch {
    re: Vec<f64>,
    im: Vec<f64>,
    acc_re: Vec<f64>,
    acc_im: Vec<f64>,
}

/// A ConvolverNode's response and state.
#[derive(Debug)]
pub struct Convolver {
    fft: Fft,
    /// One lane per response channel (at least two: a mono response is used for both sides of a
    /// stereo input), and a second set for the right input of a four-channel response.
    lanes: Vec<Lane>,
    response_channels: usize,
    scratch: Scratch,
    /// The output channel count of the last quantum with input.
    channels: usize,
}

impl Convolver {
    /// A convolver for `response` (1, 2 or 4 channels), scaled by `scale`.
    pub fn new(response: &[Vec<f32>], scale: f32) -> Convolver {
        let fft = Fft::new(FFT);
        let n = response.len();
        let lanes = (0..n.max(2))
            .map(|i| Lane::new(&fft, &response[i.min(n - 1)], scale))
            .collect();
        Convolver {
            fft,
            lanes,
            response_channels: n,
            scratch: Scratch {
                re: vec![0.0; FFT],
                im: vec![0.0; FFT],
                acc_re: vec![0.0; BINS],
                acc_im: vec![0.0; BINS],
            },
            channels: 1,
        }
    }

    /// One quantum. Without a response the output is silent.
    pub fn process(
        this: Option<&mut Convolver>,
        input: &AudioBus,
        silent: bool,
        out: &mut AudioBus,
    ) {
        let Some(c) = this else {
            out.zero(input.channels().min(2));
            return;
        };
        let in_channels = if silent {
            c.channels.min(2)
        } else {
            input.channels().min(2)
        };
        let out_channels = in_channels.max(c.response_channels).clamp(1, 2);
        if !silent {
            c.channels = in_channels;
        }
        out.zero(out_channels);
        let zero = [0.0f32; N];
        let ch = |i: usize| -> &[f32; N] {
            if silent || i >= input.channels() {
                &zero
            } else {
                input.channel(i)
            }
        };
        let (l, r) = (*ch(0), *ch(1.min(in_channels - 1)));
        let mut y = [[0.0f32; N]; 4];
        let fft = &c.fft;
        let s = &mut c.scratch;
        match (in_channels, c.response_channels) {
            (1, 1) => c.lanes[0].process(fft, &l, &mut y[0], s),
            (2, 1) | (2, 2) => {
                c.lanes[0].process(fft, &l, &mut y[0], s);
                c.lanes[1].process(fft, &r, &mut y[1], s);
            }
            (1, 2) => {
                c.lanes[0].process(fft, &l, &mut y[0], s);
                c.lanes[1].process(fft, &l, &mut y[1], s);
            }
            (_, 4) => {
                c.lanes[0].process(fft, &l, &mut y[0], s);
                c.lanes[1].process(fft, &l, &mut y[1], s);
                c.lanes[2].process(fft, &r, &mut y[2], s);
                c.lanes[3].process(fft, &r, &mut y[3], s);
                let [y0, y1, y2, y3] = &mut y;
                for (a, b) in y0.iter_mut().zip(y2.iter()) {
                    *a += b;
                }
                for (a, b) in y1.iter_mut().zip(y3.iter()) {
                    *a += b;
                }
            }
            _ => {}
        }
        for (i, o) in out.all_mut().iter_mut().enumerate() {
            o.copy_from_slice(&y[i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convolution_matches_the_direct_sum_across_partitions() {
        let response: Vec<f32> = (0..300)
            .map(|i| ((i * 7 % 13) as f32 - 6.0) / 10.0)
            .collect();
        let mut c = Some(Convolver::new(std::slice::from_ref(&response), 1.0));
        let signal: Vec<f32> = (0..3 * N)
            .map(|i| ((i * 5 % 11) as f32 - 5.0) / 7.0)
            .collect();
        let mut got = Vec::new();
        let mut out = AudioBus::new(1);
        for q in 0..3 {
            let mut input = AudioBus::new(1);
            input
                .channel_mut(0)
                .copy_from_slice(&signal[q * N..(q + 1) * N]);
            Convolver::process(c.as_mut(), &input, false, &mut out);
            got.extend_from_slice(out.channel(0));
        }
        for (n, g) in got.iter().enumerate() {
            let want: f32 = (0..=n)
                .filter(|k| n - k < response.len())
                .map(|k| signal[k] * response[n - k])
                .sum();
            assert!((g - want).abs() < 1e-4, "{n}: {g} {want}");
        }
    }

    #[test]
    fn where_the_convolution_is_zero_the_output_is_exactly_zero() {
        // A 50-frame pulse through a 200-frame box: a trapezoid that ends at frame 249.
        let mut c = Some(Convolver::new(&[vec![1.0; 200]], 1.0));
        let mut out = AudioBus::new(1);
        let mut got = Vec::new();
        for q in 0..3 {
            let mut input = AudioBus::new(1);
            if q == 0 {
                input.channel_mut(0)[..50].fill(1.0);
            }
            Convolver::process(c.as_mut(), &input, false, &mut out);
            got.extend_from_slice(out.channel(0));
        }
        assert!((got[49] - 50.0).abs() < 1e-9);
        assert!(got[249..].iter().all(|v| *v == 0.0));
    }

    #[test]
    fn a_unit_response_is_scaled_by_the_calibration() {
        let s = normalization_scale(&[vec![1.0]], 44100.0);
        assert!((s - 10f32.powf(-2.9)).abs() < 1e-7);
    }
}
