// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The task manager (Shift+Esc, or the menu's "Task manager"), after Chrome's
//! (`chrome/browser/ui/views/task_manager_view.cc`, `chrome/browser/ui/task_manager/
//! task_manager_table_model.cc`): a table of what runs, the browser and each tab, with its memory,
//! processor use, network rate and process id, refreshed every two seconds (`kRefreshTime`), and
//! End task for a tab in a renderer process of its own. Rows are grouped as Chrome's
//! `TaskManagerImpl::GetTaskIdsList` orders them (the browser first, then the tabs in strip order)
//! until a column header sorts them; the category tabs above filter them ("Tabs", "Browser", "All
//! tasks").
//!
//! Where Snowlark differs: a tab on a thread of the browser's process (the default process model)
//! shows what is attributed to it, its thread's processor time and the memory its own counters
//! report (as the tab card does), with the browser's process id; Chrome shows one process's figures
//! once for all the tasks in it. The Browser row is the whole process, tabs on threads included. A
//! tab's network rate is the body bytes its page takes per second, decoded (Chrome counts the bytes
//! read and sent on the wire). It is drawn in the window's overlay rather than a window of its own,
//! and stays open while the page is used, until its close button or Shift+Esc.

use snowlark_paint::{
    BoxShadow, Color, DisplayListBuilder, ImageId, ImageRendering, Point, Radii, Rect, Size,
};

use crate::chrome::{glyph, Face, Fonts, Theme};
use crate::tab::TabId;
use crate::widgets;

/// How often the figures are taken again (Chrome's `kRefreshTime`).
pub const REFRESH_MS: u32 = 2000;

const WIDTH: f32 = 760.0;
const HEIGHT: f32 = 460.0;
const TITLE: f32 = 48.0;
const CATEGORIES: f32 = 40.0;
const HEADER: f32 = 34.0;
const ROW: f32 = 30.0;
const FOOTER: f32 = 60.0;
const PAD: f32 = 16.0;
const TEXT: f32 = 13.0;
/// The numeric columns' widths, right to left: process id, network, CPU, memory.
const NUMBERS: [(Column, f32); 4] = [
    (Column::Memory, 150.0),
    (Column::Cpu, 70.0),
    (Column::Network, 100.0),
    (Column::Pid, 100.0),
];
/// The browser row's icon (Segoe MDL2 Assets "TVMonitor").
const BROWSER_ICON: char = '\u{E7F4}';
/// What a cell shows when there is no figure (Chrome's `IDS_TASK_MANAGER_NA_CELL_TEXT`).
pub const NOT_AVAILABLE: &str = "\u{2013}";
const END_TASK: &str = "End task";

/// Which task a row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TaskKey {
    Browser,
    /// The GPU process (`--gpu-process`).
    Gpu,
    Tab(TabId),
}

/// A row: a task and its figures (`None`: not known yet, or not measured).
#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub key: TaskKey,
    pub title: String,
    pub icon: Option<ImageId>,
    /// Bytes.
    pub memory: Option<u64>,
    /// Percent of one processor over the last interval.
    pub cpu: Option<f64>,
    /// Bytes a second.
    pub network: Option<f64>,
    pub pid: u32,
    /// Whether End task can end it (a tab in a renderer process of its own).
    pub killable: bool,
}

/// The tabs above the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Tabs,
    Browser,
    All,
}

impl Category {
    const ALL: [Category; 3] = [Category::Tabs, Category::Browser, Category::All];

    fn label(self) -> &'static str {
        match self {
            Category::Tabs => "Tabs",
            Category::Browser => "Browser",
            Category::All => "All tasks",
        }
    }

    fn shows(self, key: TaskKey) -> bool {
        match self {
            Category::Tabs => matches!(key, TaskKey::Tab(_)),
            Category::Browser => !matches!(key, TaskKey::Tab(_)),
            Category::All => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    Task,
    Memory,
    Cpu,
    Network,
    Pid,
}

impl Column {
    fn label(self) -> &'static str {
        match self {
            Column::Task => "Task",
            Column::Memory => "Memory footprint",
            Column::Cpu => "CPU",
            Column::Network => "Network",
            Column::Pid => "Process ID",
        }
    }
}

/// A part of the panel that does something.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TaskHit {
    Close,
    Category(Category),
    Header(Column),
    /// A shown row, by its place in [`TaskManager::rows`].
    Row(usize),
    EndTask,
}

/// What a press asks the window to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskAction {
    None,
    Close,
    /// End the task's process: a tab's renderer, or the GPU process.
    EndTask(TaskKey),
    /// Show the tab (a double click on its row).
    Activate(TabId),
}

pub struct TaskManager {
    pub rect: Rect,
    tasks: Vec<Task>,
    pub category: Category,
    /// The column the rows are sorted by, and whether ascending; `None`: Chrome's grouped order.
    pub sort: Option<(Column, bool)>,
    pub selected: Option<TaskKey>,
    pub hover: Option<TaskHit>,
    /// The first row shown (the table scrolls by rows).
    first: usize,
}

/// `bytes` in KiB, its digits grouped: "65,364K" (Chrome's `GetMemoryUsageText`).
pub fn memory_text(bytes: Option<u64>) -> String {
    match bytes {
        None => NOT_AVAILABLE.into(),
        Some(b) => format!("{}K", grouped(b / 1024)),
    }
}

/// Percent with one decimal: "1.4" (Chrome's `GetCpuUsageText`).
pub fn cpu_text(cpu: Option<f64>) -> String {
    match cpu {
        Some(c) if c.is_finite() => format!("{:.1}", c.max(0.0)),
        _ => NOT_AVAILABLE.into(),
    }
}

/// "0", or a speed in the largest 1024 unit it reaches, with one decimal under 100: "48.5 kB/s"
/// (Chrome's `GetNetworkUsageText`, `ui::FormatSpeed`).
pub fn network_text(rate: Option<f64>) -> String {
    let Some(r) = rate.filter(|r| r.is_finite()) else {
        return NOT_AVAILABLE.into();
    };
    let bytes = r.max(0.0).round();
    if bytes == 0.0 {
        return "0".into();
    }
    const UNITS: [&str; 5] = ["B/s", "kB/s", "MB/s", "GB/s", "TB/s"];
    let mut unit = 0;
    let mut amount = bytes;
    while amount >= 1024.0 && unit + 1 < UNITS.len() {
        amount /= 1024.0;
        unit += 1;
    }
    if unit > 0 && amount < 100.0 {
        format!("{amount:.1} {}", UNITS[unit])
    } else {
        format!("{amount:.0} {}", UNITS[unit])
    }
}

fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl TaskManager {
    /// The panel, centred in the content area `area` (DIPs), showing the tabs.
    pub fn new(area: Size) -> TaskManager {
        let mut m = TaskManager {
            rect: Rect::new(0.0, 0.0, 0.0, 0.0),
            tasks: Vec::new(),
            category: Category::Tabs,
            sort: None,
            selected: None,
            hover: None,
            first: 0,
        };
        m.place(area);
        m
    }

    /// Sizes and places the panel (the window changed size).
    pub fn place(&mut self, area: Size) {
        let w = WIDTH.min(area.width - 16.0).max(360.0);
        let h = HEIGHT
            .min(area.height - 16.0)
            .max(TITLE + CATEGORIES + HEADER + ROW + FOOTER);
        let x = ((area.width - w) / 2.0).max(0.0);
        let y = ((area.height - h) / 3.0).max(8.0);
        self.rect = Rect::new(x, y, w, h);
        self.clamp_scroll();
    }

    /// New figures. The selection stays on its task while the task is there.
    pub fn set_tasks(&mut self, tasks: Vec<Task>) {
        self.tasks = tasks;
        if let Some(s) = self.selected {
            if !self.tasks.iter().any(|t| t.key == s) {
                self.selected = None;
            }
        }
        self.clamp_scroll();
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// The task with `key`, to change its figures in place.
    pub fn task_mut(&mut self, key: TaskKey) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.key == key)
    }

    /// The rows shown: the category's tasks, in the sort order.
    pub fn rows(&self) -> Vec<&Task> {
        let mut rows: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|t| self.category.shows(t.key))
            .collect();
        if let Some((column, ascending)) = self.sort {
            rows.sort_by(|a, b| {
                let o = match column {
                    Column::Task => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
                    Column::Memory => a.memory.cmp(&b.memory),
                    Column::Cpu => a.cpu.unwrap_or(-1.0).total_cmp(&b.cpu.unwrap_or(-1.0)),
                    Column::Network => a
                        .network
                        .unwrap_or(-1.0)
                        .total_cmp(&b.network.unwrap_or(-1.0)),
                    Column::Pid => a.pid.cmp(&b.pid),
                };
                if ascending {
                    o
                } else {
                    o.reverse()
                }
            });
        }
        rows
    }

    /// Whether End task can act: a row is selected, shown, and its task can be ended.
    pub fn can_end_task(&self) -> bool {
        self.selected_task().is_some_and(|t| t.killable)
    }

    fn selected_task(&self) -> Option<&Task> {
        let s = self.selected?;
        self.rows().into_iter().find(|t| t.key == s)
    }

    fn table(&self) -> Rect {
        let r = self.rect;
        let top = r.y + TITLE + CATEGORIES;
        Rect::new(
            r.x + PAD,
            top,
            r.width - 2.0 * PAD,
            r.bottom() - FOOTER - top,
        )
    }

    fn body(&self) -> Rect {
        let t = self.table();
        Rect::new(t.x, t.y + HEADER, t.width, (t.height - HEADER).max(0.0))
    }

    fn rows_fit(&self) -> usize {
        ((self.body().height - 4.0) / ROW).floor().max(1.0) as usize
    }

    fn clamp_scroll(&mut self) {
        let n = self.rows().len();
        self.first = self.first.min(n.saturating_sub(self.rows_fit()));
    }

    /// Scrolls the table by `rows`; whether it moved.
    pub fn wheel(&mut self, rows: i32) -> bool {
        let before = self.first;
        let n = self.rows().len();
        let last = n.saturating_sub(self.rows_fit());
        self.first = (self.first as i64 + i64::from(rows)).clamp(0, last as i64) as usize;
        self.first != before
    }

    /// Each column's span in the table: the task column takes what the numbers leave.
    fn columns(&self) -> [(Column, f32, f32); 5] {
        let t = self.table();
        let mut right = t.right();
        let mut spans = [(Column::Task, 0.0, 0.0); 5];
        for (i, (c, w)) in NUMBERS.iter().rev().enumerate() {
            spans[4 - i] = (*c, right - w, *w);
            right -= w;
        }
        spans[0] = (Column::Task, t.x, right - t.x);
        spans
    }

    fn close_rect(&self) -> Rect {
        Rect::new(self.rect.right() - 44.0, self.rect.y + 9.0, 30.0, 30.0)
    }

    fn category_rect(&self, fonts: &Fonts, i: usize) -> Rect {
        let mut x = self.rect.x + PAD;
        for c in &Category::ALL[..i] {
            x += fonts.width(c.label(), TEXT) + 32.0;
        }
        let w = fonts.width(Category::ALL[i].label(), TEXT) + 24.0;
        Rect::new(x, self.rect.y + TITLE, w, CATEGORIES - 4.0)
    }

    fn end_rect(&self, fonts: &Fonts) -> Rect {
        let w = widgets::button_width(fonts, END_TASK);
        Rect::new(
            self.rect.right() - PAD - w,
            self.rect.bottom() - FOOTER + 14.0,
            w,
            32.0,
        )
    }

    /// What is at `p` (content-area DIPs).
    pub fn hit(&self, p: Point, fonts: &Fonts) -> Option<TaskHit> {
        if !self.rect.contains(p) {
            return None;
        }
        if self.close_rect().contains(p) {
            return Some(TaskHit::Close);
        }
        for (i, c) in Category::ALL.iter().enumerate() {
            if self.category_rect(fonts, i).contains(p) {
                return Some(TaskHit::Category(*c));
            }
        }
        if self.end_rect(fonts).contains(p) {
            return Some(TaskHit::EndTask);
        }
        let t = self.table();
        if p.y >= t.y && p.y < t.y + HEADER {
            return self
                .columns()
                .iter()
                .find(|(_, x, w)| p.x >= *x && p.x < x + w)
                .map(|(c, _, _)| TaskHit::Header(*c));
        }
        let body = self.body();
        if body.contains(p) {
            let i = self.first + ((p.y - body.y) / ROW) as usize;
            if i < self.rows().len() && i < self.first + self.rows_fit() {
                return Some(TaskHit::Row(i));
            }
        }
        None
    }

    /// A press at `p` (`clicks`: 2 for a double click).
    pub fn press(&mut self, p: Point, clicks: u32, fonts: &Fonts) -> TaskAction {
        match self.hit(p, fonts) {
            Some(TaskHit::Close) => TaskAction::Close,
            Some(TaskHit::Category(c)) => {
                self.category = c;
                self.first = 0;
                self.clamp_scroll();
                TaskAction::None
            }
            Some(TaskHit::Header(c)) => {
                // (a second press on the sorted column turns it round)
                self.sort = match self.sort {
                    Some((s, ascending)) if s == c => Some((c, !ascending)),
                    // (numbers start with the largest, as Chrome's do)
                    _ => Some((c, c == Column::Task)),
                };
                TaskAction::None
            }
            Some(TaskHit::Row(i)) => {
                let key = self.rows().get(i).map(|t| t.key);
                self.selected = key;
                match key {
                    Some(TaskKey::Tab(id)) if clicks >= 2 => TaskAction::Activate(id),
                    _ => TaskAction::None,
                }
            }
            Some(TaskHit::EndTask) => match self.selected_task() {
                Some(Task {
                    key,
                    killable: true,
                    ..
                }) => TaskAction::EndTask(*key),
                _ => TaskAction::None,
            },
            None => TaskAction::None,
        }
    }

    pub fn paint(&self, b: &mut DisplayListBuilder, fonts: &Fonts, t: &Theme) {
        let r = self.rect;
        b.box_shadow(BoxShadow {
            rect: r,
            radii: Radii::uniform(10.0),
            offset: Point::new(0.0, 4.0),
            blur: 18.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, if t.dark { 140 } else { 60 }),
            inset: false,
        });
        b.fill_rounded_rect(r, Radii::uniform(10.0), t.bubble_border);
        b.fill_rounded_rect(r.inflate(-1.0, -1.0), Radii::uniform(9.0), t.dropdown);
        // The title and the close button.
        fonts.label_in(
            b,
            Face::Bold,
            "Task manager",
            r.x + PAD,
            r.y + TITLE / 2.0 + 2.0,
            15.0,
            t.text,
            r.width - 80.0,
        );
        let c = self.close_rect();
        if self.hover == Some(TaskHit::Close) {
            b.fill_rounded_rect(c, Radii::uniform(15.0), t.button_hover);
        }
        fonts.icon(
            b,
            glyph::CLOSE,
            Point::new(c.x + c.width / 2.0, c.y + c.height / 2.0),
            10.0,
            t.text,
        );
        // The categories, the chosen one underlined.
        for (i, cat) in Category::ALL.iter().enumerate() {
            let cr = self.category_rect(fonts, i);
            let on = *cat == self.category;
            if self.hover == Some(TaskHit::Category(*cat)) && !on {
                b.fill_rounded_rect(cr, Radii::uniform(6.0), t.dropdown_hover);
            }
            fonts.label_in(
                b,
                if on { Face::Bold } else { Face::Ui },
                cat.label(),
                cr.x + 12.0,
                cr.y + cr.height / 2.0,
                TEXT,
                if on { t.accent } else { t.text },
                cr.width,
            );
            if on {
                b.fill_rect(Rect::new(cr.x, cr.bottom() - 2.0, cr.width, 2.0), t.accent);
            }
        }
        b.fill_rect(
            Rect::new(
                r.x + PAD,
                r.y + TITLE + CATEGORIES - 2.0,
                r.width - 2.0 * PAD,
                1.0,
            ),
            t.separator,
        );
        // The table.
        let table = self.table();
        b.stroke_rounded_rect(
            table.inflate(-0.5, -0.5),
            Radii::uniform(8.0),
            1.0,
            t.separator,
        );
        let columns = self.columns();
        let hy = table.y + HEADER / 2.0;
        for (col, x, w) in columns {
            let label = col.label();
            let lw = fonts.width_in(Face::Bold, label, TEXT);
            let numeric = col != Column::Task;
            let lx = if numeric { x + w - 12.0 - lw } else { x + 12.0 };
            if self.hover == Some(TaskHit::Header(col)) {
                b.fill_rect(
                    Rect::new(x, table.y + 1.0, w, HEADER - 1.0),
                    t.dropdown_hover,
                );
            }
            fonts.label_in(b, Face::Bold, label, lx, hy, TEXT, t.text, w - 16.0);
            if let Some((s, ascending)) = self.sort {
                if s == col {
                    let gx = if numeric { lx - 10.0 } else { lx + lw + 10.0 };
                    let g = if ascending {
                        glyph::CHEVRON_UP
                    } else {
                        glyph::CHEVRON_DOWN
                    };
                    fonts.icon(b, g, Point::new(gx, hy), 9.0, t.text_dim);
                }
            }
            if col != Column::Task {
                b.fill_rect(Rect::new(x, table.y + 8.0, 1.0, HEADER - 16.0), t.separator);
            }
        }
        b.fill_rect(
            Rect::new(table.x, table.y + HEADER - 1.0, table.width, 1.0),
            t.separator,
        );
        let body = self.body();
        b.push_clip_rect(body);
        let rows = self.rows();
        for (k, task) in rows
            .iter()
            .enumerate()
            .skip(self.first)
            .take(self.rows_fit())
        {
            let y = body.y + (k - self.first) as f32 * ROW;
            let row = Rect::new(body.x + 4.0, y + 1.0, body.width - 8.0, ROW - 2.0);
            if self.selected == Some(task.key) {
                b.fill_rounded_rect(row, Radii::uniform(6.0), t.selection);
            } else if self.hover == Some(TaskHit::Row(k)) {
                b.fill_rounded_rect(row, Radii::uniform(6.0), t.dropdown_hover);
            }
            let cy = y + ROW / 2.0;
            let (_, tx, tw) = columns[0];
            let icon = Rect::new(tx + 12.0, cy - 8.0, 16.0, 16.0);
            match (task.key, task.icon) {
                (TaskKey::Browser | TaskKey::Gpu, _) => fonts.icon(
                    b,
                    BROWSER_ICON,
                    Point::new(icon.x + 8.0, cy),
                    14.0,
                    t.accent,
                ),
                (_, Some(id)) => b.image(id, icon, None, 1.0, ImageRendering::Smooth),
                (_, None) => fonts.icon(
                    b,
                    glyph::GLOBE,
                    Point::new(icon.x + 8.0, cy),
                    14.0,
                    t.text_dim,
                ),
            }
            fonts.label_in(
                b,
                Face::Ui,
                &task.title,
                tx + 38.0,
                cy,
                TEXT,
                t.text,
                tw - 46.0,
            );
            for (col, x, w) in &columns[1..] {
                let s = match col {
                    Column::Memory => memory_text(task.memory),
                    Column::Cpu => cpu_text(task.cpu),
                    Column::Network => network_text(task.network),
                    Column::Pid => task.pid.to_string(),
                    Column::Task => String::new(),
                };
                let sw = fonts.width(&s, TEXT);
                fonts.label(b, &s, x + w - 12.0 - sw, cy, TEXT, t.text, *w);
            }
        }
        if rows.is_empty() {
            fonts.label_in(
                b,
                Face::Ui,
                "Nothing runs here.",
                body.x + 16.0,
                body.y + ROW / 2.0,
                TEXT,
                t.text_dim,
                body.width - 32.0,
            );
        }
        b.pop_clip();
        // End task.
        let e = self.end_rect(fonts);
        let enabled = self.can_end_task();
        widgets::draw_button(
            b,
            fonts,
            t,
            e,
            END_TASK,
            enabled,
            enabled && self.hover == Some(TaskHit::EndTask),
        );
        if !enabled {
            // (drawn faded: nothing selected can be ended)
            b.fill_rounded_rect(
                e,
                Radii::uniform(e.height / 2.0),
                t.dropdown.with_alpha(150),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(key: TaskKey, title: &str, cpu: f64, killable: bool) -> Task {
        Task {
            key,
            title: title.into(),
            icon: None,
            memory: Some((cpu * 1000.0) as u64 * 1024),
            cpu: Some(cpu),
            network: Some(0.0),
            pid: 7,
            killable,
        }
    }

    fn manager() -> TaskManager {
        let mut m = TaskManager::new(Size::new(1200.0, 800.0));
        m.set_tasks(vec![
            task(TaskKey::Browser, "Browser", 3.0, false),
            task(TaskKey::Tab(1), "Tab: b", 1.0, true),
            task(TaskKey::Tab(2), "Tab: A", 2.0, false),
        ]);
        m
    }

    fn keys(m: &TaskManager) -> Vec<TaskKey> {
        m.rows().iter().map(|t| t.key).collect()
    }

    #[test]
    fn figures_are_written_as_chrome_writes_them() {
        assert_eq!(memory_text(Some(66_932_736)), "65,364K");
        assert_eq!(memory_text(Some(1023)), "0K");
        assert_eq!(memory_text(None), NOT_AVAILABLE);
        assert_eq!(cpu_text(Some(1.44)), "1.4");
        assert_eq!(cpu_text(Some(0.0)), "0.0");
        assert_eq!(cpu_text(Some(f64::NAN)), NOT_AVAILABLE);
        assert_eq!(cpu_text(None), NOT_AVAILABLE);
        assert_eq!(network_text(Some(0.0)), "0");
        assert_eq!(network_text(Some(0.2)), "0");
        assert_eq!(network_text(Some(512.0)), "512 B/s");
        assert_eq!(network_text(Some(1536.0)), "1.5 kB/s");
        assert_eq!(network_text(Some(150.0 * 1024.0)), "150 kB/s");
        assert_eq!(network_text(Some(3.0 * 1024.0 * 1024.0)), "3.0 MB/s");
        assert_eq!(network_text(None), NOT_AVAILABLE);
    }

    #[test]
    fn categories_filter_and_headers_sort() {
        let mut m = manager();
        // The tabs first, in strip order.
        assert_eq!(keys(&m), vec![TaskKey::Tab(1), TaskKey::Tab(2)]);
        m.category = Category::All;
        assert_eq!(
            keys(&m),
            vec![TaskKey::Browser, TaskKey::Tab(1), TaskKey::Tab(2)]
        );
        m.category = Category::Browser;
        assert_eq!(keys(&m), vec![TaskKey::Browser]);
        m.category = Category::All;
        // Numbers sort largest first; a second press turns the order round.
        m.sort = Some((Column::Cpu, false));
        assert_eq!(
            keys(&m),
            vec![TaskKey::Browser, TaskKey::Tab(2), TaskKey::Tab(1)]
        );
        m.sort = Some((Column::Task, true));
        assert_eq!(
            keys(&m),
            vec![TaskKey::Browser, TaskKey::Tab(2), TaskKey::Tab(1)]
        );
    }

    #[test]
    fn end_task_needs_a_selected_task_that_can_end() {
        let mut m = manager();
        assert!(!m.can_end_task());
        m.selected = Some(TaskKey::Tab(2));
        assert!(!m.can_end_task());
        m.selected = Some(TaskKey::Tab(1));
        assert!(m.can_end_task());
        // A selection hidden by the category does not count.
        m.category = Category::Browser;
        assert!(!m.can_end_task());
        // A task that goes away takes the selection with it.
        m.category = Category::Tabs;
        m.set_tasks(vec![task(TaskKey::Browser, "Browser", 1.0, false)]);
        assert_eq!(m.selected, None);
    }

    #[test]
    fn the_gpu_process_is_a_browser_task_that_can_end() {
        let mut m = manager();
        let mut tasks = m.tasks().to_vec();
        tasks.insert(1, task(TaskKey::Gpu, "GPU Process", 0.5, true));
        m.set_tasks(tasks);
        m.category = Category::Browser;
        assert_eq!(keys(&m), vec![TaskKey::Browser, TaskKey::Gpu]);
        m.category = Category::Tabs;
        assert_eq!(keys(&m), vec![TaskKey::Tab(1), TaskKey::Tab(2)]);
        m.category = Category::Browser;
        m.selected = Some(TaskKey::Gpu);
        assert!(m.can_end_task());
    }
}
