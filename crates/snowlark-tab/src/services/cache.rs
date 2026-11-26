// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Cache API across processes (Service Workers §5.4 to §5.5): the browser's
//! [`CacheStorageService`] holds every storage key's caches, and each call a renderer's page makes is
//! one call on its pipe. Operations and answers travel encoded as bulk, since one response body may
//! be large. The storage key is checked against the renderer's site lock; a cache id of another key
//! is answered as unknown by the storage itself.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use snowlark_ipc::{wire, Bulk, Error, Replier};
use snowlark_sw::{CacheError, CacheStorageService, Op, Reply};

use super::{BrowserPort, RendererPort, CALL_WAIT};

/// Renderer to browser.
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    /// A call: an [`Op`] on the caches of storage key `key`, encoded, as bulk. The answer is its
    /// [`Reply`], encoded, as bulk.
    Call { key: String, op: Bulk },
}

/// The renderer's [`CacheStorageService`].
pub struct RemoteCaches {
    port: RendererPort,
}

impl RemoteCaches {
    pub fn new(port: RendererPort) -> RemoteCaches {
        RemoteCaches { port }
    }
}

impl CacheStorageService for RemoteCaches {
    fn call(&self, key: &str, op: Op) -> Reply {
        let reply = wire::to_bytes(&op)
            .and_then(|bytes| self.port.bulk(bytes, CALL_WAIT))
            .and_then(|op| {
                let message = super::Up::Cache(Up::Call {
                    key: key.to_owned(),
                    op,
                });
                self.port.call::<Bulk>(&message, CALL_WAIT)
            })
            .and_then(|bulk| self.port.open_bulk(bulk))
            .and_then(|bytes| wire::decode::<Reply>(&bytes));
        // (with the browser gone nothing can be stored: this process ends soon)
        reply.unwrap_or(Reply::Error(CacheError::QuotaExceeded))
    }
}

/// The browser's end of one renderer's cache calls.
pub struct CacheHost {
    port: BrowserPort,
    backend: Arc<dyn CacheStorageService>,
    site_lock: super::site_lock::Mode,
}

impl CacheHost {
    pub fn new(
        port: BrowserPort,
        backend: Arc<dyn CacheStorageService>,
        site_lock: super::site_lock::Mode,
    ) -> CacheHost {
        CacheHost {
            port,
            backend,
            site_lock,
        }
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    pub(super) fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        let Up::Call { key, op } = message;
        super::site_lock::check(
            &self.port,
            self.site_lock,
            super::site_lock::origin_string_violation(
                self.port.policy(),
                "the cache's storage key",
                &key,
            ),
        )?;
        let Some(replier) = replier else {
            return Err(self.bad("a cache call with nobody to answer".into()));
        };
        let bytes = self.port.open_bulk(op)?;
        let op =
            wire::decode::<Op>(&bytes).map_err(|e| self.bad(format!("a cache operation: {e}")))?;
        let reply = self.backend.call(&key, op);
        let bulk = self.port.bulk(&wire::to_bytes(&reply)?)?;
        // (a renderer that stopped waiting is no reason to stop serving it)
        let _ = replier.reply(&bulk);
        Ok(())
    }
}
