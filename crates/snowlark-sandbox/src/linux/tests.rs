// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Each test runs again in a child process of this test binary (the filter and the namespaces are
//! for good and for the whole process), which prints what each attempt gave on `sandbox:` lines.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::Command;

use super::{in_namespaces, lock_down, Namespaces};

const CHILD: &str = "SNOWLARK_SANDBOX_TEST_CHILD";

fn in_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// What an attempt gave: `ok` or the error's number.
fn outcome<T>(r: std::io::Result<T>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => e.raw_os_error().map_or(e.to_string(), |n| n.to_string()),
    }
}

fn say(what: &str, r: String) {
    println!("sandbox: {what}={r}");
}

/// Runs `test` alone in a child with `env`, in its namespaces when `namespaces`; its `sandbox:`
/// lines as `what=outcome`.
fn run_child(test: &str, env: &[(&str, String)], namespaces: bool) -> Vec<String> {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .envs(env.iter().map(|(k, v)| (*k, v)));
    if namespaces {
        let ns = Namespaces::new();
        // SAFETY: `enter` is async-signal-safe and allocates nothing.
        unsafe {
            command.pre_exec(move || match ns.enter() {
                Ok(true) => Ok(()),
                Ok(false) => Err(std::io::Error::other("no user namespace")),
                Err(n) => Err(std::io::Error::from_raw_os_error(n)),
            });
        }
    }
    let out = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{test} in a child: {:?}\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
        .lines()
        // (the harness's `test <name> ... ` has no line end of its own)
        .filter_map(|l| l.find("sandbox: ").map(|at| &l[at + "sandbox: ".len()..]))
        .map(str::to_owned)
        .collect()
}

fn eperm() -> String {
    libc::EPERM.to_string()
}

#[test]
fn a_locked_down_process_opens_no_file_but_reads_what_it_holds() {
    if in_child() {
        let mut held = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        lock_down().unwrap();
        say("open", outcome(std::fs::File::open("/etc/hostname")));
        say(
            "create",
            outcome(std::fs::File::create(std::env::temp_dir().join("x"))),
        );
        say("list", outcome(std::fs::read_dir("/")));
        say("stat", outcome(std::fs::metadata("/")));
        let mut magic = [0u8; 4];
        say("read held", outcome(held.read_exact(&mut magic)));
        say("magic", format!("{:?}", &magic[1..]));
        say("stat held", outcome(held.metadata()));
        return;
    }
    let got = run_child(
        "linux::tests::a_locked_down_process_opens_no_file_but_reads_what_it_holds",
        &[],
        false,
    );
    assert_eq!(
        got,
        [
            format!("open={}", eperm()),
            format!("create={}", eperm()),
            format!("list={}", eperm()),
            format!("stat={}", eperm()),
            "read held=ok".into(),
            "magic=[69, 76, 70]".into(),
            "stat held=ok".into(),
        ]
    );
}

#[test]
fn a_locked_down_process_makes_no_socket_but_a_local_pair() {
    if in_child() {
        let unix = std::env::var("UNIX").unwrap();
        let tcp = std::env::var("TCP").unwrap();
        lock_down().unwrap();
        say("unix", outcome(UnixStream::connect(&unix)));
        say("tcp", outcome(TcpStream::connect(&tcp)));
        say("listen", outcome(TcpListener::bind("127.0.0.1:0")));
        let pair = UnixStream::pair().and_then(|(mut a, mut b)| {
            a.write_all(b"x")?;
            let mut c = [0u8; 1];
            b.read_exact(&mut c)?;
            Ok(c)
        });
        say("pair", outcome(pair));
        return;
    }
    let dir = std::env::temp_dir().join(format!("snowlark-sandbox-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("listener");
    let _ = std::fs::remove_file(&path);
    let _unix = UnixListener::bind(&path).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let got = run_child(
        "linux::tests::a_locked_down_process_makes_no_socket_but_a_local_pair",
        &[
            ("UNIX", path.display().to_string()),
            ("TCP", tcp.local_addr().unwrap().to_string()),
        ],
        false,
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        got,
        [
            format!("unix={}", eperm()),
            format!("tcp={}", eperm()),
            format!("listen={}", eperm()),
            "pair=ok".into(),
        ]
    );
}

#[test]
fn a_locked_down_process_starts_no_process_and_signals_only_itself() {
    if in_child() {
        lock_down().unwrap();
        say("spawn", outcome(Command::new("/bin/true").status()));
        // SAFETY: signal 0 checks only whether the signal could be sent.
        let kill = |pid| unsafe {
            if libc::kill(pid, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        };
        // SAFETY: getppid and getpid cannot fail.
        let (parent, own) = unsafe { (libc::getppid(), libc::getpid()) };
        say("kill parent", outcome(kill(parent)));
        say("kill self", outcome(kill(own)));
        let thread = std::thread::spawn(|| 7)
            .join()
            .map_err(|_| std::io::Error::other("panic"));
        say("thread", outcome(thread));
        return;
    }
    let got = run_child(
        "linux::tests::a_locked_down_process_starts_no_process_and_signals_only_itself",
        &[],
        false,
    );
    assert_eq!(
        got,
        [
            format!("spawn={}", eperm()),
            format!("kill parent={}", eperm()),
            "kill self=ok".into(),
            "thread=ok".into(),
        ]
    );
}

#[test]
fn a_process_in_its_namespaces_has_no_network_and_no_capability() {
    if in_child() {
        let tcp = std::env::var("TCP").unwrap();
        say("namespaces", in_namespaces().to_string());
        say("tcp", outcome(TcpStream::connect(&tcp)));
        say("read", outcome(std::fs::read("/proc/self/status")));
        let caps = std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("CapEff:").map(|v| v.trim().to_owned()))
            .unwrap();
        // SAFETY: getuid cannot fail.
        // (root is root in its namespace too, with that namespace's capabilities: they reach
        // nothing but its own network namespace)
        let root = unsafe { libc::getuid() } == 0;
        say("capabilities", if root { "root".into() } else { caps });
        return;
    }
    if !Namespaces::available() {
        eprintln!("no unprivileged user namespaces here: skipped");
        return;
    }
    // SAFETY: getuid cannot fail.
    let root = unsafe { libc::getuid() } == 0;
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let got = run_child(
        "linux::tests::a_process_in_its_namespaces_has_no_network_and_no_capability",
        &[("TCP", tcp.local_addr().unwrap().to_string())],
        true,
    );
    assert_eq!(
        got,
        [
            "namespaces=true".into(),
            format!("tcp={}", libc::ENETUNREACH),
            "read=ok".to_string(),
            if root {
                "capabilities=root".to_string()
            } else {
                "capabilities=0000000000000000".into()
            },
        ]
    );
}
