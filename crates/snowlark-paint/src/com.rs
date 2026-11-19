// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! COM initialization for the threads that use WIC.

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

struct Guard {
    initialized: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances the successful CoInitializeEx made on this same thread.
            unsafe { CoUninitialize() };
        }
    }
}

thread_local! {
    static COM: Guard = {
        // SAFETY: CoInitializeEx with no reserved pointer. S_FALSE (already initialized) still needs
        // the balancing uninitialize; RPC_E_CHANGED_MODE (the thread is an STA) does not, and WIC
        // works there too.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Guard { initialized: hr.is_ok() }
    };
}

/// Makes sure COM is initialized on this thread (once; undone when the thread exits).
pub(crate) fn ensure() {
    COM.with(|_| {});
}
