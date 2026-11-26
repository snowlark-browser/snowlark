// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An SVG `<filter>` element as display-list data (Filter Effects 1 §6-§9, §15): the graph of
//! filter primitives a `filter: url(#id)` reference applies, resolved for one box.
//!
//! Everything is resolved where the reference is used: the filter region and every primitive
//! subregion are rectangles in the list's coordinates where the filter group is pushed (CSS px),
//! and the lengths of the primitives (standard deviations, offsets, radii, light positions) are in
//! the same units, whatever `filterUnits` and `primitiveUnits` said. A rasterizer scales them to
//! device pixels, as it does the filter functions' lengths.

use crate::geom::{Color, Rect};
use crate::ImageId;

/// The primitives of one filter at most (a longer filter draws nothing).
pub const MAX_PRIMITIVES: usize = 256;
/// The inputs of one `feMerge` at most.
pub const MAX_MERGE_INPUTS: usize = 64;
/// The values of a transfer function's table at most.
pub const MAX_TABLE: usize = 1024;
/// `feConvolveMatrix`'s order at most, per axis.
pub const MAX_ORDER: u32 = 16;
/// `feTurbulence`'s octaves at most (more add nothing visible).
pub const MAX_OCTAVES: u32 = 16;

/// A filter: its region and its primitives in document order (the last one's result is the
/// filter's result).
#[derive(Clone, Debug, PartialEq)]
pub struct SvgFilter {
    /// The filter region (§6): nothing of the filter's input or result shows outside it.
    pub region: Rect,
    pub primitives: Vec<Primitive>,
}

/// One filter primitive (§9).
#[derive(Clone, Debug, PartialEq)]
pub struct Primitive {
    pub kind: Kind,
    /// `in` (and `in2`, or each `feMergeNode`'s `in`), as many as the kind reads.
    pub inputs: Vec<Input>,
    /// The primitive subregion (§9.4), within the filter region: its result is clipped to it.
    pub subregion: Rect,
    /// `color-interpolation-filters: linearRGB` (the default): the primitive works on
    /// linear-light values; else on sRGB values.
    pub linear_rgb: bool,
}

/// Where a primitive's input comes from (§9.3 `in`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Input {
    /// The element's content (for a reference after other filter functions: their result).
    SourceGraphic,
    /// The content's alpha, black.
    SourceAlpha,
    /// Transparent black (`BackgroundImage`, `BackgroundAlpha`, `FillPaint`, `StrokePaint`).
    Transparent,
    /// The result of an earlier primitive of the same filter (its index).
    Result(u16),
}

/// How pixels beyond an input's edges are made up (`edgeMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EdgeMode {
    #[default]
    None,
    Duplicate,
    Wrap,
    Mirror,
}

/// `feComposite`'s `operator`: `in` (source) with `in2` (destination).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CompositeOp {
    #[default]
    Over,
    In,
    Out,
    Atop,
    Xor,
    Lighter,
    /// `k1·i1·i2 + k2·i1 + k3·i2 + k4` on premultiplied values.
    Arithmetic,
}

/// A transfer function's `type` (§9.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TransferKind {
    #[default]
    Identity,
    Table,
    Discrete,
    Linear,
    Gamma,
}

/// One channel's function of `feComponentTransfer`, on non-premultiplied values.
#[derive(Clone, Debug, PartialEq)]
pub struct Transfer {
    pub kind: TransferKind,
    /// `tableValues` (table and discrete; with none, the function is the identity).
    pub table: Vec<f32>,
    pub slope: f32,
    pub intercept: f32,
    pub amplitude: f32,
    pub exponent: f32,
    pub offset: f32,
}

impl Default for Transfer {
    fn default() -> Transfer {
        Transfer {
            kind: TransferKind::Identity,
            table: Vec::new(),
            slope: 1.0,
            intercept: 0.0,
            amplitude: 1.0,
            exponent: 1.0,
            offset: 0.0,
        }
    }
}

impl Transfer {
    /// The function's value at `c` (0..=1), not clamped.
    pub fn eval(&self, c: f32) -> f32 {
        let t = &self.table;
        match self.kind {
            TransferKind::Identity => c,
            TransferKind::Table | TransferKind::Discrete if t.is_empty() => c,
            TransferKind::Table => {
                let n = t.len() - 1;
                if n == 0 {
                    return t[0];
                }
                let k = ((c * n as f32).floor() as usize).min(n - 1);
                let v = t[k];
                v + (c - k as f32 / n as f32) * n as f32 * (t[k + 1] - v)
            }
            TransferKind::Discrete => {
                let n = t.len();
                t[((c * n as f32).floor() as usize).min(n - 1)]
            }
            TransferKind::Linear => self.slope * c + self.intercept,
            TransferKind::Gamma => self.amplitude * c.max(0.0).powf(self.exponent) + self.offset,
        }
    }
}

/// A light source of the lighting primitives (§9.16-§9.18), positions in the list's coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Light {
    /// `feDistantLight`: degrees.
    Distant { azimuth: f32, elevation: f32 },
    /// `fePointLight`.
    Point { x: f32, y: f32, z: f32 },
    /// `feSpotLight`: `cone` is `limitingConeAngle` in degrees, when given.
    Spot {
        x: f32,
        y: f32,
        z: f32,
        at_x: f32,
        at_y: f32,
        at_z: f32,
        exponent: f32,
        cone: Option<f32>,
    },
}

/// The color channel `feDisplacementMap` reads (`xChannelSelector`, `yChannelSelector`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Channel {
    R,
    G,
    B,
    #[default]
    A,
}

/// What a primitive does.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// `feFlood`: the color with `flood-opacity` in its alpha.
    Flood(Color),
    /// `feColorMatrix` (every `type` as a matrix, by rows on non-premultiplied `[r, g, b, a, 1]`).
    ColorMatrix([[f32; 5]; 4]),
    /// `feComponentTransfer`: R, G, B, A.
    ComponentTransfer(Box<[Transfer; 4]>),
    /// `feGaussianBlur`: standard deviations per axis.
    GaussianBlur { x: f32, y: f32 },
    /// `feOffset`.
    Offset { dx: f32, dy: f32 },
    /// `feMerge`: the inputs over one another, the first at the bottom.
    Merge,
    /// `feBlend`: `in` blended onto `in2` (the `mix-blend-mode` keyword's index, `normal` 0).
    Blend(u8),
    /// `feComposite`; `k` for `arithmetic`.
    Composite { op: CompositeOp, k: [f32; 4] },
    /// `feDropShadow`: the input over its alpha, blurred, offset and in the color.
    DropShadow {
        dx: f32,
        dy: f32,
        x: f32,
        y: f32,
        color: Color,
    },
    /// `feMorphology`: radii per axis.
    Morphology { dilate: bool, rx: f32, ry: f32 },
    /// `feTurbulence`: base frequencies in cycles per list unit.
    Turbulence {
        base_x: f32,
        base_y: f32,
        octaves: u32,
        seed: f32,
        stitch: bool,
        fractal_noise: bool,
    },
    /// `feTile`: the input's subregion repeated over this primitive's.
    Tile,
    /// `feConvolveMatrix`: `kernel` row-major as written, `order_x × order_y` values.
    ConvolveMatrix {
        order_x: u32,
        order_y: u32,
        kernel: Vec<f32>,
        divisor: f32,
        bias: f32,
        target_x: u32,
        target_y: u32,
        edge: EdgeMode,
        preserve_alpha: bool,
    },
    /// `feDisplacementMap`: `in` displaced by `in2`'s channels.
    DisplacementMap { scale: f32, x: Channel, y: Channel },
    /// `feDiffuseLighting`.
    DiffuseLighting {
        surface_scale: f32,
        constant: f32,
        color: Color,
        light: Light,
    },
    /// `feSpecularLighting`.
    SpecularLighting {
        surface_scale: f32,
        constant: f32,
        exponent: f32,
        color: Color,
        light: Light,
    },
    /// `feImage` (§9.15): the part `src` of `image` (in image pixels; empty: all of it) drawn
    /// into `dest` (the list's coordinates, after `preserveAspectRatio`), in sRGB.
    Image {
        image: ImageId,
        dest: Rect,
        src: Rect,
    },
}

impl Kind {
    /// How many inputs the primitive reads (`None`: any number, `feMerge`).
    pub fn inputs(&self) -> Option<usize> {
        Some(match self {
            Kind::Flood(_) | Kind::Turbulence { .. } | Kind::Image { .. } => 0,
            Kind::Blend(_) | Kind::Composite { .. } | Kind::DisplacementMap { .. } => 2,
            Kind::Merge => return None,
            _ => 1,
        })
    }
}

fn finite(v: &[f32]) -> bool {
    v.iter().all(|v| v.is_finite())
}

fn rect_finite(r: &Rect) -> bool {
    finite(&[r.x, r.y, r.width, r.height]) && r.width >= 0.0 && r.height >= 0.0
}

impl Light {
    fn is_valid(&self) -> bool {
        match *self {
            Light::Distant { azimuth, elevation } => finite(&[azimuth, elevation]),
            Light::Point { x, y, z } => finite(&[x, y, z]),
            Light::Spot {
                x,
                y,
                z,
                at_x,
                at_y,
                at_z,
                exponent,
                cone,
            } => {
                finite(&[x, y, z, at_x, at_y, at_z, exponent]) && cone.is_none_or(|c| c.is_finite())
            }
        }
    }
}

impl SvgFilter {
    /// Whether the filter is one a rasterizer may draw: finite values in range, as many inputs
    /// as each primitive reads, each naming an earlier result, and the limits above.
    pub fn is_valid(&self) -> bool {
        rect_finite(&self.region)
            && self.primitives.len() <= MAX_PRIMITIVES
            && self
                .primitives
                .iter()
                .enumerate()
                .all(|(i, p)| p.is_valid(i))
    }

    /// The images its `feImage` primitives draw.
    pub fn images(&self) -> impl Iterator<Item = ImageId> + '_ {
        self.primitives.iter().filter_map(|p| match p.kind {
            Kind::Image { image, .. } => Some(image),
            _ => None,
        })
    }

    /// Whether the filter changes nothing: it has no primitives or an empty region (its content
    /// is drawn as it is).
    pub fn is_identity(&self) -> bool {
        self.primitives.is_empty() || self.region.is_empty()
    }

    /// Where the filter's result of content within `r` can show: its region, whatever the
    /// content (a flood or noise paints where the content is transparent).
    pub fn map_rect(&self, r: Rect) -> Rect {
        if self.is_identity() {
            r
        } else {
            self.region
        }
    }
}

impl Primitive {
    fn is_valid(&self, index: usize) -> bool {
        let inputs_ok = match self.kind.inputs() {
            Some(n) => self.inputs.len() == n,
            None => self.inputs.len() <= MAX_MERGE_INPUTS,
        } && self.inputs.iter().all(|i| match i {
            Input::Result(r) => (*r as usize) < index,
            _ => true,
        });
        let kind_ok = match &self.kind {
            Kind::Flood(_) | Kind::Merge | Kind::Tile | Kind::Blend(_) => true,
            Kind::ColorMatrix(m) => m.iter().all(|row| finite(row)),
            Kind::ComponentTransfer(f) => f.iter().all(|t| {
                t.table.len() <= MAX_TABLE
                    && finite(&t.table)
                    && finite(&[t.slope, t.intercept, t.amplitude, t.exponent, t.offset])
            }),
            Kind::GaussianBlur { x, y } => finite(&[*x, *y]) && *x >= 0.0 && *y >= 0.0,
            Kind::Offset { dx, dy } => finite(&[*dx, *dy]),
            Kind::Composite { k, .. } => finite(k),
            Kind::DropShadow { dx, dy, x, y, .. } => {
                finite(&[*dx, *dy, *x, *y]) && *x >= 0.0 && *y >= 0.0
            }
            Kind::Morphology { rx, ry, .. } => finite(&[*rx, *ry]) && *rx >= 0.0 && *ry >= 0.0,
            Kind::Turbulence {
                base_x,
                base_y,
                octaves,
                seed,
                ..
            } => {
                finite(&[*base_x, *base_y, *seed])
                    && *base_x >= 0.0
                    && *base_y >= 0.0
                    && *octaves <= MAX_OCTAVES
            }
            Kind::ConvolveMatrix {
                order_x,
                order_y,
                kernel,
                divisor,
                bias,
                target_x,
                target_y,
                ..
            } => {
                (1..=MAX_ORDER).contains(order_x)
                    && (1..=MAX_ORDER).contains(order_y)
                    && kernel.len() == (order_x * order_y) as usize
                    && finite(kernel)
                    && finite(&[*divisor, *bias])
                    && *divisor != 0.0
                    && target_x < order_x
                    && target_y < order_y
            }
            Kind::DisplacementMap { scale, .. } => scale.is_finite(),
            Kind::DiffuseLighting {
                surface_scale,
                constant,
                light,
                ..
            } => finite(&[*surface_scale, *constant]) && *constant >= 0.0 && light.is_valid(),
            Kind::SpecularLighting {
                surface_scale,
                constant,
                exponent,
                light,
                ..
            } => {
                finite(&[*surface_scale, *constant, *exponent])
                    && *constant >= 0.0
                    && light.is_valid()
            }
            Kind::Image { dest, src, .. } => rect_finite(dest) && rect_finite(src),
        };
        inputs_ok && kind_ok && rect_finite(&self.subregion)
    }
}

/// The matrix of `feColorMatrix type="saturate"` (§9.6), by rows.
pub fn saturate_matrix(s: f32) -> [[f32; 5]; 4] {
    [
        [
            0.213 + 0.787 * s,
            0.715 - 0.715 * s,
            0.072 - 0.072 * s,
            0.0,
            0.0,
        ],
        [
            0.213 - 0.213 * s,
            0.715 + 0.285 * s,
            0.072 - 0.072 * s,
            0.0,
            0.0,
        ],
        [
            0.213 - 0.213 * s,
            0.715 - 0.715 * s,
            0.072 + 0.928 * s,
            0.0,
            0.0,
        ],
        [0.0, 0.0, 0.0, 1.0, 0.0],
    ]
}

/// The matrix of `feColorMatrix type="hueRotate"` (§9.6), degrees, by rows.
pub fn hue_rotate_matrix(degrees: f32) -> [[f32; 5]; 4] {
    let (sin, cos) = degrees.to_radians().sin_cos();
    [
        [
            0.213 + cos * 0.787 - sin * 0.213,
            0.715 - cos * 0.715 - sin * 0.715,
            0.072 - cos * 0.072 + sin * 0.928,
            0.0,
            0.0,
        ],
        [
            0.213 - cos * 0.213 + sin * 0.143,
            0.715 + cos * 0.285 + sin * 0.140,
            0.072 - cos * 0.072 - sin * 0.283,
            0.0,
            0.0,
        ],
        [
            0.213 - cos * 0.213 - sin * 0.787,
            0.715 - cos * 0.715 + sin * 0.715,
            0.072 + cos * 0.928 + sin * 0.072,
            0.0,
            0.0,
        ],
        [0.0, 0.0, 0.0, 1.0, 0.0],
    ]
}

/// The matrix of `feColorMatrix type="luminanceToAlpha"` (§9.6), by rows.
pub const LUMINANCE_TO_ALPHA: [[f32; 5]; 4] = [
    [0.0; 5],
    [0.0; 5],
    [0.0; 5],
    [0.2125, 0.7154, 0.0721, 0.0, 0.0],
];

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A valid filter with a primitive of every kind (each reading the previous result).
    pub(crate) fn every_kind() -> SvgFilter {
        let c = Color::rgba(10, 20, 30, 200);
        let spot = Light::Spot {
            x: 1.0,
            y: 2.0,
            z: 3.0,
            at_x: 4.0,
            at_y: 5.0,
            at_z: 0.0,
            exponent: 2.0,
            cone: Some(30.0),
        };
        let kinds = vec![
            Kind::Flood(c),
            Kind::ColorMatrix(hue_rotate_matrix(30.0)),
            Kind::ComponentTransfer(Box::new([
                Transfer {
                    kind: TransferKind::Table,
                    table: vec![0.0, 0.5, 1.0],
                    ..Transfer::default()
                },
                Transfer {
                    kind: TransferKind::Gamma,
                    exponent: 2.0,
                    ..Transfer::default()
                },
                Transfer::default(),
                Transfer {
                    kind: TransferKind::Discrete,
                    table: vec![1.0, 0.0],
                    ..Transfer::default()
                },
            ])),
            Kind::GaussianBlur { x: 1.0, y: 2.0 },
            Kind::Offset { dx: -3.0, dy: 4.0 },
            Kind::Merge,
            Kind::Blend(10),
            Kind::Composite {
                op: CompositeOp::Arithmetic,
                k: [0.5, 0.5, 0.25, 0.0],
            },
            Kind::DropShadow {
                dx: 2.0,
                dy: 2.0,
                x: 1.0,
                y: 1.0,
                color: c,
            },
            Kind::Morphology {
                dilate: true,
                rx: 2.0,
                ry: 1.0,
            },
            Kind::Turbulence {
                base_x: 0.05,
                base_y: 0.1,
                octaves: 2,
                seed: 7.0,
                stitch: true,
                fractal_noise: false,
            },
            Kind::Tile,
            Kind::ConvolveMatrix {
                order_x: 3,
                order_y: 2,
                kernel: vec![1.0, 0.0, -1.0, 2.0, 0.0, -2.0],
                divisor: 1.0,
                bias: 0.0,
                target_x: 1,
                target_y: 1,
                edge: EdgeMode::Wrap,
                preserve_alpha: true,
            },
            Kind::DisplacementMap {
                scale: 10.0,
                x: Channel::R,
                y: Channel::B,
            },
            Kind::DiffuseLighting {
                surface_scale: 2.0,
                constant: 1.0,
                color: c,
                light: Light::Distant {
                    azimuth: 45.0,
                    elevation: 30.0,
                },
            },
            Kind::SpecularLighting {
                surface_scale: 1.0,
                constant: 1.0,
                exponent: 20.0,
                color: c,
                light: spot,
            },
            Kind::Image {
                image: ImageId(7),
                dest: Rect::new(1.0, 2.0, 30.0, 20.0),
                src: Rect::new(0.0, 0.0, 15.0, 10.0),
            },
        ];
        let primitives = kinds
            .into_iter()
            .enumerate()
            .map(|(i, kind)| {
                let previous = match i {
                    0 => Input::SourceGraphic,
                    i => Input::Result(i as u16 - 1),
                };
                let inputs = match kind.inputs() {
                    Some(0) => vec![],
                    Some(1) => vec![previous],
                    _ => vec![previous, Input::SourceAlpha],
                };
                Primitive {
                    kind,
                    inputs,
                    subregion: Rect::new(-5.0, -5.0, 50.0, 40.0),
                    linear_rgb: i % 2 == 0,
                }
            })
            .collect();
        SvgFilter {
            region: Rect::new(-10.0, -10.0, 60.0, 50.0),
            primitives,
        }
    }

    #[test]
    fn the_sample_is_valid() {
        assert!(every_kind().is_valid());
    }

    fn flood() -> Primitive {
        Primitive {
            kind: Kind::Flood(Color::rgba(0, 128, 0, 255)),
            inputs: vec![],
            subregion: Rect::new(0.0, 0.0, 10.0, 10.0),
            linear_rgb: true,
        }
    }

    #[test]
    fn inputs_name_earlier_results_only() {
        let mut f = SvgFilter {
            region: Rect::new(0.0, 0.0, 10.0, 10.0),
            primitives: vec![flood(), flood()],
        };
        assert!(f.is_valid());
        f.primitives[1] = Primitive {
            kind: Kind::Offset { dx: 1.0, dy: 0.0 },
            inputs: vec![Input::Result(0)],
            ..flood()
        };
        assert!(f.is_valid());
        f.primitives[1].inputs = vec![Input::Result(1)];
        assert!(!f.is_valid());
        f.primitives[1].inputs = vec![];
        assert!(!f.is_valid());
        f.primitives[1].inputs = vec![Input::SourceAlpha];
        f.primitives[1].kind = Kind::GaussianBlur {
            x: f32::NAN,
            y: 0.0,
        };
        assert!(!f.is_valid());
    }

    #[test]
    fn transfer_functions_follow_the_spec() {
        let t = |kind, table: &[f32]| Transfer {
            kind,
            table: table.to_vec(),
            ..Transfer::default()
        };
        let table = t(TransferKind::Table, &[0.0, 1.0, 0.0]);
        assert!((table.eval(0.25) - 0.5).abs() < 1e-6);
        assert!((table.eval(0.75) - 0.5).abs() < 1e-6);
        assert_eq!(table.eval(1.0), 0.0);
        let discrete = t(TransferKind::Discrete, &[0.2, 0.8]);
        assert_eq!(discrete.eval(0.49), 0.2);
        assert_eq!(discrete.eval(0.5), 0.8);
        assert_eq!(discrete.eval(1.0), 0.8);
        assert_eq!(t(TransferKind::Table, &[]).eval(0.3), 0.3);
        let gamma = Transfer {
            kind: TransferKind::Gamma,
            amplitude: 2.0,
            exponent: 2.0,
            offset: 0.1,
            ..Transfer::default()
        };
        assert!((gamma.eval(0.5) - 0.6).abs() < 1e-6);
    }
}
