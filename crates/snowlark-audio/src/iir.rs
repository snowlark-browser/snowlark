// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IIRFilterNode (§1.24): a general filter given by its feedforward and feedback coefficients,
//! `y(n) = (Σ b(k) x(n-k) - Σ_{k≥1} a(k) y(n-k)) / a(0)`, and its frequency response.

use crate::biquad::div_polar;
use crate::bus::AudioBus;
use crate::RENDER_QUANTUM as N;

/// The most coefficients of either kind (§1.24.2).
pub const MAX_ORDER: usize = 20;

/// The coefficients, normalized so `a(0)` is 1.
#[derive(Clone, Debug, PartialEq)]
pub struct IirCoefs {
    pub feedforward: Vec<f64>,
    pub feedback: Vec<f64>,
}

impl IirCoefs {
    /// From the constructor's arrays (already checked: 1 to 20 each, `feedback[0]` non-zero).
    pub fn new(feedforward: &[f64], feedback: &[f64]) -> IirCoefs {
        let a0 = feedback[0];
        IirCoefs {
            feedforward: feedforward.iter().map(|b| b / a0).collect(),
            feedback: feedback.iter().map(|a| a / a0).collect(),
        }
    }

    /// The magnitude and phase at `frequency` Hz (§1.24.1 `getFrequencyResponse()`): NaN outside
    /// `[0, nyquist]`.
    pub fn response(&self, frequency: f64, sample_rate: f64) -> (f32, f32) {
        let nyquist = sample_rate / 2.0;
        if !(0.0..=nyquist).contains(&frequency) {
            return (f32::NAN, f32::NAN);
        }
        let w = -std::f64::consts::PI * frequency / nyquist;
        let poly = |c: &[f64]| {
            c.iter().enumerate().fold((0.0, 0.0), |(re, im), (k, v)| {
                let (s, co) = (w * k as f64).sin_cos();
                (re + v * co, im + v * s)
            })
        };
        let (mag, phase) = div_polar(poly(&self.feedforward), poly(&self.feedback));
        (mag as f32, phase as f32)
    }
}

/// An IIRFilterNode's state: per channel, the last inputs and outputs.
#[derive(Debug)]
pub struct Iir {
    coefs: IirCoefs,
    /// Per channel, `MAX_ORDER` past inputs then `MAX_ORDER` past outputs, newest first.
    state: Vec<[f64; 2 * MAX_ORDER]>,
}

impl Iir {
    pub fn new(coefs: IirCoefs) -> Iir {
        Iir {
            coefs,
            state: Vec::new(),
        }
    }

    pub fn process(&mut self, input: &AudioBus, silent: bool, out: &mut AudioBus) {
        let channels = if silent && !self.state.is_empty() {
            self.state.len()
        } else {
            input.channels()
        };
        self.state.resize(channels, [0.0; 2 * MAX_ORDER]);
        out.zero(channels);
        let (b, a) = (&self.coefs.feedforward, &self.coefs.feedback);
        for (c, st) in self.state.iter_mut().enumerate() {
            let x = if c < input.channels() && !silent {
                *input.channel(c)
            } else {
                [0.0; N]
            };
            let o = out.channel_mut(c);
            for f in 0..N {
                let x0 = x[f] as f64;
                let mut y = b[0] * x0;
                for k in 1..b.len() {
                    y += b[k] * st[k - 1];
                }
                for k in 1..a.len() {
                    y -= a[k] * st[MAX_ORDER + k - 1];
                }
                if y.abs() < f64::from(f32::MIN_POSITIVE) {
                    y = 0.0;
                }
                st.copy_within(0..MAX_ORDER - 1, 1);
                st[0] = x0;
                st.copy_within(MAX_ORDER..2 * MAX_ORDER - 1, MAX_ORDER + 1);
                st[MAX_ORDER] = y;
                o[f] = y as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_pole_filter_matches_its_difference_equation_and_response() {
        // y(n) = 0.5 x(n) + 0.5 y(n-1), written with a(0) = 2.
        let k = IirCoefs::new(&[1.0], &[2.0, -1.0]);
        let mut iir = Iir::new(k.clone());
        let mut input = AudioBus::new(1);
        input.channel_mut(0)[0] = 1.0;
        let mut out = AudioBus::new(1);
        iir.process(&input, false, &mut out);
        assert_eq!(&out.channel(0)[..3], &[0.5, 0.25, 0.125]);
        let (dc, phase) = k.response(0.0, 48000.0);
        assert!((dc - 1.0).abs() < 1e-6 && phase.abs() < 1e-6);
        let (ny, _) = k.response(24000.0, 48000.0);
        assert!((ny - 1.0 / 3.0).abs() < 1e-6);
    }
}
