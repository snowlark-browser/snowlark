// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! View page source (`view-source:` addresses, Ctrl+U): the response bytes of the address after
//! `view-source:`, decoded as the page decodes them (HTML's encoding determination for HTML, the
//! Content-Type's charset or UTF-8 otherwise), shown as a document of the browser's own:
//! monospaced, with line numbers, HTML's markup coloured.

use crate::settings::escape_html;

pub use crate::driver::MAX_SOURCE;

/// What a view-source address shows: the URL after `view-source:` when it is one the browser
/// fetches.
pub fn inner_url(url: &snowlark_net::Url) -> Option<snowlark_net::Url> {
    let s = url.as_str().strip_prefix("view-source:")?;
    let inner = snowlark_net::Url::parse(s).ok()?;
    matches!(inner.scheme(), "http" | "https" | "file" | "data").then_some(inner)
}

/// Whether a Content-Type is markup to colour.
pub fn is_markup(mime: &str) -> bool {
    let m = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    m.is_empty()
        || m == "text/html"
        || m == "application/xhtml+xml"
        || m == "image/svg+xml"
        || m.ends_with("/xml")
}

/// Decodes a response body as the page does.
pub fn decode(body: &[u8], headers: &snowlark_net::HeaderList, markup: bool) -> String {
    use snowlark_net::encoding;
    let transport = encoding::legacy_extract_encoding(headers);
    let enc = match transport {
        Some(e) => e,
        None if markup => encoding::sniff_html_encoding(&encoding::SniffInput::new(body)).0,
        None => encoding::encoding_for_label("utf-8").expect("UTF-8"),
    };
    encoding::decode(body, enc).0.into_owned()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Text,
    /// Inside a start or end tag, after its name.
    Tag,
    /// Raw text of `script`, `style` (until its end tag).
    Raw,
}

fn span(out: &mut String, class: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    out.push_str("<span class=");
    out.push_str(class);
    out.push('>');
    out.push_str(&escape_html(text));
    out.push_str("</span>");
}

/// The source with its markup in spans (`t` tags, `a` attribute names, `v` attribute values,
/// `m` comments, `d` doctypes and other declarations), escaped for HTML. Line breaks stay where
/// they are: spans never cross a line, so each line can be shown on its own.
pub fn highlight(src: &str) -> String {
    let mut out = String::with_capacity(src.len() * 2);
    let b = src.as_bytes();
    let mut i = 0;
    let mut state = State::Text;
    let mut raw_end = String::new();
    // (pushes `text` in `class` spans, one per line)
    let lines = |out: &mut String, class: &str, text: &str| {
        let mut first = true;
        for part in text.split('\n') {
            if !first {
                out.push('\n');
            }
            first = false;
            span(out, class, part);
        }
    };
    while i < b.len() {
        match state {
            State::Text | State::Raw => {
                let rest = &src[i..];
                if state == State::Raw {
                    // Text until the end tag.
                    let lower = rest.to_ascii_lowercase();
                    let end = lower.find(&raw_end).unwrap_or(rest.len());
                    out.push_str(&escape_html(&rest[..end]));
                    i += end;
                    state = State::Text;
                    continue;
                }
                if let Some(after) = rest.strip_prefix("<!--") {
                    let end = after.find("-->").map_or(rest.len(), |e| e + 7);
                    lines(&mut out, "m", &rest[..end]);
                    i += end;
                } else if rest.starts_with("<!") || rest.starts_with("<?") {
                    let end = rest.find('>').map_or(rest.len(), |e| e + 1);
                    lines(&mut out, "d", &rest[..end]);
                    i += end;
                } else if rest.starts_with('<')
                    && rest[1..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic() || c == '/')
                {
                    let name_len = rest[1..]
                        .find(|c: char| c.is_whitespace() || c == '>')
                        .unwrap_or(rest.len() - 1);
                    let name_end = 1 + name_len;
                    let name = &rest[..name_end];
                    // (a self-closing slash right after the name belongs to the tag's end)
                    let (name, name_end) = match name.strip_suffix('/') {
                        Some(n) if n.len() > 1 => (n, name_end - 1),
                        _ => (name, name_end),
                    };
                    span(&mut out, "t", name);
                    let tag = name.trim_start_matches(['<', '/']).to_ascii_lowercase();
                    raw_end = if !name.starts_with("</")
                        && matches!(
                            tag.as_str(),
                            "script" | "style" | "textarea" | "title" | "xmp"
                        ) {
                        format!("</{tag}")
                    } else {
                        String::new()
                    };
                    i += name_end;
                    state = State::Tag;
                } else {
                    let end = rest[1..].find('<').map_or(rest.len(), |e| e + 1);
                    out.push_str(&escape_html(&rest[..end]));
                    i += end;
                }
            }
            State::Tag => {
                let rest = &src[i..];
                let c = rest.chars().next().unwrap_or(' ');
                if c == '>' || rest.starts_with("/>") {
                    let n = if c == '>' { 1 } else { 2 };
                    span(&mut out, "t", &rest[..n]);
                    i += n;
                    state = if raw_end.is_empty() {
                        State::Text
                    } else {
                        State::Raw
                    };
                } else if c.is_whitespace() {
                    let n = rest
                        .find(|c: char| !c.is_whitespace())
                        .unwrap_or(rest.len());
                    out.push_str(&rest[..n]);
                    i += n;
                } else if c == '"' || c == '\'' {
                    let end = rest[1..].find(c).map_or(rest.len(), |e| e + 2);
                    lines(&mut out, "v", &rest[..end]);
                    i += end;
                } else if c == '=' {
                    out.push('=');
                    i += 1;
                    // An unquoted value.
                    let r = &src[i..];
                    if !r.starts_with(['"', '\'']) {
                        let n = r
                            .find(|c: char| c.is_whitespace() || c == '>')
                            .unwrap_or(r.len());
                        span(&mut out, "v", &r[..n]);
                        i += n;
                    }
                } else {
                    let n = rest
                        .find(|c: char| c.is_whitespace() || c == '=' || c == '>' || c == '/')
                        .unwrap_or(rest.len())
                        .max(c.len_utf8());
                    span(&mut out, "a", &rest[..n]);
                    i += n;
                }
            }
        }
    }
    out
}

/// The view-source document for `url` (the `view-source:` address) showing `text`: a row per
/// line with its number, long lines wrapped.
pub fn page(url: &str, text: &str, markup: bool, cut: bool) -> String {
    let body = if markup {
        highlight(text)
    } else {
        escape_html(text)
    };
    let mut rows = String::with_capacity(body.len() + text.len() / 4 + 64);
    for (n, line) in body.split('\n').enumerate() {
        rows.push_str("<div class=l><span class=n>");
        rows.push_str(&(n + 1).to_string());
        rows.push_str("</span><span class=c>");
        rows.push_str(line.strip_suffix('\r').unwrap_or(line));
        rows.push_str("</span></div>");
    }
    let note = if cut {
        format!(
            "<p class=note>Only the first {} MB are shown.</p>",
            MAX_SOURCE >> 20
        )
    } else {
        String::new()
    };
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{title}</title><style>\
html{{color-scheme:light dark}}\
body{{margin:0;padding:4px 0;background:Canvas;color:CanvasText;\
font:13px/1.5 Consolas,'Cascadia Mono',monospace}}\
.l{{display:flex;align-items:flex-start}}\
.n{{flex:none;width:4em;padding-right:12px;text-align:right;color:#999;user-select:none;\
border-right:1px solid #8884}}\
.c{{flex:1;min-width:0;padding:0 12px;white-space:pre-wrap;word-break:break-all}}\
.t{{color:#881280}}.a{{color:#994500}}.v{{color:#1a1aa6}}.m{{color:#236e25}}.d{{color:#999}}\
.note{{font:13px 'Segoe UI',sans-serif;margin:8px 16px;color:#999}}\
@media (prefers-color-scheme:dark){{.t{{color:#5db0d7}}.a{{color:#9bbbdc}}.v{{color:#f29766}}\
.m{{color:#898989}}}}\
</style></head><body>{note}{rows}</body></html>",
        title = escape_html(url),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_coloured_and_escaped() {
        let h = highlight(
            "<!doctype html>\n<a href=\"x?a=1&b\" id=y>Tom &amp; <b>Jerry</b></a><!-- c\nd -->",
        );
        assert_eq!(
            h,
            "<span class=d>&lt;!doctype html&gt;</span>\n\
             <span class=t>&lt;a</span> <span class=a>href</span>=<span class=v>&quot;x?a=1&amp;b&quot;</span> \
             <span class=a>id</span>=<span class=v>y</span><span class=t>&gt;</span>Tom &amp;amp; \
             <span class=t>&lt;b</span><span class=t>&gt;</span>Jerry<span class=t>&lt;/b</span><span class=t>&gt;</span>\
             <span class=t>&lt;/a</span><span class=t>&gt;</span><span class=m>&lt;!-- c</span>\n<span class=m>d --&gt;</span>"
        );
    }

    #[test]
    fn scripts_are_text_and_lines_are_numbered() {
        let h = highlight("<script>if (a < b) x = '<b>';</script><br/>");
        assert!(h.contains("if (a &lt; b) x = &#39;&lt;b&gt;&#39;;"), "{h}");
        assert!(
            h.ends_with("<span class=t>&lt;br</span><span class=t>/&gt;</span>"),
            "{h}"
        );
        let p = page(
            "view-source:https://a.example/",
            "one\ntwo\nthree",
            false,
            false,
        );
        assert!(p.contains("<span class=n>3</span><span class=c>three</span>"));
        assert_eq!(p.matches("<div class=l>").count(), 3);
        assert!(p.contains("<title>view-source:https://a.example/</title>"));
        assert!(page("v", "x", true, true).contains("Only the first 8 MB"));
    }

    #[test]
    fn addresses_and_decoding() {
        let u = |s: &str| snowlark_net::Url::parse(s).unwrap();
        assert_eq!(
            inner_url(&u("view-source:https://a.example/p?q=1")).map(|u| u.to_string()),
            Some("https://a.example/p?q=1".into())
        );
        assert!(inner_url(&u("view-source:javascript:alert(1)")).is_none());
        assert!(inner_url(&u("view-source:view-source:https://a.example/")).is_none());
        assert!(is_markup("text/html; charset=utf-8"));
        assert!(!is_markup("text/css"));
        let mut h = snowlark_net::HeaderList::new();
        // windows-1252 from the transport charset, or from the document's meta.
        h.append("Content-Type", "text/html; charset=windows-1252");
        assert_eq!(decode(b"caf\xe9", &h, true), "café");
        let none = snowlark_net::HeaderList::new();
        assert_eq!(
            decode(b"<meta charset=windows-1252>caf\xe9", &none, true),
            "<meta charset=windows-1252>café"
        );
        assert_eq!(decode("café".as_bytes(), &none, false), "café");
    }
}
