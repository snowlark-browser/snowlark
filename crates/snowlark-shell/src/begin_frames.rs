// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's begin frames: a thread that waits for each vsync of the display and tells the
//! compositor (Chromium's `gl::VSyncThreadWin`, `ui/gl/vsync_thread_win.cc`), and what the
//! compositor makes of each for the tabs that ask (`viz::ExternalBeginFrameSourceWin`,
//! `components/viz/service/frame_sinks/external_begin_frame_source_win.cc`). The vsync thread waits
//! only while some tab asks (`VSyncThreadWin::PostTaskIfNeeded`: "make sure that there are observers
//! waiting for VSync").

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use snowlark_paint::vsync::{VBlank, DEFAULT_INTERVAL};
use snowlark_paint::Ticks;
use snowlark_tab::comp::BeginFrameArgs;

#[derive(Default)]
struct Flags {
    observing: bool,
    quit: bool,
}

/// The vsync thread (`VSyncThreadWin`): while observed, it waits for each vsync and calls its
/// `notify` with the time and the refresh interval (`VSyncObserver::OnVSync`).
pub struct VSyncThread {
    flags: Arc<(Mutex<Flags>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl VSyncThread {
    pub fn start(notify: impl Fn(Ticks, Duration) + Send + 'static) -> VSyncThread {
        let flags: Arc<(Mutex<Flags>, Condvar)> = Arc::default();
        let shared = flags.clone();
        let thread = std::thread::Builder::new()
            .name("snowlark vsync".into())
            .spawn(move || {
                // (`base::ThreadType::kPresentation`, as the compositor's thread)
                if std::env::var("SNOWLARK_COMPOSITOR_PRIORITY").map_or(true, |v| v != "normal") {
                    snowlark_paint::window::raise_thread_priority();
                }
                let mut vblank = VBlank::new();
                let (lock, cv) = &*shared;
                // (`SNOWLARK_VSYNC_STATS=1`: a line a second while observed)
                let stats = std::env::var("SNOWLARK_VSYNC_STATS").is_ok_and(|v| v == "1");
                let (mut since, mut ticks, mut slept) = (std::time::Instant::now(), 0u32, 0u32);
                loop {
                    {
                        let mut f = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while !f.observing && !f.quit {
                            f = cv.wait(f).unwrap_or_else(|e| e.into_inner());
                        }
                        if f.quit {
                            return;
                        }
                    }
                    let (interval, sleep) = vblank.wait();
                    let time = Ticks::now();
                    if stats {
                        ticks += 1;
                        slept += u32::from(sleep);
                        if since.elapsed() >= Duration::from_secs(1) {
                            eprintln!(
                                "[vsync] {ticks} in {:.0} ms ({slept} slept), interval {interval:?}",
                                since.elapsed().as_secs_f64() * 1000.0
                            );
                            (since, ticks, slept) = (std::time::Instant::now(), 0, 0);
                        }
                    }
                    let f = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if f.quit {
                        return;
                    }
                    if f.observing {
                        drop(f);
                        notify(time, interval);
                    }
                }
            })
            .expect("vsync thread");
        VSyncThread {
            flags,
            thread: Some(thread),
        }
    }

    /// Starts or stops the waits (`AddObserver`, `RemoveObserver`).
    pub fn set_observing(&self, observing: bool) {
        let (lock, cv) = &*self.flags;
        lock.lock().unwrap_or_else(|e| e.into_inner()).observing = observing;
        cv.notify_all();
    }
}

impl Drop for VSyncThread {
    fn drop(&mut self) {
        let (lock, cv) = &*self.flags;
        lock.lock().unwrap_or_else(|e| e.into_inner()).quit = true;
        cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The begin frames the compositor sends (`ExternalBeginFrameSourceWin`): one per vsync, and a
/// missed one for a tab that begins to ask.
#[derive(Default)]
pub struct BeginFrameSource {
    /// The last vsync: its time and the interval.
    last: Option<(Ticks, Duration)>,
}

impl BeginFrameSource {
    /// A vsync's begin frame (`OnVSyncOnSequence`).
    pub fn on_vsync(&mut self, time: Ticks, interval: Duration) -> BeginFrameArgs {
        self.last = Some((time, interval));
        BeginFrameArgs {
            frame_time: time,
            interval,
            missed: false,
        }
    }

    /// The begin frame for an observer that begins to ask at `now` (`GetMissedBeginFrameArgs`): the
    /// last vsync's tick at or before `now` (`now.SnappedToNextTick(frame_time, interval) -
    /// interval`), or, before any vsync, `now` at the default interval ("so that we don't have to
    /// wait until vsync").
    pub fn missed(&self, now: Ticks) -> BeginFrameArgs {
        let (frame_time, interval) = match self.last {
            Some((last, interval)) if !interval.is_zero() => {
                let since = now.since(last);
                let into = Duration::from_nanos((since.as_nanos() % interval.as_nanos()) as u64);
                // (on a tick exactly, `SnappedToNextTick` is `now` itself: the tick before it)
                let back = if into.is_zero() { interval } else { into };
                (now.before(back), interval)
            }
            _ => (now, DEFAULT_INTERVAL),
        };
        BeginFrameArgs {
            frame_time,
            interval,
            missed: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    #[test]
    fn missed_begin_frames_fall_on_the_last_vsync_tick() {
        let mut source = BeginFrameSource::default();
        let now = Ticks::now();
        // Before any vsync: now, at 60 Hz.
        let first = source.missed(now);
        assert_eq!(first.frame_time, now);
        assert_eq!(first.interval, DEFAULT_INTERVAL);
        assert!(first.missed);
        // After one: the latest tick of its grid at or before now.
        let interval = Duration::from_micros(5556);
        let vsync = now.before(Duration::from_micros(5556 * 3 + 1000));
        let args = source.on_vsync(vsync, interval);
        assert!(!args.missed);
        let missed = source.missed(now);
        assert_eq!(missed.interval, interval);
        let back = now.since(missed.frame_time);
        assert!(
            back.abs_diff(Duration::from_micros(1000)) < Duration::from_micros(5),
            "{back:?}"
        );
    }

    #[test]
    fn the_vsync_thread_notifies_only_while_observed() {
        let (tx, rx) = channel();
        let thread = VSyncThread::start(move |time, interval| {
            let _ = tx.send((time, interval));
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(rx.try_recv().is_err(), "nothing while no one observes");
        thread.set_observing(true);
        let start = Instant::now();
        let ticks: Vec<_> = (0..10)
            .map(|_| rx.recv_timeout(Duration::from_secs(1)).unwrap())
            .collect();
        let elapsed = start.elapsed();
        let interval = ticks[0].1;
        // Ten vsyncs take about ten intervals, each a vsync later than the one before.
        assert!(
            elapsed >= interval * 8 && elapsed <= interval * 10 + Duration::from_millis(60),
            "{elapsed:?} for 10 of {interval:?}"
        );
        for w in ticks.windows(2) {
            assert!(w[1].0 > w[0].0);
        }
        thread.set_observing(false);
        std::thread::sleep(interval * 3);
        while rx.try_recv().is_ok() {}
        std::thread::sleep(interval * 5);
        assert!(rx.try_recv().is_err(), "none after observing stops");
    }
}
