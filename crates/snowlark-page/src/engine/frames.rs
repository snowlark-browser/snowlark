// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Browsing contexts (HTML §7.3, "navigables" in today's text): the top-level one of the page and one
//! per `iframe`/`frame` element, each with its Window, its active document and its JavaScript realm. All
//! of them share the page's event loop and isolate (similar-origin window agents of one browsing context
//! group).
//!
//! What script of one realm may do to another's Window and Location is decided by V8's access check
//! (`snowlark_bindings::rt::cross_origin`), from [`Frames::origins`]: a Window's origin and domain, kept
//! here so that a decision needs no DOM. A realm's security token is the fast path of that check: two
//! realms with equal tokens are same origin-domain, and one whose document set `document.domain` has a
//! token of its own, so that every access to or from it asks the check.
//!
//! A browsing context has one WindowProxy for its life: its first realm's global proxy, which each
//! later realm takes over (`Context::new` with `global_object`). Navigation creates a new Window and
//! realm for the new document, except for the first navigation away from the initial `about:blank`
//! document to a same origin-domain one, which keeps them (HTML §7.4.2.3 "create and initialize a
//! Document object" step 6). The replaced Window lives on behind its old realm's global object, for
//! as long as that realm's code can run.

use std::collections::HashMap;

use rustc_hash::FxHashMap;
use snowlark_bindings::generated::DocumentReadyState;
use snowlark_bindings::rt::{self, Cx, Handle};
use snowlark_dom::{local_name, ns, NodeId};
use snowlark_net::origin::OriginDomain;

use super::{node_handle, Engine, Object};
use crate::event_loop::TaskSource;

/// Index of a browsing context in [`Frames::list`]; the top-level one is 0.
pub type ContextId = usize;

pub const TOP: ContextId = 0;

pub struct BrowsingContext {
    pub window: Handle,
    pub document: NodeId,
    /// The realm whose global object is `window` (set once V8 made it).
    pub realm: Option<v8::Global<v8::Context>>,
    pub parent: Option<ContextId>,
    /// The `iframe`/`frame`/`object` element, for a child browsing context.
    pub container: Option<NodeId>,
    /// Removed from its container (HTML §7.3.1.5 "destroy a child navigable").
    pub discarded: bool,
    /// The browsing context still shows its initial `about:blank` document.
    pub initial_about_blank: bool,
    /// A navigation started by script (`location.href = …`, a form submission), performed after the
    /// current task.
    pub navigation_request: Option<NavigationRequest>,
    pub name: String,
    /// A pending iframe load event for a navigation in this context.
    pub pending_load_event: bool,
    /// HTML "lazy load resumption steps" of the container: the navigation a `loading=lazy`
    /// iframe waits to start until it is near the viewport.
    pub lazy_load: Option<LazyNavigation>,
    /// HTML "current navigation was lazy loaded": the container does not delay its document's
    /// load event for this navigation.
    pub lazily_loaded: bool,
    /// Counts the navigations of this context that fetch or make a document: a `javascript:` URL
    /// navigation queued before another one began is dropped.
    pub navigations: u64,
    /// The navigation (its number in `navigations`) that is an `object` container's fetch of its
    /// `data` (HTML §4.8.7): its response decides whether the element shows it or falls back.
    pub object_fetch: Option<u64>,
    /// The origin of the document that started the navigation under way, which an `about:` document
    /// takes.
    pub initiator_origin: Option<Inherited>,
    /// The container's `sandbox` attribute when the navigation under way started: what the
    /// attribute says later does not change the document it brings.
    pub sandbox_attribute: Option<String>,
    /// HTML "target snapshot params" iframe referrer policy: the container's `referrerpolicy`
    /// when the navigation under way started (or the context was made), which the document it
    /// brings computes its ancestor origins with.
    pub container_referrer_policy: snowlark_net::ReferrerPolicy,
    /// HTML "opener browsing context": the context whose script opened this one with
    /// `window.open()`, until `opener = null` clears it.
    pub opener: Option<ContextId>,
    /// HTML "is created by web content": a top-level traversable `window.open()` made.
    pub created_by_web_content: bool,
    /// HTML "is closing": `close()` queued the closing of this top-level traversable.
    pub closing: bool,
    /// The viewport of a top-level traversable other than the page's own (an auxiliary browsing
    /// context this page hosts), which its documents are laid out in.
    pub popup_viewport: Option<(f32, f32)>,
    /// HTML "is popup": window.open() asked for a minimal popup window (the bars it shows).
    pub is_popup: bool,
    /// Where the window of a top-level traversable other than the page's own sits on the screen
    /// (its view's, as the browser says; `None`: the page's own window's).
    pub screen: Option<super::window::ScreenPlacement>,
    /// A top-level context's browsing context group (0: the page's own; one `window.open()` opened
    /// with `noopener` starts another).
    pub group: u32,
    /// HTML "popup sandboxing flag set" of a top-level browsing context: the flags its opener's
    /// document had when it opened it (without `allow-popups-to-escape-sandbox`), which each of its
    /// documents starts with.
    pub(crate) popup_sandbox: Option<crate::csp::Sandbox>,
    /// The response the navigation under way commits needs a browsing context group switch (its
    /// opener policy's): the document it brings gets a browsing context of a new group.
    pub(crate) group_switch: bool,
    /// The history handling of the navigation under way, which its document takes when it
    /// becomes active (`Engine::history_commit`).
    pub history_handling: HistoryHandling,
}

/// The navigation a lazy loaded iframe resumes with.
pub enum LazyNavigation {
    /// A URL, with the referrer policy the container's `referrerpolicy` gave it when it was
    /// deferred.
    Url(url::Url, snowlark_net::ReferrerPolicy),
    Srcdoc(String),
}

/// What "the rules for choosing a navigable" chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Navigable {
    Context(ContextId),
    /// A new top-level traversable: a new tab, for the embedder.
    NewTab,
    None,
}

/// A planned navigation: the URL, the body and its content type of a POST (form submission), the
/// document that started it (none: the user) and whether it submits a form.
#[derive(Clone, Debug, PartialEq)]
pub struct NavigationRequest {
    pub url: url::Url,
    pub post: Option<(Vec<u8>, String)>,
    pub source: Option<NodeId>,
    pub form: bool,
    /// HTML "reload", not a navigation to a new document.
    pub reload: bool,
    /// HTML "navigate" its `referrerPolicy` argument: `Empty` (the source document's policy then),
    /// a link's `referrerpolicy` attribute, `NoReferrer` for `noreferrer`.
    pub referrer_policy: snowlark_net::ReferrerPolicy,
    /// The navigation's history handling.
    pub history: HistoryHandling,
}

/// HTML "navigation history behavior" (and a traversal's own navigations): what the new document
/// does to its navigable's session history entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryHandling {
    /// "auto": "push", or "replace" for the initial `about:blank` document and the same URL.
    #[default]
    Auto,
    Push,
    Replace,
    /// A reload: the entry stays, with a new document.
    Reload,
    /// A traversal to the navigable's current entry, whose document was gone: the entry stays,
    /// with the new document.
    Traverse,
    /// The embedder's own top-level navigation in the headless modes: the traversable's session
    /// history starts again with the new document's entry.
    New,
}

#[derive(Default)]
pub struct Frames {
    pub list: Vec<BrowsingContext>,
    pub by_window: rustc_hash::FxHashMap<Handle, ContextId>,
    pub by_document: rustc_hash::FxHashMap<NodeId, ContextId>,
    pub by_container: rustc_hash::FxHashMap<NodeId, ContextId>,
    /// Window singletons (Location, Navigator, the custom element registry...): (window, name) → object.
    pub singletons: HashMap<(Handle, &'static str), Handle>,
    /// The origin and domain of each Window that is not freed, also of those a navigation replaced:
    /// their code goes on with the origin it had.
    pub origins: FxHashMap<Handle, OriginDomain>,
    /// The Windows a navigation replaced, with their browsing context and their associated document.
    pub replaced: FxHashMap<Handle, (ContextId, NodeId)>,
    /// The origin each `about:` document of a browsing context took when it was made.
    pub inherited_origins: FxHashMap<NodeId, Inherited>,
    /// The iframe referrer policy each document of a child navigable was made with (HTML §3.1.3
    /// "internal ancestor origin objects list creation steps", its `referrerPolicy`).
    pub ancestor_policies: FxHashMap<NodeId, snowlark_net::ReferrerPolicy>,
    /// What cross-origin functions hold of their object ([`rt::Host::cross_origin_token`]).
    pub tokens: Tokens,
    /// The last browsing context group number given out.
    pub last_group: u32,
    /// The last number given to a new opaque origin ([`Engine::new_opaque_origin`]).
    pub opaque_serial: u64,
    /// Windows whose browsing context a browsing context group switch left (HTML "obtain a browsing
    /// context to use for a navigation response"): a WindowProxy of them reports `closed`.
    pub severed: rustc_hash::FxHashSet<Handle>,
}

/// The origin an `about:` document took when it was made (HTML §7.4.2.3 "create and initialize a
/// Document object", "determining the origin"): the initiator's of the navigation to it, else its
/// parent document's. It is fixed then: the parent's later navigations do not change it.
#[derive(Clone, Debug)]
pub struct Inherited {
    pub origin: String,
    /// The same origin as snowlark-net sees it (a sandboxed creator's opaque one included).
    pub net_origin: url::Origin,
    /// The URL that stands for the creator in requests (the client's URL, the referrer).
    pub url: url::Url,
    /// Taken from the parent, whose origin this document then shares for `document.domain`.
    pub from_parent: bool,
}

/// Numbers for Windows and Locations, never reused: a handle's slot is.
#[derive(Default)]
pub struct Tokens {
    last: u64,
    of_object: FxHashMap<Handle, u64>,
    objects: FxHashMap<u64, Handle>,
}

impl Tokens {
    pub fn of(&mut self, object: Handle) -> u64 {
        if let Some(&t) = self.of_object.get(&object) {
            return t;
        }
        self.last += 1;
        self.of_object.insert(object, self.last);
        self.objects.insert(self.last, object);
        self.last
    }

    pub fn object(&self, token: u64) -> Option<Handle> {
        self.objects.get(&token).copied()
    }

    pub fn forget(&mut self, object: Handle) {
        if let Some(t) = self.of_object.remove(&object) {
            self.objects.remove(&t);
        }
    }
}

/// The top-level browsing context above `id` (itself for a top-level one).
/// HTML "obtain a browsing context to use for a navigation response" with a browsing context group
/// switch, for the navigable of context `id` whose Window was `old_window` (none: it keeps its
/// WindowProxy): the old browsing context (its Windows) is left behind, so its WindowProxy reports
/// `closed`; the new one is in a group of its
/// own, with no opener and no name, and the contexts the old one opened have no opener now.
fn sever_browsing_context(f: &mut Frames, id: ContextId, old_window: Option<Handle>) {
    if let Some(old_window) = old_window {
        let olds: Vec<Handle> = f
            .replaced
            .iter()
            .filter(|(_, &(c, _))| c == id)
            .map(|(&w, _)| w)
            .chain(std::iter::once(old_window))
            .filter(|&w| w != f.list[id].window)
            .collect();
        f.severed.extend(olds);
    }
    f.last_group += 1;
    let group = f.last_group;
    let b = &mut f.list[id];
    b.group = group;
    b.opener = None;
    b.name.clear();
    for c in f.list.iter_mut() {
        if c.opener == Some(id) {
            c.opener = None;
        }
    }
}

pub(crate) fn top_of(f: &Frames, mut id: ContextId) -> ContextId {
    while let Some(p) = f.list[id].parent {
        id = p;
    }
    id
}

/// Makes a realm (a V8 context) whose global object is the Window `window` and installs the bindings
/// in it. The isolate went through `init_isolate` already (the page did that once).
pub(crate) fn create_realm(
    scope: &mut v8::PinScope<'_, '_>,
    window: Handle,
    origin: &str,
) -> v8::Global<v8::Context> {
    create_realm_for(
        scope,
        window,
        origin,
        snowlark_bindings::generated::GlobalKind::Window,
        None,
    )
}

/// [`create_realm`] for a kind of global (a Window or a dedicated worker's global scope), going on
/// with the global proxy `proxy` (a WindowProxy) when given.
/// The `[CachedAccessor]` key of `Window.document`.
const WINDOW_DOCUMENT: &str = "Window.document";

pub(crate) fn create_realm_for(
    scope: &mut v8::PinScope<'_, '_>,
    window: Handle,
    origin: &str,
    kind: snowlark_bindings::generated::GlobalKind,
    proxy: Option<v8::Local<v8::Object>>,
) -> v8::Global<v8::Context> {
    let global = snowlark_bindings::generated::global_template_for(scope, kind)
        .expect("the global template");
    let context = v8::Context::new(
        scope,
        v8::ContextOptions {
            global_template: Some(global),
            global_object: proxy.map(Into::into),
            ..Default::default()
        },
    );
    let token = security_token(scope, origin);
    context.set_security_token(token);
    let scope = &mut v8::ContextScope::new(scope, context);
    // (V8 refuses every call while the isolate terminates script, as the runner's watchdog or a
    // closing tab makes it: the realm is made without its interface objects then, and nothing
    // runs in it)
    if snowlark_bindings::generated::install_context_for::<Engine>(scope, Some(window), kind)
        .is_none()
    {
        eprintln!("[snowlark] a realm was made while script was terminating: it has no bindings");
    }
    hide_shared_array_buffer(scope, context);
    // V8 stops the process if `document` is read before this private exists, so it exists from here
    // on, whatever else happens; the document itself comes once the realm is in place
    // (`publish_window_document`). Nothing can run in this realm before that.
    if matches!(kind, snowlark_bindings::generated::GlobalKind::Window) {
        rt::without_termination(scope, |scope| {
            let undefined = v8::undefined(scope);
            let global = context.global(scope);
            rt::set_cached_attribute(scope, global, WINDOW_DOCUMENT, undefined.into());
        });
    }
    rt::register_realm::<Engine>(scope);
    v8::Global::new(scope, context)
}

/// HTML §8.1.3.1 "cross-origin isolated capability": without it, the realm's global object has no
/// `SharedArrayBuffer` property (HTML §2.7 / ECMAScript's SharedArrayBuffer constructor is exposed only
/// to cross-origin isolated agents). Snowlark does not make cross-origin isolated agent clusters yet
/// (`crossOriginIsolated` is false everywhere), so no realm exposes it.
fn hide_shared_array_buffer(scope: &mut v8::PinScope<'_, '_>, context: v8::Local<v8::Context>) {
    let global = context.global(scope);
    let key = rt::name(scope, "SharedArrayBuffer");
    global.delete(scope, key.into());
}

/// What a Window's realm has as its security token: its origin, or with a domain set a string of its
/// own, since same origin-domain realms then need not have equal origins and equal origins need not
/// be same origin-domain.
pub(crate) fn realm_token(window: Handle, origin: &OriginDomain) -> String {
    match origin.domain {
        Some(_) => format!("domain:{}", window.get()),
        None => origin.identity.clone(),
    }
}

/// The security token of realms of `origin`: V8 compares tokens by identity, so equal origins must give
/// the same object, which an internalized string is.
fn security_token<'s>(scope: &v8::PinScope<'s, '_>, origin: &str) -> v8::Local<'s, v8::Value> {
    v8::String::new_from_utf8(scope, origin.as_bytes(), v8::NewStringType::Internalized)
        .expect("a string")
        .into()
}

impl Engine {
    // ----- lookups -------------------------------------------------------------------------------------

    /// The top-level window.
    pub fn window(&self) -> Option<Handle> {
        self.frames.borrow().list.first().map(|b| b.window)
    }

    /// The top-level window's document.
    /// The top-level document the embedder shows: the page's own, or that of the hosted window
    /// whose view is in place ([`Engine::begin_view`]).
    pub fn shown_document(&self) -> Option<NodeId> {
        match self.view.get() {
            Some((doc, _)) => Some(doc),
            None => self.document(),
        }
    }

    /// Whether `doc`'s current document readiness is "complete".
    pub fn document_complete(&self, doc: NodeId) -> bool {
        self.doc_state(doc, |s| s.ready_state == DocumentReadyState::Complete)
    }

    /// The page's own top-level document.
    pub fn document(&self) -> Option<NodeId> {
        self.frames.borrow().list.first().map(|b| b.document)
    }

    pub(crate) fn context_of_window(&self, window: Handle) -> Option<ContextId> {
        self.frames.borrow().by_window.get(&window).copied()
    }

    /// The browsing context whose active document `doc` is.
    pub(crate) fn context_of_document(&self, doc: NodeId) -> Option<ContextId> {
        let f = self.frames.borrow();
        let id = *f.by_document.get(&doc)?;
        (f.list[id].document == doc).then_some(id)
    }

    /// The Window whose associated document is `doc`, if it is an active document.
    pub(crate) fn window_of_document(&self, doc: NodeId) -> Option<Handle> {
        let id = self.context_of_document(doc)?;
        Some(self.frames.borrow().list[id].window)
    }

    /// The document of a window that is its browsing context's: `None` for one a navigation replaced.
    pub(crate) fn document_of_window(&self, window: Handle) -> Option<NodeId> {
        let id = self.context_of_window(window)?;
        Some(self.frames.borrow().list[id].document)
    }

    /// A window's associated document (HTML §7.2.2), which stays its own after a navigation replaced
    /// the window: code of that realm can go on running, and must never get another document for it.
    pub(crate) fn associated_document(&self, window: Handle) -> Option<NodeId> {
        let f = self.frames.borrow();
        match f.by_window.get(&window) {
            Some(&id) => Some(f.list[id].document),
            None => f.replaced.get(&window).map(|&(_, doc)| doc),
        }
    }

    /// The window of the realm the code calling us runs in (the current global object), also when a
    /// navigation replaced it since.
    pub(crate) fn current_window(&self, cx: &mut Cx) -> Option<Handle> {
        let h = rt::realm_global(cx.scope().get_current_context())?;
        let f = self.frames.borrow();
        (f.by_window.contains_key(&h) || f.replaced.contains_key(&h)).then_some(h)
    }

    /// The global object of the incumbent realm (HTML §8.1.3.5), a Window also when a navigation
    /// replaced it since.
    pub(crate) fn incumbent_window(&self, cx: &mut Cx) -> Option<Handle> {
        rt::realm_global(snowlark_js::access_check::incumbent_context(cx.scope()))
    }

    /// The associated document of the entry realm's global object (HTML §8.1.3.4).
    pub(crate) fn entry_document(&self, cx: &mut Cx) -> rt::Result<NodeId> {
        match rt::realm_global(cx.scope().get_entered_or_microtask_context())
            .and_then(|w| self.associated_document(w))
        {
            Some(doc) => Ok(doc),
            None => self.current_document(cx),
        }
    }

    /// The current global object's associated document (constructors such as `new Text()`). A realm
    /// with no document left is an error, never another realm's document: that one could be of
    /// another origin.
    pub(crate) fn current_document(&self, cx: &mut Cx) -> rt::Result<NodeId> {
        self.current_window(cx)
            .and_then(|w| self.associated_document(w))
            .ok_or_else(|| {
                super::exception(
                    rt::ExceptionName::InvalidStateError,
                    "The realm has no document",
                )
            })
    }

    /// Whether `doc` is the active document of a browsing context that was not discarded.
    pub(crate) fn is_active_document(&self, doc: NodeId) -> bool {
        let f = self.frames.borrow();
        f.by_document
            .get(&doc)
            .is_some_and(|&id| f.list[id].document == doc && !f.list[id].discarded)
    }

    /// Whether `doc` was destroyed: its browsing context was discarded, a navigation replaced it
    /// (nothing makes a replaced document active again, so unloading it destroys it), or it was
    /// freed. A task can hold the id of a document past all three, and a collection decides when the
    /// last happens.
    pub(crate) fn document_destroyed(&self, doc: NodeId) -> bool {
        if !self.dom().is_alive(doc) || self.gc.borrow().unloaded.contains(&doc) {
            return true;
        }
        let f = self.frames.borrow();
        f.by_document
            .get(&doc)
            .is_some_and(|&id| f.list[id].discarded)
    }

    /// HTML §8.1.4.4 "check if we can run script", for a realm whose global object is `global`: no
    /// for a Window whose associated document is not fully active. A document's child navigables are
    /// destroyed with it, so a Window's document is fully active exactly when the Window is still its
    /// browsing context's and that context was not discarded. Any other global (a worker's) may run.
    pub(crate) fn can_run_script(&self, global: Handle) -> bool {
        let f = self.frames.borrow();
        match f.by_window.get(&global) {
            Some(&id) => !f.list[id].discarded,
            None => !f.replaced.contains_key(&global),
        }
    }

    /// A singleton object of `window`, made on first use.
    pub(crate) fn window_singleton(
        &self,
        window: Handle,
        key: &'static str,
        make: impl FnOnce() -> Object,
    ) -> Handle {
        if let Some(h) = self.frames.borrow().singletons.get(&(window, key)) {
            return *h;
        }
        let h = self.alloc(make());
        self.frames.borrow_mut().singletons.insert((window, key), h);
        h
    }

    /// The realm of a browsing context, for entering it.
    pub(crate) fn realm_of_context(&self, id: ContextId) -> Option<v8::Global<v8::Context>> {
        self.frames.borrow().list.get(id)?.realm.clone()
    }

    /// The realm the embedder's calls enter: the page's own top-level one, or, once that was
    /// destroyed while windows the page hosts go on ([`Engine::close_own_top_level`]), the first
    /// of theirs.
    pub(crate) fn entry_realm(&self) -> Option<v8::Global<v8::Context>> {
        let f = self.frames.borrow();
        f.list.first().and_then(|b| b.realm.clone()).or_else(|| {
            f.list
                .iter()
                .filter(|b| b.parent.is_none() && !b.discarded)
                .find_map(|b| b.realm.clone())
        })
    }

    /// Runs `f` in the realm of `doc`'s browsing context (the top-level realm when `doc` has none).
    pub(crate) fn in_document_realm<R>(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        f: impl FnOnce(&mut Cx) -> R,
    ) -> R {
        let realm = self
            .context_of_document(doc)
            .and_then(|id| self.realm_of_context(id));
        self.in_realm(cx, realm, f)
    }

    /// Runs `f` with `realm` (when given) as the current context.
    pub(crate) fn in_realm<R>(
        &self,
        cx: &mut Cx,
        realm: Option<v8::Global<v8::Context>>,
        f: impl FnOnce(&mut Cx) -> R,
    ) -> R {
        let Some(realm) = realm else {
            return f(cx);
        };
        let scope = cx.scope();
        let ctx = v8::Local::new(scope, &realm);
        if ctx == scope.get_current_context() {
            return f(cx);
        }
        let scope = &mut v8::ContextScope::new(scope, ctx);
        let mut cx = Cx::new(scope);
        f(&mut cx)
    }

    // ----- creation --------------------------------------------------------------------------------------

    /// Makes the top-level browsing context's Window for `document` (the page makes its realm).
    pub fn create_window(&self, document: NodeId) -> Handle {
        self.new_context(document, None, None)
    }

    fn new_context(
        &self,
        document: NodeId,
        parent: Option<ContextId>,
        container: Option<NodeId>,
    ) -> Handle {
        let container_referrer_policy = container
            .map(|c| self.iframe_referrer_policy(c))
            .unwrap_or(snowlark_net::ReferrerPolicy::Empty);
        let mut f = self.frames.borrow_mut();
        let id = f.list.len();
        let window = {
            drop(f);
            let h = self.alloc(Object::Window);
            f = self.frames.borrow_mut();
            h
        };
        if parent.is_some() {
            f.ancestor_policies
                .insert(document, container_referrer_policy);
        }
        f.list.push(BrowsingContext {
            window,
            document,
            realm: None,
            parent,
            container,
            discarded: false,
            initial_about_blank: true,
            navigation_request: None,
            name: String::new(),
            pending_load_event: false,
            lazy_load: None,
            lazily_loaded: false,
            navigations: 0,
            object_fetch: None,
            initiator_origin: None,
            sandbox_attribute: None,
            container_referrer_policy,
            popup_sandbox: None,
            group_switch: false,
            opener: None,
            created_by_web_content: false,
            closing: false,
            popup_viewport: None,
            is_popup: false,
            screen: None,
            group: 0,
            history_handling: Default::default(),
        });
        f.by_window.insert(window, id);
        f.by_document.insert(document, id);
        if let Some(c) = container {
            f.by_container.insert(c, id);
        }
        drop(f);
        self.define_inherited_policy(document);
        // (a new document's origin is its creator's; so is its domain, if that set one)
        let mut origin = OriginDomain::new(self.origin_of_document(document));
        let mut f = self.frames.borrow_mut();
        origin.domain = parent
            .map(|p| f.list[p].window)
            .and_then(|w| f.origins.get(&w))
            .and_then(|o| o.domain.clone());
        f.origins.insert(window, origin);
        window
    }

    /// Makes a worker engine's global object the top-level "window" over `document` (the worker's
    /// unexposed document, which carries its URL).
    pub(crate) fn create_worker_global(
        &self,
        document: NodeId,
        name: &str,
        url: url::Url,
    ) -> Handle {
        let global = self.alloc(Object::WorkerGlobal(crate::worker::WorkerGlobalData {
            name: name.to_owned(),
            url,
        }));
        let mut f = self.frames.borrow_mut();
        let id = f.list.len();
        f.list.push(BrowsingContext {
            window: global,
            document,
            realm: None,
            parent: None,
            container: None,
            discarded: false,
            initial_about_blank: false,
            navigation_request: None,
            name: name.to_owned(),
            pending_load_event: false,
            lazy_load: None,
            lazily_loaded: false,
            navigations: 0,
            object_fetch: None,
            initiator_origin: None,
            sandbox_attribute: None,
            container_referrer_policy: snowlark_net::ReferrerPolicy::Empty,
            popup_sandbox: None,
            group_switch: false,
            opener: None,
            created_by_web_content: false,
            closing: false,
            popup_viewport: None,
            is_popup: false,
            screen: None,
            group: 0,
            history_handling: Default::default(),
        });
        f.by_window.insert(global, id);
        f.by_document.insert(document, id);
        global
    }

    /// Gives the top-level context its realm (the page makes it).
    pub fn set_top_realm(&self, realm: v8::Global<v8::Context>) {
        self.frames.borrow_mut().list[TOP].realm = Some(realm);
    }

    /// Gives script `window`'s document: V8 reads `document` from a private of the global, so every
    /// change of a Window's document comes through here.
    pub(crate) fn publish_window_document(&self, scope: &mut v8::PinScope<'_, '_>, window: Handle) {
        let Some(doc) = self.associated_document(window) else {
            return;
        };
        let Some(realm) = self
            .context_of_window(window)
            .and_then(|id| self.realm_of_context(id))
        else {
            return;
        };
        let context = v8::Local::new(scope, &realm);
        rt::without_termination(scope, |scope| {
            // (in the window's realm: the document's wrapper belongs there)
            let scope = &mut v8::ContextScope::new(scope, context);
            if let Some(wrapper) = rt::wrap(scope, node_handle(doc)) {
                let global = context.global(scope);
                rt::set_cached_attribute(scope, global, WINDOW_DOCUMENT, wrapper.into());
            }
        });
    }

    /// Makes `doc` the active document of the browsing context `id`. A context leaving its initial
    /// `about:blank` document for a same origin-domain one keeps its Window; otherwise it gets a new
    /// Window and realm. (So does a top-level context, whose initial document never ran page script:
    /// the embedder's preparations of its realm hold whatever the page's origin.)
    pub(crate) fn set_context_document(&self, cx: &mut Cx, id: ContextId, doc: NodeId) {
        // HTML "unload a document and its descendants": the old document and those of its child
        // navigables fire `pagehide` and `unload` before the new one takes their place.
        let previous = self.frames.borrow().list[id].document;
        if previous != doc {
            self.unload_document_and_descendants(cx, previous);
            // (the new document's session history entry, while the navigable still shows the
            // old one)
            self.history_commit(id, doc);
        }
        let (old_doc, initial, old_window, initiator, parent_doc) = {
            let mut f = self.frames.borrow_mut();
            let old_doc = f.list[id].document;
            let initial = f.list[id].initial_about_blank;
            f.by_document.remove(&old_doc);
            f.by_document.insert(doc, id);
            f.list[id].document = doc;
            f.list[id].initial_about_blank = false;
            let initiator = f.list[id].initiator_origin.take();
            let parent_doc = f.list[id].parent.map(|p| f.list[p].document);
            if parent_doc.is_some() {
                let policy = f.list[id].container_referrer_policy;
                f.ancestor_policies.insert(doc, policy);
            }
            (old_doc, initial, f.list[id].window, initiator, parent_doc)
        };
        if self.dom().document_url(doc).starts_with("about:") {
            self.inherit_origin(doc, initiator, parent_doc);
        }
        // HTML "destroy a document and its descendants" (the old document is unloaded, and nothing
        // makes it active again): its child navigables go with it. Left alive, their script would go
        // on as documents of a page that is gone, under the parent's next document.
        if old_doc != doc {
            let children: Vec<ContextId> = {
                let f = self.frames.borrow();
                f.list
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.parent == Some(id) && !b.discarded)
                    .map(|(c, _)| c)
                    .collect()
            };
            self.discard_contexts(&children);
        }
        let frame = {
            let f = self.frames.borrow();
            let parent = f.list[id].parent.map(|p| f.list[p].document);
            parent.map(|p| (f.list[id].sandbox_attribute.clone(), p))
        };
        if let Some((sandbox, parent_doc)) = frame {
            self.apply_frame_sandbox(doc, sandbox.as_deref(), parent_doc);
        } else if let Some(sandbox) = self.frames.borrow().list[id].popup_sandbox {
            self.apply_popup_sandbox(doc, sandbox);
        }
        let switch = std::mem::take(&mut self.frames.borrow_mut().list[id].group_switch);
        // (the page's own top-level traversable keeps its WindowProxy, which the embedder holds: only
        // its relations to the windows it hosts change)
        let fresh = switch && id != TOP;
        self.define_inherited_policy(doc);
        let mut origin = OriginDomain::new(self.origin_of_document(doc));
        // (an `about:` document shares its creator's origin, and with it the domain)
        if self.dom().document_url(doc).starts_with("about:") {
            if let Some(p) = self.parent_document_of(doc) {
                origin.domain = self.document_domain(p);
            }
        }
        let reuse = !fresh
            && initial
            && (id == TOP
                || self
                    .frames
                    .borrow()
                    .origins
                    .get(&old_window)
                    .is_some_and(|o| o.same_origin_domain(&origin)));
        if old_doc != doc {
            // (the unloading document cleanup steps)
            self.fullscreen_unloaded(cx, old_doc);
            self.gc_document_unloaded(cx, old_doc);
            self.audio_window_unloaded(old_window);
            if !reuse {
                // (the old Window's IndexedDB connections close with its execution context)
                self.idb_global_destroyed(old_window);
            }
        }
        self.doc_state(doc, |s| s.has_browsing_context = true);
        if id == TOP {
            self.render_attach(doc);
        } else {
            let (container, popup) = {
                let f = self.frames.borrow();
                (f.list[id].container, f.list[id].popup_viewport)
            };
            if let Some(container) = container {
                self.render_attach_frame(doc, container, Some(old_doc));
            } else if let Some(viewport) = popup {
                self.render_attach_popup(doc, viewport, Some(old_doc));
            }
        }
        if reuse {
            if switch {
                sever_browsing_context(&mut self.frames.borrow_mut(), id, None);
            }
            // (the Window's associated Document is another: so are its History object and its
            // ancestor origins list, which are the document's)
            {
                let mut f = self.frames.borrow_mut();
                f.singletons.remove(&(old_window, "ancestorOrigins"));
                f.singletons.remove(&(old_window, "history"));
            }
            // The kept realm takes the new document's origin as its security token.
            self.set_window_origin(cx, id, origin);
            self.publish_window_document(cx.scope(), old_window);
            self.content_document_created(cx, doc);
            return;
        }
        // A new Window and realm for the new document, behind the context's WindowProxy.
        let window = self.alloc(Object::Window);
        let old_realm = self.realm_of_context(id);
        let scope = cx.scope();
        // (after a browsing context group switch the navigable's new browsing context has a WindowProxy
        // of its own: the old one stays with the old Window, which script elsewhere may hold)
        let proxy = old_realm.filter(|_| !fresh).map(|realm| {
            let context = v8::Local::new(scope, &realm);
            rt::note_replaced_window(scope, old_window, context);
            context.global(scope)
        });
        let realm = create_realm_for(
            scope,
            window,
            &origin.identity,
            snowlark_bindings::generated::GlobalKind::Window,
            proxy,
        );
        let mut f = self.frames.borrow_mut();
        f.by_window.remove(&old_window);
        f.by_window.insert(window, id);
        f.replaced.insert(old_window, (id, old_doc));
        f.origins.insert(window, origin);
        f.list[id].window = window;
        f.list[id].realm = Some(realm);
        if switch {
            sever_browsing_context(&mut f, id, fresh.then_some(old_window));
        }
        drop(f);
        self.publish_window_document(cx.scope(), window);
        // The old Window and its document live on only while script reaches them, as a discarded
        // browsing context's do (engine/gc.rs).
        if old_window != window && old_doc != doc {
            self.gc
                .borrow_mut()
                .retired_pending
                .push((old_window, old_doc));
        }
        // Content blocking: the document's cosmetic filters and scriptlets, before anything of it
        // is parsed or run.
        self.content_document_created(cx, doc);
    }

    /// The documents of the child navigables of `doc`'s navigable, in tree order.
    fn child_documents(&self, doc: NodeId) -> Vec<NodeId> {
        let Some(id) = self.context_of_document(doc) else {
            return Vec::new();
        };
        let f = self.frames.borrow();
        let mut children: Vec<(NodeId, NodeId)> = f
            .list
            .iter()
            .filter(|b| b.parent == Some(id) && !b.discarded && b.document != doc)
            .filter_map(|b| b.container.map(|c| (c, b.document)))
            .collect();
        drop(f);
        let dom = self.dom();
        children.sort_by(|a, b| dom.tree_order(a.0, b.0));
        children.into_iter().map(|(_, d)| d).collect()
    }

    /// HTML "unload a document and its descendants": the child navigables' documents first, then
    /// "unload" `doc`: `pagehide` while its page is showing, then `unload` (the document is not
    /// kept for history traversal), with its unload counter raised meanwhile.
    pub(crate) fn unload_document_and_descendants(&self, cx: &mut Cx, doc: NodeId) {
        for child in self.child_documents(doc) {
            self.unload_document_and_descendants(cx, child);
        }
        let Some(window) = self.window_of_document(doc) else {
            return;
        };
        if self.document_destroyed(doc) {
            return;
        }
        let showing = self.doc_state(doc, |s| {
            s.unload_counter += 1;
            std::mem::replace(&mut s.page_showing, false)
        });
        if showing {
            let ev = self.trusted_event(
                snowlark_bindings::InterfaceId::PageTransitionEvent,
                "pagehide",
                false,
                false,
                super::events::EventExt::PageTransition(false),
            );
            self.dispatch(cx, ev, window, true);
            // ("unload" step 10.3)
            self.update_visibility_state(cx, doc, false);
        }
        let ev = self.trusted_event(
            snowlark_bindings::InterfaceId::Event,
            "unload",
            false,
            false,
            super::events::EventExt::None,
        );
        self.dispatch(cx, ev, window, true);
        self.doc_state(doc, |s| s.unload_counter -= 1);
    }

    /// HTML "checking if unloading is canceled", for a navigation of `id`: `beforeunload` at its
    /// active document and those of its descendants, in tree order, each with its unload counter
    /// raised. Without a prompt to show (there is no sticky activation to ask the user with), the
    /// navigation goes on whatever the handlers say.
    fn fire_beforeunload(&self, cx: &mut Cx, id: ContextId) {
        let doc = self.frames.borrow().list[id].document;
        let mut docs = vec![doc];
        let mut i = 0;
        while i < docs.len() {
            let children = self.child_documents(docs[i]);
            docs.splice(i + 1..i + 1, children);
            i += 1;
        }
        for d in docs {
            let Some(window) = self.window_of_document(d) else {
                continue;
            };
            if self.document_destroyed(d) {
                continue;
            }
            self.doc_state(d, |s| s.unload_counter += 1);
            let ev = self.trusted_event(
                snowlark_bindings::InterfaceId::BeforeUnloadEvent,
                "beforeunload",
                false,
                true,
                super::events::EventExt::None,
            );
            self.dispatch(cx, ev, window, true);
            self.doc_state(d, |s| s.unload_counter -= 1);
        }
    }

    /// The Window of the browsing context `id` has the origin `origin`: its entry and its realm's
    /// security token follow.
    fn set_window_origin(&self, cx: &mut Cx, id: ContextId, origin: OriginDomain) {
        let window = self.frames.borrow().list[id].window;
        let token = realm_token(window, &origin);
        self.frames.borrow_mut().origins.insert(window, origin);
        if let Some(realm) = self.realm_of_context(id) {
            let scope = cx.scope();
            let ctx = v8::Local::new(scope, &realm);
            let token = security_token(scope, &token);
            ctx.set_security_token(token);
        }
    }

    /// The document's origin changed (a sandbox gave it an opaque one).
    pub(crate) fn document_origin_changed(&self, cx: &mut Cx, doc: NodeId) {
        if let Some(id) = self.context_of_document(doc) {
            let origin = OriginDomain::new(self.origin_of_document(doc));
            self.set_window_origin(cx, id, origin);
        }
    }

    /// HTML `document.domain`'s setter, last step: the document's origin's domain is `domain`.
    pub(crate) fn set_document_domain(&self, cx: &mut Cx, doc: NodeId, domain: String) {
        // (`about:` documents have their creator's origin itself, not a copy: all of them change)
        let root = self.origin_alias_root(doc);
        let ids: Vec<ContextId> = {
            let f = self.frames.borrow();
            let docs: Vec<NodeId> = f.list.iter().map(|b| b.document).collect();
            let discarded: Vec<bool> = f.list.iter().map(|b| b.discarded).collect();
            drop(f);
            docs.iter()
                .enumerate()
                .filter(|&(i, &d)| !discarded[i] && self.origin_alias_root(d) == root)
                .map(|(i, _)| i)
                .collect()
        };
        for id in ids {
            let window = self.frames.borrow().list[id].window;
            let origin = self.frames.borrow().origins.get(&window).cloned();
            if let Some(mut origin) = origin {
                origin.domain = Some(domain.clone());
                self.set_window_origin(cx, id, origin);
            }
        }
    }

    /// The document whose origin `doc` shares: through its parents while it is an `about:` document
    /// that took theirs.
    fn origin_alias_root(&self, doc: NodeId) -> NodeId {
        let mut d = doc;
        loop {
            let about = self.dom().document_url(d).starts_with("about:");
            let from_initiator = self
                .frames
                .borrow()
                .inherited_origins
                .get(&d)
                .is_some_and(|i| !i.from_parent);
            match self
                .parent_document_of(d)
                .filter(|_| about && !from_initiator)
            {
                Some(p) if self.origin_of_document(p) == self.origin_of_document(d) => d = p,
                _ => return d,
            }
        }
    }

    /// The domain of the document's origin, when `document.domain` set one.
    pub(crate) fn document_domain(&self, doc: NodeId) -> Option<String> {
        let window = self.window_of_document(doc)?;
        self.frames.borrow().origins.get(&window)?.domain.clone()
    }

    /// A document's origin and domain: its Window's, if it has one.
    fn document_origin_domain(&self, doc: NodeId) -> OriginDomain {
        self.window_of_document(doc)
            .and_then(|w| self.frames.borrow().origins.get(&w).cloned())
            .unwrap_or_else(|| OriginDomain::new(self.origin_of_document(doc)))
    }

    /// HTML "same origin-domain" for two documents.
    pub(crate) fn same_origin_domain_documents(&self, a: NodeId, b: NodeId) -> bool {
        self.document_origin_domain(a)
            .same_origin_domain(&self.document_origin_domain(b))
    }

    /// `IsPlatformObjectSameOrigin`: whether code of the realm of the Window `accessing` may touch
    /// `target`, a Window or a Location. No when the state is busy or either is unknown.
    pub(crate) fn window_may_access(&self, accessing: Handle, target: Handle) -> bool {
        let Ok(objects) = self.objects.try_borrow() else {
            return false;
        };
        let target = match objects.get((target.get() as usize).wrapping_sub(1)) {
            Some(Object::Window) => target,
            Some(Object::Location { window }) => *window,
            _ => return false,
        };
        if accessing == target {
            return true;
        }
        let Ok(f) = self.frames.try_borrow() else {
            return false;
        };
        match (f.origins.get(&accessing), f.origins.get(&target)) {
            (Some(a), Some(t)) => a.same_origin_domain(t),
            _ => false,
        }
    }

    /// The first of `window`'s child contexts named `name`, when `window`'s named access may return
    /// it: HTML §7.2.3.3's document-tree child navigable target name property set (a child showing a
    /// document same origin with `window`'s), or, as the engines keep from the earlier text of that
    /// set, a child whose container's `name` attribute is that name, whatever its origin. Sites rely on
    /// the second: a mail app's frames of another origin find their sibling through the cross-origin
    /// parent by its container's name. WPT `cross-origin-named-access` asserts
    /// the current text and fails 2 of 3 for it, as in the other engines.
    pub(crate) fn named_child_window(&self, window: Handle, name: &str) -> Option<Handle> {
        if name.is_empty() {
            return None;
        }
        let child = self
            .child_windows(window)
            .into_iter()
            .find(|&w| self.window_name(w) == name)?;
        let same_origin = {
            let f = self.frames.borrow();
            let (o, c) = (f.origins.get(&window)?, f.origins.get(&child)?);
            o.same_origin(c)
        };
        let container_named = || {
            self.frame_element_of(child)
                .is_some_and(|el| self.dom().attr(el, &ns!(), &local_name!("name")) == Some(name))
        };
        (same_origin || container_named()).then_some(child)
    }

    /// The origin a Window's script speaks with, serialized (HTML "serialization of an origin").
    pub(crate) fn window_origin_serialization(&self, window: Handle) -> String {
        self.frames
            .borrow()
            .origins
            .get(&window)
            .map_or("null", |o| o.serialization())
            .to_owned()
    }

    /// Whether a Window's origin is the one a URL parsed for `postMessage` gives.
    pub(crate) fn window_has_origin(&self, window: Handle, origin: &str) -> bool {
        self.frames
            .borrow()
            .origins
            .get(&window)
            .is_some_and(|o| o.identity == origin)
    }

    /// The identity of a Window's origin.
    pub(crate) fn window_origin(&self, window: Handle) -> Option<String> {
        let f = self.frames.borrow();
        f.origins.get(&window).map(|o| o.identity.clone())
    }

    /// Fixes the origin of `doc`, an `about:` document becoming a browsing context's active one: the
    /// initiator's of the navigation to it, else that of `parent`, its parent's document now. What
    /// the parent navigates to later does not change it.
    pub(crate) fn inherit_origin(
        &self,
        doc: NodeId,
        initiator: Option<Inherited>,
        parent: Option<NodeId>,
    ) {
        let inherited = match (initiator, parent) {
            (Some(i), _) => i,
            (None, Some(p)) => self.origin_taken_from(p, true),
            // (a top-level `about:` document the user opened: its own, as before)
            (None, None) => return,
        };
        self.frames
            .borrow_mut()
            .inherited_origins
            .insert(doc, inherited);
    }

    /// The origin an `about:` document takes from `creator` now.
    pub(crate) fn origin_taken_from(&self, creator: NodeId, from_parent: bool) -> Inherited {
        Inherited {
            origin: self.origin_of_document(creator),
            net_origin: self.net_origin_of_document(creator),
            url: self.effective_document_url(creator),
            from_parent,
        }
    }

    /// The serialized origin of a document's URL (`about:` documents inherit their creator's: the
    /// document that navigated to them, else the parent, as of when they were made).
    pub(crate) fn origin_of_document(&self, doc: NodeId) -> String {
        // (a sandboxed document's opaque origin, unique to it)
        if self.policy.borrow().any && self.sandboxed_origin(doc).is_some() {
            return format!("opaque:sandbox:{}", doc.to_bits());
        }
        if let Some(i) = self.frames.borrow().inherited_origins.get(&doc) {
            return i.origin.clone();
        }
        let url = url::Url::parse(self.dom().document_url(doc)).ok();
        match url {
            Some(u) if u.scheme() == "about" => self
                .parent_document_of(doc)
                .map(|p| self.origin_of_document(p))
                .unwrap_or_else(|| self.top_origin()),
            // (a `data:` document's origin is a new one: no other document has it, whatever its URL)
            Some(u) if u.scheme() == "data" => format!("opaque:data:{}", doc.to_bits()),
            Some(u) => {
                let o = u.origin().ascii_serialization();
                if o == "null" {
                    format!("opaque:{}", u.as_str())
                } else {
                    o
                }
            }
            None => self.top_origin(),
        }
    }

    fn top_origin(&self) -> String {
        let Some(doc) = self.document() else {
            return "null".into();
        };
        let url = url::Url::parse(self.dom().document_url(doc)).ok();
        match url {
            Some(u) if u.scheme() != "about" => u.origin().ascii_serialization(),
            _ => "about:blank".into(),
        }
    }

    /// The parent browsing context's active document of `doc`'s browsing context.
    pub(crate) fn parent_document(&self, doc: NodeId) -> Option<NodeId> {
        self.parent_document_of(doc)
    }

    pub(crate) fn parent_document_of(&self, doc: NodeId) -> Option<NodeId> {
        let f = self.frames.borrow();
        let id = *f.by_document.get(&doc)?;
        let parent = f.list[id].parent?;
        Some(f.list[parent].document)
    }

    // ----- frames (HTML §4.8.5 the iframe element) ---------------------------------------------------------

    /// Whether `el` is an element that contains a child browsing context: an `iframe`, a `frame`, or
    /// an `object` or `embed` whose content is a document (HTML §4.8.6 and §4.8.7 "process" steps;
    /// plugin types get no navigable).
    pub(crate) fn is_frame_container(&self, el: NodeId) -> bool {
        let html = self.html.borrow();
        let known = html.embed_types.get(&el).map(String::as_str);
        let dom = self.dom();
        let pdf = self.pdf_viewer_supported() && !self.plugins_sandboxed(dom.node_document(el));
        dom.is_html_element_named(el, &local_name!("iframe"))
            || dom.is_html_element_named(el, &local_name!("frame"))
            || snowlark_render::source::embeds_document(&dom, el, known, pdf)
    }

    /// HTML §4.8.7: whether an `object` shows its resource rather than its fallback content (it
    /// has a child navigable, or represents an image).
    fn object_shows_resource(&self, object: NodeId) -> bool {
        if self.context_of_container(object).is_some() {
            return true;
        }
        let html = self.html.borrow();
        let known = html.embed_types.get(&object).map(String::as_str);
        snowlark_render::source::embedded_image(&self.dom(), object, known).is_some()
    }

    /// HTML §4.8.6, §4.8.7: an `object` or `embed` with an ancestor `object` that is not showing
    /// its fallback content loads nothing.
    fn inside_object_resource(&self, el: NodeId) -> bool {
        let objects: Vec<NodeId> = {
            let dom = self.dom();
            dom.ancestors(el)
                .filter(|&a| dom.is_html_element_named(a, &local_name!("object")))
                .collect()
        };
        objects.into_iter().any(|o| self.object_shows_resource(o))
    }

    /// HTML §4.8.7: whether an `object` shows a response of type `essence` (as a document or an
    /// image; media types keep the document they make). Any other type is one Snowlark has no
    /// plugin for: the element falls back.
    pub(crate) fn object_shows_type(&self, object: NodeId, essence: &str) -> bool {
        let pdf = self.pdf_viewer_supported()
            && !self.plugins_sandboxed(self.dom().node_document(object));
        essence.is_empty()
            || ["image/", "video/", "audio/"]
                .iter()
                .any(|p| essence.starts_with(p))
            || snowlark_render::source::embeds_document(&self.dom(), object, Some(essence), pdf)
    }

    /// The `object` whose fetch of its `data` the navigation of `id` is: its navigable still shows
    /// the initial `about:blank` it was made with (on the guess that the resource is a document),
    /// or the navigation under way is the one a later `data` change started.
    pub(crate) fn object_fetching(&self, id: ContextId) -> Option<NodeId> {
        let el = {
            let f = self.frames.borrow();
            let b = f.list.get(id)?;
            let fetch = b.initial_about_blank || b.object_fetch == Some(b.navigations);
            if !fetch || b.discarded {
                return None;
            }
            b.container?
        };
        self.dom()
            .is_html_element_named(el, &local_name!("object"))
            .then_some(el)
    }

    /// HTML §4.8.7 the `object` element's steps, "fallback" (after `error` when the URL did not
    /// parse or the load failed): the element represents its children, its navigable goes, and
    /// the `object` and `embed` elements of its fallback content are processed now that it shows
    /// them. With `now`, the navigable goes at once (a fetch's outcome); otherwise in a task (the
    /// attribute processing that made it is still running).
    pub(crate) fn object_fallback(&self, id: ContextId, object: NodeId, error: bool, now: bool) {
        self.cancel_document_fetches(id);
        self.html
            .borrow_mut()
            .embed_types
            .insert(object, String::new());
        self.with_node_renderer(object, |r, st| {
            r.set_embed_type(object, "");
            st.dirty = true;
        });
        if now {
            self.frame_removed(object);
        }
        if error {
            self.queue_element_task_fire(object, "error", false, false);
        }
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            e.frame_removed(object);
            let inner: Vec<NodeId> = {
                let dom = e.dom();
                dom.descendants(object)
                    .filter(|&d| {
                        d != object
                            && (dom.is_html_element_named(d, &local_name!("object"))
                                || dom.is_html_element_named(d, &local_name!("embed")))
                    })
                    .collect()
            };
            for d in inner {
                if e.dom().is_alive(d) && e.is_frame_container(d) {
                    e.frame_inserted(cx, d);
                }
            }
        });
    }

    /// HTML §4.8.6, §4.8.7: a change to an `object`'s or `embed`'s attributes (re)determines what it
    /// represents, in a task on the DOM manipulation task source. When its resource is no longer a
    /// document (an image type, or none it shows), its child navigable is destroyed then: its
    /// document unloads (`pagehide`, `unload`), and the element represents the image or its
    /// fallback content.
    /// `local` is the attribute that changed: an `object`'s `type` counts only while it has no
    /// `data` (an `embed`'s always does).
    pub(crate) fn embed_redetermine(&self, el: NodeId, local: &str) {
        let ignored = {
            let dom = self.dom();
            local == "type"
                && dom.is_html_element_named(el, &local_name!("object"))
                && dom.get_attribute(el, "data").is_some()
        };
        if ignored || self.context_of_container(el).is_none() {
            return;
        }
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if !e.dom().is_alive(el) || e.is_frame_container(el) {
                return;
            }
            let Some(id) = e.context_of_container(el) else {
                return;
            };
            e.cancel_document_fetches(id);
            let doc = e.frames.borrow().list[id].document;
            e.unload_document_and_descendants(cx, doc);
            e.frame_removed(el);
        });
    }

    /// The attribute holding a container's URL: `data` for `object`, else `src`.
    fn container_url_attribute(&self, el: NodeId) -> &'static str {
        if self.dom().is_html_element_named(el, &local_name!("object")) {
            "data"
        } else {
            "src"
        }
    }

    /// The child browsing context of a container element.
    pub(crate) fn context_of_container(&self, el: NodeId) -> Option<ContextId> {
        let f = self.frames.borrow();
        let id = *f.by_container.get(&el)?;
        (!f.list[id].discarded).then_some(id)
    }

    /// `iframe`'s post-connection steps (HTML §4.8.5): create a child navigable, then process the
    /// iframe attributes for the first time.
    pub(crate) fn frame_inserted(&self, cx: &mut Cx, el: NodeId) {
        if !self.dom().is_connected(el) || self.context_of_container(el).is_some() {
            return;
        }
        // An `object` or `embed` that is not being rendered loads nothing (HTML §4.8.6, §4.8.7).
        let plugin_like = {
            let dom = self.dom();
            dom.is_html_element_named(el, &local_name!("object"))
                || dom.is_html_element_named(el, &local_name!("embed"))
        };
        if plugin_like && (!self.being_rendered(el) || self.inside_object_resource(el)) {
            return;
        }
        let parent_doc = self.dom().node_document(el);
        let Some(parent) = self.context_of_document(parent_doc) else {
            return;
        };
        // (a document whose navigable was destroyed is not fully active: no child navigable, as
        // for an iframe moved into its own child document)
        if self.frames.borrow().list[parent].discarded {
            return;
        }
        // "Create a new child navigable": a Window, its realm and an initial about:blank document.
        let doc = {
            let mut dom = self.dom_mut();
            let doc = dom.create_html_document(None);
            // (HTML "create a new browsing context and document": allow declarative shadow roots)
            dom.set_allow_declarative_shadow_roots(doc, true);
            if let Some(dt) = dom.document_doctype(doc) {
                dom.remove_node(dt);
            }
            dom.set_document_url(doc, "about:blank");
            // (HTML "create a new browsing context and document": its mode is "quirks")
            dom.set_quirks_mode(doc, snowlark_dom::QuirksMode::Quirks);
            doc
        };
        self.process_reactions(cx);
        // (HTML "create a new browsing context and document": with a creator, the document's
        // referrer is the serialization of the creator's URL)
        // (and its about base URL is the creator's base URL)
        let creator_url = self.dom().document_url(parent_doc).to_owned();
        let creator_base = self.document_base_url(&self.dom(), parent_doc);
        self.doc_state(doc, |s| {
            s.has_browsing_context = true;
            s.ready_state = DocumentReadyState::Complete;
            s.referrer = creator_url;
            s.about_base_url = Some(creator_base);
        });
        // (the initial document already has the origin the frame's `sandbox` gives)
        let sandbox = self.sandbox_attribute_of(el);
        self.apply_frame_sandbox(doc, sandbox.as_deref(), parent_doc);
        self.inherit_origin(doc, None, Some(parent_doc));
        let window = self.new_context(doc, Some(parent), Some(el));
        let token = {
            let f = self.frames.borrow();
            realm_token(window, &f.origins[&window])
        };
        let realm = create_realm(cx.scope(), window, &token);
        let id = self.context_of_window(window).expect("the new context");
        self.frames.borrow_mut().list[id].realm = Some(realm);
        self.publish_window_document(cx.scope(), window);
        // The initial about:blank document's policy container is a clone of its parent's.
        self.inherit_policy(cx, doc, parent_doc);
        self.render_attach_frame(doc, el, None);
        self.frames.borrow_mut().list[id].name = self
            .dom()
            .attr(el, &ns!(), &local_name!("name"))
            .unwrap_or("")
            .to_owned();
        self.process_frame_attributes(cx, el, true);
    }

    /// HTML §7.3.1.3 "create a new top-level traversable", named `name` and hosted in this page with
    /// its own viewport. With an opener, "create a new auxiliary browsing context and document" given
    /// `opener`, whose active document `creator` is: the initial `about:blank` document takes the
    /// creator's origin, policy container and URL as referrer, and the context joins the opener's
    /// browsing context group. Without, "create a new top-level browsing context and document": a
    /// new group, and an opaque origin.
    pub(crate) fn create_auxiliary(
        &self,
        cx: &mut Cx,
        opener: Option<ContextId>,
        creator: NodeId,
        name: &str,
        viewport: (f32, f32),
    ) -> ContextId {
        let Some(opener) = opener else {
            return self.create_top_level_without_opener(cx, name, viewport);
        };
        let doc = {
            let mut dom = self.dom_mut();
            let doc = dom.create_html_document(None);
            dom.set_allow_declarative_shadow_roots(doc, true);
            if let Some(dt) = dom.document_doctype(doc) {
                dom.remove_node(dt);
            }
            dom.set_document_url(doc, "about:blank");
            // (HTML "create a new browsing context and document": its mode is "quirks")
            dom.set_quirks_mode(doc, snowlark_dom::QuirksMode::Quirks);
            doc
        };
        self.process_reactions(cx);
        let creator_url = self.dom().document_url(creator).to_owned();
        let creator_base = self.document_base_url(&self.dom(), creator);
        self.doc_state(doc, |s| {
            s.has_browsing_context = true;
            s.ready_state = DocumentReadyState::Complete;
            s.referrer = creator_url;
            s.about_base_url = Some(creator_base);
        });
        self.inherit_origin(doc, Some(self.origin_taken_from(creator, false)), None);
        let window = self.new_context(doc, None, None);
        let id = self.context_of_window(window).expect("the new context");
        {
            // (the creator's origin, with its domain)
            let mut f = self.frames.borrow_mut();
            let opener_window = f.list[opener].window;
            let domain = f.origins.get(&opener_window).and_then(|o| o.domain.clone());
            if let Some(o) = f.origins.get_mut(&window) {
                o.domain = domain;
            }
            let group = f.list[top_of(&f, opener)].group;
            let b = &mut f.list[id];
            b.opener = Some(opener);
            b.group = group;
            b.created_by_web_content = true;
            b.name = name.to_owned();
            b.popup_viewport = Some(viewport);
        }
        self.finish_top_level(cx, id, window);
        self.inherit_policy(cx, doc, creator);
        self.render_attach_popup(doc, viewport, None);
        id
    }

    /// [`Engine::create_auxiliary`] without an opener.
    fn create_top_level_without_opener(
        &self,
        cx: &mut Cx,
        name: &str,
        viewport: (f32, f32),
    ) -> ContextId {
        let doc = {
            let mut dom = self.dom_mut();
            let doc = dom.create_html_document(None);
            dom.set_allow_declarative_shadow_roots(doc, true);
            if let Some(dt) = dom.document_doctype(doc) {
                dom.remove_node(dt);
            }
            dom.set_document_url(doc, "about:blank");
            // (HTML "create a new browsing context and document": its mode is "quirks")
            dom.set_quirks_mode(doc, snowlark_dom::QuirksMode::Quirks);
            doc
        };
        self.process_reactions(cx);
        self.doc_state(doc, |s| {
            s.has_browsing_context = true;
            s.ready_state = DocumentReadyState::Complete;
        });
        // ("determine the origin" of about:blank with no creator: a new opaque origin)
        let opaque = self.new_opaque_origin(url::Url::parse("about:blank").expect("about:blank"));
        self.inherit_origin(doc, Some(opaque), None);
        let window = self.new_context(doc, None, None);
        let id = self.context_of_window(window).expect("the new context");
        {
            let mut f = self.frames.borrow_mut();
            f.last_group += 1;
            let group = f.last_group;
            let b = &mut f.list[id];
            b.group = group;
            b.created_by_web_content = true;
            b.name = name.to_owned();
            b.popup_viewport = Some(viewport);
        }
        self.finish_top_level(cx, id, window);
        self.render_attach_popup(doc, viewport, None);
        id
    }

    /// A new opaque origin for an `about:` document at `url` to take (one no other document has).
    pub(crate) fn new_opaque_origin(&self, url: url::Url) -> Inherited {
        let serial = {
            let mut f = self.frames.borrow_mut();
            f.opaque_serial += 1;
            f.opaque_serial
        };
        Inherited {
            origin: format!("opaque:new:{serial}"),
            net_origin: url::Origin::new_opaque(),
            url,
            from_parent: false,
        }
    }

    /// Whether `id` is a top-level browsing context: the page's own, or another it hosts.
    pub(crate) fn is_top_level_context(&self, id: ContextId) -> bool {
        self.frames
            .borrow()
            .list
            .get(id)
            .is_some_and(|b| b.parent.is_none())
    }

    /// The realm of a new top-level context's Window, and its document published to script.
    fn finish_top_level(&self, cx: &mut Cx, id: ContextId, window: Handle) {
        let token = {
            let f = self.frames.borrow();
            realm_token(window, &f.origins[&window])
        };
        let realm = create_realm(cx.scope(), window, &token);
        self.frames.borrow_mut().list[id].realm = Some(realm);
        self.publish_window_document(cx.scope(), window);
    }

    /// HTML "being rendered": the element and its ancestors are not `display: none` (true without
    /// rendering).
    pub(crate) fn being_rendered(&self, el: NodeId) -> bool {
        let chain: Vec<NodeId> = {
            let dom = self.dom();
            std::iter::once(el)
                .chain(dom.ancestors(el))
                .filter(|&n| dom.is_element(n))
                .collect()
        };
        self.with_style(el, |r| {
            chain.iter().all(|&n| {
                r.element_style(n).is_some_and(|s| {
                    !matches!(
                        snowlark_layout::Style::display(&s),
                        snowlark_layout::style::Display::None
                    )
                })
            })
        })
        .unwrap_or(true)
    }

    /// CSS Contain 2 §4.1 "skipped contents": an ancestor of `el` has `content-visibility: hidden`
    /// (false without rendering).
    pub(crate) fn in_skipped_contents(&self, el: NodeId) -> bool {
        let chain: Vec<NodeId> = {
            let dom = self.dom();
            dom.ancestors(el).filter(|&n| dom.is_element(n)).collect()
        };
        self.with_style(el, |r| {
            chain.iter().any(|&n| {
                r.element_style(n)
                    .is_some_and(|s| snowlark_layout::Style::contents_skipped(&s))
            })
        })
        .unwrap_or(false)
    }

    /// The value of a container's `sandbox` attribute (only an `iframe` has one).
    fn sandbox_attribute_of(&self, el: NodeId) -> Option<String> {
        let dom = self.dom();
        dom.is_html_element_named(el, &local_name!("iframe"))
            .then(|| dom.get_attribute(el, "sandbox").map(str::to_owned))
            .flatten()
    }

    /// A navigation of `id` starts (HTML "snapshotting target snapshot params"): its frame's
    /// sandbox flags and iframe referrer policy are the attributes' now.
    pub(crate) fn note_frame_sandbox(&self, id: ContextId) {
        let container = self.frames.borrow().list[id].container;
        let sandbox = container.and_then(|c| self.sandbox_attribute_of(c));
        let policy = container
            .map(|c| self.iframe_referrer_policy(c))
            .unwrap_or(snowlark_net::ReferrerPolicy::Empty);
        let mut f = self.frames.borrow_mut();
        f.list[id].sandbox_attribute = sandbox;
        f.list[id].container_referrer_policy = policy;
    }

    /// HTML "determining the iframe element referrer policy": an `iframe`'s `referrerpolicy`
    /// attribute's state, the empty string for any other container.
    fn iframe_referrer_policy(&self, el: NodeId) -> snowlark_net::ReferrerPolicy {
        let dom = self.dom();
        dom.is_html_element_named(el, &local_name!("iframe"))
            .then(|| dom.get_attribute(el, "referrerpolicy"))
            .flatten()
            .and_then(|v| snowlark_net::ReferrerPolicy::parse(&v.to_ascii_lowercase()))
            .unwrap_or(snowlark_net::ReferrerPolicy::Empty)
    }

    /// HTML §3.1.3 the "internal ancestor origin objects list creation steps" for `doc`, with the
    /// iframe referrer policy it was made with: its container document's origin, then that
    /// document's own list, where a masked origin (`None`: a new opaque origin) hides the parent's
    /// origin and the run of ancestors right above it that share it. Each origin is as
    /// [`Engine::origin_of_document`] gives it.
    pub(crate) fn internal_ancestor_origins(&self, doc: NodeId) -> Vec<Option<String>> {
        let Some(parent) = self.parent_document_of(doc) else {
            return Vec::new();
        };
        let policy = self
            .frames
            .borrow()
            .ancestor_policies
            .get(&doc)
            .copied()
            .unwrap_or(snowlark_net::ReferrerPolicy::Empty);
        let parent_origin = self.origin_of_document(parent);
        let mut masked = match policy {
            snowlark_net::ReferrerPolicy::NoReferrer => true,
            snowlark_net::ReferrerPolicy::SameOrigin => {
                parent_origin != self.origin_of_document(doc)
            }
            _ => false,
        };
        let ancestors = self.internal_ancestor_origins(parent);
        let mut output = Vec::with_capacity(ancestors.len() + 1);
        output.push((!masked).then(|| parent_origin.clone()));
        for origin in ancestors {
            if masked && origin.as_deref() == Some(parent_origin.as_str()) {
                output.push(None);
                continue;
            }
            output.push(origin);
            masked = false;
        }
        output
    }

    /// `iframe`'s removing steps: destroy the child navigable.
    pub(crate) fn frame_removed(&self, el: NodeId) {
        let Some(id) = self.frames.borrow_mut().by_container.remove(&el) else {
            return;
        };
        self.discard_contexts(&[id]);
    }

    /// HTML §7.3.1.5 "destroy a child navigable", for each browsing context of `roots` that is not
    /// discarded yet and all of its descendants: they are discarded, their navigations under way end,
    /// their Windows' IndexedDB connections close and their documents leave the rendering.
    pub(crate) fn discard_contexts(&self, roots: &[ContextId]) {
        let ids: Vec<ContextId> = {
            let f = self.frames.borrow();
            let mut all: Vec<ContextId> = roots
                .iter()
                .copied()
                .filter(|&r| !f.list[r].discarded)
                .collect();
            let mut i = 0;
            while i < all.len() {
                let p = all[i];
                all.extend(
                    f.list
                        .iter()
                        .enumerate()
                        .filter(|(_, b)| b.parent == Some(p) && !b.discarded)
                        .map(|(c, _)| c),
                );
                i += 1;
            }
            all
        };
        if ids.is_empty() {
            return;
        }
        // (HTML "destroy a child navigable": its session history entries go with it)
        self.history_navigables_destroyed(&ids);
        let mut f = self.frames.borrow_mut();
        let mut docs = Vec::with_capacity(ids.len());
        let mut windows = Vec::with_capacity(ids.len());
        for &id in &ids {
            f.list[id].discarded = true;
            // Nothing here keeps the realm any more: it lives while script reaches something of it
            // (engine/gc.rs, "Discarded browsing contexts").
            f.list[id].realm = None;
            f.list[id].navigation_request = None;
            docs.push(f.list[id].document);
            windows.push(f.list[id].window);
        }
        drop(f);
        for &id in &ids {
            self.cancel_document_fetches(id);
        }
        self.gc.borrow_mut().discarded.extend(ids);
        for w in windows {
            self.idb_global_destroyed(w);
            self.audio_window_unloaded(w);
        }
        self.render_detach_documents(&docs);
    }

    /// The iframe's `src`/`srcdoc` changed (HTML §4.8.5 attribute change steps).
    pub(crate) fn frame_attribute_changed(&self, cx: &mut Cx, el: NodeId, local: &str) {
        let url_attr = self.container_url_attribute(el);
        if !(local == url_attr || local == "srcdoc") {
            return;
        }
        if self.context_of_container(el).is_none() {
            // (HTML §4.8.6, §4.8.7: an `embed` or `object` that had no resource to show gets
            // a navigable once its URL names a document)
            let plugin_like = {
                let dom = self.dom();
                dom.is_html_element_named(el, &local_name!("object"))
                    || dom.is_html_element_named(el, &local_name!("embed"))
            };
            if plugin_like {
                self.frame_inserted(cx, el);
            }
            return;
        }
        // `src` changes are ignored while `srcdoc` is present.
        if local == "src"
            && self.dom().is_html_element_named(el, &local_name!("iframe"))
            && self
                .dom()
                .attr(el, &ns!(), &local_name!("srcdoc"))
                .is_some()
        {
            return;
        }
        self.process_frame_attributes(cx, el, false);
    }

    /// HTML §4.8.5 "process the iframe attributes".
    fn process_frame_attributes(&self, cx: &mut Cx, el: NodeId, initial: bool) {
        let Some(id) = self.context_of_container(el) else {
            return;
        };
        let url_attr = self.container_url_attribute(el);
        let (srcdoc, src) = {
            let dom = self.dom();
            let is_iframe = dom.is_html_element_named(el, &local_name!("iframe"));
            (
                is_iframe
                    .then(|| {
                        dom.attr(el, &ns!(), &local_name!("srcdoc"))
                            .map(str::to_owned)
                    })
                    .flatten(),
                dom.get_attribute(el, url_attr).map(str::to_owned),
            )
        };
        // 1: srcdoc.
        if let Some(text) = srcdoc {
            if self.defer_lazy_frame(id, el, || LazyNavigation::Srcdoc(text.clone())) {
                return;
            }
            if !initial {
                self.fire_beforeunload(cx, id);
            }
            self.navigate_context_srcdoc(id, text);
            return;
        }
        // 2: "shared attribute processing steps for iframe and frame elements".
        // ("encoding-parsing a URL" relative to the element's node document: the query takes
        // the document's encoding)
        let url = match src.as_deref() {
            Some(s) if !s.is_empty() => self.encoding_parse_url(el, s),
            _ => None,
        };
        let url = match url {
            Some(u) => u,
            // (an `object`'s `data` that does not parse: `error`, then its fallback content)
            None if url_attr == "data" && src.as_deref().is_some_and(|s| !s.is_empty()) => {
                self.object_fallback(id, el, true, false);
                return;
            }
            None => {
                if initial {
                    // No URL on first processing: run the iframe load event steps right away.
                    self.iframe_load_event_steps(cx, el);
                    return;
                }
                url::Url::parse("about:blank").expect("about:blank")
            }
        };
        // (no navigation to a URL that two of the ancestor navigables' documents already have,
        // fragments aside: a page may nest itself once, as sites depend on, but not without end)
        if url_attr == "src"
            && !matches!(url.scheme(), "about" | "javascript")
            && self.an_ancestor_shows(el, &url)
        {
            return;
        }
        // (an `object` navigates only to what it fetched: a `javascript:` URL is a load that
        // failed, `error` and then the fallback content)
        if url_attr == "data" && url.scheme() == "javascript" {
            self.object_fallback(id, el, true, false);
            return;
        }
        if url.as_str() == "about:blank" && initial {
            self.iframe_load_event_steps(cx, el);
            return;
        }
        // HTML "navigate an iframe or frame" step 3: the referrer policy is the state of the
        // element's `referrerpolicy` attribute (an `iframe`'s; a deferred navigation keeps the
        // one it had then).
        let referrer_policy = {
            let dom = self.dom();
            dom.is_html_element_named(el, &local_name!("iframe"))
                .then(|| dom.attr(el, &ns!(), &local_name!("referrerpolicy")))
                .flatten()
                .and_then(|v| snowlark_net::ReferrerPolicy::parse(&v.to_ascii_lowercase()))
                .unwrap_or(snowlark_net::ReferrerPolicy::Empty)
        };
        if self.defer_lazy_frame(id, el, || LazyNavigation::Url(url.clone(), referrer_policy)) {
            return;
        }
        // ("navigate" with initialInsertion, which only a `javascript:` URL looks at)
        if initial && url.scheme() == "javascript" {
            let request = NavigationRequest {
                url,
                post: None,
                source: None,
                form: false,
                reload: false,
                referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                history: Default::default(),
            };
            self.navigate_javascript(id, request, true);
            return;
        }
        if !initial && url.scheme() != "javascript" {
            self.fire_beforeunload(cx, id);
        }
        // (an `object`'s navigation is its fetch of `data`: a failed one falls back, whatever
        // document it shows now)
        if url_attr == "data" {
            let mut f = self.frames.borrow_mut();
            let b = &mut f.list[id];
            b.object_fetch = Some(b.navigations + 1);
        }
        let history = self.frame_history_handling(id);
        self.navigate_request(
            id,
            NavigationRequest {
                url,
                post: None,
                source: None,
                form: false,
                reload: false,
                referrer_policy,
                history,
            },
        );
        if url_attr == "data" {
            // (none began: the mark must not fit a later navigation)
            let mut f = self.frames.borrow_mut();
            let b = &mut f.list[id];
            if b.object_fetch != Some(b.navigations) {
                b.object_fetch = None;
            }
        }
    }

    /// Whether two of the inclusive ancestor navigables of `el`'s node navigable have a document
    /// whose URL is `url`, fragments excluded.
    fn an_ancestor_shows(&self, el: NodeId, url: &url::Url) -> bool {
        let strip = |u: &str| u.split('#').next().unwrap_or("").to_owned();
        let target = strip(url.as_str());
        let mut doc = Some(self.dom().node_document(el));
        let mut found = false;
        while let Some(d) = doc {
            if strip(self.dom().document_url(d)) == target {
                if found {
                    return true;
                }
                found = true;
            }
            doc = self.parent_document(d);
        }
        false
    }

    /// The lazy loading steps of HTML "process the iframe attributes": "current navigation was
    /// lazy loaded" is set from the "will lazy load element steps", and when they say so the
    /// navigation waits as the element's "lazy load resumption steps" until the iframe is near
    /// the viewport ([`Engine::frames_after_layout`]). True when it waits.
    fn defer_lazy_frame(
        &self,
        id: ContextId,
        el: NodeId,
        navigation: impl FnOnce() -> LazyNavigation,
    ) -> bool {
        let lazy = self.will_lazy_load_frame(el);
        let mut f = self.frames.borrow_mut();
        let b = &mut f.list[id];
        b.lazily_loaded = lazy;
        b.lazy_load = lazy.then(navigation);
        lazy
    }

    /// HTML "will lazy load element steps" for an `iframe`: scripting is enabled for it and its
    /// `loading` attribute is in the Lazy state.
    fn will_lazy_load_frame(&self, el: NodeId) -> bool {
        let (lazy, doc) = {
            let dom = self.dom();
            (
                dom.is_html_element_named(el, &local_name!("iframe"))
                    && dom
                        .attr(el, &ns!(), &snowlark_dom::LocalName::from("loading"))
                        .is_some_and(|l| l.eq_ignore_ascii_case("lazy")),
                dom.node_document(el),
            )
        };
        lazy && !self.scripts_sandboxed(doc) && self.window_of_document(doc).is_some()
    }

    /// HTML "lazy load resumption steps" for the iframes waiting near their viewport after a
    /// layout, and those whose `loading` stopped being lazy.
    pub(crate) fn frames_after_layout(&self, cx: &mut Cx) {
        let waiting: Vec<(ContextId, NodeId)> = {
            let f = self.frames.borrow();
            f.list
                .iter()
                .enumerate()
                .filter(|(_, b)| b.lazy_load.is_some() && !b.discarded)
                .filter_map(|(id, b)| b.container.map(|c| (id, c)))
                .collect()
        };
        for (id, el) in waiting {
            if !self.will_lazy_load_frame(el) || self.near_viewport(el) {
                self.resume_lazy_frame(cx, id);
            }
        }
    }

    /// The `loading` attribute of an iframe changed: a navigation waiting for lazy loading that is
    /// no longer lazy starts now.
    pub(crate) fn frame_loading_changed(&self, cx: &mut Cx, el: NodeId) {
        if self.will_lazy_load_frame(el) {
            return;
        }
        if let Some(id) = self.context_of_container(el) {
            self.resume_lazy_frame(cx, id);
        }
    }

    /// The step of HTML "navigate" that ends a lazy loaded iframe's wait: when the navigable's
    /// container has lazy load resumption steps, they are dropped. It runs when "navigate" is
    /// called (before a fragment navigation, and before the navigation itself, which runs after
    /// the current task); a reload is not a navigation and leaves them.
    pub(crate) fn stop_lazy_loading(&self, id: ContextId) {
        self.frames.borrow_mut().list[id].lazy_load = None;
    }

    fn resume_lazy_frame(&self, cx: &mut Cx, id: ContextId) {
        let Some(navigation) = self.frames.borrow_mut().list[id].lazy_load.take() else {
            return;
        };
        // (the resumed navigation checks whether unloading is canceled, as the one `src` or
        // `srcdoc` would have started at once; the initial `about:blank` has nothing to unload)
        if !self.frames.borrow().list[id].initial_about_blank {
            self.fire_beforeunload(cx, id);
        }
        match navigation {
            LazyNavigation::Url(url, referrer_policy) => self.navigate_request(
                id,
                NavigationRequest {
                    url,
                    post: None,
                    source: None,
                    form: false,
                    reload: false,
                    referrer_policy,
                    history: Default::default(),
                },
            ),
            LazyNavigation::Srcdoc(text) => self.navigate_context_srcdoc(id, text),
        }
    }

    /// HTML §4.8.5 "iframe load event steps": fire `load` at the container.
    pub(crate) fn iframe_load_event_steps(&self, cx: &mut Cx, el: NodeId) {
        self.fire_event(cx, node_handle(el), "load", false, false);
    }

    /// Navigates a browsing context to `url`: the document is made when the response arrives.
    pub fn navigate_context(&self, id: ContextId, url: url::Url) {
        self.navigate_request(
            id,
            NavigationRequest {
                url,
                post: None,
                source: None,
                form: false,
                reload: false,
                referrer_policy: snowlark_net::ReferrerPolicy::Empty,
                history: Default::default(),
            },
        );
    }

    /// Navigates a browsing context, with a POST body for a form submission.
    pub fn navigate_request(&self, id: ContextId, request: NavigationRequest) {
        // HTML "navigate": nothing while the active document is unloading.
        let active = self.frames.borrow().list[id].document;
        if self.doc_state(active, |s| s.unload_counter > 0) {
            return;
        }
        // (a navigation script asked for earlier, still waiting for its task to end, is an
        // earlier navigation: this one replaces it)
        self.frames.borrow_mut().list[id].navigation_request = None;
        // (the embedder's own addresses are its to serve, or to refuse)
        if self.embedder_takes(id, &request.url) {
            return;
        }
        // (a `javascript:` URL runs in the target's document, and does not disturb a navigation
        // under way unless its result is a document)
        if request.url.scheme() == "javascript" {
            self.navigate_javascript(id, request, false);
            return;
        }
        // HTML "navigate": `about:srcdoc` is a child navigable's only; a top-level traversable
        // this page hosts shows an error page instead.
        if request.url.as_str() == "about:srcdoc" && id != TOP && self.is_top_level_context(id) {
            self.cancel_document_fetches(id);
            let url = request.url;
            self.queue_task(TaskSource::Networking, move |e, cx| {
                if !e.frames.borrow().list[id].discarded {
                    e.show_error_page(cx, id, &url, crate::error_page::Failure::Unavailable);
                }
            });
            return;
        }
        // M5: a top-level navigation begins in the browser, which fetches and hands the response
        // back to commit (`Engine::commit_navigation`).
        if id == TOP && self.browser_navigates() {
            self.begin_in_browser(request);
            return;
        }
        let handling = self.resolve_history_handling(id, &request);
        self.frames.borrow_mut().list[id].history_handling = handling;
        self.frames.borrow_mut().list[id].navigations += 1;
        let initiator = request.source.map(|d| self.origin_taken_from(d, false));
        self.frames.borrow_mut().list[id].initiator_origin = initiator;
        self.note_frame_sandbox(id);
        if self.frames.borrow().list[id].container.is_some() {
            self.frames.borrow_mut().list[id].pending_load_event = true;
        }
        // A navigation replaces any earlier one in flight for the same context.
        self.cancel_document_fetches(id);
        self.fetch_resource_with(
            request.url,
            crate::loader::Purpose::Document(id),
            request.post,
            crate::loader::NavInfo {
                source: request.source,
                form: request.form,
                reload: request.reload,
                referrer_policy: request.referrer_policy,
            },
        );
    }

    /// HTML §7.4.2.3.3 "navigate to a `javascript:` URL", in a task: the URL's script runs in the
    /// target's active document when the initiator is same origin-domain with it (and scripts may
    /// run there); a string result becomes a new HTML document in its place ("evaluate a
    /// `javascript:` URL": its URL and policy container the active document's). With
    /// `initial_insertion` (an iframe's `src` when it was inserted) and no new document, the iframe
    /// load event steps run for the initial `about:blank` document.
    fn navigate_javascript(
        &self,
        id: ContextId,
        request: NavigationRequest,
        initial_insertion: bool,
    ) {
        // (an `embed`'s `src` or an `object`'s `data` that is a `javascript:` URL does nothing:
        // HTML §4.8.6 and §4.8.7 navigate only to what they fetched)
        let container = self.frames.borrow().list[id].container;
        if let Some(el) = container {
            let dom = self.dom();
            if dom.is_html_element_named(el, &local_name!("embed"))
                || dom.is_html_element_named(el, &local_name!("object"))
            {
                return;
            }
        }
        // (a navigation that begins before the task runs cancels it)
        let navigations = self.frames.borrow().list[id].navigations;
        self.queue_task(TaskSource::HistoryTraversal, move |e, cx| {
            let superseded = {
                let f = e.frames.borrow();
                let b = &f.list[id];
                b.navigations != navigations || b.navigation_request.is_some()
            };
            if superseded {
                return;
            }
            if e.evaluate_javascript_navigation(cx, id, &request) || !initial_insertion {
                return;
            }
            let (container, initial, discarded) = {
                let f = e.frames.borrow();
                let b = &f.list[id];
                (b.container, b.initial_about_blank, b.discarded)
            };
            if let (Some(el), true, false) = (container, initial, discarded) {
                e.iframe_load_event_steps(cx, el);
            }
        });
    }

    /// The task of "navigate to a `javascript:` URL": true when its result became a new document.
    fn evaluate_javascript_navigation(
        &self,
        cx: &mut Cx,
        id: ContextId,
        request: &NavigationRequest,
    ) -> bool {
        let (active, discarded) = {
            let f = self.frames.borrow();
            let b = &f.list[id];
            (b.document, b.discarded)
        };
        if discarded || !self.dom().is_alive(active) {
            return false;
        }
        // (the initiator: the navigation's source, or for a frame's `src` its container's
        // document)
        let source = request.source.or_else(|| self.parent_document(active));
        if let Some(source) = source {
            if !self.dom().is_alive(source) || !self.same_origin_domain_documents(source, active) {
                return false;
            }
        }
        if self.scripts_sandboxed(active) {
            return false;
        }
        let encoded = &request.url.as_str()["javascript:".len()..];
        let code = String::from_utf8_lossy(&percent_decode(encoded.as_bytes())).into_owned();
        let script_url = self.dom().document_url(active).to_owned();
        let text = self.in_document_realm(cx, active, |cx| {
            let result = self.run_classic(cx, &code, &script_url, 0)?;
            let scope = cx.scope();
            let v = v8::Local::new(scope, &result);
            v.is_string().then(|| v.to_rust_string_lossy(scope))
        });
        let Some(text) = text else {
            return false;
        };
        // (the script may have navigated or removed the frame meanwhile; a navigation it began, such
        // as a form it submitted, takes precedence over its result)
        if self.frames.borrow().list[id].discarded
            || self.frames.borrow().list[id].document != active
            || self.frames.borrow().list[id].navigation_request.is_some()
        {
            return true;
        }
        let Ok(url) = url::Url::parse(self.dom().document_url(active)) else {
            return false;
        };
        if self.frames.borrow().list[id].container.is_some() {
            self.frames.borrow_mut().list[id].pending_load_event = true;
        }
        self.cancel_document_fetches(id);
        // (HTML "navigate to a javascript: URL": history handling "replace")
        self.frames.borrow_mut().list[id].history_handling = HistoryHandling::Replace;
        let doc = self.begin_document(cx, id, &url, "text/html", Some("utf-8".to_owned()));
        self.inherit_policy(cx, doc, active);
        // (and the active document's opener policy: a `javascript:` URL's navigation params take it,
        // so no browsing context group switch follows)
        let policy = self.doc_state(active, |s| s.opener_policy.clone());
        self.doc_state(doc, |s| s.opener_policy = policy);
        self.feed_document_text(cx, doc, &text);
        true
    }

    /// HTML "navigate" steps 12-13: a reload or a traversal keeps its entry; "auto" is "replace"
    /// while the navigable shows its initial `about:blank` document and for the URL of its active
    /// document (a navigation of a same-origin initiator), and "push" otherwise.
    fn resolve_history_handling(
        &self,
        id: ContextId,
        request: &NavigationRequest,
    ) -> HistoryHandling {
        if request.reload {
            return HistoryHandling::Reload;
        }
        let (initial, active) = {
            let f = self.frames.borrow();
            (f.list[id].initial_about_blank, f.list[id].document)
        };
        match request.history {
            HistoryHandling::Auto => {}
            h => return h,
        }
        if initial {
            return HistoryHandling::Replace;
        }
        let same_url =
            self.dom().is_alive(active) && self.dom().document_url(active) == request.url.as_str();
        let same_origin = request.source.is_none_or(|s| {
            self.dom().is_alive(s) && self.origin_of_document(s) == self.origin_of_document(active)
        });
        if same_url && same_origin {
            HistoryHandling::Replace
        } else {
            HistoryHandling::Push
        }
    }

    /// HTML "navigate an iframe or frame": "replace" while the content navigable's active
    /// document is not completely loaded, else "auto".
    fn frame_history_handling(&self, id: ContextId) -> HistoryHandling {
        let active = self.frames.borrow().list[id].document;
        if self.is_completely_loaded(active) {
            HistoryHandling::Auto
        } else {
            HistoryHandling::Replace
        }
    }

    /// HTML "completely loaded": the document's load event has run.
    pub(crate) fn is_completely_loaded(&self, doc: NodeId) -> bool {
        self.dom().is_alive(doc) && self.doc_state(doc, |s| s.load_timing.load_event_end > 0.0)
    }

    /// Navigates a child browsing context to its `srcdoc` text (`about:srcdoc`), parsed in a task.
    fn navigate_context_srcdoc(&self, id: ContextId, text: String) {
        let handling = match self.frame_history_handling(id) {
            HistoryHandling::Replace => HistoryHandling::Replace,
            _ if self.frames.borrow().list[id].initial_about_blank => HistoryHandling::Replace,
            _ => HistoryHandling::Push,
        };
        self.frames.borrow_mut().list[id].history_handling = handling;
        // (the srcdoc document takes its parent's origin, not that of an earlier navigation's
        // initiator, should that one not have finished)
        self.frames.borrow_mut().list[id].initiator_origin = None;
        self.frames.borrow_mut().list[id].pending_load_event = true;
        self.frames.borrow_mut().list[id].navigations += 1;
        self.note_frame_sandbox(id);
        self.cancel_document_fetches(id);
        self.queue_task(TaskSource::Networking, move |e, cx| {
            if e.frames.borrow().list[id].discarded {
                return;
            }
            let url = url::Url::parse("about:srcdoc").expect("about:srcdoc");
            let doc = e.begin_document(cx, id, &url, "text/html", None);
            // An iframe srcdoc document's policy container is a clone of its parent's, and its
            // about base URL is its parent's document base URL.
            if let Some(parent) = e.parent_document(doc) {
                e.inherit_policy(cx, doc, parent);
                let base = e.document_base_url(&e.dom(), parent);
                e.doc_state(doc, |s| s.about_base_url = Some(base));
            }
            e.feed_document_text(cx, doc, &text);
        });
    }

    /// The browsing context finished loading its document: the iframe load event steps at its
    /// container (HTML §7.4.6.1 "completely finish loading" step 5).
    pub(crate) fn context_loaded(&self, cx: &mut Cx, id: ContextId) {
        let container = {
            let mut f = self.frames.borrow_mut();
            let b = &mut f.list[id];
            if !b.pending_load_event || b.discarded {
                None
            } else {
                b.pending_load_event = false;
                b.container
            }
        };
        if let Some(el) = container {
            self.iframe_load_event_steps(cx, el);
        }
    }

    /// Script-requested navigations of all contexts (`location.href = …`), performed between tasks.
    pub fn take_navigation_requests(&self) -> Vec<(ContextId, NavigationRequest)> {
        self.html.borrow_mut().planned_navigations.clear();
        let mut f = self.frames.borrow_mut();
        f.list
            .iter_mut()
            .enumerate()
            .filter_map(|(i, b)| b.navigation_request.take().map(|u| (i, u)))
            .collect()
    }

    // ----- WindowProxy-ish members --------------------------------------------------------------------------

    pub(crate) fn parent_window(&self, window: Handle) -> Option<Handle> {
        let f = self.frames.borrow();
        let id = *f.by_window.get(&window)?;
        if f.list[id].discarded {
            return None;
        }
        match f.list[id].parent {
            Some(p) => Some(f.list[p].window),
            None => Some(window),
        }
    }

    pub(crate) fn top_window(&self, window: Handle) -> Option<Handle> {
        let f = self.frames.borrow();
        let mut id = *f.by_window.get(&window)?;
        if f.list[id].discarded {
            return None;
        }
        while let Some(p) = f.list[id].parent {
            id = p;
        }
        Some(f.list[id].window)
    }

    pub(crate) fn frame_element_of(&self, window: Handle) -> Option<NodeId> {
        let f = self.frames.borrow();
        let id = *f.by_window.get(&window)?;
        if f.list[id].discarded {
            return None;
        }
        f.list[id].container
    }

    /// The child browsing contexts of a window's document, in tree order of their containers.
    pub(crate) fn child_windows(&self, window: Handle) -> Vec<Handle> {
        let Some(doc) = self.document_of_window(window) else {
            return Vec::new();
        };
        let containers: Vec<NodeId> = {
            let dom = self.dom();
            let f = self.frames.borrow();
            // Most documents have no frames: skip the tree walk then.
            if !f
                .by_container
                .keys()
                .any(|&c| dom.is_alive(c) && dom.node_document(c) == doc)
            {
                return Vec::new();
            }
            dom.descendants(doc)
                .filter(|n| f.by_container.contains_key(n))
                .collect()
        };
        containers
            .into_iter()
            .filter_map(|c| self.context_of_container(c))
            .map(|id| self.frames.borrow().list[id].window)
            .collect()
    }

    pub(crate) fn context_discarded(&self, window: Handle) -> bool {
        let f = self.frames.borrow();
        f.by_window
            .get(&window)
            .is_none_or(|&id| f.list[id].discarded || f.list[id].window != window)
    }

    /// Whether `window`'s browsing context (its document's) is not null: the context it was made for
    /// has not been discarded. A window its context has since replaced keeps the context (HTML: a
    /// Document's browsing context stays until the context is discarded).
    pub(crate) fn has_browsing_context(&self, window: Handle) -> bool {
        let f = self.frames.borrow();
        if f.severed.contains(&window) {
            return false;
        }
        f.by_window
            .get(&window)
            .copied()
            .or_else(|| f.replaced.get(&window).map(|&(id, _)| id))
            .is_some_and(|id| !f.list[id].discarded)
    }

    pub(crate) fn content_window(&self, el: NodeId) -> Option<Handle> {
        let id = self.context_of_container(el)?;
        Some(self.frames.borrow().list[id].window)
    }

    /// HTML "the content document": none unless it is same origin-domain with the container's.
    pub(crate) fn content_document(&self, el: NodeId) -> Option<NodeId> {
        let id = self.context_of_container(el)?;
        let doc = self.frames.borrow().list[id].document;
        let mine = self.dom().node_document(el);
        self.same_origin_domain_documents(doc, mine).then_some(doc)
    }

    /// HTML §7.3.1.7 "the rules for choosing a navigable" from `doc` for `target` (a link's or
    /// form's target, or a `base` element's).
    pub(crate) fn choose_navigable(&self, doc: NodeId, target: &str) -> Navigable {
        let Some(me) = self.context_of_document(doc) else {
            return Navigable::None;
        };
        let f = self.frames.borrow();
        let t = target.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("_self") {
            return Navigable::Context(me);
        }
        if t.eq_ignore_ascii_case("_parent") {
            return Navigable::Context(f.list[me].parent.unwrap_or(me));
        }
        if t.eq_ignore_ascii_case("_top") {
            return Navigable::Context(top_of(&f, me));
        }
        if t.eq_ignore_ascii_case("_blank") {
            return Navigable::NewTab;
        }
        drop(f);
        match self.find_navigable_by_name(me, t) {
            Some(id) => Navigable::Context(id),
            None => Navigable::NewTab,
        }
    }

    /// HTML §7.3.1.7 "find a navigable by target name": the inclusive descendants of `me`'s
    /// traversable first, then those of the other top-level traversables of its group (the ones
    /// this page hosts), each only when `me` is familiar with it.
    pub(crate) fn find_navigable_by_name(&self, me: ContextId, name: &str) -> Option<ContextId> {
        if name.is_empty() {
            return None;
        }
        let (mine, others) = {
            let guard = self.frames.borrow();
            let f: &Frames = &guard;
            let my_top = top_of(f, me);
            let named = |id: ContextId| !f.list[id].discarded && f.list[id].name == name;
            let in_tree = |root: ContextId| {
                (0..f.list.len()).filter(move |&id| top_of(f, id) == root && !f.list[id].discarded)
            };
            let mine = in_tree(my_top).find(|&id| named(id));
            let group = f.list[my_top].group;
            let others: Vec<ContextId> = (0..f.list.len())
                .filter(|&id| {
                    let b = &f.list[id];
                    b.parent.is_none() && id != my_top && !b.discarded && b.group == group
                })
                .flat_map(|root| in_tree(root).filter(|&id| named(id)).collect::<Vec<_>>())
                .collect();
            (mine, others)
        };
        mine.or_else(|| others.into_iter().find(|&id| self.familiar_with(me, id)))
    }

    /// HTML §7.3.2.2 "familiar with": whether script in context `a` may reach context `b` by name.
    pub(crate) fn familiar_with(&self, a: ContextId, b: ContextId) -> bool {
        let (a_doc, chain, opener) = {
            let f = self.frames.borrow();
            if top_of(&f, a) == b {
                return true;
            }
            // (b and its ancestors, whose origin a's sharing makes a familiar with b)
            let mut chain = vec![f.list[b].document];
            let mut c = f.list[b].parent;
            while let Some(p) = c {
                chain.push(f.list[p].document);
                c = f.list[p].parent;
            }
            (f.list[a].document, chain, f.list[b].opener)
        };
        let origin = self.origin_of_document(a_doc);
        if chain.iter().any(|&d| self.origin_of_document(d) == origin) {
            return true;
        }
        opener.is_some_and(|o| o != b && self.familiar_with(a, o))
    }

    /// Plans a navigation of the navigable `target` chose from `doc` (a new tab is the embedder's).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plan_navigation(
        &self,
        doc: NodeId,
        target: &str,
        url: url::Url,
        post: Option<(Vec<u8>, String)>,
        background: bool,
        form: bool,
        referrer_policy: snowlark_net::ReferrerPolicy,
    ) -> Option<ContextId> {
        match self.choose_navigable(doc, target) {
            Navigable::Context(id) if !self.allowed_by_sandboxing_to_navigate(doc, id) => None,
            Navigable::Context(id) => {
                self.stop_lazy_loading(id);
                let window = self.frames.borrow().list[id].window;
                if post.is_none() && !form && self.navigate_to_fragment(window, &url, false) {
                    return None;
                }
                // (HTML "submit": a form whose document has not completely loaded replaces)
                let history = if form && !self.is_completely_loaded(doc) {
                    HistoryHandling::Replace
                } else {
                    HistoryHandling::Auto
                };
                let request = NavigationRequest {
                    url,
                    post,
                    source: Some(doc),
                    form,
                    reload: false,
                    referrer_policy,
                    history,
                };
                // (HTML "navigate" step 13: a `javascript:` URL queues its task now; a form's
                // navigation is planned first, HTML §4.10.21.3 "plan to navigate")
                if request.url.scheme() == "javascript" && !form {
                    self.navigate_request(id, request);
                    return None;
                }
                self.frames.borrow_mut().list[id].navigation_request = Some(request);
                Some(id)
            }
            Navigable::NewTab => {
                if post.is_none() {
                    self.open_requests.borrow_mut().push((url, background));
                }
                None
            }
            Navigable::None => None,
        }
    }

    /// HTML §4.10.21.3: before a form plans a navigation through its submission events (a submit
    /// button, `requestSubmit()`, implicit submission), its previous planned navigation, if still
    /// pending and not replaced since, is removed. (`form.submit()` alone keeps it: the reference
    /// engine navigates every target a form's `submit()` calls in one task name.)
    pub(crate) fn cancel_planned_form_navigation(&self, form: NodeId) {
        let Some((id, planned)) = self.html.borrow_mut().planned_navigations.remove(&form) else {
            return;
        };
        let mut f = self.frames.borrow_mut();
        if let Some(b) = f.list.get_mut(id) {
            if b.navigation_request.as_ref() == Some(&planned) {
                b.navigation_request = None;
            }
        }
    }

    /// HTML §4.2.3 the target of the document's first `base` element with a `target` attribute.
    pub(crate) fn base_target(&self, doc: NodeId) -> Option<String> {
        let dom = self.dom();
        let root = dom.document_element(doc)?;
        dom.descendants(root)
            .find(|&n| {
                dom.is_html_element_named(n, &local_name!("base"))
                    && dom.get_attribute(n, "target").is_some()
            })
            .and_then(|n| dom.get_attribute(n, "target").map(str::to_owned))
    }

    /// Navigations the page asked to open in new tabs (URL, in the background).
    pub fn take_open_requests(&self) -> Vec<(url::Url, bool)> {
        std::mem::take(&mut *self.open_requests.borrow_mut())
    }

    pub(crate) fn window_name(&self, window: Handle) -> String {
        let f = self.frames.borrow();
        f.by_window
            .get(&window)
            .map(|&id| f.list[id].name.clone())
            .unwrap_or_default()
    }

    pub(crate) fn set_window_name(&self, window: Handle, name: &str) {
        let mut f = self.frames.borrow_mut();
        if let Some(&id) = f.by_window.get(&window) {
            f.list[id].name = name.to_owned();
        }
    }
}

/// URL percent-decoding (URL Standard §1.3): `%XX` with two hex digits becomes that byte.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            if let (Some(h), Some(l)) = (hex(input[i + 1]), hex(input[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}
