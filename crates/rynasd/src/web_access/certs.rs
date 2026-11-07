//! The certificate dashboard (wave W2): every certificate rynas serves, read-only — the local
//! CA, the interface's own certificate (`:8443`), the proxy's leaf for the `.local` names,
//! and the ACME certificates rynas-caddy keeps in its storage for the own domains — with
//! expiry, renewal status and the last error the engine logged; and the D-NOTIFY events of
//! certificate trouble (firing on a change into trouble, resolved when it clears).

use std::collections::BTreeMap;

use rynas_api_types::notify::{NewEvent, NotifyCategory, NotifySeverity};
use rynas_api_types::tls::TlsCertificate;
use rynas_api_types::web_access::{CertEntry, CertKind, CertStatus};
use rynas_notify::event::{code, field, firing, resolved};

pub const DAY_MS: i64 = 86_400_000;
/// `expires_soon` below this.
pub const SOON_MS: i64 = 14 * DAY_MS;
/// A missing ACME certificate is `pending` this long after its domain appeared, then `failed`.
pub const ISSUE_GRACE_MS: i64 = 15 * 60_000;
/// Past the renewal point by this much: `renewal_overdue`.
pub const OVERDUE_MS: i64 = DAY_MS;

/// From when certmagic renews: 2/3 of the lifetime (renewal window ratio 1/3; ARI may say
/// earlier, never later).
pub fn renew_after(c: &TlsCertificate) -> i64 {
    c.not_before_ms + (c.not_after_ms - c.not_before_ms) * 2 / 3
}

/// Status of a present certificate at `now`; `acme` adds the renewal check (the others are
/// renewed by rynas itself, or never: imported).
pub fn status_of(c: &TlsCertificate, acme: bool, now: i64) -> CertStatus {
    if c.not_after_ms <= now {
        CertStatus::Expired
    } else if acme && now > renew_after(c) + OVERDUE_MS {
        CertStatus::RenewalOverdue
    } else if c.not_after_ms - now < SOON_MS {
        CertStatus::ExpiresSoon
    } else {
        CertStatus::Ok
    }
}

/// Status of a missing ACME certificate expected since `since`.
pub fn missing_status(since: i64, now: i64) -> CertStatus {
    if now - since > ISSUE_GRACE_MS {
        CertStatus::Failed
    } else {
        CertStatus::Pending
    }
}

/// Is the status trouble worth a notification?
pub fn trouble(s: CertStatus) -> Option<NotifySeverity> {
    match s {
        CertStatus::Ok | CertStatus::Pending => None,
        CertStatus::Expired => Some(NotifySeverity::Critical),
        CertStatus::ExpiresSoon | CertStatus::RenewalOverdue | CertStatus::Failed => {
            Some(NotifySeverity::Warning)
        }
    }
}

fn status_str(s: CertStatus) -> &'static str {
    match s {
        CertStatus::Ok => "ok",
        CertStatus::ExpiresSoon => "expires_soon",
        CertStatus::Expired => "expired",
        CertStatus::RenewalOverdue => "renewal_overdue",
        CertStatus::Pending => "pending",
        CertStatus::Failed => "failed",
    }
}

const SRC: &str = "web-access";

/// The event of `e` entering its status, or leaving trouble (`None` when `e` is fine).
pub fn event_for(e: &CertEntry) -> Option<NewEvent> {
    let key = format!("web-access:cert:{}", e.id);
    let name = e.names.first().cloned().unwrap_or_else(|| e.id.clone());
    let sev = trouble(e.status)?;
    let (title, body) = match e.status {
        CertStatus::Expired => (
            format!("The certificate for {name} has expired"),
            "Browsers and apps refuse the connection. Look at the certificate dashboard.",
        ),
        CertStatus::ExpiresSoon => (
            format!("The certificate for {name} expires soon"),
            "It has not been renewed. Look at the certificate dashboard.",
        ),
        CertStatus::RenewalOverdue => (
            format!("The certificate for {name} is not being renewed"),
            "Its renewal is overdue: check the DNS provider credential and the domain's checklist.",
        ),
        _ => (
            format!("No certificate could be obtained for {name}"),
            "Issuance keeps failing: check the DNS provider credential and the domain's checklist.",
        ),
    };
    let mut ev = firing(NotifyCategory::System, sev, SRC, key, title);
    ev.body = match &e.last_error {
        Some(err) => format!("{body} Last error: {err}"),
        None => body.into(),
    };
    let ev = code(ev, &format!("web_access.cert_{}", status_str(e.status)));
    let ev = field(ev, "name", &name);
    Some(field(ev, "cert", &e.id))
}

/// The resolved event of `id` (for `name`).
pub fn resolved_for(id: &str, name: &str) -> NewEvent {
    let ev = resolved(
        NotifyCategory::System,
        SRC,
        format!("web-access:cert:{id}"),
        format!("The certificate for {name} is fine again"),
    );
    let ev = code(ev, "web_access.cert_ok");
    let ev = field(ev, "name", name);
    field(ev, "cert", id)
}

/// Notification state per certificate id: the status last notified. Emits only on changes,
/// so a certificate in trouble notifies once (and again if it gets worse), and once when it
/// clears.
#[derive(Debug, Default)]
pub struct Notified {
    last: BTreeMap<String, (CertStatus, String)>,
}

impl Notified {
    /// The events for the current inventory.
    pub fn events(&mut self, entries: &[CertEntry]) -> Vec<NewEvent> {
        let mut out = Vec::new();
        let mut seen = Vec::new();
        for e in entries {
            seen.push(e.id.clone());
            let name = e.names.first().cloned().unwrap_or_else(|| e.id.clone());
            let bad = trouble(e.status).is_some();
            let prev = self.last.get(&e.id).map(|(s, _)| *s);
            match (bad, prev) {
                (true, Some(p)) if p == e.status => {}
                (true, _) => {
                    out.extend(event_for(e));
                    self.last.insert(e.id.clone(), (e.status, name));
                }
                (false, Some(_)) => {
                    out.push(resolved_for(&e.id, &name));
                    self.last.remove(&e.id);
                }
                (false, None) => {}
            }
        }
        // a certificate that went away (domain removed): its trouble is over
        let gone: Vec<String> = self
            .last
            .keys()
            .filter(|k| !seen.contains(k))
            .cloned()
            .collect();
        for k in gone {
            if let Some((_, name)) = self.last.remove(&k) {
                out.push(resolved_for(&k, &name));
            }
        }
        out
    }
}

/// The last error per certificate subject from the engine's JSON log lines (Caddy logs
/// `{"level":"error","logger":"tls.obtain"|"tls.renew"|"tls.issuance.acme...",
/// "identifier":"<name>","error":"..."}`); a later success line (`"msg":"certificate
/// obtained successfully"` / `"renewed"`) clears it.
pub fn engine_errors(log: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in log.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let logger = v["logger"].as_str().unwrap_or("");
        if !logger.starts_with("tls") {
            continue;
        }
        let Some(id) = v["identifier"].as_str() else {
            continue;
        };
        let msg = v["msg"].as_str().unwrap_or("");
        if v["level"] == "error" {
            let err = v["error"].as_str().unwrap_or(msg);
            let text = if msg.is_empty() || err == msg {
                err.to_string()
            } else {
                format!("{msg}: {err}")
            };
            let mut t = text;
            if t.len() > 400 {
                let mut cut = 400;
                while !t.is_char_boundary(cut) {
                    cut -= 1;
                }
                t.truncate(cut);
                t.push('…');
            }
            out.insert(id.to_ascii_lowercase(), t);
        } else if msg.contains("obtained successfully") || msg.contains("renewed") {
            out.remove(&id.to_ascii_lowercase());
        }
    }
    out
}

/// An entry with a certificate.
pub fn entry(id: &str, kind: CertKind, cert: TlsCertificate, acme: bool, now: i64) -> CertEntry {
    let staging = cert.issuer.contains("STAGING") || cert.issuer.contains("(STAGING)");
    CertEntry {
        id: id.into(),
        kind,
        status: status_of(&cert, acme, now),
        names: cert.dns_names.clone(),
        staging: acme && staging,
        renew_after_ms: acme.then(|| renew_after(&cert)),
        certificate: Some(cert),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(nb: i64, na: i64) -> TlsCertificate {
        TlsCertificate {
            dns_names: vec!["*.nas.example.com".into()],
            not_before_ms: nb,
            not_after_ms: na,
            issuer: "CN=E7,O=Let's Encrypt,C=US".into(),
            ..Default::default()
        }
    }

    #[test]
    fn statuses() {
        let life = 90 * DAY_MS;
        let c = cert(0, life);
        assert_eq!(status_of(&c, true, 10 * DAY_MS), CertStatus::Ok);
        assert_eq!(renew_after(&c), 60 * DAY_MS);
        assert_eq!(
            status_of(&c, true, 60 * DAY_MS + DAY_MS / 2),
            CertStatus::Ok
        );
        assert_eq!(status_of(&c, true, 62 * DAY_MS), CertStatus::RenewalOverdue);
        assert_eq!(
            status_of(&c, false, 62 * DAY_MS),
            CertStatus::Ok,
            "not ACME"
        );
        assert_eq!(status_of(&c, false, 80 * DAY_MS), CertStatus::ExpiresSoon);
        assert_eq!(status_of(&c, true, life), CertStatus::Expired);
        assert_eq!(missing_status(0, 60_000), CertStatus::Pending);
        assert_eq!(missing_status(0, ISSUE_GRACE_MS + 1), CertStatus::Failed);
    }

    #[test]
    fn notifications_fire_once_and_resolve() {
        let mut n = Notified::default();
        let mut e = entry(
            "acme:*.nas.example.com",
            CertKind::Acme,
            cert(0, 90 * DAY_MS),
            true,
            62 * DAY_MS,
        );
        assert_eq!(e.status, CertStatus::RenewalOverdue);
        let ev = n.events(std::slice::from_ref(&e));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].code, "web_access.cert_renewal_overdue");
        assert_eq!(
            ev[0].key.as_deref(),
            Some("web-access:cert:acme:*.nas.example.com")
        );
        assert!(n.events(std::slice::from_ref(&e)).is_empty(), "no repeat");
        e.status = CertStatus::Expired;
        let ev = n.events(std::slice::from_ref(&e));
        assert_eq!(ev[0].severity, NotifySeverity::Critical, "worse: again");
        e.status = CertStatus::Ok;
        let ev = n.events(std::slice::from_ref(&e));
        assert_eq!(ev[0].code, "web_access.cert_ok");
        assert_eq!(ev[0].state, rynas_api_types::notify::EventState::Resolved);
        assert!(n.events(&[e.clone()]).is_empty());
        // pending is not trouble; failed is; gone resolves
        e.status = CertStatus::Pending;
        assert!(n.events(&[e.clone()]).is_empty());
        e.status = CertStatus::Failed;
        assert_eq!(n.events(&[e.clone()]).len(), 1);
        let ev = n.events(&[]);
        assert_eq!(ev[0].state, rynas_api_types::notify::EventState::Resolved);
    }

    #[test]
    fn engine_log_errors() {
        let log = r#"{"level":"info","logger":"tls.obtain","msg":"acquiring lock","identifier":"*.nas.example.com"}
{"level":"error","logger":"tls.obtain","msg":"could not get certificate from issuer","identifier":"*.nas.example.com","error":"HTTP 403 urn:ietf:params:acme:error:unauthorized"}
{"level":"error","logger":"tls.renew","msg":"will retry","identifier":"nas.example.com","error":"presenting for challenge: cloudflare: 9109 invalid token"}
not json
{"level":"info","logger":"tls.renew","msg":"certificate renewed successfully","identifier":"nas.example.com"}"#;
        let e = engine_errors(log);
        assert_eq!(e.len(), 1);
        assert!(
            e["*.nas.example.com"].starts_with("could not get certificate from issuer: HTTP 403")
        );
    }
}
