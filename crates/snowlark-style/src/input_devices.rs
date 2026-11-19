// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pointing devices the user agent has. One description answers every place a page sees them, so
//! they always agree: the `pointer`, `any-pointer`, `hover` and `any-hover` media features (Media
//! Queries 4 §6.4, §6.5), `navigator.maxTouchPoints` (Pointer Events §11) and whether the legacy touch
//! event APIs are exposed (Touch Events §"Conditionally exposing legacy touch event APIs").

use style::servo::media_features::PointerCapabilities;

/// The user agent's pointing devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputDevices {
    /// A mouse (or another accurate pointer that can hover).
    pub mouse: bool,
    /// The most simultaneous touch contacts a touch screen takes; 0: no touch screen.
    pub max_touch_points: u32,
}

impl InputDevices {
    /// A mouse and no touch screen. The engine takes no touch input (no touch event is ever fired
    /// from the window), so this is what every mode reports: a page that saw a touch device would wait
    /// for touches that never come.
    pub const MOUSE: InputDevices = InputDevices {
        mouse: true,
        max_touch_points: 0,
    };

    /// Whether there is a touch screen: the legacy touch event APIs (`ontouchstart` and the other
    /// `ontouch*` handlers) are exposed only then.
    pub fn touch(&self) -> bool {
        self.max_touch_points > 0
    }

    /// The primary pointer (`pointer`, `hover`): the mouse when there is one, else the touch screen.
    pub fn primary_pointer(&self) -> PointerCapabilities {
        if self.mouse {
            Self::mouse_capabilities()
        } else if self.touch() {
            PointerCapabilities::COARSE
        } else {
            PointerCapabilities::empty()
        }
    }

    /// Every pointer (`any-pointer`, `any-hover`).
    pub fn all_pointers(&self) -> PointerCapabilities {
        let mut caps = PointerCapabilities::empty();
        if self.mouse {
            caps |= Self::mouse_capabilities();
        }
        if self.touch() {
            caps |= PointerCapabilities::COARSE;
        }
        caps
    }

    fn mouse_capabilities() -> PointerCapabilities {
        PointerCapabilities::FINE | PointerCapabilities::HOVER
    }
}

impl Default for InputDevices {
    fn default() -> Self {
        InputDevices::MOUSE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mouse_alone_is_fine_and_hovers_without_touch() {
        let d = InputDevices::MOUSE;
        assert!(!d.touch());
        assert_eq!(
            d.primary_pointer().bits(),
            (PointerCapabilities::FINE | PointerCapabilities::HOVER).bits()
        );
        assert_eq!(d.all_pointers().bits(), d.primary_pointer().bits());
    }

    #[test]
    fn a_touch_screen_alone_is_coarse_and_cannot_hover() {
        let d = InputDevices {
            mouse: false,
            max_touch_points: 10,
        };
        assert!(d.touch());
        assert_eq!(
            d.primary_pointer().bits(),
            PointerCapabilities::COARSE.bits()
        );
        assert_eq!(d.all_pointers().bits(), PointerCapabilities::COARSE.bits());
    }

    #[test]
    fn a_mouse_stays_primary_beside_a_touch_screen() {
        let d = InputDevices {
            mouse: true,
            max_touch_points: 5,
        };
        assert_eq!(
            d.primary_pointer().bits(),
            (PointerCapabilities::FINE | PointerCapabilities::HOVER).bits()
        );
        assert_eq!(
            d.all_pointers().bits(),
            (PointerCapabilities::FINE | PointerCapabilities::HOVER | PointerCapabilities::COARSE)
                .bits()
        );
    }
}
