//! Record clients, one per provider. Each follows the provider's public API documentation
//! (URLs in each module) and is tested against a fake server (`tests/providers.rs`).

pub mod cloudflare;
pub mod desec;
pub mod duckdns;
pub mod hetzner;
pub mod ovh;
pub mod porkbun;

use crate::DnsError;
use crate::http::{Http, Resp};

/// The error of a failed answer: 401/403 as [`DnsError::Auth`], the rest as
/// [`DnsError::Api`], with `msg` (the provider's own message) when it has one.
pub(crate) fn fail(http: &Http, r: &Resp, msg: Option<String>) -> DnsError {
    let m = http.clean(&msg.unwrap_or_else(|| format!("HTTP {}: {}", r.status, r.body.trim())));
    if r.status == 401 || r.status == 403 {
        DnsError::Auth(m)
    } else {
        DnsError::Api(m)
    }
}

/// Sorted copy (record sets compare as sets).
pub(crate) fn sorted(v: &[String]) -> Vec<String> {
    let mut v = v.to_vec();
    v.sort();
    v
}
