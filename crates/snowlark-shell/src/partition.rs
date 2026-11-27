// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A profile's storage partition: everything a site can leave in the browser, and the history of
//! what was visited. The regular profile's lives in its folder; an Incognito window's
//! ([`Partition::off_the_record`]) lives in memory only, is shared by every Incognito window and is
//! gone when it is dropped, with the last of them.
//!
//! | store | regular | off the record |
//! |---|---|---|
//! | cookies, HSTS hosts | `cookies.json`, `hsts.json` | memory |
//! | HTTP cache | memory (the network thread's) | memory, its own network thread |
//! | `localStorage` | `local-storage\` | memory |
//! | `sessionStorage` | each tab's, memory | the same |
//! | IndexedDB | `IndexedDB\`, quota from the volume | memory, quota from the machine's memory |
//! | Cache API (`caches`) | `CacheStorage\` | memory |
//! | service worker registrations | `ServiceWorker\` | memory |
//! | script code cache | `code-cache\` | none |
//! | `BroadcastChannel` | its own hub | its own hub |
//! | history | `history.json` | none kept |
//! | key systems' module state | `cdm\` | a temporary folder, deleted with it |
//!
//! Nothing is shared between two partitions, so no tab of one can see what a tab of the other
//! stored: each tab's services (its network, storage, IndexedDB and channels) are its partition's.

use std::sync::{Arc, Mutex};

#[cfg(unix)]
use snowlark_ipc::posix as ipc_os;
#[cfg(windows)]
use snowlark_ipc::win as ipc_os;
use snowlark_js::code_cache::CodeCacheStore;
use snowlark_net::{NetClient, NetConfig, NetThread};

use crate::history::History;
use crate::local_storage::LocalStorage;
use crate::profile::Profile;

/// See the module documentation.
pub struct Partition {
    /// The profile's folder (`None`: off the record).
    dir: Option<std::path::PathBuf>,
    _net_thread: NetThread,
    pub net: NetClient,
    pub local_storage: LocalStorage,
    pub idb: snowlark_page::IdbBackend,
    /// The Cache API's caches: a folder per storage key in the profile's `CacheStorage`, or memory.
    pub caches: Arc<snowlark_sw::LocalCacheStorage>,
    pub broadcast: snowlark_page::BroadcastHub,
    /// The service worker registrations: those with an active worker kept per storage key in the
    /// profile's `ServiceWorker` folder, or in memory. Their scripts are fetched through `net`.
    pub service_workers: Arc<snowlark_sw::LocalServiceWorkers>,
    /// The pages visited, shared with the history page and the address bar.
    pub history: Arc<Mutex<History>>,
    /// The key systems' module state: per origin in the profile's `cdm` folder, or a temporary
    /// folder deleted with an Incognito partition.
    pub cdm: Arc<crate::cdm::BrowserCdm>,
}

impl Partition {
    /// The regular profile's, kept in `profile`'s folder.
    pub fn regular(profile: &Profile) -> Partition {
        let mut config = NetConfig::from_env();
        profile.ensure();
        config.profile_dir = Some(profile.dir.clone());
        let net_thread = snowlark_tab::script_check::start_net(config.clone())
            .or_else(|_| {
                config.profile_dir = None;
                snowlark_tab::script_check::start_net(config)
            })
            .expect("network thread");
        let history = History::load(&profile.history_file(), profile.legacy_history().as_deref());
        let net = net_thread.client();
        Partition {
            dir: Some(profile.dir.clone()),
            service_workers: service_workers(&net, Some(profile.dir.join("ServiceWorker"))),
            net,
            _net_thread: net_thread,
            local_storage: LocalStorage::open(profile.local_storage_dir()),
            // (IndexedDB's databases live in the profile, a folder per origin; each origin's
            // quota is a share of the profile's volume)
            idb: snowlark_page::IdbBackend::persistent_with_quota(
                profile.dir.join("IndexedDB"),
                ipc_os::volume_size(&profile.dir)
                    .map_or(snowlark_idb::QUOTA, snowlark_idb::quota_for_volume),
            ),
            caches: Arc::new(snowlark_sw::LocalCacheStorage::persistent(
                profile.dir.join("CacheStorage"),
            )),
            broadcast: snowlark_page::BroadcastHub::default(),
            history: Arc::new(Mutex::new(history)),
            cdm: crate::cdm::BrowserCdm::persistent(profile.dir.join("cdm")),
        }
    }

    /// An Incognito profile's: in memory only, nothing of it reaches the disk.
    pub fn off_the_record() -> Partition {
        let mut config = NetConfig::from_env();
        config.profile_dir = None;
        let net_thread = snowlark_tab::script_check::start_net(config).expect("network thread");
        let quota = ipc_os::physical_memory().map_or(snowlark_idb::QUOTA, |memory| {
            snowlark_idb::quota_in_memory(memory, random_unit())
        });
        let net = net_thread.client();
        Partition {
            dir: None,
            service_workers: service_workers(&net, None),
            net,
            _net_thread: net_thread,
            local_storage: LocalStorage::in_memory(),
            idb: snowlark_page::IdbBackend::in_memory_with_quota(quota),
            caches: Arc::new(snowlark_sw::LocalCacheStorage::in_memory()),
            broadcast: snowlark_page::BroadcastHub::default(),
            history: Arc::new(Mutex::new(History::in_memory())),
            cdm: crate::cdm::BrowserCdm::off_the_record(),
        }
    }

    /// Whether this is an Incognito profile's partition.
    pub fn is_off_the_record(&self) -> bool {
        self.dir.is_none()
    }

    /// The script code cache renderer processes of this partition are served: the profile's,
    /// unless `SNOWLARK_CODE_CACHE=0`; none off the record.
    pub fn code_cache(&self) -> Arc<dyn CodeCacheStore> {
        let enabled = std::env::var("SNOWLARK_CODE_CACHE").map_or(true, |v| v != "0");
        match self.net.profile_dir().filter(|_| enabled) {
            Some(dir) => snowlark_js::code_cache::CodeCache::open(
                dir.join("code-cache"),
                snowlark_js::code_cache::DEFAULT_BUDGET,
            ) as Arc<dyn CodeCacheStore>,
            None => Arc::new(crate::tab_host::NoCodeCache),
        }
    }
}

/// A partition's service worker registry, fetching through `net`, kept in `folder` (none: memory).
fn service_workers(
    net: &NetClient,
    folder: Option<std::path::PathBuf>,
) -> Arc<snowlark_sw::LocalServiceWorkers> {
    Arc::new(snowlark_sw::LocalServiceWorkers::new(
        Some(net.backend().clone()),
        folder,
    ))
}

/// A number in [0, 1) that differs from run to run (from the standard library's random hash keys).
fn random_unit() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_random_unit_is_in_range() {
        for _ in 0..100 {
            let u = super::random_unit();
            assert!((0.0..1.0).contains(&u), "{u}");
        }
    }
}
