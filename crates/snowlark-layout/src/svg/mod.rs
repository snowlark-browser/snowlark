// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SVG 2 geometry and the micro-syntaxes of its attributes, independent of the DOM and of style:
//! numbers and lists (§4.2), lengths and angles (§4.5.2, §4.5.3), transform lists (§8.5, CSS
//! Transforms §7), `viewBox` and `preserveAspectRatio` (§8.6, §8.7), points (§10.7), path data
//! with its error handling (§9.3, §9.5) and the path geometry the DOM asks about (§4.4.3: total
//! length, point at length, fill and stroke containment, bounding boxes).
//!
//! The engine's SVG DOM reflects attributes through these parsers and serializers; the renderer
//! builds element geometry from them.

pub mod aspect;
pub mod geom;
pub mod length;
pub mod parse;
pub mod path;
pub mod smil;
pub mod timegraph;
pub mod transform;

pub use aspect::{Align, AspectRatio, ViewBox};
pub use geom::{Matrix, StrokeStyle};
pub use length::{Angle, AngleUnit, Length, LengthContext, LengthDir, LengthUnit};
pub use parse::{fmt_number, parse_number_list, parse_points, Scanner};
pub use path::{PathCommand, PathData};
pub use transform::{Transform, TransformKind};

#[cfg(test)]
mod tests;
