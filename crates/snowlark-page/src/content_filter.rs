// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Content blocking's part in a document's lifetime (the embedder's [`ContentFilter`]; nothing here
//! is reachable from script):
//!
//! - when a browsing context gets a new document ([`Engine::content_document_created`], before the
//!   parser has made anything), the filter's cosmetic rules for the document's URL become a
//!   user-origin style sheet of the document (not one of its CSSOM sheets: `document.styleSheets`
//!   never shows it), and its scriptlets run in the document's realm before any script of the page,
//!   as an extension's content script at `document_start` in the page's world would;
//! - as the DOM changes ([`Engine::content_dom_changed`]), classes and ids not seen before are
//!   collected, and at the next rendering update, before style, the generic hiding rules for them
//!   join the user sheet; so the rules for what the parser made are in before the first paint;
//! - procedural filters (uBlock Origin's `:has-text()`, `:matches-css()`, `:upward()`,
//!   `:matches-attr()`, `:matches-path()`, `:min-text-length()`, with the actions `:remove()`,
//!   `:style()`, `:remove-attr()`, `:remove-class()`) are evaluated by the engine itself over the
//!   DOM, at a rendering update after DOM changes, at most every [`PROCEDURAL_INTERVAL`]. Matched
//!   elements are hidden or styled the way uBlock Origin does it: an attribute with a random name
//!   per document, which the user sheet's rule for that name hides or styles.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use snowlark_bindings::rt::Cx;
use snowlark_dom::{local_name, ns, NodeId, Reaction};

use crate::engine::Engine;

/// The shortest time between two procedural passes over a document.
pub const PROCEDURAL_INTERVAL: Duration = Duration::from_millis(100);

/// A text argument of a procedural operator: `/regex/flags`, or literal text. On the wire a regular
/// expression is its source (flags are in it, as `(?i)` and the like), compiled again on arrival.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "PatternSource", into = "PatternSource")]
pub enum TextPattern {
    Literal(String),
    Regex(regex::Regex),
}

#[derive(Serialize, Deserialize)]
enum PatternSource {
    Literal(String),
    Regex(String),
}

impl From<TextPattern> for PatternSource {
    fn from(p: TextPattern) -> PatternSource {
        match p {
            TextPattern::Literal(s) => PatternSource::Literal(s),
            TextPattern::Regex(r) => PatternSource::Regex(r.as_str().to_owned()),
        }
    }
}

impl TryFrom<PatternSource> for TextPattern {
    type Error = regex::Error;
    fn try_from(p: PatternSource) -> Result<TextPattern, regex::Error> {
        Ok(match p {
            PatternSource::Literal(s) => TextPattern::Literal(s),
            PatternSource::Regex(r) => TextPattern::Regex(regex::Regex::new(&r)?),
        })
    }
}

impl TextPattern {
    /// uBlock Origin's reading of an argument; `None` for a regular expression the `regex` crate
    /// does not support (look-around, back-references).
    pub fn parse(arg: &str) -> Option<TextPattern> {
        let arg = arg.trim();
        if arg.len() >= 2 && arg.starts_with('/') {
            if let Some(end) = arg.rfind('/').filter(|&e| e > 0) {
                let flags = &arg[end + 1..];
                if flags.chars().all(|c| "gimsuy".contains(c)) {
                    let mut body = String::new();
                    if flags.contains('i') {
                        body.push_str("(?i)");
                    }
                    if flags.contains('s') {
                        body.push_str("(?s)");
                    }
                    if flags.contains('m') {
                        body.push_str("(?m)");
                    }
                    body.push_str(&arg[1..end]);
                    return regex::Regex::new(&body).ok().map(TextPattern::Regex);
                }
            }
        }
        Some(TextPattern::Literal(arg.to_owned()))
    }

    /// The text contains the pattern (`:has-text()`, `:matches-path()`).
    pub fn found_in(&self, text: &str) -> bool {
        match self {
            TextPattern::Literal(s) => text.contains(s.as_str()),
            TextPattern::Regex(r) => r.is_match(text),
        }
    }

    /// The text is the pattern (literal) or matches it (`:matches-attr()`, `:matches-css()`).
    pub fn matches_whole(&self, text: &str) -> bool {
        match self {
            TextPattern::Literal(s) => {
                if s.contains('*') {
                    // A literal with `*` wildcards.
                    let parts: Vec<&str> = s.split('*').collect();
                    let mut rest = text;
                    for (i, p) in parts.iter().enumerate() {
                        if i == 0 {
                            let Some(r) = rest.strip_prefix(p) else {
                                return false;
                            };
                            rest = r;
                        } else if i == parts.len() - 1 {
                            return rest.ends_with(p);
                        } else {
                            match rest.find(p) {
                                Some(at) => rest = &rest[at + p.len()..],
                                None => return false,
                            }
                        }
                    }
                    rest.is_empty()
                } else {
                    text == s
                }
            }
            TextPattern::Regex(r) => r.is_match(text),
        }
    }
}

/// One step of a procedural selector, applied to the elements the previous steps gave (the
/// document for the first).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProceduralStep {
    /// A CSS selector: from the document, or relative to each element (`:scope` and the selector
    /// when it starts with a combinator), or else a filter of the elements.
    Css(String),
    HasText(TextPattern),
    MatchesAttr {
        name: TextPattern,
        value: Option<TextPattern>,
    },
    MatchesCss {
        /// `None`, `Some(true)` for `::before`, `Some(false)` for `::after`.
        before: Option<bool>,
        property: String,
        value: TextPattern,
    },
    MatchesPath(TextPattern),
    MinTextLength(usize),
    UpwardCount(usize),
    UpwardSelector(String),
}

/// What happens to the elements a procedural filter selects.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProceduralAction {
    Hide,
    Remove,
    Style(String),
    RemoveAttr(TextPattern),
    RemoveClass(TextPattern),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProceduralFilter {
    pub steps: Vec<ProceduralStep>,
    pub action: ProceduralAction,
}

/// What a content filter gives a document.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DocumentFilters {
    /// Selectors whose elements are hidden.
    pub hide: Vec<String>,
    /// (selector, declarations) rules.
    pub styles: Vec<(String, String)>,
    pub procedural: Vec<ProceduralFilter>,
    /// Script run in the document's realm before the page's own ("" for none).
    pub script: String,
    /// Generic hiding by class and id applies (no `$generichide` exception).
    pub generic: bool,
    /// Generic selectors excepted for the document (handed back to
    /// [`ContentFilter::generic_selectors`]).
    pub exceptions: HashSet<String>,
}

/// The embedder's content filter.
pub trait ContentFilter: Send + Sync {
    /// The filters for a document at `url` in a page whose top-level document is at `top`; `None`
    /// when nothing applies.
    fn document_filters(&self, url: &url::Url, top: &url::Url) -> Option<DocumentFilters>;
    /// The generic hiding selectors for classes and ids first seen in a document.
    fn generic_selectors(
        &self,
        filters: &DocumentFilters,
        classes: &[String],
        ids: &[String],
    ) -> Vec<String>;
}

/// The engine's content filtering state.
#[derive(Default)]
pub(crate) struct ContentState {
    filter: Option<Arc<dyn ContentFilter>>,
    docs: FxHashMap<NodeId, DocState>,
}

struct DocState {
    filters: DocumentFilters,
    /// The attribute that hides an element (random per document); styles get `<token>-<n>`.
    token: String,
    styles: Vec<String>,
    seen_classes: FxHashSet<String>,
    seen_ids: FxHashSet<String>,
    pending_classes: Vec<String>,
    pending_ids: Vec<String>,
    /// User-sheet CSS not yet handed to the document's renderer.
    pending_css: String,
    /// The DOM changed since the last procedural pass.
    dirty: bool,
    last_pass: Option<Instant>,
}

fn rule(out: &mut String, selector: &str, declarations: &str) {
    out.push_str(selector);
    out.push('{');
    out.push_str(declarations);
    out.push_str("}\n");
}

const HIDE: &str = "display:none!important";

impl DocState {
    fn note_element(&mut self, dom: &snowlark_dom::Dom, el: NodeId) {
        if !self.filters.generic {
            return;
        }
        if let Some(classes) = dom.attr(el, &ns!(), &local_name!("class")) {
            for c in classes.split_ascii_whitespace() {
                if !self.seen_classes.contains(c) {
                    self.seen_classes.insert(c.to_owned());
                    self.pending_classes.push(c.to_owned());
                }
            }
        }
        if let Some(id) = dom.attr(el, &ns!(), &local_name!("id")) {
            if !id.is_empty() && !self.seen_ids.contains(id) {
                self.seen_ids.insert(id.to_owned());
                self.pending_ids.push(id.to_owned());
            }
        }
    }

    /// The attribute for a `:style()` action (its rule added the first time).
    fn style_token(&mut self, style: &str) -> String {
        let n = match self.styles.iter().position(|s| s == style) {
            Some(n) => n,
            None => {
                self.styles.push(style.to_owned());
                let n = self.styles.len() - 1;
                let sel = format!("[{}-{n}]", self.token);
                rule(&mut self.pending_css, &sel, style);
                n
            }
        };
        format!("{}-{n}", self.token)
    }
}

/// What a procedural pass does to one element.
enum Apply {
    SetAttr(String),
    Remove,
    RemoveAttr(TextPattern),
    RemoveClass(TextPattern),
}

impl Engine {
    /// Sets (or removes) the embedder's content filter. Documents created from then on get its
    /// filters.
    pub fn set_content_filter(&self, filter: Option<Arc<dyn ContentFilter>>) {
        let mut c = self.content.borrow_mut();
        c.filter = filter;
        c.docs.clear();
    }

    /// A browsing context's new document (`doc`, its URL set, its realm made, nothing parsed yet):
    /// its user sheet of cosmetic filters and its scriptlets.
    pub(crate) fn content_document_created(&self, cx: &mut Cx, doc: NodeId) {
        let Some(filter) = self.content.borrow().filter.clone() else {
            return;
        };
        let (url, top) = {
            let dom = self.dom();
            let Ok(url) = url::Url::parse(dom.document_url(doc)) else {
                return;
            };
            let top = self
                .document()
                .filter(|&t| t != doc)
                .and_then(|t| url::Url::parse(dom.document_url(t)).ok())
                .unwrap_or_else(|| url.clone());
            (url, top)
        };
        if !matches!(url.scheme(), "http" | "https") {
            return;
        }
        let Some(filters) = filter.document_filters(&url, &top) else {
            return;
        };
        let mut token = [0u8; 4];
        let _ = getrandom::fill(&mut token);
        let token = format!(
            "osm{:02x}{:02x}{:02x}{:02x}",
            token[0], token[1], token[2], token[3]
        );
        let mut css = String::new();
        for s in &filters.hide {
            rule(&mut css, s, HIDE);
        }
        for (s, d) in &filters.styles {
            rule(&mut css, s, d);
        }
        if !filters.procedural.is_empty() {
            rule(&mut css, &format!("[{token}]"), HIDE);
        }
        let script = (!filters.script.is_empty()).then(|| filters.script.clone());
        let dirty = !filters.procedural.is_empty();
        {
            let mut c = self.content.borrow_mut();
            let dom = self.dom();
            c.docs.retain(|d, _| dom.is_alive(*d));
            c.docs.insert(
                doc,
                DocState {
                    filters,
                    token,
                    styles: Vec::new(),
                    seen_classes: FxHashSet::default(),
                    seen_ids: FxHashSet::default(),
                    pending_classes: Vec::new(),
                    pending_ids: Vec::new(),
                    pending_css: css,
                    dirty,
                    last_pass: None,
                },
            );
        }
        self.content_flush_css(doc);
        if let Some(script) = script {
            // The document is loading from here on (the loader says so right after this hook;
            // scriptlets that wait for a later state read it now).
            self.doc_state(doc, |s| {
                s.ready_state = snowlark_bindings::generated::DocumentReadyState::Loading
            });
            self.in_document_realm(cx, doc, |cx| {
                let _ = self.eval_to_string(cx, &script, "");
            });
        }
    }

    /// Hands a document's pending user-sheet CSS to its renderer.
    fn content_flush_css(&self, doc: NodeId) {
        let css = {
            let mut c = self.content.borrow_mut();
            match c.docs.get_mut(&doc) {
                Some(s) if !s.pending_css.is_empty() => std::mem::take(&mut s.pending_css),
                _ => return,
            }
        };
        let added = self
            .with_doc_renderer(doc, |r, st| {
                r.add_user_css(&css);
                st.dirty = true;
            })
            .is_some();
        if !added {
            // Not rendered (yet): kept for when it is.
            if let Some(s) = self.content.borrow_mut().docs.get_mut(&doc) {
                s.pending_css.insert_str(0, &css);
            }
        }
    }

    /// The DOM changed: new classes and ids for generic hiding, and a procedural pass due.
    pub(crate) fn content_dom_changed(&self, reactions: &[Reaction]) {
        let mut c = self.content.borrow_mut();
        if c.docs.is_empty() {
            return;
        }
        let dom = self.dom();
        for r in reactions {
            match r {
                Reaction::Inserted {
                    node,
                    connected: true,
                    ..
                } => {
                    if !dom.is_alive(*node) || !dom.is_element(*node) {
                        continue;
                    }
                    if let Some(s) = c.docs.get_mut(&dom.node_document(*node)) {
                        s.note_element(&dom, *node);
                        s.dirty = true;
                    }
                }
                Reaction::AttributeChanged {
                    element,
                    local_name,
                    ..
                } => {
                    if !dom.is_alive(*element) {
                        continue;
                    }
                    if let Some(s) = c.docs.get_mut(&dom.node_document(*element)) {
                        if local_name.starts_with(s.token.as_str()) {
                            continue;
                        }
                        if *local_name == local_name!("class") || *local_name == local_name!("id") {
                            s.note_element(&dom, *element);
                        }
                        s.dirty = true;
                    }
                }
                Reaction::Removed {
                    node, old_parent, ..
                } => {
                    // (the node may be gone already; its old parent says which document)
                    let Some(n) = [Some(*node), *old_parent]
                        .into_iter()
                        .flatten()
                        .find(|&n| dom.is_alive(n))
                    else {
                        continue;
                    };
                    if let Some(s) = c.docs.get_mut(&dom.node_document(n)) {
                        s.dirty = true;
                    }
                }
                Reaction::ChildrenChanged { parent } => {
                    if !dom.is_alive(*parent) {
                        continue;
                    }
                    if let Some(s) = c.docs.get_mut(&dom.node_document(*parent)) {
                        s.dirty = true;
                    }
                }
                _ => {}
            }
        }
    }

    /// At a rendering update, before style: generic hiding rules for new classes and ids, and the
    /// procedural filters of documents whose DOM changed.
    pub(crate) fn content_filter_pass(&self, cx: &mut Cx) {
        let (filter, docs) = {
            let c = self.content.borrow();
            let Some(f) = c.filter.clone() else {
                return;
            };
            if c.docs.is_empty() {
                return;
            }
            (f, c.docs.keys().copied().collect::<Vec<_>>())
        };
        for doc in docs {
            // Generic class and id rules.
            let pending = {
                let mut c = self.content.borrow_mut();
                let Some(s) = c.docs.get_mut(&doc) else {
                    continue;
                };
                if s.pending_classes.is_empty() && s.pending_ids.is_empty() {
                    None
                } else {
                    Some((
                        std::mem::take(&mut s.pending_classes),
                        std::mem::take(&mut s.pending_ids),
                    ))
                }
            };
            if let Some((classes, ids)) = pending {
                let selectors = {
                    let c = self.content.borrow();
                    let s = &c.docs[&doc];
                    filter.generic_selectors(&s.filters, &classes, &ids)
                };
                if !selectors.is_empty() {
                    let mut c = self.content.borrow_mut();
                    let s = c.docs.get_mut(&doc).expect("the document's state");
                    for sel in &selectors {
                        rule(&mut s.pending_css, sel, HIDE);
                    }
                }
            }
            self.content_procedural(cx, doc);
            self.content_flush_css(doc);
        }
    }

    /// Runs a document's procedural filters if its DOM changed (at most every
    /// [`PROCEDURAL_INTERVAL`]; a pass put off asks for another rendering update).
    fn content_procedural(&self, cx: &mut Cx, doc: NodeId) {
        let filters = {
            let mut c = self.content.borrow_mut();
            let Some(s) = c.docs.get_mut(&doc) else {
                return;
            };
            if !s.dirty || s.filters.procedural.is_empty() {
                return;
            }
            if s.last_pass
                .is_some_and(|t| t.elapsed() < PROCEDURAL_INTERVAL)
            {
                drop(c);
                self.with_doc_renderer(doc, |_, st| st.dirty = true);
                return;
            }
            s.dirty = false;
            s.last_pass = Some(Instant::now());
            s.filters.procedural.clone()
        };
        if !self.dom().is_alive(doc) {
            return;
        }
        let mut work: Vec<(NodeId, Apply)> = Vec::new();
        for f in &filters {
            let found = self.procedural_select(doc, &f.steps);
            if found.is_empty() {
                continue;
            }
            let apply = |c: &mut ContentState| -> Option<Apply> {
                let s = c.docs.get_mut(&doc)?;
                Some(match &f.action {
                    ProceduralAction::Hide => Apply::SetAttr(s.token.clone()),
                    ProceduralAction::Style(style) => Apply::SetAttr(s.style_token(style)),
                    ProceduralAction::Remove => Apply::Remove,
                    ProceduralAction::RemoveAttr(p) => Apply::RemoveAttr(p.clone()),
                    ProceduralAction::RemoveClass(p) => Apply::RemoveClass(p.clone()),
                })
            };
            for el in found {
                let Some(a) = apply(&mut self.content.borrow_mut()) else {
                    return;
                };
                work.push((el, a));
            }
        }
        if work.is_empty() {
            return;
        }
        {
            let mut dom = self.dom_mut();
            for (el, a) in work {
                if !dom.is_alive(el) || dom.parent(el).is_none() {
                    continue;
                }
                match a {
                    Apply::SetAttr(name) => {
                        if dom.get_attribute(el, &name).is_none() {
                            let _ = dom.set_attribute(el, &name, "");
                        }
                    }
                    Apply::Remove => dom.remove_node(el),
                    Apply::RemoveAttr(p) => {
                        let names: Vec<String> = dom
                            .get_attribute_names(el)
                            .into_iter()
                            .map(|n| n.into_owned())
                            .filter(|n| p.matches_whole(n))
                            .collect();
                        for n in names {
                            dom.remove_attribute(el, &n);
                        }
                    }
                    Apply::RemoveClass(p) => {
                        let Some(old) = dom.get_attribute(el, "class").map(str::to_owned) else {
                            continue;
                        };
                        let kept: Vec<&str> = old
                            .split_ascii_whitespace()
                            .filter(|c| !p.matches_whole(c))
                            .collect();
                        if kept.len() != old.split_ascii_whitespace().count() {
                            let _ = dom.set_attribute(el, "class", &kept.join(" "));
                        }
                    }
                }
            }
        }
        self.after_mutation(cx);
    }

    /// The elements a procedural selector picks in `doc`.
    fn procedural_select(&self, doc: NodeId, steps: &[ProceduralStep]) -> Vec<NodeId> {
        let mut set: Option<Vec<NodeId>> = None;
        for step in steps {
            let current = set.take();
            let next: Vec<NodeId> = match step {
                ProceduralStep::Css(sel) => {
                    let relative = sel.starts_with(char::is_whitespace)
                        || sel.trim_start().starts_with(['>', '+', '~']);
                    match current {
                        None => match self.parse_selectors(sel.trim()) {
                            Ok(list) => self.query_all(doc, &list),
                            Err(_) => return Vec::new(),
                        },
                        Some(els) if relative => {
                            let Ok(list) = self.parse_selectors(&format!(":scope {}", sel.trim()))
                            else {
                                return Vec::new();
                            };
                            let mut out = Vec::new();
                            for e in els {
                                for f in self.query_all(e, &list) {
                                    if !out.contains(&f) {
                                        out.push(f);
                                    }
                                }
                            }
                            out
                        }
                        Some(els) => {
                            let Ok(list) = self.parse_selectors(sel.trim()) else {
                                return Vec::new();
                            };
                            let dom = self.dom();
                            els.into_iter()
                                .filter(|&e| self.matches_list(&dom, e, &list, Some(e)))
                                .collect()
                        }
                    }
                }
                ProceduralStep::MatchesPath(p) => {
                    let url = url::Url::parse(self.dom().document_url(doc)).ok();
                    let path = url
                        .map(|u| match u.query() {
                            Some(q) => format!("{}?{q}", u.path()),
                            None => u.path().to_owned(),
                        })
                        .unwrap_or_default();
                    if !p.found_in(&path) {
                        return Vec::new();
                    }
                    current.unwrap_or_else(|| self.all_elements(doc))
                }
                other => {
                    let els = current.unwrap_or_else(|| self.all_elements(doc));
                    self.procedural_filter_step(other, els)
                }
            };
            if next.is_empty() {
                return next;
            }
            set = Some(next);
        }
        set.unwrap_or_default()
    }

    fn all_elements(&self, doc: NodeId) -> Vec<NodeId> {
        let dom = self.dom();
        dom.descendants(doc)
            .filter(|&d| dom.is_element(d))
            .collect()
    }

    fn procedural_filter_step(&self, step: &ProceduralStep, els: Vec<NodeId>) -> Vec<NodeId> {
        match step {
            ProceduralStep::HasText(p) => {
                let dom = self.dom();
                els.into_iter()
                    .filter(|&e| p.found_in(&dom.descendant_text_content(e)))
                    .collect()
            }
            ProceduralStep::MinTextLength(n) => {
                let dom = self.dom();
                els.into_iter()
                    .filter(|&e| dom.descendant_text_content(e).chars().count() >= *n)
                    .collect()
            }
            ProceduralStep::MatchesAttr { name, value } => {
                let dom = self.dom();
                els.into_iter()
                    .filter(|&e| {
                        dom.get_attribute_names(e).iter().any(|n| {
                            name.matches_whole(n)
                                && value.as_ref().is_none_or(|v| {
                                    v.matches_whole(dom.get_attribute(e, n).unwrap_or(""))
                                })
                        })
                    })
                    .collect()
            }
            ProceduralStep::MatchesCss {
                before,
                property,
                value,
            } => els
                .into_iter()
                .filter(|&e| {
                    let v = self
                        .with_layout(e, |r, _| match before {
                            None => r.resolved_property(e, property),
                            Some(true) => r.resolved_pseudo_property(
                                e,
                                &snowlark_style::PseudoElement::Before,
                                property,
                            ),
                            Some(false) => r.resolved_pseudo_property(
                                e,
                                &snowlark_style::PseudoElement::After,
                                property,
                            ),
                        })
                        .flatten();
                    v.is_some_and(|v| value.matches_whole(&v))
                })
                .collect(),
            ProceduralStep::UpwardCount(n) => {
                let dom = self.dom();
                let mut out = Vec::new();
                for e in els {
                    let mut a = Some(e);
                    for _ in 0..*n {
                        a = a.and_then(|x| dom.parent_element(x));
                    }
                    if let Some(a) = a {
                        if !out.contains(&a) {
                            out.push(a);
                        }
                    }
                }
                out
            }
            ProceduralStep::UpwardSelector(sel) => {
                let Ok(list) = self.parse_selectors(sel) else {
                    return Vec::new();
                };
                let dom = self.dom();
                let mut out = Vec::new();
                for e in els {
                    let mut a = dom.parent_element(e);
                    while let Some(x) = a {
                        if self.matches_list(&dom, x, &list, Some(x)) {
                            if !out.contains(&x) {
                                out.push(x);
                            }
                            break;
                        }
                        a = dom.parent_element(x);
                    }
                }
                out
            }
            ProceduralStep::Css(_) | ProceduralStep::MatchesPath(_) => els,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_patterns_read_as_ublock_origin_reads_them() {
        let p = TextPattern::parse("/sponsor(ed)?/i").unwrap();
        assert!(p.found_in("A SPONSORED post"));
        assert!(!p.found_in("an article"));
        let p = TextPattern::parse("Advertisement").unwrap();
        assert!(p.found_in("-- Advertisement --"));
        assert!(!p.matches_whole("-- Advertisement --"));
        assert!(p.matches_whole("Advertisement"));
        let p = TextPattern::parse("data-ad-*").unwrap();
        assert!(p.matches_whole("data-ad-slot"));
        assert!(!p.matches_whole("data-slot"));
        // Look-behind: not supported by `regex`, the filter is dropped.
        assert!(TextPattern::parse("/(?<=a)b/").is_none());
    }
}
