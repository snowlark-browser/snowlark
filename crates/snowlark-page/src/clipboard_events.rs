// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The clipboard events and actions (Clipboard API and events §5, §8): Ctrl+C, Ctrl+X and Ctrl+V
//! (and Ctrl+Insert, Shift+Delete, Shift+Insert) fire `copy`, `cut` and `paste` (a trusted
//! `ClipboardEvent`) at the selection's element or the focused control, with a `clipboardData` that
//! holds what the clipboard has (`paste`, read-only) or what the page puts there (`copy`, `cut`).
//! Unless canceled, the default action runs: the selection's text and markup go to the clipboard
//! (and a cut deletes the selection, `deleteByCut`); a paste inserts into an editing host
//! (`insertFromPaste` with a `dataTransfer`), its markup sanitized, or plain text. A canceled copy or
//! cut puts the event's `clipboardData` on the clipboard instead.

use snowlark_bindings::rt::{Cx, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{NodeId, NodeType, ParseOptions};

use crate::editing::EditInput;
use crate::engine::datatransfer::{DataTransferData, Item, ItemData, StoreMode};
use crate::engine::events::EventExt;
use crate::engine::ranges::BoundaryPair;
use crate::engine::{node_handle, Engine, Object};
use crate::keyboard::KeyOutcome;

/// Elements a paste leaves out with their contents: script, style and metadata, embedded
/// documents and plugins, and foreign content (which can carry scripts of its own).
const DROPPED: &[&str] = &[
    "script", "style", "link", "meta", "title", "base", "head", "iframe", "frame", "frameset",
    "object", "embed", "applet", "noscript", "template", "svg", "math", "portal",
];

/// Attributes whose value is a URL that a paste must not keep when it runs script.
const URL_ATTRS: &[&str] = &[
    "href",
    "src",
    "action",
    "formaction",
    "xlink:href",
    "background",
];

/// What a clipboard action ends with, for the key handlers.
pub(crate) enum ClipboardOutcome {
    /// The event was canceled: nothing more happens (a copy or cut already put the event's data
    /// on the clipboard).
    Canceled,
    /// The default action runs.
    Default,
}

impl Engine {
    /// Clipboard API §8.1 "fire a clipboard event" step 3, the target: in an editable context, the
    /// element at the start of the selection; else the focused element; else the body.
    pub(crate) fn clipboard_target(&self, doc: NodeId) -> Option<NodeId> {
        if let Some(f) = self.focused_element(doc) {
            if self.is_text_control(f) {
                return Some(f);
            }
        }
        if self.active_editing_host().is_some() {
            if let Some((r, _)) = self.doc_selection(doc) {
                let dom = self.dom();
                let n = r.start.0;
                let el = if dom.is_element(n) {
                    Some(n)
                } else {
                    dom.parent_element(n)
                };
                if el.is_some() {
                    return el;
                }
            }
        }
        let dom = self.dom();
        self.focused_element(doc)
            .or_else(|| dom.body(doc))
            .or_else(|| dom.document_element(doc))
    }

    /// A new DataTransfer holding `items` (type, string), read-only when `read_only`.
    pub(crate) fn new_clipboard_data(&self, items: &[(&str, &str)], read_only: bool) -> Handle {
        self.alloc(Object::DataTransfer(Box::new(DataTransferData {
            items: items
                .iter()
                .map(|(t, d)| Item {
                    data: ItemData::Text((*d).to_owned()),
                    type_: (*t).to_owned(),
                    object: None,
                })
                .collect(),
            drop_effect: "none".into(),
            effect_allowed: "uninitialized".into(),
            mode: if read_only {
                StoreMode::ReadOnly
            } else {
                StoreMode::ReadWrite
            },
            ..Default::default()
        })))
    }

    /// A string item of a DataTransfer, if it has one of that type.
    fn clipboard_string(&self, dt: Handle, type_: &str) -> Option<String> {
        match &*self.obj(dt) {
            Object::DataTransfer(d) => d.items.iter().find_map(|i| match &i.data {
                ItemData::Text(s) if i.type_ == type_ => Some(s.clone()),
                _ => None,
            }),
            _ => None,
        }
    }

    /// Fires a trusted `ClipboardEvent` of `type_` at `target` with `data` as its `clipboardData`.
    /// Returns whether it was not canceled.
    fn fire_clipboard_event(&self, cx: &mut Cx, target: NodeId, type_: &str, data: Handle) -> bool {
        let e = self.new_event(
            InterfaceId::ClipboardEvent,
            type_,
            true,
            true,
            true,
            EventExt::Clipboard(Some(data)),
        );
        self.ev_mut(e, |ev| ev.is_trusted = true);
        self.dispatch(cx, e, node_handle(target), false)
    }

    /// The document selection's markup (its contents cloned and serialized), if it is not
    /// collapsed.
    pub(crate) fn selection_html(&self, cx: &mut Cx, doc: NodeId) -> Option<String> {
        let (r, _) = self.doc_selection(doc)?;
        if r.start == r.end {
            return None;
        }
        let frag = self.clone_contents_of(cx, r).ok()?;
        let html = self.dom().inner_html(frag);
        self.after_mutation(cx);
        self.dom_mut().free_subtree(frag);
        Some(html)
    }

    /// The copy and cut actions (§8.2, §8.3) up to their default action: `copy` (or `cut`) at the
    /// clipboard target. Canceled with data, the event's data goes to the clipboard (`out`).
    pub(crate) fn clipboard_copy_event(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        cut: bool,
        out: &mut KeyOutcome,
    ) -> ClipboardOutcome {
        let Some(target) = self.clipboard_target(doc) else {
            return ClipboardOutcome::Default;
        };
        let data = self.new_clipboard_data(&[], false);
        let type_ = if cut { "cut" } else { "copy" };
        if self.fire_clipboard_event(cx, target, type_, data) {
            return ClipboardOutcome::Default;
        }
        let text = self.clipboard_string(data, "text/plain");
        let html = self.clipboard_string(data, "text/html");
        if text.is_some() || html.is_some() {
            out.copy = Some(text.unwrap_or_default());
            out.copy_html = html;
        }
        ClipboardOutcome::Canceled
    }

    /// The paste action (§8.4) up to its default action: `paste` at the clipboard target with the
    /// clipboard's text and markup, read-only.
    pub(crate) fn clipboard_paste_event(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        text: Option<&str>,
        html: Option<&str>,
    ) -> ClipboardOutcome {
        let Some(target) = self.clipboard_target(doc) else {
            return ClipboardOutcome::Default;
        };
        let mut items = Vec::new();
        if let Some(t) = text {
            items.push(("text/plain", t));
        }
        if let Some(h) = html {
            items.push(("text/html", h));
        }
        let data = self.new_clipboard_data(&items, true);
        if self.fire_clipboard_event(cx, target, "paste", data) {
            ClipboardOutcome::Default
        } else {
            ClipboardOutcome::Canceled
        }
    }

    /// Ctrl+C (or Ctrl+Insert) outside a text control: `copy`, then the selection's text and
    /// markup to the clipboard.
    pub(crate) fn copy_selection(&self, cx: &mut Cx, doc: NodeId, out: &mut KeyOutcome) {
        if let ClipboardOutcome::Default = self.clipboard_copy_event(cx, doc, false, out) {
            out.copy = self.selection_text(doc);
            if out.copy.is_some() {
                out.copy_html = self.selection_html(cx, doc);
            }
        }
    }

    /// Ctrl+X (or Shift+Delete) in an editing host: `cut`, then the selection's text and markup to
    /// the clipboard and the selection deleted (`deleteByCut`).
    pub(crate) fn cut_selection(&self, cx: &mut Cx, host: NodeId, out: &mut KeyOutcome) {
        let doc = self.dom().node_document(host);
        if let ClipboardOutcome::Canceled = self.clipboard_copy_event(cx, doc, true, out) {
            return;
        }
        let Some((r, _)) = self.doc_selection(doc) else {
            return;
        };
        if r.start == r.end {
            return;
        }
        out.copy = self.selection_text(doc);
        out.copy_html = self.selection_html(cx, doc);
        self.user_edit(cx, host, EditInput::new("deleteByCut", None, r), |e, cx| {
            e.ec_delete(cx, doc);
        });
    }

    /// Ctrl+V (or Shift+Insert) in an editing host: `paste`, then, unless canceled, `beforeinput`
    /// (`insertFromPaste` with the clipboard's data) and the insertion: the clipboard's markup,
    /// sanitized, where the host takes markup; else its text, line by line, lines after the first
    /// as new paragraphs (line breaks in a plaintext-only host).
    pub(crate) fn paste_into_host(
        &self,
        cx: &mut Cx,
        host: NodeId,
        text: Option<&str>,
        html: Option<&str>,
    ) {
        let doc = self.dom().node_document(host);
        if text.is_none() && html.is_none() {
            return;
        }
        if let ClipboardOutcome::Canceled = self.clipboard_paste_event(cx, doc, text, html) {
            return;
        }
        if !self.dom().is_alive(host) {
            return;
        }
        let Some((r, _)) = self.doc_selection(doc) else {
            return;
        };
        let text = text.map(|t| t.replace("\r\n", "\n").replace('\r', "\n"));
        let mut items = Vec::new();
        if let Some(t) = &text {
            items.push(("text/plain", t.as_str()));
        }
        if let Some(h) = html {
            items.push(("text/html", h));
        }
        let data_transfer = self.new_clipboard_data(&items, true);
        let plaintext = self
            .dom()
            .get_attribute(host, "contenteditable")
            .is_some_and(|v| v.eq_ignore_ascii_case("plaintext-only"));
        let markup = html.filter(|_| !plaintext);
        self.user_edit(
            cx,
            host,
            EditInput {
                input_type: "insertFromPaste",
                data: None,
                ranges: vec![r],
                data_transfer: Some(data_transfer),
            },
            |e, cx| {
                if let Some(markup) = markup {
                    if let Some(frag) = e.paste_fragment(cx, host, markup) {
                        e.ec_insert_fragment(cx, doc, frag);
                        return;
                    }
                }
                let Some(text) = text.as_deref() else {
                    return;
                };
                for (i, line) in text.split('\n').enumerate() {
                    if i > 0 {
                        if plaintext {
                            e.ec_insert_line_break(cx, doc);
                        } else {
                            e.ec_insert_paragraph(cx, doc);
                        }
                    }
                    if !line.is_empty() {
                        e.ec_insert_text(cx, doc, line);
                    }
                }
            },
        );
    }

    /// The pasted markup as a sanitized fragment of the host's document: parsed in the host's
    /// context, the part between `<!--StartFragment-->` and `<!--EndFragment-->` when the markup
    /// marks one (the system clipboard's HTML format), without scripts, event handler attributes,
    /// script URLs, embedded documents, metadata and comments. None when nothing is left.
    pub(crate) fn paste_fragment(&self, cx: &mut Cx, host: NodeId, markup: &str) -> Option<NodeId> {
        let parsed = self
            .dom_mut()
            .parse_fragment(host, markup, false, ParseOptions::default());
        // The marked part, with the ancestors it sits in (as range contents are cloned).
        let marks = {
            let dom = self.dom();
            let find = |name: &str| {
                dom.descendants(parsed).find(|&n| {
                    dom.node_type(n) == NodeType::Comment
                        && dom.character_data(n).is_some_and(|d| d.trim() == name)
                })
            };
            find("StartFragment").zip(find("EndFragment"))
        };
        let frag = match marks {
            Some((start, end)) => {
                let (sp, si, ep, ei) = {
                    let dom = self.dom();
                    (
                        dom.parent(start)?,
                        dom.index(start) as u32,
                        dom.parent(end)?,
                        dom.index(end) as u32,
                    )
                };
                let r = BoundaryPair {
                    start: (sp, si + 1),
                    end: (ep, ei),
                };
                let f = self.clone_contents_of(cx, r).ok()?;
                self.after_mutation(cx);
                self.dom_mut().free_subtree(parsed);
                f
            }
            None => parsed,
        };
        self.sanitize_paste(frag);
        self.dom().has_child_nodes(frag).then_some(frag)
    }

    /// Removes the scripts of a fragment that markup inserted by an edit makes: script elements,
    /// event handler attributes and script URLs.
    pub(crate) fn strip_script_content(&self, frag: NodeId) {
        let scripts: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(frag)
                .filter(|&n| dom.is_script_element(n))
                .collect()
        };
        for s in scripts {
            if self.dom().parent(s).is_some() {
                self.dom_mut().remove_node(s);
            }
        }
        self.strip_script_attributes(frag);
    }

    /// Removes event handler attributes, `srcdoc` and script URLs from a fragment's elements.
    fn strip_script_attributes(&self, frag: NodeId) {
        let elements: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(frag)
                .filter(|&n| dom.is_element(n))
                .collect()
        };
        for n in elements {
            let bad: Vec<String> = {
                let dom = self.dom();
                dom.attrs(n)
                    .iter()
                    .filter(|a| {
                        let name = a.qualified_name().to_ascii_lowercase();
                        let v: String = a
                            .value()
                            .chars()
                            .filter(|c| !c.is_ascii_whitespace() && !c.is_control())
                            .collect::<String>()
                            .to_ascii_lowercase();
                        name.starts_with("on")
                            || name == "srcdoc"
                            || (URL_ATTRS.contains(&name.as_str())
                                && (v.starts_with("javascript:") || v.starts_with("vbscript:")))
                    })
                    .map(|a| a.qualified_name().into_owned())
                    .collect()
            };
            for name in bad {
                self.dom_mut().remove_attribute(n, &name);
            }
        }
    }

    /// Removes from a pasted fragment what must not come in with a paste.
    fn sanitize_paste(&self, frag: NodeId) {
        let nodes: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(frag).filter(|&n| n != frag).collect()
        };
        for n in nodes {
            let drop = {
                let dom = self.dom();
                if !dom.is_alive(n) || dom.parent(n).is_none() {
                    continue;
                }
                match dom.node_type(n) {
                    NodeType::Comment | NodeType::ProcessingInstruction => true,
                    NodeType::Element => {
                        let name = dom.local_name(n);
                        DROPPED.contains(&&**name) || !dom.is_html_element_in_html_document(n)
                    }
                    _ => false,
                }
            };
            if drop {
                self.dom_mut().remove_node(n);
            }
        }
        self.strip_script_attributes(frag);
    }

    /// Ctrl+V in a text control: `paste` at the control; unless canceled, the clipboard's text.
    pub(crate) fn paste_event_for_control(&self, cx: &mut Cx, el: NodeId, text: &str) -> bool {
        let doc = self.dom().node_document(el);
        matches!(
            self.clipboard_paste_event(cx, doc, Some(text), None),
            ClipboardOutcome::Default
        ) && self.dom().is_alive(el)
    }
}
