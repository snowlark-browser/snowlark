// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A radix-2 complex FFT with its twiddle factors and bit-reversal table computed once per size,
//! for the nodes that work in the frequency domain (AnalyserNode, ConvolverNode) and the frequency
//! responses.

use std::f64::consts::PI;

/// A sample type the transform runs in.
pub trait Real:
    Copy
    + std::ops::Add<Output = Self>
    + std::ops::Sub<Output = Self>
    + std::ops::Mul<Output = Self>
    + std::ops::AddAssign
    + std::ops::MulAssign
{
    fn of(v: f64) -> Self;
}

impl Real for f32 {
    fn of(v: f64) -> Self {
        v as f32
    }
}

impl Real for f64 {
    fn of(v: f64) -> Self {
        v
    }
}

/// A planned transform of one power-of-two size.
#[derive(Debug, Clone)]
pub struct Fft {
    n: usize,
    /// `e^{-2πik/n}` for `k < n/2`.
    cos: Vec<f64>,
    sin: Vec<f64>,
    rev: Vec<u32>,
}

impl Fft {
    /// A transform of `n` points (a power of two, at least 2).
    pub fn new(n: usize) -> Fft {
        assert!(n.is_power_of_two() && n >= 2);
        let bits = n.trailing_zeros();
        let rev = (0..n)
            .map(|i| (i.reverse_bits() >> (usize::BITS - bits)) as u32)
            .collect();
        let (cos, sin) = (0..n / 2)
            .map(|k| {
                let a = -2.0 * PI * k as f64 / n as f64;
                (a.cos(), a.sin())
            })
            .unzip();
        Fft { n, cos, sin, rev }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// In place, `X[k] = Σ x[n] e^{∓2πikn/N}` (forward: minus), without scaling.
    fn transform<T: Real>(&self, re: &mut [T], im: &mut [T], inverse: bool) {
        let n = self.n;
        for i in 0..n {
            let j = self.rev[i] as usize;
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let sign = if inverse { -1.0 } else { 1.0 };
        let mut len = 2;
        while len <= n {
            let stride = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let c = T::of(self.cos[k * stride]);
                    let s = T::of(sign * self.sin[k * stride]);
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

    /// The forward transform, in place.
    pub fn forward<T: Real>(&self, re: &mut [T], im: &mut [T]) {
        self.transform(re, im, false);
    }

    /// The inverse transform, in place, scaled by `1/n`.
    pub fn inverse<T: Real>(&self, re: &mut [T], im: &mut [T]) {
        self.transform(re, im, true);
        let s = T::of(1.0 / self.n as f64);
        for v in re.iter_mut().chain(im.iter_mut()) {
            *v *= s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_then_inverse_is_the_identity_and_a_cosine_is_two_bins() {
        let n = 64;
        let fft = Fft::new(n);
        let x: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 5.0 * i as f64 / n as f64).cos() as f32)
            .collect();
        let (mut re, mut im): (Vec<f32>, Vec<f32>) = (x.clone(), vec![0.0; n]);
        fft.forward(&mut re, &mut im);
        assert!((re[5] - 32.0).abs() < 1e-4 && (re[59] - 32.0).abs() < 1e-4);
        assert!(re[4].abs() < 1e-4 && im[5].abs() < 1e-4);
        fft.inverse(&mut re, &mut im);
        for (a, b) in re.iter().zip(&x) {
            assert!((a - b).abs() < 1e-5);
        }
    }
}
