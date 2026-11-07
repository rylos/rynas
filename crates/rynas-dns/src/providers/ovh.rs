//! OVHcloud API (https://help.ovhcloud.com/csm/en-api-getting-started-ovhcloud-api): every
//! call carries `X-Ovh-Application`, `X-Ovh-Consumer`, `X-Ovh-Timestamp` (server time from
//! `GET /auth/time`) and `X-Ovh-Signature` = `"$1$" + sha1_hex(AS+"+"+CK+"+"+METHOD+"+"+URL
//! +"+"+BODY+"+"+TS)`. Zones by `GET /domain/zone`; record ids by
//! `GET /domain/zone/{zone}/record?fieldType=&subDomain=`, each by `GET .../record/{id}`
//! `{target, ttl}`; `POST .../record` `{fieldType, subDomain, target, ttl}`, `PUT .../record/{id}`
//! `{target, ttl}`, `DELETE .../record/{id}`, then `POST /domain/zone/{zone}/refresh`.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha1::{Digest, Sha1};

use super::fail;
use crate::http::{Http, enc};
use crate::{DnsApi, DnsError, RecordSet, Rtype, Zone, relative, zone_candidates};

/// The API of an endpoint name (`ovh-eu`, `ovh-ca`, `ovh-us`).
pub fn endpoint_url(name: &str) -> Option<&'static str> {
    match name {
        "ovh-eu" => Some("https://eu.api.ovh.com/1.0"),
        "ovh-ca" => Some("https://ca.api.ovh.com/1.0"),
        "ovh-us" => Some("https://api.us.ovhcloud.com/1.0"),
        _ => None,
    }
}

pub struct Ovh {
    http: Http,
    base: String,
    app_key: String,
    app_secret: String,
    consumer: String,
    /// Server time minus local time, once known.
    delta: Mutex<Option<i64>>,
}

/// The signature of one request.
pub fn signature(
    secret: &str,
    consumer: &str,
    method: &str,
    url: &str,
    body: &str,
    ts: i64,
) -> String {
    let mut h = Sha1::new();
    h.update(format!("{secret}+{consumer}+{method}+{url}+{body}+{ts}").as_bytes());
    format!("$1${}", hex::encode(h.finalize()))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Ovh {
    pub fn new(
        http: Http,
        base: String,
        app_key: String,
        app_secret: String,
        consumer: String,
    ) -> Self {
        Self {
            http,
            base,
            app_key,
            app_secret,
            consumer,
            delta: Mutex::new(None),
        }
    }

    fn time(&self) -> Result<i64, DnsError> {
        let mut d = self.delta.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = *d {
            return Ok(now() + d);
        }
        let r = self
            .http
            .send("GET", &format!("{}/auth/time", self.base), &[], None)?;
        let t: i64 = r
            .body
            .trim()
            .parse()
            .map_err(|_| fail(&self.http, &r, None))?;
        *d = Some(t - now());
        Ok(t)
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, DnsError> {
        let url = format!("{}{path}", self.base);
        let text = body.map(|b| b.to_string()).unwrap_or_default();
        let ts = self.time()?;
        let sig = signature(&self.app_secret, &self.consumer, method, &url, &text, ts);
        let r = self.http.send(
            method,
            &url,
            &[
                ("X-Ovh-Application", self.app_key.clone()),
                ("X-Ovh-Consumer", self.consumer.clone()),
                ("X-Ovh-Timestamp", ts.to_string()),
                ("X-Ovh-Signature", sig),
            ],
            body,
        )?;
        let v = r.json().unwrap_or(Value::Null);
        if !r.ok() {
            return Err(fail(
                &self.http,
                &r,
                v["message"].as_str().map(String::from),
            ));
        }
        Ok(v)
    }

    fn ids(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<Vec<u64>, DnsError> {
        let v = self.call(
            "GET",
            &format!(
                "/domain/zone/{}/record?fieldType={}&subDomain={}",
                enc(&zone.name),
                rtype,
                enc(&relative(fqdn, &zone.name))
            ),
            None,
        )?;
        Ok(v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i.as_u64())
            .collect())
    }

    fn refresh(&self, zone: &Zone) -> Result<(), DnsError> {
        self.call(
            "POST",
            &format!("/domain/zone/{}/refresh", enc(&zone.name)),
            None,
        )?;
        Ok(())
    }
}

impl DnsApi for Ovh {
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
        let v = self.call("GET", "/domain/zone", None)?;
        let zones: Vec<String> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|z| z.as_str().map(|s| s.to_ascii_lowercase()))
            .collect();
        zone_candidates(fqdn)
            .into_iter()
            .find(|c| zones.contains(c))
            .map(|name| Zone {
                id: name.clone(),
                name,
                min_ttl: 60,
            })
            .ok_or_else(|| DnsError::NoZone(fqdn.into()))
    }

    fn get(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<Option<RecordSet>, DnsError> {
        let mut out = RecordSet::default();
        for id in self.ids(zone, fqdn, rtype)? {
            let r = self.call(
                "GET",
                &format!("/domain/zone/{}/record/{id}", enc(&zone.name)),
                None,
            )?;
            if let Some(t) = r["target"].as_str() {
                out.values.push(t.to_string());
                out.ttl = r["ttl"].as_u64().unwrap_or(0) as u32;
            }
        }
        Ok((!out.values.is_empty()).then_some(out))
    }

    fn set(
        &self,
        zone: &Zone,
        fqdn: &str,
        rtype: Rtype,
        values: &[String],
        ttl: u32,
    ) -> Result<(), DnsError> {
        let ids = self.ids(zone, fqdn, rtype)?;
        let z = enc(&zone.name);
        for (i, v) in values.iter().enumerate() {
            match ids.get(i) {
                Some(id) => {
                    self.call(
                        "PUT",
                        &format!("/domain/zone/{z}/record/{id}"),
                        Some(&json!({"target": v, "ttl": ttl})),
                    )?;
                }
                None => {
                    self.call(
                        "POST",
                        &format!("/domain/zone/{z}/record"),
                        Some(&json!({"fieldType": rtype.as_str(),
                                     "subDomain": relative(fqdn, &zone.name),
                                     "target": v, "ttl": ttl})),
                    )?;
                }
            }
        }
        for id in ids.iter().skip(values.len()) {
            self.call("DELETE", &format!("/domain/zone/{z}/record/{id}"), None)?;
        }
        self.refresh(zone)
    }

    fn delete(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<(), DnsError> {
        let ids = self.ids(zone, fqdn, rtype)?;
        if ids.is_empty() {
            return Ok(());
        }
        for id in ids {
            self.call(
                "DELETE",
                &format!("/domain/zone/{}/record/{id}", enc(&zone.name)),
                None,
            )?;
        }
        self.refresh(zone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_shape() {
        // sha1("AS+CK+GET+https://eu.api.ovh.com/1.0/domain/zone++1700000000")
        let s = signature(
            "AS",
            "CK",
            "GET",
            "https://eu.api.ovh.com/1.0/domain/zone",
            "",
            1_700_000_000,
        );
        let mut h = Sha1::new();
        h.update(b"AS+CK+GET+https://eu.api.ovh.com/1.0/domain/zone++1700000000");
        assert_eq!(s, format!("$1${}", hex::encode(h.finalize())));
        assert_eq!(s.len(), 3 + 40);
    }
}
