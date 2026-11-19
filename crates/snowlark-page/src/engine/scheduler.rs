// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Prioritized Task Scheduling (wicg.github.io/scheduling-apis): `scheduler.postTask()`,
//! `scheduler.yield()`, `TaskController`, `TaskSignal` and `TaskPriorityChangeEvent`, and
//! `navigator.scheduling.isInputPending()`.
//!
//! Scheduler tasks wait in task queues keyed by priority (or by the TaskSignal whose priority they
//! follow) and whether they are continuations; the event loop picks among them by effective priority
//! (§2.4), then age. Against the other task sources the spec leaves the order to the user agent; as
//! its notes suggest, effective priorities above "user-visible" run before other tasks,
//! "user-visible" tasks interleave with them by age (like timers), and "background" ones run only
//! when nothing else is queued. A task's scheduling state (its abort and priority sources) is the
//! V8 continuation-preserved embedder data while its callback runs, so `yield()` after an `await`
//! inherits it.

use snowlark_bindings::generated::{
    InterfaceId, IsInputPendingOptions, SchedulerImpl, SchedulerPostTaskCallback,
    SchedulerPostTaskOptions, SchedulingImpl, TaskControllerImpl, TaskControllerInit, TaskPriority,
    TaskPriorityChangeEventImpl, TaskPriorityChangeEventInit, TaskPriorityOrTaskSignal,
    TaskSignalAnyInit, TaskSignalImpl,
};
use snowlark_bindings::rt::{self, Cx, Error, ExceptionName, Handle, JsObject, JsPromise, JsValue};

use super::events::EventExt;
use super::{exception, Engine, Object};
use crate::event_loop::{Task, TaskFn, TaskSource, TimerHandler};

/// A TaskSignal's own slots (on its AbortSignal).
pub struct TaskSignalData {
    pub priority: TaskPriority,
    pub priority_changing: bool,
    /// The signal whose priority this dependent one follows (none: a fixed priority, when
    /// dependent).
    pub source: Option<Handle>,
    /// Dependent signals following this one's priority.
    pub dependents: Vec<Handle>,
}

/// Where a scheduler task's priority comes from.
#[derive(Clone, Copy, PartialEq)]
enum PrioritySource {
    Fixed(TaskPriority),
    /// A TaskSignal without fixed priority (its current priority).
    Signal(Handle),
}

/// A scheduler task queue's key: static queues by priority, dynamic ones by signal.
#[derive(Clone, Copy, PartialEq)]
struct QueueKey {
    source: PrioritySource,
    continuation: bool,
}

struct SchedulerTask {
    id: u64,
    /// The scheduler's global: its tasks are runnable while its document is fully active.
    global: Handle,
    queue: QueueKey,
    /// The event loop's enqueue order.
    order: u64,
    steps: TaskFn,
}

/// The scheduler tasks of an event loop and the promises their handles settle.
#[derive(Default)]
pub struct SchedulerState {
    tasks: Vec<SchedulerTask>,
    /// Task handles not yet run or aborted: their result promise.
    handles: rustc_hash::FxHashMap<u64, JsPromise>,
    next_id: u64,
}

fn rank(p: TaskPriority) -> u8 {
    match p {
        TaskPriority::Background => 0,
        TaskPriority::UserVisible => 1,
        TaskPriority::UserBlocking => 2,
    }
}

fn priority_name(p: TaskPriority) -> &'static str {
    match p {
        TaskPriority::Background => "background",
        TaskPriority::UserVisible => "user-visible",
        TaskPriority::UserBlocking => "user-blocking",
    }
}

fn priority_from(name: &str) -> Option<TaskPriority> {
    Some(match name {
        "background" => TaskPriority::Background,
        "user-visible" => TaskPriority::UserVisible,
        "user-blocking" => TaskPriority::UserBlocking,
        _ => return None,
    })
}

/// Effective priority 2 ("user-visible" task): the level of the other task sources.
const USER_VISIBLE_TASK: u8 = 2;

impl Engine {
    fn task_signal<R>(&self, s: Handle, f: impl FnOnce(&TaskSignalData) -> R) -> Option<R> {
        match &*self.obj(s) {
            Object::AbortSignal(sig) => sig.task.as_deref().map(f),
            _ => None,
        }
    }

    pub(crate) fn is_task_signal(&self, s: Handle) -> bool {
        self.task_signal(s, |_| ()).is_some()
    }

    /// "has fixed priority": a dependent TaskSignal with no source signal.
    fn has_fixed_priority(&self, s: Handle) -> bool {
        let dependent = matches!(&*self.obj(s), Object::AbortSignal(sig) if sig.dependent);
        dependent && self.task_signal(s, |t| t.source.is_none()).unwrap_or(true)
    }

    fn signal_priority(&self, s: Handle) -> TaskPriority {
        self.task_signal(s, |t| t.priority)
            .unwrap_or(TaskPriority::UserVisible)
    }

    fn source_priority(&self, source: PrioritySource) -> TaskPriority {
        match source {
            PrioritySource::Fixed(p) => p,
            PrioritySource::Signal(s) => self.signal_priority(s),
        }
    }

    /// §2.4 a scheduler task queue's effective priority.
    fn effective_priority(&self, key: QueueKey) -> u8 {
        rank(self.source_priority(key.source)) * 2 + key.continuation as u8
    }

    /// The scheduler queue to run next: its effective priority and its oldest task's order (§2.4
    /// "select the next scheduler task queue from all schedulers").
    fn best_scheduler_queue(&self) -> Option<(u8, u64, QueueKey)> {
        let st = self.scheduler.borrow();
        let mut best: Option<(u8, u64, QueueKey)> = None;
        for t in st
            .tasks
            .iter()
            .filter(|t| !self.context_discarded(t.global))
        {
            let p = self.effective_priority(t.queue);
            let better = match best {
                None => true,
                Some((bp, bo, _)) => p > bp || (p == bp && t.order < bo),
            };
            if better {
                best = Some((p, t.order, t.queue));
            }
        }
        best
    }

    /// Whether a scheduler task is queued (and runnable: the tasks of a discarded global are
    /// dropped here).
    pub(crate) fn scheduler_task_ready(&self) -> bool {
        let dead: Vec<u64> = {
            let st = self.scheduler.borrow();
            if st.tasks.is_empty() {
                return false;
            }
            st.tasks
                .iter()
                .filter(|t| self.context_discarded(t.global))
                .map(|t| t.id)
                .collect()
        };
        if !dead.is_empty() {
            let mut st = self.scheduler.borrow_mut();
            st.tasks.retain(|t| !dead.contains(&t.id));
            for id in dead {
                st.handles.remove(&id);
            }
        }
        !self.scheduler.borrow().tasks.is_empty()
    }

    /// The global a scheduler belongs to.
    fn scheduler_global(&self, scheduler: Handle) -> Handle {
        let f = self.frames.borrow();
        f.singletons
            .iter()
            .find_map(|((global, key), h)| {
                (*h == scheduler && *key == "scheduler").then_some(*global)
            })
            .unwrap_or(scheduler)
    }

    /// While an idle callback of `window` runs, `yield()` continues at background priority
    /// (§2.5: an idle callback's scheduling state).
    pub(crate) fn set_idle_scheduling_state(&self, cx: &mut Cx, window: Option<Handle>) {
        let state = window.and_then(|w| {
            let scheduler = self.window_singleton(w, "scheduler", || Object::Scheduler);
            self.scheduling_state_value(
                cx,
                scheduler,
                None,
                PrioritySource::Fixed(TaskPriority::Background),
            )
        });
        let scope = cx.scope();
        let v: v8::Local<v8::Value> = match state {
            Some(s) => v8::Local::new(scope, s.global()),
            None => v8::undefined(scope).into(),
        };
        scope.set_continuation_preserved_embedder_data(v);
    }

    /// The event loop's choice between the oldest other task (with its enqueue order) and the
    /// best scheduler queue: the scheduler task to run instead, if any.
    pub(crate) fn take_scheduler_task(&self, other: Option<u64>) -> Option<Task> {
        let (priority, order, key) = self.best_scheduler_queue()?;
        let run = match priority {
            p if p > USER_VISIBLE_TASK => true,
            USER_VISIBLE_TASK => other.is_none_or(|o| order < o),
            _ => other.is_none(),
        };
        if !run {
            return None;
        }
        let mut st = self.scheduler.borrow_mut();
        let i = st
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.queue == key)
            .min_by_key(|(_, t)| t.order)
            .map(|(i, _)| i)?;
        let t = st.tasks.remove(i);
        Some(Task {
            source: TaskSource::Scheduler,
            steps: t.steps,
            seq: t.order,
        })
    }

    /// "schedule a task to invoke an algorithm" in the queue `key`.
    fn enqueue_scheduler_task(&self, id: u64, global: Handle, key: QueueKey, steps: TaskFn) {
        let order = self.next_task_order();
        self.scheduler.borrow_mut().tasks.push(SchedulerTask {
            id,
            global,
            queue: key,
            order,
            steps,
        });
    }

    /// A new task handle for `promise`, aborted with `signal` (§2.3 "create a task handle").
    fn new_task_handle(&self, promise: &JsPromise, signal: Option<Handle>) -> u64 {
        let id = {
            let mut st = self.scheduler.borrow_mut();
            st.next_id += 1;
            let id = st.next_id;
            st.handles.insert(id, promise.clone());
            id
        };
        if let Some(s) = signal {
            if let Object::AbortSignal(sig) = &mut *self.obj_mut(s) {
                sig.scheduled.push(id);
            }
        }
        id
    }

    /// A task handle's abort steps (from the signal's abort algorithms): reject its promise with
    /// the reason and drop its queued task.
    pub(crate) fn abort_scheduler_tasks(
        &self,
        cx: &mut Cx,
        ids: Vec<u64>,
        reason: Option<JsValue>,
    ) {
        for id in ids {
            let promise = {
                let mut st = self.scheduler.borrow_mut();
                st.tasks.retain(|t| t.id != id);
                st.handles.remove(&id)
            };
            if let (Some(p), Some(r)) = (promise, &reason) {
                p.reject_value(cx, r);
            }
        }
    }

    /// The "task complete steps": the handle leaves its signal.
    fn complete_task_handle(&self, id: u64, signal: Option<Handle>) -> Option<JsPromise> {
        if let Some(s) = signal {
            if let Object::AbortSignal(sig) = &mut *self.obj_mut(s) {
                sig.scheduled.retain(|&i| i != id);
            }
        }
        self.scheduler.borrow_mut().handles.remove(&id)
    }

    /// The scheduling state as a value V8 carries across promise continuations: an object
    /// holding the scheduler, the abort source and the priority source.
    fn scheduling_state_value(
        &self,
        cx: &mut Cx,
        scheduler: Handle,
        abort: Option<Handle>,
        priority: PrioritySource,
    ) -> Option<JsValue> {
        let scope = cx.scope();
        let o = v8::Object::new(scope);
        let set = |scope: &v8::PinScope, key: &str, v: v8::Local<v8::Value>| {
            let k = rt::name(scope, key);
            o.set(scope, k.into(), v);
        };
        let w = rt::wrap(scope, scheduler)?;
        set(scope, "scheduler", w.into());
        let a: v8::Local<v8::Value> = match abort.and_then(|s| rt::wrap(scope, s)) {
            Some(w) => w.into(),
            None => v8::null(scope).into(),
        };
        set(scope, "abort", a);
        let p: v8::Local<v8::Value> = match priority {
            PrioritySource::Fixed(p) => v8::String::new(scope, priority_name(p))?.into(),
            PrioritySource::Signal(s) => rt::wrap(scope, s)?.into(),
        };
        set(scope, "priority", p);
        let v: v8::Local<v8::Value> = o.into();
        Some(JsValue::new(scope, v))
    }

    /// The inherited scheduling state for `scheduler` (§2.5 step 2): (abort source, priority
    /// source), from the continuation data when it was set for this scheduler.
    fn inherited_scheduling_state(
        &self,
        cx: &mut Cx,
        scheduler: Handle,
    ) -> Option<(Option<Handle>, PrioritySource)> {
        let scope = cx.scope();
        let data = scope.get_continuation_preserved_embedder_data();
        let o = v8::Local::<v8::Object>::try_from(data).ok()?;
        let get = |key: &str| {
            let k = rt::name(scope, key);
            o.get(scope, k.into())
        };
        let s = get("scheduler")?;
        let s = v8::Local::<v8::Object>::try_from(s).ok()?;
        if rt::unwrap(&s, 1, u16::MAX)? != scheduler {
            return None;
        }
        let abort = get("abort")
            .and_then(|a| v8::Local::<v8::Object>::try_from(a).ok())
            .and_then(|a| rt::unwrap(&a, 1, u16::MAX));
        let p = get("priority")?;
        let priority = if p.is_string() {
            PrioritySource::Fixed(priority_from(&p.to_rust_string_lossy(scope))?)
        } else {
            let o = v8::Local::<v8::Object>::try_from(p).ok()?;
            PrioritySource::Signal(rt::unwrap(&o, 1, u16::MAX)?)
        };
        Some((abort, priority))
    }

    /// Invokes a postTask callback with `state` as the current scheduling state: Ok(its return
    /// value) or Err(the exception it threw).
    fn invoke_scheduled_callback(
        &self,
        cx: &mut Cx,
        callback: &JsObject,
        state: &JsValue,
    ) -> Result<v8::Global<v8::Value>, Option<v8::Global<v8::Value>>> {
        self.enter_script();
        let realm = {
            let scope = cx.scope();
            let func = v8::Local::new(scope, callback.global());
            func.get_creation_context(scope)
                .map(|c| v8::Global::new(scope, c))
        };
        let r = self.in_realm(cx, realm, |cx| {
            let scope = cx.scope();
            let s = v8::Local::new(scope, state.global());
            scope.set_continuation_preserved_embedder_data(s);
            v8::tc_scope!(let tc, scope);
            let func = v8::Local::new(tc, callback.global());
            let r = match v8::Local::<v8::Function>::try_from(func) {
                Err(_) => Ok(v8::Global::new(
                    tc,
                    v8::Local::<v8::Value>::from(v8::undefined(tc)),
                )),
                Ok(func) => {
                    let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
                    match func.call(tc, recv, &[]) {
                        Some(v) => Ok(v8::Global::new(tc, v)),
                        None => {
                            let e = tc.exception().map(|e| v8::Global::new(tc, e));
                            tc.reset();
                            Err(e)
                        }
                    }
                }
            };
            let undefined: v8::Local<v8::Value> = v8::undefined(tc).into();
            tc.set_continuation_preserved_embedder_data(undefined);
            r
        });
        self.leave_script(cx);
        r
    }

    /// §2.5 "schedule a postTask task".
    fn schedule_post_task(
        &self,
        cx: &mut Cx,
        scheduler: Handle,
        callback: SchedulerPostTaskCallback,
        options: SchedulerPostTaskOptions,
    ) -> rt::Result<JsPromise> {
        let result = JsPromise::new(cx).ok_or(Error::Pending)?;
        let signal = options.signal;
        if let Some(s) = signal.filter(|&s| self.signal_aborted(s)) {
            if let Some(reason) = self.signal_reason(cx, s) {
                result.reject_value(cx, &reason);
            }
            return Ok(result);
        }
        let priority = match (options.priority, signal) {
            (Some(p), _) => PrioritySource::Fixed(p),
            (None, Some(s)) if self.is_task_signal(s) => {
                if self.has_fixed_priority(s) {
                    PrioritySource::Fixed(self.signal_priority(s))
                } else {
                    PrioritySource::Signal(s)
                }
            }
            _ => PrioritySource::Fixed(TaskPriority::UserVisible),
        };
        let state = self
            .scheduling_state_value(cx, scheduler, signal, priority)
            .ok_or(Error::Pending)?;
        let id = self.new_task_handle(&result, signal);
        let key = QueueKey {
            source: priority,
            continuation: false,
        };
        let global = self.scheduler_global(scheduler);
        let steps: TaskFn = Box::new(move |e: &Engine, cx: &mut Cx| {
            if !e.scheduler.borrow().handles.contains_key(&id) {
                return;
            }
            let outcome = e.invoke_scheduled_callback(cx, &callback.0, &state);
            // (an abort while the callback ran rejected the promise already)
            let Some(promise) = e.complete_task_handle(id, signal) else {
                return;
            };
            match outcome {
                Ok(v) => {
                    let scope = cx.scope();
                    let v = v8::Local::new(scope, v);
                    let v = JsValue::new(scope, v);
                    promise.resolve(cx, v);
                }
                Err(Some(ex)) => {
                    let scope = cx.scope();
                    let ex = v8::Local::new(scope, ex);
                    let ex = JsValue::new(scope, ex);
                    promise.reject_value(cx, &ex);
                }
                Err(None) => {}
            }
        });
        if options.delay > 0 {
            // (after the delay, unless the handle was aborted meanwhile)
            self.start_timer(
                TimerHandler::Internal(Some(Box::new(move |e, _| {
                    if e.scheduler.borrow().handles.contains_key(&id) {
                        e.enqueue_scheduler_task(id, global, key, steps);
                    }
                }))),
                options.delay as f64,
                false,
                None,
            );
        } else {
            self.enqueue_scheduler_task(id, global, key, steps);
        }
        Ok(result)
    }

    /// §2.5 "schedule a yield continuation".
    fn schedule_yield_continuation(&self, cx: &mut Cx, scheduler: Handle) -> rt::Result<JsPromise> {
        let result = JsPromise::new(cx).ok_or(Error::Pending)?;
        let (abort, priority) = self
            .inherited_scheduling_state(cx, scheduler)
            .map(|(a, p)| (a, Some(p)))
            .unwrap_or((None, None));
        if let Some(s) = abort.filter(|&s| self.signal_aborted(s)) {
            if let Some(reason) = self.signal_reason(cx, s) {
                result.reject_value(cx, &reason);
            }
            return Ok(result);
        }
        let priority = priority.unwrap_or(PrioritySource::Fixed(TaskPriority::UserVisible));
        let id = self.new_task_handle(&result, abort);
        let key = QueueKey {
            source: priority,
            continuation: true,
        };
        let global = self.scheduler_global(scheduler);
        self.enqueue_scheduler_task(
            id,
            global,
            key,
            Box::new(move |e: &Engine, cx: &mut Cx| {
                if let Some(promise) = e.complete_task_handle(id, abort) {
                    promise.resolve(cx, ());
                }
            }),
        );
        Ok(result)
    }

    /// §4 "signal priority change".
    fn signal_priority_change(
        &self,
        cx: &mut Cx,
        signal: Handle,
        priority: TaskPriority,
    ) -> rt::Result<()> {
        let (changing, previous) = self
            .task_signal(signal, |t| (t.priority_changing, t.priority))
            .ok_or(Error::Pending)?;
        if changing {
            return Err(exception(
                ExceptionName::NotAllowedError,
                "The signal's priority is already changing",
            ));
        }
        if previous == priority {
            return Ok(());
        }
        let dependents = {
            let mut o = self.obj_mut(signal);
            let Object::AbortSignal(sig) = &mut *o else {
                return Ok(());
            };
            let Some(t) = sig.task.as_deref_mut() else {
                return Ok(());
            };
            t.priority_changing = true;
            t.priority = priority;
            t.dependents.clone()
        };
        // (the dynamic queues following this signal read its priority when the loop picks)
        let e = self.trusted_event(
            InterfaceId::TaskPriorityChangeEvent,
            "prioritychange",
            false,
            false,
            EventExt::TaskPriorityChange(previous),
        );
        self.dispatch(cx, e, signal, false);
        for d in dependents {
            let _ = self.signal_priority_change(cx, d, priority);
        }
        if let Object::AbortSignal(sig) = &mut *self.obj_mut(signal) {
            if let Some(t) = sig.task.as_deref_mut() {
                t.priority_changing = false;
            }
        }
        Ok(())
    }

    fn make_task_signal(&self, signal: Handle, data: TaskSignalData) {
        if let Object::AbortSignal(sig) = &mut *self.obj_mut(signal) {
            sig.task = Some(Box::new(data));
        }
    }
}

impl SchedulerImpl for Engine {
    fn post_task(
        &self,
        cx: &mut Cx,
        this: Handle,
        callback: SchedulerPostTaskCallback,
        options: SchedulerPostTaskOptions,
    ) -> rt::Result<JsPromise> {
        self.schedule_post_task(cx, this, callback, options)
    }
    fn yield_(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.schedule_yield_continuation(cx, this)
    }
}

impl SchedulingImpl for Engine {
    /// Always false: no pending input is disclosed (Chrome has retired the signal too).
    fn is_input_pending(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _options: IsInputPendingOptions,
    ) -> rt::Result<bool> {
        Ok(false)
    }
}

impl TaskControllerImpl for Engine {
    /// §4 `new TaskController(init)`: an AbortController whose signal is a TaskSignal.
    fn constructor(&self, _cx: &mut Cx, init: TaskControllerInit) -> rt::Result<Handle> {
        let signal = self.new_abort_signal();
        self.make_task_signal(
            signal,
            TaskSignalData {
                priority: init.priority,
                priority_changing: false,
                source: None,
                dependents: Vec::new(),
            },
        );
        Ok(self.alloc(Object::AbortController { signal }))
    }
    fn set_priority(&self, cx: &mut Cx, this: Handle, priority: TaskPriority) -> rt::Result<()> {
        let signal = match &*self.obj(this) {
            Object::AbortController { signal } => *signal,
            _ => return Err(Error::Pending),
        };
        self.signal_priority_change(cx, signal, priority)
    }
}

impl TaskSignalImpl for Engine {
    fn priority(&self, this: Handle) -> TaskPriority {
        self.signal_priority(this)
    }
    /// §4 "create a dependent task signal".
    fn static_any(
        &self,
        cx: &mut Cx,
        signals: Vec<Handle>,
        init: TaskSignalAnyInit,
    ) -> rt::Result<Handle> {
        let result = snowlark_bindings::generated::AbortSignalImpl::static_any(self, cx, signals)?;
        if let Object::AbortSignal(sig) = &mut *self.obj_mut(result) {
            sig.dependent = true;
        }
        let data = match init.priority {
            TaskPriorityOrTaskSignal::TaskPriority(p) => TaskSignalData {
                priority: p,
                priority_changing: false,
                source: None,
                dependents: Vec::new(),
            },
            TaskPriorityOrTaskSignal::TaskSignal(source) => {
                let priority = self.signal_priority(source);
                let mut root = None;
                if !self.has_fixed_priority(source) {
                    let mut s = source;
                    while let Some(next) = self.task_signal(s, |t| t.source).flatten() {
                        s = next;
                    }
                    root = Some(s);
                }
                TaskSignalData {
                    priority,
                    priority_changing: false,
                    source: root,
                    dependents: Vec::new(),
                }
            }
        };
        let root = data.source;
        self.make_task_signal(result, data);
        if let Some(root) = root {
            if let Object::AbortSignal(sig) = &mut *self.obj_mut(root) {
                if let Some(t) = sig.task.as_deref_mut() {
                    t.dependents.push(result);
                }
            }
        }
        Ok(result)
    }
}

impl TaskPriorityChangeEventImpl for Engine {
    fn previous_priority(&self, this: Handle) -> TaskPriority {
        self.ev(this, |e| match e.ext {
            EventExt::TaskPriorityChange(p) => p,
            _ => TaskPriority::UserVisible,
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: TaskPriorityChangeEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::TaskPriorityChangeEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::TaskPriorityChange(init.previous_priority),
        ))
    }
}
