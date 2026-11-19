// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §8.6 "HTML sanitization": the `Sanitizer` interface, its configuration (canonical form,
//! validity, the modifier methods, "remove unsafe"), "get a sanitizer config from options", and
//! sanitizing a parsed tree with a configuration.
//!
//! The standard sanitizes while the HTML parser builds the tree; this sanitizes the parsed
//! fragment (or document) before anything is inserted, which keeps, removes and replaces the same
//! nodes and attributes. The fragment is not connected while it is sanitized, so nothing it holds
//! has run.

use snowlark_bindings::generated::{
    SanitizerAttributeNamespace, SanitizerConfig, SanitizerConfigOrSanitizerPresets,
    SanitizerElementNamespace, SanitizerElementNamespaceWithAttributes, SanitizerImpl,
    SanitizerOrSanitizerConfigOrSanitizerPresets, SanitizerProcessingInstruction,
    StringOrSanitizerAttributeNamespace, StringOrSanitizerElementNamespace,
    StringOrSanitizerElementNamespaceWithAttributes, StringOrSanitizerProcessingInstruction,
};
use snowlark_bindings::rt::{self, Cx, Error, Handle};
use snowlark_dom::{NodeId, NodeType};

use super::{Engine, Object};

const HTML: &str = "http://www.w3.org/1999/xhtml";
const SVG: &str = "http://www.w3.org/2000/svg";
const MATHML: &str = "http://www.w3.org/1998/Math/MathML";
const XLINK: &str = "http://www.w3.org/1999/xlink";

/// A canonical sanitizer name: an element's or an attribute's local name and namespace (null is
/// `None`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Name {
    pub name: String,
    pub ns: Option<String>,
}

impl Name {
    fn new(name: &str, ns: Option<&str>) -> Name {
        Name {
            name: name.to_owned(),
            ns: ns.map(str::to_owned),
        }
    }
    fn is(&self, name: &str, ns: Option<&str>) -> bool {
        self.name == name && self.ns.as_deref() == ns
    }
    /// A custom data attribute: "data-" and more, in no namespace (HTML §3.2.6.6).
    fn is_custom_data(&self) -> bool {
        self.ns.is_none() && self.name.starts_with("data-")
    }
}

/// A canonical `SanitizerElementNamespaceWithAttributes`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ElementEntry {
    pub name: Name,
    pub attributes: Option<Vec<Name>>,
    pub remove_attributes: Option<Vec<Name>>,
}

/// A canonical `SanitizerConfig`: a member that does not exist is `None`.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Config {
    pub elements: Option<Vec<ElementEntry>>,
    pub remove_elements: Option<Vec<Name>>,
    pub replace_with_children: Option<Vec<Name>>,
    pub processing_instructions: Option<Vec<String>>,
    pub remove_processing_instructions: Option<Vec<String>>,
    pub attributes: Option<Vec<Name>>,
    pub remove_attributes: Option<Vec<Name>>,
    pub comments: Option<bool>,
    pub data_attributes: Option<bool>,
    pub javascript_urls: Option<bool>,
}

/// HTML "sanitizer action".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Action {
    Keep,
    Remove,
    ReplaceWithChildren,
}

// ---- Canonicalizing (HTML §8.6.2) ----

fn canon_ns(ns: Option<String>) -> Option<String> {
    ns.filter(|n| !n.is_empty())
}

fn canon_element(e: StringOrSanitizerElementNamespace) -> Name {
    match e {
        StringOrSanitizerElementNamespace::String(s) => Name::new(&s, Some(HTML)),
        StringOrSanitizerElementNamespace::SanitizerElementNamespace(d) => Name {
            name: d.name,
            ns: canon_ns(d.namespace),
        },
    }
}

fn canon_attribute(a: StringOrSanitizerAttributeNamespace) -> Name {
    match a {
        StringOrSanitizerAttributeNamespace::String(s) => Name::new(&s, None),
        StringOrSanitizerAttributeNamespace::SanitizerAttributeNamespace(d) => Name {
            name: d.name,
            ns: canon_ns(d.namespace),
        },
    }
}

fn canon_pi(p: StringOrSanitizerProcessingInstruction) -> String {
    match p {
        StringOrSanitizerProcessingInstruction::String(s) => s,
        StringOrSanitizerProcessingInstruction::SanitizerProcessingInstruction(d) => d.target,
    }
}

fn canon_element_with_attributes(
    e: StringOrSanitizerElementNamespaceWithAttributes,
) -> ElementEntry {
    let mut entry = match e {
        StringOrSanitizerElementNamespaceWithAttributes::String(s) => ElementEntry {
            name: Name::new(&s, Some(HTML)),
            attributes: None,
            remove_attributes: None,
        },
        StringOrSanitizerElementNamespaceWithAttributes::SanitizerElementNamespaceWithAttributes(
            d,
        ) => ElementEntry {
            name: Name {
                name: d.name,
                ns: canon_ns(d.namespace),
            },
            attributes: d
                .attributes
                .map(|l| l.into_iter().map(canon_attribute).collect()),
            remove_attributes: d
                .remove_attributes
                .map(|l| l.into_iter().map(canon_attribute).collect()),
        },
    };
    if entry.attributes.is_none() && entry.remove_attributes.is_none() {
        entry.remove_attributes = Some(Vec::new());
    }
    entry
}

/// The canonicalizing steps of each list member of a dictionary (the first half of HTML
/// "canonicalize the configuration").
fn canon_dictionary(c: SanitizerConfig) -> Config {
    Config {
        elements: c
            .elements
            .map(|l| l.into_iter().map(canon_element_with_attributes).collect()),
        remove_elements: c
            .remove_elements
            .map(|l| l.into_iter().map(canon_element).collect()),
        replace_with_children: c
            .replace_with_children_elements
            .map(|l| l.into_iter().map(canon_element).collect()),
        processing_instructions: c
            .processing_instructions
            .map(|l| l.into_iter().map(canon_pi).collect()),
        remove_processing_instructions: c
            .remove_processing_instructions
            .map(|l| l.into_iter().map(canon_pi).collect()),
        attributes: c
            .attributes
            .map(|l| l.into_iter().map(canon_attribute).collect()),
        remove_attributes: c
            .remove_attributes
            .map(|l| l.into_iter().map(canon_attribute).collect()),
        comments: c.comments,
        data_attributes: c.data_attributes,
        javascript_urls: c.javascript_ur_ls,
    }
}

impl Config {
    /// HTML "canonicalize the configuration": the defaults of the members that do not exist (the
    /// lists' entries are canonical already).
    fn canonicalize(&mut self, permissive: bool) {
        if self.elements.is_none() && self.remove_elements.is_none() {
            self.remove_elements = Some(Vec::new());
        }
        if self.attributes.is_none() && self.remove_attributes.is_none() {
            self.remove_attributes = Some(Vec::new());
        }
        if self.processing_instructions.is_none() && self.remove_processing_instructions.is_none() {
            if permissive {
                self.remove_processing_instructions = Some(Vec::new());
            } else {
                self.processing_instructions = Some(Vec::new());
            }
        }
        if self.comments.is_none() {
            self.comments = Some(permissive);
        }
        if self.attributes.is_some() && self.data_attributes.is_none() {
            self.data_attributes = Some(permissive);
        }
        if self.javascript_urls.is_none() {
            self.javascript_urls = Some(permissive);
        }
    }

    /// HTML "valid" for a canonical configuration.
    fn is_valid(&self) -> bool {
        if self.elements.is_some() && self.remove_elements.is_some() {
            return false;
        }
        if self.processing_instructions.is_some() && self.remove_processing_instructions.is_some() {
            return false;
        }
        if self.attributes.is_some() && self.remove_attributes.is_some() {
            return false;
        }
        if let Some(elements) = &self.elements {
            if has_duplicates(elements.iter().map(|e| &e.name)) {
                return false;
            }
        } else if let Some(l) = &self.remove_elements {
            if has_duplicates(l.iter()) {
                return false;
            }
        }
        if let Some(l) = &self.replace_with_children {
            if has_duplicates(l.iter()) {
                return false;
            }
        }
        if let Some(l) = &self.processing_instructions {
            if has_duplicates(l.iter()) {
                return false;
            }
        } else if let Some(l) = &self.remove_processing_instructions {
            if has_duplicates(l.iter()) {
                return false;
            }
        }
        if let Some(l) = &self.attributes {
            if has_duplicates(l.iter()) {
                return false;
            }
        } else if let Some(l) = &self.remove_attributes {
            if has_duplicates(l.iter()) {
                return false;
            }
        }
        if let Some(replace) = &self.replace_with_children {
            if replace.iter().any(is_non_replaceable) {
                return false;
            }
            let clash = match &self.elements {
                Some(elements) => elements.iter().any(|e| replace.contains(&e.name)),
                None => self
                    .remove_elements
                    .iter()
                    .flatten()
                    .any(|e| replace.contains(e)),
            };
            if clash {
                return false;
            }
        }
        let none: Vec<Name> = Vec::new();
        if let Some(attributes) = &self.attributes {
            let data = self.data_attributes == Some(true);
            for e in self.elements.iter().flatten() {
                if e.attributes
                    .as_ref()
                    .is_some_and(|l| has_duplicates(l.iter()))
                {
                    return false;
                }
                if e.remove_attributes
                    .as_ref()
                    .is_some_and(|l| has_duplicates(l.iter()))
                {
                    return false;
                }
                let local = e.attributes.as_ref().unwrap_or(&none);
                if local.iter().any(|a| attributes.contains(a)) {
                    return false;
                }
                let local_remove = e.remove_attributes.as_ref().unwrap_or(&none);
                if !local_remove.iter().all(|a| attributes.contains(a)) {
                    return false;
                }
                if data && local.iter().any(Name::is_custom_data) {
                    return false;
                }
            }
            if data && attributes.iter().any(Name::is_custom_data) {
                return false;
            }
        } else {
            let remove = self.remove_attributes.as_ref().unwrap_or(&none);
            for e in self.elements.iter().flatten() {
                if e.attributes.is_some() && e.remove_attributes.is_some() {
                    return false;
                }
                if e.attributes
                    .as_ref()
                    .is_some_and(|l| has_duplicates(l.iter()))
                {
                    return false;
                }
                if e.remove_attributes
                    .as_ref()
                    .is_some_and(|l| has_duplicates(l.iter()))
                {
                    return false;
                }
                let local = e.attributes.as_ref().unwrap_or(&none);
                if local.iter().any(|a| remove.contains(a)) {
                    return false;
                }
                let local_remove = e.remove_attributes.as_ref().unwrap_or(&none);
                if local_remove.iter().any(|a| remove.contains(a)) {
                    return false;
                }
            }
            if self.data_attributes.is_some() {
                return false;
            }
        }
        true
    }

    // ---- Modifiers (HTML §8.6.2 and §8.6.4) ----

    /// HTML "remove an element" from a configuration.
    fn remove_element(&mut self, element: Name) -> bool {
        let modified = remove_from(&mut self.replace_with_children, &element);
        if let Some(elements) = &mut self.elements {
            if let Some(i) = elements.iter().position(|e| e.name == element) {
                elements.remove(i);
                return true;
            }
            return modified;
        }
        let remove = self.remove_elements.get_or_insert_with(Vec::new);
        if remove.contains(&element) {
            return modified;
        }
        remove.push(element);
        true
    }

    /// HTML "remove an attribute" from a configuration.
    fn remove_attribute(&mut self, attribute: Name) -> bool {
        if self.attributes.is_some() {
            let mut modified = remove_from(&mut self.attributes, &attribute);
            for e in self.elements.iter_mut().flatten() {
                if remove_from(&mut e.attributes, &attribute) {
                    modified = true;
                }
                if remove_from(&mut e.remove_attributes, &attribute) {
                    modified = true;
                }
            }
            return modified;
        }
        let remove = self.remove_attributes.get_or_insert_with(Vec::new);
        if remove.contains(&attribute) {
            return false;
        }
        for e in self.elements.iter_mut().flatten() {
            remove_from(&mut e.attributes, &attribute);
            remove_from(&mut e.remove_attributes, &attribute);
        }
        self.remove_attributes
            .get_or_insert_with(Vec::new)
            .push(attribute);
        true
    }

    /// HTML "remove unsafe" from a configuration.
    pub(crate) fn remove_unsafe(&mut self) -> bool {
        let mut result = false;
        for (ns, name) in BASELINE_REMOVE_ELEMENTS {
            if self.remove_element(Name::new(name, Some(ns))) {
                result = true;
            }
        }
        for name in EVENT_HANDLER_ATTRIBUTES {
            if self.remove_attribute(Name::new(name, None)) {
                result = true;
            }
        }
        if self.javascript_urls == Some(true) {
            result = true;
        }
        self.javascript_urls = Some(false);
        result
    }

    fn allow_element(&mut self, mut element: ElementEntry) -> bool {
        if self.elements.is_some() {
            let modified = remove_from(&mut self.replace_with_children, &element.name);
            if let Some(global) = self.attributes.clone() {
                if let Some(a) = element.attributes.take() {
                    let mut a = dedup(a);
                    a.retain(|x| !global.contains(x));
                    if self.data_attributes == Some(true) {
                        a.retain(|x| !x.is_custom_data());
                    }
                    element.attributes = Some(a);
                }
                if let Some(r) = element.remove_attributes.take() {
                    let mut r = dedup(r);
                    r.retain(|x| global.contains(x));
                    element.remove_attributes = Some(r);
                }
            } else {
                let global_remove = self.remove_attributes.clone().unwrap_or_default();
                if let Some(a) = element.attributes.take() {
                    let mut a = dedup(a);
                    let local_remove = element.remove_attributes.take().unwrap_or_default();
                    a.retain(|x| !local_remove.contains(x));
                    a.retain(|x| !global_remove.contains(x));
                    element.attributes = Some(a);
                } else if let Some(r) = element.remove_attributes.take() {
                    let mut r = dedup(r);
                    r.retain(|x| !global_remove.contains(x));
                    element.remove_attributes = Some(r);
                }
            }
            let elements = self.elements.get_or_insert_with(Vec::new);
            match elements.iter().position(|e| e.name == element.name) {
                None => {
                    elements.push(element);
                    true
                }
                Some(i) => {
                    if same_entry(&elements[i], &element) {
                        return modified;
                    }
                    elements.remove(i);
                    elements.push(element);
                    true
                }
            }
        } else {
            if element.attributes.is_some()
                || element
                    .remove_attributes
                    .as_ref()
                    .is_some_and(|r| !r.is_empty())
            {
                return false;
            }
            let modified = remove_from(&mut self.replace_with_children, &element.name);
            if !self
                .remove_elements
                .as_ref()
                .is_some_and(|l| l.contains(&element.name))
            {
                return modified;
            }
            remove_from(&mut self.remove_elements, &element.name);
            true
        }
    }

    fn replace_element_with_children(&mut self, element: Name) -> bool {
        if is_non_replaceable(&element) {
            return false;
        }
        let mut modified = match &mut self.elements {
            Some(l) => match l.iter().position(|e| e.name == element) {
                Some(i) => {
                    l.remove(i);
                    true
                }
                None => false,
            },
            None => false,
        };
        if remove_from(&mut self.remove_elements, &element) {
            modified = true;
        }
        let replace = self.replace_with_children.get_or_insert_with(Vec::new);
        if !replace.contains(&element) {
            replace.push(element);
            return true;
        }
        modified
    }

    fn allow_attribute(&mut self, attribute: Name) -> bool {
        if let Some(attributes) = &self.attributes {
            if self.data_attributes == Some(true) && attribute.is_custom_data() {
                return false;
            }
            if attributes.contains(&attribute) {
                return false;
            }
            for e in self.elements.iter_mut().flatten() {
                remove_from(&mut e.attributes, &attribute);
            }
            self.attributes.get_or_insert_with(Vec::new).push(attribute);
            return true;
        }
        if !self
            .remove_attributes
            .as_ref()
            .is_some_and(|l| l.contains(&attribute))
        {
            return false;
        }
        remove_from(&mut self.remove_attributes, &attribute);
        true
    }

    fn set_data_attributes(&mut self, allow: bool) -> bool {
        if self.attributes.is_none() {
            return false;
        }
        if self.data_attributes == Some(allow) {
            return false;
        }
        if allow {
            for e in self.elements.iter_mut().flatten() {
                if let Some(a) = &mut e.attributes {
                    a.retain(|x| !x.is_custom_data());
                }
            }
            if let Some(a) = &mut self.attributes {
                a.retain(|x| !x.is_custom_data());
            }
        }
        self.data_attributes = Some(allow);
        true
    }

    fn allow_processing_instruction(&mut self, pi: String) -> bool {
        if let Some(l) = &mut self.processing_instructions {
            if l.contains(&pi) {
                return false;
            }
            l.push(pi);
            return true;
        }
        if let Some(l) = &mut self.remove_processing_instructions {
            if let Some(i) = l.iter().position(|p| *p == pi) {
                l.remove(i);
                return true;
            }
        }
        false
    }

    fn remove_processing_instruction(&mut self, pi: String) -> bool {
        if let Some(l) = &mut self.processing_instructions {
            if let Some(i) = l.iter().position(|p| *p == pi) {
                l.remove(i);
                return true;
            }
            return false;
        }
        let l = self
            .remove_processing_instructions
            .get_or_insert_with(Vec::new);
        if l.contains(&pi) {
            return false;
        }
        l.push(pi);
        true
    }

    // ---- Queries (HTML §8.6.4) ----

    /// HTML "get the sanitizer action for an element name".
    pub(crate) fn action(&self, local: &str, ns: Option<&str>) -> Action {
        if self
            .replace_with_children
            .as_ref()
            .is_some_and(|l| l.iter().any(|n| n.is(local, ns)))
        {
            return Action::ReplaceWithChildren;
        }
        if let Some(elements) = &self.elements {
            if !elements.iter().any(|e| e.name.is(local, ns)) {
                return Action::Remove;
            }
        } else if self
            .remove_elements
            .as_ref()
            .is_some_and(|l| l.iter().any(|n| n.is(local, ns)))
        {
            return Action::Remove;
        }
        Action::Keep
    }

    /// HTML "check if a sanitizer config allows an attribute".
    pub(crate) fn allows_attribute(
        &self,
        local: &str,
        ns: Option<&str>,
        value: &str,
        element_local: &str,
        element_ns: Option<&str>,
    ) -> bool {
        let entry = self
            .elements
            .as_ref()
            .and_then(|l| l.iter().find(|e| e.name.is(element_local, element_ns)));
        let in_list = |l: Option<&Vec<Name>>| l.is_some_and(|l| l.iter().any(|n| n.is(local, ns)));
        let local_attrs = entry.and_then(|e| e.attributes.as_ref());
        if in_list(entry.and_then(|e| e.remove_attributes.as_ref())) {
            return false;
        } else if self.attributes.is_some() {
            if !in_list(self.attributes.as_ref())
                && !in_list(local_attrs)
                && (!local.starts_with("data-")
                    || ns.is_some()
                    || self.data_attributes != Some(true))
            {
                return false;
            }
        } else if local_attrs.is_some() && !in_list(local_attrs)
            || in_list(self.remove_attributes.as_ref())
        {
            return false;
        }
        if self.javascript_urls == Some(true) {
            return true;
        }
        let is_navigating = match element_ns {
            Some(HTML) => HTML_NAVIGATING
                .iter()
                .any(|&(e, a)| e == element_local && a == local && ns.is_none()),
            Some(SVG) => {
                element_local == "a" && local == "href" && (ns.is_none() || ns == Some(XLINK))
            }
            _ => false,
        };
        if is_navigating && contains_javascript_url(value) {
            return false;
        }
        if element_ns == Some(MATHML)
            && local == "href"
            && (ns.is_none() || ns == Some(XLINK))
            && contains_javascript_url(value)
        {
            return false;
        }
        if element_ns == Some(SVG)
            && matches!(element_local, "animate" | "animateTransform" | "set")
            && local == "attributeName"
            && ns.is_none()
            && (value == "href" || value == "xlink:href")
        {
            return false;
        }
        true
    }

    /// HTML "check if a sanitizer config allows processing instruction target".
    fn allows_pi(&self, target: &str) -> bool {
        if let Some(l) = &self.processing_instructions {
            return l.iter().any(|p| p == target);
        }
        !self
            .remove_processing_instructions
            .as_ref()
            .is_some_and(|l| l.iter().any(|p| p == target))
    }

    // ---- To the dictionary (`get()`) ----

    /// The `get()` method's result: the configuration with its lists sorted.
    fn to_dictionary(&self) -> SanitizerConfig {
        let attr = |n: &Name| {
            StringOrSanitizerAttributeNamespace::SanitizerAttributeNamespace(
                SanitizerAttributeNamespace {
                    name: n.name.clone(),
                    namespace: n.ns.clone(),
                },
            )
        };
        let attrs = |l: &Vec<Name>| sorted_names(l).iter().map(|n| attr(n)).collect::<Vec<_>>();
        let element = |n: &Name| {
            StringOrSanitizerElementNamespace::SanitizerElementNamespace(
                SanitizerElementNamespace {
                    name: n.name.clone(),
                    namespace: n.ns.clone(),
                },
            )
        };
        let pis = |l: &Vec<String>| {
            let mut l = l.clone();
            l.sort_by(|a, b| cmp_code_units(a, b));
            l.into_iter()
                .map(|target| {
                    StringOrSanitizerProcessingInstruction::SanitizerProcessingInstruction(
                        SanitizerProcessingInstruction { target },
                    )
                })
                .collect::<Vec<_>>()
        };
        let elements = self.elements.as_ref().map(|l| {
            let mut l: Vec<&ElementEntry> = l.iter().collect();
            l.sort_by(|a, b| cmp_names(&a.name, &b.name));
            l.into_iter()
                .map(|e| {
                    StringOrSanitizerElementNamespaceWithAttributes::SanitizerElementNamespaceWithAttributes(
                        SanitizerElementNamespaceWithAttributes {
                            name: e.name.name.clone(),
                            namespace: e.name.ns.clone(),
                            attributes: e.attributes.as_ref().map(attrs),
                            remove_attributes: e.remove_attributes.as_ref().map(attrs),
                        },
                    )
                })
                .collect()
        });
        SanitizerConfig {
            elements,
            remove_elements: if self.elements.is_some() {
                None
            } else {
                self.remove_elements
                    .as_ref()
                    .map(|l| sorted_names(l).iter().map(|n| element(n)).collect())
            },
            replace_with_children_elements: self
                .replace_with_children
                .as_ref()
                .map(|l| sorted_names(l).iter().map(|n| element(n)).collect()),
            processing_instructions: self.processing_instructions.as_ref().map(pis),
            remove_processing_instructions: if self.processing_instructions.is_some() {
                None
            } else {
                self.remove_processing_instructions.as_ref().map(pis)
            },
            attributes: self.attributes.as_ref().map(attrs),
            remove_attributes: if self.attributes.is_some() {
                None
            } else {
                self.remove_attributes.as_ref().map(attrs)
            },
            comments: self.comments,
            data_attributes: self.data_attributes,
            javascript_ur_ls: self.javascript_urls,
        }
    }
}

fn has_duplicates<'a, T: PartialEq + 'a>(items: impl Iterator<Item = &'a T>) -> bool {
    let items: Vec<&T> = items.collect();
    items
        .iter()
        .enumerate()
        .any(|(i, a)| items[..i].contains(a))
}

fn dedup(list: Vec<Name>) -> Vec<Name> {
    let mut out: Vec<Name> = Vec::with_capacity(list.len());
    for n in list {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// Two element entries are equal when their names and attribute lists are (lists as ordered
/// sets: the same members).
fn same_entry(a: &ElementEntry, b: &ElementEntry) -> bool {
    fn same(a: &Option<Vec<Name>>, b: &Option<Vec<Name>>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => a.len() == b.len() && a.iter().all(|x| b.contains(x)),
            _ => false,
        }
    }
    a.name == b.name
        && same(&a.attributes, &b.attributes)
        && same(&a.remove_attributes, &b.remove_attributes)
}

fn remove_from<T: PartialEq>(list: &mut Option<Vec<T>>, item: &T) -> bool {
    match list {
        Some(l) => {
            let before = l.len();
            l.retain(|x| x != item);
            l.len() != before
        }
        None => false,
    }
}

fn is_non_replaceable(n: &Name) -> bool {
    n.is("html", Some(HTML)) || n.is("svg", Some(SVG)) || n.is("math", Some(MATHML))
}

fn cmp_code_units(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// HTML "compare sanitizer items": null namespaces first, then by namespace, then by name.
fn cmp_names(a: &Name, b: &Name) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (&a.ns, &b.ns) {
        (None, Some(_)) => return Ordering::Less,
        (Some(_), None) => return Ordering::Greater,
        (Some(x), Some(y)) if x != y => return cmp_code_units(x, y),
        _ => {}
    }
    cmp_code_units(&a.name, &b.name)
}

fn sorted_names(l: &[Name]) -> Vec<&Name> {
    let mut l: Vec<&Name> = l.iter().collect();
    l.sort_by(|a, b| cmp_names(a, b));
    l
}

/// HTML "contains a javascript: URL".
fn contains_javascript_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| u.scheme() == "javascript")
}

/// HTML "configure" a sanitizer: a clone of `configuration` canonicalized; a TypeError when it is
/// not valid.
fn configure(configuration: Config, permissive: bool) -> rt::Result<Config> {
    let mut fresh = configuration;
    fresh.canonicalize(permissive);
    if !fresh.is_valid() {
        return Err(Error::Type(
            "The sanitizer configuration is not valid".into(),
        ));
    }
    Ok(fresh)
}

/// HTML "built-in safe default configuration".
pub(crate) fn safe_default() -> Config {
    let entry = |ns: &str, name: &str, attrs: &[&str]| ElementEntry {
        name: Name::new(name, Some(ns)),
        attributes: Some(attrs.iter().map(|a| Name::new(a, None)).collect()),
        remove_attributes: None,
    };
    let mut elements: Vec<ElementEntry> = HTML_DEFAULT
        .iter()
        .map(|&(name, attrs)| entry(HTML, name, attrs))
        .collect();
    elements.extend(
        FOREIGN_DEFAULT
            .iter()
            .map(|&(svg, name, attrs)| entry(if svg { SVG } else { MATHML }, name, attrs)),
    );
    Config {
        elements: Some(elements),
        processing_instructions: Some(Vec::new()),
        attributes: Some(
            DEFAULT_ATTRIBUTES
                .iter()
                .map(|a| Name::new(a, None))
                .collect(),
        ),
        comments: Some(false),
        data_attributes: Some(false),
        javascript_urls: Some(false),
        ..Config::default()
    }
}

/// The input of HTML "get a sanitizer config from options": an options dictionary's `sanitizer`
/// member.
pub(crate) enum SanitizerInput {
    Sanitizer(Handle),
    Config(SanitizerConfig),
    Default,
    /// A configuration already made (a `TrustedHTMLParserOptions`' own).
    Resolved(Config),
}

/// An empty `SanitizerConfig`: one with no member.
pub(crate) fn is_empty(c: &SanitizerConfig) -> bool {
    c.elements.is_none()
        && c.remove_elements.is_none()
        && c.replace_with_children_elements.is_none()
        && c.processing_instructions.is_none()
        && c.remove_processing_instructions.is_none()
        && c.attributes.is_none()
        && c.remove_attributes.is_none()
        && c.comments.is_none()
        && c.data_attributes.is_none()
        && c.javascript_ur_ls.is_none()
}

impl From<SanitizerOrSanitizerConfigOrSanitizerPresets> for SanitizerInput {
    fn from(v: SanitizerOrSanitizerConfigOrSanitizerPresets) -> SanitizerInput {
        match v {
            SanitizerOrSanitizerConfigOrSanitizerPresets::Sanitizer(h) => {
                SanitizerInput::Sanitizer(h)
            }
            SanitizerOrSanitizerConfigOrSanitizerPresets::SanitizerConfig(c) => {
                SanitizerInput::Config(c)
            }
            SanitizerOrSanitizerConfigOrSanitizerPresets::SanitizerPresets(_) => {
                SanitizerInput::Default
            }
        }
    }
}

impl Engine {
    /// HTML "get a sanitizer config from options": `input` is the options' `sanitizer` member,
    /// `None` when it does not exist. `None` when nothing is to be sanitized (an unsafe method
    /// without a sanitizer).
    pub(crate) fn sanitizer_config_from_options(
        &self,
        input: Option<SanitizerInput>,
        safe: bool,
    ) -> rt::Result<Option<Config>> {
        let input = match input {
            Some(i) => i,
            None if !safe => return Ok(None),
            None => SanitizerInput::Default,
        };
        let mut configuration = configure(self.sanitizer_dictionary(input)?, !safe)?;
        if safe {
            configuration.remove_unsafe();
        }
        Ok(Some(configuration))
    }

    /// The configuration a sanitizer input names, canonical but not yet configured.
    fn sanitizer_dictionary(&self, input: SanitizerInput) -> rt::Result<Config> {
        Ok(match input {
            SanitizerInput::Default => safe_default(),
            SanitizerInput::Sanitizer(h) => match &*self.obj(h) {
                Object::Sanitizer(c) => (**c).clone(),
                _ => return Err(Error::Type("Not a Sanitizer".into())),
            },
            SanitizerInput::Config(c) => canon_dictionary(c),
            SanitizerInput::Resolved(c) => c,
        })
    }

    /// Trusted Types "get a sanitizer from dictionary": the configuration of a new `Sanitizer`
    /// configured from `input` with permissive defaults.
    pub(crate) fn sanitizer_configuration(&self, input: SanitizerInput) -> rt::Result<Config> {
        configure(self.sanitizer_dictionary(input)?, true)
    }

    /// Sanitizes the children of `node` (a fragment, a document or an element) with
    /// `configuration`, and the template contents and shadow roots of the elements it keeps.
    pub(crate) fn sanitize_tree(&self, node: NodeId, configuration: &Config) {
        let children: Vec<NodeId> = self.dom().children(node).collect();
        for child in children {
            let kind = self.dom().node_type(child);
            match kind {
                NodeType::Comment => {
                    if configuration.comments != Some(true) {
                        self.dom_mut().remove_node(child);
                    }
                }
                NodeType::ProcessingInstruction => {
                    let allowed = {
                        let dom = self.dom();
                        configuration.allows_pi(dom.pi_target(child).unwrap_or(""))
                    };
                    if !allowed {
                        self.dom_mut().remove_node(child);
                    }
                }
                NodeType::Element => self.sanitize_element(node, child, configuration),
                _ => {}
            }
        }
    }

    fn sanitize_element(&self, parent: NodeId, element: NodeId, configuration: &Config) {
        let (local, ns) = {
            let dom = self.dom();
            let ns = dom.namespace(element);
            let ns = if ns.is_empty() {
                None
            } else {
                Some(ns.to_string())
            };
            (dom.local_name(element).to_string(), ns)
        };
        match configuration.action(&local, ns.as_deref()) {
            Action::Remove => self.dom_mut().remove_node(element),
            Action::ReplaceWithChildren => {
                self.sanitize_tree(element, configuration);
                let children: Vec<NodeId> = self.dom().children(element).collect();
                let mut dom = self.dom_mut();
                for c in children {
                    let _ = dom.insert_before(parent, c, Some(element));
                }
                dom.remove_node(element);
            }
            Action::Keep => {
                let removed: Vec<(Option<String>, String)> = {
                    let dom = self.dom();
                    dom.attrs(element)
                        .iter()
                        .filter_map(|a| {
                            let a_ns = a.namespace();
                            let a_ns = (!a_ns.is_empty()).then(|| a_ns.to_string());
                            let allowed = configuration.allows_attribute(
                                a.local_name(),
                                a_ns.as_deref(),
                                a.value(),
                                &local,
                                ns.as_deref(),
                            );
                            (!allowed).then(|| (a_ns, a.local_name().to_string()))
                        })
                        .collect()
                };
                for (a_ns, a_local) in removed {
                    self.dom_mut()
                        .remove_attribute_ns(element, a_ns.as_deref(), &a_local);
                }
                let (contents, shadow) = {
                    let dom = self.dom();
                    (dom.template_contents(element), dom.shadow_root(element))
                };
                if let Some(c) = contents {
                    self.sanitize_tree(c, configuration);
                }
                if let Some(s) = shadow {
                    self.sanitize_tree(s, configuration);
                }
                self.sanitize_tree(element, configuration);
            }
        }
    }

    fn sanitizer_config(&self, this: Handle) -> std::cell::RefMut<'_, Config> {
        std::cell::RefMut::map(self.obj_mut(this), |o| match o {
            Object::Sanitizer(c) => &mut **c,
            _ => panic!("not a Sanitizer"),
        })
    }
}

impl SanitizerImpl for Engine {
    fn constructor(
        &self,
        _cx: &mut Cx,
        configuration: SanitizerConfigOrSanitizerPresets,
    ) -> rt::Result<Handle> {
        let dictionary = match configuration {
            SanitizerConfigOrSanitizerPresets::SanitizerPresets(_) => safe_default(),
            SanitizerConfigOrSanitizerPresets::SanitizerConfig(c) => canon_dictionary(c),
        };
        let configuration = configure(dictionary, true)?;
        Ok(self.alloc(Object::Sanitizer(Box::new(configuration))))
    }

    fn get(&self, _cx: &mut Cx, this: Handle) -> rt::Result<SanitizerConfig> {
        Ok(self.sanitizer_config(this).to_dictionary())
    }

    fn allow_element(
        &self,
        _cx: &mut Cx,
        this: Handle,
        element: StringOrSanitizerElementNamespaceWithAttributes,
    ) -> rt::Result<bool> {
        let element = canon_element_with_attributes(element);
        Ok(self.sanitizer_config(this).allow_element(element))
    }

    fn remove_element(
        &self,
        _cx: &mut Cx,
        this: Handle,
        element: StringOrSanitizerElementNamespace,
    ) -> rt::Result<bool> {
        Ok(self
            .sanitizer_config(this)
            .remove_element(canon_element(element)))
    }

    fn replace_element_with_children(
        &self,
        _cx: &mut Cx,
        this: Handle,
        element: StringOrSanitizerElementNamespace,
    ) -> rt::Result<bool> {
        Ok(self
            .sanitizer_config(this)
            .replace_element_with_children(canon_element(element)))
    }

    fn allow_processing_instruction(
        &self,
        _cx: &mut Cx,
        this: Handle,
        pi: StringOrSanitizerProcessingInstruction,
    ) -> rt::Result<bool> {
        Ok(self
            .sanitizer_config(this)
            .allow_processing_instruction(canon_pi(pi)))
    }

    fn remove_processing_instruction(
        &self,
        _cx: &mut Cx,
        this: Handle,
        pi: StringOrSanitizerProcessingInstruction,
    ) -> rt::Result<bool> {
        Ok(self
            .sanitizer_config(this)
            .remove_processing_instruction(canon_pi(pi)))
    }

    fn allow_attribute(
        &self,
        _cx: &mut Cx,
        this: Handle,
        attribute: StringOrSanitizerAttributeNamespace,
    ) -> rt::Result<bool> {
        Ok(self
            .sanitizer_config(this)
            .allow_attribute(canon_attribute(attribute)))
    }

    fn remove_attribute(
        &self,
        _cx: &mut Cx,
        this: Handle,
        attribute: StringOrSanitizerAttributeNamespace,
    ) -> rt::Result<bool> {
        Ok(self
            .sanitizer_config(this)
            .remove_attribute(canon_attribute(attribute)))
    }

    fn set_comments(&self, _cx: &mut Cx, this: Handle, allow: bool) -> rt::Result<bool> {
        let mut c = self.sanitizer_config(this);
        if c.comments == Some(allow) {
            return Ok(false);
        }
        c.comments = Some(allow);
        Ok(true)
    }

    fn set_data_attributes(&self, _cx: &mut Cx, this: Handle, allow: bool) -> rt::Result<bool> {
        Ok(self.sanitizer_config(this).set_data_attributes(allow))
    }

    fn set_javascript_ur_ls(&self, _cx: &mut Cx, this: Handle, allow: bool) -> rt::Result<bool> {
        let mut c = self.sanitizer_config(this);
        if c.javascript_urls == Some(allow) {
            return Ok(false);
        }
        c.javascript_urls = Some(allow);
        Ok(true)
    }

    fn remove_unsafe(&self, _cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        Ok(self.sanitizer_config(this).remove_unsafe())
    }
}

// ---- Constants (HTML §8.6.5) ----

/// The built-in safe baseline configuration's `removeElements`: the HTML elements whose
/// sanitization category is Unsafe, the obsolete `frame`, SVG `script` and `use`.
const BASELINE_REMOVE_ELEMENTS: &[(&str, &str)] = &[
    (HTML, "base"),
    (HTML, "embed"),
    (HTML, "frame"),
    (HTML, "iframe"),
    (HTML, "object"),
    (HTML, "script"),
    (SVG, "script"),
    (SVG, "use"),
];

/// The event handler content attributes: those of the event handlers on elements, documents and
/// windows (the `GlobalEventHandlers`, `WindowEventHandlers` and
/// `DocumentAndElementEventHandlers` mixins).
const EVENT_HANDLER_ATTRIBUTES: &[&str] = &[
    "onabort",
    "onafterprint",
    "onanimationcancel",
    "onanimationend",
    "onanimationiteration",
    "onanimationstart",
    "onauxclick",
    "onbeforeinput",
    "onbeforematch",
    "onbeforeprint",
    "onbeforetoggle",
    "onbeforeunload",
    "onbeforexrselect",
    "onblur",
    "oncancel",
    "oncanplay",
    "oncanplaythrough",
    "onchange",
    "onclick",
    "onclose",
    "oncommand",
    "oncontextlost",
    "oncontextmenu",
    "oncontextrestored",
    "oncopy",
    "oncuechange",
    "oncut",
    "ondblclick",
    "ondrag",
    "ondragend",
    "ondragenter",
    "ondragleave",
    "ondragover",
    "ondragstart",
    "ondrop",
    "ondurationchange",
    "onemptied",
    "onended",
    "onerror",
    "onfencedtreeclick",
    "onfocus",
    "onformdata",
    "ongamepadconnected",
    "ongamepaddisconnected",
    "ongotpointercapture",
    "onhashchange",
    "oninput",
    "oninstallresult",
    "oninvalid",
    "onkeydown",
    "onkeypress",
    "onkeyup",
    "onlanguagechange",
    "onload",
    "onloadeddata",
    "onloadedmetadata",
    "onloadstart",
    "onlostpointercapture",
    "onmessage",
    "onmessageerror",
    "onmousedown",
    "onmouseenter",
    "onmouseleave",
    "onmousemove",
    "onmouseout",
    "onmouseover",
    "onmouseup",
    "onoffline",
    "ononline",
    "onpagehide",
    "onpagereveal",
    "onpageshow",
    "onpageswap",
    "onpaste",
    "onpause",
    "onplay",
    "onplaying",
    "onpointercancel",
    "onpointerdown",
    "onpointerenter",
    "onpointerleave",
    "onpointermove",
    "onpointerout",
    "onpointerover",
    "onpointerrawupdate",
    "onpointerup",
    "onpopstate",
    "onportalactivate",
    "onprogress",
    "onratechange",
    "onrejectionhandled",
    "onreset",
    "onresize",
    "onscroll",
    "onscrollend",
    "onsecuritypolicyviolation",
    "onseeked",
    "onseeking",
    "onselect",
    "onselectionchange",
    "onselectstart",
    "onslotchange",
    "onsnapchanged",
    "onsnapchanging",
    "onstalled",
    "onstorage",
    "onsubmit",
    "onsuspend",
    "ontimeupdate",
    "ontoggle",
    "ontouchcancel",
    "ontouchend",
    "ontouchmove",
    "ontouchstart",
    "ontransitioncancel",
    "ontransitionend",
    "ontransitionrun",
    "ontransitionstart",
    "onunhandledrejection",
    "onunload",
    "onvolumechange",
    "onwaiting",
    "onwebkitanimationend",
    "onwebkitanimationiteration",
    "onwebkitanimationstart",
    "onwebkittransitionend",
    "onwheel",
];

/// The HTML elements whose sanitization category is Default, with their attributes.
const HTML_DEFAULT: &[(&str, &[&str])] = &[
    ("a", &["href", "hreflang", "type"]),
    ("abbr", &[]),
    ("address", &[]),
    ("article", &[]),
    ("aside", &[]),
    ("b", &[]),
    ("bdi", &[]),
    ("bdo", &[]),
    ("blockquote", &["cite"]),
    ("body", &[]),
    ("br", &[]),
    ("caption", &[]),
    ("cite", &[]),
    ("code", &[]),
    ("col", &["span"]),
    ("colgroup", &["span"]),
    ("data", &["value"]),
    ("dd", &[]),
    ("del", &["cite", "datetime"]),
    ("dfn", &[]),
    ("div", &[]),
    ("dl", &[]),
    ("dt", &[]),
    ("em", &[]),
    ("figcaption", &[]),
    ("figure", &[]),
    ("footer", &[]),
    ("h1", &[]),
    ("h2", &[]),
    ("h3", &[]),
    ("h4", &[]),
    ("h5", &[]),
    ("h6", &[]),
    ("head", &[]),
    ("header", &[]),
    ("hgroup", &[]),
    ("hr", &[]),
    ("html", &[]),
    ("i", &[]),
    ("ins", &["cite", "datetime"]),
    ("kbd", &[]),
    ("li", &["value"]),
    ("main", &[]),
    ("mark", &[]),
    ("menu", &[]),
    ("nav", &[]),
    ("ol", &["reversed", "start", "type"]),
    ("p", &[]),
    ("pre", &[]),
    ("q", &[]),
    ("rp", &[]),
    ("rt", &[]),
    ("ruby", &[]),
    ("s", &[]),
    ("samp", &[]),
    ("search", &[]),
    ("section", &[]),
    ("small", &[]),
    ("span", &[]),
    ("strong", &[]),
    ("sub", &[]),
    ("sup", &[]),
    ("table", &[]),
    ("tbody", &[]),
    ("td", &["colspan", "headers", "rowspan"]),
    ("tfoot", &[]),
    ("th", &["abbr", "colspan", "headers", "rowspan", "scope"]),
    ("thead", &[]),
    ("time", &["datetime"]),
    ("title", &[]),
    ("tr", &[]),
    ("u", &[]),
    ("ul", &[]),
    ("var", &[]),
    ("wbr", &[]),
];

/// The HTML elements with navigating URL attributes.
const HTML_NAVIGATING: &[(&str, &str)] = &[
    ("a", "href"),
    ("area", "href"),
    ("form", "action"),
    ("input", "formaction"),
    ("button", "formaction"),
];

/// The MathML (false) and SVG (true) elements of the built-in safe default configuration, with
/// their attributes.
const FOREIGN_DEFAULT: &[(bool, &str, &[&str])] = &[
    (false, "a", &["href", "hreflang", "type"]),
    (false, "math", &[]),
    (false, "merror", &[]),
    (false, "mfrac", &[]),
    (false, "mi", &[]),
    (false, "mmultiscripts", &[]),
    (false, "mn", &[]),
    (
        false,
        "mo",
        &[
            "fence",
            "form",
            "largeop",
            "lspace",
            "maxsize",
            "minsize",
            "movablelimits",
            "rspace",
            "separator",
            "stretchy",
            "symmetric",
        ],
    ),
    (false, "mover", &["accent"]),
    (
        false,
        "mpadded",
        &["depth", "height", "lspace", "voffset", "width"],
    ),
    (false, "mphantom", &[]),
    (false, "mprescripts", &[]),
    (false, "mroot", &[]),
    (false, "mrow", &[]),
    (false, "ms", &[]),
    (false, "mspace", &["depth", "height", "width"]),
    (false, "msqrt", &[]),
    (false, "mstyle", &[]),
    (false, "msub", &[]),
    (false, "msubsup", &[]),
    (false, "msup", &[]),
    (false, "mtable", &[]),
    (false, "mtd", &["columnspan", "rowspan"]),
    (false, "mtext", &[]),
    (false, "mtr", &[]),
    (false, "munder", &["accentunder"]),
    (false, "munderover", &["accent", "accentunder"]),
    (false, "semantics", &[]),
    (true, "a", &["href", "hreflang", "type"]),
    (true, "circle", &["cx", "cy", "pathLength", "r"]),
    (true, "defs", &[]),
    (true, "desc", &[]),
    (true, "ellipse", &["cx", "cy", "pathLength", "rx", "ry"]),
    (true, "foreignObject", &["height", "width", "x", "y"]),
    (true, "g", &[]),
    (true, "line", &["pathLength", "x1", "x2", "y1", "y2"]),
    (
        true,
        "marker",
        &[
            "markerHeight",
            "markerUnits",
            "markerWidth",
            "orient",
            "preserveAspectRatio",
            "refX",
            "refY",
            "viewBox",
        ],
    ),
    (true, "metadata", &[]),
    (true, "path", &["d", "pathLength"]),
    (true, "polygon", &["pathLength", "points"]),
    (true, "polyline", &["pathLength", "points"]),
    (
        true,
        "rect",
        &["height", "pathLength", "rx", "ry", "width", "x", "y"],
    ),
    (
        true,
        "svg",
        &[
            "height",
            "preserveAspectRatio",
            "viewBox",
            "width",
            "x",
            "y",
        ],
    ),
    (
        true,
        "text",
        &["dx", "dy", "lengthAdjust", "rotate", "textLength", "x", "y"],
    ),
    (
        true,
        "textPath",
        &[
            "lengthAdjust",
            "method",
            "path",
            "side",
            "spacing",
            "startOffset",
            "textLength",
        ],
    ),
    (true, "title", &[]),
    (
        true,
        "tspan",
        &["dx", "dy", "lengthAdjust", "rotate", "textLength", "x", "y"],
    ),
];

/// The built-in safe default configuration's global `attributes`.
const DEFAULT_ATTRIBUTES: &[&str] = &[
    "dir",
    "lang",
    "title",
    "alignment-baseline",
    "baseline-shift",
    "clip-path",
    "clip-rule",
    "color",
    "color-interpolation",
    "cursor",
    "direction",
    "display",
    "displaystyle",
    "dominant-baseline",
    "fill",
    "fill-opacity",
    "fill-rule",
    "font-family",
    "font-size",
    "font-size-adjust",
    "font-stretch",
    "font-style",
    "font-variant",
    "font-weight",
    "letter-spacing",
    "marker-end",
    "marker-mid",
    "marker-start",
    "mathbackground",
    "mathcolor",
    "mathsize",
    "opacity",
    "paint-order",
    "pointer-events",
    "scriptlevel",
    "shape-rendering",
    "stop-color",
    "stop-opacity",
    "stroke",
    "stroke-dasharray",
    "stroke-dashoffset",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-opacity",
    "stroke-width",
    "text-anchor",
    "text-decoration",
    "text-overflow",
    "text-rendering",
    "transform",
    "transform-origin",
    "unicode-bidi",
    "vector-effect",
    "visibility",
    "white-space",
    "word-spacing",
    "writing-mode",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_safe_default_is_valid_and_keeps_its_lists() {
        let mut c = safe_default();
        c.canonicalize(false);
        assert!(c.is_valid());
        assert_eq!(c.elements.as_ref().map(Vec::len), Some(75 + 47));
        assert_eq!(c.attributes.as_ref().map(Vec::len), Some(58));
        // The default allows nothing unsafe: removing it changes nothing.
        let before = c.clone();
        c.remove_unsafe();
        assert_eq!(c, before);
    }

    #[test]
    fn remove_unsafe_on_a_remove_list_adds_the_baseline() {
        let mut c = Config::default();
        c.canonicalize(true);
        assert!(c.remove_unsafe());
        assert_eq!(c.action("script", Some(HTML)), Action::Remove);
        assert_eq!(c.action("use", Some(SVG)), Action::Remove);
        assert_eq!(c.action("div", Some(HTML)), Action::Keep);
        assert!(!c.allows_attribute("onclick", None, "x", "div", Some(HTML)));
        assert!(c.allows_attribute("id", None, "x", "div", Some(HTML)));
        assert!(!c.allows_attribute("href", None, "javascript:x", "a", Some(HTML)));
        assert!(c.allows_attribute("href", None, "https://e.x/", "a", Some(HTML)));
    }

    #[test]
    fn validity_rejects_mixed_and_duplicate_lists() {
        let mut c = Config {
            elements: Some(Vec::new()),
            remove_elements: Some(Vec::new()),
            ..Config::default()
        };
        c.canonicalize(true);
        assert!(!c.is_valid());
        let mut c = Config {
            remove_elements: Some(vec![Name::new("p", Some(HTML)), Name::new("p", Some(HTML))]),
            ..Config::default()
        };
        c.canonicalize(true);
        assert!(!c.is_valid());
        let mut c = Config {
            replace_with_children: Some(vec![Name::new("html", Some(HTML))]),
            ..Config::default()
        };
        c.canonicalize(true);
        assert!(!c.is_valid());
    }
}
