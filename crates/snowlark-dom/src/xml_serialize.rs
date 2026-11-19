// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! XML serialization (DOM Parsing and Serialization §3.2, "produce an XML serialization"): what
//! `XMLSerializer.serializeToString` returns, and `innerHTML`/`outerHTML` of nodes in XML documents
//! (with the "require well-formed" flag, failing with an InvalidStateError).

use std::collections::HashMap;

use html5ever::{local_name, ns};

use crate::dom::Dom;
use crate::error::{DomError, DomResult};
use crate::node::{NodeData, NodeId};
use crate::serialize::serializes_as_void;
use crate::xml::{is_name_char, is_name_start_char};

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";
const HTML_NS: &str = "http://www.w3.org/1999/xhtml";

/// The namespace prefix map: namespace (`None` for null) → prefixes, oldest first.
#[derive(Clone, Default)]
struct PrefixMap(HashMap<Option<String>, Vec<String>>);

impl PrefixMap {
    fn new() -> PrefixMap {
        let mut m = PrefixMap::default();
        m.add(Some(XML_NS), "xml");
        m
    }

    fn add(&mut self, ns: Option<&str>, prefix: &str) {
        self.0
            .entry(ns.map(str::to_owned))
            .or_default()
            .push(prefix.to_owned());
    }

    fn has(&self, ns: Option<&str>, prefix: &str) -> bool {
        self.0
            .get(&ns.map(str::to_owned))
            .is_some_and(|l| l.iter().any(|p| p == prefix))
    }

    /// "Retrieving a preferred prefix string": the preferred prefix if the namespace has it, else the
    /// namespace's most recently added prefix.
    fn preferred(&self, preferred: Option<&str>, ns: Option<&str>) -> Option<String> {
        let list = self.0.get(&ns.map(str::to_owned))?;
        if let Some(p) = preferred {
            if list.iter().any(|x| x == p) {
                return Some(p.to_owned());
            }
        }
        list.last().cloned()
    }
}

fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

fn is_xml_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(is_name_start_char) && chars.all(is_name_char)
}

fn escape_text(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

/// "Serializing an attribute value".
fn attribute_value(out: &mut String, v: &str, wf: bool) -> DomResult<()> {
    if wf && !v.chars().all(is_xml_char) {
        return Err(DomError::InvalidState);
    }
    for c in v.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    Ok(())
}

struct Serializer<'a> {
    dom: &'a Dom,
    wf: bool,
    prefix_index: u32,
    out: String,
}

impl Serializer<'_> {
    fn generate_prefix(&mut self, map: &mut PrefixMap, ns: Option<&str>) -> String {
        let p = format!("ns{}", self.prefix_index);
        self.prefix_index += 1;
        map.add(ns, &p);
        p
    }

    fn node(&mut self, n: NodeId, ns: Option<&str>, map: &PrefixMap) -> DomResult<()> {
        let dom = self.dom;
        match dom.data(n) {
            NodeData::Element(_) => self.element(n, ns, map),
            NodeData::Document(_) => {
                if self.wf && dom.document_element(n).is_none() {
                    return Err(DomError::InvalidState);
                }
                for c in dom.children(n) {
                    self.node(c, ns, map)?;
                }
                Ok(())
            }
            NodeData::DocumentFragment { .. } | NodeData::ShadowRoot(_) => {
                for c in dom.children(n) {
                    self.node(c, ns, map)?;
                }
                Ok(())
            }
            NodeData::DocumentType(d) => {
                let (name, public_id, system_id) = (&d.name, &d.public_id, &d.system_id);
                if self.wf {
                    let pubid_ok = public_id.chars().all(|c| {
                        matches!(c, ' ' | '\r' | '\n' | 'a'..='z' | 'A'..='Z' | '0'..='9')
                            || "-'()+,./:=?;!*#@$_%".contains(c)
                    });
                    if !pubid_ok || (system_id.contains('"') && system_id.contains('\'')) {
                        return Err(DomError::InvalidState);
                    }
                }
                self.out.push_str("<!DOCTYPE ");
                self.out.push_str(name);
                if !public_id.is_empty() {
                    self.out.push_str(" PUBLIC \"");
                    self.out.push_str(public_id);
                    self.out.push('"');
                }
                if !system_id.is_empty() && public_id.is_empty() {
                    self.out.push_str(" SYSTEM");
                }
                if !system_id.is_empty() {
                    self.out.push_str(" \"");
                    self.out.push_str(system_id);
                    self.out.push('"');
                }
                self.out.push('>');
                Ok(())
            }
            NodeData::Text(t) => {
                if self.wf && !t.chars().all(is_xml_char) {
                    return Err(DomError::InvalidState);
                }
                escape_text(&mut self.out, t);
                Ok(())
            }
            NodeData::CDataSection(t) => {
                if self.wf && (t.contains("]]>") || !t.chars().all(is_xml_char)) {
                    return Err(DomError::InvalidState);
                }
                self.out.push_str("<![CDATA[");
                self.out.push_str(t);
                self.out.push_str("]]>");
                Ok(())
            }
            NodeData::Comment(t) => {
                if self.wf && (!t.chars().all(is_xml_char) || t.contains("--") || t.ends_with('-'))
                {
                    return Err(DomError::InvalidState);
                }
                self.out.push_str("<!--");
                self.out.push_str(t);
                self.out.push_str("-->");
                Ok(())
            }
            NodeData::ProcessingInstruction { target, data } => {
                if self.wf
                    && (target.contains(':')
                        || target.eq_ignore_ascii_case("xml")
                        || !data.chars().all(is_xml_char)
                        || data.contains("?>"))
                {
                    return Err(DomError::InvalidState);
                }
                self.out.push_str("<?");
                self.out.push_str(target);
                self.out.push(' ');
                self.out.push_str(data);
                self.out.push_str("?>");
                Ok(())
            }
            NodeData::Attr(_) | NodeData::Free { .. } => Ok(()),
        }
    }

    /// "Recording the namespace information": returns the element's default namespace declaration.
    fn record(
        &self,
        el: NodeId,
        map: &mut PrefixMap,
        local_prefixes: &mut HashMap<String, String>,
    ) -> Option<String> {
        let mut default = None;
        for a in self.dom.attrs(el) {
            if a.name.ns != ns!(xmlns) {
                continue;
            }
            if a.name.prefix.is_none() {
                default = Some(a.value.to_string());
                continue;
            }
            let prefix = a.name.local.to_string();
            let ns_def = a.value.to_string();
            if ns_def == XML_NS {
                continue;
            }
            let ns_opt = (!ns_def.is_empty()).then_some(ns_def.as_str());
            if map.has(ns_opt, &prefix) {
                continue;
            }
            map.add(ns_opt, &prefix);
            local_prefixes.insert(prefix, ns_def.clone());
        }
        default
    }

    fn element(
        &mut self,
        el: NodeId,
        inherited: Option<&str>,
        parent_map: &PrefixMap,
    ) -> DomResult<()> {
        let dom = self.dom;
        let e = dom.element(el).expect("an element");
        let local = e.name().local.to_string();
        // 1.
        if self.wf && (local.contains(':') || !is_xml_name(&local)) {
            return Err(DomError::InvalidState);
        }
        let mut map = parent_map.clone();
        let mut local_prefixes: HashMap<String, String> = HashMap::new();
        let local_default = self.record(el, &mut map, &mut local_prefixes);
        let mut inherited_ns: Option<String> = inherited.map(str::to_owned);
        let ns: Option<String> = (!e.name().ns.is_empty()).then(|| e.name().ns.to_string());
        let mut ignore_ns_def = false;
        let qualified;
        self.out.push('<');
        if inherited_ns == ns {
            if local_default.is_some() {
                ignore_ns_def = true;
            }
            qualified = if ns.as_deref() == Some(XML_NS) {
                format!("xml:{local}")
            } else {
                local.clone()
            };
            self.out.push_str(&qualified);
        } else {
            let mut prefix = e.name().prefix.as_ref().map(|p| p.to_string());
            let mut candidate = map.preferred(prefix.as_deref(), ns.as_deref());
            if prefix.as_deref() == Some("xmlns") {
                if self.wf {
                    return Err(DomError::InvalidState);
                }
                candidate = prefix.clone();
            }
            if let Some(c) = candidate {
                qualified = format!("{c}:{local}");
                if let Some(d) = &local_default {
                    if d != XML_NS {
                        inherited_ns = (!d.is_empty()).then(|| d.clone());
                    }
                }
                self.out.push_str(&qualified);
            } else if let Some(p) = prefix.take() {
                let p = if local_prefixes.contains_key(&p) {
                    self.generate_prefix(&mut map, ns.as_deref())
                } else {
                    map.add(ns.as_deref(), &p);
                    p
                };
                qualified = format!("{p}:{local}");
                self.out.push_str(&qualified);
                self.out.push_str(" xmlns:");
                self.out.push_str(&p);
                self.out.push_str("=\"");
                attribute_value(&mut self.out, ns.as_deref().unwrap_or(""), self.wf)?;
                self.out.push('"');
                if let Some(d) = &local_default {
                    inherited_ns = (!d.is_empty()).then(|| d.clone());
                }
            } else if local_default.is_none()
                || local_default.as_deref() != Some(ns.as_deref().unwrap_or(""))
            {
                ignore_ns_def = true;
                qualified = local.clone();
                inherited_ns = ns.clone();
                self.out.push_str(&qualified);
                self.out.push_str(" xmlns=\"");
                attribute_value(&mut self.out, ns.as_deref().unwrap_or(""), self.wf)?;
                self.out.push('"');
            } else {
                qualified = local.clone();
                inherited_ns = ns.clone();
                self.out.push_str(&qualified);
            }
        }
        self.attributes(el, &mut map, &local_prefixes, ignore_ns_def)?;
        let is_html = ns.as_deref() == Some(HTML_NS);
        let contents = if is_html && e.name().local == local_name!("template") {
            dom.template_contents(el).unwrap_or(el)
        } else {
            el
        };
        let empty = dom.first_child(contents).is_none();
        if is_html && empty && serializes_as_void(&e.name().local) {
            self.out.push_str(" />");
            return Ok(());
        }
        if !is_html && empty {
            self.out.push_str("/>");
            return Ok(());
        }
        self.out.push('>');
        for c in dom.children(contents) {
            self.node(c, inherited_ns.as_deref(), &map)?;
        }
        self.out.push_str("</");
        self.out.push_str(&qualified);
        self.out.push('>');
        Ok(())
    }

    fn attributes(
        &mut self,
        el: NodeId,
        map: &mut PrefixMap,
        local_prefixes: &HashMap<String, String>,
        ignore_ns_def: bool,
    ) -> DomResult<()> {
        let mut seen: Vec<(String, String)> = Vec::new();
        for a in self.dom.attrs(el) {
            let ans: Option<String> = (!a.name.ns.is_empty()).then(|| a.name.ns.to_string());
            let local = a.name.local.to_string();
            if self.wf {
                let key = (ans.clone().unwrap_or_default(), local.clone());
                if seen.contains(&key) {
                    return Err(DomError::InvalidState);
                }
                seen.push(key);
            }
            let mut candidate = None;
            if let Some(ns) = &ans {
                let prefix = a.name.prefix.as_ref().map(|p| p.to_string());
                candidate = map.preferred(prefix.as_deref(), Some(ns));
                if ns == XMLNS_NS {
                    let skip = &*a.value == XML_NS
                        || (prefix.is_none() && ignore_ns_def)
                        || (prefix.is_some()
                            && local_prefixes.get(&local).is_some_and(|v| *v != *a.value));
                    if skip {
                        continue;
                    }
                    if self.wf && (&*a.value == XMLNS_NS || a.value.is_empty()) {
                        return Err(DomError::InvalidState);
                    }
                    if prefix.as_deref() == Some("xmlns") {
                        candidate = Some("xmlns".into());
                    }
                } else if candidate.is_none() {
                    let p = self.generate_prefix(map, Some(ns));
                    self.out.push_str(" xmlns:");
                    self.out.push_str(&p);
                    self.out.push_str("=\"");
                    attribute_value(&mut self.out, ns, self.wf)?;
                    self.out.push('"');
                    candidate = Some(p);
                }
            }
            self.out.push(' ');
            if let Some(c) = &candidate {
                self.out.push_str(c);
                self.out.push(':');
            }
            if self.wf
                && (local.contains(':')
                    || !is_xml_name(&local)
                    || (local == "xmlns" && ans.is_none()))
            {
                return Err(DomError::InvalidState);
            }
            self.out.push_str(&local);
            self.out.push_str("=\"");
            attribute_value(&mut self.out, &a.value, self.wf)?;
            self.out.push('"');
        }
        Ok(())
    }
}

impl Dom {
    /// "Produce an XML serialization" of `node` (XMLSerializer, `outerHTML` in XML documents).
    pub fn serialize_xml(&self, node: NodeId, require_well_formed: bool) -> DomResult<String> {
        let mut s = Serializer {
            dom: self,
            wf: require_well_formed,
            prefix_index: 1,
            out: String::new(),
        };
        s.node(node, None, &PrefixMap::new())?;
        Ok(s.out)
    }

    /// The XML serialization of `node`'s children (`innerHTML` in XML documents), in the context of
    /// `node`'s namespace.
    pub fn serialize_xml_children(
        &self,
        node: NodeId,
        require_well_formed: bool,
    ) -> DomResult<String> {
        let mut s = Serializer {
            dom: self,
            wf: require_well_formed,
            prefix_index: 1,
            out: String::new(),
        };
        let ns = self
            .element(node)
            .and_then(|e| (!e.name().ns.is_empty()).then(|| e.name().ns.to_string()));
        let contents = self.template_contents(node).unwrap_or(node);
        let map = PrefixMap::new();
        for c in self.children(contents) {
            s.node(c, ns.as_deref(), &map)?;
        }
        Ok(s.out)
    }
}
