// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Snowlark DOM (WHATWG DOM Standard, plus the HTML Standard's parsing and serialization).
//!
//! Nodes live in an arena, [`Dom`]: one per event loop, holding every document script there can reach. A
//! node is named by a [`NodeId`] (index + generation); tree links are ids. Elements carry their attributes
//! inline ([`AttrList`]); names are html5ever's interned atoms ([`LocalName`], [`Namespace`], [`Prefix`],
//! [`QualName`]).
//!
//! Every DOM operation is a method on [`Dom`] taking and returning [`NodeId`]s, named after the IDL member it
//! implements (`append_child` is `Node.appendChild`, `set_attribute` is `Element.setAttribute`), and
//! documented with the spec section of its algorithm. Errors are [`DomError`], one variant per
//! `DOMException` name.
//!
//! What the DOM cannot do itself it reports: [`MutationRecord`]s for the MutationObserver layer, and
//! [`Reaction`]s (insertion, removal, attribute change, adoption, cloning) for the HTML element and custom
//! element layers.
//!
//! No V8, style or layout types appear here. `&Dom` is `Sync`, so stylo's parallel traversal can read it.

mod api;
mod attrs;
mod clone;
mod dom;
mod error;
pub mod filter;
mod fragment_fast;
mod ids;
mod mutation;
pub mod names;
mod node;
mod parser;
mod records;
pub mod serialize;
mod shadow;
pub mod speculative;
pub mod text;
mod tree;
pub mod xml;
mod xml_serialize;

pub use dom::Dom;
pub use error::{DomError, DomResult};
pub use mutation::NodeOrString;
pub use node::{
    element_flags, qualified_name, Attr, AttrList, CustomElementState, DocumentData, DocumentKind,
    ElementData, Node, NodeData, NodeId, NodeType, QuirksMode, ShadowRootData, ShadowRootMode,
    SlotAssignmentMode, INLINE_ATTRS,
};
pub use parser::{AdjacentPosition, DomSink, HtmlParser, ParseOptions, ParseStep};
pub use records::{subtree, MutationKind, MutationRecord, Reaction, ReactionFilter};
pub use serialize::SerializeOptions;
pub use shadow::ShadowRootInit;
pub use tree::{position, Ancestors, Children, ChildrenRev, Descendants, Preceding};
pub use xml::{XmlParser, XmlStep};

pub use encoding_rs;
/// HTML's named character references (for parsers outside the HTML one, such as WebVTT's).
pub use html5ever::data::NAMED_ENTITIES;
pub use html5ever::{local_name, namespace_prefix, ns, LocalName, Namespace, Prefix, QualName};

#[cfg(test)]
mod tests;
