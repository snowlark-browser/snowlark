// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSSOM (https://drafts.csswg.org/cssom/): `CSSStyleDeclaration` and `CSSStyleProperties` for an
//! element's inline style (`element.style`, backed by its `style` attribute through stylo's
//! declaration blocks) and for `getComputedStyle()` (live resolved values from the renderer); the
//! `CSS` namespace; CSSOM View's `matchMedia()` and `MediaQueryList`; and CSS Font Loading's
//! `document.fonts` (`FontFaceSet`, whose `ready` promise resolves once the document's web fonts
//! have arrived) with `FontFace`.

use snowlark_bindings::generated::{
    self as g, CSSFontFaceDescriptorsImpl, CSSImpl, CSSStyleDeclarationImpl,
    CSSStylePropertiesImpl, ElementCSSInlineStyleImpl, EventListener, FontFaceLoadStatus,
    FontFaceSetLoadEventImpl, FontFaceSourceImpl, MediaQueryListEventImpl, MediaQueryListImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsPromise, JsValue};
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;
use snowlark_style::DeclarationBlock;

use super::events::EventExt;
use super::{exception, handle_node, Engine, Object};

/// Which pseudo-element a computed style declaration is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pseudo {
    None,
    Before,
    After,
    Marker,
    FirstLetter,
    FirstLine,
    /// The highlight pseudo-elements (CSS Pseudo-Elements 4 §3.1).
    Selection,
    TargetText,
    SpellingError,
    GrammarError,
    /// Not a pseudo-element this engine knows: every value is empty.
    Unknown,
}

/// A `CSSStyleDeclaration`.
#[derive(Clone, Copy, Debug)]
pub enum Decl {
    /// `element.style`: the element's inline style declarations.
    Inline(NodeId),
    /// `getComputedStyle(element, pseudo)`: read-only resolved values.
    Computed(NodeId, Pseudo),
    /// `CSSStyleRule.style`: the declarations of the rule object given.
    Rule(Handle),
    /// `CSSFontFaceRule.style` (CSS Fonts 5 `CSSFontFaceDescriptors`): the descriptors of the
    /// `@font-face` rule object given.
    FontFace(Handle),
}

/// A `MediaQueryList`. It lives while script reaches it or it has a `change` listener
/// ([`Engine::media_list_pins`]): nothing else can observe it.
pub struct Mql {
    pub query: String,
    /// The matches state as of the last "evaluate media queries and report changes" (or of the
    /// list's creation).
    pub matches: bool,
    /// The Window whose document the list evaluates against (CSSOM View §4.2 "associated
    /// document").
    pub window: Handle,
    /// What `matches` was evaluated against. While it is the same the list matches as it did, and
    /// a rendering update does not evaluate it again.
    pub seen: Option<MediaEnvironment>,
    /// The list's place in [`CssomState::media_lists`] (the order of creation).
    pub order: u64,
}

/// What a media query list is evaluated against: the document and its renderer's media generation
/// ([`snowlark_render::Renderer::media_generation`]).
pub type MediaEnvironment = (NodeId, u64);

/// A `FontFace` (see `fontload.rs`).
pub struct FontFaceData {
    /// The document whose renderer holds the face.
    pub doc: Option<NodeId>,
    pub family: String,
    pub style: String,
    pub weight: String,
    pub stretch: String,
    pub unicode_range: String,
    pub feature_settings: String,
    pub variation_settings: String,
    pub display: String,
    pub ascent_override: String,
    pub descent_override: String,
    pub line_gap_override: String,
    /// The `src` descriptor as serialized (empty for a face made from a buffer).
    pub src: String,
    /// The bytes of a face made from a buffer.
    pub bytes: Option<Vec<u8>>,
    pub status: FontFaceLoadStatus,
    /// The renderer's id of the face.
    pub rid: Option<usize>,
    /// Declared by an `@font-face` rule of the document.
    pub css: bool,
    /// The pending `loaded` promise.
    pub promise: Option<JsPromise>,
    /// Why the face is in error from the start (an invalid `src`).
    pub error: Option<(ExceptionName, &'static str)>,
}

/// CSSOM state that is not a platform object.
#[derive(Default)]
pub struct CssomState {
    /// The inline style declarations of elements, parsed from the `style` attribute value they were
    /// parsed from (reparsed when it changed).
    pub inline: rustc_hash::FxHashMap<NodeId, (String, DeclarationBlock)>,
    /// Live `MediaQueryList`s (their `change` events fire at rendering updates), by order of
    /// creation ([`Mql::order`]). A list leaves when it is freed.
    pub media_lists: std::collections::BTreeMap<u64, Handle>,
    /// The [`Mql::order`] of the next list.
    pub next_media_list: u64,
    /// How many times a rendering update evaluated a list ([`Engine::media_list_counts`]).
    pub media_evaluations: u64,
    /// `document.fonts` per document, and the pending `ready` promise.
    pub font_sets: rustc_hash::FxHashMap<NodeId, Handle>,
    /// `FontFace`s with a `loaded` promise pending.
    pub face_promises: Vec<Handle>,
    /// `document.fonts.load()` calls waiting for their faces.
    pub load_waiters: Vec<super::fontload::LoadWaiter>,
    pub fonts_ready: rustc_hash::FxHashMap<NodeId, JsPromise>,
    /// The platform objects of sheets, rule lists and rules, by the identity of what they wrap.
    pub sheets: rustc_hash::FxHashMap<usize, Handle>,
    pub rule_lists: rustc_hash::FxHashMap<usize, Handle>,
    pub rules: rustc_hash::FxHashMap<usize, Handle>,
    pub sheet_lists: rustc_hash::FxHashMap<NodeId, Handle>,
    /// CSSOM §6.4 `adoptedStyleSheets`: each document's or shadow root's backing list.
    pub adopted: rustc_hash::FxHashMap<NodeId, Vec<Handle>>,
    /// The style sheets of `<style>` elements in documents no renderer draws, with the text, `type`
    /// and `media` each was made from (`Engine::unrendered_style_sheet`).
    pub unrendered: rustc_hash::FxHashMap<NodeId, UnrenderedSheet>,
    /// CSS Typed OM `computedStyleMap()`: each element's map ([SameObject]), while it lives.
    pub computed_maps: rustc_hash::FxHashMap<NodeId, Handle>,
}

/// A `<style>` element's style sheet in a document without a renderer, and what it was made from.
pub struct UnrenderedSheet {
    pub text: String,
    pub ty: Option<String>,
    pub media: Option<String>,
    pub sheet: snowlark_style::Sheet,
}

fn parse_pseudo(p: Option<&str>) -> Pseudo {
    let Some(p) = p.filter(|p| !p.is_empty()) else {
        return Pseudo::None;
    };
    if !p.starts_with(':') {
        return Pseudo::None;
    }
    match p.to_ascii_lowercase().as_str() {
        "::before" | ":before" => Pseudo::Before,
        "::after" | ":after" => Pseudo::After,
        "::marker" => Pseudo::Marker,
        "::first-letter" | ":first-letter" => Pseudo::FirstLetter,
        "::first-line" | ":first-line" => Pseudo::FirstLine,
        "::selection" => Pseudo::Selection,
        "::target-text" => Pseudo::TargetText,
        "::spelling-error" => Pseudo::SpellingError,
        "::grammar-error" => Pseudo::GrammarError,
        _ => Pseudo::Unknown,
    }
}

/// CSSOM §2.1 "serialize an identifier".
pub(crate) fn serialize_identifier(ident: &str) -> String {
    let chars: Vec<char> = ident.chars().collect();
    let mut out = String::with_capacity(ident.len());
    for (i, &c) in chars.iter().enumerate() {
        let code = c as u32;
        if c == '\0' {
            out.push('\u{FFFD}');
        } else if (0x1..=0x1F).contains(&code)
            || code == 0x7F
            || (i == 0 && c.is_ascii_digit())
            || (i == 1 && c.is_ascii_digit() && chars[0] == '-')
        {
            out.push_str(&format!("\\{:x} ", code));
        } else if i == 0 && c == '-' && chars.len() == 1 {
            out.push_str("\\-");
        } else if code >= 0x80 || c == '-' || c == '_' || c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out
}

impl Engine {
    fn decl(&self, h: Handle) -> Decl {
        match &*self.obj(h) {
            Object::StyleDeclaration(d) => *d,
            _ => panic!("not a CSSStyleDeclaration"),
        }
    }

    /// Runs `f` on an element's inline declarations as currently in its `style` attribute.
    pub(crate) fn with_inline<R>(
        &self,
        el: NodeId,
        f: impl FnOnce(&mut DeclarationBlock) -> R,
    ) -> R {
        let attr = self
            .dom()
            .get_attribute(el, "style")
            .unwrap_or("")
            .to_owned();
        let mut cssom = self.cssom.borrow_mut();
        let entry = cssom.inline.get_mut(&el);
        match entry {
            Some((text, block)) if *text == attr => f(block),
            _ => {
                drop(cssom);
                let base = self.base_url_of(el);
                let mut block = DeclarationBlock::parse(&attr, &base);
                let r = f(&mut block);
                self.cssom.borrow_mut().inline.insert(el, (attr, block));
                r
            }
        }
    }

    /// Sets declarations in `el`'s inline style and updates its `style` attribute once (Web Animations
    /// `commitStyles()`).
    pub(crate) fn set_inline_properties(
        &self,
        cx: &mut Cx,
        el: NodeId,
        values: &[(String, String)],
    ) {
        let changed = self.with_inline(el, |b| {
            let mut changed = false;
            for (p, v) in values {
                changed |= b.set_property(p, v, "");
            }
            changed
        });
        if changed {
            self.write_inline(cx, el);
        }
    }

    /// CSSOM "update style attribute for" the inline declarations of `el` after a change.
    pub(super) fn write_inline(&self, cx: &mut Cx, el: NodeId) {
        let css = self
            .cssom
            .borrow()
            .inline
            .get(&el)
            .map(|(_, b)| b.css_text())
            .unwrap_or_default();
        if self.policy.borrow().any {
            self.cssom_style_written(el, &css);
        }
        {
            let cssom = self.cssom.borrow();
            if let Some((_, block)) = cssom.inline.get(&el) {
                self.with_node_renderer(el, |r, _| r.provide_style_attribute(el, &css, block));
            }
        }
        self.set_attr(cx, el, "style", &css);
        if let Some(entry) = self.cssom.borrow_mut().inline.get_mut(&el) {
            entry.0 = css;
        }
    }

    /// A computed value (the resolved value of CSSOM §6.7.2) of `property` for `el`.
    pub(crate) fn computed_value(&self, el: NodeId, pseudo: Pseudo, property: &str) -> String {
        let pseudo_el = match pseudo {
            Pseudo::None => None,
            Pseudo::Before => Some(snowlark_style::PseudoElement::Before),
            Pseudo::After => Some(snowlark_style::PseudoElement::After),
            Pseudo::Marker => Some(snowlark_style::PseudoElement::Marker),
            Pseudo::FirstLetter => Some(snowlark_style::PseudoElement::FirstLetter),
            Pseudo::FirstLine => Some(snowlark_style::PseudoElement::FirstLine),
            Pseudo::Selection => Some(snowlark_style::PseudoElement::Selection),
            Pseudo::TargetText => Some(snowlark_style::PseudoElement::TargetText),
            Pseudo::SpellingError => Some(snowlark_style::PseudoElement::SpellingError),
            Pseudo::GrammarError => Some(snowlark_style::PseudoElement::GrammarError),
            Pseudo::Unknown => return String::new(),
        };
        match pseudo_el {
            // (layout only when the resolved value can depend on it)
            None if snowlark_style::computed::property_id(property)
                .is_some_and(|id| snowlark_style::computed::needs_layout(&id)) =>
            {
                self.with_layout(el, |r, _| r.resolved_property(el, property))
                    .flatten()
            }
            // (an element in a size container: its style needs the container's layout)
            None => match self
                .with_style(el, |r| {
                    (!r.style_depends_on_layout(el))
                        .then(|| r.resolved_style_property(el, property))
                })
                .flatten()
            {
                Some(v) => v,
                None => self
                    .with_layout(el, |r, _| r.resolved_property(el, property))
                    .flatten(),
            },
            // (a generated `::before` or `::after` resolves some properties to used values)
            Some(p)
                if matches!(
                    p,
                    snowlark_style::PseudoElement::Before | snowlark_style::PseudoElement::After
                ) && snowlark_style::computed::property_id(property)
                    .is_some_and(|id| snowlark_style::computed::needs_layout(&id)) =>
            {
                self.with_layout(el, |r, _| r.resolved_pseudo_property(el, &p, property))
                    .flatten()
            }
            Some(p) => self
                .with_style(el, |r| r.resolved_pseudo_property(el, &p, property))
                .flatten(),
        }
        .unwrap_or_default()
    }

    fn decl_value(&self, h: Handle, property: &str) -> String {
        match self.decl(h) {
            Decl::Inline(el) => self.with_inline(el, |b| b.property_value(property)),
            Decl::Computed(el, pseudo) => self.computed_value(el, pseudo, property),
            Decl::Rule(r) => self.with_rule_decls(r, |b| (b.property_value(property), false)),
            Decl::FontFace(r) => self
                .rule_of(r)
                .font_face_descriptor(property)
                .unwrap_or_default(),
        }
    }

    /// A `@font-face` rule's descriptors changed (when `changed`): its faces are read again.
    fn font_face_changed(&self, changed: bool) {
        if changed {
            self.for_each_renderer(|r, st| {
                r.stylesheets_changed();
                st.dirty = true;
            });
        }
    }

    /// Runs `f` on a style rule's declarations; `f` says whether it changed them.
    pub(super) fn with_rule_decls<R: Default>(
        &self,
        rule: Handle,
        f: impl FnOnce(&mut DeclarationBlock) -> (R, bool),
    ) -> R {
        let Some(decls) = self.rule_of(rule).declarations() else {
            return R::default();
        };
        let mut changed = false;
        let r = decls.with(|b| {
            let (r, c) = f(b);
            changed = c;
            (r, c)
        });
        if changed {
            self.for_each_renderer(|r, st| {
                r.stylesheets_changed();
                st.dirty = true;
            });
        }
        r
    }

    fn decl_set(
        &self,
        cx: &mut Cx,
        h: Handle,
        property: &str,
        value: &str,
        priority: &str,
    ) -> rt::Result<()> {
        match self.decl(h) {
            Decl::Inline(el) => {
                if self.with_inline(el, |b| b.set_property(property, value, priority)) {
                    self.write_inline(cx, el);
                }
                Ok(())
            }
            Decl::Computed(..) => Err(read_only()),
            Decl::Rule(r) => {
                self.with_rule_decls(r, |b| ((), b.set_property(property, value, priority)));
                Ok(())
            }
            // (a descriptor has no priority)
            Decl::FontFace(r) => {
                self.font_face_changed(self.rule_of(r).set_font_face_descriptor(property, value));
                Ok(())
            }
        }
    }

    /// The `media` and `matches` of a list now (re-evaluated against the rendered document).
    /// Whether `query` matches the rendered document of `window` now: a frame's viewport is
    /// brought up to date with its container's layout first (a resized `<iframe>` changes its
    /// document's `width` at once).
    fn mql_matches(&self, window: Handle, query: &str) -> bool {
        let Some(doc) = self.document_of_window(window) else {
            return false;
        };
        self.sync_frame_viewport(doc);
        self.with_doc_renderer(doc, |r, _| r.evaluate_media(query))
            .unwrap_or(false)
    }

    /// What the media query lists of `window` are evaluated against now ([`Mql::seen`]): its
    /// document, with its viewport up to date as in [`Engine::mql_matches`], and the media
    /// generation of the document's renderer. `None` for a window with no fully active document or
    /// no renderer.
    fn media_environment(&self, window: Handle) -> Option<MediaEnvironment> {
        let doc = self.document_of_window(window)?;
        self.sync_frame_viewport(doc);
        let generation = self.with_doc_renderer(doc, |r, _| r.media_generation())?;
        Some((doc, generation))
    }

    /// HTML §8.1.7.3 "evaluate media queries and report changes": a `change` event at each
    /// `MediaQueryList` whose `matches` changed. A list is evaluated only when what it is
    /// evaluated against changed since its last evaluation ([`Mql::seen`]): a page may hold many
    /// lists, and most rendering updates change nothing a media query reads.
    pub(crate) fn report_media_query_changes(&self, cx: &mut Cx) {
        if self.cssom.borrow().media_lists.is_empty() {
            return;
        }
        let lists: Vec<Handle> = self.cssom.borrow().media_lists.values().copied().collect();
        // (the environment of each window that has lists: few windows, many lists)
        let mut environments: smallvec::SmallVec<[(Handle, Option<MediaEnvironment>); 2]> =
            smallvec::SmallVec::new();
        for h in lists {
            let (window, seen) = match &*self.obj(h) {
                Object::MediaQueryList(m) => (m.window, m.seen),
                _ => continue,
            };
            let environment = match environments.iter().find(|e| e.0 == window) {
                Some(e) => e.1,
                None => {
                    let e = self.media_environment(window);
                    environments.push((window, e));
                    e
                }
            };
            // (CSSOM View §4.2: only lists whose document is fully active)
            if environment.is_none() || environment == seen {
                continue;
            }
            let query = match &*self.obj(h) {
                Object::MediaQueryList(m) => m.query.clone(),
                _ => continue,
            };
            let now = self.mql_matches(window, &query);
            self.cssom.borrow_mut().media_evaluations += 1;
            let changed = match &mut *self.obj_mut(h) {
                Object::MediaQueryList(m) => {
                    m.seen = environment;
                    std::mem::replace(&mut m.matches, now) != now
                }
                _ => false,
            };
            if changed {
                let media = self.serialized_media(&query);
                let e = self.trusted_event(
                    InterfaceId::MediaQueryListEvent,
                    "change",
                    false,
                    false,
                    EventExt::MediaQuery {
                        media,
                        matches: now,
                    },
                );
                self.dispatch(cx, e, h, false);
            }
        }
    }

    fn serialized_media(&self, query: &str) -> String {
        self.with_renderer(|r, _| r.serialize_media(query))
            .unwrap_or_else(|| query.to_owned())
    }

    /// CSS Font Loading §2.4: `document.fonts.ready` resolves once the document has loaded and no
    /// web font is loading.
    pub(crate) fn update_fonts_ready(&self, cx: &mut Cx) {
        let pending: Vec<(NodeId, JsPromise)> = self
            .cssom
            .borrow()
            .fonts_ready
            .iter()
            .map(|(d, p)| (*d, p.clone()))
            .collect();
        for (doc, promise) in pending {
            if self.fonts_loading(doc) {
                continue;
            }
            let set = self.cssom.borrow().font_sets.get(&doc).copied();
            self.cssom.borrow_mut().fonts_ready.remove(&doc);
            if let Some(set) = set {
                promise.resolve(cx, set);
            }
        }
    }

    /// Whether `doc`'s fonts are loading: it is still loading, or the renderer fetches web fonts.
    pub(crate) fn fonts_loading(&self, doc: NodeId) -> bool {
        if !self.dom().is_alive(doc) {
            return false;
        }
        if self.doc_state(doc, |s| s.ready_state) == g::DocumentReadyState::Loading {
            return true;
        }
        self.is_rendered_document(doc)
            && self
                .with_doc_renderer(doc, |r, _| r.fonts_loading() > 0)
                .unwrap_or(false)
    }
}

fn read_only() -> rt::Error {
    exception(
        ExceptionName::NoModificationAllowedError,
        "The computed style declaration is read-only",
    )
}

impl CSSStyleDeclarationImpl for Engine {
    fn parent_rule(&self, this: Handle) -> Option<Handle> {
        match self.decl(this) {
            Decl::Rule(r) | Decl::FontFace(r) => Some(r),
            _ => None,
        }
    }
    fn css_text(&self, this: Handle) -> DomStr<'_> {
        match self.decl(this) {
            Decl::Inline(el) => DomStr::Owned(self.with_inline(el, |b| b.css_text())),
            // CSSOM §6.6: the computed flag's cssText is the empty string.
            Decl::Computed(..) => DomStr::Borrowed(""),
            Decl::Rule(r) => DomStr::Owned(self.with_rule_decls(r, |b| (b.css_text(), false))),
            Decl::FontFace(r) => DomStr::Owned(self.rule_of(r).font_face_css_text()),
        }
    }
    fn set_css_text(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        match self.decl(this) {
            Decl::Inline(el) => {
                self.with_inline(el, |b| b.set_css_text(value));
                self.write_inline(cx, el);
                Ok(())
            }
            Decl::Computed(..) => Err(read_only()),
            Decl::Rule(r) => {
                self.with_rule_decls(r, |b| {
                    b.set_css_text(value);
                    ((), true)
                });
                Ok(())
            }
            Decl::FontFace(r) => {
                self.font_face_changed(self.rule_of(r).set_font_face_css_text(value));
                Ok(())
            }
        }
    }
    fn length(&self, this: Handle) -> u32 {
        match self.decl(this) {
            Decl::Inline(el) => self.with_inline(el, |b| b.len() as u32),
            Decl::Computed(el, _) => {
                if self.dom().is_alive(el) {
                    snowlark_style::computed::computed_property_names().len() as u32
                } else {
                    0
                }
            }
            Decl::Rule(r) => self.with_rule_decls(r, |b| (b.len() as u32, false)),
            Decl::FontFace(r) => self.rule_of(r).font_face_len() as u32,
        }
    }
    fn item(&self, this: Handle, index: u32) -> Option<DomStr<'_>> {
        match self.decl(this) {
            Decl::Inline(el) => self
                .with_inline(el, |b| b.item(index as usize))
                .map(DomStr::Owned),
            Decl::Computed(..) => snowlark_style::computed::computed_property_names()
                .get(index as usize)
                .map(|n| DomStr::Borrowed(n)),
            Decl::Rule(r) => self
                .with_rule_decls(r, |b| (b.item(index as usize), false))
                .map(DomStr::Owned),
            Decl::FontFace(r) => self
                .rule_of(r)
                .font_face_item(index as usize)
                .map(DomStr::Owned),
        }
    }
    fn get_property_value(
        &self,
        _cx: &mut Cx,
        this: Handle,
        property: &str,
    ) -> rt::Result<DomStr<'_>> {
        Ok(DomStr::Owned(self.decl_value(this, property)))
    }
    fn get_property_priority(
        &self,
        _cx: &mut Cx,
        this: Handle,
        property: &str,
    ) -> rt::Result<DomStr<'_>> {
        Ok(match self.decl(this) {
            Decl::Inline(el) => {
                DomStr::Borrowed(self.with_inline(el, |b| b.property_priority(property)))
            }
            Decl::Computed(..) => DomStr::Borrowed(""),
            Decl::Rule(r) => DomStr::Borrowed(
                self.with_rule_decls(r, |b| (b.property_priority(property), false)),
            ),
            Decl::FontFace(_) => DomStr::Borrowed(""),
        })
    }
    fn set_property(
        &self,
        cx: &mut Cx,
        this: Handle,
        property: &str,
        value: &str,
        priority: &str,
    ) -> rt::Result<()> {
        self.decl_set(cx, this, property, value, priority)
    }
    fn remove_property(&self, cx: &mut Cx, this: Handle, property: &str) -> rt::Result<DomStr<'_>> {
        match self.decl(this) {
            Decl::Inline(el) => {
                let (old, changed) = self.with_inline(el, |b| b.remove_property(property));
                if changed {
                    self.write_inline(cx, el);
                }
                Ok(DomStr::Owned(old))
            }
            Decl::Computed(..) => Err(read_only()),
            Decl::Rule(r) => Ok(DomStr::Owned(
                self.with_rule_decls(r, |b| (b.remove_property(property).0, false))
                    .to_owned(),
            )),
            Decl::FontFace(r) => {
                let (old, changed) = self.rule_of(r).remove_font_face_descriptor(property);
                self.font_face_changed(changed);
                Ok(DomStr::Owned(old))
            }
        }
    }
}

impl CSSFontFaceDescriptorsImpl for Engine {
    fn css_property(&self, this: Handle, property: &'static str) -> DomStr<'_> {
        DomStr::Owned(self.decl_value(this, property))
    }
    fn set_css_property(
        &self,
        cx: &mut Cx,
        this: Handle,
        property: &'static str,
        value: &str,
    ) -> rt::Result<()> {
        self.decl_set(cx, this, property, value, "")
    }
}

impl CSSStylePropertiesImpl for Engine {
    fn css_float(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.decl_value(this, "float"))
    }
    fn set_css_float(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.decl_set(cx, this, "float", value, "")
    }
    fn css_property(&self, this: Handle, property: &'static str) -> DomStr<'_> {
        DomStr::Owned(self.decl_value(this, property))
    }
    fn set_css_property(
        &self,
        cx: &mut Cx,
        this: Handle,
        property: &'static str,
        value: &str,
    ) -> rt::Result<()> {
        self.decl_set(cx, this, property, value, "")
    }
}

impl ElementCSSInlineStyleImpl for Engine {
    fn style(&self, this: Handle) -> Handle {
        // ([SameObject]: the bindings keep the first object.)
        self.alloc(Object::StyleDeclaration(Decl::Inline(self.n(this))))
    }
    fn attribute_style_map(&self, this: Handle) -> Handle {
        // ([SameObject]: the bindings keep the first object.)
        self.attribute_style_map(self.n(this))
    }
}

impl Engine {
    /// `getComputedStyle(elt, pseudoElt)` (CSSOM §6.7.1).
    pub(crate) fn get_computed_style(&self, elt: Handle, pseudo: Option<&str>) -> Handle {
        let el = self.n(elt);
        self.alloc(Object::StyleDeclaration(Decl::Computed(
            el,
            parse_pseudo(pseudo),
        )))
    }

    /// `matchMedia(query)` (CSSOM View §4.2).
    pub(crate) fn match_media(&self, window: Handle, query: &str) -> Handle {
        let matches = self.mql_matches(window, query);
        let seen = self.media_environment(window);
        let order = {
            let mut cssom = self.cssom.borrow_mut();
            cssom.next_media_list += 1;
            cssom.next_media_list
        };
        let h = self.alloc(Object::MediaQueryList(Mql {
            query: query.to_owned(),
            matches,
            window,
            seen,
            order,
        }));
        self.cssom.borrow_mut().media_lists.insert(order, h);
        h
    }

    /// The media query lists the collector must keep: those with a `change` listener (an event
    /// handler's included), which a change of their media would call. One with none lives only
    /// while script reaches it. `None` when some state is borrowed (the pass waits).
    pub(crate) fn media_list_pins(&self) -> Option<Vec<Handle>> {
        let cssom = self.cssom.try_borrow().ok()?;
        if cssom.media_lists.is_empty() {
            return Some(Vec::new());
        }
        let listeners = self.listeners.try_borrow().ok()?;
        Some(
            cssom
                .media_lists
                .values()
                .copied()
                .filter(|h| {
                    listeners
                        .get(h)
                        .is_some_and(|l| !l.of_type("change").is_empty())
                })
                .collect(),
        )
    }

    /// The media query lists alive, and how many times a rendering update evaluated one so far
    /// (tests: lists nothing reaches are freed, and a list is evaluated only when its media
    /// changed).
    pub fn media_list_counts(&self) -> (usize, u64) {
        let cssom = self.cssom.borrow();
        (cssom.media_lists.len(), cssom.media_evaluations)
    }

    /// A freed list leaves the lists the rendering updates evaluate.
    pub(crate) fn media_list_freed(&self, list: &Mql) {
        self.cssom.borrow_mut().media_lists.remove(&list.order);
    }
}

impl MediaQueryListImpl for Engine {
    fn media(&self, this: Handle) -> DomStr<'_> {
        let query = match &*self.obj(this) {
            Object::MediaQueryList(m) => m.query.clone(),
            _ => String::new(),
        };
        DomStr::Owned(self.serialized_media(&query))
    }
    fn matches(&self, this: Handle) -> bool {
        let (query, window) = match &*self.obj(this) {
            Object::MediaQueryList(m) => (m.query.clone(), m.window),
            _ => return false,
        };
        self.mql_matches(window, &query)
    }
    /// `addListener(callback)`: `addEventListener("change", callback)`.
    fn add_listener(
        &self,
        cx: &mut Cx,
        this: Handle,
        callback: Option<EventListener>,
    ) -> rt::Result<()> {
        g::EventTargetImpl::add_event_listener(
            self,
            cx,
            this,
            "change",
            callback,
            g::AddEventListenerOptionsOrBoolean::Boolean(false),
        )
    }
    fn remove_listener(
        &self,
        cx: &mut Cx,
        this: Handle,
        callback: Option<EventListener>,
    ) -> rt::Result<()> {
        g::EventTargetImpl::remove_event_listener(
            self,
            cx,
            this,
            "change",
            callback,
            g::EventListenerOptionsOrBoolean::Boolean(false),
        )
    }
}

impl MediaQueryListEventImpl for Engine {
    fn media(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::MediaQuery { media, .. } => media.clone(),
            _ => String::new(),
        }))
    }
    fn matches(&self, this: Handle) -> bool {
        self.ev(this, |e| {
            matches!(e.ext, EventExt::MediaQuery { matches: true, .. })
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::MediaQueryListEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::MediaQueryListEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::MediaQuery {
                media: init.media,
                matches: init.matches,
            },
        ))
    }
}

/// CSS Typed OM §4.3.5: `CSS.px(value)` and the other factories, each a `CSSUnitValue` of the unit
/// it is named for.
macro_rules! unit_factories {
    ($($name:ident => $unit:literal),* $(,)?) => {
        $(fn $name(&self, _cx: &mut Cx, value: f64) -> rt::Result<Handle> {
            self.css_factory(value, $unit)
        })*
    };
}

impl CSSImpl for Engine {
    unit_factories! {
        number => "number", percent => "percent",
        cap => "cap", ch => "ch", em => "em", ex => "ex", ic => "ic", lh => "lh",
        rcap => "rcap", rch => "rch", rem => "rem", rex => "rex", ric => "ric", rlh => "rlh",
        vw => "vw", vh => "vh", vi => "vi", vb => "vb", vmin => "vmin", vmax => "vmax",
        svw => "svw", svh => "svh", svi => "svi", svb => "svb", svmin => "svmin", svmax => "svmax",
        lvw => "lvw", lvh => "lvh", lvi => "lvi", lvb => "lvb", lvmin => "lvmin", lvmax => "lvmax",
        dvw => "dvw", dvh => "dvh", dvi => "dvi", dvb => "dvb", dvmin => "dvmin", dvmax => "dvmax",
        cqw => "cqw", cqh => "cqh", cqi => "cqi", cqb => "cqb", cqmin => "cqmin", cqmax => "cqmax",
        cm => "cm", mm => "mm", q => "q", in_ => "in", pt => "pt", pc => "pc", px => "px",
        deg => "deg", grad => "grad", rad => "rad", turn => "turn",
        s => "s", ms => "ms", hz => "hz", k_hz => "khz",
        dpi => "dpi", dpcm => "dpcm", dppx => "dppx", fr => "fr",
    }
    fn element_sources(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsValue> {
        Ok(rt::undefined_value(cx.scope()))
    }
    fn escape(&self, _cx: &mut Cx, ident: &str) -> rt::Result<DomStr<'_>> {
        Ok(DomStr::Owned(serialize_identifier(ident)))
    }
    fn supports_property_value(
        &self,
        _cx: &mut Cx,
        property: &str,
        value: &str,
    ) -> rt::Result<bool> {
        Ok(snowlark_style::supports_declaration(property, value))
    }
    fn supports_condition_text(&self, _cx: &mut Cx, condition_text: &str) -> rt::Result<bool> {
        Ok(snowlark_style::supports_condition(condition_text))
    }
    fn parse_value(&self, _cx: &mut Cx, _css: &str) -> rt::Result<g::StringOrCSSStyleValue> {
        Err(exception(
            ExceptionName::NotSupportedError,
            "CSS.parseValue is not supported",
        ))
    }
    fn parse_value_list(
        &self,
        _cx: &mut Cx,
        _css: &str,
    ) -> rt::Result<Vec<g::StringOrCSSStyleValue>> {
        Err(exception(
            ExceptionName::NotSupportedError,
            "CSS.parseValueList is not supported",
        ))
    }
    fn parse_comma_value_list(
        &self,
        _cx: &mut Cx,
        _css: &str,
    ) -> rt::Result<Vec<Vec<g::StringOrCSSStyleValue>>> {
        Err(exception(
            ExceptionName::NotSupportedError,
            "CSS.parseCommaValueList is not supported",
        ))
    }
    /// CSS Properties and Values API 1 §2.4: registers the property in the document's set of
    /// registered properties.
    fn register_property(&self, _cx: &mut Cx, definition: g::PropertyDefinition) -> rt::Result<()> {
        let result = self.with_renderer(|r, _| {
            r.register_property(
                &definition.name,
                &definition.syntax,
                definition.inherits,
                definition.initial_value.as_deref(),
            )
        });
        match result {
            Some(Err(snowlark_style::RegisterPropertyError::AlreadyRegistered)) => Err(exception(
                ExceptionName::InvalidModificationError,
                "the property is registered already",
            )),
            Some(Err(snowlark_style::RegisterPropertyError::Invalid)) => Err(exception(
                ExceptionName::SyntaxError,
                "invalid property name, syntax or initial value",
            )),
            Some(Ok(())) | None => Ok(()),
        }
    }
}

impl FontFaceSourceImpl for Engine {
    fn fonts(&self, this: Handle) -> Handle {
        let doc = handle_node(this).unwrap_or_else(|| self.document().expect("a document"));
        if let Some(h) = self.cssom.borrow().font_sets.get(&doc) {
            return *h;
        }
        let h = self.alloc(Object::FontFaceSet {
            document: doc,
            script: Vec::new(),
            css: Vec::new(),
        });
        self.cssom.borrow_mut().font_sets.insert(doc, h);
        h
    }
}

impl FontFaceSetLoadEventImpl for Engine {
    fn fontfaces(&self, this: Handle) -> Vec<Handle> {
        self.ev(this, |e| match &e.ext {
            EventExt::FontFaces(f) => f.clone(),
            _ => Vec::new(),
        })
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::FontFaceSetLoadEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::FontFaceSetLoadEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::FontFaces(init.fontfaces),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_serialize() {
        assert_eq!(serialize_identifier("a b"), "a\\ b");
        assert_eq!(serialize_identifier("1x"), "\\31 x");
        assert_eq!(serialize_identifier("-"), "\\-");
        assert_eq!(serialize_identifier("-2"), "-\\32 ");
        assert_eq!(serialize_identifier("\0"), "\u{FFFD}");
        assert_eq!(serialize_identifier("é_-z"), "é_-z");
    }

    #[test]
    fn pseudo_elements() {
        assert_eq!(parse_pseudo(None), Pseudo::None);
        assert_eq!(parse_pseudo(Some("::BEFORE")), Pseudo::Before);
        assert_eq!(parse_pseudo(Some("before")), Pseudo::None);
        assert_eq!(parse_pseudo(Some("::bogus")), Pseudo::Unknown);
        assert_eq!(parse_pseudo(Some("::Selection")), Pseudo::Selection);
        assert_eq!(parse_pseudo(Some("::selection:")), Pseudo::Unknown);
        assert_eq!(parse_pseudo(Some(":selection")), Pseudo::Unknown);
    }
}
