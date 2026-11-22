// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Box generation from the styled DOM (CSS Display 3 §2, CSS 2 §9.2, CSS Tables 3 §3.2, CSS Lists 3 §3).
//!
//! Each element generates boxes by its computed `display`: none, its children's boxes (`contents`), or a
//! principal box whose kind follows the inner display type. Then each container's child list is fixed up:
//!
//! - **Block containers** get *anonymous block boxes* around runs of inline-level content when they
//!   also have block-level children; an inline box holding a block-level box is split around it
//!   (CSS 2 §9.2.1.1). Runs holding only collapsible white space and out-of-flow boxes generate nothing.
//! - **Flex and grid containers**: every in-flow child is blockified into an item; runs of text are
//!   wrapped in anonymous block items, white-space-only runs dropped (css-flexbox-1 §4, css-grid-2 §6).
//! - **Tables**: irrelevant white space is removed, missing rows and cells are generated around
//!   misparented children, and misparented table-internal boxes get an anonymous table (CSS Tables 3
//!   §3.2). Every table box sits in a table wrapper box with its captions.
//! - **List items** get a `::marker` box: inside as their first inline child, outside held aside on
//!   the item. Ordinals follow HTML's list owner rules (`<ol start reversed>`, `<li value>`).
//!
//! Floats, absolutely positioned boxes and the root are blockified (CSS Display 3 §2.7).

use snowlark_dom::{local_name, ns, Dom, NodeData, NodeId};

use crate::boxes::*;
use crate::style::*;

/// Context a child list is generated in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// Inside a block container or inline box.
    Flow,
    /// Children of a flex or grid container: blockified.
    FlexGrid,
}

/// A list owner's counter (HTML §4.4.8 ordinal values).
struct ListCounter {
    next: i64,
    step: i64,
}

pub(crate) struct Builder<'a, Src: StyleSource> {
    pub tree: &'a mut BoxTree<Src::Style>,
    pub dom: &'a Dom,
    pub src: &'a Src,
    counters: Vec<ListCounter>,
    /// While a container's contents are regenerated: its children's boxes that may be kept, by
    /// element (see [`Builder::rebuild_contents_reusing`]).
    reuse: rustc_hash::FxHashMap<NodeId, BoxId>,
    /// The kept boxes taken over so far.
    taken: Vec<BoxId>,
}

impl<'a, Src: StyleSource> Builder<'a, Src> {
    pub fn new(tree: &'a mut BoxTree<Src::Style>, dom: &'a Dom, src: &'a Src) -> Self {
        Builder {
            tree,
            dom,
            src,
            counters: Vec::new(),
            reuse: Default::default(),
            taken: Vec::new(),
        }
    }

    /// Builds the whole tree for `document`'s root element, replacing any previous tree.
    pub fn build_document(&mut self, document: NodeId) {
        if let Some(old) = self.tree.root.take() {
            self.tree.free_subtree(old);
        }
        self.tree.calcs.clear();
        self.tree.split_owners.clear();
        self.tree.split_inlines.clear();
        self.tree.canvas_background = None;
        self.tree.viewport_overflow = None;
        self.tree.line_height_quirk =
            self.dom.quirks_mode(document) != snowlark_dom::QuirksMode::NoQuirks;
        let Some(root_el) = self.dom.document_element(document) else {
            return;
        };
        let Some(style) = self.src.element_style(root_el) else {
            return;
        };
        let mut out = Vec::new();
        self.element(
            root_el,
            style,
            Ctx::Flow,
            true,
            &Decorations::new(),
            &mut out,
        );
        // The root generates at most one box (display: contents on the root behaves as block in
        // practice; CSS Display 3 says it generates no box, leaving its children, which we wrap).
        let root = match out.len() {
            0 => None,
            1 => Some(out[0]),
            _ => {
                let s = self.src.anonymous_style(
                    &self.src.element_style(root_el).unwrap(),
                    AnonymousBox::Block,
                );
                let b = self
                    .tree
                    .alloc(BoxKind::Block, Level::Block, s, None, BoxData::None);
                self.fixup_block_container(b, out);
                Some(b)
            }
        };
        if let Some(r) = root {
            let rb = self.tree.b_mut(r);
            rb.set(flags::ROOT, true);
            rb.set(flags::INDEPENDENT, true);
            rb.level = Level::Block;
        }
        self.tree.root = root;
        if let Some(r) = root {
            self.top_layer_boxes(r, document);
        }
        self.find_canvas_background(root_el);
        self.find_viewport_overflow(root_el);
        self.find_principal_writing_mode(root_el);
        self.tree.renumber();
    }

    /// CSS Position 4 §4.1: each element of the top layer (bottom first) is rendered as a child of
    /// the root's box after its other children, over its `::backdrop` box, whatever its ancestors
    /// are; both are marked [`flags::TOP_LAYER`] for painting. (Elements whose style says
    /// `display: none`, or that are not rendered, generate nothing.)
    fn top_layer_boxes(&mut self, root: BoxId, document: NodeId) {
        let elements: Vec<NodeId> = self.src.top_layer().to_vec();
        if elements.is_empty() {
            return;
        }
        let mut added = Vec::new();
        for el in elements {
            if !self.dom.is_alive(el)
                || !self.dom.is_connected(el)
                || self.dom.node_document(el) != document
                // (the root element in the top layer, `document.documentElement` fullscreen, is
                // the root box already)
                || self.dom.document_element(document) == Some(el)
            {
                continue;
            }
            let Some(style) = self.src.element_style(el) else {
                continue;
            };
            if style.display() == Display::None {
                continue;
            }
            if let Some(bs) = self.src.pseudo_style(el, PseudoElement::Backdrop) {
                if bs.display() != Display::None {
                    let (_, oof, _) = self.levels(bs.display(), &bs, Ctx::Flow, false);
                    let b =
                        self.tree
                            .alloc(BoxKind::Block, Level::Block, bs, Some(el), BoxData::None);
                    let lb = self.tree.b_mut(b);
                    lb.pseudo = Some(PseudoElement::Backdrop);
                    lb.set(flags::OUT_OF_FLOW, oof);
                    lb.set(flags::INDEPENDENT, true);
                    lb.set(flags::TOP_LAYER, true);
                    added.push(b);
                }
            }
            let mut out = Vec::new();
            self.element(el, style, Ctx::Flow, false, &Decorations::new(), &mut out);
            for b in out {
                self.tree.b_mut(b).set(flags::TOP_LAYER, true);
                added.push(b);
            }
        }
        if added.is_empty() {
            return;
        }
        let mut children = self.tree.b(root).children.clone();
        children.extend(added);
        self.tree.set_children(root, children);
    }

    /// CSS Overflow 3 §3.3: the root element's `overflow` applies to the viewport; in HTML, when the
    /// root's is `visible` on both axes, the `<body>`'s does instead (marked
    /// [`flags::VIEWPORT_OVERFLOW`]).
    /// CSS Writing Modes 4 §8: the principal writing mode is the root element's used one, which for
    /// a root with a body child is the first such child's computed `writing-mode` and `direction`
    /// (containment on either turns that off). The root box takes that frame; the viewport and the
    /// initial containing block follow it.
    pub fn find_principal_writing_mode(&mut self, root_el: NodeId) {
        let Some(root) = self.tree.root else {
            return;
        };
        let contained = |s: &Src::Style| {
            let c = s.phys_size_containment();
            c.width || c.height || s.layout_containment() || s.paint_containment()
        };
        let mut principal = root;
        if self
            .dom
            .is_html_element_named(root_el, &local_name!("html"))
            && !contained(&self.tree.b(root).style)
        {
            let body = self
                .dom
                .children(root_el)
                .filter(|&c| self.dom.is_html_element_named(c, &local_name!("body")))
                .find_map(|c| self.tree.box_of(c));
            if let Some(b) = body.filter(|&b| !contained(&self.tree.b(b).style)) {
                principal = b;
            }
        }
        self.tree.principal = Some(principal);
        let view = Flip::of(self.tree.b(principal).style.writing_mode());
        if self.tree.b(root).style.view() != view {
            let b = self.tree.b_mut(root);
            b.style = b.style.with_view(view);
            self.tree.mark_dirty(root);
        }
        // (and what takes the root's style: its text and the anonymous blocks around it)
        let mut stack = self.tree.b(root).children.clone();
        while let Some(c) = stack.pop() {
            let b = self.tree.b_mut(c);
            if b.is_anonymous() || b.kind == BoxKind::Text {
                if b.style.view() != view {
                    b.style = b.style.with_view(view);
                    b.cache.inline_stale = true;
                    self.tree.mark_dirty(c);
                }
                stack.extend(self.tree.b(c).children.iter().copied());
            }
        }
        if principal != root
            && self.tree.b(root).style.writing_mode() != self.tree.b(principal).style.writing_mode()
        {
            // (the root's own writing mode did not make the body a formatting context root)
            let b = self.tree.b(principal);
            let flow = matches!(
                b.style.display(),
                Display::Box {
                    inside: DisplayInside::Flow,
                    ..
                }
            );
            if b.kind == BoxKind::Block
                && b.level == Level::Block
                && flow
                && !b.is_out_of_flow_or_float()
                && self.tree.b(root).kind == BoxKind::Block
                && b.style.overflow_x() == Overflow::Visible
                && b.style.overflow_y() == Overflow::Visible
            {
                self.tree.b_mut(principal).set(flags::INDEPENDENT, false);
            }
        }
    }

    pub fn find_viewport_overflow(&mut self, root_el: NodeId) {
        let Some(root) = self.tree.root else {
            return;
        };
        let s = &self.tree.b(root).style;
        if s.overflow_x() != Overflow::Visible || s.overflow_y() != Overflow::Visible {
            return;
        }
        if !self
            .dom
            .is_html_element_named(root_el, &local_name!("html"))
        {
            return;
        }
        let Some(body) = self.dom.body(self.dom.node_document(root_el)) else {
            return;
        };
        if !self.dom.is_html_element_named(body, &local_name!("body")) {
            return;
        }
        if let Some(bb) = self.tree.box_of(body) {
            // (CSS Contain 2 §3.4: with paint containment on the root or the body, the body's
            // overflow stays its own)
            let contained = |s: &Src::Style| s.paint_containment();
            if contained(&self.tree.b(root).style) || contained(&self.tree.b(bb).style) {
                return;
            }
            self.tree.b_mut(bb).set(flags::VIEWPORT_OVERFLOW, true);
            self.tree.viewport_overflow = Some(bb);
            // Its own used `overflow` is then `visible`, so the overflow does not make it a block
            // formatting context root: its children's margins collapse through it as usual.
            let b = self.tree.b(bb);
            let flow = matches!(
                b.style.display(),
                Display::Box {
                    inside: DisplayInside::Flow,
                    ..
                }
            );
            let parent_flow = b.parent.is_some_and(|p| {
                let pb = self.tree.b(p);
                pb.kind == BoxKind::Block && pb.style.view() == b.style.view()
            });
            if b.kind == BoxKind::Block
                && b.level == Level::Block
                && flow
                && parent_flow
                && !b.is_out_of_flow_or_float()
                && !b.has(flags::ROOT)
                // (other reasons for a BFC stay: a line-clamp container, layout containment)
                && b.style.line_clamp().is_none()
                && !b.style.layout_containment()
            {
                self.tree.b_mut(bb).set(flags::INDEPENDENT, false);
            }
        }
    }

    /// CSS Backgrounds 3 §2.11.2: the root's background paints the canvas; in HTML, when the root's
    /// background is transparent with no image, the `<body>`'s is used instead.
    pub fn find_canvas_background(&mut self, root_el: NodeId) {
        let Some(root) = self.tree.root else {
            return;
        };
        let has_bg = |s: &Src::Style| {
            !s.background_color().is_transparent() || s.background_layer_count() > 0
        };
        let mut source = root;
        if !has_bg(&self.tree.b(root).style)
            && self
                .dom
                .is_html_element_named(root_el, &local_name!("html"))
        {
            if let Some(body) = self.dom.body(self.dom.node_document(root_el)) {
                if let Some(bb) = self.tree.box_of(body) {
                    if has_bg(&self.tree.b(bb).style) {
                        source = bb;
                    }
                }
            }
        }
        self.tree
            .b_mut(source)
            .set(flags::BACKGROUND_TO_CANVAS, true);
        self.tree.canvas_background = Some(source);
    }

    /// A list item that has no marker without asking for its `::marker` style.
    fn no_marker(src: &Src, item_style: &Src::Style) -> bool {
        item_style.list_style_type() == ListStyleType::None
            && !matches!(item_style.list_style_image(), Image::Raster { .. })
            && !src.may_style_markers()
    }

    /// Whether list item `el` (with `style`) has, or would now get, a `::marker` box.
    pub fn has_or_would_have_marker(&mut self, el: NodeId, id: BoxId, style: &Src::Style) -> bool {
        let b = self.tree.b(id);
        if b.marker.is_some()
            || b.children
                .first()
                .is_some_and(|&c| self.tree.b(c).pseudo == Some(PseudoElement::Marker))
        {
            return true;
        }
        if Self::no_marker(self.src, style) {
            return false;
        }
        let Some(m) = self.src.pseudo_style(el, PseudoElement::Marker) else {
            return false;
        };
        match m.content() {
            Content::None => false,
            Content::Items(items) => !items.is_empty(),
            Content::Normal => {
                matches!(style.list_style_image(), Image::Raster { .. })
                    || marker_text(&style.list_style_type(), 1).is_some()
            }
        }
    }

    /// Regenerates the contents of `id` (a block container, flex/grid container, table part or table
    /// wrapper) from its DOM children. The box itself stays. The caller renumbers the tree after.
    pub fn rebuild_contents(&mut self, id: BoxId) {
        let b = self.tree.b(id);
        let kind = b.kind;
        let Some(node) = b.node else {
            return;
        };
        let style = b.style.clone();
        let deco = b.decorations.clone();
        let list_item = matches!(
            self.used_display(Some(node), None, &style),
            Display::Box {
                list_item: true,
                ..
            }
        );
        // The old contents go after the new ones are made, but for the boxes taken over.
        let old = if self.reuse.is_empty() {
            self.tree.free_descendants(id);
            Vec::new()
        } else {
            let b = self.tree.b_mut(id);
            let mut old = std::mem::take(&mut b.children);
            old.extend(b.marker.take());
            old
        };
        let ctx = if matches!(kind, BoxKind::Flex | BoxKind::Grid) {
            Ctx::FlexGrid
        } else {
            Ctx::Flow
        };
        // A rebuilt list item keeps its ordinal: replay its owner's counter up to it.
        let ordinal = if list_item {
            Some(self.ordinal_in_place(node))
        } else {
            None
        };
        // A rebuilt list restarts its own counter.
        self.enter_list(node);
        let mut raw = Vec::new();
        // Decorations propagate as the first build propagated them: not into atomic boxes.
        let own_ctx = if self.tree.b(id).has(flags::FLEX_GRID_ITEM) {
            Ctx::FlexGrid
        } else {
            Ctx::Flow
        };
        let atomic = self.is_atomic(
            self.used_display(Some(node), None, &style),
            &style,
            own_ctx,
            self.tree.b(id).has(flags::ROOT),
        );
        let child_deco = self.decorations_for(&style, &deco, atomic);
        let inside_marker = style.list_style_position() == ListStylePosition::Inside;
        if let (Some(n), true) = (ordinal, inside_marker) {
            self.list_marker_inline(node, n, &style, &child_deco, &mut raw);
        }
        if kind == BoxKind::TableWrapper {
            // Rebuild the table grid box and its captions.
            let table = self.tree.alloc(
                BoxKind::Table,
                Level::Block,
                style.clone(),
                Some(node),
                BoxData::None,
            );
            self.tree.b_mut(table).set(flags::INDEPENDENT, true);
            self.children(node, &style, Ctx::Flow, &child_deco, &mut raw);
            self.finish_table(id, table, raw);
        } else {
            self.children(node, &style, ctx, &child_deco, &mut raw);
            self.fixup(id, raw);
        }
        if let (Some(n), false) = (ordinal, inside_marker) {
            self.outside_marker(id, node, n, &style, &child_deco);
        }
        self.tree.b_mut(id).cache.inline = None;
        self.leave_list(node);
        self.reuse.clear();
        if !old.is_empty() {
            let taken: rustc_hash::FxHashSet<BoxId> =
                std::mem::take(&mut self.taken).into_iter().collect();
            for c in old {
                if !taken.contains(&c) {
                    self.tree.free_subtree(c);
                }
            }
        }
        self.tree.mark_dirty(id);
    }

    /// [`Builder::rebuild_contents`], keeping the box subtrees of the children nothing changed in, as
    /// a document's layout tree attaches only the boxes of what was inserted: an element child of the
    /// container's own element keeps its principal box (and with it the layout results cached in its
    /// subtree) when neither it nor any of its descendants is in `touched` and its box is one the
    /// container holds directly (an in-flow block-level box of a block container, a flex or grid
    /// item), generated in the same context as now. A list item keeps it only with an unchanged
    /// marker; inside a list, an element that is neither a list item nor a list owner never does (its
    /// descendants may count in the list).
    pub fn rebuild_contents_reusing(
        &mut self,
        id: BoxId,
        touched: &std::collections::HashSet<NodeId>,
    ) {
        self.collect_reusable(id, touched);
        self.rebuild_contents(id);
    }

    fn collect_reusable(&mut self, id: BoxId, touched: &std::collections::HashSet<NodeId>) {
        self.reuse.clear();
        self.taken.clear();
        let b = self.tree.b(id);
        let Some(node) = b.node else {
            return;
        };
        let flex_grid = match b.kind {
            BoxKind::Flex | BoxKind::Grid => true,
            BoxKind::Block | BoxKind::TableCell | BoxKind::TableCaption => false,
            _ => return,
        };
        // (a container whose own style changed regenerates everything: what its children inherit
        // from its box, such as decorations, may differ; the flat tree of a shadow host, `details`
        // and the rendered legend of a fieldset depend on more than one child)
        if self.dom.shadow_root(node).is_some()
            || self.src.details_parts(node).is_some()
            || self.src.has_form_parts(node)
            || self
                .dom
                .is_html_element_named(node, &local_name!("fieldset"))
            || b.has(flags::FIELDSET)
        {
            return;
        }
        for &c in &b.children {
            let cb = self.tree.b(c);
            let Some(n) = cb.node else {
                continue;
            };
            // (a removed child's node may be freed already)
            if cb.pseudo.is_some()
                || !self.dom.is_alive(n)
                || touched.contains(&n)
                || self.dom.parent(n) != Some(node)
                || self.tree.box_of(n) != Some(c)
                || self.src.top_layer().contains(&n)
            {
                continue;
            }
            let held = if flex_grid {
                cb.has(flags::FLEX_GRID_ITEM)
            } else {
                cb.is_in_flow_block_level() && !cb.has(flags::FLEX_GRID_ITEM)
            };
            if held {
                self.reuse.insert(n, c);
            }
        }
    }

    /// Takes over the box `b` kept for element `el` (with style `style`), when it still holds.
    fn take_reused(&mut self, el: NodeId, b: BoxId, style: &Src::Style) -> bool {
        let list_item = matches!(
            self.used_display(Some(el), None, style),
            Display::Box {
                list_item: true,
                ..
            }
        );
        if !list_item {
            // A list owner counts its own items; any other element inside a list may hold items
            // that count in it.
            return self.counters.is_empty() || self.is_list_owner(el);
        }
        let saved = (
            self.counters.len(),
            self.counters.last().map_or(0, |c| c.next),
        );
        let n = self.ordinal(el);
        if self.marker_holds(b, n, style) {
            return true;
        }
        // (generated afresh: the ordinal is taken again)
        self.counters.truncate(saved.0);
        if let Some(c) = self.counters.last_mut() {
            c.next = saved.1;
        }
        false
    }

    /// Whether list item box `b`'s marker is the one ordinal `n` gives it.
    fn marker_holds(&self, b: BoxId, n: i64, style: &Src::Style) -> bool {
        let lb = self.tree.b(b);
        let marker = lb.marker.or_else(|| {
            lb.children
                .first()
                .copied()
                .filter(|&c| self.tree.b(c).pseudo == Some(PseudoElement::Marker))
        });
        let Some(m) = marker else {
            return Self::no_marker(self.src, style);
        };
        match self.tree.b(m).children.as_slice() {
            // (a text marker shows its ordinal in its list style; an image or `content` marker does
            // not depend on it)
            [t] => match &self.tree.b(*t).data {
                BoxData::Text(old) => {
                    marker_text(&style.list_style_type(), n).is_some_and(|s| s == *old)
                }
                _ => true,
            },
            _ => true,
        }
    }

    // ----- generation -----

    /// The decorations an element's content inherits.
    fn decorations_for(
        &self,
        style: &Src::Style,
        parent: &Decorations,
        atomic: bool,
    ) -> Decorations {
        let mut d = if atomic {
            Decorations::new()
        } else {
            parent.clone()
        };
        let line = style.text_decoration_line();
        if !line.is_none() {
            d.push(Decoration {
                line,
                color: style.text_decoration_color(),
                style: style.text_decoration_style(),
            });
        }
        d
    }

    /// The decorations of the boxes inside `principal`, the principal box of an element whose own
    /// decorations changed (its new style `style`), recomputed as generation computes them, with
    /// the boxes' structure as it stands: each box with its new decorations. `None` when its
    /// contents hold boxes this cannot follow back to the elements that generate them (generated
    /// content, markers, table parts, `display: contents`, shadow trees, split inlines, alt text):
    /// those contents are generated again instead.
    pub(crate) fn redecorated(
        &self,
        principal: BoxId,
        style: &Src::Style,
    ) -> Option<Vec<(BoxId, Decorations)>> {
        let b = self.tree.b(principal);
        let el = b.node?;
        let own = b.decorations.clone();
        let inner = self.content_decorations(el, principal, style, &own)?;
        let mut out = Vec::new();
        self.redecorate_children(principal, &own, el, &inner, &mut out)?;
        Some(out)
    }

    /// The decorations element `el`'s contents take, its box `id` taking `own`.
    fn content_decorations(
        &self,
        el: NodeId,
        id: BoxId,
        style: &Src::Style,
        own: &Decorations,
    ) -> Option<Decorations> {
        if self.src.flat_children(el).is_some()
            || self.src.details_parts(el).is_some()
            || self.src.has_form_parts(el)
            || self.src.alt_text(el).is_some()
        {
            return None;
        }
        let b = self.tree.b(id);
        let ctx = if b.has(flags::FLEX_GRID_ITEM) {
            Ctx::FlexGrid
        } else {
            Ctx::Flow
        };
        let atomic = self.is_atomic(
            self.used_display(Some(el), None, style),
            style,
            ctx,
            b.has(flags::ROOT),
        );
        Some(self.decorations_for(style, own, atomic))
    }

    /// [`Builder::redecorated`] for the children of `id` (which takes `own`), inside element `el`
    /// whose contents take `deco`.
    fn redecorate_children(
        &self,
        id: BoxId,
        own: &Decorations,
        el: NodeId,
        deco: &Decorations,
        out: &mut Vec<(BoxId, Decorations)>,
    ) -> Option<()> {
        for &c in &self.tree.b(id).children {
            let cb = self.tree.b(c);
            if cb.pseudo.is_some()
                || cb.marker.is_some()
                || cb.has(flags::NO_START_EDGE)
                || cb.has(flags::NO_END_EDGE)
            {
                return None;
            }
            match (cb.kind, cb.node) {
                (BoxKind::Text, Some(t)) if self.dom.parent(t) == Some(el) => {
                    out.push((c, deco.clone()));
                }
                // (generation stores no decorations on these)
                (BoxKind::Replaced | BoxKind::LineBreak, Some(n))
                    if self.dom.parent(n) == Some(el) => {}
                (BoxKind::Block | BoxKind::Inline | BoxKind::Flex | BoxKind::Grid, Some(n))
                    if self.dom.parent(n) == Some(el) =>
                {
                    let style = self.src.element_style(n)?;
                    let inner = self.content_decorations(n, c, &style, deco)?;
                    out.push((c, deco.clone()));
                    self.redecorate_children(c, deco, n, &inner, out)?;
                }
                // An anonymous block takes its parent's decorations, its contents the element's.
                (BoxKind::Block, None) => {
                    out.push((c, own.clone()));
                    self.redecorate_children(c, own, el, deco, out)?;
                }
                _ => return None,
            }
        }
        Some(())
    }

    /// Generates the boxes for `parent`'s children (with its `::before`/`::after`) into `out`.
    fn children(
        &mut self,
        parent: NodeId,
        parent_style: &Src::Style,
        ctx: Ctx,
        deco: &Decorations,
        out: &mut Vec<BoxId>,
    ) {
        self.pseudo(parent, PseudoElement::Before, ctx, deco, out);
        if let Some(parts) = self.src.details_parts(parent) {
            self.details_children(parent, parent_style, &parts, ctx, deco, out);
        } else if let Some(parts) = self.src.form_parts(parent) {
            self.form_part_boxes(&parts, ctx, deco, out);
        } else {
            // The flat tree: a shadow host's shadow tree, a slot's assigned nodes.
            match self.src.flat_children(parent) {
                Some(list) => self.child_nodes(&list, parent_style, ctx, deco, out),
                None => {
                    let mut child = self.dom.first_child(parent);
                    while let Some(c) = child {
                        self.child_node(c, parent_style, ctx, deco, out);
                        child = self.dom.next_sibling(c);
                    }
                }
            }
        }
        self.pseudo(parent, PseudoElement::After, ctx, deco, out);
    }

    /// Generates the boxes of `nodes` (children of a box with style `parent_style`) into `out`.
    fn child_nodes(
        &mut self,
        nodes: &[NodeId],
        parent_style: &Src::Style,
        ctx: Ctx,
        deco: &Decorations,
        out: &mut Vec<BoxId>,
    ) {
        for &c in nodes {
            self.child_node(c, parent_style, ctx, deco, out);
        }
    }

    /// Generates the boxes of the child `c` of a box with style `parent_style` into `out`.
    fn child_node(
        &mut self,
        c: NodeId,
        parent_style: &Src::Style,
        ctx: Ctx,
        deco: &Decorations,
        out: &mut Vec<BoxId>,
    ) {
        {
            match self.dom.data(c) {
                // (a top layer element's boxes are made under the root: see `top_layer_boxes`)
                NodeData::Element(_) if self.src.top_layer().contains(&c) => {}
                NodeData::Element(_) => {
                    if let Some(style) = self.src.element_style(c) {
                        self.element(c, style, ctx, false, deco, out);
                    }
                }
                NodeData::Text(t) | NodeData::CDataSection(t) if !t.is_empty() => {
                    let id = self.tree.alloc(
                        BoxKind::Text,
                        Level::Inline,
                        parent_style.clone(),
                        Some(c),
                        BoxData::Text(t.as_str().to_owned()),
                    );
                    self.tree.b_mut(id).decorations = deco.clone();
                    self.tree.set_node_box(c, id);
                    out.push(id);
                }
                _ => {}
            }
        }
    }

    /// HTML 15.5.5: the first slot of a `details` element without a `summary` child holds the default
    /// summary, which shows implementation-defined text and toggles the element like a summary.
    fn default_summary(
        &mut self,
        open: bool,
        details_style: &Src::Style,
        ctx: Ctx,
        deco: &Decorations,
        out: &mut Vec<BoxId>,
    ) {
        let style = self.src.anonymous_style(details_style, AnonymousBox::Block);
        // (the disclosure marker is part of the text: a list marker needs a list item element)
        let label = if open {
            "\u{25BC} Details"
        } else {
            "\u{25B6} Details"
        };
        let text = self.tree.alloc(
            BoxKind::Text,
            Level::Inline,
            style.clone(),
            None,
            BoxData::Text(label.to_owned()),
        );
        self.tree.b_mut(text).decorations = deco.clone();
        if let Some(b) = self.principal_box(
            None,
            Some(PseudoElement::DetailsSummary),
            style,
            ctx,
            false,
            vec![text],
        ) {
            out.push(b);
        }
    }

    /// HTML 15.5.5: a `details` element's first slot holds its summary, its second the rest, as the
    /// box of `::details-content`.
    fn details_children(
        &mut self,
        details: NodeId,
        details_style: &Src::Style,
        parts: &DetailsParts,
        ctx: Ctx,
        deco: &Decorations,
        out: &mut Vec<BoxId>,
    ) {
        match parts.summary {
            Some(s) => self.child_nodes(&[s], details_style, ctx, deco, out),
            None => self.default_summary(parts.open, details_style, ctx, deco, out),
        }
        let Some(style) = self
            .src
            .pseudo_style(details, PseudoElement::DetailsContent)
        else {
            return;
        };
        let display = style.display();
        if display == Display::None {
            return;
        }
        let d = self.decorations_for(&style, deco, false);
        if display == Display::Contents {
            self.child_nodes(&parts.content, &style, ctx, &d, out);
            return;
        }
        let mut raw = Vec::new();
        self.child_nodes(&parts.content, &style, Ctx::Flow, &d, &mut raw);
        if let Some(b) = self.principal_box(
            None,
            Some(PseudoElement::DetailsContent),
            style,
            ctx,
            false,
            raw,
        ) {
            out.push(b);
        }
    }

    /// The boxes of a `progress` or `meter` element's parts (each in the one before it), which it
    /// renders in place of its children.
    fn form_part_boxes(
        &mut self,
        parts: &[(FormPart, Src::Style)],
        ctx: Ctx,
        deco: &Decorations,
        out: &mut Vec<BoxId>,
    ) {
        let Some(((part, style), inner)) = parts.split_first() else {
            return;
        };
        let display = style.display();
        if display == Display::None {
            return;
        }
        if display == Display::Contents {
            let d = self.decorations_for(style, deco, false);
            self.form_part_boxes(inner, ctx, &d, out);
            return;
        }
        let atomic = self.is_atomic(display, style, ctx, false);
        let d = self.decorations_for(style, deco, atomic);
        let child_ctx = match display {
            Display::Box {
                inside: DisplayInside::Flex | DisplayInside::Grid,
                ..
            } => Ctx::FlexGrid,
            _ => Ctx::Flow,
        };
        let mut raw = Vec::new();
        self.form_part_boxes(inner, child_ctx, &d, &mut raw);
        if let Some(b) = self.principal_box(
            None,
            Some(PseudoElement::Part(*part)),
            style.clone(),
            ctx,
            false,
            raw,
        ) {
            self.tree.b_mut(b).decorations = deco.clone();
            out.push(b);
        }
    }

    /// `::before` / `::after` (CSS Pseudo 4 §2.6).
    fn pseudo(
        &mut self,
        el: NodeId,
        which: PseudoElement,
        ctx: Ctx,
        deco: &Decorations,
        out: &mut Vec<BoxId>,
    ) {
        let Some(style) = self.src.pseudo_style(el, which) else {
            return;
        };
        let Content::Items(items) = style.content() else {
            return;
        };
        let display = style.display();
        if display == Display::None {
            return;
        }
        let mut generated = Vec::new();
        let child_deco = self.decorations_for(&style, deco, false);
        for item in &items {
            if let Some(b) = self.generated_item(item, &style, &child_deco) {
                generated.push(b);
            }
        }
        if display == Display::Contents {
            out.extend(generated);
            return;
        }
        if let Some(b) = self.principal_box(None, Some(which), style, ctx, false, generated) {
            self.tree.b_mut(b).pseudo_of = Some(el);
            out.push(b);
        }
    }

    /// A box for one `content` item.
    fn generated_item(
        &mut self,
        item: &ContentItem,
        style: &Src::Style,
        deco: &Decorations,
    ) -> Option<BoxId> {
        match item {
            ContentItem::Text(t) if !t.is_empty() => {
                let id = self.tree.alloc(
                    BoxKind::Text,
                    Level::Inline,
                    style.clone(),
                    None,
                    BoxData::Text(t.to_string()),
                );
                self.tree.b_mut(id).decorations = deco.clone();
                Some(id)
            }
            ContentItem::Image(Image::Raster { id, natural }) => {
                // CSS Generated Content 3 §1.2: an image is an anonymous replaced element; one
                // without natural dimensions is sized as such (CSS 2 §10.3.2).
                let s = self.src.anonymous_style(style, AnonymousBox::Block);
                let b = self.tree.alloc(
                    BoxKind::Replaced,
                    Level::Inline,
                    s,
                    None,
                    BoxData::Replaced(ReplacedContent::with_natural(*id, *natural)),
                );
                self.tree.b_mut(b).set(flags::INDEPENDENT, true);
                Some(b)
            }
            // A gradient, color or cross-fade: replaced content painted as an image.
            ContentItem::Image(image) if *image != Image::None => {
                let s = self.src.anonymous_style(style, AnonymousBox::Block);
                let natural = image.natural();
                let b = self.tree.alloc(
                    BoxKind::Replaced,
                    Level::Inline,
                    s,
                    None,
                    BoxData::Replaced(ReplacedContent {
                        natural_width: natural.width,
                        natural_height: natural.height,
                        natural_ratio: natural.ratio,
                        generated: Some(image.clone()),
                        ..ReplacedContent::default()
                    }),
                );
                self.tree.b_mut(b).set(flags::INDEPENDENT, true);
                Some(b)
            }
            _ => None,
        }
    }

    /// Generates `el`'s boxes into `out`.
    fn element(
        &mut self,
        el: NodeId,
        style: Src::Style,
        ctx: Ctx,
        is_root: bool,
        deco: &Decorations,
        out: &mut Vec<BoxId>,
    ) {
        match style.display() {
            Display::None => return,
            Display::Contents if !is_root => {
                let d = self.decorations_for(&style, deco, false);
                self.children(el, &style, ctx, &d, out);
                return;
            }
            _ => {}
        }
        if !self.reuse.is_empty() {
            if let Some(b) = self.reuse.remove(&el) {
                if self.take_reused(el, b, &style) {
                    self.taken.push(b);
                    out.push(b);
                    return;
                }
            }
        }
        self.enter_list(el);
        let b = self.principal_element_box(el, style, ctx, is_root, deco);
        self.leave_list(el);
        if let Some(b) = b {
            self.mark_fieldset(el, b);
            self.tree.set_node_box(el, b);
            out.push(b);
        }
    }

    /// HTML §15.3.13: a `fieldset` that is a block container establishes a block formatting context,
    /// and its first child box generated by a `legend` element that is block-level and neither floated
    /// nor absolutely positioned is its rendered legend.
    fn mark_fieldset(&mut self, el: NodeId, b: BoxId) {
        if !self.dom.is_html_element_named(el, &local_name!("fieldset"))
            || self.tree.b(b).kind != BoxKind::Block
        {
            return;
        }
        let fs = self.tree.b_mut(b);
        fs.set(flags::FIELDSET, true);
        fs.set(flags::INDEPENDENT, true);
        let kids = self.tree.b(b).children.clone();
        let legend = kids.into_iter().find(|&c| {
            let cb = self.tree.b(c);
            cb.node
                .is_some_and(|n| self.dom.is_html_element_named(n, &local_name!("legend")))
                && cb.pseudo.is_none()
        });
        if let Some(l) = legend {
            let lb = self.tree.b(l);
            if lb.level == Level::Block && !lb.is_out_of_flow_or_float() && lb.is_block_container()
            {
                self.tree.b_mut(l).set(flags::RENDERED_LEGEND, true);
            }
        }
    }

    fn principal_element_box(
        &mut self,
        el: NodeId,
        style: Src::Style,
        ctx: Ctx,
        is_root: bool,
        deco: &Decorations,
    ) -> Option<BoxId> {
        // Replaced elements generate an atomic box with no children (CSS 2 §3.1, HTML §15.4).
        if let Some(content) = self.src.replaced_content(el) {
            let (level, oof, float) = self.levels(style.display(), &style, ctx, is_root);
            // A replaced element is atomic: table-internal display types don't apply to it.
            let level = if level == Level::Internal {
                Level::Inline
            } else {
                level
            };
            let id = self.tree.alloc(
                BoxKind::Replaced,
                level,
                style.clone(),
                Some(el),
                BoxData::Replaced(content),
            );
            let b = self.tree.b_mut(id);
            b.set(flags::INDEPENDENT, true);
            b.set(flags::OUT_OF_FLOW, oof);
            b.set(flags::FLOAT, float);
            b.set(flags::FLEX_GRID_ITEM, ctx == Ctx::FlexGrid && !oof);
            b.set(flags::ORIGINALLY_INLINE, Self::originally_inline(&style));
            return Some(id);
        }
        // An `img` that represents its alt text is a non-replaced element with that text as its
        // content (HTML §15.4.4).
        if let Some(alt) = self.src.alt_text(el) {
            let atomic = self.is_atomic(
                self.used_display(Some(el), None, &style),
                &style,
                ctx,
                is_root,
            );
            let child_deco = self.decorations_for(&style, deco, atomic);
            let text = self.tree.alloc(
                BoxKind::Text,
                Level::Inline,
                style.clone(),
                None,
                BoxData::Text(alt),
            );
            self.tree.b_mut(text).decorations = child_deco;
            let id = self.principal_box(Some(el), None, style, ctx, is_root, vec![text])?;
            self.tree.b_mut(id).decorations = deco.clone();
            return Some(id);
        }
        // <br> is a forced line break (HTML §15.3.2 `br { display-outside: newline }`).
        if self.dom.is_html_element_named(el, &local_name!("br"))
            && ctx == Ctx::Flow
            && !style.position().is_out_of_flow()
            && style.float() == Float::None
        {
            let id = self.tree.alloc(
                BoxKind::LineBreak,
                Level::Inline,
                style,
                Some(el),
                BoxData::None,
            );
            return Some(id);
        }
        let list_item = matches!(
            self.used_display(Some(el), None, &style),
            Display::Box {
                list_item: true,
                ..
            }
        );
        let atomic = self.is_atomic(
            self.used_display(Some(el), None, &style),
            &style,
            ctx,
            is_root,
        );
        let child_deco = self.decorations_for(&style, deco, atomic);
        let mut raw = Vec::new();
        let inside = match self.used_display(Some(el), None, &style) {
            Display::Box { inside, .. } => inside,
            _ => DisplayInside::Flow,
        };
        let child_ctx = if matches!(inside, DisplayInside::Flex | DisplayInside::Grid) {
            Ctx::FlexGrid
        } else {
            Ctx::Flow
        };
        // The marker comes first (CSS Lists 3 §3.1); it needs the item's box id when outside, so
        // outside markers are attached after the box exists.
        let marker_first = list_item && style.list_style_position() == ListStylePosition::Inside;
        let ordinal = if list_item {
            Some(self.ordinal(el))
        } else {
            None
        };
        if let (Some(n), true) = (ordinal, marker_first) {
            self.list_marker_inline(el, n, &style, &child_deco, &mut raw);
        }
        self.children(el, &style, child_ctx, &child_deco, &mut raw);
        let id = self.principal_box(Some(el), None, style.clone(), ctx, is_root, raw)?;
        self.tree.b_mut(id).decorations = deco.clone();
        if let (Some(n), false) = (ordinal, marker_first) {
            self.outside_marker(id, el, n, &style, &child_deco);
        }
        Some(id)
    }

    fn originally_inline(style: &Src::Style) -> bool {
        matches!(
            style.original_display(),
            Display::Box {
                outside: DisplayOutside::Inline,
                ..
            }
        )
    }

    /// Whether `node` (with no pseudo-element) is a `button` element, which uses button layout
    /// (HTML §15.5.4).
    fn is_button(&self, node: Option<NodeId>, pseudo: Option<PseudoElement>) -> bool {
        pseudo.is_none()
            && node.is_some_and(|n| self.dom.is_html_element_named(n, &local_name!("button")))
    }

    /// The `display` a box behaves as. A `button` element (button layout, HTML §15.5.3) behaves as
    /// its computed value when that is `inline-grid`, `grid`, `inline-flex`, `flex`, `none` or
    /// `contents`; otherwise as `inline-block` when the outer display type is `inline`, and as
    /// `flow-root` for any other value (block, list-item, the table and ruby types). The computed
    /// value stays as it is.
    fn used_display(
        &self,
        node: Option<NodeId>,
        pseudo: Option<PseudoElement>,
        style: &Src::Style,
    ) -> Display {
        let display = style.display();
        if !self.is_button(node, pseudo) {
            return display;
        }
        match display {
            Display::None
            | Display::Contents
            | Display::Box {
                inside: DisplayInside::Flex | DisplayInside::Grid,
                ..
            } => display,
            Display::Box {
                outside: DisplayOutside::Inline,
                ..
            } => Display::Box {
                outside: DisplayOutside::Inline,
                inside: DisplayInside::FlowRoot,
                list_item: false,
            },
            _ => Display::Box {
                outside: DisplayOutside::Block,
                inside: DisplayInside::FlowRoot,
                list_item: false,
            },
        }
    }

    /// (level, out-of-flow, floated) after blockification, for a box that behaves as `display`.
    fn levels(
        &self,
        display: Display,
        style: &Src::Style,
        ctx: Ctx,
        is_root: bool,
    ) -> (Level, bool, bool) {
        let oof = style.position().is_out_of_flow() && !is_root;
        let float = !oof && !is_root && style.float() != Float::None;
        let blockify = oof || float || is_root || ctx == Ctx::FlexGrid;
        let level = match display {
            Display::Internal(_) if blockify => Level::Block,
            Display::Internal(DisplayInternal::RubyBase | DisplayInternal::RubyText)
            | Display::Internal(
                DisplayInternal::RubyBaseContainer | DisplayInternal::RubyTextContainer,
            ) => Level::Inline,
            Display::Internal(_) => Level::Internal,
            Display::Box {
                outside: DisplayOutside::Inline,
                ..
            } if !blockify => Level::Inline,
            _ => Level::Block,
        };
        (level, oof, float)
    }

    fn is_atomic(&self, display: Display, style: &Src::Style, ctx: Ctx, is_root: bool) -> bool {
        let (level, oof, float) = self.levels(display, style, ctx, is_root);
        oof || float
            || match display {
                Display::Box {
                    inside: DisplayInside::Flow | DisplayInside::Ruby,
                    ..
                } => false,
                Display::Box { .. } => level == Level::Inline,
                _ => false,
            }
    }

    /// Creates the principal box for an element or pseudo-element with the given generated children.
    fn principal_box(
        &mut self,
        node: Option<NodeId>,
        pseudo: Option<PseudoElement>,
        style: Src::Style,
        ctx: Ctx,
        is_root: bool,
        raw: Vec<BoxId>,
    ) -> Option<BoxId> {
        let display = self.used_display(node, pseudo, &style);
        let button = self.is_button(node, pseudo);
        let (level, oof, float) = self.levels(display, &style, ctx, is_root);
        let originally_inline = Self::originally_inline(&style);
        // (a scroll container establishes an independent formatting context, `overflow: clip`
        // does not: CSS Overflow 3 §3.1; a line-clamp container does, CSS Overflow 4 §4.1)
        let independent_style = style.overflow_x().is_scroll_container()
            || style.overflow_y().is_scroll_container()
            || style.line_clamp().is_some()
            // (a multi-column container does, CSS Multi-column Layout 1 §2)
            || style.is_multicol()
            // (and so do layout and paint containment, CSS Contain 2 §3.2, §3.4)
            || style.layout_containment()
            || style.paint_containment();
        let id = match display {
            Display::None => return None,
            Display::Box { inside, .. } => match inside {
                DisplayInside::Flow | DisplayInside::Ruby if level == Level::Inline => {
                    let id =
                        self.tree
                            .alloc(BoxKind::Inline, Level::Inline, style, node, BoxData::None);
                    let raw = self.wrap_misparented_table_parts(id, raw, true);
                    self.tree.set_children(id, raw);
                    id
                }
                DisplayInside::Flow | DisplayInside::FlowRoot | DisplayInside::Ruby => {
                    let id = self
                        .tree
                        .alloc(BoxKind::Block, level, style, node, BoxData::None);
                    // CSS 2 §9.4.1: floats, absolutely positioned boxes, block containers that
                    // aren't block boxes (inline-blocks, cells), and boxes with overflow other than
                    // visible establish new block formatting contexts; so does flow-root (which a
                    // button behaves as, or as inline-block: `used_display`).
                    let bfc = inside == DisplayInside::FlowRoot
                        || oof
                        || float
                        || is_root
                        || level == Level::Inline
                        || independent_style
                        || ctx == Ctx::FlexGrid;
                    self.tree.b_mut(id).set(flags::INDEPENDENT, bfc);
                    self.fixup_block_container(id, raw);
                    id
                }
                DisplayInside::Flex | DisplayInside::Grid => {
                    let kind = if inside == DisplayInside::Flex {
                        BoxKind::Flex
                    } else {
                        BoxKind::Grid
                    };
                    let id = self.tree.alloc(kind, level, style, node, BoxData::None);
                    self.tree.b_mut(id).set(flags::INDEPENDENT, true);
                    self.fixup_flex_grid(id, raw);
                    id
                }
                DisplayInside::Table => {
                    let wrapper = self.tree.alloc(
                        BoxKind::TableWrapper,
                        level,
                        style.clone(),
                        node,
                        BoxData::None,
                    );
                    let table =
                        self.tree
                            .alloc(BoxKind::Table, Level::Block, style, node, BoxData::None);
                    self.tree.b_mut(wrapper).set(flags::INDEPENDENT, true);
                    self.tree.b_mut(table).set(flags::INDEPENDENT, true);
                    self.finish_table(wrapper, table, raw);
                    wrapper
                }
            },
            Display::Internal(internal) => {
                if level != Level::Internal {
                    // A blockified table-internal or ruby box: a block container (or inline box).
                    let kind = if level == Level::Inline {
                        BoxKind::Inline
                    } else {
                        BoxKind::Block
                    };
                    let id = self.tree.alloc(kind, level, style, node, BoxData::None);
                    if kind == BoxKind::Block {
                        self.tree.b_mut(id).set(flags::INDEPENDENT, true);
                        self.fixup_block_container(id, raw);
                    } else {
                        self.tree.set_children(id, raw);
                    }
                    id
                } else {
                    let (kind, data) = match internal {
                        DisplayInternal::TableRowGroup => {
                            (BoxKind::TableRowGroup(RowGroup::Body), BoxData::None)
                        }
                        DisplayInternal::TableHeaderGroup => {
                            (BoxKind::TableRowGroup(RowGroup::Header), BoxData::None)
                        }
                        DisplayInternal::TableFooterGroup => {
                            (BoxKind::TableRowGroup(RowGroup::Footer), BoxData::None)
                        }
                        DisplayInternal::TableRow => (BoxKind::TableRow, BoxData::None),
                        DisplayInternal::TableCell => {
                            let (colspan, rowspan) = self.spans(node);
                            (BoxKind::TableCell, BoxData::Cell { colspan, rowspan })
                        }
                        DisplayInternal::TableColumnGroup => (
                            BoxKind::TableColumnGroup,
                            BoxData::Column {
                                span: self.column_span(node),
                            },
                        ),
                        DisplayInternal::TableColumn => (
                            BoxKind::TableColumn,
                            BoxData::Column {
                                span: self.column_span(node),
                            },
                        ),
                        DisplayInternal::TableCaption => (BoxKind::TableCaption, BoxData::None),
                        _ => unreachable!("ruby internals are inline-level"),
                    };
                    let id = self.tree.alloc(kind, Level::Internal, style, node, data);
                    if matches!(kind, BoxKind::TableCell | BoxKind::TableCaption) {
                        self.tree.b_mut(id).set(flags::INDEPENDENT, true);
                    }
                    self.fixup(id, raw);
                    id
                }
            }
            Display::Contents => {
                // Only the root gets here: treat it as a block.
                let id = self
                    .tree
                    .alloc(BoxKind::Block, Level::Block, style, node, BoxData::None);
                self.fixup_block_container(id, raw);
                id
            }
        };
        let b = self.tree.b_mut(id);
        b.pseudo = pseudo;
        b.set(flags::OUT_OF_FLOW, oof);
        b.set(flags::FLOAT, float);
        b.set(flags::ORIGINALLY_INLINE, originally_inline);
        b.set(flags::FLEX_GRID_ITEM, ctx == Ctx::FlexGrid && !oof);
        b.set(flags::BUTTON, button);
        if oof || float || ctx == Ctx::FlexGrid {
            b.set(flags::INDEPENDENT, true);
        }
        Some(id)
    }

    /// Runs the fixup for a container of any kind.
    fn fixup(&mut self, id: BoxId, raw: Vec<BoxId>) {
        match self.tree.b(id).kind {
            BoxKind::Block | BoxKind::TableCell | BoxKind::TableCaption => {
                self.fixup_block_container(id, raw)
            }
            BoxKind::Flex | BoxKind::Grid => self.fixup_flex_grid(id, raw),
            BoxKind::Table => {
                let raw = self.fixup_table_children(id, raw);
                self.tree.set_children(id, raw);
            }
            BoxKind::TableRowGroup(_) => {
                let raw = self.fixup_row_group(id, raw);
                self.tree.set_children(id, raw);
            }
            BoxKind::TableRow => {
                let raw = self.fixup_row(id, raw);
                self.tree.set_children(id, raw);
            }
            BoxKind::TableColumnGroup => {
                let mut keep = Vec::new();
                for c in raw {
                    if self.tree.b(c).kind == BoxKind::TableColumn {
                        keep.push(c);
                    } else {
                        self.tree.free_subtree(c);
                    }
                }
                self.tree.set_children(id, keep);
            }
            BoxKind::TableColumn => {
                for c in raw {
                    self.tree.free_subtree(c);
                }
            }
            BoxKind::Inline => self.tree.set_children(id, raw),
            BoxKind::TableWrapper | BoxKind::Text | BoxKind::Replaced | BoxKind::LineBreak => {
                for c in raw {
                    self.tree.free_subtree(c);
                }
            }
        }
    }

    // ----- block containers -----

    /// Whether a Text box is only collapsible white space (it generates nothing between blocks).
    fn is_collapsible_whitespace(&self, id: BoxId) -> bool {
        let b = self.tree.b(id);
        let BoxData::Text(t) = &b.data else {
            return false;
        };
        is_collapsible_whitespace(&b.style, t)
    }

    fn fixup_block_container(&mut self, id: BoxId, raw: Vec<BoxId>) {
        let raw = self.wrap_misparented_table_parts(id, raw, false);
        // Split inline boxes around block-level descendants.
        let mut flat = Vec::with_capacity(raw.len());
        for c in raw {
            self.split_inline(c, &mut flat);
        }
        let any_block = flat
            .iter()
            .any(|&c| self.tree.b(c).is_in_flow_block_level());
        if !any_block {
            self.tree.b_mut(id).set(flags::INLINE_CONTENT, true);
            self.tree.set_children(id, flat);
            return;
        }
        self.tree.b_mut(id).set(flags::INLINE_CONTENT, false);
        let mut children = Vec::with_capacity(flat.len());
        let mut run: Vec<BoxId> = Vec::new();
        for c in flat {
            if self.tree.b(c).is_in_flow_block_level() {
                self.flush_inline_run(id, &mut run, &mut children);
                children.push(c);
            } else {
                run.push(c);
            }
        }
        self.flush_inline_run(id, &mut run, &mut children);
        self.tree.set_children(id, children);
    }

    /// Wraps a run of inline-level and out-of-flow boxes among blocks in an anonymous block, unless it
    /// holds only collapsible white space and out-of-flow boxes.
    fn flush_inline_run(&mut self, parent: BoxId, run: &mut Vec<BoxId>, out: &mut Vec<BoxId>) {
        if run.is_empty() {
            return;
        }
        let has_inline = run.iter().any(|&c| {
            let b = self.tree.b(c);
            !b.is_out_of_flow_or_float()
                && !(b.kind == BoxKind::Text && self.is_collapsible_whitespace(c))
        });
        if !has_inline {
            for c in run.drain(..) {
                if self.tree.b(c).kind == BoxKind::Text {
                    self.tree.free_subtree(c);
                } else {
                    out.push(c);
                }
            }
            return;
        }
        let parent_style = self.tree.b(parent).style.clone();
        let style = self.src.anonymous_style(&parent_style, AnonymousBox::Block);
        let anon = self
            .tree
            .alloc(BoxKind::Block, Level::Block, style, None, BoxData::None);
        let deco = self.tree.b(parent).decorations.clone();
        let kids: Vec<BoxId> = std::mem::take(run);
        self.tree.b_mut(anon).decorations = deco;
        self.tree.b_mut(anon).set(flags::INLINE_CONTENT, true);
        self.tree.set_children(anon, kids);
        out.push(anon);
    }

    /// CSS 2 §9.2.1.1: an inline box containing in-flow block-level boxes is broken around them.
    /// Pushes the pieces (inline boxes, and the block-level boxes between them) to `out`.
    fn split_inline(&mut self, id: BoxId, out: &mut Vec<BoxId>) {
        if self.tree.b(id).kind != BoxKind::Inline || !self.contains_block(id) {
            out.push(id);
            return;
        }
        let children = std::mem::take(&mut self.tree.b_mut(id).children);
        let mut pieces: Vec<BoxId> = Vec::new();
        for c in children {
            self.split_inline(c, &mut pieces);
        }
        let template = {
            let b = self.tree.b(id);
            (
                b.style.clone(),
                b.node,
                b.pseudo,
                b.decorations.clone(),
                b.flags,
            )
        };
        let mut current: Vec<BoxId> = Vec::new();
        let mut inline_pieces: Vec<BoxId> = Vec::new();
        let mut first = true;
        let mut emit_inline = |this: &mut Self, current: &mut Vec<BoxId>, out: &mut Vec<BoxId>| {
            let piece = if first {
                id
            } else {
                let p = this.tree.alloc(
                    BoxKind::Inline,
                    Level::Inline,
                    template.0.clone(),
                    template.1,
                    BoxData::None,
                );
                let pb = this.tree.b_mut(p);
                pb.pseudo = template.2;
                pb.decorations = template.3.clone();
                pb.flags = template.4;
                p
            };
            first = false;
            let kids: Vec<BoxId> = std::mem::take(current);
            this.tree.set_children(piece, kids);
            out.push(piece);
            piece
        };
        for p in pieces {
            if self.tree.b(p).is_in_flow_block_level() {
                if let Some(owner) = template.1 {
                    self.tree.split_owners.entry(p).or_default().push(owner);
                }
                let piece = emit_inline(self, &mut current, out);
                inline_pieces.push(piece);
                // (`id` keeps the element's style: it is the first piece)
                let outer = self.tree.split_inlines.entry(p).or_default();
                if !outer.contains(&id) {
                    outer.push(id);
                }
                out.push(p);
            } else {
                current.push(p);
            }
        }
        let piece = emit_inline(self, &mut current, out);
        inline_pieces.push(piece);
        let n = inline_pieces.len();
        for (i, &p) in inline_pieces.iter().enumerate() {
            let pb = self.tree.b_mut(p);
            pb.set(
                flags::NO_START_EDGE,
                i > 0 || pb.has(flags::NO_START_EDGE) && i == 0,
            );
            pb.set(
                flags::NO_END_EDGE,
                i + 1 < n || pb.has(flags::NO_END_EDGE) && i + 1 == n,
            );
        }
    }

    fn contains_block(&self, id: BoxId) -> bool {
        self.tree.b(id).children.iter().any(|&c| {
            let b = self.tree.b(c);
            b.is_in_flow_block_level() || (b.kind == BoxKind::Inline && self.contains_block(c))
        })
    }

    // ----- flex and grid -----

    fn fixup_flex_grid(&mut self, id: BoxId, raw: Vec<BoxId>) {
        let mut children = Vec::with_capacity(raw.len());
        let mut run: Vec<BoxId> = Vec::new();
        for c in raw {
            let b = self.tree.b(c);
            let text_like = matches!(b.kind, BoxKind::Text | BoxKind::LineBreak | BoxKind::Inline);
            if text_like {
                run.push(c);
            } else {
                self.flush_text_run(id, &mut run, &mut children);
                let b = self.tree.b_mut(c);
                if !b.is_out_of_flow() {
                    b.set(flags::FLEX_GRID_ITEM, true);
                    b.set(flags::INDEPENDENT, true);
                    if b.level != Level::Block {
                        b.level = Level::Block;
                    }
                }
                children.push(c);
            }
        }
        self.flush_text_run(id, &mut run, &mut children);
        self.tree.set_children(id, children);
    }

    /// Wraps a run of text in an anonymous block flex/grid item, unless it is all white space.
    fn flush_text_run(&mut self, parent: BoxId, run: &mut Vec<BoxId>, out: &mut Vec<BoxId>) {
        if run.is_empty() {
            return;
        }
        // css-flexbox-1 §4, css-grid-2 §6: a run of only white space (the characters `white-space`
        // can affect) is not rendered, whatever its `white-space`.
        let all_ws = run.iter().all(|&c| {
            let b = self.tree.b(c);
            matches!(&b.data, BoxData::Text(t)
                if t.bytes().all(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r')))
        });
        if all_ws {
            for c in run.drain(..) {
                self.tree.free_subtree(c);
            }
            return;
        }
        let parent_style = self.tree.b(parent).style.clone();
        let style = self.src.anonymous_style(&parent_style, AnonymousBox::Block);
        let anon = self
            .tree
            .alloc(BoxKind::Block, Level::Block, style, None, BoxData::None);
        let kids: Vec<BoxId> = std::mem::take(run);
        let deco = self.tree.b(parent).decorations.clone();
        let ab = self.tree.b_mut(anon);
        ab.decorations = deco;
        ab.set(flags::INLINE_CONTENT, true);
        ab.set(flags::FLEX_GRID_ITEM, true);
        ab.set(flags::INDEPENDENT, true);
        self.tree.set_children(anon, kids);
        out.push(anon);
    }

    // ----- tables -----

    fn is_table_part(&self, id: BoxId) -> bool {
        let b = self.tree.b(id);
        b.level == Level::Internal
            && matches!(
                b.kind,
                BoxKind::TableRowGroup(_)
                    | BoxKind::TableRow
                    | BoxKind::TableCell
                    | BoxKind::TableColumn
                    | BoxKind::TableColumnGroup
                    | BoxKind::TableCaption
            )
    }

    /// CSS Tables 3 §3.2 step 3: runs of table-internal boxes in a non-table parent get an anonymous
    /// table (an inline-table inside an inline box).
    fn wrap_misparented_table_parts(
        &mut self,
        parent: BoxId,
        raw: Vec<BoxId>,
        inline: bool,
    ) -> Vec<BoxId> {
        if !raw.iter().any(|&c| self.is_table_part(c)) {
            return raw;
        }
        let mut out = Vec::with_capacity(raw.len());
        let mut run: Vec<BoxId> = Vec::new();
        let mut pending_ws: Vec<BoxId> = Vec::new();
        let flush = |this: &mut Self, run: &mut Vec<BoxId>, out: &mut Vec<BoxId>| {
            if run.is_empty() {
                return;
            }
            let parent_style = this.tree.b(parent).style.clone();
            let style = this
                .src
                .anonymous_style(&parent_style, AnonymousBox::Table { inline });
            let level = if inline { Level::Inline } else { Level::Block };
            let wrapper = this.tree.alloc(
                BoxKind::TableWrapper,
                level,
                style.clone(),
                None,
                BoxData::None,
            );
            let table = this
                .tree
                .alloc(BoxKind::Table, Level::Block, style, None, BoxData::None);
            this.tree.b_mut(wrapper).set(flags::INDEPENDENT, true);
            this.tree.b_mut(table).set(flags::INDEPENDENT, true);
            let kids: Vec<BoxId> = std::mem::take(run);
            this.finish_table(wrapper, table, kids);
            out.push(wrapper);
        };
        for c in raw {
            if self.is_table_part(c) {
                // White space between table parts is dropped.
                for w in pending_ws.drain(..) {
                    self.tree.free_subtree(w);
                }
                run.push(c);
            } else if !run.is_empty()
                && self.tree.b(c).kind == BoxKind::Text
                && self.is_collapsible_whitespace(c)
            {
                pending_ws.push(c);
            } else {
                flush(self, &mut run, &mut out);
                out.append(&mut pending_ws);
                out.push(c);
            }
        }
        flush(self, &mut run, &mut out);
        out.append(&mut pending_ws);
        out
    }

    /// Fills a table wrapper with the table's captions and the table box (whose children are fixed up).
    fn finish_table(&mut self, wrapper: BoxId, table: BoxId, raw: Vec<BoxId>) {
        let mut captions = Vec::new();
        let mut rest = Vec::with_capacity(raw.len());
        for c in raw {
            if self.tree.b(c).kind == BoxKind::TableCaption {
                captions.push(c);
            } else {
                rest.push(c);
            }
        }
        let rest = self.fixup_table_children(table, rest);
        self.tree.set_children(table, rest);
        self.tree.table_parts_in_table_frame(table);
        captions.push(table);
        self.tree.set_children(wrapper, captions);
    }

    /// The table box's children: white space removed, runs of non-proper children wrapped in rows.
    fn fixup_table_children(&mut self, table: BoxId, raw: Vec<BoxId>) -> Vec<BoxId> {
        let mut out = Vec::with_capacity(raw.len());
        let mut run: Vec<BoxId> = Vec::new();
        for c in raw {
            let kind = self.tree.b(c).kind;
            let proper = self.tree.b(c).level == Level::Internal
                && matches!(
                    kind,
                    BoxKind::TableRowGroup(_)
                        | BoxKind::TableRow
                        | BoxKind::TableColumnGroup
                        | BoxKind::TableColumn
                        | BoxKind::TableCaption
                );
            if proper {
                self.flush_row(table, &mut run, &mut out);
                out.push(c);
            } else if kind == BoxKind::Text && self.is_collapsible_whitespace(c) {
                self.tree.free_subtree(c);
            } else {
                run.push(c);
            }
        }
        self.flush_row(table, &mut run, &mut out);
        out
    }

    fn fixup_row_group(&mut self, group: BoxId, raw: Vec<BoxId>) -> Vec<BoxId> {
        let mut out = Vec::with_capacity(raw.len());
        let mut run: Vec<BoxId> = Vec::new();
        for c in raw {
            let b = self.tree.b(c);
            if b.kind == BoxKind::TableRow && b.level == Level::Internal {
                self.flush_row(group, &mut run, &mut out);
                out.push(c);
            } else if b.kind == BoxKind::Text && self.is_collapsible_whitespace(c) {
                self.tree.free_subtree(c);
            } else {
                run.push(c);
            }
        }
        self.flush_row(group, &mut run, &mut out);
        out
    }

    /// Wraps a run of non-row children in an anonymous row.
    fn flush_row(&mut self, parent: BoxId, run: &mut Vec<BoxId>, out: &mut Vec<BoxId>) {
        if run.is_empty() {
            return;
        }
        let parent_style = self.tree.b(parent).style.clone();
        let style = self
            .src
            .anonymous_style(&parent_style, AnonymousBox::TableRow);
        let row = self.tree.alloc(
            BoxKind::TableRow,
            Level::Internal,
            style,
            None,
            BoxData::None,
        );
        let kids: Vec<BoxId> = std::mem::take(run);
        let kids = self.fixup_row(row, kids);
        self.tree.set_children(row, kids);
        out.push(row);
    }

    fn fixup_row(&mut self, row: BoxId, raw: Vec<BoxId>) -> Vec<BoxId> {
        let mut out = Vec::with_capacity(raw.len());
        let mut run: Vec<BoxId> = Vec::new();
        for c in raw {
            let b = self.tree.b(c);
            if b.kind == BoxKind::TableCell && b.level == Level::Internal {
                self.flush_cell(row, &mut run, &mut out);
                out.push(c);
            } else if b.kind == BoxKind::Text && self.is_collapsible_whitespace(c) && run.is_empty()
            {
                self.tree.free_subtree(c);
            } else {
                run.push(c);
            }
        }
        self.flush_cell(row, &mut run, &mut out);
        out
    }

    /// Wraps a run of non-cell children in an anonymous cell.
    fn flush_cell(&mut self, row: BoxId, run: &mut Vec<BoxId>, out: &mut Vec<BoxId>) {
        // Trailing white space in the run is dropped.
        while let Some(&last) = run.last() {
            if self.tree.b(last).kind == BoxKind::Text && self.is_collapsible_whitespace(last) {
                run.pop();
                self.tree.free_subtree(last);
            } else {
                break;
            }
        }
        if run.is_empty() {
            return;
        }
        let parent_style = self.tree.b(row).style.clone();
        let style = self
            .src
            .anonymous_style(&parent_style, AnonymousBox::TableCell);
        let cell = self.tree.alloc(
            BoxKind::TableCell,
            Level::Internal,
            style,
            None,
            BoxData::Cell {
                colspan: 1,
                rowspan: 1,
            },
        );
        self.tree.b_mut(cell).set(flags::INDEPENDENT, true);
        let kids: Vec<BoxId> = std::mem::take(run);
        self.fixup_block_container(cell, kids);
        out.push(cell);
    }

    /// HTML `colspan` (1..=1000) and `rowspan` (0..=65534, 0 meaning "to the end of the group").
    fn spans(&self, node: Option<NodeId>) -> (u32, u32) {
        let Some(n) = node else {
            return (1, 1);
        };
        if !(self.dom.is_html_element_named(n, &local_name!("td"))
            || self.dom.is_html_element_named(n, &local_name!("th")))
        {
            return (1, 1);
        }
        let parse = |name: &str| self.dom.get_attribute(n, name).and_then(parse_non_negative);
        let colspan = parse("colspan").map_or(1, |v| v.clamp(1, 1000));
        let rowspan = parse("rowspan").map_or(1, |v| v.min(65534));
        (colspan, rowspan)
    }

    fn column_span(&self, node: Option<NodeId>) -> u32 {
        let Some(n) = node else {
            return 1;
        };
        let is_col = self.dom.is_html_element_named(n, &local_name!("col"))
            || self.dom.is_html_element_named(n, &local_name!("colgroup"));
        if !is_col {
            return 1;
        }
        self.dom
            .get_attribute(n, "span")
            .and_then(parse_non_negative)
            .map_or(1, |v| v.clamp(1, 1000))
    }

    // ----- lists -----

    fn enter_list(&mut self, el: NodeId) {
        if !self.is_list_owner(el) {
            return;
        }
        let reversed = self.dom.is_html_element_named(el, &local_name!("ol"))
            && self.dom.has_attribute(el, "reversed");
        let start = self
            .dom
            .get_attribute(el, "start")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|_| self.dom.is_html_element_named(el, &local_name!("ol")));
        let (next, step) = if reversed {
            let count = self
                .dom
                .element_children(el)
                .filter(|&c| self.dom.is_html_element_named(c, &local_name!("li")))
                .count() as i64;
            (start.unwrap_or(count), -1)
        } else {
            (start.unwrap_or(1), 1)
        };
        self.counters.push(ListCounter { next, step });
    }

    fn leave_list(&mut self, el: NodeId) {
        if self.is_list_owner(el) {
            self.counters.pop();
        }
    }

    fn is_list_owner(&self, el: NodeId) -> bool {
        let Some(e) = self.dom.element(el) else {
            return false;
        };
        e.name().ns == ns!(html)
            && matches!(
                e.name().local,
                local_name!("ol") | local_name!("ul") | local_name!("menu") | local_name!("dir")
            )
    }

    /// The list item's ordinal value (HTML §4.4.8).
    fn ordinal(&mut self, el: NodeId) -> i64 {
        if self.counters.is_empty() {
            self.counters.push(ListCounter { next: 1, step: 1 });
        }
        let value = if self.dom.is_html_element_named(el, &local_name!("li")) {
            self.dom
                .get_attribute(el, "value")
                .and_then(|v| v.trim().parse::<i64>().ok())
        } else {
            None
        };
        let c = self.counters.last_mut().unwrap();
        let n = value.unwrap_or(c.next);
        c.next = n + c.step;
        n
    }

    /// The ordinal of list item `el` computed from its list owner (for rebuilding it alone).
    fn ordinal_in_place(&mut self, el: NodeId) -> i64 {
        let owner = self.dom.ancestors(el).find(|&a| self.is_list_owner(a));
        let Some(owner) = owner else {
            self.counters.push(ListCounter { next: 1, step: 1 });
            let n = self.ordinal(el);
            self.counters.pop();
            return n;
        };
        self.enter_list(owner);
        let mut n = 1;
        let mut child = self.dom.first_child(owner);
        while let Some(c) = child {
            if self.dom.is_element(c) {
                let is_item = c == el
                    || self.src.element_style(c).is_some_and(|s| {
                        matches!(
                            s.display(),
                            Display::Box {
                                list_item: true,
                                ..
                            }
                        )
                    });
                if is_item {
                    n = self.ordinal(c);
                }
                if c == el {
                    break;
                }
            }
            child = self.dom.next_sibling(c);
        }
        self.leave_list(owner);
        n
    }

    /// The marker's content boxes, or none.
    fn marker_content(
        &mut self,
        el: NodeId,
        ordinal: i64,
        item_style: &Src::Style,
        deco: &Decorations,
    ) -> Option<(Src::Style, Vec<BoxId>)> {
        if Self::no_marker(self.src, item_style) {
            return None;
        }
        let style = self.src.pseudo_style(el, PseudoElement::Marker)?;
        let mut kids = Vec::new();
        match style.content() {
            Content::None => return None,
            Content::Items(items) => {
                for item in &items {
                    if let Some(b) = self.generated_item(item, &style, deco) {
                        kids.push(b);
                    }
                }
            }
            Content::Normal => {
                if let Image::Raster { id, natural } = item_style.list_style_image() {
                    // CSS Lists 3 §3.1: a marker image's default object size is a 1em square.
                    let em = style.font_size();
                    let (width, height) = natural.concrete((None, None), (em, em));
                    let s = self.src.anonymous_style(&style, AnonymousBox::Block);
                    let b = self.tree.alloc(
                        BoxKind::Replaced,
                        Level::Inline,
                        s,
                        None,
                        BoxData::Replaced(ReplacedContent::image(id, width, height)),
                    );
                    self.tree.b_mut(b).set(flags::INDEPENDENT, true);
                    kids.push(b);
                } else {
                    let text = marker_text(&item_style.list_style_type(), ordinal)?;
                    kids.push(self.tree.alloc(
                        BoxKind::Text,
                        Level::Inline,
                        style.clone(),
                        None,
                        BoxData::Text(text),
                    ));
                }
            }
        }
        if kids.is_empty() {
            return None;
        }
        Some((style, kids))
    }

    /// An inside marker: an inline box at the start of the item's content.
    fn list_marker_inline(
        &mut self,
        el: NodeId,
        ordinal: i64,
        item_style: &Src::Style,
        deco: &Decorations,
        out: &mut Vec<BoxId>,
    ) {
        let Some((style, kids)) = self.marker_content(el, ordinal, item_style, deco) else {
            return;
        };
        let m = self
            .tree
            .alloc(BoxKind::Inline, Level::Inline, style, None, BoxData::None);
        self.tree.b_mut(m).pseudo = Some(PseudoElement::Marker);
        self.tree.set_children(m, kids);
        out.push(m);
    }

    /// An outside marker, held on the item rather than among its children.
    fn outside_marker(
        &mut self,
        item: BoxId,
        el: NodeId,
        ordinal: i64,
        item_style: &Src::Style,
        deco: &Decorations,
    ) {
        let Some((style, kids)) = self.marker_content(el, ordinal, item_style, deco) else {
            return;
        };
        let m = self
            .tree
            .alloc(BoxKind::Block, Level::Inline, style, None, BoxData::None);
        let mb = self.tree.b_mut(m);
        mb.pseudo = Some(PseudoElement::Marker);
        mb.set(flags::OUTSIDE_MARKER, true);
        mb.set(flags::INDEPENDENT, true);
        mb.set(flags::INLINE_CONTENT, true);
        self.tree.set_children(m, kids);
        self.tree.b_mut(m).parent = Some(item);
        self.tree.b_mut(item).marker = Some(m);
    }
}

/// Whether text in `style` is only white space that collapses away (it generates no boxes between
/// blocks, CSS 2 §9.2.1.1 with CSS Text 3 §4.1.1).
pub(crate) fn is_collapsible_whitespace<S: Style>(style: &S, t: &str) -> bool {
    match style.white_space_collapse() {
        WhiteSpaceCollapse::Collapse => {
            t.bytes().all(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
        }
        WhiteSpaceCollapse::PreserveBreaks => t.bytes().all(|c| matches!(c, b' ' | b'\t')),
        _ => false,
    }
}

/// HTML's rules for parsing non-negative integers (§2.3.4.1), leniently: leading digits.
fn parse_non_negative(v: &str) -> Option<u32> {
    let v = v.trim_start_matches([' ', '\t', '\n', '\r', '\u{c}']);
    let v = v.strip_prefix('+').unwrap_or(v);
    let digits: &str = &v[..v.bytes().take_while(u8::is_ascii_digit).count()];
    if digits.is_empty() {
        return None;
    }
    Some(
        digits
            .parse::<u64>()
            .map_or(u32::MAX, |n| n.min(u32::MAX as u64) as u32),
    )
}

/// The marker string for an ordinal in a counter style (CSS Counter Styles 3 §6–7), with its suffix.
pub(crate) fn marker_text(style: &ListStyleType, n: i64) -> Option<String> {
    let symbolic = |s: &str| Some(format!("{s} "));
    match style {
        ListStyleType::None => None,
        ListStyleType::Disc => symbolic("\u{2022}"),
        ListStyleType::Circle => symbolic("\u{25E6}"),
        ListStyleType::Square => symbolic("\u{25AA}"),
        ListStyleType::DisclosureOpen => symbolic("\u{25BE}"),
        ListStyleType::DisclosureClosed => symbolic("\u{25B8}"),
        ListStyleType::Decimal => Some(format!("{n}. ")),
        ListStyleType::DecimalLeadingZero => Some(if (0..10).contains(&n) {
            format!("0{n}. ")
        } else {
            format!("{n}. ")
        }),
        ListStyleType::LowerAlpha => Some(format!("{}. ", alphabetic(n, b'a'))),
        ListStyleType::UpperAlpha => Some(format!("{}. ", alphabetic(n, b'A'))),
        ListStyleType::LowerRoman => Some(format!("{}. ", roman(n).to_lowercase())),
        ListStyleType::UpperRoman => Some(format!("{}. ", roman(n))),
        ListStyleType::LowerGreek => Some(format!("{}. ", greek(n))),
        ListStyleType::String(s) => Some(s.to_string()),
    }
}

/// The alphabetic counter system (§6.4); out of range (< 1) falls back to decimal.
fn alphabetic(n: i64, base: u8) -> String {
    if n < 1 {
        return n.to_string();
    }
    let mut n = n;
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(base + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// The additive roman numeral system (1..=3999, else decimal).
fn roman(n: i64) -> String {
    if !(1..=3999).contains(&n) {
        return n.to_string();
    }
    const TABLE: [(i64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut n = n;
    let mut s = String::new();
    for (v, r) in TABLE {
        while n >= v {
            s.push_str(r);
            n -= v;
        }
    }
    s
}

fn greek(n: i64) -> String {
    const LETTERS: [char; 24] = [
        'α', 'β', 'γ', 'δ', 'ε', 'ζ', 'η', 'θ', 'ι', 'κ', 'λ', 'μ', 'ν', 'ξ', 'ο', 'π', 'ρ', 'σ',
        'τ', 'υ', 'φ', 'χ', 'ψ', 'ω',
    ];
    if n < 1 {
        return n.to_string();
    }
    let mut n = n;
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(LETTERS[(n % 24) as usize]);
        n /= 24;
    }
    out.iter().rev().collect()
}
