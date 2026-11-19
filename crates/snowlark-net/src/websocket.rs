// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebSocket connections (RFC 6455) as the WebSockets Standard drives them: Fetch's "establish a
//! WebSocket connection" (an HTTP/1.1 upgrade request with mode "websocket", through main fetch, so
//! CSP `connect-src`, mixed content, HSTS, cookies and `Origin` apply as to any fetch), the opening
//! handshake's checks, then framing: masked client frames, fragmentation, ping/pong, the closing
//! handshake, and failing the connection on protocol errors.
//!
//! The page thread holds a [`WebSocketConnection`]: commands go to the network thread, events come
//! back (with a wake-up call), and dropping it closes the connection.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use url::Url;

use crate::client::FetchEvent;
use crate::csp::Violation;
use crate::fetch::{NetState, Sink};
use crate::headers::HeaderList;
use crate::request::{CacheMode, Client, CredentialsMode, Mode, RedirectMode, Referrer, Request};
use crate::response::ResponseHead;

/// RFC 6455 §1.3: the GUID the server appends to the key.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The largest message accepted (the connection is failed with 1009 beyond it).
const MAX_MESSAGE: usize = 256 << 20;

/// How long the closing handshake may take before the connection is dropped.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// What the page asks of a connection.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum WsCommand {
    Text(String),
    Binary(Vec<u8>),
    /// Start the closing handshake: a Close frame with the code and reason (none without a code).
    Close {
        code: Option<u16>,
        reason: String,
    },
}

/// What happens to a connection, in order: violations, then `Open` or `Closed`; after `Open`,
/// messages, `Sent`, maybe `Closing`, and `Closed` last.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum WsEvent {
    /// A CSP violation the handshake request caused, for the document to report.
    Violation(Violation),
    /// "The WebSocket connection is established".
    Open {
        protocol: String,
        extensions: String,
    },
    Text(String),
    Binary(Vec<u8>),
    /// Application data written to the connection, in bytes (`bufferedAmount` goes down).
    Sent(usize),
    /// "The WebSocket closing handshake is started" (the server's Close frame arrived).
    Closing,
    /// "The WebSocket connection is closed": `failed` when the user agent had to fail it.
    Closed {
        failed: bool,
        clean: bool,
        code: u16,
        reason: String,
    },
}

/// Where a connection's commands go.
enum Commands {
    /// This process's network thread.
    Local(mpsc::UnboundedSender<WsCommand>),
    /// A backend of another process.
    Remote(Box<dyn Fn(WsCommand) + Send + Sync>),
}

/// The page's end of a WebSocket connection.
pub struct WebSocketConnection {
    commands: Commands,
    events: mpsc::UnboundedReceiver<WsEvent>,
    _cancel: Option<oneshot::Sender<()>>,
    /// A backend of another process: tells it the page dropped the connection.
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for WebSocketConnection {
    fn drop(&mut self) {
        if let Some(on_drop) = self.on_drop.take() {
            on_drop();
        }
    }
}

/// The network side of a connection made by [`WebSocketConnection::remote`]: a backend of another
/// process feeds it the events it receives, in [`WsEvent`]'s order.
pub struct WsEventSender {
    tx: mpsc::UnboundedSender<WsEvent>,
}

impl WsEventSender {
    /// Queues the next event; false once the page has dropped the connection. It does not wake the
    /// page: the backend runs the connection's `notify` after it.
    pub fn send(&self, event: WsEvent) -> bool {
        self.tx.send(event).is_ok()
    }
}

impl WebSocketConnection {
    /// A connection for a backend other than this process's network thread: `command` takes what
    /// the page sends, the [`WsEventSender`] gives it events, and `on_drop` runs once the page drops it.
    pub fn remote(
        command: impl Fn(WsCommand) + Send + Sync + 'static,
        on_drop: impl FnOnce() + Send + 'static,
    ) -> (WsEventSender, WebSocketConnection) {
        let (tx, events) = mpsc::unbounded_channel();
        let connection = WebSocketConnection {
            commands: Commands::Remote(Box::new(command)),
            events,
            _cancel: None,
            on_drop: Some(Box::new(on_drop)),
        };
        (WsEventSender { tx }, connection)
    }

    /// A connection that failed before it began (the backend is gone): `Closed`, not clean.
    pub fn failed() -> WebSocketConnection {
        let (events, connection) = WebSocketConnection::remote(|_| {}, || {});
        events.send(WsEvent::Closed {
            failed: true,
            clean: false,
            code: 1006,
            reason: String::new(),
        });
        connection
    }

    /// Sends a command (ignored once the connection is gone).
    pub fn send(&self, command: WsCommand) {
        match &self.commands {
            Commands::Local(tx) => {
                let _ = tx.send(command);
            }
            Commands::Remote(send) => send(command),
        }
    }

    /// The next event, if one is waiting.
    pub fn try_next(&mut self) -> Option<WsEvent> {
        self.events.try_recv().ok()
    }
}

/// The parameters of a connection.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WsParams {
    /// The `ws:` or `wss:` URL.
    pub url: Url,
    pub protocols: Vec<String>,
    pub client: Arc<Client>,
}

/// RFC 6455 §4.1: the `Sec-WebSocket-Accept` value for a key.
pub fn accept_key(key: &str) -> String {
    let mut sha = Sha1::new();
    sha.update(key.as_bytes());
    sha.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sha.finalize())
}

/// Fetch "establish a WebSocket connection" step 1: the request URL is the URL with an HTTP scheme.
fn request_url(url: &Url) -> Url {
    let mut u = url.clone();
    let scheme = if url.scheme() == "wss" {
        "https"
    } else {
        "http"
    };
    // (ws and wss are special like http and https, so the scheme can change)
    let _ = u.set_scheme(scheme);
    u
}

/// Starts a connection on the network runtime.
pub(crate) fn start(
    handle: &tokio::runtime::Handle,
    state: Arc<NetState>,
    filter: Option<Arc<dyn crate::filter::RequestFilter>>,
    params: WsParams,
    notify: Arc<dyn Fn() + Send + Sync>,
) -> WebSocketConnection {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let events = Events { tx: ev_tx, notify };
    handle.spawn(async move {
        tokio::select! {
            _ = run(state, filter, params, cmd_rx, events) => {}
            _ = cancel_rx => {}
        }
    });
    WebSocketConnection {
        commands: Commands::Local(cmd_tx),
        events: ev_rx,
        _cancel: Some(cancel_tx),
        on_drop: None,
    }
}

struct Events {
    tx: mpsc::UnboundedSender<WsEvent>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl Events {
    fn emit(&self, e: WsEvent) {
        let _ = self.tx.send(e);
        (self.notify)();
    }

    fn failed(&self) {
        self.emit(WsEvent::Closed {
            failed: true,
            clean: false,
            code: 1006,
            reason: String::new(),
        });
    }
}

/// RFC 6455 §4.1 and WebSockets §"establish a WebSocket connection" step 11: whether a handshake
/// response accepts the request. `Ok` with the protocol the server chose.
fn check_handshake(head: &ResponseHead, key: &str, protocols: &[String]) -> Result<String, String> {
    if head.status != 101 {
        return Err(format!("unexpected response status {}", head.status));
    }
    let h: &HeaderList = &head.headers;
    if !h
        .get("upgrade")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"))
    {
        return Err("the response's Upgrade header is not \"websocket\"".into());
    }
    if !h.get("connection").is_some_and(|v| {
        v.split(',')
            .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
    }) {
        return Err("the response's Connection header has no \"Upgrade\" token".into());
    }
    if h.get("sec-websocket-accept").as_deref().map(str::trim) != Some(accept_key(key).as_str()) {
        return Err("the response's Sec-WebSocket-Accept header is wrong".into());
    }
    if h.get("sec-websocket-extensions")
        .is_some_and(|v| !v.trim().is_empty())
    {
        return Err("the server used an extension the client did not offer".into());
    }
    let protocol = h.get("sec-websocket-protocol").map(|p| p.trim().to_owned());
    match protocol {
        Some(p) if !p.is_empty() => {
            if !protocols.contains(&p) {
                return Err(format!("the server chose a subprotocol not offered: {p}"));
            }
            Ok(p)
        }
        _ if !protocols.is_empty() => Err("the server chose no subprotocol".into()),
        _ => Ok(String::new()),
    }
}

async fn run(
    state: Arc<NetState>,
    filter: Option<Arc<dyn crate::filter::RequestFilter>>,
    params: WsParams,
    mut commands: mpsc::UnboundedReceiver<WsCommand>,
    events: Events,
) {
    let mut nonce = [0u8; 16];
    let _ = getrandom::fill(&mut nonce);
    let key = base64::engine::general_purpose::STANDARD.encode(nonce);
    let mut request = Request::new(request_url(&params.url));
    request.client = Some(params.client.clone());
    request.mode = Mode::WebSocket;
    request.credentials = CredentialsMode::Include;
    request.cache = CacheMode::NoStore;
    request.redirect = RedirectMode::Error;
    request.referrer = Referrer::NoReferrer;
    request.initiator_type = "websocket".into();
    request.headers.append("Upgrade", "websocket");
    request.headers.append("Connection", "Upgrade");
    request.headers.append("Sec-WebSocket-Key", &key);
    request.headers.append("Sec-WebSocket-Version", "13");
    if !params.protocols.is_empty() {
        request
            .headers
            .append("Sec-WebSocket-Protocol", &params.protocols.join(", "));
    }
    let slot: crate::transport::UpgradeSlot = Arc::new(Mutex::new(None));
    request.upgrade = Some(slot.clone());
    let (tx, mut rx) = mpsc::channel(64);
    let sink = Sink {
        tx: Some(tx),
        notify: None,
        observer: None,
        filter,
        ended: Default::default(),
    };
    let fetch = tokio::spawn(state.clone().fetch(request, sink));
    let head = loop {
        match rx.recv().await {
            Some(FetchEvent::Violation(v)) => events.emit(WsEvent::Violation(v)),
            Some(FetchEvent::Head(h)) => break Some(h),
            Some(FetchEvent::Error(_)) | None => break None,
            Some(_) => {}
        }
    };
    drop(rx);
    fetch.abort();
    let Some(head) = head else {
        return events.failed();
    };
    let protocol = match check_handshake(&head, &key, &params.protocols) {
        Ok(p) => p,
        Err(why) => {
            if state.config.trace {
                eprintln!("[net] WebSocket {}: {why}", params.url);
            }
            return events.failed();
        }
    };
    let Some(upgraded) = slot.lock().unwrap().take() else {
        return events.failed();
    };
    events.emit(WsEvent::Open {
        protocol,
        extensions: String::new(),
    });
    let io = hyper_util::rt::TokioIo::new(upgraded);
    let (reader, writer) = tokio::io::split(io);
    frames(reader, writer, &mut commands, &events).await;
}

/// A frame as read from the server.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// Why reading frames stopped.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// The connection closed (or failed) at a frame boundary or within one.
    Closed,
    /// The peer broke the protocol: fail the connection with this status code.
    Protocol(u16),
}

/// RFC 6455 §5.2: reads one frame.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame, FrameError> {
    let mut head = [0u8; 2];
    r.read_exact(&mut head)
        .await
        .map_err(|_| FrameError::Closed)?;
    let fin = head[0] & 0x80 != 0;
    if head[0] & 0x70 != 0 {
        // (RSV bits without a negotiated extension)
        return Err(FrameError::Protocol(1002));
    }
    let opcode = head[0] & 0x0f;
    if !matches!(opcode, 0 | 1 | 2 | 8 | 9 | 10) {
        return Err(FrameError::Protocol(1002));
    }
    if head[1] & 0x80 != 0 {
        // (§5.1: a client closes the connection if a server's frame is masked)
        return Err(FrameError::Protocol(1002));
    }
    let mut len = u64::from(head[1] & 0x7f);
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b).await.map_err(|_| FrameError::Closed)?;
        len = u64::from(u16::from_be_bytes(b));
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b).await.map_err(|_| FrameError::Closed)?;
        len = u64::from_be_bytes(b);
        if len >> 63 != 0 {
            return Err(FrameError::Protocol(1002));
        }
    }
    if opcode >= 8 && (len > 125 || !fin) {
        return Err(FrameError::Protocol(1002));
    }
    if len > MAX_MESSAGE as u64 {
        return Err(FrameError::Protocol(1009));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)
        .await
        .map_err(|_| FrameError::Closed)?;
    Ok(Frame {
        fin,
        opcode,
        payload,
    })
}

/// RFC 6455 §5.2-5.3: a client frame (masked) with FIN set.
pub(crate) fn encode_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len <= 0xffff {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mut mask = [0u8; 4];
    let _ = getrandom::fill(&mut mask);
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// A Close frame's payload: the code (none: empty) and the reason.
fn close_payload(code: Option<u16>, reason: &str) -> Vec<u8> {
    let mut p = Vec::new();
    if let Some(code) = code {
        p.extend_from_slice(&code.to_be_bytes());
        p.extend_from_slice(reason.as_bytes());
    }
    p
}

/// RFC 6455 §7.4.1-7.4.2: whether a status code may appear in a Close frame.
fn valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

/// RFC 6455 §5.5.1: a received Close frame's code and reason (1005 without a code).
fn parse_close(payload: &[u8]) -> Result<(u16, String), u16> {
    match payload.len() {
        0 => Ok((1005, String::new())),
        1 => Err(1002),
        _ => {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            if !valid_close_code(code) {
                return Err(1002);
            }
            let reason = std::str::from_utf8(&payload[2..]).map_err(|_| 1007u16)?;
            Ok((code, reason.to_owned()))
        }
    }
}

/// The open connection: frames in both directions until it closes.
async fn frames<R, W>(
    mut reader: R,
    mut writer: W,
    commands: &mut mpsc::UnboundedReceiver<WsCommand>,
    events: &Events,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let (ftx, mut frx) = mpsc::unbounded_channel();
    let reading = tokio::spawn(async move {
        loop {
            let f = read_frame(&mut reader).await;
            let stop = f.is_err();
            if ftx.send(f).is_err() || stop {
                break;
            }
        }
    });
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _reading = AbortOnDrop(reading);

    let mut sent_close = false;
    let mut received_close: Option<(u16, String)> = None;
    // A message being reassembled: its opcode and data.
    let mut message: Option<(u8, Vec<u8>)> = None;
    let mut deadline: Option<tokio::time::Instant> = None;

    // Fails the connection (RFC 6455 §7.1.7): a Close frame with the code if none was sent, then the
    // TCP connection goes.
    async fn fail<W: AsyncWrite + Unpin>(writer: &mut W, sent_close: bool, code: u16) {
        if !sent_close {
            let _ = writer
                .write_all(&encode_frame(8, &close_payload(Some(code), "")))
                .await;
        }
        let _ = writer.shutdown().await;
    }

    loop {
        let timeout = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            cmd = commands.recv() => {
                let Some(cmd) = cmd else {
                    // (the page let go of the object: go away)
                    if !sent_close {
                        let _ = writer.write_all(&encode_frame(8, &close_payload(Some(1001), ""))).await;
                    }
                    let _ = writer.shutdown().await;
                    return;
                };
                match cmd {
                    WsCommand::Text(_) | WsCommand::Binary(_) if sent_close => {}
                    WsCommand::Text(s) => {
                        if writer.write_all(&encode_frame(1, s.as_bytes())).await.is_err() {
                            continue;
                        }
                        events.emit(WsEvent::Sent(s.len()));
                    }
                    WsCommand::Binary(b) => {
                        if writer.write_all(&encode_frame(2, &b)).await.is_err() {
                            continue;
                        }
                        events.emit(WsEvent::Sent(b.len()));
                    }
                    WsCommand::Close { code, reason } => {
                        if !sent_close {
                            let _ = writer.write_all(&encode_frame(8, &close_payload(code, &reason))).await;
                            let _ = writer.flush().await;
                            sent_close = true;
                            deadline = Some(tokio::time::Instant::now() + CLOSE_TIMEOUT);
                        }
                    }
                }
            }
            f = frx.recv() => {
                let f = f.unwrap_or(Err(FrameError::Closed));
                let frame = match f {
                    Ok(frame) => frame,
                    Err(FrameError::Closed) => {
                        // "The WebSocket Connection is Closed": clean if the closing handshake
                        // completed.
                        let (clean, code, reason) = match received_close.take() {
                            Some((code, reason)) if sent_close => (true, code, reason),
                            _ => (false, 1006, String::new()),
                        };
                        events.emit(WsEvent::Closed { failed: false, clean, code, reason });
                        return;
                    }
                    Err(FrameError::Protocol(code)) => {
                        fail(&mut writer, sent_close, code).await;
                        return events.failed();
                    }
                };
                match frame.opcode {
                    0..=2 => {
                        if received_close.is_some() {
                            continue;
                        }
                        let data = match (frame.opcode, message.take()) {
                            (0, Some((op, mut data))) => {
                                data.extend_from_slice(&frame.payload);
                                (op, data)
                            }
                            (op @ (1 | 2), None) => (op, frame.payload),
                            // (a continuation with nothing to continue, or a new message within one)
                            _ => {
                                fail(&mut writer, sent_close, 1002).await;
                                return events.failed();
                            }
                        };
                        if data.1.len() > MAX_MESSAGE {
                            fail(&mut writer, sent_close, 1009).await;
                            return events.failed();
                        }
                        if !frame.fin {
                            message = Some(data);
                            continue;
                        }
                        match data {
                            (1, bytes) => match String::from_utf8(bytes) {
                                Ok(s) => events.emit(WsEvent::Text(s)),
                                Err(_) => {
                                    fail(&mut writer, sent_close, 1007).await;
                                    return events.failed();
                                }
                            },
                            (_, bytes) => events.emit(WsEvent::Binary(bytes)),
                        }
                    }
                    8 => {
                        if received_close.is_some() {
                            continue;
                        }
                        let (code, reason) = match parse_close(&frame.payload) {
                            Ok(c) => c,
                            Err(code) => {
                                fail(&mut writer, sent_close, code).await;
                                return events.failed();
                            }
                        };
                        events.emit(WsEvent::Closing);
                        if !sent_close {
                            // (§5.5.1: the response echoes the status code)
                            let echo = (code != 1005).then_some(code);
                            let _ = writer.write_all(&encode_frame(8, &close_payload(echo, ""))).await;
                            let _ = writer.flush().await;
                            sent_close = true;
                        }
                        received_close = Some((code, reason));
                        // (the server closes the TCP connection first, §7.1.1; not for ever)
                        deadline = Some(tokio::time::Instant::now() + CLOSE_TIMEOUT);
                    }
                    9 if !sent_close => {
                        let _ = writer.write_all(&encode_frame(10, &frame.payload)).await;
                    }
                    _ => {}
                }
            }
            _ = timeout => {
                let _ = writer.shutdown().await;
                let (clean, code, reason) = match received_close.take() {
                    Some((code, reason)) => (true, code, reason),
                    None => (false, 1006, String::new()),
                };
                events.emit(WsEvent::Closed { failed: false, clean, code, reason });
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_rfc_example() {
        // RFC 6455 §1.3
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn request_url_has_http_scheme() {
        let u = Url::parse("wss://example.com:9/chat?x").unwrap();
        assert_eq!(request_url(&u).as_str(), "https://example.com:9/chat?x");
        let u = Url::parse("ws://example.com/").unwrap();
        assert_eq!(request_url(&u).as_str(), "http://example.com/");
    }

    fn unmask(frame: &[u8]) -> (u8, Vec<u8>) {
        let len = (frame[1] & 0x7f) as usize;
        let (len, at) = match len {
            126 => (u16::from_be_bytes([frame[2], frame[3]]) as usize, 4),
            127 => (
                u64::from_be_bytes(frame[2..10].try_into().unwrap()) as usize,
                10,
            ),
            n => (n, 2),
        };
        let mask = &frame[at..at + 4];
        let data = frame[at + 4..at + 4 + len]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();
        (frame[0], data)
    }

    #[test]
    fn client_frames_are_masked() {
        for n in [0, 5, 125, 126, 70000] {
            let payload: Vec<u8> = (0..n).map(|i| i as u8).collect();
            let f = encode_frame(2, &payload);
            assert_eq!(f[1] & 0x80, 0x80);
            assert_eq!(unmask(&f), (0x82, payload));
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn reads_server_frames() {
        run(async {
            let mut bytes: &[u8] = &[0x81, 0x03, b'a', b'b', b'c', 0x02, 0x7e, 0x00, 0x02, 1, 2];
            assert_eq!(
                read_frame(&mut bytes).await,
                Ok(Frame {
                    fin: true,
                    opcode: 1,
                    payload: b"abc".to_vec()
                })
            );
            assert_eq!(
                read_frame(&mut bytes).await,
                Ok(Frame {
                    fin: false,
                    opcode: 2,
                    payload: vec![1, 2]
                })
            );
            assert_eq!(read_frame(&mut bytes).await, Err(FrameError::Closed));
            // A masked server frame, reserved bits, a long control frame, an unknown opcode.
            for bad in [
                &[0x81u8, 0x81, 0, 0, 0, 0, 0][..],
                &[0xc1, 0x00],
                &[0x89, 126, 0, 126],
                &[0x83, 0x00],
                &[0x08, 0x00],
            ] {
                let mut b = bad;
                assert_eq!(read_frame(&mut b).await, Err(FrameError::Protocol(1002)));
            }
        });
    }

    #[test]
    fn close_frames() {
        assert_eq!(parse_close(&[]), Ok((1005, String::new())));
        assert_eq!(parse_close(&[3]), Err(1002));
        assert_eq!(
            parse_close(&[0x03, 0xe8, b'o', b'k']),
            Ok((1000, "ok".into()))
        );
        assert_eq!(parse_close(&[0x03, 0xed]), Err(1002)); // 1005 is never sent
        assert_eq!(parse_close(&[0x0f, 0xa0, 0xff]), Err(1007));
        assert_eq!(close_payload(None, "x"), Vec::<u8>::new());
        assert_eq!(
            close_payload(Some(3000), "hi"),
            vec![0x0b, 0xb8, b'h', b'i']
        );
    }

    fn head(status: u16, headers: &[(&str, &str)]) -> ResponseHead {
        let mut h = ResponseHead::new(status, "");
        for (n, v) in headers {
            h.headers.append(n, v);
        }
        h
    }

    #[test]
    fn handshake_checks() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = accept_key(key);
        let ok = [
            ("Upgrade", "WebSocket"),
            ("Connection", "keep-alive, Upgrade"),
            ("Sec-WebSocket-Accept", accept.as_str()),
        ];
        assert_eq!(
            check_handshake(&head(101, &ok), key, &[]),
            Ok(String::new())
        );
        assert!(check_handshake(&head(200, &ok), key, &[]).is_err());
        let protos = vec!["chat".to_string(), "v2".to_string()];
        assert!(check_handshake(&head(101, &ok), key, &protos).is_err());
        let mut with = ok.to_vec();
        with.push(("Sec-WebSocket-Protocol", "v2"));
        assert_eq!(
            check_handshake(&head(101, &with), key, &protos),
            Ok("v2".into())
        );
        assert!(check_handshake(&head(101, &with), key, &[]).is_err());
        let mut ext = ok.to_vec();
        ext.push(("Sec-WebSocket-Extensions", "permessage-deflate"));
        assert!(check_handshake(&head(101, &ext), key, &[]).is_err());
        let wrong = [
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Accept", "nope"),
        ];
        assert!(check_handshake(&head(101, &wrong), key, &[]).is_err());
    }
}
