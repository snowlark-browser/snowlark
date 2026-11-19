// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the user chose: blocking on or off, the sites it is off for, which lists, their own lists
//! and filters. Kept as `config.json` in the blocker's folder.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BlockerConfig {
    /// Blocking everywhere (the per-site switches apply within it).
    pub enabled: bool,
    /// Sites (registrable domains) blocking is off for.
    pub disabled_sites: BTreeSet<String>,
    /// Lists switched away from their default (by id): `true` on, `false` off.
    pub lists: BTreeMap<String, bool>,
    /// The user's own list URLs.
    pub custom_lists: Vec<String>,
    /// The user's own filters, one per line (trusted, as in uBlock Origin's "My filters").
    pub custom_filters: String,
}

impl Default for BlockerConfig {
    fn default() -> Self {
        BlockerConfig {
            enabled: true,
            disabled_sites: BTreeSet::new(),
            lists: BTreeMap::new(),
            custom_lists: Vec::new(),
            custom_filters: String::new(),
        }
    }
}

impl BlockerConfig {
    pub fn load(path: &Path) -> BlockerConfig {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_defaults_on() {
        let dir =
            std::env::temp_dir().join(format!("snowlark-blocker-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        assert!(BlockerConfig::load(&path).enabled);
        let mut c = BlockerConfig::default();
        c.disabled_sites.insert("example.com".into());
        c.custom_filters = "||x.example^".into();
        c.save(&path).unwrap();
        assert_eq!(BlockerConfig::load(&path), c);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
