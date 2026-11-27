// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser half of Service Workers (W3C Service Workers, `docs/service-workers.md`): what outlives
//! the pages of a storage key and is shared by all of them. Pages reach it only through the
//! message-shaped traits here, so it can live in the browser's process while they run in renderers.
//!
//! - [`cache`]: the Cache API's storage (§5.4 to §5.5): each storage key's name to cache map, the
//!   request-response lists, the "Query Cache" and "Batch Cache Operations" algorithms, kept in
//!   memory or in a folder per key.
//! - [`registry`]: the service worker registrations (§3.4, §8): their job queues, the Register,
//!   Update, Install, Activate and Unregister algorithms, and the security checks of registration.
//! - [`host`]: the registry on a thread of its own behind the message-shaped [`ServiceWorkerHost`].

pub mod cache;
pub mod host;
pub mod registry;

pub use host::{Fetch, LocalServiceWorkers, NoticeSink, ServiceWorkerBackend, ServiceWorkerHost};

pub use cache::{
    BatchOp, CacheError, CacheId, CacheStorageService, CachedRequest, CachedResponse,
    LocalCacheStorage, Op, QueryOptions, Reply,
};

use std::sync::Arc;

/// The handle to a profile's Cache API storage that pages and workers share.
#[derive(Clone)]
pub struct CacheBackend(Arc<dyn CacheStorageService>);

impl CacheBackend {
    /// Storage in this process that keeps nothing on disk.
    pub fn in_memory() -> CacheBackend {
        CacheBackend(Arc::new(LocalCacheStorage::in_memory()))
    }

    /// A handle to another implementation (storage in another process, or a
    /// [`LocalCacheStorage`] the browser also keeps for clearing site data).
    pub fn new(service: Arc<dyn CacheStorageService>) -> CacheBackend {
        CacheBackend(service)
    }

    /// The implementation behind the handle (what the browser serves a renderer process from).
    pub fn service(&self) -> &Arc<dyn CacheStorageService> {
        &self.0
    }
}

impl Default for CacheBackend {
    fn default() -> Self {
        CacheBackend::in_memory()
    }
}

impl std::fmt::Debug for CacheBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheBackend").finish_non_exhaustive()
    }
}

impl std::ops::Deref for CacheBackend {
    type Target = dyn CacheStorageService;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}
