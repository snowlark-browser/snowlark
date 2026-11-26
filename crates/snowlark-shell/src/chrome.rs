// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's chrome, drawn by us: the tab strip (which is the title bar, with the caption
//! buttons), the toolbar with the address bar, and the overlays over the page (the status bubble,
//! address suggestions). Everything is laid out in DIPs ([`Layout`]), hit-tested ([`Layout::hit`])
//! and built as an ordinary display list ([`build`], [`build_overlay`]) that the compositor paints
//! with the same painter as page content. Text is shaped with DirectWrite through the paint crate's
//! [`TextEngine`]; icons come from Segoe Fluent Icons (Segoe MDL2 Assets on Windows 10).

use std::sync::Arc;

use snowlark_paint::window::CaptionButton;
use snowlark_paint::{
    Color, DisplayList, DisplayListBuilder, FontListId, FontQuery, FontStretch, FontStyle, ImageId,
    ImageRendering, Matrix, PathEl, Point, Radii, Rect, ShapeOptions, ShapedText, Size, TextEngine,
};

use crate::address::TextField;
use crate::side_tabs::{SideLayout, SideSpec};
use crate::tabs::tab_width;

pub const TAB_STRIP_HEIGHT: f32 = 40.0;
pub const TOOLBAR_HEIGHT: f32 = 40.0;
/// The chrome's height above the page without the bookmarks bar (DIPs).
pub const CHROME_HEIGHT: f32 = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT;
/// The bookmarks bar's height (DIPs).
pub const BOOKMARKS_BAR_HEIGHT: f32 = 30.0;
/// A popup window's chrome: its location line alone (under Windows' own caption).
pub const POPUP_BAR_HEIGHT: f32 = 34.0;
/// The widest a bookmark's title shows on the bar.
pub const BOOKMARK_MAX_TEXT: f32 = 160.0;
const CAPTION_WIDTH: f32 = 46.0;
const CAPTION_HEIGHT: f32 = 32.0;
const TAB_MIN: f32 = 56.0;
const TAB_MAX: f32 = 240.0;
const TAB_TOP: f32 = 6.0;
const BUTTON: f32 = 32.0;
const UI_SIZE: f32 = 12.0;
const ADDRESS_SIZE: f32 = 14.0;
const ICON_SIZE: f32 = 16.0;
/// The tab's audio indicator: its glyph, scaled to the tab title's font, and the round button
/// around it.
pub(crate) const AUDIO_GLYPH: f32 = UI_SIZE * 7.0 / 6.0;
const AUDIO_BUTTON: f32 = 20.0;
pub const SUGGESTION_ROW: f32 = 34.0;

/// Segoe Fluent Icons / MDL2 code points.
pub(crate) mod glyph {
    pub const BACK: char = '\u{E72B}';
    pub const FORWARD: char = '\u{E72A}';
    pub const REFRESH: char = '\u{E72C}';
    pub const STOP: char = '\u{E711}';
    pub const HOME: char = '\u{E80F}';
    pub const ADD: char = '\u{E710}';
    pub const CLOSE: char = '\u{E8BB}';
    pub const MINIMIZE: char = '\u{E921}';
    pub const MAXIMIZE: char = '\u{E922}';
    pub const RESTORE: char = '\u{E923}';
    pub const GLOBE: char = '\u{E774}';
    pub const LOCK: char = '\u{E72E}';
    pub const INFO: char = '\u{E946}';
    pub const SEARCH: char = '\u{E721}';
    pub const HISTORY: char = '\u{E81C}';
    pub const SAD: char = '\u{E783}';
    pub const CHECK: char = '\u{E73E}';
    pub const CHEVRON_RIGHT: char = '\u{E76C}';
    pub const CHEVRON_DOWN: char = '\u{E70D}';
    pub const CHEVRON_UP: char = '\u{E70E}';
    pub const STAR: char = '\u{E734}';
    pub const STAR_FILL: char = '\u{E735}';
    pub const MORE: char = '\u{E712}';
    pub const DOWNLOAD: char = '\u{E896}';
    pub const FOLDER: char = '\u{E8B7}';
    pub const SETTINGS: char = '\u{E713}';
    pub const CODE: char = '\u{E943}';
    pub const COPY: char = '\u{E8C8}';
    pub const SAVE: char = '\u{E74E}';
    pub const DELETE: char = '\u{E74D}';
    pub const EDIT: char = '\u{E70F}';
    pub const PICKER: char = '\u{E8B0}';
    pub const CLEAR: char = '\u{E894}';
    /// A gauge (Segoe Fluent Icons "Speed high"): memory use.
    pub const GAUGE: char = '\u{E9D9}';
    pub const DOCK_LEFT: char = '\u{E90C}';
    pub const DOCK_BOTTOM: char = '\u{E90E}';
    pub const DOCK_RIGHT: char = '\u{E90D}';
    /// Segoe "Open in new window": undock into a separate window.
    pub const UNDOCK: char = '\u{E8A7}';
    pub const WARNING: char = '\u{E7BA}';
    pub const ERROR: char = '\u{EA39}';
    /// A speaker with sound waves (a tab playing audio).
    pub const VOLUME: char = '\u{E767}';
    /// A crossed-out speaker (a muted tab).
    pub const MUTE: char = '\u{E74F}';
    /// A pane with an arrow into its left edge, and out of it (the side tab panel's collapse and
    /// expand buttons).
    pub const PANE_ARROW_LEFT: char = '\u{E8A0}';
    pub const PANE_ARROW_RIGHT: char = '\u{E89F}';

    /// The Unicode symbol drawn for an icon where the icon font is missing (on Linux), found by the
    /// text engine's fallback among the system's fonts (the lock and the shield only in colour
    /// emoji fonts). A code point without one stays as it is. Vector icons of our own are to
    /// replace both.
    pub fn symbol(c: char) -> char {
        match c {
            BACK => '\u{2190}',
            FORWARD => '\u{2192}',
            REFRESH => '\u{21BB}',
            STOP | CLOSE => '\u{2715}',
            HOME => '\u{2302}',
            ADD => '+',
            MINIMIZE => '\u{2013}',
            MAXIMIZE => '\u{25A1}',
            RESTORE => '\u{2750}',
            GLOBE => '\u{25CE}',
            LOCK => '\u{1F512}',
            INFO => '\u{2139}',
            // (the blocker's shield, the task manager's monitor)
            '\u{EA18}' => '\u{1F6E1}',
            '\u{E7F4}' => '\u{2395}',
            SEARCH => '\u{2315}',
            HISTORY => '\u{21BA}',
            SAD => '\u{2639}',
            CHECK => '\u{2713}',
            CHEVRON_RIGHT => '\u{203A}',
            CHEVRON_DOWN => '\u{2304}',
            CHEVRON_UP => '\u{2303}',
            STAR => '\u{2606}',
            STAR_FILL => '\u{2605}',
            MORE => '\u{22EF}',
            DOWNLOAD => '\u{2913}',
            FOLDER => '\u{25AD}',
            SETTINGS => '\u{2699}',
            CODE => '\u{2039}',
            COPY => '\u{2398}',
            SAVE => '\u{2913}',
            DELETE => '\u{232B}',
            EDIT => '\u{270E}',
            PICKER => '\u{2316}',
            CLEAR => '\u{2327}',
            GAUGE => '\u{25D4}',
            DOCK_LEFT => '\u{25E7}',
            DOCK_BOTTOM => '\u{2B13}',
            DOCK_RIGHT => '\u{25E8}',
            UNDOCK => '\u{2197}',
            WARNING => '\u{26A0}',
            ERROR => '\u{2297}',
            VOLUME => '\u{266A}',
            MUTE => '\u{2298}',
            PANE_ARROW_LEFT => '\u{21E4}',
            PANE_ARROW_RIGHT => '\u{21E5}',
            other => other,
        }
    }

    #[cfg(test)]
    #[test]
    fn every_icon_has_a_symbol_outside_the_private_use_area() {
        let all = [
            BACK,
            FORWARD,
            REFRESH,
            STOP,
            HOME,
            ADD,
            CLOSE,
            MINIMIZE,
            MAXIMIZE,
            RESTORE,
            GLOBE,
            LOCK,
            INFO,
            SEARCH,
            HISTORY,
            SAD,
            CHECK,
            CHEVRON_RIGHT,
            CHEVRON_DOWN,
            CHEVRON_UP,
            STAR,
            STAR_FILL,
            MORE,
            DOWNLOAD,
            FOLDER,
            SETTINGS,
            CODE,
            COPY,
            SAVE,
            DELETE,
            EDIT,
            PICKER,
            CLEAR,
            GAUGE,
            DOCK_LEFT,
            DOCK_BOTTOM,
            DOCK_RIGHT,
            UNDOCK,
            WARNING,
            ERROR,
            VOLUME,
            MUTE,
            PANE_ARROW_LEFT,
            PANE_ARROW_RIGHT,
        ];
        for c in all {
            assert!(
                !('\u{E000}'..='\u{F8FF}').contains(&symbol(c)),
                "{:X}",
                c as u32
            );
        }
    }
}

/// The chrome's colors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub frame: Color,
    pub frame_inactive: Color,
    pub tab_active: Color,
    pub tab_hover: Color,
    pub text: Color,
    pub text_dim: Color,
    pub toolbar: Color,
    pub field: Color,
    pub field_hover: Color,
    pub field_focused: Color,
    pub accent: Color,
    pub button_hover: Color,
    pub button_pressed: Color,
    pub separator: Color,
    pub selection: Color,
    pub close_hover: Color,
    pub bubble: Color,
    pub bubble_border: Color,
    pub dropdown: Color,
    pub dropdown_hover: Color,
}

pub fn theme(dark: bool) -> Theme {
    if dark {
        Theme {
            dark,
            frame: Color::hex(0x1F2023),
            frame_inactive: Color::hex(0x292A2D),
            tab_active: Color::hex(0x35363A),
            tab_hover: Color::hex(0x2B2C30),
            text: Color::hex(0xE8EAED),
            text_dim: Color::hex(0x9AA0A6),
            toolbar: Color::hex(0x35363A),
            field: Color::hex(0x202124),
            field_hover: Color::hex(0x292A2D),
            field_focused: Color::hex(0x202124),
            accent: Color::hex(0x8AB4F8),
            button_hover: Color::rgba(255, 255, 255, 26),
            button_pressed: Color::rgba(255, 255, 255, 46),
            separator: Color::hex(0x4A4C50),
            selection: Color::hex(0x2E5593),
            close_hover: Color::hex(0xC42B1C),
            bubble: Color::hex(0x202124),
            bubble_border: Color::hex(0x4A4C50),
            dropdown: Color::hex(0x2B2C30),
            dropdown_hover: Color::hex(0x3C3D41),
        }
    } else {
        Theme {
            dark,
            frame: Color::hex(0xDEE1E6),
            frame_inactive: Color::hex(0xE8EAED),
            tab_active: Color::hex(0xFFFFFF),
            tab_hover: Color::hex(0xEBEDF0),
            text: Color::hex(0x202124),
            text_dim: Color::hex(0x5F6368),
            toolbar: Color::hex(0xFFFFFF),
            field: Color::hex(0xF1F3F4),
            field_hover: Color::hex(0xE8EAED),
            field_focused: Color::hex(0xFFFFFF),
            accent: Color::hex(0x1A73E8),
            button_hover: Color::rgba(0, 0, 0, 18),
            button_pressed: Color::rgba(0, 0, 0, 36),
            separator: Color::hex(0xDADCE0),
            selection: Color::hex(0xB4D5FE),
            close_hover: Color::hex(0xC42B1C),
            bubble: Color::hex(0xF8F9FA),
            bubble_border: Color::hex(0xDADCE0),
            dropdown: Color::hex(0xFFFFFF),
            dropdown_hover: Color::hex(0xF1F3F4),
        }
    }
}

/// An Incognito window's colours, whatever the theme: dark, in neutral greys with no tint (the
/// toolbar a lighter grey than the regular dark theme's, the frame almost black), so the window
/// is not mistaken for a regular one, and its new tab page ([`crate::incognito_page`]) is the
/// toolbar's colour.
pub fn incognito_theme() -> Theme {
    Theme {
        frame: Color::hex(0x1F2020),
        frame_inactive: Color::hex(0x2B2B2B),
        tab_active: Color::hex(0x3C3C3C),
        tab_hover: Color::hex(0x343535),
        text: Color::hex(0xE3E3E3),
        text_dim: Color::hex(0xABABAB),
        toolbar: Color::hex(0x3C3C3C),
        field: Color::hex(0x282828),
        field_hover: Color::hex(0x303030),
        field_focused: Color::hex(0x282828),
        accent: Color::hex(0xA8C7FA),
        separator: Color::hex(0x474747),
        selection: Color::hex(0x0842A0),
        bubble: Color::hex(0x282828),
        bubble_border: Color::hex(0x474747),
        dropdown: Color::hex(0x303030),
        dropdown_hover: Color::hex(0x474747),
        ..theme(true)
    }
}

/// The Incognito button's label: "Incognito", with the count of open Incognito windows when
/// there is more than one.
pub fn incognito_label(windows: usize) -> String {
    if windows > 1 {
        format!("Incognito ({windows})")
    } else {
        "Incognito".into()
    }
}

/// The Incognito button's tooltip.
pub const INCOGNITO_TOOLTIP: &str = "You\u{2019}re Incognito";

/// The Incognito button's size for `label` (its mark, the label, the padding).
pub fn incognito_button_width(fonts: &Fonts, label: &str) -> f32 {
    10.0 + INCOGNITO_MARK + 6.0 + fonts.width(label, INCOGNITO_TEXT) + 12.0
}

const INCOGNITO_MARK: f32 = 18.0;
const INCOGNITO_TEXT: f32 = 12.5;

/// The Incognito mark (the same as the new tab page's, [`crate::incognito_page::MASK_SVG`]): a
/// mask with two eye holes, `size` wide, centred on `c`; filled even-odd.
pub fn incognito_mark(c: Point, size: f32) -> Vec<PathEl> {
    // (the 24 x 24 box of the page's SVG)
    let s = size / 24.0;
    let p = |x: f32, y: f32| Point::new(c.x + (x - 12.0) * s, c.y + (y - 12.0) * s);
    let mut path = vec![
        PathEl::MoveTo(p(2.0, 8.5)),
        PathEl::CurveTo(p(7.0, 6.0), p(17.0, 6.0), p(22.0, 8.5)),
        PathEl::CurveTo(p(22.0, 14.5), p(18.5, 17.0), p(15.5, 16.5)),
        PathEl::CurveTo(p(13.5, 16.2), p(13.0, 13.8), p(12.0, 13.8)),
        PathEl::CurveTo(p(11.0, 13.8), p(10.5, 16.2), p(8.5, 16.5)),
        PathEl::CurveTo(p(5.5, 17.0), p(2.0, 14.5), p(2.0, 8.5)),
        PathEl::Close,
    ];
    // (the eyes: ellipses, each four quarter curves)
    const K: f32 = 0.552_284_8;
    for cx in [7.5, 16.5] {
        let (cy, rx, ry) = (11.5, 2.4, 1.5);
        path.extend([
            PathEl::MoveTo(p(cx + rx, cy)),
            PathEl::CurveTo(
                p(cx + rx, cy + K * ry),
                p(cx + K * rx, cy + ry),
                p(cx, cy + ry),
            ),
            PathEl::CurveTo(
                p(cx - K * rx, cy + ry),
                p(cx - rx, cy + K * ry),
                p(cx - rx, cy),
            ),
            PathEl::CurveTo(
                p(cx - rx, cy - K * ry),
                p(cx - K * rx, cy - ry),
                p(cx, cy - ry),
            ),
            PathEl::CurveTo(
                p(cx + K * rx, cy - ry),
                p(cx + rx, cy - K * ry),
                p(cx + rx, cy),
            ),
            PathEl::Close,
        ]);
    }
    path
}

/// What a chrome point is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    None,
    Tab(usize),
    TabClose(usize),
    /// A tab's audio indicator (mutes and unmutes the tab).
    TabAudio(usize),
    NewTab,
    Back,
    Forward,
    Reload,
    Home,
    Address,
    /// The site information button at the start of the address bar.
    SiteInfo,
    /// The bookmark star at the end of the address bar.
    Star,
    /// The app menu button.
    Menu,
    /// The content blocker's button (its panel, the blocked count).
    Shield,
    /// An Incognito window's Incognito button.
    Incognito,
    /// The downloads button (while downloads are recent).
    Downloads,
    /// A bookmark (or folder) on the bookmarks bar.
    Bookmark(usize),
    /// The "»" button with the bookmarks that don't fit.
    BookmarkOverflow,
    /// The bookmarks bar where nothing is.
    BookmarksBar,
    Minimize,
    Maximize,
    Close,
    /// Empty tab strip: drags the window.
    Caption,
    /// The side tab panel where nothing is.
    TabPanel,
    /// The side tab panel's collapse (expand) button.
    SideCollapse,
    /// The side tab panel's trailing edge, which resizes it.
    SideEdge,
}

/// Where everything is (DIPs).
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub width: f32,
    pub tabs: Vec<Rect>,
    pub new_tab: Rect,
    pub minimize: Rect,
    pub maximize: Rect,
    pub close: Rect,
    pub back: Rect,
    pub forward: Rect,
    pub reload: Rect,
    pub home: Rect,
    pub address: Rect,
    /// The address text's left edge and the room it has.
    pub address_text: Rect,
    pub site_info: Rect,
    pub star: Rect,
    pub menu: Rect,
    /// The content blocker's button, before the menu.
    pub shield: Rect,
    /// An Incognito window's Incognito button, between the content blocker's and the menu
    /// ([`Layout::with_incognito`]).
    pub incognito: Option<Rect>,
    /// The downloads button, after the content blocker's ([`Layout::with_downloads`]).
    pub downloads: Option<Rect>,
    /// The bookmarks bar, when it shows.
    pub bar: Option<BarLayout>,
    /// Which tabs show an audio indicator ([`Layout::with_audio`]).
    pub audio: Vec<bool>,
    /// The window is maximized ([`Layout::with_maximized`]): the tabs reach the screen's top edge.
    pub maximized: bool,
    /// The window is fullscreen ([`Layout::with_fullscreen`]): there is no chrome at all.
    pub fullscreen: bool,
    /// The tabs are in a panel at the left ([`Layout::with_side_tabs`]) instead of the strip.
    pub side: Option<SideLayout>,
    /// A popup window's chrome ([`Layout::popup`]): the location line alone.
    pub popup: bool,
}

/// The space left of the caption buttons that drags the window when the tabs are at the side.
const GRAB: f32 = 40.0;

/// How far into a folder (on the bar: at each side; in a drop-down: above and below its row) a
/// drop still goes before or after it instead of into it.
pub const DROP_EDGE: f32 = 5.0;

/// Where the bookmarks bar's items are.
#[derive(Clone, Debug, PartialEq)]
pub struct BarLayout {
    pub rect: Rect,
    /// The items that fit, in order.
    pub items: Vec<Rect>,
    /// The "»" button when some don't fit (they are the items from `items.len()` on).
    pub overflow: Option<Rect>,
}

impl BarLayout {
    /// Lays out items `widths` wide on a bar at `y`, `width` wide.
    pub fn new(width: f32, y: f32, widths: &[f32]) -> BarLayout {
        let rect = Rect::new(0.0, y, width, BOOKMARKS_BAR_HEIGHT);
        let chevron = Rect::new(width - 34.0, y + 3.0, 26.0, BOOKMARKS_BAR_HEIGHT - 6.0);
        let mut items = Vec::new();
        let mut x = 8.0;
        for (i, &w) in widths.iter().enumerate() {
            // (the last item may use the chevron's room)
            let limit = if i + 1 == widths.len() {
                width - 8.0
            } else {
                chevron.x - 4.0
            };
            if x + w > limit {
                break;
            }
            items.push(Rect::new(x, y + 3.0, w, BOOKMARKS_BAR_HEIGHT - 6.0));
            x += w + 2.0;
        }
        let overflow = (items.len() < widths.len()).then_some(chevron);
        BarLayout {
            rect,
            items,
            overflow,
        }
    }

    /// Where a dragged item dropped at `x` goes: the index before which it is inserted, or the
    /// folder (item index) it drops into when over a folder but for [`DROP_EDGE`] at each end.
    pub fn drop_target(&self, x: f32, folders: &[bool]) -> (usize, Option<usize>) {
        for (i, r) in self.items.iter().enumerate() {
            if x < r.right() {
                let into = folders.get(i).copied().unwrap_or(false)
                    && x > r.x + DROP_EDGE
                    && x < r.right() - DROP_EDGE;
                if into {
                    return (i, Some(i));
                }
                return (if x < r.x + r.width / 2.0 { i } else { i + 1 }, None);
            }
        }
        (self.items.len(), None)
    }

    /// The x of the insertion marker before item `index`.
    pub fn marker_x(&self, index: usize) -> f32 {
        match (self.items.get(index), self.items.last()) {
            (Some(r), _) => r.x - 1.5,
            (None, Some(last)) => last.right() + 0.5,
            (None, None) => 8.0,
        }
    }
}

/// How wide a bookmark is on the bar for a title `text_width` wide.
pub fn bookmark_width(text_width: f32, has_text: bool) -> f32 {
    if has_text {
        8.0 + 16.0 + 6.0 + text_width.min(BOOKMARK_MAX_TEXT) + 10.0
    } else {
        8.0 + 16.0 + 8.0
    }
}

impl Layout {
    /// The chrome's height (with the bookmarks bar when it shows).
    pub fn height(&self) -> f32 {
        if self.fullscreen {
            return 0.0;
        }
        if self.popup {
            return POPUP_BAR_HEIGHT;
        }
        let top = if self.side.is_some() {
            TOOLBAR_HEIGHT
        } else {
            CHROME_HEIGHT
        };
        top + self.bar.as_ref().map_or(0.0, |b| b.rect.height)
    }

    /// A popup window's chrome, `width` wide: a read-only location line showing the page's
    /// address and its security state, and nothing else (no tabs, buttons or bars; the window
    /// has Windows' own caption). Chromium's popup windows show their location bar read-only.
    pub fn popup(width: f32) -> Layout {
        let mut l = Layout::make(width, 0, 0.0, None, None);
        let none = Rect::new(-1000.0, -1000.0, 0.0, 0.0);
        for r in [
            &mut l.new_tab,
            &mut l.minimize,
            &mut l.maximize,
            &mut l.close,
            &mut l.back,
            &mut l.forward,
            &mut l.reload,
            &mut l.home,
            &mut l.star,
            &mut l.menu,
            &mut l.shield,
        ] {
            *r = none;
        }
        let address = Rect::new(6.0, 4.0, (width - 12.0).max(40.0), POPUP_BAR_HEIGHT - 8.0);
        l.address = address;
        l.site_info = Rect::new(address.x + 4.0, address.y + 2.0, 28.0, address.height - 4.0);
        l.address_text = Rect::new(
            address.x + 34.0,
            address.y,
            (address.width - 34.0 - 10.0).max(10.0),
            address.height,
        );
        l.popup = true;
        l
    }

    /// `bar`: the bookmarks bar's item widths when it shows.
    pub fn new(width: f32, tabs: usize, zoom_label: f32, bar: Option<&[f32]>) -> Layout {
        Layout::make(width, tabs, zoom_label, bar, None)
    }

    /// The layout with the tabs in a panel at the left ([`SideSpec`]): the toolbar is the top row,
    /// with the caption buttons at its end; the bookmarks bar, then the panel, under it.
    pub fn with_side_tabs(
        width: f32,
        tabs: usize,
        zoom_label: f32,
        bar: Option<&[f32]>,
        side: &SideSpec,
    ) -> Layout {
        Layout::make(width, tabs, zoom_label, bar, Some(side))
    }

    fn make(
        width: f32,
        tabs: usize,
        zoom_label: f32,
        bar: Option<&[f32]>,
        side: Option<&SideSpec>,
    ) -> Layout {
        let caption_left = width - 3.0 * CAPTION_WIDTH;
        let minimize = Rect::new(caption_left, 0.0, CAPTION_WIDTH, CAPTION_HEIGHT);
        let maximize = Rect::new(
            caption_left + CAPTION_WIDTH,
            0.0,
            CAPTION_WIDTH,
            CAPTION_HEIGHT,
        );
        let close = Rect::new(
            caption_left + 2.0 * CAPTION_WIDTH,
            0.0,
            CAPTION_WIDTH,
            CAPTION_HEIGHT,
        );
        let strip_left = 8.0;
        // (room for the new tab button and a strip of caption to drag the window by)
        let room = (caption_left - strip_left - 36.0 - 40.0).max(0.0);
        let w = tab_width(tabs, room, TAB_MIN, TAB_MAX);
        let tab_h = TAB_STRIP_HEIGHT - TAB_TOP;
        let rects: Vec<Rect> = (0..tabs)
            .map(|i| Rect::new(strip_left + i as f32 * w, TAB_TOP, w, tab_h))
            .collect();
        let after = strip_left + tabs as f32 * w;
        let new_tab = Rect::new(
            (after + 4.0).min(caption_left - 32.0),
            TAB_TOP + (tab_h - 28.0) / 2.0,
            28.0,
            28.0,
        );
        let toolbar_top = if side.is_some() {
            0.0
        } else {
            TAB_STRIP_HEIGHT
        };
        let y = toolbar_top + (TOOLBAR_HEIGHT - BUTTON) / 2.0;
        let button = |i: f32| Rect::new(8.0 + i * (BUTTON + 2.0), y, BUTTON, BUTTON);
        let back = button(0.0);
        let forward = button(1.0);
        let reload = button(2.0);
        let home = button(3.0);
        let ax = home.right() + 8.0;
        let toolbar_right = if side.is_some() {
            caption_left - GRAB
        } else {
            width - 8.0
        };
        let menu = Rect::new(toolbar_right - BUTTON, y, BUTTON, BUTTON);
        let shield = Rect::new(menu.x - 2.0 - BUTTON, y, BUTTON, BUTTON);
        let address = Rect::new(ax, y, (shield.x - 6.0 - ax).max(40.0), BUTTON);
        let site_info = Rect::new(address.x + 4.0, address.y + 3.0, 28.0, BUTTON - 6.0);
        let star = Rect::new(
            address.right() - 4.0 - 28.0,
            address.y + 3.0,
            28.0,
            BUTTON - 6.0,
        );
        let address_text = Rect::new(
            address.x + 34.0,
            address.y,
            (address.width - 34.0 - 14.0 - zoom_label - star.width).max(10.0),
            address.height,
        );
        let bar = bar.map(|w| BarLayout::new(width, toolbar_top + TOOLBAR_HEIGHT, w));
        let side = side.map(|spec| {
            let top = toolbar_top + TOOLBAR_HEIGHT + bar.as_ref().map_or(0.0, |b| b.rect.height);
            SideLayout::new(top, spec, tabs)
        });
        let (rects, new_tab) = match &side {
            Some(s) => (s.tabs.clone(), s.new_tab),
            None => (rects, new_tab),
        };
        Layout {
            width,
            tabs: rects,
            new_tab,
            minimize,
            maximize,
            close,
            back,
            forward,
            reload,
            home,
            address,
            address_text,
            site_info,
            star,
            menu,
            shield,
            incognito: None,
            downloads: None,
            bar,
            audio: Vec::new(),
            maximized: false,
            fullscreen: false,
            side,
            popup: false,
        }
    }

    /// The layout of a maximized window (`maximized`): a point above the tabs, up to the screen's top
    /// edge, is on the tab below it instead of the caption, so the pointer thrown against the edge
    /// lands on a tab.
    pub fn with_maximized(mut self, maximized: bool) -> Layout {
        self.maximized = maximized;
        self
    }

    /// The layout of an Incognito window: its Incognito button, `width` wide, before the menu; the
    /// content blocker's button and the address bar's end move left to make room. A popup
    /// window's location line has none.
    pub fn with_incognito(mut self, width: f32) -> Layout {
        if self.popup {
            return self;
        }
        let room = width + 2.0;
        let button = Rect::new(self.menu.x - room, self.menu.y, width, self.menu.height);
        self.shield.x -= room;
        if let Some(d) = &mut self.downloads {
            d.x -= room;
        }
        self.narrow_address(room);
        self.incognito = Some(button);
        self
    }

    /// The layout with the downloads button, between the content blocker's and the menu (and an
    /// Incognito window's Incognito button); the content blocker's button and the address bar's end
    /// move left to make room. A popup window's location line has none.
    pub fn with_downloads(mut self) -> Layout {
        if self.popup || self.downloads.is_some() {
            return self;
        }
        let room = BUTTON + 2.0;
        self.downloads = Some(self.shield);
        self.shield.x -= room;
        self.narrow_address(room);
        self
    }

    /// The address bar gives up `room` at its end.
    fn narrow_address(&mut self, room: f32) {
        self.address.width = (self.address.width - room).max(40.0);
        self.star.x = self.address.right() - 4.0 - self.star.width;
        self.address_text.width = (self.address_text.width - room).max(10.0);
    }

    /// The layout of a fullscreen window: no chrome is shown, laid out or hit.
    pub fn with_fullscreen(mut self, fullscreen: bool) -> Layout {
        self.fullscreen = fullscreen;
        self
    }

    /// The layout with audio indicators on the tabs where `audio` is true.
    pub fn with_audio(mut self, audio: Vec<bool>) -> Layout {
        self.audio = audio;
        self
    }

    /// Tab `i`'s audio indicator (the button around the speaker, right of the favicon), when it
    /// shows one.
    pub fn tab_audio(&self, i: usize) -> Option<Rect> {
        if !self.audio.get(i).copied().unwrap_or(false) {
            return None;
        }
        let r = self.tabs.get(i)?;
        if let Some(side) = &self.side {
            return side.audio(r);
        }
        let x = r.x + 12.0 + ICON_SIZE + 2.0;
        // (a tab too narrow for it beside the favicon shows only the favicon)
        if x + AUDIO_BUTTON > r.right() - 4.0 {
            return None;
        }
        Some(Rect::new(
            x,
            r.y + (r.height - AUDIO_BUTTON) / 2.0,
            AUDIO_BUTTON,
            AUDIO_BUTTON,
        ))
    }

    pub fn tab_close(&self, i: usize) -> Option<Rect> {
        let r = self.tabs.get(i)?;
        if let Some(side) = &self.side {
            return side.close(r);
        }
        if r.width < 72.0 {
            return None;
        }
        Some(Rect::new(
            r.right() - 28.0,
            r.y + (r.height - 20.0) / 2.0,
            20.0,
            20.0,
        ))
    }

    pub fn hit(&self, p: Point) -> Hit {
        if self.fullscreen {
            return Hit::None;
        }
        if self.popup {
            // (the location line is read-only: its security state alone opens anything)
            return if self.site_info.contains(p) {
                Hit::SiteInfo
            } else {
                Hit::None
            };
        }
        if let Some(side) = &self.side {
            return self.hit_with_side(side, p);
        }
        if p.y < TAB_STRIP_HEIGHT {
            let p = if self.maximized && p.y < TAB_TOP {
                Point::new(p.x, TAB_TOP)
            } else {
                p
            };
            for (r, h) in [
                (self.minimize, Hit::Minimize),
                (self.maximize, Hit::Maximize),
                (self.close, Hit::Close),
            ] {
                if r.contains(p) {
                    return h;
                }
            }
            for (i, r) in self.tabs.iter().enumerate() {
                if r.contains(p) {
                    if self.tab_close(i).is_some_and(|c| c.contains(p)) {
                        return Hit::TabClose(i);
                    }
                    if self.tab_audio(i).is_some_and(|c| c.contains(p)) {
                        return Hit::TabAudio(i);
                    }
                    return Hit::Tab(i);
                }
            }
            if self.new_tab.contains(p) {
                return Hit::NewTab;
            }
            return Hit::Caption;
        }
        if p.y < CHROME_HEIGHT {
            return self.toolbar_hit(p);
        }
        self.bar_hit(p)
    }

    /// A point with the tabs at the side: the toolbar row (caption buttons included), the bookmarks
    /// bar, the panel.
    fn hit_with_side(&self, side: &SideLayout, p: Point) -> Hit {
        if p.y < TOOLBAR_HEIGHT {
            // (a maximized window: the pointer thrown against the screen's top edge is on the
            // button under it)
            let p = if self.maximized {
                Point::new(p.x, p.y.max(self.back.y))
            } else {
                p
            };
            for (r, h) in [
                (self.minimize, Hit::Minimize),
                (self.maximize, Hit::Maximize),
                (self.close, Hit::Close),
            ] {
                if r.contains(p) {
                    return h;
                }
            }
            return match self.toolbar_hit(p) {
                Hit::None => Hit::Caption,
                h => h,
            };
        }
        if side.rect.contains(p) {
            return side.hit(p, &self.audio);
        }
        self.bar_hit(p)
    }

    fn toolbar_hit(&self, p: Point) -> Hit {
        if self.incognito.is_some_and(|r| r.contains(p)) {
            return Hit::Incognito;
        }
        if self.downloads.is_some_and(|r| r.contains(p)) {
            return Hit::Downloads;
        }
        for (r, h) in [
            (self.back, Hit::Back),
            (self.forward, Hit::Forward),
            (self.reload, Hit::Reload),
            (self.home, Hit::Home),
            (self.menu, Hit::Menu),
            (self.shield, Hit::Shield),
            (self.site_info, Hit::SiteInfo),
            (self.star, Hit::Star),
            (self.address, Hit::Address),
        ] {
            if r.contains(p) {
                return h;
            }
        }
        Hit::None
    }

    fn bar_hit(&self, p: Point) -> Hit {
        if let Some(bar) = self.bar.as_ref().filter(|b| b.rect.contains(p)) {
            if bar.overflow.is_some_and(|o| o.contains(p)) {
                return Hit::BookmarkOverflow;
            }
            return match bar.items.iter().position(|r| r.contains(p)) {
                Some(i) => Hit::Bookmark(i),
                None => Hit::BookmarksBar,
            };
        }
        Hit::None
    }
}

/// Shaping for the chrome.
pub struct Fonts {
    text: Arc<TextEngine>,
    ui: FontListId,
    icons: FontListId,
    /// The icon font is there: its code points are drawn as they are, else as the symbols
    /// [`glyph::symbol`] maps them to.
    icon_font: bool,
    /// Monospaced (the developer tools' code, view-source).
    mono: FontListId,
    /// Monospaced bold, italic, bold italic (the console's `%c` styles).
    mono_styled: [FontListId; 3],
    bold: FontListId,
}

/// Which of the chrome's fonts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    Ui,
    Mono,
    MonoBold,
    MonoItalic,
    MonoBoldItalic,
    Bold,
}

impl Face {
    /// The monospaced face of that weight and slant.
    pub fn mono(bold: bool, italic: bool) -> Face {
        match (bold, italic) {
            (false, false) => Face::Mono,
            (true, false) => Face::MonoBold,
            (false, true) => Face::MonoItalic,
            (true, true) => Face::MonoBoldItalic,
        }
    }
}

impl Fonts {
    pub fn new(text: Arc<TextEngine>) -> Fonts {
        fn query<'a>(families: &'a [&'a str]) -> FontQuery<'a> {
            FontQuery {
                families,
                weight: 400,
                style: FontStyle::Normal,
                stretch: FontStretch::NORMAL,
            }
        }
        let ui = text
            .font_list(&query(&[
                "Segoe UI Variable Text",
                "Segoe UI",
                "sans-serif",
            ]))
            .or_else(|_| text.font_list(&query(&["sans-serif"])))
            .expect("a UI font");
        // (a missing family still gives a list, of the fallback faces)
        let icon_font = ["Segoe Fluent Icons", "Segoe MDL2 Assets"]
            .iter()
            .any(|f| text.has_family(f));
        let icons = text
            .font_list(&query(&["Segoe Fluent Icons", "Segoe MDL2 Assets"]))
            .unwrap_or(ui);
        const MONO: &[&str] = &["Consolas", "Cascadia Mono", "monospace"];
        let mono = text.font_list(&query(MONO)).unwrap_or(ui);
        let mono_styled = [
            (700, FontStyle::Normal),
            (400, FontStyle::Italic),
            (700, FontStyle::Italic),
        ]
        .map(|(weight, style)| {
            text.font_list(&FontQuery {
                weight,
                style,
                ..query(MONO)
            })
            .unwrap_or(mono)
        });
        let bold = text
            .font_list(&FontQuery {
                weight: 600,
                ..query(&["Segoe UI Variable Text", "Segoe UI", "sans-serif"])
            })
            .unwrap_or(ui);
        Fonts {
            text,
            ui,
            icons,
            icon_font,
            mono,
            mono_styled,
            bold,
        }
    }

    pub fn engine(&self) -> &Arc<TextEngine> {
        &self.text
    }

    pub fn shape(&self, s: &str, size: f32) -> ShapedText {
        self.shape_with(s, self.ui, size)
    }

    fn shape_with(&self, s: &str, list: FontListId, size: f32) -> ShapedText {
        let mut out = ShapedText::default();
        let _ = self.text.shape(
            s,
            list,
            &ShapeOptions {
                size,
                ..Default::default()
            },
            &mut out,
        );
        out
    }

    pub fn width(&self, s: &str, size: f32) -> f32 {
        self.shape(s, size).width()
    }

    fn list(&self, face: Face) -> FontListId {
        match face {
            Face::Ui => self.ui,
            Face::Mono => self.mono,
            Face::MonoBold => self.mono_styled[0],
            Face::MonoItalic => self.mono_styled[1],
            Face::MonoBoldItalic => self.mono_styled[2],
            Face::Bold => self.bold,
        }
    }

    pub fn width_in(&self, face: Face, s: &str, size: f32) -> f32 {
        self.shape_with(s, self.list(face), size).width()
    }

    /// [`Fonts::label`] in `face`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn label_in(
        &self,
        b: &mut DisplayListBuilder,
        face: Face,
        s: &str,
        x: f32,
        center_y: f32,
        size: f32,
        color: Color,
        max: f32,
    ) -> f32 {
        let list = self.list(face);
        let shaped = self.shape_with(s, list, size);
        let y = self.baseline(list, size, center_y);
        if shaped.width() <= max {
            return shaped.paint(0..shaped.clusters.len(), Point::new(x, y), color, b);
        }
        let dots = self.shape_with("…", list, size);
        let room = max - dots.width();
        let mut w = 0.0;
        let mut n = 0;
        for c in &shaped.clusters {
            if w + c.advance > room {
                break;
            }
            w += c.advance;
            n += 1;
        }
        let w = shaped.paint(0..n, Point::new(x, y), color, b);
        dots.paint(0..dots.clusters.len(), Point::new(x + w, y), color, b) + w
    }

    /// [`Fonts::caret_x`] in `face`.
    pub fn caret_x_in(&self, face: Face, s: &str, index: usize, size: f32) -> f32 {
        let shaped = self.shape_with(s, self.list(face), size);
        shaped
            .clusters
            .iter()
            .take_while(|c| (c.text.1 as usize) <= index)
            .map(|c| c.advance)
            .sum()
    }

    /// [`Fonts::index_at`] in `face`.
    pub fn index_at_in(&self, face: Face, s: &str, x: f32, size: f32) -> usize {
        let shaped = self.shape_with(s, self.list(face), size);
        let mut left = 0.0;
        for c in &shaped.clusters {
            let right = left + c.advance;
            if x < left + c.advance / 2.0 {
                return c.text.0 as usize;
            }
            if x < right {
                return c.text.1 as usize;
            }
            left = right;
        }
        s.len()
    }

    /// The height of a line of `size` text in `face` at its `normal` line height (the primary
    /// font's ascent and descent; CSS 2 10.8.1).
    pub fn line_height_in(&self, face: Face, size: f32) -> f32 {
        let list = self.list(face);
        let m = self.text.metrics(self.text.primary_font(list), size);
        m.ascent + m.descent + m.line_gap
    }

    /// The baseline that centers a line of `size` text on `center_y`.
    fn baseline(&self, list: FontListId, size: f32, center_y: f32) -> f32 {
        let m = self.text.metrics(self.text.primary_font(list), size);
        (center_y + (m.ascent - m.descent) / 2.0).round()
    }

    /// Draws `s` from `x`, vertically centered on `center_y`, cut with an ellipsis at `max` wide.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn label(
        &self,
        b: &mut DisplayListBuilder,
        s: &str,
        x: f32,
        center_y: f32,
        size: f32,
        color: Color,
        max: f32,
    ) -> f32 {
        let shaped = self.shape(s, size);
        let y = self.baseline(self.ui, size, center_y);
        if shaped.width() <= max {
            return shaped.paint(0..shaped.clusters.len(), Point::new(x, y), color, b);
        }
        let dots = self.shape("…", size);
        let room = max - dots.width();
        let mut w = 0.0;
        let mut n = 0;
        for c in &shaped.clusters {
            if w + c.advance > room {
                break;
            }
            w += c.advance;
            n += 1;
        }
        let w = shaped.paint(0..n, Point::new(x, y), color, b);
        dots.paint(0..dots.clusters.len(), Point::new(x + w, y), color, b) + w
    }

    pub(crate) fn icon(
        &self,
        b: &mut DisplayListBuilder,
        c: char,
        center: Point,
        size: f32,
        color: Color,
    ) {
        let c = if self.icon_font { c } else { glyph::symbol(c) };
        let mut buf = [0u8; 4];
        let shaped = self.shape_with(c.encode_utf8(&mut buf), self.icons, size);
        let y = (center.y + size / 2.0).round();
        let x = (center.x - shaped.width() / 2.0).round();
        shaped.paint(0..shaped.clusters.len(), Point::new(x, y), color, b);
    }

    /// The x offset of byte `index` in `s` (a caret position).
    pub fn caret_x(&self, s: &str, index: usize, size: f32) -> f32 {
        let shaped = self.shape(s, size);
        shaped
            .clusters
            .iter()
            .take_while(|c| (c.text.1 as usize) <= index)
            .map(|c| c.advance)
            .sum()
    }

    /// The caret position nearest to `x` in `s`.
    pub fn index_at(&self, s: &str, x: f32, size: f32) -> usize {
        let shaped = self.shape(s, size);
        let mut left = 0.0;
        for c in &shaped.clusters {
            let right = left + c.advance;
            if x < left + c.advance / 2.0 {
                return c.text.0 as usize;
            }
            if x < right {
                return c.text.1 as usize;
            }
            left = right;
        }
        s.len()
    }
}

/// A tab as the strip shows it.
#[derive(Clone, Debug)]
pub struct TabView<'a> {
    pub title: &'a str,
    pub icon: Option<ImageId>,
    pub loading: bool,
    pub crashed: bool,
    /// The audio indicator (shown where [`Layout::tab_audio`] has room for it).
    pub audio: crate::audio::TabAudio,
}

/// The address bar's state for drawing.
#[derive(Clone, Debug)]
pub struct AddressView<'a> {
    pub field: &'a TextField,
    pub focused: bool,
    pub caret_visible: bool,
    /// How far the text is scrolled left (to keep the caret in view).
    pub scroll: f32,
    pub secure: Option<bool>,
    pub zoom: Option<u32>,
}

/// Everything the chrome shows.
#[derive(Clone, Debug)]
pub struct ChromeView<'a> {
    pub tabs: Vec<TabView<'a>>,
    pub active: usize,
    pub hover: Hit,
    pub pressed: Hit,
    pub caption_hover: Option<CaptionButton>,
    pub maximized: bool,
    pub window_active: bool,
    pub can_back: bool,
    pub can_forward: bool,
    pub loading: bool,
    pub address: AddressView<'a>,
    /// The loading spinner's angle (radians).
    pub spinner: f32,
    pub theme: Theme,
    /// The bookmarks bar's items (when it shows).
    pub bar: Vec<BarItemView<'a>>,
    /// The page is bookmarked (the star is filled).
    pub starred: bool,
    /// A button whose popup is open (drawn pressed): the menu, the site information, a folder.
    pub open: Hit,
    /// A bookmark being dragged on the bar: the insertion marker's x, or the folder it drops into.
    pub bar_drop: Option<(f32, Option<usize>)>,
    /// The content blocker's button (`None`: blocking is not running).
    pub blocker: Option<BlockerBadge>,
    /// An Incognito window's Incognito button label ([`incognito_label`]), drawn where the layout
    /// puts it ([`Layout::with_incognito`]).
    pub incognito: Option<String>,
    /// The downloads button, drawn where the layout puts it ([`Layout::with_downloads`]).
    pub downloads: Option<DownloadsButton>,
}

/// What the toolbar's downloads button shows. It is there while a download runs and for an hour
/// after the last one ended, in the accent colour for a minute after that.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DownloadsButton {
    /// Downloads running: how much of them is done (0 to 1; `None` when a size is unknown),
    /// drawn as a ring around the arrow.
    pub running: Option<Option<f32>>,
    /// A download ended less than a minute ago.
    pub fresh: bool,
}

/// How long the downloads button stays after the last download ended (ms).
pub const DOWNLOADS_BUTTON_MS: i64 = 60 * 60 * 1000;
/// How long it stays in the accent colour after a download ended (ms).
pub const DOWNLOADS_FRESH_MS: i64 = 60 * 1000;

/// What the downloads button shows for downloads `(state, received, total, end time)` at `now`
/// (ms); `None`: no button.
pub fn downloads_button(
    downloads: impl IntoIterator<Item = (bool, u64, u64, i64)>,
    now: i64,
) -> Option<DownloadsButton> {
    let (mut running, mut received, mut total, mut unknown) = (0, 0u64, 0u64, false);
    let mut last_end = None::<i64>;
    for (in_progress, got, size, end) in downloads {
        if in_progress {
            running += 1;
            received += got;
            total += size;
            unknown |= size == 0;
        } else {
            last_end = Some(last_end.map_or(end, |l| l.max(end)));
        }
    }
    let since = last_end.map(|e| now - e);
    if running == 0 && !since.is_some_and(|s| s < DOWNLOADS_BUTTON_MS) {
        return None;
    }
    Some(DownloadsButton {
        running: (running > 0).then(|| {
            (!unknown && total > 0).then(|| (received as f64 / total as f64).min(1.0) as f32)
        }),
        fresh: since.is_some_and(|s| s < DOWNLOADS_FRESH_MS),
    })
}

/// The downloads button: the arrow (in the accent colour while a download runs or just ended),
/// with a ring of the running downloads' progress around it (a quarter turning while a size is
/// unknown, `spin` its angle).
fn downloads_button_paint(
    b: &mut DisplayListBuilder,
    r: Rect,
    d: DownloadsButton,
    view: &ChromeView,
    fonts: &Fonts,
) {
    let t = &view.theme;
    button_bg(b, r, Hit::Downloads, view, true);
    let center = Point::new(r.x + r.width / 2.0, r.y + r.height / 2.0);
    let color = if d.running.is_some() || d.fresh {
        t.accent
    } else {
        t.text
    };
    let Some(progress) = d.running else {
        fonts.icon(b, glyph::DOWNLOAD, center, 16.0, color);
        return;
    };
    fonts.icon(b, glyph::DOWNLOAD, center, 12.0, color);
    let radius = 11.0;
    let ring = |from: f32, sweep: f32| arc(center, radius, from, sweep);
    let stroke = snowlark_paint::Stroke {
        width: 2.0,
        cap: snowlark_paint::LineCap::Round,
        ..Default::default()
    };
    if let Some(track) = b.add_path(&ring(0.0, std::f32::consts::TAU)) {
        b.stroke_path(track, t.separator, &stroke, &[], true);
    }
    let start = -std::f32::consts::FRAC_PI_2;
    let (from, sweep) = match progress {
        Some(p) => (start, p * std::f32::consts::TAU),
        None => (start + view.spinner, std::f32::consts::FRAC_PI_2),
    };
    if sweep > 0.0 {
        if let Some(done) = b.add_path(&ring(from, sweep)) {
            b.stroke_path(done, color, &stroke, &[], true);
        }
    }
}

/// A circle's arc from angle `from` turning `sweep` (radians, clockwise on screen), as cubic
/// curves of at most a quarter turn each.
fn arc(center: Point, radius: f32, from: f32, sweep: f32) -> Vec<PathEl> {
    let parts = (sweep.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = sweep / parts as f32;
    let k = 4.0 / 3.0 * (step / 4.0).tan() * radius;
    let at = |a: f32| Point::new(center.x + radius * a.cos(), center.y + radius * a.sin());
    let mut path = vec![PathEl::MoveTo(at(from))];
    for i in 0..parts {
        let a0 = from + step * i as f32;
        let a1 = a0 + step;
        let (p0, p1) = (at(a0), at(a1));
        path.push(PathEl::CurveTo(
            Point::new(p0.x - k * a0.sin(), p0.y + k * a0.cos()),
            Point::new(p1.x + k * a1.sin(), p1.y - k * a1.cos()),
            p1,
        ));
    }
    path
}

/// What the content blocker's button shows for the active tab.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BlockerBadge {
    /// Requests the page had blocked.
    pub count: usize,
    /// Blocking is on for the page's site.
    pub up: bool,
}

/// A bookmark as the bar shows it.
#[derive(Clone, Debug)]
pub struct BarItemView<'a> {
    pub label: &'a str,
    pub icon: Option<ImageId>,
    pub folder: bool,
}

pub fn zoom_label(zoom: Option<u32>) -> String {
    zoom.map(|z| format!("{z}%")).unwrap_or_default()
}

/// The content blocker's button: the shield (orange while blocking is up), with the page's
/// blocked count in a badge at its lower right.
fn blocker_button(
    b: &mut DisplayListBuilder,
    r: Rect,
    badge: BlockerBadge,
    view: &ChromeView,
    fonts: &Fonts,
) {
    let t = &view.theme;
    button_bg(b, r, Hit::Shield, view, true);
    let orange = Color::hex(if t.dark { 0xFF7A45 } else { 0xE8590C });
    let center = Point::new(r.x + r.width / 2.0, r.y + r.height / 2.0);
    fonts.icon(
        b,
        crate::blocker_panel::SHIELD,
        center,
        16.0,
        if badge.up { orange } else { t.text_dim },
    );
    if badge.up && badge.count > 0 {
        let text = crate::blocker_panel::badge_text(badge.count);
        let size = 9.5;
        let w = (fonts.width_in(Face::Bold, &text, size) + 8.0).max(15.0);
        let pill = Rect::new(r.right() - w + 2.0, r.bottom() - 15.0, w, 14.0);
        b.fill_rounded_rect(pill.inflate(1.5, 1.5), Radii::uniform(8.5), t.toolbar);
        b.fill_rounded_rect(pill, Radii::uniform(7.0), orange);
        fonts.label_in(
            b,
            Face::Bold,
            &text,
            pill.x + (w - fonts.width_in(Face::Bold, &text, size)) / 2.0,
            pill.y + pill.height / 2.0,
            size,
            Color::WHITE,
            f32::INFINITY,
        );
    }
}

/// An Incognito window's Incognito button: a pill in the field colour with the Incognito mark
/// and the label.
fn incognito_button(
    b: &mut DisplayListBuilder,
    r: Rect,
    label: &str,
    view: &ChromeView,
    fonts: &Fonts,
) {
    let t = &view.theme;
    let radii = Radii::uniform(r.height / 2.0);
    b.fill_rounded_rect(r, radii, t.field);
    button_bg(b, r, Hit::Incognito, view, true);
    let center = Point::new(r.x + 10.0 + INCOGNITO_MARK / 2.0, r.y + r.height / 2.0);
    if let Some(path) = b.add_path(&incognito_mark(center, INCOGNITO_MARK)) {
        b.fill_path(path, t.text, true, true);
    }
    let x = r.x + 10.0 + INCOGNITO_MARK + 6.0;
    fonts.label(
        b,
        label,
        x,
        r.y + r.height / 2.0,
        INCOGNITO_TEXT,
        t.text,
        (r.right() - 8.0 - x).max(0.0),
    );
}

fn button_bg(b: &mut DisplayListBuilder, r: Rect, hit: Hit, view: &ChromeView, round: bool) {
    let color = if (view.pressed == hit && view.hover == hit) || view.open == hit {
        view.theme.button_pressed
    } else if view.hover == hit {
        view.theme.button_hover
    } else {
        return;
    };
    let radius = if round { r.height / 2.0 } else { 0.0 };
    b.fill_rounded_rect(r, Radii::uniform(radius), color);
}

/// The chrome's display list (DIPs).
pub fn build(view: &ChromeView, layout: &Layout, fonts: &Fonts) -> DisplayList {
    let t = &view.theme;
    let mut b = DisplayListBuilder::new();
    // (a fullscreen window: the compositor paints this list over the page wherever it is)
    if layout.fullscreen {
        return b.build();
    }
    if layout.popup {
        b.fill_rect(
            Rect::new(0.0, 0.0, layout.width, layout.height()),
            t.toolbar,
        );
        address(&mut b, view, layout, fonts);
        return b.build();
    }
    let frame = if view.window_active {
        t.frame
    } else {
        t.frame_inactive
    };
    // (tabs at the side: the toolbar is the top row, the panel is drawn with the overlay)
    let strip = if layout.side.is_some() {
        0.0
    } else {
        TAB_STRIP_HEIGHT
    };
    b.fill_rect(Rect::new(0.0, 0.0, layout.width, strip), frame);
    if strip == 0.0 {
        b.fill_rect(
            Rect::new(0.0, 0.0, layout.width, layout.height()),
            t.toolbar,
        );
    }
    // Tabs.
    let strip_tabs = if strip == 0.0 { 0 } else { view.tabs.len() };
    for (i, (tab, r)) in view.tabs[..strip_tabs].iter().zip(&layout.tabs).enumerate() {
        let active = i == view.active;
        if active {
            let shape = Rect::new(r.x, r.y, r.width, r.height + 1.0);
            b.fill_rounded_rect(
                shape,
                Radii {
                    top_left: Size::new(8.0, 8.0),
                    top_right: Size::new(8.0, 8.0),
                    bottom_right: Size::ZERO,
                    bottom_left: Size::ZERO,
                },
                t.tab_active,
            );
        } else {
            if matches!(view.hover, Hit::Tab(h) | Hit::TabClose(h) | Hit::TabAudio(h) if h == i) {
                b.fill_rounded_rect(
                    Rect::new(r.x + 2.0, r.y + 2.0, r.width - 4.0, r.height - 6.0),
                    Radii::uniform(8.0),
                    t.tab_hover,
                );
            }
            // A separator between inactive tabs that aren't next to the active one.
            if i + 1 < view.tabs.len() && i + 1 != view.active {
                b.fill_rect(
                    Rect::new(r.right() - 0.5, r.y + 9.0, 1.0, r.height - 18.0),
                    t.separator,
                );
            }
        }
        let cy = r.y + r.height / 2.0;
        let icon = Rect::new(r.x + 12.0, cy - ICON_SIZE / 2.0, ICON_SIZE, ICON_SIZE);
        if tab.loading {
            spinner(&mut b, Point::new(icon.x + 8.0, cy), view.spinner, t.accent);
        } else if tab.crashed {
            fonts.icon(
                &mut b,
                glyph::SAD,
                Point::new(icon.x + 8.0, cy),
                14.0,
                t.text_dim,
            );
        } else if let Some(id) = tab.icon {
            b.image(id, icon, None, 1.0, ImageRendering::Smooth);
        } else {
            fonts.icon(
                &mut b,
                glyph::GLOBE,
                Point::new(icon.x + 8.0, cy),
                14.0,
                t.text_dim,
            );
        }
        let close = layout.tab_close(i);
        let title_color = if active { t.text } else { t.text_dim };
        let mut text_x = icon.right() + 8.0;
        let audio_glyph = match tab.audio {
            crate::audio::TabAudio::None => None,
            crate::audio::TabAudio::Playing => Some(glyph::VOLUME),
            crate::audio::TabAudio::Muted => Some(glyph::MUTE),
        };
        if let (Some(g), Some(a)) = (audio_glyph, layout.tab_audio(i)) {
            if view.hover == Hit::TabAudio(i) {
                let bg = if view.pressed == Hit::TabAudio(i) {
                    t.button_pressed
                } else {
                    t.button_hover
                };
                b.fill_rounded_rect(a, Radii::uniform(AUDIO_BUTTON / 2.0), bg);
            }
            fonts.icon(
                &mut b,
                g,
                Point::new(a.x + a.width / 2.0, a.y + a.height / 2.0),
                AUDIO_GLYPH,
                title_color,
            );
            text_x = a.right() + 4.0;
        }
        let text_right = close.map_or(r.right() - 10.0, |c| c.x - 4.0);
        if text_right > text_x + 4.0 {
            b.push_clip_rect(Rect::new(text_x, r.y, text_right - text_x, r.height));
            let title = if tab.title.is_empty() {
                "New tab"
            } else {
                tab.title
            };
            fonts.label(
                &mut b,
                title,
                text_x,
                cy,
                UI_SIZE,
                title_color,
                text_right - text_x,
            );
            b.pop_clip();
        }
        if let Some(c) = close {
            if view.hover == Hit::TabClose(i) {
                b.fill_rounded_rect(c, Radii::uniform(10.0), t.button_hover);
            }
            fonts.icon(
                &mut b,
                glyph::CLOSE,
                Point::new(c.x + 10.0, c.y + 10.0),
                8.0,
                t.text,
            );
        }
    }
    if strip > 0.0 {
        button_bg(&mut b, layout.new_tab, Hit::NewTab, view, true);
        let nt = layout.new_tab;
        fonts.icon(
            &mut b,
            glyph::ADD,
            Point::new(nt.x + nt.width / 2.0, nt.y + nt.height / 2.0),
            12.0,
            t.text,
        );
    }
    // Caption buttons.
    for (r, button, g) in [
        (layout.minimize, CaptionButton::Minimize, glyph::MINIMIZE),
        (
            layout.maximize,
            CaptionButton::Maximize,
            if view.maximized {
                glyph::RESTORE
            } else {
                glyph::MAXIMIZE
            },
        ),
        (layout.close, CaptionButton::Close, glyph::CLOSE),
    ] {
        let hover = view.caption_hover == Some(button);
        let mut color = t.text;
        if hover {
            if button == CaptionButton::Close {
                b.fill_rect(r, t.close_hover);
                color = Color::WHITE;
            } else {
                b.fill_rect(r, t.button_hover);
            }
        }
        if !view.window_active && !hover {
            color = t.text_dim;
        }
        fonts.icon(
            &mut b,
            g,
            Point::new(r.x + r.width / 2.0, r.y + r.height / 2.0),
            10.0,
            color,
        );
    }
    // Toolbar (and the bookmarks bar under it).
    if strip > 0.0 {
        b.fill_rect(
            Rect::new(0.0, strip, layout.width, layout.height() - strip),
            t.toolbar,
        );
    }
    b.fill_rect(
        Rect::new(0.0, layout.height() - 1.0, layout.width, 1.0),
        t.separator,
    );
    if let Some(badge) = view.blocker {
        blocker_button(&mut b, layout.shield, badge, view, fonts);
    }
    if let (Some(r), Some(label)) = (layout.incognito, &view.incognito) {
        incognito_button(&mut b, r, label, view, fonts);
    }
    if let (Some(r), Some(d)) = (layout.downloads, view.downloads) {
        downloads_button_paint(&mut b, r, d, view, fonts);
    }
    button_bg(&mut b, layout.menu, Hit::Menu, view, true);
    fonts.icon(
        &mut b,
        glyph::MORE,
        Point::new(
            layout.menu.x + layout.menu.width / 2.0,
            layout.menu.y + layout.menu.height / 2.0,
        ),
        16.0,
        t.text,
    );
    if let Some(bar) = &layout.bar {
        bookmarks_bar(&mut b, view, bar, fonts);
    }
    for (r, hit, g, enabled) in [
        (layout.back, Hit::Back, glyph::BACK, view.can_back),
        (
            layout.forward,
            Hit::Forward,
            glyph::FORWARD,
            view.can_forward,
        ),
        (
            layout.reload,
            Hit::Reload,
            if view.loading {
                glyph::STOP
            } else {
                glyph::REFRESH
            },
            true,
        ),
        (layout.home, Hit::Home, glyph::HOME, true),
    ] {
        if enabled {
            button_bg(&mut b, r, hit, view, true);
        }
        let color = if enabled {
            t.text
        } else {
            t.text_dim.with_alpha(110)
        };
        fonts.icon(
            &mut b,
            g,
            Point::new(r.x + r.width / 2.0, r.y + r.height / 2.0),
            16.0,
            color,
        );
    }
    address(&mut b, view, layout, fonts);
    b.build()
}

fn address(b: &mut DisplayListBuilder, view: &ChromeView, layout: &Layout, fonts: &Fonts) {
    let t = &view.theme;
    let a = &view.address;
    let r = layout.address;
    let fill = if a.focused {
        t.field_focused
    } else if view.hover == Hit::Address {
        t.field_hover
    } else {
        t.field
    };
    let radii = Radii::uniform(r.height / 2.0);
    b.fill_rounded_rect(r, radii, fill);
    if a.focused {
        b.stroke_rounded_rect(
            r.inflate(-1.0, -1.0),
            Radii::uniform(r.height / 2.0 - 1.0),
            2.0,
            t.accent,
        );
    }
    let cy = r.y + r.height / 2.0;
    let (g, color) = match a.secure {
        Some(true) => (glyph::LOCK, t.text_dim),
        Some(false) => (glyph::INFO, t.text_dim),
        None => (glyph::SEARCH, t.text_dim),
    };
    if a.secure.is_some() {
        button_bg(b, layout.site_info, Hit::SiteInfo, view, true);
    }
    fonts.icon(b, g, Point::new(r.x + 18.0, cy), 13.0, color);
    if !layout.popup && (!a.focused || a.field.text().is_empty()) {
        button_bg(b, layout.star, Hit::Star, view, true);
        let s = layout.star;
        let (g, color) = if view.starred {
            (glyph::STAR_FILL, t.accent)
        } else {
            (glyph::STAR, t.text_dim)
        };
        fonts.icon(
            b,
            g,
            Point::new(s.x + s.width / 2.0, s.y + s.height / 2.0),
            14.0,
            color,
        );
    }
    let area = layout.address_text;
    let text = a.field.text();
    b.push_clip_rect(Rect::new(
        area.x - 2.0,
        area.y,
        area.width + 4.0,
        area.height,
    ));
    let x0 = area.x - a.scroll;
    let m_height = ADDRESS_SIZE * 1.3;
    if text.is_empty() {
        fonts.label(
            b,
            "Search or type an address",
            area.x,
            cy,
            ADDRESS_SIZE,
            t.text_dim,
            area.width,
        );
    } else {
        if a.focused && a.field.has_selection() {
            let sel = a.field.selection();
            let s0 = fonts.caret_x(text, sel.start, ADDRESS_SIZE);
            let s1 = fonts.caret_x(text, sel.end, ADDRESS_SIZE);
            b.fill_rect(
                Rect::new(x0 + s0, cy - m_height / 2.0, s1 - s0, m_height),
                t.selection,
            );
        }
        if a.focused {
            fonts.label(b, text, x0, cy, ADDRESS_SIZE, t.text, f32::INFINITY);
        } else {
            // The scheme dimmed, the rest plain.
            let split = text.find("://").map_or(0, |i| i + 3);
            let w = if split > 0 {
                fonts.label(
                    b,
                    &text[..split],
                    x0,
                    cy,
                    ADDRESS_SIZE,
                    t.text_dim,
                    f32::INFINITY,
                )
            } else {
                0.0
            };
            fonts.label(
                b,
                &text[split..],
                x0 + w,
                cy,
                ADDRESS_SIZE,
                t.text,
                (area.width - w).max(0.0),
            );
        }
    }
    if a.focused && a.caret_visible && !a.field.has_selection() {
        let cx = (x0 + fonts.caret_x(text, a.field.caret(), ADDRESS_SIZE)).round();
        b.fill_rect(Rect::new(cx, cy - m_height / 2.0, 1.0, m_height), t.text);
    }
    b.pop_clip();
    if let Some(z) = a.zoom {
        let label = zoom_label(Some(z));
        let w = fonts.width(&label, UI_SIZE);
        fonts.label(
            b,
            &label,
            layout.star.x - 6.0 - w,
            cy,
            UI_SIZE,
            t.text_dim,
            f32::INFINITY,
        );
    }
}

/// The bookmarks bar: its items with their icons, the "»" button, a drag's insertion marker.
fn bookmarks_bar(b: &mut DisplayListBuilder, view: &ChromeView, bar: &BarLayout, fonts: &Fonts) {
    let t = &view.theme;
    if view.bar.is_empty() {
        fonts.label(
            b,
            "For quick access, place your bookmarks here on the bookmarks bar (Ctrl+D bookmarks this page).",
            12.0,
            bar.rect.y + bar.rect.height / 2.0,
            UI_SIZE,
            t.text_dim,
            (bar.rect.width - 24.0).max(0.0),
        );
        return;
    }
    for (i, (item, r)) in view.bar.iter().zip(&bar.items).enumerate() {
        let hit = Hit::Bookmark(i);
        let into = view.bar_drop.is_some_and(|(_, f)| f == Some(i));
        if into {
            b.fill_rounded_rect(*r, Radii::uniform(r.height / 2.0), t.button_pressed);
        } else {
            button_bg(b, *r, hit, view, true);
        }
        let cy = r.y + r.height / 2.0;
        let icon = Rect::new(r.x + 8.0, cy - 8.0, 16.0, 16.0);
        match (item.folder, item.icon) {
            (true, _) => fonts.icon(
                b,
                glyph::FOLDER,
                Point::new(icon.x + 8.0, cy),
                14.0,
                t.text_dim,
            ),
            (false, Some(id)) => b.image(id, icon, None, 1.0, ImageRendering::Smooth),
            (false, None) => fonts.icon(
                b,
                glyph::GLOBE,
                Point::new(icon.x + 8.0, cy),
                13.0,
                t.text_dim,
            ),
        }
        let x = icon.right() + 6.0;
        if r.right() - 10.0 > x {
            fonts.label(b, item.label, x, cy, UI_SIZE, t.text, r.right() - 10.0 - x);
        }
    }
    if let Some(o) = bar.overflow {
        button_bg(b, o, Hit::BookmarkOverflow, view, true);
        fonts.label(
            b,
            "»",
            o.x + o.width / 2.0 - 4.0,
            o.y + o.height / 2.0,
            14.0,
            t.text,
            f32::INFINITY,
        );
    }
    if let Some((x, None)) = view.bar_drop {
        b.fill_rect(
            Rect::new(x - 1.0, bar.rect.y + 4.0, 2.0, bar.rect.height - 8.0),
            t.accent,
        );
    }
}

/// A loading spinner: twelve dots around a circle, fading behind the leading one.
pub(crate) fn spinner(b: &mut DisplayListBuilder, center: Point, angle: f32, color: Color) {
    let n = 12;
    for i in 0..n {
        let a = angle - i as f32 * std::f32::consts::TAU / n as f32;
        let alpha = (255.0 * (1.0 - i as f32 / n as f32)).max(30.0) as u8;
        b.push_transform(Matrix::rotation(a).then(&Matrix::translation(center.x, center.y)));
        b.fill_rounded_rect(
            Rect::new(-1.25, -7.0, 2.5, 4.0),
            Radii::uniform(1.25),
            color.with_alpha(alpha),
        );
        b.pop_transform();
    }
}

/// One address suggestion.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub search: bool,
    pub title: String,
    pub url: String,
}

/// What shows over the page (content-area DIPs).
#[derive(Clone, Debug, Default)]
pub struct OverlayView<'a> {
    pub status: Option<&'a str>,
    pub suggestions: &'a [Suggestion],
    pub selected: Option<usize>,
    pub hover: Option<usize>,
    /// A select's open list.
    pub menu: Option<&'a crate::popup::SelectMenu>,
    /// A `title` tooltip.
    pub tooltip: Option<&'a crate::tooltip::Tooltip>,
}

/// The suggestion dropdown's rectangle and row height (content-area DIPs).
pub fn suggestions_rect(layout: &Layout, count: usize) -> Rect {
    Rect::new(
        layout.address.x,
        2.0,
        layout.address.width,
        count as f32 * SUGGESTION_ROW + 8.0,
    )
}

/// The overlay display list; `page` is the page's part of the content area (content-area DIPs).
pub fn build_overlay(
    view: &OverlayView,
    layout: &Layout,
    page: Rect,
    fonts: &Fonts,
    t: &Theme,
) -> DisplayList {
    let mut b = DisplayListBuilder::new();
    if let Some(status) = view.status.filter(|s| !s.is_empty()) {
        let max = (page.width / 2.0).max(120.0);
        let w = (fonts.width(status, UI_SIZE) + 16.0).min(max);
        let r = Rect::new(page.x, page.bottom() - 24.0, w, 24.0);
        b.fill_rounded_rect(
            r,
            Radii {
                top_left: Size::ZERO,
                top_right: Size::new(6.0, 6.0),
                bottom_right: Size::ZERO,
                bottom_left: Size::ZERO,
            },
            t.bubble_border,
        );
        b.fill_rounded_rect(
            Rect::new(r.x, r.y + 1.0, r.width - 1.0, r.height - 1.0),
            Radii {
                top_left: Size::ZERO,
                top_right: Size::new(5.0, 5.0),
                bottom_right: Size::ZERO,
                bottom_left: Size::ZERO,
            },
            t.bubble,
        );
        b.push_clip_rect(r);
        fonts.label(
            &mut b,
            status,
            8.0,
            r.y + 12.5,
            UI_SIZE,
            t.text_dim,
            w - 16.0,
        );
        b.pop_clip();
    }
    if !view.suggestions.is_empty() {
        let r = suggestions_rect(layout, view.suggestions.len());
        b.box_shadow(snowlark_paint::BoxShadow {
            rect: r,
            radii: Radii::uniform(12.0),
            offset: Point::new(0.0, 4.0),
            blur: 16.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, if t.dark { 120 } else { 50 }),
            inset: false,
        });
        b.fill_rounded_rect(r, Radii::uniform(12.0), t.dropdown);
        for (i, s) in view.suggestions.iter().enumerate() {
            let row = Rect::new(
                r.x + 4.0,
                r.y + 4.0 + i as f32 * SUGGESTION_ROW,
                r.width - 8.0,
                SUGGESTION_ROW,
            );
            if view.selected == Some(i) || view.hover == Some(i) {
                b.fill_rounded_rect(row, Radii::uniform(8.0), t.dropdown_hover);
            }
            let cy = row.y + row.height / 2.0;
            fonts.icon(
                &mut b,
                if s.search {
                    glyph::SEARCH
                } else {
                    glyph::HISTORY
                },
                Point::new(row.x + 18.0, cy),
                13.0,
                t.text_dim,
            );
            let x = row.x + 40.0;
            let room = row.right() - 12.0 - x;
            b.push_clip_rect(Rect::new(x, row.y, room.max(0.0), row.height));
            let w = fonts.label(&mut b, &s.title, x, cy, ADDRESS_SIZE, t.text, room * 0.6);
            if !s.url.is_empty() {
                let dash = " – ";
                let dw = fonts.label(
                    &mut b,
                    dash,
                    x + w,
                    cy,
                    ADDRESS_SIZE,
                    t.text_dim,
                    f32::INFINITY,
                );
                fonts.label(
                    &mut b,
                    &s.url,
                    x + w + dw,
                    cy,
                    ADDRESS_SIZE,
                    t.accent,
                    (room - w - dw).max(0.0),
                );
            }
            b.pop_clip();
        }
    }
    if let Some(tip) = view.tooltip {
        tip.paint(&mut b, fonts, t);
    }
    if let Some(m) = view.menu {
        m.paint(&mut b, fonts);
    }
    b.build()
}

/// Font size of the address bar's text (for caret math by the window).
pub const fn address_font_size() -> f32 {
    ADDRESS_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_downloads_button_shows_running_and_recent_downloads() {
        let now = 10_000_000;
        let min = 60_000;
        // None at all, or only ones that ended over an hour ago: no button.
        assert_eq!(downloads_button([], now), None);
        assert_eq!(downloads_button([(false, 5, 5, now - 61 * min)], now), None);
        // Ended within the hour: the button; within the minute: in the accent colour.
        assert_eq!(
            downloads_button([(false, 5, 5, now - 30 * min)], now),
            Some(DownloadsButton {
                running: None,
                fresh: false
            })
        );
        assert_eq!(
            downloads_button(
                [(false, 5, 5, now - 30 * min), (false, 1, 1, now - 1000)],
                now
            ),
            Some(DownloadsButton {
                running: None,
                fresh: true
            })
        );
        // Running: the share of their bytes in, or unknown when a size is.
        assert_eq!(
            downloads_button([(true, 25, 100, 0), (true, 25, 100, 0)], now),
            Some(DownloadsButton {
                running: Some(Some(0.25)),
                fresh: false
            })
        );
        assert_eq!(
            downloads_button([(true, 25, 100, 0), (true, 5, 0, 0)], now),
            Some(DownloadsButton {
                running: Some(None),
                fresh: false
            })
        );
        // Its place: after the content blocker's, before an Incognito button and the menu.
        for incognito_first in [false, true] {
            let plain = Layout::new(1000.0, 2, 0.0, None);
            assert_eq!(plain.downloads, None);
            let l = if incognito_first {
                plain.clone().with_incognito(100.0).with_downloads()
            } else {
                plain.clone().with_downloads().with_incognito(100.0)
            };
            let d = l.downloads.unwrap();
            let i = l.incognito.unwrap();
            assert!(l.shield.right() <= d.x && d.right() <= i.x && i.right() <= l.menu.x);
            assert!(l.address.right() <= l.shield.x);
            assert!(l.star.right() <= l.address.right());
            let mid = Point::new(d.x + d.width / 2.0, d.y + d.height / 2.0);
            assert_eq!(l.hit(mid), Hit::Downloads);
            assert_ne!(plain.hit(mid), Hit::Downloads);
        }
        assert_eq!(Layout::popup(800.0).with_downloads().downloads, None);
        // A ring arc ends where it should.
        let ring = arc(Point::new(0.0, 0.0), 10.0, 0.0, std::f32::consts::PI);
        assert_eq!(ring.len(), 3);
        let PathEl::CurveTo(_, _, end) = ring[2] else {
            panic!("{ring:?}");
        };
        assert!((end.x + 10.0).abs() < 1e-4 && end.y.abs() < 1e-4, "{end:?}");
    }

    #[test]
    fn incognito_button_sits_before_the_menu_and_says_how_many_windows() {
        assert_eq!(incognito_label(1), "Incognito");
        assert_eq!(incognito_label(3), "Incognito (3)");
        for side in [false, true] {
            let plain = if side {
                let spec = SideSpec {
                    width: 240.0,
                    height: 800.0,
                    scroll: 0.0,
                    collapsed: false,
                    active: 0,
                    drag: None,
                };
                Layout::with_side_tabs(1280.0, 3, 0.0, None, &spec)
            } else {
                Layout::new(1280.0, 3, 0.0, None)
            };
            assert_eq!(plain.incognito, None);
            let l = plain.clone().with_incognito(100.0);
            let r = l.incognito.unwrap();
            // Between the content blocker's button and the menu, on the toolbar's row.
            assert_eq!(r.width, 100.0);
            assert_eq!((r.y, r.height), (l.menu.y, l.menu.height));
            assert!(l.shield.right() <= r.x && r.right() <= l.menu.x);
            assert!(l.address.right() <= l.shield.x);
            assert_eq!(l.address.width, plain.address.width - 102.0);
            assert!(l.star.right() <= l.address.right());
            assert_eq!(l.menu, plain.menu);
            let mid = Point::new(r.x + r.width / 2.0, r.y + r.height / 2.0);
            assert_eq!(l.hit(mid), Hit::Incognito);
            assert_ne!(plain.hit(mid), Hit::Incognito);
            assert_eq!(
                l.hit(Point::new(l.shield.x + 5.0, l.shield.y + 5.0)),
                Hit::Shield
            );
        }
        // A popup window's location line has none.
        assert_eq!(Layout::popup(800.0).with_incognito(100.0).incognito, None);
        // The mark: the mask and its two eyes, inside its box.
        let mark = incognito_mark(Point::new(50.0, 50.0), 18.0);
        let closes = mark.iter().filter(|e| matches!(e, PathEl::Close)).count();
        assert_eq!(closes, 3);
        for e in &mark {
            if let PathEl::MoveTo(p) | PathEl::LineTo(p) | PathEl::CurveTo(_, _, p) = e {
                assert!((41.0..=59.0).contains(&p.x) && (41.0..=59.0).contains(&p.y));
            }
        }
    }

    #[test]
    fn incognito_theme_is_dark_and_not_the_dark_theme() {
        let t = incognito_theme();
        let dark = theme(true);
        assert!(t.dark);
        assert_ne!(t.frame, dark.frame);
        assert_ne!(t.toolbar, dark.toolbar);
        assert_ne!(t.field, dark.field);
        // (neutral greys: no tint)
        for c in [t.frame, t.toolbar, t.field, t.tab_active] {
            assert!(c.r.abs_diff(c.g) <= 1 && c.g.abs_diff(c.b) <= 1, "{c:?}");
        }
    }

    #[test]
    fn layout_hits() {
        let l = Layout::new(1280.0, 3, 0.0, None);
        assert_eq!(l.tabs.len(), 3);
        assert_eq!(l.tabs[0].width, TAB_MAX);
        assert_eq!(l.hit(Point::new(l.tabs[1].x + 20.0, 20.0)), Hit::Tab(1));
        let c = l.tab_close(2).unwrap();
        assert_eq!(l.hit(Point::new(c.x + 10.0, c.y + 10.0)), Hit::TabClose(2));
        assert_eq!(l.hit(Point::new(1279.0, 5.0)), Hit::Close);
        assert_eq!(l.hit(Point::new(1280.0 - 46.0 - 20.0, 5.0)), Hit::Maximize);
        assert_eq!(
            l.hit(Point::new(l.new_tab.x + 5.0, l.new_tab.y + 5.0)),
            Hit::NewTab
        );
        // Between the new tab button and the caption buttons: the caption.
        assert_eq!(l.hit(Point::new(1000.0, 20.0)), Hit::Caption);
        assert_eq!(l.hit(Point::new(l.back.x + 3.0, l.back.y + 3.0)), Hit::Back);
        assert_eq!(l.hit(Point::new(600.0, 60.0)), Hit::Address);
        assert_eq!(l.hit(Point::new(600.0, 200.0)), Hit::None);
        assert!(l.address.right() <= l.menu.x);
        assert_eq!(l.hit(Point::new(l.menu.x + 5.0, l.menu.y + 5.0)), Hit::Menu);
        assert_eq!(l.hit(Point::new(l.star.x + 5.0, l.star.y + 5.0)), Hit::Star);
        assert_eq!(
            l.hit(Point::new(l.site_info.x + 5.0, l.site_info.y + 5.0)),
            Hit::SiteInfo
        );
        assert_eq!(l.height(), CHROME_HEIGHT);
    }

    /// Tabs at the side: the toolbar is the top row with the caption buttons at its end, the
    /// bookmarks bar under it, then the panel down the left with the tab rows.
    #[test]
    fn side_tabs_layout_hits() {
        let spec = SideSpec {
            width: 240.0,
            height: 800.0,
            scroll: 0.0,
            collapsed: false,
            active: 0,
            drag: None,
        };
        let l = Layout::with_side_tabs(1280.0, 3, 0.0, Some(&[80.0]), &spec)
            .with_audio(vec![false, true, false]);
        assert_eq!(l.height(), TOOLBAR_HEIGHT + BOOKMARKS_BAR_HEIGHT);
        let side = l.side.as_ref().unwrap();
        assert_eq!(
            side.rect,
            Rect::new(0.0, l.height(), 240.0, 800.0 - l.height())
        );
        assert_eq!(l.tabs, side.tabs);
        assert!(l.back.y < TOOLBAR_HEIGHT && l.address.bottom() <= TOOLBAR_HEIGHT);
        assert!(l.menu.right() + 40.0 <= l.minimize.x);
        assert_eq!(l.hit(Point::new(1279.0, 5.0)), Hit::Close);
        // The room before the caption buttons drags the window.
        assert_eq!(l.hit(Point::new(l.minimize.x - 10.0, 20.0)), Hit::Caption);
        assert_eq!(l.hit(Point::new(600.0, 20.0)), Hit::Address);
        assert_eq!(l.hit(Point::new(l.menu.x + 5.0, l.menu.y + 5.0)), Hit::Menu);
        let bar = l.bar.as_ref().unwrap();
        assert_eq!(bar.rect.y, TOOLBAR_HEIGHT);
        assert_eq!(
            l.hit(Point::new(bar.items[0].x + 5.0, bar.rect.y + 10.0)),
            Hit::Bookmark(0)
        );
        let t1 = l.tabs[1];
        assert_eq!(l.hit(Point::new(t1.x + 30.0, t1.y + 10.0)), Hit::Tab(1));
        let c = l.tab_close(1).unwrap();
        assert_eq!(l.hit(Point::new(c.x + 5.0, c.y + 5.0)), Hit::TabClose(1));
        let a = l.tab_audio(1).unwrap();
        assert_eq!(l.hit(Point::new(a.x + 5.0, a.y + 5.0)), Hit::TabAudio(1));
        assert_eq!(
            l.hit(Point::new(l.new_tab.x + 5.0, l.new_tab.y + 5.0)),
            Hit::NewTab
        );
        assert_eq!(l.hit(Point::new(100.0, 500.0)), Hit::TabPanel);
        // Right of the panel is the page.
        assert_eq!(l.hit(Point::new(300.0, 500.0)), Hit::None);
        // Maximized: the screen's top edge is on the toolbar's buttons.
        let m = l.clone().with_maximized(true);
        assert_eq!(m.hit(Point::new(m.back.x + 5.0, 0.0)), Hit::Back);
        // The list is drawn (and valid) with the panel, not with the chrome.
        let f = Fonts::new(Arc::new(TextEngine::new().unwrap()));
        let field = TextField::new("https://example.com/");
        let view = ChromeView {
            tabs: (0..3)
                .map(|_| TabView {
                    title: "A",
                    icon: None,
                    loading: false,
                    crashed: false,
                    audio: crate::audio::TabAudio::Playing,
                })
                .collect(),
            active: 0,
            hover: Hit::Tab(2),
            pressed: Hit::None,
            caption_hover: None,
            maximized: false,
            window_active: true,
            can_back: true,
            can_forward: false,
            loading: false,
            address: AddressView {
                field: &field,
                focused: false,
                caret_visible: false,
                scroll: 0.0,
                secure: Some(true),
                zoom: None,
            },
            spinner: 1.0,
            theme: theme(true),
            bar: Vec::new(),
            starred: false,
            open: Hit::None,
            bar_drop: None,
            blocker: None,
            incognito: None,
            downloads: None,
        };
        let mut b = DisplayListBuilder::new();
        crate::side_tabs::paint(&mut b, &view, &l, &f);
        assert!(b.build().validate().is_ok());
        assert!(build(&view, &l, &f).validate().is_ok());
    }

    /// A fullscreen window has no chrome: nothing is laid out, hit or drawn (the compositor paints
    /// the chrome list over the page wherever it is, so the list must be empty).
    #[test]
    fn a_fullscreen_layout_has_no_chrome() {
        let l = Layout::new(1280.0, 2, 0.0, Some(&[80.0, 90.0])).with_fullscreen(true);
        assert_eq!(l.height(), 0.0);
        assert_eq!(l.hit(Point::new(l.tabs[0].x + 20.0, 20.0)), Hit::None);
        assert_eq!(l.hit(Point::new(1279.0, 5.0)), Hit::None);
        let fonts = Fonts::new(Arc::new(TextEngine::new().unwrap()));
        let field = TextField::default();
        let view = ChromeView {
            tabs: vec![
                TabView {
                    title: "a",
                    icon: None,
                    loading: false,
                    crashed: false,
                    audio: Default::default(),
                },
                TabView {
                    title: "b",
                    icon: None,
                    loading: true,
                    crashed: false,
                    audio: Default::default(),
                },
            ],
            active: 0,
            hover: Hit::None,
            pressed: Hit::None,
            caption_hover: None,
            maximized: true,
            window_active: true,
            can_back: true,
            can_forward: false,
            loading: true,
            address: AddressView {
                field: &field,
                focused: false,
                caret_visible: false,
                scroll: 0.0,
                secure: Some(true),
                zoom: None,
            },
            spinner: 0.0,
            theme: theme(true),
            bar: Vec::new(),
            starred: false,
            open: Hit::None,
            bar_drop: None,
            blocker: None,
            incognito: None,
            downloads: None,
        };
        let list = build(&view, &l, &fonts);
        assert!(list.items.is_empty(), "{} items drawn", list.items.len());
    }

    #[test]
    fn a_maximized_windows_tabs_reach_the_top_edge() {
        let restored = Layout::new(1280.0, 3, 0.0, None);
        let maximized = restored.clone().with_maximized(true);
        let x = restored.tabs[1].x + 20.0;
        // Restored: the rows above the tabs are caption (to drag and resize the window by).
        assert_eq!(restored.hit(Point::new(x, 0.0)), Hit::Caption);
        // Maximized: they are the tab's, up to the screen's top edge.
        assert_eq!(maximized.hit(Point::new(x, 0.0)), Hit::Tab(1));
        assert_eq!(maximized.hit(Point::new(x, TAB_TOP - 1.0)), Hit::Tab(1));
        // (not its close button: the edge selects)
        let c = maximized.tab_close(1).unwrap();
        assert_eq!(maximized.hit(Point::new(c.x + 10.0, 0.0)), Hit::Tab(1));
        // Beside the tabs it stays caption, and the caption buttons keep the corner.
        assert_eq!(maximized.hit(Point::new(1000.0, 0.0)), Hit::Caption);
        assert_eq!(maximized.hit(Point::new(1279.0, 0.0)), Hit::Close);
    }

    #[test]
    fn the_bookmarks_bar_overflows_and_finds_drop_places() {
        let widths = [100.0, 120.0, 80.0, 300.0, 50.0];
        let l = Layout::new(560.0, 1, 0.0, Some(&widths));
        assert_eq!(l.height(), CHROME_HEIGHT + BOOKMARKS_BAR_HEIGHT);
        let bar = l.bar.as_ref().unwrap();
        assert_eq!(bar.items.len(), 3);
        assert!(bar.overflow.is_some());
        let y = CHROME_HEIGHT + 10.0;
        assert_eq!(l.hit(Point::new(bar.items[1].x + 5.0, y)), Hit::Bookmark(1));
        assert_eq!(l.hit(Point::new(545.0, y)), Hit::BookmarkOverflow);
        assert_eq!(l.hit(Point::new(400.0, y)), Hit::BookmarksBar);
        // Everything fits: no chevron, the last item may use its room.
        let l = Layout::new(560.0, 1, 0.0, Some(&[100.0, 400.0]));
        assert!(l.bar.as_ref().unwrap().overflow.is_none());
        // Dropping: before or after an item by its halves, into a folder but for its edges.
        let bar = BarLayout::new(600.0, 0.0, &[100.0, 100.0]);
        let folders = [false, true];
        assert_eq!(bar.drop_target(10.0, &folders), (0, None));
        assert_eq!(bar.drop_target(90.0, &folders), (1, None));
        assert_eq!(bar.drop_target(160.0, &folders), (1, Some(1)));
        assert_eq!(bar.drop_target(bar.items[1].x + 3.0, &folders), (1, None));
        assert_eq!(
            bar.drop_target(bar.items[1].x + 6.0, &folders),
            (1, Some(1))
        );
        assert_eq!(bar.drop_target(205.0, &folders), (2, None));
        assert_eq!(bar.drop_target(500.0, &folders), (2, None));
        assert!(bar.marker_x(2) > bar.items[1].right());
        assert_eq!(bookmark_width(500.0, true), 40.0 + BOOKMARK_MAX_TEXT);
    }

    #[test]
    fn audio_indicators_sit_right_of_the_favicon_and_hit() {
        let l = Layout::new(1280.0, 3, 0.0, None).with_audio(vec![false, true]);
        assert!(l.tab_audio(0).is_none());
        assert!(l.tab_audio(2).is_none(), "past the flags: none");
        let a = l.tab_audio(1).unwrap();
        let tab = l.tabs[1];
        assert!(a.x >= tab.x + 12.0 + ICON_SIZE, "after the favicon");
        assert!(a.right() < l.tab_close(1).unwrap().x);
        assert_eq!(
            l.hit(Point::new(a.x + a.width / 2.0, a.y + a.height / 2.0)),
            Hit::TabAudio(1)
        );
        // The same place on a tab without sound is the tab.
        let x0 = l.tabs[0].x + (a.x - tab.x) + 5.0;
        assert_eq!(l.hit(Point::new(x0, a.y + 5.0)), Hit::Tab(0));
        // The narrowest tabs still have room for it.
        let n = Layout::new(800.0, 30, 0.0, None).with_audio(vec![true; 30]);
        assert!(n.tab_audio(29).is_some());
    }

    #[test]
    fn many_tabs_shrink_and_lose_close_buttons() {
        let l = Layout::new(800.0, 30, 0.0, None);
        assert_eq!(l.tabs[0].width, TAB_MIN);
        assert!(l.tab_close(0).is_none());
        // The new tab button stays left of the caption buttons.
        assert!(l.new_tab.right() <= l.minimize.x);
    }

    #[test]
    fn chrome_and_overlay_lists_are_valid() {
        let fonts = Fonts::new(Arc::new(TextEngine::new().unwrap()));
        let field = TextField::new("https://news.ycombinator.com/");
        let theme = theme(false);
        let view = ChromeView {
            tabs: vec![
                TabView {
                    title: "Hacker News",
                    icon: None,
                    loading: false,
                    crashed: false,
                    audio: crate::audio::TabAudio::Playing,
                },
                TabView {
                    title: "A very long title that will not fit in the tab at all, no way",
                    icon: None,
                    loading: true,
                    crashed: false,
                    audio: crate::audio::TabAudio::Muted,
                },
            ],
            active: 0,
            hover: Hit::TabAudio(1),
            pressed: Hit::None,
            caption_hover: Some(CaptionButton::Close),
            maximized: false,
            window_active: true,
            can_back: true,
            can_forward: false,
            loading: false,
            address: AddressView {
                field: &field,
                focused: true,
                caret_visible: true,
                scroll: 0.0,
                secure: Some(true),
                zoom: Some(125),
            },
            spinner: 1.0,
            theme,
            bar: vec![
                BarItemView {
                    label: "Hacker News",
                    icon: None,
                    folder: false,
                },
                BarItemView {
                    label: "Folder",
                    icon: None,
                    folder: true,
                },
            ],
            starred: true,
            open: Hit::Menu,
            bar_drop: Some((40.0, None)),
            blocker: Some(BlockerBadge {
                count: 12,
                up: true,
            }),
            incognito: None,
            downloads: None,
        };
        let layout =
            Layout::new(1280.0, 2, 40.0, Some(&[120.0, 80.0])).with_audio(vec![true, true]);
        let list = build(&view, &layout, &fonts);
        list.validate().unwrap();
        assert!(list.items.len() > 20);
        // An Incognito window's: its button (the mark is a path) in its own colours.
        let label = incognito_label(2);
        let incognito_view = ChromeView {
            incognito: Some(label.clone()),
            theme: incognito_theme(),
            hover: Hit::Incognito,
            ..view.clone()
        };
        let incognito_layout = layout
            .clone()
            .with_incognito(incognito_button_width(&fonts, &label));
        let incognito_list = build(&incognito_view, &incognito_layout, &fonts);
        incognito_list.validate().unwrap();
        assert!(incognito_list.items.len() > list.items.len());
        let fills = |l: &DisplayList| {
            l.items
                .iter()
                .filter(|i| matches!(i, snowlark_paint::Item::FillPath { .. }))
                .count()
        };
        assert_eq!(fills(&incognito_list), fills(&list) + 1);
        // The downloads button: the ring's track and progress are two stroked paths; a finished
        // download's button has none.
        let strokes = |l: &DisplayList| {
            l.items
                .iter()
                .filter(|i| matches!(i, snowlark_paint::Item::StrokePath { .. }))
                .count()
        };
        let downloads_layout = layout.clone().with_downloads();
        let with = |d| {
            let v = ChromeView {
                downloads: d,
                hover: Hit::Downloads,
                ..view.clone()
            };
            let l = build(&v, &downloads_layout, &fonts);
            l.validate().unwrap();
            l
        };
        let none = with(None);
        let running = with(Some(DownloadsButton {
            running: Some(Some(0.4)),
            fresh: false,
        }));
        assert_eq!(strokes(&running), strokes(&none) + 2);
        let done = with(Some(DownloadsButton {
            running: None,
            fresh: true,
        }));
        assert_eq!(strokes(&done), strokes(&none));
        assert!(done.items.len() > none.items.len());
        let suggestions = [Suggestion {
            search: false,
            title: "Hacker News".into(),
            url: "news.ycombinator.com".into(),
        }];
        let overlay = build_overlay(
            &OverlayView {
                status: Some("https://example.com/"),
                suggestions: &suggestions,
                selected: Some(0),
                hover: None,
                menu: None,
                tooltip: None,
            },
            &layout,
            Rect::new(0.0, 0.0, 1280.0, 700.0),
            &fonts,
            &theme,
        );
        overlay.validate().unwrap();
        assert!(!overlay.items.is_empty());
    }

    #[test]
    fn caret_math() {
        let fonts = Fonts::new(Arc::new(TextEngine::new().unwrap()));
        let s = "hello world";
        assert_eq!(fonts.caret_x(s, 0, 14.0), 0.0);
        let mid = fonts.caret_x(s, 5, 14.0);
        let end = fonts.caret_x(s, s.len(), 14.0);
        assert!(mid > 0.0 && mid < end);
        assert_eq!(fonts.index_at(s, -5.0, 14.0), 0);
        assert_eq!(fonts.index_at(s, mid + 0.5, 14.0), 5);
        assert_eq!(fonts.index_at(s, end + 50.0, 14.0), s.len());
    }
}
