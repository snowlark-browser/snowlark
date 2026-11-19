// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Attributes (DOM §4.9, §4.9.2): the attribute list algorithms and the `Element` attribute methods.

use std::borrow::Cow;

use html5ever::{ns, LocalName, Namespace, Prefix, QualName};

use crate::dom::Dom;
use crate::error::{DomError, DomResult};
use crate::names::{self, NameContext};
use crate::node::{Attr, NodeData, NodeId};
use crate::records::{MutationKind, Reaction};

fn qualified_name_is(attr: &Attr, qualified_name: &str) -> bool {
    match &attr.name.prefix {
        None => &*attr.name.local == qualified_name,
        Some(p) => {
            let (p, l) = (&**p, &*attr.name.local);
            qualified_name.len() == p.len() + 1 + l.len()
                && qualified_name.starts_with(p)
                && qualified_name.as_bytes()[p.len()] == b':'
                && qualified_name.ends_with(l)
        }
    }
}

fn lower_if(html: bool, s: &str) -> Cow<'_, str> {
    if html && s.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(s.to_ascii_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

impl Dom {
    // ----- lookups -----------------------------------------------------------------------------------------

    /// The element's attribute list, in order.
    #[inline]
    pub fn attrs(&self, element: NodeId) -> &[Attr] {
        &self.el(element).attrs
    }

    /// DOM "get an attribute value" by namespace and local name, with atoms (no allocation, no interning).
    #[inline]
    pub fn attr(&self, element: NodeId, ns: &Namespace, local: &LocalName) -> Option<&str> {
        self.el(element)
            .attrs
            .iter()
            .find(|a| a.name.local == *local && a.name.ns == *ns)
            .map(|a| &*a.value)
    }

    /// [`Dom::attr`]'s value as UTF-8 bytes, for byte-wise comparisons (selector matching).
    #[inline]
    pub fn attr_bytes(&self, element: NodeId, ns: &Namespace, local: &LocalName) -> Option<&[u8]> {
        self.el(element)
            .attrs
            .iter()
            .find(|a| a.name.local == *local && a.name.ns == *ns)
            .map(|a| a.value.as_bytes())
    }

    /// Index of the attribute with this namespace and local name.
    pub fn attr_index(&self, element: NodeId, ns: &str, local: &str) -> Option<usize> {
        self.el(element)
            .attrs
            .iter()
            .position(|a| &*a.name.local == local && &*a.name.ns == ns)
    }

    fn attr_index_by_qualified_name(&self, element: NodeId, qualified_name: &str) -> Option<usize> {
        let html = self.is_html_element_in_html_document(element);
        let name = lower_if(html, qualified_name);
        self.el(element)
            .attrs
            .iter()
            .position(|a| qualified_name_is(a, &name))
    }

    /// `Element.getAttribute(qualifiedName)` (DOM "get an attribute by name").
    pub fn get_attribute(&self, element: NodeId, qualified_name: &str) -> Option<&str> {
        self.attr_index_by_qualified_name(element, qualified_name)
            .map(|i| &*self.el(element).attrs[i].value)
    }

    /// `Element.getAttributeNS(namespace, localName)`.
    pub fn get_attribute_ns(
        &self,
        element: NodeId,
        namespace: Option<&str>,
        local: &str,
    ) -> Option<&str> {
        self.attr_index(element, namespace.unwrap_or(""), local)
            .map(|i| &*self.el(element).attrs[i].value)
    }

    /// `Element.hasAttribute(qualifiedName)`.
    pub fn has_attribute(&self, element: NodeId, qualified_name: &str) -> bool {
        self.attr_index_by_qualified_name(element, qualified_name)
            .is_some()
    }

    /// `Element.hasAttributeNS(namespace, localName)`.
    pub fn has_attribute_ns(&self, element: NodeId, namespace: Option<&str>, local: &str) -> bool {
        self.attr_index(element, namespace.unwrap_or(""), local)
            .is_some()
    }

    /// `Element.hasAttributes()`.
    pub fn has_attributes(&self, element: NodeId) -> bool {
        !self.el(element).attrs.is_empty()
    }

    /// `Element.getAttributeNames()`.
    pub fn get_attribute_names(&self, element: NodeId) -> Vec<Cow<'_, str>> {
        self.el(element)
            .attrs
            .iter()
            .map(|a| a.qualified_name())
            .collect()
    }

    // ----- the list algorithms -----------------------------------------------------------------------------

    /// DOM §4.9 "handle attribute changes".
    fn handle_attribute_changes(
        &mut self,
        element: NodeId,
        index: usize,
        old_value: Option<String>,
        removed: Option<&Attr>,
    ) {
        let (local, ns, new_value) = match removed {
            Some(a) => (a.name.local.clone(), a.name.ns.clone(), None),
            None => {
                let a = &self.el(element).attrs[index];
                let value_bits = crate::filter::bits_of_changed_value(a);
                let names = (
                    a.name.local.clone(),
                    a.name.ns.clone(),
                    Some(a.value.to_string()),
                );
                // (a replaced attribute has the same name as the one before it)
                let bits = match old_value {
                    None => self.name_bits.get(&names.0) | value_bits,
                    Some(_) => value_bits,
                };
                self.add_filter_bits(element, bits);
                names
            }
        };
        if let Some(key) = crate::ids::key_of(&ns, &local) {
            self.reindex_attribute(element, key, old_value.as_deref(), new_value.as_deref());
        }
        self.queue_value_record(
            MutationKind::Attributes,
            element,
            Some((&local, &ns)),
            |_| old_value.clone(),
        );
        if matches!(
            local,
            html5ever::local_name!("id")
                | html5ever::local_name!("name")
                | html5ever::local_name!("class")
                | html5ever::local_name!("href")
        ) {
            self.key_attr_version += 1;
        }
        self.react(|| Reaction::AttributeChanged {
            element,
            local_name: local,
            namespace: (!ns.is_empty()).then_some(ns),
            old_value,
            new_value,
        });
        self.bump(element);
    }

    /// DOM "change an attribute" at `index` to `value`.
    pub(crate) fn change_attribute(&mut self, element: NodeId, index: usize, value: &str) {
        let attr = &mut self.element_mut(element).attrs[index];
        let old = std::mem::replace(&mut attr.value, value.into());
        self.handle_attribute_changes(element, index, Some(old.into_string()), None);
    }

    /// DOM "append an attribute".
    pub(crate) fn append_attribute(&mut self, element: NodeId, attr: Attr) {
        let node = attr.node;
        let attrs = &mut self.element_mut(element).attrs;
        attrs.push(attr);
        let index = attrs.len() - 1;
        if let Some(n) = node {
            let doc = self.node_document(element);
            self.attr_node_mut(n).owner = Some(element);
            self.set_document(n, Some(doc));
        }
        self.handle_attribute_changes(element, index, None, None);
    }

    /// Appends an attribute without records or reactions: for freshly created elements (parser, clone).
    /// "Append an attribute" for an element just created (by the parser, "create an element for a
    /// token" step 9, or by cloning): no mutation record (nothing observes a new element) but the
    /// attribute change steps, which is how a parsed `onclick` attribute becomes an event handler.
    pub(crate) fn push_parsed_attribute(&mut self, element: NodeId, attr: Attr) {
        let wanted = self.reactions_enabled
            && self
                .reaction_filter
                .is_none_or(|f| (f.new_attribute)(self.el(element), &attr));
        let bits =
            self.name_bits.get(&attr.name.local) | crate::filter::bits_of_changed_value(&attr);
        self.add_filter_bits(element, bits);
        if !wanted {
            self.element_mut(element).attrs.push(attr);
            return;
        }
        self.react(|| Reaction::AttributeChanged {
            element,
            local_name: attr.name.local.clone(),
            namespace: (!attr.name.ns.is_empty()).then(|| attr.name.ns.clone()),
            old_value: None,
            new_value: Some(attr.value.to_string()),
        });
        self.element_mut(element).attrs.push(attr);
    }

    /// Moves a removed list entry into its Attr node, if it has one.
    fn detach_entry(&mut self, attr: &Attr) {
        if let Some(n) = attr.node {
            let data = self.attr_node_mut(n);
            data.owner = None;
            data.detached = Attr {
                name: attr.name.clone(),
                value: attr.value.clone(),
                node: Some(n),
            };
        }
    }

    /// DOM "remove an attribute" at `index`.
    pub(crate) fn remove_attribute_at(&mut self, element: NodeId, index: usize) -> Attr {
        let attr = self.element_mut(element).attrs.remove(index);
        self.detach_entry(&attr);
        let old = Some(attr.value.to_string());
        self.handle_attribute_changes(element, index, old, Some(&attr));
        attr
    }

    /// DOM "replace an attribute" at `index` with `new`.
    fn replace_attribute_at(&mut self, element: NodeId, index: usize, new: Attr) -> Attr {
        let node = new.node;
        let old = std::mem::replace(&mut self.element_mut(element).attrs[index], new);
        self.detach_entry(&old);
        if let Some(n) = node {
            let doc = self.node_document(element);
            self.attr_node_mut(n).owner = Some(element);
            self.set_document(n, Some(doc));
        }
        self.handle_attribute_changes(element, index, Some(old.value.to_string()), None);
        old
    }

    /// DOM "set an attribute value": sets the attribute with namespace `ns` and local name `local` to
    /// `value`, appending it (with `prefix`) if missing. The atom-based entry point for reflection.
    pub fn set_attribute_value(
        &mut self,
        element: NodeId,
        local: LocalName,
        ns: Namespace,
        prefix: Option<Prefix>,
        value: &str,
    ) {
        let index = self
            .el(element)
            .attrs
            .iter()
            .position(|a| a.name.local == local && a.name.ns == ns);
        match index {
            Some(i) => self.change_attribute(element, i, value),
            None => {
                self.append_attribute(element, Attr::new(QualName::new(prefix, ns, local), value))
            }
        }
    }

    // ----- Element methods ---------------------------------------------------------------------------------

    /// `Element.setAttribute(qualifiedName, value)` (DOM §4.9).
    pub fn set_attribute(
        &mut self,
        element: NodeId,
        qualified_name: &str,
        value: &str,
    ) -> DomResult<()> {
        if !names::is_valid_attribute_local_name(qualified_name) {
            return Err(DomError::InvalidCharacter);
        }
        let html = self.is_html_element_in_html_document(element);
        let name = lower_if(html, qualified_name);
        let index = self
            .el(element)
            .attrs
            .iter()
            .position(|a| qualified_name_is(a, &name));
        match index {
            Some(i) => self.change_attribute(element, i, value),
            None => self.append_attribute(
                element,
                Attr::new(QualName::new(None, ns!(), LocalName::from(&*name)), value),
            ),
        }
        Ok(())
    }

    /// `Element.setAttributeNS(namespace, qualifiedName, value)`.
    pub fn set_attribute_ns(
        &mut self,
        element: NodeId,
        namespace: Option<&str>,
        qualified_name: &str,
        value: &str,
    ) -> DomResult<()> {
        let name = names::validate_and_extract(namespace, qualified_name, NameContext::Attribute)?;
        self.set_attribute_value(element, name.local, name.ns, name.prefix, value);
        Ok(())
    }

    /// `Element.removeAttribute(qualifiedName)`.
    pub fn remove_attribute(&mut self, element: NodeId, qualified_name: &str) {
        if let Some(i) = self.attr_index_by_qualified_name(element, qualified_name) {
            self.remove_attribute_at(element, i);
        }
    }

    /// `Element.removeAttributeNS(namespace, localName)`.
    pub fn remove_attribute_ns(&mut self, element: NodeId, namespace: Option<&str>, local: &str) {
        if let Some(i) = self.attr_index(element, namespace.unwrap_or(""), local) {
            self.remove_attribute_at(element, i);
        }
    }

    /// `Element.toggleAttribute(qualifiedName, force)`.
    pub fn toggle_attribute(
        &mut self,
        element: NodeId,
        qualified_name: &str,
        force: Option<bool>,
    ) -> DomResult<bool> {
        if !names::is_valid_attribute_local_name(qualified_name) {
            return Err(DomError::InvalidCharacter);
        }
        let html = self.is_html_element_in_html_document(element);
        let name = lower_if(html, qualified_name).into_owned();
        match self.attr_index_by_qualified_name(element, &name) {
            None => {
                if force != Some(false) {
                    self.append_attribute(
                        element,
                        Attr::new(QualName::new(None, ns!(), LocalName::from(&*name)), ""),
                    );
                    return Ok(true);
                }
                Ok(false)
            }
            Some(i) => {
                if force != Some(true) {
                    self.remove_attribute_at(element, i);
                    return Ok(false);
                }
                Ok(true)
            }
        }
    }

    // ----- Attr nodes --------------------------------------------------------------------------------------

    fn attr_node_mut(&mut self, attr: NodeId) -> &mut crate::node::AttrNodeData {
        match &mut self.n_mut(attr).data {
            NodeData::Attr(a) => a,
            _ => panic!("not an Attr node"),
        }
    }

    /// The Attr node for the attribute at `index`, materialized on first request (so
    /// `getAttributeNode(x) === getAttributeNode(x)`).
    pub fn attribute_node(&mut self, element: NodeId, index: usize) -> NodeId {
        if let Some(n) = self.el(element).attrs[index].node {
            return n;
        }
        let doc = self.node_document(element);
        let name = self.el(element).attrs[index].name.clone();
        let n = self.create_attr_unchecked(doc, name, "");
        self.attr_node_mut(n).owner = Some(element);
        self.element_mut(element).attrs[index].node = Some(n);
        n
    }

    /// `Element.getAttributeNode(qualifiedName)`.
    pub fn get_attribute_node(&mut self, element: NodeId, qualified_name: &str) -> Option<NodeId> {
        let i = self.attr_index_by_qualified_name(element, qualified_name)?;
        Some(self.attribute_node(element, i))
    }

    /// `Element.getAttributeNodeNS(namespace, localName)`.
    pub fn get_attribute_node_ns(
        &mut self,
        element: NodeId,
        namespace: Option<&str>,
        local: &str,
    ) -> Option<NodeId> {
        let i = self.attr_index(element, namespace.unwrap_or(""), local)?;
        Some(self.attribute_node(element, i))
    }

    /// The name and value of an Attr node: the owner's list entry while attached.
    pub fn attr_node_attr(&self, attr: NodeId) -> &Attr {
        let NodeData::Attr(a) = &self.n(attr).data else {
            panic!("not an Attr node")
        };
        match a.owner {
            Some(owner) => self
                .el(owner)
                .attrs
                .iter()
                .find(|x| x.node == Some(attr))
                .expect("attached Attr missing from its element"),
            None => &a.detached,
        }
    }

    /// `Attr.ownerElement`.
    pub fn attr_owner_element(&self, attr: NodeId) -> Option<NodeId> {
        match &self.n(attr).data {
            NodeData::Attr(a) => a.owner,
            _ => None,
        }
    }

    /// `Attr.value`.
    pub fn attr_value(&self, attr: NodeId) -> &str {
        &self.attr_node_attr(attr).value
    }

    /// DOM "set an existing attribute value" (`Attr.value` setter).
    pub fn set_attr_value(&mut self, attr: NodeId, value: &str) {
        match self.attr_owner_element(attr) {
            None => self.attr_node_mut(attr).detached.value = value.into(),
            Some(owner) => {
                let i = self
                    .el(owner)
                    .attrs
                    .iter()
                    .position(|x| x.node == Some(attr))
                    .expect("attached Attr missing");
                self.change_attribute(owner, i, value);
            }
        }
    }

    /// DOM "set an attribute" (`Element.setAttributeNode(attr)` / `setAttributeNodeNS`).
    pub fn set_attribute_node(
        &mut self,
        element: NodeId,
        attr: NodeId,
    ) -> DomResult<Option<NodeId>> {
        let owner = self.attr_owner_element(attr);
        if owner.is_some_and(|o| o != element) {
            return Err(DomError::InUseAttribute);
        }
        let (ns, local) = {
            let a = self.attr_node_attr(attr);
            (a.name.ns.clone(), a.name.local.clone())
        };
        let old = self
            .el(element)
            .attrs
            .iter()
            .position(|a| a.name.local == local && a.name.ns == ns);
        if let Some(i) = old {
            if self.el(element).attrs[i].node == Some(attr) {
                return Ok(Some(attr));
            }
        }
        let mut entry = self.attr_node_mut(attr).detached.clone();
        entry.node = Some(attr);
        match old {
            Some(i) => {
                let old_node = self.attribute_node(element, i);
                self.replace_attribute_at(element, i, entry);
                Ok(Some(old_node))
            }
            None => {
                self.append_attribute(element, entry);
                Ok(None)
            }
        }
    }

    /// `Element.removeAttributeNode(attr)`.
    pub fn remove_attribute_node(&mut self, element: NodeId, attr: NodeId) -> DomResult<NodeId> {
        let i = self
            .el(element)
            .attrs
            .iter()
            .position(|x| x.node == Some(attr))
            .ok_or(DomError::NotFound)?;
        self.remove_attribute_at(element, i);
        Ok(attr)
    }
}
