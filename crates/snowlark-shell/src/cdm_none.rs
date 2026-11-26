// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser's key systems for pages where the system has none the browser uses (Linux: no DRM
//! there, docs/linux.md "Decisions"): every key system is unsupported, so a page's
//! `requestMediaKeySystemAccess` fails as on a browser without one.

use std::sync::Arc;

use snowlark_page::media::cdm::{Cdm, CdmError, CdmEvent, CdmInbox};
use snowlark_tab::comp::{OsHandle, ProtectedPictures};

/// See the module.
pub struct BrowserCdm;

impl BrowserCdm {
    /// A profile's (the folder is not used: nothing is stored).
    pub fn persistent(_dir: std::path::PathBuf) -> Arc<BrowserCdm> {
        Arc::new(BrowserCdm)
    }

    /// An Incognito profile's.
    pub fn off_the_record() -> Arc<BrowserCdm> {
        Arc::new(BrowserCdm)
    }

    /// The protected players' pictures for the compositor: there are none.
    pub fn pictures(&self) -> Arc<dyn ProtectedPictures> {
        Arc::new(NoPictures)
    }
}

struct NoPictures;

impl ProtectedPictures for NoPictures {
    fn surface(&self, _player: u64) -> Option<Arc<OsHandle>> {
        None
    }

    fn place(&self, _player: u64, _x: i32, _y: i32, _width: i32, _height: i32) {}
}

impl Cdm for BrowserCdm {
    fn supports(&self, _key_system: &str, _content_type: &str) -> bool {
        false
    }

    fn create_keys(
        &self,
        request: u64,
        _key_system: &str,
        _origin: &str,
        inbox: &Arc<CdmInbox>,
    ) -> u64 {
        inbox.deliver(CdmEvent::Done {
            request,
            result: Err(CdmError::NotSupported(
                "The key system is not available".into(),
            )),
        });
        0
    }

    // (no keys are ever made, so no session, request or player can name any)
    fn create_session(&self, _keys: u64) -> u64 {
        0
    }

    fn generate_request(
        &self,
        _request: u64,
        _session: u64,
        _init_data_type: &str,
        _data: Vec<u8>,
    ) {
    }

    fn update(&self, _request: u64, _session: u64, _response: Vec<u8>) {}

    fn close(&self, _request: u64, _session: u64) {}

    fn remove(&self, _request: u64, _session: u64) {}

    fn release(&self, _keys: u64) {}
}
