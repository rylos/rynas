//! Cloudflare API v4 (https://developers.cloudflare.com/api/resources/dns/subresources/records/):
//! `Authorization: Bearer <token>`; zones by `GET /zones?name=`; records by
//! `GET /zones/{zone}/dns_records?type=&name=`, `POST` to create, `PUT /dns_records/{id}` to
//! replace, `DELETE /dns_records/{id}`. TTL 60-86400 (1 = automatic); `proxied: false` (the
//! LAN address must be answered as is). Answers are `{success, errors: [{code, message}],
//! result}`.

use serde_json::{Value, json};

use super::fail;
use crate::http::{Http, Resp, enc};
use crate::{DnsApi, DnsError, RecordSet, Rtype, Zone, zone_candidates};

pub const BASE: &str = "https://api.cloudflare.com/client/v4";

pub struct Cloudflare {
    pub http: Http,
    pub base: String,
    pub token: String,
}

fn message(v: &Value) -> Option<String> {
    let errs = v["errors"].as_array()?;
    let m: Vec<String> = errs
        .iter()
        .map(|e| format!("{} ({})", e["message"].as_str().unwrap_or("?"), e["code"]))
        .collect();
    (!m.is_empty()).then(|| m.join("; "))
}

impl Cloudflare {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, DnsError> {
        let r: Resp = self.http.send(
            method,
            &format!("{}{path}", self.base),
            &[("Authorization", format!("Bearer {}", self.token))],
            body,
        )?;
        let v = r.json().unwrap_or(Value::Null);
        if !r.ok() || v["success"] == false {
            return Err(fail(&self.http, &r, message(&v)));
        }
        Ok(v)
    }

    /// `(id, content, ttl)` of the records `fqdn`/`rtype`.
    fn records(
        &self,
        zone: &Zone,
        fqdn: &str,
        rtype: Rtype,
    ) -> Result<Vec<(String, String, u32)>, DnsError> {
        let v = self.call(
            "GET",
            &format!(
                "/zones/{}/dns_records?type={}&name={}&per_page=100",
                enc(&zone.id),
                rtype,
                enc(fqdn)
            ),
            None,
        )?;
        Ok(v["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some((
                    r["id"].as_str()?.to_string(),
                    r["content"].as_str()?.to_string(),
                    r["ttl"].as_u64().unwrap_or(1) as u32,
                ))
            })
            .collect())
    }
}

impl DnsApi for Cloudflare {
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
        for cand in zone_candidates(fqdn) {
            let v = self.call("GET", &format!("/zones?name={}", enc(&cand)), None)?;
            if let Some(z) = v["result"].as_array().and_then(|a| a.first())
                && let Some(id) = z["id"].as_str()
            {
                return Ok(Zone {
                    id: id.into(),
                    name: cand,
                    min_ttl: 60,
                });
            }
        }
        Err(DnsError::NoZone(fqdn.into()))
    }

    fn get(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<Option<RecordSet>, DnsError> {
        let recs = self.records(zone, fqdn, rtype)?;
        if recs.is_empty() {
            return Ok(None);
        }
        Ok(Some(RecordSet {
            ttl: recs[0].2,
            values: recs.into_iter().map(|r| r.1).collect(),
        }))
    }

    fn set(
        &self,
        zone: &Zone,
        fqdn: &str,
        rtype: Rtype,
        values: &[String],
        ttl: u32,
    ) -> Result<(), DnsError> {
        let existing = self.records(zone, fqdn, rtype)?;
        let body = |content: &str| {
            json!({"type": rtype.as_str(), "name": fqdn, "content": content, "ttl": ttl,
                   "proxied": false, "comment": "managed by rynas (LAN address of the NAS)"})
        };
        for (i, v) in values.iter().enumerate() {
            match existing.get(i) {
                Some((id, content, t)) if content == v && *t == ttl => {
                    let _ = id;
                }
                Some((id, _, _)) => {
                    self.call(
                        "PUT",
                        &format!("/zones/{}/dns_records/{}", enc(&zone.id), enc(id)),
                        Some(&body(v)),
                    )?;
                }
                None => {
                    self.call(
                        "POST",
                        &format!("/zones/{}/dns_records", enc(&zone.id)),
                        Some(&body(v)),
                    )?;
                }
            }
        }
        for (id, _, _) in existing.iter().skip(values.len()) {
            self.call(
                "DELETE",
                &format!("/zones/{}/dns_records/{}", enc(&zone.id), enc(id)),
                None,
            )?;
        }
        Ok(())
    }

    fn delete(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<(), DnsError> {
        for (id, _, _) in self.records(zone, fqdn, rtype)? {
            self.call(
                "DELETE",
                &format!("/zones/{}/dns_records/{}", enc(&zone.id), enc(&id)),
                None,
            )?;
        }
        Ok(())
    }
}
