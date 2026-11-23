//! Dynamic DNS names ([`WebAccessConfig::ddns`], `docs/research/web-access.md` §8–§9).
//!
//! Every [`EVERY`] (and on request) each name is checked: the addresses it should have (the
//! public IPv4 address of the network, asked of a few services over IPv4; the NAS's global
//! IPv6 address) against what the public resolvers answer. Then:
//! - `update`: a record set that differs is written through the provider of the name's
//!   credential (the same clients as the LAN records of the own domains). A provider that
//!   can be read is asked first (written already: the resolvers follow within the old TTL);
//!   one without a read API (DuckDNS, dyndns2) is not sent the same address again within a
//!   day, so the service never sees repeated updates;
//! - `check`: another device (the router) writes the name; a difference is only reported.
//!
//! A failure or a difference raises a notification, resolved once the name is right again.
//! What was written last is kept in `<state>/web-access/ddns.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rynas_api_types::notify::{EventState, NewEvent, NotifyCategory, NotifySeverity};
use rynas_api_types::web_access::{DdnsMode, DdnsName, DdnsState, DdnsStatus, DdnsView, WebAccessConfig};
use rynas_dns::wire::Answer;
use rynas_dns::{DnsApi, Rtype, Zone};
use rynas_notify::event::{code, field, firing, resolved};
use serde::{Deserialize, Serialize};

use super::{WebAccessSvc, domains};

/// How often every name is checked.
pub const EVERY: Duration = Duration::from_secs(300);
/// A provider without a read API gets the same address again only after this long (dynamic
/// DNS services block clients that repeat updates).
const REWRITE_AFTER_MS: i64 = 24 * 3600 * 1000;
/// After a write the resolvers may answer the old address for the old TTL: the same address
/// is not written again before this.
const SETTLE_MS: i64 = 15 * 60 * 1000;
/// The lowest TTL written.
const MIN_TTL: u32 = 60;

const SRC: &str = "web-access";

/// What `<state>/web-access/ddns.json` holds: the last write of each record set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Saved {
    /// `<name>|<A|AAAA>` → (address, ms).
    #[serde(default)]
    pub written: BTreeMap<String, (String, i64)>,
}

/// What the service remembers between checks.
#[derive(Default)]
pub struct Mem {
    pub view: DdnsView,
    /// Condition keys of the notifications raised and not resolved.
    pub open: BTreeSet<String>,
    pub saved: Option<Saved>,
}

pub fn saved_file(state_dir: &Path) -> PathBuf {
    state_dir.join("web-access").join("ddns.json")
}

fn load(state_dir: &Path) -> Saved {
    std::fs::read_to_string(saved_file(state_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(state_dir: &Path, s: &Saved) {
    let p = saved_file(state_dir);
    let Some(dir) = p.parent() else { return };
    let tmp = p.with_extension("json.tmp");
    let r = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap_or_default()))
        .and_then(|_| std::fs::rename(&tmp, &p));
    if let Err(e) = r {
        tracing::warn!("dynamic DNS: {}: {e}", p.display());
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A global unicast IPv6 address (2000::/3): the only kind a public name may carry.
pub fn global_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xe000) == 0x2000
}

fn key(name: &str, t: Rtype) -> String {
    format!("{name}|{t}")
}

/// The addresses public resolvers answer for `name`/`t`: the first resolver that answers
/// (NXDOMAIN and "no data" count as an empty answer).
fn published(svc: &WebAccessSvc, name: &str, t: Rtype) -> Result<Vec<String>, String> {
    let mut last = String::from("no public resolver answered");
    for r in domains::PUBLIC_RESOLVERS {
        let Ok(server) = r.parse::<SocketAddr>() else { continue };
        match svc.probe.query(server, name, t) {
            Ok(Answer::Addrs(a)) => {
                let mut v: Vec<String> = a.iter().map(IpAddr::to_string).collect();
                v.sort();
                v.dedup();
                return Ok(v);
            }
            Ok(Answer::NoData | Answer::NxDomain) => return Ok(Vec::new()),
            Ok(other) => last = format!("{server}: {}", other.describe()),
            Err(e) => last = format!("{server}: {e}"),
        }
    }
    Err(last)
}

/// The provider client of a name's credential.
fn client_of(svc: &WebAccessSvc, cfg: &WebAccessConfig, n: &DdnsName) -> Result<(Box<dyn DnsApi>, Zone), String> {
    let cid = n.credential.as_deref().ok_or("no credential")?;
    let cr = cfg.credentials.iter().find(|c| c.id == cid).ok_or_else(|| format!("no credential {cid}"))?;
    let creds = domains::open_creds(cr, svc.sb())?;
    let api = (svc.dns)(cr.provider, &creds).map_err(|e| e.to_string());
    domains::wipe(creds);
    let api = api?.ok_or_else(|| format!("rynas cannot write names through {}", cr.provider.as_str()))?;
    let zone = api.zone(&n.name).map_err(|e| e.to_string())?;
    Ok((api, zone))
}

/// What one name needs: per record type, the wanted address (`None`: that family is wanted
/// but there is no address).
fn wanted(n: &DdnsName, v4: Option<Ipv4Addr>, v6: Option<Ipv6Addr>) -> Vec<(Rtype, Option<String>)> {
    let mut w = Vec::new();
    if n.ipv4 {
        w.push((Rtype::A, v4.map(|a| a.to_string())));
    }
    if n.ipv6 {
        w.push((Rtype::Aaaa, v6.map(|a| a.to_string())));
    }
    w
}

impl WebAccessSvc {
    /// The last check of the dynamic DNS names, with the names configured since as pending.
    pub fn ddns_view(&self) -> DdnsView {
        let cfg = self.applied();
        let mem = self.ddns_mem.lock().unwrap_or_else(|e| e.into_inner());
        let mut v = mem.view.clone();
        v.names = cfg
            .ddns
            .iter()
            .map(|n| {
                mem.view.names.iter().find(|s| s.id == n.id && s.name == n.name).cloned().unwrap_or_else(|| DdnsStatus {
                    id: n.id.clone(),
                    name: n.name.clone(),
                    mode: n.mode,
                    state: if n.enabled { DdnsState::Pending } else { DdnsState::Disabled },
                    ..Default::default()
                })
            })
            .collect();
        v
    }

    /// Checks every name now (blocking: provider and resolver calls). `force`: a write is not
    /// held back by the waits after an earlier one.
    pub fn ddns_check(&self, force: bool) -> DdnsView {
        let _one = self.ddns_run.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = self.applied();
        let now = now_ms();
        let mut saved = {
            let mut m = self.ddns_mem.lock().unwrap_or_else(|e| e.into_inner());
            m.saved.get_or_insert_with(|| load(&self.state_dir)).clone()
        };
        let active: Vec<&DdnsName> = cfg.ddns.iter().filter(|n| n.enabled).collect();
        let (v4, v4_err) = if active.iter().any(|n| n.ipv4) {
            match (self.public_ipv4)() {
                Ok(ip) => (Some(ip), None),
                Err(e) => (None, Some(e)),
            }
        } else {
            (None, None)
        };
        let v6 = if active.iter().any(|n| n.ipv6) { self.facts().primary_v6.filter(|a| global_v6(*a)) } else { None };
        let mut names = Vec::new();
        let mut events = Vec::new();
        for n in &cfg.ddns {
            let mut st = DdnsStatus {
                id: n.id.clone(),
                name: n.name.clone(),
                mode: n.mode,
                checked_ms: now,
                updated_ms: [Rtype::A, Rtype::Aaaa].iter().filter_map(|t| saved.written.get(&key(&n.name, *t)).map(|w| w.1)).max(),
                ..Default::default()
            };
            if !n.enabled {
                st.state = DdnsState::Disabled;
                events.push(ok_event(n));
                names.push(st);
                continue;
            }
            self.check_one(&cfg, n, &wanted(n, v4, v6), &mut st, &mut saved, now, force, v4_err.as_deref());
            events.push(match st.state {
                DdnsState::Failed => failed_event(n, st.error.as_deref().unwrap_or("")),
                DdnsState::Mismatch => mismatch_event(n, &st),
                DdnsState::Ok => ok_event(n),
                // in between: what was raised stays until the name settles
                _ => {
                    names.push(st);
                    continue;
                }
            });
            names.push(st);
        }
        save(&self.state_dir, &saved);
        let view = DdnsView {
            names,
            public_ipv4: v4.map(|a| a.to_string()),
            public_ipv4_error: v4_err,
            ipv6: v6.map(|a| a.to_string()),
            checked_ms: now,
        };
        let mut m = self.ddns_mem.lock().unwrap_or_else(|e| e.into_inner());
        // names removed from the configuration: their notifications end
        let gone: Vec<String> = m.open.iter().filter(|k| !cfg.ddns.iter().any(|n| cond_key(n) == **k)).cloned().collect();
        for k in gone {
            m.open.remove(&k);
            events.push(resolved(NotifyCategory::System, SRC, k, "Dynamic DNS name removed".to_string()));
        }
        for e in events {
            let firing = e.state == EventState::Firing;
            let k = e.key.clone().unwrap_or_default();
            // resolved events only for conditions raised before; firing ones again each time
            // (the engine deduplicates)
            if firing {
                m.open.insert(k);
            } else if !m.open.remove(&k) {
                continue;
            }
            if let Some(nf) = self.notifier.get() {
                nf.emit(e);
            }
        }
        m.saved = Some(saved);
        m.view = view.clone();
        view
    }

    #[allow(clippy::too_many_arguments)]
    fn check_one(
        &self,
        cfg: &WebAccessConfig,
        n: &DdnsName,
        want: &[(Rtype, Option<String>)],
        st: &mut DdnsStatus,
        saved: &mut Saved,
        now: i64,
        force: bool,
        v4_err: Option<&str>,
    ) {
        st.wanted = want.iter().filter_map(|(_, a)| a.clone()).collect();
        let missing: Vec<&str> = want
            .iter()
            .filter(|(_, a)| a.is_none())
            .map(|(t, _)| match t {
                Rtype::A => v4_err.unwrap_or("the public IPv4 address was not found"),
                Rtype::Aaaa => "the NAS has no global IPv6 address",
            })
            .collect();
        let mut diff = Vec::new();
        for (t, a) in want {
            let Some(a) = a else { continue };
            match published(self, &n.name, *t) {
                Ok(p) => {
                    if p != [a.clone()] {
                        diff.push((*t, a.clone()));
                    }
                    st.published.extend(p);
                }
                Err(e) => {
                    st.state = DdnsState::Failed;
                    st.error = Some(format!("the public resolvers could not be asked: {e}"));
                    return;
                }
            }
        }
        if st.wanted.is_empty() {
            st.state = DdnsState::NoAddress;
            st.error = Some(missing.join("; "));
            return;
        }
        if diff.is_empty() {
            st.state = DdnsState::Ok;
            st.error = (!missing.is_empty()).then(|| missing.join("; "));
            return;
        }
        if n.mode == DdnsMode::Check {
            st.state = DdnsState::Mismatch;
            st.error = Some(format!(
                "the public resolvers answer {} instead of {}",
                if st.published.is_empty() { "nothing".to_string() } else { st.published.join(", ") },
                st.wanted.join(", ")
            ));
            return;
        }
        let (api, zone) = match client_of(self, cfg, n) {
            Ok(x) => x,
            Err(e) => {
                st.state = DdnsState::Failed;
                st.error = Some(e);
                return;
            }
        };
        let ttl = if n.ttl == 0 { zone.min_ttl.max(MIN_TTL) } else { n.ttl.max(zone.min_ttl) };
        st.state = DdnsState::Updated;
        for (t, a) in diff {
            let k = key(&n.name, t);
            let last = saved.written.get(&k).filter(|w| w.0 == a).map(|w| now - w.1);
            if api.readable() {
                match api.get(&zone, &n.name, t) {
                    // written already: the resolvers follow within the old TTL
                    Ok(Some(cur)) if cur.values == [a.clone()] => continue,
                    Ok(_) => {}
                    Err(e) => {
                        st.state = DdnsState::Failed;
                        st.error = Some(e.to_string());
                        return;
                    }
                }
                if !force && last.is_some_and(|age| age < SETTLE_MS) {
                    continue;
                }
            } else if !force && last.is_some_and(|age| age < REWRITE_AFTER_MS) {
                continue;
            }
            match api.set(&zone, &n.name, t, std::slice::from_ref(&a), ttl) {
                Ok(()) => {
                    tracing::info!(target: "rynas::audit", "dynamic DNS: {} {t} = {a}", n.name);
                    saved.written.insert(k, (a, now));
                    st.updated_ms = Some(now);
                }
                Err(e) => {
                    st.state = DdnsState::Failed;
                    st.error = Some(e.to_string());
                    return;
                }
            }
        }
    }
}

/// Checks the names every [`EVERY`].
pub fn spawn_watch(svc: Arc<WebAccessSvc>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            let s = svc.clone();
            let busy = !s.applied().ddns.is_empty() || !s.ddns_mem.lock().unwrap_or_else(|e| e.into_inner()).open.is_empty();
            if busy && let Err(e) = tokio::task::spawn_blocking(move || s.ddns_check(false)).await {
                tracing::error!("dynamic DNS: {e}");
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

// ------------------------------------------------------------------ notifications

fn cond_key(n: &DdnsName) -> String {
    format!("web-access:ddns:{}", n.id)
}

fn failed_event(n: &DdnsName, err: &str) -> NewEvent {
    let ev = firing(NotifyCategory::System, NotifySeverity::Warning, SRC, cond_key(n), format!("Dynamic DNS name {} could not be updated", n.name));
    let mut ev = code(ev, "web_access.ddns_failed");
    ev.body = format!("{err}. Check the name on Network › Web access › Domains.");
    let ev = field(ev, "name", n.name.clone());
    field(ev, "error", err)
}

fn mismatch_event(n: &DdnsName, st: &DdnsStatus) -> NewEvent {
    let ev = firing(NotifyCategory::System, NotifySeverity::Warning, SRC, cond_key(n), format!("Dynamic DNS name {} points elsewhere", n.name));
    let mut ev = code(ev, "web_access.ddns_mismatch");
    let published = if st.published.is_empty() { "nothing".to_string() } else { st.published.join(", ") };
    ev.body = format!("The public resolvers answer {published}, the public address is {}. The device that updates the name may have stopped.", st.wanted.join(", "));
    let ev = field(ev, "name", n.name.clone());
    let ev = field(ev, "published", published);
    field(ev, "wanted", st.wanted.join(", "))
}

fn ok_event(n: &DdnsName) -> NewEvent {
    let ev = resolved(NotifyCategory::System, SRC, cond_key(n), format!("Dynamic DNS name {} is right again", n.name));
    let ev = code(ev, "web_access.ddns_ok");
    field(ev, "name", n.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_addresses_and_wanted() {
        assert!(global_v6("2001:db8::1".parse().unwrap()));
        assert!(!global_v6("fd00::1".parse().unwrap()));
        assert!(!global_v6("fe80::1".parse().unwrap()));
        let n = DdnsName { ipv4: true, ipv6: true, ..Default::default() };
        let w = wanted(&n, Some("203.0.113.5".parse().unwrap()), None);
        assert_eq!(w, vec![(Rtype::A, Some("203.0.113.5".to_string())), (Rtype::Aaaa, None)]);
    }
}
