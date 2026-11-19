// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error pages: what a top-level navigation that fails shows (a DNS failure, a refused connection, a
//! timeout, a TLS or certificate failure, an HTTP error without a body, a blocked request). The page is
//! an internal `about:error` document made from one template: what happened, the URL, the error code
//! and a Reload button (a link to the URL, so it works without script). The embedder shows the failed
//! URL in its address bar ([`crate::Engine::error_page_url`]). Certificate errors name the host and the
//! reason and offer no way to proceed.

use snowlark_net::{NetError, NetErrorKind};

/// Why a navigation failed.
#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    Network(NetError),
    /// An HTTP error status with an empty body.
    Http(u16),
    /// A body that could not be read (a missing file, a bad `data:` URL).
    Unavailable,
}

/// The parts of an error page.
#[derive(Clone, Debug, PartialEq)]
pub struct ErrorText {
    pub heading: String,
    /// HTML (the host in bold).
    pub explanation: String,
    pub detail: Option<String>,
    pub code: String,
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// What the page says for a failure of `url`.
pub fn describe(url: &url::Url, failure: &Failure) -> ErrorText {
    let host = escape(url.host_str().unwrap_or(url.as_str()));
    let unreachable = "This site can\u{2019}t be reached".to_owned();
    match failure {
        Failure::Network(e) => match &e.kind {
            NetErrorKind::Dns => ErrorText {
                heading: unreachable,
                explanation: format!(
                    "<b>{host}</b>\u{2019}s server IP address could not be found. Check if there is a typo in the address."
                ),
                detail: None,
                code: "ERR_NAME_NOT_RESOLVED".into(),
            },
            NetErrorKind::Connect => ErrorText {
                heading: unreachable,
                explanation: format!("<b>{host}</b> refused to connect."),
                detail: None,
                code: "ERR_CONNECTION_REFUSED".into(),
            },
            NetErrorKind::Timeout => ErrorText {
                heading: unreachable,
                explanation: format!("<b>{host}</b> took too long to respond."),
                detail: None,
                code: "ERR_TIMED_OUT".into(),
            },
            NetErrorKind::Tls if e.message.contains("certificate") => ErrorText {
                heading: "Your connection is not private".into(),
                explanation: format!(
                    "Attackers might be trying to steal your information from <b>{host}</b> (for example, passwords, messages or credit cards). Its certificate was not accepted."
                ),
                detail: Some(escape(certificate_reason(&e.message))),
                code: "ERR_CERT_INVALID".into(),
            },
            NetErrorKind::Tls => ErrorText {
                heading: "This site can\u{2019}t provide a secure connection".into(),
                explanation: format!("<b>{host}</b> sent an invalid response."),
                detail: Some(escape(&e.message)),
                code: "ERR_SSL_PROTOCOL_ERROR".into(),
            },
            NetErrorKind::Blocked(reason) => ErrorText {
                heading: "This page has been blocked".into(),
                explanation: format!("Snowlark blocked the request to <b>{host}</b>."),
                detail: Some(escape(&e.message)),
                code: format!("ERR_BLOCKED_{}", format!("{reason:?}").to_ascii_uppercase()),
            },
            NetErrorKind::TooManyRedirects => ErrorText {
                heading: "This page isn\u{2019}t working".into(),
                explanation: format!("<b>{host}</b> redirected you too many times."),
                detail: None,
                code: "ERR_TOO_MANY_REDIRECTS".into(),
            },
            NetErrorKind::UnsupportedScheme | NetErrorKind::InvalidUrl => ErrorText {
                heading: "This page can\u{2019}t be shown".into(),
                explanation: "Snowlark can\u{2019}t open this address.".into(),
                detail: Some(escape(&e.message)),
                code: "ERR_INVALID_URL".into(),
            },
            _ => ErrorText {
                heading: unreachable,
                explanation: format!("The connection to <b>{host}</b> was interrupted."),
                detail: Some(escape(&e.message)),
                code: "ERR_CONNECTION_FAILED".into(),
            },
        },
        Failure::Http(status) => ErrorText {
            heading: if *status == 404 {
                format!("This {host} page can\u{2019}t be found")
            } else {
                "This page isn\u{2019}t working".into()
            },
            explanation: if *status == 404 {
                "No web page was found for the web address.".into()
            } else {
                format!("<b>{host}</b> is currently unable to handle this request.")
            },
            detail: None,
            code: format!("HTTP ERROR {status}"),
        },
        Failure::Unavailable => ErrorText {
            heading: "This page can\u{2019}t be shown".into(),
            explanation: "The file or data at this address could not be read.".into(),
            detail: None,
            code: "ERR_FILE_NOT_FOUND".into(),
        },
    }
}

/// The reason part of a TLS library's certificate error message.
fn certificate_reason(message: &str) -> &str {
    message
        .rsplit_once("failed: ")
        .map_or(message, |(_, r)| r)
        .trim()
}

/// The error page document for a failure of `url`.
pub fn html(url: &url::Url, failure: &Failure) -> String {
    let t = describe(url, failure);
    let title = escape(url.host_str().unwrap_or(url.as_str()));
    let shown_url = escape(url.as_str());
    let detail = t
        .detail
        .map(|d| format!("<p class=detail>{d}</p>"))
        .unwrap_or_default();
    format!(
        r#"<!doctype html><html lang=en><meta charset=utf-8><title>{title}</title>
<meta name=color-scheme content="light dark">
<style>
:root {{ color-scheme: light dark; --text: #3c4043; --head: #202124; --dim: #5f6368; --button: #1a73e8; --on: #fff; --bg: #fff; }}
@media (prefers-color-scheme: dark) {{ :root {{ --text: #bdc1c6; --head: #e8eaed; --dim: #9aa0a6; --button: #8ab4f8; --on: #202124; --bg: #202124; }} }}
html {{ background: var(--bg); }}
body {{ font: 15px/1.55 'Segoe UI', system-ui, sans-serif; color: var(--text); max-width: 600px; margin: 14vh auto 0; padding: 0 24px; }}
.icon {{ width: 72px; height: 72px; margin-bottom: 40px; border: 4px solid var(--dim); border-radius: 6px; box-sizing: border-box; position: relative; }}
.icon::before, .icon::after {{ content: ''; position: absolute; top: 22px; width: 8px; height: 8px; border-radius: 50%; background: var(--dim); }}
.icon::before {{ left: 16px; }} .icon::after {{ right: 16px; }}
h1 {{ font-size: 24px; font-weight: 500; line-height: 1.25; color: var(--head); margin: 0 0 16px; }}
.url {{ color: var(--dim); word-break: break-all; }}
.detail {{ font-size: 13px; color: var(--dim); word-break: break-word; }}
.code {{ font-size: 12px; color: var(--dim); text-transform: uppercase; margin-top: 16px; }}
.button {{ display: inline-block; margin-top: 48px; padding: 8px 24px; border-radius: 18px; background: var(--button); color: var(--on); text-decoration: none; font-weight: 500; }}
</style>
<div class=icon></div>
<h1>{heading}</h1>
<p>{explanation}</p>
<p class=url>{shown_url}</p>
{detail}
<div class=code>{code}</div>
<a class=button id=reload href="{shown_url}">Reload</a>
"#,
        heading = t.heading,
        explanation = t.explanation,
        code = escape(&t.code),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn failures_are_described() {
        let u = url("https://no-such-host.invalid/a?b=<c>");
        let dns = describe(
            &u,
            &Failure::Network(NetError::new(NetErrorKind::Dns, "can't resolve")),
        );
        assert_eq!(dns.code, "ERR_NAME_NOT_RESOLVED");
        assert!(dns.explanation.contains("<b>no-such-host.invalid</b>"));
        let cert = describe(
            &u,
            &Failure::Network(NetError::new(
                NetErrorKind::Tls,
                "TLS handshake with x failed: invalid peer certificate: UnknownIssuer",
            )),
        );
        assert_eq!(cert.heading, "Your connection is not private");
        assert_eq!(
            cert.detail.as_deref(),
            Some("invalid peer certificate: UnknownIssuer")
        );
        let http = describe(&u, &Failure::Http(500));
        assert_eq!(http.code, "HTTP ERROR 500");
        assert!(describe(&u, &Failure::Http(404))
            .heading
            .contains("can\u{2019}t be found"));
    }

    #[test]
    fn pages_escape_the_url_and_never_offer_to_proceed() {
        let u = url("https://evil.example/\"><script>alert(1)</script>");
        let page = html(
            &u,
            &Failure::Network(NetError::new(
                NetErrorKind::Tls,
                "TLS handshake failed: invalid peer certificate: Expired",
            )),
        );
        assert!(!page.contains("<script>alert"));
        assert!(page.contains("id=reload href=\"https://evil.example/%22%3E%3Cscript%3E"));
        assert!(!page.to_ascii_lowercase().contains("proceed"));
    }
}
