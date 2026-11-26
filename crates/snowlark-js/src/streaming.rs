// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Script streaming: V8 parses and compiles an external script on a background thread while its bytes
//! arrive, and the page thread only finalizes the result (V8's `ScriptCompiler::StartStreaming`,
//! `StreamedSource`, `ScriptStreamingTask` and the `Compile`/`CompileModule` overloads taking a
//! `StreamedSource`, as Chromium's `ScriptStreamer` uses them).
//!
//! The `v8` crate (152) binds none of this API, but the V8 library it links contains all of it. The
//! few entry points are declared here by their MSVC x64 names on Windows and their Itanium x86-64
//! names on Linux, and called with that target's C++ calling convention; the layouts that matter
//! were read from V8's headers and checked against each library's disassembly:
//!
//! - `StreamedSource(std::unique_ptr<ExternalSourceStream>, Encoding)`: `this`, then the
//!   `unique_ptr`, then the encoding. MSVC passes the `unique_ptr` by address (the callee moves out of
//!   it and destroys it); Itanium passes the pointer it holds in a register, as V8's libc++ (ABI
//!   version 2) makes `unique_ptr` `[[clang::trivial_abi]]`. The object is a `unique_ptr` and a
//!   `CompilationDetails`: 32 bytes, 8-aligned.
//! - `ExternalSourceStream` is a class with a virtual destructor and `GetMoreData`. MSVC's vtable has
//!   one destructor slot (the scalar deleting destructor taking a "delete" flag), Itanium's two (the
//!   complete object and the deleting destructor); [`SourceStream`] lays out the target's. V8 takes
//!   ownership of every chunk `GetMoreData` returns and frees it with `delete[]`, so chunks are
//!   allocated with `operator new[]`.
//! - `Compile`/`CompileModule` return a `MaybeLocal` through a hidden pointer passed first on MSVC (a
//!   class with constructors), in a register on Itanium (trivially copyable).
//!
//! Only Windows x64 and Linux x86-64 have these declarations; elsewhere [`ScriptStreamer::start`]
//! returns `None` and callers compile on the page thread as before.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// The text encoding of a streamed script's bytes (V8 streams these two; other encodings are decoded
/// and compiled on the page thread).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamEncoding {
    Utf8,
    Windows1252,
}

/// What the streamed source is compiled as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Classic,
    Module,
}

#[derive(Default)]
struct FeedState {
    chunks: VecDeque<Vec<u8>>,
    /// Every byte was pushed.
    finished: bool,
    /// The fetch failed or was cancelled.
    aborted: bool,
    /// Nobody will read: pushes are dropped.
    disabled: bool,
    /// The bytes started with a byte order mark of another encoding than the stream's: what V8
    /// parsed is not the script, which the page decodes and compiles itself.
    mismatch: bool,
    /// Bytes pushed so far.
    received: usize,
}

/// The bytes of a script on their way to V8's background parser: the network thread pushes them as
/// they arrive (never the page thread, which may be waiting for V8 while V8's thread waits for
/// bytes), the streaming thread pulls them.
#[derive(Default)]
pub struct Feed {
    state: Mutex<FeedState>,
    ready: Condvar,
}

impl Feed {
    pub fn new() -> Arc<Feed> {
        Arc::new(Feed::default())
    }

    /// More bytes of the body.
    pub fn push(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut s = self.state.lock().unwrap();
        if s.disabled || s.finished || s.aborted {
            return;
        }
        s.received += bytes.len();
        s.chunks.push_back(bytes.to_vec());
        drop(s);
        self.ready.notify_all();
    }

    /// The whole body was pushed.
    pub fn finish(&self) {
        self.state.lock().unwrap().finished = true;
        self.ready.notify_all();
    }

    /// The body will not complete (a network error, a cancelled fetch).
    pub fn abort(&self) {
        let mut s = self.state.lock().unwrap();
        if !s.finished {
            s.aborted = true;
        }
        drop(s);
        self.ready.notify_all();
    }

    /// No streamer will read this feed: drop what it holds and what comes.
    pub fn disable(&self) {
        let mut s = self.state.lock().unwrap();
        s.disabled = true;
        s.chunks.clear();
        drop(s);
        self.ready.notify_all();
    }

    /// Bytes pushed so far.
    pub fn received(&self) -> usize {
        self.state.lock().unwrap().received
    }

    /// Whether the stream ended normally and V8 saw the script's own text.
    fn usable(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.finished && !s.aborted && !s.mismatch && !s.disabled
    }

    /// The next chunk for V8: blocks until there is one, or `None` at the end. A chunk is at least 4
    /// bytes unless it ends the stream (V8's UTF-8 decoder handles a character split across two
    /// chunks, not three).
    #[cfg_attr(not(v8_abi), allow(dead_code))]
    fn next_chunk(&self) -> Option<Vec<u8>> {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.aborted || s.disabled || s.mismatch {
                return None;
            }
            let buffered: usize = s.chunks.iter().map(Vec::len).sum();
            if buffered >= 4 || (s.finished && buffered > 0) {
                let mut out = s.chunks.pop_front().unwrap_or_default();
                while out.len() < 4 {
                    match s.chunks.pop_front() {
                        Some(next) => out.extend_from_slice(&next),
                        None => break,
                    }
                }
                return Some(out);
            }
            if s.finished {
                return None;
            }
            s = self.ready.wait(s).unwrap();
        }
    }

    #[cfg_attr(not(v8_abi), allow(dead_code))]
    fn set_mismatch(&self) {
        self.state.lock().unwrap().mismatch = true;
    }
}

/// The result of finishing a streamed compilation.
pub enum Streamed<T> {
    /// V8 compiled the script (or threw its syntax error into the current `TryCatch`).
    Compiled(Option<T>),
    /// The stream did not carry the script (aborted, or another encoding's byte order mark): compile
    /// the decoded text as usual.
    Fallback,
}

#[cfg(v8_abi)]
mod ffi {
    use std::ffi::c_void;

    // SAFETY (all declarations): these are the MSVC x64 and Itanium x86-64 names of the V8 functions
    // in the static library the `v8` crate links; the Rust signatures follow the target's convention
    // for them (see the module documentation and the comment on each). Where the two differ, the
    // Itanium declaration is private and a function of the MSVC shape wraps it.
    extern "C" {
        /// `v8::ScriptCompiler::StreamedSource::~StreamedSource()` (Itanium: the complete object
        /// destructor).
        #[cfg_attr(
            v8_abi_msvc,
            link_name = "??1StreamedSource@ScriptCompiler@v8@@QEAA@XZ"
        )]
        #[cfg_attr(
            v8_abi_itanium,
            link_name = "_ZN2v814ScriptCompiler14StreamedSourceD1Ev"
        )]
        pub fn streamed_source_destruct(this: *mut c_void);

        /// `static ScriptStreamingTask* v8::ScriptCompiler::StartStreaming(Isolate*, StreamedSource*,
        /// ScriptType, CompileOptions, CompileHintCallback, void*)`.
        #[cfg_attr(
            v8_abi_msvc,
            link_name = "?StartStreaming@ScriptCompiler@v8@@SAPEAVScriptStreamingTask@12@PEAVIsolate@2@PEAVStreamedSource@12@W4ScriptType@2@W4CompileOptions@12@P6A_NHPEAX@Z4@Z"
        )]
        #[cfg_attr(
            v8_abi_itanium,
            link_name = "_ZN2v814ScriptCompiler14StartStreamingEPNS_7IsolateEPNS0_14StreamedSourceENS_10ScriptTypeENS0_14CompileOptionsEPFbiPvES7_"
        )]
        pub fn start_streaming(
            isolate: *mut c_void,
            source: *mut c_void,
            script_type: i32,
            options: i32,
            hint_callback: *const c_void,
            hint_data: *mut c_void,
        ) -> *mut c_void;

        /// `void v8::ScriptCompiler::ScriptStreamingTask::Run()`.
        #[cfg_attr(
            v8_abi_msvc,
            link_name = "?Run@ScriptStreamingTask@ScriptCompiler@v8@@QEAAXXZ"
        )]
        #[cfg_attr(
            v8_abi_itanium,
            link_name = "_ZN2v814ScriptCompiler19ScriptStreamingTask3RunEv"
        )]
        pub fn streaming_task_run(this: *mut c_void);

        /// `void* operator new[](size_t)`.
        #[cfg_attr(v8_abi_msvc, link_name = "??_U@YAPEAX_K@Z")]
        #[cfg_attr(v8_abi_itanium, link_name = "_Znam")]
        pub fn operator_new_array(size: usize) -> *mut u8;

        /// `void operator delete(void*)`, for the `ScriptStreamingTask` V8 allocated with `new`.
        #[cfg_attr(v8_abi_msvc, link_name = "??3@YAXPEAX@Z")]
        #[cfg_attr(v8_abi_itanium, link_name = "_ZdlPv")]
        pub fn operator_delete(p: *mut c_void);
    }

    // Itanium: the `std::unique_ptr` argument is `[[clang::trivial_abi]]` in V8's libc++ (ABI version
    // 2), so it travels as the pointer it holds, in a register, and the callee owns it; a trivially
    // copyable `MaybeLocal` comes back in a register; a constructor returns nothing.
    #[cfg(v8_abi_itanium)]
    mod itanium {
        use std::ffi::c_void;

        extern "C" {
            /// The complete object constructor.
            #[link_name = "_ZN2v814ScriptCompiler14StreamedSourceC1ENSt4__Cr10unique_ptrINS0_20ExternalSourceStreamENS2_14default_deleteIS4_EEEENS1_8EncodingE"]
            pub fn streamed_source_construct(this: *mut c_void, stream: *mut c_void, encoding: i32);

            #[link_name = "_ZN2v814ScriptCompiler7CompileENS_5LocalINS_7ContextEEEPNS0_14StreamedSourceENS1_INS_6StringEEERKNS_12ScriptOriginE"]
            pub fn compile_streamed(
                context: *const v8::Context,
                source: *mut c_void,
                full_source: *const v8::String,
                origin: *const v8::ScriptOrigin,
            ) -> *const c_void;

            #[link_name = "_ZN2v814ScriptCompiler13CompileModuleENS_5LocalINS_7ContextEEEPNS0_14StreamedSourceENS1_INS_6StringEEERKNS_12ScriptOriginE"]
            pub fn compile_module_streamed(
                context: *const v8::Context,
                source: *mut c_void,
                full_source: *const v8::String,
                origin: *const v8::ScriptOrigin,
            ) -> *const c_void;
        }
    }

    /// The `StreamedSource` constructor, taking the stream out of the `unique_ptr` storage `stream`.
    #[cfg(v8_abi_itanium)]
    pub unsafe fn streamed_source_construct(
        this: *mut c_void,
        stream: *mut *mut c_void,
        encoding: i32,
    ) -> *mut c_void {
        // SAFETY: the caller's contract, as for the MSVC declaration; the pointer moves to V8.
        unsafe {
            let owned = std::mem::replace(&mut *stream, std::ptr::null_mut());
            itanium::streamed_source_construct(this, owned, encoding);
        }
        this
    }

    /// `ScriptCompiler::Compile` of a streamed source, its `MaybeLocal` stored through `ret`.
    #[cfg(v8_abi_itanium)]
    pub unsafe fn compile_streamed(
        ret: *mut *const c_void,
        context: *const v8::Context,
        source: *mut c_void,
        full_source: *const v8::String,
        origin: *const v8::ScriptOrigin,
    ) -> *mut *const c_void {
        // SAFETY: the caller's contract, as for the MSVC declaration.
        unsafe { *ret = itanium::compile_streamed(context, source, full_source, origin) };
        ret
    }

    /// `ScriptCompiler::CompileModule` of a streamed source, its `MaybeLocal` stored through `ret`.
    #[cfg(v8_abi_itanium)]
    pub unsafe fn compile_module_streamed(
        ret: *mut *const c_void,
        context: *const v8::Context,
        source: *mut c_void,
        full_source: *const v8::String,
        origin: *const v8::ScriptOrigin,
    ) -> *mut *const c_void {
        // SAFETY: the caller's contract, as for the MSVC declaration.
        unsafe { *ret = itanium::compile_module_streamed(context, source, full_source, origin) };
        ret
    }

    // MSVC: the `std::unique_ptr` argument is passed by address and the callee moves out of it; a
    // `MaybeLocal` return value comes back through a hidden pointer; a constructor returns `this`.
    #[cfg(v8_abi_msvc)]
    extern "C" {
        /// `v8::ScriptCompiler::StreamedSource::StreamedSource(std::unique_ptr<ExternalSourceStream>,
        /// Encoding)`.
        #[link_name = "??0StreamedSource@ScriptCompiler@v8@@QEAA@V?$unique_ptr@VExternalSourceStream@ScriptCompiler@v8@@U?$default_delete@VExternalSourceStream@ScriptCompiler@v8@@@__Cr@std@@@__Cr@std@@W4Encoding@012@@Z"]
        pub fn streamed_source_construct(
            this: *mut c_void,
            stream: *mut *mut c_void,
            encoding: i32,
        ) -> *mut c_void;

        /// `static MaybeLocal<Script> v8::ScriptCompiler::Compile(Local<Context>, StreamedSource*,
        /// Local<String>, const ScriptOrigin&)`: the `MaybeLocal` comes back through `ret`.
        #[link_name = "?Compile@ScriptCompiler@v8@@SA?AV?$MaybeLocal@VScript@v8@@@2@V?$Local@VContext@v8@@@2@PEAVStreamedSource@12@V?$Local@VString@v8@@@2@AEBVScriptOrigin@2@@Z"]
        pub fn compile_streamed(
            ret: *mut *const c_void,
            context: *const v8::Context,
            source: *mut c_void,
            full_source: *const v8::String,
            origin: *const v8::ScriptOrigin,
        ) -> *mut *const c_void;

        /// `static MaybeLocal<Module> v8::ScriptCompiler::CompileModule(Local<Context>,
        /// StreamedSource*, Local<String>, const ScriptOrigin&)`.
        #[link_name = "?CompileModule@ScriptCompiler@v8@@SA?AV?$MaybeLocal@VModule@v8@@@2@V?$Local@VContext@v8@@@2@PEAVStreamedSource@12@V?$Local@VString@v8@@@2@AEBVScriptOrigin@2@@Z"]
        pub fn compile_module_streamed(
            ret: *mut *const c_void,
            context: *const v8::Context,
            source: *mut c_void,
            full_source: *const v8::String,
            origin: *const v8::ScriptOrigin,
        ) -> *mut *const c_void;
    }
}

/// An `ExternalSourceStream` subclass in Rust: the vtable pointer first, as C++ lays it out.
#[cfg(v8_abi)]
#[repr(C)]
struct SourceStream {
    vtable: *const unsafe extern "C" fn(),
    feed: Arc<Feed>,
    encoding: StreamEncoding,
    first: bool,
}

/// The MSVC vtable: a null RTTI slot (MSVC puts the complete object locator before the first virtual
/// function; V8 is built without RTTI and never reads it), the scalar deleting destructor, then
/// `GetMoreData`.
#[cfg(v8_abi_msvc)]
#[repr(C)]
struct SourceStreamVtable {
    rtti: *const std::ffi::c_void,
    destructor: unsafe extern "C" fn(*mut SourceStream, u32) -> *mut SourceStream,
    get_more_data: unsafe extern "C" fn(*mut SourceStream, *mut *const u8) -> usize,
}

/// The Itanium vtable: the offset to the top of the object (0) and a null RTTI pointer before the
/// address point, then the complete object destructor, the deleting destructor and `GetMoreData`.
#[cfg(v8_abi_itanium)]
#[repr(C)]
struct SourceStreamVtable {
    offset_to_top: isize,
    rtti: *const std::ffi::c_void,
    complete_destructor: unsafe extern "C" fn(*mut SourceStream),
    deleting_destructor: unsafe extern "C" fn(*mut SourceStream),
    get_more_data: unsafe extern "C" fn(*mut SourceStream, *mut *const u8) -> usize,
}

#[cfg(v8_abi)]
struct VtableCell(SourceStreamVtable);

// SAFETY: the vtable is immutable data (integers, a null pointer and function pointers).
#[cfg(v8_abi)]
unsafe impl Sync for VtableCell {}

#[cfg(v8_abi_msvc)]
static SOURCE_STREAM_VTABLE: VtableCell = VtableCell(SourceStreamVtable {
    rtti: std::ptr::null(),
    destructor: source_stream_destructor,
    get_more_data: source_stream_get_more_data,
});

#[cfg(v8_abi_itanium)]
static SOURCE_STREAM_VTABLE: VtableCell = VtableCell(SourceStreamVtable {
    offset_to_top: 0,
    rtti: std::ptr::null(),
    complete_destructor: source_stream_complete_destructor,
    deleting_destructor: source_stream_deleting_destructor,
    get_more_data: source_stream_get_more_data,
});

/// Where an object's vtable pointer points: the first virtual function's slot.
#[cfg(v8_abi)]
fn source_stream_vtable() -> *const unsafe extern "C" fn() {
    #[cfg(v8_abi_msvc)]
    let first = &SOURCE_STREAM_VTABLE.0.destructor as *const _;
    #[cfg(v8_abi_itanium)]
    let first = &SOURCE_STREAM_VTABLE.0.complete_destructor as *const _;
    first as *const unsafe extern "C" fn()
}

/// The Itanium complete object destructor: drops the stream in place (not a path V8 takes).
#[cfg(v8_abi_itanium)]
unsafe extern "C" fn source_stream_complete_destructor(this: *mut SourceStream) {
    // SAFETY: V8 destroys the stream it owns exactly once.
    unsafe { std::ptr::drop_in_place(this) };
}

/// The Itanium deleting destructor (`delete stream`): drops and frees the stream.
#[cfg(v8_abi_itanium)]
unsafe extern "C" fn source_stream_deleting_destructor(this: *mut SourceStream) {
    // SAFETY: V8 deletes the stream it owns exactly once; it was made by `Box::into_raw` in
    // `ScriptStreamer::start`.
    drop(unsafe { Box::from_raw(this) });
}

/// The scalar deleting destructor: drops the stream, and frees it when bit 0 of `flags` says so
/// (`delete stream`).
#[cfg(v8_abi_msvc)]
unsafe extern "C" fn source_stream_destructor(
    this: *mut SourceStream,
    flags: u32,
) -> *mut SourceStream {
    if flags & 1 != 0 {
        // SAFETY: V8 deletes the stream it owns exactly once; it was made by `Box::into_raw` in
        // `ScriptStreamer::start`.
        drop(unsafe { Box::from_raw(this) });
    } else {
        // SAFETY: as above, destroyed in place without freeing (not a path V8 takes for this class).
        unsafe { std::ptr::drop_in_place(this) };
    }
    this
}

/// `GetMoreData`: runs on the streaming thread; blocks until bytes arrive, returns 0 at the end.
#[cfg(v8_abi)]
unsafe extern "C" fn source_stream_get_more_data(
    this: *mut SourceStream,
    src: *mut *const u8,
) -> usize {
    // SAFETY: V8 calls this with the stream it owns, from the one thread running the streaming task.
    let stream = unsafe { &mut *this };
    let Some(mut chunk) = stream.feed.next_chunk() else {
        return 0;
    };
    if stream.first {
        stream.first = false;
        // HTML "decode" / "UTF-8 decode": a byte order mark decides the encoding and is not text.
        if chunk.starts_with(&[0xEF, 0xBB, 0xBF]) {
            if stream.encoding != StreamEncoding::Utf8 {
                stream.feed.set_mismatch();
                return 0;
            }
            chunk.drain(..3);
        } else if chunk.starts_with(&[0xFE, 0xFF]) || chunk.starts_with(&[0xFF, 0xFE]) {
            stream.feed.set_mismatch();
            return 0;
        }
        if chunk.is_empty() {
            // Only the byte order mark so far: the next chunk.
            // SAFETY: the same call again, on the same thread.
            return unsafe { source_stream_get_more_data(this, src) };
        }
    }
    // SAFETY: operator new[] returns a block of `len` bytes or throws (aborts); V8 takes ownership of
    // the block and frees it with delete[].
    unsafe {
        let data = ffi::operator_new_array(chunk.len());
        std::ptr::copy_nonoverlapping(chunk.as_ptr(), data, chunk.len());
        *src = data;
    }
    chunk.len()
}

/// The `StreamedSource` object's storage (32 bytes in V8 152; room to spare).
#[cfg(v8_abi)]
#[repr(C, align(16))]
struct StreamedSourceStorage([u8; 64]);

#[cfg(v8_abi)]
struct TaskPtr(*mut std::ffi::c_void);

// SAFETY: a ScriptStreamingTask is made to be run on another thread (V8's documented use); the pointer
// is moved to that thread and used only there until the thread is joined.
#[cfg(v8_abi)]
unsafe impl Send for TaskPtr {}

/// A script V8 parses and compiles on a thread of its own as its bytes arrive.
pub struct ScriptStreamer {
    feed: Arc<Feed>,
    kind: StreamKind,
    #[cfg(v8_abi)]
    source: Box<StreamedSourceStorage>,
    #[cfg(v8_abi)]
    task: *mut std::ffi::c_void,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The background task has returned.
    done: Arc<AtomicBool>,
}

impl ScriptStreamer {
    /// Starts streaming `feed` into V8 on a new thread; `None` when V8 cannot stream it (or on a
    /// target without the bindings), and the caller compiles the text when it is complete.
    /// `on_done` runs on that thread when V8's part is done ([`ScriptStreamer::is_done`]).
    #[cfg(v8_abi)]
    pub fn start(
        isolate: &v8::Isolate,
        feed: Arc<Feed>,
        encoding: StreamEncoding,
        kind: StreamKind,
        on_done: Box<dyn Fn() + Send>,
    ) -> Option<ScriptStreamer> {
        use std::ffi::c_void;
        let stream = Box::new(SourceStream {
            vtable: source_stream_vtable(),
            feed: feed.clone(),
            encoding,
            first: true,
        });
        let mut stream_ptr = Box::into_raw(stream) as *mut c_void;
        let mut source = Box::new(StreamedSourceStorage([0; 64]));
        let source_ptr = &mut *source as *mut StreamedSourceStorage as *mut c_void;
        let v8_encoding = match encoding {
            StreamEncoding::Utf8 => 2,        // StreamedSource::UTF8
            StreamEncoding::Windows1252 => 3, // StreamedSource::WINDOWS_1252
        };
        // SAFETY: `source_ptr` is 64 bytes of 16-aligned storage that outlives the StreamedSource
        // (it is destroyed in Drop before the box is freed); the constructor takes the stream out of
        // `stream_ptr` (a unique_ptr's storage: one pointer), leaving it null.
        unsafe {
            ffi::streamed_source_construct(source_ptr, &mut stream_ptr, v8_encoding);
        }
        debug_assert!(stream_ptr.is_null());
        let script_type = match kind {
            StreamKind::Classic => 0, // ScriptType::kClassic
            StreamKind::Module => 1,  // ScriptType::kModule
        };
        // SAFETY: the isolate is entered on this thread (the page thread owns it); the source was
        // constructed above. The raw isolate pointer is the C++ `v8::Isolate*`.
        let task = unsafe {
            let raw: v8::UnsafeRawIsolatePtr = isolate.as_raw_isolate_ptr();
            let isolate_ptr: *mut c_void = std::mem::transmute(raw);
            ffi::start_streaming(
                isolate_ptr,
                source_ptr,
                script_type,
                0,
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        let done = Arc::new(AtomicBool::new(false));
        let mut streamer = ScriptStreamer {
            feed,
            kind,
            source,
            task,
            thread: None,
            done: done.clone(),
        };
        if task.is_null() {
            return None;
        }
        let run = TaskPtr(task);
        let spawned = std::thread::Builder::new()
            .name("snowlark-script-stream".into())
            .spawn(move || {
                let run = run;
                // SAFETY: the task stays alive until this thread is joined (Drop and `finish` join
                // before deleting it), and Run is called once.
                unsafe { ffi::streaming_task_run(run.0) };
                done.store(true, Ordering::Release);
                on_done();
            });
        match spawned {
            Ok(t) => {
                streamer.thread = Some(t);
                Some(streamer)
            }
            Err(_) => None,
        }
    }

    #[cfg(not(v8_abi))]
    pub fn start(
        _isolate: &v8::Isolate,
        _feed: Arc<Feed>,
        _encoding: StreamEncoding,
        _kind: StreamKind,
        _on_done: Box<dyn Fn() + Send>,
    ) -> Option<ScriptStreamer> {
        None
    }

    pub fn kind(&self) -> StreamKind {
        self.kind
    }

    /// Whether V8's background part has finished, so that compiling no longer waits for it.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Waits for the background work (the stream must have ended: the body is complete).
    fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Finishes a classic script: V8 finalizes on this thread what its background thread parsed.
    /// `source` is the decoded text of the whole body (the same characters that were streamed).
    pub fn compile_script<'s>(
        mut self,
        scope: &mut v8::PinScope<'s, '_>,
        source: v8::Local<'s, v8::String>,
        origin: &v8::ScriptOrigin<'s>,
    ) -> Streamed<v8::Local<'s, v8::Script>> {
        self.join();
        if !self.feed.usable() || self.kind != StreamKind::Classic {
            return Streamed::Fallback;
        }
        #[cfg(v8_abi)]
        {
            let context = scope.get_current_context();
            let mut out: *const std::ffi::c_void = std::ptr::null();
            // SAFETY: the streaming task has run to completion (joined above); the context, string
            // and origin are live for this call; `out` receives the MaybeLocal's one pointer.
            unsafe {
                ffi::compile_streamed(&mut out, &*context, self.source_ptr(), &*source, origin);
            }
            Streamed::Compiled(local_from_raw::<v8::Script>(scope, out))
        }
        #[cfg(not(v8_abi))]
        {
            let _ = (scope, source, origin);
            Streamed::Fallback
        }
    }

    /// Finishes a module script, as [`ScriptStreamer::compile_script`].
    pub fn compile_module<'s>(
        mut self,
        scope: &mut v8::PinScope<'s, '_>,
        source: v8::Local<'s, v8::String>,
        origin: &v8::ScriptOrigin<'s>,
    ) -> Streamed<v8::Local<'s, v8::Module>> {
        self.join();
        if !self.feed.usable() || self.kind != StreamKind::Module {
            return Streamed::Fallback;
        }
        #[cfg(v8_abi)]
        {
            let context = scope.get_current_context();
            let mut out: *const std::ffi::c_void = std::ptr::null();
            // SAFETY: as in `compile_script`.
            unsafe {
                ffi::compile_module_streamed(
                    &mut out,
                    &*context,
                    self.source_ptr(),
                    &*source,
                    origin,
                );
            }
            Streamed::Compiled(local_from_raw::<v8::Module>(scope, out))
        }
        #[cfg(not(v8_abi))]
        {
            let _ = (scope, source, origin);
            Streamed::Fallback
        }
    }

    #[cfg(v8_abi)]
    fn source_ptr(&mut self) -> *mut std::ffi::c_void {
        &mut *self.source as *mut StreamedSourceStorage as *mut std::ffi::c_void
    }
}

/// A handle V8 returned in the current handle scope, as a `Local` of that scope.
#[cfg(v8_abi)]
fn local_from_raw<'s, T>(
    _scope: &mut v8::PinScope<'s, '_>,
    raw: *const std::ffi::c_void,
) -> Option<v8::Local<'s, T>> {
    let nn = std::ptr::NonNull::new(raw as *mut T)?;
    // SAFETY: `Local<'s, T>` is a `repr(C)` wrapper of one `NonNull<T>` (what V8's `Local` holds);
    // the handle was created in the innermost handle scope, which `_scope` (lifetime 's) is.
    Some(unsafe { std::mem::transmute::<std::ptr::NonNull<T>, v8::Local<'s, T>>(nn) })
}

impl Drop for ScriptStreamer {
    fn drop(&mut self) {
        // The background thread may still wait for bytes: end the stream first.
        self.feed.abort();
        self.join();
        #[cfg(v8_abi)]
        // SAFETY: the task's thread has finished; the StreamedSource was constructed in `start` and
        // is destroyed once, here; the task was allocated by V8 with `new` and is deleted once.
        unsafe {
            ffi::streamed_source_destruct(self.source_ptr());
            if !self.task.is_null() {
                ffi::operator_delete(self.task);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_scope(f: impl FnOnce(&mut v8::PinScope<'_, '_>)) {
        crate::init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let context = {
            v8::scope!(let scope, &mut isolate);
            let context = v8::Context::new(scope, v8::ContextOptions::default());
            v8::Global::new(scope, context)
        };
        v8::scope_with_context!(scope, &mut isolate, &context);
        f(scope);
    }

    fn origin<'s>(scope: &mut v8::PinScope<'s, '_>, module: bool) -> v8::ScriptOrigin<'s> {
        let name = v8::String::new(scope, "https://a.test/s.js").unwrap();
        v8::ScriptOrigin::new(
            scope,
            name.into(),
            0,
            0,
            false,
            0,
            None,
            false,
            false,
            module,
            None,
        )
    }

    #[test]
    fn streams_a_classic_script_in_chunks() {
        with_scope(|scope| {
            let text = "var total = 0;\nfor (let i = 0; i < 10; i++) total += i;\nfunction f(é) { return 'ü' + é; }\ntotal + f(1).length";
            let feed = Feed::new();
            let streamer = ScriptStreamer::start(
                scope,
                feed.clone(),
                StreamEncoding::Utf8,
                StreamKind::Classic,
                Box::new(|| {}),
            )
            .expect("V8 streams classic scripts");
            // UTF-8 split anywhere, BOM first.
            let mut bytes = vec![0xEF, 0xBB, 0xBF];
            bytes.extend_from_slice(text.as_bytes());
            for chunk in bytes.chunks(5) {
                feed.push(chunk);
            }
            feed.finish();
            let source = v8::String::new(scope, text).unwrap();
            let o = origin(scope, false);
            let script = match streamer.compile_script(scope, source, &o) {
                Streamed::Compiled(s) => s.expect("compiles"),
                Streamed::Fallback => panic!("fell back"),
            };
            let v = script.run(scope).unwrap();
            assert_eq!(v.to_rust_string_lossy(scope), "47");
            // Lazily compiled functions read the full source: their text is right.
            let f = v8::String::new(scope, "f.toString()").unwrap();
            let s = v8::Script::compile(scope, f, None).unwrap();
            let r = s.run(scope).unwrap().to_rust_string_lossy(scope);
            assert_eq!(r, "function f(é) { return 'ü' + é; }");
        });
    }

    #[test]
    fn streams_a_module_and_reports_syntax_errors() {
        with_scope(|scope| {
            let feed = Feed::new();
            let streamer = ScriptStreamer::start(
                scope,
                feed.clone(),
                StreamEncoding::Utf8,
                StreamKind::Module,
                Box::new(|| {}),
            )
            .unwrap();
            let text = "import x from './x.js'; export default x + 1;";
            feed.push(text.as_bytes());
            feed.finish();
            let source = v8::String::new(scope, text).unwrap();
            let o = origin(scope, true);
            match streamer.compile_module(scope, source, &o) {
                Streamed::Compiled(Some(m)) => assert_eq!(m.get_module_requests().length(), 1),
                _ => panic!("module did not compile"),
            }

            let feed = Feed::new();
            let streamer = ScriptStreamer::start(
                scope,
                feed.clone(),
                StreamEncoding::Windows1252,
                StreamKind::Classic,
                Box::new(|| {}),
            )
            .unwrap();
            feed.push(b"var = ;");
            feed.finish();
            let source = v8::String::new(scope, "var = ;").unwrap();
            let o = origin(scope, false);
            v8::tc_scope!(let tc, scope);
            match streamer.compile_script(tc, source, &o) {
                Streamed::Compiled(None) => assert!(tc.has_caught()),
                _ => panic!("a syntax error compiles to nothing"),
            }
        });
    }

    #[test]
    fn falls_back_on_abort_or_another_encodings_bom() {
        with_scope(|scope| {
            let feed = Feed::new();
            let streamer = ScriptStreamer::start(
                scope,
                feed.clone(),
                StreamEncoding::Utf8,
                StreamKind::Classic,
                Box::new(|| {}),
            )
            .unwrap();
            feed.push(&[0xFF, 0xFE, b'a', 0]);
            feed.finish();
            let source = v8::String::new(scope, "a").unwrap();
            let o = origin(scope, false);
            assert!(matches!(
                streamer.compile_script(scope, source, &o),
                Streamed::Fallback
            ));

            // Dropped while V8 still waits for bytes: the thread ends.
            let feed = Feed::new();
            let streamer = ScriptStreamer::start(
                scope,
                feed.clone(),
                StreamEncoding::Utf8,
                StreamKind::Classic,
                Box::new(|| {}),
            )
            .unwrap();
            feed.push(b"var a = 1;");
            drop(streamer);
        });
    }
}

#[cfg(test)]
mod gc_while_streaming {
    use super::*;

    /// V8's thread waits in `GetMoreData` for bytes that come late: a garbage collection on the
    /// page thread does not wait for it.
    #[test]
    fn a_collection_does_not_wait_for_a_stream_waiting_for_bytes() {
        crate::init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let feed = Feed::new();
        let streamer = ScriptStreamer::start(
            &isolate,
            feed.clone(),
            StreamEncoding::Utf8,
            StreamKind::Classic,
            Box::new(|| {}),
        )
        .unwrap();
        feed.push(
            b"var a = 1;
function f() { return a; }
",
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        let late = feed.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            late.finish();
        });
        let t = std::time::Instant::now();
        isolate.low_memory_notification();
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
        assert!(!streamer.is_done());
        drop(streamer);
    }
}
