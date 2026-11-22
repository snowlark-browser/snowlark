// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the window's menus and dialogs do: the [`Command`]s their rows and buttons carry, the app
//! menu, and running a command.

use snowlark_net::Url;
use snowlark_paint::window::Window;
use snowlark_paint::Point;

use super::Browser;
use crate::bookmarks::{BookmarkId, Place};
use crate::chrome::{glyph, Hit};
use crate::menu::{Entry, Menu, MenuIcon, MenuResult, MenuStack};
use crate::tabs::Placement;

/// A menu row's or a dialog button's action.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    NewTab,
    /// A new regular window.
    NewWindow,
    /// A new Incognito window.
    NewIncognitoWindow,
    /// A link, in a new Incognito window.
    OpenInIncognito(Url),
    ReopenClosedTab,
    CloseTab,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleBookmarksBar,
    DevTools,
    /// The task manager, opened or closed.
    TaskManager,
    /// Find in page: the find box.
    Find,
    /// The settings page, in a new tab (or the one showing it).
    Settings,
    /// The About page, in a new tab (or the one showing it).
    About,
    /// The history page, in a new tab (or the one showing it).
    History,
    /// The downloads page, in a new tab (or the one showing it).
    Downloads,
    BookmarkManager,
    Back,
    Forward,
    Reload,
    /// A link or image, in a new background tab.
    OpenInNewTab(Url),
    CopyText(String),
    /// A web search for the text with the default engine, in a new tab.
    SearchFor(String),
    SaveImage(Url),
    /// The page's source in a new tab.
    ViewSource,
    /// The developer tools on the element at this content-area point.
    Inspect(Point),
    /// Bookmark the page shown (or edit its bookmark).
    BookmarkPage,
    /// Open a bookmark, in a new background tab when true.
    OpenBookmark(BookmarkId, bool),
    /// A folder's submenu (on the bar's drop-downs).
    OpenFolder(BookmarkId),
    /// Every page in a folder, in new tabs.
    OpenAllInFolder(BookmarkId),
    EditBookmark(BookmarkId),
    DeleteBookmark(BookmarkId),
    /// A new folder at this place.
    AddFolder(Place),
    /// The folder chooser of the bookmark editor picked this folder (`None`: the bar).
    ChooseFolder(Option<BookmarkId>),
    Exit,
    /// The address bar's context menu: each acts on its text field.
    Address(AddressEdit),
    /// The tab strip's menu: reload a tab, mute or unmute it (by the user), mute or unmute a site
    /// (a registrable domain), close it, the others, or those to its right.
    ReloadTab(crate::tab::TabId),
    MuteTab(crate::tab::TabId, bool),
    MuteSite(String, bool),
    CloseTabId(crate::tab::TabId),
    CloseOtherTabs(crate::tab::TabId),
    CloseTabsToRight(crate::tab::TabId),
    /// Switches between the side tab panel and the tab strip.
    ToggleVerticalTabs,
    /// A row of the Elements panel's context menu, done to this node.
    DevtoolsNode(snowlark_dom::NodeId, crate::devtools::protocol::NodeOp),
    /// A row of the console's context menu.
    DevtoolsConsole(super::devtools_menus::ConsoleOp),
    /// A request's Copy submenu (its rows copy what the panel had when the menu opened).
    DevtoolsNetCopy(Box<crate::devtools::network::NetworkMenu>),
    /// A row of a developer tools field's editing menu.
    DevtoolsField(crate::devtools::FieldId, AddressEdit),
    /// A row of that menu that opens a submenu.
    DevtoolsSub(
        snowlark_dom::NodeId,
        crate::devtools::NodeKind,
        u8,
        crate::devtools::NodeSub,
    ),
    /// A dialog's buttons.
    DialogOk,
    DialogRemove,
    DialogCancel,
}

/// An edit the address bar's context menu makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressEdit {
    Undo,
    Cut,
    Copy,
    Paste,
    /// Paste over the whole text and go there (or search for it), as Enter would.
    PasteAndGo,
    Delete,
    SelectAll,
}

/// What an open dialog edits.
#[derive(Clone, Debug, PartialEq)]
pub enum DialogKind {
    /// A bookmark (a page or a folder): its field 0 is the name, then the address for a page, then
    /// the folder (a choice), which is `folder`.
    Bookmark {
        id: BookmarkId,
        folder: Option<BookmarkId>,
        has_url: bool,
    },
    /// A new folder's name, to be added at `place`.
    NewFolder { place: Place },
    /// Confirm clearing the data of a site (a host), or of all sites.
    ClearSite { site: Option<String> },
    /// Wait for, or kill the script of, a tab stuck in a pass of its loop (identified by the pass's
    /// start, see [`crate::watchdog::Liveness::stuck`]).
    TabHung { tab: crate::tab::TabId, pass: u64 },
    /// Close the window with all its tabs.
    CloseWindow,
}

impl Browser {
    /// The app menu (the "⋮" button).
    pub(super) fn app_menu(&self) -> Vec<Entry<Command>> {
        let zoom = self.tabs.active().map_or(1.0, |e| e.zoom);
        vec![
            Entry::item("New tab", Command::NewTab).shortcut("Ctrl+T"),
            Entry::item("New window", Command::NewWindow).shortcut("Ctrl+N"),
            Entry::item("New Incognito window", Command::NewIncognitoWindow)
                .shortcut("Ctrl+Shift+N"),
            Entry::item("Reopen closed tab", Command::ReopenClosedTab)
                .shortcut("Ctrl+Shift+T")
                .enabled(!self.closed.borrow().is_empty()),
            Entry::Separator,
            Entry::item("Bookmark this page…", Command::BookmarkPage)
                .shortcut("Ctrl+D")
                .icon(MenuIcon::Glyph(glyph::STAR)),
            Entry::item("Show bookmarks bar", Command::ToggleBookmarksBar)
                .shortcut("Ctrl+Shift+B")
                .checked(self.settings.bookmarks_bar),
            Entry::item("Bookmark manager", Command::BookmarkManager).shortcut("Ctrl+Shift+O"),
            Entry::item("History", Command::History).shortcut("Ctrl+H"),
            Entry::item("Downloads", Command::Downloads).shortcut("Ctrl+J"),
            Entry::Separator,
            Entry::item("Zoom in", Command::ZoomIn).shortcut("Ctrl++"),
            Entry::item("Zoom out", Command::ZoomOut).shortcut("Ctrl+-"),
            Entry::item(
                &format!("Actual size (now {}%)", (zoom * 100.0).round()),
                Command::ZoomReset,
            )
            .shortcut("Ctrl+0")
            .enabled((zoom - 1.0).abs() > 0.001),
            Entry::Separator,
            Entry::item("Find…", Command::Find)
                .shortcut("Ctrl+F")
                .icon(MenuIcon::Glyph(glyph::SEARCH)),
            Entry::Separator,
            Entry::item("Settings", Command::Settings).icon(MenuIcon::Glyph(glyph::SETTINGS)),
            Entry::item("Developer tools", Command::DevTools).shortcut("F12"),
            Entry::item("Task manager", Command::TaskManager)
                .shortcut("Shift+Esc")
                .icon(MenuIcon::Glyph(glyph::GAUGE)),
            Entry::item("About Snowlark", Command::About),
            Entry::Separator,
            Entry::item("Close tab", Command::CloseTab).shortcut("Ctrl+W"),
            Entry::item("Exit", Command::Exit),
        ]
    }

    /// Opens a menu at `at` (content-area DIPs) for the chrome button `owner`.
    pub(super) fn open_menu(
        &mut self,
        entries: Vec<Entry<Command>>,
        at: Point,
        owner: Hit,
        keyboard: bool,
    ) {
        let area = self.content_dips();
        let menu = Menu::new(entries, at, None, area, &self.fonts);
        self.popup = Some((MenuStack::new(menu, keyboard), owner));
        self.dt_popup = false;
        self.hide_menu_hover();
        self.overlay_dirty = true;
        self.chrome_dirty = true;
    }

    pub(super) fn hide_menu_hover(&mut self) {
        self.title = None;
        self.tooltip = None;
    }

    pub(super) fn close_popup(&mut self) {
        self.dt_popup = false;
        if self.popup.take().is_some() {
            self.overlay_dirty = true;
            self.chrome_dirty = true;
        }
    }

    /// What input did to the open menu.
    pub(super) fn popup_result(&mut self, window: &Window, r: MenuResult<Command>) {
        match r {
            MenuResult::None => {}
            MenuResult::Moved => self.overlay_dirty = true,
            MenuResult::Closed => self.close_popup(),
            MenuResult::OpenSub(cmd, row) => {
                let entries = self.submenu(&cmd);
                let area = if self.dt_popup {
                    self.panel_host_area()
                } else {
                    self.content_dips()
                };
                let keyboard = self.popup.as_ref().is_some_and(|(s, _)| s.keyboard);
                let menu = Menu::new(entries, Point::ZERO, Some(row), area, &self.fonts);
                if let Some((stack, _)) = &mut self.popup {
                    stack.open_sub(menu, keyboard);
                }
                self.overlay_dirty = true;
            }
            MenuResult::Chosen(cmd) if self.dt_popup => {
                // (from the tools' window: run from the main window's next event)
                self.close_popup();
                self.deferred.push(cmd);
                self.main_proxy.wake();
            }
            MenuResult::Chosen(cmd) => {
                self.close_popup();
                self.run(window, cmd);
            }
        }
    }

    /// The rows of a submenu.
    fn submenu(&mut self, cmd: &Command) -> Vec<Entry<Command>> {
        match cmd {
            Command::OpenFolder(id) => self.folder_entries(*id),
            Command::DevtoolsSub(node, kind, forced, sub) => {
                super::node_menu::sub_entries(*node, *kind, *forced, *sub)
            }
            Command::DevtoolsNetCopy(m) => super::devtools_menus::network_copy_entries(m),
            _ => Vec::new(),
        }
    }

    /// Runs a command.
    pub(super) fn run(&mut self, window: &Window, cmd: Command) {
        match cmd {
            Command::NewTab => self.new_tab(),
            Command::NewWindow => self.new_window(),
            Command::NewIncognitoWindow => self.new_incognito_window(),
            Command::OpenInIncognito(u) => self.open_in_incognito(u),
            Command::ReopenClosedTab => {
                let last = self.closed.borrow_mut().pop();
                if let Some(url) = last.and_then(|u| Url::parse(&u).ok()) {
                    self.open_tab(url, Placement::End);
                }
            }
            Command::CloseTab => {
                let i = self.tabs.active_index();
                self.close_tab(i, window);
            }
            Command::ZoomIn | Command::ZoomOut => {
                let z = self.tabs.active().map_or(1.0, |e| e.zoom);
                let dir = if cmd == Command::ZoomIn { 1 } else { -1 };
                self.set_zoom(super::next_zoom(z, dir));
            }
            Command::ZoomReset => self.set_zoom(1.0),
            Command::ToggleBookmarksBar => self.toggle_bookmarks_bar(),
            Command::ToggleVerticalTabs => self.toggle_vertical_tabs(),
            Command::DevTools => self.toggle_devtools(None),
            Command::TaskManager => self.toggle_task_manager(window),
            Command::Find => self.open_find(),
            Command::Settings => self.open_settings(),
            Command::History => self.open_history(),
            Command::Downloads => self.open_downloads(),
            Command::BookmarkManager => self.open_bookmark_manager(),
            Command::About => self.open_about(),
            Command::Back => self.go_active(-1),
            Command::Forward => self.go_active(1),
            Command::Reload => self.reload_active(),
            Command::OpenInNewTab(u) => self.open_tab(u, Placement::Background),
            Command::CopyText(s) => self.tab_ctx.clipboard.set_text(&s),
            Command::SearchFor(s) => {
                let u = crate::address::search_url_with(self.settings.engine().prefix, &s);
                self.open_tab(u, Placement::Foreground);
            }
            Command::SaveImage(u) => self.save_image(window, u),
            Command::ViewSource => self.view_source(),
            Command::Inspect(p) => self.inspect_at(p),
            Command::BookmarkPage => self.bookmark_page(),
            Command::OpenBookmark(id, new_tab) => self.open_bookmark(id, new_tab),
            Command::OpenFolder(_) => {}
            Command::OpenAllInFolder(id) => self.open_all_in_folder(id),
            Command::EditBookmark(id) => self.edit_bookmark(id, false),
            Command::DeleteBookmark(id) => {
                self.bookmarks.remove(id);
                self.bookmarks_changed();
            }
            Command::AddFolder(place) => self.new_folder_dialog(place),
            Command::ChooseFolder(f) => self.folder_chosen(f),
            Command::Exit => self.request_close(window),
            Command::Address(e) => self.address_edit(e),
            Command::ReloadTab(_)
            | Command::MuteTab(..)
            | Command::MuteSite(..)
            | Command::CloseTabId(_)
            | Command::CloseOtherTabs(_)
            | Command::CloseTabsToRight(_) => self.run_tab_command(window, cmd),
            Command::DevtoolsNode(node, op) => {
                // (Paste adds what the clipboard has)
                let op = match op {
                    crate::devtools::protocol::NodeOp::Paste(_) => {
                        crate::devtools::protocol::NodeOp::Paste(
                            self.tab_ctx.clipboard.text().unwrap_or_default(),
                        )
                    }
                    op => op,
                };
                self.send_active(crate::tab::TabCommand::Devtools(
                    crate::devtools::protocol::DevtoolsCommand::Node(node, op),
                ))
            }
            Command::DevtoolsSub(..) | Command::DevtoolsNetCopy(_) => {}
            Command::DevtoolsConsole(op) => self.devtools_console_op(op),
            Command::DevtoolsField(id, edit) => self.devtools_field_edit(id, edit),
            Command::DialogOk if matches!(self.dialog, Some((_, DialogKind::CloseWindow))) => {
                self.close_confirmed(window)
            }
            Command::DialogOk | Command::DialogRemove | Command::DialogCancel => {
                self.dialog_button(cmd)
            }
        }
    }
}
