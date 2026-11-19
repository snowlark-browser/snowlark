// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Import maps (HTML §8.1.5.3 "Import maps", §8.1.5.4 resolving module specifiers): parsing
//! `<script type=importmap>` text, merging several import maps into a global's, and the module
//! specifier resolution algorithm that `import`, `import()` and `import.meta.resolve()` use.
//!
//! The maps are kept sorted in descending code unit order (the parse steps sort them; merging keeps the
//! order), so the longest matching prefix is tried first.

use std::cmp::Ordering;

use url::Url;

/// A module specifier map: normalized specifier keys to addresses (`None`: blocked by a null or
/// invalid entry), sorted in descending code unit order of the keys.
pub type SpecifierMap = Vec<(String, Option<Url>)>;

/// An import map (HTML §8.1.5.3).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportMap {
    pub imports: SpecifierMap,
    /// Scope prefixes (serialized URLs) to specifier maps, sorted in descending code unit order.
    pub scopes: Vec<(String, SpecifierMap)>,
    /// Module integrity map: serialized URLs to integrity metadata.
    pub integrity: Vec<(String, String)>,
}

/// Why an import map's text is not an import map: the exception the parse throws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The text is not JSON (a `SyntaxError`).
    Syntax(String),
    /// The JSON is not an import map (a `TypeError`).
    Type(String),
}

/// A specifier resolution record (HTML §8.1.5.3): a resolution the global's later import maps must
/// not change.
#[derive(Clone, Debug)]
struct Record {
    serialized_base_url: String,
    specifier: String,
    /// Whether the specifier as a URL is null or special.
    url_null_or_special: bool,
}

/// A global's import map and resolved module set.
#[derive(Clone, Debug, Default)]
pub struct GlobalImports {
    pub map: ImportMap,
    resolved: Vec<Record>,
}

/// Code unit order (JavaScript string comparison), which differs from Rust's byte order for code
/// points above U+FFFF against U+E000-U+FFFF.
fn code_unit_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Sorts entries in descending code unit order of their keys.
fn sort_descending<T>(v: &mut [(String, T)]) {
    v.sort_by(|a, b| code_unit_cmp(&b.0, &a.0));
}

/// URL Standard: whether a URL's scheme is special.
fn is_special(url: &Url) -> bool {
    matches!(
        url.scheme(),
        "ftp" | "file" | "http" | "https" | "ws" | "wss"
    )
}

/// HTML "resolve a URL-like module specifier".
pub fn resolve_url_like(specifier: &str, base: &Url) -> Option<Url> {
    if specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../") {
        return base.join(specifier).ok();
    }
    Url::parse(specifier).ok()
}

/// A JSON value as "parse a JSON string to an Infra value" makes it: objects are ordered maps in
/// insertion order (a repeated key keeps its first position and takes the last value, as
/// `JSON.parse` does).
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// A JSON text parser (ECMA-404, as ECMAScript's JSON.parse reads it).
struct Json<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Json<'a> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn err<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("{what} at position {}", self.i))
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: u32) -> Result<Value, String> {
        if depth > 512 {
            return self.err("JSON nested too deeply");
        }
        self.ws();
        let Some(&c) = self.s.get(self.i) else {
            return self.err("Unexpected end of JSON input");
        };
        match c {
            b'{' => {
                self.i += 1;
                let mut members: Vec<(String, Value)> = Vec::new();
                self.ws();
                if self.eat("}") {
                    return Ok(Value::Object(members));
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&b'"') {
                        return self.err("Expected a property name");
                    }
                    let key = self.string()?;
                    self.ws();
                    if !self.eat(":") {
                        return self.err("Expected ':'");
                    }
                    let v = self.value(depth + 1)?;
                    match members.iter_mut().find(|(k, _)| *k == key) {
                        Some(m) => m.1 = v,
                        None => members.push((key, v)),
                    }
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("}") {
                        return Ok(Value::Object(members));
                    }
                    return self.err("Expected ',' or '}'");
                }
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.eat("]") {
                    return Ok(Value::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("]") {
                        return Ok(Value::Array(items));
                    }
                    return self.err("Expected ',' or ']'");
                }
            }
            b'"' => Ok(Value::String(self.string()?)),
            b't' if self.eat("true") => Ok(Value::Bool(true)),
            b'f' if self.eat("false") => Ok(Value::Bool(false)),
            b'n' if self.eat("null") => Ok(Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => self.err("Unexpected token"),
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        let digits = |j: &mut Json| {
            let from = j.i;
            while j.i < j.s.len() && j.s[j.i].is_ascii_digit() {
                j.i += 1;
            }
            j.i > from
        };
        self.eat("-");
        if self.eat("0") {
        } else if !digits(self) {
            return self.err("Bad number");
        }
        if self.eat(".") && !digits(self) {
            return self.err("Bad number");
        }
        if self.s.get(self.i).is_some_and(|c| matches!(c, b'e' | b'E')) {
            self.i += 1;
            if !self.eat("+") {
                self.eat("-");
            }
            if !digits(self) {
                return self.err("Bad number");
            }
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("0");
        Ok(Value::Number(text.parse().unwrap_or(0.0)))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let Some(h) = self.s.get(self.i..self.i + 4) else {
            return self.err("Bad unicode escape");
        };
        let h = std::str::from_utf8(h)
            .ok()
            .and_then(|h| u32::from_str_radix(h, 16).ok());
        match h {
            Some(v) => {
                self.i += 4;
                Ok(v)
            }
            None => self.err("Bad unicode escape"),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        // (at the opening quote)
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.s.get(self.i) else {
                return self.err("Unterminated string");
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let Some(&e) = self.s.get(self.i) else {
                        return self.err("Unterminated string");
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi)
                                && self.s[self.i..].starts_with(b"\\u")
                            {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    self.i = save;
                                    hi
                                }
                            } else {
                                hi
                            };
                            // (a lone surrogate, which a Rust string cannot hold, becomes U+FFFD)
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        _ => return self.err("Bad escape"),
                    }
                }
                0..=0x1f => return self.err("Bad control character in string"),
                _ => {
                    // (the input is UTF-8: copy the whole character)
                    let len = match c {
                        0..=0x7f => 1,
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    let end = (self.i + len).min(self.s.len());
                    out.push_str(std::str::from_utf8(&self.s[self.i..end]).unwrap_or("\u{FFFD}"));
                    self.i = end;
                }
            }
        }
    }
}

/// "Parse a JSON string to an Infra value".
fn parse_json(input: &str) -> Result<Value, String> {
    let mut p = Json {
        s: input.as_bytes(),
        i: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return p.err("Unexpected token after JSON");
    }
    Ok(v)
}

/// HTML "parse an import map string".
pub fn parse(
    input: &str,
    base: &Url,
    warn: &mut dyn FnMut(String),
) -> Result<ImportMap, ParseError> {
    let parsed = parse_json(input).map_err(|e| ParseError::Syntax(format!("Invalid JSON: {e}")))?;
    let Value::Object(top) = &parsed else {
        return Err(ParseError::Type(
            "An import map must be a JSON object".into(),
        ));
    };
    let mut map = ImportMap::default();
    if let Some(imports) = parsed.get("imports") {
        let Value::Object(imports) = imports else {
            return Err(ParseError::Type(
                "An import map's \"imports\" must be a JSON object".into(),
            ));
        };
        map.imports = sort_and_normalize_specifier_map(imports, base, warn);
    }
    if let Some(scopes) = parsed.get("scopes") {
        let Value::Object(scopes) = scopes else {
            return Err(ParseError::Type(
                "An import map's \"scopes\" must be a JSON object".into(),
            ));
        };
        for (prefix, potential) in scopes {
            let Value::Object(specifier_map) = potential else {
                return Err(ParseError::Type(format!(
                    "The value of the scope \"{prefix}\" must be a JSON object"
                )));
            };
            let Ok(prefix_url) = base.join(prefix) else {
                warn(format!("Ignored the scope \"{prefix}\": not a URL"));
                continue;
            };
            let normalized = sort_and_normalize_specifier_map(specifier_map, base, warn);
            let key = prefix_url.to_string();
            match map.scopes.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 = normalized,
                None => map.scopes.push((key, normalized)),
            }
        }
        sort_descending(&mut map.scopes);
    }
    if let Some(integrity) = parsed.get("integrity") {
        let Value::Object(integrity) = integrity else {
            return Err(ParseError::Type(
                "An import map's \"integrity\" must be a JSON object".into(),
            ));
        };
        for (key, value) in integrity {
            let Some(url) = resolve_url_like(key, base) else {
                warn(format!("Ignored the integrity entry \"{key}\": not a URL"));
                continue;
            };
            let Value::String(value) = value else {
                warn(format!(
                    "Ignored the integrity entry \"{key}\": its value is not a string"
                ));
                continue;
            };
            let key = url.to_string();
            match map.integrity.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 = value.clone(),
                None => map.integrity.push((key, value.clone())),
            }
        }
    }
    for (key, _) in top {
        if !matches!(key.as_str(), "imports" | "scopes" | "integrity") {
            warn(format!(
                "Ignored the unknown top-level key \"{key}\" of an import map"
            ));
        }
    }
    Ok(map)
}

/// HTML "sort and normalize a module specifier map".
fn sort_and_normalize_specifier_map(
    original: &[(String, Value)],
    base: &Url,
    warn: &mut dyn FnMut(String),
) -> SpecifierMap {
    let mut normalized: SpecifierMap = Vec::new();
    let set = |normalized: &mut SpecifierMap, key: String, value: Option<Url>| match normalized
        .iter_mut()
        .find(|(k, _)| *k == key)
    {
        Some(entry) => entry.1 = value,
        None => normalized.push((key, value)),
    };
    for (specifier_key, value) in original {
        // "normalize a specifier key"
        if specifier_key.is_empty() {
            warn("Ignored an empty specifier key in an import map".into());
            continue;
        }
        let key = match resolve_url_like(specifier_key, base) {
            Some(u) => u.to_string(),
            None => specifier_key.clone(),
        };
        let Value::String(value) = value else {
            warn(format!(
                "The address of \"{specifier_key}\" is not a string: the specifier is blocked"
            ));
            set(&mut normalized, key, None);
            continue;
        };
        let Some(address) = resolve_url_like(value, base) else {
            warn(format!(
                "The address \"{value}\" of \"{specifier_key}\" is not a URL: the specifier is blocked"
            ));
            set(&mut normalized, key, None);
            continue;
        };
        if specifier_key.ends_with('/') && !address.as_str().ends_with('/') {
            warn(format!(
                "The address \"{value}\" of the package prefix \"{specifier_key}\" must end with \"/\""
            ));
            set(&mut normalized, key, None);
            continue;
        }
        set(&mut normalized, key, Some(address));
    }
    sort_descending(&mut normalized);
    normalized
}

/// HTML "resolve an imports match": `Ok(None)` when no key matches.
fn resolve_imports_match(
    normalized_specifier: &str,
    as_url: Option<&Url>,
    map: &SpecifierMap,
) -> Result<Option<Url>, String> {
    for (key, result) in map {
        if key == normalized_specifier {
            return match result {
                Some(u) => Ok(Some(u.clone())),
                None => Err(format!(
                    "The resolution of \"{normalized_specifier}\" was blocked by a null entry"
                )),
            };
        }
        if key.ends_with('/')
            && normalized_specifier.starts_with(key.as_str())
            && as_url.is_none_or(is_special)
        {
            let Some(result) = result else {
                return Err(format!(
                    "The resolution of \"{normalized_specifier}\" was blocked by a null entry"
                ));
            };
            let after_prefix = &normalized_specifier[key.len()..];
            let Ok(url) = result.join(after_prefix) else {
                return Err(format!(
                    "The resolution of \"{normalized_specifier}\" was blocked: \"{after_prefix}\" is not a URL relative to \"{result}\""
                ));
            };
            if !url.as_str().starts_with(result.as_str()) {
                return Err(format!(
                    "The resolution of \"{normalized_specifier}\" was blocked: it backtracks above its prefix \"{key}\""
                ));
            }
            return Ok(Some(url));
        }
    }
    Ok(None)
}

/// "Merge module specifier maps": the old map's entries win.
fn merge_specifier_maps(
    new: SpecifierMap,
    old: &SpecifierMap,
    warn: &mut dyn FnMut(String),
) -> SpecifierMap {
    let mut merged = old.clone();
    for (specifier, url) in new {
        if old.iter().any(|(k, _)| *k == specifier) {
            warn(format!(
                "An import map rule for \"{specifier}\" was ignored: an earlier import map defines it"
            ));
            continue;
        }
        merged.push((specifier, url));
    }
    sort_descending(&mut merged);
    merged
}

impl GlobalImports {
    /// HTML "resolve a module specifier" for a referrer with base URL `base`, in a Window (whose import
    /// map applies; a worker's resolution uses an empty `GlobalImports`). Records the resolution in the
    /// resolved module set. `Err` is the TypeError's message.
    pub fn resolve(&mut self, specifier: &str, base: &Url) -> Result<Url, String> {
        let serialized_base_url = base.as_str();
        let as_url = resolve_url_like(specifier, base);
        let normalized = match &as_url {
            Some(u) => u.to_string(),
            None => specifier.to_owned(),
        };
        let mut result = None;
        for (prefix, scope_imports) in &self.map.scopes {
            if prefix == serialized_base_url
                || (prefix.ends_with('/') && serialized_base_url.starts_with(prefix.as_str()))
            {
                if let Some(u) = resolve_imports_match(&normalized, as_url.as_ref(), scope_imports)?
                {
                    result = Some(u);
                    break;
                }
            }
        }
        if result.is_none() {
            result = resolve_imports_match(&normalized, as_url.as_ref(), &self.map.imports)?;
        }
        let result = result.or_else(|| as_url.clone());
        match result {
            Some(u) => {
                // "add module to resolved module set"
                let url_null_or_special = as_url.as_ref().is_none_or(is_special);
                if !self.resolved.iter().any(|r| {
                    r.serialized_base_url == serialized_base_url && r.specifier == normalized
                }) {
                    self.resolved.push(Record {
                        serialized_base_url: serialized_base_url.to_owned(),
                        specifier: normalized,
                        url_null_or_special,
                    });
                }
                Ok(u)
            }
            None => Err(format!(
                "Failed to resolve module specifier \"{specifier}\". Relative references must start with either \"/\", \"./\", or \"../\"."
            )),
        }
    }

    /// HTML "resolve a module integrity metadata".
    pub fn integrity(&self, url: &str) -> Option<&str> {
        self.map
            .integrity
            .iter()
            .find(|(k, _)| k == url)
            .map(|(_, v)| v.as_str())
    }

    /// HTML "merge existing and new import maps".
    pub fn merge(&mut self, new: ImportMap, warn: &mut dyn FnMut(String)) {
        let ImportMap {
            imports: mut new_imports,
            scopes: new_scopes,
            integrity: new_integrity,
        } = new;
        for (scope_prefix, mut scope_imports) in new_scopes {
            for record in &self.resolved {
                if scope_prefix == record.serialized_base_url
                    || (scope_prefix.ends_with('/')
                        && record
                            .serialized_base_url
                            .starts_with(scope_prefix.as_str()))
                {
                    scope_imports.retain(|(key, _)| {
                        let conflicts = *key == record.specifier
                            || (key.ends_with('/')
                                && record.specifier.starts_with(key.as_str())
                                && record.url_null_or_special);
                        if conflicts {
                            warn(format!(
                                "An import map rule for \"{key}\" in the scope \"{scope_prefix}\" was ignored: a module was already resolved with it"
                            ));
                        }
                        !conflicts
                    });
                }
            }
            match self.map.scopes.iter_mut().find(|(k, _)| *k == scope_prefix) {
                Some(entry) => {
                    let merged = merge_specifier_maps(scope_imports, &entry.1, warn);
                    entry.1 = merged;
                }
                None => self.map.scopes.push((scope_prefix, scope_imports)),
            }
        }
        sort_descending(&mut self.map.scopes);
        for (url, integrity) in new_integrity {
            if self.map.integrity.iter().any(|(k, _)| *k == url) {
                warn(format!(
                    "An import map's integrity for \"{url}\" was ignored: an earlier import map defines it"
                ));
                continue;
            }
            self.map.integrity.push((url, integrity));
        }
        for record in &self.resolved {
            new_imports.retain(|(specifier, _)| {
                let conflicts = *specifier == record.specifier
                    || (specifier.ends_with('/')
                        && record.specifier.starts_with(specifier.as_str())
                        && record.url_null_or_special);
                if conflicts {
                    warn(format!(
                        "An import map rule for \"{specifier}\" was ignored: a module was already resolved with it"
                    ));
                }
                !conflicts
            });
        }
        self.map.imports = merge_specifier_maps(new_imports, &self.map.imports, warn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/app/index.html").unwrap()
    }

    fn parse_ok(s: &str) -> ImportMap {
        parse(s, &base(), &mut |_| {}).unwrap()
    }

    #[test]
    fn parse_errors() {
        assert!(matches!(
            parse("{", &base(), &mut |_| {}),
            Err(ParseError::Syntax(_))
        ));
        assert!(matches!(
            parse("[]", &base(), &mut |_| {}),
            Err(ParseError::Type(_))
        ));
        assert!(matches!(
            parse(r#"{"imports": []}"#, &base(), &mut |_| {}),
            Err(ParseError::Type(_))
        ));
        assert!(matches!(
            parse(r#"{"scopes": {"/a/": 1}}"#, &base(), &mut |_| {}),
            Err(ParseError::Type(_))
        ));
    }

    #[test]
    fn normalizes_and_sorts() {
        let m = parse_ok(
            r#"{"imports": {"a": "./a.js", "a/": "/lib/a/", "./x": "./y", "": "/z", "bad/": "/nope", "n": null}}"#,
        );
        let keys: Vec<&str> = m.imports.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["n", "https://example.com/app/x", "bad/", "a/", "a"]);
        assert_eq!(
            m.imports[4].1.as_ref().unwrap().as_str(),
            "https://example.com/app/a.js"
        );
        // A package prefix whose address lacks the trailing slash is blocked.
        assert_eq!(m.imports[2].1, None);
        assert_eq!(m.imports[0].1, None);
    }

    #[test]
    fn resolves_bare_prefix_and_scope() {
        let mut g = GlobalImports::default();
        g.merge(
            parse_ok(
                r#"{"imports": {"lodash": "/node_modules/lodash/index.js", "lodash/": "/node_modules/lodash/"},
                    "scopes": {"/app/": {"lodash": "/scoped.js"}}}"#,
            ),
            &mut |_| {},
        );
        let other = Url::parse("https://example.com/other/m.js").unwrap();
        assert_eq!(
            g.resolve("lodash", &other).unwrap().as_str(),
            "https://example.com/node_modules/lodash/index.js"
        );
        assert_eq!(
            g.resolve("lodash/fp.js", &other).unwrap().as_str(),
            "https://example.com/node_modules/lodash/fp.js"
        );
        assert_eq!(
            g.resolve("lodash", &base()).unwrap().as_str(),
            "https://example.com/scoped.js"
        );
        assert!(g.resolve("react", &other).is_err());
        assert!(g.resolve("lodash/../../etc", &other).is_err());
        assert_eq!(
            g.resolve("./x.js", &other).unwrap().as_str(),
            "https://example.com/other/x.js"
        );
    }

    #[test]
    fn merging_keeps_first_and_drops_resolved() {
        let mut g = GlobalImports::default();
        let page = base();
        assert!(g.resolve("a", &page).is_err());
        g.resolve("./b.js", &page).unwrap();
        g.merge(parse_ok(r#"{"imports": {"x": "/x1.js"}}"#), &mut |_| {});
        let mut warnings = Vec::new();
        g.merge(
            parse_ok(
                r#"{"imports": {"x": "/x2.js", "y": "/y.js", "https://example.com/app/b.js": "/c.js"}}"#,
            ),
            &mut |w| warnings.push(w),
        );
        assert_eq!(
            g.resolve("x", &page).unwrap().as_str(),
            "https://example.com/x1.js"
        );
        assert_eq!(
            g.resolve("y", &page).unwrap().as_str(),
            "https://example.com/y.js"
        );
        // Already resolved: the new rule is dropped.
        assert_eq!(
            g.resolve("./b.js", &page).unwrap().as_str(),
            "https://example.com/app/b.js"
        );
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn scopes_stay_sorted_across_maps() {
        let mut g = GlobalImports::default();
        g.merge(
            parse_ok(r#"{"scopes": {"/app/": {"bar": "/specific.js"}}}"#),
            &mut |_| {},
        );
        g.merge(
            parse_ok(r#"{"scopes": {"/": {"bar": "/general.js"}}}"#),
            &mut |_| {},
        );
        assert_eq!(
            g.resolve("bar", &base()).unwrap().as_str(),
            "https://example.com/specific.js"
        );
    }

    #[test]
    fn json_order_and_duplicates() {
        // The last of two keys normalizing to the same URL wins (insertion order, not sorted keys).
        let m = parse_ok(r#"{"imports": {"/test": "/one.js", "../test": "/two.js"}}"#);
        assert_eq!(
            m.imports[0].1.as_ref().unwrap().as_str(),
            "https://example.com/two.js"
        );
        assert_eq!(
            parse_json(r#"{"a": 1, "b": [true, null, "x\u00e9\ud83d\ude00"], "a": 2}"#).unwrap(),
            Value::Object(vec![
                ("a".into(), Value::Number(2.0)),
                (
                    "b".into(),
                    Value::Array(vec![
                        Value::Bool(true),
                        Value::Null,
                        Value::String("x\u{e9}\u{1F600}".into())
                    ])
                ),
            ])
        );
        assert!(parse_json("{} x").is_err());
        assert!(parse_json("{'a': 1}").is_err());
        assert!(parse_json("[1,]").is_err());
    }

    #[test]
    fn prefix_rules_do_not_override_resolved() {
        let mut g = GlobalImports::default();
        let page = base();
        g.resolve("../resources/log.js?name=A", &page).unwrap();
        g.merge(
            parse_ok(r#"{"imports": {"../resources/": "/foobar/", "https:/": "/scheme.js"}}"#),
            &mut |_| {},
        );
        assert_eq!(
            g.resolve("../resources/log.js?name=A", &page)
                .unwrap()
                .as_str(),
            "https://example.com/resources/log.js?name=A"
        );
    }

    #[test]
    fn integrity_entries() {
        let mut g = GlobalImports::default();
        g.merge(
            parse_ok(r#"{"integrity": {"./a.js": "sha384-abc", "/b.js": 5}}"#),
            &mut |_| {},
        );
        assert_eq!(
            g.integrity("https://example.com/app/a.js"),
            Some("sha384-abc")
        );
        assert_eq!(g.integrity("https://example.com/b.js"), None);
    }
}
