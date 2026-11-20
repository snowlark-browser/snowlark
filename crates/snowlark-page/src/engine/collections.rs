// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Collections (DOM §4.2.10): live `HTMLCollection`s and `NodeList`s over a root and a filter, cached
//! until the store's version changes, and static `NodeList`s. Also the HTML collections built on them:
//! `HTMLFormControlsCollection`, `HTMLOptionsCollection`, `RadioNodeList`.

use std::cell::RefCell;

use snowlark_bindings::generated::{
    HTMLAllCollectionImpl, HTMLCollectionImpl, HTMLCollectionOrElement,
    HTMLFormControlsCollectionImpl, HTMLOptionsCollectionImpl, NodeListImpl, RadioNodeListImpl,
    RadioNodeListOrElement,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, ns, Dom, LocalName, Namespace, NodeId, QuirksMode};

use super::{node_handle, Engine, Object};

/// What a live collection contains.
#[derive(Clone, Debug)]
pub enum Filter {
    /// `childNodes`: every child (a NodeList).
    ChildNodes,
    /// `children`: the element children.
    Children,
    /// `getElementsByTagName(qualifiedName)`: `"*"`, or the qualified name (HTML elements in an HTML
    /// document compare with its ASCII lowercase).
    TagName {
        qualified: String,
        lower: String,
    },
    /// `getElementsByTagNameNS(namespace, localName)`: `None` for `"*"`.
    TagNameNs {
        namespace: Option<Namespace>,
        local: Option<LocalName>,
    },
    /// `getElementsByClassName(classNames)`.
    ClassNames(Vec<String>),
    /// `getElementsByName(name)` (a NodeList).
    ByName(String),
    /// A Document's named elements for `name` (HTML §3.1.5 "dom-document-nameditem").
    DocumentNamed(String),
    /// A Window's named objects that are elements, for `name` (HTML §7.2.2.3).
    WindowNamed(String),
    /// `document.all` (HTML §2.7.2.1 HTMLAllCollection): every element of the document.
    All,
    /// The "all"-named elements of `document.all` for a name (HTML §2.7.2.1).
    AllNamed(String),
    Images,
    Embeds,
    Links,
    Forms,
    Scripts,
    Anchors,
    Applets,
    /// `form.elements`: the listed elements whose form owner is the form.
    FormElements(NodeId),
    /// `fieldset.elements`: listed element descendants.
    FieldsetElements,
    /// `select.options`: option children, and option children of optgroup children.
    Options,
    SelectedOptions,
    /// `datalist.options`: option descendants.
    DataListOptions,
    Areas,
    TBodies,
    TableRows,
    SectionRows,
    Cells,
    /// `labels` of a labelable element (a NodeList).
    Labels(NodeId),
    /// A RadioNodeList: the form's controls named `name`.
    RadioNodes {
        form: NodeId,
        name: String,
    },
}

impl Filter {
    /// The counter the cache is valid for: a collection whose membership reads only the tree's shape and
    /// element names (which never change) outlives attribute and character data changes, one that reads
    /// only `id`, `name`, `class` or `href` outlives other attributes' changes, any other compares the
    /// store's version.
    #[inline]
    fn cache_key(&self, dom: &Dom) -> u64 {
        if self.structural() {
            dom.tree_version()
        } else if matches!(
            self,
            Filter::ClassNames(_)
                | Filter::ByName(_)
                | Filter::DocumentNamed(_)
                | Filter::WindowNamed(_)
                | Filter::AllNamed(_)
                | Filter::Links
                | Filter::Anchors
        ) {
            dom.key_attr_tree_version()
        } else {
            dom.version()
        }
    }

    fn structural(&self) -> bool {
        matches!(
            self,
            Filter::ChildNodes
                | Filter::Children
                | Filter::TagName { .. }
                | Filter::TagNameNs { .. }
                | Filter::All
                | Filter::Images
                | Filter::Embeds
                | Filter::Forms
                | Filter::Scripts
                | Filter::Applets
                | Filter::Options
                | Filter::DataListOptions
                | Filter::Areas
                | Filter::TBodies
                | Filter::TableRows
                | Filter::SectionRows
                | Filter::Cells
        )
    }

    /// How the members are found when they can be found one at a time in tree order, so that an
    /// indexed read walks only as far as its index; `None` for the collections computed whole.
    fn walk(&self) -> Option<Walk> {
        match self {
            Filter::ChildNodes
            | Filter::Children
            | Filter::TBodies
            | Filter::SectionRows
            | Filter::Cells => Some(Walk::Children),
            Filter::ClassNames(c) if c.is_empty() => None,
            Filter::TagName { .. }
            | Filter::TagNameNs { .. }
            | Filter::ClassNames(_)
            | Filter::ByName(_)
            | Filter::DocumentNamed(_)
            | Filter::WindowNamed(_)
            | Filter::All
            | Filter::AllNamed(_)
            | Filter::Images
            | Filter::Embeds
            | Filter::Links
            | Filter::Forms
            | Filter::Scripts
            | Filter::Anchors
            | Filter::FieldsetElements
            | Filter::DataListOptions
            | Filter::Areas => Some(Walk::Descendants),
            _ => None,
        }
    }
}

/// The candidates of a collection walked one node at a time: the root's children, or its descendants
/// in tree order.
#[derive(Clone, Copy)]
enum Walk {
    Children,
    Descendants,
}

impl Walk {
    fn first(self, dom: &Dom, root: NodeId) -> Option<NodeId> {
        dom.first_child(root)
    }

    fn next(self, dom: &Dom, root: NodeId, node: NodeId) -> Option<NodeId> {
        match self {
            Walk::Children => dom.next_sibling(node),
            Walk::Descendants => dom.next_in_tree_order(node, Some(root)),
        }
    }
}

/// Whether a candidate of a walked collection is a member (the filters [`Filter::walk`] walks).
struct Matcher<'a> {
    dom: &'a Dom,
    filter: &'a Filter,
    html_doc: bool,
    quirks: bool,
}

impl<'a> Matcher<'a> {
    fn new(dom: &'a Dom, root: NodeId, filter: &'a Filter) -> Self {
        let doc = dom.node_document(root);
        let (html_doc, quirks) = match filter {
            Filter::TagName { .. } => (dom.is_html_document(doc), false),
            Filter::ClassNames(_) => (false, dom.quirks_mode(doc) == QuirksMode::Quirks),
            _ => (false, false),
        };
        Matcher {
            dom,
            filter,
            html_doc,
            quirks,
        }
    }

    fn matches(&self, d: NodeId) -> bool {
        let dom = self.dom;
        match self.filter {
            Filter::ChildNodes => true,
            Filter::Children | Filter::All => dom.is_element(d),
            Filter::TBodies => is_html(dom, d, &local_name!("tbody")),
            Filter::SectionRows => is_html(dom, d, &local_name!("tr")),
            Filter::Cells => {
                is_html(dom, d, &local_name!("td")) || is_html(dom, d, &local_name!("th"))
            }
            Filter::AllNamed(name) => dom.is_element(d) && is_all_named(dom, d, name),
            Filter::TagName { qualified, lower } => {
                let Some(e) = dom.element(d) else {
                    return false;
                };
                if qualified == "*" {
                    return true;
                }
                let q = snowlark_dom::qualified_name(e.name());
                if self.html_doc && e.name().ns == ns!(html) {
                    *q == *lower
                } else {
                    *q == *qualified
                }
            }
            Filter::TagNameNs { namespace, local } => dom.element(d).is_some_and(|e| {
                let n = e.name();
                namespace.as_ref().is_none_or(|ns| n.ns == *ns)
                    && local.as_ref().is_none_or(|l| n.local == *l)
            }),
            Filter::ClassNames(classes) => {
                dom.is_element(d) && has_class(dom, d, classes, self.quirks)
            }
            Filter::ByName(name) => {
                dom.is_element(d)
                    && dom.namespace(d) == &ns!(html)
                    && html_attr(dom, d, &local_name!("name")) == Some(name.as_str())
            }
            Filter::DocumentNamed(name) => dom.is_element(d) && is_document_named(dom, d, name),
            Filter::WindowNamed(name) => dom.is_element(d) && is_window_named(dom, d, name),
            Filter::Images => is_html(dom, d, &local_name!("img")),
            Filter::Embeds => is_html(dom, d, &local_name!("embed")),
            Filter::Links => {
                (is_html(dom, d, &local_name!("a")) || is_html(dom, d, &local_name!("area")))
                    && html_attr(dom, d, &local_name!("href")).is_some()
            }
            Filter::Forms => is_html(dom, d, &local_name!("form")),
            Filter::Scripts => is_html(dom, d, &local_name!("script")),
            Filter::Anchors => {
                is_html(dom, d, &local_name!("a"))
                    && html_attr(dom, d, &local_name!("name")).is_some()
            }
            Filter::FieldsetElements => is_listed(dom, d),
            Filter::DataListOptions => is_html(dom, d, &local_name!("option")),
            Filter::Areas => is_html(dom, d, &local_name!("area")),
            _ => unreachable!("not a walked collection"),
        }
    }
}

enum Kind {
    Live { root: NodeId, filter: Filter },
    Fixed(Vec<NodeId>),
}

/// A live collection's members as far as they have been found (DOM §4.2.10.1: a collection's
/// members are computed when read; this keeps them until the tree changes).
struct Cache {
    /// The filter's counter ([`Filter::cache_key`]) when the members were found.
    key: u64,
    /// The members found so far, in order.
    nodes: Vec<NodeId>,
    /// The next candidate of a walk that stopped early, `None` once every member is found.
    resume: Option<NodeId>,
}

/// A collection object.
pub struct Collection {
    kind: Kind,
    cache: RefCell<Option<Cache>>,
}

impl Collection {
    pub fn live(root: NodeId, filter: Filter) -> Collection {
        Collection {
            kind: Kind::Live { root, filter },
            cache: RefCell::new(None),
        }
    }

    pub fn fixed(nodes: Vec<NodeId>) -> Collection {
        Collection {
            kind: Kind::Fixed(nodes),
            cache: RefCell::new(None),
        }
    }

    /// The node whose wrapper keeps a live collection (ARCHITECTURE.md "Wrappers and GC"): the
    /// element it is about, else its root. `None` for a static list.
    pub(crate) fn live_anchor(&self) -> Option<NodeId> {
        match &self.kind {
            Kind::Fixed(_) => None,
            Kind::Live { root, filter } => Some(match filter {
                Filter::FormElements(n) | Filter::Labels(n) => *n,
                Filter::RadioNodes { form, .. } => *form,
                _ => *root,
            }),
        }
    }

    /// The nodes a collection refers to besides its anchor: a static list's nodes, a live
    /// collection's root when it is not the anchor.
    pub(crate) fn gc_refs(&self, out: &mut Vec<NodeId>) {
        match &self.kind {
            Kind::Fixed(nodes) => out.extend(nodes),
            Kind::Live { root, .. } => {
                if self.live_anchor() != Some(*root) {
                    out.push(*root);
                }
            }
        }
    }

    pub fn interface(&self) -> InterfaceId {
        match &self.kind {
            Kind::Fixed(_) => InterfaceId::NodeList,
            Kind::Live { filter, .. } => match filter {
                Filter::ChildNodes | Filter::ByName(_) | Filter::Labels(_) => InterfaceId::NodeList,
                Filter::RadioNodes { .. } => InterfaceId::RadioNodeList,
                Filter::FormElements(_) | Filter::FieldsetElements => {
                    InterfaceId::HTMLFormControlsCollection
                }
                Filter::Options => InterfaceId::HTMLOptionsCollection,
                Filter::All => InterfaceId::HTMLAllCollection,
                _ => InterfaceId::HTMLCollection,
            },
        }
    }

    /// The root of a live collection.
    pub fn root(&self) -> Option<NodeId> {
        match &self.kind {
            Kind::Live { root, .. } => Some(*root),
            Kind::Fixed(_) => None,
        }
    }

    /// Runs `f` over the collection's nodes, in order.
    pub fn with<R>(&self, dom: &Dom, f: impl FnOnce(&[NodeId]) -> R) -> R {
        let (root, filter) = match &self.kind {
            Kind::Fixed(v) => return f(v),
            Kind::Live { root, filter } => (*root, filter),
        };
        self.fill(dom, root, filter, usize::MAX);
        f(&self.cache.borrow().as_ref().unwrap().nodes)
    }

    /// The node at `index`: a walked collection finds members only up to it, and the next read goes on
    /// from there while the tree stays the same (reading items in order is linear, not quadratic).
    #[inline]
    pub fn item(&self, dom: &Dom, index: usize) -> Option<NodeId> {
        let (root, filter) = match &self.kind {
            Kind::Fixed(v) => return v.get(index).copied(),
            Kind::Live { root, filter } => (*root, filter),
        };
        if let Some(c) = &*self.cache.borrow() {
            if c.key == filter.cache_key(dom) {
                if let Some(&n) = c.nodes.get(index) {
                    return Some(n);
                }
                // (every member found and none at `index`)
                c.resume?;
            }
        }
        self.fill(dom, root, filter, index);
        let cache = self.cache.borrow();
        cache.as_ref().unwrap().nodes.get(index).copied()
    }

    /// The number of members.
    #[inline]
    pub fn length(&self, dom: &Dom) -> usize {
        let (root, filter) = match &self.kind {
            Kind::Fixed(v) => return v.len(),
            Kind::Live { root, filter } => (*root, filter),
        };
        if let Some(c) = &*self.cache.borrow() {
            if c.resume.is_none() && c.key == filter.cache_key(dom) {
                return c.nodes.len();
            }
        }
        self.fill(dom, root, filter, usize::MAX);
        let cache = self.cache.borrow();
        cache.as_ref().unwrap().nodes.len()
    }

    /// Brings the cache up to date and finds members until more than `want` of them are known (or all).
    #[inline(never)]
    fn fill(&self, dom: &Dom, root: NodeId, filter: &Filter, want: usize) {
        let key = filter.cache_key(dom);
        let walk = filter.walk();
        let mut cache = self.cache.borrow_mut();
        if !matches!(&*cache, Some(c) if c.key == key) {
            *cache = Some(match walk {
                Some(w) => Cache {
                    key,
                    nodes: Vec::new(),
                    resume: w.first(dom, root),
                },
                None => Cache {
                    key,
                    nodes: compute(dom, root, filter),
                    resume: None,
                },
            });
        }
        let c = cache.as_mut().unwrap();
        let (Some(walk), Some(mut next)) = (walk, c.resume) else {
            return;
        };
        if c.nodes.len() > want {
            return;
        }
        // Reads in order (`item(0)`, `item(1)`, ... as script loops over a collection) find members
        // ahead, as many again as are known: such a loop comes here a logarithmic number of times, not
        // once per read, and a single read of the first member walks no further than it.
        let want = want.max(c.nodes.len() * 2);
        let m = Matcher::new(dom, root, filter);
        loop {
            let after = walk.next(dom, root, next);
            if m.matches(next) {
                c.nodes.push(next);
                if c.nodes.len() > want {
                    c.resume = after;
                    return;
                }
            }
            match after {
                Some(n) => next = n,
                None => {
                    c.resume = None;
                    return;
                }
            }
        }
    }
}

fn is_html(dom: &Dom, id: NodeId, local: &LocalName) -> bool {
    dom.is_html_element_named(id, local)
}

fn html_attr<'a>(dom: &'a Dom, id: NodeId, local: &LocalName) -> Option<&'a str> {
    dom.attr(id, &ns!(), local)
}

/// The class names of an element, split on ASCII whitespace.
pub(crate) fn has_class(dom: &Dom, el: NodeId, wanted: &[String], quirks: bool) -> bool {
    let Some(classes) = html_attr(dom, el, &local_name!("class")) else {
        return false;
    };
    wanted.iter().all(|w| {
        classes.split_ascii_whitespace().any(|c| {
            if quirks {
                c.eq_ignore_ascii_case(w)
            } else {
                c == w
            }
        })
    })
}

/// Listed elements (HTML §4.10.2): button, fieldset, input, object, output, select, textarea, and
/// form-associated custom elements (not yet).
pub(crate) fn is_listed(dom: &Dom, id: NodeId) -> bool {
    let Some(e) = dom.element(id) else {
        return false;
    };
    e.name().ns == ns!(html)
        && (matches!(
            e.name().local,
            local_name!("button")
                | local_name!("fieldset")
                | local_name!("input")
                | local_name!("object")
                | local_name!("output")
                | local_name!("select")
                | local_name!("textarea")
        ) || dom.element_flags(id) & snowlark_dom::element_flags::FORM_ASSOCIATED_CUSTOM != 0)
}

/// A form-associated element's form owner (HTML §4.10.17.3), computed from the tree: the element named
/// by its `form` attribute, else the nearest form ancestor.
/// HTML §4.10.18.3 "reset the form owner": a connected element with a `form` attribute is owned by
/// the form with that ID (or none); any other by its nearest ancestor form.
pub(crate) fn form_owner(dom: &Dom, el: NodeId) -> Option<NodeId> {
    if let Some(id) = html_attr(dom, el, &local_name!("form")).filter(|_| dom.is_connected(el)) {
        if id.is_empty() {
            return None;
        }
        let root = dom.root(el);
        let target = dom.descendants(root).find(|&d| {
            dom.element(d).is_some() && dom.attr(d, &ns!(), &local_name!("id")) == Some(id)
        })?;
        return is_html(dom, target, &local_name!("form")).then_some(target);
    }
    dom.ancestors(el)
        .find(|&a| is_html(dom, a, &local_name!("form")))
}

fn compute(dom: &Dom, root: NodeId, filter: &Filter) -> Vec<NodeId> {
    if let Some(walk) = filter.walk() {
        let m = Matcher::new(dom, root, filter);
        let mut out = Vec::new();
        let mut next = walk.first(dom, root);
        while let Some(n) = next {
            if m.matches(n) {
                out.push(n);
            }
            next = walk.next(dom, root, n);
        }
        return out;
    }
    match filter {
        Filter::ClassNames(_) | Filter::Applets => Vec::new(),
        Filter::FormElements(form) => {
            let tree = dom.root(*form);
            dom.descendants(tree)
                .filter(|&d| {
                    is_listed(dom, d)
                        && !(is_html(dom, d, &local_name!("input"))
                            && html_attr(dom, d, &local_name!("type"))
                                .is_some_and(|t| t.eq_ignore_ascii_case("image")))
                        && form_owner(dom, d) == Some(*form)
                })
                .collect()
        }
        Filter::Options => {
            let mut out = Vec::new();
            for c in dom.children(root) {
                if is_html(dom, c, &local_name!("option")) {
                    out.push(c);
                } else if is_html(dom, c, &local_name!("optgroup")) {
                    out.extend(
                        dom.children(c)
                            .filter(|&o| is_html(dom, o, &local_name!("option"))),
                    );
                }
            }
            out
        }
        Filter::SelectedOptions => compute(dom, root, &Filter::Options)
            .into_iter()
            .filter(|&o| html_attr(dom, o, &local_name!("selected")).is_some())
            .collect(),
        Filter::TableRows => {
            // HTML §4.9.1: thead rows, then direct and tbody rows in tree order, then tfoot rows.
            let rows_of = |section: NodeId| {
                dom.children(section)
                    .filter(|&r| is_html(dom, r, &local_name!("tr")))
                    .collect::<Vec<_>>()
            };
            let mut out = Vec::new();
            for c in dom.children(root) {
                if is_html(dom, c, &local_name!("thead")) {
                    out.extend(rows_of(c));
                }
            }
            for c in dom.children(root) {
                if is_html(dom, c, &local_name!("tr")) {
                    out.push(c);
                } else if is_html(dom, c, &local_name!("tbody")) {
                    out.extend(rows_of(c));
                }
            }
            for c in dom.children(root) {
                if is_html(dom, c, &local_name!("tfoot")) {
                    out.extend(rows_of(c));
                }
            }
            out
        }
        Filter::Labels(el) => {
            let tree = dom.root(*el);
            dom.descendants(tree)
                .filter(|&d| {
                    is_html(dom, d, &local_name!("label"))
                        && super::html::labeled_control(dom, d) == Some(*el)
                })
                .collect()
        }
        Filter::RadioNodes { form, name } => compute(dom, root, &Filter::FormElements(*form))
            .into_iter()
            .filter(|&d| {
                html_attr(dom, d, &local_name!("id")) == Some(name.as_str())
                    || html_attr(dom, d, &local_name!("name")) == Some(name.as_str())
            })
            .collect(),
        Filter::ChildNodes
        | Filter::Children
        | Filter::TBodies
        | Filter::SectionRows
        | Filter::Cells
        | Filter::TagName { .. }
        | Filter::TagNameNs { .. }
        | Filter::ByName(_)
        | Filter::DocumentNamed(_)
        | Filter::WindowNamed(_)
        | Filter::All
        | Filter::AllNamed(_)
        | Filter::Images
        | Filter::Embeds
        | Filter::Links
        | Filter::Forms
        | Filter::Scripts
        | Filter::Anchors
        | Filter::FieldsetElements
        | Filter::DataListOptions
        | Filter::Areas => unreachable!("walked above"),
    }
}

impl Engine {
    fn coll<R>(&self, h: Handle, f: impl FnOnce(&Dom, &[NodeId]) -> R) -> R {
        let objects = self.objects.borrow();
        let Object::Collection(c) = &objects[h.get() as usize - 1] else {
            panic!("not a collection");
        };
        // (`selectedOptions` follows the options' selectedness, which is the engine's state, not
        // only the `selected` attributes)
        if let Kind::Live {
            root,
            filter: Filter::SelectedOptions,
        } = &c.kind
        {
            let select = *root;
            drop(objects);
            let nodes = self.selected_options_of(select);
            let dom = self.dom();
            return f(&dom, &nodes);
        }
        let dom = self.dom();
        c.with(&dom, |nodes| f(&dom, nodes))
    }

    fn coll_root(&self, h: Handle) -> Option<NodeId> {
        match &*self.obj(h) {
            Object::Collection(c) => c.root(),
            _ => None,
        }
    }

    /// `f` on the collection and the DOM, except for `selectedOptions` (`None`: that one goes through
    /// [`Self::coll`]).
    fn coll_direct<R>(&self, h: Handle, f: impl FnOnce(&Dom, &Collection) -> R) -> Option<R> {
        let objects = self.objects.borrow();
        let Object::Collection(c) = &objects[h.get() as usize - 1] else {
            panic!("not a collection");
        };
        if matches!(
            c.kind,
            Kind::Live {
                filter: Filter::SelectedOptions,
                ..
            }
        ) {
            return None;
        }
        Some(f(&self.dom(), c))
    }

    pub(crate) fn coll_len(&self, h: Handle) -> u32 {
        self.coll_direct(h, |dom, c| c.length(dom) as u32)
            .unwrap_or_else(|| self.coll(h, |_, n| n.len() as u32))
    }

    pub(crate) fn coll_item(&self, h: Handle, index: u32) -> Option<Handle> {
        self.coll_direct(h, |dom, c| c.item(dom, index as usize))
            .unwrap_or_else(|| self.coll(h, |_, n| n.get(index as usize).copied()))
            .map(node_handle)
    }

    /// HTMLCollection's named item: the first element whose ID is `name`, or an HTML element whose
    /// `name` attribute is.
    fn coll_named(&self, h: Handle, name: &str) -> Option<Handle> {
        if name.is_empty() {
            return None;
        }
        self.coll(h, |dom, nodes| {
            nodes
                .iter()
                .copied()
                .find(|&e| {
                    dom.attr(e, &ns!(), &local_name!("id")) == Some(name)
                        || (dom.namespace(e) == &ns!(html)
                            && dom.attr(e, &ns!(), &local_name!("name")) == Some(name))
                })
                .map(node_handle)
        })
    }

    fn coll_names(&self, h: Handle) -> Vec<String> {
        self.coll(h, |dom, nodes| {
            let mut out: Vec<String> = Vec::new();
            for &e in nodes {
                if let Some(id) = dom.attr(e, &ns!(), &local_name!("id")) {
                    if !id.is_empty() && !out.iter().any(|o| o == id) {
                        out.push(id.to_owned());
                    }
                }
                if dom.namespace(e) == &ns!(html) {
                    if let Some(n) = dom.attr(e, &ns!(), &local_name!("name")) {
                        if !n.is_empty() && !out.iter().any(|o| o == n) {
                            out.push(n.to_owned());
                        }
                    }
                }
            }
            out
        })
    }

    /// The form control collection's named item (HTML §2.7.2.1): one element, or a RadioNodeList when
    /// several match.
    pub(crate) fn controls_named(&self, h: Handle, name: &str) -> Option<RadioNodeListOrElement> {
        if name.is_empty() {
            return None;
        }
        let matches: Vec<NodeId> = self.coll(h, |dom, nodes| {
            nodes
                .iter()
                .copied()
                .filter(|&e| {
                    dom.attr(e, &ns!(), &local_name!("id")) == Some(name)
                        || dom.attr(e, &ns!(), &local_name!("name")) == Some(name)
                })
                .collect()
        });
        match matches.len() {
            0 => None,
            1 => Some(RadioNodeListOrElement::Element(node_handle(matches[0]))),
            _ => {
                let root = self.coll_root(h)?;
                let form = match &*self.obj(h) {
                    Object::Collection(c) => match &c.kind {
                        Kind::Live {
                            filter: Filter::FormElements(f),
                            ..
                        } => *f,
                        _ => root,
                    },
                    _ => root,
                };
                let filter = if form == root
                    && !self.dom().is_html_element_named(form, &local_name!("form"))
                {
                    // A fieldset: the RadioNodeList holds its matching listed descendants.
                    return Some(RadioNodeListOrElement::RadioNodeList(
                        self.alloc(Object::Collection(Collection::fixed(matches))),
                    ));
                } else {
                    Filter::RadioNodes {
                        form,
                        name: name.to_owned(),
                    }
                };
                Some(RadioNodeListOrElement::RadioNodeList(
                    self.collection(root, filter),
                ))
            }
        }
    }
}

impl HTMLCollectionImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        self.coll_len(this)
    }
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        self.coll_item(this, index)
    }
    fn named_item(&self, this: Handle, name: &str) -> Option<Handle> {
        self.coll_named(this, name)
    }
    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        self.coll_names(this)
    }
}

/// HTML §2.7.2.1: whether `el` is one of the "all"-named elements for `name`: its ID is `name`, or it
/// is an `a`, `button`, `embed`, `form`, `frame`, `frameset`, `iframe`, `img`, `input`, `map`, `meta`,
/// `object`, `select` or `textarea` HTML element whose `name` attribute is `name`.
fn is_all_named(dom: &Dom, el: NodeId, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if dom.attr(el, &ns!(), &local_name!("id")) == Some(name) {
        return true;
    }
    let named_kind = [
        local_name!("a"),
        local_name!("button"),
        local_name!("embed"),
        local_name!("form"),
        local_name!("frame"),
        local_name!("frameset"),
        local_name!("iframe"),
        local_name!("img"),
        local_name!("input"),
        local_name!("map"),
        local_name!("meta"),
        local_name!("object"),
        local_name!("select"),
        local_name!("textarea"),
    ]
    .iter()
    .any(|l| dom.is_html_element_named(el, l));
    named_kind && dom.attr(el, &ns!(), &local_name!("name")) == Some(name)
}

impl Engine {
    /// `document.all` of `doc` (one object per document).
    pub(crate) fn document_all(&self, doc: NodeId) -> Handle {
        if let Some(h) = self.doc_state(doc, |s| s.all) {
            return h;
        }
        let h = self.collection(doc, Filter::All);
        self.doc_state(doc, |s| s.all = Some(h));
        h
    }

    /// HTML §2.7.2.1 "get the 'all'-named element(s)": none, the element, or a collection of them.
    fn all_named(&self, this: Handle, name: &str) -> Option<HTMLCollectionOrElement> {
        let root = self.coll_root(this)?;
        let found: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(root)
                .filter(|&d| dom.is_element(d) && is_all_named(&dom, d, name))
                .take(2)
                .collect()
        };
        match found.as_slice() {
            [] => None,
            [one] => Some(HTMLCollectionOrElement::Element(node_handle(*one))),
            _ => Some(HTMLCollectionOrElement::HTMLCollection(
                self.collection(root, Filter::AllNamed(name.to_owned())),
            )),
        }
    }
}

impl HTMLAllCollectionImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        self.coll_len(this)
    }
    fn indexed_getter(&self, this: Handle, index: u32) -> Option<Handle> {
        self.coll_item(this, index)
    }
    fn named_item(&self, this: Handle, name: &str) -> Option<HTMLCollectionOrElement> {
        self.all_named(this, name)
    }
    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        self.coll_names(this)
    }
    /// HTML §2.7.2.1 `item(nameOrIndex)` (and the [[Call]] of `document.all(...)`): null without an
    /// argument, the element at an array index, else the "all"-named element(s).
    fn item(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name_or_index: Option<&str>,
    ) -> rt::Result<Option<HTMLCollectionOrElement>> {
        let Some(key) = name_or_index else {
            return Ok(None);
        };
        // An array index: the canonical decimal form of an integer below 2^32 - 1.
        if let Ok(i) = key.parse::<u32>() {
            if i != u32::MAX && i.to_string() == key {
                return Ok(self
                    .coll_item(this, i)
                    .map(HTMLCollectionOrElement::Element));
            }
        }
        Ok(self.all_named(this, key))
    }
}

impl NodeListImpl for Engine {
    fn item(&self, this: Handle, index: u32) -> Option<Handle> {
        self.coll_item(this, index)
    }
    fn length(&self, this: Handle) -> u32 {
        self.coll_len(this)
    }
}

impl HTMLFormControlsCollectionImpl for Engine {
    fn named_item(&self, this: Handle, name: &str) -> Option<RadioNodeListOrElement> {
        self.controls_named(this, name)
    }
    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        self.coll_names(this)
    }
}

impl RadioNodeListImpl for Engine {
    /// The value of the first checked radio button in the list (HTML §2.7.2.2).
    fn value(&self, this: Handle) -> DomStr<'_> {
        let checked = self.coll(this, |dom, nodes| {
            nodes.iter().copied().find(|&e| {
                is_html(dom, e, &local_name!("input"))
                    && html_attr(dom, e, &local_name!("type"))
                        .is_some_and(|t| t.eq_ignore_ascii_case("radio"))
            })
        });
        let Some(radio) = checked.filter(|&r| self.checkedness(r)) else {
            return DomStr::Borrowed("");
        };
        let dom = self.dom();
        DomStr::Owned(
            html_attr(&dom, radio, &local_name!("value"))
                .unwrap_or("on")
                .to_owned(),
        )
    }
    fn set_value(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let radios: Vec<NodeId> = self.coll(this, |dom, nodes| {
            nodes
                .iter()
                .copied()
                .filter(|&e| {
                    is_html(dom, e, &local_name!("input"))
                        && html_attr(dom, e, &local_name!("type"))
                            .is_some_and(|t| t.eq_ignore_ascii_case("radio"))
                        && html_attr(dom, e, &local_name!("value")).unwrap_or("on") == value
                })
                .collect()
        });
        if let Some(&r) = radios.first() {
            self.set_checkedness(cx, r, true);
        }
        Ok(())
    }
}

impl HTMLOptionsCollectionImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        self.coll_len(this)
    }
    fn set_length(&self, cx: &mut Cx, this: Handle, value: u32) -> rt::Result<()> {
        let select = self
            .coll_root(this)
            .expect("an options collection's select");
        self.set_options_length(cx, select, value)
    }
    fn indexed_setter(
        &self,
        cx: &mut Cx,
        this: Handle,
        index: u32,
        value: Option<Handle>,
    ) -> rt::Result<()> {
        let select = self
            .coll_root(this)
            .expect("an options collection's select");
        self.set_option_at(cx, select, index, value)
    }
    fn selected_index(&self, this: Handle) -> i32 {
        let select = self
            .coll_root(this)
            .expect("an options collection's select");
        self.select_selected_index(select)
    }
    fn set_selected_index(&self, cx: &mut Cx, this: Handle, value: i32) -> rt::Result<()> {
        let select = self
            .coll_root(this)
            .expect("an options collection's select");
        self.select_set_selected_index(cx, select, value);
        Ok(())
    }
    fn add(
        &self,
        cx: &mut Cx,
        this: Handle,
        element: snowlark_bindings::generated::HTMLOptionElementOrHTMLOptGroupElement,
        before: Option<snowlark_bindings::generated::HTMLElementOrLong>,
    ) -> rt::Result<()> {
        let select = self
            .coll_root(this)
            .expect("an options collection's select");
        self.select_add(cx, select, element, before)
    }
    fn remove(&self, cx: &mut Cx, this: Handle, index: i32) -> rt::Result<()> {
        let select = self
            .coll_root(this)
            .expect("an options collection's select");
        self.select_remove_index(cx, select, index)
    }
}

/// HTML §3.1.5: whether `el` is a named element of its Document for `name`: an embed, form, iframe,
/// img or object element named `name`, an object element with that id, or an img element with that
/// id and a non-empty name.
pub(crate) fn is_document_named(dom: &Dom, el: NodeId, name: &str) -> bool {
    if name.is_empty() || dom.namespace(el) != &ns!(html) {
        return false;
    }
    let local = dom.local_name(el);
    let by_name = matches!(
        *local,
        local_name!("embed")
            | local_name!("form")
            | local_name!("iframe")
            | local_name!("img")
            | local_name!("object")
    ) && html_attr(dom, el, &local_name!("name")) == Some(name);
    let id = html_attr(dom, el, &local_name!("id")) == Some(name);
    by_name
        || (id && *local == local_name!("object"))
        || (id
            && *local == local_name!("img")
            && html_attr(dom, el, &local_name!("name")).is_some_and(|n| !n.is_empty()))
}

/// HTML §7.2.2.3: whether `el` is a named object of its Window for `name`: an embed, form, img or
/// object element named `name`, or any element with that id.
pub(crate) fn is_window_named(dom: &Dom, el: NodeId, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if dom.attr(el, &ns!(), &local_name!("id")) == Some(name) {
        return true;
    }
    dom.namespace(el) == &ns!(html)
        && matches!(
            *dom.local_name(el),
            local_name!("embed") | local_name!("form") | local_name!("img") | local_name!("object")
        )
        && html_attr(dom, el, &local_name!("name")) == Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(c: &Collection) -> (usize, bool) {
        let cache = c.cache.borrow();
        let cache = cache.as_ref().unwrap();
        (cache.nodes.len(), cache.resume.is_some())
    }

    #[test]
    fn indexed_reads_in_order_walk_ahead_by_what_is_known() {
        let mut dom = Dom::new();
        let doc = dom.create_document(snowlark_dom::DocumentKind::Html);
        let root = dom.create_element(doc, "div", None).unwrap();
        dom.append_child(doc, root).unwrap();
        let mut ps = Vec::new();
        for _ in 0..40 {
            let p = dom.create_element(doc, "p", None).unwrap();
            dom.append_child(root, p).unwrap();
            ps.push(p);
        }
        let c = Collection::live(
            root,
            Filter::TagName {
                qualified: "p".into(),
                lower: "p".into(),
            },
        );
        // The first member alone: no further.
        assert_eq!(c.item(&dom, 0), Some(ps[0]));
        assert_eq!(cached(&c), (1, true));
        // Reading on: each walk finds as many again as are known.
        let mut walks = 0;
        for (i, &p) in ps.iter().enumerate() {
            let before = cached(&c).0;
            assert_eq!(c.item(&dom, i), Some(p));
            walks += usize::from(cached(&c).0 != before);
        }
        assert_eq!(c.item(&dom, 40), None);
        assert_eq!(cached(&c), (40, false));
        assert!(walks <= 6, "{walks} walks for 40 reads in order");
    }

    #[test]
    fn indexed_reads_walk_only_as_far_as_their_index() {
        let mut dom = Dom::new();
        let doc = dom.create_document(snowlark_dom::DocumentKind::Html);
        let root = dom.create_element(doc, "div", None).unwrap();
        dom.append_child(doc, root).unwrap();
        let mut ps = Vec::new();
        for _ in 0..9 {
            let p = dom.create_element(doc, "p", None).unwrap();
            dom.append_child(root, p).unwrap();
            ps.push(p);
        }
        let c = Collection::live(
            root,
            Filter::TagName {
                qualified: "p".into(),
                lower: "p".into(),
            },
        );
        assert_eq!(c.item(&dom, 1), Some(ps[1]));
        assert_eq!(cached(&c), (2, true));
        // An attribute change leaves a tag name collection's members found so far, and its walk.
        dom.set_attribute(ps[0], "class", "x").unwrap();
        assert_eq!(c.item(&dom, 0), Some(ps[0]));
        assert_eq!(cached(&c), (2, true));
        // (the read past what is known walks ahead by as many again)
        assert_eq!(c.item(&dom, 2), Some(ps[2]));
        assert_eq!(cached(&c), (5, true));
        // A tree change starts it again.
        dom.remove_child(root, ps[0]).unwrap();
        assert_eq!(c.item(&dom, 0), Some(ps[1]));
        assert_eq!(cached(&c), (1, true));
        assert_eq!(c.with(&dom, |n| n.len()), 8);
        assert_eq!(cached(&c), (8, false));
        assert_eq!(c.item(&dom, 8), None);
    }

    #[test]
    fn attribute_changes_restart_collections_that_depend_on_attributes() {
        let mut dom = Dom::new();
        let doc = dom.create_document(snowlark_dom::DocumentKind::Html);
        let root = dom.create_element(doc, "div", None).unwrap();
        dom.append_child(doc, root).unwrap();
        let mut ps = Vec::new();
        for _ in 0..3 {
            let p = dom.create_element(doc, "p", None).unwrap();
            dom.set_attribute(p, "class", "k").unwrap();
            dom.append_child(root, p).unwrap();
            ps.push(p);
        }
        let c = Collection::live(root, Filter::ClassNames(vec!["k".into()]));
        assert_eq!(c.item(&dom, 1), Some(ps[1]));
        // Another attribute's change keeps what was found; a class change starts again.
        dom.set_attribute(ps[2], "style", "color: red").unwrap();
        assert_eq!(c.item(&dom, 0), Some(ps[0]));
        assert_eq!(cached(&c), (2, true));
        dom.set_attribute(ps[1], "class", "other").unwrap();
        assert_eq!(c.item(&dom, 0), Some(ps[0]));
        assert_eq!(cached(&c), (1, true));
        assert_eq!(c.item(&dom, 1), Some(ps[2]));
        assert_eq!(c.with(&dom, |n| n.to_vec()), vec![ps[0], ps[2]]);
    }
}
