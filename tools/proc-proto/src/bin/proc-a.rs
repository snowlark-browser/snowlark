// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Process A: owns the top-level window and the visual tree, and drives the tests with synthetic
//! input posted to its own window (nothing takes focus: the window is `WS_EX_NOACTIVATE`).
//!
//!   proc-a --mode engine|control|surface|windowchain --test latency|resize|crash --secs N
//!          --out <base path> [--watch] [--raster-ms 100] [--x X --y Y --w W --h H]

use std::cell::Cell;
use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snowlark_paint::Gpu;
use proc_proto::comp::{self, CMsg, Events, Options, Target, Tree, CHROME_H};
use proc_proto::wire::*;
use proc_proto::{arg, arg_or, engine, flag, videohost};
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_APP_RESIZE: u32 = WM_APP + 1;
const WM_APP_KILL: u32 = WM_APP + 2;
const WM_APP_DIED: u32 = WM_APP + 3;
const WM_APP_HANG: u32 = WM_APP + 4;
/// The new B has a frame: show its surfaces (`--keep-old`).
const WM_APP_SWAP: u32 = WM_APP + 5;
/// Start the next B now (`--restart-delay-ms`).
const WM_APP_RESTART: u32 = WM_APP + 6;
const RESIZE_WAIT: Duration = Duration::from_millis(100);

#[derive(Default, Clone, Copy, Debug)]
struct Crash {
    kill: i64,
    detected: i64,
    restarted: i64,
    first_present: i64,
}

struct Remote {
    child: Child,
    to_b: Pipe,
    /// The surface handles of this generation (A's).
    surfaces: (HANDLE, HANDLE),
}

/// The surfaces of a new B, not shown yet.
struct Waiting {
    content: windows::core::IUnknown,
    chrome: Option<windows::core::IUnknown>,
}

struct Host {
    mode: String,
    hwnd: isize,
    size: (u32, u32),
    tx: Option<Sender<CMsg>>,
    comp_thread: Option<std::thread::JoinHandle<()>>,
    acks: Receiver<u32>,
    ack_tx: Sender<u32>,
    seq: u32,
    ack_timeouts: u32,
    tree: Option<Tree>,
    remote: Option<Remote>,
    generation: u32,
    waiting: Option<Waiting>,
    crashes: Arc<Mutex<Vec<Crash>>>,
    out: PathBuf,
}

thread_local! {
    static HOST: Cell<*mut Host> = const { Cell::new(std::ptr::null_mut()) };
}

fn host<'a>() -> Option<&'a mut Host> {
    let p = HOST.with(|h| h.get());
    // SAFETY: set once on the window's thread and only read there, before the window goes; the
    // wndproc re-enters, so references are short and nothing is held across a nested call.
    unsafe { p.as_mut() }
}

fn hwnd_of(h: isize) -> HWND {
    HWND(h as *mut core::ffi::c_void)
}

impl Host {
    fn options(&self) -> Options {
        let raster: u64 = arg_or("--raster-ms", 100);
        Options {
            raster_every: (raster > 0).then(|| Duration::from_millis(raster)),
            frame_base: self.generation * 100_000,
            log: Some(self.out.clone()),
            jitter_ms: arg_or("--jitter", 0.0),
            present_chrome_first: flag("--chrome-first"),
        }
    }

    fn start_in_process(&mut self) {
        let gpu = Gpu::new().expect("device");
        println!("[a] in-process on {}", gpu.adapter());
        let (tx, rx) = channel();
        let ack_tx = self.ack_tx.clone();
        let crashes = self.crashes.clone();
        let events = Events {
            ack: Box::new(move |seq, _| {
                let _ = ack_tx.send(seq);
            }),
            first: Box::new(move |at| {
                FIRST_FRAME.store(true, Ordering::Relaxed);
                let mut c = crashes.lock().unwrap();
                if let Some(last) = c.last_mut() {
                    last.first_present = at;
                }
            }),
        };
        let (hwnd, size, opts, mode) = (self.hwnd, self.size, self.options(), self.mode.clone());
        let tx2 = tx.clone();
        self.comp_thread = Some(std::thread::spawn(move || {
            let r = match mode.as_str() {
                "engine" => engine::run(gpu, hwnd, size, rx, tx2, events, opts),
                "control" => comp::run(gpu, Target::Hwnd(hwnd), size, rx, tx2, events, opts),
                "windowchain" => {
                    comp::run(gpu, Target::WindowChain(hwnd), size, rx, tx2, events, opts)
                }
                m => panic!("mode {m}"),
            };
            if let Err(e) = r {
                println!("[a] compositor failed: {e}");
            }
        }));
        self.tx = Some(tx);
    }

    /// Option 1: makes two surface handles and starts a B to present into them.
    fn start_b(&mut self) {
        let t0 = qpc();
        let tree = self.tree.as_ref().unwrap();
        // SAFETY: handles and surfaces are owned here; the old generation's are closed below.
        let one = self.mode == "surface1";
        let surfaces = unsafe {
            let content = DCompositionCreateSurfaceHandle(3, None).expect("surface handle");
            let cs = tree
                .dev
                .CreateSurfaceFromHandle(content)
                .expect("CreateSurfaceFromHandle");
            let defer = self.generation > 0 && flag("--keep-old");
            if !defer {
                tree.content.SetContent(&cs).expect("SetContent");
            }
            let (chrome, hs) = if one {
                tree.content.SetOffsetY2(0.0).expect("offset");
                (HANDLE::default(), None)
            } else {
                let chrome = DCompositionCreateSurfaceHandle(3, None).expect("surface handle");
                let hs = tree
                    .dev
                    .CreateSurfaceFromHandle(chrome)
                    .expect("CreateSurfaceFromHandle");
                if !defer {
                    tree.chrome.SetContent(&hs).expect("SetContent");
                }
                (chrome, Some(hs))
            };
            tree.dev.Commit().expect("Commit");
            if defer {
                self.waiting = Some(Waiting {
                    content: cs,
                    chrome: hs,
                });
            }
            (content, chrome)
        };
        let (to_b, b_reads) = pipe_to_child(true).expect("pipe");
        let (from_b, b_writes) = pipe_to_child(false).expect("pipe");
        let log = File::create(self.out.with_extension(format!("b{}.log", self.generation)))
            .expect("log");
        let exe = std::env::current_exe()
            .unwrap()
            .with_file_name("proc-b.exe");
        let stem = self.out.file_name().unwrap().to_string_lossy().to_string();
        let out = self
            .out
            .with_file_name(format!("{stem}-g{}", self.generation));
        let mut command = Command::new(exe);
        command
            .args(["--role", "content", "--r"])
            .arg(b_reads.0.to_string())
            .arg("--w")
            .arg(b_writes.0.to_string())
            .arg("--out")
            .arg(&out)
            .args(["--raster-ms", &arg_or("--raster-ms", 100u64).to_string()])
            .args(["--jitter", &arg_or("--jitter", 0.0f64).to_string()]);
        if flag("--chrome-first") {
            command.arg("--chrome-first");
        }
        if flag("--prio") {
            command.arg("--prio");
        }
        let child = command
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn B");
        close(b_reads);
        close(b_writes);
        let process = HANDLE(child.as_raw_handle());
        // SAFETY: duplicating our handles into the child; only A does this, as section 7 asks.
        let dup = |h: HANDLE| unsafe {
            let mut out = HANDLE::default();
            windows::Win32::Foundation::DuplicateHandle(
                GetCurrentProcess(),
                h,
                process,
                &mut out,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
            .expect("DuplicateHandle");
            out.0 as i64
        };
        let mut init = Rec::new(K_INIT);
        init.a = [self.size.0, self.size.1, self.generation * 100_000];
        to_b.write(&init);
        let mut r = Rec::new(K_SURFACES);
        r.t = [dup(surfaces.0), if one { 0 } else { dup(surfaces.1) }, 0];
        to_b.write(&r);
        // Death and messages from B.
        let hwnd = self.hwnd;
        let generation = self.generation;
        let (ack_tx, crashes) = (self.ack_tx.clone(), self.crashes.clone());
        std::thread::spawn(move || {
            while let Some(r) = from_b.read() {
                match r.kind {
                    K_ACK => {
                        let _ = ack_tx.send(r.a[2]);
                    }
                    K_FIRST_PRESENT => {
                        FIRST_FRAME.store(true, Ordering::Relaxed);
                        if let Some(c) = crashes.lock().unwrap().last_mut() {
                            c.first_present = r.t[0];
                        }
                        println!("[a] B generation {generation}: first frame presented");
                        // SAFETY: posting to our own window.
                        let _ = unsafe {
                            PostMessageW(Some(hwnd_of(hwnd)), WM_APP_SWAP, WPARAM(0), LPARAM(0))
                        };
                    }
                    K_FAILED => println!("[a] B failed: {:08x}", r.a[0]),
                    _ => {}
                }
            }
            close(from_b);
        });
        let wait = process.0 as isize;
        std::thread::spawn(move || {
            // SAFETY: the process handle stays open while Remote holds the Child; a closed handle
            // only ends the wait early.
            unsafe {
                WaitForSingleObject(HANDLE(wait as *mut core::ffi::c_void), INFINITE);
                let _ = PostMessageW(
                    Some(hwnd_of(hwnd)),
                    WM_APP_DIED,
                    WPARAM(generation as usize),
                    LPARAM(0),
                );
            }
        });
        if let Some(old) = self.remote.take() {
            close(old.to_b);
            // SAFETY: A's handles of the old generation.
            unsafe {
                let _ = CloseHandle(old.surfaces.0);
                if !old.surfaces.1.is_invalid() && !old.surfaces.1 .0.is_null() {
                    let _ = CloseHandle(old.surfaces.1);
                }
            }
        }
        self.remote = Some(Remote {
            child,
            to_b,
            surfaces,
        });
        println!(
            "[a] generation {} started in {:.2} ms",
            self.generation,
            qpc_ms(qpc() - t0)
        );
    }

    fn scroll(&mut self, dy: f32, input: i64) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(CMsg::Scroll { dy, input });
        } else if let Some(r) = &self.remote {
            let mut m = Rec::new(K_SCROLL);
            m.a[0] = dy.to_bits();
            m.t[0] = input;
            r.to_b.write(&m);
        }
    }

    /// `browser/mod.rs::resized`: tell the compositor and wait, at most RESIZE_WAIT.
    fn resized(&mut self, w: u32, h: u32) {
        if (w, h) == self.size || h <= CHROME_H || w == 0 {
            return;
        }
        self.size = (w, h);
        self.seq += 1;
        let (seq, at) = (self.seq, qpc());
        if let Some(tx) = &self.tx {
            let _ = tx.send(CMsg::Resize { w, h, seq, at });
        } else if let Some(r) = &self.remote {
            let mut m = Rec::new(K_RESIZE);
            m.a = [w, h, seq];
            m.t[0] = at;
            r.to_b.write(&m);
        } else {
            return;
        }
        let deadline = Instant::now() + RESIZE_WAIT;
        loop {
            match self
                .acks
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(s) if s == seq => return,
                Ok(_) => {}
                Err(_) => {
                    self.ack_timeouts += 1;
                    return;
                }
            }
        }
    }

    fn kill(&mut self) {
        let t = qpc();
        if let Some(r) = &self.remote {
            self.crashes.lock().unwrap().push(Crash {
                kill: t,
                ..Default::default()
            });
            // SAFETY: the handle of our own child.
            let _ = unsafe { TerminateProcess(HANDLE(r.child.as_raw_handle()), 9) };
        }
    }

    fn died(&mut self, generation: u32) {
        if generation != self.generation || !self.mode.starts_with("surface") {
            return;
        }
        if QUITTING.load(Ordering::Relaxed) {
            return;
        }
        let t = qpc();
        if let Some(c) = self.crashes.lock().unwrap().last_mut() {
            c.detected = t;
        }
        self.generation += 1;
        let delay: u64 = arg_or("--restart-delay-ms", 0);
        if delay > 0 {
            let hwnd = self.hwnd;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay));
                // SAFETY: posting to our own window.
                let _ = unsafe {
                    PostMessageW(Some(hwnd_of(hwnd)), WM_APP_RESTART, WPARAM(0), LPARAM(0))
                };
            });
        } else {
            self.restart();
        }
    }

    fn restart(&mut self) {
        self.start_b();
        if let Some(c) = self.crashes.lock().unwrap().last_mut() {
            c.restarted = qpc();
        }
    }

    /// Shows the new B's surfaces, once it has a frame in them.
    fn swap(&mut self) {
        if let (Some(w), Some(tree)) = (self.waiting.take(), &self.tree) {
            // SAFETY: setting our own visuals' content.
            unsafe {
                tree.content.SetContent(&w.content).expect("SetContent");
                if let Some(c) = &w.chrome {
                    tree.chrome.SetContent(c).expect("SetContent");
                }
                tree.dev.Commit().expect("Commit");
            }
        }
    }
}

static QUITTING: AtomicBool = AtomicBool::new(false);
static FIRST_FRAME: AtomicBool = AtomicBool::new(false);

/// Runs the window's messages until `done` or `limit`.
fn pump(limit: Duration, done: impl Fn() -> bool) {
    let end = Instant::now() + limit;
    while Instant::now() < end && !done() {
        // SAFETY: the standard message calls on this thread's queue.
        unsafe {
            let mut m = MSG::default();
            while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            let _ = MsgWaitForMultipleObjects(None, false, 5, QS_ALLINPUT);
        }
    }
}

/// Shows the window without activating it (`ShowWindow`'s first call can be overridden by the
/// launcher's STARTUPINFO; this can't) and says where it is.
fn show(hwnd: isize) {
    // SAFETY: calls on our own window.
    unsafe {
        let h = hwnd_of(hwnd);
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let mut r = RECT::default();
        let _ = GetWindowRect(h, &mut r);
        println!(
            "[a] window at {},{} {}x{} visible {} dpi {} foreground {}",
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            IsWindowVisible(h).as_bool(),
            GetDpiForWindow(h),
            GetForegroundWindow() == h
        );
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: called by the system with valid arguments on the window's thread.
    unsafe {
        match msg {
            WM_NCCALCSIZE if wp.0 != 0 => LRESULT(0),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_MOUSEWHEEL => {
                // (the wheel time, as the engine's window takes it)
                let t = qpc();
                if let Some(h) = host() {
                    h.scroll(lp.0 as i32 as f32, t);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let (w, h) = ((lp.0 & 0xffff) as u32, (lp.0 >> 16 & 0xffff) as u32);
                if let Some(host) = host() {
                    host.resized(w, h);
                }
                LRESULT(0)
            }
            WM_APP_RESIZE => {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    wp.0 as i32,
                    lp.0 as i32,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                LRESULT(0)
            }
            WM_APP_KILL => {
                if let Some(h) = host() {
                    h.kill();
                }
                LRESULT(0)
            }
            WM_APP_DIED => {
                if let Some(h) = host() {
                    h.died(wp.0 as u32);
                }
                LRESULT(0)
            }
            WM_APP_SWAP => {
                if let Some(h) = host() {
                    h.swap();
                }
                LRESULT(0)
            }
            WM_APP_RESTART => {
                if let Some(h) = host() {
                    h.restart();
                }
                LRESULT(0)
            }
            WM_APP_HANG => {
                if let Some(h) = host() {
                    if let Some(tx) = &h.tx {
                        let _ = tx.send(CMsg::Hang(wp.0 as u32));
                    } else if let Some(r) = &h.remote {
                        let mut m = Rec::new(K_HANG);
                        m.a[0] = wp.0 as u32;
                        r.to_b.write(&m);
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn create_window() -> isize {
    let (x, y) = (arg_or("--x", 5120 + 1500), arg_or("--y", 700));
    let (w, h) = (arg_or("--w", 900), arg_or("--h", 620));
    // SAFETY: plain window creation; the class outlives the window.
    unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = w!("SnowlarkProcProtoA");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            PCWSTR(w!("Snowlark P0 prototype").as_ptr()),
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .expect("window");
        let margins = windows::Win32::UI::Controls::MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: 1,
            cyBottomHeight: 0,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        let corners = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corners as *const _ as *const core::ffi::c_void,
            std::mem::size_of_val(&corners) as u32,
        );
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        hwnd.0 as isize
    }
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.0 >> 33) as f64 / (1u64 << 31) as f64
    }
}

/// Wheel input: bursts of 1.5 s at random 20-40 ms gaps, 400 ms apart, same in every mode.
fn scroll_driver(hwnd: isize, until: Instant) {
    let mut random = Random(42);
    while Instant::now() < until {
        let burst = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < burst && Instant::now() < until {
            // SAFETY: posting to a window of ours; a dead window only fails the call.
            let _ = unsafe {
                PostMessageW(
                    Some(hwnd_of(hwnd)),
                    WM_MOUSEWHEEL,
                    WPARAM(0),
                    LPARAM(13 + (random.next() * 40.0) as isize),
                )
            };
            std::thread::sleep(Duration::from_secs_f64(0.020 + random.next() * 0.020));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Grows and shrinks the window in steps, one step per ~16 ms, as a drag would.
fn resize_driver(hwnd: isize, until: Instant, w0: i32, h0: i32) {
    let (mut w, mut h, mut dw, mut dh) = (w0, h0, 7, 5);
    while Instant::now() < until {
        w += dw;
        h += dh;
        if !(w0 - 200..=w0 + 300).contains(&w) {
            dw = -dw;
        }
        if !(h0 - 200..=h0 + 200).contains(&h) {
            dh = -dh;
        }
        // SAFETY: as in scroll_driver.
        let _ = unsafe {
            PostMessageW(
                Some(hwnd_of(hwnd)),
                WM_APP_RESIZE,
                WPARAM(w as usize),
                LPARAM(h as isize),
            )
        };
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// How long the window's thread takes to answer, every 5 ms.
fn heartbeat(hwnd: isize, worst: Arc<AtomicI64>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let t = qpc();
        // SAFETY: WM_NULL with a timeout; no pointers.
        let _ = unsafe {
            SendMessageTimeoutW(
                hwnd_of(hwnd),
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_NORMAL,
                5000,
                None,
            )
        };
        worst.fetch_max(qpc() - t, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn start_watch(hwnd: isize, out: &std::path::Path) -> Option<(Child, Pipe)> {
    start_watcher(hwnd, out, "watch")
}

fn start_watcher(hwnd: isize, out: &std::path::Path, role: &str) -> Option<(Child, Pipe)> {
    let (to_w, w_reads) = pipe_to_child(true).ok()?;
    let exe = std::env::current_exe().ok()?.with_file_name("proc-b.exe");
    let log = File::create(out.with_extension("watch.log")).ok()?;
    let child = Command::new(exe)
        .args(["--role", role, "--r"])
        .arg(w_reads.0.to_string())
        .args(["--hwnd", &hwnd.to_string()])
        .arg("--out")
        .arg(out.with_extension("watch.tsv"))
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdout(Stdio::from(log.try_clone().ok()?))
        .stderr(Stdio::from(log))
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    close(w_reads);
    Some((child, to_w))
}

fn main() {
    // SAFETY: first thing in the process.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let mode = arg("--mode").unwrap_or("surface".into());
    let test = arg("--test").unwrap_or("latency".into());
    let secs: f64 = arg_or("--secs", 20.0);
    let out = PathBuf::from(arg("--out").expect("--out"));
    let (w0, h0): (i32, i32) = (arg_or("--w", 900), arg_or("--h", 620));

    let hwnd = create_window();
    let (ack_tx, acks) = channel();
    let host_box = Box::new(Host {
        mode: mode.clone(),
        hwnd,
        size: (w0 as u32, h0 as u32),
        tx: None,
        comp_thread: None,
        acks,
        ack_tx,
        seq: 0,
        ack_timeouts: 0,
        tree: None,
        remote: None,
        generation: 0,
        waiting: None,
        crashes: Arc::default(),
        out: out.clone(),
    });
    let raw = Box::into_raw(host_box);
    HOST.with(|h| h.set(raw));
    // SAFETY: the Box stays alive to the end of main.
    let h = unsafe { &mut *raw };
    if test == "video" {
        show(hwnd);
        let out2 = out.clone();
        let t = std::thread::spawn(move || {
            let watch = flag("--watch")
                .then(|| start_watcher(hwnd, &out2, "watch-video"))
                .flatten();
            if let Err(e) = videohost::run(hwnd, &out2, secs) {
                println!("[a] video host failed: {e} ({:08x})", e.code().0);
            }
            if let Some((mut child, pipe)) = watch {
                pipe.write(&Rec::new(K_QUIT));
                close(pipe);
                let _ = child.wait();
            }
            // SAFETY: as in scroll_driver.
            let _ = unsafe { PostMessageW(Some(hwnd_of(hwnd)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        });
        // SAFETY: the standard message loop.
        unsafe {
            let mut m = MSG::default();
            while GetMessageW(&mut m, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&m);
                DispatchMessageW(&m);
            }
        }
        let _ = t.join();
        std::process::exit(0);
    }
    if mode.starts_with("surface") {
        // No Direct3D device in A: the claim of section 8.
        h.tree = Some(Tree::new(None, hwnd).expect("visual tree with no device"));
        h.start_b();
    } else {
        h.start_in_process();
    }
    show(hwnd);
    // Let the first frame land, then start the clock.
    let t0 = Instant::now();
    pump(Duration::from_secs(60), || {
        FIRST_FRAME.load(Ordering::Relaxed)
    });
    println!(
        "[a] first frame after {:.0} ms: {}",
        t0.elapsed().as_secs_f64() * 1000.0,
        FIRST_FRAME.load(Ordering::Relaxed)
    );
    pump(
        Duration::from_millis(arg_or("--settle-ms", 1000u64)),
        || false,
    );
    let watch = flag("--watch").then(|| start_watch(hwnd, &out)).flatten();
    if watch.is_some() {
        pump(Duration::from_millis(1000), || false);
    }

    let until = Instant::now() + Duration::from_secs_f64(secs);
    let stop = Arc::new(AtomicBool::new(false));
    let worst = Arc::new(AtomicI64::new(0));
    {
        let (worst, stop) = (worst.clone(), stop.clone());
        std::thread::spawn(move || heartbeat(hwnd, worst, stop));
    }
    let t = test.clone();
    std::thread::spawn(move || {
        let kill_every: f64 = arg_or("--kill-every", 3.0);
        match t.as_str() {
            "latency" => scroll_driver(hwnd, until),
            "resize" => resize_driver(hwnd, until, w0, h0),
            "hang" => {
                // A compositor that stops answering for 3 s in the middle of a resize.
                let r = std::thread::spawn(move || resize_driver(hwnd, until, w0, h0));
                std::thread::sleep(Duration::from_secs(2));
                // SAFETY: as in scroll_driver.
                let _ = unsafe {
                    PostMessageW(Some(hwnd_of(hwnd)), WM_APP_HANG, WPARAM(3000), LPARAM(0))
                };
                let _ = r.join();
            }
            "crash" => {
                let s = std::thread::spawn(move || scroll_driver(hwnd, until));
                let r = std::thread::spawn(move || {
                    if flag("--crash-resize") {
                        resize_driver(hwnd, until, w0, h0)
                    }
                });
                while Instant::now() + Duration::from_secs_f64(kill_every) < until {
                    std::thread::sleep(Duration::from_secs_f64(kill_every));
                    // SAFETY: as in scroll_driver.
                    let _ = unsafe {
                        PostMessageW(Some(hwnd_of(hwnd)), WM_APP_KILL, WPARAM(0), LPARAM(0))
                    };
                }
                let _ = s.join();
                let _ = r.join();
            }
            _ => std::thread::sleep(until.saturating_duration_since(Instant::now())),
        }
        std::thread::sleep(Duration::from_millis(400));
        QUITTING.store(true, Ordering::Relaxed);
        // SAFETY: as in scroll_driver.
        let _ = unsafe { PostMessageW(Some(hwnd_of(hwnd)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    });

    // SAFETY: the standard message loop.
    unsafe {
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
    stop.store(true, Ordering::Relaxed);

    println!("[a] window closed");
    if let Some((mut child, pipe)) = watch {
        pipe.write(&Rec::new(K_QUIT));
        close(pipe);
        let _ = child.wait();
    }
    println!("[a] watcher done");
    if let Some(tx) = h.tx.take() {
        let _ = tx.send(CMsg::Quit);
    }
    if let Some(t) = h.comp_thread.take() {
        let _ = t.join();
    }
    if let Some(mut r) = h.remote.take() {
        r.to_b.write(&Rec::new(K_QUIT));
        close(r.to_b);
        // (B writes its logs now: killing it early would cut them)
        let t = Instant::now();
        let end = t + Duration::from_secs(40);
        while Instant::now() < end && !matches!(r.child.try_wait(), Ok(Some(_))) {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !matches!(r.child.try_wait(), Ok(Some(_))) {
            println!("[a] B did not exit in 40 s");
            let _ = r.child.kill();
        }
        println!(
            "[a] B exit took {:.0} ms",
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    let crashes = h.crashes.lock().unwrap();
    let mut summary = format!(
        "[a] mode {mode} test {test}: window thread worst stall {:.2} ms, resize acks timed out {}, resizes {}\n",
        qpc_ms(worst.load(Ordering::Relaxed)),
        h.ack_timeouts,
        h.seq
    );
    for c in crashes.iter() {
        summary += &format!(
            "[a] crash: detected {:.2} ms, restarted {:.2} ms, first present {:.2} ms after the kill\n",
            qpc_ms(c.detected - c.kill),
            qpc_ms(c.restarted - c.kill),
            qpc_ms(c.first_present - c.kill)
        );
    }
    print!("{summary}");
    if let Err(e) = std::fs::write(out.with_extension("a.txt"), summary) {
        println!("[a] log write failed: {e}");
    }
    std::process::exit(0);
}
