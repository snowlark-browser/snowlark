# WebGL

How Snowlark runs WebGL: where a context lives, how its calls reach the graphics card, how the drawing buffer
reaches the screen, what keeps a hostile page from the graphics stack, and the stages. Specifications: WebGL 1.0
and 2.0 (Khronos), HTML §4.12.5 (`getContext`), OpenGL ES 2.0 and 3.0, EGL 1.5.

## The pieces

| Piece | Where | What it does |
|---|---|---|
| `WebGLRenderingContext` and its objects | `snowlark-page` (`engine/webgl.rs`), page thread | The client: WebGL's own checks, object ids, the state script reads back as objects, encoding |
| The protocol | `snowlark-gl` (`protocol.rs`) | Plain-data messages: `Command` (no answer), `Query` → `Reply`, `present` → `Readback` |
| The service | `snowlark-gl` (`service.rs`, `ffi.rs`) | Runs the messages on ANGLE: one EGL display, a context and a pbuffer drawing buffer per canvas |
| ANGLE | `libEGL.dll`, `libGLESv2.dll` beside the executable | OpenGL ES 2.0/3.0 and EGL over Direct3D 11, with WebGL's validation and shader translator |

The bindings are generated from `idl/webgl1.idl` like every other interface; the page crate never calls OpenGL,
and `snowlark-gl` knows nothing about pages. `snowlark-gl` is the one crate with the foreign calls into ANGLE.

## Commands, not calls

A context names its objects with ids it allocates itself; the service maps each id to ANGLE's own name and ignores
ids it does not know, so `createBuffer()` needs no round trip and a client can never name an object of another
context. Calls that return nothing are queued in order and sent as one batch when the page needs an answer (a
`Query`: `getError`, `getParameter`, `readPixels`, a shader's status) or at the rendering update. Every message is
plain data, which is what lets the service move to another process without changing the client.

The order of a page's calls is kept across all its contexts (one queue). `getError` returns the errors the client
recorded first (an object of another context, a deleted object, a wrong typed array), then the service's.

## Where the service runs

Today the service runs in the page's own process, on the page thread: the renderer process in the window, the
process itself in the headless modes. That is the same place canvas 2D draws on its Direct3D device today, and it
needs no new process boundary for the first stages.

The end state is the GPU process (`docs/process-isolation.md`, M7). A sandboxed renderer will have no Direct3D
device (M6), so before the renderer sandbox is turned on the service must run in the GPU process: the batch
crosses a pipe, queries are synchronous round trips, and the drawing buffer becomes a texture the compositor
samples directly instead of pixels read back. Because the protocol is already messages, that move changes
`Backend`'s implementation, not the context. Costs of the move: a round trip per query (tens of microseconds; WebGL
pages query rarely in their frame loop, and the client answers most `getParameter` reads about bindings itself)
and shared-memory transfer for large buffer and texture uploads.

## ANGLE

ANGLE implements OpenGL ES and EGL over Direct3D 11 on Windows. Each context is created with:

- WebGL compatibility (`EGL_ANGLE_create_context_webgl_compatibility`): ANGLE applies WebGL's rules, which are
  stricter than OpenGL ES's (no client-side arrays, buffer bounds on every draw, the WebGL extension set, reserved
  `webgl_` names, framebuffer feedback loops), and its shader translator validates every shader as WebGL GLSL ES
  and rewrites it for the Direct3D compiler (bounds-checked array indexing, initialized variables).
- Robust buffer access and lose-on-reset (`EGL_EXT_create_context_robustness`): an out-of-range read returns zero
  or a value from the buffer, never other memory, and a graphics reset is reported instead of hidden.
- Robust resource initialization: new buffers, textures and drawing buffers read as zero, never as earlier
  contents of video memory.
- No resources created by binding an unused name, and the hardened context flag.

Every call through which the service reads or writes memory of the caller uses ANGLE's robust client memory entry
points (`GL_ANGLE_robust_client_memory`), which take the buffer's size: `readPixels`, `texImage2D`, every
`get*` query.

**How it is built.** `tools/angle/pin.txt` names an ANGLE commit and SHA-256 hashes of its two libraries;
`tools/angle/build.ps1` checks out that commit of `https://chromium.googlesource.com/angle/angle` with its
dependencies and builds `libEGL` and `libGLESv2` (release, Direct3D 11 only: no Vulkan, OpenGL, Direct3D 9,
SwiftShader or capture) into `<SNOWLARK_DEV_DIR>\angle\<name>`. `dev.ps1` hands that folder to cargo as
`SNOWLARK_ANGLE_DIR`, and `snowlark-gl`'s build script copies the two libraries beside the built executables. The
libraries are loaded when the first WebGL context is made, from the executable's folder; without them
`getContext("webgl")` returns null and nothing else changes. ANGLE is under a BSD licence. The release package
does not carry the two libraries yet: `tools/release/package.ps1` has to add them, and `tools/release/notices.py`
the notices of ANGLE and of what it compiles into `libGLESv2`, before a release has WebGL.

Measured on the pinned commit: `libEGL.dll` 0.5 MB and `libGLESv2.dll` 5.6 MB (6.1 MB together, release, line
tables); a clean build of the two libraries takes about 4 minutes on 8 jobs after the checkout (about 14 GB with
its toolchain). The Rust side adds `snowlark-gl` (no new crates from outside).

## The drawing buffer

The drawing buffer is a pbuffer surface of the canvas's size with the attributes the page asked for (alpha, depth,
stencil, 4× multisampling for `antialias`), so framebuffer 0 is the drawing buffer and `bindFramebuffer(null)`
needs no translation. A change of the canvas's `width` or `height` makes a new surface (cleared; the context's
state stays). `getContextAttributes()` reports what was granted.

At the rendering update (HTML "update the rendering"), each context that drew to the drawing buffer since the last
one is presented: the service reads the buffer back (RGBA) with the page's framebuffer and pack state put back
after, and, unless `preserveDrawingBuffer` is true, clears it to its defaults with every piece of state the clear
touches restored (WebGL 1.0 §2.2). The client turns the pixels into the canvas's frame (premultiplied BGRA, top row
first; premultiplied by the client when `premultipliedAlpha` is false, opaque when `alpha` is false) and hands it to
the renderer exactly as a 2D canvas's frame. `toDataURL()` reads the buffer without clearing it.

The readback is the first stage's cost: a copy from the graphics card every frame a canvas draws. The next form
shares the drawing buffer as a Direct3D 11 texture (ANGLE's `EGL_ANGLE_d3d_texture_client_buffer` or a shared
handle with a keyed mutex, as video frames already reach the compositor), double-buffered so the page draws into
one while the compositor samples the other.

## Security

- **Checked twice.** The client checks what WebGL defines before a call is sent; the service runs every call in
  ANGLE's WebGL mode, which checks again and does not trust the client. A compromised renderer that sends
  arbitrary messages gets the same checks: unknown ids become name 0, malformed uniform lengths are dropped, reads
  are bounded by the size the message states and capped at 1 GiB.
- **Shaders** never reach the driver as the page wrote them: ANGLE's translator parses, validates and rewrites them.
- **Limits** are read from ANGLE when a context is made (`Created::limits`) and are the ones `getParameter` reports.
- **Identifying strings.** `VENDOR` and `RENDERER` are fixed strings; the graphics card's name is not exposed
  (`WEBGL_debug_renderer_info` is a later decision).
- **Context loss.** After every presentation the service is asked for the reset status; a reset marks the context
  lost: every call does nothing, `isContextLost()` is true, `getError()` returns `CONTEXT_LOST_WEBGL` once. The
  `webglcontextlost` and `webglcontextrestored` events, `WEBGL_lose_context` and restoring the context are stage
  S3.
- **Off switch.** `SNOWLARK_WEBGL=off` disables WebGL (null contexts); `SNOWLARK_WEBGL=warp` runs it on Direct3D's
  software rasterizer, which gives the same pixels on every machine.

## Headless modes

`--render`, `--eval` and `--wpt` run the service in their own process like the window's renderer. A `--render` of
a WebGL page waits for each canvas's frame at the rendering update, as it does for 2D canvases.

## Stages

| Stage | What | What passes |
|---|---|---|
| S1 | WebGL 1 context through the generated bindings; buffers, shaders, programs, uniforms, attributes, draws, framebuffers and renderbuffers, `readPixels`; texture uploads from typed arrays; the drawing buffer shown in the window and in `--render`; context loss detected | A page that clears, draws a triangle and reads it back; the same pixels as other engines in `--render`; `getContext` returns the same context twice and null for another kind |
| S2 | Texture uploads from images, canvases, video, `ImageData` (with `UNPACK_FLIP_Y`, premultiply, colour space conversion); the drawing buffer as a shared texture; `drawImage` and `texImage2D` of a WebGL canvas | Image and video textures; WebGL canvases inside 2D canvases |
| S3 | Extensions (`OES_*`, `EXT_*`, `WEBGL_*` as ANGLE offers them in WebGL mode), `WEBGL_lose_context` and the context events and restore; WebGL 2 (`WebGL2RenderingContext`, an OpenGL ES 3.0 context) | Sites that need extensions or WebGL 2 (map viewers, 3D libraries) |
| S4 | Conformance: WPT `webgl/` and the Khronos WebGL conformance suite where it runs; `OffscreenCanvas` and WebGL in workers; the service in the GPU process | The conformance counts; WebGL with the renderer sandbox on |

Known gaps of S1 besides the later stages: GL objects are freed with the page, not when script drops the last
reference; a canvas removed from the document keeps its context until the page goes.
