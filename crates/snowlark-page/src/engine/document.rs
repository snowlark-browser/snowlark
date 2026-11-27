// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `Document` (DOM §4.5 and HTML §3.1), `DocumentOrShadowRoot`, `DOMImplementation`, `XMLDocument`,
//! `HTMLDocument`, `DOMParser` and `XMLSerializer`.

use snowlark_bindings::generated::{
    BooleanOrImportNodeOptions, DOMImplementationImpl, DOMParserImpl, DOMParserSupportedType,
    DocumentImpl, DocumentOrShadowRootImpl, DocumentReadyState, DocumentVisibilityState,
    HTMLDocumentImpl, NodeFilter, StringOrElementCreationOptions, TrustedHTMLOrString,
    XMLDocumentImpl, XMLSerializerImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsPromise};
use snowlark_dom::{local_name, ns, DocumentKind, NodeId, QuirksMode};

use super::collections::Filter;
use super::element::rejected;
use super::{exception, node_handle, DomResultExt, Engine, Object};

/// The loading state of a document the engine keeps beside the DOM.
#[derive(Clone, Debug)]
pub struct DocState {
    pub ready_state: DocumentReadyState,
    pub referrer: String,
    /// HTML `Document`'s "opener policy" (cross-origin opener policy), from its response or, for
    /// a popup's initial `about:blank`, its creator's top-level document.
    pub opener_policy: snowlark_net::coop::OpenerPolicy,
    pub last_modified: String,
    /// HTML "ignore-destructive-writes counter".
    pub ignore_destructive_writes: u32,
    /// HTML "throw-on-dynamic-markup-insertion counter".
    pub throw_on_dynamic_markup_insertion: u32,
    /// The document's focused area (an element), if any.
    pub focused: Option<NodeId>,
    /// HTML §6.6.5 the "sequential focus navigation starting point": where focus last moved or
    /// the user last clicked; Tab moves on from it while nothing is focused.
    pub nav_start: Option<NodeId>,
    /// HTML §6.6.4 a top-level document's "autofocus candidates", in order.
    pub autofocus_candidates: Vec<NodeId>,
    /// HTML §6.6.4 a top-level document's "autofocus processed flag".
    pub autofocus_processed: bool,
    /// HTML §7.4.6.4 the document's "target element" ("scroll to the fragment" sets it; `:target`
    /// matches it). It stays when the element is removed.
    pub target_element: Option<NodeId>,
    /// The `title` of a document created by `createHTMLDocument` etc. has no browsing context.
    pub has_browsing_context: bool,
    /// HTML "page showing": `pageshow` fired and `pagehide` not yet.
    pub page_showing: bool,
    /// HTML "unload counter": `beforeunload`, `pagehide` or `unload` is being fired at it.
    pub unload_counter: u32,
    /// HTML "active parser was aborted": its parser was aborted (`window.stop()`), after which
    /// `document.open` and `document.write` do nothing.
    pub active_parser_aborted: bool,
    /// HTML §6.8.2 design mode.
    pub design_mode: bool,
    /// HTML Editing "the default single-line container name" is `p` (else `div`).
    pub paragraph_separator_p: bool,
    /// HTML Editing "the CSS styling flag".
    pub css_styling: bool,
    /// HTML Editing the state and value overrides of the inline formatting commands.
    pub overrides: crate::exec_command::Overrides,
    /// The inline formatting a deletion removed at the caret it left (outermost first: local
    /// name and attributes), which the next text inserted there takes (as browsers keep a
    /// typing style).
    pub typing_style: Option<(
        crate::engine::ranges::Bp,
        Vec<(String, Vec<(String, String)>)>,
    )>,
    /// HTML "document load timing info".
    pub load_timing: LoadTiming,
    /// `document.all`, once made.
    pub all: Option<Handle>,
    /// HTML "will declaratively refresh": the refresh a `meta` or the `Refresh` header gave it.
    pub declarative_refresh: Option<crate::refresh::DeclarativeRefresh>,
    /// HTML `Document`'s "about base URL" (an iframe `srcdoc` document's: its parent's base URL).
    pub about_base_url: Option<url::Url>,
}

/// HTML §7.4.1 "document load timing info": the times "the end" and "update the current document
/// readiness" record, in milliseconds since the time origin; 0 until reached.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadTiming {
    /// Navigation Timing L1 `domLoading`: when the document was made.
    pub dom_loading: f64,
    /// The document came from a reload (Navigation Timing's navigation type).
    pub reload: bool,
    pub dom_interactive: f64,
    pub dom_content_loaded_event_start: f64,
    pub dom_content_loaded_event_end: f64,
    pub dom_complete: f64,
    pub load_event_start: f64,
    pub load_event_end: f64,
}

impl LoadTiming {
    /// The time behind a `PerformanceTiming` attribute name, for the ones the document load timing
    /// info records.
    pub fn get(&self, name: &str) -> Option<f64> {
        Some(match name {
            "domInteractive" => self.dom_interactive,
            "domContentLoadedEventStart" => self.dom_content_loaded_event_start,
            "domContentLoadedEventEnd" => self.dom_content_loaded_event_end,
            "domComplete" => self.dom_complete,
            "loadEventStart" => self.load_event_start,
            "loadEventEnd" => self.load_event_end,
            _ => return None,
        })
    }
}

impl Default for DocState {
    fn default() -> Self {
        DocState {
            ready_state: DocumentReadyState::Complete,
            referrer: String::new(),
            opener_policy: Default::default(),
            last_modified: String::new(),
            ignore_destructive_writes: 0,
            throw_on_dynamic_markup_insertion: 0,
            focused: None,
            nav_start: None,
            autofocus_candidates: Vec::new(),
            autofocus_processed: false,
            target_element: None,
            has_browsing_context: false,
            page_showing: false,
            unload_counter: 0,
            active_parser_aborted: false,
            design_mode: false,
            paragraph_separator_p: false,
            css_styling: false,
            overrides: Default::default(),
            typing_style: None,
            load_timing: LoadTiming::default(),
            all: None,
            declarative_refresh: None,
            about_base_url: None,
        }
    }
}

/// HTML "strip and collapse ASCII whitespace".
pub(crate) fn strip_and_collapse(s: &str) -> String {
    s.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

impl Engine {
    pub(crate) fn doc_state<R>(&self, doc: NodeId, f: impl FnOnce(&mut DocState) -> R) -> R {
        let mut html = self.html.borrow_mut();
        f(html.docs.entry(doc).or_default())
    }

    /// The document's URL, parsed (`None` when it does not parse). The last one parsed is kept
    /// with the string it came from, so that reads of an unchanged URL (`location`'s getters, the
    /// base URL of a document without `base`) parse it once.
    pub(crate) fn parsed_document_url(
        &self,
        dom: &snowlark_dom::Dom,
        doc: NodeId,
    ) -> Option<url::Url> {
        let s = dom.document_url(doc);
        let mut cache = self.document_url_cache.borrow_mut();
        if let Some((d, from, url)) = &*cache {
            if *d == doc && from == s {
                return Some(url.clone());
            }
        }
        let url = url::Url::parse(s).ok()?;
        *cache = Some((doc, s.to_owned(), url.clone()));
        Some(url)
    }

    /// HTML §2.4.1 the document's "fallback base URL": the about base URL of an iframe `srcdoc`
    /// document, or of an `about:blank` one that has one (its creator's base URL when it was
    /// made, kept by a clone), else the document's URL.
    pub(crate) fn fallback_base_url(&self, dom: &snowlark_dom::Dom, doc: NodeId) -> url::Url {
        let url = dom.document_url(doc);
        if url == "about:srcdoc" || url == "about:blank" {
            let about = self
                .html
                .borrow()
                .docs
                .get(&doc)
                .and_then(|s| s.about_base_url.clone());
            if let Some(u) = about {
                return u;
            }
        }
        self.parsed_document_url(dom, doc)
            .unwrap_or_else(|| url::Url::parse("about:blank").unwrap())
    }

    /// The document base URL (HTML §2.4.1): the frozen base URL of the first `base` element with an
    /// `href`, else the fallback base URL (the document's URL).
    pub(crate) fn document_base_url(&self, dom: &snowlark_dom::Dom, doc: NodeId) -> url::Url {
        let fallback = || self.fallback_base_url(dom, doc);
        if !dom.has_connected_base(doc) {
            return fallback();
        }
        let base = dom.descendants(doc).find(|&d| {
            dom.is_html_element_named(d, &local_name!("base"))
                && dom.attr(d, &ns!(), &local_name!("href")).is_some()
        });
        match base {
            Some(b) => {
                let href = dom.attr(b, &ns!(), &local_name!("href")).unwrap_or("");
                let url = fallback().join(href).unwrap_or_else(|_| fallback());
                // ("set the frozen base URL": a `data:` or `javascript:` URL is not one)
                if matches!(url.scheme(), "data" | "javascript") {
                    return fallback();
                }
                if self.policy.borrow().any && self.base_uri_blocked(doc, &url) {
                    return fallback();
                }
                url
            }
            None => fallback(),
        }
    }

    /// HTML §4.2.5.3 the content language state of `http-equiv`: a `meta` inserted into a document
    /// (in its document tree) whose `content` has no comma sets the document's pragma-set default
    /// language to its first token. Removing the `meta` leaves it.
    pub(crate) fn meta_content_language_inserted(&self, el: NodeId) {
        let (doc, language) = {
            let dom = self.dom();
            let doc = dom.node_document(el);
            if dom.root(el) != doc
                || !dom
                    .attr(el, &ns!(), &local_name!("http-equiv"))
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("content-language"))
            {
                return;
            }
            let Some(content) = dom.attr(el, &ns!(), &local_name!("content")) else {
                return;
            };
            if content.contains(',') {
                return;
            }
            let ws = |c: char| matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ');
            let candidate = content
                .trim_start_matches(ws)
                .split(ws)
                .next()
                .unwrap_or("");
            if candidate.is_empty() {
                return;
            }
            (doc, candidate.to_owned())
        };
        self.dom_mut().set_pragma_language(doc, &language);
        // (`:lang()` and every language-dependent style of the document may change)
        self.with_doc_renderer(doc, |r, st| {
            r.stylesheets_changed();
            st.dirty = true;
        });
    }

    /// The document of a node handle that names a document.
    fn d(&self, h: Handle) -> NodeId {
        self.n(h)
    }

    /// `document.title` (HTML §3.1.3).
    pub(crate) fn document_title(&self, doc: NodeId) -> String {
        let dom = self.dom();
        let title = dom
            .descendants(doc)
            .find(|&d| dom.is_html_element_named(d, &local_name!("title")));
        match title {
            Some(t) => strip_and_collapse(&dom.child_text_content(t)),
            None => String::new(),
        }
    }

    fn body_color(&self, doc: NodeId, attr: &str) -> DomStr<'_> {
        let body = self.dom().body(doc);
        match body.filter(|&b| self.dom().is_html_element_named(b, &local_name!("body"))) {
            Some(b) => self.attr_str(b, attr).unwrap_or(DomStr::Borrowed("")),
            None => DomStr::Borrowed(""),
        }
    }

    fn set_body_color(&self, cx: &mut Cx, doc: NodeId, attr: &str, value: &str) -> rt::Result<()> {
        let body = self.dom().body(doc);
        if let Some(b) = body.filter(|&b| self.dom().is_html_element_named(b, &local_name!("body")))
        {
            self.set_attr(cx, b, attr, value);
        }
        Ok(())
    }

    /// `document.createEvent(interface)` (DOM §4.5).
    fn create_event_steps(&self, interface: &str) -> rt::Result<Handle> {
        use snowlark_bindings::InterfaceId as I;
        let i = match interface.to_ascii_lowercase().as_str() {
            "beforeunloadevent" => I::BeforeUnloadEvent,
            "compositionevent" => I::CompositionEvent,
            "customevent" => I::CustomEvent,
            "dragevent" => I::DragEvent,
            "event" | "events" | "htmlevents" | "svgevents" => I::Event,
            "focusevent" => I::FocusEvent,
            "hashchangeevent" => I::HashChangeEvent,
            "keyboardevent" => I::KeyboardEvent,
            "mouseevent" | "mouseevents" => I::MouseEvent,
            "textevent" => I::TextEvent,
            // (Touch Events: a legacy touch event API, so only with a touch device)
            "touchevent" if self.legacy_touch_apis() => I::TouchEvent,
            "uievent" | "uievents" => I::UIEvent,
            _ => {
                return Err(exception(
                    ExceptionName::NotSupportedError,
                    "The event interface is not supported",
                ))
            }
        };
        Ok(self.create_uninitialized_event(i))
    }
}

impl Engine {
    /// HTML §3.1.2 `document.cookie`: a document with an opaque origin (a sandbox's) throws.
    fn check_cookie_origin(&self, doc: NodeId) -> rt::Result<()> {
        if self.sandboxed_origin(doc).is_some() {
            return Err(exception(
                ExceptionName::SecurityError,
                "The document is sandboxed",
            ));
        }
        Ok(())
    }

    /// Setting `document.title` of `doc` (HTML §3.1.3).
    pub(crate) fn set_document_title(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        value: &str,
    ) -> rt::Result<()> {
        let (title, head) = {
            let dom = self.dom();
            let Some(root) = dom.document_element(doc) else {
                return Ok(());
            };
            if dom.namespace(root) != &ns!(html) {
                return Ok(());
            }
            let title = dom
                .descendants(doc)
                .find(|&d| dom.is_html_element_named(d, &local_name!("title")));
            (title, dom.head(doc))
        };
        let title = match title {
            Some(t) => t,
            None => {
                let Some(head) = head else {
                    return Ok(());
                };
                let t = self
                    .dom_mut()
                    .create_html_element(doc, local_name!("title"));
                self.dom_mut().append_child(head, t).js()?;
                t
            }
        };
        self.dom_mut().string_replace_all(value, title);
        self.after_mutation(cx);
        Ok(())
    }
}

impl DocumentImpl for Engine {
    /// HTML §16.3 `document.all`: the document's HTMLAllCollection rooted at it.
    fn all(&self, this: Handle) -> Handle {
        self.document_all(self.d(this))
    }
    fn start_view_transition(
        &self,
        cx: &mut Cx,
        this: Handle,
        callback_options: snowlark_bindings::generated::ViewTransitionUpdateCallbackOrStartViewTransitionOptions,
    ) -> rt::Result<Handle> {
        Engine::start_view_transition(self, cx, self.d(this), callback_options)
    }
    fn active_view_transition(&self, this: Handle) -> Option<Handle> {
        Engine::active_view_transition(self, self.d(this))
    }

    /// Web Animations 1 §6.9 `timeline`: the default document timeline.
    fn timeline(&self, this: Handle) -> Handle {
        self.document_timeline(self.d(this))
    }
    fn implementation(&self, this: Handle) -> Handle {
        self.alloc(Object::DomImplementation {
            document: self.d(this),
        })
    }

    fn url(&self, this: Handle) -> DomStr<'_> {
        let doc = self.d(this);
        self.dom_str(|d| d.document_url(doc))
    }

    fn document_uri(&self, this: Handle) -> DomStr<'_> {
        self.url(this)
    }

    fn compat_mode(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(match self.dom().quirks_mode(self.d(this)) {
            QuirksMode::Quirks => "BackCompat",
            _ => "CSS1Compat",
        })
    }

    fn character_set(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(self.dom().encoding(self.d(this)).name())
    }

    fn charset(&self, this: Handle) -> DomStr<'_> {
        self.character_set(this)
    }

    fn input_encoding(&self, this: Handle) -> DomStr<'_> {
        self.character_set(this)
    }

    fn content_type(&self, this: Handle) -> DomStr<'_> {
        let doc = self.d(this);
        self.dom_str(|d| d.content_type(doc))
    }

    fn doctype(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().document_doctype(self.d(this)))
    }

    fn document_element(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().document_element(self.d(this)))
    }

    fn scrolling_element(&self, this: Handle) -> Option<Handle> {
        let doc = self.d(this);
        let dom = self.dom();
        if dom.quirks_mode(doc) == QuirksMode::Quirks {
            let body = dom
                .body(doc)
                .filter(|&b| dom.is_html_element_named(b, &local_name!("body")));
            drop(dom);
            // CSSOM View §5: null when the body is potentially scrollable.
            if body.is_some_and(|b| self.body_potentially_scrollable(b)) {
                return None;
            }
            self.opt_node(body)
        } else {
            self.opt_node(dom.document_element(doc))
        }
    }

    fn fullscreen_enabled(&self, this: Handle) -> bool {
        self.fullscreen_enabled_for(self.d(this))
    }

    fn fullscreen(&self, this: Handle) -> bool {
        self.fullscreen_element_of(self.d(this)).is_some()
    }

    fn location(&self, this: Handle) -> Option<Handle> {
        self.window_of_document(self.d(this))
            .filter(|_| self.is_active_document(self.d(this)))
            .map(|w| self.location_handle(w))
    }

    fn domain(&self, cx: &mut Cx, this: Handle) -> rt::Result<DomStr<'_>> {
        let mut doc = self.d(this);
        // (`new Document()`: no URL of its own, and the origin of the global that made it)
        if self.window_of_document(doc).is_none() && self.dom().document_url(doc) == "about:blank" {
            doc = self.current_document(cx)?;
        }
        // HTML §7.5.2 step 2: an opaque origin's domain is the empty string.
        if self.sandboxed_origin(doc).is_some() {
            return Ok(DomStr::Borrowed(""));
        }
        if let Some(domain) = self.document_domain(doc) {
            return Ok(DomStr::Owned(domain));
        }
        // (an `about:` document has its creator's origin)
        let url = self.effective_document_url(doc);
        Ok(DomStr::Owned(
            url.host_str().map(str::to_owned).unwrap_or_default(),
        ))
    }

    fn set_domain(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        // HTML §7.5.2 `document.domain`'s setter. (No origin-keyed agent clusters yet: step 6 never
        // returns early.)
        let doc = self.d(this);
        if self.window_of_document(doc).is_none() {
            return Err(exception(
                ExceptionName::SecurityError,
                "The document has no browsing context",
            ));
        }
        if self.document_domain_sandboxed(doc) {
            return Err(exception(
                ExceptionName::SecurityError,
                "The document is sandboxed",
            ));
        }
        let current = self.domain(cx, this)?.into_owned();
        if current.is_empty() {
            return Err(exception(
                ExceptionName::SecurityError,
                "The document has no host",
            ));
        }
        let suffix = url::Host::parse(value).ok();
        let allowed = match (&suffix, url::Host::parse(&current)) {
            (Some(s), Ok(c)) => {
                snowlark_net::origin::is_registrable_domain_suffix_of_or_equal(s, &c)
            }
            _ => false,
        };
        let Some(domain) = suffix.filter(|_| allowed) else {
            return Err(exception(
                ExceptionName::SecurityError,
                "The domain is not a suffix of the current host",
            ));
        };
        self.set_document_domain(cx, doc, domain.to_string());
        Ok(())
    }

    fn referrer(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.doc_state(self.d(this), |s| s.referrer.clone()))
    }

    fn cookie(&self, _cx: &mut Cx, this: Handle) -> rt::Result<DomStr<'_>> {
        self.check_cookie_origin(self.d(this))?;
        Ok(DomStr::Owned(self.document_cookie(self.d(this))))
    }

    fn set_cookie(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.check_cookie_origin(self.d(this))?;
        self.set_document_cookie(self.d(this), value);
        Ok(())
    }

    fn last_modified(&self, this: Handle) -> DomStr<'_> {
        let s = self.doc_state(self.d(this), |s| s.last_modified.clone());
        if s.is_empty() {
            return DomStr::Owned(super::window::last_modified_now());
        }
        DomStr::Owned(s)
    }

    fn ready_state(&self, this: Handle) -> DocumentReadyState {
        self.doc_state(self.d(this), |s| s.ready_state)
    }

    /// HTML §3.1.5 "dom-document-nameditem": the iframe's WindowProxy, the element, or a collection.
    fn named_getter(&self, this: Handle, name: &str) -> Option<Handle> {
        let doc = self.d(this);
        // (every named element has the name as its id or its name attribute)
        if !self.dom().may_have_id_or_name(doc, name) {
            return None;
        }
        let found: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(doc)
                .filter(|&d| {
                    dom.is_element(d) && super::collections::is_document_named(&dom, d, name)
                })
                .take(2)
                .collect()
        };
        match found.as_slice() {
            [] => None,
            [one] => {
                if self
                    .dom()
                    .is_html_element_named(*one, &local_name!("iframe"))
                {
                    if let Some(w) = self.content_window(*one) {
                        return Some(w);
                    }
                }
                Some(node_handle(*one))
            }
            _ => Some(self.collection(
                doc,
                super::collections::Filter::DocumentNamed(name.to_owned()),
            )),
        }
    }

    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        let doc = self.d(this);
        let dom = self.dom();
        let mut out: Vec<String> = Vec::new();
        for d in dom.descendants(doc) {
            if !dom.is_element(d) || dom.namespace(d) != &ns!(html) {
                continue;
            }
            for attr in [local_name!("name"), local_name!("id")] {
                if let Some(v) = dom.attr(d, &ns!(), &attr) {
                    if super::collections::is_document_named(&dom, d, v)
                        && !out.iter().any(|o| o == v)
                    {
                        out.push(v.to_owned());
                    }
                }
            }
        }
        out
    }

    fn title(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.document_title(self.d(this)))
    }

    fn set_title(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_document_title(cx, self.d(this), value)
    }

    fn dir(&self, this: Handle) -> DomStr<'_> {
        let root = self.dom().document_element(self.d(this));
        match root.filter(|&r| self.dom().is_html_element_named(r, &local_name!("html"))) {
            Some(r) => {
                let v = self.attr_str(r, "dir");
                DomStr::Borrowed(match v.as_deref().map(str::to_ascii_lowercase).as_deref() {
                    Some("ltr") => "ltr",
                    Some("rtl") => "rtl",
                    Some("auto") => "auto",
                    _ => "",
                })
            }
            None => DomStr::Borrowed(""),
        }
    }

    fn set_dir(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let root = self.dom().document_element(self.d(this));
        if let Some(r) = root.filter(|&r| self.dom().is_html_element_named(r, &local_name!("html")))
        {
            self.set_attr(cx, r, "dir", value);
        }
        Ok(())
    }

    fn body(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().body(self.d(this)))
    }

    /// HTML §3.1.3 `document.body` setter.
    fn set_body(&self, cx: &mut Cx, this: Handle, value: Option<Handle>) -> rt::Result<()> {
        let doc = self.d(this);
        let new = value.map(|v| self.n(v));
        let hierarchy = || {
            exception(
                ExceptionName::HierarchyRequestError,
                "Not a body or frameset",
            )
        };
        let Some(new) = new else {
            return Err(hierarchy());
        };
        {
            let dom = self.dom();
            if !dom.is_html_element_named(new, &local_name!("body"))
                && !dom.is_html_element_named(new, &local_name!("frameset"))
            {
                return Err(hierarchy());
            }
        }
        let (old, root) = {
            let dom = self.dom();
            (dom.body(doc), dom.document_element(doc))
        };
        if old == Some(new) {
            return Ok(());
        }
        let r = match (old, root) {
            (Some(old), _) => {
                let parent = self.dom().parent(old).expect("body has a parent");
                self.dom_mut().replace_child(parent, new, old).map(drop)
            }
            (None, None) => Err(snowlark_dom::DomError::HierarchyRequest),
            (None, Some(root)) => self.dom_mut().append_child(root, new).map(drop),
        }
        .js();
        self.after_mutation(cx);
        r
    }

    fn head(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().head(self.d(this)))
    }

    fn images(&self, this: Handle) -> Handle {
        self.collection(self.d(this), Filter::Images)
    }
    fn embeds(&self, this: Handle) -> Handle {
        self.collection(self.d(this), Filter::Embeds)
    }
    fn plugins(&self, this: Handle) -> Handle {
        self.collection(self.d(this), Filter::Embeds)
    }
    fn links(&self, this: Handle) -> Handle {
        self.collection(self.d(this), Filter::Links)
    }
    fn forms(&self, this: Handle) -> Handle {
        self.collection(self.d(this), Filter::Forms)
    }
    fn scripts(&self, this: Handle) -> Handle {
        self.collection(self.d(this), Filter::Scripts)
    }

    fn default_view(&self, this: Handle) -> Option<Handle> {
        if self.is_active_document(self.d(this)) {
            self.window_of_document(self.d(this))
        } else {
            None
        }
    }

    /// HTML §6.8.2 `designMode`.
    fn design_mode(&self, this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(if self.doc_state(self.d(this), |s| s.design_mode) {
            "on"
        } else {
            "off"
        })
    }
    fn set_design_mode(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let doc = self.d(this);
        let on = value.eq_ignore_ascii_case("on");
        if !on && !value.eq_ignore_ascii_case("off") {
            return Ok(());
        }
        let was = self.doc_state(doc, |s| std::mem::replace(&mut s.design_mode, on));
        if on && !was {
            // "Reset the selection to the start of the document element."
            if let Some(root) = self.dom().document_element(doc) {
                self.set_doc_selection(doc, Some(((root, 0), (root, 0))));
            }
        }
        Ok(())
    }
    fn get_selection(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        Ok(self.selection_of(self.d(this)))
    }
    fn hidden(&self, this: Handle) -> bool {
        !self.document_visible(self.d(this))
    }
    fn visibility_state(&self, this: Handle) -> DocumentVisibilityState {
        if self.document_visible(self.d(this)) {
            DocumentVisibilityState::Visible
        } else {
            DocumentVisibilityState::Hidden
        }
    }

    fn fg_color(&self, this: Handle) -> DomStr<'_> {
        self.body_color(self.d(this), "text")
    }
    fn set_fg_color(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_body_color(cx, self.d(this), "text", value)
    }
    fn link_color(&self, this: Handle) -> DomStr<'_> {
        self.body_color(self.d(this), "link")
    }
    fn set_link_color(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_body_color(cx, self.d(this), "link", value)
    }
    fn vlink_color(&self, this: Handle) -> DomStr<'_> {
        self.body_color(self.d(this), "vlink")
    }
    fn set_vlink_color(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_body_color(cx, self.d(this), "vlink", value)
    }
    fn alink_color(&self, this: Handle) -> DomStr<'_> {
        self.body_color(self.d(this), "alink")
    }
    fn set_alink_color(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_body_color(cx, self.d(this), "alink", value)
    }
    fn bg_color(&self, this: Handle) -> DomStr<'_> {
        self.body_color(self.d(this), "bgcolor")
    }
    fn set_bg_color(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_body_color(cx, self.d(this), "bgcolor", value)
    }

    fn anchors(&self, this: Handle) -> Handle {
        self.collection(self.d(this), Filter::Anchors)
    }
    fn applets(&self, this: Handle) -> Handle {
        self.collection(self.d(this), Filter::Applets)
    }
    fn was_discarded(&self, _this: Handle) -> bool {
        false
    }
    fn picture_in_picture_enabled(&self, _this: Handle) -> bool {
        false
    }
    fn prerendering(&self, _this: Handle) -> bool {
        false
    }

    fn get_elements_by_tag_name(
        &self,
        _cx: &mut Cx,
        this: Handle,
        qualified_name: &str,
    ) -> rt::Result<Handle> {
        Ok(self.tag_collection(self.d(this), qualified_name))
    }

    fn get_elements_by_tag_name_ns(
        &self,
        _cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        local_name: &str,
    ) -> rt::Result<Handle> {
        Ok(self.tag_ns_collection(self.d(this), namespace, local_name))
    }

    fn get_elements_by_class_name(
        &self,
        _cx: &mut Cx,
        this: Handle,
        class_names: &str,
    ) -> rt::Result<Handle> {
        Ok(self.class_collection(self.d(this), class_names))
    }

    fn create_element(
        &self,
        cx: &mut Cx,
        this: Handle,
        local_name: &str,
        options: StringOrElementCreationOptions,
    ) -> rt::Result<Handle> {
        let is = match options {
            StringOrElementCreationOptions::ElementCreationOptions(o) => o.is,
            StringOrElementCreationOptions::String(_) => None,
        };
        self.create_element_api(cx, self.d(this), None, local_name, is, false)
            .map(node_handle)
    }

    fn create_element_ns(
        &self,
        cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        qualified_name: &str,
        options: StringOrElementCreationOptions,
    ) -> rt::Result<Handle> {
        let is = match options {
            StringOrElementCreationOptions::ElementCreationOptions(o) => o.is,
            StringOrElementCreationOptions::String(_) => None,
        };
        self.create_element_api(cx, self.d(this), Some(namespace), qualified_name, is, true)
            .map(node_handle)
    }

    fn create_document_fragment(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(node_handle(
            self.dom_mut().create_document_fragment(self.d(this)),
        ))
    }

    fn create_text_node(&self, _cx: &mut Cx, this: Handle, data: &str) -> rt::Result<Handle> {
        Ok(node_handle(
            self.dom_mut().create_text_node(self.d(this), data),
        ))
    }

    fn create_cdata_section(&self, _cx: &mut Cx, this: Handle, data: &str) -> rt::Result<Handle> {
        self.dom_mut()
            .create_cdata_section(self.d(this), data)
            .js()
            .map(node_handle)
    }

    fn create_comment(&self, _cx: &mut Cx, this: Handle, data: &str) -> rt::Result<Handle> {
        Ok(node_handle(
            self.dom_mut().create_comment(self.d(this), data),
        ))
    }

    fn create_processing_instruction(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: &str,
        data: &str,
    ) -> rt::Result<Handle> {
        self.dom_mut()
            .create_processing_instruction(self.d(this), target, data)
            .js()
            .map(node_handle)
    }

    fn import_node(
        &self,
        cx: &mut Cx,
        this: Handle,
        node: Handle,
        options: BooleanOrImportNodeOptions,
    ) -> rt::Result<Handle> {
        let subtree = match options {
            BooleanOrImportNodeOptions::Boolean(b) => b,
            BooleanOrImportNodeOptions::ImportNodeOptions(o) => !o.self_only,
        };
        let node = self.n(node);
        {
            let dom = self.dom();
            if dom.is_document(node) || dom.is_shadow_root(node) {
                return Err(exception(
                    ExceptionName::NotSupportedError,
                    "Documents and shadow roots cannot be imported",
                ));
            }
        }
        self.clone_with_steps(cx, node, Some(self.d(this)), subtree)
            .map(node_handle)
    }

    fn adopt_node(&self, cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<Handle> {
        let r = self.dom_mut().adopt_node(self.d(this), self.n(node)).js();
        self.after_mutation(cx);
        r.map(node_handle)
    }

    fn create_attribute(&self, _cx: &mut Cx, this: Handle, local_name: &str) -> rt::Result<Handle> {
        self.dom_mut()
            .create_attribute(self.d(this), local_name)
            .js()
            .map(node_handle)
    }

    fn create_attribute_ns(
        &self,
        _cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        qualified_name: &str,
    ) -> rt::Result<Handle> {
        self.dom_mut()
            .create_attribute_ns(self.d(this), namespace, qualified_name)
            .js()
            .map(node_handle)
    }

    fn create_event(&self, _cx: &mut Cx, _this: Handle, interface: &str) -> rt::Result<Handle> {
        self.create_event_steps(interface)
    }

    fn create_range(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        Ok(self.new_range(self.d(this)))
    }

    fn create_node_iterator(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        root: Handle,
        what_to_show: u32,
        filter: Option<NodeFilter>,
    ) -> rt::Result<Handle> {
        Ok(self.new_node_iterator(self.n(root), what_to_show, filter))
    }

    fn create_tree_walker(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        root: Handle,
        what_to_show: u32,
        filter: Option<NodeFilter>,
    ) -> rt::Result<Handle> {
        Ok(self.new_tree_walker(self.n(root), what_to_show, filter))
    }

    fn exit_fullscreen(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        self.exit_fullscreen(self.d(this), Some(p.clone()), cx);
        Ok(p)
    }

    fn get_elements_by_name(
        &self,
        _cx: &mut Cx,
        this: Handle,
        element_name: &str,
    ) -> rt::Result<Handle> {
        Ok(self.collection(self.d(this), Filter::ByName(element_name.to_owned())))
    }

    fn open_unused1_unused2(
        &self,
        cx: &mut Cx,
        this: Handle,
        _unused1: Option<&str>,
        _unused2: Option<&str>,
    ) -> rt::Result<Handle> {
        self.document_open(cx, self.d(this))?;
        Ok(this)
    }

    /// HTML `document.open(url, name, features)`: a fully active document's window's
    /// `open(url, name, features)`.
    fn open_url_name_features(
        &self,
        cx: &mut Cx,
        this: Handle,
        url: &str,
        name: &str,
        features: &str,
    ) -> rt::Result<Option<Handle>> {
        let doc = self.d(this);
        let window = self
            .is_fully_active(doc)
            .then(|| self.window_of_document(doc))
            .flatten()
            .ok_or_else(|| {
                exception(
                    ExceptionName::InvalidAccessError,
                    "The document is not fully active",
                )
            })?;
        snowlark_bindings::generated::window::WindowImpl::open(
            self, cx, window, url, name, features,
        )
    }

    fn close(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.document_close(cx, self.d(this))
    }

    fn write(&self, cx: &mut Cx, this: Handle, text: Vec<TrustedHTMLOrString>) -> rt::Result<()> {
        let s: String = text
            .into_iter()
            .map(|t| self.tt_html(cx, t, self.d(this), "Document write"))
            .collect::<rt::Result<String>>()?;
        self.document_write(cx, self.d(this), &s)
    }

    fn writeln(&self, cx: &mut Cx, this: Handle, text: Vec<TrustedHTMLOrString>) -> rt::Result<()> {
        let mut s: String = text
            .into_iter()
            .map(|t| self.tt_html(cx, t, self.d(this), "Document writeln"))
            .collect::<rt::Result<String>>()?;
        s.push('\n');
        self.document_write(cx, self.d(this), &s)
    }

    fn has_focus(&self, _cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        Ok(self.is_active_document(self.d(this)))
    }

    fn exec_command(
        &self,
        cx: &mut Cx,
        this: Handle,
        command_id: &str,
        _show_ui: bool,
        value: &str,
    ) -> rt::Result<bool> {
        Ok(self.exec_editing_command(cx, self.d(this), command_id, value))
    }
    fn query_command_enabled(&self, _cx: &mut Cx, this: Handle, c: &str) -> rt::Result<bool> {
        Ok(self.query_editing_command(self.d(this), c, "enabled"))
    }
    fn query_command_indeterm(&self, _cx: &mut Cx, this: Handle, c: &str) -> rt::Result<bool> {
        Ok(self.query_editing_command(self.d(this), c, "indeterm"))
    }
    fn query_command_state(&self, _cx: &mut Cx, this: Handle, c: &str) -> rt::Result<bool> {
        Ok(self.query_editing_command(self.d(this), c, "state"))
    }
    fn query_command_supported(&self, _cx: &mut Cx, this: Handle, c: &str) -> rt::Result<bool> {
        Ok(self.query_editing_command(self.d(this), c, "supported"))
    }
    fn query_command_value(&self, _cx: &mut Cx, this: Handle, c: &str) -> rt::Result<DomStr<'_>> {
        let c = c.to_ascii_lowercase();
        Ok(DomStr::Owned(self.ec_query_value(self.d(this), &c)))
    }
    fn clear(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn capture_events(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn release_events(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn exit_picture_in_picture(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        rejected(
            cx,
            ExceptionName::InvalidStateError,
            "Not in picture-in-picture",
        )
    }
    fn exit_pointer_lock(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn has_unpartitioned_cookie_access(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        p.resolve(cx, true);
        Ok(p)
    }
    fn has_storage_access(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        p.resolve(cx, true);
        Ok(p)
    }
    fn request_storage_access(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        super::element::resolved(cx)
    }
    fn has_private_token(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _issuer: &str,
    ) -> rt::Result<JsPromise> {
        rejected(
            cx,
            ExceptionName::NotAllowedError,
            "Private state tokens are not available",
        )
    }
    fn has_redemption_record(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _issuer: &str,
    ) -> rt::Result<JsPromise> {
        rejected(
            cx,
            ExceptionName::NotAllowedError,
            "Private state tokens are not available",
        )
    }

    fn current_script(
        &self,
        _this: Handle,
    ) -> Option<snowlark_bindings::generated::HTMLScriptElementOrSVGScriptElement> {
        use snowlark_bindings::generated::HTMLScriptElementOrSVGScriptElement as S;
        let n = self.current_script.get()?;
        let svg = super::svg::is_svg_element(&self.dom(), n);
        Some(if svg {
            S::SVGScriptElement(node_handle(n))
        } else {
            S::HTMLScriptElement(node_handle(n))
        })
    }

    /// SVG 2 §5.1.3 `rootElement`.
    fn root_element(&self, this: Handle) -> Option<Handle> {
        self.svg_root_element(self.d(this))
    }

    fn static_parse_html_unsafe(
        &self,
        cx: &mut Cx,
        html: TrustedHTMLOrString,
        _options: snowlark_bindings::generated::ParseHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    ) -> rt::Result<Handle> {
        let current = self.current_document(cx)?;
        let (markup, mode) =
            self.tt_html_input(cx, html, _options, current, "Document parseHTMLUnsafe")?;
        let input = match mode {
            super::element::InsertMode::Unsafe { input, .. } => input,
            super::element::InsertMode::Safe(_) => None,
        };
        let configuration = self.sanitizer_config_from_options(input, false)?;
        let doc = self.dom_mut().parse_html_str(
            &markup,
            snowlark_dom::ParseOptions {
                scripting_enabled: false,
                iframe_srcdoc: false,
            },
        );
        if let Some(c) = &configuration {
            self.sanitize_tree(doc, c);
        }
        Ok(node_handle(doc))
    }

    fn static_parse_html(
        &self,
        _cx: &mut Cx,
        html: &str,
        _options: snowlark_bindings::generated::SetHTMLOptions,
    ) -> rt::Result<Handle> {
        let configuration =
            self.sanitizer_config_from_options(Some(_options.sanitizer.into()), true)?;
        let doc = self.dom_mut().parse_html_str(
            html,
            snowlark_dom::ParseOptions {
                scripting_enabled: false,
                iframe_srcdoc: false,
            },
        );
        if let Some(c) = &configuration {
            self.sanitize_tree(doc, c);
        }
        Ok(node_handle(doc))
    }

    /// `new Document()`: an XML document ("xml" type, `application/xml`) of the associated document's
    /// origin, with the Document interface.
    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        let doc = self.dom_mut().create_document(DocumentKind::Xml);
        Ok(node_handle(doc))
    }
}

impl DocumentOrShadowRootImpl for Engine {
    fn element_from_point(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
    ) -> rt::Result<Option<Handle>> {
        let scope = self.n(this);
        let doc = self.dom().node_document(scope);
        let first = self
            .view_elements_from_point_in(doc, scope, x, y)
            .first()
            .copied();
        // HTML §4.8.14.2: over an image with an image map, the area under the point (the list of
        // `elementsFromPoint` keeps the image).
        let mapped = first
            .filter(|&e| {
                let dom = self.dom();
                dom.is_html_element_named(e, &local_name!("img"))
                    && dom.attr(e, &ns!(), &local_name!("usemap")).is_some()
            })
            .and_then(|_| self.view_hit(doc, snowlark_paint::Point::new(x as f32, y as f32)))
            .map(|h| h.element)
            .filter(|&a| self.dom().is_html_element_named(a, &local_name!("area")))
            .map(|a| self.retarget(&self.dom(), a, scope));
        Ok(mapped.or(first).map(super::node_handle))
    }

    fn elements_from_point(
        &self,
        _cx: &mut Cx,
        this: Handle,
        x: f64,
        y: f64,
    ) -> rt::Result<Vec<Handle>> {
        let scope = self.n(this);
        let doc = self.dom().node_document(scope);
        Ok(self
            .view_elements_from_point_in(doc, scope, x, y)
            .into_iter()
            .map(super::node_handle)
            .collect())
    }

    fn adopted_style_sheets_length(&self, this: Handle) -> u32 {
        self.adopted_sheets(self.n(this)).len() as u32
    }
    fn adopted_style_sheets_item(&self, this: Handle, index: u32) -> Handle {
        self.adopted_sheets(self.n(this))[index as usize]
    }
    fn adopted_style_sheets_set_indexed_value(
        &self,
        _cx: &mut Cx,
        this: Handle,
        value: Handle,
        _index: u32,
    ) -> rt::Result<()> {
        self.check_adoptable(self.n(this), value)
    }
    fn adopted_style_sheets_delete_indexed_value(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _value: Handle,
        _index: u32,
    ) -> rt::Result<()> {
        Ok(())
    }
    fn adopted_style_sheets_store(&self, this: Handle, index: u32, value: Option<Handle>) {
        self.store_adopted(self.n(this), index, value);
    }
    /// Web Animations 1 §6.10 `getAnimations()`.
    fn get_animations(&self, cx: &mut Cx, this: Handle) -> rt::Result<Vec<Handle>> {
        Ok(self.document_or_shadow_animations(cx, self.n(this)))
    }
    /// CSSOM §6.3 `styleSheets`.
    fn style_sheets(&self, this: Handle) -> Handle {
        self.style_sheet_list(self.n(this))
    }
    fn custom_element_registry(&self, this: Handle) -> Option<Handle> {
        let doc = self.dom().node_document(self.n(this));
        self.window_of_document(doc)
            .map(|w| self.registry_handle(w))
    }
    fn fullscreen_element(&self, this: Handle) -> Option<Handle> {
        self.fullscreen_element_for(self.n(this))
    }
    /// HTML §6.6.5 `activeElement`: the focused element retargeted against this tree's root, else the
    /// body (for a document), else null.
    fn active_element(&self, this: Handle) -> Option<Handle> {
        let root = self.n(this);
        let dom_doc = self.dom().node_document(root);
        let focused = self.doc_state(dom_doc, |s| s.focused);
        let candidate = focused.and_then(|f| {
            let dom = self.dom();
            if !dom.is_connected(f) {
                return None;
            }
            let t = self.retarget(&dom, f, root);
            (dom.root(t) == root).then_some(t)
        });
        if candidate.is_some() {
            return self.opt_node(candidate);
        }
        let dom = self.dom();
        if dom.is_document(root) {
            return self.opt_node(dom.body(root).or_else(|| dom.document_element(root)));
        }
        None
    }
    fn picture_in_picture_element(&self, _this: Handle) -> Option<Handle> {
        None
    }
    fn pointer_lock_element(&self, _this: Handle) -> Option<Handle> {
        None
    }
}

impl XMLDocumentImpl for Engine {}

impl HTMLDocumentImpl for Engine {}

impl DOMImplementationImpl for Engine {
    fn create_document_type(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        public_id: &str,
        system_id: &str,
    ) -> rt::Result<Handle> {
        let doc = self.implementation_document(this);
        self.dom_mut()
            .create_document_type(doc, name, public_id, system_id)
            .js()
            .map(node_handle)
    }

    fn create_document(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        namespace: Option<&str>,
        qualified_name: &str,
        doctype: Option<Handle>,
    ) -> rt::Result<Handle> {
        let doctype = doctype.map(|d| self.n(d));
        let doc = self
            .dom_mut()
            .create_xml_document(namespace, qualified_name, doctype)
            .js()?;
        self.xml_documents.borrow_mut().insert(doc);
        Ok(node_handle(doc))
    }

    fn create_html_document(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        title: Option<&str>,
    ) -> rt::Result<Handle> {
        Ok(node_handle(self.dom_mut().create_html_document(title)))
    }

    fn has_feature(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(true)
    }
}

impl Engine {
    fn implementation_document(&self, h: Handle) -> NodeId {
        match &*self.obj(h) {
            Object::DomImplementation { document } => *document,
            _ => panic!("not a DOMImplementation"),
        }
    }
}

impl DOMParserImpl for Engine {
    /// `parseFromString` (HTML §8.5.1). HTML parses with scripting disabled; the XML types go through
    /// the XML parser, and a document that is not well-formed holds a `parsererror` element.
    fn parse_from_string(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        string: TrustedHTMLOrString,
        type_: DOMParserSupportedType,
    ) -> rt::Result<Handle> {
        // (this's relevant global object's: the realm the parser was made in, not the method's)
        let window = match &*self.obj(_this) {
            Object::DomParser { window } => *window,
            _ => None,
        };
        let relevant = match window.and_then(|w| self.associated_document(w)) {
            Some(doc) => doc,
            None => self.current_document(_cx)?,
        };
        let s = self.tt_html(_cx, string, relevant, "DOMParser parseFromString")?;
        let base = self.dom().document_url(relevant).to_owned();
        match type_ {
            DOMParserSupportedType::TextHtml => {
                // The parse's reactions belong to the new document: handle them now, while its
                // nodes are still its own (script may adopt them into a rendered document next).
                // No script runs during the parse (scripting is disabled and the document has no
                // browsing context, so no custom element is constructed): none of its nodes has a
                // wrapper.
                let doc = self.parsed_insertion(_cx, |dom| dom.parse_html_document_inert(&s));
                let mut dom = self.dom_mut();
                dom.set_document_url(doc, &base);
                dom.set_allow_declarative_shadow_roots(doc, false);
                Ok(node_handle(doc))
            }
            other => {
                let content_type = other.as_str();
                let (doc, _) = self.dom_mut().parse_xml_str(&s);
                self.process_reactions(_cx);
                let mut dom = self.dom_mut();
                dom.set_content_type(doc, content_type);
                dom.set_document_url(doc, &base);
                // (a Document, not an XMLDocument: the algorithm makes "a new Document")
                Ok(node_handle(doc))
            }
        }
    }

    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        // (the constructor runs in its own realm, the one the new object belongs to)
        let window = self.current_window(_cx);
        Ok(self.alloc(Object::DomParser { window }))
    }
}

impl XMLSerializerImpl for Engine {
    fn serialize_to_string(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        root: Handle,
    ) -> rt::Result<DomStr<'_>> {
        self.dom()
            .serialize_xml(self.n(root), false)
            .map(DomStr::Owned)
            .js()
    }

    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        Ok(self.alloc(Object::XmlSerializer))
    }
}
