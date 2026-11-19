// Snowlark: the URL Standard's basic URL parser as written (https://url.spec.whatwg.org/#concept-basic-url-parser),
// over a URL record, and the conversions between that record and `Url`. `Url::parse`, `Url::join`
// and the `quirks` setters (the URL class's API) run through it. See ../SNOWLARK-PATCHES.md.

use crate::host::{Host, HostInternal};
use crate::parser::ParseError;
use crate::Url;
use alloc::borrow::{Cow, ToOwned};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::cell::OnceCell;

use form_urlencoded::EncodingOverride;

/// A URL's host (URL Standard §3.1): a domain or opaque host, an IP address, or the empty host.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RecHost {
    Domain(String),
    Ipv4(crate::net::Ipv4Addr),
    Ipv6(crate::net::Ipv6Addr),
    Empty,
}

/// A URL's path: an opaque path (a URL path segment) or a list of segments.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RecPath {
    Opaque(String),
    List(Vec<String>),
}

/// A URL record (URL Standard §4.1).
#[derive(Clone, Debug)]
pub(crate) struct Record {
    pub scheme: String,
    pub username: String,
    pub password: String,
    pub host: Option<RecHost>,
    pub port: Option<u16>,
    pub path: RecPath,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

impl Default for Record {
    fn default() -> Self {
        Record {
            scheme: String::new(),
            username: String::new(),
            password: String::new(),
            host: None,
            port: None,
            path: RecPath::List(Vec::new()),
            query: None,
            fragment: None,
        }
    }
}

/// The states of the basic URL parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    SchemeStart,
    Scheme,
    NoScheme,
    SpecialRelativeOrAuthority,
    PathOrAuthority,
    Relative,
    RelativeSlash,
    SpecialAuthoritySlashes,
    SpecialAuthorityIgnoreSlashes,
    Authority,
    Host,
    Hostname,
    Port,
    File,
    FileSlash,
    FileHost,
    PathStart,
    Path,
    OpaquePath,
    Query,
    Fragment,
}

pub(crate) fn is_special_scheme(s: &str) -> bool {
    matches!(s, "ftp" | "file" | "http" | "https" | "ws" | "wss")
}

pub(crate) fn special_default_port(s: &str) -> Option<u16> {
    match s {
        "ftp" => Some(21),
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    }
}

impl Record {
    fn is_special(&self) -> bool {
        is_special_scheme(&self.scheme)
    }

    fn includes_credentials(&self) -> bool {
        !self.username.is_empty() || !self.password.is_empty()
    }

    pub(crate) fn has_opaque_path(&self) -> bool {
        matches!(self.path, RecPath::Opaque(_))
    }

    /// "cannot have a username/password/port".
    pub(crate) fn cannot_have_username_password_port(&self) -> bool {
        matches!(self.host, None | Some(RecHost::Empty)) || self.scheme == "file"
    }

    fn list(&mut self) -> &mut Vec<String> {
        if let RecPath::Opaque(_) = self.path {
            self.path = RecPath::List(Vec::new());
        }
        match &mut self.path {
            RecPath::List(l) => l,
            RecPath::Opaque(_) => unreachable!(),
        }
    }

    /// "shorten a url's path".
    fn shorten_path(&mut self) {
        let file = self.scheme == "file";
        let path = self.list();
        if file && path.len() == 1 && is_normalized_windows_drive_letter(&path[0]) {
            return;
        }
        path.pop();
    }

    /// The record of `url`.
    pub(crate) fn from_url(url: &Url) -> Record {
        let host = if url.has_authority() {
            Some(match url.host() {
                None => RecHost::Empty,
                Some(Host::Domain(d)) => RecHost::Domain(d.to_owned()),
                Some(Host::Ipv4(a)) => RecHost::Ipv4(a),
                Some(Host::Ipv6(a)) => RecHost::Ipv6(a),
            })
        } else {
            None
        };
        let p = url.path();
        let path = if url.cannot_be_a_base() {
            RecPath::Opaque(p.to_owned())
        } else if p.is_empty() {
            RecPath::List(Vec::new())
        } else {
            RecPath::List(p[1..].split('/').map(|s| s.to_owned()).collect())
        };
        Record {
            scheme: url.scheme().to_owned(),
            username: url.username().to_owned(),
            password: url.password().unwrap_or("").to_owned(),
            host,
            port: url.port(),
            path,
            query: url.query().map(|q| q.to_owned()),
            fragment: url.fragment().map(|f| f.to_owned()),
        }
    }

    /// The URL serializer, with the offsets `Url` keeps.
    pub(crate) fn to_url(&self) -> Url {
        let mut s = String::with_capacity(
            self.scheme.len() + 16 + self.query.as_ref().map_or(0, |q| q.len()),
        );
        s.push_str(&self.scheme);
        let scheme_end = s.len() as u32;
        s.push(':');
        let (username_end, host_start, host_end, host) = if let Some(host) = &self.host {
            s.push_str("//");
            let mut username_end = s.len() as u32;
            if self.includes_credentials() {
                s.push_str(&self.username);
                username_end = s.len() as u32;
                if !self.password.is_empty() {
                    s.push(':');
                    s.push_str(&self.password);
                }
                s.push('@');
            }
            let host_start = s.len() as u32;
            let internal = match host {
                RecHost::Domain(d) => {
                    s.push_str(d);
                    if d.is_empty() {
                        HostInternal::None
                    } else {
                        HostInternal::Domain
                    }
                }
                RecHost::Ipv4(a) => {
                    let _ = write!(s, "{}", Host::<&str>::Ipv4(*a));
                    HostInternal::Ipv4(*a)
                }
                RecHost::Ipv6(a) => {
                    let _ = write!(s, "{}", Host::<&str>::Ipv6(*a));
                    HostInternal::Ipv6(*a)
                }
                RecHost::Empty => HostInternal::None,
            };
            let host_end = s.len() as u32;
            if let Some(port) = self.port {
                let _ = write!(s, ":{}", port);
            }
            (username_end, host_start, host_end, internal)
        } else {
            let at = s.len() as u32;
            if let RecPath::List(l) = &self.path {
                if l.len() > 1 && l[0].is_empty() {
                    s.push_str("/.");
                }
            }
            (at, at, at, HostInternal::None)
        };
        let path_start = s.len() as u32;
        match &self.path {
            RecPath::Opaque(p) => s.push_str(p),
            RecPath::List(l) => {
                for seg in l {
                    s.push('/');
                    s.push_str(seg);
                }
            }
        }
        let query_start = self.query.as_ref().map(|q| {
            let at = s.len() as u32;
            s.push('?');
            s.push_str(q);
            at
        });
        let fragment_start = self.fragment.as_ref().map(|f| {
            let at = s.len() as u32;
            s.push('#');
            s.push_str(f);
            at
        });
        Url {
            serialization: s,
            scheme_end,
            username_end,
            host_start,
            host_end,
            host,
            port: if self.host.is_some() { self.port } else { None },
            path_start,
            query_start,
            fragment_start,
        }
    }
}

// ----- percent-encode sets (URL Standard §1.3) -----

fn c0_control_set(c: char) -> bool {
    c <= '\u{1F}' || c > '~'
}

fn fragment_set(c: char) -> bool {
    c0_control_set(c) || matches!(c, ' ' | '"' | '<' | '>' | '`')
}

fn query_set(c: char) -> bool {
    c0_control_set(c) || matches!(c, ' ' | '"' | '#' | '<' | '>')
}

fn special_query_set(c: char) -> bool {
    query_set(c) || c == '\''
}

fn path_set(c: char) -> bool {
    query_set(c) || matches!(c, '?' | '^' | '`' | '{' | '}')
}

pub(crate) fn userinfo_set(c: char) -> bool {
    path_set(c) || matches!(c, '/' | ':' | ';' | '=' | '@' | '['..=']' | '|')
}

/// "UTF-8 percent-encode" `c` using `set`, appending to `out`.
fn utf8_percent_encode_char(c: char, set: fn(char) -> bool, out: &mut String) {
    if !set(c) {
        out.push(c);
        return;
    }
    let mut buf = [0u8; 4];
    for b in c.encode_utf8(&mut buf).bytes() {
        let _ = write!(out, "%{:02X}", b);
    }
}

pub(crate) fn utf8_percent_encode_str(s: &str, set: fn(char) -> bool, out: &mut String) {
    for c in s.chars() {
        utf8_percent_encode_char(c, set, out);
    }
}

// ----- Windows drive letters and dot segments (URL Standard §4.2) -----

fn is_windows_drive_letter(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && (b[1] == b':' || b[1] == b'|')
}

fn is_normalized_windows_drive_letter(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn starts_with_windows_drive_letter(cps: &[char]) -> bool {
    cps.len() >= 2
        && cps[0].is_ascii_alphabetic()
        && matches!(cps[1], ':' | '|')
        && (cps.len() == 2 || matches!(cps[2], '/' | '\\' | '?' | '#'))
}

fn is_single_dot(s: &str) -> bool {
    s == "." || s.eq_ignore_ascii_case("%2e")
}

fn is_double_dot(s: &str) -> bool {
    matches!(s, "..")
        || s.eq_ignore_ascii_case(".%2e")
        || s.eq_ignore_ascii_case("%2e.")
        || s.eq_ignore_ascii_case("%2e%2e")
}

/// The host parser, with `is_opaque` for non-special URLs.
fn host_parse(input: &str, is_opaque: bool) -> Result<RecHost, ParseError> {
    let host = if is_opaque {
        Host::parse_opaque_cow(Cow::Borrowed(input))?
    } else {
        Host::parse_cow(Cow::Borrowed(input))?
    };
    Ok(match host {
        Host::Domain(d) if d.is_empty() => RecHost::Empty,
        Host::Domain(d) => RecHost::Domain(d.into_owned()),
        Host::Ipv4(a) => RecHost::Ipv4(a),
        Host::Ipv6(a) => RecHost::Ipv6(a),
    })
}

/// The parser's base URL. Its record is built the first time a state reads more than the scheme: an
/// absolute input never needs it.
struct Base<'a> {
    url: Option<&'a Url>,
    record: OnceCell<Record>,
}

impl<'a> Base<'a> {
    fn new(url: Option<&'a Url>) -> Self {
        Base {
            url,
            record: OnceCell::new(),
        }
    }

    fn scheme(&self) -> Option<&str> {
        self.url.map(Url::scheme)
    }

    fn record(&self) -> Option<&Record> {
        let url = self.url?;
        Some(self.record.get_or_init(|| Record::from_url(url)))
    }
}

/// The basic URL parser without `url` or state override: a new URL or failure.
pub(crate) fn parse(
    input: &str,
    base: Option<&Url>,
    encoding: EncodingOverride<'_>,
) -> Result<Record, ParseError> {
    let trimmed = input.trim_matches(|c: char| c <= ' ');
    let mut url = Record::default();
    run(trimmed, &Base::new(base), &mut url, None, encoding)?;
    Ok(url)
}

/// The basic URL parser with `url` and `state_override` (the API setters). `Err` is "failure";
/// `url` keeps what the steps before it changed, as the standard's in-place modification does.
pub(crate) fn parse_with_override(
    input: &str,
    url: &mut Record,
    state_override: State,
) -> Result<(), ParseError> {
    run(input, &Base::new(None), url, Some(state_override), None)
}

#[allow(clippy::cognitive_complexity)]
fn run(
    input: &str,
    base: &Base<'_>,
    url: &mut Record,
    state_override: Option<State>,
    mut encoding: EncodingOverride<'_>,
) -> Result<(), ParseError> {
    use State::*;
    // Remove all ASCII tab or newline from input.
    let chars: Vec<char> = input
        .chars()
        .filter(|&c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let n = chars.len() as isize;
    let so = state_override;
    let mut state = so.unwrap_or(SchemeStart);
    let mut buffer = String::new();
    let mut at_sign_seen = false;
    let mut inside_brackets = false;
    let mut password_token_seen = false;
    let mut p: isize = 0;
    let at = |i: isize| -> Option<char> {
        if i >= 0 && i < n {
            Some(chars[i as usize])
        } else {
            None
        }
    };
    let remaining = |i: isize| -> &[char] {
        let start = (i + 1).clamp(0, n) as usize;
        &chars[start..]
    };
    loop {
        let c = at(p);
        match state {
            SchemeStart => {
                if let Some(ch) = c.filter(|ch| ch.is_ascii_alphabetic()) {
                    buffer.push(ch.to_ascii_lowercase());
                    state = Scheme;
                } else if so.is_none() {
                    state = NoScheme;
                    p -= 1;
                } else {
                    return Err(ParseError::RelativeUrlWithoutBase);
                }
            }
            Scheme => {
                if let Some(ch) =
                    c.filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
                {
                    buffer.push(ch.to_ascii_lowercase());
                } else if c == Some(':') {
                    if so.is_some() {
                        if url.is_special() != is_special_scheme(&buffer) {
                            return Ok(());
                        }
                        if (url.includes_credentials() || url.port.is_some()) && buffer == "file" {
                            return Ok(());
                        }
                        if url.scheme == "file" && url.host == Some(RecHost::Empty) {
                            return Ok(());
                        }
                    }
                    url.scheme = core::mem::take(&mut buffer);
                    if so.is_some() {
                        if url.port.is_some() && url.port == special_default_port(&url.scheme) {
                            url.port = None;
                        }
                        return Ok(());
                    }
                    if url.scheme == "file" {
                        state = File;
                    } else if url.is_special() && base.scheme().is_some_and(|s| s == url.scheme) {
                        state = SpecialRelativeOrAuthority;
                    } else if url.is_special() {
                        state = SpecialAuthoritySlashes;
                    } else if remaining(p).first() == Some(&'/') {
                        state = PathOrAuthority;
                        p += 1;
                    } else {
                        url.path = RecPath::Opaque(String::new());
                        state = OpaquePath;
                    }
                } else if so.is_none() {
                    buffer.clear();
                    state = NoScheme;
                    p = -1;
                } else {
                    return Err(ParseError::RelativeUrlWithoutBase);
                }
            }
            NoScheme => {
                let base = match base.record() {
                    Some(b) if !(b.has_opaque_path() && c != Some('#')) => b,
                    _ => return Err(ParseError::RelativeUrlWithoutBase),
                };
                if base.has_opaque_path() && c == Some('#') {
                    url.scheme = base.scheme.clone();
                    url.path = base.path.clone();
                    url.query = base.query.clone();
                    url.fragment = Some(String::new());
                    state = Fragment;
                } else if base.scheme != "file" {
                    state = Relative;
                    p -= 1;
                } else {
                    state = File;
                    p -= 1;
                }
            }
            SpecialRelativeOrAuthority => {
                if c == Some('/') && remaining(p).first() == Some(&'/') {
                    state = SpecialAuthorityIgnoreSlashes;
                    p += 1;
                } else {
                    state = Relative;
                    p -= 1;
                }
            }
            PathOrAuthority => {
                if c == Some('/') {
                    state = Authority;
                } else {
                    state = Path;
                    p -= 1;
                }
            }
            Relative => {
                let base = base.record().expect("relative state has a base");
                url.scheme = base.scheme.clone();
                if c == Some('/') || (url.is_special() && c == Some('\\')) {
                    state = RelativeSlash;
                } else {
                    url.username = base.username.clone();
                    url.password = base.password.clone();
                    url.host = base.host.clone();
                    url.port = base.port;
                    url.path = base.path.clone();
                    url.query = base.query.clone();
                    if c == Some('?') {
                        url.query = Some(String::new());
                        state = Query;
                    } else if c == Some('#') {
                        url.fragment = Some(String::new());
                        state = Fragment;
                    } else if c.is_some() {
                        url.query = None;
                        url.shorten_path();
                        state = Path;
                        p -= 1;
                    }
                }
            }
            RelativeSlash => {
                if url.is_special() && matches!(c, Some('/') | Some('\\')) {
                    state = SpecialAuthorityIgnoreSlashes;
                } else if c == Some('/') {
                    state = Authority;
                } else {
                    let base = base.record().expect("relative slash state has a base");
                    url.username = base.username.clone();
                    url.password = base.password.clone();
                    url.host = base.host.clone();
                    url.port = base.port;
                    state = Path;
                    p -= 1;
                }
            }
            SpecialAuthoritySlashes => {
                if c == Some('/') && remaining(p).first() == Some(&'/') {
                    state = SpecialAuthorityIgnoreSlashes;
                    p += 1;
                } else {
                    state = SpecialAuthorityIgnoreSlashes;
                    p -= 1;
                }
            }
            SpecialAuthorityIgnoreSlashes => {
                if !matches!(c, Some('/') | Some('\\')) {
                    state = Authority;
                    p -= 1;
                }
            }
            Authority => {
                if c == Some('@') {
                    if at_sign_seen {
                        buffer.insert_str(0, "%40");
                    }
                    at_sign_seen = true;
                    for cp in buffer.chars() {
                        if cp == ':' && !password_token_seen {
                            password_token_seen = true;
                            continue;
                        }
                        if password_token_seen {
                            utf8_percent_encode_char(cp, userinfo_set, &mut url.password);
                        } else {
                            utf8_percent_encode_char(cp, userinfo_set, &mut url.username);
                        }
                    }
                    buffer.clear();
                } else if matches!(c, None | Some('/') | Some('?') | Some('#'))
                    || (url.is_special() && c == Some('\\'))
                {
                    if at_sign_seen && buffer.is_empty() {
                        return Err(ParseError::EmptyHost);
                    }
                    p -= buffer.chars().count() as isize + 1;
                    buffer.clear();
                    state = Host;
                } else if let Some(ch) = c {
                    buffer.push(ch);
                }
            }
            Host | Hostname => {
                if so.is_some() && url.scheme == "file" {
                    p -= 1;
                    state = FileHost;
                } else if c == Some(':') && !inside_brackets {
                    if buffer.is_empty() {
                        return Err(ParseError::EmptyHost);
                    }
                    if so == Some(Hostname) {
                        return Err(ParseError::InvalidPort);
                    }
                    let host = host_parse(&buffer, !url.is_special())?;
                    url.host = Some(host);
                    buffer.clear();
                    state = Port;
                } else if matches!(c, None | Some('/') | Some('?') | Some('#'))
                    || (url.is_special() && c == Some('\\'))
                {
                    p -= 1;
                    if url.is_special() && buffer.is_empty() {
                        return Err(ParseError::EmptyHost);
                    } else if so.is_some()
                        && buffer.is_empty()
                        && (url.includes_credentials() || url.port.is_some())
                    {
                        return Err(ParseError::EmptyHost);
                    }
                    let host = host_parse(&buffer, !url.is_special())?;
                    url.host = Some(host);
                    buffer.clear();
                    state = PathStart;
                    if so.is_some() {
                        return Ok(());
                    }
                } else if let Some(ch) = c {
                    if ch == '[' {
                        inside_brackets = true;
                    }
                    if ch == ']' {
                        inside_brackets = false;
                    }
                    buffer.push(ch);
                }
            }
            Port => {
                if let Some(ch) = c.filter(|ch| ch.is_ascii_digit()) {
                    buffer.push(ch);
                } else if matches!(c, None | Some('/') | Some('?') | Some('#'))
                    || (url.is_special() && c == Some('\\'))
                    || so.is_some()
                {
                    if !buffer.is_empty() {
                        let mut port: u32 = 0;
                        for d in buffer.bytes() {
                            port = port * 10 + u32::from(d - b'0');
                            if port > 65535 {
                                return Err(ParseError::InvalidPort);
                            }
                        }
                        let port = port as u16;
                        url.port = if Some(port) == special_default_port(&url.scheme) {
                            None
                        } else {
                            Some(port)
                        };
                        buffer.clear();
                        if so.is_some() {
                            return Ok(());
                        }
                    }
                    if so.is_some() {
                        return Err(ParseError::InvalidPort);
                    }
                    state = PathStart;
                    p -= 1;
                } else {
                    return Err(ParseError::InvalidPort);
                }
            }
            File => {
                url.scheme = "file".to_owned();
                url.host = Some(RecHost::Empty);
                if matches!(c, Some('/') | Some('\\')) {
                    state = FileSlash;
                } else if let Some(base) = base.record().filter(|b| b.scheme == "file") {
                    url.host = base.host.clone();
                    url.path = base.path.clone();
                    url.query = base.query.clone();
                    if c == Some('?') {
                        url.query = Some(String::new());
                        state = Query;
                    } else if c == Some('#') {
                        url.fragment = Some(String::new());
                        state = Fragment;
                    } else if c.is_some() {
                        url.query = None;
                        if !starts_with_windows_drive_letter(&chars[p as usize..]) {
                            url.shorten_path();
                        } else {
                            url.path = RecPath::List(Vec::new());
                        }
                        state = Path;
                        p -= 1;
                    }
                } else {
                    state = Path;
                    p -= 1;
                }
            }
            FileSlash => {
                if matches!(c, Some('/') | Some('\\')) {
                    state = FileHost;
                } else {
                    if let Some(base) = base.record().filter(|b| b.scheme == "file") {
                        url.host = base.host.clone();
                        let rest = if p >= 0 && p <= n { &chars[p as usize..] } else { &[][..] };
                        if !starts_with_windows_drive_letter(rest) {
                            if let RecPath::List(bl) = &base.path {
                                if let Some(first) = bl.first() {
                                    if is_normalized_windows_drive_letter(first) {
                                        url.list().push(first.clone());
                                    }
                                }
                            }
                        }
                    }
                    state = Path;
                    p -= 1;
                }
            }
            FileHost => {
                if matches!(c, None | Some('/') | Some('\\') | Some('?') | Some('#')) {
                    p -= 1;
                    if so.is_none() && is_windows_drive_letter(&buffer) {
                        state = Path;
                    } else if buffer.is_empty() {
                        url.host = Some(RecHost::Empty);
                        if so.is_some() {
                            return Ok(());
                        }
                        state = PathStart;
                    } else {
                        let mut host = host_parse(&buffer, !url.is_special())?;
                        if host == RecHost::Domain("localhost".to_owned()) {
                            host = RecHost::Empty;
                        }
                        url.host = Some(host);
                        if so.is_some() {
                            return Ok(());
                        }
                        buffer.clear();
                        state = PathStart;
                    }
                } else if let Some(ch) = c {
                    buffer.push(ch);
                }
            }
            PathStart => {
                if url.is_special() {
                    state = Path;
                    if !matches!(c, Some('/') | Some('\\')) {
                        p -= 1;
                    }
                } else if so.is_none() && c == Some('?') {
                    url.query = Some(String::new());
                    state = Query;
                } else if so.is_none() && c == Some('#') {
                    url.fragment = Some(String::new());
                    state = Fragment;
                } else if c.is_some() {
                    state = Path;
                    if c != Some('/') {
                        p -= 1;
                    }
                } else if so.is_some() && url.host.is_none() {
                    url.list().push(String::new());
                }
            }
            Path => {
                let slash = c == Some('/') || (url.is_special() && c == Some('\\'));
                if c.is_none() || slash || (so.is_none() && matches!(c, Some('?') | Some('#'))) {
                    if is_double_dot(&buffer) {
                        url.shorten_path();
                        if !slash {
                            url.list().push(String::new());
                        }
                    } else if is_single_dot(&buffer) && !slash {
                        url.list().push(String::new());
                    } else if !is_single_dot(&buffer) {
                        if url.scheme == "file"
                            && url.list().is_empty()
                            && is_windows_drive_letter(&buffer)
                        {
                            buffer.replace_range(1..2, ":");
                        }
                        url.list().push(core::mem::take(&mut buffer));
                    }
                    buffer.clear();
                    if c == Some('?') {
                        url.query = Some(String::new());
                        state = Query;
                    }
                    if c == Some('#') {
                        url.fragment = Some(String::new());
                        state = Fragment;
                    }
                } else if let Some(ch) = c {
                    utf8_percent_encode_char(ch, path_set, &mut buffer);
                }
            }
            OpaquePath => {
                let path = match &mut url.path {
                    RecPath::Opaque(s) => s,
                    RecPath::List(_) => unreachable!("opaque path state on a list path"),
                };
                match c {
                    Some('?') => {
                        url.query = Some(String::new());
                        state = Query;
                    }
                    Some('#') => {
                        url.fragment = Some(String::new());
                        state = Fragment;
                    }
                    Some(' ') => {
                        if matches!(remaining(p).first(), Some('?') | Some('#')) {
                            path.push_str("%20");
                        } else {
                            path.push(' ');
                        }
                    }
                    Some(ch) => utf8_percent_encode_char(ch, c0_control_set, path),
                    None => {}
                }
            }
            Query => {
                if encoding.is_some() && (!url.is_special() || matches!(&*url.scheme, "ws" | "wss"))
                {
                    encoding = None;
                }
                if (so.is_none() && c == Some('#')) || c.is_none() {
                    let set: fn(char) -> bool = if url.is_special() {
                        special_query_set
                    } else {
                        query_set
                    };
                    let query = url.query.get_or_insert_with(String::new);
                    match encoding {
                        None => utf8_percent_encode_str(&buffer, set, query),
                        Some(encode) => {
                            for &b in encode(&buffer).iter() {
                                if b >= 0x80 || set(b as char) {
                                    let _ = write!(query, "%{:02X}", b);
                                } else {
                                    query.push(b as char);
                                }
                            }
                        }
                    }
                    buffer.clear();
                    if c == Some('#') {
                        url.fragment = Some(String::new());
                        state = Fragment;
                    }
                } else if let Some(ch) = c {
                    buffer.push(ch);
                }
            }
            Fragment => {
                if let Some(ch) = c {
                    let fragment = url.fragment.get_or_insert_with(String::new);
                    utf8_percent_encode_char(ch, fragment_set, fragment);
                }
            }
        }
        if p >= n {
            break;
        }
        p += 1;
    }
    Ok(())
}
