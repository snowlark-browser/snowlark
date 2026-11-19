// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wrappers: a platform object's JS object, with the engine's [`Handle`] in internal fields 0 (its low 31
//! bits) and 2 (the rest), the interface's prototype id in internal field 1, and its wrapper group links in
//! fields 3 to 6 (see `gc`). Prototype ids are numbered in depth-first order of the
//! inheritance tree, so "implements interface I" is the range check `id(I) <= tag < id(I) + subtree(I)`.

use super::*;

/// Internal fields of every wrapper: the handle's low bits, prototype id, the handle's high bits, and the
/// group links (anchor, first member, next and previous member).
pub const WRAPPER_FIELDS: usize = 7;
const HANDLE_FIELD: i32 = 0;
const TAG_FIELD: i32 = 1;
const HANDLE_HIGH_FIELD: i32 = 2;
/// A field holds a 31-bit Smi (`snowlark_js::internal_field`); a handle takes two.
const FIELD_BITS: u32 = 31;

/// The handle whose two halves are `low` and `high`.
#[inline(always)]
fn join(low: u32, high: u32) -> Handle {
    Handle(u64::from(low) | u64::from(high) << FIELD_BITS)
}

/// Stores a handle and prototype id in a fresh wrapper's internal fields, as Smis
/// (`snowlark_js::internal_field`).
pub fn init_wrapper(obj: v8::Local<v8::Object>, handle: Handle, proto_id: u16) {
    debug_assert!(proto_id > 0);
    let h = handle.get();
    // SAFETY: every wrapper template has `WRAPPER_FIELDS` fields; each value has at most 31 bits
    // (`Handle::MAX` has 62).
    unsafe {
        snowlark_js::internal_field::write(
            &obj,
            HANDLE_FIELD,
            h as u32 & snowlark_js::internal_field::MAX,
        );
        snowlark_js::internal_field::write(&obj, HANDLE_HIGH_FIELD, (h >> FIELD_BITS) as u32);
        snowlark_js::internal_field::write(&obj, TAG_FIELD, u32::from(proto_id));
    }
}

/// The handle `init_wrapper` stored in `obj`.
///
/// # Safety
///
/// `obj` is a wrapper: it has [`WRAPPER_FIELDS`] fields.
#[inline(always)]
pub(crate) unsafe fn read_handle(obj: &v8::Object) -> Handle {
    // SAFETY: as the caller promised.
    unsafe {
        join(
            snowlark_js::internal_field::read(obj, HANDLE_FIELD),
            snowlark_js::internal_field::read(obj, HANDLE_HIGH_FIELD),
        )
    }
}

/// The prototype id of a wrapper, or 0 when `obj` is not one.
#[inline(always)]
pub fn proto_id(obj: &v8::Object) -> u16 {
    if obj.internal_field_count() != WRAPPER_FIELDS {
        return 0;
    }
    // SAFETY: objects with exactly `WRAPPER_FIELDS` internal fields are wrappers made by
    // `init_wrapper` (every template in this crate sets that many fields; V8's own objects with embedder
    // fields, array buffers and typed arrays, have two). Without the V8 sandbox (the v8 crate's default
    // build) the read is a plain load of the field, and a wrapper not yet initialized reads 0, which is no
    // prototype id.
    let raw = unsafe { snowlark_js::internal_field::read(obj, TAG_FIELD) };
    raw as u16
}

/// The handle of a wrapper whose prototype id is in `lo..hi` (the brand check), or `None`.
#[inline(always)]
pub fn unwrap(obj: &v8::Object, lo: u16, hi: u16) -> Option<Handle> {
    let tag = proto_id(obj);
    if tag < lo || tag >= hi {
        return None;
    }
    // SAFETY: `proto_id` found one of our prototype ids, so `obj` is a wrapper.
    Some(unsafe { read_handle(obj) })
}

/// The handle of a callback's receiver when the callback's template has a receiver signature: V8 checked
/// that the receiver is one of the interface's objects before the call, so the brand check is not
/// repeated (Blink's generated bindings take such a receiver with `ToWrappableUnsafe`; `fast_receiver`
/// relies on the same check for Fast API calls).
///
/// # Safety
///
/// Only for the receiver of a callback whose template has a receiver `Signature`, and that is never
/// called with a global proxy as its receiver: such a receiver was made from one of our templates, so
/// it has [`WRAPPER_FIELDS`] fields, and `init_wrapper` stored its handle before script could see it.
#[inline(always)]
pub unsafe fn receiver(obj: &v8::Object) -> Handle {
    // SAFETY: see above; `obj` is a wrapper.
    unsafe { read_handle(obj) }
}

/// Like [`unwrap`] for an arbitrary value.
#[inline(always)]
pub fn unwrap_value(v: v8::Local<v8::Value>, lo: u16, hi: u16) -> Option<Handle> {
    if !v.is_object() {
        return None;
    }
    // SAFETY: `v` is an object (checked above; `try_into` would check again, another FFI call).
    let obj = unsafe { v8::Local::<v8::Object>::cast_unchecked(v) };
    unwrap(&obj, lo, hi)
}

/// The JS wrapper for a handle: the cached one, or a new object of the interface the engine says the
/// handle's object implements (its most derived interface).
///
/// A Window that a navigation replaced has its browsing context's WindowProxy as its JS value: the
/// object its wrapper entry holds (the old realm's global object) never reaches script.
pub fn wrap<'s>(scope: &v8::PinScope<'s, '_>, handle: Handle) -> Option<v8::Local<'s, v8::Object>> {
    let st = state(scope);
    if handle.get() <= st.max_replaced.get() {
        if let Some(window) = replaced_window(&st, handle) {
            return wrap_own(scope, &st, window?);
        }
    }
    wrap_own(scope, &st, handle)
}

/// [`wrap`] of the handle's own wrapper.
pub(crate) fn wrap_own<'s>(
    scope: &v8::PinScope<'s, '_>,
    st: &State,
    handle: Handle,
) -> Option<v8::Local<'s, v8::Object>> {
    if let Some(obj) = st
        .wrappers
        .borrow()
        .get(&handle.get())
        .and_then(|w| w.get(scope))
    {
        return Some(obj);
    }
    let id = (st.interface_of)(&*st.engine, handle);
    // Web IDL "internally create a new object implementing the interface" in the object's relevant
    // realm: its prototype and its own `[LegacyUnforgeable]` members are all that realm's, whoever
    // asks. V8 instantiates a template in the context it is given, which needs no entering: nothing
    // runs but the instantiation.
    if let Some(realm) = relevant_context(scope, st, handle) {
        let wrapper = {
            // SAFETY: the isolate is entered on this thread and `scope` keeps a handle scope open.
            v8::callback_scope!(unsafe inner, realm);
            let obj = new_instance(inner, st, realm, id)?;
            init_wrapper(obj, handle, id);
            attach_error_stack(realm, obj, id);
            Wrapper::new(inner, obj)
        };
        let obj = wrapper.get(scope)?;
        st.wrappers.borrow_mut().insert(handle.get(), wrapper);
        wrapper_made(scope, st, handle);
        return Some(obj);
    }
    let obj = new_instance(scope, st, scope.get_current_context(), id)?;
    init_wrapper(obj, handle, id);
    attach_error_stack(scope.get_current_context(), obj, id);
    st.wrappers
        .borrow_mut()
        .insert(handle.get(), Wrapper::new(scope, obj));
    wrapper_made(scope, st, handle);
    Some(obj)
}

/// Web IDL's exception objects are error objects (a `DOMException` has an [[ErrorData]] slot): V8
/// captures a stack for one as it does for its own errors, which gives it a `stack` string, as an
/// own property as the engines have it (the stack accessor on `Error.prototype` is a proposal this
/// V8 does not ship, so WPT's "no own stack property" is not met).
fn attach_error_stack(context: v8::Local<v8::Context>, obj: v8::Local<v8::Object>, id: u16) {
    if id == crate::generated::ids::DOM_EXCEPTION {
        let _ = v8::Exception::capture_stack_trace(context, obj);
    }
}

/// The first embedder data slot of the boilerplates [`new_instance`] keeps, one per prototype id
/// (after `cross_origin`'s two).
const BOILERPLATE_SLOT: i32 = 2;

/// A new object of interface `id` in `context` (the current context of `scope`), as its template's
/// `instance_template().new_instance()` makes it: a clone of the context's boilerplate of
/// the interface, which the first call makes from the template, as Blink's
/// `V8PerContextData::CreateWrapperFromCache` does. Cloning copies the map (so the prototype and
/// creation context), the own properties the template installs and the internal fields; the
/// boilerplate itself is never given out. It is held in the context's embedder data, so it lives
/// exactly as long as the context.
#[inline]
fn new_instance<'s>(
    scope: &v8::PinScope<'s, '_>,
    st: &State,
    context: v8::Local<v8::Context>,
    id: u16,
) -> Option<v8::Local<'s, v8::Object>> {
    let slot = BOILERPLATE_SLOT + i32::from(id);
    // (the `v8` crate's slot numbers start after its own two fields)
    if snowlark_js::object_clone::embedder_data_fields(context) as i32 > slot + 2 {
        if let Some(boilerplate) = context
            .get_embedder_data(scope, slot)
            .and_then(|v| v8::Local::<v8::Object>::try_from(v).ok())
        {
            return Some(snowlark_js::object_clone::clone(scope, boilerplate));
        }
    }
    let template = st.template(scope, id)?;
    let boilerplate = template.instance_template(scope).new_instance(scope)?;
    context.set_embedder_data(slot, boilerplate.into());
    Some(snowlark_js::object_clone::clone(scope, boilerplate))
}

/// The context of `handle`'s relevant realm ([`Host::realm_of`]), when it is not the current one.
#[inline]
fn relevant_context<'s>(
    scope: &v8::PinScope<'s, '_>,
    st: &State,
    handle: Handle,
) -> Option<v8::Local<'s, v8::Context>> {
    if st.realms.get() < 2 {
        return None;
    }
    let realm = st.realm_of.get()?(&*st.engine, handle)?;
    let current = scope.get_current_context();
    if realm_global(current).map(|h| h.get()) == Some(realm) {
        return None;
    }
    realm_context(scope, st, realm).filter(|c| *c != current)
}

/// The context `realm` names: a registered realm's, or for an object its wrapper's (an object keeps
/// its realm whatever happens to it later, a node's adoption for one), or failing that its own.
fn realm_context<'s>(
    scope: &v8::PinScope<'s, '_>,
    st: &State,
    realm: u64,
) -> Option<v8::Local<'s, v8::Context>> {
    if let Some(context) = st.realm_contexts.borrow().get(&realm) {
        return context.to_local(scope);
    }
    let wrapper = st.wrappers.borrow().get(&realm).and_then(|w| w.get(scope));
    if let Some(wrapper) = wrapper {
        return wrapper.get_creation_context(scope);
    }
    let owner = st.realm_of.get()?(&*st.engine, Handle::new(realm))?;
    if owner == realm {
        return None;
    }
    realm_context(scope, st, owner)
}

/// Web IDL "invoke a callback function" and "call a user object's operation", what they do around
/// the call: `f` runs in `callback`'s realm ("prepare to run script"), with the incumbent settings
/// object its callback context ("prepare to run a callback"). `stored` is that context when it is not
/// the callback's own realm ([`callback_context`]). A callback of the current realm is not switched
/// to; the incumbent is pushed all the same, as older script of another realm may be on the stack.
#[inline]
pub fn in_callback_realm<R>(
    cx: &mut Cx,
    callback: &JsObject,
    stored: Option<Handle>,
    f: impl FnOnce(&mut Cx) -> R,
) -> R {
    let callback = v8::Local::new(cx.scope(), &callback.0);
    in_realm_of_callback(cx, callback, stored, f)
}

/// [`in_callback_realm`] of a callback in hand.
#[inline]
pub fn in_realm_of_callback<R>(
    cx: &mut Cx,
    callback: v8::Local<v8::Object>,
    stored: Option<Handle>,
    f: impl FnOnce(&mut Cx) -> R,
) -> R {
    let scope = cx.scope();
    let current = scope.get_current_context();
    let realm = callback.get_creation_context(scope).unwrap_or(current);
    let incumbent = stored
        .and_then(|h| realm_context(scope, &state(scope), h.get()))
        .unwrap_or(realm);
    snowlark_js::access_check::with_backup_incumbent(incumbent, move || {
        if realm == current {
            return f(cx);
        }
        let scope = &mut v8::ContextScope::new(cx.scope(), realm);
        f(&mut Cx::new(scope))
    })
}

/// A callback's callback context (Web IDL §2.13.23), kept when it is converted: the incumbent realm's
/// global object, or `None` when that is the callback's own realm, as it nearly always is.
pub fn callback_context(scope: &v8::PinScope<'_, '_>, callback: &JsObject) -> Option<Handle> {
    let incumbent = realm_global(snowlark_js::access_check::incumbent_context(scope))?;
    let own = creation_realm(scope, v8::Local::new(scope, &callback.0));
    (own != Some(incumbent)).then_some(incumbent)
}

fn realm_of_thunk<D: Host + 'static>(engine: &dyn Any, handle: Handle) -> Option<u64> {
    engine
        .downcast_ref::<D>()
        .and_then(|e| e.realm_of(handle))
        .map(|h| h.get())
}

/// Registers the current context as a realm whose global object is a wrapper (installed with
/// `install_context`), so that wrappers whose relevant realm it is ([`Host::realm_of`]) are made in
/// it from another realm's code.
pub fn register_realm<D: Host + 'static>(scope: &mut v8::PinScope<'_, '_>) {
    let st = state(scope);
    st.realm_of.set(Some(realm_of_thunk::<D>));
    let context = scope.get_current_context();
    let global = context.global(scope);
    let Some(realm) = unwrap(&global, 1, u16::MAX) else {
        return;
    };
    set_realm_global(scope, context, realm, proto_id(&global));
    st.realm_contexts
        .borrow_mut()
        .insert(realm.get(), v8::Weak::new(scope, context));
    st.realms.set(st.realms.get() + 1);
}

/// A realm registered with [`register_realm`] is gone (its global object died).
pub fn forget_realm(scope: &v8::PinScope<'_, '_>, realm: Handle) {
    let st = state(scope);
    if st
        .realm_contexts
        .borrow_mut()
        .remove(&realm.get())
        .is_some()
    {
        st.realms.set(st.realms.get().saturating_sub(1));
    }
}

/// Makes `obj` (the object V8 created for a `new` call) the wrapper of `handle`.
pub fn adopt_wrapper(
    scope: &v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
    handle: Handle,
    proto_id: u16,
) {
    init_wrapper(obj, handle, proto_id);
    attach_error_stack(scope.get_current_context(), obj, proto_id);
    let st = state(scope);
    st.wrappers
        .borrow_mut()
        .insert(handle.get(), Wrapper::new(scope, obj));
    wrapper_made(scope, &st, handle);
}

/// The JS value of an [`Error`].
pub fn error_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    error: Error,
) -> Option<v8::Local<'s, v8::Value>> {
    match error {
        Error::Type(m) => Some(v8::Exception::type_error(scope, str(scope, &m)?)),
        Error::Range(m) => Some(v8::Exception::range_error(scope, str(scope, &m)?)),
        Error::Dom(name, m) => {
            let f = state(scope).dom_exception;
            f(scope, &m, name.as_str())
        }
        Error::Pending => None,
    }
}

/// Throws the exception an [`Error`] describes (nothing for [`Error::Pending`]).
pub fn throw(scope: &mut v8::PinScope<'_, '_>, error: Error) {
    if let Some(e) = error_value(scope, error) {
        scope.throw_exception(e);
    }
}

/// Throws a `TypeError`.
pub fn throw_type_error(scope: &v8::PinScope<'_, '_>, message: &str) {
    if let Some(m) = str(scope, message) {
        let e = v8::Exception::type_error(scope, m);
        scope.throw_exception(e);
    }
}

/// Throws a `TypeError` and returns `None`, for `?` chains.
pub fn type_error<T>(scope: &v8::PinScope<'_, '_>, message: &str) -> Option<T> {
    throw_type_error(scope, message);
    None
}

/// Throws the "Illegal invocation" `TypeError` for a receiver that fails the brand check.
pub fn illegal_invocation<T>(scope: &v8::PinScope<'_, '_>) -> Option<T> {
    type_error(scope, "Illegal invocation")
}

/// Turns an engine result into an option, throwing the error.
#[inline(always)]
pub fn check<T>(scope: &mut v8::PinScope<'_, '_>, r: Result<T>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            throw(scope, e);
            None
        }
    }
}

/// A V8 string from UTF-8.
#[inline(always)]
pub fn str<'s>(scope: &v8::PinScope<'s, '_, ()>, s: &str) -> Option<v8::Local<'s, v8::String>> {
    v8::String::new_from_utf8(scope, s.as_bytes(), v8::NewStringType::Normal)
}

/// An internalized V8 string, for property names.
pub fn name<'s>(scope: &v8::PinScope<'s, '_, ()>, s: &str) -> v8::Local<'s, v8::String> {
    v8::String::new_from_utf8(scope, s.as_bytes(), v8::NewStringType::Internalized)
        .expect("property name too long")
}

/// A property name generated code reads or keeps a value under on every call (a dictionary member, a
/// `[SameObject]` attribute's private key): its V8 string and private symbol are made once per isolate
/// and kept in the isolate's [`State`], so a call does not look the name up in the string table (and
/// the private symbol registry) again. Each `Name` is a `static` of the generated code; its index into
/// the isolates' tables is given on first use, process-wide. (The reference engine's generated
/// bindings keep dictionary member names as eternal strings per isolate, and `[SameObject]` keys as
/// private symbols per isolate, the same way.)
pub struct Name {
    text: &'static str,
    /// 1 + the index into the tables; 0 until first use.
    slot: std::sync::atomic::AtomicU32,
}

/// The next name index to give (every isolate's tables use the same indices).
static NEXT_NAME: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

impl Name {
    pub const fn new(text: &'static str) -> Name {
        Name {
            text,
            slot: std::sync::atomic::AtomicU32::new(0),
        }
    }

    pub fn text(&self) -> &'static str {
        self.text
    }

    #[inline(always)]
    fn index(&self) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        match self.slot.load(Relaxed) {
            0 => self.assign(),
            s => s as usize - 1,
        }
    }

    #[cold]
    fn assign(&self) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let mine = NEXT_NAME.fetch_add(1, Relaxed) + 1;
        // (two threads may race to the first use: the first index stored wins, the other is unused)
        match self.slot.compare_exchange(0, mine, Relaxed, Relaxed) {
            Ok(_) => mine as usize - 1,
            Err(s) => s as usize - 1,
        }
    }
}

/// `key`'s string in the isolate of `scope`, made the first time.
#[inline]
pub fn cached_name<'s>(scope: &v8::PinScope<'s, '_, ()>, key: &Name) -> v8::Local<'s, v8::String> {
    let i = key.index();
    let Some(st) = scope.get_slot::<Rc<State>>() else {
        return name(scope, key.text);
    };
    if let Some(Some(g)) = st.names.borrow().get(i) {
        return v8::Local::new(scope, g);
    }
    let s = name(scope, key.text);
    let mut names = st.names.borrow_mut();
    if names.len() <= i {
        names.resize_with(i + 1, || None);
    }
    names[i] = Some(v8::Global::new(scope, s));
    s
}

/// The private symbol `v8::Private::for_api(key)` in the isolate of `scope`, made the first time.
#[inline]
pub fn cached_private<'s>(
    scope: &v8::PinScope<'s, '_, ()>,
    key: &Name,
) -> v8::Local<'s, v8::Private> {
    let i = key.index();
    let Some(st) = scope.get_slot::<Rc<State>>() else {
        let k = name(scope, key.text);
        return v8::Private::for_api(scope, Some(k));
    };
    if let Some(Some(g)) = st.privates.borrow().get(i) {
        return v8::Local::new(scope, g);
    }
    let k = name(scope, key.text);
    let p = v8::Private::for_api(scope, Some(k));
    let mut privates = st.privates.borrow_mut();
    if privates.len() <= i {
        privates.resize_with(i + 1, || None);
    }
    privates[i] = Some(v8::Global::new(scope, p));
    p
}

/// Keeps the template of a `[Global]` interface's named properties object (Web IDL §3.7.4).
pub fn set_named_properties_template(
    scope: &v8::PinScope<'_, '_>,
    iface: u16,
    t: v8::Local<v8::FunctionTemplate>,
) {
    state(scope)
        .named_properties_templates
        .borrow_mut()
        .insert(iface, v8::Global::new(scope, t));
}

/// Puts the named properties object of `iface` between its interface prototype object and the next
/// prototype (Web IDL §3.7.4), so named properties never sit on the global object itself and global
/// variable access keeps V8's fast path. (The global object's own prototype is immutable.)
pub fn install_named_properties_object(
    scope: &mut v8::PinScope<'_, '_>,
    iface: u16,
    global: v8::Local<v8::Object>,
    handle: Handle,
) -> Option<()> {
    let t = {
        let st = state(scope);
        let g = st
            .named_properties_templates
            .borrow()
            .get(&iface)
            .cloned()?;
        v8::Local::new(scope, g)
    };
    let named = t.instance_template(scope).new_instance(scope)?;
    init_wrapper(named, handle, iface);
    let proto = global.get_prototype(scope)?;
    let proto = v8::Local::<v8::Object>::try_from(proto).ok()?;
    let parent = proto.get_prototype(scope)?;
    named.set_prototype(scope, parent)?;
    proto.set_prototype(scope, named.into())?;
    Some(())
}
