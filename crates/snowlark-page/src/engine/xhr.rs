// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `XMLHttpRequest` (XHR Standard): asynchronous requests through the event loop's script fetches,
//! synchronous ones by waiting on the network thread, the progress events, and the response in each
//! `responseType`.

use std::rc::Rc;

use snowlark_bindings::generated::{
    DocumentOrBlobOrArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataViewOrFormDataOrURLSearchParamsOrString as XhrBody,
    PrivateToken,
    ReadableStreamOrBlobOrArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataViewOrFormDataOrURLSearchParamsOrString as BodyInit,
    XMLHttpRequestEventTargetImpl, XMLHttpRequestImpl, XMLHttpRequestResponseType as RT,
    XMLHttpRequestUploadImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsValue};
use snowlark_dom::NodeId;
use snowlark_net::{FetchEvent, ResponseHead};

use super::blob::array_buffer;
use super::fetch::{filtered_headers, is_forbidden_method, normalize_method, JobOwner};
use super::{exception, node_handle, Engine, Object};

const UNSENT: u16 = 0;
const OPENED: u16 = 1;
const HEADERS_RECEIVED: u16 = 2;
const LOADING: u16 = 3;
const DONE: u16 = 4;

pub struct XhrData {
    /// The window whose document is the request's client (the XHR's relevant global).
    pub window: Option<Handle>,
    pub state: u16,
    pub send_flag: bool,
    pub sync: bool,
    pub method: String,
    pub url: Option<url::Url>,
    /// The URL's blob URL entry, held by `open()` ([`snowlark_net::Request::blob_url_entry`]).
    pub blob_url_entry: Option<u64>,
    pub headers: snowlark_net::HeaderList,
    pub timeout: u32,
    pub with_credentials: bool,
    pub upload: Handle,
    pub upload_complete: bool,
    pub upload_listener: bool,
    pub response_type: RT,
    pub override_mime: Option<String>,
    pub head: Option<ResponseHead>,
    pub body: Vec<u8>,
    /// The response is a network error (failure, abort, timeout).
    pub error: bool,
    pub job: Option<u32>,
    pub timeout_timer: Option<i32>,
    pub last_progress: f64,
    /// (engine/held.rs: kept by the realm's Window once its browsing context is discarded)
    pub response_value: Option<super::held::Held<v8::Value>>,
    pub response_document: Option<Option<NodeId>>,
    pub response_text: Option<Rc<String>>,
    /// Bumped by `open()` and `abort()` so events of an old request are dropped.
    pub generation: u32,
}

fn token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

impl Engine {
    fn xhr(&self, h: Handle) -> std::cell::RefMut<'_, XhrData> {
        std::cell::RefMut::map(self.obj_mut(h), |o| match o {
            Object::Xhr(x) => &mut **x,
            _ => panic!("not an XMLHttpRequest"),
        })
    }

    /// The final MIME type (XHR §3.6.?): the override, the response's Content-Type, or `text/xml`.
    fn xhr_final_mime(&self, h: Handle) -> Option<snowlark_net::mime::MimeType> {
        let x = self.xhr(h);
        if let Some(m) = &x.override_mime {
            return snowlark_net::mime::MimeType::parse(m);
        }
        let ct = x
            .head
            .as_ref()
            .and_then(|hd| snowlark_net::mime::extract_mime_type(&filtered_headers(hd)));
        ct.or_else(|| snowlark_net::mime::MimeType::parse("text/xml"))
    }

    /// The response body decoded as text ("text response").
    fn xhr_text(&self, h: Handle) -> Rc<String> {
        if let Some(t) = self.xhr(h).response_text.clone() {
            return t;
        }
        let mime = self.xhr_final_mime(h);
        let charset = mime
            .as_ref()
            .and_then(|m| m.parameter("charset").map(str::to_owned));
        let (body, response_type) = {
            let x = self.xhr(h);
            (x.body.clone(), x.response_type)
        };
        let mut enc = charset.and_then(|c| encoding_rs::Encoding::for_label(c.trim().as_bytes()));
        let is_xml = mime.as_ref().is_some_and(|m| {
            let e = m.essence();
            e == "text/xml" || e == "application/xml" || e.ends_with("+xml")
        });
        if enc.is_none() && response_type == RT::Empty && is_xml {
            enc = crate::loader::xml_declared_encoding(&body)
                .and_then(|c| encoding_rs::Encoding::for_label(c.trim().as_bytes()));
        }
        let enc = enc.unwrap_or(encoding_rs::UTF_8);
        let (text, _, _) = enc.decode(&body);
        let t = Rc::new(text.into_owned());
        let done = self.xhr(h).state == DONE;
        if done {
            self.xhr(h).response_text = Some(t.clone());
        }
        t
    }

    /// The document response (XHR §3.6.9).
    fn xhr_document(&self, cx: &mut Cx, h: Handle) -> Option<NodeId> {
        if let Some(d) = self.xhr(h).response_document {
            return d;
        }
        let mime = self.xhr_final_mime(h);
        let essence = mime.as_ref().map(|m| m.essence()).unwrap_or_default();
        let response_type = self.xhr(h).response_type;
        let is_html = essence == "text/html";
        let is_xml =
            essence == "text/xml" || essence == "application/xml" || essence.ends_with("+xml");
        let doc = if (is_html && response_type == RT::Document) || is_xml {
            let url = self
                .xhr(h)
                .head
                .as_ref()
                .and_then(|hd| hd.url().cloned())
                .map(|u| u.to_string())
                .unwrap_or_else(|| "about:blank".into());
            if is_html {
                let charset = mime
                    .as_ref()
                    .and_then(|m| m.parameter("charset").map(str::to_owned));
                let body = self.xhr(h).body.clone();
                let enc = charset
                    .and_then(|c| encoding_rs::Encoding::for_label(c.trim().as_bytes()))
                    .unwrap_or(encoding_rs::UTF_8);
                let (text, enc, _) = enc.decode(&body);
                let doc = self.dom_mut().parse_html_str_with(
                    &text,
                    snowlark_dom::ParseOptions {
                        scripting_enabled: false,
                        iframe_srcdoc: false,
                    },
                    false,
                );
                let mut dom = self.dom_mut();
                dom.set_encoding(doc, enc);
                dom.set_document_url(doc, &url);
                dom.set_content_type(doc, "text/html");
                Some(doc)
            } else {
                let text = self.xhr_text(h);
                let (doc, r) = self.dom_mut().parse_xml_str(&text);
                if r.is_err() {
                    None
                } else {
                    let mut dom = self.dom_mut();
                    dom.set_document_url(doc, &url);
                    dom.set_content_type(doc, &essence);
                    drop(dom);
                    self.xml_documents.borrow_mut().insert(doc);
                    Some(doc)
                }
            }
        } else {
            None
        };
        self.process_reactions(cx);
        self.xhr(h).response_document = Some(doc);
        doc
    }

    /// XHR "request error steps" for `event` (error, abort or timeout).
    fn xhr_request_error(&self, cx: &mut Cx, h: Handle, event: &str) {
        let (upload, fire_upload) = {
            let mut x = self.xhr(h);
            x.state = DONE;
            x.send_flag = false;
            x.error = true;
            x.head = None;
            x.body.clear();
            let fire_upload = !x.upload_complete && x.upload_listener;
            x.upload_complete = true;
            (x.upload, fire_upload)
        };
        if self.xhr(h).sync {
            return;
        }
        self.fire_event(cx, h, "readystatechange", false, false);
        if fire_upload {
            self.fire_progress(cx, upload, event, 0, 0, false);
            self.fire_progress(cx, upload, "loadend", 0, 0, false);
        }
        self.fire_progress(cx, h, event, 0, 0, false);
        self.fire_progress(cx, h, "loadend", 0, 0, false);
    }

    /// The response head arrived (XHR "process response").
    pub(crate) fn xhr_head(&self, cx: &mut Cx, h: Handle, job: u32, head: ResponseHead) {
        if self.xhr(h).job != Some(job) {
            return;
        }
        if head.response_type == snowlark_net::ResponseType::Error {
            return;
        }
        let (upload, fire_upload, length) = {
            let mut x = self.xhr(h);
            x.head = Some(head);
            let fire_upload = !x.upload_complete && x.upload_listener;
            x.upload_complete = true;
            (x.upload, fire_upload, 0)
        };
        if fire_upload {
            self.fire_progress(cx, upload, "progress", length, length, true);
            self.fire_progress(cx, upload, "load", length, length, true);
            self.fire_progress(cx, upload, "loadend", length, length, true);
        }
        if self.xhr(h).job != Some(job) {
            return;
        }
        self.xhr(h).state = HEADERS_RECEIVED;
        self.fire_event(cx, h, "readystatechange", false, false);
    }

    /// A chunk of the response body arrived (XHR "process body chunk").
    pub(crate) fn xhr_body(&self, cx: &mut Cx, h: Handle, job: u32, bytes: &[u8]) {
        let now = self.now();
        let (fire, loaded, total) = {
            let mut x = self.xhr(h);
            if x.job != Some(job) {
                return;
            }
            x.body.extend_from_slice(bytes);
            let first = x.state == HEADERS_RECEIVED;
            let fire = first || now - x.last_progress >= 50.0;
            if fire {
                x.last_progress = now;
            }
            if x.state == HEADERS_RECEIVED {
                x.state = LOADING;
            }
            let total = x
                .head
                .as_ref()
                .and_then(|hd| hd.headers.get("content-length"))
                .and_then(|v| v.trim().parse::<u64>().ok());
            (fire, x.body.len() as u64, total)
        };
        if fire {
            self.fire_event(cx, h, "readystatechange", false, false);
            self.fire_progress(
                cx,
                h,
                "progress",
                loaded,
                total.unwrap_or(0),
                total.is_some(),
            );
        }
    }

    /// The fetch finished (XHR "handle response end-of-body") or failed.
    pub(crate) fn xhr_done(
        &self,
        cx: &mut Cx,
        h: Handle,
        job: u32,
        head: Option<ResponseHead>,
        body: Vec<u8>,
        r: Result<(), String>,
    ) {
        if self.xhr(h).job != Some(job) {
            return;
        }
        self.xhr(h).job = None;
        if let Some(t) = self.xhr(h).timeout_timer.take() {
            self.clear_timer(t);
        }
        let ok = r.is_ok()
            && head
                .as_ref()
                .is_some_and(|hd| hd.response_type != snowlark_net::ResponseType::Error);
        if !ok {
            self.xhr_request_error(cx, h, "error");
            return;
        }
        if self.xhr(h).head.is_none() {
            // A response that completed without a separate head event.
            if let Some(hd) = head.clone() {
                self.xhr_head(cx, h, job, hd);
                self.xhr(h).job = None;
            }
        }
        {
            let mut x = self.xhr(h);
            x.body = body;
        }
        let (transmitted, length) = {
            let x = self.xhr(h);
            let length = x
                .head
                .as_ref()
                .and_then(|hd| hd.headers.get("content-length"))
                .and_then(|v| v.trim().parse::<u64>().ok());
            (x.body.len() as u64, length)
        };
        if !self.xhr(h).sync {
            self.fire_progress(
                cx,
                h,
                "progress",
                transmitted,
                length.unwrap_or(0),
                length.is_some(),
            );
        }
        {
            let mut x = self.xhr(h);
            x.state = DONE;
            x.send_flag = false;
        }
        self.fire_event(cx, h, "readystatechange", false, false);
        self.fire_progress(
            cx,
            h,
            "load",
            transmitted,
            length.unwrap_or(0),
            length.is_some(),
        );
        self.fire_progress(
            cx,
            h,
            "loadend",
            transmitted,
            length.unwrap_or(0),
            length.is_some(),
        );
    }

    /// The request's body and Content-Type for `send(body)`.
    fn xhr_body_bytes(&self, cx: &mut Cx, body: XhrBody) -> (Vec<u8>, Option<String>) {
        use BodyInit as B;
        use XhrBody as X;
        let init = match body {
            X::Document(d) => {
                let doc = self.n(d);
                let html = self.dom().is_html_document(doc);
                let text = if html {
                    self.fragment_serialize(doc, false)
                } else {
                    self.dom().serialize_xml(doc, false).unwrap_or_default()
                };
                let t = if html {
                    "text/html;charset=UTF-8"
                } else {
                    "application/xml;charset=UTF-8"
                };
                return (text.into_bytes(), Some(t.into()));
            }
            X::Blob(b) => B::Blob(b),
            X::ArrayBuffer(o) => B::ArrayBuffer(o),
            X::Int8Array(o) => B::Int8Array(o),
            X::Int16Array(o) => B::Int16Array(o),
            X::Int32Array(o) => B::Int32Array(o),
            X::Uint8Array(o) => B::Uint8Array(o),
            X::Uint16Array(o) => B::Uint16Array(o),
            X::Uint32Array(o) => B::Uint32Array(o),
            X::Uint8ClampedArray(o) => B::Uint8ClampedArray(o),
            X::BigInt64Array(o) => B::BigInt64Array(o),
            X::BigUint64Array(o) => B::BigUint64Array(o),
            X::Float16Array(o) => B::Float16Array(o),
            X::Float32Array(o) => B::Float32Array(o),
            X::Float64Array(o) => B::Float64Array(o),
            X::DataView(o) => B::DataView(o),
            X::FormData(f) => B::FormData(f),
            X::URLSearchParams(p) => B::URLSearchParams(p),
            X::String(s) => B::String(s),
        };
        self.extract_body(cx, init)
    }

    fn xhr_terminate(&self, h: Handle) {
        let job = self.xhr(h).job.take();
        if let Some(id) = job {
            self.api_jobs.borrow_mut().retain(|j| j.id != id);
        }
        if let Some(t) = self.xhr(h).timeout_timer.take() {
            self.clear_timer(t);
        }
    }
}

impl XMLHttpRequestEventTargetImpl for Engine {}

impl XMLHttpRequestUploadImpl for Engine {}

impl XMLHttpRequestImpl for Engine {
    fn ready_state(&self, this: Handle) -> u16 {
        self.xhr(this).state
    }
    fn timeout(&self, this: Handle) -> u32 {
        self.xhr(this).timeout
    }
    fn set_timeout(&self, _cx: &mut Cx, this: Handle, value: u32) -> rt::Result<()> {
        let mut x = self.xhr(this);
        if x.sync && x.state == OPENED && x.window.is_some() {
            return Err(exception(
                ExceptionName::InvalidAccessError,
                "Timeouts are not supported for synchronous requests",
            ));
        }
        x.timeout = value;
        Ok(())
    }
    fn with_credentials(&self, this: Handle) -> bool {
        self.xhr(this).with_credentials
    }
    fn set_with_credentials(&self, _cx: &mut Cx, this: Handle, value: bool) -> rt::Result<()> {
        let mut x = self.xhr(this);
        if (x.state != UNSENT && x.state != OPENED) || x.send_flag {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "withCredentials can only be set before send()",
            ));
        }
        x.with_credentials = value;
        Ok(())
    }
    fn upload(&self, this: Handle) -> Handle {
        self.xhr(this).upload
    }
    fn response_url(&self, this: Handle) -> DomStr<'_> {
        let x = self.xhr(this);
        DomStr::Owned(
            x.head
                .as_ref()
                .and_then(|hd| hd.url().cloned())
                .filter(|_| !x.error)
                .map(|mut u| {
                    u.set_fragment(None);
                    u.to_string()
                })
                .unwrap_or_default(),
        )
    }
    fn status(&self, this: Handle) -> u16 {
        let x = self.xhr(this);
        match &x.head {
            Some(h) if !x.error => h.status,
            _ => 0,
        }
    }
    fn status_text(&self, this: Handle) -> DomStr<'_> {
        let x = self.xhr(this);
        DomStr::Owned(match &x.head {
            Some(h) if !x.error => h.status_text.clone(),
            _ => String::new(),
        })
    }
    fn response_type(&self, this: Handle) -> RT {
        self.xhr(this).response_type
    }
    fn set_response_type(&self, _cx: &mut Cx, this: Handle, value: RT) -> rt::Result<()> {
        let mut x = self.xhr(this);
        if x.state == LOADING || x.state == DONE {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "responseType cannot be set while loading or done",
            ));
        }
        if x.sync && x.state == OPENED && x.window.is_some() {
            return Err(exception(
                ExceptionName::InvalidAccessError,
                "responseType cannot be set for synchronous requests",
            ));
        }
        x.response_type = value;
        Ok(())
    }
    fn response(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        let (rt_, state, error) = {
            let x = self.xhr(this);
            (x.response_type, x.state, x.error)
        };
        if rt_ == RT::Empty || rt_ == RT::Text {
            let text = if state == LOADING || state == DONE {
                self.xhr_text(this)
            } else {
                Rc::new(String::new())
            };
            let scope = cx.scope();
            let s = v8::String::new(scope, &text).ok_or(rt::Error::Pending)?;
            return Ok(JsValue::new(scope, s.into()));
        }
        if state != DONE || error {
            return Ok(rt::null_value(cx.scope()));
        }
        let cached = self.xhr(this).response_value.clone();
        if let Some(v) = cached.and_then(|v| v.get(cx.scope())) {
            return Ok(v);
        }
        let v = match rt_ {
            RT::Arraybuffer => {
                let bytes = self.xhr(this).body.clone();
                let ab = array_buffer(cx, bytes);
                let scope = cx.scope();
                let l = v8::Local::new(scope, ab.global());
                JsValue::new(scope, l.into())
            }
            RT::Blob => {
                let bytes = self.xhr(this).body.clone();
                let t = self
                    .xhr_final_mime(this)
                    .map(|m| m.serialize())
                    .unwrap_or_default();
                let b = self.new_blob(bytes, &t);
                let scope = cx.scope();
                match rt::wrap(scope, b) {
                    Some(w) => JsValue::new(scope, w.into()),
                    None => rt::null_value(scope),
                }
            }
            RT::Document => match self.xhr_document(cx, this) {
                Some(d) => {
                    let scope = cx.scope();
                    match rt::wrap(scope, node_handle(d)) {
                        Some(w) => JsValue::new(scope, w.into()),
                        None => rt::null_value(scope),
                    }
                }
                None => rt::null_value(cx.scope()),
            },
            RT::Json => {
                let body = self.xhr(this).body.clone();
                match self.parse_json(cx, &body) {
                    Ok(v) => v,
                    Err(_) => rt::null_value(cx.scope()),
                }
            }
            RT::Empty | RT::Text => unreachable!("handled above"),
        };
        self.xhr(this).response_value = Some(super::held::Held::value(v.clone()));
        Ok(v)
    }
    fn response_text(&self, this: Handle) -> DomStr<'_> {
        let (rt_, state) = {
            let x = self.xhr(this);
            (x.response_type, x.state)
        };
        if (rt_ != RT::Empty && rt_ != RT::Text) || (state != LOADING && state != DONE) {
            return DomStr::Borrowed("");
        }
        DomStr::Owned((*self.xhr_text(this)).clone())
    }
    fn response_xml(&self, this: Handle) -> Option<Handle> {
        let (rt_, state, error) = {
            let x = self.xhr(this);
            (x.response_type, x.state, x.error)
        };
        if (rt_ != RT::Empty && rt_ != RT::Document) || state != DONE || error {
            return None;
        }
        // Parsing needs no script; reactions of the new nodes are processed on the next operation.
        if let Some(d) = self.xhr(this).response_document {
            return d.map(node_handle);
        }
        None.or_else(|| self.xhr_document_noscript(this))
            .map(node_handle)
    }
    fn open_method_url(
        &self,
        cx: &mut Cx,
        this: Handle,
        method: &str,
        url: &str,
    ) -> rt::Result<()> {
        self.open_method_url_async_username_password(cx, this, method, url, true, None, None)
    }
    fn open_method_url_async_username_password(
        &self,
        cx: &mut Cx,
        this: Handle,
        method: &str,
        url: &str,
        async_: bool,
        username: Option<&str>,
        password: Option<&str>,
    ) -> rt::Result<()> {
        // 2-4.
        if !token(method) {
            return Err(exception(ExceptionName::SyntaxError, "Invalid method"));
        }
        if is_forbidden_method(method) {
            return Err(exception(ExceptionName::SecurityError, "Forbidden method"));
        }
        let method = normalize_method(method);
        // 5-7.
        let window = self.xhr(this).window;
        let base = match window.and_then(|w| self.associated_document(w)) {
            Some(doc) => self.document_base_url(&self.dom(), doc),
            None => self.current_base_url(cx)?,
        };
        let mut url = base
            .join(url)
            .map_err(|_| exception(ExceptionName::SyntaxError, "Invalid URL"))?;
        // 8.
        if url.host().is_some() {
            if let Some(u) = username {
                let _ = url.set_username(u);
            }
            if let Some(p) = password {
                let _ = url.set_password(Some(p));
            }
        }
        // 9.
        {
            let x = self.xhr(this);
            if !async_ && x.window.is_some() && (x.timeout != 0 || x.response_type != RT::Empty) {
                return Err(exception(
                    ExceptionName::InvalidAccessError,
                    "Synchronous requests cannot have a timeout or responseType",
                ));
            }
        }
        // 10.
        self.xhr_terminate(this);
        // (the URL was parsed: its blob URL entry is what send() fetches)
        let blob_url_entry = self.hold_blob_url(&url);
        // 11-13.
        let fire = {
            let mut x = self.xhr(this);
            x.send_flag = false;
            x.upload_listener = false;
            x.method = method;
            x.url = Some(url);
            x.blob_url_entry = blob_url_entry;
            x.sync = !async_;
            x.headers = snowlark_net::HeaderList::new();
            x.head = None;
            x.body.clear();
            x.error = false;
            x.response_value = None;
            x.response_document = None;
            x.response_text = None;
            x.override_mime = None;
            x.generation += 1;
            let fire = x.state != OPENED;
            x.state = OPENED;
            fire
        };
        if fire {
            self.fire_event(cx, this, "readystatechange", false, false);
        }
        Ok(())
    }
    fn set_request_header(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        value: &str,
    ) -> rt::Result<()> {
        let mut x = self.xhr(this);
        if x.state != OPENED || x.send_flag {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "setRequestHeader requires an opened, unsent request",
            ));
        }
        let value = value.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
        if !snowlark_net::headers::is_header_name(name)
            || !snowlark_net::headers::is_header_value(value)
        {
            return Err(exception(ExceptionName::SyntaxError, "Invalid header"));
        }
        if snowlark_net::headers::is_forbidden_request_header(name, value) {
            return Ok(());
        }
        // 6. "Combine (name, value) in this's author request headers": one header, its values
        // joined with ", ".
        x.headers.combine(name, value);
        Ok(())
    }
    fn send(&self, cx: &mut Cx, this: Handle, body: Option<XhrBody>) -> rt::Result<()> {
        // 1-2.
        {
            let x = self.xhr(this);
            if x.state != OPENED || x.send_flag {
                return Err(exception(
                    ExceptionName::InvalidStateError,
                    "send() requires an opened, unsent request",
                ));
            }
        }
        // 3-4: the body.
        let method = self.xhr(this).method.clone();
        let body = if method == "GET" || method == "HEAD" {
            None
        } else {
            body
        };
        let text_body = matches!(body, Some(XhrBody::Document(_) | XhrBody::String(_)));
        let body = body.map(|b| self.xhr_body_bytes(cx, b));
        if let Some((_, extracted)) = &body {
            let mut x = self.xhr(this);
            match x.headers.get("content-type") {
                // 4.5: the author's Content-Type for a document or string body has every
                // charset parameter set to UTF-8 (the body is UTF-8 encoded).
                Some(author) if text_body => {
                    if let Some(mut record) = snowlark_net::mime::MimeType::parse(&author) {
                        if record
                            .parameter("charset")
                            .is_some_and(|c| !c.eq_ignore_ascii_case("UTF-8"))
                        {
                            record.set_parameter("charset", "UTF-8");
                            x.headers.set("Content-Type", &record.serialize());
                        }
                    }
                }
                Some(_) => {}
                None => {
                    if let Some(t) = extracted {
                        x.headers.append("Content-Type", t);
                    }
                }
            }
        }
        // 5-7: the request.
        let upload = self.xhr(this).upload;
        let upload_listener = self
            .listeners
            .borrow()
            .get(&upload)
            .is_some_and(|l| !l.is_empty());
        let (request, sync, timeout) = {
            let mut x = self.xhr(this);
            x.upload_listener = upload_listener;
            x.upload_complete = body.is_none();
            x.error = false;
            x.send_flag = true;
            let url = x.url.clone().expect("an opened request");
            let mut r = snowlark_net::Request::new(url);
            r.blob_url_entry = x.blob_url_entry;
            r.method = x.method.clone();
            r.headers = x.headers.clone();
            r.body = body
                .as_ref()
                .map(|(b, _)| snowlark_net::Body::new(b.clone()));
            r.mode = snowlark_net::Mode::Cors;
            r.credentials = if x.with_credentials {
                snowlark_net::CredentialsMode::Include
            } else {
                snowlark_net::CredentialsMode::SameOrigin
            };
            r.use_cors_preflight = upload_listener;
            r.unsafe_request = true;
            r.initiator_type = "xmlhttprequest".into();
            (r, x.sync, x.timeout)
        };
        let doc = match self
            .xhr(this)
            .window
            .and_then(|w| self.associated_document(w))
        {
            Some(doc) => doc,
            None => self.current_document(cx)?,
        };
        let mut request = request;
        request.client = Some(self.request_client(doc));
        if sync {
            return self.xhr_send_sync(cx, this, doc, request);
        }
        // 11: async.
        self.fire_progress(cx, this, "loadstart", 0, 0, false);
        let fire_upload = {
            let x = self.xhr(this);
            x.upload_listener && !x.upload_complete
        };
        if fire_upload {
            let len = body.as_ref().map(|(b, _)| b.len() as u64).unwrap_or(0);
            self.fire_progress(cx, upload, "loadstart", 0, len, true);
        }
        {
            let x = self.xhr(this);
            if x.state != OPENED || !x.send_flag {
                return Ok(());
            }
        }
        let job = self.start_api_fetch(doc, request, JobOwner::Xhr(this));
        match job {
            Some(id) => {
                self.xhr(this).job = Some(id);
                if timeout > 0 {
                    let steps: crate::event_loop::TaskFn =
                        Box::new(move |e: &Engine, cx: &mut Cx| {
                            if e.xhr(this).job == Some(id) {
                                e.xhr_terminate(this);
                                e.xhr_request_error(cx, this, "timeout");
                            }
                        });
                    let t = self.start_timer(
                        crate::event_loop::TimerHandler::Internal(Some(steps)),
                        timeout as f64,
                        false,
                        None,
                    );
                    self.xhr(this).timeout_timer = Some(t);
                }
            }
            None => {
                // No network: the request fails in a task.
                self.queue_task(crate::event_loop::TaskSource::Networking, move |e, cx| {
                    e.xhr_request_error(cx, this, "error");
                });
            }
        }
        Ok(())
    }
    fn abort(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.xhr_terminate(this);
        let state = {
            let x = self.xhr(this);
            (x.state, x.send_flag)
        };
        if (state.0 == OPENED && state.1) || state.0 == HEADERS_RECEIVED || state.0 == LOADING {
            self.xhr_request_error(cx, this, "abort");
        }
        let mut x = self.xhr(this);
        if x.state == DONE {
            x.state = UNSENT;
            x.error = true;
            x.head = None;
            x.body.clear();
        }
        Ok(())
    }
    fn get_response_header(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
    ) -> rt::Result<Option<DomStr<'_>>> {
        let x = self.xhr(this);
        if x.error {
            return Ok(None);
        }
        Ok(x.head
            .as_ref()
            .and_then(|hd| filtered_headers(hd).get(name))
            .map(DomStr::Owned))
    }
    fn get_all_response_headers(&self, _cx: &mut Cx, this: Handle) -> rt::Result<DomStr<'_>> {
        let x = self.xhr(this);
        if x.error {
            return Ok(DomStr::Borrowed(""));
        }
        let mut out = String::new();
        if let Some(hd) = &x.head {
            for (k, v) in filtered_headers(hd).sort_and_combine() {
                out.push_str(&k);
                out.push_str(": ");
                out.push_str(&v);
                out.push_str("\r\n");
            }
        }
        Ok(DomStr::Owned(out))
    }
    fn override_mime_type(&self, _cx: &mut Cx, this: Handle, mime: &str) -> rt::Result<()> {
        let mut x = self.xhr(this);
        if x.state == LOADING || x.state == DONE {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "overrideMimeType cannot be called while loading or done",
            ));
        }
        x.override_mime = Some(
            snowlark_net::mime::MimeType::parse(mime)
                .map(|m| m.serialize())
                .unwrap_or_else(|| "application/octet-stream".into()),
        );
        Ok(())
    }
    fn set_private_token(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _token: PrivateToken,
    ) -> rt::Result<()> {
        Ok(())
    }
    fn constructor(&self, cx: &mut Cx) -> rt::Result<Handle> {
        let window = self.current_window(cx);
        let upload = self.alloc(Object::XhrUpload);
        Ok(self.alloc(Object::Xhr(Box::new(XhrData {
            window,
            state: UNSENT,
            send_flag: false,
            sync: false,
            method: String::new(),
            url: None,
            blob_url_entry: None,
            headers: snowlark_net::HeaderList::new(),
            timeout: 0,
            with_credentials: false,
            upload,
            upload_complete: false,
            upload_listener: false,
            response_type: RT::Empty,
            override_mime: None,
            head: None,
            body: Vec::new(),
            error: false,
            job: None,
            timeout_timer: None,
            last_progress: 0.0,
            response_value: None,
            response_document: None,
            response_text: None,
            generation: 0,
        }))))
    }
}

impl Engine {
    /// A synchronous `send()`: waits on the network thread for the whole response.
    fn xhr_send_sync(
        &self,
        cx: &mut Cx,
        this: Handle,
        doc: NodeId,
        request: snowlark_net::Request,
    ) -> rt::Result<()> {
        let stream = self.policy_fetch(doc, request, None, None);
        let Some(mut stream) = stream else {
            self.xhr_request_error(cx, this, "error");
            return Err(exception(ExceptionName::NetworkError, "Network error"));
        };
        let timeout = self.xhr(this).timeout;
        let deadline = (timeout > 0)
            .then(|| std::time::Instant::now() + std::time::Duration::from_millis(timeout as u64));
        let mut head = None;
        let mut body = Vec::new();
        let ok = loop {
            if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                self.xhr_request_error(cx, this, "timeout");
                return Err(exception(ExceptionName::TimeoutError, "Timeout"));
            }
            match stream.next_blocking() {
                Some(FetchEvent::Head(h)) => head = Some(h),
                Some(FetchEvent::Body(b)) => body.extend_from_slice(&b),
                Some(FetchEvent::Violation(_)) => {}
                Some(FetchEvent::Done) => break true,
                Some(FetchEvent::Error(_)) | None => break false,
            }
        };
        let ok = ok
            && head
                .as_ref()
                .is_some_and(|h| h.response_type != snowlark_net::ResponseType::Error);
        if !ok {
            self.xhr_request_error(cx, this, "error");
            return Err(exception(ExceptionName::NetworkError, "Network error"));
        }
        {
            let mut x = self.xhr(this);
            x.head = head;
            x.body = body;
            x.upload_complete = true;
        }
        let (transmitted, length) = {
            let x = self.xhr(this);
            let length = x
                .head
                .as_ref()
                .and_then(|hd| hd.headers.get("content-length"))
                .and_then(|v| v.trim().parse::<u64>().ok());
            (x.body.len() as u64, length)
        };
        {
            let mut x = self.xhr(this);
            x.state = DONE;
            x.send_flag = false;
        }
        self.fire_event(cx, this, "readystatechange", false, false);
        self.fire_progress(
            cx,
            this,
            "load",
            transmitted,
            length.unwrap_or(0),
            length.is_some(),
        );
        self.fire_progress(
            cx,
            this,
            "loadend",
            transmitted,
            length.unwrap_or(0),
            length.is_some(),
        );
        Ok(())
    }

    /// `responseXML` without script: the document response, parsed on first access.
    fn xhr_document_noscript(&self, this: Handle) -> Option<NodeId> {
        let mime = self.xhr_final_mime(this);
        let essence = mime.as_ref().map(|m| m.essence()).unwrap_or_default();
        let response_type = self.xhr(this).response_type;
        let is_html = essence == "text/html";
        let is_xml =
            essence == "text/xml" || essence == "application/xml" || essence.ends_with("+xml");
        let url = self
            .xhr(this)
            .head
            .as_ref()
            .and_then(|hd| hd.url().cloned())
            .map(|u| u.to_string())
            .unwrap_or_else(|| "about:blank".into());
        let doc = if is_html && response_type == RT::Document {
            let body = self.xhr(this).body.clone();
            let (text, _, _) = encoding_rs::UTF_8.decode(&body);
            let doc = self.dom_mut().parse_html_str_with(
                &text,
                snowlark_dom::ParseOptions {
                    scripting_enabled: false,
                    iframe_srcdoc: false,
                },
                false,
            );
            self.dom_mut().set_document_url(doc, &url);
            Some(doc)
        } else if is_xml {
            let text = self.xhr_text(this);
            let (doc, r) = self.dom_mut().parse_xml_str(&text);
            if r.is_ok() {
                let mut dom = self.dom_mut();
                dom.set_document_url(doc, &url);
                dom.set_content_type(doc, &essence);
                drop(dom);
                self.xml_documents.borrow_mut().insert(doc);
                Some(doc)
            } else {
                None
            }
        } else {
            None
        };
        self.xhr(this).response_document = Some(doc);
        doc
    }
}
