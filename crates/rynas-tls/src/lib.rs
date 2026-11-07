//! Certificates of the rynas web interface (wave W0 of `docs/research/web-access.md`).
//!
//! - A **local CA** per NAS (ECDSA P-256, 10 years, `CN=rynas local CA (<host>)`): trusting it
//!   once ("Trust this NAS") keeps working when the leaf is re-issued for a new host name or
//!   address. The future Caddy web-access module (W1) takes the same root as its `pki` CA, so
//!   there is one root to trust.
//! - A **leaf** issued by it (ECDSA P-256, 397 days, `serverAuth`), with every name the NAS is
//!   reached by: the host name, `<host>.local`, `localhost`, the loopback and interface
//!   addresses ([`names`]).
//! - **Import** of a certificate chain and key from the administrator ([`validate_import`]):
//!   key matching the leaf, chain links verified, validity, `serverAuth`.
//! - **Details** of a certificate for the API ([`inspect`]).
//!
//! Pure functions over PEM text: files, permissions and reloads belong to the callers (rynasd
//! writes, rynas-web serves).

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType, SerialNumber,
};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

use rynas_api_types::tls::{TlsCertificate, TlsWarning};

/// Lifetime of the leaf: within the 398 days browsers accept from public CAs, and well within
/// Apple's 825-day limit that applies to every TLS server certificate.
pub const LEAF_DAYS: i64 = 397;
/// Lifetime of the local CA (Caddy's local root uses 10 years too).
pub const CA_DAYS: i64 = 3650;
/// A local leaf is re-issued this long before it expires; imported ones get a warning.
pub const RENEW_DAYS: i64 = 30;
/// Largest accepted PEM text (certificate chain or key).
pub const MAX_PEM: usize = 64 * 1024;
/// Most certificates in an imported chain.
pub const MAX_CHAIN: usize = 8;

const DAY_MS: i64 = 86_400_000;

/// An error with the request field it is about (`certificate`, `private_key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsError {
    pub field: &'static str,
    pub message: String,
}

impl TlsError {
    fn cert(message: impl Into<String>) -> Self {
        Self {
            field: "certificate",
            message: message.into(),
        }
    }
    fn key(message: impl Into<String>) -> Self {
        Self {
            field: "private_key",
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for TlsError {}

/// A certificate chain (leaf first) and its private key, PEM.
#[derive(Clone, PartialEq, Eq)]
pub struct PemPair {
    pub chain: String,
    pub key: String,
}

impl std::fmt::Debug for PemPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PemPair")
            .field("chain", &self.chain)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// The names a certificate of this NAS should cover.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    pub dns: BTreeSet<String>,
    pub ips: BTreeSet<IpAddr>,
}

impl Names {
    /// Every name as text (DNS names first), for messages and `missing_names`.
    pub fn all(&self) -> Vec<String> {
        self.dns
            .iter()
            .cloned()
            .chain(self.ips.iter().map(|i| i.to_string()))
            .collect()
    }
}

/// The names of a NAS called `hostname` with these interface addresses:
/// - DNS: the host name, `<first label>.local` (mDNS, Avahi), the full name when the host
///   name has dots, and `localhost` (SSH tunnels);
/// - IP: `127.0.0.1`, `::1` and the addresses given, except IPv6 link-local (`fe80::/10`:
///   browsers do not accept zone identifiers in URLs, so such a SAN could never match),
///   multicast and unspecified ones. IPv4 link-local (`169.254/16`) stays: it is how a NAS
///   without DHCP is reached directly.
pub fn names(hostname: &str, addrs: &[IpAddr]) -> Names {
    let mut n = Names::default();
    let host = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
    if valid_dns_name(&host) {
        let first = host.split('.').next().unwrap_or(&host).to_string();
        n.dns.insert(format!("{first}.local"));
        n.dns.insert(first);
        n.dns.insert(host);
    }
    n.dns.insert("localhost".into());
    n.ips.insert(IpAddr::from([127, 0, 0, 1]));
    n.ips.insert(IpAddr::from(std::net::Ipv6Addr::LOCALHOST));
    for ip in addrs {
        let skip = match ip {
            IpAddr::V4(v4) => v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast(),
            IpAddr::V6(v6) => {
                v6.is_unspecified() || v6.is_multicast() || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        };
        if !skip {
            n.ips.insert(canonical(*ip));
        }
    }
    n
}

/// IPv4-mapped IPv6 addresses as plain IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// A host name usable as a DNS SAN: labels of letters, digits and hyphens.
pub fn valid_dns_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn now_ms() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    d.as_millis() as i64
}

fn odt(ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn serial() -> SerialNumber {
    // 128 random bits; the top bit cleared keeps the DER integer positive
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system random source");
    bytes[0] &= 0x7f;
    bytes[0] |= 0x01;
    SerialNumber::from_slice(&bytes)
}

fn gen_err(e: rcgen::Error) -> TlsError {
    TlsError::cert(format!("certificate generation failed: {e}"))
}

/// A new local CA for `hostname`, valid from a day before `now_ms` (clock skew of clients).
pub fn generate_ca(hostname: &str, now_ms: i64) -> Result<PemPair, TlsError> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(gen_err)?;
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    let host = if valid_dns_name(hostname) {
        hostname
    } else {
        "nas"
    };
    dn.push(DnType::CommonName, format!("rynas local CA ({host})"));
    dn.push(DnType::OrganizationName, "rynas");
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    p.not_before = odt(now_ms - DAY_MS);
    p.not_after = odt(now_ms + CA_DAYS * DAY_MS);
    p.serial_number = Some(serial());
    let cert = p.self_signed(&key).map_err(gen_err)?;
    Ok(PemPair {
        chain: cert.pem(),
        key: key.serialize_pem(),
    })
}

/// A leaf for `names`, issued by `ca`; the chain is the leaf followed by the CA certificate.
pub fn issue_leaf(ca: &PemPair, names: &Names, now_ms: i64) -> Result<PemPair, TlsError> {
    let ca_key =
        KeyPair::from_pem(&ca.key).map_err(|e| TlsError::key(format!("local CA key: {e}")))?;
    let ca_der = first_der(&ca.chain)?;
    let issuer = Issuer::from_ca_cert_der(&ca_der, ca_key).map_err(gen_err)?;
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(gen_err)?;
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    let cn = names
        .dns
        .iter()
        .find(|d| *d != "localhost" && !d.ends_with(".local"))
        .cloned()
        .unwrap_or_else(|| "localhost".into());
    dn.push(DnType::CommonName, cn);
    p.distinguished_name = dn;
    let mut sans = Vec::new();
    for d in &names.dns {
        sans.push(SanType::DnsName(d.clone().try_into().map_err(gen_err)?));
    }
    sans.extend(names.ips.iter().map(|i| SanType::IpAddress(*i)));
    p.subject_alt_names = sans;
    p.is_ca = IsCa::ExplicitNoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    p.use_authority_key_identifier_extension = true;
    p.not_before = odt(now_ms - DAY_MS);
    p.not_after = odt(now_ms + LEAF_DAYS * DAY_MS);
    p.serial_number = Some(serial());
    let cert = p.signed_by(&key, &issuer).map_err(gen_err)?;
    let ca_pem = pem_of(&ca_der);
    Ok(PemPair {
        chain: format!("{}{}", cert.pem(), ca_pem),
        key: key.serialize_pem(),
    })
}

fn pem_of(der: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut s = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        s.push('\n');
    }
    s.push_str("-----END CERTIFICATE-----\n");
    s
}

/// The DER certificates of a PEM text, in order.
pub fn chain_der(pem: &str) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    if pem.len() > MAX_PEM {
        return Err(TlsError::cert(format!("over {MAX_PEM} bytes")));
    }
    let certs = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::cert(format!("not a PEM certificate: {e:?}")))?;
    if certs.is_empty() {
        return Err(TlsError::cert("no \"BEGIN CERTIFICATE\" block"));
    }
    Ok(certs)
}

fn first_der(pem: &str) -> Result<CertificateDer<'static>, TlsError> {
    Ok(chain_der(pem)?.remove(0))
}

/// The private key of a PEM text (PKCS#8, SEC1 or PKCS#1; never encrypted).
pub fn key_der(pem: &str) -> Result<PrivateKeyDer<'static>, TlsError> {
    if pem.len() > MAX_PEM {
        return Err(TlsError::key(format!("over {MAX_PEM} bytes")));
    }
    if pem.contains("ENCRYPTED") {
        return Err(TlsError::key(
            "encrypted keys are not supported: remove the passphrase first",
        ));
    }
    PrivateKeyDer::from_pem_slice(pem.as_bytes())
        .map_err(|e| TlsError::key(format!("not a PEM private key: {e:?}")))
}

/// The pair as rustls serves it: the key loaded by the ring provider and checked against the
/// leaf's public key.
pub fn certified_key(pair: &PemPair) -> Result<Arc<CertifiedKey>, TlsError> {
    let chain = chain_der(&pair.chain)?;
    let key = key_der(&pair.key)?;
    let provider = rustls::crypto::ring::default_provider();
    let signer = provider
        .key_provider
        .load_private_key(key)
        .map_err(|e| TlsError::key(format!("unsupported key: {e}")))?;
    let ck = CertifiedKey::new(chain, signer);
    ck.keys_match()
        .map_err(|_| TlsError::key("the private key does not belong to the certificate"))?;
    Ok(Arc::new(ck))
}

fn hex_colon(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn parse(der: &[u8]) -> Result<X509Certificate<'_>, TlsError> {
    X509Certificate::from_der(der)
        .map(|(_, c)| c)
        .map_err(|e| TlsError::cert(format!("not an X.509 certificate: {e}")))
}

fn key_type(c: &X509Certificate<'_>) -> String {
    let spki = c.public_key();
    let bits = spki.parsed().map(|k| k.key_size()).unwrap_or(0);
    match spki.algorithm.algorithm.to_id_string().as_str() {
        "1.2.840.10045.2.1" => match bits {
            256 => "ECDSA P-256".into(),
            384 => "ECDSA P-384".into(),
            521 => "ECDSA P-521".into(),
            b => format!("ECDSA {b}"),
        },
        "1.2.840.113549.1.1.1" => format!("RSA {bits}"),
        "1.3.101.112" => "Ed25519".into(),
        other => other.to_string(),
    }
}

fn info_of(der: &[u8]) -> Result<TlsCertificate, TlsError> {
    let c = parse(der)?;
    let mut dns_names = Vec::new();
    let mut ip_addresses = Vec::new();
    if let Ok(Some(san)) = c.subject_alternative_name() {
        for n in &san.value.general_names {
            match n {
                GeneralName::DNSName(d) => dns_names.push(d.to_string()),
                GeneralName::IPAddress(b) => {
                    if let Some(ip) = ip_of(b) {
                        ip_addresses.push(ip.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    Ok(TlsCertificate {
        subject: c.subject().to_string(),
        issuer: c.issuer().to_string(),
        dns_names,
        ip_addresses,
        not_before_ms: c.validity().not_before.timestamp() * 1000,
        not_after_ms: c.validity().not_after.timestamp() * 1000,
        serial: c.raw_serial_as_string().to_ascii_uppercase(),
        fingerprint_sha256: hex_colon(&Sha256::digest(der)),
        key_type: key_type(&c),
        self_signed: c.subject().as_raw() == c.issuer().as_raw(),
        is_ca: c.is_ca(),
    })
}

fn ip_of(b: &[u8]) -> Option<IpAddr> {
    match b.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(b).ok()?)),
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(b).ok()?)),
        _ => None,
    }
}

/// Details of every certificate of a PEM chain, leaf first.
pub fn inspect(chain_pem: &str) -> Result<Vec<TlsCertificate>, TlsError> {
    chain_der(chain_pem)?.iter().map(|d| info_of(d)).collect()
}

/// Names of `want` the leaf does not cover. DNS names compare case-insensitively and a
/// `*.example.com` SAN covers one label (`nas.example.com`, not `example.com`).
pub fn missing_names(leaf: &TlsCertificate, want: &Names) -> Vec<String> {
    let dns: Vec<String> = leaf
        .dns_names
        .iter()
        .map(|d| d.to_ascii_lowercase())
        .collect();
    let covers = |name: &str| {
        dns.iter().any(|d| {
            d == name
                || d.strip_prefix("*.").is_some_and(|base| {
                    name.split_once('.')
                        .is_some_and(|(first, rest)| !first.is_empty() && rest == base)
                })
        })
    };
    let ips: BTreeSet<IpAddr> = leaf
        .ip_addresses
        .iter()
        .filter_map(|i| i.parse().ok())
        .map(canonical)
        .collect();
    want.dns
        .iter()
        .filter(|d| !covers(d))
        .cloned()
        .chain(
            want.ips
                .iter()
                .filter(|i| !ips.contains(i))
                .map(|i| i.to_string()),
        )
        .collect()
}

/// Warnings about a served chain at `now_ms`.
pub fn warnings(chain: &[TlsCertificate], missing: &[String], now_ms: i64) -> Vec<TlsWarning> {
    let mut w = Vec::new();
    let Some(leaf) = chain.first() else {
        return w;
    };
    if leaf.not_after_ms <= now_ms {
        w.push(TlsWarning::Expired);
    } else if leaf.not_after_ms - now_ms < RENEW_DAYS * DAY_MS {
        w.push(TlsWarning::ExpiresSoon);
    }
    if !missing.is_empty() {
        w.push(TlsWarning::NamesNotCovered);
    }
    if chain.len() == 1 && !leaf.self_signed {
        w.push(TlsWarning::ChainIncomplete);
    }
    w
}

/// A local leaf needs re-issuing: it misses some names or expires within [`RENEW_DAYS`].
/// Names it has beyond `want` (an address no longer used) are harmless and do not count, so
/// a flapping address does not re-issue the leaf back and forth.
pub fn needs_reissue(leaf: &TlsCertificate, want: &Names, now_ms: i64) -> bool {
    leaf.not_after_ms - now_ms < RENEW_DAYS * DAY_MS
        || leaf.not_before_ms > now_ms + DAY_MS
        || !missing_names(leaf, want).is_empty()
}

/// An imported pair, checked and normalised.
#[derive(Debug, Clone)]
pub struct Imported {
    /// The chain re-emitted as plain PEM blocks (comments and stray text dropped), and the
    /// key as given.
    pub pair: PemPair,
    pub chain: Vec<TlsCertificate>,
}

/// Checks an administrator's certificate chain and key:
/// - PEM certificates (at most [`MAX_CHAIN`]), leaf first, no CA certificate as the leaf;
/// - a private key the ring provider supports (RSA ≥ 2048, ECDSA P-256/P-384, Ed25519),
///   matching the leaf;
/// - each certificate signed by the next one (order of a full chain);
/// - valid now (not expired, not starting more than a day from now);
/// - an Extended Key Usage, when present, that allows TLS servers.
pub fn validate_import(chain_pem: &str, key_pem: &str, now_ms: i64) -> Result<Imported, TlsError> {
    let ders = chain_der(chain_pem)?;
    if ders.len() > MAX_CHAIN {
        return Err(TlsError::cert(format!(
            "more than {MAX_CHAIN} certificates"
        )));
    }
    let parsed = ders
        .iter()
        .map(|d| parse(d))
        .collect::<Result<Vec<_>, _>>()?;
    let leaf = &parsed[0];
    if leaf.is_ca() {
        // browsers refuse a CA certificate as the server's (Firefox:
        // MOZILLA_PKIX_ERROR_CA_CERT_USED_AS_END_ENTITY); `openssl req -x509` makes one unless
        // told otherwise
        return Err(TlsError::cert(
            if leaf.subject().as_raw() == leaf.issuer().as_raw() {
                "the certificate is a CA certificate (basicConstraints CA:TRUE), which browsers refuse as a server certificate: with openssl add -addext basicConstraints=critical,CA:FALSE"
            } else {
                "the first certificate is a CA certificate: put the server certificate first"
            },
        ));
    }
    if let Ok(Some(eku)) = leaf.extended_key_usage()
        && !eku.value.any
        && !eku.value.server_auth
    {
        return Err(TlsError::cert(
            "the certificate is not meant for TLS servers (no serverAuth usage)",
        ));
    }
    for (i, pair) in parsed.windows(2).enumerate() {
        let (child, parent) = (&pair[0], &pair[1]);
        if child.issuer().as_raw() != parent.subject().as_raw()
            || child.verify_signature(Some(parent.public_key())).is_err()
        {
            return Err(TlsError::cert(format!(
                "certificate {} is not signed by certificate {}: give the server certificate first, then its intermediates in order",
                i + 1,
                i + 2
            )));
        }
    }
    let nb = leaf.validity().not_before.timestamp() * 1000;
    let na = leaf.validity().not_after.timestamp() * 1000;
    if na <= now_ms {
        return Err(TlsError::cert("the certificate has expired"));
    }
    if nb > now_ms + DAY_MS {
        return Err(TlsError::cert("the certificate is not valid yet"));
    }
    let mut chain = String::new();
    for d in &ders {
        chain.push_str(&pem_of(d));
    }
    let pair = PemPair {
        chain,
        key: key_pem.trim().to_string() + "\n",
    };
    certified_key(&pair)?;
    let chain = ders
        .iter()
        .map(|d| info_of(d))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Imported { pair, chain })
}

/// Milliseconds since the Unix epoch, now.
pub fn now() -> i64 {
    now_ms()
}

#[cfg(test)]
mod tests;
