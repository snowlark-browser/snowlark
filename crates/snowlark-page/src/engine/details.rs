// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.11.1 the `details` element: the attribute change and insertion steps (the notification
//! task steps with their coalesced `toggle` event, the `name` exclusivity group), the summary
//! activation behavior and the ancestor details revealing algorithm.
//!
//! The rendering (the closed content's `content-visibility: hidden`, the summary and content slots)
//! is the box builder's: see `snowlark-layout`'s `details_parts`.

use snowlark_bindings::rt::Cx;
use snowlark_dom::{local_name, ns, LocalName, NodeId};

use super::Engine;

impl Engine {
    fn is_details(&self, n: NodeId) -> bool {
        self.dom().is_html_element_named(n, &local_name!("details"))
    }

    /// The `details` elements in `el`'s name group (HTML: the other `details` elements in the same
    /// tree with a non-empty `name` equal to `el`'s), in tree order. Elements whose insertion the
    /// current reaction batch has not reached yet are not in the tree yet as far as these steps go
    /// (a parser inserting several open `details` keeps the first open, not the last).
    fn details_name_group(&self, el: NodeId) -> Vec<NodeId> {
        let dom = self.dom();
        let name = match dom.attr(el, &ns!(), &local_name!("name")) {
            Some(n) if !n.is_empty() => n.to_owned(),
            _ => return Vec::new(),
        };
        let pending = self.details_pending.borrow();
        if pending.contains(&el) {
            return Vec::new();
        }
        let root = dom.root(el);
        dom.descendants(root)
            .filter(|&n| {
                n != el
                    && !pending.contains(&n)
                    && dom.is_html_element_named(n, &local_name!("details"))
                    && dom.attr(n, &ns!(), &local_name!("name")) == Some(name.as_str())
            })
            .collect()
    }

    fn details_open(&self, el: NodeId) -> bool {
        self.dom().attr(el, &ns!(), &local_name!("open")).is_some()
    }

    /// "Ensure details exclusivity by closing the given element if needed".
    fn details_close_if_group_open(&self, cx: &mut Cx, el: NodeId) {
        if !self.details_open(el) {
            return;
        }
        let other_open = self
            .details_name_group(el)
            .into_iter()
            .any(|d| self.details_open(d));
        if other_open {
            self.remove_attr(cx, el, "open");
        }
    }

    /// "Ensure details exclusivity by closing other elements if needed".
    fn details_close_group_others(&self, cx: &mut Cx, el: NodeId) {
        let open = self
            .details_name_group(el)
            .into_iter()
            .find(|&d| self.details_open(d));
        if let Some(d) = open {
            self.remove_attr(cx, d, "open");
        }
    }

    /// The attribute change steps of a `details` element (null namespace only).
    pub(crate) fn details_attribute_changed(
        &self,
        cx: &mut Cx,
        el: NodeId,
        local: &LocalName,
        old_value: Option<&str>,
        new_value: Option<&str>,
    ) {
        if !self.is_details(el) {
            return;
        }
        match &**local {
            "name" => self.details_close_if_group_open(cx, el),
            "open" => {
                if old_value.is_some() != new_value.is_some() {
                    // The details notification task steps.
                    let (old, new) = if new_value.is_some() {
                        ("closed", "open")
                    } else {
                        ("open", "closed")
                    };
                    self.queue_dialog_toggle(el, old, new, None);
                }
                if old_value.is_none() && new_value.is_some() {
                    self.details_close_group_others(cx, el);
                }
            }
            _ => {}
        }
    }

    /// The insertion steps of a `details` element.
    pub(crate) fn details_inserted(&self, cx: &mut Cx, el: NodeId) {
        self.details_pending.borrow_mut().remove(&el);
        self.details_close_if_group_open(cx, el);
    }

    /// Whether `summary` is "a summary for its parent details": its parent is a `details` and it is
    /// that parent's first `summary` child.
    pub(crate) fn is_summary_for_parent_details(&self, summary: NodeId) -> Option<NodeId> {
        let dom = self.dom();
        let parent = dom.parent(summary)?;
        if !dom.is_html_element_named(parent, &local_name!("details")) {
            return None;
        }
        let first = dom
            .element_children(parent)
            .find(|&c| dom.is_html_element_named(c, &local_name!("summary")));
        (first == Some(summary)).then_some(parent)
    }

    /// The summary element's activation behavior.
    pub(crate) fn summary_activation(&self, cx: &mut Cx, summary: NodeId) {
        let Some(details) = self.is_summary_for_parent_details(summary) else {
            return;
        };
        if self.details_open(details) {
            self.remove_attr(cx, details, "open");
        } else {
            self.set_attr(cx, details, "open", "");
        }
    }

    /// The closed `details` elements that hide `node`: its ancestors (through shadow hosts) that hold
    /// it in their content, i.e. unless `node` is within that details' first summary.
    pub(crate) fn closed_details_hiding(&self, node: NodeId) -> Vec<NodeId> {
        let dom = self.dom();
        let mut out = Vec::new();
        let mut child = node;
        loop {
            let parent = match dom.parent(child) {
                Some(p) => p,
                None => match dom.host(child) {
                    Some(h) => h,
                    None => break,
                },
            };
            if dom.is_html_element_named(parent, &local_name!("details"))
                && dom.parent(child) == Some(parent)
                && dom.attr(parent, &ns!(), &local_name!("open")).is_none()
            {
                let first_summary = dom
                    .element_children(parent)
                    .find(|&c| dom.is_html_element_named(c, &local_name!("summary")));
                if first_summary != Some(child) {
                    out.push(parent);
                }
            }
            child = parent;
        }
        out
    }

    /// The ancestor details revealing algorithm: opens each `details` that hides `node`. Returns
    /// whether any was opened.
    pub(crate) fn reveal_ancestor_details(&self, cx: &mut Cx, node: NodeId) -> bool {
        let to_open = self.closed_details_hiding(node);
        let any = !to_open.is_empty();
        for d in to_open {
            self.set_attr(cx, d, "open", "");
        }
        any
    }
}
