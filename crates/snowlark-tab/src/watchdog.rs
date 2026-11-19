// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Finding a tab whose script does not return, and stopping that script. A tab's thread marks the
//! start and the end of each pass of its loop in a [`Liveness`] the window shares; the window asks it
//! once a second whether the tab has been busy for too long with the user waiting on it, and, if the
//! user chooses so, has V8 terminate the running script ([`Terminator`]), which keeps the page.
//!
//! In one process the two ends share the cells in memory. A tab in a renderer process keeps them in
//! a small section the browser made and shared ([`Liveness::shared`]), so the window reads the
//! renderer's passes as it reads a thread's; the times are on the browser's clock (the renderer's
//! epoch is set from it at the start). What the browser reads there is the renderer's word: it can
//! make the tab look hung or not, nothing more ([`Liveness::stuck`] is total over any values).
//!
//! The terminator stops script loops only. A hang in native code needs the process killed, which the
//! browser's end of a renderer process does ([`crate::renderer_process::RendererProcess::kill_script`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use snowlark_ipc::Section;
use snowlark_page::Terminator;

/// How long a tab's pass of its loop may run, with input waiting on it, before the window offers to
/// stop it. A pass is a slice of work of a few milliseconds; scripts run inside it.
pub const HANG_AFTER: Duration = Duration::from_secs(5);

/// How long after the user chose to wait the window asks again about the same stuck pass.
pub const ASK_AGAIN_AFTER: Duration = Duration::from_secs(15);

/// The bytes of a shared liveness section: two cells.
pub const LIVENESS_BYTES: usize = 16;

/// Which cell: when the pass in progress began (ms after the epoch, plus one; 0 while the thread
/// waits for something to do), and when input last reached the tab (same unit).
const PASS: usize = 0;
const INPUT: usize = 1;

/// Where the two cells are.
enum Cells {
    /// One process: the tab's thread and the window share these.
    Local([AtomicU64; 2]),
    /// A renderer process and the browser share this section.
    Shared(Section),
}

impl Cells {
    fn load(&self, cell: usize) -> u64 {
        match self {
            Cells::Local(c) => c[cell].load(Ordering::Relaxed),
            Cells::Shared(s) => s.load_u64(cell * 8).unwrap_or(0),
        }
    }

    fn store(&self, cell: usize, value: u64) {
        match self {
            Cells::Local(c) => c[cell].store(value, Ordering::Relaxed),
            Cells::Shared(s) => {
                s.store_u64(cell * 8, value);
            }
        }
    }

    fn raise(&self, cell: usize, value: u64) {
        match self {
            Cells::Local(c) => {
                c[cell].fetch_max(value, Ordering::Relaxed);
            }
            Cells::Shared(s) => {
                s.fetch_max_u64(cell * 8, value);
            }
        }
    }
}

/// What a tab's thread tells the window about whether it is getting anywhere.
pub struct Liveness {
    epoch: Instant,
    cells: Cells,
    /// The tab's page stops script through this once the thread has made it.
    terminator: OnceLock<Terminator>,
    /// A kill is under way: the thread lets scripts run again once the pass that was stopped ends.
    killing: AtomicBool,
}

impl Default for Liveness {
    fn default() -> Self {
        Liveness::with(Instant::now(), Cells::Local(Default::default()))
    }
}

impl Liveness {
    fn with(epoch: Instant, cells: Cells) -> Liveness {
        Liveness {
            epoch,
            cells,
            terminator: OnceLock::new(),
            killing: AtomicBool::new(false),
        }
    }

    /// The browser's end of a renderer process's liveness, in `section` (of [`LIVENESS_BYTES`]),
    /// which the browser made and shares with the renderer.
    pub fn shared(section: Section) -> Liveness {
        Liveness::with(Instant::now(), Cells::Shared(section))
    }

    /// The renderer's end: the section the browser shared, and its clock, [`Liveness::now_ms`] when it
    /// sent it (the transit, a millisecond or so, makes the renderer's passes look that much older).
    pub fn shared_since(section: Section, browser_now: u64) -> Liveness {
        let now = Instant::now();
        let ago = Duration::from_millis(browser_now.saturating_sub(1));
        Liveness::with(now.checked_sub(ago).unwrap_or(now), Cells::Shared(section))
    }

    fn ms(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.epoch).as_millis() as u64 + 1
    }

    /// Now on this liveness's clock (what the browser sends the renderer to set its epoch by).
    pub fn now_ms(&self) -> u64 {
        self.ms(Instant::now())
    }

    /// The tab's thread: a pass of its loop begins at `at`; `with_input` when the commands it
    /// handles include some the user's input made.
    pub fn begin(&self, at: Instant, with_input: bool) {
        let ms = self.ms(at);
        if with_input {
            self.cells.raise(INPUT, ms);
        }
        self.cells.store(PASS, ms);
    }

    /// The tab's thread: the pass ended, and the thread waits for its next job. Lets scripts run
    /// again if the pass was stopped.
    pub fn end(&self) {
        self.cells.store(PASS, 0);
        if self.killing.swap(false, Ordering::AcqRel) {
            if let Some(t) = self.terminator.get() {
                t.resume();
            }
        }
    }

    /// How long ago the tab was started (`SNOWLARK_TAB_STATS` prints the new-tab times from it).
    pub fn age(&self) -> Duration {
        self.epoch.elapsed()
    }

    /// The tab's thread: how to stop its page's script.
    pub fn set_terminator(&self, terminator: Terminator) {
        let _ = self.terminator.set(terminator);
    }

    /// The window: the user did something to the tab (a click, a key, a navigation).
    pub fn input(&self, at: Instant) {
        self.cells.raise(INPUT, self.ms(at));
    }

    /// The pass in progress: its start, which identifies it, or 0 while the tab waits for work.
    pub fn pass(&self) -> u64 {
        self.cells.load(PASS)
    }

    /// The window: the start of the pass the tab is stuck in, if it has been busy for `after` at
    /// `now` with input given since the pass began. The start identifies the pass. Any cell values
    /// give an answer (a renderer's are its word): a pass that starts after `now` is not stuck.
    pub fn stuck(&self, now: Instant, after: Duration) -> Option<u64> {
        let started = self.cells.load(PASS);
        let now = self.ms(now);
        (started != 0
            && started <= now
            && self.cells.load(INPUT) >= started
            && now - started >= after.as_millis() as u64)
            .then_some(started)
    }

    /// The window (or a renderer's IO thread, for the browser): stops the script the tab is
    /// running. False when the page has no way to be stopped (a driver without scripts).
    pub fn kill(&self) -> bool {
        let Some(t) = self.terminator.get() else {
            return false;
        };
        self.killing.store(true, Ordering::Release);
        t.terminate();
        true
    }
}

/// How many threads of the GPU process a [`Passes`] page watches.
pub const PASS_SLOTS: usize = 128;

/// The bytes of a shared [`Passes`] page: one cell per thread.
pub const PASSES_BYTES: usize = PASS_SLOTS * 8;

/// The GPU process's threads' passes (`docs/process-isolation.md`, M7 stage 3), in a page the
/// browser made and shared: one cell per thread, a count of its passes made odd while one is in
/// progress (a thread waiting for work is not in one). The browser reads the cells
/// ([`Passes::read`]) and ends the process when one pass stays in progress too long
/// ([`HangWatch`]). No clock is shared: a pass is known by its count alone. The cells are the GPU
/// process's word: it can make itself look hung or not, nothing more.
pub struct Passes {
    section: Section,
    /// Which cells a thread holds (in the process that writes them).
    taken: std::sync::Mutex<[bool; PASS_SLOTS]>,
}

impl Passes {
    /// The browser's end: a new page, to share with the GPU process.
    pub fn create() -> Result<Passes, snowlark_ipc::Error> {
        Section::create(PASSES_BYTES).map(Passes::over)
    }

    /// Either end, over `section` (of [`PASSES_BYTES`]).
    pub fn over(section: Section) -> Passes {
        Passes {
            section,
            taken: std::sync::Mutex::new([false; PASS_SLOTS]),
        }
    }

    pub fn section(&self) -> &Section {
        &self.section
    }

    /// A thread's cell: a [`PassMark`] that marks its passes, or one that marks nothing when every
    /// cell is held.
    pub fn mark(self: &std::sync::Arc<Passes>) -> PassMark {
        let mut taken = self.taken.lock().unwrap_or_else(|e| e.into_inner());
        match taken.iter().position(|t| !t) {
            Some(slot) => {
                taken[slot] = true;
                // (a cell left by a thread that ended reads as waiting)
                let count = self.section.load_u64(slot * 8).unwrap_or(0);
                self.section.store_u64(slot * 8, count + (count & 1));
                PassMark(Some((self.clone(), slot)))
            }
            None => {
                eprintln!("[gpu] every pass cell is held: a thread goes unwatched");
                PassMark::default()
            }
        }
    }

    /// The browser: every cell as it is now.
    pub fn read(&self) -> [u64; PASS_SLOTS] {
        std::array::from_fn(|slot| self.section.load_u64(slot * 8).unwrap_or(0))
    }
}

/// One thread's cell of a [`Passes`] page (the default marks nothing). Dropping it lets another
/// thread have the cell.
#[derive(Default)]
pub struct PassMark(Option<(std::sync::Arc<Passes>, usize)>);

impl PassMark {
    fn bump(&self, odd: bool) {
        if let Some((passes, slot)) = &self.0 {
            let count = passes.section.load_u64(slot * 8).unwrap_or(0);
            // (one writer: the thread that holds the cell)
            let next = if (count & 1 == 1) == odd {
                count + 2
            } else {
                count + 1
            };
            passes.section.store_u64(slot * 8, next);
        }
    }

    /// A pass begins (a new one, if one was in progress).
    pub fn begin(&self) {
        self.bump(true);
    }

    /// The pass ended: the thread waits for work.
    pub fn end(&self) {
        if let Some((passes, slot)) = &self.0 {
            let count = passes.section.load_u64(slot * 8).unwrap_or(0);
            if count & 1 == 1 {
                passes.section.store_u64(slot * 8, count + 1);
            }
        }
    }
}

impl Drop for PassMark {
    fn drop(&mut self) {
        self.end();
        if let Some((passes, slot)) = &self.0 {
            passes.taken.lock().unwrap_or_else(|e| e.into_inner())[*slot] = false;
        }
    }
}

/// The browser's watch over a [`Passes`] page, read every second or so: a thread is hung when the
/// browser has seen the same pass in progress for [`HangWatch::after`]. Only time the browser itself
/// watched counts (a gap between two reads counts as `max_gap` at most), so a browser that was
/// suspended or starved does not take the gap for a hang.
pub struct HangWatch {
    after: Duration,
    max_gap: Duration,
    last: Option<Instant>,
    seen: [(u64, Duration); PASS_SLOTS],
}

impl HangWatch {
    pub fn new(after: Duration, max_gap: Duration) -> HangWatch {
        HangWatch {
            after,
            max_gap,
            last: None,
            seen: [(0, Duration::ZERO); PASS_SLOTS],
        }
    }

    pub fn after(&self) -> Duration {
        self.after
    }

    /// The cells read at `now`: the first cell whose pass has been in progress for
    /// [`HangWatch::after`], if any. Any values give an answer.
    pub fn check(&mut self, cells: &[u64; PASS_SLOTS], now: Instant) -> Option<usize> {
        let gap = self
            .last
            .map_or(Duration::ZERO, |l| now.saturating_duration_since(l))
            .min(self.max_gap);
        self.last = Some(now);
        let mut hung = None;
        for (slot, (&count, seen)) in cells.iter().zip(self.seen.iter_mut()).enumerate() {
            if count & 1 == 1 && count == seen.0 {
                seen.1 += gap;
            } else {
                *seen = (count, Duration::ZERO);
            }
            if hung.is_none() && seen.1 >= self.after {
                hung = Some(slot);
            }
        }
        hung
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_left_in_progress_is_hung_once_watched_long_enough() {
        let passes = std::sync::Arc::new(Passes::create().unwrap());
        let a = passes.mark();
        let b = passes.mark();
        let mut watch = HangWatch::new(Duration::from_secs(10), Duration::from_secs(2));
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        // Waiting for work: never hung.
        for s in 0..30 {
            assert_eq!(watch.check(&passes.read(), at(s)), None);
        }
        // A thread that keeps making passes (one each second, never waiting) is not hung.
        for s in 30..60 {
            a.begin();
            assert_eq!(watch.check(&passes.read(), at(s)), None);
        }
        a.end();
        // One pass left in progress: hung after ten seconds of watching it.
        b.begin();
        for s in 60..70 {
            assert_eq!(watch.check(&passes.read(), at(s)), None, "{s}");
        }
        assert_eq!(watch.check(&passes.read(), at(70)), Some(1));
        b.end();
        assert_eq!(watch.check(&passes.read(), at(71)), None);
        // A gap between reads (the browser suspended) counts as two seconds at most.
        b.begin();
        assert_eq!(watch.check(&passes.read(), at(72)), None);
        assert_eq!(watch.check(&passes.read(), at(3_600)), None);
        for s in 3_601..3_608 {
            assert_eq!(watch.check(&passes.read(), at(s)), None, "{s}");
        }
        assert_eq!(watch.check(&passes.read(), at(3_608)), Some(1));
    }

    #[test]
    fn a_cell_is_held_by_one_thread_and_shared_across_mappings() {
        let browser = Passes::create().unwrap();
        let child = std::sync::Arc::new(Passes::over(second_mapping_of(
            browser.section(),
            PASSES_BYTES,
        )));
        let marks: Vec<PassMark> = (0..PASS_SLOTS).map(|_| child.mark()).collect();
        // Every cell held: the next marks nothing.
        let extra = child.mark();
        extra.begin();
        assert!(browser.read().iter().all(|c| c & 1 == 0));
        marks[5].begin();
        let cells = browser.read();
        assert_eq!(cells[5] & 1, 1);
        assert_eq!(cells.iter().filter(|c| *c & 1 == 1).count(), 1);
        // A thread that ends mid-pass leaves its cell waiting, for the next thread.
        drop(marks);
        assert!(browser.read().iter().all(|c| c & 1 == 0));
        let again = child.mark();
        again.begin();
        assert_eq!(browser.read()[0] & 1, 1);
    }

    fn second_mapping_of(s: &Section, bytes: usize) -> Section {
        let (browser, child) = snowlark_ipc::loopback::pair::<u8, u8>(0).unwrap();
        let handle = browser.share_section(s).unwrap();
        child.map_section(handle, bytes as u64).unwrap()
    }

    fn stuck_only_when_long_and_waited_on(l: &Liveness) {
        let t0 = l.epoch;
        let at = |ms| t0 + Duration::from_millis(ms);
        let after = Duration::from_secs(5);
        // Waiting for work: never stuck.
        assert_eq!(l.stuck(at(60_000), after), None);
        // A long pass nobody waits on is not stuck.
        l.begin(at(100), false);
        assert_eq!(l.stuck(at(60_000), after), None);
        // Input during the pass: stuck once it has run long enough.
        l.input(at(2_000));
        assert_eq!(l.stuck(at(5_000), after), None);
        let started = l.stuck(at(5_200), after).expect("stuck");
        // The same pass keeps its identity; a later pass has another.
        assert_eq!(l.stuck(at(9_000), after), Some(started));
        l.end();
        assert_eq!(l.stuck(at(9_000), after), None);
        l.begin(at(9_500), true);
        assert_eq!(l.stuck(at(15_000), after).map(|s| s != started), Some(true));
        // Input from before the pass does not count against it.
        l.end();
        l.begin(at(20_000), false);
        assert_eq!(l.stuck(at(40_000), after), None);
    }

    #[test]
    fn a_pass_is_stuck_only_when_long_and_waited_on() {
        stuck_only_when_long_and_waited_on(&Liveness::default());
    }

    #[test]
    fn a_shared_liveness_is_the_same_across_its_ends() {
        let section = Section::create(LIVENESS_BYTES).unwrap();
        stuck_only_when_long_and_waited_on(&Liveness::shared(section));
        // Two ends of one section (two mappings, as in two processes): the renderer's passes, on
        // the browser's clock, are what the browser reads.
        let browser = Liveness::shared(Section::create(LIVENESS_BYTES).unwrap());
        let Cells::Shared(s) = &browser.cells else {
            unreachable!()
        };
        let second = second_mapping(s);
        let renderer = Liveness::shared_since(second, browser.now_ms());
        let skew = renderer.epoch.max(browser.epoch) - renderer.epoch.min(browser.epoch);
        assert!(skew < Duration::from_millis(50), "{skew:?}");
        let now = Instant::now();
        renderer.begin(now, false);
        browser.input(now + Duration::from_millis(10));
        let after = Duration::from_secs(5);
        assert_eq!(browser.stuck(now + Duration::from_secs(4), after), None);
        assert_eq!(
            browser.stuck(now + Duration::from_secs(6), after),
            Some(renderer.pass())
        );
        renderer.end();
        assert_eq!(browser.stuck(now + Duration::from_secs(6), after), None);
    }

    /// The same section mapped a second time, as the renderer maps the browser's.
    fn second_mapping(s: &Section) -> Section {
        let (browser, child) = snowlark_ipc::loopback::pair::<u8, u8>(0).unwrap();
        let handle = browser.share_section(s).unwrap();
        child.map_section(handle, LIVENESS_BYTES as u64).unwrap()
    }

    #[test]
    fn what_a_renderer_writes_only_makes_the_tab_look_stuck_or_not() {
        let l = Liveness::shared(Section::create(LIVENESS_BYTES).unwrap());
        let Cells::Shared(s) = &l.cells else {
            unreachable!()
        };
        let now = Instant::now() + Duration::from_secs(100);
        let after = Duration::from_secs(5);
        for pass in [0, 1, 2, 99_000, 100_001, u64::MAX - 1, u64::MAX] {
            for input in [0, 1, pass, u64::MAX] {
                s.store_u64(PASS * 8, pass);
                s.store_u64(INPUT * 8, input);
                let stuck = l.stuck(now, after);
                // (a pass from the future, or with no input since, is never stuck)
                if pass > l.ms(now) || input < pass || pass == 0 {
                    assert_eq!(stuck, None, "{pass} {input}");
                } else if let Some(p) = stuck {
                    assert_eq!(p, pass);
                }
            }
        }
    }
}
