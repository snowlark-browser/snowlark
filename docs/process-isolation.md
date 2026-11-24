# Snowlark: from threads to processes

Design of the multi-process browser, and the state of each milestone. Last revised 2026-11-19. Paths are relative to
`crates/` unless they name a top-level file. `docs/process-isolation-p0.md` holds the results of the first prototype
(presenting and video across processes).

## 1. Goals

- A crash, a stack overflow or a hang in one page stays in that page: the tab shows a crash page, can be killed, and
  Back, Forward and Reload keep working.
- Memory is accounted per tab, exactly.
- A page's code cannot reach other sites' data: renderers are locked to one site, and the browser treats every
  message from a renderer as hostile.
- Hostile bytes (fonts, images, media, display lists) are parsed in processes that can be sandboxed.
- The headless modes (`--wpt`, `--render`, `--eval`, `--reftest`, `--speedometer`) stay single-process and
  deterministic, so conformance and benchmark results do not depend on the process model.

## 2. The process model

| Process | Holds | Sandbox | State |
|---|---|---|---|
| **Browser** | Window, shell, session history, profile, navigation decisions, the security policy tables, the network thread (`snowlark-net`), the storage threads (cookies, Web Storage, IndexedDB, blobs) | None | Exists |
| **Renderer**, one per (tab, site), plus one warm spare | V8, DOM, style, layout, text shaping, image decoding, display list building | Job object today; restricted token planned | On by default |
| **GPU** | The Direct3D device, raster threads, tile caches, the compositor, video textures, canvas (planned) | Planned: medium | Behind `--gpu-process` |
| **Media**, one per renderer that plays media | Media Foundation: demuxing, decoding, audio | Planned: medium | Not started |
| Network, storage | Threads in the browser process | n/a | Optional, probably never needed for security |

`--process-model=single` (or `SNOWLARK_PROCESS_MODEL=single`) keeps every tab on a thread of the browser process;
`tab` (the default in the window) runs each tab in a renderer process. Only the window has a process model: the
headless modes run their page in their own process.

### Decisions

1. **Renderers send display lists, never pixels.** A renderer does not rasterize and has no GPU device. Raster
   threads and the compositor stay together on one D3D device, so tiles never cross a process boundary. Shared NT
   handles are needed for video only. `DisplayList::encode`/`decode` (`snowlark-paint/src/display_list.rs`) have a
   validated, total decoder for this.
2. **A renderer is locked to one site and every message from it is hostile.** The browser owns the table of
   processes, their site locks and their frames. Renderers never name an origin to get access; they name a frame
   and the browser looks up what that frame may do. The unit is the site (scheme plus registrable domain,
   `snowlark_net::origin::site_of`), not the origin: same-site documents can script each other.
3. **Network and storage stay in the browser process as threads.** They are memory-safe Rust, and keeping them
   there puts the policy check, the cookie jar and the sockets in one place. A separate network process would buy
   crash isolation only. The service traits keep the option open.
4. **One renderer per (tab, site).** Per tab alone leaves the previous site's data in the process after a
   cross-site navigation; per site alone would let one crash or hang take every tab of the site and loses exact
   per-tab memory. A renderer can host several page threads, since a `Page` is per thread, so same-site tabs can
   share a process under memory pressure without a new design.
5. **Sandbox before site isolation for frames, and GPU process before the sandbox of the renderer's font path.**
   A sandbox turns one exploit into a chain of two; the GPU process takes DirectWrite and Direct2D on hostile input
   out of the browser process.
6. **Cross-origin iframes out of process come last.** They are the largest engine change: frames share one `Dom`,
   one engine and one isolate today.

## 3. What the engine already provides

- A `Page` is `!Send` and lives on one thread with one isolate. A tab reaches the rest of the browser through a
  small context object (`TabContext`) and process statics.
- The window never waits for a tab. Commands and events between them are already asynchronous; synchronous reach
  exists only from the page thread to services.
- Display lists are plain data with an encoder, a validating decoder and corruption tests. Fonts and images are
  referred to by id.
- `ResponseStream` hides the network: a page sees a stream of `FetchEvent`s. IndexedDB's backend, `BroadcastHub`,
  workers and the developer tools already talk in messages.
- Hidden tabs paint nothing, and a frame identical to the last one is neither rastered nor sent.

## 4. Services and synchronous APIs

Everything a tab shares or reaches sits behind a message-shaped trait: `NetBackend`, cookies, blobs, Web Storage,
IndexedDB (`IdbService`), `BroadcastBackend`, `CodeCacheStore`, clipboard, internal pages, content filter, audio and
video. Each has a local implementation (today's code, used by the headless modes and `single`) and a remote one
(a client in the renderer and a host in the browser).

Rule: **the page thread may block on the browser; the browser never blocks on a renderer.** A blocked page thread
runs no nested message loop, so there is no re-entrancy, and each synchronous request is answered on a service
thread, never on the window thread.

| API | Design |
|---|---|
| `document.cookie` read | The renderer caches the last string per URL with a version; the browser bumps a counter in a page shared with that renderer whenever a cookie that site can see changes. A hit is an atomic load, a miss one round trip |
| `document.cookie` write | Applied to the cache at once and sent asynchronously on the same ordered pipe as fetches, so a later `fetch()` sees it |
| `localStorage`, `sessionStorage` | The area is cached in the renderer; writes apply locally and go asynchronously; the browser fans them out to other renderers of the origin, which fire `storage`. `sessionStorage` namespaces are held by the browser so they survive a process swap. Across processes this is eventually consistent, as in every engine |
| Synchronous XHR | Unchanged: the renderer's IO thread feeds the stream while the page thread blocks |
| Layout reads | Unchanged: style, layout and shaping stay in the renderer |
| `URL.createObjectURL` | The renderer picks the UUID and sends it asynchronously, ordered before any fetch of it; the browser checks the origin |
| IndexedDB | Requests are messages sent in order without waiting; answers arrive as tasks in request order; a renderer names its transactions with an increasing counter, and a lower id kills the renderer. Across a renderer pipe 2,000 `put`s went from 26 to 4.3 microseconds each |
| Clipboard, internal pages, content filter | Asynchronous messages or calls answered by bulk data; the browser checks user activation |

## 5. IPC and the trust boundary

**Transport** (`snowlark-ipc`). One duplex overlapped named pipe per child, created by the browser with a random
128-bit name, `FILE_FLAG_FIRST_PIPE_INSTANCE`, `PIPE_REJECT_REMOTE_CLIENTS` and a DACL for the current user. The
child inherits only its end, through an explicit `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`; it never opens anything by
name. Bulk data (display lists, image pixels, response bodies) goes in pagefile-backed sections: inline up to 64 KiB,
otherwise a section; a pipe frame holds 1 MiB. **The privileged side creates every section** and duplicates it into
the child (`AllocShared`, counted against a quota), so there is no validation of renderer-made handles and shared
memory is counted exactly. A child never holds a handle to a more privileged process.
Renderer-to-GPU pipes ("links") are made by the browser, which keeps nothing of them; nothing that names a handle
or a section crosses one.

**Wire format.** Plain Rust enums per interface, `serde`, `postcard`; one binary at both ends with a build id checked
at connect, so there is no versioning. Display lists keep their own codec. Requirements:

- Decoding is total: a bad message is an `Err`, never a panic, and allocation is bounded by the bytes present.
- `Untrusted<T>`: everything off a renderer's pipe arrives wrapped, and the only way to a `T` is `validate`, which
  takes the sender's `ProcessPolicy`. Forgetting a check fails to compile.
- Shared memory is read once: copy to private memory, then decode.
- A validation failure kills the process and logs it (`bad_message`); there is no recovery. Pipes apply
  back-pressure.
- Every decoder is fuzzed (`snowlark-ipc/fuzz`: 91 million inputs in an hour without findings, after a planted bug
  showed the fuzzer reaches the handlers) and every message variant has a mutation test.

**What the browser validates.** Requests name a frame token and the browser resolves it; a renderer's own word about
its origin is ignored. The request's client fields, `Sec-Fetch-Site` inputs and user activation come from the
browser. File and `snowlark:` URLs are fetched only by processes locked to those schemes. Storage, IndexedDB,
broadcast and code cache keys are computed from the renderer's lock, not sent by it. The address bar shows what the
browser committed, not what the renderer reports. Settings changes are accepted only from the browser's own pages'
renderer. Checks that protect other sites (`X-Frame-Options`, `frame-ancestors`, opaque response blocking) run on
the browser side. Checks that protect a document from itself (its own CSP) stay in the renderer. Two kinds of
failure: a forged origin or key is something no honest renderer sends, so the sender dies; a URL of a scheme the
renderer may not have is something an honest page can ask for, so it is refused with a network error and the
renderer lives.

An `--evil-renderer` test suite (`snowlark-tab/src/evil.rs`) plays thirteen forgeries a compromised renderer might
send: ten end the renderer and three are refused with it alive. The shadow mode of the lock (`SNOWLARK_SITE_LOCK=shadow`
logs instead of killing) was run over the whole WPT suite with each test page's services across a pipe
(`SNOWLARK_WPT_SITE_LOCK=<file>`): 3 failed checks in 13,405 files, all an honest `file:` request that is expected to
fail, which is why that case is a refusal and not a kill.

## 6. Rendering across processes

| Stage | Process |
|---|---|
| Build the display list, split layers, hash, skip if unchanged | Renderer page thread |
| Encode | Renderer encode thread |
| Raster threads, tile pool, retained tiles, compositor, present, device | Browser; GPU process when enabled |
| Chrome list (tabs, address bar) | Browser, sent to the compositor like any list |

**One frame.** The page thread hands the paint job to an encode thread by move, as it hands it to the raster thread
in one process, so the page thread does nothing more (page-thread time is what Speedometer measures). The encode
thread first sends the resources the list needs, then the list. The raster thread takes the newest job, copies it to
private memory, decodes, validates and rasters; a fast renderer overwrites a job not yet read and cannot queue.
Encoding 8,500 items (1.9 MB) takes 0.36 ms and decoding 0.55 to 1.0 ms; 85,000 items (19 MB) take 3.3 and 5.2 to
8.0 ms (best of 30, release).

**Fonts.** Glyph runs carry a renderer-local font id. The raster side keeps a font table per renderer: a system font
is named by file path and last-write time and opened only if the path is exactly one DirectWrite reports for the
system font set; a web font arrives as the bytes OTS made of it in the renderer, and the table runs them
through OTS again before DirectWrite reads them (`snowlark-paint` `web_font`). Every installed family passes
through this check.

**Images** stay in the renderer's `ImageStore`; before a list uses an image for the first time the encode thread
sends its pixels (size and bounds checked against the section), and the raster side uploads and caches them by id,
frame and version.

**Presenting.** The browser keeps the window handle and a DirectComposition visual tree with no Direct3D device; the
GPU process presents chrome and content together in one swap chain through a composition surface handle (see
`docs/process-isolation-p0.md`: two swap chains cannot avoid chrome and content showing at different sizes during a
resize, in any process model). Resize keeps its acknowledgment, a wait with a 100 ms limit that also ends the moment
the child dies.

**Video.** Textures are made by the compositor's device with shared NT handles and keyed mutexes (four per video and
size); the player's process opens them. The renderer copies each frame into a private texture and then, under the
key, into a free pool texture and names it by index; the compositor takes the newest frame and gives back the one it
drew before. Holding the key around `CopyFrameToVideoSurface` itself makes the call fail. Measured in P0:
1080p60 for ten minutes with no torn or lost frame.

**Canvas.** A sandboxed renderer has no D3D device, so canvas falls back to the software backend (tiny-skia) there;
the planned end state remotes the device-space draw calls into the GPU process, which would also remove the per-frame
readback and re-upload.

## 7. Sandbox

The plan follows the published design of Chromium's Windows sandbox. In order:

1. Job object: kill on close, active process limit, UI restrictions, commit limit, die on unhandled exception
   (without that flag Windows Error Reporting held an overflowed renderer for more than 20 seconds). Done.
2. Mitigations that need nothing else: DEP, forced ASLR, strict handle checks, extension points disabled, no remote
   image loads. Done.
3. Restricted token with low or untrusted integrity and a warm-up phase: a spare renderer starts on a more capable
   token, creates what must exist (text engine, COM, the imaging factory, the random generator) and then reverts.
   Not started.
4. Alternate desktop; control-flow guard and CET in Rust (a few percent at most; measure Speedometer).
5. win32k lockdown. `snowlark-paint::window` links user32, so either delay-load it or build a second renderer
   executable without the window code; DirectWrite and the imaging components may also touch gdi32.
6. AppContainer or LPAC, only if the earlier steps leave a reason.

Open questions that a sandbox prototype must settle: whether DirectWrite's shared factory and system font collection
keep working after the token drops (fallbacks: map every font file read-only during warm-up, or a font broker);
whether WIC works under the token (fallback: pure Rust decoders, which also put hostile bytes in memory-safe code);
what Media Foundation needs, which is why media gets its own process (low integrity, less restricted token, one per
renderer so a compromised decoder sees one site's media). Arbitrary Code Guard cannot coexist with V8's JIT, so it
could only be an opt-in strict mode that disables the JIT for chosen sites.

**Self-test.** `snowlark --sandbox-selftest` is planned to launch a renderer that tries to read a file in the
profile, write to the temp folder, open a socket, start a process, open the browser's process handle, read the
registry and send a window message; each must fail. It belongs in the merge gate once the sandbox exists.

Until the GPU process exists and the renderer is sandboxed, web font bytes from the renderer are parsed on the raster
side, which is the unsandboxed browser process. DirectWrite there reads only what OTS rewrote: the outline, metric,
naming and hinting tables parsed and written out again, unknown tables dropped. The color tables, the OpenType
layout tables (`GSUB`, `GPOS`, `GDEF`, `BASE`) and the variation tables pass through unchanged, as in other
engines, because OTS's checks of them refuse fonts in wide use. What remains exposed is those tables, read by
DirectWrite, and OTS's own parsing, which is fuzzed continuously upstream. Moving
it and DirectWrite's into a sandboxed process is still the reason the GPU process comes before the renderer
sandbox.

## 8. Site isolation

A page can read memory in its own process through speculative execution; this cannot be fixed in software inside
one address space. Process per site confines what can be read to the same site, and cross-origin iframes need
out-of-process frames for the same reason; only then can `SharedArrayBuffer` return, with COOP and COEP.

**Navigation** begins in the browser: the tab host makes the request, follows redirects, chooses the process by the
final site and hands the response to the renderer to commit. A cross-site navigation swaps the process (taking the
spare, which is locked at its first commit) and session history stays in the browser, so it survives swaps and
crashes. Subframe navigations stay in the parent's process until out-of-process frames exist, so the lock has to
admit those sites; this is a documented gap.

**Frames (planned).** The browser holds the true frame tree per tab; each renderer has local frames and proxies.
A cross-origin `WindowProxy` exposes only the `CrossOriginProperties`, built with V8 access checks over an object
that holds only a frame token. This is done in process (see C1 below), so remote frames take the same shape.
`postMessage` is serialized by the sender; the browser stamps the true origin and source frame. Compositing adds an
`Item::RemoteFrame` that the compositor draws as a quad with its own tiles, as it does video; the parent never
receives child pixels. Input goes to the top frame's renderer, which reports a hit on a remote frame, and the
browser forwards only real input. Geometry, `IntersectionObserver` and `load` become asynchronous across the
boundary, as in every isolating engine.

**Auxiliary windows.** A page's windows outlive its tab, and their renderer with them: until frames are out of
process, a window opened with an opener is a view of that page in the opener's renderer. When the opener's tab
closes or moves to another renderer, the old renderer is kept until its last view detaches, closes its own
top-level traversable (so the windows see `opener` as null) and keeps running the page for the views. Cross-site
sign-in popups therefore keep working after the opener is gone.

## 9. Crashes and lifecycle

- **Detection.** The browser waits on the process handle and on pipe EOF and classifies the exit code (stack
  overflow `0xC00000FD`, access violation `0xC0000005`, fast fail `0xC0000409`, Rust panic 101). The tab shows a
  crash page with the reason; the previous frame stays on screen until something replaces it.
- **Hangs.** The page thread marks each pass of its loop in a small shared section. Input unacknowledged for 5
  seconds in the visible tab offers Wait or Kill. Kill asks the page's `Terminator` to stop the script and keeps the
  page; one second later, or at a second click, the process is terminated. `while(1){}` is stopped 1.5 ms after the
  kill and its timers go on in the same process; a native hang is ended 1.02 s after one Kill.
- **Dumps.** A crashing renderer's exception filter signals the browser, which writes a minidump from outside and
  ends the child with the original exception code (`MiniDumpWriteDump` reading a compromised renderer's memory is
  attack surface in the browser; to be revisited with the sandbox).
- **Spare renderer.** One warmed renderer (V8, text engine, a first page) waits with no site; a tab or a cross-site
  navigation takes it and a replacement starts at idle priority. Time from `TabHost::spawn` to the first frame:
  7.7 ms in single-process mode, 32.8 ms with a fresh renderer process, 9.8 ms with the spare.
- **Reload** after a crash is manual, never automatic.
- **Memory.** Each renderer's job has a commit cap; the task manager lists each renderer's private bytes, and End task
  ends one tab's process. 1,000 kill-and-reload cycles leave the browser's handles and private bytes as they were
  (the test found and fixed a leak: Direct3D destroys released resources only when the context is flushed, 2.6 MiB
  per cycle).
- **Keepalive fetches** outlive their page and renderer, for at most 30 seconds, in the browser.
- **Shutdown.** Pipes break, children exit, and the job's kill-on-close backs that up.

## 10. Single-process mode and tools

The headless modes use `Page` directly and never touch the tab code, so they run through the local service
implementations, which are today's code behind the traits. Consequences:

- WPT results and headless Speedometer do not depend on the process model.
- `SNOWLARK_IPC=loopback` puts every tab message (`TabCommand`, `TabEvent`, `CompMsg`) and every service call through
  the wire format inside `send`, on the sender's thread, so the window and the shell's tests run as if each crossed a
  process. It does not cover ordering between pipes.
- `snowlark --wpt` with `SNOWLARK_WPT_SITE_LOCK=<file>` runs each test page's services across a pipe to the browser's
  hosts with the site lock set to the test's site, which is how enforcement is checked against the whole suite.
- Profiling and statistics variables work in the window; the profile of a renderer is printed as its tab closes.
- The path from a tab through raster and compositor has no WPT coverage in any mode (`--render` and `--reftest` use a
  headless renderer); `snowlark-tab/src/raster_pixels.rs` compares 96 frames (WARP and the GPU, at 100% and 150%)
  byte for byte against a baseline made before the raster seam, for every change to it.

## 11. Rules for contributors

- No new shared object in `TabContext`, `DriverContext` or `PageOptions` without a message-shaped trait.
- No `std::fs` or OS calls in `snowlark-page`, `snowlark-render`, `snowlark-style` or `snowlark-layout`.
- A new display list `Item` updates encode, decode, `validate` and the fuzz corpus.
- Security decisions about other sites go in `snowlark-net` or the browser half, never in the page crates.
- `unsafe` is allowed in `snowlark-js`, `snowlark-paint`, `snowlark-media/src/win`, `snowlark-ipc` and
  `snowlark-sandbox`, with a `// SAFETY:` comment.

## 12. Milestones and status

| | Milestone | Status |
|---|---|---|
| P0 | Prototype: presenting and video across processes | Done 2026-11-06 |
| M0 | Seams: plain-data tab protocol, tab split into host and renderer halves, crate moves | Done 2026-11-07 |
| M1 | `snowlark-ipc`: transport, sections, handles, loopback | Done 2026-11-06 |
| M2 | Services behind traits | Done (IndexedDB over messages 2026-11-12) |
| M3 | Raster seam: lists as bytes, fonts and images by table | Done 2026-11-07; the shared-section mailbox and some window checks are not |
| M4 | Renderer process per tab, job object | Done; default since 2026-11-10 |
| M5 | Browser-owned navigation, process per site, site lock, opaque response blocking, evil-renderer suite | Done 2026-11-11 |
| C1 | Cross-origin `Window` and `Location` in process, with V8 access checks | Done 2026-11-06 |
| M7 | GPU process | Stages G1 to G5 done behind `--gpu-process`; G6 (canvas) not started; not yet the default |
| M6 | Renderer sandbox: restricted token, low integrity, warm-up, media process | Not started (the job object and free mitigations from M4 are on) |
| M8 | win32k lockdown, stricter mitigations | Not started |
| M9 | Out-of-process iframes, then auxiliary contexts and COOP/COEP | Not started; the in-process groundwork (auxiliary browsing contexts, access checks) is done |
| M10 | Optional: network or storage out of process | Not planned |

### M0 to M3: seams

`CompMsg` is entirely plain data; the compositor's own channel carries a local `CompInput` for what cannot cross
(`Bind`, `Frame`, `Videos`). `TabCommand::Go`, `Reload` and `Restore` are host calls: the renderer receives
`Navigate { url, restore, navigation, reload }` and echoes the navigation number, so events about an older load are
ignored. Timestamps are `snowlark_paint::Ticks`, the performance counter, valid system-wide. The renderer half lives
in `snowlark-tab`, which depends on neither the shell nor the window code.

M1: 48 tests with a real child (peers that break every framing and protocol rule each die with exit code 9); round
trip p50 21-25 microseconds, p99 48-60 microseconds at 77-99% CPU; 5,000 child starts and kills leave the handle
count at 138-140.

M2: `NetClient` holds an `Arc<dyn NetBackend>` and the page crates no longer call the cookie, blob or config
singletons. Web Storage is now persisted and shared between tabs (WPT `webstorage` went from 4,032 to 4,050 passes).
IndexedDB took three phases: a service trait with the local implementation (WPT `IndexedDB` 3,085 of 3,128 before and
after), requests as ordered messages, and generated keys filled in by the backend so no `add()` waits for its key.

M3: windowed Speedometer changed by -0.4%, inside noise.

### M4: renderer process per tab

`snowlark --type=renderer` runs the tab renderer; the browser starts one per tab or takes the spare. Done in steps:
the raster seam as messages (every message round-trips, every truncation is refused, 12,000 mutated messages are
refused or well formed), remote forms of every service (each tested against a real child with forged messages that
must kill it), the renderer process itself, video, then lifecycle (hangs, kill, crash classification, the spare,
1,000-cycle test, keepalive fetches, minidumps). A renderer starts making its canvas device as it starts, because the
process's first Direct3D device costs about 150 ms and its first draw waits on the driver's shader compilation; it
costs about 51 MB more private memory per renderer, which a GPU process would remove. Windowed Speedometer, 6 rounds:
single-process 19.35 +- 0.19, tab 19.69 +- 0.17 (+1.8%), no suite worse beyond the spread.

Not measured: scroll p95 and a 24-hour video loop.

### M5: navigation and enforcement

Navigation begins in the browser, a process per site with session history kept, `X-Frame-Options`,
`frame-ancestors` and opaque response blocking (ORB) in the network layer, then the site lock first in shadow mode
and then enforced. A failed check ends the renderer and the tab shows the reason. `SNOWLARK_SITE_LOCK=shadow` is the
fallback if a site breaks, `off` the last resort. Not checked yet: `user_activation` and `user_initiated` against the
browser's own input record, clipboard reads needing activation, and `frame_ancestors` as a whole (needs the frame
table of M9). A gate run against the previous baseline had no real loss.

### M7: GPU process

Staged, each stage verified with the browser working and single-process mode unchanged:

- **G1** the process and its host: a lost device and a crash are one path; crashes are counted (one forgiven every
  5 minutes, the third that is not forgiven stops launches and the browser composites itself).
- **G2** the compositor's inputs as messages, in process.
- **G3** the move: compositor, vsync, raster threads and the device run in the GPU process, behind one swap chain per
  window; a hang watchdog ends a GPU process that has made no pass for 10 s with work queued. Input to present p95
  5.83 ms in process against 5.88 ms with the GPU process (paired median +0.10 ms). Windowed Speedometer 21.37 off,
  21.16 on (-1.0%). A scripted resize shows the page frame at its old size for one frame per resize, on both paths:
  the compositor presents the resize at once with the page's last frame; fixing that means waiting, up to a
  deadline, for the frame at the new size, on both paths.
- **G4** direct renderer-to-GPU links for the raster seam and the compositor's tab commands, and video textures made
  on the GPU process's device (a link measured p95 5.78 against 5.75 ms). A GPU process takes handles out of itself
  for the browser only through a path that closes the source.
- **G5** restart: the browser replays every window, surface, resource and tab to the new process, tells each
  renderer to resend fonts, images and a whole frame and to recreate video pools; the old surface stays on a lower
  visual, frozen at its last picture, until the new process presents (a swap chain whose process ended cannot be
  replaced while DWM composes its surface). 20 kills (at rest, while scrolling, while resizing, during video): no
  blank frame, the page live again 0.35 to 0.71 s after each. 100 kill cycles on an earlier build kept handles at
  695 to 674 and private bytes at 182 to 180 MiB. Past the crash limit, graphics-card crashes fall back to WARP first
  and then to compositing in the browser; the 100-cycle run on the final build is still to be repeated on an idle
  machine.
- **G6** canvas remoted: not started. Exit tests: canvas WPT areas equal on and off, the Speedometer canvas suites at
  or above today's, a canvas page surviving a GPU kill through the context-loss steps.

The GPU process becomes the default with `--process-model=tab` once the G5 exit runs and G6 are accepted.

### C1 and M9: frames

C1 is finished in process: V8's access check on the `Window` and `Location` templates decides from a table of origin
and domain per window, with one `JSGlobalProxy` per browsing context for its whole life, so a held reference survives
any navigation. It needed `SetAccessCheckCallbackAndHandler` and related functions by mangled name
(`snowlark-js/src/access_check.rs`), because the `v8` crate binds neither access checks nor remote contexts; the
library has `Context::NewRemoteContext` and `DetachGlobal` for M9. For M9 the host methods that answer from the local
`Dom` must answer from replicated frame state, a remote frame needs a window handle without a local document, and
stamping the origin of `postMessage` moves to the browser. M9 proceeds in order: the frame tree in the browser and
remote frames in the engine; `RemoteFrame` compositing; input, focus and scrolling; geometry and observers;
`window.open`, `opener`, COOP, then COEP and `SharedArrayBuffer`. Exit: the relevant WPT areas equal between
`--process-model=site` and single; a cross-site iframe crash shows a sad frame while the page lives; a Spectre-style
test page cannot find a cross-site frame's marker bytes in its own address space.

Auxiliary browsing contexts (`engine/auxiliary.rs`) exist in process: a window that `window.open()` makes is a
top-level traversable the opener's page hosts next to its own. The embedder chooses whether to host them; the window
does not show a hosted one yet.

## 13. Costs

| Cost | Value | Basis |
|---|---|---|
| New tab, spare ready | 9.8 ms to first frame (7.7 single-process) | Measured |
| New tab, no spare | 32.8 ms | Measured |
| Synchronous IPC round trip | p50 21-25 microseconds | Measured |
| Input to page | Under 30 microseconds | Measured |
| Windowed Speedometer, tab against single | +1.8% | Measured |
| Added memory per tab | Tens of MB | Reasoned: process-level state (V8 statics, stylo data, DirectWrite factory) is new; isolate and text engine were per tab already |
| Canvas device per renderer | About 51 MB | Measured; a GPU process removes it |
| Threads per renderer | Must be capped (V8 workers, the style thread pool) | Reasoned |
| CFG, CET | 0-2% | Estimate |

## 14. Open risks

| Risk | If it goes wrong | Settled by |
|---|---|---|
| DirectWrite does not work under a real sandbox | Layout depends on it; a font broker is needed | A sandbox prototype under the planned token and job |
| A trust check is forgotten | A quiet site isolation hole | `Untrusted<T>`, shadow mode, the evil-renderer suite, fuzzing |
| Hostile fonts reach the unsandboxed raster side before the GPU process is sandboxed | A full escape through one parser bug | Every web font goes through OTS, in the renderer and again on the raster side, so DirectWrite reads only fonts OTS rewrote (done); OTS's own parsers still run there. Make the GPU process the default; later, produce web font glyph masks inside the sandbox |
| Media Foundation in a separate process | Video falls back to slower paths | Media process prototype |
| Canvas without a GPU in the renderer | Speedometer chart suites fall | A `SNOWLARK_CANVAS=skia` run, then G6 |
| A service worker design that assumes one process | Rework of the service traits and the lock | Service workers behind message-shaped traits in the browser half, with fetch interception at the network boundary (`docs/service-workers.md`) |
| Memory with many tabs on small machines | Above other browsers' use | Pool sizes, same-site renderer sharing |
| V8 access checks through the `v8` crate | M9 harder | C1 showed the declared entry points work; remote contexts are not yet declared |

## References

- [DCompositionCreateSurfaceHandle](https://learn.microsoft.com/en-us/windows/win32/api/dcomp/nf-dcomp-dcompositioncreatesurfacehandle) and
  [IDXGIFactoryMedia::CreateSwapChainForCompositionSurfaceHandle](https://learn.microsoft.com/ja-jp/windows/desktop/api/dxgi1_3/nf-dxgi1_3-idxgifactorymedia-createswapchainforcompositionsurfacehandle):
  the two-process swap chain pattern.
- [Chromium's sandbox design](https://chromium.googlesource.com/chromium/src/+/main/docs/design/sandbox.md): restricted
  token, job, desktop, integrity levels, win32k lockdown.
- [Shared memory versioning for `document.cookie`](https://blog.google/chromium/introducing-shared-memory-versioning-to/).
