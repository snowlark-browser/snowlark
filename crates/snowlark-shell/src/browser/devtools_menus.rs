// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The developer tools' context menus other than the element tree's (`node_menu`): the console's,
//! a network request's, and the editing menu of the panel's text fields. Each offers the rows of
//! Chrome's DevTools that the panel has the data for; copying goes through the window's clipboard,
//! never the page.

use snowlark_page::devtools::ValueKind;

use super::commands::{AddressEdit, Command};
use crate::address::TextField;
use crate::devtools::console::ConsoleMenu;
use crate::devtools::network::NetworkMenu;
use crate::devtools::protocol::ResultOp;
use crate::devtools::FieldId;
use crate::driver::Clipboard;
use crate::menu::Entry;

/// What a console row does (besides copying text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleOp {
    Clear,
    ClearHistory,
    /// A row on the result of the evaluation with this number.
    Result(u64, ResultOp),
}

/// The console's menu (`ConsoleView.handleContextMenuEvent`, with the result's own rows from
/// `SourcesPanel.#appendRemoteObjectItems` first). "Save as…" needs a file dialog and is shown
/// disabled; the selection is copied as plain text ("Copy", where Chrome copies it styled).
pub fn console_entries(m: &ConsoleMenu) -> Vec<Entry<Command>> {
    let mut out = Vec::new();
    if let Some((id, kind)) = m.result {
        let op = |label: &str, op: ResultOp| {
            Entry::item(label, Command::DevtoolsConsole(ConsoleOp::Result(id, op)))
        };
        out.push(op("Store as global variable", ResultOp::Store));
        match kind {
            ValueKind::String => out.push(op("Copy string contents", ResultOp::Copy)),
            ValueKind::Object => out.push(op("Copy object", ResultOp::Copy)),
            ValueKind::Function | ValueKind::Primitive => {}
        }
        out.push(Entry::Separator);
    }
    out.push(Entry::item(
        "Clear console",
        Command::DevtoolsConsole(ConsoleOp::Clear),
    ));
    out.push(Entry::item(
        "Clear console history",
        Command::DevtoolsConsole(ConsoleOp::ClearHistory),
    ));
    out.push(Entry::Separator);
    out.push(
        Entry::item("Copy all messages", Command::CopyText(m.all.clone()))
            .enabled(!m.all.is_empty()),
    );
    out.push(Entry::item("Save as…", Command::CopyText(String::new())).enabled(false));
    if m.selection.is_some() || m.message.is_some() {
        out.push(Entry::Separator);
    }
    if let Some(s) = &m.selection {
        out.push(Entry::item("Copy", Command::CopyText(s.clone())).shortcut("Ctrl+C"));
    }
    if let Some(s) = &m.message {
        out.push(Entry::item("Copy message", Command::CopyText(s.clone())));
    }
    out
}

/// A request's menu: "Open in new tab" and the Copy submenu.
pub fn network_entries(m: &NetworkMenu) -> Vec<Entry<Command>> {
    let open = snowlark_net::Url::parse(&m.url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    vec![
        match open {
            Some(u) => Entry::item("Open in new tab", Command::OpenInNewTab(u)),
            None => Entry::item("Open in new tab", Command::CopyText(String::new())).enabled(false),
        },
        Entry::Separator,
        Entry::item("Copy", Command::DevtoolsNetCopy(Box::new(m.clone()))).submenu(),
    ]
}

/// The Copy submenu (`NetworkLogView.handleContextMenuForRequest`, in its order, less the
/// "Copy as" rows, the stack trace and HAR).
pub fn network_copy_entries(m: &NetworkMenu) -> Vec<Entry<Command>> {
    let copy = |label: &str, text: &Option<String>| {
        Entry::item(label, Command::CopyText(text.clone().unwrap_or_default()))
            .enabled(text.is_some())
    };
    vec![
        Entry::item("Copy URL", Command::CopyText(m.url.clone())),
        Entry::item(
            if m.filtered {
                "Copy all listed URLs"
            } else {
                "Copy all URLs"
            },
            Command::CopyText(m.all_urls.clone()),
        ),
        copy("Copy request headers", &m.request_headers),
        copy("Copy response headers", &m.response_headers),
        copy("Copy response", &m.response),
    ]
}

/// A panel field's editing menu.
pub fn field_entries(id: FieldId, field: &TextField, clip: &dyn Clipboard) -> Vec<Entry<Command>> {
    let selection = field.has_selection();
    let pasteable = clip.text().is_some_and(|t| !t.is_empty());
    let row = |label: &str, edit: AddressEdit, shortcut: &str, on: bool| {
        Entry::item(label, Command::DevtoolsField(id, edit))
            .enabled(on)
            .shortcut(shortcut)
    };
    vec![
        row("Undo", AddressEdit::Undo, "Ctrl+Z", field.can_undo()),
        Entry::Separator,
        row("Cut", AddressEdit::Cut, "Ctrl+X", selection),
        row("Copy", AddressEdit::Copy, "Ctrl+C", selection),
        row("Paste", AddressEdit::Paste, "Ctrl+V", pasteable),
        Entry::Separator,
        row(
            "Select all",
            AddressEdit::SelectAll,
            "Ctrl+A",
            !field.text().is_empty(),
        ),
    ]
}

/// What a row of a field's editing menu does to it.
pub fn field_edit(f: &mut TextField, edit: AddressEdit, clip: &dyn Clipboard) {
    match edit {
        AddressEdit::Undo => {
            f.undo();
        }
        AddressEdit::Cut => {
            if f.has_selection() {
                clip.set_text(&f.cut());
            }
        }
        AddressEdit::Copy => {
            if f.has_selection() {
                clip.set_text(f.selected_text());
            }
        }
        AddressEdit::Paste | AddressEdit::PasteAndGo => {
            if let Some(t) = clip.text() {
                f.insert(&t);
            }
        }
        AddressEdit::Delete => f.backspace(false),
        AddressEdit::SelectAll => f.select_all(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(e: &[Entry<Command>]) -> Vec<(String, bool)> {
        e.iter()
            .map(|e| match e {
                Entry::Item { label, enabled, .. } => (label.clone(), *enabled),
                Entry::Separator => ("|".to_owned(), true),
            })
            .collect()
    }

    fn command<'a>(e: &'a [Entry<Command>], name: &str) -> &'a Command {
        e.iter()
            .find_map(|e| match e {
                Entry::Item { label, action, .. } if label == name => Some(action),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no row {name}"))
    }

    #[test]
    fn the_console_menu_offers_what_was_clicked() {
        let m = ConsoleMenu {
            selection: Some("ell".into()),
            message: Some("app.js:3 hello".into()),
            result: None,
            all: "app.js:3 hello\n".into(),
        };
        let e = console_entries(&m);
        let rows: Vec<_> = labels(&e).into_iter().map(|(l, _)| l).collect();
        assert_eq!(
            rows.join(", "),
            "Clear console, Clear console history, |, Copy all messages, Save as…, |, Copy, \
             Copy message"
        );
        assert_eq!(command(&e, "Copy"), &Command::CopyText("ell".into()));
        assert_eq!(
            command(&e, "Copy message"),
            &Command::CopyText("app.js:3 hello".into())
        );
        assert_eq!(
            command(&e, "Copy all messages"),
            &Command::CopyText("app.js:3 hello\n".into())
        );
        // A string result: store it, copy its contents; an object: copy it; a number: store only.
        for (kind, copy) in [
            (ValueKind::String, Some("Copy string contents")),
            (ValueKind::Object, Some("Copy object")),
            (ValueKind::Primitive, None),
        ] {
            let e = console_entries(&ConsoleMenu {
                result: Some((4, kind)),
                selection: None,
                ..m.clone()
            });
            assert_eq!(
                command(&e, "Store as global variable"),
                &Command::DevtoolsConsole(ConsoleOp::Result(4, ResultOp::Store))
            );
            let rows: Vec<_> = labels(&e).into_iter().map(|(l, _)| l).collect();
            assert_eq!(rows[1] == "|", copy.is_none(), "{rows:?}");
            if let Some(copy) = copy {
                assert_eq!(
                    command(&e, copy),
                    &Command::DevtoolsConsole(ConsoleOp::Result(4, ResultOp::Copy))
                );
            }
            assert!(!rows.contains(&"Copy".to_owned()));
        }
    }

    #[test]
    fn a_request_copies_its_url_headers_and_response() {
        let m = NetworkMenu {
            url: "https://a.example/x.js".into(),
            all_urls: "https://a.example/\nhttps://a.example/x.js".into(),
            filtered: false,
            request_headers: Some("accept: */*".into()),
            response_headers: None,
            response: Some("let x;".into()),
        };
        let top = network_entries(&m);
        assert_eq!(
            command(&top, "Copy"),
            &Command::DevtoolsNetCopy(Box::new(m.clone()))
        );
        let e = network_copy_entries(&m);
        assert_eq!(
            labels(&e),
            [
                ("Copy URL".into(), true),
                ("Copy all URLs".into(), true),
                ("Copy request headers".into(), true),
                ("Copy response headers".into(), false),
                ("Copy response".into(), true),
            ]
        );
        assert_eq!(
            command(&e, "Copy response"),
            &Command::CopyText("let x;".into())
        );
        let e = network_copy_entries(&NetworkMenu {
            filtered: true,
            ..m
        });
        assert_eq!(labels(&e)[1].0, "Copy all listed URLs");
    }

    #[test]
    fn the_field_menu_edits_the_field() {
        let clip = crate::driver::MemoryClipboard::default();
        let mut f = TextField::new("1 + 1");
        f.select_all();
        let e = field_entries(FieldId::ConsoleInput, &f, &clip);
        assert_eq!(
            labels(&e)
                .iter()
                .filter(|(_, on)| *on)
                .map(|(l, _)| l.as_str())
                .collect::<Vec<_>>(),
            ["|", "Cut", "Copy", "|", "Select all"]
        );
        field_edit(&mut f, AddressEdit::Copy, &clip);
        assert_eq!(clip.text().as_deref(), Some("1 + 1"));
        f.set_text("");
        field_edit(&mut f, AddressEdit::Paste, &clip);
        assert_eq!(f.text(), "1 + 1");
    }
}
