// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The event loop (HTML §8.1.7): task queues by source, timers (HTML §8.6), animation frame callbacks
//! (HTML §8.10), idle callbacks, and the waker other threads (the network) use. The loop itself is
//! [`crate::Page::run_until`]; this module is the state and the steps of one iteration.
//!
//! Rendering opportunities come from one of two clocks, as Chromium's begin frame sources
//! (`components/viz/common/frame_sinks/begin_frame_source.cc`): headless, a fixed 60 Hz timer
//! ([`FRAME_MS`]; `DelayBasedBeginFrameSource` with `BeginFrameArgs::DefaultInterval()`); in the
//! window, the display's vsync, which the compositor sends as begin frames while the page asks for
//! them ([`Engine::use_begin_frames`], [`Engine::begin_frame`], [`Engine::needs_begin_frames`];
//! `ExternalBeginFrameSource`).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use snowlark_bindings::rt::{Cx, Handle, JsObject, JsValue};

use crate::engine::Engine;

/// A task's source (HTML §8.1.7.3 "generic task sources").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskSource {
    DomManipulation,
    UserInteraction,
    Networking,
    HistoryTraversal,
    Timer,
    Parser,
    Script,
    Rendering,
    /// HTML §9.4's "posted message task source".
    PostedMessage,
    /// Intersection Observer's "IntersectionObserver task source".
    IntersectionObserver,
    /// Performance Timeline's "performance timeline task source".
    PerformanceTimeline,
    /// IndexedDB's "database access task source".
    DatabaseAccess,
    /// requestIdleCallback's "idle-task task source".
    IdleTask,
    /// Prioritized Task Scheduling's "scheduler task source".
    Scheduler,
    /// The Storage Standard's "storage task source".
    Storage,
}

/// A task's steps.
pub type TaskFn = Box<dyn FnOnce(&Engine, &mut Cx)>;

pub struct Task {
    pub source: TaskSource,
    pub steps: TaskFn,
    /// The event loop's enqueue order (shared with scheduler tasks, which interleave by age).
    pub seq: u64,
}

/// What a timer runs.
pub enum TimerHandler {
    /// A callback with its arguments and the window it was set on.
    Function(JsObject, Vec<JsValue>, Handle),
    /// Code to compile in the window's realm, with the [`crate::script::ReferrerScript`] id of the
    /// script that set the timer (0: none).
    Code(String, Handle, u32),
    Internal(Option<TaskFn>),
}

pub struct Timer {
    pub handler: TimerHandler,
    /// Present for `setInterval`.
    pub interval: Option<f64>,
    pub nesting: u32,
}

/// The waker the page thread sleeps on.
#[derive(Default)]
pub struct Waker {
    flag: Mutex<bool>,
    cv: Condvar,
    /// Also called on a wake: an embedder whose thread sleeps elsewhere (a browser tab's channel).
    forward: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Waker {
    /// Wakes the page thread (from any thread).
    pub fn wake(&self) {
        let mut f = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        *f = true;
        self.cv.notify_all();
        drop(f);
        let forward = self
            .forward
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(fw) = forward {
            fw();
        }
    }

    /// Calls `f` on every wake as well (the embedder's own wake-up).
    pub fn set_forward(&self, f: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.forward.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    /// Sleeps until woken or until `deadline`. A condition variable's timeout is only as fine as the
    /// system timer (15.6 ms on Windows), so the last stretch is slept in 1 ms steps (`thread::sleep`
    /// uses a high-resolution timer) to keep timers and frames on time.
    pub fn wait_until(&self, deadline: Instant) {
        const COARSE: Duration = Duration::from_millis(20);
        let mut f = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *f {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let left = deadline - now;
            if left > COARSE {
                let (g, _) = self
                    .cv
                    .wait_timeout(f, left - COARSE)
                    .unwrap_or_else(|e| e.into_inner());
                f = g;
            } else {
                drop(f);
                std::thread::sleep(left.min(Duration::from_millis(1)));
                f = self.flag.lock().unwrap_or_else(|e| e.into_inner());
            }
        }
        *f = false;
    }
}

/// The frame interval of a 60 Hz display: the headless frame clock's, and Chromium's
/// `BeginFrameArgs::DefaultInterval()`.
pub const FRAME_MS: f64 = 1000.0 / 60.0;

/// A begin frame: the display's vsync, which starts a rendering opportunity (Chromium's
/// `viz::BeginFrameArgs`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeginFrame {
    /// The vsync's time: the frame's time, which the animation frame callbacks get and the
    /// documents' timelines move to (Blink's `PageAnimator::ServiceScriptedAnimations`).
    pub frame_time: Instant,
    /// The next vsync's (`frame_time + interval`).
    pub deadline: Instant,
    /// The display's refresh interval.
    pub interval: Duration,
    /// Sent when the page started asking for begin frames, for the vsync that had already passed
    /// (`BeginFrameArgs::MISSED`, `ExternalBeginFrameSource::AddObserver`).
    pub missed: bool,
}

/// The begin frame a rendering update will use.
#[derive(Clone, Copy, Debug)]
pub struct PendingFrame {
    pub args: BeginFrame,
    /// The page wanted a rendering update when it came: it is the loop's one begin frame in flight
    /// (cc sends no other `BeginMainFrame` before the main thread has handled it,
    /// `SchedulerStateMachine::ShouldSendBeginMainFrame`), whose time the update uses however late
    /// the loop gets to it. One that came while the page wanted nothing serves only until its
    /// deadline (cc sends a `BeginMainFrame` for a need that arises within the impl frame).
    pub sent: bool,
}

/// requestIdleCallback §5.1: an idle period lasts at most 50 ms.
pub const IDLE_PERIOD_MS: f64 = 50.0;

/// An idle period shorter than this is not started ("if the user agent determines the idle period
/// should be delayed"): Blink's `kMinimumIdlePeriodDuration`. The gaps of a light timer loop (4 ms
/// apart, the clamp of HTML §8.6) are idle time, as in Chrome.
const MIN_IDLE_PERIOD_MS: f64 = 1.0;

/// An entry of a window's "list of idle request callbacks" or "list of runnable idle callbacks"
/// (requestIdleCallback §5.2).
pub struct IdleRequest {
    pub id: u32,
    pub callback: JsObject,
    pub window: Handle,
    /// The timer that invokes the callback once its `timeout` passes.
    pub timeout_timer: Option<i32>,
}

/// The event loop's state.
pub struct EventLoop {
    pub origin: Instant,
    /// The time origin as milliseconds since the Unix epoch (`performance.timeOrigin`).
    pub origin_unix_ms: f64,
    pub tasks: VecDeque<Task>,
    pub timers: HashMap<i32, Timer>,
    /// Timers by (due time in microseconds, start order) → id: "wait until any invocations of this
    /// algorithm ... that started before this one, and whose milliseconds is ≤ this one's".
    pub schedule: BTreeMap<(u64, u64), i32>,
    pub next_timer: i32,
    pub seq: u64,
    /// The timer nesting level of the task running now.
    pub nesting: u32,
    pub raf: Vec<(u32, JsObject, Handle)>,
    pub raf_cancelled: Vec<u32>,
    pub next_raf: u32,
    pub last_frame: f64,
    /// The windows' "list of idle request callbacks" (requested, waiting for an idle period).
    pub idle: Vec<IdleRequest>,
    /// The windows' "list of runnable idle callbacks" (of the current idle period).
    pub idle_runnable: Vec<IdleRequest>,
    /// The "last idle period start time" (ms since the time origin).
    pub idle_period_start: f64,
    /// Whether an "invoke idle callbacks" task is queued.
    pub idle_task_queued: bool,
    /// No idle period starts before this time (ms since the time origin): a frame interval after a
    /// task that ran longer than a frame interval (see [`Engine::note_task_ran`]).
    pub idle_not_before: f64,
    pub next_idle: u32,
    /// Rendering opportunities come from begin frames ([`Engine::use_begin_frames`]), not from the
    /// [`FRAME_MS`] timer.
    pub begin_frames: bool,
    /// The begin frame the next rendering update uses.
    pub begin_frame: Option<PendingFrame>,
    /// The frame time of the last begin frame a rendering update used: a missed one no later is
    /// not used twice (`BeginFrameSource::FilterAndIssueBeginFrame`, against the observer's
    /// `LastUsedBeginFrameArgs`).
    pub last_used_frame: Option<Instant>,
    /// The display's refresh interval (ms): [`FRAME_MS`] until a begin frame says otherwise.
    pub frame_interval: f64,
    /// A rendering update used the last begin frame: the page keeps asking for one more, which a
    /// page that keeps animating is likely to want (cc's `ProactiveBeginFrameWanted`, true for a
    /// frame after a commit).
    pub proactive_frame: bool,
    /// The enqueue order of the next task.
    pub task_order: u64,
    pub waker: Arc<Waker>,
}

/// The resolution of the high resolution time line, in steps per millisecond. HR Time "coarsen time"
/// (the algorithm behind `performance.now()`, `Event.timeStamp` and the timestamp of
/// `requestAnimationFrame`) takes 100 µs, or any coarser value, as the resolution of a context that is
/// not cross-origin isolated, and 5 µs, or coarser, for one that is. No context here is isolated
/// (`crossOriginIsolated` is false everywhere), so the 100 µs of the common case applies. WPT's
/// `hr-time/clamped-time-origin.html` and `timing-attack.html` check this value.
const TIME_STEPS_PER_MS: f64 = 10.0;

/// HR Time "coarsen time" for a time in milliseconds on the time origin's line: rounded down to the
/// resolution above. (A division, not a multiplication by 0.1, so the result is the nearest double
/// to a multiple of the step and script can compare it with decimal literals.)
pub(crate) fn coarsen_time(ms: f64) -> f64 {
    (ms * TIME_STEPS_PER_MS).floor() / TIME_STEPS_PER_MS
}

impl Default for EventLoop {
    fn default() -> Self {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        EventLoop {
            origin: Instant::now(),
            origin_unix_ms: unix,
            tasks: VecDeque::new(),
            timers: HashMap::new(),
            schedule: BTreeMap::new(),
            next_timer: 1,
            seq: 0,
            nesting: 0,
            raf: Vec::new(),
            raf_cancelled: Vec::new(),
            next_raf: 1,
            last_frame: f64::NEG_INFINITY,
            idle: Vec::new(),
            idle_runnable: Vec::new(),
            idle_period_start: f64::NEG_INFINITY,
            idle_task_queued: false,
            idle_not_before: f64::NEG_INFINITY,
            next_idle: 1,
            begin_frames: false,
            begin_frame: None,
            last_used_frame: None,
            frame_interval: FRAME_MS,
            proactive_frame: false,
            task_order: 1,
            waker: Arc::new(Waker::default()),
        }
    }
}

impl Engine {
    /// `performance.now()`: milliseconds since the time origin, coarsened (see [`coarsen_time`]).
    pub fn now(&self) -> f64 {
        let el = self.event_loop.borrow();
        coarsen_time(el.origin.elapsed().as_secs_f64() * 1000.0)
    }

    /// The time `at` on `performance.now()`'s line (milliseconds since the time origin, coarsened;
    /// zero for an instant before the origin).
    pub(crate) fn high_res_time(&self, at: Instant) -> f64 {
        let origin = self.event_loop.borrow().origin;
        coarsen_time(at.saturating_duration_since(origin).as_secs_f64() * 1000.0)
    }

    /// Queues a task.
    pub fn queue_task(&self, source: TaskSource, steps: impl FnOnce(&Engine, &mut Cx) + 'static) {
        let seq = self.next_task_order();
        self.event_loop.borrow_mut().tasks.push_back(Task {
            source,
            steps: Box::new(steps),
            seq,
        });
    }

    /// The next enqueue order (Prioritized Task Scheduling's "next enqueue order").
    pub(crate) fn next_task_order(&self) -> u64 {
        let mut el = self.event_loop.borrow_mut();
        let order = el.task_order;
        el.task_order += 1;
        order
    }

    /// Queues a task whose document is `doc`: "destroy a document" (HTML §7.5) removes a document's
    /// tasks, so this one does nothing once `doc` is destroyed. It cannot be taken from the queue
    /// here, as tasks do not name their document, so it looks when its turn comes.
    pub(crate) fn queue_document_task(
        &self,
        source: TaskSource,
        doc: snowlark_dom::NodeId,
        steps: impl FnOnce(&Engine, &mut Cx) + 'static,
    ) {
        self.queue_task(source, move |e, cx| {
            if !e.document_destroyed(doc) {
                steps(e, cx);
            }
        });
    }

    /// "Queue an element task" that fires a simple event at an element.
    pub(crate) fn queue_element_task_fire(
        &self,
        el: snowlark_dom::NodeId,
        type_: &'static str,
        bubbles: bool,
        cancelable: bool,
    ) {
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if e.dom().is_alive(el) {
                e.fire_event(
                    cx,
                    crate::engine::node_handle(el),
                    type_,
                    bubbles,
                    cancelable,
                );
            }
        });
    }

    /// HTML §8.6 "timer initialization steps": schedules a timer and returns its handle.
    pub(crate) fn start_timer(
        &self,
        handler: TimerHandler,
        timeout: f64,
        repeat: bool,
        previous: Option<i32>,
    ) -> i32 {
        let now = self.now();
        let mut el = self.event_loop.borrow_mut();
        let id = previous.unwrap_or_else(|| {
            let id = el.next_timer;
            el.next_timer = el.next_timer.wrapping_add(1).max(1);
            id
        });
        let nesting = el.nesting + 1;
        let mut timeout = if timeout.is_finite() {
            timeout.max(0.0)
        } else {
            0.0
        };
        if nesting > 5 && timeout < 4.0 {
            timeout = 4.0;
        }
        let due = now + timeout;
        el.seq += 1;
        let seq = el.seq;
        el.schedule.insert(((due * 1000.0) as u64, seq), id);
        el.timers.insert(
            id,
            Timer {
                handler,
                interval: repeat.then_some(timeout),
                nesting,
            },
        );
        id
    }

    /// `clearTimeout` / `clearInterval`.
    pub(crate) fn clear_timer(&self, id: i32) {
        let mut el = self.event_loop.borrow_mut();
        if el.timers.remove(&id).is_some() {
            el.schedule.retain(|_, v| *v != id);
        } else {
            // Possibly the timer running now: an interval cleared from its own callback stops.
            self.cleared_while_running.borrow_mut().insert(id);
        }
    }

    /// Moves due timers into the timer task queue, in order.
    fn queue_due_timers(&self) {
        let now = self.now();
        let due: Vec<i32> = {
            let mut el = self.event_loop.borrow_mut();
            let mut out = Vec::new();
            while let Some((&(due_us, seq), &id)) = el.schedule.iter().next() {
                if (due_us as f64) / 1000.0 > now {
                    break;
                }
                el.schedule.remove(&(due_us, seq));
                out.push(id);
            }
            out
        };
        for id in due {
            self.queue_task(TaskSource::Timer, move |e, cx| e.run_timer(cx, id));
        }
    }

    /// The timer task: run the handler, then reschedule an interval.
    fn run_timer(&self, cx: &mut Cx, id: i32) {
        let Some(timer) = self.event_loop.borrow_mut().timers.remove(&id) else {
            return;
        };
        self.cleared_while_running.borrow_mut().remove(&id);
        let Timer {
            handler,
            interval,
            nesting,
            ..
        } = timer;
        let previous_nesting =
            std::mem::replace(&mut self.event_loop.borrow_mut().nesting, nesting);
        let keep = match &handler {
            TimerHandler::Function(f, args, window) => {
                if self.context_discarded(*window) {
                    false
                } else {
                    let (f, args, window) = (f.clone(), args.clone(), *window);
                    self.call_timer_function(cx, &f, &args, window);
                    true
                }
            }
            TimerHandler::Code(code, window, referrer) => {
                if self.context_discarded(*window) {
                    false
                } else {
                    let (code, window, referrer) = (code.clone(), *window, *referrer);
                    self.run_timer_code(cx, &code, window, referrer);
                    true
                }
            }
            TimerHandler::Internal(_) => false,
        };
        let handler = match handler {
            TimerHandler::Internal(Some(steps)) => {
                steps(self, cx);
                None
            }
            other => keep.then_some(other),
        };
        self.event_loop.borrow_mut().nesting = previous_nesting;
        // An interval that was not cleared while it ran starts again with the same handle.
        if let (Some(ms), Some(handler)) = (interval, handler) {
            let cleared = self.cleared_while_running.borrow_mut().remove(&id);
            if !cleared {
                let mut el = self.event_loop.borrow_mut();
                el.nesting = nesting;
                drop(el);
                self.start_timer(handler, ms, true, Some(id));
                self.event_loop.borrow_mut().nesting = previous_nesting;
            }
        }
    }

    /// Whether a task is ready now.
    pub fn has_ready_task(&self) -> bool {
        // (fetches that finished become resource timing entries before the next task)
        self.drain_resource_timings();
        self.queue_due_timers();
        !self.event_loop.borrow().tasks.is_empty() || self.scheduler_task_ready()
    }

    /// Takes the oldest ready task.
    pub fn take_task(&self) -> Option<Task> {
        self.queue_due_timers();
        let oldest = self.event_loop.borrow().tasks.front().map(|t| t.seq);
        if let Some(t) = self.take_scheduler_task(oldest) {
            return Some(t);
        }
        self.event_loop.borrow_mut().tasks.pop_front()
    }

    /// Whether a rendering opportunity is due: animation frame callbacks wait or the rendering is out
    /// of date, and the frame clock has a frame: a frame interval has passed since the last frame
    /// (headless), or a begin frame is there to use ([`Engine::use_begin_frames`]).
    pub fn frame_due(&self) -> bool {
        if !self.frame_wanted() {
            return false;
        }
        let el = self.event_loop.borrow();
        if el.begin_frames {
            return el
                .begin_frame
                .is_some_and(|f| f.sent || Instant::now() < f.args.deadline);
        }
        let last = el.last_frame;
        drop(el);
        self.now() - last >= FRAME_MS
    }

    /// Whether the page wants a rendering update at its next rendering opportunity: animation frame
    /// callbacks wait, or the rendering is out of date (cc's `needs_begin_main_frame_`).
    fn frame_wanted(&self) -> bool {
        let el = self.event_loop.borrow();
        // A document with pending initial intersection targets does not skip its rendering update.
        if el.raf.is_empty()
            && !self.render_wanted()
            && !self.intersection_pending()
            && !self.resize_pending()
            && !self.event_timing_pending()
            && !self.animations_need_frame()
            && self.pointer.borrow().held.is_none()
        {
            return false;
        }
        drop(el);
        // HTML §8.1.7.3 "update the rendering" skips a render-blocked document, and the documents
        // of a hidden page (their navigables have no rendering opportunity).
        !self.render_blocked() && !self.page_hidden()
    }

    /// Whether pointer moves wait for the next rendering update ([`Engine::queue_pointer_move`]):
    /// while rendering opportunities come from begin frames and the page has them.
    pub(crate) fn moves_aligned(&self) -> bool {
        self.event_loop.borrow().begin_frames && !self.render_blocked() && !self.page_hidden()
    }

    /// Rendering opportunities come from begin frames from now on ([`Engine::begin_frame`]) instead
    /// of the 60 Hz timer: the window's compositor sends them at the display's vsync while the page
    /// asks for them ([`Engine::needs_begin_frames`]), as Chromium's renderer gets its begin frames
    /// from viz (`ExternalBeginFrameSource`; on Windows `ExternalBeginFrameSourceWin`, driven by
    /// `gl::VSyncThreadWin`).
    pub fn use_begin_frames(&self) {
        self.event_loop.borrow_mut().begin_frames = true;
    }

    /// Whether the page asks for begin frames: it wants a rendering update, or one used the last
    /// begin frame (cc's `SchedulerStateMachine::ShouldSubscribeToBeginFrames`:
    /// `BeginFrameRequiredForAction` or `ProactiveBeginFrameWanted`). A hidden page asks for none
    /// (it is not `visible_`).
    pub fn needs_begin_frames(&self) -> bool {
        let el = self.event_loop.borrow();
        if !el.begin_frames {
            return false;
        }
        if el.begin_frame.is_some_and(|f| f.sent) {
            return true;
        }
        let proactive = el.proactive_frame;
        drop(el);
        (proactive && !self.page_hidden()) || self.frame_wanted()
    }

    /// A begin frame from the compositor (`BeginFrameObserver::OnBeginFrame`). A missed one whose
    /// deadline has passed is dropped (`Scheduler::BeginImplFrameWithDeadline`: "Discard missed
    /// begin frames if they are too late"), as is one no later than the last frame used. While the
    /// begin frame in flight waits for the loop, later ones are dropped: the update uses the first,
    /// as a `BeginMainFrame` carries the args of the impl frame that sent it. A begin frame that comes
    /// while the page wants nothing ends its proactive frame, so it stops asking.
    pub fn begin_frame(&self, args: BeginFrame) {
        if !self.event_loop.borrow().begin_frames {
            return;
        }
        if args.missed && args.deadline < Instant::now() {
            return;
        }
        let wanted = self.frame_wanted();
        let mut el = self.event_loop.borrow_mut();
        if el.last_used_frame.is_some_and(|t| args.frame_time <= t) {
            return;
        }
        el.frame_interval = args.interval.as_secs_f64() * 1000.0;
        if el.begin_frame.is_some_and(|f| f.sent) && wanted {
            return;
        }
        if !wanted {
            el.proactive_frame = false;
        }
        el.begin_frame = Some(PendingFrame { args, sent: wanted });
    }

    /// The frame time a rendering update starts with (ms since the time origin): the begin frame's,
    /// which the update uses up, or now (the 60 Hz timer's frames, and an update forced outside a
    /// frame). With begin frames it becomes the last frame's time, from which the next frame is
    /// expected (Blink's `MainThreadSchedulerImpl::WillBeginFrame`: `estimated_next_frame_begin_`
    /// is `frame_time + interval`).
    pub(crate) fn take_frame_time(&self) -> f64 {
        let now = self.now();
        let mut el = self.event_loop.borrow_mut();
        if !el.begin_frames {
            return now;
        }
        let time = match el.begin_frame.take() {
            Some(f) => {
                el.last_used_frame = Some(f.args.frame_time);
                el.proactive_frame = true;
                let since = f.args.frame_time.saturating_duration_since(el.origin);
                coarsen_time(since.as_secs_f64() * 1000.0).min(now)
            }
            None => now,
        };
        el.last_frame = time;
        time
    }

    /// The display's refresh interval (ms): the frame interval of the 60 Hz timer headless.
    pub fn frame_interval(&self) -> f64 {
        self.event_loop.borrow().frame_interval
    }

    /// When the next frame is expected (ms since the time origin): a frame interval after the
    /// last; with begin frames, on the display's vsync grid when the last was long ago.
    fn next_frame_time(&self, now: f64) -> f64 {
        let el = self.event_loop.borrow();
        let interval = el.frame_interval;
        let next = el.last_frame + interval;
        if el.begin_frames && next < now && el.last_frame.is_finite() {
            next + ((now - next) / interval).ceil() * interval
        } else {
            next
        }
    }

    /// HTML §8.1.7.3 "update the rendering" step "run the animation frame callbacks".
    pub fn run_animation_frames(&self, cx: &mut Cx) {
        let now = self.now();
        self.run_animation_frames_at(cx, now);
    }

    /// "Run the animation frame callbacks" with the frame's time `now` (ms), which
    /// `document.timeline.currentTime` reads too.
    pub fn run_animation_frames_at(&self, cx: &mut Cx, now: f64) {
        let last = self.now();
        let callbacks = {
            let mut el = self.event_loop.borrow_mut();
            // (with begin frames, the frame's time: `take_frame_time`)
            if !el.begin_frames {
                el.last_frame = last;
            }
            el.raf_cancelled.clear();
            std::mem::take(&mut el.raf)
        };
        for (id, cb, window) in callbacks {
            if self.event_loop.borrow().raf_cancelled.contains(&id)
                || self.context_discarded(window)
            {
                continue;
            }
            self.call_frame_callback(cx, &cb, now, window);
        }
    }

    /// HTML §8.1.7.3 (event loop processing model, the step after "update the rendering"): when
    /// no task is queued, the microtask queue is empty and no rendering opportunity is due, "start
    /// an idle period" (requestIdleCallback §5.3): the requested idle callbacks become runnable and
    /// an "invoke idle callbacks" task is queued. Returns whether it queued that task.
    pub fn start_idle_period(&self) -> bool {
        {
            let el = self.event_loop.borrow();
            if el.idle_task_queued || (el.idle.is_empty() && el.idle_runnable.is_empty()) {
                return false;
            }
        }
        if self.has_ready_task() || self.frame_due() {
            return false;
        }
        let now = self.now();
        if now < self.event_loop.borrow().idle_not_before
            || self.deadline_from(now) - now < MIN_IDLE_PERIOD_MS
        {
            return false;
        }
        let mut el = self.event_loop.borrow_mut();
        el.idle_period_start = now;
        let pending = std::mem::take(&mut el.idle);
        el.idle_runnable.extend(pending);
        el.idle_task_queued = true;
        drop(el);
        self.queue_task(TaskSource::IdleTask, |e, cx| e.invoke_idle_callbacks(cx));
        true
    }

    /// After a task that started at `start` (ms since the time origin) has run. A task longer than a
    /// frame interval held up the rendering opportunity due meanwhile, so the loop is not idle until
    /// a frame interval after it: Blink starts a long idle period only once the compositor expects
    /// no frame soon (`BeginFrameNotExpectedSoon`), which a main thread that missed a frame does not
    /// get before the next frame interval. (requestIdleCallback §5.3 lets the user agent delay an idle
    /// period; a busy loop of long tasks with short gaps has none, WPT `callback-timeout-when-busy`.)
    pub fn note_task_ran(&self, start: f64) {
        let end = self.now();
        let interval = self.frame_interval();
        if end - start > interval {
            self.event_loop.borrow_mut().idle_not_before = end + interval;
        }
    }

    /// HTML "compute deadline" for the current idle period: 50 ms after its start, but no later
    /// than the next rendering opportunity when renders are pending, nor the next active timer.
    pub(crate) fn idle_deadline(&self) -> f64 {
        let start = self.event_loop.borrow().idle_period_start;
        self.deadline_from(start)
    }

    /// "Compute deadline" for an idle period starting at `start`.
    fn deadline_from(&self, start: f64) -> f64 {
        let mut deadline = start + IDLE_PERIOD_MS;
        let pending_renders = !self.event_loop.borrow().raf.is_empty() || self.render_wanted();
        if pending_renders {
            deadline = deadline.min(self.next_frame_time(start));
        }
        let el = self.event_loop.borrow();
        // (the map of active timers: setTimeout and setInterval, not the engine's own timers)
        let next_timer = el.schedule.iter().find_map(|(&(due_us, _), id)| {
            el.timers
                .get(id)
                .filter(|t| !matches!(t.handler, TimerHandler::Internal(_)))
                .map(|_| due_us as f64 / 1000.0)
        });
        if let Some(t) = next_timer {
            deadline = deadline.min(t);
        }
        deadline
    }

    /// requestIdleCallback §5.4 "invoke idle callbacks": while the idle period lasts, the first
    /// runnable callback runs (one per task); callbacks requested meanwhile wait for the next one.
    fn invoke_idle_callbacks(&self, cx: &mut Cx) {
        self.event_loop.borrow_mut().idle_task_queued = false;
        if self.now() >= self.idle_deadline() {
            return;
        }
        let request = {
            let mut el = self.event_loop.borrow_mut();
            if el.idle_runnable.is_empty() {
                return;
            }
            el.idle_runnable.remove(0)
        };
        if let Some(timer) = request.timeout_timer {
            self.clear_timer(timer);
        }
        if !self.context_discarded(request.window) {
            let start = self.event_loop.borrow().idle_period_start;
            let deadline = self.alloc(crate::engine::Object::IdleDeadline {
                period_start: start,
                did_timeout: false,
            });
            self.set_idle_scheduling_state(cx, Some(request.window));
            self.call_idle_callback(cx, &request.callback, deadline, request.window);
            self.set_idle_scheduling_state(cx, None);
        }
        let mut el = self.event_loop.borrow_mut();
        if !el.idle_runnable.is_empty() && !el.idle_task_queued {
            el.idle_task_queued = true;
            drop(el);
            self.queue_task(TaskSource::IdleTask, |e, cx| e.invoke_idle_callbacks(cx));
        }
    }

    /// requestIdleCallback §5.5 "invoke idle callback timeout": a callback still waiting when its
    /// timeout passes runs now, with no time remaining and `didTimeout` true.
    pub(crate) fn invoke_idle_callback_timeout(&self, cx: &mut Cx, id: u32) {
        let request = {
            let mut el = self.event_loop.borrow_mut();
            if let Some(i) = el.idle.iter().position(|r| r.id == id) {
                Some(el.idle.remove(i))
            } else if let Some(i) = el.idle_runnable.iter().position(|r| r.id == id) {
                Some(el.idle_runnable.remove(i))
            } else {
                None
            }
        };
        let Some(request) = request else { return };
        if self.context_discarded(request.window) {
            return;
        }
        let deadline = self.alloc(crate::engine::Object::IdleDeadline {
            period_start: self.now(),
            did_timeout: true,
        });
        self.call_idle_callback(cx, &request.callback, deadline, request.window);
    }

    /// When the loop next has something to do: the earliest timer or frame.
    pub fn next_wakeup(&self) -> Option<Instant> {
        let el = self.event_loop.borrow();
        let mut next: Option<f64> = el.schedule.keys().next().map(|(us, _)| *us as f64 / 1000.0);
        // (idle callbacks wait for the idle period a long task delayed)
        if !el.idle.is_empty() && el.idle_not_before.is_finite() {
            let at = el.idle_not_before;
            next = Some(next.map_or(at, |n| n.min(at)));
        }
        // (a hidden page has no rendering opportunities: no frame to wake for; with begin frames,
        // the next one wakes the loop, and an animation's future frame is only a time to wake at,
        // when the page starts asking for begin frames again)
        if el.begin_frames {
            // (a held move's fallback, for a frame that never comes)
            if let Some(at) = self.held_move_deadline() {
                next = Some(next.map_or(at, |n| n.min(at)));
            }
            let now_ms = el.origin.elapsed().as_secs_f64() * 1000.0;
            if let Some(at) = self.animations_frame_at().filter(|&at| at > now_ms) {
                next = Some(next.map_or(at, |n| n.min(at)));
            }
        } else if !self.page_hidden() {
            let earliest = el.last_frame + FRAME_MS;
            let frame = if !el.raf.is_empty()
                || self.render_wanted()
                || self.intersection_pending()
                || self.resize_pending()
                || self.event_timing_pending()
            {
                Some(earliest)
            } else {
                // (an animation that cannot show wakes the loop at its finish only)
                self.animations_frame_at().map(|at| at.max(earliest))
            };
            if let Some(frame) = frame {
                next = Some(next.map_or(frame, |n| n.min(frame)));
            }
        }
        let next = next.map(|ms| el.origin + Duration::from_secs_f64((ms.max(0.0)) / 1000.0));
        // (media elements' periodic steps: `timeupdate`, `progress`; animated images' frames)
        [next, self.media_next_wakeup(), self.image_frame_wakeup()]
            .into_iter()
            .flatten()
            .min()
    }

    /// Whether anything could still happen without input: tasks, timers, frames, idle callbacks,
    /// fetches in flight.
    pub fn has_pending_work(&self) -> bool {
        let el = self.event_loop.borrow();
        !el.tasks.is_empty()
            || self.scheduler_task_ready()
            || !el.schedule.is_empty()
            || !el.raf.is_empty()
            || !el.idle.is_empty()
            || !el.idle_runnable.is_empty()
            || self.loader_busy()
            || self.render_loading()
            || self.render_dirty()
            || self.render_events_pending()
            || self.animation_work_pending()
            || self.intersection_pending()
            || self.resize_pending()
            || self.event_timing_pending()
            || self.media_pending()
            || self.websockets_pending()
            || self.idb_pending()
            || self.audio_pending()
            || self.canvas_frames_pending()
            || self.fullscreen_events_pending()
    }

    /// The waker of this loop.
    pub fn waker(&self) -> Arc<Waker> {
        self.event_loop.borrow().waker.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::coarsen_time;

    #[test]
    fn time_is_coarsened_down_to_100_microseconds() {
        assert_eq!(coarsen_time(0.0), 0.0);
        assert_eq!(coarsen_time(0.099_999), 0.0);
        assert_eq!(coarsen_time(0.1), 0.1);
        assert_eq!(coarsen_time(12.345_678), 12.3);
        assert_eq!(coarsen_time(12.399_999), 12.3);
        // (before the time origin, down means further from it)
        assert_eq!(coarsen_time(-0.05), -0.1);
        // The previous 5 µs steps are gone.
        assert_eq!(coarsen_time(1.005), 1.0);
    }
}
