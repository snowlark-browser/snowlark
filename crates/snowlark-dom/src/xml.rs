// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The XML parser (HTML §13.4 "Parsing XML documents" over XML 1.0 Fifth Edition and Namespaces in XML
//! 1.0 Third Edition): a non-validating parser that checks well-formedness and namespace
//! well-formedness and builds Snowlark nodes as it goes. It stops after every `script` end tag (HTML or
//! SVG) so the page layer can run the script, like [`crate::HtmlParser`].
//!
//! Not fetched: external DTD subsets and external entities. The internal subset's general entity
//! declarations are honored (their replacement text is parsed as content); a document whose DOCTYPE
//! names one of the XHTML public identifiers gets the HTML named character references (HTML §13.4).
//! Attribute defaults from the DTD are not applied.

use std::collections::HashMap;

use html5ever::{local_name, ns, LocalName, Namespace, Prefix, QualName};

use crate::dom::Dom;
use crate::error::{DomError, DomResult};
use crate::node::{element_flags, Attr, DocumentKind, NodeId};

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";
const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
const SVG_NS: &str = "http://www.w3.org/2000/svg";
/// The namespace of the `parsererror` element a failed parse leaves (what Gecko uses; WPT checks it).
pub const PARSERERROR_NS: &str = "http://www.mozilla.org/newlayout/xml/parsererror.xml";

/// The public identifiers whose documents get the HTML named character references (HTML §13.4).
const XHTML_PUBLIC_IDS: &[&str] = &[
    "-//W3C//DTD XHTML 1.0 Transitional//EN",
    "-//W3C//DTD XHTML 1.1//EN",
    "-//W3C//DTD XHTML 1.0 Strict//EN",
    "-//W3C//DTD XHTML 1.0 Frameset//EN",
    "-//W3C//DTD XHTML Basic 1.0//EN",
    "-//W3C//DTD XHTML 1.1 plus MathML 2.0//EN",
    "-//W3C//DTD XHTML 1.1 plus MathML 2.0 plus SVG 1.1//EN",
    "-//W3C//DTD MathML 2.0//EN",
    "-//WAPFORUM//DTD XHTML Mobile 1.0//EN",
];

/// What [`XmlParser::run`] stopped for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XmlStep {
    /// A `script` element's end tag was parsed: the page prepares the script, then calls `run` again.
    Script(NodeId),
    /// The input is parsed and well-formed.
    Done,
    /// A well-formedness error: the message says where.
    Error(String),
}

struct Open {
    node: NodeId,
    /// The qualified name as written, which the end tag must repeat.
    qname: String,
    /// How many namespace bindings were in scope before this element.
    bindings: usize,
}

/// An incremental XML parser over a whole input string.
pub struct XmlParser {
    input: String,
    pos: usize,
    document: NodeId,
    /// The node children go into while no element is open (the document, or a fragment's root).
    base: NodeId,
    stack: Vec<Open>,
    /// Namespace bindings in scope: (prefix, namespace); `None` prefix is the default namespace, an
    /// empty namespace undeclares it.
    bindings: Vec<(Option<String>, String)>,
    entities: HashMap<String, String>,
    html_entities: bool,
    seen_root: bool,
    seen_doctype: bool,
    fragment: bool,
    done: Option<XmlStep>,
    /// Text not yet flushed into a Text node, and the node it goes into.
    text: String,
    /// Nesting of entity expansions (against entity bombs).
    depth: u32,
    /// A script element that ended as an empty element tag (`<script/>`), reported next.
    pending_script: Option<NodeId>,
}

/// Normalizes line endings (XML §2.11): CR LF and lone CR become LF.
fn normalize_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_owned();
    }
    let mut o = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            o.push('\n');
        } else {
            o.push(c);
        }
    }
    o
}

/// XML `Char`.
fn is_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

/// XML `NameStartChar`.
pub fn is_name_start_char(c: char) -> bool {
    matches!(c,
        ':' | 'A'..='Z' | '_' | 'a'..='z' | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}'
        | '\u{F8}'..='\u{2FF}' | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}'
        | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}')
}

/// XML `NameChar`.
pub fn is_name_char(c: char) -> bool {
    is_name_start_char(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// A QName split into (prefix, local name), or `None` when it is not one.
fn split_qname(name: &str) -> Option<(Option<&str>, &str)> {
    match name.split_once(':') {
        None => Some((None, name)),
        Some((p, l)) => {
            if p.is_empty() || l.is_empty() || l.contains(':') || !l.starts_with(is_name_start_char)
            {
                None
            } else {
                Some((Some(p), l))
            }
        }
    }
}

type R<T> = Result<T, String>;

impl XmlParser {
    /// A parser for a whole document into the (empty) document `document`.
    pub fn new(input: &str, document: NodeId) -> XmlParser {
        let input = normalize_newlines(input.strip_prefix('\u{FEFF}').unwrap_or(input));
        XmlParser {
            input,
            pos: 0,
            document,
            base: document,
            stack: Vec::new(),
            bindings: Vec::new(),
            entities: HashMap::new(),
            html_entities: false,
            seen_root: false,
            seen_doctype: false,
            fragment: false,
            done: None,
            text: String::new(),
            depth: 0,
            pending_script: None,
        }
    }

    /// A parser for a fragment (the DOM Parsing "XML fragment parsing algorithm"): content goes into
    /// `root`, with `bindings` (the context element's in-scope namespaces) declared.
    fn new_fragment(
        input: &str,
        document: NodeId,
        root: NodeId,
        bindings: Vec<(Option<String>, String)>,
    ) -> XmlParser {
        let mut p = XmlParser::new(input, document);
        p.base = root;
        p.bindings = bindings;
        p.fragment = true;
        p.seen_root = true;
        p
    }

    pub fn document(&self) -> NodeId {
        self.document
    }

    /// Parses until a script end tag, the end, or an error.
    pub fn run(&mut self, dom: &mut Dom) -> XmlStep {
        if let Some(d) = &self.done {
            return d.clone();
        }
        let r = self.step(dom);
        match r {
            Ok(Some(el)) => XmlStep::Script(el),
            Ok(None) => {
                self.done = Some(XmlStep::Done);
                XmlStep::Done
            }
            Err(m) => {
                let (line, col) = self.line_col();
                let msg = format!("error on line {line} at column {col}: {m}");
                self.done = Some(XmlStep::Error(msg.clone()));
                XmlStep::Error(msg)
            }
        }
    }

    fn line_col(&self) -> (usize, usize) {
        let before = &self.input[..self.pos.min(self.input.len())];
        let line = before.matches('\n').count() + 1;
        let col = before
            .rfind('\n')
            .map_or(before.chars().count(), |i| before[i + 1..].chars().count())
            + 1;
        (line, col)
    }

    // ----- the cursor ------------------------------------------------------------------------------------

    fn rest(&self) -> &str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn starts(&self, s: &str) -> bool {
        self.rest().starts_with(s)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.starts(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, s: &str) -> R<()> {
        if self.eat(s) {
            Ok(())
        } else {
            Err(format!("expected '{s}'"))
        }
    }

    fn skip_space(&mut self) -> bool {
        let start = self.pos;
        while self.peek().is_some_and(is_space) {
            self.pos += 1;
        }
        self.pos > start
    }

    fn name(&mut self) -> R<String> {
        let start = self.pos;
        match self.peek() {
            Some(c) if is_name_start_char(c) => {
                self.bump();
            }
            _ => return Err("expected a name".into()),
        }
        while self.peek().is_some_and(is_name_char) {
            self.bump();
        }
        Ok(self.input[start..self.pos].to_owned())
    }

    fn quoted(&mut self) -> R<String> {
        let q = match self.peek() {
            Some(c @ ('"' | '\'')) => c,
            _ => return Err("expected a quoted value".into()),
        };
        self.bump();
        let start = self.pos;
        let end = self.rest().find(q).ok_or("unterminated quoted value")?;
        self.pos += end + 1;
        Ok(self.input[start..start + end].to_owned())
    }

    // ----- the document --------------------------------------------------------------------------------

    /// Parses on from the cursor; returns a script element whose end tag was just parsed.
    fn step(&mut self, dom: &mut Dom) -> R<Option<NodeId>> {
        if self.pos == 0 && !self.fragment && self.starts("<?xml") {
            let after = self.input[5..].chars().next();
            if after.is_some_and(is_space) {
                self.xml_declaration(dom)?;
            }
        }
        loop {
            if let Some(script) = self.pending_script.take() {
                return Ok(Some(script));
            }
            if self.pos >= self.input.len() {
                self.flush_text(dom);
                if !self.stack.is_empty() {
                    return Err(format!(
                        "Premature end of data in tag {}",
                        self.stack.last().map(|o| o.qname.as_str()).unwrap_or("")
                    ));
                }
                if !self.seen_root {
                    return Err("Document is empty".into());
                }
                return Ok(None);
            }
            if self.starts("<") {
                if self.starts("</") {
                    self.flush_text(dom);
                    if let Some(script) = self.end_tag(dom)? {
                        return Ok(Some(script));
                    }
                } else if self.starts("<!--") {
                    self.flush_text(dom);
                    self.comment(dom)?;
                } else if self.starts("<![CDATA[") {
                    if self.stack.is_empty() && !self.fragment {
                        return Err("CDATA section outside the root element".into());
                    }
                    self.flush_text(dom);
                    self.cdata(dom)?;
                } else if self.starts("<!DOCTYPE") {
                    self.flush_text(dom);
                    self.doctype(dom)?;
                } else if self.starts("<?") {
                    self.flush_text(dom);
                    self.pi(dom)?;
                } else {
                    self.flush_text(dom);
                    self.start_tag(dom)?;
                }
            } else if self.starts("&") {
                if self.stack.is_empty() && !self.fragment {
                    return Err("reference outside the root element".into());
                }
                self.reference_in_content(dom)?;
            } else {
                self.char_data()?;
            }
        }
    }

    fn xml_declaration(&mut self, _dom: &mut Dom) -> R<()> {
        self.expect("<?xml")?;
        self.skip_space();
        let mut version = None;
        loop {
            self.skip_space();
            if self.eat("?>") {
                break;
            }
            let name = self.name()?;
            self.skip_space();
            self.expect("=")?;
            self.skip_space();
            let value = self.quoted()?;
            match name.as_str() {
                "version" if version.is_none() => {
                    if !value.starts_with("1.")
                        || value.len() < 3
                        || !value[2..].bytes().all(|b| b.is_ascii_digit())
                    {
                        return Err("Unsupported version".into());
                    }
                    version = Some(value);
                }
                "encoding" if version.is_some() => {}
                "standalone" if version.is_some() => {
                    if value != "yes" && value != "no" {
                        return Err("standalone accepts only 'yes' or 'no'".into());
                    }
                }
                _ => return Err("Malformed XML declaration".into()),
            }
        }
        if version.is_none() {
            return Err("Malformed declaration expecting version".into());
        }
        Ok(())
    }

    fn char_data(&mut self) -> R<()> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '<' || c == '&' {
                break;
            }
            if !is_char(c) {
                return Err(format!("Char 0x{:X} out of allowed range", c as u32));
            }
            if c == ']' && self.starts("]]>") {
                return Err("Sequence ']]>' not allowed in content".into());
            }
            self.bump();
        }
        let s = &self.input[start..self.pos];
        if self.stack.is_empty() && !self.fragment {
            if !s.chars().all(is_space) {
                return Err(if self.seen_root {
                    "Extra content at the end of the document".into()
                } else {
                    "Start tag expected, '<' not found".into()
                });
            }
            return Ok(());
        }
        let s = s.to_owned();
        self.text.push_str(&s);
        Ok(())
    }

    /// Where the next node goes, and the node document it is created in (that of the node it will be appended
    /// to): the current node, or an HTML `template`'s template contents (HTML §13.4: "When an XML parser would
    /// append a node to a template element, it must instead append it to the template element's template
    /// contents").
    fn parent(&self, dom: &Dom) -> (NodeId, NodeId) {
        let node = self.stack.last().map(|o| o.node).unwrap_or(self.base);
        let parent = dom.template_contents(node).unwrap_or(node);
        (parent, dom.node_document(parent))
    }

    fn flush_text(&mut self, dom: &mut Dom) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        let (parent, document) = self.parent(dom);
        // Adjacent text (around a comment-free reference) joins the previous Text node.
        match dom.last_child(parent) {
            Some(last) if dom.is_exclusive_text(last) => dom.append_data(last, &text),
            _ => {
                let t = dom.create_text_node(document, &text);
                dom.insert(t, parent, None, false);
            }
        }
    }

    fn comment(&mut self, dom: &mut Dom) -> R<()> {
        self.expect("<!--")?;
        let end = self.rest().find("--").ok_or("Comment not terminated")?;
        let data = self.rest()[..end].to_owned();
        self.pos += end;
        if !self.eat("-->") {
            return Err("Double hyphen within comment".into());
        }
        if data.ends_with('-') {
            return Err("Comment must not end with '-'".into());
        }
        if let Some(c) = data.chars().find(|&c| !is_char(c)) {
            return Err(format!("Char 0x{:X} out of allowed range", c as u32));
        }
        let (parent, document) = self.parent(dom);
        let c = dom.create_comment(document, &data);
        dom.insert(c, parent, None, false);
        Ok(())
    }

    fn cdata(&mut self, dom: &mut Dom) -> R<()> {
        self.expect("<![CDATA[")?;
        let end = self
            .rest()
            .find("]]>")
            .ok_or("CData section not finished")?;
        let data = self.rest()[..end].to_owned();
        self.pos += end + 3;
        if let Some(c) = data.chars().find(|&c| !is_char(c)) {
            return Err(format!("Char 0x{:X} out of allowed range", c as u32));
        }
        let (parent, document) = self.parent(dom);
        let c = dom.create_cdata_unchecked(document, &data);
        dom.insert(c, parent, None, false);
        Ok(())
    }

    fn pi(&mut self, dom: &mut Dom) -> R<()> {
        self.expect("<?")?;
        let target = self.name()?;
        if target.eq_ignore_ascii_case("xml") {
            return Err("XML declaration allowed only at the start of the document".into());
        }
        if target.contains(':') {
            return Err("colons are forbidden from PI names".into());
        }
        let data = if self.eat("?>") {
            String::new()
        } else {
            if !self.skip_space() {
                return Err("ParsePI: PI target space expected".into());
            }
            let end = self.rest().find("?>").ok_or("PI not terminated")?;
            let d = self.rest()[..end].to_owned();
            self.pos += end + 2;
            d
        };
        if let Some(c) = data.chars().find(|&c| !is_char(c)) {
            return Err(format!("Char 0x{:X} out of allowed range", c as u32));
        }
        let (parent, document) = self.parent(dom);
        let pi = dom.create_pi_unchecked(document, &target, &data);
        dom.insert(pi, parent, None, false);
        Ok(())
    }

    fn doctype(&mut self, dom: &mut Dom) -> R<()> {
        if self.seen_root || self.seen_doctype || self.fragment {
            return Err("DOCTYPE improperly placed".into());
        }
        self.seen_doctype = true;
        self.expect("<!DOCTYPE")?;
        if !self.skip_space() {
            return Err("Space required after '<!DOCTYPE'".into());
        }
        let name = self.name()?;
        let ws = self.skip_space();
        let (mut public_id, mut system_id) = (String::new(), String::new());
        if ws && self.eat("PUBLIC") {
            if !self.skip_space() {
                return Err("Space required after 'PUBLIC'".into());
            }
            public_id = self.quoted()?;
            if !public_id.chars().all(|c| {
                matches!(c, ' ' | '\r' | '\n' | 'a'..='z' | 'A'..='Z' | '0'..='9')
                    || "-'()+,./:=?;!*#@$_%".contains(c)
            }) {
                return Err("Invalid public identifier".into());
            }
            if !self.skip_space() {
                return Err("Space required after the public identifier".into());
            }
            system_id = self.quoted()?;
            self.skip_space();
        } else if ws && self.eat("SYSTEM") {
            if !self.skip_space() {
                return Err("Space required after 'SYSTEM'".into());
            }
            system_id = self.quoted()?;
            self.skip_space();
        }
        if self.eat("[") {
            self.internal_subset()?;
            self.skip_space();
        }
        self.expect(">")?;
        self.html_entities = XHTML_PUBLIC_IDS.contains(&public_id.as_str());
        let dt = dom.create_doctype_unchecked(self.document, &name, &public_id, &system_id);
        dom.insert(dt, self.document, None, false);
        Ok(())
    }

    /// The internal subset: entity declarations are kept, other declarations skipped.
    fn internal_subset(&mut self) -> R<()> {
        loop {
            self.skip_space();
            if self.eat("]") {
                return Ok(());
            }
            if self.pos >= self.input.len() {
                return Err("internal subset not terminated".into());
            }
            if self.starts("<!--") {
                self.expect("<!--")?;
                let end = self.rest().find("-->").ok_or("Comment not terminated")?;
                self.pos += end + 3;
            } else if self.starts("<?") {
                let end = self.rest().find("?>").ok_or("PI not terminated")?;
                self.pos += end + 2;
            } else if self.starts("<!ENTITY") {
                self.expect("<!ENTITY")?;
                if !self.skip_space() {
                    return Err("Space required after '<!ENTITY'".into());
                }
                let parameter = self.eat("%");
                if parameter {
                    self.skip_space();
                }
                let name = self.name()?;
                if !self.skip_space() {
                    return Err("Space required after the entity name".into());
                }
                if self.starts("\"") || self.starts("'") {
                    let value = self.quoted()?;
                    if !parameter && !self.entities.contains_key(&name) {
                        let value = self.expand_char_refs(&value)?;
                        self.entities.insert(name, value);
                    }
                } else {
                    // An external entity: not fetched.
                    self.skip_declaration()?;
                    continue;
                }
                self.skip_space();
                self.expect(">")?;
            } else if self.starts("<!") {
                self.skip_declaration()?;
            } else if self.starts("%") {
                self.bump();
                self.name()?;
                self.expect(";")?;
            } else {
                return Err("Malformed internal subset".into());
            }
        }
    }

    /// Skips a markup declaration up to its `>`, past quoted strings.
    fn skip_declaration(&mut self) -> R<()> {
        while let Some(c) = self.bump() {
            match c {
                '"' | '\'' => {
                    let end = self.rest().find(c).ok_or("unterminated quoted value")?;
                    self.pos += end + 1;
                }
                '>' => return Ok(()),
                _ => {}
            }
        }
        Err("markup declaration not terminated".into())
    }

    /// Character references in an entity value are expanded when it is declared (XML §4.5).
    fn expand_char_refs(&self, value: &str) -> R<String> {
        let mut out = String::with_capacity(value.len());
        let mut rest = value;
        while let Some(i) = rest.find("&#") {
            out.push_str(&rest[..i]);
            let end = rest[i..]
                .find(';')
                .ok_or("CharRef: invalid decimal value")?;
            let c = char_ref(&rest[i + 2..i + end])?;
            out.push(c);
            rest = &rest[i + end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// A reference in content: a character reference, a predefined entity, a declared entity (whose
    /// replacement text is parsed as content), or an HTML named reference in XHTML documents.
    fn reference_in_content(&mut self, dom: &mut Dom) -> R<()> {
        self.expect("&")?;
        if self.eat("#") {
            let end = self.rest().find(';').ok_or("CharRef: invalid value")?;
            let c = char_ref(&self.rest()[..end])?;
            self.pos += end + 1;
            self.text.push(c);
            return Ok(());
        }
        let name = self.name()?;
        if !self.eat(";") {
            return Err(format!("EntityRef: expecting ';' after '{name}'"));
        }
        if let Some(c) = predefined(&name) {
            self.text.push(c);
            return Ok(());
        }
        if let Some(value) = self.entities.get(&name).cloned() {
            if self.depth > 16 {
                return Err("Detected an entity reference loop".into());
            }
            if !value.contains('<') && !value.contains('&') {
                self.text.push_str(&value);
                return Ok(());
            }
            // Parse the replacement text in place: splice it into the input.
            self.depth += 1;
            let tail = self.input.split_off(self.pos);
            self.input.push_str(&value);
            self.input.push_str(&tail);
            return Ok(());
        }
        if self.html_entities {
            let key = format!("{name};");
            if let Some(&(a, b)) = html5ever::data::NAMED_ENTITIES.get(key.as_str()) {
                if let Some(c) = char::from_u32(a) {
                    self.text.push(c);
                }
                if b != 0 {
                    if let Some(c) = char::from_u32(b) {
                        self.text.push(c);
                    }
                }
                return Ok(());
            }
        }
        let _ = dom;
        Err(format!("Entity '{name}' not defined"))
    }

    /// An attribute value (XML §3.3.3 normalization: whitespace characters become spaces; references
    /// expand).
    fn attribute_value(&mut self) -> R<String> {
        let q = match self.peek() {
            Some(c @ ('"' | '\'')) => c,
            _ => return Err("AttValue: \" or ' expected".into()),
        };
        self.bump();
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err("AttValue: ' expected".into());
            };
            match c {
                c if c == q => return Ok(out),
                '<' => return Err("Unescaped '<' not allowed in attributes values".into()),
                '&' => {
                    if self.eat("#") {
                        let end = self.rest().find(';').ok_or("CharRef: invalid value")?;
                        let c = char_ref(&self.rest()[..end])?;
                        self.pos += end + 1;
                        out.push(c);
                    } else {
                        let name = self.name()?;
                        if !self.eat(";") {
                            return Err(format!("EntityRef: expecting ';' after '{name}'"));
                        }
                        if let Some(c) = predefined(&name) {
                            out.push(c);
                        } else if let Some(v) = self.entities.get(&name) {
                            if v.contains('<') {
                                return Err(
                                    "'<' in entity is not allowed in attributes values".into()
                                );
                            }
                            out.extend(v.chars().map(|c| if is_space(c) { ' ' } else { c }));
                        } else if let Some(&(a, b)) = self
                            .html_entities
                            .then(|| {
                                html5ever::data::NAMED_ENTITIES.get(format!("{name};").as_str())
                            })
                            .flatten()
                        {
                            out.extend(char::from_u32(a));
                            if b != 0 {
                                out.extend(char::from_u32(b));
                            }
                        } else {
                            return Err(format!("Entity '{name}' not defined"));
                        }
                    }
                }
                c if is_space(c) => out.push(' '),
                c if !is_char(c) => {
                    return Err(format!("Char 0x{:X} out of allowed range", c as u32))
                }
                c => out.push(c),
            }
        }
    }

    // ----- elements -----------------------------------------------------------------------------------

    fn lookup(&self, prefix: Option<&str>) -> Option<&str> {
        match prefix {
            Some("xml") => return Some(XML_NS),
            Some("xmlns") => return Some(XMLNS_NS),
            _ => {}
        }
        self.bindings
            .iter()
            .rev()
            .find(|(p, _)| p.as_deref() == prefix)
            .map(|(_, n)| n.as_str())
            .filter(|n| !n.is_empty())
    }

    fn start_tag(&mut self, dom: &mut Dom) -> R<()> {
        if self.stack.is_empty() && !self.fragment {
            if self.seen_root {
                return Err("Extra content at the end of the document".into());
            }
            self.seen_root = true;
        }
        self.expect("<")?;
        let qname = self.name()?;
        let mut attrs: Vec<(String, String)> = Vec::new();
        let empty;
        loop {
            let ws = self.skip_space();
            if self.eat("/>") {
                empty = true;
                break;
            }
            if self.eat(">") {
                empty = false;
                break;
            }
            if !ws {
                return Err("attributes construct error".into());
            }
            let name = self.name()?;
            self.skip_space();
            if !self.eat("=") {
                return Err(format!("Specification mandates value for attribute {name}"));
            }
            self.skip_space();
            let value = self.attribute_value()?;
            if attrs.iter().any(|(n, _)| *n == name) {
                return Err(format!("Attribute {name} redefined"));
            }
            attrs.push((name, value));
        }
        // Namespace declarations first.
        let scope_start = self.bindings.len();
        for (name, value) in &attrs {
            if name == "xmlns" {
                if value == XML_NS || value == XMLNS_NS {
                    return Err(format!("xmlns: URI {value} is reserved"));
                }
                self.bindings.push((None, value.clone()));
            } else if let Some(p) = name.strip_prefix("xmlns:") {
                if split_qname(name).is_none() {
                    return Err(format!("Failed to parse QName '{name}'"));
                }
                if p == "xmlns" {
                    return Err("xmlns: prefix 'xmlns' is reserved".into());
                }
                if p == "xml" && value != XML_NS {
                    return Err("xml namespace prefix mapped to wrong URI".into());
                }
                if p != "xml" && (value == XML_NS || value == XMLNS_NS) {
                    return Err(format!("xmlns:{p}: URI {value} is reserved"));
                }
                if value.is_empty() {
                    return Err(format!("xmlns:{p}: Empty XML namespace is not allowed"));
                }
                self.bindings.push((Some(p.to_owned()), value.clone()));
            }
        }
        let (prefix, local) =
            split_qname(&qname).ok_or_else(|| format!("Failed to parse QName '{qname}'"))?;
        let ns = match prefix {
            Some(p) => self
                .lookup(Some(p))
                .ok_or_else(|| format!("Namespace prefix {p} on {local} is not defined"))?
                .to_owned(),
            None => self.lookup(None).unwrap_or("").to_owned(),
        };
        if prefix == Some("xmlns") {
            return Err("xmlns: elements may not have the prefix 'xmlns'".into());
        }
        let name = QualName::new(
            prefix.map(Prefix::from),
            Namespace::from(ns.as_str()),
            LocalName::from(local),
        );
        let is = (ns == XHTML_NS)
            .then(|| {
                attrs
                    .iter()
                    .find(|(n, _)| n == "is")
                    .map(|(_, v)| LocalName::from(v.as_str()))
            })
            .flatten();
        let (parent, document) = self.parent(dom);
        let el = dom.create_element_with_name(document, name, is);
        let mut expanded: Vec<(String, String)> = Vec::new();
        for (aname, value) in attrs {
            let (ap, al) =
                split_qname(&aname).ok_or_else(|| format!("Failed to parse QName '{aname}'"))?;
            let ans = if aname == "xmlns" || ap == Some("xmlns") {
                XMLNS_NS.to_owned()
            } else {
                match ap {
                    Some(p) => self
                        .lookup(Some(p))
                        .ok_or_else(|| {
                            format!("Namespace prefix {p} for {al} on {local} is not defined")
                        })?
                        .to_owned(),
                    None => String::new(),
                }
            };
            if !ans.is_empty() && expanded.iter().any(|(n, l)| *n == ans && l == al) {
                return Err(format!("Namespaced Attribute {al} in '{ans}' redefined"));
            }
            expanded.push((ans.clone(), al.to_owned()));
            let qn = QualName::new(
                ap.map(Prefix::from),
                Namespace::from(ans.as_str()),
                LocalName::from(al),
            );
            dom.push_parsed_attribute(el, Attr::new(qn, &value));
        }
        let is_script = (ns == XHTML_NS || ns == SVG_NS) && local == "script";
        if is_script {
            dom.set_element_flag(el, element_flags::PARSER_INSERTED, true);
        }
        dom.insert(el, parent, None, false);
        if empty {
            self.bindings.truncate(scope_start);
            if is_script {
                // An empty script element still ends here.
                self.pending_script = Some(el);
            }
        } else {
            self.stack.push(Open {
                node: el,
                qname,
                bindings: scope_start,
            });
        }
        Ok(())
    }

    fn end_tag(&mut self, dom: &mut Dom) -> R<Option<NodeId>> {
        self.expect("</")?;
        let name = self.name()?;
        self.skip_space();
        self.expect(">")?;
        let Some(open) = self.stack.pop() else {
            return Err(format!("Unexpected end tag : {name}"));
        };
        if open.qname != name {
            return Err(format!(
                "Opening and ending tag mismatch: {} and {name}",
                open.qname
            ));
        }
        self.bindings.truncate(open.bindings);
        let is_script = dom.element(open.node).is_some_and(|e| {
            e.name().local == local_name!("script")
                && (e.name().ns == ns!(html) || e.name().ns == ns!(svg))
        });
        Ok(is_script.then_some(open.node))
    }
}

fn predefined(name: &str) -> Option<char> {
    Some(match name {
        "lt" => '<',
        "gt" => '>',
        "amp" => '&',
        "apos" => '\'',
        "quot" => '"',
        _ => return None,
    })
}

/// The character of a character reference's digits (`#` already consumed; `x` for hex).
fn char_ref(digits: &str) -> R<char> {
    let n = if let Some(h) = digits.strip_prefix('x') {
        if h.is_empty() || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("CharRef: invalid hexadecimal value".into());
        }
        u32::from_str_radix(h, 16).map_err(|_| "CharRef: invalid hexadecimal value")?
    } else {
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err("CharRef: invalid decimal value".into());
        }
        digits
            .parse::<u32>()
            .map_err(|_| "CharRef: invalid decimal value")?
    };
    char::from_u32(n)
        .filter(|&c| is_char(c))
        .ok_or_else(|| format!("xmlParseCharRef: invalid xmlChar value {n}"))
}

impl Dom {
    /// Parses a whole XML document into `doc` (no scripts run). On a well-formedness error the document
    /// holds a `parsererror` element instead, and the message is returned.
    pub fn parse_xml_into(&mut self, doc: NodeId, text: &str) -> Result<(), String> {
        let mut p = XmlParser::new(text, doc);
        loop {
            match p.run(self) {
                XmlStep::Script(el) => {
                    // (HTML "parsing XML documents": no script of this parse runs, nor a clone)
                    self.set_element_flag(el, element_flags::SCRIPT_ALREADY_STARTED, true);
                }
                XmlStep::Done => return Ok(()),
                XmlStep::Error(m) => {
                    self.xml_parse_error(doc, &m);
                    return Err(m);
                }
            }
        }
    }

    /// Replaces a document's children with the `parsererror` element of a failed XML parse.
    pub fn xml_parse_error(&mut self, doc: NodeId, message: &str) {
        while let Some(c) = self.first_child(doc) {
            self.remove(c, true);
        }
        let name = QualName::new(
            None,
            Namespace::from(PARSERERROR_NS),
            LocalName::from("parsererror"),
        );
        let root = self.create_element_with_name(doc, name, None);
        self.insert(root, doc, None, true);
        let text = self.create_text_node(doc, &format!("XML Parsing Error: {message}"));
        self.insert(text, root, None, true);
    }

    /// The XML fragment parsing algorithm (DOM Parsing): parses `markup` as the content of `context`
    /// (an element of an XML document) with its in-scope namespaces, into a new DocumentFragment.
    pub fn parse_xml_fragment(&mut self, context: NodeId, markup: &str) -> DomResult<NodeId> {
        let doc = self.node_document(context);
        // The namespace declarations in scope on the context element.
        let mut bindings: Vec<(Option<String>, String)> = Vec::new();
        let chain: Vec<NodeId> = self
            .inclusive_ancestors(context)
            .filter(|&a| self.is_element(a))
            .collect();
        for &el in chain.iter().rev() {
            let e = self.element(el).expect("an element");
            let name = e.name().clone();
            for a in e.attrs().iter() {
                if a.name.ns == ns!(xmlns) {
                    let prefix =
                        (a.name.local != local_name!("xmlns")).then(|| a.name.local.to_string());
                    bindings.push((prefix, a.value.to_string()));
                }
            }
            if !name.ns.is_empty() {
                bindings.push((
                    name.prefix.as_ref().map(|p| p.to_string()),
                    name.ns.to_string(),
                ));
            }
        }
        let root_name = QualName::new(None, ns!(), LocalName::from("fragment-root"));
        let root = self.create_element_with_name(doc, root_name, None);
        let mut p = XmlParser::new_fragment(markup, doc, root, bindings);
        let r = loop {
            match p.run(self) {
                XmlStep::Script(el) => {
                    // (HTML "parsing XML documents": no script of this parse runs, nor a clone)
                    self.set_element_flag(el, element_flags::SCRIPT_ALREADY_STARTED, true);
                }
                XmlStep::Done => break Ok(()),
                XmlStep::Error(_) => break Err(DomError::Syntax),
            }
        };
        if r.is_err() {
            self.free_subtree(root);
            return Err(DomError::Syntax);
        }
        let fragment = self.create_document_fragment(doc);
        while let Some(c) = self.first_child(root) {
            self.insert(c, fragment, None, true);
        }
        self.free_subtree(root);
        Ok(fragment)
    }

    /// Parses an XML document from a string into a new XML document (DOMParser): `Err` with the message
    /// when it is not well-formed (the document then holds a `parsererror` element).
    pub fn parse_xml_str(&mut self, text: &str) -> (NodeId, Result<(), String>) {
        let doc = self.create_document(DocumentKind::Xml);
        let r = self.parse_xml_into(doc, text);
        (doc, r)
    }
}
