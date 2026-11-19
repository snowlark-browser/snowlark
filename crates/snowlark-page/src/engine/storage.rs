// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §12 Web storage: `localStorage` and `sessionStorage`, with the `Storage` interface's named
//! properties and the `StorageEvent` interface.
//!
//! `localStorage` is a cached copy of an area the [`WebStorage`] service owns (`crate::web_storage`):
//! reads are local, a write applies here at once and goes out without waiting, and what other event
//! loops change comes back as notices. `sessionStorage` is this event loop's own, in memory. A
//! document with an opaque origin gets an area of its own that nothing else sees and nothing keeps.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use snowlark_bindings::generated::{
    self as g, StorageEventImpl, StorageImpl, WindowLocalStorageImpl, WindowSessionStorageImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_ipc::loopback::through;

use super::events::EventExt;
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;
use crate::web_storage::{
    self, ClientId, StorageNotice, StorageWrite, WebStorage, WebStorageHost, QUOTA,
};

/// A storage area as this event loop has it.
#[derive(Default)]
pub struct Area {
    data: web_storage::Area,
    /// The origin to tell the service, for an area of the service's.
    origin: Option<String>,
    /// Writes of ours the service has not answered yet, by key. What another event loop did to such
    /// a key is older than ours as far as this copy is concerned, so it does not overwrite it.
    pending: HashMap<String, u32>,
    pending_clears: u32,
}

impl Area {
    pub fn is_empty(&self) -> bool {
        self.data.items.is_empty()
    }

    /// Empties the area here (see `clear_site_storage`).
    pub fn clear(&mut self) {
        self.data = web_storage::Area::default();
    }

    /// What another event loop did, unless a write of ours supersedes it.
    fn apply(&mut self, key: &Option<String>, new_value: &Option<String>) {
        if self.pending_clears > 0 {
            return;
        }
        match (key, new_value) {
            (Some(k), _) if self.pending.contains_key(k) => {}
            (Some(k), Some(v)) => {
                self.data.set(k.clone(), v.clone());
            }
            (Some(k), None) => {
                self.data.remove(k);
            }
            (None, _) => {
                let keep: Vec<(String, String)> = std::mem::take(&mut self.data.items)
                    .into_iter()
                    .filter(|(k, _)| self.pending.contains_key(k))
                    .collect();
                self.data = web_storage::Area::from_items(keep);
            }
        }
    }

    /// The service answered a write of ours.
    fn answered(&mut self, key: &Option<String>) {
        match key {
            Some(k) => {
                if let Some(n) = self.pending.get_mut(k) {
                    *n -= 1;
                    if *n == 0 {
                        self.pending.remove(k);
                    }
                }
            }
            None => self.pending_clears = self.pending_clears.saturating_sub(1),
        }
    }

    fn replace(&mut self, items: web_storage::Items) {
        self.data = web_storage::Area::from_items(items);
        self.pending.clear();
        self.pending_clears = 0;
    }
}

/// The storage areas by origin, and this event loop's place in the service.
#[derive(Default)]
pub struct StorageState {
    pub local: HashMap<String, Rc<RefCell<Area>>>,
    pub session: HashMap<String, Rc<RefCell<Area>>>,
    service: Option<Arc<dyn WebStorage>>,
    client: Option<ClientId>,
    inbox: Arc<Mutex<VecDeque<StorageNotice>>>,
}

impl Drop for StorageState {
    fn drop(&mut self) {
        if let (Some(service), Some(client)) = (&self.service, self.client) {
            service.disconnect(client);
        }
    }
}

const LOCAL: &str = "localStorage";
const SESSION: &str = "sessionStorage";

/// Whether an origin is one the service may hold: an opaque origin is nobody's to keep.
fn shareable(origin: &str) -> bool {
    origin != "null" && !origin.starts_with("opaque:")
}

impl Engine {
    fn area(&self, h: Handle) -> Rc<RefCell<Area>> {
        match &*self.obj(h) {
            Object::Storage(a, _) => a.clone(),
            _ => panic!("not a Storage"),
        }
    }

    /// HTML §12.2.2 "If this's relevant settings object's origin is an opaque origin, throw": a
    /// sandbox's or a `data:` document's. (`file:` documents keep a storage by their URL.)
    fn check_storage_origin(&self, window: Handle) -> rt::Result<()> {
        let opaque = self
            .window_origin(window)
            .is_some_and(|o| o.starts_with("opaque:sandbox:") || o.starts_with("opaque:data:"));
        if opaque {
            return Err(super::exception(
                rt::ExceptionName::SecurityError,
                "The document has an opaque origin",
            ));
        }
        Ok(())
    }

    fn storage_window(&self, h: Handle) -> Handle {
        match &*self.obj(h) {
            Object::Storage(_, w) => *w,
            _ => panic!("not a Storage"),
        }
    }

    /// Uses `service` for this event loop's `localStorage` (the embedder's, shared by its pages).
    pub fn set_web_storage(&self, service: Arc<dyn WebStorage>) {
        self.storage.borrow_mut().service = Some(service);
    }

    /// The service and this event loop's place in it, joined on first use.
    fn web_storage(&self) -> (Arc<dyn WebStorage>, ClientId) {
        let mut st = self.storage.borrow_mut();
        let service = st
            .service
            .get_or_insert_with(|| WebStorageHost::new() as Arc<dyn WebStorage>)
            .clone();
        let client = match st.client {
            Some(c) => c,
            None => {
                let inbox = st.inbox.clone();
                let waker = self.waker();
                let c = service.connect(Arc::new(move |notice| {
                    let notice = through(notice);
                    inbox
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_back(notice);
                    waker.wake();
                }));
                st.client = Some(c);
                c
            }
        };
        (service, client)
    }

    /// The cached area of `origin`, from the service the first time.
    fn local_area(&self, origin: &str) -> Rc<RefCell<Area>> {
        if let Some(a) = self.storage.borrow().local.get(origin) {
            return a.clone();
        }
        let area = if shareable(origin) {
            let (service, client) = self.web_storage();
            Area {
                data: web_storage::Area::from_items(through(service.open(client, origin))),
                origin: Some(origin.to_owned()),
                ..Default::default()
            }
        } else {
            Area::default()
        };
        let area = Rc::new(RefCell::new(area));
        self.storage
            .borrow_mut()
            .local
            .insert(origin.to_owned(), area.clone());
        area
    }

    fn storage_for(&self, window: Handle, local: bool) -> Handle {
        let name = if local { LOCAL } else { SESSION };
        let origin = self
            .associated_document(window)
            .map(|d| self.origin_of_document(d))
            .unwrap_or_default();
        let area = if local {
            self.local_area(&origin)
        } else {
            self.storage
                .borrow_mut()
                .session
                .entry(origin)
                .or_default()
                .clone()
        };
        self.window_singleton(window, name, || Object::Storage(area, window))
    }

    /// The `Storage` objects of an area, with their windows.
    fn storages_of(&self, area: &Rc<RefCell<Area>>) -> Vec<(Handle, Handle)> {
        let f = self.frames.borrow();
        let mut found: Vec<(Handle, Handle)> = f
            .singletons
            .iter()
            .filter(|((_, name), _)| *name == LOCAL || *name == SESSION)
            .filter_map(|(&(window, _), &h)| match &*self.obj(h) {
                Object::Storage(a, _) if Rc::ptr_eq(a, area) => Some((h, window)),
                _ => None,
            })
            .collect();
        // (in a fixed order: the map's own would differ between runs)
        found.sort_by_key(|(h, _)| *h);
        found
    }

    /// HTML §12.2.1 "broadcast", to this event loop's other `Storage` objects of the area: a task
    /// for each, which fires `storage` at its window.
    fn broadcast_storage(
        &self,
        source: Option<Handle>,
        area: &Rc<RefCell<Area>>,
        key: Option<String>,
        old_value: Option<String>,
        new_value: Option<String>,
        url: String,
    ) {
        for (storage, window) in self.storages_of(area) {
            if Some(storage) == source {
                continue;
            }
            let fields = StorageFields {
                key: key.clone(),
                old_value: old_value.clone(),
                new_value: new_value.clone(),
                url: url.clone(),
                storage_area: Some(storage),
            };
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                e.fire_storage(cx, window, fields)
            });
        }
    }

    fn fire_storage(&self, cx: &mut Cx, window: Handle, fields: StorageFields) {
        let alive = self
            .document_of_window(window)
            .is_some_and(|d| self.is_active_document(d));
        if !alive {
            return;
        }
        let realm = self
            .context_of_window(window)
            .and_then(|id| self.realm_of_context(id));
        self.in_realm(cx, realm, |cx| {
            let ev = self.trusted_event(
                InterfaceId::StorageEvent,
                "storage",
                false,
                false,
                EventExt::Storage(fields),
            );
            self.dispatch(cx, ev, window, false);
        });
    }

    /// The URL of the document a `Storage` object belongs to (`StorageEvent.url`).
    fn storage_url(&self, storage: Handle) -> String {
        self.document_of_window(self.storage_window(storage))
            .map(|d| self.dom().document_url(d).to_owned())
            .unwrap_or_default()
    }

    /// A change made here: to this event loop's other documents, and on to the service.
    fn changed(
        &self,
        storage: Handle,
        area: &Rc<RefCell<Area>>,
        key: Option<String>,
        old_value: Option<String>,
        new_value: Option<String>,
    ) {
        let url = self.storage_url(storage);
        let origin = area.borrow().origin.clone();
        if let Some(origin) = origin {
            {
                let mut a = area.borrow_mut();
                match &key {
                    Some(k) => *a.pending.entry(k.clone()).or_insert(0) += 1,
                    None => a.pending_clears += 1,
                }
            }
            let write = match (&key, &new_value) {
                (Some(k), Some(v)) => StorageWrite::Set {
                    origin,
                    key: k.clone(),
                    value: v.clone(),
                    url: url.clone(),
                },
                (Some(k), None) => StorageWrite::Remove {
                    origin,
                    key: k.clone(),
                    url: url.clone(),
                },
                (None, _) => StorageWrite::Clear {
                    origin,
                    url: url.clone(),
                },
            };
            let (service, client) = self.web_storage();
            service.write(client, through(write));
        }
        self.broadcast_storage(Some(storage), area, key, old_value, new_value, url);
    }

    /// Takes in what the service said. Returns whether it said anything.
    pub(crate) fn poll_storage(&self) -> bool {
        let notices: Vec<StorageNotice> = {
            let st = self.storage.borrow();
            let mut inbox = st.inbox.lock().unwrap_or_else(|e| e.into_inner());
            inbox.drain(..).collect()
        };
        if notices.is_empty() {
            return false;
        }
        let me = self.storage.borrow().client;
        for notice in notices {
            match notice {
                StorageNotice::Changed {
                    origin,
                    key,
                    old_value,
                    new_value,
                    url,
                    by,
                } => {
                    let area = self.storage.borrow().local.get(&origin).cloned();
                    let Some(area) = area else { continue };
                    if Some(by) == me {
                        area.borrow_mut().answered(&key);
                        continue;
                    }
                    area.borrow_mut().apply(&key, &new_value);
                    self.broadcast_storage(None, &area, key, old_value, new_value, url);
                }
                StorageNotice::Refused { origin, items } => {
                    let area = self.storage.borrow().local.get(&origin).cloned();
                    if let Some(area) = area {
                        area.borrow_mut().replace(items);
                    }
                }
                StorageNotice::Wiped { origin } => {
                    let area = self.storage.borrow().local.get(&origin).cloned();
                    if let Some(area) = area {
                        area.borrow_mut().replace(Vec::new());
                    }
                }
            }
        }
        true
    }
}

impl WindowLocalStorageImpl for Engine {
    fn local_storage(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.check_storage_origin(this)?;
        Ok(self.storage_for(this, true))
    }
}

impl WindowSessionStorageImpl for Engine {
    fn session_storage(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        self.check_storage_origin(this)?;
        Ok(self.storage_for(this, false))
    }
}

impl StorageImpl for Engine {
    fn length(&self, this: Handle) -> u32 {
        self.area(this).borrow().data.items.len() as u32
    }
    fn get_item(&self, this: Handle, name: &str) -> Option<DomStr<'_>> {
        self.area(this)
            .borrow()
            .data
            .get(name)
            .map(|v| DomStr::Owned(v.to_owned()))
    }
    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        self.area(this)
            .borrow()
            .data
            .items
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }
    /// HTML §12.2.1 `setItem(key, value)`: nothing at all if the value is the same, a
    /// QuotaExceededError past the quota, and otherwise the change is broadcast.
    fn set_item(&self, _cx: &mut Cx, this: Handle, name: &str, value: &str) -> rt::Result<()> {
        let area = self.area(this);
        let old = {
            let mut a = area.borrow_mut();
            if a.data.get(name) == Some(value) {
                return Ok(());
            }
            if web_storage::size_after(&a.data, name, value) > QUOTA {
                return Err(exception(
                    ExceptionName::QuotaExceededError,
                    "The storage quota is exceeded",
                ));
            }
            a.data.set(name.to_owned(), value.to_owned())
        };
        self.changed(
            this,
            &area,
            Some(name.to_owned()),
            old,
            Some(value.to_owned()),
        );
        Ok(())
    }
    /// HTML §12.2.1 `removeItem(key)`.
    fn remove_item(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<()> {
        let area = self.area(this);
        let old = area.borrow_mut().data.remove(name);
        if old.is_some() {
            self.changed(this, &area, Some(name.to_owned()), old, None);
        }
        Ok(())
    }
    fn key(&self, _cx: &mut Cx, this: Handle, index: u32) -> rt::Result<Option<DomStr<'_>>> {
        Ok(self
            .area(this)
            .borrow()
            .data
            .items
            .get(index as usize)
            .map(|(k, _)| DomStr::Owned(k.clone())))
    }
    /// HTML §12.2.1 `clear()`.
    fn clear(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let area = self.area(this);
        if area.borrow().is_empty() {
            return Ok(());
        }
        area.borrow_mut().clear();
        self.changed(this, &area, None, None, None);
        Ok(())
    }
}

fn storage_ext(e: &EventExt) -> Option<&StorageFields> {
    match e {
        EventExt::Storage(s) => Some(s),
        _ => None,
    }
}

/// A `StorageEvent`'s members.
#[derive(Clone, Default)]
pub struct StorageFields {
    pub key: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub url: String,
    pub storage_area: Option<Handle>,
}

impl StorageEventImpl for Engine {
    fn key(&self, this: Handle) -> Option<DomStr<'_>> {
        self.ev(this, |e| storage_ext(&e.ext).and_then(|s| s.key.clone()))
            .map(DomStr::Owned)
    }
    fn old_value(&self, this: Handle) -> Option<DomStr<'_>> {
        self.ev(this, |e| {
            storage_ext(&e.ext).and_then(|s| s.old_value.clone())
        })
        .map(DomStr::Owned)
    }
    fn new_value(&self, this: Handle) -> Option<DomStr<'_>> {
        self.ev(this, |e| {
            storage_ext(&e.ext).and_then(|s| s.new_value.clone())
        })
        .map(DomStr::Owned)
    }
    fn url(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| {
            storage_ext(&e.ext)
                .map(|s| s.url.clone())
                .unwrap_or_default()
        }))
    }
    fn storage_area(&self, this: Handle) -> Option<Handle> {
        self.ev(this, |e| storage_ext(&e.ext).and_then(|s| s.storage_area))
    }
    #[allow(clippy::too_many_arguments)]
    fn init_storage_event(
        &self,
        _cx: &mut Cx,
        this: Handle,
        type_: &str,
        bubbles: bool,
        cancelable: bool,
        key: Option<&str>,
        old_value: Option<&str>,
        new_value: Option<&str>,
        url: &str,
        storage_area: Option<Handle>,
    ) -> rt::Result<()> {
        if self.ev(this, |e| e.dispatching) {
            return Ok(());
        }
        self.ev_mut(this, |e| {
            e.initialized = true;
            e.stop_propagation = false;
            e.stop_immediate = false;
            e.canceled = false;
            e.is_trusted = false;
            e.target = None;
            e.type_ = type_.to_owned();
            e.bubbles = bubbles;
            e.cancelable = cancelable;
            e.ext = EventExt::Storage(StorageFields {
                key: key.map(str::to_owned),
                old_value: old_value.map(str::to_owned),
                new_value: new_value.map(str::to_owned),
                url: url.to_owned(),
                storage_area,
            });
        });
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: g::StorageEventInit,
    ) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::StorageEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Storage(StorageFields {
                key: init.key,
                old_value: init.old_value,
                new_value: init.new_value,
                url: init.url,
                storage_area: init.storage_area,
            }),
        ))
    }
}
