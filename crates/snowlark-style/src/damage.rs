// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Style change classification: what a restyled element's new style requires of layout, in the spirit
//! of Blink's `StyleDifference` and Gecko's change hints, coarsened to the four things our box tree and
//! layout can do:
//!
//! - [`StyleChange::Unchanged`]: the computed values are equal (a new `ComputedValues` with the same
//!   values, or only custom properties or animation declarations changed).
//! - [`StyleChange::Paint`]: only properties that painting reads from the style itself changed
//!   (colours, backgrounds, border colours, box shadows, outlines, `visibility`, cursor): the boxes'
//!   fragments stand and take the new style, with no layout.
//! - [`StyleChange::Repaint`]: only properties that painting reads changed, some of which layout
//!   records in the fragments (where a box paints, how it stacks: border radii, opacity, filters,
//!   `z-index`, transforms between two non-`none` values, text decoration placement).
//! - [`StyleChange::Relayout`]: geometry may change but the element keeps its boxes (sizes, margins,
//!   padding, border widths, fonts, line height, text alignment, flex and grid properties...).
//! - [`StyleChange::Reconstruct`]: the element's boxes must be generated again (`display`, `position`,
//!   `float`, `overflow` becoming (or ceasing to be) `visible`, `appearance`).
//!
//! Separately, [`Classification::contents`] says whether the boxes the element *generates inside*
//! itself must be generated again while its own box stays: its `::before`/`::after`, its marker
//! (list properties, `content`), what its descendants' boxes store from it (`white-space-collapse`,
//! which decides which white space generates boxes). The text decorations its descendants' boxes
//! store from it are `Paint` there: the boxes take them in place.
//!
//! Each style struct is compared by pointer first (stylo shares unchanged structs between the old and
//! the new values), then by value; a struct that differs is classified by copying the fields of each
//! weaker class from the new struct into a copy of the old one and comparing again. A property this
//! module does not name is therefore `Relayout`, never `Unchanged`, so a property added to stylo can
//! only make the classification conservative.
//!
//! [`::before`/`::after`](classify_pseudo) are classified the same way, any change of theirs being a
//! change of the element's contents. `counter-*` and `quotes` changes (and elements or pseudo-elements
//! that use them appearing or disappearing) also set [`ElementRestyle::counters`]: generated content
//! of other elements may change.

use servo_arc::Arc;
use snowlark_dom::NodeId;
use style::computed_values::visibility::T as Visibility;
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use style::values::specified::box_::Overflow;

/// What a style change requires, weakest first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StyleChange {
    /// The computed values are equal.
    #[default]
    Unchanged,
    /// Only painting changes, from what it reads in the style alone.
    Paint,
    /// Only painting changes, including what layout records for it.
    Repaint,
    /// Layout of the element's boxes (and what depends on them), with the same boxes.
    Relayout,
    /// The element's boxes must be generated again.
    Reconstruct,
}

impl StyleChange {
    fn bits(self) -> u32 {
        self as u32
    }

    fn from_bits(bits: u32) -> StyleChange {
        match bits & 7 {
            0 => StyleChange::Unchanged,
            1 => StyleChange::Paint,
            2 => StyleChange::Repaint,
            3 => StyleChange::Relayout,
            _ => StyleChange::Reconstruct,
        }
    }
}

/// One element the restyle gave new styles, with what the change requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElementRestyle {
    pub element: NodeId,
    /// The element's own style (its principal box, the anonymous boxes and text that take it).
    pub change: StyleChange,
    /// What it generates inside itself (see [`Classification::contents`]): `::before`, `::after`, the
    /// marker, the decorations and white space handling its descendants' boxes take from it.
    pub contents: StyleChange,
    /// `counter-reset`, `counter-increment` or `quotes` changed: counters and quotes in generated
    /// content elsewhere in the document may change.
    pub counters: bool,
}

impl ElementRestyle {
    /// The stronger of the element's own and its contents' changes.
    pub fn strongest(&self) -> StyleChange {
        self.change.max(self.contents)
    }

    pub(crate) fn pack(c: Classification) -> u32 {
        c.change.bits() | (c.contents.bits() << 3) | (u32::from(c.counters) << 6)
    }

    pub(crate) fn unpack(element: NodeId, bits: u32) -> ElementRestyle {
        ElementRestyle {
            element,
            change: StyleChange::from_bits(bits),
            contents: StyleChange::from_bits(bits >> 3),
            counters: bits & 64 != 0,
        }
    }
}

/// What a restyle's traversal recorded since the last [`crate::StyleData::take_restyles`].
#[derive(Clone, Debug, Default)]
pub struct Restyles {
    /// The elements whose styles changed, in no particular order (an element restyled by several
    /// traversals appears once per traversal).
    pub elements: Vec<ElementRestyle>,
    /// More was recorded than the log keeps: treat every element as reconstructed.
    pub overflowed: bool,
}

/// A style change's classification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Classification {
    /// What the element's own boxes need.
    pub change: StyleChange,
    /// Whether (`Reconstruct`) the boxes it generates inside itself must be generated again with its
    /// own box kept, or (`Paint`) only the text decorations they inherit from it changed (or, for a
    /// pseudo-element's change, how that pseudo-element changed).
    pub contents: StyleChange,
    /// Counters or quotes changed: generated content elsewhere may change.
    pub counters: bool,
}

/// Copies the named fields of `$from` into `$to`.
macro_rules! copy_fields {
    ($to:ident, $from:ident; $($f:ident),* $(,)?) => {{
        $( $to.$f = $from.$f.clone(); )*
    }};
}

/// Classifies one style struct: `Unchanged` when equal; otherwise, copying the fields of each
/// weaker class in turn (after the ignored ones), `Paint` when the paint-only fields make them
/// equal, `Repaint` when the repaint fields do, else `Relayout`.
fn classify_struct<T: Clone + PartialEq>(
    old: &T,
    new: &T,
    ignored: impl Fn(&mut T, &T),
    paint: impl Fn(&mut T, &T),
    repaint: impl Fn(&mut T, &T),
) -> StyleChange {
    if std::ptr::eq(old, new) || old == new {
        return StyleChange::Unchanged;
    }
    let mut patched = old.clone();
    ignored(&mut patched, new);
    if patched == *new {
        return StyleChange::Unchanged;
    }
    paint(&mut patched, new);
    if patched == *new {
        return StyleChange::Paint;
    }
    repaint(&mut patched, new);
    if patched == *new {
        StyleChange::Repaint
    } else {
        StyleChange::Relayout
    }
}

fn differs<T: PartialEq>(a: &T, b: &T) -> bool {
    !std::ptr::eq(a, b) && a != b
}

/// Whether a style takes part in counters (CSS Lists 3 §4) or quote nesting (CSS Generated Content
/// 3 §2.2), so its appearing or disappearing changes generated content elsewhere.
fn uses_counters(s: &ComputedValues) -> bool {
    use style::values::generics::counters::{Content, ContentItem};
    let c = s.get_counters();
    !c.counter_increment.is_empty()
        || !c.counter_reset.is_empty()
        || match &c.content {
            Content::Items(items) => items.items.iter().any(|i| {
                matches!(
                    i,
                    ContentItem::OpenQuote
                        | ContentItem::CloseQuote
                        | ContentItem::NoOpenQuote
                        | ContentItem::NoCloseQuote
                )
            }),
            _ => false,
        }
}

/// Whether `align-content` is `normal` (with any overflow position).
fn align_content_is_normal(v: &ComputedValues) -> bool {
    use style::values::specified::align::AlignFlags;
    v.get_position().align_content.primary().value() == AlignFlags::NORMAL
}

/// Whether an element with these values is the containing block of its absolutely and fixed
/// positioned descendants whatever its `position` (css-position-3 §2.1): a transform or
/// perspective, `transform-style: preserve-3d` (css-transforms-1 §2, -2 §6), a filter or backdrop
/// filter (Filter Effects 1 §5, 2 §3), layout or paint containment (CSS Contain 2 §3.2, §3.3), or
/// `will-change` naming any of them (CSS Will Change 1 §3). (The root element is one only through
/// a transform; layout checks that.) `container-type` is not: it applies style and size
/// containment, not layout containment (CSS Conditional 5 §6.1, csswg-drafts issue 10544).
pub fn establishes_fixed_cb(v: &ComputedValues) -> bool {
    use style::values::computed::Contain;
    use style::values::specified::box_::WillChangeBits;
    let b = v.get_box();
    let e = v.get_effects();
    b.has_transform_or_perspective()
        || !e.filter.0.is_empty()
        || !e.backdrop_filter.0.is_empty()
        || b.contain.intersects(Contain::LAYOUT | Contain::PAINT)
        || b.will_change.bits.intersects(
            WillChangeBits::TRANSFORM
                | WillChangeBits::PERSPECTIVE
                | WillChangeBits::FIXPOS_CB_NON_SVG
                | WillChangeBits::CONTAIN,
        )
}

/// Whether two `filter` lists move pixels the same way (Filter Effects 1 §10: `blur()` and
/// `drop-shadow()` are the functions whose result reaches past their input): the same functions
/// of those kinds, in order, with the same lengths.
fn same_filter_outsets(
    a: &[style::values::computed::effects::Filter],
    b: &[style::values::computed::effects::Filter],
) -> bool {
    use style::values::generics::effects::Filter as F;
    let moves = |list: &[style::values::computed::effects::Filter]| -> Vec<[f32; 3]> {
        list.iter()
            .filter_map(|f| match f {
                F::Blur(l) => Some([l.0.px(), 0.0, 0.0]),
                F::DropShadow(s) => Some([s.blur.0.px(), s.horizontal.px(), s.vertical.px()]),
                _ => None,
            })
            .collect()
    };
    moves(a) == moves(b)
}

fn is_visible(o: Overflow) -> bool {
    o == Overflow::Visible
}

/// Classifies a change of an element's computed values from `old` to `new`.
pub fn classify(old: &ComputedValues, new: &ComputedValues) -> Classification {
    if std::ptr::eq(old, new) {
        return Classification::default();
    }
    // ----- what generates boxes -----
    let (ob, nb) = (old.get_box(), new.get_box());
    let (oc, nc) = (old.get_counters(), new.get_counters());
    let (ol, nl) = (old.get_list(), new.get_list());
    let (ot, nt) = (old.get_text(), new.get_text());
    let (oit, nit) = (old.get_inherited_text(), new.get_inherited_text());
    let counters = !std::ptr::eq(oc, nc)
        && (oc.counter_increment != nc.counter_increment || oc.counter_reset != nc.counter_reset)
        || !std::ptr::eq(ol, nl) && ol.quotes != nl.quotes;
    let decorated = !nt.text_decoration_line.is_empty();
    let reconstruct = !std::ptr::eq(ob, nb)
        && (ob.display != nb.display
            || ob.original_display != nb.original_display
            || ob.position != nb.position
            || ob.float != nb.float
            || ob.appearance != nb.appearance
            || is_visible(ob.overflow_x) != is_visible(nb.overflow_x)
            || is_visible(ob.overflow_y) != is_visible(nb.overflow_y)
            // (scroll containers and line-clamp containers establish independent formatting
            // contexts, decided when the boxes are generated)
            || ob.overflow_x.is_scrollable() != nb.overflow_x.is_scrollable()
            || ob.overflow_y.is_scrollable() != nb.overflow_y.is_scrollable()
            || ob.line_clamp.is_none() != nb.line_clamp.is_none()
            || ob.contain != nb.contain
            || ob.content_visibility != nb.content_visibility)
        // Becoming or ceasing to be the containing block of positioned descendants: the boxes
        // below find their containing blocks again.
        || establishes_fixed_cb(old) != establishes_fixed_cb(new)
        // (`align-content` other than `normal` makes a block container establish an independent
        // formatting context, CSS Box Alignment 3 §5.4, decided when its boxes are generated)
        || align_content_is_normal(old) != align_content_is_normal(new)
        // (a replaced element's image is chosen when its box is made: CSS Images 3 §5.3)
        || old.get_inherited_box().image_orientation != new.get_inherited_box().image_orientation;
    if reconstruct {
        return Classification {
            change: StyleChange::Reconstruct,
            contents: StyleChange::Unchanged,
            counters,
        };
    }
    let regenerate = differs(oc, nc)
        || differs(ol, nl)
        || !std::ptr::eq(oit, nit) && oit.white_space_collapse != nit.white_space_collapse;
    // The box tree stores the decorations its text inherits (line, style and colour, which
    // resolves `currentcolor`): updated in place, for painting only (`Paint`).
    let decorations = !std::ptr::eq(ot, nt)
        && (ot.text_decoration_line != nt.text_decoration_line
            || ot.text_decoration_style != nt.text_decoration_style
            || ot.text_decoration_color != nt.text_decoration_color)
        || decorated && !std::ptr::eq(oit, nit) && oit.color != nit.color;

    let mut change = StyleChange::Unchanged;
    let mut add = |c: StyleChange| change = change.max(c);
    add(classify_struct(
        old.get_background(),
        new.get_background(),
        |_, _| {},
        |p, n| {
            copy_fields!(p, n; background_color, background_image, background_position_x,
                background_position_y, background_repeat, background_size, background_attachment,
                background_clip, background_origin, background_blend_mode)
        },
        |_, _| {},
    ));
    add(classify_struct(
        old.get_border(),
        new.get_border(),
        |_, _| {},
        |p, n| {
            copy_fields!(p, n; border_top_color, border_right_color, border_bottom_color,
                border_left_color)
        },
        // (radii shape the clips of what is inside)
        |p, n| {
            copy_fields!(p, n; border_image_outset, border_image_repeat, border_image_slice,
                border_image_source, border_image_width, border_top_left_radius,
                border_top_right_radius, border_bottom_right_radius, border_bottom_left_radius,
                corner_top_left_shape, corner_top_right_shape, corner_bottom_right_shape,
                corner_bottom_left_shape)
        },
    ));
    // Transforms are painting (a box gaining or losing one, which makes it the containing block of
    // its positioned descendants, was a reconstruction above), recorded in the fragments: where the
    // box paints, and the stacking context it makes.
    add(classify_struct(
        ob,
        nb,
        |_, _| {},
        |_, _| {},
        |p, n| {
            copy_fields!(p, n; isolation, backface_visibility, will_change, transform,
                transform_origin, rotate, scale, translate, perspective, perspective_origin,
                transform_style, offset_path, offset_distance, offset_rotate, offset_anchor,
                offset_position);
        },
    ));
    // (opacity, filters and blending make stacking contexts, which the fragments of positioned
    // descendants record; a shadow is painted outside what the fragments record)
    add(classify_struct(
        old.get_effects(),
        new.get_effects(),
        |_, _| {},
        |p, n| copy_fields!(p, n; box_shadow),
        |p, n| {
            copy_fields!(p, n; backdrop_filter, clip, opacity, mix_blend_mode);
            // (a filter that reaches farther past the box, or less far, changes where the box
            // paints, which its fragments record: layout again; a change of colors only repaints)
            if same_filter_outsets(&p.filter.0, &n.filter.0) {
                p.filter = n.filter.clone();
            }
        },
    ));
    add(classify_struct(
        old.get_inherited_box(),
        new.get_inherited_box(),
        |_, _| {},
        |p, n| {
            copy_fields!(p, n; image_rendering);
            // `visibility: collapse` removes table rows and columns from layout.
            if p.visibility != Visibility::Collapse && n.visibility != Visibility::Collapse {
                copy_fields!(p, n; visibility);
            }
        },
        |_, _| {},
    ));
    // (clip paths and masks make stacking contexts, and clip paths' reference boxes are recorded)
    add(classify_struct(
        old.get_inherited_svg(),
        new.get_inherited_svg(),
        |_, _| {},
        |_, _| {},
        |p, n| *p = n.clone(),
    ));
    add(classify_struct(
        old.get_svg(),
        new.get_svg(),
        |_, _| {},
        |_, _| {},
        |p, n| *p = n.clone(),
    ));
    add(classify_struct(
        oit,
        nit,
        |_, _| {},
        |p, n| copy_fields!(p, n; color, text_shadow),
        |p, n| copy_fields!(p, n; text_underline_offset, text_underline_position, text_rendering),
    ));
    // (the used color scheme colours what controls draw)
    add(classify_struct(
        old.get_inherited_ui(),
        new.get_inherited_ui(),
        |_, _| {},
        |p, n| copy_fields!(p, n; caret_color, cursor, pointer_events, scrollbar_color),
        |p, n| copy_fields!(p, n; color_scheme),
    ));
    add(classify_struct(
        old.get_outline(),
        new.get_outline(),
        |_, _| {},
        |p, n| *p = n.clone(),
        |_, _| {},
    ));
    add(classify_struct(
        old.get_position(),
        new.get_position(),
        |_, _| {},
        |_, _| {},
        |p, n| copy_fields!(p, n; z_index),
    ));
    add(classify_struct(
        ot,
        nt,
        // Decoration lines, styles and colours are painted from the boxes they reach, which take
        // them in place (`contents`).
        |p, n| copy_fields!(p, n; text_decoration_line, text_decoration_style, text_decoration_color),
        |_, _| {},
        |p, n| copy_fields!(p, n; text_decoration_thickness, text_decoration_inset),
    ));
    add(classify_struct(
        old.get_ui(),
        new.get_ui(),
        // Animations and transitions act through the computed values they produce.
        |p, n| {
            copy_fields!(p, n; animation_delay, animation_direction, animation_duration,
                animation_fill_mode, animation_iteration_count, animation_name, animation_play_state,
                animation_range_end, animation_range_start, animation_timeline,
                animation_timing_function, animation_composition, transition_behavior,
                transition_delay, transition_duration, transition_property,
                transition_timing_function, view_transition_class, view_transition_name)
        },
        |p, n| copy_fields!(p, n; user_select),
        |_, _| {},
    ));
    // Everything else is layout.
    if differs(old.get_column(), new.get_column())
        || differs(old.get_font(), new.get_font())
        || differs(old.get_inherited_table(), new.get_inherited_table())
        || differs(old.get_margin(), new.get_margin())
        || differs(old.get_padding(), new.get_padding())
        || differs(old.get_table(), new.get_table())
    {
        add(StyleChange::Relayout);
    }
    if old.writing_mode != new.writing_mode || old.effective_zoom != new.effective_zoom {
        add(StyleChange::Relayout);
    }
    // The used color scheme (the page's or the preferred one changed under `color-scheme: normal`
    // or `light dark`) colors what the user agent paints: scroll bars, the canvas, controls.
    let dark = style::computed_value_flags::ComputedValueFlags::USED_DARK_COLOR_SCHEME;
    if old.flags.contains(dark) != new.flags.contains(dark) {
        add(StyleChange::Repaint);
    }
    Classification {
        change,
        contents: if regenerate {
            StyleChange::Reconstruct
        } else if decorations {
            StyleChange::Paint
        } else {
            StyleChange::Unchanged
        },
        counters,
    }
}

/// Classifies the change of a `::before` or `::after` style (`None`: the element has none): how
/// its boxes change (`Reconstruct` when it appears or disappears), and whether counters changed.
pub fn classify_pseudo(
    pseudo: &PseudoElement,
    old: Option<&ComputedValues>,
    new: Option<&ComputedValues>,
) -> (StyleChange, bool) {
    let old = old.filter(|s| pseudo.should_exist(s));
    let new = new.filter(|s| pseudo.should_exist(s));
    match (old, new) {
        (None, None) => (StyleChange::Unchanged, false),
        (Some(o), Some(n)) => {
            let c = classify(o, n);
            (c.change.max(c.contents), c.counters)
        }
        (o, n) => (StyleChange::Reconstruct, o.or(n).is_some_and(uses_counters)),
    }
}

/// Classifies an element's restyle from its old styles to its new ones (eager pseudo-elements by
/// stylo's eager index).
pub(crate) fn classify_element(
    old: Option<&Arc<ComputedValues>>,
    new: Option<&Arc<ComputedValues>>,
    old_pseudos: &[Option<Arc<ComputedValues>>],
    new_pseudos: &[Option<Arc<ComputedValues>>],
) -> u32 {
    let mut c = match (old, new) {
        (Some(o), Some(n)) => classify(o, n),
        // A first style (or none any more): new boxes.
        (o, n) => Classification {
            change: StyleChange::Reconstruct,
            contents: StyleChange::Unchanged,
            counters: o.or(n).is_some_and(|s| uses_counters(s)),
        },
    };
    for (i, (o, n)) in old_pseudos.iter().zip(new_pseudos).enumerate() {
        let p = PseudoElement::from_eager_index(i);
        if !p.is_before_or_after() {
            continue;
        }
        let (change, counters) = classify_pseudo(&p, o.as_deref(), n.as_deref());
        if change != StyleChange::Unchanged {
            c.contents = StyleChange::Reconstruct;
        }
        c.counters |= counters;
    }
    ElementRestyle::pack(c)
}
