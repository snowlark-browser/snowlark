// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The text caches on Linux. The cache of shaped runs is `snowlark-graphics`'s, as on Windows; the
//! per-word cache follows the Windows engine's script analysis and is not used here yet
//! ([`WordCache`] holds nothing: `TextEngine::shape_cached` shapes whole runs).

use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

pub use snowlark_graphics::text_cache::*;

use crate::text::TextEngine;

/// Shaped words: not used on this platform yet.
#[derive(Default)]
pub struct WordCache {
    stats: WordCacheStats,
}

impl WordCache {
    /// The most memory one generation holds (an estimate, in bytes).
    pub const GENERATION_BYTES: usize = 4 << 20;

    pub fn new() -> WordCache {
        WordCache::default()
    }

    pub fn stats(&self) -> WordCacheStats {
        self.stats
    }

    /// Segments held.
    pub fn len(&self) -> usize {
        0
    }

    pub fn is_empty(&self) -> bool {
        true
    }

    /// The estimated bytes held.
    pub fn bytes(&self) -> usize {
        0
    }
}

/// The text caches [`TextEngine::shared_text_cache`] hands out: (thread, partition, cache).
pub(crate) type SharedTextCaches = Mutex<Vec<(ThreadId, String, Arc<Mutex<TextCache>>)>>;

/// What a layout tree shapes through: whole runs, then words.
#[derive(Default)]
pub struct TextCache {
    pub runs: ShapeCache,
    pub words: WordCache,
}

impl TextEngine {
    /// The text cache shared by the documents of `partition` (an origin) on the calling thread. An
    /// empty partition (an opaque origin) gets a cache of its own.
    pub fn shared_text_cache(&self, partition: &str) -> Arc<Mutex<TextCache>> {
        /// Partitions kept, least recently used dropped first.
        const KEEP: usize = 8;
        if partition.is_empty() || partition == "null" {
            return Arc::default();
        }
        let thread = std::thread::current().id();
        let mut shared = self.shared_text.lock().unwrap();
        if let Some(i) = shared
            .iter()
            .position(|s| s.0 == thread && s.1 == partition)
        {
            let entry = shared.remove(i);
            let cache = entry.2.clone();
            shared.push(entry);
            return cache;
        }
        if shared.len() >= KEEP {
            shared.remove(0);
        }
        let cache: Arc<Mutex<TextCache>> = Arc::default();
        shared.push((thread, partition.to_string(), cache.clone()));
        cache
    }
}
