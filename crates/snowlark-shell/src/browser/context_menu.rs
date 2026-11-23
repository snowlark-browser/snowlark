// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The page's context menu (a right-click the page didn't cancel, Shift+F10, the menu key): what
//! it offers for a link, an image, selected text or the page.

use snowlark_net::Url;
use snowlark_paint::Point;

use super::commands::{AddressEdit, Command};
use super::{Browser, Focus};
use crate::address::{one_line, TextField};
use crate::chrome::{self, glyph, Hit};
use crate::driver::{Clipboard, ContextInfo};
use crate::menu::{Entry, MenuIcon};
use crate::tab::TabCommand;

/// The rows of a context menu. `inspect` is the content-area point "Inspect" picks the element
/// at.
pub fn context_entries(
    info: &ContextInfo,
    can_back: bool,
    can_forward: bool,
    engine: &str,
    inspect: Point,
    incognito: bool,
) -> Vec<Entry<Command>> {
    let t = &info.target;
    let mut out = Vec::new();
    if let Some(link) = &t.link {
        out.push(Entry::item(
            "Open link in new tab",
            Command::OpenInNewTab(link.clone()),
        ));
        // (from a regular window; an Incognito window's new tab is already Incognito)
        if !incognito {
            out.push(Entry::item(
                "Open link in Incognito window",
                Command::OpenInIncognito(link.clone()),
            ));
        }
        out.push(
            Entry::item("Copy link address", Command::CopyText(link.to_string()))
                .icon(MenuIcon::Glyph(glyph::COPY)),
        );
        out.push(Entry::Separator);
    }
    if let Some(img) = &t.image {
        out.push(Entry::item(
            "Open image in new tab",
            Command::OpenInNewTab(img.clone()),
        ));
        out.push(
            Entry::item("Save image as…", Command::SaveImage(img.clone()))
                .icon(MenuIcon::Glyph(glyph::SAVE)),
        );
        out.push(Entry::item(
            "Copy image address",
            Command::CopyText(img.to_string()),
        ));
        out.push(Entry::Separator);
    }
    if let Some(sel) = &t.selection {
        let short: String = sel.split_whitespace().collect::<Vec<_>>().join(" ");
        let short = if short.chars().count() > 30 {
            format!("{}…", short.chars().take(30).collect::<String>())
        } else {
            short
        };
        out.push(
            Entry::item("Copy", Command::CopyText(sel.clone()))
                .shortcut("Ctrl+C")
                .icon(MenuIcon::Glyph(glyph::COPY)),
        );
        out.push(
            Entry::item(
                &format!("Search {engine} for “{short}”"),
                Command::SearchFor(sel.clone()),
            )
            .icon(MenuIcon::Glyph(glyph::SEARCH)),
        );
        out.push(Entry::Separator);
    }
    if t.link.is_none() && t.image.is_none() && t.selection.is_none() {
        out.push(
            Entry::item("Back", Command::Back)
                .shortcut("Alt+Left")
                .enabled(can_back)
                .icon(MenuIcon::Glyph(glyph::BACK)),
        );
        out.push(
            Entry::item("Forward", Command::Forward)
                .shortcut("Alt+Right")
                .enabled(can_forward)
                .icon(MenuIcon::Glyph(glyph::FORWARD)),
        );
        out.push(
            Entry::item("Reload", Command::Reload)
                .shortcut("Ctrl+R")
                .icon(MenuIcon::Glyph(glyph::REFRESH)),
        );
        out.push(Entry::Separator);
        out.push(Entry::item("View page source", Command::ViewSource).shortcut("Ctrl+U"));
    }
    out.push(
        Entry::item("Inspect", Command::Inspect(inspect))
            .shortcut("Ctrl+Shift+I")
            .icon(MenuIcon::Glyph(glyph::CODE)),
    );
    out
}

/// The rows of the address bar's context menu (Chrome's, less Emoji, search engines and full
/// URLs): a row with nothing to act on is disabled.
pub fn address_entries(field: &TextField, clip: &dyn Clipboard) -> Vec<Entry<Command>> {
    let selection = field.has_selection();
    let pasteable = clip.text().is_some_and(|t| !one_line(&t).is_empty());
    let row = |label: &str, edit: AddressEdit, shortcut: &str, on: bool| {
        let e = Entry::item(label, Command::Address(edit)).enabled(on);
        if shortcut.is_empty() {
            e
        } else {
            e.shortcut(shortcut)
        }
    };
    vec![
        row("Undo", AddressEdit::Undo, "Ctrl+Z", field.can_undo()),
        Entry::Separator,
        row("Cut", AddressEdit::Cut, "Ctrl+X", selection),
        row("Copy", AddressEdit::Copy, "Ctrl+C", selection),
        row("Paste", AddressEdit::Paste, "Ctrl+V", pasteable),
        row("Paste and go", AddressEdit::PasteAndGo, "", pasteable),
        row("Delete", AddressEdit::Delete, "", selection),
        Entry::Separator,
        row(
            "Select all",
            AddressEdit::SelectAll,
            "Ctrl+A",
            !field.text().is_empty(),
        ),
    ]
}

/// A file name for a resource at `url`: its last path segment, else `fallback`.
pub fn file_name_for(url: &Url, fallback: &str) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back().map(str::to_owned))
        .filter(|s| !s.is_empty())
        .map(|s| {
            // (percent-escapes decoded where they are UTF-8)
            let bytes: Vec<u8> = {
                let b = s.as_bytes();
                let mut out = Vec::with_capacity(b.len());
                let mut i = 0;
                while i < b.len() {
                    if b[i] == b'%' && i + 2 < b.len() {
                        if let Ok(v) = u8::from_str_radix(
                            std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""),
                            16,
                        ) {
                            out.push(v);
                            i += 3;
                            continue;
                        }
                    }
                    out.push(b[i]);
                    i += 1;
                }
                out
            };
            String::from_utf8(bytes).unwrap_or(s)
        })
        .unwrap_or_else(|| fallback.to_owned())
}

impl Browser {
    /// The page at tab `i` asked for its context menu.
    pub(super) fn show_context_menu(&mut self, i: usize, info: ContextInfo) {
        if i != self.tabs.active_index() || self.popup.is_some() {
            return;
        }
        let Some(e) = self.tabs.get(i) else {
            return;
        };
        let (can_back, can_forward, zoom) = (e.tab.can_go(-1), e.tab.can_go(1), e.zoom);
        let r = self.page_rect();
        let at = Point::new(r.x + info.point.x * zoom, r.y + info.point.y * zoom);
        let entries = context_entries(
            &info,
            can_back,
            can_forward,
            self.settings.engine().name,
            at,
            self.off_the_record(),
        );
        self.open_menu(entries, at, Hit::None, info.keyboard);
    }

    /// A right press on the address bar: the field takes focus (selecting all, when it hadn't),
    /// the caret goes where the pointer is unless that is inside the selection, and the menu opens.
    pub(super) fn address_context_menu(&mut self, dip: Point) {
        if self.focus != Focus::Address {
            self.focus_address();
        } else {
            let x = dip.x - self.layout().address_text.x + self.address_scroll;
            let i = self
                .fonts
                .index_at(self.address.text(), x, chrome::address_font_size());
            self.address.context_press(i);
        }
        self.caret_on = true;
        self.chrome_dirty = true;
        let entries = address_entries(&self.address, self.tab_ctx.clipboard.as_ref());
        self.open_menu(entries, Point::new(dip.x, 0.0), Hit::Address, false);
    }

    /// What a row of the address bar's menu does.
    pub(super) fn address_edit(&mut self, edit: AddressEdit) {
        let clip = self.tab_ctx.clipboard.clone();
        match edit {
            AddressEdit::Undo => {
                self.address.undo();
            }
            AddressEdit::Cut => {
                if self.address.has_selection() {
                    clip.set_text(&self.address.cut());
                }
            }
            AddressEdit::Copy => {
                if self.address.has_selection() {
                    clip.set_text(self.address.selected_text());
                }
            }
            AddressEdit::Paste => {
                if let Some(t) = clip.text() {
                    self.address.paste(&t);
                }
            }
            AddressEdit::PasteAndGo => {
                let Some(t) = clip.text().map(|t| one_line(&t)).filter(|t| !t.is_empty()) else {
                    return;
                };
                self.address.select_all();
                self.address.paste(&t);
                self.selected = None;
                // (the same path as Enter: an address opens, anything else is a search)
                self.submit_address();
                return;
            }
            AddressEdit::Delete => self.address.backspace(false),
            AddressEdit::SelectAll => self.address.select_all(),
        }
        self.focus = Focus::Address;
        self.caret_on = true;
        self.update_suggestions();
        self.chrome_dirty = true;
    }

    /// Shift+F10 and the menu key: the context menu where the pointer is over the page, else for
    /// the focused element.
    pub(super) fn keyboard_context_menu(&mut self) {
        let at = self.page_hover.then(|| self.page_point(self.mouse));
        self.send_active(TabCommand::ContextMenu(at));
    }
}

/// The file name a download is saved as: `Content-Disposition`'s `filename` (RFC 6266, the plain
/// form; a path or a quote in it is not taken), else the URL's last segment.
pub fn download_file_name(url: &Url, head: &snowlark_net::ResponseHead) -> String {
    let from_header = head.headers.get("content-disposition").and_then(|d| {
        d.split(';').skip(1).find_map(|p| {
            let (name, value) = p.split_once('=')?;
            if !name.trim().eq_ignore_ascii_case("filename") {
                return None;
            }
            let value = value.trim().trim_matches('"').trim();
            (!value.is_empty()
                && !value.contains(['/', '\\', '"', ':'])
                && value != "."
                && value != "..")
                .then(|| value.to_owned())
        })
    });
    from_header.unwrap_or_else(|| file_name_for(url, "download"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_page::embedder::ContextTarget;

    fn labels(e: &[Entry<Command>]) -> Vec<String> {
        e.iter()
            .map(|e| match e {
                Entry::Item { label, .. } => label.clone(),
                Entry::Separator => "-".into(),
            })
            .collect()
    }

    #[test]
    fn address_menu_rows_and_what_enables_them() {
        use crate::driver::MemoryClipboard;
        let clip = MemoryClipboard::default();
        let rows = |f: &TextField| -> Vec<(String, bool)> {
            address_entries(f, &clip)
                .iter()
                .map(|e| match e {
                    Entry::Item { label, enabled, .. } => (label.clone(), *enabled),
                    Entry::Separator => ("-".into(), false),
                })
                .collect()
        };
        let names = |r: &[(String, bool)]| r.iter().map(|(l, _)| l.clone()).collect::<Vec<_>>();
        let on = |r: &[(String, bool)]| {
            r.iter()
                .filter(|(l, e)| *e && l != "-")
                .map(|(l, _)| l.clone())
                .collect::<Vec<_>>()
        };
        // Nothing typed and nothing on the clipboard: every row is disabled.
        let r = rows(&TextField::new(""));
        assert_eq!(
            names(&r),
            [
                "Undo",
                "-",
                "Cut",
                "Copy",
                "Paste",
                "Paste and go",
                "Delete",
                "-",
                "Select all"
            ]
        );
        assert!(on(&r).is_empty());
        // Text without a selection: Select all.
        let mut f = TextField::new("rust");
        f.set_caret(2, false);
        assert_eq!(on(&rows(&f)), ["Select all"]);
        // A selection: Cut, Copy, Delete.
        f.select_all();
        assert_eq!(on(&rows(&f)), ["Cut", "Copy", "Delete", "Select all"]);
        // Clipboard text: Paste and Paste and go; blank text is nothing to paste.
        clip.set_text(" \r\n ");
        assert!(!on(&rows(&f)).contains(&"Paste".to_owned()));
        clip.set_text("snowlark");
        assert_eq!(
            on(&rows(&f)),
            [
                "Cut",
                "Copy",
                "Paste",
                "Paste and go",
                "Delete",
                "Select all"
            ]
        );
        // After an edit there is something to undo; the shortcuts shown are the ones that work.
        f.insert("x");
        assert!(on(&rows(&f)).contains(&"Undo".to_owned()));
        let shown: Vec<String> = address_entries(&f, &clip)
            .iter()
            .filter_map(|e| match e {
                Entry::Item { shortcut, .. } if !shortcut.is_empty() => Some(shortcut.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(shown, ["Ctrl+Z", "Ctrl+X", "Ctrl+C", "Ctrl+V", "Ctrl+A"]);
    }

    #[test]
    fn rows_for_what_is_under_the_pointer() {
        let info = |target: ContextTarget| ContextInfo {
            point: Point::ZERO,
            target,
            keyboard: false,
        };
        let page = context_entries(
            &info(ContextTarget::default()),
            false,
            true,
            "Bing",
            Point::ZERO,
            false,
        );
        assert_eq!(
            labels(&page),
            [
                "Back",
                "Forward",
                "Reload",
                "-",
                "View page source",
                "Inspect"
            ]
        );
        assert!(matches!(&page[0], Entry::Item { enabled: false, .. }));
        let link = Url::parse("https://a.example/x").unwrap();
        let img = Url::parse("https://a.example/pic%20one.png").unwrap();
        let both = context_entries(
            &info(ContextTarget {
                link: Some(link.clone()),
                image: Some(img.clone()),
                selection: Some("some   words".into()),
                ..Default::default()
            }),
            true,
            true,
            "Bing",
            Point::ZERO,
            false,
        );
        assert_eq!(
            labels(&both),
            [
                "Open link in new tab",
                "Open link in Incognito window",
                "Copy link address",
                "-",
                "Open image in new tab",
                "Save image as…",
                "Copy image address",
                "-",
                "Copy",
                "Search Bing for “some words”",
                "-",
                "Inspect"
            ]
        );
        assert!(both
            .iter()
            .any(|e| matches!(e, Entry::Item { action: Command::SaveImage(u), .. } if *u == img)));
        assert!(both.iter().any(
            |e| matches!(e, Entry::Item { action: Command::OpenInIncognito(u), .. } if *u == link)
        ));
        // An Incognito window's link menu has no row for another Incognito window.
        let in_incognito = context_entries(
            &info(ContextTarget {
                link: Some(link.clone()),
                ..Default::default()
            }),
            false,
            false,
            "Bing",
            Point::ZERO,
            true,
        );
        let rows = labels(&in_incognito);
        let has = |label: &str| rows.iter().any(|r| r == label);
        assert!(has("Open link in new tab") && has("Copy link address"));
        assert!(!has("Open link in Incognito window"));
        assert_eq!(file_name_for(&img, "image"), "pic one.png");
        assert_eq!(
            file_name_for(&Url::parse("https://a.example/").unwrap(), "image"),
            "image"
        );
    }
}
