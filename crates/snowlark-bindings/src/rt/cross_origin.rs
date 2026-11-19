// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cross-origin `Window` and `Location` objects (HTML §7.2.1.3 "Shared internal slot", §7.2.1.3.1 to
//! §7.2.1.3.6, the WindowProxy exotic object §7.2.3 and the Location exotic object §7.2.4).
//!
//! The objects are the ordinary wrappers, with V8's access check on their templates
//! ([`snowlark_js::access_check`]): a realm that is not same origin-domain with the object reaches only
//! the handlers here. So nothing here is on the same-origin path, and an access V8 knows no handler
//! for is refused. The handlers know the object by its handle and go to the engine only through
//! [`Host`], by the object's token; a frame in another process needs nothing else from them.
//!
//! What is left to the failed access check callback (a `SecurityError`): `[[Delete]]`, and
//! `[[SetPrototypeOf]]` and `[[PreventExtensions]]`, which should return false and do not because V8
//! reports them as refused accesses before looking at the object.

use super::*;
use snowlark_js::access_check;
use v8::MapFnTo;

/// A member of `CrossOriginProperties(O)`.
pub struct CrossOriginProperty {
    pub name: &'static str,
    pub kind: CrossOriginKind,
}

/// [[NeedsGet]] and [[NeedsSet]], and the callbacks that perform the IDL member's steps. The callbacks
/// are the interface's own: with a token as their data they act on the object it names, whatever
/// `this` is ([`cross_origin_this`]).
pub enum CrossOriginKind {
    /// Neither: the operation.
    Method {
        call: v8::FunctionCallback,
        length: i32,
    },
    Accessor {
        get: Option<v8::FunctionCallback>,
        set: Option<v8::FunctionCallback>,
    },
}

/// The handle of the global object of a realm made by [`register_realm`], kept in the context: a
/// context's global proxy goes on to the next document, its Window does not.
pub(crate) struct RealmGlobal(pub Handle);

/// [`Host`], type-erased.
#[derive(Clone, Copy)]
pub(crate) struct CrossOriginHost {
    may_access: fn(&dyn Any, Handle, Handle) -> bool,
    token_of: fn(&dyn Any, Handle) -> u64,
    object_of: fn(&dyn Any, u64) -> Option<Handle>,
    named_child: fn(&dyn Any, Handle, &str) -> Option<Handle>,
    child: fn(&dyn Any, Handle, u32) -> Option<Handle>,
    active_window: fn(&dyn Any, Handle) -> Option<Option<Handle>>,
}

fn host_of<D: Host + 'static>() -> CrossOriginHost {
    CrossOriginHost {
        may_access: |e, a, t| e.downcast_ref::<D>().is_some_and(|e| e.may_access(a, t)),
        token_of: |e, h| e.downcast_ref::<D>().map_or(0, |e| e.cross_origin_token(h)),
        object_of: |e, t| e.downcast_ref::<D>().and_then(|e| e.cross_origin_object(t)),
        named_child: |e, w, n| {
            e.downcast_ref::<D>()
                .and_then(|e| e.cross_origin_named_child(w, n))
        },
        child: |e, w, i| e.downcast_ref::<D>().and_then(|e| e.window_child(w, i)),
        active_window: |e, h| e.downcast_ref::<D>().and_then(|e| e.active_window(h)),
    }
}

/// The handle of the global object of `context`'s realm: its own, not that of whatever Window its
/// global proxy shows now. `None` for a context [`register_realm`] did not register.
pub fn realm_global(context: v8::Local<v8::Context>) -> Option<Handle> {
    context.get_slot::<RealmGlobal>().map(|r| r.0)
}

/// Makes `context` a realm of the global `handle` (see [`realm_global`]).
pub(crate) fn set_realm_global(
    scope: &v8::PinScope<'_, '_>,
    context: v8::Local<v8::Context>,
    handle: Handle,
    proto: u16,
) {
    context.set_slot(Rc::new(RealmGlobal(handle)));
    // The realm's [[CrossOriginPropertyDescriptorMap]] entries: held by the context, so they keep
    // nothing else alive and go with it.
    let map = v8::Map::new(scope);
    context.set_embedder_data(MAP_SLOT, map.into());
    // What will stand for the global in the wrapper cache once the global proxy has gone on: alive
    // exactly as long as the context, and out of script's reach ([`note_replaced_window`]).
    let st = state(scope);
    let template = {
        let mut t = st.realm_anchor_template.borrow_mut();
        let t = t.get_or_insert_with(|| {
            let template = v8::ObjectTemplate::new(scope);
            template.set_internal_field_count(WRAPPER_FIELDS);
            v8::Global::new(scope, template)
        });
        v8::Local::new(scope, &*t)
    };
    if let Some(anchor) = template.new_instance(scope) {
        init_wrapper(anchor, handle, proto);
        context.set_embedder_data(ANCHOR_SLOT, anchor.into());
    }
}

const MAP_SLOT: i32 = 0;
const ANCHOR_SLOT: i32 = 1;

/// Web IDL "perform a security check" for a member of a `Window` that is not in
/// `CrossOriginProperties`: throws a `SecurityError` and returns false unless the current realm may
/// touch `this`. The usual case, a realm's own Window, is settled by V8's context of the object: the
/// realm its global proxy shows now, which is not the current one for code of the document before.
#[inline]
pub fn window_security_check(
    scope: &mut v8::PinScope<'_, '_>,
    this: v8::Local<v8::Object>,
) -> bool {
    let current = scope.get_current_context();
    if this.get_creation_context(scope) == Some(current) {
        return true;
    }
    object_security_check(scope, current, this)
}

/// [`window_security_check`] for a `Location`, whose V8 context says nothing: it is the context that
/// happened to make the wrapper.
pub fn location_security_check(
    scope: &mut v8::PinScope<'_, '_>,
    this: v8::Local<v8::Object>,
) -> bool {
    let current = scope.get_current_context();
    object_security_check(scope, current, this)
}

#[cold]
fn object_security_check(
    scope: &mut v8::PinScope<'_, '_>,
    current: v8::Local<v8::Context>,
    this: v8::Local<v8::Object>,
) -> bool {
    if may_access(scope, current, this) {
        return true;
    }
    throw_security_error(scope);
    false
}

/// `IsPlatformObjectSameOrigin(O)` for code in `context`. Fails closed: a context that is no realm,
/// or an object that is no wrapper, may not.
fn may_access(
    scope: &v8::PinScope<'_, '_>,
    context: v8::Local<v8::Context>,
    object: v8::Local<v8::Object>,
) -> bool {
    let st = state(scope);
    let Some(host) = st.cross_origin_host.get() else {
        return false;
    };
    let (Some(accessing), Some(target)) = (realm_global(context), unwrap(&object, 1, u16::MAX))
    else {
        return false;
    };
    (host.may_access)(&*st.engine, accessing, target)
}

fn throw_security_error(scope: &mut v8::PinScope<'_, '_>) {
    throw(
        scope,
        Error::dom(
            ExceptionName::SecurityError,
            "Blocked an access to a cross-origin object",
        ),
    );
}

/// `this` for a member in `CrossOriginProperties`: the object a cross-origin function was made for
/// when the callback runs as one, else the receiver, with no security check. `None` (the object is
/// gone, or the receiver fails the brand check) has no exception set.
#[inline]
pub fn cross_origin_this(
    scope: &v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    lo: u16,
    hi: u16,
) -> Option<Handle> {
    let data = args.data();
    if data.is_number() {
        return bound_object(scope, data);
    }
    unwrap(&args.this(), lo, hi)
}

#[cold]
fn bound_object(scope: &v8::PinScope<'_, '_>, data: v8::Local<v8::Value>) -> Option<Handle> {
    let token = data.number_value(scope)? as u64;
    let st = state(scope);
    (st.cross_origin_host.get()?.object_of)(&*st.engine, token)
}

/// Gives instances of `template` the access check, with `properties` as `CrossOriginProperties` of
/// the interface `id`.
pub fn guard_instances<D: Host + 'static>(
    scope: &v8::PinScope<'_, '_>,
    template: v8::Local<v8::ObjectTemplate>,
    id: u16,
    properties: Vec<CrossOriginProperty>,
) {
    let st = state(scope);
    st.cross_origin_host.set(Some(host_of::<D>()));
    st.cross_origin.borrow_mut().insert(id, Rc::new(properties));
    access_check::guard_template(
        template,
        access_check::NamedHandlers {
            getter: guarded_named_get(named_get).map_fn_to(),
            setter: guarded_named_set(named_set).map_fn_to(),
            query: guarded_named_query(named_query).map_fn_to(),
            enumerator: guarded_enumerate(named_keys).map_fn_to(),
            definer: guarded_named_define(named_define).map_fn_to(),
            descriptor: guarded_named_get(named_descriptor).map_fn_to(),
        },
        access_check::IndexedHandlers {
            getter: guarded_indexed_get(indexed_get).map_fn_to(),
            setter: guarded_indexed_set(indexed_set).map_fn_to(),
            query: guarded_indexed_query(indexed_query).map_fn_to(),
            enumerator: guarded_enumerate(indexed_keys).map_fn_to(),
            definer: guarded_indexed_define(indexed_define).map_fn_to(),
            descriptor: guarded_indexed_get(indexed_descriptor).map_fn_to(),
        },
    );
}

/// The isolate's access decisions.
pub(crate) fn install_access_hooks(scope: &mut v8::PinScope<'_, '_>) {
    access_check::install(
        scope,
        access_check::Hooks {
            allow: |scope, context, object| may_access(scope, context, object),
            failed: |scope, _| throw_security_error(scope),
        },
    );
}

/// What `[[GetOwnProperty]]` found.
enum Own<'s> {
    Data {
        value: v8::Local<'s, v8::Value>,
        enumerable: bool,
    },
    Accessor {
        get: Option<v8::Local<'s, v8::Function>>,
        set: Option<v8::Local<'s, v8::Function>>,
    },
}

struct Target {
    handle: Handle,
    tag: u16,
    window: bool,
}

/// The object a handler was called for. A handler only runs on our own wrappers; anything else is
/// refused like an unknown property.
fn target(holder: v8::Local<v8::Object>, st: &State) -> Option<Target> {
    let tag = proto_id(&holder);
    let handle = unwrap(&holder, 1, u16::MAX)?;
    let table = st.cross_origin.borrow();
    table.get(&tag)?;
    Some(Target {
        handle,
        tag,
        window: table.get(&tag)?.iter().any(|p| p.name == "window"),
    })
}

/// A function of the current realm with `callback`'s steps on the object `token` names:
/// CrossOriginGetOwnPropertyHelper's anonymous built-in functions, one per realm, object, property
/// and kind ([[CrossOriginPropertyDescriptorMap]]).
fn cross_origin_function<'s>(
    scope: &v8::PinScope<'s, '_>,
    token: u64,
    slot: u64,
    callback: v8::FunctionCallback,
    name_: &str,
    length: i32,
) -> Option<v8::Local<'s, v8::Function>> {
    let context = scope.get_current_context();
    realm_global(context)?;
    let map = context.get_embedder_data(scope, MAP_SLOT)?;
    let map = v8::Local::<v8::Map>::try_from(map).ok()?;
    let key: v8::Local<v8::Value> = v8::Number::new(scope, (token * 64 + slot) as f64).into();
    if let Some(f) = map
        .get(scope, key)
        .and_then(|f| v8::Local::<v8::Function>::try_from(f).ok())
    {
        return Some(f);
    }
    let data = v8::Number::new(scope, token as f64);
    let f = v8::Function::builder_raw(callback)
        .data(data.into())
        .length(length)
        .constructor_behavior(v8::ConstructorBehavior::Throw)
        .build(scope)?;
    f.set_name(str(scope, name_)?);
    map.set(scope, key, f.into())?;
    Some(f)
}

/// CrossOriginGetOwnPropertyHelper(O, P), for the member at `index` of the table.
fn own_member<'s>(
    scope: &v8::PinScope<'s, '_>,
    st: &State,
    t: &Target,
    index: usize,
) -> Option<Own<'s>> {
    let table = st.cross_origin.borrow().get(&t.tag)?.clone();
    let host = st.cross_origin_host.get()?;
    let token = (host.token_of)(&*st.engine, t.handle);
    let p = table.get(index)?;
    let slot = index as u64 * 2;
    match &p.kind {
        CrossOriginKind::Method { call, length } => {
            let f = cross_origin_function(scope, token, slot, *call, p.name, *length)?;
            Some(Own::Data {
                value: f.into(),
                enumerable: false,
            })
        }
        CrossOriginKind::Accessor { get, set } => {
            let get = match get {
                Some(g) => Some(cross_origin_function(
                    scope,
                    token,
                    slot,
                    *g,
                    &format!("get {}", p.name),
                    0,
                )?),
                None => None,
            };
            let set = match set {
                Some(s) => Some(cross_origin_function(
                    scope,
                    token,
                    slot + 1,
                    *s,
                    &format!("set {}", p.name),
                    1,
                )?),
                None => None,
            };
            Some(Own::Accessor { get, set })
        }
    }
}

/// CrossOriginPropertyFallback(P) for a string key.
fn fallback_string(s: &str) -> bool {
    s == "then"
}

/// CrossOriginPropertyFallback(P) for a symbol key: the ones script asks of any object it is given.
fn fallback_symbol(scope: &v8::PinScope<'_, '_>, key: v8::Local<v8::Name>) -> bool {
    let k: v8::Local<v8::Value> = key.into();
    let same = |s: v8::Local<v8::Symbol>| {
        let s: v8::Local<v8::Value> = s.into();
        s == k
    };
    same(v8::Symbol::get_to_string_tag(scope))
        || same(v8::Symbol::get_has_instance(scope))
        || same(v8::Symbol::get_is_concat_spreadable(scope))
}

/// `[[GetOwnProperty]]` of a cross-origin object for a named key: the steps of Window's and
/// Location's. `None`: a `SecurityError` was thrown.
fn get_own<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    holder: v8::Local<'s, v8::Object>,
    key: v8::Local<'s, v8::Name>,
) -> Option<Own<'s>> {
    let st = state(scope);
    let found = (|| {
        let t = target(holder, &st)?;
        let undefined = || Own::Data {
            value: v8::undefined(scope).into(),
            enumerable: false,
        };
        if key.is_symbol() {
            return fallback_symbol(scope, key).then(undefined);
        }
        let name_ = key.to_rust_string_lossy(scope);
        let table = st.cross_origin.borrow().get(&t.tag)?.clone();
        if let Some(i) = table.iter().position(|p| p.name == name_) {
            return own_member(scope, &st, &t, i);
        }
        if t.window {
            let host = st.cross_origin_host.get()?;
            if let Some(child) = (host.named_child)(&*st.engine, t.handle, &name_) {
                let value = wrap(scope, child)?;
                return Some(Own::Data {
                    value: value.into(),
                    enumerable: false,
                });
            }
        }
        fallback_string(&name_).then(undefined)
    })();
    if found.is_none() {
        throw_security_error(scope);
    }
    found
}

/// CrossOriginGet(O, P, Receiver).
fn get_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    own: Own<'s>,
    receiver: v8::Local<'s, v8::Value>,
) -> Option<v8::Local<'s, v8::Value>> {
    match own {
        Own::Data { value, .. } => Some(value),
        Own::Accessor { get: Some(g), .. } => g.call(scope, receiver, &[]),
        Own::Accessor { get: None, .. } => {
            throw_security_error(scope);
            None
        }
    }
}

fn named_get<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: v8::Local<'s, v8::Name>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    if let Some(own) = get_own(scope, args.holder(), key) {
        if let Some(v) = get_value(scope, own, args.holder().into()) {
            rv.set(v);
        }
    }
    v8::Intercepted::kYes
}

/// CrossOriginSet(O, P, V, Receiver).
fn named_set<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: v8::Local<'s, v8::Name>,
    value: v8::Local<'s, v8::Value>,
    args: v8::PropertyCallbackArguments<'s>,
    _rv: v8::ReturnValue<v8::Boolean>,
) -> v8::Intercepted {
    match get_own(scope, args.holder(), key) {
        Some(Own::Accessor { set: Some(s), .. }) => {
            s.call(scope, args.holder().into(), &[value]);
        }
        Some(_) => throw_security_error(scope),
        None => {}
    }
    v8::Intercepted::kYes
}

fn named_query<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: v8::Local<'s, v8::Name>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Integer>,
) -> v8::Intercepted {
    if get_own(scope, args.holder(), key).is_some() {
        rv.set_int32(v8::PropertyAttribute::DONT_ENUM.as_u32() as i32);
    }
    v8::Intercepted::kYes
}

/// A descriptor object, with no prototype so that nothing a page put on `Object.prototype` joins in.
fn descriptor_object<'s>(scope: &v8::PinScope<'s, '_>, own: Own<'s>) -> v8::Local<'s, v8::Object> {
    let t: v8::Local<v8::Value> = v8::Boolean::new(scope, true).into();
    let f: v8::Local<v8::Value> = v8::Boolean::new(scope, false).into();
    let undefined: v8::Local<v8::Value> = v8::undefined(scope).into();
    let null: v8::Local<v8::Value> = v8::null(scope).into();
    let configurable = name(scope, "configurable").into();
    let enumerable = name(scope, "enumerable").into();
    match own {
        Own::Data {
            value,
            enumerable: e,
        } => v8::Object::with_prototype_and_properties(
            scope,
            null,
            &[
                name(scope, "value").into(),
                name(scope, "writable").into(),
                enumerable,
                configurable,
            ],
            &[value, f, if e { t } else { f }, t],
        ),
        Own::Accessor { get, set } => v8::Object::with_prototype_and_properties(
            scope,
            null,
            &[
                name(scope, "get").into(),
                name(scope, "set").into(),
                enumerable,
                configurable,
            ],
            &[
                get.map_or(undefined, Into::into),
                set.map_or(undefined, Into::into),
                f,
                t,
            ],
        ),
    }
}

fn named_descriptor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: v8::Local<'s, v8::Name>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    if let Some(own) = get_own(scope, args.holder(), key) {
        rv.set(descriptor_object(scope, own).into());
    }
    v8::Intercepted::kYes
}

/// `[[DefineOwnProperty]]`: throws.
fn named_define<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _key: v8::Local<'s, v8::Name>,
    _desc: &v8::PropertyDescriptor,
    _args: v8::PropertyCallbackArguments<'s>,
    _rv: v8::ReturnValue<v8::Boolean>,
) -> v8::Intercepted {
    throw_security_error(scope);
    v8::Intercepted::kYes
}

/// CrossOriginOwnPropertyKeys(O): the members, `then`, then the symbols.
fn named_keys<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Array>,
) {
    let st = state(scope);
    let Some(t) = target(args.holder(), &st) else {
        return;
    };
    let Some(table) = st.cross_origin.borrow().get(&t.tag).cloned() else {
        return;
    };
    let mut keys: Vec<v8::Local<v8::Value>> = Vec::with_capacity(table.len() + 4);
    for p in table.iter() {
        keys.push(name(scope, p.name).into());
    }
    keys.push(name(scope, "then").into());
    keys.push(v8::Symbol::get_to_string_tag(scope).into());
    keys.push(v8::Symbol::get_has_instance(scope).into());
    keys.push(v8::Symbol::get_is_concat_spreadable(scope).into());
    rv.set(v8::Array::new_with_elements(scope, &keys));
}

/// The child at `index` of a cross-origin Window (WindowProxy's [[GetOwnProperty]] step 2), else a
/// `SecurityError`. A Location has no indexed properties.
fn indexed_child<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    holder: v8::Local<'s, v8::Object>,
    index: u32,
) -> Option<v8::Local<'s, v8::Object>> {
    let st = state(scope);
    let child = (|| {
        let t = target(holder, &st)?;
        if !t.window {
            return None;
        }
        let host = st.cross_origin_host.get()?;
        let child = (host.child)(&*st.engine, t.handle, index)?;
        wrap(scope, child)
    })();
    if child.is_none() {
        throw_security_error(scope);
    }
    child
}

fn indexed_get<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    index: u32,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    if let Some(child) = indexed_child(scope, args.holder(), index) {
        rv.set(child.into());
    }
    v8::Intercepted::kYes
}

fn indexed_set<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _index: u32,
    _value: v8::Local<'s, v8::Value>,
    _args: v8::PropertyCallbackArguments<'s>,
    _rv: v8::ReturnValue<v8::Boolean>,
) -> v8::Intercepted {
    throw_security_error(scope);
    v8::Intercepted::kYes
}

fn indexed_query<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    index: u32,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Integer>,
) -> v8::Intercepted {
    if indexed_child(scope, args.holder(), index).is_some() {
        rv.set_int32(v8::PropertyAttribute::READ_ONLY.as_u32() as i32);
    }
    v8::Intercepted::kYes
}

fn indexed_descriptor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    index: u32,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    if let Some(child) = indexed_child(scope, args.holder(), index) {
        let own = Own::Data {
            value: child.into(),
            enumerable: true,
        };
        rv.set(descriptor_object(scope, own).into());
    }
    v8::Intercepted::kYes
}

fn indexed_define<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _index: u32,
    _desc: &v8::PropertyDescriptor,
    _args: v8::PropertyCallbackArguments<'s>,
    _rv: v8::ReturnValue<v8::Boolean>,
) -> v8::Intercepted {
    throw_security_error(scope);
    v8::Intercepted::kYes
}

fn indexed_keys<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::PropertyCallbackArguments<'s>,
    mut rv: v8::ReturnValue<v8::Array>,
) {
    let st = state(scope);
    let Some(t) = target(args.holder(), &st).filter(|t| t.window) else {
        return;
    };
    let Some(host) = st.cross_origin_host.get() else {
        return;
    };
    let mut keys: Vec<v8::Local<v8::Value>> = Vec::new();
    let mut i = 0;
    while (host.child)(&*st.engine, t.handle, i).is_some() {
        keys.push(v8::Integer::new_from_unsigned(scope, i).into());
        i += 1;
    }
    rv.set(v8::Array::new_with_elements(scope, &keys));
}

/// The WindowProxy a handle of a replaced Window now stands for, when `handle` is one
/// ([`note_replaced_window`]): `None` when `handle` is not, `Some(None)` when its browsing context has
/// no Window to show any more.
pub(crate) fn replaced_window(st: &State, handle: Handle) -> Option<Option<Handle>> {
    let host = st.cross_origin_host.get()?;
    (host.active_window)(&*st.engine, handle)
}

/// A context's global proxy goes on to the context's next document. The wrapper of `old`, the Window
/// that stays behind in `context`, becomes the realm's anchor (an object of the old realm that script
/// cannot reach, and that lives exactly as long as the realm's code can run), and [`wrap`] gives the
/// proxy for `old`.
pub fn note_replaced_window(
    scope: &v8::PinScope<'_, '_>,
    old: Handle,
    context: v8::Local<v8::Context>,
) {
    if realm_global(context) != Some(old) {
        return;
    }
    let Some(anchor) = context
        .get_embedder_data(scope, ANCHOR_SLOT)
        .and_then(|a| v8::Local::<v8::Object>::try_from(a).ok())
    else {
        return;
    };
    let st = state(scope);
    st.wrappers
        .borrow_mut()
        .insert(old.get(), Wrapper::new(scope, anchor));
    st.gc.pending.borrow_mut().push(old.get());
    st.max_replaced.set(st.max_replaced.get().max(old.get()));
}
