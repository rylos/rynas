//! Failed-login throttling.
//!
//! Failures are counted per client IP (IPv6 per /64, one host usually owns the whole prefix)
//! and per user name. After a number of free attempts each further failure locks that key for
//! an exponentially growing delay (`base * 2^n`, capped). A success resets the user counter;
//! the IP counter only decays, so one valid account cannot be used to reset it. Counters are
//! forgotten some time after the last failure.
//!
//! Every failure is logged as `authentication failure for USER from IP` (target `rynas_auth`),
//! the shape fail2ban filters expect (see `packaging/fail2ban/`).

use std::collections::HashMap;
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{Clock, log_safe};

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Failures allowed per user name before delays start.
    pub free_per_user: u32,
    /// Failures allowed per client IP before delays start.
    pub free_per_ip: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// A key with no failure for this long starts from zero again.
    pub forget_after: Duration,
    /// Memory bound for each of the two tables.
    pub max_entries: usize,
    /// Log each failure in the fail2ban format.
    pub log_failures: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            free_per_user: 5,
            free_per_ip: 20,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(15 * 60),
            forget_after: Duration::from_secs(3600),
            max_entries: 10_000,
            log_failures: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    failures: u32,
    last_failure_ms: u64,
    locked_until_ms: u64,
}

struct Table<K> {
    free: u32,
    map: Mutex<HashMap<K, Entry>>,
}

impl<K: Hash + Eq + Clone> Table<K> {
    fn new(free: u32) -> Self {
        Self {
            free,
            map: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<K, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn retry_after(&self, key: &K, now: u64, cfg: &RateLimitConfig) -> Option<u64> {
        let map = self.lock();
        let e = map.get(key)?;
        if now >= e.last_failure_ms + cfg.forget_after.as_millis() as u64 {
            return None;
        }
        (e.locked_until_ms > now).then(|| e.locked_until_ms - now)
    }

    /// Returns the lock duration started by this failure, if any.
    fn fail(&self, key: &K, now: u64, cfg: &RateLimitConfig) -> Option<Duration> {
        let mut map = self.lock();
        if map.len() >= cfg.max_entries && !map.contains_key(key) {
            let forget = cfg.forget_after.as_millis() as u64;
            map.retain(|_, e| now < e.last_failure_ms + forget);
            if map.len() >= cfg.max_entries {
                // still full: drop the stalest entry (O(n), only under attack)
                if let Some(k) = map
                    .iter()
                    .min_by_key(|(_, e)| e.last_failure_ms)
                    .map(|(k, _)| k.clone())
                {
                    map.remove(&k);
                }
            }
        }
        let e = map.entry(key.clone()).or_default();
        if now >= e.last_failure_ms + cfg.forget_after.as_millis() as u64 {
            *e = Entry::default();
        }
        e.failures = e.failures.saturating_add(1);
        e.last_failure_ms = now;
        if e.failures <= self.free {
            return None;
        }
        let exp = (e.failures - self.free - 1).min(20);
        let delay = cfg
            .base_delay
            .saturating_mul(1u32 << exp)
            .min(cfg.max_delay);
        e.locked_until_ms = now + delay.as_millis() as u64;
        Some(delay)
    }

    fn reset(&self, key: &K) {
        self.lock().remove(key);
    }
}

pub struct LoginLimiter {
    cfg: RateLimitConfig,
    clock: Arc<dyn Clock>,
    users: Table<String>,
    ips: Table<IpAddr>,
}

/// IPv6 addresses are grouped by /64 (IPv4-mapped ones are unwrapped).
fn ip_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
    }
}

fn user_key(user: &str) -> String {
    user.to_lowercase()
}

impl LoginLimiter {
    pub fn new(cfg: RateLimitConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            users: Table::new(cfg.free_per_user),
            ips: Table::new(cfg.free_per_ip),
            cfg,
            clock,
        }
    }

    /// `Err(retry_after)` when the IP or the user is locked.
    pub fn check(&self, ip: IpAddr, user: &str) -> Result<(), Duration> {
        let now = self.clock.now_ms();
        let a = self.ips.retry_after(&ip_key(ip), now, &self.cfg);
        let b = self.users.retry_after(&user_key(user), now, &self.cfg);
        match a.max(b) {
            Some(ms) => Err(Duration::from_millis(ms)),
            None => Ok(()),
        }
    }

    pub fn record_failure(&self, ip: IpAddr, user: &str) {
        let now = self.clock.now_ms();
        if self.cfg.log_failures {
            tracing::warn!(
                target: "rynas_auth",
                "authentication failure for {} from {}",
                log_safe(user),
                ip
            );
        }
        let a = self.ips.fail(&ip_key(ip), now, &self.cfg);
        let b = self.users.fail(&user_key(user), now, &self.cfg);
        if let Some(d) = a {
            tracing::info!(target: "rynas_auth", "login from {ip} locked for {}s", d.as_secs());
        }
        if let Some(d) = b {
            tracing::info!(
                target: "rynas_auth",
                "login of {} locked for {}s",
                log_safe(user),
                d.as_secs()
            );
        }
    }

    pub fn record_success(&self, _ip: IpAddr, user: &str) {
        self.users.reset(&user_key(user));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManualClock;

    fn limiter(clock: Arc<ManualClock>) -> LoginLimiter {
        LoginLimiter::new(
            RateLimitConfig {
                free_per_user: 3,
                free_per_ip: 5,
                base_delay: Duration::from_secs(1),
                max_delay: Duration::from_secs(8),
                forget_after: Duration::from_secs(60),
                max_entries: 4,
                log_failures: false,
            },
            clock,
        )
    }

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1));

    #[test]
    fn user_backoff_grows_and_caps() {
        let clock = Arc::new(ManualClock::new(1_000_000));
        let l = limiter(clock.clone());
        for _ in 0..3 {
            assert!(l.check(IP, "bob").is_ok());
            l.record_failure(IP, "bob");
        }
        assert!(l.check(IP, "bob").is_ok(), "3 free attempts");
        l.record_failure(IP, "bob");
        assert_eq!(l.check(IP, "BOB"), Err(Duration::from_secs(1)));
        let mut expected = 1;
        for _ in 0..5 {
            clock.advance_ms(expected * 1000);
            assert!(l.check(IP, "bob").is_ok());
            l.record_failure(IP, "bob");
            expected = (expected * 2).min(8);
            // other users from another IP are not affected
            assert!(l.check("198.51.100.7".parse().unwrap(), "alice").is_ok());
            assert_eq!(l.check(IP, "bob"), Err(Duration::from_secs(expected)));
        }
        l.record_success(IP, "bob");
        assert!(l.check("198.51.100.7".parse().unwrap(), "bob").is_ok());
    }

    #[test]
    fn ip_counter_survives_success_and_decays() {
        let clock = Arc::new(ManualClock::new(1_000_000));
        let l = limiter(clock.clone());
        for i in 0..6 {
            l.record_failure(IP, &format!("u{i}"));
        }
        assert!(l.check(IP, "new").is_err());
        l.record_success(IP, "new");
        assert!(l.check(IP, "new").is_err());
        clock.advance_ms(61_000);
        assert!(l.check(IP, "new").is_ok());
    }

    #[test]
    fn ipv6_grouped_by_prefix_and_bounded() {
        let clock = Arc::new(ManualClock::new(0));
        let l = limiter(clock);
        for i in 0..6 {
            let ip: IpAddr = format!("2001:db8:1:2::{i:x}").parse().unwrap();
            l.record_failure(ip, "x");
        }
        assert!(
            l.check("2001:db8:1:2:ffff::1".parse().unwrap(), "y")
                .is_err()
        );
        assert!(l.check("2001:db8:1:3::1".parse().unwrap(), "y").is_ok());
        for i in 0..20 {
            l.record_failure(IP, &format!("spray{i}"));
        }
        assert!(l.users.lock().len() <= 4);
    }
}
