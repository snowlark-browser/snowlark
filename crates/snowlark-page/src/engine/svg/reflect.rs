// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which content attribute each SVG DOM reflecting attribute reflects, and how (SVG 2 §4.3.4 and
//! each element's DOM interface; Filter Effects §15; CSS Masking §14): the attribute's name, its
//! value type, its directionality for lengths and its initial value (SVG 2 §4.1: used when the
//! attribute is absent or invalid).

use snowlark_layout::svg::LengthDir;

/// How an attribute's value is reflected.
#[derive(Debug, PartialEq)]
pub enum Kind {
    /// `SVGAnimatedLength`.
    Length {
        dir: LengthDir,
        initial: &'static str,
    },
    /// `SVGAnimatedLengthList`.
    LengthList(LengthDir),
    /// `SVGAnimatedNumber` of a `<number>` attribute.
    Number { initial: f32 },
    /// `SVGAnimatedNumber` of one number of a `<number-optional-number>` attribute (the second
    /// defaults to the first).
    NumberPart { second: bool, initial: f32 },
    /// `SVGAnimatedInteger`.
    Integer { initial: i32 },
    /// `SVGAnimatedInteger` of one integer of an `<integer> <integer>?` attribute.
    IntegerPart { second: bool, initial: i32 },
    /// `SVGAnimatedNumberList`.
    NumberList,
    /// `SVGAnimatedBoolean` (`true`/`false`), initially false.
    Boolean,
    /// `SVGAnimatedEnumeration`: keyword `i` is value `i + 1`.
    Enum {
        keywords: &'static [&'static str],
        initial: u16,
    },
    /// `SVGAnimatedEnumeration` of the type of `orient` (SVG 2 §11.6.3 `orientType`).
    OrientType,
    /// `SVGAnimatedAngle` of `orient` (`orientAngle`).
    OrientAngle,
    /// `SVGAnimatedString` of one attribute.
    String,
    /// `SVGAnimatedString` of `href`, additionally reflecting the deprecated `xlink:href` (SVG 2
    /// §4.7.4).
    Href,
    /// `SVGAnimatedRect` (`viewBox`).
    Rect,
    /// `SVGAnimatedPreserveAspectRatio`.
    Aspect,
    /// `SVGAnimatedTransformList` of a presentation attribute.
    TransformList,
    /// `SVGPointList` (`points`).
    PointList,
    /// `SVGStringList`; `systemLanguage` is comma separated.
    StringList { commas: bool },
}

/// A reflected content attribute (in the null namespace).
#[derive(Debug, PartialEq)]
pub struct Spec {
    pub attr: &'static str,
    pub kind: Kind,
}

/// What a member without a reflection rule reflects: no attribute.
pub(crate) const NO_RULE: Spec = Spec {
    attr: "",
    kind: Kind::String,
};

const UNITS: &[&str] = &["userSpaceOnUse", "objectBoundingBox"];
const EDGE_MODES: &[&str] = &["duplicate", "wrap", "none"];
const CHANNELS: &[&str] = &["R", "G", "B", "A"];
const BLEND_MODES: &[&str] = &[
    "normal",
    "multiply",
    "screen",
    "darken",
    "lighten",
    "overlay",
    "color-dodge",
    "color-burn",
    "hard-light",
    "soft-light",
    "difference",
    "exclusion",
    "hue",
    "saturation",
    "color",
    "luminosity",
];

macro_rules! len {
    ($attr:literal, $dir:ident, $initial:literal) => {
        &Spec {
            attr: $attr,
            kind: Kind::Length {
                dir: LengthDir::$dir,
                initial: $initial,
            },
        }
    };
}

macro_rules! num {
    ($attr:literal, $initial:literal) => {
        &Spec {
            attr: $attr,
            kind: Kind::Number { initial: $initial },
        }
    };
}

macro_rules! part {
    ($attr:literal, $second:literal, $initial:literal) => {
        &Spec {
            attr: $attr,
            kind: Kind::NumberPart {
                second: $second,
                initial: $initial,
            },
        }
    };
}

macro_rules! en {
    ($attr:literal, $keywords:expr, $initial:literal) => {
        &Spec {
            attr: $attr,
            kind: Kind::Enum {
                keywords: $keywords,
                initial: $initial,
            },
        }
    };
}

macro_rules! simple {
    ($attr:literal, $kind:expr) => {
        &Spec {
            attr: $attr,
            kind: $kind,
        }
    };
}

/// The attribute the SVG DOM attribute `member` of `owner` (an interface or mixin) reflects.
pub(crate) fn spec(owner: &str, member: &str) -> Option<&'static Spec> {
    Some(match (owner, member) {
        ("SVGElement", "className") => simple!("class", Kind::String),
        ("SVGGraphicsElement" | "SVGClipPathElement", "transform") => {
            simple!("transform", Kind::TransformList)
        }
        ("SVGGeometryElement" | "SVGPathElement", "pathLength") => num!("pathLength", 0.0),
        ("SVGTests", "requiredExtensions") => {
            simple!("requiredExtensions", Kind::StringList { commas: false })
        }
        ("SVGTests", "systemLanguage") => {
            simple!("systemLanguage", Kind::StringList { commas: true })
        }
        ("SVGFitToViewBox", "viewBox") => simple!("viewBox", Kind::Rect),
        ("SVGFitToViewBox" | "SVGImageElement" | "SVGFEImageElement", "preserveAspectRatio") => {
            simple!("preserveAspectRatio", Kind::Aspect)
        }
        ("SVGURIReference", "href") => simple!("href", Kind::Href),
        ("SVGSVGElement", "width") => len!("width", Horizontal, "100%"),
        ("SVGSVGElement", "height") => len!("height", Vertical, "100%"),
        (
            "SVGSVGElement"
            | "SVGUseElement"
            | "SVGRectElement"
            | "SVGImageElement"
            | "SVGForeignObjectElement"
            | "SVGPatternElement",
            "x" | "width",
        ) => {
            if member == "x" {
                len!("x", Horizontal, "0")
            } else {
                len!("width", Horizontal, "0")
            }
        }
        (
            "SVGSVGElement"
            | "SVGUseElement"
            | "SVGRectElement"
            | "SVGImageElement"
            | "SVGForeignObjectElement"
            | "SVGPatternElement",
            "y" | "height",
        ) => {
            if member == "y" {
                len!("y", Vertical, "0")
            } else {
                len!("height", Vertical, "0")
            }
        }
        ("SVGRectElement" | "SVGEllipseElement", "rx") => len!("rx", Horizontal, "0"),
        ("SVGRectElement" | "SVGEllipseElement", "ry") => len!("ry", Vertical, "0"),
        ("SVGCircleElement" | "SVGEllipseElement", "cx") => len!("cx", Horizontal, "0"),
        ("SVGCircleElement" | "SVGEllipseElement", "cy") => len!("cy", Vertical, "0"),
        ("SVGCircleElement", "r") => len!("r", Other, "0"),
        ("SVGLineElement", "x1") => len!("x1", Horizontal, "0"),
        ("SVGLineElement", "y1") => len!("y1", Vertical, "0"),
        ("SVGLineElement", "x2") => len!("x2", Horizontal, "0"),
        ("SVGLineElement", "y2") => len!("y2", Vertical, "0"),
        ("SVGAnimatedPoints", "points" | "animatedPoints") => simple!("points", Kind::PointList),
        ("SVGTextContentElement", "textLength") => len!("textLength", Horizontal, "0"),
        ("SVGTextContentElement", "lengthAdjust") => {
            en!("lengthAdjust", &["spacing", "spacingAndGlyphs"], 1)
        }
        ("SVGTextPositioningElement", "x") => {
            simple!("x", Kind::LengthList(LengthDir::Horizontal))
        }
        ("SVGTextPositioningElement", "y") => simple!("y", Kind::LengthList(LengthDir::Vertical)),
        ("SVGTextPositioningElement", "dx") => {
            simple!("dx", Kind::LengthList(LengthDir::Horizontal))
        }
        ("SVGTextPositioningElement", "dy") => {
            simple!("dy", Kind::LengthList(LengthDir::Vertical))
        }
        ("SVGTextPositioningElement", "rotate") => simple!("rotate", Kind::NumberList),
        ("SVGTextPathElement", "startOffset") => len!("startOffset", Horizontal, "0"),
        ("SVGTextPathElement", "method") => en!("method", &["align", "stretch"], 1),
        ("SVGTextPathElement", "spacing") => en!("spacing", &["auto", "exact"], 2),
        ("SVGTextPathElement", "side") => en!("side", &["left", "right"], 1),
        ("SVGMarkerElement", "refX") => len!("refX", Horizontal, "0"),
        ("SVGMarkerElement", "refY") => len!("refY", Vertical, "0"),
        ("SVGMarkerElement", "markerWidth") => len!("markerWidth", Horizontal, "3"),
        ("SVGMarkerElement", "markerHeight") => len!("markerHeight", Vertical, "3"),
        ("SVGMarkerElement", "markerUnits") => {
            en!("markerUnits", &["userSpaceOnUse", "strokeWidth"], 2)
        }
        ("SVGMarkerElement", "orientType") => simple!("orient", Kind::OrientType),
        ("SVGMarkerElement", "orientAngle") => simple!("orient", Kind::OrientAngle),
        ("SVGGradientElement", "gradientUnits") => en!("gradientUnits", UNITS, 2),
        ("SVGGradientElement", "gradientTransform") => {
            simple!("gradientTransform", Kind::TransformList)
        }
        ("SVGGradientElement", "spreadMethod") => {
            en!("spreadMethod", &["pad", "reflect", "repeat"], 1)
        }
        ("SVGLinearGradientElement", "x1") => len!("x1", Horizontal, "0%"),
        ("SVGLinearGradientElement", "y1") => len!("y1", Vertical, "0%"),
        ("SVGLinearGradientElement", "x2") => len!("x2", Horizontal, "100%"),
        ("SVGLinearGradientElement", "y2") => len!("y2", Vertical, "0%"),
        ("SVGRadialGradientElement", "cx") => len!("cx", Horizontal, "50%"),
        ("SVGRadialGradientElement", "cy") => len!("cy", Vertical, "50%"),
        ("SVGRadialGradientElement", "r") => len!("r", Other, "50%"),
        ("SVGRadialGradientElement", "fx") => len!("fx", Horizontal, "50%"),
        ("SVGRadialGradientElement", "fy") => len!("fy", Vertical, "50%"),
        ("SVGRadialGradientElement", "fr") => len!("fr", Other, "0%"),
        ("SVGStopElement", "offset") => num!("offset", 0.0),
        ("SVGPatternElement", "patternUnits") => en!("patternUnits", UNITS, 2),
        ("SVGPatternElement", "patternContentUnits") => en!("patternContentUnits", UNITS, 1),
        ("SVGPatternElement", "patternTransform") => {
            simple!("patternTransform", Kind::TransformList)
        }
        ("SVGAElement", "target") => simple!("target", Kind::String),
        ("SVGClipPathElement", "clipPathUnits") => en!("clipPathUnits", UNITS, 1),
        ("SVGMaskElement", "maskUnits") => en!("maskUnits", UNITS, 2),
        ("SVGMaskElement", "maskContentUnits") => en!("maskContentUnits", UNITS, 1),
        ("SVGMaskElement" | "SVGFilterElement", "x") => len!("x", Horizontal, "-10%"),
        ("SVGMaskElement" | "SVGFilterElement", "y") => len!("y", Vertical, "-10%"),
        ("SVGMaskElement" | "SVGFilterElement", "width") => len!("width", Horizontal, "120%"),
        ("SVGMaskElement" | "SVGFilterElement", "height") => len!("height", Vertical, "120%"),
        ("SVGFilterElement", "filterUnits") => en!("filterUnits", UNITS, 2),
        ("SVGFilterElement", "primitiveUnits") => en!("primitiveUnits", UNITS, 1),
        ("SVGFilterPrimitiveStandardAttributes", "x") => len!("x", Horizontal, "0%"),
        ("SVGFilterPrimitiveStandardAttributes", "y") => len!("y", Vertical, "0%"),
        ("SVGFilterPrimitiveStandardAttributes", "width") => len!("width", Horizontal, "100%"),
        ("SVGFilterPrimitiveStandardAttributes", "height") => len!("height", Vertical, "100%"),
        ("SVGFilterPrimitiveStandardAttributes", "result") => simple!("result", Kind::String),
        (_, "in1") => simple!("in", Kind::String),
        (_, "in2") => simple!("in2", Kind::String),
        ("SVGFEBlendElement", "mode") => en!("mode", BLEND_MODES, 1),
        ("SVGFEColorMatrixElement", "type") => en!(
            "type",
            &["matrix", "saturate", "hueRotate", "luminanceToAlpha"],
            1
        ),
        ("SVGFEColorMatrixElement", "values") => simple!("values", Kind::NumberList),
        ("SVGComponentTransferFunctionElement", "type") => en!(
            "type",
            &["identity", "table", "discrete", "linear", "gamma"],
            1
        ),
        ("SVGComponentTransferFunctionElement", "tableValues") => {
            simple!("tableValues", Kind::NumberList)
        }
        ("SVGComponentTransferFunctionElement", "slope") => num!("slope", 1.0),
        ("SVGComponentTransferFunctionElement", "intercept") => num!("intercept", 0.0),
        ("SVGComponentTransferFunctionElement", "amplitude") => num!("amplitude", 1.0),
        ("SVGComponentTransferFunctionElement", "exponent") => num!("exponent", 1.0),
        ("SVGComponentTransferFunctionElement", "offset") => num!("offset", 0.0),
        ("SVGFECompositeElement", "operator") => en!(
            "operator",
            &["over", "in", "out", "atop", "xor", "arithmetic"],
            1
        ),
        ("SVGFECompositeElement", "k1") => num!("k1", 0.0),
        ("SVGFECompositeElement", "k2") => num!("k2", 0.0),
        ("SVGFECompositeElement", "k3") => num!("k3", 0.0),
        ("SVGFECompositeElement", "k4") => num!("k4", 0.0),
        ("SVGFEConvolveMatrixElement", "orderX") => simple!(
            "order",
            Kind::IntegerPart {
                second: false,
                initial: 3
            }
        ),
        ("SVGFEConvolveMatrixElement", "orderY") => simple!(
            "order",
            Kind::IntegerPart {
                second: true,
                initial: 3
            }
        ),
        ("SVGFEConvolveMatrixElement", "kernelMatrix") => {
            simple!("kernelMatrix", Kind::NumberList)
        }
        ("SVGFEConvolveMatrixElement", "divisor") => num!("divisor", 1.0),
        ("SVGFEConvolveMatrixElement", "bias") => num!("bias", 0.0),
        ("SVGFEConvolveMatrixElement", "targetX") => {
            simple!("targetX", Kind::Integer { initial: 0 })
        }
        ("SVGFEConvolveMatrixElement", "targetY") => {
            simple!("targetY", Kind::Integer { initial: 0 })
        }
        ("SVGFEConvolveMatrixElement", "edgeMode") => en!("edgeMode", EDGE_MODES, 1),
        ("SVGFEConvolveMatrixElement", "preserveAlpha") => {
            simple!("preserveAlpha", Kind::Boolean)
        }
        (_, "kernelUnitLengthX") => part!("kernelUnitLength", false, 0.0),
        (_, "kernelUnitLengthY") => part!("kernelUnitLength", true, 0.0),
        (_, "surfaceScale") => num!("surfaceScale", 1.0),
        ("SVGFEDiffuseLightingElement", "diffuseConstant") => num!("diffuseConstant", 1.0),
        ("SVGFESpecularLightingElement", "specularConstant") => num!("specularConstant", 1.0),
        (_, "specularExponent") => num!("specularExponent", 1.0),
        ("SVGFEDistantLightElement", "azimuth") => num!("azimuth", 0.0),
        ("SVGFEDistantLightElement", "elevation") => num!("elevation", 0.0),
        ("SVGFEPointLightElement" | "SVGFESpotLightElement", "x") => num!("x", 0.0),
        ("SVGFEPointLightElement" | "SVGFESpotLightElement", "y") => num!("y", 0.0),
        ("SVGFEPointLightElement" | "SVGFESpotLightElement", "z") => num!("z", 0.0),
        ("SVGFESpotLightElement", "pointsAtX") => num!("pointsAtX", 0.0),
        ("SVGFESpotLightElement", "pointsAtY") => num!("pointsAtY", 0.0),
        ("SVGFESpotLightElement", "pointsAtZ") => num!("pointsAtZ", 0.0),
        ("SVGFESpotLightElement", "limitingConeAngle") => num!("limitingConeAngle", 0.0),
        ("SVGFEDisplacementMapElement", "scale") => num!("scale", 0.0),
        ("SVGFEDisplacementMapElement", "xChannelSelector") => {
            en!("xChannelSelector", CHANNELS, 4)
        }
        ("SVGFEDisplacementMapElement", "yChannelSelector") => {
            en!("yChannelSelector", CHANNELS, 4)
        }
        ("SVGFEDropShadowElement", "dx") => num!("dx", 2.0),
        ("SVGFEDropShadowElement", "dy") => num!("dy", 2.0),
        ("SVGFEDropShadowElement", "stdDeviationX") => part!("stdDeviation", false, 2.0),
        ("SVGFEDropShadowElement", "stdDeviationY") => part!("stdDeviation", true, 2.0),
        ("SVGFEGaussianBlurElement", "stdDeviationX") => part!("stdDeviation", false, 0.0),
        ("SVGFEGaussianBlurElement", "stdDeviationY") => part!("stdDeviation", true, 0.0),
        ("SVGFEGaussianBlurElement", "edgeMode") => en!("edgeMode", EDGE_MODES, 3),
        ("SVGFEImageElement", "crossOrigin") => simple!("crossorigin", Kind::String),
        ("SVGFEMorphologyElement", "operator") => en!("operator", &["erode", "dilate"], 1),
        ("SVGFEMorphologyElement", "radiusX") => part!("radius", false, 0.0),
        ("SVGFEMorphologyElement", "radiusY") => part!("radius", true, 0.0),
        ("SVGFEOffsetElement", "dx") => num!("dx", 0.0),
        ("SVGFEOffsetElement", "dy") => num!("dy", 0.0),
        ("SVGFETurbulenceElement", "baseFrequencyX") => part!("baseFrequency", false, 0.0),
        ("SVGFETurbulenceElement", "baseFrequencyY") => part!("baseFrequency", true, 0.0),
        ("SVGFETurbulenceElement", "numOctaves") => {
            simple!("numOctaves", Kind::Integer { initial: 1 })
        }
        ("SVGFETurbulenceElement", "seed") => num!("seed", 0.0),
        ("SVGFETurbulenceElement", "stitchTiles") => {
            en!("stitchTiles", &["stitch", "noStitch"], 2)
        }
        ("SVGFETurbulenceElement", "type") => en!("type", &["fractalNoise", "turbulence"], 2),
        _ => return None,
    })
}
