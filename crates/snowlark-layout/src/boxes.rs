// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The box tree (CSS Display 3 §1, CSS 2 §9.2): an arena of [`LayoutBox`]es named by [`BoxId`] (index plus
//! generation, like the DOM's `NodeId`, so an id kept across an incremental rebuild can't alias a reused
//! slot). Boxes carry their style handle, their DOM node (none for anonymous boxes), their children in
//! order, and what layout caches on them: the last layout result keyed by its constraint space, the
//! intrinsic sizes, the prepared inline formatting context and taffy's per-node data.

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use smallvec::SmallVec;
use snowlark_dom::NodeId;
use snowlark_paint::TextCache;

use crate::style::*;

/// A box's identity in its [`BoxTree`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BoxId {
    pub(crate) index: u32,
    pub(crate) generation: NonZeroU32,
}

impl BoxId {
    /// Packs the id into a `u64` (taffy's node ids).
    #[inline]
    pub fn to_bits(self) -> u64 {
        (u64::from(self.generation.get()) << 32) | u64::from(self.index)
    }

    /// Inverse of [`BoxId::to_bits`].
    #[inline]
    pub fn from_bits(bits: u64) -> Option<BoxId> {
        Some(BoxId {
            index: bits as u32,
            generation: NonZeroU32::new((bits >> 32) as u32)?,
        })
    }
}

/// What kind of box (CSS Display 3 §2; CSS Tables 3 §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxKind {
    /// A block container: `flow`/`flow-root` inner display, list items, anonymous blocks, outside markers.
    /// Its children are all block-level, or all inline-level (an inline formatting context: see
    /// [`LayoutBox::has_inline_content`]).
    Block,
    Flex,
    Grid,
    /// The table wrapper box: captions and the table grid box.
    TableWrapper,
    Table,
    TableRowGroup(RowGroup),
    TableRow,
    /// A table cell (a block container).
    TableCell,
    TableColumnGroup,
    TableColumn,
    /// A table caption (a block container).
    TableCaption,
    /// A non-atomic inline box.
    Inline,
    /// A run of text (the style is the parent's).
    Text,
    /// A replaced element (atomic).
    Replaced,
    /// A forced line break (`<br>`).
    LineBreak,
}

/// Which row group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowGroup {
    Header,
    Body,
    Footer,
}

/// The outer display type a box participates with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Block,
    Inline,
    /// Table-internal (rows, cells, columns, …): laid out only by the table.
    Internal,
}

pub(crate) mod flags {
    /// A block container whose children form an inline formatting context.
    pub const INLINE_CONTENT: u32 = 1;
    pub const FLOAT: u32 = 2;
    /// `position: absolute` or `fixed`.
    pub const OUT_OF_FLOW: u32 = 4;
    /// Establishes an independent formatting context (BFC root, flex/grid/table, atomic inline, …).
    pub const INDEPENDENT: u32 = 8;
    /// Needs layout: its own content or a descendant changed.
    pub const DIRTY: u32 = 16;
    /// A continuation of an inline box split around a block (CSS 2 §9.2.1.1): no start edge.
    pub const NO_START_EDGE: u32 = 32;
    /// … no end edge.
    pub const NO_END_EDGE: u32 = 64;
    /// An outside list marker.
    pub const OUTSIDE_MARKER: u32 = 128;
    /// The box was inline-level before blockification (static position of out-of-flow boxes).
    pub const ORIGINALLY_INLINE: u32 = 256;
    /// The root element's box.
    pub const ROOT: u32 = 512;
    /// A flex or grid item (its parent is a flex or grid container).
    pub const FLEX_GRID_ITEM: u32 = 1024;
    /// The box's background was propagated to the canvas (CSS Backgrounds 3 §2.11.2).
    pub const BACKGROUND_TO_CANVAS: u32 = 2048;
    /// A `fieldset` element's block container (HTML §15.3.13).
    pub const FIELDSET: u32 = 4096;
    /// A fieldset's rendered legend: laid out in the fieldset's block-start border.
    pub const RENDERED_LEGEND: u32 = 8192;
    /// The `<body>` whose `overflow` applies to the viewport (CSS Overflow 3 §3.3).
    pub const VIEWPORT_OVERFLOW: u32 = 16384;
    /// A box of a top layer element or its `::backdrop` (a child of the root's box).
    pub const TOP_LAYER: u32 = 32768;
    /// A `button` element's principal box: it uses button layout (HTML §15.5.3), so an `auto`
    /// inline size is used as the fit-content inline size.
    pub const BUTTON: u32 = 65536;
    /// The box's custom vertical scroll bar showed in its last layout, for an `overflow-y: auto`
    /// that overflowed ([`crate::gutter`]).
    pub const VERTICAL_BAR: u32 = 131072;
    /// … its horizontal bar, for `overflow-x: auto`.
    pub const HORIZONTAL_BAR: u32 = 262144;
}

/// A text decoration propagated to a box's text (CSS Text Decoration 3 §2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decoration {
    pub line: TextDecorationLine,
    pub color: Color,
    pub style: TextDecorationStyle,
}

pub(crate) type Decorations = SmallVec<[Decoration; 1]>;

/// Box-kind specific data.
#[derive(Clone, Debug)]
pub(crate) enum BoxData {
    None,
    /// The text of a Text box (as in the DOM, before white-space processing).
    Text(String),
    /// The text of a Text box holding the rest of a text node after its `::first-letter` (CSS
    /// Pseudo 4 §3.2), and the byte offset in the node's data where it starts.
    TextRest(String, u32),
    Replaced(ReplacedContent),
    /// A table cell's spans (HTML `colspan`/`rowspan`, 1 for CSS-only cells).
    Cell {
        colspan: u32,
        rowspan: u32,
    },
    /// A column's or column group's `span`.
    Column {
        span: u32,
    },
}

/// One box.
pub struct LayoutBox<S: Style> {
    pub(crate) generation: NonZeroU32,
    pub(crate) kind: BoxKind,
    pub(crate) level: Level,
    pub(crate) style: S,
    pub(crate) node: Option<NodeId>,
    pub(crate) pseudo: Option<PseudoElement>,
    /// The originating element of a generated `::before` or `::after` box (`node` is `None`).
    pub(crate) pseudo_of: Option<NodeId>,
    pub(crate) parent: Option<BoxId>,
    pub(crate) children: Vec<BoxId>,
    pub(crate) flags: u32,
    /// Pre-order position in the tree (painting order of positioned boxes).
    pub(crate) order: u32,
    pub(crate) data: BoxData,
    /// A list item's outside marker (not one of its children).
    pub(crate) marker: Option<BoxId>,
    /// Decorations its text inherits.
    pub(crate) decorations: Decorations,
    pub(crate) cache: crate::cache::BoxCache<S>,
}

impl<S: Style> LayoutBox<S> {
    pub fn kind(&self) -> BoxKind {
        self.kind
    }

    pub fn level(&self) -> Level {
        self.level
    }

    pub fn style(&self) -> &S {
        &self.style
    }

    pub fn node(&self) -> Option<NodeId> {
        self.node
    }

    pub fn pseudo(&self) -> Option<PseudoElement> {
        self.pseudo
    }

    pub fn parent(&self) -> Option<BoxId> {
        self.parent
    }

    pub fn children(&self) -> &[BoxId] {
        &self.children
    }

    pub fn is_anonymous(&self) -> bool {
        self.node.is_none() && self.pseudo.is_none()
    }

    /// The box's text, for Text boxes.
    pub fn text(&self) -> Option<&str> {
        match &self.data {
            BoxData::Text(t) | BoxData::TextRest(t, _) => Some(t),
            _ => None,
        }
    }

    /// Where a Text box's text starts in its node's data (past a `::first-letter`'s).
    pub(crate) fn text_start(&self) -> u32 {
        match &self.data {
            BoxData::TextRest(_, start) => *start,
            _ => 0,
        }
    }

    #[inline]
    pub(crate) fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }

    #[inline]
    pub(crate) fn set(&mut self, flag: u32, on: bool) {
        if on {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }

    /// A block container whose children are inline-level.
    pub fn has_inline_content(&self) -> bool {
        self.has(flags::INLINE_CONTENT)
    }

    pub fn is_float(&self) -> bool {
        self.has(flags::FLOAT)
    }

    pub fn is_out_of_flow(&self) -> bool {
        self.has(flags::OUT_OF_FLOW)
    }

    /// Floated or absolutely positioned.
    pub fn is_out_of_flow_or_float(&self) -> bool {
        self.flags & (flags::FLOAT | flags::OUT_OF_FLOW) != 0
    }

    /// Lays out its content as a block container (block, cell, caption).
    pub fn is_block_container(&self) -> bool {
        matches!(
            self.kind,
            BoxKind::Block | BoxKind::TableCell | BoxKind::TableCaption
        )
    }

    /// An in-flow block-level box.
    pub fn is_in_flow_block_level(&self) -> bool {
        self.level == Level::Block && !self.is_out_of_flow_or_float()
    }

    /// Establishes an independent formatting context.
    pub fn is_independent(&self) -> bool {
        self.has(flags::INDEPENDENT)
    }

    /// An atomic inline-level box (inline-block, inline-table, inline-flex, replaced inline, …).
    pub fn is_atomic_inline(&self) -> bool {
        self.level == Level::Inline
            && !matches!(
                self.kind,
                BoxKind::Inline | BoxKind::Text | BoxKind::LineBreak
            )
    }

    /// The dirty flag.
    pub fn is_dirty(&self) -> bool {
        self.has(flags::DIRTY)
    }
}

/// Whether a box's number reaches painting: it sorts positioned boxes and stacking contexts
/// (see `paint::makes_stacking_context`).
fn order_matters<S: Style>(b: &LayoutBox<S>) -> bool {
    let s = &b.style;
    s.position() != Position::Static
        || s.z_index().is_some()
        || s.opacity() < 1.0
        || s.has_transform()
        || s.forces_stacking_context()
}

/// The distance between box numbers after a full numbering ([`BoxTree::renumber`]).
pub(crate) const ORDER_GAP: u32 = 64;

/// The box tree of one document.
pub struct BoxTree<S: Style> {
    pub(crate) boxes: Vec<Option<LayoutBox<S>>>,
    generations: Vec<NonZeroU32>,
    free: Vec<u32>,
    pub(crate) root: Option<BoxId>,
    /// Each DOM node's principal box, indexed by `NodeId::index`.
    node_boxes: Vec<Option<(NodeId, BoxId)>>,
    /// `<length-percentage>` sums handed to taffy as calc values, by index (see `flex_grid`).
    pub(crate) calcs: Vec<LengthPercentage>,
    /// The box whose background paints the canvas.
    pub(crate) canvas_background: Option<BoxId>,
    /// The `<body>` box whose overflow applies to the viewport.
    pub(crate) viewport_overflow: Option<BoxId>,
    /// The box whose `writing-mode` and `direction` are the principal writing mode (CSS Writing
    /// Modes 4 §8): the `<body>`, or the root.
    pub(crate) principal: Option<BoxId>,
    /// Orthogonal flows whose available inline space came from the initial containing block
    /// (CSS Writing Modes 4 §7.3.1): nothing in their constraint spaces says so, so a change of
    /// viewport marks them dirty.
    pub(crate) icb_dependents: std::collections::HashSet<BoxId>,
    /// Shaped text runs and words, shared by all inline formatting contexts (and by the embedder's
    /// other trees when it sets a shared cache).
    pub(crate) text_cache: Arc<Mutex<TextCache>>,
    /// Fonts resolved for the inline content's styles, by the identity of their font properties.
    pub(crate) fonts: crate::inline::FontCache<S>,
    /// Boxes allocated since the tree was made (statistics).
    pub(crate) allocated: u64,
    /// The block-level boxes that were inside inline boxes split around them (CSS 2 §9.2.1.1), with
    /// those inline boxes (innermost first): their relative offsets move the block too, and they can
    /// be the containing block of its out-of-flow descendants.
    pub(crate) split_inlines: rustc_hash::FxHashMap<BoxId, SmallVec<[BoxId; 2]>>,
    /// The document is in quirks or limited-quirks mode: the line height calculation quirk and the
    /// blocks-ignore-line-height quirk apply (Quirks Mode §3.3, §3.4).
    pub(crate) line_height_quirk: bool,
    /// Block-level boxes an inline box was split around (CSS 2 §9.2.1.1), with the elements of the
    /// inline boxes split (innermost first): their client rects include the block's, as browsers
    /// report them.
    pub(crate) split_owners: std::collections::HashMap<BoxId, SmallVec<[NodeId; 2]>>,
    /// For a block-level box in `split_owners`, the anonymous block box that holds it among the
    /// pieces of the inline (CSS 2 §9.2.1.1) as its last placement made it, relative to the block's
    /// border box: its containing block's content width, the block's height, where the block is
    /// before its own relative offset. The inline's client rect beside the block is this box.
    pub(crate) split_wrappers: rustc_hash::FxHashMap<BoxId, snowlark_paint::Rect>,
    /// `::first-line` styles (CSS Pseudo 4 §3.1): for a block container whose inline content
    /// holds a first formatted line, the line's root style; for each inline and text box in that
    /// content, its style as it is on the first line. The element with the `::first-line` rule
    /// is keyed too (with its `::first-line` style), so a restyle of it builds its contents again.
    pub(crate) first_line: rustc_hash::FxHashMap<BoxId, S>,
    /// The boxes made dirty since the last layout (some may have died since), so clearing the flags
    /// after a layout costs what changed, not the tree.
    dirty: Vec<BoxId>,
}

impl<S: Style> Default for BoxTree<S> {
    fn default() -> BoxTree<S> {
        BoxTree::new()
    }
}

impl<S: Style> BoxTree<S> {
    pub fn new() -> BoxTree<S> {
        BoxTree {
            boxes: Vec::new(),
            generations: Vec::new(),
            free: Vec::new(),
            root: None,
            node_boxes: Vec::new(),
            calcs: Vec::new(),
            canvas_background: None,
            icb_dependents: Default::default(),
            principal: None,
            viewport_overflow: None,
            text_cache: Default::default(),
            fonts: Default::default(),
            allocated: 0,
            split_inlines: Default::default(),
            line_height_quirk: false,
            split_owners: Default::default(),
            split_wrappers: Default::default(),
            first_line: Default::default(),
            dirty: Vec::new(),
        }
    }

    /// The root element's box.
    pub fn root(&self) -> Option<BoxId> {
        self.root
    }

    /// How many boxes were created since the tree was made (statistics).
    pub fn allocated(&self) -> u64 {
        self.allocated
    }

    /// How many boxes are alive.
    pub fn len(&self) -> usize {
        self.boxes.len() - self.free.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The box `id` names, if it is alive.
    pub fn get(&self, id: BoxId) -> Option<&LayoutBox<S>> {
        self.boxes
            .get(id.index as usize)?
            .as_ref()
            .filter(|b| b.generation == id.generation)
    }

    /// The inline boxes a block-level box `id` was split out of (innermost first; empty for most).
    pub(crate) fn split_inline_ancestors(&self, id: BoxId) -> &[BoxId] {
        if self.split_inlines.is_empty() {
            return &[];
        }
        self.split_inlines.get(&id).map_or(&[], |v| v.as_slice())
    }

    /// The node's principal box (CSS Display 3 §2.1).
    pub fn box_of(&self, node: NodeId) -> Option<BoxId> {
        match self.node_boxes.get(node.index() as usize) {
            Some(Some((n, b))) if *n == node && self.get(*b).is_some() => Some(*b),
            _ => None,
        }
    }

    #[inline]
    pub(crate) fn b(&self, id: BoxId) -> &LayoutBox<S> {
        let b = self.boxes[id.index as usize].as_ref().expect("dead box id");
        debug_assert_eq!(b.generation, id.generation, "stale box id");
        b
    }

    #[inline]
    pub(crate) fn b_mut(&mut self, id: BoxId) -> &mut LayoutBox<S> {
        let b = self.boxes[id.index as usize].as_mut().expect("dead box id");
        debug_assert_eq!(b.generation, id.generation, "stale box id");
        b
    }

    pub(crate) fn set_node_box(&mut self, node: NodeId, id: BoxId) {
        let i = node.index() as usize;
        if self.node_boxes.len() <= i {
            self.node_boxes.resize(i + 1, None);
        }
        self.node_boxes[i] = Some((node, id));
    }

    pub(crate) fn clear_node_box(&mut self, node: NodeId, id: BoxId) {
        if let Some(slot) = self.node_boxes.get_mut(node.index() as usize) {
            if *slot == Some((node, id)) {
                *slot = None;
            }
        }
    }

    pub(crate) fn alloc(
        &mut self,
        kind: BoxKind,
        level: Level,
        style: S,
        node: Option<NodeId>,
        data: BoxData,
    ) -> BoxId {
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                self.boxes.push(None);
                self.generations.push(NonZeroU32::MIN);
                (self.boxes.len() - 1) as u32
            }
        };
        let generation = self.generations[index as usize];
        self.allocated += 1;
        self.dirty.push(BoxId { index, generation });
        self.boxes[index as usize] = Some(LayoutBox {
            generation,
            kind,
            level,
            // Layout reads the box's properties in its own flow frame (`crate::logical`).
            style: style.own_view(),
            node,
            pseudo: None,
            pseudo_of: None,
            parent: None,
            children: Vec::new(),
            flags: flags::DIRTY,
            order: 0,
            data,
            marker: None,
            decorations: Decorations::new(),
            cache: Default::default(),
        });
        BoxId { index, generation }
    }

    /// Frees `id` and its descendants (and markers).
    pub(crate) fn free_subtree(&mut self, id: BoxId) {
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            let Some(b) = self.boxes[id.index as usize].take() else {
                continue;
            };
            if b.generation != id.generation {
                self.boxes[id.index as usize] = Some(b);
                continue;
            }
            if let Some(n) = b.node {
                self.clear_node_box(n, id);
            }
            if !self.split_inlines.is_empty() {
                self.split_inlines.remove(&id);
            }
            if !self.split_owners.is_empty() {
                self.split_owners.remove(&id);
            }
            if !self.split_wrappers.is_empty() {
                self.split_wrappers.remove(&id);
            }
            if !self.first_line.is_empty() {
                self.first_line.remove(&id);
            }
            stack.extend(b.children.iter().copied());
            stack.extend(b.marker);
            let g = &mut self.generations[id.index as usize];
            *g = g.checked_add(1).unwrap_or(NonZeroU32::MIN);
            self.free.push(id.index);
        }
    }

    /// Frees `id`'s descendants, keeping `id`.
    pub(crate) fn free_descendants(&mut self, id: BoxId) {
        let children = std::mem::take(&mut self.b_mut(id).children);
        let marker = self.b_mut(id).marker.take();
        for c in children.into_iter().chain(marker) {
            self.free_subtree(c);
        }
    }

    pub(crate) fn set_children(&mut self, parent: BoxId, children: Vec<BoxId>) {
        let view = self.b(parent).style.view();
        // (`sideways-rl` shares `vertical-rl`'s frame)
        let mode = view.swaps().then(|| self.b(parent).style.writing_mode());
        for &c in &children {
            let b = self.b_mut(c);
            b.parent = Some(parent);
            // CSS Writing Modes 4 §3.2: a block container whose writing mode differs from its
            // parent's establishes an independent formatting context.
            if b.kind == BoxKind::Block
                && (b.style.view() != view || mode.is_some_and(|m| b.style.writing_mode() != m))
            {
                b.flags |= flags::INDEPENDENT;
            }
        }
        self.b_mut(parent).children = children;
    }

    /// A new style handle for box `id`, read in the box's flow frame: its own writing mode's, or
    /// for a table part (see [`BoxTree::table_parts_in_table_frame`]) its parent's.
    pub(crate) fn framed(&self, id: BoxId, style: S) -> S {
        let b = self.b(id);
        let table_part = matches!(
            b.kind,
            BoxKind::TableRowGroup(_)
                | BoxKind::TableRow
                | BoxKind::TableColumnGroup
                | BoxKind::TableColumn
        );
        match b.parent {
            Some(p) if table_part => style.with_view(self.b(p).style.view()),
            _ => style.own_view(),
        }
    }

    /// CSS Writing Modes 4 §3.2: `writing-mode` does not apply to table rows, row groups, columns
    /// and column groups; they are read in their table's frame (their logical properties were
    /// already mapped by their own writing mode in the cascade).
    pub(crate) fn table_parts_in_table_frame(&mut self, table: BoxId) {
        let view = self.b(table).style.view();
        let mut stack = self.b(table).children.clone();
        while let Some(c) = stack.pop() {
            let b = self.b_mut(c);
            if !matches!(
                b.kind,
                BoxKind::TableRowGroup(_)
                    | BoxKind::TableRow
                    | BoxKind::TableColumnGroup
                    | BoxKind::TableColumn
            ) {
                continue;
            }
            if b.style.view() != view {
                b.style = b.style.with_view(view);
            }
            stack.extend(b.children.iter().copied());
        }
    }

    /// Marks `id` as needing layout, with its ancestors (whose layout depends on it).
    pub fn mark_dirty(&mut self, id: BoxId) {
        let mut cur = Some(id);
        while let Some(c) = cur {
            let b = self.b_mut(c);
            let was = b.flags & flags::DIRTY != 0;
            b.flags |= flags::DIRTY;
            b.cache.invalidate();
            cur = b.parent;
            if !was {
                self.dirty.push(c);
            }
        }
    }

    /// The number ([`LayoutBox::order`]) of the last box of `id`'s subtree in pre-order.
    pub(crate) fn subtree_end(&self, id: BoxId) -> u32 {
        let mut cur = id;
        loop {
            let b = self.b(cur);
            match b.children.last().copied().or(b.marker) {
                Some(next) => cur = next,
                None => return b.order,
            }
        }
    }

    /// The boxes in pre-order (markers before their list item's children).
    pub(crate) fn preorder(&self) -> Vec<BoxId> {
        match self.root {
            Some(root) => self.preorder_from(root),
            None => Vec::new(),
        }
    }

    /// `root`'s subtree in pre-order, `root` first.
    fn preorder_from(&self, root: BoxId) -> Vec<BoxId> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            out.push(id);
            let b = self.b(id);
            stack.extend(b.children.iter().rev().copied());
            stack.extend(b.marker);
        }
        out
    }

    /// Numbers boxes in pre-order, [`ORDER_GAP`] apart so that later insertions find room between
    /// their neighbours ([`BoxTree::renumber_incremental`]).
    pub(crate) fn renumber(&mut self) {
        for (i, id) in self.preorder().into_iter().enumerate() {
            self.b_mut(id).order = (i as u32).saturating_add(1).saturating_mul(ORDER_GAP);
        }
    }

    /// [`BoxTree::renumber`] after an incremental change, keeping the numbers that are still in
    /// pre-order and giving the boxes between them numbers in the gaps (order maintenance): inserting
    /// boxes no longer shifts every later number. A clean box whose number did change and whose
    /// fragment carries it into painting order (positioned boxes, stacking contexts) is marked dirty,
    /// so no cached fragment keeps a stale number. Without room in a gap, everything is numbered
    /// again.
    pub(crate) fn renumber_incremental(&mut self) {
        let ids = self.preorder();
        let numbers = self.gap_numbers(&ids, 0, None).unwrap_or_else(|| {
            (1..=ids.len() as u32)
                .map(|n| n.saturating_mul(ORDER_GAP))
                .collect()
        });
        self.apply_numbers(&ids, &numbers);
    }

    /// [`BoxTree::renumber_incremental`] for the descendants of `roots` only: the containers an
    /// incremental update regenerated, none inside another, when nothing outside their subtrees
    /// changed. A subtree's boxes get numbers between its container's and that of the first box
    /// after it, so the walk is the size of what changed, not of the document. Without room there,
    /// everything is numbered again.
    pub(crate) fn renumber_within(&mut self, roots: &[BoxId]) {
        let mut plan = Vec::with_capacity(roots.len());
        for &root in roots {
            if self.get(root).is_none() {
                continue;
            }
            let lo = self.b(root).order;
            let hi = self.next_after_subtree(root).map(|n| self.b(n).order);
            let mut ids = self.preorder_from(root);
            ids.remove(0);
            let numbers = if lo == 0 || hi.is_some_and(|h| h <= lo) {
                None
            } else {
                self.gap_numbers(&ids, lo, hi)
            };
            match numbers {
                Some(numbers) => plan.push((ids, numbers)),
                None => return self.renumber_incremental(),
            }
        }
        for (ids, numbers) in plan {
            self.apply_numbers(&ids, &numbers);
        }
        #[cfg(test)]
        {
            let ids = self.preorder();
            assert!(
                ids.windows(2)
                    .all(|w| self.b(w[0]).order < self.b(w[1]).order),
                "box numbers out of pre-order:\n{}",
                self.dump()
            );
        }
    }

    /// The first box after `id`'s subtree in pre-order.
    fn next_after_subtree(&self, id: BoxId) -> Option<BoxId> {
        let mut cur = id;
        loop {
            let parent = self.b(cur).parent?;
            let p = self.b(parent);
            let next = if p.marker == Some(cur) {
                p.children.first().copied()
            } else {
                p.children
                    .iter()
                    .position(|&c| c == cur)
                    .and_then(|i| p.children.get(i + 1).copied())
            };
            if next.is_some() {
                return next;
            }
            cur = parent;
        }
    }

    /// Numbers for `ids` (consecutive in pre-order, after a box numbered `lo` and before one
    /// numbered `hi`, if any): a box keeps its number when it is above the last kept one and below
    /// `hi` (new boxes have 0), and the others get numbers in the gaps (order maintenance).
    /// `None` when a gap has no room.
    fn gap_numbers(&self, ids: &[BoxId], lo: u32, hi: Option<u32>) -> Option<Vec<u32>> {
        let mut keep = vec![false; ids.len()];
        let mut last = lo;
        for (i, &id) in ids.iter().enumerate() {
            let o = self.b(id).order;
            if o > last && hi.is_none_or(|h| o < h) {
                keep[i] = true;
                last = o;
            }
        }
        let mut numbers: Vec<u32> = ids.iter().map(|&id| self.b(id).order).collect();
        let mut i = 0;
        while i < ids.len() {
            if keep[i] {
                i += 1;
                continue;
            }
            let start = i;
            while i < ids.len() && !keep[i] {
                i += 1;
            }
            let from = if start == 0 { lo } else { numbers[start - 1] } as u64;
            let run = (i - start) as u64;
            let to = if i < ids.len() {
                numbers[i] as u64
            } else {
                match hi {
                    Some(h) => h as u64,
                    None => from + (run + 1) * ORDER_GAP as u64,
                }
            };
            let step = (to - from) / (run + 1);
            if step == 0 || to > u32::MAX as u64 {
                return None;
            }
            for k in 0..run {
                numbers[start + k as usize] = (from + (k + 1) * step) as u32;
            }
        }
        Some(numbers)
    }

    /// Gives `ids` their `numbers`; a clean box whose number changed and whose fragment carries
    /// it into painting order is marked dirty.
    fn apply_numbers(&mut self, ids: &[BoxId], numbers: &[u32]) {
        let mut moved = Vec::new();
        for (&id, &n) in ids.iter().zip(numbers) {
            let b = self.b_mut(id);
            if b.order != n {
                if b.order != 0 && !b.is_dirty() && order_matters(b) {
                    moved.push(id);
                }
                b.order = n;
            }
        }
        for id in moved {
            self.mark_dirty(id);
        }
    }

    /// Clears every box's dirty flag (after a layout).
    pub(crate) fn clear_dirty(&mut self) {
        let dirty = std::mem::take(&mut self.dirty);
        if dirty.len() >= self.boxes.len() {
            for b in self.boxes.iter_mut().flatten() {
                b.flags &= !flags::DIRTY;
            }
        } else {
            for id in &dirty {
                if let Some(Some(b)) = self.boxes.get_mut(id.index as usize) {
                    if b.generation == id.generation {
                        b.flags &= !flags::DIRTY;
                    }
                }
            }
        }
        self.dirty = dirty;
        self.dirty.clear();
    }

    /// A text description of the subtree (kind, node, level, flags), for tests and debugging.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        if let Some(root) = self.root {
            self.dump_box(root, 0, &mut out);
        }
        out
    }

    fn dump_box(&self, id: BoxId, depth: usize, out: &mut String) {
        use std::fmt::Write;
        let b = self.b(id);
        let _ = write!(out, "{:indent$}{:?}", "", b.kind, indent = depth * 2);
        if b.level == Level::Inline && b.kind != BoxKind::Inline && b.kind != BoxKind::Text {
            out.push_str(" inline-level");
        }
        if b.is_anonymous() {
            out.push_str(" anon");
        }
        if b.has_inline_content() {
            out.push_str(" ifc");
        }
        if b.is_float() {
            out.push_str(" float");
        }
        if b.is_out_of_flow() {
            out.push_str(" oof");
        }
        if let Some(p) = b.pseudo {
            let _ = write!(out, " ::{p:?}");
        }
        if let Some(t) = b.text() {
            let _ = write!(out, " {t:?}");
        }
        out.push('\n');
        if let Some(m) = b.marker {
            self.dump_box(m, depth + 1, out);
        }
        for &c in &b.children {
            self.dump_box(c, depth + 1, out);
        }
    }
}
