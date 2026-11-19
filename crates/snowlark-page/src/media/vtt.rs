// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebVTT parsing: the file parser (WebVTT §6.1 "WebVTT parser algorithm", with "collect a
//! WebVTT block", "collect WebVTT cue timings and settings" and "collect a WebVTT timestamp"),
//! and the cue text parser (§6.4 "WebVTT cue text parsing rules", with its tokenizer) whose tree
//! `getCueAsHTML()` turns into DOM nodes (§6.5). Regions and style sheets are recognised and
//! dropped; cue text is kept as written.

/// A parsed cue.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cue {
    pub id: String,
    pub start: f64,
    pub end: f64,
    pub text: String,
    /// `vertical`: "", "rl" or "lr".
    pub vertical: String,
    /// `line`: a number (lines or a percentage) or none for auto.
    pub line: Option<f64>,
    pub snap_to_lines: bool,
    pub line_align: String,
    pub position: Option<f64>,
    pub position_align: String,
    pub size: f64,
    pub align: String,
}

impl Cue {
    /// A cue with the WebVTT defaults (§6.1 "cue creation").
    pub fn new() -> Cue {
        Cue {
            snap_to_lines: true,
            line_align: "start".into(),
            position_align: "auto".into(),
            size: 100.0,
            align: "center".into(),
            ..Cue::default()
        }
    }
}

/// Digits collected at `*at` in `s` (the "collect a sequence of code points" step): the text.
fn digits<'a>(s: &'a [u8], at: &mut usize) -> &'a [u8] {
    let start = *at;
    while s.get(*at).is_some_and(u8::is_ascii_digit) {
        *at += 1;
    }
    &s[start..*at]
}

fn value(d: &[u8]) -> f64 {
    d.iter().fold(0.0, |v, b| v * 10.0 + f64::from(b - b'0'))
}

/// §6.1 "collect a WebVTT timestamp" at `*at` in `s`, moving `*at` past it.
fn collect_timestamp(s: &[u8], at: &mut usize) -> Option<f64> {
    let mut hours = false;
    if !s.get(*at)?.is_ascii_digit() {
        return None;
    }
    let d = digits(s, at);
    let mut v1 = value(d);
    if d.len() != 2 || v1 > 59.0 {
        hours = true;
    }
    if s.get(*at) != Some(&b':') {
        return None;
    }
    *at += 1;
    let d = digits(s, at);
    if d.len() != 2 {
        return None;
    }
    let mut v2 = value(d);
    let v3;
    if hours || s.get(*at) == Some(&b':') {
        if s.get(*at) != Some(&b':') {
            return None;
        }
        *at += 1;
        let d = digits(s, at);
        if d.len() != 2 {
            return None;
        }
        v3 = value(d);
    } else {
        v3 = v2;
        v2 = v1;
        v1 = 0.0;
    }
    if s.get(*at) != Some(&b'.') {
        return None;
    }
    *at += 1;
    let d = digits(s, at);
    if d.len() != 3 {
        return None;
    }
    let v4 = value(d);
    if v2 > 59.0 || v3 > 59.0 {
        return None;
    }
    Some(v1 * 3600.0 + v2 * 60.0 + v3 + v4 / 1000.0)
}

/// A whole string that is one WebVTT timestamp.
pub fn parse_timestamp(s: &str) -> Option<f64> {
    let mut at = 0;
    let t = collect_timestamp(s.as_bytes(), &mut at)?;
    (at == s.len()).then_some(t)
}

/// §6.1 "parse a percentage string": digits, an optional fraction, then `%`, within 0..=100.
fn percentage(v: &str) -> Option<f64> {
    let n = v.strip_suffix('%')?;
    let (int, frac) = match n.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (n, None),
    };
    let all = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    if !all(int) || frac.is_some_and(|f| !all(f)) {
        return None;
    }
    let x: f64 = n.parse().ok()?;
    (0.0..=100.0).contains(&x).then_some(x)
}

/// The `line` setting's value without a `%` (§6.3): an optional leading `-`, digits, at most one
/// `.` between digits.
fn line_number(v: &str) -> Option<f64> {
    if !v.bytes().any(|b| b.is_ascii_digit())
        || v.bytes()
            .any(|b| !(b.is_ascii_digit() || b == b'-' || b == b'.'))
        || v[1.min(v.len())..].contains('-')
        || v.matches('.').count() > 1
    {
        return None;
    }
    if let Some(i) = v.find('.') {
        let b = v.as_bytes();
        if i == 0 || !b[i - 1].is_ascii_digit() || !b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            return None;
        }
    }
    v.parse().ok()
}

/// §6.3 "parse the WebVTT cue settings".
fn settings(cue: &mut Cue, s: &str) {
    for setting in s.split_ascii_whitespace() {
        let Some((name, value)) = setting.split_once(':') else {
            continue;
        };
        if name.is_empty() || value.is_empty() {
            continue;
        }
        match name {
            "vertical" if value == "rl" || value == "lr" => cue.vertical = value.into(),
            "line" => {
                let (pos, align) = match value.split_once(',') {
                    Some((n, a)) => (n, Some(a)),
                    None => (value, None),
                };
                let percent = pos.ends_with('%');
                let number = if percent {
                    percentage(pos)
                } else {
                    line_number(pos)
                };
                let Some(number) = number else { continue };
                let align = match align {
                    None => "start",
                    Some(a @ ("start" | "center" | "end")) => a,
                    Some(_) => continue,
                };
                cue.line = Some(number);
                cue.snap_to_lines = !percent;
                cue.line_align = align.into();
            }
            "position" => {
                let (pos, align) = match value.split_once(',') {
                    Some((n, a)) => (n, Some(a)),
                    None => (value, None),
                };
                let Some(p) = percentage(pos) else { continue };
                let align = match align {
                    None => "auto",
                    Some(a @ ("line-left" | "center" | "line-right")) => a,
                    Some(_) => continue,
                };
                cue.position = Some(p);
                cue.position_align = align.into();
            }
            "size" => {
                if let Some(p) = percentage(value) {
                    cue.size = p;
                }
            }
            "align" if ["start", "center", "end", "left", "right"].contains(&value) => {
                cue.align = value.into()
            }
            _ => {}
        }
    }
}

/// §6.1 "collect WebVTT cue timings and settings" from a line into `cue`.
fn timings_and_settings(cue: &mut Cue, line: &str) -> Option<()> {
    let s = line.as_bytes();
    let ws = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r');
    let mut at = 0;
    let skip = |at: &mut usize| {
        while s.get(*at).is_some_and(ws) {
            *at += 1;
        }
    };
    skip(&mut at);
    cue.start = collect_timestamp(s, &mut at)?;
    skip(&mut at);
    if s.get(at..at + 3) != Some(b"-->") {
        return None;
    }
    at += 3;
    skip(&mut at);
    cue.end = collect_timestamp(s, &mut at)?;
    settings(cue, &line[at..]);
    Some(())
}

/// Parses a WebVTT file. `None` when it does not start with the `WEBVTT` signature (the track
/// fails to load).
pub fn parse(input: &str) -> Option<Vec<Cue>> {
    let input = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let text = input
        .replace('\0', "\u{FFFD}")
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let b = text.as_bytes();
    // The signature: "WEBVTT" alone or followed by a space, a tab or a line feed.
    if !text.starts_with("WEBVTT") || b.get(6).is_some_and(|c| !matches!(c, b' ' | b'\t' | b'\n')) {
        return None;
    }
    let mut p = Parser {
        text: &text,
        at: 0,
        seen_cue: false,
    };
    p.line();
    let mut cues = Vec::new();
    if p.at >= text.len() {
        return Some(cues);
    }
    p.at += 1;
    if p.at >= text.len() {
        return Some(cues);
    }
    // The header.
    if b[p.at] != b'\n' {
        p.block(true);
    } else {
        p.at += 1;
    }
    p.newlines();
    while p.at < text.len() {
        if let Some(c) = p.block(false) {
            cues.push(c);
        }
        p.newlines();
    }
    Some(cues)
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
    seen_cue: bool,
}

impl<'a> Parser<'a> {
    fn line(&mut self) -> &'a str {
        let rest = &self.text[self.at..];
        let n = rest.find('\n').unwrap_or(rest.len());
        self.at += n;
        &rest[..n]
    }

    fn newlines(&mut self) {
        while self.text.as_bytes().get(self.at) == Some(&b'\n') {
            self.at += 1;
        }
    }

    /// §6.1 "collect a WebVTT block": a cue, or `None` for anything else (a comment, a style
    /// sheet, a region, the header, or a cue whose timings do not parse).
    fn block(&mut self, in_header: bool) -> Option<Cue> {
        let mut line_count = 0;
        let mut previous = self.at;
        let mut buffer = String::new();
        let mut seen_arrow = false;
        let mut cue: Option<Cue> = None;
        loop {
            let line = self.line();
            line_count += 1;
            let eof = self.at >= self.text.len();
            if !eof {
                self.at += 1;
            }
            if line.contains("-->") {
                if !in_header && (line_count == 1 || (line_count == 2 && !seen_arrow)) {
                    seen_arrow = true;
                    previous = self.at;
                    let mut c = Cue::new();
                    c.id = std::mem::take(&mut buffer);
                    if timings_and_settings(&mut c, line).is_some() {
                        self.seen_cue = true;
                        cue = Some(c);
                    } else {
                        cue = None;
                    }
                } else {
                    self.at = previous;
                    break;
                }
            } else if line.is_empty() {
                break;
            } else {
                if !in_header && line_count == 2 && !self.seen_cue {
                    // A style sheet or a region definition: its text is not a cue's.
                    let starts = |kw: &str| {
                        buffer
                            .strip_prefix(kw)
                            .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t', '\n']))
                            && !buffer.contains("-->")
                    };
                    if starts("STYLE") || starts("REGION") {
                        buffer.clear();
                    }
                }
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(line);
                previous = self.at;
            }
            if eof {
                break;
            }
        }
        let mut cue = cue?;
        cue.text = buffer;
        Some(cue)
    }
}

// ----- cue text ------------------------------------------------------------------------------

/// A node of a cue's text (§6.4 "WebVTT Node Objects").
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Text(String),
    Timestamp(f64),
    Element(Element),
}

/// An internal node: a class, italic, bold, underline, ruby, ruby text, voice or language object.
#[derive(Clone, Debug, PartialEq)]
pub struct Element {
    /// The WebVTT tag: "c", "i", "b", "u", "ruby", "rt", "v" or "lang".
    pub tag: &'static str,
    pub classes: Vec<String>,
    /// A voice's name or a language object's language.
    pub annotation: String,
    pub children: Vec<Node>,
}

impl Element {
    /// The HTML element it becomes (§6.5).
    pub fn html_name(&self) -> &'static str {
        match self.tag {
            "c" | "v" | "lang" => "span",
            t => t,
        }
    }
}

enum Token {
    Text(String),
    Start {
        name: String,
        classes: Vec<String>,
        annotation: Option<String>,
    },
    End(String),
    Timestamp(String),
}

fn is_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0c' | ' ')
}

/// HTML "consume a character reference" after an `&` at `chars[*at - 1]`: the characters it
/// stands for, `*at` moved past it, or `None` (the `&` is itself). `in_attribute` is the
/// annotation's case (`>` as the additional allowed character).
fn char_ref(chars: &[char], at: &mut usize, in_attribute: bool) -> Option<String> {
    let start = *at;
    match chars.get(start) {
        Some('#') => {
            let mut i = start + 1;
            let hex = matches!(chars.get(i), Some('x' | 'X'));
            if hex {
                i += 1;
            }
            let radix = if hex { 16 } else { 10 };
            let ds = i;
            let mut v: u32 = 0;
            while let Some(d) = chars.get(i).and_then(|c| c.to_digit(radix)) {
                v = v.saturating_mul(radix).saturating_add(d);
                i += 1;
            }
            if i == ds {
                return None;
            }
            if chars.get(i) == Some(&';') {
                i += 1;
            }
            *at = i;
            Some(numeric_char(v).to_string())
        }
        Some(c) if c.is_ascii_alphanumeric() => {
            let mut end = start;
            while end - start < 32 && chars.get(end).is_some_and(char::is_ascii_alphanumeric) {
                end += 1;
            }
            let semi = chars.get(end) == Some(&';');
            let name: String = chars[start..end].iter().collect();
            let lookup = |k: &str| {
                snowlark_dom::NAMED_ENTITIES
                    .get(k)
                    .filter(|e| e.0 != 0)
                    .copied()
            };
            if semi {
                if let Some(e) = lookup(&format!("{name};")) {
                    *at = end + 1;
                    return Some(entity(e));
                }
            }
            // The longest legacy name (no semicolon) that is a prefix.
            for n in (1..=name.len()).rev() {
                if let Some(e) = lookup(&name[..n]) {
                    let next = chars.get(start + n);
                    if in_attribute && next.is_some_and(|c| *c == '=' || c.is_ascii_alphanumeric())
                    {
                        return None;
                    }
                    *at = start + n;
                    return Some(entity(e));
                }
            }
            None
        }
        _ => None,
    }
}

fn entity((a, b): (u32, u32)) -> String {
    char::from_u32(a)
        .into_iter()
        .chain(if b != 0 { char::from_u32(b) } else { None })
        .collect()
}

/// HTML "numeric character reference end state".
fn numeric_char(v: u32) -> char {
    const C1: [u16; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160,
        0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013,
        0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
    ];
    match v {
        0 => '\u{FFFD}',
        0x80..=0x9F => char::from_u32(u32::from(C1[(v - 0x80) as usize])).unwrap_or('\u{FFFD}'),
        _ => char::from_u32(v).unwrap_or('\u{FFFD}'),
    }
}

/// §6.4 "WebVTT cue text tokenizer": the next token at `*at`.
fn token(chars: &[char], at: &mut usize) -> Token {
    #[derive(Clone, Copy)]
    enum S {
        Data,
        Tag,
        StartTag,
        Class,
        Annotation,
        EndTag,
        Timestamp,
    }
    let mut state = S::Data;
    let mut result = String::new();
    let mut buffer = String::new();
    let mut classes = Vec::new();
    loop {
        let c = chars.get(*at).copied();
        match state {
            S::Data => match c {
                Some('&') => {
                    *at += 1;
                    match char_ref(chars, at, false) {
                        Some(s) => result.push_str(&s),
                        None => result.push('&'),
                    }
                    continue;
                }
                Some('<') => {
                    if result.is_empty() {
                        state = S::Tag;
                    } else {
                        return Token::Text(result);
                    }
                }
                None => return Token::Text(result),
                Some(c) => result.push(c),
            },
            S::Tag => match c {
                Some(c) if is_ws(c) => state = S::Annotation,
                Some('.') => state = S::Class,
                Some('/') => state = S::EndTag,
                Some(c) if c.is_ascii_digit() => {
                    result.push(c);
                    state = S::Timestamp;
                }
                Some('>') => {
                    *at += 1;
                    return start(result, classes, None);
                }
                None => return start(result, classes, None),
                Some(c) => {
                    result.push(c);
                    state = S::StartTag;
                }
            },
            S::StartTag => match c {
                Some('\t' | '\x0c' | ' ') => state = S::Annotation,
                Some('\n') => {
                    buffer.push('\n');
                    state = S::Annotation;
                }
                Some('.') => state = S::Class,
                Some('>') => {
                    *at += 1;
                    return start(result, classes, None);
                }
                None => return start(result, classes, None),
                Some(c) => result.push(c),
            },
            S::Class => match c {
                Some('\t' | '\x0c' | ' ') => {
                    classes.push(std::mem::take(&mut buffer));
                    state = S::Annotation;
                }
                Some('\n') => {
                    classes.push(std::mem::take(&mut buffer));
                    buffer.push('\n');
                    state = S::Annotation;
                }
                Some('.') => classes.push(std::mem::take(&mut buffer)),
                Some('>') => {
                    *at += 1;
                    classes.push(buffer);
                    return start(result, classes, None);
                }
                None => {
                    classes.push(buffer);
                    return start(result, classes, None);
                }
                Some(c) => buffer.push(c),
            },
            S::Annotation => match c {
                Some('&') => {
                    *at += 1;
                    match char_ref(chars, at, true) {
                        Some(s) => buffer.push_str(&s),
                        None => buffer.push('&'),
                    }
                    continue;
                }
                Some('>') => {
                    *at += 1;
                    return start(result, classes, Some(buffer));
                }
                None => return start(result, classes, Some(buffer)),
                Some(c) => buffer.push(c),
            },
            S::EndTag => match c {
                Some('>') => {
                    *at += 1;
                    return Token::End(result);
                }
                None => return Token::End(result),
                Some(c) => result.push(c),
            },
            S::Timestamp => match c {
                Some('>') => {
                    *at += 1;
                    return Token::Timestamp(result);
                }
                None => return Token::Timestamp(result),
                Some(c) => result.push(c),
            },
        }
        *at += 1;
    }
}

fn start(name: String, classes: Vec<String>, annotation: Option<String>) -> Token {
    // The annotation with its whitespace collapsed and trimmed.
    let annotation = annotation.map(|a| {
        a.split(is_ws)
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    });
    Token::Start {
        name,
        classes: classes.into_iter().filter(|c| !c.is_empty()).collect(),
        annotation,
    }
}

/// §6.4 "WebVTT cue text parsing rules": the cue text's tree.
pub fn parse_cue_text(text: &str) -> Vec<Node> {
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0;
    // The open elements, innermost last; the root's children are `root`.
    let mut root: Vec<Node> = Vec::new();
    let mut open: Vec<Element> = Vec::new();
    fn append(root: &mut Vec<Node>, open: &mut [Element], n: Node) {
        match open.last_mut() {
            Some(e) => e.children.push(n),
            None => root.push(n),
        }
    }
    fn close(root: &mut Vec<Node>, open: &mut Vec<Element>) {
        if let Some(e) = open.pop() {
            append(root, open, Node::Element(e));
        }
    }
    let mut languages: Vec<String> = Vec::new();
    while at < chars.len() {
        match token(&chars, &mut at) {
            Token::Text(s) => append(&mut root, &mut open, Node::Text(s)),
            Token::Start {
                name,
                classes,
                annotation,
            } => {
                let tag = match name.as_str() {
                    "c" => "c",
                    "i" => "i",
                    "b" => "b",
                    "u" => "u",
                    "ruby" => "ruby",
                    "rt" if open.last().is_some_and(|e| e.tag == "ruby") => "rt",
                    "v" => "v",
                    "lang" => "lang",
                    _ => continue,
                };
                let annotation = match tag {
                    "v" | "lang" => annotation.unwrap_or_default(),
                    _ => String::new(),
                };
                if tag == "lang" {
                    languages.push(annotation.clone());
                }
                open.push(Element {
                    tag,
                    classes,
                    annotation,
                    children: Vec::new(),
                });
            }
            Token::End(name) => {
                let current = open.last().map(|e| e.tag);
                if current == Some(name.as_str()) {
                    if name == "lang" {
                        languages.pop();
                    }
                    close(&mut root, &mut open);
                } else if name == "ruby" && current == Some("rt") {
                    close(&mut root, &mut open);
                    close(&mut root, &mut open);
                }
            }
            Token::Timestamp(v) => {
                if let Some(t) = parse_timestamp(&v) {
                    append(&mut root, &mut open, Node::Timestamp(t));
                }
            }
        }
    }
    while !open.is_empty() {
        close(&mut root, &mut open);
    }
    root
}

/// A timestamp as a processing instruction's data (§6.5): every component, hours with at least
/// two digits.
pub fn timestamp_string(t: f64) -> String {
    let ms = (t * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp("00:01.500"), Some(1.5));
        assert_eq!(parse_timestamp("01:00:00.000"), Some(3600.0));
        assert_eq!(parse_timestamp("1:00:00.000"), Some(3600.0));
        assert_eq!(parse_timestamp("100:00:00.000"), Some(360000.0));
        assert_eq!(parse_timestamp("1:00.000"), None);
        assert_eq!(parse_timestamp("00:60.000"), None);
        assert_eq!(parse_timestamp("00:01.5"), None);
        assert_eq!(parse_timestamp("60:00.000"), None);
    }

    #[test]
    fn cues() {
        let f = "WEBVTT - a file\n\nNOTE a comment\n\n1\n00:00.000 --> 00:01.000 align:start line:10%\nHello\nworld\n\n00:02.000 --> 00:03.250\n<b>Bold</b>\n";
        let c = parse(f).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].id, "1");
        assert_eq!((c[0].start, c[0].end), (0.0, 1.0));
        assert_eq!(c[0].text, "Hello\nworld");
        assert_eq!(c[0].align, "start");
        assert_eq!(c[0].line, Some(10.0));
        assert!(!c[0].snap_to_lines);
        assert_eq!(c[1].id, "");
        assert_eq!(c[1].end, 3.25);
        assert!(parse("not a vtt").is_none());
        assert!(parse("WEBVTTX").is_none());
        assert_eq!(parse("WEBVTT").unwrap().len(), 0);
        assert_eq!(
            parse("WEBVTT\r\n\r\n00:00.000 --> 00:01.000\r\nx").unwrap()[0].text,
            "x"
        );
    }

    /// An arrow in a cue's text ends it and starts the next cue; the header may run into the
    /// first cue; a cue with bad timings is dropped with its text.
    #[test]
    fn recovery() {
        let c = parse("WEBVTT\nheader\n00:00.000 --> 00:01.000\na\n00:01.000 --> 00:02.000\nb\n\nbad --> 00:03.000\nlost\n\nid\n00:03.000 --> 00:04.000\nc").unwrap();
        let t: Vec<(&str, &str)> = c.iter().map(|c| (c.id.as_str(), c.text.as_str())).collect();
        assert_eq!(t, [("", "a"), ("", "b"), ("id", "c")]);
        // An id line, then a second id-looking line: not a cue.
        assert_eq!(
            parse("WEBVTT\n\na\nb\n00:00.000 --> 00:01.000\nx")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn settings_values() {
        let c = |s: &str| {
            parse(&format!("WEBVTT\n\n00:00.000 --> 00:01.000 {s}\nx"))
                .unwrap()
                .remove(0)
        };
        assert_eq!(c("line:-1").line, Some(-1.0));
        assert_eq!(c("line:1-").line, None);
        assert_eq!(c("line:1.5").line, Some(1.5));
        assert_eq!(c("line:.5").line, None);
        assert_eq!(c("line:auto").line, None);
        assert_eq!(c("line:10%,center").line_align, "center");
        assert_eq!(c("line:10%,bad").line, None);
        assert_eq!(c("position:10%,line-left").position_align, "line-left");
        assert_eq!(c("position:101%").position, None);
        assert_eq!(c("size:1.%").size, 100.0);
        assert_eq!(c("align:left").align, "left");
    }

    #[test]
    fn cue_text() {
        let n = parse_cue_text(
            "a &amp; <c.x.y>b</c> <v.loud Mary  Ann>hi</v><ruby>r<rt>t</ruby><00:01.000>&lt",
        );
        assert_eq!(n[0], Node::Text("a & ".into()));
        let Node::Element(c) = &n[1] else { panic!() };
        assert_eq!(
            (c.tag, c.classes.clone()),
            ("c", vec!["x".to_string(), "y".to_string()])
        );
        let Node::Element(v) = &n[3] else { panic!() };
        assert_eq!((v.tag, v.annotation.as_str()), ("v", "Mary Ann"));
        let Node::Element(r) = &n[4] else { panic!() };
        assert_eq!(r.children.len(), 2);
        assert_eq!(n[5], Node::Timestamp(1.0));
        assert_eq!(n[6], Node::Text("<".into()));
        assert_eq!(n.len(), 7);
        assert_eq!(timestamp_string(3661.5), "01:01:01.500");
    }
}
