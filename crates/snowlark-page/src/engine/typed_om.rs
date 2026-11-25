// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Typed OM Level 1: the `CSSStyleValue` family (§4), the style property maps (§3:
//! `computedStyleMap()`, `attributeStyleMap`, `CSSStyleRule.styleMap`), the `CSS.px()` factories
//! (§4.3.5) and serialization (§6). The numeric algorithms and reification are
//! `snowlark_style::typed`'s; here are the objects, which refer to each other by handle as the
//! spec's internal slots do.

use snowlark_bindings::generated::{
    self as g, CSSImageValueImpl, CSSKeywordValueImpl, CSSMathClampImpl, CSSMathInvertImpl,
    CSSMathMaxImpl, CSSMathMinImpl, CSSMathNegateImpl, CSSMathOperator, CSSMathProductImpl,
    CSSMathSumImpl, CSSMathValueImpl, CSSMatrixComponentImpl, CSSNumericArrayImpl,
    CSSNumericBaseType, CSSNumericType, CSSNumericValueImpl,
    CSSNumericValueOrStringOrCSSKeywordValue, CSSPerspectiveImpl, CSSRotateImpl, CSSScaleImpl,
    CSSSkewImpl, CSSSkewXImpl, CSSSkewYImpl, CSSStyleValueImpl, CSSStyleValueOrString,
    CSSTransformComponentImpl, CSSTransformValueImpl, CSSTranslateImpl, CSSUnitValueImpl,
    CSSUnparsedValueImpl, CSSVariableReferenceValueImpl, DoubleOrCSSNumericValue,
    StringOrCSSVariableReferenceValue, StylePropertyMapImpl, StylePropertyMapReadOnlyImpl,
    UndefinedOrCSSStyleValue,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;
use snowlark_style::typed::{self as typed, Base, Comp, Num, NumError, Reified, Seg, Typed};

use super::matrix::Matrix;
use super::{exception, Engine, Object};

/// Which declarations a style property map reflects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapKind {
    /// `element.computedStyleMap()`: the element's computed values.
    Computed(NodeId),
    /// `element.attributeStyleMap`: the element's inline declarations.
    Inline(NodeId),
    /// `CSSStyleRule.styleMap`: the declarations of the rule object given.
    Rule(Handle),
}

/// A segment of a `CSSUnparsedValue`.
#[derive(Clone, Debug)]
pub enum Segment {
    Str(String),
    /// A `CSSVariableReferenceValue`.
    Var(Handle),
}

/// A CSS Typed OM object: a `CSSStyleValue` (or one of its parts) or a style property map.
#[derive(Clone, Debug)]
pub enum Tom {
    /// A `CSSStyleValue` of no subclass (§5.2): the value it represents, as serialized, and its
    /// `[[associatedProperty]]`.
    Style {
        property: String,
        css: String,
    },
    /// A `CSSImageValue`, opaque like the above.
    Image {
        property: String,
        css: String,
    },
    Keyword(String),
    Unit(f64, String),
    /// `CSSMathSum`, `CSSMathProduct`, `CSSMathMin`, `CSSMathMax`: their `CSSNumericArray`.
    Sum(Handle),
    Product(Handle),
    Min(Handle),
    Max(Handle),
    Negate(Handle),
    Invert(Handle),
    Clamp([Handle; 3]),
    /// A `CSSNumericArray`.
    Array(Vec<Handle>),
    Unparsed(Vec<Segment>),
    /// A `CSSVariableReferenceValue`: the custom property and its fallback `CSSUnparsedValue`.
    VarRef(String, Option<Handle>),
    Transform(Vec<Handle>),
    /// x, y, z and is2D.
    Translate([Handle; 3], bool),
    /// x, y, z, angle and is2D.
    Rotate([Handle; 4], bool),
    Scale([Handle; 3], bool),
    Skew([Handle; 2]),
    SkewX(Handle),
    SkewY(Handle),
    /// The length: a `CSSNumericValue` or a `CSSKeywordValue`.
    Perspective(Handle),
    /// The `DOMMatrix` and is2D.
    Matrix(Handle, bool),
    Map(MapKind),
}

impl Tom {
    /// The interface the object's wrapper is made with.
    pub fn interface(&self) -> InterfaceId {
        match self {
            Tom::Style { .. } => InterfaceId::CSSStyleValue,
            Tom::Image { .. } => InterfaceId::CSSImageValue,
            Tom::Keyword(_) => InterfaceId::CSSKeywordValue,
            Tom::Unit(..) => InterfaceId::CSSUnitValue,
            Tom::Sum(_) => InterfaceId::CSSMathSum,
            Tom::Product(_) => InterfaceId::CSSMathProduct,
            Tom::Min(_) => InterfaceId::CSSMathMin,
            Tom::Max(_) => InterfaceId::CSSMathMax,
            Tom::Negate(_) => InterfaceId::CSSMathNegate,
            Tom::Invert(_) => InterfaceId::CSSMathInvert,
            Tom::Clamp(_) => InterfaceId::CSSMathClamp,
            Tom::Array(_) => InterfaceId::CSSNumericArray,
            Tom::Unparsed(_) => InterfaceId::CSSUnparsedValue,
            Tom::VarRef(..) => InterfaceId::CSSVariableReferenceValue,
            Tom::Transform(_) => InterfaceId::CSSTransformValue,
            Tom::Translate(..) => InterfaceId::CSSTranslate,
            Tom::Rotate(..) => InterfaceId::CSSRotate,
            Tom::Scale(..) => InterfaceId::CSSScale,
            Tom::Skew(_) => InterfaceId::CSSSkew,
            Tom::SkewX(_) => InterfaceId::CSSSkewX,
            Tom::SkewY(_) => InterfaceId::CSSSkewY,
            Tom::Perspective(_) => InterfaceId::CSSPerspective,
            Tom::Matrix(..) => InterfaceId::CSSMatrixComponent,
            Tom::Map(MapKind::Computed(_)) => InterfaceId::StylePropertyMapReadOnly,
            Tom::Map(_) => InterfaceId::StylePropertyMap,
        }
    }

    /// The element whose wrapper keeps the object (a map of an element).
    pub fn gc_owner(&self) -> Option<NodeId> {
        match self {
            Tom::Map(MapKind::Computed(n)) | Tom::Map(MapKind::Inline(n)) => Some(*n),
            _ => None,
        }
    }

    /// The objects this one refers to.
    pub fn gc_refs(&self, out: &mut Vec<Handle>) {
        match self {
            Tom::Sum(h)
            | Tom::Product(h)
            | Tom::Min(h)
            | Tom::Max(h)
            | Tom::Negate(h)
            | Tom::Invert(h)
            | Tom::SkewX(h)
            | Tom::SkewY(h)
            | Tom::Perspective(h)
            | Tom::Matrix(h, _)
            | Tom::Map(MapKind::Rule(h)) => out.push(*h),
            Tom::Clamp(v) | Tom::Translate(v, _) | Tom::Scale(v, _) => out.extend(v),
            Tom::Rotate(v, _) => out.extend(v),
            Tom::Skew(v) => out.extend(v),
            Tom::Array(v) | Tom::Transform(v) => out.extend(v),
            Tom::Unparsed(segs) => out.extend(segs.iter().filter_map(|s| match s {
                Segment::Var(h) => Some(*h),
                Segment::Str(_) => None,
            })),
            Tom::VarRef(_, fallback) => out.extend(fallback),
            Tom::Style { .. }
            | Tom::Image { .. }
            | Tom::Keyword(_)
            | Tom::Unit(..)
            | Tom::Map(_) => {}
        }
    }
}

fn num_error(e: NumError) -> Error {
    match e {
        NumError::Type => Error::Type("The numeric types cannot be combined".into()),
        NumError::Syntax => exception(ExceptionName::SyntaxError, "Not a valid unit"),
        NumError::Range => Error::Range("Division by zero".into()),
    }
}

fn type_error(message: &'static str) -> Error {
    Error::Type(message.into())
}

/// The list-valued properties (comma-separated repetitions; CSS Typed OM §2 "list-valued property").
fn list_valued(property: &str) -> bool {
    matches!(
        property,
        "animation-composition"
            | "animation-delay"
            | "animation-direction"
            | "animation-duration"
            | "animation-fill-mode"
            | "animation-iteration-count"
            | "animation-name"
            | "animation-play-state"
            | "animation-range-end"
            | "animation-range-start"
            | "animation-timeline"
            | "animation-timing-function"
            | "background-attachment"
            | "background-blend-mode"
            | "background-clip"
            | "background-image"
            | "background-origin"
            | "background-position"
            | "background-position-x"
            | "background-position-y"
            | "background-repeat"
            | "background-size"
            | "box-shadow"
            | "mask-clip"
            | "mask-composite"
            | "mask-image"
            | "mask-mode"
            | "mask-origin"
            | "mask-position"
            | "mask-repeat"
            | "mask-size"
            | "scroll-timeline-axis"
            | "scroll-timeline-name"
            | "text-shadow"
            | "transition-behavior"
            | "transition-delay"
            | "transition-duration"
            | "transition-property"
            | "transition-timing-function"
            | "view-timeline-axis"
            | "view-timeline-inset"
            | "view-timeline-name"
            | "will-change"
    )
}

fn is_css_wide(s: &str) -> bool {
    ["initial", "inherit", "unset", "revert", "revert-layer"]
        .iter()
        .any(|k| s.trim().eq_ignore_ascii_case(k))
}

/// "If property is not a custom property name string, set property to property ASCII lowercased.
/// If property is not a valid CSS property, throw a TypeError."
fn map_property(property: &str) -> rt::Result<String> {
    let p = if property.starts_with("--") {
        property.to_owned()
    } else {
        property.to_ascii_lowercase()
    };
    if snowlark_style::computed::property_id(&p).is_none() {
        return Err(type_error("Not a valid CSS property"));
    }
    Ok(p)
}

fn supports(property: &str, css: &str) -> bool {
    snowlark_style::supports_declaration(property, css)
}

impl Engine {
    fn new_tom(&self, t: Tom) -> Handle {
        self.alloc(Object::Typed(Box::new(t)))
    }

    /// The object (a shallow copy: its parts are handles).
    fn tom(&self, h: Handle) -> Tom {
        match &*self.obj(h) {
            Object::Typed(t) => (**t).clone(),
            _ => panic!("not a CSS Typed OM object"),
        }
    }

    fn tom_mut(&self, h: Handle, f: impl FnOnce(&mut Tom)) {
        if let Object::Typed(t) = &mut *self.obj_mut(h) {
            f(t);
        }
    }

    fn array(&self, h: Handle) -> Vec<Handle> {
        match self.tom(h) {
            Tom::Array(v) => v,
            _ => Vec::new(),
        }
    }

    fn is_numeric(&self, h: Handle) -> bool {
        matches!(
            self.tom(h),
            Tom::Unit(..)
                | Tom::Sum(_)
                | Tom::Product(_)
                | Tom::Min(_)
                | Tom::Max(_)
                | Tom::Negate(_)
                | Tom::Invert(_)
                | Tom::Clamp(_)
        )
    }

    /// A `CSSNumericValue` read into a tree.
    pub(crate) fn num(&self, h: Handle) -> Num {
        let list = |a: Handle| self.array(a).into_iter().map(|h| self.num(h)).collect();
        match self.tom(h) {
            Tom::Unit(v, u) => Num::Unit(v, u),
            Tom::Sum(a) => Num::Sum(list(a)),
            Tom::Product(a) => Num::Product(list(a)),
            Tom::Min(a) => Num::Min(list(a)),
            Tom::Max(a) => Num::Max(list(a)),
            Tom::Negate(v) => Num::Negate(Box::new(self.num(v))),
            Tom::Invert(v) => Num::Invert(Box::new(self.num(v))),
            Tom::Clamp([l, v, u]) => Num::Clamp(Box::new([self.num(l), self.num(v), self.num(u)])),
            _ => Num::number(f64::NAN),
        }
    }

    /// New objects for a tree.
    fn alloc_num(&self, n: &Num) -> Handle {
        let list = |v: &[Num]| {
            let items = v.iter().map(|n| self.alloc_num(n)).collect();
            self.new_tom(Tom::Array(items))
        };
        let t = match n {
            Num::Unit(v, u) => Tom::Unit(*v, u.clone()),
            Num::Sum(v) => Tom::Sum(list(v)),
            Num::Product(v) => Tom::Product(list(v)),
            Num::Min(v) => Tom::Min(list(v)),
            Num::Max(v) => Tom::Max(list(v)),
            Num::Negate(v) => Tom::Negate(self.alloc_num(v)),
            Num::Invert(v) => Tom::Invert(self.alloc_num(v)),
            Num::Clamp(v) => Tom::Clamp([
                self.alloc_num(&v[0]),
                self.alloc_num(&v[1]),
                self.alloc_num(&v[2]),
            ]),
        };
        self.new_tom(t)
    }

    /// "Rectify a numberish value", keeping the object given.
    fn rectify(&self, v: DoubleOrCSSNumericValue) -> Handle {
        match v {
            DoubleOrCSSNumericValue::Double(d) => self.new_tom(Tom::Unit(d, "number".into())),
            DoubleOrCSSNumericValue::CSSNumericValue(h) => h,
        }
    }

    fn rectify_num(&self, v: DoubleOrCSSNumericValue) -> Num {
        match v {
            DoubleOrCSSNumericValue::Double(d) => Num::number(d),
            DoubleOrCSSNumericValue::CSSNumericValue(h) => self.num(h),
        }
    }

    /// A math value of `args` (rectified) whose type is checked by `kind` (§4.3.4 constructors).
    fn math_list(
        &self,
        args: Vec<DoubleOrCSSNumericValue>,
        make: fn(Handle) -> Tom,
        tree: fn(Vec<Num>) -> Num,
    ) -> rt::Result<Handle> {
        let items: Vec<Handle> = args.into_iter().map(|a| self.rectify(a)).collect();
        if items.is_empty() {
            return Err(exception(
                ExceptionName::SyntaxError,
                "A math value needs at least one argument",
            ));
        }
        tree(items.iter().map(|h| self.num(*h)).collect())
            .checked()
            .map_err(num_error)?;
        let array = self.new_tom(Tom::Array(items));
        Ok(self.new_tom(make(array)))
    }

    /// A `CSSNumberish` time in milliseconds (Web Animations 2 §4: a `CSSNumericValue` must convert
    /// to `ms`, a TypeError otherwise).
    pub(crate) fn numberish_ms(&self, v: DoubleOrCSSNumericValue) -> rt::Result<f64> {
        match v {
            DoubleOrCSSNumericValue::Double(d) => Ok(d),
            DoubleOrCSSNumericValue::CSSNumericValue(h) => match self.num(h).to("ms") {
                Ok(Num::Unit(v, _)) => Ok(v),
                _ => Err(type_error("The time cannot be converted to milliseconds")),
            },
        }
    }

    /// Whether a `CSSNumericValue` is a `<length-percentage>`.
    pub(crate) fn is_length_percentage(&self, h: Handle) -> bool {
        self.expect_length_percentage(h).is_ok()
    }

    /// §6 serialization of a `CSSStyleValue`.
    pub(crate) fn style_value_css(&self, h: Handle) -> String {
        match self.tom(h) {
            Tom::Style { css, .. } | Tom::Image { css, .. } => css,
            // (§6.2: the value serialized as an identifier)
            Tom::Keyword(k) => super::cssom::serialize_identifier(&k),
            Tom::Unparsed(segs) => self.segments_css(&segs),
            Tom::VarRef(..) => self.var_ref_css(h),
            Tom::Transform(items) => items
                .iter()
                .map(|c| self.component_css(*c))
                .collect::<Vec<_>>()
                .join(" "),
            Tom::Map(_) | Tom::Array(_) => String::new(),
            Tom::Translate(..)
            | Tom::Rotate(..)
            | Tom::Scale(..)
            | Tom::Skew(_)
            | Tom::SkewX(_)
            | Tom::SkewY(_)
            | Tom::Perspective(_)
            | Tom::Matrix(..) => self.component_css(h),
            _ => self.num(h).serialize(),
        }
    }

    /// §6.1: the segments' tokens, serialized as CSS Syntax serializes a token list (a comment
    /// between two tokens that would otherwise read as one; comments dropped).
    fn segments_css(&self, segs: &[Segment]) -> String {
        let pieces: Vec<String> = segs
            .iter()
            .map(|s| match s {
                Segment::Str(s) => s.clone(),
                Segment::Var(h) => self.var_ref_css(*h),
            })
            .collect();
        typed::serialize_tokens(&pieces)
    }

    fn var_ref_css(&self, h: Handle) -> String {
        let Tom::VarRef(name, fallback) = self.tom(h) else {
            return String::new();
        };
        match fallback {
            Some(f) => format!("var({name},{})", self.style_value_css(f)),
            None => format!("var({name})"),
        }
    }

    fn num_css(&self, h: Handle) -> String {
        self.num(h).serialize()
    }

    /// §6.6 serialization of a transform component.
    fn component_css(&self, h: Handle) -> String {
        match self.tom(h) {
            Tom::Translate([x, y, z], is_2d) => {
                if is_2d {
                    format!("translate({}, {})", self.num_css(x), self.num_css(y))
                } else {
                    format!(
                        "translate3d({}, {}, {})",
                        self.num_css(x),
                        self.num_css(y),
                        self.num_css(z)
                    )
                }
            }
            Tom::Rotate([x, y, z, angle], is_2d) => {
                if is_2d {
                    format!("rotate({})", self.num_css(angle))
                } else {
                    format!(
                        "rotate3d({}, {}, {}, {})",
                        self.num_css(x),
                        self.num_css(y),
                        self.num_css(z),
                        self.num_css(angle)
                    )
                }
            }
            Tom::Scale([x, y, z], is_2d) => {
                if !is_2d {
                    format!(
                        "scale3d({}, {}, {})",
                        self.num_css(x),
                        self.num_css(y),
                        self.num_css(z)
                    )
                } else if self.num(x) == self.num(y) {
                    format!("scale({})", self.num_css(x))
                } else {
                    format!("scale({}, {})", self.num_css(x), self.num_css(y))
                }
            }
            Tom::Skew([ax, ay]) => match self.num(ay) {
                Num::Unit(0.0, _) => format!("skew({})", self.num_css(ax)),
                _ => format!("skew({}, {})", self.num_css(ax), self.num_css(ay)),
            },
            Tom::SkewX(ax) => format!("skewX({})", self.num_css(ax)),
            Tom::SkewY(ay) => format!("skewY({})", self.num_css(ay)),
            Tom::Perspective(length) => {
                let inner = match self.tom(length) {
                    Tom::Keyword(k) => k,
                    _ => self.num(length).serialize_with_min(0.0),
                };
                format!("perspective({inner})")
            }
            Tom::Matrix(m, is_2d) => {
                let mut matrix = self.matrix(m);
                matrix.is_2d = is_2d;
                matrix.to_css_string().unwrap_or_default()
            }
            _ => String::new(),
        }
    }

    fn component_is_2d(&self, h: Handle) -> bool {
        match self.tom(h) {
            Tom::Translate(_, d) | Tom::Rotate(_, d) | Tom::Scale(_, d) | Tom::Matrix(_, d) => d,
            Tom::Perspective(_) => false,
            _ => true,
        }
    }

    /// §4.4 `toMatrix()` of a transform component: a TypeError when a length is not absolute.
    fn component_matrix(&self, h: Handle) -> rt::Result<Matrix> {
        let px = |h: Handle| -> rt::Result<f64> {
            match self.num(h).to("px") {
                Ok(Num::Unit(v, _)) => Ok(v),
                _ => Err(type_error("The length is not absolute")),
            }
        };
        let deg = |h: Handle| -> rt::Result<f64> {
            match self.num(h).to("deg") {
                Ok(Num::Unit(v, _)) => Ok(v),
                _ => Err(type_error("Not an angle")),
            }
        };
        let number = |h: Handle| -> rt::Result<f64> {
            match self.num(h).to("number") {
                Ok(Num::Unit(v, _)) => Ok(v),
                _ => Err(type_error("Not a number")),
            }
        };
        let is_2d = self.component_is_2d(h);
        let mut m = match self.tom(h) {
            Tom::Translate([x, y, z], _) => Matrix::translation(px(x)?, px(y)?, px(z)?),
            Tom::Rotate([x, y, z, angle], d) => {
                if d {
                    Matrix::rotation_z(deg(angle)?)
                } else {
                    Matrix::rotation(number(x)?, number(y)?, number(z)?, deg(angle)?)
                }
            }
            Tom::Scale([x, y, z], _) => Matrix::scaling(number(x)?, number(y)?, number(z)?),
            Tom::Skew([ax, ay]) => Matrix::skew(deg(ax)?, deg(ay)?),
            Tom::SkewX(ax) => Matrix::skew(deg(ax)?, 0.0),
            Tom::SkewY(ay) => Matrix::skew(0.0, deg(ay)?),
            Tom::Perspective(length) => match self.tom(length) {
                Tom::Keyword(_) => Matrix::perspective(f64::INFINITY),
                _ => Matrix::perspective(px(length)?),
            },
            Tom::Matrix(m, _) => self.matrix(m),
            _ => Matrix::identity(),
        };
        // (a component that is 2D gives its 2D matrix: the 3D parts dropped)
        if is_2d {
            let e = m.m;
            m = Matrix::from_2d(e[0][0], e[0][1], e[1][0], e[1][1], e[3][0], e[3][1]);
        }
        m.is_2d = is_2d;
        Ok(m)
    }

    /// Objects for a reified value: one per iteration.
    fn reify(&self, property: &str, r: Reified) -> Vec<Handle> {
        match r {
            Reified::Unsupported(css) => vec![self.new_tom(Tom::Style {
                property: property.to_owned(),
                css,
            })],
            Reified::Typed(values) => values
                .into_iter()
                .map(|t| self.alloc_typed(property, t))
                .collect(),
        }
    }

    fn alloc_typed(&self, property: &str, t: Typed) -> Handle {
        match t {
            Typed::Keyword(k) => self.new_tom(Tom::Keyword(k)),
            Typed::Numeric(n) => self.alloc_num(&n),
            Typed::Unparsed(segs) => self.alloc_unparsed(segs),
            Typed::Transform(comps) => {
                let items = comps.into_iter().map(|c| self.alloc_comp(c)).collect();
                self.new_tom(Tom::Transform(items))
            }
            Typed::Image(css) => self.new_tom(Tom::Image {
                property: property.to_owned(),
                css,
            }),
        }
    }

    fn alloc_unparsed(&self, segs: Vec<Seg>) -> Handle {
        let segs = segs
            .into_iter()
            .map(|s| match s {
                Seg::Str(s) => Segment::Str(s),
                Seg::Var(name, fallback) => {
                    let fallback = fallback.map(|f| self.alloc_unparsed(f));
                    Segment::Var(self.new_tom(Tom::VarRef(name, fallback)))
                }
            })
            .collect();
        self.new_tom(Tom::Unparsed(segs))
    }

    fn alloc_comp(&self, c: Comp) -> Handle {
        let t = match c {
            Comp::Translate(v, d) => Tom::Translate(v.map(|n| self.alloc_num(&n)), d),
            Comp::Rotate(v, d) => Tom::Rotate(v.map(|n| self.alloc_num(&n)), d),
            Comp::Scale(v, d) => Tom::Scale(v.map(|n| self.alloc_num(&n)), d),
            Comp::Skew(ax, ay) => Tom::Skew([self.alloc_num(&ax), self.alloc_num(&ay)]),
            Comp::SkewX(ax) => Tom::SkewX(self.alloc_num(&ax)),
            Comp::SkewY(ay) => Tom::SkewY(self.alloc_num(&ay)),
            Comp::Perspective(Ok(n)) => Tom::Perspective(self.alloc_num(&n)),
            Comp::Perspective(Err(k)) => Tom::Perspective(self.new_tom(Tom::Keyword(k))),
            Comp::Matrix(v, d) => {
                let mut m = Matrix::from_3d(&v);
                m.is_2d = d;
                Tom::Matrix(self.new_matrix(m, false), d)
            }
        };
        self.new_tom(t)
    }

    fn map_kind(&self, h: Handle) -> MapKind {
        match self.tom(h) {
            Tom::Map(k) => k,
            _ => panic!("not a StylePropertyMap"),
        }
    }

    /// A map's value of `property` reified, `None` when the map has no entry for it.
    fn map_value(&self, kind: MapKind, property: &str) -> Option<Reified> {
        match kind {
            MapKind::Inline(el) => self.with_inline(el, |b| b.typed(property)),
            MapKind::Rule(r) => self.with_rule_decls(r, |b| (b.typed(property), false)),
            MapKind::Computed(el) => self
                .with_style(el, |r| r.typed_computed_property(el, property))
                .flatten(),
        }
    }

    /// The properties a map has entries for, in its order.
    fn map_names(&self, kind: MapKind) -> Vec<String> {
        match kind {
            MapKind::Inline(el) => self.with_inline(el, |b| b.property_names()),
            MapKind::Rule(r) => self.with_rule_decls(r, |b| (b.property_names(), false)),
            // (§3: standard properties, then prefixed ones, each in code-point order, then the
            // custom properties)
            MapKind::Computed(el) => {
                let mut names: Vec<String> = snowlark_style::computed::computed_property_names()
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                names.sort_by(|a, b| {
                    (a.starts_with('-'), a.as_str()).cmp(&(b.starts_with('-'), b.as_str()))
                });
                names.extend(
                    self.with_style(el, |r| r.computed_custom_property_names(el))
                        .unwrap_or_default(),
                );
                names
            }
        }
    }

    /// The declared value of `property` in an inline or rule map, as serialized.
    fn map_declared_css(&self, kind: MapKind, property: &str) -> String {
        match kind {
            MapKind::Inline(el) => self.with_inline(el, |b| b.property_value(property)),
            MapKind::Rule(r) => self.with_rule_decls(r, |b| (b.property_value(property), false)),
            MapKind::Computed(_) => String::new(),
        }
    }

    /// Sets (or, with an empty value, removes) `property` in an inline or rule map.
    fn map_write(&self, cx: &mut Cx, kind: MapKind, property: &str, css: &str) {
        match kind {
            MapKind::Inline(el) => {
                let changed = self.with_inline(el, |b| {
                    if css.is_empty() {
                        b.remove_property(property).1
                    } else {
                        b.set_property(property, css, "")
                    }
                });
                if changed {
                    self.write_inline(cx, el);
                }
            }
            MapKind::Rule(r) => self.with_rule_decls(r, |b| {
                let changed = if css.is_empty() {
                    b.remove_property(property).1
                } else {
                    b.set_property(property, css, "")
                };
                ((), changed)
            }),
            MapKind::Computed(_) => {}
        }
    }

    /// "Create an internal representation" of a `CSSStyleValue` for `property`, as CSS text: a
    /// TypeError when it does not match the property's grammar. A `CSSUnitValue` outside the
    /// property's range is wrapped in `calc()`.
    fn internal_css(&self, property: &str, h: Handle) -> rt::Result<String> {
        let mismatch = || type_error("The value does not match the property's grammar");
        let custom = property.starts_with("--");
        // (what a property takes as a subclass, §5.1; a custom property takes tokens only)
        let takes = match self.tom(h) {
            Tom::Style { .. } | Tom::Image { .. } | Tom::Unparsed(_) => true,
            Tom::Keyword(k) => !custom || is_css_wide(&k),
            Tom::Transform(_) => {
                !custom && typed::takes(property, snowlark_style::typed_props::TRANSFORM)
            }
            _ => {
                !custom
                    && self
                        .num(h)
                        .ty()
                        .is_some_and(|t| typed::takes_numeric(property, &t))
            }
        };
        if !takes {
            return Err(mismatch());
        }
        let css = match self.tom(h) {
            Tom::Style { property: p, css } | Tom::Image { property: p, css } => {
                if p != property {
                    return Err(type_error("The value is for another property"));
                }
                return Ok(css);
            }
            Tom::Unit(v, u) => {
                let plain = self.style_value_css(h);
                let wrapped = format!("calc({plain})");
                // (a `<number>` is checked as `calc()`, which no `<length>` grammar takes for a
                // unitless zero)
                if u == "number" && !supports(property, &wrapped) {
                    return Err(mismatch());
                }
                if v.is_finite() && supports(property, &plain) {
                    plain
                } else if supports(property, &wrapped) {
                    wrapped
                } else {
                    return Err(mismatch());
                }
            }
            _ => self.style_value_css(h),
        };
        if supports(property, &css) {
            Ok(css)
        } else {
            Err(mismatch())
        }
    }

    fn is_unparsed(&self, h: Handle) -> bool {
        matches!(self.tom(h), Tom::Unparsed(_))
    }

    /// `set()` and `append()` (§3.3).
    fn map_set(
        &self,
        cx: &mut Cx,
        this: Handle,
        property: &str,
        values: Vec<CSSStyleValueOrString>,
        append: bool,
    ) -> rt::Result<()> {
        let kind = self.map_kind(this);
        let property = map_property(property)?;
        let list = list_valued(&property);
        if append && !list {
            return Err(type_error("The property is not list-valued"));
        }
        if !append && values.len() > 1 && !list {
            return Err(type_error("The property takes one value"));
        }
        let mut texts = Vec::with_capacity(values.len());
        for v in &values {
            let text = match v {
                CSSStyleValueOrString::String(s) => {
                    if !supports(&property, s) {
                        return Err(type_error("The value does not parse for the property"));
                    }
                    if (append || values.len() > 1) && s.contains("var(") {
                        return Err(type_error("A var() reference cannot be one of several"));
                    }
                    s.clone()
                }
                CSSStyleValueOrString::CSSStyleValue(h) => {
                    if (append || values.len() > 1) && self.is_unparsed(*h) {
                        return Err(type_error("A var() reference cannot be one of several"));
                    }
                    self.internal_css(&property, *h)?
                }
            };
            // (a list's iterations are never CSS-wide keywords)
            if (append || values.len() > 1) && is_css_wide(&text) {
                return Err(type_error("A CSS-wide keyword cannot be a list item"));
            }
            texts.push(text);
        }
        if append {
            if texts.is_empty() {
                return Ok(());
            }
            let existing = self.map_declared_css(kind, &property);
            if is_css_wide(&existing) || existing.contains("var(") {
                return Err(type_error("Cannot append to this value"));
            }
            if !existing.is_empty() {
                texts.insert(0, existing);
            }
        }
        let css = texts.join(", ");
        if !css.is_empty() && !supports(&property, &css) {
            return Err(type_error("The values do not parse for the property"));
        }
        self.map_write(cx, kind, &property, &css);
        Ok(())
    }

    /// "Parse a CSSStyleValue" (§2): the iterations reified, a TypeError when `css` does not parse
    /// for `property`.
    fn parse_style_value(&self, property: &str, css: &str) -> rt::Result<Vec<Handle>> {
        let property = map_property(property)?;
        let base = url::Url::parse("about:blank").expect("valid URL");
        let mut block = snowlark_style::DeclarationBlock::new(&base);
        if !block.set_property(&property, css, "") {
            return Err(type_error("The value does not parse for the property"));
        }
        let r = block
            .typed(&property)
            .unwrap_or_else(|| Reified::Unsupported(css.to_owned()));
        // (a value with var() references is its tokens, a shorthand's too)
        if let Reified::Unsupported(_) = r {
            if let Some(segs) = css
                .contains("var(")
                .then(|| typed::parse_unparsed(css))
                .flatten()
            {
                return Ok(vec![self.alloc_unparsed(segs)]);
            }
        }
        Ok(self.reify(&property, r))
    }

    /// `element.computedStyleMap()`: one map per element ([SameObject]).
    pub(crate) fn computed_style_map(&self, el: NodeId) -> Handle {
        if let Some(h) = self.cssom.borrow().computed_maps.get(&el) {
            return *h;
        }
        let h = self.new_tom(Tom::Map(MapKind::Computed(el)));
        self.cssom.borrow_mut().computed_maps.insert(el, h);
        h
    }

    /// A map was freed: its element's cache entry goes.
    pub(crate) fn typed_om_freed(&self, t: &Tom) {
        if let Tom::Map(MapKind::Computed(el)) = t {
            self.cssom.borrow_mut().computed_maps.remove(el);
        }
    }

    pub(crate) fn attribute_style_map(&self, el: NodeId) -> Handle {
        self.new_tom(Tom::Map(MapKind::Inline(el)))
    }

    pub(crate) fn rule_style_map(&self, rule: Handle) -> Handle {
        self.new_tom(Tom::Map(MapKind::Rule(rule)))
    }

    fn unit(&self, value: f64, unit: &str) -> Handle {
        self.new_tom(Tom::Unit(value, unit.to_owned()))
    }

    /// The `CSS.px()` (and other) factories: a `CSSUnitValue` of the unit the factory is named for.
    pub(crate) fn css_factory(&self, value: f64, unit: &str) -> rt::Result<Handle> {
        Ok(self.unit(value, unit))
    }

    fn numeric_op(
        &self,
        this: Handle,
        values: Vec<DoubleOrCSSNumericValue>,
        op: impl FnOnce(Num, Vec<Num>) -> Result<Num, NumError>,
    ) -> rt::Result<Handle> {
        let values = values.into_iter().map(|v| self.rectify_num(v)).collect();
        let result = op(self.num(this), values).map_err(num_error)?;
        Ok(self.alloc_num(&result))
    }

    /// Checks a component's numeric part: a TypeError unless its type matches.
    fn expect_type(&self, h: Handle, ok: impl Fn(&typed::NumType) -> bool) -> rt::Result<()> {
        match self.num(h).ty() {
            Some(t) if ok(&t) => Ok(()),
            _ => Err(type_error("The value has the wrong type")),
        }
    }

    fn expect_length_percentage(&self, h: Handle) -> rt::Result<()> {
        self.expect_type(h, |t| {
            t.matches(Base::Length, true) || t.matches(Base::Percent, false)
        })
    }

    fn expect_length(&self, h: Handle) -> rt::Result<()> {
        self.expect_type(h, |t| t.matches(Base::Length, false))
    }

    fn expect_angle(&self, h: Handle) -> rt::Result<()> {
        self.expect_type(h, |t| t.matches(Base::Angle, false))
    }

    fn expect_number(&self, h: Handle) -> rt::Result<()> {
        self.expect_type(h, |t| t.is_number())
    }

    fn rectified_number(&self, v: DoubleOrCSSNumericValue) -> rt::Result<Handle> {
        let h = self.rectify(v);
        self.expect_number(h)?;
        Ok(h)
    }

    /// The length of a `CSSPerspective`: a `<length>`, or the keyword `none`.
    fn perspective_length(
        &self,
        v: CSSNumericValueOrStringOrCSSKeywordValue,
    ) -> rt::Result<Handle> {
        let keyword = match v {
            CSSNumericValueOrStringOrCSSKeywordValue::CSSNumericValue(h) => {
                self.expect_length(h)?;
                return Ok(h);
            }
            CSSNumericValueOrStringOrCSSKeywordValue::String(s) => self.new_tom(Tom::Keyword(s)),
            CSSNumericValueOrStringOrCSSKeywordValue::CSSKeywordValue(h) => h,
        };
        match self.tom(keyword) {
            Tom::Keyword(k) if k.eq_ignore_ascii_case("none") => Ok(keyword),
            _ => Err(type_error("The perspective must be a length or none")),
        }
    }
}

impl CSSStyleValueImpl for Engine {
    fn stringify(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.style_value_css(this))
    }
    fn static_parse(&self, _cx: &mut Cx, property: &str, css_text: &str) -> rt::Result<Handle> {
        self.parse_style_value(property, css_text)?
            .into_iter()
            .next()
            .ok_or_else(|| type_error("The value does not parse for the property"))
    }
    fn static_parse_all(
        &self,
        _cx: &mut Cx,
        property: &str,
        css_text: &str,
    ) -> rt::Result<Vec<Handle>> {
        self.parse_style_value(property, css_text)
    }
}

impl StylePropertyMapReadOnlyImpl for Engine {
    fn pair_at(&self, this: Handle, index: u32) -> Option<(String, Vec<Handle>)> {
        let kind = self.map_kind(this);
        let name = self.map_names(kind).into_iter().nth(index as usize)?;
        let values = self
            .map_value(kind, &name)
            .map(|r| self.reify(&name, r))
            .unwrap_or_default();
        Some((name, values))
    }
    fn size(&self, this: Handle) -> u32 {
        self.map_names(self.map_kind(this)).len() as u32
    }
    fn get(
        &self,
        _cx: &mut Cx,
        this: Handle,
        property: &str,
    ) -> rt::Result<UndefinedOrCSSStyleValue> {
        let property = map_property(property)?;
        Ok(
            match self
                .map_value(self.map_kind(this), &property)
                .and_then(|r| self.reify(&property, r).into_iter().next())
            {
                Some(h) => UndefinedOrCSSStyleValue::CSSStyleValue(h),
                None => UndefinedOrCSSStyleValue::Undefined(()),
            },
        )
    }
    fn get_all(&self, _cx: &mut Cx, this: Handle, property: &str) -> rt::Result<Vec<Handle>> {
        let property = map_property(property)?;
        Ok(self
            .map_value(self.map_kind(this), &property)
            .map(|r| self.reify(&property, r))
            .unwrap_or_default())
    }
    fn has(&self, _cx: &mut Cx, this: Handle, property: &str) -> rt::Result<bool> {
        let property = map_property(property)?;
        Ok(self.map_value(self.map_kind(this), &property).is_some())
    }
}

impl StylePropertyMapImpl for Engine {
    fn set(
        &self,
        cx: &mut Cx,
        this: Handle,
        property: &str,
        values: Vec<CSSStyleValueOrString>,
    ) -> rt::Result<()> {
        self.map_set(cx, this, property, values, false)
    }
    fn append(
        &self,
        cx: &mut Cx,
        this: Handle,
        property: &str,
        values: Vec<CSSStyleValueOrString>,
    ) -> rt::Result<()> {
        self.map_set(cx, this, property, values, true)
    }
    fn delete(&self, cx: &mut Cx, this: Handle, property: &str) -> rt::Result<()> {
        let property = map_property(property)?;
        self.map_write(cx, self.map_kind(this), &property, "");
        Ok(())
    }
    fn clear(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        // (one change of the style attribute, however many declarations go)
        match self.map_kind(this) {
            MapKind::Inline(el) => {
                let changed = self.with_inline(el, |b| {
                    let had = !b.is_empty();
                    b.set_css_text("");
                    had
                });
                if changed {
                    self.write_inline(cx, el);
                }
            }
            MapKind::Rule(r) => self.with_rule_decls(r, |b| {
                let had = !b.is_empty();
                b.set_css_text("");
                ((), had)
            }),
            MapKind::Computed(_) => {}
        }
        Ok(())
    }
}

impl CSSUnparsedValueImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        match self.tom(this) {
            Tom::Unparsed(s) => s.len() as u32,
            _ => 0,
        }
    }
    fn indexed_getter(
        &self,
        this: Handle,
        index: u32,
    ) -> Option<StringOrCSSVariableReferenceValue> {
        let Tom::Unparsed(segs) = self.tom(this) else {
            return None;
        };
        segs.get(index as usize).map(|s| match s {
            Segment::Str(s) => StringOrCSSVariableReferenceValue::String(s.clone()),
            Segment::Var(h) => StringOrCSSVariableReferenceValue::CSSVariableReferenceValue(*h),
        })
    }
    fn indexed_setter(
        &self,
        _cx: &mut Cx,
        this: Handle,
        index: u32,
        value: StringOrCSSVariableReferenceValue,
    ) -> rt::Result<()> {
        let seg = match value {
            StringOrCSSVariableReferenceValue::String(s) => Segment::Str(s),
            StringOrCSSVariableReferenceValue::CSSVariableReferenceValue(h) => Segment::Var(h),
        };
        let mut result = Ok(());
        self.tom_mut(this, |t| {
            if let Tom::Unparsed(segs) = t {
                let i = index as usize;
                if i < segs.len() {
                    segs[i] = seg;
                } else if i == segs.len() {
                    segs.push(seg);
                } else {
                    result = Err(Error::Range("The index is out of range".into()));
                }
            }
        });
        result
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        members: Vec<StringOrCSSVariableReferenceValue>,
    ) -> rt::Result<Handle> {
        let segs = members
            .into_iter()
            .map(|m| match m {
                StringOrCSSVariableReferenceValue::String(s) => Segment::Str(s),
                StringOrCSSVariableReferenceValue::CSSVariableReferenceValue(h) => Segment::Var(h),
            })
            .collect();
        Ok(self.new_tom(Tom::Unparsed(segs)))
    }
}

impl CSSVariableReferenceValueImpl for Engine {
    fn variable(&self, this: Handle) -> DomStr<'_> {
        match self.tom(this) {
            Tom::VarRef(v, _) => DomStr::Owned(v),
            _ => DomStr::Borrowed(""),
        }
    }
    fn set_variable(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        if !value.starts_with("--") {
            return Err(type_error("Not a custom property name"));
        }
        self.tom_mut(this, |t| {
            if let Tom::VarRef(v, _) = t {
                *v = value.to_owned();
            }
        });
        Ok(())
    }
    fn fallback(&self, this: Handle) -> Option<Handle> {
        match self.tom(this) {
            Tom::VarRef(_, f) => f,
            _ => None,
        }
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        variable: &str,
        fallback: Option<Handle>,
    ) -> rt::Result<Handle> {
        if !variable.starts_with("--") {
            return Err(type_error("Not a custom property name"));
        }
        Ok(self.new_tom(Tom::VarRef(variable.to_owned(), fallback)))
    }
}

impl CSSKeywordValueImpl for Engine {
    fn value(&self, this: Handle) -> DomStr<'_> {
        match self.tom(this) {
            Tom::Keyword(k) => DomStr::Owned(k),
            _ => DomStr::Borrowed(""),
        }
    }
    fn set_value(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        if value.is_empty() {
            return Err(type_error("A keyword cannot be empty"));
        }
        self.tom_mut(this, |t| {
            if let Tom::Keyword(k) = t {
                *k = value.to_owned();
            }
        });
        Ok(())
    }
    fn constructor(&self, _cx: &mut Cx, value: &str) -> rt::Result<Handle> {
        if value.is_empty() {
            return Err(type_error("A keyword cannot be empty"));
        }
        Ok(self.new_tom(Tom::Keyword(value.to_owned())))
    }
}

impl CSSNumericValueImpl for Engine {
    fn add(
        &self,
        _cx: &mut Cx,
        this: Handle,
        values: Vec<DoubleOrCSSNumericValue>,
    ) -> rt::Result<Handle> {
        self.numeric_op(this, values, Num::add_values)
    }
    fn sub(
        &self,
        _cx: &mut Cx,
        this: Handle,
        values: Vec<DoubleOrCSSNumericValue>,
    ) -> rt::Result<Handle> {
        self.numeric_op(this, values, Num::sub_values)
    }
    fn mul(
        &self,
        _cx: &mut Cx,
        this: Handle,
        values: Vec<DoubleOrCSSNumericValue>,
    ) -> rt::Result<Handle> {
        self.numeric_op(this, values, Num::mul_values)
    }
    fn div(
        &self,
        _cx: &mut Cx,
        this: Handle,
        values: Vec<DoubleOrCSSNumericValue>,
    ) -> rt::Result<Handle> {
        self.numeric_op(this, values, Num::div_values)
    }
    fn min(
        &self,
        _cx: &mut Cx,
        this: Handle,
        values: Vec<DoubleOrCSSNumericValue>,
    ) -> rt::Result<Handle> {
        self.numeric_op(this, values, |a, v| a.min_max(v, false))
    }
    fn max(
        &self,
        _cx: &mut Cx,
        this: Handle,
        values: Vec<DoubleOrCSSNumericValue>,
    ) -> rt::Result<Handle> {
        self.numeric_op(this, values, |a, v| a.min_max(v, true))
    }
    fn equals(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: Vec<DoubleOrCSSNumericValue>,
    ) -> rt::Result<bool> {
        let this = self.num(this);
        Ok(value.into_iter().all(|v| self.rectify_num(v) == this))
    }
    fn to(&self, _cx: &mut Cx, this: Handle, unit: &str) -> rt::Result<Handle> {
        let n = self.num(this).to(unit).map_err(num_error)?;
        Ok(self.alloc_num(&n))
    }
    fn to_sum(&self, _cx: &mut Cx, this: Handle, units: Vec<String>) -> rt::Result<Handle> {
        let n = self.num(this).to_sum(&units).map_err(num_error)?;
        Ok(self.alloc_num(&n))
    }
    fn type_(&self, _cx: &mut Cx, this: Handle) -> rt::Result<CSSNumericType> {
        let t = self.num(this).ty().unwrap_or_default();
        let exp = |b: Base| Some(t.exp[b as usize]).filter(|e| *e != 0);
        Ok(CSSNumericType {
            length: exp(Base::Length),
            angle: exp(Base::Angle),
            time: exp(Base::Time),
            frequency: exp(Base::Frequency),
            resolution: exp(Base::Resolution),
            flex: exp(Base::Flex),
            percent: exp(Base::Percent),
            percent_hint: t.hint.map(|b| match b {
                Base::Length => CSSNumericBaseType::Length,
                Base::Angle => CSSNumericBaseType::Angle,
                Base::Time => CSSNumericBaseType::Time,
                Base::Frequency => CSSNumericBaseType::Frequency,
                Base::Resolution => CSSNumericBaseType::Resolution,
                Base::Flex => CSSNumericBaseType::Flex,
                Base::Percent => CSSNumericBaseType::Percent,
            }),
        })
    }
    fn static_parse(&self, _cx: &mut Cx, css_text: &str) -> rt::Result<Handle> {
        let n = typed::parse_numeric(css_text.trim()).map_err(num_error)?;
        Ok(self.alloc_num(&n))
    }
}

impl CSSUnitValueImpl for Engine {
    fn value(&self, this: Handle) -> f64 {
        match self.tom(this) {
            Tom::Unit(v, _) => v,
            _ => f64::NAN,
        }
    }
    fn set_value(&self, _cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.tom_mut(this, |t| {
            if let Tom::Unit(v, _) = t {
                *v = value;
            }
        });
        Ok(())
    }
    fn unit(&self, this: Handle) -> DomStr<'_> {
        match self.tom(this) {
            Tom::Unit(_, u) => DomStr::Owned(u),
            _ => DomStr::Borrowed(""),
        }
    }
    fn constructor(&self, _cx: &mut Cx, value: f64, unit: &str) -> rt::Result<Handle> {
        let unit = typed::normalize_unit(unit).ok_or_else(|| type_error("Not a valid unit"))?;
        Ok(self.unit(value, &unit))
    }
}

impl CSSMathValueImpl for Engine {
    fn operator(&self, this: Handle) -> CSSMathOperator {
        match self.tom(this) {
            Tom::Product(_) => CSSMathOperator::Product,
            Tom::Negate(_) => CSSMathOperator::Negate,
            Tom::Invert(_) => CSSMathOperator::Invert,
            Tom::Min(_) => CSSMathOperator::Min,
            Tom::Max(_) => CSSMathOperator::Max,
            Tom::Clamp(_) => CSSMathOperator::Clamp,
            _ => CSSMathOperator::Sum,
        }
    }
}

fn values_of(t: Tom) -> Handle {
    match t {
        Tom::Sum(a) | Tom::Product(a) | Tom::Min(a) | Tom::Max(a) => a,
        _ => panic!("not a math value with a list"),
    }
}

impl CSSMathSumImpl for Engine {
    fn values(&self, this: Handle) -> Handle {
        values_of(self.tom(this))
    }
    fn constructor(&self, _cx: &mut Cx, args: Vec<DoubleOrCSSNumericValue>) -> rt::Result<Handle> {
        self.math_list(args, Tom::Sum, Num::Sum)
    }
}

impl CSSMathProductImpl for Engine {
    fn values(&self, this: Handle) -> Handle {
        values_of(self.tom(this))
    }
    fn constructor(&self, _cx: &mut Cx, args: Vec<DoubleOrCSSNumericValue>) -> rt::Result<Handle> {
        self.math_list(args, Tom::Product, Num::Product)
    }
}

impl CSSMathMinImpl for Engine {
    fn values(&self, this: Handle) -> Handle {
        values_of(self.tom(this))
    }
    fn constructor(&self, _cx: &mut Cx, args: Vec<DoubleOrCSSNumericValue>) -> rt::Result<Handle> {
        self.math_list(args, Tom::Min, Num::Min)
    }
}

impl CSSMathMaxImpl for Engine {
    fn values(&self, this: Handle) -> Handle {
        values_of(self.tom(this))
    }
    fn constructor(&self, _cx: &mut Cx, args: Vec<DoubleOrCSSNumericValue>) -> rt::Result<Handle> {
        self.math_list(args, Tom::Max, Num::Max)
    }
}

impl CSSMathNegateImpl for Engine {
    fn value(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Negate(v) => v,
            _ => panic!("not a CSSMathNegate"),
        }
    }
    fn constructor(&self, _cx: &mut Cx, arg: DoubleOrCSSNumericValue) -> rt::Result<Handle> {
        let v = self.rectify(arg);
        Ok(self.new_tom(Tom::Negate(v)))
    }
}

impl CSSMathInvertImpl for Engine {
    fn value(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Invert(v) => v,
            _ => panic!("not a CSSMathInvert"),
        }
    }
    fn constructor(&self, _cx: &mut Cx, arg: DoubleOrCSSNumericValue) -> rt::Result<Handle> {
        let v = self.rectify(arg);
        Ok(self.new_tom(Tom::Invert(v)))
    }
}

impl CSSMathClampImpl for Engine {
    fn lower(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Clamp(v) => v[0],
            _ => panic!("not a CSSMathClamp"),
        }
    }
    fn value(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Clamp(v) => v[1],
            _ => panic!("not a CSSMathClamp"),
        }
    }
    fn upper(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Clamp(v) => v[2],
            _ => panic!("not a CSSMathClamp"),
        }
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        lower: DoubleOrCSSNumericValue,
        value: DoubleOrCSSNumericValue,
        upper: DoubleOrCSSNumericValue,
    ) -> rt::Result<Handle> {
        let v = [
            self.rectify(lower),
            self.rectify(value),
            self.rectify(upper),
        ];
        Num::Clamp(Box::new(v.map(|h| self.num(h))))
            .checked()
            .map_err(num_error)?;
        Ok(self.new_tom(Tom::Clamp(v)))
    }
}

impl CSSNumericArrayImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        self.array(this).len() as u32
    }
    fn indexed_getter(&self, this: Handle, index: u32) -> Option<Handle> {
        self.array(this).get(index as usize).copied()
    }
}

impl CSSTransformValueImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        match self.tom(this) {
            Tom::Transform(v) => v.len() as u32,
            _ => 0,
        }
    }
    fn indexed_getter(&self, this: Handle, index: u32) -> Option<Handle> {
        match self.tom(this) {
            Tom::Transform(v) => v.get(index as usize).copied(),
            _ => None,
        }
    }
    fn indexed_setter(
        &self,
        _cx: &mut Cx,
        this: Handle,
        index: u32,
        value: Handle,
    ) -> rt::Result<()> {
        let mut result = Ok(());
        self.tom_mut(this, |t| {
            if let Tom::Transform(items) = t {
                let i = index as usize;
                if i < items.len() {
                    items[i] = value;
                } else if i == items.len() {
                    items.push(value);
                } else {
                    result = Err(Error::Range("The index is out of range".into()));
                }
            }
        });
        result
    }
    fn is2_d(&self, this: Handle) -> bool {
        match self.tom(this) {
            Tom::Transform(v) => v.iter().all(|c| self.component_is_2d(*c)),
            _ => true,
        }
    }
    fn to_matrix(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let Tom::Transform(items) = self.tom(this) else {
            return Ok(self.new_matrix(Matrix::identity(), false));
        };
        let mut m = Matrix::identity();
        for c in items {
            m = m.multiply(&self.component_matrix(c)?);
        }
        Ok(self.new_matrix(m, false))
    }
    fn constructor(&self, _cx: &mut Cx, transforms: Vec<Handle>) -> rt::Result<Handle> {
        if transforms.is_empty() {
            return Err(type_error("A transform needs at least one component"));
        }
        Ok(self.new_tom(Tom::Transform(transforms)))
    }
}

impl CSSTransformComponentImpl for Engine {
    fn stringify(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.component_css(this))
    }
    fn is2_d(&self, this: Handle) -> bool {
        self.component_is_2d(this)
    }
    fn set_is2_d(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        // (skews are always 2D and a perspective never is: setting does nothing)
        self.tom_mut(this, |t| match t {
            Tom::Translate(_, d) | Tom::Rotate(_, d) | Tom::Scale(_, d) | Tom::Matrix(_, d) => {
                *d = value
            }
            _ => {}
        });
        Ok(())
    }
    fn to_matrix(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let m = self.component_matrix(this)?;
        Ok(self.new_matrix(m, false))
    }
}

impl CSSTranslateImpl for Engine {
    fn x(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Translate(v, _) => v[0],
            _ => panic!("not a CSSTranslate"),
        }
    }
    fn set_x(&self, _cx: &mut Cx, this: Handle, value: Handle) -> rt::Result<()> {
        self.expect_length_percentage(value)?;
        self.tom_mut(this, |t| {
            if let Tom::Translate(v, _) = t {
                v[0] = value;
            }
        });
        Ok(())
    }
    fn y(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Translate(v, _) => v[1],
            _ => panic!("not a CSSTranslate"),
        }
    }
    fn set_y(&self, _cx: &mut Cx, this: Handle, value: Handle) -> rt::Result<()> {
        self.expect_length_percentage(value)?;
        self.tom_mut(this, |t| {
            if let Tom::Translate(v, _) = t {
                v[1] = value;
            }
        });
        Ok(())
    }
    fn z(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Translate(v, _) => v[2],
            _ => panic!("not a CSSTranslate"),
        }
    }
    fn set_z(&self, _cx: &mut Cx, this: Handle, value: Handle) -> rt::Result<()> {
        self.expect_length(value)?;
        self.tom_mut(this, |t| {
            if let Tom::Translate(v, _) = t {
                v[2] = value;
            }
        });
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        x: Handle,
        y: Handle,
        z: Option<Handle>,
    ) -> rt::Result<Handle> {
        self.expect_length_percentage(x)?;
        self.expect_length_percentage(y)?;
        if let Some(z) = z {
            self.expect_length(z)?;
        }
        let is_2d = z.is_none();
        let z = z.unwrap_or_else(|| self.unit(0.0, "px"));
        Ok(self.new_tom(Tom::Translate([x, y, z], is_2d)))
    }
}

impl Engine {
    fn rotate_part(&self, this: Handle, i: usize) -> Handle {
        match self.tom(this) {
            Tom::Rotate(v, _) => v[i],
            _ => panic!("not a CSSRotate"),
        }
    }

    fn set_rotate_part(&self, this: Handle, i: usize, value: Handle) {
        self.tom_mut(this, |t| {
            if let Tom::Rotate(v, _) = t {
                v[i] = value;
            }
        });
    }

    fn scale_part(&self, this: Handle, i: usize) -> Handle {
        match self.tom(this) {
            Tom::Scale(v, _) => v[i],
            _ => panic!("not a CSSScale"),
        }
    }

    fn set_scale_part(&self, this: Handle, i: usize, value: Handle) {
        self.tom_mut(this, |t| {
            if let Tom::Scale(v, _) = t {
                v[i] = value;
            }
        });
    }
}

impl CSSRotateImpl for Engine {
    fn x(&self, this: Handle) -> DoubleOrCSSNumericValue {
        DoubleOrCSSNumericValue::CSSNumericValue(self.rotate_part(this, 0))
    }
    fn set_x(&self, _cx: &mut Cx, this: Handle, value: DoubleOrCSSNumericValue) -> rt::Result<()> {
        let h = self.rectified_number(value)?;
        self.set_rotate_part(this, 0, h);
        Ok(())
    }
    fn y(&self, this: Handle) -> DoubleOrCSSNumericValue {
        DoubleOrCSSNumericValue::CSSNumericValue(self.rotate_part(this, 1))
    }
    fn set_y(&self, _cx: &mut Cx, this: Handle, value: DoubleOrCSSNumericValue) -> rt::Result<()> {
        let h = self.rectified_number(value)?;
        self.set_rotate_part(this, 1, h);
        Ok(())
    }
    fn z(&self, this: Handle) -> DoubleOrCSSNumericValue {
        DoubleOrCSSNumericValue::CSSNumericValue(self.rotate_part(this, 2))
    }
    fn set_z(&self, _cx: &mut Cx, this: Handle, value: DoubleOrCSSNumericValue) -> rt::Result<()> {
        let h = self.rectified_number(value)?;
        self.set_rotate_part(this, 2, h);
        Ok(())
    }
    fn angle(&self, this: Handle) -> Handle {
        self.rotate_part(this, 3)
    }
    fn set_angle(&self, _cx: &mut Cx, this: Handle, value: Handle) -> rt::Result<()> {
        self.expect_angle(value)?;
        self.set_rotate_part(this, 3, value);
        Ok(())
    }
    fn constructor_angle(&self, _cx: &mut Cx, angle: Handle) -> rt::Result<Handle> {
        self.expect_angle(angle)?;
        let v = [
            self.unit(0.0, "number"),
            self.unit(0.0, "number"),
            self.unit(1.0, "number"),
            angle,
        ];
        Ok(self.new_tom(Tom::Rotate(v, true)))
    }
    fn constructor_x_y_z_angle(
        &self,
        _cx: &mut Cx,
        x: DoubleOrCSSNumericValue,
        y: DoubleOrCSSNumericValue,
        z: DoubleOrCSSNumericValue,
        angle: Handle,
    ) -> rt::Result<Handle> {
        self.expect_angle(angle)?;
        let v = [
            self.rectified_number(x)?,
            self.rectified_number(y)?,
            self.rectified_number(z)?,
            angle,
        ];
        Ok(self.new_tom(Tom::Rotate(v, false)))
    }
}

impl CSSScaleImpl for Engine {
    fn x(&self, this: Handle) -> DoubleOrCSSNumericValue {
        DoubleOrCSSNumericValue::CSSNumericValue(self.scale_part(this, 0))
    }
    fn set_x(&self, _cx: &mut Cx, this: Handle, value: DoubleOrCSSNumericValue) -> rt::Result<()> {
        let h = self.rectified_number(value)?;
        self.set_scale_part(this, 0, h);
        Ok(())
    }
    fn y(&self, this: Handle) -> DoubleOrCSSNumericValue {
        DoubleOrCSSNumericValue::CSSNumericValue(self.scale_part(this, 1))
    }
    fn set_y(&self, _cx: &mut Cx, this: Handle, value: DoubleOrCSSNumericValue) -> rt::Result<()> {
        let h = self.rectified_number(value)?;
        self.set_scale_part(this, 1, h);
        Ok(())
    }
    fn z(&self, this: Handle) -> DoubleOrCSSNumericValue {
        DoubleOrCSSNumericValue::CSSNumericValue(self.scale_part(this, 2))
    }
    fn set_z(&self, _cx: &mut Cx, this: Handle, value: DoubleOrCSSNumericValue) -> rt::Result<()> {
        let h = self.rectified_number(value)?;
        self.set_scale_part(this, 2, h);
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        x: DoubleOrCSSNumericValue,
        y: DoubleOrCSSNumericValue,
        z: Option<DoubleOrCSSNumericValue>,
    ) -> rt::Result<Handle> {
        let x = self.rectified_number(x)?;
        let y = self.rectified_number(y)?;
        let is_2d = z.is_none();
        let z = match z {
            Some(z) => self.rectified_number(z)?,
            None => self.unit(1.0, "number"),
        };
        Ok(self.new_tom(Tom::Scale([x, y, z], is_2d)))
    }
}

impl CSSSkewImpl for Engine {
    fn ax(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Skew(v) => v[0],
            _ => panic!("not a CSSSkew"),
        }
    }
    fn set_ax(&self, _cx: &mut Cx, this: Handle, value: Handle) -> rt::Result<()> {
        self.expect_angle(value)?;
        self.tom_mut(this, |t| {
            if let Tom::Skew(v) = t {
                v[0] = value;
            }
        });
        Ok(())
    }
    fn ay(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Skew(v) => v[1],
            _ => panic!("not a CSSSkew"),
        }
    }
    fn set_ay(&self, _cx: &mut Cx, this: Handle, value: Handle) -> rt::Result<()> {
        self.expect_angle(value)?;
        self.tom_mut(this, |t| {
            if let Tom::Skew(v) = t {
                v[1] = value;
            }
        });
        Ok(())
    }
    fn constructor(&self, _cx: &mut Cx, ax: Handle, ay: Handle) -> rt::Result<Handle> {
        self.expect_angle(ax)?;
        self.expect_angle(ay)?;
        Ok(self.new_tom(Tom::Skew([ax, ay])))
    }
}

impl CSSSkewXImpl for Engine {
    fn ax(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::SkewX(v) => v,
            _ => panic!("not a CSSSkewX"),
        }
    }
    fn set_ax(&self, _cx: &mut Cx, this: Handle, value: Handle) -> rt::Result<()> {
        self.expect_angle(value)?;
        self.tom_mut(this, |t| {
            if let Tom::SkewX(v) = t {
                *v = value;
            }
        });
        Ok(())
    }
    fn constructor(&self, _cx: &mut Cx, ax: Handle) -> rt::Result<Handle> {
        self.expect_angle(ax)?;
        Ok(self.new_tom(Tom::SkewX(ax)))
    }
}

impl CSSSkewYImpl for Engine {
    fn ay(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::SkewY(v) => v,
            _ => panic!("not a CSSSkewY"),
        }
    }
    fn set_ay(&self, _cx: &mut Cx, this: Handle, value: Handle) -> rt::Result<()> {
        self.expect_angle(value)?;
        self.tom_mut(this, |t| {
            if let Tom::SkewY(v) = t {
                *v = value;
            }
        });
        Ok(())
    }
    fn constructor(&self, _cx: &mut Cx, ay: Handle) -> rt::Result<Handle> {
        self.expect_angle(ay)?;
        Ok(self.new_tom(Tom::SkewY(ay)))
    }
}

impl CSSPerspectiveImpl for Engine {
    fn length(&self, this: Handle) -> CSSNumericValueOrStringOrCSSKeywordValue {
        let Tom::Perspective(h) = self.tom(this) else {
            panic!("not a CSSPerspective");
        };
        if self.is_numeric(h) {
            CSSNumericValueOrStringOrCSSKeywordValue::CSSNumericValue(h)
        } else {
            CSSNumericValueOrStringOrCSSKeywordValue::CSSKeywordValue(h)
        }
    }
    fn set_length(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: CSSNumericValueOrStringOrCSSKeywordValue,
    ) -> rt::Result<()> {
        let h = self.perspective_length(value)?;
        self.tom_mut(this, |t| {
            if let Tom::Perspective(v) = t {
                *v = h;
            }
        });
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        length: CSSNumericValueOrStringOrCSSKeywordValue,
    ) -> rt::Result<Handle> {
        let h = self.perspective_length(length)?;
        Ok(self.new_tom(Tom::Perspective(h)))
    }
}

impl CSSMatrixComponentImpl for Engine {
    fn matrix(&self, this: Handle) -> Handle {
        match self.tom(this) {
            Tom::Matrix(m, _) => m,
            _ => panic!("not a CSSMatrixComponent"),
        }
    }
    fn set_matrix(&self, _cx: &mut Cx, this: Handle, value: Handle) -> rt::Result<()> {
        self.tom_mut(this, |t| {
            if let Tom::Matrix(m, _) = t {
                *m = value;
            }
        });
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        matrix: Handle,
        options: g::CSSMatrixComponentOptions,
    ) -> rt::Result<Handle> {
        let m = self.matrix(matrix);
        let is_2d = options.is2_d.unwrap_or(m.is_2d);
        let copy = self.new_matrix(m, false);
        Ok(self.new_tom(Tom::Matrix(copy, is_2d)))
    }
}

impl CSSImageValueImpl for Engine {}
