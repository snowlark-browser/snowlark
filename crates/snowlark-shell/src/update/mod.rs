// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Updates: with the user's consent (the `updateCheck` setting, asked on the first start of the
//! window), a background check of the project's list of releases at most once a day, the download
//! of a newer release into the profile, and its switch on the next start.
//!
//! - Nothing is sent before the user said yes. The check is one request for the list of releases
//!   ([`FEED`]) with a generic User-Agent, no cookies and no other identifier; a refusal (the
//!   site's rate limit) is quietly left until the next day.
//! - The check runs on a thread of its own at background priority, a few seconds after the first
//!   window shows ([`FIRST_CHECK_AFTER`]); the window never waits for it.
//! - A newer release ([`feed::newer`]) is downloaded, and kept only when the zip's SHA-256 is the
//!   one published beside it and the `snowlark.exe` in it is validly signed by the signer of the
//!   running executable ([`verify_signer`]). It is unpacked into the profile's `update` folder
//!   ([`Paths`]) and the window says it is ready ([`Outcome::Staged`]).
//! - The next start moves it in ([`apply`]).
//! - A build knows its own release from the build (`SNOWLARK_RELEASE`, set by the release script);
//!   a build without one never checks. Only the window checks: the headless modes, and a run that
//!   opens only an Incognito window, never do.
//!
//! `SNOWLARK_UPDATE_FEED=<url>` points the check at another list (a test's local server), and with
//! it `SNOWLARK_UPDATE_RELEASE=<name>` stands for the build's release.

pub mod apply;
pub mod feed;
mod zip;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// This build's release name (`snowlark-preview-<date>-<commit>`), empty for a build that is not a
/// release's.
pub const RELEASE: &str = env!("SNOWLARK_RELEASE");
/// The list of releases.
pub const FEED: &str = "https://api.github.com/repos/snowlark-browser/snowlark/releases";
/// The releases' page, for a user who updates by hand.
pub const RELEASES_PAGE: &str = "https://github.com/snowlark-browser/snowlark/releases";
/// The User-Agent of the check and the downloads: the product, nothing about this copy.
pub const USER_AGENT: &str = "Snowlark-Update-Check";
/// How long after the first window shows the check starts.
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(5);
/// At most one check in this time.
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// The largest list of releases read.
const MAX_FEED: u64 = 4 << 20;
/// The largest release zip downloaded.
const MAX_ZIP: u64 = 1 << 30;

/// The release this build is, when it checks for updates at all: [`RELEASE`], or with
/// `SNOWLARK_UPDATE_FEED` set, `SNOWLARK_UPDATE_RELEASE`.
pub fn own_release() -> Option<String> {
    if std::env::var_os("SNOWLARK_UPDATE_FEED").is_some() {
        return std::env::var("SNOWLARK_UPDATE_RELEASE")
            .ok()
            .filter(|r| feed::release_date(r).is_some());
    }
    (feed::release_date(RELEASE).is_some()).then(|| RELEASE.to_owned())
}

/// The list of releases the check reads.
pub fn feed_url() -> String {
    std::env::var("SNOWLARK_UPDATE_FEED").unwrap_or_else(|_| FEED.to_owned())
}

/// `SNOWLARK_TRACE=1`: the updates' steps go to stderr (`[update]` lines).
pub fn trace() -> bool {
    std::env::var_os("SNOWLARK_TRACE").is_some_and(|v| v == "1")
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The profile's `update` folder: `state.json`, the download in progress, the staged release.
#[derive(Clone, Debug)]
pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    pub fn in_profile(profile_dir: &Path) -> Paths {
        Paths {
            dir: profile_dir.join("update"),
        }
    }

    pub fn state_file(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    fn download_dir(&self) -> PathBuf {
        self.dir.join("download")
    }

    /// Where release `name` is unpacked.
    pub fn staged_dir(&self, name: &str) -> PathBuf {
        self.dir.join("staged").join(name)
    }
}

/// What the profile keeps of the updates.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct State {
    /// When the list was last asked for (seconds since the Unix epoch).
    pub last_check: Option<u64>,
    /// A release downloaded, checked and unpacked, waiting for the next start.
    pub staged: Option<Staged>,
}

/// A staged release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Staged {
    /// Its release name (its folder's name too).
    pub name: String,
    /// Its page.
    pub page: String,
    /// The files it has, all of which replace the install's.
    pub files: Vec<String>,
}

impl State {
    pub fn load(paths: &Paths) -> State {
        std::fs::read(paths.state_file())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> std::io::Result<()> {
        std::fs::create_dir_all(&paths.dir)?;
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::restore::write_atomic(&paths.state_file(), &json)
    }

    /// Whether a check is due at `now`: never checked, a day or more since the last one, or the
    /// clock went back past it.
    pub fn due(&self, now: u64) -> bool {
        match self.last_check {
            None => true,
            Some(t) => now < t || now - t >= CHECK_EVERY.as_secs(),
        }
    }
}

/// How the check gets a URL's body (the network, or a test's).
pub trait Fetch {
    /// GETs `url` and gives its body to `sink` in parts; an error for a status other than 200, a
    /// body longer than `max`, or a network error.
    fn get(
        &self,
        url: &str,
        accept: &str,
        max: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
    ) -> Result<(), String>;

    fn get_all(&self, url: &str, accept: &str, max: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        self.get(url, accept, max, &mut |b| {
            out.extend_from_slice(b);
            Ok(())
        })?;
        Ok(out)
    }
}

/// [`Fetch`] over a network stack of its own: in memory only (no cookies, no cache, nothing of the
/// profile), with [`USER_AGENT`].
pub struct NetFetch {
    net: snowlark_net::NetThread,
}

impl NetFetch {
    pub fn start() -> Result<NetFetch, String> {
        let config = snowlark_net::NetConfig {
            profile_dir: None,
            user_agent: USER_AGENT.to_owned(),
            http_cache: false,
            ..snowlark_net::NetConfig::default()
        };
        let net = snowlark_net::NetThread::start(config).map_err(|e| e.message)?;
        Ok(NetFetch { net })
    }
}

impl Fetch for NetFetch {
    fn get(
        &self,
        url: &str,
        accept: &str,
        max: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        use snowlark_net::{CredentialsMode, FetchEvent, Referrer, Request, Url};
        let url = Url::parse(url).map_err(|e| e.to_string())?;
        let mut request = Request::new(url);
        request.credentials = CredentialsMode::Omit;
        request.referrer = Referrer::NoReferrer;
        request.initiator_type = "update-check".into();
        request.headers.append("Accept", accept);
        request.headers.append("User-Agent", USER_AGENT);
        let mut stream = self.net.client().fetch(request);
        let mut length = 0u64;
        while let Some(event) = stream.next_blocking() {
            match event {
                FetchEvent::Head(head) if head.status != 200 => {
                    return Err(format!("HTTP {}", head.status));
                }
                FetchEvent::Head(_) | FetchEvent::Violation(_) => {}
                FetchEvent::Body(chunk) => {
                    length += chunk.len() as u64;
                    if length > max {
                        return Err(format!("more than {max} bytes"));
                    }
                    sink(&chunk)?;
                }
                FetchEvent::Done => return Ok(()),
                FetchEvent::Error(e) => return Err(e.message),
            }
        }
        Err("the response ended early".into())
    }
}

/// Checks that `exe` is validly signed by the signer of the running executable.
pub fn verify_signer(exe: &Path) -> Result<(), String> {
    use snowlark_ipc::win::signing::signer_of;
    let own = std::env::current_exe().map_err(|e| e.to_string())?;
    let mine = signer_of(&own).map_err(|e| format!("this build: {e}"))?;
    let theirs = signer_of(exe).map_err(|e| format!("the update: {e}"))?;
    if mine.same_as(&theirs) {
        Ok(())
    } else {
        Err("the update is signed by another signer".into())
    }
}

/// What a check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing newer (or not due yet).
    Nothing,
    /// A newer release is staged for the next start.
    Staged(Staged),
    /// A newer release exists, but the folder the browser runs from cannot be written: it is
    /// updated by hand from `page`.
    CannotWrite { name: String, page: String },
    /// The check or the download failed (it is tried again the next day).
    Failed(String),
}

/// One check, as the background thread runs it.
pub struct Check<'a> {
    pub paths: &'a Paths,
    /// This build's release.
    pub own: &'a str,
    /// The folder the browser runs from.
    pub install_dir: &'a Path,
    pub feed: &'a str,
    pub fetch: &'a dyn Fetch,
    /// Checks the unpacked executable ([`verify_signer`]).
    pub verify: &'a dyn Fn(&Path) -> Result<(), String>,
    /// Still wanted (the setting can be turned off while the check runs).
    pub wanted: &'a dyn Fn() -> bool,
}

impl Check<'_> {
    /// Runs the check at `now`.
    pub fn run(&self, now: u64) -> Outcome {
        let mut state = State::load(self.paths);
        if let Some(s) = &state.staged {
            if self.paths.staged_dir(&s.name).is_dir() {
                return Outcome::Staged(s.clone());
            }
            state.staged = None;
        }
        if !state.due(now) || !(self.wanted)() {
            return Outcome::Nothing;
        }
        // (the time is kept first: a check that fails or hangs is not repeated the same day)
        state.last_check = Some(now);
        if let Err(e) = state.save(self.paths) {
            return Outcome::Failed(format!("the profile: {e}"));
        }
        let list = match self
            .fetch
            .get_all(self.feed, "application/vnd.github+json", MAX_FEED)
            .and_then(|b| feed::parse(&b))
        {
            Ok(l) => l,
            Err(e) => return Outcome::Failed(format!("the list of releases: {e}")),
        };
        let Some(release) = feed::newer(&list, self.own) else {
            return Outcome::Nothing;
        };
        if !writable(self.install_dir) {
            return Outcome::CannotWrite {
                name: release.name.clone(),
                page: release.page.clone(),
            };
        }
        if !(self.wanted)() {
            return Outcome::Nothing;
        }
        match self.stage(release) {
            Ok(staged) => {
                state.staged = Some(staged.clone());
                match state.save(self.paths) {
                    Ok(()) => Outcome::Staged(staged),
                    Err(e) => Outcome::Failed(format!("the profile: {e}")),
                }
            }
            Err(e) => Outcome::Failed(e),
        }
    }

    /// Downloads `release`, checks it and unpacks it into its staged folder.
    fn stage(&self, release: &feed::Release) -> Result<Staged, String> {
        let sums = self
            .fetch
            .get_all(&release.sha256_url, "*/*", 4096)
            .map_err(|e| format!("the checksum: {e}"))?;
        let want = feed::checksum(&sums).ok_or("the checksum file has no SHA-256")?;
        let dl = self.paths.download_dir();
        let _ = std::fs::remove_dir_all(&dl);
        std::fs::create_dir_all(&dl).map_err(|e| e.to_string())?;
        let zip_file = dl.join(format!("{}.zip", release.name));
        let result = self.download_and_unpack(release, &zip_file, &want);
        let _ = std::fs::remove_dir_all(&dl);
        result
    }

    fn download_and_unpack(
        &self,
        release: &feed::Release,
        zip_file: &Path,
        want: &str,
    ) -> Result<Staged, String> {
        use std::io::Write;
        let mut out = std::fs::File::create(zip_file).map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        self.fetch
            .get(
                &release.zip_url,
                "application/octet-stream",
                MAX_ZIP,
                &mut |b| {
                    hash.update(b);
                    out.write_all(b).map_err(|e| e.to_string())
                },
            )
            .map_err(|e| format!("the download: {e}"))?;
        out.sync_all().map_err(|e| e.to_string())?;
        drop(out);
        let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != want {
            return Err(format!("the download's SHA-256 is {got}, not {want}"));
        }
        let bytes = std::fs::read(zip_file).map_err(|e| e.to_string())?;
        let files = zip::files(&bytes).map_err(|e| format!("the zip: {e}"))?;
        drop(bytes);
        if !files
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("snowlark.exe"))
        {
            return Err("the zip has no snowlark.exe".into());
        }
        let dir = self.paths.staged_dir(&release.name);
        let tmp = dir.with_extension("unpacking");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&dir);
        let unpacked = (|| -> Result<Vec<String>, String> {
            std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
            let mut names = Vec::new();
            for (name, content) in &files {
                std::fs::write(tmp.join(name), content).map_err(|e| format!("{name}: {e}"))?;
                names.push(name.clone());
            }
            (self.verify)(&tmp.join("snowlark.exe"))?;
            std::fs::rename(&tmp, &dir).map_err(|e| e.to_string())?;
            Ok(names)
        })();
        match unpacked {
            Ok(files) => Ok(Staged {
                name: release.name.clone(),
                page: release.page.clone(),
                files,
            }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                Err(e)
            }
        }
    }
}

/// Whether files can be made and removed in `dir`.
pub fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".snowlark-update-probe-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The window's side of the updates: one per process, shared by its windows.
#[derive(Default)]
pub struct Updates {
    /// The consent prompt was shown in this run.
    pub asked: std::cell::Cell<bool>,
    /// The check's thread was started in this run.
    started: std::cell::Cell<bool>,
    /// The setting says yes (read by the check's thread).
    wanted: Arc<AtomicBool>,
    /// What the check found, once it has.
    outcome: Arc<Mutex<Option<Outcome>>>,
    /// The window has shown the outcome.
    pub told: std::cell::Cell<bool>,
    /// The user chose to restart into the update.
    pub restart: std::cell::Cell<bool>,
}

impl Updates {
    /// Whether this build checks for updates at all.
    pub fn available() -> bool {
        own_release().is_some()
    }

    /// The setting changed (or is read for the first time): `yes` starts the check's thread once,
    /// after [`FIRST_CHECK_AFTER`].
    pub fn set_wanted(&self, yes: bool, profile_dir: &Path) {
        self.wanted.store(yes, Ordering::Relaxed);
        if !yes || self.started.get() {
            return;
        }
        let Some(own) = own_release() else {
            return;
        };
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let Some(install_dir) = exe.parent().map(Path::to_owned) else {
            return;
        };
        self.started.set(true);
        let paths = Paths::in_profile(profile_dir);
        let wanted = self.wanted.clone();
        let outcome = self.outcome.clone();
        let _ = std::thread::Builder::new()
            .name("snowlark update check".into())
            .spawn(move || {
                snowlark_ipc::win::processes::background_thread();
                std::thread::sleep(FIRST_CHECK_AFTER);
                apply::remove_old_copies(&install_dir);
                let result = match NetFetch::start() {
                    Ok(fetch) => Check {
                        paths: &paths,
                        own: &own,
                        install_dir: &install_dir,
                        feed: &feed_url(),
                        fetch: &fetch,
                        verify: &verify_signer,
                        wanted: &|| wanted.load(Ordering::Relaxed),
                    }
                    .run(now()),
                    Err(e) => Outcome::Failed(e),
                };
                if trace() {
                    eprintln!("[update] check: {result:?}");
                }
                *outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            });
    }

    /// What the check found, when it has finished.
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests;
