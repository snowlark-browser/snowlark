// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A fast path for the HTML fragment parsing algorithm (HTML §13.4) behind `innerHTML`,
//! `outerHTML`, `insertAdjacentHTML` and `createContextualFragment`.
//!
//! Most markup scripts set is plain: well-nested common elements, quoted attributes, text with a few
//! character references. For that markup the algorithm's result is fixed by a handful of the "in
//! body" insertion mode's rules, without any of its error recovery: each start tag inserts an
//! element under the current node, each end tag pops the current node, and text is inserted where
//! it is. This parser recognizes exactly such input and builds the nodes straight into the fragment,
//! in the context's document: no temporary document, tokenizer, tree builder or tendrils, and no
//! move of the parsed nodes afterwards.
//!
//! On anything outside that subset it gives up and the caller runs the full parser on the whole
//! input, as if this one had not run (the nodes it made are freed and the reactions it queued
//! dropped). The subset is chosen so that inside it the full algorithm cannot do anything else:
//! - The context is an HTML element for which "reset the insertion mode appropriately" gives "in
//!   body" and the tokenizer starts in the data state.
//! - Start tags are a fixed set of elements whose "in body" rules are either "insert an element" or
//!   that with a "close a p element", which is given up on when it would close anything (a `p` in
//!   button scope); likewise the `li`, heading, `a` and `button` rules give up when they would
//!   close or reparent anything. No custom elements, no `is` attribute, no foreign content.
//! - End tags must close the current node; void elements have none. Formatting elements closed in
//!   order never need "reconstruct the active formatting elements", and the adoption agency
//!   algorithm then only pops the current node.
//! - No comments, doctypes, CDATA, processing instructions, NUL or CR; no `<` that is not a tag;
//!   no duplicate, unquoted-with-special-characters or valueless-`=` attributes; character
//!   references only in their exact semicolon-terminated forms (named ones from html5ever's own
//!   table) and code points the tokenizer passes through unchanged.
//!
//! The tests compare this parser with the full one on generated markup: whenever this one accepts
//! input, both build the same nodes and queue the same reactions.

use html5ever::{local_name, ns, LocalName, QualName};

use crate::dom::Dom;
use crate::node::{Attr, NodeId};

/// How the "in body" insertion mode treats a start tag of the element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// `address`, `article`, `aside`, `blockquote`, `div`, `figcaption`, `figure`, `footer`,
    /// `header`, `main`, `nav`, `ol`, `p`, `section`, `ul`: "close a p element" if one is in button
    /// scope, then insert. Special.
    Block,
    /// `h1`-`h6`: as [`Kind::Block`], and a heading as the current node is popped first. Special.
    Heading,
    /// `li`: closes an open `li` (the list item loop), and a `p` in button scope. Special.
    Li,
    /// `button`: closes a `button` in scope. Special.
    Button,
    /// `a`: runs the adoption agency algorithm for an `a` in the list of active formatting elements.
    A,
    /// `b`, `code`, `em`, `i`, `s`, `small`, `strong`, `u`: formatting elements.
    Formatting,
    /// "Any other start tag": ordinary elements, not special.
    Ordinary,
    /// `br`, `img`, `input`, `wbr`: inserted and popped at once. Special.
    Void,
    /// `hr`: "close a p element" if one is in button scope, then as [`Kind::Void`]. Special.
    VoidBlock,
}

impl Kind {
    fn special(self) -> bool {
        !matches!(self, Kind::A | Kind::Formatting | Kind::Ordinary)
    }
    fn void(self) -> bool {
        matches!(self, Kind::Void | Kind::VoidBlock)
    }
}

/// The element a (lowercase) tag name is, when it is one of the subset's.
fn element(name: &[u8]) -> Option<(LocalName, Kind)> {
    use Kind::*;
    Some(match name {
        b"div" => (local_name!("div"), Block),
        b"p" => (local_name!("p"), Block),
        b"ul" => (local_name!("ul"), Block),
        b"ol" => (local_name!("ol"), Block),
        b"section" => (local_name!("section"), Block),
        b"article" => (local_name!("article"), Block),
        b"aside" => (local_name!("aside"), Block),
        b"header" => (local_name!("header"), Block),
        b"footer" => (local_name!("footer"), Block),
        b"nav" => (local_name!("nav"), Block),
        b"main" => (local_name!("main"), Block),
        b"address" => (local_name!("address"), Block),
        b"blockquote" => (local_name!("blockquote"), Block),
        b"figure" => (local_name!("figure"), Block),
        b"figcaption" => (local_name!("figcaption"), Block),
        b"h1" => (local_name!("h1"), Heading),
        b"h2" => (local_name!("h2"), Heading),
        b"h3" => (local_name!("h3"), Heading),
        b"h4" => (local_name!("h4"), Heading),
        b"h5" => (local_name!("h5"), Heading),
        b"h6" => (local_name!("h6"), Heading),
        b"li" => (local_name!("li"), Li),
        b"button" => (local_name!("button"), Button),
        b"a" => (local_name!("a"), A),
        b"b" => (local_name!("b"), Formatting),
        b"i" => (local_name!("i"), Formatting),
        b"em" => (local_name!("em"), Formatting),
        b"strong" => (local_name!("strong"), Formatting),
        b"small" => (local_name!("small"), Formatting),
        b"code" => (local_name!("code"), Formatting),
        b"s" => (local_name!("s"), Formatting),
        b"u" => (local_name!("u"), Formatting),
        b"span" => (local_name!("span"), Ordinary),
        b"label" => (local_name!("label"), Ordinary),
        b"abbr" => (local_name!("abbr"), Ordinary),
        b"cite" => (local_name!("cite"), Ordinary),
        b"data" => (local_name!("data"), Ordinary),
        b"dfn" => (local_name!("dfn"), Ordinary),
        b"kbd" => (local_name!("kbd"), Ordinary),
        b"mark" => (local_name!("mark"), Ordinary),
        b"q" => (local_name!("q"), Ordinary),
        b"samp" => (local_name!("samp"), Ordinary),
        b"sub" => (local_name!("sub"), Ordinary),
        b"sup" => (local_name!("sup"), Ordinary),
        b"time" => (local_name!("time"), Ordinary),
        b"var" => (local_name!("var"), Ordinary),
        b"ins" => (local_name!("ins"), Ordinary),
        b"del" => (local_name!("del"), Ordinary),
        b"br" => (local_name!("br"), Void),
        b"img" => (local_name!("img"), Void),
        b"input" => (local_name!("input"), Void),
        b"wbr" => (local_name!("wbr"), Void),
        b"hr" => (local_name!("hr"), VoidBlock),
        _ => return None,
    })
}

/// Whether fragment parsing with `local` (an HTML element) as context starts in the "in body"
/// insertion mode and the data state ("reset the insertion mode appropriately" with `last` set,
/// and the tokenizer's state for the context element).
fn in_body_context(local: &LocalName) -> bool {
    !matches!(
        &**local,
        "select"
            | "td"
            | "th"
            | "tr"
            | "tbody"
            | "thead"
            | "tfoot"
            | "caption"
            | "colgroup"
            | "table"
            | "template"
            | "head"
            | "html"
            | "frameset"
            | "title"
            | "textarea"
            | "style"
            | "xmp"
            | "iframe"
            | "noembed"
            | "noframes"
            | "script"
            | "noscript"
            | "plaintext"
    )
}

/// An open element: the stack of open elements above the fragment's `html` root.
struct Open {
    node: NodeId,
    name: LocalName,
    kind: Kind,
}

/// The input does not stay in the subset.
struct GiveUp;

/// Fragment parsing of `input` with `context` (an element in an HTML document) on the fast path:
/// the new DocumentFragment of `context`'s node document holding the nodes, or `None` when the
/// input is outside the subset (nothing is left behind then).
pub(crate) fn parse(dom: &mut Dom, context: NodeId, input: &str) -> Option<NodeId> {
    parse_with_flags(dom, context, input, 0)
}

/// [`parse`], with `element_flags` set on each element it makes (the document parser's
/// [`crate::node::element_flags::PARSER_INSERTED`], for [`Dom::parse_html_document_inert`]).
pub(crate) fn parse_with_flags(
    dom: &mut Dom,
    context: NodeId,
    input: &str,
    element_flags: u8,
) -> Option<NodeId> {
    {
        let name = dom.element_name(context);
        if name.ns != ns!(html) || !in_body_context(&name.local) {
            return None;
        }
    }
    let document = dom.node_document(context);
    let fragment = dom.create_document_fragment(document);
    let reactions = dom.reactions.len();
    let mut p = Parser {
        dom,
        document,
        fragment,
        input: input.as_bytes(),
        src: input,
        at: 0,
        stack: Vec::new(),
        text: String::new(),
        attrs: Vec::new(),
        attr_text: String::new(),
        element_flags,
    };
    match p.run() {
        Ok(()) => Some(fragment),
        Err(GiveUp) => {
            dom.reactions.truncate(reactions);
            dom.free_subtree(fragment);
            None
        }
    }
}

struct Parser<'a> {
    dom: &'a mut Dom,
    document: NodeId,
    fragment: NodeId,
    input: &'a [u8],
    src: &'a str,
    at: usize,
    stack: Vec<Open>,
    /// Text not inserted yet (a run between tags).
    text: String,
    /// The current start tag's attributes: names lowercase, values decoded, as ranges of
    /// `attr_text`.
    attrs: Vec<(LocalName, std::ops::Range<usize>)>,
    /// The current start tag's attribute values, one after another (one buffer for the whole
    /// parse: the element's attribute list copies each value into its own storage).
    attr_text: String,
    /// Set on each element made.
    element_flags: u8,
}

#[inline]
fn is_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0c' | b' ')
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.input.get(self.at).copied()
    }

    fn current(&self) -> NodeId {
        self.stack.last().map_or(self.fragment, |o| o.node)
    }

    fn run(&mut self) -> Result<(), GiveUp> {
        while let Some(b) = self.peek() {
            match b {
                b'<' => {
                    self.flush_text();
                    self.at += 1;
                    match self.peek() {
                        Some(b'/') => {
                            self.at += 1;
                            self.end_tag()?;
                        }
                        Some(c) if c.is_ascii_alphabetic() => self.start_tag()?,
                        // (comments, doctypes, bogus comments, a `<` that is text)
                        _ => return Err(GiveUp),
                    }
                }
                b'&' => {
                    self.at += 1;
                    let mut out = std::mem::take(&mut self.text);
                    let r = self.char_ref(&mut out);
                    self.text = out;
                    r?;
                }
                b'\0' | b'\r' => return Err(GiveUp),
                _ => {
                    // A run of plain text, up to the next `<`, `&`, NUL or CR (all ASCII, so the
                    // run ends on a character boundary).
                    let start = self.at;
                    let rest = &self.input[start..];
                    let len = rest
                        .iter()
                        .position(|&c| matches!(c, b'<' | b'&' | b'\0' | b'\r'))
                        .unwrap_or(rest.len());
                    self.text.push_str(&self.src[start..start + len]);
                    self.at += len;
                }
            }
        }
        self.flush_text();
        // At the end of the input the open elements stay where they are.
        Ok(())
    }

    /// Inserts the pending text into the current node.
    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let t = self.dom.create_text_node(self.document, &self.text);
        self.text.clear();
        let parent = self.current();
        self.dom.link_before(t, parent, None);
    }

    /// A tag name (after `<` or `</`): lowercase.
    fn tag_name(&mut self) -> Result<(LocalName, Kind), GiveUp> {
        let start = self.at;
        while let Some(b) = self.peek() {
            if is_space(b) || b == b'/' || b == b'>' {
                break;
            }
            self.at += 1;
        }
        let raw = &self.input[start..self.at];
        if raw.len() > 10 {
            return Err(GiveUp);
        }
        let mut lower = [0u8; 10];
        for (d, s) in lower.iter_mut().zip(raw) {
            *d = s.to_ascii_lowercase();
        }
        element(&lower[..raw.len()]).ok_or(GiveUp)
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(is_space) {
            self.at += 1;
        }
    }

    fn end_tag(&mut self) -> Result<(), GiveUp> {
        if !self.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
            return Err(GiveUp);
        }
        let (name, kind) = self.tag_name()?;
        self.skip_space();
        if self.peek() != Some(b'>') {
            return Err(GiveUp);
        }
        self.at += 1;
        // The end tag closes the current node, which has its name (never a void element's).
        match self.stack.last() {
            Some(o) if o.name == name && !kind.void() => {
                self.stack.pop();
                Ok(())
            }
            _ => Err(GiveUp),
        }
    }

    fn start_tag(&mut self) -> Result<(), GiveUp> {
        let (name, kind) = self.tag_name()?;
        self.attrs.clear();
        self.attr_text.clear();
        let self_closing = self.attributes()?;
        if self_closing && !kind.void() {
            return Err(GiveUp);
        }
        self.check_start_tag(kind)?;
        let element = self.dom.create_element_with_name(
            self.document,
            QualName::new(None, ns!(html), name.clone()),
            None,
        );
        if self.attrs.len() > 1 {
            self.dom
                .element_mut(element)
                .attrs
                .reserve_exact(self.attrs.len());
        }
        for (local, value) in self.attrs.drain(..) {
            self.dom.push_parsed_attribute(
                element,
                Attr::new(QualName::new(None, ns!(), local), &self.attr_text[value]),
            );
        }
        if self.element_flags != 0 {
            self.dom.set_element_flag(element, self.element_flags, true);
        }
        let parent = self.current();
        self.dom.link_before(element, parent, None);
        if !kind.void() {
            self.stack.push(Open {
                node: element,
                name,
                kind,
            });
        }
        Ok(())
    }

    /// Gives up where the start tag's "in body" rule would do more than insert the element.
    fn check_start_tag(&self, kind: Kind) -> Result<(), GiveUp> {
        // "If the stack of open elements has a p element in button scope, then close a p element."
        let closes_p = matches!(
            kind,
            Kind::Block | Kind::Heading | Kind::Li | Kind::VoidBlock
        );
        if closes_p {
            for o in self.stack.iter().rev() {
                if o.name == local_name!("p") {
                    return Err(GiveUp);
                }
                if o.kind == Kind::Button {
                    break;
                }
            }
        }
        match kind {
            // "If the current node is an HTML element whose tag name is one of h1...h6": popped.
            Kind::Heading => {
                if self.stack.last().is_some_and(|o| o.kind == Kind::Heading) {
                    return Err(GiveUp);
                }
            }
            // The list item loop: an open `li` before a special element other than address, div
            // or p is closed.
            Kind::Li => {
                for o in self.stack.iter().rev() {
                    if o.kind == Kind::Li {
                        return Err(GiveUp);
                    }
                    if o.kind.special() && !matches!(&*o.name, "address" | "div" | "p") {
                        break;
                    }
                }
            }
            // "If the stack of open elements has a button element in scope": closed (nothing on
            // this stack but `html` bounds the scope). An `a` in the list of active formatting
            // elements (with elements closed in order, exactly the open ones) runs the adoption
            // agency algorithm.
            Kind::Button | Kind::A if self.stack.iter().any(|o| o.kind == kind) => {
                return Err(GiveUp);
            }
            _ => {}
        }
        Ok(())
    }

    /// The attributes of a start tag up to and including its `>`: whether it ended with `/>`.
    fn attributes(&mut self) -> Result<bool, GiveUp> {
        loop {
            self.skip_space();
            match self.peek() {
                Some(b'>') => {
                    self.at += 1;
                    return Ok(false);
                }
                Some(b'/') => {
                    self.at += 1;
                    if self.peek() == Some(b'>') {
                        self.at += 1;
                        return Ok(true);
                    }
                    return Err(GiveUp);
                }
                Some(_) => self.attribute()?,
                None => return Err(GiveUp),
            }
        }
    }

    fn attribute(&mut self) -> Result<(), GiveUp> {
        let start = self.at;
        while let Some(b) = self.peek() {
            match b {
                b'=' | b'>' | b'/' => break,
                b if is_space(b) => break,
                b'"' | b'\'' | b'<' | b'\0' | b'\r' => return Err(GiveUp),
                _ => self.at += 1,
            }
        }
        if self.at == start {
            // (`=` first: part of the name, a parse error)
            return Err(GiveUp);
        }
        let raw = &self.src[start..self.at];
        let local = if raw.bytes().any(|b| b.is_ascii_uppercase()) {
            LocalName::from(raw.to_ascii_lowercase())
        } else {
            LocalName::from(raw)
        };
        // (`is` makes a customized built-in element; a duplicate is dropped with a flag)
        if &*local == "is" || self.attrs.iter().any(|(n, _)| *n == local) {
            return Err(GiveUp);
        }
        self.skip_space();
        // (on giving up the buffer is dropped with the parser)
        let mut value = std::mem::take(&mut self.attr_text);
        let start = value.len();
        if self.peek() == Some(b'=') {
            self.at += 1;
            self.skip_space();
            match self.peek() {
                Some(q @ (b'"' | b'\'')) => {
                    self.at += 1;
                    loop {
                        match self.peek() {
                            Some(b) if b == q => {
                                self.at += 1;
                                break;
                            }
                            Some(b'&') => {
                                self.at += 1;
                                self.char_ref(&mut value)?;
                            }
                            Some(b'\0' | b'\r') | None => return Err(GiveUp),
                            Some(_) => self.plain(&mut value, &[q, b'&', b'\0', b'\r']),
                        }
                    }
                    // (another attribute right after the quote is a parse error)
                    if !self
                        .peek()
                        .is_some_and(|b| is_space(b) || b == b'>' || b == b'/')
                    {
                        return Err(GiveUp);
                    }
                }
                Some(b'>') | None => return Err(GiveUp),
                Some(_) => loop {
                    match self.peek() {
                        Some(b) if is_space(b) || b == b'>' => break,
                        Some(b'&') => {
                            self.at += 1;
                            self.char_ref(&mut value)?;
                        }
                        Some(b'"' | b'\'' | b'<' | b'=' | b'`' | b'\0' | b'\r') | None => {
                            return Err(GiveUp)
                        }
                        Some(_) => self.plain(&mut value, b"\t\n\x0c >&\"'<=`\0\r"),
                    }
                },
            }
        }
        self.attrs.push((local, start..value.len()));
        self.attr_text = value;
        Ok(())
    }

    /// Appends input up to the next of `stops` (all ASCII) to `out`.
    fn plain(&mut self, out: &mut String, stops: &[u8]) {
        let start = self.at;
        let rest = &self.input[start..];
        let len = rest
            .iter()
            .position(|c| stops.contains(c))
            .unwrap_or(rest.len());
        out.push_str(&self.src[start..start + len]);
        self.at += len;
    }

    /// A character reference after its `&`, appended to `out`: a lone `&` (one not followed by an
    /// ASCII alphanumeric or `#`) is itself; numeric and named references are taken only in their
    /// semicolon-terminated forms.
    fn char_ref(&mut self, out: &mut String) -> Result<(), GiveUp> {
        match self.peek() {
            Some(b'#') => {
                self.at += 1;
                let hex = matches!(self.peek(), Some(b'x' | b'X'));
                if hex {
                    self.at += 1;
                }
                let start = self.at;
                let mut value: u32 = 0;
                while let Some(b) = self.peek() {
                    let d = if hex {
                        (b as char).to_digit(16)
                    } else {
                        (b as char).to_digit(10)
                    };
                    let Some(d) = d else { break };
                    if self.at - start >= 8 {
                        return Err(GiveUp);
                    }
                    value = value * if hex { 16 } else { 10 } + d;
                    self.at += 1;
                }
                if self.at == start || self.peek() != Some(b';') {
                    return Err(GiveUp);
                }
                self.at += 1;
                // Code points the tokenizer passes through as they are: no NUL, controls (other
                // than tab, LF and FF), C1 range, surrogates or out-of-range values.
                let plain = matches!(value, 0x09 | 0x0a | 0x0c | 0x20..=0x7e | 0xa0..=0xd7ff)
                    || (0xe000..=0x10ffff).contains(&value);
                match char::from_u32(value) {
                    Some(c) if plain => {
                        out.push(c);
                        Ok(())
                    }
                    _ => Err(GiveUp),
                }
            }
            Some(b) if b.is_ascii_alphanumeric() => {
                let start = self.at;
                while self.peek().is_some_and(|b| b.is_ascii_alphanumeric()) {
                    self.at += 1;
                }
                if self.peek() != Some(b';') || self.at - start > 31 {
                    return Err(GiveUp);
                }
                self.at += 1;
                let name = &self.src[start..self.at];
                match html5ever::data::NAMED_ENTITIES.get(name) {
                    Some(&(c1, c2)) if c1 != 0 => {
                        out.extend(char::from_u32(c1));
                        if c2 != 0 {
                            out.extend(char::from_u32(c2));
                        }
                        Ok(())
                    }
                    _ => Err(GiveUp),
                }
            }
            _ => {
                out.push('&');
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::node::NodeData;
    use crate::records::Reaction;
    use crate::{DocumentKind, ParseOptions};

    /// A node as the comparison sees it: its data and flags, and its children.
    fn shape(
        dom: &Dom,
        id: NodeId,
        index: &mut rustc_hash::FxHashMap<NodeId, usize>,
        out: &mut Vec<String>,
    ) {
        index.insert(id, index.len());
        let n = dom.n(id);
        let line = match &n.data {
            NodeData::Element(e) => format!(
                "<{:?} {} {:?} flags {} ef {} {:?}>",
                e.name.ns,
                e.name.local,
                e.attrs
                    .iter()
                    .map(|a| format!("{:?}:{}={:?}", a.name.ns, a.name.local, &*a.value))
                    .collect::<Vec<_>>(),
                n.flags,
                e.flags,
                e.custom_state,
            ),
            NodeData::Text(t) => format!("#text {t:?} flags {}", n.flags),
            other => format!("{other:?}"),
        };
        out.push(format!("{line} doc {:?}", n.document));
        let kids: Vec<NodeId> = dom.children(id).collect();
        out.push(format!("[{}", kids.len()));
        for k in kids {
            shape(dom, k, index, out);
        }
        out.push("]".into());
    }

    fn reactions(list: &[Reaction], index: &rustc_hash::FxHashMap<NodeId, usize>) -> Vec<String> {
        list.iter()
            .map(|r| match r {
                Reaction::AttributeChanged {
                    element,
                    local_name,
                    namespace,
                    old_value,
                    new_value,
                } => format!(
                    "attr #{:?} {local_name} {namespace:?} {old_value:?} {new_value:?}",
                    index.get(element)
                ),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// Runs both parsers on `markup` with a `context` element: whether the fast path took it.
    /// Where it did, both made the same nodes and queued the same reactions; where it did not, it
    /// left nothing behind.
    fn compare(context: &str, markup: &str) -> bool {
        let mut dom = Dom::new();
        let doc = dom.create_document(DocumentKind::Html);
        let ctx = dom.create_element(doc, context, None).unwrap();
        dom.set_reactions_enabled(true);
        dom.take_reactions();
        let count = dom.node_count();
        let fast = parse(&mut dom, ctx, markup);
        let fast_reactions = dom.take_reactions();
        let Some(fast) = fast else {
            assert_eq!(dom.node_count(), count, "{context} {markup:?}: nodes left");
            assert!(
                fast_reactions.is_empty(),
                "{context} {markup:?}: reactions left"
            );
            return false;
        };
        let full = dom.parse_fragment_full(ctx, markup, false, ParseOptions::default());
        let full_reactions = dom.take_reactions();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let (mut ia, mut ib) = (Default::default(), Default::default());
        shape(&dom, fast, &mut ia, &mut a);
        shape(&dom, full, &mut ib, &mut b);
        assert_eq!(a, b, "{context} {markup:?}: trees differ");
        assert_eq!(
            reactions(&fast_reactions, &ia),
            reactions(&full_reactions, &ib),
            "{context} {markup:?}: reactions differ"
        );
        true
    }

    #[test]
    fn plain_markup_takes_the_fast_path_and_matches_the_tree_builder() {
        for (context, markup) in [
            ("div", ""),
            ("div", "text only"),
            (
                "ul",
                r#"<li data-id="1"><div class="view"><input class="toggle" type="checkbox" checked><label>Buy milk</label><button class="destroy"></button></div><input class="edit" value="Buy milk"></li>"#,
            ),
            ("div", "<p>One &amp; two &lt;three&gt; &quot;q&quot; &#65;&#x42;&nbsp;&notin;&#x1F600;</p>"),
            ("span", "<b>bold <i>both</i></b> <a href='/x?a=1&amp;b=2'>link</a>"),
            ("body", "<section><h1>T</h1><p>a</p><ul><li>x</li><li>y</li></ul></section>"),
            ("div", "<DIV CLASS=Up id=x>Mixed<BR/><img src=a.png alt=\"\"></DIV>"),
            ("p", "<div>a div in a p context</div>"),
            ("li", "<ul><li>nested<ul><li>deeper</li></ul></li></ul>"),
            ("div", "<div><span>unclosed"),
            ("div", "a & b? <b>&</b> &#10;"),
            ("button", "<span>in a button</span><hr>"),
            ("div", "<p><button><div>block in a button in a p</div></button></p>"),
            ("div", "<ul><li><div><ul><li>list in a list</li></ul></div></li></ul>"),
            ("div", "<input disabled value=x><input value = 'y' >"),
            ("div", "<b><b><b><b>four</b></b></b></b>after"),
        ] {
            assert!(compare(context, markup), "{context} {markup:?}: not taken");
        }
    }

    #[test]
    fn markup_the_algorithm_recovers_from_falls_back() {
        for (context, markup) in [
            ("div", "<!-- comment -->"),
            ("div", "<p><div>closes the p</div>"),
            ("div", "<p>a<p>b"),
            ("div", "<li>a<li>b"),
            ("div", "<a>a<a>b</a></a>"),
            ("div", "<b><i>misnested</b></i>"),
            ("div", "</span>"),
            ("div", "<br></br>"),
            ("div", "<div/>"),
            ("div", "<table><tr><td>x</table>"),
            ("div", "<x-custom></x-custom>"),
            ("div", "<button is=x-b></button>"),
            ("div", "<span a=1 a=2></span>"),
            ("div", "a\r\nb"),
            ("div", "a\0b"),
            ("div", "AT&T"),
            ("div", "&amp"),
            ("div", "&#128;"),
            ("div", "&#0;"),
            ("div", "a < b"),
            ("div", "<svg></svg>"),
            ("div", "<span title=a\"b></span>"),
            ("div", "<span a='1'b='2'></span>"),
            ("div", "<span =a></span>"),
            ("div", "<h1><h2>x</h2></h1>"),
            ("table", "<tr>"),
            ("select", "<span>"),
            ("textarea", "<b>"),
            ("template", "<span>"),
            ("div", "<span"),
        ] {
            assert!(!compare(context, markup), "{context} {markup:?}: taken");
        }
    }

    /// Parses `markup` as `DOMParser` does, on the document fast path and with the full parser:
    /// whether the fast path took it. Where it did, both made the same document; where it did not,
    /// it left no node and no reaction behind.
    fn compare_document(markup: &str) -> bool {
        let mut dom = Dom::new();
        dom.set_reactions_enabled(true);
        dom.take_reactions();
        let count = dom.node_count();
        let fast = dom.parse_html_document_fast(markup);
        let Some(fast) = fast else {
            assert_eq!(dom.node_count(), count, "{markup:?}: nodes left");
            assert!(
                dom.take_reactions().is_empty(),
                "{markup:?}: reactions left"
            );
            return false;
        };
        let full = dom.parse_html_str_with(
            markup,
            ParseOptions {
                scripting_enabled: false,
                iframe_srcdoc: false,
            },
            false,
        );
        let tree = |doc: NodeId| -> Vec<String> {
            let mut out = Vec::new();
            let kids: Vec<NodeId> = dom.children(doc).collect();
            out.push(format!("[{}", kids.len()));
            for k in kids {
                shape(&dom, k, &mut Default::default(), &mut out);
            }
            // (each document is its own node document)
            out.into_iter()
                .map(|l| l.split(" doc ").next().unwrap().to_owned())
                .collect()
        };
        assert_eq!(tree(fast), tree(full), "{markup:?}: documents differ");
        assert_eq!(dom.quirks_mode(fast), dom.quirks_mode(full), "{markup:?}");
        assert_eq!(dom.content_type(fast), dom.content_type(full), "{markup:?}");
        assert!(dom.is_connected(dom.first_child(fast).unwrap()));
        true
    }

    #[test]
    fn a_document_of_plain_markup_takes_the_fast_path_and_matches_the_parser() {
        for markup in [
            "",
            "   \n\t ",
            "text only",
            " \r\n <li data-id=\"1\" class=\"\"><div class=\"view\"><input class=\"toggle\" type=\"checkbox\"><label>Buy milk</label><button class=\"destroy\"></button></div></li>\n",
            "<strong>0</strong> items left",
            "<ul><li>x</li></ul>  trailing  ",
            "<div><span>unclosed",
        ] {
            assert!(compare_document(markup), "{markup:?}: not taken");
        }
    }

    #[test]
    fn a_document_the_tree_builder_must_build_falls_back() {
        for markup in [
            "<!doctype html><p>x</p>",
            "\u{feff}<p>a</p>",
            " &#10;<p>a</p>",
            "<html><body>x</body></html>",
            "<body class=x>",
            "<head></head><p>x</p>",
            "<title>t</title><p>x</p>",
            "  <meta charset=utf-8><div>x</div>",
            "<link rel=stylesheet href=a.css>",
            "<style>p{}</style>",
            "<script>1</script>",
            "<table><tr><td>x</table>",
            "<p>x</p></body>",
            "<!-- c --><p>x</p>",
            "<template shadowrootmode=open><p>x</p></template>",
        ] {
            assert!(!compare_document(markup), "{markup:?}: taken");
        }
    }

    #[test]
    fn generated_documents_the_fast_path_takes_match_the_parser() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let (mut taken, mut total) = (0, 0);
        for _ in 0..20_000 {
            let mut markup = String::new();
            for _ in 0..rng.below(3) {
                markup.push_str(rng.pick(&[" ", "\n", "\t", "\r", "\x0c", "x", "<!-- -->"]));
            }
            markup.push_str(&generate(&mut rng));
            total += 1;
            if compare_document(&markup) {
                taken += 1;
            }
        }
        println!("document fast path took {taken} of {total}");
        assert!(taken > total / 4, "taken {taken} of {total}");
        assert!(taken < total * 9 / 10, "taken {taken} of {total}");
    }

    /// A tiny deterministic generator (xorshift64*).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len())]
        }
    }

    const TAGS: &[&str] = &[
        "div",
        "p",
        "ul",
        "ol",
        "li",
        "span",
        "a",
        "b",
        "i",
        "em",
        "strong",
        "small",
        "code",
        "s",
        "u",
        "label",
        "button",
        "h1",
        "h2",
        "section",
        "header",
        "footer",
        "nav",
        "main",
        "aside",
        "article",
        "address",
        "blockquote",
        "figure",
        "figcaption",
        "time",
        "q",
        "sub",
        "ins",
        "del",
        "Div",
        "SPAN",
        "LI",
        "br",
        "img",
        "input",
        "wbr",
        "hr",
        "table",
        "tr",
        "td",
        "select",
        "option",
        "template",
        "textarea",
        "pre",
        "script",
        "style",
        "nobr",
        "font",
        "dd",
        "dt",
        "form",
        "x-el",
        "svg",
        "math",
        "body",
        "html",
        "p2",
        "image",
    ];
    const TEXT: &[&str] = &[
        "a",
        " ",
        "\n",
        "\t",
        "word",
        "&amp;",
        "&lt;b&gt;",
        "&quot;",
        "x&y",
        "&",
        "& ",
        "&#65;",
        "&#x1F600;",
        "&#128;",
        "&#0;",
        "&nbsp;",
        "&notin;",
        "&not",
        "&ampx;",
        "é",
        "€",
        "<",
        "\r",
        "\0",
        ">",
        "=",
        "&#10;",
        "&#xD800;",
    ];
    const ATTR_NAMES: &[&str] = &[
        "class", "id", "href", "data-x", "Title", "checked", "type", "value", "is", "on", "a:b",
        "x-y", "CLASS",
    ];
    const VALUES: &[&str] = &[
        "", "x", "a b", "1&amp;2", "&lt;", "&", "a&b", "&#34;", "&quot;", "é", "x'y", "x\"y",
        "a=b", "`", "&notin;", "&not",
    ];

    /// The subset's tags (some in other cases), its safe text and attribute values: markup made
    /// of these is mostly taken, and fails only on nesting the subset leaves out.
    const CLEAN_TAGS: &[&str] = &[
        "div",
        "p",
        "ul",
        "ol",
        "li",
        "span",
        "a",
        "b",
        "i",
        "em",
        "strong",
        "small",
        "code",
        "s",
        "u",
        "label",
        "button",
        "h1",
        "h3",
        "section",
        "header",
        "footer",
        "nav",
        "main",
        "aside",
        "article",
        "address",
        "blockquote",
        "figure",
        "figcaption",
        "time",
        "q",
        "sub",
        "sup",
        "ins",
        "del",
        "mark",
        "kbd",
        "Div",
        "SPAN",
        "LI",
        "br",
        "img",
        "input",
        "wbr",
        "hr",
    ];
    const CLEAN_TEXT: &[&str] = &[
        "a",
        " ",
        "\n",
        "\t",
        "word",
        "&amp;",
        "&lt;b&gt;",
        "&quot;",
        "& ",
        "&#65;",
        "&#x1F600;",
        "&nbsp;",
        "&notin;",
        "é",
        "€",
        ">",
        "=",
        "&#10;",
        "&AMP;",
        "&#X41;",
        "\"",
        "'",
    ];
    const CLEAN_VALUES: &[&str] = &[
        "",
        "x",
        "a b",
        "1&amp;2",
        "&lt;",
        "& ",
        "&#34;",
        "&quot;",
        "é",
        "/path?q=1",
        "a=b",
        "&notin;",
        "  spaced  ",
    ];
    const CLEAN_NAMES: &[&str] = &[
        "class",
        "id",
        "href",
        "data-x",
        "Title",
        "checked",
        "type",
        "value",
        "x-y",
        "aria-label",
    ];

    fn generate(rng: &mut Rng) -> String {
        // A third of the cases mix in what the subset leaves out.
        let noisy = rng.below(3) == 0;
        let (tags, text, names, values) = if noisy {
            (TAGS, TEXT, ATTR_NAMES, VALUES)
        } else {
            (CLEAN_TAGS, CLEAN_TEXT, CLEAN_NAMES, CLEAN_VALUES)
        };
        let mut s = String::new();
        let mut open: Vec<&str> = Vec::new();
        for _ in 0..rng.below(16) {
            match rng.below(10) {
                0..=3 => {
                    let tag = rng.pick(tags);
                    s.push('<');
                    s.push_str(tag);
                    let mut used: Vec<&str> = Vec::new();
                    for _ in 0..rng.below(3) {
                        let name = rng.pick(names);
                        if !noisy && used.iter().any(|u| u.eq_ignore_ascii_case(name)) {
                            continue;
                        }
                        used.push(name);
                        s.push(' ');
                        s.push_str(name);
                        match rng.below(5) {
                            0 => {}
                            1 => {
                                s.push('=');
                                let v = rng.pick(values).replace([' ', '\'', '"', '='], "");
                                s.push_str(if v.is_empty() { "v" } else { &v });
                            }
                            2 => {
                                s.push_str("='");
                                s.push_str(&rng.pick(values).replace('\'', ""));
                                s.push('\'');
                            }
                            3 => {
                                s.push_str(" = \"");
                                s.push_str(&rng.pick(values).replace('"', ""));
                                s.push('"');
                            }
                            _ => {
                                s.push_str("=\"");
                                s.push_str(&rng.pick(values).replace('"', "&quot;"));
                                s.push('"');
                            }
                        }
                    }
                    let void = matches!(
                        tag.to_ascii_lowercase().as_str(),
                        "br" | "img" | "input" | "wbr" | "hr"
                    );
                    if rng.below(8) == 0 && (noisy || void) {
                        s.push('/');
                    }
                    s.push('>');
                    if !void {
                        open.push(tag);
                    }
                }
                4..=5 => {
                    // Mostly the current element's end tag, sometimes another's.
                    let tag = if (noisy && rng.below(6) == 0) || open.is_empty() {
                        if !noisy {
                            continue;
                        }
                        rng.pick(tags)
                    } else {
                        open.pop().unwrap()
                    };
                    s.push_str("</");
                    s.push_str(tag);
                    if rng.below(10) == 0 {
                        s.push(' ');
                    }
                    s.push('>');
                }
                6 if noisy && rng.below(6) == 0 => s.push_str("<!--c-->"),
                _ => {
                    for _ in 0..1 + rng.below(3) {
                        s.push_str(rng.pick(text));
                    }
                }
            }
        }
        s
    }

    #[test]
    fn generated_markup_the_fast_path_takes_matches_the_tree_builder() {
        let contexts = [
            "div", "p", "ul", "li", "span", "body", "button", "a", "h1", "b", "td", "table",
            "select", "textarea", "template", "x-el",
        ];
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let (mut taken, mut total) = (0, 0);
        for _ in 0..60_000 {
            let markup = generate(&mut rng);
            let context = contexts[rng.below(contexts.len())];
            total += 1;
            if compare(context, &markup) {
                taken += 1;
            }
        }
        println!("fast path took {taken} of {total}");
        // (the generator is built to reach both outcomes often)
        assert!(taken > total / 3, "taken {taken} of {total}");
        assert!(taken < total * 9 / 10, "taken {taken} of {total}");
    }
}

#[cfg(test)]
mod bench {
    use std::time::Instant;

    use super::*;
    use crate::{DocumentKind, ParseOptions};

    /// `cargo test --release -p snowlark-dom fragment_fast::bench -- --ignored --nocapture`: ns per node
    /// of fragment parsing a TodoMVC-like list, fast path against the tree builder (parse and free).
    #[test]
    #[ignore]
    fn fragment_parsing_ns_per_node() {
        let item = r#"<li data-id="7" class="completed"><div class="view"><input class="toggle" type="checkbox" checked><label>Something to do &amp; more</label><button class="destroy"></button></div></li>"#;
        let markup = item.repeat(100);
        let mut dom = Dom::new();
        let doc = dom.create_document(DocumentKind::Html);
        let ul = dom.create_element(doc, "ul", None).unwrap();
        let nodes = {
            let f = parse(&mut dom, ul, &markup).expect("taken");
            let n = dom.owned_nodes(f).len() - 1;
            dom.free_subtree(f);
            n
        };
        for _ in 0..3 {
            let runs = 300;
            let t = Instant::now();
            for _ in 0..runs {
                let f = parse(&mut dom, ul, &markup).unwrap();
                dom.free_subtree(f);
            }
            let fast = t.elapsed().as_nanos() as f64 / (runs * nodes) as f64;
            let t = Instant::now();
            for _ in 0..runs {
                let f = dom.parse_fragment_full(ul, &markup, false, ParseOptions::default());
                dom.free_subtree(f);
            }
            let full = t.elapsed().as_nanos() as f64 / (runs * nodes) as f64;
            println!("{nodes} nodes: fast path {fast:.0} ns/node, tree builder {full:.0} ns/node ({:.1}x)", full / fast);
        }
    }
}
