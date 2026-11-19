// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSSOM View §4.3 `Screen`, Screen Orientation's `ScreenOrientation` and CSSOM View §10
//! `VisualViewport`, each one object per Window (`[SameObject]`).
//!
//! The Web-exposed screen area is the window's viewport (CSSOM View lets the user agent choose
//! what it exposes; reporting the monitor would add a fingerprinting surface for no layout value),
//! so `availWidth`/`width` and `availHeight`/`height` are the viewport's size. There is no pinch
//! zoom: the visual viewport is the layout viewport without its scroll bar, at scale 1, offset 0
//! from it, and its page offset is the viewport's scroll position.

use snowlark_bindings::generated::{
    OrientationLockType, OrientationType, ScreenImpl, ScreenOrientationImpl, VisualViewportImpl,
};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsPromise};

use super::element::rejected;
use super::{Engine, Object};

impl Engine {
    /// The Window an object of this module belongs to.
    fn screen_window(&self, this: Handle) -> Handle {
        match &*self.obj(this) {
            Object::Screen { window }
            | Object::ScreenOrientation { window }
            | Object::VisualViewport { window } => *window,
            _ => panic!("not a Screen, ScreenOrientation or VisualViewport"),
        }
    }

    pub(crate) fn screen_handle(&self, window: Handle) -> Handle {
        self.window_singleton(window, "screen", || Object::Screen { window })
    }

    /// CSSOM View `visualViewport`: the window's associated document's VisualViewport while that
    /// document is fully active.
    pub(crate) fn visual_viewport_handle(&self, window: Handle) -> Option<Handle> {
        self.document_of_window(window)?;
        Some(
            self.window_singleton(window, "visualViewport", || Object::VisualViewport {
                window,
            }),
        )
    }

    /// The window's VisualViewport if script made it (events need no object nobody can listen on).
    pub(crate) fn visual_viewport_existing(&self, window: Handle) -> Option<Handle> {
        self.frames
            .borrow()
            .singletons
            .get(&(window, "visualViewport"))
            .copied()
    }

    /// The Web-exposed screen area: the top-level window's viewport (CSS px), for a frame's
    /// window too (the screen is the output device's, as `device-width` in media queries).
    fn screen_area(&self, _this: Handle) -> (i32, i32) {
        self.hooks.borrow().viewport
    }

    /// The visual viewport: (width, height) without a rendered scroll bar, and the scroll position;
    /// `None` when the window's document is not rendered.
    fn visual_viewport(&self, this: Handle) -> Option<(f64, f64, f64, f64)> {
        let doc = self.document_of_window(self.screen_window(this))?;
        self.sync_frame_viewport(doc);
        self.with_doc_renderer(doc, |r, st| {
            let v = r.layout_viewport(st.viewport);
            (
                v.width as f64,
                v.height as f64,
                st.scroll.x as f64,
                st.scroll.y as f64,
            )
        })
    }
}

impl ScreenImpl for Engine {
    fn avail_width(&self, this: Handle) -> i32 {
        self.screen_area(this).0
    }
    fn avail_height(&self, this: Handle) -> i32 {
        self.screen_area(this).1
    }
    fn width(&self, this: Handle) -> i32 {
        self.screen_area(this).0
    }
    fn height(&self, this: Handle) -> i32 {
        self.screen_area(this).1
    }
    /// CSSOM View: 24 (8 bits per color component, no alpha), what every engine reports.
    fn color_depth(&self, _this: Handle) -> u32 {
        24
    }
    fn pixel_depth(&self, _this: Handle) -> u32 {
        24
    }
    fn orientation(&self, this: Handle) -> Handle {
        let window = self.screen_window(this);
        self.window_singleton(window, "screen.orientation", || Object::ScreenOrientation {
            window,
        })
    }
    /// Window Management: one screen.
    fn is_extended(&self, _this: Handle) -> bool {
        false
    }
}

impl ScreenOrientationImpl for Engine {
    /// Screen Orientation §5: the screen's orientation, landscape when it is at least as wide as
    /// tall (the natural orientation of a desktop screen: primary, angle 0).
    fn type_(&self, this: Handle) -> OrientationType {
        let (w, h) = self.screen_area(this);
        if w >= h {
            OrientationType::LandscapePrimary
        } else {
            OrientationType::PortraitPrimary
        }
    }
    fn angle(&self, _this: Handle) -> u16 {
        0
    }
    /// Screen Orientation §6.1 `lock()`: a desktop window can't lock the orientation: the promise
    /// is rejected with "NotSupportedError".
    fn lock(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _orientation: OrientationLockType,
    ) -> rt::Result<JsPromise> {
        rejected(
            cx,
            ExceptionName::NotSupportedError,
            "Screen orientation can't be locked here",
        )
    }
    fn unlock(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
}

impl VisualViewportImpl for Engine {
    fn offset_left(&self, _this: Handle) -> f64 {
        0.0
    }
    fn offset_top(&self, _this: Handle) -> f64 {
        0.0
    }
    fn page_left(&self, this: Handle) -> f64 {
        self.visual_viewport(this).map_or(0.0, |v| v.2)
    }
    fn page_top(&self, this: Handle) -> f64 {
        self.visual_viewport(this).map_or(0.0, |v| v.3)
    }
    fn width(&self, this: Handle) -> f64 {
        self.visual_viewport(this).map_or(0.0, |v| v.0)
    }
    fn height(&self, this: Handle) -> f64 {
        self.visual_viewport(this).map_or(0.0, |v| v.1)
    }
    fn scale(&self, this: Handle) -> f64 {
        if self.visual_viewport(this).is_some() {
            1.0
        } else {
            0.0
        }
    }
}
