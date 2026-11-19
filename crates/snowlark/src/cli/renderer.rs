// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --type=renderer`: a renderer process (`docs/process-isolation.md`, M4). The browser starts
//! it with `--process-model=tab`, one per tab; it takes the pipe the browser gave it, says hello and
//! runs the tab the browser names until the browser closes it or is gone.

use std::time::Duration;

use snowlark_tab::services::RendererPort;

/// How long a renderer waits for the browser to answer its hello.
const CONNECT_WAIT: Duration = Duration::from_secs(20);

pub fn run() -> i32 {
    let port: RendererPort = match snowlark_ipc::connect_to_browser(CONNECT_WAIT) {
        Ok(port) => port,
        Err(e) => {
            eprintln!("[renderer] no browser to connect to: {e}");
            return 1;
        }
    };
    snowlark_tab::renderer_main::run_renderer(port)
}
