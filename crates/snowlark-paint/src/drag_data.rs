// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a drag from another program carries and how the window answers it, on every platform
//! (re-exported by each platform's `drop_target`).

use std::path::PathBuf;

/// A drag operation, as the drop target answers it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DropEffect {
    #[default]
    None,
    Copy,
    Move,
    Link,
}

/// What a drag from another program carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DragData {
    /// The files (and folders) dragged.
    pub files: Vec<PathBuf>,
    /// The text.
    pub text: Option<String>,
    /// The operations the source allows.
    pub copy: bool,
    pub moves: bool,
    pub link: bool,
}

/// What a drag out of the window carries ([`crate::window::Window::start_drag`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DragOut {
    /// The text, which every target that takes text gets.
    pub text: String,
    /// A link: offered as a URL and, to a file manager, as an internet shortcut file.
    pub link: Option<DragLink>,
    /// The picture that follows the pointer.
    pub image: Option<DragImage>,
}

/// A dragged link.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DragLink {
    pub url: String,
    /// Its page's title: the shortcut file's name.
    pub title: String,
}

/// The picture that follows the pointer during a drag out of the window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DragImage {
    pub width: u32,
    pub height: u32,
    /// Premultiplied BGRA, rows `width * 4` bytes apart.
    pub bgra: Vec<u8>,
    /// Where the pointer is in the picture.
    pub hotspot: (i32, i32),
}

/// The file name of a link's internet shortcut: the title (an empty one: the link's host, or
/// "Untitled") with the characters file names may not hold replaced by `-`, cut to 125
/// characters, then `.url`; a reserved device name gets a `_` before it.
pub fn shortcut_file_name(link: &DragLink) -> String {
    let base = match link.title.trim() {
        "" => url_host(&link.url).unwrap_or_default(),
        t => t.to_string(),
    };
    let mut name: String = base
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    // (half of MAX_PATH for the whole name, so the folder's path still fits beside it)
    if let Some((i, _)) = name.char_indices().nth(125) {
        name.truncate(i);
    }
    // (Windows drops trailing dots and spaces from a file name)
    let kept = name.trim_end_matches(['.', ' ']).len();
    name.truncate(kept);
    if name.is_empty() {
        name = "Untitled".into();
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        name.insert(0, '_');
    }
    name + ".url"
}

/// The contents of an internet shortcut file to `url`.
pub fn shortcut_file_contents(url: &str) -> Vec<u8> {
    format!("[InternetShortcut]\r\nURL={url}\r\n").into_bytes()
}

/// The host (and port) of a URL with an authority.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(url: &str, title: &str) -> String {
        shortcut_file_name(&DragLink {
            url: url.into(),
            title: title.into(),
        })
    }

    #[test]
    fn a_shortcut_is_named_after_its_page() {
        assert_eq!(name("https://a.example/", "Hacker News"), "Hacker News.url");
        assert_eq!(
            name("https://a.example/", "a/b: c? \"d\" <e>|f*"),
            "a-b- c- -d- -e--f-.url"
        );
        // (no title: the host, its port's colon replaced)
        assert_eq!(
            name("https://user@a.example:8080/x?y", "  "),
            "a.example-8080.url"
        );
        assert_eq!(name("about:blank", ""), "Untitled.url");
        assert_eq!(name("x:", "con"), "_con.url");
        assert_eq!(name("x:", "LPT1.txt"), "_LPT1.txt.url");
        assert_eq!(name("x:", "COMMON"), "COMMON.url");
        assert_eq!(name("x:", "dots..."), "dots.url");
        assert_eq!(name("x:", &"é".repeat(300)).chars().count(), 125 + 4);
    }

    #[test]
    fn a_shortcut_holds_its_url() {
        assert_eq!(
            shortcut_file_contents("https://a.example/?q=1"),
            b"[InternetShortcut]\r\nURL=https://a.example/?q=1\r\n"
        );
    }
}
