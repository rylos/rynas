//! What the agent trusts of its NAS: the NAS' **local certificate authority** and its
//! identity (`rynas_api_types::pcagent::AgentTrust`), delivered at pairing over the
//! connection the pairing code and the certificate fingerprint already authenticate, and
//! refreshed on later contacts. The NAS' HTTPS port serves, to a client asking for the name
//! `<nas id>.rynas.invalid`, a certificate issued by that authority for that name only; the
//! client checks it with rustls' WebPKI verifier and only that authority as root
//! ([`crate::http::Trust::Ca`]). So a new host name, a new address or a re-issued certificate
//! need no pairing again, while a certificate from another authority, or for another NAS
//! identity, is refused.
//!
//! Agents paired before (a pin of the exact certificate, [`State::fingerprint`]) are
//! upgraded on their next contact with a NAS that has it ([`refresh`]): the trust comes over
//! the pinned connection (so the pin matched), and is kept only once a second connection
//! that trusts nothing but that authority and identity has worked (so the NAS holds the key
//! of a certificate the authority issued for its identity). A NAS without it (404, older
//! versions) leaves the pin as it is.
//!
//! The addresses the NAS announces (host name, mDNS name, interface addresses, all on its own
//! HTTPS port) are tried in order when the stored one stops answering; `rynas-agent
//! set-server <url>` sets a new one by hand, checked the same way.

use std::sync::Arc;

use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use rynas_api_types::pcagent::{AGENT_MAX_ADDRESSES, AGENT_MAX_CAS, AGENT_TRUST, AgentTrust, agent_tls_name, valid_nas_id};

use crate::http::{Client, Endpoint, Trust};
use crate::state::{State, now_ms};

/// How often the service refreshes the trust and the addresses from the NAS.
pub const REFRESH_EVERY_MS: u64 = 6 * 3_600_000;
/// After a failed refresh, the next try.
pub const RETRY_MS: u64 = 30 * 60_000;

/// An [`AgentTrust`] checked: a valid identity, one to [`AGENT_MAX_CAS`] CA certificates
/// usable as roots, the parseable HTTPS addresses (at most [`AGENT_MAX_ADDRESSES`]).
#[derive(Debug, Clone)]
pub struct Checked {
    pub nas_id: String,
    pub cas: Vec<String>,
    pub addresses: Vec<String>,
    roots: Arc<RootCertStore>,
}

impl Checked {
    /// The client trust: these roots only, the identity's name.
    pub fn trust(&self) -> Trust {
        Trust::Ca {
            name: agent_tls_name(&self.nas_id),
            roots: self.roots.clone(),
        }
    }
}

pub fn check(t: &AgentTrust) -> Result<Checked, String> {
    if !valid_nas_id(&t.nas_id) {
        return Err("the NAS sent a bad identity".into());
    }
    if t.cas.is_empty() || t.cas.len() > AGENT_MAX_CAS {
        return Err(format!("the NAS sent {} CA certificates", t.cas.len()));
    }
    let mut roots = RootCertStore::empty();
    for pem in &t.cas {
        let mut certs = CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "the NAS sent a bad CA certificate".to_string())?;
        if certs.len() != 1 {
            return Err("the NAS sent a bad CA certificate (one per entry)".into());
        }
        roots.add(certs.remove(0)).map_err(|e| format!("the NAS sent a bad CA certificate: {e}"))?;
    }
    let mut addresses: Vec<String> = Vec::new();
    for a in &t.addresses {
        if addresses.len() >= AGENT_MAX_ADDRESSES {
            break;
        }
        if let Ok(ep) = Endpoint::parse(a)
            && ep.tls
            && !ep.host.is_empty()
            && !addresses.contains(&ep.url())
        {
            addresses.push(ep.url());
        }
    }
    Ok(Checked {
        nas_id: t.nas_id.clone(),
        cas: t.cas.clone(),
        addresses,
        roots: Arc::new(roots),
    })
}

/// The CA trust stored in `st`, or `None` (a certificate pin).
pub fn stored(st: &State) -> Option<Checked> {
    if st.nas_id.is_empty() || st.cas.is_empty() {
        return None;
    }
    check(&AgentTrust {
        nas_id: st.nas_id.clone(),
        cas: st.cas.clone(),
        addresses: st.addresses.clone(),
    })
    .ok()
}

/// The addresses to try: `url` first, then `addresses` (no duplicates; unparseable ones are
/// left out).
pub fn endpoints(url: &str, addresses: &[String]) -> Result<Vec<Endpoint>, String> {
    let mut v = vec![Endpoint::parse(url)?];
    for a in addresses {
        if let Ok(e) = Endpoint::parse(a)
            && !v.contains(&e)
        {
            v.push(e);
        }
    }
    Ok(v)
}

/// The client of the paired PC: the NAS' CA and identity at every known address, or (paired
/// before) the pinned certificate at the stored address.
pub fn client(st: &State) -> Result<Client, String> {
    let c = match stored(st) {
        Some(t) => Client::with_trust(endpoints(&st.url, &st.addresses)?, t.trust()),
        None if !st.nas_id.is_empty() => {
            return Err("the stored certificate authority of the NAS is unreadable: pair this PC again".into());
        }
        None => Client::new(Endpoint::parse(&st.url)?, &st.fingerprint),
    };
    Ok(c.with_token(&st.token))
}

/// Proves `t` against the NAS at `eps` (HTTPS only): a connection that trusts nothing but its
/// CA and identity, with the device `token`, answering with the same identity. Returns the
/// address that answered and the NAS' trust as it is now.
pub fn prove(t: &Checked, mut eps: Vec<Endpoint>, token: &str) -> Result<(String, Checked), String> {
    eps.retain(|e| e.tls);
    if eps.is_empty() {
        return Err("no HTTPS address of the NAS to check its certificate authority at".into());
    }
    let c = Client::with_trust(eps, t.trust()).with_token(token);
    let now = check(&c.get_json::<AgentTrust>(AGENT_TRUST)?)?;
    if now.nas_id != t.nas_id {
        return Err("the NAS answered with another identity".into());
    }
    Ok((c.ep().url(), now))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refreshed {
    /// The NAS has no trust to give (404): the certificate pin stays.
    Unsupported,
    Same,
    /// New CA certificates, addresses, or the address in use.
    Changed,
    /// The certificate pin was replaced by the NAS' CA and identity.
    Upgraded,
}

/// One contact's refresh of `st` (the caller saves it):
/// - CA trust: the CA certificates and addresses the NAS announces now are stored (the answer
///   came over a connection checked with the stored CA and identity), and the address that
///   answered becomes the first one;
/// - certificate pin: the upgrade (module documentation).
pub fn refresh(st: &mut State) -> Result<Refreshed, String> {
    let c = client(st)?;
    let r = c
        .request("GET", AGENT_TRUST, &[("Accept", "application/json")], &[])
        .map_err(|e| format!("{}: {e}", c.ep().url()))?;
    if r.status == 404 {
        st.trust_checked_ms = Some(now_ms());
        return Ok(Refreshed::Unsupported);
    }
    if !r.ok() {
        return Err(r.error_text());
    }
    let got = serde_json::from_slice::<AgentTrust>(&r.body).map_err(|e| format!("bad answer from the NAS: {e}"))?;
    let got = check(&got)?;
    let used = c.ep();
    st.trust_checked_ms = Some(now_ms());
    if !st.nas_id.is_empty() {
        if got.nas_id != st.nas_id {
            return Err("the NAS answered with another identity".into());
        }
        let changed = st.cas != got.cas || st.addresses != got.addresses || st.url != used.url();
        st.cas = got.cas;
        st.addresses = got.addresses;
        st.url = used.url();
        return Ok(if changed { Refreshed::Changed } else { Refreshed::Same });
    }
    // an empty pin accepts any certificate: never the ground for trusting an authority
    if used.tls && st.fingerprint.is_empty() {
        return Err("no certificate pinned for this NAS: pair this PC again".into());
    }
    let (at, now) = prove(&got, endpoints(&used.url(), &got.addresses)?, &st.token)?;
    st.nas_id = now.nas_id;
    st.cas = now.cas;
    st.addresses = now.addresses;
    // a PC paired over plain HTTP (tests on a trusted link) keeps that address first
    if used.tls {
        st.url = at;
    }
    st.fingerprint.clear();
    Ok(Refreshed::Upgraded)
}

#[cfg(test)]
mod tests {
    //! A NAS in a thread that serves certificates the way rynas-web does (the agents' one for
    //! the reserved name, the interface's for any other), made by the NAS' own code.
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use rustls::server::{ClientHello, ResolvesServerCert};
    use rustls::sign::CertifiedKey;
    use rynas_api_types::pcagent::AGENT_TLS_SUFFIX;
    use rynas_tls::{Names, PemPair};

    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";
    const DAY: i64 = 86_400_000;

    #[derive(Debug)]
    struct ByName {
        ui: Arc<CertifiedKey>,
        agent: Option<Arc<CertifiedKey>>,
    }

    impl ResolvesServerCert for ByName {
        fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            if hello.server_name().is_some_and(|n| n.ends_with(AGENT_TLS_SUFFIX))
                && let Some(a) = &self.agent
            {
                return Some(a.clone());
            }
            Some(self.ui.clone())
        }
    }

    fn agent_leaf(ca: &PemPair, id: &str, now: i64) -> PemPair {
        let names = Names { dns: [agent_tls_name(id)].into(), ips: Default::default() };
        rynas_tls::issue_leaf(ca, &names, now).unwrap()
    }

    fn ui_leaf(ca: &PemPair) -> PemPair {
        rynas_tls::issue_leaf(ca, &rynas_tls::names("nas", &["127.0.0.1".parse().unwrap()]), rynas_tls::now()).unwrap()
    }

    /// Serves `ui` / `agent` on 127.0.0.1; the trust path answers the identity [`ID`], the
    /// CAs `cas` and the server's own address (404 without `cas`).
    fn nas(ui: &PemPair, agent: Option<&PemPair>, cas: Option<&[&PemPair]>) -> u16 {
        let resolver = ByName {
            ui: rynas_tls::certified_key(ui).unwrap(),
            agent: agent.map(|a| rynas_tls::certified_key(a).unwrap()),
        };
        let cfg = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(resolver)),
        );
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let body = cas.map(|c| serde_json::to_vec(&trust_of(c, port)).unwrap());
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(s) = s else { continue };
                let (cfg, body) = (cfg.clone(), body.clone());
                std::thread::spawn(move || {
                    let c = rustls::ServerConnection::new(cfg).unwrap();
                    let mut t = BufReader::new(rustls::StreamOwned::new(c, s));
                    let mut first = String::new();
                    if t.read_line(&mut first).is_err() {
                        return;
                    }
                    loop {
                        let mut l = String::new();
                        if t.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                            break;
                        }
                    }
                    let (status, b) = match (&body, first.contains(AGENT_TRUST)) {
                        (Some(b), true) => ("200 OK", b.clone()),
                        (None, true) => ("404 Not Found", b"{}".to_vec()),
                        _ => ("200 OK", b"{}".to_vec()),
                    };
                    let w = t.get_mut();
                    let _ = write!(w, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
                    let _ = w.write_all(&b);
                    let _ = w.flush();
                    w.conn.send_close_notify();
                    let _ = w.flush();
                });
            }
        });
        port
    }

    fn url(port: u16) -> String {
        format!("https://127.0.0.1:{port}")
    }

    fn trust_of(cas: &[&PemPair], port: u16) -> AgentTrust {
        AgentTrust {
            nas_id: ID.into(),
            cas: cas.iter().map(|c| c.chain.clone()).collect(),
            addresses: vec![url(port)],
        }
    }

    fn ca_client(ca: &PemPair, port: u16) -> Client {
        let t = check(&trust_of(&[ca], port)).unwrap();
        Client::with_trust(endpoints(&url(port), &[]).unwrap(), t.trust())
    }

    fn get(c: &Client) -> Result<(), String> {
        c.request("GET", "/api/v1/pcbackup/agent/plan", &[], &[]).map(|_| ()).map_err(|e| e.to_string())
    }

    #[test]
    fn the_pinned_ca_accepts_its_leaf_at_any_address() {
        let now = rynas_tls::now();
        let ca = rynas_tls::generate_ca("nas", now).unwrap();
        // the interface's certificate is of another authority (imported): it does not matter
        let port = nas(&ui_leaf(&rynas_tls::generate_ca("x", now).unwrap()), Some(&agent_leaf(&ca, ID, now)), None);
        // the address (127.0.0.1) is in no certificate the agent checks: the identity is
        get(&ca_client(&ca, port)).unwrap();
        // the next CA announced alongside: still fine
        let next = rynas_tls::generate_ca("nas", now).unwrap();
        let t = check(&trust_of(&[&next, &ca], port)).unwrap();
        get(&Client::with_trust(endpoints(&url(port), &[]).unwrap(), t.trust())).unwrap();
    }

    #[test]
    fn another_authority_is_refused() {
        let now = rynas_tls::now();
        let ca = rynas_tls::generate_ca("nas", now).unwrap();
        let other = rynas_tls::generate_ca("nas", now).unwrap();
        // another NAS (same identity even): its own CA issued its leaf
        let port = nas(&ui_leaf(&other), Some(&agent_leaf(&other, ID, now)), None);
        let e = get(&ca_client(&ca, port)).unwrap_err();
        assert!(e.contains("not from the certificate authority"), "{e}");
    }

    #[test]
    fn an_expired_leaf_is_refused() {
        let now = rynas_tls::now();
        let ca = rynas_tls::generate_ca("nas", now - 600 * DAY).unwrap();
        let port = nas(&ui_leaf(&ca), Some(&agent_leaf(&ca, ID, now - 500 * DAY)), None);
        let e = get(&ca_client(&ca, port)).unwrap_err();
        assert!(e.contains("validity period"), "{e}");
    }

    #[test]
    fn another_identity_under_the_same_authority_is_refused() {
        let now = rynas_tls::now();
        let ca = rynas_tls::generate_ca("nas", now).unwrap();
        let port = nas(&ui_leaf(&ca), Some(&agent_leaf(&ca, "ffffffffffffffffffffffffffffffff", now)), None);
        let e = get(&ca_client(&ca, port)).unwrap_err();
        assert!(e.contains("not the one of the NAS"), "{e}");
        // a NAS without the agents' certificate (older rynasd): the interface's leaf of the
        // same CA does not carry the identity either
        let port = nas(&ui_leaf(&ca), None, None);
        let e = get(&ca_client(&ca, port)).unwrap_err();
        assert!(e.contains("not the one of the NAS"), "{e}");
    }

    #[test]
    fn a_dead_address_is_skipped_for_the_next_one() {
        let now = rynas_tls::now();
        let ca = rynas_tls::generate_ca("nas", now).unwrap();
        let port = nas(&ui_leaf(&ca), Some(&agent_leaf(&ca, ID, now)), None);
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_url = url(dead.local_addr().unwrap().port());
        drop(dead);
        let t = check(&trust_of(&[&ca], port)).unwrap();
        let c = Client::with_trust(endpoints(&dead_url, &[url(port)]).unwrap(), t.trust());
        get(&c).unwrap();
        assert_eq!(c.ep().url(), url(port));
        // a machine at the stored address that is not the NAS: the next address too
        let other = rynas_tls::generate_ca("nas", now).unwrap();
        let wrong = nas(&ui_leaf(&other), Some(&agent_leaf(&other, ID, now)), None);
        let c = Client::with_trust(endpoints(&url(wrong), &[url(port)]).unwrap(), t.trust());
        get(&c).unwrap();
        assert_eq!(c.ep().url(), url(port));
    }

    fn pinned_state(ui: &PemPair, port: u16) -> State {
        let leaf = rynas_tls::chain_der(&ui.chain).unwrap().remove(0);
        State { url: url(port), fingerprint: crate::http::fingerprint(leaf.as_ref()), token: "t".into(), ..Default::default() }
    }

    #[test]
    fn a_certificate_pin_is_upgraded_to_the_authority() {
        let now = rynas_tls::now();
        let ca = rynas_tls::generate_ca("nas", now).unwrap();
        let ui = ui_leaf(&ca);
        let agent = agent_leaf(&ca, ID, now);
        let port = nas(&ui, Some(&agent), Some(&[&ca]));
        let mut st = pinned_state(&ui, port);
        // the pinned client works as before
        get(&client(&st).unwrap()).unwrap();
        assert_eq!(refresh(&mut st).unwrap(), Refreshed::Upgraded);
        assert!(st.fingerprint.is_empty());
        assert_eq!((st.nas_id.as_str(), st.cas.len()), (ID, 1));
        assert_eq!(st.addresses, [url(port)]);
        assert!(st.trust_checked_ms.is_some());
        // from now on the CA client, and the next refresh changes nothing
        get(&client(&st).unwrap()).unwrap();
        assert_eq!(refresh(&mut st).unwrap(), Refreshed::Same);
    }


    #[test]
    fn the_upgrade_needs_a_working_authority_and_a_nas_that_has_one() {
        let now = rynas_tls::now();
        let ca = rynas_tls::generate_ca("nas", now).unwrap();
        let ui = ui_leaf(&ca);
        // an older NAS: no trust (404), the pin stays
        let port = nas(&ui, None, None);
        let mut st = pinned_state(&ui, port);
        let pin = st.fingerprint.clone();
        assert_eq!(refresh(&mut st).unwrap(), Refreshed::Unsupported);
        assert_eq!((st.fingerprint.as_str(), st.nas_id.as_str()), (pin.as_str(), ""));
        // a CA announced that did not issue the certificate served for the identity: kept out
        let other = rynas_tls::generate_ca("nas", now).unwrap();
        let port = nas(&ui, Some(&agent_leaf(&other, ID, now)), Some(&[&ca]));
        let mut st = pinned_state(&ui, port);
        assert!(refresh(&mut st).is_err());
        assert_eq!((st.fingerprint.as_str(), st.nas_id.as_str()), (pin.as_str(), ""));
        // a pin that does not match: nothing is read at all
        let mut st = pinned_state(&ui_leaf(&ca), port);
        assert!(refresh(&mut st).unwrap_err().contains("changed"));
    }

    #[test]
    fn checks_of_what_the_nas_sends() {
        let ca = rynas_tls::generate_ca("nas", rynas_tls::now()).unwrap();
        let ok = trust_of(&[&ca], 8443);
        assert!(check(&ok).is_ok());
        assert!(check(&AgentTrust { nas_id: "NOTHEX".into(), ..ok.clone() }).is_err());
        assert!(check(&AgentTrust { cas: vec![], ..ok.clone() }).is_err());
        assert!(check(&AgentTrust { cas: vec![ca.chain.clone(); 5], ..ok.clone() }).is_err());
        assert!(check(&AgentTrust { cas: vec!["garbage".into()], ..ok.clone() }).is_err());
        assert!(check(&AgentTrust { cas: vec![format!("{}{}", ca.chain, ca.chain)], ..ok.clone() }).is_err());
        let t = check(&AgentTrust {
            addresses: vec!["http://plain:80".into(), "https://a:1/x".into(), "https://nas:8443".into(), "https://nas:8443/".into()],
            ..ok
        })
        .unwrap();
        assert_eq!(t.addresses, ["https://nas:8443"]);
    }
}
