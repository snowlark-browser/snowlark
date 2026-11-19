// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A render quantum of audio in some number of channels, and the spec's channel up- and
//! down-mixing (§1.5.3 "Channel Up-Mixing and Down-Mixing").

use crate::RENDER_QUANTUM as N;

/// `ChannelCountMode` (§1.5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountMode {
    Max,
    ClampedMax,
    Explicit,
}

/// `ChannelInterpretation` (§1.5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interpretation {
    Speakers,
    Discrete,
}

/// A node input's channel configuration: `channelCount`, `channelCountMode`,
/// `channelInterpretation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelConfig {
    pub count: u32,
    pub mode: CountMode,
    pub interpretation: Interpretation,
}

impl ChannelConfig {
    pub const fn new(count: u32, mode: CountMode, interpretation: Interpretation) -> Self {
        ChannelConfig {
            count,
            mode,
            interpretation,
        }
    }

    /// §1.5.3 computedNumberOfChannels, from the largest channel count of the input's
    /// connections (`None`: no connection).
    pub fn computed_channels(&self, max_of_connections: Option<usize>) -> usize {
        let max = max_of_connections.unwrap_or(1).max(1);
        match self.mode {
            CountMode::Max => max,
            CountMode::ClampedMax => max.min(self.count as usize),
            CountMode::Explicit => self.count as usize,
        }
    }
}

/// One channel of a render quantum.
pub type Channel = [f32; N];

/// A render quantum in `channels()` channels. Its storage only grows, so a bus reused across
/// quanta allocates nothing once it has seen its widest layout.
#[derive(Clone, Debug)]
pub struct AudioBus {
    data: Vec<Channel>,
    count: usize,
    /// Every sample is zero (set by [`AudioBus::zero`], cleared by any write access).
    silent: bool,
}

impl Default for AudioBus {
    fn default() -> Self {
        AudioBus::new(1)
    }
}

impl AudioBus {
    /// A silent bus of `channels` channels.
    pub fn new(channels: usize) -> Self {
        AudioBus {
            data: vec![[0.0; N]; channels],
            count: channels,
            silent: true,
        }
    }

    pub fn channels(&self) -> usize {
        self.count
    }

    /// Whether every sample is known to be zero.
    pub fn is_silent(&self) -> bool {
        self.silent
    }

    /// Whether every sample of the channels in use is zero (scanning them unless known silent).
    pub fn all_zero(&self) -> bool {
        self.silent || self.all().iter().all(|c| c.iter().all(|v| *v == 0.0))
    }

    /// Makes the bus `channels` channels of silence.
    pub fn zero(&mut self, channels: usize) {
        if self.data.len() < channels {
            self.data.resize(channels, [0.0; N]);
        }
        self.count = channels;
        for c in &mut self.data[..channels] {
            c.fill(0.0);
        }
        self.silent = true;
    }

    pub fn channel(&self, i: usize) -> &Channel {
        &self.data[i]
    }

    pub fn channel_mut(&mut self, i: usize) -> &mut Channel {
        self.silent = false;
        &mut self.data[i]
    }

    /// The channels in use.
    pub fn all(&self) -> &[Channel] {
        &self.data[..self.count]
    }

    pub fn all_mut(&mut self) -> &mut [Channel] {
        self.silent = false;
        &mut self.data[..self.count]
    }

    /// Copies `src` (its channel count too).
    pub fn copy_from(&mut self, src: &AudioBus) {
        self.zero(src.count);
        if !src.silent {
            self.data[..src.count].copy_from_slice(&src.data[..src.count]);
            self.silent = false;
        }
    }

    /// Adds `src` into this bus, up- or down-mixing it to this bus's channel count by
    /// `interpretation` (§1.5.3).
    pub fn sum_from(&mut self, src: &AudioBus, interpretation: Interpretation) {
        if src.silent {
            return;
        }
        let (from, to) = (src.count, self.count);
        let speakers = interpretation == Interpretation::Speakers
            && matches!(from, 1 | 2 | 4 | 6)
            && matches!(to, 1 | 2 | 4 | 6);
        self.silent = false;
        if from == to || !speakers {
            // Equal layouts, and "discrete": fill channels in order, drop or leave the rest.
            for i in 0..from.min(to) {
                add(&mut self.data[i], &src.data[i], 1.0);
            }
            return;
        }
        let s = &src.data;
        let d = &mut self.data;
        let h = std::f32::consts::FRAC_1_SQRT_2;
        match (from, to) {
            // Up-mixing.
            (1, 2) => {
                add(&mut d[0], &s[0], 1.0);
                add(&mut d[1], &s[0], 1.0);
            }
            (1, 4) => {
                add(&mut d[0], &s[0], 1.0);
                add(&mut d[1], &s[0], 1.0);
            }
            (1, 6) => add(&mut d[2], &s[0], 1.0),
            (2, 4) | (2, 6) => {
                add(&mut d[0], &s[0], 1.0);
                add(&mut d[1], &s[1], 1.0);
            }
            (4, 6) => {
                add(&mut d[0], &s[0], 1.0);
                add(&mut d[1], &s[1], 1.0);
                add(&mut d[4], &s[2], 1.0);
                add(&mut d[5], &s[3], 1.0);
            }
            // Down-mixing.
            (2, 1) => {
                add(&mut d[0], &s[0], 0.5);
                add(&mut d[0], &s[1], 0.5);
            }
            (4, 1) => {
                for c in &s[..4] {
                    add(&mut d[0], c, 0.25);
                }
            }
            (6, 1) => {
                // M = 0.7071 * (L + R) + C + 0.5 * (SL + SR)
                for i in 0..N {
                    d[0][i] += h * (s[0][i] + s[1][i]) + s[2][i] + 0.5 * (s[4][i] + s[5][i]);
                }
            }
            (4, 2) => {
                // L = 0.5 * (L + SL), R = 0.5 * (R + SR)
                for i in 0..N {
                    d[0][i] += 0.5 * (s[0][i] + s[2][i]);
                    d[1][i] += 0.5 * (s[1][i] + s[3][i]);
                }
            }
            (6, 2) => {
                // L = L + 0.7071 * (C + SL), R = R + 0.7071 * (C + SR)
                for i in 0..N {
                    d[0][i] += s[0][i] + h * (s[2][i] + s[4][i]);
                    d[1][i] += s[1][i] + h * (s[2][i] + s[5][i]);
                }
            }
            (6, 4) => {
                // L = L + 0.7071 * C, R = R + 0.7071 * C, SL, SR
                for i in 0..N {
                    d[0][i] += s[0][i] + h * s[2][i];
                    d[1][i] += s[1][i] + h * s[2][i];
                    d[2][i] += s[4][i];
                    d[3][i] += s[5][i];
                }
            }
            _ => unreachable!("speaker layouts are 1, 2, 4 and 6 channels"),
        }
    }

    /// Multiplies every channel by `gain`.
    pub fn scale(&mut self, gain: f32) {
        if self.silent {
            return;
        }
        for c in &mut self.data[..self.count] {
            for v in c.iter_mut() {
                *v *= gain;
            }
        }
    }
}

fn add(dst: &mut Channel, src: &Channel, gain: f32) {
    if gain == 1.0 {
        for (d, s) in dst.iter_mut().zip(src) {
            *d += *s;
        }
    } else {
        for (d, s) in dst.iter_mut().zip(src) {
            *d += gain * *s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus(values: &[f32]) -> AudioBus {
        let mut b = AudioBus::new(values.len());
        for (i, v) in values.iter().enumerate() {
            b.channel_mut(i).fill(*v);
        }
        b
    }

    fn mix(from: &[f32], to: usize, interp: Interpretation) -> Vec<f32> {
        let mut d = AudioBus::new(to);
        d.sum_from(&bus(from), interp);
        d.all().iter().map(|c| c[0]).collect()
    }

    #[test]
    fn speaker_up_mixing_follows_the_spec_table() {
        let s = Interpretation::Speakers;
        assert_eq!(mix(&[1.0], 2, s), [1.0, 1.0]);
        assert_eq!(mix(&[1.0], 4, s), [1.0, 1.0, 0.0, 0.0]);
        assert_eq!(mix(&[1.0], 6, s), [0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
        assert_eq!(mix(&[1.0, 2.0], 4, s), [1.0, 2.0, 0.0, 0.0]);
        assert_eq!(mix(&[1.0, 2.0], 6, s), [1.0, 2.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(
            mix(&[1.0, 2.0, 3.0, 4.0], 6, s),
            [1.0, 2.0, 0.0, 0.0, 3.0, 4.0]
        );
    }

    #[test]
    fn speaker_down_mixing_follows_the_spec_table() {
        let s = Interpretation::Speakers;
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert_eq!(mix(&[1.0, 3.0], 1, s), [2.0]);
        assert_eq!(mix(&[1.0, 2.0, 3.0, 6.0], 1, s), [3.0]);
        let m = mix(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 1, s)[0];
        assert!((m - (h * 3.0 + 3.0 + 0.5 * 11.0)).abs() < 1e-6);
        assert_eq!(mix(&[1.0, 2.0, 3.0, 4.0], 2, s), [2.0, 3.0]);
        let st = mix(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 2, s);
        assert!((st[0] - (1.0 + h * 8.0)).abs() < 1e-6);
        assert!((st[1] - (2.0 + h * 9.0)).abs() < 1e-6);
        let q = mix(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 4, s);
        assert!((q[0] - (1.0 + h * 3.0)).abs() < 1e-6);
        assert!((q[1] - (2.0 + h * 3.0)).abs() < 1e-6);
        assert_eq!(&q[2..], [5.0, 6.0]);
    }

    #[test]
    fn discrete_and_odd_layouts_fill_in_order() {
        let d = Interpretation::Discrete;
        assert_eq!(mix(&[1.0], 2, d), [1.0, 0.0]);
        assert_eq!(mix(&[1.0, 2.0, 3.0], 2, d), [1.0, 2.0]);
        // 3 channels are not a speaker layout: discrete even under "speakers".
        assert_eq!(
            mix(&[1.0, 2.0, 3.0], 4, Interpretation::Speakers),
            [1.0, 2.0, 3.0, 0.0]
        );
    }

    #[test]
    fn computed_channels_by_mode() {
        let c = |mode| ChannelConfig::new(2, mode, Interpretation::Speakers);
        assert_eq!(c(CountMode::Max).computed_channels(Some(6)), 6);
        assert_eq!(c(CountMode::ClampedMax).computed_channels(Some(6)), 2);
        assert_eq!(c(CountMode::ClampedMax).computed_channels(Some(1)), 1);
        assert_eq!(c(CountMode::Explicit).computed_channels(Some(1)), 2);
        assert_eq!(c(CountMode::Max).computed_channels(None), 1);
    }
}
