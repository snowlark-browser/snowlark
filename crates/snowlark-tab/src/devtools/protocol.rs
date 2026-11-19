// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the window's developer tools and a tab's thread say to each other
//! ([`crate::tab::TabCommand::Devtools`], [`crate::tab::TabEvent::Devtools`]).

use serde::{Deserialize, Serialize};
use snowlark_dom::NodeId;
use snowlark_page::devtools::{ConsoleEntry, DomRow, Highlight, NodeDetails, ValueKind};
use snowlark_page::devtools_node::CopyKind;
use snowlark_paint::{Point, Ticks};

/// From the window to a tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DevtoolsCommand {
    /// The tools opened (record the network, keep the element tree current) or closed.
    Attach(bool),
    /// Evaluate a console input line; the result comes back with this number.
    Eval(u64, String),
    /// A row of the console's context menu on the result of the evaluation with this number.
    Result(u64, ResultOp),
    /// The console was cleared: the results kept for [`DevtoolsCommand::Result`] go (Chrome
    /// releases the console's object group).
    ClearResults,
    /// The element tree's rows with these elements expanded.
    Tree(Vec<NodeId>),
    /// The selected element (its details come back, and it is highlighted while nothing is
    /// hovered).
    Select(Option<NodeId>),
    /// The element under the pointer in the tree (highlighted in the page).
    Hover(Option<NodeId>),
    /// Pick mode on or off: the pointer over the page highlights what is under it and a click
    /// selects it, without the page seeing the pointer.
    Pick(bool),
    /// Select the element at this viewport point (CSS px), as "Inspect" does.
    InspectAt(Point),
    /// A row of the element tree's context menu, done to this node.
    Node(NodeId, NodeOp),
}

/// What the element tree's context menu does to a node (plain data, so it can cross a process
/// boundary).
/// What an in-place edit in the element tree changes.
/// What the console's context menu does to a result (`SourcesPanel.#appendRemoteObjectItems`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResultOp {
    /// "Copy string contents" or "Copy object": the text comes back as
    /// [`DevtoolsEvent::Clipboard`].
    Copy,
    /// "Store as global variable": [`DevtoolsEvent::Stored`] comes back.
    Store,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditKind {
    /// The element's markup (outerHTML).
    Html,
    /// A text or comment node's data.
    Text,
    /// This attribute of the element, as `name="value"`.
    Attribute(String),
    /// A new attribute (or several) written as `name="value"`.
    AddAttribute,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeOp {
    /// Put this text of the node on the clipboard (it comes back as [`DevtoolsEvent::Clipboard`]).
    Copy(CopyKind),
    /// Remove the node from its parent.
    Delete,
    /// Insert a deep copy after the element and select it.
    Duplicate,
    /// Add (or remove) `visibility: hidden !important` in the element's `style`.
    ToggleHide,
    ScrollIntoView,
    Focus,
    /// Open the node and everything below it in the tree.
    ExpandRecursively,
    /// Close everything below the node in the tree (the node stays open).
    CollapseChildren,
    /// Store the node in the page as `temp1`, `temp2`, ... (it comes back as
    /// [`DevtoolsEvent::Stored`]).
    StoreGlobal,
    /// Start an edit: the tab answers with [`DevtoolsEvent::EditText`], the text to edit.
    BeginEdit(EditKind),
    /// Finish an attribute edit: `text` is `name="value"` (more attributes may follow); the
    /// attribute `replacing` that it no longer has is removed.
    SetAttributes {
        replacing: Option<String>,
        text: String,
    },
    /// Finish an Edit as HTML: the element becomes this markup.
    SetOuterHtml(String),
    /// Finish an Edit text: the node's data.
    SetText(String),
    /// Copy the element's markup to the clipboard (it comes back as [`DevtoolsEvent::Clipboard`])
    /// and delete it.
    Cut,
    /// Add this markup as the last children of the element.
    Paste(String),
    /// Force (or stop forcing) a pseudo-class on the element: 0 `:active`, 1 `:hover`, 2 `:focus`,
    /// 3 `:visited`, 4 `:focus-within`, 5 `:focus-visible`, 6 `:target`.
    ForceState(u8, bool),
    // (rows of the menu that are not done yet: shown disabled, and ignored by the tab)
    CopyStyles,
    BreakOnSubtree,
    BreakOnAttributes,
    BreakOnRemoval,
    AccessibilityTree,
    Screenshot,
}

/// What happened to a request the network panel shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NetUpdate {
    Started {
        id: u64,
        url: String,
        method: String,
        /// The Fetch destination ("document", "script", "image", "" for fetch/XHR).
        destination: String,
        headers: Vec<(String, String)>,
        at: Ticks,
    },
    Head {
        id: u64,
        status: u16,
        status_text: String,
        headers: Vec<(String, String)>,
        at: Ticks,
    },
    /// `len` more bytes; `data` is kept for text responses (up to a limit).
    Body {
        id: u64,
        len: usize,
        data: Option<Vec<u8>>,
    },
    Done {
        id: u64,
        at: Ticks,
    },
    Failed {
        id: u64,
        error: String,
        /// The content blocker stopped it (Chrome's "blocked:client").
        blocked: bool,
        at: Ticks,
    },
}

/// From a tab to the window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DevtoolsEvent {
    Console(Vec<ConsoleEntry>),
    /// The result of [`DevtoolsCommand::Eval`]: the value as the console shows it and its kind,
    /// or the exception.
    EvalResult(u64, Result<(String, ValueKind), String>),
    Network(Vec<NetUpdate>),
    Tree(Vec<DomRow>),
    Details(Option<(NodeId, NodeDetails)>),
    /// The boxes to draw over the page (viewport CSS px), or none.
    Highlight(Option<Highlight>),
    /// An element was picked (or inspected): select it, expanding these ancestors.
    Picked(NodeId, Vec<NodeId>),
    /// Pick mode ended without a pick (Escape in the page, a navigation).
    PickEnded,
    /// Text for the clipboard (a "Copy" row of the element tree's menu).
    Clipboard(String),
    /// A node was stored in the page's global variable `name`; `text` is how the console shows it.
    Stored {
        name: String,
        text: String,
    },
    /// The text to edit for [`NodeOp::BeginEdit`].
    EditText {
        node: NodeId,
        kind: EditKind,
        text: String,
    },
    /// The pseudo-classes forced on this element, a bit mask (bit 0 `:active` ... bit 6 `:target`).
    Forced(NodeId, u8),
    /// A new document committed (the network log starts over unless preserved).
    Navigated,
}
