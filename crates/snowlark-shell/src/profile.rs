// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The profile folder: history, settings and the network's cookies and HSTS hosts.
//!
//! It is `%LOCALAPPDATA%\Snowlark\` (or `SNOWLARK_PROFILE_DIR`). Before the browser was renamed it was
//! `%LOCALAPPDATA%\Osmium\rust\`: the first start that finds that folder and no new one moves it
//! ([`adopt_old_profile`]), so bookmarks, history, settings, cookies and filter lists carry over.
//! `%LOCALAPPDATA%\Osmium\` itself is the C# engine's folder (its `cookies.json` and `hsts.json` are in
//! another format); its history and bookmarks are imported once into the user's own profile, since
//! their format is the same, never into one named by `SNOWLARK_PROFILE_DIR`.

use std::path::{Path, PathBuf};

pub use crate::settings::Settings;

#[derive(Clone, Debug)]
pub struct Profile {
    pub dir: PathBuf,
    /// Whether this is the user's own profile, the only one the C# engine's history and bookmarks
    /// belong to. A profile named by `SNOWLARK_PROFILE_DIR` or opened with [`Profile::at`] is someone
    /// else's (a test's, a tool's) and imports nothing.
    imports_legacy: bool,
}

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// The profile folder's place today, and before the browser was renamed.
fn new_location() -> PathBuf {
    local_app_data().join("Snowlark")
}

fn old_location() -> PathBuf {
    local_app_data().join("Osmium").join("rust")
}

/// What [`adopt`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adopted {
    /// There was nothing to do: the new folder is there, or no old one is.
    Nothing,
    /// The old folder is now the new one.
    Moved,
    /// The move failed (a file in use, another volume): the new folder is a copy and the old one is
    /// as it was.
    Copied,
    /// Neither worked. Nothing was changed; the old folder stays the profile until a later start
    /// succeeds.
    Failed,
}

/// Moves the profile of before the rename to today's place, when there is one and today's folder
/// does not exist yet. The browser process calls it once, before anything opens the profile; a
/// profile named by `SNOWLARK_PROFILE_DIR` is nobody's old one.
pub fn adopt_old_profile() -> Adopted {
    if std::env::var_os("SNOWLARK_PROFILE_DIR").is_some_and(|d| !d.is_empty()) {
        return Adopted::Nothing;
    }
    let (old, new) = (old_location(), new_location());
    let done = adopt(&old, &new, |a, b| std::fs::rename(a, b));
    if done == Adopted::Failed {
        eprintln!(
            "snowlark: the profile in {} could not be moved to {}; it is used where it is",
            old.display(),
            new.display()
        );
    }
    done
}

/// [`adopt_old_profile`] for the folders `old` and `new`, moving with `rename`. It never deletes
/// anything of `old`: a move takes the folder whole, and a copy leaves it as it was. The copy is
/// made beside `new` and renamed to it when complete, so `new` is never half a profile.
fn adopt(old: &Path, new: &Path, rename: impl Fn(&Path, &Path) -> std::io::Result<()>) -> Adopted {
    if new.exists() || !old.is_dir() {
        return Adopted::Nothing;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if rename(old, new).is_ok() {
        return Adopted::Moved;
    }
    let mut name = new.file_name().unwrap_or_default().to_owned();
    name.push(format!(".moving-{}", std::process::id()));
    let partial = new.with_file_name(name);
    let _ = std::fs::remove_dir_all(&partial);
    let copied = copy_tree(old, &partial).and_then(|()| std::fs::rename(&partial, new));
    match copied {
        Ok(()) => Adopted::Copied,
        Err(_) => {
            let _ = std::fs::remove_dir_all(&partial);
            // (another process of the browser may have finished the same move meanwhile)
            if new.is_dir() {
                Adopted::Nothing
            } else {
                Adopted::Failed
            }
        }
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The folder in use: today's, unless only the old one is there (its move has not been made yet, or
/// failed).
fn in_use(new: PathBuf, old: PathBuf) -> PathBuf {
    if !new.exists() && old.is_dir() {
        old
    } else {
        new
    }
}

impl Profile {
    pub fn default_location() -> Profile {
        match std::env::var_os("SNOWLARK_PROFILE_DIR") {
            Some(d) if !d.is_empty() => Profile::at(Path::new(&d)),
            _ => Profile {
                dir: in_use(new_location(), old_location()),
                imports_legacy: true,
            },
        }
    }

    pub fn at(dir: &Path) -> Profile {
        Profile {
            dir: dir.to_owned(),
            imports_legacy: false,
        }
    }

    pub fn history_file(&self) -> PathBuf {
        self.dir.join("history.json")
    }

    /// The C# engine's history, imported when the user's own profile has none yet.
    pub fn legacy_history(&self) -> Option<PathBuf> {
        let p = local_app_data().join("Osmium").join("history.json");
        (self.imports_legacy && p != self.history_file()).then_some(p)
    }

    pub fn settings_file(&self) -> PathBuf {
        self.dir.join("settings.json")
    }

    /// The download history ([`crate::downloads`]).
    pub fn downloads_file(&self) -> PathBuf {
        self.dir.join("downloads.json")
    }

    /// Where the minidumps of crashed renderer processes go (`snowlark_ipc::Launch::crash_dumps`); Chrome
    /// keeps its crash reports in the profile's `Crashpad/reports` folder.
    pub fn crash_dumps_dir(&self) -> PathBuf {
        self.dir.join("Crash Reports")
    }

    /// The window's size, position and maximized state ([`crate::window_state`]).
    pub fn window_file(&self) -> PathBuf {
        self.dir.join("window.json")
    }

    pub fn settings(&self) -> Settings {
        Settings::load(&self.settings_file())
    }

    pub fn bookmarks_file(&self) -> PathBuf {
        self.dir.join("bookmarks.json")
    }

    /// The C# engine's bookmarks (the same format), imported when the user's own profile has none yet.
    pub fn legacy_bookmarks(&self) -> Option<PathBuf> {
        let p = local_app_data().join("Osmium").join("bookmarks.json");
        (self.imports_legacy && p != self.bookmarks_file()).then_some(p)
    }

    /// `localStorage`: a file per origin ([`crate::local_storage`]).
    pub fn local_storage_dir(&self) -> PathBuf {
        self.dir.join("local-storage")
    }

    pub fn ensure(&self) {
        let _ = std::fs::create_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_and_files() {
        let dir = std::env::temp_dir().join(format!("snowlark-profile-{}", std::process::id()));
        let p = Profile::at(&dir);
        p.ensure();
        assert!(p.settings().homepage.is_none());
        std::fs::write(
            dir.join("settings.json"),
            r#"{"homepage":"https://example.com/","other":1}"#,
        )
        .unwrap();
        assert_eq!(
            p.settings().homepage.as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(p.history_file(), dir.join("history.json"));
        assert_eq!(p.bookmarks_file(), dir.join("bookmarks.json"));
        assert_eq!(p.downloads_file(), dir.join("downloads.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A profile opened at a folder of its own (a test's, or `SNOWLARK_PROFILE_DIR`) never takes the
    /// C# engine's history or bookmarks: they are the user's.
    #[test]
    fn a_profile_of_its_own_imports_nothing() {
        let dir = std::env::temp_dir().join(format!("snowlark-profile-own-{}", std::process::id()));
        let p = Profile::at(&dir);
        assert_eq!(p.legacy_history(), None);
        assert_eq!(p.legacy_bookmarks(), None);
        let own = Profile {
            dir: dir.clone(),
            imports_legacy: true,
        };
        assert!(own.legacy_history().is_some());
        assert!(own.legacy_bookmarks().is_some());
    }

    /// A folder to play the two profile places in, with an old profile of three files in it.
    fn old_profile(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("snowlark-adopt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let old = root.join("Osmium").join("rust");
        let lists = old.join("blocker").join("lists");
        std::fs::create_dir_all(&lists).unwrap();
        std::fs::write(old.join("bookmarks.json"), "[1]").unwrap();
        std::fs::write(old.join("cookies.json"), "{}").unwrap();
        std::fs::write(lists.join("easylist.txt"), "||ads^").unwrap();
        let new = root.join("Snowlark");
        (root, old, new)
    }

    /// Every file under `dir`, by its path from there, with its text.
    fn files(dir: &Path) -> Vec<(PathBuf, String)> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, String)>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else {
                    let name = p.strip_prefix(base).unwrap().to_owned();
                    out.push((name, std::fs::read_to_string(&p).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn refuse(_: &Path, _: &Path) -> std::io::Result<()> {
        Err(std::io::Error::other("in use"))
    }

    #[test]
    fn the_old_profile_is_moved_to_the_new_place_once() {
        let (root, old, new) = old_profile("move");
        let before = files(&old);
        assert_eq!(before.len(), 3);
        assert_eq!(in_use(new.clone(), old.clone()), old);
        assert_eq!(
            adopt(&old, &new, |a, b| std::fs::rename(a, b)),
            Adopted::Moved
        );
        assert!(!old.exists());
        assert_eq!(files(&new), before);
        assert_eq!(in_use(new.clone(), old.clone()), new);
        // The new folder is there: nothing more is moved, whatever appears at the old place.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("bookmarks.json"), "[2]").unwrap();
        assert_eq!(
            adopt(&old, &new, |a, b| std::fs::rename(a, b)),
            Adopted::Nothing
        );
        assert_eq!(files(&new), before);
        assert_eq!(in_use(new.clone(), old.clone()), new);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_profile_that_cannot_be_moved_is_copied_and_the_old_one_kept() {
        let (root, old, new) = old_profile("copy");
        let before = files(&old);
        assert_eq!(adopt(&old, &new, refuse), Adopted::Copied);
        assert_eq!(files(&old), before);
        assert_eq!(files(&new), before);
        // Nothing but the two profiles is left beside them.
        assert_eq!(names(&root), ["Osmium", "Snowlark"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_profile_that_can_be_neither_moved_nor_copied_stays_in_use_where_it_is() {
        use std::os::windows::fs::OpenOptionsExt;
        let (root, old, new) = old_profile("fail");
        let before = files(&old);
        // A file another program holds without sharing it cannot be read, so not copied.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(old.join("cookies.json"))
            .unwrap();
        assert_eq!(adopt(&old, &new, refuse), Adopted::Failed);
        drop(held);
        assert!(!new.exists());
        assert_eq!(names(&root), ["Osmium"]);
        assert_eq!(files(&old), before);
        assert_eq!(in_use(new.clone(), old.clone()), old);
        // The next start, with the file free, moves it.
        assert_eq!(
            adopt(&old, &new, |a, b| std::fs::rename(a, b)),
            Adopted::Moved
        );
        assert_eq!(files(&new), before);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_old_profile_is_nothing_to_do() {
        let root = std::env::temp_dir().join(format!("snowlark-adopt-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (old, new) = (root.join("Osmium").join("rust"), root.join("Snowlark"));
        assert_eq!(
            adopt(&old, &new, |a, b| std::fs::rename(a, b)),
            Adopted::Nothing
        );
        assert!(!new.exists() && !root.exists());
        assert_eq!(in_use(new.clone(), old), new);
    }
}
