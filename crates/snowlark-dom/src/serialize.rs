// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML fragment serialization (HTML §13.3 "Serializing HTML fragments"): the `innerHTML` / `outerHTML`
//! getters and `getHTML()`.
//!
//! XML serialization (for elements of XML documents) is not here yet; those serialize as HTML for now.

use html5ever::{local_name, ns, LocalName};

use crate::dom::Dom;
use crate::node::{NodeData, NodeId};

/// Options of the serialization algorithm (HTML `GetHTMLOptions` plus the scripting flag).
#[derive(Clone, Copy, Debug, Default)]
pub struct SerializeOptions<'a> {
    /// `serializableShadowRoots`: include shadow roots whose `serializable` is true.
    pub serializable_shadow_roots: bool,
    /// `shadowRoots`: shadow roots to include regardless.
    pub shadow_roots: &'a [NodeId],
    /// Whether scripting is enabled for the node (`noscript` contents are then raw text).
    pub scripting_enabled: bool,
}

/// HTML "serializes as void": an HTML element whose end tag is never written.
pub fn serializes_as_void(local: &LocalName) -> bool {
    matches!(
        *local,
        local_name!("area")
            | local_name!("base")
            | local_name!("basefont")
            | local_name!("bgsound")
            | local_name!("br")
            | local_name!("col")
            | local_name!("embed")
            | local_name!("frame")
            | local_name!("hr")
            | local_name!("img")
            | local_name!("input")
            | local_name!("keygen")
            | local_name!("link")
            | local_name!("meta")
            | local_name!("param")
            | local_name!("source")
            | local_name!("track")
            | local_name!("wbr")
    )
}

/// "Escaping a string" (HTML §13.3): `&`, NBSP, `<` and `>` always; `"` in attribute mode.
pub fn escape(out: &mut String, s: &str, attribute_mode: bool) {
    let mut last = 0;
    for (i, c) in s.char_indices() {
        let rep = match c {
            '&' => "&amp;",
            '\u{A0}' => "&nbsp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' if attribute_mode => "&quot;",
            _ => continue,
        };
        out.push_str(&s[last..i]);
        out.push_str(rep);
        last = i + c.len_utf8();
    }
    out.push_str(&s[last..]);
}

enum Step {
    Node(NodeId),
    End(NodeId),
    ShadowStart(NodeId),
    ShadowEnd,
}

impl Dom {
    /// `innerHTML` getter for an Element, DocumentFragment, ShadowRoot or Document.
    pub fn inner_html(&self, node: NodeId) -> String {
        self.get_html(node, SerializeOptions::default())
    }

    /// `Element.getHTML(options)` / `ShadowRoot.getHTML(options)`: the HTML fragment serialization
    /// algorithm (HTML §13.3).
    pub fn get_html(&self, node: NodeId, options: SerializeOptions<'_>) -> String {
        let mut out = String::new();
        // 1.
        if let Some(e) = self.element(node) {
            if e.name.ns == ns!(html) && serializes_as_void(&e.name.local) {
                return out;
            }
        }
        let mut steps = Vec::new();
        self.push_contents(node, options, &mut steps);
        self.run(&mut steps, options, &mut out);
        out
    }

    /// `outerHTML` getter: the serialization of a fictional node whose only child is `node`.
    pub fn outer_html(&self, node: NodeId) -> String {
        let mut out = String::new();
        let mut steps = vec![Step::Node(node)];
        self.run(&mut steps, SerializeOptions::default(), &mut out);
        out
    }

    /// Pushes (in reverse) what serializing `node`'s contents produces: steps 3-5.
    fn push_contents(&self, node: NodeId, options: SerializeOptions<'_>, steps: &mut Vec<Step>) {
        // 3.
        let node = self.template_contents(node).unwrap_or(node);
        // 5. (pushed first: runs last)
        let start = steps.len();
        steps.extend(self.children(node).map(Step::Node));
        steps[start..].reverse();
        // 4.
        if let Some(shadow) = self.shadow_root(node) {
            let data = self.shadow_root_data(shadow).expect("shadow root");
            if (data.serializable && options.serializable_shadow_roots)
                || options.shadow_roots.contains(&shadow)
            {
                steps.push(Step::ShadowEnd);
                let start = steps.len();
                steps.extend(self.children(shadow).map(Step::Node));
                steps[start..].reverse();
                steps.push(Step::ShadowStart(shadow));
            }
        }
    }

    fn run(&self, steps: &mut Vec<Step>, options: SerializeOptions<'_>, out: &mut String) {
        while let Some(step) = steps.pop() {
            match step {
                Step::Node(id) => self.serialize_node(id, options, steps, out),
                Step::End(id) => {
                    out.push_str("</");
                    self.push_tag_name(id, out);
                    out.push('>');
                }
                Step::ShadowStart(shadow) => {
                    let d = self.shadow_root_data(shadow).expect("shadow root");
                    out.push_str("<template shadowrootmode=\"");
                    out.push_str(d.mode.as_str());
                    out.push('"');
                    if d.delegates_focus {
                        out.push_str(" shadowrootdelegatesfocus=\"\"");
                    }
                    if d.serializable {
                        out.push_str(" shadowrootserializable=\"\"");
                    }
                    if d.slot_assignment == crate::SlotAssignmentMode::Manual {
                        out.push_str(" shadowrootslotassignment=\"manual\"");
                    }
                    if d.clonable {
                        out.push_str(" shadowrootclonable=\"\"");
                    }
                    out.push('>');
                }
                Step::ShadowEnd => out.push_str("</template>"),
            }
        }
    }

    fn push_tag_name(&self, id: NodeId, out: &mut String) {
        let e = self.el(id);
        if e.name.ns == ns!(html) || e.name.ns == ns!(svg) || e.name.ns == ns!(mathml) {
            out.push_str(&e.name.local);
        } else {
            out.push_str(&self.qualified_name(id));
        }
    }

    fn serialize_node(
        &self,
        id: NodeId,
        options: SerializeOptions<'_>,
        steps: &mut Vec<Step>,
        out: &mut String,
    ) {
        match &self.n(id).data {
            NodeData::Element(e) => {
                out.push('<');
                self.push_tag_name(id, out);
                if let Some(is) = &e.is_value {
                    if !e
                        .attrs
                        .iter()
                        .any(|a| a.name.ns == ns!() && &*a.name.local == "is")
                    {
                        out.push_str(" is=\"");
                        escape(out, is, true);
                        out.push('"');
                    }
                }
                for a in &e.attrs {
                    out.push(' ');
                    let ns = &a.name.ns;
                    if ns.is_empty() {
                        out.push_str(&a.name.local);
                    } else if *ns == ns!(xml) {
                        out.push_str("xml:");
                        out.push_str(&a.name.local);
                    } else if *ns == ns!(xmlns) {
                        if a.name.local != local_name!("xmlns") {
                            out.push_str("xmlns:");
                        }
                        out.push_str(&a.name.local);
                    } else if *ns == ns!(xlink) {
                        out.push_str("xlink:");
                        out.push_str(&a.name.local);
                    } else {
                        out.push_str(&a.qualified_name());
                    }
                    out.push_str("=\"");
                    escape(out, &a.value, true);
                    out.push('"');
                }
                out.push('>');
                if e.name.ns == ns!(html) && serializes_as_void(&e.name.local) {
                    return;
                }
                steps.push(Step::End(id));
                self.push_contents(id, options, steps);
            }
            NodeData::Text(s) | NodeData::CDataSection(s) => {
                let raw = self.parent(id).is_some_and(|p| {
                    self.element(p).is_some_and(|pe| {
                        pe.name.ns == ns!(html)
                            && (matches!(
                                pe.name.local,
                                local_name!("style")
                                    | local_name!("script")
                                    | local_name!("xmp")
                                    | local_name!("iframe")
                                    | local_name!("noembed")
                                    | local_name!("noframes")
                                    | local_name!("plaintext")
                            ) || (pe.name.local == local_name!("noscript")
                                && options.scripting_enabled))
                    })
                });
                if raw {
                    out.push_str(s);
                } else {
                    escape(out, s, false);
                }
            }
            NodeData::Comment(s) => {
                out.push_str("<!--");
                out.push_str(s);
                out.push_str("-->");
            }
            NodeData::ProcessingInstruction { target, data } => {
                out.push_str("<?");
                out.push_str(target);
                out.push(' ');
                out.push_str(data);
                out.push('>');
            }
            NodeData::DocumentType(d) => {
                out.push_str("<!DOCTYPE ");
                out.push_str(&d.name);
                out.push('>');
            }
            NodeData::DocumentFragment { .. } | NodeData::ShadowRoot(_) | NodeData::Document(_) => {
                self.push_contents(id, options, steps);
            }
            NodeData::Attr(_) | NodeData::Free { .. } => {}
        }
    }
}
