// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The text caches on Linux. The cache of shaped runs is `snowlark-graphics`'s, as on Windows; the
//! per-word cache follows the text engine's script analysis, and there is no engine here yet
//! (`crate::text`), so [`WordCache`] holds nothing.

use std::sync::{Arc, Mutex};

pub use snowlark_graphics::text_cache::*;

use crate::text::TextEngine;

/// Shaped words: empty until the platform has a text engine.
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

/// What a layout tree shapes through: whole runs, then words.
#[derive(Default)]
pub struct TextCache {
    pub runs: ShapeCache,
    pub words: WordCache,
}

impl TextEngine {
    /// The text cache shared by the documents of `partition` (an origin) on the calling thread.
    pub fn shared_text_cache(&self, _partition: &str) -> Arc<Mutex<TextCache>> {
        match self.never {}
    }
}
