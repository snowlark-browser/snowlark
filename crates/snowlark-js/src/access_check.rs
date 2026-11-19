// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! V8's access checks: what stands between a realm and the `WindowProxy` and `Location` objects of
//! another origin (HTML §7.2.1 "Security", §7.2.3 "The WindowProxy exotic object").
//!
//! An object template with an access check gives every instance V8's own guard: any operation on the
//! object from a context other than its own asks the access check first. V8 applies it to every path
//! it has (property lookups, descriptors, key collection, prototype walks, `Function.prototype.caller`,
//! inline caches), so a path nobody thought of is refused rather than open. A refused access goes to
//! interceptors, which is where the cross-origin properties live, or to the failed access check
//! callback, which throws. A `JSGlobalProxy` compares security tokens first, so same-origin access
//! never reaches the callback and costs what it always did.
//!
//! The `v8` crate (152) binds none of this, but the prebuilt V8 library it links contains all of it,
//! so the few entry points are declared here by their MSVC x64 names, as [`crate::streaming`] does
//! (`docs/v8-upgrade.md`). The ABI facts that matter, read from `v8-template.h`, `v8-isolate.h`
//! and `v8-local-handle.h`:
//!
//! - `Local<T>` is a trivially copyable class holding one pointer: by value it travels in a register,
//!   as a pointer does. A `Local` return value (the class has constructors) comes back through a hidden
//!   pointer, passed after `this`.
//! - `NamedPropertyHandlerConfiguration` is seven function pointers, a `Local<Value>` and a
//!   `PropertyHandlerFlags`; `IndexedPropertyHandlerConfiguration` has two more function pointers after
//!   the seven.
//! - `Context::BackupIncumbentScope` (`v8-context.h`) is a `Local<Context>`, a `uintptr_t` and a pointer,
//!   and V8 keeps its address while it lives: the isolate's list of them is a stack through `prev_`.
//!
//! Moved symbols stop the link. A changed struct does not, so [`VERIFIED_V8`] stops every isolate at
//! [`install`] until someone re-verifies (`docs/v8-upgrade.md`), and the test below drives every
//! handler slot through V8: a slot that moved calls the wrong function.
//!
//! Only Windows x64 has these declarations. Elsewhere V8's default stands: an access across security
//! tokens to a global proxy is refused outright, with no cross-origin properties, and Location has no
//! guard of its own (it cannot be reached: the Window it hangs off refuses).

use std::ffi::c_void;

/// The V8 version these declarations were checked against. An access check that silently misreads
/// its handlers is a hole, so anything else refuses to start.
pub const VERIFIED_V8: &str = "15.8.62.23";

/// The access check: whether code in the context may touch the object.
pub type AllowFn = for<'s, 'i> fn(
    &mut v8::PinScope<'s, 'i>,
    v8::Local<'s, v8::Context>,
    v8::Local<'s, v8::Object>,
) -> bool;

/// A refused access with no interceptor to take it: throws.
pub type FailedFn = for<'s, 'i> fn(&mut v8::PinScope<'s, 'i>, v8::Local<'s, v8::Object>);

/// What the embedder decides, kept in an isolate slot. An isolate without them refuses everything.
#[derive(Clone, Copy)]
pub struct Hooks {
    pub allow: AllowFn,
    pub failed: FailedFn,
}

/// The interceptors of refused named accesses.
pub struct NamedHandlers {
    pub getter: v8::NamedPropertyGetterCallback,
    pub setter: v8::NamedPropertySetterCallback,
    pub query: v8::NamedPropertyQueryCallback,
    pub enumerator: v8::NamedPropertyEnumeratorCallback,
    pub definer: v8::NamedPropertyDefinerCallback,
    pub descriptor: v8::NamedPropertyDescriptorCallback,
}

/// The interceptors of refused indexed accesses.
pub struct IndexedHandlers {
    pub getter: v8::IndexedPropertyGetterCallback,
    pub setter: v8::IndexedPropertySetterCallback,
    pub query: v8::IndexedPropertyQueryCallback,
    pub enumerator: v8::IndexedPropertyEnumeratorCallback,
    pub definer: v8::IndexedPropertyDefinerCallback,
    pub descriptor: v8::IndexedPropertyDescriptorCallback,
}

#[cfg(all(windows, target_arch = "x86_64"))]
mod ffi {
    use std::ffi::c_void;

    /// `v8::NamedPropertyHandlerConfiguration`.
    #[repr(C)]
    pub struct NamedConfiguration {
        pub getter: Option<v8::NamedPropertyGetterCallback>,
        pub setter: Option<v8::NamedPropertySetterCallback>,
        pub query: Option<v8::NamedPropertyQueryCallback>,
        pub deleter: Option<v8::NamedPropertyDeleterCallback>,
        pub enumerator: Option<v8::NamedPropertyEnumeratorCallback>,
        pub definer: Option<v8::NamedPropertyDefinerCallback>,
        pub descriptor: Option<v8::NamedPropertyDescriptorCallback>,
        pub data: *const v8::Value,
        pub flags: i32,
    }

    // Seven function pointers, the data handle and the flags, padded to a word.
    const _: () = assert!(std::mem::size_of::<NamedConfiguration>() == 9 * 8);

    /// `v8::IndexedPropertyHandlerConfiguration`.
    #[repr(C)]
    pub struct IndexedConfiguration {
        pub getter: Option<v8::IndexedPropertyGetterCallback>,
        pub setter: Option<v8::IndexedPropertySetterCallback>,
        pub query: Option<v8::IndexedPropertyQueryCallback>,
        pub deleter: Option<v8::IndexedPropertyDeleterCallback>,
        pub enumerator: Option<v8::IndexedPropertyEnumeratorCallback>,
        pub definer: Option<v8::IndexedPropertyDefinerCallback>,
        pub descriptor: Option<v8::IndexedPropertyDescriptorCallback>,
        pub index_of: *const c_void,
        pub iterable_to_list: *const c_void,
        pub data: *const v8::Value,
        pub flags: i32,
    }

    // SAFETY (all declarations): these are the MSVC x64 names of the V8 functions in the static
    // library the `v8` crate links; the Rust signatures follow the Microsoft x64 convention for them
    // (see the module documentation).
    extern "C" {
        /// `void v8::ObjectTemplate::SetAccessCheckCallbackAndHandler(AccessCheckCallback, const
        /// NamedPropertyHandlerConfiguration&, const IndexedPropertyHandlerConfiguration&,
        /// Local<Value>)`.
        #[link_name = "?SetAccessCheckCallbackAndHandler@ObjectTemplate@v8@@QEAAXP6A_NV?$Local@VContext@v8@@@2@V?$Local@VObject@v8@@@2@V?$Local@VValue@v8@@@2@@ZAEBUNamedPropertyHandlerConfiguration@2@AEBUIndexedPropertyHandlerConfiguration@2@2@Z"]
        pub fn set_access_check_callback_and_handler(
            this: *const v8::ObjectTemplate,
            callback: *const c_void,
            named: *const NamedConfiguration,
            indexed: *const IndexedConfiguration,
            data: *const v8::Value,
        );

        /// `void v8::Isolate::SetFailedAccessCheckCallbackFunction(FailedAccessCheckCallback)`.
        #[link_name = "?SetFailedAccessCheckCallbackFunction@Isolate@v8@@QEAAXP6AXV?$Local@VObject@v8@@@2@W4AccessType@2@V?$Local@VValue@v8@@@2@@Z@Z"]
        pub fn set_failed_access_check_callback_function(
            this: *mut c_void,
            callback: *const c_void,
        );

        /// `static Isolate* v8::Isolate::GetCurrent()`.
        #[link_name = "?GetCurrent@Isolate@v8@@SAPEAV12@XZ"]
        pub fn get_current_isolate() -> *mut c_void;

        /// `Local<Context> v8::Isolate::GetCurrentContext()`: the `Local` comes back through `ret`.
        #[link_name = "?GetCurrentContext@Isolate@v8@@QEAA?AV?$Local@VContext@v8@@@2@XZ"]
        pub fn get_current_context(
            this: *mut c_void,
            ret: *mut *const c_void,
        ) -> *mut *const c_void;

        /// `Local<Context> v8::Isolate::GetIncumbentContext()`: the `Local` comes back through `ret`.
        #[link_name = "?GetIncumbentContext@Isolate@v8@@QEAA?AV?$Local@VContext@v8@@@2@XZ"]
        pub fn get_incumbent_context(
            this: *mut c_void,
            ret: *mut *const c_void,
        ) -> *mut *const c_void;

        /// `Context::BackupIncumbentScope::BackupIncumbentScope(Local<Context>)`.
        #[link_name = "??0BackupIncumbentScope@Context@v8@@QEAA@V?$Local@VContext@v8@@@2@@Z"]
        pub fn backup_incumbent_scope_new(
            this: *mut BackupIncumbentScope,
            context: *const c_void,
        ) -> *mut BackupIncumbentScope;

        /// `Context::BackupIncumbentScope::~BackupIncumbentScope()`.
        #[link_name = "??1BackupIncumbentScope@Context@v8@@QEAA@XZ"]
        pub fn backup_incumbent_scope_drop(this: *mut BackupIncumbentScope);
    }

    /// `v8::Context::BackupIncumbentScope`.
    #[repr(C)]
    pub struct BackupIncumbentScope {
        pub context: *const c_void,
        pub js_stack_comparable_address: usize,
        pub prev: *const c_void,
    }
}

/// `bool (*)(Local<Context> accessing_context, Local<Object> accessed_object, Local<Value> data)`.
///
/// Fails closed: without hooks, or after a panic in them, the access is refused.
#[cfg(all(windows, target_arch = "x86_64"))]
#[allow(improper_ctypes_definitions)]
unsafe extern "C" fn access_check<'a>(
    context: v8::Local<'a, v8::Context>,
    object: v8::Local<'a, v8::Object>,
    _data: *const v8::Value,
) -> bool {
    v8::callback_scope!(unsafe scope, context);
    let Some(hooks) = scope.get_slot::<Hooks>().copied() else {
        return false;
    };
    crate::guard::catch(|| (hooks.allow)(scope, context, object)).unwrap_or(false)
}

/// `void (*)(Local<Object> target, AccessType type, Local<Value> data)`. V8 requires an exception
/// from it; a panic terminates the script instead, which also unwinds out of the access.
#[cfg(all(windows, target_arch = "x86_64"))]
#[allow(improper_ctypes_definitions)]
unsafe extern "C" fn failed_access_check<'a>(
    target: v8::Local<'a, v8::Object>,
    _access: i32,
    _data: *const v8::Value,
) {
    // A scope on the isolate's current context, the one that was refused: the exception is its.
    let mut out: *const c_void = std::ptr::null();
    // SAFETY: V8 calls this on the isolate's thread, inside a handle scope of its own, with a context
    // entered; `out` receives the `Local`'s one pointer.
    let context = unsafe {
        ffi::get_current_context(ffi::get_current_isolate(), &mut out);
        match std::ptr::NonNull::new(out as *mut v8::Context) {
            Some(c) => {
                std::mem::transmute::<std::ptr::NonNull<v8::Context>, v8::Local<'a, v8::Context>>(c)
            }
            None => return,
        }
    };
    v8::callback_scope!(unsafe scope, context);
    if let Some(hooks) = scope.get_slot::<Hooks>().copied() {
        if crate::guard::catch(|| (hooks.failed)(scope, target)).is_some() {
            return;
        }
    }
    scope.terminate_execution();
}

/// Sets the isolate's decisions, and the callback V8 calls for a refused access nothing intercepts.
pub fn install(isolate: &mut v8::Isolate, hooks: Hooks) {
    let version = v8::V8::get_version();
    assert!(
        version.starts_with(VERIFIED_V8),
        "V8 is {version}, access_check.rs was verified against {VERIFIED_V8}: see docs/v8-upgrade.md"
    );
    isolate.set_slot(hooks);
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        let callback: unsafe extern "C" fn(v8::Local<'static, v8::Object>, i32, *const v8::Value) =
            failed_access_check;
        // SAFETY: the isolate is entered on this thread. The raw isolate pointer is the C++
        // `v8::Isolate*`; the callback has the C++ callback's signature (see the module documentation).
        unsafe {
            let raw: v8::UnsafeRawIsolatePtr = isolate.as_raw_isolate_ptr();
            let isolate: *mut c_void = std::mem::transmute(raw);
            ffi::set_failed_access_check_callback_function(isolate, callback as *const c_void);
        }
    }
}

/// Makes instances of `template` (and, for a global template, the global proxy of contexts made from
/// it) access checked. The template must not have been instantiated yet.
#[cfg_attr(not(all(windows, target_arch = "x86_64")), allow(unused_variables))]
pub fn guard_template(
    template: v8::Local<'_, v8::ObjectTemplate>,
    named: NamedHandlers,
    indexed: IndexedHandlers,
) {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        let n = ffi::NamedConfiguration {
            getter: Some(named.getter),
            setter: Some(named.setter),
            query: Some(named.query),
            // (V8 reports a refused delete to the failed access check callback)
            deleter: None,
            enumerator: Some(named.enumerator),
            definer: Some(named.definer),
            descriptor: Some(named.descriptor),
            data: std::ptr::null(),
            flags: 0,
        };
        let i = ffi::IndexedConfiguration {
            getter: Some(indexed.getter),
            setter: Some(indexed.setter),
            query: Some(indexed.query),
            deleter: None,
            enumerator: Some(indexed.enumerator),
            definer: Some(indexed.definer),
            descriptor: Some(indexed.descriptor),
            index_of: std::ptr::null(),
            iterable_to_list: std::ptr::null(),
            data: std::ptr::null(),
            flags: 0,
        };
        let check: unsafe extern "C" fn(
            v8::Local<'static, v8::Context>,
            v8::Local<'static, v8::Object>,
            *const v8::Value,
        ) -> bool = access_check;
        // SAFETY: `template` is a live handle (the C++ `this` is the handle's slot, as the `v8` crate
        // passes it); the configurations are read during the call; the check has the C++ callback's
        // signature; a null `Local<Value>` is the empty handle.
        unsafe {
            ffi::set_access_check_callback_and_handler(
                &*template,
                check as *const c_void,
                &n,
                &i,
                std::ptr::null(),
            );
        }
    }
}

/// HTML's incumbent realm: V8's incumbent context (the context of the topmost script on the stack,
/// not of the API function being called, which is the current one).
pub fn incumbent_context<'s>(scope: &v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Context> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        let mut out: *const c_void = std::ptr::null();
        // SAFETY: the isolate is entered on this thread; `out` receives the `Local`'s one pointer, a
        // handle in the current handle scope, and is not null (V8 falls back to the current context).
        unsafe {
            ffi::get_incumbent_context(ffi::get_current_isolate(), &mut out);
            if let Some(context) = std::ptr::NonNull::new(out as *mut v8::Context) {
                return std::mem::transmute::<
                    std::ptr::NonNull<v8::Context>,
                    v8::Local<'s, v8::Context>,
                >(context);
            }
        }
    }
    scope.get_current_context()
}

/// HTML's "prepare to run a callback" and "clean up after running a callback" (§8.1.3.3): `f` runs with
/// `context` on the backup incumbent settings object stack, so [`incumbent_context`] gives it wherever
/// no script newer than this call is on the stack, whatever older script there is.
pub fn with_backup_incumbent<'s, R>(
    context: v8::Local<'s, v8::Context>,
    f: impl FnOnce() -> R,
) -> R {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        struct Scope(std::mem::MaybeUninit<ffi::BackupIncumbentScope>);
        impl Drop for Scope {
            fn drop(&mut self) {
                // SAFETY: built below, at this address, before anything could drop it; scopes end in
                // reverse order of their start, as locals of nested calls do.
                unsafe { ffi::backup_incumbent_scope_drop(self.0.as_mut_ptr()) };
            }
        }
        let mut scope = Scope(std::mem::MaybeUninit::uninit());
        // SAFETY: `scope` stays at this address until it drops at the end of this block, and V8 keeps
        // `context`'s handle, which outlives the block ('s), for that long. A `Local` travels as its
        // one pointer (see the module's notes).
        unsafe {
            ffi::backup_incumbent_scope_new(
                scope.0.as_mut_ptr(),
                &*context as *const v8::Context as *const c_void,
            );
        }
        f()
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    {
        let _ = context;
        f()
    }
}

#[cfg(all(test, windows, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use v8::MapFnTo;

    thread_local! {
        /// The handlers that ran, in order.
        static CALLED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    fn called(slot: &'static str) {
        CALLED.with(|c| c.borrow_mut().push(slot));
    }

    /// Runs `source` and gives the handlers it reached.
    fn slots(scope: &mut v8::PinScope<'_, '_>, source: &str) -> Vec<&'static str> {
        CALLED.with(|c| c.borrow_mut().clear());
        run(scope, source);
        CALLED.with(|c| c.borrow().clone())
    }

    fn get(
        scope: &mut v8::PinScope,
        key: v8::Local<v8::Name>,
        _args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue,
    ) -> v8::Intercepted {
        called("named getter");
        if key.to_rust_string_lossy(scope) == "visible" {
            rv.set(v8::Integer::new(scope, 42).into());
        } else {
            let m = v8::String::new(scope, "blocked").unwrap();
            let e = v8::Exception::error(scope, m);
            scope.throw_exception(e);
        }
        v8::Intercepted::kYes
    }
    fn descriptor(
        scope: &mut v8::PinScope,
        _key: v8::Local<v8::Name>,
        _args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue,
    ) -> v8::Intercepted {
        called("named descriptor");
        rv.set(data_descriptor(scope).into());
        v8::Intercepted::kYes
    }
    fn data_descriptor<'s>(scope: &v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Object> {
        let null: v8::Local<v8::Value> = v8::null(scope).into();
        let key = v8::String::new(scope, "value").unwrap();
        let value: v8::Local<v8::Value> = v8::Integer::new(scope, 1).into();
        v8::Object::with_prototype_and_properties(scope, null, &[key.into()], &[value])
    }
    fn set(
        _scope: &mut v8::PinScope,
        _key: v8::Local<v8::Name>,
        _value: v8::Local<v8::Value>,
        _args: v8::PropertyCallbackArguments,
        _rv: v8::ReturnValue<v8::Boolean>,
    ) -> v8::Intercepted {
        called("named setter");
        v8::Intercepted::kYes
    }
    fn query(
        _scope: &mut v8::PinScope,
        _key: v8::Local<v8::Name>,
        _args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue<v8::Integer>,
    ) -> v8::Intercepted {
        called("named query");
        rv.set_int32(2);
        v8::Intercepted::kYes
    }
    fn keys(
        scope: &mut v8::PinScope,
        _args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue<v8::Array>,
    ) {
        called("named enumerator");
        let k = v8::String::new(scope, "visible").unwrap();
        rv.set(v8::Array::new_with_elements(scope, &[k.into()]));
    }
    fn define(
        _scope: &mut v8::PinScope,
        _key: v8::Local<v8::Name>,
        _desc: &v8::PropertyDescriptor,
        _args: v8::PropertyCallbackArguments,
        _rv: v8::ReturnValue<v8::Boolean>,
    ) -> v8::Intercepted {
        called("named definer");
        v8::Intercepted::kYes
    }
    fn iget(
        scope: &mut v8::PinScope,
        _index: u32,
        _args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue,
    ) -> v8::Intercepted {
        called("indexed getter");
        rv.set(v8::Integer::new(scope, 7).into());
        v8::Intercepted::kYes
    }
    fn idescriptor(
        scope: &mut v8::PinScope,
        _index: u32,
        _args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue,
    ) -> v8::Intercepted {
        called("indexed descriptor");
        rv.set(data_descriptor(scope).into());
        v8::Intercepted::kYes
    }
    fn iset(
        _scope: &mut v8::PinScope,
        _index: u32,
        _value: v8::Local<v8::Value>,
        _args: v8::PropertyCallbackArguments,
        _rv: v8::ReturnValue<v8::Boolean>,
    ) -> v8::Intercepted {
        called("indexed setter");
        v8::Intercepted::kYes
    }
    fn iquery(
        _scope: &mut v8::PinScope,
        _index: u32,
        _args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue<v8::Integer>,
    ) -> v8::Intercepted {
        called("indexed query");
        rv.set_int32(1);
        v8::Intercepted::kYes
    }
    fn ikeys(
        scope: &mut v8::PinScope,
        _args: v8::PropertyCallbackArguments,
        mut rv: v8::ReturnValue<v8::Array>,
    ) {
        called("indexed enumerator");
        let k: v8::Local<v8::Value> = v8::Integer::new(scope, 0).into();
        rv.set(v8::Array::new_with_elements(scope, &[k]));
    }
    fn idefine(
        _scope: &mut v8::PinScope,
        _index: u32,
        _desc: &v8::PropertyDescriptor,
        _args: v8::PropertyCallbackArguments,
        _rv: v8::ReturnValue<v8::Boolean>,
    ) -> v8::Intercepted {
        called("indexed definer");
        v8::Intercepted::kYes
    }

    fn refuse<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        context: v8::Local<'s, v8::Context>,
        _object: v8::Local<'s, v8::Object>,
    ) -> bool {
        // (the context V8 passes is the accessing one, the one current here)
        assert!(context == scope.get_current_context());
        false
    }
    fn failed<'s>(scope: &mut v8::PinScope<'s, '_>, _target: v8::Local<'s, v8::Object>) {
        called("failed access");
        let m = v8::String::new(scope, "refused").unwrap();
        let e = v8::Exception::error(scope, m);
        scope.throw_exception(e);
    }

    fn run(scope: &mut v8::PinScope<'_, '_>, source: &str) -> String {
        v8::tc_scope!(let tc, scope);
        let code = v8::String::new(tc, source).unwrap();
        let result = v8::Script::compile(tc, code, None).and_then(|s| s.run(tc));
        match result {
            Some(v) => v.to_rust_string_lossy(tc),
            None => format!(
                "threw {}",
                tc.exception()
                    .map_or(String::new(), |e| e.to_rust_string_lossy(tc))
            ),
        }
    }

    /// The library is the one the declarations were checked against: what [`install`] insists on.
    #[test]
    fn the_library_is_the_verified_one() {
        crate::init();
        assert!(v8::V8::get_version().starts_with(VERIFIED_V8));
    }

    /// `name` of the incumbent context's global object, for [`backup_scope_sets_the_incumbent`].
    fn incumbent_name(scope: &v8::PinScope<'_, '_>) -> String {
        let global = incumbent_context(scope).global(scope);
        let key = v8::String::new(scope, "name").unwrap();
        let name = global.get(scope, key.into()).unwrap();
        name.to_rust_string_lossy(scope)
    }

    fn record_incumbent(
        scope: &mut v8::PinScope,
        _args: v8::FunctionCallbackArguments,
        mut rv: v8::ReturnValue,
    ) {
        let name = incumbent_name(scope);
        rv.set(v8::String::new(scope, &name).unwrap().into());
    }

    /// The hand-bound scope works: it decides the incumbent with no script on the stack, gives way
    /// to newer script, nests and ends. The native function is called as a bound listener is, by V8
    /// from the embedder, and by script.
    #[test]
    fn backup_scope_sets_the_incumbent() {
        crate::init();
        let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let scope, isolate);
        let a = v8::Context::new(scope, Default::default());
        let b = v8::Context::new(scope, Default::default());
        let c = v8::Context::new(scope, Default::default());
        let token = v8::String::new(scope, "same origin").unwrap();
        for (context, name) in [(a, "a"), (b, "b"), (c, "c")] {
            context.set_security_token(token.into());
            let scope = &mut v8::ContextScope::new(scope, context);
            run(scope, &format!("var name = '{name}'"));
        }
        let scope = &mut v8::ContextScope::new(scope, a);
        assert_eq!(incumbent_name(scope), "a");
        with_backup_incumbent(b, || {
            assert_eq!(incumbent_name(scope), "b");
            with_backup_incumbent(c, || assert_eq!(incumbent_name(scope), "c"));
            assert_eq!(incumbent_name(scope), "b");
        });
        assert_eq!(incumbent_name(scope), "a");
        let f = v8::Function::new(scope, record_incumbent).unwrap();
        let recv = v8::undefined(scope);
        with_backup_incumbent(b, || {
            let r = f.call(scope, recv.into(), &[]).unwrap();
            assert_eq!(r.to_rust_string_lossy(scope), "b");
        });
        // Script newer than the scope is the incumbent.
        let global = a.global(scope);
        let key = v8::String::new(scope, "native").unwrap();
        global.set(scope, key.into(), f.into()).unwrap();
        with_backup_incumbent(b, || assert_eq!(run(scope, "native()"), "a"));
    }

    /// The hand-bound entry points work: each refused operation reaches its own handler slot (a moved
    /// or reordered field of the configurations reaches another, or crashes), a refused operation
    /// nothing intercepts reaches the failed callback, prototype walks stop, and a global proxy stays
    /// the same object for a new context of the same template.
    #[test]
    fn refused_access_reaches_the_handlers_and_the_proxy_outlives_its_context() {
        crate::init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        install(
            &mut isolate,
            Hooks {
                allow: refuse,
                failed,
            },
        );
        v8::scope!(let scope, &mut isolate);
        let template = v8::ObjectTemplate::new(scope);
        template.set_internal_field_count(2);
        guard_template(
            template,
            NamedHandlers {
                getter: get.map_fn_to(),
                setter: set.map_fn_to(),
                query: query.map_fn_to(),
                enumerator: keys.map_fn_to(),
                definer: define.map_fn_to(),
                descriptor: descriptor.map_fn_to(),
            },
            IndexedHandlers {
                getter: iget.map_fn_to(),
                setter: iset.map_fn_to(),
                query: iquery.map_fn_to(),
                enumerator: ikeys.map_fn_to(),
                definer: idefine.map_fn_to(),
                descriptor: idescriptor.map_fn_to(),
            },
        );
        let options = || v8::ContextOptions {
            global_template: Some(template),
            ..Default::default()
        };
        let a = v8::Context::new(scope, options());
        let b = v8::Context::new(scope, options());
        let token_a = v8::String::new(scope, "a").unwrap();
        let token_b = v8::String::new(scope, "b").unwrap();
        a.set_security_token(token_a.into());
        b.set_security_token(token_b.into());
        let proxy = b.global(scope);
        {
            let scope = &mut v8::ContextScope::new(scope, a);
            let key = v8::String::new(scope, "other").unwrap();
            let g = a.global(scope);
            g.set(scope, key.into(), proxy.into());
            assert_eq!(run(scope, "other.visible"), "42");
            assert_eq!(run(scope, "other.hidden"), "threw Error: blocked");
            assert_eq!(run(scope, "String(Object.getPrototypeOf(other))"), "null");
            assert_eq!(run(scope, "delete other.visible"), "threw Error: refused");
            // (V8 reports a refused define before any interceptor: OrdinaryDefineOwnProperty deals with
            // access checks first, so the definers are not reached here)
            for (source, slot) in [
                ("other.visible", "named getter"),
                ("other.visible = 1", "named setter"),
                ("'visible' in other", "named query"),
                (
                    "Object.defineProperty(other, 'x', { value: 1 })",
                    "failed access",
                ),
                (
                    "Object.getOwnPropertyDescriptor(other, 'x')",
                    "named descriptor",
                ),
                ("Object.getOwnPropertyNames(other)", "named enumerator"),
                ("other[0]", "indexed getter"),
                ("other[0] = 1", "indexed setter"),
                ("0 in other", "indexed query"),
                (
                    "Object.defineProperty(other, 0, { value: 1 })",
                    "failed access",
                ),
                (
                    "Object.getOwnPropertyDescriptor(other, 0)",
                    "indexed descriptor",
                ),
                ("Object.getOwnPropertyNames(other)", "indexed enumerator"),
                ("delete other[0]", "failed access"),
            ] {
                assert!(
                    slots(scope, source).contains(&slot),
                    "{source} did not reach the {slot}"
                );
            }
        }
        // The proxy goes on with a new context.
        let c = v8::Context::new(
            scope,
            v8::ContextOptions {
                global_template: Some(template),
                global_object: Some(proxy.into()),
                ..Default::default()
            },
        );
        c.set_security_token(token_a.into());
        assert!(c.global(scope) == proxy);
        {
            let scope = &mut v8::ContextScope::new(scope, c);
            run(scope, "var fresh = 7");
        }
        let scope = &mut v8::ContextScope::new(scope, a);
        // (the same token now: ordinary access, to the new context's global)
        assert_eq!(run(scope, "other.fresh"), "7");
        assert_eq!(run(scope, "other === other.other"), "false");
    }
}
