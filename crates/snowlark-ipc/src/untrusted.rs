// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `Untrusted<T>`: everything off a less privileged process's pipe arrives wrapped, and the only way
//! to the `T` is [`Untrusted::validate`], which takes the sender's [`ProcessPolicy`]. The field is
//! private and there is no other accessor, so a forgotten check does not compile.

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// What the browser knows about a process. It says who the sender is; the sender's own word is never
/// asked. M9 adds the frame table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessPolicy {
    /// The browser's number for the process (not its operating system id, which gets reused).
    pub process: u64,
    pub kind: ProcessKind,
    /// The site this process is locked to, shared by every handle of its connection.
    pub lock: Arc<SiteLock>,
}

/// A process's site lock (M5 step 4): the site its documents are of, set once by the browser at
/// the first commit of a document with a site and never changed (a document of another site
/// commits in another process), and the sites of the child navigables the browser itself delivered
/// to the process, which share it until out-of-process frames (M9). Only the browser's own
/// knowledge goes in; the process's word never does.
#[derive(Debug, Default)]
pub struct SiteLock {
    site: OnceLock<String>,
    hosted: Mutex<HashSet<String>>,
    /// The browser shows windows the process's page hosts (auxiliary browsing contexts, with an
    /// opener): their navigations are top-level, and their documents' sites, hosted as a child
    /// navigable's are, are top-level sites of the process too, until out-of-process frames (M9).
    windows: std::sync::atomic::AtomicBool,
    /// Checks that failed while the lock is in shadow mode (logged, not enforced).
    noted: AtomicU64,
}

impl SiteLock {
    /// Locks the process to `site`. A lock already set to another site stays, and is the error.
    pub fn lock_to(&self, site: &str) -> Result<(), String> {
        match self.site.get() {
            Some(have) if have != site => Err(format!("locked to {have}, not {site}")),
            Some(_) => Ok(()),
            None => {
                let _ = self.site.set(site.to_owned());
                Ok(())
            }
        }
    }

    /// The site the process is locked to (`None`: not locked yet, which admits any site).
    pub fn site(&self) -> Option<&str> {
        self.site.get().map(String::as_str)
    }

    /// The browser delivered a child navigable's document of `site` to this process.
    pub fn host(&self, site: &str) {
        self.hosted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(site.to_owned());
    }

    /// The browser shows a window the process's page hosts ([`SiteLock::hosts_windows`]).
    pub fn host_windows(&self) {
        self.windows.store(true, Ordering::Release);
    }

    /// Whether the browser has shown a window the process's page hosts: the process navigates
    /// top-level traversables of its own then.
    pub fn hosts_windows(&self) -> bool {
        self.windows.load(Ordering::Acquire)
    }

    /// Whether `site` may be the site of a top-level document of the process: the lock's (or any,
    /// not locked yet), or a hosted one's when the process hosts windows.
    pub fn admits_top_level(&self, site: &str) -> bool {
        match self.site() {
            None => true,
            Some(own) if own == site => true,
            Some(_) => self.hosts_windows() && self.admits(Some(site)),
        }
    }

    /// Whether data of `site` (`None`: an opaque origin, which is no site's) may reach the process:
    /// not locked yet, the lock's own site, or a hosted child navigable's.
    pub fn admits(&self, site: Option<&str>) -> bool {
        let Some(site) = site else {
            return true;
        };
        match self.site() {
            None => true,
            Some(own) if own == site => true,
            Some(_) => self
                .hosted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(site),
        }
    }

    /// Counts a check that failed in shadow mode; the count so far.
    pub fn note(&self) -> u64 {
        self.noted.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The checks that failed in shadow mode.
    pub fn noted(&self) -> u64 {
        self.noted.load(Ordering::Relaxed)
    }
}

impl PartialEq for SiteLock {
    fn eq(&self, other: &SiteLock) -> bool {
        self.site() == other.site()
            && *self.hosted.lock().unwrap_or_else(|e| e.into_inner())
                == *other.hosted.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Eq for SiteLock {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessKind {
    Renderer,
    /// A renderer for `snowlark:` pages, the only kind that may ask for internal data.
    InternalRenderer,
    Gpu,
    Media,
}

/// Why a message was refused. The sender is compromised or broken, so it dies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadMessage(pub String);

impl fmt::Display for BadMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A message type's checks. There is no default: each type says what a sender may send.
pub trait Validate: Sized {
    fn validate(self, policy: &ProcessPolicy) -> Result<Self, BadMessage>;
}

/// A value from a process that may be compromised.
pub struct Untrusted<T>(T);

impl<T> Untrusted<T> {
    pub(crate) fn new(value: T) -> Untrusted<T> {
        Untrusted(value)
    }
}

impl<T: Validate> Untrusted<T> {
    /// The only way to the value. On `Err` the caller reports it, and the sender dies
    /// ([`crate::Connection::bad_message`], or [`crate::Connection::check`] for both steps).
    pub fn validate(self, policy: &ProcessPolicy) -> Result<T, BadMessage> {
        self.0.validate(policy)
    }
}

impl<T> fmt::Debug for Untrusted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Untrusted(..)")
    }
}

/// Which side a connection hears from, and so what it hands out.
pub trait Trust: Send + Sync + 'static {
    /// How a value from the peer reaches the caller.
    type Wrap<T>;
    fn wrap<T>(value: T) -> Self::Wrap<T>;
    /// Whether this end is the browser: it kills a peer that breaks the rules, and serves sections.
    const BROWSER: bool;
}

/// The browser's end: the peer is a child, and may be compromised.
pub struct FromChild;

impl Trust for FromChild {
    type Wrap<T> = Untrusted<T>;
    fn wrap<T>(value: T) -> Untrusted<T> {
        Untrusted::new(value)
    }
    const BROWSER: bool = true;
}

/// A child's end: the peer is the browser, more privileged than this process and so trusted by it.
pub struct FromBrowser;

impl Trust for FromBrowser {
    type Wrap<T> = T;
    fn wrap<T>(value: T) -> T {
        value
    }
    const BROWSER: bool = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Open(String);

    impl Validate for Open {
        fn validate(self, policy: &ProcessPolicy) -> Result<Open, BadMessage> {
            if self.0.starts_with("snowlark:") && policy.kind != ProcessKind::InternalRenderer {
                return Err(BadMessage("snowlark: from a web renderer".into()));
            }
            Ok(self)
        }
    }

    fn policy(kind: ProcessKind) -> ProcessPolicy {
        ProcessPolicy {
            process: 1,
            kind,
            lock: Default::default(),
        }
    }

    #[test]
    fn the_lock_is_set_once_and_admits_its_site_and_its_hosted_frames() {
        let lock = SiteLock::default();
        assert!(lock.admits(Some("https://any.test")));
        assert!(lock.admits(None));
        assert_eq!(lock.lock_to("https://a.test"), Ok(()));
        assert_eq!(lock.lock_to("https://a.test"), Ok(()));
        assert!(lock.lock_to("https://b.test").is_err());
        assert_eq!(lock.site(), Some("https://a.test"));
        assert!(lock.admits(Some("https://a.test")));
        assert!(lock.admits(None));
        assert!(!lock.admits(Some("https://b.test")));
        lock.host("https://b.test");
        assert!(lock.admits(Some("https://b.test")));
        assert!(!lock.admits(Some("file://")));
        assert_eq!(lock.note(), 1);
        assert_eq!(lock.noted(), 1);
    }

    #[test]
    fn the_policy_decides() {
        let web = policy(ProcessKind::Renderer);
        let internal = policy(ProcessKind::InternalRenderer);
        let wrap = |s: &str| Untrusted::new(Open(s.into()));
        assert!(wrap("https://a.example/").validate(&web).is_ok());
        assert!(wrap("snowlark:settings").validate(&web).is_err());
        assert!(wrap("snowlark:settings").validate(&internal).is_ok());
    }

    #[test]
    fn debug_output_shows_nothing_of_the_value() {
        assert_eq!(format!("{:?}", Untrusted::new(5)), "Untrusted(..)");
    }
}
