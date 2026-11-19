// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Storage Standard's `StorageManager` (`navigator.storage`): whether the bucket of a global's
//! storage shelf is persistent, the request to make it so, and the estimate of its usage and quota.
//!
//! The shelf is the storage key IndexedDB uses ([`Engine::storage_key`]). Its one bucket is
//! "best-effort": Snowlark grants no "persistent-storage" permission, so `persisted()` and `persist()`
//! answer false. `estimate()` asks the IndexedDB backend (in the browser's process when tabs have
//! their own), which answers with what the key's databases take on disk and the quota.

use snowlark_bindings::generated::{NavigatorStorageImpl, StorageEstimate, StorageManagerImpl};
use snowlark_bindings::rt::{self, Cx, Handle, JsPromise};

use super::{Engine, Object};
use crate::event_loop::TaskSource;

impl Engine {
    /// The global (Window or WorkerGlobalScope) a StorageManager belongs to.
    fn storage_manager_global(&self, manager: Handle) -> Handle {
        match &*self.obj(manager) {
            Object::StorageManager { global } => *global,
            _ => manager,
        }
    }

    /// "Obtain a local storage shelf" for the manager's global, or a rejected promise (TypeError)
    /// when it has none (an opaque origin).
    fn storage_shelf(&self, cx: &mut Cx, manager: Handle, p: &JsPromise) -> Option<String> {
        let key = self.storage_key(self.storage_manager_global(manager));
        if key.is_none() {
            p.reject(
                cx,
                rt::Error::Type("Storage is not available in an opaque origin".into()),
            );
        }
        key
    }

    /// The backend's answer to `estimate()`.
    pub(crate) fn storage_estimated(&self, cx: &mut Cx, id: u64, usage: u64, quota: u64) {
        let Some(p) = self.idb.borrow_mut().estimates.remove(&id) else {
            return;
        };
        p.resolve(
            cx,
            StorageEstimate {
                usage: Some(usage),
                quota: Some(quota),
            },
        );
    }

    /// `persisted()` and `persist()`: false, in a task of the storage task source.
    fn storage_not_persistent(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        if self.storage_shelf(cx, this, &p).is_some() {
            let resolve = p.clone();
            self.queue_task(TaskSource::Storage, move |_, cx| {
                resolve.resolve(cx, false);
            });
        }
        Ok(p)
    }
}

impl NavigatorStorageImpl for Engine {
    /// `[SameObject] storage`: one StorageManager per global.
    fn storage(&self, this: Handle) -> Handle {
        let global = self.navigator_window(this);
        self.window_singleton(global, "storage", || Object::StorageManager { global })
    }
}

impl StorageManagerImpl for Engine {
    /// "The persisted() method steps": the bucket's mode is "persistent". It never is.
    fn persisted(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.storage_not_persistent(cx, this)
    }

    /// "The persist() method steps": request permission to use "persistent-storage". The user
    /// agent denies it (there is no such permission to give), so the bucket stays best-effort.
    fn persist(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.storage_not_persistent(cx, this)
    }

    /// "The estimate() method steps": the shelf's storage usage and quota, from the backend.
    fn estimate(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let Some(key) = self.storage_shelf(cx, this, &p) else {
            return Ok(p);
        };
        let id = self.idb_next_id();
        let (backend, loop_id) = self.idb_backend();
        self.idb.borrow_mut().estimates.insert(id, p.clone());
        backend.estimate(loop_id, id, &key);
        Ok(p)
    }
}
