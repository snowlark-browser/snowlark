// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a drag from another program carries and how the window answers it, on every platform
//! (re-exported by each platform's `drop_target`).

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
    /// A link: offered as a URL and, to a file manager, as an internet shortcut file (unless
    /// `file` is offered).
    pub link: Option<DragLink>,
    /// Markup (`text/html`), offered as HTML.
    pub html: Option<String>,
    /// A file made from what is dragged (an image's), which a file manager or the desktop saves.
    pub file: Option<DragFile>,
    /// The operations the drag allows (none: copy and link).
    pub effects: Option<DropEffects>,
    /// The picture that follows the pointer.
    pub image: Option<DragImage>,
}

/// A file a drag out of the window offers by its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragFile {
    /// Its name (a valid file name, [`safe_file_name`]).
    pub name: String,
    pub contents: FileContents,
}

/// A dragged file's contents: at hand, or still on their way (fetched while the drag runs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileContents {
    Bytes(Vec<u8>),
    Later(LaterBytes),
}

/// Bytes that arrive from another thread: [`LaterBytes::set`] once, [`LaterBytes::wait`] for
/// them. Clones share them.
#[derive(Clone, Default)]
pub struct LaterBytes(Arc<(Mutex<Arrival>, Condvar)>);

/// Whether the bytes arrived (outer) and what arrived (inner; `None`: they will not come).
type Arrival = Option<Option<Arc<[u8]>>>;

impl LaterBytes {
    pub fn new() -> LaterBytes {
        LaterBytes::default()
    }

    /// The bytes arrived (`None`: they never will, the fetch failed). Only the first call counts.
    pub fn set(&self, bytes: Option<Vec<u8>>) {
        let (slot, cv) = &*self.0;
        let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
        if s.is_none() {
            *s = Some(bytes.map(Arc::from));
            cv.notify_all();
        }
    }

    /// The bytes, once they arrive, waiting at most `limit` for them; `None` when they will not
    /// come or did not in time.
    pub fn wait(&self, limit: Duration) -> Option<Arc<[u8]>> {
        let (slot, cv) = &*self.0;
        let s = slot.lock().unwrap_or_else(|e| e.into_inner());
        let (s, _) = cv
            .wait_timeout_while(s, limit, |s| s.is_none())
            .unwrap_or_else(|e| e.into_inner());
        s.clone().flatten()
    }
}

impl PartialEq for LaterBytes {
    fn eq(&self, other: &LaterBytes) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for LaterBytes {}

impl std::fmt::Debug for LaterBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let arrived = self.0 .0.lock().map(|s| s.is_some()).unwrap_or(false);
        f.debug_struct("LaterBytes")
            .field("arrived", &arrived)
            .finish()
    }
}

/// The operations a drag out of the window allows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DropEffects {
    pub copy: bool,
    pub link: bool,
    pub moves: bool,
}

impl DropEffects {
    /// The operations an `effectAllowed` value allows (HTML §6.11.3; `uninitialized` allows
    /// every one, as `all` does).
    pub fn from_effect_allowed(allowed: &str) -> DropEffects {
        let (copy, link, moves) = match allowed {
            "none" => (false, false, false),
            "copy" => (true, false, false),
            "link" => (false, true, false),
            "move" => (false, false, true),
            "copyLink" => (true, true, false),
            "copyMove" => (true, false, true),
            "linkMove" => (false, true, true),
            _ => (true, true, true),
        };
        DropEffects { copy, link, moves }
    }
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
    safe_file_name(&base, "url")
}

/// A file name made of `base` and the extension `ext`: the characters file names may not hold
/// replaced by `-`, cut to 125 characters (an empty name: "Untitled"); a reserved device name
/// gets a `_` before it.
pub fn safe_file_name(base: &str, ext: &str) -> String {
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
    format!("{name}.{ext}")
}

/// Markup as the Windows clipboard's HTML format (`CF_HTML`): a header giving the byte offsets
/// of the document and of the fragment, then a document holding the markup as its fragment.
pub fn cf_html(markup: &str) -> Vec<u8> {
    const HEADER: &str =
        "Version:0.9\r\nStartHTML:{a}\r\nEndHTML:{b}\r\nStartFragment:{c}\r\nEndFragment:{d}\r\n";
    // (each offset is written in ten digits: the header's length does not depend on them)
    let header_len = HEADER.len() - 4 * 3 + 4 * 10;
    let start = "<html>\r\n<body>\r\n<!--StartFragment-->";
    let end = "<!--EndFragment-->\r\n</body>\r\n</html>";
    let start_html = header_len;
    let start_fragment = start_html + start.len();
    let end_fragment = start_fragment + markup.len();
    let end_html = end_fragment + end.len();
    let header = HEADER
        .replace("{a}", &format!("{start_html:010}"))
        .replace("{b}", &format!("{end_html:010}"))
        .replace("{c}", &format!("{start_fragment:010}"))
        .replace("{d}", &format!("{end_fragment:010}"));
    format!("{header}{start}{markup}{end}").into_bytes()
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
    fn html_goes_with_its_offsets() {
        let b = cf_html("<a href=\"x\">é</a>");
        let s = String::from_utf8(b.clone()).unwrap();
        let offset = |key: &str| -> usize {
            let at = s.find(key).unwrap() + key.len();
            s[at..at + 10].parse().unwrap()
        };
        assert!(s.starts_with("Version:0.9\r\n"));
        assert_eq!(
            &b[offset("StartHTML:")..offset("StartHTML:") + 6],
            b"<html>"
        );
        assert_eq!(offset("EndHTML:"), b.len());
        assert_eq!(
            &b[offset("StartFragment:")..offset("EndFragment:")],
            "<a href=\"x\">é</a>".as_bytes()
        );
    }

    #[test]
    fn effect_allowed_gives_the_drag_its_operations() {
        let e = DropEffects::from_effect_allowed;
        assert_eq!(
            e("uninitialized"),
            DropEffects {
                copy: true,
                link: true,
                moves: true
            }
        );
        assert_eq!(e("none"), DropEffects::default());
        assert_eq!(
            e("copyLink"),
            DropEffects {
                copy: true,
                link: true,
                moves: false
            }
        );
        assert_eq!(safe_file_name("cat: photo", "png"), "cat- photo.png");
        assert_eq!(safe_file_name("", "png"), "Untitled.png");
    }

    #[test]
    fn a_shortcut_holds_its_url() {
        assert_eq!(
            shortcut_file_contents("https://a.example/?q=1"),
            b"[InternetShortcut]\r\nURL=https://a.example/?q=1\r\n"
        );
    }
}
