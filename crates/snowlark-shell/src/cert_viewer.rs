// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark://certificate/?id=N`: the certificate chain a site's server presented, which the
//! connection's verifier accepted, as the site information panel's "Certificate is valid" opens it.
//! "General" names whom the site's certificate was issued to and by, when it is valid, and the
//! SHA-256 fingerprints of the certificate and of its public key; "Details" lists the chain and
//! every field of the chosen certificate (RFC 5280 §4.1), with the extensions decoded where their
//! syntax is known. The page only shows what was verified: it decides nothing.
//!
//! The browser keeps the last few chains opened ([`Chains`]); the page names one by its id.

use std::fmt::Write as _;
use std::sync::Arc;

use const_oid::ObjectIdentifier;
use sha2::{Digest, Sha256};
use snowlark_net::{TlsInfo, Url};
use x509_cert::der::asn1::{
    BmpString, Ia5StringRef, PrintableStringRef, TeletexStringRef, Utf8StringRef,
};
use x509_cert::der::{Any, Decode, Encode};
use x509_cert::ext::pkix::name::{DistributionPointName, GeneralName};
use x509_cert::ext::pkix::{
    AuthorityInfoAccessSyntax, AuthorityKeyIdentifier, BasicConstraints, CertificatePolicies,
    CrlDistributionPoints, ExtendedKeyUsage, KeyUsage, SubjectAltName, SubjectKeyIdentifier,
};
use x509_cert::name::Name;
use x509_cert::Certificate;

use crate::history_page::{colors, friendly_date, time_of_day};
use crate::settings::{escape_html, form_pairs};

pub const CERTIFICATE_PAGE: &str = "snowlark://certificate/";

/// How many chains the browser keeps for the page (the oldest goes first).
const KEPT: usize = 16;

pub fn is_certificate_url(url: &Url) -> bool {
    url.scheme() == "snowlark" && url.host_str() == Some("certificate")
}

/// The chains the panel opened the page for, by id.
#[derive(Default)]
pub struct Chains {
    next: u64,
    kept: Vec<(u64, String, Arc<TlsInfo>)>,
}

impl Chains {
    /// Keeps the chain of `host`'s connection; the page's address for it.
    pub fn add(&mut self, host: &str, tls: Arc<TlsInfo>) -> String {
        if let Some((id, _, _)) = self
            .kept
            .iter()
            .find(|(_, h, t)| h == host && Arc::ptr_eq(t, &tls))
        {
            return page_url(*id);
        }
        self.next += 1;
        if self.kept.len() == KEPT {
            self.kept.remove(0);
        }
        self.kept.push((self.next, host.to_owned(), tls));
        page_url(self.next)
    }

    pub fn get(&self, id: u64) -> Option<(&str, &TlsInfo)> {
        self.kept
            .iter()
            .find(|(i, _, _)| *i == id)
            .map(|(_, h, t)| (h.as_str(), &**t))
    }
}

pub fn page_url(id: u64) -> String {
    format!("{CERTIFICATE_PAGE}?id={id}")
}

/// The page's tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    General,
    Details,
}

/// What the address asks for: the chain's id, the tab, and the certificate chosen in the chain
/// (0: the server's own).
pub fn view_of(url: &Url) -> (Option<u64>, Tab, usize) {
    let pairs = form_pairs(url.query().unwrap_or(""));
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let tab = match get("tab") {
        Some("details") => Tab::Details,
        _ => Tab::General,
    };
    (
        get("id").and_then(|v| v.parse().ok()),
        tab,
        get("cert").and_then(|v| v.parse().ok()).unwrap_or(0),
    )
}

/// A name's attributes as the viewer's General tab shows them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameParts {
    pub common_name: Option<String>,
    pub organization: Option<String>,
    pub unit: Option<String>,
}

/// What the viewer shows of one certificate.
#[derive(Clone, Debug)]
pub struct CertView {
    pub subject: NameParts,
    pub issuer: NameParts,
    /// Seconds since the epoch.
    pub not_before: i64,
    pub not_after: i64,
    pub sha256: [u8; 32],
    /// The SHA-256 of the DER SubjectPublicKeyInfo.
    pub spki_sha256: [u8; 32],
    /// The subject as one line (its common name, else the whole name).
    pub title: String,
    pub fields: Field,
}

/// A node of the certificate's field tree: a name, a value (empty for a group), its children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub value: String,
    pub children: Vec<Field>,
}

impl Field {
    fn leaf(name: &str, value: String) -> Field {
        Field {
            name: name.to_owned(),
            value,
            children: Vec::new(),
        }
    }

    fn group(name: &str, children: Vec<Field>) -> Field {
        Field {
            name: name.to_owned(),
            value: String::new(),
            children,
        }
    }
}

/// Reads a DER certificate.
pub fn parse(der: &[u8]) -> Option<CertView> {
    let cert = Certificate::from_der(der).ok()?;
    let tbs = &cert.tbs_certificate;
    let subject = name_parts(&tbs.subject);
    let title = subject
        .common_name
        .clone()
        .or_else(|| subject.organization.clone())
        .unwrap_or_else(|| tbs.subject.to_string());
    let spki = tbs.subject_public_key_info.to_der().ok()?;
    Some(CertView {
        issuer: name_parts(&tbs.issuer),
        not_before: tbs.validity.not_before.to_unix_duration().as_secs() as i64,
        not_after: tbs.validity.not_after.to_unix_duration().as_secs() as i64,
        sha256: Sha256::digest(der).into(),
        spki_sha256: Sha256::digest(&spki).into(),
        fields: fields(&cert, &title),
        subject,
        title,
    })
}

/// Whom a DER certificate was issued by, for the panel: the issuer's common name, else its
/// organization, else the whole name.
pub fn issuer_of(der: &[u8]) -> Option<String> {
    let cert = Certificate::from_der(der).ok()?;
    let issuer = &cert.tbs_certificate.issuer;
    let parts = name_parts(issuer);
    Some(
        parts
            .common_name
            .or(parts.organization)
            .unwrap_or_else(|| issuer.to_string()),
    )
}

const CN: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.3");
const O: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.10");
const OU: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.11");

fn name_parts(name: &Name) -> NameParts {
    let first = |oid: ObjectIdentifier| {
        name.0
            .iter()
            .flat_map(|rdn| rdn.0.iter())
            .find(|atv| atv.oid == oid)
            .map(|atv| text_of(&atv.value))
    };
    NameParts {
        common_name: first(CN),
        organization: first(O),
        unit: first(OU),
    }
}

/// A directory string's text (RFC 5280 §4.1.2.4), or its bytes in hex.
fn text_of(value: &Any) -> String {
    if let Ok(s) = value.decode_as::<Utf8StringRef>() {
        return s.to_string();
    }
    if let Ok(s) = value.decode_as::<PrintableStringRef>() {
        return s.to_string();
    }
    if let Ok(s) = value.decode_as::<Ia5StringRef>() {
        return s.to_string();
    }
    if let Ok(s) = value.decode_as::<TeletexStringRef>() {
        return s.to_string();
    }
    if let Ok(s) = value.decode_as::<BmpString>() {
        return s.to_string();
    }
    hex(value.value(), ' ')
}

/// The short name of a name attribute ("CN"), else its name in the OID registry, else its OID.
fn attribute_name(oid: &ObjectIdentifier) -> String {
    let short = match oid.to_string().as_str() {
        "2.5.4.3" => "CN",
        "2.5.4.6" => "C",
        "2.5.4.7" => "L",
        "2.5.4.8" => "ST",
        "2.5.4.9" => "STREET",
        "2.5.4.10" => "O",
        "2.5.4.11" => "OU",
        "2.5.4.5" => "serialNumber",
        "2.5.4.15" => "businessCategory",
        "0.9.2342.19200300.100.1.25" => "DC",
        "1.2.840.113549.1.9.1" => "E",
        "1.3.6.1.4.1.311.60.2.1.3" => "jurisdictionC",
        "1.3.6.1.4.1.311.60.2.1.2" => "jurisdictionST",
        "1.3.6.1.4.1.311.60.2.1.1" => "jurisdictionL",
        _ => return oid_name(oid),
    };
    short.to_owned()
}

/// An OID's name in the registry, else the OID itself.
fn oid_name(oid: &ObjectIdentifier) -> String {
    const_oid::db::DB
        .by_oid(oid)
        .map(str::to_owned)
        .unwrap_or_else(|| oid.to_string())
}

/// A name one attribute per line, most significant last as written ("CN = a.example").
fn name_lines(name: &Name) -> String {
    let mut out = String::new();
    for atv in name.0.iter().flat_map(|rdn| rdn.0.iter()) {
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = write!(
            out,
            "{} = {}",
            attribute_name(&atv.oid),
            text_of(&atv.value)
        );
    }
    out
}

/// The algorithm's name as people read it.
fn algorithm_name(oid: &ObjectIdentifier) -> String {
    let name = match oid.to_string().as_str() {
        "1.2.840.113549.1.1.1" => "RSA Encryption",
        "1.2.840.113549.1.1.5" => "SHA-1 With RSA Encryption",
        "1.2.840.113549.1.1.11" => "SHA-256 With RSA Encryption",
        "1.2.840.113549.1.1.12" => "SHA-384 With RSA Encryption",
        "1.2.840.113549.1.1.13" => "SHA-512 With RSA Encryption",
        "1.2.840.113549.1.1.10" => "RSA-PSS",
        "1.2.840.10045.2.1" => "Elliptic Curve Public Key",
        "1.2.840.10045.4.3.2" => "ECDSA Signature with SHA-256",
        "1.2.840.10045.4.3.3" => "ECDSA Signature with SHA-384",
        "1.2.840.10045.4.3.4" => "ECDSA Signature with SHA-512",
        "1.3.101.112" => "Ed25519",
        "1.3.101.113" => "Ed448",
        _ => return oid_name(oid),
    };
    name.to_owned()
}

/// The named curve of an elliptic curve key.
fn curve_name(params: Option<&Any>) -> Option<&'static str> {
    let oid: ObjectIdentifier = params?.decode_as().ok()?;
    Some(match oid.to_string().as_str() {
        "1.2.840.10045.3.1.7" => "P-256",
        "1.3.132.0.34" => "P-384",
        "1.3.132.0.35" => "P-521",
        _ => return None,
    })
}

/// Bytes in hex, two digits each, `sep` between them, 16 to a line for long ones.
pub fn hex(bytes: &[u8], sep: char) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(if bytes.len() > 20 && i % 16 == 0 {
                '\n'
            } else {
                sep
            });
        }
        let _ = write!(out, "{b:02X}");
    }
    out
}

/// "Tuesday, November 4, 2025 at 1:00 AM", local time.
fn date_time(secs: i64, offset: &dyn Fn(i64) -> i64) -> String {
    let ms = secs * 1000;
    let local = ms + offset(ms);
    format!(
        "{} at {}",
        friendly_date(local.div_euclid(86_400_000)),
        time_of_day(local)
    )
}

fn general_name(n: &GeneralName) -> String {
    match n {
        GeneralName::DnsName(s) => format!("DNS Name: {s}"),
        GeneralName::Rfc822Name(s) => format!("Email Address: {s}"),
        GeneralName::UniformResourceIdentifier(s) => format!("URI: {s}"),
        GeneralName::IpAddress(b) => {
            let b = b.as_bytes();
            let ip = match b.len() {
                4 => std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string(),
                16 => {
                    let mut a = [0u8; 16];
                    a.copy_from_slice(b);
                    std::net::Ipv6Addr::from(a).to_string()
                }
                _ => hex(b, ':'),
            };
            format!("IP Address: {ip}")
        }
        GeneralName::DirectoryName(name) => format!("Directory Name: {name}"),
        GeneralName::RegisteredId(oid) => format!("Registered ID: {oid}"),
        GeneralName::OtherName(o) => format!("Other Name: {}", oid_name(&o.type_id)),
        GeneralName::EdiPartyName(_) => "EDI Party Name".to_owned(),
    }
}

/// An extension's name and its value as text: decoded where the syntax is known (RFC 5280 §4.2),
/// in hex otherwise.
fn extension(oid: &ObjectIdentifier, critical: bool, der: &[u8]) -> (String, String) {
    let id = oid.to_string();
    let decoded: Option<(&str, String)> = match id.as_str() {
        "2.5.29.17" => SubjectAltName::from_der(der).ok().map(|n| {
            (
                "Subject Alternative Name",
                n.0.iter().map(general_name).collect::<Vec<_>>().join("\n"),
            )
        }),
        "2.5.29.15" => KeyUsage::from_der(der).ok().map(|k| {
            // ("DigitalSignature" read as "Digital Signature")
            let names: Vec<String> =
                k.0.into_iter()
                    .map(|f| {
                        let mut s = String::new();
                        for c in format!("{f:?}").chars() {
                            if c.is_ascii_uppercase() && !s.is_empty() {
                                s.push(' ');
                            }
                            s.push(c);
                        }
                        s
                    })
                    .collect();
            ("Key Usage", names.join(", "))
        }),
        "2.5.29.37" => ExtendedKeyUsage::from_der(der).ok().map(|e| {
            let names: Vec<String> =
                e.0.iter()
                    .map(|o| match o.to_string().as_str() {
                        "1.3.6.1.5.5.7.3.1" => "TLS Web Server Authentication".to_owned(),
                        "1.3.6.1.5.5.7.3.2" => "TLS Web Client Authentication".to_owned(),
                        _ => oid_name(o),
                    })
                    .collect();
            ("Extended Key Usage", names.join("\n"))
        }),
        "2.5.29.19" => BasicConstraints::from_der(der).ok().map(|b| {
            let mut v = if b.ca {
                "Is a Certification Authority".to_owned()
            } else {
                "Is not a Certification Authority".to_owned()
            };
            if let Some(n) = b.path_len_constraint {
                let _ = write!(v, "\nMaximum number of intermediate CAs: {n}");
            }
            ("Basic Constraints", v)
        }),
        "2.5.29.14" => SubjectKeyIdentifier::from_der(der)
            .ok()
            .map(|k| ("Subject Key ID", hex(k.0.as_bytes(), ' '))),
        "2.5.29.35" => AuthorityKeyIdentifier::from_der(der).ok().map(|k| {
            let v = k
                .key_identifier
                .map(|id| format!("Key ID: {}", hex(id.as_bytes(), ' ')))
                .unwrap_or_default();
            ("Authority Key ID", v)
        }),
        "1.3.6.1.5.5.7.1.1" => AuthorityInfoAccessSyntax::from_der(der).ok().map(|a| {
            let lines: Vec<String> =
                a.0.iter()
                    .map(|d| {
                        let method = match d.access_method.to_string().as_str() {
                            "1.3.6.1.5.5.7.48.1" => "OCSP Responder".to_owned(),
                            "1.3.6.1.5.5.7.48.2" => "CA Issuers".to_owned(),
                            _ => oid_name(&d.access_method),
                        };
                        format!("{method}: {}", general_name(&d.access_location))
                    })
                    .collect();
            ("Authority Information Access", lines.join("\n"))
        }),
        "2.5.29.31" => CrlDistributionPoints::from_der(der).ok().map(|c| {
            let lines: Vec<String> =
                c.0.iter()
                    .filter_map(|p| match &p.distribution_point {
                        Some(DistributionPointName::FullName(names)) => Some(
                            names
                                .iter()
                                .map(general_name)
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        Some(DistributionPointName::NameRelativeToCRLIssuer(_)) | None => None,
                    })
                    .collect();
            ("CRL Distribution Points", lines.join("\n"))
        }),
        "2.5.29.32" => CertificatePolicies::from_der(der).ok().map(|p| {
            let lines: Vec<String> =
                p.0.iter()
                    .map(|i| match i.policy_identifier.to_string().as_str() {
                        "2.23.140.1.2.1" => "Domain Validated (2.23.140.1.2.1)".to_owned(),
                        "2.23.140.1.2.2" => "Organization Validated (2.23.140.1.2.2)".to_owned(),
                        "2.23.140.1.2.3" => "Individual Validated (2.23.140.1.2.3)".to_owned(),
                        "2.23.140.1.1" => "Extended Validation (2.23.140.1.1)".to_owned(),
                        _ => format!("Policy: {}", i.policy_identifier),
                    })
                    .collect();
            ("Certificate Policies", lines.join("\n"))
        }),
        "1.3.6.1.4.1.11129.2.4.2" => Some(("Signed Certificate Timestamp List", hex(der, ' '))),
        _ => None,
    };
    let (name, value) = match decoded {
        Some((n, v)) => (n.to_owned(), v),
        None => (oid_name(oid), hex(der, ' ')),
    };
    let critical = if critical { "Critical" } else { "Not Critical" };
    (name, format!("{critical}\n{value}"))
}

/// The certificate's fields (RFC 5280 §4.1), under the certificate's title.
fn fields(cert: &Certificate, title: &str) -> Field {
    let tbs = &cert.tbs_certificate;
    let spki = &tbs.subject_public_key_info;
    let key_alg = {
        let mut n = algorithm_name(&spki.algorithm.oid);
        if let Some(c) = curve_name(spki.algorithm.parameters.as_ref()) {
            let _ = write!(n, " ({c})");
        }
        n
    };
    let key_bits = spki.subject_public_key.raw_bytes();
    let mut children = vec![
        Field::leaf("Version", format!("{:?}", tbs.version).to_uppercase()),
        Field::leaf("Serial Number", hex(tbs.serial_number.as_bytes(), ':')),
        Field::leaf(
            "Certificate Signature Algorithm",
            algorithm_name(&tbs.signature.oid),
        ),
        Field::leaf("Issuer", name_lines(&tbs.issuer)),
        Field::group(
            "Validity",
            vec![
                Field::leaf(
                    "Not Before",
                    format!("{} GMT", utc(tbs.validity.not_before.to_unix_duration())),
                ),
                Field::leaf(
                    "Not After",
                    format!("{} GMT", utc(tbs.validity.not_after.to_unix_duration())),
                ),
            ],
        ),
        Field::leaf("Subject", name_lines(&tbs.subject)),
        Field::group(
            "Subject Public Key Info",
            vec![
                Field::leaf("Subject Public Key Algorithm", key_alg),
                Field::leaf(
                    "Subject's Public Key",
                    format!("{} bytes:\n{}", key_bits.len(), hex(key_bits, ' ')),
                ),
            ],
        ),
    ];
    if let Some(exts) = &tbs.extensions {
        let list = exts
            .iter()
            .map(|e| {
                let (name, value) = extension(&e.extn_id, e.critical, e.extn_value.as_bytes());
                Field::leaf(&name, value)
            })
            .collect();
        children.push(Field::group("Extensions", list));
    }
    children.push(Field::leaf(
        "Certificate Signature Algorithm",
        algorithm_name(&cert.signature_algorithm.oid),
    ));
    let sig = cert.signature.raw_bytes();
    children.push(Field::leaf(
        "Certificate Signature Value",
        format!("{} bytes:\n{}", sig.len(), hex(sig, ' ')),
    ));
    Field::group(title, children)
}

/// "Tuesday, November 4, 2025 at 1:00 AM" in UTC.
fn utc(d: std::time::Duration) -> String {
    date_time(d.as_secs() as i64, &|_| 0)
}

fn row(out: &mut String, name: &str, value: Option<&str>) {
    let value = match value {
        Some(v) => escape_html(v),
        None => "<span class=absent>&lt;Not part of certificate&gt;</span>".to_owned(),
    };
    let _ = write!(
        out,
        "<div class=row><div class=attr>{}</div><div class=value>{value}</div></div>",
        escape_html(name)
    );
}

fn field_html(out: &mut String, f: &Field, depth: usize) {
    let open = if f.children.is_empty() || depth > 1 {
        ""
    } else {
        " open"
    };
    let _ = write!(
        out,
        "<details{open}><summary>{}</summary>",
        escape_html(&f.name)
    );
    if !f.value.is_empty() {
        let _ = write!(out, "<pre>{}</pre>", escape_html(&f.value));
    }
    for c in &f.children {
        field_html(out, c, depth + 1);
    }
    out.push_str("</details>");
}

/// The page for the chain `id` (none: a note that it is gone), on `tab`, with certificate `chosen`
/// of the chain shown in Details.
pub fn certificate_html(
    chain: Option<(&str, &TlsInfo)>,
    id: u64,
    tab: Tab,
    chosen: usize,
    dark: bool,
    offset: &dyn Fn(i64) -> i64,
) -> String {
    let (scheme, colors) = colors(dark);
    let views: Vec<Option<CertView>> = chain
        .map(|(_, t)| t.certificates.iter().map(|c| parse(c)).collect())
        .unwrap_or_default();
    let leaf = views.first().and_then(Option::as_ref);
    let title = match leaf {
        Some(v) => format!("Certificate Viewer: {}", v.title),
        None => "Certificate Viewer".to_owned(),
    };
    let mut body = String::new();
    match (chain, leaf) {
        (None, _) => body.push_str(
            "<p class=note>This certificate is no longer available. Open it again from the site \
             information panel (the button at the start of the address bar).</p>",
        ),
        (Some(_), None) => {
            body.push_str("<p class=note>The certificate could not be read.</p>");
        }
        (Some((host, tls)), Some(leaf)) => {
            let link =
                |t: &str, c: usize| format!("{CERTIFICATE_PAGE}?id={id}&amp;tab={t}&amp;cert={c}");
            let _ = write!(
                body,
                "<nav role=tablist><a role=tab href=\"{}\"{}>General</a><a role=tab href=\"{}\"{}>Details</a></nav>",
                link("general", 0),
                if tab == Tab::General { " aria-selected=true" } else { "" },
                link("details", chosen.min(views.len() - 1)),
                if tab == Tab::Details { " aria-selected=true" } else { "" },
            );
            match tab {
                Tab::General => {
                    body.push_str("<section>");
                    let _ = write!(
                        body,
                        "<p class=verified>The connection to {} used this certificate, which was \
                         verified{}.</p>",
                        escape_html(host),
                        match tls.verifier {
                            snowlark_net::CertVerifier::System =>
                                " by the operating system's certificate store",
                            snowlark_net::CertVerifier::SystemWithExtraRoots =>
                                " by the operating system's certificate store and the roots the browser was started with",
                            snowlark_net::CertVerifier::Custom => "",
                        }
                    );
                    body.push_str("<h3>Issued To</h3>");
                    row(
                        &mut body,
                        "Common Name (CN)",
                        leaf.subject.common_name.as_deref(),
                    );
                    row(
                        &mut body,
                        "Organization (O)",
                        leaf.subject.organization.as_deref(),
                    );
                    row(
                        &mut body,
                        "Organizational Unit (OU)",
                        leaf.subject.unit.as_deref(),
                    );
                    body.push_str("<h3>Issued By</h3>");
                    row(
                        &mut body,
                        "Common Name (CN)",
                        leaf.issuer.common_name.as_deref(),
                    );
                    row(
                        &mut body,
                        "Organization (O)",
                        leaf.issuer.organization.as_deref(),
                    );
                    row(
                        &mut body,
                        "Organizational Unit (OU)",
                        leaf.issuer.unit.as_deref(),
                    );
                    body.push_str("<h3>Validity Period</h3>");
                    row(
                        &mut body,
                        "Issued On",
                        Some(&date_time(leaf.not_before, offset)),
                    );
                    row(
                        &mut body,
                        "Expires On",
                        Some(&date_time(leaf.not_after, offset)),
                    );
                    body.push_str("<h3>SHA-256 Fingerprints</h3>");
                    let _ = write!(
                        body,
                        "<div class=row><div class=attr>Certificate</div><div class=\"value hash\">{}</div></div>\
                         <div class=row><div class=attr>Public Key</div><div class=\"value hash\">{}</div></div>",
                        hex(&leaf.sha256, ' ').to_lowercase().replace(' ', ""),
                        hex(&leaf.spki_sha256, ' ').to_lowercase().replace(' ', ""),
                    );
                    let _ = write!(
                        body,
                        "<h3>Connection</h3><div class=row><div class=attr>Protocol</div><div class=value>{}</div></div>\
                         <div class=row><div class=attr>Cipher Suite</div><div class=value>{}</div></div>",
                        escape_html(&tls.version.replace("TLSv1_", "TLS 1.")),
                        escape_html(&tls.cipher_suite),
                    );
                    if let Some(k) = &tls.key_exchange {
                        row(&mut body, "Key Exchange", Some(k));
                    }
                    body.push_str("</section>");
                }
                Tab::Details => {
                    let chosen = chosen.min(views.len() - 1);
                    body.push_str("<section><h3>Certificate Hierarchy</h3><ol class=chain>");
                    // (the issuer at the top, as the chain is drawn; the server's own last)
                    for (depth, (i, v)) in views.iter().enumerate().rev().enumerate() {
                        let name = v
                            .as_ref()
                            .map(|v| v.title.clone())
                            .unwrap_or_else(|| "(unreadable)".to_owned());
                        let _ = write!(
                            body,
                            "<li style=\"padding-left:{}px\"><a href=\"{}\"{}>{}</a></li>",
                            depth * 16,
                            link("details", i),
                            if i == chosen {
                                " aria-current=true"
                            } else {
                                ""
                            },
                            escape_html(&name)
                        );
                    }
                    body.push_str("</ol><h3>Certificate Fields</h3><div class=fields>");
                    match &views[chosen] {
                        Some(v) => field_html(&mut body, &v.fields, 0),
                        None => {
                            body.push_str("<p class=note>The certificate could not be read.</p>")
                        }
                    }
                    body.push_str("</div></section>");
                }
            }
        }
    }
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{}</title><style>\
html{{color-scheme:{scheme};{colors}}}\
body{{font:14px 'Segoe UI',sans-serif;margin:0;background:var(--bg);color:var(--text)}}\
main{{max-width:720px;margin:0 auto;padding:16px 24px 64px}}\
h1{{font:500 20px 'Segoe UI',sans-serif;margin:8px 0 16px;overflow-wrap:anywhere}}\
nav{{display:flex;gap:4px;border-bottom:1px solid var(--line);margin-bottom:8px}}\
nav a{{padding:10px 16px;color:var(--muted);text-decoration:none;border-bottom:2px solid transparent}}\
nav a[aria-selected]{{color:var(--accent);border-bottom-color:var(--accent)}}nav a:hover{{background:var(--hover)}}\
section{{background:var(--card);border-radius:8px;padding:8px 20px 16px;box-shadow:0 0 0 1px var(--line)}}\
h3{{font:600 14px 'Segoe UI',sans-serif;margin:18px 0 8px}}\
.row{{display:flex;gap:16px;padding:3px 0}}.attr{{flex:none;width:220px;color:var(--muted)}}\
.value{{flex:1;min-width:0;overflow-wrap:anywhere}}.hash{{font:13px Consolas,monospace}}.absent{{color:var(--muted)}}\
.verified,.note{{color:var(--muted)}}\
ol.chain{{list-style:none;margin:0;padding:0}}ol.chain a{{display:block;padding:4px 8px;border-radius:4px;color:var(--text);text-decoration:none}}\
ol.chain a:hover{{background:var(--hover)}}ol.chain a[aria-current]{{background:var(--sel);color:var(--sel-text)}}\
.fields details{{margin-left:16px}}.fields>details{{margin-left:0}}summary{{padding:3px 0;cursor:pointer}}\
pre{{font:13px Consolas,monospace;white-space:pre-wrap;overflow-wrap:anywhere;margin:2px 0 6px 16px;padding:6px 8px;border-radius:4px;background:var(--field)}}\
</style></head><body><main><h1>{}</h1>{body}</main></body></html>",
        escape_html(&title),
        escape_html(&title),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_views() {
        assert_eq!(hex(&[0x0a, 0xff], ':'), "0A:FF");
        let u = Url::parse("snowlark://certificate/?id=3&tab=details&cert=1").unwrap();
        assert!(is_certificate_url(&u));
        assert_eq!(view_of(&u), (Some(3), Tab::Details, 1));
        let u = Url::parse("snowlark://certificate/?id=x").unwrap();
        assert_eq!(view_of(&u), (None, Tab::General, 0));
    }

    #[test]
    fn chains_are_kept_by_id() {
        let tls = Arc::new(TlsInfo {
            version: "TLSv1_3".into(),
            cipher_suite: "TLS13_AES_128_GCM_SHA256".into(),
            key_exchange: None,
            certificates: Vec::new(),
            verifier: snowlark_net::CertVerifier::System,
        });
        let mut c = Chains::default();
        let a = c.add("a.example", tls.clone());
        assert_eq!(a, "snowlark://certificate/?id=1");
        // (the same chain again: the same page)
        assert_eq!(c.add("a.example", tls.clone()), a);
        for i in 0..KEPT {
            c.add(&format!("h{i}.example"), Arc::new((*tls).clone()));
        }
        assert!(c.get(1).is_none(), "the oldest goes");
        assert_eq!(c.get(2).map(|(h, _)| h), Some("h0.example"));
        let page = certificate_html(None, 1, Tab::General, 0, false, &|_| 0);
        assert!(page.contains("no longer available"));
    }
}
