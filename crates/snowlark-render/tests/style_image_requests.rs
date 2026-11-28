// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The images a computed style refers to are fetched whatever the element: `cursor` (CSS UI 4 §6.1.1)
//! and `list-style-image` (CSS Lists 3 §3) on an element that is not a list item load their image as
//! the other style images do (Fetch: destination `image`), though neither paints there.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use snowlark_net::{NetConfig, NetThread, Url};
use snowlark_paint::TextEngine;
use snowlark_render::Renderer;
use snowlark_style::DeviceConfig;

/// A server on 127.0.0.1 that records the head of each request and answers 404.
fn serve_recording() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let log = log.clone();
            std::thread::spawn(move || answer(stream, &log));
        }
    });
    (port, seen)
}

fn answer(mut stream: TcpStream, log: &Mutex<Vec<String>>) {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    log.lock()
        .unwrap()
        .push(String::from_utf8_lossy(&head).to_ascii_lowercase());
    let _ = stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

fn requests_of(html: &str) -> Vec<String> {
    let (port, seen) = serve_recording();
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (200.0, 100.0),
            scripting: false,
            ..Default::default()
        },
    );
    let html = html.replace("{port}", &port.to_string());
    let url = Url::parse(&format!("http://127.0.0.1:{port}/page.html")).unwrap();
    r.load_html(html.as_bytes(), &url, None);
    r.render_until_settled(Duration::from_secs(20));
    let seen = seen.lock().unwrap().clone();
    seen
}

fn asked_for(requests: &[String], path: &str) -> bool {
    requests
        .iter()
        .any(|head| head.starts_with(&format!("get {path} ")))
}

#[test]
fn cursor_images_are_fetched_as_images() {
    let requests = requests_of(
        "<!DOCTYPE html><div style='cursor: url(http://127.0.0.1:{port}/cursor.png), auto'>x</div>",
    );
    assert!(asked_for(&requests, "/cursor.png"), "{requests:?}");
    let head = requests
        .iter()
        .find(|h| h.starts_with("get /cursor.png "))
        .unwrap();
    assert!(head.contains("sec-fetch-dest: image"), "{head}");
}

#[test]
fn list_style_image_is_fetched_on_elements_that_are_not_list_items() {
    let requests = requests_of(
        "<!DOCTYPE html><div style='list-style-image: url(http://127.0.0.1:{port}/marker.png)'>x</div>",
    );
    assert!(asked_for(&requests, "/marker.png"), "{requests:?}");
}
