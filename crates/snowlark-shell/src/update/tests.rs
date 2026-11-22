// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::apply::{self, Applied, System};
use super::*;

const OWN: &str = "snowlark-preview-2026-11-22-c1cb0259";
const NEXT: &str = "snowlark-preview-2026-11-24-0123abcd";

/// A folder of its own under the temporary folder, removed when dropped.
struct Temp(PathBuf);

impl Temp {
    fn new(what: &str) -> Temp {
        let dir =
            std::env::temp_dir().join(format!("snowlark-update-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Temp(dir)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn release_zip() -> Vec<u8> {
    zip::tests::make(&[
        (&format!("{NEXT}/snowlark.exe"), b"MZ the new program"),
        (&format!("{NEXT}/README.txt"), b"the new read me"),
    ])
}

fn feed_json(base: &str) -> String {
    format!(
        r#"[{{"tag_name":"preview-2026-11-24","html_url":"{base}/page","draft":false,"prerelease":true,"published_at":"2026-11-24T09:00:00Z","assets":[{{"name":"{NEXT}.zip","size":1,"browser_download_url":"{base}/{NEXT}.zip"}},{{"name":"{NEXT}.zip.sha256","size":1,"browser_download_url":"{base}/{NEXT}.zip.sha256"}}]}},{{"tag_name":"preview-2026-11-22","html_url":"{base}/old","draft":false,"prerelease":true,"published_at":"2026-11-22T08:09:31Z","assets":[{{"name":"{OWN}.zip","size":1,"browser_download_url":"{base}/{OWN}.zip"}},{{"name":"{OWN}.zip.sha256","size":1,"browser_download_url":"{base}/{OWN}.zip.sha256"}}]}}]"#
    )
}

/// A [`Fetch`] from a table of URL endings, counting its requests.
struct Table {
    entries: Vec<(String, Vec<u8>)>,
    requests: Cell<usize>,
}

impl Table {
    fn release(sha: &str) -> Table {
        let base = "https://example.test";
        Table {
            entries: vec![
                ("/releases".into(), feed_json(base).into_bytes()),
                (
                    format!("{NEXT}.zip.sha256"),
                    format!("{sha}  {NEXT}.zip\n").into_bytes(),
                ),
                (format!("{NEXT}.zip"), release_zip()),
            ],
            requests: Cell::new(0),
        }
    }
}

impl Fetch for Table {
    fn get(
        &self,
        url: &str,
        _accept: &str,
        max: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        self.requests.set(self.requests.get() + 1);
        let (_, body) = self
            .entries
            .iter()
            .find(|(end, _)| url.ends_with(end.as_str()))
            .ok_or("HTTP 404")?;
        if body.len() as u64 > max {
            return Err("too long".into());
        }
        sink(body)
    }
}

struct Setup {
    _root: Temp,
    paths: Paths,
    install: PathBuf,
}

fn setup(what: &str) -> Setup {
    let root = Temp::new(what);
    let paths = Paths::in_profile(&root.0.join("profile"));
    let install = root.0.join("install");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join("snowlark.exe"), b"MZ the old program").unwrap();
    std::fs::write(install.join("README.txt"), b"the old read me").unwrap();
    Setup {
        _root: root,
        paths,
        install,
    }
}

fn accept(_: &Path) -> Result<(), String> {
    Ok(())
}

fn check<'a>(
    s: &'a Setup,
    fetch: &'a dyn Fetch,
    verify: &'a dyn Fn(&Path) -> Result<(), String>,
) -> Check<'a> {
    Check {
        paths: &s.paths,
        own: OWN,
        install_dir: &s.install,
        feed: "https://example.test/releases",
        fetch,
        verify,
        wanted: &|| true,
    }
}

#[test]
fn a_check_is_due_once_a_day() {
    let day = CHECK_EVERY.as_secs();
    let mut state = State::default();
    assert!(state.due(1_000_000));
    state.last_check = Some(1_000_000);
    assert!(!state.due(1_000_000));
    assert!(!state.due(1_000_000 + day - 1));
    assert!(state.due(1_000_000 + day));
    // (the clock went back)
    assert!(state.due(999_999));

    // A check that found nothing is not made again the same day.
    let s = setup("daily");
    let fetch = Table::release(&sha256_hex(&release_zip()));
    let mut c = check(&s, &fetch, &accept);
    c.own = NEXT;
    assert_eq!(c.run(5_000_000), Outcome::Nothing);
    assert_eq!(fetch.requests.get(), 1);
    assert_eq!(c.run(5_000_000 + day - 60), Outcome::Nothing);
    assert_eq!(fetch.requests.get(), 1);
    assert_eq!(c.run(5_000_000 + day), Outcome::Nothing);
    assert_eq!(fetch.requests.get(), 2);
}

#[test]
fn nothing_is_requested_before_the_user_said_yes() {
    let s = setup("consent");
    let fetch = Table::release(&sha256_hex(&release_zip()));
    let mut c = check(&s, &fetch, &accept);
    c.wanted = &|| false;
    assert_eq!(c.run(5_000_000), Outcome::Nothing);
    assert_eq!(fetch.requests.get(), 0);
    // The window's side starts no thread for "no" (nor for a build that does not update).
    let updates = Updates::default();
    updates.set_wanted(false, &s.paths.dir);
    assert!(!updates.started.get());
    // The setting is off until it is answered.
    assert_eq!(crate::settings::Settings::default().update_check, None);
}

#[test]
fn a_newer_release_is_downloaded_checked_and_staged() {
    let s = setup("stage");
    let fetch = Table::release(&sha256_hex(&release_zip()));
    let verified = Cell::new(0);
    let verify = |exe: &Path| {
        verified.set(verified.get() + 1);
        assert_eq!(std::fs::read(exe).unwrap(), b"MZ the new program");
        Ok(())
    };
    let outcome = check(&s, &fetch, &verify).run(5_000_000);
    let Outcome::Staged(staged) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(staged.name, NEXT);
    assert_eq!(staged.files, ["snowlark.exe", "README.txt"]);
    assert_eq!(verified.get(), 1);
    let dir = s.paths.staged_dir(NEXT);
    assert_eq!(
        std::fs::read(dir.join("README.txt")).unwrap(),
        b"the new read me"
    );
    // (the zip is gone; the state says what is staged)
    assert!(!s.paths.dir.join("download").exists());
    assert_eq!(State::load(&s.paths).staged, Some(staged.clone()));
    // A later check reports it again without asking anything.
    let before = fetch.requests.get();
    assert_eq!(
        check(&s, &fetch, &verify).run(5_000_001),
        Outcome::Staged(staged)
    );
    assert_eq!(fetch.requests.get(), before);
}

#[test]
fn a_download_whose_checksum_differs_is_deleted() {
    let s = setup("checksum");
    let fetch = Table::release(&"0".repeat(64));
    let outcome = check(&s, &fetch, &accept).run(5_000_000);
    assert!(
        matches!(&outcome, Outcome::Failed(e) if e.contains("SHA-256")),
        "{outcome:?}"
    );
    assert!(!s.paths.dir.join("download").exists());
    assert!(!s.paths.staged_dir(NEXT).exists());
    assert_eq!(State::load(&s.paths).staged, None);
}

#[test]
fn an_update_by_another_signer_is_deleted() {
    let s = setup("signer");
    let fetch = Table::release(&sha256_hex(&release_zip()));
    let other = |_: &Path| Err("the update is signed by another signer".to_owned());
    let outcome = check(&s, &fetch, &other).run(5_000_000);
    assert!(
        matches!(&outcome, Outcome::Failed(e) if e.contains("another signer")),
        "{outcome:?}"
    );
    assert!(!s.paths.staged_dir(NEXT).exists());
    assert!(!s
        .paths
        .staged_dir(NEXT)
        .with_extension("unpacking")
        .exists());
    assert_eq!(State::load(&s.paths).staged, None);
    // The real check refuses an unsigned executable (a test binary is never signed).
    assert!(verify_signer(&std::env::current_exe().unwrap()).is_err());
}

#[test]
fn an_install_that_cannot_be_written_points_at_the_release_page() {
    let mut s = setup("readonly");
    s.install = s.install.join("not there");
    let fetch = Table::release(&sha256_hex(&release_zip()));
    assert_eq!(
        check(&s, &fetch, &accept).run(5_000_000),
        Outcome::CannotWrite {
            name: NEXT.into(),
            page: "https://example.test/page".into()
        }
    );
    // (only the list was asked for)
    assert_eq!(fetch.requests.get(), 1);
}

fn staged(s: &Setup) -> Staged {
    let fetch = Table::release(&sha256_hex(&release_zip()));
    match check(s, &fetch, &accept).run(5_000_000) {
        Outcome::Staged(st) => st,
        o => panic!("{o:?}"),
    }
}

fn system<'a>(
    s: &'a Setup,
    exe: &'a Path,
    launch: &'a dyn Fn(&Path) -> std::io::Result<()>,
) -> System<'a> {
    System {
        paths: &s.paths,
        exe,
        verify: &accept,
        others_running: &|| false,
        launch,
        wait: Duration::from_millis(300),
    }
}

#[test]
fn the_next_start_moves_the_release_in_and_starts_it() {
    let s = setup("apply");
    staged(&s);
    let exe = s.install.join("snowlark.exe");
    let launched = Cell::new(None);
    let launch = |p: &Path| {
        launched.set(Some(p.to_owned()));
        Ok(())
    };
    assert_eq!(
        apply::at_start(&system(&s, &exe, &launch)),
        Applied::Started
    );
    assert_eq!(launched.take(), Some(exe.clone()));
    assert_eq!(std::fs::read(&exe).unwrap(), b"MZ the new program");
    assert_eq!(
        std::fs::read(s.install.join("README.txt")).unwrap(),
        b"the new read me"
    );
    // The old files are set aside until a later start removes them.
    assert_eq!(
        std::fs::read(s.install.join("snowlark.exe.update-old")).unwrap(),
        b"MZ the old program"
    );
    assert_eq!(State::load(&s.paths).staged, None);
    assert!(!s.paths.staged_dir(NEXT).exists());
    apply::remove_old_copies(&s.install);
    let mut left: Vec<String> = std::fs::read_dir(&s.install)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["README.txt", "snowlark.exe"]);
    // Nothing staged: the start goes on.
    assert_eq!(apply::at_start(&system(&s, &exe, &launch)), Applied::No);
}

#[test]
fn a_failed_move_puts_every_file_back() {
    let s = setup("rollback");
    staged(&s);
    let exe = s.install.join("snowlark.exe");
    // (the second file cannot be copied in: a folder is where its copy goes)
    std::fs::create_dir_all(s.install.join("README.txt.update-new").join("x")).unwrap();
    let launch = |_: &Path| -> std::io::Result<()> { panic!("started after a failed move") };
    assert_eq!(apply::at_start(&system(&s, &exe, &launch)), Applied::No);
    assert_eq!(std::fs::read(&exe).unwrap(), b"MZ the old program");
    assert_eq!(
        std::fs::read(s.install.join("README.txt")).unwrap(),
        b"the old read me"
    );
    assert!(!s.install.join("snowlark.exe.update-old").exists());
    assert!(!s.install.join("snowlark.exe.update-new").exists());
    // The release stays staged for the next start.
    assert_eq!(
        State::load(&s.paths).staged.map(|st| st.name),
        Some(NEXT.into())
    );
    assert!(s.paths.staged_dir(NEXT).is_dir());
}

#[test]
fn a_new_executable_that_does_not_start_is_moved_out_again() {
    let s = setup("nolaunch");
    staged(&s);
    let exe = s.install.join("snowlark.exe");
    let launch = |_: &Path| Err(std::io::Error::other("cannot start"));
    assert_eq!(apply::at_start(&system(&s, &exe, &launch)), Applied::No);
    assert_eq!(std::fs::read(&exe).unwrap(), b"MZ the old program");
    assert_eq!(
        std::fs::read(s.install.join("README.txt")).unwrap(),
        b"the old read me"
    );
    assert!(State::load(&s.paths).staged.is_some());
}

#[test]
fn nothing_moves_while_another_process_of_the_install_runs() {
    let s = setup("others");
    staged(&s);
    let exe = s.install.join("snowlark.exe");
    let launch = |_: &Path| -> std::io::Result<()> { panic!("started while another runs") };
    let mut sys = system(&s, &exe, &launch);
    sys.others_running = &|| true;
    assert_eq!(apply::at_start(&sys), Applied::No);
    assert_eq!(std::fs::read(&exe).unwrap(), b"MZ the old program");
    assert!(State::load(&s.paths).staged.is_some());
    // A staged release whose executable no longer verifies is dropped.
    let mut sys = system(&s, &exe, &launch);
    sys.verify = &|_| Err("changed".into());
    assert_eq!(apply::at_start(&sys), Applied::No);
    assert_eq!(State::load(&s.paths).staged, None);
    assert!(!s.paths.staged_dir(NEXT).exists());
}

/// Serves the routes `routes` makes from the server's origin (path, body) over HTTP on this
/// machine, one connection at a time, until the test process ends; gives the origin. A body
/// `REDIRECT <url>` is a 302 to that URL.
fn serve(routes: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let routes = routes(&origin);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
            // (the rest of the request's head)
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            let (status, body) = match routes.iter().find(|(p, _)| *p == path) {
                Some((_, b)) => ("200 OK", b.clone()),
                None => ("404 Not Found", b"no".to_vec()),
            };
            if let Some(to) = body.strip_prefix(b"REDIRECT ") {
                let to = String::from_utf8_lossy(to);
                let _ = write!(
                    conn,
                    "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                continue;
            }
            let _ = write!(
                conn,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = conn.write_all(&body);
        }
    });
    origin
}

#[test]
fn a_check_over_a_local_feed_stages_the_release() {
    let zip = release_zip();
    // (the zip's address redirects to another origin, as the site's downloads do)
    let files = serve(|_| vec![(format!("/{NEXT}.zip"), zip.clone())]);
    let origin = serve(|origin| {
        let feed = feed_json(origin).replace(
            &format!("{origin}/{NEXT}.zip\""),
            &format!("{origin}/go/{NEXT}.zip\""),
        );
        vec![
            ("/releases".to_owned(), feed.into_bytes()),
            (
                format!("/{NEXT}.zip.sha256"),
                format!("{}  {NEXT}.zip\n", sha256_hex(&zip)).into_bytes(),
            ),
            (
                format!("/go/{NEXT}.zip"),
                format!("REDIRECT {files}/{NEXT}.zip").into_bytes(),
            ),
        ]
    });
    let s = setup("local");
    let fetch = NetFetch::start().unwrap();
    let feed = format!("{origin}/releases");
    let c = Check {
        feed: &feed,
        fetch: &fetch,
        ..check(&s, &fetch, &accept)
    };
    let outcome = c.run(now());
    assert!(
        matches!(&outcome, Outcome::Staged(st) if st.name == NEXT),
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read(s.paths.staged_dir(NEXT).join("snowlark.exe")).unwrap(),
        b"MZ the new program"
    );
}

/// One real read of the list of releases: it answers, and it is a list (run by hand, never in a
/// normal test run).
#[test]
#[ignore = "reads the real list of releases on the network"]
fn the_real_list_of_releases_reads() {
    let fetch = NetFetch::start().unwrap();
    let body = fetch
        .get_all(FEED, "application/vnd.github+json", 4 << 20)
        .unwrap();
    let list = feed::parse(&body).unwrap();
    eprintln!(
        "{} bytes, {} release(s) with a checksum",
        body.len(),
        list.len()
    );
    let all: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(all.as_array().is_some_and(|a| !a.is_empty()));
}
