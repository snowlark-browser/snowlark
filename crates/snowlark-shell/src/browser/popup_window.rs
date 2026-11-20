// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Popup windows: a window a page opens with an opener and popup features (HTML "is popup":
//! `popup`, or `width`, `height`, `left`, `top`, or no `toolbar`/`menubar` and the like) shows in
//! a top-level window of its own with a minimal frame: Windows' own caption, a read-only location
//! line, no tab strip, toolbar or bars. It is sized from `width` and `height` (the viewport) and
//! placed from `left` and `top`, kept on the screen (CSSOM View "set up browsing context
//! features"); script moves and resizes it (`moveTo()`, `resizeTo()`: [`TabEvent::WindowRect`]).
//! Chromium's counterpart is a browser of the popup type, made for a new window whose disposition
//! is a popup (its window features), with a frame that has no tab strip and a read-only location
//! bar; a normal tabbed window ignores a page's moves.
//!
//! A popup window is one more window of the one [`Browser`], on the same UI thread: the fields
//! that are a window's ([`WindowFields`]: its compositor, its tab list of one tab, its size,
//! chrome state, menus, find box, developer tools and the like) trade places with the browser's
//! own around each call that serves it ([`Browser::with_window`]), so input, drawing, find, the
//! developer tools and the menus serve the popup's tab as they serve the main window's active
//! one. Its tab is a view of the opener's page ([`TabHost::view`]), served by the opener's
//! renderer, whose frames reach the main window's compositor, which forwards them to the popup
//! window's own ([`CompMsg::Forward`]). The tab's events reach the main window, which hands them
//! to the popup's window ([`PopupWindow::inbox`]). New tabs asked for from a popup window open in
//! the main window. Closing the popup window closes its tab, and its tab's closing (the page's
//! `window.close()`) closes the window. The popup outlives its opener's tab (its renderer goes on
//! serving it).

use snowlark_paint::window::{WindowAt, WindowHandle, WindowOptions, WindowProxy};

use super::*;
use crate::composite::CompInput;

/// The fields of a [`Browser`] that are one window's. The main window's are the browser's own;
/// each popup window keeps its own here ([`PopupWindow::fields`]), which trade places with the
/// browser's while a call serves that window ([`Browser::with_window`]).
macro_rules! window_fields {
    ($($name:ident: $ty:ty,)*) => {
        pub(super) struct WindowFields {
            $($name: $ty,)*
        }

        impl WindowFields {
            /// Trades places with the browser's own.
            fn trade(&mut self, b: &mut Browser) {
                $(std::mem::swap(&mut self.$name, &mut b.$name);)*
            }
        }
    };
}

window_fields! {
    comp: CompositorThread,
    tabs: TabList<TabEntry>,
    popup_window: bool,
    size: (u32, u32),
    dpi: u32,
    placement_px: Option<snowlark_paint::window::WindowPlacement>,
    maximized: bool,
    window_active: bool,
    hover: Hit,
    pressed: Hit,
    caption_hover: Option<CaptionButton>,
    focus: Focus,
    address: TextField,
    address_scroll: f32,
    side_scroll: f32,
    side_revealed: Option<(usize, usize)>,
    side_list: Option<snowlark_paint::DisplayList>,
    side_resize: Option<(f32, f32, Option<f32>)>,
    tab_drag: Option<side_tabs_ui::TabDrag>,
    address_drag: bool,
    caret_on: bool,
    suggestions: Vec<Suggestion>,
    selected: Option<usize>,
    hover_suggestion: Option<usize>,
    spinner: f32,
    spinning: bool,
    mouse: Point,
    page_hover: bool,
    scrollbar_drag: Option<f32>,
    scrollbar_hover: bool,
    chrome_dirty: bool,
    overlay_dirty: bool,
    sent_cursor: Cursor,
    menu: Option<SelectMenu>,
    swallow_release: bool,
    title: Option<String>,
    tooltip: Option<Tooltip>,
    tab_card: Option<crate::tab_card::TabCard>,
    sad_hover: bool,
    popup: Option<(MenuStack<Command>, Hit)>,
    dialog: Option<(Dialog<Command>, commands::DialogKind)>,
    bm_drag: Option<bookmark_drag::BookmarkDrag>,
    devtools_open: bool,
    dock: Dock,
    dock_menu: Option<crate::devtools::dock_menu::DockMenu>,
    attached: Option<TabId>,
    splitter_drag: Option<f32>,
    panel_hover: bool,
    site_info: Option<crate::site_info::SiteInfo>,
    blocker_panel: Option<crate::blocker_panel::BlockerPanel>,
    task_manager: Option<(
        crate::task_manager::TaskManager,
        task_manager_ui::TaskSampler,
    )>,
    autoscroll: Option<autoscroll::Session>,
    autoscroll_pending: Option<autoscroll::PendingPress>,
    restore_bubble: Option<crate::restore_bubble::RestoreBubble>,
    fullscreen: fullscreen::FullscreenUi,
}

impl WindowFields {
    /// A new popup window's: its compositor, its one tab, and the rest as a new window has it.
    fn popup(comp: CompositorThread, tab: TabEntry, handle: WindowHandle, dpi: u32) -> Self {
        let mut tabs = TabList::default();
        tabs.add(tab, Placement::End);
        WindowFields {
            comp,
            tabs,
            popup_window: true,
            size: handle.client_size(),
            dpi,
            placement_px: Some(handle.screen_placement()),
            maximized: false,
            window_active: !window::no_activate(),
            hover: Hit::None,
            pressed: Hit::None,
            caption_hover: None,
            focus: Focus::Page,
            address: TextField::default(),
            address_scroll: 0.0,
            side_scroll: 0.0,
            side_revealed: None,
            side_list: None,
            side_resize: None,
            tab_drag: None,
            address_drag: false,
            caret_on: true,
            suggestions: Vec::new(),
            selected: None,
            hover_suggestion: None,
            spinner: 0.0,
            spinning: false,
            mouse: Point::ZERO,
            page_hover: false,
            scrollbar_drag: None,
            scrollbar_hover: false,
            chrome_dirty: true,
            overlay_dirty: true,
            sent_cursor: Cursor::Arrow,
            menu: None,
            swallow_release: false,
            title: None,
            tooltip: None,
            tab_card: None,
            sad_hover: false,
            popup: None,
            dialog: None,
            bm_drag: None,
            devtools_open: false,
            // (a popup window's tools dock in it: the undocked tools' window is the main
            // window's)
            dock: Dock::Bottom,
            dock_menu: None,
            attached: None,
            splitter_drag: None,
            panel_hover: false,
            site_info: None,
            blocker_panel: None,
            task_manager: None,
            autoscroll: None,
            autoscroll_pending: None,
            restore_bubble: None,
            fullscreen: Default::default(),
        }
    }
}

/// A popup window of the browser's.
pub(super) struct PopupWindow {
    id: u64,
    proxy: WindowProxy,
    /// Its window's fields (`None` while they are the browser's: [`Browser::with_window`]).
    fields: Option<WindowFields>,
    /// Its tab.
    pub tab: TabId,
    /// Its tab's events, which the main window took from the browser's channel, for its turn.
    inbox: Vec<(TabId, TabEvent)>,
    /// It has not had a turn yet: its first shows its tab.
    fresh: bool,
    /// Its window went (it is removed once no call serves it).
    closed: bool,
}

/// The tabs of the windows whose fields `popups` holds: the windows the calls do not serve (with
/// the browser's own `tabs`, every tab).
pub(super) fn tabs_of(popups: &[PopupWindow]) -> impl Iterator<Item = &TabEntry> {
    popups
        .iter()
        .filter_map(|p| p.fields.as_ref())
        .flat_map(|f| f.tabs.iter())
}

/// Where a new popup window's frame goes when the page gives no `left` or `top`: this far right
/// of and below its opener window's (device-independent px).
const CASCADE: f32 = 40.0;

impl Browser {
    /// Runs `f` with popup window `id`'s fields in the browser's place (the calls serve that
    /// window); `None` (nothing run) when it is gone or a call already serves a popup window.
    pub(super) fn with_window<R>(
        &mut self,
        id: u64,
        f: impl FnOnce(&mut Browser) -> R,
    ) -> Option<R> {
        if self.current.is_some() {
            return None;
        }
        let k = self.popups.iter().position(|p| p.id == id && !p.closed)?;
        let mut w = self.popups[k].fields.take()?;
        w.trade(self);
        self.current = Some(id);
        let r = f(self);
        self.current = None;
        w.trade(self);
        if let Some(p) = self.popups.iter_mut().find(|p| p.id == id) {
            p.fields = Some(w);
        }
        Some(r)
    }

    /// The tab events of the window the calls serve: a popup window's from its inbox; the main
    /// window's from the browser's channel, from which a popup window's tab's go to that window's
    /// inbox (and wake it).
    pub(super) fn take_tab_events(&mut self) -> Vec<(TabId, TabEvent)> {
        if let Some(id) = self.current {
            return self
                .popups
                .iter_mut()
                .find(|p| p.id == id)
                .map(|p| std::mem::take(&mut p.inbox))
                .unwrap_or_default();
        }
        let mut mine = Vec::new();
        while let Ok((tab, event)) = self.events.try_recv() {
            if !self.tabs.iter().any(|e| e.tab.id == tab) {
                if let Some(p) = self.popups.iter_mut().find(|p| !p.closed && p.tab == tab) {
                    p.inbox.push((tab, event));
                    p.proxy.wake();
                    continue;
                }
            }
            mine.push((tab, event));
        }
        mine
    }

    /// Tab `opener`'s page opened window `window` as a popup window (`size`: its viewport in CSS
    /// px; `position`: where its frame's left and top edges are asked to be, in the screen's CSS
    /// px): a window of its own shows it, a view of that page in the opener's renderer.
    pub(super) fn open_popup_window(
        &mut self,
        opener: usize,
        window: u64,
        size: (f32, f32),
        position: (Option<i32>, Option<i32>),
    ) {
        let Some(o) = self.tabs.get(opener) else {
            return;
        };
        let zoom = o.zoom;
        let scale = self.scale();
        let px = |dips: f32| (dips * scale).round() as i32;
        // (the opener window's frame, moved in a little, where the page says nothing)
        let (ox, oy) = self
            .placement_px
            .map_or((px(CASCADE), px(CASCADE)), |((l, t, _, _), _)| {
                (l + px(CASCADE), t + px(CASCADE))
            });
        let at = WindowAt {
            left: position.0.map_or(ox, |x| px(x as f32)),
            top: position.1.map_or(oy, |y| px(y as f32)),
            client_width: px(size.0 * zoom),
            client_height: px(size.1 * zoom + chrome::POPUP_BAR_HEIGHT),
        };
        let options = WindowOptions {
            at: Some(at),
            ..WindowOptions::default()
        };
        let id = self.next_popup;
        self.next_popup += 1;
        let me = self.me.clone();
        let mut proxy = None;
        let handle = match window::open(options, |w| {
            proxy = Some(w.proxy());
            Route::popup(me, id)
        }) {
            Ok(h) => h,
            Err(e) => {
                // (no window of its own: a tab, as before)
                eprintln!("[browser] no popup window: {e}");
                self.open_window(opener, window);
                return;
            }
        };
        let proxy = proxy.unwrap_or_else(WindowProxy::detached);
        let dpi = scale_override().unwrap_or(handle.dpi());
        let comp = CompositorThread::start(
            self.tab_ctx.gpu.clone(),
            handle,
            self.fonts.engine().clone(),
            self.images.clone(),
            &self.windows,
            self.gpu_process.as_ref().map(|h| h.link()),
        );
        comp.send(CompMsg::Dark(self.chrome_dark()));
        if let Some(v) = &self.tab_ctx.videos {
            comp.send(CompInput::videos(v.clone()));
        }
        let tab_id = self.next_id;
        self.next_id += 1;
        // (what the opener's renderer sends for the tab reaches the main window's compositor,
        // which forwards it: before the renderer hears of the view)
        let forward = CompMsg::Forward {
            tab: tab_id,
            window: Some(handle.into()),
        };
        let _ = self.tab_ctx.compositor.send(forward.into());
        // (the window's own device follows at its first turn: `resized`)
        let device = Device {
            viewport: Size::new(size.0, size.1),
            scale: dpi as f32 / 96.0 * zoom,
            scrollbar: SCROLLBAR_WIDTH / zoom,
            screen: Default::default(),
        };
        let Some(o) = self.tabs.get(opener) else {
            return;
        };
        let tab = TabHost::view(
            tab_id,
            &o.tab,
            window,
            &comp.sender(),
            comp.scroll_table(),
            device,
        );
        let entry = self.view_entry(opener, tab);
        self.popups.push(PopupWindow {
            id,
            proxy: proxy.clone(),
            fields: Some(WindowFields::popup(comp, entry, handle, dpi)),
            tab: tab_id,
            inbox: Vec::new(),
            fresh: true,
            closed: false,
        });
        // (its first turn: what its window said while the browser was busy, and its tab shown)
        proxy.wake();
    }

    /// An event of popup window `id`'s; returns whether it was used.
    pub(super) fn popup_window_event(&mut self, id: u64, window: &Window, event: Event) -> bool {
        let used = match event {
            // (the user closes the window: its tab closes, and the window with it)
            Event::CloseRequested => {
                self.with_window(id, |b| {
                    if b.tabs.is_empty() {
                        window.close();
                    } else {
                        b.close_tab(0, window);
                    }
                });
                true
            }
            Event::Destroyed => {
                self.popup_window_gone(id);
                true
            }
            Event::EndSession => true,
            event => {
                let fresh = self
                    .popups
                    .iter_mut()
                    .find(|p| p.id == id)
                    .is_some_and(|p| std::mem::take(&mut p.fresh));
                self.with_window(id, |b| {
                    if fresh {
                        b.size = window.client_size();
                        b.placement_px = Some(window.screen_placement());
                        b.resized();
                        b.activated();
                    }
                    WindowHandler::event(b, window, event)
                })
                .unwrap_or(false)
            }
        };
        self.reap_popup_windows();
        used
    }

    /// Popup window `id`'s window went: its tab closes, and its compositor ends.
    fn popup_window_gone(&mut self, id: u64) {
        let Some(p) = self.popups.iter_mut().find(|p| p.id == id) else {
            return;
        };
        p.closed = true;
        let tab = p.tab;
        let fields = p.fields.take();
        let forward = CompMsg::Forward { tab, window: None };
        let _ = self.tab_ctx.compositor.send(forward.into());
        // (its tab closes: a view's close closes the page's window)
        drop(fields);
    }

    /// Removes the popup windows that went (once no call serves them).
    fn reap_popup_windows(&mut self) {
        if self.current.is_none() {
            self.popups.retain(|p| !p.closed);
        }
    }

    /// Tabs the popup window the calls serve asked for (a link opened in a new tab, Ctrl+T)
    /// open in the main window: queued for its turn, which this wakes.
    pub(super) fn open_in_main_window(&mut self, url: Url, placement: Placement) {
        self.main_opens.push((url, placement));
        self.main_proxy.wake();
    }

    /// The main window's turn: the tabs popup windows asked for.
    pub(super) fn open_main_window_tabs(&mut self) {
        if self.current.is_some() {
            return;
        }
        for (url, placement) in std::mem::take(&mut self.main_opens) {
            self.open_tab(url, placement);
        }
    }

    /// Script asked to move or resize the popup window the calls serve to `rect` (left, top,
    /// width, height on the screen, in device-independent px). A normal window is not moved by
    /// its pages.
    pub(super) fn move_popup_window(&mut self, window: &Window, rect: (i32, i32, i32, i32)) {
        if !self.popup_window {
            return;
        }
        let s = self.scale();
        let px = |v: i32| (v as f32 * s).round() as i32;
        window.set_outer_rect((px(rect.0), px(rect.1), px(rect.2), px(rect.3)));
    }
}
