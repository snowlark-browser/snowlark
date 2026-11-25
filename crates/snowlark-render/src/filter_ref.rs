// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `filter: url(#id)` (Filter Effects 1 §5-§9): a `<filter>` element read into a [`FilterRef`],
//! which [`FilterRef::resolve`] turns into display-list data for the box it filters.
//!
//! The filter is read from the DOM: the `<filter>`'s attributes (region and units) and those of its
//! primitive children, with the primitives' `flood-color`, `flood-opacity`, `lighting-color` and
//! `color-interpolation-filters` from their computed style. Lengths are kept as written (a number
//! or a percentage) until the reference box is known: the filtered element's border box, which is
//! also the origin of its user space and the viewport its user-space percentages resolve against.

use std::collections::HashMap;

use snowlark_dom::{ns, Dom, LocalName, NodeId};
use snowlark_layout::svg::{AspectRatio, ViewBox};
use snowlark_net::Url;
use snowlark_paint::svg_filter::{
    hue_rotate_matrix, saturate_matrix, Channel, CompositeOp, EdgeMode, Input, Kind, Light,
    Primitive, SvgFilter, Transfer, TransferKind, LUMINANCE_TO_ALPHA, MAX_MERGE_INPUTS,
    MAX_OCTAVES, MAX_ORDER, MAX_PRIMITIVES, MAX_TABLE,
};
use snowlark_paint::{Color, ImageId, Rect};
use style::context::{QuirksMode, TreeCountingCaches};
use style::device::Device;
use style::dom::DummyElementContext;
use style::parser::{Parse, ParserContext};
use style::properties::{ComputedValues, StyleBuilder};
use style::rule_cache::RuleCacheConditions;
use style::rule_tree::RuleCascadeFlags;
use style::servo_arc::Arc as ServoArc;
use style::stylesheets::container_rule::ContainerSizeQuery;
use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
use style::values::computed::{Context, Length, LengthPercentage, ToComputedValue};
use style::values::specified;
use style_traits::ParsingMode;

/// The `<filter>` element `id` names for `user`: in `user`'s tree, then the document's.
pub(crate) fn find_filter(dom: &Dom, user: NodeId, id: &str) -> Option<NodeId> {
    let named = |root: NodeId| {
        dom.descendants(root)
            .find(|&n| dom.is_element(n) && dom.attr(n, &ns!(), &LocalName::from("id")) == Some(id))
    };
    let root = dom.root(user);
    let found = named(root).or_else(|| {
        let doc = dom.node_document(user);
        (doc != root).then(|| named(doc)).flatten()
    })?;
    let e = dom.element(found)?;
    (e.name().ns == ns!(svg) && &*e.name().local == "filter").then_some(found)
}

/// A length as written: a number (user units, or a fraction of the reference box), a percentage, or
/// math that mixes them (CSS Values 4 §10, `calc(25% + 0.25px)`), kept until its basis is known.
#[derive(Clone, Debug, PartialEq)]
enum Len {
    Num(f32),
    Percent(f32),
    Calc(LengthPercentage),
}

impl Len {
    /// The length along an axis of `size`: in bounding box units a fraction of `size` (math with
    /// a percentage taken against 1, then scaled, as a percentage is a fraction too); else user
    /// units, a percentage of `size` as the viewport.
    fn along(&self, size: f32, bbox_units: bool) -> f32 {
        match (self, bbox_units) {
            (Len::Num(v), true) => v * size,
            (Len::Percent(p), _) => p / 100.0 * size,
            (Len::Num(v), false) => *v,
            (Len::Calc(c), true) => c.resolve(Length::new(1.0)).px() * size,
            (Len::Calc(c), false) => c.resolve(Length::new(size)).px(),
        }
    }
}

/// What reading lengths needs from the document: its device and quirks mode (font-relative,
/// viewport and absolute units, math functions) and its base URL (a style sheet's context).
pub(crate) struct Lengths<'a> {
    pub device: &'a Device,
    pub quirks: QuirksMode,
    pub base: &'a Url,
}

impl Lengths<'_> {
    /// A length attribute of an element whose computed style is `values` (SVG 2 §4.6.2: a CSS
    /// `<length-percentage>`, or a number of user units): a number, a percentage, or any length
    /// and math as CSS computes it for the element.
    fn parse(&self, v: &str, values: Option<&ComputedValues>) -> Option<Len> {
        let v = v.trim();
        if let Some(p) = v.strip_suffix('%').and_then(number) {
            return Some(Len::Percent(p));
        }
        if let Some(n) = number(v.strip_suffix("px").unwrap_or(v)) {
            return Some(Len::Num(n));
        }
        let url = UrlExtraData::from(self.base.clone());
        let context = ParserContext::new(
            Origin::Author,
            &url,
            Some(CssRuleType::Style),
            ParsingMode::ALLOW_UNITLESS_LENGTH,
            self.quirks,
            Default::default(),
            None,
            None,
            Default::default(),
        );
        let mut parser = cssparser::Parser::new(v);
        let specified = parser
            .parse_entirely(|p| specified::LengthPercentage::parse(&context, p))
            .ok()?;
        // (computed as for a property of the element: what it inherits gives its font size)
        let mut conditions = RuleCacheConditions::default();
        let mut caches = TreeCountingCaches::default();
        let builder = StyleBuilder::for_inheritance(self.device, None, values, None);
        let cx = Context::new(
            builder,
            self.quirks,
            &mut conditions,
            ContainerSizeQuery::none(),
            RuleCascadeFlags::empty(),
            &DummyElementContext,
            &mut caches,
        );
        let computed = specified.to_computed_value(&cx);
        if let Some(l) = computed.to_length() {
            return Some(Len::Num(l.px()).filter_finite());
        }
        if let Some(p) = computed.to_percentage() {
            return Some(Len::Percent(p.0 * 100.0).filter_finite());
        }
        Some(Len::Calc(computed))
    }
}

impl Len {
    /// A non-finite value (an infinite `calc()`) as 0.
    fn filter_finite(self) -> Len {
        match self {
            Len::Num(v) if !v.is_finite() => Len::Num(0.0),
            Len::Percent(v) if !v.is_finite() => Len::Num(0.0),
            other => other,
        }
    }
}

/// A primitive as read: its kind with lengths in primitive units, its subregion's attributes.
#[derive(Clone, Debug, PartialEq)]
struct RawPrimitive {
    kind: Kind,
    inputs: Vec<Input>,
    /// `x`, `y`, `width`, `height`, where given.
    sub: [Option<Len>; 4],
    linear_rgb: bool,
    /// An `feImage`'s image (its `kind` is then transparent black, what it shows without one).
    image: Option<RawImage>,
}

/// What an `feImage`'s `href` refers to (§9.15).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ImageHref {
    /// An SVG element of the filter's document: drawn as `use` would draw it.
    Element(NodeId),
    /// An image resource (an absolute URL).
    Url(String),
}

/// What an `feImage` draws, as the renderer has it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ImageSource {
    /// An image with natural dimensions (CSS px), placed in the subregion by `preserveAspectRatio`.
    Sized {
        image: ImageId,
        width: f32,
        height: f32,
    },
    /// An image drawn over the whole subregion, its origin at the subregion's corner (an element's
    /// rendering, an SVG image without natural dimensions).
    Fill(ImageId),
}

#[derive(Clone, Debug, PartialEq)]
struct RawImage {
    href: Option<ImageHref>,
    aspect: AspectRatio,
    /// `None` until the renderer has the image (or when it has none to show).
    source: Option<ImageSource>,
}

/// A `<filter>` element as read, before the reference box is known.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FilterRef {
    /// `filterUnits="userSpaceOnUse"` (else `objectBoundingBox`, the default).
    user_space: bool,
    /// `primitiveUnits="objectBoundingBox"` (else `userSpaceOnUse`, the default).
    primitive_bbox: bool,
    /// `x`, `y`, `width`, `height`.
    region: [Len; 4],
    primitives: Vec<RawPrimitive>,
}

fn svg_name(dom: &Dom, n: NodeId) -> Option<&str> {
    let e = dom.element(n)?;
    (e.name().ns == ns!(svg)).then(|| &*e.name().local)
}

fn attr<'a>(dom: &'a Dom, n: NodeId, name: &str) -> Option<&'a str> {
    dom.attr(n, &ns!(), &LocalName::from(name)).map(str::trim)
}

fn number(v: &str) -> Option<f32> {
    v.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

/// Numbers separated by whitespace and/or a comma.
fn numbers(v: &str) -> Option<Vec<f32>> {
    v.split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(number)
        .collect()
}

/// `<number-optional-number>`: one number for both, or two.
fn number_pair(v: Option<&str>, default: f32) -> (f32, f32) {
    match v.and_then(numbers).as_deref() {
        Some([a]) => (*a, *a),
        Some([a, b]) => (*a, *b),
        _ => (default, default),
    }
}

fn num_attr(dom: &Dom, n: NodeId, name: &str, default: f32) -> f32 {
    attr(dom, n, name).and_then(number).unwrap_or(default)
}

/// What the primitive's computed style says, or the initial values without one.
struct PrimitiveStyle {
    flood: Color,
    lighting: Color,
    linear_rgb: bool,
}

fn primitive_style(values: Option<&ComputedValues>) -> PrimitiveStyle {
    let Some(v) = values else {
        return PrimitiveStyle {
            flood: Color::rgba(0, 0, 0, 255),
            lighting: Color::rgba(255, 255, 255, 255),
            linear_rgb: true,
        };
    };
    use style::properties::longhands::color_interpolation_filters::computed_value::T as Space;
    let svg = v.get_svg();
    let mut flood = crate::adapter::to_color(&v.resolve_color(&svg.flood_color));
    flood.a = (flood.a as f32 * svg.flood_opacity.clamp(0.0, 1.0)).round() as u8;
    PrimitiveStyle {
        flood,
        lighting: crate::adapter::to_color(&v.resolve_color(&svg.lighting_color)),
        // (`auto` takes sRGB)
        linear_rgb: v.get_inherited_svg().color_interpolation_filters == Space::Linearrgb,
    }
}

/// What `feImage` `fe` of `filter` refers to (§9.15: `href`, else `xlink:href`, against `base`):
/// a local reference (a fragment of the document's own URL) names an SVG element of the filter's
/// tree, or nothing (no element, not an SVG element, or one that contains the filter: transparent
/// black); any other URL is an image to fetch.
fn image_href(dom: &Dom, filter: NodeId, fe: NodeId, base: &Url) -> Option<ImageHref> {
    let href = dom
        .attr(fe, &ns!(), &LocalName::from("href"))
        .or_else(|| dom.attr(fe, &ns!(xlink), &LocalName::from("href")))?
        .trim();
    if href.is_empty() {
        return None;
    }
    let local = match href.strip_prefix('#') {
        Some(id) => Some(id.to_owned()),
        None => {
            let url = base.join(href).ok()?;
            let document = Url::parse(dom.document_url(dom.node_document(fe))).ok();
            let same = document.is_some_and(|mut d| {
                let mut u = url.clone();
                d.set_fragment(None);
                u.set_fragment(None);
                d == u
            });
            if !same {
                return Some(ImageHref::Url(url.to_string()));
            }
            url.fragment()
                .map(snowlark_layout::svg::parse::percent_decoded)
        }
    };
    let id = local.filter(|id| !id.is_empty())?;
    let found = dom.descendants(dom.root(filter)).find(|&e| {
        dom.is_element(e) && dom.attr(e, &ns!(), &LocalName::from("id")) == Some(&*id)
    })?;
    (svg_name(dom, found).is_some() && !dom.is_inclusive_ancestor_of(found, filter))
        .then_some(ImageHref::Element(found))
}

/// Reads the `<filter>` element `filter`; `styles` gives an element's computed style, `lengths`
/// reads lengths in its document (and has its base URL).
pub(crate) fn read(
    dom: &Dom,
    filter: NodeId,
    styles: &dyn Fn(NodeId) -> Option<ServoArc<ComputedValues>>,
    lengths: &Lengths<'_>,
) -> FilterRef {
    let base = lengths.base;
    let units = |name: &str| attr(dom, filter, name);
    let filter_style = styles(filter);
    let region_len = |name: &str, default: Len| {
        attr(dom, filter, name)
            .and_then(|v| lengths.parse(v, filter_style.as_deref()))
            .unwrap_or(default)
    };
    let mut out = FilterRef {
        user_space: units("filterUnits") == Some("userSpaceOnUse"),
        primitive_bbox: units("primitiveUnits") == Some("objectBoundingBox"),
        region: [
            region_len("x", Len::Percent(-10.0)),
            region_len("y", Len::Percent(-10.0)),
            region_len("width", Len::Percent(120.0)),
            region_len("height", Len::Percent(120.0)),
        ],
        primitives: Vec::new(),
    };
    // (an element without a computed style, in a `display: none` subtree: its
    // `color-interpolation-filters` attribute, else its parent's value; `auto` takes sRGB)
    let space = |n: NodeId, parent: bool| match attr(dom, n, "color-interpolation-filters") {
        Some("linearRGB") => true,
        Some("sRGB") | Some("auto") => false,
        _ => parent,
    };
    let filter_linear = match filter_style.as_deref() {
        Some(v) => primitive_style(Some(v)).linear_rgb,
        None => space(filter, false),
    };
    let mut named: HashMap<String, u16> = HashMap::new();
    for n in dom.element_children(filter) {
        if out.primitives.len() >= MAX_PRIMITIVES {
            break;
        }
        let Some(name) = svg_name(dom, n) else {
            continue;
        };
        let index = out.primitives.len();
        let input = |a: &str| -> Input {
            match attr(dom, n, a).unwrap_or("") {
                "SourceGraphic" => Input::SourceGraphic,
                "SourceAlpha" => Input::SourceAlpha,
                "BackgroundImage" | "BackgroundAlpha" | "FillPaint" | "StrokePaint" => {
                    Input::Transparent
                }
                // (a name no earlier primitive has as its result, or none: the previous
                // primitive's result, or the source for the first)
                other => named
                    .get(other)
                    .copied()
                    .map(Input::Result)
                    .unwrap_or(match index {
                        0 => Input::SourceGraphic,
                        i => Input::Result(i as u16 - 1),
                    }),
            }
        };
        let values = styles(n);
        let style = values.as_deref().map_or(
            PrimitiveStyle {
                linear_rgb: space(n, filter_linear),
                ..primitive_style(None)
            },
            |v| primitive_style(Some(v)),
        );
        let in1 = || vec![input("in")];
        let in2 = || vec![input("in"), input("in2")];
        // (a primitive with parameters in error shows transparent black)
        let error = (Kind::Flood(Color::rgba(0, 0, 0, 0)), Vec::new());
        let (kind, inputs) =
            match name {
                "feFlood" => (Kind::Flood(style.flood), Vec::new()),
                "feColorMatrix" => {
                    let values = attr(dom, n, "values");
                    let m = match attr(dom, n, "type").unwrap_or("matrix") {
                        "saturate" => values.map_or(Some(1.0), number).map(saturate_matrix),
                        "hueRotate" => values.map_or(Some(0.0), number).map(hue_rotate_matrix),
                        "luminanceToAlpha" => Some(LUMINANCE_TO_ALPHA),
                        _ => match values.map(numbers) {
                            None => Some(IDENTITY),
                            Some(Some(v)) if v.len() == 20 => Some(std::array::from_fn(|r| {
                                std::array::from_fn(|c| v[r * 5 + c])
                            })),
                            // (a wrong count of values: the identity)
                            Some(_) => Some(IDENTITY),
                        },
                    };
                    (Kind::ColorMatrix(m.unwrap_or(IDENTITY)), in1())
                }
                "feComponentTransfer" => {
                    let mut funcs: [Transfer; 4] = Default::default();
                    for f in dom.element_children(n) {
                        let i = match svg_name(dom, f) {
                            Some("feFuncR") => 0,
                            Some("feFuncG") => 1,
                            Some("feFuncB") => 2,
                            Some("feFuncA") => 3,
                            _ => continue,
                        };
                        let kind = match attr(dom, f, "type").unwrap_or("") {
                            "table" => TransferKind::Table,
                            "discrete" => TransferKind::Discrete,
                            "linear" => TransferKind::Linear,
                            "gamma" => TransferKind::Gamma,
                            _ => TransferKind::Identity,
                        };
                        let mut table = attr(dom, f, "tableValues")
                            .and_then(numbers)
                            .unwrap_or_default();
                        table.truncate(MAX_TABLE);
                        funcs[i] = Transfer {
                            kind,
                            table,
                            slope: num_attr(dom, f, "slope", 1.0),
                            intercept: num_attr(dom, f, "intercept", 0.0),
                            amplitude: num_attr(dom, f, "amplitude", 1.0),
                            exponent: num_attr(dom, f, "exponent", 1.0),
                            offset: num_attr(dom, f, "offset", 0.0),
                        };
                    }
                    (Kind::ComponentTransfer(Box::new(funcs)), in1())
                }
                "feGaussianBlur" => {
                    // (a negative deviation is taken as 0)
                    let (x, y) = number_pair(attr(dom, n, "stdDeviation"), 0.0);
                    (
                        Kind::GaussianBlur {
                            x: x.max(0.0),
                            y: y.max(0.0),
                        },
                        in1(),
                    )
                }
                "feOffset" => (
                    Kind::Offset {
                        dx: num_attr(dom, n, "dx", 0.0),
                        dy: num_attr(dom, n, "dy", 0.0),
                    },
                    in1(),
                ),
                "feMerge" => {
                    let mut inputs = Vec::new();
                    for m in dom.element_children(n) {
                        if svg_name(dom, m) != Some("feMergeNode")
                            || inputs.len() >= MAX_MERGE_INPUTS
                        {
                            continue;
                        }
                        inputs.push(match attr(dom, m, "in").unwrap_or("") {
                            "SourceGraphic" => Input::SourceGraphic,
                            "SourceAlpha" => Input::SourceAlpha,
                            "BackgroundImage" | "BackgroundAlpha" | "FillPaint" | "StrokePaint" => {
                                Input::Transparent
                            }
                            other => named.get(other).copied().map(Input::Result).unwrap_or(
                                match index {
                                    0 => Input::SourceGraphic,
                                    i => Input::Result(i as u16 - 1),
                                },
                            ),
                        });
                    }
                    (Kind::Merge, inputs)
                }
                "feBlend" => {
                    const MODES: [&str; 16] = [
                        "normal",
                        "multiply",
                        "screen",
                        "overlay",
                        "darken",
                        "lighten",
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
                    let mode = attr(dom, n, "mode").unwrap_or("normal");
                    let mode = MODES.iter().position(|m| *m == mode).unwrap_or(0);
                    (Kind::Blend(mode as u8), in2())
                }
                "feComposite" => {
                    let op = match attr(dom, n, "operator").unwrap_or("over") {
                        "in" => CompositeOp::In,
                        "out" => CompositeOp::Out,
                        "atop" => CompositeOp::Atop,
                        "xor" => CompositeOp::Xor,
                        "lighter" => CompositeOp::Lighter,
                        "arithmetic" => CompositeOp::Arithmetic,
                        _ => CompositeOp::Over,
                    };
                    let k = ["k1", "k2", "k3", "k4"].map(|k| num_attr(dom, n, k, 0.0));
                    (Kind::Composite { op, k }, in2())
                }
                "feDropShadow" => {
                    let (x, y) = number_pair(attr(dom, n, "stdDeviation"), 2.0);
                    (
                        Kind::DropShadow {
                            dx: num_attr(dom, n, "dx", 2.0),
                            dy: num_attr(dom, n, "dy", 2.0),
                            x: x.max(0.0),
                            y: y.max(0.0),
                            color: style.flood,
                        },
                        in1(),
                    )
                }
                "feMorphology" => {
                    let (rx, ry) = number_pair(attr(dom, n, "radius"), 0.0);
                    (
                        Kind::Morphology {
                            dilate: attr(dom, n, "operator") == Some("dilate"),
                            rx: rx.max(0.0),
                            ry: ry.max(0.0),
                        },
                        in1(),
                    )
                }
                "feTurbulence" => {
                    // (a negative frequency is in error: the default, 0)
                    let (bx, by) = number_pair(attr(dom, n, "baseFrequency"), 0.0);
                    let (bx, by) = if bx < 0.0 || by < 0.0 {
                        (0.0, 0.0)
                    } else {
                        (bx, by)
                    };
                    (
                        Kind::Turbulence {
                            base_x: bx,
                            base_y: by,
                            octaves: (num_attr(dom, n, "numOctaves", 1.0).max(0.0) as u32)
                                .min(MAX_OCTAVES),
                            seed: num_attr(dom, n, "seed", 0.0),
                            stitch: attr(dom, n, "stitchTiles") == Some("stitch"),
                            fractal_noise: attr(dom, n, "type") == Some("fractalNoise"),
                        },
                        Vec::new(),
                    )
                }
                "feTile" => (Kind::Tile, in1()),
                "feConvolveMatrix" => convolve(dom, n).map_or(error, |k| (k, in1())),
                "feDisplacementMap" => {
                    let channel = |a: &str| match attr(dom, n, a).unwrap_or("A") {
                        "R" => Channel::R,
                        "G" => Channel::G,
                        "B" => Channel::B,
                        _ => Channel::A,
                    };
                    (
                        Kind::DisplacementMap {
                            scale: num_attr(dom, n, "scale", 0.0),
                            x: channel("xChannelSelector"),
                            y: channel("yChannelSelector"),
                        },
                        in2(),
                    )
                }
                "feDiffuseLighting" | "feSpecularLighting" => match light(dom, n) {
                    None => error,
                    Some(light) => {
                        let surface_scale = num_attr(dom, n, "surfaceScale", 1.0);
                        let kind = if name == "feDiffuseLighting" {
                            Kind::DiffuseLighting {
                                surface_scale,
                                constant: num_attr(dom, n, "diffuseConstant", 1.0),
                                color: style.lighting,
                                light,
                            }
                        } else {
                            Kind::SpecularLighting {
                                surface_scale,
                                constant: num_attr(dom, n, "specularConstant", 1.0),
                                exponent: num_attr(dom, n, "specularExponent", 1.0),
                                color: style.lighting,
                                light,
                            }
                        };
                        let negative = match &kind {
                            Kind::DiffuseLighting { constant, .. }
                            | Kind::SpecularLighting { constant, .. } => *constant < 0.0,
                            _ => false,
                        };
                        if negative {
                            error
                        } else {
                            (kind, in1())
                        }
                    }
                },
                "feImage" => error,
                _ => continue,
            };
        let image = (name == "feImage").then(|| RawImage {
            href: image_href(dom, filter, n, base),
            aspect: attr(dom, n, "preserveAspectRatio")
                .and_then(AspectRatio::parse)
                .unwrap_or_default(),
            source: None,
        });
        let sub = ["x", "y", "width", "height"]
            .map(|a| attr(dom, n, a).and_then(|v| lengths.parse(v, values.as_deref())));
        out.primitives.push(RawPrimitive {
            kind,
            inputs,
            sub,
            linear_rgb: style.linear_rgb,
            image,
        });
        if let Some(result) = attr(dom, n, "result").filter(|r| !r.is_empty()) {
            named.insert(result.to_owned(), index as u16);
        }
    }
    out
}

const IDENTITY: [[f32; 5]; 4] = [
    [1.0, 0.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 0.0, 1.0, 0.0],
];

/// `feConvolveMatrix`'s parameters, or `None` when they are in error (§9.9).
fn convolve(dom: &Dom, n: NodeId) -> Option<Kind> {
    let (ox, oy) = number_pair(attr(dom, n, "order"), 3.0);
    if ox < 1.0 || oy < 1.0 || ox.fract() != 0.0 || oy.fract() != 0.0 {
        return None;
    }
    let (order_x, order_y) = (ox as u32, oy as u32);
    if order_x > MAX_ORDER || order_y > MAX_ORDER {
        return None;
    }
    let kernel = attr(dom, n, "kernelMatrix").and_then(numbers)?;
    if kernel.len() != (order_x * order_y) as usize {
        return None;
    }
    let sum: f32 = kernel.iter().sum();
    let divisor = match attr(dom, n, "divisor").and_then(number) {
        Some(0.0) => return None,
        Some(d) => d,
        None if sum == 0.0 => 1.0,
        None => sum,
    };
    let target = |a: &str, order: u32| -> Option<u32> {
        match attr(dom, n, a).and_then(number) {
            None => Some(order / 2),
            Some(t) if t >= 0.0 && (t as u32) < order && t.fract() == 0.0 => Some(t as u32),
            Some(_) => None,
        }
    };
    Some(Kind::ConvolveMatrix {
        order_x,
        order_y,
        kernel,
        divisor,
        bias: num_attr(dom, n, "bias", 0.0),
        target_x: target("targetX", order_x)?,
        target_y: target("targetY", order_y)?,
        edge: match attr(dom, n, "edgeMode").unwrap_or("duplicate") {
            "wrap" => EdgeMode::Wrap,
            "none" => EdgeMode::None,
            "mirror" => EdgeMode::Mirror,
            _ => EdgeMode::Duplicate,
        },
        preserve_alpha: attr(dom, n, "preserveAlpha") == Some("true"),
    })
}

/// A lighting primitive's light source: its first light source child.
fn light(dom: &Dom, n: NodeId) -> Option<Light> {
    dom.element_children(n).find_map(|l| {
        let a = |name: &str| num_attr(dom, l, name, 0.0);
        Some(match svg_name(dom, l)? {
            "feDistantLight" => Light::Distant {
                azimuth: a("azimuth"),
                elevation: a("elevation"),
            },
            "fePointLight" => Light::Point {
                x: a("x"),
                y: a("y"),
                z: a("z"),
            },
            "feSpotLight" => Light::Spot {
                x: a("x"),
                y: a("y"),
                z: a("z"),
                at_x: a("pointsAtX"),
                at_y: a("pointsAtY"),
                at_z: a("pointsAtZ"),
                exponent: num_attr(dom, l, "specularExponent", 1.0),
                cone: attr(dom, l, "limitingConeAngle").and_then(number),
            },
            _ => return None,
        })
    })
}

impl FilterRef {
    /// What each `feImage` refers to, by the primitive's index.
    pub(crate) fn image_hrefs(&self) -> impl Iterator<Item = (usize, &ImageHref)> {
        self.primitives
            .iter()
            .enumerate()
            .filter_map(|(i, p)| Some((i, p.image.as_ref()?.href.as_ref()?)))
    }

    /// Gives `feImage` `index` what it draws.
    pub(crate) fn set_image_source(&mut self, index: usize, source: Option<ImageSource>) {
        if let Some(image) = self
            .primitives
            .get_mut(index)
            .and_then(|p| p.image.as_mut())
        {
            image.source = source;
        }
    }

    /// The filter for a box whose border box is `bbox` (in the list's coordinates).
    pub(crate) fn resolve(&self, bbox: Rect) -> SvgFilter {
        // (a length along an axis: a fraction of the box in bounding box units; else user units,
        // a percentage of the box as the viewport)
        let along = |l: &Len, size: f32, bbox_units: bool| l.along(size, bbox_units);
        let rect = |r: &[Len; 4], bbox_units: bool| {
            Rect::new(
                bbox.x + along(&r[0], bbox.width, bbox_units),
                bbox.y + along(&r[1], bbox.height, bbox_units),
                along(&r[2], bbox.width, bbox_units),
                along(&r[3], bbox.height, bbox_units),
            )
        };
        let region = rect(&self.region, !self.user_space);
        let pb = self.primitive_bbox;
        // (a primitive's numbers in bounding box units are fractions of the box)
        let (sx, sy) = if pb {
            (bbox.width, bbox.height)
        } else {
            (1.0, 1.0)
        };
        let point = |x: f32, y: f32| (bbox.x + x * sx, bbox.y + y * sy);
        let z_scale = if pb {
            ((bbox.width * bbox.width + bbox.height * bbox.height) / 2.0).sqrt()
        } else {
            1.0
        };
        let light = |l: Light| match l {
            Light::Distant { .. } => l,
            Light::Point { x, y, z } => {
                let (x, y) = point(x, y);
                Light::Point {
                    x,
                    y,
                    z: z * z_scale,
                }
            }
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
                let (x, y) = point(x, y);
                let (at_x, at_y) = point(at_x, at_y);
                Light::Spot {
                    x,
                    y,
                    z: z * z_scale,
                    at_x,
                    at_y,
                    at_z: at_z * z_scale,
                    exponent,
                    cone,
                }
            }
        };
        let mut primitives: Vec<Primitive> = Vec::with_capacity(self.primitives.len());
        for p in &self.primitives {
            // §9.4: the union of the inputs' subregions, or the region when an input is a
            // standard one or there is none; then each attribute given replaces its part.
            let mut default: Option<Rect> = None;
            let mut standard = p.inputs.is_empty();
            for i in &p.inputs {
                match i {
                    Input::Result(r) => {
                        let s = primitives[*r as usize].subregion;
                        default = Some(default.map_or(s, |d| d.union(&s)));
                    }
                    _ => standard = true,
                }
            }
            let default = if standard {
                region
            } else {
                default.unwrap_or(region)
            };
            let given = |i: usize, size: f32| p.sub[i].as_ref().map(|l| along(l, size, pb));
            let x = given(0, bbox.width).map_or(default.x, |v| bbox.x + v);
            let y = given(1, bbox.height).map_or(default.y, |v| bbox.y + v);
            let w = given(2, bbox.width).unwrap_or(default.width).max(0.0);
            let h = given(3, bbox.height).unwrap_or(default.height).max(0.0);
            let kind = match p.kind.clone() {
                Kind::GaussianBlur { x, y } => Kind::GaussianBlur {
                    x: x * sx,
                    y: y * sy,
                },
                Kind::Offset { dx, dy } => Kind::Offset {
                    dx: dx * sx,
                    dy: dy * sy,
                },
                Kind::DropShadow {
                    dx,
                    dy,
                    x,
                    y,
                    color,
                } => Kind::DropShadow {
                    dx: dx * sx,
                    dy: dy * sy,
                    x: x * sx,
                    y: y * sy,
                    color,
                },
                Kind::Morphology { dilate, rx, ry } => Kind::Morphology {
                    dilate,
                    rx: rx * sx,
                    ry: ry * sy,
                },
                Kind::Turbulence {
                    base_x,
                    base_y,
                    octaves,
                    seed,
                    stitch,
                    fractal_noise,
                } => Kind::Turbulence {
                    base_x: base_x / sx.max(f32::MIN_POSITIVE),
                    base_y: base_y / sy.max(f32::MIN_POSITIVE),
                    octaves,
                    seed,
                    stitch,
                    fractal_noise,
                },
                Kind::DisplacementMap { scale, x, y } => Kind::DisplacementMap {
                    scale: scale * (sx * sy).sqrt(),
                    x,
                    y,
                },
                Kind::DiffuseLighting {
                    surface_scale,
                    constant,
                    color,
                    light: l,
                } => Kind::DiffuseLighting {
                    surface_scale,
                    constant,
                    color,
                    light: light(l),
                },
                Kind::SpecularLighting {
                    surface_scale,
                    constant,
                    exponent,
                    color,
                    light: l,
                } => Kind::SpecularLighting {
                    surface_scale,
                    constant,
                    exponent,
                    color,
                    light: light(l),
                },
                other => other,
            };
            let subregion = Rect::new(x, y, w, h);
            let kind = match p.image.as_ref().and_then(|i| Some((i.source?, i.aspect))) {
                Some((source, aspect)) => image_kind(source, aspect, subregion).unwrap_or(kind),
                None => kind,
            };
            primitives.push(Primitive {
                kind,
                inputs: p.inputs.clone(),
                subregion,
                linear_rgb: p.linear_rgb,
            });
        }
        SvgFilter { region, primitives }
    }
}

/// An `feImage` drawing `source` in `subregion`: an image with natural dimensions placed by
/// `aspect` as a `viewBox` of its size would be (the subregion cuts what a `slice` leaves over);
/// `None` for an empty image (transparent black).
fn image_kind(source: ImageSource, aspect: AspectRatio, subregion: Rect) -> Option<Kind> {
    let dest = match source {
        ImageSource::Fill(_) => subregion,
        ImageSource::Sized { width, height, .. } => {
            if !(width > 0.0 && height > 0.0) {
                return None;
            }
            let vb = ViewBox {
                x: 0.0,
                y: 0.0,
                width: width as f64,
                height: height as f64,
            };
            let [sx, _, _, sy, tx, ty] = aspect
                .transform(&vb, subregion.width as f64, subregion.height as f64)
                .as_coeffs();
            Rect::new(
                subregion.x + tx as f32,
                subregion.y + ty as f32,
                width * sx as f32,
                height * sy as f32,
            )
        }
    };
    let (ImageSource::Fill(image) | ImageSource::Sized { image, .. }) = source;
    Some(Kind::Image {
        image,
        dest,
        src: Rect::ZERO,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn near(a: Rect, b: Rect) {
        let d = [a.x - b.x, a.y - b.y, a.width - b.width, a.height - b.height];
        assert!(d.iter().all(|d| d.abs() < 1e-3), "{a:?} is not {b:?}");
    }

    fn raw(kind: Kind, inputs: Vec<Input>, sub: [Option<Len>; 4]) -> RawPrimitive {
        RawPrimitive {
            kind,
            inputs,
            sub,
            linear_rgb: true,
            image: None,
        }
    }

    /// An image with natural dimensions is placed by `preserveAspectRatio` (§9.15 with SVG 2
    /// §8.7): centred and fitted by default, stretched with `none`, covering with `slice` (the
    /// subregion cuts the rest); an element's rendering fills the subregion from its corner.
    #[test]
    fn images_are_placed_by_their_aspect_ratio() {
        let sub = Rect::new(10.0, 20.0, 100.0, 50.0);
        let sized = ImageSource::Sized {
            image: ImageId(3),
            width: 20.0,
            height: 20.0,
        };
        let dest = |source, aspect: &str| match image_kind(
            source,
            AspectRatio::parse(aspect).unwrap(),
            sub,
        ) {
            Some(Kind::Image { image, dest, src }) => {
                assert_eq!((image, src), (ImageId(3), Rect::ZERO));
                dest
            }
            other => panic!("{other:?}"),
        };
        near(
            dest(sized, "xMidYMid meet"),
            Rect::new(35.0, 20.0, 50.0, 50.0),
        );
        near(
            dest(sized, "xMinYMax meet"),
            Rect::new(10.0, 20.0, 50.0, 50.0),
        );
        near(dest(sized, "none"), sub);
        near(
            dest(sized, "xMidYMid slice"),
            Rect::new(10.0, -5.0, 100.0, 100.0),
        );
        near(dest(ImageSource::Fill(ImageId(3)), "none"), sub);
        let empty = ImageSource::Sized {
            image: ImageId(3),
            width: 0.0,
            height: 20.0,
        };
        assert_eq!(image_kind(empty, AspectRatio::default(), sub), None);
    }

    /// The default region is the box with 10% around it; a primitive's subregion is the
    /// region, or its inputs' union, with the attributes given replacing their parts.
    #[test]
    fn regions_and_subregions_follow_the_units() {
        let f = FilterRef {
            user_space: false,
            primitive_bbox: false,
            region: [
                Len::Percent(-10.0),
                Len::Percent(-10.0),
                Len::Percent(120.0),
                Len::Percent(120.0),
            ],
            primitives: vec![
                raw(
                    Kind::Flood(Color::rgba(0, 0, 0, 255)),
                    vec![],
                    [Some(Len::Num(10.0)), None, Some(Len::Num(20.0)), None],
                ),
                raw(
                    Kind::Offset { dx: 5.0, dy: 0.0 },
                    vec![Input::Result(0)],
                    Default::default(),
                ),
                raw(
                    Kind::Offset { dx: 5.0, dy: 0.0 },
                    vec![Input::SourceGraphic],
                    [None, Some(Len::Percent(50.0)), None, None],
                ),
            ],
        };
        let r = f.resolve(Rect::new(100.0, 200.0, 100.0, 50.0));
        near(r.region, Rect::new(90.0, 195.0, 120.0, 60.0));
        // (user units are from the box's corner)
        near(
            r.primitives[0].subregion,
            Rect::new(110.0, 195.0, 20.0, 60.0),
        );
        near(r.primitives[1].subregion, r.primitives[0].subregion);
        // (a percentage of the box as the viewport)
        near(
            r.primitives[2].subregion,
            Rect::new(90.0, 225.0, 120.0, 60.0),
        );
    }

    #[test]
    fn bounding_box_primitive_units_scale_the_lengths() {
        let f = FilterRef {
            user_space: true,
            primitive_bbox: true,
            region: [Len::Num(0.0), Len::Num(0.0), Len::Num(50.0), Len::Num(50.0)],
            primitives: vec![raw(
                Kind::GaussianBlur { x: 0.1, y: 0.1 },
                vec![Input::SourceGraphic],
                Default::default(),
            )],
        };
        let r = f.resolve(Rect::new(10.0, 10.0, 100.0, 40.0));
        near(r.region, Rect::new(10.0, 10.0, 50.0, 50.0));
        assert_eq!(r.primitives[0].kind, Kind::GaussianBlur { x: 10.0, y: 4.0 });
        assert!(r.is_valid());
    }
}
