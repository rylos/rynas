//! The dyndns2 update protocol most dynamic DNS services and routers speak
//! (https://help.dyn.com/remote-access-api/perform-update/, also the services that copied
//! it): `GET https://<server>/nic/update?hostname=<fqdn>&myip=<address>` with HTTP Basic
//! authentication. The answer is one line: `good <ip>` or `nochg <ip>` when the address is
//! set, a code (`badauth`, `nohost`, `notfqdn`, `abuse`, `badagent`, `dnserr`, `911`)
//! otherwise. There is no read API: the address is written when the public resolvers show
//! another one.

use base64::Engine;

use super::fail;
use crate::http::{Http, enc};
use crate::{DnsApi, DnsError, RecordSet, Rtype, Zone};

pub struct Dyndns2 {
    pub http: Http,
    /// `https://<server>` (a bare host name gets `https://`).
    pub base: String,
    pub username: String,
    pub password: String,
}

/// The base URL of a `server` value: a host name (`members.example.org`) or a URL.
pub fn base_of(server: &str) -> Option<String> {
    let s = server.trim().trim_end_matches('/');
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    if let Some(rest) = s.strip_prefix("https://") {
        return (!rest.is_empty()).then(|| s.to_string());
    }
    // plain http would send the password in the clear: refused
    if s.contains("://") {
        return None;
    }
    Some(format!("https://{s}"))
}

/// What a dyndns2 answer means: `Ok` for `good`/`nochg`, else the reason.
pub fn verdict(body: &str) -> Result<(), String> {
    let line = body.lines().next().unwrap_or("").trim();
    let code = line.split_whitespace().next().unwrap_or("");
    match code {
        "good" | "nochg" => Ok(()),
        "badauth" => Err("wrong user name or password (badauth)".into()),
        "nohost" => Err("the host name does not exist in this account (nohost)".into()),
        "notfqdn" => Err("not a fully qualified host name (notfqdn)".into()),
        "abuse" => Err("the service blocked this host for too many updates (abuse)".into()),
        "badagent" => Err("the service refused the client (badagent)".into()),
        "dnserr" | "911" => Err(format!("the service has a problem ({code}): try again later")),
        "" => Err("empty answer".into()),
        other => Err(format!("unexpected answer: {}", other.chars().take(60).collect::<String>())),
    }
}

impl DnsApi for Dyndns2 {
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
        Ok(Zone {
            id: fqdn.to_string(),
            name: fqdn.to_string(),
            min_ttl: 60,
        })
    }

    fn get(&self, _zone: &Zone, _fqdn: &str, _rtype: Rtype) -> Result<Option<RecordSet>, DnsError> {
        Err(DnsError::Unsupported("the dyndns2 protocol has no read API".into()))
    }

    fn set(&self, _zone: &Zone, fqdn: &str, _rtype: Rtype, values: &[String], _ttl: u32) -> Result<(), DnsError> {
        let ip = values
            .first()
            .ok_or_else(|| DnsError::Unsupported("dyndns2 sets exactly one address".into()))?;
        let url = format!("{}/nic/update?hostname={}&myip={}", self.base, enc(fqdn), enc(ip));
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.username, self.password));
        let r = self.http.send("GET", &url, &[("Authorization", format!("Basic {auth}"))], None)?;
        if !r.ok() {
            return Err(fail(&self.http, &r, None));
        }
        verdict(&r.body).map_err(|m| {
            let m = self.http.clean(&m);
            if r.body.trim_start().starts_with("badauth") { DnsError::Auth(m) } else { DnsError::Api(m) }
        })
    }

    fn delete(&self, _zone: &Zone, _fqdn: &str, _rtype: Rtype) -> Result<(), DnsError> {
        Err(DnsError::Unsupported("the dyndns2 protocol cannot remove an address".into()))
    }

    fn readable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_and_servers() {
        assert!(verdict("good 1.2.3.4").is_ok());
        assert!(verdict("nochg 1.2.3.4\n").is_ok());
        assert!(verdict("badauth").unwrap_err().contains("badauth"));
        assert!(verdict("911").is_err());
        assert!(verdict("<html>").is_err());
        assert_eq!(base_of("members.example.org").as_deref(), Some("https://members.example.org"));
        assert_eq!(base_of("https://api.example.com/").as_deref(), Some("https://api.example.com"));
        assert_eq!(base_of("http://x.example"), None);
        assert_eq!(base_of(" "), None);
    }
}
