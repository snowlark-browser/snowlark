// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Gamepad: `navigator.getGamepads()`, and the Gamepad, GamepadButton, GamepadHapticActuator and
//! GamepadEvent interfaces.
//!
//! The engine has no gamepad source, so no gamepad ever connects: no Gamepad, GamepadButton or
//! GamepadHapticActuator object is ever made (their members below are never reached), no
//! `gamepadconnected` event is fired, and `getGamepads()` answers an empty list.

use snowlark_bindings::generated::{
    GamepadButtonImpl, GamepadEffectParameters, GamepadEventImpl, GamepadEventInit,
    GamepadHapticActuatorImpl, GamepadHapticEffectType, GamepadImpl, GamepadMappingType,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsPromise};
use snowlark_bindings::InterfaceId;

use super::events::EventExt;
use super::policy::Feature;
use super::{exception, Engine};

impl Engine {
    /// Gamepad §"getGamepads() method".
    pub(crate) fn get_gamepads(&self, navigator: Handle) -> rt::Result<Vec<Option<Handle>>> {
        let window = self.navigator_window(navigator);
        // 1-2. A navigator without a fully active document has no gamepads.
        let Some(doc) = self
            .document_of_window(window)
            .filter(|&d| self.is_fully_active(d))
        else {
            return Ok(Vec::new());
        };
        // 3.
        if !self.allowed_to_use(doc, Feature::Gamepad) {
            return Err(exception(
                ExceptionName::SecurityError,
                "The \"gamepad\" feature is not allowed in this document",
            ));
        }
        // 4. No gamepad has ever connected, so the navigator has no gamepad user gesture: an
        // empty list.
        Ok(Vec::new())
    }
}

impl GamepadImpl for Engine {
    fn id(&self, _this: Handle) -> DomStr<'_> {
        DomStr::from("")
    }
    fn index(&self, _this: Handle) -> i32 {
        0
    }
    fn connected(&self, _this: Handle) -> bool {
        false
    }
    fn timestamp(&self, _this: Handle) -> f64 {
        0.0
    }
    fn mapping(&self, _this: Handle) -> GamepadMappingType {
        GamepadMappingType::Empty
    }
    fn axes(&self, _this: Handle) -> Vec<f64> {
        Vec::new()
    }
    fn buttons(&self, _this: Handle) -> Vec<Handle> {
        Vec::new()
    }
    fn vibration_actuator(&self, this: Handle) -> Handle {
        this
    }
}

impl GamepadButtonImpl for Engine {
    fn pressed(&self, _this: Handle) -> bool {
        false
    }
    fn touched(&self, _this: Handle) -> bool {
        false
    }
    fn value(&self, _this: Handle) -> f64 {
        0.0
    }
}

impl GamepadHapticActuatorImpl for Engine {
    fn effects(&self, _this: Handle) -> Vec<GamepadHapticEffectType> {
        Vec::new()
    }
    fn play_effect(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _type_: GamepadHapticEffectType,
        _params: GamepadEffectParameters,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        p.reject(
            cx,
            exception(ExceptionName::InvalidStateError, "No gamepad is connected"),
        );
        Ok(p)
    }
    fn reset(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        p.reject(
            cx,
            exception(ExceptionName::InvalidStateError, "No gamepad is connected"),
        );
        Ok(p)
    }
}

impl GamepadEventImpl for Engine {
    /// No Gamepad object exists, so an event can only have been made without one.
    fn gamepad(&self, _this: Handle) -> Option<Handle> {
        None
    }
    fn constructor(&self, _cx: &mut Cx, type_: &str, init: GamepadEventInit) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::GamepadEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::None,
        ))
    }
}
