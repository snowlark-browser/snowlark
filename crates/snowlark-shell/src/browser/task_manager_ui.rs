// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of the task manager ([`crate::task_manager`]): opening and closing it, taking
//! the figures every two seconds while it shows, and acting on its presses.
//!
//! Each refresh reads the browser's process and each renderer process from outside
//! (`snowlark_ipc::Usage`: private bytes, processor time) and each tab thread's processor time, as
//! Chrome's `TaskGroupSampler` reads a process's (`GetPlatformIndependentCPUUsage`: the processor
//! time used since the last refresh over the time between them, in percent of one processor), and
//! asks every loaded tab for [`TabCommand::Memory`]. The answers come later (the window never waits
//! for a tab): a tab on a thread gets its memory from it, and every tab its network rate (the
//! growth of the bytes it has received between two answers).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use snowlark_paint::window::Window;
use snowlark_paint::Point;

use super::{Browser, TIMER_TASKS};
use crate::tab::{MemoryUse, TabCommand, TabId};
use crate::task_manager::{Task, TaskAction, TaskKey, TaskManager, REFRESH_MS};

/// What the last refresh and answers measured, to make rates of the next.
#[derive(Default)]
pub(super) struct TaskSampler {
    /// Processor time so far, by task, and when it was read.
    cpu: HashMap<TaskKey, (Instant, Duration)>,
    /// Bytes received so far, by tab, and when the tab said so.
    received: HashMap<TabId, (Instant, u64)>,
    /// The last memory a tab on a thread reported.
    memory: HashMap<TabId, u64>,
    /// The last network rate of each tab.
    network: HashMap<TabId, f64>,
}

impl TaskSampler {
    /// The percent of one processor `key` used since its last reading (`None` the first time).
    fn cpu(&mut self, key: TaskKey, now: Instant, total: Option<Duration>) -> Option<f64> {
        let total = total?;
        let last = self.cpu.insert(key, (now, total));
        let (then, before) = last?;
        let wall = now.duration_since(then).as_secs_f64();
        (wall > 0.0).then(|| total.saturating_sub(before).as_secs_f64() / wall * 100.0)
    }
}

impl Browser {
    /// Shift+Esc and the menu: opens the task manager, or closes it.
    pub(super) fn toggle_task_manager(&mut self, window: &Window) {
        if self.task_manager.take().is_some() {
            window.kill_timer(TIMER_TASKS);
            self.overlay_dirty = true;
            return;
        }
        self.close_popup();
        let mut m = TaskManager::new(self.content_dips());
        // (the active tab's row is selected, as Chrome's `TaskManagerView::Show` does)
        m.selected = self.active_id().map(TaskKey::Tab);
        self.task_manager = Some((m, TaskSampler::default()));
        self.refresh_task_manager(window);
    }

    fn close_task_manager(&mut self, window: &Window) {
        if self.task_manager.take().is_some() {
            window.kill_timer(TIMER_TASKS);
            self.overlay_dirty = true;
        }
    }

    /// Takes the figures again, and asks the tabs for theirs.
    pub(super) fn refresh_task_manager(&mut self, window: &Window) {
        let Some((_, sampler)) = &mut self.task_manager else {
            window.kill_timer(TIMER_TASKS);
            return;
        };
        let now = Instant::now();
        let browser_pid = std::process::id();
        let own = snowlark_ipc::own_usage();
        let mut tasks = vec![Task {
            key: TaskKey::Browser,
            title: "Browser".into(),
            icon: None,
            memory: own.map(|u| u.private_bytes),
            cpu: sampler.cpu(TaskKey::Browser, now, own.map(|u| u.cpu)),
            // (the browser's own fetches are not counted)
            network: None,
            pid: browser_pid,
            killable: false,
        }];
        // The GPU process (Chrome lists it under the browser, as "GPU Process").
        if let Some(p) = self.gpu_process.as_ref().and_then(|h| h.process()) {
            let usage = p.usage();
            tasks.push(Task {
                key: TaskKey::Gpu,
                title: "GPU Process".into(),
                icon: None,
                memory: usage.map(|u| u.private_bytes),
                cpu: sampler.cpu(TaskKey::Gpu, now, usage.map(|u| u.cpu)),
                network: None,
                pid: p.pid(),
                killable: true,
            });
        }
        // (every window's tabs: a popup window's too)
        let others = super::popup_window::tabs_of(&self.popups);
        for e in self.tabs.iter().chain(others) {
            // (a crashed tab has no page, a restored one none yet: Chrome lists neither)
            if e.crashed.is_some() || e.lazy.is_some() {
                continue;
            }
            let id = e.tab.id;
            let key = TaskKey::Tab(id);
            let process = e.tab.renderer_process();
            let usage = process.and_then(|p| p.usage());
            let (memory, cpu, pid) = match process {
                Some(p) => (
                    usage.map(|u| u.private_bytes),
                    usage.map(|u| u.cpu),
                    p.pid(),
                ),
                None => (
                    sampler.memory.get(&id).copied(),
                    e.tab.thread_cpu(),
                    browser_pid,
                ),
            };
            let state = &e.tab.state;
            let name = if state.title.is_empty() {
                state.url.as_str()
            } else {
                state.title.as_str()
            };
            tasks.push(Task {
                key,
                title: format!("Tab: {name}"),
                icon: e.favicon,
                memory,
                cpu: sampler.cpu(key, now, cpu),
                network: sampler.network.get(&id).copied(),
                pid,
                killable: process.is_some(),
            });
            e.tab.send(TabCommand::Memory);
        }
        // (forget what tabs that went away measured)
        let live: Vec<TaskKey> = tasks.iter().map(|t| t.key).collect();
        sampler.cpu.retain(|k, _| live.contains(k));
        sampler
            .received
            .retain(|id, _| live.contains(&TaskKey::Tab(*id)));
        sampler
            .memory
            .retain(|id, _| live.contains(&TaskKey::Tab(*id)));
        sampler
            .network
            .retain(|id, _| live.contains(&TaskKey::Tab(*id)));
        if let Some((m, _)) = &mut self.task_manager {
            m.set_tasks(tasks);
        }
        self.overlay_dirty = true;
        window.set_timer(TIMER_TASKS, REFRESH_MS);
    }

    /// A tab answered [`TabCommand::Memory`]: its memory (a tab on a thread) and network rate.
    pub(super) fn task_manager_memory(&mut self, id: TabId, m: &MemoryUse) {
        let on_thread = self
            .tabs
            .iter()
            .any(|e| e.tab.id == id && e.tab.renderer_process().is_none());
        let Some((manager, sampler)) = &mut self.task_manager else {
            return;
        };
        let now = Instant::now();
        if let Some((then, before)) = sampler.received.insert(id, (now, m.received)) {
            let wall = now.duration_since(then).as_secs_f64();
            if wall > 0.0 {
                let rate = m.received.saturating_sub(before) as f64 / wall;
                sampler.network.insert(id, rate);
            }
        }
        if on_thread {
            sampler.memory.insert(id, m.total());
        }
        let network = sampler.network.get(&id).copied();
        if let Some(t) = manager.task_mut(TaskKey::Tab(id)) {
            if on_thread {
                t.memory = Some(m.total());
            }
            t.network = network;
            self.overlay_dirty = true;
        }
    }

    /// The pointer moved; whether the task manager is under it.
    pub(super) fn task_manager_move(&mut self, cp: Point) -> bool {
        let Some((m, _)) = &mut self.task_manager else {
            return false;
        };
        let h = m.hit(cp, &self.fonts);
        if h != m.hover {
            m.hover = h;
            self.overlay_dirty = true;
        }
        m.rect.contains(cp)
    }

    /// A press; whether the task manager took it (one outside it goes on to the page).
    pub(super) fn task_manager_press(&mut self, window: &Window, cp: Point, clicks: u32) -> bool {
        let Some((m, _)) = &mut self.task_manager else {
            return false;
        };
        if !m.rect.contains(cp) {
            return false;
        }
        let action = m.press(cp, clicks, &self.fonts);
        self.overlay_dirty = true;
        self.swallow_release = true;
        match action {
            TaskAction::None => {}
            TaskAction::Close => self.close_task_manager(window),
            TaskAction::EndTask(TaskKey::Gpu) => {
                // (another is started at once: `GpuHost`)
                if let Some(h) = &self.gpu_process {
                    h.kill();
                }
            }
            TaskAction::EndTask(TaskKey::Browser) => {}
            TaskAction::EndTask(TaskKey::Tab(id)) => {
                // (the renderer's end is the tab's crash: the window shows its sad page)
                let others = super::popup_window::tabs_of(&self.popups);
                if let Some(e) = self.tabs.iter().chain(others).find(|e| e.tab.id == id) {
                    if let Some(p) = e.tab.renderer_process() {
                        p.kill();
                    }
                }
            }
            TaskAction::Activate(id) => {
                if let Some(i) = self.tabs.position(|e| e.tab.id == id) {
                    self.tabs.select(i);
                    self.activated();
                    self.chrome_dirty = true;
                }
            }
        }
        true
    }

    /// A wheel turn; whether the task manager took it.
    pub(super) fn task_manager_wheel(&mut self, cp: Point, dy: i32) -> bool {
        let Some((m, _)) = &mut self.task_manager else {
            return false;
        };
        if !m.rect.contains(cp) {
            return false;
        }
        let rows = -(dy as f32 / 120.0 * 3.0).round() as i32;
        if m.wheel(rows) {
            self.overlay_dirty = true;
        }
        true
    }
}
