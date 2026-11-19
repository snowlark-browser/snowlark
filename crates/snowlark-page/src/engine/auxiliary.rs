// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Auxiliary browsing contexts (HTML §7.2.2.1 "the window open steps", §7.3.1.7 "the rules for
//! choosing a navigable", §7.3.2.1): `window.open()`, `opener`, `closed` and `close()`.
//!
//! A window that script opens with an opener is a top-level traversable this page hosts next to its
//! own, in the same event loop and isolate, so script reaches it as it reaches a frame: through its
//! WindowProxy, with the access checks of another origin's window when its document is of one. The
//! embedder decides whether the page hosts such windows ([`Engine::set_new_windows_in_tabs`]): the
//! headless modes do; a browser that opens each in a tab of its own gets the URL instead and the
//! script gets `null`, as with `noopener`.

use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsValue, ToJs};
use snowlark_dom::NodeId;

use super::frames::{ContextId, Navigable, NavigationRequest};
use super::window::ScreenPlacement;
use super::{exception, Engine};
use crate::event_loop::TaskSource;

/// A top-level traversable a page hosts besides its own (an auxiliary browsing context, or a
/// window with no opener in the headless modes): its browsing context's number in the page.
pub type HostedWindow = usize;

/// A change of the top-level traversables a page hosts, for an embedder that shows them
/// ([`Engine::set_shows_hosted_windows`]).
#[derive(Clone, Debug, PartialEq)]
pub enum WindowChange {
    /// Script opened one: whether it asked for a popup window ("is popup"), its viewport (CSS
    /// px, from the `width` and `height` features), and where its window's left and top edges
    /// are to be on the screen (the `left` and `top` features, CSS px; `None` where not given).
    Opened {
        window: HostedWindow,
        popup: bool,
        size: (f32, f32),
        position: (Option<i32>, Option<i32>),
    },
    /// Script closed one (`window.close()`).
    Closed(HostedWindow),
    /// Script moved or resized one (`moveTo()`, `resizeBy()` and the like): its window's new
    /// rectangle, left, top, width and height on the screen (the window's device-independent px).
    Rect {
        window: HostedWindow,
        rect: (i32, i32, i32, i32),
    },
}

/// HTML "tokenize the features argument": the names and values of a `window.open()` features
/// string, in order (a later name replaces the value of an earlier one).
pub(crate) fn tokenize_features(features: &str) -> Vec<(String, String)> {
    // (a "feature separator": ASCII whitespace, `=` or `,`)
    let sep = |c: char| c.is_ascii_whitespace() || c == '=' || c == ',';
    let chars: Vec<char> = features.chars().collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // Leading separators, then the name.
        while i < chars.len() && sep(chars[i]) {
            i += 1;
        }
        let start = i;
        while i < chars.len() && !sep(chars[i]) {
            i += 1;
        }
        let name = normalize_feature_name(
            chars[start..i]
                .iter()
                .collect::<String>()
                .to_ascii_lowercase(),
        );
        // To the first `=`, not past a `,` or a non-separator.
        while i < chars.len() && chars[i] != '=' {
            if chars[i] == ',' || !sep(chars[i]) {
                break;
            }
            i += 1;
        }
        let mut value = String::new();
        if i < chars.len() && sep(chars[i]) {
            // To the first non-separator, not past a `,`.
            while i < chars.len() && sep(chars[i]) {
                if chars[i] == ',' {
                    break;
                }
                i += 1;
            }
            let start = i;
            while i < chars.len() && !sep(chars[i]) {
                i += 1;
            }
            value = chars[start..i]
                .iter()
                .collect::<String>()
                .to_ascii_lowercase();
        }
        if !name.is_empty() {
            match out.iter_mut().find(|(n, _)| *n == name) {
                Some(entry) => entry.1 = value,
                None => out.push((name, value)),
            }
        }
    }
    out
}

/// HTML "normalize a feature name": the legacy aliases.
fn normalize_feature_name(name: String) -> String {
    match name.as_str() {
        "screenx" => "left".to_owned(),
        "screeny" => "top".to_owned(),
        "innerwidth" => "width".to_owned(),
        "innerheight" => "height".to_owned(),
        _ => name,
    }
}

fn feature<'a>(features: &'a [(String, String)], name: &str) -> Option<&'a str> {
    features
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// HTML "rules for parsing integers" (None: an error).
fn parse_integer(s: &str) -> Option<i64> {
    let s = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let digits: &str = &digits[..digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len())];
    if digits.is_empty() {
        return None;
    }
    let value = digits.bytes().fold(0i64, |v, d| {
        v.saturating_mul(10).saturating_add(i64::from(d - b'0'))
    });
    Some(if negative { -value } else { value })
}

/// HTML "parse a boolean feature".
pub(crate) fn parse_boolean_feature(value: &str) -> bool {
    if value.is_empty() || value == "yes" || value == "true" {
        return true;
    }
    parse_integer(value).unwrap_or(0) != 0
}

/// HTML "check if a window feature is set".
fn feature_set(features: &[(String, String)], name: &str, default: bool) -> bool {
    feature(features, name).map_or(default, parse_boolean_feature)
}

/// HTML "check if a popup window is requested".
pub(crate) fn popup_window_requested(features: &[(String, String)]) -> bool {
    if features.is_empty() {
        return false;
    }
    if let Some(popup) = feature(features, "popup") {
        return parse_boolean_feature(popup);
    }
    let location = feature_set(features, "location", false);
    let toolbar = feature_set(features, "toolbar", false);
    if !location && !toolbar {
        return true;
    }
    !feature_set(features, "menubar", false)
        || !feature_set(features, "resizable", true)
        || !feature_set(features, "scrollbars", false)
        || !feature_set(features, "status", false)
}

/// Whether `url` "matches about:blank": `about:blank`, with any query and fragment.
fn matches_about_blank(url: &url::Url) -> bool {
    url.scheme() == "about"
        && url.path() == "blank"
        && url.username().is_empty()
        && url.password().is_none()
        && url.host().is_none()
}

impl Engine {
    /// Whether the embedder opens each new top-level traversable a page asks for in a tab of its
    /// own, with no opener (`window.open()` then returns `null`), rather than letting the page host
    /// it as an auxiliary browsing context. Off, as in the headless modes, the page hosts them.
    pub fn set_new_windows_in_tabs(&self, on: bool) {
        self.embedder.borrow_mut().new_windows_in_tabs = on;
    }

    /// The popup blocker (HTML §7.3.1.7): a new top-level traversable needs transient activation
    /// of the document that asks for it. Off, as in the headless modes, any script opens windows.
    pub fn set_popup_blocker(&self, on: bool) {
        self.embedder.borrow_mut().popup_blocker = on;
    }

    /// The browsing context of a Window (also of one a navigation replaced since).
    fn browsing_context_of(&self, window: Handle) -> Option<ContextId> {
        let f = self.frames.borrow();
        // (a browsing context group switch left it: its browsing context is not the navigable's)
        if f.severed.contains(&window) {
            return None;
        }
        let id = f
            .by_window
            .get(&window)
            .copied()
            .or_else(|| f.replaced.get(&window).map(|&(id, _)| id))?;
        (!f.list[id].discarded).then_some(id)
    }

    /// HTML §7.2.2.1 "the window open steps".
    pub(crate) fn window_open(
        &self,
        cx: &mut Cx,
        url: &str,
        target: &str,
        features: &str,
    ) -> rt::Result<Option<Handle>> {
        // 2-4. The URL, relative to the entry global object's document.
        let source = self.entry_document(cx)?;
        let url_record = if url.is_empty() {
            None
        } else {
            Some(
                self.base_url_of(source)
                    .join(url)
                    .map_err(|_| exception(ExceptionName::SyntaxError, "Invalid URL"))?,
            )
        };
        // 5-11.
        let target = if target.is_empty() { "_blank" } else { target };
        let mut features = tokenize_features(features);
        let noreferrer = feature(&features, "noreferrer").is_some_and(parse_boolean_feature);
        let mut noopener = self.noopener_for_window_open(source, &features, url_record.as_ref());
        features.retain(|(n, _)| n != "noopener" && n != "noreferrer");
        let mut referrer_policy = snowlark_net::ReferrerPolicy::Empty;
        if noreferrer {
            noopener = true;
            referrer_policy = snowlark_net::ReferrerPolicy::NoReferrer;
        }
        let Some(current) = self.context_of_document(source) else {
            return Ok(None);
        };
        // 12-13. The rules for choosing a navigable.
        let id = match self.choose_navigable(source, target) {
            Navigable::None => return Ok(None),
            Navigable::Context(id) => {
                // 15. An existing navigable: navigated when there is a URL.
                if let Some(url) = url_record {
                    self.plan_navigation(source, target, url, None, false, false, referrer_policy);
                }
                let keyword = ["_self", "_parent", "_top"]
                    .iter()
                    .any(|k| target.eq_ignore_ascii_case(k));
                if noopener && !keyword {
                    return Ok(None);
                }
                id
            }
            Navigable::NewTab => {
                if !self.may_open_new_window(source) {
                    return Ok(None);
                }
                self.consume_user_activation(source);
                let mut target = target;
                if self.opener_policy_forces_noopener(source) {
                    noopener = true;
                    target = "_blank";
                }
                if self.embedder.borrow().new_windows_in_tabs
                    || (noopener && self.noopener_windows_are_tabs())
                {
                    // A tab of the browser's, with no opener.
                    if let Some(url) = url_record.filter(|u| !matches_about_blank(u)) {
                        self.open_requests.borrow_mut().push((url, false));
                    }
                    return Ok(None);
                }
                let opener = (!noopener).then_some(current);
                let id = self.new_top_level(cx, source, opener, target, &features);
                let url = url_record
                    .unwrap_or_else(|| url::Url::parse("about:blank").expect("about:blank"));
                if matches_about_blank(&url) {
                    // ("the URL and history update steps": `about:blank?foo` stays put)
                    if url.as_str() != "about:blank" {
                        let doc = self.frames.borrow().list[id].document;
                        self.dom_mut().set_document_url(doc, url.as_str());
                    }
                } else {
                    self.navigate_request(
                        id,
                        NavigationRequest {
                            url,
                            post: None,
                            source: Some(source),
                            form: false,
                            reload: false,
                            referrer_policy,
                            history: Default::default(),
                        },
                    );
                }
                // ("new with no opener")
                if noopener {
                    return Ok(None);
                }
                id
            }
        };
        Ok(Some(self.frames.borrow().list[id].window))
    }

    /// HTML "get an element's noopener" for an `a`, `area` or `form` element navigating `target`:
    /// (noopener, noreferrer). `rel=noopener` or `noreferrer`, or `_blank` without `rel=opener`.
    pub(crate) fn element_noopener(&self, el: NodeId, target: &str) -> (bool, bool) {
        let dom = self.dom();
        let rel = dom.get_attribute(el, "rel").unwrap_or("");
        let has = |kind: &str| {
            rel.split_ascii_whitespace()
                .any(|t| t.eq_ignore_ascii_case(kind))
        };
        let noreferrer = has("noreferrer");
        let noopener = noreferrer
            || has("noopener")
            || (!has("opener") && target.trim().eq_ignore_ascii_case("_blank"));
        (noopener, noreferrer)
    }

    /// HTML "follow the hyperlink" and the form submission algorithm from step "let
    /// targetNavigable be...": the rules for choosing a navigable given `el`'s target and
    /// noopener, then a navigation of it. A new top-level traversable is hosted by this page,
    /// unless the embedder opens new windows in tabs of its own.
    pub(crate) fn navigate_element_target(
        &self,
        cx: &mut Cx,
        el: NodeId,
        target: &str,
        url: url::Url,
        post: Option<(Vec<u8>, String)>,
        form: bool,
    ) -> Option<ContextId> {
        let doc = self.dom().node_document(el);
        let new = matches!(self.choose_navigable(doc, target), Navigable::NewTab);
        let (mut noopener, noreferrer) = self.element_noopener(el, target);
        let referrer_policy = self.element_referrer_policy(el, noreferrer);
        let mut target = target;
        if new && self.opener_policy_forces_noopener(doc) {
            noopener = true;
            target = "_blank";
        }
        if !new
            || self.embedder.borrow().new_windows_in_tabs
            || (noopener && self.noopener_windows_are_tabs())
        {
            return self.plan_navigation(doc, target, url, post, false, form, referrer_policy);
        }
        let current = self.context_of_document(doc)?;
        if !self.may_open_new_window(doc) {
            return None;
        }
        self.consume_user_activation(doc);
        let opener = (!noopener).then_some(current);
        let id = self.new_top_level(cx, doc, opener, target.trim(), &[]);
        self.navigate_request(
            id,
            NavigationRequest {
                url,
                post,
                source: Some(doc),
                form,
                reload: false,
                referrer_policy,
                history: Default::default(),
            },
        );
        // (a new top-level traversable's navigation is under way: nothing left to replace)
        None
    }

    /// HTML "follow the hyperlink" and "plan to navigate": the navigation's referrer policy, "no-referrer"
    /// for `rel=noreferrer`, else the `a` or `area` element's `referrerpolicy` attribute ("" for a form
    /// or a missing or invalid value: the document's policy then).
    fn element_referrer_policy(
        &self,
        el: NodeId,
        noreferrer: bool,
    ) -> snowlark_net::ReferrerPolicy {
        if noreferrer {
            return snowlark_net::ReferrerPolicy::NoReferrer;
        }
        let dom = self.dom();
        let link = dom.is_html_element_named(el, &snowlark_dom::local_name!("a"))
            || dom.is_html_element_named(el, &snowlark_dom::local_name!("area"));
        link.then(|| dom.get_attribute(el, "referrerpolicy"))
            .flatten()
            .and_then(|v| snowlark_net::ReferrerPolicy::parse(&v.to_ascii_lowercase()))
            .unwrap_or(snowlark_net::ReferrerPolicy::Empty)
    }

    /// HTML "get noopener for window open": a `blob:` URL whose entry's origin is not same site with
    /// the source document's top-level origin opens with no opener, as does the `noopener` feature.
    fn noopener_for_window_open(
        &self,
        source: NodeId,
        features: &[(String, String)],
        url: Option<&url::Url>,
    ) -> bool {
        if let Some(url) = url.filter(|u| u.scheme() == "blob") {
            // (the entry's environment's origin is the one the URL names: `blob:<origin>/<uuid>`)
            let held = self.hooks.borrow().net.as_ref().is_some_and(|net| {
                net.hold_blob_url(url)
                    .map(|token| net.release_blob_url(token))
                    .is_some()
            });
            if held {
                let top = self.top_level_net_origin(source);
                if !snowlark_net::origin::same_site(&url.origin(), &top) {
                    return true;
                }
            }
        }
        feature(features, "noopener").is_some_and(parse_boolean_feature)
    }

    /// The origin of the top-level document of `doc`'s traversable (its relevant settings object's
    /// top-level origin).
    fn top_level_net_origin(&self, doc: NodeId) -> url::Origin {
        self.net_origin_of_document(self.top_level_document(doc))
    }

    /// The active document of `doc`'s traversable (`doc` itself without a browsing context).
    pub(crate) fn top_level_document(&self, doc: NodeId) -> NodeId {
        self.context_of_document(doc)
            .map(|id| {
                let f = self.frames.borrow();
                f.list[super::frames::top_of(&f, id)].document
            })
            .unwrap_or(doc)
    }

    /// HTML "the rules for choosing a navigable", a new top-level traversable: a document whose opener
    /// policy is `same-origin` and whose origin is not its top-level origin opens it with no opener.
    /// (The policy that rules a frame is its top-level document's: the frame's own about:blank takes it
    /// only when same origin with the top.)
    fn opener_policy_forces_noopener(&self, doc: NodeId) -> bool {
        use snowlark_net::coop::OpenerPolicyValue::{SameOrigin, SameOriginPlusCoep};
        let top = self.top_level_document(doc);
        let value = self.doc_state(top, |s| s.opener_policy.value);
        matches!(value, SameOrigin | SameOriginPlusCoep)
            && self.net_origin_of_document(doc) != self.net_origin_of_document(top)
    }

    /// The popup blocker and the sandboxed auxiliary navigation browsing context flag: whether
    /// `source` may have a new top-level traversable.
    fn may_open_new_window(&self, source: NodeId) -> bool {
        if self.embedder.borrow().popup_blocker && !self.has_transient_activation(source) {
            self.console_print(
                "warn",
                "Blocked a new window opened without user activation",
            );
            return false;
        }
        if self.auxiliary_navigation_sandboxed(source) {
            self.console_print(
                "warn",
                "Blocked a new window opened from a sandboxed document without 'allow-popups'",
            );
            return false;
        }
        true
    }

    /// The rules for choosing a navigable when a new top-level traversable is requested: "create a
    /// new top-level traversable" given `opener` (an auxiliary browsing context of `source`'s, or
    /// one of a new browsing context group without), hosted by this page.
    fn new_top_level(
        &self,
        cx: &mut Cx,
        source: NodeId,
        opener: Option<ContextId>,
        target: &str,
        features: &[(String, String)],
    ) -> ContextId {
        let name = if target.eq_ignore_ascii_case("_blank") {
            ""
        } else {
            target
        };
        let viewport = self.new_window_viewport(features);
        let id = self.create_auxiliary(cx, opener, source, name, viewport);
        let doc = self.frames.borrow().list[id].document;
        // (the sandbox propagates to auxiliary browsing contexts: the popup's sandboxing flag set
        // takes the source's flags, and its documents start with them, its initial one included)
        if let Some(sandbox) = self.sandbox_propagated_to_popups(source) {
            self.frames.borrow_mut().list[id].popup_sandbox = Some(sandbox);
            self.apply_popup_sandbox(doc, sandbox);
            if !sandbox.same_origin {
                self.document_origin_changed(cx, doc);
            }
        }
        // HTML "create a new browsing context and document" with a creator: the initial about:blank
        // takes the opener policy of the creator's top-level document when the creator is same origin
        // with it.
        if opener.is_some() {
            let top = self.top_level_document(source);
            if self.net_origin_of_document(source) == self.net_origin_of_document(top) {
                let policy = self.doc_state(top, |s| s.opener_policy.clone());
                self.doc_state(doc, |s| s.opener_policy = policy);
            }
        }
        let popup = popup_window_requested(features);
        self.frames.borrow_mut().list[id].is_popup = popup;
        let mut embedder = self.embedder.borrow_mut();
        if embedder.shows_hosted_windows {
            embedder.window_changes.push(WindowChange::Opened {
                window: id,
                popup,
                size: viewport,
                position: new_window_position(features),
            });
        }
        id
    }

    /// CSSOM View §4.1 "set up browsing context features": the `width` and `height` features (at
    /// least 100 pixels, at most the screen), else the page's own viewport.
    fn new_window_viewport(&self, features: &[(String, String)]) -> (f32, f32) {
        let own = self.render.borrow().viewport;
        let screen = self.hooks.borrow().viewport;
        let (w, h) = if own.width > 0.0 && own.height > 0.0 {
            (own.width, own.height)
        } else if screen.0 > 0 && screen.1 > 0 {
            (screen.0 as f32, screen.1 as f32)
        } else {
            (800.0, 600.0)
        };
        let max = |screen: i32, own: f32| if screen > 0 { screen as f32 } else { own };
        let size = |name: &str, own: f32, screen: i32| {
            feature(features, name)
                .and_then(parse_integer)
                .map_or(own, |v| {
                    (v as f32).max(100.0).min(max(screen, own).max(100.0))
                })
        };
        (size("width", w, screen.0), size("height", h, screen.1))
    }

    /// HTML §7.2.2.1 the `opener` getter.
    pub(crate) fn window_opener(&self, cx: &mut Cx, window: Handle) -> rt::Result<JsValue> {
        let opener = self.browsing_context_of(window).and_then(|id| {
            let f = self.frames.borrow();
            let o = f.list[id].opener?;
            (!f.list[o].discarded).then_some(f.list[o].window)
        });
        let scope = cx.scope();
        match opener.and_then(|w| w.to_js(scope)) {
            Some(v) => Ok(JsValue::new(scope, v)),
            None => Ok(rt::null_value(scope)),
        }
    }

    /// HTML §7.2.2.1 the `opener` setter: null clears the opener browsing context; anything else
    /// becomes an own data property of the window.
    pub(crate) fn set_window_opener(
        &self,
        cx: &mut Cx,
        window: Handle,
        value: JsValue,
    ) -> rt::Result<()> {
        let scope = cx.scope();
        let v = v8::Local::new(scope, value.global());
        if v.is_null() {
            if let Some(id) = self.browsing_context_of(window) {
                self.frames.borrow_mut().list[id].opener = None;
            }
            return Ok(());
        }
        let Some(object) = rt::wrap(scope, window) else {
            return Ok(());
        };
        let key = v8::String::new(scope, "opener").expect("a short string");
        object.create_data_property(scope, key.into(), v);
        Ok(())
    }

    /// HTML §7.2.2.1 the `closed` getter: the browsing context is gone, or closing.
    pub(crate) fn window_closed(&self, window: Handle) -> bool {
        if self.context_discarded(window) {
            return true;
        }
        let f = self.frames.borrow();
        f.by_window
            .get(&window)
            .is_some_and(|&id| f.list[id].closing)
    }

    /// HTML §7.2.2.1 `close()`: a top-level traversable that script may close (one `window.open()`
    /// made) closes in a task, when the incumbent window is familiar with it and allowed by sandboxing
    /// to navigate it. (The page's own top-level traversable is the embedder's: it stays.)
    pub(crate) fn window_close(&self, cx: &mut Cx, window: Handle) {
        let Some(id) = self.context_of_window(window) else {
            return;
        };
        {
            let f = self.frames.borrow();
            let b = &f.list[id];
            if b.discarded || b.parent.is_some() || b.closing || !b.created_by_web_content {
                return;
            }
        }
        let incumbent = self
            .incumbent_window(cx)
            .and_then(|w| self.browsing_context_of(w));
        let Some(incumbent) = incumbent else {
            return;
        };
        if !self.familiar_with(incumbent, id) {
            return;
        }
        let source = self.frames.borrow().list[incumbent].document;
        if !self.allowed_by_sandboxing_to_navigate(source, id) {
            return;
        }
        self.frames.borrow_mut().list[id].closing = true;
        // "Definitely close": the traversable and its frames are destroyed.
        self.queue_task(TaskSource::DomManipulation, move |e, _cx| {
            e.discard_contexts(&[id]);
            let mut embedder = e.embedder.borrow_mut();
            if embedder.shows_hosted_windows {
                embedder.window_changes.push(WindowChange::Closed(id));
            }
        });
    }

    /// An embedder that shows the top-level traversables this page hosts as views of their own
    /// ([`Engine::take_window_changes`]); a new window with no opener is not hosted then, but asked
    /// of the browser ([`Engine::take_open_requests`]), and `window.open()` returns `null` for it.
    pub fn set_shows_hosted_windows(&self, on: bool) {
        self.embedder.borrow_mut().shows_hosted_windows = on;
    }

    /// The hosted top-level traversables made and closed by script since the last call.
    pub fn take_window_changes(&self) -> Vec<WindowChange> {
        std::mem::take(&mut self.embedder.borrow_mut().window_changes)
    }

    /// Whether a new window with no opener is the browser's rather than this page's.
    fn noopener_windows_are_tabs(&self) -> bool {
        let e = self.embedder.borrow();
        e.new_windows_in_tabs || e.shows_hosted_windows
    }

    /// The active document of a top-level traversable this page hosts besides its own (`None`
    /// once it closed).
    pub fn hosted_window_document(&self, window: HostedWindow) -> Option<NodeId> {
        let f = self.frames.borrow();
        let b = f.list.get(window)?;
        (b.parent.is_none() && !b.discarded && b.created_by_web_content).then_some(b.document)
    }

    /// The user closed the view of a hosted top-level traversable: it is destroyed (HTML "close
    /// a top-level traversable"; `closed` is true for its WindowProxy from now on).
    pub fn close_hosted_window(&self, window: HostedWindow) {
        if self.hosted_window_document(window).is_none() {
            return;
        }
        self.end_view();
        self.frames.borrow_mut().list[window].closing = true;
        self.discard_contexts(&[window]);
    }

    /// The user navigates the hosted top-level traversable `window` (its view's address bar,
    /// Reload, a session history entry): a navigation with no initiator document. False when the
    /// window is gone.
    pub fn navigate_hosted_window(
        &self,
        window: HostedWindow,
        url: url::Url,
        reload: bool,
    ) -> bool {
        if self.hosted_window_document(window).is_none() {
            return false;
        }
        self.navigate_request(
            window,
            NavigationRequest {
                url,
                post: None,
                source: None,
                form: false,
                reload,
                referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                history: Default::default(),
            },
        );
        true
    }

    /// The browser closed the page's own tab (or moved it to another renderer) while windows the
    /// page hosts stay open: HTML "definitely close a top-level traversable" for the page's own:
    /// its document and its descendants unload (`pagehide`, `unload`), then "destroy a top-level
    /// traversable" discards its browsing context and its frames, so the windows it opened see
    /// `opener` null (the opener getter: an opener browsing context that was discarded). The
    /// windows it hosts go on. (No prompt to unload: the user closed the tab.)
    pub fn close_own_top_level(&self, cx: &mut Cx) {
        let doc = {
            let f = self.frames.borrow();
            match f.list.first() {
                Some(b) if !b.discarded => b.document,
                _ => return,
            }
        };
        self.end_view();
        self.unload_document_and_descendants(cx, doc);
        self.frames.borrow_mut().list[0].closing = true;
        self.discard_contexts(&[0]);
    }

    /// Whether `window`'s top-level traversable is a popup window (HTML "is popup").
    pub(crate) fn window_is_popup(&self, window: Handle) -> bool {
        let f = self.frames.borrow();
        f.by_window
            .get(&window)
            .is_some_and(|&id| f.list[super::frames::top_of(&f, id)].is_popup)
    }

    /// Where the window of `window`'s top-level traversable sits on the screen, and its viewport
    /// when it has no window to say: a hosted window's own (its view's), else the page's.
    pub(crate) fn screen_of(&self, window: Handle) -> (ScreenPlacement, (i32, i32)) {
        let top = {
            let f = self.frames.borrow();
            f.by_window.get(&window).map(|&id| {
                let b = &f.list[super::frames::top_of(&f, id)];
                (b.screen, b.popup_viewport)
            })
        };
        let h = self.hooks.borrow();
        match top {
            Some((Some(s), viewport)) => (
                s,
                viewport.map_or(h.viewport, |(w, h)| (w as i32, h as i32)),
            ),
            _ => (h.screen, h.viewport),
        }
    }

    /// The screen placement for the events of a document: its top-level traversable's window's.
    pub(crate) fn screen_of_document(&self, doc: NodeId) -> ScreenPlacement {
        match self.window_of_document(doc) {
            Some(w) => self.screen_of(w).0,
            None => self.hooks.borrow().screen,
        }
    }

    /// Where the view of the hosted top-level traversable `window` sits on the screen (the
    /// browser's word, as [`Engine::set_screen_placement`] is for the page's own window).
    pub fn set_hosted_window_screen(&self, window: HostedWindow, placement: ScreenPlacement) {
        if self.hosted_window_document(window).is_none() {
            return;
        }
        self.frames.borrow_mut().list[window].screen = Some(placement);
    }

    /// CSSOM View §4 `moveTo()`, `moveBy()`, `resizeTo()`, `resizeBy()`: `rect` makes the window's
    /// new rectangle (left, top, width, height, in the window's device-independent px, as
    /// `screenX` and `outerWidth` report them) out of its current one. Only a window that script
    /// opened ("an auxiliary browsing context that was created by a script") moves, and only one
    /// this page hosts for an embedder that shows them; whether it does is the embedder's (a
    /// browser moves popup windows, not tabs).
    pub(crate) fn window_move_or_resize(
        &self,
        window: Handle,
        rect: impl FnOnce((i32, i32, i32, i32)) -> (i32, i32, i32, i32),
    ) {
        let Some(id) = self.context_of_window(window) else {
            return;
        };
        {
            let f = self.frames.borrow();
            let b = &f.list[id];
            if b.parent.is_some() || b.discarded || !b.created_by_web_content || id == 0 {
                return;
            }
        }
        if !self.embedder.borrow().shows_hosted_windows {
            return;
        }
        let (screen, viewport) = self.screen_of(window);
        let current =
            screen
                .outer
                .unwrap_or((screen.origin.0, screen.origin.1, viewport.0, viewport.1));
        let (x, y, w, h) = rect(current);
        // (a window is never smaller than 100 by 100: CSSOM View's "optionally clamp", as
        // Chromium keeps a minimum size)
        let (w, h) = (w.max(100), h.max(100));
        // (the window is where it was asked to be until its view says otherwise, so a `moveTo()`
        // and a `resizeTo()` in one turn both take: Chromium keeps the pending rectangle)
        let mut placement = screen;
        if let Some(old) = placement.outer {
            placement.origin = (
                placement.origin.0 + x - old.0,
                placement.origin.1 + y - old.1,
            );
        }
        placement.outer = Some((x, y, w, h));
        self.frames.borrow_mut().list[id].screen = Some(placement);
        self.embedder
            .borrow_mut()
            .window_changes
            .push(WindowChange::Rect {
                window: id,
                rect: (x, y, w, h),
            });
    }
}

/// CSSOM View §4.1 "set up browsing context features": the `left` and `top` features (the
/// window's left and top edges on the screen, in CSS px; the tokenizer maps `screenx` and
/// `screeny` to them), each when it parses as an integer.
fn new_window_position(features: &[(String, String)]) -> (Option<i32>, Option<i32>) {
    let at = |name: &str| {
        feature(features, name)
            .and_then(parse_integer)
            .map(|v| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
    };
    (at("left"), at("top"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(s: &str) -> Vec<(String, String)> {
        tokenize_features(s)
    }

    fn pair(n: &str, v: &str) -> (String, String) {
        (n.to_owned(), v.to_owned())
    }

    #[test]
    fn features_tokenize_as_the_spec_says() {
        assert_eq!(tok(""), vec![]);
        assert_eq!(tok("noopener"), vec![pair("noopener", "")]);
        assert_eq!(
            tok(" width = 300 ,HEIGHT=200,,popup"),
            vec![
                pair("width", "300"),
                pair("height", "200"),
                pair("popup", "")
            ]
        );
        assert_eq!(
            tok("screenx=5,innerWidth=7"),
            vec![pair("left", "5"), pair("width", "7")]
        );
        assert_eq!(tok("a=1,a=2"), vec![pair("a", "2")]);
        assert_eq!(tok("noopener  =  yes"), vec![pair("noopener", "yes")]);
        assert_eq!(tok("a b"), vec![pair("a", ""), pair("b", "")]);
    }

    #[test]
    fn boolean_features_parse_as_integers_or_words() {
        for yes in ["", "yes", "true", "1", " 2", "-1", "3px"] {
            assert!(parse_boolean_feature(yes), "{yes:?}");
        }
        for no in ["no", "false", "0", "foo", "0.5"] {
            assert!(!parse_boolean_feature(no), "{no:?}");
        }
    }

    #[test]
    fn a_popup_is_requested_by_minimal_features() {
        assert!(!popup_window_requested(&tok("")));
        assert!(popup_window_requested(&tok("popup")));
        assert!(!popup_window_requested(&tok("popup=0")));
        assert!(popup_window_requested(&tok("width=100")));
        assert!(!popup_window_requested(&tok(
            "location,menubar,scrollbars,status"
        )));
        assert!(popup_window_requested(&tok(
            "toolbar,menubar,scrollbars,status,resizable=no"
        )));
    }
}
