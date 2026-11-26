// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Precise timed waits on Linux: the kernel's high-resolution timers end a timed wait when it is
//! due, with no system tick to raise, so a thread's request does nothing (the Windows half raises
//! the timer resolution while a wait needs it).

use std::time::Duration;

/// The tick a wait could end on: none on Linux (the Windows half's 15.625 ms is kept as the
/// threshold below which a wait counts as one that needs precision).
pub const DEFAULT_TICK: Duration = Duration::from_micros(15_625);

/// Whether a wait of `wait` would need a finer timer elsewhere (shorter than two ticks).
pub fn wait_needs_high_res(wait: Duration) -> bool {
    wait < DEFAULT_TICK * 2
}

/// One thread's request for precise waits: always granted, so it only records whether it is made.
#[derive(Debug, Default)]
pub struct HighResWaits {
    active: bool,
}

impl HighResWaits {
    pub const fn new() -> HighResWaits {
        HighResWaits { active: false }
    }

    /// Whether the request is in force.
    pub fn active(&self) -> bool {
        self.active
    }

    /// Makes the request (`needed`) or ends it.
    pub fn set(&mut self, needed: bool) {
        self.active = needed;
    }
}
