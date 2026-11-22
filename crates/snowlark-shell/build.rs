// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Records which build this is, for the About page: the commit's short hash and date (empty outside a
//! git checkout, such as a source archive), when the build was made, and the release it is made for
//! (`SNOWLARK_RELEASE`, empty for any other build).
//!
//! The build time is that of the last build that had something to compile: this script runs again
//! when a source file changes or a commit is made, not on a build with nothing to do.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `YYYY-MM-DD HH:MM UTC` for seconds since the Unix epoch (days to civil date: Howard Hinnant's
/// `civil_from_days`).
fn utc_text(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (hour, minute) = (secs % 86_400 / 3600, secs % 3600 / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

fn main() {
    // (a new commit moves HEAD's ref, or HEAD itself when detached; a worktree has its own HEAD)
    for path in [
        git(&["rev-parse", "--git-path", "HEAD"]),
        git(&["symbolic-ref", "-q", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r])),
        git(&["rev-parse", "--git-path", "packed-refs"]),
    ]
    .into_iter()
    .flatten()
    {
        println!("cargo:rerun-if-changed={path}");
    }
    // (every source the browser is built from, so that the build time is not an old one)
    for path in ["..", "../../vendor", "../../Cargo.lock"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    let date = git(&["log", "-1", "--format=%cs"]).unwrap_or_default();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    println!("cargo:rustc-env=SNOWLARK_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=SNOWLARK_GIT_DATE={date}");
    println!("cargo:rustc-env=SNOWLARK_BUILD_TIME={}", utc_text(now));
    // The release this build is made for (the release script sets it): which updates are newer.
    println!("cargo:rerun-if-env-changed=SNOWLARK_RELEASE");
    let release = std::env::var("SNOWLARK_RELEASE").unwrap_or_default();
    println!("cargo:rustc-env=SNOWLARK_RELEASE={release}");
}
