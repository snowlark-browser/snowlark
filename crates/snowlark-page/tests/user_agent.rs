// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The user agent a browser reports is one thing: for each preset and for a custom string, the
//! `User-Agent` header of the document request and of a subresource request, `navigator.userAgent`
//! in the page and in a worker, the other `navigator` fields, `navigator.userAgentData`, `oscpu` and
//! the `Sec-CH-UA` request headers all describe the same browser. A user agent with a line break in
//! it cannot start a second header.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use snowlark_net::ua::{Family, UserAgent, PRESETS};
use snowlark_page::{Page, PageOptions};

/// What the server saw: the request lines of each request, in order.
type Seen = Arc<Mutex<Vec<Vec<String>>>>;

/// A server of a few pages that records every request's header lines; `/echo` answers with the
/// header lines of its own request as plain text.
fn serve() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut lines = Vec::new();
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).is_err() || l == "\r\n" || l.is_empty() {
                    break;
                }
                lines.push(l.trim_end_matches(['\r', '\n']).to_owned());
            }
            let path = lines
                .first()
                .and_then(|f| f.split(' ').nth(1))
                .unwrap_or("/")
                .to_owned();
            log.lock().unwrap().push(lines.clone());
            let (mime, body) = match path.as_str() {
                "/echo" => ("text/plain", lines.join("\n")),
                "/w.js" => (
                    "text/javascript",
                    "postMessage([navigator.userAgent, navigator.appVersion, navigator.platform,                      typeof navigator.userAgentData].join('|'));"
                        .to_owned(),
                ),
                _ => ("text/html", "<!doctype html><title>ua</title>".to_owned()),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}"), seen)
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

fn wait_for(page: &mut Page, condition: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let js = format!("String(!!({condition}))");
    while !page.run_until(deadline, |p| p.eval(&js, "t").is_ok_and(|v| v == "true"))
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The value of header `name` among a request's lines.
fn header<'a>(lines: &'a [String], name: &str) -> Option<&'a str> {
    lines.iter().skip(1).find_map(|l| {
        let (n, v) = l.split_once(':')?;
        n.eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// A page of the server under the net client's current user agent: the document is `/echo`.
fn open(base: &str, net: &snowlark_net::NetThread) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.navigate(url::Url::parse(&format!("{base}/echo")).unwrap());
    page.run_until_settled(Duration::from_secs(10));
    page
}

/// A document of `url` (an `https:` one: `navigator.userAgentData` is `[SecureContext]`, and the
/// test server speaks plain HTTP) made without a fetch.
fn open_secure(url: &str, net: &snowlark_net::NetThread) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        ..Default::default()
    });
    page.load_html("<!doctype html><body>", &url::Url::parse(url).unwrap());
    page.run_until_settled(Duration::from_secs(10));
    page
}

/// Checks that everything a site can read says the same of `ua`.
fn check(label: &str, ua: &UserAgent, base: &str, seen: &Seen, net: &snowlark_net::NetThread) {
    net.client().set_user_agent(ua.clone());
    seen.lock().unwrap().clear();
    let mut page = open(base, net);
    let profile = ua.profile();
    // The document request's header.
    let doc = seen.lock().unwrap()[0].clone();
    assert_eq!(
        header(&doc, "user-agent"),
        Some(ua.value.as_str()),
        "{label}: document"
    );
    // The page's own view.
    let js = |p: &mut Page, e: &str| eval(p, e);
    assert_eq!(js(&mut page, "navigator.userAgent"), ua.value, "{label}");
    assert_eq!(
        js(&mut page, "navigator.appVersion"),
        ua.value.strip_prefix("Mozilla/").unwrap_or(&ua.value),
        "{label}"
    );
    assert_eq!(
        js(&mut page, "navigator.platform"),
        profile.nav_platform(),
        "{label}"
    );
    assert_eq!(
        js(&mut page, "navigator.vendor"),
        profile.vendor(),
        "{label}"
    );
    assert_eq!(
        js(&mut page, "navigator.productSub"),
        profile.product_sub(),
        "{label}"
    );
    assert_eq!(js(&mut page, "navigator.product"), "Gecko", "{label}");
    // Members only some browsers have.
    let data = js(&mut page, "typeof navigator.userAgentData");
    assert_eq!(
        data == "object",
        profile.has_ua_data(),
        "{label}: userAgentData is {data}"
    );
    let oscpu = js(&mut page, "typeof navigator.oscpu");
    assert_eq!(
        oscpu == "string",
        profile.has_oscpu(),
        "{label}: oscpu is {oscpu}"
    );
    if profile.has_oscpu() {
        assert_eq!(js(&mut page, "navigator.oscpu"), profile.oscpu(), "{label}");
    }
    if profile.has_ua_data() {
        let brands = js(
            &mut page,
            "navigator.userAgentData.brands.map(b => b.brand + '/' + b.version).join(',')",
        );
        let want: Vec<String> = profile
            .brands(false)
            .iter()
            .map(|(b, v)| format!("{b}/{v}"))
            .collect();
        assert_eq!(brands, want.join(","), "{label}");
        assert_eq!(
            js(&mut page, "navigator.userAgentData.platform"),
            profile.os,
            "{label}"
        );
        assert_eq!(
            js(&mut page, "String(navigator.userAgentData.mobile)"),
            "false",
            "{label}"
        );
    }
    // A subresource request: the same header, and the client hints of a Chromium user agent
    // (and of no other).
    seen.lock().unwrap().clear();
    eval(
        &mut page,
        "window.sub = null; fetch('/echo').then(r => r.text()).then(t => { window.sub = t; }); 1",
    );
    wait_for(&mut page, "window.sub !== null");
    let sub = seen.lock().unwrap().last().cloned().expect("a request");
    assert_eq!(
        header(&sub, "user-agent"),
        Some(ua.value.as_str()),
        "{label}: subresource"
    );
    let hints = profile.client_hint_headers();
    for (name, value) in &hints {
        assert_eq!(header(&sub, name), Some(value.as_str()), "{label}: {name}");
        assert_eq!(
            header(&doc, name),
            Some(value.as_str()),
            "{label}: {name} on the document"
        );
    }
    if hints.is_empty() {
        assert!(
            sub.iter()
                .all(|l| !l.to_ascii_lowercase().starts_with("sec-ch-ua")),
            "{label}: client hints from a non-Chromium agent: {sub:?}"
        );
    } else {
        // The header and navigator.userAgentData say the same brands.
        let list = header(&sub, "sec-ch-ua").unwrap();
        let from_js = js(
            &mut page,
            "navigator.userAgentData.brands.map(b => '\"' + b.brand + '\";v=\"' + b.version + '\"').join(', ')",
        );
        assert_eq!(list, from_js, "{label}");
    }
    // A worker made after: its navigator agrees too.
    eval(
        &mut page,
        "window.w = null; const w = new Worker('/w.js'); w.onmessage = e => { window.w = e.data; }; 1",
    );
    wait_for(&mut page, "window.w !== null");
    let w = eval(&mut page, "window.w");
    // (a worker's navigator has what WorkerNavigator exposes: not vendor, productSub or oscpu)
    let want = format!(
        "{}|{}|{}|{}",
        ua.value,
        ua.value.strip_prefix("Mozilla/").unwrap_or(&ua.value),
        profile.nav_platform(),
        if profile.has_ua_data() {
            "object"
        } else {
            "undefined"
        },
    );
    assert_eq!(w, want, "{label}: worker");
}

#[test]
fn every_preset_and_a_custom_string_say_the_same_browser_everywhere() {
    let (base, seen) = serve();
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    for p in &PRESETS {
        let ua = UserAgent::preset(p.id).unwrap();
        check(p.id, &ua, &base, &seen, &net);
    }
    // A custom Chromium string on Linux, and one that names no known browser.
    let linux = UserAgent::chosen(
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36",
    );
    assert_eq!(linux.profile().family, Family::Chromium);
    check("custom chromium on linux", &linux, &base, &seen, &net);
    check(
        "custom unknown",
        &UserAgent::chosen("MyCrawler/2.1 (+https://a.example/bot)"),
        &base,
        &seen,
        &net,
    );
}

#[test]
fn the_default_is_the_familys_string_with_snowlark_last() {
    // The family's string with the `Chrome/` token of the release the engine's V8 comes from (V8
    // X.Y ships in release XY), then Snowlark's own product token; the family's brand and Snowlark's.
    let v8: Vec<u32> = snowlark_js::v8_version()
        .split('.')
        .take(2)
        .map(|n| n.parse().unwrap())
        .collect();
    let chrome = (v8[0] * 10 + v8[1]).to_string();
    assert_eq!(
        snowlark_net::ua::COMPAT_VERSION,
        format!("{chrome}.0.0.0"),
        "V8 {}",
        snowlark_js::v8_version()
    );
    let (base, seen) = serve();
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    // (a client nobody touched: as every headless mode runs)
    seen.lock().unwrap().clear();
    let mut page = open(&base, &net);
    assert_eq!(
        eval(&mut page, "navigator.userAgent"),
        snowlark_net::metadata::USER_AGENT
    );
    assert_eq!(eval(&mut page, "typeof navigator.userAgentData"), "object");
    assert_eq!(
        eval(
            &mut page,
            "navigator.userAgentData.brands.map(b => b.brand + '/' + b.version).sort().join()"
        ),
        {
            let mut b: Vec<String> = UserAgent::default()
                .profile()
                .brands(false)
                .into_iter()
                .map(|(n, v)| format!("{n}/{v}"))
                .collect();
            b.sort();
            assert!(b.contains(&format!("Chromium/{chrome}")) && b.contains(&"Snowlark/0".into()));
            b.join(",")
        }
    );
    assert_eq!(eval(&mut page, "typeof navigator.oscpu"), "undefined");
    assert_eq!(eval(&mut page, "navigator.vendor"), "Google Inc.");
    // (the platform of the system's User-Agent string)
    let platform = if cfg!(windows) {
        "Win32"
    } else {
        "Linux x86_64"
    };
    assert_eq!(eval(&mut page, "navigator.platform"), platform);
    assert_eq!(eval(&mut page, "navigator.productSub"), "20030107");
    let doc = seen.lock().unwrap()[0].clone();
    assert_eq!(
        header(&doc, "user-agent"),
        Some(snowlark_net::metadata::USER_AGENT)
    );
    let hint = header(&doc, "sec-ch-ua").unwrap().to_owned();
    assert!(
        hint.contains(&format!("\"Chromium\";v=\"{chrome}\""))
            && hint.contains("\"Snowlark\";v=\"0\"")
            && !hint.contains("Google"),
        "{hint}"
    );
    assert_eq!(
        hint,
        eval(
            &mut page,
            "navigator.userAgentData.brands.map(b => '\"' + b.brand + '\";v=\"' + b.version + '\"').join(', ')"
        )
    );
    // `appVersion` is the same string after `Mozilla/`.
    let os = if cfg!(windows) {
        "Windows NT 10.0; Win64; x64"
    } else {
        "X11; Linux x86_64"
    };
    assert_eq!(
        snowlark_net::metadata::USER_AGENT,
        format!(
            "Mozilla/5.0 ({os}) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/{chrome}.0.0.0 Safari/537.36 Snowlark/0.1"
        )
    );
    assert_eq!(
        eval(&mut page, "navigator.appVersion"),
        format!(
            "5.0 ({os}) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/{chrome}.0.0.0 Safari/537.36 Snowlark/0.1"
        )
    );
    assert_eq!(
        eval(
            &mut page,
            "navigator.userAgentData.getHighEntropyValues(['fullVersionList', 'uaFullVersion', \
         'platform']).then(v => window.hev = v.fullVersionList.map(b => b.brand + ' ' + \
         b.version).join() + ' | ' + v.uaFullVersion + ' | ' + v.platform); 'ok'"
        ),
        "ok"
    );
    page.run_until_settled(std::time::Duration::from_secs(5));
    let hev = eval(&mut page, "window.hev");
    let (list, rest) = hev.split_once(" | ").unwrap();
    let mut list: Vec<String> = list.split(',').map(str::to_owned).collect();
    list.sort();
    let mut expected: Vec<String> = UserAgent::default()
        .profile()
        .brands(true)
        .into_iter()
        .map(|(n, v)| format!("{n} {v}"))
        .collect();
    expected.sort();
    assert_eq!(list, expected);
    assert!(
        list.contains(&format!("Chromium {chrome}.0.0.0")) && list.contains(&"Snowlark 0.1".into())
    );
    let platform = if cfg!(windows) { "Windows" } else { "Linux" };
    assert_eq!(rest, format!("{chrome}.0.0.0 | {platform}"));
}

#[test]
fn a_line_break_in_the_user_agent_cannot_start_a_header() {
    let (base, seen) = serve();
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    // Straight into the client, past the cleaning where a setting is stored: the header is made
    // clean again where it is built.
    net.client().set_user_agent(UserAgent {
        value: "Evil/1.0\r\nX-Injected: yes\r\n\r\nGET /smuggled HTTP/1.1".into(),
        strict: true,
        site_compat: false,
        unbranded: false,
    });
    seen.lock().unwrap().clear();
    let mut page = open(&base, &net);
    let all = seen.lock().unwrap().clone();
    assert_eq!(all.len(), 1, "one request, not two: {all:?}");
    let doc = &all[0];
    assert!(
        header(doc, "x-injected").is_none(),
        "an injected header reached the server: {doc:?}"
    );
    assert!(doc
        .iter()
        .all(|l| !l.contains("smuggled") || l.to_ascii_lowercase().starts_with("user-agent")));
    let ua = header(doc, "user-agent").unwrap();
    assert!(ua.starts_with("Evil/1.0"), "{ua}");
    assert!(!eval(&mut page, "navigator.userAgent").contains(['\r', '\n']));
    // And through the setting: the stored text is already one clean line.
    let chosen = UserAgent::chosen("Evil/1.0\r\nX-Injected: yes");
    assert!(!chosen.value.contains(['\r', '\n']));
    assert_eq!(chosen.value, "Evil/1.0X-Injected: yes");
}

/// Waits until the server has seen a request whose request line contains `marker`, and returns
/// its header lines.
fn request_with(page: &mut Page, seen: &Seen, marker: &str) -> Vec<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let find = || {
        seen.lock()
            .unwrap()
            .iter()
            .find(|r| r.first().is_some_and(|l| l.contains(marker)))
            .cloned()
    };
    loop {
        if let Some(r) = find() {
            return r;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no request for {marker}"
        );
        page.run_until(
            std::time::Instant::now() + Duration::from_millis(20),
            |_| false,
        );
    }
}

/// Per-site identities: with the window's default identity, a listed host gets the default string
/// in the `User-Agent` of every request to it and in the `navigator` of its documents, its frames
/// and its workers, with brands that leave out Snowlark's; another site keeps the default brands,
/// also when one of them requests the other.
#[test]
fn listed_sites_get_the_default_unbranded_in_requests_and_navigator() {
    let (base, seen) = serve();
    let port = base.rsplit(':').next().unwrap().to_owned();
    let config = snowlark_net::NetConfig {
        host_rules: snowlark_net::HostRule::parse_list(
            "MAP mail.google.com 127.0.0.1, MAP other.test 127.0.0.1",
        ),
        ..Default::default()
    };
    let net = snowlark_net::NetThread::start(config).unwrap();
    net.client()
        .set_user_agent(UserAgent::default_with_site_compat());
    // The listed sites get the default string; only their brands differ (below).
    let plain = UserAgent::default().value;
    let compat = plain.clone();
    let listed = format!("http://mail.google.com:{port}");
    let other = format!("http://other.test:{port}");

    // A listed site: its document, a frame of its origin and its worker.
    seen.lock().unwrap().clear();
    let mut page = open(&listed, &net);
    let doc = seen.lock().unwrap()[0].clone();
    assert_eq!(
        header(&doc, "user-agent"),
        Some(compat.as_str()),
        "document"
    );
    assert_eq!(eval(&mut page, "navigator.userAgent"), compat);
    assert_eq!(
        eval(&mut page, "navigator.appVersion"),
        compat.strip_prefix("Mozilla/").unwrap()
    );
    assert_eq!(
        eval(
            &mut page,
            "const f = document.createElement('iframe'); document.body.append(f); \
             f.contentWindow.navigator.userAgent"
        ),
        compat,
        "an about:blank frame of the site"
    );
    eval(
        &mut page,
        "window.w = null; const w = new Worker('/w.js'); w.onmessage = e => { window.w = e.data; }; 1",
    );
    wait_for(&mut page, "window.w");
    assert_eq!(
        eval(&mut page, "window.w.split('|')[0]"),
        compat,
        "the site's worker"
    );
    // The brands leave out the browser's own (the mail app turns away `Snowlark` 0); the rest of
    // `navigator` follows the default identity.
    let default_brands = {
        let mut b: Vec<String> = UserAgent::default()
            .profile()
            .brands(false)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        b.sort();
        b
    };
    let unbranded: Vec<String> = default_brands
        .iter()
        .filter(|b| b.as_str() != snowlark_net::ua::PRODUCT)
        .cloned()
        .collect();
    assert!(unbranded.iter().any(|b| b == "Chromium"));
    assert_ne!(unbranded, default_brands);
    // (`navigator.userAgentData` only in a secure document of the site: below)
    assert_eq!(
        eval(&mut page, "typeof navigator.userAgentData"),
        "undefined"
    );
    assert_eq!(
        eval(&mut page, "navigator.platform"),
        UserAgent::default().profile().nav_platform()
    );
    // A request from the listed site to another one carries the other site's string.
    eval(
        &mut page,
        &format!("fetch('{other}/from-listed', {{mode: 'no-cors'}}); 1"),
    );
    let r = request_with(&mut page, &seen, "/from-listed");
    assert_eq!(
        header(&r, "user-agent"),
        Some(plain.as_str()),
        "listed -> other"
    );

    // Another site: the default, in its document and its navigator; its request to the listed
    // site carries the listed site's string.
    seen.lock().unwrap().clear();
    let mut page = open(&other, &net);
    let doc = seen.lock().unwrap()[0].clone();
    assert_eq!(
        header(&doc, "user-agent"),
        Some(plain.as_str()),
        "other document"
    );
    assert_eq!(eval(&mut page, "navigator.userAgent"), plain);
    eval(
        &mut page,
        &format!("fetch('{listed}/from-other', {{mode: 'no-cors'}}); 1"),
    );
    let r = request_with(&mut page, &seen, "/from-other");
    assert_eq!(
        header(&r, "user-agent"),
        Some(compat.as_str()),
        "other -> listed"
    );

    // Off (the headless modes, a preset, a custom string): the listed site gets the default.
    net.client().set_user_agent(UserAgent::default());
    seen.lock().unwrap().clear();
    let mut page = open(&listed, &net);
    let doc = seen.lock().unwrap()[0].clone();
    assert_eq!(
        header(&doc, "user-agent"),
        Some(plain.as_str()),
        "off: document"
    );
    assert_eq!(
        eval(&mut page, "navigator.userAgent"),
        plain,
        "off: navigator"
    );

    // `navigator.userAgentData` is `[SecureContext]`: the brands, in secure documents of the sites
    // (pages made after the ones above, which are not used again).
    net.client()
        .set_user_agent(UserAgent::default_with_site_compat());
    let mut secure = open_secure("https://mail.google.com/", &net);
    let brands_js = "navigator.userAgentData.brands.map(b => b.brand).sort().join()";
    assert_eq!(eval(&mut secure, brands_js), unbranded.join(","));
    assert_eq!(
        eval(
            &mut secure,
            "const f = document.createElement('iframe'); document.body.append(f); \
             f.contentWindow.navigator.userAgentData.brands.map(b => b.brand).sort().join()"
        ),
        unbranded.join(","),
        "the site's frame"
    );
    assert_eq!(
        eval(
            &mut secure,
            "JSON.stringify(navigator.userAgentData.toJSON().brands.map(b => b.brand).sort())"
        ),
        serde_json::to_string(&unbranded).unwrap()
    );
    eval(
        &mut secure,
        "window.fv = null; navigator.userAgentData.getHighEntropyValues(['fullVersionList']) \
         .then(v => { window.fv = v.fullVersionList.map(b => b.brand).sort().join(); }); 1",
    );
    wait_for(&mut secure, "window.fv");
    assert_eq!(eval(&mut secure, "window.fv"), unbranded.join(","));
    let mut secure = open_secure("https://other.test/", &net);
    assert_eq!(
        eval(&mut secure, brands_js),
        default_brands.join(","),
        "other: brands"
    );
}

/// HBO Max (its app, its API and its licence host) gets the Edge preset's identity in the window with
/// the Default setting: the requests' header, `navigator.userAgent` and the `Microsoft Edge` brand
/// of `navigator.userAgentData` in its documents and workers; a request from it to another site
/// carries that site's string.
#[test]
fn hbo_max_gets_the_edge_identity_in_requests_and_navigator() {
    let (base, seen) = serve();
    let port = base.rsplit(':').next().unwrap().to_owned();
    let config = snowlark_net::NetConfig {
        host_rules: snowlark_net::HostRule::parse_list(
            "MAP play.hbomax.com 127.0.0.1, MAP default.any-any.prd.api.hbomax.com 127.0.0.1, \
             MAP busy.any-any.prd.api.discomax.com 127.0.0.1, MAP other.test 127.0.0.1",
        ),
        ..Default::default()
    };
    let net = snowlark_net::NetThread::start(config).unwrap();
    net.client()
        .set_user_agent(UserAgent::default_with_site_compat());
    let edge = UserAgent::preset("edge").unwrap();
    let plain = UserAgent::default().value;
    let app = format!("http://play.hbomax.com:{port}");
    let api = format!("http://default.any-any.prd.api.hbomax.com:{port}");
    let licence = format!("http://busy.any-any.prd.api.discomax.com:{port}");
    let other = format!("http://other.test:{port}");

    seen.lock().unwrap().clear();
    let mut page = open(&app, &net);
    let doc = seen.lock().unwrap()[0].clone();
    assert_eq!(
        header(&doc, "user-agent"),
        Some(edge.value.as_str()),
        "document"
    );
    assert_eq!(eval(&mut page, "navigator.userAgent"), edge.value);
    let profile = edge.profile();
    let want: Vec<String> = profile
        .brands(false)
        .iter()
        .map(|(b, v)| format!("{b}/{v}"))
        .collect();
    // (`navigator.userAgentData` only in a secure document of the app: at the end)
    assert!(want.iter().any(|b| b == "Microsoft Edge/150"), "{want:?}");
    eval(
        &mut page,
        "window.w = null; const w = new Worker('/w.js'); w.onmessage = e => { window.w = e.data; }; 1",
    );
    wait_for(&mut page, "window.w");
    assert_eq!(
        eval(&mut page, "window.w.split('|')[0]"),
        edge.value,
        "the app's worker"
    );
    // Its API and licence host get the same identity; another site gets its own.
    for (i, (to, want)) in [
        (&api, edge.value.as_str()),
        (&licence, edge.value.as_str()),
        (&other, plain.as_str()),
    ]
    .into_iter()
    .enumerate()
    {
        let marker = format!("/from-app-{i}");
        eval(
            &mut page,
            &format!("fetch('{to}{marker}', {{mode: 'no-cors'}}); 1"),
        );
        let r = request_with(&mut page, &seen, &marker);
        assert_eq!(header(&r, "user-agent"), Some(want), "app -> {to}");
    }

    // A chosen preset wins: the Chrome preset is sent to the app as chosen.
    let chrome = UserAgent::preset("chrome").unwrap();
    net.client().set_user_agent(chrome.clone());
    seen.lock().unwrap().clear();
    let mut page = open(&app, &net);
    let doc = seen.lock().unwrap()[0].clone();
    assert_eq!(header(&doc, "user-agent"), Some(chrome.value.as_str()));
    assert_eq!(eval(&mut page, "navigator.userAgent"), chrome.value);

    // `navigator.userAgentData` is `[SecureContext]`: the Edge brands, in a secure document of the
    // app (made after the pages above, which are not used again).
    net.client()
        .set_user_agent(UserAgent::default_with_site_compat());
    let mut secure = open_secure("https://play.hbomax.com/", &net);
    let brands_js = "navigator.userAgentData.brands.map(b => b.brand + '/' + b.version).join(',')";
    assert_eq!(eval(&mut secure, brands_js), want.join(","));
    assert_eq!(
        eval(&mut secure, "navigator.userAgentData.platform"),
        "Windows"
    );
    assert_eq!(
        eval(
            &mut secure,
            "const f = document.createElement('iframe'); document.body.append(f); \
             f.contentWindow.navigator.userAgentData.brands.map(b => b.brand + '/' + b.version).join(',')"
        ),
        want.join(","),
        "an about:blank frame of the app"
    );
    eval(
        &mut secure,
        "window.fv = null; navigator.userAgentData.getHighEntropyValues(['fullVersionList', 'uaFullVersion']) \
         .then(v => { window.fv = v.fullVersionList.map(b => b.brand).sort().join() + '|' + v.uaFullVersion; }); 1",
    );
    wait_for(&mut secure, "window.fv");
    let mut full: Vec<String> = profile.brands(true).into_iter().map(|(b, _)| b).collect();
    full.sort();
    assert_eq!(
        eval(&mut secure, "window.fv"),
        format!("{}|{}", full.join(","), profile.ua_full_version())
    );
}
