# Upgrading V8

`snowlark-js` reaches parts of V8 that the `v8` crate does not bind, by mangled symbol name and by layout. Any
change of the `v8` crate version, or of the V8 library's build arguments, must re-verify those declarations
against the new library (symbol names, argument passing, vtable and struct layout) and run the streaming, code
cache and access check tests before anything else.

## What is reached directly

- `crates/snowlark-js/src/cached_accessor.rs` reaches `FunctionTemplate::NewWithCache` by its mangled name.
- `crates/snowlark-js/src/streaming.rs` reaches V8's script-streaming and off-thread code-cache APIs through
  their MSVC x64 mangled symbol names and a hand-laid vtable.
- `crates/snowlark-js/src/access_check.rs` reaches V8's access checks the same way, with hand-laid
  `*PropertyHandlerConfiguration` structs.
- `crates/snowlark-js/src/object_clone.rs` reaches `Object::Clone(Isolate*)` (new wrappers are clones of a
  per-context boilerplate) by its mangled name, with the hidden return pointer of a member function.
- `crates/snowlark-js/src/hooks.rs` reaches `ObjectTemplate::MarkAsUndetectable` and
  `ObjectTemplate::SetCodeLike` (TrustedScript is code like) by their mangled names.
- `crates/snowlark-js/src/wrapper_ref.rs` reaches `v8::api_internal`'s global handle functions
  (`GlobalizeReference`, `MakeWeak`, `ClearWeak`, `DisposeGlobal`) by name and writes a `ReturnValue`'s slot
  directly (a full address, which with pointer compression and static roots is what V8's own store leaves
  there).
- `crates/snowlark-js/src/raw_callback.rs` reads a `FunctionCallbackInfo` in place (`index`: argc 0, isolate 5,
  return value 6, receiver 9, first argument 10, from the enum in `$v8/v8/include/v8-function-callback.h` with
  `kFrameCPSlotCount` 0), writes Smis of 31 bits as `value << 1` in the return slot's low half (a value outside
  `SMI_MIN..=SMI_MAX` goes through V8, which makes a number), and reads the isolate's data slots at
  `ISOLATE_EMBEDDER_DATA_OFFSET` (`Internals::kIsolateEmbedderDataOffset`, 688 at present).
  `raw_callback::tests` checks each against the crate's accessors, and `verify_embedder_data` panics on a
  changed offset in every isolate the bindings install.
- `crates/snowlark-js/src/internal_field.rs` reads and writes a wrapper's internal fields inline, as 31-bit Smis
  in the slot's 32-bit tagged half (the other half is V8's C++ heap pointer table handle, never touched), at
  `Internals` offsets (`kMapInstanceTypeOffset`, `kJSAPIObjectWithEmbedderSlotsHeaderSize`,
  `kEmbedderDataSlotSize`, the API object instance type range; tagged size 4, the map decompressed from the cage
  base, `kPtrComprCageBaseAlignment`). A wrapper's 62-bit handle takes two fields (`snowlark-bindings`
  `rt/wrap.rs`). Its `verify` panics in every isolate the bindings install when V8's own
  `Get/SetInternalField` disagree.

V8 is built with pointer compression (the `v8_enable_pointer_compression` feature in the workspace `Cargo.toml`,
and the pinned library): the inline field and Smi code assumes it.

## The access check pin

The access check is the origin boundary, so `access_check::install` panics on any V8 but `VERIFIED_V8`: no page
starts until the new version is verified. A symbol that moved stops the link; a struct that changed does not,
which is why the pin exists. To re-verify, with `$v8` the crate's source (it ships V8's headers and sources in
`v8/`):

1. **Names.** `dumpbin /linkermember:1 <the library>`, and search for each `link_name` in the files above. The
   mangled name carries the argument types, so a changed signature shows here.
2. **Layout.** In `$v8/v8/include/v8-template.h`, compare the fields of `NamedPropertyHandlerConfiguration` and
   `IndexedPropertyHandlerConfiguration`, in order, with `ffi::NamedConfiguration` and
   `ffi::IndexedConfiguration`, and the callback typedefs with the crate's `Named*Callback` and
   `Indexed*Callback`. In `v8-local-handle.h`, `Local<T>` must still be one pointer, trivially copyable.
3. **Behaviour.** Read `Isolate::MayAccess` and `Isolate::ReportFailedAccessCheck` in
   `$v8/v8/src/execution/isolate.cc`, and the `ReportFailedAccessCheck` callers: the token comparison first, the
   callback after, an exception required.
4. Set `VERIFIED_V8`, then run `.\dev.ps1 test --release --package snowlark-js access_check` (every handler slot
   is driven through V8: one that moved fails or crashes) and
   `.\dev.ps1 test --release --package snowlark-page --test it cross_origin::`.

## The library

`dev.ps1` links the library named by `tools/v8-pgo/pin.txt` ([v8-build.md](v8-build.md)); with a crate version
other than the pin's it leaves V8 to the crate and says so, until a library is built for the new version with
`tools/v8-pgo/` (`docs/v8-speed.md`, section 6) and pinned.

The ThinLTO step (`tools/v8-pgo/thinlto-native.py`) keeps the bindings, V8's public C++ API and every
`link_name` in our crates, and makes the rest internal: a new mangled-name declaration outside the public API
shows as an unresolved symbol until the library is rebuilt.

The same V8 version built with a new profile changes no symbol, struct or offset: run the access check tests
and the micro benchmarks' `js/` tests.

The crate is vendored (`vendor/v8`): `vendor/v8/SNOWLARK-PATCHES.md` says which commit it is, why, and when the
directory can go.
