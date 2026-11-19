// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Stylesheets: parsing (with `@import` resolution left to an [`ImportLoader`]), media lists, the
//! user-agent sheet, and the `@font-face` rules a sheet contributes ([`FontFace`]).

use std::sync::{Arc as StdArc, LazyLock};

use cssparser::Parser;
use servo_arc::Arc;
use style::context::QuirksMode;
use style::device::Device;
use style::font_face::Source;
use style::media_queries::MediaList;
use style::parser::ParserContext;
use style::shared_lock::{Locked, SharedRwLock, SharedRwLockReadGuard};
use style::stylesheets::import_rule::{ImportLayer, ImportSheet, ImportSupportsCondition};
use style::stylesheets::scope_rule::ImplicitScopeRoot;
use style::stylesheets::{
    AllRules, AllowImportRules, CssRule, CssRuleType, CustomMediaMap, DocumentStyleSheet,
    ImportRule, Origin, Stylesheet, StylesheetContents, StylesheetInDocument, StylesheetLoader,
    UrlExtraData,
};
use style::values::{CssUrl, SourceLocation};
use style_traits::{ParsingMode, ToCss};
use url::Url;

/// The user-agent stylesheet: HTML §15 "Rendering", the CSS the spec says a user agent is expected to
/// apply.
pub const UA_CSS: &str = include_str!("ua.css");

/// The extra user-agent rules for documents in quirks mode (HTML §15.3.10 and the Quirks Mode Standard).
pub const QUIRKS_CSS: &str = include_str!("quirks.css");

/// A parsed stylesheet. Cheap to clone (a shared pointer); two `Sheet`s are equal when they are the same
/// parsed sheet.
#[derive(Clone, Debug, PartialEq)]
pub struct Sheet(pub(crate) DocumentStyleSheet);

impl Sheet {
    /// The stylo stylesheet, for the CSSOM layer.
    pub fn stylo(&self) -> &Arc<Stylesheet> {
        &self.0 .0
    }

    /// Whether the sheet was parsed under `data`'s lock, so it can join that document's cascade
    /// (stylo reads a sheet under its own lock's guard).
    pub fn uses_lock_of(&self, data: &crate::StyleData) -> bool {
        self.0 .0.shared_lock.same_lock(data.shared_lock())
    }

    /// Whether the sheet is disabled (`StyleSheet.disabled`).
    pub fn disabled(&self) -> bool {
        self.0 .0.disabled()
    }

    /// Sets the scoping root of the sheet's `@scope` rules that have no `<scope-start>` (CSS Cascade
    /// 6 §2.1.1).
    pub(crate) fn set_implicit_scope_root(&self, root: Option<ImplicitScopeRoot>) {
        *self.0 .0.implicit_scope_root.lock().unwrap() = root;
    }

    /// Sets `StyleSheet.disabled`. The engine needs `StyleEngine::sheets_changed` afterwards.
    pub fn set_disabled(&self, disabled: bool) -> bool {
        self.0 .0.set_disabled(disabled)
    }

    /// Gives the sheet contents of its own when it shares them with other sheets (the engine's cache
    /// of `style` element text): a deep copy, before the CSSOM reaches its rules, so that a change
    /// through one sheet's rules is that sheet's alone.
    pub(crate) fn ensure_unique_contents(&self) {
        let sheet = &self.0 .0;
        if !sheet
            .shared_contents
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let lock = &sheet.shared_lock;
        let copy = {
            let guard = lock.read();
            sheet
                .contents
                .read_with(&guard)
                .deep_clone(lock, None, &guard)
        };
        let mut guard = lock.write();
        *sheet.contents.write_with(&mut guard) = copy;
        sheet
            .shared_contents
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// The sheet's contents, for the engine's cache of `style` element text, when they can be shared
    /// with other `style` elements of the same text: none when the sheet has `@import` rules (each
    /// import is loaded for its own sheet) or `@scope` rules (the scoping root of those without a
    /// `<scope-start>` is the owner's, and the cascade data built from the contents keeps it).
    pub(crate) fn shareable_contents(
        &self,
        device: &Device,
        guard: &SharedRwLockReadGuard,
    ) -> Option<Arc<StylesheetContents>> {
        let contents = self.0 .0.contents(guard);
        let custom_media = CustomMediaMap::default();
        let blocked = contents
            .iter_rules::<AllRules, _>(device, &custom_media, guard)
            .any(|rule| matches!(rule, CssRule::Import(_) | CssRule::Scope(_)));
        (!blocked).then(|| self.0 .0.contents.read_with(guard).clone())
    }

    /// A new sheet over `contents` (shared with the sheet they came from) with its own `media`.
    pub(crate) fn sharing(
        contents: Arc<StylesheetContents>,
        media: &str,
        lock: &SharedRwLock,
    ) -> Sheet {
        let media = parse_media_list(media, &contents.url_data, contents.quirks_mode);
        Sheet(DocumentStyleSheet(Arc::new(Stylesheet {
            contents: lock.wrap(contents),
            shared_lock: lock.clone(),
            media: Arc::new(lock.wrap(media)),
            disabled: Default::default(),
            implicit_scope_root: Default::default(),
            shared_contents: std::sync::atomic::AtomicBool::new(true),
        })))
    }

    /// The sheet's `@import` rules, in order: the URL each one requested (none for one that
    /// requested nothing: a URL that does not parse or a `supports()` condition that does not
    /// hold) and the sheet it brought, when that had arrived as the sheet was parsed.
    pub fn imports(&self) -> Vec<(Option<Url>, Option<Sheet>)> {
        let sheet = &self.0 .0;
        let guard = sheet.shared_lock.read();
        let contents = sheet.contents.read_with(&guard);
        contents
            .rules(&guard)
            .iter()
            .filter_map(|rule| {
                let CssRule::Import(r) = rule else {
                    return None;
                };
                let r = r.read_with(&guard);
                let enabled = r.supports.as_ref().is_none_or(|s| s.enabled);
                let url = r.url.url().filter(|_| enabled).map(|u| Url::clone(u));
                let child = r
                    .stylesheet
                    .as_sheet()
                    .map(|s| Sheet(DocumentStyleSheet(s.clone())));
                Some((url, child))
            })
            .collect()
    }

    /// Marks the sheet's contents as shared (the cache holds them for other sheets).
    pub(crate) fn mark_shared(&self) {
        self.0
             .0
            .shared_contents
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Where a sheet comes from (CSS Cascade §6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SheetOrigin {
    UserAgent,
    User,
    Author,
}

impl SheetOrigin {
    pub(crate) fn stylo(self) -> Origin {
        match self {
            SheetOrigin::UserAgent => Origin::UserAgent,
            SheetOrigin::User => Origin::User,
            SheetOrigin::Author => Origin::Author,
        }
    }
}

/// An `@import` rule asking for a sheet.
#[derive(Debug)]
pub struct ImportRequest<'a> {
    /// The resolved URL of the import.
    pub url: &'a Url,
    /// The URL of the sheet containing the rule.
    pub parent_url: &'a Url,
    /// How deep the import is (1 for an import from a top-level sheet).
    pub depth: usize,
}

/// The answer to an [`ImportRequest`].
pub enum ImportResponse {
    /// The sheet's text and its final URL (after redirects), parsed right away.
    Css { text: String, url: Url },
    /// No sheet (network error, blocked by policy...). The rule applies nothing.
    Failed,
}

/// Fetches `@import`ed sheets. The page layer implements it over the network stack, with its CSP and
/// mixed-content checks. Called during parsing, synchronously.
pub trait ImportLoader: Send + Sync {
    fn load(&self, request: &ImportRequest<'_>) -> ImportResponse;
}

/// Nested `@import`s deeper than this are refused (a sheet importing itself, or a chain of URLs that
/// grows at each level).
const MAX_IMPORT_DEPTH: usize = 16;

struct Loader<'a> {
    loader: &'a dyn ImportLoader,
    parent_url: &'a Url,
    depth: usize,
    quirks_mode: QuirksMode,
    origin: Origin,
}

impl StylesheetLoader for Loader<'_> {
    fn request_stylesheet(
        &self,
        url: CssUrl,
        location: SourceLocation,
        lock: &SharedRwLock,
        media: Arc<Locked<MediaList>>,
        supports: Option<ImportSupportsCondition>,
        layer: ImportLayer,
    ) -> Arc<Locked<ImportRule>> {
        let refused = || ImportSheet::new_refused();
        let enabled = supports.as_ref().is_none_or(|s| s.enabled);
        let sheet = match url.url() {
            Some(resolved) if enabled && self.depth < MAX_IMPORT_DEPTH => {
                let request = ImportRequest {
                    url: resolved,
                    parent_url: self.parent_url,
                    depth: self.depth + 1,
                };
                match self.loader.load(&request) {
                    ImportResponse::Css {
                        text,
                        url: final_url,
                    } => {
                        let nested = Loader {
                            loader: self.loader,
                            parent_url: &final_url,
                            depth: self.depth + 1,
                            quirks_mode: self.quirks_mode,
                            origin: self.origin,
                        };
                        let sheet = Stylesheet::from_str(
                            &text,
                            UrlExtraData::from(final_url.clone()),
                            self.origin,
                            media,
                            lock.clone(),
                            Some(&nested),
                            None,
                            self.quirks_mode,
                            AllowImportRules::Yes,
                        );
                        ImportSheet::new(Arc::new(sheet))
                    }
                    ImportResponse::Failed => refused(),
                }
            }
            _ => refused(),
        };
        Arc::new(lock.wrap(ImportRule {
            url,
            stylesheet: sheet,
            supports,
            layer,
            source_location: location,
        }))
    }
}

/// Parses a media query list (`<style media>`, `<link media>`; Media Queries 5 §3). An empty string is the
/// empty list, which matches everything.
pub(crate) fn parse_media_list(
    media: &str,
    url: &UrlExtraData,
    quirks_mode: QuirksMode,
) -> MediaList {
    if media.trim().is_empty() {
        return MediaList::empty();
    }
    let mut parser = Parser::new(media);
    let mut context = ParserContext::new(
        Origin::Author,
        url,
        Some(CssRuleType::Media),
        ParsingMode::DEFAULT,
        quirks_mode,
        Default::default(),
        None,
        None,
        Default::default(),
    );
    MediaList::parse(&mut context, &mut parser)
}

/// Parses an author sheet with a lock of its own (a constructed `CSSStyleSheet` of a document that
/// does not render).
pub fn parse_standalone(css: &str, base_url: &Url, media: &str) -> Sheet {
    let lock = SharedRwLock::new();
    constructed(parse_sheet(
        css,
        base_url,
        media,
        SheetOrigin::Author,
        &lock,
        QuirksMode::NoQuirks,
        None,
    ))
}

/// A constructed sheet: the implicit scoping root of its `@scope` rules is the root of the tree that
/// adopts it, the shadow host or the document (CSS Cascade 6 §2.1.1).
fn constructed(sheet: Sheet) -> Sheet {
    sheet.set_implicit_scope_root(Some(ImplicitScopeRoot::Constructed));
    sheet
}

/// Parses an author sheet under the lock of a document's style table (a constructed
/// `CSSStyleSheet` whose constructor document is that document: it can be adopted there, CSSOM
/// §6.4). `@import` rules are not allowed.
pub fn parse_with_lock_of(
    css: &str,
    base_url: &Url,
    media: &str,
    data: &crate::StyleData,
) -> Sheet {
    constructed(parse_sheet(
        css,
        base_url,
        media,
        SheetOrigin::Author,
        data.shared_lock(),
        QuirksMode::NoQuirks,
        None,
    ))
}

/// Parses a stylesheet.
#[allow(clippy::too_many_arguments)]
pub(crate) fn parse_sheet(
    css: &str,
    base_url: &Url,
    media: &str,
    origin: SheetOrigin,
    lock: &SharedRwLock,
    quirks_mode: QuirksMode,
    loader: Option<&dyn ImportLoader>,
) -> Sheet {
    crate::prefs::init();
    let url = UrlExtraData::from(base_url.clone());
    let media = parse_media_list(media, &url, quirks_mode);
    let loader = loader.map(|l| Loader {
        loader: l,
        parent_url: base_url,
        depth: 0,
        quirks_mode,
        origin: origin.stylo(),
    });
    let sheet = Stylesheet::from_str(
        css,
        url,
        origin.stylo(),
        Arc::new(lock.wrap(media)),
        lock.clone(),
        loader.as_ref().map(|l| l as &dyn StylesheetLoader),
        None,
        quirks_mode,
        if loader.is_some() {
            AllowImportRules::Yes
        } else {
            AllowImportRules::No
        },
    );
    Sheet(DocumentStyleSheet(Arc::new(sheet)))
}

/// The URL user-agent sheets are parsed against.
pub(crate) fn ua_url() -> &'static Url {
    static URL: LazyLock<Url> =
        LazyLock::new(|| Url::parse("about:snowlark-ua").expect("valid URL"));
    &URL
}

/// One `@font-face` rule (CSS Fonts 4 §4), with its descriptors serialized.
#[derive(Clone, Debug, PartialEq)]
pub struct FontFace {
    /// `font-family`.
    pub family: String,
    /// `src`, in order.
    pub sources: Vec<FontFaceSource>,
    /// `font-weight`, `font-style`, `font-stretch`, `unicode-range` and `font-display` as CSS text (empty
    /// when absent).
    pub weight: String,
    pub style: String,
    pub stretch: String,
    pub unicode_range: String,
    pub display: String,
}

/// One entry of an `@font-face` `src` list.
#[derive(Clone, Debug, PartialEq)]
pub enum FontFaceSource {
    /// `url(...)` with the resolved URL and the `format()` hint.
    Url { url: String, format: Option<String> },
    /// `local(...)`.
    Local(String),
}

fn css_of<T: ToCss>(value: &Option<T>) -> String {
    value
        .as_ref()
        .map(|v| v.to_css_string())
        .unwrap_or_default()
}

/// The cascade layers of a document in the order of their first appearance (CSS Cascade 5 §6.4.3),
/// as a tree: sublayers are ordered within their parent.
#[derive(Default)]
pub(crate) struct LayerOrder {
    children: Vec<(Option<String>, LayerOrder)>,
}

impl LayerOrder {
    /// The position of the layer `path` in the order (registering it and its ancestors): one index
    /// per level, which compare lexicographically in layer order. An anonymous layer (`None`) is
    /// always a new one.
    fn intern(&mut self, path: &[Option<String>]) -> Vec<usize> {
        let mut node = self;
        let mut out = Vec::with_capacity(path.len());
        for seg in path {
            let found = match seg {
                Some(_) => node.children.iter().position(|(n, _)| n == seg),
                None => None,
            };
            let i = found.unwrap_or_else(|| {
                node.children.push((seg.clone(), LayerOrder::default()));
                node.children.len() - 1
            });
            out.push(i);
            node = &mut node.children[i].1;
        }
        out
    }
}

fn layer_segments(name: &style::stylesheets::layer_rule::LayerName) -> Vec<Option<String>> {
    name.0.iter().map(|a| Some(a.0.to_string())).collect()
}

/// The cascade position of a rule in layer `path`: its layer's position, then past the layer's
/// sublayers (a layer's own rules beat them); unlayered rules are last of all.
fn layer_rank(order: &mut LayerOrder, path: &[Option<String>]) -> Vec<usize> {
    let mut rank = order.intern(path);
    rank.push(usize::MAX);
    rank
}

struct FaceWalk<'a> {
    device: &'a Device,
    guard: &'a SharedRwLockReadGuard<'a>,
    quirks: QuirksMode,
    custom_media: CustomMediaMap,
}

impl FaceWalk<'_> {
    fn effective(&self, rule: &CssRule) -> bool {
        style::stylesheets::EffectiveRules::is_effective(
            self.guard,
            self.device,
            self.quirks,
            &self.custom_media,
            &rule.into(),
        )
    }

    fn walk(
        &self,
        rules: &[CssRule],
        path: &mut Vec<Option<String>>,
        order: &mut LayerOrder,
        out: &mut Vec<(Vec<usize>, FontFace)>,
    ) {
        for rule in rules {
            match rule {
                CssRule::FontFace(r) => {
                    if let Some(face) = face_of(&r.read_with(self.guard).descriptors) {
                        out.push((layer_rank(order, path), face));
                    }
                }
                CssRule::LayerBlock(r) => {
                    let depth = path.len();
                    match &r.name {
                        Some(n) => path.extend(layer_segments(n)),
                        None => path.push(None),
                    }
                    order.intern(path);
                    self.walk(r.rules.read_with(self.guard).0.as_slice(), path, order, out);
                    path.truncate(depth);
                }
                CssRule::LayerStatement(r) => {
                    for n in &r.names {
                        let mut p = path.clone();
                        p.extend(layer_segments(n));
                        order.intern(&p);
                    }
                }
                CssRule::Import(r) => {
                    if !self.effective(rule) {
                        continue;
                    }
                    use style::stylesheets::import_rule::ImportLayer;
                    let depth = path.len();
                    match &r.read_with(self.guard).layer {
                        ImportLayer::None => {}
                        ImportLayer::Anonymous => path.push(None),
                        ImportLayer::Named(n) => path.extend(layer_segments(n)),
                    }
                    order.intern(path);
                    self.walk(rule.children(self.guard), path, order, out);
                    path.truncate(depth);
                }
                CssRule::Media(_)
                | CssRule::Supports(_)
                | CssRule::Document(_)
                | CssRule::Container(_)
                | CssRule::Scope(_)
                | CssRule::StartingStyle(_)
                    if self.effective(rule) =>
                {
                    self.walk(rule.children(self.guard), path, order, out);
                }
                _ => {}
            }
        }
    }
}

fn face_of(d: &style::font_face::Descriptors) -> Option<FontFace> {
    let family = d.font_family.as_ref()?;
    let sources = d
        .src
        .as_ref()
        .map(|list| {
            list.0
                .iter()
                // CSS Fonts 4 §4.3.1: a source needing a font technology the user agent lacks is
                // skipped.
                .filter(|s| match s {
                    Source::Url(u) => {
                        u.tech_flags.is_empty()
                            || style::stylesheets::supports_rule::eval_font_tech(&u.tech_flags)
                    }
                    Source::Local(_) => true,
                })
                .map(|s| match s {
                    Source::Url(u) => FontFaceSource::Url {
                        url: u
                            .url
                            .url()
                            .map(|u| u.as_str().to_owned())
                            .unwrap_or_else(|| u.url.as_str().to_owned()),
                        format: u.format_hint.as_ref().map(|f| f.to_css_string()),
                    },
                    Source::Local(name) => FontFaceSource::Local(name.name.to_string()),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(FontFace {
        family: family.name.to_string(),
        sources,
        weight: css_of(&d.font_weight),
        style: css_of(&d.font_style),
        stretch: css_of(&d.font_width),
        unicode_range: d
            .unicode_range
            .as_ref()
            .map(|r| {
                r.iter()
                    .map(|u| u.to_css_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        display: css_of(&d.font_display),
    })
}

/// The `@font-face` rules that apply to `device` in `sheet` (inside matching `@media`, `@supports` and
/// imported sheets), each with its position in the cascade-layer order `order` (register the
/// document's sheets in cascade order into one `order`, then sort by position).
pub(crate) fn ranked_font_faces(
    sheet: &Stylesheet,
    device: &Device,
    guard: &SharedRwLockReadGuard,
    order: &mut LayerOrder,
) -> Vec<(Vec<usize>, FontFace)> {
    let contents = sheet.contents(guard);
    let walk = FaceWalk {
        device,
        guard,
        quirks: contents.quirks_mode,
        custom_media: CustomMediaMap::default(),
    };
    let mut out = Vec::new();
    walk.walk(contents.rules(guard), &mut Vec::new(), order, &mut out);
    out
}

/// The `@font-face` rules that apply to `device` in `sheet`, in source order.
pub(crate) fn font_faces(
    sheet: &Stylesheet,
    device: &Device,
    guard: &SharedRwLockReadGuard,
) -> Vec<FontFace> {
    ranked_font_faces(sheet, device, guard, &mut LayerOrder::default())
        .into_iter()
        .map(|(_, f)| f)
        .collect()
}

/// Parses declarations as the body of an `@font-face` rule (what `new FontFace(...)` and the
/// descriptor setters of CSS Font Loading 3 do): the face with the descriptors that parsed, or
/// `None` when the rule is invalid (no usable `font-family`).
pub fn parse_font_face(declarations: &str, base_url: &Url) -> Option<FontFace> {
    let sheet = parse_standalone(&format!("@font-face {{ {declarations} }}"), base_url, "");
    let guard = sheet.stylo().shared_lock.read();
    for rule in sheet.stylo().contents(&guard).rules(&guard) {
        if let CssRule::FontFace(r) = rule {
            return face_of(&r.read_with(&guard).descriptors);
        }
    }
    None
}

/// The serialized form of descriptor `name: value` of an `@font-face` rule (as `FontFace`'s
/// attributes serialize them), or `None` when `value` is not valid for it.
pub fn font_face_descriptor(name: &str, value: &str, base_url: &Url) -> Option<String> {
    // The tested descriptor comes last so that it replaces a `font-family` or `src` of the frame.
    let family = if name == "font-family" {
        ""
    } else {
        "font-family: a; "
    };
    let src = if name == "src" { "" } else { "src: local(a); " };
    let css = format!("@font-face {{ {family}{src}{name}: {value} }}");
    let sheet = parse_standalone(&css, base_url, "");
    let guard = sheet.stylo().shared_lock.read();
    for rule in sheet.stylo().contents(&guard).rules(&guard) {
        if let CssRule::FontFace(r) = rule {
            let text =
                style::shared_lock::ToCssWithGuard::to_css_string(r.read_with(&guard), &guard);
            let body = text.split_once('{')?.1.rsplit_once('}')?.0;
            // Declarations are separated by `;` outside strings and `url()`s.
            let mut found = None;
            let (mut depth, mut quote, mut start) = (0i32, None, 0usize);
            let bytes = body.as_bytes();
            for (i, &b) in bytes.iter().enumerate() {
                match (quote, b) {
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), _) => {}
                    (None, b'"' | b'\'') => quote = Some(b),
                    (None, b'(') => depth += 1,
                    (None, b')') => depth -= 1,
                    (None, b';') if depth == 0 => {
                        let decl = body[start..i].trim();
                        if let Some((n, v)) = decl.split_once(':') {
                            if n.trim() == name {
                                found = Some(v.trim().to_owned());
                            }
                        }
                        start = i + 1;
                    }
                    _ => {}
                }
            }
            let tail = body[start..].trim();
            if let Some((n, v)) = tail.split_once(':') {
                if n.trim() == name {
                    found = Some(v.trim().to_owned());
                }
            }
            return found;
        }
    }
    None
}

/// A handler for the `@font-face` rules of each sheet added to an engine.
pub type FontFaceHandler = StdArc<dyn Fn(&FontFace) + Send + Sync>;
