// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS declaration blocks for the CSSOM (CSSOM §6.6 `CSSStyleDeclaration`): an element's `style`
//! attribute parsed into stylo's `PropertyDeclarationBlock`, with the CSSOM operations
//! (`getPropertyValue`, `getPropertyPriority`, `setProperty`, `removeProperty`, `cssText`, the indexed
//! property names) and their serialization rules (shorthands from their longhands, `!important`).

use std::sync::OnceLock;

use style::properties::{
    parse_one_declaration_into, parse_style_attribute, Importance, NonCustomPropertyId,
    PropertyDeclarationBlock, PropertyId, SourcePropertyDeclaration,
    SourcePropertyDeclarationUpdate,
};
use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
use style_traits::{CssStringWriter, ParsingMode};
use url::Url;

use crate::computed::property_id;

/// A CSS declaration block (an element's inline style, or one built by script).
#[derive(Clone, Debug)]
pub struct DeclarationBlock {
    block: PropertyDeclarationBlock,
    url: UrlExtraData,
}

impl DeclarationBlock {
    /// An empty block whose relative URLs resolve against `base`.
    pub fn new(base: &Url) -> DeclarationBlock {
        crate::prefs::init();
        DeclarationBlock {
            block: PropertyDeclarationBlock::new(),
            url: UrlExtraData::from(base.clone()),
        }
    }

    /// A block from stylo's parts (a style rule's declarations).
    pub(crate) fn from_parts(
        block: PropertyDeclarationBlock,
        url: UrlExtraData,
    ) -> DeclarationBlock {
        DeclarationBlock { block, url }
    }

    pub(crate) fn into_block(self) -> PropertyDeclarationBlock {
        self.block
    }

    pub(crate) fn block(&self) -> &PropertyDeclarationBlock {
        &self.block
    }

    /// CSSOM "parse a CSS declaration block" of a `style` attribute's value.
    pub fn parse(css: &str, base: &Url) -> DeclarationBlock {
        crate::prefs::init();
        let url = UrlExtraData::from(base.clone());
        let block = parse_style_attribute(
            css,
            &url,
            None,
            style::context::QuirksMode::NoQuirks,
            CssRuleType::Style,
        );
        DeclarationBlock { block, url }
    }

    /// CSSOM "serialize a CSS declaration block" (`cssText`).
    pub fn css_text(&self) -> String {
        let mut out = CssStringWriter::new();
        let _ = self.block.to_css(&mut out);
        out
    }

    /// The number of declarations (`length`): longhands and custom properties.
    pub fn len(&self) -> usize {
        self.block.declarations().len()
    }

    pub fn is_empty(&self) -> bool {
        self.block.declarations().is_empty()
    }

    /// The name of the declaration at `index` (`item()`).
    pub fn item(&self, index: usize) -> Option<String> {
        let d = self.block.declarations().get(index)?;
        let mut out = CssStringWriter::new();
        let _ = style_traits::ToCss::to_css(&d.id(), &mut style_traits::CssWriter::new(&mut out));
        Some(out)
    }

    /// `getPropertyValue(property)`.
    pub fn property_value(&self, name: &str) -> String {
        let Some(id) = property_id(name) else {
            return String::new();
        };
        let mut out = CssStringWriter::new();
        let _ = self.block.property_value_to_css(&id, &mut out);
        out
    }

    /// `getPropertyPriority(property)`: "important" or "".
    pub fn property_priority(&self, name: &str) -> &'static str {
        match property_id(name) {
            Some(id) if self.block.property_priority(&id).important() => "important",
            _ => "",
        }
    }

    /// CSSOM `setProperty(property, value, priority)` steps 2-9; an empty value removes the property.
    /// Returns whether the block changed (the caller then updates the `style` attribute).
    pub fn set_property(&mut self, name: &str, value: &str, priority: &str) -> bool {
        let Some(id) = property_id(name) else {
            return false;
        };
        if value.is_empty() {
            return self.remove(&id).1;
        }
        let importance = if priority.is_empty() {
            Importance::Normal
        } else if priority.eq_ignore_ascii_case("important") {
            Importance::Important
        } else {
            return false;
        };
        let mut source = SourcePropertyDeclaration::default();
        if parse_one_declaration_into(
            &mut source,
            id,
            value,
            Origin::Author,
            &self.url,
            None,
            ParsingMode::DEFAULT,
            style::context::QuirksMode::NoQuirks,
            CssRuleType::Style,
        )
        .is_err()
        {
            return false;
        }
        let mut updates = SourcePropertyDeclarationUpdate::default();
        if !self
            .block
            .prepare_for_update(&source, importance, &mut updates)
        {
            return false;
        }
        self.block.update(source.drain(), importance, &mut updates);
        true
    }

    /// `removeProperty(property)`: the removed value and whether anything was removed.
    pub fn remove_property(&mut self, name: &str) -> (String, bool) {
        match property_id(name) {
            Some(id) => self.remove(&id),
            None => (String::new(), false),
        }
    }

    fn remove(&mut self, id: &PropertyId) -> (String, bool) {
        let mut value = CssStringWriter::new();
        let _ = self.block.property_value_to_css(id, &mut value);
        match self.block.first_declaration_to_remove(id) {
            Some(first) => {
                self.block.remove_property(id, first);
                (value, true)
            }
            None => (value, false),
        }
    }

    /// CSS Typed OM §3.2: the declared value of `property` reified, `None` when the block does not
    /// declare it (a shorthand is declared when all its longhands are, and is never reified).
    pub fn typed(&self, name: &str) -> Option<crate::typed::Reified> {
        let id = property_id(name)?;
        let list = self.block.property_value_to_typed_value_list(&id).ok()?;
        if id.as_shorthand().is_ok() {
            return Some(crate::typed::Reified::Unsupported(
                self.property_value(name),
            ));
        }
        Some(crate::typed::reified(name, list, false, || {
            self.property_value(name)
        }))
    }

    /// The names of the declared properties, in order (longhands and custom properties).
    pub fn property_names(&self) -> Vec<String> {
        (0..self.len()).filter_map(|i| self.item(i)).collect()
    }

    /// Replaces the declarations with those parsed from `css` (`cssText` setter).
    pub fn set_css_text(&mut self, css: &str) {
        let url = self.url.clone();
        self.block = parse_style_attribute(
            css,
            &url,
            None,
            style::context::QuirksMode::NoQuirks,
            CssRuleType::Style,
        );
    }
}

/// CSS Conditional 3 `CSS.supports(property, value)`: whether `value` parses for `property`.
pub fn supports_declaration(property: &str, value: &str) -> bool {
    let base = Url::parse("about:blank").expect("valid URL");
    let mut block = DeclarationBlock::new(&base);
    !value.is_empty() && block.set_property(property, value, "")
}

/// CSS Conditional 3 `CSS.supports(conditionText)`: a `<supports-condition>`, or a declaration that
/// becomes one in parentheses.
pub fn supports_condition(text: &str) -> bool {
    use style::parser::ParserContext;
    use style::stylesheets::supports_rule::parse_condition_or_declaration;
    crate::prefs::init();
    let url = UrlExtraData::from(Url::parse("about:blank").expect("valid URL"));
    let context = ParserContext::new(
        Origin::Author,
        &url,
        Some(CssRuleType::Style),
        ParsingMode::DEFAULT,
        style::context::QuirksMode::NoQuirks,
        Default::default(),
        None,
        None,
        Default::default(),
    );
    let mut parser = cssparser::Parser::new(text);
    match parser.parse_entirely(parse_condition_or_declaration) {
        Ok(condition) => condition.eval(&context),
        Err(_) => false,
    }
}

/// The supported CSS properties (CSSOM's "supported CSS property"): every longhand, shorthand and
/// alias enabled for content, sorted. The bindings generator turns them into `CSSStyleProperties`
/// attributes (`crates/snowlark-idl/data/css-properties.txt`, checked against this list by a test).
pub fn supported_property_names() -> &'static [&'static str] {
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        crate::prefs::init();
        let mut names: Vec<&'static str> = NonCustomPropertyId::iter()
            .map(|id| id.name())
            .filter(|name| PropertyId::parse_enabled_for_all_content(name).is_ok())
            // (Gecko's prefixed spellings would make pages take this engine for Firefox.)
            .filter(|name| !name.starts_with("-moz-"))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("http://example.test/").unwrap()
    }

    /// CSS Multi-column Layout 1 §4.4 `column-rule` (CSSOM: the shortest serialization) and
    /// Multi-column Layout 2 `columns` with `/ <'column-height'>`, which resets `column-wrap`.
    #[test]
    fn column_shorthands() {
        let rule = |v: &str| DeclarationBlock::parse(&format!("column-rule: {v}"), &base());
        assert_eq!(rule("10px").property_value("column-rule"), "10px");
        assert_eq!(rule("dotted").property_value("column-rule"), "dotted");
        assert_eq!(rule("red").property_value("column-rule"), "red");
        assert_eq!(
            rule("currentcolor hidden medium").property_value("column-rule"),
            "hidden"
        );
        assert_eq!(
            rule("currentcolor none medium").property_value("column-rule"),
            "medium"
        );
        assert_eq!(
            rule("blue 1px solid").property_value("column-rule"),
            "1px solid blue"
        );

        let columns = |v: &str| DeclarationBlock::parse(&format!("columns: {v}"), &base());
        assert_eq!(
            columns("2 10px / 100px").property_value("columns"),
            "10px 2 / 100px"
        );
        assert_eq!(
            columns("auto / 100px").property_value("columns"),
            "auto / 100px"
        );
        assert_eq!(columns("10px / auto").property_value("columns"), "10px");
        assert_eq!(columns("2 / 3em").property_value("column-height"), "3em");
        assert_eq!(columns("2").property_value("column-wrap"), "auto");
        for invalid in ["/ 100px", "auto /", "2 /", "10px 20px"] {
            assert_eq!(columns(invalid).len(), 0, "{invalid}");
        }
        let mut b = columns("2 / 100px");
        assert!(b.set_property("column-wrap", "nowrap", ""));
        assert_eq!(b.property_value("columns"), "");
        assert!(b.set_property("columns", "2 / 100px", ""));
        assert_eq!(b.property_value("column-wrap"), "auto");
    }

    /// CSS Fragmentation 3 §3: `break-*` with the column values, the legacy `page-break-*`
    /// shorthands, and `orphans` and `widows` (positive integers).
    #[test]
    fn font_palette_takes_names_and_palette_mix() {
        let value = |s: &str| {
            DeclarationBlock::parse(&format!("font-palette: {s}"), &base())
                .property_value("font-palette")
        };
        // CSS Fonts 4 §9.1.
        assert_eq!(value("LIGHT"), "light");
        assert_eq!(value("--foo"), "--foo");
        assert_eq!(value("foo"), "");
        // §9.2 palette-mix(), serialized and its percentages normalized as color-mix()'s.
        for (specified, serialized) in [
            (
                "palette-mix(light 30%, dark)",
                "palette-mix(light 30%, dark 70%)",
            ),
            (
                "palette-mix(in oklab, 30% --foo, normal)",
                "palette-mix(--foo 30%, normal 70%)",
            ),
            (
                "palette-mix(in xyz, light 10%, dark)",
                "palette-mix(in xyz-d65, light 10%, dark 90%)",
            ),
            (
                "palette-mix(in hsl shorter hue, light, dark)",
                "palette-mix(in hsl, light, dark)",
            ),
            (
                "palette-mix(in lch longer hue, light 10%, dark)",
                "palette-mix(in lch longer hue, light 10%, dark 90%)",
            ),
            (
                "palette-mix(in srgb, light 50%, dark 50%)",
                "palette-mix(in srgb, light, dark)",
            ),
            (
                "palette-mix(in srgb, light 75%, dark 75%)",
                "palette-mix(in srgb, light 75%, dark 75%)",
            ),
            (
                "palette-mix(in srgb, light calc(50%), dark)",
                "palette-mix(in srgb, light calc(50%), dark)",
            ),
            (
                "palette-mix(in srgb, dark 100%)",
                "palette-mix(in srgb, dark)",
            ),
            (
                "palette-mix(in srgb, dark 50%, light, --foo)",
                "palette-mix(in srgb, dark 50%, light 25%, --foo 25%)",
            ),
            (
                "palette-mix(palette-mix(in srgb, light 30%, normal) 20%, dark)",
                "palette-mix(palette-mix(in srgb, light 30%, normal 70%) 20%, dark 80%)",
            ),
        ] {
            assert_eq!(value(specified), serialized, "{specified}");
        }
        for invalid in [
            "palette-mix()",
            "palette-mix(in srgb)",
            "palette-mix(light 120%, dark)",
            "palette-mix(light, foo)",
            "palette-mix(light 10% 20%, dark)",
        ] {
            assert_eq!(value(invalid), "", "{invalid}");
        }
    }

    #[test]
    fn font_variant_emoji_is_a_longhand_of_font_variant() {
        let parse = |s: &str| DeclarationBlock::parse(s, &base());
        // CSS Fonts 4 §6.13.
        for v in ["normal", "text", "emoji", "unicode"] {
            let b = parse(&format!("font-variant-emoji: {v}"));
            assert_eq!(b.property_value("font-variant-emoji"), v);
        }
        assert_eq!(parse("font-variant-emoji: color").len(), 0);
        let b = parse("font-variant: small-caps emoji");
        assert_eq!(b.property_value("font-variant-emoji"), "emoji");
        assert_eq!(b.property_value("font-variant"), "small-caps emoji");
        let b = parse("font-variant: text");
        assert_eq!(b.property_value("font-variant"), "text");
        // `font` resets it, and serializes as "" while it is not initial.
        let b = parse("font-variant-emoji: text; font: 16px serif");
        assert_eq!(b.property_value("font-variant-emoji"), "normal");
        let b = parse("font: 16px serif; font-variant-emoji: text");
        assert_eq!(b.property_value("font"), "");
    }

    #[test]
    fn font_variant_caps_synthesis_and_system_fonts() {
        let parse = |s: &str| DeclarationBlock::parse(s, &base());
        // CSS Fonts 4 §6.6: every font-variant-caps value; the font shorthand takes only small-caps.
        for v in [
            "all-small-caps",
            "petite-caps",
            "all-petite-caps",
            "unicase",
            "titling-caps",
        ] {
            let b = parse(&format!("font-variant-caps: {v}"));
            assert_eq!(b.property_value("font-variant-caps"), v);
            assert_eq!(
                parse(&format!("font-variant: {v}")).property_value("font-variant"),
                v
            );
        }
        assert_eq!(parse("font: petite-caps 16px serif").len(), 0);
        let b = parse("font: 16px serif; font-variant-caps: all-small-caps");
        assert_eq!(b.property_value("font"), "");
        // §5.2-5.6 font-synthesis and its longhands.
        let b = parse("font-synthesis: style small-caps");
        assert_eq!(b.property_value("font-synthesis"), "style small-caps");
        assert_eq!(b.property_value("font-synthesis-weight"), "none");
        assert_eq!(b.property_value("font-synthesis-position"), "none");
        assert_eq!(
            parse("font-synthesis: none").property_value("font-synthesis"),
            "none"
        );
        assert_eq!(
            parse("font-synthesis-style: none").property_value("font-synthesis-style"),
            "none"
        );
        assert_eq!(parse("font-synthesis: bold").len(), 0);
        // §3.7.1 system font keywords: the shorthand as a whole.
        for v in [
            "caption",
            "icon",
            "menu",
            "message-box",
            "small-caption",
            "status-bar",
        ] {
            let b = parse(&format!("font: {v}"));
            assert_eq!(b.property_value("font"), v);
            assert_eq!(b.property_value("font-family"), "");
            assert_eq!(b.property_value("line-height"), "normal");
        }
        assert_eq!(parse("font: caption 12px").len(), 0);
        assert_eq!(
            parse("font: 12px caption").property_value("font-family"),
            "caption"
        );
        let b = parse("font: menu; font-weight: bold");
        assert_eq!(b.property_value("font"), "");
    }

    #[test]
    fn break_properties() {
        let parse = |s: &str| DeclarationBlock::parse(s, &base());
        assert_eq!(
            parse("break-before: column").property_value("break-before"),
            "column"
        );
        assert_eq!(
            parse("break-after: avoid-column").property_value("break-after"),
            "avoid-column"
        );
        assert_eq!(
            parse("break-inside: avoid").property_value("break-inside"),
            "avoid"
        );
        let legacy = parse("page-break-before: always");
        assert_eq!(legacy.property_value("break-before"), "page");
        assert_eq!(legacy.property_value("page-break-before"), "always");
        assert_eq!(parse("page-break-after: column").len(), 0);
        assert_eq!(parse("orphans: 3").property_value("orphans"), "3");
        assert_eq!(parse("widows: 1").property_value("widows"), "1");
        for invalid in ["orphans: 0", "widows: -1", "orphans: 1.5"] {
            assert_eq!(parse(invalid).len(), 0, "{invalid}");
        }
    }

    #[test]
    fn inline_style_operations() {
        let mut b = DeclarationBlock::parse("color: red; margin: 1px 2px; bogus: 1", &base());
        assert_eq!(b.property_value("color"), "red");
        assert_eq!(b.property_value("margin"), "1px 2px");
        assert_eq!(b.property_value("margin-left"), "2px");
        assert_eq!(b.len(), 5);
        assert_eq!(b.item(0).as_deref(), Some("color"));
        assert_eq!(b.css_text(), "color: red; margin: 1px 2px;");
        assert!(b.set_property("COLOR", "blue", "important"));
        assert_eq!(b.property_priority("color"), "important");
        assert!(!b.set_property("color", "not a colour", ""));
        assert!(!b.set_property("color", "green", "bogus"));
        assert_eq!(b.remove_property("margin"), ("1px 2px".to_string(), true));
        assert_eq!(b.css_text(), "color: blue !important;");
        assert!(b.set_property("--x", " 5 ", ""));
        assert_eq!(b.property_value("--x"), "5");
        assert!(b.set_property("color", "", ""));
        assert_eq!(b.css_text(), "--x: 5;");
        b.set_css_text("padding: 3px");
        assert_eq!(b.property_value("padding-top"), "3px");
    }

    #[test]
    fn supports() {
        assert!(supports_declaration("display", "flex"));
        assert!(!supports_declaration("display", "bogus"));
        assert!(supports_condition(
            "(display: grid) and (not (display: bogus))"
        ));
        assert!(supports_condition("display: block"));
        assert!(!supports_condition("(display: bogus)"));
    }

    #[test]
    fn supported_properties_match_the_generator_data() {
        let names = supported_property_names();
        assert!(names.contains(&"background-color"));
        assert!(names.contains(&"-webkit-transform"));
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../snowlark-idl/data/css-properties.txt"
        );
        let expected: String = names.iter().map(|n| format!("{n}\n")).collect();
        if std::env::var("SNOWLARK_UPDATE_CSS_PROPERTIES").is_ok_and(|v| v == "1") {
            std::fs::write(path, &expected).unwrap();
        }
        let have = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            have == expected,
            "{path} is out of date: run this test with SNOWLARK_UPDATE_CSS_PROPERTIES=1, then \
             `snowlark-idl gen`"
        );
    }
}
