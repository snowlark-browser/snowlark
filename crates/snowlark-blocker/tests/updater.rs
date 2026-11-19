// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The updater against a local server: a list is downloaded once, compiled and saved, then
//! revalidated with its ETag (a 304 costs no download and no compile).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use snowlark_blocker::engine::NetworkMatch;
use snowlark_blocker::lists::{ListStore, CATALOG};
use snowlark_blocker::{updater, ContentBlocker};

fn path_of(request_line: &str) -> String {
    request_line.split(' ').nth(1).unwrap_or("").to_owned()
}

type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// Serves one list (which includes `part.txt`) with an ETag; records each request's path and
/// If-None-Match.
fn serve(seen: Seen) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            let mut inm = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("if-none-match:") {
                    inm = Some(v.trim().to_owned());
                }
            }
            seen.lock().unwrap().push((path_of(&first), inm.clone()));
            let body = if inm.is_none() && path_of(&first) == "/part.txt" {
                "||tracker.example^\n"
            } else {
                "! Title: Test\n! Expires: 2 days\n||ads.example^\n!#include part.txt\n!#include ../escape.txt\n"
            };
            let response = if inm.as_deref() == Some("\"v1\"") {
                "HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n".to_owned()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

#[test]
fn lists_are_downloaded_compiled_and_revalidated() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = serve(seen.clone());
    let dir = std::env::temp_dir().join(format!("snowlark-blocker-updater-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let blocker = ContentBlocker::open(&dir);
    blocker.set_resource_repository(None);
    // Only the user's list (no downloads from the real servers in a test).
    blocker.update_config(|c| {
        for i in CATALOG {
            c.lists.insert(i.id.to_owned(), false);
        }
    });
    let url = format!("http://127.0.0.1:{port}/list.txt");
    assert!(blocker.add_custom_list(&url));
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let mut store = ListStore::open(&dir);

    let next = updater::pass(&blocker, &net.client(), &mut store, false);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [
            ("/list.txt".to_owned(), None),
            ("/part.txt".to_owned(), None)
        ],
        "the include is fetched, the one outside the list's directory is not"
    );
    for host in ["ads.example", "tracker.example"] {
        assert_eq!(
            blocker.engine().match_request(
                &format!("https://{host}/x.js"),
                "https://a.example/",
                "script"
            ),
            NetworkMatch::Block,
            "{host}"
        );
    }
    assert!(dir.join("engine.dat").is_file());
    // Due again when the copy expires (2 days).
    let now = updater::pass(&blocker, &net.client(), &mut store, false);
    assert_eq!(now, next);
    assert_eq!(seen.lock().unwrap().len(), 2, "not due: no request");

    // Forced: revalidated with the ETag; a 304 keeps the engine.
    let generation = blocker.generation();
    updater::pass(&blocker, &net.client(), &mut store, true);
    assert_eq!(seen.lock().unwrap()[2].1.as_deref(), Some("\"v1\""));
    assert_eq!(seen.lock().unwrap()[3].1.as_deref(), Some("\"v1\""));
    assert_eq!(blocker.generation(), generation);

    // A new process loads the saved engine.
    let again = ContentBlocker::open(&dir);
    assert_eq!(
        again
            .engine()
            .match_request("https://ads.example/x.js", "https://a.example/", "script"),
        NetworkMatch::Block
    );
    let status = again.list_status();
    let mine = status.iter().find(|s| s.custom).unwrap();
    assert_eq!(mine.lines, 4);
    assert!(mine.error.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
