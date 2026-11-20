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

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use snowlark_net::Url;
use snowlark_paint::window::{self, WindowOptions, WindowProxy};

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

    /// Keeps the settings in the profile; an Incognito window's changes stay with it.
    pub(super) fn save_settings(&self) {
        if !self.off_the_record() {
            let _ = self.settings.save(&self.profile.settings_file());
        }
    }

    /// Ctrl+Shift+N, the app menu's "New Incognito window": a new one, at its new-tab page.
    pub(super) fn new_incognito_window(&self) {
        self.app.open_incognito_window(None);
    }

    /// "Open link in Incognito window".
    pub(super) fn open_in_incognito(&self, url: Url) {
        self.app.open_incognito_window(Some(url));
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
        })
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

    /// An Incognito window's: its title until its first tab has one, the default placement.
    pub fn incognito_window_options() -> WindowOptions {
        WindowOptions {
            title: "Snowlark (Incognito)".into(),
            ..WindowOptions::default()
        }
    }

    /// Opens a new Incognito window, at `url` or its new-tab page. Called on the window thread.
    pub fn open_incognito_window(self: &Rc<App>, url: Option<Url>) {
        let app = self.clone();
        let partition = self.off_the_record();
        let spares = self.spares();
        let options = App::incognito_window_options();
        if let Err(e) = window::open_browser_window(options, move |w| {
            Route::main(Browser::new(w, app, partition, url, spares))
        }) {
            eprintln!("[browser] no Incognito window: {e}");
        }
    }
}
