// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CORS protocol (Fetch §3.2): the CORS check, evaluating a preflight response, the CORS-preflight
//! cache (§4.9), and `Access-Control-Expose-Headers`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use url::Url;

use crate::headers::{
    cors_unsafe_request_header_names, is_cors_non_wildcard_request_header_name,
    is_cors_safelisted_method, HeaderList,
};
use crate::mime::{is_token, split_header_value};
use crate::request::{CredentialsMode, Request};

/// Fetch "CORS check": `Access-Control-Allow-Origin` (and `-Credentials` for credentialed requests).
pub fn cors_check(request: &Request, response_headers: &HeaderList) -> Result<(), String> {
    let Some(origin) = response_headers.get("access-control-allow-origin") else {
        return Err(
            "No 'Access-Control-Allow-Origin' header is present on the requested resource.".into(),
        );
    };
    let credentials = request.credentials == CredentialsMode::Include;
    if !credentials && origin == "*" {
        return Ok(());
    }
    let serialized = request.serialized_origin();
    if origin != serialized {
        return Err(format!(
            "The 'Access-Control-Allow-Origin' header has a value '{origin}' that is not equal to the supplied origin '{serialized}'."
        ));
    }
    if !credentials {
        return Ok(());
    }
    if response_headers
        .get("access-control-allow-credentials")
        .as_deref()
        == Some("true")
    {
        Ok(())
    } else {
        Err("The value of the 'Access-Control-Allow-Credentials' header must be 'true' when the request's credentials mode is 'include'.".into())
    }
}

/// Fetch "extract header list values" for a `#token` header: `None` for absent, `Err` for a parse failure.
pub fn extract_token_list(headers: &HeaderList, name: &str) -> Result<Option<Vec<String>>, String> {
    let Some(values) = headers.get_decode_split(name) else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for v in values {
        if v.is_empty() {
            continue;
        }
        if !is_token(&v) {
            return Err(format!("invalid {name} value: {v:?}"));
        }
        out.push(v);
    }
    Ok(Some(out))
}

/// The CORS-exposed header-name list of a CORS response (main fetch step 13.1).
pub fn exposed_header_names(request: &Request, response_headers: &HeaderList) -> Vec<String> {
    let names = extract_token_list(response_headers, "access-control-expose-headers")
        .ok()
        .flatten()
        .unwrap_or_default();
    if request.credentials != CredentialsMode::Include && names.iter().any(|n| n == "*") {
        response_headers.names()
    } else {
        names
    }
}

/// What a successful preflight allowed, for the cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreflightResult {
    pub methods: Vec<String>,
    pub header_names: Vec<String>,
    pub max_age: Duration,
}

/// The longest a preflight result is cached, whatever `Access-Control-Max-Age` says (Chromium's limit).
pub const MAX_PREFLIGHT_AGE: Duration = Duration::from_secs(7200);
/// The default when `Access-Control-Max-Age` is absent.
pub const DEFAULT_PREFLIGHT_AGE: Duration = Duration::from_secs(5);

/// Fetch §4.8 steps 7.1–7.10: evaluate a preflight response's `Access-Control-Allow-*` for `request`
/// (after the CORS check and the ok-status check passed).
pub fn evaluate_preflight(
    request: &Request,
    headers: &HeaderList,
) -> Result<PreflightResult, String> {
    let methods = extract_token_list(headers, "access-control-allow-methods")
        .map_err(|_| "Invalid 'Access-Control-Allow-Methods' header.".to_string())?;
    let header_names = extract_token_list(headers, "access-control-allow-headers")
        .map_err(|_| "Invalid 'Access-Control-Allow-Headers' header.".to_string())?
        .unwrap_or_default();
    let methods = match methods {
        None if request.use_cors_preflight => vec![request.method.clone()],
        None => Vec::new(),
        Some(m) => m,
    };
    let credentials = request.credentials == CredentialsMode::Include;
    if !methods.contains(&request.method)
        && !is_cors_safelisted_method(&request.method)
        && (credentials || !methods.iter().any(|m| m == "*"))
    {
        return Err(format!(
            "Method {} is not allowed by Access-Control-Allow-Methods in preflight response.",
            request.method
        ));
    }
    let listed = |name: &str| header_names.iter().any(|h| h.eq_ignore_ascii_case(name));
    for (name, _) in request.headers.iter() {
        if is_cors_non_wildcard_request_header_name(name) && !listed(name) {
            return Err(format!(
                "Request header field {name} is not allowed by Access-Control-Allow-Headers in preflight response."
            ));
        }
    }
    let wildcard = header_names.iter().any(|h| h == "*");
    for name in cors_unsafe_request_header_names(&request.headers) {
        if !listed(&name) && (credentials || !wildcard) {
            return Err(format!(
                "Request header field {name} is not allowed by Access-Control-Allow-Headers in preflight response."
            ));
        }
    }
    let max_age = headers
        .get("access-control-max-age")
        .and_then(|v| split_header_value(&v).into_iter().next())
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_PREFLIGHT_AGE)
        .min(MAX_PREFLIGHT_AGE);
    Ok(PreflightResult {
        methods,
        header_names,
        max_age,
    })
}

#[derive(Clone, Debug)]
struct Entry {
    origin: String,
    url: Url,
    credentials: bool,
    method: Option<String>,
    header_name: Option<String>,
    expires: Instant,
}

/// Fetch §4.9 the CORS-preflight cache.
#[derive(Debug, Default)]
pub struct PreflightCache {
    entries: Mutex<Vec<Entry>>,
}

impl PreflightCache {
    fn matches(e: &Entry, request: &Request, now: Instant) -> bool {
        e.expires > now
            && e.origin == request.serialized_origin()
            && e.url == *request.url()
            && e.credentials == (request.credentials == CredentialsMode::Include)
    }

    /// "method cache entry match", with the `*` entries counting only for uncredentialed requests.
    pub fn has_method(&self, request: &Request) -> bool {
        let now = Instant::now();
        let credentials = request.credentials == CredentialsMode::Include;
        self.entries.lock().unwrap().iter().any(|e| {
            Self::matches(e, request, now)
                && e.method
                    .as_deref()
                    .is_some_and(|m| m == request.method || (m == "*" && !credentials))
        })
    }

    /// "header-name cache entry match".
    pub fn has_header(&self, request: &Request, name: &str) -> bool {
        let now = Instant::now();
        let credentials = request.credentials == CredentialsMode::Include;
        self.entries.lock().unwrap().iter().any(|e| {
            Self::matches(e, request, now)
                && e.header_name.as_deref().is_some_and(|h| {
                    h.eq_ignore_ascii_case(name)
                        || (h == "*"
                            && !credentials
                            && !is_cors_non_wildcard_request_header_name(name))
                })
        })
    }

    /// Whether `request` needs a preflight given what is cached (HTTP fetch step 4.1).
    pub fn needs_preflight(&self, request: &Request) -> bool {
        (!self.has_method(request) && !is_cors_safelisted_method(&request.method))
            || cors_unsafe_request_header_names(&request.headers)
                .iter()
                .any(|name| !self.has_header(request, name))
    }

    /// Store a successful preflight's result (§4.8 steps 7.11–7.13).
    pub fn store(&self, request: &Request, result: &PreflightResult) {
        let now = Instant::now();
        let expires = now + result.max_age;
        let base = Entry {
            origin: request.serialized_origin(),
            url: request.url().clone(),
            credentials: request.credentials == CredentialsMode::Include,
            method: None,
            header_name: None,
            expires,
        };
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|e| e.expires > now);
        for method in &result.methods {
            match entries
                .iter_mut()
                .find(|e| Self::matches(e, request, now) && e.method.as_deref() == Some(method))
            {
                Some(e) => e.expires = expires,
                None => entries.push(Entry {
                    method: Some(method.clone()),
                    ..base.clone()
                }),
            }
        }
        for name in &result.header_names {
            match entries.iter_mut().find(|e| {
                Self::matches(e, request, now)
                    && e.header_name
                        .as_deref()
                        .is_some_and(|h| h.eq_ignore_ascii_case(name))
            }) {
                Some(e) => e.expires = expires,
                None => entries.push(Entry {
                    header_name: Some(name.clone()),
                    ..base.clone()
                }),
            }
        }
    }

    /// "clear cache entries" for a request whose preflight or CORS check failed.
    pub fn clear_for(&self, request: &Request) {
        let origin = request.serialized_origin();
        let url = request.url().clone();
        self.entries
            .lock()
            .unwrap()
            .retain(|e| !(e.origin == origin && e.url == url));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Client, RequestOrigin};
    use std::sync::Arc;

    fn request(method: &str) -> Request {
        let page = Url::parse("https://a.example/").unwrap();
        let mut r = Request::script_fetch(
            Url::parse("https://api.example/data").unwrap(),
            Arc::new(Client::for_url(&page)),
        );
        r.origin = RequestOrigin::Origin(page.origin());
        r.method = method.into();
        r
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderList {
        pairs.iter().copied().collect()
    }

    #[test]
    fn check() {
        let mut r = request("GET");
        assert!(cors_check(&r, &headers(&[("Access-Control-Allow-Origin", "*")])).is_ok());
        assert!(cors_check(
            &r,
            &headers(&[("Access-Control-Allow-Origin", "https://a.example")])
        )
        .is_ok());
        assert!(cors_check(
            &r,
            &headers(&[("Access-Control-Allow-Origin", "https://a.example/")])
        )
        .is_err());
        assert!(cors_check(&r, &headers(&[])).is_err());
        r.credentials = CredentialsMode::Include;
        assert!(cors_check(&r, &headers(&[("Access-Control-Allow-Origin", "*")])).is_err());
        assert!(cors_check(
            &r,
            &headers(&[("Access-Control-Allow-Origin", "https://a.example")])
        )
        .is_err());
        assert!(cors_check(
            &r,
            &headers(&[
                ("Access-Control-Allow-Origin", "https://a.example"),
                ("Access-Control-Allow-Credentials", "true")
            ])
        )
        .is_ok());
        r.tainted_origin = true;
        assert!(cors_check(
            &r,
            &headers(&[
                ("Access-Control-Allow-Origin", "null"),
                ("Access-Control-Allow-Credentials", "true")
            ])
        )
        .is_ok());
    }

    #[test]
    fn preflight_evaluation() {
        let mut r = request("PUT");
        r.headers.append("X-Custom", "1");
        assert!(evaluate_preflight(&r, &headers(&[])).is_err());
        let ok = headers(&[
            ("Access-Control-Allow-Methods", "PUT, DELETE"),
            ("Access-Control-Allow-Headers", "x-custom"),
            ("Access-Control-Max-Age", "600"),
        ]);
        let result = evaluate_preflight(&r, &ok).unwrap();
        assert_eq!(result.max_age, Duration::from_secs(600));
        let wildcard = headers(&[
            ("Access-Control-Allow-Methods", "*"),
            ("Access-Control-Allow-Headers", "*"),
        ]);
        assert!(evaluate_preflight(&r, &wildcard).is_ok());
        r.headers.append("Authorization", "Bearer x");
        assert!(evaluate_preflight(&r, &wildcard).is_err());
        r.credentials = CredentialsMode::Include;
        r.headers.delete("authorization");
        assert!(evaluate_preflight(&r, &wildcard).is_err());
        assert!(
            evaluate_preflight(&r, &headers(&[("Access-Control-Allow-Methods", "PU T")])).is_err()
        );
    }

    #[test]
    fn cache() {
        let cache = PreflightCache::default();
        let mut r = request("PUT");
        r.headers.append("X-Custom", "1");
        assert!(cache.needs_preflight(&r));
        cache.store(
            &r,
            &PreflightResult {
                methods: vec!["PUT".into()],
                header_names: vec!["X-Custom".into()],
                max_age: Duration::from_secs(60),
            },
        );
        assert!(!cache.needs_preflight(&r));
        r.headers.append("X-Other", "1");
        assert!(cache.needs_preflight(&r));
        cache.clear_for(&r);
        r.headers.delete("x-other");
        assert!(cache.needs_preflight(&r));
        // safelisted method and headers never need one
        assert!(!cache.needs_preflight(&request("POST")));
    }

    #[test]
    fn exposure() {
        let r = request("GET");
        let h = headers(&[("Access-Control-Expose-Headers", "*"), ("X-A", "1")]);
        assert_eq!(
            exposed_header_names(&r, &h),
            vec!["access-control-expose-headers", "x-a"]
        );
        let h = headers(&[("Access-Control-Expose-Headers", "x-a, X-B")]);
        assert_eq!(exposed_header_names(&r, &h), vec!["x-a", "X-B"]);
    }
}
