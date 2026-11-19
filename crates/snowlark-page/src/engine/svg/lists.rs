// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The list interfaces (SVG 2 §4.5.4 and §4.5.5–§4.5.7, §8.14.2 `SVGTransformList`, §10.7
//! `SVGPointList`): synchronization with the reflected attribute, attaching and detaching of
//! elements, the list methods and reserialization.

use snowlark_bindings::generated::{
    DOMMatrix2DInit, SVGLengthListImpl, SVGNumberListImpl, SVGPointListImpl, SVGStringListImpl,
    SVGTransformListImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};
use snowlark_dom::NodeId;
use snowlark_layout::svg::{parse, transform as tf, Length, LengthDir, Matrix, Transform};

use super::super::geometry::Point4;
use super::super::{exception, Engine, Object};
use super::reflect::{Kind, Spec};
use super::{read_only_error, ItemValue, List, ListItems, Owner, SvgObject};
use rt::ExceptionName;

/// The values of a list attribute.
enum Values {
    Items(Vec<ItemValue>),
    Points(Vec<(f64, f64)>),
    Strings(Vec<String>),
}

/// A `<length>` list (lengths separated by `comma-wsp`); `None` when in error.
fn parse_length_list(s: &str) -> Option<Vec<Length>> {
    let mut sc = parse::Scanner::new(s);
    let mut out = Vec::new();
    sc.skip_wsp();
    while !sc.at_end() {
        let start = sc.i;
        while sc.peek().is_some_and(|b| !parse::is_wsp(b) && b != b',') {
            sc.i += 1;
        }
        let token = std::str::from_utf8(&sc.s[start..sc.i]).ok()?;
        out.push(Length::parse(token)?);
        let comma = sc.skip_comma_wsp();
        if comma && sc.at_end() {
            return None;
        }
    }
    Some(out)
}

/// A list attribute's value parsed (the initial value, the empty list, when absent or invalid).
fn parse_values(spec: &Spec, v: Option<&str>) -> Values {
    if let (Kind::StringList { .. }, None) = (&spec.kind, v) {
        return Values::Strings(Vec::new());
    }
    let v = v.unwrap_or("");
    match spec.kind {
        Kind::LengthList(dir) => Values::Items(
            parse_length_list(v)
                .unwrap_or_default()
                .into_iter()
                .map(|l| ItemValue::Length(l, dir))
                .collect(),
        ),
        Kind::NumberList => Values::Items(
            parse::parse_number_list(v)
                .unwrap_or_default()
                .into_iter()
                .map(|n| ItemValue::Number(n as f32))
                .collect(),
        ),
        Kind::TransformList => Values::Items(
            tf::parse_transform_list(v)
                .unwrap_or_default()
                .into_iter()
                .map(|t| ItemValue::Transform(t, None))
                .collect(),
        ),
        Kind::PointList => Values::Points(parse::parse_points(v).unwrap_or_default()),
        Kind::StringList { commas } => Values::Strings(parse::split_string_list(v, commas)),
        _ => Values::Items(Vec::new()),
    }
}

fn index_error() -> rt::Error {
    exception(
        ExceptionName::IndexSizeError,
        "The index is greater than or equal to the number of items.",
    )
}

impl Engine {
    pub(crate) fn new_svg_list(&self, element: NodeId, spec: &'static Spec, anim: bool) -> Handle {
        let items = match spec.kind {
            Kind::StringList { .. } => ListItems::Strings(Vec::new()),
            _ => ListItems::Objects(Vec::new()),
        };
        self.alloc(Object::Svg(SvgObject::List(Box::new(List {
            element,
            spec,
            anim,
            items,
            synced: None,
        }))))
    }

    fn svg_list_info(&self, list: Handle) -> (NodeId, &'static Spec, bool) {
        self.svg_obj(list, |o| match o {
            SvgObject::List(l) => (l.element, l.spec, l.anim),
            _ => panic!("not an SVG list"),
        })
    }

    fn svg_list_items(&self, list: Handle) -> Vec<Handle> {
        self.svg_obj(list, |o| match o {
            SvgObject::List(l) => match &l.items {
                ListItems::Objects(v) => v.clone(),
                ListItems::Strings(_) => Vec::new(),
            },
            _ => Vec::new(),
        })
    }

    fn set_svg_list_items(&self, list: Handle, items: Vec<Handle>) {
        self.svg_obj_mut(list, |o| {
            if let SvgObject::List(l) = o {
                l.items = ListItems::Objects(items);
            }
        });
    }

    /// A new element object for a list, attached to it (§4.5.4 "attach").
    fn svg_new_list_item(&self, list: Handle, spec: &Spec, anim: bool, v: ItemValue) -> Handle {
        let v = match (v, &spec.kind) {
            (ItemValue::Length(l, _), Kind::LengthList(dir)) => ItemValue::Length(l, *dir),
            (v, _) => v,
        };
        self.new_svg_item(v, Owner::List(list), anim)
    }

    fn svg_new_point(&self, list: Handle, anim: bool, (x, y): (f64, f64)) -> Handle {
        self.alloc(Object::DomPoint(Point4 {
            x,
            y,
            z: 0.0,
            w: 1.0,
            read_only: anim,
            svg: Some(list),
        }))
    }

    /// Detaches a list element (§4.5.4 "detach"): no element, not read only, unspecified
    /// directionality; its value is kept.
    pub(crate) fn svg_detach(&self, h: Handle) {
        let mut objects = self.objects.borrow_mut();
        match objects.get_mut(h.get() as usize - 1) {
            Some(Object::Svg(SvgObject::Item(i))) => {
                i.owner = Owner::Detached;
                i.read_only = false;
                if let ItemValue::Length(_, dir) = &mut i.value {
                    *dir = LengthDir::Other;
                }
            }
            Some(Object::DomPoint(p)) => {
                p.svg = None;
                p.read_only = false;
            }
            _ => {}
        }
    }

    /// Synchronizes a list with its attribute (§4.5.4) when the attribute changed since the list
    /// last synchronized or serialized.
    pub(crate) fn svg_sync_list(&self, list: Handle) {
        let (element, spec, anim) = self.svg_list_info(list);
        // (the common read: the base value as last seen, compared where it lies)
        if !anim {
            let dom = self.dom();
            if dom.is_alive(element) && dom.is_element(element) {
                let index = dom.attr_index(element, "", spec.attr);
                let now = index.map(|i| dom.attrs(element)[i].value());
                let same = self.svg_obj(list, |o| match o {
                    SvgObject::List(l) => l.synced.as_ref().is_some_and(|s| s.as_deref() == now),
                    _ => false,
                });
                if same {
                    return;
                }
            }
        }
        let cur = self.svg_value(element, spec, anim);
        let same = self.svg_obj(list, |o| match o {
            SvgObject::List(l) => l.synced.as_ref() == Some(&cur),
            _ => false,
        });
        if same {
            return;
        }
        match parse_values(spec, cur.as_deref()) {
            Values::Strings(s) => self.svg_obj_mut(list, |o| {
                if let SvgObject::List(l) = o {
                    l.items = ListItems::Strings(s);
                }
            }),
            Values::Items(values) => {
                let mut items = self.svg_list_items(list);
                for &h in items.iter().skip(values.len()) {
                    self.svg_detach(h);
                }
                items.truncate(values.len());
                for v in values.iter().skip(items.len()) {
                    items.push(self.svg_new_list_item(list, spec, anim, v.clone()));
                }
                for (&h, v) in items.iter().zip(values) {
                    self.svg_obj_mut(h, |o| {
                        if let SvgObject::Item(i) = o {
                            i.value = match (&i.value, v) {
                                (ItemValue::Transform(_, m), ItemValue::Transform(t, _)) => {
                                    ItemValue::Transform(t, *m)
                                }
                                (_, v) => v,
                            };
                        }
                    });
                }
                self.set_svg_list_items(list, items);
            }
            Values::Points(values) => {
                let mut items = self.svg_list_items(list);
                for &h in items.iter().skip(values.len()) {
                    self.svg_detach(h);
                }
                items.truncate(values.len());
                for &v in values.iter().skip(items.len()) {
                    items.push(self.svg_new_point(list, anim, v));
                }
                for (&h, (x, y)) in items.iter().zip(values) {
                    if let Object::DomPoint(p) = &mut *self.obj_mut(h) {
                        p.x = x;
                        p.y = y;
                    }
                }
                self.set_svg_list_items(list, items);
            }
        }
        self.svg_obj_mut(list, |o| {
            if let SvgObject::List(l) = o {
                l.synced = Some(cur);
            }
        });
    }

    /// The serialization of a list's elements (§4.3.5).
    fn svg_serialize_list(&self, list: Handle) -> String {
        let strings = self.svg_obj(list, |o| match o {
            SvgObject::List(l) => match &l.items {
                ListItems::Strings(s) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        });
        if let Some(s) = strings {
            return s.join(" ");
        }
        let mut parts = Vec::new();
        for h in self.svg_list_items(list) {
            let objects = self.objects.borrow();
            match objects.get(h.get() as usize - 1) {
                Some(Object::Svg(SvgObject::Item(i))) => parts.push(match &i.value {
                    ItemValue::Number(n) => parse::fmt_number(*n),
                    ItemValue::Length(l, _) => l.to_css(),
                    ItemValue::Angle(a) => a.to_css(),
                    ItemValue::Transform(t, _) => t.to_css(),
                }),
                Some(Object::DomPoint(p)) => {
                    parts.push(format!("{},{}", parse::fmt_f64(p.x), parse::fmt_f64(p.y)))
                }
                _ => {}
            }
        }
        parts.join(" ")
    }

    /// Reserializes the list's attribute from its elements.
    pub(crate) fn svg_reserialize_list(&self, cx: &mut Cx, list: Handle) {
        let (element, spec, _) = self.svg_list_info(list);
        let s = self.svg_serialize_list(list);
        self.svg_set_attr(cx, element, spec.attr, &s);
        self.svg_obj_mut(list, |o| {
            if let SvgObject::List(l) = o {
                l.synced = Some(Some(s));
            }
        });
    }

    /// The list's length after synchronizing.
    fn svg_list_len(&self, list: Handle) -> u32 {
        self.svg_sync_list(list);
        self.svg_obj(list, |o| match o {
            SvgObject::List(l) => match &l.items {
                ListItems::Objects(v) => v.len() as u32,
                ListItems::Strings(v) => v.len() as u32,
            },
            _ => 0,
        })
    }

    fn svg_list_check_writable(&self, list: Handle) -> rt::Result<()> {
        self.svg_sync_list(list);
        if self.svg_list_info(list).2 {
            return Err(read_only_error());
        }
        Ok(())
    }

    /// `newItem` as an element to insert: itself when detached, else a new detached copy, then
    /// attached to `list` (§4.5.4).
    fn svg_adopt_item(&self, list: Handle, item: Handle) -> Handle {
        let (_, spec, anim) = self.svg_list_info(list);
        if spec.kind == Kind::PointList {
            let p = match &*self.obj(item) {
                Object::DomPoint(p) => *p,
                _ => Point4 {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    w: 1.0,
                    read_only: false,
                    svg: None,
                },
            };
            if p.svg.is_none() && !p.read_only {
                if let Object::DomPoint(p) = &mut *self.obj_mut(item) {
                    p.svg = Some(list);
                }
                return item;
            }
            return self.svg_new_point(list, anim, (p.x, p.y));
        }
        let (value, owner, _) = self.svg_item(item);
        if owner == Owner::Detached {
            self.svg_obj_mut(item, |o| {
                if let SvgObject::Item(i) = o {
                    i.owner = Owner::List(list);
                    if let (ItemValue::Length(_, dir), Kind::LengthList(d)) =
                        (&mut i.value, &spec.kind)
                    {
                        *dir = *d;
                    }
                }
            });
            return item;
        }
        let value = match value {
            ItemValue::Transform(t, _) => ItemValue::Transform(t, None),
            v => v,
        };
        self.svg_new_list_item(list, spec, anim, value)
    }

    fn svg_list_get(&self, list: Handle, index: u32) -> rt::Result<Handle> {
        let items = {
            self.svg_sync_list(list);
            self.svg_list_items(list)
        };
        items.get(index as usize).copied().ok_or_else(index_error)
    }

    fn svg_list_clear(&self, cx: &mut Cx, list: Handle) -> rt::Result<()> {
        self.svg_list_check_writable(list)?;
        for h in self.svg_list_items(list) {
            self.svg_detach(h);
        }
        self.svg_obj_mut(list, |o| {
            if let SvgObject::List(l) = o {
                l.items = match l.items {
                    ListItems::Strings(_) => ListItems::Strings(Vec::new()),
                    ListItems::Objects(_) => ListItems::Objects(Vec::new()),
                };
            }
        });
        self.svg_reserialize_list(cx, list);
        Ok(())
    }

    fn svg_list_initialize(&self, cx: &mut Cx, list: Handle, item: Handle) -> rt::Result<Handle> {
        self.svg_list_check_writable(list)?;
        for h in self.svg_list_items(list) {
            self.svg_detach(h);
        }
        self.set_svg_list_items(list, Vec::new());
        let item = self.svg_adopt_item(list, item);
        self.set_svg_list_items(list, vec![item]);
        self.svg_reserialize_list(cx, list);
        Ok(item)
    }

    fn svg_list_insert(
        &self,
        cx: &mut Cx,
        list: Handle,
        item: Handle,
        index: u32,
    ) -> rt::Result<Handle> {
        self.svg_list_check_writable(list)?;
        let item = self.svg_adopt_item(list, item);
        let mut items = self.svg_list_items(list);
        let i = (index as usize).min(items.len());
        items.insert(i, item);
        self.set_svg_list_items(list, items);
        self.svg_reserialize_list(cx, list);
        Ok(item)
    }

    fn svg_list_replace(
        &self,
        cx: &mut Cx,
        list: Handle,
        item: Handle,
        index: u32,
    ) -> rt::Result<Handle> {
        self.svg_list_check_writable(list)?;
        let mut items = self.svg_list_items(list);
        if index as usize >= items.len() {
            return Err(index_error());
        }
        let item = self.svg_adopt_item(list, item);
        let old = std::mem::replace(&mut items[index as usize], item);
        if old != item {
            self.svg_detach(old);
        }
        self.set_svg_list_items(list, items);
        self.svg_reserialize_list(cx, list);
        Ok(item)
    }

    fn svg_list_remove(&self, cx: &mut Cx, list: Handle, index: u32) -> rt::Result<Handle> {
        self.svg_list_check_writable(list)?;
        let mut items = self.svg_list_items(list);
        if index as usize >= items.len() {
            return Err(index_error());
        }
        let item = items.remove(index as usize);
        self.svg_detach(item);
        self.set_svg_list_items(list, items);
        self.svg_reserialize_list(cx, list);
        Ok(item)
    }

    fn svg_list_append(&self, cx: &mut Cx, list: Handle, item: Handle) -> rt::Result<Handle> {
        let n = self.svg_list_len(list);
        self.svg_list_insert(cx, list, item, n)
    }

    // ----- string lists ----------------------------------------------------------------------------

    fn svg_strings(&self, list: Handle) -> Vec<String> {
        self.svg_sync_list(list);
        self.svg_obj(list, |o| match o {
            SvgObject::List(l) => match &l.items {
                ListItems::Strings(s) => s.clone(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        })
    }

    fn svg_set_strings(&self, cx: &mut Cx, list: Handle, s: Vec<String>) {
        self.svg_obj_mut(list, |o| {
            if let SvgObject::List(l) = o {
                l.items = ListItems::Strings(s);
            }
        });
        self.svg_reserialize_list(cx, list);
    }
}

macro_rules! object_list_impl {
    ($trait:ident) => {
        impl $trait for Engine {
            fn length(&self, this: Handle) -> u32 {
                self.svg_list_len(this)
            }
            fn number_of_items(&self, this: Handle) -> u32 {
                self.svg_list_len(this)
            }
            fn get_item(&self, this: Handle, index: u32) -> Option<Handle> {
                self.svg_list_get(this, index).ok()
            }
            fn indexed_setter(
                &self,
                cx: &mut Cx,
                this: Handle,
                index: u32,
                value: Handle,
            ) -> rt::Result<()> {
                self.svg_list_replace(cx, this, value, index).map(|_| ())
            }
            fn clear(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
                self.svg_list_clear(cx, this)
            }
            fn initialize(
                &self,
                cx: &mut Cx,
                this: Handle,
                new_item: Handle,
            ) -> rt::Result<Handle> {
                self.svg_list_initialize(cx, this, new_item)
            }
            fn insert_item_before(
                &self,
                cx: &mut Cx,
                this: Handle,
                new_item: Handle,
                index: u32,
            ) -> rt::Result<Handle> {
                self.svg_list_insert(cx, this, new_item, index)
            }
            fn replace_item(
                &self,
                cx: &mut Cx,
                this: Handle,
                new_item: Handle,
                index: u32,
            ) -> rt::Result<Handle> {
                self.svg_list_replace(cx, this, new_item, index)
            }
            fn remove_item(&self, cx: &mut Cx, this: Handle, index: u32) -> rt::Result<Handle> {
                self.svg_list_remove(cx, this, index)
            }
            fn append_item(
                &self,
                cx: &mut Cx,
                this: Handle,
                new_item: Handle,
            ) -> rt::Result<Handle> {
                self.svg_list_append(cx, this, new_item)
            }
        }
    };
}

object_list_impl!(SVGNumberListImpl);
object_list_impl!(SVGLengthListImpl);
object_list_impl!(SVGPointListImpl);

impl SVGTransformListImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        self.svg_list_len(this)
    }
    fn number_of_items(&self, this: Handle) -> u32 {
        self.svg_list_len(this)
    }
    fn get_item(&self, this: Handle, index: u32) -> Option<Handle> {
        self.svg_list_get(this, index).ok()
    }
    fn indexed_setter(
        &self,
        cx: &mut Cx,
        this: Handle,
        index: u32,
        value: Handle,
    ) -> rt::Result<()> {
        self.svg_list_replace(cx, this, value, index).map(|_| ())
    }
    fn clear(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.svg_list_clear(cx, this)
    }
    fn initialize(&self, cx: &mut Cx, this: Handle, new_item: Handle) -> rt::Result<Handle> {
        self.svg_list_initialize(cx, this, new_item)
    }
    fn insert_item_before(
        &self,
        cx: &mut Cx,
        this: Handle,
        new_item: Handle,
        index: u32,
    ) -> rt::Result<Handle> {
        self.svg_list_insert(cx, this, new_item, index)
    }
    fn replace_item(
        &self,
        cx: &mut Cx,
        this: Handle,
        new_item: Handle,
        index: u32,
    ) -> rt::Result<Handle> {
        self.svg_list_replace(cx, this, new_item, index)
    }
    fn remove_item(&self, cx: &mut Cx, this: Handle, index: u32) -> rt::Result<Handle> {
        self.svg_list_remove(cx, this, index)
    }
    fn append_item(&self, cx: &mut Cx, this: Handle, new_item: Handle) -> rt::Result<Handle> {
        self.svg_list_append(cx, this, new_item)
    }
    /// SVG 2 §8.14.2 `createSVGTransformFromMatrix()`.
    fn create_svg_transform_from_matrix(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        matrix: DOMMatrix2DInit,
    ) -> rt::Result<Handle> {
        let m = super::super::geometry::matrix_from_2d_init(&matrix)?;
        Ok(self.new_svg_item(
            ItemValue::Transform(Transform::matrix(Matrix::new(m)), None),
            Owner::Detached,
            false,
        ))
    }
    /// SVG 2 §8.14.2 `consolidate()`.
    fn consolidate(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        self.svg_list_check_writable(this)?;
        let items = self.svg_list_items(this);
        if items.is_empty() {
            return Ok(None);
        }
        let mut m = Matrix::IDENTITY;
        for &h in &items {
            if let (ItemValue::Transform(t, _), _, _) = self.svg_item(h) {
                m *= t.matrix;
            }
            self.svg_detach(h);
        }
        let (_, spec, anim) = self.svg_list_info(this);
        let t = self.svg_new_list_item(
            this,
            spec,
            anim,
            ItemValue::Transform(Transform::matrix(m), None),
        );
        self.set_svg_list_items(this, vec![t]);
        self.svg_reserialize_list(cx, this);
        Ok(Some(t))
    }
}

impl SVGStringListImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        self.svg_list_len(this)
    }
    fn number_of_items(&self, this: Handle) -> u32 {
        self.svg_list_len(this)
    }
    fn get_item(&self, this: Handle, index: u32) -> Option<DomStr<'_>> {
        self.svg_strings(this)
            .get(index as usize)
            .map(|s| DomStr::Owned(s.clone()))
    }
    fn indexed_setter(&self, cx: &mut Cx, this: Handle, index: u32, value: &str) -> rt::Result<()> {
        SVGStringListImpl::replace_item(self, cx, this, value, index).map(|_| ())
    }
    fn clear(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.svg_strings(this);
        self.svg_set_strings(cx, this, Vec::new());
        Ok(())
    }
    fn initialize(&self, cx: &mut Cx, this: Handle, new_item: &str) -> rt::Result<DomStr<'_>> {
        self.svg_strings(this);
        self.svg_set_strings(cx, this, vec![new_item.to_owned()]);
        Ok(DomStr::Owned(new_item.to_owned()))
    }
    fn insert_item_before(
        &self,
        cx: &mut Cx,
        this: Handle,
        new_item: &str,
        index: u32,
    ) -> rt::Result<DomStr<'_>> {
        let mut s = self.svg_strings(this);
        let i = (index as usize).min(s.len());
        s.insert(i, new_item.to_owned());
        self.svg_set_strings(cx, this, s);
        Ok(DomStr::Owned(new_item.to_owned()))
    }
    fn replace_item(
        &self,
        cx: &mut Cx,
        this: Handle,
        new_item: &str,
        index: u32,
    ) -> rt::Result<DomStr<'_>> {
        let mut s = self.svg_strings(this);
        let slot = s.get_mut(index as usize).ok_or_else(index_error)?;
        *slot = new_item.to_owned();
        self.svg_set_strings(cx, this, s);
        Ok(DomStr::Owned(new_item.to_owned()))
    }
    fn remove_item(&self, cx: &mut Cx, this: Handle, index: u32) -> rt::Result<DomStr<'_>> {
        let mut s = self.svg_strings(this);
        if index as usize >= s.len() {
            return Err(index_error());
        }
        let removed = s.remove(index as usize);
        self.svg_set_strings(cx, this, s);
        Ok(DomStr::Owned(removed))
    }
    fn append_item(&self, cx: &mut Cx, this: Handle, new_item: &str) -> rt::Result<DomStr<'_>> {
        let mut s = self.svg_strings(this);
        s.push(new_item.to_owned());
        self.svg_set_strings(cx, this, s);
        Ok(DomStr::Owned(new_item.to_owned()))
    }
}
