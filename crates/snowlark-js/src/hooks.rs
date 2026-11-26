// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Host hooks that V8 calls without a scope of its own: module resolution while linking, `import.meta`
//! initialization, and the promise rejection tracker. Each raw callback here opens the callback scope
//! (the one place that needs `unsafe`) and calls the safe hook the embedder installed in the isolate.

/// Resolves `specifier` imported by `referrer` with a module type (`"javascript"`, `"json"`...) while a
/// module graph links: the module, or `None` with an exception thrown.
pub type ResolveHook = for<'s, 'i> fn(
    &mut v8::PinScope<'s, 'i>,
    &str,
    &str,
    v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>>;

/// Fills in `import.meta` for `module`.
pub type ImportMetaHook =
    for<'s, 'i> fn(&mut v8::PinScope<'s, 'i>, v8::Local<'s, v8::Module>, v8::Local<'s, v8::Object>);

/// HTML's HostPromiseRejectionTracker: `handled` is false for "reject" (a rejection without handlers)
/// and true for "handle" (a handler added after the rejection was reported).
pub type RejectionHook = for<'s, 'i> fn(
    &mut v8::PinScope<'s, 'i>,
    bool,
    v8::Local<'s, v8::Promise>,
    Option<v8::Local<'s, v8::Value>>,
);

/// HTML's HostLoadImportedModule for `import()`: (host-defined options, the referrer's resource name,
/// the specifier, the import attributes) to the promise of the module's namespace.
pub type DynamicImportHook = for<'s, 'i> fn(
    &mut v8::PinScope<'s, 'i>,
    v8::Local<'s, v8::Data>,
    v8::Local<'s, v8::Value>,
    v8::Local<'s, v8::String>,
    v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>>;

/// Work the embedder asked to run between two JS steps ([`request_interrupt`]); the scope has no
/// context (the hook enters one of its own).
pub type InterruptHook = for<'s, 'i> fn(&mut v8::PinScope<'s, 'i, ()>);

/// CSP's EnsureCSPDoesNotBlockStringCompilation (HTML's HostEnsureCanCompileStrings) for `eval()` and
/// `new Function()` in a context whose code generation from strings was turned off
/// ([`v8::Context::set_allow_generation_from_strings`]): given what was passed, whether it may compile.
/// V8 also asks for a non-string argument in any context: the hook allows those as they are (they
/// are returned unchanged) or gives the source to compile for them (Trusted Types' TrustedScript).
/// The flag is V8's `is_code_like`: the value is a code-like object ([`mark_as_code_like`]), or a
/// `Function` constructor's source made only of code-like arguments.
pub type CodegenHook =
    for<'s, 'i> fn(&mut v8::PinScope<'s, 'i>, v8::Local<'s, v8::Value>, bool) -> Codegen<'s>;

/// The string compilation hook's decision.
pub enum Codegen<'s> {
    Block,
    Allow,
    /// Compile this source instead of the value passed.
    Compile(v8::Local<'s, v8::String>),
}

/// Whether WebAssembly may be compiled in the current context (CSP §4.5 'wasm-unsafe-eval').
pub type WasmCodegenHook = for<'s, 'i> fn(&mut v8::PinScope<'s, 'i>) -> bool;

#[derive(Clone, Copy, Default)]
struct Hooks {
    resolve: Option<ResolveHook>,
    import_meta: Option<ImportMetaHook>,
    rejection: Option<RejectionHook>,
    interrupt: Option<InterruptHook>,
    dynamic_import: Option<DynamicImportHook>,
    codegen: Option<CodegenHook>,
    wasm: Option<WasmCodegenHook>,
    wasm_streaming: Option<WasmStreamingHook>,
}

/// Feeds `WebAssembly.compileStreaming(source)` and `instantiateStreaming(source)` (the
/// WebAssembly Web API's "compile a potential WebAssembly response"): `source` as script passed
/// it, and V8's compilation, which takes the response's bytes.
pub type WasmStreamingHook =
    for<'s, 'i> fn(&mut v8::PinScope<'s, 'i>, v8::Local<'s, v8::Value>, v8::WasmStreaming<false>);

/// Installs the streaming compilation hook; with it V8 provides `WebAssembly.compileStreaming`
/// and `WebAssembly.instantiateStreaming`.
pub fn set_wasm_streaming_hook(isolate: &mut v8::Isolate, hook: WasmStreamingHook) {
    let mut h = isolate.get_slot::<Hooks>().copied().unwrap_or_default();
    h.wasm_streaming = Some(hook);
    isolate.set_slot(h);
    isolate.set_wasm_streaming_callback(wasm_streaming_callback);
}

fn wasm_streaming_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: v8::Local<'s, v8::Value>,
    streaming: v8::WasmStreaming<false>,
) {
    let Some(hook) = scope.get_slot::<Hooks>().and_then(|h| h.wasm_streaming) else {
        return;
    };
    // (a panic aborts the compilation without settling its promise rather than crossing into V8)
    let _ = crate::guard::catch(|| hook(scope, source, streaming));
}

/// Installs the string compilation hook. Contexts that allow code generation from strings (the
/// default) compile strings without asking it.
pub fn set_codegen_hook(isolate: &mut v8::Isolate, hook: CodegenHook) {
    let mut h = isolate.get_slot::<Hooks>().copied().unwrap_or_default();
    h.codegen = Some(hook);
    isolate.set_slot(h);
    isolate.set_modify_code_generation_from_strings_callback(codegen_callback);
}

fn codegen_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: v8::Local<'s, v8::Value>,
    is_code_like: bool,
) -> v8::ModifyCodeGenerationFromStringsResult<'s> {
    let decision = match scope.get_slot::<Hooks>().and_then(|h| h.codegen) {
        // (a hook that panicked blocks: the safe answer)
        Some(hook) => {
            crate::guard::catch(|| hook(scope, source, is_code_like)).unwrap_or(Codegen::Block)
        }
        None => Codegen::Allow,
    };
    match decision {
        Codegen::Block => v8::ModifyCodeGenerationFromStringsResult {
            codegen_allowed: false,
            modified_source: None,
        },
        Codegen::Allow => v8::ModifyCodeGenerationFromStringsResult {
            codegen_allowed: true,
            modified_source: None,
        },
        Codegen::Compile(s) => v8::ModifyCodeGenerationFromStringsResult {
            codegen_allowed: true,
            modified_source: Some(s),
        },
    }
}

/// Makes the objects of `template` "undetectable" (V8's name for HTML's [[IsHTMLDDA]] internal slot,
/// ECMA-262 Annex B.3.6): `typeof` says "undefined", they compare loosely equal to undefined and
/// null, and convert to false. The `v8` crate does not bind `ObjectTemplate::MarkAsUndetectable`, so
/// it is reached by its mangled name (see `docs/v8-upgrade.md`).
pub fn mark_as_undetectable(template: v8::Local<v8::ObjectTemplate>) {
    extern "C" {
        #[cfg_attr(
            not(v8_abi_itanium),
            link_name = "?MarkAsUndetectable@ObjectTemplate@v8@@QEAAXXZ"
        )]
        #[cfg_attr(
            v8_abi_itanium,
            link_name = "_ZN2v814ObjectTemplate18MarkAsUndetectableEv"
        )]
        fn mark_as_undetectable_raw(this: *const v8::ObjectTemplate);
    }
    // SAFETY: a `Local<ObjectTemplate>` is the handle pointer the C++ API takes as `this` (as the
    // crate's own `v8__ObjectTemplate__*` shims pass it); the method takes no other argument, returns
    // nothing, and only sets a flag on the template, which the local keeps alive for the call.
    unsafe { mark_as_undetectable_raw(&*template) }
}

/// Makes the objects of `template` "code like" (TC39 dynamic code brand checks; Trusted Types'
/// TrustedScript): `eval()` and the `Function` constructor hand them to the string compilation hook
/// instead of passing them through, and a `Function` whose arguments were all code like reaches the
/// hook as its source with `is_code_like` set. The `v8` crate does not bind
/// `ObjectTemplate::SetCodeLike`, so it is reached by its mangled name (see
/// `docs/v8-upgrade.md`).
pub fn mark_as_code_like(template: v8::Local<v8::ObjectTemplate>) {
    extern "C" {
        #[cfg_attr(
            not(v8_abi_itanium),
            link_name = "?SetCodeLike@ObjectTemplate@v8@@QEAAXXZ"
        )]
        #[cfg_attr(v8_abi_itanium, link_name = "_ZN2v814ObjectTemplate11SetCodeLikeEv")]
        fn set_code_like_raw(this: *const v8::ObjectTemplate);
    }
    // SAFETY: as for `mark_as_undetectable`: `this` is the template's handle pointer, the method
    // takes no other argument, returns nothing, and only sets a flag on the template.
    unsafe { set_code_like_raw(&*template) }
}

/// Installs the WebAssembly compilation hook (asked for every compilation; without one, V8 allows
/// WebAssembly exactly where it allows code generation from strings).
pub fn set_wasm_codegen_hook(isolate: &mut v8::Isolate, hook: WasmCodegenHook) {
    let mut h = isolate.get_slot::<Hooks>().copied().unwrap_or_default();
    h.wasm = Some(hook);
    isolate.set_slot(h);
    isolate.set_allow_wasm_code_generation_callback(wasm_codegen_callback);
}

unsafe extern "C" fn wasm_codegen_callback(
    context: v8::Local<v8::Context>,
    _source: v8::Local<v8::String>,
) -> bool {
    // SAFETY: V8 calls this synchronously on the isolate's thread, in `context`, while script compiles
    // WebAssembly; the callback scope lives only for this call.
    v8::callback_scope!(unsafe scope, context);
    let Some(hook) = scope.get_slot::<Hooks>().and_then(|h| h.wasm) else {
        return true;
    };
    v8::scope!(let scope, scope);
    crate::guard::catch(|| hook(scope)).unwrap_or(false)
}

/// Installs the module resolution hook used by [`instantiate`].
pub fn set_resolve_hook(isolate: &mut v8::Isolate, hook: ResolveHook) {
    let mut h = isolate.get_slot::<Hooks>().copied().unwrap_or_default();
    h.resolve = Some(hook);
    isolate.set_slot(h);
}

/// Installs the `import.meta` hook.
pub fn set_import_meta_hook(isolate: &mut v8::Isolate, hook: ImportMetaHook) {
    let mut h = isolate.get_slot::<Hooks>().copied().unwrap_or_default();
    h.import_meta = Some(hook);
    isolate.set_slot(h);
    isolate.set_host_initialize_import_meta_object_callback(import_meta_callback);
}

/// Installs the promise rejection tracker.
pub fn set_rejection_hook(isolate: &mut v8::Isolate, hook: RejectionHook) {
    let mut h = isolate.get_slot::<Hooks>().copied().unwrap_or_default();
    h.rejection = Some(hook);
    isolate.set_slot(h);
    isolate.set_promise_reject_callback(rejection_callback);
}

/// Installs the `import()` hook.
pub fn set_dynamic_import_hook(isolate: &mut v8::Isolate, hook: DynamicImportHook) {
    let mut h = isolate.get_slot::<Hooks>().copied().unwrap_or_default();
    h.dynamic_import = Some(hook);
    isolate.set_slot(h);
    isolate.set_host_import_module_dynamically_callback(dynamic_import_callback);
}

fn dynamic_import_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    options: v8::Local<'s, v8::Data>,
    resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let hook = scope.get_slot::<Hooks>().and_then(|h| h.dynamic_import)?;
    match crate::guard::catch(|| hook(scope, options, resource_name, specifier, attributes)) {
        Some(p) => p,
        None => {
            scope.terminate_execution();
            None
        }
    }
}

/// Installs the hook [`request_interrupt`] runs.
pub fn set_interrupt_hook(isolate: &mut v8::Isolate, hook: InterruptHook) {
    let mut h = isolate.get_slot::<Hooks>().copied().unwrap_or_default();
    h.interrupt = Some(hook);
    isolate.set_slot(h);
}

/// Asks V8 to run the installed [`InterruptHook`] at the next interrupt check of the running script
/// (a loop back edge or a call); nothing runs if no script is running before the isolate is dropped.
pub fn request_interrupt(isolate: &v8::Isolate) {
    isolate
        .thread_safe_handle()
        .request_interrupt(interrupt_callback, std::ptr::null_mut());
}

unsafe extern "C" fn interrupt_callback(
    isolate: v8::UnsafeRawIsolatePtr,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: V8 runs interrupt callbacks on the isolate's own thread, inside the isolate, between two
    // steps of the running script; the isolate pointer is the isolate itself and outlives this call.
    let mut isolate = unsafe { v8::Isolate::from_raw_isolate_ptr(isolate) };
    let Some(hook) = isolate.get_slot::<Hooks>().and_then(|h| h.interrupt) else {
        return;
    };
    // SAFETY: as above: the callback scope lives only for this call, on the isolate's thread.
    v8::callback_scope!(unsafe scope, &mut isolate);
    v8::scope!(let scope, scope);
    if crate::guard::catch(|| hook(scope)).is_none() {
        scope.terminate_execution();
    }
}

/// Links `module` (`InstantiateModule`), resolving imports with the installed [`ResolveHook`].
pub fn instantiate(
    scope: &mut v8::PinScope<'_, '_>,
    module: v8::Local<v8::Module>,
) -> Option<bool> {
    module.instantiate_module(scope, resolve_callback)
}

fn resolve_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    import_attributes: v8::Local<'s, v8::FixedArray>,
    referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    // SAFETY: V8 calls the resolve callback synchronously from InstantiateModule, on the isolate's
    // thread, with a live context; the callback scope lives only for this call.
    v8::callback_scope!(unsafe scope, context);
    let hook = scope.get_slot::<Hooks>().and_then(|h| h.resolve)?;
    let spec = specifier.to_rust_string_lossy(scope);
    let ty = import_type(scope, import_attributes, 3).unwrap_or_default();
    match crate::guard::catch(|| hook(scope, &spec, &ty, referrer)) {
        Some(m) => m,
        None => {
            scope.terminate_execution();
            None
        }
    }
}

/// # Safety
///
/// Only V8 calls this, as the HostInitializeImportMetaObjectCallback, with a live context.
unsafe extern "C" fn import_meta_callback(
    context: v8::Local<v8::Context>,
    module: v8::Local<v8::Module>,
    meta: v8::Local<v8::Object>,
) {
    // SAFETY: V8 passes a context that is alive for the duration of the callback, on its own thread.
    v8::callback_scope!(unsafe scope, context);
    if let Some(hook) = scope.get_slot::<Hooks>().and_then(|h| h.import_meta) {
        if crate::guard::catch(|| hook(scope, module, meta)).is_none() {
            scope.terminate_execution();
        }
    }
}

extern "C" fn rejection_callback(message: v8::PromiseRejectMessage) {
    // SAFETY: V8 calls the promise reject callback synchronously on the isolate's thread with a message
    // that is valid for the duration of the call.
    v8::callback_scope!(unsafe scope, &message);
    let Some(hook) = scope.get_slot::<Hooks>().and_then(|h| h.rejection) else {
        return;
    };
    let handled = match message.get_event() {
        v8::PromiseRejectEvent::PromiseRejectWithNoHandler => false,
        v8::PromiseRejectEvent::PromiseHandlerAddedAfterReject => true,
        _ => return,
    };
    v8::scope!(let scope, scope);
    let promise = message.get_promise();
    let value = message.get_value();
    let Some(context) = promise.get_creation_context(scope) else {
        return;
    };
    let scope = &mut v8::ContextScope::new(scope, context);
    if crate::guard::catch(|| hook(scope, handled, promise, value)).is_none() {
        scope.terminate_execution();
    }
}

/// The `type` of an import's attributes (`with { type: "json" }`): "javascript" when absent, `None`
/// when an attribute other than `type` is present (HTML: unsupported attributes fail the import).
/// `stride` is 2 for the key-value pairs V8 passes to the resolve and dynamic import callbacks and 3
/// for a `ModuleRequest`'s (key, value, source offset) triples.
pub fn import_type(
    scope: &mut v8::PinScope<'_, '_>,
    attributes: v8::Local<v8::FixedArray>,
    stride: usize,
) -> Option<String> {
    let mut ty = "javascript".to_owned();
    let n = attributes.length();
    // V8 passes (key, value) pairs or (key, value, offset) triples; an offset is a number.
    let stride = if n >= 3 {
        match attributes.get(scope, 2) {
            Some(d) => match v8::Local::<v8::Value>::try_from(d) {
                Ok(v) if v.is_number() => 3,
                _ => 2,
            },
            None => stride,
        }
    } else {
        stride
    };
    let mut i = 0;
    while i + 1 < n {
        let key = attributes.get(scope, i)?;
        let value = attributes.get(scope, i + 1)?;
        let key: v8::Local<v8::Value> = key.try_into().ok()?;
        let value: v8::Local<v8::Value> = value.try_into().ok()?;
        let k = key.to_rust_string_lossy(scope);
        if k != "type" {
            return None;
        }
        ty = value.to_rust_string_lossy(scope);
        i += stride;
    }
    Some(ty)
}

/// The values of the synthetic modules [`synthetic_module`] made, by module identity hash, until they
/// evaluate.
#[derive(Default)]
struct SyntheticValues(std::collections::HashMap<i32, v8::Global<v8::Value>>);

/// A synthetic module whose only export, `default`, is `value` (JSON and text module scripts).
pub fn synthetic_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
    value: v8::Local<'s, v8::Value>,
) -> Option<v8::Local<'s, v8::Module>> {
    let name = v8::String::new(scope, name)?;
    let default = v8::String::new(scope, "default")?;
    let module = v8::Module::create_synthetic_module(scope, name, &[default], synthetic_steps);
    let hash = module.get_identity_hash().get();
    let global = v8::Global::new(scope, value);
    let mut values = scope
        .get_slot::<std::rc::Rc<std::cell::RefCell<SyntheticValues>>>()
        .cloned();
    if values.is_none() {
        let v = std::rc::Rc::new(std::cell::RefCell::new(SyntheticValues::default()));
        scope.set_slot(v.clone());
        values = Some(v);
    }
    values?.borrow_mut().0.insert(hash, global);
    Some(module)
}

fn synthetic_steps<'s>(
    context: v8::Local<'s, v8::Context>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    // SAFETY: V8 calls the evaluation steps synchronously from Module::Evaluate, on the isolate's
    // thread, with a live context; the callback scope lives only for this call.
    v8::callback_scope!(unsafe scope, context);
    let hash = module.get_identity_hash().get();
    let value = scope
        .get_slot::<std::rc::Rc<std::cell::RefCell<SyntheticValues>>>()
        .and_then(|v| v.borrow_mut().0.remove(&hash))?;
    let value = v8::Local::new(scope, value);
    let default = v8::String::new(scope, "default")?;
    module.set_synthetic_module_export(scope, default, value)?;
    // A module's Evaluate() returns a promise (ECMA-262, top-level await); V8 requires synthetic
    // evaluation steps to return one, resolved here as the steps cannot fail after this point.
    let resolver = v8::PromiseResolver::new(scope)?;
    let undefined = v8::undefined(scope);
    resolver.resolve(scope, undefined.into())?;
    Some(resolver.get_promise(scope).into())
}

/// Keeps the flag of [`watch_heap_limit`] alive as long as the isolate (an isolate slot).
struct HeapFlag(*const std::sync::atomic::AtomicBool);

impl Drop for HeapFlag {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `Arc::into_raw` in `watch_heap_limit` and is released once,
        // here, when the isolate drops its slots (after V8 stopped calling the callback).
        unsafe { drop(std::sync::Arc::from_raw(self.0)) };
    }
}

/// Watches the isolate's heap limit: when V8 nears it, `flag` is set and the limit is raised by half,
/// so the embedder can stop the page cleanly instead of V8 aborting the process.
pub fn watch_heap_limit(
    isolate: &mut v8::Isolate,
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let ptr = std::sync::Arc::into_raw(flag);
    isolate.set_slot(HeapFlag(ptr));
    isolate.add_near_heap_limit_callback(near_heap_limit, ptr as *mut std::ffi::c_void);
}

unsafe extern "C" fn near_heap_limit(
    data: *mut std::ffi::c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    // SAFETY: `data` is the `Arc<AtomicBool>` pointer `watch_heap_limit` registered; the isolate's
    // `HeapFlag` slot keeps it alive while V8 can call this.
    let flag = unsafe { &*(data as *const std::sync::atomic::AtomicBool) };
    flag.store(true, std::sync::atomic::Ordering::SeqCst);
    current_heap_limit + current_heap_limit / 2
}

/// How many garbage collections V8 finished in an isolate ([`count_gcs`]).
struct GcCount(std::sync::atomic::AtomicU64);

/// Counts `isolate`'s garbage collections from now on ([`gc_count`]): weak handles only die in one,
/// so a reader that scans them can skip the scan while the count stays the same.
pub fn count_gcs(isolate: &mut v8::Isolate) {
    let count = Box::new(GcCount(std::sync::atomic::AtomicU64::new(0)));
    let data = &*count as *const GcCount as *mut std::ffi::c_void;
    // The box lives in the isolate's slot, so as long as the isolate (and its callbacks).
    isolate.set_slot(count);
    isolate.add_gc_epilogue_callback(gc_epilogue, data, v8::GCType::kGCTypeAll);
}

/// The garbage collections counted since [`count_gcs`] (`None` when not counting).
pub fn gc_count(isolate: &v8::Isolate) -> Option<u64> {
    isolate
        .get_slot::<Box<GcCount>>()
        .map(|c| c.0.load(std::sync::atomic::Ordering::Relaxed))
}

unsafe extern "C" fn gc_epilogue(
    _isolate: v8::UnsafeRawIsolatePtr,
    _type: v8::GCType,
    _flags: v8::GCCallbackFlags,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: `data` points at the GcCount boxed in the isolate's slot ([`count_gcs`]), which lives
    // as long as the isolate that runs this callback.
    let count = unsafe { &*(data as *const GcCount) };
    count.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
