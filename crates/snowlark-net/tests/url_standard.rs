// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The URL Standard's basic URL parser and API setters (vendor/url, src/whatwg.rs), on cases from
//! WPT url/resources/urltestdata.json and setters_tests.json.

use url::{quirks, Url};

fn parse(input: &str, base: Option<&str>) -> Option<String> {
    let base = base.map(|b| Url::parse(b).expect("valid base"));
    Url::options()
        .base_url(base.as_ref())
        .parse(input)
        .ok()
        .map(|u| {
            u.check_invariants().expect("the record's offsets");
            u.as_str().to_owned()
        })
}

#[track_caller]
fn check(input: &str, base: Option<&str>, expected: Option<&str>) {
    assert_eq!(
        parse(input, base).as_deref(),
        expected,
        "{input:?} against {base:?}"
    );
}

#[test]
fn ascii_domains_are_lowercased_not_validated() {
    // URL Standard "domain parser": with beStrict false an ASCII domain is only lowercased.
    check(
        "http://a.b.c.XN--pokxncvks",
        None,
        Some("http://a.b.c.xn--pokxncvks/"),
    );
    check("https://xn--/", None, Some("https://xn--/"));
    check("https://xn--ab-j1t/x", None, Some("https://xn--ab-j1t/x"));
    check("https://a%25b/", None, None);
    check("https://EXAMPLE.com/", None, Some("https://example.com/"));
    check(
        "https://☕.example/",
        None,
        Some("https://xn--53h.example/"),
    );
}

#[test]
fn file_urls() {
    check("file:///w|/m", None, Some("file:///w:/m"));
    check("/", Some("file://h/C:/a/b"), Some("file://h/C:/"));
    check("file:\\\\//", None, Some("file:////"));
    check("file://spider///", None, Some("file://spider///"));
    check("file://localhost//a//../..//", None, Some("file://///"));
    check("C|", Some("file://host/dir/file"), Some("file://host/C:"));
    check(
        "file://example.net/C:/",
        None,
        Some("file://example.net/C:/"),
    );
    check("file:////foo", None, Some("file:////foo"));
    check("file:.//p", None, Some("file:////p"));
    check("..", Some("file:///C:/"), Some("file:///C:/"));
    check("file://localhost/", None, Some("file:///"));
}

#[test]
fn opaque_paths_and_non_special_urls() {
    check(
        "non-special:opaque  ?hi",
        None,
        Some("non-special:opaque %20?hi"),
    );
    check("..", Some("abc://x/y/z/C:/"), Some("abc://x/y/z/"));
    check("\\a", Some("foo://foo/a"), Some("foo://foo/\\a"));
    check(
        "web+demo:/.//not-a-host/",
        None,
        Some("web+demo:/.//not-a-host/"),
    );
    check("sc:", None, Some("sc:"));
    check("sc://h", None, Some("sc://h"));
    check("#x", Some("sc:opaque"), Some("sc:opaque#x"));
    check("x", Some("sc:opaque"), None);
}

#[test]
fn special_urls() {
    check("///test", Some("http://example.org/"), Some("http://test/"));
    check(
        "///example.org/../path",
        Some("http://example.org/"),
        Some("http://example.org/path"),
    );
    check("wss://host/ ^`{}", None, Some("wss://host/%20%5E%60%7B%7D"));
    check(
        "http://user:pa:ss@h:80/",
        None,
        Some("http://user:pa%3Ass@h/"),
    );
    check("http://h:65536/", None, None);
    check(
        "http://[::1]:8080/a?b#c",
        None,
        Some("http://[::1]:8080/a?b#c"),
    );
    check("http://0x7f.1/", None, Some("http://127.0.0.1/"));
    check("  http://h/\t\na  ", None, Some("http://h/a"));
    check("?q=1", Some("http://h/p?x"), Some("http://h/p?q=1"));
    check("http://h/?'", None, Some("http://h/?%27"));
    check("sc://h/?'", None, Some("sc://h/?'"));
}

#[test]
fn blob_url_origin() {
    let origin = |s: &str| quirks::origin(&Url::parse(s).unwrap());
    assert_eq!(origin("blob:https://example.org/x"), "https://example.org");
    assert_eq!(origin("blob:ws://example.org/"), "null");
    assert_eq!(origin("blob:blob:https://example.org/"), "null");
}

#[test]
fn api_setters() {
    let set = |href: &str, f: fn(&mut Url, &str), v: &str| {
        let mut u = Url::parse(href).unwrap();
        f(&mut u, v);
        u.check_invariants().expect("the record's offsets");
        u.as_str().to_owned()
    };
    let host = |u: &mut Url, v: &str| {
        let _ = quirks::set_host(u, v);
    };
    let hostname = |u: &mut Url, v: &str| {
        let _ = quirks::set_hostname(u, v);
    };
    let port = |u: &mut Url, v: &str| {
        let _ = quirks::set_port(u, v);
    };
    let protocol = |u: &mut Url, v: &str| {
        let _ = quirks::set_protocol(u, v);
    };
    assert_eq!(set("https://example.com/", host, "xn--"), "https://xn--/");
    assert_eq!(set("non-spec:/.//p", hostname, "h"), "non-spec://h//p");
    assert_eq!(set("non-spec:/.//p", hostname, ""), "non-spec:////p");
    assert_eq!(
        set("https://domain.com:3000", port, "\n\n\t\t"),
        "https://domain.com:3000/"
    );
    assert_eq!(set("http://h:1/", port, "8080stuff"), "http://h:8080/");
    assert_eq!(set("http://h/", port, "80"), "http://h/");
    assert_eq!(set("foo:///some/path", quirks::set_pathname, ""), "foo://");
    assert_eq!(
        set("non-spec:/", quirks::set_pathname, "//p"),
        "non-spec:/.//p"
    );
    assert_eq!(
        set("non-spec:/.//", quirks::set_pathname, "p"),
        "non-spec:/p"
    );
    assert_eq!(
        set("file://monkey/", quirks::set_pathname, "\\\\"),
        "file://monkey//"
    );
    assert_eq!(
        set("data:space ?query", quirks::set_search, ""),
        "data:space%20"
    );
    assert_eq!(
        set("sc:space  ?query#fragment", quirks::set_hash, ""),
        "sc:space %20?query"
    );
    assert_eq!(set("http://h/", protocol, "https"), "https://h/");
    assert_eq!(set("http://h/", protocol, "foo"), "http://h/");
    assert_eq!(
        set("http://h:8080/", protocol, "https:x"),
        "https://h:8080/"
    );
    assert_eq!(set("https://h:443/", protocol, "http"), "http://h/");
    assert_eq!(
        set("http://h/", quirks::set_search, "?a b"),
        "http://h/?a%20b"
    );
    assert_eq!(
        set("http://h/", quirks::set_hash, "#a b"),
        "http://h/#a%20b"
    );
    let mut u = Url::parse("http://h/").unwrap();
    quirks::set_username(&mut u, "a b").unwrap();
    quirks::set_password(&mut u, "c@d").unwrap();
    assert_eq!(u.as_str(), "http://a%20b:c%40d@h/");
    let mut f = Url::parse("file:///x").unwrap();
    assert!(quirks::set_username(&mut f, "u").is_err());
}
