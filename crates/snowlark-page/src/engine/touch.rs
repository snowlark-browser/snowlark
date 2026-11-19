// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Touch Events: the `Touch`, `TouchList` and `TouchEvent` interfaces, made by script.
//!
//! The engine takes no touch input, so it never fires a touch event and reports no touch device
//! (`snowlark_style::InputDevices`): the legacy touch event APIs (`ontouch*`, `createEvent("TouchEvent")`)
//! are exposed only with one, while these interfaces always are.

use snowlark_bindings::generated::{
    TouchEventImpl, TouchEventInit, TouchImpl, TouchInit, TouchListImpl, TouchType,
};
use snowlark_bindings::rt::{self, Cx, Handle};
use snowlark_bindings::InterfaceId;

use super::events::{EventExt, UiFields};
use super::{Engine, Object};

/// A `Touch`: the touch point's values as its `TouchInit` gave them.
#[derive(Clone, Debug)]
pub struct TouchData {
    pub identifier: i32,
    pub target: Handle,
    pub screen: (f64, f64),
    pub client: (f64, f64),
    pub page: (f64, f64),
    pub radius: (f32, f32),
    pub rotation_angle: f32,
    pub force: f32,
    pub altitude_angle: f32,
    pub azimuth_angle: f32,
    pub stylus: bool,
}

/// A TouchEvent's `touches`, `targetTouches` and `changedTouches`, in that order.
const TOUCHES: usize = 0;
const TARGET_TOUCHES: usize = 1;
const CHANGED_TOUCHES: usize = 2;

impl Engine {
    fn touch<R>(&self, h: Handle, f: impl FnOnce(&TouchData) -> R) -> R {
        match &*self.obj(h) {
            Object::Touch(t) => f(t),
            _ => panic!("not a Touch"),
        }
    }

    /// One of a TouchEvent's three lists (`which`: [`TOUCHES`], ...). An event made by
    /// `createEvent("TouchEvent")` gets three empty ones at the first read.
    fn touch_list(&self, event: Handle, which: usize) -> Handle {
        if let Some(lists) = self.ui(event, |u| u.touch_lists) {
            return lists[which];
        }
        let lists = [(); 3].map(|_| self.alloc(Object::TouchList(Vec::new())));
        self.ev_mut(event, |e| {
            if let EventExt::Ui(u) = &mut e.ext {
                u.touch_lists = Some(lists);
            }
        });
        lists[which]
    }

    /// Whether the legacy touch event APIs are exposed (Touch Events §"Conditionally exposing legacy
    /// touch event APIs"): only with a touch device, as `navigator.maxTouchPoints` and the `pointer`
    /// media feature report it.
    pub(crate) fn legacy_touch_apis(&self) -> bool {
        self.hooks.borrow().input.touch()
    }
}

impl TouchImpl for Engine {
    fn identifier(&self, this: Handle) -> i32 {
        self.touch(this, |t| t.identifier)
    }
    fn target(&self, this: Handle) -> Handle {
        self.touch(this, |t| t.target)
    }
    fn screen_x(&self, this: Handle) -> f64 {
        self.touch(this, |t| t.screen.0)
    }
    fn screen_y(&self, this: Handle) -> f64 {
        self.touch(this, |t| t.screen.1)
    }
    fn client_x(&self, this: Handle) -> f64 {
        self.touch(this, |t| t.client.0)
    }
    fn client_y(&self, this: Handle) -> f64 {
        self.touch(this, |t| t.client.1)
    }
    fn page_x(&self, this: Handle) -> f64 {
        self.touch(this, |t| t.page.0)
    }
    fn page_y(&self, this: Handle) -> f64 {
        self.touch(this, |t| t.page.1)
    }
    fn radius_x(&self, this: Handle) -> f32 {
        self.touch(this, |t| t.radius.0)
    }
    fn radius_y(&self, this: Handle) -> f32 {
        self.touch(this, |t| t.radius.1)
    }
    fn rotation_angle(&self, this: Handle) -> f32 {
        self.touch(this, |t| t.rotation_angle)
    }
    fn force(&self, this: Handle) -> f32 {
        self.touch(this, |t| t.force)
    }
    fn altitude_angle(&self, this: Handle) -> f32 {
        self.touch(this, |t| t.altitude_angle)
    }
    fn azimuth_angle(&self, this: Handle) -> f32 {
        self.touch(this, |t| t.azimuth_angle)
    }
    fn touch_type(&self, this: Handle) -> TouchType {
        if self.touch(this, |t| t.stylus) {
            TouchType::Stylus
        } else {
            TouchType::Direct
        }
    }
    fn constructor(&self, _cx: &mut Cx, init: TouchInit) -> rt::Result<Handle> {
        Ok(self.alloc(Object::Touch(Box::new(TouchData {
            identifier: init.identifier,
            target: init.target,
            screen: (init.screen_x, init.screen_y),
            client: (init.client_x, init.client_y),
            page: (init.page_x, init.page_y),
            radius: (init.radius_x, init.radius_y),
            rotation_angle: init.rotation_angle,
            force: init.force,
            altitude_angle: init.altitude_angle as f32,
            azimuth_angle: init.azimuth_angle as f32,
            stylus: matches!(init.touch_type, TouchType::Stylus),
        }))))
    }
}

impl TouchListImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        match &*self.obj(this) {
            Object::TouchList(l) => l.len() as u32,
            _ => 0,
        }
    }
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        match &*self.obj(this) {
            Object::TouchList(l) => l.get(index as usize).copied(),
            _ => None,
        }
    }
}

impl TouchEventImpl for Engine {
    fn touches(&self, this: Handle) -> Handle {
        self.touch_list(this, TOUCHES)
    }
    fn target_touches(&self, this: Handle) -> Handle {
        self.touch_list(this, TARGET_TOUCHES)
    }
    fn changed_touches(&self, this: Handle) -> Handle {
        self.touch_list(this, CHANGED_TOUCHES)
    }
    fn alt_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.alt)
    }
    fn meta_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.meta)
    }
    fn ctrl_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.ctrl)
    }
    fn shift_key(&self, this: Handle) -> bool {
        self.ui(this, |u| u.shift)
    }
    fn get_modifier_state(&self, _cx: &mut Cx, this: Handle, key_arg: &str) -> rt::Result<bool> {
        Ok(self.modifier_state(this, key_arg))
    }
    fn constructor(&self, _cx: &mut Cx, type_: &str, init: TouchEventInit) -> rt::Result<Handle> {
        let mut u = UiFields {
            view: init.view,
            detail: init.detail,
            which: init.which,
            ..Default::default()
        };
        super::events::modifiers_init!(u, init);
        let list = |touches: Vec<Handle>| self.alloc(Object::TouchList(touches));
        u.touch_lists = Some([
            list(init.touches),
            list(init.target_touches),
            list(init.changed_touches),
        ]);
        Ok(self.ui_event(
            InterfaceId::TouchEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            u,
        ))
    }
}
