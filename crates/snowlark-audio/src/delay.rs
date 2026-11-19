// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DelayNode (§1.17): a delay line per channel read with linear interpolation. In a cycle the node
//! is split into a writer and a reader (§2.4 "Rendering an Audio Graph"): the reader runs where the
//! node is ordered and reads at least one render quantum back, the writer after every other node.

use crate::bus::AudioBus;
use crate::RENDER_QUANTUM as N;

/// A DelayNode's delay lines.
#[derive(Debug)]
pub struct Delay {
    /// Per channel, the last `len` input frames; frame `t` is at `t % len`.
    lines: Vec<Vec<f32>>,
    len: usize,
    /// Input frames written so far.
    written: u64,
    /// The frame written last with more channels than the input now has: the extra lines are
    /// dropped once it has played out.
    wide_until: u64,
    /// Ordered as reader and writer (the node is in a cycle).
    pub split: bool,
}

impl Delay {
    /// A delay of up to `max_delay` seconds.
    pub fn new(max_delay: f64, sample_rate: f64) -> Delay {
        let len = (max_delay * sample_rate).ceil() as usize + 2 * N + 2;
        Delay {
            lines: Vec::new(),
            len,
            written: 0,
            wide_until: 0,
            split: false,
        }
    }

    /// Appends one quantum of input.
    pub fn write(&mut self, input: &AudioBus, silent: bool) {
        let wanted = if silent { 0 } else { input.channels() };
        if wanted > self.lines.len() {
            self.lines.resize_with(wanted, || vec![0.0; self.len]);
        }
        if wanted >= self.lines.len() {
            self.wide_until = self.written;
        } else if self.written > self.wide_until + self.len as u64 {
            self.lines.truncate(wanted.max(1));
        }
        let at = (self.written % self.len as u64) as usize;
        for (c, line) in self.lines.iter_mut().enumerate() {
            let src = if c < wanted {
                input.channel(c)
            } else {
                &[0.0; N]
            };
            let first = (self.len - at).min(N);
            line[at..at + first].copy_from_slice(&src[..first]);
            line[..N - first].copy_from_slice(&src[first..]);
        }
        self.written += N as u64;
    }

    /// Reads one quantum delayed by `delay` (seconds per frame) into `out`. `base` is the frame
    /// count the quantum starts at, which is `written` before this quantum's write.
    pub fn read(&self, base: u64, delay: &[f32; N], sample_rate: f64, out: &mut AudioBus) {
        out.zero(self.lines.len().max(1));
        if self.lines.is_empty() {
            return;
        }
        let min = if self.split { N as f64 } else { 0.0 };
        let max = (self.len - N - 2) as f64;
        let len = self.len as i64;
        for (f, delay) in delay.iter().enumerate() {
            let d = (*delay as f64 * sample_rate).clamp(min, max);
            let pos = (base + f as u64) as f64 - d;
            if pos < 0.0 {
                continue;
            }
            let k = pos.floor();
            let t = (pos - k) as f32;
            let i0 = (k as i64).rem_euclid(len) as usize;
            let i1 = (i0 + 1) % self.len;
            for (c, line) in self.lines.iter().enumerate() {
                let a = line[i0];
                let b = if t > 0.0 { line[i1] } else { a };
                out.channel_mut(c)[f] = a + (b - a) * t;
            }
        }
    }

    /// The frames written so far.
    pub fn written(&self) -> u64 {
        self.written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(start: f32) -> AudioBus {
        let mut b = AudioBus::new(1);
        for (i, v) in b.channel_mut(0).iter_mut().enumerate() {
            *v = start + i as f32;
        }
        b
    }

    #[test]
    fn a_whole_frame_delay_shifts_and_a_half_frame_interpolates() {
        let sr = 1024.0;
        let mut d = Delay::new(1.0, sr);
        let mut out = AudioBus::new(1);
        d.write(&ramp(1.0), false);
        d.read(0, &[3.0 / 1024.0; N], sr, &mut out);
        assert_eq!(&out.channel(0)[..5], &[0.0, 0.0, 0.0, 1.0, 2.0]);
        d.read(0, &[2.5 / 1024.0; N], sr, &mut out);
        assert_eq!(&out.channel(0)[3..5], &[1.5, 2.5]);
        // A whole quantum back, at a rate where it is exact.
        let mut d = Delay::new(1.0, 1024.0);
        d.write(&ramp(1.0), false);
        d.write(&ramp(129.0), false);
        d.read(128, &[0.125; N], 1024.0, &mut out);
        assert_eq!(out.channel(0)[0], 1.0);
    }

    #[test]
    fn a_split_delay_reads_at_least_a_quantum_back() {
        let sr = 1000.0;
        let mut d = Delay::new(1.0, sr);
        d.split = true;
        let mut out = AudioBus::new(1);
        d.write(&ramp(1.0), false);
        d.read(128, &[0.0; N], sr, &mut out);
        assert_eq!(out.channel(0)[0], 1.0);
        assert_eq!(out.channel(0)[127], 128.0);
    }
}
