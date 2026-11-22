// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CSSOM's view of parsed stylesheets (CSSOM §6.4 `CSSRule` and friends) over stylo's rule tree:
//! rule lists with `insertRule`/`deleteRule` (CSSOM "insert a CSS rule", "remove a CSS rule"), each
//! rule's type, `cssText`, `selectorText`, the declarations of style rules, the media and condition
//! text of grouping rules, and the sheets of `@import` rules. The page layer wraps these in platform
//! objects; a change through them is followed by `StyleEngine::sheets_changed`.

use servo_arc::Arc;
use style::shared_lock::{Locked, SharedRwLock, ToCssWithGuard};
use style::stylesheets::keyframes_rule::{Keyframe, KeyframesRule};
use style::stylesheets::{
    AllowImportRules, CssRule, CssRuleType, CssRuleTypes, CssRules, RulesMutateError,
    StylesheetInDocument,
};
use style_traits::{CssStringWriter, ToCss};

use crate::sheets::Sheet;

/// Why a rule list change failed (the CSSOM exceptions).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleError {
    Syntax,
    IndexSize,
    HierarchyRequest,
    InvalidState,
}

impl From<RulesMutateError> for RuleError {
    fn from(e: RulesMutateError) -> Self {
        match e {
            RulesMutateError::Syntax => RuleError::Syntax,
            RulesMutateError::IndexSize => RuleError::IndexSize,
            RulesMutateError::HierarchyRequest => RuleError::HierarchyRequest,
            RulesMutateError::InvalidState => RuleError::InvalidState,
        }
    }
}

/// A list of rules: a sheet's, a grouping rule's, or a `@keyframes` rule's keyframes.
#[derive(Clone)]
pub struct RuleList {
    source: Source,
    sheet: Sheet,
    containing: CssRuleTypes,
    relative: Option<CssRuleType>,
}

/// What a [`RuleList`] lists.
#[derive(Clone)]
enum Source {
    Rules(Arc<Locked<CssRules>>),
    /// The keyframes of a `@keyframes` rule (CSS Animations 1 §6.2 `cssRules`).
    Keyframes(Arc<Locked<KeyframesRule>>),
}

/// One rule of a sheet.
#[derive(Clone)]
pub struct Rule {
    rule: CssRule,
    sheet: Sheet,
    containing: CssRuleTypes,
    /// A keyframe of the `@keyframes` rule `rule` (a `CSSKeyframeRule`), when this is one.
    keyframe: Option<Arc<Locked<Keyframe>>>,
}

/// What kind of rule a [`Rule`] is (which CSSOM interface it gets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleKind {
    Style,
    Import,
    Media,
    Supports,
    FontFace,
    Keyframes,
    /// A keyframe in a `@keyframes` rule.
    Keyframe,
    Namespace,
    Page,
    LayerBlock,
    LayerStatement,
    Container,
    Property,
    FontFeatureValues,
    FontPaletteValues,
    /// A rule CSSOM has an interface for that this engine does not expose yet.
    Other,
}

/// The attributes of a `CSSFontPaletteValuesRule`, serialized.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FontPaletteValues {
    pub name: String,
    pub font_family: String,
    pub base_palette: String,
    pub override_colors: String,
}

/// The attributes of a `CSSPropertyRule` (CSS Properties and Values API 1 §5.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropertyRuleValues {
    /// The custom property name, with its `--`, serialized as an identifier.
    pub name: String,
    /// The `syntax` descriptor's string as written (the universal syntax as `*`).
    pub syntax: String,
    pub inherits: bool,
    /// The `initial-value` descriptor's tokens, `None` when it has none.
    pub initial_value: Option<String>,
}

/// The address of a shared value (an identity).
fn ptr<T>(a: &Arc<T>) -> usize {
    &**a as *const T as usize
}

/// The list holding the rule whose identity is `id`, and its index there, searched depth first.
fn find_rule(list: &RuleList, id: usize) -> Option<(RuleList, usize)> {
    for i in 0..list.len() {
        let rule = list.get(i)?;
        if rule.id() == id {
            return Some((list.clone(), i));
        }
        if let Some(found) = rule.child_rules().and_then(|c| find_rule(&c, id)) {
            return Some(found);
        }
    }
    None
}

fn lock(sheet: &Sheet) -> &SharedRwLock {
    &sheet.stylo().shared_lock
}

impl Sheet {
    /// The sheet's top-level rules (`cssRules`).
    pub fn rules(&self) -> RuleList {
        self.ensure_unique_contents();
        let guard = lock(self).read();
        let rules = self.0 .0.contents(&guard).rules.clone();
        drop(guard);
        RuleList {
            source: Source::Rules(rules),
            sheet: self.clone(),
            containing: CssRuleTypes::default(),
            relative: None,
        }
    }

    /// The sheet's media list, serialized (`media.mediaText`).
    pub fn media_text(&self) -> String {
        let guard = lock(self).read();
        let mut out = String::new();
        let _ = self
            .stylo()
            .media
            .read_with(&guard)
            .to_css(&mut style_traits::CssWriter::new(&mut out));
        out
    }

    /// Replaces the sheet's rules with those parsed from `text` (`replaceSync`).
    pub fn replace_rules(&self, text: &str) {
        self.ensure_unique_contents();
        let lock = lock(self);
        let (base, rules) = {
            let guard = lock.read();
            let contents = self.0 .0.contents(&guard);
            (contents.url_data.clone(), contents.rules.clone())
        };
        let base: url::Url = (*base.0).clone();
        let parsed = crate::sheets::parse_sheet(
            text,
            &base,
            "",
            crate::sheets::SheetOrigin::Author,
            lock,
            style::context::QuirksMode::NoQuirks,
            None,
        );
        let new_rules = {
            let guard = lock.read();
            let contents = parsed.0 .0.contents(&guard);
            contents.rules.read_with(&guard).0.clone()
        };
        let mut guard = lock.write();
        rules.write_with(&mut guard).0 = new_rules;
    }

    /// Whether two sheets are the same parsed sheet (pointer identity).
    pub fn id(&self) -> usize {
        ptr(self.stylo())
    }
}

impl RuleList {
    /// A stable identity for the list (the CSSOM's `[SameObject]`).
    pub fn id(&self) -> usize {
        match &self.source {
            Source::Rules(r) => ptr(r),
            // (the list of a `@keyframes` rule is an object of its own beside the rule)
            Source::Keyframes(r) => ptr(r) + 1,
        }
    }

    pub fn len(&self) -> usize {
        let guard = lock(&self.sheet).read();
        match &self.source {
            Source::Rules(r) => r.read_with(&guard).0.len(),
            Source::Keyframes(r) => r.read_with(&guard).keyframes.len(),
        }
    }

    /// The rules of a list of rules; `None` for a list of keyframes.
    fn css_rules(&self) -> Option<&Arc<Locked<CssRules>>> {
        match &self.source {
            Source::Rules(r) => Some(r),
            Source::Keyframes(_) => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The rule at `index`.
    pub fn get(&self, index: usize) -> Option<Rule> {
        let guard = lock(&self.sheet).read();
        match &self.source {
            Source::Rules(r) => {
                let rule = r.read_with(&guard).0.get(index)?.clone();
                Some(Rule {
                    rule,
                    sheet: self.sheet.clone(),
                    containing: self.containing,
                    keyframe: None,
                })
            }
            Source::Keyframes(r) => {
                let keyframe = r.read_with(&guard).keyframes.get(index)?.clone();
                Some(Rule {
                    rule: CssRule::Keyframes(r.clone()),
                    sheet: self.sheet.clone(),
                    containing: self.containing,
                    keyframe: Some(keyframe),
                })
            }
        }
    }

    /// CSSOM "insert a CSS rule": parses `text` and inserts it at `index`. Returns the index.
    pub fn insert(&self, text: &str, index: usize) -> Result<usize, RuleError> {
        let lock = lock(&self.sheet);
        let rules = self.css_rules().ok_or(RuleError::HierarchyRequest)?;
        let rule = {
            let guard = lock.read();
            let contents = self.sheet.0 .0.contents(&guard);
            rules.read_with(&guard).parse_rule_for_insert(
                lock,
                text,
                contents,
                index,
                self.containing,
                self.relative,
                None,
                AllowImportRules::Yes,
            )?
        };
        let mut guard = lock.write();
        rules.write_with(&mut guard).0.insert(index, rule);
        Ok(index)
    }

    /// CSSOM "remove a CSS rule".
    pub fn remove(&self, index: usize) -> Result<(), RuleError> {
        let rules = self.css_rules().ok_or(RuleError::HierarchyRequest)?;
        let mut guard = lock(&self.sheet).write();
        rules.write_with(&mut guard).remove_rule(index)?;
        Ok(())
    }
}

impl Rule {
    /// A stable identity for the rule (its shared pointer).
    pub fn id(&self) -> usize {
        if let Some(k) = &self.keyframe {
            return ptr(k);
        }
        match &self.rule {
            CssRule::Style(r) => ptr(r),
            CssRule::Namespace(r) => ptr(r),
            CssRule::Import(r) => ptr(r),
            CssRule::Media(r) => ptr(r),
            CssRule::CustomMedia(r) => ptr(r),
            CssRule::Container(r) => ptr(r),
            CssRule::FontFace(r) => ptr(r),
            CssRule::FontFeatureValues(r) => ptr(r),
            CssRule::FontPaletteValues(r) => ptr(r),
            CssRule::CounterStyle(r) => ptr(r),
            CssRule::Keyframes(r) => ptr(r),
            CssRule::Margin(r) => ptr(r),
            CssRule::Supports(r) => ptr(r),
            CssRule::Page(r) => ptr(r),
            CssRule::Property(r) => ptr(r),
            CssRule::Document(r) => ptr(r),
            CssRule::LayerBlock(r) => ptr(r),
            CssRule::LayerStatement(r) => ptr(r),
            CssRule::Scope(r) => ptr(r),
            CssRule::StartingStyle(r) => ptr(r),
            CssRule::AppearanceBase(r) => ptr(r),
            CssRule::PositionTry(r) => ptr(r),
            CssRule::NestedDeclarations(r) => ptr(r),
            CssRule::ViewTransition(r) => ptr(r),
        }
    }

    pub fn kind(&self) -> RuleKind {
        if self.keyframe.is_some() {
            return RuleKind::Keyframe;
        }
        match &self.rule {
            CssRule::Style(_) => RuleKind::Style,
            CssRule::Import(_) => RuleKind::Import,
            CssRule::Media(_) => RuleKind::Media,
            CssRule::Supports(_) => RuleKind::Supports,
            CssRule::FontFace(_) => RuleKind::FontFace,
            CssRule::Keyframes(_) => RuleKind::Keyframes,
            CssRule::Namespace(_) => RuleKind::Namespace,
            CssRule::Page(_) => RuleKind::Page,
            CssRule::LayerBlock(_) => RuleKind::LayerBlock,
            CssRule::LayerStatement(_) => RuleKind::LayerStatement,
            CssRule::Container(_) => RuleKind::Container,
            CssRule::Property(_) => RuleKind::Property,
            CssRule::FontFeatureValues(_) => RuleKind::FontFeatureValues,
            CssRule::FontPaletteValues(_) => RuleKind::FontPaletteValues,
            _ => RuleKind::Other,
        }
    }

    /// The legacy `CSSRule.type` code (0 for rules without one).
    pub fn type_code(&self) -> u16 {
        match self.kind() {
            RuleKind::Style => 1,
            RuleKind::Import => 3,
            RuleKind::Media => 4,
            RuleKind::FontFace => 5,
            RuleKind::Page => 6,
            RuleKind::Keyframes => 7,
            RuleKind::Keyframe => 8,
            RuleKind::Namespace => 10,
            RuleKind::Supports => 12,
            RuleKind::FontFeatureValues => 14,
            _ => 0,
        }
    }

    /// CSSOM "serialize a CSS rule" (`cssText`).
    pub fn css_text(&self) -> String {
        let guard = lock(&self.sheet).read();
        let mut out = CssStringWriter::new();
        let _ = match &self.keyframe {
            Some(k) => k.read_with(&guard).to_css(&guard, &mut out),
            None => self.rule.to_css(&guard, &mut out),
        };
        out
    }

    /// `CSSFontPaletteValuesRule`'s attributes (CSS Fonts 4 §9.3.1): the rule's name and each
    /// descriptor's value as it serializes, the empty string for one not set.
    pub fn font_palette_values(&self) -> Option<FontPaletteValues> {
        let CssRule::FontPaletteValues(r) = &self.rule else {
            return None;
        };
        fn css(value: &impl style_traits::ToCss) -> String {
            let mut out = String::new();
            let _ = value.to_css(&mut style_traits::CssWriter::new(&mut out));
            out
        }
        Some(FontPaletteValues {
            name: css(&r.name),
            font_family: if r.family_names.is_empty() {
                String::new()
            } else {
                css(&r.family_names)
            },
            base_palette: r.base_palette.as_ref().map(css).unwrap_or_default(),
            override_colors: if r.override_colors.is_empty() {
                String::new()
            } else {
                css(&r.override_colors)
            },
        })
    }

    /// `CSSPropertyRule`'s attributes (CSS Properties and Values API 1 §5.1). A rule that is
    /// in a sheet is valid, so its `syntax` and `inherits` descriptors are present.
    pub fn property_rule(&self) -> Option<PropertyRuleValues> {
        let CssRule::Property(r) = &self.rule else {
            return None;
        };
        let mut name = String::new();
        let _ = style_traits::ToCss::to_css(&r.name, &mut style_traits::CssWriter::new(&mut name));
        let syntax = match &r.descriptors.syntax {
            Some(s) => match s.specified_string() {
                Some(text) => text.to_owned(),
                None => {
                    let mut out = String::new();
                    let _ =
                        style_traits::ToCss::to_css(s, &mut style_traits::CssWriter::new(&mut out));
                    out
                }
            },
            None => "*".to_owned(),
        };
        Some(PropertyRuleValues {
            name,
            syntax,
            inherits: r.descriptors.inherits(),
            initial_value: r
                .descriptors
                .initial_value
                .as_ref()
                .map(|v| v.css_text().to_owned()),
        })
    }

    /// `CSSFontFeatureValuesRule.fontFamily` (CSS Fonts 4 §8.1.1): the rule's family names,
    /// serialized as a list.
    pub fn font_feature_values_family(&self) -> Option<String> {
        let CssRule::FontFeatureValues(r) = &self.rule else {
            return None;
        };
        let mut out = String::new();
        let _ = r
            .family_names
            .to_css(&mut style_traits::CssWriter::new(&mut out));
        Some(out)
    }

    /// Setting `CSSFontFeatureValuesRule.fontFamily`: `text` parsed as a list of family names (an
    /// invalid one changes nothing). Stylo's rule is shared and immutable, so the list that holds it
    /// gets a copy with the new names in its place; the returned rule is what this one became
    /// (`None`: nothing changed).
    pub fn set_font_feature_values_family(&self, text: &str) -> Option<Rule> {
        let CssRule::FontFeatureValues(old) = &self.rule else {
            return None;
        };
        let url = {
            let guard = lock(&self.sheet).read();
            self.sheet.0 .0.contents(&guard).url_data.clone()
        };
        let context = style::parser::ParserContext::new(
            style::stylesheets::Origin::Author,
            &url,
            Some(CssRuleType::FontFeatureValues),
            style_traits::ParsingMode::DEFAULT,
            style::context::QuirksMode::NoQuirks,
            Default::default(),
            None,
            None,
            Default::default(),
        );
        let names = cssparser::Parser::new(text)
            .parse_entirely(|p| {
                style::stylesheets::font_feature_values_rule::parse_family_name_list(&context, p)
            })
            .ok()?;
        let (list, index) = find_rule(&self.sheet.rules(), ptr(old))?;
        let mut copy = (**old).clone();
        copy.family_names = names;
        let rule = CssRule::FontFeatureValues(Arc::new(copy));
        let mut guard = lock(&self.sheet).write();
        list.css_rules()?.write_with(&mut guard).0[index] = rule.clone();
        Some(Rule {
            rule,
            sheet: self.sheet.clone(),
            containing: self.containing,
            keyframe: None,
        })
    }

    /// `CSSStyleRule.selectorText`.
    pub fn selector_text(&self) -> Option<String> {
        let CssRule::Style(r) = &self.rule else {
            return None;
        };
        let guard = lock(&self.sheet).read();
        let mut out = String::new();
        let _ = cssparser::ToCss::to_css(&r.read_with(&guard).selectors, &mut out);
        Some(out)
    }

    /// Sets `selectorText` when it parses as a selector list. Returns whether it changed.
    pub fn set_selector_text(&self, text: &str) -> bool {
        let CssRule::Style(r) = &self.rule else {
            return false;
        };
        let url = {
            let guard = lock(&self.sheet).read();
            self.sheet.0 .0.contents(&guard).url_data.clone()
        };
        let Ok(list) =
            style::selector_parser::SelectorParser::parse_author_origin_no_namespace(text, &url)
        else {
            return false;
        };
        let mut guard = lock(&self.sheet).write();
        r.write_with(&mut guard).selectors = list;
        true
    }

    /// The child rules of a grouping rule (`@media`, `@supports`, `@layer {}`, `@container`, nested
    /// style rules).
    pub fn child_rules(&self) -> Option<RuleList> {
        if self.keyframe.is_some() {
            return None;
        }
        let guard = lock(&self.sheet).read();
        let (rules, ty) = match &self.rule {
            CssRule::Media(r) => (r.rules.clone(), CssRuleType::Media),
            CssRule::Supports(r) => (r.rules.clone(), CssRuleType::Supports),
            CssRule::LayerBlock(r) => (r.rules.clone(), CssRuleType::LayerBlock),
            CssRule::Container(r) => (r.rules.clone(), CssRuleType::Container),
            CssRule::Style(r) => {
                let r = r.read_with(&guard);
                let rules = r.rules.clone()?;
                (rules, CssRuleType::Style)
            }
            _ => return None,
        };
        let mut containing = self.containing;
        containing.insert(ty);
        Some(RuleList {
            source: Source::Rules(rules),
            sheet: self.sheet.clone(),
            containing,
            relative: (ty == CssRuleType::Style).then_some(CssRuleType::Style),
        })
    }

    /// The declarations of a style rule (`CSSStyleRule.style`).
    pub fn declarations(&self) -> Option<RuleDeclarations> {
        let guard = lock(&self.sheet).read();
        let block = match (&self.keyframe, &self.rule) {
            (Some(k), _) => k.read_with(&guard).block.clone(),
            (None, CssRule::Style(r)) => r.read_with(&guard).block.clone(),
            (None, CssRule::Page(r)) => r.read_with(&guard).block.clone(),
            _ => return None,
        };
        let url = self.sheet.0 .0.contents(&guard).url_data.clone();
        Some(RuleDeclarations {
            block,
            sheet: self.sheet.clone(),
            url,
        })
    }

    /// Runs `f` on the descriptors of a `@font-face` rule (`CSSFontFaceRule.style`, CSS Fonts 5
    /// `CSSFontFaceDescriptors`) with a parser context for its sheet; `None` for another kind of rule.
    fn with_font_face<R>(
        &self,
        f: impl FnOnce(&mut style::font_face::Descriptors, &style::parser::ParserContext) -> R,
    ) -> Option<R> {
        let CssRule::FontFace(rule) = &self.rule else {
            return None;
        };
        let url = {
            let guard = lock(&self.sheet).read();
            self.sheet.0 .0.contents(&guard).url_data.clone()
        };
        let context = style::parser::ParserContext::new(
            style::stylesheets::Origin::Author,
            &url,
            Some(CssRuleType::FontFace),
            style_traits::ParsingMode::DEFAULT,
            style::context::QuirksMode::NoQuirks,
            Default::default(),
            None,
            None,
            Default::default(),
        );
        let mut guard = lock(&self.sheet).write();
        let rule = rule.write_with(&mut guard);
        Some(f(&mut rule.descriptors, &context))
    }

    /// The serialized value of a `@font-face` descriptor by its name (or alias); empty when it is
    /// not set or not a descriptor. `None` for another kind of rule.
    pub fn font_face_descriptor(&self, name: &str) -> Option<String> {
        use style::font_face::DescriptorId;
        self.with_font_face(|d, _| {
            let mut out = String::new();
            if let Ok(id) = DescriptorId::from_ident(name) {
                let _ = d.get(id, &mut out);
            }
            out
        })
    }

    /// Sets a `@font-face` descriptor from its text (CSSOM `setProperty`: an empty value removes it,
    /// an invalid one changes nothing). Whether the rule changed.
    pub fn set_font_face_descriptor(&self, name: &str, value: &str) -> bool {
        use style::font_face::DescriptorId;
        let Ok(id) = DescriptorId::from_ident(name) else {
            return false;
        };
        self.with_font_face(|d, context| {
            if value.is_empty() {
                return d.remove(id);
            }
            let mut parser = cssparser::Parser::new(value);
            d.set(id, context, &mut parser).unwrap_or(false)
        })
        .unwrap_or(false)
    }

    /// Removes a `@font-face` descriptor (CSSOM `removeProperty`): its old value, and whether it was
    /// set.
    pub fn remove_font_face_descriptor(&self, name: &str) -> (String, bool) {
        use style::font_face::DescriptorId;
        let Ok(id) = DescriptorId::from_ident(name) else {
            return (String::new(), false);
        };
        self.with_font_face(|d, _| {
            let mut out = String::new();
            let _ = d.get(id, &mut out);
            (out, d.remove(id))
        })
        .unwrap_or_default()
    }

    /// How many descriptors a `@font-face` rule sets.
    pub fn font_face_len(&self) -> usize {
        self.with_font_face(|d, _| d.len()).unwrap_or(0)
    }

    /// The name of the descriptor at `index` of a `@font-face` rule.
    pub fn font_face_item(&self, index: usize) -> Option<String> {
        self.with_font_face(|d, _| d.at(index).map(|id| id.name().to_owned()))
            .flatten()
    }

    /// The descriptors of a `@font-face` rule as `cssText` serializes them.
    pub fn font_face_css_text(&self) -> String {
        self.with_font_face(|d, _| {
            let mut out = String::new();
            let mut w = style_traits::CssWriter::new(&mut out);
            let _ = d.to_css(&mut w);
            out.trim_end().to_owned()
        })
        .unwrap_or_default()
    }

    /// Replaces the descriptors of a `@font-face` rule with those parsed from `text` (CSSOM
    /// setting `cssText`). Whether the rule changed.
    pub fn set_font_face_css_text(&self, text: &str) -> bool {
        self.with_font_face(|d, context| {
            let mut parser = cssparser::Parser::new(text);
            let rule = style::font_face::parse_font_face_block(
                context,
                &mut parser,
                cssparser::SourceLocation { line: 0, column: 0 },
            );
            let changed = *d != rule.descriptors;
            *d = rule.descriptors;
            changed
        })
        .unwrap_or(false)
    }

    /// The media list of an `@media` or `@import` rule, serialized.
    pub fn media_text(&self) -> Option<String> {
        let guard = lock(&self.sheet).read();
        let mut out = String::new();
        let mut w = style_traits::CssWriter::new(&mut out);
        match &self.rule {
            CssRule::Media(r) => {
                let _ = r.media_queries.read_with(&guard).to_css(&mut w);
            }
            CssRule::Import(r) => {
                let _ = r.read_with(&guard).stylesheet.media(&guard)?.to_css(&mut w);
            }
            _ => return None,
        }
        Some(out)
    }

    /// The `conditionText` of a conditional group rule.
    pub fn condition_text(&self) -> Option<String> {
        let guard = lock(&self.sheet).read();
        let mut out = String::new();
        match &self.rule {
            CssRule::Media(r) => {
                let _ = r
                    .media_queries
                    .read_with(&guard)
                    .to_css(&mut style_traits::CssWriter::new(&mut out));
            }
            CssRule::Supports(r) => {
                let _ = r
                    .condition
                    .to_css(&mut style_traits::CssWriter::new(&mut out));
            }
            CssRule::Container(r) => {
                let _ = r
                    .conditions
                    .to_css(&mut style_traits::CssWriter::new(&mut out));
            }
            _ => return None,
        }
        Some(out)
    }

    /// CSS Conditional 5 §6.1 `CSSContainerRule.conditions`: each `<container-condition>`'s name
    /// and query, serialized ("" for what it lacks).
    pub fn container_conditions(&self) -> Option<Vec<(String, String)>> {
        let CssRule::Container(r) = &self.rule else {
            return None;
        };
        let conditions = r
            .conditions
            .0
            .iter()
            .map(|c| {
                let mut name = String::new();
                if !c.name().is_none() {
                    let _ = c
                        .name()
                        .to_css(&mut style_traits::CssWriter::new(&mut name));
                }
                let mut query = String::new();
                if let Some(q) = c.query_condition() {
                    let _ = q.to_css(&mut style_traits::CssWriter::new(&mut query));
                }
                (name, query)
            })
            .collect();
        Some(conditions)
    }

    /// CSS Cascade 5 `CSSImportRule.layerName`: `None` without `layer`, "" for an anonymous layer.
    pub fn import_layer_name(&self) -> Option<String> {
        use style::stylesheets::import_rule::ImportLayer;
        let CssRule::Import(r) = &self.rule else {
            return None;
        };
        let guard = lock(&self.sheet).read();
        match &r.read_with(&guard).layer {
            ImportLayer::None => None,
            ImportLayer::Anonymous => Some(String::new()),
            ImportLayer::Named(name) => {
                let mut out = String::new();
                let _ = name.to_css(&mut style_traits::CssWriter::new(&mut out));
                Some(out)
            }
        }
    }

    /// CSS Conditional 5 `CSSImportRule.supportsText`: the `supports()` condition, serialized.
    pub fn import_supports_text(&self) -> Option<String> {
        let CssRule::Import(r) = &self.rule else {
            return None;
        };
        let guard = lock(&self.sheet).read();
        let supports = r.read_with(&guard).supports.as_ref()?;
        let mut out = String::new();
        let _ = supports
            .condition
            .to_css(&mut style_traits::CssWriter::new(&mut out));
        Some(out)
    }

    /// An `@import` rule's URL as written.
    pub fn import_href(&self) -> Option<String> {
        let CssRule::Import(r) = &self.rule else {
            return None;
        };
        let guard = lock(&self.sheet).read();
        Some(r.read_with(&guard).url.as_str().to_owned())
    }

    /// An `@import` rule's sheet, when it loaded.
    pub fn import_sheet(&self) -> Option<Sheet> {
        let CssRule::Import(r) = &self.rule else {
            return None;
        };
        let guard = lock(&self.sheet).read();
        let sheet = r.read_with(&guard).stylesheet.as_sheet()?.clone();
        Some(Sheet(style::stylesheets::DocumentStyleSheet(sheet)))
    }

    /// A `@keyframes` rule's name.
    pub fn keyframes_name(&self) -> Option<String> {
        let CssRule::Keyframes(r) = &self.rule else {
            return None;
        };
        let guard = lock(&self.sheet).read();
        Some(r.read_with(&guard).name.as_atom().to_string())
    }

    /// The `@keyframes` rule itself, unless this is one of its keyframes.
    fn keyframes_rule(&self) -> Option<&Arc<Locked<KeyframesRule>>> {
        match (&self.rule, &self.keyframe) {
            (CssRule::Keyframes(r), None) => Some(r),
            _ => None,
        }
    }

    /// CSS Animations 1 §6.2: sets a `@keyframes` rule's `name` (a CSS-wide keyword or `none`
    /// then serializes as a string). Returns whether it changed.
    pub fn set_keyframes_name(&self, name: &str) -> bool {
        let Some(r) = self.keyframes_rule() else {
            return false;
        };
        let mut guard = lock(&self.sheet).write();
        let rule = r.write_with(&mut guard);
        let name = style::values::KeyframesName::from_ident(name);
        if rule.name == name {
            return false;
        }
        rule.name = name;
        true
    }

    /// The keyframes of a `@keyframes` rule (`CSSKeyframesRule.cssRules`).
    pub fn keyframe_rules(&self) -> Option<RuleList> {
        let r = self.keyframes_rule()?;
        let mut containing = self.containing;
        containing.insert(CssRuleType::Keyframes);
        Some(RuleList {
            source: Source::Keyframes(r.clone()),
            sheet: self.sheet.clone(),
            containing,
            relative: None,
        })
    }

    /// CSS Animations 1 §6.2 `appendRule()`: parses `text` as one keyframe and appends it; a text
    /// that is not exactly one keyframe is ignored. Returns whether one was appended.
    pub fn append_keyframe(&self, text: &str) -> bool {
        let Some(r) = self.keyframes_rule() else {
            return false;
        };
        let lock = lock(&self.sheet);
        let keyframe = {
            let guard = lock.read();
            let contents = self.sheet.0 .0.contents(&guard);
            match Keyframe::parse(text, contents, lock) {
                Ok(k) => k,
                Err(_) => return false,
            }
        };
        let mut guard = lock.write();
        r.write_with(&mut guard).keyframes.push(keyframe);
        true
    }

    /// The index of the last keyframe whose key text matches `select` (CSS Animations 1 §6.2
    /// `findRule()` and `deleteRule()`).
    fn find_keyframe_index(&self, select: &str) -> Option<usize> {
        let r = self.keyframes_rule()?;
        let guard = lock(&self.sheet).read();
        r.read_with(&guard).find_rule(&guard, select)
    }

    /// `findRule()`: the last keyframe whose key text matches `select`.
    pub fn find_keyframe(&self, select: &str) -> Option<Rule> {
        let index = self.find_keyframe_index(select)?;
        self.keyframe_rules()?.get(index)
    }

    /// `deleteRule()`: removes the last keyframe whose key text matches `select`; returns the
    /// removed keyframe's identity ([`Rule::id`]).
    pub fn delete_keyframe(&self, select: &str) -> Option<usize> {
        let index = self.find_keyframe_index(select)?;
        let r = self.keyframes_rule()?;
        let mut guard = lock(&self.sheet).write();
        let removed = r.write_with(&mut guard).keyframes.remove(index);
        Some(ptr(&removed))
    }

    /// CSS Animations 1 §6.3 `CSSKeyframeRule.keyText`: the keyframe's selectors, serialized.
    pub fn key_text(&self) -> Option<String> {
        let k = self.keyframe.as_ref()?;
        let guard = lock(&self.sheet).read();
        let mut out = String::new();
        let _ = k
            .read_with(&guard)
            .selector
            .to_css(&mut style_traits::CssWriter::new(&mut out));
        Some(out)
    }

    /// Sets `keyText`: `Err(Syntax)` when `text` is not a keyframe selector list. Returns whether
    /// it changed.
    pub fn set_key_text(&self, text: &str) -> Result<bool, RuleError> {
        use style::stylesheets::keyframes_rule::KeyframeSelectors;
        let Some(k) = &self.keyframe else {
            return Ok(false);
        };
        let selector = cssparser::Parser::new(text)
            .parse_entirely(KeyframeSelectors::parse)
            .map_err(|_| RuleError::Syntax)?;
        let mut guard = lock(&self.sheet).write();
        let k = k.write_with(&mut guard);
        if k.selector == selector {
            return Ok(false);
        }
        k.selector = selector;
        Ok(true)
    }

    /// The sheet the rule belongs to.
    pub fn sheet(&self) -> &Sheet {
        &self.sheet
    }
}

/// The declarations of a style rule, for `CSSStyleRule.style`.
#[derive(Clone)]
pub struct RuleDeclarations {
    block: Arc<Locked<style::properties::PropertyDeclarationBlock>>,
    sheet: Sheet,
    url: style::stylesheets::UrlExtraData,
}

impl RuleDeclarations {
    /// Runs `f` on the declarations as a [`crate::DeclarationBlock`]; writes them back when `f`
    /// returns true in its second result (they changed).
    pub fn with<R>(&self, f: impl FnOnce(&mut crate::DeclarationBlock) -> (R, bool)) -> R {
        let block = {
            let guard = lock(&self.sheet).read();
            self.block.read_with(&guard).clone()
        };
        let mut d = crate::DeclarationBlock::from_parts(block, self.url.clone());
        let (r, changed) = f(&mut d);
        if changed {
            let mut guard = lock(&self.sheet).write();
            *self.block.write_with(&mut guard) = d.into_block();
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_lists_insert_and_remove() {
        let base = url::Url::parse("http://example.test/").unwrap();
        let sheet = crate::parse_standalone(
            "p { color: red } @media screen { p { margin: 0 } }",
            &base,
            "",
        );
        let rules = sheet.rules();
        assert_eq!(rules.len(), 2);
        let first = rules.get(0).unwrap();
        assert_eq!(first.kind(), RuleKind::Style);
        assert_eq!(first.selector_text().as_deref(), Some("p"));
        let media = rules.get(1).unwrap();
        assert_eq!(media.condition_text().as_deref(), Some("screen"));
        let nested = media.child_rules().unwrap();
        assert_eq!(nested.insert("b { color: blue }", 0), Ok(0));
        assert_eq!(nested.len(), 2);
        assert_eq!(rules.insert("i { color: green }", 2), Ok(2));
        assert_eq!(
            rules.insert("@import url(x.css);", 1),
            Err(RuleError::HierarchyRequest)
        );
        assert_eq!(rules.insert("@@@", 0), Err(RuleError::Syntax));
        rules.remove(2).unwrap();
        assert_eq!(rules.remove(9), Err(RuleError::IndexSize));
        assert!(first.set_selector_text("div > p"));
        assert_eq!(first.css_text(), "div > p { color: red; }");
        sheet.replace_rules("a { color: red }");
        assert_eq!(sheet.rules().len(), 1);
    }

    #[test]
    fn font_palette_values_rules_have_their_kind_and_descriptors() {
        let base = url::Url::parse("http://example.test/").unwrap();
        let sheet = crate::parse_standalone(
            "@font-palette-values --P { font-family: foo, 'Bar Baz'; base-palette: 1; \
             base-palette: 2; override-colors: 3 #123, 0 color-mix(in lch, red, blue); \
             font-weight: 400 } @font-palette-values --Q { override-colors: 0 currentcolor }",
            &base,
            "",
        );
        let rule = sheet.rules().get(0).unwrap();
        assert_eq!(rule.kind(), RuleKind::FontPaletteValues);
        assert_eq!(rule.type_code(), 0);
        assert_eq!(
            rule.font_palette_values().unwrap(),
            FontPaletteValues {
                name: "--P".into(),
                font_family: "foo, \"Bar Baz\"".into(),
                base_palette: "2".into(),
                override_colors: "3 rgb(17, 34, 51), 0 color-mix(in lch, red, blue)".into(),
            }
        );
        assert!(!rule.css_text().contains("font-weight"));
        // An override color must be absolute.
        let empty = sheet.rules().get(1).unwrap().font_palette_values().unwrap();
        assert_eq!(
            (empty.name.as_str(), empty.override_colors.as_str()),
            ("--Q", "")
        );
    }

    #[test]
    fn font_feature_values_rules_have_their_kind_and_family() {
        let base = url::Url::parse("http://example.test/").unwrap();
        let sheet = crate::parse_standalone(
            "@font-feature-values Bongo, 'Foo Bar' { @swash { flowing: 1; } }",
            &base,
            "",
        );
        let rule = sheet.rules().get(0).unwrap();
        assert_eq!(rule.kind(), RuleKind::FontFeatureValues);
        assert_eq!(rule.type_code(), 14);
        assert_eq!(
            rule.font_feature_values_family().as_deref(),
            Some("Bongo, \"Foo Bar\"")
        );
        assert!(rule.css_text().contains("flowing: 1;"));
        assert!(rule.set_font_feature_values_family("serif").is_none());
        assert!(rule.set_font_feature_values_family("A,").is_none());
        let changed = rule.set_font_feature_values_family("Other, 'X Y'").unwrap();
        assert_ne!(changed.id(), rule.id());
        assert_eq!(
            changed.font_feature_values_family().as_deref(),
            Some("Other, \"X Y\"")
        );
        let again = sheet.rules().get(0).unwrap();
        assert_eq!(again.id(), changed.id());
        assert!(again.css_text().contains("flowing: 1;"));
        // Inside a grouping rule too.
        let sheet = crate::parse_standalone(
            "@media all { @font-feature-values A { @styleset { x: 2; } } }",
            &base,
            "",
        );
        let inner = sheet.rules().get(0).unwrap().child_rules().unwrap();
        let rule = inner.get(0).unwrap();
        let changed = rule.set_font_feature_values_family("B").unwrap();
        assert_eq!(inner.get(0).unwrap().id(), changed.id());
    }

    #[test]
    fn font_face_rules_expose_their_descriptors() {
        let base = url::Url::parse("http://example.test/").unwrap();
        let sheet = crate::parse_standalone(
            "@font-face { font-family: A; src: url(a.woff) format(woff); font-weight: 700 }",
            &base,
            "",
        );
        let rule = sheet.rules().get(0).unwrap();
        assert_eq!(rule.kind(), RuleKind::FontFace);
        assert_eq!(rule.font_face_len(), 3);
        assert_eq!(rule.font_face_item(0).as_deref(), Some("font-family"));
        assert_eq!(
            rule.font_face_descriptor("font-weight").as_deref(),
            Some("700")
        );
        assert_eq!(
            rule.font_face_descriptor("src").as_deref(),
            Some("url(\"a.woff\") format(woff)")
        );
        // `font-stretch` is the legacy name of `font-width`.
        assert!(rule.set_font_face_descriptor("font-stretch", "75%"));
        assert_eq!(
            rule.font_face_descriptor("font-width").as_deref(),
            Some("75%")
        );
        assert_eq!(
            rule.font_face_descriptor("font-stretch").as_deref(),
            Some("75%")
        );
        // An invalid value changes nothing; an empty one removes.
        assert!(!rule.set_font_face_descriptor("font-weight", "bolder"));
        assert!(!rule.set_font_face_descriptor("font-weight", "bold a"));
        assert_eq!(
            rule.font_face_descriptor("font-weight").as_deref(),
            Some("700")
        );
        assert!(rule.set_font_face_descriptor("font-weight", ""));
        assert_eq!(
            rule.font_face_descriptor("font-weight").as_deref(),
            Some("")
        );
        assert_eq!(
            rule.remove_font_face_descriptor("font-width"),
            ("75%".to_owned(), true)
        );
        // Not a descriptor: nothing, and no change.
        assert!(!rule.set_font_face_descriptor("color", "red"));
        assert_eq!(rule.font_face_descriptor("color").as_deref(), Some(""));
        assert!(rule.set_font_face_css_text("font-family: B; src: local(x)"));
        assert_eq!(rule.font_face_css_text(), "font-family: B; src: local(x);");
        // The rule's own cssText follows.
        assert!(rule.css_text().contains("font-family: B"));
    }

    #[test]
    fn font_face_descriptors_take_auto_and_serialize_a_zero_angle_as_normal() {
        let base = url::Url::parse("http://example.test/").unwrap();
        let sheet = crate::parse_standalone("@font-face { font-family: A }", &base, "");
        let rule = sheet.rules().get(0).unwrap();
        let set = |name: &str, value: &str| {
            assert!(
                rule.set_font_face_descriptor(name, value),
                "{name}: {value}"
            );
            rule.font_face_descriptor(name).unwrap()
        };
        // CSS Fonts 4 §4.4: `auto` (the initial value) for the three style descriptors.
        assert_eq!(set("font-weight", "auto"), "auto");
        assert_eq!(set("font-style", "auto"), "auto");
        assert_eq!(set("font-width", "auto"), "auto");
        assert_eq!(set("font-stretch", "75%"), "75%");
        assert_eq!(set("font-stretch", "AUTO"), "auto");
        assert!(!rule.set_font_face_descriptor("font-weight", "auto 700"));
        assert!(!rule.set_font_face_descriptor("font-style", "auto italic"));
        // `oblique 0deg` is `normal`, in any angle unit, though not as a `calc()`.
        assert_eq!(set("font-style", "oblique 0rad"), "normal");
        assert_eq!(set("font-style", "oblique 0turn 0deg"), "normal");
        assert_eq!(set("font-style", "oblique 0deg"), "normal");
        assert_eq!(
            set("font-style", "oblique calc(0deg)"),
            "oblique calc(0deg)"
        );
        assert_eq!(
            set("font-style", "oblique 0deg 10deg"),
            "oblique 0deg 10deg"
        );
        assert_eq!(set("font-style", "oblique 10deg 10deg"), "oblique 10deg");
    }
}
