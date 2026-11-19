// Copyright 2016 The rust-url developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Getters and setters for URL components implemented per <https://url.spec.whatwg.org/#api>
//!
//! Unless you need to be interoperable with web browsers,
//! you probably want to use `Url` method instead.

use crate::whatwg::{self, RecPath, Record, State};
use crate::{Host, ParseError, Position, Url};
use alloc::string::String;
use alloc::vec::Vec;

/// Internal components / offsets of a URL.
///
/// https://user@pass:example.com:1234/foo/bar?baz#quux
///      |      |    |          | ^^^^|       |   |
///      |      |    |          | |   |       |   `----- fragment_start
///      |      |    |          | |   |       `--------- query_start
///      |      |    |          | |   `----------------- path_start
///      |      |    |          | `--------------------- port
///      |      |    |          `----------------------- host_end
///      |      |    `---------------------------------- host_start
///      |      `--------------------------------------- username_end
///      `---------------------------------------------- scheme_end
#[derive(Copy, Clone)]
#[cfg(feature = "expose_internals")]
pub struct InternalComponents {
    pub scheme_end: u32,
    pub username_end: u32,
    pub host_start: u32,
    pub host_end: u32,
    pub port: Option<u16>,
    pub path_start: u32,
    pub query_start: Option<u32>,
    pub fragment_start: Option<u32>,
}

/// Internal component / parsed offsets of the URL.
///
/// This can be useful for implementing efficient serialization
/// for the URL.
#[cfg(feature = "expose_internals")]
pub fn internal_components(url: &Url) -> InternalComponents {
    InternalComponents {
        scheme_end: url.scheme_end,
        username_end: url.username_end,
        host_start: url.host_start,
        host_end: url.host_end,
        port: url.port,
        path_start: url.path_start,
        query_start: url.query_start,
        fragment_start: url.fragment_start,
    }
}

/// <https://url.spec.whatwg.org/#dom-url-domaintoascii>
pub fn domain_to_ascii(domain: &str) -> String {
    match Host::parse(domain) {
        Ok(Host::Domain(domain)) => domain,
        _ => String::new(),
    }
}

/// <https://url.spec.whatwg.org/#dom-url-domaintounicode>
pub fn domain_to_unicode(domain: &str) -> String {
    match Host::parse(domain) {
        Ok(Host::Domain(ref domain)) => {
            let (unicode, _errors) = idna::domain_to_unicode(domain);
            unicode
        }
        _ => String::new(),
    }
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-href>
pub fn href(url: &Url) -> &str {
    url.as_str()
}

/// Setter for <https://url.spec.whatwg.org/#dom-url-href>
pub fn set_href(url: &mut Url, value: &str) -> Result<(), ParseError> {
    *url = Url::parse(value)?;
    Ok(())
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-origin>
pub fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-protocol>
#[inline]
pub fn protocol(url: &Url) -> &str {
    &url.as_str()[..url.scheme().len() + ":".len()]
}

/// Runs `f` on the URL record of `url` and stores the result back (the API setters modify the URL
/// in place, URL Standard §6.1).
fn with_record<R>(url: &mut Url, f: impl FnOnce(&mut Record) -> R) -> R {
    let mut record = Record::from_url(url);
    let result = f(&mut record);
    *url = record.to_url();
    result
}

/// Setter for <https://url.spec.whatwg.org/#dom-url-protocol>
#[allow(clippy::result_unit_err)]
pub fn set_protocol(url: &mut Url, new_protocol: &str) -> Result<(), ()> {
    let mut input = String::with_capacity(new_protocol.len() + 1);
    input.push_str(new_protocol);
    input.push(':');
    with_record(url, |r| whatwg::parse_with_override(&input, r, State::SchemeStart)).map_err(|_| ())
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-username>
#[inline]
pub fn username(url: &Url) -> &str {
    url.username()
}

/// Setter for <https://url.spec.whatwg.org/#dom-url-username>
#[allow(clippy::result_unit_err)]
pub fn set_username(url: &mut Url, new_username: &str) -> Result<(), ()> {
    with_record(url, |r| {
        if r.cannot_have_username_password_port() {
            return Err(());
        }
        r.username.clear();
        whatwg::utf8_percent_encode_str(new_username, whatwg::userinfo_set, &mut r.username);
        Ok(())
    })
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-password>
#[inline]
pub fn password(url: &Url) -> &str {
    url.password().unwrap_or("")
}

/// Setter for <https://url.spec.whatwg.org/#dom-url-password>
#[allow(clippy::result_unit_err)]
pub fn set_password(url: &mut Url, new_password: &str) -> Result<(), ()> {
    with_record(url, |r| {
        if r.cannot_have_username_password_port() {
            return Err(());
        }
        r.password.clear();
        whatwg::utf8_percent_encode_str(new_password, whatwg::userinfo_set, &mut r.password);
        Ok(())
    })
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-host>
#[inline]
pub fn host(url: &Url) -> &str {
    &url[Position::BeforeHost..Position::AfterPort]
}

/// Setter for <https://url.spec.whatwg.org/#dom-url-host>
#[allow(clippy::result_unit_err)]
pub fn set_host(url: &mut Url, new_host: &str) -> Result<(), ()> {
    with_record(url, |r| {
        if r.has_opaque_path() {
            return Err(());
        }
        whatwg::parse_with_override(new_host, r, State::Host).map_err(|_| ())
    })
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-hostname>
#[inline]
pub fn hostname(url: &Url) -> &str {
    url.host_str().unwrap_or("")
}

/// Setter for <https://url.spec.whatwg.org/#dom-url-hostname>
#[allow(clippy::result_unit_err)]
pub fn set_hostname(url: &mut Url, new_hostname: &str) -> Result<(), ()> {
    with_record(url, |r| {
        if r.has_opaque_path() {
            return Err(());
        }
        whatwg::parse_with_override(new_hostname, r, State::Hostname).map_err(|_| ())
    })
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-port>
#[inline]
pub fn port(url: &Url) -> &str {
    &url[Position::BeforePort..Position::AfterPort]
}

/// Setter for <https://url.spec.whatwg.org/#dom-url-port>
#[allow(clippy::result_unit_err)]
pub fn set_port(url: &mut Url, new_port: &str) -> Result<(), ()> {
    with_record(url, |r| {
        if r.cannot_have_username_password_port() {
            return Err(());
        }
        if new_port.is_empty() {
            r.port = None;
            return Ok(());
        }
        whatwg::parse_with_override(new_port, r, State::Port).map_err(|_| ())
    })
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-pathname>
#[inline]
pub fn pathname(url: &Url) -> &str {
    url.path()
}

/// Setter for <https://url.spec.whatwg.org/#dom-url-pathname>
pub fn set_pathname(url: &mut Url, new_pathname: &str) {
    if url.cannot_be_a_base() {
        return;
    }
    with_record(url, |r| {
        r.path = RecPath::List(Vec::new());
        let _ = whatwg::parse_with_override(new_pathname, r, State::PathStart);
    })
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-search>
pub fn search(url: &Url) -> &str {
    trim(&url[Position::AfterPath..Position::AfterQuery])
}

/// Setter for <https://url.spec.whatwg.org/#dom-url-search>
pub fn set_search(url: &mut Url, new_search: &str) {
    with_record(url, |r| {
        if new_search.is_empty() {
            r.query = None;
            return;
        }
        let input = new_search.strip_prefix('?').unwrap_or(new_search);
        r.query = Some(String::new());
        let _ = whatwg::parse_with_override(input, r, State::Query);
    })
}

/// Getter for <https://url.spec.whatwg.org/#dom-url-hash>
pub fn hash(url: &Url) -> &str {
    trim(&url[Position::AfterQuery..])
}

/// Setter for <https://url.spec.whatwg.org/#dom-url-hash>
pub fn set_hash(url: &mut Url, new_hash: &str) {
    with_record(url, |r| {
        if new_hash.is_empty() {
            r.fragment = None;
            return;
        }
        let input = new_hash.strip_prefix('#').unwrap_or(new_hash);
        r.fragment = Some(String::new());
        let _ = whatwg::parse_with_override(input, r, State::Fragment);
    })
}

fn trim(s: &str) -> &str {
    if s.len() == 1 {
        ""
    } else {
        s
    }
}
