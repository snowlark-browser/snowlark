// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Dragging the page's address out of the window. The site button at the start of the address
//! field, or the address itself while all of it is selected and unedited, drags the page's URL as
//! a link: dropped on the desktop or in a folder it makes an internet shortcut named after the
//! page, on another program it gives the URL, on a tab strip or a page it opens there. Selected
//! text of an edited address drags as text. A press that does not move as far as a drag does what
//! a click there does.

use snowlark_paint::drop_target::{DragImage, DragLink, DragOut};
use snowlark_paint::window::Window;
use snowlark_paint::{
    Color, DisplayListBuilder, HeadlessRenderer, ImageId, ImageRendering, Point, Radii, Rect,
    Sources, Viewport,
};

use super::Browser;
use crate::address::display_url;
use crate::chrome::Hit;

/// How far the pointer moves with the button held before a press becomes a drag (Windows'
/// default drag rectangle, `SM_CXDRAG`).
const DRAG_THRESHOLD: f32 = 4.0;

/// The drag picture's widest (DIPs), as the link drags of other browsers.
const IMAGE_MAX_WIDTH: f32 = 150.0;

/// Where a press that may become a drag out of the window was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum DragOrigin {
    /// The site button: a click opens the site information.
    SiteButton,
    /// The address, all of it selected or a selection in it; `caret`: where a click puts the
    /// caret (none: the press focused the field, which a click leaves as it is).
    Address { caret: Option<usize> },
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DragOutPress {
    at: Point,
    origin: DragOrigin,
}

impl Browser {
    /// A press at `at` that becomes a drag if the pointer moves far enough before its release.
    pub(super) fn drag_out_press(&mut self, window: &Window, at: Point, origin: DragOrigin) {
        self.drag_out = Some(DragOutPress { at, origin });
        window.capture_mouse();
    }

    /// The pointer moved with the button held: past the threshold the drag starts. Whether a
    /// press of this kind is under way (the move is then this module's).
    pub(super) fn drag_out_move(&mut self, window: &Window, at: Point) -> bool {
        let Some(p) = self.drag_out else {
            return false;
        };
        if (at.x - p.at.x).abs() <= DRAG_THRESHOLD && (at.y - p.at.y).abs() <= DRAG_THRESHOLD {
            return true;
        }
        self.drag_out = None;
        if let Some(out) = self.drag_out_data(p.origin) {
            window.start_drag(out);
        }
        true
    }

    /// The button came up before a drag started: what a click there does. Whether such a press
    /// was under way.
    pub(super) fn drag_out_release(&mut self) -> bool {
        let Some(p) = self.drag_out.take() else {
            return false;
        };
        self.pressed = Hit::None;
        self.chrome_dirty = true;
        match p.origin {
            DragOrigin::SiteButton => self.open_site_info(),
            DragOrigin::Address { caret: Some(i) } => {
                self.address.press(i, 1, false);
                self.caret_on = true;
            }
            DragOrigin::Address { caret: None } => {}
        }
        true
    }

    /// The drag ended (dropped or canceled): the press is over.
    pub(super) fn drag_out_ended(&mut self) {
        self.drag_out = None;
        self.pressed = Hit::None;
        self.chrome_dirty = true;
    }

    /// What a drag from `origin` carries: the page's link, or the selected text.
    fn drag_out_data(&self, origin: DragOrigin) -> Option<DragOut> {
        let e = self.tabs.active()?;
        let url = self.active_url();
        let page = || DragLink {
            url: url.clone(),
            title: e.tab.state.title.clone(),
        };
        let link = match origin {
            DragOrigin::SiteButton => Some(page()),
            DragOrigin::Address { .. } => {
                let text = self.address.text();
                let whole = self.address.selection() == (0..text.len());
                (whole && text == display_url(&url)).then(page)
            }
        };
        let text = match &link {
            Some(l) => l.url.clone(),
            None => self.address.selected_text().to_owned(),
        };
        if text.is_empty() || url.is_empty() && link.is_some() {
            return None;
        }
        let image = link.as_ref().and_then(|l| self.drag_image(l, e.favicon));
        Some(DragOut { text, link, image })
    }

    /// The picture that follows the pointer: the page's icon and title on the address field's
    /// color, as wide as the title needs up to [`IMAGE_MAX_WIDTH`].
    fn drag_image(&self, link: &DragLink, icon: Option<ImageId>) -> Option<DragImage> {
        let t = self.chrome_theme();
        let scale = self.scale();
        let title = if link.title.is_empty() {
            link.url.as_str()
        } else {
            link.title.as_str()
        };
        const PAD: f32 = 8.0;
        const ICON: f32 = 16.0;
        const SIZE: f32 = 12.0;
        let height = 28.0;
        let room = IMAGE_MAX_WIDTH - PAD * 3.0 - ICON;
        let text_w = self.fonts.width(title, SIZE).min(room);
        let width = (PAD * 3.0 + ICON + text_w).ceil();
        let mut b = DisplayListBuilder::new();
        let r = Rect::new(0.0, 0.0, width, height);
        b.fill_rounded_rect(r, Radii::uniform(6.0), t.field_focused);
        let cy = height / 2.0;
        let icon_rect = Rect::new(PAD, cy - ICON / 2.0, ICON, ICON);
        match icon {
            Some(id) => b.image(id, icon_rect, None, 1.0, ImageRendering::Smooth),
            None => self.fonts.icon(
                &mut b,
                crate::chrome::glyph::GLOBE,
                Point::new(PAD + ICON / 2.0, cy),
                14.0,
                t.text_dim,
            ),
        }
        self.fonts
            .label(&mut b, title, PAD * 2.0 + ICON, cy, SIZE, t.text, room);
        let list = b.build();
        let view = Viewport {
            width: (width * scale).ceil() as u32,
            height: (height * scale).ceil() as u32,
            scale,
            background: Color::TRANSPARENT,
        };
        let sources = Sources {
            text: &**self.fonts.engine(),
            images: &*self.images,
        };
        let pixels = HeadlessRenderer::new()
            .ok()?
            .render(&list, sources, &view)
            .ok()?;
        Some(DragImage {
            width: pixels.width,
            height: pixels.height,
            bgra: pixels.data,
            // (the pointer at the picture's middle)
            hotspot: ((pixels.width / 2) as i32, (pixels.height / 2) as i32),
        })
    }
}
