// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML parsing through html5ever (HTML §13.2): a `TreeSink` that builds Snowlark nodes, an incremental
//! [`HtmlParser`] that stops at every `</script>` so the page layer can run the script, and the fragment
//! parsing algorithm behind `innerHTML`, `outerHTML`, `insertAdjacentHTML` and `setHTMLUnsafe`.
//!
//! The sink reaches the [`Dom`] through a `RefCell` (any `D: Borrow<RefCell<Dom>>`: an owned cell, a
//! reference, or an `Rc`), because html5ever's sink API takes `&self`. The cell wraps the whole store, never
//! a node. Between [`HtmlParser::run`] calls nothing is borrowed, so a paused parser and running script share
//! the store.

use std::borrow::{Borrow, Cow};
use std::cell::{Cell, RefCell};

use html5ever::interface::{
    ElemName, ElementFlags, NodeOrText, QuirksMode as H5QuirksMode, TreeSink,
};
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{BufferQueue, Tokenizer, TokenizerOpts};
use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts};
use html5ever::{local_name, ns, Attribute, LocalName, Namespace, QualName, TokenizerResult};

use crate::dom::Dom;
use crate::error::{DomError, DomResult};
use crate::node::{
    element_flags, Attr, DocumentKind, NodeData, NodeId, QuirksMode, ShadowRootMode,
    SlotAssignmentMode,
};
use crate::shadow::ShadowRootInit;

/// Parser options.
#[derive(Clone, Copy, Debug)]
pub struct ParseOptions {
    /// Scripting is enabled for the document (affects `noscript`). Default true.
    pub scripting_enabled: bool,
    /// The document is an `iframe srcdoc` document (affects quirks mode detection).
    pub iframe_srcdoc: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        ParseOptions {
            scripting_enabled: true,
            iframe_srcdoc: false,
        }
    }
}

/// An element name handed to html5ever: two atom clones (static atoms copy, dynamic ones bump a count).
#[derive(Debug)]
pub struct SinkElemName {
    ns: Namespace,
    local: LocalName,
}

impl ElemName for SinkElemName {
    #[inline]
    fn ns(&self) -> &Namespace {
        &self.ns
    }
    #[inline]
    fn local_name(&self) -> &LocalName {
        &self.local
    }
}

/// html5ever's `TreeSink` over a [`Dom`].
pub struct DomSink<D: Borrow<RefCell<Dom>>> {
    dom: D,
    document: NodeId,
    fragment: bool,
    /// Whether scripts of the document can run at all.
    scripting: bool,
    /// A fragment parse whose scripts may run once inserted ([`Dom::parse_fragment_running_scripts`]).
    run_scripts: bool,
    /// The tokenizer's line (1-based, newlines of `document.write` text included) as of the token
    /// being processed.
    line: Cell<u64>,
    /// The newlines in the text `document.write` inserted so far: not lines of the document's source.
    written_lines: Cell<u64>,
    /// The source line of each `script` start tag whose end tag has not been reached yet.
    script_lines: RefCell<Vec<(NodeId, u32)>>,
}

impl<D: Borrow<RefCell<Dom>>> DomSink<D> {
    /// A sink that builds into `document`.
    pub fn new(dom: D, document: NodeId) -> Self {
        DomSink {
            dom,
            document,
            fragment: false,
            scripting: true,
            run_scripts: false,
            line: Cell::new(1),
            written_lines: Cell::new(0),
            script_lines: RefCell::new(Vec::new()),
        }
    }

    #[inline]
    fn cell(&self) -> &RefCell<Dom> {
        self.dom.borrow()
    }

    #[inline]
    fn dom(&self) -> std::cell::RefMut<'_, Dom> {
        self.cell().borrow_mut()
    }

    /// Inserts a node the tree builder moves (adoption agency, foster parenting, reparenting). Script
    /// can have rearranged the tree since the tree builder last saw it, so the insertion may be one
    /// that DOM's "ensure pre-insert validity" refuses: `node` a host-including inclusive ancestor of
    /// `parent` (through shadow roots and template contents). Such a node is dropped (removed from
    /// its parent) rather than inserted, which would make a cycle.
    fn insert_node(dom: &mut Dom, node: NodeId, parent: NodeId, before: Option<NodeId>) {
        if dom.is_host_including_inclusive_ancestor_of(node, parent) {
            if dom.parent(node).is_some() {
                dom.remove_node(node);
            }
            return;
        }
        // (the reference child moved elsewhere meanwhile: append)
        let before = before.filter(|&b| dom.parent(b) == Some(parent));
        dom.insert(node, parent, before, false);
    }

    fn append_text(dom: &mut Dom, parent: NodeId, before: Option<NodeId>, text: &str) {
        let prev = match before {
            Some(b) => dom.previous_sibling(b),
            None => dom.last_child(parent),
        };
        match prev {
            Some(p) if dom.is_exclusive_text(p) => dom.append_data(p, text),
            _ => {
                let doc = dom.node_document(parent);
                let t = dom.create_text_node(doc, text);
                dom.insert(t, parent, before, false);
            }
        }
    }
}

fn has_attr(attrs: &[Attribute], local: LocalName) -> bool {
    attrs
        .iter()
        .any(|a| a.name.ns == ns!() && a.name.local == local)
}

impl<D: Borrow<RefCell<Dom>>> TreeSink for DomSink<D> {
    type Handle = NodeId;
    type Output = D;
    type ElemName<'a>
        = SinkElemName
    where
        Self: 'a;

    fn finish(self) -> D {
        self.dom
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        self.document
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> SinkElemName {
        let dom = self.cell().borrow();
        let name = dom.element_name(*target);
        SinkElemName {
            ns: name.ns.clone(),
            local: name.local.clone(),
        }
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        let mut dom = self.dom();
        let is = attrs
            .iter()
            .find(|a| a.name.ns == ns!() && &*a.name.local == "is")
            .map(|a| LocalName::from(&*a.value));
        let el = dom.create_element_with_name(self.document, name, is);
        for a in attrs {
            dom.push_parsed_attribute(el, Attr::new(a.name, &a.value));
        }
        let mut bits = 0;
        if flags.mathml_annotation_xml_integration_point {
            bits |= element_flags::MATHML_ANNOTATION_XML_INTEGRATION_POINT;
        }
        if flags.had_duplicate_attributes {
            bits |= element_flags::HAD_DUPLICATE_ATTRIBUTES;
        }
        if !self.fragment {
            // (html5ever pushes every element it creates onto the stack of open elements, and
            // reports each removal from it to `pop`)
            bits |= element_flags::PARSER_INSERTED | element_flags::OPEN_IN_PARSER;
        }
        if ((self.fragment && !self.run_scripts) || !self.scripting) && dom.is_script_element(el) {
            // (HTML does this for its own at the start tag: these scripts never run, nor do their
            // clones)
            bits |= element_flags::SCRIPT_ALREADY_STARTED;
        }
        if bits != 0 {
            dom.set_element_flag(el, bits, true);
        }
        if !self.fragment && dom.element_name(el).ns == ns!(html) && dom.is_script_element(el) {
            // The line the start tag ends on, which is where the script's text starts: lines
            // `document.write` inserted are not the document's.
            let line = self
                .line
                .get()
                .saturating_sub(self.written_lines.get())
                .max(1);
            let mut lines = self.script_lines.borrow_mut();
            // (a script whose end tag never comes, at the end of the input, leaves its entry)
            if lines.len() >= 16 {
                lines.remove(0);
            }
            lines.push((el, u32::try_from(line).unwrap_or(u32::MAX)));
        }
        el
    }

    fn set_current_line(&self, line_number: u64) {
        self.line.set(line_number);
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.dom().create_comment(self.document, &text)
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.dom()
            .create_pi_unchecked(self.document, &target, &data)
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let mut dom = self.dom();
        match child {
            NodeOrText::AppendNode(n) => Self::insert_node(&mut dom, n, *parent, None),
            NodeOrText::AppendText(t) => Self::append_text(&mut dom, *parent, None, &t),
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let has_parent = self.cell().borrow().parent(*element).is_some();
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        let mut dom = self.dom();
        let dt = dom.create_doctype_unchecked(self.document, &name, &public_id, &system_id);
        dom.insert(dt, self.document, None, false);
    }

    fn mark_script_already_started(&self, node: &NodeId) {
        let mut dom = self.dom();
        // (a fragment parse marks each script at its start tag, before it is inserted, unless its
        // scripts run; one still open at the end of the input never runs)
        if self.run_scripts && dom.parent(*node).is_none() {
            return;
        }
        dom.set_element_flag(*node, element_flags::SCRIPT_ALREADY_STARTED, true);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        self.dom
            .borrow()
            .borrow()
            .template_contents(*target)
            .expect("template without contents")
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: H5QuirksMode) {
        let mode = match mode {
            H5QuirksMode::Quirks => QuirksMode::Quirks,
            H5QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
            H5QuirksMode::NoQuirks => QuirksMode::NoQuirks,
        };
        self.dom().set_quirks_mode(self.document, mode);
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let mut dom = self.dom();
        // (script can have detached the sibling: nothing to insert before, the node is not
        // inserted)
        let Some(parent) = dom.parent(*sibling) else {
            return;
        };
        match new_node {
            NodeOrText::AppendNode(n) => Self::insert_node(&mut dom, n, parent, Some(*sibling)),
            NodeOrText::AppendText(t) => Self::append_text(&mut dom, parent, Some(*sibling), &t),
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        let mut dom = self.dom();
        for a in attrs {
            if dom.attr(*target, &a.name.ns, &a.name.local).is_none() {
                dom.append_attribute(*target, Attr::new(a.name, &a.value));
            }
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.dom().remove_node(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut dom = self.dom();
        while let Some(c) = dom.first_child(*node) {
            Self::insert_node(&mut dom, c, *new_parent, None);
        }
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &NodeId) -> bool {
        self.cell().borrow().element_flags(*handle)
            & element_flags::MATHML_ANNOTATION_XML_INTEGRATION_POINT
            != 0
    }

    fn allow_declarative_shadow_roots(&self, intended_parent: &NodeId) -> bool {
        let dom = self.cell().borrow();
        dom.allow_declarative_shadow_roots(dom.node_document(*intended_parent))
    }

    /// HTML §13.2.6.4.4 "in head", a start tag "template" with a `shadowrootmode` (steps 3-8).
    fn attach_declarative_shadow(
        &self,
        host: &NodeId,
        template: &NodeId,
        attrs: &[Attribute],
    ) -> bool {
        let mut dom = self.dom();
        let mode = match attrs
            .iter()
            .find(|a| a.name.ns == ns!() && a.name.local == local_name!("shadowrootmode"))
            .map(|a| &*a.value)
        {
            Some("open") => ShadowRootMode::Open,
            Some("closed") => ShadowRootMode::Closed,
            _ => return false,
        };
        if !dom.is_element(*host) || dom.shadow_root(*host).is_some() {
            return false;
        }
        let init = ShadowRootInit {
            mode,
            delegates_focus: has_attr(attrs, LocalName::from("shadowrootdelegatesfocus")),
            // (the shadowrootslotassignment attribute: "manual", ASCII case-insensitively, or named)
            slot_assignment: if attrs.iter().any(|a| {
                a.name.ns == ns!()
                    && &*a.name.local == "shadowrootslotassignment"
                    && a.value.eq_ignore_ascii_case("manual")
            }) {
                SlotAssignmentMode::Manual
            } else {
                SlotAssignmentMode::Named
            },
            clonable: has_attr(attrs, LocalName::from("shadowrootclonable")),
            serializable: has_attr(attrs, LocalName::from("shadowrootserializable")),
        };
        let Ok(shadow) = dom.attach_shadow(*host, init) else {
            return false;
        };
        dom.set_shadow_root_declarative(shadow, true);
        dom.set_shadow_root_available_to_internals(shadow);
        // The template's contents become the shadow root; the fragment created with it is dropped.
        let old = dom.template_contents(*template);
        dom.element_mut(*template).template_contents = Some(shadow);
        if let Some(old) = old {
            dom.free_subtree(old);
        }
        true
    }

    /// An element was popped off the stack of open elements: for an `option`, HTML's "maybe clone an option
    /// into selectedcontent" (the vendored html5ever reports every pop, also those at the end of input).
    fn pop(&self, node: &NodeId) {
        let is_option = {
            let mut dom = self.dom();
            if !self.fragment && dom.is_alive(*node) && dom.is_element(*node) {
                dom.set_element_flag(*node, element_flags::OPEN_IN_PARSER, false);
            }
            dom.is_html_element_named(*node, &local_name!("option"))
        };
        if is_option {
            self.clone_option_into_selectedcontent(*node);
        }
    }

    /// Handled in [`TreeSink::pop`], which sees every option pop.
    fn maybe_clone_an_option_into_selectedcontent(&self, _option: &NodeId) {}
}

impl<D: Borrow<RefCell<Dom>>> DomSink<D> {
    /// HTML "maybe clone an option into selectedcontent". Selectedness at parse time is approximated: an
    /// option is selected if it has a `selected` attribute, or if it is the select's first option and no
    /// option has one.
    fn clone_option_into_selectedcontent(&self, option: NodeId) {
        let mut dom = self.dom();
        let Some(select) = dom
            .ancestors(option)
            .find(|&a| dom.is_html_element_named(a, &local_name!("select")))
        else {
            return;
        };
        if dom.attr(select, &ns!(), &local_name!("multiple")).is_some() {
            return;
        }
        let Some(selectedcontent) = dom
            .descendants(select)
            .find(|&d| dom.is_html_element_named(d, &LocalName::from("selectedcontent")))
        else {
            return;
        };
        let selected_attr =
            |dom: &Dom, o: NodeId| dom.attr(o, &ns!(), &local_name!("selected")).is_some();
        let selected = selected_attr(&dom, option) || {
            let options: Vec<NodeId> = dom
                .descendants(select)
                .filter(|&d| dom.is_html_element_named(d, &local_name!("option")))
                .collect();
            options.first() == Some(&option) && !options.iter().any(|&o| selected_attr(&dom, o))
        };
        if !selected {
            return;
        }
        let doc = dom.node_document(selectedcontent);
        let fragment = dom.create_document_fragment(doc);
        let children: Vec<NodeId> = dom.children(option).collect();
        for c in children {
            let copy = dom.clone_a_node(c, doc, true, None);
            dom.insert(copy, fragment, None, false);
        }
        dom.replace_all(Some(fragment), selectedcontent);
    }
}

/// What [`HtmlParser::run`] stopped for.
#[derive(Debug, PartialEq)]
pub enum ParseStep {
    /// All pushed input is consumed; push more or call [`HtmlParser::finish`].
    NeedInput,
    /// A `script` end tag was processed: the caller runs the script (HTML §13.2.6.4.8, "an end tag whose tag
    /// name is script"), then calls `run` again.
    Script(NodeId),
    /// A `meta` declared this encoding; the caller may restart with it (HTML §13.2.3.4 "changing the encoding
    /// while parsing").
    EncodingIndicator(String),
}

/// An incremental HTML document parser. Input arrives as bytes or text; [`HtmlParser::run`] tokenizes and
/// tree-builds until the input runs out or a script must run. Script execution, `document.write` and parser
/// blocking are the page layer's: it gets the script element from [`ParseStep::Script`] and can insert text
/// at the insertion point with [`HtmlParser::insert_input`].
pub struct HtmlParser<D: Borrow<RefCell<Dom>>> {
    tokenizer: Tokenizer<TreeBuilder<NodeId, DomSink<D>>>,
    /// The input before the insertion point (all of it while there is none): what the tokenizer reads.
    input: BufferQueue,
    /// The input after each insertion point, the current one's last (HTML §13.2.6.4.8: a script end
    /// tag sets the insertion point "just before the next input character" and later restores the
    /// old one; the input between the two belongs to the old one). `held[0]` runs to the end of the
    /// stream.
    held: Vec<BufferQueue>,
    decoder: Option<encoding_rs::Decoder>,
    document: NodeId,
}

/// Moves the whole of `from` to the end of `to`.
fn append_queue(to: &BufferQueue, from: &BufferQueue) {
    while let Some(chunk) = from.pop_front() {
        to.push_back(chunk);
    }
}

fn tree_builder_opts(options: ParseOptions, mode: QuirksMode) -> TreeBuilderOpts {
    TreeBuilderOpts {
        scripting_enabled: options.scripting_enabled,
        iframe_srcdoc: options.iframe_srcdoc,
        quirks_mode: match mode {
            QuirksMode::NoQuirks => H5QuirksMode::NoQuirks,
            QuirksMode::Quirks => H5QuirksMode::Quirks,
            QuirksMode::LimitedQuirks => H5QuirksMode::LimitedQuirks,
        },
        ..Default::default()
    }
}

impl<D: Borrow<RefCell<Dom>>> HtmlParser<D> {
    /// A parser that builds into the (empty) document `document` of `dom`.
    pub fn new(dom: D, document: NodeId, options: ParseOptions) -> Self {
        let mut sink = DomSink::new(dom, document);
        sink.scripting = options.scripting_enabled;
        let tb = TreeBuilder::new(sink, tree_builder_opts(options, QuirksMode::NoQuirks));
        HtmlParser {
            tokenizer: Tokenizer::new(tb, TokenizerOpts::default()),
            input: BufferQueue::default(),
            held: Vec::new(),
            decoder: None,
            document,
        }
    }

    /// The queue the end of the input stream is in.
    fn stream_end(&self) -> &BufferQueue {
        self.held.first().unwrap_or(&self.input)
    }

    /// The document being built.
    pub fn document(&self) -> NodeId {
        self.document
    }

    /// The store the parser builds into.
    pub fn dom(&self) -> &RefCell<Dom> {
        self.tokenizer.sink.sink.dom.borrow()
    }

    /// Sets the encoding that [`HtmlParser::push_bytes`] decodes with. A byte order mark overrides it (the
    /// decoder sniffs it, per Encoding §6 "decode").
    pub fn set_encoding(&mut self, encoding: &'static encoding_rs::Encoding) {
        self.decoder = Some(encoding.new_decoder());
        self.dom()
            .borrow_mut()
            .set_encoding(self.document, encoding);
    }

    /// HTML §13.2.3.4 "change the encoding" on the fly: the bytes pushed from now on are decoded
    /// with `encoding` (those already pushed stay as they were decoded), and it is the document's.
    /// The caller changes at a point where the old decoder holds no partial character.
    pub fn change_encoding(&mut self, encoding: &'static encoding_rs::Encoding) {
        if self.decoder.as_ref().map(|d| d.encoding()) == Some(encoding) {
            return;
        }
        self.decoder = Some(encoding.new_decoder_without_bom_handling());
        self.dom()
            .borrow_mut()
            .set_encoding(self.document, encoding);
    }

    /// Appends bytes to the input stream; `last` marks the end of the byte stream.
    pub fn push_bytes(&mut self, bytes: &[u8], last: bool) {
        let decoder = self
            .decoder
            .get_or_insert_with(|| encoding_rs::UTF_8.new_decoder());
        let mut out = String::with_capacity(
            decoder
                .max_utf8_buffer_length(bytes.len())
                .unwrap_or(bytes.len() * 3),
        );
        let _ = decoder.decode_to_string(bytes, &mut out, last);
        if !out.is_empty() {
            self.stream_end().push_back(StrTendril::from(out));
        }
    }

    /// Appends text to the input stream.
    pub fn push_str(&mut self, text: &str) {
        if !text.is_empty() {
            self.stream_end().push_back(StrTendril::from_slice(text));
        }
    }

    /// Inserts text just before the insertion point (`document.write`): after what earlier calls
    /// inserted there and the tokenizer has not consumed yet.
    pub fn insert_input(&mut self, text: &str) {
        if !text.is_empty() {
            // (the tokenizer counts a CR LF pair, or a lone CR, as one newline)
            let b = text.as_bytes();
            let newlines = b
                .iter()
                .enumerate()
                .filter(|&(i, &c)| c == b'\n' || (c == b'\r' && b.get(i + 1) != Some(&b'\n')))
                .count() as u64;
            let sink = &self.tokenizer.sink.sink;
            sink.written_lines.set(sink.written_lines.get() + newlines);
            self.input.push_back(StrTendril::from_slice(text));
        }
    }

    /// The source line (1-based) where the text of the `script` element `el`, which the parser
    /// created, starts: the line its start tag ends on. Once per element, at its end tag.
    pub fn take_script_line(&mut self, el: NodeId) -> Option<u32> {
        let mut lines = self.tokenizer.sink.sink.script_lines.borrow_mut();
        let i = lines.iter().position(|&(e, _)| e == el)?;
        Some(lines.remove(i).1)
    }

    /// Sets the insertion point just before the next input character, keeping the current one to
    /// [`HtmlParser::restore_insertion_point`]: [`HtmlParser::run`] stops there until then.
    pub fn set_insertion_point(&mut self) {
        let after = BufferQueue::default();
        self.input.swap_with(&after);
        self.held.push(after);
    }

    /// Restores the insertion point that [`HtmlParser::set_insertion_point`] replaced (none, if it
    /// was the first): the input up to it is the tokenizer's again.
    pub fn restore_insertion_point(&mut self) {
        if let Some(after) = self.held.pop() {
            append_queue(&self.input, &after);
        }
    }

    /// The input not yet consumed in stream order, insertion points ignored.
    fn for_each_pending(&self, mut f: impl FnMut(&StrTendril)) {
        for queue in std::iter::once(&self.input).chain(self.held.iter().rev()) {
            let taken = BufferQueue::default();
            queue.swap_with(&taken);
            while let Some(chunk) = taken.pop_front() {
                f(&chunk);
                queue.push_back(chunk);
            }
        }
    }

    /// The input not yet consumed, from its `skip`th byte (for the speculative parser: what comes
    /// after the insertion point while the parser waits for a script).
    pub fn pending_input(&self, skip: usize) -> String {
        let mut text = String::new();
        let mut seen = 0usize;
        self.for_each_pending(|chunk| {
            let len = chunk.len();
            if seen + len > skip {
                let from = skip.saturating_sub(seen);
                text.push_str(chunk.get(from..).unwrap_or(""));
            }
            seen += len;
        });
        text
    }

    /// The length in bytes of the input not yet consumed.
    pub fn pending_len(&self) -> usize {
        let mut n = 0;
        self.for_each_pending(|chunk| n += chunk.len());
        n
    }

    /// Parses until the input is consumed, up to the insertion point while there is one, or a script
    /// end tag was processed.
    pub fn run(&mut self) -> ParseStep {
        match self.tokenizer.feed(&self.input) {
            TokenizerResult::Done => ParseStep::NeedInput,
            TokenizerResult::Script(node) => ParseStep::Script(node),
            TokenizerResult::EncodingIndicator(e) => ParseStep::EncodingIndicator(e.to_string()),
        }
    }

    /// Ends the input stream: parses what is left, ignoring script stops, then runs the end-of-file steps.
    pub fn finish(mut self) -> D {
        while !self.held.is_empty() {
            self.restore_insertion_point();
        }
        while self.tokenizer.feed(&self.input) != TokenizerResult::Done {}
        self.tokenizer.end();
        self.tokenizer.sink.sink.finish()
    }
}

/// Runs `f` with the store moved into a `RefCell`, for the one-shot parsing entry points on `&mut Dom`.
fn with_cell<R>(dom: &mut Dom, f: impl FnOnce(&RefCell<Dom>) -> R) -> R {
    let cell = RefCell::new(std::mem::take(dom));
    let r = f(&cell);
    *dom = cell.into_inner();
    r
}

/// `insertAdjacentHTML` positions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdjacentPosition {
    BeforeBegin,
    AfterBegin,
    BeforeEnd,
    AfterEnd,
}

impl AdjacentPosition {
    /// Parses the position string ASCII case-insensitively.
    pub fn parse(s: &str) -> DomResult<AdjacentPosition> {
        let p = s.to_ascii_lowercase();
        Ok(match p.as_str() {
            "beforebegin" => AdjacentPosition::BeforeBegin,
            "afterbegin" => AdjacentPosition::AfterBegin,
            "beforeend" => AdjacentPosition::BeforeEnd,
            "afterend" => AdjacentPosition::AfterEnd,
            _ => return Err(DomError::Syntax),
        })
    }
}

impl Dom {
    /// Parses a complete HTML document from bytes in `encoding` (a BOM overrides it) into a new document.
    /// Scripts are not run: the incremental [`HtmlParser`] is the entry point for that.
    pub fn parse_html_document(
        &mut self,
        bytes: &[u8],
        encoding: &'static encoding_rs::Encoding,
        options: ParseOptions,
    ) -> NodeId {
        let doc = self.create_document(DocumentKind::Html);
        self.set_allow_declarative_shadow_roots(doc, true);
        with_cell(self, |cell| {
            let mut parser = HtmlParser::new(cell, doc, options);
            parser.set_encoding(encoding);
            parser.push_bytes(bytes, true);
            parser.finish();
        });
        doc
    }

    /// Parses a complete HTML document from text into a new document.
    pub fn parse_html_str(&mut self, html: &str, options: ParseOptions) -> NodeId {
        self.parse_html_str_with(html, options, true)
    }

    /// [`Dom::parse_html_str`] with the document's "allow declarative shadow roots" (HTML §4.12.3
    /// and §13.2.6.4.4): false for `DOMParser` and `XMLHttpRequest` documents, where a
    /// `<template shadowrootmode>` stays a template.
    pub fn parse_html_str_with(
        &mut self,
        html: &str,
        options: ParseOptions,
        allow_declarative_shadow_roots: bool,
    ) -> NodeId {
        let doc = self.create_document(DocumentKind::Html);
        self.set_allow_declarative_shadow_roots(doc, allow_declarative_shadow_roots);
        with_cell(self, |cell| {
            let mut parser = HtmlParser::new(cell, doc, options);
            parser.push_str(html);
            parser.finish();
        });
        doc
    }

    /// `DOMParser.parseFromString(html, "text/html")`'s parse (HTML §8.5.1): a new HTML document
    /// parsed from `html` with scripting disabled and declarative shadow roots not allowed.
    ///
    /// Most strings scripts parse this way are a list or a few elements of plain markup, with no
    /// doctype, `html`, `head` or `body` of their own. For those, the tree construction stage does
    /// a fixed thing up to the first token that is not whitespace: the initial insertion mode
    /// (no doctype) sets quirks mode, the whitespace before that token is ignored ("initial",
    /// "before html", "before head"), the `html`, `head` and `body` elements are made in turn
    /// ("before html", "before head", "in head", "after head" for "anything else") and the rest is
    /// parsed "in body". That rest is then parsed by the fragment fast path with the `body` as
    /// context, whose subset has no element "in head" or "after head" would take and nothing whose
    /// "in body" rules depend on quirks mode (`table`). Anything outside the subset is parsed by the
    /// full parser, as if this had not run.
    pub fn parse_html_document_inert(&mut self, html: &str) -> NodeId {
        if let Some(doc) = self.parse_html_document_fast(html) {
            return doc;
        }
        self.parse_html_str_with(
            html,
            ParseOptions {
                scripting_enabled: false,
                iframe_srcdoc: false,
            },
            false,
        )
    }

    pub(crate) fn parse_html_document_fast(&mut self, html: &str) -> Option<NodeId> {
        if html.starts_with('\u{feff}') {
            // (the full parser drops it: kept there, with its other effects)
            return None;
        }
        // (the tokenizer turns CR into LF: both are whitespace here)
        let rest = html.trim_start_matches(['\t', '\n', '\x0c', '\r', ' ']);
        if rest.starts_with('&') {
            // (a character reference can be whitespace, which the modes before `body` also ignore)
            return None;
        }
        let doc = self.create_document(DocumentKind::Html);
        let body = self.create_html_element(doc, local_name!("body"));
        self.set_element_flag(body, element_flags::PARSER_INSERTED, true);
        let Some(fragment) = crate::fragment_fast::parse_with_flags(
            self,
            body,
            rest,
            element_flags::PARSER_INSERTED,
        ) else {
            self.free_subtree(body);
            self.free_subtree(doc);
            return None;
        };
        self.set_quirks_mode(doc, QuirksMode::Quirks);
        let root = self.create_html_element(doc, local_name!("html"));
        let head = self.create_html_element(doc, local_name!("head"));
        for el in [root, head] {
            self.set_element_flag(el, element_flags::PARSER_INSERTED, true);
        }
        self.insert(root, doc, None, false);
        self.insert(head, root, None, false);
        self.insert(body, root, None, false);
        self.insert(fragment, body, None, false);
        self.free_subtree(fragment);
        Some(doc)
    }

    /// The HTML fragment parsing algorithm (HTML §13.4) with `context`, returning the nodes in a new
    /// DocumentFragment whose node document is `context`'s.
    pub fn parse_fragment(
        &mut self,
        context: NodeId,
        input: &str,
        allow_declarative_shadow_roots: bool,
        options: ParseOptions,
    ) -> NodeId {
        // Plain markup (well-nested common elements, nothing the algorithm would recover from)
        // builds the same nodes without the tree builder.
        if let Some(fragment) = crate::fragment_fast::parse(self, context, input) {
            return fragment;
        }
        self.parse_fragment_full(context, input, allow_declarative_shadow_roots, options)
    }

    /// [`Dom::parse_fragment`] with `runScripts` (HTML "initialize an HTML fragment parser": the
    /// parser's script target document is the context's): a script whose end tag the input has
    /// may run once the fragment is inserted into a document.
    pub fn parse_fragment_running_scripts(
        &mut self,
        context: NodeId,
        input: &str,
        allow_declarative_shadow_roots: bool,
    ) -> NodeId {
        self.parse_fragment_with(
            context,
            input,
            allow_declarative_shadow_roots,
            ParseOptions::default(),
            true,
        )
    }

    /// [`Dom::parse_fragment`] through html5ever's tree builder, whatever the input.
    pub(crate) fn parse_fragment_full(
        &mut self,
        context: NodeId,
        input: &str,
        allow_declarative_shadow_roots: bool,
        options: ParseOptions,
    ) -> NodeId {
        self.parse_fragment_with(
            context,
            input,
            allow_declarative_shadow_roots,
            options,
            false,
        )
    }

    fn parse_fragment_with(
        &mut self,
        context: NodeId,
        input: &str,
        allow_declarative_shadow_roots: bool,
        options: ParseOptions,
        run_scripts: bool,
    ) -> NodeId {
        let context_doc = self.node_document(context);
        // 1-3. A new HTML document with the context document's mode.
        let temp = self.create_document(DocumentKind::Html);
        let mode = self.quirks_mode(context_doc);
        self.set_quirks_mode(temp, mode);
        self.set_allow_declarative_shadow_roots(temp, allow_declarative_shadow_roots);
        // 11. The form element pointer.
        let form = self
            .inclusive_ancestors(context)
            .find(|&a| self.is_html_element_named(a, &local_name!("form")));
        // The parse happens in a document no script can reach, and the nodes then move into a new
        // fragment: the insertions, removals and adoptions of those steps concern nobody (no wrapper,
        // range, observer or custom element definition can see that document). The insertion of
        // the fragment where it goes reports the nodes.
        let quiet = std::mem::replace(&mut self.quiet_tree_reactions, true);
        with_cell(self, |cell| {
            let mut sink = DomSink::new(cell, temp);
            sink.fragment = true;
            sink.run_scripts = run_scripts;
            // 4-10, 12.
            let tb = TreeBuilder::new_for_fragment(
                sink,
                context,
                form,
                tree_builder_opts(options, mode),
            );
            let state = tb.tokenizer_state_for_context_elem(options.scripting_enabled);
            let tokenizer = Tokenizer::new(
                tb,
                TokenizerOpts {
                    initial_state: Some(state),
                    ..Default::default()
                },
            );
            let queue = BufferQueue::default();
            if !input.is_empty() {
                queue.push_back(StrTendril::from_slice(input));
            }
            while tokenizer.feed(&queue) != TokenizerResult::Done {}
            tokenizer.end();
        });
        // 14-15. The root's children move into a fragment of the context's document.
        let root = self.document_element(temp).expect("fragment parser root");
        let fragment = self.create_document_fragment(context_doc);
        self.move_parsed_children(root, fragment, context_doc);
        self.remove(root, true);
        self.free_subtree(root);
        self.free_subtree(temp);
        self.quiet_tree_reactions = quiet;
        fragment
    }

    /// The context element for fragment parsing into `node`: the node itself, or a shadow root's host.
    fn fragment_context(&self, node: NodeId) -> DomResult<NodeId> {
        match &self.n(node).data {
            NodeData::Element(_) => Ok(node),
            NodeData::ShadowRoot(s) => Ok(s.host),
            _ => Err(DomError::NotSupported),
        }
    }

    /// The fragment parsing algorithm (DOM Parsing): the HTML fragment parser for a context in an HTML
    /// document, the XML one (which can throw a SyntaxError) otherwise.
    pub fn fragment_parse(&mut self, context: NodeId, markup: &str) -> DomResult<NodeId> {
        if self.is_html_document(self.node_document(context)) {
            Ok(self.parse_fragment(context, markup, false, ParseOptions::default()))
        } else {
            self.parse_xml_fragment(context, markup)
        }
    }

    /// `innerHTML` setter on an Element or ShadowRoot (HTML §13.3 / DOM Parsing): parses `markup` with the
    /// element (or the shadow root's host) as context and replaces the children (of the template contents
    /// for a `template`).
    pub fn set_inner_html(&mut self, node: NodeId, markup: &str) -> DomResult<()> {
        let context = self.fragment_context(node)?;
        let fragment = self.fragment_parse(context, markup)?;
        let target = if self.is_shadow_root(node) {
            node
        } else {
            self.template_contents(node).unwrap_or(node)
        };
        self.insert_parsed_fragment(fragment, |dom| dom.replace_all(Some(fragment), target));
        Ok(())
    }

    /// Runs `insert`, an insertion of `fragment` (which the fragment parsing algorithm just made and
    /// no script has seen) with the fragment known as such ([`Dom::unreached_fragment`]), then
    /// frees the fragment.
    fn insert_parsed_fragment<R>(
        &mut self,
        fragment: NodeId,
        insert: impl FnOnce(&mut Dom) -> R,
    ) -> R {
        let outer = self.unreached_fragment.replace(fragment);
        let r = insert(self);
        self.unreached_fragment = outer;
        self.free_subtree(fragment);
        r
    }

    /// `setHTMLUnsafe(html)` on an Element or ShadowRoot: like `innerHTML` with declarative shadow roots.
    pub fn set_html_unsafe(&mut self, node: NodeId, markup: &str) -> DomResult<()> {
        let context = self.fragment_context(node)?;
        let fragment = self.parse_fragment(context, markup, true, ParseOptions::default());
        let target = if self.is_shadow_root(node) {
            node
        } else {
            self.template_contents(node).unwrap_or(node)
        };
        self.insert_parsed_fragment(fragment, |dom| dom.replace_all(Some(fragment), target));
        Ok(())
    }

    /// `outerHTML` setter.
    pub fn set_outer_html(&mut self, element: NodeId, markup: &str) -> DomResult<()> {
        // 2-3.
        let Some(parent) = self.parent(element) else {
            return Ok(());
        };
        if self.is_document(parent) {
            return Err(DomError::NoModificationAllowed);
        }
        // 4.
        let context = if self.is_document_fragment(parent) {
            let doc = self.node_document(element);
            self.create_html_element(doc, local_name!("body"))
        } else {
            parent
        };
        // 5-6.
        let fragment = match self.fragment_parse(context, markup) {
            Ok(f) => f,
            Err(e) => {
                if context != parent {
                    self.free_subtree(context);
                }
                return Err(e);
            }
        };
        let r = self
            .insert_parsed_fragment(fragment, |dom| dom.replace_child(parent, fragment, element));
        if context != parent {
            self.free_subtree(context);
        }
        r.map(drop)
    }

    /// `Element.insertAdjacentHTML(position, string)`.
    pub fn insert_adjacent_html(
        &mut self,
        element: NodeId,
        position: AdjacentPosition,
        markup: &str,
    ) -> DomResult<()> {
        let mut context = match position {
            AdjacentPosition::BeforeBegin | AdjacentPosition::AfterEnd => {
                match self.parent(element) {
                    Some(p) if !self.is_document(p) => p,
                    _ => return Err(DomError::NoModificationAllowed),
                }
            }
            AdjacentPosition::AfterBegin | AdjacentPosition::BeforeEnd => element,
        };
        let mut temp_body = None;
        let needs_body = !self.is_element(context)
            || (self.is_html_document(self.node_document(context))
                && self.is_html_element_named(context, &local_name!("html")));
        if needs_body {
            let doc = self.node_document(element);
            context = self.create_html_element(doc, local_name!("body"));
            temp_body = Some(context);
        }
        let fragment = match self.fragment_parse(context, markup) {
            Ok(f) => f,
            Err(e) => {
                if let Some(b) = temp_body {
                    self.free_subtree(b);
                }
                return Err(e);
            }
        };
        let r = self.insert_parsed_fragment(fragment, |dom| match position {
            AdjacentPosition::BeforeBegin => {
                let parent = dom.parent(element).expect("checked");
                dom.insert_before(parent, fragment, Some(element)).map(drop)
            }
            AdjacentPosition::AfterBegin => {
                let first = dom.first_child(element);
                dom.insert_before(element, fragment, first).map(drop)
            }
            AdjacentPosition::BeforeEnd => dom.append_child(element, fragment).map(drop),
            AdjacentPosition::AfterEnd => {
                let parent = dom.parent(element).expect("checked");
                let next = dom.next_sibling(element);
                dom.insert_before(parent, fragment, next).map(drop)
            }
        });
        if let Some(b) = temp_body {
            self.free_subtree(b);
        }
        r
    }
}
