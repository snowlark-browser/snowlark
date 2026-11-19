// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Color schemes (CSS Color Adjust 1 §2): an element's used color scheme, and the page color scheme
//! the HTML `color-scheme` `<meta>` sets (HTML §4.2.5.1).
//!
//! stylo computes `color-scheme` and, with Snowlark's patches, decides the used scheme for each style
//! as it builds it (`ComputedValueFlags::USED_DARK_COLOR_SCHEME`), from the element's value, the page
//! color scheme and the user's preferred scheme, by [`used_color_scheme_is_dark`]. System colors,
//! `light-dark()` and the initial `color` (`CanvasText`) resolve against the same decision, and
//! layout reads it through [`used_color_scheme`] for what the user agent paints itself (scroll bars,
//! the canvas surface).

use snowlark_dom::{local_name, ns, Dom, NodeId};
use style::properties::{ComputedValues, LonghandId, PropertyDeclaration};
use style::values::specified::color::ColorSchemeFlags;

use style::computed_value_flags::ComputedValueFlags;
pub use style::device::servo::used_color_scheme_is_dark;

use crate::engine::ColorScheme;

/// The used color scheme of the element (or pseudo-element, or anonymous box) whose computed values
/// these are (CSS Color Adjust 1 §2.3).
pub fn used_color_scheme(values: &ComputedValues) -> ColorScheme {
    if values
        .flags
        .contains(ComputedValueFlags::USED_DARK_COLOR_SCHEME)
    {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    }
}

/// The color schemes a `color-scheme` `<meta>`'s `content` names, if it is a valid CSS
/// `color-scheme` value (HTML §4.2.5.1: "parse a list of component values"; CSS-wide keywords are
/// not `color-scheme` values). `normal` names none.
pub(crate) fn parse_meta_content(content: &str) -> Option<ColorSchemeFlags> {
    match crate::computed::parse_longhand(LonghandId::ColorScheme, content)? {
        PropertyDeclaration::ColorScheme(value) => Some(value.bits),
        _ => None,
    }
}

/// HTML §4.2.5.1 "the page's supported color-schemes": the first `meta` element in tree order that is
/// in a document tree, is named `color-scheme` and has a valid `content` decides it. `candidates`
/// are the `meta` elements that may qualify (any order, stale ids allowed); the ones that can no
/// longer qualify (removed, gone) are dropped from it. Empty: the page sets no color scheme.
pub(crate) fn page_color_scheme(
    dom: &Dom,
    document: NodeId,
    candidates: &mut Vec<NodeId>,
) -> ColorSchemeFlags {
    candidates.retain(|&m| {
        dom.is_alive(m)
            && dom.is_html_element_named(m, &local_name!("meta"))
            && dom.node_document(m) == document
            && dom.root(m) == document
    });
    candidates.sort_by(|&a, &b| dom.tree_order(a, b));
    candidates.dedup();
    for &m in candidates.iter() {
        let named = dom
            .attr(m, &ns!(), &local_name!("name"))
            .is_some_and(|n| n.eq_ignore_ascii_case("color-scheme"));
        if !named {
            continue;
        }
        let Some(content) = dom.attr(m, &ns!(), &local_name!("content")) else {
            continue;
        };
        if let Some(flags) = parse_meta_content(content) {
            return flags;
        }
    }
    ColorSchemeFlags::empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use style::queries::values::PrefersColorScheme;

    const L: ColorSchemeFlags = ColorSchemeFlags::LIGHT;
    const D: ColorSchemeFlags = ColorSchemeFlags::DARK;

    fn none() -> ColorSchemeFlags {
        ColorSchemeFlags::empty()
    }

    #[test]
    fn used_color_scheme_follows_css_color_adjust() {
        let light = PrefersColorScheme::Light;
        let dark = PrefersColorScheme::Dark;
        // `normal` without a page color scheme: the UA's default, light, whatever the preference.
        assert!(!used_color_scheme_is_dark(none(), none(), dark));
        assert!(!used_color_scheme_is_dark(none(), none(), light));
        // One scheme: that one.
        assert!(used_color_scheme_is_dark(D, none(), light));
        assert!(!used_color_scheme_is_dark(L, none(), dark));
        assert!(used_color_scheme_is_dark(
            D | ColorSchemeFlags::ONLY,
            none(),
            light
        ));
        // Both: the preferred one.
        assert!(used_color_scheme_is_dark(L | D, none(), dark));
        assert!(!used_color_scheme_is_dark(L | D, none(), light));
        // `normal` takes the page color scheme; an element's own value wins over it.
        assert!(used_color_scheme_is_dark(none(), D, light));
        assert!(used_color_scheme_is_dark(none(), L | D, dark));
        assert!(!used_color_scheme_is_dark(L, D, dark));
    }

    #[test]
    fn meta_content_is_a_color_scheme_value() {
        assert_eq!(parse_meta_content("dark"), Some(D));
        assert_eq!(parse_meta_content(" light  dark "), Some(L | D));
        assert_eq!(
            parse_meta_content("only dark"),
            Some(D | ColorSchemeFlags::ONLY)
        );
        assert_eq!(parse_meta_content("normal"), Some(none()));
        assert_eq!(parse_meta_content("foo dark"), Some(D));
        assert_eq!(parse_meta_content(""), None);
        assert_eq!(parse_meta_content(",,invalid"), None);
        assert_eq!(parse_meta_content("dark; color: red"), None);
        assert_eq!(parse_meta_content("inherit"), None);
        assert_eq!(parse_meta_content("normal dark"), None);
    }
}
