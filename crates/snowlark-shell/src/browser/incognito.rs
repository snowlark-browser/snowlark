// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Incognito windows. The browser's windows share an [`App`]: the profile, the regular storage
//! partition and the content blocker. An Incognito window is a [`Browser`] of its own (its own
//! window, compositor, tabs and spare renderer) over the off-the-record partition
//! ([`Partition::off_the_record`]), which every open Incognito window shares and which goes with
//! the last of them: its cookies, storage, databases and cache with it. Its tabs' renderers are
//! its own, never a regular window's (its own spare pool; each tab's services are its
//! partition's). It records no history, keeps no session for a restore, saves no settings and no
//! window placement. It is told apart from a regular window at a glance: its frame and toolbar
//! are in the Incognito theme whatever the theme ([`chrome::incognito_theme`]), its toolbar has
//! an Incognito button with the count of open Incognito windows, its new tab page says what
//! Incognito keeps and what it does not ([`crate::incognito_page`]), and its title says
//! "(Incognito)".
//!
//! The browser's process ends when its last window (regular or Incognito) closes.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use snowlark_net::Url;
use snowlark_paint::placement::Placement;
use snowlark_paint::window::{self, WindowOptions, WindowProxy};

use crate::restore::{SavedWindow, SessionFile, Store};

use super::{Browser, Route};
use crate::chrome;
use crate::partition::Partition;
use crate::profile::Profile;
use crate::tab_host::ProcessModel;

impl Browser {
    /// Whether this is an Incognito window.
    pub(super) fn off_the_record(&self) -> bool {
        self.partition.is_off_the_record()
    }

    /// Whether the window's own parts (its frame, tab strip, toolbar) are drawn dark: with the dark
    /// theme, and always in an Incognito window. Pages are told the theme alone.
    pub(super) fn chrome_dark(&self) -> bool {
        self.dark || self.off_the_record()
    }

    /// The colours of the window's own parts: the Incognito theme in an Incognito window, else
    /// the light or dark theme.
    pub(super) fn chrome_theme(&self) -> chrome::Theme {
        if self.off_the_record() {
            chrome::incognito_theme()
        } else {
            chrome::theme(self.dark)
        }
    }

    /// An Incognito window's Incognito button label (with the count of open Incognito windows),
    /// `None` in a regular window and a popup window.
    pub(super) fn incognito_label(&self) -> Option<String> {
        (self.off_the_record() && !self.popup_window)
            .then(|| chrome::incognito_label(self.app.incognito_windows()))
    }

    /// The window was woken: an Incognito window opened or closed changes its button's count.
    pub(super) fn incognito_count_changed(&self) -> bool {
        self.off_the_record() && self.app.incognito_windows() != self.incognito_shown
    }

    /// Keeps the settings in the profile, and gives them to the other regular windows; an
    /// Incognito window's changes stay with it.
    pub(super) fn save_settings(&self) {
        if !self.off_the_record() {
            let _ = self.settings.save(&self.profile.settings_file());
            let seen = self
                .app
                .publish(&self.me, Some(self.settings.clone()), None);
            self.shared_seen.set(seen);
        }
    }

    /// After each event: a change this regular window made to the bookmarks (each saves the
    /// profile's file) goes to the other regular windows.
    pub(super) fn publish_bookmarks(&self) {
        if self.off_the_record() || self.bookmarks.version() == self.bookmarks_published.get() {
            return;
        }
        self.bookmarks_published.set(self.bookmarks.version());
        let seen = self
            .app
            .publish(&self.me, None, Some(self.bookmarks.snapshot()));
        self.shared_seen.set(seen);
    }

    /// Woken: what another regular window changed since this one last looked (the settings, the
    /// bookmarks) is taken up and shown.
    pub(super) fn take_shared(&mut self, window: &window::Window) {
        if self.off_the_record() {
            return;
        }
        let (settings, bookmarks) = self.app.shared.borrow().since(self.shared_seen.get());
        if let Some((n, b)) = bookmarks {
            self.bookmarks.take_tree(&b);
            self.bookmarks_published.set(self.bookmarks.version());
            self.shared_seen.set((self.shared_seen.get().0, n));
            self.bookmarks_changed();
        }
        if let Some((n, s)) = settings {
            self.shared_seen.set((n, self.shared_seen.get().1));
            self.settings_arrived(window, s);
        }
    }

    /// Ctrl+N, the app menu's "New window": a new regular window, beside this one. In an Incognito
    /// window it is a regular one too (Ctrl+Shift+N opens another Incognito window).
    pub(super) fn new_window(&self, window: &window::Window) {
        let placement = window.placement().map(|p| crate::window_state::beside(&p));
        self.app.open_window(None, placement, None);
    }

    /// Ctrl+Shift+N, the app menu's "New Incognito window": a new one, at its new-tab page,
    /// beside this window as a new regular window would be.
    pub(super) fn new_incognito_window(&self, window: &window::Window) {
        let placement = window.placement().map(|p| crate::window_state::beside(&p));
        self.app.open_incognito_window(None, placement);
    }

    /// "Open link in Incognito window": beside this window, as Ctrl+Shift+N.
    pub(super) fn open_in_incognito(&self, window: &window::Window, url: Url) {
        let placement = window.placement().map(|p| crate::window_state::beside(&p));
        self.app.open_incognito_window(Some(url), placement);
    }
}

/// What the browser's windows share (see the module documentation).
pub struct App {
    pub profile: Profile,
    pub model: ProcessModel,
    /// The regular profile's partition (it lives as long as the process).
    pub regular: Rc<Partition>,
    /// Content blocking (`None` with `SNOWLARK_BLOCK=0`): one engine and one list updater for
    /// every window.
    pub blocker: Option<Arc<snowlark_blocker::ContentBlocker>>,
    /// The GPU process (`--gpu-process`), shared by the windows.
    pub gpu_process: Option<Rc<crate::gpu_process::GpuHost>>,
    /// The off-the-record partition while an Incognito window holds it.
    off_the_record: RefCell<Weak<Partition>>,
    /// The Incognito windows' browsers, and their windows to wake when their count changes.
    incognito: RefCell<Vec<(Weak<RefCell<Browser>>, WindowProxy)>>,
    /// The regular windows' browsers, woken when one of them changes what they share.
    regular_windows: RefCell<Vec<(Weak<RefCell<Browser>>, WindowProxy)>>,
    /// The regular profile's settings and bookmarks as its windows last changed them.
    pub(super) shared: RefCell<Shared>,
    /// The regular profile's recently closed tabs (one list for its windows, newest last).
    pub(super) closed: Rc<RefCell<Vec<String>>>,
    /// The regular profile's downloads, loaded when its first window opens.
    downloads: RefCell<Option<Arc<std::sync::Mutex<crate::downloads::DownloadHistory>>>>,
    /// The saved passwords, loaded by the first regular window.
    passwords: RefCell<Option<Arc<std::sync::Mutex<crate::passwords::PasswordStore>>>>,
    /// The process's session file, which every regular window keeps its part of; opened by the
    /// first regular window.
    session: RefCell<Option<Store>>,
    /// The id the next regular window has in the session file.
    next_window: Cell<u64>,
    /// The windows a restart into an update left (`restore::take_restart`), taken when the session
    /// file is opened; the first regular window opens them.
    pub(super) restart_session: RefCell<Option<SessionFile>>,
    /// The updates: the check, its outcome, the restart (`crate::update`).
    pub updates: crate::update::Updates,
}

/// What the regular profile's windows share besides the partition: each window works on its own
/// copy of the settings and the bookmarks (and saves them to the profile), publishes every change
/// here with a count of the changes so far, and the others take the newest up when woken
/// ([`Browser::take_shared`]). An Incognito window takes no part: it reads the profile's files as
/// they are when it opens, and writes nothing.
#[derive(Default)]
pub(crate) struct Shared {
    settings: Option<(u64, crate::settings::Settings)>,
    bookmarks: Option<(u64, crate::bookmarks::Bookmarks)>,
}

/// What [`Shared::since`] gives: the settings and the bookmarks that changed, each with its count.
pub(crate) type Changes = (
    Option<(u64, crate::settings::Settings)>,
    Option<(u64, crate::bookmarks::Bookmarks)>,
);

impl Shared {
    /// The counts of the settings' and the bookmarks' changes so far.
    pub(crate) fn seen(&self) -> (u64, u64) {
        (
            self.settings.as_ref().map_or(0, |(n, _)| *n),
            self.bookmarks.as_ref().map_or(0, |(n, _)| *n),
        )
    }

    /// A window changed the settings or the bookmarks: they are the newest, one change more.
    /// The counts after it.
    pub(crate) fn publish(
        &mut self,
        settings: Option<crate::settings::Settings>,
        bookmarks: Option<crate::bookmarks::Bookmarks>,
    ) -> (u64, u64) {
        let (s, b) = self.seen();
        if let Some(v) = settings {
            self.settings = Some((s + 1, v));
        }
        if let Some(v) = bookmarks {
            self.bookmarks = Some((b + 1, v));
        }
        self.seen()
    }

    /// What changed since a window had the counts `seen`: the settings and the bookmarks (a
    /// copy that saves nowhere), each with its count.
    pub(crate) fn since(&self, seen: (u64, u64)) -> Changes {
        (
            self.settings
                .as_ref()
                .filter(|(n, _)| *n > seen.0)
                .map(|(n, v)| (*n, v.clone())),
            self.bookmarks
                .as_ref()
                .filter(|(n, _)| *n > seen.1)
                .map(|(n, v)| (*n, v.snapshot())),
        )
    }
}

impl App {
    pub fn new(
        profile: Profile,
        model: ProcessModel,
        gpu_process: Option<crate::gpu_process::GpuHost>,
    ) -> Rc<App> {
        let regular = Rc::new(Partition::regular(&profile));
        // Content blocking: on unless SNOWLARK_BLOCK=0.
        let blocker = snowlark_blocker::policy::enabled_by_env(true).then(|| {
            snowlark_blocker::ContentBlocker::open(&snowlark_blocker::data_dir(&profile.dir))
        });
        if let Some(b) = &blocker {
            // (the lists are fetched through the browser's own client, not filtered)
            b.start_updater(regular.net.clone());
        }
        Rc::new(App {
            profile,
            model,
            regular,
            blocker,
            gpu_process: gpu_process.map(Rc::new),
            off_the_record: RefCell::default(),
            incognito: RefCell::default(),
            regular_windows: RefCell::default(),
            shared: RefCell::default(),
            closed: Rc::default(),
            downloads: RefCell::default(),
            passwords: RefCell::default(),
            session: RefCell::default(),
            next_window: Cell::new(1),
            restart_session: RefCell::default(),
            updates: Default::default(),
        })
    }

    /// A regular window opened: it joins the session file (opened by the first one, or again by
    /// a regular window opened after the last one closed while an Incognito window stayed).
    /// Returns its id there and, when this call opened the file, the session of an unexpected
    /// end to offer.
    pub(super) fn join_session(&self) -> (u64, Option<SessionFile>) {
        let id = self.next_window.get();
        self.next_window.set(id + 1);
        let mut session = self.session.borrow_mut();
        let mut offer = None;
        if !session.as_ref().is_some_and(Store::is_open) {
            let (store, o) = Store::open(&self.profile.dir);
            *session = Some(store);
            offer = o;
            *self.restart_session.borrow_mut() = crate::restore::take_restart(&self.profile.dir);
        }
        if let Some(s) = session.as_mut() {
            s.register(id);
        }
        (id, offer)
    }

    /// Regular window `id`'s tabs and placement as they are now.
    pub(super) fn session_update(&self, id: u64, window: SavedWindow) {
        if let Some(s) = self.session.borrow_mut().as_mut() {
            s.update(id, window);
        }
    }

    /// Regular window `id` closed ([`Store::window_closed`]).
    pub(super) fn leave_session(&self, id: u64) {
        if let Some(s) = self.session.borrow_mut().as_mut() {
            s.window_closed(id);
        }
    }

    /// Windows ends the session (a shutdown is not a crash): the file is closed normally with
    /// every window in it.
    pub(super) fn end_session(&self) {
        if let Some(s) = self.session.borrow_mut().as_mut() {
            s.close_clean();
        }
    }

    /// The user chose to restart into an update, in the window of `from`: every regular window's
    /// tabs go to the restart file (`Store::close_for_restart`), and every window is woken to close
    /// (`Browser::closing_for_restart`). The process starts the new version once the last one has
    /// closed (`crate::run`).
    pub(super) fn restart_into_update(&self, from: &Weak<RefCell<Browser>>) {
        for (w, _) in self.regular_windows.borrow().iter() {
            if w.ptr_eq(from) {
                continue;
            }
            if let Some(b) = w.upgrade() {
                if let Ok(mut b) = b.try_borrow_mut() {
                    b.save_session();
                }
            }
        }
        if let Some(s) = self.session.borrow_mut().as_mut() {
            s.close_for_restart();
        }
        self.updates.restart.set(true);
        let regular = self.regular_windows.borrow();
        let incognito = self.incognito.borrow();
        for (_, proxy) in regular.iter().chain(incognito.iter()) {
            proxy.wake();
        }
    }

    /// The saved passwords a new window uses: the profile's, or none for an Incognito window
    /// (it neither saves nor fills: `docs/password-manager.md`).
    pub(super) fn passwords(
        &self,
        off_the_record: bool,
    ) -> Option<Arc<std::sync::Mutex<crate::passwords::PasswordStore>>> {
        if off_the_record {
            return None;
        }
        Some(
            self.passwords
                .borrow_mut()
                .get_or_insert_with(|| {
                    Arc::new(std::sync::Mutex::new(
                        crate::passwords::PasswordStore::load(
                            &self.profile.passwords_file(),
                            Box::new(crate::passwords::Dpapi),
                        ),
                    ))
                })
                .clone(),
        )
    }

    /// The downloads list a new window shows: the regular profile's one, or an Incognito
    /// window's own, kept only while it is open (its files stay).
    pub(super) fn downloads(
        &self,
        off_the_record: bool,
    ) -> Arc<std::sync::Mutex<crate::downloads::DownloadHistory>> {
        if off_the_record {
            return Arc::new(std::sync::Mutex::new(
                crate::downloads::DownloadHistory::in_memory(),
            ));
        }
        self.downloads
            .borrow_mut()
            .get_or_insert_with(|| {
                Arc::new(std::sync::Mutex::new(
                    crate::downloads::DownloadHistory::load(&self.profile.downloads_file()),
                ))
            })
            .clone()
    }

    /// A regular window's browser was made (`window`: its window).
    pub(super) fn add_regular(&self, b: Weak<RefCell<Browser>>, window: WindowProxy) {
        let mut list = self.regular_windows.borrow_mut();
        list.retain(|(w, _)| w.strong_count() > 0);
        list.push((b, window));
    }

    /// How many regular windows are open.
    pub fn regular_window_count(&self) -> usize {
        let mut list = self.regular_windows.borrow_mut();
        list.retain(|(w, _)| w.strong_count() > 0);
        list.len()
    }

    /// The window of `from` changed the settings (`settings`) or the bookmarks: the newest copy is
    /// kept, with the count of changes so far, and every other regular window is woken to take it.
    pub(super) fn publish(
        &self,
        from: &Weak<RefCell<Browser>>,
        settings: Option<crate::settings::Settings>,
        bookmarks: Option<crate::bookmarks::Bookmarks>,
    ) -> (u64, u64) {
        let seen = self.shared.borrow_mut().publish(settings, bookmarks);
        for (w, proxy) in self.regular_windows.borrow().iter() {
            if !w.ptr_eq(from) {
                proxy.wake();
            }
        }
        seen
    }

    /// The off-the-record partition: the open Incognito windows' one, or a new one.
    pub fn off_the_record(&self) -> Rc<Partition> {
        if let Some(p) = self.off_the_record.borrow().upgrade() {
            return p;
        }
        let p = Rc::new(Partition::off_the_record());
        *self.off_the_record.borrow_mut() = Rc::downgrade(&p);
        p
    }

    /// Whether the off-the-record partition is alive (an Incognito window holds it).
    pub fn has_off_the_record(&self) -> bool {
        self.off_the_record.borrow().strong_count() > 0
    }

    /// How many Incognito windows are open.
    pub fn incognito_windows(&self) -> usize {
        let mut list = self.incognito.borrow_mut();
        list.retain(|(w, _)| w.strong_count() > 0);
        list.len()
    }

    /// An Incognito window's browser was made (`window`: its window): it counts among them, and
    /// the others show the new count.
    pub(super) fn add_incognito(&self, b: Weak<RefCell<Browser>>, window: WindowProxy) {
        self.incognito.borrow_mut().push((b, window));
        self.incognito_changed();
    }

    /// An Incognito window opened or closed: every one is woken to redraw its button's count
    /// (a gone window's proxy does nothing).
    pub(super) fn incognito_changed(&self) {
        for (_, window) in self.incognito.borrow().iter() {
            window.wake();
        }
    }

    /// The spare renderer pool for a window of `partition`'s (its own: a spare never serves
    /// another partition's tab).
    fn spares(&self) -> Option<snowlark_tab::spare::SparePool> {
        let dumps = self.profile.crash_dumps_dir();
        crate::tab_host::spare_pool(self.model, move || {
            crate::tab_host::renderer_launch(None, &dumps)
        })
    }

    /// An Incognito window's: its title until its first tab has one, and `placement` (when it fits
    /// the monitors). An Incognito window is placed as a regular one is: beside the window it is
    /// opened from, or where the profile's last regular window was; it only keeps no placement of
    /// its own ([`Browser::save_window`]).
    pub fn incognito_window_options(placement: Option<Placement>) -> WindowOptions {
        WindowOptions {
            title: "Snowlark (Incognito)".into(),
            placement,
            ..WindowOptions::default()
        }
    }

    /// Opens a new regular window, at `url` or the start page, at `placement` (when it fits the
    /// monitors), with the tabs of `restored` (a window of a session being restored) in place of
    /// the start page: it shares the regular partition, the settings, the bookmarks, the
    /// downloads, the recently closed tabs and the session file with the other regular windows;
    /// its tabs' renderers come from a spare pool of its own. Called on the window thread.
    pub fn open_window(
        self: &Rc<App>,
        url: Option<Url>,
        placement: Option<Placement>,
        restored: Option<SavedWindow>,
    ) {
        let app = self.clone();
        let partition = self.regular.clone();
        let spares = self.spares();
        let options = WindowOptions {
            title: "Snowlark".into(),
            placement,
            ..WindowOptions::default()
        };
        if let Err(e) = window::open_browser_window(options, move |w| {
            let mut b = Browser::new(w, app, partition, url, spares);
            if let Some(saved) = restored {
                b.restore_window(saved);
            }
            Route::main(b)
        }) {
            eprintln!("[browser] no new window: {e}");
        }
    }

    /// Opens a new Incognito window, at `url` or its new-tab page, at `placement` (when it fits the
    /// monitors). Called on the window thread.
    pub fn open_incognito_window(self: &Rc<App>, url: Option<Url>, placement: Option<Placement>) {
        let app = self.clone();
        let partition = self.off_the_record();
        let spares = self.spares();
        let options = App::incognito_window_options(placement);
        if let Err(e) = window::open_browser_window(options, move |w| {
            Route::main(Browser::new(w, app, partition, url, spares))
        }) {
            eprintln!("[browser] no Incognito window: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Shared;
    use crate::bookmarks::{Bookmarks, Item, Place};
    use crate::settings::Settings;

    fn bar(b: &Bookmarks) -> Vec<String> {
        b.bar().iter().map(|i| i.title.clone()).collect()
    }

    /// Two regular windows: what one publishes the other takes up once, and only what changed.
    #[test]
    fn a_window_takes_up_what_another_shared_once() {
        let mut shared = Shared::default();
        let (mut a, mut b) = (shared.seen(), shared.seen());
        assert_eq!(a, (0, 0));
        // Window A adds a bookmark (its copy saves the file) and shares the tree.
        let mut mine = Bookmarks::in_memory();
        mine.add(
            Item::page("Alpha", "https://a.example/", None),
            Place {
                parent: None,
                index: 0,
            },
        );
        a = shared.publish(None, Some(mine.snapshot()));
        assert_eq!(shared.since(a).1.map(|(n, _)| n), None);
        // Window B is woken: the bookmarks, and no settings.
        let (settings, bookmarks) = shared.since(b);
        assert!(settings.is_none());
        let (n, tree) = bookmarks.unwrap();
        let mut theirs = Bookmarks::in_memory();
        let v = theirs.version();
        theirs.take_tree(&tree);
        assert_eq!(bar(&theirs), ["Alpha"]);
        assert!(theirs.version() > v);
        b = (b.0, n);
        assert!(shared.since(b).1.is_none());
        // Window B changes the settings; A takes them, and the bookmarks it has stay its own.
        let changed = Settings {
            bookmarks_bar: !Settings::default().bookmarks_bar,
            ..Settings::default()
        };
        b = shared.publish(Some(changed.clone()), None);
        let (settings, bookmarks) = shared.since(a);
        assert_eq!(settings.map(|(_, s)| s), Some(changed));
        assert!(bookmarks.is_none());
        assert_eq!(b, (1, 1));
    }
}
