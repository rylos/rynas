//! HTTPS certificate of the web interface (wave W0 of `docs/research/web-access.md`): the
//! certificate `rynas-web` serves on its own port (`:8443`, the emergency door that never goes
//! through the reverse proxy), where it comes from, and the import of a user certificate.
//!
//! `GET /api/v1/system/tls` → [`TlsStatus`]; `POST /api/v1/system/tls/import`
//! ([`TlsImportRequest`]) and `POST /api/v1/system/tls/local` → [`TlsStatus`];
//! `GET /api/v1/system/tls/ca` → the local CA certificate (PEM download).

use serde::{Deserialize, Serialize};

/// Where the active certificate comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TlsSource {
    /// Issued by rynas's own local CA: names follow the host name and addresses, renewed
    /// automatically before it expires.
    #[default]
    Local,
    /// Imported by an administrator: served as is, never renewed by rynas.
    Imported,
}

/// Details of one X.509 certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TlsCertificate {
    /// Subject as an RFC 4514 string, e.g. "CN=nas".
    pub subject: String,
    pub issuer: String,
    /// DNS names of the Subject Alternative Name extension.
    #[serde(default)]
    pub dns_names: Vec<String>,
    /// IP addresses of the Subject Alternative Name extension.
    #[serde(default)]
    pub ip_addresses: Vec<String>,
    /// Milliseconds since the Unix epoch.
    pub not_before_ms: i64,
    pub not_after_ms: i64,
    /// Serial number, hexadecimal with colons.
    pub serial: String,
    /// SHA-256 of the DER encoding, uppercase hexadecimal with colons (what browsers show).
    pub fingerprint_sha256: String,
    /// "ECDSA P-256", "RSA 2048", "Ed25519"…
    pub key_type: String,
    /// Subject and issuer are the same (a root or a self-signed leaf).
    pub self_signed: bool,
    /// Basic constraints say CA.
    pub is_ca: bool,
}

/// Something about the active certificate the UI should point out (codes, translated by the
/// UI as `tls.warn.<code>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsWarning {
    /// Expires within 30 days.
    ExpiresSoon,
    /// Already expired.
    Expired,
    /// Some names or addresses of this NAS are not in the certificate (`missing_names`).
    NamesNotCovered,
    /// An imported leaf from a CA came without its intermediate certificates: many clients
    /// will not build the chain.
    ChainIncomplete,
}

/// `GET /api/v1/system/tls`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TlsStatus {
    pub source: TlsSource,
    /// The leaf served by `rynas-web`.
    pub certificate: TlsCertificate,
    /// The rest of the served chain (intermediates, the local CA), leaf excluded.
    #[serde(default)]
    pub chain: Vec<TlsCertificate>,
    /// rynas's local CA ("Trust this NAS"); also present while an imported certificate is
    /// active, so it can be switched back.
    #[serde(default)]
    pub local_ca: Option<TlsCertificate>,
    #[serde(default)]
    pub warnings: Vec<TlsWarning>,
    /// Host names and addresses of this NAS the leaf does not cover.
    #[serde(default)]
    pub missing_names: Vec<String>,
}

/// `POST /api/v1/system/tls/import`: PEM texts. `certificate` holds the leaf first, then its
/// intermediates (a "full chain"); `private_key` the leaf's unencrypted key (PKCS#8, SEC1 or
/// PKCS#1). Errors: `400 invalid_value` with `field` = `certificate` or `private_key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TlsImportRequest {
    pub certificate: String,
    pub private_key: String,
}
