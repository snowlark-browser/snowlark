// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! V8's foreground tasks, run by the event loop of the thread that owns each isolate.
//!
//! V8 hands an isolate's thread work from its own background threads as foreground tasks: an
//! asynchronous `WebAssembly.compile` or `instantiate` finishing (which resolves its promise), the
//! garbage collector's jobs, a `FinalizationRegistry`'s cleanup, `Atomics.waitAsync` timeouts. The
//! platform gives every such task to the isolate's [`TaskQueue`], which wakes that thread's event
//! loop when one arrives, so the task runs as soon as the loop is free rather than when it next
//! wakes for something else (a timer, a frame). Delayed tasks (the garbage collector's
//! housekeeping) wait in the queue and run when the loop turns after they are due.
//!
//! An isolate whose thread runs no event loop (a script check, a test) has no queue: its tasks are
//! dropped as they are posted, unrun, as V8's own platform does for an isolate shutting down.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// The queue of every isolate a loop runs, by the address of its V8 isolate. Shared with V8's
/// background threads, which post to it, so it is process-wide like the platform itself.
fn queues() -> MutexGuard<'static, HashMap<usize, Arc<TaskQueue>>> {
    static QUEUES: OnceLock<Mutex<HashMap<usize, Arc<TaskQueue>>>> = OnceLock::new();
    QUEUES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The address V8 names an isolate by when it posts to it.
fn key(isolate: &v8::Isolate) -> usize {
    // SAFETY: the pointer is only read as a number, never dereferenced. `UnsafeRawIsolatePtr` is a
    // `repr(transparent)` wrapper of the `v8::Isolate*` that the platform's task runners pass.
    let raw: *mut std::ffi::c_void = unsafe { std::mem::transmute(isolate.as_raw_isolate_ptr()) };
    raw as usize
}

/// The platform V8 is initialized with: the default one (its worker threads run background
/// jobs), with every foreground task given to the posting isolate's queue.
pub(crate) fn new_platform() -> v8::UniqueRef<v8::Platform> {
    v8::new_custom_platform(0, false, false, Tasks)
}

struct Tasks;

impl Tasks {
    fn post(&self, isolate: *mut std::ffi::c_void, task: v8::Task, delay: f64, nestable: bool) {
        let queue = queues().get(&(isolate as usize)).cloned();
        match queue {
            Some(queue) => queue.push(task, delay, nestable),
            // (no loop runs this isolate: the task is destroyed unrun)
            None => drop(task),
        }
    }
}

impl v8::PlatformImpl for Tasks {
    fn post_task(&self, isolate: *mut std::ffi::c_void, task: v8::Task) {
        self.post(isolate, task, 0.0, true);
    }

    fn post_non_nestable_task(&self, isolate: *mut std::ffi::c_void, task: v8::Task) {
        self.post(isolate, task, 0.0, false);
    }

    fn post_delayed_task(&self, isolate: *mut std::ffi::c_void, task: v8::Task, delay: f64) {
        self.post(isolate, task, delay, true);
    }

    fn post_non_nestable_delayed_task(
        &self,
        isolate: *mut std::ffi::c_void,
        task: v8::Task,
        delay: f64,
    ) {
        self.post(isolate, task, delay, false);
    }

    /// (the platform is made without idle task support, so V8 posts none)
    fn post_idle_task(&self, _isolate: *mut std::ffi::c_void, task: v8::IdleTask) {
        drop(task);
    }
}

struct Pending {
    task: v8::Task,
    /// Whether the task may run inside a nested event loop (a loop run while script runs).
    nestable: bool,
}

#[derive(Default)]
struct State {
    /// Closed when the isolate's loop ends: tasks posted after that are dropped.
    closed: bool,
    ready: VecDeque<Pending>,
    /// Tasks not due yet, by when they are, in posting order.
    delayed: Vec<(Instant, Pending)>,
}

/// One isolate's foreground tasks, which its thread's event loop runs ([`TaskQueue::run`]).
pub struct TaskQueue {
    state: Mutex<State>,
    /// Wakes the isolate's event loop (from any thread).
    wake: Box<dyn Fn() + Send + Sync>,
}

impl TaskQueue {
    /// Gives `isolate`'s foreground tasks to a new queue, which calls `wake` when a task arrives that
    /// the loop does not know of yet. Call [`TaskQueue::close`] before the isolate is disposed.
    pub fn register(isolate: &v8::Isolate, wake: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        let queue = Arc::new(TaskQueue {
            state: Mutex::default(),
            wake: Box::new(wake),
        });
        queues().insert(key(isolate), queue.clone());
        queue
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, task: v8::Task, delay: f64, nestable: bool) {
        let pending = Pending { task, nestable };
        let mut st = self.state();
        if st.closed {
            drop(st);
            drop(pending);
            return;
        }
        // (a wake per task the loop has not seen: once tasks are waiting it will run them all; a
        // delayed task is housekeeping, a garbage collector's job, which runs when the loop next
        // turns after it is due and wakes nothing)
        let wake = if delay > 0.0 {
            let due = Instant::now() + Duration::from_secs_f64(delay);
            st.delayed.push((due, pending));
            false
        } else {
            st.ready.push_back(pending);
            st.ready.len() == 1
        };
        drop(st);
        if wake {
            (self.wake)();
        }
    }

    /// Runs the tasks that are ready (and the delayed ones now due), those that V8 posts meanwhile
    /// too, until none is left. In a nested loop (`nested`: script is running below it) the tasks
    /// that must not run there wait. Returns whether any ran. Call it on the isolate's thread, with
    /// the isolate entered.
    pub fn run(&self, nested: bool) -> bool {
        let mut ran = false;
        loop {
            let next = {
                let mut st = self.state();
                let now = Instant::now();
                if st.delayed.iter().any(|(at, _)| *at <= now) {
                    let (due, later) = std::mem::take(&mut st.delayed)
                        .into_iter()
                        .partition::<Vec<_>, _>(|(at, _)| *at <= now);
                    st.delayed = later;
                    let mut due = due;
                    due.sort_by_key(|(at, _)| *at);
                    st.ready.extend(due.into_iter().map(|(_, p)| p));
                }
                let at = st.ready.iter().position(|p| p.nestable || !nested);
                at.and_then(|i| st.ready.remove(i))
            };
            match next {
                // (outside the lock: the task may post others)
                Some(p) => {
                    p.task.run();
                    ran = true;
                }
                None => return ran,
            }
        }
    }

    /// Whether a task is ready to run.
    pub fn has_ready(&self) -> bool {
        let st = self.state();
        let now = Instant::now();
        !st.ready.is_empty() || st.delayed.iter().any(|(at, _)| *at <= now)
    }

    /// Ends the queue as its loop ends, while the isolate is alive: the tasks waiting are destroyed
    /// unrun, as are those posted from now on.
    pub fn close(&self, isolate: &v8::Isolate) {
        {
            let mut q = queues();
            let k = key(isolate);
            if q.get(&k).is_some_and(|q| std::ptr::eq(q.as_ref(), self)) {
                q.remove(&k);
            }
        }
        let (ready, delayed) = {
            let mut st = self.state();
            st.closed = true;
            (
                std::mem::take(&mut st.ready),
                std::mem::take(&mut st.delayed),
            )
        };
        // (outside the lock: a task's destructor reaches into V8)
        drop(ready);
        drop(delayed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A `WebAssembly.compile` of a module resolves through a foreground task that V8 posts from a
    /// background thread: the queue wakes its loop for it, and running the queue resolves the
    /// promise.
    #[test]
    fn async_wasm_compile_wakes_the_loop_and_resolves() {
        crate::init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        let wakes = Arc::new(AtomicUsize::new(0));
        let woken = wakes.clone();
        let queue = TaskQueue::register(&isolate, move || {
            woken.fetch_add(1, Ordering::SeqCst);
        });
        {
            v8::scope!(let scope, &mut isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let source = v8::String::new(
                scope,
                "globalThis.done = false;\
                 WebAssembly.compile(new Uint8Array([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0]))\
                   .then(() => { globalThis.done = true; });",
            )
            .unwrap();
            let script = v8::Script::compile(scope, source, None).unwrap();
            script.run(scope).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let done = v8::String::new(scope, "done").unwrap();
            loop {
                if wakes.load(Ordering::SeqCst) > 0 {
                    queue.run(false);
                    scope.perform_microtask_checkpoint();
                }
                let global = context.global(scope);
                if global.get(scope, done.into()).unwrap().is_true() {
                    break;
                }
                assert!(Instant::now() < deadline, "the compilation never resolved");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(wakes.load(Ordering::SeqCst) > 0, "the loop was woken");
        queue.close(&isolate);
    }

    /// An isolate without a queue has its tasks dropped, and a closed queue takes none.
    #[test]
    fn tasks_of_a_closed_queue_are_dropped() {
        crate::init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let queue = TaskQueue::register(&isolate, || {});
        queue.close(&isolate);
        {
            v8::scope!(let scope, &mut isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let source = v8::String::new(
                scope,
                "WebAssembly.compile(new Uint8Array([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0]))",
            )
            .unwrap();
            let script = v8::Script::compile(scope, source, None).unwrap();
            script.run(scope).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        assert!(!queue.has_ready());
        assert!(!queue.run(false));
    }
}
