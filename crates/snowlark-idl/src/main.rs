// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark-idl report` prints parse and link results for `idl/`.
//! `snowlark-idl gen` regenerates `crates/snowlark-bindings/src/generated/`.
//! `snowlark-idl check` fails if regenerating would change anything (what CI runs).

use snowlark_idl::tool;
use std::process::ExitCode;

fn report() -> ExitCode {
    let root = tool::workspace_root();
    let (model, errors, count) = tool::load(&root).expect("read idl/");
    println!("{count} files, {} failed to parse", errors.len());
    for e in &errors {
        println!("  {e}");
    }
    println!(
        "{} interfaces, {} mixins, {} dictionaries, {} enums, {} typedefs, {} callbacks",
        model.interfaces.len(),
        model.mixins.len(),
        model.dictionaries.len(),
        model.enums.len(),
        model.typedefs.len(),
        model.callbacks.len()
    );
    println!("{} link diagnostics", model.diagnostics.len());
    for d in &model.diagnostics {
        println!("  {d}");
    }
    if errors.is_empty() && model.diagnostics.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    let root = tool::workspace_root();
    let dir = tool::generated_dir(&root);
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "report".into());
    match cmd.as_str() {
        "report" => report(),
        "gen" | "check" => {
            let files = match tool::generate(&root) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            };
            if cmd == "check" {
                let stale = tool::stale(&files, &dir);
                if stale.is_empty() {
                    println!("generated bindings are up to date ({} files)", files.len());
                    return ExitCode::SUCCESS;
                }
                eprintln!("generated bindings are out of date; run `snowlark-idl gen`:");
                for s in stale {
                    eprintln!("  {s}");
                }
                return ExitCode::FAILURE;
            }
            std::fs::create_dir_all(&dir).expect("create the generated directory");
            for name in tool::existing(&dir).keys() {
                if !files.contains_key(name) {
                    std::fs::remove_file(dir.join(name)).expect("remove a stale file");
                }
            }
            let lines: usize = files.values().map(|s| s.lines().count()).sum();
            for (name, src) in &files {
                std::fs::write(dir.join(name), src).expect("write a generated file");
            }
            println!(
                "wrote {} files ({lines} lines) to {}",
                files.len(),
                dir.display()
            );
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("usage: snowlark-idl [report|gen|check] (got {other})");
            ExitCode::from(2)
        }
    }
}
