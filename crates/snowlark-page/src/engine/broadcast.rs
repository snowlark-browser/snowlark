// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Broadcasting to other browsing contexts (HTML §9.6): `BroadcastChannel`.
//!
//! Channels of one origin and name reach each other across the realms of every event loop that shares
//! a [`BroadcastHub`]: a page's frames and workers, and every tab of a browser window when the
//! embedder gives them one hub. Each event loop has an inbox in the hub; `postMessage` serializes
//! once and puts the bytes in the inbox of each event loop with a destination, which
//! [`Engine::poll_broadcasts`] turns into tasks.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use snowlark_bindings::generated::BroadcastChannelImpl;
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsValue};
use snowlark_bindings::InterfaceId;

use super::events::EventExt;
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// A message on its way to one channel: the destination's id, the serialized message, the
/// source's origin.
pub type Delivery = (u64, Arc<Vec<u8>>, String);

/// One event loop's inbox, and how to wake its thread.
pub struct Inbox {
    queue: Mutex<VecDeque<Delivery>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Inbox {
    /// An inbox whose thread `wake` wakes: an event loop's, or the browser's end of a renderer's
    /// channels, which forwards what arrives over the renderer's pipe.
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Inbox {
        Inbox {
            queue: Mutex::new(VecDeque::new()),
            wake,
        }
    }

    /// Takes everything queued, in order: the channel's id, the message, the source's origin.
    pub fn take(&self) -> Vec<Delivery> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// Queues `data` from `origin` for the channel `id`. The caller wakes the thread with
    /// [`Inbox::wake`], once for all it delivers.
    pub fn deliver(&self, id: u64, data: Arc<Vec<u8>>, origin: String) {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back((id, data, origin));
    }

    /// Wakes the event loop's thread.
    pub fn wake(&self) {
        (self.wake)();
    }
}

/// What an event loop's channels reach each other through (`docs/process-isolation.md`, M2): a
/// [`LocalBroadcast`], or, in a renderer, the same calls as messages to the browser's hub, which
/// derives `key` itself.
pub trait BroadcastBackend: Send + Sync {
    /// Adds a channel of storage key `key` and `name`; its messages go to `inbox`. Returns its id.
    fn subscribe(&self, key: &str, name: &str, inbox: &Arc<Inbox>) -> u64;

    /// Removes the channel `id`.
    fn unsubscribe(&self, id: u64);

    /// Puts `data` in the inbox of every other channel of storage key `key` and `name`; their
    /// messages' origin is `origin`.
    fn post(&self, from: u64, key: &str, origin: &str, name: &str, data: Vec<u8>);
}

struct Subscriber {
    id: u64,
    key: String,
    name: String,
    inbox: Weak<Inbox>,
}

#[derive(Default)]
struct HubInner {
    next_id: u64,
    /// In creation order (HTML: destinations are sorted by creation).
    subscribers: Vec<Subscriber>,
}

/// The channel registry of the event loops of this process.
#[derive(Default)]
pub struct LocalBroadcast(Mutex<HubInner>);

impl BroadcastBackend for LocalBroadcast {
    fn subscribe(&self, key: &str, name: &str, inbox: &Arc<Inbox>) -> u64 {
        let mut h = self.0.lock().unwrap_or_else(|e| e.into_inner());
        h.next_id += 1;
        let id = h.next_id;
        h.subscribers.push(Subscriber {
            id,
            key: key.to_owned(),
            name: name.to_owned(),
            inbox: Arc::downgrade(inbox),
        });
        id
    }

    fn unsubscribe(&self, id: u64) {
        let mut h = self.0.lock().unwrap_or_else(|e| e.into_inner());
        h.subscribers.retain(|s| s.id != id);
    }

    fn post(&self, from: u64, key: &str, origin: &str, name: &str, data: Vec<u8>) {
        let data = Arc::new(data);
        let mut wake: Vec<Arc<Inbox>> = Vec::new();
        {
            let mut h = self.0.lock().unwrap_or_else(|e| e.into_inner());
            // (the subscribers of event loops that are gone go)
            h.subscribers.retain(|s| s.inbox.strong_count() > 0);
            for s in &h.subscribers {
                if s.id == from || s.key != key || s.name != name {
                    continue;
                }
                let Some(inbox) = s.inbox.upgrade() else {
                    continue;
                };
                inbox.deliver(s.id, data.clone(), origin.to_owned());
                if !wake.iter().any(|w| Arc::ptr_eq(w, &inbox)) {
                    wake.push(inbox);
                }
            }
        }
        for inbox in wake {
            inbox.wake();
        }
    }
}

/// What event loops share to reach each other's channels (one per browser).
#[derive(Clone)]
pub struct BroadcastHub(Arc<dyn BroadcastBackend>);

impl Default for BroadcastHub {
    fn default() -> Self {
        BroadcastHub(Arc::new(LocalBroadcast::default()))
    }
}

impl std::fmt::Debug for BroadcastHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BroadcastHub").finish_non_exhaustive()
    }
}

impl BroadcastHub {
    /// A hub of `backend`: what a renderer process makes of its pipe to the browser.
    pub fn with_backend(backend: Arc<dyn BroadcastBackend>) -> Self {
        BroadcastHub(backend)
    }

    /// The registry behind the hub (what the browser serves a renderer process from).
    pub fn backend(&self) -> &Arc<dyn BroadcastBackend> {
        &self.0
    }

    fn subscribe(&self, key: &str, name: &str, inbox: &Arc<Inbox>) -> u64 {
        self.0.subscribe(key, name, inbox)
    }

    fn unsubscribe(&self, id: u64) {
        self.0.unsubscribe(id);
    }

    fn post(&self, from: u64, key: &str, origin: &str, name: &str, data: Vec<u8>) {
        self.0.post(from, key, origin, name, data);
    }
}

/// A `BroadcastChannel`.
pub struct ChannelData {
    pub name: String,
    /// The serialization of its relevant settings object's origin (`event.origin` of its messages).
    pub origin: String,
    /// Its storage key as the hub matches channels by: the origin, or for an opaque origin a key
    /// of its own (each opaque origin is distinct).
    pub key: String,
    pub closed: bool,
    /// Its id in the hub.
    pub id: u64,
    pub global: Handle,
}

impl Engine {
    fn channel<R>(&self, h: Handle, f: impl FnOnce(&mut ChannelData) -> R) -> Option<R> {
        match &mut *self.obj_mut(h) {
            Object::BroadcastChannel(c) => Some(f(c)),
            _ => None,
        }
    }

    /// The hub this event loop's channels use, and its inbox there (made on first use).
    fn broadcast_inbox(&self) -> (BroadcastHub, Arc<Inbox>) {
        let mut b = self.broadcast.borrow_mut();
        let hub = b.0.get_or_insert_with(BroadcastHub::default).clone();
        let inbox = match &b.1 {
            Some(i) => i.clone(),
            None => {
                let waker = self.waker();
                let i = Arc::new(Inbox {
                    queue: Mutex::new(VecDeque::new()),
                    wake: Arc::new(move || waker.wake()),
                });
                b.1 = Some(i.clone());
                i
            }
        };
        (hub, inbox)
    }

    /// Uses `hub` for this event loop's channels (the embedder's, shared by its pages; a worker
    /// shares its parent's).
    pub fn set_broadcast_hub(&self, hub: BroadcastHub) {
        self.broadcast.borrow_mut().0 = Some(hub);
    }

    /// The hub of this event loop, for a worker it starts.
    pub(crate) fn broadcast_hub(&self) -> BroadcastHub {
        self.broadcast_inbox().0
    }

    /// Turns the messages in this event loop's inbox into tasks. Returns whether there were any.
    pub(crate) fn poll_broadcasts(&self, _cx: &mut Cx) -> bool {
        let inbox = self.broadcast.borrow().1.clone();
        let Some(inbox) = inbox else {
            return false;
        };
        let items = inbox.take();
        if items.is_empty() {
            return false;
        }
        for (id, data, origin) in items {
            let target = self
                .broadcasts
                .borrow()
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, h)| *h);
            if let Some(target) = target {
                self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                    e.deliver_broadcast(cx, target, &data, &origin)
                });
            }
        }
        true
    }

    /// HTML §9.6 `postMessage()` step 8's task, for one destination.
    fn deliver_broadcast(&self, cx: &mut Cx, target: Handle, bytes: &[u8], origin: &str) {
        let Some((closed, global)) = self.channel(target, |c| (c.closed, c.global)) else {
            return;
        };
        // (a closed channel, or one whose document is no longer fully active or whose worker is
        // closing, gets nothing)
        if closed || !self.broadcast_global_alive(global) {
            return;
        }
        let realm = self
            .context_of_window(global)
            .and_then(|id| self.realm_of_context(id));
        self.in_realm(cx, realm, |cx| {
            let (type_, data) = match self.deserialize_value(cx, bytes) {
                Ok(v) => ("message", Some(v)),
                Err(()) => ("messageerror", None),
            };
            let ev = self.trusted_event(
                InterfaceId::MessageEvent,
                type_,
                false,
                false,
                EventExt::Message {
                    data: data.map(super::held::Held::value),
                    origin: origin.to_owned(),
                    last_event_id: String::new(),
                    source: None,
                    ports: Vec::new(),
                },
            );
            self.dispatch(cx, ev, target, false);
        });
    }

    /// Whether a channel's global may receive messages: a Window whose document is fully active, or
    /// a worker's global scope that is not closing.
    fn broadcast_global_alive(&self, global: Handle) -> bool {
        match &*self.obj(global) {
            Object::Window => self
                .document_of_window(global)
                .is_some_and(|d| self.is_active_document(d)),
            Object::WorkerGlobal(_) => !self.worker_closing(),
            _ => false,
        }
    }

    fn close_channel(&self, h: Handle) {
        let id = self.channel(h, |c| {
            let was = c.closed;
            c.closed = true;
            (!was).then_some(c.id)
        });
        if let Some(Some(id)) = id {
            self.broadcasts.borrow_mut().retain(|(i, _)| *i != id);
            let hub = self.broadcast.borrow().0.clone();
            if let Some(hub) = hub {
                hub.unsubscribe(id);
            }
        }
    }
}

impl BroadcastChannelImpl for Engine {
    /// HTML §9.6 `new BroadcastChannel(name)`.
    fn constructor(&self, cx: &mut Cx, name: &str) -> rt::Result<Handle> {
        let global = self
            .current_global(cx)
            .ok_or_else(|| exception(ExceptionName::InvalidStateError, "No global object"))?;
        let doc = self.current_document(cx)?;
        let origin = self.origin_of_document(doc);
        let (hub, inbox) = self.broadcast_inbox();
        let key = if origin == "null" {
            format!(
                "null#{:x}#{:x}",
                Arc::as_ptr(&inbox) as usize,
                doc.to_bits()
            )
        } else {
            origin.clone()
        };
        let id = hub.subscribe(&key, name, &inbox);
        let h = self.alloc(Object::BroadcastChannel(Box::new(ChannelData {
            name: name.to_owned(),
            origin,
            key,
            closed: false,
            id,
            global,
        })));
        self.broadcasts.borrow_mut().push((id, h));
        Ok(h)
    }

    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.channel(this, |c| c.name.clone()).unwrap_or_default())
    }

    /// HTML §9.6 `postMessage(message)`.
    fn post_message(&self, cx: &mut Cx, this: Handle, message: JsValue) -> rt::Result<()> {
        let Some((closed, id, origin, key, name, global)) = self.channel(this, |c| {
            (
                c.closed,
                c.id,
                c.origin.clone(),
                c.key.clone(),
                c.name.clone(),
                c.global,
            )
        }) else {
            return Ok(());
        };
        // 1. (a channel that is not eligible for messaging posts nothing)
        if !self.broadcast_global_alive(global) {
            return Ok(());
        }
        // 2.
        if closed {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The BroadcastChannel is closed",
            ));
        }
        // 3.
        let bytes = self.serialize_to_bytes(cx, &message)?;
        // 5-8.
        let (hub, _) = self.broadcast_inbox();
        hub.post(id, &key, &origin, &name, bytes);
        Ok(())
    }

    /// HTML §9.6 `close()`.
    fn close(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.close_channel(this);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_delivers_by_origin_and_name() {
        let woke = Arc::new(Mutex::new(0));
        let w = woke.clone();
        let inbox = Arc::new(Inbox {
            queue: Mutex::new(VecDeque::new()),
            wake: Arc::new(move || *w.lock().unwrap() += 1),
        });
        let local = Arc::new(LocalBroadcast::default());
        let hub = BroadcastHub::with_backend(local.clone());
        let a = hub.subscribe("https://a.test", "x", &inbox);
        let b = hub.subscribe("https://a.test", "x", &inbox);
        let _c = hub.subscribe("https://b.test", "x", &inbox);
        let _d = hub.subscribe("https://a.test", "y", &inbox);
        hub.post(a, "https://a.test", "https://a.test", "x", vec![1]);
        let got: Vec<u64> = inbox.queue.lock().unwrap().iter().map(|d| d.0).collect();
        assert_eq!(got, [b]);
        assert_eq!(*woke.lock().unwrap(), 1);
        hub.unsubscribe(b);
        hub.post(a, "https://a.test", "https://a.test", "x", vec![2]);
        assert_eq!(inbox.queue.lock().unwrap().len(), 1);
        // An inbox whose event loop is gone is dropped from the registry.
        drop(inbox);
        hub.post(a, "https://a.test", "https://a.test", "x", vec![3]);
        assert!(local.0.lock().unwrap().subscribers.is_empty());
    }

    /// A backend that keeps what it is asked: the browser's end of a pipe.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl BroadcastBackend for Recorder {
        fn subscribe(&self, key: &str, name: &str, _: &Arc<Inbox>) -> u64 {
            self.0
                .lock()
                .unwrap()
                .push(format!("subscribe {key} {name}"));
            7
        }
        fn unsubscribe(&self, id: u64) {
            self.0.lock().unwrap().push(format!("unsubscribe {id}"));
        }
        fn post(&self, from: u64, key: &str, origin: &str, name: &str, data: Vec<u8>) {
            let line = format!("post {from} {key} {origin} {name} {data:?}");
            self.0.lock().unwrap().push(line);
        }
    }

    #[test]
    fn a_hub_does_what_its_backend_says() {
        let backend = Arc::new(Recorder::default());
        let hub = BroadcastHub::with_backend(backend.clone());
        let inbox = Arc::new(Inbox {
            queue: Mutex::new(VecDeque::new()),
            wake: Arc::new(|| {}),
        });
        let id = hub.subscribe("k", "x", &inbox);
        hub.post(id, "k", "https://a.test", "x", vec![1]);
        hub.unsubscribe(id);
        assert_eq!(
            *backend.0.lock().unwrap(),
            [
                "subscribe k x",
                "post 7 k https://a.test x [1]",
                "unsubscribe 7"
            ]
        );
    }

    /// What arrives from elsewhere is queued for its channel, and the thread woken.
    #[test]
    fn an_inbox_takes_deliveries_from_outside() {
        let woke = Arc::new(Mutex::new(0));
        let w = woke.clone();
        let inbox = Inbox {
            queue: Mutex::new(VecDeque::new()),
            wake: Arc::new(move || *w.lock().unwrap() += 1),
        };
        inbox.deliver(3, Arc::new(vec![9]), "https://a.test".to_owned());
        inbox.wake();
        let got = inbox.queue.lock().unwrap().pop_front().unwrap();
        assert_eq!((got.0, got.2.as_str()), (3, "https://a.test"));
        assert_eq!(*woke.lock().unwrap(), 1);
    }
}
