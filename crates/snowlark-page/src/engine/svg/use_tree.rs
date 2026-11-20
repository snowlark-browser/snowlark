// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The shadow trees of `use` elements (SVG 2 §5.6): a `use` of an element of its own document holds a
//! closed shadow root with a clone of what it refers to, so that the instance has the style of its
//! own place: it matches the document's rules there (stylo's rule collection looks past a `use`
//! host for them), inherits from the `use`, and selectors see the shadow tree, not the original's
//! ancestors. The trees are built at the rendering update, after whatever changed what they
//! clone; a `use` of another document instead shows a copy from its resource document
//! (snowlark-render's `svg_resource`).

use rustc_hash::{FxHashMap, FxHashSet};
use snowlark_bindings::rt::Cx;
use snowlark_dom::{element_flags, ns, Dom, NodeId};
use snowlark_layout::svg::parse::percent_decoded;

use super::super::Engine;
use super::{is_svg, is_svg_element};

/// How deep `use` elements may nest through their shadow trees (a guard against cycles of
/// references, which §5.6 makes invalid).
const MAX_DEPTH: usize = 16;

#[derive(Default)]
pub(crate) struct UseTrees {
    /// The connected `use` elements, instances included.
    elements: FxHashSet<NodeId>,
    /// Those whose tree must follow what it clones.
    dirty: FxHashSet<NodeId>,
    /// Which elements are referred to, by whom.
    by_target: FxHashMap<NodeId, Vec<NodeId>>,
    /// Those whose reference found nothing, which a new element may satisfy.
    unresolved: FxHashSet<NodeId>,
    /// An `id` changed: any reference may now find something else.
    ids_changed: bool,
}

impl UseTrees {
    pub(crate) fn nodes_freed(&mut self, freed: &FxHashSet<NodeId>) {
        if self.elements.is_empty() && self.by_target.is_empty() {
            return;
        }
        self.elements.retain(|n| !freed.contains(n));
        self.dirty.retain(|n| !freed.contains(n));
        self.unresolved.retain(|n| !freed.contains(n));
        self.by_target.retain(|n, _| !freed.contains(n));
    }
}

/// The nearest tree that is not a `use` element's shadow tree, where its references resolve.
fn document_tree(dom: &Dom, n: NodeId) -> NodeId {
    let mut root = dom.root(n);
    while dom.is_shadow_root(root) {
        match dom.host(root) {
            Some(host) if is_svg(dom, host, "use") => root = dom.root(host),
            _ => break,
        }
    }
    root
}

/// Whether an instance `b` can stand for `a`.
fn same_kind(dom: &Dom, a: NodeId, b: NodeId) -> bool {
    match (dom.element(a), dom.element(b)) {
        (Some(x), Some(y)) => x.name() == y.name(),
        (None, None) => std::mem::discriminant(dom.data(a)) == std::mem::discriminant(dom.data(b)),
        _ => false,
    }
}

/// A new instance of `node`. Script in a shadow tree does not run (SVG 2 §5.6), and a
/// `foreignObject` is not instantiated (w3c/svgwg#876: what it holds would run and load).
fn instance(dom: &mut Dom, node: NodeId) -> Option<NodeId> {
    let clone = dom.clone_node(node, true).ok()?;
    let foreign: Vec<NodeId> = dom
        .inclusive_descendants(clone)
        .filter(|&d| is_svg(dom, d, "foreignObject"))
        .collect();
    if foreign.contains(&clone) {
        return None;
    }
    for f in foreign {
        dom.remove_node(f);
    }
    let scripts: Vec<NodeId> = dom
        .inclusive_descendants(clone)
        .filter(|&d| dom.is_script_element(d))
        .collect();
    for s in scripts {
        dom.set_element_flag(s, element_flags::SCRIPT_ALREADY_STARTED, true);
    }
    Some(clone)
}

/// Brings the instance `to` in line with `from`, keeping the elements that still correspond: they
/// are the ones with state of their own, a running CSS transition say, which a new clone would lose.
fn reconcile(dom: &mut Dom, from: NodeId, to: NodeId) {
    if let Some(e) = dom.element(from) {
        let wanted: Vec<_> = e
            .attrs()
            .iter()
            .map(|a| {
                let name = (a.local_name().clone(), a.namespace().clone());
                (name, a.prefix().cloned(), a.value().to_owned())
            })
            .collect();
        let extra: Vec<_> = dom.element(to).map_or(Vec::new(), |e| {
            e.attrs()
                .iter()
                .filter(|a| {
                    !wanted
                        .iter()
                        .any(|w| w.0 .0 == *a.local_name() && w.0 .1 == *a.namespace())
                })
                .map(|a| (a.namespace().to_string(), a.local_name().to_string()))
                .collect()
        });
        for (namespace, local) in extra {
            dom.remove_attribute_ns(
                to,
                Some(namespace.as_str()).filter(|n| !n.is_empty()),
                &local,
            );
        }
        for ((local, namespace), prefix, value) in wanted {
            if dom.attr(to, &namespace, &local) != Some(value.as_str()) {
                dom.set_attribute_value(to, local, namespace, prefix, &value);
            }
        }
    } else if let Some(data) = dom.character_data(from).map(str::to_owned) {
        if dom.character_data(to) != Some(data.as_str()) {
            dom.set_data(to, &data);
        }
    }
    let source: Vec<NodeId> = dom
        .children(from)
        .filter(|&c| !is_svg(dom, c, "foreignObject"))
        .collect();
    let have: Vec<NodeId> = dom.children(to).collect();
    let mut next = 0;
    for s in source {
        // (the next instance of its kind, what comes before that gone)
        let found = have[next..].iter().position(|&h| same_kind(dom, s, h));
        match found {
            Some(skip) => {
                for &h in &have[next..next + skip] {
                    dom.remove_node(h);
                }
                next += skip + 1;
                reconcile(dom, s, have[next - 1]);
            }
            None => {
                if let Some(clone) = instance(dom, s) {
                    let _ = dom.insert_before(to, clone, have.get(next).copied());
                }
            }
        }
    }
    for &h in &have[next..] {
        dom.remove_node(h);
    }
}

/// Whether `n` is in a `use` element's shadow tree.
pub(crate) fn in_use_tree(dom: &Dom, n: NodeId) -> bool {
    let root = dom.root(n);
    dom.is_shadow_root(root) && dom.host(root).is_some_and(|h| is_svg(dom, h, "use"))
}

impl Engine {
    /// `node` or something in it changed: what clones it must follow.
    fn use_touch(&self, node: NodeId) {
        let mut st = self.svg.borrow_mut();
        if st.uses.by_target.is_empty() {
            return;
        }
        let dom = self.dom();
        // (a parser's fragment is freed before the reactions of what left it run)
        if !dom.is_alive(node) {
            return;
        }
        let t = &mut st.uses;
        for a in dom.inclusive_ancestors(node) {
            if let Some(list) = t.by_target.get(&a) {
                t.dirty.extend(list.iter().copied());
            }
        }
    }

    /// An element was inserted (`connected`: into a document).
    pub(crate) fn use_inserted(&self, node: NodeId, connected: bool) {
        let is_use = connected && is_svg(&self.dom(), node, "use");
        self.use_inserted_as(node, is_use);
    }

    /// [`Engine::use_inserted`] for a node whose caller knows whether it is a `use` element
    /// inserted into a document.
    pub(crate) fn use_inserted_as(&self, node: NodeId, connected_use: bool) {
        if connected_use {
            let mut st = self.svg.borrow_mut();
            st.uses.elements.insert(node);
            st.uses.dirty.insert(node);
        }
        {
            let mut st = self.svg.borrow_mut();
            let t = &mut st.uses;
            if !t.unresolved.is_empty() {
                let waiting = std::mem::take(&mut t.unresolved);
                t.dirty.extend(waiting);
            }
        }
        self.use_touch(node);
    }

    /// An element was removed from `old_parent`.
    pub(crate) fn use_removed(&self, node: NodeId, old_parent: Option<NodeId>) {
        {
            let mut st = self.svg.borrow_mut();
            let t = &mut st.uses;
            t.elements.remove(&node);
            t.dirty.remove(&node);
            t.unresolved.remove(&node);
            if let Some(list) = t.by_target.remove(&node) {
                t.dirty.extend(list);
            }
        }
        if let Some(p) = old_parent {
            self.use_touch(p);
        }
    }

    /// An attribute of `el` changed.
    pub(crate) fn use_attribute_changed(&self, el: NodeId, local: &str) {
        {
            let mut st = self.svg.borrow_mut();
            let t = &mut st.uses;
            if t.elements.is_empty() {
                return;
            }
            if local == "id" {
                t.ids_changed = true;
            } else if local == "href" && t.elements.contains(&el) {
                t.dirty.insert(el);
            }
        }
        self.use_touch(el);
    }

    /// The children of `parent` changed.
    pub(crate) fn use_children_changed(&self, parent: NodeId) {
        self.use_touch(parent);
    }

    /// Whether a rendering update has trees to build.
    pub(crate) fn use_trees_dirty(&self) -> bool {
        let st = self.svg.borrow();
        !st.uses.dirty.is_empty() || st.uses.ids_changed
    }

    /// After a change of `id`: the `use` elements whose reference finds something else now.
    fn use_retarget(&self) {
        let (elements, known) = {
            let mut st = self.svg.borrow_mut();
            let t = &mut st.uses;
            if !std::mem::take(&mut t.ids_changed) {
                return;
            }
            let mut known = FxHashMap::default();
            for (&target, list) in &t.by_target {
                known.extend(list.iter().map(|&u| (u, target)));
            }
            (t.elements.iter().copied().collect::<Vec<_>>(), known)
        };
        let mut moved = Vec::new();
        {
            let dom = self.dom();
            let mut ids = super::smil::Ids::default();
            for u in elements {
                if dom.is_alive(u)
                    && dom.is_connected(u)
                    && self.use_target(&dom, u, &mut ids) != known.get(&u).copied()
                {
                    moved.push(u);
                }
            }
        }
        self.svg.borrow_mut().uses.dirty.extend(moved);
    }

    /// What a `use` element refers to in its own document (`None`: another document, or nothing).
    fn use_target(&self, dom: &Dom, u: NodeId, ids: &mut super::smil::Ids) -> Option<NodeId> {
        let href = dom
            .attr(u, &ns!(), &"href".into())
            .or_else(|| dom.attr(u, &ns!(xlink), &"href".into()))?
            .trim();
        let fragment = match href.strip_prefix('#') {
            Some(f) => percent_decoded(f),
            None => {
                let url = self.base_url_of(u).join(href).ok()?;
                let mut there = url.clone();
                there.set_fragment(None);
                let doc = url::Url::parse(dom.document_url(dom.node_document(u))).ok();
                let mut here = doc?;
                here.set_fragment(None);
                if there != here {
                    return None;
                }
                percent_decoded(url.fragment()?)
            }
        };
        let target = ids.get(dom, document_tree(dom, u), &fragment)?;
        // SVG 2 §5.6: not an SVG element, or an ancestor through the shadow trees: in error.
        if !is_svg_element(dom, target) {
            return None;
        }
        let mut cur = Some(u);
        let mut depth = 0;
        while let Some(n) = cur {
            if n == target || depth > MAX_DEPTH {
                return None;
            }
            let parent = dom.parent(n);
            cur = match parent {
                Some(p) if dom.is_shadow_root(p) => {
                    depth += 1;
                    dom.host(p)
                }
                p => p,
            };
        }
        Some(target)
    }

    /// Builds the trees that changed. What they hold may hold `use` elements in turn.
    pub(crate) fn svg_use_sync(&self, cx: &mut Cx) {
        self.use_retarget();
        for _ in 0..=MAX_DEPTH {
            let dirty = std::mem::take(&mut self.svg.borrow_mut().uses.dirty);
            if dirty.is_empty() {
                return;
            }
            let mut ids = super::smil::Ids::default();
            let mut targets = Vec::with_capacity(dirty.len());
            {
                let dom = self.dom();
                let mut list: Vec<NodeId> = dirty
                    .into_iter()
                    .filter(|&u| dom.is_alive(u) && dom.is_connected(u))
                    .collect();
                list.sort_by_key(|n| n.to_bits());
                for u in list {
                    targets.push((u, self.use_target(&dom, u, &mut ids)));
                }
            }
            for (u, target) in targets {
                self.use_build(u, target);
            }
            self.process_reactions(cx);
        }
        // (nesting beyond the guard: left as it is)
        self.svg.borrow_mut().uses.dirty.clear();
    }

    /// Makes `u`'s tree an instance of `target`.
    fn use_build(&self, u: NodeId, target: Option<NodeId>) {
        let previous = {
            let mut st = self.svg.borrow_mut();
            let t = &mut st.uses;
            let previous = t
                .by_target
                .iter()
                .find(|(_, list)| list.contains(&u))
                .map(|(&n, _)| n);
            t.by_target.retain(|_, list| {
                list.retain(|&x| x != u);
                !list.is_empty()
            });
            t.unresolved.remove(&u);
            match target {
                Some(n) => t.by_target.entry(n).or_default().push(u),
                None => {
                    t.unresolved.insert(u);
                }
            }
            previous
        };
        let mut dom = self.dom_mut();
        let shadow = dom.shadow_root(u);
        if shadow.is_none() && target.is_none() {
            return;
        }
        let shadow = dom.use_shadow_root(u);
        let old: Vec<NodeId> = dom.children(shadow).collect();
        if let (Some(n), Some(&have)) = (target, old.first()) {
            if previous == target && same_kind(&dom, n, have) {
                reconcile(&mut dom, n, have);
                return;
            }
        }
        for c in old {
            dom.remove_node(c);
        }
        let Some(clone) = target.and_then(|n| instance(&mut dom, n)) else {
            return;
        };
        let _ = dom.append_child(shadow, clone);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_dom::ParseOptions;

    const SVG: &str = "http://www.w3.org/2000/svg";

    /// A document with `markup` in an `svg`, and the `svg`'s first child, with an instance of it.
    fn tree(markup: &str) -> (Dom, NodeId, NodeId) {
        let mut dom = Dom::new();
        let doc = dom.parse_html_str(&format!("<svg>{markup}</svg>"), ParseOptions::default());
        let target = dom
            .inclusive_descendants(doc)
            .find(|&n| dom.is_element(n) && dom.attr(n, &ns!(), &"id".into()) == Some("t"))
            .unwrap();
        let clone = instance(&mut dom, target).unwrap();
        (dom, target, clone)
    }

    fn find(dom: &Dom, root: NodeId, name: &str) -> NodeId {
        dom.inclusive_descendants(root)
            .find(|&n| is_svg(dom, n, name))
            .unwrap()
    }

    #[test]
    fn reconciling_keeps_what_still_corresponds() {
        let (mut dom, target, clone) = tree("<g id=t><rect/><text>a</text><circle/></g>");
        let (rect, circle) = (find(&dom, clone, "rect"), find(&dom, clone, "circle"));
        // (an attribute, text, a child gone from the middle, a new one, and one that is no instance)
        let source = find(&dom, target, "rect");
        dom.set_attribute(source, "class", "x").unwrap();
        let text = find(&dom, target, "text");
        dom.remove_node(text);
        let doc = dom.node_document(target);
        let line = dom.create_element_ns(doc, Some(SVG), "line", None).unwrap();
        dom.append_child(target, line).unwrap();
        let foreign = dom
            .create_element_ns(doc, Some(SVG), "foreignObject", None)
            .unwrap();
        dom.append_child(target, foreign).unwrap();
        reconcile(&mut dom, target, clone);
        assert_eq!(find(&dom, clone, "rect"), rect);
        assert_eq!(find(&dom, clone, "circle"), circle);
        assert_eq!(dom.attr(rect, &ns!(), &"class".into()), Some("x"));
        let names: Vec<_> = dom
            .children(clone)
            .map(|c| dom.local_name(c).to_string())
            .collect();
        assert_eq!(names, ["rect", "circle", "line"]);
        // (and an attribute gone from the original goes from the instance)
        dom.remove_attribute(source, "class");
        reconcile(&mut dom, target, clone);
        assert_eq!(dom.attr(rect, &ns!(), &"class".into()), None);
        assert!(dom.inner_html(clone).contains("<line"));
    }

    #[test]
    fn instances_hold_no_foreign_object_and_run_no_script() {
        let (dom, _, clone) = tree("<g id=t><foreignObject></foreignObject><script>x</script></g>");
        assert!(!dom
            .inclusive_descendants(clone)
            .any(|n| is_svg(&dom, n, "foreignObject")));
        let script = find(&dom, clone, "script");
        assert_ne!(
            dom.element_flags(script) & element_flags::SCRIPT_ALREADY_STARTED,
            0
        );
    }
}
