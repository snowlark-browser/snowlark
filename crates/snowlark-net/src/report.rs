// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sending Content Security Policy violation reports (CSP Level 3 §5.3 "report a violation") and the
//! Reporting API's endpoint configuration (Reporting API §3, `Reporting-Endpoints`; the legacy
//! `Report-To` header), and HTML's X-Frame-Options check of a framed response (HTML §7.1.7).
//!
//! Pure functions over plain data: the page layer gathers the violation's fields (the document's URL
//! and referrer, the script location), these build the bodies and the requests, and the page sends
//! them through its fetch path like any other request of the document.

use std::sync::Arc;

use serde_json::{json, Map, Value};
use url::{Origin, Url};

use crate::csp::{strip_url_for_report, CspList, Disposition, Violation};
use crate::headers::HeaderList;
use crate::request::{
    Body, Client, CredentialsMode, Destination, Mode, RedirectMode, Referrer, Request,
};

/// A violation's fields as the `securitypolicyviolation` event and the reports carry them (CSP §2.4 and
/// §5.3; `SecurityPolicyViolationEventInit` and `CSPViolationReportBody`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViolationReport {
    /// The document's (or worker's) URL, stripped for reports.
    pub document_url: String,
    pub referrer: String,
    pub blocked_url: String,
    pub effective_directive: String,
    pub original_policy: String,
    /// The script the violation came from, stripped; "" when unknown.
    pub source_file: String,
    pub sample: String,
    pub disposition: Disposition,
    pub status_code: u16,
    pub line_number: u32,
    pub column_number: u32,
}

impl ViolationReport {
    /// The report fields of `violation` in a document at `document_url` (CSP §5.3 steps 1-3).
    pub fn new(
        violation: &Violation,
        document_url: &Url,
        referrer: &str,
        status_code: u16,
        source: Option<(&str, u32, u32)>,
    ) -> ViolationReport {
        let (source_file, line_number, column_number) = match source {
            Some((file, line, column)) => (
                Url::parse(file)
                    .map(|u| strip_url_for_report(&u))
                    .unwrap_or_else(|_| file.to_owned()),
                line,
                column,
            ),
            None => (String::new(), 0, 0),
        };
        ViolationReport {
            document_url: strip_url_for_report(document_url),
            referrer: Url::parse(referrer)
                .map(|u| strip_url_for_report(&u))
                .unwrap_or_default(),
            blocked_url: violation.blocked_uri(),
            effective_directive: violation.effective_directive.clone(),
            original_policy: violation.original_policy.clone(),
            source_file,
            sample: violation.sample.clone(),
            disposition: violation.disposition,
            status_code,
            line_number,
            column_number,
        }
    }

    fn disposition_str(&self) -> &'static str {
        match self.disposition {
            Disposition::Enforce => "enforce",
            Disposition::Report => "report",
        }
    }

    /// The `report-uri` body (CSP §5.3 step 4, the deprecated serialization), `application/csp-report`.
    pub fn csp_report_json(&self) -> String {
        let mut r = Map::new();
        r.insert("document-uri".into(), json!(self.document_url));
        r.insert("referrer".into(), json!(self.referrer));
        r.insert("violated-directive".into(), json!(self.effective_directive));
        r.insert(
            "effective-directive".into(),
            json!(self.effective_directive),
        );
        r.insert("original-policy".into(), json!(self.original_policy));
        r.insert("disposition".into(), json!(self.disposition_str()));
        r.insert("blocked-uri".into(), json!(self.blocked_url));
        r.insert("status-code".into(), json!(self.status_code));
        r.insert("script-sample".into(), json!(self.sample));
        if !self.source_file.is_empty() {
            r.insert("source-file".into(), json!(self.source_file));
            r.insert("line-number".into(), json!(self.line_number));
            r.insert("column-number".into(), json!(self.column_number));
        }
        json!({ "csp-report": Value::Object(r) }).to_string()
    }

    /// The Reporting API body (`CSPViolationReportBody`) of a `csp-violation` report.
    pub fn report_body(&self) -> Value {
        json!({
            "documentURL": self.document_url,
            "referrer": self.referrer,
            "blockedURL": self.blocked_url,
            "effectiveDirective": self.effective_directive,
            "originalPolicy": self.original_policy,
            "sourceFile": if self.source_file.is_empty() { Value::Null } else { json!(self.source_file) },
            "sample": self.sample,
            "disposition": self.disposition_str(),
            "statusCode": self.status_code,
            "lineNumber": if self.source_file.is_empty() { Value::Null } else { json!(self.line_number) },
            "columnNumber": if self.source_file.is_empty() { Value::Null } else { json!(self.column_number) },
        })
    }

    /// A delivery of this report to a Reporting API endpoint (Reporting API §3.5.2 "serialize a list of
    /// reports to JSON"), `application/reports+json`.
    pub fn reports_json(&self, user_agent: &str) -> String {
        json!([{
            "type": "csp-violation",
            "age": 0,
            "url": self.document_url,
            "user_agent": user_agent,
            "body": self.report_body(),
        }])
        .to_string()
    }
}

/// Reporting API §3.2 "process reporting endpoints" (`Reporting-Endpoints`, a structured field
/// dictionary of strings), then the legacy `Report-To` header (JSON objects with a `group` and its
/// `endpoints`): (name, URL) pairs, each URL resolved against `base` and potentially trustworthy or
/// at least HTTP(S).
pub fn reporting_endpoints(headers: &HeaderList, base: &Url) -> Vec<(String, Url)> {
    let mut out: Vec<(String, Url)> = Vec::new();
    for value in headers.get_all("reporting-endpoints") {
        for member in split_top_level(value, ',') {
            let Some((name, rest)) = member.split_once('=') else {
                continue;
            };
            let name = name.trim();
            let rest = rest.trim();
            // (an sf-string; parameters after it are ignored)
            let Some(s) = rest.strip_prefix('"') else {
                continue;
            };
            let Some(end) = s.find('"') else {
                continue;
            };
            if let Ok(url) = base.join(&s[..end]) {
                if matches!(url.scheme(), "http" | "https") && !name.is_empty() {
                    out.push((name.to_owned(), url));
                }
            }
        }
    }
    for value in headers.get_all("report-to") {
        // (several JSON objects may be listed, comma-separated)
        let text = format!("[{value}]");
        let Ok(Value::Array(groups)) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        for g in groups {
            let name = g
                .get("group")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_owned();
            if out.iter().any(|(n, _)| *n == name) {
                continue;
            }
            let endpoints = g.get("endpoints").and_then(Value::as_array);
            if let Some(url) = endpoints
                .into_iter()
                .flatten()
                .filter_map(|e| e.get("url").and_then(Value::as_str))
                .filter_map(|u| base.join(u).ok())
                .find(|u| matches!(u.scheme(), "http" | "https"))
            {
                out.push((name, url));
            }
        }
    }
    out
}

/// Splits `s` at `sep` outside double quotes.
fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c == sep && !quoted => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// CSP §5.3 step 4.4 / Reporting API §3.5.3: the request delivering a report, from `client`: a POST
/// with destination "report", no-cors, same-origin credentials, redirects an error, keepalive.
pub fn report_request(url: Url, body: String, content_type: &str, client: Arc<Client>) -> Request {
    let mut r = Request::new(url);
    r.method = "POST".into();
    r.client = Some(client);
    r.destination = Destination::Report;
    r.mode = Mode::NoCors;
    r.credentials = CredentialsMode::SameOrigin;
    r.redirect = RedirectMode::Error;
    r.referrer = Referrer::NoReferrer;
    r.keepalive = true;
    r.initiator_type = "other".into();
    r.headers.append("Content-Type", content_type);
    r.body = Some(Body::new(body.into_bytes()));
    r
}

/// Where a violation of `list`'s policy is reported: the `report-to` endpoint (looked up in
/// `endpoints`) when the policy names one, else its `report-uri` URLs resolved against `base` (§5.3
/// step 4: `report-uri` is ignored when `report-to` is present). Each entry: the URL and whether it is
/// a Reporting API endpoint.
pub fn report_destinations(
    list: &CspList,
    violation: &Violation,
    endpoints: &[(String, Url)],
    base: &Url,
) -> Vec<(Url, bool)> {
    let Some(policy) = list.policies.get(violation.policy_index) else {
        return Vec::new();
    };
    if let Some(group) = policy.report_to() {
        return endpoints
            .iter()
            .filter(|(name, _)| name == group)
            .map(|(_, url)| (url.clone(), true))
            .take(1)
            .collect();
    }
    policy
        .report_uris()
        .into_iter()
        .filter_map(|u| base.join(u).ok())
        .map(|u| (u, false))
        .collect()
}

/// HTML §7.1.7 "check a navigation response's adherence to `X-Frame-Options`" for a response shown in a
/// child navigable: `ancestors` are the origins of the active documents of the navigable's ancestors
/// (nearest first), `destination` the response's origin. An enforced `frame-ancestors` in `csp`
/// takes precedence. Returns whether the response may be shown.
pub fn x_frame_options_allows(
    headers: &HeaderList,
    csp: &CspList,
    ancestors: &[Origin],
    destination: &Origin,
) -> bool {
    if csp
        .policies
        .iter()
        .any(|p| p.disposition == Disposition::Enforce && p.directive("frame-ancestors").is_some())
    {
        return true;
    }
    let mut xfo: Vec<String> = Vec::new();
    for value in headers.get_all("x-frame-options") {
        for v in value.split(',') {
            let v = v.trim().to_ascii_lowercase();
            if !xfo.contains(&v) {
                xfo.push(v);
            }
        }
    }
    if xfo.len() > 1 {
        return !xfo
            .iter()
            .any(|v| matches!(v.as_str(), "deny" | "allowall" | "sameorigin"));
    }
    match xfo.first().map(String::as_str) {
        Some("deny") => false,
        Some("sameorigin") => ancestors.iter().all(|a| a == destination),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csp::{PolicySource, ViolationResource};

    fn violation(policy: &str, disposition: Disposition) -> Violation {
        Violation {
            resource: ViolationResource::Url(Url::parse("https://b.example/x.png#f").unwrap()),
            effective_directive: "img-src".into(),
            directive: "img-src 'none'".into(),
            disposition,
            policy_index: 0,
            original_policy: policy.into(),
            sample: String::new(),
        }
    }

    #[test]
    fn report_uri_body() {
        let v = violation("img-src 'none'; report-uri /r", Disposition::Enforce);
        let page = Url::parse("https://user:pw@a.example/page?q#frag").unwrap();
        let r = ViolationReport::new(&v, &page, "", 200, None);
        let body: Value = serde_json::from_str(&r.csp_report_json()).unwrap();
        let c = &body["csp-report"];
        assert_eq!(c["document-uri"], "https://a.example/page?q");
        assert_eq!(c["blocked-uri"], "https://b.example/x.png");
        assert_eq!(c["violated-directive"], "img-src");
        assert_eq!(c["effective-directive"], "img-src");
        assert_eq!(c["original-policy"], "img-src 'none'; report-uri /r");
        assert_eq!(c["disposition"], "enforce");
        assert_eq!(c["status-code"], 200);
        assert!(c.get("source-file").is_none());
        let with_source = ViolationReport::new(
            &v,
            &page,
            "https://c.example/from",
            200,
            Some(("https://a.example/s.js", 3, 7)),
        );
        let body: Value = serde_json::from_str(&with_source.csp_report_json()).unwrap();
        assert_eq!(body["csp-report"]["source-file"], "https://a.example/s.js");
        assert_eq!(body["csp-report"]["line-number"], 3);
        assert_eq!(body["csp-report"]["referrer"], "https://c.example/from");
        let reports: Value = serde_json::from_str(&with_source.reports_json("UA")).unwrap();
        assert_eq!(reports[0]["type"], "csp-violation");
        assert_eq!(reports[0]["body"]["blockedURL"], "https://b.example/x.png");
        assert_eq!(reports[0]["body"]["columnNumber"], 7);
    }

    #[test]
    fn endpoints_and_destinations() {
        let base = Url::parse("https://a.example/dir/page").unwrap();
        let mut h = HeaderList::new();
        h.append(
            "Reporting-Endpoints",
            "csp-endpoint=\"/reports\", other=\"https://r.example/x\";p=1, bad=\"ftp://x/\"",
        );
        h.append(
            "Report-To",
            r#"{"group":"legacy","max_age":10,"endpoints":[{"url":"https://l.example/r"}]}"#,
        );
        let e = reporting_endpoints(&h, &base);
        assert_eq!(
            e,
            vec![
                (
                    "csp-endpoint".into(),
                    Url::parse("https://a.example/reports").unwrap()
                ),
                ("other".into(), Url::parse("https://r.example/x").unwrap()),
                ("legacy".into(), Url::parse("https://l.example/r").unwrap()),
            ]
        );
        let mut list = CspList::new();
        list.push(
            crate::csp::Policy::parse(
                "img-src 'none'; report-uri /a https://b.example/b",
                PolicySource::Header,
                Disposition::Enforce,
                base.origin(),
            )
            .0,
        );
        let v = violation("", Disposition::Enforce);
        let d = report_destinations(&list, &v, &e, &base);
        assert_eq!(
            d,
            vec![
                (Url::parse("https://a.example/a").unwrap(), false),
                (Url::parse("https://b.example/b").unwrap(), false)
            ]
        );
        // report-to wins over report-uri.
        let mut list = CspList::new();
        list.push(
            crate::csp::Policy::parse(
                "img-src 'none'; report-uri /a; report-to legacy",
                PolicySource::Header,
                Disposition::Enforce,
                base.origin(),
            )
            .0,
        );
        let d = report_destinations(&list, &v, &e, &base);
        assert_eq!(d, vec![(Url::parse("https://l.example/r").unwrap(), true)]);
    }

    #[test]
    fn x_frame_options() {
        let a = Url::parse("https://a.example/").unwrap().origin();
        let b = Url::parse("https://b.example/").unwrap().origin();
        let xfo = |v: &str| {
            let mut h = HeaderList::new();
            h.append("X-Frame-Options", v);
            h
        };
        let none = CspList::new();
        assert!(!x_frame_options_allows(
            &xfo("DENY"),
            &none,
            std::slice::from_ref(&a),
            &a
        ));
        assert!(x_frame_options_allows(
            &xfo("SAMEORIGIN"),
            &none,
            std::slice::from_ref(&a),
            &a
        ));
        assert!(!x_frame_options_allows(
            &xfo("sameorigin"),
            &none,
            &[a.clone(), b.clone()],
            &a
        ));
        assert!(x_frame_options_allows(
            &xfo("ALLOWALL"),
            &none,
            std::slice::from_ref(&b),
            &a
        ));
        assert!(!x_frame_options_allows(
            &xfo("deny, sameorigin"),
            &none,
            std::slice::from_ref(&a),
            &a
        ));
        assert!(x_frame_options_allows(
            &xfo("foo, bar"),
            &none,
            std::slice::from_ref(&b),
            &a
        ));
        assert!(x_frame_options_allows(
            &HeaderList::new(),
            &none,
            std::slice::from_ref(&b),
            &a
        ));
        // An enforced frame-ancestors makes X-Frame-Options moot.
        let mut csp = CspList::new();
        csp.push(
            crate::csp::Policy::parse(
                "frame-ancestors *",
                PolicySource::Header,
                Disposition::Enforce,
                a.clone(),
            )
            .0,
        );
        assert!(x_frame_options_allows(&xfo("DENY"), &csp, &[b], &a));
    }
}
