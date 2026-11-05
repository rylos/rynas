//! In-memory sessions.
//!
//! The cookie carries a 256-bit random token; the store keeps only its SHA-256, so a memory
//! dump or a timing side channel on the map lookup does not reveal usable tokens. Each session
//! has its own CSRF token, an idle timeout (renewed by activity) and an absolute lifetime.
//! Sessions do not survive a restart of `rynas-web`: users log in again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::{Session, UserInfo};
use sha2::{Digest, Sha256};

use crate::{Clock, ct_eq, random_token};

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub idle_timeout: Duration,
    pub absolute_timeout: Duration,
    /// Oldest sessions of a user are dropped beyond this.
    pub max_per_user: usize,
    /// Oldest sessions overall are dropped beyond this.
    pub max_total: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(30 * 60),
            absolute_timeout: Duration::from_secs(12 * 3600),
            max_per_user: 16,
            max_total: 4096,
        }
    }
}

/// Hash of a session token: the key of the store.
pub type SessionId = [u8; 32];

pub fn session_id(token: &str) -> SessionId {
    Sha256::digest(token.as_bytes()).into()
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionData {
    pub id: SessionId,
    pub user: UserInfo,
    pub csrf_token: String,
    pub totp_enabled: bool,
    pub created_ms: u64,
    pub last_seen_ms: u64,
}

impl SessionData {
    pub fn check_csrf(&self, header: Option<&str>) -> bool {
        header.is_some_and(|h| ct_eq(h, &self.csrf_token))
    }
}

pub struct SessionStore {
    cfg: SessionConfig,
    clock: Arc<dyn Clock>,
    map: Mutex<HashMap<SessionId, SessionData>>,
}

impl SessionStore {
    pub fn new(cfg: SessionConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            cfg,
            clock,
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &SessionConfig {
        &self.cfg
    }

    fn expired(&self, s: &SessionData, now: u64) -> bool {
        now >= s.created_ms + self.cfg.absolute_timeout.as_millis() as u64
            || now >= s.last_seen_ms + self.cfg.idle_timeout.as_millis() as u64
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, SessionData>> {
        // a panic while holding the lock cannot leave the map inconsistent
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Create a session; returns the cookie token (never stored) and the session.
    pub fn create(&self, user: UserInfo, totp_enabled: bool) -> (String, SessionData) {
        let now = self.clock.now_ms();
        let token = random_token();
        let data = SessionData {
            id: session_id(&token),
            user,
            csrf_token: random_token(),
            totp_enabled,
            created_ms: now,
            last_seen_ms: now,
        };
        let mut map = self.lock();
        map.retain(|_, s| !self.expired(s, now));
        let mine: Vec<(SessionId, u64)> = map
            .values()
            .filter(|s| s.user.username == data.user.username && s.user.realm == data.user.realm)
            .map(|s| (s.id, s.last_seen_ms))
            .collect();
        evict_oldest(&mut map, mine, self.cfg.max_per_user.saturating_sub(1));
        let all: Vec<(SessionId, u64)> = map.values().map(|s| (s.id, s.last_seen_ms)).collect();
        evict_oldest(&mut map, all, self.cfg.max_total.saturating_sub(1));
        map.insert(data.id, data.clone());
        (token, data)
    }

    /// Look a session up by cookie token. `touch` renews the idle timeout (normal requests);
    /// long-lived streams pass `false` so that an open tab does not keep a session alive.
    pub fn get(&self, token: &str, touch: bool) -> Option<SessionData> {
        self.get_by_id(&session_id(token), touch)
    }

    pub fn get_by_id(&self, id: &SessionId, touch: bool) -> Option<SessionData> {
        let now = self.clock.now_ms();
        let mut map = self.lock();
        let s = map.get_mut(id)?;
        if self.expired(s, now) {
            map.remove(id);
            return None;
        }
        if touch {
            s.last_seen_ms = now;
        }
        Some(s.clone())
    }

    pub fn is_valid(&self, id: &SessionId) -> bool {
        self.get_by_id(id, false).is_some()
    }

    pub fn remove(&self, id: &SessionId) -> bool {
        self.lock().remove(id).is_some()
    }

    /// Drop every session of a user except `keep`.
    pub fn remove_user_except(&self, username: &str, realm: &str, keep: Option<&SessionId>) {
        self.lock().retain(|id, s| {
            Some(id) == keep || s.user.username != username || s.user.realm != realm
        });
    }

    pub fn set_totp_enabled(&self, username: &str, realm: &str, enabled: bool) {
        for s in self.lock().values_mut() {
            if s.user.username == username && s.user.realm == realm {
                s.totp_enabled = enabled;
            }
        }
    }

    /// Remove expired sessions (also done on every `create`).
    pub fn purge(&self) {
        let now = self.clock.now_ms();
        self.lock().retain(|_, s| !self.expired(s, now));
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The public view of a session.
    pub fn to_api(&self, s: &SessionData) -> Session {
        let idle = self.cfg.idle_timeout.as_millis() as u64;
        let expires_ms = s.created_ms + self.cfg.absolute_timeout.as_millis() as u64;
        Session {
            user: s.user.clone(),
            csrf_token: s.csrf_token.clone(),
            totp_enabled: s.totp_enabled,
            created_ms: s.created_ms,
            expires_ms,
            idle_expires_ms: (s.last_seen_ms + idle).min(expires_ms),
            idle_timeout_s: self.cfg.idle_timeout.as_secs(),
        }
    }
}

/// Remove the least recently used of `candidates` until at most `keep` remain.
fn evict_oldest(
    map: &mut HashMap<SessionId, SessionData>,
    mut candidates: Vec<(SessionId, u64)>,
    keep: usize,
) {
    if candidates.len() <= keep {
        return;
    }
    candidates.sort_by_key(|&(_, seen)| seen);
    let n = candidates.len() - keep;
    for (id, _) in candidates.into_iter().take(n) {
        map.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManualClock;
    use rynas_api_types::Role;

    fn user(name: &str) -> UserInfo {
        UserInfo {
            username: name.into(),
            realm: "rynas".into(),
            role: Role::Admin,
        }
    }

    fn store(clock: Arc<ManualClock>) -> SessionStore {
        SessionStore::new(
            SessionConfig {
                idle_timeout: Duration::from_secs(60),
                absolute_timeout: Duration::from_secs(300),
                max_per_user: 2,
                max_total: 3,
            },
            clock,
        )
    }

    #[test]
    fn idle_and_absolute_timeouts() {
        let clock = Arc::new(ManualClock::new(1_000_000));
        let st = store(clock.clone());
        let (token, s) = st.create(user("a"), false);
        assert_ne!(token, s.csrf_token);
        assert!(st.get(&token, true).is_some());
        clock.advance_ms(59_000);
        assert!(st.get(&token, false).is_some()); // not touched
        clock.advance_ms(2_000);
        assert!(st.get(&token, true).is_none(), "idle timeout");

        let (token, _) = st.create(user("a"), false);
        for _ in 0..9 {
            clock.advance_ms(30_000);
            assert!(st.get(&token, true).is_some());
        }
        clock.advance_ms(29_999);
        assert!(st.get(&token, true).is_some());
        clock.advance_ms(1); // 300 s after creation
        assert!(st.get(&token, true).is_none(), "absolute timeout");
        assert!(st.is_empty());
    }

    #[test]
    fn api_view_and_csrf() {
        let clock = Arc::new(ManualClock::new(10_000));
        let st = store(clock.clone());
        let (token, s) = st.create(user("a"), true);
        let api = st.to_api(&s);
        assert_eq!(api.expires_ms, 310_000);
        assert_eq!(api.idle_expires_ms, 70_000);
        assert_eq!(api.idle_timeout_s, 60);
        assert!(api.totp_enabled);
        assert!(s.check_csrf(Some(&s.csrf_token)));
        assert!(!s.check_csrf(Some("nope")));
        assert!(!s.check_csrf(None));
        assert!(st.get("not-a-token", true).is_none());
        assert!(st.remove(&session_id(&token)));
        assert!(st.get(&token, true).is_none());
    }

    #[test]
    fn limits_evict_least_recently_used() {
        let clock = Arc::new(ManualClock::new(0));
        let st = store(clock.clone());
        let (t1, _) = st.create(user("a"), false);
        clock.advance_ms(1);
        let (t2, _) = st.create(user("a"), false);
        clock.advance_ms(1);
        let (t3, _) = st.create(user("a"), false);
        assert!(st.get(&t1, false).is_none(), "per-user limit");
        assert!(st.get(&t2, false).is_some() && st.get(&t3, false).is_some());
        clock.advance_ms(1);
        st.create(user("b"), false);
        clock.advance_ms(1);
        st.create(user("c"), false);
        assert_eq!(st.len(), 3, "global limit");
        assert!(st.get(&t2, false).is_none());
    }

    #[test]
    fn per_user_operations() {
        let clock = Arc::new(ManualClock::new(0));
        let st = store(clock);
        let (ta, a) = st.create(user("a"), false);
        let (tb, _) = st.create(user("b"), false);
        st.set_totp_enabled("a", "rynas", true);
        assert!(st.get(&ta, false).unwrap().totp_enabled);
        assert!(!st.get(&tb, false).unwrap().totp_enabled);
        let (ta2, _) = st.create(user("a"), true);
        st.remove_user_except("a", "rynas", Some(&a.id));
        assert!(st.get(&ta, false).is_some());
        assert!(st.get(&ta2, false).is_none());
        assert!(st.get(&tb, false).is_some());
    }
}
