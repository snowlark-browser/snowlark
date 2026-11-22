// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Moving a staged release in, as the browser starts.
//!
//! Windows does not let a running executable be overwritten, but it lets it be renamed. So, when a
//! release is staged and no other process of this executable runs: each of the install's files that
//! the release replaces is renamed aside (`<name>.update-old`), the release's file is copied in
//! beside it and renamed into place, and the new executable is started in this one's stead. Any
//! failure puts back what was moved, so the install is never left half replaced, and this start goes
//! on with the old version, the release still staged. The copies set aside are removed on a later
//! start ([`remove_old_copies`]), once nothing runs from them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{Paths, State};

/// The ending of an install's file set aside for a release.
const OLD: &str = "update-old";
/// The ending of a release's file being copied in.
const NEW: &str = "update-new";
/// How long the start waits for the other processes of the install to end (the browser that
/// restarted into this one is still closing).
pub const WAIT_FOR_OTHERS: Duration = Duration::from_secs(15);

/// What the start did.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// Nothing staged, or it could not be moved in now: this start goes on.
    No,
    /// The release is in and its executable started: this process ends.
    Started,
}

/// What [`at_start`] uses of the system (the real one, or a test's).
pub struct System<'a> {
    pub paths: &'a Paths,
    /// The running executable, in the folder the release goes into.
    pub exe: &'a Path,
    pub verify: &'a dyn Fn(&Path) -> Result<(), String>,
    /// Whether another process of the executable runs.
    pub others_running: &'a dyn Fn() -> bool,
    /// Starts the new executable.
    pub launch: &'a dyn Fn(&Path) -> std::io::Result<()>,
    pub wait: Duration,
}

/// Moves a staged release in and starts it (see the module documentation).
pub fn at_start(sys: &System) -> Applied {
    let mut state = State::load(sys.paths);
    let Some(staged) = state.staged.clone() else {
        return Applied::No;
    };
    let dir = sys.paths.staged_dir(&staged.name);
    let Some(install) = sys.exe.parent() else {
        return Applied::No;
    };
    let forget = |state: &mut State| {
        state.staged = None;
        let _ = state.save(sys.paths);
        let _ = std::fs::remove_dir_all(&dir);
    };
    if !dir.is_dir() || staged.files.is_empty() {
        forget(&mut state);
        return Applied::No;
    }
    let until = Instant::now() + sys.wait;
    while (sys.others_running)() {
        if Instant::now() >= until {
            return Applied::No;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // (checked again: the staged folder is the profile's, not the install's)
    if (sys.verify)(&dir.join("snowlark.exe")).is_err() {
        forget(&mut state);
        return Applied::No;
    }
    let moved = match swap(install, &dir, &staged.files) {
        Ok(m) => m,
        Err(_) => return Applied::No,
    };
    if (sys.launch)(&install.join("snowlark.exe")).is_err() {
        undo(install, &moved);
        return Applied::No;
    }
    forget(&mut state);
    Applied::Started
}

/// One file swapped: its name and whether the install had one (set aside).
type Moved = Vec<(String, bool)>;

fn aside(install: &Path, name: &str) -> PathBuf {
    install.join(format!("{name}.{OLD}"))
}

/// Puts each of `files` from `from` into `install`, setting aside the files they replace; on a
/// failure, puts everything back and gives the error.
fn swap(install: &Path, from: &Path, files: &[String]) -> std::io::Result<Moved> {
    let mut moved: Moved = Vec::new();
    for name in files {
        let target = install.join(name);
        let old = aside(install, name);
        let new = install.join(format!("{name}.{NEW}"));
        let step = (|| -> std::io::Result<bool> {
            if old.exists() {
                // (an old copy still in use cannot be removed: then nothing can be swapped now)
                std::fs::remove_file(&old)?;
            }
            let _ = std::fs::remove_file(&new);
            std::fs::copy(from.join(name), &new)?;
            let had = target.exists();
            if had {
                std::fs::rename(&target, &old)?;
            }
            if let Err(e) = std::fs::rename(&new, &target) {
                if had {
                    let _ = std::fs::rename(&old, &target);
                }
                return Err(e);
            }
            Ok(had)
        })();
        match step {
            Ok(had) => moved.push((name.clone(), had)),
            Err(e) => {
                let _ = std::fs::remove_file(&new);
                undo(install, &moved);
                return Err(e);
            }
        }
    }
    Ok(moved)
}

/// Puts back the files `moved` swapped.
fn undo(install: &Path, moved: &Moved) {
    for (name, had) in moved.iter().rev() {
        let target = install.join(name);
        if *had {
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::rename(aside(install, name), &target);
        } else {
            let _ = std::fs::remove_file(&target);
        }
    }
}

/// Removes the files a past update set aside in `install` (those still in use stay until a later
/// start).
pub fn remove_old_copies(install: &Path) {
    let Ok(entries) = std::fs::read_dir(install) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(&format!(".{OLD}")) || name.ends_with(&format!(".{NEW}")) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The start of the window: moves a staged release in and starts it with this process's
/// arguments ([`Applied::Started`]: this process ends). Nothing happens in a build that does not
/// update.
pub fn at_window_start(profile_dir: &Path) -> Applied {
    if super::own_release().is_none() {
        return Applied::No;
    }
    let paths = Paths::in_profile(profile_dir);
    // (the common case, nothing staged, reads one small file)
    if !paths.state_file().exists() {
        return Applied::No;
    }
    let Ok(exe) = std::env::current_exe() else {
        return Applied::No;
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    at_start(&System {
        paths: &paths,
        exe: &exe,
        verify: &super::verify_signer,
        others_running: &|| !snowlark_ipc::win::processes::others_running(&exe).is_empty(),
        launch: &|new| {
            std::process::Command::new(new)
                .args(&args)
                .spawn()
                .map(drop)
        },
        wait: WAIT_FOR_OTHERS,
    })
}
