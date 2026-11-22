// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snowlark layout: from the styled DOM to an immutable fragment tree, and from fragments to a display
//! list.
//!
//! - [`style`]: the [`Style`] trait (the computed values layout reads; `snowlark-render` implements it over
//!   stylo) and [`StyleSource`] (element, pseudo-element and anonymous box styles, replaced content).
//! - [`LayoutTree`]: the box tree (CSS Display 3 box generation with anonymous boxes, tables, list
//!   markers, `display: contents`), rebuilt incrementally, and layout passes producing a
//!   [`Fragment`] tree. Block, inline, table and positioned layout are ours; flex and grid go through
//!   `taffy` with our layout as the leaf measure. Each box caches its last result keyed by its
//!   constraint space, so a relayout after a change reuses every clean subtree's fragments.
//! - [`paint`]: fragments to a `snowlark_paint::DisplayListBuilder` in CSS 2 Appendix E order;
//!   [`hit_test`] in the same order.
//! - [`testing`]: a plain-struct [`Style`] and a DOM-plus-styles builder for tests and demos.
//!
//! Text goes through the paint crate's `TextEngine` (DirectWrite shaping, clusters with break
//! opportunities and bidi levels); layout measures and paints the same glyphs.

mod block;
mod boxes;
mod build;
mod cache;
pub mod clip;
pub mod controls;
pub mod find;
mod flex_grid;
mod floats;
mod flow;
mod fragment;
mod incremental;
mod inline;
mod intrinsic;
mod layout;
pub mod logical;
mod lookup;
pub mod media;
pub(crate) mod multicol;
mod paint;
mod positioned;
mod replaced;
pub mod scroll_snap;
mod sizing;
pub mod style;
pub mod svg;
mod table;
mod table_heights;
mod table_widths;
pub mod testing;

use std::collections::HashMap;
use std::sync::Arc;

use snowlark_dom::{Dom, NodeId};
use snowlark_paint::{DisplayListBuilder, Point, Rect, Size, TextEngine};

pub use boxes::{BoxId, BoxKind, BoxTree, LayoutBox, Level, RowGroup};
pub use fragment::{
    fragment_flags, BoxContent, BoxFragment, CollapsedBorderSegment, Fragment, FragmentChild,
    FragmentKind, TextFragment,
};
pub use incremental::{BoxDamage, ElementChange, TreeChanges, UpdateStats};
pub use layout::LayoutStats;
pub use paint::{HitTestResult, SelectedText, TextSelection};
pub use style::{Style, StyleSource};
pub use testing::{TestDocument, TestStyle, TestStyles};

use crate::build::Builder;
use crate::fragment::add_overflow;
use crate::layout::{Cx, Space};

/// A document's layout: its box tree and the last fragment tree.
pub struct LayoutTree<S: Style> {
    tree: BoxTree<S>,
    pass: u32,
    stats: LayoutStats,
    root: Option<Arc<Fragment<S>>>,
    /// The top layer the tree was built with.
    top_layer: Vec<NodeId>,
    /// The document's fullscreen element, as of the last build or update.
    fullscreen: Option<NodeId>,
    /// The viewport of the last layout.
    viewport: Option<Size>,
    /// Finding nodes' fragments in `root`.
    lookup: lookup::Lookup<S>,
    /// Boxes given a new style for painting only during an update (`BoxDamage::Paint`), whose
    /// cached fragments take it at the update's end.
    painted: Vec<BoxId>,
}

impl<S: Style> Default for LayoutTree<S> {
    fn default() -> Self {
        LayoutTree::new()
    }
}

impl<S: Style> LayoutTree<S> {
    pub fn new() -> LayoutTree<S> {
        LayoutTree {
            tree: BoxTree::new(),
            pass: 0,
            stats: LayoutStats::default(),
            root: None,
            top_layer: Vec::new(),
            fullscreen: None,
            viewport: None,
            lookup: Default::default(),
            painted: Vec::new(),
        }
    }

    /// The box tree.
    pub fn boxes(&self) -> &BoxTree<S> {
        &self.tree
    }

    /// The `background-color` of the canvas background (CSS Backgrounds 3 §2.11.2: the root
    /// element's background, or the `<body>`'s when the root has none), as of the last box tree
    /// build; `None` before one, or with no root box.
    pub fn canvas_background_color(&self) -> Option<snowlark_paint::Color> {
        let source = self.tree.canvas_background?;
        Some(self.tree.get(source)?.style.background_color())
    }

    /// The `overflow-x` and `overflow-y` that apply to the viewport (CSS Overflow 3 §3.3: the root
    /// element's, or the `<body>`'s when the root's is `visible`), as of the last box tree build.
    pub fn viewport_overflow(&self) -> Option<(style::Overflow, style::Overflow)> {
        let root = self.tree.root?;
        // (Fullscreen API: an element other than the root fullscreen covers the viewport, which
        // shows no scrollbar beside it then)
        if self
            .fullscreen
            .is_some_and(|e| self.tree.b(root).node != Some(e))
        {
            return Some((style::Overflow::Hidden, style::Overflow::Hidden));
        }
        let id = self.tree.viewport_overflow.unwrap_or(root);
        let s = &self.tree.b(id).style;
        Some((s.phys_overflow_x(), s.phys_overflow_y()))
    }

    /// The principal writing mode and direction (CSS Writing Modes 4 §8), as of the last box tree
    /// build: what the viewport scrolls by.
    pub fn principal_writing_mode(&self) -> Option<(style::WritingMode, style::Direction)> {
        let id = self.tree.principal.or(self.tree.root)?;
        let s = &self.tree.get(id)?.style;
        Some((s.writing_mode(), s.direction()))
    }

    /// Shapes this tree's text through `cache` (shared with other trees) instead of a cache of its
    /// own: see [`snowlark_paint::text_cache`] for what may share one.
    pub fn set_text_cache(
        &mut self,
        cache: std::sync::Arc<std::sync::Mutex<snowlark_paint::TextCache>>,
    ) {
        self.tree.text_cache = cache;
    }

    /// The word cache's statistics (cumulative, over every tree sharing it).
    pub fn word_cache_stats(&self) -> snowlark_paint::WordCacheStats {
        self.tree
            .text_cache
            .lock()
            .map(|c| c.words.stats())
            .unwrap_or_default()
    }

    /// Builds the box tree for `document` from scratch.
    pub fn build<Src: StyleSource<Style = S>>(
        &mut self,
        dom: &Dom,
        document: NodeId,
        styles: &Src,
    ) {
        Builder::new(&mut self.tree, dom, styles).build_document(document);
        self.painted.clear();
        self.top_layer = styles.top_layer().to_vec();
        self.fullscreen = styles.fullscreen_element();
    }

    /// A text node's data changed. Returns `false` when the node has no box (its text was white space
    /// dropped between blocks), or when it becomes or ceases to be such white space: call
    /// [`LayoutTree::children_changed`] on its parent instead.
    pub fn text_changed(&mut self, dom: &Dom, node: NodeId) -> bool {
        let Some(id) = self.tree.box_of(node) else {
            return false;
        };
        let Some(text) = dom.character_data(node) else {
            return false;
        };
        let b = self.tree.b(id);
        if b.kind != BoxKind::Text {
            return false;
        }
        // Text becoming (or ceasing to be) white space can change whether it has a box and which
        // anonymous boxes exist: collapsible white space generates nothing among blocks (CSS 2
        // §9.2.1.1, CSS Text 3 §4.1.1), and a run of any white space generates no flex or grid
        // item (css-flexbox-1 §4, css-grid-2 §6). That is a structural change, whatever box holds it.
        let collapsible = |s: &str| build::is_collapsible_whitespace(&b.style, s);
        let white_space = |s: &str| s.bytes().all(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'));
        let old = b.text().unwrap_or("");
        if collapsible(old) != collapsible(text) || white_space(old) != white_space(text) {
            return false;
        }
        let b = self.tree.b_mut(id);
        b.data = boxes_text(text);
        // The inline formatting context holding it must be prepared again (reusing unchanged shaping).
        let mut owner = b.parent;
        while let Some(o) = owner {
            let ob = self.tree.b_mut(o);
            if ob.kind != BoxKind::Inline {
                ob.cache.inline_stale = true;
                break;
            }
            owner = ob.parent;
        }
        self.tree.mark_dirty(id);
        true
    }

    /// `parent`'s children changed (insertions, removals): its boxes are regenerated from the
    /// nearest ancestor block container (or flex, grid or table) that owns them.
    pub fn children_changed<Src: StyleSource<Style = S>>(
        &mut self,
        dom: &Dom,
        parent: NodeId,
        styles: &Src,
    ) {
        self.rebuild_from(dom, Some(parent), styles);
    }

    /// `element`'s computed style changed: its parent's boxes are regenerated (its own box kind, its
    /// level and the anonymous boxes around it may change).
    pub fn style_changed<Src: StyleSource<Style = S>>(
        &mut self,
        dom: &Dom,
        element: NodeId,
        styles: &Src,
    ) {
        let parent = dom.parent_element(element);
        if parent.is_none() {
            let doc = dom.node_document(element);
            self.build(dom, doc, styles);
            return;
        }
        self.rebuild_from(dom, parent, styles);
    }

    fn rebuild_from<Src: StyleSource<Style = S>>(
        &mut self,
        dom: &Dom,
        start: Option<NodeId>,
        styles: &Src,
    ) {
        let mut cur = start;
        while let Some(n) = cur {
            if let Some(b) = self.tree.box_of(n) {
                let lb = self.tree.b(b);
                let ok = matches!(
                    lb.kind,
                    BoxKind::Block
                        | BoxKind::TableCell
                        | BoxKind::TableCaption
                        | BoxKind::Flex
                        | BoxKind::Grid
                        | BoxKind::TableWrapper
                ) && lb.pseudo.is_none();
                if ok {
                    Builder::new(&mut self.tree, dom, styles).rebuild_contents(b);
                    self.tree.renumber_incremental();
                    return;
                }
            }
            cur = dom.parent_element(n);
        }
        let doc = start.map(|s| dom.node_document(s));
        if let Some(doc) = doc {
            self.build(dom, doc, styles);
        }
    }

    /// Lays out the tree in a viewport (the initial containing block), reusing clean boxes'
    /// fragments. Returns the root fragment: the initial containing block, holding the root element's
    /// fragment and fixed/absolute boxes positioned against it.
    pub fn layout(&mut self, text: &TextEngine, viewport: Size) -> Arc<Fragment<S>> {
        // Nothing changed since the last layout in this viewport: its fragments stand (every change
        // marks its box and the box's ancestors dirty, so a clean root means a clean tree).
        if let (Some(root), Some(f)) = (self.tree.root, &self.root) {
            if self.viewport == Some(viewport) && !self.tree.b(root).is_dirty() {
                self.stats = LayoutStats::default();
                return f.clone();
            }
        }
        if self.viewport != Some(viewport) {
            for id in std::mem::take(&mut self.tree.icb_dependents) {
                if self.tree.get(id).is_some() {
                    self.tree.mark_dirty(id);
                }
            }
        }
        self.viewport = Some(viewport);
        self.lookup.reset();
        self.pass = self.pass.wrapping_add(1).max(1);
        let Some(root) = self.tree.root else {
            let f = Arc::new(Fragment {
                size: viewport,
                kind: FragmentKind::Line { baseline: 0.0 },
                children: Vec::new(),
                overflow: Rect::new(0.0, 0.0, viewport.width, viewport.height),
                paint_bounds: Rect::new(0.0, 0.0, viewport.width, viewport.height),
                inflow: None,
                baseline: None,
                last_baseline: None,
            });
            self.root = Some(f.clone());
            return f;
        };
        // The initial containing block takes the root's writing mode (CSS Writing Modes 4 §8): the
        // root is laid out in its own frame and the ICB's children are mapped to physical
        // coordinates at the end.
        let frame = self.tree.b(root).style.view();
        let mut cx = Cx {
            tree: &mut self.tree,
            text,
            pass: self.pass,
            stats: LayoutStats::default(),
            clamp: None,
            clipped_inlines: false,
            striped_inlines: false,
            pct_height_basis: None,
            frame,
            icb: viewport,
            measuring_across: false,
            column_flow: false,
            nested: Vec::new(),
            nested_path: Vec::new(),
        };
        let icb_size = frame.inverse().size(viewport);
        let space = Space::fill(icb_size.width, Some(icb_size.height));
        let result = cx.layout(root, &space, None, Point::ZERO);
        let mut children = vec![FragmentChild {
            offset: Point::new(result.margin.left, result.margin.top),
            fragment: result.fragment.clone(),
            stacking_owner: 0,
        }];
        let icb = Rect::new(0.0, 0.0, icb_size.width, icb_size.height);
        let mut pending: Vec<_> = result
            .oof
            .iter()
            .map(|o| o.shifted(result.margin.left, result.margin.top))
            .collect();
        // Boxes whose containing block is the initial containing block (and what they pass on).
        let mut guard = 0;
        while !pending.is_empty() && guard < 64 {
            guard += 1;
            let mut next = Vec::new();
            for o in pending {
                let (child, more) = positioned_at_icb(&mut cx, o, icb);
                children.push(child);
                next.extend(more);
            }
            pending = next;
        }
        self.stats = cx.stats;
        if !frame.is_identity() {
            for c in &mut children {
                let r = frame.rect(
                    Rect::new(
                        c.offset.x,
                        c.offset.y,
                        c.fragment.size.width,
                        c.fragment.size.height,
                    ),
                    icb_size,
                );
                c.offset = Point::new(r.x, r.y);
                c.fragment = flow::map_fragment(&c.fragment, frame, logical::Flip::IDENTITY);
            }
        }
        let icb = Rect::new(0.0, 0.0, viewport.width, viewport.height);
        let mut overflow = icb;
        let mut bounds = overflow;
        for c in &children {
            // (the initial containing block's boxes are all its rectangle)
            if let Some(child) = c.fragment.as_box() {
                if child.style.transform_needs_containing() {
                    let outer = icb.translate(-c.offset.x, -c.offset.y);
                    child.containing.set(style::ContainingBoxes {
                        border: outer,
                        padding: outer,
                        content: outer,
                        radii: [(0.0, 0.0); 4],
                    });
                }
            }
            add_overflow(&mut overflow, &mut bounds, c);
        }
        let root_style = self.tree.b(root).style.with_view(logical::Flip::IDENTITY);
        let f = Arc::new(Fragment {
            size: viewport,
            kind: FragmentKind::Box(BoxFragment {
                style: root_style,
                box_id: Some(root),
                node: None,
                pseudo: None,
                border: Default::default(),
                padding: Default::default(),
                flags: fragment_flags::INITIAL_CONTAINING_BLOCK,
                order: 0,
                subtree_end: u32::MAX,
                content: BoxContent::None,
                containing: Default::default(),
                clip_reference: Default::default(),
                strip: Default::default(),
                stitched: None,
            }),
            children,
            overflow,
            paint_bounds: bounds,
            inflow: None,
            baseline: None,
            last_baseline: None,
        });
        self.tree.clear_dirty();
        self.root = Some(f.clone());
        f
    }

    /// The fragment tree of the last layout.
    pub fn fragments(&self) -> Option<&Arc<Fragment<S>>> {
        self.root.as_ref()
    }

    /// Runs `f` with the fragments `node` generated in the last layout, in tree order, each with its
    /// offset in the root fragment's coordinates (`::backdrop` boxes included). The cost is the
    /// node's path through the tree, or O(1) once a burst of reads made an index (see `lookup`).
    pub fn with_fragments<R>(
        &self,
        node: NodeId,
        f: impl FnOnce(&[(Point, &Fragment<S>)]) -> R,
    ) -> R {
        let Some(root) = &self.root else {
            return f(&[]);
        };
        let ancestors = || self.box_ancestor_nodes(node);
        self.lookup.with(root, node, ancestors, self.tree.len(), f)
    }

    /// [`LayoutTree::with_fragments`], each fragment with the matrix from its own coordinates to
    /// the root fragment's (its own transform and its ancestor fragments' applied), at the same
    /// cost.
    fn with_transformed_fragments<R>(
        &self,
        node: NodeId,
        f: impl FnOnce(&[(lookup::Transformed, &Fragment<S>)]) -> R,
    ) -> R {
        let Some(root) = &self.root else {
            return f(&[]);
        };
        let ancestors = || self.box_ancestor_nodes(node);
        self.lookup
            .with_transformed(root, node, ancestors, self.tree.len(), f)
    }

    /// The nodes of the box tree ancestors of `node`'s principal box (`None` when it has none).
    fn box_ancestor_nodes(&self, node: NodeId) -> Option<rustc_hash::FxHashSet<NodeId>> {
        let b = self.tree.box_of(node)?;
        let mut set = rustc_hash::FxHashSet::default();
        let mut cur = self.tree.b(b).parent;
        while let Some(p) = cur {
            let pb = self.tree.b(p);
            if let Some(n) = pb.node {
                set.insert(n);
            }
            cur = pb.parent;
        }
        Some(set)
    }

    /// The fragments the geometry reads since the last layout visited (what they cost: a read
    /// visits its node's path, not the tree).
    pub fn fragments_searched(&self) -> usize {
        self.lookup.searched()
    }

    /// The resolved values of `grid-template-columns` and `grid-template-rows` (CSS Grid 1 §7.2.6)
    /// of `node` when it is a grid container: its used tracks in the last layout.
    pub fn grid_track_lists(&self, node: NodeId) -> Option<(String, String)> {
        let b = self.tree.box_of(node)?;
        let g = self.tree.b(b).cache.taffy.as_ref()?.grid.as_ref()?;
        Some((
            flex_grid::resolved_track_list(&g.columns),
            flex_grid::resolved_track_list(&g.rows),
        ))
    }

    /// The border, padding and content boxes of `node`'s first box fragment in the last layout
    /// (what most CSSOM reads need: no pass over its children for the scrollable overflow).
    pub fn box_rects(&self, node: NodeId) -> Option<BoxRects> {
        self.with_fragments(node, |found| {
            found
                .iter()
                .find(|(_, f)| f.as_box().is_some() && !lookup::is_backdrop(f))
                .map(|(at, f)| {
                    let t = |r: Rect| r.translate(at.x, at.y);
                    BoxRects {
                        border_box: t(f.border_rect()),
                        padding_box: t(f.padding_rect()),
                        content_box: t(f.content_rect()),
                    }
                })
        })
    }

    /// The resizer of `node`'s first box fragment (CSS UI 4 §5.1) in the root fragment's
    /// coordinates, unscrolled: `None` when the user cannot resize its box.
    pub fn resizer(&self, node: NodeId) -> Option<Resizer> {
        self.with_fragments(node, |found| {
            let (at, f) = found
                .iter()
                .find(|(_, f)| f.as_box().is_some() && !lookup::is_backdrop(f))?;
            let axes = controls::resizer_axes(f)?;
            let t = |r: Rect| r.translate(at.x, at.y);
            Some(Resizer {
                axes,
                corner: t(controls::resizer_rect(f)),
                border_box: t(f.border_rect()),
                content_box: t(f.content_rect()),
                border_box_sizing: f.as_box()?.style.box_sizing() == style::BoxSizing::BorderBox,
            })
        })
    }

    /// The geometry of `node`'s first box fragment in the last layout.
    /// Runs `f` with the first box fragment `node` generated in the last layout (not its
    /// `::backdrop`), at the cost of [`LayoutTree::with_fragments`]: what
    /// [`Fragment::find`] on the root finds, without searching the whole tree.
    pub fn with_box_fragment<R>(
        &self,
        node: NodeId,
        f: impl FnOnce(&Fragment<S>) -> R,
    ) -> Option<R> {
        self.with_fragments(node, |found| {
            found
                .iter()
                .find(|(_, fr)| fr.as_box().is_some() && !lookup::is_backdrop(fr))
                .map(|(_, fr)| f(fr))
        })
    }

    pub fn geometry(&self, node: NodeId) -> Option<BoxGeometry> {
        self.with_fragments(node, |found| {
            found
                .iter()
                .find(|(_, f)| f.as_box().is_some() && !lookup::is_backdrop(f))
                .map(|(at, f)| BoxGeometry::of(f, *at))
        })
    }

    /// How far sticky positioning moves `node`'s first box fragment (its own offset and its sticky
    /// ancestors') with the viewport (`viewport` CSS px) scrolled by `viewport_scroll` and scroll
    /// containers by `scroll`: geometry APIs see sticky boxes at their offset position
    /// (css-position-3 §3.4.1). Zero when no sticky box is involved.
    pub fn sticky_offset(
        &self,
        node: NodeId,
        viewport: Size,
        viewport_scroll: Point,
        scroll: &std::collections::HashMap<NodeId, Point>,
    ) -> Point {
        let Some(id) = self.tree.box_of(node) else {
            return Point::ZERO;
        };
        let mut cur = Some(id);
        let mut sticky = false;
        while let Some(c) = cur {
            let b = self.tree.b(c);
            if b.style.position() == style::Position::Sticky {
                sticky = true;
                break;
            }
            cur = b.parent;
        }
        let Some(root) = self.root.as_ref().filter(|_| sticky) else {
            return Point::ZERO;
        };
        let order = self.tree.b(id).order;
        paint::sticky_shift(root, order, viewport, viewport_scroll, scroll).unwrap_or(Point::ZERO)
    }

    /// The border boxes of every fragment `node` generated (`Element.getClientRects()`), or of its
    /// text fragments for a text node, in the root fragment's coordinates. An inline element split
    /// around a block (CSS 2 §9.2.1.1) also reports that block's border box, between its own pieces,
    /// as browsers do.
    pub fn client_rects(&self, node: NodeId) -> Vec<Rect> {
        self.fragment_rects(node, true)
    }

    /// [`LayoutTree::client_rects`] without transforms: the layout boxes (`offsetWidth` and
    /// `offsetHeight` measure those, CSSOM View §7).
    pub fn layout_rects(&self, node: NodeId) -> Vec<Rect> {
        self.fragment_rects(node, false)
    }

    /// The padding box of `node`'s first box fragment with the transforms of the box and its
    /// ancestors applied (the rectangle bounding its transformed corners), in the root fragment's
    /// coordinates: where its content clip is on screen, as [`LayoutTree::client_rects`] places
    /// its border box. `None` when it has no box fragment.
    pub fn transformed_padding_box(&self, node: NodeId) -> Option<Rect> {
        if !self.has_transformed_path(node) {
            return self.box_rects(node).map(|b| b.padding_box);
        }
        self.with_transformed_fragments(node, |found| {
            found
                .iter()
                .find(|(_, f)| f.as_box().is_some() && !lookup::is_backdrop(f))
                .map(|(p, f)| {
                    let own = f.padding_rect();
                    if p.any {
                        p.m.apply_rect(&own)
                    } else {
                        own.translate(p.m.dx, p.m.dy)
                    }
                })
        })
    }

    /// Whether `node`'s box or one of its box tree ancestors has a transform (the fragments of a
    /// box lie under its box tree ancestors' only).
    fn has_transformed_path(&self, node: NodeId) -> bool {
        let tree = &self.tree;
        let mut cur = tree.box_of(node);
        while let Some(c) = cur {
            let b = tree.b(c);
            if b.style.has_transform() {
                return true;
            }
            cur = b.parent;
        }
        false
    }

    fn fragment_rects(&self, node: NodeId, transforms: bool) -> Vec<Rect> {
        let tree = &self.tree;
        let split = !tree.split_owners.is_empty()
            && tree
                .split_owners
                .values()
                .any(|owners| owners.contains(&node));
        // (CSSOM View §6.1: with the transforms of the box and its ancestors applied, a box
        // becomes the rectangle bounding its transformed corners)
        let transformed = transforms && self.has_transformed_path(node);
        if !split && !transformed {
            return self.with_fragments(node, |found| {
                found
                    .iter()
                    .filter(|(_, f)| !lookup::is_backdrop(f))
                    .map(|(at, f)| Rect::new(at.x, at.y, f.size.width, f.size.height))
                    .collect()
            });
        }
        if !split {
            // (the transforms composed along the node's path, as Blink's MapLocalToAncestor climbs
            // the container chain: see `lookup`)
            return self.with_transformed_fragments(node, |found| {
                found
                    .iter()
                    .filter(|(_, f)| !lookup::is_backdrop(f))
                    .map(|(p, f)| {
                        let own = Rect::new(0.0, 0.0, f.size.width, f.size.height);
                        if p.any {
                            p.m.apply_rect(&own)
                        } else {
                            own.translate(p.m.dx, p.m.dy)
                        }
                    })
                    .collect()
            });
        }
        // (an inline split around blocks, rare: a walk of the tree keeps the pieces and the blocks
        // in tree order and applies the transforms)
        let mut out = Vec::new();
        let split_block = |n: Option<NodeId>| {
            split
                && n.and_then(|n| tree.box_of(n))
                    .and_then(|b| tree.split_owners.get(&b))
                    .is_some_and(|owners| owners.contains(&node))
        };
        // (`visit` gets the fragment's map with its own transform, and the map of its place in its
        // parent, without it)
        fn walk<S: Style>(
            f: &Fragment<S>,
            m: snowlark_paint::Matrix,
            transformed: bool,
            placed: (snowlark_paint::Matrix, bool),
            transforms: bool,
            visit: &mut impl FnMut(&Fragment<S>, &dyn Fn(Rect) -> Rect, &dyn Fn(Rect) -> Rect),
        ) {
            let mapper = |m: snowlark_paint::Matrix, transformed: bool| {
                move |r: Rect| {
                    if transformed {
                        m.apply_rect(&r)
                    } else {
                        r.translate(m.dx, m.dy)
                    }
                }
            };
            visit(f, &mapper(m, transformed), &mapper(placed.0, placed.1));
            for c in &f.children {
                let offset = snowlark_paint::Matrix::translation(c.offset.x, c.offset.y);
                let local = c
                    .fragment
                    .as_box()
                    .filter(|_| transforms)
                    .and_then(|b| b.used_transform(c.fragment.size));
                let placed = (offset.then(&m), transformed);
                match local {
                    Some(t) => walk(
                        &c.fragment,
                        t.then(&offset).then(&m),
                        true,
                        placed,
                        transforms,
                        visit,
                    ),
                    None => walk(
                        &c.fragment,
                        placed.0,
                        transformed,
                        placed,
                        transforms,
                        visit,
                    ),
                }
            }
        }
        if let Some(root) = &self.root {
            let mut walked = 0;
            walk(
                root,
                snowlark_paint::Matrix::IDENTITY,
                false,
                (snowlark_paint::Matrix::IDENTITY, false),
                transforms,
                &mut |f, map, placed| {
                    walked += 1;
                    let backdrop = lookup::is_backdrop(f);
                    let own = f.node() == Some(node);
                    let block_piece =
                        !own && f.as_box().is_some_and(|b| {
                            b.flags & fragment_flags::INLINE_BOX == 0 && b.pseudo.is_none()
                        }) && split_block(f.node());
                    if own && !backdrop {
                        out.push(map(Rect::new(0.0, 0.0, f.size.width, f.size.height)));
                    } else if block_piece {
                        // (the block's inline-axis margins count too, as in Chrome, whose inline box
                        // holds the block's margin box on its line; the block's own transform is
                        // left out, as for the inline's own pieces: it transforms the block, not
                        // the inline, CSSOM View §6.1)
                        let s = &f.as_box().expect("box fragment").style;
                        let ml = s.margin(style::Side::Left).resolve_or_zero(0.0);
                        let mr = s.margin(style::Side::Right).resolve_or_zero(0.0);
                        out.push(placed(Rect::new(
                            -ml,
                            0.0,
                            f.size.width + ml + mr,
                            f.size.height,
                        )));
                    }
                },
            );
            self.lookup.count(walked);
        }
        out
    }

    /// The rectangles of the part of text node `node` between DOM byte offsets `start` and `end`
    /// in the last layout (CSSOM View §6.2 `Range.getClientRects()`: the part of each text box
    /// within the range), in the root fragment's coordinates. A collapsed range (`start == end`)
    /// gives one zero-width rectangle where it is.
    pub fn text_range_rects(&self, node: NodeId, start: u32, end: u32) -> Vec<Rect> {
        self.with_fragments(node, |found| {
            let mut out = Vec::new();
            for (at, f) in found {
                let Some(t) = f.as_text() else {
                    continue;
                };
                let (lo, hi) = (start.max(t.text_range.start), end.min(t.text_range.end));
                if lo > hi || (lo == hi && start != end) {
                    continue;
                }
                let x0 = t.x_of(t.processed_offset(lo));
                let x1 = t.x_of(t.processed_offset(hi));
                // (in the text's own frame: vertical text runs down or up its fragment)
                let local = t.flip.inverse().size(f.size);
                let r = Rect::new(x0.min(x1), 0.0, (x1 - x0).abs(), local.height);
                out.push(t.flip.rect(r, local).translate(at.x, at.y));
                if start == end {
                    break;
                }
            }
            out
        })
    }

    /// What the last layout pass did.
    pub fn stats(&self) -> LayoutStats {
        self.stats
    }

    /// Marks a node's box as needing layout (for changes layout can't see, like a replaced
    /// element's natural size).
    pub fn mark_dirty(&mut self, node: NodeId) {
        if let Some(b) = self.tree.box_of(node) {
            self.tree.mark_dirty(b);
        }
    }
}

fn boxes_text(t: &str) -> boxes::BoxData {
    boxes::BoxData::Text(t.to_string())
}

fn positioned_at_icb<S: Style>(
    cx: &mut Cx<'_, S>,
    o: layout::Oof,
    icb: Rect,
) -> (FragmentChild<S>, Vec<layout::Oof>) {
    positioned::layout_absolute(cx, o, icb)
}

/// What painting and hit testing need besides the fragments: the viewport and scroll positions.
#[derive(Clone, Copy, Default)]
pub struct PaintOptions<'a> {
    /// The viewport size in CSS px.
    pub viewport: Size,
    /// How far the viewport is scrolled (fixed boxes stay put).
    pub viewport_scroll: Point,
    /// Scroll positions of scroll containers, by their node.
    pub scroll: Option<&'a HashMap<NodeId, Point>>,
    /// Nested documents' display lists (a frame's content, painted in its own viewport
    /// coordinates), by the replaced element that shows them: painted into its content box,
    /// clipped to it.
    pub embeds: Option<&'a HashMap<NodeId, snowlark_paint::DisplayList>>,
    /// The document's selection highlight and editing caret.
    pub selection: Option<&'a TextSelection>,
    /// Find in page: the matches to highlight (behind the text, the selection over them).
    pub find: Option<&'a find::FindHighlights>,
    /// Mark the content a compositor can move on its own as it scrolls the viewport (fixed and
    /// sticky boxes) as layers ([`snowlark_paint::Item::PushLayer`]).
    pub layers: bool,
    /// Device pixels per CSS px (0 counts as 1): overflow of less than a device pixel shows no
    /// scroll bar.
    pub device_scale: f32,
    /// With `layers`, the elements with a transform or opacity animation in effect (Blink's
    /// direct compositing reasons `kActiveTransformAnimation`, `kActiveOpacityAnimation`): their
    /// stacking contexts are marked [`snowlark_paint::LayerKind::Composited`], as are those of
    /// elements whose `will-change` names transform or opacity.
    pub composited: Option<&'a dyn Fn(NodeId) -> bool>,
    /// The transform and opacity animations a compositor can run for a composited element
    /// (`docs/compositor-animations.md`, S2), pushed into the vector: given the element, its
    /// transform reference box (the box's coordinates) and where the box is painted (the list's
    /// coordinates). An element with any always paints its own transform and opacity groups, and
    /// its animations go into the list ([`snowlark_paint::DisplayList::animations`]).
    pub animations: Option<&'a CompositorAnimations<'a>>,
}

/// See [`PaintOptions::animations`].
pub type CompositorAnimations<'a> =
    dyn Fn(NodeId, Rect, Point, &mut Vec<snowlark_paint::layer_animation::LayerAnimation>) + 'a;

impl PaintOptions<'_> {
    /// A viewport with nothing scrolled.
    pub fn viewport(viewport: Size) -> PaintOptions<'static> {
        PaintOptions {
            viewport,
            viewport_scroll: Point::ZERO,
            scroll: None,
            embeds: None,
            selection: None,
            find: None,
            layers: false,
            device_scale: 1.0,
            composited: None,
            animations: None,
        }
    }
}

/// Paints a fragment tree (from [`LayoutTree::layout`]) into `out` in CSS 2 Appendix E order, for a
/// viewport of `viewport` CSS px with nothing scrolled.
pub fn paint<S: Style>(
    root: &Fragment<S>,
    text: &TextEngine,
    viewport: Size,
    out: &mut DisplayListBuilder,
) {
    paint_with(root, text, &PaintOptions::viewport(viewport), out);
}

/// [`paint`] with scroll positions.
pub fn paint_with<S: Style>(
    root: &Fragment<S>,
    text: &TextEngine,
    options: &PaintOptions,
    out: &mut DisplayListBuilder,
) {
    paint_part(root, text, options, None, out)
}

/// [`paint_with`] for the part of the page in `cull` (the root's coordinates, as painted: with the
/// viewport scrolled): subtrees whose paint bounds miss it are left out (their ink overflow, like a
/// shadow, can reach a little farther: the caller leaves a margin). `None` paints everything.
pub fn paint_part<S: Style>(
    root: &Fragment<S>,
    text: &TextEngine,
    options: &PaintOptions,
    cull: Option<Rect>,
    out: &mut DisplayListBuilder,
) {
    let viewport = Rect::new(0.0, 0.0, options.viewport.width, options.viewport.height);
    let canvas = viewport.union(&root.overflow);
    let mut sink = paint::DisplayListSink::new(out, text, canvas);
    sink.cull = cull;
    sink.embeds = options.embeds;
    sink.selection = options.selection;
    sink.find = options.find;
    sink.viewport = viewport;
    let device_scale = if options.device_scale > 0.0 {
        options.device_scale
    } else {
        1.0
    };
    sink.device_scale = device_scale;
    let mut p = paint::Painter {
        sink: &mut sink,
        viewport,
        viewport_scroll: options.viewport_scroll,
        scroll: options.scroll,
        layers: options.layers,
        groups: 0,
        in_layer: false,
        in_fixed: false,
        port: None,
        device_scale,
        sticky_cb: Default::default(),
        composited: options.composited,
        animations: options.animations,
    };
    p.paint_root(root);
}

/// The topmost fragment under `point` (CSS px in the viewport), searched in reverse painting order,
/// honoring clips, transforms, `visibility` and `pointer-events`.
pub fn hit_test<S: Style>(
    root: &Fragment<S>,
    point: Point,
    viewport: Size,
) -> Option<HitTestResult> {
    hit_test_with(root, point, &PaintOptions::viewport(viewport))
}

/// [`hit_test`] with scroll positions.
pub fn hit_test_with<S: Style>(
    root: &Fragment<S>,
    point: Point,
    options: &PaintOptions,
) -> Option<HitTestResult> {
    let mut sink = paint::HitSink::new(point);
    hit_test_into(root, options, &mut sink);
    sink.result()
}

/// Every fragment under `point`, topmost first (CSSOM View `elementsFromPoint()`: the boxes the
/// point is in, in paint order), with the same rules as [`hit_test_with`].
pub fn hit_test_all_with<S: Style>(
    root: &Fragment<S>,
    point: Point,
    options: &PaintOptions,
) -> Vec<HitTestResult> {
    let mut sink = paint::HitSink::collecting_all(point);
    hit_test_into(root, options, &mut sink);
    sink.all_results()
}

fn hit_test_into<S: Style>(root: &Fragment<S>, options: &PaintOptions, sink: &mut paint::HitSink) {
    let mut p = paint::Painter {
        sink,
        viewport: Rect::new(0.0, 0.0, options.viewport.width, options.viewport.height),
        viewport_scroll: options.viewport_scroll,
        scroll: options.scroll,
        layers: false,
        groups: 0,
        in_layer: false,
        in_fixed: false,
        port: None,
        device_scale: 1.0,
        sticky_cb: Default::default(),
        composited: None,
        animations: None,
    };
    p.paint_root(root);
}

/// The caret position for a viewport point (CSS px): the text node and DOM byte offset of the nearest
/// cluster boundary in the text painted on the point's line (else the nearest line), among the text
/// nodes `keep` accepts.
pub fn caret_position<S: Style>(
    root: &Fragment<S>,
    point: Point,
    options: &PaintOptions,
    keep: &dyn Fn(NodeId) -> bool,
) -> Option<(NodeId, u32)> {
    // Two walks: the text on the point's line (fragments elsewhere are skipped), and only when
    // there is none, the nearest line (fragments farther than the best so far are skipped).
    let mut sink = paint::CaretSink::new(paint::CaretQuery::Position { point, keep });
    for line_only in [true, false] {
        sink.line_only = line_only;
        let mut p = paint::Painter {
            sink: &mut sink,
            viewport: Rect::new(0.0, 0.0, options.viewport.width, options.viewport.height),
            viewport_scroll: options.viewport_scroll,
            scroll: options.scroll,
            layers: false,
            groups: 0,
            in_layer: false,
            in_fixed: false,
            port: None,
            device_scale: 1.0,
            sticky_cb: Default::default(),
            composited: None,
            animations: None,
        };
        p.paint_root(root);
        if sink.position().is_some() {
            break;
        }
    }
    sink.position()
}

/// The caret rectangle (viewport CSS px) at a DOM byte offset of a text node, as painted.
pub fn caret_rect<S: Style>(
    root: &Fragment<S>,
    node: NodeId,
    byte: u32,
    options: &PaintOptions,
) -> Option<Rect> {
    let mut sink = paint::CaretSink::new(paint::CaretQuery::Rect { node, byte });
    let mut p = paint::Painter {
        sink: &mut sink,
        viewport: Rect::new(0.0, 0.0, options.viewport.width, options.viewport.height),
        viewport_scroll: options.viewport_scroll,
        scroll: options.scroll,
        layers: false,
        groups: 0,
        in_layer: false,
        in_fixed: false,
        port: None,
        device_scale: 1.0,
        sticky_cb: Default::default(),
        composited: None,
        animations: None,
    };
    p.paint_root(root);
    sink.caret_rect()
}

/// A box's geometry (CSSOM View's offset, client and scroll metrics start from it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxGeometry {
    /// The border box in the root fragment's coordinates (unscrolled, untransformed).
    pub border_box: Rect,
    pub padding_box: Rect,
    pub content_box: Rect,
    /// The scrollable overflow rectangle, in the same coordinates: the padding box and the
    /// reachable overflow ([`Fragment::scrollable_overflow`]).
    pub scrollable_overflow: Rect,
    /// Where the scroll origin is: at the padding box's right edge (else its left) and at its
    /// bottom (else its top) ([`Fragment::scroll_origin_end`]). Scroll positions are measured from
    /// it, so they are negative on an axis whose origin is at the end.
    pub scroll_origin_end: (bool, bool),
}

impl BoxGeometry {
    /// The range of scroll positions on each axis (CSSOM View §2 "scroll position"): from the
    /// scroll origin to the far edge of the scrollable overflow rectangle, `min` negative when
    /// the origin is at the right or bottom edge.
    pub fn scroll_range(&self) -> (Point, Point) {
        let pb = self.padding_box;
        let so = self.scrollable_overflow;
        let (right, bottom) = self.scroll_origin_end;
        let x = if right {
            ((so.x - pb.x).min(0.0), 0.0)
        } else {
            (0.0, (so.right() - pb.right()).max(0.0))
        };
        let y = if bottom {
            ((so.y - pb.y).min(0.0), 0.0)
        } else {
            (0.0, (so.bottom() - pb.bottom()).max(0.0))
        };
        (Point::new(x.0, y.0), Point::new(x.1, y.1))
    }
}

/// A box's border, padding and content boxes: [`BoxGeometry`] without the scrollable overflow,
/// which costs a pass over the box's children.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxRects {
    pub border_box: Rect,
    pub padding_box: Rect,
    pub content_box: Rect,
}

/// A box the user can resize ([`LayoutTree::resizer`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Resizer {
    /// The sizes its resizer changes.
    pub axes: style::Resize,
    /// The resizer's square.
    pub corner: Rect,
    pub border_box: Rect,
    pub content_box: Rect,
    /// Whether `width` and `height` size its border box (`box-sizing: border-box`).
    pub border_box_sizing: bool,
}

impl BoxGeometry {
    /// The geometry of fragment `f` placed at `at` in the root fragment's coordinates (what
    /// [`Fragment::walk`] passes along): [`LayoutTree::geometry`] of the node's first fragment.
    pub fn of<S: Style>(f: &Fragment<S>, at: Point) -> BoxGeometry {
        let t = |r: Rect| r.translate(at.x, at.y);
        BoxGeometry {
            border_box: t(f.border_rect()),
            padding_box: t(f.padding_rect()),
            content_box: t(f.content_rect()),
            // (a box split into columns measures as if stitched together)
            scrollable_overflow: t(f
                .as_box()
                .and_then(|b| b.stitched.as_deref().copied())
                .unwrap_or_else(|| f.scrollable_overflow())),
            scroll_origin_end: f.scroll_origin_end(),
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_api;
#[cfg(test)]
mod tests_find;
#[cfg(test)]
mod tests_flex;
#[cfg(test)]
mod tests_flex_cache;
#[cfg(test)]
mod tests_fuzz;
#[cfg(test)]
mod tests_grid;
#[cfg(test)]
mod tests_incremental;
#[cfg(test)]
mod tests_inline;
#[cfg(test)]
mod tests_more;
#[cfg(test)]
mod tests_overflow;
#[cfg(test)]
mod tests_position;
#[cfg(test)]
mod tests_wpt;
#[cfg(test)]
mod tests_writing;
