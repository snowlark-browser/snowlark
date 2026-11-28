// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Intrinsic inline sizes (css-sizing-3 §5): each box's min-content and max-content content widths
//! (cached on the box by [`crate::layout::Cx::content_sizes`]) and outer contributions to its parent.

use crate::boxes::*;
use crate::layout::Cx;
use crate::sizing::*;
use crate::style::*;

/// The content-box min/max-content widths of `id`.
pub(crate) fn compute_content_sizes<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) -> ContentSizes {
    let b = cx.tree.b(id);
    // CSS Contain 2 §3.2: under (inline-)size containment the intrinsic inline sizes are those of
    // the box with no content (a replaced box's natural size is zero, see `replaced::natural`).
    // Containment does not apply to internal table boxes; flex and grid containers measure
    // themselves without their items (`flex_grid::contained_inputs`).
    let contained = b.style.size_containment();
    if contained.width && b.kind == BoxKind::Block {
        let w = contained.intrinsic_width.unwrap_or(0.0);
        return ContentSizes { min: w, max: w };
    }
    // (a block container's custom scroll bars take space beside its content, CSS Overflow 3 §3;
    // taffy counts a flex or grid container's)
    let gutter = match b.kind {
        BoxKind::Block | BoxKind::TableCell | BoxKind::TableCaption => {
            let g = crate::gutter::gutter(b);
            g.left + g.right
        }
        _ => 0.0,
    };
    let sizes = match b.kind {
        BoxKind::Block if b.style.is_multicol() && b.is_independent() => {
            let style = b.style.clone();
            let mut spanners = ContentSizes::ZERO;
            let columns = if b.has_inline_content() {
                crate::inline::content_sizes(cx, id)
            } else {
                block_children_sizes(cx, id, Some(&mut spanners))
            };
            crate::multicol::content_sizes(&style, columns, spanners)
        }
        BoxKind::Block | BoxKind::TableCell | BoxKind::TableCaption => {
            if b.has_inline_content() {
                crate::inline::content_sizes(cx, id)
            } else {
                block_children_sizes(cx, id, None)
            }
        }
        BoxKind::Flex | BoxKind::Grid => crate::flex_grid::content_sizes(cx, id),
        BoxKind::TableWrapper => crate::table::content_sizes(cx, id),
        BoxKind::Replaced => {
            let w = crate::replaced::natural_content_width(cx, id);
            ContentSizes { min: w, max: w }
        }
        _ => ContentSizes::ZERO,
    };
    sizes.add(gutter)
}

/// Block children: the widest contribution; floats side by side add up until something clears them.
/// With `spanners`, the children of a multi-column container: a spanner's contribution goes
/// there instead (it is not in the columns).
fn block_children_sizes<S: Style>(
    cx: &mut Cx<'_, S>,
    id: BoxId,
    mut spanners: Option<&mut ContentSizes>,
) -> ContentSizes {
    let kids = cx.tree.b(id).children.clone();
    let mut result = ContentSizes::ZERO;
    let mut floats = ContentSizes::ZERO;
    for c in kids {
        let cb = cx.tree.b(c);
        if cb.is_out_of_flow()
            || matches!(cb.kind, BoxKind::TableColumn | BoxKind::TableColumnGroup)
        {
            continue;
        }
        if let Some(s) = spanners.as_deref_mut() {
            if !cb.is_float() && cb.level == Level::Block && cb.style.column_span_all() {
                *s = s.union(contribution(cx, c));
                continue;
            }
        }
        let clears = cb.style.clear() != Clear::None;
        let is_float = cb.is_float();
        let contrib = contribution(cx, c);
        if is_float {
            if clears {
                result = result.union(floats);
                floats = ContentSizes::ZERO;
            }
            floats.min = floats.min.max(contrib.min);
            floats.max += contrib.max;
        } else {
            if clears {
                result = result.union(floats);
                floats = ContentSizes::ZERO;
            }
            // A block beside floats: max-content puts it next to them (if it avoids floats) or under.
            result = result.union(contrib);
            result.max = result.max.max(
                floats.max
                    + if cx.tree.b(c).is_independent() {
                        contrib.max
                    } else {
                        0.0
                    },
            );
        }
    }
    result.union(floats)
}

/// The outer (margin-box) min/max-content contribution of `id` to its containing block.
pub(crate) fn contribution<S: Style>(cx: &mut Cx<'_, S>, id: BoxId) -> ContentSizes {
    let style = cx.style_here(id);
    let kind = cx.tree.b(id).kind;
    let own_frame = cx.tree.b(id).style.view() == cx.frame;
    // Percentages of an indefinite width count as zero for margins and padding here.
    let edges = Edges::of(&style, 0.0);
    let pb = edges.pb_h();
    let margins = edges.margin_h();
    let sizing = style.box_sizing();
    // Within a height context (see `Cx::pct_height_basis`): the box's own definite content height,
    // for its children's percentage heights (a percentage of the parent's definite height, CSS 2
    // §10.5). Outside one, heights are not looked at (as before; it keeps the cache in use).
    let outer_basis = cx.pct_height_basis;
    let own_basis = outer_basis.and_then(|_| {
        resolve_height(style.height(), outer_basis, edges.pb_v(), sizing)
            .map(|h| (h - edges.pb_v()).max(0.0))
    });
    let mut content: Option<ContentSizes> = None;
    let mut get = || {
        if let Some(c) = content {
            return c;
        }
        let saved = std::mem::replace(&mut cx.pct_height_basis, own_basis);
        let c = cx.content_sizes(id);
        cx.pct_height_basis = saved;
        content = Some(c);
        c
    };
    if kind == BoxKind::Replaced && own_frame {
        let (min, max) = crate::replaced::contribution_widths(cx, id);
        return ContentSizes {
            min: min + margins,
            max: max + margins,
        };
    }
    let specified = match style.width() {
        SizeValue::LengthPercentage(lp) if !lp.has_percent() => {
            Some(to_border_box(lp.length, pb, sizing))
        }
        SizeValue::MinContent => Some(get().min + pb),
        SizeValue::MaxContent => Some(get().max + pb),
        // (a definite height transfers through `aspect-ratio`, css-sizing-4 §5.1)
        _ => own_basis
            .zip(style_ratio(&style))
            .map(|(h, r)| match sizing {
                BoxSizing::BorderBox => (h + edges.pb_v()) * r,
                BoxSizing::ContentBox => h * r + pb,
            }),
    };
    let mut sizes = match specified {
        Some(w) => ContentSizes { min: w, max: w },
        None => {
            let c = get();
            ContentSizes {
                min: c.min + pb,
                max: c.max + pb,
            }
        }
    };
    // A sizing keyword as a min or max width is the box's own min-content or max-content width;
    // `fit-content` is the min-content width in the min-content contribution and the max-content
    // width in the max-content one (css-sizing-3 §5.2). `stretch` is ignored here (`auto`/`none`).
    let mut bound = |v: SizeValue, max_contribution: bool| match v {
        SizeValue::LengthPercentage(lp) if !lp.has_percent() => {
            Some(to_border_box(lp.length, pb, sizing))
        }
        SizeValue::MinContent => Some(get().min + pb),
        SizeValue::MaxContent => Some(get().max + pb),
        SizeValue::FitContent(None) if max_contribution => Some(get().max + pb),
        SizeValue::FitContent(None) => Some(get().min + pb),
        _ => None,
    };
    let max_width = match style.max_width() {
        MaxSizeValue::LengthPercentage(lp) => SizeValue::LengthPercentage(lp),
        MaxSizeValue::MinContent => SizeValue::MinContent,
        MaxSizeValue::MaxContent => SizeValue::MaxContent,
        MaxSizeValue::FitContent(l) => SizeValue::FitContent(l),
        _ => SizeValue::Auto,
    };
    let (min_width, max_width) = (style.min_width(), max_width);
    let (min, max) = (bound(min_width, false), bound(max_width, false));
    sizes.min = clamp(sizes.min, min, max);
    let (min, max) = (bound(min_width, true), bound(max_width, true));
    sizes.max = clamp(sizes.max, min, max);
    sizes.add(margins)
}
