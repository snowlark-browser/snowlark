// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A sampling profiler for the native (Rust and C++) side of a thread, for `SNOWLARK_PROFILE=1`.
//!
//! V8's CPU profile attributes the time of a binding call to the JS function that made it; this
//! sampler says where that time goes inside the engine. A sampler thread suspends the profiled
//! thread at a fixed interval, copies its registers and the live part of its stack, resumes it, and
//! only then unwinds the copy with the unwind tables (`RtlVirtualUnwind`), as Chromium's stack
//! sampler does: nothing that could take a lock the suspended thread holds runs while it is
//! suspended. Frames are named with dbghelp from the PDB when the report is made.
//!
//! Frames in V8's generated code are unwound when V8 registered unwind data for them; the walk
//! stops at the first frame without any.
//!
//! A sample also records the last [`SampleMarks::mark`] the thread passed, as an outermost frame
//! `mark:<name>` in the folded stacks: what the sampled time belongs to, in a run that marks its
//! phases (Speedometer's `performance.mark` calls: `tools/speedometer/split_scored.py`).
//!
//! Only Windows has the sampler: elsewhere [`NativeSampler::start`] returns `None` and frames are
//! not named.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use windows::core::PCSTR;
#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows::Win32::System::Diagnostics::Debug::{
    GetThreadContext, RtlLookupFunctionEntry, RtlVirtualUnwind, SymAddrIncludeInlineTrace,
    SymFromAddr, SymFromInlineContext, SymGetLineFromAddr64, SymGetLineFromInlineContext,
    SymInitialize, SymQueryInlineTrace, SymSetOptions, CONTEXT, CONTEXT_CONTROL_AMD64,
    CONTEXT_INTEGER_AMD64, IMAGEHLP_LINE64, SYMBOL_INFO, SYMOPT_DEFERRED_LOADS, SYMOPT_LOAD_LINES,
    SYMOPT_UNDNAME, UNW_FLAG_NHANDLER,
};
#[cfg(windows)]
use windows::Win32::System::Memory::{
    VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_GUARD, PAGE_NOACCESS,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread,
    THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

/// The most stack copied per sample, and frames kept per sample.
#[cfg(windows)]
const STACK_COPY: usize = 512 * 1024;
#[cfg(windows)]
const MAX_FRAMES: usize = 96;

/// The bits of a sample's outermost entry when it is a mark's number, not a return address (an
/// address of the kernel's half: no user-mode frame has it).
const MARK_FRAME: u64 = 0xffff_0000_0000_0000;

/// The marks a sampled thread passes, shared with the sampler.
#[derive(Clone, Default)]
pub struct SampleMarks {
    /// The last mark's number in `names`, plus one (0: none yet).
    current: Arc<AtomicU64>,
    names: Arc<Mutex<Vec<String>>>,
}

impl SampleMarks {
    /// The samples from now on belong to `name`.
    pub fn mark(&self, name: &str) {
        let mut names = self.names.lock().unwrap();
        let n = match names.iter().position(|n| n == name) {
            Some(n) => n,
            None => {
                names.push(name.to_owned());
                names.len() - 1
            }
        };
        self.current.store(n as u64 + 1, Ordering::Relaxed);
    }

    /// The frame a mark's number stands for (`;` separates frames in the folded format).
    fn frame(&self, entry: u64) -> Option<String> {
        let n = (entry & !MARK_FRAME).checked_sub(1)?;
        let names = self.names.lock().unwrap();
        Some(format!("mark:{}", names.get(n as usize)?.replace(';', "_")))
    }
}

/// `GetThreadContext` wants a 16-byte aligned `CONTEXT`.
#[cfg(windows)]
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Context(CONTEXT);

/// A thread handle the sampler thread may use.
#[cfg(windows)]
struct Thread(HANDLE);

// SAFETY: a thread handle is a kernel object reference, usable from any thread of the process.
#[cfg(windows)]
unsafe impl Send for Thread {}

/// Samples the thread that started it until stopped.
pub struct NativeSampler {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<(Vec<Vec<u64>>, Duration)>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NativeSampler {
    /// Starts sampling the calling thread every `interval`, each sample with the last of `marks`.
    /// `None` when the thread cannot be opened for suspension.
    #[cfg(not(windows))]
    pub fn start(_interval: Duration, _marks: SampleMarks) -> Option<NativeSampler> {
        None
    }

    /// Starts sampling the calling thread every `interval`, each sample with the last of `marks`.
    /// `None` when the thread cannot be opened for suspension.
    #[cfg(windows)]
    pub fn start(interval: Duration, marks: SampleMarks) -> Option<NativeSampler> {
        // SAFETY: plain Win32 calls on the current thread's id; the handle is closed by the
        // sampler thread when it ends.
        let handle = unsafe {
            OpenThread(
                THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                false,
                GetCurrentThreadId(),
            )
        }
        .ok()?;
        let target = Thread(handle);
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new((Vec::new(), Duration::ZERO)));
        let (s, out) = (stop.clone(), samples.clone());
        let thread = std::thread::Builder::new()
            .name("snowlark-native-sampler".into())
            .spawn(move || {
                let target = target;
                let mut stack = vec![0u64; STACK_COPY / 8];
                let mut local = Vec::new();
                let started = std::time::Instant::now();
                while !s.load(Ordering::Relaxed) {
                    std::thread::sleep(interval);
                    if let Some(mut frames) = sample(&target, &mut stack) {
                        match marks.current.load(Ordering::Relaxed) {
                            0 => {}
                            mark => frames.push(MARK_FRAME | mark),
                        }
                        local.push(frames);
                    }
                }
                // SAFETY: the handle was opened above and is not used after this.
                unsafe {
                    let _ = CloseHandle(target.0);
                }
                *out.lock().unwrap() = (local, started.elapsed());
            })
            .ok()?;
        Some(NativeSampler {
            stop,
            samples,
            thread: Some(thread),
        })
    }

    /// Stops sampling and returns the stacks (innermost frame first) and how long the sampling
    /// ran (the sleep between samples is often longer than asked: divide this by the samples for
    /// the time a sample stands for).
    /// How many samples have been taken so far (the sampler keeps running).
    pub fn sample_count(&self) -> usize {
        self.samples.lock().map(|s| s.0.len()).unwrap_or(0)
    }

    pub fn stop(mut self) -> (Vec<Vec<u64>>, Duration) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        std::mem::take(&mut *self.samples.lock().unwrap())
    }
}

impl Drop for NativeSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Whether the region holding a suspended thread's stack pointer may be read from another thread.
/// Not when it is the stack's guard page: a thread can be suspended after its stack pointer moved
/// into the guard page and before it touched it, and reading a guard page from a thread that does
/// not own the stack raises `STATUS_GUARD_PAGE_VIOLATION`, which nothing handles (the sampled
/// process died of it under `SNOWLARK_PROFILE=1`). Chromium's sampler skips such samples
/// (`base/profiler/suspendable_thread_delegate_win.cc`, `CanCopyStack`, `PointsToGuardPage`).
#[cfg(windows)]
fn can_copy_stack(region: &MEMORY_BASIC_INFORMATION) -> bool {
    region.State == MEM_COMMIT
        && (region.Protect & PAGE_GUARD).0 == 0
        && (region.Protect & PAGE_NOACCESS).0 == 0
}

/// One sample of `target`: its stack's return addresses, innermost first.
#[cfg(windows)]
fn sample(target: &Thread, stack: &mut [u64]) -> Option<Vec<u64>> {
    let mut ctx = Context(CONTEXT::default());
    ctx.0.ContextFlags = CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64;
    let (rsp, top, copied);
    // SAFETY: the thread is suspended between SuspendThread and ResumeThread; in between only the
    // context and its stack are read (no allocation, no lock), from memory VirtualQuery says is
    // the committed stack region holding rsp.
    unsafe {
        if SuspendThread(target.0) == u32::MAX {
            return None;
        }
        if GetThreadContext(target.0, &mut ctx.0).is_err() {
            ResumeThread(target.0);
            return None;
        }
        rsp = ctx.0.Rsp;
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        let n = VirtualQuery(
            Some(rsp as *const _),
            &mut mbi,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        );
        if n == 0 || rsp % 8 != 0 || !can_copy_stack(&mbi) {
            ResumeThread(target.0);
            return None;
        }
        top = mbi.BaseAddress as u64 + mbi.RegionSize as u64;
        copied = (((top - rsp) / 8) as usize).min(stack.len());
        std::ptr::copy_nonoverlapping(rsp as *const u64, stack.as_mut_ptr(), copied);
        ResumeThread(target.0);
    }
    // Rebase pointers into the stack onto the copy, so the unwinder reads the copy.
    let base = stack.as_ptr() as u64;
    let end = rsp + copied as u64 * 8;
    let rebase = |v: u64| {
        if v >= rsp && v < end {
            v - rsp + base
        } else {
            v
        }
    };
    for w in &mut stack[..copied] {
        *w = rebase(*w);
    }
    ctx.0.Rsp = base;
    {
        let c = &mut ctx.0;
        for r in [
            &mut c.Rbp, &mut c.Rbx, &mut c.Rsi, &mut c.Rdi, &mut c.R12, &mut c.R13, &mut c.R14,
            &mut c.R15,
        ] {
            *r = rebase(*r);
        }
    }
    let copy_end = base + copied as u64 * 8;
    // Unwinding reads saved registers above rsp: keep a margin below the end of a truncated copy.
    let limit = if copied == stack.len() {
        copy_end.saturating_sub(64 * 1024)
    } else {
        copy_end
    };
    let in_copy = |v: u64| v >= base && v + 8 <= limit;
    let mut frames = Vec::with_capacity(32);
    let mut leaf_guess = true;
    while frames.len() < MAX_FRAMES {
        let pc = ctx.0.Rip;
        if pc == 0 {
            break;
        }
        frames.push(pc);
        if !in_copy(ctx.0.Rsp) {
            break;
        }
        let mut image_base = 0u64;
        // SAFETY: lookups and unwinding on the copied context, whose stack pointer is inside the
        // copy (checked above); the function table entry comes from the system for `pc`.
        unsafe {
            let entry = RtlLookupFunctionEntry(pc, &mut image_base, None);
            if entry.is_null() {
                // A leaf function without unwind data: its return address is at rsp. Only
                // once, at the top of the stack; elsewhere the walk cannot go on.
                if !leaf_guess {
                    break;
                }
                ctx.0.Rip = *(ctx.0.Rsp as *const u64);
                ctx.0.Rsp += 8;
            } else {
                // A function with a frame register: the unwinder reads the frame through it, so
                // it must point into the copy (it may hold anything else in the prolog or when
                // the sample landed in code that does not keep it).
                let info = image_base + (*entry).Anonymous.UnwindInfoAddress as u64;
                let frame_register = *((info + 3) as *const u8) & 0x0f;
                if frame_register != 0 && !in_copy(register(&ctx.0, frame_register)) {
                    break;
                }
                let mut handler_data = std::ptr::null_mut();
                let mut establisher = 0u64;
                RtlVirtualUnwind(
                    UNW_FLAG_NHANDLER,
                    image_base,
                    pc,
                    entry,
                    &mut ctx.0,
                    &mut handler_data,
                    &mut establisher,
                    None,
                );
            }
        }
        leaf_guess = false;
    }
    Some(frames)
}

/// The value of integer register `n` (x64 encoding: 0 = rax ... 15 = r15) in `c`.
#[cfg(windows)]
fn register(c: &CONTEXT, n: u8) -> u64 {
    match n {
        0 => c.Rax,
        1 => c.Rcx,
        2 => c.Rdx,
        3 => c.Rbx,
        4 => c.Rsp,
        5 => c.Rbp,
        6 => c.Rsi,
        7 => c.Rdi,
        8 => c.R8,
        9 => c.R9,
        10 => c.R10,
        11 => c.R11,
        12 => c.R12,
        13 => c.R13,
        14 => c.R14,
        _ => c.R15,
    }
}

/// What the samples add up to, by function.
#[derive(Clone, Debug, Default)]
pub struct NativeSummary {
    pub samples: usize,
    /// (function, samples where it is the innermost frame, samples where it is on the stack).
    pub functions: Vec<(String, usize, usize)>,
    /// Samples by stack, outermost frame first, frames joined with `;` (the "folded" format
    /// flame graph tools read).
    pub folded: HashMap<String, usize>,
}

/// Names the samples' frames (dbghelp, from the PDB) and counts self and total samples per
/// function. `marks` is what the sampler was given.
pub fn summarize(samples: &[Vec<u64>], marks: &SampleMarks) -> NativeSummary {
    let mut names: HashMap<u64, String> = HashMap::new();
    // SNOWLARK_PROFILE_LINES=1: the two innermost frames also name their source line (`@file:line`,
    // the innermost inlined code there), to place self time inside a function that inlines much.
    let lines = std::env::var("SNOWLARK_PROFILE_LINES").is_ok_and(|v| v == "1");
    #[cfg(windows)]
    {
        // The linker records only the PDB's file name (rustc passes /PDBALTPATH:%_PDB%): search the
        // executable's directory, then dbghelp's defaults.
        let mut path = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.display().to_string()))
            .unwrap_or_default();
        path.push_str(";.\0");
        // SAFETY: dbghelp is initialized once for this process here and only used on this thread;
        // the search path is NUL-terminated.
        unsafe {
            let load_lines = if lines { SYMOPT_LOAD_LINES } else { 0 };
            SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | load_lines);
            let _ = SymInitialize(GetCurrentProcess(), PCSTR(path.as_ptr()), true);
        }
    }
    // (per address: the folded frames, innermost first, that the two innermost frames expand to)
    let mut lined: HashMap<u64, Vec<String>> = HashMap::new();
    let mut name = |pc: u64| -> String {
        names
            .entry(pc)
            .or_insert_with(|| symbol(pc).unwrap_or_else(|| format!("0x{pc:x}")))
            .clone()
    };
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    let mut folded: HashMap<String, usize> = HashMap::new();
    for s in samples {
        let mut seen: Vec<String> = Vec::new();
        let mut stack: Vec<String> = Vec::with_capacity(s.len());
        for (i, &pc) in s.iter().enumerate() {
            if pc & MARK_FRAME == MARK_FRAME {
                stack.extend(marks.frame(pc));
                continue;
            }
            // A return address points after the call: look up the call itself.
            let at = if i == 0 { pc } else { pc - 1 };
            let n = name(at);
            if lines && i < 2 {
                let frames = lined.entry(at).or_insert_with(|| {
                    let mut f = inline_frames(at);
                    f.push(match source_line(at) {
                        Some(l) => format!("{}@{l}", short(&n)),
                        None => short(&n),
                    });
                    f
                });
                stack.extend(frames.iter().cloned());
            } else {
                stack.push(short(&n));
            }
            let e = counts.entry(n.clone()).or_default();
            if i == 0 {
                e.0 += 1;
            }
            if !seen.contains(&n) {
                e.1 += 1;
                seen.push(n);
            }
        }
        stack.reverse();
        *folded.entry(stack.join(";")).or_default() += 1;
    }
    let mut functions: Vec<_> = counts.into_iter().map(|(n, (s, t))| (n, s, t)).collect();
    functions.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
    NativeSummary {
        samples: samples.len(),
        functions,
        folded,
    }
}

#[cfg(not(windows))]
fn symbol(_pc: u64) -> Option<String> {
    None
}

#[cfg(not(windows))]
fn inline_frames(_pc: u64) -> Vec<String> {
    Vec::new()
}

#[cfg(not(windows))]
fn source_line(_pc: u64) -> Option<String> {
    None
}

#[cfg(windows)]
fn symbol(pc: u64) -> Option<String> {
    #[repr(C, align(8))]
    struct Buf {
        info: SYMBOL_INFO,
        name: [u8; 1024],
    }
    let mut buf = Buf {
        info: SYMBOL_INFO::default(),
        name: [0; 1024],
    };
    buf.info.SizeOfStruct = std::mem::size_of::<SYMBOL_INFO>() as u32;
    buf.info.MaxNameLen = 1024;
    let mut displacement = 0u64;
    // SAFETY: the buffer holds a SYMBOL_INFO followed by MaxNameLen bytes for the name.
    unsafe {
        SymFromAddr(
            GetCurrentProcess(),
            pc,
            Some(&mut displacement),
            &mut buf.info,
        )
        .ok()?;
        let len = (buf.info.NameLen as usize).min(1024);
        let p = buf.info.Name.as_ptr() as *const u8;
        Some(String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned())
    }
}

/// The functions inlined at `pc`, innermost first, each as `function@file:line` (the line inside
/// it), from the PDB's inline sites.
#[cfg(windows)]
fn inline_frames(pc: u64) -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: plain dbghelp queries on this process with sized out-parameters; the symbol buffer
    // holds a SYMBOL_INFO followed by MaxNameLen bytes, the file name is read before the next call.
    unsafe {
        let process = GetCurrentProcess();
        let count = SymAddrIncludeInlineTrace(process, pc);
        if count == 0 {
            return out;
        }
        let (mut context, mut index) = (0u32, 0u32);
        if SymQueryInlineTrace(process, pc, 0, pc, pc, &mut context, &mut index).is_err() {
            return out;
        }
        for c in context..context + count {
            #[repr(C, align(8))]
            struct Buf {
                info: SYMBOL_INFO,
                name: [u8; 1024],
            }
            let mut buf = Buf {
                info: SYMBOL_INFO::default(),
                name: [0; 1024],
            };
            buf.info.SizeOfStruct = std::mem::size_of::<SYMBOL_INFO>() as u32;
            buf.info.MaxNameLen = 1024;
            let mut displacement = 0u64;
            let name =
                if SymFromInlineContext(process, pc, c, Some(&mut displacement), &mut buf.info)
                    .is_ok()
                {
                    let len = (buf.info.NameLen as usize).min(1024);
                    let p = buf.info.Name.as_ptr() as *const u8;
                    short(&String::from_utf8_lossy(std::slice::from_raw_parts(p, len)))
                } else {
                    "?inline".to_owned()
                };
            let mut line = IMAGEHLP_LINE64 {
                SizeOfStruct: std::mem::size_of::<IMAGEHLP_LINE64>() as u32,
                ..Default::default()
            };
            let mut d32 = 0u32;
            let at =
                if SymGetLineFromInlineContext(process, pc, c, None, &mut d32, &mut line).is_ok() {
                    let file = line.FileName.to_string().unwrap_or_default();
                    let file = file.rsplit(['\\', '/']).next().unwrap_or("").to_owned();
                    format!("@{file}:{}", line.LineNumber)
                } else {
                    String::new()
                };
            out.push(format!("[inl]{name}{at}"));
        }
    }
    out
}

/// `file:line` of the code at `pc` (the file's name only), from the PDB's line tables.
#[cfg(windows)]
fn source_line(pc: u64) -> Option<String> {
    let mut line = IMAGEHLP_LINE64 {
        SizeOfStruct: std::mem::size_of::<IMAGEHLP_LINE64>() as u32,
        ..Default::default()
    };
    let mut displacement = 0u32;
    // SAFETY: `line` is a sized IMAGEHLP_LINE64; the file name dbghelp returns is a NUL-terminated
    // string it owns, read before the next dbghelp call.
    unsafe {
        SymGetLineFromAddr64(GetCurrentProcess(), pc, &mut displacement, &mut line).ok()?;
        let file = line.FileName.to_string().ok()?;
        let file = file.rsplit(['\\', '/']).next().unwrap_or(&file).to_owned();
        Some(format!("{file}:{}", line.LineNumber))
    }
}

impl NativeSummary {
    /// The stacks in the folded format, one `frame;frame;... count` line each.
    pub fn folded_text(&self) -> String {
        let mut v: Vec<_> = self.folded.iter().collect();
        v.sort();
        let mut o = String::new();
        for (stack, n) in v {
            let _ = writeln!(o, "{stack} {n}");
        }
        o
    }

    /// The `top` functions by self samples and by total samples, as `[profile]` lines, in
    /// milliseconds with each sample standing for `per_sample`.
    pub fn report(&self, top: usize, per_sample: Duration) -> String {
        let ms = |n: usize| n as f64 * per_sample.as_secs_f64() * 1000.0;
        let mut o = String::new();
        let _ = writeln!(
            o,
            "[profile] native: {} samples, one per {} us",
            self.samples,
            per_sample.as_micros()
        );
        let _ = writeln!(
            o,
            "[profile] {:>10} {:>10}  native function (by self)",
            "self ms", "total ms"
        );
        for (f, s, t) in self.functions.iter().take(top) {
            let _ = writeln!(
                o,
                "[profile] {:>10.1} {:>10.1}  {}",
                ms(*s),
                ms(*t),
                short(f)
            );
        }
        let mut by_total: Vec<_> = self.functions.iter().collect();
        by_total.sort_by_key(|f| std::cmp::Reverse(f.2));
        let _ = writeln!(
            o,
            "[profile] {:>10} {:>10}  native function (by total)",
            "self ms", "total ms"
        );
        for (f, s, t) in by_total.iter().take(top) {
            let _ = writeln!(
                o,
                "[profile] {:>10.1} {:>10.1}  {}",
                ms(*s),
                ms(*t),
                short(f)
            );
        }
        o
    }
}

/// A function name without generic arguments (they make Rust names unreadable in a table).
fn short(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0;
    for c in name.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn a_stack_pointer_in_a_guard_page_is_not_copied() {
        use windows::Win32::System::Memory::{
            VirtualAlloc, VirtualFree, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
        };
        let region = |p: *const u8| {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: a query of this process's address space into a local.
            let n = unsafe {
                VirtualQuery(
                    Some(p.cast()),
                    &mut mbi,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            assert_ne!(n, 0);
            mbi
        };
        // This thread's own stack is copied.
        let here = 0u64;
        assert!(can_copy_stack(&region(std::ptr::addr_of!(here).cast())));
        // A committed page with the guard flag, as the page below a thread's stack is, is not; nor
        // is a reserved one.
        // SAFETY: fresh allocations of this test's own, released below and never dereferenced.
        unsafe {
            let guard = VirtualAlloc(None, 4096, MEM_COMMIT, PAGE_READWRITE | PAGE_GUARD);
            assert!(!guard.is_null());
            assert!(!can_copy_stack(&region(guard.cast())));
            let reserved = VirtualAlloc(None, 65536, MEM_RESERVE, PAGE_NOACCESS);
            assert!(!reserved.is_null());
            assert!(!can_copy_stack(&region(reserved.cast())));
            VirtualFree(guard, 0, MEM_RELEASE).unwrap();
            VirtualFree(reserved, 0, MEM_RELEASE).unwrap();
        }
    }

    #[cfg(windows)]
    #[inline(never)]
    fn spin_for(d: Duration) -> u64 {
        let t = std::time::Instant::now();
        let mut x = 0u64;
        while t.elapsed() < d {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
        }
        x
    }

    #[cfg(windows)]
    #[test]
    fn samples_and_names_the_running_thread() {
        // Under a loaded machine (the whole workspace's tests in parallel) the sampler thread and the
        // spinning thread are both descheduled for long stretches, so one 150 ms window can hold too
        // few samples. The sampler is not at fault for that: spin until enough samples exist, with a
        // generous cap, and judge the attribution proportionally.
        let marks = SampleMarks::default();
        let sampler =
            NativeSampler::start(Duration::from_micros(500), marks.clone()).expect("a sampler");
        let started = std::time::Instant::now();
        let mut spun = Duration::ZERO;
        let (samples, wall) = loop {
            spin_for(Duration::from_millis(150));
            spun += Duration::from_millis(150);
            if spun >= Duration::from_millis(150) * 8 || started.elapsed() >= Duration::from_secs(4)
            {
                break sampler.stop();
            }
            if sampler.sample_count() > 60 {
                break sampler.stop();
            }
        };
        assert!(wall >= Duration::from_millis(150));
        assert!(
            samples.len() > 20,
            "{} samples in {:?}",
            samples.len(),
            wall
        );
        let s = summarize(&samples, &marks);
        let needed = (samples.len() / 4).max(5);
        assert!(
            s.functions
                .iter()
                .any(|f| f.0.contains("spin_for") && f.2 > needed),
            "{:?}",
            &s.functions[..s.functions.len().min(10)]
        );
        assert!(s.report(5, Duration::from_micros(500)).contains("native"));
    }

    #[cfg(windows)]
    #[test]
    fn samples_carry_the_last_mark_as_their_outermost_frame() {
        let marks = SampleMarks::default();
        let sampler =
            NativeSampler::start(Duration::from_micros(500), marks.clone()).expect("a sampler");
        spin_for(Duration::from_millis(100));
        marks.mark("one;two");
        spin_for(Duration::from_millis(100));
        marks.mark("three");
        spin_for(Duration::from_millis(100));
        marks.mark("one;two");
        spin_for(Duration::from_millis(100));
        let (samples, _) = sampler.stop();
        let s = summarize(&samples, &marks);
        let with = |prefix: &str| {
            s.folded
                .iter()
                .filter(|(stack, _)| stack.starts_with(prefix))
                .map(|(_, n)| n)
                .sum::<usize>()
        };
        assert!(with("mark:one_two;") > 0, "{:?}", s.folded.keys().next());
        assert!(with("mark:three;") > 0, "{:?}", s.folded.keys().next());
        // (before the first mark: as they always were)
        assert_eq!(
            with("mark:")
                + s.folded
                    .iter()
                    .filter(|(k, _)| !k.contains("mark:"))
                    .map(|(_, n)| n)
                    .sum::<usize>(),
            samples.len()
        );
        assert!(s.functions.iter().all(|f| !f.0.contains("mark:")));
    }

    #[test]
    fn generic_arguments_are_left_out() {
        assert_eq!(short("a::b<c::d<e>>::f<g>"), "a::b::f");
    }
}
