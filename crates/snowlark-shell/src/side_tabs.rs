// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tabs down a panel at the window's left instead of the strip at the top: where the panel, its
//! tab rows and its new tab button are ([`SideLayout`]), and drawing it ([`paint`]).
//!
//! The panel is part of the content area, under the toolbar and the bookmarks bar: the page is
//! laid out right of it (and the developer tools split what is left), and the panel is drawn in the
//! overlay layer. Its geometry is in window DIPs like the rest of [`crate::chrome::Layout`], whose
//! hit test gives the same [`Hit`] values the strip gives, so a tab row is clicked, closed, muted,
//! hovered and given its menu as a tab in the strip is.

use snowlark_paint::{BoxShadow, Color, DisplayListBuilder, ImageRendering, Point, Radii, Rect};

use crate::chrome::{glyph, ChromeView, Fonts, Hit, Layout};

/// The panel's width when it opens for the first time.
pub const DEFAULT_WIDTH: f32 = 240.0;
/// The narrowest and widest the panel is with its titles showing.
pub const MIN_WIDTH: f32 = 126.0;
pub const MAX_WIDTH: f32 = 400.0;
/// The collapsed panel's width: favicons only.
pub const COLLAPSED_WIDTH: f32 = 56.0;
/// A resize that leaves the panel this narrow or narrower collapses it.
pub const COLLAPSE_SNAP: f32 = (MIN_WIDTH + COLLAPSED_WIDTH) / 2.0;
/// A resize that ends this close to the default width ends at it.
const SNAP_DISTANCE: f32 = 15.0;
/// The resize area at the panel's trailing edge (inside it), and when collapsed.
const EDGE: f32 = 5.0;
const EDGE_COLLAPSED: f32 = 2.0;
/// The collapse button at the panel's top.
const HEADER_BUTTON: f32 = 28.0;
const HEADER_PAD: f32 = 6.0;
/// Space between the panel's edges and its tab rows.
const PAD_X: f32 = 12.0;
const PAD_Y: f32 = 8.0;
/// A tab row and the space between rows.
pub const TAB_HEIGHT: f32 = 30.0;
const TAB_GAP: f32 = 2.0;
const NEW_TAB_HEIGHT: f32 = 32.0;
const ICON: f32 = 16.0;
/// The favicon's distance from the row's leading edge.
const ICON_INSET: f32 = 8.0;
/// The close button (and the audio indicator before it).
const BUTTON: f32 = 20.0;
const BUTTON_INSET: f32 = 5.0;
const TITLE_SIZE: f32 = 12.0;
/// How far one notch of the wheel scrolls the tab list.
pub const WHEEL_STEP: f32 = 3.0 * (TAB_HEIGHT + TAB_GAP);

/// What the window asks of the panel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SideSpec {
    /// The panel's width (DIPs).
    pub width: f32,
    /// The window's client height (DIPs).
    pub height: f32,
    /// How far the tab list is scrolled (clamped by [`SideLayout::new`]).
    pub scroll: f32,
    /// Favicons only (`width` is then [`COLLAPSED_WIDTH`]).
    pub collapsed: bool,
    /// The active tab (a collapsed row's one button is its close button).
    pub active: usize,
    /// A tab being dragged and where its row's top is (window DIPs): it is drawn there, over the
    /// others, which keep their places.
    pub drag: Option<(usize, f32)>,
}

/// Where the panel's parts are (window DIPs).
#[derive(Clone, Debug, PartialEq)]
pub struct SideLayout {
    /// The whole panel.
    pub rect: Rect,
    /// The part the tab rows scroll in.
    pub list: Rect,
    /// The new tab button, under the list.
    pub new_tab: Rect,
    /// Every tab row where it is at the current scroll (rows outside `list` are not shown).
    pub tabs: Vec<Rect>,
    pub scroll: f32,
    pub max_scroll: f32,
    /// The collapse (expand) button.
    pub collapse: Rect,
    /// The resize area along the trailing edge.
    pub edge: Rect,
    pub collapsed: bool,
    pub active: usize,
    /// The tab being dragged ([`SideSpec::drag`]).
    pub dragging: Option<usize>,
}

/// Where a resize of the panel from `start` DIPs wide, dragged `delta` DIPs, leaves it: collapsed,
/// or its width with titles (clamped, and the default width when it ends near it).
pub fn resized(start: f32, delta: f32) -> Option<f32> {
    let proposed = start + delta;
    if proposed <= COLLAPSE_SNAP {
        return None;
    }
    let w = proposed.clamp(MIN_WIDTH, MAX_WIDTH);
    Some(if (w - DEFAULT_WIDTH).abs() < SNAP_DISTANCE {
        DEFAULT_WIDTH
    } else {
        w.round()
    })
}

impl SideLayout {
    /// The panel under the chrome (`top` DIPs high), `tabs` rows in it.
    pub fn new(top: f32, spec: &SideSpec, tabs: usize) -> SideLayout {
        let rect = Rect::new(0.0, top, spec.width, (spec.height - top).max(0.0));
        let inner = (rect.width - 2.0 * PAD_X).max(1.0);
        let new_tab = Rect::new(
            PAD_X,
            (rect.bottom() - PAD_Y - NEW_TAB_HEIGHT).max(rect.y + PAD_Y),
            inner,
            NEW_TAB_HEIGHT,
        );
        let collapse = Rect::new(
            ((COLLAPSED_WIDTH - HEADER_BUTTON) / 2.0).min(rect.width),
            rect.y + HEADER_PAD,
            HEADER_BUTTON,
            HEADER_BUTTON,
        );
        let edge_w = if spec.collapsed { EDGE_COLLAPSED } else { EDGE };
        let edge = Rect::new(rect.right() - edge_w, rect.y, edge_w, rect.height);
        let list_top = collapse.bottom() + HEADER_PAD;
        let list = Rect::new(
            0.0,
            list_top,
            rect.width,
            (new_tab.y - PAD_Y - list_top).max(0.0),
        );
        let content = if tabs == 0 {
            0.0
        } else {
            tabs as f32 * (TAB_HEIGHT + TAB_GAP) - TAB_GAP
        };
        let max_scroll = (content - list.height).max(0.0);
        let scroll = spec.scroll.clamp(0.0, max_scroll);
        let tabs = (0..tabs)
            .map(|i| {
                let y = match spec.drag {
                    // (a dragged row stays inside the list)
                    Some((d, top)) if d == i => top.min(list.bottom() - TAB_HEIGHT).max(list.y),
                    _ => list.y + i as f32 * (TAB_HEIGHT + TAB_GAP) - scroll,
                };
                Rect::new(PAD_X, y, inner, TAB_HEIGHT)
            })
            .collect();
        SideLayout {
            rect,
            list,
            new_tab,
            tabs,
            scroll,
            max_scroll,
            collapse,
            edge,
            collapsed: spec.collapsed,
            active: spec.active,
            dragging: spec.drag.map(|(i, _)| i),
        }
    }

    /// The place among `count` rows for a dragged row whose top is at `top`: the slot its middle
    /// is nearest.
    pub fn drop_index(&self, top: f32, count: usize) -> usize {
        let slot = (top + self.scroll - self.list.y) / (TAB_HEIGHT + TAB_GAP);
        (slot.round().max(0.0) as usize).min(count.saturating_sub(1))
    }

    /// Whether any of row `r` shows in the list.
    pub fn shows(&self, r: &Rect) -> bool {
        r.bottom() > self.list.y && r.y < self.list.bottom()
    }

    /// A collapsed row's one button (close, audio or nothing over the favicon), centred.
    fn centre(&self, r: &Rect) -> Rect {
        Rect::new(
            r.x + (r.width - BUTTON) / 2.0,
            r.y + (r.height - BUTTON) / 2.0,
            BUTTON,
            BUTTON,
        )
    }

    /// The close button of row `r`.
    pub fn close(&self, r: &Rect) -> Option<Rect> {
        if self.collapsed {
            return Some(self.centre(r));
        }
        if r.width < ICON_INSET + ICON + 2.0 * BUTTON {
            return None;
        }
        Some(Rect::new(
            r.right() - BUTTON_INSET - BUTTON,
            r.y + (r.height - BUTTON) / 2.0,
            BUTTON,
            BUTTON,
        ))
    }

    /// The audio indicator of row `r`, before the close button.
    pub fn audio(&self, r: &Rect) -> Option<Rect> {
        if self.collapsed {
            return Some(self.centre(r));
        }
        let close = self.close(r)?;
        let x = close.x - 2.0 - BUTTON;
        (x >= r.x + ICON_INSET + ICON + 4.0).then(|| Rect::new(x, close.y, BUTTON, BUTTON))
    }

    /// The scroll that brings row `i` fully into the list (the current one if it is).
    pub fn reveal(&self, i: usize) -> f32 {
        let Some(r) = self.tabs.get(i) else {
            return self.scroll;
        };
        let top = r.y + self.scroll - self.list.y;
        let s = if r.y < self.list.y {
            top
        } else if r.bottom() > self.list.bottom() {
            top + TAB_HEIGHT - self.list.height
        } else {
            self.scroll
        };
        s.clamp(0.0, self.max_scroll)
    }

    /// What is at `p` in the panel (`p` inside [`SideLayout::rect`]). `audio`: which tabs show an
    /// audio indicator.
    pub fn hit(&self, p: Point, audio: &[bool]) -> Hit {
        if self.edge.contains(p) {
            return Hit::SideEdge;
        }
        if self.collapse.contains(p) {
            return Hit::SideCollapse;
        }
        if self.new_tab.contains(p) {
            return Hit::NewTab;
        }
        if self.list.contains(p) {
            for (i, r) in self.tabs.iter().enumerate() {
                if !r.contains(p) {
                    continue;
                }
                let has_audio = audio.get(i).copied().unwrap_or(false);
                if self.collapsed {
                    // (one button over the favicon: the active tab's close button, which shows
                    // while the pointer is on it, or an audible tab's speaker)
                    if self.centre(r).contains(p) {
                        if i == self.active {
                            return Hit::TabClose(i);
                        }
                        if has_audio {
                            return Hit::TabAudio(i);
                        }
                    }
                    return Hit::Tab(i);
                }
                if self.close(r).is_some_and(|c| c.contains(p)) {
                    return Hit::TabClose(i);
                }
                if has_audio && self.audio(r).is_some_and(|c| c.contains(p)) {
                    return Hit::TabAudio(i);
                }
                return Hit::Tab(i);
            }
        }
        Hit::TabPanel
    }
}

/// Draws the panel (window DIPs) of `layout`, which has one.
pub fn paint(b: &mut DisplayListBuilder, view: &ChromeView, layout: &Layout, fonts: &Fonts) {
    let Some(side) = &layout.side else {
        return;
    };
    let t = &view.theme;
    let bg = if view.window_active {
        t.frame
    } else {
        t.frame_inactive
    };
    b.fill_rect(side.rect, bg);
    b.fill_rect(
        Rect::new(side.rect.right() - 1.0, side.rect.y, 1.0, side.rect.height),
        t.separator,
    );
    b.push_clip_rect(side.list);
    // (a dragged row last, over the others)
    let count = view.tabs.len().min(side.tabs.len());
    let order = (0..count)
        .filter(|&i| Some(i) != side.dragging)
        .chain(side.dragging.filter(|&i| i < count));
    for i in order {
        let (tab, r) = (&view.tabs[i], &side.tabs[i]);
        if !side.shows(r) {
            continue;
        }
        if side.dragging == Some(i) {
            b.box_shadow(BoxShadow {
                rect: *r,
                radii: Radii::uniform(8.0),
                offset: Point::new(0.0, 2.0),
                blur: 8.0,
                spread: 0.0,
                color: Color::rgba(0, 0, 0, if t.dark { 140 } else { 60 }),
                inset: false,
            });
            b.fill_rounded_rect(*r, Radii::uniform(8.0), bg);
        }
        let active = i == view.active;
        let hovered =
            matches!(view.hover, Hit::Tab(h) | Hit::TabClose(h) | Hit::TabAudio(h) if h == i);
        if active {
            b.fill_rounded_rect(*r, Radii::uniform(8.0), t.tab_active);
        } else if hovered {
            b.fill_rounded_rect(*r, Radii::uniform(8.0), t.tab_hover);
        }
        let audio_glyph = match tab.audio {
            crate::audio::TabAudio::None => None,
            crate::audio::TabAudio::Playing => Some(glyph::VOLUME),
            crate::audio::TabAudio::Muted => Some(glyph::MUTE),
        };
        // Collapsed, a row shows one thing in its middle: the close button of the active tab
        // under the pointer, else an audible tab's speaker, else the favicon.
        let (show_icon, show_audio, show_close) = if side.collapsed {
            let close = active && hovered;
            let audio = !close && audio_glyph.is_some();
            (!close && !audio, audio, close)
        } else {
            (true, true, active || hovered)
        };
        let cy = r.y + r.height / 2.0;
        let icon_x = if side.collapsed {
            r.x + (r.width - ICON) / 2.0
        } else {
            r.x + ICON_INSET
        };
        let icon = Rect::new(icon_x, cy - ICON / 2.0, ICON, ICON);
        let center = Point::new(icon.x + ICON / 2.0, cy);
        if !show_icon {
            // (the row's one place is taken)
        } else if tab.loading {
            crate::chrome::spinner(b, center, view.spinner, t.accent);
        } else if tab.crashed {
            fonts.icon(b, glyph::SAD, center, 14.0, t.text_dim);
        } else if let Some(id) = tab.icon {
            b.image(id, icon, None, 1.0, ImageRendering::Smooth);
        } else {
            fonts.icon(b, glyph::GLOBE, center, 14.0, t.text_dim);
        }
        let title_color = if active { t.text } else { t.text_dim };
        // The close button shows on the active tab and the one under the pointer.
        let close = side.close(r).filter(|_| show_close);
        let mut text_right = close.map_or(r.right() - 8.0, |c| c.x - 2.0);
        if let (Some(g), Some(a), true) = (audio_glyph, side.audio(r), show_audio) {
            if view.hover == Hit::TabAudio(i) {
                let bg = if view.pressed == Hit::TabAudio(i) {
                    t.button_pressed
                } else {
                    t.button_hover
                };
                b.fill_rounded_rect(a, Radii::uniform(BUTTON / 2.0), bg);
            }
            fonts.icon(
                b,
                g,
                Point::new(a.x + a.width / 2.0, a.y + a.height / 2.0),
                crate::chrome::AUDIO_GLYPH,
                title_color,
            );
            text_right = a.x - 2.0;
        }
        let text_x = icon.right() + 8.0;
        if !side.collapsed && text_right > text_x + 4.0 {
            b.push_clip_rect(Rect::new(text_x, r.y, text_right - text_x, r.height));
            let title = if tab.title.is_empty() {
                "New tab"
            } else {
                tab.title
            };
            fonts.label(
                b,
                title,
                text_x,
                cy,
                TITLE_SIZE,
                title_color,
                text_right - text_x,
            );
            b.pop_clip();
        }
        if let Some(c) = close {
            if view.hover == Hit::TabClose(i) {
                b.fill_rounded_rect(c, Radii::uniform(BUTTON / 2.0), t.button_hover);
            }
            fonts.icon(
                b,
                glyph::CLOSE,
                Point::new(c.x + c.width / 2.0, c.y + c.height / 2.0),
                8.0,
                t.text,
            );
        }
    }
    b.pop_clip();
    // The collapse button.
    let cb = side.collapse;
    if view.hover == Hit::SideCollapse {
        let c = if view.pressed == Hit::SideCollapse {
            t.button_pressed
        } else {
            t.button_hover
        };
        b.fill_rounded_rect(cb, Radii::uniform(6.0), c);
    }
    fonts.icon(
        b,
        if side.collapsed {
            glyph::PANE_ARROW_RIGHT
        } else {
            glyph::PANE_ARROW_LEFT
        },
        Point::new(cb.x + cb.width / 2.0, cb.y + cb.height / 2.0),
        16.0,
        t.text,
    );
    // The new tab button.
    let nt = side.new_tab;
    if view.hover == Hit::NewTab {
        let c = if view.pressed == Hit::NewTab {
            t.button_pressed
        } else {
            t.button_hover
        };
        b.fill_rounded_rect(nt, Radii::uniform(8.0), c);
    }
    let cy = nt.y + nt.height / 2.0;
    fonts.icon(
        b,
        glyph::ADD,
        Point::new(nt.x + ICON_INSET + ICON / 2.0, cy),
        12.0,
        t.text,
    );
    let text_x = nt.x + ICON_INSET + ICON + 8.0;
    if nt.right() - 8.0 > text_x + 4.0 {
        fonts.label(
            b,
            "New tab",
            text_x,
            cy,
            TITLE_SIZE,
            t.text,
            nt.right() - 8.0 - text_x,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(height: f32, scroll: f32) -> SideSpec {
        SideSpec {
            width: DEFAULT_WIDTH,
            height,
            scroll,
            collapsed: false,
            active: 0,
            drag: None,
        }
    }

    #[test]
    fn rows_stack_under_the_collapse_button_with_the_new_tab_button_at_the_bottom() {
        let s = SideLayout::new(70.0, &spec(600.0, 0.0), 3);
        assert_eq!(s.rect, Rect::new(0.0, 70.0, 240.0, 530.0));
        assert_eq!(s.collapse, Rect::new(14.0, 76.0, 28.0, 28.0));
        assert_eq!(s.tabs[0], Rect::new(12.0, 110.0, 216.0, 30.0));
        assert_eq!(s.tabs[1].y, 142.0);
        assert_eq!(s.new_tab.bottom(), 592.0);
        assert_eq!(s.max_scroll, 0.0);
        let audio = [false, true, false];
        assert_eq!(s.hit(Point::new(20.0, 80.0), &audio), Hit::SideCollapse);
        assert_eq!(s.hit(Point::new(238.0, 300.0), &audio), Hit::SideEdge);
        assert_eq!(s.hit(Point::new(40.0, 150.0), &audio), Hit::Tab(1));
        let c = s.close(&s.tabs[1]).unwrap();
        assert_eq!(
            s.hit(Point::new(c.x + 5.0, c.y + 5.0), &audio),
            Hit::TabClose(1)
        );
        let a = s.audio(&s.tabs[1]).unwrap();
        assert_eq!(
            s.hit(Point::new(a.x + 5.0, a.y + 5.0), &audio),
            Hit::TabAudio(1)
        );
        // (no indicator: the place is the tab)
        let a0 = s.audio(&s.tabs[0]).unwrap();
        assert_eq!(
            s.hit(Point::new(a0.x + 5.0, a0.y + 5.0), &audio),
            Hit::Tab(0)
        );
        assert_eq!(s.hit(Point::new(40.0, 300.0), &audio), Hit::TabPanel);
        assert_eq!(s.hit(Point::new(40.0, 580.0), &audio), Hit::NewTab);
    }

    #[test]
    fn a_long_list_scrolls_and_reveals_a_row() {
        let s = SideLayout::new(70.0, &spec(400.0, 1e6), 40);
        let content = 40.0 * 32.0 - 2.0;
        assert_eq!(s.max_scroll, content - s.list.height);
        assert_eq!(s.scroll, s.max_scroll);
        assert!(!s.shows(&s.tabs[0]) && s.shows(&s.tabs[39]));
        // A row above the list scrolls to the top edge, one below to the bottom edge.
        let up = SideLayout::new(70.0, &spec(400.0, s.reveal(3)), 40);
        assert_eq!(up.tabs[3].y, up.list.y);
        let down = SideLayout::new(70.0, &spec(400.0, up.reveal(30)), 40);
        assert_eq!(down.tabs[30].bottom(), down.list.bottom());
        // A row in the list stays.
        assert_eq!(down.reveal(29), down.scroll);
        // Rows scrolled out of the list are not hit.
        let p = Point::new(40.0, up.list.y - 2.0);
        assert_ne!(up.hit(p, &[]), Hit::Tab(2));
    }

    /// Collapsed, a row is its favicon in the middle of the panel; the one button there is the
    /// active tab's close button or an audible tab's speaker.
    #[test]
    fn a_collapsed_panel_centres_one_button_per_row() {
        let spec = SideSpec {
            width: COLLAPSED_WIDTH,
            collapsed: true,
            active: 1,
            ..spec(600.0, 0.0)
        };
        let s = SideLayout::new(70.0, &spec, 3);
        assert_eq!(s.tabs[0], Rect::new(12.0, 110.0, 32.0, 30.0));
        assert_eq!(s.collapse.x + s.collapse.width / 2.0, 28.0);
        let audio = [true, false, false];
        let mid = |i: usize| Point::new(28.0, s.tabs[i].y + 15.0);
        assert_eq!(s.hit(mid(0), &audio), Hit::TabAudio(0));
        assert_eq!(s.hit(mid(1), &audio), Hit::TabClose(1));
        assert_eq!(s.hit(mid(2), &audio), Hit::Tab(2));
        // Off the button, still the tab.
        assert_eq!(
            s.hit(Point::new(14.0, s.tabs[1].y + 2.0), &audio),
            Hit::Tab(1)
        );
        assert_eq!(s.hit(Point::new(55.0, 300.0), &audio), Hit::SideEdge);
        assert_eq!(s.hit(Point::new(52.0, 300.0), &audio), Hit::TabPanel);
    }

    /// A dragged row follows the pointer, inside the list, over the others; its place is the slot
    /// its middle is nearest.
    #[test]
    fn a_dragged_row_follows_the_pointer_and_finds_its_slot() {
        let still = SideLayout::new(70.0, &spec(600.0, 0.0), 4);
        let spec = SideSpec {
            drag: Some((1, 170.0)),
            ..spec(600.0, 0.0)
        };
        let s = SideLayout::new(70.0, &spec, 4);
        assert_eq!(s.dragging, Some(1));
        assert_eq!(s.tabs[1].y, 170.0);
        // The others keep their places.
        assert_eq!(s.tabs[0], still.tabs[0]);
        assert_eq!(s.tabs[2], still.tabs[2]);
        // Slots are 32 DIPs from the list's top (110): 170 is nearest the third slot (174).
        assert_eq!(s.drop_index(170.0, 4), 2);
        assert_eq!(s.drop_index(150.0, 4), 1);
        assert_eq!(s.drop_index(0.0, 4), 0);
        assert_eq!(s.drop_index(1000.0, 4), 3);
        // A row dragged past the list stays inside it.
        let far = SideLayout::new(
            70.0,
            &SideSpec {
                drag: Some((0, 2000.0)),
                ..spec
            },
            4,
        );
        assert_eq!(far.tabs[0].bottom(), far.list.bottom());
    }

    #[test]
    fn a_resize_clamps_snaps_and_collapses() {
        assert_eq!(resized(240.0, 60.0), Some(300.0));
        assert_eq!(resized(240.0, 500.0), Some(MAX_WIDTH));
        // Near the default width: the default width.
        assert_eq!(resized(300.0, -50.0), Some(DEFAULT_WIDTH));
        // Narrower than the least width with titles: held there until the snap point.
        assert_eq!(resized(240.0, -140.0), Some(MIN_WIDTH));
        assert_eq!(resized(240.0, -149.0), None);
        // From collapsed, dragged out past the snap point: titles again.
        assert_eq!(resized(COLLAPSED_WIDTH, 30.0), None);
        assert_eq!(resized(COLLAPSED_WIDTH, 40.0), Some(MIN_WIDTH));
    }
}
