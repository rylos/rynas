//! Hetzner DNS through the Hetzner Cloud API (`https://api.hetzner.cloud/v1`, `Authorization:
//! Bearer <token>`). The old `dns.hetzner.com` API was shut down in May 2026 and its tokens do
//! not work here (https://docs.hetzner.com/networking/dns/faq/beta). Zones by
//! `GET /zones?name=`; a record set by `GET /zones/{zone}/rrsets/{name}/{type}` (`@` for the
//! apex, 404 when absent); `POST /zones/{zone}/rrsets` `{name, type, ttl, records: [{value}]}`
//! to create; `POST .../rrsets/{name}/{type}/actions/set_records` and `.../change_ttl` to
//! change; `DELETE .../rrsets/{name}/{type}`. (Shapes taken from
//! https://docs.hetzner.cloud/reference/cloud#zones and a published client,
//! https://docs.rs/dns-update/0.5.3/src/dns_update/providers/hetzner.rs.html.)

use serde_json::{Value, json};

use super::{fail, sorted};
use crate::http::{Http, enc};
use crate::{DnsApi, DnsError, RecordSet, Rtype, Zone, relative, zone_candidates};

pub const BASE: &str = "https://api.hetzner.cloud/v1";

pub struct Hetzner {
    pub http: Http,
    pub base: String,
    pub token: String,
}

fn message(v: &Value) -> Option<String> {
    v["error"]["message"]
        .as_str()
        .map(|m| format!("{m} ({})", v["error"]["code"].as_str().unwrap_or("?")))
}

fn rel(fqdn: &str, zone: &Zone) -> String {
    let r = relative(fqdn, &zone.name);
    if r.is_empty() { "@".into() } else { r }
}

impl Hetzner {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), DnsError> {
        let r = self.http.send(
            method,
            &format!("{}{path}", self.base),
            &[("Authorization", format!("Bearer {}", self.token))],
            body,
        )?;
        let v = r.json().unwrap_or(Value::Null);
        if r.status == 404 && (method == "GET" || method == "DELETE") {
            return Ok((404, Value::Null));
        }
        if !r.ok() {
            return Err(fail(&self.http, &r, message(&v)));
        }
        Ok((r.status, v))
    }

    fn path(zone: &Zone, fqdn: &str, rtype: Rtype) -> String {
        format!(
            "/zones/{}/rrsets/{}/{}",
            enc(&zone.id),
            enc(&rel(fqdn, zone)),
            rtype
        )
    }
}

impl DnsApi for Hetzner {
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
        for cand in zone_candidates(fqdn) {
            let (_, v) = self.call("GET", &format!("/zones?name={}", enc(&cand)), None)?;
            if let Some(z) = v["zones"].as_array().and_then(|a| a.first()) {
                let id = match &z["id"] {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                return Ok(Zone {
                    id,
                    name: cand,
                    min_ttl: 60,
                });
            }
        }
        Err(DnsError::NoZone(fqdn.into()))
    }

    fn get(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<Option<RecordSet>, DnsError> {
        let (status, v) = self.call("GET", &Self::path(zone, fqdn, rtype), None)?;
        if status == 404 {
            return Ok(None);
        }
        let rr = &v["rrset"];
        let values: Vec<String> = rr["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["value"].as_str().map(String::from))
            .collect();
        if values.is_empty() {
            return Ok(None);
        }
        Ok(Some(RecordSet {
            values,
            ttl: rr["ttl"].as_u64().unwrap_or(0) as u32,
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
        let records: Vec<Value> = values.iter().map(|v| json!({"value": v})).collect();
        match self.get(zone, fqdn, rtype)? {
            None => {
                self.call(
                    "POST",
                    &format!("/zones/{}/rrsets", enc(&zone.id)),
                    Some(&json!({"name": rel(fqdn, zone), "type": rtype.as_str(),
                                 "ttl": ttl, "records": records})),
                )?;
            }
            Some(cur) => {
                let p = Self::path(zone, fqdn, rtype);
                if sorted(&cur.values) != sorted(values) {
                    self.call(
                        "POST",
                        &format!("{p}/actions/set_records"),
                        Some(&json!({"records": records})),
                    )?;
                }
                if cur.ttl != ttl {
                    self.call(
                        "POST",
                        &format!("{p}/actions/change_ttl"),
                        Some(&json!({"ttl": ttl})),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn delete(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<(), DnsError> {
        self.call("DELETE", &Self::path(zone, fqdn, rtype), None)?;
        Ok(())
    }
}
