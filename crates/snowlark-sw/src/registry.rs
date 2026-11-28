// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The service worker registrations of a profile and their job queues (W3C Service Workers §3.4
//! "Service Worker Registration", §8 "Algorithms": Schedule Job, Run Job, Finish Job, Register,
//! Update, Install, Try Activate, Activate, Unregister, Clear Registration).
//!
//! [`Registry`] is the state machine alone: it takes calls from connections (the event loops of
//! pages and workers, and the ones that run versions) and answers with [`Effect`]s: notices to
//! send to a connection, script fetches to start, registrations to store. [`crate::host`] runs it
//! on a thread of its own and carries the effects out. The security checks of registration (the
//! script's and the scope's origin, the script's MIME type, the scope under the script's
//! directory unless `Service-Worker-Allowed` widens it) are made here, never in the page crates.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use url::Url;

/// A registration, unique in the profile for as long as it lives.
pub type RegistrationId = u64;
/// A service worker (a version of a registration's script).
pub type VersionId = u64;
/// An event loop's connection to the registry.
pub type ConnId = u64;
/// A call's number on its connection, which the answer carries back.
pub type CallId = u64;
/// A service worker client (a window's document) as its connection numbers it.
pub type ClientId = u64;

/// How long a running service worker may stay without an event before it is stopped (the spec
/// leaves this to the user agent; 30 s as the engine family does).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one lifecycle event (its `waitUntil` promises included) may take before it fails.
pub const EVENT_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a version's script may take to start (fetched already: only its evaluation).
pub const START_TIMEOUT: Duration = Duration::from_secs(60);

/// §3.1.1 a service worker's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerState {
    Parsed,
    Installing,
    Installed,
    Activating,
    Activated,
    Redundant,
}

impl WorkerState {
    /// The `ServiceWorkerState` enumeration value.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerState::Parsed => "parsed",
            WorkerState::Installing => "installing",
            WorkerState::Installed => "installed",
            WorkerState::Activating => "activating",
            WorkerState::Activated => "activated",
            WorkerState::Redundant => "redundant",
        }
    }
}

/// §3.2 a registration's update via cache mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateViaCache {
    #[default]
    Imports,
    All,
    None,
}

/// A service worker's type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerType {
    #[default]
    Classic,
    Module,
}

/// A registration's three worker slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Slot {
    Installing,
    Waiting,
    Active,
}

/// The lifecycle events a version is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    Install,
    Activate,
}

/// What a client knows of a service worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub id: VersionId,
    pub script_url: Url,
    pub state: WorkerState,
}

/// What a client knows of a registration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationInfo {
    pub id: RegistrationId,
    pub scope: Url,
    pub update_via_cache: UpdateViaCache,
    pub installing: Option<VersionInfo>,
    pub waiting: Option<VersionInfo>,
    pub active: Option<VersionInfo>,
}

/// A script of a version's script resource map (§2.1) other than its main script: a classic
/// script `importScripts()` fetched, or a module of a module worker's graph, by its request URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedScript {
    pub url: Url,
    /// Its response's URL (the script's base URL).
    pub final_url: Url,
    pub source: Vec<u8>,
}

/// A rejected job's exception: a `DOMException` name or `TypeError`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobError {
    pub name: String,
    pub message: String,
}

impl JobError {
    fn new(name: &str, message: impl Into<String>) -> JobError {
        JobError {
            name: name.into(),
            message: message.into(),
        }
    }
    fn type_error(message: impl Into<String>) -> JobError {
        JobError::new("TypeError", message)
    }
    fn security(message: impl Into<String>) -> JobError {
        JobError::new("SecurityError", message)
    }
}

/// A call from a connection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Call {
    /// `register()` (§3.4.3 and §8.? Start Register): the client's URL, origin and policy are the
    /// job client's; the scope is resolved already (the script's `./` when none was given).
    Register {
        client: snowlark_net::Client,
        script_url: Url,
        scope: Url,
        update_via_cache: UpdateViaCache,
        worker_type: WorkerType,
    },
    /// `registration.update()`.
    Update {
        client: snowlark_net::Client,
        registration: RegistrationId,
    },
    /// `registration.unregister()`.
    Unregister {
        client: snowlark_net::Client,
        registration: RegistrationId,
    },
    /// `getRegistration(clientURL)`: Match Service Worker Registration.
    GetRegistration { client_url: Url },
    /// `getRegistrations()`.
    GetRegistrations,
    /// `ready`: answered once the registration matching `client_url` has an active worker.
    Ready { client_url: Url },
    /// From the runner: the version's script ran (`ok`) or threw or failed to start.
    Evaluated { version: VersionId, ok: bool },
    /// From the runner: a lifecycle event and its extensions settled.
    EventDone {
        version: VersionId,
        event: Lifecycle,
        ok: bool,
    },
    /// `skipWaiting()` from the version's global scope.
    SkipWaiting { version: VersionId },
    /// From the runner: the version's thread ended on its own (closed, or its event loop died).
    Stopped { version: VersionId },
    /// A service worker client of the connection's event loop (a window's environment settings
    /// object): its creation URL and whether it is a secure context. No answer.
    Client {
        client: ClientId,
        url: Url,
        secure: bool,
    },
    /// The client is gone (its document was unloaded): §8 Handle Service Worker Client Unload.
    /// No answer.
    ClientGone { client: ClientId },
    /// `clients.claim()` from the version's global scope (§4.3.4).
    Claim { version: VersionId },
    /// From the version's global scope while it is parsed or installing (§6.3.2 importScripts'
    /// "perform the fetch" steps 12-14, and a module of its graph): a script fetched and put in
    /// its script resource map. No answer.
    Imported {
        version: VersionId,
        script: ImportedScript,
    },
    /// The same, for a script its map had already (step 5): it joins the set of used scripts.
    /// No answer.
    UsedImport { version: VersionId, url: Url },
}

/// The answer to a call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Answer {
    Registration(Result<RegistrationInfo, JobError>),
    Lookup(Option<RegistrationInfo>),
    Lookups(Vec<RegistrationInfo>),
    Unregistered(Result<bool, JobError>),
    Claimed(Result<(), JobError>),
    Done,
}

/// What a connection is sent, in order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Notice {
    Answer {
        call: CallId,
        answer: Answer,
    },
    /// §8 Update Registration State: a registration's slot now holds `version`.
    Slot {
        registration: RegistrationId,
        slot: Slot,
        version: Option<VersionInfo>,
    },
    /// §8 Update Worker State.
    State {
        version: VersionId,
        state: WorkerState,
    },
    /// §8 Install step: fire `updatefound` at the registration's objects.
    UpdateFound {
        registration: RegistrationId,
    },
    /// To a runner: start the version's worker with its script.
    Start {
        version: VersionInfo,
        registration: RegistrationInfo,
        worker_type: WorkerType,
        source: Vec<u8>,
        /// The rest of its script resource map.
        imports: Vec<ImportedScript>,
    },
    /// To a runner: fire a lifecycle event at the version's global.
    Dispatch {
        version: VersionId,
        event: Lifecycle,
    },
    /// To a runner: terminate the version's worker.
    Stop {
        version: VersionId,
    },
    /// The client's active service worker changed (§8 Notify Controller Change): fire
    /// `controllerchange` at its container.
    Controller {
        client: ClientId,
        version: Option<VersionInfo>,
    },
}

/// What the registry asks its host to do.
// (effects are made and carried out at once, never stored)
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Effect {
    Notify(ConnId, Notice),
    /// Fetch a job's script; the result goes to [`Registry::fetched`].
    Fetch {
        job: u64,
        request: Box<snowlark_net::Request>,
    },
    /// Fetch one of the newest worker's imported scripts for a job's byte check; the result goes
    /// to [`Registry::fetched_import`].
    FetchImport {
        job: u64,
        index: usize,
        request: Box<snowlark_net::Request>,
    },
    /// A storage key's stored registrations changed ([`Registry::stored`]).
    Persist(String),
}

/// A registration as it is kept across restarts: those with an active worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRegistration {
    pub scope: Url,
    pub update_via_cache: UpdateViaCache,
    pub script_url: Url,
    pub worker_type: WorkerType,
    pub source: Vec<u8>,
    /// The rest of its active worker's script resource map.
    pub imports: Vec<ImportedScript>,
}

/// The storage key of an origin: its ASCII serialization (storage is not partitioned yet).
pub fn storage_key(url: &Url) -> String {
    url.origin().ascii_serialization()
}

struct Conn {
    key: String,
    /// Whether it can run service workers of its key (an event loop of a page's process).
    runner: bool,
}

/// A service worker client.
struct ClientRec {
    key: String,
    /// Its creation URL, which Match Service Worker Registration takes.
    url: Url,
    secure: bool,
    /// Its active service worker (the controller).
    controller: Option<VersionId>,
}

struct Registration {
    key: String,
    /// Removed from the registration map by Unregister while a client still uses it (§8 Try
    /// Clear Registration clears it when the last one goes).
    unregistered: bool,
    scope: Url,
    update_via_cache: UpdateViaCache,
    installing: Option<VersionId>,
    waiting: Option<VersionId>,
    active: Option<VersionId>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Running {
    Stopped,
    Starting,
    Started,
}

struct Version {
    registration: RegistrationId,
    script_url: Url,
    worker_type: WorkerType,
    source: Vec<u8>,
    /// The rest of its script resource map (§2.1), in the order the scripts came.
    imports: Vec<ImportedScript>,
    /// Its set of used scripts, besides the main one: the imports its install keeps.
    used: Vec<Url>,
    state: WorkerState,
    skip_waiting: bool,
    running: Running,
    runner: Option<ConnId>,
    /// The worker's own connection (it sent [`Call::Evaluated`]): its events go there, in order
    /// with the notices about its state.
    own: Option<ConnId>,
    /// The lifecycle event sent and not done yet, with its deadline.
    current: Option<(Lifecycle, Instant)>,
    /// Events waiting for the worker to start.
    queued: VecDeque<Lifecycle>,
    /// Since when it has had nothing to do (the idle timer), or when it was asked to start.
    since: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Register,
    Update,
    Unregister,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Queued,
    Fetching,
    /// The main script is unchanged: the newest worker's imports are being fetched (§8 Update's
    /// perform the fetch, step 21).
    CheckingImports,
    Evaluating(VersionId),
    Installing(VersionId),
}

struct Job {
    id: u64,
    kind: Kind,
    key: String,
    scope: Url,
    script_url: Option<Url>,
    update_via_cache: UpdateViaCache,
    worker_type: WorkerType,
    client: snowlark_net::Client,
    /// The connection the job came from (its runner, when it can run one).
    origin_conn: ConnId,
    /// The promises of this job and its equivalent jobs.
    waiters: Vec<(ConnId, CallId)>,
    settled: bool,
    stage: Stage,
    /// The byte check of imported scripts, while [`Stage::CheckingImports`].
    check: Option<Box<ImportCheck>>,
}

/// §8 Update step 21: the main script fetched, unchanged, and the newest worker's imports.
struct ImportCheck {
    source: Vec<u8>,
    /// The newest worker's imports, each with its fetch's good response once it is in.
    stored: Vec<(ImportedScript, Option<Option<ImportedScript>>)>,
    changed: bool,
}

impl Job {
    /// §8 "job equivalent".
    fn equivalent(&self, other: &Job) -> bool {
        self.kind == other.kind
            && match self.kind {
                Kind::Unregister => self.scope == other.scope,
                _ => {
                    self.scope == other.scope
                        && self.script_url == other.script_url
                        && self.update_via_cache == other.update_via_cache
                        && self.worker_type == other.worker_type
                }
            }
    }
}

/// The registrations of a profile, their versions and job queues.
#[derive(Default)]
pub struct Registry {
    conns: HashMap<ConnId, Conn>,
    registrations: BTreeMap<RegistrationId, Registration>,
    versions: HashMap<VersionId, Version>,
    /// The scope to job queue map, by storage key and scope URL.
    queues: HashMap<(String, String), VecDeque<Job>>,
    readies: Vec<(ConnId, CallId, String, Url)>,
    clients: BTreeMap<(ConnId, ClientId), ClientRec>,
    next_id: u64,
    effects: Vec<Effect>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry::default()
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Takes what the last calls asked the host to do.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// A new connection `conn` (numbered by the host) of an event loop whose storage key is `key`.
    pub fn connect(&mut self, conn: ConnId, key: String, runner: bool) {
        self.conns.insert(conn, Conn { key, runner });
    }

    /// A connection closed: its readies are dropped, and the versions it ran stopped.
    pub fn disconnect(&mut self, conn: ConnId, now: Instant) {
        self.conns.remove(&conn);
        self.readies.retain(|r| r.0 != conn);
        let gone: Vec<ClientId> = self
            .clients
            .range((conn, 0)..=(conn, ClientId::MAX))
            .map(|((_, c), _)| *c)
            .collect();
        for c in gone {
            self.client_gone(conn, c, now);
        }
        let ran: Vec<VersionId> = self
            .versions
            .iter()
            .filter(|(_, v)| v.runner == Some(conn) || v.own == Some(conn))
            .map(|(id, _)| *id)
            .collect();
        for v in ran {
            self.stopped(v, now);
        }
    }

    /// Restores a storage key's stored registrations (each with an activated worker).
    pub fn restore(&mut self, key: &str, stored: Vec<StoredRegistration>, now: Instant) {
        for s in stored {
            let rid = self.id();
            let vid = self.id();
            self.versions.insert(
                vid,
                Version {
                    registration: rid,
                    script_url: s.script_url,
                    worker_type: s.worker_type,
                    source: s.source,
                    used: s.imports.iter().map(|i| i.url.clone()).collect(),
                    imports: s.imports,
                    state: WorkerState::Activated,
                    skip_waiting: false,
                    running: Running::Stopped,
                    runner: None,
                    own: None,
                    current: None,
                    queued: VecDeque::new(),
                    since: now,
                },
            );
            self.registrations.insert(
                rid,
                Registration {
                    key: key.to_owned(),
                    unregistered: false,
                    scope: s.scope,
                    update_via_cache: s.update_via_cache,
                    installing: None,
                    waiting: None,
                    active: Some(vid),
                },
            );
        }
    }

    /// A storage key's registrations as they are stored (those with an active worker).
    pub fn stored(&self, key: &str) -> Vec<StoredRegistration> {
        self.registrations
            .values()
            .filter(|r| r.key == key && !r.unregistered)
            .filter_map(|r| {
                let v = self.versions.get(&r.active?)?;
                Some(StoredRegistration {
                    scope: r.scope.clone(),
                    update_via_cache: r.update_via_cache,
                    script_url: v.script_url.clone(),
                    worker_type: v.worker_type,
                    source: v.source.clone(),
                    imports: v.imports.clone(),
                })
            })
            .collect()
    }

    /// The storage keys that have registrations, sorted.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.registrations.values().map(|r| r.key.clone()).collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    /// Removes every registration of a storage key (clearing site data).
    pub fn clear_key(&mut self, key: &str) {
        let ids: Vec<RegistrationId> = self
            .registrations
            .iter()
            .filter(|(_, r)| r.key == key)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.clear_registration(id);
        }
        self.effects.push(Effect::Persist(key.to_owned()));
    }

    fn version_info(&self, id: VersionId) -> Option<VersionInfo> {
        let v = self.versions.get(&id)?;
        Some(VersionInfo {
            id,
            script_url: v.script_url.clone(),
            state: v.state,
        })
    }

    fn info(&self, id: RegistrationId) -> Option<RegistrationInfo> {
        let r = self.registrations.get(&id)?;
        Some(RegistrationInfo {
            id,
            scope: r.scope.clone(),
            update_via_cache: r.update_via_cache,
            installing: r.installing.and_then(|v| self.version_info(v)),
            waiting: r.waiting.and_then(|v| self.version_info(v)),
            active: r.active.and_then(|v| self.version_info(v)),
        })
    }

    /// The registration whose scope is `scope` in `key` ("Get Registration").
    fn get_registration(&self, key: &str, scope: &Url) -> Option<RegistrationId> {
        self.registrations
            .iter()
            .find(|(_, r)| r.key == key && !r.unregistered && r.scope == *scope)
            .map(|(id, _)| *id)
    }

    /// §8 Match Service Worker Registration: the registration whose scope is the longest prefix of
    /// `url` (as serialized) in `key`.
    pub fn match_registration(&self, key: &str, url: &Url) -> Option<RegistrationId> {
        let s = url.as_str();
        self.registrations
            .iter()
            .filter(|(_, r)| r.key == key && !r.unregistered && s.starts_with(r.scope.as_str()))
            .max_by_key(|(_, r)| r.scope.as_str().len())
            .map(|(id, _)| *id)
    }

    fn newest(&self, r: &Registration) -> Option<VersionId> {
        r.installing.or(r.waiting).or(r.active)
    }

    fn answer(&mut self, conn: ConnId, call: CallId, answer: Answer) {
        self.effects
            .push(Effect::Notify(conn, Notice::Answer { call, answer }));
    }

    /// Sends `notice` to every connection of `key`.
    fn broadcast(&mut self, key: &str, notice: Notice) {
        let mut conns: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.key == key)
            .map(|(id, _)| *id)
            .collect();
        conns.sort_unstable();
        for c in conns {
            self.effects.push(Effect::Notify(c, notice.clone()));
        }
    }

    /// Handles a call from `conn`.
    pub fn call(&mut self, conn: ConnId, id: CallId, call: Call, now: Instant) {
        let Some(key) = self.conns.get(&conn).map(|c| c.key.clone()) else {
            return;
        };
        // (a registration or a version of another storage key is not this connection's to name)
        let named = match &call {
            Call::Update { registration, .. } | Call::Unregister { registration, .. } => {
                Some(*registration)
            }
            Call::Evaluated { version, .. }
            | Call::EventDone { version, .. }
            | Call::SkipWaiting { version }
            | Call::Stopped { version }
            | Call::Claim { version }
            | Call::Imported { version, .. }
            | Call::UsedImport { version, .. } => {
                self.versions.get(version).map(|v| v.registration)
            }
            _ => None,
        };
        if named
            .and_then(|r| self.registrations.get(&r))
            .is_some_and(|r| r.key != key)
        {
            return;
        }
        match call {
            Call::Register {
                client,
                script_url,
                scope,
                update_via_cache,
                worker_type,
            } => {
                // §8 Start Register: the URLs' schemes and origins.
                if !matches!(script_url.scheme(), "http" | "https") {
                    return self.answer(
                        conn,
                        id,
                        Answer::Registration(Err(JobError::type_error(
                            "The script URL's scheme is not http or https",
                        ))),
                    );
                }
                if !matches!(scope.scheme(), "http" | "https") {
                    return self.answer(
                        conn,
                        id,
                        Answer::Registration(Err(JobError::type_error(
                            "The scope URL's scheme is not http or https",
                        ))),
                    );
                }
                let decoded = scope.path().to_ascii_lowercase();
                if decoded.contains("%2f") || decoded.contains("%5c") {
                    return self.answer(
                        conn,
                        id,
                        Answer::Registration(Err(JobError::type_error(
                            "The scope's path contains an escaped slash",
                        ))),
                    );
                }
                let script_path = script_url.path().to_ascii_lowercase();
                if script_path.contains("%2f") || script_path.contains("%5c") {
                    return self.answer(
                        conn,
                        id,
                        Answer::Registration(Err(JobError::type_error(
                            "The script URL's path contains an escaped slash",
                        ))),
                    );
                }
                let mut scope = scope;
                scope.set_fragment(None);
                let mut script_url = script_url;
                script_url.set_fragment(None);
                self.schedule(Job {
                    id: 0,
                    kind: Kind::Register,
                    key,
                    scope,
                    script_url: Some(script_url),
                    update_via_cache,
                    worker_type,
                    client,
                    origin_conn: conn,
                    waiters: vec![(conn, id)],
                    settled: false,
                    stage: Stage::Queued,
                    check: None,
                });
            }
            Call::Update {
                client,
                registration,
            } => {
                let Some(r) = self.registrations.get(&registration) else {
                    return self.answer(
                        conn,
                        id,
                        Answer::Registration(Err(JobError::new(
                            "InvalidStateError",
                            "The registration was unregistered",
                        ))),
                    );
                };
                let Some(newest) = self.newest(r) else {
                    return self.answer(
                        conn,
                        id,
                        Answer::Registration(Err(JobError::new(
                            "InvalidStateError",
                            "The registration has no worker",
                        ))),
                    );
                };
                let (scope, uvc) = (r.scope.clone(), r.update_via_cache);
                let v = &self.versions[&newest];
                let (script_url, worker_type) = (v.script_url.clone(), v.worker_type);
                self.schedule(Job {
                    id: 0,
                    kind: Kind::Update,
                    key,
                    scope,
                    script_url: Some(script_url),
                    update_via_cache: uvc,
                    worker_type,
                    client,
                    origin_conn: conn,
                    waiters: vec![(conn, id)],
                    settled: false,
                    stage: Stage::Queued,
                    check: None,
                });
            }
            Call::Unregister {
                client,
                registration,
            } => {
                let scope = match self.registrations.get(&registration) {
                    Some(r) => r.scope.clone(),
                    // (already unregistered: the job finds no registration and answers false)
                    None => return self.answer(conn, id, Answer::Unregistered(Ok(false))),
                };
                self.schedule(Job {
                    id: 0,
                    kind: Kind::Unregister,
                    key,
                    scope,
                    script_url: None,
                    update_via_cache: UpdateViaCache::Imports,
                    worker_type: WorkerType::Classic,
                    client,
                    origin_conn: conn,
                    waiters: vec![(conn, id)],
                    settled: false,
                    stage: Stage::Queued,
                    check: None,
                });
            }
            Call::GetRegistration { client_url } => {
                let found = self
                    .match_registration(&key, &client_url)
                    .and_then(|r| self.info(r));
                self.answer(conn, id, Answer::Lookup(found));
            }
            Call::GetRegistrations => {
                let ids: Vec<RegistrationId> = self
                    .registrations
                    .iter()
                    .filter(|(_, r)| r.key == key && !r.unregistered)
                    .map(|(id, _)| *id)
                    .collect();
                let all = ids.into_iter().filter_map(|r| self.info(r)).collect();
                self.answer(conn, id, Answer::Lookups(all));
            }
            Call::Ready { client_url } => {
                self.readies.push((conn, id, key, client_url));
                self.check_readies();
            }
            Call::Evaluated { version, ok } => {
                if let Some(v) = self.versions.get_mut(&version) {
                    v.own = Some(conn);
                }
                self.evaluated(version, ok, now)
            }
            Call::EventDone { version, event, ok } => self.event_done(version, event, ok, now),
            Call::SkipWaiting { version } => {
                if let Some(v) = self.versions.get_mut(&version) {
                    v.skip_waiting = true;
                    let r = v.registration;
                    self.try_activate(r, now);
                }
                self.answer(conn, id, Answer::Done);
            }
            Call::Stopped { version } => self.stopped(version, now),
            Call::Client {
                client,
                url,
                secure,
            } => {
                let rec = self
                    .clients
                    .entry((conn, client))
                    .or_insert_with(|| ClientRec {
                        key: key.clone(),
                        url: url.clone(),
                        secure,
                        controller: None,
                    });
                rec.url = url;
                rec.secure = secure;
            }
            Call::ClientGone { client } => self.client_gone(conn, client, now),
            Call::Claim { version } => {
                let answer = self.claim(version, now);
                self.answer(conn, id, Answer::Claimed(answer));
            }
            Call::Imported { version, script } => {
                // (only a parsed or installing worker adds to its map: §6.3.2 step 4)
                let Some(v) = self.versions.get_mut(&version) else {
                    return;
                };
                if !matches!(v.state, WorkerState::Parsed | WorkerState::Installing) {
                    return;
                }
                if !v.used.contains(&script.url) {
                    v.used.push(script.url.clone());
                }
                match v.imports.iter_mut().find(|s| s.url == script.url) {
                    Some(s) => *s = script,
                    None => v.imports.push(script),
                }
            }
            Call::UsedImport { version, url } => {
                let Some(v) = self.versions.get_mut(&version) else {
                    return;
                };
                if matches!(v.state, WorkerState::Parsed | WorkerState::Installing)
                    && v.imports.iter().any(|s| s.url == url)
                    && !v.used.contains(&url)
                {
                    v.used.push(url);
                }
            }
        }
    }

    /// Whether a service worker client is using `rid`: its active service worker is one of the
    /// registration's.
    fn in_use(&self, rid: RegistrationId) -> bool {
        self.clients.values().any(|c| {
            c.controller
                .and_then(|v| self.versions.get(&v))
                .is_some_and(|v| v.registration == rid)
        })
    }

    /// §8 Notify Controller Change: the client's connection fires `controllerchange`.
    fn notify_controller(&mut self, conn: ConnId, client: ClientId) {
        let version = self
            .clients
            .get(&(conn, client))
            .and_then(|c| c.controller)
            .and_then(|v| self.version_info(v));
        self.effects
            .push(Effect::Notify(conn, Notice::Controller { client, version }));
    }

    /// §4.3.4 `claim()`: every secure client of the worker's origin whose creation URL matches
    /// its registration, and that another worker (or none) controls, gets it as its active
    /// service worker.
    fn claim(&mut self, vid: VersionId, now: Instant) -> Result<(), JobError> {
        let rid = self
            .versions
            .get(&vid)
            .filter(|v| matches!(v.state, WorkerState::Activating | WorkerState::Activated))
            .map(|v| v.registration)
            .filter(|r| {
                self.registrations
                    .get(r)
                    .is_some_and(|r| r.active == Some(vid))
            })
            .ok_or_else(|| {
                JobError::new(
                    "InvalidStateError",
                    "Only the active service worker can claim clients",
                )
            })?;
        let key = self.registrations[&rid].key.clone();
        let claimed: Vec<((ConnId, ClientId), Option<VersionId>)> = self
            .clients
            .iter()
            .filter(|(_, c)| c.key == key && c.secure && c.controller != Some(vid))
            .filter(|(_, c)| self.match_registration(&key, &c.url) == Some(rid))
            .map(|(k, c)| (*k, c.controller))
            .collect();
        let mut released = Vec::new();
        for ((conn, client), old) in claimed {
            if let Some(c) = self.clients.get_mut(&(conn, client)) {
                c.controller = Some(vid);
            }
            self.notify_controller(conn, client);
            // (the registration it used before may now be free to clear or to activate)
            if let Some(r) = old
                .and_then(|v| self.versions.get(&v))
                .map(|v| v.registration)
            {
                if r != rid && !released.contains(&r) {
                    released.push(r);
                }
            }
        }
        for r in released {
            self.registration_released(r, now);
        }
        Ok(())
    }

    /// A client is gone: §8 Handle Service Worker Client Unload.
    fn client_gone(&mut self, conn: ConnId, client: ClientId, now: Instant) {
        let Some(rec) = self.clients.remove(&(conn, client)) else {
            return;
        };
        if let Some(r) = rec
            .controller
            .and_then(|v| self.versions.get(&v))
            .map(|v| v.registration)
        {
            self.registration_released(r, now);
        }
    }

    /// The last steps of Handle Service Worker Client Unload: once no client uses `rid`, an
    /// unregistered registration is cleared (Try Clear Registration) and a waiting worker may
    /// activate (Try Activate).
    fn registration_released(&mut self, rid: RegistrationId, now: Instant) {
        if self.in_use(rid) {
            return;
        }
        if self.registrations.get(&rid).is_some_and(|r| r.unregistered) {
            self.clear_registration(rid);
            return;
        }
        self.try_activate(rid, now);
    }

    /// Answers the readies whose matching registration has an active worker.
    fn check_readies(&mut self) {
        let mut i = 0;
        while i < self.readies.len() {
            let (conn, call, key, url) = self.readies[i].clone();
            let found = self
                .match_registration(&key, &url)
                .filter(|r| self.registrations[r].active.is_some())
                .and_then(|r| self.info(r));
            if let Some(info) = found {
                self.readies.remove(i);
                self.answer(conn, call, Answer::Registration(Ok(info)));
            } else {
                i += 1;
            }
        }
    }

    /// §8 Schedule Job.
    fn schedule(&mut self, mut job: Job) {
        job.id = self.id();
        let q = self
            .queues
            .entry((job.key.clone(), job.scope.as_str().to_owned()))
            .or_default();
        if let Some(last) = q.back_mut() {
            if !last.settled && last.equivalent(&job) {
                last.waiters.append(&mut job.waiters);
                return;
            }
            q.push_back(job);
            return;
        }
        let key = (job.key.clone(), job.scope.as_str().to_owned());
        q.push_back(job);
        self.run_job(key);
    }

    fn job_mut(&mut self, qk: &(String, String)) -> Option<&mut Job> {
        self.queues.get_mut(qk)?.front_mut()
    }

    /// Resolves (or rejects) every promise of the front job of a queue.
    fn settle(&mut self, qk: &(String, String), answer: impl Fn() -> Answer) {
        let Some(job) = self.job_mut(qk) else {
            return;
        };
        if job.settled {
            return;
        }
        job.settled = true;
        let waiters = std::mem::take(&mut job.waiters);
        for (conn, call) in waiters {
            self.answer(conn, call, answer());
        }
    }

    /// §8 Finish Job: the front job leaves its queue and the next one runs.
    fn finish_job(&mut self, qk: &(String, String)) {
        let Some(q) = self.queues.get_mut(qk) else {
            return;
        };
        q.pop_front();
        if q.is_empty() {
            self.queues.remove(qk);
        } else {
            self.run_job(qk.clone());
        }
    }

    fn reject(&mut self, qk: &(String, String), e: JobError) {
        let kind = self.job_mut(qk).map(|j| j.kind);
        if kind == Some(Kind::Unregister) {
            self.settle(qk, || Answer::Unregistered(Err(e.clone())));
        } else {
            self.settle(qk, || Answer::Registration(Err(e.clone())));
        }
        self.finish_job(qk);
    }

    /// §8 Run Job.
    fn run_job(&mut self, qk: (String, String)) {
        let Some(job) = self.job_mut(&qk) else {
            return;
        };
        match job.kind {
            Kind::Register => self.register(qk),
            Kind::Update => self.update(qk),
            Kind::Unregister => self.unregister(qk),
        }
    }

    /// §8 Register.
    fn register(&mut self, qk: (String, String)) {
        let job = self.job_mut(&qk).expect("the job");
        let script_url = job.script_url.clone().expect("a register job's script");
        let scope = job.scope.clone();
        let client_origin = job.client.origin.clone();
        let (uvc, worker_type, key) = (job.update_via_cache, job.worker_type, job.key.clone());
        if !snowlark_net::origin::is_url_potentially_trustworthy(&script_url) {
            return self.reject(
                &qk,
                JobError::security("The script URL is not potentially trustworthy"),
            );
        }
        if script_url.origin() != client_origin {
            return self.reject(
                &qk,
                JobError::security("The script's origin is not the client's"),
            );
        }
        if scope.origin() != client_origin {
            return self.reject(
                &qk,
                JobError::security("The scope's origin is not the client's"),
            );
        }
        match self.get_registration(&key, &scope) {
            Some(rid) => {
                let r = &self.registrations[&rid];
                let same = self.newest(r).is_some_and(|v| {
                    let v = &self.versions[&v];
                    v.script_url == script_url && v.worker_type == worker_type
                }) && r.update_via_cache == uvc;
                if same {
                    let info = self.info(rid).expect("the registration");
                    self.settle(&qk, || Answer::Registration(Ok(info.clone())));
                    return self.finish_job(&qk);
                }
                self.registrations
                    .get_mut(&rid)
                    .expect("the registration")
                    .update_via_cache = uvc;
            }
            None => {
                let rid = self.id();
                self.registrations.insert(
                    rid,
                    Registration {
                        key,
                        unregistered: false,
                        scope,
                        update_via_cache: uvc,
                        installing: None,
                        waiting: None,
                        active: None,
                    },
                );
            }
        }
        self.update(qk)
    }

    /// §8 Update: checks the registration, then fetches the script.
    fn update(&mut self, qk: (String, String)) {
        let job = self.job_mut(&qk).expect("the job");
        let (key, scope, kind) = (job.key.clone(), job.scope.clone(), job.kind);
        let script_url = job.script_url.clone().expect("an update job's script");
        let Some(rid) = self.get_registration(&key, &scope) else {
            return self.reject(&qk, JobError::type_error("The registration is gone"));
        };
        let r = &self.registrations[&rid];
        if kind == Kind::Update {
            if let Some(n) = self.newest(r) {
                if self.versions[&n].script_url != script_url {
                    return self.reject(
                        &qk,
                        JobError::type_error("The registration's script changed"),
                    );
                }
            }
        }
        let uvc = r.update_via_cache;
        let job = self.job_mut(&qk).expect("the job");
        let mut request = snowlark_net::Request::new(script_url);
        request.client = Some(std::sync::Arc::new(job.client.clone()));
        request.destination = snowlark_net::Destination::ServiceWorker;
        request.mode = snowlark_net::Mode::SameOrigin;
        request.credentials = snowlark_net::CredentialsMode::SameOrigin;
        request.redirect = snowlark_net::RedirectMode::Error;
        request.initiator_type = "other".into();
        request.parser_metadata = snowlark_net::ParserMetadata::NotParserInserted;
        // (the main script bypasses the HTTP cache unless the registration says "all")
        if uvc != UpdateViaCache::All {
            request.cache = snowlark_net::CacheMode::NoCache;
        }
        request.headers.append("Service-Worker", "script");
        job.stage = Stage::Fetching;
        let id = job.id;
        self.effects.push(Effect::Fetch {
            job: id,
            request: Box::new(request),
        });
    }

    fn queue_of_job(&self, job: u64) -> Option<(String, String)> {
        self.queues
            .iter()
            .find(|(_, q)| q.front().is_some_and(|j| j.id == job))
            .map(|(k, _)| k.clone())
    }

    /// §8 Update, after the fetch: the response checks, the byte comparison, then a new version
    /// that is started for evaluation.
    pub fn fetched(
        &mut self,
        job: u64,
        result: Result<snowlark_net::Response, String>,
        now: Instant,
    ) {
        let Some(qk) = self.queue_of_job(job) else {
            return;
        };
        let j = self.job_mut(&qk).expect("the job");
        if j.stage != Stage::Fetching {
            return;
        }
        let (key, scope, worker_type) = (j.key.clone(), j.scope.clone(), j.worker_type);
        let script_url = j.script_url.clone().expect("the job's script");
        let response = match result {
            Ok(r) => r,
            Err(e) => {
                return self.fail_update(
                    &qk,
                    JobError::type_error(format!("Fetching the script failed: {e}")),
                )
            }
        };
        if !response.head.ok() {
            return self.fail_update(
                &qk,
                JobError::type_error(format!(
                    "Fetching the script failed with status {}",
                    response.status()
                )),
            );
        }
        let mime = snowlark_net::mime::extract_mime_type(&response.head.headers);
        if !mime.as_ref().is_some_and(|m| m.is_javascript()) {
            return self.fail_update(
                &qk,
                JobError::security("The script's MIME type is not a JavaScript MIME type"),
            );
        }
        // The scope must be under the script's directory, or under `Service-Worker-Allowed`.
        let script_final = response
            .head
            .url_list
            .last()
            .cloned()
            .unwrap_or_else(|| script_url.clone());
        let max_scope = match response.head.headers.get("Service-Worker-Allowed") {
            Some(allowed) => match script_final.join(allowed.trim()) {
                Ok(u) => u,
                Err(_) => {
                    return self.fail_update(
                        &qk,
                        JobError::security("Service-Worker-Allowed is not a URL"),
                    )
                }
            },
            None => script_final.join("./").unwrap_or(script_final.clone()),
        };
        if max_scope.origin() != scope.origin() || !scope.path().starts_with(max_scope.path()) {
            return self.fail_update(
                &qk,
                JobError::security(format!(
                    "The scope {} is not under the script's allowed scope {}",
                    scope, max_scope
                )),
            );
        }
        let source = response.body_bytes().to_vec();
        let Some(rid) = self.get_registration(&key, &scope) else {
            return self.fail_update(&qk, JobError::type_error("The registration is gone"));
        };
        if let Some(n) = self.newest(&self.registrations[&rid]) {
            let v = &self.versions[&n];
            if v.source == source && v.script_url == script_url && v.worker_type == worker_type {
                if v.imports.is_empty() {
                    let info = self.info(rid).expect("the registration");
                    self.settle(&qk, || Answer::Registration(Ok(info.clone())));
                    return self.finish_job(&qk);
                }
                // Step 21: the imported scripts are checked too (each fetched again; the
                // modules of a module worker's graph as well, as its classic imports are).
                let stored = v.imports.clone();
                let uvc = self.registrations[&rid].update_via_cache;
                let j = self.job_mut(&qk).expect("the job");
                let client = std::sync::Arc::new(j.client.clone());
                let id = j.id;
                j.stage = Stage::CheckingImports;
                for (index, s) in stored.iter().enumerate() {
                    let mut request = snowlark_net::Request::new(s.url.clone());
                    request.client = Some(client.clone());
                    request.destination = if worker_type == WorkerType::Module {
                        snowlark_net::Destination::ServiceWorker
                    } else {
                        snowlark_net::Destination::Script
                    };
                    if worker_type == WorkerType::Module {
                        request.mode = snowlark_net::Mode::Cors;
                    }
                    request.parser_metadata = snowlark_net::ParserMetadata::NotParserInserted;
                    request.initiator_type = "other".into();
                    if uvc == UpdateViaCache::None {
                        request.cache = snowlark_net::CacheMode::NoCache;
                    }
                    self.effects.push(Effect::FetchImport {
                        job: id,
                        index,
                        request: Box::new(request),
                    });
                }
                self.job_mut(&qk).expect("the job").check = Some(Box::new(ImportCheck {
                    source,
                    stored: stored.into_iter().map(|s| (s, None)).collect(),
                    changed: false,
                }));
                return;
            }
        }
        self.new_version(&qk, rid, source, Vec::new(), now);
    }

    /// §8 Update step 21, one import's fetch: a good response that differs from the stored one
    /// is an update; a bad one is left out of the check (and of the new worker's map). When the
    /// last is in, the job ends unchanged or goes on with a new version.
    pub fn fetched_import(
        &mut self,
        job: u64,
        index: usize,
        result: Result<snowlark_net::Response, String>,
        now: Instant,
    ) {
        let Some(qk) = self.queue_of_job(job) else {
            return;
        };
        let j = self.job_mut(&qk).expect("the job");
        if j.stage != Stage::CheckingImports {
            return;
        }
        let Some(check) = j.check.as_mut() else {
            return;
        };
        let Some(entry) = check.stored.get_mut(index) else {
            return;
        };
        let good = result.ok().filter(|r| {
            r.head.ok()
                && snowlark_net::mime::extract_mime_type(&r.head.headers)
                    .is_some_and(|m| m.is_javascript())
        });
        let fetched = good.map(|r| ImportedScript {
            url: entry.0.url.clone(),
            final_url: r
                .head
                .url_list
                .last()
                .cloned()
                .unwrap_or(entry.0.url.clone()),
            source: r.body_bytes().to_vec(),
        });
        if fetched.as_ref().is_some_and(|f| f.source != entry.0.source) {
            check.changed = true;
        }
        entry.1 = Some(fetched);
        if check.stored.iter().any(|(_, f)| f.is_none()) {
            return;
        }
        let check = j.check.take().expect("the check");
        let (key, scope) = (j.key.clone(), j.scope.clone());
        let Some(rid) = self.get_registration(&key, &scope) else {
            return self.fail_update(&qk, JobError::type_error("The registration is gone"));
        };
        if !check.changed {
            let info = self.info(rid).expect("the registration");
            self.settle(&qk, || Answer::Registration(Ok(info.clone())));
            return self.finish_job(&qk);
        }
        let imports = check
            .stored
            .into_iter()
            .filter_map(|(_, f)| f.flatten())
            .collect();
        self.new_version(&qk, rid, check.source, imports, now);
    }

    /// §8 Update steps 10-17: a new worker with the job's script and the updated resource map,
    /// started for evaluation.
    fn new_version(
        &mut self,
        qk: &(String, String),
        rid: RegistrationId,
        source: Vec<u8>,
        imports: Vec<ImportedScript>,
        now: Instant,
    ) {
        let qk = qk.clone();
        let j = self.job_mut(&qk).expect("the job");
        let (worker_type, origin_conn) = (j.worker_type, j.origin_conn);
        let script_url = j.script_url.clone().expect("the job's script");
        let vid = self.id();
        self.versions.insert(
            vid,
            Version {
                registration: rid,
                script_url,
                worker_type,
                source,
                imports,
                used: Vec::new(),
                state: WorkerState::Parsed,
                skip_waiting: false,
                running: Running::Stopped,
                runner: None,
                own: None,
                current: None,
                queued: VecDeque::new(),
                since: now,
            },
        );
        self.job_mut(&qk).expect("the job").stage = Stage::Evaluating(vid);
        if !self.start(vid, Some(origin_conn), now) {
            self.versions.remove(&vid);
            self.fail_update(
                &qk,
                JobError::type_error("No event loop can run the service worker"),
            );
        }
    }

    /// The update failed: the job is rejected, and a registration left without any worker is
    /// removed (§8 Update "If ... newestWorker is null, invoke Clear Registration").
    fn fail_update(&mut self, qk: &(String, String), e: JobError) {
        let (key, scope) = qk.clone();
        let scope = Url::parse(&scope).ok();
        if let Some(rid) = scope.and_then(|s| self.get_registration(&key, &s)) {
            if self.newest(&self.registrations[&rid]).is_none() {
                self.registrations.remove(&rid);
            }
        }
        self.reject(qk, e);
    }

    /// Starts a version's worker on a runner of its key (`prefer` first). False when none can.
    fn start(&mut self, vid: VersionId, prefer: Option<ConnId>, now: Instant) -> bool {
        let Some(v) = self.versions.get(&vid) else {
            return false;
        };
        let key = self.registrations[&v.registration].key.clone();
        let runner = prefer
            .filter(|c| self.conns.get(c).is_some_and(|c| c.runner && c.key == key))
            .or_else(|| {
                let mut c: Vec<ConnId> = self
                    .conns
                    .iter()
                    .filter(|(_, c)| c.runner && c.key == key)
                    .map(|(id, _)| *id)
                    .collect();
                c.sort_unstable();
                c.first().copied()
            });
        let Some(runner) = runner else {
            return false;
        };
        let rid = v.registration;
        let notice = Notice::Start {
            version: self.version_info(vid).expect("the version"),
            registration: self.info(rid).expect("the registration"),
            worker_type: v.worker_type,
            source: v.source.clone(),
            imports: v.imports.clone(),
        };
        let v = self.versions.get_mut(&vid).expect("the version");
        v.running = Running::Starting;
        v.runner = Some(runner);
        v.since = now;
        self.effects.push(Effect::Notify(runner, notice));
        true
    }

    /// The runner says whether the version's script ran.
    fn evaluated(&mut self, vid: VersionId, ok: bool, now: Instant) {
        let Some(v) = self.versions.get_mut(&vid) else {
            return;
        };
        if v.running != Running::Starting {
            return;
        }
        let rid = v.registration;
        let evaluating = self.queues.iter().find_map(|(k, q)| {
            q.front()
                .filter(|j| j.stage == Stage::Evaluating(vid))
                .map(|_| k.clone())
        });
        if !ok {
            let v = self.versions.get_mut(&vid).expect("the version");
            v.running = Running::Stopped;
            if let Some(runner) = v.runner.take() {
                self.effects
                    .push(Effect::Notify(runner, Notice::Stop { version: vid }));
            }
            if let Some(qk) = evaluating {
                self.versions.remove(&vid);
                return self.fail_update(
                    &qk,
                    JobError::type_error("The service worker's script threw or did not compile"),
                );
            }
            // (an installed worker that fails to start again: its waiting events fail)
            let v = self.versions.get_mut(&vid).expect("the version");
            let queued: Vec<Lifecycle> = v.queued.drain(..).collect();
            for e in queued {
                self.event_done(vid, e, false, now);
            }
            return;
        }
        let v = self.versions.get_mut(&vid).expect("the version");
        v.running = Running::Started;
        v.since = now;
        if let Some(qk) = evaluating {
            self.install(qk, rid, vid, now);
        } else {
            self.pump(vid, now);
        }
    }

    /// Sends the version's next queued event, starting the worker when it is stopped.
    fn pump(&mut self, vid: VersionId, now: Instant) {
        let Some(v) = self.versions.get_mut(&vid) else {
            return;
        };
        if v.current.is_some() || v.queued.is_empty() {
            return;
        }
        match v.running {
            Running::Starting => {}
            Running::Stopped => {
                if !self.start(vid, None, now) {
                    let v = self.versions.get_mut(&vid).expect("the version");
                    let queued: Vec<Lifecycle> = v.queued.drain(..).collect();
                    for e in queued {
                        self.event_done(vid, e, false, now);
                    }
                }
            }
            Running::Started => {
                let event = v.queued.pop_front().expect("an event");
                v.current = Some((event, now + EVENT_TIMEOUT));
                let runner = v.own.or(v.runner).expect("a running version's runner");
                self.effects.push(Effect::Notify(
                    runner,
                    Notice::Dispatch {
                        version: vid,
                        event,
                    },
                ));
            }
        }
    }

    fn dispatch(&mut self, vid: VersionId, event: Lifecycle, now: Instant) {
        if let Some(v) = self.versions.get_mut(&vid) {
            v.queued.push_back(event);
        }
        self.pump(vid, now);
    }

    /// §8 Update Worker State.
    fn set_state(&mut self, vid: VersionId, state: WorkerState) {
        let Some(v) = self.versions.get_mut(&vid) else {
            return;
        };
        if v.state == state {
            return;
        }
        v.state = state;
        let key = self
            .registrations
            .get(&v.registration)
            .map(|r| r.key.clone());
        if let Some(key) = key {
            self.broadcast(
                &key,
                Notice::State {
                    version: vid,
                    state,
                },
            );
        }
    }

    /// §8 Update Registration State.
    fn set_slot(&mut self, rid: RegistrationId, slot: Slot, vid: Option<VersionId>) {
        let Some(r) = self.registrations.get_mut(&rid) else {
            return;
        };
        match slot {
            Slot::Installing => r.installing = vid,
            Slot::Waiting => r.waiting = vid,
            Slot::Active => r.active = vid,
        }
        let key = r.key.clone();
        let version = vid.and_then(|v| self.version_info(v));
        self.broadcast(
            &key,
            Notice::Slot {
                registration: rid,
                slot,
                version,
            },
        );
    }

    /// Terminates a version's worker and makes it redundant.
    fn retire(&mut self, vid: VersionId) {
        if let Some(v) = self.versions.get_mut(&vid) {
            if v.running != Running::Stopped {
                v.running = Running::Stopped;
                if let Some(runner) = v.runner.take() {
                    self.effects
                        .push(Effect::Notify(runner, Notice::Stop { version: vid }));
                }
            }
            v.current = None;
            v.queued.clear();
        }
        self.set_state(vid, WorkerState::Redundant);
    }

    /// §8 Install, up to the install event.
    fn install(&mut self, qk: (String, String), rid: RegistrationId, vid: VersionId, now: Instant) {
        if let Some(j) = self.job_mut(&qk) {
            j.stage = Stage::Installing(vid);
        }
        self.set_slot(rid, Slot::Installing, Some(vid));
        self.set_state(vid, WorkerState::Installing);
        let info = self.info(rid).expect("the registration");
        self.settle(&qk, || Answer::Registration(Ok(info.clone())));
        let key = self.registrations[&rid].key.clone();
        self.broadcast(&key, Notice::UpdateFound { registration: rid });
        self.dispatch(vid, Lifecycle::Install, now);
    }

    /// A lifecycle event settled (or timed out, or its worker stopped).
    fn event_done(&mut self, vid: VersionId, event: Lifecycle, ok: bool, now: Instant) {
        let Some(v) = self.versions.get_mut(&vid) else {
            return;
        };
        if v.current.map(|c| c.0) == Some(event) {
            v.current = None;
            v.since = now;
        }
        let rid = v.registration;
        match event {
            Lifecycle::Install => {
                let Some(qk) = self.queues.iter().find_map(|(k, q)| {
                    q.front()
                        .filter(|j| j.stage == Stage::Installing(vid))
                        .map(|_| k.clone())
                }) else {
                    return;
                };
                if !ok || !self.registrations.contains_key(&rid) {
                    // §8 Install: the install failed.
                    self.retire(vid);
                    if self
                        .registrations
                        .get(&rid)
                        .is_some_and(|r| r.installing == Some(vid))
                    {
                        self.set_slot(rid, Slot::Installing, None);
                    }
                    if self
                        .registrations
                        .get(&rid)
                        .is_some_and(|r| self.newest(r).is_none())
                    {
                        self.registrations.remove(&rid);
                    }
                    return self.finish_job(&qk);
                }
                // §8 Install steps 13-15: the scripts its evaluation and install did not use
                // leave its map.
                if let Some(v) = self.versions.get_mut(&vid) {
                    let used = std::mem::take(&mut v.used);
                    v.imports.retain(|s| used.contains(&s.url));
                    v.used = used;
                }
                if let Some(old) = self.registrations[&rid].waiting {
                    self.retire(old);
                }
                self.set_slot(rid, Slot::Waiting, Some(vid));
                self.set_slot(rid, Slot::Installing, None);
                self.set_state(vid, WorkerState::Installed);
                self.finish_job(&qk);
                self.try_activate(rid, now);
            }
            Lifecycle::Activate => {
                // (an activate event's failure changes nothing: §8 Activate)
                if self.versions.get(&vid).map(|v| v.state) == Some(WorkerState::Activating) {
                    self.set_state(vid, WorkerState::Activated);
                    let key = self.registrations.get(&rid).map(|r| r.key.clone());
                    if let Some(key) = key {
                        self.effects.push(Effect::Persist(key));
                    }
                }
                self.check_readies();
                // (a waiting worker that came while this one activated)
                self.try_activate(rid, now);
            }
        }
        self.pump(vid, now);
    }

    /// §8 Try Activate: the waiting worker activates when there is no active worker, when no
    /// client uses the registration, or when it called `skipWaiting()`.
    fn try_activate(&mut self, rid: RegistrationId, now: Instant) {
        let Some(r) = self.registrations.get(&rid) else {
            return;
        };
        let Some(waiting) = r.waiting else {
            return;
        };
        if let Some(active) = r.active {
            if self.versions[&active].state == WorkerState::Activating {
                return;
            }
            if !self.versions[&waiting].skip_waiting && self.in_use(rid) {
                return;
            }
        }
        if self.versions[&waiting].state != WorkerState::Installed {
            return;
        }
        self.activate(rid, now);
    }

    /// §8 Activate.
    fn activate(&mut self, rid: RegistrationId, now: Instant) {
        let r = &self.registrations[&rid];
        let Some(vid) = r.waiting else {
            return;
        };
        let old = r.active;
        // (the clients using the registration, which the new active worker controls)
        let using: Vec<(ConnId, ClientId)> = self
            .clients
            .iter()
            .filter(|(_, c)| {
                c.controller
                    .and_then(|v| self.versions.get(&v))
                    .is_some_and(|v| v.registration == rid)
            })
            .map(|(k, _)| *k)
            .collect();
        if let Some(old) = old {
            self.retire(old);
        }
        self.set_slot(rid, Slot::Active, Some(vid));
        self.set_slot(rid, Slot::Waiting, None);
        self.set_state(vid, WorkerState::Activating);
        for (conn, client) in using {
            if let Some(c) = self.clients.get_mut(&(conn, client)) {
                c.controller = Some(vid);
            }
            self.notify_controller(conn, client);
        }
        self.dispatch(vid, Lifecycle::Activate, now);
    }

    /// §8 Unregister.
    fn unregister(&mut self, qk: (String, String)) {
        let job = self.job_mut(&qk).expect("the job");
        let (key, scope) = (job.key.clone(), job.scope.clone());
        if scope.origin() != job.client.origin {
            return self.reject(
                &qk,
                JobError::security("The scope's origin is not the client's"),
            );
        }
        let Some(rid) = self.get_registration(&key, &scope) else {
            self.settle(&qk, || Answer::Unregistered(Ok(false)));
            return self.finish_job(&qk);
        };
        // (it leaves the registration map; its workers stay while a client uses it)
        if let Some(r) = self.registrations.get_mut(&rid) {
            r.unregistered = true;
        }
        self.settle(&qk, || Answer::Unregistered(Ok(true)));
        // §8 Try Clear Registration
        if !self.in_use(rid) {
            self.clear_registration(rid);
        }
        self.effects.push(Effect::Persist(key));
        self.finish_job(&qk);
    }

    /// §8 Clear Registration: the registration leaves the map and its workers become redundant.
    fn clear_registration(&mut self, rid: RegistrationId) {
        let Some(r) = self.registrations.get(&rid) else {
            return;
        };
        let (i, w, a) = (r.installing, r.waiting, r.active);
        if let Some(v) = i {
            self.retire(v);
            self.set_slot(rid, Slot::Installing, None);
        }
        if let Some(v) = w {
            self.retire(v);
            self.set_slot(rid, Slot::Waiting, None);
        }
        if let Some(v) = a {
            self.retire(v);
            self.set_slot(rid, Slot::Active, None);
        }
        self.registrations.remove(&rid);
    }

    /// A version's worker ended (its runner went away, or it timed out): its event fails.
    fn stopped(&mut self, vid: VersionId, now: Instant) {
        let Some(v) = self.versions.get_mut(&vid) else {
            return;
        };
        let was_starting = v.running == Running::Starting;
        v.running = Running::Stopped;
        v.runner = None;
        v.own = None;
        if was_starting {
            return self.evaluated_failed_start(vid, now);
        }
        if let Some((event, _)) = v.current.take() {
            self.event_done(vid, event, false, now);
        }
    }

    fn evaluated_failed_start(&mut self, vid: VersionId, now: Instant) {
        if let Some(v) = self.versions.get_mut(&vid) {
            v.running = Running::Starting;
        }
        self.evaluated(vid, false, now);
    }

    /// Timers: events past their deadline fail, workers that failed to start in time stop, and
    /// idle workers stop. Returns when to call again.
    pub fn tick(&mut self, now: Instant) -> Option<Instant> {
        let ids: Vec<VersionId> = self.versions.keys().copied().collect();
        let mut next: Option<Instant> = None;
        let mut at = |t: Instant| next = Some(next.map_or(t, |n: Instant| n.min(t)));
        for vid in ids {
            let Some(v) = self.versions.get(&vid) else {
                continue;
            };
            match (v.running, v.current) {
                (Running::Starting, _) => {
                    if now >= v.since + START_TIMEOUT {
                        self.evaluated(vid, false, now);
                    } else {
                        at(v.since + START_TIMEOUT);
                    }
                }
                (Running::Started, Some((event, deadline))) => {
                    if now >= deadline {
                        self.event_done(vid, event, false, now);
                    } else {
                        at(deadline);
                    }
                }
                (Running::Started, None) if v.queued.is_empty() => {
                    if now >= v.since + IDLE_TIMEOUT {
                        let v = self.versions.get_mut(&vid).expect("the version");
                        v.running = Running::Stopped;
                        if let Some(runner) = v.runner.take() {
                            self.effects
                                .push(Effect::Notify(runner, Notice::Stop { version: vid }));
                        }
                    } else {
                        at(v.since + IDLE_TIMEOUT);
                    }
                }
                _ => {}
            }
        }
        // (versions nothing refers to any more)
        let live: std::collections::HashSet<VersionId> = self
            .registrations
            .values()
            .flat_map(|r| [r.installing, r.waiting, r.active])
            .flatten()
            .chain(self.clients.values().filter_map(|c| c.controller))
            .chain(self.queues.values().filter_map(|q| match q.front()?.stage {
                Stage::Evaluating(v) | Stage::Installing(v) => Some(v),
                _ => None,
            }))
            .collect();
        self.versions
            .retain(|id, v| live.contains(id) || v.running != Running::Stopped);
        next
    }

    /// The version's registration and script, for a worker that asks again (tests).
    pub fn version(&self, vid: VersionId) -> Option<VersionInfo> {
        self.version_info(vid)
    }
}

#[cfg(test)]
mod tests;
