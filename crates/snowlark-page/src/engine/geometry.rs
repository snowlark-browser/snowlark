// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Geometry Interfaces Module Level 1: `DOMPointReadOnly`/`DOMPoint` (§3), `DOMRectReadOnly`/`DOMRect`
//! and `DOMRectList` (§4), `DOMQuad` (§5), `DOMMatrixReadOnly`/`DOMMatrix` (§6); and CSSOM View's
//! `GeometryUtils` (`getBoxQuads`, `convert*FromNode`) over the last layout, without transforms.

use snowlark_bindings::generated::{
    self as g, CSSBoxType, DOMMatrixImpl, DOMMatrixInit, DOMMatrixReadOnlyImpl, DOMPointImpl,
    DOMPointInit, DOMPointReadOnlyImpl, DOMQuadImpl, DOMQuadInit, DOMRectImpl, DOMRectInit,
    DOMRectListImpl, DOMRectReadOnlyImpl, GeometryUtilsImpl, StringOrUnrestrictedDoubleSequence,
    TextOrElementOrDocument,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle, JsObject};
use snowlark_dom::NodeId;

use super::matrix::{self, Matrix};
use super::{exception, Engine, Object};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub read_only: bool,
    /// The SVG object (an `SVGAnimatedRect`) whose attribute this rect reflects (SVG 2 §4.6.8).
    pub svg: Option<Handle>,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
            read_only: false,
            svg: None,
        }
    }

    pub fn read_only(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            read_only: true,
            ..Rect::new(x, y, width, height)
        }
    }
}

/// `min` that propagates NaN, as the Geometry spec's "minimum" does.
fn nan_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

fn nan_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

impl Engine {
    fn rect(&self, h: Handle) -> Rect {
        let r = match &*self.obj(h) {
            Object::DomRect(r) => *r,
            _ => panic!("not a DOMRect"),
        };
        match r.svg {
            Some(svg) => self.svg_linked_rect(svg, r),
            None => r,
        }
    }

    /// A `DOMRect` setter: the change, then the reflected `viewBox` reserialized for a linked rect.
    fn rect_set(&self, cx: &mut Cx, h: Handle, f: impl FnOnce(&mut Rect)) -> rt::Result<()> {
        let mut r = self.rect(h);
        if r.read_only {
            return Err(super::svg::read_only_error());
        }
        f(&mut r);
        self.rect_mut(h, |stored| *stored = r);
        if let Some(svg) = r.svg {
            self.svg_linked_rect_set(cx, svg, &r);
        }
        Ok(())
    }

    fn rect_mut(&self, h: Handle, f: impl FnOnce(&mut Rect)) {
        if let Object::DomRect(r) = &mut *self.obj_mut(h) {
            f(r);
        }
    }
}

impl DOMRectReadOnlyImpl for Engine {
    fn x(&self, this: Handle) -> f64 {
        self.rect(this).x
    }
    fn y(&self, this: Handle) -> f64 {
        self.rect(this).y
    }
    fn width(&self, this: Handle) -> f64 {
        self.rect(this).width
    }
    fn height(&self, this: Handle) -> f64 {
        self.rect(this).height
    }
    fn top(&self, this: Handle) -> f64 {
        let r = self.rect(this);
        nan_min(r.y, r.y + r.height)
    }
    fn right(&self, this: Handle) -> f64 {
        let r = self.rect(this);
        nan_max(r.x, r.x + r.width)
    }
    fn bottom(&self, this: Handle) -> f64 {
        let r = self.rect(this);
        nan_max(r.y, r.y + r.height)
    }
    fn left(&self, this: Handle) -> f64 {
        let r = self.rect(this);
        nan_min(r.x, r.x + r.width)
    }
    fn static_from_rect(&self, _cx: &mut Cx, other: DOMRectInit) -> rt::Result<Handle> {
        Ok(self.alloc(Object::DomRect(Rect::read_only(
            other.x,
            other.y,
            other.width,
            other.height,
        ))))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> rt::Result<Handle> {
        Ok(self.alloc(Object::DomRect(Rect::read_only(x, y, width, height))))
    }
}

impl DOMRectImpl for Engine {
    fn x(&self, this: Handle) -> f64 {
        self.rect(this).x
    }
    fn set_x(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.rect_set(cx, this, |r| r.x = value)
    }
    fn y(&self, this: Handle) -> f64 {
        self.rect(this).y
    }
    fn set_y(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.rect_set(cx, this, |r| r.y = value)
    }
    fn width(&self, this: Handle) -> f64 {
        self.rect(this).width
    }
    fn set_width(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.rect_set(cx, this, |r| r.width = value)
    }
    fn height(&self, this: Handle) -> f64 {
        self.rect(this).height
    }
    fn set_height(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.rect_set(cx, this, |r| r.height = value)
    }
    fn static_from_rect(&self, _cx: &mut Cx, other: DOMRectInit) -> rt::Result<Handle> {
        Ok(self.alloc(Object::DomRect(Rect::new(
            other.x,
            other.y,
            other.width,
            other.height,
        ))))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> rt::Result<Handle> {
        Ok(self.alloc(Object::DomRect(Rect::new(x, y, width, height))))
    }
}

impl DOMRectListImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        match &*self.obj(this) {
            Object::DomRectList(l) => l.len() as u32,
            _ => 0,
        }
    }
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        match &*self.obj(this) {
            Object::DomRectList(l) => l.get(index as usize).copied(),
            _ => None,
        }
    }
}

/// A `DOMPointReadOnly` or `DOMPoint`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point4 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub w: f64,
    pub read_only: bool,
    /// The SVG point list this point is an element of (SVG 2 §10.7 `SVGPointList`).
    pub svg: Option<Handle>,
}

impl Point4 {
    fn from_init(p: &DOMPointInit, read_only: bool) -> Point4 {
        Point4 {
            x: p.x,
            y: p.y,
            z: p.z,
            w: p.w,
            read_only,
            svg: None,
        }
    }
}

/// A `DOMMatrixReadOnly` or `DOMMatrix`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MatrixData {
    pub m: Matrix,
    pub read_only: bool,
    /// The `SVGTransform` whose `matrix` this is (SVG 2 §8.14.1: it reflects the transform).
    pub svg: Option<Handle>,
}

/// Web IDL SameValueZero for doubles.
fn same_value_zero(a: f64, b: f64) -> bool {
    a == b || (a.is_nan() && b.is_nan())
}

/// Geometry §6.1 "validate and fixup (2D)": the six 2D elements (m11, m12, m21, m22, m41, m42) from
/// the aliases `a`..`f` and the elements, or a TypeError when an alias and its element disagree.
fn fixup_2d(aliases: [Option<f64>; 6], elements: [Option<f64>; 6]) -> rt::Result<[f64; 6]> {
    const DEFAULTS: [f64; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    const NAMES: [(&str, &str); 6] = [
        ("a", "m11"),
        ("b", "m12"),
        ("c", "m21"),
        ("d", "m22"),
        ("e", "m41"),
        ("f", "m42"),
    ];
    let mut out = [0.0; 6];
    for i in 0..6 {
        if let (Some(x), Some(y)) = (aliases[i], elements[i]) {
            if !same_value_zero(x, y) {
                return Err(Error::Type(
                    format!("{} and {} differ", NAMES[i].0, NAMES[i].1).into(),
                ));
            }
        }
        out[i] = elements[i].or(aliases[i]).unwrap_or(DEFAULTS[i]);
    }
    Ok(out)
}

/// Geometry §6.1 "create a DOMMatrix from the dictionary" (after "validate and fixup").
pub(crate) fn matrix_from_init(i: &DOMMatrixInit) -> rt::Result<Matrix> {
    let [m11, m12, m21, m22, m41, m42] = fixup_2d(
        [i.a, i.b, i.c, i.d, i.e, i.f],
        [i.m11, i.m12, i.m21, i.m22, i.m41, i.m42],
    )?;
    let flat = [i.m13, i.m14, i.m23, i.m24, i.m31, i.m32, i.m34, i.m43]
        .iter()
        .all(|v| *v == 0.0)
        && i.m33 == 1.0
        && i.m44 == 1.0;
    if i.is2_d == Some(true) && !flat {
        return Err(Error::Type(
            "is2D is true but the matrix has 3D elements".into(),
        ));
    }
    let mut m = Matrix::from_3d(&[
        m11, m12, i.m13, i.m14, m21, m22, i.m23, i.m24, i.m31, i.m32, i.m33, i.m34, m41, m42,
        i.m43, i.m44,
    ]);
    m.is_2d = i.is2_d.unwrap_or(flat);
    Ok(m)
}

/// The same for a `DOMMatrix2DInit` (canvas `setTransform`): a, b, c, d, e, f.
pub(crate) fn matrix_from_2d_init(i: &g::DOMMatrix2DInit) -> rt::Result<[f64; 6]> {
    fixup_2d(
        [i.a, i.b, i.c, i.d, i.e, i.f],
        [i.m11, i.m12, i.m21, i.m22, i.m41, i.m42],
    )
}

/// NaN-safe minimum or maximum (Geometry §5 `getBounds()`).
fn nan_safe(values: [f64; 4], max: bool) -> f64 {
    if values.iter().any(|v| v.is_nan()) {
        return f64::NAN;
    }
    let start = if max {
        f64::NEG_INFINITY
    } else {
        f64::INFINITY
    };
    values
        .into_iter()
        .fold(start, |a, b| if max { a.max(b) } else { a.min(b) })
}

impl Engine {
    fn point(&self, h: Handle) -> Point4 {
        let list = match &*self.obj(h) {
            Object::DomPoint(p) => p.svg,
            _ => None,
        };
        // (a point linked to an element is a non-outermost `currentTranslate`, not a list's)
        if let Some(list) = list.filter(|h| super::handle_node(*h).is_none()) {
            self.svg_sync_list(list);
        }
        match &*self.obj(h) {
            Object::DomPoint(p) => *p,
            _ => panic!("not a DOMPoint"),
        }
    }

    fn point_mut(&self, h: Handle, f: impl FnOnce(&mut Point4)) {
        if let Object::DomPoint(p) = &mut *self.obj_mut(h) {
            f(p);
        }
    }

    /// A `DOMPoint` setter: a point of an `animVal` point list is read only, one of a `baseVal`
    /// list reserializes the list's attribute (SVG 2 §4.5.4).
    fn point_set(&self, cx: &mut Cx, h: Handle, f: impl FnOnce(&mut Point4)) -> rt::Result<()> {
        let p = self.point(h);
        if let Some(list) = p.svg {
            if p.read_only {
                return Err(super::svg::read_only_error());
            }
            self.point_mut(h, f);
            let p = self.point(h);
            match super::handle_node(list) {
                Some(svg) => self.svg_set_current_translate(svg, p.x, p.y),
                None => self.svg_reserialize_list(cx, list),
            }
            return Ok(());
        }
        self.point_mut(h, f);
        Ok(())
    }

    pub(crate) fn new_point(&self, x: f64, y: f64, z: f64, w: f64, read_only: bool) -> Handle {
        self.alloc(Object::DomPoint(Point4 {
            x,
            y,
            z,
            w,
            read_only,
            svg: None,
        }))
    }

    fn quad_points(&self, h: Handle) -> [Handle; 4] {
        match &*self.obj(h) {
            Object::DomQuad(p) => *p,
            _ => panic!("not a DOMQuad"),
        }
    }

    fn new_quad(&self, points: [Point4; 4]) -> Handle {
        let p = points.map(|p| {
            self.alloc(Object::DomPoint(Point4 {
                read_only: false,
                svg: None,
                ..p
            }))
        });
        self.alloc(Object::DomQuad(p))
    }

    fn quad_from_rect(&self, x: f64, y: f64, w: f64, h: f64) -> Handle {
        let pt = |x, y| Point4 {
            x,
            y,
            z: 0.0,
            w: 1.0,
            read_only: false,
            svg: None,
        };
        self.new_quad([pt(x, y), pt(x + w, y), pt(x + w, y + h), pt(x, y + h)])
    }

    fn matrix(&self, h: Handle) -> Matrix {
        let (m, svg) = match &*self.obj(h) {
            Object::DomMatrix(m) => (m.m, m.svg),
            _ => panic!("not a DOMMatrix"),
        };
        match svg {
            Some(t) => self.svg_linked_matrix(t),
            None => m,
        }
    }

    pub(crate) fn new_matrix(&self, m: Matrix, read_only: bool) -> Handle {
        self.alloc(Object::DomMatrix(Box::new(MatrixData {
            m,
            read_only,
            svg: None,
        })))
    }

    /// Runs a mutable transform method on `this` and returns it.
    fn matrix_self(
        &self,
        cx: &mut Cx,
        this: Handle,
        f: impl FnOnce(&mut Matrix),
    ) -> rt::Result<Handle> {
        if let Some(t) = self.matrix_link(this) {
            if self.svg_linked_matrix_read_only(t) {
                return Err(super::svg::read_only_error());
            }
            let mut m = self.svg_linked_matrix(t);
            f(&mut m);
            self.svg_linked_matrix_set(cx, t, &m);
            return Ok(this);
        }
        if let Object::DomMatrix(m) = &mut *self.obj_mut(this) {
            f(&mut m.m);
        }
        Ok(this)
    }

    /// The `SVGTransform` a `DOMMatrix` reflects.
    fn matrix_link(&self, h: Handle) -> Option<Handle> {
        match &*self.obj(h) {
            Object::DomMatrix(m) => m.svg,
            _ => None,
        }
    }

    /// An immutable transform method: the mutable one on a new `DOMMatrix` copy.
    fn matrix_copy(&self, this: Handle, f: impl FnOnce(&mut Matrix)) -> rt::Result<Handle> {
        let mut m = self.matrix(this);
        f(&mut m);
        Ok(self.new_matrix(m, false))
    }

    /// A `DOMMatrix` element setter (Geometry §6.6): a 3D element set to anything but its identity
    /// value makes the matrix 3D.
    fn set_element(
        &self,
        cx: &mut Cx,
        this: Handle,
        i: usize,
        j: usize,
        value: f64,
    ) -> rt::Result<()> {
        if self.matrix_link(this).is_some() {
            let identity = if i == j { 1.0 } else { 0.0 };
            let element_2d = matches!((i, j), (0, 0) | (0, 1) | (1, 0) | (1, 1) | (3, 0) | (3, 1));
            return self
                .matrix_self(cx, this, |m| {
                    m.m[i][j] = value;
                    if !element_2d && value != identity {
                        m.is_2d = false;
                    }
                })
                .map(|_| ());
        }
        if let Object::DomMatrix(m) = &mut *self.obj_mut(this) {
            m.m.m[i][j] = value;
            let element_2d = matches!((i, j), (0, 0) | (0, 1) | (1, 0) | (1, 1) | (3, 0) | (3, 1));
            let identity = if i == j { 1.0 } else { 0.0 };
            if !element_2d && value != identity {
                m.m.is_2d = false;
            }
        }
        Ok(())
    }

    /// The `DOMMatrixReadOnly`/`DOMMatrix` constructor steps (Geometry §6.3).
    fn construct_matrix(
        &self,
        cx: &mut Cx,
        init: Option<StringOrUnrestrictedDoubleSequence>,
        read_only: bool,
    ) -> rt::Result<Handle> {
        let m = match init {
            None => Matrix::identity(),
            Some(StringOrUnrestrictedDoubleSequence::String(s)) => {
                let window = self
                    .current_global(cx)
                    .is_some_and(|g| matches!(&*self.obj(g), Object::Window));
                if !window {
                    return Err(Error::Type(
                        "A DOMMatrix is made from a string only in a Window".into(),
                    ));
                }
                matrix::parse_transform_list(&s).map_err(|_| {
                    exception(
                        ExceptionName::SyntaxError,
                        "The string is not a transform list",
                    )
                })?
            }
            Some(StringOrUnrestrictedDoubleSequence::UnrestrictedDoubleSequence(v)) => {
                matrix_from_values(&v)?
            }
        };
        Ok(self.new_matrix(m, read_only))
    }

    /// `fromFloat32Array` / `fromFloat64Array`.
    fn matrix_from_array(
        &self,
        cx: &mut Cx,
        array: JsObject,
        read_only: bool,
    ) -> rt::Result<Handle> {
        let values: Vec<f64> = {
            let scope = cx.scope();
            let obj = v8::Local::new(scope, array.global());
            if let Ok(a) = v8::Local::<v8::Float32Array>::try_from(obj) {
                let mut bytes = vec![0u8; a.byte_length()];
                a.copy_contents(&mut bytes);
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes(*c) as f64)
                    .collect()
            } else if let Ok(a) = v8::Local::<v8::Float64Array>::try_from(obj) {
                let mut bytes = vec![0u8; a.byte_length()];
                a.copy_contents(&mut bytes);
                bytes
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| f64::from_le_bytes(*c))
                    .collect()
            } else {
                return Err(Error::Type("Not a float array".into()));
            }
        };
        Ok(self.new_matrix(matrix_from_values(&values)?, read_only))
    }

    fn matrix_array(&self, cx: &mut Cx, this: Handle, single: bool) -> rt::Result<JsObject> {
        let e = self.matrix(this).elements();
        let bytes: Vec<u8> = if single {
            e.iter().flat_map(|v| (*v as f32).to_le_bytes()).collect()
        } else {
            e.iter().flat_map(|v| v.to_le_bytes()).collect()
        };
        let scope = cx.scope();
        let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
        let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
        let arr: v8::Local<v8::Object> = if single {
            v8::Float32Array::new(scope, ab, 0, 16)
                .ok_or(Error::Pending)?
                .into()
        } else {
            v8::Float64Array::new(scope, ab, 0, 16)
                .ok_or(Error::Pending)?
                .into()
        };
        Ok(JsObject::new(scope, arr))
    }
}

/// A 2D matrix of 6 values or a 3D one of 16; any other count is a TypeError.
fn matrix_from_values(v: &[f64]) -> rt::Result<Matrix> {
    match v.len() {
        6 => Ok(Matrix::from_2d(v[0], v[1], v[2], v[3], v[4], v[5])),
        16 => Ok(Matrix::from_3d(v)),
        n => Err(Error::Type(
            format!("A matrix has 6 or 16 elements, not {n}").into(),
        )),
    }
}

impl DOMPointReadOnlyImpl for Engine {
    fn x(&self, this: Handle) -> f64 {
        self.point(this).x
    }
    fn y(&self, this: Handle) -> f64 {
        self.point(this).y
    }
    fn z(&self, this: Handle) -> f64 {
        self.point(this).z
    }
    fn w(&self, this: Handle) -> f64 {
        self.point(this).w
    }
    fn static_from_point(&self, _cx: &mut Cx, other: DOMPointInit) -> rt::Result<Handle> {
        Ok(self.alloc(Object::DomPoint(Point4::from_init(&other, true))))
    }
    /// Geometry §3 `matrixTransform()`.
    fn matrix_transform(
        &self,
        _cx: &mut Cx,
        this: Handle,
        matrix: DOMMatrixInit,
    ) -> rt::Result<Handle> {
        let m = matrix_from_init(&matrix)?;
        let p = self.point(this);
        let [x, y, z, w] = m.transform_point([p.x, p.y, p.z, p.w]);
        Ok(self.new_point(x, y, z, w, false))
    }
    fn constructor(&self, _cx: &mut Cx, x: f64, y: f64, z: f64, w: f64) -> rt::Result<Handle> {
        Ok(self.new_point(x, y, z, w, true))
    }
}

impl DOMPointImpl for Engine {
    fn x(&self, this: Handle) -> f64 {
        self.point(this).x
    }
    fn set_x(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.point_set(cx, this, |p| p.x = value)
    }
    fn y(&self, this: Handle) -> f64 {
        self.point(this).y
    }
    fn set_y(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.point_set(cx, this, |p| p.y = value)
    }
    fn z(&self, this: Handle) -> f64 {
        self.point(this).z
    }
    fn set_z(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.point_set(cx, this, |p| p.z = value)
    }
    fn w(&self, this: Handle) -> f64 {
        self.point(this).w
    }
    fn set_w(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
        self.point_set(cx, this, |p| p.w = value)
    }
    fn static_from_point(&self, _cx: &mut Cx, other: DOMPointInit) -> rt::Result<Handle> {
        Ok(self.alloc(Object::DomPoint(Point4::from_init(&other, false))))
    }
    fn constructor(&self, _cx: &mut Cx, x: f64, y: f64, z: f64, w: f64) -> rt::Result<Handle> {
        Ok(self.new_point(x, y, z, w, false))
    }
}

impl DOMQuadImpl for Engine {
    fn p1(&self, this: Handle) -> Handle {
        self.quad_points(this)[0]
    }
    fn p2(&self, this: Handle) -> Handle {
        self.quad_points(this)[1]
    }
    fn p3(&self, this: Handle) -> Handle {
        self.quad_points(this)[2]
    }
    fn p4(&self, this: Handle) -> Handle {
        self.quad_points(this)[3]
    }
    fn static_from_rect(&self, _cx: &mut Cx, other: DOMRectInit) -> rt::Result<Handle> {
        Ok(self.quad_from_rect(other.x, other.y, other.width, other.height))
    }
    fn static_from_quad(&self, _cx: &mut Cx, other: DOMQuadInit) -> rt::Result<Handle> {
        let p = |p: &Option<DOMPointInit>| Point4::from_init(&p.clone().unwrap_or_default(), false);
        Ok(self.new_quad([p(&other.p1), p(&other.p2), p(&other.p3), p(&other.p4)]))
    }
    /// Geometry §5 `getBounds()`.
    fn get_bounds(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let pts = self.quad_points(this).map(|h| self.point(h));
        let xs = pts.map(|p| p.x);
        let ys = pts.map(|p| p.y);
        let (left, top) = (nan_safe(xs, false), nan_safe(ys, false));
        let (right, bottom) = (nan_safe(xs, true), nan_safe(ys, true));
        Ok(self.alloc(Object::DomRect(Rect::new(
            left,
            top,
            right - left,
            bottom - top,
        ))))
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        p1: DOMPointInit,
        p2: DOMPointInit,
        p3: DOMPointInit,
        p4: DOMPointInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_quad([&p1, &p2, &p3, &p4].map(|p| Point4::from_init(p, false))))
    }
}

macro_rules! matrix_getters {
    ($($name:ident => $i:expr, $j:expr;)*) => {
        $(fn $name(&self, this: Handle) -> f64 {
            self.matrix(this).m[$i][$j]
        })*
    };
}

macro_rules! matrix_setters {
    ($($name:ident => $i:expr, $j:expr;)*) => {
        $(fn $name(&self, cx: &mut Cx, this: Handle, value: f64) -> rt::Result<()> {
            self.set_element(cx, this, $i, $j, value)
        })*
    };
}

impl DOMMatrixReadOnlyImpl for Engine {
    matrix_getters! {
        a => 0, 0; b => 0, 1; c => 1, 0; d => 1, 1; e => 3, 0; f => 3, 1;
        m11 => 0, 0; m12 => 0, 1; m13 => 0, 2; m14 => 0, 3;
        m21 => 1, 0; m22 => 1, 1; m23 => 1, 2; m24 => 1, 3;
        m31 => 2, 0; m32 => 2, 1; m33 => 2, 2; m34 => 2, 3;
        m41 => 3, 0; m42 => 3, 1; m43 => 3, 2; m44 => 3, 3;
    }
    fn is2_d(&self, this: Handle) -> bool {
        self.matrix(this).is_2d
    }
    fn is_identity(&self, this: Handle) -> bool {
        self.matrix(this).is_identity()
    }
    /// Geometry §6.4 the stringifier: a non-finite element is an InvalidStateError.
    fn stringify(&self, _cx: &mut Cx, this: Handle) -> rt::Result<DomStr<'_>> {
        self.matrix(this)
            .to_css_string()
            .map(DomStr::Owned)
            .ok_or_else(|| {
                exception(
                    ExceptionName::InvalidStateError,
                    "The matrix has a non-finite element",
                )
            })
    }
    fn static_from_matrix(&self, _cx: &mut Cx, other: DOMMatrixInit) -> rt::Result<Handle> {
        Ok(self.new_matrix(matrix_from_init(&other)?, true))
    }
    fn static_from_float32_array(&self, cx: &mut Cx, array32: JsObject) -> rt::Result<Handle> {
        self.matrix_from_array(cx, array32, true)
    }
    fn static_from_float64_array(&self, cx: &mut Cx, array64: JsObject) -> rt::Result<Handle> {
        self.matrix_from_array(cx, array64, true)
    }
    fn translate(
        &self,
        _cx: &mut Cx,
        this: Handle,
        tx: f64,
        ty: f64,
        tz: f64,
    ) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| m.translate_self(tx, ty, tz))
    }
    #[allow(clippy::too_many_arguments)]
    fn scale(
        &self,
        _cx: &mut Cx,
        this: Handle,
        scale_x: f64,
        scale_y: Option<f64>,
        scale_z: f64,
        origin_x: f64,
        origin_y: f64,
        origin_z: f64,
    ) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| {
            m.scale_self(scale_x, scale_y, scale_z, origin_x, origin_y, origin_z)
        })
    }
    fn scale_non_uniform(
        &self,
        _cx: &mut Cx,
        this: Handle,
        scale_x: f64,
        scale_y: f64,
    ) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| {
            m.scale_self(scale_x, Some(scale_y), 1.0, 0.0, 0.0, 0.0)
        })
    }
    fn scale3d(
        &self,
        _cx: &mut Cx,
        this: Handle,
        scale: f64,
        origin_x: f64,
        origin_y: f64,
        origin_z: f64,
    ) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| {
            m.scale3d_self(scale, origin_x, origin_y, origin_z)
        })
    }
    fn rotate(
        &self,
        _cx: &mut Cx,
        this: Handle,
        rot_x: f64,
        rot_y: Option<f64>,
        rot_z: Option<f64>,
    ) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| m.rotate_self(rot_x, rot_y, rot_z))
    }
    fn rotate_from_vector(&self, _cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| m.rotate_from_vector_self(x, y))
    }
    fn rotate_axis_angle(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
        z: f64,
        angle: f64,
    ) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| m.rotate_axis_angle_self(x, y, z, angle))
    }
    fn skew_x(&self, _cx: &mut Cx, this: Handle, sx: f64) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| m.skew_x_self(sx))
    }
    fn skew_y(&self, _cx: &mut Cx, this: Handle, sy: f64) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| m.skew_y_self(sy))
    }
    fn multiply(&self, _cx: &mut Cx, this: Handle, other: DOMMatrixInit) -> rt::Result<Handle> {
        let o = matrix_from_init(&other)?;
        self.matrix_copy(this, |m| m.multiply_self(&o))
    }
    fn flip_x(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| {
            m.multiply_self(&Matrix::from_2d(-1.0, 0.0, 0.0, 1.0, 0.0, 0.0))
        })
    }
    fn flip_y(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| {
            m.multiply_self(&Matrix::from_2d(1.0, 0.0, 0.0, -1.0, 0.0, 0.0))
        })
    }
    fn inverse(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.matrix_copy(this, |m| m.invert_self())
    }
    fn transform_point(
        &self,
        _cx: &mut Cx,
        this: Handle,
        point: DOMPointInit,
    ) -> rt::Result<Handle> {
        let [x, y, z, w] = self
            .matrix(this)
            .transform_point([point.x, point.y, point.z, point.w]);
        Ok(self.new_point(x, y, z, w, false))
    }
    fn to_float32_array(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsObject> {
        self.matrix_array(cx, this, true)
    }
    fn to_float64_array(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsObject> {
        self.matrix_array(cx, this, false)
    }
    fn constructor(
        &self,
        cx: &mut Cx,
        init: Option<StringOrUnrestrictedDoubleSequence>,
    ) -> rt::Result<Handle> {
        self.construct_matrix(cx, init, true)
    }
}

impl DOMMatrixImpl for Engine {
    matrix_getters! {
        a => 0, 0; b => 0, 1; c => 1, 0; d => 1, 1; e => 3, 0; f => 3, 1;
        m11 => 0, 0; m12 => 0, 1; m13 => 0, 2; m14 => 0, 3;
        m21 => 1, 0; m22 => 1, 1; m23 => 1, 2; m24 => 1, 3;
        m31 => 2, 0; m32 => 2, 1; m33 => 2, 2; m34 => 2, 3;
        m41 => 3, 0; m42 => 3, 1; m43 => 3, 2; m44 => 3, 3;
    }
    matrix_setters! {
        set_a => 0, 0; set_b => 0, 1; set_c => 1, 0; set_d => 1, 1; set_e => 3, 0; set_f => 3, 1;
        set_m11 => 0, 0; set_m12 => 0, 1; set_m13 => 0, 2; set_m14 => 0, 3;
        set_m21 => 1, 0; set_m22 => 1, 1; set_m23 => 1, 2; set_m24 => 1, 3;
        set_m31 => 2, 0; set_m32 => 2, 1; set_m33 => 2, 2; set_m34 => 2, 3;
        set_m41 => 3, 0; set_m42 => 3, 1; set_m43 => 3, 2; set_m44 => 3, 3;
    }
    fn static_from_matrix(&self, _cx: &mut Cx, other: DOMMatrixInit) -> rt::Result<Handle> {
        Ok(self.new_matrix(matrix_from_init(&other)?, false))
    }
    fn static_from_float32_array(&self, cx: &mut Cx, array32: JsObject) -> rt::Result<Handle> {
        self.matrix_from_array(cx, array32, false)
    }
    fn static_from_float64_array(&self, cx: &mut Cx, array64: JsObject) -> rt::Result<Handle> {
        self.matrix_from_array(cx, array64, false)
    }
    fn multiply_self(&self, cx: &mut Cx, this: Handle, other: DOMMatrixInit) -> rt::Result<Handle> {
        let o = matrix_from_init(&other)?;
        self.matrix_self(cx, this, |m| m.multiply_self(&o))
    }
    fn pre_multiply_self(
        &self,
        cx: &mut Cx,
        this: Handle,
        other: DOMMatrixInit,
    ) -> rt::Result<Handle> {
        let o = matrix_from_init(&other)?;
        self.matrix_self(cx, this, |m| m.pre_multiply_self(&o))
    }
    fn translate_self(
        &self,
        cx: &mut Cx,
        this: Handle,
        tx: f64,
        ty: f64,
        tz: f64,
    ) -> rt::Result<Handle> {
        self.matrix_self(cx, this, |m| m.translate_self(tx, ty, tz))
    }
    #[allow(clippy::too_many_arguments)]
    fn scale_self(
        &self,
        cx: &mut Cx,
        this: Handle,
        scale_x: f64,
        scale_y: Option<f64>,
        scale_z: f64,
        origin_x: f64,
        origin_y: f64,
        origin_z: f64,
    ) -> rt::Result<Handle> {
        self.matrix_self(cx, this, |m| {
            m.scale_self(scale_x, scale_y, scale_z, origin_x, origin_y, origin_z)
        })
    }
    fn scale3d_self(
        &self,
        cx: &mut Cx,
        this: Handle,
        scale: f64,
        origin_x: f64,
        origin_y: f64,
        origin_z: f64,
    ) -> rt::Result<Handle> {
        self.matrix_self(cx, this, |m| {
            m.scale3d_self(scale, origin_x, origin_y, origin_z)
        })
    }
    fn rotate_self(
        &self,
        cx: &mut Cx,
        this: Handle,
        rot_x: f64,
        rot_y: Option<f64>,
        rot_z: Option<f64>,
    ) -> rt::Result<Handle> {
        self.matrix_self(cx, this, |m| m.rotate_self(rot_x, rot_y, rot_z))
    }
    fn rotate_from_vector_self(
        &self,
        cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
    ) -> rt::Result<Handle> {
        self.matrix_self(cx, this, |m| m.rotate_from_vector_self(x, y))
    }
    fn rotate_axis_angle_self(
        &self,
        cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
        z: f64,
        angle: f64,
    ) -> rt::Result<Handle> {
        self.matrix_self(cx, this, |m| m.rotate_axis_angle_self(x, y, z, angle))
    }
    fn skew_x_self(&self, cx: &mut Cx, this: Handle, sx: f64) -> rt::Result<Handle> {
        self.matrix_self(cx, this, |m| m.skew_x_self(sx))
    }
    fn skew_y_self(&self, cx: &mut Cx, this: Handle, sy: f64) -> rt::Result<Handle> {
        self.matrix_self(cx, this, |m| m.skew_y_self(sy))
    }
    fn invert_self(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.matrix_self(cx, this, |m| m.invert_self())
    }
    /// Geometry §6.5 `setMatrixValue()`.
    fn set_matrix_value(
        &self,
        cx: &mut Cx,
        this: Handle,
        transform_list: &str,
    ) -> rt::Result<Handle> {
        let parsed = matrix::parse_transform_list(transform_list).map_err(|_| {
            exception(
                ExceptionName::SyntaxError,
                "The string is not a transform list",
            )
        })?;
        self.matrix_self(cx, this, |m| *m = parsed)
    }
    fn constructor(
        &self,
        cx: &mut Cx,
        init: Option<StringOrUnrestrictedDoubleSequence>,
    ) -> rt::Result<Handle> {
        self.construct_matrix(cx, init, false)
    }
}

// ----- CSSOM View §8 GeometryUtils ------------------------------------------------------------------

impl Engine {
    fn geometry_node(&self, n: &TextOrElementOrDocument) -> Option<NodeId> {
        let h = match n {
            TextOrElementOrDocument::Text(h)
            | TextOrElementOrDocument::Element(h)
            | TextOrElementOrDocument::Document(h) => *h,
        };
        super::handle_node(h)
    }

    /// The `which` box of `node` in its viewport's coordinates: the border box's fragments, or the
    /// padding or content box (the margin box is taken as the border box, which is what this layout
    /// keeps). A document's box is its scrolled viewport; text nodes have none here.
    fn geometry_boxes(&self, node: NodeId, which: CSSBoxType) -> Vec<Rect> {
        let (is_doc, is_el) = {
            let dom = self.dom();
            (dom.is_document(node), dom.element(node).is_some())
        };
        if is_doc {
            let scroll = self
                .window_of_document(node)
                .map_or((0.0, 0.0), |w| self.window_scroll(w));
            let (w, h) = self.view_viewport_size(node).unwrap_or((0.0, 0.0));
            return vec![Rect::new(-scroll.0, -scroll.1, w, h)];
        }
        if !is_el {
            return Vec::new();
        }
        match which {
            CSSBoxType::Content => self.view_content_box(node).into_iter().collect(),
            CSSBoxType::Padding => self
                .view_padding_box(node)
                .map(|(r, _)| r)
                .into_iter()
                .collect(),
            CSSBoxType::Border | CSSBoxType::Margin => self.view_client_rects(node),
        }
    }

    /// The origin of `node`'s `which` box in viewport coordinates (0, 0 without one).
    fn geometry_origin(&self, node: NodeId, which: CSSBoxType) -> (f64, f64) {
        self.geometry_boxes(node, which)
            .first()
            .map_or((0.0, 0.0), |r| (r.x, r.y))
    }

    /// What converting from `from`'s box to `this`'s adds to a coordinate.
    fn geometry_shift(
        &self,
        this: Handle,
        from: &TextOrElementOrDocument,
        options: &g::ConvertCoordinateOptions,
    ) -> (f64, f64) {
        let from = self.geometry_node(from);
        let to = super::handle_node(this);
        let (fx, fy) = from.map_or((0.0, 0.0), |n| self.geometry_origin(n, options.from_box));
        let (tx, ty) = to.map_or((0.0, 0.0), |n| self.geometry_origin(n, options.to_box));
        (fx - tx, fy - ty)
    }
}

impl GeometryUtilsImpl for Engine {
    fn get_box_quads(
        &self,
        _cx: &mut Cx,
        this: Handle,
        options: g::BoxQuadOptions,
    ) -> rt::Result<Vec<Handle>> {
        let Some(node) = super::handle_node(this) else {
            return Ok(Vec::new());
        };
        let (ox, oy) = match &options.relative_to {
            Some(r) => self
                .geometry_node(r)
                .map_or((0.0, 0.0), |n| self.geometry_origin(n, CSSBoxType::Border)),
            None => {
                let doc = self.dom().node_document(node);
                self.geometry_origin(doc, CSSBoxType::Border)
            }
        };
        Ok(self
            .geometry_boxes(node, options.box_)
            .into_iter()
            .map(|r| self.quad_from_rect(r.x - ox, r.y - oy, r.width, r.height))
            .collect())
    }
    fn convert_quad_from_node(
        &self,
        _cx: &mut Cx,
        this: Handle,
        quad: DOMQuadInit,
        from: TextOrElementOrDocument,
        options: g::ConvertCoordinateOptions,
    ) -> rt::Result<Handle> {
        let (dx, dy) = self.geometry_shift(this, &from, &options);
        let p = |p: &Option<DOMPointInit>| {
            let mut p = Point4::from_init(&p.clone().unwrap_or_default(), false);
            p.x += dx;
            p.y += dy;
            p
        };
        Ok(self.new_quad([p(&quad.p1), p(&quad.p2), p(&quad.p3), p(&quad.p4)]))
    }
    fn convert_rect_from_node(
        &self,
        _cx: &mut Cx,
        this: Handle,
        rect: Handle,
        from: TextOrElementOrDocument,
        options: g::ConvertCoordinateOptions,
    ) -> rt::Result<Handle> {
        let (dx, dy) = self.geometry_shift(this, &from, &options);
        let r = self.rect(rect);
        Ok(self.quad_from_rect(r.x + dx, r.y + dy, r.width, r.height))
    }
    fn convert_point_from_node(
        &self,
        _cx: &mut Cx,
        this: Handle,
        point: DOMPointInit,
        from: TextOrElementOrDocument,
        options: g::ConvertCoordinateOptions,
    ) -> rt::Result<Handle> {
        let (dx, dy) = self.geometry_shift(this, &from, &options);
        Ok(self.new_point(point.x + dx, point.y + dy, point.z, point.w, false))
    }
}
