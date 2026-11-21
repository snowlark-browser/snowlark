// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Selectors for `querySelector`, `querySelectorAll`, `matches` and `closest` (DOM §4.2.6 "scope-match a
//! selectors string"), with Servo's `selectors` crate (the one stylo uses) over the arena DOM.
//!
//! When snowlark-style lands, its `selectors::Element` implementation and pseudo-class set replace the
//! small ones here; the entry points stay.

use std::borrow::Borrow;
use std::fmt;

use cssparser::{CowRcStr, ParseError, ToCss};
use precomputed_hash::PrecomputedHash;
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::BloomFilter;
use selectors::context::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, QuirksMode,
    SelectorCaches,
};
use selectors::matching::{matches_selector_list, ElementSelectorFlags};
use selectors::parser::{Component, ParseRelative, SelectorParseErrorKind};
use selectors::{OpaqueElement, SelectorList};
use smallvec::SmallVec;

use snowlark_bindings::rt::{self, ExceptionName};
use snowlark_dom::{local_name, ns, Dom, LocalName, Namespace, NodeData, NodeId};
use snowlark_style::ElementState;

use super::{exception, Engine};

/// The selector implementation's string types.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CssString(pub String);

impl<'a> From<&'a str> for CssString {
    fn from(s: &'a str) -> Self {
        CssString(s.to_owned())
    }
}

impl AsRef<str> for CssString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl ToCss for CssString {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        cssparser::serialize_string(&self.0, dest)
    }
}

impl PrecomputedHash for CssString {
    fn precomputed_hash(&self) -> u32 {
        // FNV-1a.
        let mut h: u32 = 0x811c_9dc5;
        for b in self.0.bytes() {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
        h
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CssLocalName(pub LocalName);

impl<'a> From<&'a str> for CssLocalName {
    fn from(s: &'a str) -> Self {
        CssLocalName(LocalName::from(s))
    }
}

impl ToCss for CssLocalName {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        cssparser::serialize_identifier(&self.0, dest)
    }
}

impl PrecomputedHash for CssLocalName {
    fn precomputed_hash(&self) -> u32 {
        self.0.precomputed_hash()
    }
}

impl Borrow<LocalName> for CssLocalName {
    fn borrow(&self) -> &LocalName {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CssNamespace(pub Namespace);

impl PrecomputedHash for CssNamespace {
    fn precomputed_hash(&self) -> u32 {
        self.0.precomputed_hash()
    }
}

impl Borrow<Namespace> for CssNamespace {
    fn borrow(&self) -> &Namespace {
        &self.0
    }
}

/// The non-tree-structural pseudo-classes DOM-level matching knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PseudoClass {
    AnyLink,
    Link,
    Visited,
    Hover,
    Active,
    Focus,
    FocusWithin,
    FocusVisible,
    Target,
    Checked,
    Indeterminate,
    Enabled,
    Disabled,
    Required,
    Optional,
    ReadOnly,
    ReadWrite,
    PlaceholderShown,
    Default,
    Valid,
    Invalid,
    UserValid,
    UserInvalid,
    InRange,
    OutOfRange,
    Defined,
    Autofill,
    Open,
    PopoverOpen,
    Modal,
    Fullscreen,
    HasSlotted,
    State(String),
    /// `:dir()` with its argument, lowercased (any identifier parses; only `ltr` and `rtl` match).
    Dir(String),
    /// `:lang()` with its language ranges.
    Lang(Vec<String>),
}

impl ToCss for PseudoClass {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        let s = match self {
            PseudoClass::AnyLink => ":any-link",
            PseudoClass::Link => ":link",
            PseudoClass::Visited => ":visited",
            PseudoClass::Hover => ":hover",
            PseudoClass::Active => ":active",
            PseudoClass::Focus => ":focus",
            PseudoClass::FocusWithin => ":focus-within",
            PseudoClass::FocusVisible => ":focus-visible",
            PseudoClass::Target => ":target",
            PseudoClass::Checked => ":checked",
            PseudoClass::Indeterminate => ":indeterminate",
            PseudoClass::Enabled => ":enabled",
            PseudoClass::Disabled => ":disabled",
            PseudoClass::Required => ":required",
            PseudoClass::Optional => ":optional",
            PseudoClass::ReadOnly => ":read-only",
            PseudoClass::ReadWrite => ":read-write",
            PseudoClass::PlaceholderShown => ":placeholder-shown",
            PseudoClass::Default => ":default",
            PseudoClass::Valid => ":valid",
            PseudoClass::Invalid => ":invalid",
            PseudoClass::UserValid => ":user-valid",
            PseudoClass::UserInvalid => ":user-invalid",
            PseudoClass::InRange => ":in-range",
            PseudoClass::OutOfRange => ":out-of-range",
            PseudoClass::Defined => ":defined",
            PseudoClass::Autofill => ":autofill",
            PseudoClass::Open => ":open",
            PseudoClass::PopoverOpen => ":popover-open",
            PseudoClass::Modal => ":modal",
            PseudoClass::Fullscreen => ":fullscreen",
            PseudoClass::HasSlotted => ":has-slotted",
            PseudoClass::State(s) => {
                dest.write_str(":state(")?;
                cssparser::serialize_identifier(s, dest)?;
                return dest.write_str(")");
            }
            PseudoClass::Dir(d) => {
                dest.write_str(":dir(")?;
                cssparser::serialize_identifier(d, dest)?;
                return dest.write_str(")");
            }
            PseudoClass::Lang(ranges) => {
                dest.write_str(":lang(")?;
                for (i, r) in ranges.iter().enumerate() {
                    if i > 0 {
                        dest.write_str(", ")?;
                    }
                    cssparser::serialize_string(r, dest)?;
                }
                return dest.write_str(")");
            }
        };
        dest.write_str(s)
    }
}

impl selectors::parser::NonTSPseudoClass for PseudoClass {
    fn is_active_or_hover(&self) -> bool {
        matches!(self, PseudoClass::Active | PseudoClass::Hover)
    }
    fn is_user_action_state(&self) -> bool {
        matches!(
            self,
            PseudoClass::Active | PseudoClass::Hover | PseudoClass::Focus
        )
    }
}

/// Pseudo-elements: they parse (a selector may name one) but never match an element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PseudoElement(String);

impl ToCss for PseudoElement {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        dest.write_str("::")?;
        dest.write_str(&self.0)
    }
}

impl selectors::parser::PseudoElement for PseudoElement {
    /// CSS Scoping 1 §3.2.2: `::slotted()` can be followed by the tree-abiding pseudo-elements (CSS
    /// Pseudo 4 §4) and the element-backed ones.
    fn valid_after_slotted(&self) -> bool {
        matches!(
            self.0.as_str(),
            "before"
                | "after"
                | "marker"
                | "placeholder"
                | "file-selector-button"
                | "details-content"
                | "picker"
                | "picker-icon"
                | "scroll-marker"
                | "scroll-marker-group"
                | "scroll-button"
        )
    }

    /// CSS Pseudo 4 §4: `::before::marker`, `::after::marker`.
    fn valid_after_before_or_after(&self) -> bool {
        self.0 == "marker"
    }

    fn is_before_or_after(&self) -> bool {
        matches!(self.0.as_str(), "before" | "after")
    }

    fn parses_as_element_backed(&self) -> bool {
        self.0 == "details-content"
    }

    /// User action pseudo-classes may follow a `::-webkit-` pseudo-element (Selectors 4 §compat
    /// makes them valid; `::-webkit-scrollbar-thumb:hover`).
    fn accepts_state_pseudo_classes(&self) -> bool {
        self.0.starts_with("-webkit-")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Impl;

impl selectors::parser::SelectorImpl for Impl {
    type ExtraMatchingData<'a> = ();
    type AttrValue = CssString;
    type Identifier = CssString;
    type LocalName = CssLocalName;
    type NamespaceUrl = CssNamespace;
    type NamespacePrefix = CssString;
    type BorrowedNamespaceUrl = Namespace;
    type BorrowedLocalName = LocalName;
    type NonTSPseudoClass = PseudoClass;
    type PseudoElement = PseudoElement;
}

struct SelectorParser;

impl<'i> selectors::Parser<'i> for SelectorParser {
    type Impl = Impl;
    type Error = SelectorParseErrorKind;

    fn parse_slotted(&self) -> bool {
        true
    }
    fn parse_part(&self) -> bool {
        true
    }
    fn parse_nth_child_of(&self) -> bool {
        true
    }
    fn parse_is_and_where(&self) -> bool {
        true
    }
    fn parse_has(&self) -> bool {
        true
    }
    fn parse_host(&self) -> bool {
        true
    }
    fn is_is_alias(&self, name: &str) -> bool {
        name.eq_ignore_ascii_case("-webkit-any")
    }

    fn parse_non_ts_pseudo_class(
        &self,
        name: CowRcStr<'i>,
    ) -> Result<PseudoClass, ParseError<Self::Error>> {
        let pc = match_ignore_ascii_case(
            &name,
            &[
                ("any-link", PseudoClass::AnyLink),
                ("link", PseudoClass::Link),
                ("visited", PseudoClass::Visited),
                ("hover", PseudoClass::Hover),
                ("active", PseudoClass::Active),
                ("focus", PseudoClass::Focus),
                ("focus-within", PseudoClass::FocusWithin),
                ("focus-visible", PseudoClass::FocusVisible),
                ("target", PseudoClass::Target),
                ("checked", PseudoClass::Checked),
                ("indeterminate", PseudoClass::Indeterminate),
                ("enabled", PseudoClass::Enabled),
                ("disabled", PseudoClass::Disabled),
                ("required", PseudoClass::Required),
                ("optional", PseudoClass::Optional),
                ("read-only", PseudoClass::ReadOnly),
                ("read-write", PseudoClass::ReadWrite),
                ("placeholder-shown", PseudoClass::PlaceholderShown),
                ("default", PseudoClass::Default),
                ("valid", PseudoClass::Valid),
                ("invalid", PseudoClass::Invalid),
                ("user-valid", PseudoClass::UserValid),
                ("user-invalid", PseudoClass::UserInvalid),
                ("in-range", PseudoClass::InRange),
                ("out-of-range", PseudoClass::OutOfRange),
                ("defined", PseudoClass::Defined),
                ("autofill", PseudoClass::Autofill),
                ("-webkit-autofill", PseudoClass::Autofill),
                ("open", PseudoClass::Open),
                ("popover-open", PseudoClass::PopoverOpen),
                ("modal", PseudoClass::Modal),
                ("fullscreen", PseudoClass::Fullscreen),
                ("has-slotted", PseudoClass::HasSlotted),
            ],
        );
        pc.ok_or_else(unsupported)
    }

    fn parse_non_ts_functional_pseudo_class(
        &self,
        name: CowRcStr<'i>,
        parser: &mut cssparser::Parser<'i>,
        _after_part: bool,
    ) -> Result<PseudoClass, ParseError<Self::Error>> {
        if name.eq_ignore_ascii_case("state") {
            let ident = parser.expect_ident()?.as_ref().to_owned();
            return Ok(PseudoClass::State(ident));
        }
        // Selectors 4 §8.1: `:dir(<ident>)`.
        if name.eq_ignore_ascii_case("dir") {
            let ident = parser.expect_ident()?.as_ref().to_ascii_lowercase();
            return Ok(PseudoClass::Dir(ident));
        }
        // Selectors 4 §8.2: `:lang(<ident> | <string>#)`.
        if name.eq_ignore_ascii_case("lang") {
            let ranges = parser
                .parse_comma_separated(|p| Ok(p.expect_ident_or_string()?.as_ref().to_owned()))?;
            return Ok(PseudoClass::Lang(ranges));
        }
        let _ = name;
        Err(unsupported())
    }

    fn parse_pseudo_element(
        &self,
        name: CowRcStr<'i>,
    ) -> Result<PseudoElement, ParseError<Self::Error>> {
        const KNOWN: &[&str] = &[
            "before",
            "after",
            "first-line",
            "first-letter",
            "marker",
            "placeholder",
            "selection",
            "backdrop",
            "file-selector-button",
            "grammar-error",
            "spelling-error",
            "target-text",
            "cue",
            "details-content",
            "checkmark",
            "picker-icon",
            "column",
            "scroll-marker",
            "scroll-marker-group",
        ];
        let lower = name.to_ascii_lowercase();
        if KNOWN.contains(&lower.as_str()) || lower.starts_with("-webkit-") {
            return Ok(PseudoElement(lower));
        }
        Err(unsupported())
    }

    fn parse_functional_pseudo_element(
        &self,
        name: CowRcStr<'i>,
        arguments: &mut cssparser::Parser<'i>,
    ) -> Result<PseudoElement, ParseError<Self::Error>> {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "highlight" | "picker" | "scroll-button" | "view-transition-group"
        ) {
            while arguments.next().is_ok() {}
            return Ok(PseudoElement(lower));
        }
        let _ = name;
        Err(unsupported())
    }
}

/// A parse error for an unknown pseudo-class or pseudo-element.
fn unsupported() -> ParseError<SelectorParseErrorKind> {
    ParseError::custom(SelectorParseErrorKind::UnsupportedPseudoClassOrElement)
}

fn match_ignore_ascii_case(name: &str, table: &[(&str, PseudoClass)]) -> Option<PseudoClass> {
    table
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, p)| p.clone())
}

/// An element seen by the selector engine.
#[derive(Clone, Copy)]
pub struct El<'a> {
    pub dom: &'a Dom,
    pub engine: &'a Engine,
    pub id: NodeId,
}

impl fmt::Debug for El<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "El({:?})", self.id)
    }
}

impl<'a> El<'a> {
    fn wrap(&self, id: NodeId) -> El<'a> {
        El {
            dom: self.dom,
            engine: self.engine,
            id,
        }
    }

    fn html_attr(&self, name: &LocalName) -> Option<&'a str> {
        self.dom.attr(self.id, &ns!(), name)
    }

    fn is_html(&self, name: &LocalName) -> bool {
        self.dom.is_html_element_named(self.id, name)
    }
}

impl selectors::Element for El<'_> {
    type Impl = Impl;

    fn opaque(&self) -> OpaqueElement {
        OpaqueElement::new(self.dom.node(self.id))
    }

    fn parent_element(&self) -> Option<Self> {
        self.dom.parent_element(self.id).map(|p| self.wrap(p))
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        self.dom
            .parent(self.id)
            .is_some_and(|p| self.dom.is_shadow_root(p))
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        let root = self.dom.root(self.id);
        self.dom.shadow_root_data(root).map(|s| self.wrap(s.host()))
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        self.dom
            .previous_element_sibling(self.id)
            .map(|p| self.wrap(p))
    }

    fn next_sibling_element(&self) -> Option<Self> {
        self.dom.next_element_sibling(self.id).map(|p| self.wrap(p))
    }

    fn first_element_child(&self) -> Option<Self> {
        self.dom.first_element_child(self.id).map(|p| self.wrap(p))
    }

    fn is_html_element_in_html_document(&self) -> bool {
        self.dom.is_html_element_in_html_document(self.id)
    }

    fn has_local_name(&self, local_name: &LocalName) -> bool {
        self.dom.local_name(self.id) == local_name
    }

    fn has_namespace(&self, ns: &Namespace) -> bool {
        self.dom.namespace(self.id) == ns
    }

    fn is_same_type(&self, other: &Self) -> bool {
        self.dom.element_name(self.id).local == other.dom.element_name(other.id).local
            && self.dom.namespace(self.id) == other.dom.namespace(other.id)
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&CssNamespace>,
        local_name: &CssLocalName,
        operation: &AttrSelectorOperation<&CssString>,
    ) -> bool {
        self.dom.attrs(self.id).iter().any(|a| {
            if *a.local_name() != local_name.0 {
                return false;
            }
            let ns_ok = match ns {
                NamespaceConstraint::Any => true,
                NamespaceConstraint::Specific(n) => *a.namespace() == n.0,
            };
            ns_ok && operation.eval_str(a.value())
        })
    }

    fn match_non_ts_pseudo_class(
        &self,
        pc: &PseudoClass,
        _context: &mut MatchingContext<Impl>,
    ) -> bool {
        let e = self.engine;
        let id = self.id;
        match pc {
            PseudoClass::AnyLink | PseudoClass::Link => self.is_link(),
            PseudoClass::Hover => {
                e.pointer.borrow().hovered.contains(&id)
                    || e.devtools_forced(id, ElementState::HOVER)
            }
            PseudoClass::Active => {
                e.pointer.borrow().active.contains(&id)
                    || e.devtools_forced(id, ElementState::ACTIVE)
            }
            PseudoClass::Visited => e.devtools_forced(id, ElementState::VISITED),
            PseudoClass::FocusVisible => {
                e.focus_ring.get() == Some(id) || e.devtools_forced(id, ElementState::FOCUSRING)
            }
            // (HTML §4.16.3: the document's target element)
            PseudoClass::Target => {
                e.target_element(self.dom.node_document(id)) == Some(id)
                    || e.devtools_forced(id, ElementState::URLTARGET)
            }
            PseudoClass::Autofill => false,
            PseudoClass::Fullscreen => e.has_fullscreen_flag(id),
            // (a dialog whose "is modal" flag is set)
            // (HTML: a modal dialog, or an element whose fullscreen flag is set)
            PseudoClass::Modal => {
                e.dialogs.borrow().modal.contains(&id) || e.has_fullscreen_flag(id)
            }
            PseudoClass::State(name) => e.has_custom_state(id, name),
            // (HTML §3.2.6.4, with the text fields' values as the page has them)
            PseudoClass::Dir(d) => {
                let rtl = snowlark_style::i18n::directionality_is_rtl(self.dom, id, &|n| {
                    e.control_value(n)
                });
                (d == "rtl" && rtl) || (d == "ltr" && !rtl)
            }
            PseudoClass::Lang(ranges) => {
                let tag = snowlark_style::i18n::language_of(self.dom, id);
                ranges
                    .iter()
                    .any(|r| snowlark_style::i18n::lang_matches(tag, r))
            }
            PseudoClass::Focus => {
                e.has_focus(self.dom, id) || e.devtools_forced(id, ElementState::FOCUS)
            }
            PseudoClass::FocusWithin => {
                e.focused_element(self.dom.node_document(id))
                    .is_some_and(|f| self.dom.is_shadow_including_inclusive_ancestor_of(id, f))
                    || e.devtools_forced(id, ElementState::FOCUS_WITHIN)
            }
            PseudoClass::Checked => e.matches_checked(self.dom, id),
            PseudoClass::Indeterminate => e.matches_indeterminate(self.dom, id),
            PseudoClass::Enabled => {
                super::html::can_be_disabled(self.dom, id)
                    && !super::html::is_disabled(self.dom, id)
            }
            PseudoClass::Disabled => super::html::is_disabled(self.dom, id),
            PseudoClass::Required => {
                super::html::is_required_capable(self.dom, id)
                    && self.html_attr(&local_name!("required")).is_some()
            }
            PseudoClass::Optional => {
                super::html::is_required_capable(self.dom, id)
                    && self.html_attr(&local_name!("required")).is_none()
            }
            PseudoClass::ReadWrite => super::html::is_read_write(self.dom, id),
            PseudoClass::ReadOnly => !super::html::is_read_write(self.dom, id),
            PseudoClass::PlaceholderShown => {
                (self.is_html(&local_name!("input")) || self.is_html(&local_name!("textarea")))
                    && self.html_attr(&local_name!("placeholder")).is_some()
                    && e.control_value(id).is_empty()
            }
            PseudoClass::Default => {
                (self.is_html(&local_name!("option"))
                    && self.html_attr(&local_name!("selected")).is_some())
                    || (self.is_html(&local_name!("input"))
                        && self.html_attr(&local_name!("checked")).is_some())
            }
            // (HTML §4.16.3: the state the form controls' constraint validation last gave)
            PseudoClass::Valid => e.form_state(id).contains(ElementState::VALID),
            PseudoClass::Invalid => e.form_state(id).contains(ElementState::INVALID),
            PseudoClass::UserValid => e.form_state(id).contains(ElementState::USER_VALID),
            PseudoClass::UserInvalid => e.form_state(id).contains(ElementState::USER_INVALID),
            PseudoClass::InRange => e.form_state(id).contains(ElementState::INRANGE),
            PseudoClass::OutOfRange => e.form_state(id).contains(ElementState::OUTOFRANGE),
            PseudoClass::Defined => {
                let state = self.dom.custom_element_state(id);
                matches!(
                    state,
                    snowlark_dom::CustomElementState::Uncustomized
                        | snowlark_dom::CustomElementState::Custom
                )
            }
            PseudoClass::Open => {
                (self.is_html(&local_name!("details")) || self.is_html(&local_name!("dialog")))
                    && self.html_attr(&local_name!("open")).is_some()
            }
            PseudoClass::PopoverOpen => e.popover_showing(id),
            PseudoClass::HasSlotted => e.has_slotted(id),
        }
    }

    fn match_pseudo_element(
        &self,
        _pe: &PseudoElement,
        _context: &mut MatchingContext<Impl>,
    ) -> bool {
        false
    }

    fn apply_selector_flags(&self, _flags: ElementSelectorFlags) {}

    fn is_link(&self) -> bool {
        (self.is_html(&local_name!("a")) || self.is_html(&local_name!("area")))
            && self.html_attr(&local_name!("href")).is_some()
    }

    fn is_html_slot_element(&self) -> bool {
        self.is_html(&local_name!("slot"))
    }

    fn has_id(&self, id: &CssString, case_sensitivity: CaseSensitivity) -> bool {
        self.dom
            .attr_bytes(self.id, &ns!(), &local_name!("id"))
            .is_some_and(|v| case_sensitivity.eq(v, id.0.as_bytes()))
    }

    fn has_class(&self, name: &CssString, case_sensitivity: CaseSensitivity) -> bool {
        self.dom
            .attr_bytes(self.id, &ns!(), &local_name!("class"))
            .is_some_and(|v| class_tokens(v).any(|c| case_sensitivity.eq(c, name.0.as_bytes())))
    }

    fn has_custom_state(&self, _name: &CssString) -> bool {
        false
    }

    fn imported_part(&self, _name: &CssString) -> Option<CssString> {
        None
    }

    fn is_part(&self, _name: &CssString) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        !self.dom.children(self.id).any(|c| match self.dom.data(c) {
            NodeData::Element(_) => true,
            NodeData::Text(t) | NodeData::CDataSection(t) => !t.is_empty(),
            _ => false,
        })
    }

    fn is_root(&self) -> bool {
        self.dom
            .parent(self.id)
            .is_some_and(|p| self.dom.is_document(p))
    }

    fn add_element_unique_hashes(&self, _filter: &mut BloomFilter) -> bool {
        false
    }
}

/// A parsed selector list.
pub type Selectors = SelectorList<Impl>;

/// Whether matching `list` reads the form controls' constraint validation state (`:valid`,
/// `:invalid`, `:in-range`, `:out-of-range`, `:user-valid`, `:user-invalid`), anywhere in it,
/// `:has()` and the other nested lists included: only then must it be brought up to date first.
pub(crate) fn reads_form_state(list: &Selectors) -> bool {
    struct Visitor(bool);
    impl selectors::visitor::SelectorVisitor for Visitor {
        type Impl = Impl;
        fn visit_simple_selector(&mut self, c: &Component<Impl>) -> bool {
            if let Component::NonTSPseudoClass(p) = c {
                self.0 |= matches!(
                    p,
                    PseudoClass::Valid
                        | PseudoClass::Invalid
                        | PseudoClass::InRange
                        | PseudoClass::OutOfRange
                        | PseudoClass::UserValid
                        | PseudoClass::UserInvalid
                );
            }
            !self.0
        }
        fn visit_relative_selector_list(
            &mut self,
            list: &[selectors::parser::RelativeSelector<Impl>],
        ) -> bool {
            list.iter().all(|r| r.selector.visit(self))
        }
    }
    let mut v = Visitor(false);
    list.slice().iter().all(|s| s.visit(&mut v));
    v.0
}

/// The host of the shadow tree `el` is in (none for a node in a document or a detached tree).
fn shadow_host_of(dom: &Dom, el: NodeId) -> Option<NodeId> {
    let root = dom.root(el);
    dom.is_shadow_root(root).then(|| dom.host(root)).flatten()
}

/// The tokens of a class attribute's value (split on ASCII white space, HTML §2.3.4.7).
fn class_tokens(v: &[u8]) -> impl Iterator<Item = &[u8]> {
    v.split(|b| b.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
}

/// A simple selector of a selector's rightmost compound that an element must match for the whole
/// selector to: its id, a class or its type. Compared leniently (ASCII case-insensitively, either
/// spelling of a type), so it can only rule elements out; the full match decides.
pub(crate) enum Key {
    Id(Box<[u8]>),
    Class(Box<[u8]>),
    Type(LocalName, LocalName),
}

impl Key {
    /// Each selector's key, or `None` when one has none (it could match any element).
    fn of(list: &Selectors) -> Option<SmallVec<[Key; 2]>> {
        let mut keys = SmallVec::new();
        for selector in list.slice() {
            let mut key = None;
            for c in selector.iter() {
                match c {
                    Component::ID(id) => {
                        key = Some(Key::Id(id.0.as_bytes().into()));
                        break;
                    }
                    Component::Class(name) if !matches!(key, Some(Key::Class(_))) => {
                        key = Some(Key::Class(name.0.as_bytes().into()))
                    }
                    Component::LocalName(l) if key.is_none() => {
                        key = Some(Key::Type(l.name.0.clone(), l.lower_name.0.clone()))
                    }
                    _ => {}
                }
            }
            keys.push(key?);
        }
        Some(keys)
    }

    #[inline]
    fn may_match(&self, dom: &Dom, el: NodeId) -> bool {
        match self {
            Key::Id(id) => dom
                .attr_bytes(el, &ns!(), &local_name!("id"))
                .is_some_and(|v| v.eq_ignore_ascii_case(id)),
            Key::Class(c) => dom
                .attr_bytes(el, &ns!(), &local_name!("class"))
                .is_some_and(|v| class_tokens(v).any(|t| t.eq_ignore_ascii_case(c))),
            Key::Type(name, lower) => {
                let n = dom.local_name(el);
                n == name || n == lower
            }
        }
    }
}

/// A selector list parsed for the DOM's queries, with what a query derives from it.
pub(crate) struct ParsedSelectors {
    pub(crate) list: Selectors,
    /// [`reads_form_state`].
    pub(crate) reads_form_state: bool,
    /// [`query_filter`].
    filter: Option<SmallVec<[snowlark_dom::filter::Filter; 2]>>,
    /// [`Key::of`].
    keys: Option<SmallVec<[Key; 2]>>,
}

impl ParsedSelectors {
    /// Whether `el` (an element) may match: false only when no selector's key matches.
    #[inline]
    fn may_match(&self, dom: &Dom, el: NodeId) -> bool {
        self.keys
            .as_ref()
            .is_none_or(|keys| keys.iter().any(|k| k.may_match(dom, el)))
    }
}

/// The selector lists the DOM's queries parsed, by their text: pages call `matches`, `closest` and
/// `querySelector` with the same few strings over and over (event delegation, component lookups).
/// Parsing depends on the text alone (no namespace prefixes are declared for these queries), so one
/// cache serves every document of the page. Only valid lists are kept.
#[derive(Default)]
pub(crate) struct SelectorCache {
    entries: rustc_hash::FxHashMap<Box<str>, std::rc::Rc<ParsedSelectors>>,
}

impl SelectorCache {
    /// At most this many lists are kept; past it, one is dropped for each new one.
    const CAPACITY: usize = 256;
}

impl Engine {
    /// "Parse a selector" (Selectors §17): a SyntaxError DOMException for an invalid list.
    pub(crate) fn parse_selectors(&self, text: &str) -> rt::Result<Selectors> {
        let mut parser = cssparser::Parser::new(text);
        SelectorList::parse(&SelectorParser, &mut parser, ParseRelative::No)
            .map_err(|_| exception(ExceptionName::SyntaxError, "The selector is not valid"))
    }

    /// How many selector lists `matches`, `closest` and `querySelector(All)` parsed (a cost
    /// counter: the others came from the [`SelectorCache`]).
    pub fn selector_lists_parsed(&self) -> u64 {
        self.selector_lists_parsed.get()
    }

    /// [`Engine::parse_selectors`] through the page's [`SelectorCache`].
    pub(crate) fn parsed_selectors(&self, text: &str) -> rt::Result<std::rc::Rc<ParsedSelectors>> {
        if let Some(p) = self.selector_cache.borrow().entries.get(text) {
            return Ok(p.clone());
        }
        let list = self.parse_selectors(text)?;
        self.selector_lists_parsed
            .set(self.selector_lists_parsed.get() + 1);
        let parsed = std::rc::Rc::new(ParsedSelectors {
            reads_form_state: reads_form_state(&list),
            filter: query_filter(&list),
            keys: Key::of(&list),
            list,
        });
        let mut cache = self.selector_cache.borrow_mut();
        if cache.entries.len() >= SelectorCache::CAPACITY {
            if let Some(k) = cache.entries.keys().next().cloned() {
                cache.entries.remove(&k);
            }
        }
        cache.entries.insert(text.into(), parsed.clone());
        Ok(parsed)
    }

    fn quirks(&self, dom: &Dom, node: NodeId) -> QuirksMode {
        match dom.quirks_mode(dom.node_document(node)) {
            snowlark_dom::QuirksMode::Quirks => QuirksMode::Quirks,
            snowlark_dom::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
            snowlark_dom::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
        }
    }

    /// Whether `el` matches `list`, with `scope` as the `:scope` element.
    pub(crate) fn matches_list(
        &self,
        dom: &Dom,
        el: NodeId,
        list: &Selectors,
        scope: Option<NodeId>,
    ) -> bool {
        self.first_matching(dom, el, std::iter::once(el), list, None, scope)
            .is_some()
    }

    /// [`Engine::matches_list`] with a cached list.
    pub(crate) fn matches_cached(
        &self,
        dom: &Dom,
        el: NodeId,
        p: &ParsedSelectors,
        scope: Option<NodeId>,
    ) -> bool {
        p.may_match(dom, el)
            && self
                .first_matching(dom, el, std::iter::once(el), &p.list, None, scope)
                .is_some()
    }

    /// The first of `candidates` (elements in the same tree as `tree_of`) matching `list` (whose
    /// [`Key`]s are `keys`, when known), with `scope` as the `:scope` element.
    ///
    /// The context is made once for all of them: they share the document (its quirks mode) and the
    /// tree, so the shadow host `:host` matches against (CSS Scoping 1 §3.2.1: the host of the
    /// shadow tree the element is in) is found once rather than by walking to the root from each.
    pub(crate) fn first_matching(
        &self,
        dom: &Dom,
        tree_of: NodeId,
        candidates: impl Iterator<Item = NodeId>,
        list: &Selectors,
        keys: Option<&[Key]>,
        scope: Option<NodeId>,
    ) -> Option<NodeId> {
        let mut found = None;
        self.each_matching(dom, tree_of, candidates, list, keys, scope, |e| {
            found = Some(e);
            false
        });
        found
    }

    /// [`Engine::first_matching`] with a cached list.
    pub(crate) fn first_matching_cached(
        &self,
        dom: &Dom,
        tree_of: NodeId,
        candidates: impl Iterator<Item = NodeId>,
        p: &ParsedSelectors,
        scope: Option<NodeId>,
    ) -> Option<NodeId> {
        self.first_matching(dom, tree_of, candidates, &p.list, p.keys.as_deref(), scope)
    }

    /// Calls `f` with each of `candidates` (elements in the same tree as `tree_of`) matching
    /// `list`, in order, until it returns false. Elements none of `keys` (the list's [`Key`]s)
    /// matches are passed over without matching.
    #[allow(clippy::too_many_arguments)]
    fn each_matching(
        &self,
        dom: &Dom,
        tree_of: NodeId,
        candidates: impl Iterator<Item = NodeId>,
        list: &Selectors,
        keys: Option<&[Key]>,
        scope: Option<NodeId>,
        mut f: impl FnMut(NodeId) -> bool,
    ) {
        let mut candidates = candidates
            .filter(|&d| dom.is_element(d))
            .filter(|&d| keys.is_none_or(|keys| keys.iter().any(|k| k.may_match(dom, d))))
            .peekable();
        // (no context is made when nothing can match)
        if candidates.peek().is_none() {
            return;
        }
        let mut caches = SelectorCaches::default();
        let mut cx = MatchingContext::new(
            MatchingMode::Normal,
            None,
            &mut caches,
            self.quirks(dom, tree_of),
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        cx.scope_element = scope.map(|s| OpaqueElement::new(dom.node(s)));
        cx.current_host = shadow_host_of(dom, tree_of).map(|h| OpaqueElement::new(dom.node(h)));
        for d in candidates {
            let e = El {
                dom,
                engine: self,
                id: d,
            };
            if matches_selector_list(list, &e, &mut cx) && !f(d) {
                return;
            }
        }
    }

    /// The first inclusive-descendant-excluding element of `root` matching `list` (scoped to `root`).
    /// Every descendant element of `root` matching `list`, in tree order.
    pub(crate) fn query_all(&self, root: NodeId, list: &Selectors) -> Vec<NodeId> {
        let mut out = Vec::new();
        let filter = query_filter(list);
        self.each_descendant_matching(root, list, filter.as_deref(), None, |e| {
            out.push(e);
            true
        });
        out
    }

    /// The first descendant element of `root` matching the cached list `p` (scoped to `root`).
    pub(crate) fn query_first_cached(&self, root: NodeId, p: &ParsedSelectors) -> Option<NodeId> {
        let mut found = None;
        let (filter, keys) = (p.filter.as_deref(), p.keys.as_deref());
        self.each_descendant_matching(root, &p.list, filter, keys, |e| {
            found = Some(e);
            false
        });
        found
    }

    /// [`Engine::query_all`] with a cached list.
    pub(crate) fn query_all_cached(&self, root: NodeId, p: &ParsedSelectors) -> Vec<NodeId> {
        let mut out = Vec::new();
        let (filter, keys) = (p.filter.as_deref(), p.keys.as_deref());
        self.each_descendant_matching(root, &p.list, filter, keys, |e| {
            out.push(e);
            true
        });
        out
    }

    /// [`Self::each_matching`] over `root`'s descendants (scoped to `root`), skipping each subtree whose
    /// filter rules out every selector of `list` (`filter`, from [`query_filter`];
    /// [`snowlark_dom::filter`]).
    fn each_descendant_matching(
        &self,
        root: NodeId,
        list: &Selectors,
        filter: Option<&[snowlark_dom::filter::Filter]>,
        keys: Option<&[Key]>,
        f: impl FnMut(NodeId) -> bool,
    ) {
        let dom = self.dom();
        let scope = dom.is_element(root).then_some(root);
        match filter {
            None => self.each_matching(&dom, root, dom.descendants(root), list, keys, scope, f),
            Some(needs) => {
                let candidates = FilteredDescendants {
                    dom: &dom,
                    root,
                    next: dom.first_child(root),
                    needs,
                };
                self.each_matching(&dom, root, candidates, list, keys, scope, f)
            }
        }
    }
}

/// The filter bits each selector of `list` needs an element to have names for, or `None` when one
/// needs none (no subtree can be ruled out).
///
/// Only the rightmost compound counts, and in it only the simple selectors an element must itself
/// satisfy: a class (`class_bits`), an id (the `id` attribute) and an attribute selector of any kind
/// (the attribute's local name; `[a~=""]` and the like never match, so requiring the name is still
/// sound). Selectors inside `:is()`, `:not()`, `:has()` and the like add nothing. A selector with a
/// pseudo-element, `:host`, `::slotted()` or `::part()` is matched against something other than the
/// candidate's own names, so it needs none.
fn query_filter(list: &Selectors) -> Option<SmallVec<[snowlark_dom::filter::Filter; 2]>> {
    use snowlark_dom::filter::{attribute_bits, class_bits};
    let mut needs = SmallVec::new();
    for selector in list.slice() {
        if selector.has_pseudo_element() {
            return None;
        }
        let mut need = snowlark_dom::filter::Filter::EMPTY;
        for c in selector.iter() {
            match c {
                Component::Class(name) => need |= class_bits(name.as_ref()),
                Component::ID(_) => need |= attribute_bits("id"),
                Component::AttributeInNoNamespaceExists { local_name, .. }
                | Component::AttributeInNoNamespace { local_name, .. } => {
                    need |= attribute_bits(&local_name.0)
                }
                Component::AttributeOther(a) => need |= attribute_bits(&a.local_name.0),
                Component::Host(_)
                | Component::Slotted(_)
                | Component::Part(_)
                | Component::PseudoElement(_) => return None,
                _ => {}
            }
        }
        if need.is_empty() {
            return None;
        }
        needs.push(need);
    }
    Some(needs)
}

/// Descendants of `root` in tree order, without the subtrees whose filter rules out every one of
/// `needs`.
struct FilteredDescendants<'a> {
    dom: &'a Dom,
    root: NodeId,
    next: Option<NodeId>,
    needs: &'a [snowlark_dom::filter::Filter],
}

impl Iterator for FilteredDescendants<'_> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        loop {
            let id = self.next?;
            let filter = self.dom.subtree_filter(id);
            if self
                .needs
                .iter()
                .any(|&need| snowlark_dom::filter::could_match(filter, need))
            {
                self.next = self.dom.next_in_tree_order(id, Some(self.root));
                return Some(id);
            }
            self.next = self.dom.next_skipping_children(id, Some(self.root));
        }
    }
}
