// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Attributes: `Attr`, `NamedNodeMap`, the reflection primitives (`rt::Reflect`), `DOMTokenList` and
//! `DOMStringMap`.

use std::cell::Ref;

use snowlark_bindings::generated::{
    AttrImpl, DOMStringMapImpl, DOMTokenListImpl, NamedNodeMapImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};
use snowlark_dom::{ns, LocalName, Namespace, NodeData, NodeId};

use super::{exception, node_handle, DomResultExt, Engine, Object};
use snowlark_bindings::rt::ExceptionName;

impl Engine {
    pub(crate) fn is_attr_node(&self, id: NodeId) -> bool {
        matches!(self.dom().data(id), NodeData::Attr(_))
    }

    /// `Attr.value` setter (DOM "set an existing attribute value").
    pub(crate) fn set_attr_value(&self, cx: &mut Cx, attr: NodeId, value: &str) -> rt::Result<()> {
        self.dom_mut().set_attr_value(attr, value);
        self.after_mutation(cx);
        Ok(())
    }

    /// An attribute value in the null namespace, borrowed.
    pub(crate) fn attr_str<'a>(&'a self, element: NodeId, local: &str) -> Option<DomStr<'a>> {
        let dom = self.dom();
        let index = dom.attr_index(element, "", local)?;
        Some(DomStr::Guard(Ref::map(dom, |d| {
            d.attrs(element)[index].value()
        })))
    }

    /// Sets an attribute in the null namespace (reflection, `id`, `className`...).
    pub(crate) fn set_attr(&self, cx: &mut Cx, element: NodeId, local: &str, value: &str) {
        self.dom_mut()
            .set_attribute_value(element, LocalName::from(local), ns!(), None, value);
        self.after_mutation(cx);
    }

    /// Removes an attribute in the null namespace.
    pub(crate) fn remove_attr(&self, cx: &mut Cx, element: NodeId, local: &str) {
        self.dom_mut().remove_attribute_ns(element, None, local);
        self.after_mutation(cx);
    }

    fn map_element(&self, h: Handle) -> NodeId {
        match &*self.obj(h) {
            Object::AttrMap { element } | Object::StringMap { element } => *element,
            Object::TokenList { element, .. } => *element,
            _ => panic!("not an attribute collection"),
        }
    }
}

impl rt::Reflect for Engine {
    fn get_content_attribute(&self, element: Handle, local_name: &str) -> Option<DomStr<'_>> {
        if self.is_internals(element) {
            return self.internals_aria(element, local_name);
        }
        self.attr_str(self.n(element), local_name)
    }

    fn set_content_attribute(
        &self,
        cx: &mut Cx,
        element: Handle,
        local_name: &str,
        value: &str,
    ) -> rt::Result<()> {
        if self.is_internals(element) {
            self.set_internals_aria(element, local_name, Some(value));
            return Ok(());
        }
        self.set_attr(cx, self.n(element), local_name, value);
        Ok(())
    }

    fn remove_content_attribute(
        &self,
        cx: &mut Cx,
        element: Handle,
        local_name: &str,
    ) -> rt::Result<()> {
        if self.is_internals(element) {
            self.set_internals_aria(element, local_name, None);
            return Ok(());
        }
        self.remove_attr(cx, self.n(element), local_name);
        Ok(())
    }

    fn resolve_url(&self, element: Handle, url: &str) -> Option<String> {
        self.encoding_parse_url(self.n(element), url)
            .map(String::from)
    }

    fn token_list(&self, element: Handle, local_name: &str) -> Handle {
        self.alloc(Object::TokenList {
            element: self.n(element),
            attr: LocalName::from(local_name),
        })
    }
}

impl AttrImpl for Engine {
    fn namespace_uri(&self, this: Handle) -> Option<DomStr<'_>> {
        let id = self.n(this);
        let dom = self.dom();
        if dom.attr_node_attr(id).namespace().is_empty() {
            return None;
        }
        Some(DomStr::Guard(Ref::map(dom, |d| {
            &**d.attr_node_attr(id).namespace()
        })))
    }
    fn prefix(&self, this: Handle) -> Option<DomStr<'_>> {
        let id = self.n(this);
        let dom = self.dom();
        dom.attr_node_attr(id).prefix()?;
        Some(DomStr::Guard(Ref::map(dom, |d| {
            d.attr_node_attr(id).prefix().map_or("", |p| &**p)
        })))
    }
    fn local_name(&self, this: Handle) -> DomStr<'_> {
        let id = self.n(this);
        self.dom_str(|d| d.attr_node_attr(id).local_name())
    }
    fn name(&self, this: Handle) -> DomStr<'_> {
        let id = self.n(this);
        DomStr::Owned(self.dom().attr_node_attr(id).qualified_name().into_owned())
    }
    fn value(&self, this: Handle) -> DomStr<'_> {
        let id = self.n(this);
        self.dom_str(|d| d.attr_value(id))
    }
    fn set_value(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_attr_value(cx, self.n(this), value)
    }
    fn owner_element(&self, this: Handle) -> Option<Handle> {
        self.opt_node(self.dom().attr_owner_element(self.n(this)))
    }
    fn specified(&self, _this: Handle) -> bool {
        true
    }
}

impl NamedNodeMapImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        let el = self.map_element(this);
        self.dom().attrs(el).len() as u32
    }
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        let el = self.map_element(this);
        if index as usize >= self.dom().attrs(el).len() {
            return None;
        }
        Some(node_handle(
            self.dom_mut().attribute_node(el, index as usize),
        ))
    }
    fn get_named_item(&self, this: Handle, name: &str) -> Option<Handle> {
        let el = self.map_element(this);
        self.opt_node(self.dom_mut().get_attribute_node(el, name))
    }
    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        // Web IDL: the qualified names, without duplicates; in an HTML document, an HTML element's
        // names that contain ASCII uppercase are left out.
        let el = self.map_element(this);
        let dom = self.dom();
        let html = dom.is_html_element_in_html_document(el);
        let mut out: Vec<String> = Vec::new();
        for a in dom.attrs(el) {
            let q = a.qualified_name();
            if html && q.bytes().any(|b| b.is_ascii_uppercase()) {
                continue;
            }
            if !out.iter().any(|o| *o == *q) {
                out.push(q.into_owned());
            }
        }
        out
    }
    fn get_named_item_ns(
        &self,
        _cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        local_name: &str,
    ) -> rt::Result<Option<Handle>> {
        let el = self.map_element(this);
        Ok(self.opt_node(
            self.dom_mut()
                .get_attribute_node_ns(el, namespace, local_name),
        ))
    }
    fn set_named_item(
        &self,
        cx: &mut Cx,
        this: Handle,
        attr: Handle,
    ) -> rt::Result<Option<Handle>> {
        let el = self.map_element(this);
        let r = self.dom_mut().set_attribute_node(el, self.n(attr)).js();
        self.after_mutation(cx);
        r.map(|o| o.map(node_handle))
    }
    fn set_named_item_ns(
        &self,
        cx: &mut Cx,
        this: Handle,
        attr: Handle,
    ) -> rt::Result<Option<Handle>> {
        self.set_named_item(cx, this, attr)
    }
    fn remove_named_item(
        &self,
        cx: &mut Cx,
        this: Handle,
        qualified_name: &str,
    ) -> rt::Result<Handle> {
        let el = self.map_element(this);
        let attr = self
            .dom_mut()
            .get_attribute_node(el, qualified_name)
            .ok_or_else(|| exception(ExceptionName::NotFoundError, "No such attribute"))?;
        let r = self.dom_mut().remove_attribute_node(el, attr).js();
        self.after_mutation(cx);
        r.map(node_handle)
    }
    fn remove_named_item_ns(
        &self,
        cx: &mut Cx,
        this: Handle,
        namespace: Option<&str>,
        local_name: &str,
    ) -> rt::Result<Handle> {
        let el = self.map_element(this);
        let attr = self
            .dom_mut()
            .get_attribute_node_ns(el, namespace, local_name)
            .ok_or_else(|| exception(ExceptionName::NotFoundError, "No such attribute"))?;
        let r = self.dom_mut().remove_attribute_node(el, attr).js();
        self.after_mutation(cx);
        r.map(node_handle)
    }
}

// ----- DOMTokenList (DOM §7.1) -----------------------------------------------------------------------------

/// The ordered set parser (DOM §2.2 / Infra): split on ASCII whitespace, drop duplicates.
pub(crate) fn ordered_set(value: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for t in value.split_ascii_whitespace() {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

fn validate_token(token: &str) -> rt::Result<()> {
    if token.is_empty() {
        return Err(exception(ExceptionName::SyntaxError, "The token is empty"));
    }
    if token
        .bytes()
        .any(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r'))
    {
        return Err(exception(
            ExceptionName::InvalidCharacterError,
            "The token contains ASCII whitespace",
        ));
    }
    Ok(())
}

/// The supported tokens of the token lists that have them (HTML's link types, `sandbox`...).
fn supported_tokens(element_local: &str, attr: &str) -> Option<&'static [&'static str]> {
    Some(match (element_local, attr) {
        ("link", "rel") => &[
            "alternate",
            "dns-prefetch",
            "expect",
            "icon",
            "manifest",
            "modulepreload",
            "next",
            "pingback",
            "preconnect",
            "prefetch",
            "preload",
            "search",
            "stylesheet",
        ],
        ("a" | "area" | "form", "rel") => &["noreferrer", "noopener", "opener"],
        ("iframe", "sandbox") => &[
            "allow-downloads",
            "allow-forms",
            "allow-modals",
            "allow-orientation-lock",
            "allow-pointer-lock",
            "allow-popups",
            "allow-popups-to-escape-sandbox",
            "allow-presentation",
            "allow-same-origin",
            "allow-scripts",
            "allow-top-navigation",
            "allow-top-navigation-by-user-activation",
            "allow-top-navigation-to-custom-protocols",
        ],
        (_, "blocking") => &["render"],
        _ => return None,
    })
}

impl Engine {
    fn token_list_parts(&self, h: Handle) -> (NodeId, LocalName) {
        match &*self.obj(h) {
            Object::TokenList { element, attr } => (*element, attr.clone()),
            _ => panic!("not a DOMTokenList"),
        }
    }

    fn tokens(&self, h: Handle) -> Vec<String> {
        let (el, attr) = self.token_list_parts(h);
        let dom = self.dom();
        ordered_set(dom.attr(el, &ns!(), &attr).unwrap_or(""))
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// The DOMTokenList "update steps": set the attribute to the serialized set, unless the attribute is
    /// absent and the set is empty.
    fn set_tokens(&self, cx: &mut Cx, h: Handle, tokens: &[String]) {
        let (el, attr) = self.token_list_parts(h);
        let absent = self.dom().attr(el, &ns!(), &attr).is_none();
        if absent && tokens.is_empty() {
            return;
        }
        let value = tokens.join(" ");
        self.dom_mut()
            .set_attribute_value(el, attr, Namespace::from(""), None, &value);
        self.after_mutation(cx);
    }
}

impl DOMTokenListImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        self.tokens(this).len() as u32
    }
    fn item(&self, this: Handle, index: u32) -> Option<DomStr<'_>> {
        self.tokens(this)
            .into_iter()
            .nth(index as usize)
            .map(DomStr::Owned)
    }
    fn value(&self, this: Handle) -> DomStr<'_> {
        let (el, attr) = self.token_list_parts(this);
        let dom = self.dom();
        if dom.attr(el, &ns!(), &attr).is_none() {
            return DomStr::Borrowed("");
        }
        DomStr::Guard(Ref::map(dom, |d| d.attr(el, &ns!(), &attr).unwrap_or("")))
    }
    fn set_value(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let (el, attr) = self.token_list_parts(this);
        self.dom_mut()
            .set_attribute_value(el, attr, Namespace::from(""), None, value);
        self.after_mutation(cx);
        Ok(())
    }
    fn contains(&self, _cx: &mut Cx, this: Handle, token: &str) -> rt::Result<bool> {
        Ok(self.tokens(this).iter().any(|t| t == token))
    }
    fn add(&self, cx: &mut Cx, this: Handle, tokens: Vec<String>) -> rt::Result<()> {
        for t in &tokens {
            validate_token(t)?;
        }
        let mut set = self.tokens(this);
        for t in tokens {
            if !set.contains(&t) {
                set.push(t);
            }
        }
        self.set_tokens(cx, this, &set);
        Ok(())
    }
    fn remove(&self, cx: &mut Cx, this: Handle, tokens: Vec<String>) -> rt::Result<()> {
        for t in &tokens {
            validate_token(t)?;
        }
        let mut set = self.tokens(this);
        set.retain(|t| !tokens.contains(t));
        self.set_tokens(cx, this, &set);
        Ok(())
    }
    fn toggle(
        &self,
        cx: &mut Cx,
        this: Handle,
        token: &str,
        force: Option<bool>,
    ) -> rt::Result<bool> {
        validate_token(token)?;
        let mut set = self.tokens(this);
        if set.iter().any(|t| t == token) {
            if force != Some(true) {
                set.retain(|t| t != token);
                self.set_tokens(cx, this, &set);
                return Ok(false);
            }
            return Ok(true);
        }
        if force != Some(false) {
            set.push(token.to_owned());
            self.set_tokens(cx, this, &set);
            return Ok(true);
        }
        Ok(false)
    }
    fn replace(&self, cx: &mut Cx, this: Handle, token: &str, new_token: &str) -> rt::Result<bool> {
        validate_token(token)?;
        validate_token(new_token)?;
        let set = self.tokens(this);
        if !set.iter().any(|t| t == token) {
            return Ok(false);
        }
        // Infra "replace" in an ordered set: the first instance of either becomes the replacement, all
        // other instances of both go.
        let first = set
            .iter()
            .position(|t| t == token || t == new_token)
            .expect("token is in the set");
        let mut out: Vec<String> = Vec::with_capacity(set.len());
        for (j, t) in set.into_iter().enumerate() {
            if j == first {
                out.push(new_token.to_owned());
            } else if t != token && t != new_token {
                out.push(t);
            }
        }
        self.set_tokens(cx, this, &out);
        Ok(true)
    }
    fn supports(&self, _cx: &mut Cx, this: Handle, token: &str) -> rt::Result<bool> {
        let (el, attr) = self.token_list_parts(this);
        let local = self.dom().local_name(el).to_string();
        match supported_tokens(&local, &attr) {
            Some(list) => Ok(list.iter().any(|t| t.eq_ignore_ascii_case(token))),
            None => Err(rt::Error::Type(
                "This token list has no supported tokens".into(),
            )),
        }
    }
}

// ----- DOMStringMap (HTML §3.2.6.6) ------------------------------------------------------------------------

/// `data-foo-bar` → `fooBar`, if the attribute name maps to a property name.
fn data_name_to_prop(attr: &str) -> Option<String> {
    let rest = attr.strip_prefix("data-")?;
    if rest.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let mut out = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' {
            if let Some(&n) = chars.peek() {
                if n.is_ascii_lowercase() {
                    out.push(n.to_ascii_uppercase());
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    Some(out)
}

/// `fooBar` → `data-foo-bar`; a SyntaxError for `-` followed by a lowercase letter.
fn prop_to_data_name(name: &str) -> rt::Result<String> {
    let b = name.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_lowercase) {
            return Err(exception(
                ExceptionName::SyntaxError,
                "'-' followed by a lowercase letter",
            ));
        }
    }
    let mut out = String::from("data-");
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

impl DOMStringMapImpl for Engine {
    fn named_getter(&self, this: Handle, name: &str) -> Option<DomStr<'_>> {
        let el = self.map_element(this);
        let dom = self.dom();
        let index = dom.attrs(el).iter().position(|a| {
            a.namespace().is_empty() && data_name_to_prop(a.local_name()).as_deref() == Some(name)
        })?;
        Some(DomStr::Guard(Ref::map(dom, |d| d.attrs(el)[index].value())))
    }
    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        let el = self.map_element(this);
        let dom = self.dom();
        dom.attrs(el)
            .iter()
            .filter(|a| a.namespace().is_empty())
            .filter_map(|a| data_name_to_prop(a.local_name()))
            .collect()
    }
    fn named_setter(&self, cx: &mut Cx, this: Handle, name: &str, value: &str) -> rt::Result<()> {
        let el = self.map_element(this);
        let attr = prop_to_data_name(name)?;
        if !snowlark_dom::names::is_valid_attribute_local_name(&attr) {
            return Err(exception(
                ExceptionName::InvalidCharacterError,
                "Not a valid attribute name",
            ));
        }
        let r = self.dom_mut().set_attribute(el, &attr, value).js();
        self.after_mutation(cx);
        r
    }
    fn named_deleter(&self, cx: &mut Cx, this: Handle, name: &str) -> rt::Result<()> {
        let el = self.map_element(this);
        if let Ok(attr) = prop_to_data_name(name) {
            self.dom_mut().remove_attribute(el, &attr);
            self.after_mutation(cx);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_names() {
        assert_eq!(data_name_to_prop("data-foo-bar").as_deref(), Some("fooBar"));
        assert_eq!(data_name_to_prop("data-foo-").as_deref(), Some("foo-"));
        assert_eq!(data_name_to_prop("data-Foo"), None);
        assert_eq!(prop_to_data_name("fooBar").unwrap(), "data-foo-bar");
        assert!(prop_to_data_name("foo-bar").is_err());
    }

    #[test]
    fn ordered_sets() {
        assert_eq!(ordered_set("  a b  a\tc "), vec!["a", "b", "c"]);
    }
}
