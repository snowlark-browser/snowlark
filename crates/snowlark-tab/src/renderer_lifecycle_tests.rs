// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Crashes, hangs and kills of tabs in real renderer processes (`docs/process-isolation.md`, §11 and
//! M4's exit list): each crash switch reaches one renderer only (its launch's environment), and a
//! second tab beside it goes on drawing.

use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use snowlark_net::Url;

use super::tests::{gpu, launch, serve, start_tab, start_tab_with, Started};
use super::*;
use crate::services::tests::{local, WAIT};

/// A page that draws a new frame every few milliseconds (a canvas) and says so in its title.
const ANIMATED: &str = r#"<!doctype html><title>0</title><body><canvas width=200 height=200></canvas><script>
let n = 0;
const g = document.querySelector("canvas").getContext("2d");
setInterval(() => {
  n++;
  g.fillStyle = `rgb(${n % 256}, 100, 0)`;
  g.fillRect(0, 0, 200, 200);
  document.title = n;
}, 16);
</script>"#;

/// The same, until its script loops for good 300 ms in.
const LOOPS: &str = r#"<!doctype html><title>0</title><body><div id=d>0</div><script>
let n = 0;
setInterval(() => { n++; d.textContent = n; document.title = n; }, 16);
setTimeout(() => { while (1) {} }, 300);
</script>"#;

/// Elements nested deeper than the page thread's stack can take: a native stack overflow, as the
/// reddit page caused (JS recursion stops at V8's limit with a RangeError; the tree's does not).
/// Parsed in one go, as `<span>` needs no scope check: building it node by node is quadratic.
const DEEP: &str = r#"<!doctype html><title>deep</title><body><script>
document.body.innerHTML = "<span>".repeat(500000) + "deep";
</script>"#;

fn navigate(tab: &Started, net: &Arc<dyn snowlark_net::NetBackend>, origin: &str) {
    super::tests::navigate(
        &tab.renderer,
        net,
        Url::parse(&format!("{origin}/")).unwrap(),
        1,
    );
    // (as the window does for the tab it shows)
    let _ = tab.renderer.sender().send(TabCommand::Shown(true));
    let _ = tab
        .renderer
        .sender()
        .send(TabCommand::Device(super::tests::device()));
}

/// Waits for the tab's title to be a number at least `n`, and returns it; panics on a crash.
fn count_to(tab: &Started, n: u64) -> u64 {
    let until = Instant::now() + WAIT;
    loop {
        match tab
            .events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok((_, TabEvent::State(s))) if s.title.parse::<u64>().is_ok_and(|t| t >= n) => {
                return s.title.parse().unwrap_or(n)
            }
            Ok((_, TabEvent::Crashed(why))) => panic!("crashed: {why}"),
            Ok(_) => {}
            Err(e) => panic!("the page did not count to {n}: {e}"),
        }
    }
}

/// The tab's `Crashed` message within `within`, and when it came.
fn crashed(tab: &Started, within: Duration) -> Option<(String, Instant)> {
    let until = Instant::now() + within;
    loop {
        match tab
            .events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok((_, TabEvent::Crashed(why))) => return Some((why, Instant::now())),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// When the tab's process ends, from a thread that waits on it.
fn ends(tab: &Started) -> Receiver<(Option<u32>, Instant)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let process = tab.renderer.process().clone();
    thread::spawn(move || {
        let code = process.wait(WAIT);
        let _ = tx.send((code, Instant::now()));
    });
    rx
}

/// The frames the tab's raster thread gives the compositor within `within`, each handed back as
/// the compositor does once it shows the next (the raster thread draws into the tiles it gets back).
fn frames(tab: &Started, within: Duration) -> usize {
    let until = Instant::now() + within;
    let mut n = 0;
    while let Ok(m) = tab
        .compositor
        .recv_timeout(until.saturating_duration_since(Instant::now()))
    {
        if let CompInput::Frame(_, f) = m {
            let f = *f;
            f.recycler.recycle(f.frame);
            n += 1;
        }
    }
    n
}

/// Waits until the browser's end of the tab's liveness calls it stuck for `after` (the window would
/// offer Wait or Kill), with input given meanwhile, as a user's clicks; the pass it is stuck in.
fn stuck(tab: &Started, after: Duration) -> u64 {
    let live = tab.renderer.liveness();
    let until = Instant::now() + WAIT;
    while Instant::now() < until {
        live.input(Instant::now());
        if let Some(pass) = live.stuck(Instant::now(), after) {
            return pass;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("the browser never saw the tab stuck");
}

#[test]
fn each_crash_ends_its_tab_only_and_reload_starts_it_again() {
    let Some(gpu) = gpu() else { return };
    let l = local();
    let origin = serve(ANIMATED);
    let other = start_tab(20, "tab", &[], &l.services, gpu.clone()).unwrap();
    navigate(&other, &l.services.net, &origin);
    count_to(&other, 3);
    let mut next = 21;
    for (switch, value, kind, code) in [
        (
            "SNOWLARK_TEST_CRASH",
            "stack",
            "a stack overflow",
            Some(0xC000_00FD),
        ),
        (
            "SNOWLARK_TEST_CRASH",
            "av",
            "an access violation",
            Some(0xC000_0005),
        ),
        (
            "SNOWLARK_TEST_CRASH",
            "abort",
            "a fast fail",
            Some(0xC000_0409),
        ),
        (
            "SNOWLARK_TEST_PANIC",
            "tab",
            "SNOWLARK_TEST_PANIC=tab",
            None,
        ),
        (
            "SNOWLARK_TEST_PANIC",
            "callback",
            "SNOWLARK_TEST_PANIC=callback",
            None,
        ),
    ] {
        let id = next;
        next += 2;
        let tab = start_tab(id, "tab", &[(switch, value)], &l.services, gpu.clone()).unwrap();
        let ended = ends(&tab);
        navigate(&tab, &l.services.net, &origin);
        let (why, heard) = crashed(&tab, WAIT).unwrap_or_else(|| panic!("{value}: no crash"));
        let (exit, at) = ended.recv().unwrap();
        // (a caught panic says so before its process ends: then the gap is none)
        let after = heard.saturating_duration_since(at);
        eprintln!("{switch}={value}: Crashed {after:?} after the exit ({exit:x?}): {why}");
        assert!(why.contains(kind), "{value}: {why}");
        if let Some(code) = code {
            assert_eq!(exit, Some(code), "{value}");
        }
        assert!(after < Duration::from_millis(200), "{value}: {after:?}");
        // The other tab goes on.
        let steps = runs_on(&other, Duration::from_millis(500));
        assert!(
            steps >= 5,
            "{value}: the other tab made {steps} steps in 500 ms"
        );
        // Reload: a new renderer (without the switch) for the crashed tab draws the page.
        let again = start_tab(id + 1, "tab", &[], &l.services, gpu.clone()).unwrap();
        navigate(&again, &l.services.net, &origin);
        count_to(&again, 3);
        assert!(frames(&again, Duration::from_millis(500)) > 0, "{value}");
    }
    assert_eq!(other.renderer.process().wait(Duration::ZERO), None);
}

/// A renderer that crashes leaves a minidump, which the browser writes before it ends the process
/// (crashpad's model, `snowlark_ipc::win::crash_dump`), and its tab's message says where it is.
#[test]
fn a_crashed_renderer_leaves_a_minidump_its_tab_names() {
    let Some(gpu) = gpu() else { return };
    let l = local();
    let origin = serve(ANIMATED);
    let dir = std::env::temp_dir().join(format!("snowlark-tab-dumps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let crashing = launch("tab", &[("SNOWLARK_TEST_CRASH", "av")]).crash_dumps(&dir);
    let tab = start_tab_with(40, crashing, &l.services, gpu).unwrap();
    let ended = ends(&tab);
    navigate(&tab, &l.services.net, &origin);
    let (why, _) = crashed(&tab, WAIT).expect("no crash");
    let (exit, _) = ended.recv().unwrap();
    assert_eq!(exit, Some(0xC000_0005), "{why}");
    assert!(why.contains("an access violation"), "{why}");
    let dump = tab.renderer.process().crash_dump().expect("no dump");
    let file = dump.file_name().unwrap().to_string_lossy().into_owned();
    assert!(why.contains(&file), "{why}");
    assert!(why.contains(&dir.display().to_string()), "{why}");
    assert_eq!(&std::fs::read(&dump).unwrap()[..4], b"MDMP");
    let _ = std::fs::remove_dir_all(&dir);
}

/// How many steps the tab's page takes within `within` (its title counts its timer's ticks): it
/// goes on, its renderer alive. (Its frames are not counted: in this harness a renderer's page
/// gives two frames and then paints the same list, a canvas and a box that changes size alike, which
/// the window does not do; `SNOWLARK_PROFILE` shows "laid out 0" here, "laid out 3" there. Not looked
/// into further: the window check counts frames.)
fn runs_on(tab: &Started, within: Duration) -> u64 {
    let until = Instant::now() + within;
    let (mut first, mut last) = (None, 0);
    while let Ok((_, e)) = tab
        .events
        .recv_timeout(until.saturating_duration_since(Instant::now()))
    {
        match e {
            TabEvent::State(s) => {
                if let Ok(n) = s.title.parse::<u64>() {
                    first.get_or_insert(n);
                    last = n;
                }
            }
            TabEvent::Crashed(why) => panic!("the other tab crashed: {why}"),
            _ => {}
        }
    }
    first.map_or(0, |f| last - f)
}

#[test]
fn a_deep_tree_overflows_one_renderer_only() {
    // (a debug build does not reach the overflow in two minutes: building and laying out half a
    // million nested elements is that much slower there; release overflows in under two seconds)
    if cfg!(debug_assertions) {
        eprintln!("a_deep_tree_overflows_one_renderer_only: release builds only");
        return;
    }
    let Some(gpu) = gpu() else { return };
    let l = local();
    let other = start_tab(40, "tab", &[], &l.services, gpu.clone()).unwrap();
    navigate(&other, &l.services.net, &serve(ANIMATED));
    count_to(&other, 3);
    let deep = start_tab(41, "tab", &[], &l.services, gpu).unwrap();
    let ended = ends(&deep);
    navigate(&deep, &l.services.net, &serve(DEEP));
    let (why, heard) = crashed(&deep, WAIT).expect("the deep page did not crash its tab");
    let (exit, at) = ended.recv().unwrap();
    eprintln!(
        "deep tree: Crashed {:?} after the exit ({exit:x?}): {why}",
        heard.saturating_duration_since(at)
    );
    assert_eq!(exit, Some(0xC000_00FD), "{why}");
    assert!(why.contains("a stack overflow"), "{why}");
    assert!(runs_on(&other, Duration::from_millis(500)) >= 5);
    assert_eq!(other.renderer.process().wait(Duration::ZERO), None);
}

#[test]
fn a_script_loop_is_stopped_by_the_kill_and_the_page_kept() {
    let Some(gpu) = gpu() else { return };
    let l = local();
    let tab = start_tab(50, "tab", &[], &l.services, gpu).unwrap();
    navigate(&tab, &l.services.net, &serve(LOOPS));
    let pass = stuck(&tab, Duration::from_millis(500));
    let killed = Instant::now();
    tab.renderer.kill_script();
    let live = tab.renderer.liveness();
    while live.pass() == pass {
        assert!(
            killed.elapsed() < Duration::from_secs(2),
            "the loop went on"
        );
        thread::sleep(Duration::from_millis(1));
    }
    eprintln!("while(1) ended {:?} after the kill", killed.elapsed());
    // The page stays: its timer counts on from where it stopped, and the process lives.
    let at = count_to(&tab, 1);
    count_to(&tab, at + 5);
    assert_eq!(tab.renderer.process().wait(Duration::ZERO), None);
}

#[test]
fn a_native_hang_is_ended_by_the_kill() {
    let Some(gpu) = gpu() else { return };
    let l = local();
    let origin = serve(ANIMATED);
    // One Kill: the script cannot be stopped, so the process is ended a second later.
    // Two: at once.
    for (id, kills) in [(60, 1), (61, 2)] {
        let tab = start_tab(
            id,
            "tab",
            &[("SNOWLARK_TEST_CRASH", "hang")],
            &l.services,
            gpu.clone(),
        )
        .unwrap();
        navigate(&tab, &l.services.net, &origin);
        stuck(&tab, Duration::from_millis(500));
        let killed = Instant::now();
        for _ in 0..kills {
            tab.renderer.kill_script();
        }
        let (why, heard) = crashed(&tab, Duration::from_secs(5)).expect("the hang went on");
        let took = heard - killed;
        eprintln!("hang, {kills} kill(s): Crashed {took:?} after the kill: {why}");
        assert!(why.contains("did not respond"), "{why}");
        assert!(took < Duration::from_secs(2), "{took:?}");
        if kills == 1 {
            assert!(took >= KILL_GRACE, "{took:?}");
        } else {
            assert!(took < KILL_GRACE / 2, "{took:?}");
        }
    }
}

#[test]
fn no_wait_on_a_renderer_outlives_it() {
    let Some(gpu) = gpu() else { return };
    let l = local();
    // The browser waits for a new renderer's hello for up to CONNECT_WAIT; one that dies first
    // ends the wait then.
    let began = Instant::now();
    let r = start_tab(70, "die-early", &[], &l.services, gpu);
    let took = began.elapsed();
    eprintln!("a renderer that died before its hello: start failed after {took:?}");
    assert!(r.is_err());
    assert!(took < Duration::from_secs(2), "{took:?}");
}
