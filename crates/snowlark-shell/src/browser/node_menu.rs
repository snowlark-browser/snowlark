// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The element tree's context menu (a right-click on a node of the Elements panel): the rows of
//! Chromium's, each enabled only when it does something for the node chosen. A row that is not
//! done yet stays disabled.

use snowlark_dom::NodeId;
use snowlark_page::devtools_node::CopyKind;

use super::commands::Command;
use crate::devtools::protocol::{EditKind, NodeOp};
use crate::devtools::{NodeKind, NodeSub, NodeTarget};
use crate::menu::Entry;

/// The main menu for `node`.
pub fn entries(target: &NodeTarget, can_paste: bool) -> Vec<Entry<Command>> {
    let (node, kind) = (target.node, target.kind);
    let element = kind == NodeKind::Element;
    let op = |label: &str, op: NodeOp, on: bool| {
        Entry::item(label, Command::DevtoolsNode(node, op)).enabled(on)
    };
    let sub = |label: &str, sub: NodeSub, on: bool| {
        Entry::item(label, Command::DevtoolsSub(node, kind, target.forced, sub))
            .submenu()
            .enabled(on)
    };
    vec![
        op(
            "Add attribute",
            NodeOp::BeginEdit(EditKind::AddAttribute),
            element,
        ),
        op(
            "Edit attribute",
            NodeOp::BeginEdit(EditKind::Attribute(target.attr.clone().unwrap_or_default())),
            element && target.attr.is_some(),
        ),
        // (a text or comment node edits its text; the document element's markup can't be replaced)
        match kind {
            NodeKind::Text | NodeKind::Comment => {
                op("Edit text", NodeOp::BeginEdit(EditKind::Text), true)
            }
            _ => op(
                "Edit as HTML",
                NodeOp::BeginEdit(EditKind::Html),
                element && !target.root,
            ),
        },
        op("Duplicate element", NodeOp::Duplicate, element),
        op("Delete element", NodeOp::Delete, true),
        Entry::Separator,
        op("Cut", NodeOp::Cut, element && !target.root),
        sub("Copy", NodeSub::Copy, true),
        op("Paste", NodeOp::Paste(String::new()), element && can_paste),
        Entry::Separator,
        op("Hide element", NodeOp::ToggleHide, element),
        sub("Force state", NodeSub::ForceState, element),
        sub("Break on", NodeSub::BreakOn, false),
        Entry::Separator,
        op("Expand recursively", NodeOp::ExpandRecursively, element),
        op("Collapse children", NodeOp::CollapseChildren, element),
        op(
            "Switch to accessibility tree",
            NodeOp::AccessibilityTree,
            false,
        ),
        op("Capture node screenshot", NodeOp::Screenshot, false),
        op("Scroll into view", NodeOp::ScrollIntoView, element),
        op("Focus", NodeOp::Focus, element),
        sub("Badge settings", NodeSub::Badges, false),
        Entry::Separator,
        op("Store as global variable", NodeOp::StoreGlobal, true),
    ]
}

/// The rows of a submenu.
pub fn sub_entries(node: NodeId, kind: NodeKind, forced: u8, sub: NodeSub) -> Vec<Entry<Command>> {
    match sub {
        NodeSub::Copy => {
            let element = kind == NodeKind::Element;
            // (a text or comment node has no selector, JS path or styles; a doctype has no XPath)
            let xpath = kind != NodeKind::Doctype;
            let copy = |label: &str, what: CopyKind, on: bool| {
                Entry::item(label, Command::DevtoolsNode(node, NodeOp::Copy(what))).enabled(on)
            };
            vec![
                copy("Copy element", CopyKind::OuterHtml, true),
                Entry::Separator,
                copy("Copy outerHTML", CopyKind::OuterHtml, true),
                copy("Copy selector", CopyKind::Selector, element),
                copy("Copy JS path", CopyKind::JsPath, element),
                Entry::item(
                    "Copy styles",
                    Command::DevtoolsNode(node, NodeOp::CopyStyles),
                )
                .enabled(false),
                copy("Copy XPath", CopyKind::XPath, xpath),
                copy("Copy full XPath", CopyKind::FullXPath, xpath),
            ]
        }
        NodeSub::ForceState => snowlark_page::devtools_node::FORCE_STATES
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let on = forced >> i & 1 == 1;
                Entry::item(
                    name,
                    Command::DevtoolsNode(node, NodeOp::ForceState(i as u8, !on)),
                )
                .checked(on)
            })
            .collect(),
        NodeSub::BreakOn => [
            ("subtree modifications", NodeOp::BreakOnSubtree),
            ("attribute modifications", NodeOp::BreakOnAttributes),
            ("node removal", NodeOp::BreakOnRemoval),
        ]
        .into_iter()
        .map(|(label, op)| {
            Entry::item(label, Command::DevtoolsNode(node, op))
                .checked(false)
                .enabled(false)
        })
        .collect(),
        NodeSub::Badges => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> NodeId {
        NodeId::from_bits(1 << 32 | 1).unwrap()
    }

    fn target(kind: NodeKind) -> NodeTarget {
        NodeTarget {
            node: id(),
            kind,
            attr: None,
            root: false,
            forced: 0,
        }
    }

    fn labels(e: &[Entry<Command>]) -> Vec<String> {
        e.iter()
            .map(|e| match e {
                Entry::Item { label, .. } => label.clone(),
                Entry::Separator => "|".to_owned(),
            })
            .collect()
    }

    fn enabled(e: &[Entry<Command>], name: &str) -> bool {
        e.iter()
            .any(|e| matches!(e, Entry::Item { label, enabled: true, .. } if label == name))
    }

    #[test]
    fn the_rows_are_chromiums_in_order() {
        let m = entries(&target(NodeKind::Element), true);
        assert_eq!(
            labels(&m).join(", "),
            "Add attribute, Edit attribute, Edit as HTML, Duplicate element, Delete element, |, \
             Cut, Copy, Paste, |, Hide element, Force state, Break on, |, Expand recursively, \
             Collapse children, Switch to accessibility tree, Capture node screenshot, \
             Scroll into view, Focus, Badge settings, |, Store as global variable"
        );
        let c = sub_entries(id(), NodeKind::Element, 0, NodeSub::Copy);
        assert_eq!(
            labels(&c).join(", "),
            "Copy element, |, Copy outerHTML, Copy selector, Copy JS path, Copy styles, \
             Copy XPath, Copy full XPath"
        );
        let f = sub_entries(id(), NodeKind::Element, 0, NodeSub::ForceState);
        assert_eq!(
            labels(&f).join(", "),
            ":active, :hover, :focus, :visited, :focus-within, :focus-visible, :target"
        );
        let b = sub_entries(id(), NodeKind::Element, 0, NodeSub::BreakOn);
        assert_eq!(
            labels(&b).join(", "),
            "subtree modifications, attribute modifications, node removal"
        );
    }

    #[test]
    fn rows_without_a_target_are_disabled_as_in_chromium() {
        let el = entries(&target(NodeKind::Element), true);
        for row in [
            "Duplicate element",
            "Delete element",
            "Hide element",
            "Expand recursively",
            "Collapse children",
            "Scroll into view",
            "Focus",
            "Store as global variable",
            "Copy",
        ] {
            assert!(enabled(&el, row), "{row} for an element");
        }
        let text = entries(&target(NodeKind::Text), true);
        for row in [
            "Duplicate element",
            "Hide element",
            "Expand recursively",
            "Collapse children",
            "Scroll into view",
            "Focus",
        ] {
            assert!(!enabled(&text, row), "{row} for text");
        }
        assert!(enabled(&text, "Delete element") && enabled(&text, "Store as global variable"));
        let copy = sub_entries(id(), NodeKind::Text, 0, NodeSub::Copy);
        assert!(enabled(&copy, "Copy outerHTML") && enabled(&copy, "Copy XPath"));
        assert!(!enabled(&copy, "Copy selector") && !enabled(&copy, "Copy JS path"));
        let doc = sub_entries(id(), NodeKind::Doctype, 0, NodeSub::Copy);
        assert!(!enabled(&doc, "Copy XPath") && enabled(&doc, "Copy outerHTML"));
    }

    #[test]
    fn editing_rows_follow_what_was_clicked() {
        let mut t = target(NodeKind::Element);
        let m = entries(&t, true);
        assert!(enabled(&m, "Add attribute") && enabled(&m, "Edit as HTML"));
        assert!(
            !enabled(&m, "Edit attribute"),
            "no attribute under the pointer"
        );
        t.attr = Some("class".into());
        let m = entries(&t, true);
        assert!(enabled(&m, "Edit attribute"));
        assert!(m.iter().any(|e| matches!(e, Entry::Item { action: Command::DevtoolsNode(_, NodeOp::BeginEdit(EditKind::Attribute(n))), .. } if n == "class")));
        // The document element's markup can't be replaced.
        t.root = true;
        assert!(!enabled(&entries(&t, true), "Edit as HTML"));
        // Text edits its text; a doctype edits nothing.
        let m = entries(&target(NodeKind::Text), true);
        assert!(enabled(&m, "Edit text") && !enabled(&m, "Add attribute"));
        let m = entries(&target(NodeKind::Doctype), true);
        assert!(!enabled(&m, "Edit as HTML") && !enabled(&m, "Edit text"));
    }

    #[test]
    fn cut_paste_and_force_state_follow_the_target_and_the_clipboard() {
        let mut t = target(NodeKind::Element);
        let m = entries(&t, false);
        assert!(enabled(&m, "Cut") && enabled(&m, "Force state"));
        assert!(!enabled(&m, "Paste"), "nothing to paste");
        assert!(enabled(&entries(&t, true), "Paste"));
        t.root = true;
        assert!(
            !enabled(&entries(&t, true), "Cut"),
            "the document element stays"
        );
        let m = entries(&target(NodeKind::Text), true);
        assert!(!enabled(&m, "Cut") && !enabled(&m, "Paste") && !enabled(&m, "Force state"));
        // The states offered carry a check where they are forced, and choosing one toggles it.
        let f = sub_entries(id(), NodeKind::Element, 0b0001010, NodeSub::ForceState);
        let on: Vec<String> = f
            .iter()
            .filter_map(|e| match e {
                Entry::Item {
                    label,
                    checked: Some(true),
                    ..
                } => Some(label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(on, [":hover", ":visited"]);
        assert!(f.iter().any(|e| matches!(
            e,
            Entry::Item { label, action: Command::DevtoolsNode(_, NodeOp::ForceState(1, false)), .. } if label == ":hover"
        )));
        assert!(f.iter().any(|e| matches!(
            e,
            Entry::Item { label, action: Command::DevtoolsNode(_, NodeOp::ForceState(2, true)), .. } if label == ":focus"
        )));
        assert!(f
            .iter()
            .all(|e| matches!(e, Entry::Item { enabled: true, .. })));
    }
}
