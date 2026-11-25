// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Anchor Positioning 1 in layout: the target anchor element of an absolutely positioned box
//! (§2.3) and where its box is. An anchor is laid out before the boxes that may refer to it (that is
//! what makes it acceptable), so when a containing block places its out-of-flow boxes, every
//! acceptable anchor's fragments are among the fragments it has placed already; the anchor's border
//! box is found there, relative to the containing block.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use snowlark_paint::{Point, Rect, Size};

use crate::boxes::*;
use crate::fragment::{FragmentChild, FragmentKind};
use crate::layout::containing_block_in;
use crate::style::*;

/// The boxes that have anchor names in one layout pass, by name, in tree order.
#[derive(Default)]
pub(crate) struct AnchorIndex {
    by_name: FxHashMap<Box<str>, Vec<BoxId>>,
}

impl AnchorIndex {
    pub fn build<S: Style>(tree: &BoxTree<S>) -> AnchorIndex {
        let mut by_name: FxHashMap<Box<str>, Vec<(u32, BoxId)>> = FxHashMap::default();
        for (index, slot) in tree.boxes.iter().enumerate() {
            let Some(b) = slot else {
                continue;
            };
            // (an element's principal box: its table box shares the wrapper's style)
            if b.is_anonymous()
                || matches!(b.kind, BoxKind::Table | BoxKind::Text | BoxKind::LineBreak)
                || !b.style.has_anchor_name()
            {
                continue;
            }
            let id = BoxId {
                index: index as u32,
                generation: b.generation,
            };
            let order = b.order;
            b.style.anchor_names(&mut |name| {
                by_name.entry(name.into()).or_default().push((order, id));
            });
        }
        AnchorIndex {
            by_name: by_name
                .into_iter()
                .map(|(name, mut boxes)| {
                    boxes.sort_unstable_by_key(|&(order, _)| order);
                    (name, boxes.into_iter().map(|(_, id)| id).collect())
                })
                .collect(),
        }
    }
}

/// [`AnchorQuery`] for the box `pos` being placed by its containing block `cb`, whose padding box
/// is `padding_box` and which has placed `placed` (in its fragment's coordinates).
pub(crate) struct Lookup<'t, S: Style> {
    pub tree: &'t BoxTree<S>,
    pub index: &'t std::cell::OnceCell<AnchorIndex>,
    pub pos: BoxId,
    pub cb: Option<BoxId>,
    pub padding_box: Rect,
    pub placed: &'t [FragmentChild<S>],
    /// The flow frame padding_box and placed are in (see [crate::logical]): anchor boxes
    /// are given in physical coordinates.
    pub frame: Flip,
}

impl<S: Style> Lookup<'_, S> {
    fn in_top_layer(&self, id: BoxId) -> bool {
        self.tree.b(id).has(flags::TOP_LAYER)
    }

    /// §2.3: `anchor` is laid out strictly before the positioned box: it has the same containing
    /// block and is in a lower top layer, or in the same one and not absolutely positioned or
    /// earlier in tree order; or the box generating its containing block is acceptable.
    fn acceptable(&self, anchor: BoxId) -> bool {
        let pos = self.tree.b(self.pos);
        let mut el = anchor;
        loop {
            if el == self.pos {
                return false;
            }
            let b = self.tree.b(el);
            // (as if it were absolutely positioned: the static boxes between an element and its
            // containing block do not matter here)
            let cb = containing_block_in(self.tree, el);
            if cb == self.cb {
                let (a_top, p_top) = (self.in_top_layer(el), self.in_top_layer(self.pos));
                if a_top != p_top {
                    return !a_top;
                }
                return !b.style.position().is_out_of_flow() || b.order < pos.order;
            }
            match cb {
                Some(c) => el = c,
                None => return false,
            }
        }
    }

    /// The target anchor element for a name (§2.3): the nearest acceptable ancestor with the name,
    /// else the last acceptable box with it in tree order.
    fn named(&self, name: &str) -> Option<BoxId> {
        let index = self.index.get_or_init(|| AnchorIndex::build(self.tree));
        let boxes = index.by_name.get(name)?;
        let mut cur = self.tree.b(self.pos).parent;
        while let Some(c) = cur {
            if boxes.contains(&c) && self.acceptable(c) {
                return Some(c);
            }
            cur = self.tree.b(c).parent;
        }
        boxes.iter().rev().copied().find(|&b| self.acceptable(b))
    }

    /// The implicit anchor element (§2.4.1): the style's, or a pseudo-element's originating
    /// element, when it is acceptable.
    fn implicit(&self) -> Option<BoxId> {
        let b = self.tree.b(self.pos);
        let node = b.style.implicit_anchor().or(b.pseudo_of)?;
        let id = self.tree.box_of(node)?;
        self.acceptable(id).then_some(id)
    }

    /// The border box of `target` among the placed fragments (the bounding box of its pieces),
    /// relative to the containing block's padding box.
    fn border_box(&self, target: BoxId) -> Option<Rect> {
        let mut ancestors: SmallVec<[BoxId; 16]> = SmallVec::new();
        let mut cur = self.tree.b(target).parent;
        while let Some(c) = cur {
            ancestors.push(c);
            cur = self.tree.b(c).parent;
        }
        let mut found = None;
        find(self.placed, Point::ZERO, target, &ancestors, &mut found);
        let within = Size::new(self.padding_box.width, self.padding_box.height);
        found.map(|r: Rect| {
            self.frame.rect(
                r.translate(-self.padding_box.x, -self.padding_box.y),
                within,
            )
        })
    }
}

fn find<S: Style>(
    children: &[FragmentChild<S>],
    at: Point,
    target: BoxId,
    ancestors: &[BoxId],
    found: &mut Option<Rect>,
) {
    for c in children {
        let o = Point::new(at.x + c.offset.x, at.y + c.offset.y);
        let f = &c.fragment;
        let descend = match &f.kind {
            FragmentKind::Box(b) if b.box_id == Some(target) => {
                let r = Rect::new(o.x, o.y, f.size.width, f.size.height);
                *found = Some(match *found {
                    Some(u) => Rect::from_ltrb(
                        u.x.min(r.x),
                        u.y.min(r.y),
                        (u.x + u.width).max(r.x + r.width),
                        (u.y + u.height).max(r.y + r.height),
                    ),
                    None => r,
                });
                false
            }
            FragmentKind::Box(b) => b.box_id.is_none_or(|id| ancestors.contains(&id)),
            FragmentKind::Line { .. } => true,
            FragmentKind::Text(_) => false,
        };
        if descend {
            find(&f.children, o, target, ancestors, found);
        }
    }
}

impl<S: Style> AnchorQuery<S> for Lookup<'_, S> {
    fn anchor(&self, spec: AnchorSpec<'_>) -> Option<Rect> {
        let target = match spec {
            AnchorSpec::Name(name) => self.named(name)?,
            AnchorSpec::Implicit => self.implicit()?,
        };
        self.border_box(target)
    }

    fn containing_block(&self) -> Size {
        self.frame
            .size(Size::new(self.padding_box.width, self.padding_box.height))
    }

    fn containing_style(&self) -> Option<&S> {
        let cb = self.cb.or(self.tree.root)?;
        Some(&self.tree.b(cb).style)
    }
}
