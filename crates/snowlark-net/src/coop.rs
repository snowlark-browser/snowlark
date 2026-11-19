// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §7.1.3 cross-origin opener policies: the `Cross-Origin-Opener-Policy` and
//! `Cross-Origin-Opener-Policy-Report-Only` headers ("obtain an opener policy"), and the decision a
//! navigation's response makes with them ("checking if COOP values require a browsing context
//! group switch"), which the engine acts on: the new document's browsing context is one of a new
//! group, with no opener.

use crate::HeaderList;

/// HTML "opener policy value".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OpenerPolicyValue {
    #[default]
    UnsafeNone,
    SameOriginAllowPopups,
    SameOrigin,
    /// `same-origin` with an embedder policy compatible with cross-origin isolation.
    SameOriginPlusCoep,
    NoopenerAllowPopups,
}

impl OpenerPolicyValue {
    /// The header token that names the value.
    pub fn as_str(self) -> &'static str {
        match self {
            OpenerPolicyValue::UnsafeNone => "unsafe-none",
            OpenerPolicyValue::SameOriginAllowPopups => "same-origin-allow-popups",
            OpenerPolicyValue::SameOrigin => "same-origin",
            OpenerPolicyValue::SameOriginPlusCoep => "same-origin-plus-coep",
            OpenerPolicyValue::NoopenerAllowPopups => "noopener-allow-popups",
        }
    }
}

/// HTML "opener policy".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenerPolicy {
    pub value: OpenerPolicyValue,
    pub reporting_endpoint: Option<String>,
    pub report_only_value: OpenerPolicyValue,
    pub report_only_reporting_endpoint: Option<String>,
}

/// HTML "obtain an opener policy" for a response whose reserved environment is a secure context
/// (`secure`) or not. Without an embedder policy, `same-origin` stays `same-origin`.
pub fn obtain(headers: &HeaderList, secure: bool) -> OpenerPolicy {
    let mut policy = OpenerPolicy::default();
    if !secure {
        return policy;
    }
    if let Some((token, report_to)) = header_item(headers, "cross-origin-opener-policy") {
        let value = match token.as_str() {
            "same-origin" => Some(OpenerPolicyValue::SameOrigin),
            "same-origin-allow-popups" => Some(OpenerPolicyValue::SameOriginAllowPopups),
            "noopener-allow-popups" => Some(OpenerPolicyValue::NoopenerAllowPopups),
            "unsafe-none" => Some(OpenerPolicyValue::UnsafeNone),
            _ => None,
        };
        if let Some(v) = value {
            policy.value = v;
            policy.reporting_endpoint = report_to;
        }
    }
    if let Some((token, report_to)) = header_item(headers, "cross-origin-opener-policy-report-only")
    {
        let value = match token.as_str() {
            "same-origin" => Some(OpenerPolicyValue::SameOrigin),
            "same-origin-allow-popups" => Some(OpenerPolicyValue::SameOriginAllowPopups),
            "unsafe-none" => Some(OpenerPolicyValue::UnsafeNone),
            _ => None,
        };
        if let Some(v) = value {
            policy.report_only_value = v;
            policy.report_only_reporting_endpoint = report_to;
        }
    }
    policy
}

/// The header's value parsed as a structured field item (RFC 8941 §4.2) whose bare item is a token:
/// the token and its `report-to` parameter when that is a string. `None` when the header is missing,
/// does not parse as an item (a repeated header combines into a list), or its item is not a token.
fn header_item(headers: &HeaderList, name: &str) -> Option<(String, Option<String>)> {
    let value = headers.get(name)?;
    let mut p = Parser {
        s: value.as_bytes(),
        i: 0,
    };
    p.skip_sp();
    let token = p.token()?;
    let mut report_to = None;
    // (parameters: `;` key [`=` bare item])
    loop {
        if p.peek() != Some(b';') {
            break;
        }
        p.i += 1;
        p.skip_sp();
        let key = p.key()?;
        let value = if p.peek() == Some(b'=') {
            p.i += 1;
            Some(p.bare_item()?)
        } else {
            None
        };
        if key == "report-to" {
            report_to = match value {
                Some(BareItem::String(s)) => Some(s),
                _ => None,
            };
        }
    }
    p.skip_sp();
    (p.i == p.s.len()).then_some((token, report_to))
}

enum BareItem {
    String(String),
    Other,
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn skip_sp(&mut self) {
        while self.peek() == Some(b' ') {
            self.i += 1;
        }
    }

    /// RFC 8941 §4.2.6 "parsing a token".
    fn token(&mut self) -> Option<String> {
        let start = self.i;
        match self.peek() {
            Some(c) if c.is_ascii_alphabetic() || c == b'*' => self.i += 1,
            _ => return None,
        }
        while let Some(c) = self.peek() {
            let tchar = c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~:/".contains(&c);
            if !tchar {
                break;
            }
            self.i += 1;
        }
        Some(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }

    /// RFC 8941 §4.2.3.3 "parsing a key".
    fn key(&mut self) -> Option<String> {
        let start = self.i;
        match self.peek() {
            Some(c) if c.is_ascii_lowercase() || c == b'*' => self.i += 1,
            _ => return None,
        }
        while let Some(c) = self.peek() {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-.*".contains(&c)) {
                break;
            }
            self.i += 1;
        }
        Some(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }

    /// RFC 8941 §4.2.3.1 "parsing a bare item": strings are kept, the other kinds checked only.
    fn bare_item(&mut self) -> Option<BareItem> {
        match self.peek()? {
            b'"' => {
                self.i += 1;
                let mut out = String::new();
                loop {
                    let c = self.peek()?;
                    self.i += 1;
                    match c {
                        b'\\' => {
                            let n = self.peek()?;
                            if n != b'"' && n != b'\\' {
                                return None;
                            }
                            self.i += 1;
                            out.push(n as char);
                        }
                        b'"' => return Some(BareItem::String(out)),
                        0x20..=0x7e => out.push(c as char),
                        _ => return None,
                    }
                }
            }
            b'?' => {
                self.i += 1;
                match self.peek()? {
                    b'0' | b'1' => {
                        self.i += 1;
                        Some(BareItem::Other)
                    }
                    _ => None,
                }
            }
            b':' => {
                self.i += 1;
                while let Some(c) = self.peek() {
                    self.i += 1;
                    if c == b':' {
                        return Some(BareItem::Other);
                    }
                    if !(c.is_ascii_alphanumeric() || b"+/=".contains(&c)) {
                        return None;
                    }
                }
                None
            }
            c if c == b'-' || c.is_ascii_digit() => {
                self.i += 1;
                while self.peek().is_some_and(|c| c.is_ascii_digit() || c == b'.') {
                    self.i += 1;
                }
                Some(BareItem::Other)
            }
            _ => self.token().map(|_| BareItem::Other),
        }
    }
}

/// HTML "match opener policy values".
pub fn values_match<O: PartialEq>(
    document_coop: OpenerPolicyValue,
    document_origin: &O,
    response_coop: OpenerPolicyValue,
    response_origin: &O,
) -> bool {
    use OpenerPolicyValue::UnsafeNone;
    if document_coop == UnsafeNone && response_coop == UnsafeNone {
        return true;
    }
    if document_coop == UnsafeNone || response_coop == UnsafeNone {
        return false;
    }
    document_coop == response_coop && document_origin == response_origin
}

/// HTML "checking if popup COOP values require a browsing context group switch": the response
/// replaces an initial `about:blank` document.
pub fn popup_values_require_switch<O: PartialEq>(
    response_origin: &O,
    active_origin: &O,
    response_coop: OpenerPolicyValue,
    active_coop: OpenerPolicyValue,
) -> bool {
    use OpenerPolicyValue::*;
    if response_coop == NoopenerAllowPopups {
        return true;
    }
    if matches!(active_coop, SameOriginAllowPopups | NoopenerAllowPopups)
        && response_coop == UnsafeNone
    {
        return false;
    }
    !values_match(active_coop, active_origin, response_coop, response_origin)
}

/// HTML "checking if COOP values require a browsing context group switch".
pub fn values_require_switch<O: PartialEq>(
    is_initial_about_blank: bool,
    response_origin: &O,
    active_origin: &O,
    response_coop: OpenerPolicyValue,
    active_coop: OpenerPolicyValue,
) -> bool {
    if is_initial_about_blank {
        return popup_values_require_switch(
            response_origin,
            active_origin,
            response_coop,
            active_coop,
        );
    }
    !values_match(active_coop, active_origin, response_coop, response_origin)
}

/// HTML "checking if enforcing report-only COOP would require a browsing context group switch".
pub fn report_only_requires_switch<O: PartialEq>(
    is_initial_about_blank: bool,
    response_origin: &O,
    active_origin: &O,
    response: &OpenerPolicy,
    active: &OpenerPolicy,
) -> bool {
    let check =
        |r, a| values_require_switch(is_initial_about_blank, response_origin, active_origin, r, a);
    if !check(response.report_only_value, active.report_only_value) {
        return false;
    }
    check(response.value, active.report_only_value)
        || check(response.report_only_value, active.value)
}

#[cfg(test)]
mod tests {
    use super::OpenerPolicyValue::*;
    use super::*;

    fn coop(value: &str) -> OpenerPolicy {
        let mut h = HeaderList::new();
        h.append("Cross-Origin-Opener-Policy", value);
        obtain(&h, true)
    }

    #[test]
    fn the_header_parses_as_a_structured_token() {
        assert_eq!(coop("same-origin").value, SameOrigin);
        assert_eq!(
            coop("  same-origin-allow-popups ").value,
            SameOriginAllowPopups
        );
        assert_eq!(coop("noopener-allow-popups").value, NoopenerAllowPopups);
        assert_eq!(
            coop("same-origin;report-to=\"ep\"")
                .reporting_endpoint
                .as_deref(),
            Some("ep")
        );
        assert_eq!(coop("same-origin; foo=bar").value, SameOrigin);
        // (not a token, an unknown or obsolete one, a list, a bad parameter)
        for bad in [
            "\"same-origin\"",
            "jibberish",
            "same-site",
            "same-origin unsafe-allow-outgoing",
            "same-origin, unsafe-none",
            "same-origin;",
            "SAME-ORIGIN",
            "same-origin;report-to=\"x",
        ] {
            assert_eq!(coop(bad).value, UnsafeNone, "{bad}");
        }
        let mut h = HeaderList::new();
        h.append("Cross-Origin-Opener-Policy", "same-origin");
        h.append("Cross-Origin-Opener-Policy", "same-origin");
        assert_eq!(obtain(&h, true).value, UnsafeNone, "a repeated header");
        assert_eq!(obtain(&h, false).value, UnsafeNone);
        let mut h = HeaderList::new();
        h.append("Cross-Origin-Opener-Policy", "same-origin");
        assert_eq!(
            obtain(&h, false).value,
            UnsafeNone,
            "a non-secure context has none"
        );
    }

    #[test]
    fn report_only_takes_no_noopener_allow_popups() {
        let mut h = HeaderList::new();
        h.append(
            "Cross-Origin-Opener-Policy-Report-Only",
            "same-origin; report-to=\"r\"",
        );
        let p = obtain(&h, true);
        assert_eq!((p.value, p.report_only_value), (UnsafeNone, SameOrigin));
        assert_eq!(p.report_only_reporting_endpoint.as_deref(), Some("r"));
        let mut h = HeaderList::new();
        h.append(
            "Cross-Origin-Opener-Policy-Report-Only",
            "noopener-allow-popups",
        );
        assert_eq!(obtain(&h, true).report_only_value, UnsafeNone);
    }

    #[test]
    fn a_switch_follows_the_spec_tables() {
        let (a, b) = ("https://a.test", "https://b.test");
        // (a document's own navigation)
        assert!(!values_require_switch(
            false, &a, &a, UnsafeNone, UnsafeNone
        ));
        assert!(values_require_switch(false, &a, &a, SameOrigin, UnsafeNone));
        assert!(!values_require_switch(
            false, &a, &a, SameOrigin, SameOrigin
        ));
        assert!(values_require_switch(false, &b, &a, SameOrigin, SameOrigin));
        assert!(values_require_switch(
            false,
            &a,
            &a,
            SameOrigin,
            SameOriginAllowPopups
        ));
        assert!(values_require_switch(
            false,
            &a,
            &a,
            UnsafeNone,
            SameOriginAllowPopups
        ));
        // (a popup's initial about:blank, which took its opener's policy)
        assert!(!values_require_switch(
            true,
            &b,
            &a,
            UnsafeNone,
            SameOriginAllowPopups
        ));
        assert!(!values_require_switch(
            true,
            &b,
            &a,
            UnsafeNone,
            NoopenerAllowPopups
        ));
        assert!(values_require_switch(true, &b, &a, UnsafeNone, SameOrigin));
        assert!(values_require_switch(
            true,
            &a,
            &a,
            NoopenerAllowPopups,
            NoopenerAllowPopups
        ));
        assert!(values_require_switch(
            true,
            &a,
            &a,
            NoopenerAllowPopups,
            UnsafeNone
        ));
        assert!(!values_require_switch(
            true,
            &a,
            &a,
            SameOriginAllowPopups,
            SameOriginAllowPopups
        ));
        assert!(values_require_switch(
            true,
            &b,
            &a,
            SameOriginAllowPopups,
            SameOriginAllowPopups
        ));
    }

    #[test]
    fn report_only_switches_when_either_side_would() {
        let a = "https://a.test";
        let none = OpenerPolicy::default();
        let ro = OpenerPolicy {
            report_only_value: SameOrigin,
            ..Default::default()
        };
        assert!(report_only_requires_switch(false, &a, &a, &ro, &none));
        assert!(!report_only_requires_switch(false, &a, &a, &none, &none));
    }
}
