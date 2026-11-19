// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! StereoPannerNode (§1.31) and PannerNode (§1.27) with the equal-power panning model, its
//! distance models and sound cone (§6 "Spatialization/Panning").

use crate::bus::AudioBus;
use crate::RENDER_QUANTUM as N;
use std::f64::consts::FRAC_PI_2;

/// Left and right gains of the equal-power law at `x` in `[0, 1]`.
fn equal_power(x: f64) -> (f32, f32) {
    let (s, c) = (x * FRAC_PI_2).sin_cos();
    (c as f32, s as f32)
}

/// Pans one frame: `x` the position for a mono input; for a stereo one `toward_left` says which
/// side the other channel moves to.
fn pan_frame(input: &AudioBus, f: usize, x: f64, toward_left: bool, l: &mut f32, r: &mut f32) {
    let (gl, gr) = equal_power(x);
    if input.channels() == 1 {
        let s = input.channel(0)[f];
        *l = s * gl;
        *r = s * gr;
    } else {
        let (il, ir) = (input.channel(0)[f], input.channel(1)[f]);
        if toward_left {
            *l = il + ir * gl;
            *r = ir * gr;
        } else {
            *l = il * gl;
            *r = ir + il * gr;
        }
    }
}

/// StereoPannerNode: one quantum, `pan` per frame. The output is stereo.
pub fn stereo_pan(input: &AudioBus, pan: &[f32; N], out: &mut AudioBus) {
    out.zero(2);
    let mono = input.channels() == 1;
    let mut l = [0.0f32; N];
    let mut r = [0.0f32; N];
    for f in 0..N {
        let p = pan[f].clamp(-1.0, 1.0) as f64;
        let x = if mono {
            (p + 1.0) / 2.0
        } else if p <= 0.0 {
            p + 1.0
        } else {
            p
        };
        pan_frame(input, f, x, p <= 0.0, &mut l[f], &mut r[f]);
    }
    out.channel_mut(0).copy_from_slice(&l);
    out.channel_mut(1).copy_from_slice(&r);
}

/// `DistanceModelType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistanceModel {
    Linear,
    Inverse,
    Exponential,
}

/// A PannerNode's attributes that are not AudioParams.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PannerSettings {
    pub distance_model: DistanceModel,
    pub ref_distance: f64,
    pub max_distance: f64,
    pub rolloff_factor: f64,
    pub cone_inner_angle: f64,
    pub cone_outer_angle: f64,
    pub cone_outer_gain: f64,
}

impl Default for PannerSettings {
    fn default() -> Self {
        PannerSettings {
            distance_model: DistanceModel::Inverse,
            ref_distance: 1.0,
            max_distance: 10000.0,
            rolloff_factor: 1.0,
            cone_inner_angle: 360.0,
            cone_outer_angle: 360.0,
            cone_outer_gain: 0.0,
        }
    }
}

type V3 = [f64; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn norm(a: V3) -> f64 {
    dot(a, a).sqrt()
}
fn normalize(a: V3) -> V3 {
    let n = norm(a);
    if n == 0.0 {
        a
    } else {
        [a[0] / n, a[1] / n, a[2] / n]
    }
}
fn is_zero(a: V3) -> bool {
    a == [0.0; 3]
}
fn degrees(cos: f64) -> f64 {
    cos.clamp(-1.0, 1.0).acos().to_degrees()
}

/// Where the source and the listener are for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub position: V3,
    pub orientation: V3,
    pub listener_position: V3,
    pub listener_forward: V3,
    pub listener_up: V3,
}

/// §6.? "Azimuth and Elevation": the source's azimuth and elevation in degrees.
pub fn azimuth_elevation(g: &Geometry) -> (f64, f64) {
    let source_listener = normalize(sub(g.position, g.listener_position));
    if is_zero(source_listener) {
        return (0.0, 0.0);
    }
    let right = normalize(cross(g.listener_forward, g.listener_up));
    let forward = normalize(g.listener_forward);
    let up = cross(right, forward);
    let up_projection = dot(source_listener, up);
    let projected = normalize(sub(
        source_listener,
        [
            up_projection * up[0],
            up_projection * up[1],
            up_projection * up[2],
        ],
    ));
    let mut azimuth = degrees(dot(projected, right));
    if dot(projected, forward) < 0.0 {
        azimuth = 360.0 - azimuth;
    }
    azimuth = if (0.0..=270.0).contains(&azimuth) {
        90.0 - azimuth
    } else {
        450.0 - azimuth
    };
    let mut elevation = 90.0 - degrees(dot(source_listener, up));
    if elevation > 90.0 {
        elevation = 180.0 - elevation;
    } else if elevation < -90.0 {
        elevation = -180.0 - elevation;
    }
    (azimuth, elevation)
}

impl PannerSettings {
    /// §6 "Distance Effects".
    pub fn distance_gain(&self, distance: f64) -> f64 {
        let (r, m, f) = (self.ref_distance, self.max_distance, self.rolloff_factor);
        match self.distance_model {
            DistanceModel::Linear => {
                let f = f.clamp(0.0, 1.0);
                if m <= r {
                    return 1.0 - f;
                }
                let d = distance.clamp(r, m);
                1.0 - f * (d - r) / (m - r)
            }
            DistanceModel::Inverse => {
                let d = distance.max(r);
                if r == 0.0 && d == 0.0 {
                    return 1.0;
                }
                r / (r + f * (d - r))
            }
            DistanceModel::Exponential => {
                let d = distance.max(r);
                if r == 0.0 {
                    return if d == 0.0 { 1.0 } else { 0.0 };
                }
                (d / r).powf(-f)
            }
        }
    }

    /// §6 "Sound Cones".
    pub fn cone_gain(&self, g: &Geometry) -> f64 {
        if is_zero(g.orientation)
            || (self.cone_inner_angle == 360.0 && self.cone_outer_angle == 360.0)
        {
            return 1.0;
        }
        let to_listener = normalize(sub(g.listener_position, g.position));
        let angle = degrees(dot(to_listener, normalize(g.orientation))).abs();
        let inner = self.cone_inner_angle.abs() / 2.0;
        let outer = self.cone_outer_angle.abs() / 2.0;
        if angle <= inner {
            1.0
        } else if angle >= outer {
            self.cone_outer_gain
        } else {
            let x = (angle - inner) / (outer - inner);
            (1.0 - x) + self.cone_outer_gain * x
        }
    }
}

/// PannerNode: one quantum with the equal-power model. `params`: positionX/Y/Z, orientationX/Y/Z,
/// then the listener's positionX/Y/Z, forwardX/Y/Z and upX/Y/Z.
pub fn spatial_pan(
    settings: &PannerSettings,
    params: &[&[f32; N]],
    input: &AudioBus,
    out: &mut AudioBus,
) {
    out.zero(2);
    let constant = params
        .iter()
        .all(|p| p.iter().all(|v| v.to_bits() == p[0].to_bits()));
    let at = |f: usize| {
        let v = |i: usize| -> V3 {
            [
                params[i][f] as f64,
                params[i + 1][f] as f64,
                params[i + 2][f] as f64,
            ]
        };
        Geometry {
            position: v(0),
            orientation: v(3),
            listener_position: v(6),
            listener_forward: v(9),
            listener_up: v(12),
        }
    };
    let gains = |g: &Geometry| {
        let (azimuth, _) = azimuth_elevation(g);
        let gain = settings.distance_gain(norm(sub(g.position, g.listener_position)))
            * settings.cone_gain(g);
        (azimuth, gain)
    };
    let fixed = constant.then(|| gains(&at(0)));
    let mono = input.channels() == 1;
    let mut l = [0.0f32; N];
    let mut r = [0.0f32; N];
    for f in 0..N {
        let (azimuth, gain) = fixed.unwrap_or_else(|| gains(&at(f)));
        let mut a = azimuth.clamp(-180.0, 180.0);
        if a < -90.0 {
            a = -180.0 - a;
        } else if a > 90.0 {
            a = 180.0 - a;
        }
        let x = if mono {
            (a + 90.0) / 180.0
        } else if a <= 0.0 {
            (a + 90.0) / 90.0
        } else {
            a / 90.0
        };
        pan_frame(input, f, x, a <= 0.0, &mut l[f], &mut r[f]);
        l[f] *= gain as f32;
        r[f] *= gain as f32;
    }
    out.channel_mut(0).copy_from_slice(&l);
    out.channel_mut(1).copy_from_slice(&r);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(position: V3) -> Geometry {
        Geometry {
            position,
            orientation: [1.0, 0.0, 0.0],
            listener_position: [0.0; 3],
            listener_forward: [0.0, 0.0, -1.0],
            listener_up: [0.0, 1.0, 0.0],
        }
    }

    #[test]
    fn a_source_to_the_right_is_at_azimuth_90_and_in_front_at_0() {
        let (a, e) = azimuth_elevation(&geometry([1.0, 0.0, 0.0]));
        assert!((a - 90.0).abs() < 1e-9 && e.abs() < 1e-9, "{a} {e}");
        let (a, _) = azimuth_elevation(&geometry([0.0, 0.0, -1.0]));
        assert!(a.abs() < 1e-9);
        let (_, e) = azimuth_elevation(&geometry([0.0, 1.0, 0.0]));
        assert!((e - 90.0).abs() < 1e-9);
    }

    #[test]
    fn the_distance_models_follow_their_formulas() {
        let mut s = PannerSettings::default();
        assert!((s.distance_gain(4.0) - 0.25).abs() < 1e-12);
        s.distance_model = DistanceModel::Linear;
        s.max_distance = 11.0;
        assert!((s.distance_gain(6.0) - 0.5).abs() < 1e-12);
        s.distance_model = DistanceModel::Exponential;
        s.rolloff_factor = 2.0;
        assert!((s.distance_gain(2.0) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn a_hard_right_mono_source_goes_to_the_right_channel_only() {
        let mut input = AudioBus::new(1);
        input.channel_mut(0).fill(1.0);
        let mut out = AudioBus::new(2);
        stereo_pan(&input, &[1.0; N], &mut out);
        assert!(out.channel(0)[0].abs() < 1e-7 && (out.channel(1)[0] - 1.0).abs() < 1e-7);
        let p = [[0.0f32; N]; 15];
        let mut params: Vec<&[f32; N]> = p.iter().collect();
        let right = [1.0f32; N];
        let fwd = [-1.0f32; N];
        params[0] = &right;
        params[11] = &fwd;
        params[13] = &right;
        spatial_pan(&PannerSettings::default(), &params, &input, &mut out);
        assert!(out.channel(0)[5].abs() < 1e-7, "{}", out.channel(0)[5]);
        assert!((out.channel(1)[5] - 1.0).abs() < 1e-6);
    }
}
