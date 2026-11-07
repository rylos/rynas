//! Record planning: which record sets a domain wants, what the provider has, what a sync
//! changes. A plan only reads; [`apply`] writes the changes of a plan.

use std::net::{Ipv4Addr, Ipv6Addr};

use rynas_api_types::web_access::{DnsAction, DnsChange};

use crate::providers::sorted;
use crate::{DEFAULT_TTL, DnsApi, DnsError, Rtype, Zone};

/// One record set a domain wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub name: String,
    pub rtype: Rtype,
    pub values: Vec<String>,
}

/// The LAN record sets of a domain:
/// - providers that store wildcards: `*.<domain>` and `<domain>`;
/// - providers whose one record covers every name below it (DuckDNS): `<domain>`;
/// - otherwise `<domain>` and each of `names` (the routes' `<label>.<domain>`).
///
/// A for `v4`, AAAA for `v6`; nothing without an address.
pub fn wanted(
    domain: &str,
    wildcard_records: bool,
    covers_subdomains: bool,
    names: &[String],
    v4: Option<Ipv4Addr>,
    v6: Option<Ipv6Addr>,
) -> Vec<Wanted> {
    let mut owners: Vec<String> = Vec::new();
    if covers_subdomains {
        owners.push(domain.to_string());
    } else if wildcard_records {
        owners.push(format!("*.{domain}"));
        owners.push(domain.to_string());
    } else {
        owners.push(domain.to_string());
        for n in names {
            if !owners.contains(n) {
                owners.push(n.clone());
            }
        }
    }
    let mut out = Vec::new();
    for o in owners {
        if let Some(a) = v4 {
            out.push(Wanted {
                name: o.clone(),
                rtype: Rtype::A,
                values: vec![a.to_string()],
            });
        }
        if let Some(a) = v6 {
            out.push(Wanted {
                name: o.clone(),
                rtype: Rtype::Aaaa,
                values: vec![a.to_string()],
            });
        }
    }
    out
}

/// The TTL rynas writes in `zone`.
pub fn ttl_for(zone: &Zone) -> u32 {
    DEFAULT_TTL.max(zone.min_ttl)
}

fn change(domain: &str, name: &str, rtype: Rtype, action: DnsAction) -> DnsChange {
    DnsChange {
        domain: domain.into(),
        name: name.into(),
        rtype: rtype.as_str().into(),
        action,
        ..Default::default()
    }
}

/// Records the administrator adds by hand (no client for the provider, CNAME delegation).
pub fn manual(domain: &str, wanted: &[Wanted], detail: &str) -> Vec<DnsChange> {
    wanted
        .iter()
        .map(|w| DnsChange {
            wanted: w.values.clone(),
            ttl: DEFAULT_TTL,
            detail: detail.into(),
            ..change(domain, &w.name, w.rtype, DnsAction::Manual)
        })
        .collect()
}

/// Every wanted and owned (not wanted) record as `unknown` with `detail` (the zone could
/// not be read, or the provider is not asked).
pub fn unknown(domain: &str, wanted: &[Wanted], owned: &[Wanted], detail: &str) -> Vec<DnsChange> {
    let mut v: Vec<DnsChange> = wanted
        .iter()
        .map(|w| DnsChange {
            wanted: w.values.clone(),
            detail: detail.into(),
            ..change(domain, &w.name, w.rtype, DnsAction::Unknown)
        })
        .collect();
    v.extend(
        owned
            .iter()
            .filter(|o| !wanted.iter().any(|w| w.name == o.name && w.rtype == o.rtype))
            .map(|o| DnsChange {
                detail: detail.into(),
                ..change(domain, &o.name, o.rtype, DnsAction::Unknown)
            }),
    );
    v
}

/// What a provider that cannot be read (DuckDNS) does to its one record, said plainly.
pub const UNREADABLE_WRITE: &str = "the provider cannot be read: its one record is set to the \
     NAS's LAN address on every sync, replacing any other address it held (a public one too)";

/// What a sync changes. Reads only.
///
/// `owned` are the sets rynas wrote before, with the values it wrote (empty: not known).
/// Records rynas did not write are never changed without consent:
/// - a wanted set that is missing is created; one with the wanted values is kept (and not
///   adopted: the caller only keeps owning what it owned);
/// - a wanted set with other values is updated when rynas wrote it and it still holds what
///   rynas wrote, or when its name is in `take_over`; otherwise it is a `conflict`;
/// - an owned set no longer wanted is deleted only when it still holds what rynas wrote;
///   otherwise it is a `conflict` (left in place, no longer managed).
pub fn plan(
    domain: &str,
    api: &dyn DnsApi,
    wanted: &[Wanted],
    owned: &[Wanted],
    take_over: &[String],
) -> Result<(Zone, Vec<DnsChange>), DnsError> {
    let probe = wanted
        .first()
        .map(|w| w.name.clone())
        .or_else(|| owned.first().map(|s| s.name.clone()))
        .unwrap_or_else(|| domain.to_string());
    let zone = api.zone(&probe)?;
    let ttl = ttl_for(&zone);
    let mine = |name: &str, rtype: Rtype| {
        owned
            .iter()
            .find(|o| o.name == name && o.rtype == rtype)
            .filter(|o| !o.values.is_empty())
    };
    let mut out = Vec::new();
    for w in wanted {
        let mut c = DnsChange {
            wanted: w.values.clone(),
            ttl,
            ..change(domain, &w.name, w.rtype, DnsAction::Create)
        };
        if !api.readable() {
            c.action = DnsAction::Update;
            c.detail = UNREADABLE_WRITE.into();
        } else {
            match api.get(&zone, &w.name, w.rtype) {
                Ok(None) => {}
                Ok(Some(cur)) => {
                    let cur_s = sorted(&cur.values);
                    if cur_s == sorted(&w.values) {
                        c.action = DnsAction::Keep;
                    } else if mine(&w.name, w.rtype).is_some_and(|o| sorted(&o.values) == cur_s)
                    {
                        c.action = DnsAction::Update;
                    } else if take_over.iter().any(|t| t == &w.name) {
                        c.action = DnsAction::Update;
                        c.detail = "taken over: the existing values are replaced".into();
                    } else {
                        c.action = DnsAction::Conflict;
                        c.detail = if owned.iter().any(|o| o.name == w.name && o.rtype == w.rtype)
                        {
                            "changed outside rynas since rynas wrote it: left as it is until \
                             the name is taken over"
                        } else {
                            "exists with other values and was not written by rynas: left as \
                             it is until the name is taken over"
                        }
                        .into();
                    }
                    c.current = cur.values;
                }
                Err(e) => {
                    c.action = DnsAction::Unknown;
                    c.detail = e.to_string();
                }
            }
        }
        out.push(c);
    }
    for o in owned {
        if wanted.iter().any(|w| w.name == o.name && w.rtype == o.rtype) {
            continue;
        }
        let mut c = change(domain, &o.name, o.rtype, DnsAction::Delete);
        if !api.readable() {
            c.action = DnsAction::Conflict;
            c.detail = "the provider cannot be read: left as it is and no longer managed \
                        (clear it by hand if needed)"
                .into();
        } else {
            match api.get(&zone, &o.name, o.rtype) {
                Ok(None) => continue,
                Ok(Some(cur)) => {
                    if o.values.is_empty() || sorted(&cur.values) != sorted(&o.values) {
                        c.action = DnsAction::Conflict;
                        c.detail = "changed outside rynas since rynas wrote it: left in place \
                                    and no longer managed"
                            .into();
                    }
                    c.current = cur.values;
                }
                Err(e) => {
                    c.action = DnsAction::Unknown;
                    c.detail = e.to_string();
                }
            }
        }
        out.push(c);
    }
    Ok((zone, out))
}

/// Writes the `create`/`update`/`delete` changes; a failed one keeps its action and gets the
/// error in `detail`. Returns whether every change went through.
pub fn apply(api: &dyn DnsApi, zone: &Zone, changes: &mut [DnsChange]) -> bool {
    let mut ok = true;
    for c in changes.iter_mut() {
        let Some(rtype) = Rtype::parse(&c.rtype) else {
            continue;
        };
        let r = match c.action {
            DnsAction::Create | DnsAction::Update => {
                api.set(zone, &c.name, rtype, &c.wanted, c.ttl.max(ttl_for(zone)))
            }
            DnsAction::Delete => api.delete(zone, &c.name, rtype),
            DnsAction::Unknown => {
                ok = false;
                continue;
            }
            DnsAction::Keep | DnsAction::Manual | DnsAction::Conflict => continue,
        };
        if let Err(e) = r {
            ok = false;
            c.detail = format!("failed: {e}");
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RecordSet;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// A provider in memory.
    #[derive(Default)]
    struct Mem {
        sets: Mutex<BTreeMap<(String, Rtype), Vec<String>>>,
        calls: Mutex<Vec<String>>,
        fail_set: bool,
    }

    impl DnsApi for Mem {
        fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
            if fqdn.ends_with("example.com") {
                Ok(Zone {
                    id: "z1".into(),
                    name: "example.com".into(),
                    min_ttl: 3600,
                })
            } else {
                Err(DnsError::NoZone(fqdn.into()))
            }
        }
        fn get(&self, _z: &Zone, n: &str, t: Rtype) -> Result<Option<RecordSet>, DnsError> {
            Ok(self
                .sets
                .lock()
                .unwrap()
                .get(&(n.into(), t))
                .map(|v| RecordSet {
                    values: v.clone(),
                    ttl: 300,
                }))
        }
        fn set(
            &self,
            _z: &Zone,
            n: &str,
            t: Rtype,
            v: &[String],
            ttl: u32,
        ) -> Result<(), DnsError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set {n} {t} {v:?} {ttl}"));
            if self.fail_set {
                return Err(DnsError::Api("quota".into()));
            }
            self.sets.lock().unwrap().insert((n.into(), t), v.to_vec());
            Ok(())
        }
        fn delete(&self, _z: &Zone, n: &str, t: Rtype) -> Result<(), DnsError> {
            self.calls.lock().unwrap().push(format!("delete {n} {t}"));
            self.sets.lock().unwrap().remove(&(n.into(), t));
            Ok(())
        }
    }

    fn v4() -> Option<Ipv4Addr> {
        Some("192.0.2.10".parse().unwrap())
    }

    #[test]
    fn wanted_sets() {
        let w = wanted("nas.example.com", true, false, &[], v4(), None);
        let names: Vec<_> = w
            .iter()
            .map(|w| format!("{} {}", w.name, w.rtype))
            .collect();
        assert_eq!(names, ["*.nas.example.com A", "nas.example.com A"]);
        let w = wanted(
            "x.duckdns.org",
            false,
            true,
            &["immich.x.duckdns.org".into()],
            v4(),
            Some("fd00::10".parse().unwrap()),
        );
        assert_eq!(w.len(), 2);
        assert!(w.iter().all(|w| w.name == "x.duckdns.org"));
        let w = wanted(
            "nas.example.com",
            false,
            false,
            &["immich.nas.example.com".into()],
            v4(),
            None,
        );
        assert_eq!(w[1].name, "immich.nas.example.com");
        assert!(wanted("nas.example.com", true, false, &[], None, None).is_empty());
    }

    #[test]
    fn plan_then_apply() {
        let api = Mem::default();
        api.sets.lock().unwrap().insert(
            ("nas.example.com".into(), Rtype::A),
            vec!["198.51.100.1".into()],
        );
        api.sets.lock().unwrap().insert(
            ("old.nas.example.com".into(), Rtype::A),
            vec!["192.0.2.10".into()],
        );
        let w = wanted("nas.example.com", true, false, &[], v4(), None);
        let set = |n: &str, v: &str| Wanted {
            name: n.into(),
            rtype: Rtype::A,
            values: vec![v.into()],
        };
        // rynas wrote nas.example.com = 198.51.100.1 earlier (the address changed since)
        let owned = vec![
            set("nas.example.com", "198.51.100.1"),
            set("old.nas.example.com", "192.0.2.10"),
            set("gone.nas.example.com", "192.0.2.10"),
        ];
        let (zone, mut changes) = plan("nas.example.com", &api, &w, &owned, &[]).unwrap();
        let acts: Vec<_> = changes
            .iter()
            .map(|c| (c.name.as_str(), c.action))
            .collect();
        assert_eq!(
            acts,
            [
                ("*.nas.example.com", DnsAction::Create),
                ("nas.example.com", DnsAction::Update),
                ("old.nas.example.com", DnsAction::Delete),
            ],
            "a stale set that is not there is left out"
        );
        assert_eq!(changes[1].current, ["198.51.100.1"]);
        assert_eq!(changes[0].ttl, 3600, "raised to the zone's minimum");
        assert!(api.calls.lock().unwrap().is_empty(), "a plan never writes");
        assert!(apply(&api, &zone, &mut changes));
        assert_eq!(api.calls.lock().unwrap().len(), 3);
        // a second plan keeps everything
        let (_, again) = plan("nas.example.com", &api, &w, &[], &[]).unwrap();
        assert!(again.iter().all(|c| c.action == DnsAction::Keep));
    }

    /// Review 2026-11-07 H2: records rynas did not write are never changed or deleted
    /// without consent, and a set edited after rynas wrote it is not deleted.
    #[test]
    fn foreign_records_are_conflicts() {
        let api = Mem::default();
        let put = |n: &str, v: &str| {
            api.sets
                .lock()
                .unwrap()
                .insert((n.into(), Rtype::A), vec![v.into()]);
        };
        put("nas.example.com", "203.0.113.5");
        let w = wanted("nas.example.com", true, false, &[], v4(), None);
        let (zone, mut changes) = plan("nas.example.com", &api, &w, &[], &[]).unwrap();
        assert_eq!(changes[1].action, DnsAction::Conflict);
        assert_eq!(changes[1].current, ["203.0.113.5"]);
        assert!(apply(&api, &zone, &mut changes));
        assert_eq!(
            api.calls.lock().unwrap().as_slice(),
            ["set *.nas.example.com A [\"192.0.2.10\"] 3600"],
            "the conflict is left alone"
        );
        // taken over: replaced
        let take = vec!["nas.example.com".to_string()];
        let (_, c2) = plan("nas.example.com", &api, &w, &[], &take).unwrap();
        assert_eq!(c2[1].action, DnsAction::Update);
        // a set rynas wrote, edited by hand since: neither updated nor deleted
        put("old.nas.example.com", "198.51.100.7");
        let owned = vec![Wanted {
            name: "old.nas.example.com".into(),
            rtype: Rtype::A,
            values: vec!["192.0.2.10".into()],
        }];
        let (_, c3) = plan("nas.example.com", &api, &[], &owned, &[]).unwrap();
        assert_eq!(c3[0].action, DnsAction::Conflict);
        let (_, c4) = plan("nas.example.com", &api, &w[..1], &owned, &[]).unwrap();
        assert_eq!(c4[1].action, DnsAction::Conflict, "{c4:?}");
        // owned without known values (older state file): never deleted
        let legacy = vec![Wanted {
            values: Vec::new(),
            ..owned[0].clone()
        }];
        let (_, c5) = plan("nas.example.com", &api, &[], &legacy, &[]).unwrap();
        assert_eq!(c5[0].action, DnsAction::Conflict);
    }

    #[test]
    fn failures_are_reported() {
        let api = Mem {
            fail_set: true,
            ..Default::default()
        };
        let w = wanted("nas.example.com", true, false, &[], v4(), None);
        let (zone, mut changes) = plan("nas.example.com", &api, &w, &[], &[]).unwrap();
        assert!(!apply(&api, &zone, &mut changes));
        assert!(
            changes[0]
                .detail
                .starts_with("failed: provider error: quota")
        );
        assert!(matches!(
            plan(
                "nas.other.org",
                &api,
                &wanted("nas.other.org", true, false, &[], v4(), None),
                &[],
                &[]
            ),
            Err(DnsError::NoZone(_))
        ));
        let m = manual("nas.example.com", &w, "add by hand");
        assert!(m.iter().all(|c| c.action == DnsAction::Manual));
    }
}
