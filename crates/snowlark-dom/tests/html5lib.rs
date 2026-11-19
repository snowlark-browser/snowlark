// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! html5lib tree-construction tests: every `.dat` file in WPT's `html/syntax/parsing/resources`, parsed
//! straight through `snowlark-dom` (no page, no scripts) and dumped in the format WPT's `resources/test.js`
//! uses. Cases marked `#script-off` are skipped: the parser runs with scripting enabled, as in a browser.
//!
//! The checkout is `wpt` next to the workspace, or `SNOWLARK_WPT`. Without it the test passes
//! vacuously. Known failures are listed, with the reason, in `tests/html5lib-known-failures.txt`; any other
//! failure fails the test, and so does a listed case that now passes (so the list stays exact).
//!
//! `cargo test -p snowlark-dom --test html5lib -- --nocapture` prints the per-file counts.

use std::collections::BTreeSet;
use std::path::PathBuf;

use snowlark_dom::{ns, DocumentKind, Dom, NodeData, NodeId, ParseOptions};

fn resources() -> PathBuf {
    let wpt = std::env::var_os("SNOWLARK_WPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../wpt"));
    wpt.join("html/syntax/parsing/resources")
}

/// The upstream html5lib TestData format: sections start with `#name` lines.
fn parse_dat(text: &str) -> Vec<Vec<(String, String)>> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = text.split('\n').collect();
    let mut tests = Vec::new();
    let mut data: Option<Vec<(String, String)>> = None;
    let mut key: Option<String> = None;
    let finish = |d: Vec<(String, String)>| -> Vec<(String, String)> {
        d.into_iter()
            .map(|(k, v)| {
                let v = v.strip_suffix('\n').map(str::to_owned).unwrap_or(v);
                (k, v)
            })
            .collect()
    };
    for (i, raw) in lines.iter().enumerate() {
        let line = if i == lines.len() - 1 {
            raw.to_string()
        } else {
            format!("{raw}\n")
        };
        let heading = line
            .strip_prefix('#')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_owned);
        if let Some(h) = heading {
            if let Some(mut d) = data.take() {
                if h == "description" || (h == "data" && key.as_deref() != Some("description")) {
                    if let Some(last) = d.last_mut() {
                        if !last.1.is_empty() {
                            last.1.pop();
                        }
                    }
                    tests.push(finish(d));
                } else {
                    data = Some(d);
                }
            }
            data.get_or_insert_with(Vec::new)
                .push((h.clone(), String::new()));
            key = Some(h);
        } else if key.is_some() {
            if let Some(d) = data.as_mut() {
                d.last_mut().expect("section").1.push_str(&line);
            }
        }
    }
    if let Some(d) = data {
        tests.push(finish(d));
    }
    tests
}

fn get<'a>(test: &'a [(String, String)], key: &str) -> Option<&'a str> {
    test.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn prefix_for(ns: &str) -> &'static str {
    match ns {
        "http://www.w3.org/1999/xhtml" => "html",
        "http://www.w3.org/1998/Math/MathML" => "math",
        "http://www.w3.org/2000/svg" => "svg",
        "http://www.w3.org/1999/xlink" => "xlink",
        "http://www.w3.org/XML/1998/namespace" => "xml",
        "http://www.w3.org/2000/xmlns/" => "xmlns",
        _ => "undefined",
    }
}

/// WPT `resources/test.js` `serializeTree`.
fn dump(dom: &Dom, node: NodeId, depth: usize, lines: &mut Vec<String>) {
    let pad = if depth > 0 {
        " ".repeat(2 * depth - 1)
    } else {
        String::new()
    };
    let inner = " ".repeat(2 * depth + 1);
    match dom.data(node) {
        NodeData::DocumentType(_) => {
            let (name, public, system) = dom.doctype(node).expect("doctype");
            if !name.is_empty() {
                if !public.is_empty() || !system.is_empty() {
                    lines.push(format!("|{pad}<!DOCTYPE {name} \"{public}\" \"{system}\">"));
                } else {
                    lines.push(format!("|{pad}<!DOCTYPE {name}>"));
                }
            } else {
                lines.push(format!("|{pad}<!DOCTYPE >"));
            }
        }
        NodeData::Document(_) => lines.push("#document".into()),
        NodeData::DocumentFragment { .. } | NodeData::ShadowRoot(_) => {
            lines.push("#document-fragment".into())
        }
        NodeData::Comment(c) => lines.push(format!("|{pad}<!-- {c} -->")),
        NodeData::ProcessingInstruction { target, data } => {
            lines.push(format!("|{pad}<?{target} {data}?>"))
        }
        NodeData::Text(t) | NodeData::CDataSection(t) => lines.push(format!("|{pad}\"{t}\"")),
        NodeData::Element(e) => {
            if dom.get_attribute(node, "data-skip").is_some() {
                return;
            }
            let ns = &*e.name().ns;
            let tag = if !ns.is_empty() && e.name().ns != ns!(html) {
                format!("{} {}", prefix_for(ns), &*e.name().local)
            } else {
                e.name().local.to_string()
            };
            lines.push(format!("|{pad}<{tag}>"));
            let mut attrs: Vec<(String, String)> = e
                .attrs()
                .iter()
                .map(|a| {
                    let name = if a.namespace().is_empty() {
                        a.local_name().to_string()
                    } else {
                        format!("{} {}", prefix_for(a.namespace()), &**a.local_name())
                    };
                    (name, a.value().to_owned())
                })
                .collect();
            attrs.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            for (n, v) in attrs {
                lines.push(format!("|{inner}{n}=\"{v}\""));
            }
            if e.name().ns == ns!(html) && &*e.name().local == "template" {
                if let Some(content) = dom.template_contents(node) {
                    lines.push(format!("|{inner}content"));
                    for c in dom.children(content) {
                        dump(dom, c, depth + 2, lines);
                    }
                }
            }
        }
        _ => {}
    }
    for c in dom.children(node) {
        dump(dom, c, depth + 1, lines);
    }
}

/// `node.normalize()`, including inside template contents (which `normalize` does not reach).
fn normalize_all(dom: &mut Dom, root: NodeId) {
    dom.normalize(root);
    let templates: Vec<NodeId> = dom
        .inclusive_descendants(root)
        .filter_map(|d| dom.template_contents(d))
        .collect();
    for t in templates {
        normalize_all(dom, t);
    }
}

fn run_case(data: &str, context: Option<&str>) -> String {
    let mut dom = Dom::new();
    let root = match context {
        None => dom.parse_html_str(data, ParseOptions::default()),
        Some(container) => {
            let doc = dom.create_document(DocumentKind::Html);
            let parts: Vec<&str> = container.split(' ').collect();
            let context = if parts.len() > 1 {
                let ns = match parts[0] {
                    "svg" => "http://www.w3.org/2000/svg",
                    _ => "http://www.w3.org/1998/Math/MathML",
                };
                dom.create_element_ns(doc, Some(ns), &format!("{}:{}", parts[0], parts[1]), None)
                    .expect("context element")
            } else {
                dom.create_element(doc, container, None)
                    .expect("context element")
            };
            dom.parse_fragment(context, data, false, ParseOptions::default())
        }
    };
    normalize_all(&mut dom, root);
    let mut lines = Vec::new();
    dump(&dom, root, 0, &mut lines);
    lines[0] = "#document".into();
    lines.join("\n")
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
}

#[test]
fn html5lib_tree_construction() {
    let dir = resources();
    let Ok(read) = std::fs::read_dir(&dir) else {
        eprintln!("html5lib: {} not found, skipping", dir.display());
        return;
    };
    let mut files: Vec<PathBuf> = read
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "dat"))
        .collect();
    files.sort();

    let known_text = include_str!("html5lib-known-failures.txt");
    let known: BTreeSet<&str> = known_text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| l.split(" \t# ").next().unwrap_or(l))
        .collect();

    let (mut passed, mut total) = (0, 0);
    let mut unexpected = Vec::new();
    let mut fixed = Vec::new();
    let mut report = String::new();
    for file in &files {
        let name = file.file_stem().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(file).expect("read .dat");
        let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        let (mut fp, mut ft) = (0, 0);
        for test in parse_dat(&text) {
            if get(&test, "script-off").is_some() {
                continue;
            }
            ft += 1;
            let data = get(&test, "data").unwrap_or("");
            let context = get(&test, "document-fragment");
            let expected = format!("#document\n{}", get(&test, "document").unwrap_or(""));
            let actual = std::panic::catch_unwind(|| run_case(data, context))
                .unwrap_or_else(|_| "PANIC".to_owned());
            let id = format!(
                "{name}: {}{}",
                escape(data),
                context
                    .map(|c| format!(" (fragment in {c})"))
                    .unwrap_or_default()
            );
            if actual == expected {
                fp += 1;
                if known.contains(id.as_str()) {
                    fixed.push(id);
                }
            } else {
                report.push_str(&format!(
                    "=== {id}\n--- expected\n{expected}\n--- actual\n{actual}\n\n"
                ));
                if !known.contains(id.as_str()) {
                    unexpected.push(id);
                }
            }
        }
        println!("{name:<40} {fp:>5} / {ft}");
        passed += fp;
        total += ft;
    }
    println!("{:<40} {passed:>5} / {total}", "TOTAL");
    let out = std::env::temp_dir().join("snowlark-dom-html5lib-failures.txt");
    let _ = std::fs::write(&out, &report);
    println!("failures: {}", out.display());
    assert!(
        unexpected.is_empty(),
        "{} unexpected failures (see {}):\n{}",
        unexpected.len(),
        out.display(),
        unexpected.join("\n")
    );
    assert!(
        fixed.is_empty(),
        "now passing, remove from the known list:\n{}",
        fixed.join("\n")
    );
}
