// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Serializing computed values for `getComputedStyle` (CSSOM §6.7 "resolved values").
//!
//! The resolved value of most properties is the computed value, serialized by stylo. For the properties
//! CSSOM lists as layout-dependent ('width', 'height', margins, paddings, insets, 'line-height' when not
//! `normal`, ...) it is the used value when the element is being rendered: layout supplies those through
//! [`UsedValues`]; without it the computed value is returned.

use std::sync::OnceLock;

use style::properties::{
    ComputedValues, LonghandId, NonCustomPropertyId, PropertyDeclaration, PropertyDeclarationId,
    PropertyId, ShorthandId,
};
use style::values::resolved;
use style_traits::CssStringWriter;

/// Layout's answer for the layout-dependent properties of one element: the used value of `property`
/// (a physical longhand) serialized as CSS (`"120px"`), or `None` when the element has no box and the
/// computed value applies.
pub trait UsedValues {
    fn used_value(&self, property: LonghandId) -> Option<String>;
}

/// Parses a CSS property name (a longhand, shorthand, alias or custom property) the way
/// `CSSStyleDeclaration.getPropertyValue` does: ASCII case-insensitively for non-custom properties.
pub fn property_id(name: &str) -> Option<PropertyId> {
    crate::prefs::init();
    if name.starts_with("--") {
        return PropertyId::parse_enabled_for_all_content(name).ok();
    }
    PropertyId::parse_enabled_for_all_content(&name.to_ascii_lowercase()).ok()
}

/// The physical longhand a (logical) longhand `name` maps to in the writing mode of `values`
/// (`margin-inline-start` in a horizontal left-to-right box is `margin-left`); other names unchanged.
pub fn physical_name(values: &ComputedValues, name: &str) -> String {
    let Some(id) = property_id(name) else {
        return name.to_owned();
    };
    match id.as_shorthand() {
        Err(PropertyDeclarationId::Longhand(l)) => {
            let p = PropertyDeclarationId::Longhand(l).to_physical(values.writing_mode);
            let mut out = String::new();
            let _ = style_traits::ToCss::to_css(&p, &mut style_traits::CssWriter::new(&mut out));
            out
        }
        _ => name.to_owned(),
    }
}

/// The serialized computed value of `property` in `values`. Shorthands are serialized from their
/// longhands' values; a shorthand the longhands cannot express (`all`, a mix the grammar can't write)
/// is the empty string. An unregistered custom property that is not set is the empty string.
pub fn computed_property(values: &ComputedValues, property: &PropertyId) -> String {
    match property.as_shorthand() {
        Ok(shorthand) => shorthand_value(values, shorthand, None),
        Err(PropertyDeclarationId::Longhand(id)) => longhand_value(values, id, None),
        Err(custom) => values.computed_value_to_string(custom),
    }
}

/// The computed value of a longhand as stylo serializes it, with stylo's Gecko spellings of the legacy
/// `text-align` keywords written as the `-webkit-` aliases the web uses (they parse as the same value).
fn serialize_longhand(values: &ComputedValues, id: LonghandId) -> String {
    let css = values.computed_value_to_string(PropertyDeclarationId::Longhand(id));
    if id == LonghandId::TextAlign {
        if let Some(rest) = css.strip_prefix("-moz-") {
            return format!("-webkit-{rest}");
        }
    }
    css
}

/// Whether `a` and `b` hold the same computed value of every property: each style struct (shared
/// ones are the same object, the others compared by value), the effective zoom and the writing
/// mode. Custom properties are not compared: what they gave the properties is in the structs.
pub fn same_values(a: &ComputedValues, b: &ComputedValues) -> bool {
    if std::ptr::eq(a, b) {
        return true;
    }
    macro_rules! same {
        ($($get:ident),*) => {
            $({
                let (x, y) = (a.$get(), b.$get());
                std::ptr::eq(x, y) || x == y
            })&&*
        };
    }
    a.effective_zoom == b.effective_zoom
        && a.writing_mode == b.writing_mode
        && same!(
            get_background,
            get_border,
            get_box,
            get_column,
            get_counters,
            get_effects,
            get_font,
            get_inherited_box,
            get_inherited_svg,
            get_inherited_table,
            get_inherited_text,
            get_inherited_ui,
            get_list,
            get_margin,
            get_outline,
            get_padding,
            get_position,
            get_svg,
            get_table,
            get_text,
            get_ui
        )
}

/// Whether `property` has the same computed value in `a` and `b` (every longhand of a shorthand;
/// never for a custom property or a logical longhand). Equal computed values serialize the same by
/// [`computed_property`] when the two styles' `color` is equal too, which resolves `currentcolor`
/// (CSS Color 4 §4.4); the only other inputs of a resolved value are `animation-timeline`'s UI
/// struct and `content`'s pseudo-element.
pub fn same_computed_value(a: &ComputedValues, b: &ComputedValues, property: &PropertyId) -> bool {
    match property.as_shorthand() {
        Ok(shorthand) => shorthand.longhands().all(|l| a.longhand_equals(b, l)),
        Err(PropertyDeclarationId::Longhand(id)) => a.longhand_equals(b, id),
        Err(_) => false,
    }
}

/// The initial values of every property (CSS Cascade 4 §7.1), with the initial font.
pub fn initial_values() -> &'static ComputedValues {
    static INITIAL: OnceLock<servo_arc::Arc<ComputedValues>> = OnceLock::new();
    INITIAL.get_or_init(|| {
        ComputedValues::initial_values_with_font_override(
            style::properties::style_structs::Font::initial_values(),
        )
    })
}

/// [`computed_property`] by name; `None` for an unknown property.
pub fn computed_property_by_name(values: &ComputedValues, name: &str) -> Option<String> {
    property_id(name).map(|id| computed_property(values, &id))
}

/// CSSOM "resolved value": [`computed_property`], except that layout-dependent longhands take the used
/// value from `used` when it has one.
pub fn resolved_property(
    values: &ComputedValues,
    property: &PropertyId,
    used: Option<&dyn UsedValues>,
) -> String {
    match property.as_shorthand() {
        Ok(shorthand) => shorthand_value(values, shorthand, used),
        Err(PropertyDeclarationId::Longhand(id)) => longhand_value(values, id, used),
        Err(custom) => values.computed_value_to_string(custom),
    }
}

/// Whether CSSOM's resolved value of `property` can depend on layout (CSSOM §6.7.2: 'line-height',
/// 'height', 'margin-*', 'padding-*', 'width', the insets; and the logical versions of those; CSS
/// Transforms 1 §11: 'transform' as a matrix over the used reference box; CSS Grid 1 §7.2.6: the used
/// track lists of 'grid-template-rows/columns').
pub fn is_layout_dependent(property: LonghandId) -> bool {
    use LonghandId as L;
    matches!(
        property,
        L::LineHeight
            | L::Width
            | L::Height
            | L::InlineSize
            | L::BlockSize
            | L::MarginTop
            | L::MarginRight
            | L::MarginBottom
            | L::MarginLeft
            | L::MarginBlockStart
            | L::MarginBlockEnd
            | L::MarginInlineStart
            | L::MarginInlineEnd
            | L::PaddingTop
            | L::PaddingRight
            | L::PaddingBottom
            | L::PaddingLeft
            | L::PaddingBlockStart
            | L::PaddingBlockEnd
            | L::PaddingInlineStart
            | L::PaddingInlineEnd
            | L::Top
            | L::Right
            | L::Bottom
            | L::Left
            | L::InsetBlockStart
            | L::InsetBlockEnd
            | L::InsetInlineStart
            | L::InsetInlineEnd
            | L::TransformOrigin
            | L::PerspectiveOrigin
            | L::Transform
            | L::GridTemplateColumns
            | L::GridTemplateRows
    )
}

/// Whether `element` is a flex item or a grid item (CSS Flexbox 1 §4, CSS Grid 1 §6): an in-flow
/// element with a box whose flat-tree parent box (past `display: contents` ancestors) is a flex or
/// grid container.
pub fn is_flex_or_grid_item(
    dom: &snowlark_dom::Dom,
    data: &crate::StyleData,
    element: snowlark_dom::NodeId,
) -> bool {
    use style::values::computed::PositionProperty as P;
    use style::values::specified::box_::DisplayInside;
    let Some(own) = data.styles(element) else {
        return false;
    };
    let b = own.get_box();
    if b.display.is_none() || matches!(b.position, P::Absolute | P::Fixed) {
        return false;
    }
    let mut parent = crate::handle::flat_parent(dom, data, element);
    while let Some(p) = parent {
        let Some(ps) = data.styles(p) else {
            return false;
        };
        let display = ps.get_box().display;
        if display.is_contents() {
            parent = crate::handle::flat_parent(dom, data, p);
            continue;
        }
        return matches!(display.inside(), DisplayInside::Flex | DisplayInside::Grid);
    }
    false
}

/// CSS Sizing 3 §5.2 "resolved value" of `min-width`/`min-height: auto` (and their logical
/// forms): `auto` for flex and grid items, zero for every other box and without a box. `None`
/// when `property` is not one of those or its computed value is not `auto`.
pub fn resolved_min_size_auto(
    values: &ComputedValues,
    property: &PropertyId,
    flex_or_grid_item: impl FnOnce() -> bool,
) -> Option<String> {
    let Err(PropertyDeclarationId::Longhand(id)) = property.as_shorthand() else {
        return None;
    };
    let physical = id.to_physical(values.writing_mode);
    if !matches!(physical, LonghandId::MinWidth | LonghandId::MinHeight) || flex_or_grid_item() {
        return None;
    }
    (serialize_longhand(values, physical) == "auto").then(|| "0px".to_owned())
}

/// Whether the resolved value of `property` (a longhand, shorthand or custom property) can need
/// layout: a read of it brings layout up to date, any other read only style (CSSOM §6.7.1 resolves
/// the declaration's values "with style and layout up to date" only as far as they are needed).
pub fn needs_layout(property: &PropertyId) -> bool {
    match property.as_shorthand() {
        Ok(shorthand) => shorthand.longhands().any(is_layout_dependent),
        Err(PropertyDeclarationId::Longhand(id)) => is_layout_dependent(id),
        Err(_) => false,
    }
}

/// The names `getComputedStyle()`'s declaration exposes as its indexed properties: every longhand
/// enabled for content, sorted (CSSOM §6.7 "all longhand properties that are supported CSS properties,
/// in lexicographical order").
pub fn computed_property_names() -> &'static [&'static str] {
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: Vec<&'static str> = NonCustomPropertyId::iter()
            .filter_map(NonCustomPropertyId::as_longhand)
            .map(|id| id.name())
            .filter(|name| PropertyId::parse_enabled_for_all_content(name).is_ok())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    })
}

fn longhand_value(
    values: &ComputedValues,
    id: LonghandId,
    used: Option<&dyn UsedValues>,
) -> String {
    if let Some(used) = used {
        let physical = id.to_physical(values.writing_mode);
        if is_layout_dependent(physical) {
            // 'line-height: normal' resolves to 'normal' whatever layout says.
            let normal_line_height = physical == LonghandId::LineHeight
                && values.computed_value_to_string(PropertyDeclarationId::Longhand(physical))
                    == "normal";
            if !normal_line_height {
                if let Some(v) = used.used_value(physical) {
                    return v;
                }
            }
        }
    }
    serialize_longhand(values, id)
}

fn shorthand_value(
    values: &ComputedValues,
    shorthand: ShorthandId,
    used: Option<&dyn UsedValues>,
) -> String {
    if shorthand == ShorthandId::All {
        return String::new();
    }
    let declarations: Vec<PropertyDeclaration> = shorthand
        .longhands()
        .map(|longhand| {
            let physical = longhand.to_physical(values.writing_mode);
            if used.is_some() && is_layout_dependent(physical) {
                // A used value replaces the computed one: re-parse it as a declaration of the
                // longhand so the shorthand serializer sees it.
                let text = longhand_value(values, physical, used);
                if let Some(d) = parse_longhand(longhand, &text) {
                    return d;
                }
            }
            let mut context = resolved::Context {
                style: values,
                for_property: PropertyId::NonCustom(longhand.into()),
                current_longhand: Some(longhand),
            };
            values.computed_or_resolved_declaration(longhand, Some(&mut context))
        })
        .collect();
    let refs: Vec<&PropertyDeclaration> = declarations.iter().collect();
    let mut css = CssStringWriter::new();
    if shorthand.longhands_to_css(&refs, &mut css).is_err() {
        return String::new();
    }
    css
}

/// Parses `text` as a value of `longhand` (for used values from layout, which are plain lengths).
pub(crate) fn parse_longhand(longhand: LonghandId, text: &str) -> Option<PropertyDeclaration> {
    use style::properties::{parse_one_declaration_into, SourcePropertyDeclaration};
    use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
    use style_traits::ParsingMode;

    static URL: OnceLock<UrlExtraData> = OnceLock::new();
    let url =
        URL.get_or_init(|| UrlExtraData::from(url::Url::parse("about:blank").expect("valid URL")));
    let mut declarations = SourcePropertyDeclaration::default();
    parse_one_declaration_into(
        &mut declarations,
        PropertyId::NonCustom(longhand.into()),
        text,
        Origin::Author,
        url,
        None,
        ParsingMode::DEFAULT,
        style::context::QuirksMode::NoQuirks,
        CssRuleType::Style,
    )
    .ok()?;
    let first = declarations.drain().declarations.next();
    first
}
