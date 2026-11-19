// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IDL → ECMAScript value conversions (Web IDL §3.2): what an engine result becomes in JS.

use super::*;

/// Converts a Rust value of an IDL type to its JS value. `None` means an exception is pending (only
/// strings beyond V8's maximum length and failed wrapper creation can fail).
pub trait ToJs {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>>;
}

impl ToJs for () {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(v8::undefined(scope).into())
    }
}

impl ToJs for bool {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(v8::Boolean::new(scope, *self).into())
    }
}

macro_rules! int_to_js {
    ($($t:ty => $via:ident),*) => {$(
        impl ToJs for $t {
            fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
                Some(v8::$via::new(scope, (*self).into()).into())
            }
        }
    )*};
}

int_to_js!(i8 => Integer, i16 => Integer, i32 => Integer, u8 => Integer, u16 => Integer);

impl ToJs for u32 {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(v8::Integer::new_from_unsigned(scope, *self).into())
    }
}

impl ToJs for i64 {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(v8::Number::new(scope, *self as f64).into())
    }
}

impl ToJs for u64 {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(v8::Number::new(scope, *self as f64).into())
    }
}

impl ToJs for f32 {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(v8::Number::new(scope, *self as f64).into())
    }
}

impl ToJs for f64 {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(v8::Number::new(scope, *self).into())
    }
}

impl ToJs for str {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(str(scope, self)?.into())
    }
}

impl ToJs for String {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        self.as_str().to_js(scope)
    }
}

impl ToJs for Cow<'_, str> {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        self.as_ref().to_js(scope)
    }
}

impl ToJs for DomStr<'_> {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        (**self).to_js(scope)
    }
}

impl ToJs for Handle {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(wrap(scope, *self)?.into())
    }
}

/// Sets a callback's return value to `handle`'s wrapper, or null: a cached wrapper is stored into the
/// return value with no local handle (Blink's `DOMDataStore::SetReturnValueFromInlineStorage`, which
/// calls `ReturnValue::SetNonEmpty` with the stored reference); otherwise one is made by [`wrap`].
#[inline]
pub fn return_handle(
    scope: &v8::PinScope<'_, '_>,
    rv: &mut v8::ReturnValue<'_, v8::Value>,
    handle: Option<Handle>,
) {
    let Some(handle) = handle else {
        rv.set_null();
        return;
    };
    let st = state(scope);
    // (a replaced Window's value is its WindowProxy, not the wrapper cached for it)
    if handle.get() > st.max_replaced.get() {
        if let Some(w) = st.wrappers.borrow().get(&handle.get()) {
            if w.set_return(rv) {
                return;
            }
        }
    }
    if let Some(obj) = wrap(scope, handle) {
        rv.set(obj.into());
    }
}

impl ToJs for JsValue {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(v8::Local::new(scope, &self.0))
    }
}

impl ToJs for JsObject {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        Some(v8::Local::new(scope, &self.0).into())
    }
}

impl ToJs for JsPromise {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        let r = v8::Local::new(scope, &self.0);
        Some(r.get_promise(scope).into())
    }
}

impl<T: ToJs> ToJs for Option<T> {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        match self {
            Some(v) => v.to_js(scope),
            None => Some(v8::null(scope).into()),
        }
    }
}

impl<T: ToJs> ToJs for Vec<T> {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        let mut items = Vec::with_capacity(self.len());
        for v in self {
            items.push(v.to_js(scope)?);
        }
        Some(v8::Array::new_with_elements(scope, &items).into())
    }
}

impl<T: ToJs + ?Sized> ToJs for &T {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        (**self).to_js(scope)
    }
}

/// A `FrozenArray<T>` result: a frozen JS array.
pub struct Frozen<T>(pub Vec<T>);

impl<T: ToJs> ToJs for Frozen<T> {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        let a = self.0.to_js(scope)?;
        let o: v8::Local<v8::Object> = a.try_into().ok()?;
        o.set_integrity_level(scope, v8::IntegrityLevel::Frozen)?;
        Some(a)
    }
}

/// A `record<K, V>`: ordered string keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record<V>(pub Vec<(String, V)>);

impl<V: ToJs> ToJs for Record<V> {
    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {
        let o = v8::Object::new(scope);
        for (k, v) in &self.0 {
            let key = str(scope, k)?;
            let value = v.to_js(scope)?;
            o.create_data_property(scope, key.into(), value)?;
        }
        Some(o.into())
    }
}

/// Web IDL §3.7.7.1 "default toJSON steps" for an operation whose collected attribute identifiers are
/// `attributes`: a new object with each attribute's value from its getter steps with `this` (the
/// accessor found on `this`'s prototype chain, so an own property of the same name does not shadow it).
/// `None` when a getter threw.
pub fn default_to_json<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    this: v8::Local<'_, v8::Object>,
    attributes: &[&str],
) -> Option<v8::Local<'s, v8::Object>> {
    let out = v8::Object::new(scope);
    let get_key = name(scope, "get");
    for attr in attributes {
        let key = name(scope, attr);
        let mut proto = this.get_prototype(scope);
        while let Some(p) = proto.and_then(|p| v8::Local::<v8::Object>::try_from(p).ok()) {
            let desc = p.get_own_property_descriptor(scope, key.into())?;
            if let Ok(desc) = v8::Local::<v8::Object>::try_from(desc) {
                let getter = desc.get(scope, get_key.into())?;
                if let Ok(getter) = v8::Local::<v8::Function>::try_from(getter) {
                    let value = getter.call(scope, this.into(), &[])?;
                    out.create_data_property(scope, key.into(), value);
                }
                break;
            }
            proto = p.get_prototype(scope);
        }
    }
    Some(out)
}
