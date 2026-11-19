// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `Document` factory methods (DOM §4.5), `DOMImplementation` (§4.5.1), and the namespace lookups of
//! `Node` (§4.4 "locate a namespace").

use html5ever::{local_name, ns, LocalName, Namespace, QualName};

use crate::dom::Dom;
use crate::error::{DomError, DomResult};
use crate::names::{self, NameContext};
use crate::node::{DocumentKind, NodeData, NodeId};

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

impl Dom {
    /// `Document.createElement(localName, options)`; `is` is the options' `is` member.
    pub fn create_element(
        &mut self,
        document: NodeId,
        local_name: &str,
        is: Option<&str>,
    ) -> DomResult<NodeId> {
        // 1.
        if !names::is_valid_element_local_name(local_name) {
            return Err(DomError::InvalidCharacter);
        }
        // 2.
        let html = self.is_html_document(document);
        let local = if html {
            LocalName::from(local_name.to_ascii_lowercase())
        } else {
            LocalName::from(local_name)
        };
        // 5.
        let namespace = if html || self.content_type(document) == "application/xhtml+xml" {
            ns!(html)
        } else {
            ns!()
        };
        Ok(self.create_element_with_name(
            document,
            QualName::new(None, namespace, local),
            is.map(LocalName::from),
        ))
    }

    /// `Document.createElementNS(namespace, qualifiedName, options)`.
    pub fn create_element_ns(
        &mut self,
        document: NodeId,
        namespace: Option<&str>,
        qualified_name: &str,
        is: Option<&str>,
    ) -> DomResult<NodeId> {
        let name = names::validate_and_extract(namespace, qualified_name, NameContext::Element)?;
        Ok(self.create_element_with_name(document, name, is.map(LocalName::from)))
    }

    /// `Document.createCDATASection(data)`.
    pub fn create_cdata_section(&mut self, document: NodeId, data: &str) -> DomResult<NodeId> {
        if self.is_html_document(document) {
            return Err(DomError::NotSupported);
        }
        if data.contains("]]>") {
            return Err(DomError::InvalidCharacter);
        }
        Ok(self.create_cdata_unchecked(document, data))
    }

    /// `Document.createProcessingInstruction(target, data)`.
    pub fn create_processing_instruction(
        &mut self,
        document: NodeId,
        target: &str,
        data: &str,
    ) -> DomResult<NodeId> {
        if !names::is_xml_name(target) || data.contains("?>") {
            return Err(DomError::InvalidCharacter);
        }
        Ok(self.create_pi_unchecked(document, target, data))
    }

    /// `Document.createAttribute(localName)`.
    pub fn create_attribute(&mut self, document: NodeId, local_name: &str) -> DomResult<NodeId> {
        if !names::is_valid_attribute_local_name(local_name) {
            return Err(DomError::InvalidCharacter);
        }
        let local = if self.is_html_document(document) {
            LocalName::from(local_name.to_ascii_lowercase())
        } else {
            LocalName::from(local_name)
        };
        Ok(self.create_attr_unchecked(document, QualName::new(None, ns!(), local), ""))
    }

    /// `Document.createAttributeNS(namespace, qualifiedName)`.
    pub fn create_attribute_ns(
        &mut self,
        document: NodeId,
        namespace: Option<&str>,
        qualified_name: &str,
    ) -> DomResult<NodeId> {
        let name = names::validate_and_extract(namespace, qualified_name, NameContext::Attribute)?;
        Ok(self.create_attr_unchecked(document, name, ""))
    }

    /// `DOMImplementation.createDocumentType(name, publicId, systemId)`; the doctype's node document is
    /// `document`.
    pub fn create_document_type(
        &mut self,
        document: NodeId,
        name: &str,
        public_id: &str,
        system_id: &str,
    ) -> DomResult<NodeId> {
        if !names::is_valid_doctype_name(name) {
            return Err(DomError::InvalidCharacter);
        }
        Ok(self.create_doctype_unchecked(document, name, public_id, system_id))
    }

    /// `DOMImplementation.createHTMLDocument(title)`.
    pub fn create_html_document(&mut self, title: Option<&str>) -> NodeId {
        let doc = self.create_document(DocumentKind::Html);
        let doctype = self.create_doctype_unchecked(doc, "html", "", "");
        self.insert(doctype, doc, None, true);
        let html = self.create_html_element(doc, local_name!("html"));
        self.insert(html, doc, None, true);
        let head = self.create_html_element(doc, local_name!("head"));
        self.insert(head, html, None, true);
        if let Some(t) = title {
            let title = self.create_html_element(doc, local_name!("title"));
            self.insert(title, head, None, true);
            let text = self.create_text_node(doc, t);
            self.insert(text, title, None, true);
        }
        let body = self.create_html_element(doc, local_name!("body"));
        self.insert(body, html, None, true);
        doc
    }

    /// `DOMImplementation.createDocument(namespace, qualifiedName, doctype)`: an XML document.
    pub fn create_xml_document(
        &mut self,
        namespace: Option<&str>,
        qualified_name: &str,
        doctype: Option<NodeId>,
    ) -> DomResult<NodeId> {
        let name = if qualified_name.is_empty() {
            None
        } else {
            Some(names::validate_and_extract(
                namespace,
                qualified_name,
                NameContext::Element,
            )?)
        };
        let doc = self.create_document(DocumentKind::Xml);
        let element = name.map(|n| self.create_element_with_name(doc, n, None));
        if let Some(dt) = doctype {
            self.append_child(doc, dt)?;
        }
        if let Some(e) = element {
            self.append_child(doc, e)?;
        }
        let content_type = match namespace {
            Some("http://www.w3.org/1999/xhtml") => "application/xhtml+xml",
            Some("http://www.w3.org/2000/svg") => "image/svg+xml",
            _ => "application/xml",
        };
        self.set_content_type(doc, content_type);
        Ok(doc)
    }

    // ----- namespace lookups -------------------------------------------------------------------------------

    /// DOM §4.4 "locate a namespace" for `prefix` (`None` for the default namespace).
    pub fn locate_namespace(&self, node: NodeId, prefix: Option<&str>) -> Option<String> {
        let element = match &self.n(node).data {
            NodeData::Element(_) => node,
            NodeData::Document(_) => self.document_element(node)?,
            NodeData::DocumentType(_)
            | NodeData::DocumentFragment { .. }
            | NodeData::ShadowRoot(_) => return None,
            NodeData::Attr(a) => a.owner?,
            _ => self.parent_element(node)?,
        };
        let mut cur = Some(element);
        while let Some(el) = cur {
            let e = self.el(el);
            match prefix {
                Some("xml") => return Some(XML_NS.to_owned()),
                Some("xmlns") => return Some(XMLNS_NS.to_owned()),
                _ => {}
            }
            if !e.name.ns.is_empty() && e.name.prefix.as_deref() == prefix {
                return Some(e.name.ns.to_string());
            }
            let decl = e.attrs.iter().find(|a| {
                &*a.name.ns == XMLNS_NS
                    && match prefix {
                        Some(p) => a.name.prefix.as_deref() == Some("xmlns") && &*a.name.local == p,
                        None => a.name.prefix.is_none() && &*a.name.local == "xmlns",
                    }
            });
            if let Some(a) = decl {
                return (!a.value.is_empty()).then(|| a.value.to_string());
            }
            cur = self.parent_element(el);
        }
        None
    }

    fn locate_prefix(&self, element: NodeId, namespace: &str) -> Option<String> {
        let mut cur = Some(element);
        while let Some(el) = cur {
            let e = self.el(el);
            if &*e.name.ns == namespace {
                if let Some(p) = &e.name.prefix {
                    return Some(p.to_string());
                }
            }
            if let Some(a) = e
                .attrs
                .iter()
                .find(|a| a.name.prefix.as_deref() == Some("xmlns") && &*a.value == namespace)
            {
                return Some(a.name.local.to_string());
            }
            cur = self.parent_element(el);
        }
        None
    }

    /// `Node.lookupPrefix(namespace)`.
    pub fn lookup_prefix(&self, node: NodeId, namespace: Option<&str>) -> Option<String> {
        let namespace = namespace.filter(|n| !n.is_empty())?;
        let element = match &self.n(node).data {
            NodeData::Element(_) => node,
            NodeData::Document(_) => self.document_element(node)?,
            NodeData::DocumentType(_)
            | NodeData::DocumentFragment { .. }
            | NodeData::ShadowRoot(_) => return None,
            NodeData::Attr(a) => a.owner?,
            _ => self.parent_element(node)?,
        };
        self.locate_prefix(element, namespace)
    }

    /// `Node.lookupNamespaceURI(prefix)`.
    pub fn lookup_namespace_uri(&self, node: NodeId, prefix: Option<&str>) -> Option<String> {
        self.locate_namespace(node, prefix.filter(|p| !p.is_empty()))
    }

    /// `Node.isDefaultNamespace(namespace)`.
    pub fn is_default_namespace(&self, node: NodeId, namespace: Option<&str>) -> bool {
        let namespace = namespace.filter(|n| !n.is_empty());
        self.locate_namespace(node, None).as_deref() == namespace
    }

    /// The `Namespace` atom for an optional namespace string (null and "" are the null namespace).
    pub fn namespace_atom(namespace: Option<&str>) -> Namespace {
        namespace.map_or(ns!(), Namespace::from)
    }
}
