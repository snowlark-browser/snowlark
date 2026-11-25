// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The browser's working directory.
//!
//! A graphics driver's user-mode part (NVIDIA's, loaded with the first Direct3D device) creates a
//! `NVIDIA Corporation\umdlogs` folder in the process's current directory. A browser started from a
//! shortcut or a terminal would leave it beside its executable or in whatever folder the user was in.
//! The window processes (the browser, its renderers and GPU process, which inherit the browser's
//! directory) therefore move to a private folder in the system's temporary folder before anything loads
//! a driver; the driver's folder, when it is made at all, is made there.
//!
//! The command-line argument that names a file is made absolute first, so it still means the same file.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The private folder's name inside the temporary folder.
const FOLDER: &str = "snowlark-cwd";

/// Moves this process's current directory to the private folder under the system's temporary folder
/// (made when missing). Returns the folder, or `None` when none could be made or entered (the
/// directory is then unchanged).
pub fn leave_working_directory() -> Option<PathBuf> {
    leave_working_directory_in(&std::env::temp_dir())
}

/// [`leave_working_directory`] with the temporary folder given.
pub fn leave_working_directory_in(temp: &Path) -> Option<PathBuf> {
    let dir = temp.join(FOLDER);
    std::fs::create_dir_all(&dir).ok()?;
    std::env::set_current_dir(&dir).ok()?;
    Some(dir)
}

/// `start`, the address or path the browser was started with, made absolute when it names an existing
/// file or folder relative to the current directory; anything else (a URL, a search, an absolute path) is
/// returned as it is.
pub fn absolute_start(start: &str) -> OsString {
    let path = Path::new(start);
    if path.is_relative() && path.exists() {
        if let Ok(abs) = std::path::absolute(path) {
            return abs.into_os_string();
        }
    }
    OsString::from(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The process of the test below: it leaves, then writes where it is.
    #[test]
    #[ignore = "run by a_process_that_leaves_ends_in_the_private_folder"]
    fn the_leaving_process() {
        let (Some(temp), Some(out)) = (
            std::env::var_os("SNOWLARK_TEST_TEMP"),
            std::env::var_os("SNOWLARK_TEST_OUT"),
        ) else {
            return;
        };
        let dir = leave_working_directory_in(Path::new(&temp)).expect("the folder");
        let here = std::env::current_dir().unwrap();
        // A relative path now means a file of the private folder, not of where the process started.
        std::fs::write(out, format!("{}\n{}", dir.display(), here.display())).unwrap();
    }

    #[test]
    fn a_process_that_leaves_ends_in_the_private_folder_and_the_start_folder_stays_clean() {
        let base = std::env::temp_dir().join(format!("snowlark-cwd-test-{}", std::process::id()));
        let started_in = base.join("started");
        let temp = base.join("temp");
        std::fs::create_dir_all(&started_in).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let out = base.join("out.txt");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "working_dir::tests::the_leaving_process",
            ])
            .current_dir(&started_in)
            .env("SNOWLARK_TEST_TEMP", &temp)
            .env("SNOWLARK_TEST_OUT", &out)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let written = std::fs::read_to_string(&out).unwrap();
        let mut lines = written.lines();
        let dir = PathBuf::from(lines.next().unwrap());
        let here = PathBuf::from(lines.next().unwrap());
        assert_eq!(dir, temp.join(FOLDER));
        assert_eq!(
            std::fs::canonicalize(&here).unwrap(),
            std::fs::canonicalize(&dir).unwrap()
        );
        // The folder the process started in is not the one a driver writes to.
        assert_eq!(std::fs::read_dir(&started_in).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_start_that_names_a_file_is_made_absolute_and_the_rest_is_kept() {
        let name = format!("snowlark-cwd-start-{}.html", std::process::id());
        let file = std::env::current_dir().unwrap().join(&name);
        std::fs::write(&file, "x").unwrap();
        let made = absolute_start(&name);
        assert!(Path::new(&made).is_absolute());
        assert!(Path::new(&made).ends_with(&name));
        let _ = std::fs::remove_file(&file);
        assert_eq!(
            absolute_start("https://example.com/a"),
            "https://example.com/a"
        );
        assert_eq!(absolute_start("no-such-file.html"), "no-such-file.html");
        assert_eq!(absolute_start("C:\\x\\y.html"), "C:\\x\\y.html");
    }
}
