// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Node storage: ids, the arena slot, and the data each node kind carries.

use std::borrow::Cow;
use std::num::NonZeroU32;

use html5ever::{LocalName, Namespace, Prefix, QualName};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// A node's identity: an arena index plus the slot's generation, so a stale id can never alias a node that
/// later reuses the slot. `Option<NodeId>` is 8 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct NodeId {
    pub(crate) index: u32,
    pub(crate) generation: NonZeroU32,
}

impl NodeId {
    /// The arena slot index. Side tables (style data, layout boxes) may be indexed by it.
    #[inline]
    pub fn index(self) -> u32 {
        self.index
    }

    /// The slot generation this id was issued for.
    #[inline]
    pub fn generation(self) -> u32 {
        self.generation.get()
    }

    /// The largest generation a slot reaches before it is retired: 30 bits, so [`NodeId::to_bits`] has 62,
    /// which a JS wrapper keeps in two 31-bit internal fields.
    pub const MAX_GENERATION: u32 = (1 << 30) - 1;

    /// Packs the id into a `u64` (generation in the high half), for a JS wrapper's internal fields.
    #[inline]
    pub fn to_bits(self) -> u64 {
        (u64::from(self.generation.get()) << 32) | u64::from(self.index)
    }

    /// Inverse of [`NodeId::to_bits`]. `None` for a zero generation, which no id has.
    #[inline]
    pub fn from_bits(bits: u64) -> Option<NodeId> {
        Some(NodeId {
            index: bits as u32,
            generation: NonZeroU32::new((bits >> 32) as u32)?,
        })
    }
}

/// A set of nodes kept as one bit per arena slot: inserting and testing cost an index, where a hash
/// set of tens of thousands of ids costs a hash and a probe each, and its growth rehashes them.
/// Membership is by slot: an id of a slot's earlier node (a stale id) is in the set when the slot's
/// current node is. Meant for live nodes about to be freed together, whose slots no other live
/// node has.
#[derive(Clone, Default, Debug)]
pub struct NodeSet {
    bits: Vec<u64>,
    len: usize,
}

impl NodeSet {
    /// Adds `id`'s slot; false when it was in the set.
    #[inline]
    pub fn insert(&mut self, id: NodeId) -> bool {
        let (w, bit) = (id.index as usize / 64, id.index % 64);
        if w >= self.bits.len() {
            self.bits.resize(w + 1, 0);
        }
        let new = self.bits[w] & (1 << bit) == 0;
        self.bits[w] |= 1 << bit;
        self.len += usize::from(new);
        new
    }

    #[inline]
    pub fn contains(&self, id: &NodeId) -> bool {
        self.bits
            .get(id.index as usize / 64)
            .is_some_and(|w| w & (1 << (id.index % 64)) != 0)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Extend<NodeId> for NodeSet {
    fn extend<I: IntoIterator<Item = NodeId>>(&mut self, iter: I) {
        for id in iter {
            self.insert(id);
        }
    }
}

/// `Node.nodeType` values (DOM §4.4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum NodeType {
    Element = 1,
    Attribute = 2,
    Text = 3,
    CDataSection = 4,
    ProcessingInstruction = 7,
    Comment = 8,
    Document = 9,
    DocumentType = 10,
    DocumentFragment = 11,
}

/// A document's mode (DOM §4.5 "mode").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum QuirksMode {
    #[default]
    NoQuirks,
    Quirks,
    LimitedQuirks,
}

/// Whether a document is an HTML document or an XML document (DOM §4.5 "type").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DocumentKind {
    Html,
    Xml,
}

/// `ShadowRootMode` (DOM §4.8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ShadowRootMode {
    Open,
    Closed,
}

impl ShadowRootMode {
    /// The IDL enumeration value.
    pub fn as_str(self) -> &'static str {
        match self {
            ShadowRootMode::Open => "open",
            ShadowRootMode::Closed => "closed",
        }
    }
}

/// `SlotAssignmentMode` (DOM §4.8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum SlotAssignmentMode {
    #[default]
    Named,
    Manual,
}

/// An element's custom element state (DOM §4.9 "custom element state").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum CustomElementState {
    #[default]
    Uncustomized,
    Undefined,
    Failed,
    Precustomized,
    Custom,
}

/// Attribute values up to this many bytes are stored in the attribute itself.
const INLINE_VALUE: usize = 22;

/// An attribute's value: short ones (most: ids, class names, types, numbers) inline, without an
/// allocation of their own; longer ones on the heap.
#[derive(Clone)]
pub struct AttrValue(ValueRepr);

#[derive(Clone)]
enum ValueRepr {
    Inline { len: u8, bytes: [u8; INLINE_VALUE] },
    Heap(Box<str>),
}

impl AttrValue {
    /// The value as a string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            // (made from a `str`, so always UTF-8)
            ValueRepr::Inline { len, bytes } => {
                std::str::from_utf8(&bytes[..*len as usize]).unwrap_or_default()
            }
            ValueRepr::Heap(s) => s,
        }
    }

    /// The value's UTF-8 bytes (without the check [`AttrValue::as_str`] makes).
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.0 {
            ValueRepr::Inline { len, bytes } => &bytes[..*len as usize],
            ValueRepr::Heap(s) => s.as_bytes(),
        }
    }

    /// The value as an owned `String`.
    pub fn into_string(self) -> String {
        match self.0 {
            ValueRepr::Heap(s) => s.into_string(),
            _ => self.as_str().to_owned(),
        }
    }
}

impl From<&str> for AttrValue {
    #[inline]
    fn from(s: &str) -> AttrValue {
        if s.len() <= INLINE_VALUE {
            let mut bytes = [0; INLINE_VALUE];
            bytes[..s.len()].copy_from_slice(s.as_bytes());
            AttrValue(ValueRepr::Inline {
                len: s.len() as u8,
                bytes,
            })
        } else {
            AttrValue(ValueRepr::Heap(s.into()))
        }
    }
}

impl std::ops::Deref for AttrValue {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for AttrValue {
    fn eq(&self, other: &AttrValue) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for AttrValue {}

impl std::fmt::Debug for AttrValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

/// Text, CDATA section and comment data up to this many bytes is stored in the node itself.
const INLINE_DATA: usize = 38;

/// A CharacterData node's data: short runs (most text a parser makes, as the whitespace between
/// tags) inline in the node's slot, without an allocation of their own; longer ones, and any data
/// script changed in place, in a `String`.
#[derive(Clone)]
pub struct CharData(DataRepr);

#[derive(Clone)]
enum DataRepr {
    Inline(arrayvec::ArrayString<INLINE_DATA>),
    Heap(String),
}

impl CharData {
    /// The data as a string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            DataRepr::Inline(s) => s,
            DataRepr::Heap(s) => s,
        }
    }

    /// Replaces the bytes `range` (on character boundaries) with `data`: in place, staying inline
    /// while the result fits.
    pub(crate) fn splice(&mut self, range: std::ops::Range<usize>, data: &str) {
        if let DataRepr::Inline(s) = &self.0 {
            if s.len() - range.len() + data.len() <= INLINE_DATA {
                let mut out = arrayvec::ArrayString::<INLINE_DATA>::new();
                // (fits, checked above)
                for part in [&s[..range.start], data, &s[range.end..]] {
                    let _ = out.try_push_str(part);
                }
                self.0 = DataRepr::Inline(out);
                return;
            }
        }
        self.make_mut().replace_range(range, data);
    }

    /// The data as a `String` to change in place (inline data moves to the heap first).
    pub(crate) fn make_mut(&mut self) -> &mut String {
        if let DataRepr::Inline(s) = &self.0 {
            self.0 = DataRepr::Heap(s.as_str().to_owned());
        }
        match &mut self.0 {
            DataRepr::Heap(s) => s,
            DataRepr::Inline(_) => unreachable!("moved to the heap above"),
        }
    }
}

impl From<&str> for CharData {
    #[inline]
    fn from(s: &str) -> CharData {
        match arrayvec::ArrayString::from(s) {
            Ok(inline) => CharData(DataRepr::Inline(inline)),
            Err(_) => CharData(DataRepr::Heap(s.to_owned())),
        }
    }
}

impl From<String> for CharData {
    #[inline]
    fn from(s: String) -> CharData {
        if s.len() <= INLINE_DATA {
            CharData::from(s.as_str())
        } else {
            CharData(DataRepr::Heap(s))
        }
    }
}

impl std::ops::Deref for CharData {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for CharData {
    fn eq(&self, other: &CharData) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for CharData {}

impl std::fmt::Debug for CharData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

impl std::fmt::Display for CharData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

/// One attribute in an element's attribute list (DOM §4.9.2): namespace, namespace prefix, local name and
/// value. Stored inline on the element; an `Attr` *node* is materialized only when script asks for one
/// (see [`crate::Dom::attribute_node`]).
#[derive(Clone, Debug)]
pub struct Attr {
    pub(crate) name: QualName,
    pub(crate) value: AttrValue,
    /// The materialized `Attr` node for this attribute, if script ever asked for one.
    pub(crate) node: Option<NodeId>,
}

impl Attr {
    pub(crate) fn new(name: QualName, value: &str) -> Attr {
        Attr {
            name,
            value: value.into(),
            node: None,
        }
    }
    /// The attribute's local name.
    #[inline]
    pub fn local_name(&self) -> &LocalName {
        &self.name.local
    }
    /// The attribute's namespace (empty for null).
    #[inline]
    pub fn namespace(&self) -> &Namespace {
        &self.name.ns
    }
    /// The attribute's namespace prefix.
    #[inline]
    pub fn prefix(&self) -> Option<&Prefix> {
        self.name.prefix.as_ref()
    }
    /// The full name.
    #[inline]
    pub fn qual_name(&self) -> &QualName {
        &self.name
    }
    /// The attribute's value.
    #[inline]
    pub fn value(&self) -> &str {
        &self.value
    }
    /// The attribute's qualified name: `prefix:local` or `local` (DOM §4.9.2).
    pub fn qualified_name(&self) -> Cow<'_, str> {
        qualified_name(&self.name)
    }
    /// The materialized Attr node, if any.
    pub fn node(&self) -> Option<NodeId> {
        self.node
    }
}

/// `prefix:local`, or `local` without a prefix.
pub fn qualified_name(name: &QualName) -> Cow<'_, str> {
    match &name.prefix {
        Some(p) => Cow::Owned(format!("{}:{}", &**p, &*name.local)),
        None => Cow::Borrowed(&name.local),
    }
}

/// Inline capacity of an element's attribute list. Most elements have at most two attributes; a
/// larger inline list made every node slot bigger (312 bytes instead of 216), which measured slower
/// for parsing and DOM building even counting the elements whose lists spill to the heap.
pub const INLINE_ATTRS: usize = 2;

/// An element's attribute list.
pub type AttrList = SmallVec<[Attr; INLINE_ATTRS]>;

/// Flags on an element that the parser or the HTML layer sets ([`crate::Dom::element_flags`]).
pub mod element_flags {
    /// The parser created this `annotation-xml` element as an HTML integration point.
    pub const MATHML_ANNOTATION_XML_INTEGRATION_POINT: u8 = 1;
    /// A `script` element's "already started" flag (HTML §4.12.1.1), set by the fragment parser.
    pub const SCRIPT_ALREADY_STARTED: u8 = 2;
    /// The element was created by the HTML parser for a document (a script's "parser document").
    pub const PARSER_INSERTED: u8 = 4;
    /// The tokenizer saw duplicate attributes on the start tag (CSP: the element is not nonceable).
    pub const HAD_DUPLICATE_ATTRIBUTES: u8 = 8;
    /// A form-associated custom element (HTML §4.13.7): a listed, labelable, submittable and
    /// resettable element.
    pub const FORM_ASSOCIATED_CUSTOM: u8 = 16;
    /// The page's mark: the element's form pseudo-class state (HTML §4.16.3) is to be computed
    /// again, and the element is on the page's list of those.
    pub const FORM_STATE_MARKED: u8 = 32;
}

/// Data of an Element node (DOM §4.9).
#[derive(Clone, Debug)]
pub struct ElementData {
    pub(crate) name: QualName,
    pub(crate) attrs: AttrList,
    pub(crate) shadow_root: Option<NodeId>,
    /// The template contents of an HTML `template` element (HTML §4.12.3).
    pub(crate) template_contents: Option<NodeId>,
    pub(crate) custom_state: CustomElementState,
    pub(crate) is_value: Option<LocalName>,
    pub(crate) flags: u8,
}

impl ElementData {
    /// The element's namespace, prefix and local name.
    #[inline]
    pub fn name(&self) -> &QualName {
        &self.name
    }
    /// The attribute list.
    #[inline]
    pub fn attrs(&self) -> &[Attr] {
        &self.attrs
    }
    /// The custom element state.
    #[inline]
    pub fn custom_state(&self) -> CustomElementState {
        self.custom_state
    }
    /// The [`element_flags`] bits.
    #[inline]
    pub fn flags(&self) -> u8 {
        self.flags
    }
}

/// Data of a Document node (DOM §4.5).
#[derive(Clone, Debug)]
pub struct DocumentData {
    pub(crate) kind: DocumentKind,
    pub(crate) mode: QuirksMode,
    pub(crate) content_type: String,
    pub(crate) url: String,
    pub(crate) encoding: &'static encoding_rs::Encoding,
    pub(crate) allow_declarative_shadow_roots: bool,
    /// The template contents owner document, created on first need (HTML §4.12.3).
    pub(crate) template_contents_owner: Option<NodeId>,
    /// This document is itself a template contents owner document (it has no browsing context).
    pub(crate) is_template_contents_owner: bool,
    /// Bumped on every mutation of a node whose node document is this one.
    pub(crate) version: u64,
    /// How many HTML `base` elements are connected in this document (shadow trees included).
    pub(crate) connected_bases: u32,
    /// HTML "pragma-set default language" (a `meta http-equiv=content-language`'s).
    pub(crate) pragma_language: Option<Box<str>>,
    /// The id and name index ([`crate::ids`]).
    pub(crate) names: crate::ids::NameIndex,
}

/// Data of a ShadowRoot node (DOM §4.8).
#[derive(Clone, Debug)]
pub struct ShadowRootData {
    pub(crate) host: NodeId,
    pub(crate) mode: ShadowRootMode,
    pub(crate) delegates_focus: bool,
    pub(crate) slot_assignment: SlotAssignmentMode,
    pub(crate) clonable: bool,
    pub(crate) serializable: bool,
    pub(crate) declarative: bool,
    pub(crate) available_to_element_internals: bool,
}

impl ShadowRootData {
    /// The host element.
    pub fn host(&self) -> NodeId {
        self.host
    }
    /// `mode`.
    pub fn mode(&self) -> ShadowRootMode {
        self.mode
    }
    /// `delegatesFocus`.
    pub fn delegates_focus(&self) -> bool {
        self.delegates_focus
    }
    /// `slotAssignment`.
    pub fn slot_assignment(&self) -> SlotAssignmentMode {
        self.slot_assignment
    }
    /// `clonable`.
    pub fn clonable(&self) -> bool {
        self.clonable
    }
    /// `serializable`.
    pub fn serializable(&self) -> bool {
        self.serializable
    }
    /// The "declarative" flag.
    pub fn declarative(&self) -> bool {
        self.declarative
    }
    /// The "available to element internals" flag.
    pub fn available_to_element_internals(&self) -> bool {
        self.available_to_element_internals
    }
}

/// Data of a DocumentType node (DOM §4.6).
#[derive(Clone, Debug)]
pub struct DoctypeData {
    pub(crate) name: String,
    pub(crate) public_id: String,
    pub(crate) system_id: String,
}

/// Data of a materialized Attr node. While `owner` is set, the live name and value are the owner element's
/// list entry whose `node` is this one; `detached` holds them only while the Attr has no element.
#[derive(Clone, Debug)]
pub struct AttrNodeData {
    pub(crate) owner: Option<NodeId>,
    pub(crate) detached: Attr,
}

/// What a node is, with the data for its kind. Elements, the most common nodes, are stored inline (no
/// allocation per element); the rare kinds are boxed.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum NodeData {
    Document(Box<DocumentData>),
    DocumentType(Box<DoctypeData>),
    /// A DocumentFragment. `host` is non-null for template contents (the template element).
    DocumentFragment {
        host: Option<NodeId>,
    },
    ShadowRoot(Box<ShadowRootData>),
    Element(ElementData),
    Attr(Box<AttrNodeData>),
    Text(CharData),
    CDataSection(CharData),
    Comment(CharData),
    ProcessingInstruction {
        target: String,
        data: String,
    },
    /// A free arena slot (reusable when [`crate::Dom`]'s free bit for it is set; a retired one is not).
    Free {},
}

pub(crate) mod flags {
    /// The node's shadow-including root is a document.
    pub const CONNECTED: u8 = 1;
    /// A MutationObserver is registered on this node.
    pub const OBSERVED: u8 = 2;
    /// A transient registered observer is on this node (DOM §4.3.1).
    pub const TRANSIENT_OBSERVED: u8 = 4;
    /// The layer above made a script wrapper for this node ([`crate::Dom::set_wrapped`]).
    pub const WRAPPED: u8 = 8;
}

/// One arena slot.
#[derive(Clone, Debug)]
pub struct Node {
    pub(crate) generation: NonZeroU32,
    pub(crate) flags: u8,
    pub(crate) parent: Option<NodeId>,
    pub(crate) first_child: Option<NodeId>,
    pub(crate) last_child: Option<NodeId>,
    pub(crate) prev_sibling: Option<NodeId>,
    pub(crate) next_sibling: Option<NodeId>,
    /// The node document. For a Document node, itself.
    pub(crate) document: Option<NodeId>,
    pub(crate) data: NodeData,
}

impl Node {
    /// The node's data.
    #[inline]
    pub fn data(&self) -> &NodeData {
        &self.data
    }

    #[inline]
    pub(crate) fn node_type(&self) -> NodeType {
        match self.data {
            NodeData::Document(_) => NodeType::Document,
            NodeData::DocumentType(_) => NodeType::DocumentType,
            NodeData::DocumentFragment { .. } | NodeData::ShadowRoot(_) => {
                NodeType::DocumentFragment
            }
            NodeData::Element(_) => NodeType::Element,
            NodeData::Attr(_) => NodeType::Attribute,
            NodeData::Text(_) => NodeType::Text,
            NodeData::CDataSection(_) => NodeType::CDataSection,
            NodeData::Comment(_) => NodeType::Comment,
            NodeData::ProcessingInstruction { .. } => NodeType::ProcessingInstruction,
            NodeData::Free { .. } => unreachable!("free node slot"),
        }
    }
}
