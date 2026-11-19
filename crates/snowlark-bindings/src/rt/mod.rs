// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The handwritten runtime the generated bindings call: handles and wrappers, the per-isolate binding
//! state, Web IDL value conversions in both directions, exceptions, and the hooks the engine implements.
//!
//! Nothing here defines a JS-visible property by itself; every property comes from `generated/`.

mod async_iter;
mod convert;
mod cross_origin;
mod fast;
mod gc;
mod guard;
mod host;
mod iter;
mod light;
mod observable;
pub mod prof;
mod reflect;
mod template;
mod tojs;
mod wrap;
mod wrapper_table;

pub use async_iter::*;
pub use convert::*;
pub use cross_origin::*;
pub use fast::*;
pub use gc::*;
pub use guard::*;
pub use host::*;
pub use iter::*;
pub use light::*;
pub use observable::*;
pub use reflect::*;
pub use template::*;
pub use tojs::*;
pub use wrap::*;
use wrapper_table::WrapperTable;

use std::any::Any;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// A string the engine returns: borrowed from its own storage when it can (directly, or through the
/// `RefCell` guard of the state it lives in), owned otherwise. The generated code copies it into a V8
/// string at once and drops it, so a guard never outlives the call that returned it.
pub enum DomStr<'a> {
    Borrowed(&'a str),
    Owned(String),
    Guard(std::cell::Ref<'a, str>),
}

impl<'a> DomStr<'a> {
    /// The string as an owned `String` (no copy for [`DomStr::Owned`]).
    pub fn into_owned(self) -> String {
        match self {
            DomStr::Owned(s) => s,
            other => other.to_string(),
        }
    }
}

impl std::ops::Deref for DomStr<'_> {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        match self {
            DomStr::Borrowed(s) => s,
            DomStr::Owned(s) => s,
            DomStr::Guard(s) => s,
        }
    }
}

impl AsRef<str> for DomStr<'_> {
    fn as_ref(&self) -> &str {
        self
    }
}

impl std::fmt::Debug for DomStr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl std::fmt::Display for DomStr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

impl PartialEq for DomStr<'_> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl PartialEq<str> for DomStr<'_> {
    fn eq(&self, other: &str) -> bool {
        &**self == other
    }
}

impl PartialEq<&str> for DomStr<'_> {
    fn eq(&self, other: &&str) -> bool {
        &**self == *other
    }
}

impl<'a> From<&'a str> for DomStr<'a> {
    fn from(s: &'a str) -> Self {
        DomStr::Borrowed(s)
    }
}

impl From<String> for DomStr<'_> {
    fn from(s: String) -> Self {
        DomStr::Owned(s)
    }
}

impl<'a> From<Cow<'a, str>> for DomStr<'a> {
    fn from(s: Cow<'a, str>) -> Self {
        match s {
            Cow::Borrowed(b) => DomStr::Borrowed(b),
            Cow::Owned(o) => DomStr::Owned(o),
        }
    }
}

impl<'a> From<std::cell::Ref<'a, str>> for DomStr<'a> {
    fn from(s: std::cell::Ref<'a, str>) -> Self {
        DomStr::Guard(s)
    }
}

/// The engine's reference to the object behind a wrapper: a `NodeId` for nodes, an index for other
/// objects. The engine defines the encoding; the bindings only store it (in two of the wrapper's
/// internal fields) and key the wrapper cache by it, so it must be unique across all kinds of objects.
/// Values use the low 62 bits: a field holds a 31-bit Smi (`snowlark_js::internal_field`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Handle(u64);

impl Handle {
    /// The largest value a handle can carry.
    pub const MAX: u64 = (1 << 62) - 1;

    /// Makes a handle. Panics when `value` exceeds [`Handle::MAX`].
    pub const fn new(value: u64) -> Handle {
        assert!(value <= Self::MAX, "handle out of range");
        Handle(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The names of the DOMException error names table (Web IDL §2.8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExceptionName {
    IndexSizeError,
    HierarchyRequestError,
    WrongDocumentError,
    InvalidCharacterError,
    NoModificationAllowedError,
    NotFoundError,
    NotSupportedError,
    InUseAttributeError,
    InvalidStateError,
    SyntaxError,
    InvalidModificationError,
    NamespaceError,
    InvalidAccessError,
    TypeMismatchError,
    SecurityError,
    NetworkError,
    AbortError,
    URLMismatchError,
    QuotaExceededError,
    TimeoutError,
    InvalidNodeTypeError,
    DataCloneError,
    EncodingError,
    NotReadableError,
    UnknownError,
    ConstraintError,
    DataError,
    TransactionInactiveError,
    ReadOnlyError,
    VersionError,
    OperationError,
    NotAllowedError,
    OptInRequiredError,
}

impl ExceptionName {
    pub fn as_str(self) -> &'static str {
        use ExceptionName::*;
        match self {
            IndexSizeError => "IndexSizeError",
            HierarchyRequestError => "HierarchyRequestError",
            WrongDocumentError => "WrongDocumentError",
            InvalidCharacterError => "InvalidCharacterError",
            NoModificationAllowedError => "NoModificationAllowedError",
            NotFoundError => "NotFoundError",
            NotSupportedError => "NotSupportedError",
            InUseAttributeError => "InUseAttributeError",
            InvalidStateError => "InvalidStateError",
            SyntaxError => "SyntaxError",
            InvalidModificationError => "InvalidModificationError",
            NamespaceError => "NamespaceError",
            InvalidAccessError => "InvalidAccessError",
            TypeMismatchError => "TypeMismatchError",
            SecurityError => "SecurityError",
            NetworkError => "NetworkError",
            AbortError => "AbortError",
            URLMismatchError => "URLMismatchError",
            QuotaExceededError => "QuotaExceededError",
            TimeoutError => "TimeoutError",
            InvalidNodeTypeError => "InvalidNodeTypeError",
            DataCloneError => "DataCloneError",
            EncodingError => "EncodingError",
            NotReadableError => "NotReadableError",
            UnknownError => "UnknownError",
            ConstraintError => "ConstraintError",
            DataError => "DataError",
            TransactionInactiveError => "TransactionInactiveError",
            ReadOnlyError => "ReadOnlyError",
            VersionError => "VersionError",
            OperationError => "OperationError",
            NotAllowedError => "NotAllowedError",
            OptInRequiredError => "OptInRequiredError",
        }
    }

    /// The legacy `code` of the name, or 0 (Web IDL §2.8.1, "Legacy code name and value").
    pub fn legacy_code(name: &str) -> u16 {
        match name {
            "IndexSizeError" => 1,
            "HierarchyRequestError" => 3,
            "WrongDocumentError" => 4,
            "InvalidCharacterError" => 5,
            "NoModificationAllowedError" => 7,
            "NotFoundError" => 8,
            "NotSupportedError" => 9,
            "InUseAttributeError" => 10,
            "InvalidStateError" => 11,
            "SyntaxError" => 12,
            "InvalidModificationError" => 13,
            "NamespaceError" => 14,
            "InvalidAccessError" => 15,
            "TypeMismatchError" => 17,
            "SecurityError" => 18,
            "NetworkError" => 19,
            "AbortError" => 20,
            "URLMismatchError" => 21,
            "QuotaExceededError" => 22,
            "TimeoutError" => 23,
            "InvalidNodeTypeError" => 24,
            "DataCloneError" => 25,
            _ => 0,
        }
    }
}

/// An exception the engine raises from an operation or setter.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// A `TypeError` (Web IDL "simple exception").
    Type(Cow<'static, str>),
    /// A `RangeError`.
    Range(Cow<'static, str>),
    /// A `DOMException` with the given name.
    Dom(ExceptionName, Cow<'static, str>),
    /// A JS exception is already pending (a callback threw); it propagates as is.
    Pending,
}

impl Error {
    pub fn dom(name: ExceptionName, message: impl Into<Cow<'static, str>>) -> Error {
        Error::Dom(name, message.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The execution context an operation runs in: what the engine needs to call back into script
/// (event listeners, callbacks) or to hand values to it. Wraps the V8 scope of the call.
pub struct Cx<'a, 's, 'i> {
    pub(crate) scope: &'a mut v8::PinScope<'s, 'i>,
}

impl<'a, 's, 'i> Cx<'a, 's, 'i> {
    pub fn new(scope: &'a mut v8::PinScope<'s, 'i>) -> Self {
        Cx { scope }
    }

    /// The V8 scope, for code that drives V8 directly (snowlark-js, the page crate's script runner).
    pub fn scope(&mut self) -> &mut v8::PinScope<'s, 'i> {
        self.scope
    }
}

/// A JS value the engine holds (Web IDL `any`).
#[derive(Clone)]
pub struct JsValue(pub(crate) v8::Global<v8::Value>);

/// A JS object the engine holds (Web IDL `object`, buffer sources, `Promise` arguments).
#[derive(Clone)]
pub struct JsObject(pub(crate) v8::Global<v8::Object>);

/// A pending promise the engine settles later, returned by Promise-typed operations.
#[derive(Clone)]
pub struct JsPromise(pub(crate) v8::Global<v8::PromiseResolver>);

impl JsValue {
    pub fn global(&self) -> &v8::Global<v8::Value> {
        &self.0
    }

    /// Holds a value the engine got from V8 directly.
    pub fn from_global(g: v8::Global<v8::Value>) -> JsValue {
        JsValue(g)
    }

    /// Holds a local value.
    pub fn new(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> JsValue {
        JsValue(v8::Global::new(scope, v))
    }

    /// Whether the value is `undefined`.
    pub fn is_undefined(&self, cx: &mut Cx) -> bool {
        v8::Local::new(cx.scope, &self.0).is_undefined()
    }
}

impl JsObject {
    pub fn global(&self) -> &v8::Global<v8::Object> {
        &self.0
    }

    /// Holds an object the engine got from V8 directly.
    pub fn from_global(g: v8::Global<v8::Object>) -> JsObject {
        JsObject(g)
    }

    /// Holds a local object.
    pub fn new(scope: &v8::PinScope<'_, '_>, o: v8::Local<v8::Object>) -> JsObject {
        JsObject(v8::Global::new(scope, o))
    }
}

impl JsPromise {
    /// The promise's resolver.
    pub fn resolver(&self) -> &v8::Global<v8::PromiseResolver> {
        &self.0
    }

    /// Holds a resolver the engine kept itself.
    pub fn from_resolver(g: v8::Global<v8::PromiseResolver>) -> JsPromise {
        JsPromise(g)
    }

    /// Creates a pending promise.
    pub fn new(cx: &mut Cx) -> Option<JsPromise> {
        let r = v8::PromiseResolver::new(cx.scope)?;
        Some(JsPromise(v8::Global::new(cx.scope, r)))
    }

    /// Resolves with a value converted by the generated code's [`ToJs`].
    pub fn resolve<T: ToJs>(&self, cx: &mut Cx, value: T) {
        let r = v8::Local::new(cx.scope, &self.0);
        if let Some(v) = value.to_js(cx.scope) {
            r.resolve(cx.scope, v);
        }
    }

    /// Rejects with a JS value (an exception script threw, an abort reason).
    pub fn reject_value(&self, cx: &mut Cx, value: &JsValue) {
        let r = v8::Local::new(cx.scope, &self.0);
        let v = v8::Local::new(cx.scope, value.global());
        r.reject(cx.scope, v);
    }

    /// The promise object itself.
    pub fn promise(&self, cx: &mut Cx) -> JsValue {
        let r = v8::Local::new(cx.scope, &self.0);
        let p: v8::Local<v8::Value> = r.get_promise(cx.scope).into();
        JsValue(v8::Global::new(cx.scope, p))
    }

    /// Marks the promise as handled: its rejection is not reported as unhandled (the Streams
    /// Standard's "set promise.[[PromiseIsHandled]] to true").
    pub fn mark_handled(&self, cx: &mut Cx) {
        let r = v8::Local::new(cx.scope, &self.0);
        r.get_promise(cx.scope).mark_as_handled();
    }

    /// Rejects with the exception an [`Error`] describes.
    pub fn reject(&self, cx: &mut Cx, error: Error) {
        let r = v8::Local::new(cx.scope, &self.0);
        if let Some(e) = error_value(cx.scope, error) {
            r.reject(cx.scope, e);
        }
    }
}

/// Signature of the generated function that makes a DOMException object.
pub type DomExceptionFn =
    for<'s, 'i> fn(&mut v8::PinScope<'s, 'i>, &str, &str) -> Option<v8::Local<'s, v8::Value>>;

/// The engine's `Host::realm_of`, type-erased: the handle of the realm's global object.
pub type RealmOfFn = fn(&dyn Any, Handle) -> Option<u64>;

/// The bindings' per-isolate state, kept in an isolate slot.
pub struct State {
    pub(crate) engine: Rc<dyn Any>,
    pub(crate) interface_of: fn(&dyn Any, Handle) -> u16,
    pub(crate) dom_exception: DomExceptionFn,
    /// The interface templates by prototype id.
    pub(crate) templates: RefCell<Vec<Option<v8::Global<v8::FunctionTemplate>>>>,
    /// Wrappers by handle, strong or weak ([`gc`]).
    pub(crate) wrappers: RefCell<WrapperTable>,
    /// Wrapper lifetime bookkeeping.
    pub(crate) gc: Gc,
    /// The iterator prototype templates of pair-iterable interfaces, by interface prototype id.
    pub(crate) iterator_templates: RefCell<HashMap<u16, v8::Global<v8::FunctionTemplate>>>,
    /// The asynchronous iterator prototype object templates, by interface.
    pub(crate) async_iterator_templates: RefCell<HashMap<u16, v8::Global<v8::FunctionTemplate>>>,
    pub(crate) options: InstallOptions,
    /// The engine's `Host::realm_of`, once [`register_realm`] installed it.
    pub(crate) realm_of: Cell<Option<RealmOfFn>>,
    /// The context of each registered realm, by its global's handle. Weak: nothing here keeps a realm.
    pub(crate) realm_contexts: RefCell<HashMap<u64, v8::Weak<v8::Context>>>,
    /// How many realms were registered.
    pub(crate) realms: Cell<usize>,
    /// The templates of `[Global]` interfaces' named properties objects, by prototype id.
    pub(crate) named_properties_templates: RefCell<HashMap<u16, v8::Global<v8::FunctionTemplate>>>,
    /// `CrossOriginProperties(O)`, by the prototype id of the interface.
    pub(crate) cross_origin: RefCell<HashMap<u16, Rc<Vec<CrossOriginProperty>>>>,
    /// The engine's side of cross-origin access, once a template took the access check.
    pub(crate) cross_origin_host: Cell<Option<CrossOriginHost>>,
    /// The largest handle of a Window that a navigation replaced ([`note_replaced_window`]): every
    /// wrapper lookup of a larger handle, so of every node, skips the question.
    pub(crate) max_replaced: Cell<u64>,
    /// The template of realms' anchors (`set_realm_global`).
    pub(crate) realm_anchor_template: RefCell<Option<v8::Global<v8::ObjectTemplate>>>,
    /// The addresses of `null`, `true` and `false` ([`set_state`]), for results set in place ([`Call`]).
    pub(crate) roots: Cell<Roots>,
    /// The isolate's strings of the property names generated code uses ([`Name`]), by name index.
    pub(crate) names: RefCell<Vec<Option<v8::Global<v8::String>>>>,
    /// The isolate's private symbols of `[SameObject]` attributes ([`Name`]), by name index.
    pub(crate) privates: RefCell<Vec<Option<v8::Global<v8::Private>>>>,
}

/// Choices made when the bindings are installed.
#[derive(Clone, Copy, Debug)]
pub struct InstallOptions {
    /// Attach Fast API C functions to eligible getters (on by default; off to measure the slow path).
    pub fast_api: bool,
    /// Whether the realm is a secure context: `[SecureContext]` members are installed only then.
    pub secure_context: bool,
    /// Whether the isolate's realms are dedicated workers' rather than Windows: members whose
    /// `[Exposed]` is narrower than their interface's are installed only where they are exposed.
    pub worker: bool,
    /// The browser family the user agent claims, when members gated by it (`[UaGate=...]`: `oscpu`
    /// for Gecko, `userAgentData` for Chromium) follow it; `None` installs them all.
    pub ua_family: Option<UaFamily>,
    /// Whether the user agent has a touch device: the legacy touch event APIs (`[TouchDevice]`:
    /// `ontouchstart` and the other `ontouch*` handlers) are installed only then (Touch Events
    /// "conditionally exposing legacy touch event APIs").
    pub touch_events: bool,
}

/// A browser family a user agent can claim ([`InstallOptions::ua_family`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UaFamily {
    Gecko,
    Chromium,
    WebKit,
    Other,
}

impl UaFamily {
    fn name(self) -> &'static str {
        match self {
            UaFamily::Gecko => "Gecko",
            UaFamily::Chromium => "Chromium",
            UaFamily::WebKit => "WebKit",
            UaFamily::Other => "Other",
        }
    }
}

impl Default for InstallOptions {
    fn default() -> Self {
        InstallOptions {
            fast_api: true,
            secure_context: true,
            worker: false,
            ua_family: None,
            touch_events: false,
        }
    }
}

impl InstallOptions {
    /// Whether a member gated to the browser family `family` is installed: always, unless the user
    /// agent's family is set and another.
    pub fn ua_allows(&self, family: &str) -> bool {
        self.ua_family.is_none_or(|f| f.name() == family)
    }

    /// Whether a member exposed in the global names `names` (`*` for all) is exposed in the
    /// isolate's kind of realm.
    pub fn exposes(&self, names: &[&str]) -> bool {
        names.iter().any(|n| {
            *n == "*"
                || if self.worker {
                    *n == "Worker" || *n == "DedicatedWorker"
                } else {
                    *n == "Window"
                }
        })
    }
}

impl State {
    pub fn new<D: 'static>(
        engine: Rc<D>,
        interface_of: fn(&dyn Any, Handle) -> u16,
        dom_exception: DomExceptionFn,
        prototype_count: usize,
        options: InstallOptions,
    ) -> State {
        State {
            engine,
            interface_of,
            dom_exception,
            templates: RefCell::new(vec![None; prototype_count + 1]),
            wrappers: RefCell::new(WrapperTable::default()),
            gc: Gc::default(),
            iterator_templates: RefCell::new(HashMap::new()),
            async_iterator_templates: RefCell::new(HashMap::new()),
            options,
            realm_of: Cell::new(None),
            realm_contexts: RefCell::new(HashMap::new()),
            realms: Cell::new(0),
            named_properties_templates: RefCell::new(HashMap::new()),
            cross_origin: RefCell::new(HashMap::new()),
            cross_origin_host: Cell::new(None),
            max_replaced: Cell::new(0),
            realm_anchor_template: RefCell::new(None),
            roots: Cell::new(Roots::default()),
            names: RefCell::new(Vec::new()),
            privates: RefCell::new(Vec::new()),
        }
    }

    /// The engine, as the type the bindings were installed for.
    #[inline(always)]
    pub fn engine<D: 'static>(&self) -> &D {
        self.engine
            .downcast_ref::<D>()
            .expect("bindings installed for another engine type")
    }

    pub fn options(&self) -> InstallOptions {
        self.options
    }

    pub fn set_template(
        &self,
        id: u16,
        scope: &v8::PinScope<'_, '_, ()>,
        t: v8::Local<v8::FunctionTemplate>,
    ) {
        self.templates.borrow_mut()[id as usize] = Some(v8::Global::new(scope, t));
    }

    pub fn template<'s>(
        &self,
        scope: &v8::PinScope<'s, '_, ()>,
        id: u16,
    ) -> Option<v8::Local<'s, v8::FunctionTemplate>> {
        self.templates.borrow()[id as usize]
            .as_ref()
            .map(|g| v8::Local::new(scope, g))
    }
}

/// Hashes handles for the wrapper cache: a handle is already a unique integer (a node's index and
/// generation, or an object slot), so one multiply spreads it; SipHash would cost more than the lookup.
#[derive(Default, Clone, Copy)]
pub struct HandleHasher(u64);

impl std::hash::Hasher for HandleHasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    #[inline(always)]
    fn write_u64(&mut self, v: u64) {
        // Fold the generation (high half) into the index (low half), then spread.
        self.0 = (v ^ (v >> 32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

/// The [`std::hash::BuildHasher`] of [`HandleHasher`].
pub type HandleHash = std::hash::BuildHasherDefault<HandleHasher>;

/// The binding state of the isolate a scope belongs to. Panics if the bindings are not installed.
#[inline(always)]
pub fn state(scope: &v8::PinScope<'_, '_>) -> Rc<State> {
    scope
        .get_slot::<Rc<State>>()
        .expect("snowlark bindings are not installed in this isolate")
        .clone()
}

/// The binding state of the isolate a scope belongs to, if the bindings are installed there.
pub fn state_of(scope: &v8::PinScope<'_, '_, ()>) -> Option<Rc<State>> {
    scope.get_slot::<Rc<State>>().cloned()
}

/// Installs the binding state in the isolate of `scope`.
pub fn set_state(scope: &mut v8::PinScope<'_, '_>, state: State) -> Rc<State> {
    use snowlark_js::raw_callback::{address_of, slot_of, verify_embedder_data};
    // (read-only roots: they never move, so their addresses hold for the isolate's life, as the ones
    // V8's `ReturnValue::SetNull` reads from the isolate's root table do)
    state.roots.set(Roots {
        null: address_of(v8::null(scope)),
        true_: address_of(v8::Boolean::new(scope, true)),
        false_: address_of(v8::Boolean::new(scope, false)),
        undefined: slot_of(v8::undefined(scope)),
    });
    let rc = Rc::new(state);
    scope.set_slot(rc.clone());
    // The state's address in the isolate's data slot, where callbacks that read their arguments in
    // place find it ([`Call`], as Blink's `V8PerIsolateData::From`); the slot map above keeps it.
    verify_embedder_data(scope, STATE_DATA_SLOT);
    // The wrappers' internal fields are read inline (`rt::wrap`, `rt::fast`): check that read here.
    snowlark_js::internal_field::verify(scope);
    scope.set_data(STATE_DATA_SLOT, Rc::as_ptr(&rc) as *mut std::ffi::c_void);
    install_access_hooks(scope);
    rc
}

/// The body of a stubbed trait method (feature `stubs`, for test mocks).
#[cold]
pub fn stub<T>(what: &str) -> T {
    panic!("not implemented: {what}")
}

/// `null` as a [`JsValue`] (default values of `any`).
pub fn null_value(scope: &v8::PinScope<'_, '_>) -> JsValue {
    let v: v8::Local<v8::Value> = v8::null(scope).into();
    JsValue(v8::Global::new(scope, v))
}

/// `undefined` as a [`JsValue`].
pub fn undefined_value(scope: &v8::PinScope<'_, '_>) -> JsValue {
    let v: v8::Local<v8::Value> = v8::undefined(scope).into();
    JsValue(v8::Global::new(scope, v))
}

/// The `this` value of a callback invocation (`undefined` when none is given).
pub fn callback_this<'s>(
    scope: &v8::PinScope<'s, '_>,
    this_arg: Option<&dyn ToJs>,
) -> v8::Local<'s, v8::Value> {
    this_arg
        .and_then(|t| t.to_js(scope))
        .unwrap_or_else(|| v8::undefined(scope).into())
}

/// The function and `this` to call for a callback interface operation (Web IDL §3.11): the object with
/// the callback this value when it is callable, else its `operation` property (which must be callable)
/// with the object.
pub fn callback_interface_method<'s>(
    scope: &v8::PinScope<'s, '_>,
    obj: v8::Local<'s, v8::Object>,
    operation: &Name,
    this_arg: Option<&dyn ToJs>,
) -> Result<(v8::Local<'s, v8::Function>, v8::Local<'s, v8::Value>)> {
    if let Ok(f) = v8::Local::<v8::Function>::try_from(obj) {
        return Ok((f, callback_this(scope, this_arg)));
    }
    let m = obj
        .get(scope, cached_name(scope, operation).into())
        .ok_or(Error::Pending)?;
    match v8::Local::<v8::Function>::try_from(m) {
        Ok(f) => Ok((f, obj.into())),
        Err(_) => Err(Error::Type(
            format!("The '{}' property is not callable", operation.text()).into(),
        )),
    }
}

/// Throws the "Illegal invocation" TypeError.
pub fn throw_illegal_invocation(scope: &v8::PinScope<'_, '_>) {
    throw_type_error(scope, "Illegal invocation");
}

/// A promise rejected with an "Illegal invocation" TypeError (a promise-returning member called on
/// an object that is not one of its interface's).
pub fn rejected_illegal_invocation<'s>(
    scope: &v8::PinScope<'s, '_>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let message = v8::String::new(scope, "Illegal invocation")?;
    let e = v8::Exception::type_error(scope, message);
    let r = v8::PromiseResolver::new(scope)?;
    r.reject(scope, e);
    Some(r.get_promise(scope))
}
