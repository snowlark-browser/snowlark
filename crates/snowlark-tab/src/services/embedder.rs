// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tab's embedder services across processes: the window's clipboard, the browser's own pages
//! (`snowlark:`, `about:`, `view-source:`) and the content filter. Each is a call answered by bulk,
//! since a clipboard's text, a view-source body and a document's filters can each pass a frame.
//!
//! Not yet checked by the browser (M5): that a clipboard read follows user activation, and that only
//! a renderer locked to `snowlark:` asks for a settings page.

use serde::{Deserialize, Serialize};
use snowlark_ipc::{Bulk, Error, Replier};
use snowlark_net::Url;
use snowlark_page::{ContentFilter, DocumentFilters};

use super::{answer, ask, pack, unpack, BrowserPort, LocalServices, RendererPort};
use crate::driver::{Clipboard, InternalAnswer, InternalPages, InternalQuery};

/// Renderer to browser.
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    /// A call: the answer is an `Option<String>`.
    ClipboardText,
    /// A `String`.
    SetClipboardText(Bulk),
    /// A call: the answer is an `Option<String>`, the clipboard's markup.
    ClipboardHtml,
    /// A `(String, Option<String>)`: text and markup.
    SetClipboardTextAndHtml(Bulk),
    /// A call with an [`InternalQuery`]: the answer is an [`InternalAnswer`].
    Internal(Bulk),
    /// A call: the answer is an `Option<DocumentFilters>`.
    DocumentFilters { url: String, top: String },
    /// A call with a [`GenericQuery`]: the answer is a `Vec<String>`.
    GenericSelectors(Bulk),
}

/// What [`ContentFilter::generic_selectors`] is asked. A document's filters stay in the renderer:
/// what a filter uses of them for generic hiding, its exceptions, goes with the question.
#[derive(Debug, Serialize, Deserialize)]
pub struct GenericQuery {
    pub exceptions: Vec<String>,
    pub generic: bool,
    pub classes: Vec<String>,
    pub ids: Vec<String>,
}

fn send(port: &RendererPort, message: Up) {
    let _ = port.send(&super::Up::Embedder(message));
}

/// The window's clipboard, as a renderer reaches it.
pub struct RemoteClipboard(pub RendererPort);

impl Clipboard for RemoteClipboard {
    fn text(&self) -> Option<String> {
        ask(&self.0, &super::Up::Embedder(Up::ClipboardText)).flatten()
    }

    fn set_text(&self, text: &str) {
        if let Ok(text) = pack(&self.0, &text) {
            send(&self.0, Up::SetClipboardText(text));
        }
    }

    fn html(&self) -> Option<String> {
        ask(&self.0, &super::Up::Embedder(Up::ClipboardHtml)).flatten()
    }

    fn set_text_and_html(&self, text: &str, html: Option<&str>) {
        if let Ok(both) = pack(&self.0, &(text, html)) {
            send(&self.0, Up::SetClipboardTextAndHtml(both));
        }
    }
}

/// The browser's own pages, as a renderer reaches them.
pub struct RemoteInternalPages(pub RendererPort);

impl InternalPages for RemoteInternalPages {
    fn request(&self, request: InternalQuery) -> InternalAnswer {
        let fallback = || match &request {
            InternalQuery::Page { url } => InternalAnswer::Page {
                shown: url.clone(),
                html: None,
            },
            InternalQuery::Kind { .. } => InternalAnswer::Kind(crate::driver::InternalKind::Other),
            InternalQuery::ViewSourceTarget { .. } => InternalAnswer::ViewSourceTarget(None),
            InternalQuery::ViewSource { .. } => InternalAnswer::ViewSource {
                html: String::new(),
            },
        };
        let Ok(query) = pack(&self.0, &request) else {
            return fallback();
        };
        ask(&self.0, &super::Up::Embedder(Up::Internal(query))).unwrap_or_else(fallback)
    }
}

/// The tab's content filter, as a renderer reaches it.
pub struct RemoteContentFilter(pub RendererPort);

impl ContentFilter for RemoteContentFilter {
    fn document_filters(&self, url: &Url, top: &Url) -> Option<DocumentFilters> {
        let message = super::Up::Embedder(Up::DocumentFilters {
            url: url.as_str().to_owned(),
            top: top.as_str().to_owned(),
        });
        ask(&self.0, &message).flatten()
    }

    fn generic_selectors(
        &self,
        filters: &DocumentFilters,
        classes: &[String],
        ids: &[String],
    ) -> Vec<String> {
        let query = GenericQuery {
            exceptions: filters.exceptions.iter().cloned().collect(),
            generic: filters.generic,
            classes: classes.to_vec(),
            ids: ids.to_vec(),
        };
        let Ok(query) = pack(&self.0, &query) else {
            return Vec::new();
        };
        ask(&self.0, &super::Up::Embedder(Up::GenericSelectors(query))).unwrap_or_default()
    }
}

/// The browser's end of one renderer's embedder services.
pub struct EmbedderHost {
    port: BrowserPort,
    local: LocalServices,
}

impl EmbedderHost {
    pub fn new(port: BrowserPort, local: LocalServices) -> EmbedderHost {
        EmbedderHost { port, local }
    }

    fn bad(&self, why: String) -> Error {
        self.port.bad_message(&why);
        Error::BadMessage(why)
    }

    pub(super) fn serve(&self, message: Up, replier: Option<Replier>) -> Result<(), Error> {
        let port = &self.port;
        match message {
            Up::ClipboardText => answer(port, replier, &self.local.clipboard.text()),
            Up::SetClipboardText(text) => {
                let text: String = unpack(port, text)?;
                self.local.clipboard.set_text(&text);
                Ok(())
            }
            Up::ClipboardHtml => answer(port, replier, &self.local.clipboard.html()),
            Up::SetClipboardTextAndHtml(both) => {
                let (text, html): (String, Option<String>) = unpack(port, both)?;
                self.local
                    .clipboard
                    .set_text_and_html(&text, html.as_deref());
                Ok(())
            }
            Up::Internal(query) => {
                let query: InternalQuery = unpack(port, query)?;
                // (the browser's own pages are served to its own renderer only)
                if let InternalQuery::Page { url } = &query {
                    if url.starts_with("snowlark:") && !super::site_lock::internal(port.policy()) {
                        super::site_lock::check(
                            port,
                            self.local.site_lock,
                            Some(format!("the page {url}, the browser's own")),
                        )?;
                    }
                }
                answer(port, replier, &self.local.internal.request(query))
            }
            Up::DocumentFilters { url, top } => {
                let (Ok(url), Ok(top)) = (Url::parse(&url), Url::parse(&top)) else {
                    return Err(
                        self.bad("document filters for an address that does not parse".into())
                    );
                };
                let filters = self
                    .local
                    .content_filter
                    .as_ref()
                    .and_then(|f| f.document_filters(&url, &top));
                answer(port, replier, &filters)
            }
            Up::GenericSelectors(query) => {
                let q: GenericQuery = unpack(port, query)?;
                let filters = DocumentFilters {
                    exceptions: q.exceptions.into_iter().collect(),
                    generic: q.generic,
                    ..DocumentFilters::default()
                };
                let selectors = self
                    .local
                    .content_filter
                    .as_ref()
                    .map(|f| f.generic_selectors(&filters, &q.classes, &q.ids))
                    .unwrap_or_default();
                answer(port, replier, &selectors)
            }
        }
    }
}
