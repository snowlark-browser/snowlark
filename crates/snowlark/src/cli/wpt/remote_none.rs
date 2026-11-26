// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `--wpt` with each test page's services across a pipe (`SNOWLARK_WPT_SITE_LOCK`) needs the
//! renderer's service connection, which is built for Windows only so far (docs/linux.md stage 4):
//! here the setting is refused and every run is the plain one.

use snowlark_net::NetClient;

/// What the services cross to: none can be made here.
pub enum Remote {}

/// One test page's connection: none can be made here.
pub enum Session {}

impl Remote {
    /// `None` when `SNOWLARK_WPT_SITE_LOCK` is unset, an error when it is set.
    pub fn from_env(_net: &NetClient) -> Result<Option<Remote>, String> {
        match std::env::var_os("SNOWLARK_WPT_SITE_LOCK") {
            None => Ok(None),
            Some(_) => Err("SNOWLARK_WPT_SITE_LOCK needs the renderer's service connection, \
                            which this system does not have yet"
                .into()),
        }
    }

    pub fn start(&self, _url: &url::Url) -> Result<Session, String> {
        match *self {}
    }
}

impl Session {
    pub fn connect(&self, _browser: &NetClient, _options: &mut snowlark_page::PageOptions) {
        match *self {}
    }

    pub fn finish(self, _remote: &Remote, _test: &str) {
        match self {}
    }
}
