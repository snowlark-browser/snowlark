// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pointer Events 3 "converting between tiltX/tiltY and altitudeAngle/azimuthAngle": a constructed
//! PointerEvent given only the tilts gets the angles from them, and the other way round.

use std::f64::consts::{FRAC_PI_2, PI, TAU};

/// A tilt in degrees brought into -90..=90 by half turns.
pub(crate) fn tilt_in_range(mut t: i32) -> i32 {
    while t > 90 {
        t -= 180;
    }
    while t < -90 {
        t += 180;
    }
    t
}

/// An azimuth in radians brought into 0..=2π.
pub(crate) fn azimuth_in_range(mut a: f64) -> f64 {
    if !a.is_finite() {
        return 0.0;
    }
    while a > TAU {
        a -= TAU;
    }
    while a < 0.0 {
        a += TAU;
    }
    a
}

/// An altitude in radians brought into 0..=π/2.
pub(crate) fn altitude_in_range(mut a: f64) -> f64 {
    if !a.is_finite() {
        return FRAC_PI_2;
    }
    while a > FRAC_PI_2 {
        a -= FRAC_PI_2;
    }
    while a < 0.0 {
        a += FRAC_PI_2;
    }
    a
}

pub(crate) fn azimuth_from_tilt(tx: f64, ty: f64) -> f64 {
    if tx == 0.0 {
        return if ty > 0.0 {
            FRAC_PI_2
        } else if ty < 0.0 {
            3.0 * FRAC_PI_2
        } else {
            0.0
        };
    }
    if ty == 0.0 {
        return if tx < 0.0 { PI } else { 0.0 };
    }
    if tx.abs() == 90.0 || ty.abs() == 90.0 {
        return 0.0;
    }
    let a = ty.to_radians().tan().atan2(tx.to_radians().tan());
    if a >= 0.0 {
        a
    } else {
        a + TAU
    }
}

pub(crate) fn altitude_from_tilt(tx: f64, ty: f64) -> f64 {
    if tx.abs() == 90.0 || ty.abs() == 90.0 {
        return 0.0;
    }
    if tx == 0.0 {
        return FRAC_PI_2 - ty.to_radians().abs();
    }
    if ty == 0.0 {
        return FRAC_PI_2 - tx.to_radians().abs();
    }
    (1.0 / (tx.to_radians().tan().powi(2) + ty.to_radians().tan().powi(2)).sqrt()).atan()
}

pub(crate) fn tilt_x_from_spherical(azimuth: f64, altitude: f64) -> i32 {
    if altitude != 0.0 {
        return ((azimuth.cos() / altitude.tan()).atan().to_degrees() + 0.5).floor() as i32;
    }
    if azimuth == FRAC_PI_2 || azimuth == 3.0 * FRAC_PI_2 {
        0
    } else if !(FRAC_PI_2..=3.0 * FRAC_PI_2).contains(&azimuth) {
        90
    } else {
        -90
    }
}

pub(crate) fn tilt_y_from_spherical(azimuth: f64, altitude: f64) -> i32 {
    if altitude != 0.0 {
        return ((azimuth.sin() / altitude.tan()).atan().to_degrees() + 0.5).floor() as i32;
    }
    if azimuth == 0.0 || azimuth == PI || azimuth == TAU {
        0
    } else if azimuth < PI {
        90
    } else {
        -90
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilts_and_angles_convert_both_ways() {
        assert_eq!(azimuth_from_tilt(0.0, 90.0), FRAC_PI_2);
        assert_eq!(azimuth_from_tilt(0.0, -90.0), 3.0 * FRAC_PI_2);
        assert_eq!(azimuth_from_tilt(-45.0, 0.0), PI);
        assert_eq!(altitude_from_tilt(90.0, 0.0), 0.0);
        assert_eq!(altitude_from_tilt(0.0, 0.0), FRAC_PI_2);
        assert_eq!(tilt_x_from_spherical(0.0, 0.0), 90);
        assert_eq!(tilt_y_from_spherical(FRAC_PI_2, 0.0), 90);
        assert_eq!(tilt_x_from_spherical(PI, FRAC_PI_2 / 2.0), -45);
        assert_eq!(tilt_in_range(100), -80);
        assert_eq!(azimuth_in_range(-FRAC_PI_2), 3.0 * FRAC_PI_2);
    }
}
