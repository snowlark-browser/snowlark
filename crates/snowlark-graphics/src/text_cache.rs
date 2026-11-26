// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The cache of whole shaped runs ([`ShapeCache`]) and the statistics of the per-word cache, which
//! sit above any text engine. `snowlark-paint`'s `text_cache` has the per-word cache, whose
//! segmentation follows the engine's script analysis.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Arc;

use rustc_hash::FxBuildHasher;
use smallvec::SmallVec;

use crate::text::{FontFeature, FontListId, ShapedText, VerticalText};

/// What the cache did (statistics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WordCacheStats {
    /// Segments found in the cache.
    pub hits: u64,
    /// Segments shaped (and stored, when short enough).
    pub misses: u64,
    /// UTF-16 code units the text engine shaped for missed segments.
    pub shaped_units: u64,
    /// Runs shaped per segment, and runs shaped whole (scripts not split, right-to-left runs).
    pub runs_by_word: u64,
    pub runs_whole: u64,
    /// Texts that skipped the script and bidi analysis and font fallback.
    pub simple_texts: u64,
    /// Boundary probes made.
    pub probes: u64,
}

/// Shaped text runs by everything shaping depends on (text, font list, size, direction, locale,
/// features), shared by all inline formatting contexts of a layout tree (and its partition's other
/// trees, see `snowlark-paint`'s `TextEngine::shared_text_cache`): an unchanged run in a regenerated or re-prepared
/// formatting context shapes nothing. Bounded by an estimate of the memory held: two
/// generations, each up to half the budget; a hit in the older one moves the entry to the newer, and
/// when the newer is full the older is dropped (an LRU approximation with O(1) work per access).
#[derive(Default)]
pub struct ShapeCache {
    current: ShapeMap,
    previous: ShapeMap,
    current_bytes: usize,
    /// Lookups answered from the cache, and shapings stored in it (statistics).
    pub hits: u64,
    pub misses: u64,
}

type ShapeMap = HashMap<u64, SmallVec<[ShapeEntry; 1]>, FxBuildHasher>;

pub struct ShapeEntry {
    text: Box<str>,
    list: u32,
    size: u32,
    rtl: bool,
    vertical: VerticalText,
    locale: Box<str>,
    features: Box<[FontFeature]>,
    shaped: Arc<ShapedText>,
    bytes: usize,
}

/// What a shaping depends on (borrowed: lookups allocate nothing).
pub struct ShapeKey<'a> {
    pub text: &'a str,
    pub list: FontListId,
    pub size: f32,
    pub rtl: bool,
    pub vertical: VerticalText,
    pub locale: &'a str,
    pub features: &'a [FontFeature],
}

impl ShapeKey<'_> {
    fn hash(&self) -> u64 {
        let mut h = FxBuildHasher.build_hasher();
        self.text.hash(&mut h);
        self.list.0.hash(&mut h);
        self.size.to_bits().hash(&mut h);
        self.rtl.hash(&mut h);
        self.vertical.hash(&mut h);
        self.locale.hash(&mut h);
        for f in self.features {
            f.tag.hash(&mut h);
            f.value.hash(&mut h);
        }
        h.finish()
    }

    fn matches(&self, e: &ShapeEntry) -> bool {
        &*e.text == self.text
            && e.list == self.list.0
            && e.size == self.size.to_bits()
            && e.rtl == self.rtl
            && e.vertical == self.vertical
            && &*e.locale == self.locale
            && &*e.features == self.features
    }
}

impl ShapeCache {
    /// The budget for both generations together (an estimate of the bytes held).
    pub const BUDGET: usize = 32 << 20;
    /// Longer runs are shaped every time (one such run is a large share of the budget).
    pub const MAX_TEXT: usize = 64 << 10;

    fn take(map: &mut ShapeMap, hash: u64, key: &ShapeKey) -> Option<ShapeEntry> {
        let bucket = map.get_mut(&hash)?;
        let i = bucket.iter().position(|e| key.matches(e))?;
        let e = bucket.swap_remove(i);
        if bucket.is_empty() {
            map.remove(&hash);
        }
        Some(e)
    }

    /// The cached shaping for `key`, if any.
    pub fn get(&mut self, key: &ShapeKey) -> Option<Arc<ShapedText>> {
        let hash = key.hash();
        if let Some(bucket) = self.current.get(&hash) {
            if let Some(e) = bucket.iter().find(|e| key.matches(e)) {
                self.hits += 1;
                return Some(e.shaped.clone());
            }
        }
        let e = Self::take(&mut self.previous, hash, key)?;
        self.hits += 1;
        let shaped = e.shaped.clone();
        self.store(hash, e);
        Some(shaped)
    }

    /// Caches `shaped` for `key`.
    pub fn insert(&mut self, key: &ShapeKey, shaped: Arc<ShapedText>) {
        if key.text.len() > Self::MAX_TEXT {
            return;
        }
        self.misses += 1;
        let bytes = std::mem::size_of::<ShapeEntry>()
            + key.text.len()
            + key.locale.len()
            + std::mem::size_of_val(key.features)
            + std::mem::size_of_val(&shaped.runs[..])
            + std::mem::size_of_val(&shaped.clusters[..])
            + shaped.glyphs.len() * (2 + 4)
            + std::mem::size_of_val(&shaped.offsets[..]);
        let e = ShapeEntry {
            text: key.text.into(),
            list: key.list.0,
            size: key.size.to_bits(),
            rtl: key.rtl,
            vertical: key.vertical,
            locale: key.locale.into(),
            features: key.features.into(),
            shaped,
            bytes,
        };
        self.store(key.hash(), e);
    }

    fn store(&mut self, hash: u64, e: ShapeEntry) {
        if self.current_bytes + e.bytes > Self::BUDGET / 2 {
            self.previous = std::mem::take(&mut self.current);
            self.current_bytes = 0;
        }
        self.current_bytes += e.bytes;
        self.current.entry(hash).or_default().push(e);
    }

    /// Entries held (both generations).
    pub fn len(&self) -> usize {
        self.current
            .values()
            .chain(self.previous.values())
            .map(|b| b.len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The estimated bytes held by the newer generation.
    pub fn current_bytes(&self) -> usize {
        self.current_bytes
    }
}
