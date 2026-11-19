// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hooks for the embedder (the browser window) that are no part of any web-visible surface:
//!
//! - the navigation filter: navigations a page starts itself (links, forms, `location`, frames)
//!   to addresses the embedder serves (its own pages, `view-source:`) are handed to the embedder
//!   instead of being fetched, with the browsing context they were for, so that it can decide
//!   whether the document asking may go there (a web page may not open the browser's settings);
//! - the preferred color scheme (`prefers-color-scheme`) of every rendered document;
//! - the context menu: a `contextmenu` event nothing canceled asks for the embedder's menu, with
//!   what is under it ([`Engine::context_target`]).

use serde::{Deserialize, Serialize};
use snowlark_dom::{local_name, NodeId};

use crate::engine::Engine;

/// What a context menu is about.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextTarget {
    pub element: Option<NodeId>,
    /// The hyperlink the element is in.
    pub link: Option<url::Url>,
    /// The image the element is (`img`'s `src`).
    pub image: Option<url::Url>,
    /// The document's selected text.
    pub selection: Option<String>,
    /// A text field or editing host.
    pub editable: bool,
}

/// The top-level browsing context's number in [`NavigationFilter`] calls.
pub const TOP_CONTEXT: usize = crate::engine::frames::TOP;

/// Decides whether the embedder takes a navigation of a browsing context (by number; see
/// [`TOP_CONTEXT`]) to a URL (true: it is not fetched).
pub type NavigationFilter = Box<dyn Fn(usize, &url::Url) -> bool>;

/// A top-level navigation the page asks the browser to begin (`docs/process-isolation.md`, M5):
/// the browser makes the request, follows its redirects, chooses the renderer by the final site
/// and hands the response back to commit ([`Engine::commit_navigation`]). `seq` names it, so the
/// commit can find what the renderer kept of it (the initiator's origin, which an `about:`
/// document takes). Chromium: `RenderFrameHostImpl::BeginNavigation`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BeginNavigation {
    pub seq: u64,
    pub params: snowlark_net::navigation::NavigationParams,
}

impl PartialEq for BeginNavigation {
    fn eq(&self, other: &BeginNavigation) -> bool {
        self.seq == other.seq && self.params.url == other.params.url
    }
}

/// A same-document change of the top-level document's URL (`pushState`, `replaceState`, a
/// fragment navigation): the browser's session history gets an entry for each (Chromium's
/// `NavigationControllerImpl::RendererDidNavigateToNewEntry` clones the entry for a same-document
/// commit). `index` is the document's own index for the entry, which the browser names when it
/// traverses back to it ([`Engine::go_in_document`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SameDocumentChange {
    pub url: String,
    /// A new entry (`pushState`, a fragment navigation), or the current one changed
    /// (`replaceState`).
    pub push: bool,
    pub index: usize,
}

/// What the renderer keeps of a navigation it asked the browser to begin.
pub(crate) struct Begun {
    pub source: Option<NodeId>,
    pub initiator: Option<crate::engine::frames::Inherited>,
}

/// How many begun navigations the renderer remembers (a page that navigates in a loop).
const BEGUN_KEPT: usize = 16;

/// The embedder's hooks of an engine.
#[derive(Default)]
pub struct EmbedderHooks {
    pub filter: Option<NavigationFilter>,
    /// Top-level navigations begin in the browser ([`Engine::set_browser_navigation`]).
    pub browser_navigates: bool,
    pub begun: Vec<BeginNavigation>,
    pub next_seq: u64,
    pub(crate) begun_kept: Vec<(u64, Begun)>,
    pub same_document: Vec<SameDocumentChange>,
    /// Traversals of the session history the browser performs (steps; negative is back).
    pub traversals: Vec<isize>,
    /// The URL schemes the embedder serves: a form submits to them with GET as to `http:`
    /// (HTML §4.10.21.3 "submit": other schemes act "in a manner analogous" to the ones listed).
    pub schemes: Vec<String>,
    /// The element of a `contextmenu` event that was not canceled, until the embedder takes it.
    pub context_menu: Option<NodeId>,
    /// The system clipboard the embedder lends the page (the async Clipboard API).
    pub clipboard: Option<std::sync::Arc<dyn crate::engine::platform::ClipboardAccess>>,
    /// Each document's last user activation (HTML §6.4.3), as a high resolution time.
    pub activations: rustc_hash::FxHashMap<NodeId, f64>,
    /// `enumerateDevices()` promises waiting for their document to be in view (Media Capture and
    /// Streams §9.2 "device enumeration can proceed").
    pub device_enumerations: Vec<(NodeId, snowlark_bindings::rt::JsPromise)>,
    /// HTML "PDF viewer supported" (§8.9.1.6): PDF documents show inline in the engine's PDF viewer
    /// (`loader::pdf_viewer_markup`); on unless the embedder turns it off.
    pub pdf_viewer: bool,
    /// Fullscreen viewport requests for the embedder (the Fullscreen API).
    pub fullscreen: crate::engine::fullscreen::FullscreenAsks,
    /// New top-level traversables open in tabs of the browser's, with no opener
    /// ([Engine::set_new_windows_in_tabs]).
    pub new_windows_in_tabs: bool,
    /// The embedder shows each top-level traversable this page hosts besides its own as a view
    /// of its own ([Engine::set_shows_hosted_windows]); ones with no opener open in tabs of the
    /// browser's.
    pub shows_hosted_windows: bool,
    /// Hosted top-level traversables made since the embedder last asked, and ones closed since
    /// ([Engine::take_window_changes]).
    pub window_changes: Vec<crate::engine::WindowChange>,
    /// The popup blocker is on ([Engine::set_popup_blocker]).
    pub popup_blocker: bool,
}

impl Engine {
    /// Sets (or removes) the navigation filter, for addresses of `schemes`.
    pub fn set_navigation_filter(&self, schemes: &[&str], filter: Option<NavigationFilter>) {
        let mut h = self.embedder.borrow_mut();
        h.schemes = schemes.iter().map(|s| (*s).to_owned()).collect();
        h.filter = filter;
    }

    /// Whether the filter takes this navigation.
    /// Whether top-level navigations begin in the browser (M5): the engine then hands each to it
    /// ([`Engine::take_begun_navigations`]) instead of fetching, and commits what the browser hands
    /// back ([`Engine::commit_navigation`]); `history.go()` goes to the browser too. Off, as in the
    /// headless modes, the engine navigates by itself.
    pub fn set_browser_navigation(&self, on: bool) {
        self.embedder.borrow_mut().browser_navigates = on;
    }

    pub(crate) fn browser_navigates(&self) -> bool {
        self.embedder.borrow().browser_navigates
    }

    /// The navigations the page asked the browser to begin since the last call.
    pub fn take_begun_navigations(&self) -> Vec<BeginNavigation> {
        std::mem::take(&mut self.embedder.borrow_mut().begun)
    }

    /// The same-document URL changes of the top-level document since the last call.
    pub fn take_same_document_changes(&self) -> Vec<SameDocumentChange> {
        std::mem::take(&mut self.embedder.borrow_mut().same_document)
    }

    /// The session history traversals the page asked for since the last call (steps; negative is
    /// back), for the browser to perform.
    pub fn take_traversals(&self) -> Vec<isize> {
        std::mem::take(&mut self.embedder.borrow_mut().traversals)
    }

    pub(crate) fn note_begun(
        &self,
        params: snowlark_net::navigation::NavigationParams,
        begun: Begun,
    ) {
        let mut h = self.embedder.borrow_mut();
        h.next_seq += 1;
        let seq = h.next_seq;
        h.begun.push(BeginNavigation { seq, params });
        h.begun_kept.push((seq, begun));
        if h.begun_kept.len() > BEGUN_KEPT {
            h.begun_kept.remove(0);
        }
    }

    pub(crate) fn take_begun(&self, seq: u64) -> Option<Begun> {
        let mut h = self.embedder.borrow_mut();
        let i = h.begun_kept.iter().position(|(s, _)| *s == seq)?;
        Some(h.begun_kept.remove(i).1)
    }

    pub(crate) fn note_same_document(&self, doc: NodeId, url: &str, push: bool, index: usize) {
        if !self.browser_navigates() || self.document() != Some(doc) {
            return;
        }
        self.embedder
            .borrow_mut()
            .same_document
            .push(SameDocumentChange {
                url: url.to_owned(),
                push,
                index,
            });
    }

    pub(crate) fn embedder_takes(&self, id: usize, url: &url::Url) -> bool {
        self.embedder
            .borrow()
            .filter
            .as_ref()
            .is_some_and(|f| f(id, url))
    }

    /// The element of the last `contextmenu` event nothing canceled (the embedder shows its menu).
    pub fn take_context_menu(&self) -> Option<NodeId> {
        self.embedder.borrow_mut().context_menu.take()
    }

    /// What a context menu for `element` (else the focused element) is about.
    pub fn context_target(&self, element: Option<NodeId>) -> ContextTarget {
        let doc = self.document();
        let element = element
            .filter(|&e| self.dom().is_alive(e))
            .or_else(|| doc.and_then(|d| self.focused_element(d)));
        let link = element.and_then(|e| self.hyperlink_at(e));
        let image = element.and_then(|e| {
            let src = {
                let dom = self.dom();
                if !dom.is_html_element_named(e, &local_name!("img")) {
                    return None;
                }
                dom.get_attribute(e, "src")?.trim().to_owned()
            };
            (!src.is_empty())
                .then(|| self.base_url_of(e).join(&src).ok())
                .flatten()
        });
        let selection = element
            .map(|e| self.dom().node_document(e))
            .or(doc)
            .and_then(|d| self.selection_text(d))
            .filter(|t| !t.trim().is_empty());
        let editable = element.is_some_and(|e| self.is_text_control(e));
        ContextTarget {
            element,
            link,
            image,
            selection,
            editable,
        }
    }

    /// Whether the embedder serves addresses of `scheme`.
    pub(crate) fn embedder_scheme(&self, scheme: &str) -> bool {
        self.embedder.borrow().schemes.iter().any(|s| s == scheme)
    }

    /// The user prefers a dark color scheme (`prefers-color-scheme: dark`) or not: every rendered
    /// document's media queries are evaluated again.
    pub fn set_color_scheme(&self, dark: bool) {
        {
            let mut st = self.render.borrow_mut();
            match st.setup.as_mut() {
                Some(s) if s.dark != dark => s.dark = dark,
                _ => return,
            }
        }
        let scheme = if dark {
            snowlark_style::ColorScheme::Dark
        } else {
            snowlark_style::ColorScheme::Light
        };
        self.for_each_renderer(|r, st| {
            r.set_color_scheme(scheme);
            st.dirty = true;
        });
        self.render.borrow_mut().dirty = true;
    }
}
