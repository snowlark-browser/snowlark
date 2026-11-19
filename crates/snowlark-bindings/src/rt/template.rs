// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers the generated template builders use to give each property the attributes Web IDL §3.7 asks
//! for: operations writable/enumerable/configurable and not constructors, accessors enumerable and
//! configurable with named getter/setter functions, constants read-only and non-configurable.

use super::*;

type Builder<'s> = v8::FunctionBuilder<'s, v8::FunctionTemplate>;

/// Builds an operation's function template: `name`, `length`, receiver signature, not a constructor and
/// without a `prototype` property.
pub fn method<'s>(
    scope: &v8::PinScope<'s, '_, ()>,
    builder: Builder<'s>,
    name_: &str,
    length: i32,
    signature: Option<v8::Local<'s, v8::Signature>>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let mut b = builder
        .length(length)
        .constructor_behavior(v8::ConstructorBehavior::Throw);
    if let Some(sig) = signature {
        b = b.signature(sig);
    }
    let t = b.build(scope);
    t.set_class_name(name(scope, name_));
    t.remove_prototype();
    t
}

/// Builds an attribute getter (`get <name>`), with Fast API overloads when given and enabled.
pub fn getter<'s>(
    scope: &v8::PinScope<'s, '_>,
    builder: Builder<'s>,
    attr: &str,
    signature: Option<v8::Local<'s, v8::Signature>>,
    fast: Option<&'static [v8::fast_api::CFunction]>,
    options: &InstallOptions,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let mut b = builder
        .length(0)
        .constructor_behavior(v8::ConstructorBehavior::Throw);
    if let Some(sig) = signature {
        b = b.signature(sig);
    }
    let t = match fast {
        Some(c) if options.fast_api => b.build_fast(scope, c),
        _ => b.build(scope),
    };
    t.set_class_name(name(scope, &format!("get {attr}")));
    t.remove_prototype();
    t
}

/// Builds the getter of a `[CachedAccessor]` attribute: V8 reads the value from the private `key` of
/// the holder, which must be set before script can reach the holder ([`set_cached_attribute`]).
pub fn cached_getter<'s, F>(
    scope: &v8::PinScope<'s, '_, ()>,
    f: F,
    attr: &str,
    key: &str,
    signature: Option<v8::Local<'s, v8::Signature>>,
) -> v8::Local<'s, v8::FunctionTemplate>
where
    F: for<'a, 'b> Fn(
            &mut v8::PinScope<'a, 'b>,
            v8::FunctionCallbackArguments<'a>,
            v8::ReturnValue<'a, v8::Value>,
        ) + Copy,
{
    let private = v8::Private::for_api(scope, Some(name(scope, key)));
    let t = snowlark_js::cached_accessor::getter_template(scope, guarded(f), private, signature);
    t.set_class_name(name(scope, &format!("get {attr}")));
    t.remove_prototype();
    t
}

/// Sets the value of the `[CachedAccessor]` attribute `key` ("Interface.attribute") of `holder`.
pub fn set_cached_attribute(
    scope: &v8::PinScope<'_, '_>,
    holder: v8::Local<v8::Object>,
    key: &str,
    value: v8::Local<v8::Value>,
) {
    let private = v8::Private::for_api(scope, Some(name(scope, key)));
    holder.set_private(scope, private, value);
}

/// Runs `f` with any termination of script held back: V8 refuses every call while the isolate
/// terminates, and `f` must happen all the same. Script terminates again afterwards.
pub fn without_termination<'s, 'i, R>(
    scope: &mut v8::PinScope<'s, 'i>,
    f: impl FnOnce(&mut v8::PinScope<'s, 'i>) -> R,
) -> R {
    let terminating = scope.is_execution_terminating();
    if terminating {
        scope.cancel_terminate_execution();
    }
    let result = f(scope);
    if terminating {
        scope.terminate_execution();
    }
    result
}

/// Builds an attribute setter (`set <name>`).
pub fn setter<'s>(
    scope: &v8::PinScope<'s, '_, ()>,
    builder: Builder<'s>,
    attr: &str,
    signature: Option<v8::Local<'s, v8::Signature>>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    let mut b = builder
        .length(1)
        .constructor_behavior(v8::ConstructorBehavior::Throw);
    if let Some(sig) = signature {
        b = b.signature(sig);
    }
    let t = b.build(scope);
    t.set_class_name(name(scope, &format!("set {attr}")));
    t.remove_prototype();
    t
}

/// Installs an accessor property. `unforgeable` (`[LegacyUnforgeable]`) makes it non-configurable.
pub fn accessor(
    scope: &v8::PinScope<'_, '_, ()>,
    target: v8::Local<v8::ObjectTemplate>,
    attr: &str,
    getter: v8::Local<v8::FunctionTemplate>,
    setter: Option<v8::Local<v8::FunctionTemplate>>,
    unforgeable: bool,
) {
    let flags = if unforgeable {
        v8::PropertyAttribute::DONT_DELETE
    } else {
        v8::PropertyAttribute::NONE
    };
    target.set_accessor_property(name(scope, attr).into(), Some(getter), setter, flags);
}

/// Installs a static accessor on the interface object.
pub fn static_accessor(
    scope: &v8::PinScope<'_, '_, ()>,
    target: v8::Local<v8::FunctionTemplate>,
    attr: &str,
    getter: v8::Local<v8::FunctionTemplate>,
    setter: Option<v8::Local<v8::FunctionTemplate>>,
) {
    target.set_accessor_property(
        name(scope, attr).into(),
        Some(getter),
        setter,
        v8::PropertyAttribute::NONE,
    );
}

/// Installs an operation (a data property holding the function).
pub fn operation(
    scope: &v8::PinScope<'_, '_, ()>,
    target: &v8::Template,
    op: &str,
    f: v8::Local<v8::FunctionTemplate>,
    unforgeable: bool,
) {
    let flags = if unforgeable {
        v8::PropertyAttribute::DONT_DELETE | v8::PropertyAttribute::READ_ONLY
    } else {
        v8::PropertyAttribute::NONE
    };
    target.set_with_attr(name(scope, op).into(), f.into(), flags);
}

/// HTML §7.10.5, a Location object's creation steps 3 and 4: its own `valueOf` is the realm's
/// %Object.prototype.valueOf% and its own @@toPrimitive is undefined, both non-writable,
/// non-enumerable and non-configurable (as Blink installs them, on the instance template).
pub fn location_own_properties(
    scope: &v8::PinScope<'_, '_, ()>,
    inst: v8::Local<v8::ObjectTemplate>,
) {
    let flags = || {
        v8::PropertyAttribute::READ_ONLY
            | v8::PropertyAttribute::DONT_ENUM
            | v8::PropertyAttribute::DONT_DELETE
    };
    inst.set_intrinsic_data_property(
        name(scope, "valueOf").into(),
        v8::Intrinsic::ObjProtoValueOf,
        flags(),
    );
    let to_primitive = v8::Symbol::get_to_primitive(scope);
    inst.set_with_attr(to_primitive.into(), v8::undefined(scope).into(), flags());
}

/// Installs a constant on both the interface object and the interface prototype object.
pub fn constant(
    scope: &v8::PinScope<'_, '_, ()>,
    interface: v8::Local<v8::FunctionTemplate>,
    proto: v8::Local<v8::ObjectTemplate>,
    constant_name: &str,
    value: v8::Local<v8::Data>,
) {
    let key = name(scope, constant_name);
    let flags = || v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_DELETE;
    interface.set_with_attr(key.into(), value, flags());
    proto.set_with_attr(key.into(), value, flags());
}

/// Sets `@@toStringTag` on an interface prototype object (Web IDL §3.7.3).
pub fn to_string_tag(
    scope: &v8::PinScope<'_, '_, ()>,
    proto: v8::Local<v8::ObjectTemplate>,
    tag: &str,
) {
    let sym = v8::Symbol::get_to_string_tag(scope);
    proto.set_with_attr(
        sym.into(),
        name(scope, tag).into(),
        v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
    );
}

/// Defines `@@unscopables` on an interface prototype object whose interface (with its mixins) has
/// `[Unscopable]` members (Web IDL §3.7.3): an object with a null prototype and a `true` property
/// per member, read-only, not enumerable, configurable. It is made per realm, as the prototype
/// object is, since a template cannot hold an object with a null prototype.
pub fn unscopables(
    scope: &mut v8::PinScope<'_, '_>,
    interface_object: v8::Local<v8::Function>,
    names: &[&str],
) -> Option<()> {
    let proto_key = name(scope, "prototype");
    let proto: v8::Local<v8::Object> = interface_object
        .get(scope, proto_key.into())?
        .try_into()
        .ok()?;
    let keys: Vec<v8::Local<v8::Name>> = names.iter().map(|n| name(scope, n).into()).collect();
    let yes: v8::Local<v8::Value> = v8::Boolean::new(scope, true).into();
    let values = vec![yes; keys.len()];
    let null: v8::Local<v8::Value> = v8::null(scope).into();
    let object = v8::Object::with_prototype_and_properties(scope, null, &keys, &values);
    let sym = v8::Symbol::get_unscopables(scope);
    proto.define_own_property(
        scope,
        sym.into(),
        object.into(),
        v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
    )?;
    Some(())
}

/// The @@iterator of an interface that supports indexed properties without declaring an iterable
/// (Web IDL §3.7.10): %Array.prototype.values%, and nothing else.
pub fn indexed_iterator(scope: &v8::PinScope<'_, '_, ()>, proto: v8::Local<v8::ObjectTemplate>) {
    proto.set_intrinsic_data_property(
        v8::Symbol::get_iterator(scope).into(),
        v8::Intrinsic::ArrayProtoValues,
        v8::PropertyAttribute::DONT_ENUM,
    );
}

/// The value iterator members of an interface with an indexed getter and `iterable<V>` (Web IDL
/// §3.7.9): `entries`, `keys`, `values`, `forEach` and `@@iterator` are the Array.prototype functions.
pub fn value_iterator(scope: &v8::PinScope<'_, '_, ()>, proto: v8::Local<v8::ObjectTemplate>) {
    proto.set_intrinsic_data_property(
        name(scope, "entries").into(),
        v8::Intrinsic::ArrayProtoEntries,
        v8::PropertyAttribute::NONE,
    );
    proto.set_intrinsic_data_property(
        name(scope, "keys").into(),
        v8::Intrinsic::ArrayProtoKeys,
        v8::PropertyAttribute::NONE,
    );
    proto.set_intrinsic_data_property(
        name(scope, "values").into(),
        v8::Intrinsic::ArrayProtoValues,
        v8::PropertyAttribute::NONE,
    );
    proto.set_intrinsic_data_property(
        name(scope, "forEach").into(),
        v8::Intrinsic::ArrayProtoForEach,
        v8::PropertyAttribute::NONE,
    );
    proto.set_intrinsic_data_property(
        v8::Symbol::get_iterator(scope).into(),
        v8::Intrinsic::ArrayProtoValues,
        v8::PropertyAttribute::DONT_ENUM,
    );
}

/// Defines an interface object on the global object: writable, configurable, not enumerable.
pub fn expose(
    scope: &v8::PinScope<'_, '_>,
    global: v8::Local<v8::Object>,
    interface: &str,
    f: v8::Local<v8::Function>,
) -> Option<()> {
    global.define_own_property(
        scope,
        name(scope, interface).into(),
        f.into(),
        v8::PropertyAttribute::DONT_ENUM,
    )?;
    Some(())
}

/// The arguments-count TypeError of an operation (Web IDL §3.7.6 step "If n < required").
pub fn not_enough_arguments<T>(
    scope: &v8::PinScope<'_, '_>,
    what: &str,
    required: usize,
    given: i32,
) -> Option<T> {
    type_error(
        scope,
        &format!("Failed to execute '{what}': {required} argument(s) required, but only {given} present."),
    )
}

/// Throws the TypeError of calling an interface object without `new`, or one without a constructor.
pub fn illegal_constructor(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    throw_type_error(scope, "Illegal constructor");
}

/// A `[SameObject]` cache: the value stored on the wrapper under a private key.
pub fn same_object_get<'s>(
    scope: &v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    key: &Name,
) -> Option<v8::Local<'s, v8::Value>> {
    let p = cached_private(scope, key);
    let v = obj.get_private(scope, p)?;
    if v.is_undefined() {
        None
    } else {
        Some(v)
    }
}

/// Stores a `[SameObject]` value.
pub fn same_object_set(
    scope: &v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
    key: &Name,
    value: v8::Local<v8::Value>,
) {
    let p = cached_private(scope, key);
    obj.set_private(scope, p, value);
}
