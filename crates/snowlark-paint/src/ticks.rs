// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A moment in time that can cross a process boundary. [`std::time::Instant`] means nothing outside
//! its process and has no value to send, but on Windows it reads the performance counter, whose
//! ticks are valid system-wide: a time stamp in a message is a [`Ticks`].

use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// A reading of the system's performance counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Ticks(i64);

/// Counter ticks per second (fixed at boot).
fn frequency() -> i64 {
    static FREQUENCY: OnceLock<i64> = OnceLock::new();
    *FREQUENCY.get_or_init(|| {
        let mut f = 0;
        // SAFETY: the call writes one i64 to the valid reference it is given, and it cannot fail
        // from Windows XP on (it is checked anyway: a zero would divide below).
        let read = unsafe { QueryPerformanceFrequency(&mut f) };
        if read.is_ok() && f > 0 {
            f
        } else {
            1
        }
    })
}

impl Ticks {
    pub fn now() -> Ticks {
        let mut t = 0;
        // SAFETY: the call writes one i64 to the valid reference it is given.
        let _ = unsafe { QueryPerformanceCounter(&mut t) };
        Ticks(t)
    }

    /// The time from `earlier` to this reading (none when `earlier` is the later one).
    pub fn since(self, earlier: Ticks) -> Duration {
        let ticks = self.0.saturating_sub(earlier.0).max(0) as u128;
        let nanos = ticks * 1_000_000_000 / frequency() as u128;
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    /// The reading `time` before this one.
    pub fn before(self, time: Duration) -> Ticks {
        let ticks = time.as_nanos() * frequency() as u128 / 1_000_000_000;
        Ticks(
            self.0
                .saturating_sub(i64::try_from(ticks).unwrap_or(i64::MAX)),
        )
    }

    /// The time since this reading.
    pub fn elapsed(self) -> Duration {
        Ticks::now().since(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_measure_what_instant_measures() {
        use std::time::Instant;
        // (`Instant` reads the same counter: what it says around a span and within it bounds what
        // the ticks say of it, however late the thread runs)
        let outer = Instant::now();
        let t = Ticks::now();
        let inner = Instant::now();
        std::thread::sleep(Duration::from_millis(30));
        let within = inner.elapsed();
        let ticks = t.elapsed();
        let around = outer.elapsed();
        let rounding = Duration::from_micros(1);
        assert!(within <= ticks + rounding, "{within:?} and {ticks:?}");
        assert!(ticks <= around + rounding, "{ticks:?} and {around:?}");
    }

    #[test]
    fn an_earlier_reading_is_no_time_after_a_later_one() {
        let a = Ticks::now();
        let b = Ticks::now();
        assert!(b >= a);
        assert_eq!(a.since(b).max(b.since(a)), b.since(a));
        assert_eq!(Ticks(5).since(Ticks(9)), Duration::ZERO);
    }
}
