//! DNS for the own domains of web access (wave W2, `docs/research/web-access.md` §3 and
//! "W2 research"):
//!
//! - [`DnsApi`]: one provider's record API, behind a trait. rynasd writes the LAN records
//!   `*.<domain>` and `<domain>` (A, optionally AAAA) = the NAS's LAN address with the same
//!   token the ACME DNS-01 module uses, so every device on the LAN resolves app names with no
//!   router change. Clients: Cloudflare, deSEC, Hetzner (Cloud API), Porkbun, OVH, DuckDNS
//!   ([`providers`]).
//! - [`plan`]: what a sync changes (read first, never write in a plan), and the apply.
//! - [`wire`]: a minimal DNS client over UDP, to ask a given resolver (the LAN's, a public
//!   one) for A/AAAA records without the system resolver in between.
//! - [`rebind`]: the verdict that tells DNS rebind protection (the router drops public
//!   answers pointing to private addresses) from a missing record.
//!
//! Nothing here runs a request on its own: every network call is made by the caller, and
//! every client takes its base URL, so tests use fake servers (no real provider is ever
//! called by the test suite).

use std::collections::BTreeMap;
use std::fmt;

use rynas_api_types::web_access::{DnsProvider, DnsProviderField, DnsProviderInfo};

pub mod cache;
pub mod http;
pub mod plan;
pub mod providers;
pub mod rebind;
pub mod wire;

/// Record types rynas writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rtype {
    A,
    Aaaa,
}

impl Rtype {
    pub fn as_str(self) -> &'static str {
        match self {
            Rtype::A => "A",
            Rtype::Aaaa => "AAAA",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "A" => Some(Rtype::A),
            "AAAA" => Some(Rtype::Aaaa),
            _ => None,
        }
    }
}

impl fmt::Display for Rtype {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The zone at the provider that holds a name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Zone {
    /// Provider id (Cloudflare zone id, Hetzner zone id; the name for the others).
    pub id: String,
    /// Zone apex, lowercase (`example.com`).
    pub name: String,
    /// Lowest TTL the zone accepts.
    pub min_ttl: u32,
}

/// One record set at the provider.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecordSet {
    pub values: Vec<String>,
    pub ttl: u32,
}

/// A provider call that failed. Messages never carry a secret (clients redact the values of
/// their credential before building one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsError {
    /// The token was refused (401/403).
    Auth(String),
    /// No zone at this provider holds the name.
    NoZone(String),
    /// The provider answered with an error.
    Api(String),
    /// Connection, timeout, unreadable answer.
    Network(String),
    /// The provider cannot do this.
    Unsupported(String),
    /// The provider throttles (HTTP 429 after one wait): tried again with the record retry.
    RateLimited(String),
}

impl fmt::Display for DnsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DnsError::Auth(m) => write!(f, "the provider refused the credential: {m}"),
            DnsError::NoZone(m) => write!(f, "no zone at the provider holds {m}"),
            DnsError::Api(m) => write!(f, "provider error: {m}"),
            DnsError::Network(m) => write!(f, "provider unreachable: {m}"),
            DnsError::Unsupported(m) => write!(f, "not supported: {m}"),
            DnsError::RateLimited(m) => write!(f, "the provider is rate limiting: {m}"),
        }
    }
}

impl std::error::Error for DnsError {}

/// The values of a credential, by field name.
pub type Creds = BTreeMap<String, String>;

/// One provider's record API. Names are fully qualified, lowercase, without the final dot.
/// Blocking: call it from a blocking thread.
pub trait DnsApi: Send + Sync {
    /// The zone that holds `fqdn` (the longest matching apex).
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError>;
    /// The record set `fqdn`/`rtype`, `None` when there is none.
    fn get(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<Option<RecordSet>, DnsError>;
    /// Creates or replaces the record set with exactly `values`.
    fn set(
        &self,
        zone: &Zone,
        fqdn: &str,
        rtype: Rtype,
        values: &[String],
        ttl: u32,
    ) -> Result<(), DnsError>;
    /// Removes the record set (no error when it does not exist).
    fn delete(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<(), DnsError>;
    /// Whether [`DnsApi::get`] works (DuckDNS has no read API: the address is written again
    /// on every sync).
    fn readable(&self) -> bool {
        true
    }
}

/// `fqdn` relative to `zone`: `*.nas` for `*.nas.example.com` in `example.com`, `""` for the
/// apex.
pub fn relative(fqdn: &str, zone: &str) -> String {
    if fqdn == zone {
        return String::new();
    }
    fqdn.strip_suffix(zone)
        .and_then(|s| s.strip_suffix('.'))
        .unwrap_or(fqdn)
        .to_string()
}

/// Candidate zone apexes of `fqdn`, longest first, at least two labels; a leading `*` label
/// is skipped.
pub fn zone_candidates(fqdn: &str) -> Vec<String> {
    let name = fqdn.strip_prefix("*.").unwrap_or(fqdn);
    let labels: Vec<&str> = name.split('.').filter(|l| !l.is_empty()).collect();
    (0..labels.len().saturating_sub(1))
        .map(|i| labels[i..].join("."))
        .collect()
}

/// Replaces every credential value (4 characters or more) in `text` by `***`.
pub fn redact(text: &str, creds: &Creds) -> String {
    let mut out = text.to_string();
    for v in creds.values().filter(|v| v.len() >= 4) {
        out = out.replace(v.as_str(), "***");
    }
    out
}

/// Default TTL of the LAN records (raised to the zone's minimum).
pub const DEFAULT_TTL: u32 = 300;

/// One credential field: name, secret, optional, and whether the Caddy module replaces
/// placeholders in it (so rynas renders `{file./path}` instead of the value).
#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    pub name: &'static str,
    pub secret: bool,
    pub optional: bool,
    /// The caddy-dns module calls `caddy.NewReplacer().ReplaceAll` on it in `Provision`.
    pub placeholder: bool,
}

macro_rules! f {
    ($name:expr, $secret:expr, $optional:expr, $placeholder:expr) => {
        FieldSpec {
            name: $name,
            secret: $secret,
            optional: $optional,
            placeholder: $placeholder,
        }
    };
}

/// The credential fields of a provider, named like the caddy-dns module's JSON fields.
pub fn fields(p: DnsProvider) -> &'static [FieldSpec] {
    match p {
        DnsProvider::Cloudflare => &[f!("api_token", true, false, true)],
        DnsProvider::Desec => &[f!("token", true, false, true)],
        DnsProvider::Hetzner => &[f!("api_token", true, false, true)],
        DnsProvider::Porkbun => &[
            f!("api_key", true, false, true),
            f!("api_secret_key", true, false, true),
        ],
        DnsProvider::Ovh => &[
            f!("endpoint", false, true, true),
            f!("application_key", true, false, true),
            f!("application_secret", true, false, true),
            f!("consumer_key", true, false, true),
        ],
        DnsProvider::Duckdns => &[f!("api_token", true, false, true)],
        DnsProvider::Gandi => &[f!("bearer_token", true, false, true)],
        DnsProvider::Route53 => &[
            f!("access_key_id", true, false, true),
            f!("secret_access_key", true, false, true),
            f!("region", false, true, true),
        ],
        // not checked in the module source: written as values, not placeholders
        DnsProvider::Digitalocean => &[f!("auth_token", true, false, false)],
        DnsProvider::Rfc2136 => &[
            f!("server", false, false, false),
            f!("key_name", false, false, false),
            f!("key_alg", false, true, false),
            f!("key", true, false, false),
        ],
        // the module replaces placeholders only in the password
        DnsProvider::Acmedns => &[
            f!("server_url", false, false, false),
            f!("username", false, false, false),
            f!("subdomain", false, false, false),
            f!("password", true, false, true),
        ],
    }
}

/// What rynas offers for each provider.
pub fn info(p: DnsProvider) -> DnsProviderInfo {
    let (records, wildcard_records, scoped, min_ttl, hint) = match p {
        DnsProvider::Cloudflare => (
            true,
            true,
            true,
            60,
            "Create an API token (My Profile → API Tokens) with the permission Zone → DNS → Edit, limited to the one zone (Zone Resources: Include → Specific zone). Not the Global API Key.",
        ),
        DnsProvider::Desec => (
            true,
            true,
            true,
            3600,
            "Create a token with a policy that allows writes only to this domain (and, if you like, only to the names *.<domain>, <domain> and _acme-challenge.<domain>), optionally limited to your public address.",
        ),
        DnsProvider::Hetzner => (
            true,
            true,
            false,
            60,
            "Use a Hetzner Console (Cloud API) token with Read & Write permission. Tokens of the old dns.hetzner.com API no longer work. Tokens cover a whole project: keep the zone in a project of its own.",
        ),
        DnsProvider::Porkbun => (
            true,
            true,
            true,
            600,
            "Create an API key, turn on API access for this domain only (Domain Management → Details → API Access), and limit the key to DNS if offered.",
        ),
        DnsProvider::Ovh => (
            true,
            true,
            true,
            60,
            "Create an application and a consumer key whose rights allow only GET/POST/PUT/DELETE on /domain/zone/<zone>/* (and GET /domain/zone).",
        ),
        DnsProvider::Duckdns => (
            true,
            false,
            false,
            60,
            "The account token covers all your DuckDNS names. The domain is <name>.duckdns.org; DuckDNS answers every name below it with the same address. DuckDNS cannot be read: rynas sets its one record to the NAS's LAN address on every sync, replacing any other address it holds (a public one kept by a DDNS updater too).",
        ),
        DnsProvider::Gandi => (
            false,
            true,
            true,
            300,
            "Create a personal access token limited to this domain with the permission 'Manage domain technical configurations'.",
        ),
        DnsProvider::Route53 => (
            false,
            true,
            true,
            60,
            "Create an IAM user whose policy allows route53:ChangeResourceRecordSets and ListResourceRecordSets on the one hosted zone, plus route53:ListHostedZonesByName and GetChange.",
        ),
        DnsProvider::Digitalocean => (
            false,
            true,
            false,
            30,
            "Tokens cannot be limited to one domain: prefer CNAME delegation of _acme-challenge to a zone used only for that.",
        ),
        DnsProvider::Rfc2136 => (
            false,
            true,
            true,
            0,
            "Use a TSIG key whose update-policy allows only TXT records of _acme-challenge.<domain>.",
        ),
        DnsProvider::Acmedns => (
            false,
            false,
            true,
            0,
            "Register an account on the acme-dns server and add the CNAME _acme-challenge.<domain> → <fulldomain> once. The account can only change that one TXT record.",
        ),
    };
    DnsProviderInfo {
        provider: p,
        fields: fields(p)
            .iter()
            .map(|s| DnsProviderField {
                name: s.name.into(),
                secret: s.secret,
                optional: s.optional,
            })
            .collect(),
        records,
        wildcard_records,
        scoped_tokens: scoped,
        min_ttl,
        token_hint: hint.into(),
    }
}

/// Checks that `creds` has every required field of `p` and no unknown one; returns the
/// first problem as `(field, message)`.
pub fn check_creds(p: DnsProvider, creds: &Creds) -> Result<(), (String, String)> {
    let spec = fields(p);
    for k in creds.keys() {
        if !spec.iter().any(|s| s.name == k) {
            return Err((k.clone(), format!("{} has no field {k}", p.as_str())));
        }
    }
    for s in spec.iter().filter(|s| !s.optional) {
        if creds.get(s.name).is_none_or(|v| v.trim().is_empty()) {
            return Err((s.name.into(), format!("{} is required", s.name)));
        }
    }
    for (k, v) in creds {
        if v.len() > 4096 || v.chars().any(|c| c.is_control()) {
            return Err((k.clone(), "a single line of at most 4096 characters".into()));
        }
    }
    Ok(())
}

/// Base URLs of the providers (tests point them at fake servers).
#[derive(Debug, Clone, Default)]
pub struct Endpoints {
    /// Provider → base URL replacing the public one.
    pub overrides: BTreeMap<DnsProvider, String>,
}

impl Endpoints {
    fn base(&self, p: DnsProvider, default: &str) -> String {
        self.overrides
            .get(&p)
            .cloned()
            .unwrap_or_else(|| default.to_string())
            .trim_end_matches('/')
            .to_string()
    }
}

/// The record client of `p` with `creds`, `None` when rynas has none for that provider.
pub fn client(
    p: DnsProvider,
    creds: &Creds,
    endpoints: &Endpoints,
) -> Result<Option<Box<dyn DnsApi>>, DnsError> {
    let http = http::Http::new(creds.clone())?;
    let get = |k: &str| creds.get(k).cloned().unwrap_or_default();
    Ok(Some(match p {
        DnsProvider::Cloudflare => Box::new(providers::cloudflare::Cloudflare {
            http,
            base: endpoints.base(p, providers::cloudflare::BASE),
            token: get("api_token"),
        }),
        DnsProvider::Desec => Box::new(providers::desec::Desec {
            http,
            base: endpoints.base(p, providers::desec::BASE),
            token: get("token"),
        }),
        DnsProvider::Hetzner => Box::new(providers::hetzner::Hetzner {
            http,
            base: endpoints.base(p, providers::hetzner::BASE),
            token: get("api_token"),
        }),
        DnsProvider::Porkbun => Box::new(providers::porkbun::Porkbun {
            http,
            base: endpoints.base(p, providers::porkbun::BASE),
            key: get("api_key"),
            secret: get("api_secret_key"),
        }),
        DnsProvider::Ovh => {
            let ep = creds
                .get("endpoint")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "ovh-eu".into());
            let default = providers::ovh::endpoint_url(&ep)
                .ok_or_else(|| DnsError::Unsupported(format!("OVH endpoint {ep}")))?;
            Box::new(providers::ovh::Ovh::new(
                http,
                endpoints.base(p, default),
                get("application_key"),
                get("application_secret"),
                get("consumer_key"),
            ))
        }
        DnsProvider::Duckdns => Box::new(providers::duckdns::Duckdns {
            http,
            base: endpoints.base(p, providers::duckdns::BASE),
            token: get("api_token"),
        }),
        _ => return Ok(None),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_candidates() {
        assert_eq!(relative("*.nas.example.com", "example.com"), "*.nas");
        assert_eq!(relative("example.com", "example.com"), "");
        assert_eq!(
            zone_candidates("*.nas.example.co.uk"),
            vec!["nas.example.co.uk", "example.co.uk", "co.uk"]
        );
        assert_eq!(zone_candidates("example.com"), vec!["example.com"]);
    }

    #[test]
    fn every_provider_has_info_and_fields() {
        for p in DnsProvider::ALL {
            let i = info(p);
            assert!(!i.fields.is_empty(), "{p:?}");
            assert!(!i.token_hint.is_empty());
            let has_client = client(p, &Creds::new(), &Endpoints::default())
                .unwrap()
                .is_some();
            assert_eq!(has_client, i.records, "{p:?}");
        }
    }

    #[test]
    fn credential_checks() {
        let mut c = Creds::new();
        assert_eq!(
            check_creds(DnsProvider::Porkbun, &c).unwrap_err().0,
            "api_key"
        );
        c.insert("api_key".into(), "pk1_x".into());
        c.insert("api_secret_key".into(), "sk1_x".into());
        assert!(check_creds(DnsProvider::Porkbun, &c).is_ok());
        c.insert("token".into(), "x".into());
        assert_eq!(
            check_creds(DnsProvider::Porkbun, &c).unwrap_err().0,
            "token"
        );
        let mut o = Creds::new();
        for k in ["application_key", "application_secret", "consumer_key"] {
            o.insert(k.into(), "v".into());
        }
        assert!(
            check_creds(DnsProvider::Ovh, &o).is_ok(),
            "endpoint is optional"
        );
        o.insert("consumer_key".into(), "a\nb".into());
        assert!(check_creds(DnsProvider::Ovh, &o).is_err());
    }

    #[test]
    fn redaction() {
        let mut c = Creds::new();
        c.insert("api_token".into(), "s3cr3t-token".into());
        c.insert("region".into(), "eu".into());
        assert_eq!(
            redact("Bearer s3cr3t-token refused in eu", &c),
            "Bearer *** refused in eu"
        );
    }
}
