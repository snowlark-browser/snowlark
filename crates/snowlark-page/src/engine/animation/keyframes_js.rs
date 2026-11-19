// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Animations 1 §6.6.3 "process a keyframes argument" (reading script's keyframes, in the order
//! the spec gives so getters and iterators run as they would elsewhere) and `getKeyframes()`'s result
//! objects. What comes after the reading is `snowlark_style::web_animations::keyframes`.

use snowlark_bindings::rt::{self, Cx, Error, JsObject};
use snowlark_style::web_animations::keyframes::{
    animation_property_from_idl, idl_from_animation_property, process_property_indexed,
    process_sequence,
};
use snowlark_style::web_animations::{CompositeOperation, Keyframe, PropertyIndexed, RawKeyframe};
use url::Url;

use super::super::Engine;

/// A V8 call that returned nothing threw: the exception propagates.
fn pending<T>(v: Option<T>) -> rt::Result<T> {
    v.ok_or(Error::Pending)
}

fn composite_or_auto(s: &str) -> rt::Result<Option<CompositeOperation>> {
    match s {
        "replace" => Ok(Some(CompositeOperation::Replace)),
        "add" => Ok(Some(CompositeOperation::Add)),
        "accumulate" => Ok(Some(CompositeOperation::Accumulate)),
        "auto" => Ok(None),
        _ => Err(Error::Type(
            format!(
                "The provided value '{s}' is not a valid enum value of type CompositeOperationOrAuto."
            )
            .into(),
        )),
    }
}

pub(crate) fn composite_name(c: CompositeOperation) -> &'static str {
    match c {
        CompositeOperation::Replace => "replace",
        CompositeOperation::Add => "add",
        CompositeOperation::Accumulate => "accumulate",
    }
}

/// The `@@iterator` method of `v` when it is an object that has one (`None`: not an object, or the
/// method is undefined). `Err` when getting it threw.
fn iterator_method<'s>(
    scope: &v8::PinScope<'s, '_>,
    v: v8::Local<'s, v8::Value>,
) -> rt::Result<Option<(v8::Local<'s, v8::Object>, v8::Local<'s, v8::Value>)>> {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(v) else {
        return Ok(None);
    };
    let sym = v8::Symbol::get_iterator(scope);
    let method = pending(obj.get(scope, sym.into()))?;
    if method.is_undefined() || method.is_null() {
        return Ok(None);
    }
    Ok(Some((obj, method)))
}

/// Runs an iterator from `method` over `obj` (GetIterator, IteratorStep, IteratorValue).
fn iterate_with<'s>(
    scope: &v8::PinScope<'s, '_>,
    obj: v8::Local<'s, v8::Object>,
    method: v8::Local<'s, v8::Value>,
    mut each: impl FnMut(&v8::PinScope<'s, '_>, v8::Local<'s, v8::Value>) -> rt::Result<()>,
) -> rt::Result<()> {
    let Ok(method) = v8::Local::<v8::Function>::try_from(method) else {
        return Err(Error::Type(
            "The object's @@iterator is not callable".into(),
        ));
    };
    let iterator = pending(method.call(scope, obj.into(), &[]))?;
    let Ok(iterator) = v8::Local::<v8::Object>::try_from(iterator) else {
        return Err(Error::Type(
            "Result of the Symbol.iterator method is not an object".into(),
        ));
    };
    let next_key = rt::name(scope, "next");
    let next = pending(iterator.get(scope, next_key.into()))?;
    let Ok(next) = v8::Local::<v8::Function>::try_from(next) else {
        return Err(Error::Type("The iterator's next is not a function".into()));
    };
    let done_key = rt::name(scope, "done");
    let value_key = rt::name(scope, "value");
    loop {
        let result = pending(next.call(scope, iterator.into(), &[]))?;
        let Ok(result) = v8::Local::<v8::Object>::try_from(result) else {
            return Err(Error::Type("Iterator result is not an object".into()));
        };
        if pending(result.get(scope, done_key.into()))?.boolean_value(scope) {
            return Ok(());
        }
        let value = pending(result.get(scope, value_key.into()))?;
        each(scope, value)?;
    }
}

fn to_string(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> rt::Result<String> {
    pending(rt::to_dom_string(scope, v))
}

/// `double?` (restricted: NaN and infinities are TypeErrors).
fn to_nullable_double(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
) -> rt::Result<Option<f64>> {
    if v.is_null_or_undefined() {
        return Ok(None);
    }
    pending(rt::to_double(scope, v, true)).map(Some)
}

/// `(T or sequence<T>)` read with `one` for each value.
fn one_or_list<'s, T>(
    scope: &v8::PinScope<'s, '_>,
    v: v8::Local<'s, v8::Value>,
    one: impl Fn(&v8::PinScope<'s, '_>, v8::Local<'s, v8::Value>) -> rt::Result<T>,
) -> rt::Result<Vec<T>> {
    if let Some((obj, method)) = iterator_method(scope, v)? {
        let mut out = Vec::new();
        iterate_with(scope, obj, method, |scope, item| {
            out.push(one(scope, item)?);
            Ok(())
        })?;
        return Ok(out);
    }
    Ok(vec![one(scope, v)?])
}

/// The animation properties of a keyframe-like object (§6.6.3 steps 2-4): its own enumerable string
/// keys that name animatable properties (or custom properties), sorted by code point, with their
/// animation property names.
fn animation_properties(
    scope: &v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
) -> rt::Result<Vec<(String, String)>> {
    let keys = pending(obj.get_own_property_names(
        scope,
        v8::GetPropertyNamesArgs {
            mode: v8::KeyCollectionMode::OwnOnly,
            property_filter: v8::PropertyFilter::ONLY_ENUMERABLE | v8::PropertyFilter::SKIP_SYMBOLS,
            index_filter: v8::IndexFilter::SkipIndices,
            key_conversion: v8::KeyConversionMode::ConvertToString,
        },
    ))?;
    let mut out = Vec::new();
    for i in 0..keys.length() {
        let k = pending(keys.get_index(scope, i))?;
        let name = to_string(scope, k)?;
        if let Some(property) = animation_property_from_idl(&name) {
            out.push((name, property));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

impl Engine {
    /// §6.6.3 "process a keyframes argument" for `keyframes` (relative URLs against `base`).
    pub(crate) fn process_keyframes(
        &self,
        cx: &mut Cx,
        keyframes: Option<&JsObject>,
        base: &Url,
    ) -> rt::Result<Vec<Keyframe>> {
        let Some(keyframes) = keyframes else {
            return Ok(Vec::new());
        };
        let scope = cx.scope();
        let obj = v8::Local::new(scope, keyframes.global());
        let sym = v8::Symbol::get_iterator(scope);
        let method = pending(obj.get(scope, sym.into()))?;
        let result = if !method.is_undefined() && !method.is_null() {
            let mut raws: Vec<RawKeyframe> = Vec::new();
            iterate_with(scope, obj, method, |scope, item| {
                if !(item.is_null_or_undefined() || item.is_object()) {
                    return Err(Error::Type(
                        "Keyframes must be objects, or null or undefined".into(),
                    ));
                }
                raws.push(read_keyframe(scope, item)?);
                Ok(())
            })?;
            process_sequence(raws, base)
        } else {
            let indexed = read_property_indexed(scope, obj.into())?;
            process_property_indexed(indexed, base)
        };
        result.map_err(|m| Error::Type(m.into()))
    }

    /// `getKeyframes()`: one object per keyframe with `offset`, `computedOffset`, `easing`,
    /// `composite` and the property values by IDL name.
    pub(crate) fn keyframe_objects(&self, cx: &mut Cx, keyframes: &[Keyframe]) -> Vec<JsObject> {
        let scope = cx.scope();
        keyframes
            .iter()
            .map(|k| {
                let o = v8::Object::new(scope);
                let put = |scope: &v8::PinScope<'_, '_>, key: &str, v: v8::Local<v8::Value>| {
                    let key = rt::name(scope, key);
                    o.create_data_property(scope, key.into(), v);
                };
                let offset: v8::Local<v8::Value> = match k.offset {
                    Some(x) => v8::Number::new(scope, x).into(),
                    None => v8::null(scope).into(),
                };
                put(scope, "offset", offset);
                let computed = v8::Number::new(scope, k.computed_offset).into();
                put(scope, "computedOffset", computed);
                let easing = rt::name(scope, k.easing.css()).into();
                put(scope, "easing", easing);
                let composite = rt::name(scope, k.composite.map_or("auto", composite_name)).into();
                put(scope, "composite", composite);
                for (property, value) in &k.values {
                    let v = rt::name(scope, value).into();
                    put(scope, &idl_from_animation_property(property), v);
                }
                JsObject::new(scope, o)
            })
            .collect()
    }
}

/// "Process a keyframe-like object" with allow lists false.
fn read_keyframe(
    scope: &v8::PinScope<'_, '_>,
    input: v8::Local<v8::Value>,
) -> rt::Result<RawKeyframe> {
    let mut k = RawKeyframe {
        offset: None,
        easing: "linear".into(),
        composite: None,
        values: Vec::new(),
    };
    let Ok(obj) = v8::Local::<v8::Object>::try_from(input) else {
        return Ok(k);
    };
    // The BaseKeyframe dictionary, members in order.
    let v = pending(rt::get_member(scope, obj, "composite"))?;
    if !v.is_undefined() {
        k.composite = composite_or_auto(&to_string(scope, v)?)?;
    }
    let v = pending(rt::get_member(scope, obj, "easing"))?;
    if !v.is_undefined() {
        k.easing = to_string(scope, v)?;
    }
    let v = pending(rt::get_member(scope, obj, "offset"))?;
    k.offset = to_nullable_double(scope, v)?;
    for (name, property) in animation_properties(scope, obj)? {
        let key = rt::name(scope, &name);
        let raw = pending(obj.get(scope, key.into()))?;
        k.values.push((property, to_string(scope, raw)?));
    }
    Ok(k)
}

/// "Process a keyframe-like object" with allow lists true.
fn read_property_indexed(
    scope: &v8::PinScope<'_, '_>,
    input: v8::Local<v8::Value>,
) -> rt::Result<PropertyIndexed> {
    let mut out = PropertyIndexed::default();
    let Ok(obj) = v8::Local::<v8::Object>::try_from(input) else {
        return Ok(out);
    };
    let v = pending(rt::get_member(scope, obj, "composite"))?;
    if !v.is_undefined() {
        out.composites = one_or_list(scope, v, |scope, v| {
            composite_or_auto(&to_string(scope, v)?)
        })?;
    }
    let v = pending(rt::get_member(scope, obj, "easing"))?;
    if !v.is_undefined() {
        out.easings = one_or_list(scope, v, to_string)?;
    }
    let v = pending(rt::get_member(scope, obj, "offset"))?;
    if !v.is_undefined() {
        out.offsets = if v.is_null() {
            vec![None]
        } else {
            one_or_list(scope, v, to_nullable_double)?
        };
    }
    for (name, property) in animation_properties(scope, obj)? {
        let key = rt::name(scope, &name);
        let raw = pending(obj.get(scope, key.into()))?;
        let values = one_or_list(scope, raw, to_string)?;
        out.values.push((property, values));
    }
    Ok(out)
}
