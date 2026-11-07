//! Porkbun API v3 (https://porkbun.com/api/json/v3/documentation): every call is a `POST`
//! with `{apikey, secretapikey}` in the JSON body. Domains by `/domain/listAll`; records by
//! `/dns/retrieveByNameType/{domain}/{type}/{subdomain}`, `/dns/create/{domain}`
//! `{name, type, content, ttl}`, `/dns/editByNameType/{domain}/{type}/{subdomain}`
//! `{content, ttl}`, `/dns/deleteByNameType/{domain}/{type}/{subdomain}` (no subdomain
//! segment for the apex). Answers carry `status: SUCCESS|ERROR` and `message`. TTL ≥ 600.

use serde_json::{Value, json};

use super::{fail, sorted};
use crate::http::{Http, enc};
use crate::{DnsApi, DnsError, RecordSet, Rtype, Zone, relative, zone_candidates};

pub const BASE: &str = "https://api.porkbun.com/api/json/v3";
pub const MIN_TTL: u32 = 600;

pub struct Porkbun {
    pub http: Http,
    pub base: String,
    pub key: String,
    pub secret: String,
}

impl Porkbun {
    fn call(&self, path: &str, extra: Value) -> Result<Value, DnsError> {
        let mut body = json!({"apikey": self.key, "secretapikey": self.secret});
        if let (Some(b), Value::Object(e)) = (body.as_object_mut(), extra) {
            b.extend(e);
        }
        let r = self
            .http
            .send("POST", &format!("{}{path}", self.base), &[], Some(&body))?;
        let v = r.json().unwrap_or(Value::Null);
        if !r.ok() || v["status"] != "SUCCESS" {
            return Err(fail(
                &self.http,
                &r,
                v["message"].as_str().map(String::from),
            ));
        }
        Ok(v)
    }

    fn by_name(op: &str, zone: &Zone, fqdn: &str, rtype: Rtype) -> String {
        let sub = relative(fqdn, &zone.name);
        if sub.is_empty() {
            format!("/dns/{op}/{}/{}", enc(&zone.name), rtype)
        } else {
            format!("/dns/{op}/{}/{}/{}", enc(&zone.name), rtype, enc(&sub))
        }
    }
}

impl DnsApi for Porkbun {
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
        let v = self.call("/domain/listAll", json!({"start": "0"}))?;
        let owned: Vec<String> = v["domains"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["domain"].as_str().map(|s| s.to_ascii_lowercase()))
            .collect();
        zone_candidates(fqdn)
            .into_iter()
            .find(|c| owned.contains(c))
            .map(|name| Zone {
                id: name.clone(),
                name,
                min_ttl: MIN_TTL,
            })
            .ok_or_else(|| DnsError::NoZone(fqdn.into()))
    }

    fn get(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<Option<RecordSet>, DnsError> {
        let v = self.call(
            &Self::by_name("retrieveByNameType", zone, fqdn, rtype),
            json!({}),
        )?;
        let recs: Vec<&Value> = v["records"].as_array().into_iter().flatten().collect();
        if recs.is_empty() {
            return Ok(None);
        }
        let ttl = recs[0]["ttl"]
            .as_str()
            .and_then(|t| t.parse().ok())
            .or_else(|| recs[0]["ttl"].as_u64().map(|t| t as u32))
            .unwrap_or(0);
        Ok(Some(RecordSet {
            values: recs
                .iter()
                .filter_map(|r| r["content"].as_str().map(String::from))
                .collect(),
            ttl,
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
        let ttl = ttl.max(MIN_TTL).to_string();
        let cur = self.get(zone, fqdn, rtype)?;
        match (&cur, values) {
            // one value replacing what is there: edit in place
            (Some(c), [v]) if c.values.len() == 1 => {
                if sorted(&c.values) != sorted(values) || c.ttl.to_string() != ttl {
                    self.call(
                        &Self::by_name("editByNameType", zone, fqdn, rtype),
                        json!({"content": v, "ttl": ttl}),
                    )?;
                }
            }
            _ => {
                if cur.is_some() {
                    self.delete(zone, fqdn, rtype)?;
                }
                for v in values {
                    self.call(
                        &format!("/dns/create/{}", enc(&zone.name)),
                        json!({"name": relative(fqdn, &zone.name), "type": rtype.as_str(),
                               "content": v, "ttl": ttl}),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn delete(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<(), DnsError> {
        if self.get(zone, fqdn, rtype)?.is_none() {
            return Ok(());
        }
        self.call(
            &Self::by_name("deleteByNameType", zone, fqdn, rtype),
            json!({}),
        )?;
        Ok(())
    }
}
