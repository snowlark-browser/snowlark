// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML Editing APIs §7 "Justifying the selection": the `justifyCenter`, `justifyFull`,
//! `justifyLeft` and `justifyRight` commands, with their states, values and indeterminacy (an
//! element's "alignment value").

use snowlark_bindings::rt::Cx;
use snowlark_dom::NodeId;

use super::nodes::Item;
use crate::engine::cssom::Pseudo;
use crate::engine::Engine;

/// The alignment a justify command sets.
pub(super) fn alignment_of(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "justifycenter" => "center",
        "justifyfull" => "justify",
        "justifyleft" => "left",
        "justifyright" => "right",
        _ => return None,
    })
}

impl Engine {
    /// The resolved value of `property` for a connected element ("" otherwise).
    fn ec_resolved_of(&self, el: NodeId, property: &str) -> String {
        let ok = {
            let dom = self.dom();
            dom.is_alive(el) && dom.is_element(el) && dom.is_connected(el)
        };
        if ok {
            self.computed_value(el, Pseudo::None, property)
        } else {
            String::new()
        }
    }

    /// "The alignment value" of a node.
    fn ec_alignment_value(&self, node: NodeId) -> &'static str {
        let mut cur = Some(node);
        while let Some(n) = cur {
            if self.dom().is_element(n) && !matches!(self.ec_display(n).as_str(), "inline" | "none")
            {
                break;
            }
            cur = self.ec_parent(n);
        }
        let Some(el) = cur.filter(|&n| self.dom().is_element(n)) else {
            return "left";
        };
        let rtl = self.ec_resolved_of(el, "direction") == "rtl";
        let align = self.ec_resolved_of(el, "text-align");
        // (`center` and `align` give the legacy `-moz-center`, `-moz-left` and `-moz-right`)
        let align = align
            .strip_prefix("-moz-")
            .or_else(|| align.strip_prefix("-webkit-"))
            .unwrap_or(&align);
        match align {
            "start" | "auto" | "" => {
                if rtl {
                    "right"
                } else {
                    "left"
                }
            }
            "end" => {
                if rtl {
                    "left"
                } else {
                    "right"
                }
            }
            "center" => "center",
            "justify" => "justify",
            "right" => "right",
            _ => "left",
        }
    }

    /// The visible editable childless nodes contained in the block-extended active range.
    fn ec_justify_leaves(&self, doc: NodeId) -> Option<Vec<NodeId>> {
        let r = self.ec_block_extend(self.ec_active(doc)?);
        Some(self.ec_contained_nodes(r, true, |e, n| {
            e.ec_editable(n) && e.ec_visible(n) && !e.dom().has_child_nodes(n)
        }))
    }

    /// `queryCommandState` of a justify command.
    pub(super) fn ec_justify_state(&self, doc: NodeId, alignment: &str) -> bool {
        self.ec_justify_leaves(doc).is_some_and(|nodes| {
            !nodes.is_empty()
                && nodes
                    .iter()
                    .all(|&n| self.ec_alignment_value(n) == alignment)
        })
    }

    /// `queryCommandIndeterm` of a justify command.
    pub(super) fn ec_justify_indeterm(&self, doc: NodeId, alignment: &str) -> bool {
        self.ec_justify_leaves(doc).is_some_and(|nodes| {
            nodes
                .iter()
                .any(|&n| self.ec_alignment_value(n) == alignment)
                && nodes
                    .iter()
                    .any(|&n| self.ec_alignment_value(n) != alignment)
        })
    }

    /// `queryCommandValue` of a justify command.
    pub(super) fn ec_justify_value(&self, doc: NodeId) -> String {
        match self.ec_justify_leaves(doc) {
            None => String::new(),
            Some(nodes) => nodes
                .first()
                .map_or("left", |&n| self.ec_alignment_value(n))
                .to_owned(),
        }
    }

    /// "Justify the selection" with `alignment`.
    pub(super) fn ec_justify_selection(&self, cx: &mut Cx, doc: NodeId, alignment: &str) {
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        let r = self.ec_block_extend(r);
        let elements = self.ec_contained_nodes(r, true, |e, n| {
            e.dom().is_element(n)
                && e.ec_editable(n)
                && (e.dom().has_attribute(n, "align")
                    || !e.ec_style_value(n, "text-align").is_empty()
                    || e.ec_is(n, &["center"]))
        });
        for el in elements {
            if !self.dom().is_alive(el) {
                continue;
            }
            self.ec_remove_attribute(cx, el, "align");
            if !self.ec_style_value(el, "text-align").is_empty() {
                self.ec_set_style(cx, el, "text-align", "");
            }
            let no_attrs = !self.dom().has_attributes(el);
            if self.ec_is(el, &["div", "span", "center"]) && no_attrs {
                self.ec_remove_preserving_descendants(cx, el);
            } else if self.ec_is(el, &["center"]) {
                self.ec_set_tag_name(cx, el, "div");
            }
        }
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        let r = self.ec_block_extend(r);
        let mut list = self.ec_contained_nodes(r, false, |e, n| {
            e.ec_editable(n)
                && e.ec_allowed_child(Item::Node(n), Item::Name("div"))
                && e.ec_alignment_value(n) != alignment
        });
        list.reverse();
        while let Some(first) = list.pop() {
            let mut sublist = vec![first];
            while let Some(&next) = list.last() {
                if self.dom().next_sibling(*sublist.last().unwrap()) == Some(next) {
                    sublist.push(next);
                    list.pop();
                } else {
                    break;
                }
            }
            let criteria = |e: &Engine, n: NodeId| {
                e.ec_is(n, &["div"]) && {
                    let attrs: Vec<(String, String)> = {
                        let dom = e.dom();
                        dom.attrs(n)
                            .iter()
                            .map(|a| (a.qualified_name().into_owned(), a.value().to_owned()))
                            .collect()
                    };
                    attrs.iter().all(|(name, value)| {
                        (name == "align" && value.eq_ignore_ascii_case(alignment))
                            || (name == "style"
                                && e.ec_style_value(n, "text-align") == alignment
                                && e.with_inline(n, |b| b.len()) == 1)
                    })
                }
            };
            self.ec_wrap(cx, sublist, &criteria, &|e, _| {
                let div = e.ec_create(doc, "div");
                let _ =
                    e.dom_mut()
                        .set_attribute(div, "style", &format!("text-align: {alignment}"));
                Some(div)
            });
        }
    }
}
