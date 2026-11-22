// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSSOM §6.1-6.4: `StyleSheet`/`CSSStyleSheet` (`LinkStyle.sheet`, `document.styleSheets`, constructed
//! sheets), `MediaList`, `CSSRuleList` and the `CSSRule` interfaces over the renderer's parsed sheets
//! (`snowlark_style::cssom`). Changes made through them reach the style engine
//! (`Renderer::stylesheets_changed`). Platform objects keep their identity per sheet, rule list and
//! rule.

use snowlark_bindings::generated::{
    self as g, CSSConditionRuleImpl, CSSContainerRuleImpl, CSSFontFaceRuleImpl,
    CSSFontFeatureValuesRuleImpl, CSSFontPaletteValuesRuleImpl, CSSGroupingRuleImpl,
    CSSImportRuleImpl, CSSKeyframeRuleImpl, CSSKeyframesRuleImpl, CSSLayerBlockRuleImpl,
    CSSLayerStatementRuleImpl, CSSMediaRuleImpl, CSSNamespaceRuleImpl, CSSPropertyRuleImpl,
    CSSRuleImpl, CSSRuleListImpl, CSSStyleRuleImpl, CSSStyleSheetImpl, CSSSupportsRuleImpl,
    LinkStyleImpl, MediaListImpl, StyleSheetImpl, StyleSheetListImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsPromise};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, NodeId};
use snowlark_style::cssom::{Rule, RuleError, RuleKind, RuleList};
use snowlark_style::Sheet;

use super::cssom::Decl;
use super::{exception, node_handle, Engine, Object};

/// A `CSSStyleSheet`.
pub struct SheetObj {
    pub sheet: Sheet,
    pub owner: Option<NodeId>,
    pub owner_rule: Option<Handle>,
    /// Made with `new CSSStyleSheet()`.
    pub constructed: bool,
    /// CSSOM §6.1 "constructor document": the document of the global the constructor ran in.
    pub constructor_document: Option<NodeId>,
}

/// What a `MediaList` lists.
#[derive(Clone, Copy)]
pub enum MediaOwner {
    Sheet(Handle),
    Rule(Handle),
}

fn rule_error(e: RuleError) -> rt::Error {
    let (name, message) = match e {
        RuleError::Syntax => (ExceptionName::SyntaxError, "The rule does not parse"),
        RuleError::IndexSize => (ExceptionName::IndexSizeError, "The index is out of range"),
        RuleError::HierarchyRequest => (
            ExceptionName::HierarchyRequestError,
            "The rule cannot be inserted there",
        ),
        RuleError::InvalidState => (
            ExceptionName::InvalidStateError,
            "The rule cannot be removed",
        ),
    };
    exception(name, message)
}

impl Engine {
    /// CSSOM §6.4 `adoptedStyleSheets`: the backing list of a document or shadow root.
    pub(crate) fn adopted_sheets(&self, scope: NodeId) -> Vec<Handle> {
        self.cssom
            .borrow()
            .adopted
            .get(&scope)
            .cloned()
            .unwrap_or_default()
    }

    /// The "set an indexed value" steps of `adoptedStyleSheets` (CSSOM §6.4): only a constructed
    /// sheet whose constructor document is the scope's node document can be adopted.
    pub(crate) fn check_adoptable(&self, scope: NodeId, sheet: Handle) -> rt::Result<()> {
        let doc = self.dom().node_document(scope);
        let ok = self.sheet_obj(sheet, |s| {
            s.constructed && s.constructor_document == Some(doc)
        });
        if ok {
            Ok(())
        } else {
            Err(exception(
                ExceptionName::NotAllowedError,
                "Only a constructed style sheet made in this document can be adopted",
            ))
        }
    }

    /// Writes the backing list of `scope` (a value at an index, appended at its size, or the last
    /// item removed) and hands the scope's adopted sheets to its document's renderer: they apply
    /// after the scope's own sheets (CSSOM §6.4 "document or shadow root CSS style sheets").
    pub(crate) fn store_adopted(&self, scope: NodeId, index: u32, value: Option<Handle>) {
        let list = {
            let mut c = self.cssom.borrow_mut();
            let list = c.adopted.entry(scope).or_default();
            match value {
                Some(h) if (index as usize) < list.len() => list[index as usize] = h,
                Some(h) => list.push(h),
                None => {
                    list.pop();
                }
            }
            list.clone()
        };
        self.hand_adopted(scope, &list);
    }

    /// Hands `scope`'s adopted sheets (`list`) to its document's renderer, which takes their rules
    /// again: a shadow root's restyle its shadow tree only.
    fn hand_adopted(&self, scope: NodeId, list: &[Handle]) {
        let sheets: Vec<Sheet> = list
            .iter()
            .map(|&h| self.sheet_obj(h, |s| s.sheet.clone()))
            .collect();
        let doc = self.dom().node_document(scope);
        self.with_doc_renderer(doc, |r, st| {
            r.set_adopted_sheets(scope, sheets);
            st.dirty = true;
        });
    }

    /// HTML §4.2.6 "update a style block" for a `<style>` element of a document no renderer draws
    /// (one without a browsing context, such as `createHTMLDocument`'s): a connected element whose
    /// `type` is CSS and which the document's policy allows has a style sheet of its child text
    /// content, made again when that text, `type` or `media` changed (each runs the steps again),
    /// so rules a script inserted stay until then. The renderer does this for drawn documents.
    pub(crate) fn unrendered_style_sheet(&self, owner: NodeId) -> Option<Sheet> {
        let (doc, text, ty, media) = {
            let dom = self.dom();
            if !dom.is_connected(owner) {
                drop(dom);
                self.cssom.borrow_mut().unrendered.remove(&owner);
                return None;
            }
            let attr = |n| dom.attr(owner, &snowlark_dom::ns!(), &n).map(str::to_owned);
            (
                dom.node_document(owner),
                dom.child_text_content(owner),
                attr(local_name!("type")),
                attr(local_name!("media")),
            )
        };
        if ty
            .as_deref()
            .is_some_and(|t| !t.is_empty() && !t.eq_ignore_ascii_case("text/css"))
        {
            return None;
        }
        if let Some(allowed) = self.style_block_check(doc) {
            if !allowed(&self.dom(), owner) {
                return None;
            }
        }
        if let Some(s) = self.cssom.borrow().unrendered.get(&owner) {
            if s.text == text && s.ty == ty && s.media == media {
                return Some(s.sheet.clone());
            }
        }
        let sheet = snowlark_style::parse_standalone(
            &text,
            &self.base_url_of(doc),
            media.as_deref().unwrap_or(""),
        );
        self.cssom.borrow_mut().unrendered.insert(
            owner,
            super::cssom::UnrenderedSheet {
                text,
                ty,
                media,
                sheet: sheet.clone(),
            },
        );
        Some(sheet)
    }

    /// The platform object of a sheet (one per parsed sheet).
    pub(crate) fn sheet_handle(&self, sheet: Sheet, owner: Option<NodeId>) -> Handle {
        let id = sheet.id();
        if let Some(h) = self.cssom.borrow().sheets.get(&id) {
            return *h;
        }
        let h = self.alloc(Object::StyleSheet(Box::new(SheetObj {
            sheet,
            owner,
            owner_rule: None,
            constructed: false,
            constructor_document: None,
        })));
        self.cssom.borrow_mut().sheets.insert(id, h);
        h
    }

    fn sheet_obj<R>(&self, h: Handle, f: impl FnOnce(&SheetObj) -> R) -> R {
        match &*self.obj(h) {
            Object::StyleSheet(s) => f(s),
            _ => panic!("not a CSSStyleSheet"),
        }
    }

    fn rule_list_handle(&self, list: RuleList, sheet: Handle, parent: Option<Handle>) -> Handle {
        let id = list.id();
        if let Some(h) = self.cssom.borrow().rule_lists.get(&id) {
            return *h;
        }
        let h = self.alloc(Object::CssRuleList {
            list,
            sheet,
            parent,
        });
        self.cssom.borrow_mut().rule_lists.insert(id, h);
        h
    }

    fn rule_handle(&self, rule: Rule, sheet: Handle, parent: Option<Handle>) -> Handle {
        let id = rule.id();
        if let Some(h) = self.cssom.borrow().rules.get(&id) {
            return *h;
        }
        let h = self.alloc(Object::CssRule {
            rule,
            sheet,
            parent,
            detached: false,
        });
        self.cssom.borrow_mut().rules.insert(id, h);
        h
    }

    pub(crate) fn rule_of(&self, h: Handle) -> Rule {
        match &*self.obj(h) {
            Object::CssRule { rule, .. } => rule.clone(),
            _ => panic!("not a CSSRule"),
        }
    }

    fn rule_sheet(&self, h: Handle) -> Handle {
        match &*self.obj(h) {
            Object::CssRule { sheet, .. } => *sheet,
            _ => panic!("not a CSSRule"),
        }
    }

    /// The interface of a rule object.
    pub(crate) fn rule_interface(rule: &Rule) -> InterfaceId {
        match rule.kind() {
            RuleKind::Style => InterfaceId::CSSStyleRule,
            RuleKind::Import => InterfaceId::CSSImportRule,
            RuleKind::Media => InterfaceId::CSSMediaRule,
            RuleKind::Supports => InterfaceId::CSSSupportsRule,
            RuleKind::FontFace => InterfaceId::CSSFontFaceRule,
            RuleKind::Keyframes => InterfaceId::CSSKeyframesRule,
            RuleKind::Keyframe => InterfaceId::CSSKeyframeRule,
            RuleKind::Namespace => InterfaceId::CSSNamespaceRule,
            RuleKind::LayerBlock => InterfaceId::CSSLayerBlockRule,
            RuleKind::LayerStatement => InterfaceId::CSSLayerStatementRule,
            RuleKind::Container => InterfaceId::CSSContainerRule,
            RuleKind::FontFeatureValues => InterfaceId::CSSFontFeatureValuesRule,
            RuleKind::FontPaletteValues => InterfaceId::CSSFontPaletteValuesRule,
            RuleKind::Property => InterfaceId::CSSPropertyRule,
            _ => InterfaceId::CSSRule,
        }
    }

    /// CSSOM: the sheet's rules are readable (its origin-clean flag is set): a sheet of another
    /// origin than its document's is not.
    fn check_origin_clean(&self, sheet: Handle) -> rt::Result<()> {
        let (owner, constructed) = self.sheet_obj(sheet, |s| (s.owner, s.constructed));
        let Some(owner) = owner.filter(|_| !constructed) else {
            return Ok(());
        };
        let dom = self.dom();
        if !dom.is_alive(owner) || !dom.is_html_element_named(owner, &local_name!("link")) {
            return Ok(());
        }
        let Some(href) = dom.get_attribute(owner, "href").map(str::to_owned) else {
            return Ok(());
        };
        let doc = dom.node_document(owner);
        drop(dom);
        let base = self.base_url_of(owner);
        let doc_origin = self.origin_of_document(doc);
        match base.join(href.trim()) {
            Ok(u) if u.origin().ascii_serialization() != doc_origin && u.scheme() != "data" => {
                Err(exception(
                    ExceptionName::SecurityError,
                    "The sheet is of another origin",
                ))
            }
            _ => Ok(()),
        }
    }

    /// `sheet`'s rules changed (or it was enabled or disabled): the documents and shadow roots it
    /// applies to take them again. A constructed sheet applies where it is adopted (a shadow root
    /// restyles its shadow tree only), and nowhere before that: `new CSSStyleSheet()` then
    /// `replaceSync()`, as Lit makes its components' sheets, restyles nothing. A sheet of a `style` or
    /// `link` element applies in its document; one an `@import` brought, wherever its parent does
    /// (every document, as before).
    fn sheet_mutated(&self, sheet: Handle) {
        let (constructed, owner, imported) =
            self.sheet_obj(sheet, |s| (s.constructed, s.owner, s.owner_rule.is_some()));
        if imported {
            return self.sheets_mutated();
        }
        if constructed {
            let scopes: Vec<(NodeId, Vec<Handle>)> = self
                .cssom
                .borrow()
                .adopted
                .iter()
                .filter(|(_, list)| list.contains(&sheet))
                .map(|(&scope, list)| (scope, list.clone()))
                .collect();
            for (scope, list) in scopes {
                let (alive, shadow, doc) = {
                    let dom = self.dom();
                    (
                        dom.is_alive(scope),
                        dom.is_alive(scope) && dom.is_shadow_root(scope),
                        dom.is_alive(scope).then(|| dom.node_document(scope)),
                    )
                };
                if !alive {
                    continue;
                }
                if shadow {
                    self.hand_adopted(scope, &list);
                } else if let Some(doc) = doc {
                    self.with_doc_renderer(doc, |r, st| {
                        r.stylesheets_changed();
                        st.dirty = true;
                    });
                }
            }
            return;
        }
        match owner {
            Some(owner) => {
                let doc = {
                    let dom = self.dom();
                    dom.is_alive(owner).then(|| dom.node_document(owner))
                };
                if let Some(doc) = doc {
                    self.with_doc_renderer(doc, |r, st| {
                        r.stylesheets_changed();
                        st.dirty = true;
                    });
                }
            }
            None => self.sheets_mutated(),
        }
    }

    /// A sheet's rules changed, wherever it applies: every renderer rebuilds its cascade.
    fn sheets_mutated(&self) {
        self.for_each_renderer(|r, st| {
            r.stylesheets_changed();
            st.dirty = true;
        });
    }

    fn sheet_rule_list(&self, sheet: Handle) -> Handle {
        let list = self.sheet_obj(sheet, |s| s.sheet.rules());
        self.rule_list_handle(list, sheet, None)
    }

    fn sheet_insert(&self, sheet: Handle, rule: &str, index: u32) -> rt::Result<u32> {
        self.check_origin_clean(sheet)?;
        let list = self.sheet_obj(sheet, |s| s.sheet.rules());
        let i = list.insert(rule, index as usize).map_err(rule_error)?;
        self.sheet_mutated(sheet);
        Ok(i as u32)
    }

    fn sheet_delete(&self, sheet: Handle, index: u32) -> rt::Result<()> {
        self.check_origin_clean(sheet)?;
        let list = self.sheet_obj(sheet, |s| s.sheet.rules());
        list.remove(index as usize).map_err(rule_error)?;
        self.sheet_mutated(sheet);
        Ok(())
    }

    /// The sheets of a document or shadow root in tree order, for `styleSheets`.
    fn tree_sheets(&self, scope: NodeId) -> Vec<(NodeId, Sheet)> {
        let doc = {
            let dom = self.dom();
            if dom.is_document(scope) {
                scope
            } else {
                dom.node_document(scope)
            }
        };
        if !self.is_rendered_document(doc) {
            return Vec::new();
        }
        // (the lists follow the DOM as it changes: reading them needs no style update)
        self.with_node_renderer(scope, |r, _| {
            r.style()
                .map(|(engine, _)| engine.tree_sheets(scope))
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    fn style_sheet_list_scope(&self, h: Handle) -> NodeId {
        match &*self.obj(h) {
            Object::StyleSheetList { scope } => *scope,
            _ => panic!("not a StyleSheetList"),
        }
    }

    /// `DocumentOrShadowRoot.styleSheets`.
    pub(crate) fn style_sheet_list(&self, scope: NodeId) -> Handle {
        if let Some(h) = self.cssom.borrow().sheet_lists.get(&scope) {
            return *h;
        }
        let h = self.alloc(Object::StyleSheetList { scope });
        self.cssom.borrow_mut().sheet_lists.insert(scope, h);
        h
    }

    /// HTML "create a CSS module script" steps 3-4: a constructed sheet of `doc` made with an
    /// empty dictionary, whose rules `text` replaces synchronously (its `@import` rules dropped);
    /// relative URLs in it resolve against the module's URL `base`.
    pub(crate) fn css_module_sheet(&self, doc: NodeId, text: &str, base: &url::Url) -> Handle {
        let cleaned = text.replace("@import", "@-snowlark-dropped-import");
        let sheet = self
            .with_doc_renderer(doc, |r, _| r.parse_constructed_sheet(&cleaned, base, ""))
            .flatten()
            .unwrap_or_else(|| snowlark_style::parse_standalone(&cleaned, base, ""));
        let id = sheet.id();
        let h = self.alloc(Object::StyleSheet(Box::new(SheetObj {
            sheet,
            owner: None,
            owner_rule: None,
            constructed: true,
            constructor_document: Some(doc),
        })));
        self.cssom.borrow_mut().sheets.insert(id, h);
        h
    }
}

impl LinkStyleImpl for Engine {
    fn sheet(&self, this: Handle) -> Option<Handle> {
        let owner = self.n(this);
        let sheet = match self
            .with_node_renderer(owner, |r, _| r.style().and_then(|(e, _)| e.sheet_of(owner)))
        {
            Some(sheet) => sheet?,
            // (a document no renderer draws: one without a browsing context)
            None => self.unrendered_style_sheet(owner)?,
        };
        Some(self.sheet_handle(sheet, Some(owner)))
    }
}

impl StyleSheetListImpl for Engine {
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        let scope = self.style_sheet_list_scope(this);
        let (owner, sheet) = self.tree_sheets(scope).into_iter().nth(index as usize)?;
        Some(self.sheet_handle(sheet, Some(owner)))
    }
    fn length(&self, this: Handle) -> u32 {
        let scope = self.style_sheet_list_scope(this);
        self.tree_sheets(scope).len() as u32
    }
}

impl StyleSheetImpl for Engine {
    fn type_(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("text/css")
    }
    fn href(&self, this: Handle) -> Option<DomStr<'_>> {
        let owner = self.sheet_obj(this, |s| s.owner)?;
        let is_link = self
            .dom()
            .is_html_element_named(owner, &local_name!("link"));
        if !is_link {
            return None;
        }
        let href = self.dom().get_attribute(owner, "href")?.to_owned();
        let base = self.base_url_of(owner);
        base.join(href.trim())
            .ok()
            .map(|u| DomStr::Owned(u.to_string()))
    }
    fn owner_node(&self, this: Handle) -> Option<g::ElementOrProcessingInstruction> {
        let owner = self.sheet_obj(this, |s| s.owner)?;
        if self.dom().is_element(owner) {
            Some(g::ElementOrProcessingInstruction::Element(node_handle(
                owner,
            )))
        } else {
            Some(g::ElementOrProcessingInstruction::ProcessingInstruction(
                node_handle(owner),
            ))
        }
    }
    fn parent_style_sheet(&self, _this: Handle) -> Option<Handle> {
        None
    }
    fn title(&self, this: Handle) -> Option<DomStr<'_>> {
        let owner = self.sheet_obj(this, |s| s.owner)?;
        let t = self.dom().get_attribute(owner, "title")?.to_owned();
        (!t.is_empty()).then_some(DomStr::Owned(t))
    }
    fn media(&self, this: Handle) -> Handle {
        self.alloc(Object::MediaList(MediaOwner::Sheet(this)))
    }
    fn disabled(&self, this: Handle) -> bool {
        self.sheet_obj(this, |s| s.sheet.disabled())
    }
    fn set_disabled(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        if self.sheet_obj(this, |s| s.sheet.set_disabled(value)) {
            self.sheet_mutated(this);
        }
        Ok(())
    }
}

impl CSSStyleSheetImpl for Engine {
    fn owner_rule(&self, this: Handle) -> Option<Handle> {
        self.sheet_obj(this, |s| s.owner_rule)
    }
    fn css_rules(&self, this: Handle) -> Handle {
        // (A sheet of another origin answers an empty list: the getter cannot throw here.)
        if self.check_origin_clean(this).is_err() {
            return self.alloc(Object::CssRuleList {
                list: snowlark_style::parse_standalone(
                    "",
                    &url::Url::parse("about:blank").expect("valid URL"),
                    "",
                )
                .rules(),
                sheet: this,
                parent: None,
            });
        }
        self.sheet_rule_list(this)
    }
    fn rules(&self, this: Handle) -> Handle {
        CSSStyleSheetImpl::css_rules(self, this)
    }
    fn insert_rule(&self, _cx: &mut Cx, this: Handle, rule: &str, index: u32) -> rt::Result<u32> {
        self.sheet_insert(this, rule, index)
    }
    fn delete_rule(&self, _cx: &mut Cx, this: Handle, index: u32) -> rt::Result<()> {
        self.sheet_delete(this, index)
    }
    fn replace(&self, cx: &mut Cx, this: Handle, text: &str) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        match CSSStyleSheetImpl::replace_sync(self, cx, this, text) {
            Ok(()) => p.resolve(cx, this),
            Err(e) => p.reject(cx, e),
        }
        Ok(p)
    }
    fn replace_sync(&self, _cx: &mut Cx, this: Handle, text: &str) -> rt::Result<()> {
        if !self.sheet_obj(this, |s| s.constructed) {
            return Err(exception(
                ExceptionName::NotAllowedError,
                "Only constructed sheets can be replaced",
            ));
        }
        // CSSOM: `@import` rules of a replaced sheet are dropped.
        let cleaned: String = text.replace("@import", "@-snowlark-dropped-import");
        self.sheet_obj(this, |s| s.sheet.replace_rules(&cleaned));
        self.sheet_mutated(this);
        Ok(())
    }
    fn add_rule(
        &self,
        _cx: &mut Cx,
        this: Handle,
        selector: &str,
        style: &str,
        index: Option<u32>,
    ) -> rt::Result<i32> {
        let len = self.sheet_obj(this, |s| s.sheet.rules().len()) as u32;
        let rule = if style.is_empty() {
            format!("{selector} {{ }}")
        } else {
            format!("{selector} {{ {style} }}")
        };
        self.sheet_insert(this, &rule, index.unwrap_or(len))?;
        Ok(-1)
    }
    fn remove_rule(&self, _cx: &mut Cx, this: Handle, index: u32) -> rt::Result<()> {
        self.sheet_delete(this, index)
    }
    /// CSSOM §6.1.2 `new CSSStyleSheet(options)`: a constructed sheet whose constructor document
    /// is the current global's document, parsed under that document's style lock so it can be
    /// adopted there.
    fn constructor(&self, cx: &mut Cx, options: g::CSSStyleSheetInit) -> rt::Result<Handle> {
        let constructor_document = self.current_document(cx)?;
        let document_base = self.base_url_of(constructor_document);
        let base = match options.base_url.as_deref() {
            Some(b) => document_base.join(b).map_err(|_| {
                exception(
                    ExceptionName::NotAllowedError,
                    "The base URL does not parse",
                )
            })?,
            None => document_base,
        };
        let media = match &options.media {
            g::MediaListOrString::String(s) => s.clone(),
            g::MediaListOrString::MediaList(h) => MediaListImpl::media_text(self, *h).into_owned(),
        };
        let sheet = self
            .with_doc_renderer(constructor_document, |r, _| {
                r.parse_constructed_sheet("", &base, &media)
            })
            .flatten()
            .unwrap_or_else(|| snowlark_style::parse_standalone("", &base, &media));
        sheet.set_disabled(options.disabled);
        let id = sheet.id();
        let h = self.alloc(Object::StyleSheet(Box::new(SheetObj {
            sheet,
            owner: None,
            owner_rule: None,
            constructed: true,
            constructor_document: Some(constructor_document),
        })));
        self.cssom.borrow_mut().sheets.insert(id, h);
        Ok(h)
    }
}

impl MediaListImpl for Engine {
    fn media_text(&self, this: Handle) -> DomStr<'_> {
        let owner = match &*self.obj(this) {
            Object::MediaList(o) => *o,
            _ => return DomStr::Borrowed(""),
        };
        DomStr::Owned(match owner {
            MediaOwner::Sheet(s) => self.sheet_obj(s, |s| s.sheet.media_text()),
            MediaOwner::Rule(r) => self.rule_of(r).media_text().unwrap_or_default(),
        })
    }
    fn set_media_text(&self, _cx: &mut Cx, _this: Handle, _value: &str) -> rt::Result<()> {
        Ok(())
    }
    fn length(&self, this: Handle) -> u32 {
        let text = MediaListImpl::media_text(self, this).into_owned();
        if text.is_empty() {
            0
        } else {
            text.split(',').count() as u32
        }
    }
    fn item(&self, this: Handle, index: u32) -> Option<DomStr<'_>> {
        let text = MediaListImpl::media_text(self, this).into_owned();
        text.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .nth(index as usize)
            .map(|s| DomStr::Owned(s.to_owned()))
    }
    fn append_medium(&self, _cx: &mut Cx, _this: Handle, _medium: &str) -> rt::Result<()> {
        Ok(())
    }
    fn delete_medium(&self, _cx: &mut Cx, _this: Handle, _medium: &str) -> rt::Result<()> {
        Ok(())
    }
}

impl CSSRuleListImpl for Engine {
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        let (list, sheet, parent) = match &*self.obj(this) {
            Object::CssRuleList {
                list,
                sheet,
                parent,
            } => (list.clone(), *sheet, *parent),
            _ => return None,
        };
        let rule = list.get(index as usize)?;
        Some(self.rule_handle(rule, sheet, parent))
    }
    fn length(&self, this: Handle) -> u32 {
        match &*self.obj(this) {
            Object::CssRuleList { list, .. } => list.len() as u32,
            _ => 0,
        }
    }
}

impl CSSRuleImpl for Engine {
    fn css_text(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.rule_of(this).css_text())
    }
    fn set_css_text(&self, _cx: &mut Cx, _this: Handle, _value: &str) -> rt::Result<()> {
        // CSSOM: setting `cssText` does nothing.
        Ok(())
    }
    fn parent_rule(&self, this: Handle) -> Option<Handle> {
        match &*self.obj(this) {
            Object::CssRule {
                parent,
                detached: false,
                ..
            } => *parent,
            _ => None,
        }
    }
    fn parent_style_sheet(&self, this: Handle) -> Option<Handle> {
        match &*self.obj(this) {
            Object::CssRule {
                sheet,
                detached: false,
                ..
            } => Some(*sheet),
            _ => None,
        }
    }
    fn type_(&self, this: Handle) -> u16 {
        self.rule_of(this).type_code()
    }
}

impl CSSStyleRuleImpl for Engine {
    fn selector_text(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.rule_of(this).selector_text().unwrap_or_default())
    }
    fn set_selector_text(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        if self.rule_of(this).set_selector_text(value) {
            self.sheets_mutated();
        }
        Ok(())
    }
    fn style(&self, this: Handle) -> Handle {
        self.alloc(Object::StyleDeclaration(Decl::Rule(this)))
    }
}

impl Engine {
    fn group_list(&self, this: Handle) -> Option<Handle> {
        let list = self.rule_of(this).child_rules()?;
        Some(self.rule_list_handle(list, self.rule_sheet(this), Some(this)))
    }
}

impl CSSGroupingRuleImpl for Engine {
    fn css_rules(&self, this: Handle) -> Handle {
        match self.group_list(this) {
            Some(h) => h,
            None => self.alloc(Object::CssRuleList {
                list: snowlark_style::parse_standalone(
                    "",
                    &url::Url::parse("about:blank").expect("valid URL"),
                    "",
                )
                .rules(),
                sheet: self.rule_sheet(this),
                parent: Some(this),
            }),
        }
    }
    fn insert_rule(&self, _cx: &mut Cx, this: Handle, rule: &str, index: u32) -> rt::Result<u32> {
        let list = self
            .rule_of(this)
            .child_rules()
            .ok_or_else(|| rule_error(RuleError::HierarchyRequest))?;
        let i = list.insert(rule, index as usize).map_err(rule_error)?;
        self.sheets_mutated();
        Ok(i as u32)
    }
    fn delete_rule(&self, _cx: &mut Cx, this: Handle, index: u32) -> rt::Result<()> {
        let list = self
            .rule_of(this)
            .child_rules()
            .ok_or_else(|| rule_error(RuleError::IndexSize))?;
        list.remove(index as usize).map_err(rule_error)?;
        self.sheets_mutated();
        Ok(())
    }
}

impl CSSConditionRuleImpl for Engine {
    fn condition_text(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.rule_of(this).condition_text().unwrap_or_default())
    }
}

impl CSSMediaRuleImpl for Engine {
    fn media(&self, this: Handle) -> Handle {
        self.alloc(Object::MediaList(MediaOwner::Rule(this)))
    }
    fn matches(&self, this: Handle) -> bool {
        let text = self.rule_of(this).media_text().unwrap_or_default();
        self.with_renderer(|r, _| r.evaluate_media(&text))
            .unwrap_or(false)
    }
}

impl CSSSupportsRuleImpl for Engine {
    fn matches(&self, this: Handle) -> bool {
        let text = self.rule_of(this).condition_text().unwrap_or_default();
        snowlark_style::supports_condition(&text)
    }
}

impl CSSImportRuleImpl for Engine {
    fn href(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.rule_of(this).import_href().unwrap_or_default())
    }
    fn media(&self, this: Handle) -> Handle {
        self.alloc(Object::MediaList(MediaOwner::Rule(this)))
    }
    fn style_sheet(&self, this: Handle) -> Option<Handle> {
        let sheet = self.rule_of(this).import_sheet()?;
        let h = self.sheet_handle(sheet, None);
        if let Object::StyleSheet(s) = &mut *self.obj_mut(h) {
            s.owner_rule = Some(this);
        }
        Some(h)
    }
    fn layer_name(&self, this: Handle) -> Option<DomStr<'_>> {
        self.rule_of(this).import_layer_name().map(DomStr::Owned)
    }
    fn supports_text(&self, this: Handle) -> Option<DomStr<'_>> {
        self.rule_of(this).import_supports_text().map(DomStr::Owned)
    }
}

/// CSS Conditional 5 §6.1: `containerName` and `containerQuery` describe the rule's only
/// `<container-condition>`; with several they are empty.
impl CSSContainerRuleImpl for Engine {
    fn container_name(&self, this: Handle) -> DomStr<'_> {
        let conditions = self
            .rule_of(this)
            .container_conditions()
            .unwrap_or_default();
        DomStr::Owned(match conditions.as_slice() {
            [(name, _)] => name.clone(),
            _ => String::new(),
        })
    }
    fn container_query(&self, this: Handle) -> DomStr<'_> {
        let conditions = self
            .rule_of(this)
            .container_conditions()
            .unwrap_or_default();
        DomStr::Owned(match conditions.as_slice() {
            [(_, query)] => query.clone(),
            _ => String::new(),
        })
    }
    fn conditions(&self, this: Handle) -> Vec<g::CSSContainerCondition> {
        self.rule_of(this)
            .container_conditions()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, query)| g::CSSContainerCondition { name, query })
            .collect()
    }
}

impl CSSFontFaceRuleImpl for Engine {
    fn style(&self, this: Handle) -> Handle {
        self.alloc(Object::StyleDeclaration(Decl::FontFace(this)))
    }
}

/// CSS Fonts 4 §8.1.1. The feature value maps (`stylistic`, `swash`, ...) are `maplike`, which the
/// bindings do not generate yet, so they are absent.
impl CSSFontFeatureValuesRuleImpl for Engine {
    fn font_family(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.rule_of(this)
                .font_feature_values_family()
                .unwrap_or_default(),
        )
    }
    /// The rule is replaced by a copy in its list (stylo's rules of this kind are immutable); the
    /// platform object stays the same and now stands for the copy.
    fn set_font_family(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let old = self.rule_of(this);
        let Some(new) = old.set_font_feature_values_family(value) else {
            return Ok(());
        };
        {
            let mut cssom = self.cssom.borrow_mut();
            cssom.rules.remove(&old.id());
            cssom.rules.insert(new.id(), this);
        }
        if let Object::CssRule { rule, .. } = &mut *self.obj_mut(this) {
            *rule = new;
        }
        self.sheets_mutated();
        Ok(())
    }
}

/// CSS Fonts 4 §9.3.1: the rule's name and descriptors, read only.
impl CSSFontPaletteValuesRuleImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        let values = self.rule_of(this).font_palette_values();
        DomStr::Owned(values.map(|v| v.name).unwrap_or_default())
    }
    fn font_family(&self, this: Handle) -> DomStr<'_> {
        let values = self.rule_of(this).font_palette_values();
        DomStr::Owned(values.map(|v| v.font_family).unwrap_or_default())
    }
    fn base_palette(&self, this: Handle) -> DomStr<'_> {
        let values = self.rule_of(this).font_palette_values();
        DomStr::Owned(values.map(|v| v.base_palette).unwrap_or_default())
    }
    fn override_colors(&self, this: Handle) -> DomStr<'_> {
        let values = self.rule_of(this).font_palette_values();
        DomStr::Owned(values.map(|v| v.override_colors).unwrap_or_default())
    }
}

/// CSS Properties and Values API 1 §5.1: the rule's name and descriptors, read only.
impl CSSPropertyRuleImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        let values = self.rule_of(this).property_rule();
        DomStr::Owned(values.map(|v| v.name).unwrap_or_default())
    }
    fn syntax(&self, this: Handle) -> DomStr<'_> {
        let values = self.rule_of(this).property_rule();
        DomStr::Owned(values.map(|v| v.syntax).unwrap_or_default())
    }
    fn inherits(&self, this: Handle) -> bool {
        self.rule_of(this)
            .property_rule()
            .is_some_and(|v| v.inherits)
    }
    fn initial_value(&self, this: Handle) -> Option<DomStr<'_>> {
        let values = self.rule_of(this).property_rule()?;
        values.initial_value.map(DomStr::Owned)
    }
}

/// CSS Animations 1 §6.2.
impl CSSKeyframesRuleImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.rule_of(this).keyframes_name().unwrap_or_default())
    }
    fn set_name(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        if self.rule_of(this).set_keyframes_name(value) {
            self.sheets_mutated();
        }
        Ok(())
    }
    fn css_rules(&self, this: Handle) -> Handle {
        match self.rule_of(this).keyframe_rules() {
            Some(list) => self.rule_list_handle(list, self.rule_sheet(this), Some(this)),
            None => CSSGroupingRuleImpl::css_rules(self, this),
        }
    }
    fn length(&self, this: Handle) -> u32 {
        self.rule_of(this)
            .keyframe_rules()
            .map_or(0, |l| l.len() as u32)
    }
    fn indexed_getter(&self, this: Handle, index: u32) -> Option<Handle> {
        let rule = self.rule_of(this).keyframe_rules()?.get(index as usize)?;
        Some(self.rule_handle(rule, self.rule_sheet(this), Some(this)))
    }
    fn append_rule(&self, _cx: &mut Cx, this: Handle, rule: &str) -> rt::Result<()> {
        if self.rule_of(this).append_keyframe(rule) {
            self.sheets_mutated();
        }
        Ok(())
    }
    fn delete_rule(&self, _cx: &mut Cx, this: Handle, select: &str) -> rt::Result<()> {
        let Some(removed) = self.rule_of(this).delete_keyframe(select) else {
            return Ok(());
        };
        // The removed keyframe's object, if script holds one, has no parent any more.
        let object = self.cssom.borrow_mut().rules.remove(&removed);
        if let Some(h) = object {
            if let Object::CssRule { detached, .. } = &mut *self.obj_mut(h) {
                *detached = true;
            }
        }
        self.sheets_mutated();
        Ok(())
    }
    fn find_rule(&self, _cx: &mut Cx, this: Handle, select: &str) -> rt::Result<Option<Handle>> {
        let Some(rule) = self.rule_of(this).find_keyframe(select) else {
            return Ok(None);
        };
        Ok(Some(self.rule_handle(
            rule,
            self.rule_sheet(this),
            Some(this),
        )))
    }
}

/// CSS Animations 1 §6.3.
impl CSSKeyframeRuleImpl for Engine {
    fn key_text(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.rule_of(this).key_text().unwrap_or_default())
    }
    fn set_key_text(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        if self.rule_of(this).set_key_text(value).map_err(rule_error)? {
            self.sheets_mutated();
        }
        Ok(())
    }
    fn style(&self, this: Handle) -> Handle {
        self.alloc(Object::StyleDeclaration(Decl::Rule(this)))
    }
}

impl CSSNamespaceRuleImpl for Engine {
    fn namespace_uri(&self, this: Handle) -> DomStr<'_> {
        // `@namespace prefix url(...)`: the URL from the serialization.
        let text = self.rule_of(this).css_text();
        let uri = text
            .split("url(\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap_or("");
        DomStr::Owned(uri.to_owned())
    }
    fn prefix(&self, this: Handle) -> DomStr<'_> {
        let text = self.rule_of(this).css_text();
        let rest = text.trim_start_matches("@namespace").trim_start();
        let prefix = if rest.starts_with("url(") {
            ""
        } else {
            rest.split_whitespace().next().unwrap_or("")
        };
        DomStr::Owned(prefix.to_owned())
    }
}

impl CSSLayerBlockRuleImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        let text = self.rule_of(this).css_text();
        let rest = text.trim_start_matches("@layer").trim_start();
        let name = if rest.starts_with('{') {
            ""
        } else {
            rest.split(['{', ' ']).next().unwrap_or("")
        };
        DomStr::Owned(name.to_owned())
    }
}

impl CSSLayerStatementRuleImpl for Engine {
    fn name_list(&self, this: Handle) -> Vec<String> {
        let text = self.rule_of(this).css_text();
        text.trim_start_matches("@layer")
            .trim_end_matches(';')
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    }
}
