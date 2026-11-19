// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WebSocket interface (WebSockets Standard §3) over snowlark-net's connections, and `CloseEvent`.
//!
//! The connection runs on the network thread ([`snowlark_net::websocket`]); its events are taken by
//! [`Engine::poll_websockets`] and each becomes a task on the WebSocket task source (here the
//! networking task source), which changes the ready state and fires the events.

use snowlark_bindings::generated::{
    ArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataViewOrBlobOrString as SendData,
    BinaryType, CloseEventImpl, CloseEventInit, StringOrStringSequence, WebSocketImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsValue};
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;
use snowlark_net::websocket::{WebSocketConnection, WsCommand, WsEvent, WsParams};

use super::blob::array_buffer;
use super::encoding::buffer_bytes;
use super::events::EventExt;
use super::{exception, Engine, Object};
use crate::event_loop::TaskSource;

pub const CONNECTING: u16 = 0;
pub const OPEN: u16 = 1;
pub const CLOSING: u16 = 2;
pub const CLOSED: u16 = 3;

/// A `WebSocket`.
pub struct WsData {
    pub url: String,
    pub ready_state: u16,
    pub buffered_amount: u64,
    pub protocol: String,
    pub extensions: String,
    pub binary_type: BinaryType,
    /// The connection, until it closes.
    pub conn: Option<WebSocketConnection>,
    /// The closing handshake was started by `close()`.
    pub close_started: bool,
    /// The relevant global object and the document whose policy container the connection used.
    pub global: Handle,
    pub doc: NodeId,
}

/// RFC 7230 §3.2.6 `token`: what a subprotocol name may be (RFC 6455 §4.1).
fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && !b"()<>@,;:\\\"/[]?={}".contains(&b))
}

impl Engine {
    fn socket<R>(&self, h: Handle, f: impl FnOnce(&mut WsData) -> R) -> Option<R> {
        match &mut *self.obj_mut(h) {
            Object::WebSocket(w) => Some(f(w)),
            _ => None,
        }
    }

    /// Whether a WebSocket is connecting or closing (an open one waits for its peer, which is not
    /// work the page has pending).
    pub(crate) fn websockets_pending(&self) -> bool {
        self.websockets
            .borrow()
            .iter()
            .any(|&h| self.socket(h, |w| w.ready_state != OPEN).unwrap_or(false))
    }

    /// Takes the network's events for every open WebSocket and queues their tasks. Returns whether
    /// there were any.
    pub(crate) fn poll_websockets(&self, cx: &mut Cx) -> bool {
        let sockets: Vec<Handle> = self.websockets.borrow().clone();
        let mut progressed = false;
        for h in sockets {
            while let Some(Some(event)) =
                self.socket(h, |w| w.conn.as_mut().and_then(|c| c.try_next()))
            {
                progressed = true;
                match event {
                    WsEvent::Violation(v) => {
                        let doc = self.socket(h, |w| w.doc);
                        if let Some(doc) = doc.filter(|d| self.dom().is_alive(*d)) {
                            self.report_violation(cx, doc, None, &v, None);
                        }
                    }
                    WsEvent::Closed {
                        failed,
                        clean,
                        code,
                        reason,
                    } => {
                        self.socket(h, |w| w.conn = None);
                        self.websockets.borrow_mut().retain(|&s| s != h);
                        self.queue_task(TaskSource::Networking, move |e, cx| {
                            e.ws_closed(cx, h, failed, clean, code, &reason)
                        });
                        break;
                    }
                    event => self.queue_task(TaskSource::Networking, move |e, cx| {
                        e.ws_event(cx, h, event)
                    }),
                }
            }
        }
        progressed
    }

    fn ws_realm(&self, global: Handle) -> Option<v8::Global<v8::Context>> {
        self.context_of_window(global)
            .and_then(|id| self.realm_of_context(id))
    }

    /// The tasks of an open connection's events.
    fn ws_event(&self, cx: &mut Cx, h: Handle, event: WsEvent) {
        let Some((global, state)) = self.socket(h, |w| (w.global, w.ready_state)) else {
            return;
        };
        let realm = self.ws_realm(global);
        self.in_realm(cx, realm, |cx| match event {
            // "When the WebSocket connection is established"
            WsEvent::Open {
                protocol,
                extensions,
            } => {
                if state != CONNECTING {
                    return;
                }
                self.socket(h, |w| {
                    w.ready_state = OPEN;
                    w.protocol = protocol;
                    w.extensions = extensions;
                });
                self.fire_event(cx, h, "open", false, false);
            }
            // "When a WebSocket message has been received"
            WsEvent::Text(_) | WsEvent::Binary(_) => {
                if state != OPEN {
                    return;
                }
                let Some((url, binary_type)) = self.socket(h, |w| (w.url.clone(), w.binary_type))
                else {
                    return;
                };
                let data = match event {
                    WsEvent::Text(s) => {
                        let scope = cx.scope();
                        let Some(v) = v8::String::new(scope, &s) else {
                            return;
                        };
                        JsValue::new(scope, v.into())
                    }
                    WsEvent::Binary(bytes) => match binary_type {
                        BinaryType::Blob => {
                            let b = self.new_blob(bytes, "");
                            let scope = cx.scope();
                            let Some(o) = rt::wrap(scope, b) else {
                                return;
                            };
                            JsValue::new(scope, o.into())
                        }
                        BinaryType::Arraybuffer => {
                            let ab = array_buffer(cx, bytes);
                            let scope = cx.scope();
                            let o = v8::Local::new(scope, ab.global());
                            JsValue::new(scope, o.into())
                        }
                    },
                    _ => return,
                };
                let origin = url::Url::parse(&url)
                    .map(|u| u.origin().ascii_serialization())
                    .unwrap_or_default();
                let ev = self.trusted_event(
                    InterfaceId::MessageEvent,
                    "message",
                    false,
                    false,
                    EventExt::Message {
                        data: Some(super::held::Held::value(data)),
                        origin,
                        last_event_id: String::new(),
                        source: None,
                        ports: Vec::new(),
                    },
                );
                self.dispatch(cx, ev, h, false);
            }
            WsEvent::Sent(n) => {
                self.socket(h, |w| {
                    w.buffered_amount = w.buffered_amount.saturating_sub(n as u64)
                });
            }
            // "When the WebSocket closing handshake is started"
            WsEvent::Closing => {
                self.socket(h, |w| {
                    if w.ready_state != CLOSED {
                        w.ready_state = CLOSING
                    }
                });
            }
            WsEvent::Violation(_) | WsEvent::Closed { .. } => {}
        });
    }

    /// "When the WebSocket connection is closed, possibly cleanly".
    fn ws_closed(
        &self,
        cx: &mut Cx,
        h: Handle,
        failed: bool,
        clean: bool,
        code: u16,
        reason: &str,
    ) {
        let Some(global) = self.socket(h, |w| {
            w.ready_state = CLOSED;
            w.global
        }) else {
            return;
        };
        let realm = self.ws_realm(global);
        self.in_realm(cx, realm, |cx| {
            if failed {
                self.fire_event(cx, h, "error", false, false);
            }
            let ev = self.trusted_event(
                InterfaceId::CloseEvent,
                "close",
                false,
                false,
                EventExt::Close {
                    was_clean: clean,
                    code,
                    reason: reason.to_owned(),
                },
            );
            self.dispatch(cx, ev, h, false);
        });
    }

    fn ws_send(&self, h: Handle, bytes: usize, command: WsCommand) -> rt::Result<()> {
        let state = self.socket(h, |w| w.ready_state).unwrap_or(CLOSED);
        if state == CONNECTING {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "The WebSocket is still connecting",
            ));
        }
        self.socket(h, |w| {
            // (data sent once the closing handshake started is counted, not sent)
            w.buffered_amount += bytes as u64;
            if w.ready_state == OPEN && !w.close_started {
                if let Some(c) = &w.conn {
                    c.send(command);
                }
            }
        });
        Ok(())
    }
}

impl WebSocketImpl for Engine {
    /// §3.1 `new WebSocket(url, protocols)`.
    fn constructor(
        &self,
        cx: &mut Cx,
        url: &str,
        protocols: StringOrStringSequence,
    ) -> rt::Result<Handle> {
        // 1-3.
        let base = self.current_base_url(cx)?;
        let Ok(mut record) = base.join(url) else {
            return Err(exception(
                ExceptionName::SyntaxError,
                "The URL is not valid",
            ));
        };
        // 4-5.
        match record.scheme() {
            "http" => {
                let _ = record.set_scheme("ws");
            }
            "https" => {
                let _ = record.set_scheme("wss");
            }
            _ => {}
        }
        // 6.
        if !matches!(record.scheme(), "ws" | "wss") {
            return Err(exception(
                ExceptionName::SyntaxError,
                "A WebSocket URL's scheme must be ws or wss",
            ));
        }
        // 7.
        if record.fragment().is_some() {
            return Err(exception(
                ExceptionName::SyntaxError,
                "A WebSocket URL has no fragment",
            ));
        }
        // 8-9.
        let protocols = match protocols {
            StringOrStringSequence::String(s) => vec![s],
            StringOrStringSequence::StringSequence(v) => v,
        };
        for (i, p) in protocols.iter().enumerate() {
            // (repeats are compared ASCII case-insensitively, as subprotocol names are matched)
            if !is_token(p) || protocols[..i].iter().any(|q| q.eq_ignore_ascii_case(p)) {
                return Err(exception(
                    ExceptionName::SyntaxError,
                    "The subprotocols must be distinct tokens",
                ));
            }
        }
        // 10-12.
        let global = self
            .current_global(cx)
            .ok_or_else(|| exception(ExceptionName::InvalidStateError, "No global object"))?;
        let doc = self.current_document(cx)?;
        let h = self.alloc(Object::WebSocket(Box::new(WsData {
            url: record.to_string(),
            ready_state: CONNECTING,
            buffered_amount: 0,
            protocol: String::new(),
            extensions: String::new(),
            binary_type: BinaryType::Blob,
            conn: None,
            close_started: false,
            global,
            doc,
        })));
        let net = self.hooks.borrow().net.clone();
        match net {
            Some(net) => {
                let client = self.request_client(doc);
                let waker = self.waker();
                let conn = net.websocket(
                    WsParams {
                        url: record,
                        protocols,
                        client,
                    },
                    std::sync::Arc::new(move || waker.wake()),
                );
                self.socket(h, |w| w.conn = Some(conn));
                self.websockets.borrow_mut().push(h);
            }
            // (no network: the connection fails)
            None => self.queue_task(TaskSource::Networking, move |e, cx| {
                e.ws_closed(cx, h, true, false, 1006, "")
            }),
        }
        Ok(h)
    }

    fn url(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.socket(this, |w| w.url.clone()).unwrap_or_default())
    }

    fn ready_state(&self, this: Handle) -> u16 {
        self.socket(this, |w| w.ready_state).unwrap_or(CLOSED)
    }

    fn buffered_amount(&self, this: Handle) -> u64 {
        self.socket(this, |w| w.buffered_amount).unwrap_or(0)
    }

    fn extensions(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.socket(this, |w| w.extensions.clone())
                .unwrap_or_default(),
        )
    }

    fn protocol(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.socket(this, |w| w.protocol.clone())
                .unwrap_or_default(),
        )
    }

    fn binary_type(&self, this: Handle) -> BinaryType {
        self.socket(this, |w| w.binary_type)
            .unwrap_or(BinaryType::Blob)
    }

    fn set_binary_type(&self, _cx: &mut Cx, this: Handle, value: BinaryType) -> rt::Result<()> {
        self.socket(this, |w| w.binary_type = value);
        Ok(())
    }

    /// §3.2 `close(code, reason)`.
    fn close(
        &self,
        cx: &mut Cx,
        this: Handle,
        code: Option<u16>,
        reason: Option<&str>,
    ) -> rt::Result<()> {
        // 1.
        if let Some(c) = code {
            if c != 1000 && !(3000..=4999).contains(&c) {
                return Err(exception(
                    ExceptionName::InvalidAccessError,
                    "The close code must be 1000 or in 3000-4999",
                ));
            }
        }
        // 2.
        if reason.is_some_and(|r| r.len() > 123) {
            return Err(exception(
                ExceptionName::SyntaxError,
                "The close reason is longer than 123 bytes",
            ));
        }
        // 3.
        let state = self.socket(this, |w| w.ready_state).unwrap_or(CLOSED);
        match state {
            CLOSING | CLOSED => {}
            CONNECTING => {
                // "Fail the WebSocket connection" and set the ready state to CLOSING.
                let had = self.socket(this, |w| {
                    w.ready_state = CLOSING;
                    w.conn.take().is_some()
                });
                self.websockets.borrow_mut().retain(|&s| s != this);
                if had == Some(true) {
                    self.queue_task(TaskSource::Networking, move |e, cx| {
                        e.ws_closed(cx, this, true, false, 1006, "")
                    });
                }
            }
            _ => {
                let started = self.socket(this, |w| {
                    let was = w.close_started;
                    w.close_started = true;
                    w.ready_state = CLOSING;
                    was
                });
                if started == Some(false) {
                    // (a reason without a code goes with 1000)
                    let code = code.or(reason.map(|_| 1000));
                    let reason = reason.unwrap_or("").to_owned();
                    self.socket(this, |w| {
                        if let Some(c) = &w.conn {
                            c.send(WsCommand::Close { code, reason });
                        }
                    });
                }
            }
        }
        let _ = cx;
        Ok(())
    }

    /// §3.3 `send(data)`.
    fn send(&self, cx: &mut Cx, this: Handle, data: SendData) -> rt::Result<()> {
        use SendData as D;
        match data {
            D::String(s) => {
                let n = s.len();
                self.ws_send(this, n, WsCommand::Text(s))
            }
            D::Blob(b) => {
                let (bytes, _) = self.blob_contents(b);
                let bytes = bytes.to_vec();
                self.ws_send(this, bytes.len(), WsCommand::Binary(bytes))
            }
            D::ArrayBuffer(o)
            | D::Int8Array(o)
            | D::Int16Array(o)
            | D::Int32Array(o)
            | D::Uint8Array(o)
            | D::Uint16Array(o)
            | D::Uint32Array(o)
            | D::Uint8ClampedArray(o)
            | D::BigInt64Array(o)
            | D::BigUint64Array(o)
            | D::Float16Array(o)
            | D::Float32Array(o)
            | D::Float64Array(o)
            | D::DataView(o) => {
                let bytes = buffer_bytes(cx, &o);
                self.ws_send(this, bytes.len(), WsCommand::Binary(bytes))
            }
        }
    }
}

impl CloseEventImpl for Engine {
    fn was_clean(&self, this: Handle) -> bool {
        self.ev(this, |e| {
            matches!(
                e.ext,
                EventExt::Close {
                    was_clean: true,
                    ..
                }
            )
        })
    }

    fn code(&self, this: Handle) -> u16 {
        self.ev(this, |e| match &e.ext {
            EventExt::Close { code, .. } => *code,
            _ => 0,
        })
    }

    fn reason(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.ev(this, |e| match &e.ext {
            EventExt::Close { reason, .. } => reason.clone(),
            _ => String::new(),
        }))
    }

    fn constructor(&self, _cx: &mut Cx, type_: &str, init: CloseEventInit) -> rt::Result<Handle> {
        Ok(self.new_event(
            InterfaceId::CloseEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Close {
                was_clean: init.was_clean,
                code: init.code,
                reason: init.reason,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::is_token;

    #[test]
    fn subprotocol_tokens() {
        assert!(is_token("chat"));
        assert!(is_token("v1.chat-x_y"));
        assert!(!is_token(""));
        assert!(!is_token("a b"));
        assert!(!is_token("a,b"));
        assert!(!is_token("é"));
    }
}
