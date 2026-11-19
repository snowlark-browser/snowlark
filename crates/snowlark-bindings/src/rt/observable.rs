// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web IDL §3.10 observable array exotic objects, for `ObservableArray<T>` attributes
//! (`DocumentOrShadowRoot.adoptedStyleSheets`): a proxy whose target is an empty array and whose
//! handler's traps run the spec's algorithms (§3.10.1-3.10.9) against the attribute's backing list.
//! The engine keeps the backing list and runs the attribute's "set an indexed value" and "delete an
//! indexed value" steps; the generated code ties them together per attribute as an
//! [`ObservableArray`].

use super::*;

/// One `ObservableArray<T>` attribute of a platform object `obj` (the wrapper the proxy belongs to).
pub trait ObservableArray: 'static {
    /// The size of the backing list; `None` when `obj` is not a platform object of the interface.
    fn len(scope: &mut v8::PinScope<'_, '_>, obj: v8::Local<v8::Object>) -> Option<u32>;
    /// The backing list's item at `index` (`index < len`), converted to a JavaScript value.
    fn get<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        obj: v8::Local<v8::Object>,
        index: u32,
    ) -> Option<v8::Local<'s, v8::Value>>;
    /// "Set the indexed value" steps 4-8 for `index <= len`: converts `value` to the element type,
    /// runs "delete an indexed value" for the item it replaces, then "set an indexed value", and
    /// stores it (appended at `len`). `None` with an exception pending.
    fn set(
        scope: &mut v8::PinScope<'_, '_>,
        obj: v8::Local<v8::Object>,
        index: u32,
        value: v8::Local<v8::Value>,
    ) -> Option<()>;
    /// Runs "delete an indexed value" for the last item and removes it from the backing list.
    fn delete_last(scope: &mut v8::PinScope<'_, '_>, obj: v8::Local<v8::Object>) -> Option<()>;
}

/// Creates the observable array exotic object of an attribute of `obj` (Web IDL §3.10 "create an
/// observable array exotic object"): a proxy over a new empty array with a handler of the traps
/// below, each knowing `obj` through its function data.
pub fn observable_array_new<'s, A: ObservableArray>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
) -> Option<v8::Local<'s, v8::Object>> {
    let target = v8::Array::new(scope, 0);
    let handler = v8::Object::new(scope);
    let data: v8::Local<v8::Value> = obj.into();
    macro_rules! trap {
        ($name:literal, $f:expr) => {{
            let f = v8::Function::builder($f)
                .data(data)
                .length(0)
                .build(scope)?;
            let k = name(scope, $name);
            handler.set(scope, k.into(), f.into())?;
        }};
    }
    trap!("defineProperty", trap_define_property::<A>);
    trap!("deleteProperty", trap_delete_property::<A>);
    trap!("get", trap_get::<A>);
    trap!(
        "getOwnPropertyDescriptor",
        trap_get_own_property_descriptor::<A>
    );
    trap!("has", trap_has::<A>);
    trap!("ownKeys", trap_own_keys::<A>);
    trap!("preventExtensions", trap_prevent_extensions);
    trap!("set", trap_set::<A>);
    let proxy = v8::Proxy::new(scope, target.into(), handler)?;
    Some(proxy.into())
}

/// The property key as an array index (ECMA-262 §6.1.7: a canonical numeric string below 2³² − 1).
fn array_index(scope: &mut v8::PinScope<'_, '_>, key: v8::Local<v8::Value>) -> Option<u32> {
    if !key.is_string() {
        return None;
    }
    let s = key.to_rust_string_lossy(scope);
    if s.is_empty() || s.len() > 10 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    let v: u64 = s.parse().ok()?;
    (v < u32::MAX as u64).then_some(v as u32)
}

fn is_length(scope: &mut v8::PinScope<'_, '_>, key: v8::Local<v8::Value>) -> bool {
    key.is_string() && key.to_rust_string_lossy(scope) == "length"
}

/// The platform object a trap belongs to, and the trap's target and key arguments.
fn trap_args<'s>(
    args: &v8::FunctionCallbackArguments<'s>,
) -> Option<(
    v8::Local<'s, v8::Object>,
    v8::Local<'s, v8::Object>,
    v8::Local<'s, v8::Value>,
)> {
    let obj = v8::Local::<v8::Object>::try_from(args.data()).ok()?;
    let target = v8::Local::<v8::Object>::try_from(args.get(0)).ok()?;
    Some((obj, target, args.get(1)))
}

/// §3.10.10 "set the length": `None` with an exception pending.
fn set_length<A: ObservableArray>(
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
    new_len: v8::Local<v8::Value>,
) -> Option<bool> {
    let uint32_len = new_len.uint32_value(scope)?;
    let number_len = new_len.number_value(scope)?;
    if uint32_len as f64 != number_len {
        let m = str(scope, "Invalid array length")?;
        let e = v8::Exception::range_error(scope, m);
        scope.throw_exception(e);
        return None;
    }
    let old_len = A::len(scope, obj)?;
    if uint32_len > old_len {
        return Some(false);
    }
    for _ in uint32_len..old_len {
        A::delete_last(scope, obj)?;
    }
    Some(true)
}

/// §3.10.11 "set the indexed value".
fn set_indexed<A: ObservableArray>(
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
    index: u32,
    value: v8::Local<v8::Value>,
) -> Option<bool> {
    let old_len = A::len(scope, obj)?;
    if index > old_len {
        return Some(false);
    }
    A::set(scope, obj, index, value)?;
    Some(true)
}

/// A descriptor object's field, when present.
fn field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    desc: v8::Local<v8::Object>,
    key: &str,
) -> Option<Option<v8::Local<'s, v8::Value>>> {
    let k = name(scope, key);
    if !desc.has(scope, k.into())? {
        return Some(None);
    }
    Some(Some(desc.get(scope, k.into())?))
}

/// §3.10.1 `defineProperty`.
fn trap_define_property<A: ObservableArray>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some((obj, target, key)) = trap_args(&args) else {
        return;
    };
    let Ok(desc) = v8::Local::<v8::Object>::try_from(args.get(2)) else {
        return throw_type_error(scope, "Property description must be an object");
    };
    let Some(get) = field(scope, desc, "get") else {
        return;
    };
    let Some(set) = field(scope, desc, "set") else {
        return;
    };
    let Some(value) = field(scope, desc, "value") else {
        return;
    };
    let Some(writable) = field(scope, desc, "writable") else {
        return;
    };
    let Some(enumerable) = field(scope, desc, "enumerable") else {
        return;
    };
    let Some(configurable) = field(scope, desc, "configurable") else {
        return;
    };
    let flag = |v: Option<v8::Local<v8::Value>>, scope: &mut v8::PinScope| {
        v.map(|v| v.boolean_value(scope))
    };
    let accessor = get.is_some() || set.is_some();
    let writable_b = flag(writable, scope);
    let enumerable_b = flag(enumerable, scope);
    let configurable_b = flag(configurable, scope);
    let ok = if is_length(scope, key) {
        if accessor
            || configurable_b == Some(true)
            || enumerable_b == Some(true)
            || writable_b == Some(false)
        {
            Some(false)
        } else if let Some(v) = value {
            set_length::<A>(scope, obj, v)
        } else {
            Some(true)
        }
    } else if let Some(index) = array_index(scope, key) {
        if accessor
            || configurable_b == Some(false)
            || enumerable_b == Some(false)
            || writable_b == Some(false)
        {
            Some(false)
        } else if let Some(v) = value {
            set_indexed::<A>(scope, obj, index, v)
        } else {
            Some(true)
        }
    } else {
        // target.[[DefineOwnProperty]](P, descriptor)
        let Ok(name_key) = v8::Local::<v8::Name>::try_from(key) else {
            return;
        };
        let undefined: v8::Local<v8::Value> = v8::undefined(scope).into();
        let mut d = match (value, writable_b) {
            (Some(v), Some(w)) => v8::PropertyDescriptor::new_from_value_writable(v, w),
            (Some(v), None) => v8::PropertyDescriptor::new_from_value(v),
            _ if accessor => v8::PropertyDescriptor::new_from_get_set(
                get.unwrap_or(undefined),
                set.unwrap_or(undefined),
            ),
            _ => v8::PropertyDescriptor::new(),
        };
        if let Some(e) = enumerable_b {
            d.set_enumerable(e);
        }
        if let Some(c) = configurable_b {
            d.set_configurable(c);
        }
        target.define_property(scope, name_key, &d)
    };
    if let Some(ok) = ok {
        rv.set_bool(ok);
    }
}

/// §3.10.2 `deleteProperty`.
fn trap_delete_property<A: ObservableArray>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some((obj, target, key)) = trap_args(&args) else {
        return;
    };
    if is_length(scope, key) {
        return rv.set_bool(false);
    }
    if let Some(index) = array_index(scope, key) {
        let Some(old_len) = A::len(scope, obj) else {
            return;
        };
        if old_len == 0 || index != old_len - 1 {
            return rv.set_bool(false);
        }
        if A::delete_last(scope, obj).is_some() {
            rv.set_bool(true);
        }
        return;
    }
    if let Some(ok) = target.delete(scope, key) {
        rv.set_bool(ok);
    }
}

/// §3.10.3 `get`.
fn trap_get<A: ObservableArray>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some((obj, target, key)) = trap_args(&args) else {
        return;
    };
    let Some(len) = A::len(scope, obj) else {
        return;
    };
    if is_length(scope, key) {
        return rv.set_uint32(len);
    }
    if let Some(index) = array_index(scope, key) {
        if index >= len {
            return rv.set_undefined();
        }
        if let Some(v) = A::get(scope, obj, index) {
            rv.set(v);
        }
        return;
    }
    if let Some(v) = target.get(scope, key) {
        rv.set(v);
    }
}

/// A data property descriptor object (`FromPropertyDescriptor`).
fn data_descriptor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
    writable: bool,
    enumerable: bool,
    configurable: bool,
) -> Option<v8::Local<'s, v8::Object>> {
    let d = v8::Object::new(scope);
    for (k, v) in [
        ("value", value),
        ("writable", v8::Boolean::new(scope, writable).into()),
        ("enumerable", v8::Boolean::new(scope, enumerable).into()),
        ("configurable", v8::Boolean::new(scope, configurable).into()),
    ] {
        let k = name(scope, k);
        d.create_data_property(scope, k.into(), v)?;
    }
    Some(d)
}

/// §3.10.4 `getOwnPropertyDescriptor`.
fn trap_get_own_property_descriptor<A: ObservableArray>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some((obj, target, key)) = trap_args(&args) else {
        return;
    };
    let Some(len) = A::len(scope, obj) else {
        return;
    };
    if is_length(scope, key) {
        let v = v8::Integer::new_from_unsigned(scope, len).into();
        if let Some(d) = data_descriptor(scope, v, true, false, false) {
            rv.set(d.into());
        }
        return;
    }
    if let Some(index) = array_index(scope, key) {
        if index >= len {
            return rv.set_undefined();
        }
        let Some(v) = A::get(scope, obj, index) else {
            return;
        };
        if let Some(d) = data_descriptor(scope, v, true, true, true) {
            rv.set(d.into());
        }
        return;
    }
    let Ok(name_key) = v8::Local::<v8::Name>::try_from(key) else {
        return;
    };
    if let Some(d) = target.get_own_property_descriptor(scope, name_key) {
        rv.set(d);
    }
}

/// §3.10.5 `has`.
fn trap_has<A: ObservableArray>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some((obj, target, key)) = trap_args(&args) else {
        return;
    };
    if is_length(scope, key) {
        return rv.set_bool(true);
    }
    if let Some(index) = array_index(scope, key) {
        if let Some(len) = A::len(scope, obj) {
            rv.set_bool(index < len);
        }
        return;
    }
    if let Some(has) = target.has(scope, key) {
        rv.set_bool(has);
    }
}

/// §3.10.6 `ownKeys`: the backing list's indices, then the target's own keys (`length`).
fn trap_own_keys<A: ObservableArray>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some((obj, target, _)) = trap_args(&args) else {
        return;
    };
    let Some(len) = A::len(scope, obj) else {
        return;
    };
    let mut keys: Vec<v8::Local<v8::Value>> = Vec::with_capacity(len as usize + 1);
    for i in 0..len {
        let Some(k) = str(scope, &i.to_string()) else {
            return;
        };
        keys.push(k.into());
    }
    let Some(own) = target.get_own_property_names(
        scope,
        v8::GetPropertyNamesArgs {
            mode: v8::KeyCollectionMode::OwnOnly,
            property_filter: v8::PropertyFilter::ALL_PROPERTIES,
            index_filter: v8::IndexFilter::IncludeIndices,
            key_conversion: v8::KeyConversionMode::ConvertToString,
        },
    ) else {
        return;
    };
    for i in 0..own.length() {
        let Some(k) = own.get_index(scope, i) else {
            return;
        };
        keys.push(k);
    }
    rv.set(v8::Array::new_with_elements(scope, &keys).into());
}

/// §3.10.7 `preventExtensions`: an observable array cannot be made non-extensible.
fn trap_prevent_extensions(
    _scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set_bool(false);
}

/// §3.10.8 `set`.
fn trap_set<A: ObservableArray>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some((obj, target, key)) = trap_args(&args) else {
        return;
    };
    let value = args.get(2);
    let ok = if is_length(scope, key) {
        set_length::<A>(scope, obj, value)
    } else if let Some(index) = array_index(scope, key) {
        set_indexed::<A>(scope, obj, index, value)
    } else {
        target.set(scope, key, value)
    };
    if let Some(ok) = ok {
        rv.set_bool(ok);
    }
}
