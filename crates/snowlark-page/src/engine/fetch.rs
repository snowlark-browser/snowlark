// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Fetch Standard's JavaScript API (§5): `Headers`, `Request`, `Response`, the `Body` mixin and
//! `fetch()`, over snowlark-net's fetch algorithms; `FormData` (XHR Standard §5) and body extraction,
//! which `XMLHttpRequest` shares. `fetch()` resolves at the response head with a Response whose body
//! is a byte stream the network's bytes are enqueued to as they arrive (Fetch §5.6 "fetch()", the
//! "process response" steps). Other bodies are buffered: their `ReadableStream` (`body`) is made on
//! first access and delivers the bytes as one chunk. A Response made from a ReadableStream reads it
//! when consumed; a Request's body can't be a stream yet.

use std::rc::Rc;
use std::sync::Arc;

use snowlark_bindings::generated::ids;
use snowlark_bindings::generated::{
    BodyImpl, FileOrString, FormDataImpl, HeadersImpl, IPAddressSpace,
    ReadableStreamOrBlobOrArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataViewOrFormDataOrURLSearchParamsOrString as BodyInit,
    ReferrerPolicy, RequestCache, RequestCredentials, RequestDestination, RequestDuplex,
    RequestImpl, RequestInit, RequestMode, RequestOrString, RequestRedirect, ResponseImpl,
    ResponseInit, ResponseType, StringSequenceSequenceOrStringRecord,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle, JsPromise, JsValue};
use snowlark_dom::NodeId;
use snowlark_net::{FetchEvent, HeadersGuard, ResponseHead, ResponseStream};

use super::blob::{array_buffer, uint8_array};
use super::encoding::buffer_bytes;
use super::{Engine, Object};

/// A body: its bytes (buffered) and whether it was read. `None` bytes is a null body.
#[derive(Clone, Default)]
pub struct BodyData {
    pub bytes: Option<Rc<Vec<u8>>>,
    pub used: bool,
    /// The body's stream: made from the bytes on first access to `body`, or the stream a Response
    /// was made from (then `bytes` is `None`).
    pub stream: Option<Handle>,
    /// A fetched Response's body still arriving: the id of the API fetch job that holds its bytes
    /// so far (`bytes` and `stream` are `None`). It becomes `bytes` when the fetch ends, or a
    /// stream the rest goes to when script asks for `body` first.
    pub net: Option<u32>,
}

/// What a body read waits with while the body is still arriving: called with its bytes, or the
/// error it failed with, in the fetch's realm.
pub(crate) type BodyConsumer = Box<dyn FnOnce(&Engine, &mut Cx, Result<Vec<u8>, JsValue>)>;

pub struct RequestData {
    pub method: String,
    pub url: url::Url,
    /// The URL's blob URL entry, held when the URL was parsed
    /// ([`snowlark_net::Request::blob_url_entry`]): a Request made of a `blob:` URL fetches that
    /// blob even once the URL is revoked.
    pub blob_url_entry: Option<u64>,
    pub headers: Handle,
    pub destination: RequestDestination,
    /// "client", "no-referrer" (""), or a URL.
    pub referrer: String,
    pub referrer_policy: ReferrerPolicy,
    pub mode: RequestMode,
    pub credentials: RequestCredentials,
    pub cache: RequestCache,
    pub redirect: RequestRedirect,
    pub integrity: String,
    pub keepalive: bool,
    pub signal: Handle,
    pub body: BodyData,
    /// The document whose environment the request belongs to.
    pub client: Option<NodeId>,
}

pub struct ResponseData {
    pub type_: ResponseType,
    pub url_list: Vec<url::Url>,
    pub status: u16,
    pub status_text: String,
    pub headers: Handle,
    pub body: BodyData,
}

/// A FormData entry's value.
#[derive(Clone)]
pub enum FormValue {
    Str(String),
    File(Handle),
}

/// What an API fetch in flight reports to.
pub enum JobOwner {
    Fetch {
        promise: JsPromise,
        signal: Handle,
        realm: Option<v8::Global<v8::Context>>,
    },
    Xhr(Handle),
    /// A beacon: nothing waits for the response.
    Beacon,
}

/// A fetch started by script (`fetch()`, `XMLHttpRequest`), polled by the event loop.
pub struct ApiJob {
    pub id: u32,
    pub owner: JobOwner,
    pub stream: ResponseStream,
    pub head: Option<ResponseHead>,
    pub body: Vec<u8>,
    /// The document whose fetch group the fetch is in (its client).
    pub client: NodeId,
    /// What the fetch counts against its group's keepalive quota: its body's length when it is a
    /// keepalive fetch, else 0.
    pub keepalive_bytes: usize,
    /// A `fetch()` whose promise was resolved at the response head: its Response, whose body
    /// (`BodyData::net`) is this job's bytes.
    pub response: Option<Handle>,
    /// The controller of that Response's body stream, once script asked for it: the bytes go there
    /// as they arrive instead of to `body`.
    pub body_controller: Option<Handle>,
    /// Body reads (`text()`, `json()`, ...) waiting for the whole body.
    pub consumers: Vec<BodyConsumer>,
}

/// Fetch, HTTP-network-or-cache fetch: a fetch group's keepalive requests may have at most 64 KiB
/// of bodies in flight (Blink's `ResourceFetcher::kKeepaliveInflightBytesQuota`).
pub(crate) const KEEPALIVE_INFLIGHT_QUOTA: usize = 64 * 1024;

/// The status codes whose responses have no body.
pub(crate) fn is_null_body_status(s: u16) -> bool {
    matches!(s, 101 | 103 | 204 | 205 | 304)
}

fn type_error(m: impl Into<String>) -> rt::Error {
    rt::Error::Type(m.into().into())
}

fn headers_error(e: snowlark_net::headers::HeadersError) -> rt::Error {
    type_error(e.to_string())
}

/// A token (RFC 9110 `token`), as methods must be.
fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Normalizes a method (Fetch §2.2.1): the six CORS-relevant ones are uppercased.
pub(crate) fn normalize_method(m: &str) -> String {
    let u = m.to_ascii_uppercase();
    if matches!(
        u.as_str(),
        "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT"
    ) {
        u
    } else {
        m.to_owned()
    }
}

pub(crate) fn is_forbidden_method(m: &str) -> bool {
    matches!(
        m.to_ascii_uppercase().as_str(),
        "CONNECT" | "TRACE" | "TRACK"
    )
}

/// A boundary for `multipart/form-data` bodies.
fn boundary() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("----snowlark{:x}", n)
}

impl Engine {
    // ----- objects -----------------------------------------------------------------------------------

    pub(crate) fn new_headers(&self, guard: HeadersGuard) -> Handle {
        self.alloc(Object::Headers(snowlark_net::Headers::new(guard)))
    }

    pub(crate) fn headers_of(&self, h: Handle) -> std::cell::RefMut<'_, snowlark_net::Headers> {
        std::cell::RefMut::map(self.obj_mut(h), |o| match o {
            Object::Headers(x) => x,
            _ => panic!("not a Headers"),
        })
    }

    fn request(&self, h: Handle) -> std::cell::RefMut<'_, RequestData> {
        std::cell::RefMut::map(self.obj_mut(h), |o| match o {
            Object::Request(r) => &mut **r,
            _ => panic!("not a Request"),
        })
    }

    fn response(&self, h: Handle) -> std::cell::RefMut<'_, ResponseData> {
        std::cell::RefMut::map(self.obj_mut(h), |o| match o {
            Object::Response(r) => &mut **r,
            _ => panic!("not a Response"),
        })
    }

    pub(crate) fn form_data(&self, h: Handle) -> std::cell::RefMut<'_, Vec<(String, FormValue)>> {
        std::cell::RefMut::map(self.obj_mut(h), |o| match o {
            Object::FormData(f) => f,
            _ => panic!("not a FormData"),
        })
    }

    /// "Fill" a Headers object from a HeadersInit.
    fn fill_headers(
        &self,
        h: Handle,
        init: StringSequenceSequenceOrStringRecord,
    ) -> rt::Result<()> {
        match init {
            StringSequenceSequenceOrStringRecord::StringSequenceSequence(seq) => {
                for pair in seq {
                    if pair.len() != 2 {
                        return Err(type_error("A header pair must have exactly two items"));
                    }
                    self.headers_of(h)
                        .append(&pair[0], &pair[1])
                        .map_err(headers_error)?;
                }
            }
            StringSequenceSequenceOrStringRecord::StringRecord(rec) => {
                for (k, v) in rec.0 {
                    self.headers_of(h).append(&k, &v).map_err(headers_error)?;
                }
            }
        }
        Ok(())
    }

    /// The API base URL of the current realm's document.
    pub(crate) fn current_base_url(&self, cx: &mut Cx) -> rt::Result<url::Url> {
        let doc = self.current_document(cx)?;
        Ok(self.document_base_url(&self.dom(), doc))
    }

    // ----- bodies ------------------------------------------------------------------------------------

    /// "Extract a body" (Fetch §5.2): the bytes and the Content-Type it implies.
    pub(crate) fn extract_body(&self, cx: &mut Cx, init: BodyInit) -> (Vec<u8>, Option<String>) {
        match init {
            // (callers that take streams handle them before: here a stream has no bytes)
            BodyInit::ReadableStream(_) => (Vec::new(), None),
            BodyInit::String(s) => (s.into_bytes(), Some("text/plain;charset=UTF-8".into())),
            BodyInit::URLSearchParams(p) => {
                let list = match &*self.obj(p) {
                    Object::UrlSearchParams(d) => d.list.clone(),
                    _ => Vec::new(),
                };
                let s = url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(list.iter())
                    .finish();
                (
                    s.into_bytes(),
                    Some("application/x-www-form-urlencoded;charset=UTF-8".into()),
                )
            }
            BodyInit::Blob(b) => {
                let (bytes, t) = self.blob_contents(b);
                (bytes.to_vec(), (!t.is_empty()).then_some(t))
            }
            BodyInit::FormData(f) => {
                let b = boundary();
                let bytes = self.multipart(f, &b);
                (bytes, Some(format!("multipart/form-data; boundary={b}")))
            }
            BodyInit::ArrayBuffer(o)
            | BodyInit::Int8Array(o)
            | BodyInit::Int16Array(o)
            | BodyInit::Int32Array(o)
            | BodyInit::Uint8Array(o)
            | BodyInit::Uint16Array(o)
            | BodyInit::Uint32Array(o)
            | BodyInit::Uint8ClampedArray(o)
            | BodyInit::BigInt64Array(o)
            | BodyInit::BigUint64Array(o)
            | BodyInit::Float16Array(o)
            | BodyInit::Float32Array(o)
            | BodyInit::Float64Array(o)
            | BodyInit::DataView(o) => (buffer_bytes(cx, &o), None),
        }
    }

    /// The `multipart/form-data` encoding of a FormData's entries (HTML §4.10.21.8).
    pub(crate) fn multipart(&self, form: Handle, boundary: &str) -> Vec<u8> {
        let entries = self.form_data(form).clone();
        self.multipart_entries(&entries, boundary, encoding_rs::UTF_8)
    }

    /// The `multipart/form-data` encoding of an entry list (RFC 7578, as HTML §4.10.21.8 uses it):
    /// names, string values and file names in `encoding` (the form's), the names and file names then
    /// escaped byte by byte (CR, LF and `"` as `%0D`, `%0A`, `%22`).
    pub(crate) fn multipart_entries(
        &self,
        entries: &[(String, FormValue)],
        boundary: &str,
        encoding: &'static encoding_rs::Encoding,
    ) -> Vec<u8> {
        use super::forms::encode_html;
        let entries = entries.to_vec();
        let mut out = Vec::new();
        let esc = |out: &mut Vec<u8>, s: &str| {
            for &b in encode_html(encoding, s).iter() {
                match b {
                    b'\r' => out.extend_from_slice(b"%0D"),
                    b'\n' => out.extend_from_slice(b"%0A"),
                    b'"' => out.extend_from_slice(b"%22"),
                    _ => out.push(b),
                }
            }
        };
        for (name, value) in entries {
            // (HTML §4.10.21.8: names, and string values, have their newlines normalized first)
            let name = super::forms::normalize_newlines(&name);
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            out.extend_from_slice(b"Content-Disposition: form-data; name=\"");
            esc(&mut out, &name);
            match value {
                FormValue::Str(v) => {
                    out.extend_from_slice(b"\"\r\n\r\n");
                    let v = v
                        .replace("\r\n", "\n")
                        .replace('\r', "\n")
                        .replace('\n', "\r\n");
                    out.extend_from_slice(&encode_html(encoding, &v));
                }
                FormValue::File(f) => {
                    let (bytes, t) = self.blob_contents(f);
                    let fname = self.blob_file_name(f).unwrap_or_else(|| "blob".into());
                    let t = if t.is_empty() {
                        "application/octet-stream".to_owned()
                    } else {
                        t
                    };
                    out.extend_from_slice(b"\"; filename=\"");
                    esc(&mut out, &fname);
                    out.extend_from_slice(format!("\"\r\nContent-Type: {t}\r\n\r\n").as_bytes());
                    out.extend_from_slice(&bytes);
                }
            }
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
    }

    /// The body and headers of a Request or Response.
    fn body_parts(&self, h: Handle) -> Option<(BodyData, Handle)> {
        match &*self.obj(h) {
            Object::Request(r) => Some((r.body.clone(), r.headers)),
            Object::Response(r) => Some((r.body.clone(), r.headers)),
            _ => None,
        }
    }

    fn set_body_used(&self, h: Handle) {
        match &mut *self.obj_mut(h) {
            Object::Request(r) => r.body.used = true,
            Object::Response(r) => r.body.used = true,
            _ => {}
        }
    }

    /// "Consume body" (Fetch §5.3): the bytes, or a rejected promise when the body was read.
    /// The Body mixin's "get the MIME type" (Fetch §6.2.1): "extract a MIME type" from the
    /// header list, serialized; `None` on failure.
    fn body_mime_type(&self, headers: Handle) -> Option<String> {
        snowlark_net::mime::extract_mime_type(&self.headers_of(headers).list).map(|m| m.serialize())
    }

    fn consume(
        &self,
        cx: &mut Cx,
        h: Handle,
        convert: impl FnOnce(&Engine, &mut Cx, Vec<u8>, Option<String>, &JsPromise) -> rt::Result<()>
            + 'static,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let Some((body, headers)) = self.body_parts(h) else {
            return Err(type_error("Illegal invocation"));
        };
        if body.used {
            p.reject(cx, type_error("Body has already been consumed"));
            return Ok(p);
        }
        if let Some(id) = body.net {
            // The body is still arriving: the read waits for all of it ("fully read" the body,
            // without a stream nobody else can see).
            self.set_body_used(h);
            let mime = self.body_mime_type(headers);
            let p2 = p.clone();
            let consume: BodyConsumer = Box::new(move |e, cx, r| match r {
                Ok(bytes) => {
                    if let Err(err) = convert(e, cx, bytes, mime, &p2) {
                        p2.reject(cx, err);
                    }
                }
                Err(err) => p2.reject_value(cx, &err),
            });
            if let Some(j) = self.api_jobs.borrow_mut().iter_mut().find(|j| j.id == id) {
                j.consumers.push(consume);
            }
            return Ok(p);
        }
        if let Some(s) = body.stream {
            // "Fully read" the body's stream.
            if self.readable_locked(s) || self.readable_disturbed(s) {
                p.reject(cx, type_error("Body has already been consumed"));
                return Ok(p);
            }
            self.set_body_used(h);
            let reader = self.acquire_default_reader(cx, s)?;
            let mime = self.body_mime_type(headers);
            let p2 = p.clone();
            super::streams::readable::read_all_bytes(
                self,
                cx,
                reader,
                Box::new(move |e, cx, r| match r {
                    Ok(bytes) => {
                        if let Err(err) = convert(e, cx, bytes, mime, &p2) {
                            p2.reject(cx, err);
                        }
                    }
                    Err(err) => p2.reject_value(cx, &err),
                }),
            );
            return Ok(p);
        }
        if body.bytes.is_some() {
            self.set_body_used(h);
        }
        let bytes = body.bytes.map(|b| b.to_vec()).unwrap_or_default();
        let mime = self.body_mime_type(headers);
        if let Err(e) = convert(self, cx, bytes, mime, &p) {
            p.reject(cx, e);
        }
        Ok(p)
    }

    /// Parses a body as JSON (`JSON.parse` of its UTF-8 text); `Err` holds the SyntaxError thrown.
    pub(crate) fn parse_json(&self, cx: &mut Cx, bytes: &[u8]) -> Result<JsValue, JsValue> {
        let (text, _) = encoding_rs::UTF_8.decode_with_bom_removal(bytes);
        let scope = cx.scope();
        v8::tc_scope!(let tc, scope);
        let Some(s) = v8::String::new(tc, &text) else {
            return Err(rt::undefined_value(tc));
        };
        match v8::json::parse(tc, s) {
            Some(v) => Ok(JsValue::new(tc, v)),
            None => {
                let e = tc
                    .exception()
                    .map(|e| JsValue::new(tc, e))
                    .unwrap_or_else(|| rt::undefined_value(tc));
                tc.reset();
                Err(e)
            }
        }
    }

    /// Parses `application/x-www-form-urlencoded` or `multipart/form-data` into a FormData.
    fn parse_form_data(&self, bytes: &[u8], mime: Option<&str>) -> rt::Result<Handle> {
        let m = mime.and_then(snowlark_net::mime::MimeType::parse);
        let essence = m.as_ref().map(|m| m.essence()).unwrap_or_default();
        let mut entries = Vec::new();
        if essence == "application/x-www-form-urlencoded" {
            for (k, v) in url::form_urlencoded::parse(bytes) {
                entries.push((k.into_owned(), FormValue::Str(v.into_owned())));
            }
        } else if essence == "multipart/form-data" {
            let b = m
                .as_ref()
                .and_then(|m| m.parameter("boundary"))
                .ok_or_else(|| type_error("Missing boundary"))?
                .to_owned();
            let parts = snowlark_net::multipart::parse(bytes, &b)
                .ok_or_else(|| type_error("Invalid multipart/form-data body"))?;
            for part in parts {
                let value = match part.filename {
                    Some(f) => FormValue::File(self.new_file(
                        part.body,
                        part.content_type.as_deref().unwrap_or(""),
                        &f,
                        0,
                    )),
                    None => FormValue::Str(String::from_utf8_lossy(&part.body).into_owned()),
                };
                entries.push((part.name, value));
            }
        } else {
            return Err(type_error("Could not parse the body as FormData"));
        }
        Ok(self.alloc(Object::FormData(entries)))
    }

    // ----- Request and Response construction ----------------------------------------------------------

    /// Fetch §5.4 `new Request(input, init)`.
    fn construct_request(
        &self,
        cx: &mut Cx,
        input: RequestOrString,
        init: RequestInit,
    ) -> rt::Result<Handle> {
        let base = self.current_base_url(cx)?;
        let client = Some(self.current_document(cx)?);
        // 5-6.
        let (mut r, input_request, fallback_cors) = match input {
            RequestOrString::String(s) => {
                let url = base
                    .join(&s)
                    .map_err(|_| type_error(format!("Failed to parse URL from {s}")))?;
                if !url.username().is_empty() || url.password().is_some() {
                    return Err(type_error("Request URL includes credentials"));
                }
                let blob_url_entry = self.hold_blob_url(&url);
                let headers = self.new_headers(HeadersGuard::Request);
                let signal = self.alloc(Object::AbortSignal(Default::default()));
                (
                    RequestData {
                        method: "GET".into(),
                        url,
                        blob_url_entry,
                        headers,
                        destination: RequestDestination::Empty,
                        referrer: "client".into(),
                        referrer_policy: ReferrerPolicy::Empty,
                        mode: RequestMode::Cors,
                        credentials: RequestCredentials::SameOrigin,
                        cache: RequestCache::Default,
                        redirect: RequestRedirect::Follow,
                        integrity: String::new(),
                        keepalive: false,
                        signal,
                        body: BodyData::default(),
                        client,
                    },
                    None,
                    true,
                )
            }
            RequestOrString::Request(h) => {
                let headers = self.new_headers(HeadersGuard::Request);
                let signal = self.alloc(Object::AbortSignal(Default::default()));
                let (r, src_headers) = {
                    let src = self.request(h);
                    (
                        RequestData {
                            method: src.method.clone(),
                            url: src.url.clone(),
                            blob_url_entry: src.blob_url_entry,
                            headers,
                            destination: src.destination,
                            referrer: src.referrer.clone(),
                            referrer_policy: src.referrer_policy,
                            mode: src.mode,
                            credentials: src.credentials,
                            cache: src.cache,
                            redirect: src.redirect,
                            integrity: src.integrity.clone(),
                            keepalive: src.keepalive,
                            signal,
                            body: src.body.clone(),
                            client,
                        },
                        src.headers,
                    )
                };
                let list = self.headers_of(src_headers).list.clone();
                self.headers_of(headers).list = list;
                (r, Some(h), false)
            }
        };
        let input_signal = input_request.map(|h| self.request(h).signal);
        // 12: `window` must be null.
        if let Some(w) = &init.window {
            let scope = cx.scope();
            let v = v8::Local::new(scope, w.global());
            if !v.is_null_or_undefined() {
                return Err(type_error("RequestInit.window must be null"));
            }
        }
        let init_present = init.method.is_some()
            || init.headers.is_some()
            || init.body.is_some()
            || init.referrer.is_some()
            || init.referrer_policy.is_some()
            || init.mode.is_some()
            || init.credentials.is_some()
            || init.cache.is_some()
            || init.redirect.is_some()
            || init.integrity.is_some()
            || init.keepalive.is_some()
            || init.signal.is_some()
            || init.window.is_some()
            || init.duplex.is_some();
        // 13.
        if init_present {
            if r.mode == RequestMode::Navigate {
                r.mode = RequestMode::SameOrigin;
            }
            r.referrer = "client".into();
            r.referrer_policy = ReferrerPolicy::Empty;
        }
        // 14: referrer.
        if let Some(referrer) = &init.referrer {
            if referrer.is_empty() {
                r.referrer = String::new();
            } else {
                let u = base
                    .join(referrer)
                    .map_err(|_| type_error("Invalid referrer URL"))?;
                let same = u.origin() == self.net_origin_of_document(self.current_document(cx)?);
                r.referrer = if u.as_str() == "about:client" || !same {
                    "client".into()
                } else {
                    u.to_string()
                };
            }
        }
        if let Some(p) = init.referrer_policy {
            r.referrer_policy = p;
        }
        // 16-18: mode.
        let mode = init.mode.or(fallback_cors.then_some(RequestMode::Cors));
        if let Some(m) = init.mode {
            if m == RequestMode::Navigate {
                return Err(type_error("Cannot construct a Request with mode navigate"));
            }
        }
        if let Some(m) = mode {
            r.mode = m;
        }
        if let Some(c) = init.credentials {
            r.credentials = c;
        }
        if let Some(c) = init.cache {
            r.cache = c;
        }
        if r.cache == RequestCache::OnlyIfCached && r.mode != RequestMode::SameOrigin {
            return Err(type_error("only-if-cached requires mode same-origin"));
        }
        if let Some(rd) = init.redirect {
            r.redirect = rd;
        }
        if let Some(i) = &init.integrity {
            r.integrity = i.clone();
        }
        if let Some(k) = init.keepalive {
            r.keepalive = k;
        }
        // 25: method.
        if let Some(m) = &init.method {
            if !is_token(m) {
                return Err(type_error(format!("'{m}' is not a valid HTTP method")));
            }
            if is_forbidden_method(m) {
                return Err(type_error(format!("'{m}' HTTP method is unsupported")));
            }
            r.method = normalize_method(m);
        }
        // 26-29: signal.
        let follow = init.signal.or(input_signal);
        if let Some(s) = follow {
            if self.signal_aborted(s) {
                let reason = match &*self.obj(s) {
                    Object::AbortSignal(d) => d.reason.clone(),
                    _ => None,
                };
                if let Object::AbortSignal(d) = &mut *self.obj_mut(r.signal) {
                    d.aborted = true;
                    d.reason = reason;
                }
            } else {
                self.follow_signal(r.signal, s);
            }
        }
        // 30-32: headers.
        if r.mode == RequestMode::NoCors {
            if !matches!(r.method.as_str(), "GET" | "HEAD" | "POST") {
                return Err(type_error("no-cors requests allow only GET, HEAD and POST"));
            }
            self.headers_of(r.headers).guard = HeadersGuard::RequestNoCors;
        }
        if let Some(h) = init.headers {
            self.headers_of(r.headers).list = snowlark_net::HeaderList::new();
            self.fill_headers(r.headers, h)?;
        } else if r.mode == RequestMode::NoCors && input_request.is_some() {
            // Re-filter the copied headers through the no-cors guard.
            let list = self.headers_of(r.headers).list.clone();
            self.headers_of(r.headers).list = snowlark_net::HeaderList::new();
            for (k, v) in list.iter() {
                let _ = self.headers_of(r.headers).append(k, v);
            }
        }
        // 33-37: body.
        let has_input_body = r.body.bytes.is_some();
        let init_body = init.body;
        if (init_body.is_some() || has_input_body) && matches!(r.method.as_str(), "GET" | "HEAD") {
            return Err(type_error("Request with GET/HEAD method cannot have body"));
        }
        if matches!(init_body, Some(BodyInit::ReadableStream(_))) {
            return Err(type_error(
                "ReadableStream request bodies are not supported yet",
            ));
        }
        if let Some(b) = init_body {
            let (bytes, ctype) = self.extract_body(cx, b);
            if let Some(t) = ctype {
                if !self.headers_of(r.headers).list.contains("content-type") {
                    let _ = self.headers_of(r.headers).append("Content-Type", &t);
                }
            }
            r.body = BodyData {
                bytes: Some(Rc::new(bytes)),
                used: false,
                stream: None,
                net: None,
            };
        }
        if let Some(h) = input_request {
            // "Unusable": the body was read, or its stream is locked (a reader was taken) or
            // disturbed.
            let unusable = {
                let body = &self.request(h).body;
                (body.used, body.stream)
            };
            let unusable = unusable.0
                || unusable
                    .1
                    .is_some_and(|s| self.readable_locked(s) || self.readable_disturbed(s));
            if has_input_body && unusable {
                return Err(type_error(
                    "Cannot construct a Request with a Request whose body was used",
                ));
            }
            if has_input_body {
                // The input's body moves to the new request.
                self.request(h).body.used = true;
            }
        }
        Ok(self.alloc(Object::Request(Box::new(r))))
    }

    /// Makes `follower` abort when `parent` does (a dependent signal, DOM §3.2).
    fn follow_signal(&self, follower: Handle, parent: Handle) {
        if let Object::AbortSignal(p) = &mut *self.obj_mut(parent) {
            p.dependents.push(follower);
        }
    }

    pub(crate) fn new_response(
        &self,
        type_: ResponseType,
        status: u16,
        status_text: &str,
        guard: HeadersGuard,
        body: Option<Vec<u8>>,
    ) -> Handle {
        let headers = self.new_headers(guard);
        self.alloc(Object::Response(Box::new(ResponseData {
            type_,
            url_list: Vec::new(),
            status,
            status_text: status_text.to_owned(),
            headers,
            body: BodyData {
                bytes: body.map(Rc::new),
                used: false,
                stream: None,
                net: None,
            },
        })))
    }

    /// "Initialize a response" (Fetch §5.5).
    fn initialize_response(
        &self,
        response: Handle,
        init: ResponseInit,
        body: Option<(Vec<u8>, Option<String>)>,
    ) -> rt::Result<()> {
        if !(200..=599).contains(&init.status) {
            return Err(rt::Error::Range(
                "The status must be in the range 200 to 599".into(),
            ));
        }
        if !init
            .status_text
            .bytes()
            .all(|b| b == b'\t' || (b' '..=0x7E).contains(&b) || b >= 0x80)
        {
            return Err(type_error("Invalid statusText"));
        }
        let headers = {
            let mut r = self.response(response);
            r.status = init.status;
            r.status_text = init.status_text.clone();
            r.headers
        };
        if let Some(h) = init.headers {
            self.fill_headers(headers, h)?;
        }
        if let Some((bytes, ctype)) = body {
            if is_null_body_status(init.status) {
                return Err(type_error(
                    "Response with null body status cannot have body",
                ));
            }
            self.response(response).body.bytes = Some(Rc::new(bytes));
            if let Some(t) = ctype {
                if !self.headers_of(headers).list.contains("content-type") {
                    self.headers_of(headers).list.append("Content-Type", &t);
                }
            }
        }
        Ok(())
    }

    // ----- fetch() --------------------------------------------------------------------------------------

    /// The snowlark-net request for a Request object.
    fn net_request(&self, h: Handle) -> snowlark_net::Request {
        let header_handle = self.request(h).headers;
        let header_list = self.headers_of(header_handle).list.clone();
        let r = self.request(h);
        let mut n = snowlark_net::Request::new(r.url.clone());
        n.blob_url_entry = r.blob_url_entry;
        n.method = r.method.clone();
        n.headers = header_list;
        n.body = r
            .body
            .bytes
            .as_ref()
            .map(|b| snowlark_net::Body::new(b.to_vec()));
        if let Some(doc) = r.client {
            n.client = Some(self.request_client(doc));
        }
        n.mode = snowlark_net::Mode::parse(r.mode.as_str()).unwrap_or(snowlark_net::Mode::Cors);
        n.credentials = snowlark_net::CredentialsMode::parse(r.credentials.as_str())
            .unwrap_or(snowlark_net::CredentialsMode::SameOrigin);
        n.redirect = snowlark_net::RedirectMode::parse(r.redirect.as_str())
            .unwrap_or(snowlark_net::RedirectMode::Follow);
        n.cache = snowlark_net::CacheMode::parse(r.cache.as_str())
            .unwrap_or(snowlark_net::CacheMode::Default);
        n.referrer = match r.referrer.as_str() {
            "" => snowlark_net::Referrer::NoReferrer,
            "client" => snowlark_net::Referrer::Client,
            u => url::Url::parse(u)
                .map(snowlark_net::Referrer::Url)
                .unwrap_or(snowlark_net::Referrer::Client),
        };
        if let Some(p) = snowlark_net::ReferrerPolicy::parse(r.referrer_policy.as_str()) {
            n.referrer_policy = p;
        }
        n.integrity = r.integrity.clone();
        n.keepalive = r.keepalive;
        n.unsafe_request = true;
        n.initiator_type = "fetch".into();
        n
    }

    /// Fetch §5.6 `fetch(input, init)`.
    pub(crate) fn fetch_steps(
        &self,
        cx: &mut Cx,
        input: RequestOrString,
        init: RequestInit,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let request = match self.construct_request(cx, input, init) {
            Ok(r) => r,
            Err(e) => {
                p.reject(cx, e);
                return Ok(p);
            }
        };
        let signal = self.request(request).signal;
        if self.signal_aborted(signal) {
            let reason = {
                let scope = cx.scope();
                match &*self.obj(signal) {
                    Object::AbortSignal(d) => d.reason.as_ref().and_then(|r| r.get(scope)),
                    _ => None,
                }
            };
            let reason = reason.unwrap_or_else(|| {
                self.dom_exception_value(cx, "AbortError", "The operation was aborted")
            });
            p.reject_value(cx, &reason);
            return Ok(p);
        }
        let net_request = self.net_request(request);
        let client = self.request(request).client;
        let client_doc = match client {
            Some(doc) => doc,
            None => self.current_document(cx)?,
        };
        let realm = {
            let scope = cx.scope();
            let ctx = scope.get_current_context();
            Some(v8::Global::new(scope, ctx))
        };
        let owner = JobOwner::Fetch {
            promise: p.clone(),
            signal,
            realm,
        };
        match self.start_api_fetch(client_doc, net_request, owner) {
            Some(id) => {
                if let Object::AbortSignal(d) = &mut *self.obj_mut(signal) {
                    d.fetches.push(id);
                }
            }
            None => p.reject(cx, type_error("Failed to fetch")),
        }
        Ok(p)
    }

    /// Starts an API fetch of `doc` (its client, through the request policy); returns its job id.
    pub(crate) fn start_api_fetch(
        &self,
        doc: NodeId,
        request: snowlark_net::Request,
        owner: JobOwner,
    ) -> Option<u32> {
        let keepalive_bytes = self.keepalive_quota(doc, &request)?;
        let waker = self.waker();
        let stream = self.policy_fetch(doc, request, Some(Arc::new(move || waker.wake())), None)?;
        let id = {
            let mut n = self.next_api_job.get();
            n = n.wrapping_add(1).max(1);
            self.next_api_job.set(n);
            n
        };
        self.api_jobs.borrow_mut().push(ApiJob {
            id,
            owner,
            stream,
            head: None,
            body: Vec::new(),
            client: doc,
            keepalive_bytes,
            response: None,
            body_controller: None,
            consumers: Vec::new(),
        });
        Some(id)
    }

    /// Fetch, HTTP-network-or-cache fetch, the keepalive quota: for a keepalive `request` with a
    /// body, the sum of its length and the bodies of `doc`'s keepalive fetches not done yet may not
    /// be over 64 KiB (`None`: a network error). Otherwise what the request counts against it. As
    /// Blink's `ResourceFetcher::StartLoad`, which takes the bytes when the load starts, and
    /// `HandleLoaderFinish`, which gives them back when it ends; a job ends at its last event.
    pub(crate) fn keepalive_quota(
        &self,
        doc: NodeId,
        request: &snowlark_net::Request,
    ) -> Option<usize> {
        let size = match &request.body {
            Some(body) if request.keepalive => body.len(),
            _ => return Some(0),
        };
        let inflight: usize = self
            .api_jobs
            .borrow()
            .iter()
            .filter(|j| j.client == doc)
            .map(|j| j.keepalive_bytes)
            .sum();
        (size <= KEEPALIVE_INFLIGHT_QUOTA.saturating_sub(inflight)).then_some(size)
    }

    /// Fetch's "abort the fetch() call" for the job with `id`.
    pub(crate) fn abort_api_fetch(&self, cx: &mut Cx, id: u32, reason: Option<JsValue>) {
        let job = {
            let mut jobs = self.api_jobs.borrow_mut();
            jobs.iter().position(|j| j.id == id).map(|i| jobs.remove(i))
        };
        // (dropping the job drops its network stream, which ends the fetch and closes its connection)
        let Some(job) = job else { return };
        let JobOwner::Fetch { promise, realm, .. } = &job.owner else {
            return;
        };
        let (promise, realm) = (promise.clone(), realm.clone());
        self.in_realm(cx, realm, |cx| {
            let reason = reason.unwrap_or_else(|| {
                self.dom_exception_value(cx, "AbortError", "The operation was aborted")
            });
            match job.response {
                // The promise was resolved with the Response: "if response's body is non-null
                // and is readable, then error response's body with error".
                Some(resp) => self.end_net_body(cx, job, resp, Err(reason)),
                None => promise.reject_value(cx, &reason),
            }
        });
    }

    /// The end of a fetched Response's body (`job` is its fetch, taken out of the job list): its
    /// bytes, or the error its body errors with. A stream script took closes or errors; else the
    /// body becomes its bytes (or an errored stream) and the reads waiting for it settle.
    fn end_net_body(&self, cx: &mut Cx, job: ApiJob, resp: Handle, r: Result<(), JsValue>) {
        if let Some(c) = job.body_controller {
            match r {
                Ok(()) => self.readable_controller_close(cx, c),
                Err(e) => self.readable_controller_error(cx, c, e),
            }
            return;
        }
        match r {
            Ok(()) => {
                let bytes = Rc::new(job.body);
                {
                    let mut resp = self.response(resp);
                    resp.body.net = None;
                    resp.body.bytes = Some(bytes.clone());
                }
                for consume in job.consumers {
                    consume(self, cx, Ok(bytes.to_vec()));
                }
            }
            Err(e) => {
                let s = self.create_readable_stream(
                    cx,
                    Rc::new(NetBodySource { job: job.id }),
                    0.0,
                    true,
                );
                {
                    let mut resp = self.response(resp);
                    resp.body.net = None;
                    resp.body.stream = Some(s);
                }
                if let Some(c) = self.rs(s, |s| s.controller) {
                    self.readable_controller_error(cx, c, e.clone());
                }
                for consume in job.consumers {
                    consume(self, cx, Err(e.clone()));
                }
            }
        }
    }

    /// A fetched Response's body stream, made when script first asks for it while the body is
    /// still arriving (`id`: its job): the bytes so far are its first chunk, the rest are enqueued
    /// as they arrive.
    fn net_body_stream(&self, cx: &mut Cx, resp: Handle, id: u32) -> Handle {
        let s = self.create_readable_stream(cx, Rc::new(NetBodySource { job: id }), 0.0, true);
        let c = self
            .rs(s, |s| s.controller)
            .expect("a stream set up has a controller");
        {
            let mut r = self.response(resp);
            r.body.net = None;
            r.body.stream = Some(s);
        }
        let so_far = {
            let mut jobs = self.api_jobs.borrow_mut();
            match jobs.iter_mut().find(|j| j.id == id) {
                Some(j) => {
                    j.body_controller = Some(c);
                    Some(std::mem::take(&mut j.body))
                }
                None => None,
            }
        };
        match so_far {
            Some(bytes) if !bytes.is_empty() => {
                let chunk = super::streams::readable::uint8_value(cx, bytes);
                let _ = self.readable_controller_enqueue(cx, c, chunk);
            }
            Some(_) => {}
            // (the job is gone: its realm's document was unloaded)
            None => {
                let e = super::streams::type_error(cx, "The fetch was terminated");
                self.readable_controller_error(cx, c, e);
            }
        }
        s
    }

    /// The realm of a `fetch()` job and its body stream's controller, when script took the
    /// stream of its Response's body.
    fn streaming_job(&self, job: &ApiJob) -> Option<(Option<v8::Global<v8::Context>>, Handle)> {
        match (&job.owner, job.body_controller) {
            (JobOwner::Fetch { realm, .. }, Some(c)) => Some((realm.clone(), c)),
            _ => None,
        }
    }

    /// Fetch §5.6 "fetch()", "process response" for the job at `i`: resolves its promise with a
    /// Response whose body is the job's bytes as they arrive (`BodyData::net`). A network error
    /// waits for the job's end to reject.
    fn fetch_response_head(&self, cx: &mut Cx, i: usize, head: &ResponseHead) {
        if head.response_type == snowlark_net::ResponseType::Error {
            return;
        }
        let (id, promise, realm) = {
            let jobs = self.api_jobs.borrow();
            match &jobs[i].owner {
                JobOwner::Fetch { promise, realm, .. } => {
                    (jobs[i].id, promise.clone(), realm.clone())
                }
                _ => return,
            }
        };
        let (resp, has_body) = self.in_realm(cx, realm, |cx| {
            let resp = self.response_from_head(head, Vec::new());
            let has_body = {
                let mut r = self.response(resp);
                let has_body = r.body.bytes.take().is_some();
                if has_body {
                    r.body.net = Some(id);
                }
                has_body
            };
            promise.resolve(cx, resp);
            (resp, has_body)
        });
        let mut jobs = self.api_jobs.borrow_mut();
        let Some(i) = jobs.iter().position(|j| j.id == id) else {
            return;
        };
        if has_body {
            jobs[i].response = Some(resp);
        } else {
            // A null body: nothing more to give script; the job ends here.
            jobs.remove(i);
        }
    }

    /// Takes the network events of script fetches. Returns whether anything happened.
    pub(crate) fn poll_api_fetches(&self, cx: &mut Cx) -> bool {
        let mut progressed = false;
        let mut i = 0;
        loop {
            let event = {
                let mut jobs = self.api_jobs.borrow_mut();
                if i >= jobs.len() {
                    break;
                }
                jobs[i].stream.try_next()
            };
            let Some(event) = event else {
                i += 1;
                continue;
            };
            progressed = true;
            match event {
                FetchEvent::Violation(_) => {}
                FetchEvent::Head(h) => {
                    let (id, xhr, fetch) = {
                        let mut jobs = self.api_jobs.borrow_mut();
                        jobs[i].head = Some(h.clone());
                        let (xhr, fetch) = match jobs[i].owner {
                            JobOwner::Xhr(x) => (Some(x), false),
                            JobOwner::Fetch { .. } => (None, true),
                            JobOwner::Beacon => (None, false),
                        };
                        (jobs[i].id, xhr, fetch)
                    };
                    if let Some(x) = xhr {
                        self.xhr_head(cx, x, id, h);
                    } else if fetch {
                        self.fetch_response_head(cx, i, &h);
                    }
                }
                FetchEvent::Body(b) => {
                    let (id, xhr, stream) = {
                        let mut jobs = self.api_jobs.borrow_mut();
                        let stream = self.streaming_job(&jobs[i]);
                        if stream.is_none() {
                            jobs[i].body.extend_from_slice(&b);
                        }
                        let xhr = match jobs[i].owner {
                            JobOwner::Xhr(x) => Some(x),
                            _ => None,
                        };
                        (jobs[i].id, xhr, stream)
                    };
                    if let Some(x) = xhr {
                        self.xhr_body(cx, x, id, &b);
                    } else if let Some((realm, c)) = stream {
                        if !b.is_empty() {
                            self.in_realm(cx, realm, |cx| {
                                let chunk = super::streams::readable::uint8_value(cx, b.to_vec());
                                let _ = self.readable_controller_enqueue(cx, c, chunk);
                            });
                        }
                    }
                }
                FetchEvent::Done => {
                    let job = self.api_jobs.borrow_mut().remove(i);
                    self.api_fetch_done(cx, job, Ok(()));
                }
                FetchEvent::Error(e) => {
                    let job = self.api_jobs.borrow_mut().remove(i);
                    self.api_fetch_done(cx, job, Err(e.to_string()));
                }
            }
        }
        progressed
    }

    fn api_fetch_done(&self, cx: &mut Cx, job: ApiJob, r: Result<(), String>) {
        // (a request that succeeded may have been a sign-in by script: the password manager
        // watches the login form the user typed in from now on)
        if r.is_ok()
            && !matches!(job.owner, JobOwner::Beacon)
            && job
                .head
                .as_ref()
                .is_some_and(|h| (200..300).contains(&h.status))
        {
            self.password_submission_may_follow(None);
        }
        if let (Some(resp), JobOwner::Fetch { realm, .. }) = (job.response, &job.owner) {
            // The Response is script's already: its body ends, or errors with a TypeError when
            // the fetch failed after its head.
            let realm = realm.clone();
            self.in_realm(cx, realm, |cx| {
                let r = r.map_err(|_| super::streams::type_error(cx, "Failed to fetch"));
                self.end_net_body(cx, job, resp, r);
            });
            return;
        }
        match job.owner {
            JobOwner::Beacon => {}
            JobOwner::Xhr(x) => self.xhr_done(cx, x, job.id, job.head, job.body, r),
            JobOwner::Fetch {
                promise,
                signal,
                realm,
            } => {
                let _ = signal;
                self.in_realm(cx, realm, |cx| match (r, job.head) {
                    (Ok(()), Some(head))
                        if head.response_type != snowlark_net::ResponseType::Error =>
                    {
                        let resp = self.response_from_head(&head, job.body);
                        promise.resolve(cx, resp);
                    }
                    _ => promise.reject(cx, type_error("Failed to fetch")),
                });
            }
        }
    }

    /// A Response object for a response from the network (the filtered response's view).
    pub(crate) fn response_from_head(&self, head: &ResponseHead, body: Vec<u8>) -> Handle {
        use snowlark_net::ResponseType as T;
        let type_ = match head.response_type {
            T::Basic => ResponseType::Basic,
            T::Cors => ResponseType::Cors,
            T::Default => ResponseType::Default,
            T::Error => ResponseType::Error,
            T::Opaque => ResponseType::Opaque,
            T::OpaqueRedirect => ResponseType::Opaqueredirect,
        };
        let opaque = matches!(type_, ResponseType::Opaque | ResponseType::Opaqueredirect);
        let (status, status_text) = if opaque {
            (0, String::new())
        } else {
            (head.status, head.status_text.clone())
        };
        let body = (!opaque && !is_null_body_status(status) && !body.is_empty()
            || (!opaque && !is_null_body_status(status)))
        .then_some(body);
        let h = self.new_response(type_, status, &status_text, HeadersGuard::Immutable, body);
        let headers = self.response(h).headers;
        self.headers_of(headers).list = filtered_headers(head);
        // An opaque filtered response's URL list is empty; an opaque-redirect filtered response
        // keeps its internal response's (Fetch §2.2.6), so its `url` is the request's.
        if !matches!(type_, ResponseType::Opaque) {
            self.response(h).url_list = head.url_list.clone();
        }
        h
    }
}

/// The underlying source of a fetched Response's body stream: the network pushes its bytes as they
/// arrive; cancelling the stream aborts the fetch (Fetch §5.6 "fetch()": the stream is set up with
/// byte reading support and a cancel algorithm that aborts the fetch controller).
struct NetBodySource {
    job: u32,
}

impl super::streams::readable::NativeSource for NetBodySource {
    fn cancel(&self, e: &Engine, cx: &mut Cx, _reason: JsValue) -> JsValue {
        // Dropping the job drops its network stream: the fetch ends and its connection closes.
        let job = {
            let mut jobs = e.api_jobs.borrow_mut();
            jobs.iter()
                .position(|j| j.id == self.job)
                .map(|i| jobs.remove(i))
        };
        drop(job);
        super::streams::Deferred::resolved_undefined(cx).value(cx)
    }
}

/// The header list a filtered response exposes (Fetch §2.2.6).
pub(crate) fn filtered_headers(head: &ResponseHead) -> snowlark_net::HeaderList {
    use snowlark_net::ResponseType as T;
    let mut out = snowlark_net::HeaderList::new();
    match head.response_type {
        T::Opaque | T::OpaqueRedirect | T::Error => {}
        T::Cors => {
            let safelisted = [
                "cache-control",
                "content-language",
                "content-length",
                "content-type",
                "expires",
                "last-modified",
                "pragma",
            ];
            let star = !head.request_includes_credentials
                && head.cors_exposed_header_names.iter().any(|n| n == "*");
            for (k, v) in head.headers.iter() {
                let lk = k.to_ascii_lowercase();
                if lk == "set-cookie" || lk == "set-cookie2" {
                    continue;
                }
                if safelisted.contains(&lk.as_str())
                    || star
                    || head
                        .cors_exposed_header_names
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(k))
                {
                    out.append(k, v);
                }
            }
        }
        _ => {
            for (k, v) in head.headers.iter() {
                let lk = k.to_ascii_lowercase();
                if lk != "set-cookie" && lk != "set-cookie2" {
                    out.append(k, v);
                }
            }
        }
    }
    out
}

impl HeadersImpl for Engine {
    fn pair_at(&self, this: Handle, index: u32) -> Option<(String, String)> {
        self.headers_of(this)
            .entries()
            .into_iter()
            .nth(index as usize)
    }
    fn append(&self, _cx: &mut Cx, this: Handle, name: &str, value: &str) -> rt::Result<()> {
        self.headers_of(this)
            .append(name, value)
            .map_err(headers_error)
    }
    fn delete(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<()> {
        self.headers_of(this).delete(name).map_err(headers_error)
    }
    fn get(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<Option<DomStr<'_>>> {
        Ok(self
            .headers_of(this)
            .get(name)
            .map_err(headers_error)?
            .map(DomStr::Owned))
    }
    fn get_set_cookie(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<String>> {
        Ok(self.headers_of(this).get_set_cookie())
    }
    fn has(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<bool> {
        self.headers_of(this).has(name).map_err(headers_error)
    }
    fn set(&self, _cx: &mut Cx, this: Handle, name: &str, value: &str) -> rt::Result<()> {
        self.headers_of(this)
            .set(name, value)
            .map_err(headers_error)
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        init: Option<StringSequenceSequenceOrStringRecord>,
    ) -> rt::Result<Handle> {
        let h = self.new_headers(HeadersGuard::None);
        if let Some(init) = init {
            self.fill_headers(h, init)?;
        }
        Ok(h)
    }
}

impl RequestImpl for Engine {
    fn method(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.request(this).method.clone())
    }
    fn url(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.request(this).url.to_string())
    }
    fn headers(&self, this: Handle) -> Handle {
        self.request(this).headers
    }
    fn destination(&self, this: Handle) -> RequestDestination {
        self.request(this).destination
    }
    fn referrer(&self, this: Handle) -> DomStr<'_> {
        let r = self.request(this).referrer.clone();
        DomStr::Owned(match r.as_str() {
            "client" => "about:client".into(),
            _ => r,
        })
    }
    fn referrer_policy(&self, this: Handle) -> ReferrerPolicy {
        self.request(this).referrer_policy
    }
    fn mode(&self, this: Handle) -> RequestMode {
        self.request(this).mode
    }
    fn credentials(&self, this: Handle) -> RequestCredentials {
        self.request(this).credentials
    }
    fn cache(&self, this: Handle) -> RequestCache {
        self.request(this).cache
    }
    fn redirect(&self, this: Handle) -> RequestRedirect {
        self.request(this).redirect
    }
    fn integrity(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.request(this).integrity.clone())
    }
    fn keepalive(&self, this: Handle) -> bool {
        self.request(this).keepalive
    }
    fn is_reload_navigation(&self, _this: Handle) -> bool {
        false
    }
    fn is_history_navigation(&self, _this: Handle) -> bool {
        false
    }
    fn signal(&self, this: Handle) -> Handle {
        self.request(this).signal
    }
    fn duplex(&self, _this: Handle) -> RequestDuplex {
        RequestDuplex::Half
    }
    fn target_address_space(&self, _this: Handle) -> IPAddressSpace {
        IPAddressSpace::Public
    }
    fn clone(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        // Taken before the constructor runs: it marks an input request's body as used.
        let mut body = self.request(this).body.clone();
        if body.used
            || body
                .stream
                .is_some_and(|s| self.readable_locked(s) || self.readable_disturbed(s))
        {
            return Err(type_error("Request body is already used"));
        }
        let h =
            self.construct_request(cx, RequestOrString::Request(this), RequestInit::default())?;
        // Cloning does not consume the original's body.
        self.request(this).body.used = false;
        // The bytes are shared; a stream made from them stays with the original, and the clone
        // makes its own when asked.
        if body.bytes.is_some() {
            body.stream = None;
        }
        self.request(h).body = body;
        let src_headers = self.request(this).headers;
        let guard = self.headers_of(src_headers).guard;
        let headers = self.request(h).headers;
        self.headers_of(headers).guard = guard;
        Ok(h)
    }
    fn constructor(
        &self,
        cx: &mut Cx,
        input: RequestOrString,
        init: RequestInit,
    ) -> rt::Result<Handle> {
        self.construct_request(cx, input, init)
    }
}

impl ResponseImpl for Engine {
    fn type_(&self, this: Handle) -> ResponseType {
        self.response(this).type_
    }
    fn url(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.response(this)
                .url_list
                .last()
                .map(|u| {
                    let mut u = u.clone();
                    u.set_fragment(None);
                    u.to_string()
                })
                .unwrap_or_default(),
        )
    }
    fn redirected(&self, this: Handle) -> bool {
        self.response(this).url_list.len() > 1
    }
    fn status(&self, this: Handle) -> u16 {
        self.response(this).status
    }
    fn ok(&self, this: Handle) -> bool {
        (200..=299).contains(&self.response(this).status)
    }
    fn status_text(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.response(this).status_text.clone())
    }
    fn headers(&self, this: Handle) -> Handle {
        self.response(this).headers
    }
    fn static_error(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        Ok(self.new_response(ResponseType::Error, 0, "", HeadersGuard::Immutable, None))
    }
    fn static_redirect(&self, cx: &mut Cx, url: &str, status: u16) -> rt::Result<Handle> {
        let base = self.current_base_url(cx)?;
        let u = base
            .join(url)
            .map_err(|_| type_error(format!("Failed to parse URL from {url}")))?;
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Err(rt::Error::Range("Invalid redirect status".into()));
        }
        let h = self.new_response(
            ResponseType::Default,
            status,
            "",
            HeadersGuard::Immutable,
            None,
        );
        let headers = self.response(h).headers;
        self.headers_of(headers).list.set("Location", u.as_str());
        Ok(h)
    }
    fn static_json(&self, cx: &mut Cx, data: JsValue, init: ResponseInit) -> rt::Result<Handle> {
        let text = {
            let scope = cx.scope();
            v8::tc_scope!(let tc, scope);
            let v = v8::Local::new(tc, data.global());
            match v8::json::stringify(tc, v) {
                Some(s) if !v.is_undefined() && !v.is_function() && !v.is_symbol() => {
                    s.to_rust_string_lossy(tc)
                }
                _ => {
                    if tc.has_caught() {
                        tc.rethrow();
                        return Err(rt::Error::Pending);
                    }
                    return Err(type_error("The data is not JSON-serializable"));
                }
            }
        };
        let h = self.new_response(ResponseType::Default, 200, "", HeadersGuard::Response, None);
        self.initialize_response(
            h,
            init,
            Some((text.into_bytes(), Some("application/json".into()))),
        )?;
        Ok(h)
    }
    fn clone(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        // A stream-only body is teed: one branch stays, the other goes to the clone. A body still
        // arriving becomes a stream first.
        let arriving = {
            let r = self.response(this);
            r.body.net.is_some() && !r.body.used
        };
        if arriving {
            self.body_stream(cx, this);
        }
        let (stream, has_bytes) = {
            let r = self.response(this);
            (r.body.stream, r.body.bytes.is_some())
        };
        let mut clone_stream = None;
        if let Some(s) = stream {
            if self.readable_locked(s) || self.readable_disturbed(s) {
                return Err(type_error("Response body is already used"));
            }
            if !has_bytes {
                let (a, b) = self.readable_tee(cx, s, true)?;
                self.response(this).body.stream = Some(a);
                clone_stream = Some(b);
            }
        }
        let (mut copy, src_headers) = {
            let r = self.response(this);
            if r.body.used {
                return Err(type_error("Response body is already used"));
            }
            (
                ResponseData {
                    type_: r.type_,
                    url_list: r.url_list.clone(),
                    status: r.status,
                    status_text: r.status_text.clone(),
                    headers: r.headers,
                    body: r.body.clone(),
                },
                r.headers,
            )
        };
        let (guard, list) = {
            let h = self.headers_of(src_headers);
            (h.guard, h.list.clone())
        };
        let headers = self.new_headers(guard);
        self.headers_of(headers).list = list;
        copy.headers = headers;
        copy.body.stream = clone_stream;
        Ok(self.alloc(Object::Response(Box::new(copy))))
    }
    fn constructor(
        &self,
        cx: &mut Cx,
        body: Option<BodyInit>,
        init: ResponseInit,
    ) -> rt::Result<Handle> {
        let h = self.new_response(ResponseType::Default, 200, "", HeadersGuard::Response, None);
        if let Some(BodyInit::ReadableStream(s)) = body {
            // Fetch §5.2 "extract a body" from a stream: unread and unlocked.
            if self.readable_locked(s) || self.readable_disturbed(s) {
                return Err(type_error("The stream is locked or disturbed"));
            }
            if is_null_body_status(init.status) {
                return Err(type_error(
                    "Response with null body status cannot have body",
                ));
            }
            self.initialize_response(h, init, None)?;
            self.response(h).body.stream = Some(s);
            return Ok(h);
        }
        let extracted = body.map(|b| self.extract_body(cx, b));
        self.initialize_response(h, init, extracted)?;
        Ok(h)
    }
}

impl Engine {
    /// The body's stream, made on first access (Fetch §5.3 `body`): `None` for a null body.
    fn body_stream(&self, cx: &mut Cx, h: Handle) -> Option<Handle> {
        let (body, _) = self.body_parts(h)?;
        if let Some(s) = body.stream {
            return Some(s);
        }
        if let Some(id) = body.net {
            if !body.used {
                return Some(self.net_body_stream(cx, h, id));
            }
            // A read already takes the bytes: a stream that is disturbed and locked, and never
            // fed (job 0 is no job).
            let s = self.create_readable_stream(cx, Rc::new(NetBodySource { job: 0 }), 0.0, true);
            let _ = self.acquire_default_reader(cx, s);
            self.rs(s, |s| s.disturbed = true);
            if let Object::Response(r) = &mut *self.obj_mut(h) {
                r.body.stream = Some(s);
            }
            return Some(s);
        }
        let bytes = body.bytes?;
        let s = self.create_readable_stream(
            cx,
            Rc::new(super::streams::readable::BytesSource {
                bytes: std::cell::RefCell::new(Some(bytes.to_vec())),
            }),
            0.0,
            true,
        );
        if body.used {
            // (a body read before its stream was asked for: the stream is disturbed and locked)
            let _ = self.acquire_default_reader(cx, s);
            self.rs(s, |s| s.disturbed = true);
        }
        match &mut *self.obj_mut(h) {
            Object::Request(r) => r.body.stream = Some(s),
            Object::Response(r) => r.body.stream = Some(s),
            _ => {}
        }
        Some(s)
    }
}

/// V8's streaming compilation of one `compileStreaming` or `instantiateStreaming` call, until it is
/// given the response's bytes or aborted.
type WasmStreaming = Rc<std::cell::RefCell<Option<v8::WasmStreaming<false>>>>;

/// Rejects the compilation's promise with `reason` (V8's "abort").
fn abort_wasm(cx: &mut Cx, streaming: &WasmStreaming, reason: &JsValue) {
    if let Some(s) = streaming.borrow_mut().take() {
        let scope = cx.scope();
        let reason = v8::Local::new(scope, reason.global());
        s.abort(Some(reason));
    }
}

/// Rejects the compilation's promise with a TypeError.
fn abort_wasm_type_error(cx: &mut Cx, streaming: &WasmStreaming, message: &str) {
    let scope = cx.scope();
    let Some(message) = v8::String::new(scope, message) else {
        return;
    };
    let error = v8::Exception::type_error(scope, message);
    let error = JsValue::new(scope, error);
    abort_wasm(cx, streaming, &error);
}

/// `WebAssembly.compileStreaming(source)` and `instantiateStreaming(source)`: the WebAssembly Web
/// API's "compile a potential WebAssembly response" (§3), with V8 compiling the bytes.
pub fn wasm_streaming_hook<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: v8::Local<'s, v8::Value>,
    streaming: v8::WasmStreaming<false>,
) {
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    let mut cx = Cx::new(scope);
    let source = JsValue::new(cx.scope(), source);
    let streaming: WasmStreaming = Rc::new(std::cell::RefCell::new(Some(streaming)));
    // "Upon fulfillment of source with value unwrappedSource" (a Response or a promise of one);
    // upon its rejection, the compilation is rejected with the reason.
    engine.upon(
        &mut cx,
        &source,
        Box::new(move |e, cx, r| match r {
            Ok(value) => e.compile_wasm_response(cx, &value, streaming),
            Err(reason) => abort_wasm(cx, &streaming, &reason),
        }),
    );
}

impl Engine {
    /// The steps of "compile a potential WebAssembly response" once the source is a value.
    fn compile_wasm_response(&self, cx: &mut Cx, value: &JsValue, streaming: WasmStreaming) {
        let v = v8::Local::new(cx.scope(), value.global());
        let Some(h) = rt::unwrap_value(v, ids::RESPONSE, ids::RESPONSE_END) else {
            abort_wasm_type_error(
                cx,
                &streaming,
                "WebAssembly: the argument is not a Response or a promise of one",
            );
            return;
        };
        let (type_, status, url, headers) = {
            let r = self.response(h);
            (r.type_, r.status, r.url_list.last().cloned(), r.headers)
        };
        // "Let mimeType be the result of getting `Content-Type` from response's header list";
        // without HTTP tab or space bytes at its ends, it must be `application/wasm` exactly
        // (ASCII case-insensitively: no parameters).
        let mime = self.headers_of(headers).get("content-type").ok().flatten();
        let wasm = mime.as_deref().is_some_and(|m| {
            m.trim_matches(|c| c == ' ' || c == '\t')
                .eq_ignore_ascii_case("application/wasm")
        });
        if !wasm {
            let message = match &mime {
                Some(m) => format!(
                    "WebAssembly: the response's MIME type is '{m}', not 'application/wasm'"
                ),
                None => "WebAssembly: the response has no MIME type, not 'application/wasm'".into(),
            };
            abort_wasm_type_error(cx, &streaming, &message);
            return;
        }
        // (CORS-same-origin: a basic, cors or default response)
        if !matches!(
            type_,
            ResponseType::Basic | ResponseType::Cors | ResponseType::Default
        ) {
            abort_wasm_type_error(
                cx,
                &streaming,
                "WebAssembly: the response is not CORS-same-origin",
            );
            return;
        }
        if !(200..=299).contains(&status) {
            abort_wasm_type_error(
                cx,
                &streaming,
                &format!("WebAssembly: the response's status is {status}, not an ok status"),
            );
            return;
        }
        // (the module's URL, for its functions' stack frames)
        if let Some(url) = url {
            if let Some(s) = streaming.borrow_mut().as_mut() {
                s.set_url(url.as_str());
            }
        }
        // "Consume response's body as an ArrayBuffer": its bytes go to the compilation; a body
        // already used, or failing as it arrives, rejects it.
        let fed = streaming.clone();
        let consumed = self.consume(cx, h, move |_, cx, bytes, _, p| {
            if let Some(mut s) = fed.borrow_mut().take() {
                s.on_bytes_received(&bytes);
                s.finish();
            }
            let u = super::streams::undefined(cx);
            p.resolve(cx, u);
            Ok(())
        });
        match consumed {
            Ok(p) => {
                let p = p.promise(cx);
                self.upon(
                    cx,
                    &p,
                    Box::new(move |_, cx, r| {
                        if let Err(reason) = r {
                            abort_wasm(cx, &streaming, &reason);
                        }
                    }),
                );
            }
            Err(_) => abort_wasm_type_error(cx, &streaming, "WebAssembly: the body cannot be read"),
        }
    }
}

impl BodyImpl for Engine {
    fn body(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        Ok(self.body_stream(cx, this))
    }
    fn body_used(&self, this: Handle) -> bool {
        self.body_parts(this)
            .is_some_and(|(b, _)| b.used || b.stream.is_some_and(|s| self.readable_disturbed(s)))
    }
    /// The body's text as a stream of strings: its stream through a UTF-8 TextDecoderStream.
    fn text_stream(&self, cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        if let Some(s) = self.body_parts(this).and_then(|(b, _)| b.stream) {
            if self.readable_locked(s) || self.readable_disturbed(s) {
                return Err(type_error("Body has already been consumed"));
            }
        }
        let decoder = snowlark_bindings::generated::TextDecoderStreamImpl::constructor(
            self,
            cx,
            "utf-8",
            snowlark_bindings::generated::TextDecoderOptions::default(),
        )?;
        let readable =
            snowlark_bindings::generated::GenericTransformStreamImpl::readable(self, decoder);
        let writable =
            snowlark_bindings::generated::GenericTransformStreamImpl::writable(self, decoder);
        match self.body_stream(cx, this) {
            Some(s) => {
                let p = self.readable_pipe_to(
                    cx,
                    s,
                    writable,
                    snowlark_bindings::generated::StreamPipeOptions::default(),
                )?;
                p.mark_handled(cx);
            }
            None => {
                let w = self.acquire_writer(cx, writable)?;
                let p = self.writer_close_with_error_propagation(cx, w);
                p.mark_handled(cx);
            }
        }
        Ok(readable)
    }
    fn array_buffer(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.consume(cx, this, |_, cx, bytes, _, p| {
            let ab = array_buffer(cx, bytes);
            p.resolve(cx, ab);
            Ok(())
        })
    }
    fn blob(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.consume(cx, this, |e, cx, bytes, mime, p| {
            let b = e.new_blob_with_exact_type(bytes, mime.as_deref().unwrap_or(""));
            p.resolve(cx, b);
            Ok(())
        })
    }
    fn bytes(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.consume(cx, this, |_, cx, bytes, _, p| {
            let a = uint8_array(cx, bytes)?;
            p.resolve(cx, a);
            Ok(())
        })
    }
    fn form_data(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.consume(cx, this, |e, cx, bytes, mime, p| {
            let f = e.parse_form_data(&bytes, mime.as_deref())?;
            p.resolve(cx, f);
            Ok(())
        })
    }
    fn json(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.consume(cx, this, |e, cx, bytes, _, p| {
            match e.parse_json(cx, &bytes) {
                Ok(v) => p.resolve(cx, v),
                Err(ex) => p.reject_value(cx, &ex),
            }
            Ok(())
        })
    }
    fn text(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        self.consume(cx, this, |_, cx, bytes, _, p| {
            let (t, _) = encoding_rs::UTF_8.decode_with_bom_removal(&bytes);
            p.resolve(cx, DomStr::Owned(t.into_owned()));
            Ok(())
        })
    }
}

impl FormDataImpl for Engine {
    fn pair_at(&self, this: Handle, index: u32) -> Option<(String, FileOrString)> {
        self.form_data(this).get(index as usize).map(|(k, v)| {
            (
                k.clone(),
                match v {
                    FormValue::Str(s) => FileOrString::String(s.clone()),
                    FormValue::File(f) => FileOrString::File(*f),
                },
            )
        })
    }
    fn append_name_value(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        value: &str,
    ) -> rt::Result<()> {
        self.form_data(this)
            .push((name.to_owned(), FormValue::Str(value.to_owned())));
        Ok(())
    }
    fn append_name_blob_value_filename(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        blob_value: Handle,
        filename: Option<&str>,
    ) -> rt::Result<()> {
        let f = self.form_file(blob_value, filename);
        self.form_data(this)
            .push((name.to_owned(), FormValue::File(f)));
        Ok(())
    }
    fn delete(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<()> {
        self.form_data(this).retain(|(k, _)| k != name);
        Ok(())
    }
    fn get(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<Option<FileOrString>> {
        Ok(self
            .form_data(this)
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| match v {
                FormValue::Str(s) => FileOrString::String(s.clone()),
                FormValue::File(f) => FileOrString::File(*f),
            }))
    }
    fn get_all(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<Vec<FileOrString>> {
        Ok(self
            .form_data(this)
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| match v {
                FormValue::Str(s) => FileOrString::String(s.clone()),
                FormValue::File(f) => FileOrString::File(*f),
            })
            .collect())
    }
    fn has(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<bool> {
        Ok(self.form_data(this).iter().any(|(k, _)| k == name))
    }
    fn set_name_value(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        value: &str,
    ) -> rt::Result<()> {
        self.form_set(this, name, FormValue::Str(value.to_owned()));
        Ok(())
    }
    fn set_name_blob_value_filename(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        blob_value: Handle,
        filename: Option<&str>,
    ) -> rt::Result<()> {
        let f = self.form_file(blob_value, filename);
        self.form_set(this, name, FormValue::File(f));
        Ok(())
    }
    fn constructor(
        &self,
        cx: &mut Cx,
        form: Option<Handle>,
        submitter: Option<Handle>,
    ) -> rt::Result<Handle> {
        let entries = match form {
            Some(f) => {
                let form = self.n(f);
                let submitter = submitter.map(|s| self.n(s));
                if let Some(s) = submitter {
                    let dom = self.dom();
                    let is_submit = super::forms::is_submit_button(&dom, s);
                    if !is_submit {
                        return Err(type_error("The submitter is not a submit button"));
                    }
                    if super::collections::form_owner(&dom, s) != Some(form) {
                        return Err(rt::Error::Dom(
                            rt::ExceptionName::NotFoundError,
                            "The submitter is not owned by the form".into(),
                        ));
                    }
                }
                self.construct_entry_list(cx, form, submitter, encoding_rs::UTF_8)
                    .ok_or_else(|| {
                        rt::Error::Dom(
                            rt::ExceptionName::InvalidStateError,
                            "The form is constructing its entry list".into(),
                        )
                    })?
            }
            None => Vec::new(),
        };
        Ok(self.alloc(Object::FormData(entries)))
    }
}

impl Engine {
    /// "Create an entry" with a Blob value: a File named `filename` (or the File's own name, or "blob").
    fn form_file(&self, blob: Handle, filename: Option<&str>) -> Handle {
        let is_file = self.blob_file_name(blob).is_some();
        if is_file && filename.is_none() {
            return blob;
        }
        let (bytes, t) = self.blob_contents(blob);
        let name = filename
            .map(str::to_owned)
            .or_else(|| self.blob_file_name(blob))
            .unwrap_or_else(|| "blob".into());
        let modified = match &*self.obj(blob) {
            Object::Blob(b) => b.file.as_ref().map(|(_, m)| *m),
            _ => None,
        }
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        });
        self.new_file(bytes.to_vec(), &t, &name, modified)
    }

    /// FormData `set`: replaces the first entry named `name`, removes the others.
    fn form_set(&self, this: Handle, name: &str, value: FormValue) {
        let mut f = self.form_data(this);
        match f.iter().position(|(k, _)| k == name) {
            Some(i) => {
                f[i].1 = value;
                let mut seen = false;
                let mut idx = 0;
                f.retain(|(k, _)| {
                    let keep = k != name || {
                        let first = !seen && idx == i;
                        if first {
                            seen = true;
                        }
                        first
                    };
                    idx += 1;
                    keep
                });
            }
            None => f.push((name.to_owned(), value)),
        }
    }

    /// HTML §4.10.21.4 "constructing the entry list": none while the form is constructing one;
    /// else the controls' entries, handed to the `formdata` event (bubbling, not cancelable) in a
    /// FormData whose entries the listeners may change, and returned as they are after it.
    pub(crate) fn construct_entry_list(
        &self,
        cx: &mut Cx,
        form: NodeId,
        submitter: Option<NodeId>,
        encoding: &'static encoding_rs::Encoding,
    ) -> Option<Vec<(String, FormValue)>> {
        if !self.html.borrow_mut().constructing_entry_list.insert(form) {
            return None;
        }
        let entries = self.form_entries(form, submitter, encoding);
        let form_data = self.alloc(Object::FormData(entries));
        let e = self.trusted_event(
            snowlark_bindings::InterfaceId::FormDataEvent,
            "formdata",
            true,
            false,
            super::events::EventExt::FormData(form_data),
        );
        self.dispatch(cx, e, super::node_handle(form), false);
        self.html.borrow_mut().constructing_entry_list.remove(&form);
        let entries = self.form_data(form_data).clone();
        Some(entries)
    }
}
