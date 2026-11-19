// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The spare renderer (`docs/process-isolation.md` §11, "Spare process"): with a renderer process per
//! tab, the browser keeps one started, connected and warmed up before any tab needs it: its
//! executable and libraries loaded, V8 initialized, the text engine made, and a page of its own made
//! and dropped (what a process's first page pays once), its tab's thread waiting
//! (`crate::renderer_main`). A new tab, or a crashed tab's reload, takes it and has only its page
//! left to make; a replacement then starts at idle priority, so it warms up on time nothing else
//! wants. The tab's page is not made before the tab: it asks the browser for the user agent as it
//! is made, which only the tab's host answers.
//!
//! A spare that dies before a tab takes it is never handed out: a watcher notices and starts another,
//! and [`SparePool::take`] checks again. Spares that keep failing (three in a row) stop the pool:
//! tabs then start their own renderers, as without one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use snowlark_ipc::{Error, Launch, Process};

use crate::renderer_process::{Spare, CONNECT_WAIT};

/// The process numbers spares get (`ProcessPolicy::process`): above any tab id, which numbers the
/// browser's other renderers.
pub const SPARE_NUMBERS: u64 = 1 << 40;
/// Spares in a row that may fail or die before use before the pool stops starting them.
const MAX_FAILURES: u32 = 3;
/// How often a ready spare's watcher looks whether it was taken (it waits on the process meanwhile).
const WATCH_SLICE: Duration = Duration::from_millis(500);

/// One spare renderer, kept ready. Dropping the pool ends a spare no tab took.
pub struct SparePool {
    inner: Arc<Inner>,
}

struct Inner {
    launch: Box<dyn Fn() -> Launch + Send + Sync>,
    slot: Mutex<Slot>,
    changed: Condvar,
    next: AtomicU64,
}

struct Slot {
    state: State,
    /// Spares in a row that failed to start or died before use.
    failures: u32,
    closed: bool,
}

enum State {
    Empty,
    /// Being launched; its process once started (a tab that waits for it raises its priority).
    Starting(Option<Arc<Process>>),
    Ready(Spare),
}

impl SparePool {
    /// Starts the first spare at once, in the background, at normal priority (at the browser's
    /// launch the first tab is about to want it). `launch` says how to start a renderer.
    pub fn start(launch: impl Fn() -> Launch + Send + Sync + 'static) -> SparePool {
        let inner = Arc::new(Inner {
            launch: Box::new(launch),
            slot: Mutex::new(Slot {
                state: State::Empty,
                failures: 0,
                closed: false,
            }),
            changed: Condvar::new(),
            next: AtomicU64::new(SPARE_NUMBERS),
        });
        {
            let mut slot = inner.slot.lock().unwrap();
            begin(&inner, &mut slot, false);
        }
        SparePool { inner }
    }

    /// A running spare for a new tab: the ready one, else the one being started (waited for, at
    /// normal priority from now on, no longer than [`CONNECT_WAIT`]), else `None` (the caller
    /// starts a renderer itself). Whenever one is taken or found dead, another starts at idle
    /// priority.
    pub fn take(&self) -> Option<Spare> {
        let inner = &self.inner;
        let until = Instant::now() + CONNECT_WAIT;
        let mut slot = inner.slot.lock().unwrap();
        loop {
            if slot.closed {
                return None;
            }
            match std::mem::replace(&mut slot.state, State::Empty) {
                State::Ready(spare) => {
                    if spare.alive() {
                        slot.failures = 0;
                        begin(inner, &mut slot, true);
                        return Some(spare);
                    }
                    // (died before its watcher saw it: never handed to a tab; its replacement is
                    // waited for, as one being started is)
                    died(inner, &mut slot);
                }
                State::Starting(process) => {
                    if let Some(p) = &process {
                        p.set_background(false);
                    }
                    slot.state = State::Starting(process);
                    let left = until.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    slot = inner.changed.wait_timeout(slot, left).unwrap().0;
                }
                State::Empty => {
                    if slot.failures < MAX_FAILURES {
                        begin(inner, &mut slot, true);
                    }
                    return None;
                }
            }
        }
    }

    /// The ready spare's process, once there is one (within `timeout`): for tests and timing.
    pub fn ready(&self, timeout: Duration) -> Option<Arc<Process>> {
        let until = Instant::now() + timeout;
        let mut slot = self.inner.slot.lock().unwrap();
        loop {
            match &slot.state {
                State::Ready(spare) => return Some(spare.process().clone()),
                State::Empty if slot.failures >= MAX_FAILURES => return None,
                _ => {}
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            slot = self.inner.changed.wait_timeout(slot, left).unwrap().0;
        }
    }
}

impl Drop for SparePool {
    fn drop(&mut self) {
        let mut slot = self.inner.slot.lock().unwrap();
        slot.closed = true;
        match std::mem::replace(&mut slot.state, State::Empty) {
            State::Ready(spare) => spare.process().kill(),
            State::Starting(Some(p)) => p.kill(),
            _ => {}
        }
        self.inner.changed.notify_all();
    }
}

/// A spare died or failed before use: counted, and another started unless too many did.
fn died(inner: &Arc<Inner>, slot: &mut MutexGuard<Slot>) {
    slot.failures += 1;
    slot.state = State::Empty;
    if slot.failures < MAX_FAILURES {
        begin(inner, slot, true);
    } else {
        eprintln!("[spare] {MAX_FAILURES} spare renderers in a row did not last: no more spares");
    }
    inner.changed.notify_all();
}

/// Starts a spare on a thread of its own (`background`: at idle priority). The slot is `Empty`.
fn begin(inner: &Arc<Inner>, slot: &mut MutexGuard<Slot>, background: bool) {
    slot.state = State::Starting(None);
    let inner2 = inner.clone();
    let spawned = std::thread::Builder::new()
        .name("snowlark spare renderer".into())
        .spawn(move || launch_and_watch(&inner2, background));
    if let Err(e) = spawned {
        eprintln!("[spare] no thread to start a spare: {e}");
        slot.state = State::Empty;
        slot.failures = MAX_FAILURES;
    }
}

fn launch_and_watch(inner: &Arc<Inner>, background: bool) {
    let number = inner.next.fetch_add(1, Ordering::Relaxed);
    let started = (|| -> Result<Spare, Error> {
        let spawned = (inner.launch)().spawn()?;
        let process = spawned.process().clone();
        if background {
            process.set_background(true);
        }
        {
            let mut slot = inner.slot.lock().unwrap();
            if slot.closed {
                process.kill();
                return Err(Error::Closed);
            }
            if let State::Starting(p) = &mut slot.state {
                *p = Some(process);
            }
        }
        let spare = Spare::connect(spawned, number)?;
        spare.warm_up()?;
        Ok(spare)
    })();
    let process = {
        let mut slot = inner.slot.lock().unwrap();
        match started {
            Ok(spare) if slot.closed => {
                spare.process().kill();
                return;
            }
            Ok(spare) => {
                let process = spare.process().clone();
                slot.state = State::Ready(spare);
                inner.changed.notify_all();
                process
            }
            Err(_) if slot.closed => return,
            Err(e) => {
                eprintln!("[spare] a spare renderer did not start: {e}");
                died(inner, &mut slot);
                return;
            }
        }
    };
    // Watches it until a tab takes it: one that ends first is replaced.
    loop {
        let ended = process.wait(WATCH_SLICE).is_some();
        let mut slot = inner.slot.lock().unwrap();
        let ours = matches!(&slot.state, State::Ready(s) if Arc::ptr_eq(s.process(), &process));
        if !ours {
            return;
        }
        if ended {
            eprintln!("[spare] a spare renderer ended before use; starting another");
            died(inner, &mut slot);
            return;
        }
    }
}
