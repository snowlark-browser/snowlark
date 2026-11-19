// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of the bookmark manager: opening it, applying the changes its page asks for,
//! and keeping the page's copy of the bookmarks current.

use snowlark_net::Url;

use super::settings_ui::now_ms;
use super::Browser;
use crate::bookmarks::{BookmarkId, Item, Place};
use crate::bookmarks_page::{deleted_label, fixed_url, Op, BOOKMARKS_PAGE, TOAST_MS};
use crate::tabs::Placement;

impl Browser {
    /// Ctrl+Shift+O, the menu's "Bookmark manager".
    pub(super) fn open_bookmark_manager(&mut self) {
        self.open_own_page("snowlark://bookmarks", BOOKMARKS_PAGE);
    }

    /// The bookmark manager's copy of the bookmarks follows a change, and the tabs showing it
    /// show it again.
    pub(super) fn bookmark_pages_changed(&mut self) {
        self.internal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bookmarks = self.bookmarks.snapshot();
        for e in self.tabs.iter_mut().filter(|e| e.crashed.is_none()) {
            if e.tab.state.url.starts_with("snowlark://bookmarks") {
                e.tab.reload();
            }
        }
    }

    /// The bookmark manager asked for a change (`query`, from its apply address).
    pub(super) fn apply_bookmarks(&mut self, query: &str) {
        let Some(op) = Op::parse(query) else {
            return;
        };
        let changed = match op {
            Op::Edit { id, title, url } => {
                let url = match url.as_deref().map(fixed_url) {
                    Some(None) => return,
                    Some(Some(u)) => Some(u),
                    None => None,
                };
                self.bookmarks.edit(id, &title, url.as_deref())
            }
            Op::Add { parent, title, url } => {
                let item = match url.as_deref().map(fixed_url) {
                    Some(None) => return,
                    Some(Some(u)) => Item::page(&title, &u, None),
                    None => Item::folder(&title),
                };
                let end = Place {
                    parent,
                    index: usize::MAX,
                };
                self.bookmarks.add(item, end).is_some()
            }
            Op::Delete(ids) => {
                let n = self.bookmarks.delete(&ids);
                if n > 0 {
                    let label = deleted_label(&self.bookmarks.last_deleted());
                    self.set_bookmark_toast(Some((label, now_ms() + TOAST_MS)));
                }
                n > 0
            }
            Op::Undo => {
                // (the notice goes whether or not there was something to put back)
                self.set_bookmark_toast(None);
                self.bookmarks.undo_delete();
                true
            }
            Op::Move { ids, to } => self.bookmarks.move_all(&ids, to),
            Op::Sort(parent) => self.bookmarks.sort(parent),
            Op::Open { ids, active } => {
                self.open_bookmarks(&ids, active);
                false
            }
        };
        if changed {
            self.bookmarks_changed();
        }
    }

    fn set_bookmark_toast(&mut self, toast: Option<(String, i64)>) {
        self.internal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bookmark_toast = toast;
    }

    /// Opens pages in new tabs (a folder: the pages directly in it), behind this one or, with
    /// `active`, the first one shown.
    fn open_bookmarks(&mut self, ids: &[BookmarkId], active: bool) {
        let mut urls = Vec::new();
        for &id in ids {
            match self.bookmarks.find(id) {
                Some(Item {
                    children: Some(c), ..
                }) => urls.extend(c.iter().filter(|i| !i.is_folder()).map(|i| i.url.clone())),
                Some(i) => urls.push(i.url.clone()),
                None => {}
            }
        }
        for (k, url) in urls.iter().filter_map(|u| Url::parse(u).ok()).enumerate() {
            let placement = if active && k == 0 {
                Placement::Foreground
            } else {
                Placement::Background
            };
            self.open_tab(url, placement);
        }
    }
}
