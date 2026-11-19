// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the `snowlark-idl` binary does, as functions the tests can call: generating the formatted bindings
//! and comparing them with the checked-in ones.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The workspace root (two levels above this crate).
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// Where the generated bindings live.
pub fn generated_dir(root: &Path) -> PathBuf {
    root.join("crates/snowlark-bindings/src/generated")
}

/// Formats Rust source with rustfmt (edition 2021, LF), as `cargo fmt` leaves it.
pub fn rustfmt(src: &str) -> Result<String, String> {
    let mut child = Command::new("rustfmt")
        .args([
            "--edition",
            "2021",
            "--emit",
            "stdout",
            "--config",
            "newline_style=Unix",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run rustfmt: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = src.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    writer
        .join()
        .expect("rustfmt writer thread")
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

/// CSSOM §6.6.1 `CSSStyleProperties`: for each supported CSS property (the list in
/// `data/css-properties.txt`, which snowlark-style's test keeps equal to stylo's), the camel-cased
/// attribute, the webkit-cased attribute (for `-webkit-` properties) and the dashed attribute (for
/// properties with a dash), as IDL that marks each with the property it stands for.
pub fn css_properties_idl(list: &str) -> String {
    // CSSOM "CSS property to IDL attribute".
    fn to_idl(property: &str, lowercase_first: bool) -> String {
        let mut out = String::new();
        let mut upper = false;
        let p = if lowercase_first {
            &property[1..]
        } else {
            property
        };
        for c in p.chars() {
            if c == '-' {
                upper = true;
            } else if upper {
                out.push(c.to_ascii_uppercase());
                upper = false;
            } else {
                out.push(c);
            }
        }
        out
    }
    let mut seen = std::collections::BTreeSet::new();
    seen.insert("cssFloat".to_string());
    let mut out = String::from("partial interface CSSStyleProperties {\n");
    for property in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut names = vec![to_idl(property, false)];
        if property.starts_with("-webkit-") {
            names.push(to_idl(property, true));
        }
        if property.contains('-') {
            names.push(property.to_string());
        }
        for name in names {
            if seen.insert(name.clone()) {
                out.push_str(&format!(
                    "  [CEReactions, SnowlarkCssProperty=\"{property}\"] attribute [LegacyNullToEmptyString] CSSOMString {name};\n"
                ));
            }
        }
    }
    out.push_str("};\n");
    out
}

/// Parses and links `idl/` with the synthesized IDL (the CSS property attributes) and the legacy
/// members no specification has (`data/legacy.idl`).
pub fn load(root: &Path) -> std::io::Result<(crate::model::Model, Vec<String>, usize)> {
    let list = std::fs::read_to_string(root.join("crates/snowlark-idl/data/css-properties.txt"))?;
    let legacy = std::fs::read_to_string(root.join("crates/snowlark-idl/data/legacy.idl"))?;
    crate::model::load_dir_with(
        &root.join("idl"),
        &[
            ("css-properties.idl".into(), css_properties_idl(&list)),
            ("legacy.idl".into(), legacy),
        ],
    )
}

/// Parses and links `idl/`, generates the bindings and formats them. Fails unless the IDL parses and
/// links without diagnostics.
pub fn generate(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let (model, errors, _) = load(root).map_err(|e| e.to_string())?;
    if !errors.is_empty() || !model.diagnostics.is_empty() {
        return Err(format!(
            "the IDL does not parse and link cleanly:\n{}\n{}",
            errors.join("\n"),
            model.diagnostics.join("\n")
        ));
    }
    let enumerated = std::fs::read_to_string(root.join("crates/snowlark-idl/data/enumerated.txt"))
        .map_err(|e| e.to_string())?;
    let files = crate::codegen::generate(&model, &enumerated);
    // rustfmt is the slow part: format the files in parallel.
    let handles: Vec<_> = files
        .into_iter()
        .map(|(name, src)| {
            std::thread::spawn(move || {
                if name.ends_with(".rs") {
                    rustfmt(&src)
                        .map(|f| (name.clone(), f))
                        .map_err(|e| format!("{name}: {e}"))
                } else {
                    Ok((name, src))
                }
            })
        })
        .collect();
    let mut out = BTreeMap::new();
    for h in handles {
        let (name, src) = h.join().expect("format thread")?;
        out.insert(name, src);
    }
    Ok(out)
}

/// The files currently in a directory, by name.
pub fn existing(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                out.insert(name, std::fs::read_to_string(&p).unwrap_or_default());
            }
        }
    }
    out
}

/// What differs between freshly generated files and the checked-in ones (empty when up to date).
pub fn stale(generated: &BTreeMap<String, String>, dir: &Path) -> Vec<String> {
    let have = existing(dir);
    let mut out = Vec::new();
    for (name, src) in generated {
        if have.get(name) != Some(src) {
            out.push(format!("changed: {name}"));
        }
    }
    for name in have.keys() {
        if !generated.contains_key(name) {
            out.push(format!("not generated: {name}"));
        }
    }
    out
}
