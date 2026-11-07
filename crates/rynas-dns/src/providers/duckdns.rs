//! DuckDNS (https://www.duckdns.org/spec.jsp): one address pair per `<name>.duckdns.org`,
//! set with `GET /update?domains=<name>&token=<token>&ip=<v4>` (or `ipv6=`), cleared with
//! `&clear=true`; the answer is `OK` or `KO`. DuckDNS answers every name below
//! `<name>.duckdns.org` with the same address, so the one record covers `*.<name>`. There is
//! no read API.

use super::fail;
use crate::http::{Http, enc};
use crate::{DnsApi, DnsError, RecordSet, Rtype, Zone};

pub const BASE: &str = "https://www.duckdns.org";

pub struct Duckdns {
    pub http: Http,
    pub base: String,
    pub token: String,
}

/// `<name>` of `<name>.duckdns.org` (or of a name below it).
pub fn account_name(fqdn: &str) -> Option<String> {
    let base = fqdn.strip_suffix(".duckdns.org")?;
    let name = base.rsplit('.').next()?;
    (!name.is_empty() && name != "*").then(|| name.to_string())
}

impl Duckdns {
    fn update(&self, name: &str, extra: &str) -> Result<(), DnsError> {
        let url = format!(
            "{}/update?domains={}&token={}{extra}",
            self.base,
            enc(name),
            enc(&self.token)
        );
        let r = self.http.send("GET", &url, &[], None)?;
        if !r.ok() || r.body.trim() != "OK" {
            return Err(fail(
                &self.http,
                &r,
                Some(format!(
                    "DuckDNS answered {} (HTTP {}): wrong token or name?",
                    r.body.trim(),
                    r.status
                )),
            ));
        }
        Ok(())
    }
}

impl DnsApi for Duckdns {
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
        let name = account_name(fqdn).ok_or_else(|| DnsError::NoZone(fqdn.into()))?;
        Ok(Zone {
            name: format!("{name}.duckdns.org"),
            id: name,
            min_ttl: 60,
        })
    }

    fn get(&self, _zone: &Zone, _fqdn: &str, _rtype: Rtype) -> Result<Option<RecordSet>, DnsError> {
        Err(DnsError::Unsupported("DuckDNS has no read API".into()))
    }

    fn set(
        &self,
        zone: &Zone,
        fqdn: &str,
        rtype: Rtype,
        values: &[String],
        _ttl: u32,
    ) -> Result<(), DnsError> {
        if fqdn != zone.name {
            return Err(DnsError::Unsupported(format!(
                "DuckDNS holds one address per name ({}), not {fqdn}",
                zone.name
            )));
        }
        let v = values
            .first()
            .ok_or_else(|| DnsError::Unsupported("DuckDNS needs exactly one address".into()))?;
        let param = match rtype {
            Rtype::A => "ip",
            Rtype::Aaaa => "ipv6",
        };
        self.update(&zone.id, &format!("&{param}={}", enc(v)))
    }

    fn delete(&self, zone: &Zone, _fqdn: &str, _rtype: Rtype) -> Result<(), DnsError> {
        self.update(&zone.id, "&clear=true")
    }

    fn readable(&self) -> bool {
        false
    }
}
