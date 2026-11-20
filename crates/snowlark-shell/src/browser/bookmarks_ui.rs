// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of the bookmarks: the bar's items and icons, opening them, their folders'
//! drop-downs and menus, dragging them along the bar and through the drop-downs (the targets are
//! `bookmark_drag.rs`), Ctrl+D and the bookmark editor.

use base64::Engine as _;
use snowlark_net::Url;
use snowlark_paint::window::{Modifiers, MouseButton, Window};
use snowlark_paint::Point;

use super::bookmark_drag::{self, BookmarkDrag, DropTarget, Opens};
use super::commands::{Command, DialogKind};
use super::{Browser, TIMER_FOLDER_DROP};
use crate::address::TextField;
use crate::bookmarks::{BookmarkId, Item, Place};
use crate::chrome::{self, glyph, BarItemView, Hit};
use crate::dialog::{Dialog, DialogResult, Field};
use crate::menu::{Entry, MenuIcon};
use crate::tabs::Placement;

/// The largest icon file kept with a bookmark.
const MAX_ICON_BYTES: usize = 64 * 1024;

impl Browser {
    /// The widths of the bar's items (cached per bookmarks version).
    pub(super) fn bar_item_widths(&self) -> Vec<f32> {
        let version = self.bookmarks.version();
        if let Some((v, w)) = &*self.bar_widths.borrow() {
            if *v == version {
                return w.clone();
            }
        }
        let widths: Vec<f32> = self
            .bookmarks
            .bar()
            .iter()
            .map(|i| {
                let label = i.label();
                chrome::bookmark_width(self.fonts.width(label, 12.0), !label.is_empty())
            })
            .collect();
        *self.bar_widths.borrow_mut() = Some((version, widths.clone()));
        widths
    }

    /// Decodes the icons of the bar's items not decoded yet.
    pub(super) fn ensure_bookmark_icons(&mut self) {
        let missing: Vec<(BookmarkId, Option<String>)> = self
            .bookmarks
            .bar()
            .iter()
            .filter(|i| !self.bookmark_icons.contains_key(&i.id))
            .map(|i| (i.id, i.icon.clone()))
            .collect();
        for (id, icon) in missing {
            let img = icon
                .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
                .and_then(|bytes| crate::tab::decode_icon(&bytes))
                .map(|img| self.images.add(img));
            self.bookmark_icons.insert(id, img);
        }
    }

    pub(super) fn bar_views(&self) -> Vec<BarItemView<'_>> {
        self.bookmarks
            .bar()
            .iter()
            .map(|i| BarItemView {
                label: i.label(),
                icon: self.bookmark_icons.get(&i.id).copied().flatten(),
                folder: i.is_folder(),
            })
            .collect()
    }

    /// The bookmarks changed: the bar and anything showing them are drawn again.
    pub(super) fn bookmarks_changed(&mut self) {
        // (icons of removed items go from the image store)
        let live: Vec<BookmarkId> = self
            .bookmark_icons
            .keys()
            .copied()
            .filter(|id| self.bookmarks.find(*id).is_none())
            .collect();
        for id in live {
            if let Some(Some(img)) = self.bookmark_icons.remove(&id) {
                self.images.remove(img);
            }
        }
        self.chrome_dirty = true;
        self.overlay_dirty = true;
        self.bookmark_pages_changed();
    }

    pub(super) fn toggle_bookmarks_bar(&mut self) {
        self.settings.bookmarks_bar = !self.settings.bookmarks_bar;
        self.save_settings();
        self.close_popup();
        // (the page area changes: a resize of the content)
        self.resized();
        self.chrome_dirty = true;
        self.overlay_dirty = true;
    }

    fn active_page(&self) -> Option<(String, String, Option<String>)> {
        let e = self.tabs.active()?;
        let url = e.tab.state.url.clone();
        if url.is_empty() || url == crate::address::START_PAGE || e.crashed.is_some() {
            return None;
        }
        let icon = e
            .icon_bytes
            .as_ref()
            .filter(|b| b.len() <= MAX_ICON_BYTES)
            .map(|b| base64::engine::general_purpose::STANDARD.encode(b.as_slice()));
        Some((url, e.tab.state.title.clone(), icon))
    }

    /// Whether the page shown is bookmarked.
    pub(super) fn active_starred(&self) -> bool {
        let url = self.active_url();
        !url.is_empty() && self.bookmarks.find_url(&url).is_some()
    }

    /// Ctrl+D and the star: bookmarks the page at the end of the bar (or finds its bookmark) and
    /// opens the editor under the star.
    pub(super) fn bookmark_page(&mut self) {
        let Some((url, title, icon)) = self.active_page() else {
            return;
        };
        let (id, is_new) = match self.bookmarks.find_url(&url) {
            Some(id) => (id, false),
            None => {
                let place = Place {
                    parent: None,
                    index: usize::MAX,
                };
                let title = if title.trim().is_empty() {
                    url.clone()
                } else {
                    title
                };
                match self.bookmarks.add(Item::page(&title, &url, icon), place) {
                    Some(id) => (id, true),
                    None => return,
                }
            }
        };
        self.bookmarks_changed();
        self.edit_bookmark(id, is_new);
    }

    /// The editor of a bookmark (or folder): from the star when `is_new` (Remove, Done), else from
    /// its menu (Cancel, Save).
    pub(super) fn edit_bookmark(&mut self, id: BookmarkId, is_new: bool) {
        let Some(item) = self.bookmarks.find(id).cloned() else {
            return;
        };
        let folder = self.bookmarks.place_of(id).and_then(|p| p.parent);
        let mut fields = vec![Field::Text {
            label: "Name".into(),
            field: TextField::new(&item.title),
        }];
        let has_url = !item.is_folder();
        if has_url && !is_new {
            fields.push(Field::Text {
                label: "URL".into(),
                field: TextField::new(&item.url),
            });
        }
        fields.push(Field::Choice {
            label: "Folder".into(),
            value: self.folder_name(folder),
        });
        let (title, buttons) = if is_new {
            (
                "Bookmark added",
                vec![
                    ("Remove".to_owned(), Command::DialogRemove, false),
                    ("Done".to_owned(), Command::DialogOk, true),
                ],
            )
        } else {
            (
                if item.is_folder() {
                    "Edit folder"
                } else {
                    "Edit bookmark"
                },
                vec![
                    ("Cancel".to_owned(), Command::DialogCancel, false),
                    ("Save".to_owned(), Command::DialogOk, true),
                ],
            )
        };
        let mut d = Dialog::new(title, Vec::new(), fields, buttons);
        let anchor = is_new.then(|| {
            let star = self.layout().star;
            Point::new(star.right() + 4.0, 4.0)
        });
        d.place(anchor, self.content_dips(), &self.fonts);
        self.dialog = Some((
            d,
            DialogKind::Bookmark {
                id,
                folder,
                has_url: has_url && !is_new,
            },
        ));
        self.close_popup();
        self.overlay_dirty = true;
        self.chrome_dirty = true;
    }

    pub(super) fn new_folder_dialog(&mut self, place: Place) {
        let mut d = Dialog::new(
            "New folder",
            Vec::new(),
            vec![Field::Text {
                label: "Name".into(),
                field: TextField::new("New folder"),
            }],
            vec![
                ("Cancel".to_owned(), Command::DialogCancel, false),
                ("Save".to_owned(), Command::DialogOk, true),
            ],
        );
        d.place(None, self.content_dips(), &self.fonts);
        self.dialog = Some((d, DialogKind::NewFolder { place }));
        self.overlay_dirty = true;
    }

    fn folder_name(&self, folder: Option<BookmarkId>) -> String {
        folder
            .and_then(|f| self.bookmarks.find(f))
            .map_or_else(|| "Bookmarks bar".to_owned(), |i| i.label().to_owned())
    }

    /// The bookmark editor's folder chooser picked `folder`.
    pub(super) fn folder_chosen(&mut self, folder: Option<BookmarkId>) {
        let name = self.folder_name(folder);
        if let Some((d, DialogKind::Bookmark { folder: f, .. })) = &mut self.dialog {
            *f = folder;
            let i = d.fields.len() - 1;
            d.set_choice(i, &name);
            self.overlay_dirty = true;
        }
    }

    /// A dialog's button (or Enter, or Escape).
    pub(super) fn dialog_button(&mut self, cmd: Command) {
        let Some((d, kind)) = self.dialog.take() else {
            return;
        };
        self.overlay_dirty = true;
        self.chrome_dirty = true;
        match (cmd, kind) {
            (Command::DialogRemove, DialogKind::Bookmark { id, .. }) => {
                self.bookmarks.remove(id);
            }
            (
                Command::DialogOk,
                DialogKind::Bookmark {
                    id,
                    folder,
                    has_url,
                },
            ) => {
                let name = d.text(0).trim().to_owned();
                let url = has_url
                    .then(|| d.text(1).trim().to_owned())
                    .and_then(|u| crate::address::interpret(&u))
                    .map(|u| u.to_string());
                self.bookmarks.edit(id, &name, url.as_deref());
                if self.bookmarks.place_of(id).map(|p| p.parent) != Some(folder) {
                    self.bookmarks.move_to(
                        id,
                        Place {
                            parent: folder,
                            index: usize::MAX,
                        },
                    );
                }
            }
            (Command::DialogOk, DialogKind::ClearSite { site }) => {
                self.clear_site_data(site);
                return;
            }
            (Command::DialogOk, DialogKind::TabHung { tab, .. }) => {
                self.kill_tab_script(tab);
                return;
            }
            (_, DialogKind::CloseWindow) => return,
            (Command::DialogCancel, DialogKind::TabHung { tab, pass }) => {
                self.wait_for_tab(tab, pass);
                return;
            }
            (Command::DialogOk, DialogKind::NewFolder { place }) => {
                let name = d.text(0).trim().to_owned();
                let name = if name.is_empty() {
                    "New folder".to_owned()
                } else {
                    name
                };
                self.bookmarks.add(Item::folder(&name), place);
            }
            _ => {}
        }
        self.bookmarks_changed();
    }

    /// What a dialog result asks for.
    pub(super) fn dialog_result(&mut self, window: &Window, r: DialogResult<Command>) {
        match r {
            DialogResult::None => {}
            DialogResult::Redraw => self.overlay_dirty = true,
            DialogResult::Button(cmd) => self.run(window, cmd),
            DialogResult::Cancel => self.dialog_button(Command::DialogCancel),
            DialogResult::Choose(_, rect) => {
                let current = match &self.dialog {
                    Some((_, DialogKind::Bookmark { folder, id, .. })) => Some((*folder, *id)),
                    _ => None,
                };
                let Some((current, editing)) = current else {
                    return;
                };
                let entries: Vec<Entry<Command>> = self
                    .bookmarks
                    .folders()
                    .into_iter()
                    // (a folder doesn't go into itself)
                    .filter(|(f, _, _)| {
                        f.is_none_or(|f| {
                            let mut cur = Some(f);
                            while let Some(c) = cur {
                                if c == editing {
                                    return false;
                                }
                                cur = self.bookmarks.place_of(c).and_then(|p| p.parent);
                            }
                            true
                        })
                    })
                    .map(|(f, depth, name)| {
                        let label = format!("{}{}", "    ".repeat(depth), name);
                        Entry::item(&label, Command::ChooseFolder(f)).checked(f == current)
                    })
                    .collect();
                self.open_menu(entries, Point::new(rect.x, rect.bottom()), Hit::None, false);
            }
        }
    }

    /// Opens a bookmark here or in a new background tab.
    pub(super) fn open_bookmark(&mut self, id: BookmarkId, new_tab: bool) {
        let Some(url) = self
            .bookmarks
            .find(id)
            .filter(|i| !i.is_folder())
            .and_then(|i| Url::parse(&i.url).ok())
        else {
            return;
        };
        if new_tab {
            self.open_tab(url, Placement::Background);
        } else {
            self.navigate_active(url);
        }
    }

    pub(super) fn open_all_in_folder(&mut self, id: BookmarkId) {
        let urls: Vec<Url> = self
            .bookmarks
            .find(id)
            .and_then(|f| f.children.as_ref())
            .map(|c| {
                c.iter()
                    .filter(|i| !i.is_folder())
                    .filter_map(|i| Url::parse(&i.url).ok())
                    .collect()
            })
            .unwrap_or_default();
        for u in urls {
            self.open_tab(u, Placement::Background);
        }
    }

    fn item_entry(&self, i: &Item) -> Entry<Command> {
        if i.is_folder() {
            Entry::item(i.label(), Command::OpenFolder(i.id))
                .icon(MenuIcon::Glyph(glyph::FOLDER))
                .submenu()
        } else {
            let icon = match self.bookmark_icons.get(&i.id).copied().flatten() {
                Some(img) => MenuIcon::Image(img),
                None => MenuIcon::Glyph(glyph::GLOBE),
            };
            Entry::item(i.label(), Command::OpenBookmark(i.id, false)).icon(icon)
        }
    }

    /// A folder's drop-down rows.
    pub(super) fn folder_entries(&mut self, id: BookmarkId) -> Vec<Entry<Command>> {
        let Some(children) = self.bookmarks.find(id).and_then(|f| f.children.clone()) else {
            return Vec::new();
        };
        self.ensure_icons_of(&children);
        let mut out: Vec<Entry<Command>> = children.iter().map(|i| self.item_entry(i)).collect();
        let pages = children.iter().filter(|i| !i.is_folder()).count();
        if out.is_empty() {
            out.push(Entry::item("(empty)", Command::OpenFolder(id)).enabled(false));
        }
        if pages > 1 {
            out.push(Entry::Separator);
            out.push(Entry::item(
                &format!("Open all ({pages})"),
                Command::OpenAllInFolder(id),
            ));
        }
        out
    }

    fn ensure_icons_of(&mut self, items: &[Item]) {
        for i in items {
            if !self.bookmark_icons.contains_key(&i.id) {
                let img = i
                    .icon
                    .as_ref()
                    .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
                    .and_then(|bytes| crate::tab::decode_icon(&bytes))
                    .map(|img| self.images.add(img));
                self.bookmark_icons.insert(i.id, img);
            }
        }
    }

    /// A bookmark's (or the bar's) right-click menu.
    pub(super) fn bookmark_menu(&mut self, item: Option<usize>, at: Point) {
        let mut entries = Vec::new();
        let target = item.and_then(|i| self.bookmarks.bar().get(i)).cloned();
        let place = match &target {
            Some(_) => Place {
                parent: None,
                index: item.unwrap_or(0) + 1,
            },
            None => Place {
                parent: None,
                index: usize::MAX,
            },
        };
        if let Some(t) = &target {
            if t.is_folder() {
                entries.push(Entry::item(
                    "Open all in new tabs",
                    Command::OpenAllInFolder(t.id),
                ));
            } else {
                entries.push(Entry::item("Open", Command::OpenBookmark(t.id, false)));
                entries.push(Entry::item(
                    "Open in new tab",
                    Command::OpenBookmark(t.id, true),
                ));
            }
            entries.push(Entry::Separator);
            entries.push(
                Entry::item(
                    if t.is_folder() {
                        "Rename…"
                    } else {
                        "Edit…"
                    },
                    Command::EditBookmark(t.id),
                )
                .icon(MenuIcon::Glyph(glyph::EDIT)),
            );
            entries.push(
                Entry::item("Delete", Command::DeleteBookmark(t.id))
                    .icon(MenuIcon::Glyph(glyph::DELETE)),
            );
            entries.push(Entry::Separator);
        }
        entries.push(Entry::item("Bookmark this page…", Command::BookmarkPage));
        entries.push(Entry::item("Add folder…", Command::AddFolder(place)));
        entries.push(Entry::Separator);
        entries.push(Entry::item("Bookmark manager", Command::BookmarkManager));
        entries.push(
            Entry::item("Show bookmarks bar", Command::ToggleBookmarksBar)
                .checked(self.settings.bookmarks_bar),
        );
        self.open_menu(entries, at, Hit::None, false);
    }

    /// A press on the bar's item `i` (a left press may become a drag).
    pub(super) fn bar_press(&mut self, i: usize, dip: Point, button: MouseButton, mods: Modifiers) {
        match button {
            MouseButton::Left => {
                if let Some(id) = self.bookmarks.bar().get(i).map(|b| b.id) {
                    self.bm_drag = Some(BookmarkDrag::new(id, Some(i), dip));
                }
            }
            MouseButton::Middle => {
                if let Some(id) = self.bookmarks.bar().get(i).map(|b| b.id) {
                    if self.bookmarks.find(id).is_some_and(|b| b.is_folder()) {
                        self.open_all_in_folder(id);
                    } else {
                        self.open_bookmark(id, true);
                    }
                }
            }
            MouseButton::Right => {
                let at = Point::new(dip.x, 0.0);
                self.bookmark_menu(Some(i), at);
            }
            _ => {}
        }
        let _ = mods;
    }

    /// The bar's item `i` was clicked (pressed and released without a drag).
    pub(super) fn bar_click(&mut self, i: usize, mods: Modifiers) {
        let Some(item) = self.bookmarks.bar().get(i).cloned() else {
            return;
        };
        if item.is_folder() {
            self.open_folder_dropdown(i);
        } else {
            self.open_bookmark(item.id, mods.ctrl);
        }
    }

    /// Opens the drop-down of the bar's folder `i` under it.
    fn open_folder_dropdown(&mut self, i: usize) {
        let Some(id) = self.bookmarks.bar().get(i).map(|b| b.id) else {
            return;
        };
        let entries = self.folder_entries(id);
        let x = self
            .layout()
            .bar
            .as_ref()
            .and_then(|b| b.items.get(i).map(|r| r.x))
            .unwrap_or(0.0);
        self.open_menu(entries, Point::new(x, 0.0), Hit::Bookmark(i), false);
    }

    /// The "»" button: the items that don't fit.
    pub(super) fn bar_overflow(&mut self) {
        let layout = self.layout();
        let Some(bar) = &layout.bar else {
            return;
        };
        let shown = bar.items.len();
        let x = bar.overflow.map_or(0.0, |o| o.right());
        let items: Vec<Item> = self.bookmarks.bar()[shown..].to_vec();
        self.ensure_icons_of(&items);
        let entries: Vec<Entry<Command>> = items.iter().map(|i| self.item_entry(i)).collect();
        self.open_menu(entries, Point::new(x, 0.0), Hit::BookmarkOverflow, false);
    }

    /// A left press on a bookmark row of an open folder drop-down (or the "»" menu): the row
    /// acts on release unless the press becomes a drag. Returns whether the press was one.
    pub(super) fn menu_bookmark_press(&mut self, cp: Point, dip: Point) -> bool {
        let Some((stack, owner)) = self.popup.as_ref().filter(|_| !self.dt_popup) else {
            return false;
        };
        if !matches!(owner, Hit::Bookmark(_) | Hit::BookmarkOverflow) {
            return false;
        }
        let Some(depth) = stack.menu_at(cp) else {
            return false;
        };
        let m = &stack.menus[depth];
        let Some(row) = m.row_at(cp) else {
            return false;
        };
        let id = match &m.entries[row] {
            Entry::Item {
                enabled: true,
                action: Command::OpenBookmark(id, _) | Command::OpenFolder(id),
                ..
            } => *id,
            _ => return false,
        };
        self.bm_drag = Some(BookmarkDrag::new(id, None, dip));
        true
    }

    /// The pointer moved while a bookmark is pressed: past a few pixels it is a drag, whose drop
    /// target follows the pointer (over the bar, or over the rows of an open drop-down). Resting
    /// on a folder, or the "»" button, opens its drop-down after a moment.
    pub(super) fn bookmark_drag_move(&mut self, window: &Window, dip: Point) {
        let Some(drag) = self.bm_drag.as_mut() else {
            return;
        };
        let was = drag.dragging;
        if !drag.moved(dip) {
            return;
        }
        let id = drag.id;
        let cp = self.content_point(dip);
        let layout = self.layout();
        let target = bookmark_drag::drop_target(
            &self.bookmarks,
            id,
            layout.bar.as_ref(),
            dip,
            self.popup
                .as_ref()
                .filter(|_| !self.dt_popup)
                .map(|(s, _)| s),
            cp,
        );
        let drag = self.bm_drag.as_mut().expect("the drag");
        if was && drag.target == target {
            return;
        }
        drag.target = target;
        let opened = drag.opened;
        self.chrome_dirty = true;
        self.overlay_dirty = true;
        window.kill_timer(TIMER_FOLDER_DROP);
        if !was {
            self.hide_menu_hover();
        }
        // A drop-down this drag opened closes when the drag moves elsewhere on the bar (into the
        // drop-down it stays, as does one the drag started from).
        if opened && target.is_none_or(|t| t.on_bar()) {
            let same = match (target.and_then(|t| t.opens()), self.popup.as_ref()) {
                (Some(Opens::BarFolder(i)), Some((_, Hit::Bookmark(o)))) => i == *o,
                (Some(Opens::Overflow), Some((_, Hit::BookmarkOverflow))) => true,
                _ => false,
            };
            if !same && target.is_some() {
                self.close_popup();
                if let Some(d) = self.bm_drag.as_mut() {
                    d.opened = false;
                }
            }
        }
        if let Some((stack, _)) = self.popup.as_mut() {
            stack.drop = match target {
                Some(DropTarget::Menu {
                    depth,
                    row,
                    position,
                    ..
                }) => Some((depth, row, position)),
                _ => None,
            };
        }
        if target.is_some_and(|t| t.opens().is_some()) {
            window.set_timer(TIMER_FOLDER_DROP, bookmark_drag::OPEN_DELAY_MS);
        }
    }

    /// The drag rested on a folder (or the "»" button) long enough: its drop-down opens.
    pub(super) fn bookmark_drag_timer(&mut self, window: &Window) {
        window.kill_timer(TIMER_FOLDER_DROP);
        let Some(opens) = self
            .bm_drag
            .as_ref()
            .filter(|d| d.dragging)
            .and_then(|d| d.target.and_then(|t| t.opens()))
        else {
            return;
        };
        let owner = self.popup.as_ref().map(|(_, o)| *o);
        match opens {
            Opens::BarFolder(i) => {
                if owner == Some(Hit::Bookmark(i)) {
                    return;
                }
                self.open_folder_dropdown(i);
            }
            Opens::Overflow => {
                if owner == Some(Hit::BookmarkOverflow) {
                    return;
                }
                self.bar_overflow();
            }
            Opens::Submenu(depth, row) => {
                let Some((stack, _)) = self.popup.as_mut() else {
                    return;
                };
                if stack.menus.len() > depth + 1 && stack.menus[depth].highlight == Some(row) {
                    return;
                }
                let r = stack.open_submenu_of(depth, row);
                self.popup_result(window, r);
                return;
            }
        }
        if let Some(d) = self.bm_drag.as_mut() {
            d.opened = true;
        }
    }

    /// Where a drag over the bar drops: (marker x, folder index), for the bar's painting.
    pub(super) fn bar_drop_view(&self) -> Option<(f32, Option<usize>)> {
        match self.bm_drag.as_ref()?.target? {
            DropTarget::Bar { marker_x, .. } => Some((marker_x, None)),
            DropTarget::BarFolder { index, .. } => Some((0.0, Some(index))),
            _ => None,
        }
    }

    /// The press on a bookmark ended at `dip`: a click (on the bar's item, or the drop-down's
    /// row under the pointer), or the drop of a drag.
    pub(super) fn bookmark_drag_release(&mut self, window: &Window, dip: Point, mods: Modifiers) {
        let Some(drag) = self.bm_drag.take() else {
            return;
        };
        window.kill_timer(TIMER_FOLDER_DROP);
        self.chrome_dirty = true;
        self.overlay_dirty = true;
        if !drag.dragging {
            match drag.bar_item {
                Some(i) => {
                    if self.layout().hit(dip) == Hit::Bookmark(i) {
                        self.bar_click(i, mods);
                    }
                }
                None => {
                    let cp = self.content_point(dip);
                    if let Some((stack, _)) = self.popup.as_mut() {
                        let r = stack.click(cp);
                        self.popup_result(window, r);
                    }
                }
            }
            return;
        }
        let place = drag.target.and_then(|t| t.place());
        self.close_popup();
        if let Some(place) = place {
            if self.bookmarks.move_to(drag.id, place) {
                self.bookmarks_changed();
            }
        }
    }

    /// Escape during a drag: nothing moves, and the drop-downs close.
    pub(super) fn cancel_bookmark_drag(&mut self, window: &Window) {
        if self.bm_drag.take().is_none() {
            return;
        }
        window.kill_timer(TIMER_FOLDER_DROP);
        window.release_mouse();
        self.pressed = Hit::None;
        self.close_popup();
        self.chrome_dirty = true;
        self.overlay_dirty = true;
    }
}
