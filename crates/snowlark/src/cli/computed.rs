// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --computed <file.html> <selector> [property...] [--viewport WxH]`: parses a local HTML file,
//! styles it (its `<style>` elements, `<link rel=stylesheet>` and `@import` of local files), and prints
//! the computed values of `property...` (all longhands when none is given) for every element matching
//! `selector`, the way `getComputedStyle` serializes them.

use std::path::Path;
use std::sync::Arc;

use snowlark_dom::{local_name, ns, Dom, NodeId, ParseOptions};
use snowlark_style::{
    computed, DeviceConfig, ImportLoader, ImportRequest, ImportResponse, StyleData, StyleEngine,
};
use url::Url;

const USAGE: &str = "usage: snowlark --computed <file.html> <selector> [property...] [--viewport WxH]";

/// `@import` and `<link>` for `file:` URLs only: the CLI has no network stack.
struct FileLoader;

fn read_file_url(url: &Url) -> Option<String> {
    if url.scheme() != "file" {
        return None;
    }
    let path = url.to_file_path().ok()?;
    let bytes = std::fs::read(path).ok()?;
    // CSS Syntax §3.2: a BOM decides, else UTF-8 (the `@charset` and referrer fallbacks are the page
    // layer's).
    let (text, _, _) = snowlark_dom::encoding_rs::UTF_8.decode(&bytes);
    Some(text.into_owned())
}

impl ImportLoader for FileLoader {
    fn load(&self, request: &ImportRequest<'_>) -> ImportResponse {
        match read_file_url(request.url) {
            Some(text) => ImportResponse::Css {
                text,
                url: request.url.clone(),
            },
            None => ImportResponse::Failed,
        }
    }
}

pub fn run(args: &[String]) -> i32 {
    let mut positional = Vec::new();
    let mut viewport = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--viewport" {
            viewport = args.get(i + 1).and_then(|v| {
                let (w, h) = v.split_once('x')?;
                Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
            });
            if viewport.is_none() {
                eprintln!("{USAGE}");
                return 2;
            }
            i += 2;
        } else {
            positional.push(args[i].as_str());
            i += 1;
        }
    }
    let [file, selector, properties @ ..] = positional.as_slice() else {
        eprintln!("{USAGE}");
        return 2;
    };
    match print_computed(Path::new(file), selector, properties, viewport) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("computed: {e}");
            1
        }
    }
}

fn print_computed(
    file: &Path,
    selector: &str,
    properties: &[&str],
    viewport: Option<(f32, f32)>,
) -> Result<(), String> {
    let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let path = std::fs::canonicalize(file).map_err(|e| e.to_string())?;
    let url = Url::from_file_path(&path).map_err(|_| "not a file path".to_string())?;

    let mut dom = Dom::new();
    let doc = dom.parse_html_document(
        &bytes,
        snowlark_dom::encoding_rs::UTF_8,
        ParseOptions::default(),
    );
    dom.set_document_url(doc, url.as_str());

    let mut config = DeviceConfig::default();
    if let Some(v) = viewport {
        config.viewport = v;
    }
    let mut data = StyleData::new();
    let mut engine = StyleEngine::new(&dom, doc, config);
    engine.set_import_loader(Some(Arc::new(FileLoader)));
    engine.update_all_style_blocks(&dom, &data);
    add_linked_sheets(&dom, &data, &mut engine, doc, &url);
    engine.restyle(&dom, &mut data);

    let ids: Vec<_> = properties
        .iter()
        .map(|&name| {
            computed::property_id(name)
                .map(|id| (name.to_ascii_lowercase(), id))
                .ok_or_else(|| format!("unknown property {name}"))
        })
        .collect::<Result<_, _>>()?;
    let list = engine
        .parse_selector(selector)
        .map_err(|e| format!("invalid selector {selector}: {e}"))?;
    let matches = engine.query_selector_all(&dom, &data, doc, &list);
    if matches.is_empty() {
        return Err(format!("no element matches {selector}"));
    }
    for element in matches {
        let Some(values) = engine.computed_style(&dom, &data, element) else {
            continue;
        };
        println!("{}", describe(&dom, element));
        if ids.is_empty() {
            for name in computed::computed_property_names() {
                if let Some(v) = computed::computed_property_by_name(&values, name) {
                    println!("  {name}: {v}");
                }
            }
        } else {
            for (name, id) in &ids {
                println!("  {name}: {}", computed::computed_property(&values, id));
            }
        }
    }
    Ok(())
}

/// HTML §4.2.4 `link` elements whose `rel` includes `stylesheet` (and not `alternate`), fetched from
/// `file:` URLs, in tree order.
fn add_linked_sheets(dom: &Dom, data: &StyleData, engine: &mut StyleEngine, doc: NodeId, base: &Url) {
    let links: Vec<NodeId> = dom
        .descendants(doc)
        .filter(|&n| dom.is_html_element_named(n, &local_name!("link")))
        .collect();
    for link in links {
        let rel = dom.attr(link, &ns!(), &local_name!("rel")).unwrap_or("");
        let has = |kw: &str| rel.split_ascii_whitespace().any(|t| t.eq_ignore_ascii_case(kw));
        if !has("stylesheet") || has("alternate") {
            continue;
        }
        let Some(href) = dom.attr(link, &ns!(), &local_name!("href")) else {
            continue;
        };
        let Ok(url) = base.join(href) else {
            continue;
        };
        let Some(text) = read_file_url(&url) else {
            eprintln!("computed: could not load {url}");
            continue;
        };
        let media = dom.attr(link, &ns!(), &local_name!("media")).unwrap_or("");
        let sheet = engine.parse_sheet(data, &text, &url, media);
        engine.add_sheet(dom, data, link, sheet);
    }
}

/// `tag#id.class.class` for the header line of an element.
fn describe(dom: &Dom, element: NodeId) -> String {
    let mut out = dom.local_name(element).to_string();
    if let Some(id) = dom.attr(element, &ns!(), &local_name!("id")) {
        out.push('#');
        out.push_str(id);
    }
    if let Some(class) = dom.attr(element, &ns!(), &local_name!("class")) {
        for c in class.split_ascii_whitespace() {
            out.push('.');
            out.push_str(c);
        }
    }
    out
}
