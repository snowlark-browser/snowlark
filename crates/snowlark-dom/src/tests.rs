// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unit tests: one per spec step that can throw, plus the behaviour around them.

use std::cell::RefCell;

use crate::*;

fn html_doc() -> (Dom, NodeId) {
    let mut dom = Dom::new();
    let doc = dom.create_document(DocumentKind::Html);
    (dom, doc)
}

fn el(dom: &mut Dom, doc: NodeId, name: &str) -> NodeId {
    dom.create_element(doc, name, None).unwrap()
}

fn names(dom: &Dom, parent: NodeId) -> Vec<String> {
    dom.children(parent)
        .map(|c| dom.node_name(c).into_owned())
        .collect()
}

// ----- arena ------------------------------------------------------------------------------------------------

#[test]
fn dom_is_sync_and_send() {
    fn assert_sync<T: Sync + Send>() {}
    assert_sync::<Dom>();
}

#[test]
fn node_slot_size() {
    // One arena slot per node: links, flags, and the element data with two inline attributes
    // (values up to 22 bytes inline).
    let size = std::mem::size_of::<Node>();
    println!(
        "Node: {size} bytes, Attr: {} bytes",
        std::mem::size_of::<Attr>()
    );
    assert!(size <= 232, "Node grew to {size} bytes");
}

#[test]
fn freed_slots_are_reused_with_a_new_generation() {
    let (mut dom, doc) = html_doc();
    let a = dom.create_text_node(doc, "a");
    dom.free_subtree(a);
    assert!(!dom.is_alive(a));
    let b = dom.create_text_node(doc, "b");
    assert_eq!(a.index(), b.index());
    assert_ne!(a.generation(), b.generation());
    assert!(dom.is_alive(b));
    assert_eq!(NodeId::from_bits(b.to_bits()), Some(b));
}

/// Freed slots are reused lowest first, so a tree made after many frees in any order sits in adjacent
/// slots, as one made into a fresh arena does, rather than in the order the slots were freed.
#[test]
fn a_tree_made_into_freed_slots_is_contiguous() {
    let (mut dom, doc) = html_doc();
    let base = dom.tree_scatter();
    let nodes: Vec<NodeId> = (0..2000)
        .map(|i| dom.create_text_node(doc, &i.to_string()))
        .collect();
    // Freed in a scrambled order (a multiplicative step through the indices).
    for k in 0..nodes.len() {
        dom.free_subtree(nodes[(k * 769) % nodes.len()]);
    }
    let parent = dom.create_text_node(doc, "p");
    dom.free_subtree(parent);
    let list = dom.create_document_fragment(doc);
    for i in 0..2000 {
        let t = dom.create_text_node(doc, &i.to_string());
        dom.append_child(list, t).unwrap();
    }
    let scatter = dom.tree_scatter();
    assert!(
        scatter < base.max(1.0) + 1.0,
        "neighbours {scatter} slots apart"
    );
}

#[test]
#[should_panic(expected = "stale NodeId")]
fn stale_ids_panic() {
    let (mut dom, doc) = html_doc();
    let a = dom.create_text_node(doc, "a");
    dom.free_subtree(a);
    dom.create_text_node(doc, "b");
    dom.node_type(a);
}

#[test]
fn free_subtree_frees_descendants_and_template_contents() {
    let (mut dom, doc) = html_doc();
    let before = dom.node_count();
    let div = el(&mut dom, doc, "div");
    let t = el(&mut dom, doc, "template");
    dom.append_child(div, t).unwrap();
    let contents = dom.template_contents(t).unwrap();
    let text = dom.create_text_node(doc, "x");
    dom.append_child(contents, text).unwrap();
    dom.free_subtree(div);
    assert!(!dom.is_alive(text));
    assert!(!dom.is_alive(contents));
    // The template contents owner document stays with the document.
    assert_eq!(dom.node_count(), before + 1);
}

#[test]
fn per_document_node_counts_follow_creation_adoption_and_freeing() {
    let (mut dom, doc) = html_doc();
    let (_, other) = {
        let o = dom.create_document(crate::DocumentKind::Html);
        (0, o)
    };
    let walk = |dom: &Dom, d: NodeId| {
        dom.nodes
            .iter()
            .filter(|n| !matches!(n.data, NodeData::Free { .. }) && n.document == Some(d))
            .count() as u32
    };
    let check = |dom: &Dom| {
        for d in [doc, other] {
            assert_eq!(dom.doc_nodes.get(&d).copied().unwrap_or(0), walk(dom, d));
        }
    };
    let html = el(&mut dom, doc, "html");
    dom.append_child(doc, html).unwrap();
    let div = el(&mut dom, doc, "div");
    let span = el(&mut dom, doc, "span");
    dom.append_child(div, span).unwrap();
    dom.set_attribute(div, "id", "x").unwrap();
    check(&dom);
    // Adopted into another document, with its descendants.
    dom.append_child(other, div).unwrap();
    check(&dom);
    dom.remove_child(other, div).unwrap();
    check(&dom);
    dom.free_subtree(div);
    check(&dom);
    assert!(dom.documents_owned_within(&dom.owned_nodes(other)));
    let loose = el(&mut dom, doc, "p");
    assert!(!dom.documents_owned_within(&dom.owned_nodes(doc)));
    dom.free_subtree(loose);
    assert!(dom.documents_owned_within(&dom.owned_nodes(doc)));
    check(&dom);
}

#[test]
fn a_documents_detached_roots_are_its_nodes_outside_its_tree() {
    let (mut dom, doc) = html_doc();
    let html = el(&mut dom, doc, "html");
    dom.append_child(doc, html).unwrap();
    let ul = el(&mut dom, doc, "ul");
    let li = el(&mut dom, doc, "li");
    dom.append_child(ul, li).unwrap();
    dom.append_child(html, ul).unwrap();
    assert!(dom.detached_roots_of(doc).is_empty());
    // A removed subtree is one root, however many nodes it has; a node never inserted is another.
    dom.remove_child(html, ul).unwrap();
    let loose = el(&mut dom, doc, "p");
    let mut roots = dom.detached_roots_of(doc);
    roots.sort();
    let mut want = vec![ul, loose];
    want.sort();
    assert_eq!(roots, want);
    dom.free_subtree(ul);
    dom.free_subtree(loose);
    assert!(dom.detached_roots_of(doc).is_empty());
}

#[test]
fn a_document_is_owned_within_its_tree_only_without_outside_nodes() {
    let (mut dom, doc) = html_doc();
    let html = el(&mut dom, doc, "html");
    dom.append_child(doc, html).unwrap();
    let t = el(&mut dom, doc, "template");
    dom.append_child(html, t).unwrap();
    // A node of the document outside its tree keeps it from being freed with the tree.
    let loose = el(&mut dom, doc, "div");
    let owned = dom.owned_nodes(doc);
    assert!(!dom.documents_owned_within(&owned));
    dom.free_subtree(loose);
    assert!(dom.documents_owned_within(&owned));
    // A subtree without a Document needs no walk.
    let other = el(&mut dom, doc, "p");
    assert!(dom.documents_owned_within(&dom.owned_nodes(other)));
    dom.free_subtree(other);
    dom.free_owned(doc, &owned);
    assert_eq!(dom.node_count(), 0);
}

#[test]
fn the_store_knows_whether_it_ever_held_a_shadow_root() {
    let (mut dom, doc) = html_doc();
    let host = el(&mut dom, doc, "div");
    assert!(!dom.has_shadow_roots());
    dom.attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    assert!(dom.has_shadow_roots());
}

#[test]
fn opaque_roots_cross_hosts_and_attribute_owners() {
    let (mut dom, doc) = html_doc();
    let root = el(&mut dom, doc, "div");
    let host = el(&mut dom, doc, "section");
    dom.append_child(root, host).unwrap();
    let sr = dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    let inner = el(&mut dom, doc, "span");
    dom.append_child(sr, inner).unwrap();
    let t = el(&mut dom, doc, "template");
    dom.append_child(inner, t).unwrap();
    let contents = dom.template_contents(t).unwrap();
    let text = dom.create_text_node(doc, "x");
    dom.append_child(contents, text).unwrap();
    let attr = dom.create_attribute(doc, "a").unwrap();
    dom.set_attribute_node(inner, attr).unwrap();
    for n in [root, host, sr, inner, t, contents, text, attr] {
        assert_eq!(dom.opaque_root(n), root);
    }
    // A connected node's opaque root is its document; a removed attribute is its own.
    let detached_attr = dom.create_attribute(doc, "b").unwrap();
    assert_eq!(dom.opaque_root(detached_attr), detached_attr);
    let owned = dom.owned_nodes(root);
    for n in [root, host, sr, inner, t, contents, text, attr] {
        assert!(owned.contains(&n));
    }
    assert_eq!(owned.len(), 8, "each owned node once");
    dom.append_child(doc, root).unwrap();
    assert_eq!(dom.opaque_root(text), doc);
    // Connected now: every node of the tree, its shadow tree and the document itself.
    for n in [doc, root, host, sr, inner, t] {
        assert!(dom.is_connected(n) || n == doc);
        assert_eq!(dom.opaque_root(n), doc);
    }
    // Template contents and attributes are not connected; they reach the document through their
    // host and owner.
    assert_eq!(dom.opaque_root(contents), doc);
    assert_eq!(dom.opaque_root(attr), doc);
}

// ----- pre-insert validity (DOM §4.2.3 "ensure pre-insert validity") --------------------------------------

#[test]
fn pre_insert_step1_parent_must_be_a_parent_kind() {
    let (mut dom, doc) = html_doc();
    let text = dom.create_text_node(doc, "t");
    let child = el(&mut dom, doc, "b");
    assert_eq!(
        dom.append_child(text, child),
        Err(DomError::HierarchyRequest)
    );
    let dt = dom.create_document_type(doc, "html", "", "").unwrap();
    assert_eq!(dom.append_child(dt, child), Err(DomError::HierarchyRequest));
}

#[test]
fn pre_insert_step2_no_cycles_including_through_hosts() {
    let (mut dom, doc) = html_doc();
    let a = el(&mut dom, doc, "div");
    let b = el(&mut dom, doc, "div");
    dom.append_child(a, b).unwrap();
    assert_eq!(dom.append_child(b, a), Err(DomError::HierarchyRequest));
    assert_eq!(dom.append_child(a, a), Err(DomError::HierarchyRequest));
    // A template can't go into its own contents (host-including).
    let t = el(&mut dom, doc, "template");
    let contents = dom.template_contents(t).unwrap();
    assert_eq!(
        dom.append_child(contents, t),
        Err(DomError::HierarchyRequest)
    );
    // Nor a host into its shadow root.
    let shadow = dom
        .attach_shadow(a, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    assert_eq!(dom.append_child(shadow, a), Err(DomError::HierarchyRequest));
}

#[test]
fn pre_insert_step3_child_must_be_a_child() {
    let (mut dom, doc) = html_doc();
    let a = el(&mut dom, doc, "div");
    let b = el(&mut dom, doc, "div");
    let c = el(&mut dom, doc, "div");
    assert_eq!(dom.insert_before(a, b, Some(c)), Err(DomError::NotFound));
}

#[test]
fn pre_insert_step4_node_kind() {
    let (mut dom, doc) = html_doc();
    let a = el(&mut dom, doc, "div");
    let other = dom.create_document(DocumentKind::Html);
    assert_eq!(dom.append_child(a, other), Err(DomError::HierarchyRequest));
    let attr = dom.create_attribute(doc, "x").unwrap();
    assert_eq!(dom.append_child(a, attr), Err(DomError::HierarchyRequest));
}

#[test]
fn pre_insert_step5_text_in_document_and_doctype_outside() {
    let (mut dom, doc) = html_doc();
    let text = dom.create_text_node(doc, "t");
    assert_eq!(dom.append_child(doc, text), Err(DomError::HierarchyRequest));
    let dt = dom.create_document_type(doc, "html", "", "").unwrap();
    let div = el(&mut dom, doc, "div");
    assert_eq!(dom.append_child(div, dt), Err(DomError::HierarchyRequest));
    // Comments are fine in a document.
    let c = dom.create_comment(doc, "c");
    assert!(dom.append_child(doc, c).is_ok());
}

#[test]
fn pre_insert_step6_document_constraints() {
    let (mut dom, doc) = html_doc();
    let html = el(&mut dom, doc, "html");
    dom.append_child(doc, html).unwrap();
    // A second element.
    let second = el(&mut dom, doc, "html");
    assert_eq!(
        dom.append_child(doc, second),
        Err(DomError::HierarchyRequest)
    );
    // A fragment with two elements, or with text.
    let frag = dom.create_document_fragment(doc);
    let (x, y) = (el(&mut dom, doc, "a"), el(&mut dom, doc, "b"));
    dom.append_child(frag, x).unwrap();
    dom.append_child(frag, y).unwrap();
    assert_eq!(dom.append_child(doc, frag), Err(DomError::HierarchyRequest));
    let frag2 = dom.create_document_fragment(doc);
    let t = dom.create_text_node(doc, "t");
    dom.append_child(frag2, t).unwrap();
    assert_eq!(
        dom.append_child(doc, frag2),
        Err(DomError::HierarchyRequest)
    );
    // A fragment with one element when the document already has one.
    let frag3 = dom.create_document_fragment(doc);
    let z = el(&mut dom, doc, "c");
    dom.append_child(frag3, z).unwrap();
    assert_eq!(
        dom.append_child(doc, frag3),
        Err(DomError::HierarchyRequest)
    );
    // A doctype after the element, or a second doctype.
    let dt = dom.create_document_type(doc, "html", "", "").unwrap();
    assert_eq!(dom.append_child(doc, dt), Err(DomError::HierarchyRequest));
    dom.insert_before(doc, dt, Some(html)).unwrap();
    let dt2 = dom.create_document_type(doc, "html", "", "").unwrap();
    assert_eq!(
        dom.insert_before(doc, dt2, Some(dt)),
        Err(DomError::HierarchyRequest)
    );
    // An element before the doctype.
    dom.remove_child(doc, html).unwrap();
    assert_eq!(
        dom.insert_before(doc, html, Some(dt)),
        Err(DomError::HierarchyRequest)
    );
    assert!(dom.append_child(doc, html).is_ok());
}

#[test]
fn insert_before_itself_uses_the_next_sibling() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "div");
    let a = el(&mut dom, doc, "a");
    let b = el(&mut dom, doc, "b");
    dom.append_child(p, a).unwrap();
    dom.append_child(p, b).unwrap();
    dom.insert_before(p, a, Some(a)).unwrap();
    assert_eq!(names(&dom, p), ["A", "B"]);
}

#[test]
fn inserting_a_fragment_moves_its_children() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "div");
    let frag = dom.create_document_fragment(doc);
    for n in ["a", "b", "c"] {
        let e = el(&mut dom, doc, n);
        dom.append_child(frag, e).unwrap();
    }
    dom.append_child(p, frag).unwrap();
    assert_eq!(names(&dom, p), ["A", "B", "C"]);
    assert!(!dom.has_child_nodes(frag));
}

// ----- replace (DOM §4.2.3 "replace") ----------------------------------------------------------------------

#[test]
fn replace_child_checks() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "div");
    let a = el(&mut dom, doc, "a");
    let b = el(&mut dom, doc, "b");
    dom.append_child(p, a).unwrap();
    // 1.
    let text = dom.create_text_node(doc, "t");
    assert_eq!(
        dom.replace_child(text, b, a),
        Err(DomError::HierarchyRequest)
    );
    // 2.
    assert_eq!(dom.replace_child(a, p, a), Err(DomError::HierarchyRequest));
    // 3.
    assert_eq!(dom.replace_child(p, a, b), Err(DomError::NotFound));
    // 4.
    let other = dom.create_document(DocumentKind::Html);
    assert_eq!(
        dom.replace_child(p, other, a),
        Err(DomError::HierarchyRequest)
    );
    // 5.
    let html = el(&mut dom, doc, "html");
    dom.append_child(doc, html).unwrap();
    assert_eq!(
        dom.replace_child(doc, text, html),
        Err(DomError::HierarchyRequest)
    );
    let dt = dom.create_document_type(doc, "html", "", "").unwrap();
    assert_eq!(dom.replace_child(p, dt, a), Err(DomError::HierarchyRequest));
    // 6. Replacing the document element with an element is fine; adding a second one is not.
    let html2 = el(&mut dom, doc, "html");
    assert_eq!(dom.replace_child(doc, html2, html), Ok(html));
    let c = dom.create_comment(doc, "c");
    dom.append_child(doc, c).unwrap();
    assert_eq!(
        dom.replace_child(doc, html, c),
        Err(DomError::HierarchyRequest)
    );
    // A doctype may not be put after the element.
    assert_eq!(
        dom.replace_child(doc, dt, c),
        Err(DomError::HierarchyRequest)
    );
    // Success.
    dom.replace_child(p, b, a).unwrap();
    assert_eq!(names(&dom, p), ["B"]);
    assert_eq!(dom.parent(a), None);
}

#[test]
fn replace_child_with_itself_is_a_no_op() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "div");
    let a = el(&mut dom, doc, "a");
    let b = el(&mut dom, doc, "b");
    dom.append_child(p, a).unwrap();
    dom.append_child(p, b).unwrap();
    dom.replace_child(p, a, a).unwrap();
    assert_eq!(names(&dom, p), ["A", "B"]);
}

#[test]
fn remove_child_not_found() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "div");
    let a = el(&mut dom, doc, "a");
    assert_eq!(dom.remove_child(p, a), Err(DomError::NotFound));
}

#[test]
fn replace_all_and_text_content() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "div");
    for n in ["a", "b"] {
        let e = el(&mut dom, doc, n);
        dom.append_child(p, e).unwrap();
    }
    dom.set_text_content(p, "hello");
    assert_eq!(dom.child_count(p), 1);
    assert_eq!(dom.text_content(p).as_deref(), Some("hello"));
    dom.set_text_content(p, "");
    assert_eq!(dom.child_count(p), 0);
    assert_eq!(dom.text_content(doc), None);
}

// ----- ParentNode / ChildNode -------------------------------------------------------------------------------

#[test]
fn parent_node_and_child_node_methods() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "div");
    let a = el(&mut dom, doc, "a");
    dom.append(
        p,
        vec![NodeOrString::Node(a), NodeOrString::String("t".into())],
    )
    .unwrap();
    assert_eq!(names(&dom, p), ["A", "#text"]);
    let b = el(&mut dom, doc, "b");
    dom.prepend(p, vec![NodeOrString::Node(b)]).unwrap();
    assert_eq!(names(&dom, p), ["B", "A", "#text"]);
    let c = el(&mut dom, doc, "c");
    dom.before(a, vec![NodeOrString::Node(c), NodeOrString::Node(a)])
        .unwrap();
    assert_eq!(names(&dom, p), ["B", "C", "A", "#text"]);
    let d = el(&mut dom, doc, "d");
    dom.after(b, vec![NodeOrString::Node(d)]).unwrap();
    assert_eq!(names(&dom, p), ["B", "D", "C", "A", "#text"]);
    let e = el(&mut dom, doc, "e");
    dom.replace_with(c, vec![NodeOrString::Node(e), NodeOrString::Node(c)])
        .unwrap();
    assert_eq!(names(&dom, p), ["B", "D", "E", "C", "A", "#text"]);
    dom.replace_children(p, vec![NodeOrString::String("x".into())])
        .unwrap();
    assert_eq!(dom.text_content(p).as_deref(), Some("x"));
    dom.remove_node(p); // no parent: nothing happens
    let text = dom.first_child(p).unwrap();
    dom.remove_node(text);
    assert_eq!(dom.child_count(p), 0);
    // Errors propagate.
    assert_eq!(
        dom.append(p, vec![NodeOrString::Node(p)]),
        Err(DomError::HierarchyRequest)
    );
}

// ----- adoption ---------------------------------------------------------------------------------------------

#[test]
fn adopt_node_checks_and_moves_documents() {
    let (mut dom, doc) = html_doc();
    let other = dom.create_document(DocumentKind::Html);
    assert_eq!(dom.adopt_node(doc, other), Err(DomError::NotSupported));
    let host = el(&mut dom, doc, "div");
    let shadow = dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    assert_eq!(
        dom.adopt_node(other, shadow),
        Err(DomError::HierarchyRequest)
    );
    // Template contents (a fragment with a host) are left alone.
    let t = el(&mut dom, doc, "template");
    let contents = dom.template_contents(t).unwrap();
    let contents_doc = dom.node_document(contents);
    assert_eq!(dom.adopt_node(other, contents), Ok(contents));
    assert_eq!(dom.node_document(contents), contents_doc);
    // An element moves with its subtree, shadow tree, attributes and template contents.
    dom.append_child(host, t).unwrap();
    dom.set_attribute(host, "id", "x").unwrap();
    let attr = dom.get_attribute_node(host, "id").unwrap();
    let inner = dom.create_text_node(doc, "in shadow");
    dom.append_child(shadow, inner).unwrap();
    dom.adopt_node(other, host).unwrap();
    for n in [host, t, shadow, inner, attr] {
        assert_eq!(dom.node_document(n), other);
    }
    let other_owner = dom.template_contents_owner_document(other);
    assert_eq!(dom.node_document(contents), other_owner);
}

#[test]
fn appending_across_documents_adopts() {
    let (mut dom, doc) = html_doc();
    let other = dom.create_html_document(Some("t"));
    let body = dom.body(other).unwrap();
    let div = el(&mut dom, doc, "div");
    dom.append_child(body, div).unwrap();
    assert_eq!(dom.node_document(div), other);
    assert!(dom.is_connected(div));
}

// ----- connectedness ----------------------------------------------------------------------------------------

#[test]
fn is_connected_follows_insertion_and_shadow_roots() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let body = dom.body(doc).unwrap();
    let host = el(&mut dom, doc, "div");
    let shadow = dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Closed))
        .unwrap();
    let inner = el(&mut dom, doc, "span");
    dom.append_child(shadow, inner).unwrap();
    assert!(!dom.is_connected(inner));
    dom.append_child(body, host).unwrap();
    assert!(dom.is_connected(host) && dom.is_connected(shadow) && dom.is_connected(inner));
    assert_eq!(dom.get_root_node(inner, false), shadow);
    assert_eq!(dom.get_root_node(inner, true), doc);
    dom.remove_node(host);
    assert!(!dom.is_connected(inner));
    assert_eq!(dom.open_shadow_root(host), None);
}

// ----- shadow roots -----------------------------------------------------------------------------------------

#[test]
fn attach_shadow_checks() {
    let (mut dom, doc) = html_doc();
    let svg = dom
        .create_element_ns(doc, Some("http://www.w3.org/2000/svg"), "svg", None)
        .unwrap();
    let init = ShadowRootInit::new(ShadowRootMode::Open);
    assert_eq!(dom.attach_shadow(svg, init), Err(DomError::NotSupported));
    let img = el(&mut dom, doc, "img");
    assert_eq!(dom.attach_shadow(img, init), Err(DomError::NotSupported));
    let custom = el(&mut dom, doc, "my-el");
    assert!(dom.attach_shadow(custom, init).is_ok());
    assert_eq!(dom.attach_shadow(custom, init), Err(DomError::NotSupported));
}

#[test]
fn declarative_shadow_root_is_reused_by_attach_shadow() {
    let mut dom = Dom::new();
    let doc = dom.parse_html_str(
        "<div id=h><template shadowrootmode=open><b>x</b></template></div>",
        ParseOptions::default(),
    );
    let body = dom.body(doc).unwrap();
    let host = dom.first_child(body).unwrap();
    let shadow = dom.shadow_root(host).expect("declarative shadow root");
    assert!(dom.shadow_root_data(shadow).unwrap().declarative());
    assert_eq!(dom.inner_html(shadow), "<b>x</b>");
    assert_eq!(dom.child_count(host), 0);
    // Wrong mode fails; same mode empties and reuses it.
    assert_eq!(
        dom.attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Closed)),
        Err(DomError::NotSupported)
    );
    assert_eq!(
        dom.attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open)),
        Ok(shadow)
    );
    assert_eq!(dom.child_count(shadow), 0);
    assert!(!dom.shadow_root_data(shadow).unwrap().declarative());
}

#[test]
fn inner_html_does_not_create_declarative_shadow_roots_but_set_html_unsafe_does() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let div = el(&mut dom, doc, "div");
    let markup = "<p><template shadowrootmode=open>s</template></p>";
    dom.set_inner_html(div, markup).unwrap();
    let p = dom.first_child(div).unwrap();
    assert_eq!(dom.shadow_root(p), None);
    dom.set_html_unsafe(div, markup).unwrap();
    let p = dom.first_child(div).unwrap();
    assert!(dom.shadow_root(p).is_some());
}

// ----- attributes -------------------------------------------------------------------------------------------

#[test]
fn set_attribute_validates_and_lowercases() {
    let (mut dom, doc) = html_doc();
    let e = el(&mut dom, doc, "div");
    assert_eq!(
        dom.set_attribute(e, "a b", "1"),
        Err(DomError::InvalidCharacter)
    );
    assert_eq!(
        dom.set_attribute(e, "", "1"),
        Err(DomError::InvalidCharacter)
    );
    dom.set_attribute(e, "DATA-X", "1").unwrap();
    assert_eq!(dom.get_attribute(e, "data-x"), Some("1"));
    assert_eq!(dom.get_attribute(e, "Data-X"), Some("1"));
    dom.set_attribute(e, "data-x", "2").unwrap();
    assert_eq!(dom.attrs(e).len(), 1);
    assert_eq!(dom.attr(e, &ns!(), &LocalName::from("data-x")), Some("2"));
    dom.remove_attribute(e, "DATA-X");
    assert!(!dom.has_attributes(e));
}

#[test]
fn set_attribute_ns_validates() {
    let (mut dom, doc) = html_doc();
    let e = el(&mut dom, doc, "div");
    assert_eq!(
        dom.set_attribute_ns(e, None, "p:x", "1"),
        Err(DomError::Namespace)
    );
    assert_eq!(
        dom.set_attribute_ns(e, Some("urn:a"), "xmlns", "1"),
        Err(DomError::Namespace)
    );
    assert_eq!(
        dom.set_attribute_ns(e, Some("urn:a"), "a>b", "1"),
        Err(DomError::InvalidCharacter)
    );
    dom.set_attribute_ns(e, Some("urn:a"), "p:x", "1").unwrap();
    dom.set_attribute_ns(e, Some("urn:a"), "q:x", "2").unwrap();
    assert_eq!(dom.attrs(e).len(), 1);
    assert_eq!(dom.attrs(e)[0].qualified_name(), "p:x");
    assert_eq!(dom.get_attribute_ns(e, Some("urn:a"), "x"), Some("2"));
    assert_eq!(dom.get_attribute(e, "p:x"), Some("2"));
    dom.remove_attribute_ns(e, Some("urn:a"), "x");
    assert!(!dom.has_attribute_ns(e, Some("urn:a"), "x"));
}

#[test]
fn toggle_attribute() {
    let (mut dom, doc) = html_doc();
    let e = el(&mut dom, doc, "div");
    assert_eq!(
        dom.toggle_attribute(e, "=", None),
        Err(DomError::InvalidCharacter)
    );
    assert_eq!(dom.toggle_attribute(e, "Hidden", None), Ok(true));
    assert!(dom.has_attribute(e, "hidden"));
    assert_eq!(dom.toggle_attribute(e, "hidden", Some(true)), Ok(true));
    assert_eq!(dom.toggle_attribute(e, "hidden", None), Ok(false));
    assert_eq!(dom.toggle_attribute(e, "hidden", Some(false)), Ok(false));
    assert!(!dom.has_attribute(e, "hidden"));
}

#[test]
fn attr_nodes_keep_identity_and_track_their_element() {
    let (mut dom, doc) = html_doc();
    let e = el(&mut dom, doc, "div");
    let f = el(&mut dom, doc, "div");
    dom.set_attribute(e, "title", "a").unwrap();
    let attr = dom.get_attribute_node(e, "title").unwrap();
    assert_eq!(dom.get_attribute_node(e, "title"), Some(attr));
    assert_eq!(dom.attr_owner_element(attr), Some(e));
    dom.set_attr_value(attr, "b");
    assert_eq!(dom.get_attribute(e, "title"), Some("b"));
    assert_eq!(dom.node_name(attr), "title");
    // InUseAttributeError.
    assert_eq!(
        dom.set_attribute_node(f, attr),
        Err(DomError::InUseAttribute)
    );
    // Setting the same node again returns it.
    assert_eq!(dom.set_attribute_node(e, attr), Ok(Some(attr)));
    // Removing detaches it with its value.
    assert_eq!(dom.remove_attribute_node(f, attr), Err(DomError::NotFound));
    dom.remove_attribute_node(e, attr).unwrap();
    assert_eq!(dom.attr_owner_element(attr), None);
    assert_eq!(dom.attr_value(attr), "b");
    // Then it can move to another element, replacing an attribute with the same name.
    dom.set_attribute(f, "title", "old").unwrap();
    let old = dom.set_attribute_node(f, attr).unwrap().unwrap();
    assert_eq!(dom.attr_value(old), "old");
    assert_eq!(dom.attr_owner_element(old), None);
    assert_eq!(dom.get_attribute(f, "title"), Some("b"));
    assert_eq!(dom.text_content(attr).as_deref(), Some("b"));
}

#[test]
fn create_attribute_validates() {
    let (mut dom, doc) = html_doc();
    assert_eq!(
        dom.create_attribute(doc, "a b"),
        Err(DomError::InvalidCharacter)
    );
    let a = dom.create_attribute(doc, "ID").unwrap();
    assert_eq!(dom.node_name(a), "id");
    assert_eq!(
        dom.create_attribute_ns(doc, None, "x:y"),
        Err(DomError::Namespace)
    );
}

// ----- factory methods --------------------------------------------------------------------------------------

#[test]
fn document_factories_validate() {
    let (mut dom, doc) = html_doc();
    assert_eq!(
        dom.create_element(doc, "1a", None),
        Err(DomError::InvalidCharacter)
    );
    assert_eq!(
        dom.create_element(doc, "", None),
        Err(DomError::InvalidCharacter)
    );
    let e = dom.create_element(doc, "DIV", None).unwrap();
    assert_eq!(dom.local_name(e), &local_name!("div"));
    assert_eq!(dom.namespace(e), &ns!(html));
    assert_eq!(dom.tag_name(e), "DIV");
    assert_eq!(
        dom.create_cdata_section(doc, "x"),
        Err(DomError::NotSupported)
    );
    assert_eq!(
        dom.create_processing_instruction(doc, "1x", "d"),
        Err(DomError::InvalidCharacter)
    );
    assert_eq!(
        dom.create_processing_instruction(doc, "x", "a?>b"),
        Err(DomError::InvalidCharacter)
    );
    assert_eq!(
        dom.create_document_type(doc, "a b", "", ""),
        Err(DomError::InvalidCharacter)
    );
    let xml = dom.create_xml_document(None, "", None).unwrap();
    assert_eq!(
        dom.create_cdata_section(xml, "a]]>"),
        Err(DomError::InvalidCharacter)
    );
    let cdata = dom.create_cdata_section(xml, "a").unwrap();
    assert_eq!(dom.node_type(cdata), NodeType::CDataSection);
    // In an XML document, createElement keeps case and has no namespace.
    let x = dom.create_element(xml, "Foo", None).unwrap();
    assert_eq!(dom.tag_name(x), "Foo");
    assert_eq!(dom.namespace(x), &ns!());
    assert_eq!(
        dom.create_element_ns(xml, None, "a:b", None),
        Err(DomError::Namespace)
    );
    let custom = dom.create_element(doc, "my-thing", None).unwrap();
    assert_eq!(
        dom.custom_element_state(custom),
        CustomElementState::Undefined
    );
    let with_is = dom
        .create_element(doc, "button", Some("my-button"))
        .unwrap();
    assert_eq!(dom.is_value(with_is).map(|a| &**a), Some("my-button"));
}

#[test]
fn create_xml_document_content_types() {
    let mut dom = Dom::new();
    let d = dom
        .create_xml_document(Some("http://www.w3.org/2000/svg"), "svg", None)
        .unwrap();
    assert_eq!(dom.content_type(d), "image/svg+xml");
    let svg = dom.document_element(d).unwrap();
    assert_eq!(dom.namespace(svg), &ns!(svg));
    assert_eq!(
        dom.create_xml_document(None, "a:b", None),
        Err(DomError::Namespace)
    );
}

// ----- text -------------------------------------------------------------------------------------------------

#[test]
fn character_data_index_errors() {
    let (mut dom, doc) = html_doc();
    let t = dom.create_text_node(doc, "hello");
    assert_eq!(dom.substring_data(t, 6, 1), Err(DomError::IndexSize));
    assert_eq!(dom.insert_data(t, 6, "x"), Err(DomError::IndexSize));
    assert_eq!(dom.delete_data(t, 6, 1), Err(DomError::IndexSize));
    assert_eq!(dom.replace_data(t, 6, 0, "x"), Err(DomError::IndexSize));
    assert_eq!(dom.split_text(t, 6), Err(DomError::IndexSize));
    assert_eq!(dom.substring_data(t, 1, 100).unwrap(), "ello");
    dom.insert_data(t, 5, "!").unwrap();
    dom.delete_data(t, 0, 1).unwrap();
    dom.replace_data(t, 0, 3, "J").unwrap();
    assert_eq!(dom.character_data(t), Some("Jo!"));
    dom.append_data(t, "?");
    assert_eq!(dom.length(t), 4);
}

#[test]
fn character_data_of_any_length_is_kept_and_changed_in_place() {
    // Short data is stored in the node, long data on the heap: both read and change alike.
    let (mut dom, doc) = html_doc();
    for len in [0, 1, 37, 38, 39, 200] {
        let s: String = "\u{e9}x".repeat(len).chars().take(len).collect();
        let t = dom.create_text_node(doc, &s);
        let c = dom.create_comment(doc, &s);
        assert_eq!(dom.character_data(t), Some(s.as_str()));
        assert_eq!(dom.character_data(c), Some(s.as_str()));
        let copy = dom.clone_node(t, false).unwrap();
        assert_eq!(dom.character_data(copy), Some(s.as_str()));
        dom.append_data(t, "\u{1F600}");
        dom.insert_data(c, 0, "<").unwrap();
        assert_eq!(
            dom.character_data(t),
            Some(format!("{s}\u{1F600}").as_str())
        );
        assert_eq!(dom.character_data(c), Some(format!("<{s}").as_str()));
        assert_eq!(dom.character_data(copy), Some(s.as_str()));
        // (back under the inline size, and replaced whole)
        dom.delete_data(c, 1, len).unwrap();
        assert_eq!(dom.character_data(c), Some("<"));
        dom.set_data(t, &s);
        assert_eq!(dom.character_data(t), Some(s.as_str()));
        dom.replace_data(copy, 0, 1, "ab").unwrap();
        let mut want = String::from("ab");
        want.extend(s.chars().skip(1));
        assert_eq!(dom.character_data(copy), Some(want.as_str()));
    }
}

#[test]
fn utf16_offsets_in_character_data() {
    let (mut dom, doc) = html_doc();
    let t = dom.create_text_node(doc, "a\u{1F600}b");
    assert_eq!(dom.length(t), 4);
    assert_eq!(dom.substring_data(t, 1, 2).unwrap(), "\u{1F600}");
    dom.insert_data(t, 3, "-").unwrap();
    assert_eq!(dom.character_data(t), Some("a\u{1F600}-b"));
}

#[test]
fn split_text_inserts_the_rest_after() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "p");
    let t = dom.create_text_node(doc, "hello world");
    dom.append_child(p, t).unwrap();
    let rest = dom.split_text(t, 5).unwrap();
    assert_eq!(dom.character_data(t), Some("hello"));
    assert_eq!(dom.character_data(rest), Some(" world"));
    assert_eq!(dom.next_sibling(t), Some(rest));
    assert_eq!(dom.whole_text(rest), "hello world");
}

#[test]
fn normalize_merges_and_drops_empty_text() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "p");
    for s in ["a", "", "b"] {
        let t = dom.create_text_node(doc, s);
        dom.append_child(p, t).unwrap();
    }
    let b = el(&mut dom, doc, "b");
    dom.append_child(p, b).unwrap();
    let e = dom.create_text_node(doc, "");
    dom.append_child(b, e).unwrap();
    let c = dom.create_text_node(doc, "c");
    dom.append_child(p, c).unwrap();
    dom.normalize(p);
    assert_eq!(names(&dom, p), ["#text", "B", "#text"]);
    assert_eq!(dom.text_content(p).as_deref(), Some("abc"));
    assert_eq!(dom.child_count(b), 0);
}

// ----- cloning and equality ---------------------------------------------------------------------------------

#[test]
fn clone_node_deep_with_template_and_clonable_shadow() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let div = el(&mut dom, doc, "div");
    dom.set_inner_html(div, "<p class=a>x<template><i>t</i></template></p>")
        .unwrap();
    let mut init = ShadowRootInit::new(ShadowRootMode::Open);
    init.clonable = true;
    let shadow = dom.attach_shadow(div, init).unwrap();
    dom.set_inner_html(shadow, "<b>s</b>").unwrap();

    let shallow = dom.clone_node(div, false).unwrap();
    assert_eq!(dom.child_count(shallow), 0);
    let deep = dom.clone_node(div, true).unwrap();
    assert_eq!(
        dom.inner_html(deep),
        "<p class=\"a\">x<template><i>t</i></template></p>"
    );
    assert!(dom.is_equal_node(div, deep));
    let copy_shadow = dom.shadow_root(deep).expect("clonable shadow root cloned");
    assert_eq!(dom.inner_html(copy_shadow), "<b>s</b>");
    assert_eq!(dom.clone_node(shadow, true), Err(DomError::NotSupported));
    dom.set_attribute(deep, "x", "1").unwrap();
    assert!(!dom.is_equal_node(div, deep));
    let other = dom.create_document(DocumentKind::Html);
    let imported = dom.import_node(other, div, true).unwrap();
    assert_eq!(dom.node_document(imported), other);
    let p = dom.first_child(imported).unwrap();
    assert_eq!(dom.node_document(p), other);
    assert_eq!(
        dom.import_node(other, doc, true),
        Err(DomError::NotSupported)
    );
}

#[test]
fn cloning_a_document() {
    let mut dom = Dom::new();
    let doc = dom.parse_html_str("<!doctype html><title>x</title>", ParseOptions::default());
    let copy = dom.clone_node(doc, true).unwrap();
    assert_eq!(dom.node_document(copy), copy);
    let html = dom.document_element(copy).unwrap();
    assert_eq!(dom.node_document(html), copy);
    assert!(dom.is_equal_node(doc, copy));
    assert_eq!(
        dom.outer_html(html),
        dom.outer_html(dom.document_element(doc).unwrap())
    );
}

// ----- tree order -------------------------------------------------------------------------------------------

#[test]
fn compare_document_position_cases() {
    use position::*;
    let mut dom = Dom::new();
    let doc = dom.parse_html_str("<p id=a><b></b></p><i></i>", ParseOptions::default());
    let body = dom.body(doc).unwrap();
    let p = dom.first_child(body).unwrap();
    let b = dom.first_child(p).unwrap();
    let i = dom.next_sibling(p).unwrap();
    assert_eq!(dom.compare_document_position(p, p), 0);
    assert_eq!(
        dom.compare_document_position(p, b),
        CONTAINED_BY | FOLLOWING
    );
    assert_eq!(dom.compare_document_position(b, p), CONTAINS | PRECEDING);
    assert_eq!(dom.compare_document_position(b, i), FOLLOWING);
    assert_eq!(dom.compare_document_position(i, b), PRECEDING);
    let loose = el(&mut dom, doc, "x");
    let r = dom.compare_document_position(p, loose);
    assert_eq!(
        r & (DISCONNECTED | IMPLEMENTATION_SPECIFIC),
        DISCONNECTED | IMPLEMENTATION_SPECIFIC
    );
    let r2 = dom.compare_document_position(loose, p);
    assert_ne!(r & (PRECEDING | FOLLOWING), r2 & (PRECEDING | FOLLOWING));
    // Attributes.
    dom.set_attribute(p, "title", "t").unwrap();
    let id = dom.get_attribute_node(p, "id").unwrap();
    let title = dom.get_attribute_node(p, "title").unwrap();
    assert_eq!(
        dom.compare_document_position(id, title),
        IMPLEMENTATION_SPECIFIC | FOLLOWING
    );
    assert_eq!(
        dom.compare_document_position(title, id),
        IMPLEMENTATION_SPECIFIC | PRECEDING
    );
    assert_eq!(
        dom.compare_document_position(p, id),
        CONTAINED_BY | FOLLOWING
    );
    assert_eq!(dom.compare_document_position(id, p), CONTAINS | PRECEDING);
    assert!(dom.contains(body, Some(b)));
    assert!(dom.contains(b, Some(b)));
    assert!(!dom.contains(b, Some(body)));
    assert!(!dom.contains(b, None));
}

#[test]
fn iterators_walk_tree_order() {
    let mut dom = Dom::new();
    let doc = dom.parse_html_str(
        "<a><b></b><c><d></d></c></a><e></e>",
        ParseOptions::default(),
    );
    let body = dom.body(doc).unwrap();
    let order: Vec<String> = dom
        .descendants(body)
        .map(|n| dom.node_name(n).into_owned())
        .collect();
    assert_eq!(order, ["A", "B", "C", "D", "E"]);
    let d = dom.descendants(body).nth(3).unwrap();
    let following: Vec<String> = dom
        .following(d)
        .map(|n| dom.node_name(n).into_owned())
        .collect();
    assert_eq!(following, ["E"]);
    let preceding: Vec<String> = dom
        .preceding(d)
        .map(|n| dom.node_name(n).into_owned())
        .collect();
    assert_eq!(
        preceding,
        ["C", "B", "A", "BODY", "HEAD", "HTML", "#document"]
    );
    let ancestors: Vec<String> = dom
        .ancestors(d)
        .map(|n| dom.node_name(n).into_owned())
        .collect();
    assert_eq!(ancestors, ["C", "A", "BODY", "HTML", "#document"]);
    // Shadow-including order visits a shadow tree right after its host.
    let a = dom.first_child(body).unwrap();
    let shadow = dom.attach_shadow(a, ShadowRootInit::new(ShadowRootMode::Open));
    assert_eq!(shadow, Err(DomError::NotSupported)); // "a" can't be a host
    let host = dom.create_element(doc, "span", None).unwrap();
    dom.insert_before(body, host, Some(a)).unwrap();
    let shadow = dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    let s = dom.create_element(doc, "s", None).unwrap();
    dom.append_child(shadow, s).unwrap();
    let k = dom.create_element(doc, "k", None).unwrap();
    dom.append_child(host, k).unwrap();
    let order: Vec<String> = dom
        .shadow_including_descendants(body)
        .map(|n| dom.node_name(n).into_owned())
        .take(4)
        .collect();
    assert_eq!(order, ["SPAN", "#document-fragment", "S", "K"]);
}

#[test]
fn shadow_including_tree_order_agrees_with_the_traversal() {
    let (mut dom, doc) = html_doc();
    let html = el(&mut dom, doc, "html");
    dom.append_child(doc, html).unwrap();
    let body = el(&mut dom, doc, "body");
    dom.append_child(html, body).unwrap();
    // body > [p > [host (shadow: s, u > t) > [k, l]], i]
    let p = el(&mut dom, doc, "p");
    dom.append_child(body, p).unwrap();
    let host = el(&mut dom, doc, "span");
    dom.append_child(p, host).unwrap();
    let shadow = dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    for name in ["s", "u"] {
        let e = el(&mut dom, doc, name);
        dom.append_child(shadow, e).unwrap();
    }
    let u = dom.last_child(shadow).unwrap();
    let t = el(&mut dom, doc, "t");
    dom.append_child(u, t).unwrap();
    for name in ["k", "l"] {
        let e = el(&mut dom, doc, name);
        dom.append_child(host, e).unwrap();
    }
    let i = el(&mut dom, doc, "i");
    dom.append_child(body, i).unwrap();
    let order: Vec<NodeId> = dom.shadow_including_descendants(doc).collect();
    for (ia, &a) in order.iter().enumerate() {
        for (ib, &b) in order.iter().enumerate() {
            assert_eq!(
                dom.shadow_including_tree_order(a, b),
                ia.cmp(&ib),
                "{} vs {}",
                dom.node_name(a),
                dom.node_name(b)
            );
        }
    }
    // Nodes of another tree are not ordered against these.
    let loose = el(&mut dom, doc, "x");
    assert_eq!(
        dom.shadow_including_tree_order(loose, i),
        std::cmp::Ordering::Equal
    );
}

// ----- mutation records -------------------------------------------------------------------------------------

#[test]
fn records_are_queued_only_for_observed_subtrees() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "div");
    let a = el(&mut dom, doc, "a");
    dom.append_child(p, a).unwrap();
    assert!(dom.mutation_records().is_empty());
    dom.set_observed(p, true);
    let b = el(&mut dom, doc, "b");
    dom.append_child(p, b).unwrap();
    dom.set_attribute(a, "x", "1").unwrap();
    dom.set_attribute(a, "x", "2").unwrap();
    let t = dom.create_text_node(doc, "t");
    dom.append_child(a, t).unwrap();
    dom.set_data(t, "u");
    dom.remove_child(p, a).unwrap();
    let records = dom.take_mutation_records();
    assert_eq!(records.len(), 6, "{records:#?}");
    assert_eq!(records[0].kind, MutationKind::ChildList);
    assert_eq!(records[0].added_nodes, vec![b]);
    assert_eq!(records[0].previous_sibling, Some(a));
    assert_eq!(records[1].kind, MutationKind::Attributes);
    assert_eq!(records[1].old_value, None);
    assert_eq!(records[2].old_value.as_deref(), Some("1"));
    assert_eq!(records[2].attribute_name.as_deref(), Some("x"));
    assert_eq!(records[3].target, a);
    assert_eq!(records[4].kind, MutationKind::CharacterData);
    assert_eq!(records[4].old_value.as_deref(), Some("t"));
    assert_eq!(records[5].removed_nodes, vec![a]);
    assert_eq!(records[5].next_sibling, Some(b));
    // The removed node keeps a transient observer until delivery.
    dom.set_attribute(a, "y", "1").unwrap();
    assert_eq!(dom.take_mutation_records().len(), 1);
    assert_eq!(dom.transient_observers(), &[(a, p)]);
    dom.clear_transient_observers();
    dom.set_attribute(a, "z", "1").unwrap();
    assert!(dom.take_mutation_records().is_empty());
}

#[test]
fn inserting_a_fragment_records_its_removal_from_the_fragment() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "div");
    let frag = dom.create_document_fragment(doc);
    let a = el(&mut dom, doc, "a");
    dom.append_child(frag, a).unwrap();
    dom.set_observed(frag, true);
    dom.set_observed(p, true);
    dom.append_child(p, frag).unwrap();
    let r = dom.take_mutation_records();
    assert_eq!(r.len(), 2);
    assert_eq!((r[0].target, r[0].removed_nodes.clone()), (frag, vec![a]));
    assert!(r[0].added_nodes.is_empty());
    assert_eq!((r[1].target, r[1].added_nodes.clone()), (p, vec![a]));
}

#[test]
fn replace_child_queues_one_record() {
    let (mut dom, doc) = html_doc();
    let p = el(&mut dom, doc, "div");
    let a = el(&mut dom, doc, "a");
    let b = el(&mut dom, doc, "b");
    dom.append_child(p, a).unwrap();
    dom.set_observed(p, true);
    dom.replace_child(p, b, a).unwrap();
    let r = dom.take_mutation_records();
    assert_eq!(r.len(), 1);
    assert_eq!(
        (r[0].added_nodes.clone(), r[0].removed_nodes.clone()),
        (vec![b], vec![a])
    );
    // replace all: one record with both lists.
    dom.set_text_content(p, "x");
    let r = dom.take_mutation_records();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].removed_nodes, vec![b]);
}

#[test]
fn version_counters_bump_on_mutation() {
    let (mut dom, doc) = html_doc();
    let other = dom.create_document(DocumentKind::Html);
    let p = el(&mut dom, doc, "div");
    let v = dom.document_version(doc);
    let ov = dom.document_version(other);
    dom.set_attribute(p, "a", "b").unwrap();
    assert!(dom.document_version(doc) > v);
    assert_eq!(dom.document_version(other), ov);
    let v = dom.version();
    let t = dom.create_text_node(doc, "x");
    dom.append_child(p, t).unwrap();
    assert!(dom.version() > v);
}

#[test]
fn reactions_report_hook_points() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let body = dom.body(doc).unwrap();
    dom.set_reactions_enabled(true);
    let p = el(&mut dom, doc, "p");
    dom.set_attribute(p, "id", "x").unwrap();
    dom.append_child(body, p).unwrap();
    dom.remove_node(p);
    let r = dom.take_reactions();
    assert!(r.contains(&Reaction::AttributeChanged {
        element: p,
        local_name: local_name!("id"),
        namespace: None,
        old_value: None,
        new_value: Some("x".into()),
    }));
    assert!(r.contains(&Reaction::Inserted {
        node: p,
        connected: true,
        root: true,
    }));
    assert!(r.iter().any(|x| matches!(x, Reaction::Removed {
        node,
        old_parent: Some(parent),
        was_connected: true,
        ..
    } if *node == p && *parent == body)));
    assert!(r.contains(&Reaction::ChildrenChanged { parent: body }));
}

#[test]
fn a_removed_nodes_reaction_lists_its_descendants() {
    // The removed node's reaction indexes its shadow-including descendants at its removal, in
    // tree order, in the list taken with the batch; the descendants' own reactions list none.
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let body = dom.body(doc).unwrap();
    let ul = el(&mut dom, doc, "ul");
    let li = el(&mut dom, doc, "li");
    let host = el(&mut dom, doc, "div");
    dom.append_child(ul, li).unwrap();
    let t = dom.create_text_node(doc, "x");
    dom.append_child(li, t).unwrap();
    dom.append_child(ul, host).unwrap();
    let shadow = dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    let inner = el(&mut dom, doc, "span");
    dom.append_child(shadow, inner).unwrap();
    let other = el(&mut dom, doc, "p");
    dom.append_child(body, ul).unwrap();
    dom.append_child(body, other).unwrap();
    dom.set_reactions_enabled(true);
    dom.remove_node(other);
    dom.remove_node(ul);
    let r = dom.take_reactions();
    let list = dom.take_removed_descendants();
    let range = |n: NodeId| {
        r.iter()
            .find_map(|x| match x {
                Reaction::Removed {
                    node, descendants, ..
                } if *node == n => Some(descendants.0 as usize..descendants.1 as usize),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(list[range(ul)], [li, t, host, shadow, inner]);
    assert!(range(other).is_empty());
    assert!(range(li).is_empty());
    // The next batch has a list of its own.
    dom.remove_node(li);
    let r = dom.take_reactions();
    assert!(r
        .iter()
        .any(|x| matches!(x, Reaction::Removed { node, descendants: (0, 1), .. } if *node == li)));
    assert_eq!(dom.take_removed_descendants(), [t]);
}

// ----- namespaces -------------------------------------------------------------------------------------------

#[test]
fn namespace_lookups() {
    let mut dom = Dom::new();
    let doc = dom
        .create_xml_document(Some("urn:root"), "r:root", None)
        .unwrap();
    let root = dom.document_element(doc).unwrap();
    dom.set_attribute_ns(
        root,
        Some("http://www.w3.org/2000/xmlns/"),
        "xmlns:q",
        "urn:q",
    )
    .unwrap();
    dom.set_attribute_ns(
        root,
        Some("http://www.w3.org/2000/xmlns/"),
        "xmlns",
        "urn:default",
    )
    .unwrap();
    let child = dom.create_element_ns(doc, None, "c", None).unwrap();
    dom.append_child(root, child).unwrap();
    assert_eq!(
        dom.lookup_namespace_uri(child, Some("r")).as_deref(),
        Some("urn:root")
    );
    assert_eq!(
        dom.lookup_namespace_uri(child, Some("q")).as_deref(),
        Some("urn:q")
    );
    assert_eq!(
        dom.lookup_namespace_uri(child, None).as_deref(),
        Some("urn:default")
    );
    assert_eq!(
        dom.lookup_prefix(child, Some("urn:q")).as_deref(),
        Some("q")
    );
    assert_eq!(
        dom.lookup_prefix(child, Some("urn:root")).as_deref(),
        Some("r")
    );
    assert!(dom.is_default_namespace(child, Some("urn:default")));
    assert_eq!(
        dom.lookup_namespace_uri(child, Some("xml")).as_deref(),
        Some("http://www.w3.org/XML/1998/namespace")
    );
}

// ----- parsing and serialization ----------------------------------------------------------------------------

#[test]
fn parse_document_from_bytes_honours_encoding_and_bom() {
    let mut dom = Dom::new();
    let doc = dom.parse_html_document(
        b"<p>caf\xe9</p>",
        encoding_rs::WINDOWS_1252,
        ParseOptions::default(),
    );
    assert_eq!(
        dom.text_content(dom.body(doc).unwrap()).as_deref(),
        Some("caf\u{e9}")
    );
    let doc = dom.parse_html_document(
        b"\xEF\xBB\xBF<p>caf\xc3\xa9</p>",
        encoding_rs::WINDOWS_1252,
        ParseOptions::default(),
    );
    assert_eq!(
        dom.text_content(dom.body(doc).unwrap()).as_deref(),
        Some("caf\u{e9}")
    );
    assert_eq!(dom.quirks_mode(doc), QuirksMode::Quirks);
    let doc = dom.parse_html_str("<!DOCTYPE html><p>", ParseOptions::default());
    assert_eq!(dom.quirks_mode(doc), QuirksMode::NoQuirks);
}

#[test]
fn foreign_content_namespaces() {
    let mut dom = Dom::new();
    let doc = dom.parse_html_str(
        "<svg viewBox='0 0 1 1'><foreignObject><p>x</p></foreignObject></svg><math><mi>y</mi></math>",
        ParseOptions::default(),
    );
    let body = dom.body(doc).unwrap();
    let svg = dom.first_child(body).unwrap();
    assert_eq!(dom.namespace(svg), &ns!(svg));
    assert_eq!(dom.get_attribute(svg, "viewBox"), Some("0 0 1 1"));
    let fo = dom.first_child(svg).unwrap();
    assert_eq!(&**dom.local_name(fo), "foreignObject");
    assert_eq!(dom.tag_name(fo), "foreignObject");
    let p = dom.first_child(fo).unwrap();
    assert_eq!(dom.namespace(p), &ns!(html));
    let math = dom.next_sibling(svg).unwrap();
    assert_eq!(dom.namespace(math), &ns!(mathml));
}

#[test]
fn incremental_parser_stops_at_scripts() {
    let cell = RefCell::new(Dom::new());
    let doc = cell.borrow_mut().create_document(DocumentKind::Html);
    let mut parser = HtmlParser::new(&cell, doc, ParseOptions::default());
    parser.push_str("<p>a</p><scr");
    assert_eq!(parser.run(), ParseStep::NeedInput);
    parser.push_str("ipt>x()</script><p>b");
    let ParseStep::Script(script) = parser.run() else {
        panic!("expected a script stop")
    };
    {
        // The page layer runs the script with the store borrowed; it may write at the insertion point.
        let dom = cell.borrow();
        assert_eq!(dom.text_content(script).as_deref(), Some("x()"));
        assert_ne!(
            dom.element_flags(script) & element_flags::PARSER_INSERTED,
            0
        );
    }
    // The insertion point goes just before the next input character while the script runs: what it
    // writes goes in there, and parsing it stops there (the input after it waits).
    parser.set_insertion_point();
    parser.insert_input("<i>w</i>");
    parser.insert_input("<u>v</u>");
    assert_eq!(parser.run(), ParseStep::NeedInput);
    {
        let dom = cell.borrow();
        let body = dom.body(doc).unwrap();
        assert_eq!(
            dom.inner_html(body),
            "<p>a</p><script>x()</script><i>w</i><u>v</u>"
        );
    }
    parser.restore_insertion_point();
    assert_eq!(parser.run(), ParseStep::NeedInput);
    parser.finish();
    let dom = cell.borrow();
    let body = dom.body(doc).unwrap();
    assert_eq!(
        dom.inner_html(body),
        "<p>a</p><script>x()</script><i>w</i><u>v</u><p>b</p>"
    );
}

#[test]
fn fragment_parsing_marks_scripts_already_started() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let div = el(&mut dom, doc, "div");
    dom.set_inner_html(div, "<script>1</script>").unwrap();
    let s = dom.first_child(div).unwrap();
    assert_ne!(
        dom.element_flags(s) & element_flags::SCRIPT_ALREADY_STARTED,
        0
    );
}

/// HTML "initialize an HTML fragment parser" with `runScripts`: a script whose end tag the input has
/// may run once inserted (SVG's too); one still open at the end of the input never runs.
#[test]
fn a_fragment_parse_that_runs_scripts_leaves_closed_scripts_to_run() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let div = el(&mut dom, doc, "div");
    let started = |dom: &Dom, s| dom.element_flags(s) & element_flags::SCRIPT_ALREADY_STARTED != 0;
    let frag = dom.parse_fragment_running_scripts(
        div,
        "<script>1</script><svg><script>2</script></svg><script>3",
        true,
    );
    let scripts: Vec<NodeId> = dom
        .descendants(frag)
        .filter(|&n| dom.is_script_element(n))
        .collect();
    assert_eq!(scripts.len(), 3);
    assert!(!started(&dom, scripts[0]));
    assert!(!started(&dom, scripts[1]));
    assert!(started(&dom, scripts[2]));
    // (without runScripts every one is already started)
    let frag = dom.parse_fragment(div, "<script>1</script>", true, ParseOptions::default());
    let s = dom.first_child(frag).unwrap();
    assert!(started(&dom, s));
}

#[test]
fn inner_html_round_trips_and_escapes() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let div = el(&mut dom, doc, "div");
    dom.set_inner_html(
        div,
        "<p title='a\"b&amp;<>'>x &amp; &lt;y&gt;\u{a0}</p><br><!--c--><style>a<b</style><svg><path d=1 /></svg>",
    )
    .unwrap();
    assert_eq!(
        dom.inner_html(div),
        "<p title=\"a&quot;b&amp;&lt;&gt;\">x &amp; &lt;y&gt;&nbsp;</p><br><!--c--><style>a<b</style><svg><path d=\"1\"></path></svg>"
    );
    // Void elements have no inner HTML.
    let br = dom.child_at(div, 1).unwrap();
    assert_eq!(dom.inner_html(br), "");
    assert_eq!(dom.outer_html(br), "<br>");
    // Templates serialize their contents.
    dom.set_inner_html(div, "<template><b>t</b></template>")
        .unwrap();
    let t = dom.first_child(div).unwrap();
    assert_eq!(dom.inner_html(t), "<b>t</b>");
    dom.set_inner_html(t, "<i>u</i>").unwrap();
    assert_eq!(dom.child_count(t), 0);
    assert_eq!(dom.outer_html(t), "<template><i>u</i></template>");
}

#[test]
fn serialization_of_attributes_namespaces_and_is() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let div = el(&mut dom, doc, "div");
    dom.set_inner_html(div, "<svg xlink:href=a xml:lang=b xmlns:q=c></svg>")
        .unwrap();
    assert_eq!(
        dom.inner_html(div),
        "<svg xlink:href=\"a\" xml:lang=\"b\" xmlns:q=\"c\"></svg>"
    );
    let b = dom.create_element(doc, "button", Some("my-b")).unwrap();
    assert_eq!(dom.outer_html(b), "<button is=\"my-b\"></button>");
    let x = dom
        .create_element_ns(doc, Some("urn:x"), "p:q", None)
        .unwrap();
    assert_eq!(dom.outer_html(x), "<p:q></p:q>");
}

#[test]
fn get_html_serializes_shadow_roots_on_request() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let div = el(&mut dom, doc, "div");
    let host = el(&mut dom, doc, "span");
    dom.append_child(div, host).unwrap();
    let mut init = ShadowRootInit::new(ShadowRootMode::Open);
    init.serializable = true;
    init.delegates_focus = true;
    let shadow = dom.attach_shadow(host, init).unwrap();
    dom.set_inner_html(shadow, "<b>s</b>").unwrap();
    assert_eq!(dom.inner_html(div), "<span></span>");
    let opts = SerializeOptions {
        serializable_shadow_roots: true,
        ..Default::default()
    };
    assert_eq!(
        dom.get_html(div, opts),
        "<span><template shadowrootmode=\"open\" shadowrootdelegatesfocus=\"\" shadowrootserializable=\"\"><b>s</b></template></span>"
    );
    let closed = el(&mut dom, doc, "p");
    dom.append_child(div, closed).unwrap();
    let cs = dom
        .attach_shadow(closed, ShadowRootInit::new(ShadowRootMode::Closed))
        .unwrap();
    let opts = SerializeOptions {
        shadow_roots: &[cs],
        ..Default::default()
    };
    assert!(dom
        .get_html(div, opts)
        .ends_with("<p><template shadowrootmode=\"closed\"></template></p>"));
}

#[test]
fn outer_html_and_insert_adjacent_html() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let body = dom.body(doc).unwrap();
    let p = el(&mut dom, doc, "p");
    dom.append_child(body, p).unwrap();
    dom.insert_adjacent_html(p, AdjacentPosition::BeforeBegin, "<a></a>")
        .unwrap();
    dom.insert_adjacent_html(p, AdjacentPosition::AfterBegin, "1")
        .unwrap();
    dom.insert_adjacent_html(p, AdjacentPosition::BeforeEnd, "2")
        .unwrap();
    dom.insert_adjacent_html(p, AdjacentPosition::AfterEnd, "<i></i>")
        .unwrap();
    assert_eq!(dom.inner_html(body), "<a></a><p>12</p><i></i>");
    assert_eq!(AdjacentPosition::parse("nowhere"), Err(DomError::Syntax));
    let html = dom.document_element(doc).unwrap();
    assert_eq!(
        dom.insert_adjacent_html(html, AdjacentPosition::BeforeBegin, "x"),
        Err(DomError::NoModificationAllowed)
    );
    dom.set_outer_html(p, "<b>3</b><b>4</b>").unwrap();
    assert_eq!(dom.inner_html(body), "<a></a><b>3</b><b>4</b><i></i>");
    assert_eq!(dom.parent(p), None);
    assert_eq!(
        dom.set_outer_html(html, "<x>"),
        Err(DomError::NoModificationAllowed)
    );
    // No parent: nothing happens.
    assert_eq!(dom.set_outer_html(p, "<x>"), Ok(()));
}

#[test]
fn parsed_text_merges_into_one_node() {
    let mut dom = Dom::new();
    let doc = dom.parse_html_str("a&amp;b<!---->c", ParseOptions::default());
    let body = dom.body(doc).unwrap();
    assert_eq!(names(&dom, body), ["#text", "#comment", "#text"]);
    assert_eq!(
        dom.character_data(dom.first_child(body).unwrap()),
        Some("a&b")
    );
}

#[test]
fn processing_instructions_and_noscript() {
    let mut dom = Dom::new();
    let doc = dom.parse_html_str(
        "<?pi data?><noscript><b>x</b></noscript>",
        ParseOptions::default(),
    );
    let pi = dom.first_child(doc).unwrap();
    assert_eq!(dom.node_type(pi), NodeType::ProcessingInstruction);
    assert_eq!(dom.pi_target(pi), Some("pi"));
    assert_eq!(dom.character_data(pi), Some("data"));
    let head = dom.head(doc).unwrap();
    let noscript = dom.first_child(head).unwrap();
    // Scripting enabled: noscript content is raw text.
    assert_eq!(names(&dom, noscript), ["#text"]);
    let doc = dom.parse_html_str(
        "<body><noscript><b>x</b></noscript>",
        ParseOptions {
            scripting_enabled: false,
            ..Default::default()
        },
    );
    let noscript = dom.first_child(dom.body(doc).unwrap()).unwrap();
    assert_eq!(names(&dom, noscript), ["B"]);
}

#[test]
fn fragment_parsing_in_quirks_document_and_tables() {
    let mut dom = Dom::new();
    let doc = dom.parse_html_str("<p>", ParseOptions::default());
    assert_eq!(dom.quirks_mode(doc), QuirksMode::Quirks);
    let table = el(&mut dom, doc, "table");
    let frag = dom.parse_fragment(table, "<tr><td>x", false, ParseOptions::default());
    assert_eq!(
        dom.get_html(frag, SerializeOptions::default()),
        "<tbody><tr><td>x</td></tr></tbody>"
    );
    // The temporary parsing document is gone.
    let count = dom.node_count();
    let frag2 = dom.parse_fragment(table, "<tr>", false, ParseOptions::default());
    dom.free_subtree(frag2);
    assert_eq!(dom.node_count(), count);
}

#[test]
fn xml_documents_parse_with_namespaces() {
    let mut dom = Dom::new();
    let (doc, r) = dom.parse_xml_str(
        "<?xml version='1.0'?>\r\n<!DOCTYPE r [<!ENTITY e 'ent'>]>\n<!-- c -->\
         <r xmlns='urn:a' xmlns:p='urn:p' p:x='1&amp;2'><p:c>t&e;&#x41;<![CDATA[<x>]]></p:c><?pi d?></r>",
    );
    assert_eq!(r, Ok(()));
    let names: Vec<String> = dom
        .children(doc)
        .map(|c| dom.node_name(c).to_string())
        .collect();
    assert_eq!(names, ["r", "#comment", "r"]);
    let root = dom.document_element(doc).unwrap();
    assert_eq!(&**dom.namespace(root), "urn:a");
    let a = &dom.attrs(root)[2];
    assert_eq!(
        (&*a.name.ns, &*a.name.local, &*a.value),
        ("urn:p", "x", "1&2")
    );
    let c = dom.first_child(root).unwrap();
    assert_eq!(&**dom.namespace(c), "urn:p");
    assert_eq!(dom.child_text_content(c), "tentA<x>");
    assert_eq!(dom.children(c).count(), 2, "text then CDATA");
}

#[test]
fn xml_well_formedness_errors_leave_a_parsererror() {
    let mut dom = Dom::new();
    for bad in [
        "<a><b></a>",
        "<a></a><b/>",
        "<a x='1' x='2'/>",
        "<p:a/>",
        "<a>&nope;</a>",
        "text<a/>",
        "<a xmlns:p=''/>",
        "<a><!-- x -- y --></a>",
        "",
        "<a>&#0;</a>",
    ] {
        let (doc, r) = dom.parse_xml_str(bad);
        assert!(r.is_err(), "{bad:?} parsed");
        let root = dom.document_element(doc).unwrap();
        assert_eq!(&**dom.local_name(root), "parsererror", "{bad:?}");
    }
}

#[test]
fn xml_fragments_use_the_context_namespaces() {
    let mut dom = Dom::new();
    let (doc, _) = dom.parse_xml_str("<r xmlns='urn:a' xmlns:q='urn:q'/>");
    let root = dom.document_element(doc).unwrap();
    let frag = dom.parse_xml_fragment(root, "x<c/><q:d/>").unwrap();
    let kids: Vec<NodeId> = dom.children(frag).collect();
    assert_eq!(kids.len(), 3);
    assert_eq!(&**dom.namespace(kids[1]), "urn:a");
    assert_eq!(&**dom.namespace(kids[2]), "urn:q");
    assert_eq!(dom.parse_xml_fragment(root, "<c>"), Err(DomError::Syntax));
}

#[test]
fn xml_parsing_appends_a_templates_children_to_its_contents() {
    let mut dom = Dom::new();
    let (doc, r) = dom.parse_xml_str(
        "<html xmlns='http://www.w3.org/1999/xhtml'><template>t<!--c--><div>\
         <template><span/></template></div></template></html>",
    );
    assert_eq!(r, Ok(()));
    let root = dom.document_element(doc).unwrap();
    let template = dom.first_child(root).unwrap();
    assert_eq!(dom.children(template).count(), 0, "no children of its own");
    let contents = dom.template_contents(template).unwrap();
    let owner = dom.node_document(contents);
    assert_ne!(owner, doc);
    let kids: Vec<NodeId> = dom.children(contents).collect();
    assert_eq!(kids.len(), 3, "text, comment, div");
    assert!(kids.iter().all(|&k| dom.node_document(k) == owner));
    let inner = dom.first_child(kids[2]).unwrap();
    assert_eq!(dom.children(inner).count(), 0);
    let inner_contents = dom.template_contents(inner).unwrap();
    let span = dom.first_child(inner_contents).unwrap();
    assert_eq!(dom.node_document(span), dom.node_document(inner_contents));

    // The fragment parsing algorithm (`innerHTML` in an XML document) does the same.
    let frag = dom
        .parse_xml_fragment(
            root,
            "<template xmlns='http://www.w3.org/1999/xhtml'><b/></template>",
        )
        .unwrap();
    let t = dom.first_child(frag).unwrap();
    assert_eq!(dom.children(t).count(), 0);
    let b = dom.first_child(dom.template_contents(t).unwrap()).unwrap();
    assert_eq!(dom.node_document(b), owner);
}

#[test]
fn inner_html_reports_its_nodes_once_where_they_land() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let body = dom.body(doc).unwrap();
    let old = el(&mut dom, doc, "p");
    dom.append_child(body, old).unwrap();
    dom.set_reactions_enabled(true);
    dom.set_inner_html(body, "<ul><li onclick=go()>a</li></ul>")
        .unwrap();
    let r = dom.take_reactions();
    let ul = dom.first_child(body).unwrap();
    let li = dom.first_child(ul).unwrap();
    let text = dom.first_child(li).unwrap();
    // The parse in the fragment parser's own document and the move into the fragment are not
    // reported; the insertion into the body is, for each node, connected (the fragment's own
    // removal of them is, as for any fragment); the removed child too.
    for n in [ul, li, text] {
        let inserted: Vec<_> = r
            .iter()
            .filter(|x| matches!(x, Reaction::Inserted { node, .. } if *node == n))
            .collect();
        assert_eq!(
            inserted,
            [&Reaction::Inserted {
                node: n,
                connected: true,
                root: n == ul,
            }]
        );
        assert!(!r
            .iter()
            .any(|x| matches!(x, Reaction::Adopted { node, .. } if *node == n)));
    }
    assert!(r
        .iter()
        .any(|x| matches!(x, Reaction::Removed { node, .. } if *node == old)));
    // Attribute changes are kept (event handler content attributes act on them).
    assert!(r.iter().any(|x| matches!(x,
        Reaction::AttributeChanged { element, local_name, .. }
            if *element == li && &**local_name == "onclick")));
    // Later changes are reported again.
    dom.remove_node(ul);
    assert!(dom
        .take_reactions()
        .iter()
        .any(|x| matches!(x, Reaction::Removed { node, .. } if *node == li)));
}

/// A document parsed from one input chunk (whole tags take the tokenizer's one-scan path) and from
/// one chunk per character (every tag goes through the state machine) must come out the same: the
/// tree, and the attributes' names, values and order.
#[test]
fn tokenizer_tag_scan_matches_state_machine() {
    let inputs = [
        "<DIV Class=a ID='b' data-X = \"c d\" hidden>x</DIV>",
        "<p\ta\nb=1\x0Cc = ' 2 '>y</p >",
        "<br/><img src=a.png alt=\"\"/><input value=x/>",
        "<a href=\"?x&amp;y\" title='t&lt;'>e</a>",
        "<a b=1 b=2 B=3>dup</a>",
        "<a b=\"1\"c=2>no space</a>",
        "<a b= >missing</a><a =b>eq</a><a b=x`y>tick</a>",
        "<a \"b\"=1 c<d=2>odd names</a>",
        "</b x=1><b/x>end tag attrs</b>",
        "<svg viewBox='0 0 1 1'><circle CX=1 /></svg><math><mi>z</mi></math>",
        "<ul>\r\n<li\r\nclass=a>\0</li></ul>",
        "<table><tr><td a=1 b=2 c=3 d=4 e=5 f=6 g=7 h=8 i=9>many</td></tr></table>",
        "<textarea>  <b>raw</b></textarea><title a=1>t</title><script>if (a<b) x()</script>",
        "<é á=ó>unicode</é>",
    ];
    for input in inputs {
        let parse = |chunked: bool| {
            let cell = RefCell::new(Dom::new());
            let doc = cell.borrow_mut().create_document(DocumentKind::Html);
            let mut parser = HtmlParser::new(&cell, doc, ParseOptions::default());
            if chunked {
                for c in input.chars() {
                    parser.push_str(c.encode_utf8(&mut [0; 4]));
                }
            } else {
                parser.push_str(input);
            }
            parser.finish();
            let dom = cell.borrow();
            dom.outer_html(dom.document_element(doc).unwrap())
        };
        assert_eq!(parse(false), parse(true), "{input:?}");
    }
}

/// With a reaction filter, the node a tree mutation is about is always reported, its descendants only
/// when the filter takes them (and never when they are not elements); new elements' attributes only
/// when the filter takes them. Connectedness is kept for every node all the same.
#[test]
fn reaction_filter_limits_descendant_and_new_attribute_reactions() {
    fn descendant(e: &ElementData) -> bool {
        &*e.name().local == "script"
    }
    fn new_attribute(_: &ElementData, a: &Attr) -> bool {
        a.local_name().starts_with("on")
    }
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let body = dom.body(doc).unwrap();
    dom.set_reactions_enabled(true);
    dom.set_reaction_filter(Some(ReactionFilter {
        descendant,
        new_attribute,
    }));
    dom.set_inner_html(
        body,
        "<ul class=a onclick=go()><li>x</li><script>s</script></ul><p>y</p>",
    )
    .unwrap();
    let r = dom.take_reactions();
    let ul = dom.first_child(body).unwrap();
    let li = dom.first_child(ul).unwrap();
    let script = dom.next_sibling(li).unwrap();
    let p = dom.next_sibling(ul).unwrap();
    let inserted: Vec<(NodeId, bool)> = r
        .iter()
        .filter_map(|x| match x {
            Reaction::Inserted { node, root, .. } => Some((*node, *root)),
            _ => None,
        })
        .collect();
    assert_eq!(inserted, [(ul, true), (script, false), (p, true)]);
    let attrs: Vec<&str> = r
        .iter()
        .filter_map(|x| match x {
            Reaction::AttributeChanged { local_name, .. } => Some(&**local_name),
            _ => None,
        })
        .collect();
    assert_eq!(attrs, ["onclick"]);
    assert!(dom.is_connected(li) && dom.is_connected(dom.first_child(li).unwrap()));
    dom.remove_node(ul);
    let removed: Vec<NodeId> = dom
        .take_reactions()
        .iter()
        .filter_map(|x| match x {
            Reaction::Removed { node, .. } => Some(*node),
            _ => None,
        })
        .collect();
    assert_eq!(removed, [ul, script]);
    assert!(!dom.is_connected(li));
    // Without a filter, every node is reported again.
    dom.set_reaction_filter(None);
    dom.append_child(body, ul).unwrap();
    let n = dom
        .take_reactions()
        .iter()
        .filter(|x| matches!(x, Reaction::Inserted { .. }))
        .count();
    assert_eq!(n, 5);
}

/// "Replace all" runs the children changed steps once, for the children it ends with.
#[test]
fn replace_all_reports_children_changed_once() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let body = dom.body(doc).unwrap();
    dom.set_inner_html(body, "<p>a</p><p>b</p><p>c</p>")
        .unwrap();
    dom.set_reactions_enabled(true);
    let changed = |dom: &mut Dom| {
        dom.take_reactions()
            .iter()
            .filter(|r| matches!(r, Reaction::ChildrenChanged { parent } if *parent == body))
            .count()
    };
    dom.set_inner_html(body, "<i>x</i><i>y</i>").unwrap();
    assert_eq!(changed(&mut dom), 1);
    dom.replace_children(body, Vec::new()).unwrap();
    assert_eq!(changed(&mut dom), 1);
    assert!(dom.first_child(body).is_none());
    dom.replace_children(body, Vec::new()).unwrap();
    assert_eq!(changed(&mut dom), 0);
}

/// The markup setters insert the fragment the fragment parsing algorithm made without the removing
/// steps of its children (no script has seen that fragment): each new node is reported once, as
/// inserted, and the only removals reported are of the nodes they replace.
#[test]
fn markup_setters_report_no_removal_from_the_parsed_fragment() {
    let mut dom = Dom::new();
    let doc = dom.create_html_document(None);
    let body = dom.body(doc).unwrap();
    dom.set_inner_html(body, "<div id=a><p>old</p></div>")
        .unwrap();
    let div = dom.first_child(body).unwrap();
    let old = dom.first_child(div).unwrap();
    dom.set_reactions_enabled(true);
    dom.take_reactions();
    let check = |dom: &mut Dom, what: &str, replaced: Option<NodeId>| {
        let reactions = dom.take_reactions();
        let inserted = reactions
            .iter()
            .filter(|r| matches!(r, Reaction::Inserted { root: true, .. }))
            .count();
        assert_eq!(inserted, 2, "{what}: {reactions:?}");
        for r in &reactions {
            match r {
                // (a removed root has its old parent; its descendants are reported with none)
                Reaction::Removed {
                    node,
                    old_parent: Some(_),
                    ..
                } => assert_eq!(Some(*node), replaced, "{what}: {reactions:?}"),
                Reaction::ChildrenChanged { parent } => {
                    assert!(*parent == div || *parent == body, "{what}: {reactions:?}")
                }
                _ => {}
            }
        }
    };
    dom.set_inner_html(div, "<i>x</i><b>y</b>").unwrap();
    check(&mut dom, "innerHTML", Some(old));
    let first = dom.first_child(div).unwrap();
    dom.insert_adjacent_html(div, AdjacentPosition::BeforeEnd, "<i>z</i><b>w</b>")
        .unwrap();
    check(&mut dom, "insertAdjacentHTML", None);
    dom.set_outer_html(first, "<u>1</u><u>2</u>").unwrap();
    check(&mut dom, "outerHTML", Some(first));
    dom.set_html_unsafe(div, "<s>3</s><s>4</s>").unwrap();
    let reactions = dom.take_reactions();
    assert!(
        reactions.iter().all(|r| match r {
            Reaction::ChildrenChanged { parent } => *parent == div,
            _ => true,
        }),
        "setHTMLUnsafe: {reactions:?}"
    );
    assert_eq!(names(&dom, div), ["S", "S"]);
}

/// Attribute values of any length and content read back as set, inline or not.
#[test]
fn attribute_values_short_long_and_unicode() {
    let (mut dom, doc) = html_doc();
    let e = el(&mut dom, doc, "div");
    for v in [
        "",
        "a",
        "exactly-twenty-two-b!!",
        "twenty-three bytes long",
        "ünïcödé ✓",
        "a much longer value that certainly does not fit inline, with ✓ in it",
    ] {
        dom.set_attribute(e, "title", v).unwrap();
        assert_eq!(dom.get_attribute(e, "title"), Some(v));
        let copy = dom.clone_node(e, false).unwrap();
        assert_eq!(dom.get_attribute(copy, "title"), Some(v));
    }
    dom.set_inner_html(e, "<p data-x=\"x&amp;y\" title='∑'></p>")
        .unwrap();
    let p = dom.first_child(e).unwrap();
    assert_eq!(dom.get_attribute(p, "data-x"), Some("x&y"));
    assert_eq!(dom.get_attribute(p, "title"), Some("∑"));
}

/// HTML §13.2.6.4.7 adoption agency with the tree rearranged by script in the middle of the parse
/// (WPT html/syntax/parsing/adoption-agency-reparenting-*-cycle): a move that would make the
/// furthest block a host-including inclusive ancestor of itself drops it instead, and the parse
/// ends without a cycle.
#[test]
fn adoption_agency_never_makes_a_cycle() {
    // (the script's rearrangement for each case: fb detached, then ca put under fb directly, in a
    // shadow root of a host under fb, or in the contents of a template under fb)
    for case in ["direct", "inner-loop", "shadow", "template"] {
        let cell = RefCell::new(Dom::new());
        let doc = cell.borrow_mut().create_document(DocumentKind::Html);
        let mut parser = HtmlParser::new(&cell, doc, ParseOptions::default());
        let start = if case == "inner-loop" {
            "<body><div id=ca><b><i><p id=fb><script>x</script>"
        } else {
            "<body><div id=ca><b><p id=fb><script>x</script>"
        };
        parser.push_str(start);
        let ParseStep::Script(_) = parser.run() else {
            panic!("expected the script stop")
        };
        let (ca, fb) = {
            let mut dom = cell.borrow_mut();
            let find = |dom: &Dom, id: &str| {
                dom.descendants(doc)
                    .find(|&n| dom.attr(n, &ns!(), &local_name!("id")) == Some(id))
                    .unwrap()
            };
            let (ca, fb) = (find(&dom, "ca"), find(&dom, "fb"));
            let body = dom.parent(ca).unwrap();
            dom.append_child(body, fb).unwrap();
            match case {
                "shadow" => {
                    let host = dom.create_html_element(doc, local_name!("span"));
                    dom.append_child(fb, host).unwrap();
                    let root = dom
                        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
                        .unwrap();
                    dom.append_child(root, ca).unwrap();
                }
                "template" => {
                    let t = dom.create_html_element(doc, local_name!("template"));
                    dom.append_child(fb, t).unwrap();
                    let content = dom.template_contents(t).unwrap();
                    let content_doc = dom.node_document(content);
                    dom.adopt(ca, content_doc);
                    dom.append_child(content, ca).unwrap();
                }
                _ => {
                    dom.append_child(fb, ca).unwrap();
                }
            }
            (ca, fb)
        };
        parser.push_str("</b></div>");
        while parser.run() != ParseStep::NeedInput {}
        parser.finish();
        let dom = cell.borrow();
        // The move of the furthest block into the common ancestor was dropped, and so was the
        // furthest block.
        assert!(
            !dom.is_host_including_inclusive_ancestor_of(ca, fb),
            "{case}: the common ancestor does not contain the furthest block"
        );
        assert_ne!(dom.root(fb), doc, "{case}: the furthest block is dropped");
        // The walk over the document ends (a cycle would never end).
        assert!(dom.descendants(doc).count() < 100, "{case}");
    }
}

#[test]
fn tree_order_is_consistent_across_trees() {
    // A boundary point comparison swaps its arguments on `Greater`: a pair that is `Greater` both
    // ways recursed until the stack overflowed, which took the whole browser down (a click on
    // reddit's search box, with a selection's ends in the document and a shadow tree).
    use std::cmp::Ordering::{Greater, Less};
    let (mut dom, doc) = html_doc();
    let html = el(&mut dom, doc, "html");
    dom.append_child(doc, html).unwrap();
    let host = el(&mut dom, doc, "div");
    dom.append_child(html, host).unwrap();
    let shadow = dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    let inner = el(&mut dom, doc, "span");
    dom.append_child(shadow, inner).unwrap();
    let loose = el(&mut dom, doc, "x");
    for (a, b) in [(html, inner), (host, shadow), (html, loose), (inner, loose)] {
        let ab = dom.tree_order(a, b);
        assert!(matches!(ab, Less | Greater), "{ab:?}");
        assert_eq!(dom.tree_order(b, a), ab.reverse());
    }
}

// ----- subtree filters ----------------------------------------------------------------------------------------

#[test]
fn subtree_filters_hold_every_name_inserted_below_them() {
    use crate::filter::{attribute_bits, class_bits, could_match};
    let (mut dom, doc) = html_doc();
    let html = el(&mut dom, doc, "html");
    let div = el(&mut dom, doc, "div");
    assert!(dom.subtree_filter(div).is_empty());
    assert!(
        dom.subtree_filter(doc).is_empty(),
        "only elements have a filter"
    );
    dom.append_child(doc, html).unwrap();
    dom.append_child(html, div).unwrap();
    // (the fast fragment parser, and html5ever for the table)
    dom.set_inner_html(div, "<p class='A  b'><span data-x=1>t</span></p>")
        .unwrap();
    dom.set_inner_html(
        dom.first_child(div).unwrap(),
        "<span data-x=1>t</span><table><tr><td class=c>x</td></tr></table>",
    )
    .unwrap();
    let p = dom.first_child(div).unwrap();
    let span = dom.first_child(p).unwrap();
    let f = dom.subtree_filter(html);
    for needed in [
        class_bits("a"),
        class_bits("B"),
        class_bits("c"),
        attribute_bits("DATA-X"),
        attribute_bits("class"),
    ] {
        assert!(could_match(f, needed));
    }
    assert!(!could_match(dom.subtree_filter(span), class_bits("c")));

    dom.set_attribute(span, "class", "late").unwrap();
    assert!(could_match(dom.subtree_filter(html), class_bits("late")));
    let attr = dom.get_attribute_node(span, "class").unwrap();
    dom.set_attr_value(attr, "via-attr");
    assert!(could_match(
        dom.subtree_filter(html),
        class_bits("via-attr")
    ));
    dom.set_attribute(span, "data-new", "").unwrap();
    assert!(could_match(
        dom.subtree_filter(p),
        attribute_bits("data-new")
    ));

    // A removal leaves the bits (a filter may hold names its subtree no longer has).
    dom.remove_node(p);
    assert!(could_match(dom.subtree_filter(div), class_bits("late")));
    dom.check_subtree_filters(doc);

    let copy = dom.clone_node(p, true).unwrap();
    assert!(could_match(dom.subtree_filter(copy), class_bits("c")));
    dom.check_subtree_filters(copy);
    dom.append_child(div, copy).unwrap();
    dom.check_subtree_filters(doc);
}

#[test]
fn a_tree_may_hold_slots_only_once_a_slot_element_is_in_it() {
    let (mut dom, doc) = html_doc();
    let host = el(&mut dom, doc, "div");
    let sr = dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    assert!(!dom.may_hold_slots(sr));
    let div = el(&mut dom, doc, "div");
    let span = el(&mut dom, doc, "span");
    dom.set_attribute(span, "name", "a").unwrap();
    dom.set_attribute(span, "class", "slot").unwrap();
    dom.append_child(div, span).unwrap();
    let text = dom.create_text_node(doc, "slot");
    dom.append_child(sr, text).unwrap();
    dom.append_child(sr, div).unwrap();
    assert!(
        !dom.may_hold_slots(sr),
        "elements, names and text that are not slots"
    );
    assert!(!dom.may_hold_slots(text));

    // A slot below an element already in the tree reaches its root's child.
    let slot = el(&mut dom, doc, "slot");
    assert!(dom.may_hold_slots(slot));
    dom.append_child(span, slot).unwrap();
    assert!(dom.may_hold_slots(sr));
    assert!(dom.may_hold_slots(div));
    dom.check_subtree_filters(sr);

    // Copies and parsed slots have it from their creation.
    let copy = dom.clone_node(div, true).unwrap();
    assert!(dom.may_hold_slots(copy));
    let frag = dom.parse_fragment(
        host,
        "<p><b><slot name=x></slot></b></p>",
        false,
        ParseOptions::default(),
    );
    assert!(dom.may_hold_slots(frag));
    let host2 = el(&mut dom, doc, "section");
    let sr2 = dom
        .attach_shadow(host2, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    let p = dom.first_child(frag).unwrap();
    dom.append_child(sr2, p).unwrap();
    assert!(dom.may_hold_slots(sr2));
    dom.check_subtree_filters(sr2);

    // A slot inside a shadow tree within the tree is that tree's, not this one's.
    let inner_host = el(&mut dom, doc, "div");
    let inner = dom
        .attach_shadow(inner_host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    let inner_slot = el(&mut dom, doc, "slot");
    dom.append_child(inner, inner_slot).unwrap();
    let host3 = el(&mut dom, doc, "div");
    let sr3 = dom
        .attach_shadow(host3, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    dom.append_child(sr3, inner_host).unwrap();
    assert!(!dom.may_hold_slots(sr3));
    assert!(dom.may_hold_slots(inner));
}

#[test]
fn connected_base_elements_are_counted_per_document() {
    let (mut dom, doc) = html_doc();
    let html = el(&mut dom, doc, "html");
    dom.append_child(doc, html).unwrap();
    let base = el(&mut dom, doc, "base");
    assert!(
        !dom.has_connected_base(doc),
        "a created base is not connected"
    );
    let div = el(&mut dom, doc, "div");
    dom.append_child(div, base).unwrap();
    assert!(!dom.has_connected_base(doc));
    dom.append_child(html, div).unwrap();
    assert!(dom.has_connected_base(doc), "connected with its parent");
    dom.set_inner_html(div, "<p><base href=x></p>").unwrap();
    assert!(
        dom.has_connected_base(doc),
        "the parsed one, the first one removed"
    );
    let p = dom.first_child(div).unwrap();
    dom.remove_node(p);
    assert!(!dom.has_connected_base(doc), "both removed");
    let other = dom.create_document(DocumentKind::Html);
    let host = el(&mut dom, other, "html");
    dom.append_child(other, host).unwrap();
    dom.append_child(host, p).unwrap();
    assert!(dom.has_connected_base(other) && !dom.has_connected_base(doc));
    dom.remove_node(p);
    assert!(!dom.has_connected_base(other));
}

#[test]
fn the_id_index_finds_the_first_in_tree_order_of_the_roots_tree() {
    let (mut dom, doc) = html_doc();
    let html = el(&mut dom, doc, "html");
    dom.append_child(doc, html).unwrap();
    let body = el(&mut dom, doc, "div");
    dom.append_child(html, body).unwrap();
    let detached = el(&mut dom, doc, "div");
    dom.set_attribute(detached, "id", "x").unwrap();
    assert_eq!(
        dom.indexed_element_by_id(doc, "x"),
        Some(None),
        "not connected"
    );
    assert_eq!(
        dom.indexed_element_by_id(detached, "x"),
        None,
        "the caller walks"
    );
    dom.set_inner_html(body, "<p id=a><b id=x></b></p><i id=x name=n></i>")
        .unwrap();
    let p = dom.first_child(body).unwrap();
    let b = dom.first_child(p).unwrap();
    let i = dom.next_sibling(p).unwrap();
    assert_eq!(dom.indexed_element_by_id(doc, "x"), Some(Some(b)));
    // A later element inserted first in tree order wins.
    dom.insert_before(body, detached, Some(p)).unwrap();
    assert_eq!(dom.indexed_element_by_id(doc, "x"), Some(Some(detached)));
    dom.remove_node(detached);
    dom.set_attribute(b, "id", "y").unwrap();
    assert_eq!(dom.indexed_element_by_id(doc, "x"), Some(Some(i)));
    assert_eq!(dom.indexed_element_by_id(doc, "y"), Some(Some(b)));
    assert!(dom.may_have_id_or_name(doc, "n") && dom.may_have_id_or_name(doc, "a"));
    dom.remove_attribute(i, "name");
    assert!(!dom.may_have_id_or_name(doc, "n"));
    dom.remove_node(p);
    assert_eq!(dom.indexed_element_by_id(doc, "y"), Some(None));
    assert!(!dom.may_have_id_or_name(doc, "a"));
    // A shadow tree's elements are found from their own root only.
    let host = el(&mut dom, doc, "div");
    dom.append_child(body, host).unwrap();
    let shadow = dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    dom.set_inner_html(shadow, "<s id=z></s>").unwrap();
    let s = dom.first_child(shadow).unwrap();
    assert_eq!(dom.indexed_element_by_id(doc, "z"), Some(None));
    assert_eq!(dom.indexed_element_by_id(shadow, "z"), Some(Some(s)));
    dom.remove_node(host);
    assert_eq!(dom.indexed_element_by_id(doc, "x"), Some(Some(i)));
    assert!(!dom.may_have_id_or_name(doc, "z"));
}

/// The parser appends each text run to the Text node before it ("insert a character"); a script's
/// text arrives as one run per line and one per line break. Appending must cost the run, not the node:
/// four times the text takes about four times as long, where measuring the node on each append made it
/// sixteen.
#[test]
fn parsing_a_long_non_ascii_text_node_is_linear() {
    fn parse(lines: usize) -> std::time::Duration {
        let text = "const s = \"\u{e9}\u{2014}\u{1F600}\";\n".repeat(lines);
        let html = format!("<script type=text/plain>{text}</script>");
        let mut best = std::time::Duration::MAX;
        for _ in 0..3 {
            let mut dom = Dom::new();
            let start = std::time::Instant::now();
            let doc = dom.parse_html_str(&html, ParseOptions::default());
            best = best.min(start.elapsed());
            let head = dom.first_child(dom.document_element(doc).unwrap()).unwrap();
            let script = dom.first_child(head).unwrap();
            let t = dom.first_child(script).unwrap();
            assert_eq!(dom.next_sibling(t), None, "one Text node");
            assert_eq!(dom.character_data(t), Some(text.as_str()));
        }
        best
    }
    let small = parse(4_000);
    let large = parse(16_000);
    assert!(
        large < small * 8,
        "4x the text took {:.1}x as long ({small:?} -> {large:?})",
        large.as_secs_f64() / small.as_secs_f64()
    );
}
