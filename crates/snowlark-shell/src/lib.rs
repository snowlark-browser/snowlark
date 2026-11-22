// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! snowlark-shell: the browser window.
//!
//! Threads (ARCHITECTURE.md's "Threads"):
//!
//! - The window's thread runs the Win32 message loop ([`snowlark_paint::window`]) and the
//!   [`browser::Browser`]: the tab strip and toolbar ([`chrome`]), the address bar ([`address`]),
//!   history ([`history`]), input routing. It never waits for a tab or for the screen.
//! - The compositor thread ([`composite`]) owns the DirectComposition layers, every tab's latest
//!   tiled frame and the viewport scroll positions; it scrolls by moving tiles and presents at the
//!   display's rate while anything moves.
//! - One thread per tab ([`tab`]) owns the tab's [`driver::TabDriver`] (the document: by default
//!   [`page_driver::PageDriver`] over the page crate's `Page`, with scripts;
//!   [`renderer_driver::RendererDriver`] renders without them) and a tile rasterizer. The window
//!   holds the other end, the [`tab_host::TabHost`], with the session history ([`session`]).
//!
//! All `unsafe` (Win32, DirectComposition, DXGI) is in snowlark-paint; this crate has none.

#![forbid(unsafe_code)]

pub use snowlark_tab::{driver, page_driver, renderer_driver, tab, video, watchdog};

pub mod about;
pub mod address;
pub mod audio;
pub mod begin_frames;
pub mod blocker_panel;
pub mod bookmarks;
pub mod bookmarks_page;
pub mod browser;
pub mod cdm;
pub mod chrome;
pub mod clear_data;
pub mod composite;
pub mod crash;
pub mod credits;
pub mod devtools;
pub mod dialog;
pub mod downloads;
pub mod downloads_page;
pub mod find_bar;
mod gpu_compositor;
pub mod gpu_process;
pub mod history;
pub mod history_page;
pub mod incognito_page;
pub mod local_storage;
pub mod menu;
pub mod partition;
pub mod popup;
pub mod profile;
#[cfg(test)]
mod protocol_tests;
pub mod restore;
pub mod restore_bubble;
pub mod scroll;
pub mod session;
pub mod settings;
pub mod side_tabs;
pub mod site_info;
pub mod tab_card;
pub mod tab_host;
#[cfg(test)]
mod tab_tests;
pub mod tabs;
pub mod task_manager;
pub mod text_area;
pub mod tooltip;
pub mod view_source;
pub mod widgets;
pub mod window_state;

use snowlark_paint::window::{self, WindowOptions};
pub use tab_host::ProcessModel;

/// How the browser's first window opens (the command line's switches).
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowFlags {
    /// The windows composite in a GPU process (`--gpu-process`, `gpu_process::wanted`).
    pub gpu_process: bool,
    /// The first window is an Incognito window (`--incognito`).
    pub incognito: bool,
}

/// Opens the browser window (at `start`, else its new tab page or the startup page) with its tabs'
/// renderers where `model` says, and runs it until its last window is closed. Returns the process
/// exit code.
pub fn run(start: Option<&str>, model: ProcessModel, flags: WindowFlags) -> i32 {
    let WindowFlags {
        gpu_process,
        incognito,
    } = flags;
    let url = match start {
        Some(text) => match address::interpret(text) {
            Some(u) => Some(u),
            None => {
                eprintln!("not an address: {text}");
                return 2;
            }
        },
        None => None,
    };
    // (a profile from before the rename moves to today's place, before anything opens it)
    profile::adopt_old_profile();
    let profile = profile::Profile::default_location();
    // Every panic, caught or not, goes to the profile's crash log first.
    crash::install_panic_hook(profile.dir.join("crash.log"));
    // Where the last run of this profile left the window (the window checks it against the monitors).
    // (an Incognito window opens there too, as a regular one would; it keeps no placement)
    let placement = window_state::load(&profile.window_file());
    let options = if incognito {
        browser::incognito::App::incognito_window_options(placement)
    } else {
        WindowOptions {
            placement,
            ..WindowOptions::default()
        }
    };
    // A renderer process warms up while the window opens (`--process-model=tab`): the first tab
    // takes it.
    let dumps = profile.crash_dumps_dir();
    let gpu = gpu_process::wanted(model, gpu_process).then(|| {
        let dumps = dumps.clone();
        gpu_process::GpuHost::start(move || gpu_process::gpu_launch(&dumps))
    });
    let spares = tab_host::spare_pool(model, move || tab_host::renderer_launch(None, &dumps));
    // (what every window shares: the regular partition, the content blocker, the GPU process)
    let app = browser::incognito::App::new(profile, model, gpu);
    let first = app.clone();
    let result = window::run(options, move |w| {
        let partition = if incognito {
            first.off_the_record()
        } else {
            first.regular.clone()
        };
        browser::Route::main(browser::Browser::new(w, first, partition, url, spares))
    });
    // (the regular partition's last writes reach the profile)
    drop(app);
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("window: {e}");
            1
        }
    }
}
