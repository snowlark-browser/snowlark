// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Channel messaging (HTML §9.4): `MessageChannel` and `MessagePort` for ports entangled within one
//! event loop. Each port has a port message queue, disabled until `start()` (or the first set of
//! `onmessage`); a message waits there as its serialized form and is delivered by a task on the posted
//! message task source.
//!
//! Transferring ports (HTML §2.7.4 "transferable objects"): a `MessagePort` in a transfer list of
//! `MessagePort.postMessage()` or `window.postMessage()` is detached, and a new port entangled with
//! its partner, holding its waiting messages, arrives with the message (`event.ports`). Within one
//! event loop only: a worker's `postMessage()` cannot transfer ports.

use std::collections::VecDeque;

use snowlark_bindings::generated::{
    MessageChannelImpl, MessagePortImpl, StructuredSerializeOptions,
};
use snowlark_bindings::rt::{self, Cx, ExceptionName, Handle, JsObject, JsValue};
use snowlark_bindings::InterfaceId;

use super::events::EventExt;
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

/// A `MessagePort`.
pub struct PortData {
    /// The port this one is entangled with.
    pub entangled: Option<Handle>,
    /// Whether the port message queue is enabled.
    pub enabled: bool,
    /// The port message queue: serialized messages (and the ports transferred with them) not yet
    /// handed to the event loop.
    pub queue: VecDeque<(Vec<u8>, Vec<Handle>)>,
    /// The port was transferred: this object is detached (HTML [[Detached]]).
    pub detached: bool,
    /// The port's relevant global object.
    pub global: Handle,
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
            self.port(new, |p| {
                p.entangled = partner;
                p.queue = queue;
            });
            if let Some(partner) = partner {
                self.port(partner, |p| p.entangled = Some(new));
            }
        }
        Ok((bytes, map.into_iter().map(|(_, new)| new).collect()))
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
        let Some((Some((bytes, ports)), global)) =
            self.port(port, |p| (p.queue.pop_front(), p.global))
        else {
            return;
        };
        let realm = self
            .context_of_window(global)
            .and_then(|id| self.realm_of_context(id));
        self.in_realm(cx, realm, |cx| {
            for &p in &ports {
                self.receive_port(cx, p);
            }
            let (type_, data) = match self.deserialize_value(cx, &bytes) {
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
        let target = self.port(source, |p| p.entangled).flatten();
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
            if h.is_some() && h == target {
                doomed = true;
                self.console_print(
                    "warn",
                    "MessagePort.postMessage(): transferring the target port drops the message",
                );
            }
        }
        // 6.
        let message = self.serialize_with_ports(cx, message, transfer)?;
        // 7.
        let Some(target) = target.filter(|_| !doomed) else {
            return Ok(());
        };
        // 8.
        let enabled = self
            .port(target, |p| {
                p.queue.push_back(message);
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
        self.port(port1, |p| p.entangled = Some(port2));
        self.port(port2, |p| p.entangled = Some(port1));
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
        if let Some(Some(other)) = self.port(this, |p| p.entangled.take()) {
            self.port(other, |p| p.entangled = None);
        }
        Ok(())
    }
}
