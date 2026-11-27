# Embedding Snowlark

Status: design. Nothing in this document is built yet; it describes how the engine is to be offered to applications
and the steps that get it there.

An application embeds Snowlark to show web content in a window of its own: a help viewer, a mail client's message
pane, an application whose interface is HTML, a browser with a different shell. Today the engine reaches the screen
only through Snowlark's own window (`snowlark-shell`). This design adds a crate, `snowlark-embed`, that holds
everything a page needs below the window, with a small public API over it; the browser window becomes its first
user.

## 1. The shape

```
application ── snowlark-embed ── snowlark-tab ── snowlark-page ── …
  (window,       Engine            TabCommand /
   its UI,       WebView           TabEvent, the
   event loop)   Surface           renderer half
                 delegates
```

- **`Engine`**: one per profile in a process. It owns the network and storage threads of its partition, the GPU
  device or the GPU process, the spare renderer, and the queue of events for the application.
- **`WebView`**: one page and its session history: what `TabHost` is today, plus the input routing the window does
  now. It is a handle; dropping it closes the page.
- **`Surface`**: where a WebView's frames go on screen. The application supplies it from its own window; the
  engine's compositor draws into it at the display's rate, on its own thread. The application never paints or
  presents a frame.
- **Delegates**: traits the application implements to hear what a page did and to decide what it asks for.

Everything below `snowlark-embed` keeps its current interfaces. The engine crates stay free of any window, as they
are now; `snowlark-tab` stays the renderer half, driven by `TabCommand` and answering with `TabEvent`.

## 2. The public API

The sketches below show the intended types and calls. Names may change when the crate is written; the division of
work between application and engine is the design.

### 2.1 The engine

```rust
let engine = snowlark_embed::Engine::builder()
    .profile(Profile::folder(app_data.join("web")))   // or Profile::in_memory()
    .process_model(ProcessModel::Tab)                  // or ProcessModel::Single
    .helper(Helper::Path(exe_dir.join("snowlark-helper.exe")))
    .user_agent_suffix("ExampleApp/2.1")
    .waker(Arc::new(MyWaker(proxy)))
    .delegate(Rc::new(MyEngineDelegate))
    .build()?;
```

- **Thread.** The `Engine` and its `WebView`s live on the thread that built the engine, the application's UI
  thread; they are `!Send`. Nothing they do waits for a page, a renderer process or the GPU: commands are messages,
  as the window's are today.
- **The waker.** Events from renderers, the network and the compositor arrive on other threads. The engine queues
  them and calls `Waker::wake` (any thread, `Send + Sync`); the application then calls `engine.pump()` on its UI
  thread, which delivers the queued events as delegate calls and returns. A Win32 application posts itself a
  message in `wake`; a winit application sends a user event through its proxy; a GTK or Qt application schedules an
  idle callback. `pump` never blocks.
- **Lifetime.** Dropping the engine closes its WebViews, lets the renderer and GPU processes end (killing those that
  do not within a few seconds, as the window does now) and flushes the profile's last writes.

### 2.2 Surfaces

A surface is a rectangle of the application's window that one WebView shows in. The engine's compositor presents one
swap chain or buffer per surface; scrolling, smooth scrolling, fixed and sticky layers and video run there without
the application's thread, as they do in the browser window.

| Platform | Constructor | What it is |
|---|---|---|
| Windows | `Surface::composition()` | The engine makes a composition surface and returns its handle; the application binds it to a visual of its own visual tree (`IDCompositionDevice::CreateSurfaceFromHandle`, then `IDCompositionVisual::SetContent`). The compositor presents into it from its own thread or from the GPU process. This is the path the browser window already uses with a GPU process (`WindowSurface`, `Compositor::on_surface`). |
| Windows | `Surface::for_window(hwnd)` | For an application without a visual tree: the engine binds its own visual tree to a child window the application made for it. |
| Linux | `Surface::wayland(display, surface)` | A `wl_surface` the application made (usually a subsurface of its window); the compositor attaches its buffers and paces itself on the surface's frame callbacks, as the Linux compositor does for its own window today. |
| Any | `Surface::pixels(callback)` | Each presented frame as a BGRA buffer with its damaged rectangles: for toolkits with no way to show a foreign surface, for tests and for headless use. |

The Windows and Wayland constructors also take a `raw-window-handle` window, so applications built on winit, SDL,
GTK or Qt pass what their toolkit gives them. An EGL surface on Linux (`Surface::egl`) follows once the Linux
compositor draws on the GPU (`docs/linux.md`).

A surface's size and scale change together: `surface.resize(width, height, scale)` in device pixels. While the
application is inside its own resize handling it may call `surface.resize_and_wait(timeout)`, which returns once a
frame of the new size is on its way to the screen (the window does this today with `CompMsg::Ack`, so the desktop
never shows the window at a size its content does not have yet).

`surface.show(&webview)` shows a WebView in the surface (the WebView it showed before is hidden and told so, as a
background tab is: `TabCommand::Shown`). Several WebViews on screen at once take a surface each; a tab strip shows
one of several WebViews in one surface.

### 2.3 WebViews

```rust
let view = engine.web_view(Rc::new(MyViewDelegate)).build();
surface.show(&view);
view.load("https://example.org/".parse()?);
```

What the application drives, and what carries it today:

| Call | Today |
|---|---|
| `load(url)`, `reload()`, `stop()` | `TabHost::navigate`, `reload`, `stop` |
| `go(delta)`, `can_go(delta)`, `history()` | `TabHost::go`, `can_go`, `trail`; restore with `restore(trail)` |
| `url()`, `title()`, `favicon()`, `loading()`, `progress()` | `TabState`, `TabEvent::Favicon` |
| `pointer(event)`, `wheel(event)`, `key(event)`, `ime(event)` | `TabCommand::Pointer`, `Key`, `Ime`; the wheel and the scrolling keys go to the compositor (`CompMsg::ScrollBy`), as the window routes them now |
| `drag(event)`, `drop(data)` | `TabCommand::Drag` |
| `set_focus(bool)` | new (the window's focus reaches the page through `TabCommand::Shown` and key routing today) |
| `set_color_scheme(scheme)` | `TabCommand::ColorScheme` |
| `set_zoom(factor)` | new: page zoom, which changes the CSS pixel's size in device pixels |
| `find(text, options)`, `find_next()`, `stop_finding()` | `TabCommand::Find` |
| `evaluate(script, callback)` | new: runs a script in the top-level document's realm, answers with the result as JSON or the exception's message |
| `post_message(json)`, and `MessagePort` the page receives | new: a channel between the page and the application, exposed to the page only when the application asks for it (§4) |
| `mute(bool)`, `audible()` | `TabMuteControl`, `TabEvent::Audible` |
| `screenshot(rect, callback)` | `Compositor::read_back` |
| `clear_site_data(origin, kinds)` | `TabCommand::ClearSiteData` |
| `close()` (runs `beforeunload` and `unload`) | `TabCommand::Close` |

Input events are the engine's own types (pointer kind, buttons, position in device pixels, modifiers, key with its
code and text, composition events), with helpers that convert a Win32 message (`input::win32::from_message`) and a
winit event (feature `winit`). The cursor a page asks for comes back through the delegate; the application sets it.

### 2.4 Delegates

Every delegate method has a default, so an application implements only what it needs. Calls happen inside
`engine.pump()`. A request that waits on the user is an object the application may keep and answer later, from any
place on the UI thread: `allow()`, `deny()`, or a value; dropping it unanswered gives the default.

`WebViewDelegate`:

| Method | Default | Today |
|---|---|---|
| `url_changed`, `title_changed`, `favicon_changed`, `load_changed` | nothing | `TabEvent::Committed`, `DocumentUrl`, `SameDocument`, `State`, `Favicon` |
| `cursor_changed`, `status_text` (the link under the pointer) | nothing | `TabEvent::Hover` |
| `fullscreen(request)` | deny | `TabEvent::Fullscreen` |
| `navigation(request)`: the URL, who started it, whether it replaces | allow | `TabEvent::BeginNavigation`, decided in `TabHost::navigation_ready` |
| `new_window(request)`: `window.open`, `target=_blank`, the features | deny; the application allows by giving a new WebView | `TabEvent::OpenWindow`, `Open`, `TabHost::view` |
| `close_window()`, `window_rect(rect)` | nothing | `TabEvent::CloseWindow`, `WindowRect` |
| `select_popup(request)`, `context_menu(request)` | closed (the select keeps its value) / nothing | `TabEvent::SelectPopup`, `ContextMenu` |
| `dialog(request)`: `alert`, `confirm`, `prompt`, `beforeunload` | dismissed | new (the page answers them without asking anyone today) |
| `permission(request)`: the feature and the origin | deny | new (fixed answers today) |
| `file_chooser(request)`: accept types, multiple, folder | cancelled | new |
| `download(request)`: URL, suggested name, MIME type, a stream | cancelled; the application may give a path and hear progress | `TabHost::take_download` |
| `authentication(request)`: HTTP authentication and client certificates | cancelled | new |
| `certificate_error(request)` | the error page | the network's error page today |
| `ime_area(rect)`: where the composition window goes | nothing | new |
| `crashed(reason)`, `unresponsive()`, `responsive()` | nothing; the application calls `reload()` | `TabEvent::Crashed`, `Liveness` |
| `message(json)` from the page's channel | nothing | new |

`EngineDelegate` has what is not one page's: the GPU process's state (`GpuStatus`), a renderer or helper that would
not start (§3), and the clipboard (the `Clipboard` trait the tabs use now, with the system's clipboard as the
default).

Custom schemes are a trait the application registers on the builder (`SchemeHandler`: a URL in, a response head and
body stream out), generalising `InternalPages`, the trait the browser's own pages use today. A scheme is registered
with its security properties (secure context or not, whether it has an origin of its own), and a renderer is locked
to it as to any site.

## 3. What moves where

The browser window holds four things that are not the window: the tab's host, the compositor, its begin frames and
the GPU process. Each is read below for what ties it to the window today.

| File (in `snowlark-shell`) | What it is | What ties it to the window | Change |
|---|---|---|---|
| `tab_host.rs` | The browser's half of a tab: session history, navigations through the network, site swaps, downloads, renderer crash and restart | Nothing of the window's types: it uses `composite::{CompInput, SharedScroll}`, `session`, and `snowlark-tab`. It starts renderers from the running executable (`renderer_launch`: `current_exe()`, `--type=renderer`) and passes the process's `SNOWLARK_*` variables on. A site swap and a download wait for the window to act (`needs_swap`, `swap_renderer`, `take_download`). | Moves as it is. The launch takes the engine's helper (§4.2); the variables become builder settings, the environment staying a developer override. The WebView does the swap itself and hands downloads to its delegate. |
| `composite.rs` | The compositor thread: every tab's frames, scrolling, sticky and fixed layers, videos, frame pacing | `snowlark_paint::window::WindowHandle` names the window to bind (`Compositor::new` makes a DirectComposition target for its HWND) and identifies the compositor among the windows (`WindowId` is the handle's value); `display_refresh_hz` and `raise_thread_priority` come from the window module. The window's chrome shares its swap chain: `CompMsg::Resize` carries the chrome's height and lists, `Chrome`, `Overlay`, `DevtoolsWindow` and `PageArea` are the window's. It uses the shell's `scroll.rs` (scroll bars, smooth scrolling). | Moves with `scroll.rs`. A surface (§2.2) replaces the window handle; `WindowId` becomes a surface id the engine assigns. The chrome and overlay messages stay, behind an extension only the browser window uses (§3.2). |
| `gpu_compositor.rs` | The compositor inside the GPU process and the proxy that feeds it | `InBrowser { window }` and the map of windows' compositors; a composition surface's handle, duplicated into the GPU process | Moves as it is, with surfaces for windows. |
| `gpu_process.rs` | The GPU process's host and its child's main | `gpu_launch` starts the running executable with `--type=gpu`; the process model is the shell's type; its tests make hidden windows | Moves as it is; the launch takes the helper. |
| `begin_frames.rs` | The vsync thread and the begin frames it sends the tabs that ask | `snowlark_paint::vsync` waits for the primary display's vertical blank | Moves as it is. A surface on Wayland paces on its frame callbacks; later, a surface can name its display. |

What else must move with them, because they need it:

- `session.rs` (the session history `TabHost` keeps).
- `partition.rs` and `local_storage.rs` (a profile's network, cookies, Web Storage, IndexedDB, Cache API and code
  cache, which every tab's services are made from), without the visited-pages history, which is the browser's.
- `cdm.rs` (the key systems a page's media can use), off by default (§4).
- The input routing in `browser/mod.rs` (which wheel and key input scrolls in the compositor and which goes to the
  page, `fn wheel` and its neighbours), and the event sink that wakes the window's loop, which becomes the waker.

What stays in `snowlark-shell`: the chrome, the address bar, tabs, bookmarks, history, downloads' list and folder,
passwords, the developer tools, settings, the update check, the task manager, the content blocker's panel, menus,
dialogs, the internal pages (`snowlark:`), and the profile folder's place.

In `snowlark-paint`, `window.rs` (the Win32 window and its message loop) stays where it is for the shell. The
compositor's dependence on it is a handle type only, which moves to a small module of its own so that
`snowlark-embed` needs no window code.

### 3.1 Dependency direction

```
snowlark (binary) → snowlark-shell → snowlark-embed → {snowlark-tab, snowlark-blocker, snowlark-media} → snowlark-page → …
an application    →                  snowlark-embed
```

`snowlark-embed` depends on nothing of the shell. A test of the move: `snowlark-shell`'s tab and protocol tests
(`tab_tests.rs`, `protocol_tests.rs`) move with the code they test and pass unchanged.

### 3.2 The browser window as an embedder

The browser window draws its chrome into the same swap chain as the page, so that a resize can never show the chrome
at one size and the page at another (two layers presented apart can). An application with its own UI toolkit has no
use for that: its UI is in its own visuals beside the surface. So the public API has no chrome; a `host_layers`
extension (a cargo feature, outside the stable API) lets the browser window send its display lists for the chrome,
the overlay and the docked developer tools as it does now. The shell then uses `Engine`, `WebView` and `Surface`
like any application, plus that extension.

## 4. Processes

### 4.1 Today

The browser runs each tab's renderer in a process of its own by default, and the compositor in a GPU process behind
`--gpu-process` (`docs/process-isolation.md`). Both children are the running executable, started with
`--type=renderer` or `--type=gpu`; the child connects back over the pipe its environment names
(`snowlark_ipc::connect_to_browser`) and runs `snowlark_tab::renderer_main::run_renderer` or
`gpu_process::run_child`. `--process-model=single` keeps every tab on a thread of the browser's process.

The embedding application's process is the browser process: it holds the network, storage, the site locks and the
policy tables, and is not sandboxed.

### 4.2 Options for an application

| | How | For | Against |
|---|---|---|---|
| **A helper executable** (`Helper::Path`) | The crate builds `snowlark-helper`, a small binary that is only the two child mains; the application ships it beside its own executable. | The sandboxed process runs only engine code: none of the application's start-up, statics or imported libraries. The sandbox's later stages need exactly this: win32k lockdown needs an executable that does not link `user32` (`docs/process-isolation.md` §7, step 5), which the browser's own executable cannot be. It starts faster than a large application. | One more file to ship, and it must be from the same build as the engine. |
| **An entry point** (`Helper::CurrentExe`) | The application calls `snowlark_embed::run_child()` first in `main`; it returns the child's exit code when the process was started as a child, else nothing and `main` goes on. | One executable, as the browser itself does. | Everything before the call, and every library the executable imports, runs in the renderer too; the strict sandbox stages are not possible. |
| **One process** (`ProcessModel::Single`) | Every page on a thread of the application's process. | Content the application trusts (its own pages, a local help file), tests, platforms without the process layer. | No crash isolation and no sandbox: a page that crashes the engine crashes the application. |

The default is the helper with `ProcessModel::Tab`. The entry point stays supported for applications that must be
one file. The engine and its helper speak a protocol that is not versioned across builds, so the launch passes the
engine's build id and a helper of another build ends at once with an exit code of its own, which the engine reports
to `EngineDelegate` instead of showing every page as crashed.

The GPU process stays the application's choice, off by default as it is in the browser until restarting it brings
the content back on its own.

### 4.3 What the sandbox needs from an application

From `docs/process-isolation.md` §7: the job object and the process mitigations are on for every renderer today and
need nothing from the application. The restricted token with its warm-up, the alternate desktop and win32k lockdown
need the helper. Crash dumps of the children go to a folder the application names (the profile's by default).
The application must not start renderers itself or pass them handles: the engine's launch keeps the allow-list of
what a child inherits.

## 5. Profile, storage, network, features

A profile is the application's choice. `Profile::folder(dir)` keeps everything in `partition.rs`'s table in that
folder (cookies, HSTS, Web Storage, IndexedDB, the Cache API, the code cache, key systems' state); one engine at a
time uses a folder, which a lock file in it enforces. `Profile::in_memory()` keeps everything in memory, as an
Incognito window does. The browser's own folder is the browser's: an engine never defaults to it.

Network settings on the builder: the User-Agent (Snowlark's string with the application's product token after it;
the application may replace it, at the cost of site compatibility), the languages (`Accept-Language`,
`navigator.languages`), a proxy, extra trusted roots, and request interception for the application's own schemes.
Everything else (mixed-content blocking, HSTS, CSP, the request policy) is the engine's and cannot be turned off.

Browser features stay off unless the application turns them on, so that an embedded page observes what it would in
`--wpt` or `--render` (the rule for browser features in `CONTRIBUTING.md`):

| Feature | Default in an embedding | Turned on by |
|---|---|---|
| Content blocking (`snowlark-blocker`) | off | `content_filter(lists)`; its cosmetic styles stay at user origin |
| Password saving and filling | off | not offered at first: it needs the browser's UI and store |
| Developer tools | off | later: a remote protocol endpoint |
| The update check | never | the application updates itself |
| Internal pages (`snowlark:`) | none; `about:blank` only | the application's schemes |
| Key systems for protected media | off | `key_systems(...)` (Windows) |
| The page–application channel (§2.3) | not exposed | `web_view(..).message_channel(name)`, which adds a global only to pages of the origins the application lists |

## 6. Packaging

- **V8.** Snowlark links its own build of V8 as a static library of about 150 MB (`docs/v8-build.md`): the `v8`
  crate's version that no crate release covers yet, with pointer compression. An application's build must use that
  library: the crate's build script takes it from `RUSTY_V8_ARCHIVE`. `snowlark-embed` documents the variables and
  ships a small build helper that downloads the pinned library from Snowlark's release and checks its hash, as the
  repository's own build does. Nothing of V8 is shipped as a separate file: it is in the application's executable
  (and the helper's).
- **Patched crates.** The workspace builds patched copies of `html5ever`, `stylo`, `taffy`, `url`, `urlpattern`,
  `v8` and `dav1d-sys` (`vendor/`, through `[patch]` in the workspace's `Cargo.toml`). A `[patch]` table applies only
  in the workspace that has it, not in an application that depends on Snowlark. Until each patch is upstream or the
  copy is published under a name of its own, an application depends on Snowlark through git, at a release's tag, and
  copies the `[patch]` table (the crate's documentation keeps it with each release). Publishing on crates.io waits
  for that.
- **ANGLE.** WebGL loads `libEGL` and `libGLESv2` at run time from beside the executable (`snowlark-gl`): two DLLs on
  Windows, two shared objects on Linux, beside the process that runs WebGL's executor. Without them every WebGL
  context fails and the rest works. A release carries them.
- **Size.** The browser's executable is about 130 MB today; an application grows by about as much, and the helper
  is of similar size, being the same engine.
- **Licences.** Snowlark is MPL-2.0: file-level copyleft. An application may be under any licence; changes to
  Snowlark's own files must be published under the MPL. A binary must carry the notices of everything it links:
  `tools/release/notices.py` writes them for the browser; it is to take an application's manifest and write them for it.
  Stylo is MPL-2.0 too, and its source with Snowlark's patches is published with every release that changes it.
- **Versions.** `snowlark-embed` is the only crate with an API meant for others. While it is 0.x every minor version
  may break it, and the release notes say what changed; 1.0 comes when a sample application in each platform and the
  browser window have used one API through a few releases without changes. The other crates are internal: they are
  versioned with the workspace and carry no promise. An engine and its helper must be from one build (§4.2).

## 7. Steps

Sizes: S is up to about a thousand lines changed, M a few thousand, L more, most of it moved code.

| # | Step | Size | Done when |
|---|---|---|---|
| 1 | **The crate split.** `snowlark-embed` with `tab_host`, `composite`, `gpu_compositor`, `gpu_process`, `begin_frames`, `session`, `scroll`, `partition`, `local_storage` and `cdm` moved from `snowlark-shell`; a surface in place of the window handle, a surface id in place of `WindowId`; the window-handle type out of `snowlark-paint::window`. The browser behaves as before. | L (moved) | The workspace tests, the frame comparison (`snowlark-tab/src/raster_pixels.rs`) and the window's tab and protocol tests pass unchanged; the browser opens, scrolls, plays video and survives a renderer and a GPU process crash. |
| 2 | **The public API.** `Engine`, `WebView`, `Surface` and the delegates over what moved; the pump and waker; the input routing out of `browser/mod.rs`; the `host_layers` extension; the shell moved onto the API. | L | The shell uses only the public API and the extension; a test embeds a WebView with a pixel surface and loads, navigates, scrolls and reads back a frame. |
| 3 | **What the delegates need from the engine.** Dialogs, permission requests, the file chooser, page zoom, `evaluate`, HTTP authentication, the page–application channel. Each is JS-visible or changes layout and comes with its WPT area. | M, one per feature | Each with tests and its WPT area's count; the browser window gets the UI for each as its own delegate. |
| 4 | **Processes.** `snowlark-helper`, `run_child()`, the build id check, the crash-dump folder and child settings from the builder. | M | Renderer and GPU processes start from the helper and from the entry point; a helper of another build is reported, not shown as crashes. |
| 5 | **A Windows sample.** `examples/embed-windows`: one WebView in a plain Win32 window made with the `windows` crate (not Snowlark's window code), with an address on the command line, Back and Forward, and resize. | S | It builds from a clean checkout with the documented steps and runs with the helper beside it. |
| 6 | **A Linux sample.** The same on Wayland with a subsurface, after the Linux window and compositor stages of `docs/linux.md`. | M | It runs on a Wayland desktop. |
| 7 | **Documentation.** Rustdoc for every public item, an embedding guide (build, ship, the `[patch]` table, the helper, the V8 library), this document updated to what was built. | S | The guide's steps build the samples from nothing. |
| 8 | **A release.** A tag with the helper, ANGLE's files, the V8 library's hash and the notices; crates.io once the patched crates have names of their own. | M | An application outside the repository builds against the tag. |

Steps 1 and 2 come first and in order. Steps 3 and 4 are independent of each other; 5 needs 2 and 4; 6 needs 5 and
the Linux window.
