// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Iteration helpers: the sequence conversion (Web IDL §3.2.19, "create a sequence from an iterable").

use super::*;

/// Iterates `v` with its `@@iterator`, calling `each` with every value. TypeError when `v` is not an
/// object or has no callable `@@iterator`.
pub fn iterate<'s>(
    scope: &v8::PinScope<'s, '_>,
    v: v8::Local<'s, v8::Value>,
    mut each: impl FnMut(&v8::PinScope<'s, '_>, v8::Local<'s, v8::Value>) -> Option<()>,
) -> Option<()> {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(v) else {
        return type_error(scope, "The value is not iterable");
    };
    let sym = v8::Symbol::get_iterator(scope);
    let method = obj.get(scope, sym.into())?;
    let Ok(method) = v8::Local::<v8::Function>::try_from(method) else {
        return type_error(scope, "The value is not iterable");
    };
    let iterator = method.call(scope, obj.into(), &[])?;
    let Ok(iterator) = v8::Local::<v8::Object>::try_from(iterator) else {
        return type_error(
            scope,
            "Result of the Symbol.iterator method is not an object",
        );
    };
    let next_key = name(scope, "next");
    let next = iterator.get(scope, next_key.into())?;
    let Ok(next) = v8::Local::<v8::Function>::try_from(next) else {
        return type_error(scope, "The iterator's next is not a function");
    };
    let done_key = name(scope, "done");
    let value_key = name(scope, "value");
    loop {
        let result = next.call(scope, iterator.into(), &[])?;
        let Ok(result) = v8::Local::<v8::Object>::try_from(result) else {
            return type_error(scope, "Iterator result is not an object");
        };
        if result.get(scope, done_key.into())?.boolean_value(scope) {
            return Some(());
        }
        let value = result.get(scope, value_key.into())?;
        each(scope, value)?;
    }
}

const ITER_TARGET: &str = "snowlark:iterator.target";
const ITER_KIND: &str = "snowlark:iterator.kind";
const ITER_INDEX: &str = "snowlark:iterator.index";
const ITER_IFACE: &str = "snowlark:iterator.interface";

fn private<'s>(scope: &v8::PinScope<'s, '_>, key: &str) -> v8::Local<'s, v8::Private> {
    let k = name(scope, key);
    v8::Private::for_api(scope, Some(k))
}

/// Builds the iterator prototype object template of a pair-iterable interface (Web IDL §3.7.10.1):
/// class string `"<Interface> Iterator"`, a `next` method, and `@@toStringTag`.
pub fn pair_iterator_template(
    scope: &v8::PinScope<'_, '_>,
    iface: u16,
    class_name: &str,
    next: v8::Local<v8::FunctionTemplate>,
) {
    let t = v8::FunctionTemplate::builder(illegal_constructor).build(scope);
    t.set_class_name(name(scope, class_name));
    let proto = t.prototype_template(scope);
    operation(scope, &proto, "next", next, false);
    to_string_tag(scope, proto, class_name);
    let st = state(scope);
    st.iterator_templates
        .borrow_mut()
        .insert(iface, v8::Global::new(scope, t));
}

/// Per context: gives the iterator prototype object %IteratorPrototype% as its [[Prototype]], and makes
/// the interface prototype's `@@iterator` the same function as its `entries` (Web IDL §3.7.9.1).
pub fn pair_iterator_install(
    scope: &mut v8::PinScope<'_, '_>,
    iface: u16,
    interface_object: v8::Local<v8::Function>,
) -> Option<()> {
    pair_iterator_install_as(scope, iface, interface_object, "entries")
}

/// [`pair_iterator_install`] with `@@iterator` the same function object as the prototype's `default`
/// member (`entries` for pair iterators, `values` for setlike declarations, Web IDL §3.7.12).
pub fn pair_iterator_install_as(
    scope: &mut v8::PinScope<'_, '_>,
    iface: u16,
    interface_object: v8::Local<v8::Function>,
    default: &str,
) -> Option<()> {
    let t = {
        let st = state(scope);
        let g = st.iterator_templates.borrow().get(&iface).cloned()?;
        v8::Local::new(scope, g)
    };
    let f = t.get_function(scope)?;
    let proto_key = name(scope, "prototype");
    let iter_proto: v8::Local<v8::Object> = f.get(scope, proto_key.into())?.try_into().ok()?;
    // %IteratorPrototype% is the [[Prototype]] of %ArrayIteratorPrototype%.
    let array = v8::Array::new(scope, 0);
    let sym = v8::Symbol::get_iterator(scope);
    let values: v8::Local<v8::Function> = array.get(scope, sym.into())?.try_into().ok()?;
    let array_iter: v8::Local<v8::Object> =
        values.call(scope, array.into(), &[])?.try_into().ok()?;
    let array_iter_proto: v8::Local<v8::Object> =
        array_iter.get_prototype(scope)?.try_into().ok()?;
    let iterator_prototype = array_iter_proto.get_prototype(scope)?;
    iter_proto.set_prototype(scope, iterator_prototype)?;
    let proto: v8::Local<v8::Object> = interface_object
        .get(scope, proto_key.into())?
        .try_into()
        .ok()?;
    let entries = proto.get(scope, name(scope, default).into())?;
    proto.define_own_property(scope, sym.into(), entries, v8::PropertyAttribute::DONT_ENUM)?;
    Some(())
}

/// Creates a default iterator object (Web IDL §3.7.10.2) for `target` with `kind` 0 (key+value), 1 (key)
/// or 2 (value), and index 0.
pub fn pair_iterator_new<'s>(
    scope: &v8::PinScope<'s, '_>,
    target: v8::Local<'s, v8::Object>,
    iface: u16,
    kind: u8,
) -> Option<v8::Local<'s, v8::Object>> {
    let t = {
        let st = state(scope);
        let g = st.iterator_templates.borrow().get(&iface).cloned()?;
        v8::Local::new(scope, g)
    };
    let it = t.instance_template(scope).new_instance(scope)?;
    it.set_private(scope, private(scope, ITER_TARGET), target.into())?;
    let k = v8::Integer::new(scope, kind.into());
    it.set_private(scope, private(scope, ITER_KIND), k.into())?;
    let i = v8::Integer::new(scope, 0);
    it.set_private(scope, private(scope, ITER_INDEX), i.into())?;
    let id = v8::Integer::new(scope, iface.into());
    it.set_private(scope, private(scope, ITER_IFACE), id.into())?;
    Some(it)
}

/// The target, kind and index of a default iterator object of interface `iface`; `None` when `this` is
/// not one.
pub fn pair_iterator_state<'s>(
    scope: &v8::PinScope<'s, '_>,
    this: v8::Local<'s, v8::Object>,
    iface: u16,
) -> Option<(v8::Local<'s, v8::Object>, u8, u32)> {
    let id = this.get_private(scope, private(scope, ITER_IFACE))?;
    if !id.is_int32() || id.int32_value(scope)? != i32::from(iface) {
        return None;
    }
    let target = this.get_private(scope, private(scope, ITER_TARGET))?;
    let target: v8::Local<v8::Object> = target.try_into().ok()?;
    let kind = this
        .get_private(scope, private(scope, ITER_KIND))?
        .int32_value(scope)? as u8;
    let index = this
        .get_private(scope, private(scope, ITER_INDEX))?
        .uint32_value(scope)?;
    Some((target, kind, index))
}

/// Stores the next index of a default iterator object.
pub fn pair_iterator_set_index(
    scope: &v8::PinScope<'_, '_>,
    this: v8::Local<v8::Object>,
    index: u32,
) {
    let i = v8::Integer::new_from_unsigned(scope, index);
    this.set_private(scope, private(scope, ITER_INDEX), i.into());
}

/// An iterator ran past the end. Its index stays there, so later `next()` calls are done too.
pub fn pair_iterator_finish(_scope: &v8::PinScope<'_, '_>, _this: v8::Local<v8::Object>) {}

/// The iteration value for a pair: `[key, value]`, `key` or `value` by kind.
pub fn pair_iterator_value<'s>(
    scope: &v8::PinScope<'s, '_>,
    kind: u8,
    key: v8::Local<'s, v8::Value>,
    value: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Value> {
    match kind {
        0 => v8::Array::new_with_elements(scope, &[key, value]).into(),
        1 => key,
        _ => value,
    }
}

/// `CreateIterResultObject(value, done)`: done when `value` is `None`.
pub fn iter_result<'s>(
    scope: &v8::PinScope<'s, '_>,
    value: Option<v8::Local<'s, v8::Value>>,
) -> v8::Local<'s, v8::Object> {
    let o = v8::Object::new(scope);
    let done = value.is_none();
    let v = value.unwrap_or_else(|| v8::undefined(scope).into());
    let vk = name(scope, "value");
    let dk = name(scope, "done");
    o.create_data_property(scope, vk.into(), v);
    let d = v8::Boolean::new(scope, done);
    o.create_data_property(scope, dk.into(), d.into());
    o
}
