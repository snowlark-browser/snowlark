// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML Editing APIs: the `insertParagraph`, `insertLineBreak` and `insertText` commands.

use snowlark_bindings::generated::RangeImpl;
use snowlark_bindings::rt::Cx;
use snowlark_dom::NodeId;

use super::delete::DeleteFlags;
use super::nodes::Item;
use crate::engine::ranges::BoundaryPair;
use crate::engine::{node_handle, Engine};

impl Engine {
    /// Range `insertNode(node)` on the active range.
    fn ec_insert_node(&self, cx: &mut Cx, doc: NodeId, node: NodeId) {
        if let Some(h) = self.ec_range_handle(doc) {
            let _ = RangeImpl::insert_node(self, cx, h, node_handle(node));
            self.after_mutation(cx);
        }
    }

    /// The `insertParagraph` command's action.
    pub(crate) fn ec_insert_paragraph(&self, cx: &mut Cx, doc: NodeId) -> bool {
        self.ec_delete_selection(cx, doc, DeleteFlags::default());
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        if !self.ec_editable_or_host(r.start.0) {
            return true;
        }
        // An editing host that is inline or cannot hold a paragraph gets a line break instead: a
        // line feed where its white space keeps line feeds, else a `br`.
        if let Some(host) = self.ec_host_of(r.start.0) {
            let tag = self.ec_default_single_line_container(doc);
            if self.ec_inline(host) || !self.ec_allowed_child(Item::Name(tag), Item::Node(host)) {
                let el = if self.dom().is_element(r.start.0) {
                    Some(r.start.0)
                } else {
                    self.dom().parent_element(r.start.0)
                };
                let keeps_lf = el.is_some_and(|e| {
                    matches!(
                        self.ec_white_space(e).as_str(),
                        "pre" | "pre-wrap" | "pre-line" | "break-spaces"
                    )
                });
                if keeps_lf {
                    self.ec_insert_linefeed(cx, doc);
                    return true;
                }
                return self.ec_insert_line_break(cx, doc);
            }
        }
        let (mut node, mut offset) = r.start;
        if self.dom().is_text(node) && offset != 0 && offset != self.ec_len(node) {
            let _ = self.dom_mut().split_text(node, offset as usize);
            self.after_mutation(cx);
        }
        if self.dom().is_text(node) && offset == self.ec_len(node) {
            if let Some(p) = self.ec_parent(node) {
                offset = 1 + self.ec_index(node);
                node = p;
            }
        }
        let char_data = {
            let dom = self.dom();
            dom.is_text(node) || dom.node_type(node) == snowlark_dom::NodeType::Comment
        };
        if char_data {
            if let Some(p) = self.ec_parent(node) {
                offset = self.ec_index(node);
                node = p;
            }
        }
        self.ec_collapse(doc, (node, offset));
        let mut container = node;
        while !self.ec_single_line_container(container) {
            let p = self.ec_parent(container);
            if self.ec_editable_o(p) && self.ec_same_host(node, p) {
                container = p.unwrap();
            } else {
                break;
            }
        }
        if self.ec_editable(container)
            && self.ec_single_line_container(container)
            && self.ec_same_host(node, self.ec_parent(container))
            && self.ec_is(container, &["p", "div"])
        {
            let mut outer = container;
            while !self.ec_is(outer, &["dd", "dt", "li"]) {
                let p = self.ec_parent(outer);
                if self.ec_editable_o(p) {
                    outer = p.unwrap();
                } else {
                    break;
                }
            }
            if self.ec_is(outer, &["dd", "dt", "li"]) {
                container = outer;
            }
        }
        if !self.ec_editable(container)
            || !self.ec_same_host(container, Some(node))
            || !self.ec_single_line_container(container)
        {
            let tag = self.ec_default_single_line_container(doc);
            let Some(active) = self.ec_active(doc) else {
                return true;
            };
            let new_range = self.ec_block_extend(active);
            let mut list: Vec<NodeId> = self
                .ec_contained_nodes(new_range, true, |e, n| {
                    e.ec_allowed_child(Item::Node(n), Item::Name("p"))
                })
                .into_iter()
                .take(1)
                .collect();
            if list.is_empty() {
                let start = active.start.0;
                if !self.ec_allowed_child(Item::Name(tag), Item::Node(start)) {
                    return true;
                }
                let c = self.ec_create(doc, tag);
                self.ec_insert_node(cx, doc, c);
                let br = self.ec_create(doc, "br");
                self.ec_append(cx, c, br);
                self.ec_collapse(doc, (c, 0));
                return true;
            }
            loop {
                let next = self.dom().next_sibling(*list.last().unwrap());
                match next {
                    Some(n) if self.ec_allowed_child(Item::Node(n), Item::Name("p")) => {
                        list.push(n)
                    }
                    _ => break,
                }
            }
            match self.ec_wrap(cx, list, &|_, _| false, &|e, _| Some(e.ec_create(doc, tag))) {
                Some(c) => container = c,
                None => return true,
            }
        }
        if self.ec_is(container, &["address", "listing", "pre"]) {
            let br = self.ec_create(doc, "br");
            self.ec_insert_node(cx, doc, br);
            self.ec_collapse(doc, (node, offset + 1));
            let next = self.dom().next_in_tree_order(br, None);
            if !next.is_some_and(|n| self.dom().is_ancestor_of(container, n)) {
                let br2 = self.ec_create(doc, "br");
                self.ec_insert_node(cx, doc, br2);
                self.ec_collapse(doc, (node, offset + 1));
            }
            return true;
        }
        if self.ec_is(container, &["li", "dt", "dd"]) && {
            let kids = self.ec_children(container);
            kids.is_empty() || (kids.len() == 1 && self.ec_is(kids[0], &["br"]))
        } {
            self.ec_split_parent(cx, &[container]);
            if !self.dom().has_child_nodes(container) {
                let br = self.ec_create(doc, "br");
                self.ec_append(cx, container, br);
            }
            let mut container = container;
            if self.ec_is(container, &["dd", "dt"]) && self.ec_disallowed_everywhere(container) {
                let name = self.ec_default_single_line_container(doc);
                container = self.ec_set_tag_name(cx, container, name);
            }
            self.ec_fix_disallowed_ancestors(cx, doc, container);
            return true;
        }
        let Some(active) = self.ec_active(doc) else {
            return true;
        };
        let mut start = active.start;
        let end = (container, self.ec_len(container));
        while start.1 == 0 && !self.ec_prohibited_paragraph_child(Some(start.0)) {
            let Some(p) = self.ec_parent(start.0) else {
                break;
            };
            start = (p, self.ec_index(start.0));
        }
        while start.1 == self.ec_len(start.0) && !self.ec_prohibited_paragraph_child(Some(start.0))
        {
            let Some(p) = self.ec_parent(start.0) else {
                break;
            };
            start = (p, 1 + self.ec_index(start.0));
        }
        let new_line_range = BoundaryPair { start, end };
        let contained = self.ec_contained_nodes(new_line_range, false, |_, _| true);
        let end_of_line =
            contained.is_empty() || (contained.len() == 1 && self.ec_is(contained[0], &["br"]));
        let name = self.ec_name(container).unwrap_or_default();
        let new_name: String =
            if ["h1", "h2", "h3", "h4", "h5", "h6"].contains(&name.as_str()) && end_of_line {
                self.ec_default_single_line_container(doc).to_owned()
            } else if name == "dt" && end_of_line {
                "dd".to_owned()
            } else if name == "dd" && end_of_line {
                "dt".to_owned()
            } else {
                name
            };
        let mut new_container = self.ec_create(doc, &new_name);
        self.ec_copy_attributes(cx, container, new_container);
        self.ec_remove_attribute(cx, new_container, "id");
        self.ec_insert_after(cx, container, new_container);
        let contained_all = self.ec_contained_nodes(new_line_range, true, |_, _| true);
        let frag = match self.extract_contents_of(cx, None, new_line_range) {
            Ok(f) => f,
            Err(_) => return true,
        };
        self.after_mutation(cx);
        let descendants: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(frag)
                .filter(|&d| dom.is_element(d))
                .collect()
        };
        for d in descendants {
            if !contained_all.contains(&d) {
                self.ec_remove_attribute(cx, d, "id");
            }
        }
        self.ec_append(cx, new_container, frag);
        while self.ec_prohibited_paragraph_child(self.dom().last_child(container)) {
            container = self.dom().last_child(container).unwrap();
        }
        while self.ec_prohibited_paragraph_child(self.dom().last_child(new_container)) {
            new_container = self.dom().last_child(new_container).unwrap();
        }
        for c in [container, new_container] {
            if !self.ec_children(c).into_iter().any(|k| self.ec_visible(k)) {
                let br = self.ec_create(doc, "br");
                self.ec_append(cx, c, br);
            }
        }
        self.ec_collapse(doc, (new_container, 0));
        true
    }

    /// Inserts a line feed at the active range's start (in a Text node there or a new one) and
    /// collapses the selection after it.
    fn ec_insert_linefeed(&self, cx: &mut Cx, doc: NodeId) {
        let Some(r) = self.ec_active(doc) else {
            return;
        };
        let (node, offset) = r.start;
        if self.dom().is_text(node) {
            self.ec_insert_data(cx, node, offset, "\n");
            self.ec_collapse(doc, (node, offset + 1));
        } else {
            let text = self.dom_mut().create_text_node(doc, "\n");
            self.ec_insert_node(cx, doc, text);
            self.ec_collapse(doc, (text, 1));
        }
    }

    /// The `insertLineBreak` command's action.
    pub(crate) fn ec_insert_line_break(&self, cx: &mut Cx, doc: NodeId) -> bool {
        self.ec_delete_selection(
            cx,
            doc,
            DeleteFlags {
                strip_wrappers: false,
                ..Default::default()
            },
        );
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        let start = r.start.0;
        if !self.ec_editable_or_host(start) {
            return true;
        }
        let is_el = self.dom().is_element(start);
        if is_el && !self.ec_allowed_child(Item::Name("br"), Item::Node(start)) {
            return true;
        }
        if !is_el
            && !self
                .ec_parent(start)
                .is_some_and(|p| self.ec_allowed_child(Item::Name("br"), Item::Node(p)))
        {
            return true;
        }
        if self.dom().is_text(start) {
            if let Some(p) = self.ec_parent(start) {
                if r.start.1 == 0 {
                    self.ec_collapse(doc, (p, self.ec_index(start)));
                } else if r.start.1 == self.ec_len(start) {
                    self.ec_collapse(doc, (p, 1 + self.ec_index(start)));
                }
            }
        }
        let br = self.ec_create(doc, "br");
        self.ec_insert_node(cx, doc, br);
        let Some(p) = self.ec_parent(br) else {
            return true;
        };
        self.ec_collapse(doc, (p, 1 + self.ec_index(br)));
        if self.ec_collapsed_line_break(br) || self.ec_ends_inline_host(br) {
            let extra = self.ec_create(doc, "br");
            self.ec_insert_node(cx, doc, extra);
            if let Some(p) = self.ec_parent(br) {
                self.ec_collapse(doc, (p, 1 + self.ec_index(br)));
            }
        }
        true
    }

    /// Whether `br` is the last visible node of an inline editing host: the line after it would be
    /// the host's end, which content outside the host may share, so the caret gets a line of its
    /// own only from a second `br` (as browsers do; the spec's collapsed line break looks past the
    /// host).
    fn ec_ends_inline_host(&self, br: NodeId) -> bool {
        let Some(host) = self.ec_host_of(br) else {
            return false;
        };
        if !self.ec_inline(host) {
            return false;
        }
        let following: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(host)
                .skip_while(|&n| n != br)
                .skip(1)
                .filter(|&n| !dom.is_inclusive_ancestor_of(n, br))
                .collect()
        };
        !following
            .into_iter()
            .any(|n| self.ec_visible(n) && (self.dom().is_text(n) || self.ec_is(n, &["br", "img"])))
    }

    /// The `insertHTML` command's action with `value` parsed already: `frag` is a DocumentFragment
    /// of the active range's document (`createContextualFragment()`'s result, or a paste's
    /// sanitized one).
    pub(crate) fn ec_insert_fragment(&self, cx: &mut Cx, doc: NodeId, frag: NodeId) -> bool {
        self.ec_delete_selection(cx, doc, DeleteFlags::default());
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        if !self.ec_editable_or_host(r.start.0) {
            return true;
        }
        let Some(last_child) = self.dom().last_child(frag) else {
            return true;
        };
        let descendants: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(frag).filter(|&n| n != frag).collect()
        };
        // A block start node loses its editable collapsed block props at or after the offset.
        if self.ec_block(r.start.0) {
            let props: Vec<NodeId> = self
                .ec_children(r.start.0)
                .into_iter()
                .skip(r.start.1 as usize)
                .filter(|&c| self.ec_editable(c) && self.ec_collapsed_block_prop(c))
                .collect();
            for p in props {
                self.ec_remove(cx, p);
            }
        }
        self.ec_insert_node(cx, doc, frag);
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        if self.ec_block(r.start.0) && !descendants.iter().any(|&d| self.ec_visible(d)) {
            let br = self.ec_create(doc, "br");
            self.ec_insert_node(cx, doc, br);
        }
        if let Some(p) = self.ec_parent(last_child) {
            self.ec_collapse(doc, (p, 1 + self.ec_index(last_child)));
        }
        for d in descendants {
            if self.dom().is_alive(d) && self.dom().is_element(d) {
                self.ec_fix_disallowed_ancestors(cx, doc, d);
            }
        }
        true
    }

    /// The `insertHTML` command's action: `value` parsed in the active range's context.
    pub(super) fn ec_insert_html(&self, cx: &mut Cx, doc: NodeId, value: &str) -> bool {
        self.ec_delete_selection(cx, doc, DeleteFlags::default());
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        if !self.ec_editable_or_host(r.start.0) {
            return true;
        }
        // The markup parsed in the range's context, its scripts inert and then left out with event
        // handler attributes and script URLs (as browsers insert it: scripts do not run).
        let context = {
            let dom = self.dom();
            if dom.is_element(r.start.0) {
                Some(r.start.0)
            } else {
                dom.parent_element(r.start.0)
            }
        };
        let Some(context) = context else {
            return true;
        };
        let frag = self.dom_mut().parse_fragment(
            context,
            value,
            false,
            snowlark_dom::ParseOptions::default(),
        );
        self.strip_script_content(frag);
        // A plaintext-only host takes the markup's text, its line breaks as `br`s.
        let plaintext = self.ec_host_of(r.start.0).is_some_and(|h| {
            self.dom()
                .get_attribute(h, "contenteditable")
                .is_some_and(|v| v.eq_ignore_ascii_case("plaintext-only"))
        });
        if plaintext {
            let text = self.fragment_text(frag);
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    self.ec_insert_line_break(cx, doc);
                }
                if !line.is_empty() {
                    self.ec_insert_text(cx, doc, line);
                }
            }
            return true;
        }
        self.ec_insert_fragment(cx, doc, frag)
    }

    /// The `insertHorizontalRule` command's action.
    pub(super) fn ec_insert_horizontal_rule(&self, cx: &mut Cx, doc: NodeId) -> bool {
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        let (mut start_node, mut start_offset) = r.start;
        while start_offset == 0 {
            let Some(p) = self.ec_parent(start_node) else {
                break;
            };
            start_offset = self.ec_index(start_node);
            start_node = p;
        }
        let (mut end_node, mut end_offset) = r.end;
        while end_offset == self.ec_len(end_node) {
            let Some(p) = self.ec_parent(end_node) else {
                break;
            };
            end_offset = 1 + self.ec_index(end_node);
            end_node = p;
        }
        self.ec_select(doc, (start_node, start_offset), (end_node, end_offset));
        self.ec_delete_selection(
            cx,
            doc,
            DeleteFlags {
                block_merging: false,
                ..DeleteFlags::default()
            },
        );
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        if !self.ec_editable_or_host(r.start.0) {
            return true;
        }
        let (node, offset) = r.start;
        if self.dom().is_text(node) {
            if let Some(p) = self.ec_parent(node) {
                if offset == 0 {
                    self.ec_collapse(doc, (p, self.ec_index(node)));
                } else if offset == self.ec_len(node) {
                    self.ec_collapse(doc, (p, 1 + self.ec_index(node)));
                }
            }
        }
        let hr = self.ec_create(doc, "hr");
        self.ec_insert_node(cx, doc, hr);
        self.ec_fix_disallowed_ancestors(cx, doc, hr);
        if let Some(p) = self.ec_parent(hr) {
            self.ec_collapse(doc, (p, 1 + self.ec_index(hr)));
        }
        true
    }

    /// The `insertImage` command's action.
    pub(super) fn ec_insert_image(&self, cx: &mut Cx, doc: NodeId, value: &str) -> bool {
        if value.is_empty() {
            return false;
        }
        self.ec_delete_selection(
            cx,
            doc,
            DeleteFlags {
                strip_wrappers: false,
                ..DeleteFlags::default()
            },
        );
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        if !self.ec_editable_or_host(r.start.0) {
            return true;
        }
        let (node, offset) = r.start;
        if offset == 0 && self.ec_block(node) && self.dom().child_count(node) == 1 {
            let only = self.dom().first_child(node);
            if let Some(br) = only.filter(|&c| self.ec_is(c, &["br"])) {
                self.ec_remove(cx, br);
            }
        }
        let img = self.ec_create(doc, "img");
        self.set_attr(cx, img, "src", value);
        self.ec_insert_node(cx, doc, img);
        if let Some(p) = self.ec_parent(img) {
            self.ec_collapse(doc, (p, 1 + self.ec_index(img)));
        }
        true
    }

    /// The text a fragment shows: white space collapsed outside `pre`, `listing` and `textarea`,
    /// a line break for each `br` and between blocks.
    pub(crate) fn fragment_text(&self, frag: NodeId) -> String {
        const BLOCKS: &[&str] = &[
            "address",
            "article",
            "aside",
            "blockquote",
            "dd",
            "div",
            "dl",
            "dt",
            "fieldset",
            "figure",
            "footer",
            "form",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "header",
            "hr",
            "li",
            "listing",
            "main",
            "nav",
            "ol",
            "p",
            "pre",
            "section",
            "table",
            "tr",
            "ul",
        ];
        let dom = self.dom();
        let mut out = String::new();
        // (a block boundary asks for a line break before the next text)
        let mut pending_break = false;
        let mut last_space = true;
        for n in dom.descendants(frag) {
            if dom.is_text(n) {
                let pre = dom.inclusive_ancestors(n).any(|a| {
                    ["pre", "listing", "textarea"]
                        .iter()
                        .any(|t| dom.is_element(a) && &**dom.local_name(a) == *t)
                });
                let data = dom.character_data(n).unwrap_or("");
                let text: String = if pre {
                    data.to_owned()
                } else {
                    let mut s = String::new();
                    for c in data.chars() {
                        if matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c') {
                            if !last_space {
                                s.push(' ');
                            }
                            last_space = true;
                        } else {
                            s.push(c);
                            last_space = false;
                        }
                    }
                    s
                };
                if text.trim().is_empty() && !pre {
                    continue;
                }
                if pending_break && !out.is_empty() {
                    out.push('\n');
                }
                pending_break = false;
                out.push_str(if pre { &text } else { text.trim_start() });
                last_space = out.ends_with(' ');
            } else if dom.is_element(n) {
                let name = dom.local_name(n);
                if &**name == "br" {
                    out.push('\n');
                    pending_break = false;
                    last_space = true;
                } else if BLOCKS.contains(&&**name) {
                    pending_break = true;
                    last_space = true;
                }
            }
        }
        out.trim_end_matches(' ').to_owned()
    }

    /// The `insertText` command's action.
    pub(crate) fn ec_insert_text(&self, cx: &mut Cx, doc: NodeId, value: &str) -> bool {
        self.ec_delete_selection(
            cx,
            doc,
            DeleteFlags {
                strip_wrappers: false,
                ..Default::default()
            },
        );
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        if !self.ec_editable_or_host(r.start.0) {
            return true;
        }
        if value.chars().nth(1).is_some() {
            // (each element: a code unit of the spec's string; a surrogate pair stays whole)
            for c in value.chars() {
                let mut buf = [0u8; 4];
                self.ec_insert_text(cx, doc, c.encode_utf8(&mut buf));
            }
            return true;
        }
        if value.is_empty() {
            return true;
        }
        if value == "\n" {
            self.ec_insert_paragraph(cx, doc);
            return true;
        }
        // The formatting a deletion removed at this caret wraps the text.
        let style = self.doc_state(doc, |s| s.typing_style.take());
        if let Some((at, chain)) = style.filter(|(at, _)| r.start == r.end && *at == r.start) {
            let mut outer: Option<NodeId> = None;
            let mut inner: Option<NodeId> = None;
            for (name, attrs) in chain {
                let el = self.ec_create(doc, &name);
                for (n, v) in attrs {
                    let _ = self.dom_mut().set_attribute(el, &n, &v);
                }
                match inner {
                    Some(p) => {
                        let _ = self.dom_mut().append_child(p, el);
                    }
                    None => outer = Some(el),
                }
                inner = Some(el);
            }
            if let (Some(outer), Some(inner)) = (outer, inner) {
                let text = self.dom_mut().create_text_node(doc, value);
                let _ = self.dom_mut().append_child(inner, text);
                self.ec_collapse(doc, at);
                self.ec_insert_node(cx, doc, outer);
                let len = value.encode_utf16().count() as u32;
                self.ec_collapse(doc, (text, len));
                return true;
            }
        }
        let (mut node, mut offset) = r.start;
        if let Some(c) = self
            .ec_child_before(node, offset)
            .filter(|&c| self.dom().is_text(c))
        {
            node = c;
            offset = self.ec_len(c);
        }
        if let Some(c) = self
            .ec_child(node, offset)
            .filter(|&c| self.dom().is_text(c))
        {
            node = c;
            offset = 0;
        }
        let overrides = self.ec_record_current_overrides(doc);
        self.ec_collapse(doc, (node, offset));
        self.ec_canonicalize_whitespace(cx, node, offset, true);
        let Some(r) = self.ec_active(doc) else {
            return true;
        };
        let (node, offset) = r.start;
        let len = value.encode_utf16().count() as u32;
        if self.dom().is_text(node) {
            self.ec_insert_data(cx, node, offset, value);
            self.ec_select(doc, (node, offset), (node, offset + len));
        } else {
            let kids = self.ec_children(node);
            if kids.len() == 1 && self.ec_collapsed_line_break(kids[0]) {
                self.ec_remove(cx, kids[0]);
            }
            let text = self.dom_mut().create_text_node(doc, value);
            self.ec_insert_node(cx, doc, text);
            self.ec_select(doc, (text, 0), (text, len));
        }
        self.ec_restore_states_and_values(cx, doc, &overrides);
        if let Some(r) = self.ec_active(doc) {
            self.ec_canonicalize_whitespace(cx, r.start.0, r.start.1, false);
        }
        if let Some(r) = self.ec_active(doc) {
            self.ec_canonicalize_whitespace(cx, r.end.0, r.end.1, false);
        }
        // (autolinking a URL before a typed space is not done yet)
        self.ec_collapse_to(doc, false);
        true
    }
}
