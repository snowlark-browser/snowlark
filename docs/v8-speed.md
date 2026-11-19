# Script speed: where our V8 differs from Chrome's, and what each option is worth

Speedometer 3.1's profile against Chrome (2026-11-15) put script first: JS (JIT code and
builtins) 26% of the mean suite's scored time and V8's C++ (runtime, compilers, parser) 20% (inflated by the profiler,
whose code events run in it), and if all of script (JS, V8, DOM and bindings) took Chrome's time the score would be
x1.288. This document splits that gap into what V8 decides and what our code decides, lists every way our V8 differs
from Chrome's, and gives each option's measured or bounded worth, cost and risk.

## 1. V8 alone against Chrome's V8: `bench/micro/js/`

Twelve pages that time JavaScript only (no DOM, no bindings), one kind of work each: property access on four shapes
(`object-props`), the array builtins, the string builtins, the regexp engine, `JSON.parse`/`stringify`, `Map`/`Set`,
classes and calls, a megamorphic site, compiling a new function each call, a framework's tree diff, iterators and
generators, short-lived allocation. Run with `tools/micro-bench/run.py js` (release build of main against
Chrome 157 headless, 3 processes each, steady state after two 50 ms warm-up rounds, on an otherwise idle machine):

| test | Snowlark ms | Chrome ms | ratio | where the time is |
|---|---:|---:|---:|---|
| object-props | 0.0303 | 0.0315 | 0.96 | optimized code |
| iterators-generators | 0.0315 | 0.0310 | 1.02 | optimized code, builtins |
| array-builtins | 0.0804 | 0.0755 | 1.06 | builtins |
| string-ops | 0.0234 | 0.0218 | 1.08 | builtins |
| class-calls | 0.0170 | 0.0148 | 1.15 | allocation, calls |
| map-set | 0.0159 | 0.0133 | 1.19 | builtins, allocation |
| vdom-diff | 0.0515 | 0.0434 | 1.19 | allocation |
| regexp | 0.0468 | 0.0383 | 1.22 | regexp code, runtime |
| allocation | 0.0840 | 0.0689 | 1.22 | allocation, scavenges |
| json-roundtrip | 0.0951 | 0.0680 | 1.40 | C++ (`JSON.parse`, `JSON.stringify`) |
| compile-functions | 0.1934 | 0.1377 | 1.40 | C++ (parser, bytecode generator) |
| megamorphic | 0.0376 | 0.0254 | 1.48 | stub cache, C++ IC misses |

Geometric mean **1.19**. The split is clean: code that runs as V8's generated machine code (optimized JS, the
embedded builtins) is at Chrome's speed within 0-8%; allocation-heavy work is 15-22% slower; work done in V8's C++
(parser and compiler, JSON, IC misses and the runtime) is 40-48% slower. A cold variant (no warm-up, three 10 ms
rounds) was too noisy to use: in each test, two of the three Snowlark processes ran about 2x slower than the third, and
the third matched the steady state.

So for the same JavaScript, our V8 is at most 1.2x Chrome's on average, and the C++ half of V8 is where it loses.
In Speedometer, V8's C++ is 20% of scored time and JIT code plus builtins 26%; closing the C++ gap (1.4x to 1.0) is
worth about x1.06 of the score, the allocation gap about x1.02, the JIT gap about x1.01 (shares from the profile,
ratios from this table). The rest of the x1.288 "script" gap is our own code under script: DOM, bindings and the
engine work they start (53% of the samples under a JS call), which is DOM and bindings work, not V8's.

## 2. How our V8 differs from Chrome's

**Flags: none.** Chrome's V8 initializer passes no V8 flags by default: each of its V8 features only sets a flag
when a field trial overrides it, otherwise "the corresponding V8 flags default values" apply (Sparkplug, Maglev,
Turbofan, lazy feedback, bytecode flushing, concurrent marking: all V8's defaults). Ours sets none either
(`snowlark_js::init` only reads `SNOWLARK_V8_FLAGS`). There is no flag of Chrome's to copy.

**Heap limits: small.** Chrome sizes each isolate from the machine (`ResourceConstraints::ConfigureDefaults(physical
memory, virtual memory)`); ours uses V8's defaults (`CreateParams::default()`). On 64-bit V8 both give a 32 MB
maximum semi-space (`Heap::DefaultMaxSemiSpaceSize`); the old generation's limits differ, which matters for when
full collections start. V8's GC is 1.5% of scored time (x1.015 if it were gone): a bound, not a lever.

**Build: four differences, all decided when V8 is compiled.**

| | ours (`v8` crate 152.2.0, prebuilt) | Chrome 157/158 |
|---|---|---|
| V8 version | 15.2.124.1 | 15.7 (installed 157), 15.8 (stable 158 today) |
| pointer compression | off; on since 2026-11-16 (3c) | on |
| sandbox | off | on |
| builtins PGO (block layout of the embedded builtins from a profile) | off: V8 applies its profile only with pointer compression, sandbox and external code space on x64; since 2026-11-17 our own profile's branch hints (3c) | on |
| C++ compiled with | clang `/O2`, no LTO, no PGO (not an official build); ours since 2026-11-16: PGO and ThinLTO (3b, 3b') | clang, ThinLTO and PGO (official build) |

## 3. The options

### 3a. Flags or configuration

Nothing to copy (above). Flags beyond Chrome's configuration (lower tier-up thresholds, larger young generation,
no bytecode flushing) trade memory or behaviour against Chrome's and are not a parity fix.

**Job finalization and lazy compile (2026-11-17, V8 15.8 with pointer compression).** A profile run with V8's CPU
profiler on makes V8 log every function it installs, and that logging lands in the finalization of Maglev and
Turbofan jobs: in such a profile (`SNOWLARK_PROFILE=1`) Maglev finalization was 2.5% of scored samples, 74% of it the
profiler's `CodeCreateEvent`. With the native sampler alone (`SNOWLARK_PROFILE_NATIVE=only`, all suites, 3
iterations) Maglev finalization is 0.5% and every `StackGuard` interrupt together 1.4% (the engine's own GC pass
included). V8 15.8 already builds Maglev code and frees its jobs on worker threads by default
(`maglev_build_code_on_background`, `maglev_destroy_on_background`), and no embedder flag changes the main-thread
part, so there is no flag to try here: at most x1.005.

Lazy compile (`Runtime_CompileLazy`, parse and bytecode) is 1.7% of that 3-iteration profile. V8's function log
(`--log-function-events`) over the same run: the scripts of every suite are compiled once and found in V8's
compilation cache in later iterations (the host-defined options are equal by value across loads: one id per base
URL and fetch options), and no function is lazily compiled twice, so the cost is each function's first call, almost
all in the first iteration: about 0.5% of a 10-iteration score, the same work any embedder of this V8 does.

**Build arguments the v8 crate sets against V8's defaults (2026-11-17).** The crate's `.gn` changes two that touch
script speed: `v8_typed_array_max_size_in_heap = 0` (every typed array gets a backing store from the array buffer
allocator; V8's default keeps those up to 64 bytes on the heap) and `v8_enable_javascript_promise_hooks = true` (the
promise builtins check for JavaScript hooks, which Snowlark never installs; V8's default is off). Bounded from the
native sampler's scored samples (all suites, 3 iterations, an earlier main): backing store allocation and freeing and
typed array code together 0.05%, all promise code 0.94%, so both under x1.002. Not worth the rebuild: changing
either alters builtins whose hashes the builtins' profile records, so the profile would be trained again too.
`v8_promise_internal_field_count = 1` (one embedder field per promise; V8's default 0) costs a word per promise.

**Where V8's scored time is now (2026-11-17, same profile).** Self samples in scored windows: embedded builtins
12.5%, V8's C++ 11.5%, generated code 6.9%; no single V8 C++ function above 0.5% (`LookupIterator::Start` 0.48%,
`BytecodeGenerator::GenerateBytecode` 0.39%, `HandleScope::CloseAndEscape` 0.38%, `GlobalizeReference` 0.32%), and
`Object::InternalFieldCount`, called by every wrapper brand check, 0.07%. The engine's own code (DOM, style, layout,
render, bindings) is 41% and the allocator and Rust's core library 22%, spread with no caller of the allocator above
0.4%.

### 3b. Profile-guided optimization of V8's C++ (our own profile)

The largest measured gap, C++ at 1.4x, is what an official build's PGO and ThinLTO address. Chrome's
profile belongs to Chrome's own revision and binary, so we train our own: build V8 instrumented
(`chrome_pgo_phase=1`), run Snowlark on a training set, merge the counts with `llvm-profdata`, and build V8 again
with them (`chrome_pgo_phase=2 pgo_data_path=...`). Same V8 version, same flags, same headers and layout: nothing
in `docs/v8-upgrade.md` changes (no symbol, struct or offset moves), so the access check needs no re-verification beyond
running its tests. ThinLTO is the next section.

How it is built (section 6). Cost: three V8 builds (22-33 min each at 8 jobs on the test machine) and a 2-minute
training run, once per V8 version; nothing for anyone else's builds once the library is published (they take it
through `RUSTY_V8_ARCHIVE` as they take the prebuilt one now). Risk: a profile that does not match V8's sources only
loses its effect (clang warns and compiles those functions without it); the code generated is the same code at
another layout, so the behaviour risk is a compiler bug's.

### 3b'. ThinLTO of V8, done once and shipped as a native library

An official build compiles V8 to LLVM bitcode (`use_thin_lto=true`) and optimizes across its files in the final
link: functions inlined from one file into another, code no caller reaches dropped, symbols nothing outside
needs made internal. That needs the final link to read bitcode, so lld-link (from V8's own clang; rustc's LLVM
is not V8's), and every build of ours would link with it: a ThinLTO step of V8 in every test binary's link.

Instead, `tools/v8-pgo/thinlto-native.py` does that link step once, as a distributed ThinLTO build does, and puts
native objects back into a library that the MSVC linker takes like the prebuilt one:

1. A thin link of the library's members alone (`lld-link /thinlto-index-only`), as a DLL with no entry point. Its
   roots, the symbols no consumer may lose, are every symbol the bindings' two members define (read from those
   members compiled alone), V8's public C++ API (the archive index's names in the `v8` and `cppgc` namespaces,
   outside `internal` and `base`: 2,410), and every `link_name` our crates and the crate's Rust declare
   (`access_check.rs`, `streaming.rs` and the rest call V8's C++ API by mangled name). Everything else may be
   internalized.
2. One backend compile per member (`clang -fthinlto-index=`), with the flags the build's own link step passes
   (`-O2`, the import limit of 30, the inline hint threshold of 360). Members nothing reaches from the roots are
   left out.
3. Two things the index-only link does not keep, added back: the regular (not thin) part of the LTO unit, which
   holds vtables (`/lto-obj-path`), and the vector deleting destructors (`??_E`), which this ABI emits as weak
   aliases of the scalar ones (`??_G`) and which no module ends up owning: each becomes `jmp ??_G` in a COMDAT of
   its own, the same code the alias stands for.

A root that is missing shows as an unresolved symbol when Snowlark links, never as wrong code. Whole-program
devirtualization (which the build turns on with ThinLTO) stays off (`prepare.ps1` patches it out): it assumes
every subclass of a V8 class is in the LTO unit, and `streaming.rs` subclasses `ExternalSourceStream` in Rust.
Same V8, same headers, so as for PGO, the access check needs its tests run and nothing re-verified.

Linking all of Snowlark with lld-link instead (ThinLTO in the final link, the same library) measured a further
+0.3% (25.21 against 25.13, below), which lld's function ordering from the profile (`.llvm.call-graph-profile`,
which the MSVC linker ignores) would explain; not worth changing every build's linker for.

### 3c. Pointer compression

Halves the size of every tagged field, so less memory traffic and fewer scavenges: the 15-22% allocation-heavy
gap in section 1 is its territory, worth at most about x1.02 of the score (allocation and V8's GC shares). The
`openworkers-v8` crate publishes prebuilt libraries with it (and with the sandbox); they are a third party's binaries
in the origin boundary, so we would build our own (`v8_enable_pointer_compression=true`, one V8 build). Our code
that relies on its absence: `raw_callback.rs` writes Smis as `value << 32` (31-bit Smis are `value << 1`, and a
value outside 31 bits must become a heap number), `ISOLATE_EMBEDDER_DATA_OFFSET` moves, `wrapper_ref.rs` writes a
`ReturnValue` slot directly; all of `docs/v8-upgrade.md` is re-verified. The V8 heap is limited to 4 GB per isolate
group with it.

Done on 15.8 (2026-11-16, x1.093, section 4), as Chrome builds it without the sandbox: a shared cage, static roots,
the external code space. What changed on our side: on 15.8 an embedder slot with compression is a 32-bit tagged half
and a 32-bit handle into V8's C++ heap pointer table, which V8's marker reads, so the inline fields keep 31-bit Smis
in the tagged half only, and a wrapper's 62-bit handle takes two fields (a node slot is retired at generation 2^30 - 1
so that every handle fits). A 64-bit store across the slot, which V8's own aligned-pointer store did before 15.8, would
put our bits in V8's table handle.

Builtins PGO (the block layout of V8's embedded builtins from V8's own profile) comes only with pointer
compression **and** the sandbox on x64. The sandbox also requires every `ArrayBuffer` backing store to live inside
it: our 7 places that hand Rust-owned memory to V8 (`new_backing_store_from_*` in `snowlark-page`: audio, blob,
encoding, geometry, IndexedDB keys, byte streams) would copy instead. Builtins are 20% of the samples under script
and already at Chrome's speed in section 1 (0-8%), so its worth is small.

That condition is only on V8's own shipped profile (its builtins' code graphs, hashed, differ without the sandbox);
V8 takes any profile made by a build of the same configuration (`v8_builtins_profiling_log_file`). **Done
(2026-11-17): our own builtins' profile**, from Speedometer and three sites run on a V8 built with
`v8_enable_builtins_profiling=true` (section 6), with pointer compression and no sandbox. Builtins are 15.9% of
scored samples (native sampler, all suites: `LoadICGenericBaseline`, the megamorphic load and store ICs, the
baseline prologue and calls on top). mksnapshot takes the profile's branch hints (5,461 over 951 builtins) for the
block layout inside each builtin: x1.006 to x1.011 measured (section 4). Its other use, reordering the builtins by
call graph, is off: measured (2026-11-17, with a patch that lets it link, section 6) at x0.995, 17 of 20 suites
slower, so not taken.

### 3d. A newer V8

Chrome stable is 158 (V8 15.8) today; ours is 15.2, six releases behind. The `v8` crate has not rolled since
15.2.124.1 (2026-08-01; its last release, 152.2.0, is 2026-08-20; the automatic-roll pull request has been open
since August). A newer V8 therefore means rolling the crate ourselves: its C++ binding layer (`src/binding.cc`) and
Rust API follow V8's API, which changes every release, so six releases of API changes to port, then everything in
`docs/v8-upgrade.md` re-verified (mangled names of the script-streaming, code-cache, access-check, `Object::Clone` and
global-handle entry points, the hand-laid structs, the callback-info indices, the isolate data offset) and
`VERIFIED_V8` moved. Its worth is what six releases of V8 work give our workload, unmeasured: the JIT tiers are
already at Chrome's speed on section 1's tests, so it would come mainly from the C++ side (parser, JSON, ICs, GC),
the same 1.4x that 3b goes after. Cost: days, and a fork of the crate to keep up after that.

**Done on a branch (2026-11-16): the crate's own roll.** The crate's maintainers have since rolled it to 15.8
themselves (`v8` 158.0.0, V8 15.8.62.23: pull request 2098, one commit, `d2bc4ef3`, CI green on every target
including Windows x64; not merged or published yet). Porting six releases by hand would redo that, so we take it
as is. What it changes for us: synthetic module evaluation steps must return a promise (`hooks.rs`
`synthetic_steps`), and nothing else we call (`cppgc::Heap::terminate` is gone; we do not use it). Re-verified
against 15.8 as `docs/v8-upgrade.md` says: every `link_name` is in the library under the same mangled name; the
interceptor configuration structs, their callback typedefs, `FunctionCallbackInfo`'s indices and the isolate's
embedder data offset are unchanged; `ExternalSourceStream`, `StreamedSource`, `CachedData` and the code-cache
task are unchanged; `Isolate::MayAccess` and `ReportFailedAccessCheck` are unchanged (15.8 adds two checked
callers, the error `stack` accessors). `snowlark-js` (35) and `snowlark-page cross_origin::` (44) pass on it.
One behaviour change reached a page: 15.8's `Function` constructor hands each object argument to the string
compilation hook rather than converting it with `toString`, which broke how we recognised a `Function` made of
TrustedScripts; TrustedScript is now marked code like (`SetCodeLike`, by its mangled name in `hooks.rs`) and the
hook trusts what V8 marks `is_code_like`. With the control library (15.8, no PGO or ThinLTO): the WPT
gate against main has no loss of ours (two files it flagged behave the same on main's build), the
workspace tests pass but for known load flakes, and `--render` of Wikipedia, a YouTube watch page and the
Guardian is normal.

**Speed, with the 15.8 PGO + ThinLTO library (section 6): done 2026-11-16, x1.014 on Speedometer** (section 4).
15.8 alone tied 15.2: its JavaScript is faster (`js/megamorphic` -27%, `js/json-roundtrip` -11%, `js/object-props`
-9%, `js/string-ops` -7%, `js/vdom-diff` -7%), but DOM bindings got slower (`bindings/child-nodes-iterate` +12%,
`dom/tree-walk-1000` +16%, `events/add-remove-listener-100` +14%, `dom/append-remove-100` +9%). The cause is 15.8's
internal fields: `SetAlignedPointerInInternalField` no longer stores the pointer in the object but allocates a
record on V8's C++ heap for each store and keeps a reference to it, and `GetAlignedPointerFromInternalField` lost
its inline read (it finds the current isolate and goes through two indirections, out of line). Every binding call
reads its receiver's field and every wrapper made writes two. `crates/snowlark-js/src/internal_field.rs` now keeps
the wrapper's handle and prototype id where V8 15.2 kept aligned pointers itself, as Smis in the field's tagged
word, read and written inline at V8's `Internals` offsets for API objects (any other object goes through
`Get/SetInternalField` with the same Smi), and checks the layout against V8 in every isolate the bindings install.
That removed the binding loss (child-nodes +0.9%, tree walk -6.4%) and kept the JavaScript gains.

What 15.8 still costs, measured: `events/add-remove-listener-100` stays +7% (listener bookkeeping inside V8, not
the fields), and `selectors/qsa-complex` is +10-11% in both orders of an A/B. The latter's time is all in our Rust
selector matching (`matches_complex_selector_internal` +5%, `matches_generic_nth_child` +28% in a sampling profile
of 100,000 calls), code that is the same in both builds and touches no V8 object: a code placement effect of the
different link, not V8's work (`selectors/matches-200` moves -21% the other way, the same kind of effect), and within the micro gate's threshold against main's stored baseline. Not tried:
whether 15.2 gains from the same inline fields (the field read there is already inline in V8's header; we would
save only the crate's out-of-line C call), moot now that 15.8 is in.

How the fork is kept: the crate's Rust side (manifest, `build.rs`, `src/`: 1.3 MB) is vendored in `vendor/v8`
like the other patched crates, because that is all cargo compiles once it is handed a library, and it makes the
repository alone enough for cargo. The rejected ways: a cargo git dependency clones V8 and Chromium's
dependencies with full history on every new machine (gigabytes, for a build that never reads them); a crate in a
folder outside the repository is not in it at all; a submodule needs a remote fork. The V8 library is built from the same
upstream commit by `tools/v8-pgo` (section 6) and pinned by `tools/v8-pgo/pin.txt`, as for 15.2. When 158.x is
published, `vendor/v8` goes.

### 3e. Binding-side costs

Most of the "script" gap is not V8: under a JS call, 53% of the samples are our own code (DOM operations, wrappers,
the engine work they start). These are DOM and bindings work (wrappers made and found, the
`HTMLCollection` interceptor, `innerHTML`, forced style and layout), not V8's.

## 4. Measurements of the build options

**Control.** V8 15.2.124.1 built from the crate's own sources at tag v152.2.0 with the crate's arguments
(section 6) times the same as the prebuilt library on every `js/` test (within noise; e.g. compile-functions
0.1931 against 0.1934 ms, json-roundtrip 0.0961 against 0.0951). So a library of ours starts where the prebuilt one
is, and anything below is the profile's doing.

**PGO (3b), 2026-11-15/16**, release builds of the same tree, one with the control library and one with the
PGO one, on an idle machine:

| Speedometer 3.1, 10 iterations, alternating | runs | mean |
|---|---|---:|
| control | 22.40, 22.32, 22.02, 22.06 | 22.20 |
| PGO | 23.03, 23.00, 22.55, 23.00 | **22.89 (+3.1%)** |

Every PGO run is above every control run. Micro benchmarks (`--runs 3`, PGO against the control as baseline):
0 losses, 6 gains: `js/json-roundtrip` -25.7%, `js/compile-functions` -22.5%, `bindings/child-nodes-iterate`
-14.8%, `selectors/qs-class-last` -13.9%, `events/dispatch-bubble-deep` -10.6%, `paint/canvas-fill-rect-1000`
-10.2%; the `js/` tests against Chrome go from 1.19 to 1.12-1.16 in geometric mean (two runs; JSON 1.41 to 1.05,
compile 1.40 to 1.09-1.15), the 67 tests from 1.32 to 1.29. Against the newest stored baseline
the PGO build has 0 losses and 2 gains; the control has 1 loss (`selectors/qs-class-last`), which PGO removes. Only the megamorphic test does not move (1.45-1.46): its time is in
the stub cache's generated code, not in C++.

The profile came from a 2-minute training run (Speedometer 3.1 twice, the micro benchmarks, Wikipedia, a YouTube
watch page, the BBC's news page); it covers what the browser does on pages, not only Speedometer's suites, and
the micro tests and renders that gained were in it as well. A profile tuned to one benchmark could overfit it; the
gains in C++ that every page runs (the parser, JSON, IC misses) are not of that kind.

**ThinLTO on top of PGO (3b'), 2026-11-16**, release builds of main, base = the PGO library
(`152.2.0-pgo1`), alternating, 10 iterations each, on an idle machine (faster than the night before):

| Speedometer 3.1 | runs | mean |
|---|---|---:|
| base (PGO) | 24.53, 24.91, 24.74, 24.98 / 24.74, 24.93, 24.95, 25.01 | 24.79 / 24.91 |
| PGO + ThinLTO, native library | 24.86, 24.95, 25.01, 25.08 / 25.05, 25.08, 25.13, 25.27 | **24.97 / 25.13 (+0.7% / +0.9%)** |
| PGO + ThinLTO, all of Snowlark linked by lld-link | 25.21, 25.13, 25.20, 25.29 (second set) | 25.21 (+1.2%) |

Every ThinLTO run is above the base run before it. Micro benchmarks (`--runs 3`, against the base): 0 losses, 2
gains (`js/compile-functions` -11.6%, `events/add-remove-listener-100` -11.9%).

**V8 15.8 (3d), 2026-11-16**, release builds, base = main (15.2 PGO + ThinLTO), new = the same tree on
15.8 with its PGO + ThinLTO library (`158.0.0-pgo1-lto2`, profile from the same training run on 15.8), alternating,
10 iterations each, on an idle machine (browserbench.org's 3.1):

| Speedometer 3.1 | runs | mean |
|---|---|---:|
| 15.8 alone, base / new | 4 pairs | 24.74 / 24.80 (+0.3%, noise) |
| base (15.2) | 25.00, 25.10, 25.26, 24.81 | 25.04 |
| 15.8 + inline wrapper fields | 25.51, 25.42, 25.25, 25.37 | **25.39 (+1.4%)** |

Three pairs of four are clearly above (the third ties, 25.25 against 25.26); Welch's t is about 3. An earlier set
under outside load had its two clean pairs at +0.9%. Suites that gained most: Lit -8.7%, CodeMirror -7.1%,
Backbone -6.1%, Preact -4.6%, Stockcharts -4.6%; lost: ES5 TodoMVC +10%, TipTap +8%. Micro benchmarks (`--runs 3`,
against the base): 1 loss (`selectors/qsa-complex`, section 3d), 3 gains (`js/megamorphic` -27%,
`js/json-roundtrip` -10%, `selectors/matches-200` -21%); against main's stored baseline: 0 losses, 2 gains.

**Pointer compression on 15.8 (3c), 2026-11-16**, release builds, base = main (15.8 PGO + ThinLTO,
`158.0.0-pgo1-lto2`), new = the same tree with pointer compression (`158.0.0-ptrcomp-pgo1-lto2b`: a new profile from
the same training run on the compressed build, ThinLTO, PartitionAlloc off), alternating, 10 iterations each, the
local 3.1 mirror, on an idle machine:

| Speedometer 3.1 | runs | mean |
|---|---|---:|
| base | 26.11, 26.16, 26.49, 26.23 | 26.25 |
| pointer compression | 28.56, 28.46, 28.82, 28.95 | **28.70 (+9.3%)** |

Every new run is above every base run; a first set of four pairs, which another benchmark overlapped,
gave 25.69 against 27.98 (+8.9%). The profile changed with the build, so the two are not measured apart; PGO as a
whole was worth 3.1%, and the micro tests that moved are the allocation-heavy ones section 1 named: micro benchmarks
(`--runs 3`, against the base build): 0 losses, 2 gains, `js/allocation` -40%, `js/vdom-diff` -21%; below the
threshold, `bindings/node-getters-100` +6.9% (a wrapper's handle is now read from two fields). Against main's stored
baseline: 1 loss, `selectors/qsa-complex` +12%, which main's own build has too (-1.2% between them).

**The builtins' profile (3c), 2026-11-17.** Release builds, base = main (`158.0.0-ptrcomp-pgo1-lto2b`), new
= the same tree on `158.0.0-ptrcomp-pgo1-lto2b-bpgo2` (the same C++ profile and ThinLTO, plus the builtins' branch
hints), alternating, 10 pairs of 10 iterations, the local 3.1 mirror, idle machine:

| | runs | mean of run scores | mean of run medians |
|---|---:|---:|---:|
| base | 10 | 28.66 | 29.27 |
| builtins' profile | 10 | 28.84 (+0.6%) | **29.57 (+1.0%)** |

The new run's median is above its pair's in all 10 pairs (x1.011 on average). Two new runs each had one iteration
in which nearly every suite took 2-2.5 times as long at once (19.8 and 21.5), which no base run had; the suites in
it have nothing in common but the machine, so the medians are the measure here. Micro benchmarks (`--runs 3`,
against the base build): 0 losses, 1 gain (`js/class-calls` -10.6%; `js/megamorphic` -8.7% below the threshold).

## 5. Recommendation

1. **Done: PGO of V8's C++ (3b).** x1.031 measured, no change to V8's version, flags or behaviour, no
   re-verification beyond the access check's tests, and nothing for other builds to do: `dev.ps1` links the
   library when the machine has it (`tools/v8-pgo/pin.txt`).
2. **Done: ThinLTO with PGO (3b')**, as a native library: x1.008 more, the same nothing for other builds to do.
   A further x1.003 needs every build linked by lld-link; not taken.
3. **Done: V8 15.8 (3d)**, Chrome stable's, from the crate's own roll, vendored: x1.014, with the wrapper
   fields kept inline (without them it ties 15.2).
4. **Done: pointer compression without the sandbox (3c)**, on 15.8: x1.093 measured (with its own profile), far
   above the x1.02 bounded from allocation and GC shares alone. `internal_field.rs` keeps 31-bit Smis in the
   tagged half of a slot (the other half is V8's C++ heap pointer table handle) and a wrapper's handle in two
   fields; `raw_callback.rs` stores 31-bit Smis in the return slot's low half; `wrapper_ref.rs` unchanged (a full
   address is what V8 stores too, with static roots). The heap is one 4 GB cage per process.
5. **Done: our own builtins' profile (3c)**: branch hints for the embedded builtins, x1.006-x1.011. Their
   reordering by call graph, measured with a patch to V8's unwind table: x0.995, not taken. Flags for job
   finalization and lazy compile (3a): nothing to gain. The crate's typed array and promise hook build arguments
   (3a): under x1.002 each. Nothing left on V8's side measures above that (3a, "Where V8's scored time is now").
6. **The larger part of the script gap is ours (3e):** DOM, bindings and the work they start, in the page and dom
   items.

## 6. Building our V8

The library is the v8 crate's V8 at the crate's tag, built by the crate's own build script (`V8_FROM_SOURCE=1`)
through `dev.ps1`, then linked into every build with `RUSTY_V8_ARCHIVE` (and the bindings the build generated,
`RUSTY_V8_SRC_BINDING_PATH`). The scripts are in `tools/v8-pgo/` (`<dev>` is the folder `SNOWLARK_DEV_DIR` names):

```powershell
.\tools\v8-pgo\prepare.ps1 -Work <dev>\v8-pgo -Tag v152.2.0
.\tools\v8-pgo\build-v8.ps1 -Work <dev>\v8-pgo -Name instrumented -GnArgs 'chrome_pgo_phase=1'
.\tools\v8-pgo\build-cli.ps1 -Work <dev>\v8-pgo -Name instrumented -Instrumented
.\tools\v8-pgo\train.ps1 -Work <dev>\v8-pgo -Name instrumented
.\tools\v8-pgo\build-v8.ps1 -Work <dev>\v8-pgo -Name pgo -GnArgs 'chrome_pgo_phase=2 pgo_data_path="<dev>/v8-pgo/snowlark.profdata" use_thin_lto=true'
python tools\v8-pgo\thinlto-native.py --lib <dev>\v8-pgo\libs\pgo\rusty_v8.lib --clang <dev>\v8-pgo\target-pgo\iter\clang --work <dev>\v8-pgo\native --roots <dev>\v8-pgo\rusty_v8\src --roots crates --roots <dev>\v8-pgo\libs\pgo\src_binding.rs --out <dev>\v8-pgo\libs\pgo\native\rusty_v8.lib --jobs 8
```

(The last step takes about 2 minutes: the thin link 8 s, 1,271 backends in 1-2 minutes at 8 jobs. The library
pinned then, `152.2.0-pgo1-lto2`, was made from an earlier checkout and profile with exactly these two steps; the ThinLTO build
took 25 minutes. Copy `src_binding.rs` next to the native library.)

(The library pinned then was made by these steps run one at a time on 2026-11-15; the scripts put them together
and only `prepare.ps1` has been run as a script. Expect to fix a path the first time.)
Then copy the native library (`<Work>\libs\pgo\native\rusty_v8.lib`), `src_binding.rs` and the profile to a new
`<dev>\v8\<crate version>-pgo<n>-lto\` folder (a new name each time: the v8 crate copies the library into its
build folder and copies it again only when the path changes), and point `tools/v8-pgo/pin.txt` at it with the library's
SHA-256 (`Get-FileHash`). A control (`-Name control`, no GN arguments) is worth building once per version to
time the profile against. On the test machine: V8 builds take 22 minutes (33 instrumented) at 8 jobs, the
instrumented CLI 4, training 2.

What `prepare.ps1` works around, so the next version knows:

- The crate on crates.io leaves out files a build from source needs (ICU's `icudtl.dat`, vendored crates' build
  scripts), so it clones the repository with its submodules; `tools/win` is a submodule its repository does not
  update (`update = none`) and comes from the pinned commit's archive.
- The build pins Windows SDK 10.0.26100 and the Windows 11 24H2 target; the test machine has SDK 10.0.22621, so both
  are set to that SDK (its headers only; V8's code does not change).
- The PGO configuration lists the browser's profile pin (`chrome/build/win64.pgo.txt`) as an input even when
  `pgo_data_path` names ours; a placeholder file satisfies it.
- An instrumented build writes its profile at exit, but the process ends through `ExitProcess`, which skips the C
  runtime's exit handlers: `snowlark_js::write_pgo_profile` (feature `pgo-train`) writes it first, and the
  instrumented library needs the profile runtime (`clang_rt.profile-x86_64.lib`) on the link path.

15.8's clang emits a deleting destructor's calls to the global delete helpers (`__global_delete`,
`__global_array_delete`) as weak externals that default to `__empty_global_delete`; an index-only link leaves them
undefined, so `thinlto-native.py` defines them as jumps to it, as it does the vector deleting destructors' (what a
link without ThinLTO resolves them to).

With pointer compression (the workspace's `v8_enable_pointer_compression`, which `build-v8.ps1` picks up through
`-p v8`), V8's build turns PartitionAlloc on by default for its standalone shell, and its malloc shim then defines
`malloc`, `free` and `_msize` in the library: the link fails against the C runtime (LNK2005), and where it does not,
the process's allocator changes. `build-v8.ps1` passes `v8_enable_partition_alloc=false`, as an embedder's build has.
The pinned library `158.0.0-ptrcomp-pgo1-lto2b` was made with exactly the commands above; its instrumented build had PartitionAlloc
still in, which only leaves the profile with entries for functions the final build does not have.

**The builtins' profile** (3c) is made by a V8 build with `v8_enable_builtins_profiling=true` (no C++ profile or
ThinLTO needed: only the builtins' code graphs must match, and mksnapshot stops the final build on any builtin whose
hash differs, `--abort-on-bad-builtin-profile-data`), a CLI on it, and a training run; the final library then takes
the hints with the C++ profile and ThinLTO as above:

```powershell
.\tools\v8-pgo\build-v8.ps1 -Work <dev>\v8\bprof-work -Name bprof -GnArgs 'v8_enable_builtins_profiling=true' -Src <dev>\v8\rusty-v8-roll
.\tools\v8-pgo\build-cli.ps1 -Work <dev>\v8\bprof-work -Name bprof
.\tools\v8-pgo\train-builtins.ps1 -Work <dev>\v8\bprof-work -Name bprof -Src <dev>\v8\rusty-v8-roll
.\tools\v8-pgo\build-v8.ps1 -Work <dev>\v8\bprof-work -Name bpgo2 -GnArgs 'chrome_pgo_phase=2 pgo_data_path="<dev>/v8/pgo-work/snowlark.profdata" use_thin_lto=true v8_builtins_profiling_log_file="/<dev>/v8/bprof-work/builtins.profile" v8_enable_builtins_reordering=false' -Src <dev>\v8\rusty-v8-roll
```

then `thinlto-native.py` on `libs\bpgo2` as above. The profiling build takes 36 minutes, the CLI 6, the training 2 (a
log per process; V8 writes it as the isolate is disposed and never closes the file, so the process's exit loses the
last buffer and the script keeps only builtins whose data is complete), the final build 37. A path in GN arguments
that names a source file is written `/E:/...`.

`v8_enable_builtins_reordering=false`: with reordering (V8's default once a profile is given), the builtins are laid
out in a new order but V8's Windows unwind table for them (`.pdata`, `platform-embedded-file-writer-win.cc`) is still
written in builtin id order with each entry clamped to start after the previous builtin's end, so entries come out
unsorted and some end before they start; MSVC's linker refuses the object (LNK1223, "invalid .pdata contributions").
`tools/v8-pgo/patches/v8-pdata-by-address.patch` writes them in address order (and starts an entry for code before a
frame no lower than the blob's start: without it the first entry begins 4 bytes before the blob, in every build).
With it a reordered library links and runs, but measured slower (x0.995, 2026-11-17), so the
build does not apply it and the pinned library keeps `v8_enable_builtins_reordering=false`. To try again (a new
profile, a new V8): `git -C <checkout>\v8 apply -p2 tools\v8-pgo\patches\v8-pdata-by-address.patch`, drop that GN
argument, then `thinlto-native.py` and a pin as above. `embedded.S`'s `.pdata` section in the build's `gen\v8` shows
the entries.

A new V8 version changes the library: everything in `docs/v8-upgrade.md` applies to it as to the crate's
prebuilt one. The same version with a new profile changes no symbol, struct or offset: run the access check tests
(`access_check`, `cross_origin::`) and the gates.
