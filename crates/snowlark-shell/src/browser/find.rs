// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Find in page in the window (Ctrl+F): the active tab's find box, its keyboard and mouse, and
//! what it asks of the tab's page. The box belongs to the tab ([`super::TabEntry::find`]): another
//! tab hides it and shows its own; the page does the searching on its thread
//! ([`crate::driver::FindCommand`]) and answers with the counter ([`crate::tab::TabEvent::Find`]).

use snowlark_paint::window::{keys, Modifiers, MouseButton};
use snowlark_paint::Point;

use super::{Browser, Focus};
use crate::driver::{FindCommand, FindStatus};
use crate::find_bar::{FindBar, FindHit};
use crate::tab::TabCommand;
use crate::widgets::{self, KeyEdit};

impl Browser {
    /// The active tab's find box, when it shows.
    fn find_bar(&self) -> Option<&FindBar> {
        self.tabs
            .active()
            .filter(|e| e.crashed.is_none())
            .and_then(|e| e.find.as_ref())
    }

    fn find_bar_mut(&mut self) -> Option<&mut FindBar> {
        self.tabs
            .active_mut()
            .filter(|e| e.crashed.is_none())
            .and_then(|e| e.find.as_mut())
    }

    /// Ctrl+F: opens the box (or selects its text again) with the keyboard in it. The text starts
    /// as the page's selected text, else what was searched for last.
    pub(super) fn open_find(&mut self) {
        if self.active_crashed() || self.tabs.active().is_none() {
            return;
        }
        let previous = match self.find_bar_mut() {
            Some(bar) => {
                bar.field.select_all();
                bar.fresh = true;
                bar.field.text().to_owned()
            }
            None => {
                let text = self.find_last.clone();
                if let Some(e) = self.tabs.active_mut() {
                    e.find = Some(FindBar::new(&text));
                }
                text
            }
        };
        self.close_popup();
        self.close_site_info();
        self.close_blocker_panel();
        self.focus = Focus::Find;
        self.caret_on = true;
        self.address_drag = false;
        self.send_active(TabCommand::Find(FindCommand::Open { previous }));
        self.overlay_dirty = true;
        self.chrome_dirty = true;
    }

    /// Closes the box and removes the highlights; the active match stays selected in the page.
    pub(super) fn close_find(&mut self) {
        let Some(bar) = self.tabs.active_mut().and_then(|e| e.find.take()) else {
            return;
        };
        if !bar.field.text().is_empty() {
            self.find_last = bar.field.text().to_owned();
        }
        self.send_active(TabCommand::Find(FindCommand::Close { select: true }));
        if self.focus == Focus::Find {
            self.focus = Focus::Page;
        }
        self.overlay_dirty = true;
        self.chrome_dirty = true;
    }

    /// The box's text changed: search for it.
    fn find_query(&mut self) {
        let Some(bar) = self.find_bar_mut() else {
            return;
        };
        bar.fresh = false;
        let text = bar.field.text().to_owned();
        self.find_last = text.clone();
        self.send_active(TabCommand::Find(FindCommand::Query(text)));
        self.caret_on = true;
        self.overlay_dirty = true;
    }

    /// The next (or previous) match; F3 opens the box when it is closed.
    pub(super) fn find_step(&mut self, forward: bool) {
        if self.find_bar().is_none() {
            self.open_find();
            return;
        }
        self.send_active(TabCommand::Find(if forward {
            FindCommand::Next
        } else {
            FindCommand::Previous
        }));
    }

    /// A key while the box has the keyboard.
    pub(super) fn find_key(&mut self, key: u16, mods: Modifiers) -> bool {
        if self.focus != Focus::Find || self.find_bar().is_none() {
            return false;
        }
        match key {
            keys::ESCAPE => self.close_find(),
            keys::ENTER => self.find_step(!mods.shift),
            keys::DOWN if !mods.alt => self.find_step(true),
            keys::UP if !mods.alt => self.find_step(false),
            _ => {
                let clip = self.tab_ctx.clipboard.clone();
                let edit = match self.find_bar_mut() {
                    Some(bar) => widgets::edit_key(&mut bar.field, key, mods, clip.as_ref()),
                    None => KeyEdit::Ignored,
                };
                match edit {
                    KeyEdit::Changed => self.find_query(),
                    KeyEdit::Moved => {
                        self.caret_on = true;
                        self.overlay_dirty = true;
                    }
                    KeyEdit::Ignored => return false,
                }
            }
        }
        true
    }

    /// A character typed while the box has the keyboard.
    pub(super) fn find_char(&mut self, c: char) {
        let typed = match self.find_bar_mut() {
            Some(bar) => widgets::type_char(&mut bar.field, c),
            None => false,
        };
        if typed {
            self.find_query();
        }
    }

    /// What the page answered: the box's counter (and, for a box just opened, the text the page
    /// started with).
    pub(super) fn find_status(&mut self, tab: usize, status: FindStatus) {
        let active = tab == self.tabs.active_index();
        if let Some(bar) = self.tabs.get_mut(tab).and_then(|e| e.find.as_mut()) {
            if bar.fresh {
                bar.fresh = false;
                if status.query != bar.field.text() {
                    bar.field.set_text(&status.query);
                    bar.field.select_all();
                }
            }
            bar.status = status;
            if active {
                self.overlay_dirty = true;
            }
        }
    }

    /// The pointer moved over the content area; whether the box is under it.
    pub(super) fn find_move(&mut self, cp: Point) -> Option<FindHit> {
        let area = self.page_dips();
        let bar = self.find_bar_mut()?;
        let hit = bar.hit(area, cp);
        let hover = hit.filter(|h| *h != FindHit::Box);
        if bar.hover != hover {
            bar.hover = hover;
            self.overlay_dirty = true;
        }
        hit
    }

    /// A press in the content area; whether the box took it (a press elsewhere takes the keyboard
    /// from it and goes on).
    pub(super) fn find_press(&mut self, cp: Point, button: MouseButton, clicks: u32) -> bool {
        let area = self.page_dips();
        let Some(hit) = self.find_bar().and_then(|b| b.hit(area, cp)) else {
            if self.focus == Focus::Find {
                self.focus = Focus::Page;
                self.overlay_dirty = true;
            }
            return false;
        };
        if button == MouseButton::Left {
            match hit {
                FindHit::Close => self.close_find(),
                FindHit::Next => self.find_step(true),
                FindHit::Previous => self.find_step(false),
                FindHit::Field | FindHit::Box => {
                    self.focus = Focus::Find;
                    self.caret_on = true;
                    let fonts_i = {
                        let bar = self.find_bar();
                        bar.map(|b| b.index_at(&self.fonts, area, cp.x))
                    };
                    if let (Some(i), Some(bar)) = (fonts_i, self.find_bar_mut()) {
                        if hit == FindHit::Field {
                            if clicks >= 2 {
                                bar.field.select_word_at(i);
                            } else {
                                bar.field.set_caret(i, false);
                            }
                        }
                    }
                    self.overlay_dirty = true;
                }
            }
        }
        self.swallow_release = true;
        true
    }
}
