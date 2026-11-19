// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The spare renderer (`crate::spare`), with real child processes: a tab takes the warm one and
//! another starts behind it at idle priority, one that died is never given to a tab, and the pool
//! ends the one no tab took.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use snowlark_net::Url;

use super::tests::{device, gpu, launch, serve};
use super::*;
use crate::services::tests::{local, WAIT};
use crate::spare::SparePool;

const TAB: TabId = 11;

const PAGE: &str = "<!doctype html><title>warm</title><body>a tab in a spare renderer";

/// Gives `spare` tab [`TAB`], loads `origin` in it and waits for its title: the tab works. (The
/// browser's services it is served from go with it.)
fn load_in(spare: Spare, origin: &str, gpu: Arc<Gpu>) -> (RendererProcess, impl Sized) {
    let (renderer, l, _) = load_until(spare, origin, gpu, |t| t == "warm");
    (renderer, l)
}

/// [`load_in`], waiting for a title `done` accepts, which it returns.
fn load_until(
    spare: Spare,
    origin: &str,
    gpu: Arc<Gpu>,
    done: impl Fn(&str) -> bool,
) -> (RendererProcess, impl Sized, String) {
    let l = local();
    let (events, events_rx) = snowlark_ipc::channel();
    let (compositor, _compositor_rx) = mpsc::channel();
    let window = WindowEnds {
        gpu,
        compositor,
        gpu_raster: None,
        events: Arc::new(events),
    };
    let renderer = RendererProcess::assign(spare, TAB, device(), &l.services, window).unwrap();
    super::tests::navigate(
        &renderer,
        &l.services.net,
        Url::parse(&format!("{origin}/")).unwrap(),
        1,
    );
    let until = Instant::now() + WAIT;
    loop {
        match events_rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok((tab, TabEvent::State(s))) if done(&s.title) => {
                assert_eq!(tab, TAB);
                return (renderer, l, s.title);
            }
            Ok((_, TabEvent::Crashed(why))) => panic!("the tab crashed: {why}"),
            Ok(_) => {}
            Err(e) => panic!("the page did not load: {e}"),
        }
    }
}

#[test]
fn a_tab_takes_the_warm_spare_and_another_starts_at_idle_priority() {
    let Some(gpu) = gpu() else { return };
    let origin = serve(PAGE);
    let pool = SparePool::start(|| launch("tab", &[]));
    // The first spare (the browser's launch) runs at normal priority: a tab is about to want it.
    let first = pool.ready(WAIT).expect("no spare");
    assert!(!first.is_background());
    let spare = pool.take().expect("the spare was not taken");
    assert_eq!(spare.process().pid(), first.pid());
    let renderer = load_in(spare, &origin, gpu.clone());
    assert!(!renderer.0.process().is_background());
    // Its replacement: another process, warming up below everything else.
    let second = pool.ready(WAIT).expect("the spare was not replaced");
    assert_ne!(second.pid(), first.pid());
    assert!(
        second.is_background(),
        "the replacement is not at idle priority"
    );
    // Taken in turn, it runs at normal priority and loads a page.
    let spare = pool.take().unwrap();
    assert_eq!(spare.process().pid(), second.pid());
    let other = load_in(spare, &origin, gpu);
    assert!(!other.0.process().is_background());
    drop((renderer, other));
}

#[test]
fn a_spare_that_died_is_never_given_to_a_tab() {
    let Some(gpu) = gpu() else { return };
    let origin = serve(PAGE);
    let pool = SparePool::start(|| launch("tab", &[]));
    let dead = pool.ready(WAIT).expect("no spare");
    dead.kill();
    assert!(dead.wait(WAIT).is_some());
    // At once, before or after the pool's watcher saw it: a live spare that is not the dead one.
    let spare = pool.take().expect("no spare after the death");
    assert_ne!(spare.process().pid(), dead.pid());
    assert!(spare.alive());
    let renderer = load_in(spare, &origin, gpu);
    // A death with no tab asking: the watcher replaces it.
    let next = pool.ready(WAIT).expect("no spare");
    next.kill();
    assert!(next.wait(WAIT).is_some());
    let until = Instant::now() + WAIT;
    let replaced = loop {
        let p = pool.ready(WAIT).expect("the dead spare was not replaced");
        if p.pid() != next.pid() {
            break p;
        }
        assert!(Instant::now() < until, "the dead spare was not replaced");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(replaced.wait(Duration::ZERO).is_none());
    drop(renderer);
}

#[test]
fn a_spare_no_tab_took_ends_with_the_pool() {
    let pool = SparePool::start(|| launch("tab", &[]));
    let spare = pool.ready(WAIT).expect("no spare");
    drop(pool);
    assert!(
        spare.wait(Duration::from_secs(5)).is_some(),
        "the spare outlived its pool"
    );
}

/// A canvas made by script half a second after the page loaded, never connected (so nothing about
/// the page asked for a device ahead): the time its first `getContext('2d')` and one fill take.
const CANVAS: &str = "<!doctype html><title>wait</title><script>
setTimeout(() => {
  const a = performance.now();
  document.createElement('canvas').getContext('2d').fillRect(0, 0, 1, 1);
  document.title = 'ctx ' + (performance.now() - a);
}, 500);
</script>";

/// A renderer process makes its canvas device as it starts, as Chromium's renderer establishes its
/// GPU channel at startup (`RenderThreadImpl::Init`), so a page's first canvas does not wait for
/// the process's first Direct3D device (about 150 ms: the driver's start in a process).
#[test]
fn a_renderers_first_canvas_finds_its_device_made() {
    let Some(gpu) = gpu() else { return };
    // (the renderer's canvases draw on the graphics card: without one there is no device to make)
    if Gpu::with_driver(snowlark_paint::gpu::Driver::Hardware).is_err() {
        return;
    }
    let origin = serve(CANVAS);
    let pool = SparePool::start(|| launch("tab", &[]));
    pool.ready(WAIT).expect("no spare");
    let spare = pool.take().expect("the spare was not taken");
    let (renderer, _l, title) = load_until(spare, &origin, gpu, |t| t.starts_with("ctx "));
    let ms: f64 = title["ctx ".len()..].parse().unwrap();
    assert!(
        ms < 50.0,
        "the first getContext('2d') took {ms:.1} ms: the renderer made its device then"
    );
    drop(renderer);
}
