// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `Node` and the tree mixins (`ParentNode`, `ChildNode`, `NonDocumentTypeChildNode`,
//! `NonElementParentNode`, `Slottable`) over the DOM Standard algorithms in `snowlark-dom`.

use snowlark_bindings::generated::{
    self as g, ChildNodeImpl, GetRootNodeOptions, NodeImpl, NonDocumentTypeChildNodeImpl,
    NonElementParentNodeImpl, ParentNodeImpl, SlottableImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};
use snowlark_dom::{NodeData, NodeId, NodeOrString};

use super::element::InsertMode;

use super::collections::{Collection, Filter};
use super::{handle_node, node_handle, DomResultExt, Engine, Object};

impl Engine {
    #[inline(always)]
    pub(crate) fn opt_node(&self, id: Option<NodeId>) -> Option<Handle> {
        id.map(node_handle)
    }

    /// The arguments of `append()` and friends as the DOM's union.
    pub(crate) fn nodes_or_strings(&self, nodes: Vec<g::NodeOrString>) -> Vec<NodeOrString> {
        nodes
            .into_iter()
            .map(|n| match n {
                g::NodeOrString::Node(h) => NodeOrString::Node(self.n(h)),
                g::NodeOrString::String(s) => NodeOrString::String(s),
            })
            .collect()
    }

    /// A new live collection object.
    pub(crate) fn collection(&self, root: NodeId, filter: Filter) -> Handle {
        self.alloc(Object::Collection(Collection::live(root, filter)))
    }

    /// A new static NodeList.
    pub(crate) fn static_node_list(&self, nodes: Vec<NodeId>) -> Handle {
        self.alloc(Object::Collection(Collection::fixed(nodes)))
    }

    /// The document base URL (HTML §2.4.1) of `node`'s node document.
    pub(crate) fn base_url_of(&self, node: NodeId) -> url::Url {
        let dom = self.dom();
        let doc = dom.node_document(node);
        self.document_base_url(&dom, doc)
    }

    /// HTML §2.4.2 "encoding-parsing a URL" relative to `node`'s node document: its base URL, and
    /// its character encoding for the query (the URL parser's "percent-encode after encoding").
    pub(crate) fn encoding_parse_url(&self, node: NodeId, input: &str) -> Option<url::Url> {
        let (base, encoding) = {
            let dom = self.dom();
            let doc = dom.node_document(node);
            (self.document_base_url(&dom, doc), dom.encoding(doc))
        };
        snowlark_render::encoding_parse_url(&base, encoding, input)
    }
}

impl NodeImpl for Engine {
    fn node_type(&self, this: Handle) -> u16 {
        self.dom().node_type(self.n(this)) as u16
    }

    fn node_name(&self, this: Handle) -> DomStr<'_> {
        let id = self.n(this);
        let dom = self.dom();
        match dom.node_name(id) {
            std::borrow::Cow::Borrowed(_) => {
                DomStr::Guard(std::cell::Ref::map(dom, |d| match d.node_name(id) {
                    std::borrow::Cow::Borrowed(s) => s,
                    std::borrow::Cow::Owned(_) => unreachable!(),
                }))
            }
            std::borrow::Cow::Owned(s) => DomStr::Owned(s),
        }
    }

    fn base_uri(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.base_url_of(self.n(this)).into())
    }

    fn is_connected(&self, this: Handle) -> bool {
        self.dom().is_connected(self.n(this))
    }

    fn owner_document(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().owner_document(self.n(this)))
    }

    fn parent_node(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().parent(self.n(this)))
    }

    fn parent_element(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().parent_element(self.n(this)))
    }

    fn child_nodes(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::ChildNodes)
    }

    fn first_child(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().first_child(self.n(this)))
    }

    fn last_child(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().last_child(self.n(this)))
    }

    fn previous_sibling(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().previous_sibling(self.n(this)))
    }

    fn next_sibling(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().next_sibling(self.n(this)))
    }

    fn node_value(&self, this: Handle) -> Option<DomStr<'_>> {
        let id = self.n(this);
        let dom = self.dom();
        dom.node_value(id)?;
        Some(DomStr::Guard(std::cell::Ref::map(dom, |d| {
            d.node_value(id).unwrap_or("")
        })))
    }

    fn set_node_value(&self, cx: &mut Cx, this: Handle, value: Option<&str>) -> rt::Result<()> {
        let id = self.n(this);
        if self.is_attr_node(id) {
            return self.set_attr_value(cx, id, value.unwrap_or(""));
        }
        self.dom_mut().set_node_value(id, value.unwrap_or(""));
        self.after_mutation(cx);
        Ok(())
    }

    fn text_content(&self, this: Handle) -> Option<DomStr<'_>> {
        let id = self.n(this);
        let dom = self.dom();
        match dom.data(id) {
            NodeData::Text(_)
            | NodeData::CDataSection(_)
            | NodeData::Comment(_)
            | NodeData::ProcessingInstruction { .. }
            | NodeData::Attr(_) => Some(DomStr::Guard(std::cell::Ref::map(dom, |d| {
                d.node_value(id).unwrap_or("")
            }))),
            _ => dom.text_content(id).map(DomStr::Owned),
        }
    }

    fn set_text_content(&self, cx: &mut Cx, this: Handle, value: Option<&str>) -> rt::Result<()> {
        let id = self.n(this);
        if self.is_attr_node(id) {
            return self.set_attr_value(cx, id, value.unwrap_or(""));
        }
        self.dom_mut().set_text_content(id, value.unwrap_or(""));
        self.after_mutation(cx);
        Ok(())
    }

    fn get_root_node(
        &self,
        _cx: &mut Cx,
        this: Handle,
        options: GetRootNodeOptions,
    ) -> rt::Result<Handle> {
        Ok(node_handle(
            self.dom().get_root_node(self.n(this), options.composed),
        ))
    }

    fn has_child_nodes(&self, _cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        Ok(self.dom().has_child_nodes(self.n(this)))
    }

    fn normalize(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.dom_mut().normalize(self.n(this));
        self.after_mutation(cx);
        Ok(())
    }

    fn clone_node(&self, cx: &mut Cx, this: Handle, subtree: bool) -> rt::Result<Handle> {
        let id = self.n(this);
        if self.dom().is_shadow_root(id) {
            return Err(super::dom_err(snowlark_dom::DomError::NotSupported));
        }
        let copy = self.clone_with_steps(cx, id, None, subtree)?;
        Ok(node_handle(copy))
    }

    fn is_equal_node(
        &self,
        _cx: &mut Cx,
        this: Handle,
        other_node: Option<Handle>,
    ) -> rt::Result<bool> {
        Ok(match other_node {
            Some(o) => self.dom().is_equal_node(self.n(this), self.n(o)),
            None => false,
        })
    }

    fn is_same_node(
        &self,
        _cx: &mut Cx,
        this: Handle,
        other_node: Option<Handle>,
    ) -> rt::Result<bool> {
        Ok(other_node == Some(this))
    }

    fn compare_document_position(
        &self,
        _cx: &mut Cx,
        this: Handle,
        other: Handle,
    ) -> rt::Result<u16> {
        Ok(self
            .dom()
            .compare_document_position(self.n(this), self.n(other)))
    }

    fn contains(&self, _cx: &mut Cx, this: Handle, other: Option<Handle>) -> rt::Result<bool> {
        Ok(self
            .dom()
            .contains(self.n(this), other.and_then(handle_node)))
    }

    fn lookup_prefix(
        &self,
        _cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
    ) -> rt::Result<Option<DomStr<'_>>> {
        Ok(self
            .dom()
            .lookup_prefix(self.n(this), namespace)
            .map(DomStr::Owned))
    }

    fn lookup_namespace_uri(
        &self,
        _cx: &mut Cx,
        this: Handle,
        prefix: Option<&str>,
    ) -> rt::Result<Option<DomStr<'_>>> {
        Ok(self
            .dom()
            .lookup_namespace_uri(self.n(this), prefix)
            .map(DomStr::Owned))
    }

    fn is_default_namespace(
        &self,
        _cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
    ) -> rt::Result<bool> {
        Ok(self.dom().is_default_namespace(self.n(this), namespace))
    }

    fn insert_before(
        &self,
        cx: &mut Cx,
        this: Handle,
        node: Handle,
        child: Option<Handle>,
    ) -> rt::Result<Handle> {
        let r = self
            .dom_mut()
            .insert_before(self.n(this), self.n(node), child.map(|c| self.n(c)))
            .js();
        self.after_mutation(cx);
        r.map(node_handle)
    }

    fn append_child(&self, cx: &mut Cx, this: Handle, node: Handle) -> rt::Result<Handle> {
        let r = self.dom_mut().append_child(self.n(this), self.n(node)).js();
        self.after_mutation(cx);
        r.map(node_handle)
    }

    fn replace_child(
        &self,
        cx: &mut Cx,
        this: Handle,
        node: Handle,
        child: Handle,
    ) -> rt::Result<Handle> {
        let r = self
            .dom_mut()
            .replace_child(self.n(this), self.n(node), self.n(child))
            .js();
        self.after_mutation(cx);
        r.map(node_handle)
    }

    fn remove_child(&self, cx: &mut Cx, this: Handle, child: Handle) -> rt::Result<Handle> {
        let r = self
            .dom_mut()
            .remove_child(self.n(this), self.n(child))
            .js();
        self.after_mutation(cx);
        r.map(node_handle)
    }
}

impl ParentNodeImpl for Engine {
    fn children(&self, this: Handle) -> Handle {
        self.collection(self.n(this), Filter::Children)
    }

    fn first_element_child(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().first_element_child(self.n(this)))
    }

    fn last_element_child(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().last_element_child(self.n(this)))
    }

    fn child_element_count(&self, this: Handle) -> u32 {
        self.dom().child_element_count(self.n(this)) as u32
    }

    fn prepend(&self, cx: &mut Cx, this: Handle, nodes: Vec<g::NodeOrString>) -> rt::Result<()> {
        let nodes = self.nodes_or_strings(nodes);
        let r = self.dom_mut().prepend(self.n(this), nodes).js();
        self.after_mutation(cx);
        r
    }

    fn append(&self, cx: &mut Cx, this: Handle, nodes: Vec<g::NodeOrString>) -> rt::Result<()> {
        let nodes = self.nodes_or_strings(nodes);
        let r = self.dom_mut().append(self.n(this), nodes).js();
        self.after_mutation(cx);
        r
    }

    fn replace_children(
        &self,
        cx: &mut Cx,
        this: Handle,
        nodes: Vec<g::NodeOrString>,
    ) -> rt::Result<()> {
        let nodes = self.nodes_or_strings(nodes);
        let r = self.dom_mut().replace_children(self.n(this), nodes).js();
        self.after_mutation(cx);
        r
    }

    /// `moveBefore(node, child)` (DOM §4.2.6): the move keeps the node's state; until the state-preserving
    /// move exists in snowlark-dom it validates as specified and then moves by removal and insertion.
    fn move_before(
        &self,
        cx: &mut Cx,
        this: Handle,
        node: Handle,
        child: Option<Handle>,
    ) -> rt::Result<()> {
        let parent = self.n(this);
        let node = self.n(node);
        let child = child.map(|c| self.n(c));
        {
            let dom = self.dom();
            // 1-2: the shadow-including roots must be the same.
            if dom.shadow_including_root(parent) != dom.shadow_including_root(node) {
                return Err(super::dom_err(snowlark_dom::DomError::HierarchyRequest));
            }
            if !dom.is_element(node) && !dom.is_character_data(node) {
                return Err(super::dom_err(snowlark_dom::DomError::HierarchyRequest));
            }
            if dom.is_document(parent) && !dom.is_element(node) && dom.is_text(node) {
                return Err(super::dom_err(snowlark_dom::DomError::HierarchyRequest));
            }
        }
        let r = self.dom_mut().insert_before(parent, node, child).js();
        // (the node stays connected: its CSS animations go on)
        if r.is_ok() {
            self.with_node_renderer(node, |r, _| r.node_moved(node));
        }
        self.svg.borrow_mut().moving = true;
        self.after_mutation(cx);
        self.svg.borrow_mut().moving = false;
        r.map(drop)
    }

    fn query_selector(
        &self,
        cx: &mut Cx,
        this: Handle,
        selectors: &str,
    ) -> rt::Result<Option<Handle>> {
        let parsed = self.parsed_selectors(selectors)?;
        self.sync_form_states_if(cx, parsed.reads_form_state);
        Ok(self.opt_node(self.query_first_cached(self.n(this), &parsed)))
    }

    fn query_selector_all(&self, cx: &mut Cx, this: Handle, selectors: &str) -> rt::Result<Handle> {
        let parsed = self.parsed_selectors(selectors)?;
        self.sync_form_states_if(cx, parsed.reads_form_state);
        let found = self.query_all_cached(self.n(this), &parsed);
        Ok(self.static_node_list(found))
    }
}

impl ChildNodeImpl for Engine {
    fn before(&self, cx: &mut Cx, this: Handle, nodes: Vec<g::NodeOrString>) -> rt::Result<()> {
        let nodes = self.nodes_or_strings(nodes);
        let r = self.dom_mut().before(self.n(this), nodes).js();
        self.after_mutation(cx);
        r
    }

    fn after(&self, cx: &mut Cx, this: Handle, nodes: Vec<g::NodeOrString>) -> rt::Result<()> {
        let nodes = self.nodes_or_strings(nodes);
        let r = self.dom_mut().after(self.n(this), nodes).js();
        self.after_mutation(cx);
        r
    }

    fn replace_with(
        &self,
        cx: &mut Cx,
        this: Handle,
        nodes: Vec<g::NodeOrString>,
    ) -> rt::Result<()> {
        let nodes = self.nodes_or_strings(nodes);
        let r = self.dom_mut().replace_with(self.n(this), nodes).js();
        self.after_mutation(cx);
        r
    }

    fn remove(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.dom_mut().remove_node(self.n(this));
        self.after_mutation(cx);
        Ok(())
    }
}

impl NonDocumentTypeChildNodeImpl for Engine {
    fn previous_element_sibling(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().previous_element_sibling(self.n(this)))
    }

    fn next_element_sibling(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().next_element_sibling(self.n(this)))
    }
    fn before_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: &str,
        _o: g::SetHTMLOptions,
    ) -> rt::Result<()> {
        self.insert_html_steps(cx, self.n(this), html, InsertMode::safe(_o), 0)
    }
    fn after_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: &str,
        _o: g::SetHTMLOptions,
    ) -> rt::Result<()> {
        self.insert_html_steps(cx, self.n(this), html, InsertMode::safe(_o), 1)
    }
    fn replace_with_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: &str,
        _o: g::SetHTMLOptions,
    ) -> rt::Result<()> {
        self.insert_html_steps(cx, self.n(this), html, InsertMode::safe(_o), 2)
    }
    fn before_html_unsafe(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: g::TrustedHTMLOrString,
        _o: g::SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    ) -> rt::Result<()> {
        let doc = self.tt_document(cx, self.n(this));
        let (m, mode) = self.tt_html_input(cx, html, _o, doc, "Node beforeHTMLUnsafe")?;
        self.insert_html_steps(cx, self.n(this), &m, mode, 0)
    }
    fn after_html_unsafe(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: g::TrustedHTMLOrString,
        _o: g::SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    ) -> rt::Result<()> {
        let doc = self.tt_document(cx, self.n(this));
        let (m, mode) = self.tt_html_input(cx, html, _o, doc, "Node afterHTMLUnsafe")?;
        self.insert_html_steps(cx, self.n(this), &m, mode, 1)
    }
    fn replace_with_html_unsafe(
        &self,
        cx: &mut Cx,
        this: Handle,
        html: g::TrustedHTMLOrString,
        _o: g::SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    ) -> rt::Result<()> {
        let doc = self.tt_document(cx, self.n(this));
        let (m, mode) = self.tt_html_input(cx, html, _o, doc, "Node replaceWithHTMLUnsafe")?;
        self.insert_html_steps(cx, self.n(this), &m, mode, 2)
    }
}

impl NonElementParentNodeImpl for Engine {
    fn get_element_by_id(
        &self,
        _cx: &mut Cx,
        this: Handle,
        element_id: &str,
    ) -> rt::Result<Option<Handle>> {
        if element_id.is_empty() {
            return Ok(None);
        }
        let root = self.n(this);
        let dom = self.dom();
        if let Some(found) = dom.indexed_element_by_id(root, element_id) {
            return Ok(self.opt_node(found));
        }
        let found = dom.descendants(root).find(|&d| {
            dom.element(d).is_some_and(|e| {
                e.attrs().iter().any(|a| {
                    a.namespace().is_empty() && &**a.local_name() == "id" && a.value() == element_id
                })
            })
        });
        Ok(self.opt_node(found))
    }
}

impl SlottableImpl for Engine {
    fn assigned_slot(&self, this: Handle) -> Option<Handle> {
        let id = self.n(this);
        let slot = self.find_slot(id, true);
        self.opt_node(slot)
    }
}
