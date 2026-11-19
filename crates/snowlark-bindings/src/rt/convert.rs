// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ECMAScript → IDL value conversions (Web IDL §3.2). Each returns `None` with a JS exception pending
//! when the conversion throws.

use super::*;

/// How an integer conversion treats out-of-range values (`[EnforceRange]`, `[Clamp]`, or neither).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntMode {
    Wrap,
    Clamp,
    EnforceRange,
}

/// Web IDL ConvertToInt (§3.2.4.9) for a bit length and signedness, as an f64 the caller casts.
pub fn convert_to_int(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    bits: u32,
    signed: bool,
    mode: IntMode,
) -> Option<f64> {
    let (lower, upper) = if bits == 64 {
        let max = 9007199254740991.0; // 2^53 - 1
        if signed {
            (-max, max)
        } else {
            (0.0, max)
        }
    } else if signed {
        (
            -(2f64.powi(bits as i32 - 1)),
            2f64.powi(bits as i32 - 1) - 1.0,
        )
    } else {
        (0.0, 2f64.powi(bits as i32) - 1.0)
    };
    let x = if v.is_number() {
        v.number_value(scope)?
    } else {
        v.to_number(scope)?.value()
    };
    let x = if x == 0.0 { 0.0 } else { x }; // -0 → +0
    match mode {
        IntMode::EnforceRange => {
            if !x.is_finite() {
                return type_error(scope, "Value is not a finite number");
            }
            let x = x.trunc();
            if x < lower || x > upper {
                return type_error(scope, "Value is outside the accepted range");
            }
            Some(x + 0.0)
        }
        IntMode::Clamp => {
            if x.is_nan() {
                return Some(0.0);
            }
            // Clamp, then round to the nearest integer with ties to even.
            Some(x.clamp(lower, upper).round_ties_even() + 0.0)
        }
        IntMode::Wrap => {
            if !x.is_finite() {
                return Some(0.0);
            }
            let x = x.trunc();
            if bits == 64 {
                // ToInt64/ToUint64 semantics through a modulo of 2^64.
                let m = x.rem_euclid(18446744073709551616.0);
                if signed && m >= 9223372036854775808.0 {
                    return Some(m - 18446744073709551616.0);
                }
                return Some(m);
            }
            let size = 2f64.powi(bits as i32);
            let m = x.rem_euclid(size);
            if signed && m >= size / 2.0 {
                Some(m - size)
            } else {
                Some(m)
            }
        }
    }
}

macro_rules! int_conv {
    ($name:ident, $t:ty, $bits:expr, $signed:expr) => {
        #[inline]
        pub fn $name(
            scope: &v8::PinScope<'_, '_>,
            v: v8::Local<v8::Value>,
            mode: IntMode,
        ) -> Option<$t> {
            if mode == IntMode::Wrap && $bits == 32 && v.is_int32() {
                return Some(v.int32_value(scope)? as $t);
            }
            convert_to_int(scope, v, $bits, $signed, mode).map(|x| x as $t)
        }
    };
}

int_conv!(to_byte, i8, 8, true);
int_conv!(to_octet, u8, 8, false);
int_conv!(to_short, i16, 16, true);
int_conv!(to_unsigned_short, u16, 16, false);
int_conv!(to_long, i32, 32, true);
int_conv!(to_unsigned_long, u32, 32, false);

/// `long long` (ConvertToInt with 64 bits; values beyond 2^53 lose precision as in the spec).
pub fn to_long_long(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    mode: IntMode,
) -> Option<i64> {
    let x = convert_to_int(scope, v, 64, true, mode)?;
    Some(x as i64)
}

/// `unsigned long long`.
pub fn to_unsigned_long_long(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    mode: IntMode,
) -> Option<u64> {
    let x = convert_to_int(scope, v, 64, false, mode)?;
    Some(x as u64)
}

/// `double` / `unrestricted double`.
#[inline]
pub fn to_double(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    restricted: bool,
) -> Option<f64> {
    let x = if v.is_number() {
        v.number_value(scope)?
    } else {
        v.to_number(scope)?.value()
    };
    if restricted && !x.is_finite() {
        return type_error(scope, "The provided double value is non-finite");
    }
    Some(x)
}

/// `float` / `unrestricted float`.
pub fn to_float(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    restricted: bool,
) -> Option<f32> {
    let x = to_double(scope, v, restricted)?;
    let y = x as f32;
    if restricted && y.is_infinite() {
        return type_error(scope, "The provided float value is out of range");
    }
    Some(y)
}

/// `boolean` (ToBoolean).
#[inline]
pub fn to_boolean(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> bool {
    v.boolean_value(scope)
}

/// `DOMString` (ToString). Lone surrogates become U+FFFD: the DOM stores UTF-8.
#[inline]
pub fn to_dom_string(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> Option<String> {
    if v.is_string() {
        let s: v8::Local<v8::String> = v.try_into().ok()?;
        return Some(s.to_rust_string_lossy(scope));
    }
    Some(v.to_string(scope)?.to_rust_string_lossy(scope))
}

/// [`to_dom_string`] for a value the binding passes on as `&str`: a string that fits `buf` is
/// converted into it, without an allocation.
#[inline]
pub fn to_dom_str<'b, const N: usize>(
    scope: &mut v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    buf: &'b mut [std::mem::MaybeUninit<u8>; N],
) -> Option<std::borrow::Cow<'b, str>> {
    let s: v8::Local<v8::String> = if v.is_string() {
        v.try_into().ok()?
    } else {
        v.to_string(scope)?
    };
    Some(s.to_rust_cow_lossy(scope, buf))
}

/// `DOMString` with `[LegacyNullToEmptyString]`.
pub fn to_dom_string_null_empty(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
) -> Option<String> {
    if v.is_null() {
        return Some(String::new());
    }
    to_dom_string(scope, v)
}

/// `USVString`: ToString, then lone surrogates replaced by U+FFFD.
pub fn to_usv_string(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> Option<String> {
    to_dom_string(scope, v)
}

/// `ByteString`: ToString, TypeError if any code unit is above 255; each byte becomes one `char`.
pub fn to_byte_string(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> Option<String> {
    let s = v.to_string(scope)?;
    if !s.contains_only_onebyte() {
        return type_error(scope, "Value is not a valid ByteString");
    }
    Some(s.to_rust_string_lossy(scope))
}

/// `any`.
pub fn to_any(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> JsValue {
    JsValue(v8::Global::new(scope, v))
}

/// `object`.
pub fn to_object(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    what: &str,
) -> Option<JsObject> {
    match v8::Local::<v8::Object>::try_from(v) {
        Ok(o) => Some(JsObject(v8::Global::new(scope, o))),
        Err(_) => type_error(scope, &format!("{what} is not an object")),
    }
}

/// Whether `v` is a SharedArrayBuffer or a view onto one.
pub fn is_shared_buffer(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> bool {
    if v.is_shared_array_buffer() {
        return true;
    }
    match v8::Local::<v8::ArrayBufferView>::try_from(v) {
        Ok(view) => view
            .buffer(scope)
            .is_some_and(|b| b.is_shared_array_buffer()),
        Err(_) => false,
    }
}

/// A buffer source type (`ArrayBuffer`, a typed array, `DataView`): the object; a TypeError when it is,
/// or views, a SharedArrayBuffer and the type is not `[AllowShared]` (Web IDL §3.2.25–3.2.27).
pub fn to_buffer(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    allow_shared: bool,
    what: &str,
) -> Option<JsObject> {
    if !allow_shared && is_shared_buffer(scope, v) {
        return type_error(scope, &format!("{what} is a shared buffer"));
    }
    to_object(scope, v, what)
}

/// An interface type: the brand-checked handle.
#[inline]
pub fn to_interface(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    lo: u16,
    hi: u16,
    what: &str,
    iface: &str,
) -> Option<Handle> {
    match unwrap_value(v, lo, hi) {
        Some(h) => Some(h),
        None => type_error(scope, &format!("{what} is not of type '{iface}'")),
    }
}

/// An enumeration: the index of the matching value, TypeError otherwise.
pub fn to_enum(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    values: &[&str],
    enum_name: &str,
) -> Option<usize> {
    let s = to_dom_string(scope, v)?;
    match values.iter().position(|x| *x == s) {
        Some(i) => Some(i),
        None => type_error(
            scope,
            &format!("The provided value '{s}' is not a valid enum value of type {enum_name}"),
        ),
    }
}

/// A callback function: TypeError unless callable; `[LegacyTreatNonObjectAsNull]` callbacks accept any
/// object and turn non-objects into null (`Ok(None)`).
pub fn to_callback_function(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    treat_non_object_as_null: bool,
    what: &str,
) -> Option<Option<JsObject>> {
    if treat_non_object_as_null {
        return match v8::Local::<v8::Object>::try_from(v) {
            Ok(o) => Some(Some(JsObject(v8::Global::new(scope, o)))),
            Err(_) => Some(None),
        };
    }
    if !v.is_function() {
        return type_error(scope, &format!("{what} is not a function"));
    }
    let o: v8::Local<v8::Object> = v.try_into().ok()?;
    Some(Some(JsObject(v8::Global::new(scope, o))))
}

/// A callback interface: TypeError unless an object.
pub fn to_callback_interface(
    scope: &v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    what: &str,
) -> Option<JsObject> {
    match v8::Local::<v8::Object>::try_from(v) {
        Ok(o) => Some(JsObject(v8::Global::new(scope, o))),
        Err(_) => type_error(scope, &format!("{what} is not an object")),
    }
}

/// A `Promise<T>` argument: `Promise.resolve(v)` (Web IDL §3.2.23).
pub fn to_promise(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> Option<JsObject> {
    let r = v8::PromiseResolver::new(scope)?;
    r.resolve(scope, v)?;
    let p: v8::Local<v8::Object> = r.get_promise(scope).into();
    Some(JsObject(v8::Global::new(scope, p)))
}

/// Gets a dictionary member (`Get(O, name)`), for generated dictionary conversions.
#[inline]
pub fn get_member<'s>(
    scope: &v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    key: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let k = name(scope, key);
    obj.get(scope, k.into())
}

/// [`get_member`] with the name's string kept in the isolate ([`Name`]): what generated dictionary
/// conversions call for each member.
#[inline]
pub fn get_named_member<'s>(
    scope: &v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    key: &Name,
) -> Option<v8::Local<'s, v8::Value>> {
    let k = cached_name(scope, key);
    obj.get(scope, k.into())
}

/// The object a dictionary is read from: `None` for undefined or null (all members take their defaults),
/// TypeError for other non-objects.
pub fn dictionary_object<'s>(
    scope: &v8::PinScope<'s, '_>,
    v: v8::Local<'s, v8::Value>,
    dict: &str,
) -> Option<Option<v8::Local<'s, v8::Object>>> {
    if v.is_null_or_undefined() {
        return Some(None);
    }
    match v8::Local::<v8::Object>::try_from(v) {
        Ok(o) => Some(Some(o)),
        Err(_) => type_error(scope, &format!("The value is not of type '{dict}'")),
    }
}

/// The record conversion (Web IDL §3.2.20): `[[OwnPropertyKeys]]`, then for each key in order its
/// `[[GetOwnProperty]]` and, when it is present and enumerable, the key's conversion (`key`; a
/// symbol throws), its `[[Get]]` and the value's conversion (`value`). A key that converts to one
/// already there (lone surrogates of a USVString key) replaces that entry's value in place.
pub fn record_entries<'s, T>(
    scope: &v8::PinScope<'s, '_>,
    v: v8::Local<'s, v8::Value>,
    mut key: impl FnMut(&v8::PinScope<'s, '_>, v8::Local<'s, v8::Value>) -> Option<String>,
    mut value: impl FnMut(&v8::PinScope<'s, '_>, v8::Local<'s, v8::Value>) -> Option<T>,
) -> Option<Vec<(String, T)>> {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(v) else {
        return type_error(scope, "The value is not an object");
    };
    // All own keys, without a filter (a filter would read every descriptor up front).
    let keys = obj.get_own_property_names(
        scope,
        v8::GetPropertyNamesArgs {
            mode: v8::KeyCollectionMode::OwnOnly,
            property_filter: v8::PropertyFilter::ALL_PROPERTIES,
            index_filter: v8::IndexFilter::IncludeIndices,
            key_conversion: v8::KeyConversionMode::ConvertToString,
        },
    )?;
    let enumerable = name(scope, "enumerable");
    let mut out: Vec<(String, T)> = Vec::with_capacity(keys.length() as usize);
    for i in 0..keys.length() {
        let k = keys.get_index(scope, i)?;
        let Ok(k_name) = v8::Local::<v8::Name>::try_from(k) else {
            continue;
        };
        let desc = obj.get_own_property_descriptor(scope, k_name)?;
        // The descriptor is a fresh ordinary object: reading it runs no page code.
        let Ok(desc) = v8::Local::<v8::Object>::try_from(desc) else {
            continue;
        };
        if !desc.get(scope, enumerable.into())?.boolean_value(scope) {
            continue;
        }
        let typed_key = key(scope, k)?;
        let item = obj.get(scope, k)?;
        let typed_value = value(scope, item)?;
        match out.iter_mut().find(|e| e.0 == typed_key) {
            Some(e) => e.1 = typed_value,
            None => out.push((typed_key, typed_value)),
        }
    }
    Some(out)
}

/// Whether `v` is an object with a callable `@@iterator` (the sequence test of overload resolution and
/// union conversion).
pub fn is_iterable(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> Option<bool> {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(v) else {
        return Some(false);
    };
    let sym = v8::Symbol::get_iterator(scope);
    let m = obj.get(scope, sym.into())?;
    Some(m.is_function())
}
