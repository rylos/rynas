//! A read cache in front of a provider client (review 2026-11-07, M6): every plan reads the
//! zone and each record set, and the interface asks for a plan on every review, while deSEC
//! and Porkbun throttle hard.
//!
//! - zone lookups are kept [`ZONE_TTL`] per (credential key, name);
//! - record reads are kept [`READ_TTL`], and only for plans: a sync that writes asks the
//!   provider fresh ([`Cached::fresh_reads`]), so its conflict checks never use an old value;
//! - every write (or delete) through the cache drops that credential's cached reads.
//!
//! The cache is small (a few entries per domain) and drops expired entries on each insert.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{DnsApi, DnsError, RecordSet, Rtype, Zone};

/// How long a zone lookup is kept.
pub const ZONE_TTL: Duration = Duration::from_secs(600);
/// How long a record read is kept (plans only).
pub const READ_TTL: Duration = Duration::from_secs(30);

type ReadKey = (String, String, String, Rtype);

#[derive(Default)]
struct Inner {
    zones: BTreeMap<(String, String), (Zone, Instant)>,
    reads: BTreeMap<ReadKey, (Option<RecordSet>, Instant)>,
}

/// The shared cache (one per rynasd).
#[derive(Default, Clone)]
pub struct Cache(Arc<Mutex<Inner>>);

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps `api`; `key` names the credential (and its values' version), so another token
    /// never sees this one's answers.
    pub fn wrap(&self, key: String, api: Box<dyn DnsApi>, fresh_reads: bool) -> Box<dyn DnsApi> {
        Box::new(Cached {
            cache: self.clone(),
            key,
            api,
            fresh_reads,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn forget(&self, key: &str) {
        self.lock().reads.retain(|k, _| k.0 != key);
    }

    /// Entries held (tests).
    pub fn len(&self) -> usize {
        let g = self.lock();
        g.zones.len() + g.reads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A provider client behind the cache.
pub struct Cached {
    cache: Cache,
    key: String,
    api: Box<dyn DnsApi>,
    /// Record reads skip the cache (a sync that writes); zone lookups still use it.
    pub fresh_reads: bool,
}

impl DnsApi for Cached {
    fn zone(&self, fqdn: &str) -> Result<Zone, DnsError> {
        let k = (self.key.clone(), fqdn.to_string());
        if let Some((z, at)) = self.cache.lock().zones.get(&k)
            && at.elapsed() < ZONE_TTL
        {
            return Ok(z.clone());
        }
        let z = self.api.zone(fqdn)?;
        let mut g = self.cache.lock();
        g.zones.retain(|_, (_, at)| at.elapsed() < ZONE_TTL);
        g.zones.insert(k, (z.clone(), Instant::now()));
        Ok(z)
    }

    fn get(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<Option<RecordSet>, DnsError> {
        let k = (self.key.clone(), zone.id.clone(), fqdn.to_string(), rtype);
        if !self.fresh_reads
            && let Some((v, at)) = self.cache.lock().reads.get(&k)
            && at.elapsed() < READ_TTL
        {
            return Ok(v.clone());
        }
        let v = self.api.get(zone, fqdn, rtype)?;
        let mut g = self.cache.lock();
        g.reads.retain(|_, (_, at)| at.elapsed() < READ_TTL);
        g.reads.insert(k, (v.clone(), Instant::now()));
        Ok(v)
    }

    fn set(
        &self,
        zone: &Zone,
        fqdn: &str,
        rtype: Rtype,
        values: &[String],
        ttl: u32,
    ) -> Result<(), DnsError> {
        let r = self.api.set(zone, fqdn, rtype, values, ttl);
        self.cache.forget(&self.key);
        r
    }

    fn delete(&self, zone: &Zone, fqdn: &str, rtype: Rtype) -> Result<(), DnsError> {
        let r = self.api.delete(zone, fqdn, rtype);
        self.cache.forget(&self.key);
        r
    }

    fn readable(&self) -> bool {
        self.api.readable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Count {
        zones: AtomicUsize,
        gets: AtomicUsize,
    }

    struct Api(Arc<Count>);

    impl DnsApi for Api {
        fn zone(&self, _: &str) -> Result<Zone, DnsError> {
            self.0.zones.fetch_add(1, Ordering::SeqCst);
            Ok(Zone {
                id: "z".into(),
                name: "example.com".into(),
                min_ttl: 60,
            })
        }
        fn get(&self, _: &Zone, _: &str, _: Rtype) -> Result<Option<RecordSet>, DnsError> {
            self.0.gets.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
        fn set(&self, _: &Zone, _: &str, _: Rtype, _: &[String], _: u32) -> Result<(), DnsError> {
            Ok(())
        }
        fn delete(&self, _: &Zone, _: &str, _: Rtype) -> Result<(), DnsError> {
            Ok(())
        }
    }

    #[test]
    fn plans_share_reads_syncs_read_fresh() {
        let n = Arc::new(Count::default());
        let cache = Cache::new();
        let plan = cache.wrap("cf#1".into(), Box::new(Api(n.clone())), false);
        let z = plan.zone("nas.example.com").unwrap();
        plan.zone("nas.example.com").unwrap();
        plan.get(&z, "nas.example.com", Rtype::A).unwrap();
        plan.get(&z, "nas.example.com", Rtype::A).unwrap();
        assert_eq!(n.zones.load(Ordering::SeqCst), 1);
        assert_eq!(n.gets.load(Ordering::SeqCst), 1);
        // a sync: zone cached, reads fresh
        let sync = cache.wrap("cf#1".into(), Box::new(Api(n.clone())), true);
        sync.zone("nas.example.com").unwrap();
        sync.get(&z, "nas.example.com", Rtype::A).unwrap();
        assert_eq!(n.zones.load(Ordering::SeqCst), 1);
        assert_eq!(n.gets.load(Ordering::SeqCst), 2);
        // a write drops the reads; another credential never shares
        sync.set(&z, "nas.example.com", Rtype::A, &[], 300).unwrap();
        plan.get(&z, "nas.example.com", Rtype::A).unwrap();
        assert_eq!(n.gets.load(Ordering::SeqCst), 3);
        let other = cache.wrap("cf#2".into(), Box::new(Api(n.clone())), false);
        other.zone("nas.example.com").unwrap();
        assert_eq!(n.zones.load(Ordering::SeqCst), 2);
    }
}
