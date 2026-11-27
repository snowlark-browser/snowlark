// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! File dialogs through the desktop portal's `org.freedesktop.portal.FileChooser`: `OpenFile`
//! (with `directory` for a folder) and `SaveFile`. Each call returns a request object that answers
//! later with its `Response` signal (`u` 0 for a choice, then `a{sv}` with the chosen `uris`); the
//! request's path is known before the call (`/org/freedesktop/portal/desktop/request/<sender>/<token>`),
//! so the signal is listened for first. Without a portal, `zenity` or `kdialog` asks when present;
//! with neither, nothing is chosen.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use dbus::arg::{prop_cast, PropMap, RefArg, Variant};

use crate::desktop_bus;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const CHOOSER: &str = "org.freedesktop.portal.FileChooser";

/// How long a dialog may stay open (the user answers it, so this only ends a portal that never does).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

/// What is asked for.
pub(super) enum Ask<'a> {
    Folder {
        start: Option<&'a Path>,
    },
    Save {
        suggested: &'a str,
        /// (description, patterns separated by `;`, as `*.pdf;*.txt`)
        filters: &'a [(&'a str, &'a str)],
        dir: Option<&'a Path>,
    },
}

/// Asks the user; `parent` is the window's exported handle (`wayland:<handle>`) or empty.
pub(super) fn ask(ask: &Ask, parent: &str) -> Option<PathBuf> {
    match portal(ask, parent) {
        Some(answer) => answer,
        None => fallback(ask),
    }
}

/// The portal's answer: `None` when there is no portal (or it failed before showing a dialog),
/// `Some(None)` when the user canceled.
fn portal(ask: &Ask, parent: &str) -> Option<Option<PathBuf>> {
    let conn = desktop_bus::session()?;
    if !desktop_bus::has_service(&conn, PORTAL) {
        return None;
    }
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let token = format!("snowlark{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let path = dbus::Path::new(request_path(&conn.unique_name(), &token)).ok()?;
    let (method, title, options) = request(ask, &token);
    let called = std::cell::Cell::new(false);
    let answer: Option<(u32, PropMap)> = desktop_bus::call_and_wait(
        &conn,
        PORTAL,
        &path,
        "org.freedesktop.portal.Request",
        "Response",
        ANSWER_TIMEOUT,
        || {
            let (handle,): (dbus::Path<'static>,) = desktop_bus::call(
                &conn,
                PORTAL,
                PORTAL_PATH,
                CHOOSER,
                method,
                (parent, title, options),
            )?;
            called.set(true);
            // (portals before 0.9 made the path up themselves; this one would never answer)
            (handle == path).then_some(())
        },
    );
    if !called.get() {
        return None;
    }
    // (a dialog was asked for: no answer is no choice, not a reason to ask again another way)
    Some(answer.and_then(|(code, results)| chosen(code, &results)))
}

/// The first chosen local file of a `Response`.
fn chosen(code: u32, results: &PropMap) -> Option<PathBuf> {
    if code != 0 {
        return None;
    }
    let uris = prop_cast::<Vec<String>>(results, "uris")?;
    let (files, _) = super::drag::parse_uri_list(&uris.join("\r\n"));
    files.into_iter().next()
}

/// The request object's path the portal will use: the sender's unique name without its `:` and
/// with `.` as `_`, then the token.
fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("{PORTAL_PATH}/request/{sender}/{token}")
}

/// The method, title and options of a request.
fn request(ask: &Ask, token: &str) -> (&'static str, &'static str, PropMap) {
    let mut options: PropMap = HashMap::new();
    let mut put = |k: &str, v: Box<dyn RefArg>| {
        options.insert(k.to_owned(), Variant(v));
    };
    put("handle_token", Box::new(token.to_owned()));
    put("modal", Box::new(true));
    match ask {
        Ask::Folder { start } => {
            put("directory", Box::new(true));
            if let Some(dir) = start {
                put("current_folder", Box::new(nul_ended(dir)));
            }
            ("OpenFile", "Choose a folder", options)
        }
        Ask::Save {
            suggested,
            filters,
            dir,
        } => {
            put("current_name", Box::new((*suggested).to_owned()));
            if let Some(dir) = dir {
                put("current_folder", Box::new(nul_ended(dir)));
            }
            if !filters.is_empty() {
                put("filters", Box::new(portal_filters(filters)));
            }
            ("SaveFile", "Save as", options)
        }
    }
}

/// Filters as the portal takes them, `a(sa(us))`: each a name and its glob patterns (kind 0).
fn portal_filters(filters: &[(&str, &str)]) -> Vec<(String, Vec<(u32, String)>)> {
    filters
        .iter()
        .map(|(name, patterns)| {
            let globs = patterns
                .split(';')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| (0, p.to_owned()))
                .collect();
            ((*name).to_owned(), globs)
        })
        .collect()
}

/// A path as the portal's `ay` folder: its bytes and a NUL.
fn nul_ended(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// Without a portal: `zenity`, else `kdialog`, when one is installed.
fn fallback(ask: &Ask) -> Option<PathBuf> {
    let (zenity, kdialog) = fallback_args(ask);
    for (program, args) in [("zenity", zenity), ("kdialog", kdialog)] {
        let Ok(out) = std::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        else {
            continue; // not installed
        };
        if !out.status.success() {
            return None; // canceled
        }
        let text = String::from_utf8(out.stdout).ok()?;
        let path = PathBuf::from(text.trim_end_matches('\n'));
        return path.is_absolute().then_some(path);
    }
    None
}

/// The arguments of `zenity` and of `kdialog` for a request.
fn fallback_args(ask: &Ask) -> (Vec<String>, Vec<String>) {
    match ask {
        Ask::Folder { start } => {
            let start = start.map(|d| d.display().to_string());
            let mut z = vec!["--file-selection".into(), "--directory".into()];
            if let Some(s) = &start {
                z.push(format!("--filename={s}/"));
            }
            let k = vec![
                "--getexistingdirectory".into(),
                start.unwrap_or_else(|| ".".into()),
            ];
            (z, k)
        }
        Ask::Save {
            suggested,
            filters,
            dir,
        } => {
            let file = match dir {
                Some(d) => d.join(suggested).display().to_string(),
                None => (*suggested).to_owned(),
            };
            let mut z = vec![
                "--file-selection".into(),
                "--save".into(),
                "--confirm-overwrite".into(),
                format!("--filename={file}"),
            ];
            for (name, patterns) in filters.iter() {
                z.push(format!(
                    "--file-filter={name} | {}",
                    patterns.replace(';', " ")
                ));
            }
            let kf = filters
                .iter()
                .map(|(name, patterns)| format!("{name} ({})", patterns.replace(';', " ")))
                .collect::<Vec<_>>()
                .join("\n");
            let mut k = vec!["--getsavefilename".into(), file];
            if !kf.is_empty() {
                k.push(kf);
            }
            (z, k)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_path_is_the_senders_name_and_the_token() {
        assert_eq!(
            request_path(":1.42", "snowlark3"),
            "/org/freedesktop/portal/desktop/request/1_42/snowlark3"
        );
    }

    #[test]
    fn filters_split_into_globs_and_folders_end_in_nul() {
        assert_eq!(
            portal_filters(&[("Documents", "*.pdf; *.txt")]),
            vec![(
                "Documents".to_owned(),
                vec![(0, "*.pdf".to_owned()), (0, "*.txt".to_owned())]
            )]
        );
        assert_eq!(nul_ended(Path::new("/home/a b")), b"/home/a b\0".to_vec());
    }

    #[test]
    fn a_response_gives_its_first_local_file() {
        let mut results: PropMap = HashMap::new();
        results.insert(
            "uris".into(),
            Variant(Box::new(vec!["file:///home/u/My%20File.zip".to_owned()])),
        );
        assert_eq!(
            chosen(0, &results),
            Some(PathBuf::from("/home/u/My File.zip"))
        );
        assert_eq!(chosen(1, &results), None, "canceled");
        assert_eq!(chosen(0, &HashMap::new()), None);
    }

    /// Against a running desktop portal (`SNOWLARK_TEST_PORTAL_DIALOG=1`) whose dialogs someone
    /// answers: Save in the Save dialog, then Cancel in the folder dialog. Save gives the suggested
    /// name in the given folder; Cancel, nothing.
    #[test]
    fn the_portals_dialogs_answer() {
        if std::env::var_os("SNOWLARK_TEST_PORTAL_DIALOG").is_none() {
            return;
        }
        let dir = std::env::temp_dir().join("snowlark-dialog-test");
        std::fs::create_dir_all(&dir).unwrap();
        let saved = portal(
            &Ask::Save {
                suggested: "a.pdf",
                filters: &[("PDF file", "*.pdf")],
                dir: Some(&dir),
            },
            "",
        );
        assert_eq!(saved, Some(Some(dir.join("a.pdf"))));
        let folder = portal(&Ask::Folder { start: Some(&dir) }, "");
        assert_eq!(folder, Some(None), "canceled");
    }

    #[test]
    fn save_requests_carry_the_name_folder_and_filters() {
        let ask = Ask::Save {
            suggested: "a.pdf",
            filters: &[("PDF file", "*.pdf")],
            dir: Some(Path::new("/tmp/d")),
        };
        let (method, _, options) = request(&ask, "t");
        assert_eq!(method, "SaveFile");
        assert_eq!(
            prop_cast::<String>(&options, "current_name").map(String::as_str),
            Some("a.pdf")
        );
        assert_eq!(
            prop_cast::<Vec<u8>>(&options, "current_folder").cloned(),
            Some(b"/tmp/d\0".to_vec())
        );
        assert!(options.contains_key("filters"));
        let (z, k) = fallback_args(&ask);
        assert!(z.contains(&"--filename=/tmp/d/a.pdf".to_owned()));
        assert!(z.contains(&"--file-filter=PDF file | *.pdf".to_owned()));
        assert_eq!(
            k[..2],
            ["--getsavefilename".to_owned(), "/tmp/d/a.pdf".to_owned()]
        );
    }
}
