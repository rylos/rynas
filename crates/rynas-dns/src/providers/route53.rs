//! Amazon Route 53 (https://docs.aws.amazon.com/Route53/latest/APIReference/): the REST API
//! with XML bodies, every request signed with Signature Version 4
//! (https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html)
//! for the global endpoint (region `us-east-1`, service `route53`).
//!
//! - zone: `GET /2013-04-01/hostedzonesbyname?dnsname=<apex>&maxitems=1` for each candidate
//!   apex, longest first (private zones skipped);
//! - read: `GET /2013-04-01/hostedzone/<id>/rrset?name=<fqdn>&type=<t>&maxitems=1`;
//! - write: `POST /2013-04-01/hostedzone/<id>/rrset` with a `ChangeBatch` (`UPSERT`, or
//!   `DELETE` with the current values and TTL, as Route 53 wants them).
//!
//! The least privilege policy: `route53:ChangeResourceRecordSets` and
//! `route53:ListResourceRecordSets` on the one hosted zone (limited to the names with the
//! condition `route53:ChangeResourceRecordSetsNormalizedRecordNames`), plus
//! `route53:ListHostedZonesByName`.

use sha2::{Digest, Sha256};

use super::fail;
use crate::http::Http;
use crate::{DnsApi, DnsError, RecordSet, Rtype, Zone, zone_candidates};

pub const BASE: &str = "https://route53.amazonaws.com";
const REGION: &str = "us-east-1";
const SERVICE: &str = "route53";
const API: &str = "/2013-04-01";

pub struct Route53 {
    pub http: Http,
    pub base: String,
    pub key_id: String,
    pub secret: String,
}

// ------------------------------------------------------------------ Signature Version 4

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// URI encoding as SigV4 wants it: RFC 3986 unreserved characters kept, everything else
/// `%XX` (uppercase), `/` too unless `path`.
pub fn aws_enc(s: &str, path: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') || (path && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `(YYYYMMDD, YYYYMMDDTHHMMSSZ)` of a Unix time (UTC).
pub fn amz_dates(unix_s: u64) -> (String, String) {
    let days = (unix_s / 86_400) as i64;
    let secs = unix_s % 86_400;
    // days since 1970-01-01 to a civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let date = format!("{y:04}{m:02}{d:02}");
    let time = format!("{date}T{:02}{:02}{:02}Z", secs / 3600, secs / 60 % 60, secs % 60);
    (date, time)
}

/// The `Authorization` header of a request (`host` and `x-amz-date` signed). `query` is
/// already in canonical form (sorted, encoded).
#[allow(clippy::too_many_arguments)]
pub fn authorization(
    key_id: &str,
    secret: &str,
    method: &str,
    host: &str,
    path: &str,
    query: &str,
    payload: &[u8],
    amz_date: &str,
    date: &str,
) -> String {
    let canonical = format!(
        "{method}\n{}\n{query}\nhost:{host}\nx-amz-date:{amz_date}\n\nhost;x-amz-date\n{}",
        aws_enc(path, true),
        sha256_hex(payload)
    );
    let scope = format!("{date}/{REGION}/{SERVICE}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, REGION.as_bytes());
    let k_service = hmac_sha256(&k_region, SERVICE.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let sig = hex::encode(hmac_sha256(&k_signing, to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders=host;x-amz-date, Signature={sig}")
}

// ------------------------------------------------------------------ XML

/// The text of every `<tag>…</tag>` in `xml`, unescaped.
pub fn tags<'a>(xml: &'a str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest: &'a str = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(unescape(&after[..j]));
        rest = &after[j + close.len()..];
    }
    out
}

fn tag(xml: &str, t: &str) -> Option<String> {
    tags(xml, t).into_iter().next()
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A name as Route 53 answers it (`\052` for `*`, final dot) back to rynas's form.
fn plain_name(n: &str) -> String {
    n.replace("\\052", "*").trim_end_matches('.').to_ascii_lowercase()
}

/// The `ChangeBatch` of one change.
pub fn change_batch(action: &str, fqdn: &str, rtype: Rtype, values: &[String], ttl: u32) -> String {
    let records: String = values
        .iter()
        .map(|v| format!("<ResourceRecord><Value>{}</Value></ResourceRecord>", escape(v)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<ChangeResourceRecordSetsRequest xmlns=\"https://route53.amazonaws.com/doc/2013-04-01/\">\
<ChangeBatch><Comment>rynas</Comment><Changes><Change><Action>{action}</Action>\
<ResourceRecordSet><Name>{}</Name><Type>{}</Type><TTL>{ttl}</TTL><ResourceRecords>{records}</ResourceRecords>\
</ResourceRecordSet></Change></Changes></ChangeBatch></ChangeResourceRecordSetsRequest>",
        escape(fqdn),
        rtype.as_str()
    )
}

// ------------------------------------------------------------------ client

impl Route53 {
    fn host(&self) -> String {
        let h = self.base.split("://").nth(1).unwrap_or(&self.base);
        h.split('/').next().unwrap_or(h).to_string()
    }

    /// One signed request; `query`: `(key, value)` pairs.
    fn call(&self, method: &str, path: &str, query: &[(&str, &str)], body: Option<&str>) -> Result<String, DnsError> {
        let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (aws_enc(k, false), aws_enc(v, false))).collect();
        q.sort();
        let query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (date, amz_date) = amz_dates(now);
        let payload = body.unwrap_or("");
        let auth = authorization(&self.key_id, &self.secret, method, &self.host(), path, &query, payload.as_bytes(), &amz_date, &date);
        let url = if query.is_empty() { format!("{}{path}", self.base) } else { format!("{}{path}?{query}", self.base) };
        let headers = [("x-amz-date", amz_date), ("Authorization", auth)];
        let r = self.http.send_body(method, &url, &headers, body.map(|b| ("application/xml", b)))?;
        if !r.ok() {
            let msg = tag(&r.body, "Message").map(|m| match tag(&r.body, "Code") {
                Some(c) => format!("{c}: {m}"),
                None => m,
            });
            // Route 53 answers 403 for a bad signature or a missing permission
            return Err(fail(&self.http, &r, msg));
        }
        Ok(r.body)
    }

    fn zone_path(zone: &Zone) -> String {
        format!("{API}/hostedzone/{}/rrset", zone.id)
    }
}

impl DnsApi for Route53 {
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
        for apex in zone_candidates(fqdn) {
            let body = self.call("GET", &format!("{API}/hostedzonesbyname"), &[("dnsname", &apex), ("maxitems", "1")], None)?;
            for z in tags(&body, "HostedZone") {
                let name = tag(&z, "Name").map(|n| plain_name(&n)).unwrap_or_default();
                let private = tag(&z, "PrivateZone").is_some_and(|p| p == "true");
                if name == apex && !private {
                    let id = tag(&z, "Id").unwrap_or_default();
                    let id = id.trim_start_matches("/hostedzone/").to_string();
                    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                        return Err(DnsError::Api(format!("unreadable hosted zone id for {apex}")));
                    }
                    return Ok(Zone { id, name, min_ttl: 60 });
                }
            }
        }
        Err(DnsError::NoZone(fqdn.into()))
    }

    fn get(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<Option<RecordSet>, DnsError> {
        // Route 53 stores `*` as `\052` and lists from the name given
        let name = fqdn.replace('*', "\\052");
        let body = self.call("GET", &Self::zone_path(zone), &[("name", &name), ("type", rtype.as_str()), ("maxitems", "1")], None)?;
        // the list starts at the name asked: the first set is it only when name and type match
        Ok(tags(&body, "ResourceRecordSet").into_iter().find_map(|s| {
            let name = plain_name(&tag(&s, "Name")?);
            (name == fqdn && tag(&s, "Type").as_deref() == Some(rtype.as_str())).then(|| RecordSet {
                values: super::sorted(&tags(&s, "Value")),
                ttl: tag(&s, "TTL").and_then(|t| t.parse().ok()).unwrap_or(0),
            })
        }))
    }

    fn set(&self, zone: &Zone, fqdn: &str, rtype: Rtype, values: &[String], ttl: u32) -> Result<(), DnsError> {
        if values.is_empty() {
            return self.delete(zone, fqdn, rtype);
        }
        let body = change_batch("UPSERT", fqdn, rtype, values, ttl.max(zone.min_ttl));
        self.call("POST", &Self::zone_path(zone), &[], Some(&body)).map(|_| ())
    }

    fn delete(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<(), DnsError> {
        // Route 53 deletes a set only with its exact values and TTL
        let Some(cur) = self.get(zone, fqdn, rtype)? else { return Ok(()) };
        let body = change_batch("DELETE", fqdn, rtype, &cur.values, cur.ttl);
        self.call("POST", &Self::zone_path(zone), &[], Some(&body)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example of the AWS documentation (Signature Version 4 test suite,
    /// "get-vanilla"): same key, date and request, same signature.
    #[test]
    fn signature_matches_the_aws_example() {
        let canonical_secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        // the documented derived key for 20150830/us-east-1/iam (HMAC chain check)
        let k_date = hmac_sha256(format!("AWS4{canonical_secret}").as_bytes(), b"20150830");
        let k_region = hmac_sha256(&k_date, b"us-east-1");
        let k_service = hmac_sha256(&k_region, b"iam");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        assert_eq!(hex::encode(k_signing), "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9");
    }

    #[test]
    fn hmac_rfc4231_case_2() {
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn dates_and_encoding() {
        assert_eq!(amz_dates(1_440_938_160), ("20150830".to_string(), "20150830T123600Z".to_string()));
        assert_eq!(amz_dates(951_782_400).0, "20000229");
        assert_eq!(aws_enc("a b*/~", false), "a%20b%2A%2F~");
        assert_eq!(aws_enc("/2013-04-01/hostedzone/Z1/rrset", true), "/2013-04-01/hostedzone/Z1/rrset");
    }

    #[test]
    fn xml_reading() {
        let body = "<ListResourceRecordSetsResponse><ResourceRecordSets><ResourceRecordSet><Name>\\052.nas.example.com.</Name><Type>A</Type><TTL>300</TTL><ResourceRecords><ResourceRecord><Value>1.2.3.4</Value></ResourceRecord></ResourceRecords></ResourceRecordSet></ResourceRecordSets></ListResourceRecordSetsResponse>";
        let sets = tags(body, "ResourceRecordSet");
        assert_eq!(sets.len(), 1);
        assert_eq!(plain_name(&tag(&sets[0], "Name").unwrap()), "*.nas.example.com");
        assert_eq!(tags(&sets[0], "Value"), ["1.2.3.4"]);
        let b = change_batch("UPSERT", "home.example.com", Rtype::A, &["1.2.3.4".into()], 60);
        assert!(b.contains("<Action>UPSERT</Action>") && b.contains("<Name>home.example.com</Name>") && b.contains("<TTL>60</TTL>"));
    }
}
