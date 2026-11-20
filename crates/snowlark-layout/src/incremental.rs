// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Incremental box tree maintenance: applying a batch of DOM and style changes ([`TreeChanges`]) to
//! an existing box tree, touching only the boxes that need it.
//!
//! - **Style changes** come classified by the style engine ([`BoxDamage`], from `snowlark-style`'s
//!   restyle records). `Repaint` and `Relayout` keep the element's boxes: the new style handle is
//!   swapped into its principal box (and the table box sharing it), into the text boxes of its own
//!   text children and into the anonymous boxes that inherit from it (anonymous blocks, flex items,
//!   table parts), and those boxes are marked dirty with their inline formatting contexts re-prepared
//!   (shaping is reused). `Paint` swaps the same handles without marking anything dirty: the
//!   fragments made from those boxes, and the inline items, take the new handles where they are
//!   cached ([`LayoutTree::update`]'s last step), so nothing is laid out again (replaced boxes and
//!   table parts, whose fragments hold what layout resolved from colours, take the `Repaint`
//!   path). `Reconstruct` regenerates the contents of the nearest block, flex, grid or
//!   table container of the element's parent, as [`LayoutTree::style_changed`] does. A change of
//!   what the element generates inside itself ([`ElementChange::contents`]: `::before`/`::after`,
//!   markers, white space handling; and any list item's, whose `::marker` stylo does not diff)
//!   regenerates the element's own contents when it is a container, else its parent's container's.
//!   A change of the text decorations its contents inherit ([`ElementChange::decorations`]) gives
//!   the boxes inside it their new decorations in place, painting only (their fragments take them
//!   as they take a `Paint` style), unless they hold boxes that cannot be followed back to their
//!   elements: then the contents regenerate as above.
//!   An attribute change takes a replaced element's content again, and regenerates other boxes.
//! - **Structural changes** (children inserted or removed) regenerate the parent's container's
//!   contents; **text changes** update the text box in place ([`LayoutTree::text_changed`]).
//!
//! The containers to regenerate are collected first and deduplicated (one inside another is covered by
//! it), then regenerated once each; the tree is renumbered once. Clean boxes keep their cached layout
//! results, so the next layout reuses every untouched subtree's fragments.

use std::collections::HashSet;
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use snowlark_dom::{local_name, Dom, NodeData, NodeId};

use crate::boxes::*;
use crate::build::Builder;
use crate::fragment::{BoxFragment, Fragment, FragmentChild, FragmentKind};
use crate::style::*;
use crate::LayoutTree;

/// What a style change requires of an element's boxes (the style engine's classification, minus
/// "unchanged").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BoxDamage {
    /// Only what painting reads from the style changes: the boxes take the new style handles and
    /// their fragments stand, taking them too, with no layout.
    Paint,
    /// Only painting changes: new style handles, fragments regenerated (sizes are unchanged).
    Repaint,
    /// The same boxes, laid out again.
    Relayout,
    /// The boxes are generated again.
    Reconstruct,
}

/// One changed element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElementChange {
    pub node: NodeId,
    /// What its own boxes need.
    pub damage: BoxDamage,
    /// The boxes it generates inside itself must be generated again: its `::before`/`::after`,
    /// its marker, the white space handling its descendants take from it.
    pub contents: bool,
    /// The text decorations its descendants' boxes take from it changed: they take them in place
    /// (for painting only), or are generated again where that cannot be done.
    pub decorations: bool,
    /// An attribute boxes read changed (a replaced element's content, a cell's spans, a list
    /// item's value...): a replaced element's content is taken again, other boxes regenerated.
    pub attributes: bool,
}

impl ElementChange {
    /// A style change.
    pub fn restyled(node: NodeId, damage: BoxDamage, contents: bool) -> ElementChange {
        ElementChange {
            node,
            damage,
            contents,
            decorations: false,
            attributes: false,
        }
    }

    /// A style change that also changed the text decorations its contents take from it.
    pub fn decorations(mut self, on: bool) -> ElementChange {
        self.decorations = on;
        self
    }

    /// The element's boxes must be generated again.
    pub fn reconstruct(node: NodeId) -> ElementChange {
        ElementChange::restyled(node, BoxDamage::Reconstruct, false)
    }

    /// An attribute that boxes may read changed.
    pub fn attributes(node: NodeId) -> ElementChange {
        ElementChange {
            node,
            damage: BoxDamage::Relayout,
            contents: false,
            decorations: false,
            attributes: true,
        }
    }
}

/// A batch of changes since the last update, applied by [`LayoutTree::update`].
#[derive(Clone, Debug, Default)]
pub struct TreeChanges {
    /// Restyled elements (an element may appear more than once).
    pub restyled: Vec<ElementChange>,
    /// Nodes whose children were inserted or removed.
    pub children: Vec<NodeId>,
    /// Nodes some of whose text children's data changed.
    pub text: Vec<NodeId>,
}

impl TreeChanges {
    pub fn is_empty(&self) -> bool {
        self.restyled.is_empty() && self.children.is_empty() && self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.restyled.clear();
        self.children.clear();
        self.text.clear();
    }
}

/// What an update did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpdateStats {
    /// The whole tree was built again.
    pub full_build: bool,
    /// Elements whose boxes took their new style in place.
    pub restyled_in_place: u32,
    /// Boxes whose style handle was replaced.
    pub boxes_restyled: u32,
    /// Containers whose contents were regenerated.
    pub containers_rebuilt: u32,
    /// Boxes created (by regeneration or the full build).
    pub boxes_built: u32,
    /// Text boxes updated in place.
    pub text_updated: u32,
}

impl UpdateStats {
    /// Adds another update's counts.
    pub fn add(&mut self, o: &UpdateStats) {
        self.full_build |= o.full_build;
        self.restyled_in_place += o.restyled_in_place;
        self.boxes_restyled += o.boxes_restyled;
        self.containers_rebuilt += o.containers_rebuilt;
        self.boxes_built += o.boxes_built;
        self.text_updated += o.text_updated;
    }
}

/// What a box's new style handle invalidated ([`LayoutTree::swap_style`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Swapped {
    /// A paint-only change: the box keeps its layout.
    Painted,
    /// Layout changed, but the new style prepares inline content as the old one did.
    SamePreparation,
    /// Inline content that holds the box is prepared again.
    Changed,
}

/// How an in-place restyle went.
enum InPlace {
    Done,
    /// Regenerate this box's contents (its own style is already swapped).
    Contents(BoxId),
    /// Regenerate the parent's container's contents.
    Parent,
}

/// The element whose boxes hold `n`'s in the box tree: its parent element, or the host of the shadow
/// root it is a child of (the flat tree, CSS Scoping 1 §3.1: a host's boxes are generated from its
/// shadow tree). `None` at the document. (A slotted element's DOM parent is the host, whose container
/// covers the slot.)
fn box_parent(dom: &Dom, n: NodeId) -> Option<NodeId> {
    let p = dom.parent(n)?;
    if dom.is_element(p) {
        Some(p)
    } else if dom.is_shadow_root(p) {
        dom.host(p)
    } else {
        None
    }
}

/// The element `n`'s children belong to in the box tree: `n` itself, or the host when `n` is a
/// shadow root.
fn box_owner(dom: &Dom, n: NodeId) -> Option<NodeId> {
    if dom.is_element(n) {
        Some(n)
    } else if dom.is_shadow_root(n) {
        dom.host(n)
    } else {
        None
    }
}

/// Copies fragments with the new styles of the boxes in `painted` (see
/// [`LayoutTree::refresh_painted`]).
struct Refresh<'a, S: Style> {
    tree: &'a BoxTree<S>,
    /// The boxes restyled for painting only.
    painted: &'a FxHashSet<BoxId>,
    /// Those boxes and their ancestors: the boxes whose fragments can hold theirs.
    path: &'a FxHashSet<BoxId>,
    /// Each fragment visited: its copy, or `None` when it holds nothing that changed.
    done: FxHashMap<*const Fragment<S>, Option<Arc<Fragment<S>>>>,
}

impl<S: Style> Refresh<'_, S> {
    /// Box `id`'s style as `old` reads it (a fragment or an inline item holds the box's style
    /// through the view of the frame it is in).
    fn style(&self, id: BoxId, old: &S) -> S {
        let s = &self.tree.b(id).style;
        if s.view() == old.view() {
            s.clone()
        } else {
            s.with_view(old.view())
        }
    }

    /// `f` with the new styles, or `None` when nothing in it changed.
    fn fragment(&mut self, f: &Arc<Fragment<S>>) -> Option<Arc<Fragment<S>>> {
        let key = Arc::as_ptr(f);
        if let Some(done) = self.done.get(&key) {
            return done.clone();
        }
        let mut children: Option<Vec<FragmentChild<S>>> = None;
        for (i, c) in f.children.iter().enumerate() {
            let enter = match &c.fragment.kind {
                FragmentKind::Box(b) => b.box_id.is_none_or(|id| self.path.contains(&id)),
                FragmentKind::Line { .. } => true,
                FragmentKind::Text(t) => t.box_id.is_some_and(|id| self.painted.contains(&id)),
            };
            if !enter {
                continue;
            }
            if let Some(new) = self.fragment(&c.fragment) {
                children.get_or_insert_with(|| f.children.clone())[i].fragment = new;
            }
        }
        let own = match &f.kind {
            FragmentKind::Box(b) => b.box_id,
            FragmentKind::Text(t) => t.box_id,
            FragmentKind::Line { .. } => None,
        }
        .filter(|id| self.painted.contains(id));
        let out = if children.is_none() && own.is_none() {
            None
        } else {
            let children = children.unwrap_or_else(|| f.children.clone());
            let kind = match &f.kind {
                FragmentKind::Box(b) => FragmentKind::Box(BoxFragment {
                    style: own.map_or_else(|| b.style.clone(), |id| self.style(id, &b.style)),
                    box_id: b.box_id,
                    node: b.node,
                    pseudo: b.pseudo,
                    border: b.border,
                    padding: b.padding,
                    flags: b.flags,
                    order: b.order,
                    subtree_end: b.subtree_end,
                    content: b.content.clone(),
                    containing: b.containing.copied(),
                    clip_reference: b.clip_reference.copied(),
                    stitched: b.stitched.clone(),
                }),
                FragmentKind::Text(t) => {
                    let mut t = t.clone();
                    if let Some(id) = own {
                        t.style = self.style(id, &t.style);
                        // (a text node's run: an ellipsis takes its style from a text box but
                        // draws no decorations; decorations change in place only for text nodes)
                        if t.node.is_some() {
                            t.decorations = self.tree.b(id).decorations.clone();
                        }
                    }
                    FragmentKind::Text(t)
                }
                FragmentKind::Line { baseline } => FragmentKind::Line {
                    baseline: *baseline,
                },
            };
            Some(Arc::new(Fragment {
                size: f.size,
                kind,
                children,
                overflow: f.overflow,
                paint_bounds: f.paint_bounds,
                inflow: f.inflow,
                baseline: f.baseline,
                last_baseline: f.last_baseline,
            }))
        };
        self.done.insert(key, out.clone());
        out
    }
}

fn is_container<S: Style>(b: &LayoutBox<S>) -> bool {
    matches!(
        b.kind,
        BoxKind::Block
            | BoxKind::TableCell
            | BoxKind::TableCaption
            | BoxKind::Flex
            | BoxKind::Grid
            | BoxKind::TableWrapper
    ) && b.pseudo.is_none()
}

impl<S: Style> LayoutTree<S> {
    /// The nearest container box (block container, flex, grid, table wrapper) of `start` or its
    /// ancestors, whose contents regenerate `start`'s children.
    fn container_of(&self, dom: &Dom, start: NodeId) -> Option<BoxId> {
        let mut cur = Some(start);
        while let Some(n) = cur {
            if let Some(b) = self.tree.box_of(n) {
                if is_container(self.tree.b(b)) {
                    return Some(b);
                }
            }
            cur = box_parent(dom, n);
        }
        None
    }

    /// Applies `changes` to the box tree of `document` (built before with [`LayoutTree::build`]):
    /// see the module documentation. Falls back to a full build when there is no tree yet or a change
    /// reaches the root.
    pub fn update<Src: StyleSource<Style = S>>(
        &mut self,
        dom: &Dom,
        document: NodeId,
        styles: &Src,
        changes: &TreeChanges,
    ) -> UpdateStats {
        let allocated = self.tree.allocated;
        let mut stats = UpdateStats::default();
        let full = |this: &mut Self, stats: &mut UpdateStats| {
            this.build(dom, document, styles);
            stats.full_build = true;
            stats.boxes_built = (this.tree.allocated - allocated) as u32;
            *stats
        };
        self.fullscreen = styles.fullscreen_element();
        if self.tree.root.is_none() || styles.top_layer() != self.top_layer.as_slice() {
            return full(self, &mut stats);
        }
        // The top layer's boxes hang off the root, not where their elements are (a container
        // regenerated around one skips it and leaves them): a restyle of a top layer element (its
        // boxes, its backdrop's) builds the tree again, as does regenerating the root's contents.
        if changes
            .restyled
            .iter()
            .any(|c| self.top_layer.contains(&c.node))
        {
            return full(self, &mut stats);
        }
        let mine =
            |n: NodeId| dom.is_alive(n) && dom.is_connected(n) && dom.node_document(n) == document;
        let mut rebuild: Vec<BoxId> = Vec::new();
        // The nodes whose boxes a change reaches (a regenerated container keeps its other
        // children's boxes), and the elements whose own boxes take a new style by regenerating.
        let mut touched: HashSet<NodeId> = HashSet::new();
        let mut restyled: HashSet<NodeId> = HashSet::new();

        // Children inserted or removed.
        for &parent in &changes.children {
            // (a shadow root's children are its host's in the box tree: libraries like Lit render
            // straight into the shadow root)
            if !dom.is_alive(parent) {
                continue;
            }
            touched.insert(parent);
            let Some(parent) = box_owner(dom, parent) else {
                if mine(parent) {
                    return full(self, &mut stats);
                }
                continue;
            };
            if !mine(parent) {
                continue;
            }
            if styles.element_style(parent).is_none() {
                // In a `display: none` subtree: no boxes.
                continue;
            }
            match self.container_of(dom, parent) {
                Some(c) => rebuild.push(c),
                None => return full(self, &mut stats),
            }
        }

        // Text data changed: update the text boxes that differ.
        for &changed in &changes.text {
            if !dom.is_alive(changed) {
                continue;
            }
            let Some(parent) = box_owner(dom, changed) else {
                continue;
            };
            if !mine(parent) {
                continue;
            }
            let Some(parent_style) = styles.element_style(parent) else {
                continue;
            };
            let mut structural = false;
            for c in dom.children(changed) {
                let Some(data) = dom.character_data(c) else {
                    continue;
                };
                if !matches!(dom.data(c), NodeData::Text(_) | NodeData::CDataSection(_)) {
                    continue;
                }
                match self.tree.box_of(c) {
                    Some(b) => {
                        if self.tree.b(b).text() != Some(data) {
                            if self.text_changed(dom, c) {
                                stats.text_updated += 1;
                            } else {
                                structural = true;
                            }
                        }
                    }
                    // Text without a box is collapsible white space between blocks (or empty);
                    // it needs one now unless it still is.
                    None => {
                        if !data.is_empty()
                            && !crate::build::is_collapsible_whitespace(&parent_style, data)
                        {
                            structural = true;
                        }
                    }
                }
            }
            if structural {
                touched.insert(changed);
                touched.insert(parent);
                match self.container_of(dom, parent) {
                    Some(c) => rebuild.push(c),
                    None => return full(self, &mut stats),
                }
            }
        }

        // Restyled elements.
        for change in &changes.restyled {
            let el = change.node;
            if !mine(el) || !dom.is_element(el) {
                continue;
            }
            let outcome = if change.damage == BoxDamage::Reconstruct {
                InPlace::Parent
            } else {
                self.restyle_in_place(dom, styles, change, &mut stats)
            };
            if !matches!(outcome, InPlace::Done) {
                touched.insert(el);
                restyled.insert(el);
            }
            match outcome {
                InPlace::Done => {}
                InPlace::Contents(b) => rebuild.push(b),
                InPlace::Parent => {
                    let Some(parent) = box_parent(dom, el) else {
                        return full(self, &mut stats);
                    };
                    match self.container_of(dom, parent) {
                        Some(c) => rebuild.push(c),
                        None => return full(self, &mut stats),
                    }
                }
            }
        }

        if !self.top_layer.is_empty() && self.tree.root.is_some_and(|r| rebuild.contains(&r)) {
            return full(self, &mut stats);
        }

        // A node is touched when one of its descendants is (a container covered by another one is
        // regenerated with it, so its ancestors' boxes cannot be kept either).
        for n in touched.iter().copied().collect::<Vec<_>>() {
            let mut cur = box_parent(dom, n);
            while let Some(a) = cur {
                if !touched.insert(a) {
                    break;
                }
                cur = box_parent(dom, a);
            }
        }

        // Regenerate each container once, outermost only.
        let set: HashSet<BoxId> = rebuild.iter().copied().collect();
        let mut done: HashSet<BoxId> = HashSet::new();
        let mut rebuilt: Vec<BoxId> = Vec::new();
        for &c in &rebuild {
            if done.contains(&c) || self.tree.get(c).is_none() {
                continue;
            }
            let mut covered = false;
            let mut p = self.tree.b(c).parent;
            while let Some(a) = p {
                if set.contains(&a) {
                    covered = true;
                    break;
                }
                p = self.tree.b(a).parent;
            }
            done.insert(c);
            if covered {
                continue;
            }
            let mut builder = Builder::new(&mut self.tree, dom, styles);
            // (what a restyled container's children inherit from its boxes may differ)
            if builder
                .tree
                .b(c)
                .node
                .is_some_and(|n| restyled.contains(&n))
            {
                builder.rebuild_contents(c);
            } else {
                builder.rebuild_contents_reusing(c, &touched);
            }
            rebuilt.push(c);
            stats.containers_rebuilt += 1;
        }
        if !rebuilt.is_empty() {
            // (a container's regeneration changes only its own subtree: see `rebuild_contents`)
            self.tree.renumber_within(&rebuilt);
        }
        self.refresh_canvas_background(dom, document, styles);
        self.refresh_painted();
        stats.boxes_built = (self.tree.allocated - allocated) as u32;
        stats
    }

    /// Gives the fragments of the boxes restyled for painting only ([`BoxDamage::Paint`]) their
    /// boxes' new styles, wherever fragments are kept for reuse: the last fragment tree, each box's
    /// cached layout result, and a flex or grid container's built fragment and leaf results; and
    /// the prepared inline items those boxes have in their formatting contexts. Fragments nest
    /// only in the fragments of their boxes' ancestors (and in line boxes), so only the boxes on
    /// the way from those boxes to the root are visited; a fragment that holds none of them is
    /// shared as it is, and one that does is copied with what changed (a fragment reached from
    /// several caches is copied once).
    fn refresh_painted(&mut self) {
        if self.painted.is_empty() {
            return;
        }
        let painted: FxHashSet<BoxId> = self
            .painted
            .drain(..)
            .filter(|&id| self.tree.get(id).is_some())
            .collect();
        let mut path: FxHashSet<BoxId> = FxHashSet::default();
        for &id in &painted {
            let mut cur = Some(id);
            while let Some(c) = cur {
                if !path.insert(c) {
                    break;
                }
                cur = self.tree.b(c).parent;
            }
        }
        let mut r = Refresh {
            tree: &self.tree,
            painted: &painted,
            path: &path,
            done: FxHashMap::default(),
        };
        let root = self.root.as_ref().and_then(|f| r.fragment(f));
        // (a box's caches, refreshed, then put back once the tree is no longer borrowed)
        let mut caches = Vec::new();
        for &id in &path {
            let cache = &self.tree.b(id).cache;
            let layout = cache
                .layout
                .as_ref()
                .and_then(|c| r.fragment(&c.result.fragment));
            let (leaf, built) = cache.taffy.as_ref().map_or((None, None), |t| {
                (
                    t.leaf.as_ref().and_then(|l| r.fragment(&l.fragment)),
                    t.built.as_ref().and_then(|b| r.fragment(&b.fragment)),
                )
            });
            let items: Vec<(usize, S)> = cache.inline.as_ref().map_or(Vec::new(), |c| {
                c.items
                    .iter()
                    .enumerate()
                    .filter(|(_, it)| painted.contains(&it.box_id))
                    .map(|(i, it)| (i, r.style(it.box_id, &it.style)))
                    .collect()
            });
            if layout.is_some() || leaf.is_some() || built.is_some() || !items.is_empty() {
                caches.push((id, layout, leaf, built, items));
            }
        }
        if let Some(root) = root {
            self.root = Some(root);
            self.lookup.reset();
        }
        for (id, layout, leaf, built, items) in caches {
            let cache = &mut self.tree.b_mut(id).cache;
            if let (Some(f), Some(c)) = (layout, cache.layout.as_mut()) {
                c.result.fragment = f;
            }
            if let Some(t) = cache.taffy.as_mut() {
                if let (Some(f), Some(l)) = (leaf, t.leaf.as_mut()) {
                    l.fragment = f;
                }
                if let (Some(f), Some(b)) = (built, t.built.as_mut()) {
                    b.fragment = f;
                }
            }
            if let Some(c) = cache.inline.as_mut() {
                for (i, s) in items {
                    c.items[i].style = s;
                }
            }
        }
    }

    /// The canvas background source (the root's or the body's box) may have changed.
    fn refresh_canvas_background<Src: StyleSource<Style = S>>(
        &mut self,
        dom: &Dom,
        document: NodeId,
        styles: &Src,
    ) {
        let Some(root_el) = dom.document_element(document) else {
            return;
        };
        let old = self
            .tree
            .canvas_background
            .filter(|&b| self.tree.get(b).is_some());
        if let Some(o) = old {
            self.tree.b_mut(o).set(flags::BACKGROUND_TO_CANVAS, false);
        }
        Builder::new(&mut self.tree, dom, styles).find_canvas_background(root_el);
        let new = self.tree.canvas_background;
        if new != old {
            for b in [old, new].into_iter().flatten() {
                self.tree.mark_dirty(b);
            }
        }
        // (and the body whose overflow applies to the viewport)
        let old = self
            .tree
            .viewport_overflow
            .take()
            .filter(|&b| self.tree.get(b).is_some());
        if let Some(o) = old {
            let b = self.tree.b_mut(o);
            b.set(flags::VIEWPORT_OVERFLOW, false);
            // (its own `overflow` applies again, and may make it a formatting context root)
            let s = &b.style;
            if s.overflow_x() != Overflow::Visible || s.overflow_y() != Overflow::Visible {
                b.set(flags::INDEPENDENT, true);
            }
        }
        Builder::new(&mut self.tree, dom, styles).find_viewport_overflow(root_el);
        let new = self.tree.viewport_overflow;
        if new != old {
            for b in [old, new].into_iter().flatten() {
                self.tree.mark_dirty(b);
            }
        }
        Builder::new(&mut self.tree, dom, styles).find_principal_writing_mode(root_el);
    }

    /// Replaces a box's style handle; what was derived from the old one goes. With `paint` (a
    /// paint-only change), a box whose fragments hold nothing layout resolved from the style keeps
    /// its layout and is listed for [`LayoutTree::refresh_painted`]. Otherwise its own inline
    /// content (if it has some) is prepared again, or only takes the new style handles when the
    /// new style prepares it the same way.
    fn swap_style(&mut self, id: BoxId, style: S, stats: &mut UpdateStats, paint: bool) -> Swapped {
        let style = self.tree.framed(id, style);
        let b = self.tree.b_mut(id);
        let old = std::mem::replace(&mut b.style, style);
        if paint
            && matches!(
                b.kind,
                BoxKind::Block
                    | BoxKind::Flex
                    | BoxKind::Grid
                    | BoxKind::TableCaption
                    | BoxKind::Inline
                    | BoxKind::Text
                    | BoxKind::LineBreak
            )
        {
            stats.boxes_restyled += 1;
            self.painted.push(id);
            return Swapped::Painted;
        }
        b.cache.taffy = None;
        let same = old.same_inline_preparation(&b.style);
        if same {
            b.cache.inline_restyled = true;
        } else {
            b.cache.inline_stale = true;
        }
        stats.boxes_restyled += 1;
        self.tree.mark_dirty(id);
        if same {
            Swapped::SamePreparation
        } else {
            Swapped::Changed
        }
    }

    /// Gives `el`'s boxes its new style without regenerating them.
    fn restyle_in_place<Src: StyleSource<Style = S>>(
        &mut self,
        dom: &Dom,
        styles: &Src,
        change: &ElementChange,
        stats: &mut UpdateStats,
    ) -> InPlace {
        let el = change.node;
        let pseudos = change.contents;
        let Some(style) = styles.element_style(el) else {
            // No style: it is in a `display: none` subtree, so it has no boxes.
            return if self.tree.box_of(el).is_some() {
                InPlace::Parent
            } else {
                InPlace::Done
            };
        };
        let Some(principal) = self.tree.box_of(el) else {
            if change.attributes {
                return InPlace::Parent;
            }
            if style.display() != Display::Contents {
                // `display: none`: nothing to update.
                return InPlace::Done;
            }
            if pseudos || change.decorations {
                return InPlace::Parent;
            }
            // `display: contents`: only its text children take its style.
            let paint = change.damage == BoxDamage::Paint;
            for c in dom.children(el) {
                if let Some(t) = self.tree.box_of(c) {
                    if self.tree.b(t).kind == BoxKind::Text {
                        let swapped = self.swap_style(t, style.clone(), stats, paint);
                        self.restyled_owner(t, swapped);
                    }
                }
            }
            stats.restyled_in_place += 1;
            return InPlace::Done;
        };
        let b = self.tree.b(principal);
        // A new writing mode changes which boxes establish formatting contexts (CSS Writing Modes
        // 4 §3.2) and how the box's parent sees it.
        if b.style.writing_mode() != style.writing_mode() {
            return InPlace::Parent;
        }
        // An `img` showing its alt text (whose text box takes its style), or one that stopped
        // being replaced (its style can make it show its alt text), is built again.
        // A `progress` or `meter` drawn by its parts (whose styles inherit from it), or that is a
        // native control again, is built again too.
        if (b.kind != BoxKind::Replaced && styles.alt_text(el).is_some())
            || (b.kind == BoxKind::Replaced && styles.replaced_content(el).is_none())
            || (b.kind != BoxKind::Replaced
                && (dom.is_html_element_named(el, &local_name!("progress"))
                    || dom.is_html_element_named(el, &local_name!("meter"))))
        {
            return InPlace::Parent;
        }
        if change.attributes {
            // Only a replaced element's content can be taken again in place (and only while it
            // stays replaced).
            let replaced = styles.replaced_content(el);
            if b.kind != BoxKind::Replaced || replaced.is_none() {
                return InPlace::Parent;
            }
        }
        // A split inline box has pieces elsewhere in its container.
        if b.kind == BoxKind::Inline && (b.has(flags::NO_START_EDGE) || b.has(flags::NO_END_EDGE)) {
            return InPlace::Parent;
        }
        let container = is_container(b);
        let list_item = matches!(
            style.display(),
            Display::Box {
                list_item: true,
                ..
            }
        );
        let marker = list_item
            && Builder::new(&mut self.tree, dom, styles)
                .has_or_would_have_marker(el, principal, &style);
        // The decorations its contents take from it, recomputed in place (`None`: its contents
        // are generated again instead).
        let redecorated = if change.decorations && !pseudos && !marker {
            Builder::new(&mut self.tree, dom, styles).redecorated(principal, &style)
        } else {
            None
        };
        let regenerate = pseudos || marker || (change.decorations && redecorated.is_none());
        if regenerate && !container {
            return InPlace::Parent;
        }
        stats.restyled_in_place += 1;
        // (what the element generates inside itself is regenerated: it is laid out again)
        let paint = change.damage == BoxDamage::Paint && !change.attributes && !regenerate;
        let swapped = self.swap_style(principal, style.clone(), stats, paint);
        self.restyled_owner(principal, swapped);
        if self.tree.b(principal).kind == BoxKind::Replaced {
            if let Some(content) = styles.replaced_content(el) {
                self.tree.b_mut(principal).data = BoxData::Replaced(content);
            }
        }
        if regenerate {
            return InPlace::Contents(principal);
        }
        // Text boxes whose decorations changed are painted again from their fragments as they
        // stand (decorations are painted along the text, not laid out).
        for (id, d) in redecorated.into_iter().flatten() {
            let b = self.tree.b_mut(id);
            if b.decorations != d {
                b.decorations = d;
                if b.kind == BoxKind::Text {
                    self.painted.push(id);
                }
            }
        }
        // The text and anonymous boxes that take or inherit its style.
        let mut stack: Vec<BoxId> = self.tree.b(principal).children.clone();
        while let Some(c) = stack.pop() {
            let cb = self.tree.b(c);
            let parent = cb.parent.expect("a child box has a parent");
            match (cb.kind, cb.node, cb.pseudo) {
                (BoxKind::Text, Some(t), None) => {
                    if dom.parent(t) == Some(el) {
                        self.swap_style(c, style.clone(), stats, paint);
                    }
                }
                // The table box shares its wrapper's (the element's) style.
                (BoxKind::Table, Some(n), None) if n == el => {
                    self.swap_style(c, style.clone(), stats, paint);
                    stack.extend(self.tree.b(c).children.iter().copied());
                }
                (kind, None, None) => {
                    let parent_box = self.tree.b(parent);
                    let s = if kind == BoxKind::Table && parent_box.is_anonymous() {
                        parent_box.style.clone()
                    } else {
                        let anon = match kind {
                            BoxKind::TableWrapper => AnonymousBox::Table {
                                inline: cb.level == Level::Inline,
                            },
                            BoxKind::TableRow => AnonymousBox::TableRow,
                            BoxKind::TableCell => AnonymousBox::TableCell,
                            _ => AnonymousBox::Block,
                        };
                        styles.anonymous_style(&parent_box.style, anon)
                    };
                    self.swap_style(c, s, stats, paint);
                    if swapped == Swapped::Changed {
                        // (an anonymous block's content takes `unicode-bidi`, which is not
                        // inherited, from the block around it)
                        self.tree.b_mut(c).cache.inline_stale = true;
                    }
                    stack.extend(self.tree.b(c).children.iter().copied());
                }
                _ => {}
            }
        }
        InPlace::Done
    }

    /// Marks the inline formatting context holding `id` (its nearest ancestor that is not an inline
    /// box) for preparation again: its items hold the boxes' styles.
    fn stale_owner(&mut self, id: BoxId) {
        let mut cur = self.tree.b(id).parent;
        while let Some(o) = cur {
            let b = self.tree.b_mut(o);
            b.cache.inline_stale = true;
            if b.kind != BoxKind::Inline {
                break;
            }
            cur = b.parent;
        }
    }

    /// After `id`'s style was swapped: the inline formatting context holding it is prepared again,
    /// or, when the new style prepares content the same way, its items take the new style handles
    /// (a paint-only swap refreshes them through [`LayoutTree::refresh_painted`]).
    fn restyled_owner(&mut self, id: BoxId, swapped: Swapped) {
        match swapped {
            Swapped::Painted => {}
            Swapped::Changed => self.stale_owner(id),
            Swapped::SamePreparation => {
                let mut cur = self.tree.b(id).parent;
                while let Some(o) = cur {
                    let b = self.tree.b_mut(o);
                    b.cache.inline_restyled = true;
                    if b.kind != BoxKind::Inline {
                        break;
                    }
                    cur = b.parent;
                }
            }
        }
    }

    /// Replaces box styles throughout the tree (after a change the style handles carry outside the
    /// computed values, like the web fonts a handle resolves `font-family` with). `f` returns the new
    /// handle for a box's style and whether layout changes with it (the box's font changed, say), or
    /// `None` to keep it. Returns how many boxes changed for layout.
    pub fn restyle_all(&mut self, mut f: impl FnMut(&S) -> Option<(S, bool)>) -> u32 {
        let mut live = Vec::new();
        for (index, b) in self.tree.boxes.iter().enumerate() {
            if let Some(b) = b {
                live.push(BoxId {
                    index: index as u32,
                    generation: b.generation,
                });
            }
        }
        let mut changed = 0;
        for id in live {
            let Some((style, layout)) = f(&self.tree.b(id).style) else {
                continue;
            };
            let style = self.tree.framed(id, style);
            let b = self.tree.b_mut(id);
            b.style = style;
            if layout {
                b.cache.taffy = None;
                b.cache.inline_stale = true;
                self.tree.mark_dirty(id);
                self.stale_owner(id);
                changed += 1;
            }
        }
        changed
    }
}
