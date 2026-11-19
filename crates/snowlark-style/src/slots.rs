// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Named slot assignment (DOM §4.2.2.3), which the flat tree that style walks is built from. `snowlark-dom`
//! has no slot assignment yet, so the style crate computes it from the tree: "find a slot" and "find
//! slottables" for shadow roots whose slot assignment is "named". Manual slot assignment
//! (`HTMLSlotElement.assign()`) comes from the page layer as each slot's assigned nodes
//! ([`crate::StyleEngine::set_manual_slot_assignment`]).

use rustc_hash::FxHashMap;
use snowlark_dom::{local_name, ns, Dom, NodeData, NodeId, SlotAssignmentMode};

/// A slottable's name (DOM §4.2.2.1): an element's `slot` attribute, else the empty string.
fn slottable_name(dom: &Dom, node: NodeId) -> &str {
    if dom.is_element(node) {
        dom.attr(node, &ns!(), &local_name!("slot")).unwrap_or("")
    } else {
        ""
    }
}

/// Whether `node` is a slottable (an element or a Text node).
fn is_slottable(dom: &Dom, node: NodeId) -> bool {
    matches!(dom.data(node), NodeData::Element(_) | NodeData::Text(_))
}

/// The slots of `shadow` in tree order, keyed by name (the first slot of each name wins).
pub(crate) fn slots_by_name(dom: &Dom, shadow: NodeId) -> FxHashMap<&str, NodeId> {
    let mut map = FxHashMap::default();
    if !dom.may_hold_slots(shadow) {
        return map;
    }
    for node in dom.descendants(shadow) {
        if dom.is_html_element_named(node, &local_name!("slot")) {
            let name = dom.attr(node, &ns!(), &local_name!("name")).unwrap_or("");
            map.entry(name).or_insert(node);
        }
    }
    map
}

/// DOM "find a slot" for `slottable` (closed shadow roots included).
pub(crate) fn find_slot(dom: &Dom, slottable: NodeId) -> Option<NodeId> {
    let parent = dom.parent(slottable)?;
    let shadow = dom.shadow_root(parent)?;
    let data = dom.shadow_root_data(shadow)?;
    if data.slot_assignment() == SlotAssignmentMode::Manual || !dom.may_hold_slots(shadow) {
        return None;
    }
    let name = slottable_name(dom, slottable);
    dom.descendants(shadow).find(|&n| {
        dom.is_html_element_named(n, &local_name!("slot"))
            && dom.attr(n, &ns!(), &local_name!("name")).unwrap_or("") == name
    })
}

/// The slot assignment of one shadow tree: each slot's assigned nodes ("find slottables", in tree order)
/// and, for every child of the host, its slot.
pub(crate) struct Assignment {
    pub slots: Vec<(NodeId, Vec<NodeId>)>,
    pub children: Vec<(NodeId, Option<NodeId>)>,
}

/// Computes the slot assignment of `shadow` ("assign slottables for a tree", DOM §4.2.2.4).
pub(crate) fn assign(
    dom: &Dom,
    shadow: NodeId,
    manual_lists: &FxHashMap<NodeId, Vec<NodeId>>,
) -> Assignment {
    let host = dom.host(shadow).expect("shadow root without host");
    let manual = dom
        .shadow_root_data(shadow)
        .is_some_and(|d| d.slot_assignment() == SlotAssignmentMode::Manual);
    let by_name = if manual {
        FxHashMap::default()
    } else {
        slots_by_name(dom, shadow)
    };
    let mut slots: Vec<(NodeId, Vec<NodeId>)> =
        by_name.values().map(|&s| (s, Vec::new())).collect();
    // Every slot of the tree, including the shadowed duplicates, gets an (empty) entry so stale
    // assignments are cleared. (A tree without slots assigns its host's children to none.)
    if dom.may_hold_slots(shadow) {
        for node in dom.descendants(shadow) {
            if dom.is_html_element_named(node, &local_name!("slot"))
                && !slots.iter().any(|s| s.0 == node)
            {
                slots.push((node, Vec::new()));
            }
        }
    }
    let mut children = Vec::new();
    if manual {
        // "Find slottables" for a manual slot: its manually assigned nodes that are children of
        // the host, in assignment order; a node goes to the first slot (in tree order) listing it.
        let mut taken: Vec<NodeId> = Vec::new();
        for (slot, nodes) in slots.iter_mut() {
            if let Some(list) = manual_lists.get(slot) {
                for &n in list {
                    if dom.is_alive(n)
                        && dom.parent(n) == Some(host)
                        && is_slottable(dom, n)
                        && !taken.contains(&n)
                    {
                        nodes.push(n);
                        taken.push(n);
                    }
                }
            }
        }
        for child in dom.children(host) {
            if !is_slottable(dom, child) {
                continue;
            }
            let slot = slots.iter().find(|s| s.1.contains(&child)).map(|s| s.0);
            children.push((child, slot));
        }
        return Assignment { slots, children };
    }
    for child in dom.children(host) {
        if !is_slottable(dom, child) {
            continue;
        }
        let slot = by_name.get(slottable_name(dom, child)).copied();
        if let Some(slot) = slot {
            if let Some(entry) = slots.iter_mut().find(|s| s.0 == slot) {
                entry.1.push(child);
            }
        }
        children.push((child, slot));
    }
    Assignment { slots, children }
}
