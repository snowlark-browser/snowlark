// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one updater of the process: a thread that keeps the enabled lists' copies current and
//! compiles the engine when they change. Its downloads go through the browser's network client
//! (they run on the network thread's runtime like any fetch); compiling, which takes a fraction of
//! a second for the default lists, happens on the updater's own thread so the network thread never
//! waits for it.
//!
//! A list is checked again when its copy expires (its `! Expires:`, else uBlock Origin's interval
//! for it), with `If-None-Match` / `If-Modified-Since` so an unchanged list costs a 304; a failed
//! download is retried after an hour, doubling up to a day, trying the list's mirrors in turn. The
//! first run downloads what is missing; until then the built-in minimal list blocks.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use snowlark_net::{CredentialsMode, NetClient, Request, Url};

use crate::engine::{FilterEngine, ListFormat, ListText};
use crate::lists::{enabled_lists, expires_of, looks_like_hosts, ListSpec, ListState, ListStore};
use crate::ContentBlocker;

/// What the updater is asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdaterMsg {
    /// The configuration changed: fetch new lists, compile if needed.
    Changed,
    /// Check every list now, expired or not ("Update now").
    UpdateAll,
    Stop,
}

/// The largest list accepted (the default lists are under 13 MB each).
const MAX_LIST_BYTES: usize = 64 << 20;

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Starts the updater of `blocker` (which must have a folder), downloading through `net`.
pub fn start(blocker: Arc<ContentBlocker>, net: NetClient) -> Sender<UpdaterMsg> {
    let (tx, rx) = channel();
    let b = blocker.clone();
    let _ = std::thread::Builder::new()
        .name("snowlark-blocker-updater".into())
        .spawn(move || run(b, net, rx));
    tx
}

fn run(blocker: Arc<ContentBlocker>, net: NetClient, rx: Receiver<UpdaterMsg>) {
    let Some(dir) = blocker.dir().map(|d| d.to_owned()) else {
        return;
    };
    let mut store = ListStore::open(&dir);
    let mut force = false;
    loop {
        let next = pass(&blocker, &net, &mut store, force);
        let wait = next.saturating_sub(unix_now()).clamp(60, 6 * 3600);
        match rx.recv_timeout(Duration::from_secs(wait)) {
            Ok(UpdaterMsg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(m) => force = m == UpdaterMsg::UpdateAll,
            Err(RecvTimeoutError::Timeout) => force = false,
        }
    }
}

/// One round: fetch what is due (everything with `force`), compile when the lists or the user's
/// filters changed, and say when the next list is due (Unix seconds).
pub fn pass(blocker: &ContentBlocker, net: &NetClient, store: &mut ListStore, force: bool) -> u64 {
    let config = blocker.config();
    let specs = enabled_lists(&config);
    let now = unix_now();
    for spec in &specs {
        let has = store.has_text(&spec.id);
        let due = store.states.get(&spec.id).map_or(0, |s| s.due(has));
        if force || due <= now {
            update_list(net, store, spec, now);
        }
    }
    crate::scriptlets::update_resources(blocker, net, store, force, now);
    let signature = store.signature(&specs, &config.custom_filters);
    let saved = blocker
        .dir()
        .is_some_and(|d| d.join("engine.dat").is_file());
    if signature != store.built || !saved {
        let resources = crate::resources::load_resources(
            &blocker.dir().expect("a folder").join("resources.json"),
        );
        if let Some(engine) = compile_from_store(store, &specs, &config.custom_filters, resources) {
            if let Some(dir) = blocker.dir() {
                let tmp = dir.join("engine.dat.tmp");
                if std::fs::write(&tmp, engine.serialize()).is_ok() {
                    let _ = std::fs::rename(&tmp, dir.join("engine.dat"));
                }
            }
            blocker.set_engine(engine);
            store.built = signature;
        }
    }
    store.save();
    specs
        .iter()
        .map(|s| {
            store
                .states
                .get(&s.id)
                .map_or(0, |st| st.due(store.has_text(&s.id)))
        })
        .chain(std::iter::once(crate::scriptlets::resources_due(store)))
        .min()
        .unwrap_or(now + 86_400)
}

/// The engine of `specs` (the copies there are) and the user's filters; `None` while none of the
/// lists has a copy yet (the current engine stays).
pub fn compile_from_store(
    store: &ListStore,
    specs: &[ListSpec],
    custom_filters: &str,
    resources: Vec<adblock::resources::Resource>,
) -> Option<FilterEngine> {
    let texts: Vec<(String, &ListSpec)> = specs
        .iter()
        .filter_map(|s| store.text(&s.id).map(|t| (t, s)))
        .collect();
    if texts.is_empty() && !specs.is_empty() {
        return None;
    }
    let mut lists: Vec<ListText> = texts
        .iter()
        .map(|(t, s)| ListText {
            text: t,
            format: if s.id.starts_with("custom-") && looks_like_hosts(t) {
                ListFormat::Hosts
            } else {
                s.format
            },
            trusted: s.trusted,
        })
        .collect();
    lists.push(ListText {
        text: custom_filters,
        format: ListFormat::Filters,
        trusted: true,
    });
    Some(FilterEngine::compile(&lists, resources))
}

/// The result of a conditional download.
pub(crate) enum Download {
    NotModified,
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// A GET of `url` the browser makes for itself (no cookies), revalidating with `etag` /
/// `last_modified`.
pub(crate) fn download(
    net: &NetClient,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<Download, String> {
    let url = Url::parse(url).map_err(|e| e.to_string())?;
    let mut request = Request::new(url);
    request.credentials = CredentialsMode::Omit;
    request.initiator_type = "content-blocker".into();
    if let Some(e) = etag {
        request.headers.append("If-None-Match", e);
    }
    if let Some(m) = last_modified {
        request.headers.append("If-Modified-Since", m);
    }
    let response = net.fetch(request).read_all().map_err(|e| e.message)?;
    match response.head.status {
        304 => Ok(Download::NotModified),
        200 => {
            let bytes = response.body.map(|b| b.to_vec()).unwrap_or_default();
            if bytes.len() > MAX_LIST_BYTES {
                return Err(format!("{} bytes is too large", bytes.len()));
            }
            Ok(Download::Body {
                bytes,
                etag: response.head.headers.get("etag"),
                last_modified: response.head.headers.get("last-modified"),
            })
        }
        s => Err(format!("HTTP {s}")),
    }
}

/// Checks one list (its mirrors in turn), keeping a new copy. Returns whether the copy changed.
fn update_list(net: &NetClient, store: &mut ListStore, spec: &ListSpec, now: u64) -> bool {
    let state = store.states.get(&spec.id).cloned().unwrap_or_default();
    let has = store.has_text(&spec.id);
    let mut error = String::new();
    for url in &spec.urls {
        let same = has && state.url == *url;
        let (etag, modified) = if same {
            (state.etag.as_deref(), state.last_modified.as_deref())
        } else {
            (None, None)
        };
        let (main, fresh) = match download(net, url, etag, modified) {
            // (a copy from before includes were read is the list's own file)
            Ok(Download::NotModified) => {
                match store.part(&spec.id, "").or_else(|| store.text(&spec.id)) {
                    Some(t) => (t, None),
                    None => {
                        error = format!("{url}: 304 without a copy");
                        continue;
                    }
                }
            }
            Ok(Download::Body {
                bytes,
                etag,
                last_modified,
            }) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let start = text.trim_start();
                if start.is_empty() || start.starts_with('<') {
                    error = format!("{url}: not a filter list");
                    continue;
                }
                (text, Some((etag, last_modified)))
            }
            Err(e) => {
                error = format!("{url}: {e}");
                continue;
            }
        };
        let mut includes = if same {
            state.includes.clone()
        } else {
            Default::default()
        };
        let mut changed = fresh.is_some() || !has;
        let expanded = match expand_includes(
            net,
            store,
            &spec.id,
            url,
            &main,
            &mut includes,
            &mut changed,
        ) {
            Ok(t) => t,
            Err(e) => {
                error = e;
                continue;
            }
        };
        if fresh.is_some() {
            let _ = store.write_part(&spec.id, "", &main);
        }
        if changed {
            if let Err(e) = store.write_text(&spec.id, &expanded) {
                error = format!("can't save {}: {e}", spec.id);
                break;
            }
        }
        let (etag, last_modified) =
            fresh.unwrap_or((state.etag.clone(), state.last_modified.clone()));
        store.states.insert(
            spec.id.clone(),
            ListState {
                url: url.clone(),
                etag,
                last_modified,
                checked: now,
                expires: expires_of(&main).unwrap_or(spec.update_after_hours * 3600),
                bytes: expanded.len(),
                lines: expanded.lines().count(),
                error: None,
                failures: 0,
                failed_at: 0,
                includes,
            },
        );
        return changed;
    }
    let s = store.states.entry(spec.id.clone()).or_default();
    s.failures += 1;
    s.failed_at = now;
    s.error = Some(error);
    false
}

/// uBlock Origin's `!#include name`: the named file, from the list's own origin and directory,
/// takes the line's place (one level, as uBlock Origin reads them). Each file is revalidated with
/// its ETag; `changed` is set when one is new.
fn expand_includes(
    net: &NetClient,
    store: &ListStore,
    id: &str,
    base: &str,
    main: &str,
    includes: &mut std::collections::BTreeMap<String, Option<String>>,
    changed: &mut bool,
) -> Result<String, String> {
    let base = Url::parse(base).map_err(|e| e.to_string())?;
    let mut out = String::with_capacity(main.len());
    let mut used = std::collections::BTreeMap::new();
    for line in main.lines() {
        let Some(name) = line.strip_prefix("!#include ").map(str::trim) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let Ok(u) = base.join(name) else {
            continue;
        };
        if name.contains("..") || u.origin() != base.origin() || used.contains_key(name) {
            continue;
        }
        let cached = store.part(id, name);
        let etag = includes.get(name).cloned().flatten();
        let text = match download(net, u.as_str(), cached.as_ref().and(etag.as_deref()), None)
            .map_err(|e| format!("{u}: {e}"))?
        {
            Download::NotModified => cached.ok_or_else(|| format!("{u}: 304 without a copy"))?,
            Download::Body { bytes, etag, .. } => {
                let t = String::from_utf8_lossy(&bytes).into_owned();
                let _ = store.write_part(id, name, &t);
                *changed = true;
                used.insert(name.to_owned(), etag);
                out.push_str(t.trim_end_matches(['\r', '\n']));
                out.push('\n');
                continue;
            }
        };
        used.insert(name.to_owned(), etag);
        out.push_str(text.trim_end_matches(['\r', '\n']));
        out.push('\n');
    }
    if used.len() != includes.len() {
        *changed = true;
    }
    *includes = used;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BlockerConfig;

    #[test]
    fn compiles_what_there_is() {
        let dir = std::env::temp_dir().join(format!("snowlark-blocker-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = ListStore::open(&dir);
        let specs = enabled_lists(&BlockerConfig::default());
        // No copies yet: keep the current engine.
        assert!(compile_from_store(&store, &specs, "", Vec::new()).is_none());
        store.write_text("easylist", "||ads.example^\n").unwrap();
        store
            .write_text("plowe-0", "127.0.0.1 tracker.example\n")
            .unwrap();
        let e = compile_from_store(&store, &specs, "||mine.example^", Vec::new()).unwrap();
        for host in ["ads.example", "tracker.example", "mine.example"] {
            assert_eq!(
                e.match_request(&format!("https://{host}/x"), "https://a.example/", "script"),
                crate::engine::NetworkMatch::Block,
                "{host}"
            );
        }
        // Every list off: an engine of the user's filters alone.
        let e = compile_from_store(&store, &[], "", Vec::new()).unwrap();
        assert_eq!(
            e.match_request("https://ads.example/x", "https://a.example/", "script"),
            crate::engine::NetworkMatch::Allow
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
