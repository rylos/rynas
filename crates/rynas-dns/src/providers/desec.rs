//! deSEC API v1 (https://desec.readthedocs.io/en/latest/dns/rrsets.html):
//! `Authorization: Token <token>`; the domain that holds a name by
//! `GET /domains/?owns_qname=<name>` (its `minimum_ttl` too); a record set by
//! `GET /domains/{domain}/rrsets/{subname}/{type}/` (`@` for the apex, 404 when absent);
//! create/replace/delete in one atomic bulk `PATCH /domains/{domain}/rrsets/` with
//! `[{subname, type, ttl, records}]` (empty `records` deletes). Wildcards only as the first
//! label.

use serde_json::{Value, json};

use super::fail;
use crate::http::{Http, enc};
use crate::{DnsApi, DnsError, RecordSet, Rtype, Zone, relative};

pub const BASE: &str = "https://desec.io/api/v1";

pub struct Desec {
    pub http: Http,
    pub base: String,
    pub token: String,
}

fn message(v: &Value) -> Option<String> {
    match v {
        Value::Object(o) => o
            .get("detail")
            .and_then(|d| d.as_str())
            .map(String::from)
            .or_else(|| Some(v.to_string())),
        Value::Array(_) => Some(v.to_string()),
        _ => None,
    }
}

impl Desec {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), DnsError> {
        let r = self.http.send(
            method,
            &format!("{}{path}", self.base),
            &[("Authorization", format!("Token {}", self.token))],
            body,
        )?;
        let v = r.json().unwrap_or(Value::Null);
        if r.status == 404 && method == "GET" {
            return Ok((404, Value::Null));
        }
        if !r.ok() {
            return Err(fail(&self.http, &r, message(&v)));
        }
        Ok((r.status, v))
    }

    fn patch(
        &self,
        zone: &Zone,
        fqdn: &str,
        rtype: Rtype,
        records: &[String],
        ttl: u32,
    ) -> Result<(), DnsError> {
        let body = json!([{
            "subname": relative(fqdn, &zone.name),
            "type": rtype.as_str(),
            "ttl": ttl.max(zone.min_ttl),
            "records": records,
        }]);
        self.call(
            "PATCH",
            &format!("/domains/{}/rrsets/", enc(&zone.name)),
            Some(&body),
        )?;
        Ok(())
    }
}

impl DnsApi for Desec {
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
        let q = fqdn.strip_prefix("*.").unwrap_or(fqdn);
        let (_, v) = self.call("GET", &format!("/domains/?owns_qname={}", enc(q)), None)?;
        let d = v
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| DnsError::NoZone(fqdn.into()))?;
        let name = d["name"]
            .as_str()
            .ok_or_else(|| DnsError::NoZone(fqdn.into()))?;
        Ok(Zone {
            id: name.into(),
            name: name.into(),
            min_ttl: d["minimum_ttl"].as_u64().unwrap_or(3600) as u32,
        })
    }

    fn get(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<Option<RecordSet>, DnsError> {
        let sub = relative(fqdn, &zone.name);
        let sub = if sub.is_empty() { "@".to_string() } else { sub };
        let (status, v) = self.call(
            "GET",
            &format!(
                "/domains/{}/rrsets/{}/{}/",
                enc(&zone.name),
                enc(&sub),
                rtype
            ),
            None,
        )?;
        if status == 404 {
            return Ok(None);
        }
        let values: Vec<String> = v["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(String::from))
            .collect();
        if values.is_empty() {
            return Ok(None);
        }
        Ok(Some(RecordSet {
            values,
            ttl: v["ttl"].as_u64().unwrap_or(0) as u32,
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
        self.patch(zone, fqdn, rtype, values, ttl)
    }

    fn delete(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<(), DnsError> {
        self.patch(zone, fqdn, rtype, &[], zone.min_ttl)
    }
}
