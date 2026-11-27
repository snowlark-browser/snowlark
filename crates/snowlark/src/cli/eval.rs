// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --eval "<js>"`: runs the script in a window whose document is
//! `<!doctype html><html><head></head><body></body></html>`, or the file given with `--html <file>` (its
//! scripts run first), then runs the event loop until it settles and prints the completion value.
//! Console output goes to stderr. With `--layout` the page renders (styles, layout and geometry
//! APIs work, as in `--wpt`); without it there is no layout.

use std::time::Duration;

use snowlark_page::ConsoleOutput;
use snowlark_page::{Page, PageOptions};

pub fn run(args: &[String]) -> i32 {
    let mut source: Option<&str> = None;
    let mut html_file: Option<&str> = None;
    let mut layout = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--eval" => {}
            "--layout" => layout = true,
            "--html" => {
                i += 1;
                html_file = args.get(i).map(String::as_str);
            }
            other if source.is_none() => source = Some(other),
            _ => {}
        }
        i += 1;
    }
    let source = source.unwrap_or("undefined");
    // (the network thread lives as long as the page)
    let mut _net_thread = None;
    let render = if layout {
        let thread = match snowlark_tab::script_check::start_net(snowlark_net::NetConfig::from_env()) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("network thread: {e}");
                return 1;
            }
        };
        match snowlark_page::RenderSetup::new() {
            Ok(mut r) => {
                r.net = Some(thread.client());
                _net_thread = Some(thread);
                Some(r)
            }
            Err(e) => {
                eprintln!("render: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    let mut page = Page::new(PageOptions {
        console: ConsoleOutput::Stderr,
        net: _net_thread.as_ref().map(|t| t.client()),
        render,
        webgl: snowlark_shell::gpu_process::headless_webgl(),
        ..Default::default()
    });
    let (html, url) = match html_file {
        Some(f) => match std::fs::read(f) {
            Ok(bytes) => {
                let path = std::fs::canonicalize(f).unwrap_or_else(|_| f.into());
                let url = url::Url::from_file_path(&path)
                    .unwrap_or_else(|_| url::Url::parse("about:blank").unwrap());
                (String::from_utf8_lossy(&bytes).into_owned(), url)
            }
            Err(e) => {
                eprintln!("cannot read {f}: {e}");
                return 1;
            }
        },
        None => (
            "<!doctype html><html><head></head><body></body></html>".to_owned(),
            url::Url::parse("about:blank").unwrap(),
        ),
    };
    if layout && html_file.is_some() {
        page.navigate(url);
    } else {
        page.load_html(&html, &url);
    }
    page.run_until_settled(Duration::from_secs(5));
    let result = page.eval(source, "<eval>");
    page.run_until_settled(Duration::from_secs(5));
    match result {
        Ok(v) => {
            println!("{v}");
            0
        }
        Err(e) => {
            eprintln!("Uncaught {e}");
            1
        }
    }
}
