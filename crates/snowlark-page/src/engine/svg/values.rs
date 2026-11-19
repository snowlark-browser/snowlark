// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The basic data types (SVG 2 §4.5.1 `SVGNumber`, §4.5.2 `SVGLength`, §4.5.3 `SVGAngle`, §8.14.1
//! `SVGTransform`, §8.14.4 `SVGPreserveAspectRatio`) and the animated value interfaces of §4.6.

use snowlark_bindings::generated::{
    DOMMatrix2DInit, SVGAngleImpl, SVGAnimatedAngleImpl, SVGAnimatedBooleanImpl,
    SVGAnimatedEnumerationImpl, SVGAnimatedIntegerImpl, SVGAnimatedLengthImpl,
    SVGAnimatedLengthListImpl, SVGAnimatedNumberImpl, SVGAnimatedNumberListImpl,
    SVGAnimatedPreserveAspectRatioImpl, SVGAnimatedRectImpl, SVGAnimatedStringImpl,
    SVGAnimatedTransformListImpl, SVGLengthImpl, SVGNumberImpl, SVGPreserveAspectRatioImpl,
    SVGTransformImpl, StringOrTrustedScriptURL,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};
use snowlark_dom::{ns, NodeId};
use snowlark_layout::svg::{
    parse, Align, Angle, AngleUnit, AspectRatio, Length, LengthContext, LengthDir, LengthUnit,
    Matrix, Transform, TransformKind, ViewBox,
};

use super::super::geometry::{self, Rect};
use super::super::matrix;
use super::super::{exception, Engine, Object};
use super::reflect::{Kind, Spec};
use super::{read_only_error, Item, ItemValue, Owner, SvgObject};
use rt::ExceptionName;

/// The geometry module's 4×4 matrix of a 2D affine one.
pub(crate) fn matrix_of(m: &Matrix) -> matrix::Matrix {
    let [a, b, c, d, e, f] = m.as_coeffs();
    matrix::Matrix::from_2d(a, b, c, d, e, f)
}

/// The 2D part of a 4×4 matrix.
pub(crate) fn affine_of(m: &matrix::Matrix) -> Matrix {
    Matrix::new([
        m.m[0][0], m.m[0][1], m.m[1][0], m.m[1][1], m.m[3][0], m.m[3][1],
    ])
}

fn not_supported() -> rt::Error {
    exception(
        ExceptionName::NotSupportedError,
        "The unit type is not supported.",
    )
}

fn syntax_error() -> rt::Error {
    exception(ExceptionName::SyntaxError, "The value could not be parsed.")
}

/// The initial value of a length attribute.
fn initial_length(spec: &Spec) -> Length {
    match spec.kind {
        Kind::Length { initial, .. } => Length::parse(initial).unwrap_or(Length::ZERO),
        _ => Length::ZERO,
    }
}

fn spec_dir(spec: &Spec) -> LengthDir {
    match spec.kind {
        Kind::Length { dir, .. } | Kind::LengthList(dir) => dir,
        _ => LengthDir::Other,
    }
}

/// `orient`'s value as an angle (`auto` and `auto-start-reverse` are 0, as is an invalid value).
fn orient_angle(v: Option<&str>) -> Angle {
    v.and_then(Angle::parse).unwrap_or(Angle::ZERO)
}

/// The `SVG_MARKER_ORIENT_*` type of an `orient` value (the initial value `0` is an angle).
fn orient_type(v: Option<&str>) -> u16 {
    match v.map(|s| s.trim_matches(|c: char| c.is_ascii() && parse::is_wsp(c as u8))) {
        Some("auto") => 1,
        Some("auto-start-reverse") => 3,
        Some(s) if Angle::parse(s).is_some() => 2,
        _ => 2,
    }
}

impl Engine {
    // ----- value objects ---------------------------------------------------------------------------

    pub(crate) fn new_svg_item(&self, value: ItemValue, owner: Owner, read_only: bool) -> Handle {
        self.alloc(Object::Svg(SvgObject::Item(Box::new(Item {
            value,
            owner,
            read_only,
        }))))
    }

    /// A value object's current value, owner and read-only flag (an attribute's value read now, a
    /// list element's after its list synchronized).
    pub(crate) fn svg_item(&self, h: Handle) -> (ItemValue, Owner, bool) {
        let owner = self.svg_obj(h, |o| match o {
            SvgObject::Item(i) => i.owner,
            _ => panic!("not an SVG value object"),
        });
        if let Owner::List(list) = owner {
            self.svg_sync_list(list);
        }
        // (the synchronization may have detached it)
        let (value, owner, ro) = self.svg_obj(h, |o| match o {
            SvgObject::Item(i) => (i.value.clone(), i.owner, i.read_only),
            _ => panic!("not an SVG value object"),
        });
        match owner {
            Owner::Attr { element, spec } => {
                (self.svg_attr_item(element, spec, &value, ro), owner, ro)
            }
            _ => (value, owner, ro),
        }
    }

    /// The base value of a reflected (non-list) attribute as a value object's value.
    fn svg_attr_item(
        &self,
        element: NodeId,
        spec: &'static Spec,
        like: &ItemValue,
        anim: bool,
    ) -> ItemValue {
        let v = self.svg_value(element, spec, anim);
        match spec.kind {
            Kind::OrientAngle => ItemValue::Angle(orient_angle(v.as_deref())),
            Kind::Length { dir, .. } => ItemValue::Length(
                v.as_deref()
                    .and_then(Length::parse)
                    .unwrap_or_else(|| initial_length(spec)),
                dir,
            ),
            _ => like.clone(),
        }
    }

    /// Sets a value object's value: read-only check, then the reflected attribute is reserialized
    /// (SVG 2 §4.3.5) for an object that reflects one.
    pub(crate) fn svg_set_item(&self, cx: &mut Cx, h: Handle, value: ItemValue) -> rt::Result<()> {
        let (_, owner, ro) = self.svg_item(h);
        if ro {
            return Err(read_only_error());
        }
        self.svg_obj_mut(h, |o| {
            if let SvgObject::Item(i) = o {
                // (a transform keeps its matrix object)
                let value = match (&i.value, value) {
                    (ItemValue::Transform(_, m), ItemValue::Transform(t, _)) => {
                        ItemValue::Transform(t, *m)
                    }
                    (_, v) => v,
                };
                i.value = value;
            }
        });
        let value = self.svg_obj(h, |o| match o {
            SvgObject::Item(i) => i.value.clone(),
            _ => unreachable!(),
        });
        match owner {
            Owner::Detached => {}
            Owner::Attr { element, spec } => {
                let s = match &value {
                    ItemValue::Length(l, _) => l.to_css(),
                    ItemValue::Angle(a) => a.to_css(),
                    ItemValue::Number(n) => parse::fmt_number(*n),
                    ItemValue::Transform(t, _) => t.to_css(),
                };
                self.svg_set_attr(cx, element, spec.attr, &s);
            }
            Owner::List(list) => self.svg_reserialize_list(cx, list),
        }
        Ok(())
    }

    /// The context lengths of a value object resolve in: its element's viewport and font (a
    /// detached object has none).
    fn svg_item_context(&self, owner: Owner) -> LengthContext {
        match owner {
            Owner::Detached => LengthContext::default(),
            Owner::Attr { element, .. } => self.svg_length_context(element),
            Owner::List(list) => {
                let el = self.svg_obj(list, |o| match o {
                    SvgObject::List(l) => Some(l.element),
                    _ => None,
                });
                el.map(|e| self.svg_length_context(e)).unwrap_or_default()
            }
        }
    }

    fn svg_length(&self, h: Handle) -> (Length, LengthDir, Owner, bool) {
        match self.svg_item(h) {
            (ItemValue::Length(l, d), owner, ro) => (l, d, owner, ro),
            _ => panic!("not an SVGLength"),
        }
    }

    fn svg_angle(&self, h: Handle) -> Angle {
        match self.svg_item(h).0 {
            ItemValue::Angle(a) => a,
            _ => panic!("not an SVGAngle"),
        }
    }

    fn svg_transform(&self, h: Handle) -> Transform {
        match self.svg_item(h).0 {
            ItemValue::Transform(t, _) => t,
            _ => panic!("not an SVGTransform"),
        }
    }

    fn set_length(&self, cx: &mut Cx, h: Handle, l: Length) -> rt::Result<()> {
        let (_, dir, _, _) = self.svg_length(h);
        self.svg_set_item(cx, h, ItemValue::Length(l, dir))
    }

    fn set_transform(&self, cx: &mut Cx, h: Handle, t: Transform) -> rt::Result<()> {
        self.svg_set_item(cx, h, ItemValue::Transform(t, None))
    }

    // ----- the matrix of an SVGTransform -----------------------------------------------------------

    /// A linked `DOMMatrix`'s current value: its transform's matrix.
    pub(crate) fn svg_linked_matrix(&self, transform: Handle) -> matrix::Matrix {
        matrix_of(&self.svg_transform(transform).matrix)
    }

    /// Whether writes to a linked `DOMMatrix` are refused (its transform is read only).
    pub(crate) fn svg_linked_matrix_read_only(&self, transform: Handle) -> bool {
        self.svg_item(transform).2
    }

    /// A linked `DOMMatrix` changed (SVG 2 §8.14.1: the transform becomes a `matrix(…)` and its
    /// attribute is reserialized).
    pub(crate) fn svg_linked_matrix_set(&self, cx: &mut Cx, transform: Handle, m: &matrix::Matrix) {
        let _ = self.set_transform(cx, transform, Transform::matrix(affine_of(m)));
    }

    // ----- the DOMRect of viewBox ------------------------------------------------------------------

    /// A `viewBox` rect's current value (the attribute's, `0 0 0 0` when absent or invalid).
    pub(crate) fn svg_linked_rect(&self, animated: Handle, r: Rect) -> Rect {
        let (element, spec) = self.svg_animated(animated);
        let vb = self
            .svg_value(element, spec, r.read_only)
            .and_then(|v| ViewBox::parse(&v));
        let (x, y, w, h) = vb.map_or((0.0, 0.0, 0.0, 0.0), |v| (v.x, v.y, v.width, v.height));
        Rect {
            x,
            y,
            width: w,
            height: h,
            ..r
        }
    }

    /// A `viewBox` rect changed: the attribute is reserialized.
    pub(crate) fn svg_linked_rect_set(&self, cx: &mut Cx, animated: Handle, r: &Rect) {
        let (element, spec) = self.svg_animated(animated);
        let s = ViewBox {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
        .to_css();
        self.svg_set_attr(cx, element, spec.attr, &s);
    }

    /// The element and attribute of an animated object.
    pub(crate) fn svg_animated(&self, h: Handle) -> (NodeId, &'static Spec) {
        self.svg_obj(h, |o| match o {
            SvgObject::Animated { element, spec } => (*element, *spec),
            SvgObject::Aspect { element, spec, .. } => (*element, *spec),
            _ => panic!("not an SVG animated object"),
        })
    }

    // ----- primitive reflections -------------------------------------------------------------------

    /// The two numbers of a `<number-optional-number>` attribute (the second defaulting to the first),
    /// or `None` when absent or invalid.
    fn svg_number_pair(&self, element: NodeId, spec: &Spec, anim: bool) -> Option<(f64, f64)> {
        let v = self.svg_value(element, spec, anim)?;
        let (a, b) = parse::parse_number_optional_number(&v)?;
        Some((a, b.unwrap_or(a)))
    }

    fn svg_number_value(&self, element: NodeId, spec: &Spec, anim: bool) -> f32 {
        match spec.kind {
            Kind::Number { initial } => self
                .svg_value(element, spec, anim)
                .and_then(|v| parse::parse_number(&v))
                .map_or(initial, |v| v as f32),
            Kind::NumberPart { second, initial } => self
                .svg_number_pair(element, spec, anim)
                .map_or(initial, |(a, b)| if second { b } else { a } as f32),
            _ => 0.0,
        }
    }

    fn svg_integer_value(&self, element: NodeId, spec: &Spec, anim: bool) -> i32 {
        let int = |s: &str| -> Option<i32> {
            let v = parse::parse_number(s)?;
            (v.fract() == 0.0 && v.abs() <= i32::MAX as f64).then_some(v as i32)
        };
        match spec.kind {
            Kind::Integer { initial } => self
                .svg_value(element, spec, anim)
                .and_then(|v| int(&v))
                .unwrap_or(initial),
            Kind::IntegerPart { second, initial } => {
                let v = self.svg_value(element, spec, anim);
                let parts: Option<Vec<i32>> = v.and_then(|v| {
                    let list = parse::parse_number_list(&v)?;
                    (1..=2)
                        .contains(&list.len())
                        .then(|| list.iter().map(|n| int(&n.to_string())).collect())
                        .flatten()
                });
                match parts.as_deref() {
                    Some([a]) => *a,
                    Some([a, b]) => {
                        if second {
                            *b
                        } else {
                            *a
                        }
                    }
                    _ => initial,
                }
            }
            _ => 0,
        }
    }

    fn svg_enum_value(&self, element: NodeId, spec: &Spec, anim: bool) -> u16 {
        let v = self.svg_value(element, spec, anim);
        match spec.kind {
            Kind::Enum { keywords, initial } => v
                .and_then(|v| keywords.iter().position(|k| *k == v))
                .map_or(initial, |i| i as u16 + 1),
            Kind::OrientType => orient_type(v.as_deref()),
            _ => 0,
        }
    }

    /// An `SVGAnimatedString`'s value (SVG 2 §4.6.7: `href` falls back to `xlink:href`).
    fn svg_string_value(&self, element: NodeId, spec: &Spec, anim: bool) -> String {
        if anim {
            if let Some(v) = self.smil_value(element, spec, None) {
                return v;
            }
        }
        if spec.kind == Kind::Href {
            let dom = self.dom();
            if !dom.is_alive(element) || !dom.is_element(element) {
                return String::new();
            }
            if let Some(v) = dom.attr(element, &ns!(), &"href".into()) {
                return v.to_owned();
            }
            if let Some(v) = dom.attr(element, &ns!(xlink), &"href".into()) {
                return v.to_owned();
            }
            return String::new();
        }
        self.svg_attr(element, spec.attr).unwrap_or_default()
    }
}

// ----- SVGNumber, SVGLength, SVGAngle ---------------------------------------------------------------

impl SVGNumberImpl for Engine {
    fn value(&self, this: Handle) -> f32 {
        match self.svg_item(this).0 {
            ItemValue::Number(n) => n,
            _ => 0.0,
        }
    }
    fn set_value(&self, cx: &mut Cx, this: Handle, value: f32) -> rt::Result<()> {
        self.svg_set_item(cx, this, ItemValue::Number(value))
    }
}

impl SVGLengthImpl for Engine {
    fn unit_type(&self, this: Handle) -> u16 {
        self.svg_length(this).0.unit_type()
    }
    /// SVG 2 §4.5.2 `value`: in user units.
    fn value(&self, this: Handle) -> f32 {
        let (l, dir, owner, _) = self.svg_length(this);
        if matches!(l.unit, LengthUnit::Number) {
            return l.value;
        }
        if l.unit.is_absolute() {
            return l.to_user(dir, &LengthContext::default());
        }
        l.to_user(dir, &self.svg_item_context(owner))
    }
    fn set_value(&self, cx: &mut Cx, this: Handle, value: f32) -> rt::Result<()> {
        // (SVG 2 §4.5.2 "On setting value" makes the value a <number>; WPT's
        // svg/types/scripted/SVGLength-*.html measure the length keeping its unit)
        let (l, dir, owner, ro) = self.svg_length(this);
        if ro {
            return Err(read_only_error());
        }
        let new = match l.unit {
            LengthUnit::Number | LengthUnit::Calc(_) => Length::number(value),
            u => Length::converted(u, value, dir, &self.svg_item_context(owner)),
        };
        self.set_length(cx, this, new)
    }
    fn value_in_specified_units(&self, this: Handle) -> f32 {
        self.svg_length(this).0.specified()
    }
    fn set_value_in_specified_units(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: f32,
    ) -> rt::Result<()> {
        let (l, _, _, ro) = self.svg_length(this);
        if ro {
            return Err(read_only_error());
        }
        let unit = match l.unit {
            LengthUnit::Calc(_) => LengthUnit::Number,
            u => u,
        };
        self.set_length(cx, this, Length { value, unit })
    }
    fn value_as_string(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.svg_length(this).0.to_css())
    }
    fn set_value_as_string(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        if self.svg_length(this).3 {
            return Err(read_only_error());
        }
        let l = Length::parse(value).ok_or_else(syntax_error)?;
        self.set_length(cx, this, l)
    }
    fn new_value_specified_units(
        &self,
        cx: &mut Cx,
        this: Handle,
        unit_type: u16,
        value_in_specified_units: f32,
    ) -> rt::Result<()> {
        if self.svg_length(this).3 {
            return Err(read_only_error());
        }
        let unit = LengthUnit::from_type(unit_type).ok_or_else(not_supported)?;
        self.set_length(
            cx,
            this,
            Length {
                value: value_in_specified_units,
                unit,
            },
        )
    }
    fn convert_to_specified_units(
        &self,
        cx: &mut Cx,
        this: Handle,
        unit_type: u16,
    ) -> rt::Result<()> {
        let (l, dir, owner, ro) = self.svg_length(this);
        if ro {
            return Err(read_only_error());
        }
        let unit = LengthUnit::from_type(unit_type).ok_or_else(not_supported)?;
        let ctx = self.svg_item_context(owner);
        let absolute = l.to_user(dir, &ctx);
        self.set_length(cx, this, Length::converted(unit, absolute, dir, &ctx))
    }
}

impl SVGAngleImpl for Engine {
    fn unit_type(&self, this: Handle) -> u16 {
        self.svg_angle(this).unit.unit_type()
    }
    fn value(&self, this: Handle) -> f32 {
        self.svg_angle(this).degrees()
    }
    fn set_value(&self, cx: &mut Cx, this: Handle, value: f32) -> rt::Result<()> {
        let a = self.svg_angle(this);
        let new = match a.unit {
            AngleUnit::Unspecified => Angle {
                value,
                unit: AngleUnit::Unspecified,
            },
            u => Angle::converted(u, value),
        };
        self.svg_set_item(cx, this, ItemValue::Angle(new))
    }
    fn value_in_specified_units(&self, this: Handle) -> f32 {
        self.svg_angle(this).value
    }
    fn set_value_in_specified_units(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: f32,
    ) -> rt::Result<()> {
        let a = self.svg_angle(this);
        self.svg_set_item(cx, this, ItemValue::Angle(Angle { value, ..a }))
    }
    fn value_as_string(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.svg_angle(this).to_css())
    }
    fn set_value_as_string(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        if self.svg_item(this).2 {
            return Err(read_only_error());
        }
        let a = Angle::parse(value).ok_or_else(syntax_error)?;
        self.svg_set_item(cx, this, ItemValue::Angle(a))
    }
    fn new_value_specified_units(
        &self,
        cx: &mut Cx,
        this: Handle,
        unit_type: u16,
        value_in_specified_units: f32,
    ) -> rt::Result<()> {
        if self.svg_item(this).2 {
            return Err(read_only_error());
        }
        let unit = AngleUnit::from_type(unit_type).ok_or_else(not_supported)?;
        self.svg_set_item(
            cx,
            this,
            ItemValue::Angle(Angle {
                value: value_in_specified_units,
                unit,
            }),
        )
    }
    fn convert_to_specified_units(
        &self,
        cx: &mut Cx,
        this: Handle,
        unit_type: u16,
    ) -> rt::Result<()> {
        if self.svg_item(this).2 {
            return Err(read_only_error());
        }
        let unit = AngleUnit::from_type(unit_type).ok_or_else(not_supported)?;
        let degrees = self.svg_angle(this).degrees();
        self.svg_set_item(cx, this, ItemValue::Angle(Angle::converted(unit, degrees)))
    }
}

// ----- SVGTransform -------------------------------------------------------------------------------

impl SVGTransformImpl for Engine {
    fn type_(&self, this: Handle) -> u16 {
        self.svg_transform(this).kind as u16
    }
    /// SVG 2 §8.14.1 `matrix`: a `DOMMatrix` that reflects the transform.
    fn matrix(&self, this: Handle) -> Handle {
        let existing = self.svg_obj(this, |o| match o {
            SvgObject::Item(i) => match i.value {
                ItemValue::Transform(_, m) => m,
                _ => None,
            },
            _ => None,
        });
        if let Some(m) = existing {
            return m;
        }
        let t = self.svg_transform(this);
        let m = self.alloc(Object::DomMatrix(Box::new(geometry::MatrixData {
            m: matrix_of(&t.matrix),
            read_only: false,
            svg: Some(this),
        })));
        self.svg_obj_mut(this, |o| {
            if let SvgObject::Item(i) = o {
                if let ItemValue::Transform(_, slot) = &mut i.value {
                    *slot = Some(m);
                }
            }
        });
        m
    }
    fn angle(&self, this: Handle) -> f32 {
        let t = self.svg_transform(this);
        match t.kind {
            TransformKind::Rotate | TransformKind::SkewX | TransformKind::SkewY => t.angle as f32,
            _ => 0.0,
        }
    }
    fn set_matrix(&self, cx: &mut Cx, this: Handle, matrix: DOMMatrix2DInit) -> rt::Result<()> {
        if self.svg_item(this).2 {
            return Err(read_only_error());
        }
        let m = geometry::matrix_from_2d_init(&matrix)?;
        self.set_transform(cx, this, Transform::matrix(Matrix::new(m)))
    }
    fn set_translate(&self, cx: &mut Cx, this: Handle, tx: f32, ty: f32) -> rt::Result<()> {
        self.set_transform(cx, this, Transform::translate(tx as f64, ty as f64))
    }
    fn set_scale(&self, cx: &mut Cx, this: Handle, sx: f32, sy: f32) -> rt::Result<()> {
        self.set_transform(cx, this, Transform::scale(sx as f64, sy as f64))
    }
    fn set_rotate(
        &self,
        cx: &mut Cx,
        this: Handle,
        angle: f32,
        cx_: f32,
        cy: f32,
    ) -> rt::Result<()> {
        self.set_transform(
            cx,
            this,
            Transform::rotate(angle as f64, cx_ as f64, cy as f64),
        )
    }
    fn set_skew_x(&self, cx: &mut Cx, this: Handle, angle: f32) -> rt::Result<()> {
        self.set_transform(cx, this, Transform::skew_x(angle as f64))
    }
    fn set_skew_y(&self, cx: &mut Cx, this: Handle, angle: f32) -> rt::Result<()> {
        self.set_transform(cx, this, Transform::skew_y(angle as f64))
    }
}

// ----- SVGPreserveAspectRatio ---------------------------------------------------------------------

impl Engine {
    fn svg_aspect(&self, h: Handle) -> (NodeId, &'static Spec, bool, AspectRatio) {
        let (element, spec, anim) = self.svg_obj(h, |o| match o {
            SvgObject::Aspect {
                element,
                spec,
                anim,
            } => (*element, *spec, *anim),
            _ => panic!("not an SVGPreserveAspectRatio"),
        });
        let v = self
            .svg_value(element, spec, anim)
            .and_then(|v| AspectRatio::parse(&v))
            .unwrap_or_default();
        (element, spec, anim, v)
    }
}

impl SVGPreserveAspectRatioImpl for Engine {
    fn align(&self, this: Handle) -> u16 {
        self.svg_aspect(this).3.align as u16
    }
    fn set_align(&self, cx: &mut Cx, this: Handle, value: u16) -> rt::Result<()> {
        let (element, spec, anim, v) = self.svg_aspect(this);
        if anim {
            return Err(read_only_error());
        }
        let align = Align::from_u16(value)
            .ok_or_else(|| rt::Error::Type("Invalid alignment type.".into()))?;
        let s = AspectRatio { align, ..v }.to_css();
        self.svg_set_attr(cx, element, spec.attr, &s);
        Ok(())
    }
    fn meet_or_slice(&self, this: Handle) -> u16 {
        if self.svg_aspect(this).3.slice {
            2
        } else {
            1
        }
    }
    fn set_meet_or_slice(&self, cx: &mut Cx, this: Handle, value: u16) -> rt::Result<()> {
        let (element, spec, anim, v) = self.svg_aspect(this);
        if anim {
            return Err(read_only_error());
        }
        let slice = match value {
            1 => false,
            2 => true,
            _ => return Err(rt::Error::Type("Invalid meet-or-slice type.".into())),
        };
        let s = AspectRatio { slice, ..v }.to_css();
        self.svg_set_attr(cx, element, spec.attr, &s);
        Ok(())
    }
}

// ----- the animated value interfaces (§4.6) -------------------------------------------------------

impl SVGAnimatedBooleanImpl for Engine {
    fn base_val(&self, this: Handle) -> bool {
        let (element, spec) = self.svg_animated(this);
        self.svg_attr(element, spec.attr).as_deref() == Some("true")
    }
    fn set_base_val(&self, cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        let (element, spec) = self.svg_animated(this);
        self.svg_set_attr(cx, element, spec.attr, if value { "true" } else { "false" });
        Ok(())
    }
    fn anim_val(&self, this: Handle) -> bool {
        let (element, spec) = self.svg_animated(this);
        self.svg_value(element, spec, true).as_deref() == Some("true")
    }
}

impl SVGAnimatedEnumerationImpl for Engine {
    fn base_val(&self, this: Handle) -> u16 {
        let (element, spec) = self.svg_animated(this);
        self.svg_enum_value(element, spec, false)
    }
    /// SVG 2 §4.6.2: 0 and values without a keyword are a TypeError.
    fn set_base_val(&self, cx: &mut Cx, this: Handle, value: u16) -> rt::Result<()> {
        let (element, spec) = self.svg_animated(this);
        let keyword = match spec.kind {
            Kind::Enum { keywords, .. } => (value as usize)
                .checked_sub(1)
                .and_then(|i| keywords.get(i))
                .copied(),
            Kind::OrientType => match value {
                1 => Some("auto"),
                2 => Some("0"),
                3 => Some("auto-start-reverse"),
                _ => None,
            },
            _ => None,
        };
        let keyword =
            keyword.ok_or_else(|| rt::Error::Type("Invalid enumeration value.".into()))?;
        // (setting orientType to ANGLE keeps an angle that is already there)
        if spec.kind == Kind::OrientType && value == 2 {
            let cur = self.svg_attr(element, spec.attr);
            if cur.as_deref().and_then(Angle::parse).is_some() {
                return Ok(());
            }
        }
        self.svg_set_attr(cx, element, spec.attr, keyword);
        Ok(())
    }
    fn anim_val(&self, this: Handle) -> u16 {
        let (element, spec) = self.svg_animated(this);
        self.svg_enum_value(element, spec, true)
    }
}

impl SVGAnimatedIntegerImpl for Engine {
    fn base_val(&self, this: Handle) -> i32 {
        let (element, spec) = self.svg_animated(this);
        self.svg_integer_value(element, spec, false)
    }
    fn set_base_val(&self, cx: &mut Cx, this: Handle, value: i32) -> rt::Result<()> {
        let (element, spec) = self.svg_animated(this);
        let s = match spec.kind {
            Kind::IntegerPart { second, initial } => {
                let part = |second| Spec {
                    attr: spec.attr,
                    kind: Kind::IntegerPart { second, initial },
                };
                let mut a = self.svg_integer_value(element, &part(false), false);
                let mut b = self.svg_integer_value(element, &part(true), false);
                if second {
                    b = value;
                } else {
                    a = value;
                }
                format!("{a} {b}")
            }
            _ => value.to_string(),
        };
        self.svg_set_attr(cx, element, spec.attr, &s);
        Ok(())
    }
    fn anim_val(&self, this: Handle) -> i32 {
        let (element, spec) = self.svg_animated(this);
        self.svg_integer_value(element, spec, true)
    }
}

impl SVGAnimatedNumberImpl for Engine {
    fn base_val(&self, this: Handle) -> f32 {
        let (element, spec) = self.svg_animated(this);
        self.svg_number_value(element, spec, false)
    }
    fn set_base_val(&self, cx: &mut Cx, this: Handle, value: f32) -> rt::Result<()> {
        let (element, spec) = self.svg_animated(this);
        let s = match spec.kind {
            Kind::NumberPart { second, initial } => {
                let (mut a, mut b) = self
                    .svg_number_pair(element, spec, false)
                    .map_or((initial, initial), |(a, b)| (a as f32, b as f32));
                if second {
                    b = value;
                } else {
                    a = value;
                }
                format!("{} {}", parse::fmt_number(a), parse::fmt_number(b))
            }
            _ => parse::fmt_number(value),
        };
        self.svg_set_attr(cx, element, spec.attr, &s);
        Ok(())
    }
    fn anim_val(&self, this: Handle) -> f32 {
        let (element, spec) = self.svg_animated(this);
        self.svg_number_value(element, spec, true)
    }
}

impl SVGAnimatedStringImpl for Engine {
    fn base_val(&self, this: Handle) -> StringOrTrustedScriptURL {
        let (element, spec) = self.svg_animated(this);
        StringOrTrustedScriptURL::String(self.svg_string_value(element, spec, false))
    }
    /// SVG 2 §4.6.7: `href` sets `xlink:href` when only that one is present; a script's `href`
    /// goes through Trusted Types.
    fn set_base_val(
        &self,
        cx: &mut Cx,
        this: Handle,
        value: StringOrTrustedScriptURL,
    ) -> rt::Result<()> {
        let (element, spec) = self.svg_animated(this);
        let is_script = super::is_svg(&self.dom(), element, "script");
        let v = if is_script && spec.kind == Kind::Href {
            use super::super::trusted_types::{TtKind, TtValue};
            let doc = self.dom().node_document(element);
            let tt = match value {
                StringOrTrustedScriptURL::TrustedScriptURL(h) => TtValue::Trusted(h),
                StringOrTrustedScriptURL::String(s) => TtValue::String(s),
            };
            self.tt_compliant(cx, tt, TtKind::ScriptUrl, doc, "SVGScriptElement href")?
        } else {
            match value {
                StringOrTrustedScriptURL::TrustedScriptURL(h) => match &*self.obj(h) {
                    Object::TrustedScriptUrl(s) => s.clone(),
                    _ => String::new(),
                },
                StringOrTrustedScriptURL::String(s) => s,
            }
        };
        if spec.kind == Kind::Href {
            if !self.dom().is_alive(element) {
                return Ok(());
            }
            let (plain, xlink) = {
                let dom = self.dom();
                (
                    dom.attr(element, &ns!(), &"href".into()).is_some(),
                    dom.attr(element, &ns!(xlink), &"href".into()).is_some(),
                )
            };
            if !plain && xlink {
                self.dom_mut().set_attribute_value(
                    element,
                    "href".into(),
                    ns!(xlink),
                    Some("xlink".into()),
                    &v,
                );
                self.after_mutation(cx);
                return Ok(());
            }
        }
        self.svg_set_attr(cx, element, spec.attr, &v);
        Ok(())
    }
    fn anim_val(&self, this: Handle) -> DomStr<'_> {
        let (element, spec) = self.svg_animated(this);
        DomStr::Owned(self.svg_string_value(element, spec, true))
    }
}

impl Engine {
    /// The animated object's `baseVal`/`animVal` value object for a non-list attribute.
    fn svg_animated_item(&self, this: Handle, anim: bool) -> Handle {
        let (element, spec) = self.svg_animated(this);
        let value = match spec.kind {
            Kind::OrientAngle => ItemValue::Angle(Angle::ZERO),
            _ => ItemValue::Length(Length::ZERO, spec_dir(spec)),
        };
        self.new_svg_item(value, Owner::Attr { element, spec }, anim)
    }
}

impl SVGAnimatedLengthImpl for Engine {
    fn base_val(&self, this: Handle) -> Handle {
        self.svg_animated_item(this, false)
    }
    fn anim_val(&self, this: Handle) -> Handle {
        self.svg_animated_item(this, true)
    }
}

impl SVGAnimatedAngleImpl for Engine {
    fn base_val(&self, this: Handle) -> Handle {
        self.svg_animated_item(this, false)
    }
    fn anim_val(&self, this: Handle) -> Handle {
        self.svg_animated_item(this, true)
    }
}

impl SVGAnimatedRectImpl for Engine {
    /// SVG 2 §4.6.8: a `DOMRect` that reflects the attribute.
    fn base_val(&self, this: Handle) -> Handle {
        self.alloc(Object::DomRect(Rect {
            svg: Some(this),
            ..Rect::new(0.0, 0.0, 0.0, 0.0)
        }))
    }
    fn anim_val(&self, this: Handle) -> Handle {
        self.alloc(Object::DomRect(Rect {
            svg: Some(this),
            ..Rect::read_only(0.0, 0.0, 0.0, 0.0)
        }))
    }
}

impl SVGAnimatedPreserveAspectRatioImpl for Engine {
    fn base_val(&self, this: Handle) -> Handle {
        let (element, spec) = self.svg_animated(this);
        self.alloc(Object::Svg(SvgObject::Aspect {
            element,
            spec,
            anim: false,
        }))
    }
    fn anim_val(&self, this: Handle) -> Handle {
        let (element, spec) = self.svg_animated(this);
        self.alloc(Object::Svg(SvgObject::Aspect {
            element,
            spec,
            anim: true,
        }))
    }
}

impl SVGAnimatedLengthListImpl for Engine {
    fn base_val(&self, this: Handle) -> Handle {
        let (element, spec) = self.svg_animated(this);
        self.new_svg_list(element, spec, false)
    }
    fn anim_val(&self, this: Handle) -> Handle {
        let (element, spec) = self.svg_animated(this);
        self.new_svg_list(element, spec, true)
    }
}

impl SVGAnimatedNumberListImpl for Engine {
    fn base_val(&self, this: Handle) -> Handle {
        let (element, spec) = self.svg_animated(this);
        self.new_svg_list(element, spec, false)
    }
    fn anim_val(&self, this: Handle) -> Handle {
        let (element, spec) = self.svg_animated(this);
        self.new_svg_list(element, spec, true)
    }
}

impl SVGAnimatedTransformListImpl for Engine {
    fn base_val(&self, this: Handle) -> Handle {
        let (element, spec) = self.svg_animated(this);
        self.new_svg_list(element, spec, false)
    }
    fn anim_val(&self, this: Handle) -> Handle {
        let (element, spec) = self.svg_animated(this);
        self.new_svg_list(element, spec, true)
    }
}
