// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --type=gpu`: the GPU process (`docs/process-isolation.md`, M7). The browser starts it with
//! `--gpu-process`; it takes the pipe the browser gave it, says hello and runs until the browser
//! closes it or is gone, or its device is removed (`snowlark_shell::gpu_process`).

use std::time::Duration;

use snowlark_shell::gpu_process::GpuChildPort;

/// How long the GPU process waits for the browser to answer its hello.
const CONNECT_WAIT: Duration = Duration::from_secs(20);

pub fn run() -> i32 {
    // (a hard crash is written as a minidump by the browser, as a renderer's)
    snowlark_ipc::install_crash_handler();
    let port: GpuChildPort = match snowlark_ipc::connect_to_browser(CONNECT_WAIT) {
        Ok(port) => port,
        Err(e) => {
            eprintln!("[gpu] no browser to connect to: {e}");
            return 1;
        }
    };
    snowlark_shell::gpu_process::run_child(port)
}
