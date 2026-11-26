// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `--wpt` with each test page's services across a pipe (`SNOWLARK_WPT_SITE_LOCK=<file>`): the page's
//! network, cookies, blobs, storage, broadcast channels and IndexedDB are the renderer process's
//! clients, served by the browser's hosts over an in-process connection that crosses the wire format,
//! with the host's process policy locked to the test's site, as the window's tab host locks a renderer
//! at its first commit. The site lock's checks run in their configured mode (`SNOWLARK_SITE_LOCK`,
//! shadow by default), and after each test the number of failed checks, when not zero, is appended to
//! the file with the test's URL: a full WPT run read for false alarms (`docs/process-isolation.md`,
//! M5 step 4).
//!
//! The top-level navigation is the browser's in the window (the tab host fetches it), so here it goes
//! to the network directly, not through the renderer's client.

use std::io::Write;
use std::sync::{Arc, Mutex};

use snowlark_net::{NetBackend, NetClient, Observer, Request, RequestFilter, ResponseStream};
use snowlark_tab::services::{
    site_lock, BrowserPort, LocalServices, RemoteServices, RendererPort, ServiceHost,
};

/// What the services cross to, for every test of the run.
pub struct Remote {
    local: LocalServices,
    log: Mutex<std::fs::File>,
}

/// One test page's connection.
pub struct Session {
    pub services: Arc<RemoteServices>,
    browser: BrowserPort,
    child: RendererPort,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Remote {
    /// From `SNOWLARK_WPT_SITE_LOCK`; `None` when unset.
    pub fn from_env(net: &NetClient) -> Result<Option<Remote>, String> {
        let Ok(path) = std::env::var("SNOWLARK_WPT_SITE_LOCK") else {
            return Ok(None);
        };
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open {path}: {e}"))?;
        let local = LocalServices {
            broadcast: Arc::new(snowlark_page::LocalBroadcast::default()),
            storage: snowlark_page::web_storage::WebStorageHost::new(),
            code_cache: Arc::new(snowlark_shell::tab_host::NoCodeCache),
            clipboard: Arc::new(snowlark_tab::driver::MemoryClipboard::default()),
            internal: Arc::new(snowlark_tab::driver::NoInternalPages),
            content_filter: None,
            net: net.backend().clone(),
            request_filter: None,
            idb: Arc::new(snowlark_idb::LocalIdb::in_memory()),
            caches: Arc::new(snowlark_sw::LocalCacheStorage::in_memory()),
            site_lock: site_lock::Mode::from_env(),
            audible: Arc::new(|_| {}),
            muted: true,
            audio_out: None,
            videos: None,
            cdm: None,
        };
        Ok(Some(Remote {
            local,
            log: Mutex::new(log),
        }))
    }

    /// Connects a page that will navigate to `url`, locked to its site.
    pub fn start(&self, url: &url::Url) -> Result<Session, String> {
        let (browser, child) = snowlark_ipc::loopback::pair(0).map_err(|e| format!("{e:?}"))?;
        if let Some(site) = snowlark_net::origin::site_of_url(url) {
            browser.policy().lock.lock_to(&site)?;
        }
        let services = Arc::new(RemoteServices::new(&child));
        let host = ServiceHost::new(browser.clone(), &self.local);
        let mut threads = Vec::new();
        threads.push(
            std::thread::Builder::new()
                .name("wpt service host".into())
                .spawn(move || {
                    host.run();
                })
                .map_err(|e| e.to_string())?,
        );
        {
            let (services, child) = (services.clone(), child.clone());
            threads.push(
                std::thread::Builder::new()
                    .name("wpt renderer io".into())
                    .spawn(move || {
                        services.run(&child);
                    })
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(Session {
            services,
            browser,
            child,
            threads,
        })
    }
}

impl Session {
    /// The page's network: the renderer's client, but the top-level navigation the browser's own.
    pub fn net(&self, browser: &NetClient) -> NetClient {
        NetClient::with_backend(Arc::new(Split {
            renderer: self.services.net.clone(),
            browser: browser.backend().clone(),
        }))
    }

    /// Closes the connection and logs the test's failed checks, if any.
    pub fn finish(self, remote: &Remote, test: &str) {
        let noted = self.browser.policy().lock.noted();
        if noted > 0 {
            let mut log = remote.log.lock().unwrap_or_else(|e| e.into_inner());
            let _ = log.write_all(format!("{test}\t{noted}\n").as_bytes());
        }
        self.child.close();
        self.browser.close();
        for t in self.threads {
            let _ = t.join();
        }
    }
}

struct Split {
    renderer: Arc<snowlark_tab::services::net::RemoteNet>,
    browser: Arc<dyn NetBackend>,
}

impl NetBackend for Split {
    fn start(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        observer: Option<Observer>,
        filter: Option<Arc<dyn RequestFilter>>,
    ) -> ResponseStream {
        if request.top_level_navigation {
            self.browser.start(request, notify, observer, filter)
        } else {
            self.renderer.start(request, notify, observer, filter)
        }
    }

    fn websocket(
        &self,
        params: snowlark_net::websocket::WsParams,
        filter: Option<Arc<dyn RequestFilter>>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> snowlark_net::websocket::WebSocketConnection {
        self.renderer.websocket(params, filter, notify)
    }

    fn preload(
        &self,
        request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        filter: Option<Arc<dyn RequestFilter>>,
    ) -> Option<ResponseStream> {
        self.renderer.preload(request, notify, filter)
    }

    fn preload_counts(&self) -> (usize, usize) {
        self.renderer.preload_counts()
    }

    fn is_preloaded(&self, request: &Request) -> bool {
        self.renderer.is_preloaded(request)
    }

    fn preconnect(&self, url: &url::Url) {
        self.renderer.preconnect(url)
    }

    fn dns_prefetch(&self, host: &str, port: u16) {
        self.renderer.dns_prefetch(host, port)
    }

    fn user_agent(&self) -> Arc<snowlark_net::ua::UserAgent> {
        self.renderer.user_agent()
    }

    fn trace(&self) -> bool {
        self.renderer.trace()
    }

    fn profile_dir(&self) -> Option<std::path::PathBuf> {
        self.renderer.profile_dir()
    }

    fn document_cookie(&self, context: &snowlark_net::cookies::CookieContext) -> String {
        self.renderer.document_cookie(context)
    }

    fn set_document_cookie(&self, value: &str, context: &snowlark_net::cookies::CookieContext) {
        self.renderer.set_document_cookie(value, context)
    }

    fn register_blob(&self, origin: &url::Origin, data: snowlark_net::blob::BlobData) -> String {
        self.renderer.register_blob(origin, data)
    }

    fn revoke_blob(&self, url: &url::Url, origin: &url::Origin) {
        self.renderer.revoke_blob(url, origin)
    }

    fn hold_blob_url(&self, url: &url::Url) -> Option<u64> {
        self.renderer.hold_blob_url(url)
    }

    fn release_blob_url(&self, token: u64) {
        self.renderer.release_blob_url(token)
    }

    fn hand_over(&self, stream: ResponseStream) -> Option<u64> {
        self.renderer.hand_over(stream)
    }

    fn adopt(
        &self,
        id: u64,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Option<ResponseStream> {
        self.renderer.adopt(id, notify)
    }
}
