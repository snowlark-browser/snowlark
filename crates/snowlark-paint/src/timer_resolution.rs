// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Precise timed waits for a thread that paces frames by waiting.
//!
//! Windows ends a timed wait (`WaitForSingleObject`, `WaitOnAddress`, and so a channel's
//! `recv_timeout`) on the system timer's tick, 15.625 ms unless the process asks for a finer one:
//! a 16.7 ms wait for the next frame takes two ticks, 31 ms, and frames come at 32 a second.
//! Chromium raises the resolution while a thread's next wake-up needs it
//! (`ThreadControllerWithMessagePumpImpl::DoIdleWork` calls `Time::ActivateHighResolutionTimer`
//! when `SequenceManagerImpl::NextWakeUpNeedsHighRes`), to 1 ms on AC power and 8 ms on battery
//! (`base/time/time_win.cc`, `kMinTimerIntervalHighResMs` and `kMinTimerIntervalLowResMs`, chosen
//! by `HighResolutionTimerManager` from the power state). Windows keeps a count of the requests of
//! each process (`timeBeginPeriod` and a matching `timeEndPeriod`), so each thread holds its own
//! and nothing is shared. Since Windows 10 2004 a process's request affects that process only.

use std::time::Duration;

use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod, TIMERR_NOERROR};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

/// The system timer's default tick (`base::Time::kMinLowResolutionThresholdMs` rounds it to 16).
pub const DEFAULT_TICK: Duration = Duration::from_micros(15_625);

/// The resolution on AC power (`kMinTimerIntervalHighResMs`).
const HIGH_RES_MS: u32 = 1;
/// The resolution on battery (`kMinTimerIntervalLowResMs`).
const LOW_RES_MS: u32 = 8;

/// Whether a wait of `wait` needs a finer timer: one shorter than two default ticks can last up to
/// twice as long on the default tick (a frame interval is one).
pub fn wait_needs_high_res(wait: Duration) -> bool {
    wait < DEFAULT_TICK * 2
}

/// One thread's request for a finer timer resolution: [`HighResWaits::set`] it before each wait;
/// dropping it ends the request.
#[derive(Debug, Default)]
pub struct HighResWaits {
    /// The period requested (ms), 0 when none is.
    period: u32,
}

impl HighResWaits {
    pub const fn new() -> HighResWaits {
        HighResWaits { period: 0 }
    }

    /// Whether the request is in force.
    pub fn active(&self) -> bool {
        self.period != 0
    }

    /// Makes the request (`needed`) or ends it; nothing when it is already so.
    pub fn set(&mut self, needed: bool) {
        if needed == self.active() {
            return;
        }
        if needed {
            let period = if on_battery() {
                LOW_RES_MS
            } else {
                HIGH_RES_MS
            };
            // SAFETY: `timeBeginPeriod` takes a value and has no other effect than the process's
            // timer resolution; each successful call is matched by `timeEndPeriod` of the same
            // value below (or in `drop`).
            if unsafe { timeBeginPeriod(period) } == TIMERR_NOERROR {
                self.period = period;
            }
        } else {
            // SAFETY: ends the request this value made above (the one `timeBeginPeriod` accepted).
            unsafe { timeEndPeriod(self.period) };
            self.period = 0;
        }
    }
}

impl Drop for HighResWaits {
    fn drop(&mut self) {
        self.set(false);
    }
}

/// Whether the machine runs on battery (unknown counts as not, as Chromium's power monitor
/// reports a desktop).
fn on_battery() -> bool {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: the call writes one `SYSTEM_POWER_STATUS` to the valid reference it is given.
    let read = unsafe { GetSystemPowerStatus(&mut status) };
    read.is_ok() && status.ACLineStatus == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    /// The median time a channel's `recv_timeout` of 17 ms takes, over 15 waits.
    fn median_wait() -> Duration {
        let (_tx, rx) = mpsc::channel::<()>();
        let mut waits: Vec<Duration> = (0..15)
            .map(|_| {
                let t = Instant::now();
                let _ = rx.recv_timeout(Duration::from_millis(17));
                t.elapsed()
            })
            .collect();
        waits.sort();
        waits[waits.len() / 2]
    }

    #[test]
    fn a_frame_wait_ends_near_its_time_while_the_request_holds() {
        assert!(wait_needs_high_res(Duration::from_micros(16_667)));
        assert!(!wait_needs_high_res(Duration::from_millis(50)));
        let mut high = HighResWaits::new();
        high.set(true);
        assert!(high.active());
        let wait = median_wait();
        // (on the default tick it takes two, about 31 ms; at 1 ms (AC) about 17.5, at 8 (battery) 24)
        let limit = if on_battery() { 26 } else { 22 };
        assert!(
            wait < Duration::from_millis(limit),
            "a 17 ms wait took {wait:?}"
        );
        high.set(false);
        assert!(!high.active());
    }
}
