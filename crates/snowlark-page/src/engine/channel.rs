// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Channel messaging (HTML §9.4): `MessageChannel` and `MessagePort`. Each port has a port message
//! queue, disabled until `start()` (or the first set of `onmessage`); a message waits there as its
//! serialized form and is delivered by a task on the posted message task source.
//!
//! Transferring ports (HTML §2.7.4 "transferable objects"): a `MessagePort` in a transfer list of
//! `MessagePort.postMessage()`, `window.postMessage()` or a worker's `postMessage()` is detached, and
//! a new port entangled with its partner, holding its waiting messages, arrives with the message
//! (`event.ports`). Two ports of one event loop are entangled by their handles; once one of them goes
//! to another event loop (a worker's), each reads a [`Mailbox`] the other posts to, and wakes the
//! other's event loop, which takes what arrived each turn ([`Engine::poll_ports`]).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use snowlark_bindings::generated::{
    MessageChannelImpl, MessagePortImpl, StructuredSerializeOptions,
};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsObject, JsValue};
use snowlark_bindings::InterfaceId;

use super::events::EventExt;
use super::offscreen::Transferred;
use super::{exception, Engine, Object};
use crate::event_loop::{TaskSource, Waker};

/// A `MessagePort`.
pub struct PortData {
    /// The port this one is entangled with.
    pub entangled: Option<Peer>,
    /// Whether the port message queue is enabled.
    pub enabled: bool,
    /// The port message queue: serialized messages (and the ports transferred with them) not yet
    /// handed to the event loop.
    pub queue: VecDeque<Queued>,
    /// The port was transferred: this object is detached (HTML [[Detached]]).
    pub detached: bool,
    /// The port's relevant global object.
    pub global: Handle,
}

/// The port a port is entangled with.
pub enum Peer {
    /// In this event loop.
    Local(Handle),
    /// In another event loop.
    Remote(PortEnd),
}

/// A message in a port message queue.
pub enum Queued {
    /// Posted in this event loop: the ports it transferred are already this loop's.
    Local(Vec<u8>, Vec<Handle>),
    /// From another event loop: the objects it transferred, received when it is delivered.
    Remote(Vec<u8>, Vec<Transferred>),
}

/// What one of two ports in different event loops posts to the other: the messages not taken yet,
/// and the reading port's event loop to wake (none while that port is on its way to another).
#[derive(Default)]
pub struct Mailbox {
    inner: Mutex<MailboxInner>,
}

#[derive(Default)]
struct MailboxInner {
    messages: VecDeque<PortMsg>,
    waker: Option<Arc<Waker>>,
}

/// What a port posts to a partner in another event loop.
pub enum PortMsg {
    Message(Vec<u8>, Vec<Transferred>),
    /// The port was closed: the partner is disentangled.
    Closed,
}

impl Mailbox {
    fn post(&self, m: PortMsg) {
        let waker = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.messages.push_back(m);
            i.waker.clone()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }

    fn take(&self) -> VecDeque<PortMsg> {
        std::mem::take(
            &mut self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .messages,
        )
    }

    fn set_waker(&self, waker: Option<Arc<Waker>>) {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let wake = waker.clone().filter(|_| !i.messages.is_empty());
        i.waker = waker;
        drop(i);
        if let Some(w) = wake {
            w.wake();
        }
    }
}

/// HTML StructuredSerializeWithTransfer step 4.1: an object in the transfer list that is neither
/// an ArrayBuffer nor a platform object (none of which has a [[Detached]] slot) is a
/// DataCloneError, before anything is transferred.
pub(crate) fn check_transferable(cx: &mut Cx, transfer: &[JsObject]) -> rt::Result<()> {
    let scope = cx.scope();
    for t in transfer {
        let o = v8::Local::new(scope, t.global());
        if o.is_array_buffer() || rt::unwrap(&o, 1, u16::MAX).is_some() {
            continue;
        }
        return Err(exception(
            ExceptionName::DataCloneError,
            "An object in the transfer list is not transferable",
        ));
    }
    Ok(())
}

/// A port's side of an entanglement across event loops: what it reads, what its partner reads.
pub struct PortEnd {
    pub inbox: Arc<Mailbox>,
    pub outbox: Arc<Mailbox>,
}

/// A port on its way to another event loop (its transfer data, HTML MessagePort's transfer steps):
/// the key its message's serialization names it by, its end of the entanglement and the messages
/// waiting in its port message queue.
pub struct PortTransfer {
    pub key: u64,
    pub end: PortEnd,
    pub queue: Vec<(Vec<u8>, Vec<Transferred>)>,
}

impl Engine {
    fn port<R>(&self, h: Handle, f: impl FnOnce(&mut PortData) -> R) -> Option<R> {
        match &mut *self.obj_mut(h) {
            Object::MessagePort(p) => Some(f(p)),
            _ => None,
        }
    }

    fn new_port(&self, global: Handle) -> Handle {
        self.alloc(Object::MessagePort(Box::new(PortData {
            entangled: None,
            enabled: false,
            queue: VecDeque::new(),
            detached: false,
            global,
        })))
    }

    /// HTML StructuredSerializeWithTransfer for the ports in `transfer` (other transferables are
    /// not transferred): the serialization, and the new ports that the deserialization gives the
    /// receiving realm. The transferred ports are detached; their partners are entangled with the
    /// new ports, which take over their queues.
    pub(crate) fn serialize_with_ports(
        &self,
        cx: &mut Cx,
        message: &JsValue,
        transfer: &[JsObject],
    ) -> rt::Result<(Vec<u8>, Vec<Handle>)> {
        check_transferable(cx, transfer)?;
        let mut ports: Vec<Handle> = Vec::new();
        // (ArrayBuffers in the transfer list: serialized by value, then detached)
        let mut buffers: Vec<v8::Global<v8::ArrayBuffer>> = Vec::new();
        for t in transfer {
            let h = {
                let scope = cx.scope();
                let o = v8::Local::new(scope, t.global());
                if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(o) {
                    let dup = buffers.iter().any(|b| v8::Local::new(scope, b) == ab);
                    if dup || ab.was_detached() || !ab.is_detachable() {
                        return Err(exception(
                            ExceptionName::DataCloneError,
                            "An ArrayBuffer in the transfer list cannot be transferred",
                        ));
                    }
                    buffers.push(v8::Global::new(scope, ab));
                    continue;
                }
                rt::unwrap(
                    &o,
                    snowlark_bindings::generated::ids::MESSAGE_PORT,
                    snowlark_bindings::generated::ids::MESSAGE_PORT_END,
                )
            };
            let Some(h) = h else { continue };
            if ports.contains(&h) {
                return Err(exception(
                    ExceptionName::DataCloneError,
                    "A port appears twice in the transfer list",
                ));
            }
            if self.port(h, |p| p.detached).unwrap_or(true) {
                return Err(exception(
                    ExceptionName::DataCloneError,
                    "A transferred port cannot be transferred again",
                ));
            }
            ports.push(h);
        }
        let bitmaps = self.bitmap_transfer_list(cx, transfer)?;
        let map: Vec<(Handle, Handle)> = ports
            .iter()
            .map(|&old| {
                let global = self.port(old, |p| p.global).unwrap_or(old);
                (old, self.new_port(global))
            })
            .collect();
        *self.transferring.borrow_mut() = map.clone();
        let bytes = self.serialize_to_bytes(cx, message);
        self.transferring.borrow_mut().clear();
        let bytes = bytes?;
        // (the ImageBitmaps are checked and detached first: one that cannot be transferred
        // throws before anything is detached)
        self.transfer_bitmaps(&bitmaps)?;
        {
            let scope = cx.scope();
            for b in &buffers {
                v8::Local::new(scope, b).detach(None);
            }
        }
        for &(old, new) in &map {
            let (partner, queue) = self
                .port(old, |p| {
                    p.detached = true;
                    p.enabled = false;
                    (p.entangled.take(), std::mem::take(&mut p.queue))
                })
                .unwrap_or_default();
            match &partner {
                Some(Peer::Local(partner)) => {
                    self.port(*partner, |p| p.entangled = Some(Peer::Local(new)));
                }
                Some(Peer::Remote(_)) => {
                    for h in self.remote_ports.borrow_mut().iter_mut() {
                        if *h == old {
                            *h = new;
                        }
                    }
                }
                None => {}
            }
            self.port(new, |p| {
                p.entangled = partner;
                p.queue = queue;
            });
        }
        Ok((bytes, map.into_iter().map(|(_, new)| new).collect()))
    }

    /// The `MessagePort`s of a transfer list for another event loop, checked as
    /// StructuredSerializeWithTransfer checks them (each once, none detached).
    pub(crate) fn port_transfer_list(
        &self,
        cx: &mut Cx,
        transfer: &[JsObject],
    ) -> rt::Result<Vec<Handle>> {
        let mut ports = Vec::new();
        for t in transfer {
            let h = {
                let scope = cx.scope();
                let o = v8::Local::new(scope, t.global());
                rt::unwrap(
                    &o,
                    snowlark_bindings::generated::ids::MESSAGE_PORT,
                    snowlark_bindings::generated::ids::MESSAGE_PORT_END,
                )
            };
            let Some(h) = h else { continue };
            if ports.contains(&h) {
                return Err(exception(
                    ExceptionName::DataCloneError,
                    "A port appears twice in the transfer list",
                ));
            }
            if self.port(h, |p| p.detached).unwrap_or(true) {
                return Err(exception(
                    ExceptionName::DataCloneError,
                    "A transferred port cannot be transferred again",
                ));
            }
            ports.push(h);
        }
        Ok(ports)
    }

    /// MessagePort's transfer steps toward another event loop: `h` is detached, and its partner,
    /// if in this event loop, is entangled with it across two mailboxes from now on. Its waiting
    /// messages go with it, the ports they carry sent the same way.
    pub(crate) fn export_port(&self, h: Handle) -> PortTransfer {
        let (peer, queue) = self
            .port(h, |p| {
                p.detached = true;
                p.enabled = false;
                (p.entangled.take(), std::mem::take(&mut p.queue))
            })
            .unwrap_or_default();
        let end = match peer {
            Some(Peer::Remote(end)) => {
                self.remote_ports.borrow_mut().retain(|&p| p != h);
                end
            }
            Some(Peer::Local(partner)) => {
                let (here, there) = (Arc::new(Mailbox::default()), Arc::new(Mailbox::default()));
                here.set_waker(Some(self.waker()));
                self.port(partner, |p| {
                    p.entangled = Some(Peer::Remote(PortEnd {
                        inbox: here.clone(),
                        outbox: there.clone(),
                    }))
                });
                self.remote_ports.borrow_mut().push(partner);
                PortEnd {
                    inbox: there,
                    outbox: here,
                }
            }
            // (disentangled: what it posts goes nowhere)
            None => PortEnd {
                inbox: Arc::default(),
                outbox: Arc::default(),
            },
        };
        end.inbox.set_waker(None);
        let queue = queue
            .into_iter()
            .map(|m| match m {
                Queued::Remote(bytes, t) => (bytes, t),
                Queued::Local(bytes, ports) => {
                    let t = ports
                        .into_iter()
                        .map(|p| Transferred::Port(Box::new(self.export_port(p))))
                        .collect();
                    (bytes, t)
                }
            })
            .collect();
        PortTransfer {
            key: h.get(),
            end,
            queue,
        }
    }

    /// The transfer-receiving steps of ports from another event loop: a new port here for each
    /// [`Transferred::Port`] in `transferred` (taken out of it), entangled with its partner across
    /// the mailboxes, its waiting messages in its queue, in the current realm.
    fn import_ports(&self, cx: &mut Cx, transferred: &mut Vec<Transferred>) -> Vec<(u64, Handle)> {
        let global = self.current_global(cx).or_else(|| self.window());
        let mut out = Vec::new();
        let mut rest = Vec::with_capacity(transferred.len());
        for t in transferred.drain(..) {
            let Transferred::Port(p) = t else {
                rest.push(t);
                continue;
            };
            let PortTransfer { key, end, queue } = *p;
            end.inbox.set_waker(Some(self.waker()));
            let Some(global) = global else { continue };
            let h = self.new_port(global);
            self.port(h, |p| {
                p.entangled = Some(Peer::Remote(end));
                p.queue = queue
                    .into_iter()
                    .map(|(b, t)| Queued::Remote(b, t))
                    .collect();
            });
            self.remote_ports.borrow_mut().push(h);
            out.push((key, h));
        }
        *transferred = rest;
        out
    }

    /// StructuredDeserializeWithTransfer of a message from another event loop: the value, and the
    /// ports it transferred (new ports here, in transfer order: the event's `ports`).
    pub(crate) fn deserialize_from_elsewhere(
        &self,
        cx: &mut Cx,
        bytes: &[u8],
        mut transferred: Vec<Transferred>,
    ) -> (Result<JsValue, ()>, Vec<Handle>) {
        let ports = self.import_ports(cx, &mut transferred);
        let handles = ports.iter().map(|&(_, h)| h).collect();
        let prev = self.ports_in.replace(Some(ports));
        let r = self.deserialize_with_transfer(cx, bytes, transferred);
        self.ports_in.replace(prev);
        (r, handles)
    }

    /// The port a deserialization from another event loop names by `key`: `None` when the bytes
    /// being read are this event loop's own (their ports are named by handle), `Some(None)` for a
    /// key that message did not transfer.
    pub(crate) fn port_for_key(&self, key: u64) -> Option<Option<Handle>> {
        self.ports_in
            .borrow()
            .as_ref()
            .map(|ports| ports.iter().find(|(k, _)| *k == key).map(|&(_, h)| h))
    }

    /// Takes what the partners in other event loops posted to this loop's ports: messages go to
    /// the port message queues (a task each while enabled), a close disentangles. Returns whether
    /// anything arrived.
    pub(crate) fn poll_ports(&self) -> bool {
        let ports = self.remote_ports.borrow().clone();
        let mut progressed = false;
        for h in ports {
            let inbox = self
                .port(h, |p| match &p.entangled {
                    Some(Peer::Remote(end)) => Some(end.inbox.clone()),
                    _ => None,
                })
                .flatten();
            let Some(inbox) = inbox else {
                self.remote_ports.borrow_mut().retain(|&p| p != h);
                continue;
            };
            for m in inbox.take() {
                progressed = true;
                match m {
                    PortMsg::Message(bytes, t) => {
                        let enabled = self
                            .port(h, |p| {
                                p.queue.push_back(Queued::Remote(bytes, t));
                                p.enabled
                            })
                            .unwrap_or(false);
                        if enabled {
                            self.queue_task(TaskSource::PostedMessage, move |e, cx| {
                                e.deliver_port_message(cx, h);
                            });
                        }
                    }
                    PortMsg::Closed => {
                        self.port(h, |p| p.entangled = None);
                        self.remote_ports.borrow_mut().retain(|&p| p != h);
                    }
                }
            }
        }
        progressed
    }

    /// The port a transferred port became, as the serialization writes it.
    pub(crate) fn transferred_port(&self, old: Handle) -> Option<Handle> {
        self.transferring
            .borrow()
            .iter()
            .find(|(o, _)| *o == old)
            .map(|(_, n)| *n)
    }

    /// A transferred port arriving in the current realm (its deserialization): the realm's global
    /// is its relevant global object.
    pub(crate) fn receive_port(&self, cx: &mut Cx, h: Handle) -> bool {
        let global = self.current_global(cx);
        self.port(h, |p| {
            if let Some(g) = global {
                p.global = g;
            }
        })
        .is_some()
    }

    /// Hands the messages waiting in an enabled port's queue to the event loop, one task each.
    fn flush_port_queue(&self, port: Handle) {
        let n = self
            .port(port, |p| if p.enabled { p.queue.len() } else { 0 })
            .unwrap_or(0);
        for _ in 0..n {
            self.queue_task(TaskSource::PostedMessage, move |e, cx| {
                e.deliver_port_message(cx, port);
            });
        }
    }

    /// The task a posted message runs (HTML §9.4.4 "message port post message steps" step 8).
    fn deliver_port_message(&self, cx: &mut Cx, port: Handle) {
        let Some((Some(message), global)) = self.port(port, |p| (p.queue.pop_front(), p.global))
        else {
            return;
        };
        let realm = self
            .context_of_window(global)
            .and_then(|id| self.realm_of_context(id));
        self.in_realm(cx, realm, |cx| {
            let (data, ports) = match message {
                Queued::Local(bytes, ports) => {
                    for &p in &ports {
                        self.receive_port(cx, p);
                    }
                    (self.deserialize_value(cx, &bytes), ports)
                }
                Queued::Remote(bytes, t) => self.deserialize_from_elsewhere(cx, &bytes, t),
            };
            let (type_, data) = match data {
                Ok(v) => ("message", Some(v)),
                Err(()) => ("messageerror", None),
            };
            let ev = self.trusted_event(
                InterfaceId::MessageEvent,
                type_,
                false,
                false,
                EventExt::Message {
                    data: data.map(crate::engine::held::Held::value),
                    origin: String::new(),
                    last_event_id: String::new(),
                    source: None,
                    ports,
                },
            );
            self.dispatch(cx, ev, port, false);
        });
    }

    /// HTML §9.4.4: the first set of a port's `onmessage` enables its port message queue.
    pub(crate) fn port_onmessage_set(&self, target: Handle) {
        if super::handle_node(target).is_none()
            && matches!(&*self.obj(target), Object::MessagePort(_))
        {
            self.start_port(target);
        }
    }

    fn start_port(&self, port: Handle) {
        let was = self
            .port(port, |p| std::mem::replace(&mut p.enabled, true))
            .unwrap_or(true);
        if !was {
            self.flush_port_queue(port);
        }
    }

    /// HTML §9.4.4 "message port post message steps".
    fn port_post_message(
        &self,
        cx: &mut Cx,
        source: Handle,
        message: &JsValue,
        transfer: &[JsObject],
    ) -> rt::Result<()> {
        // 1.
        enum Target {
            Local(Handle),
            Remote(Arc<Mailbox>),
        }
        let target = self
            .port(source, |p| match &p.entangled {
                Some(Peer::Local(h)) => Some(Target::Local(*h)),
                Some(Peer::Remote(end)) => Some(Target::Remote(end.outbox.clone())),
                None => None,
            })
            .flatten();
        let local = match &target {
            Some(Target::Local(h)) => Some(*h),
            _ => None,
        };
        // 3-5.
        let mut doomed = false;
        for t in transfer {
            let h = {
                let scope = cx.scope();
                let o = v8::Local::new(scope, t.global());
                rt::unwrap(&o, 1, u16::MAX)
            };
            if h == Some(source) {
                return Err(exception(
                    ExceptionName::DataCloneError,
                    "A port cannot transfer itself",
                ));
            }
            if h.is_some() && h == local {
                doomed = true;
                self.console_print(
                    "warn",
                    "MessagePort.postMessage(): transferring the target port drops the message",
                );
            }
        }
        // 6. (for a partner in another event loop, serialized for it)
        if let Some(Target::Remote(outbox)) = &target {
            let (bytes, t) = self.serialize_with_transfer(cx, message, transfer)?;
            outbox.post(PortMsg::Message(bytes, t));
            return Ok(());
        }
        let (bytes, ports) = self.serialize_with_ports(cx, message, transfer)?;
        // 7.
        let Some(target) = local.filter(|_| !doomed) else {
            return Ok(());
        };
        // 8.
        let enabled = self
            .port(target, |p| {
                p.queue.push_back(Queued::Local(bytes, ports));
                p.enabled
            })
            .unwrap_or(false);
        if enabled {
            self.queue_task(TaskSource::PostedMessage, move |e, cx| {
                e.deliver_port_message(cx, target);
            });
        }
        Ok(())
    }
}

impl MessageChannelImpl for Engine {
    fn port1(&self, this: Handle) -> Handle {
        match &*self.obj(this) {
            Object::MessageChannel { port1, .. } => *port1,
            _ => panic!("not a MessageChannel"),
        }
    }
    fn port2(&self, this: Handle) -> Handle {
        match &*self.obj(this) {
            Object::MessageChannel { port2, .. } => *port2,
            _ => panic!("not a MessageChannel"),
        }
    }
    /// HTML §9.4.3 `new MessageChannel()`: two new ports, entangled.
    fn constructor(&self, cx: &mut Cx) -> rt::Result<Handle> {
        let global = self
            .current_global(cx)
            .ok_or_else(|| exception(ExceptionName::InvalidStateError, "No global object"))?;
        let port1 = self.new_port(global);
        let port2 = self.new_port(global);
        self.port(port1, |p| p.entangled = Some(Peer::Local(port2)));
        self.port(port2, |p| p.entangled = Some(Peer::Local(port1)));
        Ok(self.alloc(Object::MessageChannel { port1, port2 }))
    }
}

impl MessagePortImpl for Engine {
    fn post_message_message_transfer(
        &self,
        cx: &mut Cx,
        this: Handle,
        message: JsValue,
        transfer: Vec<JsObject>,
    ) -> rt::Result<()> {
        self.port_post_message(cx, this, &message, &transfer)
    }
    fn post_message_message_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        message: JsValue,
        options: StructuredSerializeOptions,
    ) -> rt::Result<()> {
        self.port_post_message(cx, this, &message, &options.transfer)
    }
    /// HTML §9.4.4 `start()`: enables the port message queue.
    fn start(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.start_port(this);
        Ok(())
    }
    /// HTML §9.4.4 `close()`: disentangles the port.
    fn close(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        match self.port(this, |p| p.entangled.take()).flatten() {
            Some(Peer::Local(other)) => {
                self.port(other, |p| p.entangled = None);
            }
            Some(Peer::Remote(end)) => {
                end.outbox.post(PortMsg::Closed);
                self.remote_ports.borrow_mut().retain(|&p| p != this);
            }
            None => {}
        }
        Ok(())
    }
}
