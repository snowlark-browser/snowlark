// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What layout keeps on a box between layouts: the last result with the constraint space it was
//! computed for (LayoutNG's idea: a clean box laid out in an equal space reuses its fragment), the
//! intrinsic inline sizes, the prepared inline formatting context, and taffy's per-node state.

use crate::flex_grid::TaffyNode;
use crate::inline::InlineContent;
use crate::layout::{LayoutResult, Space};
use crate::sizing::ContentSizes;
use crate::style::Style;

pub(crate) struct CachedLayout<S: Style> {
    pub space: Space,
    /// The layout pass that computed it (a dirty box reuses results from the current pass).
    pub pass: u32,
    pub result: LayoutResult<S>,
}

pub(crate) struct BoxCache<S: Style> {
    pub layout: Option<Box<CachedLayout<S>>>,
    pub intrinsic: Option<(u32, ContentSizes)>,
    /// The intrinsic inline sizes as an orthogonal flow's parent sees them (its block size,
    /// CSS Writing Modes 4 §7.3.2).
    pub intrinsic_across: Option<(u32, ContentSizes)>,
    pub inline: Option<Box<InlineContent<S>>>,
    /// The prepared inline content no longer matches the box's text or children.
    pub inline_stale: bool,
    /// Boxes of the inline content were given styles that prepare it the same way
    /// ([`Style::same_inline_preparation`]): the prepared items stay, and take their boxes' new
    /// style handles before the next layout.
    pub inline_restyled: bool,
    pub taffy: Option<Box<TaffyNode<S>>>,
}

impl<S: Style> Default for BoxCache<S> {
    fn default() -> Self {
        BoxCache {
            layout: None,
            intrinsic: None,
            intrinsic_across: None,
            inline: None,
            inline_stale: true,
            inline_restyled: false,
            taffy: None,
        }
    }
}

impl<S: Style> BoxCache<S> {
    /// Drops what depends on layout constraints or descendants (not the shaped inline content, which
    /// only depends on the box's own text and is invalidated separately).
    pub fn invalidate(&mut self) {
        self.layout = None;
        self.intrinsic = None;
        self.intrinsic_across = None;
        if let Some(t) = &mut self.taffy {
            t.cache.clear();
            t.built = None;
        }
    }
}
