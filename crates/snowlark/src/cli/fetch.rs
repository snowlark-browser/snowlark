// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --fetch <url>`: fetch a URL as a top-level navigation and print the status, the headers and
//! the body's length. `snowlark --fetch-trace <url>` (or `--fetch <url> --fetch-trace`) also logs each
//! network request, redirect and connection to stderr, as the C# engine's trace did. The cookie
//! jar and HSTS list are in memory only: a smoke test leaves the profile alone.

use std::time::Instant;

use snowlark_net::{FetchEvent, NetConfig, Request, Url};

/// A URL from the command line: absolute, a local path, or a host that gets https://.
fn parse_target(text: &str) -> Option<Url> {
    if let Ok(url) = Url::parse(text) {
        // (a Windows path like C:\x parses as a URL with scheme "c")
        if url.scheme().len() > 1 {
            return Some(url);
        }
    }
    let path = std::path::Path::new(text);
    if path.exists() {
        return std::fs::canonicalize(path)
            .ok()
            .and_then(|p| Url::from_file_path(p).ok());
    }
    Url::parse(&format!("https://{text}")).ok()
}

pub fn run(args: &[String]) -> i32 {
    let trace = args.iter().any(|a| a == "--fetch-trace");
    let Some(target) = args.iter().skip(1).find(|a| !a.starts_with("--")) else {
        eprintln!("usage: snowlark --fetch <url> [--fetch-trace]");
        return 2;
    };
    let Some(url) = parse_target(target) else {
        eprintln!("not a URL: {target}");
        return 2;
    };
    let mut config = NetConfig::from_env();
    config.trace |= trace;
    let thread = match snowlark_tab::script_check::start_net(config) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("can't start the network thread: {e}");
            return 1;
        }
    };
    let started = Instant::now();
    let mut stream = thread.client().fetch(Request::navigation(url));
    let mut length = 0usize;
    while let Some(event) = stream.next_blocking() {
        match event {
            FetchEvent::Violation(v) => eprintln!("{}", v.console_message()),
            FetchEvent::Head(head) => {
                let url = head.url().map(Url::to_string).unwrap_or_default();
                println!("{} {} ({})", head.status, head.status_text, url);
                let c = &head.connection;
                if !c.protocol.is_empty() {
                    println!(
                        "via {}{}{}",
                        c.protocol,
                        c.tls_version.as_deref().map(|v| format!(", {v}")).unwrap_or_default(),
                        if head.redirected() {
                            format!(", {} redirect(s)", head.url_list.len() - 1)
                        } else {
                            String::new()
                        }
                    );
                }
                for (name, value) in head.headers.iter() {
                    println!("{name}: {value}");
                }
            }
            FetchEvent::Body(chunk) => length += chunk.len(),
            FetchEvent::Done => {
                println!();
                println!("body: {length} bytes in {} ms", started.elapsed().as_millis());
                return 0;
            }
            FetchEvent::Error(e) => {
                eprintln!("network error: {e}");
                return 1;
            }
        }
    }
    1
}
