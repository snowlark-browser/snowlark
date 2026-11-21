// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the developer tools see of a page, outside any script-visible surface:
//!
//! - the console sink: once enabled ([`Engine::devtools_console`]), every console message, with
//!   its level, its group depth and where it came from (the caller's script position for the
//!   console API, the error's for uncaught exceptions and unhandled rejections), is kept for the
//!   embedder ([`Engine::take_console_entries`]) instead of the plain list tests read;
//! - evaluation in the top-level realm with the result formatted as a console shows it
//!   ([`Engine::devtools_eval`]);
//! - the DOM inspector's view of the document: the visible rows of the element tree for a set of
//!   expanded nodes ([`Engine::devtools_tree`]), an element's computed style and box model
//!   ([`Engine::devtools_details`]), the boxes to highlight ([`Engine::devtools_highlight`]) and
//!   the element at a viewport point ([`Engine::devtools_node_at`]).

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use snowlark_bindings::rt::Cx;
use snowlark_dom::{NodeData, NodeId};
use snowlark_paint::{Point, Rect};

use crate::engine::Engine;

/// The most console entries kept for the embedder (older ones go).
pub const MAX_CONSOLE_ENTRIES: usize = 5000;

/// The most console results kept for copying (older ones go).
pub const MAX_RESULTS: usize = 1000;

/// Chrome's `toStringForClipboard` (`front_end/core/sdk/PageFunctions.ts`), called on the value.
const TO_STRING_FOR_CLIPBOARD: &str = "(function (indent) {
    if (this instanceof Node) {
        return this instanceof Element ? this.outerHTML : undefined;
    }
    try {
        return JSON.stringify(this, null, indent);
    } catch (e) {
        return String(this);
    }
})";

/// What kind of value a console result is: the console's context menu offers its rows by it, as
/// Chrome's `SourcesPanel.#appendRemoteObjectItems` does by the remote object's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValueKind {
    String,
    /// `typeof` "object", not null.
    Object,
    Function,
    /// A number, boolean, bigint, symbol, undefined or null.
    Primitive,
}

impl ValueKind {
    pub fn of(v: v8::Local<v8::Value>) -> ValueKind {
        if v.is_string() {
            ValueKind::String
        } else if v.is_function() {
            ValueKind::Function
        } else if v.is_object() {
            ValueKind::Object
        } else {
            ValueKind::Primitive
        }
    }
}

/// A script position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLocation {
    pub url: String,
    pub line: u32,
    pub column: u32,
}

impl std::fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.url, self.line, self.column)
    }
}

/// One console message.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConsoleEntry {
    /// "log", "info", "warn", "error", "debug", or "clear" (`console.clear()`).
    pub level: &'static str,
    pub text: String,
    pub source: Option<SourceLocation>,
    /// The group depth (`console.group`).
    pub group: usize,
    /// The URL of the realm the message came from (the document of the window, or the worker's
    /// script), when known: tells a frame's or a worker's messages from the page's.
    pub realm: Option<String>,
    /// The Formatter's `%c` styles (Console Standard 2.2.1): from byte `.0` of `text` on, the
    /// style `.1`, up to the next one. Empty when the message has none.
    pub styles: Vec<(u32, ConsoleStyle)>,
}

/// What the console shows of a `%c` argument's CSS: the declarations a line of console text can
/// take (a colour, a background colour, bold and italic text). A declaration of any other
/// property, or a value that is not a plain colour, is ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleStyle {
    /// `color`, as straight RGBA.
    pub color: Option<[u8; 4]>,
    /// `background-color`, or a `background` that names a colour.
    pub background: Option<[u8; 4]>,
    /// `font-weight` bold, bolder or 600 and over.
    pub bold: bool,
    /// `font-style` italic or oblique.
    pub italic: bool,
}

impl ConsoleStyle {
    /// The style of the declarations in `css` (a `style` attribute's syntax). Later declarations
    /// win; `!important` makes no difference here.
    pub fn parse(css: &str) -> ConsoleStyle {
        let mut style = ConsoleStyle::default();
        for decl in split_top_level(css, ';') {
            let Some((name, value)) = decl.split_once(':') else {
                continue;
            };
            let name = name.trim().to_ascii_lowercase();
            let mut value = value.trim();
            if let Some(v) = value
                .len()
                .checked_sub(10)
                .filter(|&i| value.is_char_boundary(i))
                .filter(|&i| value[i..].eq_ignore_ascii_case("!important"))
            {
                value = value[..v].trim_end();
            }
            match name.as_str() {
                "color" => {
                    if let Some(c) = css_color(value) {
                        style.color = Some(c);
                    }
                }
                "background-color" => {
                    if let Some(c) = css_color(value) {
                        style.background = Some(c);
                    }
                }
                "background" => {
                    // (the shorthand's colour is one of its components)
                    if let Some(c) = css_color(value)
                        .or_else(|| split_top_level(value, ' ').find_map(|t| css_color(t.trim())))
                    {
                        style.background = Some(c);
                    }
                }
                "font-weight" => {
                    let v = value.to_ascii_lowercase();
                    match v.as_str() {
                        "bold" | "bolder" => style.bold = true,
                        "normal" | "lighter" => style.bold = false,
                        _ => {
                            if let Ok(n) = v.parse::<f32>() {
                                style.bold = n >= 600.0;
                            }
                        }
                    }
                }
                "font-style" => {
                    let v = value.to_ascii_lowercase();
                    style.italic = v == "italic" || v.starts_with("oblique");
                }
                _ => {}
            }
        }
        style
    }
}

/// `s` cut at each `sep` outside parentheses and quotes.
fn split_top_level(s: &str, sep: char) -> impl Iterator<Item = &str> {
    let mut depth = 0u32;
    let mut quote = None;
    let mut start = 0;
    let mut parts = Vec::new();
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, c) if c == sep && depth == 0 => {
                parts.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts.into_iter().filter(|p| !p.trim().is_empty())
}

/// A CSS colour value as straight RGBA.
fn css_color(value: &str) -> Option<[u8; 4]> {
    let c = snowlark_render::canvas::parse_color(value)?.to_color_u8();
    Some([c.red(), c.green(), c.blue(), c.alpha()])
}

// (a level from a message is one the console has, or the message is bad)
impl<'de> Deserialize<'de> for ConsoleEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Entry {
            level: String,
            text: String,
            source: Option<SourceLocation>,
            group: usize,
            #[serde(default)]
            realm: Option<String>,
            #[serde(default)]
            styles: Vec<(u32, ConsoleStyle)>,
        }
        let e = Entry::deserialize(deserializer)?;
        let level = ["log", "info", "warn", "error", "debug", "clear"]
            .into_iter()
            .find(|l| *l == e.level)
            .ok_or_else(|| serde::de::Error::custom("not a console level"))?;
        Ok(ConsoleEntry {
            level,
            text: e.text,
            source: e.source,
            group: e.group,
            realm: e.realm,
            styles: e.styles,
        })
    }
}

impl ConsoleEntry {
    /// The entry as one line, `[console.<level>] <text> (<url>:<line>:<column>) [realm <url>]`: the
    /// text with line breaks escaped as `\n`, the source and the realm when known. For tools that
    /// read a headless run's console (`SNOWLARK_CONSOLE=tagged`).
    pub fn tagged_line(&self) -> String {
        let text = self.text.replace('\r', "").replace('\n', "\\n");
        let mut line = match &self.source {
            Some(s) => format!("[console.{}] {} ({})", self.level, text, s),
            None => format!("[console.{}] {}", self.level, text),
        };
        if let Some(realm) = &self.realm {
            line.push_str(" [realm ");
            line.push_str(realm);
            line.push(']');
        }
        line
    }
}

/// The console messages waiting for the embedder, and the caller of the console call in progress.
#[derive(Default)]
pub struct ConsoleSink {
    pub entries: VecDeque<ConsoleEntry>,
    pub caller: Option<SourceLocation>,
    /// The URL of the realm of the message in progress (see [`ConsoleEntry::realm`]).
    pub realm: Option<String>,
    /// The `%c` styles of the message in progress (see [`ConsoleEntry::styles`]).
    pub styles: Vec<(u32, ConsoleStyle)>,
}

impl ConsoleSink {
    pub fn push(&mut self, entry: ConsoleEntry) {
        if self.entries.len() >= MAX_CONSOLE_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }
}

/// A row of the element tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DomRowKind {
    /// An element's start tag; `inline_text` is a single short text child shown on the same row
    /// (with the end tag).
    Open {
        tag: String,
        attrs: Vec<(String, String)>,
        inline_text: Option<String>,
        /// No children (shown with its end tag on the row).
        empty: bool,
    },
    /// An expanded element's end tag.
    Close {
        tag: String,
    },
    Text(String),
    Comment(String),
    Doctype(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DomRow {
    pub node: NodeId,
    pub depth: u16,
    pub kind: DomRowKind,
    /// It has children to show.
    pub expandable: bool,
    pub expanded: bool,
}

/// Edges in CSS px: top, right, bottom, left.
pub type Edges = [f32; 4];

/// An element's box model (CSS px).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BoxModel {
    pub margin: Edges,
    pub border: Edges,
    pub padding: Edges,
    /// The content box's size.
    pub content: (f32, f32),
}

/// What the inspector shows about the selected element.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeDetails {
    /// "div#main.wide"
    pub title: String,
    /// Every longhand's resolved value, by name.
    pub computed: Vec<(String, String)>,
    pub box_model: Option<BoxModel>,
}

/// The boxes of an element to draw over the page (viewport CSS px).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Highlight {
    pub margin: Rect,
    pub border: Rect,
    pub padding: Rect,
    pub content: Rect,
    /// "div.card 320 × 48"
    pub label: String,
}

/// The longest text a tree row shows.
const ROW_TEXT: usize = 200;

pub(crate) fn clip(s: &str, max: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn px(v: Option<String>) -> f32 {
    v.as_deref()
        .and_then(|s| s.trim().strip_suffix("px"))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0.0)
}

#[cfg(test)]
fn shrink(r: Rect, e: &Edges) -> Rect {
    Rect::new(
        r.x + e[3],
        r.y + e[0],
        (r.width - e[1] - e[3]).max(0.0),
        (r.height - e[0] - e[2]).max(0.0),
    )
}

fn grow(r: Rect, e: &Edges) -> Rect {
    Rect::new(
        r.x - e[3],
        r.y - e[0],
        r.width + e[1] + e[3],
        r.height + e[0] + e[2],
    )
}

impl Engine {
    // ----- the console -------------------------------------------------------------------------------

    /// Keeps console messages for [`Engine::take_console_entries`] (on), or in the plain list (off).
    pub fn devtools_console(&self, on: bool) {
        let mut hooks = self.hooks.borrow_mut();
        match (on, hooks.console.devtools.is_some()) {
            (true, false) => hooks.console.devtools = Some(ConsoleSink::default()),
            (false, true) => hooks.console.devtools = None,
            _ => {}
        }
    }

    /// The console messages since the last call.
    pub fn take_console_entries(&self) -> Vec<ConsoleEntry> {
        self.hooks
            .borrow_mut()
            .console
            .devtools
            .as_mut()
            .map(|s| s.entries.drain(..).collect())
            .unwrap_or_default()
    }

    /// Notes where the console call in progress came from (the innermost script frame), when the
    /// sink is on.
    pub(crate) fn devtools_note_caller(&self, cx: &mut Cx) {
        if self.hooks.borrow().console.devtools.is_none() {
            return;
        }
        let scope = cx.scope();
        let location = v8::StackTrace::current_stack_trace(scope, 1).and_then(|t| {
            let f = t.get_frame(scope, 0)?;
            let url = f
                .get_script_name_or_source_url(scope)
                .map(|n| n.to_rust_string_lossy(scope))
                .unwrap_or_default();
            Some(SourceLocation {
                url,
                line: f.get_line_number() as u32,
                column: f.get_column() as u32,
            })
        });
        let realm = snowlark_bindings::rt::realm_global(scope.get_current_context())
            .and_then(|w| self.associated_document(w))
            .map(|doc| self.dom().document_url(doc).to_owned());
        if let Some(s) = self.hooks.borrow_mut().console.devtools.as_mut() {
            s.caller = location;
            s.realm = realm;
        }
    }

    /// Notes that the message in progress comes from `doc`'s realm (a message the engine reports for
    /// a document, such as a policy violation or an uncaught error), when the sink is on.
    pub(crate) fn devtools_note_realm(&self, doc: NodeId) {
        if self.hooks.borrow().console.devtools.is_none() {
            return;
        }
        let url = self.dom().document_url(doc).to_owned();
        if let Some(s) = self.hooks.borrow_mut().console.devtools.as_mut() {
            s.realm = Some(url);
        }
    }

    /// "Uncaught (in promise) …" for an unhandled rejection, with the reason's position and call
    /// frames when it is an error object.
    pub(crate) fn console_rejection(
        &self,
        cx: &mut Cx,
        reason: Option<&v8::Global<v8::Value>>,
        text: &str,
    ) {
        let mut stack = String::new();
        let source = reason.and_then(|g| {
            let scope = cx.scope();
            let v = v8::Local::new(scope, g);
            if !v.is_native_error() {
                return None;
            }
            stack = crate::engine::window::captured_stack(scope, v);
            let m = v8::Exception::create_message(scope, v);
            let url = m
                .get_script_resource_name(scope)
                .map(|n| n.to_rust_string_lossy(scope))
                .unwrap_or_default();
            (!url.is_empty() && url != "undefined").then(|| SourceLocation {
                url,
                line: m.get_line_number(scope).unwrap_or(0) as u32,
                column: u32::try_from(m.get_start_column()).map_or(0, |c| c + 1),
            })
        });
        let text = format!("Uncaught (in promise) {text}{stack}");
        match source {
            Some(s) => self.console_print_at("error", &text, Some(s)),
            None => self.console_error(&text),
        }
    }

    /// Evaluates `source` as a classic script in the top-level realm, as a console's input line
    /// does: the completion value formatted as the console formats values and its kind, or the
    /// exception. The value is kept under `id` for [`Engine::devtools_copy_result`] and
    /// [`Engine::devtools_store_result`].
    pub fn devtools_eval(
        &self,
        cx: &mut Cx,
        id: u64,
        source: &str,
    ) -> Result<(String, ValueKind), String> {
        self.enter_script();
        let result = {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let r = (|| {
                let code = v8::String::new(tc, source)?;
                // (no name, as the console's scripts have none; not muted: an error code typed
                // here throws later is reported to the page as any other)
                let undefined = v8::undefined(tc);
                let origin = v8::ScriptOrigin::new(
                    tc,
                    undefined.into(),
                    0,
                    0,
                    true,
                    0,
                    None,
                    false,
                    false,
                    false,
                    None,
                );
                let script = v8::Script::compile(tc, code, Some(&origin))?;
                script.run(tc)
            })();
            match r {
                Some(v) => {
                    let mut kept = self.devtools_results.borrow_mut();
                    if kept.len() >= MAX_RESULTS {
                        kept.pop_front();
                    }
                    kept.push_back((id, v8::Global::new(tc, v)));
                    Ok((
                        crate::engine::window::inspect(tc, v, 0, true),
                        ValueKind::of(v),
                    ))
                }
                None => {
                    let text = match tc.exception() {
                        Some(e) => {
                            let s = crate::engine::window::inspect(tc, e, 0, true);
                            if s.starts_with("Uncaught") {
                                s
                            } else {
                                format!("Uncaught {s}")
                            }
                        }
                        None => "Uncaught (terminated)".into(),
                    };
                    tc.reset();
                    Err(text)
                }
            }
        };
        self.leave_script(cx);
        result
    }

    /// Forgets the kept console results (the console was cleared, or the document went).
    pub fn devtools_clear_results(&self) {
        self.devtools_results.borrow_mut().clear();
    }

    fn devtools_result(&self, id: u64) -> Option<v8::Global<v8::Value>> {
        let kept = self.devtools_results.borrow();
        kept.iter().find(|(i, _)| *i == id).map(|(_, v)| v.clone())
    }

    /// The text "Copy string contents" or "Copy object" puts on the clipboard for the result kept
    /// under `id`, as Chrome makes it: a string as it is (`SourcesPanel.#appendRemoteObjectItems`),
    /// an object by `toStringForClipboard` (`front_end/core/sdk/PageFunctions.ts`), run in the
    /// page with the editor's default indent of four spaces: an element's `outerHTML`, another
    /// node nothing, anything else `JSON.stringify(value, null, indent)`, or `String(value)` when
    /// that throws. `None` when there is nothing to copy.
    pub fn devtools_copy_result(&self, cx: &mut Cx, id: u64) -> Option<String> {
        let kept = self.devtools_result(id)?;
        self.enter_script();
        let out = {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let v = v8::Local::new(tc, &kept);
            let out = if v.is_string() {
                Some(v.to_rust_string_lossy(tc))
            } else if v.is_object() && !v.is_function() {
                (|| {
                    let code = v8::String::new(tc, TO_STRING_FOR_CLIPBOARD)?;
                    let f = v8::Script::compile(tc, code, None)?.run(tc)?;
                    let f: v8::Local<v8::Function> = f.try_into().ok()?;
                    let indent = v8::String::new(tc, "    ")?;
                    let r = f.call(tc, v, &[indent.into()])?;
                    r.is_string().then(|| r.to_rust_string_lossy(tc))
                })()
            } else {
                None
            };
            if tc.has_caught() {
                tc.reset();
            }
            out
        };
        self.leave_script(cx);
        out
    }

    /// "Store as global variable" on the result kept under `id`: a global `tempN` (the first
    /// unused) holds it in the top-level realm; its name and the console's formatting of the value.
    pub fn devtools_store_result(&self, cx: &mut Cx, id: u64) -> Option<(String, String)> {
        let kept = self.devtools_result(id)?;
        self.enter_script();
        let out = {
            let scope = cx.scope();
            let v = v8::Local::new(scope, &kept);
            let text = crate::engine::window::inspect(scope, v, 0, true);
            crate::devtools_node::define_temp(scope, v).map(|name| (name, text))
        };
        self.leave_script(cx);
        out
    }

    // ----- the element tree ----------------------------------------------------------------------------

    /// The rows of the top-level document's tree with `expanded` elements open (whitespace-only
    /// text is left out, as inspectors do).
    pub fn devtools_tree(&self, expanded: &std::collections::HashSet<NodeId>) -> Vec<DomRow> {
        let Some(doc) = self.document() else {
            return Vec::new();
        };
        let dom = self.dom();
        let mut out = Vec::new();
        let shown = |n: NodeId| match dom.data(n) {
            NodeData::Text(t) | NodeData::CDataSection(t) => !t.trim().is_empty(),
            NodeData::Element(_) | NodeData::Comment(_) | NodeData::DocumentType(_) => true,
            _ => false,
        };
        // (node, depth, closing)
        let mut stack: Vec<(NodeId, u16, bool)> = dom
            .children(doc)
            .filter(|&c| shown(c))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|c| (c, 0, false))
            .collect();
        while let Some((n, depth, closing)) = stack.pop() {
            match dom.data(n) {
                NodeData::Element(_) => {
                    let tag = dom.qualified_name(n).into_owned();
                    if closing {
                        out.push(DomRow {
                            node: n,
                            depth,
                            kind: DomRowKind::Close { tag },
                            expandable: false,
                            expanded: false,
                        });
                        continue;
                    }
                    let kids: Vec<NodeId> = dom.children(n).filter(|&c| shown(c)).collect();
                    let inline_text = match kids.as_slice() {
                        [only] => match dom.data(*only) {
                            NodeData::Text(t) if t.trim().chars().count() <= 80 => {
                                Some(clip(t, ROW_TEXT))
                            }
                            _ => None,
                        },
                        _ => None,
                    };
                    let expandable = !kids.is_empty() && inline_text.is_none();
                    let is_open = expandable && expanded.contains(&n);
                    let attrs = dom
                        .attrs(n)
                        .iter()
                        .map(|a| (a.qualified_name().into_owned(), clip(a.value(), ROW_TEXT)))
                        .collect();
                    out.push(DomRow {
                        node: n,
                        depth,
                        kind: DomRowKind::Open {
                            tag,
                            attrs,
                            inline_text,
                            empty: kids.is_empty(),
                        },
                        expandable,
                        expanded: is_open,
                    });
                    if is_open {
                        stack.push((n, depth, true));
                        for &k in kids.iter().rev() {
                            stack.push((k, depth + 1, false));
                        }
                    }
                }
                NodeData::Text(t) | NodeData::CDataSection(t) => out.push(DomRow {
                    node: n,
                    depth,
                    kind: DomRowKind::Text(clip(t, ROW_TEXT)),
                    expandable: false,
                    expanded: false,
                }),
                NodeData::Comment(c) => out.push(DomRow {
                    node: n,
                    depth,
                    kind: DomRowKind::Comment(clip(c, ROW_TEXT)),
                    expandable: false,
                    expanded: false,
                }),
                NodeData::DocumentType(_) => {
                    let name = dom.doctype(n).map(|(name, _, _)| name.to_owned());
                    out.push(DomRow {
                        node: n,
                        depth,
                        kind: DomRowKind::Doctype(name.unwrap_or_default()),
                        expandable: false,
                        expanded: false,
                    });
                }
                _ => {}
            }
        }
        out
    }

    /// The elements from the document element down to `node` (to expand to reveal it).
    pub fn devtools_path(&self, node: NodeId) -> Vec<NodeId> {
        let dom = self.dom();
        if !dom.is_alive(node) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cur = dom.parent(node);
        while let Some(c) = cur {
            if dom.is_element(c) {
                out.push(c);
            }
            cur = dom.parent(c);
        }
        out.reverse();
        out
    }

    /// The top-level document's version (bumped by every DOM change).
    pub fn devtools_dom_version(&self) -> u64 {
        self.dom().version()
    }

    /// "div#main.a.b"
    fn devtools_title(&self, node: NodeId) -> String {
        let dom = self.dom();
        let mut s = dom.local_name(node).to_string();
        if let Some(id) = dom.get_attribute(node, "id").filter(|v| !v.is_empty()) {
            s.push('#');
            s.push_str(id);
        }
        if let Some(class) = dom.get_attribute(node, "class") {
            for c in class.split_whitespace() {
                s.push('.');
                s.push_str(c);
            }
        }
        s
    }

    /// The selected element's computed style and box model (`None` when it is gone or not an
    /// element).
    pub fn devtools_details(&self, node: NodeId) -> Option<NodeDetails> {
        if !self.dom().is_alive(node) || !self.dom().is_element(node) {
            return None;
        }
        let title = self.devtools_title(node);
        let names = snowlark_style::computed::computed_property_names();
        let laid = self.with_layout(node, |r, _| {
            let computed: Vec<(String, String)> = names
                .iter()
                .map(|n| {
                    (
                        (*n).to_owned(),
                        r.resolved_property(node, n).unwrap_or_default(),
                    )
                })
                .collect();
            let g = |n: &str| px(r.resolved_property(node, n));
            let box_model = r.geometry(node).map(|geo| BoxModel {
                margin: [
                    g("margin-top"),
                    g("margin-right"),
                    g("margin-bottom"),
                    g("margin-left"),
                ],
                border: [
                    g("border-top-width"),
                    g("border-right-width"),
                    g("border-bottom-width"),
                    g("border-left-width"),
                ],
                padding: [
                    g("padding-top"),
                    g("padding-right"),
                    g("padding-bottom"),
                    g("padding-left"),
                ],
                content: (geo.content_box.width, geo.content_box.height),
            });
            (computed, box_model)
        });
        let (computed, box_model) = laid.unwrap_or_default();
        Some(NodeDetails {
            title,
            computed,
            box_model,
        })
    }

    /// The boxes of `node` relative to the viewport (`None` without a box).
    pub fn devtools_highlight(&self, node: NodeId) -> Option<Highlight> {
        if !self.dom().is_alive(node) || !self.dom().is_element(node) {
            return None;
        }
        let title = self.devtools_title(node);
        self.with_layout(node, |r, st| {
            let geo = r.geometry(node)?;
            let g = |n: &str| px(r.resolved_property(node, n));
            let margin = [
                g("margin-top"),
                g("margin-right"),
                g("margin-bottom"),
                g("margin-left"),
            ];
            let scroll = st.scroll;
            let at = |r: Rect| r.translate(-scroll.x, -scroll.y);
            let border = at(geo.border_box);
            let padding = at(geo.padding_box);
            let content = at(geo.content_box);
            Some(Highlight {
                margin: grow(border, &margin),
                border,
                padding,
                content,
                label: format!(
                    "{title}  {} × {}",
                    (border.width * 100.0).round() / 100.0,
                    (border.height * 100.0).round() / 100.0
                ),
            })
        })
        .flatten()
    }

    /// Clears the session storage areas of the origins on `site` (a host; its registrable domain and
    /// subdomains), or of every origin (`None`): "clear site data". `localStorage` is the browser's
    /// to clear, once for every tab ([`crate::web_storage::WebStorageHost::wipe`]). Returns how many
    /// areas had items.
    pub fn clear_site_storage(&self, site: Option<&str>) -> usize {
        let matches = crate::web_storage::site_matcher(site);
        let st = self.storage.borrow();
        let mut cleared = 0;
        for (origin, area) in st.session.iter() {
            if matches(origin) {
                let mut a = area.borrow_mut();
                if !a.is_empty() {
                    cleared += 1;
                }
                a.clear();
            }
        }
        cleared
    }

    /// The element at a viewport point of the top-level document.
    pub fn devtools_node_at(&self, p: Point) -> Option<NodeId> {
        let doc = self.document()?;
        self.view_elements_from_point(doc, p.x as f64, p.y as f64)
            .first()
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(clip("  a \n  b  ", 10), "a b");
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(px(Some("12.5px".into())), 12.5);
        assert_eq!(px(Some("auto".into())), 0.0);
        let r = Rect::new(10.0, 10.0, 100.0, 50.0);
        let e = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(grow(shrink(r, &e), &e), r);
    }

    #[test]
    fn tagged_console_lines_carry_level_text_and_source() {
        let entry = ConsoleEntry {
            level: "error",
            text: "Uncaught TypeError: x is not a function\n    at f".into(),
            source: Some(SourceLocation {
                url: "https://a.example/app.js".into(),
                line: 3,
                column: 14,
            }),
            group: 0,
            realm: None,
            styles: Vec::new(),
        };
        assert_eq!(
            entry.tagged_line(),
            "[console.error] Uncaught TypeError: x is not a function\\n    at f (https://a.example/app.js:3:14)"
        );
        let entry = ConsoleEntry {
            level: "log",
            text: "hi".into(),
            source: None,
            group: 1,
            realm: Some("about:srcdoc".into()),
            styles: Vec::new(),
        };
        assert_eq!(entry.tagged_line(), "[console.log] hi [realm about:srcdoc]");
    }
}
