// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The fragment tree: layout's immutable output (as LayoutNG's physical fragments). A [`Fragment`] has a
//! size, its children with their offsets ([`FragmentChild`]), its scrollable overflow and baselines, and
//! what it is: a box fragment (with the style handle and DOM node), a line box, or a run of shaped text.
//!
//! Positions are relative: a child's offset is from its parent fragment's border-box origin, so a
//! fragment can be reused unchanged wherever its parent places it. Fragments are shared through `Arc`
//! between consecutive layouts (an unchanged subtree's fragments are the same allocations).

use std::ops::Range;
use std::sync::Arc;

use snowlark_dom::NodeId;
use snowlark_paint::{Point, Rect, ShapedText, Sides, Size};

use crate::boxes::{BoxId, Decoration};
use crate::style::{
    BoxArea, ContainingBoxes, Direction, Display, DisplayInside, FlexDirection, FlexWrap, Overflow,
    PseudoElement, ReplacedContent, Style, WritingMode,
};

/// A positioned child.
pub struct FragmentChild<S: Style> {
    /// The child's border-box origin in the parent's border-box coordinates (relative positioning
    /// included).
    pub offset: Point,
    pub fragment: Arc<Fragment<S>>,
    /// For an absolutely or fixed positioned child placed here by its containing block: the box
    /// order ([`BoxFragment::order`]) of the stacking context it belongs to when that is a
    /// descendant of the containing block (CSS 2 Appendix E: positioned descendants are painted
    /// by their nearest ancestor stacking context, which need not contain their containing
    /// block); 0 for the stacking context the containing block is in.
    pub stacking_owner: u32,
}

impl<S: Style> Clone for FragmentChild<S> {
    fn clone(&self) -> Self {
        FragmentChild {
            offset: self.offset,
            fragment: self.fragment.clone(),
            stacking_owner: self.stacking_owner,
        }
    }
}

/// One fragment.
pub struct Fragment<S: Style> {
    /// The border-box size.
    pub size: Size,
    pub kind: FragmentKind<S>,
    pub children: Vec<FragmentChild<S>>,
    /// Scrollable overflow (css-overflow-3 §2.2) in this fragment's coordinates: its border box united
    /// with what its descendants contribute.
    pub overflow: Rect,
    /// Where the subtree paints, in this fragment's coordinates: its border box united with its
    /// children's paint bounds, through their transforms and not cut by clips (what painting a part
    /// of the page, or finding what is under a point, compares to skip the subtree).
    pub paint_bounds: Rect,
    /// The bounds of the in-flow content (CSS Overflow 3 §2.2), in this fragment's coordinates:
    /// for a box, the margin boxes of its in-flow block-level children (at their positions before
    /// relative positioning, through the block-end margins that collapse inside it) or its line
    /// boxes, or its flex or grid items' margin boxes; for a line box, the extent of its content
    /// with the margins of atomic inlines. A scroll container's scrollable overflow includes it
    /// with the container's padding around it (the end padding). `None`: no in-flow content.
    pub inflow: Option<Rect>,
    /// The first baseline, from the border-box top.
    pub baseline: Option<f32>,
    /// The last baseline, from the border-box top.
    pub last_baseline: Option<f32>,
}

/// What a fragment is.
// (most fragments are box fragments: boxing the variant would add an allocation to each)
#[allow(clippy::large_enum_variant)]
pub enum FragmentKind<S: Style> {
    Box(BoxFragment<S>),
    /// A line box (CSS 2 §9.4.2); `baseline` from its top.
    Line {
        baseline: f32,
    },
    Text(TextFragment<S>),
}

/// Flags on box fragments.
pub mod fragment_flags {
    pub const FLOAT: u32 = 1;
    pub const ABSOLUTE: u32 = 2;
    pub const FIXED: u32 = 4;
    /// An inline box's piece on one line.
    pub const INLINE_BOX: u32 = 8;
    /// An atomic inline (inline-block, inline replaced, inline-table, …).
    pub const ATOMIC_INLINE: u32 = 16;
    /// The root element's box.
    pub const ROOT: u32 = 32;
    /// Clips its content horizontally (`overflow-x` other than visible).
    pub const CLIP_X: u32 = 64;
    pub const CLIP_Y: u32 = 128;
    /// A scroll container (`hidden`, `scroll`, `auto`).
    pub const SCROLL_CONTAINER: u32 = 256;
    pub const LIST_MARKER: u32 = 512;
    pub const TABLE_CELL: u32 = 1024;
    pub const TABLE: u32 = 2048;
    /// Its background paints the canvas instead (CSS Backgrounds 3 §2.11.2).
    pub const BACKGROUND_TO_CANVAS: u32 = 4096;
    /// Empty cell with `empty-cells: hide`: no background or borders.
    pub const HIDE_EMPTY: u32 = 8192;
    /// Table wrapper box (paints nothing itself).
    pub const TABLE_WRAPPER: u32 = 16384;
    /// A table row or row group, column or column group (backgrounds painted under cells).
    pub const TABLE_PART: u32 = 32768;
    /// Positioned (`position` other than static).
    pub const POSITIONED: u32 = 65536;
    /// `position: sticky` (offsets resolved at paint time against the scrollport).
    pub const STICKY: u32 = 131072;
    /// A cell in the collapsing border model: its border is painted by the table.
    pub const NO_BORDER_PAINT: u32 = 262144;
    /// The initial containing block (paints nothing itself).
    pub const INITIAL_CONTAINING_BLOCK: u32 = 524288;
    /// A flex or grid item (`z-index` applies without positioning, css-flexbox-1 §4.3).
    pub const FLEX_GRID_ITEM: u32 = 1048576;
    /// A `fieldset` (HTML §15.3.13): its block-start border is painted through its rendered legend's
    /// middle, interrupted behind the legend.
    pub const FIELDSET: u32 = 2097152;
    /// A fieldset's rendered legend.
    pub const RENDERED_LEGEND: u32 = 4194304;
    /// The `<body>` whose `overflow` applies to the viewport (CSS Overflow 3 §3.3; the root
    /// element's always does, see [`ROOT`]): its scroll container is the viewport's.
    pub const VIEWPORT_OVERFLOW: u32 = 8388608;
    /// A top layer element's box or its `::backdrop` (CSS Position 4 §4.1): painted after
    /// everything else in the root stacking context, in top layer order.
    pub const TOP_LAYER: u32 = 16777216;
    /// Where a spanner (`column-span: all`, CSS Multi-column Layout 1 §6) is in a multi-column
    /// container's flow: an empty fragment of it, which the container replaces with the spanner
    /// laid out across its columns.
    pub const SPANNER: u32 = 33554432;
    /// A box of a multi-column container's flow with a spanner inside.
    pub const HAS_SPANNER: u32 = 67108864;
    /// A line-clamp container whose content was clamped (CSS Overflow 4 §4.2): what remains of
    /// its floats past its block-end content edge is clipped there (and is not scrollable
    /// overflow).
    pub const LINE_CLAMPED: u32 = 268435456;
}

/// A box fragment.
pub struct BoxFragment<S: Style> {
    pub style: S,
    /// The generating box (whose style `style` is, read through its view): a paint-only style
    /// change gives the box's fragments its new style without layout ([`refresh_styles`]).
    pub(crate) box_id: Option<BoxId>,
    /// The generating DOM node (`None` for anonymous boxes).
    pub node: Option<NodeId>,
    pub pseudo: Option<PseudoElement>,
    /// Used border widths (zero on the sliced sides of an inline box piece; collapsed-border halves
    /// for cells in the collapsing model).
    pub border: Sides,
    pub padding: Sides,
    pub flags: u32,
    /// Pre-order position of the generating box in the box tree.
    pub order: u32,
    /// The pre-order position of the last box in the generating box's subtree (its descendants
    /// are the boxes numbered `order..=subtree_end`).
    pub subtree_end: u32,
    pub content: BoxContent,
    /// Its containing block's boxes, when its transform needs them (`offset-path`): set by the
    /// parent fragment, which knows where it placed it. A fragment reused under another parent gets
    /// new ones, so this is not part of the fragment's value.
    pub containing: ContainingCell,
    /// The reference boxes of its `clip-path` when they are not its own, relative to this
    /// fragment: an inline box over several lines clips every piece to its first piece's boxes
    /// (CSS Masking 1 does not say; what browsers do). Set by the inline layout that placed the
    /// pieces, after it placed them.
    pub clip_reference: ClipReferenceCell,
    /// On a piece of an inline box on several lines that has background images: the border,
    /// padding and content boxes of all its pieces side by side as one box, relative to this
    /// fragment, where the images are positioned (CSS Backgrounds 3 §3.9, CSS Fragmentation 3
    /// §5.4: `box-decoration-break: slice`). Set by the inline layout that placed the pieces.
    pub strip: StripCell,
    /// On the first piece of a box that block fragmentation split (a multi-column container's
    /// columns): the box's scrollable overflow in its own coordinates as if it were not split, which
    /// is what its scroll size measures (the pieces stitched together).
    pub stitched: Option<Box<Rect>>,
}

impl<S: Style> BoxFragment<S> {
    /// The box's transform (`transform`, `translate`, `rotate`, `scale`, `offset-*`) at `size`.
    pub fn used_transform(&self, size: Size) -> Option<snowlark_paint::Matrix> {
        if !self.style.has_transform() {
            return None;
        }
        let mut reference = snowlark_paint::Rect::new(0.0, 0.0, size.width, size.height);
        if self.style.transform_box_is_content() {
            reference = reference.deflate(&self.border).deflate(&self.padding);
        }
        if !self.style.transform_needs_containing() {
            return self.style.transform(reference);
        }
        self.style
            .transform_in(reference, self.containing.get().as_ref())
    }
}

/// See [`BoxFragment::containing`].
#[derive(Debug, Default)]
pub struct ContainingCell(std::sync::RwLock<Option<ContainingBoxes>>);

impl ContainingCell {
    pub fn get(&self) -> Option<ContainingBoxes> {
        *self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    /// A cell of its own with the same boxes.
    pub fn copied(&self) -> ContainingCell {
        ContainingCell(std::sync::RwLock::new(self.get()))
    }

    pub fn set(&self, boxes: ContainingBoxes) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(boxes);
    }
}

/// A box's reference boxes for `clip-path` (CSS Masking 1 §5.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReferenceBoxes {
    pub border: Rect,
    pub padding: Rect,
    pub content: Rect,
}

/// See [`BoxFragment::clip_reference`].
#[derive(Debug, Default)]
pub struct ClipReferenceCell(std::sync::RwLock<Option<ReferenceBoxes>>);

impl ClipReferenceCell {
    pub fn get(&self) -> Option<ReferenceBoxes> {
        *self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set(&self, boxes: ReferenceBoxes) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(boxes);
    }

    /// A cell of its own with the same boxes.
    pub fn copied(&self) -> ClipReferenceCell {
        ClipReferenceCell(std::sync::RwLock::new(self.get()))
    }
}

/// See [`BoxFragment::strip`]: the border, padding and content boxes.
#[derive(Debug, Default)]
pub struct StripCell(std::sync::RwLock<Option<[Rect; 3]>>);

impl StripCell {
    pub fn get(&self) -> Option<[Rect; 3]> {
        *self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set(&self, boxes: Option<[Rect; 3]>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = boxes;
    }

    /// A cell of its own with the same boxes.
    pub fn copied(&self) -> StripCell {
        StripCell(std::sync::RwLock::new(self.get()))
    }
}

/// What a box fragment shows besides its children.
#[derive(Clone, Debug, PartialEq)]
pub enum BoxContent {
    None,
    /// A replaced element; `rect` is where the content goes (after `object-fit`), in the
    /// fragment's coordinates, `clip` its content box.
    Replaced {
        content: ReplacedContent,
        rect: Rect,
        clip: Rect,
        /// A form control's laid-out text.
        control: Option<Arc<crate::controls::ControlLayout>>,
        /// How the control's own frame (what `control` is laid out in) maps to the frame this
        /// fragment is in: a vertical control is a horizontal one turned, as vertical text is.
        control_flip: crate::logical::Flip,
    },
    /// A table in the collapsing border model: the resolved border segments to paint.
    CollapsedBorders(Arc<Vec<CollapsedBorderSegment>>),
    /// A multi-column container: the gaps between adjacent columns of each row, where its
    /// column rule is painted (CSS Multi-column Layout 1 §4).
    ColumnGaps(Arc<[ColumnGap]>),
}

/// The gap between two adjacent columns of a row of a multi-column container, in the container
/// fragment's coordinates: across the gap, from the row's block-start to its block-end.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnGap {
    pub rect: Rect,
    /// 1 on the columns' inline-start side, 0 on the others: the side a rule is drawn as (which
    /// way `groove` and `ridge` shade), and across which the gap runs.
    pub side: Sides,
}

/// A painted border segment of a table in the collapsing border model, in the table fragment's
/// coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CollapsedBorderSegment {
    pub rect: Rect,
    pub color: snowlark_paint::Color,
    pub style: snowlark_paint::BorderStyle,
    /// Horizontal (top/bottom border) or vertical.
    pub horizontal: bool,
}

/// A run of shaped text on one line: one bidi run's clusters of one text item.
#[derive(Clone)]
pub struct TextFragment<S: Style> {
    pub style: S,
    /// The box whose style `style` is (see [`BoxFragment::box_id`]).
    pub(crate) box_id: Option<BoxId>,
    pub node: Option<NodeId>,
    pub shaped: Arc<ShapedText>,
    /// The shaped run these clusters belong to (font, size, bidi level).
    pub run: u32,
    pub clusters: Range<u32>,
    /// The baseline, from the fragment's top.
    pub baseline: f32,
    /// Extra advance added after every space cluster (`text-align: justify`).
    pub justify: f32,
    /// `letter-spacing` added after every cluster.
    pub letter_spacing: f32,
    pub decorations: smallvec::SmallVec<[Decoration; 1]>,
    /// The byte range of the clusters in the text box's DOM text (for hit testing and selection).
    pub text_range: Range<u32>,
    /// Where the shaped item's text starts in the DOM text, and where white space collapsing removed
    /// characters ([`TextFragment::dom_offset`] maps the shaped text's offsets back).
    pub dom_start: u32,
    pub collapsed: Option<Arc<[(u32, u32)]>>,
    /// How the text's own frame (x along the line from line-left, y down with the baseline at
    /// [`TextFragment::baseline`] and line-over up) maps to the frame this fragment is in: identity
    /// for horizontal text; after layout, a quarter turn for vertical text (CSS Writing Modes 4).
    pub flip: crate::logical::Flip,
    /// The text aligns on its central baseline (CSS Writing Modes 4 §4.2): [`TextFragment::baseline`]
    /// is halfway through the em box, and the glyphs' alphabetic baseline is below it by half the
    /// difference of the font's ascent and descent.
    pub central: bool,
    /// `text-combine-upright`: the text is one 1em square of horizontal text on a vertical line
    /// (CSS Writing Modes 4 §9.1.2), of this natural width.
    pub combined: Option<f32>,
}

/// A processed (shaped) text offset → its byte offset in the DOM text.
pub(crate) fn dom_offset(dom_start: u32, collapsed: Option<&[(u32, u32)]>, processed: u32) -> u32 {
    let removed = collapsed.map_or(0, |c| {
        let i = c.partition_point(|&(at, _)| at <= processed);
        if i == 0 {
            0
        } else {
            c[i - 1].1
        }
    });
    dom_start + processed + removed
}

impl<S: Style> TextFragment<S> {
    /// The DOM text byte offset of an offset of the shaped text (a cluster boundary). The
    /// fragment's first and last cluster edges are its `text_range`'s ends (for a hyphen at a soft
    /// hyphen, CSS Text 3 §6.1, the hyphenation character's ends are the soft hyphen's).
    pub fn dom_offset(&self, processed: u32) -> u32 {
        let clusters =
            &self.shaped.clusters[self.clusters.start as usize..self.clusters.end as usize];
        match (clusters.first(), clusters.last()) {
            (Some(first), _) if processed <= first.text.0 => self.text_range.start,
            (_, Some(last)) if processed >= last.text.1 => self.text_range.end,
            _ => dom_offset(self.dom_start, self.collapsed.as_deref(), processed),
        }
    }

    /// The shaped text offset (a cluster boundary of this fragment) nearest at or before a DOM text
    /// byte offset, clamped to the fragment's clusters.
    pub fn processed_offset(&self, dom: u32) -> u32 {
        let clusters =
            &self.shaped.clusters[self.clusters.start as usize..self.clusters.end as usize];
        let mut best = clusters.first().map_or(0, |c| c.text.0);
        for c in clusters {
            if self.dom_offset(c.text.0) <= dom {
                best = c.text.0;
            }
            if self.dom_offset(c.text.1) <= dom {
                best = c.text.1;
            }
        }
        best
    }

    /// The x of a shaped text offset from the fragment's start (letter spacing and justification
    /// included).
    pub fn x_of(&self, processed: u32) -> f32 {
        let mut x = 0.0;
        for c in &self.shaped.clusters[self.clusters.start as usize..self.clusters.end as usize] {
            if c.text.0 >= processed {
                break;
            }
            x += c.advance + self.letter_spacing + if c.whitespace { self.justify } else { 0.0 };
        }
        x
    }
}

impl<S: Style> Fragment<S> {
    /// The children that painting, hit testing, selection and finding visit: none for the box of an
    /// element whose contents are skipped (CSS Contain 2 `content-visibility: hidden`). Their
    /// fragments stay in `children`, laid out, so that geometry queries give real sizes.
    pub fn visible_children(&self) -> &[FragmentChild<S>] {
        match self.as_box() {
            Some(b) if b.style.contents_skipped() => &[],
            _ => &self.children,
        }
    }

    /// The box fragment data, for box fragments.
    pub fn as_box(&self) -> Option<&BoxFragment<S>> {
        match &self.kind {
            FragmentKind::Box(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&TextFragment<S>> {
        match &self.kind {
            FragmentKind::Text(t) => Some(t),
            _ => None,
        }
    }

    /// The generating node of a box or text fragment.
    pub fn node(&self) -> Option<NodeId> {
        match &self.kind {
            FragmentKind::Box(b) => b.node,
            FragmentKind::Text(t) => t.node,
            FragmentKind::Line { .. } => None,
        }
    }

    /// The border box in its own coordinates.
    pub fn border_rect(&self) -> Rect {
        Rect::new(0.0, 0.0, self.size.width, self.size.height)
    }

    /// The padding box in its own coordinates.
    pub fn padding_rect(&self) -> Rect {
        match &self.kind {
            FragmentKind::Box(b) => self.border_rect().deflate(&b.border),
            _ => self.border_rect(),
        }
    }

    /// The scrollable overflow rectangle of the box as a scroll container (CSS Overflow 3 §2.2):
    /// its padding box united with what its descendants contribute and, for a scroll container,
    /// its in-flow content bounds ([`Fragment::inflow`]) enlarged by its padding; less the
    /// unreachable scrollable overflow region on the start sides of the scroll origin (§2.3). (Unlike
    /// [`Fragment::overflow`], which is what the box contributes to its ancestors, its own border
    /// box included: a border is not something to scroll to.)
    pub fn scrollable_overflow(&self) -> Rect {
        let pb = self.padding_rect();
        let (right, bottom) = self.scroll_origin_end();
        // A box wholly in the unreachable region (before the origin on either axis) contributes
        // nothing, not even on the other axis (§2.2).
        let unreachable = |r: &Rect| {
            let before = |lo: f32, hi: f32, size: f32, edge: f32, end: bool| {
                if end {
                    lo > edge || (lo == edge && size > 0.0)
                } else {
                    hi < edge || (hi == edge && size > 0.0)
                }
            };
            before(
                r.x,
                r.right(),
                r.width,
                if right { pb.right() } else { pb.x },
                right,
            ) || before(
                r.y,
                r.bottom(),
                r.height,
                if bottom { pb.bottom() } else { pb.y },
                bottom,
            )
        };
        // (only a scroll container has an unreachable region to leave boxes out of; another box
        // measures what it holds from its origin, CSSOM View's scrolling area)
        let scroll_container = self
            .as_box()
            .is_some_and(|b| b.flags & fragment_flags::SCROLL_CONTAINER != 0);
        let mut overflow = pb;
        let mut bounds = overflow;
        for c in &self.children {
            let mut one = Rect::ZERO;
            add_overflow(&mut one, &mut bounds, c);
            if !(scroll_container && unreachable(&one)) {
                overflow = unite(overflow, one);
            }
        }
        if let FragmentKind::Box(b) = &self.kind {
            // (as for a scroll container, the content of a box with layout containment is
            // measured with its end padding: CSS Contain 2 §3.2 makes it an independent box whose
            // scrollable overflow is its own)
            if b.flags & fragment_flags::SCROLL_CONTAINER != 0 || b.style.layout_containment() {
                if let Some(inflow) = self.inflow {
                    let p = &b.padding;
                    let padded = Rect::from_ltrb(
                        inflow.x - p.left,
                        inflow.y - p.top,
                        inflow.right() + p.right,
                        inflow.bottom() + p.bottom,
                    );
                    overflow = unite(overflow, padded);
                }
            }
        }
        let (l, r) = if right {
            (overflow.x, overflow.right().min(pb.right()))
        } else {
            (overflow.x.max(pb.x), overflow.right())
        };
        let (t, bm) = if bottom {
            (overflow.y, overflow.bottom().min(pb.bottom()))
        } else {
            (overflow.y.max(pb.y), overflow.bottom())
        };
        Rect::from_ltrb(l, t, r.max(l), bm.max(t))
    }

    /// Where the box's scroll origin is (CSS Overflow 3 §2.3): whether it is at the right edge of
    /// the padding box (else the left) and at the bottom (else the top). It is the block-start
    /// inline-start corner of the box's writing mode, and for a flex container the main-start
    /// cross-start corner (`row-reverse`/`column-reverse` and `wrap-reverse` flip an axis).
    pub fn scroll_origin_end(&self) -> (bool, bool) {
        let FragmentKind::Box(b) = &self.kind else {
            return (false, false);
        };
        let s = &b.style;
        let rtl = s.direction() == Direction::Rtl;
        // (inline axis horizontal, x at the right, y at the bottom)
        let (horizontal, mut x_end, mut y_end) = match s.writing_mode() {
            WritingMode::HorizontalTb => (true, rtl, false),
            WritingMode::VerticalRl | WritingMode::SidewaysRl => (false, true, rtl),
            WritingMode::VerticalLr => (false, false, rtl),
            WritingMode::SidewaysLr => (false, false, !rtl),
        };
        let flex = matches!(
            s.display(),
            Display::Box {
                inside: DisplayInside::Flex,
                ..
            }
        );
        if flex {
            let (row, reverse) = match s.flex_direction() {
                FlexDirection::Row => (true, false),
                FlexDirection::RowReverse => (true, true),
                FlexDirection::Column => (false, false),
                FlexDirection::ColumnReverse => (false, true),
            };
            // The main axis is horizontal when it is the inline axis of a horizontal writing
            // mode or the block axis of a vertical one.
            let main_horizontal = row == horizontal;
            if reverse {
                if main_horizontal {
                    x_end = !x_end;
                } else {
                    y_end = !y_end;
                }
            }
            if s.flex_wrap() == FlexWrap::WrapReverse {
                if main_horizontal {
                    y_end = !y_end;
                } else {
                    x_end = !x_end;
                }
            }
        }
        (x_end, y_end)
    }

    /// The content box in its own coordinates.
    pub fn content_rect(&self) -> Rect {
        match &self.kind {
            FragmentKind::Box(b) => self.border_rect().deflate(&b.border).deflate(&b.padding),
            _ => self.border_rect(),
        }
    }

    /// Visits every fragment (depth first) with its offset from this fragment's origin.
    pub fn walk(&self, f: &mut impl FnMut(&Fragment<S>, Point)) {
        fn go<S: Style>(fr: &Fragment<S>, at: Point, f: &mut impl FnMut(&Fragment<S>, Point)) {
            f(fr, at);
            for c in &fr.children {
                go(
                    &c.fragment,
                    Point::new(at.x + c.offset.x, at.y + c.offset.y),
                    f,
                );
            }
        }
        go(self, Point::ZERO, f);
    }

    /// The border boxes of the fragments generated by `node` (in this fragment's coordinates).
    pub fn rects_of(&self, node: NodeId) -> Vec<Rect> {
        let mut out = Vec::new();
        self.walk(&mut |fr, at| {
            if fr.as_box().and_then(|b| b.node) == Some(node) {
                out.push(Rect::new(at.x, at.y, fr.size.width, fr.size.height));
            }
        });
        out
    }

    /// The first box fragment generated by `node`, with its offset.
    pub fn find(&self, node: NodeId) -> Option<(Point, &Fragment<S>)> {
        fn go<S: Style>(
            fr: &Fragment<S>,
            at: Point,
            node: NodeId,
        ) -> Option<(Point, &Fragment<S>)> {
            // (a `::backdrop` carries its element's node, for hit testing, but is not its box)
            if fr
                .as_box()
                .filter(|b| b.pseudo != Some(PseudoElement::Backdrop))
                .and_then(|b| b.node)
                == Some(node)
            {
                return Some((at, fr));
            }
            for c in &fr.children {
                if let Some(r) = go(
                    &c.fragment,
                    Point::new(at.x + c.offset.x, at.y + c.offset.y),
                    node,
                ) {
                    return Some(r);
                }
            }
            None
        }
        go(self, Point::ZERO, node)
    }

    /// How many fragments the tree holds.
    pub fn count(&self) -> usize {
        1 + self
            .children
            .iter()
            .map(|c| c.fragment.count())
            .sum::<usize>()
    }

    /// A text description (kind, node, rect), for tests and debugging.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        self.dump_into(Point::ZERO, 0, &mut out);
        out
    }

    fn dump_into(&self, at: Point, depth: usize, out: &mut String) {
        use std::fmt::Write;
        let what = match &self.kind {
            FragmentKind::Box(b) => format!(
                "box{}{}",
                b.node
                    .map(|n| format!(" #{}", n.index()))
                    .unwrap_or_default(),
                b.pseudo.map(|p| format!(" ::{p:?}")).unwrap_or_default()
            ),
            FragmentKind::Line { baseline } => format!("line baseline={baseline}"),
            FragmentKind::Text(t) => format!("text {:?}", t.clusters),
        };
        let _ = writeln!(
            out,
            "{:indent$}{what} ({}, {}) {}x{}",
            "",
            at.x,
            at.y,
            self.size.width,
            self.size.height,
            indent = depth * 2
        );
        for c in &self.children {
            c.fragment.dump_into(
                Point::new(at.x + c.offset.x, at.y + c.offset.y),
                depth + 1,
                out,
            );
        }
    }
}

/// The union of two overflow rectangles, where a rectangle with no area but a width or a height
/// (a zero-width box's height, say) still counts (CSS Overflow 3 §2.2 includes zero-area boxes);
/// one with neither contributes nothing.
pub(crate) fn unite(a: Rect, b: Rect) -> Rect {
    let absent = |r: &Rect| !(r.width > 0.0 || r.height > 0.0);
    if absent(&b) {
        return a;
    }
    if absent(&a) {
        return b;
    }
    Rect::from_ltrb(
        a.x.min(b.x),
        a.y.min(b.y),
        a.right().max(b.right()),
        a.bottom().max(b.bottom()),
    )
}

impl<S: Style> Fragment<S> {
    /// What the fragment contributes to its containing block's scrollable overflow, in its own
    /// (untransformed) coordinates (CSS Overflow 3 §2.2): its border box and, in each axis it does
    /// not clip, its own scrollable overflow. An axis clipped by `overflow: clip` lets the
    /// overflow through up to its overflow clip edge (§3.2 `overflow-clip-margin`); a scroll
    /// container's overflow stays inside it.
    pub(crate) fn overflow_contribution(&self) -> Rect {
        let Some(b) = self.as_box() else {
            return self.overflow;
        };
        let cx = b.flags & fragment_flags::CLIP_X != 0;
        let cy = b.flags & fragment_flags::CLIP_Y != 0;
        // (the root's and a propagating body's `overflow` is the viewport's, §3.3)
        let viewport = fragment_flags::ROOT | fragment_flags::VIEWPORT_OVERFLOW;
        if !cx && !cy || b.flags & viewport != 0 {
            if let Some(edge) = self.line_clamp_edge() {
                // (its floats are clipped there, and their clipped parts are no overflow)
                let o = self.overflow;
                let bottom = o.bottom().min(edge).max(self.size.height);
                return Rect::from_ltrb(o.x, o.y, o.right(), bottom);
            }
            return self.overflow;
        }
        let border = self.border_rect();
        let o = self.overflow;
        let edge = self.overflow_clip_edge();
        let axis = |clips: bool, o0: f32, o1: f32, e: Option<(f32, f32)>, b0: f32, b1: f32| {
            if !clips {
                return (o0, o1);
            }
            match e {
                Some((e0, e1)) => (o0.max(e0).min(b0), o1.min(e1).max(b1)),
                None => (b0, b1),
            }
        };
        let (l, r) = axis(
            cx,
            o.x,
            o.right(),
            edge.map(|e| (e.x, e.right())),
            border.x,
            border.right(),
        );
        let (t, bm) = axis(
            cy,
            o.y,
            o.bottom(),
            edge.map(|e| (e.y, e.bottom())),
            border.y,
            border.bottom(),
        );
        Rect::from_ltrb(l, t, r, bm)
    }

    /// The overflow clip edge of a box clipped by `overflow: clip` (CSS Overflow 3 §3.2): its
    /// `overflow-clip-margin` reference box enlarged by the margin, in the fragment's coordinates.
    /// `None` for a scroll container or a box that does not clip (whose clip edge is its padding
    /// box).
    pub fn overflow_clip_edge(&self) -> Option<Rect> {
        let b = self.as_box()?;
        let s = &b.style;
        let clip = s.overflow_x() == Overflow::Clip
            || s.overflow_y() == Overflow::Clip
            || s.paint_containment();
        if !clip || b.flags & fragment_flags::SCROLL_CONTAINER != 0 {
            return None;
        }
        Some(self.clip_margin_rect())
    }

    /// For a clamped line-clamp container: its block-end content edge as the clamp point leaves
    /// it (the content's extent, which a fixed height does not change), where its floats are
    /// clipped (CSS Overflow 4 §4.2).
    pub fn line_clamp_edge(&self) -> Option<f32> {
        let b = self.as_box()?;
        if b.flags & fragment_flags::LINE_CLAMPED == 0 {
            return None;
        }
        Some(
            self.inflow
                .map_or_else(|| self.content_rect().bottom(), |r| r.bottom()),
        )
    }

    /// The `overflow-clip-margin` reference box enlarged by the margin.
    fn clip_margin_rect(&self) -> Rect {
        let Some(b) = self.as_box() else {
            return self.border_rect();
        };
        let m = b.style.overflow_clip_margin();
        let reference = match m.reference {
            BoxArea::BorderBox => self.border_rect(),
            BoxArea::PaddingBox => self.padding_rect(),
            BoxArea::ContentBox => self.content_rect(),
        };
        reference.inflate(m.margin, m.margin)
    }

    /// The rectangle a box clips its content to when it clips: its overflow clip edge for
    /// `overflow: clip` and paint containment; for a scroll container its padding box, which
    /// `overflow-clip-margin` can only reduce (a visual box or a negative margin inside it).
    pub fn overflow_clip_rect(&self) -> Rect {
        if let Some(e) = self.overflow_clip_edge() {
            return e;
        }
        let pb = self.padding_rect();
        match self.as_box() {
            // (a `border-box` reference is ignored on a scroll container, its offset too)
            Some(b)
                if b.flags & fragment_flags::SCROLL_CONTAINER != 0
                    && b.style.overflow_clip_margin().reference != BoxArea::BorderBox =>
            {
                pb.intersect(&self.clip_margin_rect())
            }
            _ => pb,
        }
    }
}

/// The union of a rectangle with a child's contribution to scrollable overflow, and of `bounds`
/// with the child's paint bounds ([`Fragment::paint_bounds`]), both through the child's
/// transform.
pub(crate) fn add_overflow<S: Style>(
    overflow: &mut Rect,
    bounds: &mut Rect,
    child: &FragmentChild<S>,
) {
    let f = &child.fragment;
    let transform = f.as_box().and_then(|b| b.used_transform(f.size));
    let mut pb = f.paint_bounds;
    let mut r = f.overflow_contribution();
    if let Some(m) = transform {
        pb = m.apply_rect(&pb);
        r = m.apply_rect(&r);
    }
    if !(pb.width <= 0.0 && pb.height <= 0.0) {
        *bounds = bounds.union(&pb.translate(child.offset.x, child.offset.y));
    }
    // (zero-area boxes count when an element generates them; an empty anonymous box or line box,
    // like the one an inline split around a block leaves, contributes nothing)
    let zero_area = !(r.width > 0.0 && r.height > 0.0);
    if zero_area && f.node().is_none() {
        return;
    }
    *overflow = unite(*overflow, r.translate(child.offset.x, child.offset.y));
}
