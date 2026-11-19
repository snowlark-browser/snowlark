// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resources for `redirect=` filters and `##+js()` scriptlets.
//!
//! uBlock Origin's own resources (its scriptlets and `web_accessible_resources`, GPL-3) are fetched
//! at run time with the lists ([`crate::scriptlets`]) and kept in the profile; nothing of them is in
//! the binary. Until they arrive, the trivial neutered resources below (written here: empty
//! scripts, styles and documents, transparent images) answer the redirect filters that name them,
//! under uBlock Origin's resource names and aliases so the lists' filters find them.

use adblock::resources::{MimeType, Resource, ResourceType};
use base64::Engine as _;

fn resource(name: &str, aliases: &[&str], mime: MimeType, bytes: &[u8]) -> Resource {
    Resource {
        name: name.to_owned(),
        aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
        kind: ResourceType::Mime(mime),
        content: base64::engine::general_purpose::STANDARD.encode(bytes),
        dependencies: Vec::new(),
        permission: Default::default(),
    }
}

fn b64(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("a built-in resource")
}

/// The built-in neutered resources.
pub fn fallback_resources() -> Vec<Resource> {
    use MimeType::*;
    vec![
        resource(
            "noop.js",
            &["noopjs", "abp-resource:blank-js"],
            ApplicationJavascript,
            b"(function() {\n    'use strict';\n})();\n",
        ),
        resource("noop.css", &[], TextCss, b""),
        resource("noop.txt", &["nooptext"], TextPlain, b""),
        resource("empty", &[], TextPlain, b""),
        resource("noop.json", &["noopjson"], ApplicationJson, b"{}"),
        resource(
            "noop.html",
            &["noopframe"],
            TextHtml,
            b"<!DOCTYPE html><html><head><meta charset=\"utf-8\"></head><body></body></html>\n",
        ),
        resource(
            "noop-vast2.xml",
            &["noopvast-2.0"],
            TextXml,
            b"<VAST version=\"2.0\"></VAST>",
        ),
        resource(
            "noop-vast3.xml",
            &["noopvast-3.0"],
            TextXml,
            b"<VAST version=\"3.0\"></VAST>",
        ),
        resource(
            "noop-vast4.xml",
            &["noopvast-4.0"],
            TextXml,
            b"<VAST version=\"4.0\"></VAST>",
        ),
        resource(
            "1x1.gif",
            &["1x1-transparent.gif"],
            ImageGif,
            &b64("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7"),
        ),
        resource(
            "2x2.png",
            &["2x2-transparent.png"],
            ImagePng,
            &b64("iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAC0lEQVR42mNgQAcAABIAAeRVjecAAAAASUVORK5CYII="),
        ),
        resource(
            "3x2.png",
            &["3x2-transparent.png"],
            ImagePng,
            &b64("iVBORw0KGgoAAAANSUhEUgAAAAMAAAACCAYAAACddGYaAAAAC0lEQVR42mNgwAUAABoAAS+Yl6YAAAAASUVORK5CYII="),
        ),
        resource(
            "32x32.png",
            &["32x32-transparent.png"],
            ImagePng,
            &b64("iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAAGklEQVR42u3BAQEAAACCIP+vbkhAAQAAAO8GECAAAcm1w7EAAAAASUVORK5CYII="),
        ),
    ]
}

/// Resources saved in the profile (adblock-rust's JSON form), with the built-in ones for names
/// they lack.
pub fn load_resources(path: &std::path::Path) -> Vec<Resource> {
    let mut list: Vec<Resource> = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    for r in fallback_resources() {
        if !list.iter().any(|x| x.name == r.name) {
            list.push(r);
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_resources_load_and_answer_redirects() {
        let e = crate::engine::FilterEngine::compile(
            &[crate::engine::ListText {
                text: "||a.example/p.gif$image,redirect=1x1.gif\n||a.example/f$subdocument,redirect=noopframe",
                format: crate::engine::ListFormat::Filters,
                trusted: false,
            }],
            fallback_resources(),
        );
        let m = e.match_request("https://a.example/p.gif", "https://b.example/", "image");
        assert!(
            matches!(&m, crate::engine::NetworkMatch::Redirect(d) if d.starts_with("data:image/gif;base64,")),
            "{m:?}"
        );
        let m = e.match_request("https://a.example/f", "https://b.example/", "subdocument");
        assert!(
            matches!(&m, crate::engine::NetworkMatch::Redirect(d) if d.starts_with("data:text/html;base64,")),
            "{m:?}"
        );
    }
}
