// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The network layer's script parser for opaque-response blocking ([`snowlark_net::orb`]), backed
//! by V8 ([`snowlark_js::script_check`]): the process that owns the network thread (the browser, or
//! a headless mode) installs it with [`start_net`].

use std::sync::Arc;

use snowlark_net::{NetConfig, NetError, NetThread};

/// [`snowlark_net::orb::ScriptCheck`] over a [`snowlark_js::script_check::ScriptChecker`].
#[derive(Default)]
pub struct V8ScriptCheck(snowlark_js::script_check::ScriptChecker);

impl snowlark_net::orb::ScriptCheck for V8ScriptCheck {
    fn parses_as_script(&self, source: String, reply: Box<dyn FnOnce(bool) + Send>) {
        self.0.check(source, reply);
    }
}

/// [`NetThread::start`] with the script parser installed.
pub fn start_net(config: NetConfig) -> Result<NetThread, NetError> {
    let net = NetThread::start(config)?;
    net.set_script_check(Arc::new(V8ScriptCheck::default()));
    Ok(net)
}
