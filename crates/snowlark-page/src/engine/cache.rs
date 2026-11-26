// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Cache API (Service Workers §5.4 `Cache`, §5.5 `CacheStorage`, and `caches` on Window and
//! WorkerGlobalScope): the objects pages hold, over the storage in `snowlark-sw`.
//!
//! A `Cache` names its request-response list by an id the storage gave; every method turns its
//! Request and Response into the storage's plain form, sends one operation ("Query Cache" and
//! "Batch Cache Operations" run there), and settles its promise in a task with the answer. `put()`
//! and `addAll()` read the response bodies here first, so the storage gets whole entries. The
//! storage key is the global's ([`Engine::storage_key`]); a global without one (an opaque origin)
//! is refused with a SecurityError.

use std::cell::RefCell;
use std::rc::Rc;

use snowlark_bindings::generated::ids;
use snowlark_bindings::generated::{
    CacheImpl, CacheQueryOptions, CacheStorageImpl, MultiCacheQueryOptions, ReferrerPolicy,
    RequestCache, RequestCredentials, RequestDestination, RequestInit, RequestMode,
    RequestOrString, RequestRedirect, ResponseType,
};
use snowlark_bindings::rt::{self, Cx, Error, ExceptionName, Frozen, Handle, JsPromise, JsValue};
use snowlark_net::HeadersGuard;
use snowlark_sw::{
    BatchOp, CacheBackend, CacheError, CacheId, CachedRequest, CachedResponse, Op, QueryOptions,
    Reply,
};

use super::fetch::{BodyData, RequestData};
use super::{Engine, Object};
use crate::event_loop::TaskSource;

/// What a body read for the cache ends with: the bytes (`None` for a null body) or the error.
type BodyRead = Box<dyn FnOnce(&Engine, &mut Cx, Result<Option<Vec<u8>>, JsValue>)>;

fn type_error(m: &str) -> Error {
    Error::Type(m.to_owned().into())
}

fn query_options(o: &CacheQueryOptions) -> QueryOptions {
    QueryOptions {
        ignore_search: o.ignore_search,
        ignore_method: o.ignore_method,
        ignore_vary: o.ignore_vary,
    }
}

/// The exception a storage error rejects with.
fn cache_error(e: CacheError) -> Error {
    match e {
        CacheError::Type(m) => Error::Type(m.into()),
        CacheError::InvalidState(m) => Error::Dom(ExceptionName::InvalidStateError, m.into()),
        CacheError::QuotaExceeded => Error::Dom(
            ExceptionName::QuotaExceededError,
            "The cache could not be stored".into(),
        ),
        CacheError::UnknownCache => Error::Dom(
            ExceptionName::InvalidStateError,
            "The cache was cleared".into(),
        ),
    }
}

fn is_http_scheme(url: &url::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

impl Engine {
    /// Uses `backend` for this event loop's caches (the embedder's, shared by its pages; a worker
    /// shares its parent's). Must come before the first use.
    pub fn set_cache_backend(&self, backend: CacheBackend) {
        *self.caches.borrow_mut() = Some(backend);
    }

    /// The storage of this event loop (and of the workers it starts).
    pub(crate) fn cache_backend(&self) -> CacheBackend {
        self.caches
            .borrow_mut()
            .get_or_insert_with(CacheBackend::in_memory)
            .clone()
    }

    /// `[SameObject] caches`: one CacheStorage per global.
    pub(crate) fn cache_storage(&self, global: Handle) -> Handle {
        self.window_singleton(global, "caches", || Object::CacheStorage { global })
    }

    /// A CacheStorage's or Cache's global, and the Cache's id.
    fn cache_parts(&self, h: Handle) -> (Handle, Option<CacheId>) {
        match &*self.obj(h) {
            Object::CacheStorage { global } => (*global, None),
            Object::Cache { global, id } => (*global, Some(*id)),
            _ => (h, None),
        }
    }

    /// Runs `op` on the caches of `global`'s storage key, then settles `p` in a task: `then` with
    /// the answer, or a rejection with the storage's error.
    fn cache_run(
        &self,
        cx: &mut Cx,
        global: Handle,
        p: &JsPromise,
        op: Op,
        then: impl FnOnce(&Engine, &mut Cx, &JsPromise, Reply) + 'static,
    ) {
        let Some(key) = self.storage_key(global) else {
            p.reject(
                cx,
                Error::Dom(
                    ExceptionName::SecurityError,
                    "The Cache API is not available in an opaque origin".into(),
                ),
            );
            return;
        };
        let reply = self.cache_backend().call(&key, op);
        let p = p.clone();
        self.queue_task(TaskSource::DomManipulation, move |e, cx| match reply {
            Reply::Error(err) => p.reject(cx, cache_error(err)),
            reply => then(e, cx, &p, reply),
        });
    }

    /// A RequestInfo's request: a Request's own, or a new Request's of a string.
    fn request_info(&self, cx: &mut Cx, info: RequestOrString) -> rt::Result<Handle> {
        match info {
            RequestOrString::Request(h) => Ok(h),
            s => self.construct_request(cx, s, RequestInit::default()),
        }
    }

    /// A request as the storage keeps it.
    fn cached_request(&self, h: Handle) -> CachedRequest {
        let (headers, mut c) = {
            let r = self.request(h);
            (
                r.headers,
                CachedRequest {
                    method: r.method.clone(),
                    url: r.url.to_string(),
                    headers: Vec::new(),
                    mode: r.mode.as_str().into(),
                    credentials: r.credentials.as_str().into(),
                    cache: r.cache.as_str().into(),
                    redirect: r.redirect.as_str().into(),
                    destination: r.destination.as_str().into(),
                    referrer: r.referrer.clone(),
                    referrer_policy: r.referrer_policy.as_str().into(),
                    integrity: r.integrity.clone(),
                },
            )
        };
        c.headers = self
            .headers_of(headers)
            .list
            .iter()
            .map(|(n, v)| (n.to_owned(), v.to_owned()))
            .collect();
        c
    }

    /// A response's head as the storage keeps it (its body comes from a read).
    fn cached_response_head(&self, h: Handle) -> CachedResponse {
        let (headers, mut c) = {
            let r = self.response(h);
            (
                r.headers,
                CachedResponse {
                    type_: r.type_.as_str().into(),
                    url_list: r.url_list.iter().map(|u| u.to_string()).collect(),
                    status: r.status,
                    status_text: r.status_text.clone(),
                    headers: Vec::new(),
                    body: None,
                },
            )
        };
        c.headers = self
            .headers_of(headers)
            .list
            .iter()
            .map(|(n, v)| (n.to_owned(), v.to_owned()))
            .collect();
        c
    }

    /// A Response of a stored response, its headers immutable.
    fn response_from_cache(&self, r: CachedResponse) -> Handle {
        let type_ = ResponseType::parse(&r.type_).unwrap_or(ResponseType::Default);
        let h = self.new_response(
            type_,
            r.status,
            &r.status_text,
            HeadersGuard::Immutable,
            r.body,
        );
        let headers = {
            let mut d = self.response(h);
            d.url_list = r
                .url_list
                .iter()
                .filter_map(|u| url::Url::parse(u).ok())
                .collect();
            d.headers
        };
        let mut list = self.headers_of(headers);
        for (n, v) in &r.headers {
            list.list.append(n, v);
        }
        h
    }

    /// A Request of a stored request, its headers immutable.
    fn request_from_cache(&self, r: CachedRequest) -> Option<Handle> {
        let url = url::Url::parse(&r.url).ok()?;
        let headers = self.new_headers(HeadersGuard::Immutable);
        {
            let mut list = self.headers_of(headers);
            for (n, v) in &r.headers {
                list.list.append(n, v);
            }
        }
        let signal = self.alloc(Object::AbortSignal(Default::default()));
        Some(self.alloc(Object::Request(Box::new(RequestData {
            method: r.method,
            url,
            blob_url_entry: None,
            headers,
            destination:
                RequestDestination::parse(&r.destination).unwrap_or(RequestDestination::Empty),
            referrer: r.referrer,
            referrer_policy:
                ReferrerPolicy::parse(&r.referrer_policy).unwrap_or(ReferrerPolicy::Empty),
            mode: RequestMode::parse(&r.mode).unwrap_or(RequestMode::Cors),
            credentials:
                RequestCredentials::parse(&r.credentials).unwrap_or(RequestCredentials::SameOrigin),
            cache: RequestCache::parse(&r.cache).unwrap_or(RequestCache::Default),
            redirect: RequestRedirect::parse(&r.redirect).unwrap_or(RequestRedirect::Follow),
            integrity: r.integrity,
            keepalive: false,
            signal,
            body: BodyData::default(),
            client: None,
        }))))
    }

    /// Reads a response's whole body for the cache ("read all bytes" of its stream), marking it
    /// used: `done` gets the bytes, `None` for a null body, or the error the read failed with.
    fn read_for_cache(&self, cx: &mut Cx, response: Handle, done: BodyRead) {
        let null = self
            .body_parts(response)
            .is_none_or(|(b, _)| b.bytes.is_none() && b.stream.is_none() && b.net.is_none());
        if null {
            done(self, cx, Ok(None));
            return;
        }
        let done = Rc::new(RefCell::new(Some(done)));
        let on_bytes = done.clone();
        let read = self.consume(cx, response, move |e, cx, bytes, _, read| {
            read.resolve(cx, ());
            if let Some(done) = on_bytes.borrow_mut().take() {
                done(e, cx, Ok(Some(bytes)));
            }
            Ok(())
        });
        match read {
            Ok(read) => {
                let value = read.promise(cx);
                self.upon(
                    cx,
                    &value,
                    Box::new(move |e, cx, r| {
                        if let Err(err) = r {
                            if let Some(done) = done.borrow_mut().take() {
                                done(e, cx, Err(err));
                            }
                        }
                    }),
                );
            }
            Err(err) => {
                if let Some(done) = done.borrow_mut().take() {
                    let value = self.error_value(cx, err);
                    done(self, cx, Err(value));
                }
            }
        }
    }

    /// The JS value of an error, as a rejection carries it.
    fn error_value(&self, cx: &mut Cx, err: Error) -> JsValue {
        let Some(p) = JsPromise::new(cx) else {
            let scope = cx.scope();
            let undefined = v8::undefined(scope).into();
            return JsValue::new(scope, undefined);
        };
        p.mark_handled(cx);
        p.reject(cx, err);
        let value = p.promise(cx);
        let scope = cx.scope();
        let v = v8::Local::new(scope, value.global());
        match v8::Local::<v8::Promise>::try_from(v) {
            Ok(promise) => {
                let reason = promise.result(scope);
                JsValue::new(scope, reason)
            }
            Err(_) => value,
        }
    }

    /// `Cache.matchAll()` and `match()` (with `first`).
    fn cache_match(
        &self,
        cx: &mut Cx,
        this: Handle,
        request: Option<RequestOrString>,
        options: &CacheQueryOptions,
        first: bool,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (global, Some(cache)) = self.cache_parts(this) else {
            return Err(type_error("Illegal invocation"));
        };
        let query = match request {
            None => None,
            Some(info) => match self.request_info(cx, info) {
                Ok(r) => Some(self.cached_request(r)),
                Err(e) => {
                    p.reject(cx, e);
                    return Ok(p);
                }
            },
        };
        let op = Op::Match {
            cache,
            request: query,
            options: query_options(options),
            first,
        };
        self.cache_run(cx, global, &p, op, move |e, cx, p, reply| {
            let Reply::Responses(found) = reply else {
                return;
            };
            let responses: Vec<Handle> = found
                .into_iter()
                .map(|r| e.response_from_cache(r))
                .collect();
            if first {
                match responses.first() {
                    Some(&h) => p.resolve(cx, h),
                    None => p.resolve(cx, ()),
                }
            } else {
                p.resolve(cx, Frozen(responses));
            }
        });
        Ok(p)
    }

    /// Puts each request with its fetched response, all or nothing ("Batch Cache Operations").
    fn cache_put_all(
        &self,
        cx: &mut Cx,
        global: Handle,
        cache: CacheId,
        entries: Vec<(CachedRequest, CachedResponse)>,
        p: &JsPromise,
    ) {
        let ops = entries
            .into_iter()
            .map(|(request, response)| BatchOp::Put { request, response })
            .collect();
        self.cache_run(cx, global, p, Op::Batch { cache, ops }, |_, cx, p, _| {
            p.resolve(cx, ())
        });
    }

    /// The checks `put()` and `addAll()` make of a response: none with status 206, none whose
    /// `Vary` lists `*`.
    fn cacheable_response(&self, head: &CachedResponse) -> Result<(), Error> {
        if head.status == 206 {
            return Err(type_error(
                "Partial response (status code 206) is unsupported",
            ));
        }
        if snowlark_sw::cache::varies_on_everything(&head.headers) {
            return Err(type_error("Vary header contains *"));
        }
        Ok(())
    }
}

impl CacheStorageImpl for Engine {
    /// §5.5.1 `match(request, options)`.
    fn match_(
        &self,
        cx: &mut Cx,
        this: Handle,
        request: RequestOrString,
        options: MultiCacheQueryOptions,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (global, _) = self.cache_parts(this);
        let query = match self.request_info(cx, request) {
            Ok(r) => self.cached_request(r),
            Err(e) => {
                p.reject(cx, e);
                return Ok(p);
            }
        };
        let op = Op::MatchAny {
            cache_name: options.cache_name.clone(),
            request: query,
            options: QueryOptions {
                ignore_search: options.ignore_search,
                ignore_method: options.ignore_method,
                ignore_vary: options.ignore_vary,
            },
        };
        self.cache_run(cx, global, &p, op, |e, cx, p, reply| {
            if let Reply::Responses(mut found) = reply {
                match found.pop() {
                    Some(r) => {
                        let h = e.response_from_cache(r);
                        p.resolve(cx, h);
                    }
                    None => p.resolve(cx, ()),
                }
            }
        });
        Ok(p)
    }

    /// §5.5.2 `has(cacheName)`.
    fn has(&self, cx: &mut Cx, this: Handle, cache_name: &str) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (global, _) = self.cache_parts(this);
        let op = Op::Has {
            name: cache_name.to_owned(),
        };
        self.cache_run(cx, global, &p, op, |_, cx, p, reply| {
            if let Reply::Bool(b) = reply {
                p.resolve(cx, b);
            }
        });
        Ok(p)
    }

    /// §5.5.3 `open(cacheName)`: a new Cache object of the named cache, made if there is none.
    fn open(&self, cx: &mut Cx, this: Handle, cache_name: &str) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (global, _) = self.cache_parts(this);
        let op = Op::Open {
            name: cache_name.to_owned(),
        };
        self.cache_run(cx, global, &p, op, move |e, cx, p, reply| {
            if let Reply::Cache(id) = reply {
                let h = e.alloc(Object::Cache { global, id });
                p.resolve(cx, h);
            }
        });
        Ok(p)
    }

    /// §5.5.4 `delete(cacheName)`.
    fn delete(&self, cx: &mut Cx, this: Handle, cache_name: &str) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (global, _) = self.cache_parts(this);
        let op = Op::Delete {
            name: cache_name.to_owned(),
        };
        self.cache_run(cx, global, &p, op, |_, cx, p, reply| {
            if let Reply::Bool(b) = reply {
                p.resolve(cx, b);
            }
        });
        Ok(p)
    }

    /// §5.5.5 `keys()`: the cache names in the order they were made.
    fn keys(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (global, _) = self.cache_parts(this);
        self.cache_run(cx, global, &p, Op::Names, |_, cx, p, reply| {
            if let Reply::Names(names) = reply {
                p.resolve(cx, names);
            }
        });
        Ok(p)
    }
}

impl CacheImpl for Engine {
    /// §5.4.2 `match(request, options)`.
    fn match_(
        &self,
        cx: &mut Cx,
        this: Handle,
        request: RequestOrString,
        options: CacheQueryOptions,
    ) -> rt::Result<JsPromise> {
        self.cache_match(cx, this, Some(request), &options, true)
    }

    /// §5.4.3 `matchAll(request, options)`.
    fn match_all(
        &self,
        cx: &mut Cx,
        this: Handle,
        request: Option<RequestOrString>,
        options: CacheQueryOptions,
    ) -> rt::Result<JsPromise> {
        self.cache_match(cx, this, request, &options, false)
    }

    /// §5.4.4 `add(request)`: `addAll([request])`.
    fn add(&self, cx: &mut Cx, this: Handle, request: RequestOrString) -> rt::Result<JsPromise> {
        self.add_all(cx, this, vec![request])
    }

    /// §5.4.5 `addAll(requests)`: fetches every request, and puts them all when every response
    /// is whole and ok, or none.
    fn add_all(
        &self,
        cx: &mut Cx,
        this: Handle,
        requests: Vec<RequestOrString>,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (global, Some(cache)) = self.cache_parts(this) else {
            return Err(type_error("Illegal invocation"));
        };
        let mut list = Vec::with_capacity(requests.len());
        for info in requests {
            let r = match self.request_info(cx, info) {
                Ok(r) => r,
                Err(e) => {
                    p.reject(cx, e);
                    return Ok(p);
                }
            };
            let (http, get) = {
                let d = self.request(r);
                (is_http_scheme(&d.url), d.method == "GET")
            };
            if !http || !get {
                p.reject(
                    cx,
                    type_error("Add/AddAll only supports the GET request method over http(s)"),
                );
                return Ok(p);
            }
            list.push(r);
        }
        if list.is_empty() {
            self.cache_put_all(cx, global, cache, Vec::new(), &p);
            return Ok(p);
        }
        struct AddAll {
            left: usize,
            failed: bool,
            entries: Vec<Option<(CachedRequest, CachedResponse)>>,
        }
        let state = Rc::new(RefCell::new(AddAll {
            left: list.len(),
            failed: false,
            entries: vec![None; list.len()],
        }));
        // Each settles once: a failure rejects `p` and the rest are ignored.
        let fail = |state: &Rc<RefCell<AddAll>>,
                    cx: &mut Cx,
                    p: &JsPromise,
                    err: Result<Error, JsValue>| {
            let mut s = state.borrow_mut();
            if s.failed {
                return;
            }
            s.failed = true;
            drop(s);
            match err {
                Ok(e) => p.reject(cx, e),
                Err(v) => p.reject_value(cx, &v),
            }
        };
        for (i, r) in list.into_iter().enumerate() {
            let request = self.cached_request(r);
            let fetched =
                self.fetch_steps(cx, RequestOrString::Request(r), RequestInit::default())?;
            let value = fetched.promise(cx);
            let (state, p) = (state.clone(), p.clone());
            self.upon(
                cx,
                &value,
                Box::new(move |e, cx, outcome| {
                    let value = match outcome {
                        Ok(v) => v,
                        Err(err) => return fail(&state, cx, &p, Err(err)),
                    };
                    let v = v8::Local::new(cx.scope(), value.global());
                    let Some(response) = rt::unwrap_value(v, ids::RESPONSE, ids::RESPONSE_END)
                    else {
                        return fail(&state, cx, &p, Ok(type_error("Failed to fetch")));
                    };
                    let head = e.cached_response_head(response);
                    if head.type_ == "error" || !(200..=299).contains(&head.status) {
                        return fail(
                            &state,
                            cx,
                            &p,
                            Ok(type_error("Request failed: the response is not ok")),
                        );
                    }
                    if let Err(err) = e.cacheable_response(&head) {
                        return fail(&state, cx, &p, Ok(err));
                    }
                    e.read_for_cache(
                        cx,
                        response,
                        Box::new(move |e, cx, body| {
                            let mut head = head;
                            match body {
                                Ok(bytes) => head.body = bytes,
                                Err(err) => return fail(&state, cx, &p, Err(err)),
                            }
                            let entries = {
                                let mut s = state.borrow_mut();
                                if s.failed {
                                    return;
                                }
                                s.entries[i] = Some((request, head));
                                s.left -= 1;
                                if s.left > 0 {
                                    return;
                                }
                                std::mem::take(&mut s.entries)
                            };
                            let entries = entries.into_iter().flatten().collect();
                            e.cache_put_all(cx, global, cache, entries, &p);
                        }),
                    );
                }),
            );
        }
        Ok(p)
    }

    /// §5.4.6 `put(request, response)`.
    fn put(
        &self,
        cx: &mut Cx,
        this: Handle,
        request: RequestOrString,
        response: Handle,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (global, Some(cache)) = self.cache_parts(this) else {
            return Err(type_error("Illegal invocation"));
        };
        let r = match self.request_info(cx, request) {
            Ok(r) => r,
            Err(e) => {
                p.reject(cx, e);
                return Ok(p);
            }
        };
        let (http, get) = {
            let d = self.request(r);
            (is_http_scheme(&d.url), d.method == "GET")
        };
        if !http {
            p.reject(cx, type_error("Request scheme must be http or https"));
            return Ok(p);
        }
        if !get {
            p.reject(cx, type_error("Request method must be GET"));
            return Ok(p);
        }
        let head = self.cached_response_head(response);
        if let Err(e) = self.cacheable_response(&head) {
            p.reject(cx, e);
            return Ok(p);
        }
        let unusable = self.body_parts(response).is_some_and(|(b, _)| {
            b.used
                || b.stream
                    .is_some_and(|s| self.readable_locked(s) || self.readable_disturbed(s))
        });
        if unusable {
            p.reject(cx, type_error("Response body is already used"));
            return Ok(p);
        }
        let request = self.cached_request(r);
        let p2 = p.clone();
        self.read_for_cache(
            cx,
            response,
            Box::new(move |e, cx, body| match body {
                Ok(bytes) => {
                    let mut head = head;
                    head.body = bytes;
                    e.cache_put_all(cx, global, cache, vec![(request, head)], &p2);
                }
                Err(err) => p2.reject_value(cx, &err),
            }),
        );
        Ok(p)
    }

    /// §5.4.7 `delete(request, options)`.
    fn delete(
        &self,
        cx: &mut Cx,
        this: Handle,
        request: RequestOrString,
        options: CacheQueryOptions,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (global, Some(cache)) = self.cache_parts(this) else {
            return Err(type_error("Illegal invocation"));
        };
        let r = match self.request_info(cx, request) {
            Ok(r) => r,
            Err(e) => {
                p.reject(cx, e);
                return Ok(p);
            }
        };
        let op = Op::Batch {
            cache,
            ops: vec![BatchOp::Delete {
                request: self.cached_request(r),
                options: query_options(&options),
            }],
        };
        self.cache_run(cx, global, &p, op, |_, cx, p, reply| {
            if let Reply::Batch(counts) = reply {
                p.resolve(cx, counts.first().is_some_and(|&n| n > 0));
            }
        });
        Ok(p)
    }

    /// §5.4.8 `keys(request, options)`.
    fn keys(
        &self,
        cx: &mut Cx,
        this: Handle,
        request: Option<RequestOrString>,
        options: CacheQueryOptions,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let (global, Some(cache)) = self.cache_parts(this) else {
            return Err(type_error("Illegal invocation"));
        };
        let query = match request {
            None => None,
            Some(info) => match self.request_info(cx, info) {
                Ok(r) => Some(self.cached_request(r)),
                Err(e) => {
                    p.reject(cx, e);
                    return Ok(p);
                }
            },
        };
        let op = Op::Keys {
            cache,
            request: query,
            options: query_options(&options),
        };
        self.cache_run(cx, global, &p, op, |e, cx, p, reply| {
            if let Reply::Requests(found) = reply {
                let requests: Vec<Handle> = found
                    .into_iter()
                    .filter_map(|r| e.request_from_cache(r))
                    .collect();
                p.resolve(cx, Frozen(requests));
            }
        });
        Ok(p)
    }
}
