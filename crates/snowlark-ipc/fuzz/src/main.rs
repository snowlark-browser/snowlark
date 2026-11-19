// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A coverage-guided fuzzer for what a hostile child can send: bytes on the pipe, through framing and
//! the browser's dispatch, to `recv` and the decoders.
//!
//! Without nightly, clang or libFuzzer, this is the small part of one that is needed:
//! the counters of stable rustc's `-Cinstrument-coverage` say what an input reached, an input that
//! reaches something new joins the corpus, and inputs are mutated from the corpus or generated as
//! valid messages of random shape and then corrupted. A finding is a panic, an exec that takes too long,
//! or an allocation out of proportion to the input (section 7: "allocation bounded by the bytes
//! present"). Anything worse ends the process, with the seed in hand to run it again.
//!
//! `--plant` adds a bug that only a run of the right bytes through the real path reaches. A fuzzer
//! that finds nothing proves nothing until it has been seen to find something.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use snowlark_ipc::frame::{self, Kind};
use snowlark_ipc::loopback::pair;
use snowlark_ipc::wire;
use snowlark_ipc::{BadMessage, Error, ProcessPolicy, Received, Validate};
use serde::{Deserialize, Serialize};

// --- Coverage: the counters of `-Cinstrument-coverage`, read in place.

extern "C" {
    fn __llvm_profile_begin_counters() -> *mut u64;
    fn __llvm_profile_end_counters() -> *mut u64;
}

fn counters() -> &'static mut [u64] {
    // SAFETY: the profiler runtime's bounds of the counter section, which lives as long as the
    // process. Only this thread runs instrumented code while the slice is in use: the target runs
    // on this thread, and the watchdog only sleeps and reads an atomic, whose counts are harmless.
    // On COFF the bounds are marker symbols, so the counters between them start at the next
    // 8-byte boundary, and anything between marker and boundary is padding.
    unsafe {
        let start = (__llvm_profile_begin_counters() as usize + 7) & !7;
        let end = __llvm_profile_end_counters() as usize & !7;
        std::slice::from_raw_parts_mut(start as *mut u64, (end - start) / 8)
    }
}

// --- Allocation check.

/// The most one allocation may ask for: a frame body is 1 MiB, and buffers around it some of that.
const MAX_ONE_ALLOC: usize = 4 << 20;

struct Counting;
static BIGGEST: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator, with a counter on the side.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        BIGGEST.fetch_max(layout.size(), Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        System.dealloc(p, layout)
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        BIGGEST.fetch_max(size, Ordering::Relaxed);
        System.realloc(p, layout, size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

// --- The target.

/// The shapes real messages use (see `snowlark-shell`'s `protocol_tests.rs`): every serde form that
/// postcard reads differently, none of them recursive.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum Sample {
    Unit,
    Number(u64),
    Signed(i32, i64),
    Float(f32, f64),
    Text(String),
    Bytes(Vec<u8>),
    Char(char),
    Maybe(Option<Box<Inner>>),
    Many(Vec<Inner>),
    Map(BTreeMap<String, u32>),
    Struct { a: u8, b: bool, c: (u16, String) },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Inner {
    name: String,
    point: (f32, f32),
    list: Vec<u32>,
}

impl Validate for Sample {
    fn validate(self, _: &ProcessPolicy) -> Result<Sample, BadMessage> {
        Ok(self)
    }
}

static PLANTED: AtomicBool = AtomicBool::new(false);

/// A bug of the kind decoders have: it takes a particular shape of data, reached a step at a time.
/// (Not a magic string: with no comparison tracing, exact bytes are luck for any mutator.)
fn planted(m: &Sample) {
    if !PLANTED.load(Ordering::Relaxed) {
        return;
    }
    if let Sample::Many(items) = m {
        if items.len() >= 3 {
            if let Some(third) = items.get(2) {
                if third.list.len() >= 2 {
                    if third.name.is_empty() {
                        panic!("the planted bug");
                    }
                }
            }
        }
    }
}

/// One input. The first byte chooses what the rest is, and how it is cut:
///
/// - The raw stream a child would write, which attacks the framing itself.
/// - One payload, framed correctly as each kind of frame. A mutation that changes a length inside a
///   payload would otherwise have to fix the frame's length too, which mutators almost never do, and
///   then nothing past the framing check gets tested.
fn run_one(data: &[u8]) {
    let Some((&first, rest)) = data.split_first() else {
        return;
    };
    wire::fuzz_transport_messages(rest);
    let _ = wire::decode::<Sample>(rest);
    let Ok((browser, child)) = pair::<Sample, Sample>(0) else {
        return;
    };
    let framed = |kind, id| frame::encode(kind, id, rest).unwrap_or_default();
    let stream = match first & 7 {
        0 | 1 => rest.to_vec(),
        2 | 3 => framed(Kind::Message, 0),
        4 => framed(Kind::Call, 1),
        5 => framed(Kind::Reply, 1),
        6 => framed(Kind::Control, 1),
        _ => framed(Kind::ControlReply, 1),
    };
    let chunk = (first >> 3) as usize * 8 + 1;
    for bytes in stream.chunks(chunk) {
        if child.write_raw(bytes.to_vec()).is_err() {
            break;
        }
    }
    loop {
        match browser.recv_timeout(Duration::ZERO) {
            Ok(Received::Message(m)) => {
                if let Ok(m) = browser.check(m) {
                    planted(&m);
                }
            }
            Ok(Received::Call(m, replier)) => {
                if let Ok(m) = browser.check(m) {
                    planted(&m);
                }
                let _ = replier.reply(&Sample::Unit);
            }
            Err(Error::Timeout) | Err(Error::Closed) | Err(Error::BadMessage(_)) => break,
            Err(_) => break,
        }
    }
}

// --- The fuzzer.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // (xorshift64*)
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const MAX_INPUT: usize = 1 << 16;

fn random_text(rng: &mut Rng) -> String {
    const PIECES: [&str; 6] = ["", "a", "name", "h\u{e9}llo", "\u{1F600}", "\0"];
    (0..rng.below(4))
        .map(|_| PIECES[rng.below(PIECES.len())])
        .collect()
}

fn random_inner(rng: &mut Rng) -> Inner {
    Inner {
        name: random_text(rng),
        point: (rng.next() as f32, f32::from_bits(rng.next() as u32)),
        list: (0..rng.below(6))
            .map(|_| rng.next() as u32 >> rng.below(32))
            .collect(),
    }
}

/// A valid message of a random shape. Byte mutations alone rarely make one: a change of length has
/// to be matched by exactly that much data, or the strict decoder stops at once and nothing deeper
/// is reached.
fn random_sample(rng: &mut Rng) -> Sample {
    match rng.below(11) {
        0 => Sample::Unit,
        1 => Sample::Number(rng.next() >> rng.below(64)),
        2 => Sample::Signed(rng.next() as i32, rng.next() as i64),
        3 => Sample::Float(
            f32::from_bits(rng.next() as u32),
            f64::from_bits(rng.next()),
        ),
        4 => Sample::Text(random_text(rng)),
        5 => Sample::Bytes((0..rng.below(40)).map(|_| rng.next() as u8).collect()),
        6 => Sample::Char(char::from_u32(rng.next() as u32 % 0x11_0000).unwrap_or('x')),
        7 => Sample::Maybe((rng.below(2) == 0).then(|| Box::new(random_inner(rng)))),
        8 => Sample::Many((0..rng.below(6)).map(|_| random_inner(rng)).collect()),
        9 => Sample::Map(
            (0..rng.below(5))
                .map(|_| (random_text(rng), rng.next() as u32))
                .collect(),
        ),
        _ => Sample::Struct {
            a: rng.next() as u8,
            b: rng.below(2) == 0,
            c: (rng.next() as u16, random_text(rng)),
        },
    }
}

/// A new input from valid parts: one payload for a framed mode, or a stream of well-made frames.
fn generate(rng: &mut Rng) -> Vec<u8> {
    let chunk = (rng.below(32) as u8) << 3;
    if rng.below(4) == 0 {
        let mut out = vec![chunk];
        for _ in 0..1 + rng.below(4) {
            let body = wire::encode(&random_sample(rng)).unwrap();
            let (kind, id) = match rng.below(4) {
                0 => (Kind::Call, rng.next() >> 40),
                1 => (Kind::Reply, rng.below(3) as u64),
                _ => (Kind::Message, 0),
            };
            out.extend_from_slice(&frame::encode(kind, id, &body).unwrap());
        }
        return out;
    }
    let mode = [2u8, 2, 4, 5][rng.below(4)];
    let mut out = vec![chunk | mode];
    out.extend_from_slice(&wire::encode(&random_sample(rng)).unwrap());
    out
}

fn mutate(input: &mut Vec<u8>, corpus: &[Vec<u8>], rng: &mut Rng) {
    for _ in 0..1 + rng.below(4) {
        let len = input.len();
        match rng.below(11) {
            0 if len > 0 => input[rng.below(len)] ^= 1 << rng.below(8),
            1 if len > 0 => input[rng.below(len)] = rng.next() as u8,
            2 if len > 0 => {
                const SPECIAL: [u8; 8] = [0, 1, 2, 0x7F, 0x80, 0xFE, 0xFF, 9];
                input[rng.below(len)] = SPECIAL[rng.below(8)];
            }
            3 if len > 1 => {
                let at = rng.below(len);
                let n = 1 + rng.below((len - at).min(32));
                input.drain(at..at + n);
            }
            4 => {
                let at = rng.below(len + 1);
                let n = 1 + rng.below(32);
                let bytes: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
                input.splice(at..at, bytes);
            }
            5 if len > 1 => {
                let from = rng.below(len);
                let n = 1 + rng.below((len - from).min(64));
                let bytes = input[from..from + n].to_vec();
                let at = rng.below(len + 1);
                input.splice(at..at, bytes);
            }
            6 => {
                let other = &corpus[rng.below(corpus.len())];
                if !other.is_empty() {
                    let from = rng.below(other.len());
                    let n = 1 + rng.below(other.len() - from);
                    let at = rng.below(len + 1);
                    input.splice(at..at, other[from..from + n].iter().copied());
                }
            }
            // A length field set to something that keeps the stream in step, or a lie.
            7 if len >= 5 => {
                let at = rng.below(len - 4);
                let value: u32 = match rng.below(4) {
                    0 => (len - at).saturating_sub(4) as u32,
                    1 => frame::MAX_BODY as u32,
                    2 => frame::MAX_BODY as u32 + 1,
                    _ => rng.below(64) as u32,
                };
                input[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
            // A varint at its largest.
            8 if len >= 10 => {
                let at = rng.below(len - 9);
                input[at..at + 9].fill(0xFF);
                input[at + 9] = 1;
            }
            9 if len > 0 => input.truncate(rng.below(len)),
            _ if len > 0 => {
                let at = rng.below(len);
                input[at] = input[at].wrapping_add(1 + rng.below(4) as u8);
            }
            _ => input.push(rng.next() as u8),
        }
    }
    input.truncate(MAX_INPUT);
}

/// Well-formed streams to start from.
fn seeds() -> Vec<Vec<u8>> {
    let inner = || Inner {
        name: "a name".into(),
        point: (1.5, -2.0),
        list: vec![1, 300, 70_000],
    };
    let samples = [
        Sample::Unit,
        Sample::Number(u64::MAX),
        Sample::Signed(-1, i64::MIN),
        Sample::Float(1.5, f64::NAN),
        Sample::Text("héllo".into()),
        Sample::Bytes(vec![1, 2, 3]),
        Sample::Char('é'),
        Sample::Maybe(Some(Box::new(inner()))),
        Sample::Many(vec![inner(), inner()]),
        Sample::Map(BTreeMap::from([("k".to_string(), 5)])),
        Sample::Struct {
            a: 1,
            b: true,
            c: (7, "x".into()),
        },
    ];
    let mut out = vec![vec![0]];
    let mut all = vec![0xF8];
    for s in &samples {
        let body = wire::encode(s).unwrap();
        // As a payload, for each framed mode.
        for mode in [2u8, 4, 5] {
            let mut one = vec![0xF8 | mode];
            one.extend_from_slice(&body);
            out.push(one);
        }
        // As a raw stream.
        let bytes = frame::encode(Kind::Message, 0, &body).unwrap();
        let mut one = vec![0xF8];
        one.extend_from_slice(&bytes);
        out.push(one);
        all.extend_from_slice(&bytes);
    }
    out.push(all);
    for payload in [vec![0, 16], vec![1, 1], vec![0, 255, 255, 255, 255, 15]] {
        let mut one = vec![0xF8 | 6];
        one.extend_from_slice(&payload);
        out.push(one);
    }
    out
}

/// The buckets a hit count falls in: a loop's depth is worth a little, not one edge per iteration.
fn bucket(n: u64) -> u8 {
    match n {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 4,
        4..=7 => 8,
        8..=15 => 16,
        16..=31 => 32,
        32..=127 => 64,
        _ => 128,
    }
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let secs: u64 = arg("--secs").and_then(|v| v.parse().ok()).unwrap_or(60);
    let seed: u64 = arg("--seed")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x9E3779B97F4A7C15);
    let dir = arg("--out").unwrap_or_else(|| "findings".into());
    PLANTED.store(std::env::args().any(|a| a == "--plant"), Ordering::Relaxed);
    let edges = counters().len();
    println!("{edges} counters, seed {seed}");
    assert!(edges > 100, "no coverage: built without the flags?");
    // (quiet by default: a panic is reported as a finding, with its input. `--verbose` shows it.)
    if !std::env::args().any(|a| a == "--verbose") {
        std::panic::set_hook(Box::new(|_| {}));
    }
    let _ = std::fs::create_dir_all(&dir);

    if let Some(path) = arg("--replay") {
        let input = std::fs::read(&path).expect("input");
        let t = Instant::now();
        let r = catch_unwind(AssertUnwindSafe(|| run_one(&input)));
        println!(
            "{path}: {} bytes, {:.3} ms, panicked {}, biggest allocation {}",
            input.len(),
            t.elapsed().as_secs_f64() * 1000.0,
            r.is_err(),
            BIGGEST.load(Ordering::Relaxed)
        );
        return;
    }

    // A hung exec: nothing here may block, so one that runs long is a finding, with its input.
    static STARTED: AtomicU64 = AtomicU64::new(0);
    static CURRENT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    let begin = Instant::now();
    let hang_path = format!("{dir}/hang.bin");
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let at = STARTED.load(Ordering::Relaxed);
        let now = begin.elapsed().as_millis() as u64;
        if at != 0 && now.saturating_sub(at) > 30_000 {
            let _ = std::fs::write(&hang_path, &*CURRENT.lock().unwrap());
            println!("FINDING: an exec has run for over 30 s: {hang_path}");
            std::process::exit(3);
        }
    });

    let mut rng = Rng(seed);
    let mut seen = vec![0u8; edges];
    let mut corpus: Vec<Vec<u8>> = Vec::new();
    let mut covered = 0usize;
    let mut findings = 0u64;
    let (mut execs, mut last_print) = (0u64, Instant::now());
    let mut pending: Vec<Vec<u8>> = seeds();
    let start = Instant::now();
    let limit = Duration::from_secs(secs);
    while start.elapsed() < limit {
        let input = pending.pop().unwrap_or_else(|| match rng.below(4) {
            // Valid as it is, and valid with a few bytes wrong.
            0 => generate(&mut rng),
            1 => {
                let mut input = generate(&mut rng);
                mutate(&mut input, &corpus, &mut rng);
                input
            }
            _ => {
                let mut input = corpus[rng.below(corpus.len())].clone();
                mutate(&mut input, &corpus, &mut rng);
                input
            }
        });
        counters().fill(0);
        BIGGEST.store(0, Ordering::Relaxed);
        CURRENT.lock().unwrap().clone_from(&input);
        STARTED.store(begin.elapsed().as_millis().max(1) as u64, Ordering::Relaxed);
        let result = catch_unwind(AssertUnwindSafe(|| run_one(&input)));
        STARTED.store(0, Ordering::Relaxed);
        execs += 1;
        let biggest = BIGGEST.load(Ordering::Relaxed);
        if result.is_err() || biggest > MAX_ONE_ALLOC {
            findings += 1;
            let why = if result.is_err() {
                "panic"
            } else {
                "allocation"
            };
            let path = format!("{dir}/{why}-{findings}.bin");
            let _ = std::fs::write(&path, &input);
            println!(
                "FINDING {findings}: {why} after {execs} execs, {:.1} s, biggest allocation {biggest}: {path}",
                start.elapsed().as_secs_f64()
            );
            if PLANTED.load(Ordering::Relaxed) {
                println!("the plant was found");
                return;
            }
        }
        let mut new = false;
        for (hit, best) in counters().iter().zip(seen.iter_mut()) {
            let b = bucket(*hit);
            if b & !*best != 0 {
                if *best == 0 {
                    covered += 1;
                }
                *best |= b;
                new = true;
            }
        }
        if new && corpus.len() < 200_000 {
            corpus.push(input);
        }
        if last_print.elapsed() > Duration::from_secs(60) {
            last_print = Instant::now();
            println!(
                "{:.0} s: {execs} execs, {:.0} per s, {} inputs, {covered} of {edges} edges, {findings} findings",
                start.elapsed().as_secs_f64(),
                execs as f64 / start.elapsed().as_secs_f64(),
                corpus.len()
            );
        }
    }
    println!(
        "done: {:.0} s, {execs} execs, {} inputs, {covered} of {edges} edges, {findings} findings",
        start.elapsed().as_secs_f64(),
        corpus.len()
    );
    std::process::exit(i32::from(findings > 0));
}
