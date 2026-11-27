// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inline `<svg>` in HTML documents, as an interim: an outermost `svg` element is a replaced box
//! (SVG 2 §8.2) whose content is the element's subtree rendered as an SVG image through the existing
//! resvg path ([`crate::svg`]): the subtree is serialized to an SVG document with what the page's
//! style gave each element, the elements it refers to elsewhere in the document (`<use href="#id">`
//! sprites, `url(#id)` paint servers, clip paths, masks, markers) are copied into a `<defs>`, and the
//! image is rasterized at each size it is drawn at (with the SVG image cache).
//!
//! What the page's style gives an element (its `fill`, `stroke`, `color` from `currentColor`, ...)
//! comes from its computed values, which include the SVG presentation attributes (snowlark-style
//! maps them as presentational hints, SVG 2 §6.6): each element gets a `style` attribute with the
//! properties it does not merely inherit from its parent (all of them on the outermost element, which
//! inherits from the HTML around it), so content instantiated by `<use>` still inherits from the
//! `use` element as SVG requires.
//!
//! An `<image>` element's `href` (SVG 2 §5.7: `href`, else `xlink:href`) is part of the document,
//! not of an image in secure static mode: it is resolved against the document's base URL and
//! fetched as the document's image (with `<img>`'s request, so the fetch policies apply);
//! [`Serialized::images`] lists those URLs, and the rasterizer is given the fetched bytes
//! ([`crate::svg::SvgImage::with_images`]).
//!
//! Geometry properties and `transform` are written from computed values, whatever set them (a style
//! rule, an animation, the attribute); animated values of other attributes come from the DOM's side
//! table ([`Dom::animated_attr`]).
//!
//! A `use` of an element of another document ([`crate::svg_resource`]) refers to a copy in the `defs`
//! under ids of its own, written from the resource document's computed values, so that its style
//! sheets apply and only what its rules set stands against what the instance inherits from the `use`.
//!
//! What this does not do (a real SVG layout would): `<foreignObject>`, text selection,
//! `transform-box`, `vector-effect`, and the shadow tree semantics of a same-document `<use>` (a
//! clone shows its original's style, not a cascade of its own).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::OnceLock;

use snowlark_dom::{local_name, ns, Dom, NodeData, NodeId};
use snowlark_net::Url;
use snowlark_style::computed::same_computed_value;
use snowlark_style::{ComputedValues, StyleData};
use style::properties::PropertyId;

use crate::svg_resource::{style_key, SvgResource, CURRENT_COLOR};
use snowlark_layout::svg::parse::percent_decoded;

/// The properties copied from an element's computed values: (name, inherited).
const PROPERTIES: &[(&str, bool)] = &[
    ("color", true),
    ("fill", true),
    ("fill-opacity", true),
    ("fill-rule", true),
    ("stroke", true),
    ("stroke-width", true),
    ("stroke-opacity", true),
    ("stroke-linecap", true),
    ("stroke-linejoin", true),
    ("stroke-miterlimit", true),
    ("stroke-dasharray", true),
    ("stroke-dashoffset", true),
    ("visibility", true),
    ("font-family", true),
    ("font-size", true),
    ("font-weight", true),
    ("font-style", true),
    ("text-anchor", true),
    ("paint-order", true),
    ("letter-spacing", true),
    ("word-spacing", true),
    ("marker-start", true),
    ("marker-mid", true),
    ("marker-end", true),
    ("clip-rule", true),
    ("shape-rendering", true),
    ("text-rendering", true),
    ("dominant-baseline", true),
    ("image-rendering", true),
    ("opacity", false),
    ("mix-blend-mode", false),
    ("isolation", false),
    ("stop-color", false),
    ("stop-opacity", false),
    ("flood-color", false),
    ("flood-opacity", false),
    ("lighting-color", false),
    ("clip-path", false),
    ("filter", false),
    ("mask", false),
    ("mix-blend-mode", false),
];

/// The attributes that are geometry properties or `transform` where they are presentation
/// attributes: what is written is the computed value, whatever set it.
const GEOMETRY: [&str; 13] = [
    "x",
    "y",
    "width",
    "height",
    "cx",
    "cy",
    "r",
    "rx",
    "ry",
    "d",
    "transform",
    "gradientTransform",
    "patternTransform",
];

/// The ids of [`PROPERTIES`], in its order, and of the properties [`GEOMETRY`] attributes map to:
/// an element reads dozens of them and a name's parse costs more than its value.
struct Ids {
    color: Option<PropertyId>,
    copied: Vec<Option<PropertyId>>,
    geometry: HashMap<&'static str, PropertyId>,
}

/// Whether `v` is the initial value of the non-inherited [`PROPERTIES`] entry `name`, as
/// serialized.
fn is_initial_serialization(name: &str, v: &str) -> bool {
    matches!(
        (name, v),
        ("opacity", "1")
            | ("stop-color", "rgb(0, 0, 0)")
            | ("stop-opacity", "1")
            | ("flood-color", "rgb(0, 0, 0)")
            | ("flood-opacity", "1")
            | ("lighting-color", "rgb(255, 255, 255)")
            | ("clip-path", "none")
            | ("filter", "none")
            | ("mask", "none")
            | ("mix-blend-mode", "normal")
            | ("isolation", "auto")
    )
}

fn ids() -> &'static Ids {
    static IDS: OnceLock<Ids> = OnceLock::new();
    IDS.get_or_init(|| Ids {
        color: snowlark_style::computed::property_id("color"),
        copied: PROPERTIES
            .iter()
            .map(|(name, _)| snowlark_style::computed::property_id(name))
            .collect(),
        geometry: [
            "x", "y", "width", "height", "cx", "cy", "r", "rx", "ry", "d",
        ]
        .into_iter()
        .filter_map(|name| Some((name, snowlark_style::computed::property_id(name)?)))
        .collect(),
    })
}

/// Attributes that reference other elements by fragment (`#id` or `url(#id)`).
const REFERENCING: &[&str] = &[
    "href",
    "fill",
    "stroke",
    "clip-path",
    "mask",
    "filter",
    "marker-start",
    "marker-mid",
    "marker-end",
];

/// The most elements copied in from elsewhere for one image (a guard against reference cycles and
/// sprites that pull in a whole page).
const MAX_REFERENCED: usize = 256;

/// Whether `el` is an outermost `svg` element (an SVG `svg` whose parent is not an SVG element).
pub(crate) fn is_outermost_svg(dom: &Dom, el: NodeId) -> bool {
    let Some(e) = dom.element(el) else {
        return false;
    };
    if e.name().ns != ns!(svg) || e.name().local != local_name!("svg") {
        return false;
    }
    !svg_parent(dom, el)
        .and_then(|p| dom.element(p))
        .is_some_and(|p| p.name().ns == ns!(svg))
}

/// The parent element, through a `use` element's shadow root to the `use`.
fn svg_parent(dom: &Dom, n: NodeId) -> Option<NodeId> {
    match dom.parent(n) {
        Some(p) if dom.is_shadow_root(p) => dom.host(p),
        _ => dom.parent_element(n),
    }
}

/// The outermost `svg` element holding `n` (itself included), when `n` is an SVG element.
pub(crate) fn outermost_svg_of(dom: &Dom, n: NodeId) -> Option<NodeId> {
    let mut cur = n;
    loop {
        if dom.element(cur)?.name().ns != ns!(svg) {
            return None;
        }
        if is_outermost_svg(dom, cur) {
            return Some(cur);
        }
        cur = svg_parent(dom, cur)?;
    }
}

/// Whether `n`, in the svg `svg`, is in what other svgs can reference: an element with an `id`
/// (`use`, `url(#...)`), or `defs` or `symbol` content.
pub(crate) fn referable(dom: &Dom, n: NodeId, svg: NodeId) -> bool {
    let mut cur = Some(n);
    while let Some(c) = cur {
        if c == svg {
            return false;
        }
        if let Some(e) = dom.element(c) {
            if matches!(e.name().local, local_name!("defs") | local_name!("symbol"))
                || dom.get_attribute(c, "id").is_some()
            {
                return true;
            }
        }
        cur = dom.parent_element(c);
    }
    false
}

fn escape_into(out: &mut String, s: &str, attr: bool) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// The `#id` fragments a value refers to: `#id` itself or `url(#id)` (quoted or not).
fn fragment_refs(value: &str, out: &mut Vec<String>) {
    let v = value.trim();
    if let Some(id) = v.strip_prefix('#') {
        if !id.is_empty() {
            out.push(id.to_owned());
        }
        return;
    }
    let mut rest = v;
    while let Some(i) = rest.find("url(") {
        rest = &rest[i + 4..];
        let end = rest.find(')').unwrap_or(rest.len());
        let inner = rest[..end].trim().trim_matches(|c| c == '"' || c == '\'');
        if let Some(id) = inner.strip_prefix('#') {
            if !id.is_empty() {
                out.push(id.to_owned());
            }
        }
        rest = &rest[end..];
    }
}

/// A computed value as SVG's CSS parser reads it: a `url()` (which computes to an absolute URL) as
/// the fragment it names, `url(#a)`; references to other documents are not followed by images.
#[cfg(test)]
fn svg_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find("url(") {
        out.push_str(&rest[..i + 4]);
        rest = &rest[i + 4..];
        let end = rest.find(')').unwrap_or(rest.len());
        let inner = rest[..end].trim().trim_matches(|c| c == '"' || c == '\'');
        match inner.rfind('#') {
            Some(h) => out.push_str(&inner[h..]),
            None => out.push_str(inner),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// The color functions of CSS Color 4 and 5 that SVG's image renderer does not read (it reads hex,
/// named, `rgb()` and `hsl()` colors, those of SVG 1.1).
const MODERN_COLORS: [&str; 9] = [
    "lab(",
    "lch(",
    "oklab(",
    "oklch(",
    "color(",
    "hwb(",
    "color-mix(",
    "light-dark(",
    "contrast-color(",
];

/// `v`, a computed value, with each color written in a function the image renderer does not read
/// (a `color-mix()` computes to the mix in its interpolation space, `lch(...)`) written as the
/// sRGB color it converts to (CSS Color 4 §12, the values clamped to the sRGB gamut as the paint
/// operations clamp them), `rgba(r, g, b, a)`: `None` when there is none. A color that still
/// depends on `currentcolor` is left as it is.
fn srgb_colors(v: &str) -> Option<String> {
    if !v.contains('(') {
        return None;
    }
    let b = v.as_bytes();
    let starts = |i: usize, name: &str| {
        b.len() - i >= name.len() && b[i..i + name.len()].eq_ignore_ascii_case(name.as_bytes())
    };
    // (the index after the `)` that closes the one at `open`)
    let close = |open: usize| {
        let mut depth = 0usize;
        for (j, &c) in b.iter().enumerate().skip(open) {
            match c {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j + 1);
                    }
                }
                _ => {}
            }
        }
        None
    };
    let mut out = String::new();
    let (mut i, mut written) = (0, 0);
    while i < b.len() {
        let at_word = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'-');
        if !at_word {
            i += 1;
            continue;
        }
        // (a URL is not a color, whatever it spells)
        if starts(i, "url(") {
            i = v[i..].find(')').map_or(b.len(), |e| i + e + 1);
            continue;
        }
        let Some(name) = MODERN_COLORS.iter().find(|m| starts(i, m)) else {
            i += 1;
            continue;
        };
        let Some(end) = close(i + name.len() - 1) else {
            break;
        };
        if let Some(srgb) = srgb_color(&v[i..end]) {
            out.push_str(&v[written..i]);
            out.push_str(&srgb);
            written = end;
        }
        i = end;
    }
    if written == 0 {
        return None;
    }
    out.push_str(&v[written..]);
    Some(out)
}

/// One color, as [`srgb_colors`] writes it.
fn srgb_color(text: &str) -> Option<String> {
    use style::parser::ParserContext;
    use style::values::computed::Color;
    let url = style::stylesheets::UrlExtraData::from(
        snowlark_net::Url::parse("about:blank").expect("a valid URL"),
    );
    let context = ParserContext::new(
        style::stylesheets::Origin::Author,
        &url,
        Some(style::stylesheets::CssRuleType::Style),
        style_traits::ParsingMode::DEFAULT,
        style::context::QuirksMode::NoQuirks,
        Default::default(),
        None,
        None,
        Default::default(),
    );
    let mut parser = cssparser::Parser::new(text);
    let computed =
        style::values::specified::Color::parse_and_compute(&context, &mut parser, None).ok()?;
    let Color::Absolute(absolute) = computed else {
        return None;
    };
    let c = crate::adapter::to_color(&absolute);
    Some(if c.a == 255 {
        format!("rgb({}, {}, {})", c.r, c.g, c.b)
    } else {
        let alpha = f32::from(c.a) / 255.0;
        format!("rgba({}, {}, {}, {alpha})", c.r, c.g, c.b)
    })
}

/// What a serialization knows of external references.
pub(crate) struct External<'a> {
    pub resources: &'a HashMap<String, SvgResource>,
    /// The URL of the document being rendered.
    pub document: &'a Url,
    /// The resources written so far (the index names their ids).
    written: RefCell<Vec<String>>,
    /// The instances written so far.
    instances: std::cell::Cell<usize>,
}

impl<'a> External<'a> {
    pub fn new(resources: &'a HashMap<String, SvgResource>, document: &'a Url) -> Self {
        External {
            resources,
            document,
            written: RefCell::new(Vec::new()),
            instances: std::cell::Cell::new(0),
        }
    }

    /// An id for the copy of a `use` element's instance.
    fn instance_id(&self) -> String {
        let n = self.instances.get();
        self.instances.set(n + 1);
        format!("snowlark-use-{n}")
    }

    /// The prefix of the ids of resource `key`'s elements.
    fn prefix(&self, key: &str) -> String {
        let mut written = self.written.borrow_mut();
        let i = written.iter().position(|k| k == key).unwrap_or_else(|| {
            written.push(key.to_owned());
            written.len() - 1
        });
        format!("snowlark-resource-{i}-")
    }
}

/// `value` with the `#id` and `url(#id)` in it under `prefix`, for the ids `ours` says so of.
fn prefixed_refs(value: &str, prefix: &str, ours: &dyn Fn(&str) -> bool) -> String {
    if let Some(id) = value.trim().strip_prefix('#') {
        return if ours(id) {
            format!("#{prefix}{id}")
        } else {
            value.to_owned()
        };
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(i) = rest.find("url(") {
        out.push_str(&rest[..i + 4]);
        rest = &rest[i + 4..];
        let end = rest.find(')').unwrap_or(rest.len());
        let inner = rest[..end].trim().trim_matches(|c| c == '"' || c == '\'');
        match inner.strip_prefix('#') {
            Some(id) if ours(id) => {
                out.push('#');
                out.push_str(prefix);
                out.push_str(id);
            }
            _ => out.push_str(&rest[..end]),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

struct Writer<'a> {
    dom: &'a Dom,
    data: &'a StyleData,
    external: &'a External<'a>,
    /// The URL of the document this writes elements of.
    own_url: &'a Url,
    /// Writing a resource document's element, or the instance of a `use`: the prefix of its ids.
    prefix: Option<String>,
    /// Writing an instance: its ids, the ones the prefix is for (what it refers to outside itself
    /// is the document's).
    scoped: Option<HashSet<String>>,
    /// The `use` elements with a shadow tree: (element, the clone in it, the id of its copy).
    instances: Vec<(NodeId, NodeId, String)>,
    /// That element, and the id it gets when it is referred to as the document.
    top: Option<(NodeId, Option<String>)>,
    /// The elements of resources to write: (resource, fragment).
    wanted: Vec<(String, Option<String>)>,
    /// The `use` elements of the document with an external reference, and its resource.
    uses: Vec<(NodeId, String)>,
    /// The resources that the resources written refer to.
    resources: Vec<String>,
    /// The documents style properties refer to.
    style_resources: Vec<String>,
    /// The document's base URL (`<image>` references resolve against it).
    base: &'a Url,
    out: String,
    /// Fragments referenced by what was written.
    refs: Vec<String>,
    /// The absolute URLs of the `<image>` elements written (not `data:` ones).
    images: Vec<String>,
    /// The `viewBox` (or viewport) rectangles of the `svg` elements being written, innermost
    /// last: a CSS transform's `view-box` reference box.
    viewports: Vec<(f32, f32, f32, f32)>,
    /// The paths made for `textPath` elements (`path`, `side`), for the `defs`.
    made_paths: String,
    made_count: usize,
    /// [`PROPERTIES`] serialized as they are needed (`None` until then; then `None` where the
    /// property is unknown), by the computed values they are of: elements share their values with
    /// siblings that match the same rules, and each is the parent of its children. (The values
    /// outlive the writer, which holds the document's `StyleData`: an address stays theirs.)
    serialized: HashMap<*const ComputedValues, Box<[Option<Option<String>>]>>,
}

/// A number list attribute of `el` (`viewBox`: four numbers).
fn numbers(value: &str) -> Vec<f32> {
    value
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .collect()
}

/// A length attribute in user units (a plain number or `px`), when it is one.
fn user_length(dom: &Dom, el: NodeId, name: &str) -> Option<f32> {
    let v = dom.get_attribute(el, name)?.trim();
    let v = v.strip_suffix("px").unwrap_or(v);
    v.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// The geometry of the SVG shape element with the ID `id` in `el`'s tree, as path data in its user
/// coordinates: what `offset-path: url()` follows (CSS Motion Path 1 §2.1). Basic shapes go as their
/// equivalent paths do (SVG 2 §10), clockwise from the right for circles and ellipses.
pub(crate) fn shape_path_by_id(dom: &Dom, el: NodeId, id: &str) -> Option<String> {
    let mut root = el;
    while let Some(parent) = dom.parent(root) {
        root = parent;
    }
    let shape = dom
        .descendants(root)
        .find(|&n| dom.element(n).is_some() && dom.get_attribute(n, "id") == Some(id))?;
    let name = dom.element(shape)?.name();
    if name.ns != ns!(svg) {
        return None;
    }
    let n = |name: &str| user_length(dom, shape, name);
    let z = |name: &str| n(name).unwrap_or(0.0);
    Some(match &*name.local {
        "path" => dom.get_attribute(shape, "d")?.to_owned(),
        "line" => format!("M {} {} L {} {}", z("x1"), z("y1"), z("x2"), z("y2")),
        "rect" => {
            let (x, y, w, h) = (z("x"), z("y"), n("width")?, n("height")?);
            let rx = n("rx").or(n("ry")).unwrap_or(0.0).min(w / 2.0);
            let ry = n("ry").or(n("rx")).unwrap_or(0.0).min(h / 2.0);
            let (r, b) = (x + w, y + h);
            format!(
                "M {} {y} H {} A {rx} {ry} 0 0 1 {r} {} V {} A {rx} {ry} 0 0 1 {} {b} H {} \
                 A {rx} {ry} 0 0 1 {x} {} V {} A {rx} {ry} 0 0 1 {} {y} Z",
                x + rx,
                r - rx,
                y + ry,
                b - ry,
                r - rx,
                x + rx,
                b - ry,
                y + ry,
                x + rx,
            )
        }
        "circle" | "ellipse" => {
            let (rx, ry) = match n("r") {
                Some(r) if &*name.local == "circle" => (r, r),
                _ => (n("rx")?, n("ry")?),
            };
            let (cx, cy) = (z("cx"), z("cy"));
            format!(
                "M {} {cy} A {rx} {ry} 0 1 1 {} {cy} A {rx} {ry} 0 1 1 {} {cy} Z",
                cx + rx,
                cx - rx,
                cx + rx
            )
        }
        "polyline" | "polygon" => {
            let numbers: Vec<&str> = dom
                .get_attribute(shape, "points")?
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .collect();
            let mut d = String::new();
            for (i, p) in numbers.as_chunks::<2>().0.iter().enumerate() {
                d.push_str(if i == 0 { "M " } else { " L " });
                d.push_str(p[0]);
                d.push(' ');
                d.push_str(p[1]);
            }
            if &*name.local == "polygon" {
                d.push_str(" Z");
            }
            d
        }
        _ => return None,
    })
}

/// The object bounding box (SVG 2 §8.10) of a basic shape or a positioned element, from its
/// attributes; `None` for paths, groups and text, whose geometry is the rasterizer's.
fn attribute_bbox(dom: &Dom, el: NodeId, local: &str) -> Option<(f32, f32, f32, f32)> {
    let n = |name: &str| user_length(dom, el, name);
    match local {
        "rect" | "image" | "use" | "foreignObject" | "svg" => Some((
            n("x").unwrap_or(0.0),
            n("y").unwrap_or(0.0),
            n("width")?,
            n("height")?,
        )),
        "circle" => {
            let r = n("r")?;
            Some((
                n("cx").unwrap_or(0.0) - r,
                n("cy").unwrap_or(0.0) - r,
                2.0 * r,
                2.0 * r,
            ))
        }
        "ellipse" => {
            let (rx, ry) = (n("rx")?, n("ry")?);
            Some((
                n("cx").unwrap_or(0.0) - rx,
                n("cy").unwrap_or(0.0) - ry,
                2.0 * rx,
                2.0 * ry,
            ))
        }
        "line" => {
            let (x1, y1) = (n("x1").unwrap_or(0.0), n("y1").unwrap_or(0.0));
            let (x2, y2) = (n("x2").unwrap_or(0.0), n("y2").unwrap_or(0.0));
            Some((x1.min(x2), y1.min(y2), (x2 - x1).abs(), (y2 - y1).abs()))
        }
        _ => None,
    }
}

/// An outermost `svg` element as a standalone SVG document.
pub(crate) struct Serialized {
    pub markup: String,
    /// The URLs its `<image>` elements show, to fetch (each once).
    pub images: Vec<String>,
    /// Its `use` elements with an external reference: (element, the resource's URL).
    pub uses: Vec<(NodeId, String)>,
    /// The resources that resources refer to.
    pub resources: Vec<String>,
    /// The documents style properties refer to.
    pub style_resources: Vec<String>,
}

impl Writer<'_> {
    /// Whether `el`'s `image-orientation` is `none` (CSS Images 3 §5.3).
    fn unoriented(&self, el: NodeId) -> bool {
        use style::properties::longhands::image_orientation::computed_value::T;
        self.data
            .styles(el)
            .is_some_and(|v| v.get_inherited_box().image_orientation == T::None)
    }

    /// The CSS transform of `el` (CSS Transforms 1: it replaces the `transform` attribute) as an
    /// SVG matrix, resolved against its `transform-box` (`view-box`: the nearest viewport's
    /// `viewBox`; `fill-box`: its object bounding box; `stroke-box`: that with its stroke).
    fn css_transform(&self, el: NodeId, local: &str) -> Option<String> {
        let m = self.css_matrix(el, local)?;
        Some(format!(
            "matrix({} {} {} {} {} {})",
            m.m11, m.m12, m.m21, m.m22, m.dx, m.dy
        ))
    }

    /// The object bounding box of `el` in its user space (SVG 2 §8.10): its geometry, or for a
    /// container its children's, through their transforms.
    fn object_bbox(&self, el: NodeId, local: &str) -> Option<(f32, f32, f32, f32)> {
        use kurbo::Shape;
        match local {
            "g" | "a" | "switch" => {
                let mut all: Option<snowlark_paint::Rect> = None;
                for child in self.dom.children(el) {
                    let Some(e) = self.dom.element(child) else {
                        continue;
                    };
                    let name = e.name().local.clone();
                    let Some(b) = self.object_bbox(child, &name) else {
                        continue;
                    };
                    let mut r = snowlark_paint::Rect::new(b.0, b.1, b.2, b.3);
                    if let Some(m) = self.css_matrix(child, &name) {
                        r = m.apply_rect(&r);
                    }
                    all = Some(all.map_or(r, |a| a.union(&r)));
                }
                all.map(|r| (r.x, r.y, r.width, r.height))
            }
            "path" => {
                let d = self.dom.get_attribute(el, "d")?;
                let b = snowlark_layout::svg::PathData::parse(d)
                    .to_bezpath()
                    .bounding_box();
                let (x, y) = (b.x0 as f32, b.y0 as f32);
                Some((x, y, b.width() as f32, b.height() as f32))
            }
            _ => attribute_bbox(self.dom, el, local),
        }
    }

    /// [`Writer::object_bbox`] with half the stroke's width around it (a `stroke-box`).
    fn stroke_bbox(&self, el: NodeId, local: &str) -> Option<(f32, f32, f32, f32)> {
        let (x, y, w, h) = self.object_bbox(el, local)?;
        let values = self.data.styles(el)?;
        let property = |name| snowlark_style::computed::computed_property_by_name(values, name);
        // (a non-scaling stroke has no size in user space: the box is the fill box)
        let stroked = property("stroke").is_some_and(|s| s != "none")
            && property("vector-effect").is_none_or(|v| v != "non-scaling-stroke");
        let width = property("stroke-width")
            .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
            .filter(|_| stroked)
            .unwrap_or(0.0);
        let half = width / 2.0;
        Some((x - half, y - half, w + width, h + width))
    }

    fn css_matrix(&self, el: NodeId, local: &str) -> Option<snowlark_paint::Matrix> {
        use style::values::computed::TransformBox;
        let values = self.data.styles(el)?;
        if !crate::adapter::has_css_transform(values) {
            return None;
        }
        let view = self
            .viewports
            .last()
            .map_or((0.0, 0.0, 300.0, 150.0), |&(_, _, w, h)| (0.0, 0.0, w, h));
        let reference = match values.get_box().transform_box {
            TransformBox::ViewBox => view,
            TransformBox::StrokeBox | TransformBox::BorderBox => {
                self.stroke_bbox(el, local).unwrap_or(view)
            }
            _ => self.object_bbox(el, local).unwrap_or(view),
        };
        let path =
            crate::motion::url_fragment(values).and_then(|id| shape_path_by_id(self.dom, el, &id));
        crate::adapter::css_transform(
            values,
            snowlark_paint::Rect::new(reference.0, reference.1, reference.2, reference.3),
            None,
            path.as_deref(),
        )
    }

    /// The `style` declarations for `el`: the properties it does not inherit unchanged from
    /// `parent` (all of them without a parent).
    fn style_of(&mut self, el: NodeId, parent: Option<&ComputedValues>) -> String {
        let data = self.data;
        let Some(values) = data.styles(el) else {
            return String::new();
        };
        let mut style = String::new();
        // (what a `use` of another document targets is `:target` there, whose rules may be all that
        // show it)
        let top = self.scoped.is_none() && self.top.as_ref().is_some_and(|t| t.0 == el);
        if values.get_box().display.is_none() && !top {
            style.push_str("display:none;");
        }
        let color = ids().color.as_ref();
        for (i, &(name, inherited)) in PROPERTIES.iter().enumerate() {
            // (equal computed values, and an equal `color` for `currentcolor`, serialize the same:
            // what neither the parent nor the initial value would change is not serialized)
            if let (Some(p), Some(id)) = (parent, &ids().copied[i]) {
                let same = if inherited {
                    same_computed_value(values, p, id)
                        && color.is_some_and(|c| same_computed_value(values, p, c))
                } else {
                    // (no initial value here holds `currentcolor`: `initial_values_skip`)
                    same_computed_value(values, snowlark_style::computed::initial_values(), id)
                };
                if same {
                    continue;
                }
            }
            let Some(v) = self.serialized_value(values, i) else {
                continue;
            };
            let v = &v;
            // (in a resource document: what comes from the `use` element stays open)
            let current = self.prefix.is_some() && v.contains(CURRENT_COLOR);
            if current && name == "color" {
                continue;
            }
            let emit = match parent {
                None => true,
                Some(p) if inherited => self.serialized_value(p, i).as_ref() != Some(v),
                // (non-inherited: when it is not the initial value)
                Some(_) => !is_initial_serialization(name, v),
            };
            if emit && current {
                let v = self.style_value(v).replace(CURRENT_COLOR, "currentColor");
                let _ = write!(style, "{name}:{v};");
            } else if emit {
                let mut v = self.style_value(v);
                if name == "filter" {
                    match self.drawn_filter(el, &v) {
                        Some(f) => v = f,
                        None => continue,
                    }
                }
                let _ = write!(style, "{name}:{v};");
            }
        }
        style
    }

    /// A `filter` value without the `url()`s that name no `<filter>` (a missing or not yet
    /// loaded one): the element is drawn without them, as a box is; `None` when nothing is left.
    fn drawn_filter(&self, el: NodeId, v: &str) -> Option<String> {
        let mut out = String::with_capacity(v.len());
        let mut rest = v;
        while let Some(i) = rest.find("url(") {
            out.push_str(&rest[..i]);
            let end = rest[i..].find(')').map_or(rest.len(), |e| i + e + 1);
            let token = &rest[i..end];
            let id = token
                .rsplit_once('#')
                .map(|(_, id)| id.trim_end_matches([')', '"', '\'', ' ']))
                .unwrap_or("");
            let named = id.starts_with("snowlark-") && id != "snowlark-none"
                || crate::filter_ref::find_filter(self.dom, el, id).is_some();
            if named {
                out.push_str(token);
            }
            rest = &rest[end..];
        }
        out.push_str(rest);
        let out = out.trim();
        (!out.is_empty()).then(|| out.to_owned())
    }

    /// [`PROPERTIES`]`[i]` of `values`, `None` where the property is unknown.
    fn serialized_value(&mut self, values: &ComputedValues, i: usize) -> Option<String> {
        let slots = self
            .serialized
            .entry(std::ptr::from_ref(values))
            .or_insert_with(|| vec![None; PROPERTIES.len()].into_boxed_slice());
        slots[i]
            .get_or_insert_with(|| {
                let v = ids().copied[i]
                    .as_ref()
                    .map(|id| snowlark_style::computed::computed_property(values, id))?;
                let colors = matches!(
                    PROPERTIES[i].0,
                    "color" | "fill" | "stroke" | "stop-color" | "flood-color" | "lighting-color"
                );
                Some(match colors.then(|| srgb_colors(&v)).flatten() {
                    Some(srgb) => srgb,
                    None => v,
                })
            })
            .clone()
    }

    /// Whether the prefix is for `id`.
    fn owns(&self, id: &str) -> bool {
        !id.starts_with("snowlark-resource-") && self.scoped.as_ref().is_none_or(|s| s.contains(id))
    }

    /// A computed value as SVG's CSS parser reads it: [`svg_value`], with a `url()` of another
    /// document naming the copy of its element in the `defs` (SVG 2 §16.1.7; a reference still
    /// unresolved names nothing, so that the value's fallback shows).
    fn style_value(&mut self, v: &str) -> String {
        if !v.contains("url(") {
            return v.to_owned();
        }
        let mut out = String::with_capacity(v.len());
        let mut rest = v;
        while let Some(i) = rest.find("url(") {
            out.push_str(&rest[..i + 4]);
            rest = rest[i + 4..].trim_start();
            // (a quoted URL may hold parentheses)
            let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
            let from = quote.map_or(0, |_| 1);
            let end = match quote {
                Some(q) => rest[1..].find(q).map_or(rest.len(), |e| e + 1),
                None => rest.find(')').unwrap_or(rest.len()),
            };
            let inner = rest[from..end].trim();
            out.push_str(&self.style_reference(inner));
            rest = &rest[(end + from).min(rest.len())..];
        }
        out.push_str(rest);
        out
    }

    /// What a `url()` of a computed value is written as.
    fn style_reference(&mut self, inner: &str) -> String {
        let fragment_only = || match inner.rfind('#') {
            Some(h) => inner[h..].to_owned(),
            None => inner.to_owned(),
        };
        let Ok(url) = Url::parse(inner) else {
            return fragment_only();
        };
        let mut key = url.clone();
        key.set_fragment(None);
        let without = |u: &Url| {
            let mut u = u.clone();
            u.set_fragment(None);
            u
        };
        if key == without(self.own_url) || key == without(self.base) {
            return fragment_only();
        }
        let Some(fragment) = url.fragment().map(percent_decoded) else {
            return "#snowlark-none".to_owned();
        };
        let key = key.to_string();
        self.style_resources.push(key.clone());
        let stored = style_key(&key);
        match self.external.resources.get(&stored) {
            Some(SvgResource::Loaded(r)) if r.target(Some(&fragment)).is_some() => {
                let prefix = self.external.prefix(&stored);
                let id = format!("#{prefix}{fragment}");
                self.wanted.push((stored, Some(fragment)));
                id
            }
            _ => "#snowlark-none".to_owned(),
        }
    }

    /// `value` with its references to what this writes under the prefix.
    fn scoped_refs(&self, value: &str) -> String {
        match &self.prefix {
            Some(prefix) => prefixed_refs(value, prefix, &|id| self.owns(id)),
            None => value.to_owned(),
        }
    }

    /// The `href` a `use` element's reference is written as (`None`: nothing to show yet, or it is
    /// invalid). A reference to the document itself stays one, whatever its form; one to another
    /// document refers to the copy of its element in the `defs` (SVG 2 §16.1.7).
    fn use_reference(&mut self, el: NodeId, href: &str) -> Option<String> {
        // (its shadow tree: the instance is what shows)
        let clone = self
            .dom
            .shadow_root(el)
            .and_then(|s| self.dom.children(s).find(|&c| self.dom.is_element(c)));
        if let Some(clone) = clone {
            // (an instance with `display: none`, or under one, shows nothing)
            if self
                .data
                .styles(clone)
                .is_none_or(|v| v.get_box().display.is_none())
            {
                return None;
            }
            let id = self.external.instance_id();
            self.instances.push((el, clone, id.clone()));
            return Some(format!("#{id}"));
        }
        let v = href.trim();
        let local = |w: &mut Self, id: String| {
            let out = match &w.prefix {
                Some(prefix) if w.owns(&id) => format!("#{prefix}{id}"),
                _ => format!("#{id}"),
            };
            w.refs.push(id);
            Some(out)
        };
        if let Some(fragment) = v.strip_prefix('#') {
            return local(self, percent_decoded(fragment));
        }
        let url = self.base.join(v).ok()?;
        let fragment = url.fragment().map(percent_decoded);
        let mut key = url.clone();
        key.set_fragment(None);
        let mut own = self.own_url.clone();
        own.set_fragment(None);
        if key == own {
            return local(self, fragment?);
        }
        let key = key.to_string();
        if self.prefix.is_some() {
            self.resources.push(key.clone());
        } else {
            self.uses.push((el, key.clone()));
        }
        let Some(SvgResource::Loaded(r)) = self.external.resources.get(&key) else {
            return None;
        };
        r.target(fragment.as_deref())?;
        let prefix = self.external.prefix(&key);
        let id = fragment.clone().unwrap_or_else(|| "root".to_owned());
        self.wanted.push((key, fragment));
        Some(format!("#{prefix}{id}"))
    }

    /// The attribute a computed geometry property or `transform` of `el` makes of `name`: `None`
    /// when the attribute stands, `Some(None)` when it goes (`auto`, no transform).
    fn computed_attribute(&self, el: NodeId, name: &str, root: bool) -> Option<Option<String>> {
        if root {
            return None;
        }
        let element = self.dom.element(el)?;
        let local = element.name().local.clone();
        // (svgwg#1059: `width` and `height` of `use` and `symbol` are attributes, not properties)
        if matches!(&*local, "use" | "symbol") && matches!(name, "width" | "height") {
            return None;
        }
        let property = snowlark_style::svg_presentation_property(&local, false, name)?;
        let values = self.data.styles(el)?;
        if property == "transform" {
            // (an SVG transform list of the matrix, with its origin in it)
            return Some(self.css_transform(el, &local));
        }
        let v = match ids().geometry.get(property) {
            Some(id) => snowlark_style::computed::computed_property(values, id),
            None => snowlark_style::computed::computed_property_by_name(values, property)?,
        };
        match v.as_str() {
            "auto" => Some(None),
            "none" => None,
            v if property == "d" => {
                let inner = v.strip_prefix("path(\"")?.strip_suffix("\")")?;
                // (the attribute as written, when that is what computed: its segments are what
                // markers go by)
                use snowlark_layout::svg::PathData;
                let written = self.dom.attr(el, &ns!(), &local_name!("d"));
                let same = written.is_some_and(|d| {
                    d == inner
                        || PathData::parse(d).normalized() == PathData::parse(inner).normalized()
                });
                (!same).then(|| Some(inner.to_owned()))
            }
            v if v.contains('(') => None,
            v => Some(Some(v.to_owned())),
        }
    }

    /// A `textPath` whose path is not one resvg can name: its `path` attribute (SVG 2 §11.8.1, which
    /// wins over `href`), or a referenced path to be followed from its end (`side="right"`). The
    /// path is made in the `defs`; returns the `href` to write.
    fn text_path_source(&mut self, el: NodeId) -> Option<String> {
        use snowlark_layout::svg::PathData;
        // (a `path` with no path in it falls back on `href`)
        let path = self
            .dom
            .attr(el, &ns!(), &"path".into())
            .filter(|d| !PathData::parse(d).commands.is_empty());
        let right = self.dom.attr(el, &ns!(), &"side".into()).map(str::trim) == Some("right");
        if path.is_none() && !right {
            return None;
        }
        let geometry = match path {
            Some(d) => PathData::parse(d).to_bezpath(),
            None => {
                let id = self
                    .dom
                    .attr(el, &ns!(), &local_name!("href"))
                    .or_else(|| self.dom.attr(el, &ns!(xlink), &local_name!("href")))?
                    .trim()
                    .strip_prefix('#')?;
                let root = self.dom.root(el);
                let target = self.dom.descendants(root).find(|&n| {
                    self.dom.is_element(n)
                        && self.dom.attr(n, &ns!(), &local_name!("id")) == Some(id)
                })?;
                PathData::parse(self.dom.attr(target, &ns!(), &local_name!("d"))?).to_bezpath()
            }
        };
        let geometry = if right {
            geometry.reverse_subpaths()
        } else {
            geometry
        };
        self.made_count += 1;
        let prefix = self.prefix.as_deref().unwrap_or("");
        let id = format!("{prefix}snowlark-text-path-{}", self.made_count);
        let _ = write!(
            self.made_paths,
            "<path id=\"{id}\" d=\"{}\"/>",
            geometry.to_svg()
        );
        Some(format!("#{id}"))
    }

    /// A number attribute of `el`, its computed geometry property where it is one.
    fn number(&self, el: NodeId, name: &str) -> f64 {
        self.computed_attribute(el, name, false)
            .flatten()
            .or_else(|| self.dom.attr(el, &ns!(), &name.into()).map(str::to_owned))
            .and_then(|v| v.trim().trim_end_matches("px").parse().ok())
            .unwrap_or(0.0)
    }

    /// The path data of a markable element (SVG 2 §13.7.1: `path`, `line`, `polyline`, `polygon`).
    fn marker_path(&self, el: NodeId, local: &str) -> Option<snowlark_layout::svg::PathData> {
        use snowlark_layout::svg::{parse_points, PathData};
        let points = |closed: bool| -> Option<PathData> {
            let list = parse_points(self.dom.presented_attr(el, &"points".into())?)?;
            let mut d = String::new();
            for (i, (x, y)) in list.iter().enumerate() {
                let _ = write!(d, "{} {x} {y} ", if i == 0 { 'M' } else { 'L' });
            }
            if closed {
                d.push('Z');
            }
            Some(PathData::parse(&d))
        };
        match local {
            "path" => {
                let computed = self.computed_attribute(el, "d", false).flatten();
                let d = computed
                    .as_deref()
                    .or(self.dom.presented_attr(el, &local_name!("d")))?;
                Some(PathData::parse(d))
            }
            "line" => {
                let n = |name: &str| self.number(el, name);
                let d = format!("M {} {} L {} {}", n("x1"), n("y1"), n("x2"), n("y2"));
                Some(PathData::parse(&d))
            }
            "polyline" => points(false),
            "polygon" => points(true),
            _ => None,
        }
    }

    /// The markers of `el`, as paths of their own that put each one at its vertex (SVG 2 §13.7).
    /// resvg goes by the segments it made of the path, which for arcs and closed subpaths are not
    /// the path's vertices, so `el` writes none and each marker is a line through its vertex in the
    /// direction it orients by, painted like `el` so that context paint holds, but invisible. Each
    /// line also holds `el`'s bounding box, which context paint in bounding box units resolves against;
    /// that is why a middle marker is the end of a line: an end comes after whatever precedes it.
    /// Returns what to add to `el`'s `style` and the paths to write after it.
    fn marker_instances(
        &mut self,
        el: NodeId,
        local: &str,
        style: &str,
    ) -> Option<(&'static str, String)> {
        let values = self.data.styles(el)?;
        if values.get_box().display.is_none() {
            return None;
        }
        let in_clip_path = self.dom.ancestors(el).any(|a| {
            self.dom
                .element(a)
                .is_some_and(|e| &*e.name().local == "clipPath")
        });
        let mut marker = |name: &str| {
            snowlark_style::computed::computed_property_by_name(values, name)
                .filter(|v| v != "none")
                .map(|v| self.style_value(&v))
        };
        let (start, mid, end) = (
            marker("marker-start"),
            marker("marker-mid"),
            marker("marker-end"),
        );
        if in_clip_path || (start.is_none() && mid.is_none() && end.is_none()) {
            return None;
        }
        // (only where resvg's segments are not the vertices: its own markers know context paint)
        let path = self.marker_path(el, local)?;
        let own_vertices = path
            .commands
            .iter()
            .any(|c| matches!(c.cmd.to_ascii_uppercase(), b'A' | b'Z'));
        if !own_vertices {
            return None;
        }
        let vertices = path.marker_vertices();
        let bounds = {
            use kurbo::Shape;
            path.to_bezpath().bounding_box()
        };
        let extent = format!(
            "M {} {} L {} {}",
            bounds.x0, bounds.y0, bounds.x1, bounds.y1
        );
        let e = self.dom.element(el)?;
        let mut common = String::new();
        for a in e.attrs() {
            let name = a.local_name();
            let paint = *a.namespace() == ns!()
                && !matches!(
                    &**name,
                    "id" | "class"
                        | "style"
                        | "d"
                        | "points"
                        | "x1"
                        | "y1"
                        | "x2"
                        | "y2"
                        | "pathLength"
                        | "transform"
                        | "transform-origin"
                )
                && !name.starts_with("marker-")
                && !name.to_ascii_lowercase().starts_with("on");
            if paint {
                let _ = write!(common, " {name}=\"");
                escape_into(&mut common, a.value(), true);
                common.push('"');
            }
        }
        let transform = match self.computed_attribute(el, "transform", false) {
            Some(t) => t,
            None => self
                .dom
                .attr(el, &ns!(), &"transform".into())
                .map(str::to_owned),
        };
        if let Some(t) = transform {
            common.push_str(" transform=\"");
            escape_into(&mut common, &t, true);
            common.push('"');
        }
        let mut paths = String::new();
        let last = vertices.len().saturating_sub(1);
        let mut write_one = |w: &mut Self, kind: &str, url: &str, d: String| {
            let mut refs = Vec::new();
            fragment_refs(url, &mut refs);
            w.refs.extend(refs);
            let url = w.scoped_refs(url);
            let _ = write!(paths, "<path d=\"{d}\"{common} style=\"");
            escape_into(&mut paths, style, true);
            paths.push_str("fill-opacity:0;stroke-opacity:0;stroke-dasharray:none;");
            paths.push_str("marker-start:none;marker-mid:none;marker-end:none;");
            let _ = write!(paths, "marker-{kind}:");
            escape_into(&mut paths, &url, true);
            paths.push_str(";\"/>");
        };
        // (start, then the middle ones along the path, then end: the order they paint in)
        for (i, (p, angle)) in vertices.iter().enumerate() {
            let (x, y) = (p.x, p.y);
            // (short enough to stay in the bounding box, long enough for an f32 to keep the angle)
            let size = x
                .abs()
                .max(y.abs())
                .max(bounds.width())
                .max(bounds.height());
            let step = (size * 1e-4).max(1e-6);
            let (dx, dy) = (angle.cos() * step, angle.sin() * step);
            let leaving = format!("M {x} {y} L {} {} {extent}", x + dx, y + dy);
            let arriving = format!("{extent} M {} {} L {x} {y}", x - dx, y - dy);
            if i == 0 {
                if let Some(url) = &start {
                    write_one(self, "start", url, leaving);
                }
            }
            if i != 0 && i != last {
                if let Some(url) = &mid {
                    write_one(self, "end", url, arriving.clone());
                }
            }
            if i == last {
                if let Some(url) = &end {
                    write_one(self, "end", url, arriving);
                }
            }
        }
        Some(("marker-start:none;marker-mid:none;marker-end:none;", paths))
    }

    /// The length of a shape, as SVG 2 §9.6.2 scales its dashes by it against `pathLength`.
    fn shape_length(&self, el: NodeId, local: &str) -> Option<f64> {
        use snowlark_layout::svg::{geom::total_length, parse_points, PathData};
        let px = |name: &str| self.number(el, name);
        // (a circle and an ellipse measure as the arcs of a path do)
        let oval = |cx: f64, cy: f64, rx: f64, ry: f64| {
            let (a, b) = (cx + rx, cx - rx);
            let arc = format!("A {rx} {ry} 0 0 1");
            let d = format!("M {a} {cy} {arc} {b} {cy} {arc} {a} {cy} Z");
            total_length(&PathData::parse(&d).to_bezpath())
        };
        let polyline = |closed: bool| -> Option<f64> {
            let points = parse_points(self.dom.presented_attr(el, &"points".into())?)?;
            let legs = points
                .windows(2)
                .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1));
            let back = match (closed, points.first(), points.last()) {
                (true, Some(a), Some(b)) => (a.0 - b.0).hypot(a.1 - b.1),
                _ => 0.0,
            };
            Some(legs.fold(back, |sum, l| sum + l))
        };
        Some(match local {
            "path" => {
                let d = self.dom.presented_attr(el, &local_name!("d"))?;
                total_length(&PathData::parse(d).to_bezpath())
            }
            "rect" => 2.0 * (px("width") + px("height")),
            "circle" => oval(px("cx"), px("cy"), px("r"), px("r")),
            "ellipse" => oval(px("cx"), px("cy"), px("rx"), px("ry")),
            "line" => (px("x2") - px("x1")).hypot(px("y2") - px("y1")),
            "polyline" => polyline(false)?,
            "polygon" => polyline(true)?,
            _ => return None,
        })
    }

    /// `pathLength` as a scale on the element's dashes: declarations to add to its `style`.
    fn dash_scale(&self, el: NodeId, local: &str) -> Option<String> {
        let declared: f64 = self
            .dom
            .presented_attr(el, &"pathLength".into())?
            .trim()
            .parse()
            .ok()
            .filter(|v: &f64| *v >= 0.0)?;
        let values = self.data.styles(el)?;
        let dashes =
            snowlark_style::computed::computed_property_by_name(values, "stroke-dasharray")?;
        if dashes == "none" {
            return None;
        }
        let scale = if declared > 0.0 {
            self.shape_length(el, local)? / declared
        } else {
            0.0
        };
        let scaled = |list: &str| -> Option<String> {
            let mut out = Vec::new();
            for t in list.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
                if t.is_empty() {
                    continue;
                }
                let v: f64 = t.trim_end_matches("px").parse().ok()?;
                out.push((v * scale).to_string());
            }
            Some(out.join(" "))
        };
        let offset =
            snowlark_style::computed::computed_property_by_name(values, "stroke-dashoffset")?;
        Some(format!(
            "stroke-dasharray:{};stroke-dashoffset:{};",
            scaled(&dashes)?,
            scaled(&offset)?
        ))
    }

    /// Writes `el` and its subtree. `parent` is the computed style `el` inherits from (none for an
    /// element whose context is outside what is written).
    fn element(&mut self, el: NodeId, parent: Option<&ComputedValues>, root: bool) {
        let Some(e) = self.dom.element(el) else {
            return;
        };
        let name = e.name();
        if name.ns != ns!(svg) {
            return;
        }
        // (HTML in `foreignObject` and scripts are not part of an image)
        if matches!(
            name.local,
            local_name!("foreignObject") | local_name!("script") | local_name!("style")
        ) {
            return;
        }
        // SVG 2 §5.7: an `image`'s `href` without a namespace wins over `xlink:href`.
        let image = name.local == local_name!("image");
        let is_use = name.local == local_name!("use");
        let plain_href = (image || is_use)
            && e.attrs()
                .iter()
                .any(|a| *a.namespace() == ns!() && &**a.local_name() == "href");
        let viewport = (name.local == local_name!("svg")).then(|| {
            let vb = self
                .dom
                .get_attribute(el, "viewBox")
                .map(numbers)
                .filter(|n| n.len() == 4 && n[2] > 0.0 && n[3] > 0.0);
            match vb {
                Some(n) => (n[0], n[1], n[2], n[3]),
                None => (
                    0.0,
                    0.0,
                    user_length(self.dom, el, "width").unwrap_or(300.0),
                    user_length(self.dom, el, "height").unwrap_or(150.0),
                ),
            }
        });
        let made_href = if &*name.local == "textPath" {
            self.text_path_source(el)
        } else {
            None
        };
        self.out.push('<');
        self.out.push_str(&name.local);
        if let Some(href) = &made_href {
            self.out.push_str(" href=\"");
            escape_into(&mut self.out, href, true);
            self.out.push('"');
        }
        if root {
            self.out.push_str(
                " xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\"",
            );
        }
        // (a resource's root element, referred to without a fragment: an id for the reference)
        let forced_id = self
            .top
            .as_ref()
            .filter(|t| t.0 == el)
            .and_then(|t| t.1.clone());
        if let Some(id) = &forced_id {
            let _ = write!(self.out, " id=\"{id}\"");
        }
        // (each is read for the attributes that stand and again for those it writes)
        let geometry = GEOMETRY.map(|name| self.computed_attribute(el, name, root));
        let replaced = |name: &str| {
            GEOMETRY
                .iter()
                .position(|g| *g == name)
                .is_some_and(|i| geometry[i].is_some())
        };
        for a in e.attrs() {
            let local = a.local_name();
            let qualified = if *a.namespace() == ns!(xlink) {
                format!("xlink:{local}")
            } else if *a.namespace() == ns!() {
                local.to_string()
            } else {
                continue;
            };
            if qualified == "style"
                || qualified.starts_with("xmlns")
                || qualified.to_ascii_lowercase().starts_with("on")
            {
                continue;
            }
            if *a.namespace() == ns!() && replaced(local) {
                continue;
            }
            // (the matrix written for `transform` has its origin in it)
            if &**local == "transform-origin" && replaced("transform") {
                continue;
            }
            if matches!(&**local, "path" | "side") && &*name.local == "textPath"
                || (made_href.is_some() && &**local == "href")
            {
                continue;
            }
            let animated = self.dom.animated_attr(el, local);
            if is_use && &**local == "href" {
                if !(*a.namespace() == ns!(xlink) && plain_href) {
                    if let Some(href) = self.use_reference(el, animated.unwrap_or(a.value())) {
                        self.out.push_str(" href=\"");
                        escape_into(&mut self.out, &href, true);
                        self.out.push('"');
                    }
                }
                continue;
            }
            // (`filter` is written from the computed value, without the references that name no
            // `<filter>`)
            if &**local == "filter" {
                continue;
            }
            // (a `url()` of another document: the computed value written below has it)
            if &**local != "href"
                && REFERENCING.contains(&&**local)
                && a.value().contains("url(")
                && !a.value().contains("url(#")
                && !a.value().contains("url(\"#")
                && !a.value().contains("url('#")
                && self.data.styles(el).is_some()
            {
                continue;
            }
            if REFERENCING.contains(&&**local) {
                fragment_refs(a.value(), &mut self.refs);
            }
            let mut value = std::borrow::Cow::Borrowed(animated.unwrap_or(a.value()));
            if &**local == "d" {
                if let Some(d) = crate::motion::without_bearings(&value) {
                    value = d.into();
                }
            }
            if let Some(prefix) = self.prefix.clone() {
                if qualified == "id" {
                    if forced_id.is_some() {
                        continue;
                    }
                    value = std::borrow::Cow::Owned(format!("{prefix}{}", a.value()));
                } else if REFERENCING.contains(&&**local) && !image {
                    value = std::borrow::Cow::Owned(self.scoped_refs(a.value()));
                }
            }
            if image && &**local == "href" {
                if *a.namespace() == ns!(xlink) && plain_href {
                    continue;
                }
                // (resolved against the document's base URL; `data:` URLs are the image's own)
                let v = a.value().trim();
                if !v.starts_with('#') {
                    if let Ok(u) = self.base.join(v) {
                        if u.scheme() != "data" {
                            let s = u.to_string();
                            if !self.images.contains(&s) {
                                self.images.push(s.clone());
                            }
                            // (`image-orientation: none` is told to the image resolver by a
                            // suffix only it reads: crate::svg::UNORIENTED)
                            value = std::borrow::Cow::Owned(if self.unoriented(el) {
                                format!("{s}{}", crate::svg::UNORIENTED)
                            } else {
                                s
                            });
                        }
                    }
                }
            }
            self.out.push(' ');
            self.out.push_str(&qualified);
            self.out.push_str("=\"");
            escape_into(&mut self.out, &value, true);
            self.out.push('"');
        }
        // SVG 2 §5.6: a `use` of a `symbol` without the size takes the symbol's.
        if is_use {
            let symbol = self.dom.shadow_root(el).and_then(|s| {
                self.dom.children(s).find(|&c| {
                    self.dom
                        .element(c)
                        .is_some_and(|e| &*e.name().local == "symbol")
                })
            });
            if let Some(symbol) = symbol {
                for name in ["width", "height"] {
                    let own = self.dom.attr(el, &ns!(), &name.into());
                    if let (None, Some(v)) = (own, self.dom.attr(symbol, &ns!(), &name.into())) {
                        let _ = write!(self.out, " {name}=\"");
                        escape_into(&mut self.out, v, true);
                        self.out.push('"');
                    }
                }
            }
        }
        // (what animations set on an element without the attribute)
        for (local, v) in self.dom.animated_attrs(el) {
            if !GEOMETRY.contains(&&**local) && !e.attrs().iter().any(|a| a.local_name() == local) {
                self.out.push(' ');
                self.out.push_str(local);
                self.out.push_str("=\"");
                escape_into(&mut self.out, v, true);
                self.out.push('"');
            }
        }
        for (name, v) in GEOMETRY.iter().zip(&geometry) {
            if let Some(Some(v)) = v {
                self.out.push(' ');
                self.out.push_str(name);
                self.out.push_str("=\"");
                escape_into(&mut self.out, v, true);
                self.out.push('"');
            }
        }
        let mut style = self.style_of(el, parent);
        if let Some(dashes) = self.dash_scale(el, &name.local) {
            style.push_str(&dashes);
        }
        let markers = self.marker_instances(el, &name.local, &style);
        if let Some((none, _)) = &markers {
            style.push_str(none);
        }
        if !style.is_empty() {
            let mut refs = Vec::new();
            fragment_refs(&style, &mut refs);
            self.refs.extend(refs);
            style = self.scoped_refs(&style);
            self.out.push_str(" style=\"");
            escape_into(&mut self.out, &style, true);
            self.out.push('"');
        }
        self.out.push('>');
        if let Some(v) = viewport {
            self.viewports.push(v);
        }
        // SVG 2 §8.6: a viewBox of zero width or height disables the rendering of the element.
        let disabled = !root
            && self
                .dom
                .attr(el, &ns!(), &"viewBox".into())
                .and_then(snowlark_layout::svg::ViewBox::parse)
                .is_some_and(|v| v.width == 0.0 || v.height == 0.0);
        let values = self.data.styles(el);
        for c in self.dom.children(el).filter(|_| !disabled) {
            match self.dom.data(c) {
                NodeData::Element(_) => self.element(c, values, false),
                NodeData::Text(t) => escape_into(&mut self.out, t, false),
                _ => {}
            }
        }
        if viewport.is_some() {
            self.viewports.pop();
        }
        self.out.push_str("</");
        self.out.push_str(&name.local);
        self.out.push('>');
        if let Some((_, paths)) = markers {
            self.out.push_str(&paths);
        }
    }
}

/// Serializes the outermost `svg` element `root` as a standalone SVG document (see the module
/// documentation), with the elements it refers to elsewhere in its document copied into a `defs`,
/// and lists the images its `<image>` elements show (`base` is the document's base URL).
pub(crate) fn serialize(
    dom: &Dom,
    data: &StyleData,
    root: NodeId,
    base: &Url,
    external: &External<'_>,
) -> Serialized {
    let mut w = Writer {
        dom,
        data,
        external,
        own_url: external.document,
        prefix: None,
        scoped: None,
        instances: Vec::new(),
        top: None,
        wanted: Vec::new(),
        uses: Vec::new(),
        resources: Vec::new(),
        style_resources: Vec::new(),
        base,
        out: String::new(),
        refs: Vec::new(),
        images: Vec::new(),
        viewports: Vec::new(),
        made_paths: String::new(),
        made_count: 0,
        serialized: HashMap::new(),
    };
    // (the outermost element inherits from the HTML around it: all of its properties are written)
    w.element(root, None, true);
    let mut defs = String::new();
    write_instances(&mut w, &mut defs);
    write_referenced(&mut w, root, &mut defs);
    defs.push_str(&w.made_paths);
    write_resources(&mut w, &mut defs);
    if !defs.is_empty() {
        // (before the closing tag of the root)
        let close = w.out.rfind("</").unwrap_or(w.out.len());
        w.out.insert_str(close, &format!("<defs>{defs}</defs>"));
    }
    Serialized {
        markup: w.out,
        images: w.images,
        uses: w.uses,
        resources: w.resources,
        style_resources: w.style_resources,
    }
}

/// Writes the elements outside `root` that what `w` wrote refers to, and those they refer to, into
/// `defs`.
fn write_referenced(w: &mut Writer<'_>, root: NodeId, defs: &mut String) {
    let (dom, data) = (w.dom, w.data);
    let mut ids: HashMap<String, NodeId> = HashMap::new();
    let mut wanted: Vec<String> = std::mem::take(&mut w.refs);
    let mut done: HashSet<String> = HashSet::new();
    let mut copied = 0;
    // (references resolve in the svg's own tree: its shadow tree, or the document)
    let doc = dom.root(root);
    while let Some(id) = wanted.pop() {
        if !done.insert(id.clone()) || copied >= MAX_REFERENCED {
            continue;
        }
        if ids.is_empty() {
            for n in dom.descendants(doc).filter(|&n| dom.is_element(n)) {
                if let Some(v) = dom.attr(n, &ns!(), &local_name!("id")) {
                    ids.entry(v.to_owned()).or_insert(n);
                }
            }
        }
        let Some(&target) = ids.get(&id) else {
            continue;
        };
        if dom.is_inclusive_ancestor_of(root, target) {
            continue;
        }
        copied += 1;
        // (inherited values come from where it is used: only what it sets itself is written)
        let parent = dom.parent_element(target).and_then(|p| data.styles(p));
        let before = w.out.len();
        w.element(target, parent, false);
        defs.push_str(&w.out[before..]);
        w.out.truncate(before);
        write_instances(w, defs);
        wanted.append(&mut w.refs);
    }
}

/// A `clipPath` element as a document with one shape that uses it, 1 by 1 so that a clip in
/// `objectBoundingBox` units keeps them (see [`crate::clip_ref`]). Percentages in its content are
/// of the nearest `svg` around it (its `viewBox`, else its `width` and `height` attributes, 300 by
/// 150 without them), as the reference browser resolves them, whatever the element using the clip.
pub(crate) fn clip_path_markup(
    dom: &Dom,
    data: &StyleData,
    clip: NodeId,
    base: &Url,
    external: &External<'_>,
) -> String {
    let viewport = svg_parent(dom, clip).map_or((300.0, 150.0), |svg| {
        let vb = dom
            .get_attribute(svg, "viewBox")
            .map(numbers)
            .filter(|n| n.len() == 4 && n[2] > 0.0 && n[3] > 0.0);
        match vb {
            Some(n) => (n[2], n[3]),
            None => (
                user_length(dom, svg, "width").unwrap_or(300.0),
                user_length(dom, svg, "height").unwrap_or(150.0),
            ),
        }
    });
    let mut w = Writer {
        dom,
        data,
        external,
        own_url: external.document,
        prefix: None,
        scoped: None,
        instances: Vec::new(),
        top: None,
        wanted: Vec::new(),
        uses: Vec::new(),
        resources: Vec::new(),
        style_resources: Vec::new(),
        base,
        out: String::new(),
        refs: Vec::new(),
        images: Vec::new(),
        viewports: Vec::new(),
        made_paths: String::new(),
        made_count: 0,
        serialized: HashMap::new(),
    };
    let parent = dom.parent_element(clip).and_then(|p| data.styles(p));
    w.element(clip, parent, false);
    let mut defs = String::new();
    write_instances(&mut w, &mut defs);
    write_referenced(&mut w, clip, &mut defs);
    defs.push_str(&w.made_paths);
    write_resources(&mut w, &mut defs);
    let mut id = String::new();
    escape_into(
        &mut id,
        dom.attr(clip, &ns!(), &local_name!("id")).unwrap_or(""),
        true,
    );
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         width=\"{}\" height=\"{}\"><defs>{}{defs}</defs>\
         <rect width=\"1\" height=\"1\" clip-path=\"url(#{id})\"/></svg>",
        viewport.0.max(1.0),
        viewport.1.max(1.0),
        w.out
    )
}

/// The element an `feImage` refers to (Filter Effects 1 §9.15), as a document that draws it with
/// the style it computes where it is (all its properties written: what it inherits there too),
/// shown even in a hidden container. The document has no size: drawn into the primitive
/// subregion, its user space starts at the subregion's corner, one unit a CSS px.
pub(crate) fn fe_image_markup(
    dom: &Dom,
    data: &StyleData,
    el: NodeId,
    base: &Url,
    external: &External<'_>,
) -> String {
    let mut w = Writer {
        dom,
        data,
        external,
        own_url: external.document,
        prefix: None,
        scoped: None,
        instances: Vec::new(),
        top: Some((el, None)),
        wanted: Vec::new(),
        uses: Vec::new(),
        resources: Vec::new(),
        style_resources: Vec::new(),
        base,
        out: String::new(),
        refs: Vec::new(),
        images: Vec::new(),
        viewports: Vec::new(),
        made_paths: String::new(),
        made_count: 0,
        serialized: HashMap::new(),
    };
    w.element(el, None, false);
    let mut defs = String::new();
    write_instances(&mut w, &mut defs);
    write_referenced(&mut w, el, &mut defs);
    defs.push_str(&w.made_paths);
    write_resources(&mut w, &mut defs);
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\">\
         <defs>{defs}</defs>{}</svg>",
        w.out
    )
}

/// Writes the instances of the `use` elements `w` met into `defs`: each clone under ids of its
/// own, with its computed values, which inherit from the `use` (SVG 2 §5.6).
fn write_instances(w: &mut Writer<'_>, defs: &mut String) {
    while let Some((host, clone, id)) = w.instances.pop() {
        let dom = w.dom;
        let scoped: HashSet<String> = dom
            .inclusive_descendants(clone)
            .filter(|&n| dom.is_element(n))
            .filter_map(|n| dom.attr(n, &ns!(), &local_name!("id")).map(str::to_owned))
            .collect();
        let mut sub = Writer {
            dom,
            data: w.data,
            external: w.external,
            own_url: w.own_url,
            prefix: Some(format!("{id}-")),
            scoped: Some(scoped),
            instances: Vec::new(),
            top: Some((clone, Some(id))),
            wanted: Vec::new(),
            uses: Vec::new(),
            resources: Vec::new(),
            style_resources: Vec::new(),
            base: w.base,
            out: String::new(),
            refs: Vec::new(),
            images: Vec::new(),
            made_paths: String::new(),
            made_count: 0,
            viewports: w.viewports.clone(),
            serialized: HashMap::new(),
        };
        sub.element(clone, w.data.styles(host), false);
        defs.push_str(&sub.out);
        defs.push_str(&sub.made_paths);
        let refs = std::mem::take(&mut sub.refs);
        let outside: Vec<String> = refs.into_iter().filter(|r| !sub.owns(r)).collect();
        w.refs.extend(outside);
        w.instances.append(&mut sub.instances);
        w.wanted.append(&mut sub.wanted);
        w.images.append(&mut sub.images);
        w.resources.append(&mut sub.resources);
        w.style_resources.append(&mut sub.style_resources);
    }
}

/// Writes the elements of other documents that `w` wanted into `defs`, and what they refer to in
/// turn.
fn write_resources(w: &mut Writer<'_>, defs: &mut String) {
    let mut wanted = std::mem::take(&mut w.wanted);
    let mut done: HashSet<(String, Option<String>)> = HashSet::new();
    let mut written: Vec<(String, NodeId)> = Vec::new();
    while let Some((key, fragment)) = wanted.pop() {
        if done.len() >= MAX_REFERENCED || !done.insert((key.clone(), fragment.clone())) {
            continue;
        }
        let Some(SvgResource::Loaded(r)) = w.external.resources.get(&key) else {
            continue;
        };
        let Some(target) = r.target(fragment.as_deref()) else {
            continue;
        };
        // (already there, as part of an element written)
        if written
            .iter()
            .any(|(k, n)| *k == key && r.dom.is_inclusive_ancestor_of(*n, target))
        {
            continue;
        }
        written.push((key.clone(), target));
        let prefix = w.external.prefix(&key);
        let forced = fragment.is_none().then(|| format!("{prefix}root"));
        let mut sub = Writer {
            dom: &r.dom,
            data: &r.data,
            external: w.external,
            own_url: &r.url,
            prefix: Some(prefix),
            scoped: None,
            instances: Vec::new(),
            top: Some((target, forced)),
            wanted: Vec::new(),
            uses: Vec::new(),
            resources: Vec::new(),
            style_resources: Vec::new(),
            base: &r.url,
            out: String::new(),
            refs: Vec::new(),
            images: Vec::new(),
            made_paths: String::new(),
            made_count: 0,
            serialized: HashMap::new(),
            viewports: w.viewports.clone(),
        };
        // (inherited values come from the `use`: only what the element sets itself is written)
        let parent = r
            .dom
            .parent_element(target)
            .or(r.initial)
            .and_then(|p| r.data.styles(p));
        sub.element(target, parent, false);
        defs.push_str(&sub.out);
        defs.push_str(&sub.made_paths);
        wanted.append(&mut sub.wanted);
        wanted.extend(sub.refs.drain(..).map(|id| (key.clone(), Some(id))));
        w.instances.append(&mut sub.instances);
        w.images.append(&mut sub.images);
        w.resources.append(&mut sub.resources);
        w.style_resources.append(&mut sub.style_resources);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_references() {
        let mut v = Vec::new();
        fragment_refs("#icon", &mut v);
        fragment_refs("url(#g) none", &mut v);
        fragment_refs("url(\"#q\")", &mut v);
        fragment_refs("https://x/#nope", &mut v);
        fragment_refs("red", &mut v);
        assert_eq!(v, ["icon", "g", "q"]);
        assert_eq!(svg_value("url(\"#g\") none"), "url(#g) none");
        assert_eq!(svg_value("url(\"file:///C:/x/page.html#g\")"), "url(#g)");
    }

    #[test]
    fn initial_values_skip() {
        // A non-inherited property whose computed value is the initial value is skipped without
        // being serialized; its serialization must be one `is_initial_serialization` skips too
        // (and must not depend on `color`: no initial value here is `currentcolor`).
        let initial = snowlark_style::computed::initial_values();
        for (i, &(name, inherited)) in PROPERTIES.iter().enumerate() {
            let Some(id) = &ids().copied[i] else {
                continue;
            };
            if !inherited {
                let v = snowlark_style::computed::computed_property(initial, id);
                assert!(is_initial_serialization(name, &v), "{name}: {v}");
            }
        }
    }

    #[test]
    fn modern_colors_are_written_in_srgb() {
        assert_eq!(srgb_colors("rgb(1, 2, 3)"), None);
        assert_eq!(srgb_colors("none"), None);
        assert_eq!(
            srgb_colors("lch(30 0.00000357628 none)").as_deref(),
            Some("rgb(71, 71, 71)")
        );
        assert_eq!(
            srgb_colors("color(srgb 1 0 0 / 0.5)").as_deref(),
            Some("rgba(255, 0, 0, 0.5019608)")
        );
        assert_eq!(
            srgb_colors("url(\"#lab(x)\") oklch(0.628 0.2577 29.23)").as_deref(),
            Some("url(\"#lab(x)\") rgb(255, 0, 0)")
        );
        // (`currentcolor` is not known here)
        assert_eq!(srgb_colors("color-mix(in srgb, currentcolor, red)"), None);
    }

    #[test]
    fn references_of_a_resource_keep_to_its_copies() {
        let all = |_: &str| true;
        assert_eq!(prefixed_refs("#a", "r-", &all), "#r-a");
        assert_eq!(prefixed_refs("url(#a) none", "r-", &all), "url(#r-a) none");
        assert_eq!(prefixed_refs("url(\"#a\")", "r-", &all), "url(#r-a)");
        assert_eq!(prefixed_refs("red", "r-", &all), "red");
        assert_eq!(prefixed_refs("#a", "r-", &|id| id == "b"), "#a");
        assert_eq!(percent_decoded("a%20b%C3%A9%"), "a bé%");
    }
}
