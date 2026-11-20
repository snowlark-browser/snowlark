// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's side of the side tab panel: whether it shows and how wide, the points that are on
//! it, scrolling its list, and switching between it and the tab strip.

use snowlark_paint::Point;

use super::Browser;
use crate::chrome::{self, Hit, Layout};
use crate::side_tabs::{self, SideSpec};

/// The least room the panel leaves the page (DIPs).
const PAGE_MIN: f32 = 200.0;
/// How far the pointer moves from a press on a tab before the tab is dragged (DIPs).
const DRAG_START: f32 = 10.0;
/// How far the list scrolls for each pointer move past its edge during a drag (DIPs).
const DRAG_SCROLL: f32 = 8.0;

/// A tab pressed in the side panel, which becomes a drag once the pointer has moved far enough.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct TabDrag {
    /// The tab's place now (it moves in the list as the drag passes the others).
    index: usize,
    press: Point,
    /// The pointer's distance below the row's top at the press.
    grab: f32,
    started: bool,
}

impl Browser {
    /// The tabs show in the side panel (never in fullscreen, where there is no chrome).
    pub(super) fn side_tabs_on(&self) -> bool {
        self.settings.vertical_tabs && !self.fullscreen.on() && !self.popup_window
    }

    /// The panel's width (DIPs; 0 when it does not show).
    pub(super) fn side_width(&self) -> f32 {
        if !self.side_tabs_on() {
            return 0.0;
        }
        let w = match self.side_resize {
            // (during a resize, the width under the pointer)
            Some((_, _, live)) => live,
            None if self.settings.vertical_tabs_collapsed => None,
            None => Some(self.settings.vertical_tabs_width),
        };
        let Some(w) = w else {
            return side_tabs::COLLAPSED_WIDTH;
        };
        let w = w.clamp(side_tabs::MIN_WIDTH, side_tabs::MAX_WIDTH);
        w.min((self.width_dips() - PAGE_MIN).max(side_tabs::MIN_WIDTH))
            .round()
    }

    pub(super) fn side_spec(&self) -> SideSpec {
        SideSpec {
            width: self.side_width(),
            height: self.size.1 as f32 / self.scale(),
            scroll: self.side_scroll,
            collapsed: self.side_width() == side_tabs::COLLAPSED_WIDTH,
            active: self.tabs.active_index(),
            drag: self
                .tab_drag
                .filter(|d| d.started)
                .map(|d| (d.index, self.mouse.y - d.grab)),
        }
    }

    /// A press on tab `i`'s row: it may become a drag.
    pub(super) fn side_drag_press(&mut self, i: usize, dip: Point) {
        if let Some(r) = self.layout().tabs.get(i) {
            self.tab_drag = Some(TabDrag {
                index: i,
                press: dip,
                grab: dip.y - r.y,
                started: false,
            });
        }
    }

    /// The pointer moved with a tab pressed: past the start distance, the row follows it and the
    /// tab moves among the others as its row passes their middles; past the list's ends the list
    /// scrolls. True while a drag goes on (the move is the drag's).
    pub(super) fn side_drag_move(&mut self, dip: Point) -> bool {
        let Some(mut d) = self.tab_drag else {
            return false;
        };
        if !d.started {
            let (dx, dy) = (dip.x - d.press.x, dip.y - d.press.y);
            if (dx * dx + dy * dy).sqrt() <= DRAG_START {
                return false;
            }
            d.started = true;
            self.tab_card = None;
            self.tooltip = None;
            self.hover = Hit::None;
            self.pressed = Hit::None;
            self.overlay_dirty = true;
        }
        if let Some(side) = self.layout().side {
            if dip.y < side.list.y {
                self.side_scroll = (side.scroll - DRAG_SCROLL).max(0.0);
            } else if dip.y > side.list.bottom() {
                self.side_scroll = (side.scroll + DRAG_SCROLL).min(side.max_scroll);
            }
        }
        if let Some(side) = self.layout().side {
            let to = side.drop_index(dip.y - d.grab, self.tabs.len());
            if to != d.index && d.index < self.tabs.len() {
                self.tabs.move_tab(d.index, to);
                d.index = to;
            }
        }
        self.tab_drag = Some(d);
        self.chrome_dirty = true;
        true
    }

    /// The button is let go: a drag ends with the tab where it is. True when a drag ended (the
    /// release is the drag's).
    pub(super) fn side_drag_end(&mut self, dip: Point) -> bool {
        let Some(d) = self.tab_drag.take() else {
            return false;
        };
        if !d.started {
            return false;
        }
        self.pressed = Hit::None;
        self.side_revealed = None;
        self.hover = if self.in_chrome(dip) {
            self.layout().hit(dip)
        } else {
            Hit::None
        };
        self.chrome_dirty = true;
        true
    }

    /// The collapse button: favicons only, or titles again.
    pub(super) fn toggle_side_collapsed(&mut self) {
        self.settings.vertical_tabs_collapsed = !self.settings.vertical_tabs_collapsed;
        self.save_settings();
        self.side_width_changed();
    }

    /// A press on the panel's edge starts a resize.
    pub(super) fn side_resize_start(&mut self, dip: Point) {
        let w = self.side_width();
        let live = (w != side_tabs::COLLAPSED_WIDTH).then_some(w);
        self.side_resize = Some((w, dip.x, live));
        self.tab_card = None;
    }

    /// The edge is dragged: the panel follows (collapsing when dragged narrow enough, expanding
    /// again when dragged back out). True while a resize goes on.
    pub(super) fn side_resize_move(&mut self, dip: Point) -> bool {
        let Some((start, x, live)) = self.side_resize else {
            return false;
        };
        let now = side_tabs::resized(start, dip.x - x);
        if now != live {
            self.side_resize = Some((start, x, now));
            self.side_width_changed();
        }
        true
    }

    /// The edge is let go: the width is kept. True when a resize ended.
    pub(super) fn side_resize_end(&mut self, dip: Point) -> bool {
        let Some((_, _, live)) = self.side_resize.take() else {
            return false;
        };
        // Kept when the drag ends: collapsed, or the width with titles (a drag that collapses the
        // panel keeps the width it had, for the expand button).
        match live {
            None => self.settings.vertical_tabs_collapsed = true,
            Some(w) => {
                self.settings.vertical_tabs_collapsed = false;
                self.settings.vertical_tabs_width = w;
            }
        }
        self.pressed = Hit::None;
        self.save_settings();
        self.hover = if self.in_chrome(dip) {
            self.layout().hit(dip)
        } else {
            Hit::None
        };
        self.chrome_dirty = true;
        true
    }

    /// The panel's width changed: the page area moves (no change to the chrome's height).
    fn side_width_changed(&mut self) {
        self.side_revealed = None;
        self.tab_card = None;
        self.relayout_page();
    }

    /// Whether a window DIP point is on the chrome: the tab strip and toolbar rows, or the side
    /// panel.
    pub(super) fn in_chrome(&self, dip: Point) -> bool {
        dip.y < self.chrome_h() || dip.x < self.side_width()
    }

    /// The wheel over the panel scrolls its list (true when the point is on the panel).
    pub(super) fn side_wheel(&mut self, dip: Point, dy: i32) -> bool {
        let layout = self.layout();
        let Some(side) = layout.side.as_ref().filter(|s| s.rect.contains(dip)) else {
            return false;
        };
        let to =
            (side.scroll - dy as f32 / 120.0 * side_tabs::WHEEL_STEP).clamp(0.0, side.max_scroll);
        if to != self.side_scroll {
            self.side_scroll = to;
            self.chrome_dirty = true;
            // (the list moved under the pointer)
            self.hover = self.layout().hit(dip);
        }
        true
    }

    /// Before drawing: when the active tab or the number of tabs changed, the list scrolls so the
    /// active tab shows.
    pub(super) fn reveal_active_side_tab(&mut self) {
        if !self.side_tabs_on() || self.tab_drag.is_some_and(|d| d.started) {
            return;
        }
        let now = (self.tabs.active_index(), self.tabs.len());
        if self.side_revealed == Some(now) {
            return;
        }
        self.side_revealed = Some(now);
        if let Some(side) = self.layout().side {
            let s = side.reveal(now.0);
            if s != self.side_scroll {
                self.side_scroll = s;
                self.chrome_dirty = true;
            }
        }
    }

    /// Switches between the side panel and the tab strip (the tab menu's last item), and keeps the
    /// choice.
    pub(super) fn toggle_vertical_tabs(&mut self) {
        self.settings.vertical_tabs = !self.settings.vertical_tabs;
        self.save_settings();
        self.side_layout_changed();
        self.refresh_settings_pages();
    }

    /// The panel showed, hid or changed width: the page area changes.
    pub(super) fn side_layout_changed(&mut self) {
        self.side_revealed = None;
        self.hover = Hit::None;
        self.close_popup();
        self.tab_card = None;
        self.resized();
        self.chrome_dirty = true;
        self.overlay_dirty = true;
    }

    /// The panel of `layout` (when it has one) for the overlay, in its (content-area) coordinates.
    pub(super) fn side_panel_list(
        &self,
        view: &chrome::ChromeView,
        layout: &Layout,
    ) -> Option<snowlark_paint::DisplayList> {
        layout.side.as_ref().map(|_| {
            let mut b = snowlark_paint::DisplayListBuilder::new();
            b.push_transform(snowlark_paint::Matrix::translation(0.0, -layout.height()));
            side_tabs::paint(&mut b, view, layout, &self.fonts);
            b.pop_transform();
            b.build()
        })
    }

    /// Keeps the panel's list for the overlay.
    pub(super) fn set_side_list(&mut self, list: Option<snowlark_paint::DisplayList>) {
        // (the chrome changes more often than the panel: the address bar's caret)
        if list != self.side_list {
            self.side_list = list;
            self.overlay_dirty = true;
        }
    }
}
