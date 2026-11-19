// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! BiquadFilterNode (§1.13): the filter types' coefficients from the Audio EQ Cookbook formulas the
//! spec gives, with their limits at 0 Hz and at Nyquist, the frequency response, and the
//! second-order section run per channel.

use crate::bus::AudioBus;
use crate::RENDER_QUANTUM as N;

/// `BiquadFilterType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterType {
    Lowpass,
    Highpass,
    Bandpass,
    Lowshelf,
    Highshelf,
    Peaking,
    Notch,
    Allpass,
}

/// A filter's coefficients, normalized by `a0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coefs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coefs {
    const fn gain(g: f64) -> Coefs {
        Coefs {
            b0: g,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    fn normalized(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Coefs {
        let s = 1.0 / a0;
        Coefs {
            b0: b0 * s,
            b1: b1 * s,
            b2: b2 * s,
            a1: a1 * s,
            a2: a2 * s,
        }
    }

    /// §1.13.4: the coefficients for `frequency` (computed with detune, Hz), `q` and `gain` (dB)
    /// at `sample_rate`.
    pub fn new(kind: FilterType, frequency: f64, q: f64, gain: f64, sample_rate: f64) -> Coefs {
        let nyquist = sample_rate / 2.0;
        // The normalized frequency, 1 at Nyquist.
        let f = (frequency / nyquist).clamp(0.0, 1.0);
        let a = 10f64.powf(gain / 40.0);
        let w0 = std::f64::consts::PI * f;
        let k = w0.cos();
        match kind {
            FilterType::Lowpass | FilterType::Highpass => {
                let low = kind == FilterType::Lowpass;
                if f >= 1.0 {
                    return Coefs::gain(if low { 1.0 } else { 0.0 });
                }
                if f <= 0.0 {
                    return Coefs::gain(if low { 0.0 } else { 1.0 });
                }
                // Q in decibels for these two.
                let alpha = w0.sin() / (2.0 * 10f64.powf(q / 20.0));
                let beta = if low {
                    (1.0 - k) / 2.0
                } else {
                    (1.0 + k) / 2.0
                };
                let b1 = if low { 2.0 * beta } else { -2.0 * beta };
                Coefs::normalized(beta, b1, beta, 1.0 + alpha, -2.0 * k, 1.0 - alpha)
            }
            FilterType::Lowshelf | FilterType::Highshelf => {
                let low = kind == FilterType::Lowshelf;
                if f >= 1.0 {
                    return Coefs::gain(if low { a * a } else { 1.0 });
                }
                if f <= 0.0 {
                    return Coefs::gain(if low { 1.0 } else { a * a });
                }
                // S = 1.
                let alpha = 0.5 * w0.sin() * 2f64.sqrt();
                let k2 = 2.0 * a.sqrt() * alpha;
                let (ap1, am1) = (a + 1.0, a - 1.0);
                if low {
                    Coefs::normalized(
                        a * (ap1 - am1 * k + k2),
                        2.0 * a * (am1 - ap1 * k),
                        a * (ap1 - am1 * k - k2),
                        ap1 + am1 * k + k2,
                        -2.0 * (am1 + ap1 * k),
                        ap1 + am1 * k - k2,
                    )
                } else {
                    Coefs::normalized(
                        a * (ap1 + am1 * k + k2),
                        -2.0 * a * (am1 + ap1 * k),
                        a * (ap1 + am1 * k - k2),
                        ap1 - am1 * k + k2,
                        2.0 * (am1 - ap1 * k),
                        ap1 - am1 * k - k2,
                    )
                }
            }
            FilterType::Bandpass
            | FilterType::Peaking
            | FilterType::Notch
            | FilterType::Allpass => {
                if f <= 0.0 || f >= 1.0 {
                    // The limits: a bandpass passes nothing there, the others everything.
                    return Coefs::gain(if kind == FilterType::Bandpass {
                        0.0
                    } else {
                        1.0
                    });
                }
                if q <= 0.0 {
                    return Coefs::gain(match kind {
                        FilterType::Bandpass => 1.0,
                        FilterType::Peaking => a * a,
                        FilterType::Notch => 0.0,
                        _ => -1.0,
                    });
                }
                let alpha = w0.sin() / (2.0 * q);
                match kind {
                    FilterType::Bandpass => {
                        Coefs::normalized(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * k, 1.0 - alpha)
                    }
                    FilterType::Peaking => Coefs::normalized(
                        1.0 + alpha * a,
                        -2.0 * k,
                        1.0 - alpha * a,
                        1.0 + alpha / a,
                        -2.0 * k,
                        1.0 - alpha / a,
                    ),
                    FilterType::Notch => {
                        Coefs::normalized(1.0, -2.0 * k, 1.0, 1.0 + alpha, -2.0 * k, 1.0 - alpha)
                    }
                    _ => Coefs::normalized(
                        1.0 - alpha,
                        -2.0 * k,
                        1.0 + alpha,
                        1.0 + alpha,
                        -2.0 * k,
                        1.0 - alpha,
                    ),
                }
            }
        }
    }

    /// The magnitude and phase of the response at `frequency` Hz (§1.13.3
    /// `getFrequencyResponse()`): NaN for both outside `[0, nyquist]`.
    pub fn response(&self, frequency: f64, sample_rate: f64) -> (f32, f32) {
        let nyquist = sample_rate / 2.0;
        if !(0.0..=nyquist).contains(&frequency) {
            return (f32::NAN, f32::NAN);
        }
        let w = std::f64::consts::PI * frequency / nyquist;
        // z^-1 = e^{-iw}.
        let (s1, c1) = (-w).sin_cos();
        let (s2, c2) = (-2.0 * w).sin_cos();
        let num = (
            self.b0 + self.b1 * c1 + self.b2 * c2,
            self.b1 * s1 + self.b2 * s2,
        );
        let den = (
            1.0 + self.a1 * c1 + self.a2 * c2,
            self.a1 * s1 + self.a2 * s2,
        );
        let (mag, phase) = div_polar(num, den);
        (mag as f32, phase as f32)
    }
}

/// `num / den` as magnitude and phase.
pub(crate) fn div_polar(num: (f64, f64), den: (f64, f64)) -> (f64, f64) {
    let d = den.0 * den.0 + den.1 * den.1;
    let re = (num.0 * den.0 + num.1 * den.1) / d;
    let im = (num.1 * den.0 - num.0 * den.1) / d;
    (re.hypot(im), im.atan2(re))
}

/// A BiquadFilterNode's state: per channel the last two inputs and outputs.
#[derive(Debug)]
pub struct Biquad {
    pub kind: FilterType,
    state: Vec<[f64; 4]>,
}

impl Biquad {
    pub fn new(kind: FilterType) -> Biquad {
        Biquad {
            kind,
            state: Vec::new(),
        }
    }

    /// One quantum. `params`: frequency, detune, Q, gain. The coefficients are computed once when
    /// every parameter holds one value in the quantum, else per frame.
    pub fn process(
        &mut self,
        sample_rate: f64,
        params: &[&[f32; N]],
        input: &AudioBus,
        silent: bool,
        out: &mut AudioBus,
    ) {
        let channels = if silent && !self.state.is_empty() {
            self.state.len()
        } else {
            input.channels()
        };
        self.state.resize(channels, [0.0; 4]);
        out.zero(channels);
        let constant = params[..4]
            .iter()
            .all(|p| p.iter().all(|v| v.to_bits() == p[0].to_bits()));
        let coefs_at = |i: usize| {
            let f = params[0][i] as f64 * 2f64.powf(params[1][i] as f64 / 1200.0);
            Coefs::new(
                self.kind,
                f,
                params[2][i] as f64,
                params[3][i] as f64,
                sample_rate,
            )
        };
        let fixed = constant.then(|| coefs_at(0));
        let per_frame: Vec<Coefs> = if fixed.is_none() {
            (0..N).map(coefs_at).collect()
        } else {
            Vec::new()
        };
        for (c, state) in self.state.iter_mut().enumerate() {
            let x = if c < input.channels() && !silent {
                *input.channel(c)
            } else {
                [0.0; N]
            };
            let o = out.channel_mut(c);
            let [mut x1, mut x2, mut y1, mut y2] = *state;
            for f in 0..N {
                let k = fixed.unwrap_or_else(|| per_frame[f]);
                let x0 = x[f] as f64;
                let y0 = k.b0 * x0 + k.b1 * x1 + k.b2 * x2 - k.a1 * y1 - k.a2 * y2;
                x2 = x1;
                x1 = x0;
                y2 = y1;
                y1 = y0;
                o[f] = y0 as f32;
            }
            // Flush denormals so a decaying tail ends.
            for v in [&mut x1, &mut x2, &mut y1, &mut y2] {
                if v.abs() < f64::from(f32::MIN_POSITIVE) {
                    *v = 0.0;
                }
            }
            *state = [x1, x2, y1, y2];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lowpass_passes_dc_and_halves_power_at_its_cutoff() {
        // Q = -3.0103 dB gives a Butterworth response: -3 dB at the cutoff.
        let k = Coefs::new(FilterType::Lowpass, 1000.0, -3.0103, 0.0, 48000.0);
        let (dc, _) = k.response(0.0, 48000.0);
        assert!((dc - 1.0).abs() < 1e-6, "{dc}");
        let (m, _) = k.response(1000.0, 48000.0);
        assert!((m - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3, "{m}");
        assert!(k.response(30000.0, 48000.0).0.is_nan());
    }

    #[test]
    fn the_limits_at_zero_and_nyquist_follow_the_spec() {
        let sr = 48000.0;
        assert_eq!(
            Coefs::new(FilterType::Lowpass, 24000.0, 1.0, 0.0, sr),
            Coefs::gain(1.0)
        );
        assert_eq!(
            Coefs::new(FilterType::Highpass, 0.0, 1.0, 0.0, sr),
            Coefs::gain(1.0)
        );
        assert_eq!(
            Coefs::new(FilterType::Bandpass, 0.0, 1.0, 0.0, sr),
            Coefs::gain(0.0)
        );
        assert_eq!(
            Coefs::new(FilterType::Allpass, 100.0, 0.0, 0.0, sr),
            Coefs::gain(-1.0)
        );
        let a2 = 10f64.powf(6.0 / 20.0);
        let low = Coefs::new(FilterType::Lowshelf, 24000.0, 1.0, 6.0, sr);
        assert!((low.b0 - a2).abs() < 1e-12);
    }

    #[test]
    fn a_peaking_filter_has_its_gain_at_its_frequency() {
        let k = Coefs::new(FilterType::Peaking, 2000.0, 1.0, 12.0, 44100.0);
        let (m, p) = k.response(2000.0, 44100.0);
        assert!((20.0 * m.log10() - 12.0).abs() < 1e-3, "{m}");
        assert!(p.abs() < 1e-4);
    }
}
