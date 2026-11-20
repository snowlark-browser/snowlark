// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The remote services against a real second process: this test binary, run again with
//! `services::tests::child_main` as its only test and a role in the environment.

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use snowlark_ipc::{connect_to_browser, Bulk, Error, Launch, Process, ProcessKind, ProcessPolicy};
use snowlark_js::code_cache::{CacheKind, CodeCache, CodeCacheStore};
use snowlark_page::web_storage::{StorageNotice, StorageWrite, WebStorage, WebStorageHost};
use snowlark_page::{BroadcastBackend, DocumentFilters, Inbox, LocalBroadcast};

use super::*;

const ROLE: &str = "SNOWLARK_SERVICES_TEST_ROLE";
/// What a role is given besides (an address, say).
const ARG: &str = "SNOWLARK_SERVICES_TEST_ARG";
pub(crate) const WAIT: Duration = Duration::from_secs(20);

pub(crate) fn policy() -> ProcessPolicy {
    ProcessPolicy {
        process: 1,
        kind: ProcessKind::Renderer,
        lock: Default::default(),
    }
}

/// A renderer in another process, served by the browser's `local` services on a thread of its own.
struct Child {
    host: Arc<ServiceHost>,
    process: Arc<Process>,
    served: thread::JoinHandle<Error>,
}

fn start(role: &str, local: &LocalServices) -> Child {
    start_with(role, local, "")
}

/// [`start`], with `arg` in the child's [`ARG`].
fn start_with(role: &str, local: &LocalServices, arg: &str) -> Child {
    start_as(role, local, arg, policy())
}

/// [`start_with`], with the browser's policy for the child.
fn start_as(role: &str, local: &LocalServices, arg: &str, policy: ProcessPolicy) -> Child {
    let (port, process) = Launch::new(std::env::current_exe().unwrap())
        .arg("services::tests::child_main")
        .arg("--exact")
        .arg("--nocapture")
        .env(ROLE, role)
        .env(ARG, arg)
        .spawn()
        .unwrap()
        .connect::<Down, Up>(policy, 64 << 20, WAIT)
        .unwrap();
    let host = Arc::new(ServiceHost::new(port, local));
    let served = {
        let host = host.clone();
        thread::spawn(move || host.run())
    };
    Child {
        host,
        process,
        served,
    }
}

/// Waits until `done` holds, or fails the test.
fn until(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < WAIT, "waited for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// An inbox, and what wakes when something arrives in it.
fn inbox() -> (Arc<Inbox>, mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    let inbox = Arc::new(Inbox::new(Arc::new(move || {
        let _ = tx.send(());
    })));
    (inbox, rx)
}

/// Waits for `n` deliveries in `inbox`.
fn receive(
    inbox: &Inbox,
    woken: &mpsc::Receiver<()>,
    n: usize,
) -> Vec<(u64, Arc<Vec<u8>>, String)> {
    let mut got = Vec::new();
    while got.len() < n {
        woken.recv_timeout(WAIT).expect("a delivery");
        got.extend(inbox.take());
    }
    assert_eq!(got.len(), n);
    got
}

/// What each role does. Not a test of its own: without the variable it does nothing.
#[test]
fn child_main() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let port: RendererPort = connect_to_browser(WAIT).unwrap();
    let code = match role.as_str() {
        "broadcast-echo" => broadcast_echo(&port),
        "storage" => storage_child(&port),
        "code-cache" => code_cache_child(&port),
        "forge-storage-client" => {
            let write = Bulk::Inline(vec![]);
            port.send(&Up::Storage(storage::Up::Write { client: 5, write }))
                .unwrap();
            linger()
        }
        "forge-storage-write" => {
            port.send(&Up::Storage(storage::Up::Connect { client: 1 }))
                .unwrap();
            let write = Bulk::Inline(vec![200, 200, 200]);
            port.send(&Up::Storage(storage::Up::Write { client: 1, write }))
                .unwrap();
            linger()
        }
        "forge-cache-kind" => {
            port.send(&Up::CodeCache(code_cache::Up::Store {
                url: "u".into(),
                kind: 9,
                hash: [0; 32],
                data: Bulk::Inline(vec![1]),
            }))
            .unwrap();
            linger()
        }
        "forge-post" => {
            port.send(&Up::Broadcast(broadcast::Up::Post {
                from: 7,
                key: "k".into(),
                origin: "o".into(),
                name: "n".into(),
                data: Bulk::Inline(vec![1]),
            }))
            .unwrap();
            linger()
        }
        "forge-subscribe-twice" => {
            for _ in 0..2 {
                port.send(&Up::Broadcast(broadcast::Up::Subscribe {
                    id: 1,
                    key: "k".into(),
                    name: "n".into(),
                }))
                .unwrap();
            }
            linger()
        }
        "forge-unsubscribe" => {
            port.send(&Up::Broadcast(broadcast::Up::Unsubscribe { id: 3 }))
                .unwrap();
            linger()
        }
        "embedder" => embedder_child(&port),
        "net" => net_child(&port, &std::env::var(ARG).unwrap()),
        "net-threads" => net_threads_child(&port),
        "net-keepalive" => {
            let base = std::env::var(ARG).unwrap();
            let services = remote(&port);
            let client = snowlark_net::NetClient::with_backend(services.net.clone());
            let mut keepalive =
                snowlark_net::Request::new(format!("{base}/a?keepalive").parse().unwrap());
            keepalive.keepalive = true;
            let plain = snowlark_net::Request::new(format!("{base}/a?plain").parse().unwrap());
            let streams = [client.fetch(plain), client.fetch(keepalive)];
            // The renderer ends with both in flight and never lets go of them, as a crash would.
            thread::sleep(Duration::from_millis(100));
            std::mem::forget(streams);
            std::process::exit(0)
        }
        "forge-fetch-twice" => {
            let table = std::sync::Mutex::new(snowlark_net::wire::OriginTable::renderer());
            let request = snowlark_net::Request::new("https://a.test/".parse().unwrap());
            for _ in 0..2 {
                let request = net::pack_with(&port, &table, &request).unwrap();
                port.send(&Up::Net(net::Up::Fetch { id: 0, request }))
                    .unwrap();
            }
            linger()
        }
        "forge-request" => {
            let request = Bulk::Inline(vec![7; 40]);
            port.send(&Up::Net(net::Up::Fetch { id: 0, request }))
                .unwrap();
            linger()
        }
        // A fetch as a page of the origin in ARG (M5 step 4: the site lock).
        "fetch-as" => {
            let page: snowlark_net::Url = std::env::var(ARG).unwrap().parse().unwrap();
            let client = snowlark_net::NetClient::with_backend(remote(&port).net.clone());
            let request = snowlark_net::Request::subresource(
                "http://127.0.0.1:1/x".parse().unwrap(),
                snowlark_net::Destination::Image,
                Arc::new(snowlark_net::Client::for_url(&page)),
            );
            let _ = client.fetch(request).read_all();
            linger()
        }
        // A stream the browser never handed over (M5).
        "forge-adopt" => {
            port.send(&Up::Net(net::Up::Adopt {
                id: net::HANDED_FROM + 5,
            }))
            .unwrap();
            linger()
        }
        // A fetch of the renderer's own with an id in the range of the browser's hand-overs.
        "forge-fetch-in-the-browsers-range" => {
            let services = remote(&port);
            let client = snowlark_net::NetClient::with_backend(services.net.clone());
            let request = snowlark_net::Request::new("http://127.0.0.1:65530/x".parse().unwrap());
            let packed = net::pack_with(
                &port,
                &std::sync::Mutex::new(snowlark_net::wire::OriginTable::renderer()),
                &request,
            )
            .unwrap();
            port.send(&Up::Net(net::Up::Fetch {
                id: net::HANDED_FROM,
                request: packed,
            }))
            .unwrap();
            drop(client);
            linger()
        }
        // M5: the stream the browser handed over as its first (the rest of a navigation's
        // response, whose head the browser took), read to the end.
        "adopt" => adopt_child(&port),
        "forge-question" => {
            port.send(&Up::Embedder(embedder::Up::ClipboardText))
                .unwrap();
            linger()
        }
        "forge-filters-url" => {
            let message = Up::Embedder(embedder::Up::DocumentFilters {
                url: "not an address".into(),
                top: "https://a.test/".into(),
            });
            let _ = port.call::<Bulk>(&message, WAIT);
            linger()
        }
        // The keys of the storage services as the site in ARG (M5 step 4: the site lock); the
        // calls among them exit 7 when answered.
        "idb-as" => {
            let key = std::env::var(ARG).unwrap();
            let call = |c: &idb::Call| Up::Idb(idb::Up(pack(&port, c).unwrap()));
            port.send(&call(&idb::Call::RegisterLoop { loop_: 1 }))
                .unwrap();
            port.send(&call(&idb::Call::Open {
                loop_: 1,
                req: 1,
                key,
                name: "x".into(),
                version: None,
                upgrade: 1,
            }))
            .unwrap();
            linger()
        }
        "broadcast-as" => {
            port.send(&Up::Broadcast(broadcast::Up::Subscribe {
                id: 1,
                key: std::env::var(ARG).unwrap(),
                name: "c".into(),
            }))
            .unwrap();
            linger()
        }
        "code-cache-as" => {
            let message = Up::CodeCache(code_cache::Up::Has {
                url: std::env::var(ARG).unwrap(),
            });
            match port.call::<bool>(&message, WAIT) {
                Ok(_) => 7,
                Err(_) => linger(),
            }
        }
        "internal-page-as" => {
            let query = crate::driver::InternalQuery::Page {
                url: std::env::var(ARG).unwrap(),
            };
            let message = Up::Embedder(embedder::Up::Internal(pack(&port, &query).unwrap()));
            match port.call::<Bulk>(&message, WAIT) {
                Ok(_) => 7,
                Err(_) => linger(),
            }
        }
        // An honest fetch of the URL in ARG, as a page of `https://a.test`, or (ARG `file`) of a
        // script file of its own as a page of that file (the network serves `file:` to no web page,
        // and the response must pass the opaque-response check): 7 with a body, 0 without.
        "fetch-url" => {
            let client = snowlark_net::NetClient::with_backend(remote(&port).net.clone());
            let arg = std::env::var(ARG).unwrap();
            let url: snowlark_net::Url = if arg == "file" {
                let path =
                    std::env::temp_dir().join(format!("snowlark-evil-{}.js", std::process::id()));
                std::fs::write(&path, "1;").unwrap();
                snowlark_net::Url::from_file_path(&path).unwrap()
            } else {
                arg.parse().unwrap()
            };
            let page = if url.scheme() == "file" {
                url.clone()
            } else {
                "https://a.test/page".parse().unwrap()
            };
            let request = snowlark_net::Request::subresource(
                url,
                snowlark_net::Destination::Image,
                Arc::new(snowlark_net::Client::for_url(&page)),
            );
            match client.fetch(request).read_all() {
                Ok(response) if !response.body_bytes().is_empty() => 7,
                Ok(response) => {
                    eprintln!("[fetch-url] {} with no body", response.head.status);
                    0
                }
                Err(e) => {
                    eprintln!("[fetch-url] {e}");
                    0
                }
            }
        }
        "idb" => idb_child(&port),
        "idb-dies" => idb_dies_child(&port),
        "idb-vc-a" => idb_versionchange_child(&port),
        "idb-vc-b" => idb_upgrading_child(&port),
        "idb-del-a" => idb_holding_child(&port),
        "idb-del-b" => idb_deleting_child(&port),
        "idb-pipelined" => idb_pipelined_child(&port),
        "idb-unanswered" => idb_unanswered_child(&port),
        "idb-generated" => idb_generated_child(&port),
        "idb-disk" => idb_disk_child(&port),
        "forge-idb-txn" | "forge-idb-conn" | "forge-idb-key" | "forge-idb-mode"
        | "forge-idb-loop" | "forge-idb-txn-id" => idb_forge(&port, &role),
        "audio" => audio_child(&port),
        "forge-audio-call" => {
            let _ = port.call::<Bulk>(&Up::Audio(audio::Up::Audible(true)), WAIT);
            linger()
        }
        "audio-out" => audio_out_child(&port),
        "audio-out-exit" => audio_out_exit_child(&port),
        "forge-audio-out-stream"
        | "forge-audio-out-size"
        | "forge-audio-out-credits"
        | "forge-audio-out-id" => audio_out_forge(&port, &role),
        "cdm" => cdm_child(&port),
        "forge-cdm-session" | "forge-cdm-keys" | "forge-cdm-size" | "forge-cdm-twice"
        | "forge-cdm-release" => cdm_forge(&port, &role),
        other => panic!("role {other}"),
    };
    std::process::exit(code);
}

/// Waits to be killed.
fn linger() -> i32 {
    thread::sleep(Duration::from_secs(600));
    0
}

/// Three channels, one of which hears the browser's: each message from `https://tab` it echoes
/// back with `echo:` in front, `stop` ends it, and its exit code is 10 plus the number of messages
/// it heard from other children.
fn broadcast_echo(port: &RendererPort) -> i32 {
    let services = Arc::new(RemoteServices::new(port));
    {
        let services = services.clone();
        let port = port.clone();
        thread::spawn(move || services.run(&port));
    }
    let b = &services.broadcast;
    let (inbox, woken) = inbox();
    let chan = b.subscribe("key", "chan", &inbox);
    // (another name, another key: neither hears it)
    b.subscribe("key", "other", &inbox);
    b.subscribe("key2", "chan", &inbox);
    let mut from_children = 0;
    loop {
        if woken.recv_timeout(WAIT).is_err() {
            return 4;
        }
        for (id, data, origin) in inbox.take() {
            if id != chan {
                return 5;
            }
            match origin.as_str() {
                "https://tab" if data[..] == *b"stop" => {
                    port.flush(WAIT).unwrap();
                    return 10 + from_children;
                }
                "https://tab" => {
                    let mut echo = b"echo:".to_vec();
                    echo.extend_from_slice(&data);
                    b.post(chan, "key", "https://child", "chan", echo);
                }
                _ => from_children += 1,
            }
        }
    }
}

/// The browser's own services, each also as itself.
pub(crate) struct Local {
    broadcast: Arc<LocalBroadcast>,
    pub(crate) storage: Arc<WebStorageHost>,
    code_cache: Arc<CodeCache>,
    clipboard: Arc<crate::driver::MemoryClipboard>,
    pub(crate) idb: snowlark_idb::LocalIdb,
    /// The browser's network thread, kept running.
    _net: snowlark_net::NetThread,
    pub(crate) services: LocalServices,
}

/// Blocks every request whose address has `blocked` in it.
struct TestRequestFilter;

impl snowlark_net::RequestFilter for TestRequestFilter {
    fn check(&self, request: &snowlark_net::Request) -> snowlark_net::FilterVerdict {
        if request.url().as_str().contains("blocked") {
            snowlark_net::FilterVerdict {
                action: snowlark_net::FilterAction::Block("test".into()),
                csp: None,
            }
        } else {
            snowlark_net::FilterVerdict::allow()
        }
    }
}

pub(crate) fn local() -> Local {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "snowlark-services-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let broadcast = Arc::new(LocalBroadcast::default());
    let storage = WebStorageHost::new();
    let code_cache = CodeCache::open(dir, 64 << 20);
    let clipboard = Arc::new(crate::driver::MemoryClipboard::default());
    let net = snowlark_net::NetThread::start(snowlark_net::NetConfig::default()).unwrap();
    let idb = snowlark_idb::LocalIdb::in_memory();
    idb.set_quota(IDB_QUOTA);
    Local {
        services: LocalServices {
            broadcast: broadcast.clone(),
            storage: storage.clone(),
            code_cache: code_cache.clone(),
            clipboard: clipboard.clone(),
            internal: Arc::new(TestPages),
            content_filter: Some(Arc::new(TestFilter)),
            net: net.client().backend().clone(),
            request_filter: Some(Arc::new(TestRequestFilter)),
            idb: Arc::new(idb.clone()),
            audible: Arc::new(|_| {}),
            muted: false,
            audio_out: None,
            videos: None,
            site_lock: site_lock::Mode::Shadow,
            cdm: None,
        },
        idb,
        broadcast,
        storage,
        code_cache,
        clipboard,
        _net: net,
    }
}

/// Past a frame, so it crosses as a section.
fn big_text() -> String {
    (0..2 << 20)
        .map(|i| (b'a' + (i % 26) as u8) as char)
        .collect()
}

/// `snowlark://big` is a page of [`big_text`]; a view-source page says how long its body was.
struct TestPages;

impl crate::driver::InternalPages for TestPages {
    fn request(&self, request: crate::driver::InternalQuery) -> crate::driver::InternalAnswer {
        use crate::driver::{InternalAnswer, InternalKind, InternalQuery};
        match request {
            InternalQuery::Page { url } => InternalAnswer::Page {
                html: (url == "snowlark://big").then(big_text),
                shown: url,
            },
            InternalQuery::ViewSource { body, .. } => InternalAnswer::ViewSource {
                html: format!("{} bytes", body.len()),
            },
            InternalQuery::Kind { .. } => InternalAnswer::Kind(InternalKind::About),
            InternalQuery::ViewSourceTarget { url } => InternalAnswer::ViewSourceTarget(Some(url)),
        }
    }
}

/// Gives every document a procedural filter with a regular expression and an exception; its generic
/// selectors are each class and id but the excepted ones.
struct TestFilter;

impl snowlark_page::ContentFilter for TestFilter {
    fn document_filters(
        &self,
        _: &snowlark_net::Url,
        _: &snowlark_net::Url,
    ) -> Option<DocumentFilters> {
        use snowlark_page::content_filter::{
            ProceduralAction, ProceduralFilter, ProceduralStep, TextPattern,
        };
        Some(DocumentFilters {
            hide: vec![".banner".into()],
            procedural: vec![ProceduralFilter {
                steps: vec![
                    ProceduralStep::Css("div".into()),
                    ProceduralStep::HasText(TextPattern::parse("/^ad$/i").unwrap()),
                ],
                action: ProceduralAction::Remove,
            }],
            generic: true,
            exceptions: [".keep".to_owned()].into_iter().collect(),
            ..DocumentFilters::default()
        })
    }

    fn generic_selectors(
        &self,
        filters: &DocumentFilters,
        classes: &[String],
        ids: &[String],
    ) -> Vec<String> {
        let classes = classes.iter().map(|c| format!(".{c}"));
        let ids = ids.iter().map(|i| format!("#{i}"));
        classes
            .chain(ids)
            .filter(|s| !filters.exceptions.contains(s))
            .collect()
    }
}

fn broadcast_local() -> (Arc<LocalBroadcast>, LocalServices) {
    let l = local();
    (l.broadcast, l.services)
}

#[test]
fn broadcast_reaches_a_renderer_and_back_at_every_size() {
    let (local, services) = broadcast_local();
    let child = start("broadcast-echo", &services);
    until("the child's channels", || {
        child.host.broadcast.channels() == 3
    });
    let (tab, woken) = inbox();
    let me = local.subscribe("key", "chan", &tab);
    for size in [5, snowlark_ipc::INLINE_MAX, 3 << 20] {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        local.post(me, "key", "https://tab", "chan", data.clone());
        let got = receive(&tab, &woken, 1);
        let (id, echo, origin) = &got[0];
        assert_eq!((*id, origin.as_str()), (me, "https://child"), "{size}");
        assert_eq!(echo[..5], *b"echo:", "{size}");
        assert!(echo[5..] == data[..], "{size}");
    }
    local.post(me, "key", "https://tab", "chan", b"stop".to_vec());
    assert_eq!(child.process.wait(WAIT), Some(10));
    assert!(matches!(child.served.join().unwrap(), Error::Closed));
    // The renderer's channels leave the hub with it.
    let host = Arc::into_inner(child.host).expect("only the test holds the host");
    let left = Arc::downgrade(&host.broadcast.inbox);
    drop(host);
    assert!(left.upgrade().is_none());
    local.post(me, "key", "https://tab", "chan", b"to nobody".to_vec());
}

#[test]
fn broadcast_reaches_one_renderer_from_another() {
    let (local, services) = broadcast_local();
    let a = start("broadcast-echo", &services);
    let b = start("broadcast-echo", &services);
    until("the children's channels", || {
        a.host.broadcast.channels() == 3 && b.host.broadcast.channels() == 3
    });
    let (tab, woken) = inbox();
    let me = local.subscribe("key", "chan", &tab);
    local.post(me, "key", "https://tab", "chan", b"hello".to_vec());
    // Each child's echo reaches the tab and the other child.
    for (_, echo, origin) in receive(&tab, &woken, 2) {
        assert_eq!(
            (echo.as_slice(), origin.as_str()),
            (&b"echo:hello"[..], "https://child")
        );
    }
    local.post(me, "key", "https://tab", "chan", b"stop".to_vec());
    assert_eq!(a.process.wait(WAIT), Some(11));
    assert_eq!(b.process.wait(WAIT), Some(11));
}

#[test]
fn a_renderer_that_breaks_a_service_rule_dies() {
    for role in [
        "forge-post",
        "forge-subscribe-twice",
        "forge-unsubscribe",
        "forge-storage-client",
        "forge-storage-write",
        "forge-cache-kind",
        "forge-question",
        "forge-filters-url",
        "forge-fetch-twice",
        "forge-request",
        "forge-audio-call",
        "forge-adopt",
        "forge-fetch-in-the-browsers-range",
    ] {
        let (_, services) = broadcast_local();
        let child = start(role, &services);
        assert_eq!(child.process.wait(WAIT), Some(9), "{role}");
        assert!(
            matches!(child.served.join().unwrap(), Error::BadMessage(_)),
            "{role}"
        );
    }
}

/// M5 step 4: a renderer locked to a site that fetches as a page of another site dies when the
/// lock is enforced; in shadow mode the fetch is served and the failed check counted on the lock;
/// a fetch as the lock's own site, or a subdomain of it, is never a failed check.
#[test]
fn a_renderer_locked_to_a_site_fetches_as_that_site_only() {
    let locked = || {
        let policy = policy();
        policy.lock.lock_to("https://a.test").unwrap();
        policy
    };
    let (_, mut services) = broadcast_local();
    services.site_lock = site_lock::Mode::Enforce;
    let child = start_as("fetch-as", &services, "https://b.test/page", locked());
    assert_eq!(child.process.wait(WAIT), Some(9));
    assert!(matches!(child.served.join().unwrap(), Error::BadMessage(_)));
    let child = start_as("fetch-as", &services, "https://www.a.test/page", locked());
    assert_eq!(
        child.process.wait(Duration::from_secs(2)),
        None,
        "an honest renderer lives"
    );
    assert_eq!(child.host.policy().lock.noted(), 0);
    drop(child);

    let (_, mut services) = broadcast_local();
    services.site_lock = site_lock::Mode::Shadow;
    let child = start_as("fetch-as", &services, "https://b.test/page", locked());
    until("the failed check is noted", || {
        child.host.policy().lock.noted() == 1
    });
    assert_eq!(
        child.process.wait(Duration::from_secs(1)),
        None,
        "shadow mode serves"
    );
}

/// M5 step 4: the keys a renderer names to the storage services (an IndexedDB storage key, a
/// broadcast channel's, a code cache entry's partition) and the browser's own pages it asks for
/// are checked against its site lock: a forged one kills it under `enforce`, an honest one is
/// served with nothing noted, and under `shadow` the forgery is served and noted.
#[test]
fn a_renderer_locked_to_a_site_keeps_to_its_own_keys() {
    let locked = || {
        let policy = policy();
        policy.lock.lock_to("https://a.test").unwrap();
        policy
    };
    for (role, forged, honest, answered) in [
        ("idb-as", "https://b.test", "https://a.test", None),
        ("broadcast-as", "https://b.test", "https://sub.a.test", None),
        (
            "code-cache-as",
            "https://b.test https://cdn.test/s.js",
            "https://a.test https://cdn.test/s.js",
            Some(7),
        ),
        (
            "internal-page-as",
            "snowlark:settings",
            "about:credits",
            Some(7),
        ),
    ] {
        let (_, mut services) = broadcast_local();
        services.site_lock = site_lock::Mode::Enforce;
        let child = start_as(role, &services, forged, locked());
        assert_eq!(child.process.wait(WAIT), Some(9), "{role}");
        assert!(
            matches!(child.served.join().unwrap(), Error::BadMessage(_)),
            "{role}"
        );
        let child = start_as(role, &services, honest, locked());
        assert_eq!(
            child.process.wait(Duration::from_secs(2)),
            answered,
            "{role}: an honest renderer is served"
        );
        assert_eq!(child.host.policy().lock.noted(), 0, "{role}");
    }
    let (_, mut services) = broadcast_local();
    services.site_lock = site_lock::Mode::Shadow;
    let child = start_as("idb-as", &services, "https://b.test", locked());
    until("the failed check is noted", || {
        child.host.policy().lock.noted() == 1
    });
    assert_eq!(
        child.process.wait(Duration::from_secs(1)),
        None,
        "shadow mode serves"
    );
}

/// M5 step 4: a `file:` request from a renderer locked to a web site is refused with a network
/// error, not a reason to end it (an honest page's `file:` stylesheet sends one); a renderer not
/// locked yet reads the file.
#[test]
fn a_renderer_locked_to_a_web_site_has_its_file_requests_refused_not_ended() {
    let locked = policy();
    locked.lock.lock_to("https://a.test").unwrap();
    // (`l` keeps the browser's network thread alive)
    let l = local();
    let mut services = l.services.clone();
    services.site_lock = site_lock::Mode::Enforce;
    let child = start_as("fetch-url", &services, "file", locked);
    assert_eq!(child.process.wait(WAIT), Some(0), "refused, alive");
    let child = start_as("fetch-url", &services, "file", policy());
    assert_eq!(child.process.wait(WAIT), Some(7), "not locked yet: read");
}

const ORIGIN: &str = "https://a.test";

fn set(key: &str, value: String) -> StorageWrite {
    StorageWrite::Set {
        origin: ORIGIN.into(),
        key: key.into(),
        value,
        url: format!("{ORIGIN}/"),
    }
}

/// Runs the renderer's services on a thread of their own, as its IO thread would.
fn remote(port: &RendererPort) -> Arc<RemoteServices> {
    let services = Arc::new(RemoteServices::new(port));
    let (s, port) = (services.clone(), port.clone());
    thread::spawn(move || s.run(&port));
    services
}

/// A client of the browser's storage. It reads `big` from the area and writes it back as `echo`
/// with `!` after it; a `ping` from another client it answers with `pong`, saying whether it heard
/// its own `echo` as its own; `stop` ends it.
fn storage_child(port: &RendererPort) -> i32 {
    let services = remote(port);
    let (tx, heard) = mpsc::channel();
    let s = &services.storage;
    let me = s.connect(Arc::new(move |n| {
        let _ = tx.send(n);
    }));
    let items = s.open(me, ORIGIN);
    let Some((_, big)) = items.into_iter().find(|(k, _)| k == "big") else {
        return 20;
    };
    s.write(me, set("echo", big + "!"));
    let mut mine = false;
    loop {
        let Ok(StorageNotice::Changed { key, by, .. }) = heard.recv_timeout(WAIT) else {
            return 21;
        };
        match key.as_deref() {
            Some("echo") => mine = by == me,
            Some("ping") if by == 0 => s.write(me, set("pong", mine.to_string())),
            // (its own pong, as the answer to its write)
            Some("pong") if by == me => {}
            Some("stop") => {
                port.flush(WAIT).unwrap();
                return 0;
            }
            _ => return 22,
        }
    }
}

/// Waits for a change of `key` in the notices `heard`, and returns its new value.
fn changed(heard: &mpsc::Receiver<StorageNotice>, key: &str) -> Option<String> {
    loop {
        let notice = heard.recv_timeout(WAIT).expect("a notice");
        if let StorageNotice::Changed {
            key: Some(k),
            new_value,
            ..
        } = notice
        {
            if k == key {
                return new_value;
            }
        }
    }
}

#[test]
fn storage_reaches_a_renderer_and_back() {
    let l = local();
    let (tx, heard) = mpsc::channel();
    let tab = l.storage.connect(Arc::new(move |n| {
        let _ = tx.send(n);
    }));
    l.storage.open(tab, ORIGIN);
    // Past a frame, so the area and the echo both go as sections.
    let big: String = (0..2 << 20)
        .map(|i| (b'a' + (i % 26) as u8) as char)
        .collect();
    l.storage.write(tab, set("big", big.clone()));
    assert_eq!(changed(&heard, "big").as_ref(), Some(&big));

    let child = start("storage", &l.services);
    assert_eq!(changed(&heard, "echo"), Some(big + "!"));
    l.storage.write(tab, set("ping", "1".into()));
    assert_eq!(changed(&heard, "pong").as_deref(), Some("true"));
    assert_eq!(child.host.storage.clients(), 1);
    l.storage.write(tab, set("stop", "1".into()));
    assert_eq!(child.process.wait(WAIT), Some(0));
    assert!(matches!(child.served.join().unwrap(), Error::Closed));
}

const SCRIPT: &str = "https://a.test/app.js";

/// Stores an entry, reads it back and removes it. 0 if all of it held.
fn code_cache_child(port: &RendererPort) -> i32 {
    let services = remote(port);
    let cc = &services.code_cache;
    let until = |want: bool| {
        let start = Instant::now();
        while cc.has(SCRIPT) != want {
            if start.elapsed() > WAIT {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    };
    if cc.has(SCRIPT) {
        return 30;
    }
    let data: Vec<u8> = (0..2 << 20).map(|i| (i % 253) as u8).collect();
    cc.store(SCRIPT, CacheKind::Module, [7; 32], data.clone());
    if !until(true) {
        return 31;
    }
    match cc.read_async(SCRIPT).recv_timeout(WAIT) {
        Ok(Some(e)) if e.kind == CacheKind::Module && e.response_hash == [7; 32] => {
            if e.data != data {
                return 32;
            }
        }
        _ => return 33,
    }
    // Reads past the cap find nothing rather than wait.
    let missing: Vec<_> = (0..code_cache::MAX_READS * 2)
        .map(|_| cc.read_async("https://a.test/none.js"))
        .collect();
    for read in missing {
        if !matches!(read.recv_timeout(WAIT), Ok(None)) {
            return 34;
        }
    }
    cc.remove(SCRIPT);
    if !until(false) {
        return 35;
    }
    port.flush(WAIT).unwrap();
    0
}

#[test]
fn code_cache_entries_cross_both_ways() {
    let l = local();
    let child = start("code-cache", &l.services);
    assert_eq!(child.process.wait(WAIT), Some(0));
    assert!(matches!(child.served.join().unwrap(), Error::Closed));
    assert!(l.code_cache.read(SCRIPT).is_none());
}

/// Reads the clipboard and writes it, asks for two of the browser's pages, and takes a document's
/// filters and its generic selectors. 0 if every answer was the browser's.
fn embedder_child(port: &RendererPort) -> i32 {
    use crate::driver::{Clipboard, InternalAnswer, InternalPages, InternalQuery};
    use snowlark_page::content_filter::{ProceduralStep, TextPattern};
    use snowlark_page::ContentFilter;
    let services = remote(port);
    if services.clipboard.text().as_deref() != Some("from the browser") {
        return 40;
    }
    services.clipboard.set_text(&big_text());
    let page = services.internal.request(InternalQuery::Page {
        url: "snowlark://big".into(),
    });
    if page
        != (InternalAnswer::Page {
            shown: "snowlark://big".into(),
            html: Some(big_text()),
        })
    {
        return 41;
    }
    let source = services.internal.request(InternalQuery::ViewSource {
        url: "view-source:https://a.test/".into(),
        headers: vec![],
        body: vec![7; 3 << 20],
        cut: false,
    });
    if source
        != (InternalAnswer::ViewSource {
            html: format!("{} bytes", 3 << 20),
        })
    {
        return 42;
    }
    let url = snowlark_net::Url::parse("https://a.test/").unwrap();
    let Some(filters) = services.content_filter.document_filters(&url, &url) else {
        return 43;
    };
    // The regular expression is compiled again here, flags and all.
    let regex_holds = matches!(
        filters.procedural.first().map(|f| &f.steps[..]),
        Some([ProceduralStep::Css(_), ProceduralStep::HasText(TextPattern::Regex(r))])
            if r.is_match("AD") && !r.is_match("ADD")
    );
    if !regex_holds || filters.hide != [".banner"] {
        return 44;
    }
    let classes = ["ad".to_owned(), "keep".to_owned()];
    let selectors =
        services
            .content_filter
            .generic_selectors(&filters, &classes, &["x".to_owned()]);
    if selectors != [".ad", "#x"] {
        return 45;
    }
    port.flush(WAIT).unwrap();
    0
}

#[test]
fn embedder_services_cross_both_ways() {
    use crate::driver::Clipboard;
    let l = local();
    l.clipboard.set_text("from the browser");
    let child = start("embedder", &l.services);
    assert_eq!(child.process.wait(WAIT), Some(0));
    assert!(matches!(child.served.join().unwrap(), Error::Closed));
    assert_eq!(l.clipboard.text(), Some(big_text()));
}

/// Every kind of network call, through the browser's network. `hang` is an address that accepts and
/// never answers: the child starts a fetch of it and drops the stream. 0 if every answer held.
fn net_child(port: &RendererPort, hang: &str) -> i32 {
    use snowlark_net::blob::BlobData;
    use snowlark_net::cookies::CookieContext;
    use snowlark_net::websocket::{WsEvent, WsParams};
    use snowlark_net::{
        BlockReason, Client, NetBackend, NetClient, NetErrorKind, Origin, Request, Url,
    };
    let services = remote(port);
    let net = &services.net;
    let client = NetClient::with_backend(net.clone());
    let fetch = |url: &Url| client.fetch(Request::new(url.clone())).read_all();

    // A sandboxed document's blob, past a frame: its opaque origin is one origin in the browser.
    let mine = Origin::new_opaque();
    let data: Vec<u8> = (0..3 << 20).map(|i| (i % 249) as u8).collect();
    let Ok(url) = net
        .register_blob(&mine, BlobData::new(data.clone().into(), "text/plain"))
        .parse::<Url>()
    else {
        return 50;
    };
    match fetch(&url) {
        Ok(r) if r.body.as_deref() == Some(&data[..]) => {}
        _ => return 51,
    }
    // Revoked from another opaque origin: not revoked. From its own: gone.
    net.revoke_blob(&url, &Origin::new_opaque());
    if fetch(&url).is_err() {
        return 52;
    }
    net.revoke_blob(&url, &mine);
    if fetch(&url).is_ok() {
        return 53;
    }

    // The tab's request filter applies in the browser.
    let blocked: Url = "http://127.0.0.1:65530/blocked".parse().unwrap();
    match fetch(&blocked) {
        Err(e) if e.kind == NetErrorKind::Blocked(BlockReason::ContentFilter) => {}
        _ => return 54,
    }

    // Cookies, as `document.cookie` sets and reads them.
    let context = CookieContext {
        url: "http://a.test/".parse().unwrap(),
        http_api: false,
        same_site: true,
        top_level_navigation: false,
        safe_method: true,
    };
    net.set_document_cookie("a=1", &context);
    if net.document_cookie(&context) != "a=1" {
        return 55;
    }
    if net.user_agent().value.is_empty() {
        return 56;
    }

    // A WebSocket connection to nothing fails, and says so through the pipe.
    let (tx, woken) = mpsc::channel();
    let page: Url = "http://a.test/".parse().unwrap();
    let mut socket = net.websocket(
        WsParams {
            url: "ws://127.0.0.1:65530/".parse().unwrap(),
            protocols: vec![],
            client: Arc::new(Client::for_url(&page)),
        },
        None,
        Arc::new(move || {
            let _ = tx.send(());
        }),
    );
    loop {
        if woken.recv_timeout(WAIT).is_err() {
            return 57;
        }
        match socket.try_next() {
            Some(WsEvent::Closed { failed: true, .. }) => break,
            Some(WsEvent::Closed { .. }) | Some(WsEvent::Open { .. }) => return 58,
            _ => {}
        }
    }
    if net.waiting() != (0, 0) {
        return 59;
    }

    // A fetch that never ends, dropped: the browser cancels it.
    let stream = client.fetch(Request::new(hang.parse().unwrap()));
    thread::sleep(Duration::from_secs(1));
    drop(stream);
    port.flush(WAIT).unwrap();
    0
}

/// Fetches from eight threads at once, as a page's thread and its media players do: each fetch's
/// id is taken on its own thread.
fn net_threads_child(port: &RendererPort) -> i32 {
    use snowlark_net::blob::BlobData;
    use snowlark_net::{NetBackend, NetClient, Origin, Request, Url};
    let services = remote(port);
    let net = services.net.clone();
    let origin = Origin::new_opaque();
    let Ok(url) = net
        .register_blob(&origin, BlobData::new(vec![7u8; 64].into(), "text/plain"))
        .parse::<Url>()
    else {
        return 60;
    };
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let (net, url) = (net.clone(), url.clone());
            thread::spawn(move || {
                let client = NetClient::with_backend(net);
                (0..200).all(|_| client.fetch(Request::new(url.clone())).read_all().is_ok())
            })
        })
        .collect();
    if !threads.into_iter().all(|t| t.join().unwrap_or(false)) {
        return 61;
    }
    port.flush(WAIT).unwrap();
    0
}

/// Fetches a renderer starts on several threads reach the browser in the order of their ids, which
/// the browser requires (an id only goes up): a renderer is never killed for its own threads' race.
#[test]
fn fetches_from_many_threads_of_a_renderer() {
    let l = local();
    let child = start("net-threads", &l.services);
    assert_eq!(child.process.wait(WAIT), Some(0));
}

#[test]
fn the_network_reaches_a_renderer() {
    use std::io::Read;
    let l = local();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let hang = format!("http://{}/hang", listener.local_addr().unwrap());
    let child = start_with("net", &l.services, &hang);
    listener.set_nonblocking(true).unwrap();
    let begin = Instant::now();
    let mut conn = loop {
        match listener.accept() {
            Ok((conn, _)) => break conn,
            Err(_) => {
                if let Some(code) = child.process.wait(Duration::ZERO) {
                    panic!("the child ended with {code} before its fetch to hang");
                }
                assert!(begin.elapsed() < WAIT, "the fetch to hang never came");
                thread::sleep(Duration::from_millis(5));
            }
        }
    };
    assert_eq!(child.process.wait(WAIT), Some(0));
    assert!(matches!(child.served.join().unwrap(), Error::Closed));
    // The child's drop reached the network: the connection closes, and nothing is left open.
    until("the hung fetch to go", || child.host.net.open() == (0, 0));
    conn.set_nonblocking(false).unwrap();
    conn.set_read_timeout(Some(WAIT)).unwrap();
    let mut rest = Vec::new();
    // (the request arrives, then the end of the stream, not a timeout)
    assert!(conn.read_to_end(&mut rest).is_ok());
    assert!(rest.starts_with(b"GET /hang"));
}

/// M5: a navigation's response, fetched by the browser (which takes its head), is handed to a
/// renderer to read the rest ([`net::NetHost::hand_over`], the renderer's `NetClient::adopt`):
/// the renderer gets the body, in pieces that mostly arrive after the hand-over, woken for each,
/// and nothing stays open after. The hand-over is made before the renderer is served, so its
/// `Adopt` finds the stream whatever the timing. Without the host's own wake-up on the handed
/// stream, the renderer got only what had arrived when the pump last looked (YouTube's home page
/// never finished parsing, 2026-11-11).
#[test]
fn a_navigation_response_is_handed_to_a_renderer_to_read() {
    use std::io::{BufRead, Write};
    const SIZE: usize = 300 * 1024;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url: snowlark_net::Url = format!("http://{}/page", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            thread::spawn(move || {
                let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let body: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {SIZE}\r\nConnection: close\r\n\r\n"
                );
                let _ = conn.write_all(head.as_bytes());
                // (the body comes in pieces over half a second: most of it after the browser
                // took the head and handed the rest over, as a large page's does)
                for piece in body.chunks(SIZE / 6) {
                    let _ = conn.write_all(piece);
                    let _ = conn.flush();
                    thread::sleep(Duration::from_millis(90));
                }
            });
        }
    });
    let l = local();
    // The browser's fetch, to its head.
    let client = snowlark_net::NetClient::with_backend(l.services.net.clone());
    let mut stream = client.fetch(snowlark_net::Request::navigation(url));
    let head = loop {
        match stream.next_blocking() {
            Some(snowlark_net::FetchEvent::Head(h)) => break h,
            Some(snowlark_net::FetchEvent::Violation(_)) => {}
            other => panic!("no head: {other:?}"),
        }
    };
    assert_eq!(head.status, 200);
    // The renderer, handed the rest before it is served.
    let (port, process) = Launch::new(std::env::current_exe().unwrap())
        .arg("services::tests::child_main")
        .arg("--exact")
        .arg("--nocapture")
        .env(ROLE, "adopt")
        .env(ARG, SIZE.to_string())
        .spawn()
        .unwrap()
        .connect::<Down, Up>(policy(), 64 << 20, WAIT)
        .unwrap();
    let host = Arc::new(ServiceHost::new(port, &l.services));
    assert_eq!(host.net().hand_over(stream), net::HANDED_FROM);
    let served = {
        let host = host.clone();
        thread::spawn(move || host.run())
    };
    assert_eq!(
        process.wait(WAIT),
        Some(0),
        "the renderer's read of the body"
    );
    assert!(matches!(served.join().unwrap(), Error::Closed));
    until("nothing to stay open", || host.net().open() == (0, 0));
}

/// The "adopt" role of [`a_navigation_response_is_handed_to_a_renderer_to_read`].
fn adopt_child(port: &RendererPort) -> i32 {
    let n: usize = std::env::var(ARG).unwrap().parse().unwrap();
    let services = remote(port);
    let client = snowlark_net::NetClient::with_backend(services.net.clone());
    let (tx, woken) = mpsc::channel();
    let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = tx.send(());
    });
    let Some(mut stream) = client.adopt(net::HANDED_FROM, Some(notify)) else {
        return 60;
    };
    // (an id below the browser's range is never a handed stream)
    if client.adopt(3, None).is_some() {
        return 63;
    }
    let mut body = Vec::new();
    let mut wakes = 0;
    loop {
        match stream.try_next() {
            Some(snowlark_net::FetchEvent::Body(b)) => body.extend_from_slice(&b),
            Some(snowlark_net::FetchEvent::Done) => break,
            Some(snowlark_net::FetchEvent::Head(_)) => return 64,
            Some(snowlark_net::FetchEvent::Error(_)) => return 62,
            Some(_) => {}
            None => {
                if woken.recv_timeout(WAIT).is_err() {
                    return 65;
                }
                wakes += 1;
            }
        }
    }
    let expected: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
    if body != expected {
        return 61;
    }
    if wakes == 0 {
        return 66;
    }
    0
}

/// A server whose `/a…` answers a second after the request with a redirect to `/b…`; returns its
/// origin and the paths `/b` was asked for.
fn redirecting_server() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(std::sync::Mutex::new(Vec::new()));
    let h = hits.clone();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let hits = h.clone();
            thread::spawn(move || {
                let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = first.split(' ').nth(1).unwrap_or("").to_string();
                let answer = if let Some(rest) = path.strip_prefix("/a") {
                    thread::sleep(Duration::from_secs(1));
                    format!("HTTP/1.1 302 Found\r\nLocation: /b{rest}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                } else {
                    hits.lock().unwrap().push(path);
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()
                };
                let _ = conn.write_all(answer.as_bytes());
            });
        }
    });
    (base, hits)
}

/// A renderer that ends with fetches in flight: the browser cancels them (nothing stays open), but
/// a keepalive one goes on to its response, through a redirect (Fetch's keepalive flag; Chromium's
/// `KeepAliveURLLoader::OnURLLoaderDisconnected`).
#[test]
fn a_keepalive_fetch_outlives_its_renderer() {
    let l = local();
    let (base, hits) = redirecting_server();
    let child = start_with("net-keepalive", &l.services, &base);
    assert_eq!(child.process.wait(WAIT), Some(0));
    until("the renderer's fetches to be let go", || {
        child.host.net.open() == (0, 0)
    });
    until("the keepalive fetch to reach the redirect's target", || {
        !hits.lock().unwrap().is_empty()
    });
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(*hits.lock().unwrap(), ["/b?keepalive"]);
}

/// The quota the browser's IndexedDB backend reports in these tests.
const IDB_QUOTA: u64 = 123_456_789;
const IDB_KEY: &str = "https://a.test";
/// How many records a test database holds.
const RECORDS: u32 = 4;

/// An IndexedDB event loop: its id in a backend, and the messages the backend posts to it.
pub(crate) struct IdbLoop {
    pub(crate) id: snowlark_idb::LoopId,
    inbox: Arc<snowlark_idb::Inbox>,
    woken: mpsc::Receiver<()>,
    queued: std::cell::RefCell<std::collections::VecDeque<Msg>>,
    /// The last request's number.
    seq: std::cell::Cell<u64>,
}

impl IdbLoop {
    pub(crate) fn new(idb: &dyn IdbService) -> IdbLoop {
        let (tx, woken) = mpsc::channel();
        let inbox = Arc::new(snowlark_idb::Inbox::new(Box::new(move || {
            let _ = tx.send(());
        })));
        IdbLoop {
            id: idb.register_loop(&inbox),
            inbox,
            woken,
            queued: Default::default(),
            seq: Default::default(),
        }
    }

    /// The next message the backend posted, or `None` after [`WAIT`].
    pub(crate) fn next(&self) -> Option<Msg> {
        let start = Instant::now();
        loop {
            {
                let mut queued = self.queued.borrow_mut();
                queued.extend(self.inbox.drain());
                if let Some(m) = queued.pop_front() {
                    return Some(m);
                }
            }
            let left = WAIT.checked_sub(start.elapsed())?;
            let _ = self.woken.recv_timeout(left);
        }
    }
    /// The answer to request `seq`, or `None` after `wait`; the other messages stay queued.
    pub(crate) fn answer(&self, seq: u64, wait: Duration) -> Option<snowlark_idb::Answer> {
        let start = Instant::now();
        loop {
            {
                let mut queued = self.queued.borrow_mut();
                queued.extend(self.inbox.drain());
                let at = queued
                    .iter()
                    .position(|m| matches!(m, Msg::Answer { seq: s, .. } if *s == seq));
                if let Some(Msg::Answer { answer, .. }) = at.and_then(|at| queued.remove(at)) {
                    return Some(answer);
                }
            }
            let left = wait.checked_sub(start.elapsed())?;
            let _ = self.woken.recv_timeout(left);
        }
    }
}

/// A backend whose requests are each waited for, as phase 1's calls were.
pub(crate) struct Asking<'a> {
    pub(crate) idb: &'a dyn IdbService,
    pub(crate) l: &'a IdbLoop,
}

impl<'a> std::ops::Deref for Asking<'a> {
    type Target = dyn IdbService + 'a;

    fn deref(&self) -> &Self::Target {
        self.idb
    }
}

impl Asking<'_> {
    /// Sends a request and waits up to `wait` for its answer.
    pub(crate) fn ask_within(&self, txn: u64, request: Request, wait: Duration) -> Option<Answer> {
        let seq = self.l.seq.get() + 1;
        self.l.seq.set(seq);
        self.idb.request(self.l.id, txn, seq, request);
        self.l.answer(seq, wait)
    }

    fn ask(&self, txn: u64, request: Request) -> Option<Answer> {
        self.ask_within(txn, request, WAIT)
    }

    pub(crate) fn apply(&self, txn: u64, change: Change, add: bool) -> Result<(), WriteError> {
        match self.ask(txn, Request::Apply { change, add }) {
            Some(Answer::Applied(r)) => r,
            _ => Err(WriteError::Refused),
        }
    }

    pub(crate) fn first(
        &self,
        txn: u64,
        source: Source,
        range: &KeyRange,
    ) -> Result<Option<Found>, Refused> {
        let range = range.clone();
        match self.ask(txn, Request::First { source, range }) {
            Some(Answer::Found(r)) => r.map(|f| f.map(Found::from)),
            _ => Err(Refused),
        }
    }

    pub(crate) fn get_all(
        &self,
        txn: u64,
        source: Source,
        range: &KeyRange,
        direction: Direction,
        count: u32,
    ) -> Result<Vec<Found>, Refused> {
        let range = range.clone();
        let request = Request::GetAll {
            source,
            range,
            direction,
            count,
        };
        match self.ask(txn, request) {
            Some(Answer::All(r)) => r.map(|v| v.into_iter().map(Found::from).collect()),
            _ => Err(Refused),
        }
    }

    pub(crate) fn count(&self, txn: u64, source: Source, range: &KeyRange) -> Result<u64, Refused> {
        let range = range.clone();
        match self.ask(txn, Request::Count { source, range }) {
            Some(Answer::Count(r)) => r,
            _ => Err(Refused),
        }
    }

    pub(crate) fn cursor_advance(
        &self,
        txn: u64,
        source: Source,
        direction: Direction,
        range: &KeyRange,
        seek: Seek,
        count: u32,
    ) -> Result<Option<Found>, Refused> {
        let request = Request::CursorAdvance {
            source,
            direction,
            range: range.clone(),
            seek,
            count,
        };
        match self.ask(txn, request) {
            Some(Answer::Found(r)) => r.map(|f| f.map(Found::from)),
            _ => Err(Refused),
        }
    }
}

use snowlark_idb::{
    Answer, Change, Direction, Durability, Found, IdbService, Key, KeyPath, KeyRange, Mode, Msg,
    Record, Refused, Request, Seek, Source, WriteError,
};

/// Record `i`'s value: past what a message holds inline.
fn big(i: u32) -> Vec<u8> {
    (0..(100 << 10) + i)
        .map(|j| ((j * 7 + i) % 251) as u8)
        .collect()
}

/// Record `i` of the store, with its key in the index: `even` or `odd`.
fn idb_put(store: u64, index: u64, i: u32) -> Change {
    let parity = if i.is_multiple_of(2) { "even" } else { "odd" };
    Change::Put {
        store,
        key: Key::Number(i as f64),
        record: Record {
            value: big(i).into(),
            index_keys: vec![(index, vec![Key::string(parity)])],
        },
    }
}

/// Opens `name` at version 1 and makes its store `s` with the index `by`, as a page's upgrade does:
/// (connection, the upgrade transaction, still running, store, index).
fn idb_create(idb: &dyn IdbService, l: &IdbLoop, name: &str) -> Option<(u64, u64, u64, u64)> {
    idb.open(l.id, 1, IDB_KEY, name, Some(1));
    let Some(Msg::Upgrade {
        conn,
        txn,
        mut schema,
        old: 0,
        new: 1,
        ..
    }) = l.next()
    else {
        return None;
    };
    let (store, index) = (schema.take_id(), schema.take_id());
    let changes = [
        Change::Version(1),
        Change::CreateStore {
            id: store,
            name: "s".into(),
            key_path: None,
            auto_increment: false,
        },
        Change::CreateIndex {
            store,
            id: index,
            name: "by".into(),
            key_path: KeyPath::String("x".into()),
            unique: false,
            multi_entry: false,
        },
        Change::PopulateIndex {
            store,
            index,
            keys: vec![],
        },
    ];
    for change in changes {
        idb.change(txn, change);
    }
    Some((conn, txn, store, index))
}

/// Commits an upgrade transaction and finishes it: whether it committed.
fn idb_commit_upgrade(idb: &dyn IdbService, l: &IdbLoop, txn: u64) -> bool {
    idb.commit(txn, Durability::Default);
    let committed = matches!(l.next(), Some(Msg::Committed { txn: t, result: Ok(()) }) if t == txn);
    idb.upgrade_finished(txn, true);
    committed
}

/// Opens the existing `name`: (connection, the id of its store `s`).
fn idb_open(idb: &dyn IdbService, l: &IdbLoop, name: &str) -> Option<(u64, u64)> {
    idb.open(l.id, 2, IDB_KEY, name, None);
    let Some(Msg::Opened { conn, schema, .. }) = l.next() else {
        return None;
    };
    Some((conn, schema.store_named("s")?.id))
}

/// A transaction over `store`, once it has started.
fn idb_begin(idb: &dyn IdbService, l: &IdbLoop, conn: u64, mode: Mode, store: u64) -> Option<u64> {
    let t = idb.create_transaction(conn, mode, vec![store]);
    matches!(l.next(), Some(Msg::Started { txn }) if txn == t).then_some(t)
}

/// Makes a database, writes and reads it in its upgrade, aborts a transaction, and reads again.
/// 0 if every answer held.
fn idb_child(port: &RendererPort) -> i32 {
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let l = IdbLoop::new(idb);
    let idb = &Asking { idb, l: &l };
    let Some((conn, txn, store, index)) = idb_create(idb.idb, &l, "db") else {
        return 60;
    };
    for i in 0..RECORDS {
        if idb.apply(txn, idb_put(store, index, i), false).is_err() {
            return 61;
        }
    }
    if idb.apply(txn, idb_put(store, index, 0), true) != Err(WriteError::KeyExists) {
        return 62;
    }
    let all = KeyRange::unbounded();
    match idb.first(txn, Source::Store(store), &KeyRange::only(Key::Number(2.0))) {
        Ok(Some((k, _, v))) if k == Key::Number(2.0) && v[..] == big(2)[..] => {}
        _ => return 63,
    }
    match idb.get_all(txn, Source::Store(store), &all, Direction::Prev, 0) {
        Ok(found)
            if found.len() == RECORDS as usize
                && found.iter().rev().enumerate().all(|(i, (k, _, v))| {
                    *k == Key::Number(i as f64) && v[..] == big(i as u32)[..]
                }) => {}
        _ => return 64,
    }
    let by = Source::Index(store, index);
    if idb.count(txn, by, &KeyRange::only(Key::string("odd"))) != Ok(RECORDS as u64 / 2) {
        return 65;
    }
    // Two steps of a cursor over the index: the `even` records, 0 then 2.
    match idb.cursor_advance(txn, by, Direction::Next, &all, Seek::default(), 2) {
        Ok(Some((k, pk, v)))
            if k == Key::string("even") && pk == Key::Number(2.0) && v[..] == big(2)[..] => {}
        _ => return 66,
    }
    if !idb_commit_upgrade(idb.idb, &l, txn) {
        return 67;
    }

    // An aborted transaction leaves nothing behind.
    let Some(t) = idb_begin(idb.idb, &l, conn, Mode::ReadWrite, store) else {
        return 68;
    };
    if idb.apply(t, Change::Clear(store), false).is_err()
        || idb.count(t, Source::Store(store), &all) != Ok(0)
    {
        return 69;
    }
    idb.revert(t);
    idb.transaction_finished(t);
    // (finished, it is refused or not answered, not a lie)
    let late = Request::Count {
        source: Source::Store(store),
        range: all.clone(),
    };
    match idb.ask_within(t, late, Duration::from_millis(300)) {
        None | Some(Answer::Count(Err(Refused))) => {}
        Some(_) => return 70,
    }
    let Some(r) = idb_begin(idb.idb, &l, conn, Mode::ReadOnly, store) else {
        return 71;
    };
    if idb.count(r, Source::Store(store), &all) != Ok(RECORDS as u64) {
        return 72;
    }
    // A read-only transaction does not write (the backend's own check).
    if idb.apply(r, idb_put(store, index, 9), false) != Err(WriteError::Refused) {
        return 73;
    }
    idb.transaction_finished(r);
    idb.connection_closed(conn);
    idb.flush();
    port.flush(WAIT).unwrap();
    0
}

#[test]
fn indexeddb_reaches_a_renderer_and_back() {
    let l = local();
    let child = start("idb", &l.services);
    assert_eq!(child.process.wait(WAIT), Some(0));
    assert!(matches!(child.served.join().unwrap(), Error::Closed));
    // Nothing of its is left open but its loop.
    assert_eq!(child.host.idb.open(), (1, 0, 0));
    // What it committed is the browser's.
    let idb = &l.idb;
    let me = IdbLoop::new(idb);
    let (conn, store) = idb_open(idb, &me, "db").expect("the renderer's database");
    let t = idb_begin(idb, &me, conn, Mode::ReadOnly, store).expect("a transaction");
    let all = idb.get_all(
        t,
        Source::Store(store),
        &KeyRange::unbounded(),
        Direction::Next,
        0,
    );
    let all = all.unwrap();
    assert_eq!(all.len(), RECORDS as usize);
    for (i, (k, _, v)) in all.iter().enumerate() {
        assert_eq!(*k, Key::Number(i as f64));
        assert!(v[..] == big(i as u32)[..], "record {i}");
    }
}

/// Clears the store of `db` and writes a record in a transaction, then ends without finishing it.
fn idb_dies_child(port: &RendererPort) -> i32 {
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let l = IdbLoop::new(idb);
    let idb = &Asking { idb, l: &l };
    let Some((conn, store)) = idb_open(idb.idb, &l, "db") else {
        return 80;
    };
    let Some(t) = idb_begin(idb.idb, &l, conn, Mode::ReadWrite, store) else {
        return 81;
    };
    if idb.apply(t, Change::Clear(store), false).is_err() {
        return 82;
    }
    let put = Change::Put {
        store,
        key: Key::Number(7.0),
        record: Record {
            value: big(7).into(),
            index_keys: vec![],
        },
    };
    if idb.apply(t, put, false).is_err() {
        return 83;
    }
    if idb.count(t, Source::Store(store), &KeyRange::unbounded()) != Ok(1) {
        return 84;
    }
    port.flush(WAIT).unwrap();
    0
}

#[test]
fn a_renderer_that_dies_mid_transaction_leaves_the_database_as_it_was() {
    let l = local();
    let idb = &l.idb;
    let me = IdbLoop::new(idb);
    let (conn, txn, store, index) = idb_create(idb, &me, "db").unwrap();
    for i in 0..RECORDS {
        idb.apply(txn, idb_put(store, index, i), false).unwrap();
    }
    assert!(idb_commit_upgrade(idb, &me, txn));

    let Child {
        host,
        process,
        served,
    } = start("idb-dies", &l.services);
    assert_eq!(process.wait(WAIT), Some(0));
    assert!(matches!(served.join().unwrap(), Error::Closed));
    // Until the browser lets the renderer go, its transaction is open, and holds ours back.
    assert_eq!(host.idb.open(), (1, 1, 1));
    let t = idb.create_transaction(conn, Mode::ReadOnly, vec![store]);
    assert!(!me.inbox.has_messages());
    drop(Arc::into_inner(host).expect("only the test holds the host"));
    assert!(matches!(me.next(), Some(Msg::Started { txn }) if txn == t));
    // Its writes are gone, and what it cleared is back.
    let all = KeyRange::unbounded();
    assert_eq!(idb.count(t, Source::Store(store), &all), Ok(RECORDS as u64));
    let seven = KeyRange::only(Key::Number(7.0));
    assert_eq!(idb.first(t, Source::Store(store), &seven), Ok(None));
    let by = Source::Index(store, index);
    assert_eq!(
        idb.count(t, by, &KeyRange::only(Key::string("even"))),
        Ok(2)
    );
}

/// Makes `vc` at version 1, then waits for another renderer's upgrade: it closes its connection, as
/// a page that closes in its `versionchange` handler does, then says it fired the event. 10 if so.
fn idb_versionchange_child(port: &RendererPort) -> i32 {
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let l = IdbLoop::new(idb);
    let idb = &Asking { idb, l: &l };
    let Some((conn, txn, _, _)) = idb_create(idb.idb, &l, "vc") else {
        return 90;
    };
    if !idb_commit_upgrade(idb.idb, &l, txn) {
        return 91;
    }
    match l.next() {
        Some(Msg::VersionChange {
            conn: c,
            old: 1,
            new: Some(2),
        }) if c == conn => {}
        _ => return 92,
    }
    idb.connection_closed(conn);
    // (a connection of its that closed: it names nothing, and is no lie)
    idb.versionchange_fired(conn);
    port.flush(WAIT).unwrap();
    10
}

/// Opens `vc` at version 2, once the other renderer let go of it. 20 if its upgrade ran.
fn idb_upgrading_child(port: &RendererPort) -> i32 {
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let l = IdbLoop::new(idb);
    let idb = &Asking { idb, l: &l };
    idb.open(l.id, 1, IDB_KEY, "vc", Some(2));
    let Some(Msg::Upgrade {
        conn,
        txn,
        schema,
        old: 1,
        new: 2,
        ..
    }) = l.next()
    else {
        return 95;
    };
    if schema.store_named("s").is_none() {
        return 96;
    }
    if idb.apply(txn, Change::Version(2), false).is_err() || !idb_commit_upgrade(idb.idb, &l, txn) {
        return 97;
    }
    idb.connection_closed(conn);
    port.flush(WAIT).unwrap();
    20
}

#[test]
fn versionchange_reaches_one_renderer_from_another() {
    let l = local();
    let a = start("idb-vc-a", &l.services);
    until("the first renderer's connection", || {
        a.host.idb.open() == (1, 1, 0)
    });
    let b = start("idb-vc-b", &l.services);
    assert_eq!(a.process.wait(WAIT), Some(10));
    assert_eq!(b.process.wait(WAIT), Some(20));
    let idb = &l.idb;
    let me = IdbLoop::new(idb);
    idb.open(me.id, 1, IDB_KEY, "vc", None);
    assert!(matches!(me.next(), Some(Msg::Opened { version: 2, .. })));
}

/// Makes `del` and holds it open until another renderer's delete asks it to let go (a
/// `versionchange` to no version), then lets go a while after, so the delete is blocked first. 30
/// if all came as it should.
fn idb_holding_child(port: &RendererPort) -> i32 {
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let l = IdbLoop::new(idb);
    let Some((conn, txn, _, _)) = idb_create(idb, &l, "del") else {
        return 100;
    };
    if !idb_commit_upgrade(idb, &l, txn) {
        return 101;
    }
    match l.next() {
        Some(Msg::VersionChange {
            conn: c,
            old: 1,
            new: None,
        }) if c == conn => {}
        _ => return 102,
    }
    idb.versionchange_fired(conn);
    std::thread::sleep(Duration::from_millis(300));
    idb.connection_closed(conn);
    port.flush(WAIT).unwrap();
    30
}

/// The names and versions `databases()` answers.
fn idb_list(idb: &dyn IdbService, l: &IdbLoop, id: u64) -> Option<Vec<(String, u64)>> {
    idb.databases(l.id, id, IDB_KEY);
    match l.next() {
        Some(Msg::Databases {
            id: i,
            result: Ok(list),
        }) if i == id => Some(list),
        _ => None,
    }
}

/// Lists the databases, deletes `del` while the other renderer holds it (blocked, then done once it
/// lets go), and lists them again. 40 if all came as it should.
fn idb_deleting_child(port: &RendererPort) -> i32 {
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let l = IdbLoop::new(idb);
    if idb_list(idb, &l, 1) != Some(vec![("del".to_owned(), 1)]) {
        return 110;
    }
    idb.delete_database(l.id, 2, IDB_KEY, "del");
    match l.next() {
        Some(Msg::Blocked {
            req: 2,
            old: 1,
            new: None,
        }) => {}
        _ => return 111,
    }
    match l.next() {
        Some(Msg::Deleted {
            req: 2,
            result: Ok(1),
        }) => {}
        _ => return 112,
    }
    if idb_list(idb, &l, 3) != Some(vec![]) {
        return 113;
    }
    // `estimate()` across the pipe: the browser's backend answers with its quota.
    idb.estimate(l.id, 4, IDB_KEY);
    match l.next() {
        Some(Msg::Estimate {
            id: 4,
            usage: 0,
            quota: IDB_QUOTA,
        }) => {}
        _ => return 114,
    }
    port.flush(WAIT).unwrap();
    40
}

#[test]
fn a_delete_from_one_renderer_is_blocked_by_another_then_lists_nothing() {
    let l = local();
    let a = start("idb-del-a", &l.services);
    until("the first renderer's connection", || {
        a.host.idb.open() == (1, 1, 0)
    });
    let b = start("idb-del-b", &l.services);
    assert_eq!(a.process.wait(WAIT), Some(30));
    assert_eq!(b.process.wait(WAIT), Some(40));
}

/// Sends a transaction's writes and a count one after another without waiting, then takes the
/// answers: each comes once, in the order sent, and the count sees every write. 50 if so.
fn idb_pipelined_child(port: &RendererPort) -> i32 {
    const WRITES: u64 = 200;
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let l = IdbLoop::new(idb);
    let Some((_, txn, store, index)) = idb_create(idb, &l, "pipe") else {
        return 120;
    };
    for seq in 1..=WRITES {
        let change = idb_put(store, index, seq as u32);
        idb.request(l.id, txn, seq, Request::Apply { change, add: true });
    }
    let count = Request::Count {
        source: Source::Store(store),
        range: KeyRange::unbounded(),
    };
    idb.request(l.id, txn, WRITES + 1, count);
    let mut seqs = Vec::new();
    while seqs.len() as u64 <= WRITES {
        match l.next() {
            Some(Msg::Answer {
                txn: t,
                seq,
                answer,
            }) if t == txn => {
                let ok = match answer {
                    Answer::Applied(r) => r.is_ok() && seq <= WRITES,
                    Answer::Count(r) => r == Ok(WRITES) && seq == WRITES + 1,
                    _ => false,
                };
                if !ok {
                    return 121;
                }
                seqs.push(seq);
            }
            _ => return 122,
        }
    }
    if seqs != (1..=WRITES + 1).collect::<Vec<_>>() {
        return 123;
    }
    port.flush(WAIT).unwrap();
    50
}

#[test]
fn a_renderers_requests_are_answered_in_order_without_waiting() {
    let l = local();
    let child = start("idb-pipelined", &l.services);
    assert_eq!(child.process.wait(WAIT), Some(50));
}

/// Makes two transactions once its database is made, and exits 50 as soon as `transaction()` has
/// returned for both: the browser's backend makes neither until the test lets it.
fn idb_unanswered_child(port: &RendererPort) -> i32 {
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let l = IdbLoop::new(idb);
    let Some((conn, txn, store, _)) = idb_create(idb, &l, "gated") else {
        return 130;
    };
    if !idb_commit_upgrade(idb, &l, txn) {
        return 131;
    }
    let a = idb.create_transaction(conn, Mode::ReadOnly, vec![store]);
    let b = idb.create_transaction(conn, Mode::ReadWrite, vec![store]);
    if a == 0 || b <= a {
        return 132;
    }
    50
}

/// Makes a store with a key generator, in-line keys at `id` and an index on `id`, sends adds with no
/// key without waiting, and checks the backend's keys come back in order and the index has them.
fn idb_generated_child(port: &RendererPort) -> i32 {
    const ADDS: u64 = 50;
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let l = IdbLoop::new(idb);
    idb.open(l.id, 1, IDB_KEY, "generated", Some(1));
    let Some(Msg::Upgrade {
        txn, mut schema, ..
    }) = l.next()
    else {
        return 140;
    };
    let (store, index) = (schema.take_id(), schema.take_id());
    let changes = [
        Change::Version(1),
        Change::CreateStore {
            id: store,
            name: "s".into(),
            key_path: Some(KeyPath::String("id".into())),
            auto_increment: true,
        },
        Change::CreateIndex {
            store,
            id: index,
            name: "by_id".into(),
            key_path: KeyPath::String("id".into()),
            unique: true,
            multi_entry: false,
        },
        Change::PopulateIndex {
            store,
            index,
            keys: vec![],
        },
    ];
    for change in changes {
        idb.change(txn, change);
    }
    for seq in 1..=ADDS {
        let request = Request::Generate {
            store,
            record: Record {
                value: big(seq as u32).into(),
                index_keys: vec![],
            },
            fill: vec![(index, snowlark_idb::Fill::Key)],
            add: true,
        };
        idb.request(l.id, txn, seq, request);
    }
    let found = Request::First {
        source: Source::Index(store, index),
        range: KeyRange::only(Key::Number(25.0)),
    };
    idb.request(l.id, txn, ADDS + 1, found);
    for seq in 1..=ADDS {
        match l.next() {
            Some(Msg::Answer {
                seq: s,
                answer: Answer::Generated(Ok(k)),
                ..
            }) if s == seq && k == seq => {}
            _ => return 141,
        }
    }
    match l.next() {
        Some(Msg::Answer {
            answer: Answer::Found(Ok(Some(f))),
            ..
        }) if f.1 == Key::Number(25.0) => {}
        _ => return 142,
    }
    50
}

#[test]
fn a_renderers_adds_to_a_key_generator_are_keyed_by_the_backend_in_order() {
    let l = local();
    let child = start("idb-generated", &l.services);
    assert_eq!(child.process.wait(WAIT), Some(50));
}

/// Makes a database, writes [`RECORDS`] records in its upgrade and commits it: 50 once committed.
fn idb_disk_child(port: &RendererPort) -> i32 {
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let l = IdbLoop::new(idb);
    let Some((_, txn, store, index)) = idb_create(idb, &l, "disk") else {
        return 150;
    };
    for i in 0..RECORDS {
        let change = idb_put(store, index, i);
        idb.request(
            l.id,
            txn,
            i as u64 + 1,
            Request::Apply { change, add: true },
        );
    }
    for _ in 0..RECORDS {
        if !matches!(
            l.next(),
            Some(Msg::Answer {
                answer: Answer::Applied(Ok(())),
                ..
            })
        ) {
            return 151;
        }
    }
    if !idb_commit_upgrade(idb, &l, txn) {
        return 152;
    }
    50
}

#[test]
fn a_renderers_writes_to_a_disk_backend_outlive_the_backend() {
    let dir = std::env::temp_dir().join(format!("snowlark-idb-disk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut l = local();
        let disk = snowlark_idb::LocalIdb::persistent(dir.clone());
        l.services.idb = Arc::new(disk.clone());
        let child = start("idb-disk", &l.services);
        assert_eq!(child.process.wait(WAIT), Some(50));
        disk.flush();
    }
    // A new backend over the same folder (the browser restarted) has the records.
    let again = snowlark_idb::LocalIdb::persistent(dir.clone());
    let me = IdbLoop::new(&again);
    let (conn, store) = idb_open(&again, &me, "disk").expect("the database");
    let t = idb_begin(&again, &me, conn, Mode::ReadOnly, store).expect("a transaction");
    let asking = Asking {
        idb: &again,
        l: &me,
    };
    let all = KeyRange::unbounded();
    assert_eq!(
        asking.count(t, Source::Store(store), &all),
        Ok(RECORDS as u64)
    );
    drop(me);
    drop(again);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The browser's backend, whose `create_transaction` waits until the test opens the gate.
struct Gated {
    idb: snowlark_idb::LocalIdb,
    open: std::sync::Mutex<bool>,
    opened: std::sync::Condvar,
}

impl Gated {
    fn open_gate(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}

impl IdbService for Gated {
    fn register_loop(&self, inbox: &Arc<snowlark_idb::Inbox>) -> snowlark_idb::LoopId {
        self.idb.register_loop(inbox)
    }
    fn loop_gone(&self, loop_: snowlark_idb::LoopId) {
        self.idb.loop_gone(loop_)
    }
    fn open(&self, loop_: snowlark_idb::LoopId, req: u64, key: &str, name: &str, v: Option<u64>) {
        self.idb.open(loop_, req, key, name, v)
    }
    fn delete_database(&self, loop_: snowlark_idb::LoopId, req: u64, key: &str, name: &str) {
        self.idb.delete_database(loop_, req, key, name)
    }
    fn databases(&self, loop_: snowlark_idb::LoopId, id: u64, key: &str) {
        self.idb.databases(loop_, id, key)
    }
    fn estimate(&self, loop_: snowlark_idb::LoopId, id: u64, key: &str) {
        self.idb.estimate(loop_, id, key)
    }
    fn versionchange_fired(&self, conn: u64) {
        self.idb.versionchange_fired(conn)
    }
    fn connection_closed(&self, conn: u64) {
        self.idb.connection_closed(conn)
    }
    fn upgrade_finished(&self, txn: u64, committed: bool) {
        self.idb.upgrade_finished(txn, committed)
    }
    fn create_transaction(&self, conn: u64, mode: Mode, scope: Vec<u64>) -> u64 {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
        drop(open);
        self.idb.create_transaction(conn, mode, scope)
    }
    fn commit(&self, txn: u64, durability: Durability) {
        IdbService::commit(&self.idb, txn, durability)
    }
    fn transaction_finished(&self, txn: u64) {
        self.idb.transaction_finished(txn)
    }
    fn change(&self, txn: u64, change: Change) {
        IdbService::change(&self.idb, txn, change)
    }
    fn revert(&self, txn: u64) {
        self.idb.revert(txn)
    }
    fn request(&self, loop_: snowlark_idb::LoopId, txn: u64, seq: u64, request: Request) {
        IdbService::request(&self.idb, loop_, txn, seq, request)
    }
    fn flush(&self) {
        self.idb.flush()
    }
}

#[test]
fn a_renderers_transaction_waits_for_no_answer() {
    let mut l = local();
    let gated = Arc::new(Gated {
        idb: l.idb.clone(),
        open: Default::default(),
        opened: Default::default(),
    });
    l.services.idb = gated.clone();
    let child = start("idb-unanswered", &l.services);
    let exited = child.process.wait(Duration::from_secs(5));
    gated.open_gate();
    assert_eq!(exited, Some(50));
}

/// Breaks one rule of the browser's IndexedDB end, and waits to be killed for it. [`ARG`] is another
/// renderer's live transaction and the store it writes: `txn,store`.
fn idb_forge(port: &RendererPort, role: &str) -> i32 {
    let services = remote(port);
    let idb: &dyn IdbService = &*services.idb;
    let arg = std::env::var(ARG).unwrap();
    let (theirs, their_store) = arg.split_once(',').unwrap();
    let (theirs, their_store): (u64, u64) = (theirs.parse().unwrap(), their_store.parse().unwrap());
    if role == "forge-idb-loop" {
        idb.open(3, 1, IDB_KEY, "x", None);
        return linger();
    }
    let l = IdbLoop::new(idb);
    if role == "forge-idb-conn" {
        idb.create_transaction(5, Mode::ReadOnly, vec![]);
        return linger();
    }
    let Some((conn, txn, store, _)) = idb_create(idb, &l, "mine") else {
        return 1;
    };
    match role {
        "forge-idb-txn" => {
            // (an id past every one it was given, so a lie, not a finished one of its own)
            if theirs <= txn {
                return 2;
            }
            let change = Change::Clear(their_store);
            idb.request(l.id, theirs, 1, Request::Apply { change, add: false });
        }
        "forge-idb-key" => {
            let put = Change::Put {
                store,
                key: Key::Number(f64::NAN),
                record: Record {
                    value: vec![1].into(),
                    index_keys: vec![],
                },
            };
            idb.request(
                l.id,
                txn,
                1,
                Request::Apply {
                    change: put,
                    add: false,
                },
            );
        }
        "forge-idb-mode" => {
            idb.create_transaction(conn, Mode::VersionChange, vec![store]);
        }
        "forge-idb-txn-id" => {
            // (the id it named its upgrade transaction, named again)
            let call = idb::Call::CreateTransaction {
                conn,
                mode: Mode::ReadOnly,
                scope: vec![store],
                txn,
            };
            port.send(&Up::Idb(idb::Up(pack(port, &call).unwrap())))
                .unwrap();
        }
        other => panic!("role {other}"),
    }
    linger()
}

#[test]
fn an_indexeddb_renderer_that_breaks_a_rule_dies() {
    for role in [
        "forge-idb-txn",
        "forge-idb-conn",
        "forge-idb-key",
        "forge-idb-mode",
        "forge-idb-loop",
        "forge-idb-txn-id",
    ] {
        let l = local();
        let idb = &l.idb;
        let me = IdbLoop::new(idb);
        let (_, theirs, store, index) = idb_create(idb, &me, "theirs").unwrap();
        for i in 0..2 {
            idb.apply(theirs, idb_put(store, index, i), false).unwrap();
        }
        let child = start_with(role, &l.services, &format!("{theirs},{store}"));
        assert_eq!(child.process.wait(WAIT), Some(9), "{role}");
        assert!(
            matches!(child.served.join().unwrap(), Error::BadMessage(_)),
            "{role}"
        );
        // The other renderer's transaction is as it was.
        let all = KeyRange::unbounded();
        assert_eq!(
            idb.count(theirs, Source::Store(store), &all),
            Ok(2),
            "{role}"
        );
    }
}

/// A player of the tab: it waits for the tab's mute to come down as `true`, plays, waits for the
/// browser to unmute the tab, stops (its player goes), and plays again until it is told to end by
/// the mute coming down `true` once more, then exits while playing.
fn audio_child(port: &RendererPort) -> i32 {
    use snowlark_media::audio::{Audible, PlayerAudio};
    let services = remote(port);
    let group = services.audio.group().clone();
    let muted = |want: bool| {
        until("the tab's mute", || group.muted() == want);
    };
    let playing = Audible {
        playing: true,
        ..Audible::new(1.0, false)
    };
    muted(true);
    let mut player = PlayerAudio::new(Some(group.clone()));
    player.update(&playing);
    muted(false);
    drop(player);
    let mut player = PlayerAudio::new(Some(group.clone()));
    player.update(&playing);
    muted(true);
    port.flush(WAIT).unwrap();
    // (exits with the player still audible: the browser must clear the sound itself)
    std::mem::forget(player);
    0
}

#[test]
fn a_tab_in_a_renderer_is_muted_and_heard() {
    let mut l = local();
    let (tx, heard) = mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    l.services.audible = Arc::new(move |a| {
        let _ = tx.lock().unwrap().send(a);
    });
    l.services.muted = true;
    let child = start("audio", &l.services);
    let next = || heard.recv_timeout(WAIT).expect("an audible change");
    assert!(next());
    assert!(child.host.audio().audible());
    snowlark_media::TabMuteControl::set_muted(&**child.host.audio(), false);
    assert!(!next());
    assert!(next());
    snowlark_media::TabMuteControl::set_muted(&**child.host.audio(), true);
    assert_eq!(child.process.wait(WAIT), Some(0));
    assert!(matches!(child.served.join().unwrap(), Error::Closed));
    assert!(!next(), "a renderer that ends while audible is silent");
    assert!(heard.try_recv().is_err());
}

/// An output device for the tests: 44.1 kHz stereo, its sinks paced as a device's (a null sink's
/// clock) and recording each period's first frame, channel by channel.
struct RecordingOutputs {
    heard: Arc<std::sync::Mutex<Vec<Vec<f32>>>>,
}

const TEST_DEVICE: snowlark_audio::realtime::DeviceInfo = snowlark_audio::realtime::DeviceInfo {
    sample_rate: 44100.0,
    channels: 2,
    min_period: 0.01,
    max_period: 0.08,
    output_latency: 0.005,
};

struct RecordingSink {
    clock: snowlark_audio::realtime::NullSink,
    heard: Arc<std::sync::Mutex<Vec<Vec<f32>>>>,
}

impl snowlark_audio::realtime::AudioSink for RecordingSink {
    fn start(
        &mut self,
        config: &snowlark_audio::realtime::StreamConfig,
    ) -> Result<(), snowlark_audio::realtime::SinkError> {
        self.clock.start(config)
    }
    fn wait_period(&mut self) -> Result<Instant, snowlark_audio::realtime::SinkError> {
        self.clock.wait_period()
    }
    fn write(&mut self, channels: &[Vec<f32>]) -> Result<(), snowlark_audio::realtime::SinkError> {
        let first = channels.iter().map(|c| c[0]).collect();
        self.heard.lock().unwrap().push(first);
        self.clock.write(channels)
    }
    fn stop(&mut self) {
        self.clock.stop()
    }
}

impl snowlark_audio::realtime::AudioOutputs for RecordingOutputs {
    fn device(&self) -> Option<snowlark_audio::realtime::DeviceInfo> {
        Some(TEST_DEVICE)
    }
    fn open(&self) -> Box<dyn snowlark_audio::realtime::AudioSink> {
        Box::new(RecordingSink {
            clock: Default::default(),
            heard: self.heard.clone(),
        })
    }
}

const AUDIO_OUT_PERIODS: usize = 20;
const AUDIO_OUT_FRAMES: usize = 512;

/// An AudioContext's output in a renderer: it asks for the device, starts a stream and sends
/// periods `1..=AUDIO_OUT_PERIODS` (channel 0 the period's number, channel 1 its negative) as the
/// credits allow, then stops. Exits 0 when the device and the credits were as they should be.
fn audio_out_child(port: &RendererPort) -> i32 {
    use snowlark_audio::realtime::{AudioOutputs, StreamConfig};
    let services = remote(port);
    let outputs = services.audio_out.clone();
    if outputs.device() != Some(TEST_DEVICE) {
        return 61;
    }
    let mut sink = outputs.open();
    let config = StreamConfig {
        sample_rate: 44100.0,
        channels: 2,
        period_frames: AUDIO_OUT_FRAMES,
    };
    if sink.start(&config).is_err() {
        return 62;
    }
    let period = Duration::from_secs_f64(AUDIO_OUT_FRAMES as f64 / 44100.0);
    let started = Instant::now();
    for i in 1..=AUDIO_OUT_PERIODS {
        if sink.wait_period().is_err() {
            return 63;
        }
        let v = i as f32;
        let channels = vec![vec![v; AUDIO_OUT_FRAMES], vec![-v; AUDIO_OUT_FRAMES]];
        if sink.write(&channels).is_err() {
            return 64;
        }
    }
    // (the credits pace the renderer at the device's rate: no faster than the periods beyond the
    // first ones in flight can play)
    let least = period * (AUDIO_OUT_PERIODS - audio_out::IN_FLIGHT - 1) as u32;
    if started.elapsed() < least {
        return 65;
    }
    // (a stop drops the periods not played yet: silence behind the last one carries it out)
    let silence = vec![vec![0.0; AUDIO_OUT_FRAMES]; 2];
    for _ in 0..audio_out::IN_FLIGHT {
        if sink.wait_period().is_err() || sink.write(&silence).is_err() {
            return 66;
        }
    }
    sink.stop();
    port.flush(WAIT).unwrap();
    0
}

/// A renderer that breaks the output service's rules: a period for a stream it never opened, one
/// of the wrong size, more periods than its credits, a stream id used twice.
fn audio_out_forge(port: &RendererPort, role: &str) -> i32 {
    let config = audio_out::Config {
        sample_rate: 44100.0,
        channels: 2,
        period_frames: 4096,
    };
    let start = |id: u32| {
        let up = Up::AudioOut(audio_out::Up::Start { id, config });
        let _ = port.call::<Bulk>(&up, WAIT);
    };
    let period = |id: u32, n: usize| {
        let samples = vec![0.5; n];
        port.send(&Up::AudioOut(audio_out::Up::Period { id, samples }))
            .unwrap();
    };
    match role {
        "forge-audio-out-stream" => period(3, 8192),
        "forge-audio-out-size" => {
            start(1);
            period(1, 8191);
        }
        "forge-audio-out-credits" => {
            start(1);
            for _ in 0..audio_out::IN_FLIGHT + 6 {
                period(1, 8192);
            }
        }
        "forge-audio-out-id" => {
            start(4);
            start(4);
        }
        other => panic!("role {other}"),
    }
    linger()
}

/// Starts a stream, sends a period and exits without stopping it.
fn audio_out_exit_child(port: &RendererPort) -> i32 {
    use snowlark_audio::realtime::{AudioOutputs, StreamConfig};
    let services = remote(port);
    let mut sink = services.audio_out.open();
    let config = StreamConfig {
        sample_rate: 44100.0,
        channels: 2,
        period_frames: 128,
    };
    if sink.start(&config).is_err() || sink.wait_period().is_err() {
        return 71;
    }
    let _ = sink.write(&[vec![0.5; 128], vec![0.5; 128]]);
    port.flush(WAIT).unwrap();
    std::mem::forget(sink);
    0
}

/// An AudioContext in a renderer plays on the browser's device: the periods arrive whole, in
/// order, at the device's pace, and the stream ends with the renderer's `stop`.
#[test]
fn a_renderers_audio_plays_on_the_browsers_device() {
    let mut l = local();
    let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
    l.services.audio_out = Some(Arc::new(RecordingOutputs {
        heard: heard.clone(),
    }));
    let child = start("audio-out", &l.services);
    assert_eq!(child.process.wait(WAIT), Some(0), "the renderer's checks");
    until("the stream to end", || child.host.audio_out.streams() == 0);
    let played: Vec<Vec<f32>> = heard
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p[0] != 0.0)
        .cloned()
        .collect();
    let want: Vec<Vec<f32>> = (1..=AUDIO_OUT_PERIODS)
        .map(|i| vec![i as f32, -(i as f32)])
        .collect();
    assert_eq!(played, want);
}

#[test]
fn a_renderer_that_breaks_the_audio_output_rules_dies() {
    for role in [
        "forge-audio-out-stream",
        "forge-audio-out-size",
        "forge-audio-out-credits",
        "forge-audio-out-id",
    ] {
        let mut l = local();
        l.services.audio_out = Some(Arc::new(RecordingOutputs {
            heard: Arc::default(),
        }));
        let child = start(role, &l.services);
        assert_eq!(child.process.wait(WAIT), Some(9), "{role}");
        assert!(
            matches!(child.served.join().unwrap(), Error::BadMessage(_)),
            "{role}"
        );
        assert_eq!(child.host.audio_out.streams(), 0, "{role}: its streams end");
    }
}

/// A renderer that ends while its context plays leaves no stream behind.
#[test]
fn a_renderers_streams_end_with_it() {
    let mut l = local();
    l.services.audio_out = Some(Arc::new(RecordingOutputs {
        heard: Arc::default(),
    }));
    let child = start("audio-out-exit", &l.services);
    assert_eq!(child.process.wait(WAIT), Some(0));
    assert!(matches!(child.served.join().unwrap(), Error::Closed));
    assert_eq!(child.host.audio_out.streams(), 0);
}

/// The browser's key systems for the tests: one, `org.test.module`, whose instances are 1001,
/// 1002, ... and sessions 2001, ... (never the renderer's ids), answering every call at once and
/// logging it.
#[derive(Default)]
struct FakeCdm {
    inbox: std::sync::Mutex<Option<Arc<snowlark_page::media::cdm::CdmInbox>>>,
    next: std::sync::atomic::AtomicU64,
    calls: std::sync::Mutex<Vec<String>>,
}

impl FakeCdm {
    fn log(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
    fn send(&self, e: snowlark_page::media::cdm::CdmEvent) {
        let inbox = self.inbox.lock().unwrap().clone();
        inbox.unwrap().deliver(e);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl snowlark_page::media::cdm::Cdm for FakeCdm {
    fn supports(&self, key_system: &str, _: &str) -> bool {
        key_system == "org.test.module"
    }
    fn create_keys(
        &self,
        request: u64,
        _: &str,
        origin: &str,
        inbox: &Arc<snowlark_page::media::cdm::CdmInbox>,
    ) -> u64 {
        use std::sync::atomic::Ordering;
        let keys = 1001 + self.next.fetch_add(1, Ordering::Relaxed);
        self.log(format!("create {keys} {origin}"));
        *self.inbox.lock().unwrap() = Some(inbox.clone());
        self.send(snowlark_page::media::cdm::CdmEvent::Done {
            request,
            result: Ok(String::new()),
        });
        keys
    }
    fn create_session(&self, keys: u64) -> u64 {
        let session = keys + 1000;
        self.log(format!("session {keys} {session}"));
        session
    }
    fn generate_request(&self, request: u64, session: u64, _: &str, mut data: Vec<u8>) {
        use snowlark_page::media::cdm::{CdmEvent, MessageKind};
        self.log(format!("generate {session}"));
        data.reverse();
        self.send(CdmEvent::Message {
            session,
            kind: MessageKind::LicenseRequest,
            data,
        });
        self.send(CdmEvent::Done {
            request,
            result: Ok(format!("module-{session}")),
        });
    }
    fn update(&self, request: u64, session: u64, response: Vec<u8>) {
        use snowlark_page::media::cdm::{CdmEvent, KeyStatus};
        self.log(format!("update {session}"));
        self.send(CdmEvent::KeyStatuses {
            session,
            statuses: vec![(response, KeyStatus::Usable)],
        });
        // (an event for a session the renderer never had is not forwarded)
        self.send(CdmEvent::Closed { session: 9999 });
        self.send(CdmEvent::Done {
            request,
            result: Ok(String::new()),
        });
    }
    fn close(&self, _: u64, _: u64) {}
    fn remove(&self, _: u64, _: u64) {}
    fn release(&self, keys: u64) {
        self.log(format!("release {keys}"));
    }
}

/// A renderer's EME calls in its own ids: two instances, a session's request, update and key
/// statuses; it releases the first and exits with the second still made.
fn cdm_child(port: &RendererPort) -> i32 {
    use snowlark_page::media::cdm::{Cdm, CdmEvent, CdmInbox, KeyStatus, MessageKind};
    let services = remote(port);
    let cdm: &dyn Cdm = &*services.cdm;
    let (tx, woken) = mpsc::channel();
    let inbox = Arc::new(CdmInbox::new(Arc::new(move || {
        let _ = tx.send(());
    })));
    let events = |n: usize| {
        let mut got = Vec::new();
        while got.len() < n {
            if woken.recv_timeout(WAIT).is_err() {
                break;
            }
            got.extend(inbox.take());
        }
        got
    };
    if !cdm.supports("org.test.module", "video/mp4") || cdm.supports("org.other", "video/mp4") {
        return 11;
    }
    let done = |request: u64, id: &str| CdmEvent::Done {
        request,
        result: Ok(id.to_owned()),
    };
    let keys = cdm.create_keys(1, "org.test.module", "https://a.test", &inbox);
    if events(1) != [done(1, "")] {
        return 12;
    }
    let session = cdm.create_session(keys);
    cdm.generate_request(2, session, "cenc", vec![1, 2, 3]);
    let message = CdmEvent::Message {
        session,
        kind: MessageKind::LicenseRequest,
        data: vec![3, 2, 1],
    };
    if events(2) != [message, done(2, "module-2001")] {
        return 13;
    }
    cdm.update(3, session, vec![9]);
    let statuses = CdmEvent::KeyStatuses {
        session,
        statuses: vec![(vec![9], KeyStatus::Usable)],
    };
    if events(2) != [statuses, done(3, "")] {
        return 14;
    }
    let other = cdm.create_keys(4, "org.test.module", "https://a.test", &inbox);
    if other == keys || events(1) != [done(4, "")] {
        return 15;
    }
    cdm.release(keys);
    port.flush(WAIT).unwrap();
    0
}

/// Calls no honest renderer makes.
fn cdm_forge(port: &RendererPort, role: &str) -> i32 {
    use cdm::Up as C;
    let send = |m: C| port.send(&Up::Cdm(m)).unwrap();
    let keys = |request: u64, keys: u64| C::CreateKeys {
        request,
        keys,
        key_system: "org.test.module".into(),
        origin: "https://a.test".into(),
    };
    match role {
        // a session it never made
        "forge-cdm-session" => send(C::Generate {
            request: 1,
            session: 5,
            init_data_type: "cenc".into(),
            data: vec![1],
        }),
        // a session of an instance it never made
        "forge-cdm-keys" => send(C::CreateSession {
            keys: 3,
            session: 1,
        }),
        // more than any licence
        "forge-cdm-size" => {
            send(keys(1, 1));
            send(C::CreateSession {
                keys: 1,
                session: 2,
            });
            send(C::Update {
                request: 3,
                session: 2,
                response: vec![0; cdm::MAX_DATA + 1],
            });
        }
        "forge-cdm-twice" => {
            send(keys(1, 1));
            send(keys(2, 1));
        }
        "forge-cdm-release" => send(C::Release { keys: 4 }),
        other => panic!("role {other}"),
    }
    linger()
}

#[test]
fn a_renderers_key_systems_cross_in_its_own_ids() {
    let mut l = local();
    let fake = Arc::new(FakeCdm::default());
    l.services.cdm = Some(fake.clone());
    let child = start("cdm", &l.services);
    assert_eq!(child.process.wait(WAIT), Some(0), "the renderer's checks");
    assert!(matches!(child.served.join().unwrap(), Error::Closed));
    assert_eq!(
        fake.calls(),
        [
            "create 1001 https://a.test",
            "session 1001 2001",
            "generate 2001",
            "update 2001",
            "create 1002 https://a.test",
            "release 1001",
            // (the one it left is released with it)
            "release 1002",
        ]
    );
}

#[test]
fn a_renderer_that_breaks_the_key_system_rules_dies() {
    for (role, calls) in [
        ("forge-cdm-session", &[][..]),
        ("forge-cdm-keys", &[]),
        (
            "forge-cdm-size",
            &[
                "create 1001 https://a.test",
                "session 1001 2001",
                "release 1001",
            ],
        ),
        (
            "forge-cdm-twice",
            &["create 1001 https://a.test", "release 1001"],
        ),
        ("forge-cdm-release", &[]),
    ] {
        let mut l = local();
        let fake = Arc::new(FakeCdm::default());
        l.services.cdm = Some(fake.clone());
        let child = start(role, &l.services);
        assert_eq!(child.process.wait(WAIT), Some(9), "{role}");
        assert!(
            matches!(child.served.join().unwrap(), Error::BadMessage(_)),
            "{role}"
        );
        assert_eq!(fake.calls(), calls, "{role}");
    }
}
