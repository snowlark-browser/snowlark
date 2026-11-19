// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Network configuration: the profile folder, timeouts, connection limits, TLS trust, host resolver rules.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::CertificateDer;

/// Which certificates TLS trusts.
#[derive(Clone, Debug, Default)]
pub enum TlsTrust {
    /// The operating system's store and policy (on Windows, CryptoAPI chain building with revocation).
    #[default]
    Platform,
    /// The platform's, plus extra roots (test certificate authorities, enterprise roots).
    PlatformWithExtraRoots(Vec<CertificateDer<'static>>),
    /// A fully custom rustls configuration (ALPN is set by the transport).
    Custom(Arc<rustls::ClientConfig>),
}

/// One `MAP <pattern> <target>` rule, as Chromium's `--host-resolver-rules` takes them: the pattern is a
/// host or `*.suffix`; the target is the host or address to connect to instead. TLS still verifies the
/// original host name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRule {
    pub pattern: String,
    pub target: String,
}

impl HostRule {
    /// Parse "MAP *.web-platform.test 127.0.0.1, MAP web-platform.test 127.0.0.1".
    pub fn parse_list(text: &str) -> Vec<HostRule> {
        text.split(',')
            .filter_map(|rule| {
                let parts: Vec<&str> = rule.split_whitespace().collect();
                (parts.len() == 3 && parts[0].eq_ignore_ascii_case("MAP")).then(|| HostRule {
                    pattern: parts[1].to_ascii_lowercase(),
                    target: parts[2].to_string(),
                })
            })
            .collect()
    }

    fn matches(&self, host: &str) -> bool {
        match self.pattern.strip_prefix('*') {
            Some(suffix) => host.ends_with(suffix),
            None => self.pattern == host,
        }
    }
}

/// Where to connect for `host` under `rules` (the host itself when no rule matches).
pub fn resolve_host<'a>(rules: &'a [HostRule], host: &'a str) -> &'a str {
    let lower = host.to_ascii_lowercase();
    rules
        .iter()
        .find(|r| r.matches(&lower))
        .map_or(host, |r| r.target.as_str())
}

/// Configuration for a [`crate::NetThread`].
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Where cookies.json and hsts.json live; `None` keeps everything in memory.
    pub profile_dir: Option<PathBuf>,
    pub user_agent: String,
    pub accept_language: String,
    /// Log each network request to stderr (as the C# engine's trace did).
    pub trace: bool,
    pub connect_timeout: Duration,
    /// From sending the request to receiving the response head.
    pub response_timeout: Duration,
    /// The longest silence between two body chunks.
    pub read_timeout: Duration,
    /// How long an idle HTTP/1.1 connection stays pooled.
    pub idle_timeout: Duration,
    /// Concurrent HTTP/1.1 connections per origin (HTTP/2 multiplexes on one).
    pub max_connections_per_host: usize,
    pub tls: TlsTrust,
    pub host_rules: Vec<HostRule>,
    /// Neither send nor store cookies on cross-site subresource requests.
    pub block_third_party_cookies: bool,
    /// How often changed cookies and HSTS hosts are written to the profile.
    pub save_interval: Duration,
    /// Keep responses in the HTTP cache (RFC 9111, in memory) and reuse them.
    pub http_cache: bool,
    /// How long a keepalive fetch whose page let go of it before its response may still run
    /// ([`crate::NetBackend::start`]); Chromium's `kDefaultDisconnectedKeepAliveURLLoaderTimeout`
    /// (`content/browser/loader/keep_alive_url_loader.cc`), 30 s.
    pub keepalive_detached_timeout: Duration,
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig {
            profile_dir: None,
            user_agent: crate::metadata::USER_AGENT.to_string(),
            accept_language: crate::metadata::ACCEPT_LANGUAGE.to_string(),
            trace: false,
            connect_timeout: Duration::from_secs(15),
            response_timeout: Duration::from_secs(60),
            read_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(90),
            max_connections_per_host: 6,
            tls: TlsTrust::Platform,
            host_rules: Vec::new(),
            block_third_party_cookies: true,
            save_interval: Duration::from_secs(1),
            http_cache: true,
            keepalive_detached_timeout: Duration::from_secs(30),
        }
    }
}

impl NetConfig {
    /// The defaults, plus `SNOWLARK_HOST_RESOLVER_RULES`, `SNOWLARK_TRACE=1` and
    /// `SNOWLARK_EXTRA_ROOT_CERTS=<pem file>` (certificate authorities trusted besides the platform's:
    /// the WPT runner's test CA; only ever from the environment, logged once when set) from the
    /// environment.
    pub fn from_env() -> Self {
        let mut config = NetConfig::default();
        if let Ok(rules) = std::env::var("SNOWLARK_HOST_RESOLVER_RULES") {
            config.host_rules = HostRule::parse_list(&rules);
        }
        config.trace = std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1");
        if let Some(path) = std::env::var_os("SNOWLARK_EXTRA_ROOT_CERTS") {
            static LOGGED: std::sync::Once = std::sync::Once::new();
            let path = PathBuf::from(path);
            match std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|pem| extra_roots_from_pem(&pem))
            {
                Ok(roots) => {
                    LOGGED.call_once(|| {
                        eprintln!(
                            "[net] SNOWLARK_EXTRA_ROOT_CERTS: trusting {} extra root certificate(s) from {}",
                            roots.len(),
                            path.display()
                        )
                    });
                    config.tls = TlsTrust::PlatformWithExtraRoots(roots);
                }
                Err(e) => LOGGED.call_once(|| {
                    eprintln!(
                        "[net] SNOWLARK_EXTRA_ROOT_CERTS: {} not used: {e}",
                        path.display()
                    )
                }),
            }
        }
        config
    }
}

/// The certificates of a PEM file (its `CERTIFICATE` blocks; other blocks are skipped). An error when
/// it has none or one does not decode.
pub fn extra_roots_from_pem(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, String> {
    use rustls::pki_types::pem::{PemObject, SectionKind};
    let mut certs = Vec::new();
    for section in <(SectionKind, Vec<u8>)>::pem_slice_iter(pem) {
        if let (SectionKind::Certificate, der) =
            section.map_err(|e| format!("not a PEM file: {e:?}"))?
        {
            certs.push(CertificateDer::from(der));
        }
    }
    if certs.is_empty() {
        return Err("no CERTIFICATE in the PEM file".into());
    }
    Ok(certs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_roots_come_from_the_certificate_blocks_of_a_pem_file() {
        let pem = [
            "junk before",
            "-----BEGIN PRIVATE KEY-----",
            "AAEC",
            "-----END PRIVATE KEY-----",
            "-----BEGIN CERTIFICATE-----",
            "AQID",
            "-----END CERTIFICATE-----",
            "-----BEGIN CERTIFICATE-----",
            "BAUG",
            "-----END CERTIFICATE-----",
        ]
        .join("\n");
        let roots = extra_roots_from_pem(pem.as_bytes()).unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].as_ref(), &[1, 2, 3]);
        assert_eq!(roots[1].as_ref(), &[4, 5, 6]);
        assert!(extra_roots_from_pem(b"no pem here").is_err());
        let key_only = "-----BEGIN PRIVATE KEY-----\nAAEC\n-----END PRIVATE KEY-----\n";
        assert!(extra_roots_from_pem(key_only.as_bytes()).is_err());
    }

    #[test]
    fn host_rules() {
        let rules = HostRule::parse_list(
            "MAP *.web-platform.test 127.0.0.1, MAP web-platform.test 127.0.0.2, junk",
        );
        assert_eq!(rules.len(), 2);
        assert_eq!(resolve_host(&rules, "www.Web-Platform.test"), "127.0.0.1");
        assert_eq!(resolve_host(&rules, "web-platform.test"), "127.0.0.2");
        assert_eq!(resolve_host(&rules, "example.com"), "example.com");
    }
}
