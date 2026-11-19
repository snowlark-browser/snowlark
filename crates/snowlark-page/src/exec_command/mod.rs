// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The HTML Editing APIs (https://w3c.github.io/editing/docs/execCommand/): `execCommand` and the
//! `queryCommand*` methods, the commands' shared definitions and algorithms, and the commands
//! `delete`, `forwardDelete`, `insertParagraph`, `insertLineBreak`, `insertText`, `insertHTML`,
//! `insertHorizontalRule`, `insertImage`, `defaultParagraphSeparator`, `styleWithCSS`, `useCSS`
//! and `selectAll`.
//!
//! Every command the spec defines is supported (`queryCommandSupported`) and enabled by its rule;
//! the inline formatting commands are in `inline.rs`, justify in `justify.rs`, lists, `indent` and
//! `outdent` in `lists.rs`, `formatBlock` in `format_block.rs`; `copy`, `cut` and `paste` do
//! nothing yet (`execCommand` returns false); `undo` and `redo` are `editing.rs`'s.

mod delete;
mod format_block;
mod inline;
mod insert;
mod justify;
mod lists;
mod nodes;
mod ranges;
mod structure;
mod whitespace;

use snowlark_bindings::generated::SelectionImpl;
use snowlark_bindings::rt::Cx;
use snowlark_dom::NodeId;

use crate::engine::{node_handle, Engine};

pub(crate) use inline::Overrides;

/// The commands that "preserve overrides": their overrides are recorded before the action and
/// restored after it when the active range is collapsed.
const PRESERVES_OVERRIDES: &[&str] = &[
    "delete",
    "formatblock",
    "forwarddelete",
    "indent",
    "inserthorizontalrule",
    "inserthtml",
    "insertimage",
    "insertlinebreak",
    "insertorderedlist",
    "insertparagraph",
    "insertunorderedlist",
    "justifycenter",
    "justifyfull",
    "justifyleft",
    "justifyright",
    "outdent",
];

/// Every command the spec defines, lowercased.
const COMMANDS: &[&str] = &[
    "backcolor",
    "bold",
    "createlink",
    "fontname",
    "fontsize",
    "forecolor",
    "hilitecolor",
    "italic",
    "removeformat",
    "strikethrough",
    "subscript",
    "superscript",
    "underline",
    "unlink",
    "delete",
    "formatblock",
    "forwarddelete",
    "indent",
    "inserthorizontalrule",
    "inserthtml",
    "insertimage",
    "insertlinebreak",
    "insertorderedlist",
    "insertparagraph",
    "inserttext",
    "insertunorderedlist",
    "justifycenter",
    "justifyfull",
    "justifyleft",
    "justifyright",
    "outdent",
    "copy",
    "cut",
    "defaultparagraphseparator",
    "paste",
    "redo",
    "selectall",
    "stylewithcss",
    "undo",
    "usecss",
];

/// The "Miscellaneous commands" that are always enabled.
const ALWAYS_ENABLED: &[&str] = &[
    "copy",
    "defaultparagraphseparator",
    "selectall",
    "stylewithcss",
    "usecss",
];

/// The commands that style or restructure content (disabled in a plaintext-only editing host).
const STYLING: &[&str] = &[
    "backcolor",
    "bold",
    "createlink",
    "fontname",
    "fontsize",
    "forecolor",
    "hilitecolor",
    "italic",
    "removeformat",
    "strikethrough",
    "subscript",
    "superscript",
    "underline",
    "unlink",
    "formatblock",
    "indent",
    "insertorderedlist",
    "insertunorderedlist",
    "justifycenter",
    "justifyfull",
    "justifyleft",
    "justifyright",
    "outdent",
];

/// What `execCommand` did: whether it ran, and the `input` event it calls for (the editing host and
/// the `inputType`).
pub(crate) struct ExecOutcome {
    pub ran: bool,
    pub input: Option<(NodeId, &'static str)>,
    /// The `input` event's `data`, when the command gives it one.
    pub data: Option<String>,
}

/// The CSS serialization of a color value (Input Events `data` of the color commands).
fn serialize_color(value: &str) -> Option<String> {
    let v = value.trim();
    let hex = (v.len() == 3 || v.len() == 6) && v.bytes().all(|c| c.is_ascii_hexdigit());
    let v = if hex { format!("#{v}") } else { v.to_owned() };
    let c = snowlark_render::canvas::parse_color(&v)?.to_color_u8();
    Some(if c.alpha() == 255 {
        format!("rgb({}, {}, {})", c.red(), c.green(), c.blue())
    } else {
        let a = format!("{:.3}", f64::from(c.alpha()) / 255.0);
        let a = a.trim_end_matches('0').trim_end_matches('.');
        format!("rgba({}, {}, {}, {a})", c.red(), c.green(), c.blue())
    })
}

impl Engine {
    /// "The default single-line container name" of a document: `div` or `p`.
    pub(super) fn ec_default_single_line_container(&self, doc: NodeId) -> &'static str {
        if self.doc_state(doc, |s| s.paragraph_separator_p) {
            "p"
        } else {
            "div"
        }
    }

    /// Whether `cmd` (lowercased) is supported.
    pub(crate) fn ec_supported(cmd: &str) -> bool {
        COMMANDS.contains(&cmd)
    }

    /// Whether `cmd` (lowercased) is supported and enabled: the miscellaneous ones always, the
    /// others when the active range's start and end are editable or editing hosts and an editing
    /// host is an inclusive ancestor of both.
    pub(crate) fn ec_enabled(&self, doc: NodeId, cmd: &str) -> bool {
        if !Self::ec_supported(cmd) {
            return false;
        }
        // (a focused text control edits its own value, which these commands don't reach yet)
        let text_control = self
            .focused_element(doc)
            .is_some_and(|f| self.is_text_control(f));
        if ALWAYS_ENABLED.contains(&cmd) {
            return !text_control;
        }
        // (a text control's value is plain text: nothing styles it)
        if text_control && STYLING.contains(&cmd) {
            return false;
        }
        let Some(r) = self.ec_active(doc) else {
            return false;
        };
        if !self.ec_editable_or_host(r.start.0) || !self.ec_editable_or_host(r.end.0) {
            return false;
        }
        // A plaintext-only editing host takes no styling (the outermost one: in design mode, or in
        // a host that is `true`, a plaintext-only element inside is edited as the rest).
        if STYLING.contains(&cmd)
            && self.editing_host(r.start.0).is_some_and(|h| {
                self.dom()
                    .get_attribute(h, "contenteditable")
                    .is_some_and(|v| v.eq_ignore_ascii_case("plaintext-only"))
            })
        {
            return false;
        }
        let common = {
            let dom = self.dom();
            dom.inclusive_ancestors(r.start.0)
                .find(|&a| dom.is_inclusive_ancestor_of(a, r.end.0))
        };
        let Some(common) = common else {
            return false;
        };
        let chain: Vec<NodeId> = self.dom().inclusive_ancestors(common).collect();
        chain.into_iter().any(|a| self.ec_editing_host(a))
    }

    /// `queryCommandIndeterm` of the commands this module has.
    pub(crate) fn ec_query_indeterm(&self, doc: NodeId, cmd: &str) -> bool {
        if let Some(a) = justify::alignment_of(cmd) {
            return self.ec_justify_indeterm(doc, a);
        }
        match cmd {
            "formatblock" => return self.ec_format_block_indeterm(doc),
            "insertorderedlist" => {
                return matches!(
                    self.ec_list_state(doc),
                    lists::ListState::Mixed | lists::ListState::MixedOl
                )
            }
            "insertunorderedlist" => {
                return matches!(
                    self.ec_list_state(doc),
                    lists::ListState::Mixed | lists::ListState::MixedUl
                )
            }
            _ => {}
        }
        Self::ec_is_inline_command(cmd) && self.ec_inline_indeterm(doc, cmd)
    }

    /// `queryCommandState` of the commands this module has.
    pub(crate) fn ec_query_state(&self, doc: NodeId, cmd: &str) -> bool {
        match cmd {
            "stylewithcss" => self.doc_state(doc, |s| s.css_styling),
            "insertorderedlist" => self.ec_list_state(doc) == lists::ListState::Ol,
            "insertunorderedlist" => self.ec_list_state(doc) == lists::ListState::Ul,
            _ if Self::ec_is_inline_command(cmd) => self.ec_state(doc, cmd),
            _ => justify::alignment_of(cmd).is_some_and(|a| self.ec_justify_state(doc, a)),
        }
    }

    /// `queryCommandValue` (the command lowercased).
    pub(crate) fn ec_query_value(&self, doc: NodeId, cmd: &str) -> String {
        match cmd {
            "defaultparagraphseparator" => self.ec_default_single_line_container(doc).to_owned(),
            _ if Self::ec_is_inline_command(cmd) => self.ec_value(doc, cmd),
            _ if justify::alignment_of(cmd).is_some() => self.ec_justify_value(doc),
            "formatblock" => self.ec_format_block_value(doc),
            _ => String::new(),
        }
    }

    /// `execCommand(cmd, false, value)` for the commands this module has (`cmd` lowercased): the
    /// enabled check, the action, and the `input` event the action calls for.
    pub(crate) fn ec_exec(&self, cx: &mut Cx, doc: NodeId, cmd: &str, value: &str) -> ExecOutcome {
        let no = ExecOutcome {
            ran: false,
            input: None,
            data: None,
        };
        if !self.ec_enabled(doc, cmd) {
            return no;
        }
        let input_type = match cmd {
            "delete" => Some("deleteContentBackward"),
            "forwarddelete" => Some("deleteContentForward"),
            "insertparagraph" => Some("insertParagraph"),
            "insertlinebreak" => Some("insertLineBreak"),
            "inserttext" => Some("insertText"),
            "inserthtml" => Some("insertHTML"),
            "bold" => Some("formatBold"),
            "italic" => Some("formatItalic"),
            "underline" => Some("formatUnderline"),
            "strikethrough" => Some("formatStrikeThrough"),
            "superscript" => Some("formatSuperscript"),
            "subscript" => Some("formatSubscript"),
            "backcolor" | "hilitecolor" => Some("formatBackColor"),
            "forecolor" => Some("formatFontColor"),
            "fontname" => Some("formatFontName"),
            "fontsize" => Some(""),
            "removeformat" => Some("formatRemove"),
            "createlink" => Some("insertLink"),
            "unlink" => Some(""),
            "justifycenter" => Some("formatJustifyCenter"),
            "justifyfull" => Some("formatJustifyFull"),
            "justifyleft" => Some("formatJustifyLeft"),
            "justifyright" => Some("formatJustifyRight"),
            "insertorderedlist" => Some("insertOrderedList"),
            "insertunorderedlist" => Some("insertUnorderedList"),
            "indent" => Some("formatIndent"),
            "outdent" => Some("formatOutdent"),
            "formatblock" => Some(""),
            "inserthorizontalrule" => Some("insertHorizontalRule"),
            "insertimage" => Some("insertImage"),
            _ => None,
        };
        let data = match cmd {
            "inserttext" | "fontname" | "createlink" => Some(value.to_owned()),
            "backcolor" | "hilitecolor" | "forecolor" => serialize_color(value),
            _ => None,
        };
        // (the affected editing host: the one of the active range's start before the action)
        let host = input_type
            .and_then(|_| self.ec_active(doc))
            .and_then(|r| self.ec_host_of(r.start.0));
        let overrides = PRESERVES_OVERRIDES
            .contains(&cmd)
            .then(|| self.ec_record_current_overrides(doc));
        let ran = match cmd {
            _ if Self::ec_is_inline_command(cmd) => self.ec_inline_action(cx, doc, cmd, value),
            _ if justify::alignment_of(cmd).is_some() => {
                self.ec_justify_selection(cx, doc, justify::alignment_of(cmd).unwrap_or("left"));
                true
            }
            "insertorderedlist" => {
                self.ec_toggle_lists(cx, doc, "ol");
                true
            }
            "insertunorderedlist" => {
                self.ec_toggle_lists(cx, doc, "ul");
                true
            }
            "indent" => self.ec_indent(cx, doc),
            "outdent" => self.ec_outdent(cx, doc),
            "formatblock" => self.ec_format_block(cx, doc, value),
            "inserthorizontalrule" => self.ec_insert_horizontal_rule(cx, doc),
            "insertimage" => self.ec_insert_image(cx, doc, value),
            "delete" => self.ec_delete(cx, doc),
            "forwarddelete" => self.ec_forward_delete(cx, doc),
            "insertparagraph" => self.ec_insert_paragraph(cx, doc),
            "insertlinebreak" => self.ec_insert_line_break(cx, doc),
            "inserttext" => self.ec_insert_text(cx, doc, value),
            "inserthtml" => self.ec_insert_html(cx, doc, value),
            "defaultparagraphseparator" => {
                let v = value.to_ascii_lowercase();
                if v == "p" || v == "div" {
                    self.doc_state(doc, |s| s.paragraph_separator_p = v == "p");
                    true
                } else {
                    false
                }
            }
            "stylewithcss" => {
                let on = !value.eq_ignore_ascii_case("false");
                self.doc_state(doc, |s| s.css_styling = on);
                true
            }
            "usecss" => {
                let on = value.eq_ignore_ascii_case("false");
                self.doc_state(doc, |s| s.css_styling = on);
                true
            }
            "selectall" => {
                let target = {
                    let dom = self.dom();
                    dom.body(doc).or_else(|| dom.document_element(doc))
                };
                match (target, self.selection_of(doc)) {
                    (Some(t), Some(sel)) => {
                        let _ = SelectionImpl::select_all_children(self, cx, sel, node_handle(t));
                    }
                    (None, Some(_)) => self.set_doc_selection(doc, None),
                    _ => {}
                }
                true
            }
            _ => false,
        };
        if let Some(o) = overrides {
            if self.ec_active(doc).is_some_and(|r| r.start == r.end) {
                self.ec_restore_states_and_values(cx, doc, &o);
            }
        }
        let input = match (ran, host, input_type) {
            (true, Some(h), Some(t)) if self.dom().is_alive(h) => Some((h, t)),
            _ => None,
        };
        ExecOutcome { ran, input, data }
    }
}
