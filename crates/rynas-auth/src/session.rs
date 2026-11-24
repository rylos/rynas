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
    /// Lifetime of a mobile device's access token (a device session: no idle timeout, no
    /// renewal; the device refreshes).
    pub device_access_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(30 * 60),
            absolute_timeout: Duration::from_secs(12 * 3600),
            max_per_user: 16,
            max_total: 4096,
            device_access_timeout: Duration::from_secs(rynas_api_types::devices::ACCESS_TTL_S),
        }
    }
}

/// The paired device behind a device session (bearer access token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRef {
    pub id: String,
    pub name: String,
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
    /// The last password (and TOTP) check: the login, or a later `auth/reauth`; for a device
    /// session its refresh (a check of the device key).
    pub auth_ms: u64,
    /// `Some` for a device session (bearer access token), never reachable with the cookie.
    pub device: Option<DeviceRef>,
}

impl SessionData {
    pub fn check_csrf(&self, header: Option<&str>) -> bool {
        // a device session has no CSRF token: never equal to anything
        !self.csrf_token.is_empty() && header.is_some_and(|h| ct_eq(h, &self.csrf_token))
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
        if s.device.is_some() {
            return now >= s.created_ms + self.cfg.device_access_timeout.as_millis() as u64;
        }
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
            auth_ms: now,
            device: None,
        };
        let mut map = self.lock();
        map.retain(|_, s| !self.expired(s, now));
        // device sessions are bounded per device (create_device), not by these caps
        let mine: Vec<(SessionId, u64)> = map
            .values()
            .filter(|s| s.device.is_none())
            .filter(|s| s.user.username == data.user.username && s.user.realm == data.user.realm)
            .map(|s| (s.id, s.last_seen_ms))
            .collect();
        evict_oldest(&mut map, mine, self.cfg.max_per_user.saturating_sub(1));
        let all: Vec<(SessionId, u64)> = map
            .values()
            .filter(|s| s.device.is_none())
            .map(|s| (s.id, s.last_seen_ms))
            .collect();
        evict_oldest(&mut map, all, self.cfg.max_total.saturating_sub(1));
        map.insert(data.id, data.clone());
        (token, data)
    }

    /// A device session for an access token handed to `device` (returned once, never
    /// stored). At most two live per device: the previous one stays valid for the requests
    /// already on their way, older ones end.
    pub fn create_device(&self, user: UserInfo, device: DeviceRef) -> (String, SessionData) {
        let now = self.clock.now_ms();
        let token = random_token();
        let data = SessionData {
            id: session_id(&token),
            user,
            csrf_token: String::new(),
            totp_enabled: false,
            created_ms: now,
            last_seen_ms: now,
            auth_ms: now,
            device: Some(device),
        };
        let mut map = self.lock();
        map.retain(|_, s| !self.expired(s, now));
        let id = data.device.as_ref().map(|d| d.id.clone());
        let same: Vec<(SessionId, u64)> = map
            .values()
            .filter(|s| s.device.as_ref().map(|d| d.id.clone()) == id)
            .map(|s| (s.id, s.created_ms))
            .collect();
        evict_oldest(&mut map, same, 1);
        map.insert(data.id, data.clone());
        (token, data)
    }

    /// Look a web session up by cookie token. `touch` renews the idle timeout (normal
    /// requests); long-lived streams pass `false` so that an open tab does not keep a session
    /// alive. A device's access token is never a cookie.
    pub fn get(&self, token: &str, touch: bool) -> Option<SessionData> {
        self.get_by_id(&session_id(token), touch)
            .filter(|s| s.device.is_none())
    }

    /// Look a device session up by bearer access token. A cookie token is never a bearer.
    pub fn get_bearer(&self, token: &str) -> Option<SessionData> {
        self.get_by_id(&session_id(token), true)
            .filter(|s| s.device.is_some())
    }

    /// Ends every session of the device `id` (revoked); returns how many.
    pub fn remove_device(&self, id: &str) -> usize {
        let mut map = self.lock();
        let before = map.len();
        map.retain(|_, s| s.device.as_ref().is_none_or(|d| d.id != id));
        before - map.len()
    }

    /// The devices with a live session.
    pub fn device_ids(&self) -> Vec<String> {
        let now = self.clock.now_ms();
        let mut v: Vec<String> = self
            .lock()
            .values()
            .filter(|s| !self.expired(s, now))
            .filter_map(|s| s.device.as_ref().map(|d| d.id.clone()))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Ends the device sessions whose device `keep` refuses (devices revoked elsewhere).
    pub fn retain_devices(&self, keep: impl Fn(&str) -> bool) {
        self.lock()
            .retain(|_, s| s.device.as_ref().is_none_or(|d| keep(&d.id)));
    }

    /// The device's new name in its live sessions (audit lines name it).
    pub fn rename_device(&self, id: &str, name: &str) {
        for s in self.lock().values_mut() {
            if let Some(d) = s.device.as_mut().filter(|d| d.id == id) {
                d.name = name.to_string();
            }
        }
    }

    /// Records a fresh password (and TOTP) check on the session `id`.
    pub fn mark_auth(&self, id: &SessionId) {
        let now = self.clock.now_ms();
        if let Some(s) = self.lock().get_mut(id) {
            s.auth_ms = now;
        }
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

    /// Drop every web session of a user except `keep` (paired devices keep theirs: they
    /// are bound to the device key, and revoked on their own).
    pub fn remove_user_except(&self, username: &str, realm: &str, keep: Option<&SessionId>) {
        self.lock().retain(|id, s| {
            Some(id) == keep
                || s.device.is_some()
                || s.user.username != username
                || s.user.realm != realm
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
                device_access_timeout: Duration::from_secs(900),
            },
            clock,
        )
    }

    fn dev(id: &str) -> DeviceRef {
        DeviceRef {
            id: id.into(),
            name: format!("phone {id}"),
        }
    }

    #[test]
    fn device_sessions_are_apart() {
        let clock = Arc::new(ManualClock::new(0));
        let st = store(clock.clone());
        let (web, _) = st.create(user("a"), false);
        let (t1, s1) = st.create_device(user("a"), dev("d_1"));
        assert!(s1.csrf_token.is_empty() && !s1.check_csrf(Some("")));
        // a bearer is not a cookie and a cookie is not a bearer
        assert!(st.get(&t1, true).is_none());
        assert!(st.get_bearer(&web).is_none());
        assert_eq!(st.get_bearer(&t1).unwrap().device, Some(dev("d_1")));
        // the web caps do not count devices, nor evict them
        clock.advance_ms(1);
        st.create(user("a"), false);
        clock.advance_ms(1);
        st.create(user("a"), false);
        assert!(st.get_bearer(&t1).is_some());
        // two access tokens per device at most
        clock.advance_ms(1);
        let (t2, _) = st.create_device(user("a"), dev("d_1"));
        clock.advance_ms(1);
        let (t3, _) = st.create_device(user("a"), dev("d_1"));
        assert!(st.get_bearer(&t1).is_none());
        assert!(st.get_bearer(&t2).is_some() && st.get_bearer(&t3).is_some());
        st.rename_device("d_1", "new");
        assert_eq!(st.get_bearer(&t3).unwrap().device.unwrap().name, "new");
        // fixed lifetime, activity or not
        clock.advance_ms(899_000);
        assert!(st.get_bearer(&t3).is_some());
        clock.advance_ms(1_000);
        assert!(st.get_bearer(&t3).is_none());
        let (t4, _) = st.create_device(user("a"), dev("d_2"));
        let (t5, _) = st.create_device(user("a"), dev("d_3"));
        st.retain_devices(|id| id != "d_2");
        assert!(st.get_bearer(&t4).is_none() && st.get_bearer(&t5).is_some());
        assert_eq!(st.remove_device("d_3"), 1);
        assert!(st.get_bearer(&t5).is_none());
    }

    #[test]
    fn reauth_moves_the_auth_time() {
        let clock = Arc::new(ManualClock::new(1_000));
        let st = store(clock.clone());
        let (t, s) = st.create(user("a"), false);
        assert_eq!(s.auth_ms, 1_000);
        clock.advance_ms(30_000);
        st.mark_auth(&s.id);
        assert_eq!(st.get(&t, false).unwrap().auth_ms, 31_000);
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
