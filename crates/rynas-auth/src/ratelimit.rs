//! Failed-login throttling.
//!
//! Failures are counted per client IP (IPv6 per /64, one host usually owns the whole prefix)
//! and per user name. After a number of free attempts each further failure locks that key for
//! an exponentially growing delay (`base * 2^n`, capped). A success resets the user counter;
//! the IP counter only decays, so one valid account cannot be used to reset it. Counters are
//! forgotten some time after the last failure.
//!
//! Every failure is logged as `authentication failure for USER from IP` (target `rynas_auth`),
//! the shape fail2ban filters expect (see `packaging/fail2ban/`). The mobile devices' public
//! routes have their own limiter ([`FailureLog::Device`]): their own tables, the same delays,
//! and their own line, `device authentication failure from IP`, for a filter of its own.

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
    /// Attempts being checked now, their outcome not known yet ([`LoginLimiter::begin`]).
    in_flight: u32,
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
            map.retain(|_, e| e.in_flight > 0 || now < e.last_failure_ms + forget);
            if map.len() >= cfg.max_entries {
                // still full: drop the stalest entry (O(n), only under attack)
                if let Some(k) = map
                    .iter()
                    .filter(|(_, e)| e.in_flight == 0)
                    .min_by_key(|(_, e)| e.last_failure_ms)
                    .map(|(k, _)| k.clone())
                {
                    map.remove(&k);
                }
            }
        }
        let e = map.entry(key.clone()).or_default();
        if now >= e.last_failure_ms + cfg.forget_after.as_millis() as u64 {
            *e = Entry {
                in_flight: e.in_flight,
                ..Entry::default()
            };
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
        let mut map = self.lock();
        let in_flight = map.get(key).map_or(0, |e| e.in_flight);
        if in_flight == 0 {
            map.remove(key);
        } else {
            map.insert(
                key.clone(),
                Entry {
                    in_flight,
                    ..Entry::default()
                },
            );
        }
    }

    /// Counts an attempt in, unless as many are being checked already as could still fail,
    /// one after the other, before the key locks (one, once it has been locked).
    fn enter(&self, key: &K, now: u64, cfg: &RateLimitConfig) -> bool {
        let mut map = self.lock();
        let e = map.entry(key.clone()).or_default();
        let failures = if now >= e.last_failure_ms + cfg.forget_after.as_millis() as u64 {
            0
        } else {
            e.failures
        };
        let room = self.free.saturating_sub(failures).saturating_add(1);
        if e.in_flight >= room {
            return false;
        }
        e.in_flight += 1;
        true
    }

    /// Counts an attempt out; a key without failures leaves the table with its last one.
    fn leave(&self, key: &K) {
        let mut map = self.lock();
        let Some(e) = map.get_mut(key) else { return };
        e.in_flight = e.in_flight.saturating_sub(1);
        let empty = e.in_flight == 0 && e.failures == 0;
        if empty {
            map.remove(key);
        }
    }
}

/// The line a failure writes; each has its own fail2ban filter (`packaging/fail2ban/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FailureLog {
    /// `authentication failure for USER from IP` (filter `rynas`): sign-ins.
    #[default]
    Login,
    /// `device authentication failure from IP` (filter `rynas-devices`): the mobile devices'
    /// public routes (claim, poll, refresh). Never matches the sign-in filter.
    Device,
}

pub struct LoginLimiter {
    cfg: RateLimitConfig,
    clock: Arc<dyn Clock>,
    users: Table<String>,
    ips: Table<IpAddr>,
    log: FailureLog,
}

/// `retry_after` of an attempt refused only because others are being checked.
pub const BUSY_RETRY: Duration = Duration::from_secs(1);

/// An attempt being checked ([`LoginLimiter::begin`]); counted out when dropped.
pub struct Attempt<'a> {
    limiter: &'a LoginLimiter,
    ip: IpAddr,
    user: String,
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        self.limiter.ips.leave(&self.ip);
        self.limiter.users.leave(&self.user);
    }
}

/// IPv6 addresses are grouped by /64 (IPv4-mapped ones are unwrapped).
pub fn ip_key(ip: IpAddr) -> IpAddr {
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

/// Lower case, and no longer than a valid name: the table keeps what a client sent as the
/// name, whatever its length.
fn user_key(user: &str) -> String {
    let end = user.char_indices().nth(64).map_or(user.len(), |(i, _)| i);
    user[..end].to_lowercase()
}

impl LoginLimiter {
    pub fn new(cfg: RateLimitConfig, clock: Arc<dyn Clock>) -> Self {
        Self::with_log(cfg, clock, FailureLog::Login)
    }

    /// A limiter whose failures write the line `log`.
    pub fn with_log(cfg: RateLimitConfig, clock: Arc<dyn Clock>, log: FailureLog) -> Self {
        Self {
            users: Table::new(cfg.free_per_user),
            ips: Table::new(cfg.free_per_ip),
            cfg,
            clock,
            log,
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

    /// [`LoginLimiter::check`], then the attempt is counted in until the returned value is
    /// dropped: attempts sent at the same time cannot all pass the check before the first of
    /// them has failed. `Err(retry_after)` when the IP or the user is locked, or has as many
    /// attempts being checked as it could still fail before locking ([`BUSY_RETRY`]).
    pub fn begin(&self, ip: IpAddr, user: &str) -> Result<Attempt<'_>, Duration> {
        self.check(ip, user)?;
        let now = self.clock.now_ms();
        let (ip, user) = (ip_key(ip), user_key(user));
        if !self.ips.enter(&ip, now, &self.cfg) {
            return Err(BUSY_RETRY);
        }
        if !self.users.enter(&user, now, &self.cfg) {
            self.ips.leave(&ip);
            return Err(BUSY_RETRY);
        }
        Ok(Attempt {
            limiter: self,
            ip,
            user,
        })
    }

    /// Whether the user name itself is locked now (a refused attempt may also come from a
    /// locked client address alone).
    pub fn user_locked(&self, user: &str) -> bool {
        let now = self.clock.now_ms();
        self.users
            .retry_after(&user_key(user), now, &self.cfg)
            .is_some()
    }

    /// Whether the client's address (its /64 for IPv6) is locked now.
    pub fn ip_locked(&self, ip: IpAddr) -> bool {
        let now = self.clock.now_ms();
        self.ips.retry_after(&ip_key(ip), now, &self.cfg).is_some()
    }

    pub fn record_failure(&self, ip: IpAddr, user: &str) {
        let now = self.clock.now_ms();
        if self.cfg.log_failures {
            match self.log {
                FailureLog::Login => tracing::warn!(
                    target: "rynas_auth",
                    "authentication failure for {} from {}",
                    log_safe(user),
                    ip
                ),
                FailureLog::Device => {
                    tracing::warn!(target: "rynas_auth", "device authentication failure from {ip}")
                }
            }
        }
        let a = self.ips.fail(&ip_key(ip), now, &self.cfg);
        let b = self.users.fail(&user_key(user), now, &self.cfg);
        let what = match self.log {
            FailureLog::Login => "login",
            FailureLog::Device => "device authentication",
        };
        if let Some(d) = a {
            tracing::info!(target: "rynas_auth", "{what} from {ip} locked for {}s", d.as_secs());
        }
        // the device routes' "user" is the client address again: one line is enough
        if let Some(d) = b.filter(|_| self.log == FailureLog::Login) {
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

    #[test]
    fn attempts_in_flight_count_before_they_fail() {
        let clock = Arc::new(ManualClock::new(1_000_000));
        let l = limiter(clock.clone());
        // as many at once as could fail one after the other before the lock: 3 free + 1
        let held: Vec<Attempt<'_>> = (0..4).map(|_| l.begin(IP, "bob").unwrap()).collect();
        assert_eq!(l.begin(IP, "Bob").err(), Some(BUSY_RETRY));
        // other names from the same address have room, up to the address's own (5 free + 1)
        let alice = l.begin(IP, "alice").unwrap();
        let carol = l.begin(IP, "carol").unwrap();
        assert!(l.begin(IP, "dave").is_err());
        drop(alice);
        assert!(l.begin(IP, "dave").is_ok());
        // an attempt that did not fail leaves nothing behind
        assert!(!l.users.lock().contains_key("alice"));
        assert!(!l.users.lock().contains_key("dave"));
        // they all fail: the name is locked; after the lock, one attempt at a time
        for _ in &held {
            l.record_failure(IP, "bob");
        }
        drop(held);
        drop(carol);
        assert!(l.user_locked("bob"));
        assert!(l.begin(IP, "bob").is_err());
        clock.advance_ms(1_000);
        let one = l.begin(IP, "bob").unwrap();
        assert!(l.begin(IP, "bob").is_err());
        // a success forgets the failures, not the attempt still being checked
        l.record_success(IP, "bob");
        assert!(l.users.lock().contains_key("bob"));
        drop(one);
        assert!(!l.users.lock().contains_key("bob"));
    }

    #[test]
    fn a_long_name_is_not_kept_whole() {
        let clock = Arc::new(ManualClock::new(0));
        let l = limiter(clock);
        let long = "é".repeat(100_000);
        l.record_failure(IP, &long);
        assert!(l.users.lock().keys().all(|k| k.chars().count() <= 64));
        // a valid name is its own key
        assert_eq!(user_key("Bob"), "bob");
        assert_eq!(user_key(&"a".repeat(64)), "a".repeat(64));
    }

    #[test]
    fn user_locked_tells_the_account_from_the_address() {
        let clock = Arc::new(ManualClock::new(1_000_000));
        let l = limiter(clock);
        // six names from one address: the address is locked, none of the names
        for i in 0..6 {
            l.record_failure(IP, &format!("u{i}"));
        }
        assert!(l.check(IP, "u0").is_err());
        assert!(!l.user_locked("u0"));
        assert!(l.ip_locked(IP));
        assert!(!l.ip_locked("198.51.100.7".parse().unwrap()));
        // four failures of one name from several addresses: the name is locked
        for i in 0..4 {
            l.record_failure(format!("198.51.100.{i}").parse().unwrap(), "bob");
        }
        assert!(l.user_locked("BOB"));
    }
}
