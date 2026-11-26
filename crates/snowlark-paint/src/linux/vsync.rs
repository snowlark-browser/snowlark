// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The display's vsync on Linux. A Wayland client cannot wait for a vertical blank: the window's
//! compositor paces its own frames by the desktop's frame callbacks, and begin frames for the tabs
//! come from a timer at the refresh interval, ticking on a fixed timebase as Chromium's
//! delay-based begin frame source does where no vsync signal is available.

use std::time::{Duration, Instant};

/// The fallback interval (`BeginFrameArgs::DefaultInterval()`).
pub const DEFAULT_INTERVAL: Duration = Duration::from_nanos(16_666_667);

/// Ticks at the refresh interval on the timebase of its first wait.
#[derive(Default)]
pub struct VBlank {
    timebase: Option<Instant>,
}

impl VBlank {
    pub fn new() -> VBlank {
        VBlank::default()
    }

    /// Sleeps until the next tick. Returns the refresh interval, and whether it slept (always: no
    /// vertical blank was waited for).
    pub fn wait(&mut self) -> (Duration, bool) {
        let interval = refresh_interval();
        let now = Instant::now();
        let base = *self.timebase.get_or_insert(now);
        std::thread::sleep(until_next_tick(now - base, interval));
        (interval, true)
    }
}

/// How long from `elapsed` since the timebase to the next whole `interval` after it.
fn until_next_tick(elapsed: Duration, interval: Duration) -> Duration {
    let i = interval.as_nanos().max(1);
    let into = elapsed.as_nanos() % i;
    Duration::from_nanos((i - into) as u64)
}

/// The refresh interval: the default (the outputs' modes are not read yet).
pub fn refresh_interval() -> Duration {
    DEFAULT_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_stay_on_the_timebase() {
        let i = Duration::from_millis(16);
        assert_eq!(until_next_tick(Duration::ZERO, i), i);
        assert_eq!(
            until_next_tick(Duration::from_millis(5), i),
            Duration::from_millis(11)
        );
        assert_eq!(
            until_next_tick(Duration::from_millis(37), i),
            Duration::from_millis(11)
        );
    }
}
