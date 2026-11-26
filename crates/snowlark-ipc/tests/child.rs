// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A real second process. The child is this same test binary, run again with `child::child_main`
//! as its only test and a role in the environment, so the build id check is the real one.

use std::fs::File;
use std::io::{Read, Seek, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd as OwnedHandle};
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use snowlark_ipc::frame::{self, Kind};
#[cfg(windows)]
use snowlark_ipc::JobLimits;
use snowlark_ipc::{
    connect_as, connect_link, connect_to_browser, BadMessage, BuildId, Bulk, ClientEnd, Connection,
    Error, FromBrowser, FromChild, HostEnd, Launch, LinkEnd, Process, ProcessKind, ProcessPolicy,
    Received, RemoteHandle, Validate,
};
#[cfg(windows)]
use windows::core::w;
#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, CompareObjectHandles, HANDLE, WAIT_OBJECT_0};
#[cfg(windows)]
use windows::Win32::Security::SECURITY_ATTRIBUTES;
#[cfg(windows)]
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, OpenProcess, WaitForSingleObject, EVENT_ALL_ACCESS,
    PROCESS_SYNCHRONIZE,
};

const ROLE: &str = "SNOWLARK_IPC_TEST_ROLE";
const WAIT: Duration = Duration::from_secs(20);

/// A variable every child is given by default.
#[cfg(windows)]
const BASE_VAR: &str = "SystemRoot";
#[cfg(unix)]
const BASE_VAR: &str = "HOME";

/// Takes a handle the browser gave.
fn take(c: &Child, handle: RemoteHandle) -> OwnedHandle {
    #[cfg(windows)]
    return c.take_handle(handle);
    #[cfg(unix)]
    return c.take_handle(handle).unwrap();
}

/// Gives the browser a handle: its value in this process's table too.
fn give(c: &Child, file: File) -> (RemoteHandle, u64) {
    #[cfg(windows)]
    {
        let value = std::os::windows::io::AsRawHandle::as_raw_handle(&file) as usize as u64;
        (c.give_handle(OwnedHandle::from(file)), value)
    }
    #[cfg(unix)]
    {
        let value = file.as_raw_fd() as u64;
        (c.give_handle(OwnedHandle::from(file)).unwrap(), value)
    }
}

/// Whether a handle value is open in this process.
fn is_open(value: u64) -> bool {
    // SAFETY: asks about a handle value, which may name nothing.
    unsafe {
        #[cfg(windows)]
        return windows::Win32::Foundation::GetHandleInformation(
            HANDLE(value as usize as *mut _),
            &mut 0,
        )
        .is_ok();
        #[cfg(unix)]
        return libc::fcntl(value as i32, libc::F_GETFD) != -1;
    }
}

/// The descriptors or handles this process holds.
fn handle_count() -> u32 {
    snowlark_ipc::own_usage().unwrap().handles
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum Down {
    Ping(u64),
    Echo(Vec<u8>),
    Alloc(u64),
    File(RemoteHandle),
    Env(String),
    HasHandle(u64),
    StartProcess,
    /// Opens the bulk and sends its bytes back as bulk of its own.
    Bulk(Bulk),
    /// Answers with a bulk that breaks rule `n` of `Connection::open_bulk`.
    Forge(u8),
    /// Joins a link as its host and takes this many messages of bytes on it: answers with the
    /// checksum of all of them, or `Refused` when the link ends first.
    LinkHost(LinkEnd, u32),
    /// Joins a link as its client and sends this many messages of bytes on it.
    LinkClient(LinkEnd, u32),
    /// Writes a file at this path and gives the browser its handle.
    GiveFile(String),
    /// Whether this handle value is open in the child.
    IsOpen(u64),
    /// Takes this many bytes of memory and writes them.
    Eat(u64),
    Stop,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum Up {
    Pong(u64),
    Data(Vec<u8>),
    Section {
        id: u64,
        size: u64,
    },
    Refused,
    Text(Option<String>),
    Yes(bool),
    Count(u64),
    Bulk(Bulk),
    Bulks(Bulk, Bulk),
    /// A handle given to the browser, and its value in the child's table.
    Given(RemoteHandle, u64),
}

impl Validate for Up {
    fn validate(self, policy: &ProcessPolicy) -> Result<Up, BadMessage> {
        if let Up::Text(Some(t)) = &self {
            if t == "forbidden" {
                return Err(BadMessage(format!("{:?} may not say that", policy.kind)));
            }
        }
        Ok(self)
    }
}

type Browser = Connection<Down, Up, FromChild>;
type Child = Connection<Up, Down, FromBrowser>;

fn policy() -> ProcessPolicy {
    ProcessPolicy {
        process: 1,
        kind: ProcessKind::Renderer,
        lock: Default::default(),
    }
}

fn launch(role: &str) -> Launch {
    Launch::new(std::env::current_exe().unwrap())
        .arg("child::child_main")
        .arg("--exact")
        .arg("--nocapture")
        .env(ROLE, role)
}

fn start(role: &str) -> (Browser, Arc<Process>) {
    launch(role)
        .spawn()
        .unwrap()
        .connect(policy(), 64 << 20, WAIT)
        .unwrap()
}

/// What each role does. Not a test of its own: without the variable it does nothing.
#[test]
fn child_main() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let code = run_role(&role);
    std::process::exit(code);
}

fn run_role(role: &str) -> i32 {
    match role {
        "serve" => serve(connect_to_browser(WAIT).unwrap()),
        "exit-at-once" => 7,
        // (a crash: an access violation, with the crash handler installed or not)
        "crash-av" | "crash-av-no-handler" => {
            if role == "crash-av" && !snowlark_ipc::install_crash_handler() {
                return 3;
            }
            // SAFETY: this is the point: an invalid write, which the process cannot survive.
            unsafe {
                std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1)
            };
            4
        }
        "crash-stack" => {
            if !snowlark_ipc::install_crash_handler() {
                return 3;
            }
            #[inline(never)]
            #[allow(unconditional_recursion)]
            fn recurse(depth: u64) -> u64 {
                let pad = [depth as u8; 256];
                let below = recurse(std::hint::black_box(depth) + 1);
                below + u64::from(std::hint::black_box(pad)[3])
            }
            recurse(0) as i32
        }
        "crash-two-threads" => {
            if !snowlark_ipc::install_crash_handler() {
                return 3;
            }
            let other = std::thread::spawn(|| {
                // SAFETY: as above, on a second thread at about the same time.
                unsafe { std::ptr::write_volatile(std::hint::black_box(8 as *mut u8), 1) };
            });
            // SAFETY: as above.
            unsafe { std::ptr::write_volatile(std::hint::black_box(16 as *mut u8), 1) };
            let _ = other.join();
            4
        }
        // (connects, starts a grandchild, says its id, and exits without killing it)
        "middle" => {
            let c: Child = connect_to_browser(WAIT).unwrap();
            let (_g, gp) = start("sleep");
            c.send(&Up::Count(u64::from(gp.pid()))).unwrap();
            c.flush(WAIT).unwrap();
            std::process::exit(0);
        }
        "wrong-build" => {
            let r = connect_as::<Up, Down>(BuildId([9; 16]), WAIT);
            i32::from(!matches!(r, Err(Error::BuildMismatch)))
        }
        "sleep" => {
            let _c: Child = connect_to_browser(WAIT).unwrap();
            std::thread::sleep(Duration::from_secs(600));
            0
        }
        "flood" => {
            let c: Child = connect_to_browser(WAIT).unwrap();
            for n in 0..100_000u64 {
                while let Err(Error::Backlog) = c.send(&Up::Count(n)) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            c.flush(Duration::from_secs(60)).unwrap();
            let _ = c.recv();
            0
        }
        raw if raw.starts_with("raw-") => {
            let c: Child = connect_to_browser(WAIT).unwrap();
            let bytes = match raw {
                "raw-garbage" => vec![0xFF; 64],
                "raw-length" => (u32::MAX).to_le_bytes().to_vec(),
                "raw-kind" => {
                    let mut f = frame::encode(Kind::Message, 0, b"x").unwrap();
                    f[4] = 99;
                    f
                }
                "raw-reply" => frame::encode(Kind::Reply, 5000, b"x").unwrap(),
                "raw-hello" => frame::encode(Kind::Hello, 0, b"x").unwrap(),
                "raw-control" => frame::encode(Kind::Control, 1, &[250, 250]).unwrap(),
                "raw-free" => frame::encode(Kind::Control, 1, &[1, 77]).unwrap(),
                "raw-control-reply" => frame::encode(Kind::ControlReply, 1, b"x").unwrap(),
                "raw-undecodable" => frame::encode(Kind::Message, 0, &[200, 200]).unwrap(),
                "raw-handle" => frame::encode(Kind::Handle, 1, &[]).unwrap(),
                other => panic!("{other}"),
            };
            c.write_raw(bytes).unwrap();
            c.flush(WAIT).unwrap();
            std::thread::sleep(Duration::from_secs(600));
            0
        }
        other => panic!("role {other}"),
    }
}

fn serve(c: Child) -> i32 {
    let mut sections = Vec::new();
    loop {
        let (message, replier) = match c.recv() {
            Ok(Received::Message(m)) => (m, None),
            Ok(Received::Call(m, r)) => (m, Some(r)),
            Err(_) => return 3,
        };
        let answer = match message {
            Down::Ping(n) => Up::Pong(n),
            Down::Echo(bytes) => Up::Data(bytes),
            Down::Alloc(size) => match c.alloc_shared(size, WAIT) {
                Ok(s) => {
                    assert!(s.write(0, b"from the child"));
                    let answer = Up::Section {
                        id: s.id(),
                        size: s.size() as u64,
                    };
                    sections.push(s);
                    answer
                }
                Err(_) => Up::Refused,
            },
            Down::File(handle) => {
                let mut file = File::from(take(&c, handle));
                file.write_all(b"written by the child").unwrap();
                Up::Yes(true)
            }
            Down::Env(name) => Up::Text(std::env::var(name).ok()),
            Down::HasHandle(value) => Up::Yes(has_handle(value)),
            Down::StartProcess => Up::Yes(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("--list")
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .is_ok(),
            ),
            Down::Bulk(bulk) => {
                let bytes = c.open_bulk(bulk).unwrap();
                Up::Bulk(c.bulk(bytes, WAIT).unwrap())
            }
            Down::Forge(n) => forge(&c, n),
            Down::LinkHost(end, count) => link_host(end, count),
            Down::LinkClient(end, count) => link_client(end, count),
            Down::GiveFile(path) => {
                let mut file = File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)
                    .unwrap();
                file.write_all(b"given by the child").unwrap();
                let (handle, value) = give(&c, file);
                Up::Given(handle, value)
            }
            Down::IsOpen(value) => Up::Yes(is_open(value)),
            Down::Eat(bytes) => {
                Up::Yes(std::hint::black_box(vec![1u8; bytes as usize]).len() as u64 == bytes)
            }
            Down::Stop => {
                c.flush(WAIT).unwrap();
                // (a section's drop asks the browser, which has nothing left to say)
                std::mem::forget(sections);
                return 0;
            }
        };
        if let Some(r) = replier {
            r.reply(&answer).unwrap();
        }
    }
}

/// What crosses the test links: a number and its bytes.
#[derive(Serialize, Deserialize, Debug)]
struct LinkMsg(u32, Bulk);

impl Validate for LinkMsg {
    fn validate(self, _: &ProcessPolicy) -> Result<LinkMsg, BadMessage> {
        Ok(self)
    }
}

/// The sizes the client sends, round and round: inline, in the ring, and past the ring's size.
const LINK_SIZES: [usize; 3] = [10, 100_000, 3 << 20];
/// The ring each way.
const LINK_RING: u64 = 1 << 20;

fn link_bytes(seq: u32) -> Vec<u8> {
    let len = LINK_SIZES[seq as usize % LINK_SIZES.len()];
    (0..len)
        .map(|i| (i as u32 ^ seq.wrapping_mul(31)) as u8)
        .collect()
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(bytes.len() as u64, |sum, &b| {
        sum.wrapping_mul(31).wrapping_add(b as u64)
    })
}

fn link_host(end: LinkEnd, count: u32) -> Up {
    let host: HostEnd<LinkMsg, LinkMsg> = connect_link(end, WAIT).unwrap();
    let mut sum = 0u64;
    for _ in 0..count {
        let Ok(Received::Message(m)) = host.recv_timeout(WAIT) else {
            return Up::Refused;
        };
        let Ok(LinkMsg(_, bulk)) = host.check(m, &policy()) else {
            return Up::Refused;
        };
        let Ok(bytes) = host.open_bulk(bulk, WAIT) else {
            return Up::Refused;
        };
        sum = sum.wrapping_add(checksum(&bytes));
    }
    Up::Count(sum)
}

fn link_client(end: LinkEnd, count: u32) -> Up {
    let client: ClientEnd<LinkMsg, LinkMsg> = connect_link(end, WAIT).unwrap();
    for seq in 0..count {
        if client
            .send_bulk(&link_bytes(seq), |bulk| LinkMsg(seq, bulk), WAIT)
            .is_err()
        {
            return Up::Yes(false);
        }
    }
    client.flush(WAIT).unwrap();
    Up::Yes(true)
}

fn forge(c: &Child, n: u8) -> Up {
    let handed = |size: u64| c.alloc_shared(size, WAIT).unwrap().hand_over();
    match n {
        0 => Up::Bulk(Bulk::Inline(vec![1; snowlark_ipc::INLINE_MAX + 1])),
        1 => Up::Bulk(Bulk::Shared { id: 999, len: 1 }),
        2 => Up::Bulk(Bulk::Shared {
            id: handed(4096),
            len: 4097,
        }),
        3 => Up::Bulk(Bulk::Given {
            handle: 4,
            size: 1,
            len: 1,
        }),
        4 => {
            let id = handed(4096);
            Up::Bulks(Bulk::Shared { id, len: 1 }, Bulk::Shared { id, len: 1 })
        }
        // A section the child still holds as its own may be handed over by id: it is the child's
        // to give, and the browser takes it out of the quota as it reads.
        _ => {
            let s = c.alloc_shared(4096, WAIT).unwrap();
            assert!(s.write(0, b"kept"));
            let id = s.id();
            std::mem::forget(s);
            Up::Bulk(Bulk::Shared { id, len: 4 })
        }
    }
}

/// The file whose descriptor `a_child_inherits_only_its_list` leaves open across exec.
#[cfg(unix)]
fn inherit_path(browser: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("snowlark-ipc-test-inherit-{browser}"))
}

/// Whether `value` here is the test's file, open: a value alone could be anything.
#[cfg(unix)]
fn has_handle(value: u64) -> bool {
    // SAFETY: reads the parent's process id.
    let browser = unsafe { libc::getppid() } as u32;
    let open = std::fs::read_link(format!("/proc/self/fd/{value}"));
    open.is_ok_and(|p| p == inherit_path(browser))
}

/// Whether `value` here is the test's named event: a value alone could be anything.
#[cfg(windows)]
fn has_handle(value: u64) -> bool {
    // SAFETY: opens a named event, and compares two handle values, one possibly invalid.
    unsafe {
        let name = w!("Local\\snowlark-ipc-test-inherit");
        let Ok(event) = OpenEventW(EVENT_ALL_ACCESS, false, name) else {
            return false;
        };
        let same = CompareObjectHandles(HANDLE(value as usize as *mut _), event).as_bool();
        let _ = CloseHandle(event);
        same
    }
}

fn call(c: &Browser, m: &Down) -> Up {
    c.check(c.call::<Up>(m, WAIT).unwrap()).unwrap()
}

fn stop(c: &Browser, p: &Process) {
    c.send(&Down::Stop).unwrap();
    assert_eq!(p.wait(WAIT), Some(0));
}

/// Two children the browser linked talk to each other with no browser between them: bytes of every
/// size cross, the large ones in the link's ring.
#[test]
fn two_linked_children_talk_directly() {
    let (host, hp) = start("serve");
    let (client, cp) = start("serve");
    let [host_end, client_end] = host.link(&client, [0, LINK_RING]).unwrap();
    let count = 12;
    let to_host = host.clone();
    let hosting = std::thread::spawn(move || call(&to_host, &Down::LinkHost(host_end, count)));
    assert_eq!(
        call(&client, &Down::LinkClient(client_end, count)),
        Up::Yes(true)
    );
    let want = (0..count).fold(0u64, |sum, seq| {
        sum.wrapping_add(checksum(&link_bytes(seq)))
    });
    assert_eq!(hosting.join().unwrap(), Up::Count(want));
    stop(&host, &hp);
    stop(&client, &cp);
}

/// A linked child that dies ends the link at once for the other, which lives on.
#[test]
fn a_linked_child_that_dies_ends_the_link() {
    let (host, hp) = start("serve");
    let (client, cp) = start("serve");
    let [host_end, client_end] = host.link(&client, [0, LINK_RING]).unwrap();
    let to_host = host.clone();
    let hosting = std::thread::spawn(move || call(&to_host, &Down::LinkHost(host_end, u32::MAX)));
    client
        .send(&Down::LinkClient(client_end, u32::MAX))
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    cp.kill();
    let start = Instant::now();
    assert_eq!(hosting.join().unwrap(), Up::Refused);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(call(&host, &Down::Ping(1)), Up::Pong(1));
    stop(&host, &hp);
}

#[test]
fn a_child_round_trip() {
    let (c, p) = start("serve");
    assert_eq!(call(&c, &Down::Ping(41)), Up::Pong(41));
    stop(&c, &p);
}

#[test]
fn every_size_up_to_the_cap_crosses() {
    let (c, p) = start("serve");
    let cap = frame::MAX_PAYLOAD - 16;
    for size in [0usize, 1, 4095, 4096, 65_535, 65_536, 65_537, 300_000, cap] {
        let data: Vec<u8> = (0..size).map(|i| (i * 31) as u8).collect();
        assert_eq!(
            call(&c, &Down::Echo(data.clone())),
            Up::Data(data),
            "{size}"
        );
    }
    stop(&c, &p);
}

#[test]
fn over_the_cap_is_refused_by_the_sender() {
    let (c, p) = start("serve");
    let big = Down::Echo(vec![0; frame::MAX_PAYLOAD]);
    assert!(matches!(c.send(&big), Err(Error::TooBig(_))));
    stop(&c, &p);
}

#[test]
fn calls_from_many_threads_get_their_own_answers() {
    let (c, p) = start("serve");
    let threads: Vec<_> = (0..8u64)
        .map(|t| {
            let c = c.clone();
            std::thread::spawn(move || {
                for i in 0..500 {
                    let n = t * 1000 + i;
                    assert_eq!(call(&c, &Down::Ping(n)), Up::Pong(n));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    stop(&c, &p);
}

#[test]
fn a_call_fails_at_once_when_the_child_dies() {
    let (c, p) = start("sleep");
    let killer = {
        let p = p.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            p.kill();
        })
    };
    let t = Instant::now();
    let r = c.call::<Up>(&Down::Ping(1), Duration::from_secs(60));
    assert!(matches!(r, Err(Error::Closed)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    killer.join().unwrap();
    assert!(matches!(c.send(&Down::Ping(2)), Err(Error::Closed)));
    assert!(matches!(c.recv(), Err(Error::Closed)));
}

#[test]
fn a_call_times_out_and_the_child_lives() {
    let (c, p) = start("sleep");
    let r = c.call::<Up>(&Down::Ping(1), Duration::from_millis(50));
    assert!(matches!(r, Err(Error::Timeout)), "{r:?}");
    assert_eq!(p.wait(Duration::from_millis(100)), None, "still alive");
}

#[test]
fn a_child_that_exits_before_hello_fails_the_connect() {
    let r = launch("exit-at-once")
        .spawn()
        .unwrap()
        .connect::<Down, Up>(policy(), 0, WAIT);
    assert!(matches!(r, Err(Error::Closed)), "{:?}", r.err());
}

#[test]
fn another_build_does_not_connect() {
    let r = launch("wrong-build")
        .spawn()
        .unwrap()
        .connect::<Down, Up>(policy(), 0, WAIT);
    assert!(matches!(r, Err(Error::BuildMismatch)), "{:?}", r.err());
}

#[test]
fn a_child_that_breaks_the_rules_dies() {
    for role in [
        "raw-garbage",
        "raw-length",
        "raw-kind",
        "raw-reply",
        "raw-hello",
        "raw-control",
        "raw-free",
        "raw-control-reply",
        "raw-handle",
    ] {
        let (c, p) = start(role);
        assert_eq!(p.wait(WAIT), Some(9), "{role}");
        assert!(matches!(c.recv(), Err(Error::Closed)), "{role}");
    }
}

#[test]
fn a_message_that_does_not_decode_kills_its_sender() {
    let (c, p) = start("raw-undecodable");
    assert!(matches!(c.recv(), Err(Error::BadMessage(_))));
    assert_eq!(p.wait(WAIT), Some(9));
}

#[test]
fn a_failed_validation_kills_the_sender() {
    let (c, p) = launch("serve")
        .env("WORD", "forbidden")
        .spawn()
        .unwrap()
        .connect::<Down, Up>(policy(), 0, WAIT)
        .unwrap();
    let r = c.call::<Up>(&Down::Env("WORD".into()), WAIT).unwrap();
    assert!(matches!(c.check(r), Err(Error::BadMessage(_))));
    assert_eq!(p.wait(WAIT), Some(9));
}

#[test]
fn sections_are_made_counted_and_shared() {
    let (c, p) = start("serve");
    let Up::Section { id, size } = call(&c, &Down::Alloc(1 << 20)) else {
        panic!();
    };
    assert_eq!(size, 1 << 20);
    assert_eq!(c.shared_bytes(), 1 << 20);
    let section = c.section(id).unwrap();
    assert_eq!(section.read_once(0, 14).unwrap(), b"from the child");
    // Past the quota, and sizes that make no sense: a refusal, not a death.
    assert_eq!(call(&c, &Down::Alloc(64 << 20)), Up::Refused);
    assert_eq!(call(&c, &Down::Alloc(0)), Up::Refused);
    assert_eq!(call(&c, &Down::Alloc(u64::MAX)), Up::Refused);
    assert_eq!(c.shared_bytes(), 1 << 20);
    stop(&c, &p);
}

#[test]
fn bulk_crosses_both_ways_at_every_size() {
    let (c, p) = start("serve");
    let inline = snowlark_ipc::INLINE_MAX;
    for size in [0, 1, inline, inline + 1, 3 << 20, 40 << 20] {
        let bytes: Vec<u8> = (0..size).map(|i| (i * 7 + size) as u8).collect();
        let down = c.bulk(&bytes).unwrap();
        assert_eq!(matches!(down, Bulk::Inline(_)), size <= inline, "{size}");
        let Up::Bulk(up) = call(&c, &Down::Bulk(down)) else {
            panic!("{size}");
        };
        assert_eq!(matches!(up, Bulk::Inline(_)), size <= inline, "{size}");
        assert_eq!(c.open_bulk(up).unwrap(), bytes, "{size}");
        // (read, so out of the child's quota)
        assert_eq!(c.shared_bytes(), 0, "{size}");
    }
    stop(&c, &p);
}

#[test]
fn a_child_may_hand_over_a_section_by_id() {
    let (c, p) = start("serve");
    let Up::Bulk(up) = call(&c, &Down::Forge(5)) else {
        panic!();
    };
    assert_eq!(c.shared_bytes(), 4096);
    assert_eq!(c.open_bulk(up).unwrap(), b"kept");
    assert_eq!(c.shared_bytes(), 0);
    stop(&c, &p);
}

#[test]
fn a_bulk_that_breaks_a_rule_kills_the_child() {
    for n in 0..=4 {
        let (c, p) = start("serve");
        match call(&c, &Down::Forge(n)) {
            Up::Bulk(b) => assert!(matches!(c.open_bulk(b), Err(Error::BadMessage(_))), "{n}"),
            Up::Bulks(first, second) => {
                assert_eq!(c.open_bulk(first).unwrap(), [0], "{n}");
                assert!(
                    matches!(c.open_bulk(second), Err(Error::BadMessage(_))),
                    "{n}"
                );
            }
            other => panic!("{n}: {other:?}"),
        }
        assert_eq!(p.wait(WAIT), Some(9), "{n}");
    }
}

#[test]
fn handles_go_to_the_child() {
    let (c, p) = start("serve");
    let path = std::env::temp_dir().join(format!("snowlark-ipc-{}.txt", std::process::id()));
    let mut file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    let handle = OwnedHandle::from(file.try_clone().unwrap());
    let remote = c.duplicate_to_child(&handle).unwrap();
    assert_eq!(call(&c, &Down::File(remote)), Up::Yes(true));
    let mut text = String::new();
    file.rewind().unwrap();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "written by the child");
    stop(&c, &p);
    drop(file);
    let _ = std::fs::remove_file(path);
}

/// A GPU process's handle comes out of its table into the browser's (closed there); from any other
/// child the browser refuses to take one.
#[test]
fn the_browser_takes_handles_only_out_of_a_gpu_process() {
    let path = std::env::temp_dir().join(format!("snowlark-ipc-give-{}.txt", std::process::id()));
    let given = |c: &Browser| match call(c, &Down::GiveFile(path.display().to_string())) {
        Up::Given(handle, value) => (handle, value),
        other => panic!("{other:?}"),
    };
    let gpu = ProcessPolicy {
        kind: ProcessKind::Gpu,
        ..policy()
    };
    let (c, p) = launch("serve")
        .spawn()
        .unwrap()
        .connect(gpu, 64 << 20, WAIT)
        .unwrap();
    let (handle, value) = given(&c);
    let mut file = File::from(c.take_from_child(handle).unwrap());
    let mut text = String::new();
    file.rewind().unwrap();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "given by the child");
    assert_eq!(call(&c, &Down::IsOpen(value)), Up::Yes(false));
    stop(&c, &p);
    drop(file);

    let (c, p) = start("serve");
    let (handle, value) = given(&c);
    assert!(matches!(c.take_from_child(handle), Err(Error::Refused)));
    // (on Windows the handle stays in the child's table, not taken; on Linux it went with the
    // message and the child kept no copy)
    assert_eq!(call(&c, &Down::IsOpen(value)), Up::Yes(cfg!(windows)));
    stop(&c, &p);
    let _ = std::fs::remove_file(path);
}

/// A descriptor of the browser's left open across exec (no close-on-exec) is still not the child's:
/// a launch closes everything but its channel.
#[cfg(unix)]
#[test]
fn a_child_inherits_only_its_list() {
    let path = inherit_path(std::process::id());
    let file = File::create(&path).unwrap();
    // SAFETY: clears close-on-exec on our own open file.
    assert_eq!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) },
        0
    );
    let (c, p) = start("serve");
    assert!(has_handle_here(file.as_raw_fd() as u64, &path));
    assert_eq!(
        call(&c, &Down::HasHandle(file.as_raw_fd() as u64)),
        Up::Yes(false)
    );
    stop(&c, &p);
    drop(file);
    let _ = std::fs::remove_file(path);
}

/// `has_handle`'s check, in this process (that the check can see the file at all).
#[cfg(unix)]
fn has_handle_here(value: u64, path: &std::path::Path) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{value}")).is_ok_and(|p| p == path)
}

#[cfg(windows)]
#[test]
fn a_child_inherits_only_its_list() {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: true.into(),
        ..Default::default()
    };
    // SAFETY: an inheritable named event, closed below.
    let event = unsafe {
        CreateEventW(
            Some(&attributes),
            true,
            false,
            w!("Local\\snowlark-ipc-test-inherit"),
        )
        .unwrap()
    };
    let (c, p) = start("serve");
    let value = event.0 as usize as u64;
    assert_eq!(call(&c, &Down::HasHandle(value)), Up::Yes(false));
    stop(&c, &p);
    // SAFETY: the event made above.
    let _ = unsafe { CloseHandle(event) };
}

#[test]
fn a_child_sees_only_the_environment_it_is_given() {
    assert!(std::env::var_os("PATH").is_some());
    let (c, p) = start("serve");
    assert_eq!(call(&c, &Down::Env("PATH".into())), Up::Text(None));
    assert!(matches!(
        call(&c, &Down::Env(BASE_VAR.into())),
        Up::Text(Some(_))
    ));
    stop(&c, &p);
}

#[cfg(windows)]
#[test]
fn the_job_can_refuse_child_processes() {
    let (c, p) = launch("serve")
        .limits(JobLimits {
            no_children: true,
            commit: Some(1 << 30),
        })
        .spawn()
        .unwrap()
        .connect::<Down, Up>(policy(), 0, WAIT)
        .unwrap();
    assert_eq!(call(&c, &Down::StartProcess), Up::Yes(false));
    stop(&c, &p);
}

/// The commit limit holds: a child that asks for more private memory than it may have fails to get
/// it (and aborts), while one within the limit goes on.
#[cfg(unix)]
#[test]
fn the_commit_limit_holds() {
    let limits = snowlark_ipc::JobLimits {
        no_children: false,
        commit: Some(1 << 30),
    };
    let (c, p) = launch("serve")
        .limits(limits)
        .spawn()
        .unwrap()
        .connect::<Down, Up>(policy(), 64 << 20, WAIT)
        .unwrap();
    assert_eq!(call(&c, &Down::Eat(64 << 20)), Up::Yes(true));
    c.send(&Down::Eat(2 << 30)).unwrap();
    // (SIGABRT: the allocation failed)
    assert_eq!(p.wait(WAIT), Some(libc::SIGABRT as u32));
}

/// A child dies with the browser: the kernel kills it when the browser's launcher goes.
#[cfg(unix)]
#[test]
fn a_child_dies_with_its_browser() {
    let (c, p) = start("middle");
    let Ok(Received::Message(m)) = c.recv_timeout(WAIT) else {
        panic!("no grandchild");
    };
    let Up::Count(grandchild) = c.check(m).unwrap() else {
        panic!();
    };
    assert_eq!(p.wait(WAIT), Some(0));
    let t = Instant::now();
    let alive = || {
        std::fs::read_to_string(format!("/proc/{grandchild}/stat")).is_ok_and(|s| {
            !s.rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        })
    };
    while alive() && t.elapsed() < WAIT {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(), "the grandchild outlived its browser");
}

/// Dropping a child's `Process` kills it, and nothing of it is left.
#[cfg(unix)]
#[test]
fn closing_the_job_kills_the_child() {
    let (c, p) = start("sleep");
    let pid = p.pid();
    drop(c);
    drop(p);
    // (reaped by the drop: the id names no process now, unless a new one already took it)
    let command = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    assert!(!String::from_utf8_lossy(&command).contains("child::child_main"));
}

#[cfg(windows)]
#[test]
fn closing_the_job_kills_the_child() {
    let (c, p) = start("sleep");
    // SAFETY: a handle of our own to wait on, closed below.
    let watch = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, p.pid()).unwrap() };
    drop(c);
    drop(p);
    // SAFETY: the handle just opened.
    unsafe {
        assert_eq!(WaitForSingleObject(watch, 20_000), WAIT_OBJECT_0);
        let _ = CloseHandle(watch);
    }
}

/// A crash dump folder of its own for one test.
#[cfg(windows)]
fn dump_dir(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("snowlark-ipc-dumps-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Spawns `role` with crash dumps into `dir`, without saying hello (it crashes before).
#[cfg(windows)]
fn spawn_crashing(role: &str, dir: &std::path::Path) -> Arc<Process> {
    launch(role)
        .crash_dumps(dir)
        .spawn()
        .unwrap()
        .process()
        .clone()
}

/// The minidump of a crashed child (crashpad's model, `crash_dump`): the child's filter has the
/// browser write it, then the browser ends the child with the exception's code.
#[cfg(windows)]
#[test]
fn a_crashed_child_leaves_a_minidump_and_its_exception_code() {
    for (role, code) in [("crash-av", 0xC000_0005u32), ("crash-stack", 0xC000_00FD)] {
        let dir = dump_dir(role);
        let p = spawn_crashing(role, &dir);
        assert_eq!(p.wait(WAIT), Some(code), "{role}");
        let dump = p.crash_dump().unwrap_or_else(|| panic!("{role}: no dump"));
        assert!(dump.starts_with(&dir), "{role}: {dump:?}");
        let bytes = std::fs::read(&dump).unwrap();
        // (a minidump starts with its signature, and holds the threads' stacks and the exception)
        assert_eq!(&bytes[..4], b"MDMP", "{role}");
        assert!(bytes.len() > 16 << 10, "{role}: {} bytes", bytes.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Without the child's handler there is nothing to write: the child ends as before, with its code.
#[cfg(windows)]
#[test]
fn a_child_without_the_handler_leaves_no_dump() {
    let dir = dump_dir("none");
    let p = spawn_crashing("crash-av-no-handler", &dir);
    assert_eq!(p.wait(WAIT), Some(0xC000_0005));
    assert_eq!(p.crash_dump(), None);
    assert!(!dir.exists());
}

/// Two threads that crash together give one dump; the second waits in the filter until the browser
/// ends the process.
#[cfg(windows)]
#[test]
fn two_threads_crashing_give_one_dump() {
    let dir = dump_dir("two");
    let p = spawn_crashing("crash-two-threads", &dir);
    assert_eq!(p.wait(WAIT), Some(0xC000_0005));
    assert!(p.crash_dump().is_some());
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A folder keeps the newest few dumps only.
#[cfg(windows)]
#[test]
fn a_dump_folder_keeps_the_newest_dumps() {
    let dir = dump_dir("keep");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..snowlark_ipc::KEPT_DUMPS + 3 {
        std::fs::write(dir.join(format!("old-{i:02}.dmp")), b"MDMP").unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    let p = spawn_crashing("crash-av", &dir);
    assert_eq!(p.wait(WAIT), Some(0xC000_0005));
    let dump = p.crash_dump().unwrap();
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left.len(), snowlark_ipc::KEPT_DUMPS);
    assert!(left.contains(&dump.file_name().unwrap().to_string_lossy().into_owned()));
    // (the oldest went)
    assert!(!left.contains(&"old-00.dmp".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_full_queue_stops_reading_and_nothing_is_lost() {
    let (c, p) = start("flood");
    let t = Instant::now();
    // (give the child time to fill everything it can)
    let mut seen = c.queued();
    while t.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(50));
        seen = seen.max(c.queued());
    }
    // One read's worth may land after the queue fills.
    assert!(seen >= snowlark_ipc::MAX_QUEUED, "{seen}");
    assert!(seen <= snowlark_ipc::MAX_QUEUED + (64 << 10), "{seen}");
    for n in 0..100_000u64 {
        match c.recv_timeout(WAIT).unwrap() {
            Received::Message(m) => assert_eq!(c.check(m).unwrap(), Up::Count(n)),
            Received::Call(..) => panic!(),
        }
    }
    p.kill();
}

/// One child: start, one call, kill. Everything of it is gone when this returns.
fn start_and_kill() {
    let (c, p) = start("serve");
    assert_eq!(call(&c, &Down::Ping(1)), Up::Pong(1));
    p.kill();
    assert_eq!(p.wait(WAIT), Some(9));
    assert!(matches!(c.recv(), Err(Error::Closed)));
}

fn settle(limit: u32) -> u32 {
    let t = Instant::now();
    let mut n = handle_count();
    while n > limit && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
        n = handle_count();
    }
    n
}

/// M1's exit: `--test it child::thousand_starts_and_kills -- --ignored --nocapture --test-threads=1`.
/// `SNOWLARK_IPC_CYCLES` runs more: a leak grows with the count, and the runtime's own handles do not.
#[test]
#[ignore]
fn thousand_starts_and_kills() {
    let cycles: u32 = std::env::var("SNOWLARK_IPC_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    for _ in 0..20 {
        start_and_kill();
    }
    let before = settle(0);
    let t = Instant::now();
    let mut peak = 0;
    for i in 0..cycles {
        start_and_kill();
        peak = peak.max(handle_count());
        if i % 500 == 499 {
            println!("{} starts, {} handles", i + 1, handle_count());
        }
    }
    let after = settle(before);
    println!(
        "{cycles} starts and kills in {:.1} s: handles {before} before, {after} after, peak {peak}",
        t.elapsed().as_secs_f64()
    );
    // (a handle per child would be `cycles` more; a few is the process's own, made once on the way)
    assert!(
        after <= before + 4,
        "{before} handles before, {after} after"
    );
}

/// M1's exit: 100,000 pings, `--test it child::ping -- --ignored --nocapture --test-threads=1`.
#[test]
#[ignore]
fn ping() {
    let (c, p) = start("serve");
    for n in 0..5_000 {
        assert_eq!(call(&c, &Down::Ping(n)), Up::Pong(n));
    }
    let mut times = Vec::with_capacity(100_000);
    let start = Instant::now();
    for n in 0..100_000 {
        let t = Instant::now();
        let reply = c.call::<Up>(&Down::Ping(n), WAIT).unwrap();
        times.push(t.elapsed().as_nanos() as u64);
        assert_eq!(c.check(reply).unwrap(), Up::Pong(n));
    }
    let wall = start.elapsed().as_secs_f64();
    times.sort_unstable();
    let at = |p: f64| times[((times.len() - 1) as f64 * p).round() as usize] as f64 / 1000.0;
    println!(
        "100000 pings in {wall:.2} s: p50 {:.1} p90 {:.1} p99 {:.1} p99.9 {:.1} max {:.1} us",
        at(0.5),
        at(0.9),
        at(0.99),
        at(0.999),
        at(1.0)
    );
    stop(&c, &p);
}
